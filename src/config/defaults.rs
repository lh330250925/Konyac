use super::ServiceConfig;
use serde::{Deserialize, Serialize};

/// Default service and model configuration
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DefaultConfig {
    /// Default LLM service name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,

    /// Default model name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

impl DefaultConfig {
    /// Validate default service and model against the configured services list
    pub fn validate(&self, services: &[ServiceConfig]) -> Result<(), String> {
        if let Some(ref service_name) = self.service {
            let trimmed_service = service_name.trim();
            if trimmed_service.is_empty() {
                return Err("Field 'service' in [default] cannot be empty".to_string());
            }

            if !services.is_empty() {
                let target_svc = services.iter().find(|s| s.name == trimmed_service);
                let Some(svc) = target_svc else {
                    let mut available: Vec<_> = services.iter().map(|s| s.name.as_str()).collect();
                    available.sort();
                    return Err(format!(
                        "Default service '{trimmed_service}' not found in configured [[service]] list. Available: {available:?}"
                    ));
                };

                if let Some(ref model_name) = self.model {
                    let trimmed_model = model_name.trim();
                    if trimmed_model.is_empty() {
                        return Err("Field 'model' in [default] cannot be empty".to_string());
                    }
                    if !svc.models.iter().any(|m| m.trim() == trimmed_model) {
                        return Err(format!(
                            "Default model '{trimmed_model}' is not in configured models for service '{trimmed_service}'. Configured: {:?}",
                            svc.models
                        ));
                    }
                }
            }
        } else if let Some(ref model_name) = self.model {
            let trimmed_model = model_name.trim();
            if trimmed_model.is_empty() {
                return Err("Field 'model' in [default] cannot be empty".to_string());
            }

            if let Some(first_svc) = services.first()
                && !first_svc.models.iter().any(|m| m.trim() == trimmed_model)
            {
                return Err(format!(
                    "Default model '{trimmed_model}' is not in configured models for fallback service '{}'. Configured: {:?}",
                    first_svc.name, first_svc.models
                ));
            }
        }

        Ok(())
    }

    /// Convert into validated runtime configuration
    pub fn into_runtime(self) -> Result<Self, String> {
        Ok(Self {
            service: self
                .service
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            model: self
                .model
                .map(|m| m.trim().to_string())
                .filter(|m| !m.is_empty()),
        })
    }
}

pub type RuntimeDefaultConfig = DefaultConfig;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Protocol;

    #[test]
    fn test_default_config_validation() {
        let services = vec![ServiceConfig {
            name: "deepseek".into(),
            protocol: Protocol::Openai,
            base_url: "https://api.deepseek.com/v1".into(),
            api_key: Some("sk-test".into()),
            models: vec!["deepseek-chat".into()],
        }];

        let valid = DefaultConfig {
            service: Some("deepseek".into()),
            model: Some("deepseek-chat".into()),
        };
        assert!(valid.validate(&services).is_ok());
        assert_eq!(
            valid.into_runtime().unwrap().service.as_deref(),
            Some("deepseek")
        );

        assert!(
            DefaultConfig {
                service: Some("unknown".into()),
                model: None
            }
            .validate(&services)
            .is_err()
        );
        assert!(
            DefaultConfig {
                service: Some("deepseek".into()),
                model: Some("wrong".into())
            }
            .validate(&services)
            .is_err()
        );
    }
}
