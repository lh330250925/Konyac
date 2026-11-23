use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

pub(crate) const SSE_DONE: &str = "done";
pub(crate) const SSE_ERROR: &str = "error";

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    ServiceNotConfigured,
    ModelNotConfigured,
    Unauthorized,
    Forbidden,
    UpstreamTimeout,
    RateLimited,
    OutputTruncated,
    ContentBlocked,
    UpstreamInterrupted,
    UpstreamError,
    /// Keeps older responses and new codes readable for the client.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Shared error payload for HTTP responses and SSE error events.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KonyacError {
    #[serde(default)]
    pub code: ErrorCode,
    #[serde(rename = "error")]
    pub message: String,
}

impl KonyacError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for KonyacError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for KonyacError {}

/// Health check response body.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: Cow<'static, str>,
    pub version: Cow<'static, str>,
    pub target_lang: String,
    pub services: Vec<ServiceInfo>,
}

/// Conversation history item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessage {
    /// A user or assistant turn. The gateway also accepts `model` for assistant turns.
    pub role: String,
    pub content: String,
}

/// Metadata for an available translation service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    pub name: String,
    pub protocol: String,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,
}

/// Reasoning / thinking depth hint mapped natively across LLM protocols.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            other => Err(format!(
                "Invalid thinking level '{other}': expected minimal, low, medium, or high"
            )),
        }
    }
}

/// Base64-encoded image input for multimodal translation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageInput {
    pub mime_type: String,
    pub data: String,
}

/// Translate request payload.
#[cfg_attr(test, derive(Default))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TranslateRequest {
    /// Text to translate (optional when images are provided).
    #[serde(default)]
    pub text: String,

    /// Optional base64 images to translate alongside or instead of text.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageInput>,

    /// Overrides the daemon's default target language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_lang: Option<String>,

    /// Overrides the system prompt template.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,

    /// Supplemental instructions or terminology context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,

    /// Optional conversation history for context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<HistoryMessage>>,

    /// Overrides the daemon's default backend service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    /// Overrides the selected service's default model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,

    /// Overrides the service's default reasoning / thinking depth.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingLevel>,

    /// Optional JSON schema for structured output.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub json_schema: Option<serde_json::Value>,

    /// Optional prompt cache routing key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
}
