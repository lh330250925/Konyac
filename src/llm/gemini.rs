use super::{
    DriverRequest, HistoryRole, SseAction, apply_custom_headers, provider_error, sensitive_header,
    stream_headers, stream_request,
};
use crate::api::ErrorCode;
use crate::config::ServiceConfig;
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName};
use serde_json::{Value, json};

/// Google Gemini Interactions API driver.
pub struct GeminiDriver {
    endpoint_url: String,
    headers: HeaderMap,
    max_tokens: Option<u32>,
}

impl GeminiDriver {
    pub fn new(config: &ServiceConfig) -> Result<Self, String> {
        let mut headers = stream_headers();
        if let Some(key) = config.api_key.as_deref() {
            let key = key.trim();
            if !key.is_empty() {
                headers.insert(
                    HeaderName::from_static("x-goog-api-key"),
                    sensitive_header(key)?,
                );
            }
        }
        apply_custom_headers(&mut headers, &config.headers)?;
        Ok(Self {
            endpoint_url: interactions_url(&config.base_url),
            headers,
            max_tokens: config.max_tokens,
        })
    }

    pub fn build_request(
        &self,
        client: &reqwest::Client,
        request: DriverRequest<'_>,
    ) -> reqwest::RequestBuilder {
        let DriverRequest {
            model,
            system_prompt,
            history,
            text,
            images,
            thinking,
            response_schema,
            cache_key: _,
        } = request;
        let mut input = Vec::with_capacity(history.len() + 1);
        input.extend(history.iter().map(|message| {
            let kind = match message.role {
                HistoryRole::User => "user_input",
                HistoryRole::Assistant => "model_output",
            };
            json!({
                "type": kind,
                "content": [{ "type": "text", "text": message.content }],
            })
        }));

        let mut user_content: Vec<Value> = images
            .iter()
            .map(|img| {
                json!({
                    "type": "image",
                    "mime_type": img.mime_type,
                    "data": img.data,
                })
            })
            .collect();
        user_content.push(json!({ "type": "text", "text": text }));
        input.push(json!({
            "type": "user_input",
            "content": user_content,
        }));

        let mut payload = json!({
            "model": model.trim_start_matches("models/"),
            "input": input,
            "stream": true,
            "store": false,
        });
        if !system_prompt.trim().is_empty() {
            payload["system_instruction"] = json!(system_prompt);
        }
        if let Some(schema) = response_schema {
            payload["response_format"] = json!({
                "type": "text",
                "mime_type": "application/json",
                "schema": schema,
            });
        }
        let mut generation_config = serde_json::Map::new();
        if let Some(max_tokens) = self.max_tokens {
            generation_config.insert("max_output_tokens".into(), json!(max_tokens));
        }
        if let Some(level) = thinking {
            generation_config.insert("thinking_level".into(), json!(level.as_str()));
        }
        if !generation_config.is_empty() {
            payload["generation_config"] = Value::Object(generation_config);
        }

        stream_request(client, &self.endpoint_url, &self.headers, &payload)
    }
}

fn interactions_url(base_url: &str) -> String {
    let Ok(mut url) = Url::parse(base_url) else {
        return base_url.to_string();
    };
    let path = url.path().trim_end_matches('/');
    let new_path = if path.ends_with("/interactions") {
        path.to_owned()
    } else if path.ends_with("/v1") || path.ends_with("/v1beta") {
        format!("{path}/interactions")
    } else {
        format!("{path}/v1/interactions")
    };
    url.set_path(&new_path);
    url.into()
}

