mod defaults;
mod prompt;
pub(crate) mod server;
mod service;
pub mod template;

pub use defaults::{DefaultConfig, RuntimeDefaultConfig};
pub use prompt::{DEFAULT_SYSTEM_PROMPT, PromptConfig, RuntimePromptConfig};
pub use server::{RuntimeServerConfig, ServerConfig};
pub use service::{Protocol, RuntimeServiceConfig, ServiceConfig};

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Unified runtime configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub server: RuntimeServerConfig,
    pub default: RuntimeDefaultConfig,
    pub service: Vec<RuntimeServiceConfig>,
    pub prompt: RuntimePromptConfig,
}

/// Unified configuration parsed from file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: ServerConfig,
    pub default: DefaultConfig,
    pub service: Vec<ServiceConfig>,
    pub prompt: PromptConfig,
}

impl Config {
    /// Returns the standard user configuration file path.
    pub fn default_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
        dirs::config_dir()
            .map(|p| p.join("konyac/config.toml"))
            .ok_or_else(|| "Failed to determine standard user config directory".into())
    }

    /// Validates configuration correctness.
    pub fn validate(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.server
            .validate()
            .map_err(|e| format!("Server config error: {e}"))?;
        self.prompt
            .validate()
            .map_err(|e| format!("Prompt config error: {e}"))?;

        let mut seen_names = HashSet::new();
        for service in &self.service {
            service
                .validate()
                .map_err(|e| format!("Service config error: {e}"))?;
            if !seen_names.insert(&service.name) {
                return Err(format!("Duplicate service name found: '{}'", service.name).into());
            }
        }

        self.default
            .validate(&self.service)
            .map_err(|e| format!("Default config error: {e}"))?;

        Ok(())
    }

    /// Returns the default configuration template string.
    pub fn default_template() -> String {
        template::default_template()
    }

    /// Loads an existing configuration file or creates one from the default template.
    pub fn load_or_create(
        path: Option<&Path>,
    ) -> Result<(Self, PathBuf), Box<dyn std::error::Error>> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => Self::default_path()?,
        };

        if path.exists() {
            let config = Self::load_from_file(&path)?;
            Ok((config, path))
        } else {
            let template = Self::default_template();
            Self::write_default_template(&path, &template)?;
            let config: Self = toml::from_str(&template)
                .map_err(|e| format!("Failed to parse default template: {e}"))?;
            config.validate().map_err(|e| {
                format!(
                    "Default configuration validation failed in '{}': {e}",
                    path.display()
                )
            })?;
            Ok((config, path))
        }
    }

    /// Loads and validates configuration from a specified file.
    pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self, Box<dyn std::error::Error>> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)
            .map_err(|e| format!("Failed to read config from '{}': {e}", path.display()))?;
        let config: Self = toml::from_str(&content)
            .map_err(|e| format!("Failed to parse TOML in '{}': {e}", path.display()))?;
        config.validate().map_err(|e| {
            format!(
                "Configuration validation failed in '{}': {e}",
                path.display()
            )
        })?;
        Ok(config)
    }

    /// Writes the default template with 0600 file permissions on Unix.
    fn write_default_template(
        path: &Path,
        content: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory '{}': {e}", parent.display()))?;
        }

        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path)
                .map_err(|e| format!("Failed to create config file '{}': {e}", path.display()))?;
            file.write_all(content.as_bytes())?;
        }

        #[cfg(not(unix))]
        {
            std::fs::write(path, content)
                .map_err(|e| format!("Failed to create config file '{}': {e}", path.display()))?;
        }

        Ok(())
    }

    /// Validates and resolves into `RuntimeConfig`.
    pub fn into_runtime(self) -> Result<RuntimeConfig, Box<dyn std::error::Error>> {
        self.validate()?;
        let server = self
            .server
            .into_runtime()
            .map_err(|e| format!("Server config error: {e}"))?;
        let default = self
            .default
            .into_runtime()
            .map_err(|e| format!("Default config error: {e}"))?;
        let prompt = self
            .prompt
            .into_runtime()
            .map_err(|e| format!("Prompt config error: {e}"))?;
        let service = self
            .service
            .into_iter()
            .map(|s| {
                s.into_runtime()
                    .map_err(|e| format!("Service config error: {e}").into())
            })
            .collect::<Result<Vec<_>, Box<dyn std::error::Error>>>()?;

        Ok(RuntimeConfig {
            server,
            default,
            service,
            prompt,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_lifecycle() {
        let config = Config {
            service: vec![ServiceConfig {
                name: "deepseek".into(),
                protocol: Protocol::Openai,
                base_url: "https://api.deepseek.com/v1/".into(),
                api_key: Some("sk-key".into()),
                models: vec!["deepseek-chat".into()],
            }],
            ..Default::default()
        };

        let runtime = config.clone().into_runtime().unwrap();
        assert_eq!(runtime.server.tcp.as_ref().unwrap().port, 16742);
        assert_eq!(runtime.service.len(), 1);

        // Duplicate service name validation error
        let dup = Config {
            service: vec![config.service[0].clone(), config.service[0].clone()],
            ..Default::default()
        };
        assert!(dup.validate().is_err());
    }

    #[test]
    fn test_config_file_lifecycle_and_permissions() {
        let temp_dir = std::env::temp_dir().join("konyac_test_lifecycle");
        let custom_file = temp_dir.join("sub").join("config.toml");

        let (config, path) = Config::load_or_create(Some(&custom_file)).unwrap();
        assert_eq!(path, custom_file);
        assert!(custom_file.exists());
        assert_eq!(config.server.tcp.as_ref().unwrap().port, 16742);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&custom_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "Must have 0600 permissions");
        }

        let toml_content = r#"
            [server.tcp]
            port = 18888
            host = "127.0.0.1"

            [[service]]
            name = "deepseek"
            protocol = "openai"
            base_url = "https://api.deepseek.com/v1"
            api_key = "sk-test"
            models = ["deepseek-chat"]
        "#;
        std::fs::write(&custom_file, toml_content).unwrap();
        let reloaded = Config::load_from_file(&custom_file).unwrap();
        assert_eq!(reloaded.server.tcp.unwrap().port, 18888);
        assert_eq!(reloaded.service.len(), 1);

        let non_existent = PathBuf::from("/non/existent/path/xyz.toml");
        assert!(
            Config::load_from_file(&non_existent)
                .unwrap_err()
                .to_string()
                .contains("xyz.toml")
        );

        let _ = std::fs::remove_dir_all(temp_dir);
    }
}
