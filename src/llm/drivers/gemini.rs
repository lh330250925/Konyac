use super::SseAction;
use crate::llm::types::HistoryMessage;
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

#[derive(Serialize)]
struct GeminiPart<'a> {
    text: &'a str,
}

#[derive(Serialize)]
struct GeminiContent<'a> {
    role: &'a str,
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
struct GeminiSystemInstruction<'a> {
    parts: Vec<GeminiPart<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiGenerationConfig<'a> {
    response_mime_type: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    response_schema: Option<&'a serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GeminiPayload<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction<'a>>,
    contents: Vec<GeminiContent<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    generation_config: Option<GeminiGenerationConfig<'a>>,
}

#[derive(Deserialize)]
struct GeminiChunk<'a> {
    #[serde(borrow)]
    candidates: Option<Vec<GeminiCandidate<'a>>>,
    #[serde(borrow)]
    error: Option<GeminiError<'a>>,
}

#[derive(Deserialize)]
struct GeminiCandidate<'a> {
    #[serde(borrow)]
    content: Option<GeminiCandidateContent<'a>>,
    #[serde(rename = "finishReason")]
    finish_reason: Option<&'a str>,
}

#[derive(Deserialize)]
struct GeminiCandidateContent<'a> {
    #[serde(borrow)]
    parts: Option<Vec<GeminiPartChunk<'a>>>,
}

#[derive(Deserialize)]
struct GeminiPartChunk<'a> {
    #[serde(borrow)]
    text: Option<Cow<'a, str>>,
}

#[derive(Deserialize)]
struct GeminiError<'a> {
    message: Option<&'a str>,
}

/// Google Gemini protocol driver.
#[derive(Clone)]
pub struct GeminiDriver {
    pub base_url: String,
    pub headers: HeaderMap,
}

impl GeminiDriver {
    pub fn new(base_url: &str, api_key: Option<&str>) -> Self {
        let mut headers = HeaderMap::with_capacity(3);
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));

        if let Some(key) = api_key {
            let trimmed = key.trim();
            if !trimmed.is_empty()
                && let Ok(val) = HeaderValue::from_str(trimmed)
            {
                headers.insert(HeaderName::from_static("x-goog-api-key"), val);
            }
        }

        Self {
            base_url: base_url.to_string(),
            headers,
        }
    }

    pub fn build_url(&self, model: &str) -> String {
        let base = self.base_url.trim_end_matches('/');
        let model_cleaned = model.trim_start_matches("models/");

        if base.contains(":streamGenerateContent") {
            if base.contains("alt=sse") {
                base.to_string()
            } else if base.contains('?') {
                format!("{base}&alt=sse")
            } else {
                format!("{base}?alt=sse")
            }
        } else {
            let prefix = if base.ends_with("/v1beta") || base.ends_with("/v1") {
                base.to_string()
            } else {
                format!("{base}/v1beta")
            };
            format!("{prefix}/models/{model_cleaned}:streamGenerateContent?alt=sse")
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
        let url = self.build_url(model);

        let system_instruction = if system_prompt.trim().is_empty() {
            None
        } else {
            Some(GeminiSystemInstruction {
                parts: vec![GeminiPart {
                    text: system_prompt,
                }],
            })
        };

        let capacity = history.map_or(0, |h| h.len()) + 1;
        let mut contents = Vec::with_capacity(capacity);

        if let Some(hist) = history {
            for msg in hist {
                let role = if msg.role.trim().eq_ignore_ascii_case("assistant")
                    || msg.role.trim().eq_ignore_ascii_case("model")
                {
                    "model"
                } else {
                    "user"
                };
                contents.push(GeminiContent {
                    role,
                    parts: vec![GeminiPart { text: &msg.content }],
                });
            }
        }

        contents.push(GeminiContent {
            role: "user",
            parts: vec![GeminiPart { text }],
        });

        let generation_config = json_schema.map(|schema| GeminiGenerationConfig {
            response_mime_type: "application/json",
            response_schema: Some(schema),
        });

        let payload = GeminiPayload {
            system_instruction,
            contents,
            generation_config,
        };

        client
            .post(url)
            .headers(self.headers.clone())
            .json(&payload)
    }
}

pub fn parse_sse_event<'a>(event_data: &'a str) -> SseAction<'a> {
    let trimmed = event_data.trim();
    if trimmed.is_empty() {
        return SseAction::Ignore;
    }

    let Ok(chunk) = serde_json::from_str::<GeminiChunk<'a>>(trimmed) else {
        return SseAction::Ignore;
    };

    if let Some(err) = chunk.error {
        let msg = err.message.unwrap_or("Unknown Gemini error");
        return SseAction::Error(format!("Gemini API error: {msg}"));
    }

    let mut text_opt = None;
    let mut is_finished = false;

    if let Some(candidates) = chunk.candidates
        && let Some(first) = candidates.into_iter().next()
    {
        if let Some(content) = first.content
            && let Some(parts) = content.parts
            && let Some(part) = parts.into_iter().next()
            && let Some(t) = part.text
            && !t.is_empty()
        {
            text_opt = Some(t);
        }

        if let Some(reason) = first.finish_reason
            && !reason.is_empty()
            && reason != "FINISH_REASON_UNSPECIFIED"
        {
            is_finished = true;
        }
    }

    match (text_opt, is_finished) {
        (Some(text), true) => SseAction::TextAndDone(text),
        (Some(text), false) => SseAction::Text(text),
        (None, true) => SseAction::Done,
        (None, false) => SseAction::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_gemini_driver() {
        let driver =
            GeminiDriver::new("https://generativelanguage.googleapis.com", Some("AIzaKey"));
        assert!(
            driver
                .build_url("gemini-2.0-flash")
                .contains("streamGenerateContent?alt=sse")
        );

        let client = reqwest::Client::new();
        let req = driver
            .build_request(&client, "gemini-2.0-flash", "sys", None, "hi", None)
            .build()
            .unwrap();
        assert_eq!(req.headers().get("x-goog-api-key").unwrap(), "AIzaKey");

        // SSE parsing
        let chunk = r#"{"candidates":[{"content":{"parts":[{"text":"Hello"}]}}]}"#;
        assert!(matches!(
            parse_sse_event(chunk),
            SseAction::Text(t) if t == "Hello"
        ));
        let stop =
            r#"{"candidates":[{"content":{"parts":[{"text":"."}]},"finishReason":"STOP"}]}"#;
        assert!(matches!(
            parse_sse_event(stop),
            SseAction::TextAndDone(t) if t == "."
        ));
        assert!(matches!(
            parse_sse_event(r#"{"error":{"message":"bad"}}"#),
            SseAction::Error(_)
        ));
    }
}
