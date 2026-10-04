use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
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
    replay_dir: PathBuf,
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
            replay_dir: run_dir.to_path_buf(),
            failed: false,
            notice: None,
        })
    }

    /// Opaque replay state is kept in bounded owner-only sidecars, never
    /// inline in journals or exports. Content addressing binds the reference.
    pub fn store_response_items(&self, items: &[Value]) -> Result<Option<Value>> {
        if items.is_empty() {
            return Ok(None);
        }
        let bytes = serde_json::to_vec(items)?;
        if bytes.len() > REPLAY_LIMIT {
            bail!("opaque replay state exceeds 4 MiB");
        }
        let hash = crate::context::sha256_hex(&bytes);
        let filename = format!("replay-{hash}.json");
        let path = self.replay_dir.join(&filename);
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        match options.open(&path) {
            Ok(mut file) => {
                file.write_all(&bytes)?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if read_replay_file(&path)? != bytes {
                    bail!("opaque replay state hash collision or damaged sidecar");
                }
            }
            Err(e) => return Err(e.into()),
        }
        Ok(Some(json!({"file": filename, "sha256": hash})))
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

const REPLAY_LIMIT: usize = 4 * 1024 * 1024;
const JOURNAL_LINE_LIMIT: usize = 16 * 1024 * 1024;

pub(crate) fn read_replay_file(path: &Path) -> Result<Vec<u8>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options
        .open(path)
        .context("opaque replay sidecar is unavailable")?;
    if !file.metadata()?.is_file() || file.metadata()?.len() > REPLAY_LIMIT as u64 {
        bail!("opaque replay sidecar exceeds bounds");
    }
    let mut bytes = Vec::new();
    file.take(REPLAY_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > REPLAY_LIMIT {
        bail!("opaque replay sidecar exceeds bounds");
    }
    Ok(bytes)
}

/// Restore the active context epoch. Legacy text journals remain readable;
/// image observations and missing/tampered opaque state cannot claim replay.
pub fn replay_history(path: &Path, max_users: usize) -> Result<Vec<crate::types::Message>> {
    let mut events = Vec::new();
    let mut bytes = 0;
    let mut users = 0;
    let mut reader = BufReader::new(File::open(path)?);
    loop {
        let mut line = String::new();
        let count = reader
            .by_ref()
            .take(JOURNAL_LINE_LIMIT as u64 + 1)
            .read_line(&mut line)?;
        if count == 0 {
            break;
        }
        if count > JOURNAL_LINE_LIMIT {
            bail!("journal event exceeds replay bounds");
        }
        let event: Value = serde_json::from_str(&line)?;
        if event["type"] == "user" {
            users += 1;
            if users > max_users {
                break;
            }
        }
        bytes += count;
        if bytes > 32 * 1024 * 1024 {
            bail!("journal exceeds 32 MiB replay limit");
        }
        events.push(event);
    }
    replay_events(path, events.iter())
}

pub(crate) fn replay_events<'a>(
    path: &Path,
    events: impl IntoIterator<Item = &'a Value>,
) -> Result<Vec<crate::types::Message>> {
    use crate::types::Message;
    let mut history = Vec::new();
    let mut replay_bytes = 0;
    for event in events {
        replay_bytes += serde_json::to_vec(event)?.len();
        let data = &event["data"];
        match event["type"].as_str() {
            Some("journal_error") => bail!("cannot replay incomplete journal"),
            Some("image_observation") => bail!(
                "cannot reconstruct identical request history: image observation was memory-only"
            ),
            Some("context_checkpoint") => {
                history = serde_json::from_value(data["messages"].clone())
                    .context("invalid context checkpoint")?;
            }
            Some("user") => history.push(Message::User {
                content: data["content"]
                    .as_str()
                    .context("invalid user event")?
                    .into(),
            }),
            Some("assistant") => {
                let mut items = Vec::new();
                if let Some(reference) = data.get("response_items_ref").filter(|r| !r.is_null()) {
                    let hash = reference["sha256"]
                        .as_str()
                        .context("missing replay hash")?;
                    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                        bail!("invalid replay hash");
                    }
                    let expected = format!("replay-{hash}.json");
                    if reference["file"].as_str() != Some(expected.as_str()) {
                        bail!("invalid replay sidecar path");
                    }
                    let bytes = read_replay_file(
                        &path
                            .parent()
                            .context("journal parent unavailable")?
                            .join(expected),
                    )?;
                    if crate::context::sha256_hex(&bytes) != hash {
                        bail!("opaque replay state failed integrity check");
                    }
                    items = serde_json::from_slice(&bytes)?;
                    replay_bytes += bytes.len();
                } else if data["response_items_count"].as_u64().unwrap_or(0) > 0 {
                    bail!("opaque replay state is missing");
                }
                let content = data["content"]
                    .as_str()
                    .filter(|c| !c.is_empty())
                    .map(String::from);
                history.push(Message::Assistant {
                    content,
                    tool_calls: data
                        .get("tool_calls")
                        .filter(|v| !v.is_null())
                        .map(|v| serde_json::from_value::<Vec<crate::types::ToolCall>>(v.clone()))
                        .transpose()
                        .context("invalid recorded tool calls")?
                        .filter(|v| !v.is_empty()),
                    reasoning_content: data["reasoning_content"].as_str().map(String::from),
                    response_items: items,
                });
            }
            Some("tool") => history.push(Message::Tool {
                tool_call_id: data["tool_call_id"]
                    .as_str()
                    .context("invalid tool id")?
                    .into(),
                content: data["result"]
                    .as_str()
                    .context("invalid tool result")?
                    .into(),
            }),
            _ => {}
        }
        if replay_bytes > 64 * 1024 * 1024 {
            bail!("history exceeds 64 MiB replay limit");
        }
    }
    Ok(history)
}

#[cfg(all(test, target_os = "linux"))]
impl Journal {
    fn for_test(f: File) -> Self {
        Self {
            f,
            replay_dir: PathBuf::new(),
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
