#[cfg(test)]
mod tests;
mod unix;

use crate::api::{ErrorCode, HealthResponse, KonyacError, SSE_DONE, SSE_ERROR, TranslateRequest};
use crate::config::{ServerConfig, split_tcp_addr, tcp_authorization};
use crate::translation::{TranslationEvent, TranslationService};
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Request, State};
use axum::http::{HeaderValue, StatusCode, header, uri::Authority};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use std::future::IntoFuture;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::watch;

const SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);

fn error_response(status: StatusCode, error: KonyacError) -> Response {
    (status, Json(error)).into_response()
}

fn invalid_json(rejection: JsonRejection) -> Response {
    error_response(
        rejection.status(),
        KonyacError::new(ErrorCode::InvalidRequest, rejection.body_text()),
    )
}

fn translation_error(route: &'static str, error: KonyacError) -> Response {
    let status = match error.code {
        ErrorCode::InvalidRequest
        | ErrorCode::ServiceNotConfigured
        | ErrorCode::ModelNotConfigured => StatusCode::BAD_REQUEST,
        _ => StatusCode::BAD_GATEWAY,
    };
    if status.is_server_error() {
        eprintln!(
            "[konyac] {route} ({status} {:?}): {}",
            error.code, error.message
        );
    }
    error_response(status, error)
}

async fn handle_health(State(service): State<Arc<TranslationService>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        target_lang: service.target_lang().to_owned(),
        services: service.list_services().cloned().collect(),
    })
}

