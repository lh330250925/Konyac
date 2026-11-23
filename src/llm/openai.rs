use super::{
    DriverRequest, SseAction, apply_custom_headers, provider_error, sensitive_header,
    stream_headers, stream_request,
};
use crate::api::ErrorCode;
use crate::config::ServiceConfig;
use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap};
use serde_json::{Value, json};

/// OpenAI Responses API driver.
pub struct OpenaiDriver {
    service_name: String,
    endpoint_url: String,
    headers: HeaderMap,
    max_tokens: Option<u32>,
    prompt_cache: bool,
}

impl OpenaiDriver {
    pub fn new(config: &ServiceConfig) -> Result<Self, String> {
        let mut headers = stream_headers();
        if let Some(key) = config.api_key.as_deref() {
            let key = key.trim();
            if !key.is_empty() {
                headers.insert(AUTHORIZATION, sensitive_header(&format!("Bearer {key}"))?);
            }
        }
        apply_custom_headers(&mut headers, &config.headers)?;
        Ok(Self {
            service_name: config.name.clone(),
            endpoint_url: responses_url(&config.base_url),
            headers,
            max_tokens: config.max_tokens,
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
            cache_key,
        } = request;
        let mut input = Vec::with_capacity(history.len() + 2);
        if !system_prompt.trim().is_empty() {
            input.push(json!({ "role": "system", "content": system_prompt }));
        }
        input.extend(history.iter().map(|message| {
            json!({
                "role": message.role.as_str(),
                "content": message.content,
            })
        }));
        let user_content = if images.is_empty() {
            json!(text)
        } else {
            let mut parts: Vec<Value> = images
                .iter()
                .map(|img| {
                    json!({
                        "type": "input_image",
                        "image_url": format!("data:{};base64,{}", img.mime_type, img.data),
                    })
                })
                .collect();
            parts.push(json!({ "type": "input_text", "text": text }));
            Value::Array(parts)
        };
        input.push(json!({ "role": "user", "content": user_content }));

        let mut payload = json!({
            "model": model,
            "input": input,
            "stream": true,
            "store": false,
        });
        if let Some(max_tokens) = self.max_tokens {
            payload["max_output_tokens"] = json!(max_tokens);
        }
        if let Some(level) = thinking {
            payload["reasoning"] = json!({ "effort": level.as_str() });
        }
        if let Some(schema) = response_schema {
            payload["text"] = json!({
                "format": {
                    "type": "json_schema",
                    "name": "structured_output",
                    "schema": schema,
                }
            });
        }
        if let Some(key) = cache_key {
            payload["prompt_cache_key"] = json!(key);
        } else if self.prompt_cache {
            payload["prompt_cache_key"] = json!(format!("konyac:{}:{model}", self.service_name));
        }

        stream_request(client, &self.endpoint_url, &self.headers, &payload)
    }
}

fn responses_url(base_url: &str) -> String {
    let Ok(mut url) = Url::parse(base_url) else {
        return base_url.to_string();
    };
    let path = url.path().trim_end_matches('/');
    let endpoint = if path.is_empty()
        && url
            .host_str()
            .is_some_and(|h| h.eq_ignore_ascii_case("api.openai.com"))
    {
        "/v1/responses".to_owned()
    } else if path.ends_with("/responses") {
        path.to_owned()
    } else {
        format!("{path}/responses")
    };
    url.set_path(&endpoint);
    url.into()
}

pub fn parse_sse_event(event_type: &str, data: &str) -> SseAction {
    if data.trim().is_empty() {
        return SseAction::Ignore;
    }
    let val: Value = match serde_json::from_str(data) {
        Ok(val) => val,
        Err(error) => {
            return SseAction::error(
                ErrorCode::UpstreamError,
                format!("Invalid Responses API stream event: {error}"),
            );
        }
    };
    if let Some(error) = val.get("error").filter(|e| !e.is_null()) {
        return SseAction::Finish(Err(provider_error("Responses", error)));
    }

    match val
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or(event_type)
    {
        "response.output_text.delta" => match val.get("delta") {
            Some(Value::String(delta)) if !delta.is_empty() => SseAction::Text(delta.clone()),
            Some(Value::String(_)) | Some(Value::Null) | None => SseAction::Ignore,
            Some(_) => {
                SseAction::error(ErrorCode::UpstreamError, "Invalid Responses API text delta")
            }
        },
        "response.completed" => SseAction::Finish(Ok(())),
        "response.incomplete" => {
            let reason = val
                .pointer("/response/incomplete_details/reason")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            let code = if reason == "content_filter" {
                ErrorCode::ContentBlocked
            } else {
                ErrorCode::OutputTruncated
            };
            SseAction::error(code, format!("Responses API output incomplete: {reason}"))
        }
        "response.refusal.done" => SseAction::error(
            ErrorCode::ContentBlocked,
            val.get("refusal")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .unwrap_or("Upstream model refused the request"),
        ),
        "response.failed" => match val.pointer("/response/error").filter(|e| !e.is_null()) {
            Some(error) => SseAction::Finish(Err(provider_error("Responses", error))),
            None => SseAction::error(ErrorCode::UpstreamError, "Responses API request failed"),
        },
        "error" => SseAction::Finish(Err(provider_error("Responses", &val))),
        _ => SseAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ImageInput, ThinkingLevel};
    use crate::config::Protocol;
    use crate::llm::{ChatMessage, HistoryRole, request_json};

    #[test]
    fn builds_responses_requests_with_text_history() {
        let mut cfg =
            ServiceConfig::test("openai-test", Protocol::Openai, "https://api.openai.com/v1");
        cfg.api_key = Some("sk-test-key".into());
        let driver = OpenaiDriver::new(&cfg).unwrap();
        let history = [
            ChatMessage {
                role: HistoryRole::User,
                content: "previous input",
            },
            ChatMessage {
                role: HistoryRole::Assistant,
                content: "previous translation",
            },
        ];
        let client = reqwest::Client::new();
        let request = driver
            .build_request(
                &client,
                DriverRequest {
                    model: "mock-model",
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
            "https://api.openai.com/v1/responses"
        );
        assert_eq!(request.headers()[AUTHORIZATION], "Bearer sk-test-key");
        assert!(request.headers()[AUTHORIZATION].is_sensitive());

        let mut bad_cfg = cfg.clone();
        bad_cfg.api_key = Some("bad\nkey".into());
        assert!(OpenaiDriver::new(&bad_cfg).is_err());

        assert_eq!(
            request_json(&request),
            json!({
                "model": "mock-model",
                "input": [
                    {"role":"system","content":"sys"},
                    {"role":"user","content":"previous input"},
                    {"role":"assistant","content":"previous translation"},
                    {"role":"user","content":"hi"}
                ],
                "stream": true,
                "store": false
            })
        );

        cfg.max_tokens = Some(2048);
        cfg.prompt_cache = true;
        cfg.headers.insert("X-Proxy".into(), "proxy-secret".into());
        let enhanced_driver = OpenaiDriver::new(&cfg).unwrap();
        let images = [ImageInput {
            mime_type: "image/png".into(),
            data: "iVBORw0KGgo=".into(),
        }];
        let schema = json!({"type": "object", "properties": {"translation": {"type": "string"}}});
        let req2 = enhanced_driver
            .build_request(
                &client,
                DriverRequest {
                    model: "gpt-5",
                    system_prompt: "sys",
                    text: "translate image",
                    images: &images,
                    thinking: Some(ThinkingLevel::Low),
                    response_schema: Some(&schema),
                    ..Default::default()
                },
            )
            .build()
            .unwrap();
        assert_eq!(req2.headers()["x-proxy"], "proxy-secret");
        assert!(req2.headers()["x-proxy"].is_sensitive());
        let payload2 = request_json(&req2);
        assert_eq!(payload2["max_output_tokens"], 2048);
        assert_eq!(payload2["reasoning"], json!({"effort": "low"}));
        assert_eq!(payload2["prompt_cache_key"], "konyac:openai-test:gpt-5");
        assert_eq!(
            payload2.pointer("/text/format"),
            Some(&json!({"type": "json_schema", "name": "structured_output", "schema": schema}))
        );
        assert!(payload2.get("tools").is_none() && payload2.get("tool_choice").is_none());
        assert_eq!(
            payload2["input"][1]["content"],
            json!([
                {"type": "input_image", "image_url": "data:image/png;base64,iVBORw0KGgo="},
                {"type": "input_text", "text": "translate image"}
            ])
        );
    }

    #[test]
    fn resolves_responses_endpoints() {
        for (base, expected) in [
            (
                "https://api.openai.com",
                "https://api.openai.com/v1/responses",
            ),
            (
                "http://localhost:11434/v1/",
                "http://localhost:11434/v1/responses",
            ),
            (
                "https://example.test/proxy/v1/responses?tenant=1",
                "https://example.test/proxy/v1/responses?tenant=1",
            ),
            (
                "https://api.deepseek.com",
                "https://api.deepseek.com/responses",
            ),
        ] {
            assert_eq!(responses_url(base), expected);
        }
    }

    #[test]
    fn parses_semantic_events_without_emitting_reasoning_or_duplicate_text() {
        assert!(matches!(
            parse_sse_event("message", r#"{"type":"response.output_text.delta","delta":"Hello"}"#),
            SseAction::Text(text) if text == "Hello"
        ));
        for (name, data) in [
            (
                "response.reasoning_summary_text.delta",
                r#"{"delta":"reasoning"}"#,
            ),
            (
                "response.function_call_arguments.delta",
                r#"{"type":"response.custom_tool.delta","delta":{"nested":true}}"#,
            ),
            ("response.output_text.done", r#"{"text":"Hello"}"#),
            ("response.refusal.delta", r#"{"delta":"refusal"}"#),
        ] {
            assert!(matches!(parse_sse_event(name, data), SseAction::Ignore));
        }
        assert!(matches!(
            parse_sse_event(
                "response.completed",
                r#"{"response":{"status":"completed"}}"#
            ),
            SseAction::Finish(Ok(()))
        ));
        for (name, data, expected) in [
            (
                "response.incomplete",
                r#"{"response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#,
                ErrorCode::OutputTruncated,
            ),
            (
                "response.incomplete",
                r#"{"response":{"incomplete_details":{"reason":"content_filter"}}}"#,
                ErrorCode::ContentBlocked,
            ),
            (
                "response.refusal.done",
                r#"{"refusal":"Cannot comply"}"#,
                ErrorCode::ContentBlocked,
            ),
            (
                "response.failed",
                r#"{"response":{"error":{"code":"rate_limit_exceeded","message":"quota"}}}"#,
                ErrorCode::RateLimited,
            ),
            (
                "error",
                r#"{"type":"error","code":"request_timeout","message":"deadline"}"#,
                ErrorCode::UpstreamTimeout,
            ),
        ] {
            assert!(
                matches!(parse_sse_event(name, data), SseAction::Finish(Err(error)) if error.code == expected)
            );
        }
    }
}
