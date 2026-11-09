mod cli;
mod client;
mod clipboard;
mod config;
mod llm;
mod prompt_engine;
mod server;
mod spinner;

use clap::{CommandFactory, Parser};
use cli::{Cli, Commands};
use client::KonyacClient;
use config::Config;
use llm::LlmGateway;
use prompt_engine::PromptEngine;
use server::types::{PromptOptions, TranslateRequest};
use spinner::Spinner;
use std::io::{IsTerminal, Read, Write};

fn read_input_text(paste: bool, text_args: &[String]) -> Result<String, String> {
    if paste {
        return clipboard::get_text();
    }
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

fn resolve_service_and_model(
    services: &[config::ServiceConfig],
    default_cfg: &config::RuntimeDefaultConfig,
    cli_service: Option<&str>,
    cli_model: Option<String>,
) -> Result<(String, String), String> {
    let (service, is_default_service) = match cli_service {
        Some(s) => {
            let is_default = default_cfg.service.as_deref() == Some(s);
            (s.to_string(), is_default)
        }
        None => {
            if let Some(ref def_srv) = default_cfg.service {
                (def_srv.clone(), true)
            } else if let Some(first) = services.first() {
                (first.name.clone(), true)
            } else {
                return Err(
                    "No LLM service specified and no services configured in config.toml.\nPlease add at least one [[service]] section or specify -s / --service.".into(),
                );
            }
        }
    };

    let model = match cli_model {
        Some(m) => m,
        None => {
            if is_default_service && let Some(ref def_model) = default_cfg.model {
                def_model.clone()
            } else if let Some(srv) = services.iter().find(|s| s.name == service) {
                if let Some(first_model) = srv.models.first() {
                    first_model.clone()
                } else {
                    return Err(format!(
                        "Service '{service}' has no models configured. Please specify -m / --model <name> or configure models in config.toml."
                    ));
                }
            } else {
                return Err(format!(
                    "No model specified and service '{service}' not found in local config. Please specify -m / --model <name>."
                ));
            }
        }
    };

    Ok((service, model))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    if let Some(Commands::Completions { shell }) = cli.command {
        let mut cmd = Cli::command();
        let bin_name = cmd.get_name().to_string();
        clap_complete::generate(shell, &mut cmd, bin_name, &mut std::io::stdout());
        return Ok(());
    }

    let (config, config_path) = Config::load_or_create(cli.config.as_deref())?;

    match cli.command {
        Some(Commands::Serve { port, host, unix }) => {
            println!("Configuration loaded from: {}", config_path.display());
            let mut runtime_config = config.into_runtime()?;
            runtime_config.server = runtime_config.server.apply_overrides(host, port, unix)?;

            let prompt_engine = PromptEngine::new(runtime_config.prompt.system);
            let gateway = LlmGateway::new(&runtime_config.service)?;
            let services = gateway.list_services();
            println!(
                "LLM gateway initialized with {} active service(s).",
                services.len()
            );

            for info in services {
                println!(
                    "Service '{}' [protocol: {}] ready with models: {:?}",
                    info.name, info.protocol, info.configured_models
                );
            }

            if let Some(ref def_srv) = runtime_config.default.service {
                println!("Default service: '{def_srv}'");
            }
            if let Some(ref def_model) = runtime_config.default.model {
                println!("Default model:   '{def_model}'");
            }

            if runtime_config.service.is_empty() {
                println!(
                    "Note: No translation services configured in {}.\nPlease add at least one [[service]] section to start translating.",
                    config_path.display()
                );
            }

            server::run(prompt_engine, gateway, &runtime_config.server).await?;
        }
        Some(Commands::Health) => {
            let client = KonyacClient::from_config(&config.server, cli.endpoint.as_deref())?;
            match client.health().await {
                Ok(health) => {
                    println!("konyac daemon is healthy");
                    println!("  Version:         {}", health.version);
                    println!("  Status:          {}", health.status);
                    println!("  Active services: {}", health.services_count);
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Services) => {
            let client = KonyacClient::from_config(&config.server, cli.endpoint.as_deref())?;
            match client.services().await {
                Ok(services) => {
                    if services.is_empty() {
                        println!("No services currently configured or loaded in daemon.");
                    } else {
                        println!("Active LLM Services ({}):", services.len());
                        for s in services {
                            println!("- {} [protocol: {}]", s.name, s.protocol);
                            if s.configured_models.is_empty() {
                                println!("    Models: (none configured)");
                            } else {
                                println!("    Models: {}", s.configured_models.join(", "));
                            }
                        }
                    }
                }
                Err(e) => {
                    eprintln!("{e}");
                    std::process::exit(1);
                }
            }
        }
        Some(Commands::Completions { .. }) => unreachable!(),
        None => {
            let text = match read_input_text(cli.paste, &cli.text) {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            };

            let text = text.trim();
            if text.is_empty() {
                eprintln!("Error: No text provided for translation.\n");
                eprintln!("Usage:");
                eprintln!("  konyac [OPTIONS] <TEXT>...");
                eprintln!("  konyac -p [OPTIONS]         (translate from clipboard)");
                eprintln!("  echo \"text\" | konyac [OPTIONS]");
                eprintln!("  konyac --help");
                std::process::exit(1);
            }

            let default_cfg = config.default.into_runtime()?;
            let prompt_cfg = config.prompt.into_runtime()?;
            let target_lang = cli.target.unwrap_or(prompt_cfg.target_lang);

            let (service, model) = match resolve_service_and_model(
                &config.service,
                &default_cfg,
                cli.service.as_deref(),
                cli.model,
            ) {
                Ok(pair) => pair,
                Err(e) => {
                    eprintln!("Error: {e}");
                    std::process::exit(1);
                }
            };

            let prompt = cli.extra.map(|extra| PromptOptions {
                system: None,
                extra: Some(extra),
            });

            let req = TranslateRequest {
                text: text.to_string(),
                target_lang,
                prompt,
                history: None,
                service,
                model,
                json_schema: None,
            };

            let client = KonyacClient::from_config(&config.server, cli.endpoint.as_deref())?;

            let mut spinner = Spinner::start("Translating...", !cli.no_spinner);
            let mut last_char_was_newline = true;
            let mut any_output = false;
            let mut collected = if cli.copy { Some(String::new()) } else { None };

            let result = client
                .translate(&req, |chunk| {
                    if !chunk.is_empty() {
                        if !any_output {
                            spinner.stop();
                        }
                        any_output = true;
                        if let Some(ref mut buf) = collected {
                            buf.push_str(chunk);
                        }
                        print!("{chunk}");
                        let _ = std::io::stdout().flush();
                        last_char_was_newline = chunk.ends_with('\n');
                    }
                })
                .await;

            spinner.stop();

            if let Err(e) = result {
                if any_output && !last_char_was_newline {
                    eprintln!();
                }
                eprintln!("{e}");
                std::process::exit(1);
            }

            if any_output && !last_char_was_newline {
                println!();
            }

            if let Some(buf) = collected {
                let trimmed = buf.trim();
                if !trimmed.is_empty()
                    && let Err(e) = clipboard::set_text(trimmed)
                {
                    eprintln!("Warning: Failed to copy to clipboard: {e}");
                }
            }
        }
    }

    Ok(())
}
