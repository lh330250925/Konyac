use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// TCP listener configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Tcp {
    pub port: u16,
    pub host: String,
}

impl Default for Tcp {
    fn default() -> Self {
        Self {
            port: 16742,
            host: "127.0.0.1".to_string(),
        }
    }
}

/// Server listener configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tcp: Option<Tcp>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unix: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tcp: Some(Tcp::default()),
            unix: Some(
                dirs::runtime_dir()
                    .map(|p| p.join("konyac.sock"))
                    .unwrap_or_else(|| std::env::temp_dir().join("konyac.sock")),
            ),
        }
    }
}

impl ServerConfig {
    /// Validate server configuration
    pub fn validate(&self) -> Result<(), String> {
        if self.tcp.is_none() && self.unix.is_none() {
            return Err(
                "At least one server endpoint (TCP or Unix socket) must be enabled".to_string(),
            );
        }

        if let Some(ref tcp) = self.tcp {
            if tcp.port == 0 {
                return Err("Server port cannot be 0".to_string());
            }
            if tcp.host.trim().is_empty() {
                return Err("Server host cannot be empty".to_string());
            }
        }

        if let Some(ref unix) = self.unix {
            if unix.as_os_str().is_empty() {
                return Err("Unix socket path cannot be empty".to_string());
            }
            if unix.as_os_str().as_encoded_bytes().len() >= 104 {
                return Err(format!(
                    "Unix socket path is too long (max 103 bytes): {}",
                    unix.display()
                ));
            }
        }

        Ok(())
    }

    /// Validate and convert into runtime server configuration
    pub fn into_runtime(mut self) -> Result<Self, String> {
        self.validate()?;
        if let Some(ref mut tcp) = self.tcp {
            tcp.host = tcp.host.trim().to_string();
        }
        Ok(self)
    }

    /// Apply CLI overrides to server configuration
    pub fn apply_overrides(
        mut self,
        host: Option<String>,
        port: Option<u16>,
        unix: Option<PathBuf>,
    ) -> Result<Self, String> {
        if host.is_some() || port.is_some() {
            let tcp = self.tcp.get_or_insert_with(Tcp::default);
            if let Some(h) = host {
                tcp.host = h;
            }
            if let Some(p) = port {
                tcp.port = p;
            }
        }

        if let Some(u) = unix {
            self.unix = Some(u);
        }

        self.into_runtime()
    }
}

/// Type alias for runtime server configuration
pub type RuntimeServerConfig = ServerConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_and_overrides() {
        let server = ServerConfig::default();
        assert_eq!(server.tcp.as_ref().unwrap().port, 16742);
        assert!(server.validate().is_ok());

        let overridden = server
            .apply_overrides(
                Some("0.0.0.0".into()),
                Some(18000),
                Some(PathBuf::from("/tmp/k.sock")),
            )
            .unwrap();
        let tcp = overridden.tcp.unwrap();
        assert_eq!(tcp.host, "0.0.0.0");
        assert_eq!(tcp.port, 18000);
        assert_eq!(overridden.unix, Some(PathBuf::from("/tmp/k.sock")));

        // Validations
        assert!(
            ServerConfig {
                tcp: None,
                unix: None
            }
            .validate()
            .is_err()
        );
        assert!(
            ServerConfig {
                tcp: Some(Tcp {
                    port: 0,
                    host: "127.0.0.1".into()
                }),
                unix: None
            }
            .validate()
            .is_err()
        );
        assert!(
            ServerConfig {
                tcp: None,
                unix: Some(PathBuf::from(""))
            }
            .validate()
            .is_err()
        );
    }
}