pub fn parse_sse_event(event_type: &str, data: &str) -> SseAction {
    if event_type == "done" || data.trim().is_empty() {
        return SseAction::Ignore;
    }
    let val: Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(error) => {
            return SseAction::error(
                ErrorCode::UpstreamError,
                format!("Invalid Gemini stream event: {error}"),
            );
        }
    };
    if let Some(error) = val.get("error").filter(|e| !e.is_null()) {
        return SseAction::Finish(Err(provider_error("Gemini", error)));
    }

    match val
        .get("event_type")
        .and_then(Value::as_str)
        .unwrap_or(event_type)
    {
        "step.start" => {
            let Some(step) = val
                .get("step")
                .filter(|s| s.get("type").and_then(Value::as_str) == Some("model_output"))
            else {
                return SseAction::Ignore;
            };
            let Some(parts) = step.get("content").and_then(Value::as_array) else {
                return SseAction::Ignore;
            };
            let text: String = parts
                .iter()
                .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|part| part.get("text").and_then(Value::as_str))
                .collect();
            if text.is_empty() {
                SseAction::Ignore
            } else {
                SseAction::Text(text)
            }
        }
        "step.delta" => match val.get("delta") {
            Some(delta) if delta.get("type").and_then(Value::as_str) == Some("text") => {
                match delta.get("text").and_then(Value::as_str) {
                    Some(text) if !text.is_empty() => SseAction::Text(text.to_owned()),
                    _ => SseAction::Ignore,
                }
            }
            _ => SseAction::Ignore,
        },
        "interaction.completed" => {
            if let Some(error) = val.pointer("/interaction/error").filter(|e| !e.is_null()) {
                return SseAction::Finish(Err(provider_error("Gemini", error)));
            }
            let status = val
                .pointer("/interaction/status")
                .and_then(Value::as_str)
                .or_else(|| val.get("status").and_then(Value::as_str));
            match status {
                Some("completed") => SseAction::Finish(Ok(())),
                Some(status) => terminal_error(status),
                None => SseAction::error(
                    ErrorCode::UpstreamError,
                    "Gemini completed without a status",
                ),
            }
        }
        "interaction.status_update" => match val.get("status").and_then(Value::as_str) {
            Some(status @ ("incomplete" | "requires_continuation" | "cancelled" | "failed")) => {
                terminal_error(status)
            }
            _ => SseAction::Ignore,
        },
        "error" => SseAction::error(ErrorCode::UpstreamError, "Unknown Gemini interaction error"),
        _ => SseAction::Ignore,
    }
}

