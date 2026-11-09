use super::prompt::DEFAULT_SYSTEM_PROMPT;

/// Generate the default configuration file template.
pub fn default_template() -> String {
    let unix_path = dirs::runtime_dir()
        .map(|p| p.join("konyac.sock"))
        .unwrap_or_else(|| std::env::temp_dir().join("konyac.sock"));

    format!(
        r#"# ==============================================================================
# konyac (AI Translator Daemon) Configuration
# ==============================================================================

[server]
# Unix domain socket path (optional IPC endpoint)
unix = "{unix_path}"

[server.tcp]
# TCP HTTP / SSE listen address and port
port = 16742
host = "127.0.0.1"

# ==============================================================================
# Default Service & Model
#
# Fallback service and model when -s or -m is not explicitly provided.
# If omitted, defaults to the first configured service and its first model.
# ==============================================================================
# [default]
# service = "ollama"
# model = "qwen2.5:7b"

# ==============================================================================
# Upstream Translation Services
#
# Supports multiple backends. base_url is required. Local services can omit api_key.
# Use ${{ENV_VAR}} syntax to reference environment variables securely.
# ==============================================================================

# --- Example 1: Local LLM service (Ollama / vLLM / LMStudio) ---
# Local service (pointing to localhost / 127.0.0.1) does not require an API key.
#
# [[service]]
# name = "ollama"
# protocol = "openai"                  # Protocol: openai, anthropic, gemini
# base_url = "http://localhost:11434/v1"
# models = ["qwen2.5:7b", "llama3.2"]

# --- Example 2: Cloud LLM API (DeepSeek, OpenAI, SiliconFlow, etc.) ---
#
# [[service]]
# name = "deepseek"
# protocol = "openai"
# base_url = "https://api.deepseek.com/v1"
# api_key = "${{DEEPSEEK_API_KEY}}"
# models = ["deepseek-chat"]

# --- Example 3: Anthropic Claude Native Protocol ---
# [[service]]
# name = "anthropic"
# protocol = "anthropic"
# base_url = "https://api.anthropic.com/v1"
# api_key = "${{ANTHROPIC_API_KEY}}"
# models = ["claude-3-5-sonnet-20241022"]

# --- Example 4: Google Gemini Native Protocol ---
# [[service]]
# name = "gemini"
# protocol = "gemini"
# base_url = "https://generativelanguage.googleapis.com"
# api_key = "${{GEMINI_API_KEY}}"
# models = ["gemini-1.5-pro", "gemini-2.0-flash"]

[prompt]
# Default target language (e.g. zh-CN, en, ja)
target_lang = "zh-CN"

# Global system prompt template (must include {{target_lang}} placeholder)
system = """
{DEFAULT_SYSTEM_PROMPT}"""
"#,
        unix_path = unix_path.display(),
        DEFAULT_SYSTEM_PROMPT = DEFAULT_SYSTEM_PROMPT.trim(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn test_default_template_is_valid_and_contains_service_examples() {
        let template = default_template();
        assert!(template.contains("# [[service]]"));
        assert!(template.contains("# name = \"ollama\""));
        assert!(template.contains("# name = \"deepseek\""));
        assert!(template.contains("# name = \"anthropic\""));
        assert!(template.contains("# name = \"gemini\""));

        let config: Config = toml::from_str(&template).unwrap();
        assert!(config.service.is_empty());
        assert_eq!(config.server.tcp.as_ref().unwrap().port, 16742);
        assert_eq!(config.prompt.target_lang, "zh-CN");
        assert!(config.validate().is_ok());
    }
}
