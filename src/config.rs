use crate::api::ThinkingLevel;
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue, InvalidHeaderValue};
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

const DEFAULT_PORT: u16 = 16742;

pub(crate) fn bearer_authorization(token: &str) -> Result<HeaderValue, InvalidHeaderValue> {
    let mut header = HeaderValue::try_from(format!("Bearer {token}"))?;
    header.set_sensitive(true);
    Ok(header)
}

pub(crate) fn tcp_authorization() -> Result<Option<HeaderValue>, String> {
    let token = match std::env::var("KONYAC_TCP_TOKEN") {
        Ok(token) => token,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(_) => return Err("KONYAC_TCP_TOKEN must be valid UTF-8".into()),
    };
    if token.is_empty() || token.bytes().any(|b| b.is_ascii_whitespace()) {
        return Err("KONYAC_TCP_TOKEN must be a non-empty token without whitespace".into());
    }
    bearer_authorization(&token)
        .map(Some)
        .map_err(|e| format!("Invalid KONYAC_TCP_TOKEN: {e}"))
}

pub(crate) fn split_tcp_addr(addr: &str) -> Result<(&str, u16), String> {
    let addr = addr.trim();
    if let Some(rest) = addr.strip_prefix('[') {
        let (host, port_part) = rest
            .split_once("]:")
            .ok_or_else(|| format!("Invalid TCP address '{addr}': expected [ipv6]:port"))?;
        if host.is_empty() {
            return Err(format!("Invalid TCP address '{addr}': empty host"));
        }
        let port = port_part
            .parse::<u16>()
            .map_err(|_| format!("Invalid TCP port in '{addr}'"))?;
        return Ok((host, port));
    }

    let (host, port_part) = addr
        .rsplit_once(':')
        .ok_or_else(|| format!("Invalid TCP address '{addr}': expected host:port"))?;
    if host.is_empty() || host.contains(':') {
        return Err(format!(
            "Invalid TCP address '{addr}': use [ipv6]:port for IPv6 addresses"
        ));
    }
    let port = port_part
        .parse::<u16>()
        .map_err(|_| format!("Invalid TCP port in '{addr}'"))?;
    Ok((host, port))
}

pub(crate) fn parse_tcp_addr(addr: &str) -> Result<(&str, u16), String> {
    let (host, port) = split_tcp_addr(addr)?;
    if port == 0 {
        return Err("Server TCP port cannot be 0".into());
    }
    Ok((host, port))
}

pub fn default_config_path() -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(dir) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(dir).join("konyac/config.toml"));
    }
    let home = std::env::var_os("HOME")
        .filter(|v| !v.is_empty())
        .ok_or("Failed to determine config directory ($XDG_CONFIG_HOME or $HOME not set)")?;
    Ok(PathBuf::from(home).join(".config/konyac/config.toml"))
}

pub fn default_socket_path() -> PathBuf {
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(dir).join("konyac.sock");
    }
    std::env::temp_dir()
        .join(format!("konyac-{}", rustix::process::geteuid().as_raw()))
        .join("konyac.sock")
}

/// Server listener configuration (supports Unix-only, TCP-only, or both).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    pub tcp: Option<String>,
    pub unix: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            tcp: Some(format!("127.0.0.1:{DEFAULT_PORT}")),
            unix: Some(default_socket_path()),
        }
    }
}

impl ServerConfig {
    /// Loads listener addresses for client commands without `--endpoint`.
    pub fn load_for_client(path: Option<&Path>) -> Result<Self, Box<dyn std::error::Error>> {
        #[derive(Deserialize)]
        struct ClientConfig {
            #[serde(default)]
            server: ServerConfig,
        }

        let config_path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };
        let contents = match std::fs::read_to_string(&config_path) {
            Ok(contents) => contents,
            Err(e) if path.is_none() && e.kind() == ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(format!(
                    "Failed to read config from '{}': {e}",
                    config_path.display()
                )
                .into());
            }
        };
        let config: ClientConfig = toml::from_str(&contents)
            .map_err(|e| format!("Failed to parse TOML in '{}': {e}", config_path.display()))?;
        config
            .server
            .validate()
            .map_err(|e| format!("Invalid server config in '{}': {e}", config_path.display()))?;
        Ok(config.server)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.tcp.is_none() && self.unix.is_none() {
            return Err("At least one server endpoint (tcp or unix) must be enabled".into());
        }
        if let Some(ref tcp) = self.tcp {
            parse_tcp_addr(tcp)?;
        }
        if let Some(ref unix) = self.unix
            && unix.as_os_str().is_empty()
        {
            return Err("Unix socket path cannot be empty".into());
        }
        Ok(())
    }

    pub fn apply_overrides(
        mut self,
        tcp: Option<String>,
        unix: Option<PathBuf>,
    ) -> Result<Self, String> {
        if let Some(addr) = tcp {
            self.tcp = Some(addr);
        }
        if let Some(path) = unix {
            self.unix = Some(path);
        }
        self.validate()?;
        Ok(self)
    }
}

