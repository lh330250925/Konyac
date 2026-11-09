pub(crate) mod drivers;
pub mod types;

use crate::config::{Protocol, RuntimeServiceConfig};
use crate::llm::drivers::{
    Driver, SseAction, anthropic::AnthropicDriver, gemini::GeminiDriver, openai::OpenaiDriver,
};
use eventsource_stream::Eventsource;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::collections::HashMap;
use std::time::Duration;

pub use types::{HistoryMessage, ServiceInfo};

/// Unified LLM gateway error type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LlmError {
    /// Invalid client request (e.g. unknown service or unconfigured model).
    InvalidRequest(String),
    /// Upstream network or provider API error.
    UpstreamError(String),
}

impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidRequest(msg) => write!(f, "{msg}"),
            Self::UpstreamError(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for LlmError {}

/// Creates the default HTTP client.
pub fn create_http_client() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .tcp_nodelay(true)
        .tcp_keepalive(Some(Duration::from_secs(60)))
        .pool_idle_timeout(Some(Duration::from_secs(90)))
        .pool_max_idle_per_host(10)
        .connect_timeout(Duration::from_secs(10))
        .build()
}

/// Internal service entry.
struct ServiceEntry {
    protocol: Protocol,
    configured_models: Vec<String>,
    driver: Driver,
}

impl ServiceEntry {
    fn new(config: &RuntimeServiceConfig) -> Self {
        let driver = match config.protocol {
            Protocol::Openai => Driver::Openai(OpenaiDriver::new(
                &config.base_url,
                config.api_key.as_deref(),
            )),
            Protocol::Anthropic => Driver::Anthropic(AnthropicDriver::new(
                &config.base_url,
                config.api_key.as_deref(),
            )),
            Protocol::Gemini => Driver::Gemini(GeminiDriver::new(
                &config.base_url,
                config.api_key.as_deref(),
            )),
        };

        Self {
            protocol: config.protocol,
            configured_models: config.models.clone(),
            driver,
        }
    }

    fn validate_model<'a>(&self, service_name: &str, model: &'a str) -> Result<&'a str, String> {
        if self.configured_models.iter().any(|m| m == model) {
            Ok(model)
        } else {
            Err(format!(
                "Model '{model}' is not configured for LLM service '{service_name}'. Configured models: {:?}",
                self.configured_models
            ))
        }
    }
}

/// Gateway aggregating multiple LLM backends and streaming responses.
pub struct LlmGateway {
    client: reqwest::Client,
    services: HashMap<String, ServiceEntry>,
    services_info: Vec<ServiceInfo>,
}

impl LlmGateway {
    /// Initializes the gateway from runtime service configurations.
    pub fn new(services: &[RuntimeServiceConfig]) -> Result<Self, Box<dyn std::error::Error>> {
        let client = create_http_client()?;
        Self::with_client(services, client)
    }

    /// Initializes the gateway with a custom HTTP client.
    pub fn with_client(
        services: &[RuntimeServiceConfig],
        client: reqwest::Client,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let mut map = HashMap::with_capacity(services.len());
        let mut services_info = Vec::with_capacity(services.len());

        for service_cfg in services {
            let entry = ServiceEntry::new(service_cfg);
            services_info.push(ServiceInfo {
                name: service_cfg.name.clone(),
                protocol: entry.protocol.to_string(),
                configured_models: entry.configured_models.clone(),
            });
            map.insert(service_cfg.name.clone(), entry);
        }

        services_info.sort_by(|a, b| a.name.cmp(&b.name));

        Ok(Self {
            client,
            services: map,
            services_info,
        })
    }

    /// Lists metadata for all registered services.
    #[inline]
    pub fn list_services(&self) -> &[ServiceInfo] {
        &self.services_info
    }

    /// Initiates a streaming chat request to the specified backend service.
    pub async fn chat_stream(
        &self,
        service: &str,
        model: &str,
        system_prompt: &str,
        history: Option<&[HistoryMessage]>,
        text: &str,
        json_schema: Option<&serde_json::Value>,
    ) -> Result<BoxStream<'static, Result<String, String>>, LlmError> {
        let entry = self.resolve_service(service).map_err(LlmError::InvalidRequest)?;
        let model = entry.validate_model(service, model).map_err(LlmError::InvalidRequest)?;

        let req_builder = entry.driver.build_request(
            &self.client,
            model,
            system_prompt,
            history,
            text,
            json_schema,
        );

        let response = req_builder
            .send()
            .await
            .map_err(|e| LlmError::UpstreamError(format!("HTTP request failed: {e}")))?;

        if !response.status().is_success() {
            let status = response.status();
            let err_body = response.text().await.unwrap_or_default();
            return Err(LlmError::UpstreamError(format!(
                "{} API error ({status}): {err_body}",
                entry.driver.driver_name()
            )));
        }

