use crate::api::{HealthResponse, ImageInput, ServiceInfo, ThinkingLevel, TranslateRequest};
use crate::client::KonyacClient;
use crate::config::Config;
use crate::server::BoundServer;
use crate::translation::TranslationService;
use clap::{Parser, Subcommand};
use std::fmt::Write as _;
use std::io::{IsTerminal, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// konyac: Lightweight AI translation daemon & CLI
#[derive(Parser, Debug, Clone)]
#[command(
    name = "konyac",
    version,
    about = "Translate text from arguments or stdin via the konyac daemon"
)]
struct Cli {
    /// Daemon config file, or listener addresses for client commands without --endpoint
    #[arg(
        short = 'c',
        long = "config",
        value_name = "FILE",
        env = "KONYAC_CONFIG",
        global = true
    )]
    config: Option<PathBuf>,

    /// Custom daemon endpoint (http://..., https://..., or unix:/path/to.sock)
    #[arg(
        long = "endpoint",
        value_name = "URL",
        env = "KONYAC_ENDPOINT",
        global = true
    )]
    endpoint: Option<String>,

    /// Override the daemon's default target language
    #[arg(short = 't', long = "target", value_name = "LANG")]
    target: Option<String>,

    /// Override the daemon's default LLM service
    #[arg(short = 's', long = "service", value_name = "NAME")]
    service: Option<String>,

    /// Override the selected service's default model
    #[arg(short = 'm', long = "model", value_name = "MODEL")]
    model: Option<String>,

    /// Reasoning / thinking depth hint (minimal, low, medium, high)
    #[arg(long = "thinking", value_name = "LEVEL")]
    thinking: Option<ThinkingLevel>,

    /// Image file(s) to translate alongside or instead of text (PNG/JPEG/WebP/GIF)
    #[arg(short = 'i', long = "image", value_name = "FILE")]
    image: Vec<PathBuf>,

    /// Extra instructions or context for translation
    #[arg(short = 'e', long = "extra", value_name = "TEXT")]
    extra: Option<String>,

    #[command(subcommand)]
    command: Option<Commands>,

    /// Text to translate (reads from stdin when omitted)
    #[arg(value_name = "TEXT")]
    text: Vec<String>,
}

#[derive(Subcommand, Debug, Clone, PartialEq, Eq)]
enum Commands {
    /// Start translation daemon
    Serve {
        /// Custom TCP listen address (host:port or [ipv6]:port)
        #[arg(long, value_name = "ADDR")]
        tcp: Option<String>,
        /// Custom Unix domain socket path
        #[arg(short, long, value_name = "PATH")]
        unix: Option<PathBuf>,
    },
    /// Check daemon health, default language, and configured services
    Health,
}

