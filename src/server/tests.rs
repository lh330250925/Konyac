use super::*;
use crate::api::HealthResponse;
use crate::config::{PromptConfig, Protocol, ServiceConfig, bearer_authorization};
use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

fn test_router() -> Router {
    test_router_with_upstream("http://[")
}

fn test_router_with_upstream(url: &str) -> Router {
    let mut service = ServiceConfig::test("test-service", Protocol::Openai, url);
    service.models = vec!["qwen2.5:7b".into()];
    build_router(TranslationService::new(&[service], PromptConfig::default()).unwrap())
}

async fn send(app: &Router, req: Request<Body>) -> (StatusCode, Vec<u8>) {
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = to_bytes(res.into_body(), 4096).await.unwrap();
    (status, bytes.to_vec())
}

async fn post_translate(app: &Router, body: Value) -> (StatusCode, Vec<u8>) {
    send(
        app,
        Request::builder()
            .method("POST")
            .uri("/translate")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn spawn_mock(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[tokio::test]
async fn bound_server_supports_tcp_only_unix_only_and_both_modes() {
    let empty_service = || TranslationService::new(&[], PromptConfig::default()).unwrap();

    // 1. TCP-only mode
    let tcp_only = BoundServer::bind(
        empty_service(),
        &ServerConfig {
            tcp: Some("127.0.0.1:0".into()),
            unix: None,
        },
    )
    .await
    .unwrap();
    assert!(tcp_only.tcp_addr().is_some());
    assert!(tcp_only.unix_path().is_none());

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("konyac_bind_{}_{unique:x}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // 2. Unix-only mode
    let unix_only_path = dir.join("only.sock");
    let unix_only = BoundServer::bind(
        empty_service(),
        &ServerConfig {
            tcp: None,
            unix: Some(unix_only_path.clone()),
        },
    )
    .await
    .unwrap();
    assert!(unix_only.tcp_addr().is_none());
    assert_eq!(unix_only.unix_path(), Some(unix_only_path.as_path()));
    drop(unix_only);

    // 3. Both mode
    let both_path = dir.join("both.sock");
    let both = BoundServer::bind(
        empty_service(),
        &ServerConfig {
            tcp: Some("127.0.0.1:0".into()),
            unix: Some(both_path.clone()),
        },
    )
    .await
    .unwrap();
    assert!(both.tcp_addr().is_some());
    assert_eq!(both.unix_path(), Some(both_path.as_path()));
    drop(both);

    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_server_routes_and_validation() {
    let app = test_router();

    let (status, bytes) = send(
        &app,
        Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let health: HealthResponse = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(health.status, "ok");
    assert_eq!(health.target_lang, "zh-CN");
    assert_eq!(health.services.len(), 1);
    assert_eq!(health.services[0].name, "test-service");
    assert_eq!(health.services[0].models, ["qwen2.5:7b"]);

    let (status, bytes) = post_translate(&app, json!({ "text": "" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(
        serde_json::from_slice::<KonyacError>(&bytes).unwrap(),
        KonyacError::new(
            ErrorCode::InvalidRequest,
            "Either 'text' or 'images' must be provided"
        ),
    );

    for (payload, expected) in [
        (
            json!({"images": [{"mime_type": "image/bmp", "data": "AAAA"}]}),
            "images[0].mime_type",
        ),
        (
            json!({"text": "hello", "history": [{"role": "user", "content": "earlier"}, {"role": "system", "content": "invalid role"}]}),
            "history[1].role",
        ),
    ] {
        let (status, bytes) = post_translate(&app, payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let error: KonyacError = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(error.message.contains(expected));
    }

    let (status, _) = post_translate(&app, json!({ "text": "hello" })).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}

#[tokio::test]
async fn translation_errors_share_the_same_http_and_sse_payload() {
    use axum::Json;
    use axum::response::IntoResponse;
    use axum::response::sse::{Event, Sse};
    use std::convert::Infallible;

    let upstream = Router::new().route(
        "/responses",
        post(|Json(payload): Json<Value>| async move {
            let text = payload["input"].as_array().unwrap().last().unwrap()["content"]
                .as_str()
                .unwrap();
            match text {
                "limit" => (
                    StatusCode::TOO_MANY_REQUESTS,
                    Json(json!({"error":{"message":"Request limit reached"}})),
                )
                    .into_response(),
                "timeout" => StatusCode::GATEWAY_TIMEOUT.into_response(),
                "truncated" => Sse::new(futures_util::stream::iter([
                    Ok::<_, Infallible>(
                        Event::default()
                            .data(r#"{"type":"response.output_text.delta","delta":"partial"}"#),
                    ),
                    Ok(Event::default().data(
                        r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
                    )),
                ]))
                .into_response(),
                _ => unreachable!(),
            }
        }),
    );
    let addr = spawn_mock(upstream).await;
    let app = test_router_with_upstream(&format!("http://{addr}"));

    for (payload, expected_status, code) in [
        (
            json!({"text":"limit"}),
            StatusCode::BAD_GATEWAY,
            "rate_limited",
        ),
        (
            json!({"text":"timeout"}),
            StatusCode::BAD_GATEWAY,
            "upstream_timeout",
        ),
        (
            json!({"text":"limit","model":"unknown"}),
            StatusCode::BAD_REQUEST,
            "model_not_configured",
        ),
        (json!({}), StatusCode::BAD_REQUEST, "invalid_request"),
        (
            json!({"text": 123}),
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_request",
        ),
    ] {
        let (status, body) = post_translate(&app, payload).await;
        assert_eq!(status, expected_status);
        let error: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["code"], code);
        assert!(!error["error"].as_str().unwrap().is_empty());
    }

    let (status, body) = post_translate(&app, json!({"text":"truncated"})).await;
    assert_eq!(status, StatusCode::OK);
    let error_data = std::str::from_utf8(&body)
        .unwrap()
        .strip_prefix("data: partial\n\nevent: error\ndata: ")
        .unwrap();
    let error: Value = serde_json::from_str(error_data.trim_end()).unwrap();
    assert_eq!(error["code"], "output_truncated");
    assert!(
        error["error"]
            .as_str()
            .unwrap()
            .contains("max_output_tokens")
    );
}

#[tokio::test]
async fn translate_uses_daemon_defaults_and_explicit_overrides() {
    use axum::Json;
    use axum::extract::Path;
    use axum::response::sse::{Event, Sse};
    use std::convert::Infallible;

    let upstream = Router::new().route(
        "/{service}/responses",
        post(
            |Path(service): Path<String>, Json(payload): Json<Value>| async move {
                let model = payload["model"].as_str().unwrap();
                let system = payload["input"][0]["content"].as_str().unwrap();
                let content = &payload["input"][1]["content"];
                let summary = if let Some(parts) = content.as_array() {
                    format!(
                        "{service}/{model}/{system}/img={}/{}",
                        parts[0]["type"].as_str().unwrap(),
                        parts[1]["text"].as_str().unwrap()
                    )
                } else {
                    assert_eq!(content, "  Hello\n");
                    format!("{service}/{model}/{system}")
                };
                let delta = json!({"type":"response.output_text.delta","delta":summary});
                Sse::new(futures_util::stream::iter([
                    Ok::<_, Infallible>(Event::default().json_data(delta).unwrap()),
                    Ok(Event::default().data(r#"{"type":"response.completed"}"#)),
                ]))
            },
        ),
    );
    let addr = spawn_mock(upstream).await;

    let services = ["z-first", "a-other"].map(|name| {
        let mut cfg = ServiceConfig::test(name, Protocol::Openai, &format!("http://{addr}/{name}"));
        cfg.models = vec![format!("{name}-first"), format!("{name}-second")];
        cfg
    });
    let prompt = PromptConfig {
        system: Some("Target: {target_lang}".into()),
        target_lang: "fr".into(),
    };
    let app = build_router(TranslationService::new(&services, prompt).unwrap());

    let (status, health_body) = send(
        &app,
        Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let health: HealthResponse = serde_json::from_slice(&health_body).unwrap();
    assert_eq!(health.target_lang, "fr");
    let names: Vec<_> = health.services.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, ["z-first", "a-other"]);

    for (overrides, expected) in [
        (json!({}), "z-first/z-first-first/Target: fr"),
        (
            json!({"target_lang":" ja "}),
            "z-first/z-first-first/Target: ja",
        ),
        (
            json!({"model":"z-first-second"}),
            "z-first/z-first-second/Target: fr",
        ),
        (
            json!({"service":"a-other"}),
            "a-other/a-other-first/Target: fr",
        ),
        (
            json!({"service":"a-other","model":"a-other-second","target_lang":"de"}),
            "a-other/a-other-second/Target: de",
        ),
        (
            json!({"system":"Return strict JSON"}),
            "z-first/z-first-first/Return strict JSON",
        ),
        (
            json!({"system":"Sys {target_lang}","extra":"Keep terms"}),
            "z-first/z-first-first/Sys fr\n\nAdditional instructions and context (prioritize these terminology, domain, and style preferences over defaults while keeping output boundaries intact):\nKeep terms",
        ),
    ] {
        let mut payload = overrides;
        payload["text"] = json!("  Hello\n");
        let (status, body) = post_translate(&app, payload).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            format!(
                "data: {}\n\nevent: done\ndata: [DONE]\n\n",
                expected.replace('\n', "\ndata: ")
            )
            .into_bytes()
        );
    }

    let (status, image_body) = post_translate(
        &app,
        json!({"images": [{"mime_type": "image/png", "data": "iVBORw0KGgo="}]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        image_body,
        b"data: z-first/z-first-first/Target: fr/img=input_image/Translate all visible text in the image.\n\nevent: done\ndata: [DONE]\n\n"
    );

    for (overrides, error) in [
        (
            json!({"service":"z-first","model":"a-other-first"}),
            "is not configured for",
        ),
        (json!({"service":"missing"}), "not found"),
        (json!({"service":" "}), "Field 'service' cannot be empty"),
        (json!({"model":""}), "Field 'model' cannot be empty"),
        (
            json!({"target_lang":" "}),
            "Field 'target_lang' cannot be empty",
        ),
        (json!({"system":"   "}), "Field 'system' cannot be empty"),
    ] {
        let mut payload = overrides;
        payload["text"] = json!("  Hello\n");
        let (status, body) = post_translate(&app, payload).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(String::from_utf8_lossy(&body).contains(error));
    }
}

#[tokio::test]
async fn test_tcp_access() {
    let app = test_router().layer(middleware::from_fn_with_state(None, tcp_access));
    for (host, expected) in [
        ("localhost:16742", StatusCode::OK),
        ("[::1]:16742", StatusCode::OK),
        ("non-local.example", StatusCode::FORBIDDEN),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .header(header::HOST, host)
                    .header(header::ORIGIN, "https://non-local.example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
    }

    let authorization = bearer_authorization("example-token").unwrap();
    assert!(authorization.is_sensitive());
    let app = test_router().layer(middleware::from_fn_with_state(
        Some(authorization),
        tcp_access,
    ));
    for (auth, expected) in [
        (None, StatusCode::UNAUTHORIZED),
        (Some("Bearer invalid-token"), StatusCode::UNAUTHORIZED),
        (Some("bearer example-token"), StatusCode::OK),
    ] {
        let mut req = Request::builder().uri("/health");
        if let Some(auth) = auth {
            req = req.header(header::AUTHORIZATION, auth);
        }
        assert_eq!(
            send(&app, req.body(Body::empty()).unwrap()).await.0,
            expected
        );
    }
}
