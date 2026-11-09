use std::io::{IsTerminal, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::task::JoinHandle;

/// Terminal loading spinner active until the first response token arrives.
pub struct Spinner {
    stopped: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Starts the spinner on stderr if stderr is a terminal and spinner is enabled.
    pub fn start(message: &'static str, enabled: bool) -> Self {
        let is_term = enabled
            && std::io::stderr().is_terminal()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true);

        if !is_term {
            return Self {
                stopped: Arc::new(AtomicBool::new(true)),
                handle: None,
            };
        }

        let use_color = std::env::var("NO_COLOR").is_err();
        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_clone = stopped.clone();

        let handle = tokio::spawn(async move {
            let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let mut i = 0;
            while !stopped_clone.load(Ordering::Relaxed) {
                let frame = frames[i % frames.len()];
                i += 1;
                if use_color {
                    eprint!("\r\x1b[2K\x1b[36m{frame}\x1b[0m {message}");
                } else {
                    eprint!("\r\x1b[2K{frame} {message}");
                }
                let _ = std::io::stderr().flush();
                tokio::time::sleep(Duration::from_millis(80)).await;
            }
        });

        Self {
            stopped,
            handle: Some(handle),
        }
    }

    /// Stops the spinner and clears the current line on stderr.
    pub fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::Relaxed) {
            return;
        }

        if let Some(handle) = self.handle.take() {
            handle.abort();
            eprint!("\r\x1b[2K");
            let _ = std::io::stderr().flush();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_spinner_lifecycle() {
        let mut spinner = Spinner::start("Translating...", false);
        assert!(spinner.stopped.load(Ordering::Relaxed));
        assert!(spinner.handle.is_none());
        spinner.stop();
        assert!(spinner.stopped.load(Ordering::Relaxed));
    }
}
