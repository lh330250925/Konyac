use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Health check response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: Cow<'static, str>,
    pub version: Cow<'static, str>,
    pub services_count: usize,
}

/// Standard error response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ErrorResponse {
    pub error: String,
}

pub use crate::llm::HistoryMessage;
pub use crate::prompt_engine::PromptOptions;

/// Translation request payload.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslateRequest {
    /// Text to translate.
    pub text: String,

    /// Target language code (e.g., "zh-CN", "en").
    pub target_lang: String,

    /// Prompt customization options.
    #[serde(default)]
    pub prompt: Option<PromptOptions>,

    /// Optional conversation history for context.
    #[serde(default)]
    pub history: Option<Vec<HistoryMessage>>,

    /// Backend service name.
    pub service: String,

    /// Target model name.
    pub model: String,

    /// Optional JSON schema for structured output.
    #[serde(default)]
    pub json_schema: Option<serde_json::Value>,
}

impl TranslateRequest {
    /// Validates required fields and returns trimmed references.
    pub fn validate(&self) -> Result<(&str, &str, &str, &str), &'static str> {
        let text = self.text.trim();
        if text.is_empty() {
            return Err("Field 'text' cannot be empty");
        }
        let target_lang = self.target_lang.trim();
        if target_lang.is_empty() {
            return Err("Field 'target_lang' cannot be empty");
        }
        let service = self.service.trim();
        if service.is_empty() {
            return Err("Field 'service' cannot be empty");
        }
        let model = self.model.trim();
        if model.is_empty() {
            return Err("Field 'model' cannot be empty");
        }
        Ok((text, target_lang, service, model))
    }
}
