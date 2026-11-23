use super::{
    DriverRequest, SseAction, apply_custom_headers, provider_error, sensitive_header,
    stream_headers, stream_request,
};
use crate::api::{ErrorCode, ThinkingLevel};
use crate::config::ServiceConfig;
use reqwest::Url;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Value, json};

const DEFAULT_MAX_TOKENS: u32 = 8192;

/// Anthropic Messages protocol driver.
pub struct AnthropicDriver {
    endpoint_url: String,
    headers: HeaderMap,
    max_tokens: u32,
    prompt_cache: bool,
}

impl AnthropicDriver {
    pub fn new(config: &ServiceConfig) -> Result<Self, String> {
        let mut headers = stream_headers();
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2026-10-21"),
        );

        if let Some(key) = config.api_key.as_deref() {
            let trimmed = key.trim();
            if !trimmed.is_empty() {
                headers.insert(
                    HeaderName::from_static("x-api-key"),
                    sensitive_header(trimmed)?,
                );
            }
        }
        apply_custom_headers(&mut headers, &config.headers)?;

        Ok(Self {
            endpoint_url: messages_url(&config.base_url),
            headers,
            max_tokens: config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            prompt_cache: config.prompt_cache,
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

        let mut messages = Vec::with_capacity(history.len() + 1);
        let last_history = history.len().saturating_sub(1);
        for (idx, message) in history.iter().enumerate() {
            let content = if self.prompt_cache && idx == last_history {
                json!([{
                    "type": "text",
                    "text": message.content,
                    "cache_control": { "type": "ephemeral" },
                }])
            } else {
                json!(message.content)
            };
            messages.push(json!({ "role": message.role.as_str(), "content": content }));
        }

        let user_content = if images.is_empty() {
            json!(text)
        } else {
            let mut blocks: Vec<Value> = images
                .iter()
                .map(|img| {
                    json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": img.mime_type,
                            "data": img.data,
                        }
                    })
                })
                .collect();
            blocks.push(json!({ "type": "text", "text": text }));
            Value::Array(blocks)
        };
        messages.push(json!({ "role": "user", "content": user_content }));

        let mut payload = json!({
            "model": model,
            "messages": messages,
            "max_tokens": self.max_tokens,
            "stream": true,
        });
        if !system_prompt.trim().is_empty() {
            payload["system"] = if self.prompt_cache {
                json!([{
                    "type": "text",
                    "text": system_prompt,
                    "cache_control": { "type": "ephemeral" },
                }])
            } else {
                json!(system_prompt)
            };
        }

        let mut output_config = serde_json::Map::new();
        if let Some(level) = thinking {
            let effort = match level {
                ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
                ThinkingLevel::Medium => "medium",
                ThinkingLevel::High => "high",
            };
            output_config.insert("effort".into(), json!(effort));
        }
        if let Some(schema) = response_schema {
            output_config.insert(
                "format".into(),
                json!({
                    "type": "json_schema",
                    "schema": schema,
                }),
            );
        }
        if !output_config.is_empty() {
            payload["output_config"] = Value::Object(output_config);
        }

        stream_request(client, &self.endpoint_url, &self.headers, &payload)
    }
}

fn messages_url(base_url: &str) -> String {
    let Ok(mut url) = Url::parse(base_url) else {
        return base_url.to_string();
    };
    let path = url.path().trim_end_matches('/');
    let endpoint = if path.is_empty()
        && url
            .host_str()
            .is_some_and(|host| host.eq_ignore_ascii_case("api.anthropic.com"))
    {
        "/v1/messages".to_owned()
    } else if path.ends_with("/messages") {
        path.to_owned()
    } else {
        format!("{path}/messages")
    };
    url.set_path(&endpoint);
    url.into()
}