pub(super) fn run() -> ExitCode {
    let cli = Cli::parse();
    match dispatch(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let is_serve = matches!(cli.command, Some(Commands::Serve { .. }));
    let runtime = if is_serve {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
    } else {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
    };
    runtime.block_on(run_command(cli))
}

async fn run_command(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let connect = || KonyacClient::from_options(cli.config.as_deref(), cli.endpoint.as_deref());

    match cli.command {
        Some(Commands::Serve { tcp, unix }) => {
            let (config, config_path) = Config::load(cli.config.as_deref())?;
            let server_config = config.server.apply_overrides(tcp, unix)?;
            let target_lang = config.prompt.target_lang.clone();

            let translation = TranslationService::new(&config.service, config.prompt)?;
            let services: Vec<ServiceInfo> = translation.list_services().cloned().collect();

            let bound = BoundServer::bind(translation, &server_config).await?;
            print!(
                "{}",
                format_serve_startup(
                    &config_path,
                    bound.unix_path(),
                    bound
                        .tcp_addr()
                        .map(|addr| (addr, bound.tcp_auth_enabled())),
                    &target_lang,
                    &services,
                )
            );

            bound
                .serve(|signal| println!("\nReceived {signal}, shutting down..."))
                .await?;
            println!("Daemon stopped.");
        }
        Some(Commands::Health) => {
            let health = connect()?.health().await?;
            print!("{}", format_health(&health));
        }
        None => {
            let text = read_input_text(&cli.text)?;
            let images = cli
                .image
                .iter()
                .map(|path| load_image(path))
                .collect::<Result<Vec<_>, _>>()?;
            if text.trim().is_empty() && images.is_empty() {
                return Err(
                    "No input provided. Pass text, pipe stdin, or specify --image <FILE>.".into(),
                );
            }

            let req = TranslateRequest {
                text,
                images,
                target_lang: cli.target,
                system: None,
                extra: cli.extra,
                history: None,
                service: cli.service,
                model: cli.model,
                thinking: cli.thinking,
                json_schema: None,
                cache_key: None,
            };

            let client = connect()?;
            let mut ends_with_newline = None;
            let result = client
                .translate(&req, |chunk| {
                    if !chunk.is_empty() {
                        write_stdout(chunk.as_bytes())?;
                        ends_with_newline = Some(chunk.ends_with('\n'));
                    }
                    Ok(())
                })
                .await;

            if std::io::stdout().is_terminal() && ends_with_newline == Some(false) {
                write_stdout(b"\n")?;
            }
            result?;
        }
    }

    Ok(())
}

fn read_input_text(text_args: &[String]) -> Result<String, String> {
    if !text_args.is_empty() {
        return Ok(text_args.join(" "));
    }
    if !std::io::stdin().is_terminal() {
        let mut buffer = String::new();
        std::io::stdin()
            .read_to_string(&mut buffer)
            .map_err(|e| format!("Failed to read stdin: {e}"))?;
        return Ok(buffer);
    }
    Ok(String::new())
}

const MAX_CLI_IMAGE_BYTES: usize = 6 * 1024 * 1024;

fn load_image(path: &Path) -> Result<ImageInput, String> {
    let bytes = std::fs::read(path)
        .map_err(|e| format!("Failed to read image '{}': {e}", path.display()))?;
    if bytes.is_empty() {
        return Err(format!("Image file '{}' is empty", path.display()));
    }
    if bytes.len() > MAX_CLI_IMAGE_BYTES {
        return Err(format!(
            "Image '{}' exceeds maximum raw size of {} MiB",
            path.display(),
            MAX_CLI_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let mime_type = detect_image_mime(&bytes, path)
        .ok_or_else(|| format!("Unsupported image format: '{}'", path.display()))?;
    Ok(ImageInput {
        mime_type: mime_type.into(),
        data: base64_encode(&bytes),
    })
}

fn detect_image_mime(bytes: &[u8], path: &Path) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some("image/png");
    }
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("image/jpeg");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("webp") => Some("image/webp"),
        Some("gif") => Some("image/gif"),
        _ => None,
    }
}

fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as usize;
        let b1 = *chunk.get(1).unwrap_or(&0) as usize;
        let b2 = *chunk.get(2).unwrap_or(&0) as usize;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) & 0x3f] as char);
        out.push(ALPHABET[(n >> 12) & 0x3f] as char);
        out.push(if chunk.len() > 1 {
            ALPHABET[(n >> 6) & 0x3f] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            ALPHABET[n & 0x3f] as char
        } else {
            '='
        });
    }
    out
}

fn write_stdout(bytes: &[u8]) -> std::io::Result<()> {
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(bytes)?;
    stdout.flush()
}

fn format_health(health: &HealthResponse) -> String {
    let mut out = format!(
        "konyac v{} ({}) - target: {}, {} active service(s)\n",
        health.version,
        health.status,
        health.target_lang,
        health.services.len()
    );
    if health.services.is_empty() {
        out.push_str("  (no services configured)\n");
    } else {
        for service in &health.services {
            let models = if service.models.is_empty() {
                "(none)".into()
            } else {
                service.models.join(", ")
            };
            match service.thinking {
                Some(level) => {
                    let _ = writeln!(
                        out,
                        "  - {} [{}, thinking: {level}]: {models}",
                        service.name, service.protocol
                    );
                }
                None => {
                    let _ = writeln!(out, "  - {} [{}]: {models}", service.name, service.protocol);
                }
            }
        }
    }
    out
}

