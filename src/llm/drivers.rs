pub mod anthropic;
pub mod gemini;
pub mod openai;

use crate::llm::types::HistoryMessage;
use anthropic::AnthropicDriver;
use eventsource_stream::Event;
use gemini::GeminiDriver;
use openai::OpenaiDriver;

use std::borrow::Cow;

/// Action result from parsing an SSE stream event.
pub enum SseAction<'a> {
    /// Text delta chunk.
    Text(Cow<'a, str>),
    /// Text delta chunk with end-of-stream indicator.
    TextAndDone(Cow<'a, str>),
    /// Stream completed normally.
    Done,
    /// Upstream protocol error.
    Error(String),
    /// Ignored event.
    Ignore,
}

/// Unified LLM protocol driver enum.
#[derive(Clone)]
pub enum Driver {
    Openai(OpenaiDriver),
    Anthropic(AnthropicDriver),
    Gemini(GeminiDriver),
}

impl Driver {
    #[inline]
    pub fn driver_name(&self) -> &'static str {
        match self {
            Self::Openai(_) => "openai",
            Self::Anthropic(_) => "anthropic",
            Self::Gemini(_) => "gemini",
        }
    }

    #[inline]
    pub fn build_request(
        &self,
        client: &reqwest::Client,
        model: &str,
        system_prompt: &str,
        history: Option<&[HistoryMessage]>,
        text: &str,
        json_schema: Option<&serde_json::Value>,
    ) -> reqwest::RequestBuilder {
        match self {
            Self::Openai(d) => {
                d.build_request(client, model, system_prompt, history, text, json_schema)
            }
            Self::Anthropic(d) => {
                d.build_request(client, model, system_prompt, history, text, json_schema)
            }
            Self::Gemini(d) => {
                d.build_request(client, model, system_prompt, history, text, json_schema)
            }
        }
    }

    #[inline]
    pub fn parse_sse<'a>(&self, event: &'a Event) -> SseAction<'a> {
        match self {
            Self::Openai(_) => openai::parse_sse_event(&event.data),
            Self::Anthropic(_) => anthropic::parse_sse_event(&event.event, &event.data),
            Self::Gemini(_) => gemini::parse_sse_event(&event.data),
        }
    }
}