async fn handle_translate(
    State(service): State<Arc<TranslationService>>,
    request: Result<Json<TranslateRequest>, JsonRejection>,
) -> Response {
    let req = match request {
        Ok(Json(req)) => req,
        Err(rejection) => return invalid_json(rejection),
    };
    let text_stream = match service.translate(&req).await {
        Ok(stream) => stream,
        Err(error) => return translation_error("POST /translate", error),
    };

    let sse_stream = text_stream.map(|res| match res {
        Ok(TranslationEvent::Text(chunk)) => Ok(Event::default().data(chunk)),
        Ok(TranslationEvent::Done) => Ok(Event::default().event(SSE_DONE).data("[DONE]")),
        Err(error) => {
            eprintln!(
                "[konyac] POST /translate stream ({:?}): {}",
                error.code, error.message
            );
            Event::default().event(SSE_ERROR).json_data(error)
        }
    });

    Sse::new(sse_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}

/// Builds the Axum router.
fn build_router(service: TranslationService) -> Router {
    Router::new()
        .route("/health", get(handle_health))
        .route(
            "/translate",
            post(handle_translate).layer(DefaultBodyLimit::max(16 * 1024 * 1024)),
        )
        .with_state(Arc::new(service))
}

fn loopback_authority(authority: &Authority) -> bool {
    let host = authority
        .host()
        .trim_start_matches('[')
        .trim_end_matches(']');
    host.trim_end_matches('.').eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

async fn tcp_access(
    State(authorization): State<Option<HeaderValue>>,
    request: Request,
    next: Next,
) -> Response {
    if let Some(expected) = authorization {
        let authorized = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|header| header.to_str().ok())
            .and_then(|header| header.split_once(' '))
            .is_some_and(|(scheme, token)| {
                scheme.eq_ignore_ascii_case("bearer")
                    && expected
                        .as_bytes()
                        .strip_prefix(b"Bearer ")
                        .is_some_and(|expected| bool::from(token.as_bytes().ct_eq(expected)))
            });
        if !authorized {
            let mut response = error_response(
                StatusCode::UNAUTHORIZED,
                KonyacError::new(ErrorCode::Unauthorized, "Bearer token required"),
            );
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return response;
        }
    } else {
        let host = request
            .headers()
            .get(header::HOST)
            .and_then(|host| Authority::try_from(host.as_bytes()).ok());
        let authority = request.uri().authority().or(host.as_ref());
        if !authority.is_some_and(loopback_authority)
            || host.as_ref().is_some_and(|host| !loopback_authority(host))
        {
            return error_response(
                StatusCode::FORBIDDEN,
                KonyacError::new(ErrorCode::Forbidden, "A loopback Host is required"),
            );
        }
    }
    next.run(request).await
}

pub struct BoundServer {
    router: Router,
    tcp_listener: Option<TcpListener>,
    tcp_addr: Option<SocketAddr>,
    authorization: Option<HeaderValue>,
    unix_listener: Option<UnixListener>,
    unix_path: Option<PathBuf>,
    _socket_guard: Option<unix::SocketGuard>,
}

impl BoundServer {
    /// Binds TCP / Unix socket listeners and validates access configuration.
    pub async fn bind(
        service: TranslationService,
        server_cfg: &ServerConfig,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let router = build_router(service);

        let (tcp_listener, tcp_addr, authorization) = if let Some(tcp) = &server_cfg.tcp {
            let authorization = tcp_authorization()?;
            let (host, port) = split_tcp_addr(tcp)?;
            let listener = TcpListener::bind((host, port)).await?;
            let addr = listener.local_addr()?;
            if authorization.is_none() && !addr.ip().to_canonical().is_loopback() {
                return Err("KONYAC_TCP_TOKEN is required for a non-loopback TCP listener".into());
            }
            (Some(listener), Some(addr), authorization)
        } else {
            (None, None, None)
        };

        let (unix_listener, unix_path, _socket_guard) = if let Some(path) = &server_cfg.unix {
            let (listener, guard) = unix::SocketGuard::bind(path).await?;
            (Some(listener), Some(path.clone()), Some(guard))
        } else {
            (None, None, None)
        };

        if tcp_listener.is_none() && unix_listener.is_none() {
            return Err("No valid server endpoints (TCP or Unix socket) configured to run".into());
        }

        Ok(Self {
            router,
            tcp_listener,
            tcp_addr,
            authorization,
            unix_listener,
            unix_path,
            _socket_guard,
        })
    }

    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        self.tcp_addr
    }

    pub fn tcp_auth_enabled(&self) -> bool {
        self.authorization.is_some()
    }

    pub fn unix_path(&self) -> Option<&Path> {
        self.unix_path.as_deref()
    }

    /// Runs HTTP / IPC servers until a shutdown signal is received.
    pub async fn serve(
        self,
        on_signal: impl FnOnce(&'static str) + Send + 'static,
    ) -> std::io::Result<()> {
        let Self {
            router,
            tcp_listener,
            tcp_addr: _,
            authorization,
            unix_listener,
            unix_path: _,
            _socket_guard,
        } = self;

        let (shutdown_tx, shutdown_rx) = watch::channel(());
        let signal_tx = shutdown_tx.clone();
        let signal_task = tokio::spawn(async move {
            let signal = shutdown_signal().await;
            on_signal(signal);
            let _ = signal_tx.send(());
        });

        let tcp_router = router
            .clone()
            .layer(middleware::from_fn_with_state(authorization, tcp_access));
        let tcp_task = serve_listener(tcp_listener, tcp_router, shutdown_rx.clone());
        let unix_task = serve_listener(unix_listener, router, shutdown_rx);

        let result = tokio::try_join!(
            async {
                let res = tcp_task.await;
                if res.is_err() {
                    let _ = shutdown_tx.send(());
                }
                res
            },
            async {
                let res = unix_task.await;
                if res.is_err() {
                    let _ = shutdown_tx.send(());
                }
                res
            }
        )
        .map(|((), ())| ());

        signal_task.abort();
        result
    }
}

async fn serve_listener<L: axum::serve::Listener>(
    listener: Option<L>,
    router: Router,
    mut shutdown: watch::Receiver<()>,
) -> std::io::Result<()>
where
    L::Addr: std::fmt::Debug,
{
    let Some(listener) = listener else {
        return Ok(());
    };
    let mut drain_signal = shutdown.clone();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = drain_signal.changed().await;
        })
        .into_future();
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        _ = shutdown.changed() => {
            if let Ok(result) = tokio::time::timeout(SHUTDOWN_DRAIN_TIMEOUT, &mut server).await {
                result
            } else {
                eprintln!(
                    "[konyac] Shutdown drain timed out after {}s; closing remaining connections",
                    SHUTDOWN_DRAIN_TIMEOUT.as_secs()
                );
                Ok(())
            }
        }
    }
}

/// Waits for system shutdown signals (SIGINT / SIGTERM).
async fn shutdown_signal() -> &'static str {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C signal handler");
    };

    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM signal handler")
            .recv()
            .await;
    };

    tokio::select! {
        _ = ctrl_c => "Ctrl+C (SIGINT)",
        _ = terminate => "SIGTERM",
    }
}