fn format_serve_startup(
    config_path: &Path,
    unix: Option<&Path>,
    tcp: Option<(SocketAddr, bool)>,
    target_lang: &str,
    services: &[ServiceInfo],
) -> String {
    let mut out = format!(
        "konyac v{} listening (config: {}, target: {target_lang})\n",
        env!("CARGO_PKG_VERSION"),
        config_path.display()
    );
    if let Some(path) = unix {
        let _ = writeln!(out, "  unix: {}", path.display());
    }
    if let Some((addr, auth)) = tcp {
        let suffix = if auth { " (bearer auth)" } else { "" };
        let _ = writeln!(out, "  tcp: http://{addr}{suffix}");
    }
    match services.first() {
        Some(default) => {
            let model = default
                .models
                .first()
                .map(String::as_str)
                .unwrap_or("(none)");
            let _ = writeln!(
                out,
                "  default: {}/{model} ({} service(s) loaded)",
                default.name,
                services.len()
            );
        }
        None => out.push_str("  services: none configured\n"),
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::borrow::Cow;

    #[test]
    fn parses_cli_flags_and_subcommands() {
        let cli = Cli::try_parse_from([
            "konyac",
            "-t",
            "ja",
            "-s",
            "ollama",
            "--thinking",
            "low",
            "-i",
            "shot.png",
            "hello",
        ])
        .unwrap();
        assert_eq!(cli.target.as_deref(), Some("ja"));
        assert_eq!(cli.service.as_deref(), Some("ollama"));
        assert_eq!(cli.thinking, Some(ThinkingLevel::Low));
        assert_eq!(cli.image, vec![PathBuf::from("shot.png")]);
        assert_eq!(cli.text, vec!["hello"]);

        let img_only = Cli::try_parse_from(["konyac", "-i", "a.png"]).unwrap();
        assert!(img_only.text.is_empty());
        assert_eq!(img_only.image, vec![PathBuf::from("a.png")]);

        assert!(matches!(
            Cli::try_parse_from(["konyac", "serve", "--tcp", "127.0.0.1:18000", "-u", "/tmp/k.sock"])
                .unwrap()
                .command,
            Some(Commands::Serve {
                tcp: Some(ref tcp),
                unix: Some(ref unix),
            }) if tcp == "127.0.0.1:18000" && unix == Path::new("/tmp/k.sock")
        ));
        assert_eq!(
            Cli::try_parse_from(["konyac", "health"]).unwrap().command,
            Some(Commands::Health)
        );
    }

    #[test]
    fn encodes_and_detects_images() {
        for (raw, encoded) in [("", ""), ("f", "Zg=="), ("fo", "Zm8="), ("foo", "Zm9v")] {
            assert_eq!(base64_encode(raw.as_bytes()), encoded);
        }
        for (bytes, file, expected) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), "noext", Some("image/png")),
            (&[0xff, 0xd8, 0xff], "noext", Some("image/jpeg")),
            (b"RIFF1234WEBP", "noext", Some("image/webp")),
            (b"GIF89a", "noext", Some("image/gif")),
            (b"raw", "pic.jpeg", Some("image/jpeg")),
            (b"raw", "pic.txt", None),
        ] {
            assert_eq!(
                detect_image_mime(bytes, Path::new(file)),
                expected,
                "{file}"
            );
        }
    }

    #[test]
    fn formats_health_and_startup_summary() {
        let services = vec![
            ServiceInfo {
                name: "deepseek".into(),
                protocol: "openai".into(),
                models: vec!["deepseek-flash".into()],
                thinking: None,
            },
            ServiceInfo {
                name: "gemini".into(),
                protocol: "gemini".into(),
                models: vec!["gemini-3.8-flash".into()],
                thinking: Some(ThinkingLevel::Low),
            },
        ];
        let health = HealthResponse {
            status: Cow::Borrowed("ok"),
            version: Cow::Borrowed("0.1.0"),
            target_lang: "zh-CN".into(),
            services: services.clone(),
        };
        assert_eq!(
            format_health(&health),
            "konyac v0.1.0 (ok) - target: zh-CN, 2 active service(s)\n  - deepseek [openai]: deepseek-flash\n  - gemini [gemini, thinking: low]: gemini-3.8-flash\n"
        );

        let empty_health = HealthResponse {
            status: Cow::Borrowed("ok"),
            version: Cow::Borrowed("0.1.0"),
            target_lang: "ja".into(),
            services: Vec::new(),
        };
        assert_eq!(
            format_health(&empty_health),
            "konyac v0.1.0 (ok) - target: ja, 0 active service(s)\n  (no services configured)\n"
        );

        let summary = format_serve_startup(
            Path::new("/etc/konyac/config.toml"),
            Some(Path::new("/run/user/1000/konyac.sock")),
            Some(("127.0.0.1:16742".parse().unwrap(), true)),
            "zh-CN",
            &services,
        );
        assert!(summary.contains(&format!(
            "konyac v{} listening (config: /etc/konyac/config.toml, target: zh-CN)\n",
            env!("CARGO_PKG_VERSION")
        )));
        assert!(summary.contains("  unix: /run/user/1000/konyac.sock\n"));
        assert!(summary.contains("  tcp: http://127.0.0.1:16742 (bearer auth)\n"));
        assert!(summary.contains("  default: deepseek/deepseek-flash (2 service(s) loaded)\n"));

        let no_services = format_serve_startup(
            Path::new("/etc/konyac/config.toml"),
            None,
            Some(("127.0.0.1:16742".parse().unwrap(), false)),
            "ja",
            &[],
        );
        assert!(no_services.contains("  services: none configured\n"));
    }
}
