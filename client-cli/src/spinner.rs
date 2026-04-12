// src/spinner.rs
//
// Subtle "robot is working" spinner for live API calls.
// No external deps — just tokio + ANSI escapes via the `colored` crate
// (already in Cargo.toml).
//
// Usage:
//
//     let sp = Spinner::start(&["Pinging the nodes…", "Counting containers…"]);
//     let result = do_thing().await;
//     sp.stop();
//
// The spinner is silent if NORDKRAFT_NO_SPINNER=1 or if stdout is not a TTY,
// so it never corrupts piped/redirected output.

use colored::*;
use std::io::{stdout, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

const TICKS: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Spinner {
    handle: Option<JoinHandle<()>>,
    stop_flag: Arc<AtomicBool>,
}

impl Spinner {
    /// Start a spinner that rotates through the given messages.
    /// Returns a no-op handle if stdout is not a TTY or the user opted out.
    pub fn start(messages: &'static [&'static str]) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));

        // Don't render in non-TTY contexts (pipes, CI, etc.) — would corrupt output.
        if !stdout().is_terminal() || std::env::var("NORDKRAFT_NO_SPINNER").is_ok() {
            return Self {
                handle: None,
                stop_flag,
            };
        }

        let stop = stop_flag.clone();
        let handle = tokio::spawn(async move {
            let mut tick = 0usize;
            let mut msg_idx = 0usize;
            // Rotate message every ~12 ticks (≈960ms at 80ms tick rate)
            let rotate_every = 12;

            while !stop.load(Ordering::Relaxed) {
                let frame = TICKS[tick % TICKS.len()];
                let msg = messages[msg_idx % messages.len()];
                // \r returns to line start; ANSI 2K clears the line.
                print!("\r\x1b[2K{} {}", frame.cyan(), msg.dimmed());
                let _ = stdout().flush();

                tokio::time::sleep(Duration::from_millis(80)).await;
                tick += 1;
                if tick % rotate_every == 0 {
                    msg_idx += 1;
                }
            }

            // Clear the spinner line on exit so the real output starts clean.
            print!("\r\x1b[2K");
            let _ = stdout().flush();
        });

        Self {
            handle: Some(handle),
            stop_flag,
        }
    }

    /// Stop the spinner and clear its line. Idempotent.
    pub fn stop(mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            // Best-effort: give the task one tick to clear the line.
            tokio::spawn(async move {
                let _ = h.await;
            });
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        // Safety net if caller forgets to call stop() (e.g. early-return on error).
        self.stop_flag.store(true, Ordering::Relaxed);
    }
}

// =============================================================================
// Message banks — tweak the wording here, not at call sites.
// Keep them dry, lowercase-ish, no exclamation marks. The robot is calm.
// =============================================================================

pub const LIST_MESSAGES: &[&str] = &[
    "Pinging the nodes…",
    "Asking who's awake…",
    "Counting containers…",
];

pub const LOGS_MESSAGES: &[&str] = &[
    "Tracking down the container…",
    "Tailing the tape…",
    "Catching up on the gossip…",
];

pub const INSPECT_MESSAGES: &[&str] = &[
    "Knocking on the agent's door…",
    "Reading the fine print…",
    "Cross-referencing…",
];
