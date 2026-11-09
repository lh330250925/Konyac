use super::AppState;
use super::types::{ErrorResponse, HealthResponse, TranslateRequest};
use crate::llm::{LlmError, ServiceInfo};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures::StreamExt;
use std::convert::Infallible;

fn error_response(status: StatusCode, error: impl Into<String>) -> Response {
    (
        status,
        Json(ErrorResponse {
            error: error.into(),
        }),
    )
        .into_response()
}

/// GET /health - Daemon health check endpoint.
pub async fn handle_health(State(state): State<&'static AppState>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        services_count: state.gateway.list_services().len(),
    })
}

/// GET /services - Lists all available LLM services and configured models.
pub async fn handle_services(
    State(state): State<&'static AppState>,
) -> Json<&'static [ServiceInfo]> {
    Json(state.gateway.list_services())
}

/// POST /translate - Handles translation requests with SSE streaming.
pub async fn handle_translate(
    State(state): State<&'static AppState>,
    Json(req): Json<TranslateRequest>,
) -> Response {
    let (text, target_lang, service, model) = match req.validate() {
        Ok(v) => v,
        Err(e) => return error_response(StatusCode::BAD_REQUEST, e),
    };

    let system_prompt = state.prompt.resolve(target_lang, req.prompt.as_ref());

    let text_stream = match state
        .gateway
        .chat_stream(
            service,
            model,
            &system_prompt,
            req.history.as_deref(),
            text,
            req.json_schema.as_ref(),
        )
        .await
    {
        Ok(stream) => stream,
        Err(LlmError::InvalidRequest(e)) => return error_response(StatusCode::BAD_REQUEST, e),
        Err(LlmError::UpstreamError(e)) => return error_response(StatusCode::BAD_GATEWAY, e),
    };

    let sse_stream = text_stream.map(|res| match res {
        Ok(chunk) => Ok::<_, Infallible>(Event::default().data(chunk)),
        Err(e) => Ok::<_, Infallible>(Event::default().event("error").data(e)),
    });

    Sse::new(sse_stream)
        .keep_alive(KeepAlive::default())
        .into_response()
}
