use serde::{Deserialize, Serialize};

/// Options for customizing prompt behavior.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PromptOptions {
    /// Custom system prompt overriding the base prompt.
    #[serde(default)]
    pub system: Option<String>,

    /// Supplemental instructions or context.
    #[serde(default)]
    pub extra: Option<String>,
}

/// System prompt assembly engine.
#[derive(Debug, Clone)]
pub struct PromptEngine {
    base_system: String,
}

impl Default for PromptEngine {
    fn default() -> Self {
        Self::new(crate::config::DEFAULT_SYSTEM_PROMPT)
    }
}

impl PromptEngine {
    /// Creates a prompt engine from a base template.
    pub fn new(base_system: impl Into<String>) -> Self {
        Self {
            base_system: base_system.into(),
        }
    }

    /// Resolves the effective system prompt for a target language and options.
    pub fn resolve(&self, target_lang: &str, options: Option<&PromptOptions>) -> String {
        let extra = options
            .and_then(|o| o.extra.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        let mut prompt = if let Some(custom) = options
            .and_then(|o| o.system.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            custom.replace("{target_lang}", target_lang)
        } else {
            self.base_system.replace("{target_lang}", target_lang)
        };

        if let Some(extra) = extra {
            prompt.push_str(
                "\n\n### [Section: Additional Instructions & Context]\n> Purpose: Client-specified ad-hoc directives, context, and terminology.\n",
            );
            if extra.contains("{target_lang}") {
                prompt.push_str(&extra.replace("{target_lang}", target_lang));
            } else {
                prompt.push_str(extra);
            }
        }

        prompt
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_resolution() {
        let engine = PromptEngine::default();
        assert!(engine.resolve("zh-CN", None).contains("zh-CN"));

        let custom = PromptOptions {
            system: Some("Custom {target_lang}".into()),
            extra: Some("Note {target_lang}".into()),
        };
        let resolved = engine.resolve("ja", Some(&custom));
        assert!(resolved.starts_with("Custom ja"));
        assert!(resolved.contains("Note ja"));
    }
}