/// LLM communication protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Protocol {
    Openai,
    Anthropic,
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

/// Translation service configuration (first service and its first model are defaults).
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ServiceConfig {
    pub name: String,
    pub protocol: Protocol,
    pub base_url: String,
    pub api_key: Option<String>,
    pub models: Vec<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub thinking: Option<ThinkingLevel>,
    #[serde(default)]
    pub prompt_cache: bool,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

#[cfg(test)]
impl ServiceConfig {
    pub(crate) fn test(name: &str, protocol: Protocol, base_url: &str) -> Self {
        Self {
            name: name.into(),
            protocol,
            base_url: base_url.into(),
            api_key: None,
            models: vec!["m1".into()],
            max_tokens: None,
            thinking: None,
            prompt_cache: false,
            headers: BTreeMap::new(),
        }
    }
}

fn resolve_env_placeholder(service: &str, field: &str, raw: &str) -> Result<String, String> {
    if let Some(var) = raw
        .strip_prefix("${")
        .and_then(|s| s.strip_suffix('}'))
        .map(str::trim)
    {
        if var.is_empty() {
            return Err(format!(
                "Service '{service}' has empty environment variable name in {field}"
            ));
        }
        return std::env::var(var)
            .ok()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| {
                format!("Service '{service}' requires environment variable '{var}' to be set")
            });
    }
    Ok(raw.to_owned())
}

impl ServiceConfig {
    fn finalize(&mut self) -> Result<(), String> {
        self.name = self.name.trim().to_owned();
        if self.name.is_empty() {
            return Err("Service 'name' cannot be empty".into());
        }

        self.base_url = self.base_url.trim().trim_end_matches('/').to_owned();
        let parsed_url = Url::parse(&self.base_url).map_err(|e| {
            format!(
                "Service '{}' has invalid base_url '{}': {e}",
                self.name, self.base_url
            )
        })?;
        if !matches!(parsed_url.scheme(), "http" | "https") || parsed_url.host_str().is_none() {
            return Err(format!(
                "Service '{}' base_url must be a valid http:// or https:// URL, got: '{}'",
                self.name, self.base_url
            ));
        }

        self.api_key = match self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            Some(key) => Some(resolve_env_placeholder(&self.name, "api_key", key)?),
            None => None,
        };

        if self.models.is_empty() || self.models.iter().any(|m| m.trim().is_empty()) {
            return Err(format!(
                "Service '{}' must specify at least one valid non-empty model name",
                self.name
            ));
        }
        for model in &mut self.models {
            *model = model.trim().to_owned();
        }

        if self.max_tokens == Some(0) {
            return Err(format!(
                "Service '{}' max_tokens must be greater than 0",
                self.name
            ));
        }

        let mut seen_headers = HashSet::new();
        let mut normalized_headers = BTreeMap::new();
        for (raw_key, raw_value) in &self.headers {
            let key = raw_key.trim();
            if key.is_empty() {
                return Err(format!(
                    "Service '{}' header name cannot be empty",
                    self.name
                ));
            }
            let header_name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| {
                format!(
                    "Service '{}' has invalid header name '{key}': {e}",
                    self.name
                )
            })?;
            if !seen_headers.insert(header_name) {
                return Err(format!(
                    "Service '{}' defines duplicate header '{key}'",
                    self.name
                ));
            }
            let trimmed_val = raw_value.trim();
            if trimmed_val.is_empty() {
                return Err(format!(
                    "Service '{}' header '{key}' cannot be empty",
                    self.name
                ));
            }
            let resolved =
                resolve_env_placeholder(&self.name, &format!("headers['{key}']"), trimmed_val)?;
            HeaderValue::from_str(&resolved).map_err(|e| {
                format!(
                    "Service '{}' header '{key}' has invalid value: {e}",
                    self.name
                )
            })?;
            normalized_headers.insert(key.to_owned(), resolved);
        }
        self.headers = normalized_headers;

        Ok(())
    }
}

