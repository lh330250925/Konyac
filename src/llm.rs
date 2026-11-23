mod anthropic;
mod gemini;
mod openai;
#[cfg(test)]
mod tests;

use crate::api::{ErrorCode, ImageInput, KonyacError, ServiceInfo, ThinkingLevel};
use crate::config::{Protocol, ServiceConfig};
use anthropic::AnthropicDriver;
use eventsource_stream::{Event, Eventsource};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;
use gemini::GeminiDriver;
use openai::OpenaiDriver;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue, RETRY_AFTER};
use serde_json::Value;
use std::collections::BTreeMap;
use std::future::Future;
use std::time::Duration;
use tokio::time::timeout;

const UPSTREAM_HEADERS_TIMEOUT: Duration = Duration::from_secs(60);
const UPSTREAM_EVENT_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_UPSTREAM_ERROR_BYTES: usize = 8 * 1024;
const MAX_PRE_STREAM_RETRIES: usize = 1;
const MAX_RETRY_AFTER: Duration = Duration::from_secs(5);
#[cfg(test)]
const RETRY_DELAY: Duration = Duration::from_millis(10);
#[cfg(not(test))]
const RETRY_DELAY: Duration = Duration::from_millis(400);

#[derive(Debug, PartialEq, Eq)]
pub enum TextEvent {
    Text(String),
    Done,
}

pub type TextStream = BoxStream<'static, Result<TextEvent, KonyacError>>;

#[cfg_attr(test, derive(Default))]
pub struct ChatRequest<'a> {
    pub model: Option<&'a str>,
    pub system_prompt: &'a str,
    pub history: &'a [ChatMessage<'a>],
    pub text: &'a str,
    pub images: &'a [ImageInput],
    pub thinking: Option<ThinkingLevel>,
    pub response_schema: Option<&'a Value>,
    pub cache_key: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HistoryRole {
    User,
    Assistant,
}

impl HistoryRole {
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ChatMessage<'a> {
    pub role: HistoryRole,
    pub content: &'a str,
}

enum SseAction {
    Text(String),
    Finish(Result<(), KonyacError>),
    Ignore,
}

impl SseAction {
    fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Finish(Err(KonyacError::new(code, message)))
    }
}

enum Driver {
    Openai(OpenaiDriver),
    Anthropic(AnthropicDriver),
    Gemini(GeminiDriver),
}

#[derive(Clone, Copy)]
enum StreamFormat {
    Openai,
    Anthropic,
    Gemini,
}

#[cfg_attr(test, derive(Default))]
struct DriverRequest<'a> {
    model: &'a str,
    system_prompt: &'a str,
    history: &'a [ChatMessage<'a>],
    text: &'a str,
    images: &'a [ImageInput],
    thinking: Option<ThinkingLevel>,
    response_schema: Option<&'a Value>,
    cache_key: Option<&'a str>,
}

impl Driver {
    fn new(config: &ServiceConfig) -> Result<Self, String> {
        match config.protocol {
            Protocol::Openai => Ok(Self::Openai(OpenaiDriver::new(config)?)),
            Protocol::Anthropic => Ok(Self::Anthropic(AnthropicDriver::new(config)?)),
            Protocol::Gemini => Ok(Self::Gemini(GeminiDriver::new(config)?)),
        }
    }

    fn build_request(
        &self,
        client: &reqwest::Client,
        request: DriverRequest<'_>,
    ) -> (reqwest::RequestBuilder, StreamFormat) {
        match self {
            Self::Openai(driver) => (driver.build_request(client, request), StreamFormat::Openai),
            Self::Anthropic(driver) => (
                driver.build_request(client, request),
                StreamFormat::Anthropic,
            ),
            Self::Gemini(driver) => (driver.build_request(client, request), StreamFormat::Gemini),
        }
    }
}

impl StreamFormat {
    fn parse_sse(self, event: &Event) -> SseAction {
        if event.event == "ping" {
            return SseAction::Ignore;
        }
        match self {
            Self::Openai => openai::parse_sse_event(&event.event, &event.data),
            Self::Anthropic => anthropic::parse_sse_event(&event.event, &event.data),
            Self::Gemini => gemini::parse_sse_event(&event.event, &event.data),
        }
    }
}

fn stream_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    headers
}

fn sensitive_header(value: &str) -> Result<HeaderValue, String> {
    let mut header =
        HeaderValue::from_str(value).map_err(|e| format!("Invalid API key header: {e}"))?;
    header.set_sensitive(true);
    Ok(header)
}

fn apply_custom_headers(
    headers: &mut HeaderMap,
    custom: &BTreeMap<String, String>,
) -> Result<(), String> {
    for (name, value) in custom {
        let header_name = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|e| format!("Invalid custom header name '{name}': {e}"))?;
        headers.insert(header_name, sensitive_header(value.trim())?);
    }
    Ok(())
}

#[cfg(test)]
fn request_json(request: &reqwest::Request) -> Value {
    serde_json::from_slice(request.body().unwrap().as_bytes().unwrap()).unwrap()
}

