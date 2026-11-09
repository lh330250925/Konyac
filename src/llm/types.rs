use serde::{Deserialize, Serialize};

/// Conversation history message item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryMessage {
    pub role: String,
    pub content: String,
}

/// Metadata for an active LLM service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceInfo {
    /// Service name.
    pub name: String,
    /// Communication protocol.
    pub protocol: String,
    /// List of configured models.
    pub configured_models: Vec<String>,
}
