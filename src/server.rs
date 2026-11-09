mod handlers;
pub mod types;

use crate::config::RuntimeServerConfig;
use crate::llm::LlmGateway;
use crate::prompt_engine::PromptEngine;
use axum::Router;
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;

/// Shared application state for the daemon.
pub(crate) struct AppState {
    pub prompt: PromptEngine,
    pub gateway: LlmGateway,
}

/// Builds the Axum router.
pub fn build_router(prompt: PromptEngine, gateway: LlmGateway) -> Router {
    let state: &'static AppState = Box::leak(Box::new(AppState { prompt, gateway }));
    Router::new()
        .route("/health", get(handlers::handle_health))
        .route("/services", get(handlers::handle_services))
        .route("/translate", post(handlers::handle_translate))
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// Starts HTTP / IPC servers and listens for shutdown signals.
pub async fn run(
    prompt: PromptEngine,
    gateway: LlmGateway,
    server_cfg: &RuntimeServerConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    let router = build_router(prompt, gateway);

    let tcp_listener = if let Some(ref tcp) = server_cfg.tcp {
        let addr = format!("{}:{}", tcp.host, tcp.port);
        let listener = TcpListener::bind(&addr).await?;
        println!("AI translation daemon TCP endpoint: http://{addr}");
        Some(listener)
    } else {
        None
    };

    #[cfg(unix)]
    struct SocketGuard(std::path::PathBuf);

    #[cfg(unix)]
    impl Drop for SocketGuard {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[cfg(unix)]
    let (unix_listener, _socket_guard) = if let Some(ref path) = server_cfg.unix {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(path);
        let listener = tokio::net::UnixListener::bind(path)?;
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        println!("AI translation daemon Unix socket: {}", path.display());
        (Some(listener), Some(SocketGuard(path.clone())))
    } else {
        (None, None)
    };

    #[cfg(not(unix))]
    let (unix_listener, _socket_guard): (Option<()>, Option<()>) = (None, None);

    if tcp_listener.is_none() && unix_listener.is_none() {
        return Err("No valid server endpoints (TCP or Unix socket) configured to run".into());
    }

    println!("API endpoints:");
    println!("  - GET  /health");
    println!("  - GET  /services");
    println!("  - POST /translate (SSE stream)");

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(());
    tokio::spawn(async move {
        shutdown_signal().await;
        let _ = shutdown_tx.send(());
    });

    let (mut rx1, mut rx2) = (shutdown_rx.clone(), shutdown_rx);
    let tcp_router = router.clone();
    let tcp_task = async move {
        if let Some(tcp) = tcp_listener {
            axum::serve(tcp, tcp_router)
                .with_graceful_shutdown(async move {
                    let _ = rx1.changed().await;
                })
                .await
        } else {
            Ok(())
        }
    };

    #[cfg(unix)]
    let unix_task = async move {
        if let Some(unix) = unix_listener {
            axum::serve(unix, router)
                .with_graceful_shutdown(async move {
                    let _ = rx2.changed().await;
                })
                .await
        } else {
            Ok(())
        }
    };
    #[cfg(not(unix))]
    let unix_task = async { Ok::<(), std::io::Error>(()) };

    let (r1, r2) = tokio::join!(tcp_task, unix_task);
    r1?;
    r2?;

    println!("Daemon stopped gracefully.");
    Ok(())
}

/// Waits for system shutdown signals (SIGINT / SIGTERM).
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C signal handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {
            println!("\nReceived Ctrl+C (SIGINT), initiating graceful shutdown...");
        },
        _ = terminate => {
            println!("\nReceived SIGTERM, initiating graceful shutdown...");
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Protocol, RuntimeServiceConfig};
    use crate::llm::ServiceInfo;
    use crate::server::types::HealthResponse;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn test_router() -> Router {
        let service = RuntimeServiceConfig {
            name: "test-service".into(),
            protocol: Protocol::Openai,
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            models: vec!["qwen2.5:7b".into()],
        };
        build_router(
            PromptEngine::default(),
            LlmGateway::new(&[service]).unwrap(),
        )
    }

    #[tokio::test]
    async fn test_server_routes_and_validation() {
        let app = test_router();

        // 1. GET /health
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let health: HealthResponse = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(health.status, "ok");

        // 2. GET /services
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/services")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let srvs: Vec<ServiceInfo> = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(srvs.len(), 1);

        // 3. POST /translate (validation error: empty text)
        let bad_json = serde_json::json!({
            "text": "", "target_lang": "zh", "service": "test-service", "model": "qwen2.5:7b"
        });
        let res = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/translate")
                    .header("content-type", "application/json")
                    .body(Body::from(bad_json.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);

        // 4. POST /translate (upstream unreachable -> BAD_GATEWAY)
        let valid_json = serde_json::json!({
            "text": "hello", "target_lang": "zh", "service": "test-service", "model": "qwen2.5:7b"
        });
        let res = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/translate")
                    .header("content-type", "application/json")
                    .body(Body::from(valid_json.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::BAD_GATEWAY);
    }
}