fn stream_request(
    client: &reqwest::Client,
    endpoint: &str,
    headers: &HeaderMap,
    payload: &Value,
) -> reqwest::RequestBuilder {
    client.post(endpoint).headers(headers.clone()).json(payload)
}

fn status_code(status: u64) -> Option<ErrorCode> {
    match status {
        429 => Some(ErrorCode::RateLimited),
        408 | 504 => Some(ErrorCode::UpstreamTimeout),
        _ => None,
    }
}

fn error_code_from_value(value: &Value) -> Option<ErrorCode> {
    match value.as_str() {
        Some(
            "rate_limit_exceeded"
            | "rate_limit_error"
            | "RESOURCE_EXHAUSTED"
            | "insufficient_quota"
            | "overloaded_error",
        ) => Some(ErrorCode::RateLimited),
        Some(
            "timeout_error" | "request_timeout" | "DEADLINE_EXCEEDED" | "deadline_exceeded"
            | "gateway_timeout",
        ) => Some(ErrorCode::UpstreamTimeout),
        Some(
            "safety" | "recitation" | "language" | "prohibited_content" | "spii" | "blocklist"
            | "content_blocked",
        ) => Some(ErrorCode::ContentBlocked),
        _ => status_code(value.as_u64()?),
    }
}

fn provider_error_code(err: &Value) -> Option<ErrorCode> {
    ["code", "type", "status"]
        .into_iter()
        .filter_map(|key| err.get(key))
        .find_map(error_code_from_value)
}

fn provider_error(provider: &str, err: &Value) -> KonyacError {
    let code = provider_error_code(err).unwrap_or(ErrorCode::UpstreamError);
    let message = err
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("Unknown upstream error");
    KonyacError::new(code, format!("{provider} API error: {message}"))
}

fn upstream_http_error(protocol: Protocol, status: reqwest::StatusCode, body: &str) -> KonyacError {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let provider_err = parsed.as_ref().and_then(|v| v.get("error"));
    let code = status_code(status.as_u16().into())
        .or_else(|| provider_err.and_then(provider_error_code))
        .unwrap_or(ErrorCode::UpstreamError);
    let detail = provider_err
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .unwrap_or(body)
        .trim();
    let mut message = format!("{protocol} API error ({status})");
    if !detail.is_empty() {
        message.push_str(": ");
        message.push_str(detail);
    }
    KonyacError::new(code, message)
}

async fn upstream_deadline<T>(
    duration: Duration,
    operation: impl Future<Output = T>,
    message: &'static str,
) -> Result<T, KonyacError> {
    timeout(duration, operation)
        .await
        .map_err(|_| KonyacError::new(ErrorCode::UpstreamTimeout, message))
}

fn create_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .http2_keep_alive_interval(Some(Duration::from_secs(30)))
        .http2_keep_alive_timeout(Duration::from_secs(10))
        .http2_keep_alive_while_idle(true)
        .pool_max_idle_per_host(10)
        .connect_timeout(Duration::from_secs(10))
        .build()
}

fn retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    let Some(raw) = headers.get(RETRY_AFTER).and_then(|v| v.to_str().ok()) else {
        return Some(RETRY_DELAY);
    };
    if let Ok(secs) = raw.trim().parse::<u64>() {
        let delay = Duration::from_secs(secs);
        return (delay <= MAX_RETRY_AFTER).then_some(delay);
    }
    Some(RETRY_DELAY)
}

async fn read_bounded_error_body(mut response: reqwest::Response) -> String {
    let mut buf = Vec::new();
    let mut truncated = false;
    let _ = timeout(Duration::from_secs(5), async {
        while let Ok(Some(chunk)) = response.chunk().await {
            let remaining = MAX_UPSTREAM_ERROR_BYTES.saturating_sub(buf.len());
            if chunk.len() > remaining {
                buf.extend_from_slice(&chunk[..remaining]);
                truncated = true;
                break;
            }
            buf.extend_from_slice(&chunk);
        }
    })
    .await;
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        text.push('…');
    }
    text
}

struct ServiceEntry {
    protocol: Protocol,
    default_thinking: Option<ThinkingLevel>,
    info: ServiceInfo,
    driver: Driver,
}

impl ServiceEntry {
    fn new(config: &ServiceConfig) -> Result<Self, String> {
        Ok(Self {
            protocol: config.protocol,
            default_thinking: config.thinking,
            info: ServiceInfo {
                name: config.name.clone(),
                protocol: config.protocol.to_string(),
                models: config.models.clone(),
                thinking: config.thinking,
            },
            driver: Driver::new(config)?,
        })
    }

