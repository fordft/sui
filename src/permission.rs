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
    /// Shared session-scoped approval flag (TUI): 'a' raises it, the
    /// Ask/Auto toggle clears it — revocation reaches the live gate
    /// without touching the agent. Absent on headless/stdin paths.
    session: Option<Arc<AtomicBool>>,
    sink: Option<Sink>,
    cancel: Option<Arc<AtomicBool>>,
    seq: u64,
}

impl Gate {
    pub fn new(auto: bool) -> Self {
        Self {
            auto,
            session_allow: false,
            session: None,
            sink: None,
            cancel: None,
            seq: 0,
        }
    }

    /// Interactive mode: decisions arrive via UiEvent::Permission replies.
    /// `session` is the live session-approval flag shared with the UI.
    pub fn set_ui(
        &mut self,
        sink: Sink,
        cancel: Arc<AtomicBool>,
        session: Option<Arc<AtomicBool>>,
    ) {
        self.sink = Some(sink);
        self.cancel = Some(cancel);
        self.session = session;
    }

    /// Effective "skip prompts" state: constructor auto, a session grant,
    /// or the shared flag. Re-checked on every dispatch so revoking [a]
    /// or toggling Auto off takes effect on the very next tool call.
    fn open(&self) -> bool {
        self.auto
            || self.session_allow
            || self
                .session
                .as_ref()
                .map(|f| f.load(Ordering::Relaxed))
                .unwrap_or(false)
    }

    /// Returns true if the action may proceed. `run` ties the prompt to
    /// the activity group that spawned it.
    pub async fn check(&mut self, summary: &str, agent: &str, run: u64) -> bool {
        self.decide(summary, agent, run).await != GateChoice::Deny
    }

    /// Same gate as check, preserving one-action versus session approval.
    pub async fn decide(&mut self, summary: &str, agent: &str, run: u64) -> GateChoice {
        self.decide_inner(summary, agent, run, false).await
    }

    /// Independent surface consent: local YOLO/session grants cannot authorize it.
    /// A session response is owned by the caller, never raises the local gate.
    pub async fn decide_surface(&mut self, summary: &str, agent: &str, run: u64) -> GateChoice {
        self.decide_inner(summary, agent, run, true).await
    }

    async fn decide_inner(
        &mut self,
        summary: &str,
        agent: &str,
        run: u64,
        surface: bool,
    ) -> GateChoice {
        if !surface && self.open() {
            // Under a UI (sink set) raw writes would corrupt the alt screen.
            if self.sink.is_none() {
                eprintln!("» allow (auto): {summary}");
            }
            return GateChoice::Session;
        }
        if let Some(tx) = self.sink.clone() {
            self.seq += 1;
            let (reply_tx, mut reply_rx) = tokio::sync::mpsc::unbounded_channel();
            let _ = tx.send(UiEvent::Permission {
                run,
                id: self.seq,
                agent: agent.to_string(),
                summary: summary.to_string(),
                reply: reply_tx,
            });
            // Wait for the UI's answer or a Stop. This must stay fully
            // async: a blocking recv inside the agent task starves its
            // runtime worker — the TUI's select then stops redrawing and
            // the modal only responds once per physical keypress.
            loop {
                tokio::select! {
                    choice = reply_rx.recv() => match choice {
                        Some(c @ (GateChoice::Once | GateChoice::Session | GateChoice::Deny)) => {
                            if !surface && matches!(c, GateChoice::Session) {
                                // Raise the shared session flag — revocable by
                                // the UI toggle. Local session_allow stays for
                                // the stdin path below.
                                if let Some(f) = &self.session {
                                    f.store(true, Ordering::Relaxed);
                                } else {
                                    self.session_allow = true;
                                }
                            }
                            return c;
                        }
                        None => return GateChoice::Deny, // UI gone
                    },
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {
                        if self
                            .cancel
                            .as_ref()
                            .map(|c| c.load(Ordering::Relaxed))
                            .unwrap_or(false)
                        {
                            return GateChoice::Deny;
                        }
                    }
                }
            }
        }
        eprint!("» allow {summary}? [y/n/a] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => return GateChoice::Deny,
            Ok(_) => {}
        }
        match line.trim().to_lowercase().as_str() {
            "y" | "yes" | "" => GateChoice::Once, // empty = yes, single-keystroke flow
            "a" | "all" => {
                if !surface {
                    self.session_allow = true;
                }
                GateChoice::Session
            }
            _ => GateChoice::Deny,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn yolo_does_not_skip_surface_consent() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut gate = Gate::new(true);
        gate.set_ui(tx, Arc::new(AtomicBool::new(false)), None);
        let task = tokio::spawn(async move { gate.decide_surface("browser", "worker", 1).await });
        let UiEvent::Permission { reply, .. } = rx.recv().await.unwrap() else {
            panic!("missing independent prompt");
        };
        reply.send(GateChoice::Deny).unwrap();
        assert_eq!(task.await.unwrap(), GateChoice::Deny);
    }

    #[tokio::test]
    async fn surface_session_consent_does_not_authorize_local_writes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let session = Arc::new(AtomicBool::new(false));
        let mut gate = Gate::new(false);
        gate.set_ui(tx, Arc::new(AtomicBool::new(false)), Some(session.clone()));
        let task = tokio::spawn(async move {
            assert_eq!(
                gate.decide_surface("browser", "worker", 1).await,
                GateChoice::Session
            );
            gate.check("write_file", "worker", 1).await
        });
        let UiEvent::Permission { reply, .. } = rx.recv().await.unwrap() else {
            panic!("missing browser prompt");
        };
        reply.send(GateChoice::Session).unwrap();
        let UiEvent::Permission { reply, .. } = rx.recv().await.unwrap() else {
            panic!("browser session leaked into local gate");
        };
        assert!(!session.load(Ordering::Relaxed));
        reply.send(GateChoice::Deny).unwrap();
        assert!(!task.await.unwrap());
    }
}
