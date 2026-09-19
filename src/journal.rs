use anyhow::Result;
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// Event-type names are an implicit contract between writers (tui,
/// mission, headless) and readers (export, --latest). Centralize the
/// ones that must match so a fourth writer can't drift.
pub mod ev {
    /// Run-level metadata: mode, workspace, sui_version, approval.
    pub const SESSION: &str = "session";
}

/// One-shot failure-diagnostic sink (see `Journal::notice`).
type Notice = Box<dyn Fn(&str) + Send>;

/// Append-only event journal. Lives outside the repo so writes never
/// perturb the repository epoch fingerprint.
pub struct Journal {
    f: File,
    /// Latched on the first persistence failure — the journal is the
    /// run's evidence trail, so once writes break we stop half-writing
    /// and let consumers see the gap.
    failed: bool,
    /// Where the one-time failure diagnostic goes. Default stderr —
    /// but under the TUI alt-screen a raw eprintln corrupts the
    /// display, so wired agents reroute it into the UiEvent stream.
    notice: Option<Notice>,
}

impl Journal {
    pub fn open(run_dir: &Path) -> Result<Self> {
        Self::open_named(run_dir, "events")
    }

    /// Named journal within the same run dir (per-scenario logs).
    pub fn open_named(run_dir: &Path, name: &str) -> Result<Self> {
        std::fs::create_dir_all(run_dir)?;
        // Journals hold prompts, code, and command output — same
        // sensitivity as the sanitized export, which is written 0600.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(run_dir, std::fs::Permissions::from_mode(0o700));
        }
        let mut o = OpenOptions::new();
        o.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600);
        }
        let f = o.open(run_dir.join(format!("{name}.jsonl")))?;
        Ok(Self {
            f,
            failed: false,
            notice: None,
        })
    }

    /// Route the failure diagnostic somewhere other than stderr —
    /// called by agents with a UI event channel. Fires on the first
    /// failure only (the latch suppresses repeats).
    pub fn set_notice(&mut self, f: Box<dyn Fn(&str) + Send>) {
        self.notice = Some(f);
    }

    fn notice(&self, msg: &str) {
        match &self.notice {
            Some(f) => f(msg),
            None => eprintln!("journal: {msg}"),
        }
    }

    /// Path of a named journal file (for replay/reconstruction).
    pub fn path_of(run_dir: &Path, name: &str) -> PathBuf {
        run_dir.join(format!("{name}.jsonl"))
    }

    /// Append one event. Returns () — callers can't meaningfully
    /// recover mid-turn — but failures leave evidence instead of
    /// vanishing: a serialize error writes a `journal_error` marker so
    /// the gap is IN the stream; an IO error latches `failed` and
    /// emits one stderr diagnostic rather than spamming or
    /// half-writing every subsequent event.
    pub fn log(&mut self, kind: &str, data: Value) {
        if self.failed {
            return;
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        let ev = json!({ "ts_unix": ts, "type": kind, "data": data });
        let line = match serde_json::to_string(&ev) {
            Ok(l) => l,
            Err(e) => {
                // serialize failure is a data bug, not IO — record a
                // fixed-format marker (can't itself fail to serialize)
                // so replay/export sees the gap, not a bare newline.
                // `kind` is a compile-time string, but escape anyway —
                // a raw " in a kind name would corrupt the marker JSON.
                self.notice(&format!("serialize {kind}: {e}"));
                format!(
                    "{{\"ts_unix\":{ts},\"type\":\"journal_error\",\"data\":{{\"serialize_failed\":{}}}}}",
                    json!(kind)
                )
            }
        };
        if self
            .f
            .write_all(line.as_bytes())
            .and_then(|_| self.f.write_all(b"\n"))
            .and_then(|_| self.f.flush())
            .is_err()
        {
            self.failed = true;
            self.notice("write failed — evidence for this run is incomplete");
        }
    }

    /// True once any event failed to persist — the run's journal is
    /// provably incomplete from that point.
    pub fn failed(&self) -> bool {
        self.failed
    }
}

#[cfg(test)]
impl Journal {
    fn for_test(f: File) -> Self {
        Self {
            f,
            failed: false,
            notice: None,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn io_failure_latches_failed() {
        // /dev/full returns ENOSPC on write — deterministic IO fault.
        let f = OpenOptions::new().write(true).open("/dev/full").unwrap();
        let mut j = Journal::for_test(f);
        assert!(!j.failed());
        j.log("user", json!({"x": 1}));
        assert!(j.failed(), "write failure must latch");
        // subsequent events don't half-write or spam — early return
        j.log("user", json!({"x": 2}));
        assert!(j.failed());
    }
}
