use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// konyac: AI translation service daemon & CLI tool
#[derive(Parser, Debug, Clone)]
#[command(
    name = "konyac",
    version,
    about = "AI translation service daemon & CLI tool"
)]
pub struct Cli {
    /// Path to configuration file
    #[arg(short = 'c', long = "config", env = "KONYAC_CONFIG", global = true)]
    pub config: Option<PathBuf>,

    /// Custom daemon endpoint URL (overrides [server.tcp] host and port)
    #[arg(long = "endpoint", env = "KONYAC_ENDPOINT", global = true)]
    pub endpoint: Option<String>,

    /// Target translation language (defaults to config)
    #[arg(short = 't', long = "target")]
    pub target: Option<String>,

    /// LLM service to use (defaults to configured default)
    #[arg(short = 's', long = "service")]
    pub service: Option<String>,

    /// Model to use (defaults to service's configured model)
    #[arg(short = 'm', long = "model")]
    pub model: Option<String>,

    /// Extra instructions or context for translation
    #[arg(short = 'e', long = "extra")]
    pub extra: Option<String>,

    /// Read text from system clipboard as translation input
    #[arg(short = 'p', long = "paste")]
    pub paste: bool,

    /// Copy translation result to system clipboard
    #[arg(short = 'C', long = "copy")]
    pub copy: bool,

    /// Disable loading spinner animation
    #[arg(long = "no-spinner")]
    pub no_spinner: bool,

    /// Optional subcommand
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Text to translate (supports stdin piping and argument concatenation)
    #[arg(value_name = "TEXT")]
    pub text: Vec<String>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
pub enum Commands {
    /// Start translation daemon
    Serve {
        /// Custom port
        #[arg(short, long)]
        port: Option<u16>,
        /// Custom host
        #[arg(long)]
        host: Option<String>,
        /// Custom Unix domain socket path
        #[arg(short, long)]
        unix: Option<PathBuf>,
    },
    /// Check daemon health and status
    Health,
    /// List available LLM services and configured models
    Services,
    /// Generate shell completion scripts
    Completions {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parsing() {
        let t = Cli::try_parse_from(["konyac", "-t", "ja", "-p", "-C", "--no-spinner", "hello"])
            .unwrap();
        assert_eq!(t.target.as_deref(), Some("ja"));
        assert!(t.paste && t.copy && t.no_spinner);
        assert_eq!(t.text, vec!["hello"]);

        assert!(matches!(
            Cli::try_parse_from(["konyac", "serve", "-p", "18000"])
                .unwrap()
                .command,
            Some(Commands::Serve {
                port: Some(18000),
                ..
            })
        ));
        assert_eq!(
            Cli::try_parse_from(["konyac", "health"]).unwrap().command,
            Some(Commands::Health)
        );
        assert_eq!(
            Cli::try_parse_from(["konyac", "services"]).unwrap().command,
            Some(Commands::Services)
        );
        assert!(matches!(
            Cli::try_parse_from(["konyac", "completions", "bash"])
                .unwrap()
                .command,
            Some(Commands::Completions { .. })
        ));
    }
}