pub fn parse_sse_event(event_type: &str, event_data: &str) -> SseAction {
    let trimmed = event_data.trim();
    if trimmed.is_empty() {
        return SseAction::Ignore;
    }
    if event_type == "message_stop" {
        return SseAction::Finish(Ok(()));
    }

    let val: Value = match serde_json::from_str(trimmed) {
        Ok(val) => val,
        Err(err) => {
            return SseAction::error(
                ErrorCode::UpstreamError,
                format!("Invalid Anthropic stream event: {err}"),
            );
        }
    };

    let msg_type = val
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(event_type);
    if msg_type == "message_stop" {
        return SseAction::Finish(Ok(()));
    }
    if let Some(error) = val.get("error").filter(|e| !e.is_null()) {
        return SseAction::Finish(Err(provider_error("Anthropic", error)));
    }
    if msg_type == "error" || event_type == "error" {
        return SseAction::error(ErrorCode::UpstreamError, "Unknown Anthropic error");
    }

    if msg_type == "message_delta" {
        if let Some(reason) = val.pointer("/delta/stop_reason").and_then(Value::as_str)
            && !matches!(reason, "end_turn" | "stop_sequence")
        {
            let code = match reason {
                "max_tokens" | "model_context_window_exceeded" => ErrorCode::OutputTruncated,
                "refusal" => ErrorCode::ContentBlocked,
                _ => ErrorCode::UpstreamError,
            };
            return SseAction::error(
                code,
                format!("Anthropic translation ended with stop_reason={reason}"),
            );
        }
        return SseAction::Ignore;
    }

    if msg_type == "content_block_delta" || event_type == "content_block_delta" {
        return match val.pointer("/delta/text").and_then(Value::as_str) {
            Some(text) if !text.is_empty() => SseAction::Text(text.to_owned()),
            _ => SseAction::Ignore,
        };
    }

    SseAction::Ignore
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::ImageInput;
    use crate::config::Protocol;
    use crate::llm::{ChatMessage, HistoryRole, request_json};

    #[test]
    fn test_anthropic_driver() {
        let mut cfg = ServiceConfig::test(
            "anthropic-test",
            Protocol::Anthropic,
            "https://api.anthropic.com",
        );
        cfg.api_key = Some("sk-ant-test".into());
        let driver = AnthropicDriver::new(&cfg).unwrap();
        assert_eq!(driver.endpoint_url, "https://api.anthropic.com/v1/messages");
        assert_eq!(
            messages_url("https://proxy.test/v1?tenant=1"),
            "https://proxy.test/v1/messages?tenant=1"
        );
        let mut bad_cfg = cfg.clone();
        bad_cfg.api_key = Some("bad\nkey".into());
        assert!(AnthropicDriver::new(&bad_cfg).is_err());

        cfg.base_url = "https://api.anthropic.com/v1".into();
        cfg.max_tokens = Some(16384);
        let custom_driver = AnthropicDriver::new(&cfg).unwrap();

        let client = reqwest::Client::new();
        let req = custom_driver
            .build_request(
                &client,
                DriverRequest {
                    model: "claude-model",
                    system_prompt: "sys",
                    history: &[ChatMessage {
                        role: HistoryRole::Assistant,
                        content: "previous translation",
                    }],
                    text: "hi",
                    ..Default::default()
                },
            )
            .build()
            .unwrap();
        assert_eq!(req.headers().get("x-api-key").unwrap(), "sk-ant-test");
        assert!(req.headers().get("x-api-key").unwrap().is_sensitive());
        assert_eq!(req.headers()["anthropic-version"], "2026-10-21");
        let payload = request_json(&req);
        assert_eq!(payload["max_tokens"], 16384);
        assert_eq!(payload["system"], "sys");
        assert_eq!(
            payload["messages"],
            json!([
                {"role": "assistant", "content": "previous translation"},
                {"role": "user", "content": "hi"}
            ])
        );

        cfg.prompt_cache = true;
        cfg.headers
            .insert("X-Anthropic-Custom".into(), "custom".into());
        let cached_driver = AnthropicDriver::new(&cfg).unwrap();
        let schema = json!({"type": "object"});
        let images = [ImageInput {
            mime_type: "image/jpeg".into(),
            data: "/9j/4AAQ".into(),
        }];
        let req2 = cached_driver
            .build_request(
                &client,
                DriverRequest {
                    model: "claude-model",
                    system_prompt: "sys",
                    history: &[ChatMessage {
                        role: HistoryRole::Assistant,
                        content: "prev",
                    }],
                    text: "hi",
                    images: &images,
                    thinking: Some(ThinkingLevel::Minimal),
                    response_schema: Some(&schema),
                    ..Default::default()
                },
            )
            .build()
            .unwrap();
        assert_eq!(req2.headers()["x-anthropic-custom"], "custom");
        assert!(req2.headers()["x-anthropic-custom"].is_sensitive());
        let payload2 = request_json(&req2);
        assert_eq!(
            payload2["system"],
            json!([{"type": "text", "text": "sys", "cache_control": {"type": "ephemeral"}}])
        );
        assert_eq!(
            payload2["messages"],
            json!([
                {
                    "role": "assistant",
                    "content": [{"type": "text", "text": "prev", "cache_control": {"type": "ephemeral"}}]
                },
                {
                    "role": "user",
                    "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "/9j/4AAQ"}},
                        {"type": "text", "text": "hi"}
                    ]
                }
            ])
        );
        assert_eq!(
            payload2["output_config"],
            json!({
                "effort": "low",
                "format": {"type": "json_schema", "schema": schema}
            })
        );
        assert!(payload2.get("tools").is_none() && payload2.get("tool_choice").is_none());

        assert!(matches!(
            parse_sse_event(
                "content_block_delta",
                r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#
            ),
            SseAction::Text(t) if t == "Hi"
        ));
        assert!(matches!(
            parse_sse_event(
                "content_block_start",
                r#"{"type":"content_block_start","delta":{"unexpected":[1,2,3]}}"#
            ),
            SseAction::Ignore
        ));
        assert!(matches!(
            parse_sse_event("message_stop", r#"{"type":"message_stop"}"#),
            SseAction::Finish(Ok(()))
        ));
        assert!(matches!(
            parse_sse_event(
                "error",
                r#"{"error":{"type":"timeout_error","message":"upstream stopped waiting"}}"#
            ),
            SseAction::Finish(Err(e)) if e.code == ErrorCode::UpstreamTimeout
        ));
        assert!(matches!(
            parse_sse_event(
                "message_delta",
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#
            ),
            SseAction::Finish(Err(e)) if e.code == ErrorCode::OutputTruncated
        ));
    }
}