fn terminal_error(status: &str) -> SseAction {
    let code = match status {
        "incomplete" | "requires_continuation" => ErrorCode::OutputTruncated,
        "cancelled" => ErrorCode::UpstreamInterrupted,
        _ => ErrorCode::UpstreamError,
    };
    SseAction::error(
        code,
        format!("Gemini interaction ended with status={status}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ImageInput, ThinkingLevel};
    use crate::config::Protocol;
    use crate::llm::{ChatMessage, request_json};

    #[test]
    fn builds_stateless_interactions_with_text_history() {
        let mut cfg = ServiceConfig::test(
            "gemini-test",
            Protocol::Gemini,
            "https://generativelanguage.googleapis.com",
        );
        cfg.api_key = Some("test-key".into());
        let driver = GeminiDriver::new(&cfg).unwrap();
        let client = reqwest::Client::new();
        let history = [ChatMessage {
            role: HistoryRole::Assistant,
            content: "previous translation",
        }];
        let request = driver
            .build_request(
                &client,
                DriverRequest {
                    model: "models/gemini-3.8-flash",
                    system_prompt: "sys",
                    history: &history,
                    text: "hi",
                    ..Default::default()
                },
            )
            .build()
            .unwrap();
        assert_eq!(
            request.url().as_str(),
            "https://generativelanguage.googleapis.com/v1/interactions"
        );
        assert_eq!(request.headers()["x-goog-api-key"], "test-key");
        assert!(request.headers()["x-goog-api-key"].is_sensitive());

        let mut bad_cfg = cfg.clone();
        bad_cfg.api_key = Some("bad\nkey".into());
        assert!(GeminiDriver::new(&bad_cfg).is_err());

        assert_eq!(
            request_json(&request),
            json!({
                "model": "gemini-3.8-flash",
                "system_instruction": "sys",
                "stream": true,
                "store": false,
                "input": [
                    {"type":"model_output","content":[{"type":"text","text":"previous translation"}]},
                    {"type":"user_input","content":[{"type":"text","text":"hi"}]}
                ]
            })
        );

        cfg.max_tokens = Some(4096);
        cfg.headers.insert("X-Tenant".into(), "team-a".into());
        let enhanced_driver = GeminiDriver::new(&cfg).unwrap();
        let images = [ImageInput {
            mime_type: "image/webp".into(),
            data: "UklGR".into(),
        }];
        let schema = json!({"type": "object", "properties": {"translation": {"type": "string"}}});
        let req2 = enhanced_driver
            .build_request(
                &client,
                DriverRequest {
                    model: "gemini-3.8-flash",
                    system_prompt: "sys",
                    text: "hi",
                    images: &images,
                    thinking: Some(ThinkingLevel::High),
                    response_schema: Some(&schema),
                    ..Default::default()
                },
            )
            .build()
            .unwrap();
        assert_eq!(req2.headers()["x-tenant"], "team-a");
        assert!(req2.headers()["x-tenant"].is_sensitive());
        let body2 = request_json(&req2);
        assert_eq!(
            body2["generation_config"],
            json!({"max_output_tokens": 4096, "thinking_level": "high"})
        );
        assert_eq!(
            body2.pointer("/response_format"),
            Some(&json!({"type": "text", "mime_type": "application/json", "schema": schema}))
        );
        assert!(body2.get("tools").is_none() && body2.get("tool_choice").is_none());
        assert_eq!(
            body2["input"][0],
            json!({
                "type": "user_input",
                "content": [
                    {"type": "image", "mime_type": "image/webp", "data": "UklGR"},
                    {"type": "text", "text": "hi"}
                ]
            })
        );
    }

    #[test]
    fn resolves_interactions_endpoints() {
        for (base, expected) in [
            (
                "https://example.test/proxy/",
                "https://example.test/proxy/v1/interactions",
            ),
            (
                "https://example.test/proxy/v1",
                "https://example.test/proxy/v1/interactions",
            ),
            (
                "https://generativelanguage.googleapis.com/v1beta",
                "https://generativelanguage.googleapis.com/v1beta/interactions",
            ),
            (
                "https://example.test/proxy/v1/interactions/?key=1",
                "https://example.test/proxy/v1/interactions?key=1",
            ),
        ] {
            assert_eq!(interactions_url(base), expected);
        }
    }

    #[test]
    fn reads_interaction_output_and_terminal_events() {
        assert!(matches!(
            parse_sse_event("step.start", r#"{"step":{"type":"model_output","content":[{"type":"text","text":"Hello"},{"type":"text","text":" world"}]}}"#),
            SseAction::Text(text) if text == "Hello world"
        ));
        assert!(matches!(
            parse_sse_event("message", r#"{"event_type":"step.delta","delta":{"type":"text","text":"!"}}"#),
            SseAction::Text(text) if text == "!"
        ));
        assert!(matches!(
            parse_sse_event(
                "step.delta",
                r#"{"delta":{"type":"thought_summary","content":{"type":"text","text":"thinking"}}}"#
            ),
            SseAction::Ignore
        ));
        assert!(matches!(
            parse_sse_event(
                "interaction.completed",
                r#"{"interaction":{"status":"completed"}}"#
            ),
            SseAction::Finish(Ok(()))
        ));
        assert!(matches!(
            parse_sse_event(
                "interaction.completed",
                r#"{"interaction":{"status":"incomplete"}}"#
            ),
            SseAction::Finish(Err(error)) if error.code == ErrorCode::OutputTruncated
        ));
        assert!(matches!(
            parse_sse_event(
                "error",
                r#"{"error":{"code":"rate_limit_exceeded","message":"quota"}}"#
            ),
            SseAction::Finish(Err(error)) if error.code == ErrorCode::RateLimited
        ));
        assert!(matches!(
            parse_sse_event(
                "error",
                r#"{"error":{"code":"gateway_timeout","message":"deadline"}}"#
            ),
            SseAction::Finish(Err(error)) if error.code == ErrorCode::UpstreamTimeout
        ));
        assert!(matches!(
            parse_sse_event("error", r#"{"error":{"code":"safety","message":"blocked"}}"#),
            SseAction::Finish(Err(error)) if error.code == ErrorCode::ContentBlocked
        ));
        assert!(matches!(
            parse_sse_event(
                "step.start",
                r#"{"step":{"type":"thought","content":{"unexpected":true}}}"#
            ),
            SseAction::Ignore
        ));
        assert!(matches!(
            parse_sse_event(
                "interaction.status_update",
                r#"{"event_type":"interaction.status_update","status":"failed"}"#
            ),
            SseAction::Finish(Err(error)) if error.code == ErrorCode::UpstreamError
        ));
    }
}
