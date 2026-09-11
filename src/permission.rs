use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use crate::events::{GateChoice, Sink, UiEvent};

/// Permission gate for mutating tools. y = once, a = rest of session,
/// n = deny (returned to the model as a tool result).
/// With `set_ui`, prompts become modal events: the UI decides.
pub struct Gate {
    auto: bool,
    session_allow: bool,
    sink: Option<Sink>,
    cancel: Option<Arc<AtomicBool>>,
    seq: u64,
}

impl Gate {
    pub fn new(auto: bool) -> Self {
        Self {
            auto,
            session_allow: false,
            sink: None,
            cancel: None,
            seq: 0,
        }
    }

    /// Interactive mode: decisions arrive via UiEvent::Permission replies.
    pub fn set_ui(&mut self, sink: Sink, cancel: Arc<AtomicBool>) {
        self.sink = Some(sink);
        self.cancel = Some(cancel);
    }

    /// Returns true if the action may proceed.
    pub fn check(&mut self, summary: &str) -> bool {
        if self.auto || self.session_allow {
            eprintln!("» allow (auto): {summary}");
            return true;
        }
        if let Some(tx) = self.sink.clone() {
            self.seq += 1;
            let (reply_tx, reply_rx) = std::sync::mpsc::channel();
            let _ = tx.send(UiEvent::Permission {
                id: self.seq,
                summary: summary.to_string(),
                reply: reply_tx,
            });
            // Block until the UI answers or the run is cancelled. The poll
            // interval lets a Stop break a parked gate instead of deadlocking.
            loop {
                match reply_rx.recv_timeout(Duration::from_millis(200)) {
                    Ok(choice) => match choice {
                        GateChoice::Once => return true,
                        GateChoice::Session => {
                            self.session_allow = true;
                            return true;
                        }
                        GateChoice::Deny => return false,
                    },
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if self
                            .cancel
                            .as_ref()
                            .map(|c| c.load(Ordering::Relaxed))
                            .unwrap_or(false)
                        {
                            return false;
                        }
                    }
                    Err(_) => return false, // UI gone
                }
            }
        }
        eprint!("» allow {summary}? [y/n/a] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" | "" => true, // empty = yes, single-keystroke flow
            "a" | "all" => {
                self.session_allow = true;
                true
            }
            _ => false,
        }
    }
}
