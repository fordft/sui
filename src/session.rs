//! Native session recovery. Journals are evidence; recovery never executes tools.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

const JOURNAL_LIMIT: u64 = 32 * 1024 * 1024;
const SIDECAR_LIMIT: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signature {
    pub provider: String,
    pub system: String,
    pub tools: String,
    pub guidance: Option<String>,
}
impl Signature {
    pub fn current(provider: &crate::provider::Provider, workspace: &Path) -> Self {
        let hashes =
            crate::context::layer_hashes(&crate::tools::schemas(), &crate::context::system());
        Self {
            provider: provider.resume_fingerprint(),
            system: hashes.static_prefix,
            tools: hashes.tool_schema,
            guidance: crate::charter::project_guidance(workspace)
                .map(|g| crate::context::sha256_hex(g.as_bytes())),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Header {
    pub format: u32,
    pub workspace: PathBuf,
    pub profile: Option<String>,
    pub model: String,
    pub session_id: String,
    pub agent_id: String,
    pub signature: Signature,
}

/// An advisory process-lifetime lock. The OS releases it even after a crash.
/// Every native writer holds it; recovery refuses an active writer.
pub struct SessionLock {
    _file: File,
}
impl SessionLock {
    pub fn acquire(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(false).read(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let f = opts.open(dir.join("session.lock"))?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: fd is owned and live, and flock takes no pointers.
            if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                bail!("session is still open in another process; close it before resuming");
            }
        }
        #[cfg(not(unix))]
        bail!("session recovery requires Unix process locks");
        #[cfg(unix)]
        Ok(Self { _file: f })
    }
}

pub fn runs_root() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_default()
        .join(".local/share/sui/runs")
}

pub fn new_run_dir() -> Result<PathBuf> {
    let id = format!(
        "session-{}-{:032x}",
        std::process::id(),
        rand::random::<u128>()
    );
    let dir = runs_root().join(id);
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[derive(Debug, Clone)]
pub struct Summary {
    pub id: String,
    pub label: String,
}

/// Bounded previews, scoped strictly to the current workspace. Older journals
/// remain visible but need the recorded wire signature before they can resume.
pub fn recent(root: &Path, workspace: &Path, exclude: Option<&Path>) -> Result<Vec<Summary>> {
    if !root.exists() {
        return Ok(vec![]);
    }
    let want = workspace.canonicalize()?;
    let mut dirs = Vec::new();
    for (n, entry) in std::fs::read_dir(root)?.enumerate() {
        if n >= 10_000 {
            bail!("session listing exceeds 10000 entries; select an exact run ID");
        }
        let entry = entry?;
        if entry.file_type()?.is_dir() && exclude != Some(entry.path().as_path()) {
            let Some(journal) = native_journal(&entry.path()) else {
                continue;
            };
            let modified = std::fs::metadata(journal)?
                .modified()
                .unwrap_or(std::time::UNIX_EPOCH);
            dirs.push((modified, entry.path()));
        }
    }
    dirs.sort_by_key(|v| std::cmp::Reverse(v.0));
    let mut rows = Vec::new();
    for (_, dir) in dirs {
        let Some(path) = native_journal(&dir) else {
            continue;
        };
        let Ok(events) = preview(&path) else { continue };
        let matches = events.iter().any(|v| {
            v["type"] == crate::journal::ev::SESSION
                && v["data"]["workspace"].as_str().is_some_and(|p| {
                    Path::new(p)
                        .canonicalize()
                        .is_ok_and(|recorded| recorded == want)
                })
        });
        if !matches {
            continue;
        }
        let header = events.iter().find(|v| v["type"] == "resume_header");
        let model = header
            .and_then(|v| v["data"]["model"].as_str())
            .unwrap_or("model unknown");
        let task = events
            .iter()
            .find(|v| v["type"] == "task")
            .and_then(|v| v["data"]["task"].as_str())
            .or_else(|| {
                events
                    .iter()
                    .find(|v| v["type"] == "user")
                    .and_then(|v| v["data"]["content"].as_str())
            })
            .unwrap_or("recorded session");
        let id = dir
            .file_name()
            .context("session ID unavailable")?
            .to_string_lossy()
            .into_owned();
        let title: String = task
            .chars()
            .take(80)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let suffix = if header.is_none() {
            " · older journal (resume unavailable)"
        } else {
            ""
        };
        rows.push(Summary {
            label: format!("{id} · {model} · {title}{suffix}"),
            id,
        });
        if rows.len() == 30 {
            break;
        }
    }
    Ok(rows)
}

fn native_journal(dir: &Path) -> Option<PathBuf> {
    ["solo.jsonl", "headless.jsonl"]
        .into_iter()
        .map(|n| dir.join(n))
        .find(|p| p.is_file())
}

fn open_read(path: &Path) -> Result<File> {
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("session journal is not a regular file");
    }
    Ok(file)
}

fn preview(path: &Path) -> Result<Vec<Value>> {
    let mut reader = BufReader::new(open_read(path)?.take(64 * 1024));
    let mut events = Vec::new();
    for _ in 0..10 {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        if let Ok(v) = serde_json::from_str(&line) {
            events.push(v);
        }
    }
    Ok(events)
}

pub struct SavedSession {
    pub id: String,
    pub header: Header,
    pub history: Vec<crate::types::Message>,
    pub events: Vec<Value>,
    pub next_request: u64,
    pub epoch: u64,
    raw: Vec<u8>,
    sidecars: BTreeMap<String, Vec<u8>>,
    _lock: SessionLock,
}

impl SavedSession {
    pub fn load(root: &Path, id: &str, workspace: &Path) -> Result<Self> {
        let id = if id == "latest" {
            recent(root, workspace, None)?
                .into_iter()
                .next()
                .context("no native sessions for this workspace")?
                .id
        } else {
            id.to_string()
        };
        if id.is_empty()
            || id.len() > 160
            || id.contains("..")
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        {
            bail!("invalid session ID");
        }
        let root = root.canonicalize().context("no recorded sessions")?;
        let dir = root.join(&id).canonicalize().context("session not found")?;
        if dir.parent() != Some(root.as_path()) {
            bail!("session escapes the runs directory");
        }
        let lock = SessionLock::acquire(&dir)?;
        let path = native_journal(&dir).context("only native Solo/headless sessions can resume")?;
        let file = open_read(&path)?;
        if file.metadata()?.len() > JOURNAL_LIMIT {
            bail!("session journal exceeds 32 MiB recovery limit");
        }
        let mut raw = Vec::new();
        file.take(JOURNAL_LIMIT + 1).read_to_end(&mut raw)?;
        if raw.len() as u64 > JOURNAL_LIMIT || !raw.ends_with(b"\n") {
            bail!("session journal is oversized or ends in an incomplete event");
        }
        let events: Vec<Value> = raw[..raw.len() - 1]
            .split(|b| *b == b'\n')
            .map(serde_json::from_slice)
            .collect::<std::result::Result<_, _>>()
            .context("damaged session journal")?;
        if events.len() > 50_000 {
            bail!("session exceeds 50000 recovery events");
        }
        let headers: Vec<_> = events
            .iter()
            .filter(|v| v["type"] == "resume_header")
            .collect();
        if headers.len() != 1 {
            bail!("journal has no unique resume header; older sessions remain exportable");
        }
        let header: Header =
            serde_json::from_value(headers[0]["data"].clone()).context("invalid resume header")?;
        if header.format != 1 {
            bail!("unsupported session format");
        }
        if header.workspace.canonicalize()? != workspace.canonicalize()? {
            bail!("session belongs to another workspace");
        }
        let mut turn_open = false;
        let mut ended = false;
        let mut pending = BTreeSet::new();
        let mut next_request = 0;
        let mut epoch = 0;
        let mut sidecars = BTreeMap::new();
        let mut sidecar_bytes: usize = 0;
        for event in &events {
            let d = &event["data"];
            match event["type"].as_str() {
                Some("turn_start") => {
                    if turn_open {
                        bail!("session has an unfinished turn; old commands will not be replayed");
                    }
                    turn_open = true;
                }
                Some("turn_end") => {
                    if !turn_open {
                        bail!("session has an unmatched turn end");
                    }
                    if !pending.is_empty() {
                        bail!("turn ended with unresolved tool calls");
                    }
                    turn_open = false;
                    ended = true;
                }
                Some("assistant") => {
                    if !pending.is_empty() {
                        bail!("session has unresolved tool calls");
                    }
                    if let Some(calls) = d["tool_calls"].as_array() {
                        for call in calls {
                            let id = call["id"].as_str().context("invalid tool call")?;
                            if !pending.insert(id.to_string()) {
                                bail!("duplicate pending tool call");
                            }
                        }
                    }
                    if let Some(reference) = d.get("response_items_ref").filter(|v| !v.is_null()) {
                        let hash = reference["sha256"]
                            .as_str()
                            .context("missing replay hash")?;
                        if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                            bail!("invalid replay hash");
                        }
                        let name = format!("replay-{hash}.json");
                        if reference["file"].as_str() != Some(&name) {
                            bail!("invalid replay sidecar path");
                        }
                        if let std::collections::btree_map::Entry::Vacant(entry) =
                            sidecars.entry(name)
                        {
                            let bytes = crate::journal::read_replay_file(&dir.join(entry.key()))?;
                            if crate::context::sha256_hex(&bytes) != hash {
                                bail!("opaque replay state failed integrity check");
                            }
                            sidecar_bytes = sidecar_bytes
                                .checked_add(bytes.len())
                                .context("replay state exceeds bounds")?;
                            if sidecar_bytes > SIDECAR_LIMIT {
                                bail!("opaque replay state exceeds 64 MiB recovery limit");
                            }
                            entry.insert(bytes);
                        }
                    }
                }
                Some("tool") => {
                    let id = d["tool_call_id"].as_str().context("invalid tool result")?;
                    if !pending.remove(id) {
                        bail!("unmatched recorded tool result");
                    }
                }
                Some("user" | "runtime_observation" | "context_checkpoint")
                    if !pending.is_empty() =>
                {
                    bail!("session has unresolved tool calls")
                }
                Some("context_checkpoint") => {
                    epoch = d["epoch"].as_u64().context("invalid context epoch")?;
                }
                Some("request") => {
                    let id = d["request_id"].as_u64().context("invalid request ID")?;
                    next_request =
                        next_request.max(id.checked_add(1).context("request ID overflow")?);
                }
                _ => {}
            }
        }
        if turn_open || !ended || !pending.is_empty() {
            bail!("session has an unfinished turn or tool call; inspect/export it instead of rerunning old commands");
        }
        let history = crate::journal::replay_events(&path, events.iter())?;
        if history.is_empty() {
            bail!("session has no recoverable conversation");
        }
        Ok(Self {
            id,
            header,
            history,
            events,
            next_request,
            epoch,
            raw,
            sidecars,
            _lock: lock,
        })
    }

    /// Copy the evidence into a fresh private run. The source remains intact.
    pub fn fork(&self, dir: &Path, journal_name: &str) -> Result<()> {
        let _lock = SessionLock::acquire(dir)?;
        write_private(&dir.join(format!("{journal_name}.jsonl")), &self.raw)?;
        for (name, bytes) in &self.sidecars {
            write_private(&dir.join(name), bytes)?;
        }
        Ok(())
    }
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = opts.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