        let event_stream = response.bytes_stream().eventsource();
        let driver = entry.driver.clone();

        let state = (event_stream, driver, Option::<String>::None, false, false);
        let text_stream = futures::stream::unfold(
            state,
            move |(mut event_stream, driver, mut pending_done, mut done_emitted, mut errored)| async move {
                loop {
                    if let Some(done) = pending_done.take() {
                        return Some((
                            Ok(done),
                            (event_stream, driver, pending_done, done_emitted, errored),
                        ));
                    }
                    if errored {
                        return None;
                    }
                    match event_stream.next().await {
                        Some(Ok(event)) => match driver.parse_sse(&event) {
                            SseAction::Text(text) => {
                                return Some((
                                    Ok(text.into_owned()),
                                    (event_stream, driver, pending_done, done_emitted, errored),
                                ));
                            }
                            SseAction::TextAndDone(text) => {
                                if !done_emitted {
                                    done_emitted = true;
                                    pending_done = Some("[DONE]".to_string());
                                }
                                return Some((
                                    Ok(text.into_owned()),
                                    (event_stream, driver, pending_done, done_emitted, errored),
                                ));
                            }
                            SseAction::Done => {
                                if !done_emitted {
                                    done_emitted = true;
                                    return Some((
                                        Ok("[DONE]".to_string()),
                                        (event_stream, driver, pending_done, done_emitted, errored),
                                    ));
                                }
                            }
                            SseAction::Error(e) => {
                                errored = true;
                                return Some((
                                    Err(e),
                                    (event_stream, driver, pending_done, done_emitted, errored),
                                ));
                            }
                            SseAction::Ignore => continue,
                        },
                        Some(Err(e)) => {
                            errored = true;
                            return Some((
                                Err(format!("SSE stream read error: {e}")),
                                (event_stream, driver, pending_done, done_emitted, errored),
                            ));
                        }
                        None => {
                            if !done_emitted {
                                done_emitted = true;
                                return Some((
                                    Ok("[DONE]".to_string()),
                                    (event_stream, driver, pending_done, done_emitted, errored),
                                ));
                            }
                            return None;
                        }
                    }
                }
            },
        );

        Ok(text_stream.boxed())
    }

    /// Resolves a service entry by name.
    fn resolve_service(&self, service: &str) -> Result<&ServiceEntry, String> {
        self.services.get(service).ok_or_else(|| {
            let mut available: Vec<_> = self.services.keys().cloned().collect();
            available.sort();
            format!("LLM service '{service}' not found. Available services: {available:?}")
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_llm_gateway_resolution() {
        assert!(LlmGateway::new(&[]).unwrap().list_services().is_empty());

        let services = vec![RuntimeServiceConfig {
            name: "ollama".into(),
            protocol: Protocol::Openai,
            base_url: "http://localhost:11434/v1".into(),
            api_key: None,
            models: vec!["qwen2.5".into()],
        }];

        let gateway = LlmGateway::new(&services).unwrap();
        assert_eq!(gateway.list_services().len(), 1);

        let entry = gateway.resolve_service("ollama").unwrap();
        assert!(entry.validate_model("ollama", "qwen2.5").is_ok());
        assert!(entry.validate_model("ollama", "unknown").is_err());
        assert!(gateway.resolve_service("non-existent").is_err());
    }

    #[tokio::test]
    async fn test_llm_gateway_streaming() {
        use axum::response::IntoResponse;
        use axum::response::sse::{Event, Sse};
        use axum::routing::post;
        use axum::Router;
        use std::convert::Infallible;

        let app = Router::new().route(
            "/v1/chat/completions",
            post(|axum::Json(payload): axum::Json<serde_json::Value>| async move {
                assert_eq!(payload["model"], "mock-model");
                let stream = futures::stream::iter(vec![
                    Ok::<_, Infallible>(
                        Event::default()
                            .data(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#),
                    ),
                    Ok::<_, Infallible>(
                        Event::default()
                            .data(r#"{"choices":[{"delta":{"content":" World"}}]}"#),
                    ),
                    Ok::<_, Infallible>(Event::default().data("[DONE]")),
                ]);
                Sse::new(stream).into_response()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let configs = vec![RuntimeServiceConfig {
            name: "test-openai".into(),
            protocol: Protocol::Openai,
            base_url: format!("http://127.0.0.1:{port}/v1"),
            api_key: Some("test-key".into()),
            models: vec!["mock-model".into()],
        }];

        let gateway = LlmGateway::new(&configs).unwrap();
        let mut stream = gateway
            .chat_stream("test-openai", "mock-model", "sys", None, "hi", None)
            .await
            .unwrap();

        let mut chunks = Vec::new();
        while let Some(item) = stream.next().await {
            chunks.push(item.unwrap());
        }
        assert_eq!(chunks, vec!["Hello", " World", "[DONE]"]);
    }
}
