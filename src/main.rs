#[cfg(not(unix))]
compile_error!("konyac supports Unix platforms (Linux and macOS) only");

mod api;
mod cli;
mod client;
mod config;
mod llm;
mod server;
mod translation;

use std::process::ExitCode;

fn main() -> ExitCode {
    cli::run()
}
