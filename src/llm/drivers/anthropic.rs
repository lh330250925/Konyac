use super::SseAction;
use crate::llm::types::HistoryMessage;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Serialize)]
struct AnthropicMessage<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
struct AnthropicPayload<'a> {
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    messages: Vec<AnthropicMessage<'a>>,
    max_tokens: u32,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<AnthropicTool<'a>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<AnthropicToolChoice<'a>>,
}

#[derive(Serialize)]
struct AnthropicTool<'a> {
    name: &'a str,
    description: &'a str,
    input_schema: serde_json::Value,
}

#[derive(Serialize)]
struct AnthropicToolChoice<'a> {
    r#type: &'a str,
    name: &'a str,
}

#[derive(Deserialize)]
struct AnthropicChunk<'a> {
    r#type: Option<&'a str>,
    #[serde(borrow)]
    delta: Option<AnthropicDelta<'a>>,
    #[serde(borrow)]
    error: Option<AnthropicError<'a>>,
}

#[derive(Deserialize)]
struct AnthropicDelta<'a> {
    #[serde(borrow)]
    text: Option<Cow<'a, str>>,
    #[serde(borrow)]
    partial_json: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct AnthropicError<'a> {
    message: Option<&'a str>,
}

/// Anthropic Messages protocol driver.
#[derive(Clone)]
pub struct AnthropicDriver {
    pub endpoint_url: String,
    pub headers: HeaderMap,
}

impl AnthropicDriver {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        let base = base_url.trim_end_matches('/');
        let endpoint_url = if base.ends_with("/messages") {
            base.to_string()
        } else {
            format!("{base}/messages")
        };

        let mut headers = HeaderMap::with_capacity(4);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(
            HeaderName::from_static("anthropic-version"),
            HeaderValue::from_static("2023-06-01"),
        );

        if let Some(key) = api_key {
            let trimmed = key.trim();
            if !trimmed.is_empty()
                && let Ok(val) = HeaderValue::from_str(trimmed)
            {
                headers.insert(HeaderName::from_static("x-api-key"), val);
            }
        }

        Self {
            endpoint_url,
            headers,
        }
    }

    pub fn build_request(
        &self,
        client: &reqwest::Client,
        model: &str,
        system_prompt: &str,
        history: Option<&[HistoryMessage]>,
        text: &str,
        json_schema: Option<&serde_json::Value>,
    ) -> reqwest::RequestBuilder {
        let system = if system_prompt.trim().is_empty() {
            None
        } else {
            Some(system_prompt)
        };

        let capacity = history.map_or(0, |h| h.len()) + 1;
        let mut messages = Vec::with_capacity(capacity);

        if let Some(hist) = history {
            for msg in hist {
                let role = if msg.role.trim().eq_ignore_ascii_case("assistant") {
                    "assistant"
                } else {
                    "user"
                };
                messages.push(AnthropicMessage {
                    role,
                    content: &msg.content,
                });
            }
        }

        messages.push(AnthropicMessage {
            role: "user",
            content: text,
        });

        let (tools, tool_choice) = match json_schema {
            Some(schema) => (
                Some(vec![AnthropicTool {
                    name: "structured_output",
                    description: "Outputs structured JSON data matching the provided schema",
                    input_schema: schema.clone(),
                }]),
                Some(AnthropicToolChoice {
                    r#type: "tool",
                    name: "structured_output",
                }),
            ),
            None => (None, None),
        };

        let payload = AnthropicPayload {
            model,
            system,
            messages,
            max_tokens: 4096,
            stream: true,
            tools,
            tool_choice,
        };

        client
            .post(&self.endpoint_url)
            .headers(self.headers.clone())
            .json(&payload)
    }
}

pub fn parse_sse_event<'a>(event_type: &str, event_data: &'a str) -> SseAction<'a> {
    let trimmed = event_data.trim();
    if trimmed.is_empty() {
        return SseAction::Ignore;
    }

    if event_type == "message_stop" {
        return SseAction::Done;
    }

    let Ok(chunk) = serde_json::from_str::<AnthropicChunk<'a>>(trimmed) else {
        return SseAction::Ignore;
    };

    let msg_type = chunk.r#type.unwrap_or(event_type);

    if msg_type == "message_stop" {
        return SseAction::Done;
    }

    if msg_type == "error" || event_type == "error" {
        let msg = chunk
            .error
            .and_then(|e| e.message)
            .unwrap_or("Unknown Anthropic error");
        return SseAction::Error(format!("Anthropic API error: {msg}"));
    }

    if (msg_type == "content_block_delta" || event_type == "content_block_delta")
        && let Some(delta) = chunk.delta
    {
        if let Some(text) = delta.text
            && !text.is_empty()
        {
            return SseAction::Text(text);
        }
        if let Some(json) = delta.partial_json
            && !json.is_empty()
        {
            return SseAction::Text(json);
        }
    }

    SseAction::Ignore
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_anthropic_driver() {
        let driver = AnthropicDriver::new("https://api.anthropic.com/v1", Some("sk-ant-test"));
        assert_eq!(
            driver.endpoint_url,
            "https://api.anthropic.com/v1/messages"
        );

        let client = reqwest::Client::new();
        let req = driver
            .build_request(&client, "claude-3-5-sonnet", "sys", None, "hi", None)
            .build()
            .unwrap();
        assert_eq!(req.headers().get("x-api-key").unwrap(), "sk-ant-test");

        // SSE parsing
        let delta = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}"#;
        assert!(matches!(
            parse_sse_event("content_block_delta", delta),
            SseAction::Text(t) if t == "Hi"
        ));
        assert!(matches!(
            parse_sse_event("message_stop", r#"{"type":"message_stop"}"#),
            SseAction::Done
        ));
        assert!(matches!(
            parse_sse_event("error", r#"{"error":{"message":"bad"}}"#),
            SseAction::Error(_)
        ));
    }
}