/// Prompt configuration.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PromptConfig {
    pub system: Option<String>,
    pub target_lang: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            system: None,
            target_lang: "zh-CN".to_string(),
        }
    }
}

impl PromptConfig {
    fn finalize(&mut self) -> Result<(), String> {
        if let Some(system) = self.system.as_deref().map(str::trim) {
            if system.is_empty() {
                return Err("Prompt template 'system' cannot be empty".into());
            }
            if !system.contains("{target_lang}") {
                return Err(
                    "Prompt template 'system' must contain '{target_lang}' placeholder".into(),
                );
            }
            self.system = Some(system.to_owned());
        }
        self.target_lang = self.target_lang.trim().to_owned();
        if self.target_lang.is_empty() {
            return Err("Prompt 'target_lang' cannot be empty".into());
        }
        Ok(())
    }
}

/// Unified daemon configuration.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    pub service: Vec<ServiceConfig>,
    pub prompt: PromptConfig,
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<(Self, PathBuf), Box<dyn std::error::Error>> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => default_config_path()?,
        };
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(format!(
                    "Config file not found at '{}'. Copy config.example.toml there and configure at least one [[service]].",
                    path.display()
                )
                .into());
            }
            Err(e) => {
                return Err(format!("Failed to read config from '{}': {e}", path.display()).into());
            }
        };
        let mut config: Self = toml::from_str(&contents)
            .map_err(|e| format!("Failed to parse TOML in '{}': {e}", path.display()))?;
        config.finalize().map_err(|e| {
            format!(
                "Configuration validation failed in '{}': {e}",
                path.display()
            )
        })?;
        Ok((config, path))
    }

    fn finalize(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        self.server
            .validate()
            .map_err(|e| format!("Server config error: {e}"))?;
        self.prompt
            .finalize()
            .map_err(|e| format!("Prompt config error: {e}"))?;

        let mut seen = HashSet::new();
        for service in &mut self.service {
            service
                .finalize()
                .map_err(|e| format!("Service config error: {e}"))?;
            if !seen.insert(service.name.clone()) {
                return Err(format!("Duplicate service name found: '{}'", service.name).into());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_listener_modes_and_tcp_addresses() {
        let unix_only: ServerConfig = toml::from_str(r#"unix = "/tmp/konyac.sock""#).unwrap();
        assert!(unix_only.tcp.is_none() && unix_only.unix.is_some());
        assert!(unix_only.validate().is_ok());

        let tcp_only: ServerConfig = toml::from_str(r#"tcp = "127.0.0.1:18080""#).unwrap();
        assert!(tcp_only.unix.is_none() && tcp_only.tcp.as_deref() == Some("127.0.0.1:18080"));
        assert!(tcp_only.validate().is_ok());

        let both = ServerConfig::default();
        assert_eq!(both.tcp.as_deref(), Some("127.0.0.1:16742"));
        assert!(both.unix.is_some());
        assert!(both.validate().is_ok());

        let overridden = both
            .apply_overrides(
                Some("[::1]:18000".into()),
                Some(PathBuf::from("/tmp/k.sock")),
            )
            .unwrap();
        assert_eq!(overridden.tcp.as_deref(), Some("[::1]:18000"));
        assert_eq!(overridden.unix, Some(PathBuf::from("/tmp/k.sock")));

        assert_eq!(
            parse_tcp_addr("127.0.0.1:16742").unwrap(),
            ("127.0.0.1", 16742)
        );
        assert_eq!(parse_tcp_addr("[::1]:16742").unwrap(), ("::1", 16742));
        for bad in [
            "127.0.0.1",
            "127.0.0.1:0",
            "::1:16742",
            ":16742",
            "[]:16742",
        ] {
            assert!(parse_tcp_addr(bad).is_err(), "expected error for {bad}");
        }
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
                tcp: None,
                unix: Some(PathBuf::from(""))
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn validates_services_prompts_and_env_keys() {
        let mut s_lan: ServiceConfig = toml::from_str(
            r#"
            name = "ollama-lan"
            protocol = "openai"
            base_url = "http://192.168.1.50:11434/v1/"
            models = [" qwen2.5:7b "]
        "#,
        )
        .unwrap();
        assert!(s_lan.finalize().is_ok());
        assert_eq!(s_lan.base_url, "http://192.168.1.50:11434/v1");
        assert_eq!(s_lan.models, vec!["qwen2.5:7b"]);
        assert_eq!(s_lan.api_key, None);
        assert_eq!(s_lan.thinking, None);
        assert!(!s_lan.prompt_cache);
        assert!(s_lan.headers.is_empty());

        unsafe {
            std::env::set_var("KONYAC_TEST_KEY", "secret");
            std::env::set_var("KONYAC_TEST_HDR", "hdr-secret");
        }
        let mut s_env: ServiceConfig = toml::from_str(
            r#"
            name = "env-srv"
            protocol = "openai"
            base_url = "https://api.openai.com/v1"
            api_key = "${KONYAC_TEST_KEY}"
            models = ["gpt-4o"]
            thinking = "low"
            prompt_cache = true
            headers = { " X-Custom " = "${KONYAC_TEST_HDR}", "HTTP-Referer" = "https://konyac.local" }
        "#,
        )
        .unwrap();
        s_env.finalize().unwrap();
        assert_eq!(s_env.api_key.as_deref(), Some("secret"));
        assert_eq!(s_env.thinking, Some(ThinkingLevel::Low));
        assert!(s_env.prompt_cache);
        assert_eq!(
            s_env.headers.get("X-Custom").map(String::as_str),
            Some("hdr-secret")
        );
        assert_eq!(
            s_env.headers.get("HTTP-Referer").map(String::as_str),
            Some("https://konyac.local")
        );
        unsafe {
            std::env::remove_var("KONYAC_TEST_KEY");
            std::env::remove_var("KONYAC_TEST_HDR");
        }
        for mut bad in [
            ServiceConfig {
                api_key: Some("${KONYAC_MISSING_VAR}".into()),
                ..s_lan.clone()
            },
            ServiceConfig {
                headers: BTreeMap::from([("Bad Header".into(), "ok".into())]),
                ..s_lan.clone()
            },
            ServiceConfig {
                headers: BTreeMap::from([("X-Test".into(), "bad\nval".into())]),
                ..s_lan.clone()
            },
            ServiceConfig {
                headers: BTreeMap::from([
                    ("X-Foo".into(), "1".into()),
                    ("x-foo".into(), "2".into()),
                ]),
                ..s_lan.clone()
            },
            ServiceConfig {
                base_url: "http://[".into(),
                ..s_lan.clone()
            },
            ServiceConfig {
                max_tokens: Some(0),
                ..s_lan
            },
        ] {
            assert!(bad.finalize().is_err());
        }

        let mut custom: PromptConfig =
            toml::from_str("system = '  Translate to {target_lang}  '\ntarget_lang = '  en  '")
                .unwrap();
        custom.finalize().unwrap();
        assert_eq!(custom.target_lang, "en");
        assert_eq!(custom.system.as_deref(), Some("Translate to {target_lang}"));
        for invalid in ["", "  ", "No placeholder"] {
            assert!(
                PromptConfig {
                    system: Some(invalid.into()),
                    target_lang: "en".into()
                }
                .finalize()
                .is_err()
            );
        }
    }

    #[test]
    fn loads_example_config_and_rejects_unknown_fields_and_missing_files() {
        struct TempDirGuard(PathBuf);
        impl Drop for TempDirGuard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        let example = include_str!("../config.example.toml");
        let parsed: Config = toml::from_str(example).unwrap();
        assert!(parsed.service.is_empty());
        assert!(toml::from_str::<Config>("[[services]]\nname = \"x\"").is_err());

        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let temp_dir =
            std::env::temp_dir().join(format!("konyac_cfg_test_{}_{unique:x}", std::process::id()));
        let _guard = TempDirGuard(temp_dir.clone());
        std::fs::create_dir_all(&temp_dir).unwrap();
        let cfg_path = temp_dir.join("config.toml");

        assert!(
            Config::load(Some(&cfg_path))
                .unwrap_err()
                .to_string()
                .contains("config.example.toml")
        );

        std::fs::write(
            &cfg_path,
            r#"
            [server]
            tcp = "127.0.0.1:18888"

            [[service]]
            name = "deepseek"
            protocol = "openai"
            base_url = "https://api.deepseek.com"
            api_key = "sk-test"
            models = ["deepseek-chat"]
            "#,
        )
        .unwrap();
        let (loaded, _) = Config::load(Some(&cfg_path)).unwrap();
        assert_eq!(loaded.server.tcp.as_deref(), Some("127.0.0.1:18888"));
        assert!(loaded.server.unix.is_none());
        assert_eq!(loaded.service.len(), 1);
        assert_eq!(
            ServerConfig::load_for_client(Some(&cfg_path))
                .unwrap()
                .tcp
                .as_deref(),
            Some("127.0.0.1:18888")
        );
    }
}
