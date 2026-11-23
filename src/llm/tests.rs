use super::*;
use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, Sse};
use axum::routing::post;
use serde_json::json;
use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn spawn_mock(app: Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

#[test]
fn test_llm_gateway_resolution_and_error_mapping() {
    assert_eq!(LlmGateway::new(&[]).unwrap().list_services().len(), 0);

    let mut ollama = ServiceConfig::test("ollama", Protocol::Openai, "http://localhost:11434/v1");
    ollama.models = vec!["qwen2.5".into()];
    ollama.thinking = Some(ThinkingLevel::Low);
    let services = [
        ollama.clone(),
        ServiceConfig {
            name: "another-service".into(),
            ..ollama
        },
    ];

    let gateway = LlmGateway::new(&services).unwrap();
    let names: Vec<_> = gateway
        .list_services()
        .map(|info| info.name.as_str())
        .collect();
    assert_eq!(names, ["ollama", "another-service"]);
    assert_eq!(
        gateway.list_services().next().unwrap().thinking,
        Some(ThinkingLevel::Low)
    );

    let entry = gateway.resolve_service("ollama").unwrap();
    assert_eq!(entry.resolve_model(None).unwrap(), "qwen2.5");
    assert!(entry.resolve_model(Some("qwen2.5")).is_ok());
    assert!(entry.resolve_model(Some("unknown")).is_err());
    assert!(gateway.resolve_service("non-existent").is_err());

    for raw_code in ["insufficient_quota", "overloaded_error"] {
        let err = provider_error("Test", &json!({"code": raw_code, "message": "busy"}));
        assert_eq!(err.code, ErrorCode::RateLimited);
    }
}

#[tokio::test]
async fn test_llm_gateway_streaming_retries_and_errors() {
    let transient_hits = Arc::new(AtomicUsize::new(0));
    let transient_counter = transient_hits.clone();
    let long_wait_hits = Arc::new(AtomicUsize::new(0));
    let long_wait_counter = long_wait_hits.clone();
    let big_body = "x".repeat(MAX_UPSTREAM_ERROR_BYTES + 2048);

    let sse_delta = |delta: &str| {
        Ok::<_, Infallible>(Event::default().data(format!(
            r#"{{"type":"response.output_text.delta","delta":"{delta}"}}"#
        )))
    };
    let sse_done =
        || Ok::<_, Infallible>(Event::default().data(r#"{"type":"response.completed"}"#));

    let app = Router::new()
        .route(
            "/ok/responses",
            post(move |axum::Json(payload): axum::Json<Value>| async move {
                assert_eq!(payload["model"], "mock-model");
                let input = payload["input"].as_array().unwrap();
                let message = input.last().unwrap();
                let mut events = vec![sse_delta("Hello"), sse_delta(" World")];
                if message["content"] != "truncated" {
                    assert_eq!(message["content"], "  hi\n");
                    assert_eq!(
                        &input[1..3],
                        &[
                            json!({"role": "user", "content": "prior input"}),
                            json!({"role": "assistant", "content": "prior output"}),
                        ]
                    );
                    events.push(sse_done());
                }
                Sse::new(futures_util::stream::iter(events)).into_response()
            }),
        )
        .route(
            "/transient/responses",
            post(move || {
                let hits = transient_counter.clone();
                async move {
                    if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                        return (StatusCode::SERVICE_UNAVAILABLE, "retry me").into_response();
                    }
                    Sse::new(futures_util::stream::iter([sse_delta("ok"), sse_done()]))
                        .into_response()
                }
            }),
        )
        .route(
            "/long-wait/responses",
            post(move || {
                let hits = long_wait_counter.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [(RETRY_AFTER, "60")],
                        r#"{"error":{"message":"slow down"}}"#,
                    )
                        .into_response()
                }
            }),
        )
        .route(
            "/big-err/responses",
            post(move || {
                let body = big_body.clone();
                async move { (StatusCode::BAD_GATEWAY, body) }
            }),
        );

    let addr = spawn_mock(app).await;
    let mut ok_cfg = ServiceConfig::test("ok", Protocol::Openai, &format!("http://{addr}/ok"));
    ok_cfg.api_key = Some("test-key".into());
    ok_cfg.models = vec!["mock-model".into()];

    let gateway = LlmGateway::new(&[
        ok_cfg,
        ServiceConfig::test(
            "transient",
            Protocol::Openai,
            &format!("http://{addr}/transient"),
        ),
        ServiceConfig::test(
            "long-wait",
            Protocol::Openai,
            &format!("http://{addr}/long-wait"),
        ),
        ServiceConfig::test(
            "big-err",
            Protocol::Openai,
            &format!("http://{addr}/big-err"),
        ),
    ])
    .unwrap();

    let stream = gateway
        .chat_stream(
            "ok",
            ChatRequest {
                model: Some("mock-model"),
                system_prompt: "sys",
                history: &[
                    ChatMessage {
                        role: HistoryRole::User,
                        content: "prior input",
                    },
                    ChatMessage {
                        role: HistoryRole::Assistant,
                        content: "prior output",
                    },
                ],
                text: "  hi\n",
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        stream.collect::<Vec<_>>().await,
        vec![
            Ok(TextEvent::Text("Hello".into())),
            Ok(TextEvent::Text(" World".into())),
            Ok(TextEvent::Done),
        ]
    );

    let stream = gateway
        .chat_stream(
            "ok",
            ChatRequest {
                system_prompt: "sys",
                text: "truncated",
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let events = stream.collect::<Vec<_>>().await;
    assert_eq!(events.len(), 3);
    assert!(matches!(&events[2], Err(error)
        if error.code == ErrorCode::UpstreamInterrupted && error.message.contains("before a completion event")));

    let stream = gateway
        .chat_stream("transient", ChatRequest::default())
        .await
        .unwrap();
    assert_eq!(
        stream.collect::<Vec<_>>().await,
        vec![Ok(TextEvent::Text("ok".into())), Ok(TextEvent::Done)]
    );
    assert_eq!(transient_hits.load(Ordering::SeqCst), 2);

    let err = gateway
        .chat_stream("long-wait", ChatRequest::default())
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::RateLimited);
    assert_eq!(long_wait_hits.load(Ordering::SeqCst), 1);

    let err = gateway
        .chat_stream("big-err", ChatRequest::default())
        .await
        .err()
        .unwrap();
    assert_eq!(err.code, ErrorCode::UpstreamError);
    assert!(err.message.ends_with('…'));
    assert!(err.message.len() < MAX_UPSTREAM_ERROR_BYTES + 128);
}

#[tokio::test]
async fn upstream_deadline_returns_a_timeout_code() {
    let err = upstream_deadline(
        Duration::ZERO,
        std::future::pending::<()>(),
        "upstream stalled",
    )
    .await
    .unwrap_err();
    assert_eq!(err.code, ErrorCode::UpstreamTimeout);
    assert_eq!(err.message, "upstream stalled");
}
