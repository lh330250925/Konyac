use serde::{Deserialize, Serialize};
use std::fmt;

/// LLM communication protocol
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    /// OpenAI Chat Completions compatible protocol
    Openai,
    /// Anthropic Messages protocol
    Anthropic,
    /// Google Gemini protocol
    Gemini,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Protocol::Openai => write!(f, "openai"),
            Protocol::Anthropic => write!(f, "anthropic"),
            Protocol::Gemini => write!(f, "gemini"),
        }
    }
}

/// Translation service configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceConfig {
    /// Unique service identifier
    pub name: String,

    /// Communication protocol
    pub protocol: Protocol,

    /// Service base URL endpoint
    pub base_url: String,

    /// API access key
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// List of configured models for this service
    pub models: Vec<String>,
}

impl ServiceConfig {
    /// Get effective base URL without trailing slashes or whitespace
    pub fn effective_base_url(&self) -> &str {
        self.base_url.trim().trim_end_matches('/')
    }

    /// Validate service configuration syntax
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() {
            return Err("Service 'name' cannot be empty".to_string());
        }

        let base_url = self.effective_base_url();
        if base_url.is_empty() {
            return Err(format!("Service '{}' base_url cannot be empty", self.name));
        }
        if !base_url.starts_with("http://") && !base_url.starts_with("https://") {
            return Err(format!(
                "Service '{}' base_url must start with 'http://' or 'https://', got: '{}'",
                self.name, self.base_url
            ));
        }

        let is_local =
            base_url.starts_with("http://localhost") || base_url.starts_with("http://127.0.0.1");

        match self.api_key.as_deref().map(str::trim) {
            Some(key) if !key.is_empty() => {
                if let Some(var) = key.strip_prefix("${").and_then(|s| s.strip_suffix('}'))
                    && var.trim().is_empty()
                {
                    return Err(format!(
                        "Service '{}' has empty environment variable name in api_key",
                        self.name
                    ));
                }
            }
            _ => {
                if !is_local {
                    return Err(format!(
                        "Service '{}' requires an 'api_key' in configuration",
                        self.name
                    ));
                }
            }
        }

        if self.models.is_empty() || self.models.iter().any(|m| m.trim().is_empty()) {
            return Err(format!(
                "Service '{}' must specify at least one valid non-empty model name",
                self.name
            ));
        }

        Ok(())
    }

    /// Validate and resolve environment variables into runtime service configuration
    pub fn into_runtime(self) -> Result<RuntimeServiceConfig, String> {
        self.validate()?;

        let base_url = self.effective_base_url();
        let is_local =
            base_url.starts_with("http://localhost") || base_url.starts_with("http://127.0.0.1");

        let api_key = match self.api_key.as_deref().map(str::trim) {
            Some(key) if !key.is_empty() => {
                if let Some(var) = key.strip_prefix("${").and_then(|s| s.strip_suffix('}')) {
                    let var = var.trim();
                    let val = std::env::var(var)
                        .ok()
                        .map(|v| v.trim().to_string())
                        .filter(|s| !s.is_empty());
                    if val.is_none() && !is_local {
                        return Err(format!(
                            "Service '{}' requires environment variable '{var}' to be set",
                            self.name
                        ));
                    }
                    val
                } else {
                    Some(key.to_string())
                }
            }
            _ => None,
        };

        if !is_local && api_key.is_none() {
            return Err(format!(
                "Service '{}' requires an 'api_key' in configuration",
                self.name
            ));
        }

        let name = self.name.trim().to_string();
        let base_url = self.effective_base_url().to_string();
        let models = self
            .models
            .into_iter()
            .map(|m| m.trim().to_string())
            .collect();
        Ok(RuntimeServiceConfig {
            name,
            protocol: self.protocol,
            base_url,
            api_key,
            models,
        })
    }
}

/// Runtime translation service configuration
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeServiceConfig {
    /// Unique service identifier
    pub name: String,

    /// Communication protocol
    pub protocol: Protocol,

    /// Normalized service base URL
    pub base_url: String,

    /// Resolved API access key
    pub api_key: Option<String>,

    /// Configured models for this service
    pub models: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_config_and_validation() {
        // Cloud service with key
        let s_cloud: ServiceConfig = toml::from_str(
            r#"
            name = "deepseek"
            protocol = "openai"
            base_url = "https://api.deepseek.com/v1///"
            api_key = "sk-key"
            models = ["deepseek-chat"]
        "#,
        )
        .unwrap();
        assert_eq!(s_cloud.effective_base_url(), "https://api.deepseek.com/v1");
        assert!(s_cloud.validate().is_ok());

        // Local service without key
        let s_local: ServiceConfig = toml::from_str(
            r#"
            name = "ollama"
            protocol = "openai"
            base_url = "http://localhost:11434/v1"
            models = ["qwen2.5:7b"]
        "#,
        )
        .unwrap();
        assert!(s_local.validate().is_ok());

        // Env var substitution
        unsafe {
            std::env::set_var("KONYAC_TEST_KEY", "secret");
        }
        let s_env: ServiceConfig = toml::from_str(
            r#"
            name = "env-srv"
            protocol = "openai"
            base_url = "https://api.openai.com/v1"
            api_key = "${KONYAC_TEST_KEY}"
            models = ["gpt-4o"]
        "#,
        )
        .unwrap();
        assert_eq!(
            s_env.clone().into_runtime().unwrap().api_key.as_deref(),
            Some("secret")
        );
        unsafe {
            std::env::remove_var("KONYAC_TEST_KEY");
        }

        // Validations
        assert!(
            ServiceConfig {
                name: "".into(),
                ..s_cloud.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            ServiceConfig {
                base_url: "bad-url".into(),
                ..s_cloud.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            ServiceConfig {
                models: vec![],
                ..s_cloud.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            ServiceConfig {
                api_key: None,
                ..s_cloud
            }
            .validate()
            .is_err()
        );
    }
}
