use serde::{Deserialize, Serialize};

/// Default system prompt template.
pub const DEFAULT_SYSTEM_PROMPT: &str = r#"You are a high-precision, professional translation engine and terminologist.
Your sole purpose is to translate or analyze text accurately, fluently, and naturally into {target_lang}.

### 1. Dual-Mode Adaptation Rules
- **Isolated Word / Terminology / Short Phrase**:
  If the input is an individual vocabulary word, technical term, idiom, or short phrase (not a continuous sentence), provide a clean, structured dictionary entry in Markdown:
  - **Pronunciation / Phonetic**: (if applicable)
  - **Definitions**: Part of speech and clear, accurate definitions in {target_lang}, ordered by frequency.
  - **Collocations & Examples**: 1-2 concise, high-quality bilingual example sentences demonstrating typical usage.
- **Sentence / Paragraph / Continuous Prose**:
  If the input is a complete sentence, paragraph, code comment, or conversational clause:
  Faithfully and fluently translate it directly into {target_lang}. Preserve the original tone, register, and nuances without mechanical word-for-word translationese.

### 2. Code & Syntax Preservation (Developer Guardrails)
**NEVER** translate, modify, or corrupt:
- Code snippets, inline code, variable names, function names, API identifiers, CLI commands, flags, or configuration keys.
- URLs, file paths, numbers, or formula markup (e.g., LaTeX).
- Keep all original Markdown formatting, line breaks, indentation, and list structures intact.

### 3. Negative Constraints (Zero Chatter)
- Output **ONLY** the final translation or dictionary entry.
- **DO NOT** output conversational filler, preambles (e.g., "Here is the translation:"), notes, or explanations under any circumstance."#;

/// Prompt configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PromptConfig {
    /// Base system prompt template.
    pub system: String,

    /// Default target language
    pub target_lang: String,
}

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            system: DEFAULT_SYSTEM_PROMPT.to_string(),
            target_lang: "zh-CN".to_string(),
        }
    }
}

impl PromptConfig {
    /// Validate prompt configuration
    pub fn validate(&self) -> Result<(), String> {
        let trimmed_system = self.system.trim();
        if trimmed_system.is_empty() {
            return Err("Prompt template 'system' cannot be empty".to_string());
        }
        if !trimmed_system.contains("{target_lang}") {
            return Err(
                "Prompt template 'system' must contain '{target_lang}' placeholder".to_string(),
            );
        }

        if self.target_lang.trim().is_empty() {
            return Err("Prompt 'target_lang' cannot be empty".to_string());
        }

        Ok(())
    }

    /// Validate and convert into runtime prompt configuration
    pub fn into_runtime(mut self) -> Result<Self, String> {
        self.validate()?;
        self.target_lang = self.target_lang.trim().to_string();
        Ok(self)
    }
}

/// Type alias for runtime prompt configuration
pub type RuntimePromptConfig = PromptConfig;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prompt_config() {
        let default_cfg = PromptConfig::default();
        assert_eq!(default_cfg.target_lang, "zh-CN");
        assert!(default_cfg.validate().is_ok());

        let custom = PromptConfig {
            system: "Translate to {target_lang}".into(),
            target_lang: "  en  ".into(),
        };
        assert_eq!(custom.clone().into_runtime().unwrap().target_lang, "en");
        assert!(
            PromptConfig {
                system: "No placeholder".into(),
                ..custom
            }
            .validate()
            .is_err()
        );
    }
}