    fn resolve_model<'a>(&'a self, model: Option<&'a str>) -> Result<&'a str, KonyacError> {
        let service_name = &self.info.name;
        let models = &self.info.models;
        let Some(model) = model else {
            return models.first().map(String::as_str).ok_or_else(|| {
                KonyacError::new(
                    ErrorCode::ModelNotConfigured,
                    format!("LLM service '{service_name}' has no configured models"),
                )
            });
        };
        if models.iter().any(|m| m == model) {
            Ok(model)
        } else {
            Err(KonyacError::new(
                ErrorCode::ModelNotConfigured,
                format!(
                    "Model '{model}' is not configured for LLM service '{service_name}'. Configured models: {models:?}"
                ),
            ))
        }
    }
}

/// Gateway aggregating multiple LLM backends and streaming responses.
pub struct LlmGateway {
    client: reqwest::Client,
    services: Vec<ServiceEntry>,
}

impl LlmGateway {
    pub fn new(services: &[ServiceConfig]) -> Result<Self, Box<dyn std::error::Error>> {
        let client = create_http_client()?;
        let services = services
            .iter()
            .map(ServiceEntry::new)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { client, services })
    }

    pub fn list_services(&self) -> impl ExactSizeIterator<Item = &ServiceInfo> {
        self.services.iter().map(|entry| &entry.info)
    }

    pub async fn chat_stream(
        &self,
        service: &str,
        request: ChatRequest<'_>,
    ) -> Result<TextStream, KonyacError> {
        let entry = self.resolve_service(service)?;
        let model = entry.resolve_model(request.model)?;
        let thinking = request.thinking.or(entry.default_thinking);
        let protocol = entry.protocol;

        let (response, stream_format) = {
            let mut attempt = 0;
            loop {
                let (req_builder, stream_format) = entry.driver.build_request(
                    &self.client,
                    DriverRequest {
                        model,
                        system_prompt: request.system_prompt,
                        history: request.history,
                        text: request.text,
                        images: request.images,
                        thinking,
                        response_schema: request.response_schema,
                        cache_key: request.cache_key,
                    },
                );
                let send_result = upstream_deadline(
                    UPSTREAM_HEADERS_TIMEOUT,
                    req_builder.send(),
                    "Timed out waiting for upstream response headers",
                )
                .await?;

                match send_result {
                    Ok(response) if response.status().is_success() => {
                        break (response, stream_format);
                    }
                    Ok(response) => {
                        let status = response.status();
                        let retry_delay = (attempt < MAX_PRE_STREAM_RETRIES
                            && matches!(status.as_u16(), 429 | 502 | 503 | 504))
                        .then(|| retry_after_delay(response.headers()))
                        .flatten();
                        if let Some(delay) = retry_delay {
                            attempt += 1;
                            drop(response);
                            tokio::time::sleep(delay).await;
                            continue;
                        }
                        let err_body = read_bounded_error_body(response).await;
                        return Err(upstream_http_error(protocol, status, &err_body));
                    }
                    Err(e) => {
                        if attempt < MAX_PRE_STREAM_RETRIES
                            && !e.is_timeout()
                            && (e.is_connect() || e.is_request())
                        {
                            attempt += 1;
                            tokio::time::sleep(RETRY_DELAY).await;
                            continue;
                        }
                        let code = if e.is_timeout() {
                            ErrorCode::UpstreamTimeout
                        } else {
                            ErrorCode::UpstreamError
                        };
                        return Err(KonyacError::new(code, format!("HTTP request failed: {e}")));
                    }
                }
            }
        };

        let event_stream = response.bytes_stream().eventsource();
        let text_stream =
            futures_util::stream::try_unfold(Some(event_stream), move |upstream| async move {
                let Some(mut upstream) = upstream else {
                    return Ok(None);
                };
                upstream_deadline(
                    UPSTREAM_EVENT_TIMEOUT,
                    async {
                        while let Some(event) = upstream.next().await {
                            let event = event.map_err(|e| {
                                KonyacError::new(
                                    ErrorCode::UpstreamInterrupted,
                                    format!("SSE stream read error: {e}"),
                                )
                            })?;
                            let (item, next) = match stream_format.parse_sse(&event) {
                                SseAction::Ignore => continue,
                                SseAction::Text(text) => (TextEvent::Text(text), Some(upstream)),
                                SseAction::Finish(result) => {
                                    result?;
                                    (TextEvent::Done, None)
                                }
                            };
                            return Ok(Some((item, next)));
                        }
                        Err(KonyacError::new(
                            ErrorCode::UpstreamInterrupted,
                            "Upstream stream ended before a completion event",
                        ))
                    },
                    "Timed out waiting for upstream translation data",
                )
                .await?
            });

        Ok(text_stream.boxed())
    }

    fn resolve_service(&self, service: &str) -> Result<&ServiceEntry, KonyacError> {
        self.services
            .iter()
            .find(|entry| entry.info.name == service)
            .ok_or_else(|| {
                let available: Vec<_> = self.list_services().map(|info| &info.name).collect();
                KonyacError::new(
                    ErrorCode::ServiceNotConfigured,
                    format!("LLM service '{service}' not found. Available services: {available:?}"),
                )
            })
    }
}
