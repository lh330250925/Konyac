use super::SseAction;
use crate::llm::types::HistoryMessage;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Serialize)]
struct Message<'a> {
    role: &'a str,
    content: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum OpenaiResponseFormat<'a> {
    #[serde(rename = "json_schema")]
    JsonSchema {
        json_schema: OpenaiJsonSchemaPayload<'a>,
    },
}

#[derive(Serialize)]
struct OpenaiJsonSchemaPayload<'a> {
    name: &'a str,
    schema: &'a serde_json::Value,
}

#[derive(Serialize)]
struct RequestPayload<'a> {
    model: &'a str,
    messages: Vec<Message<'a>>,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_format: Option<OpenaiResponseFormat<'a>>,
}

#[derive(Deserialize)]
struct OpenAiChunk<'a> {
    #[serde(borrow)]
    choices: Option<Vec<OpenAiChoice<'a>>>,
    #[serde(borrow)]
    error: Option<OpenAiError<'a>>,
}

#[derive(Deserialize)]
struct OpenAiChoice<'a> {
    #[serde(borrow)]
    delta: OpenAiDelta<'a>,
}

#[derive(Deserialize)]
struct OpenAiDelta<'a> {
    #[serde(borrow)]
    content: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct OpenAiError<'a> {
    message: Option<&'a str>,
}

/// OpenAI Chat Completions protocol driver.
#[derive(Clone)]
pub struct OpenaiDriver {
    pub endpoint_url: String,
    pub headers: HeaderMap,
}

impl OpenaiDriver {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        let base = base_url.trim_end_matches('/');
        let endpoint_url = if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        };

        let mut headers = HeaderMap::with_capacity(3);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));

        if let Some(key) = api_key {
            let trimmed = key.trim();
            if !trimmed.is_empty()
                && let Ok(val) = HeaderValue::from_str(&format!("Bearer {trimmed}"))
            {
                headers.insert(AUTHORIZATION, val);
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
        let capacity = 1 + history.map_or(0, |h| h.len()) + 1;
        let mut messages = Vec::with_capacity(capacity);
        if !system_prompt.trim().is_empty() {
            messages.push(Message {
                role: "system",
                content: system_prompt,
            });
        }

        if let Some(hist) = history {
            for msg in hist {
                messages.push(Message {
                    role: &msg.role,
                    content: &msg.content,
                });
            }
        }

        messages.push(Message {
            role: "user",
            content: text,
        });

        let response_format = json_schema.map(|schema| OpenaiResponseFormat::JsonSchema {
            json_schema: OpenaiJsonSchemaPayload {
                name: "custom_schema",
                schema,
            },
        });

        let payload = RequestPayload {
            model,
            messages,
            stream: true,
            response_format,
        };

        client
            .post(&self.endpoint_url)
            .headers(self.headers.clone())
            .json(&payload)
    }
}

pub fn parse_sse_event<'a>(event_data: &'a str) -> SseAction<'a> {
    let trimmed = event_data.trim();
    if trimmed.is_empty() {
        return SseAction::Ignore;
    }
    if trimmed == "[DONE]" {
        return SseAction::Done;
    }

    let Ok(chunk) = serde_json::from_str::<OpenAiChunk<'a>>(trimmed) else {
        return SseAction::Ignore;
    };

    if let Some(err) = chunk.error {
        let msg = err.message.unwrap_or("Unknown OpenAI error");
        return SseAction::Error(format!("OpenAI API error: {msg}"));
    }

    if let Some(choices) = chunk.choices
        && let Some(first) = choices.into_iter().next()
        && let Some(content) = first.delta.content
        && !content.is_empty()
    {
        return SseAction::Text(content);
    }

    SseAction::Ignore
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openai_driver() {
        let driver = OpenaiDriver::new("http://localhost:11434/v1", Some("sk-test-key"));
        assert_eq!(
            driver.endpoint_url,
            "http://localhost:11434/v1/chat/completions"
        );

        let client = reqwest::Client::new();
        let req = driver
            .build_request(&client, "qwen2.5:7b", "sys", None, "hi", None)
            .build()
            .unwrap();
        assert_eq!(
            req.headers().get(AUTHORIZATION).unwrap(),
            "Bearer sk-test-key"
        );

        // SSE parsing
        assert!(matches!(
            parse_sse_event(r#"{"choices":[{"delta":{"content":"Hello"}}]}"#),
            SseAction::Text(t) if t == "Hello"
        ));
        assert!(matches!(parse_sse_event("[DONE]"), SseAction::Done));
        assert!(matches!(
            parse_sse_event(r#"{"error":{"message":"err"}}"#),
            SseAction::Error(_)
        ));
    }
}
