//! Read-only code locations. Every call walks current workspace files; no
//! persistent index, repo writes, shell, model call or prompt-prefix mutation.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tree_sitter::{Language, ParseOptions, Parser};

mod symbols;
mod walk;

use super::{ExecKind, ExecOut, ToolContext};

const MAX_ENTRIES: usize = 10_000;
const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_SCAN_BYTES: usize = 32 * 1024 * 1024;
const MAX_NODES: usize = 500_000;
const MAX_OUTPUT: usize = 24 * 1024;
const SCAN_TIME: Duration = Duration::from_secs(3);

pub fn schema() -> Value {
    json!({
        "type": "function",
        "function": {
            "name": "inventory",
            "description": "Locate workspace files or syntax-based definitions with file:line positions. Read-only; respects ignore files, skips generated directories and symlinks. Symbols support Rust, JS/JSX, TS/TSX, Python and Go; not semantic references or a call graph. Read the relevant region with read_file before editing.",
            "parameters": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["files", "symbols"] },
                    "query": { "type": "string", "description": "Literal case-insensitive substring of file path or symbol name (default empty)" },
                    "path": { "type": "string", "description": "Workspace-relative file or directory to search (default .)" },
                    "limit": { "type": "integer", "description": "Max results (default 50, range 1-200)", "minimum": 1, "maximum": 200 }
                },
                "required": ["action"],
                "additionalProperties": false
            }
        }
    })
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Files,
    Symbols,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: Action,
    #[serde(default)]
    query: String,
    #[serde(default = "default_path")]
    path: String,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_path() -> String {
    ".".into()
}
fn default_limit() -> usize {
    50
}

/// Dropping the tool signals cooperative cancellation. An explicit cancel
/// also joins the worker; regular-file reads and traversal check this flag.
struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub async fn execute(
    ctx: &ToolContext,
    args: &Value,
    cancel: impl std::future::Future<Output = ()>,
) -> Result<ExecOut> {
    let args: Args = match serde_json::from_value(args.clone()) {
        Ok(v) => v,
        Err(_) => return Ok(error("invalid inventory arguments; use action files or symbols, string query/path and integer limit")),
    };
    if args.query.len() > 256 || args.path.len() > 4096 || !(1..=200).contains(&args.limit) {
        return Ok(error(
            "query must be <=256 bytes, path <=4096 bytes and limit 1-200",
        ));
    }
    let root = ctx.workspace.canonicalize()?;
    let scope = super::fs::resolve_ctx(ctx, &args.path)?;
    let rel = scope.strip_prefix(&root)?.to_path_buf();
    if !scope.exists() {
        return Ok(error("inventory path does not exist"));
    }
    let mut probe = root.clone();
    for component in rel.components() {
        probe.push(component);
        if probe.symlink_metadata()?.file_type().is_symlink() {
            return Ok(error("inventory does not follow symlinks"));
        }
    }
    if matches!(args.action, Action::Symbols) && scope.is_file() && language(&scope).is_none() {
        return Ok(error(
            "unsupported symbol language; use files/read_file or bash search",
        ));
    }
    let stop = Stop(Arc::new(AtomicBool::new(false)));
    let flag = stop.0.clone();
    let mut task = tokio::task::spawn_blocking(move || scan(root, rel, args, &flag));
    tokio::select! {
        biased;
        _ = cancel => {
            stop.0.store(true, Ordering::Relaxed);
            let _ = task.await; // Cooperatively stop AND reap the blocking work.
            Ok(cancelled())
        },
        result = &mut task => result.context("inventory worker failed")?,
    }
}

fn error(message: &str) -> ExecOut {
    ExecOut::plain(format!("status: error\nerror: {message}"), ExecKind::Error)
}

pub(crate) struct Scan {
    entry_limit: usize,
    byte_limit: usize,
    pub(crate) ignore_errors: usize,
    pub(crate) entries: usize,
    pub(crate) files: usize,
    pub(crate) parsed: usize,
    pub(crate) bytes: usize,
    pub(crate) nodes: usize,
    pub(crate) skipped: usize,
    pub(crate) unsupported: usize,
    pub(crate) syntax_errors: usize,
    matches: usize,
    rows: Vec<String>,
    row_bytes: usize,
    pub(crate) stopped: Option<&'static str>,
}
impl Default for Scan {
    fn default() -> Self {
        Self {
            entry_limit: MAX_ENTRIES,
            byte_limit: MAX_SCAN_BYTES,
            ignore_errors: 0,
            entries: 0,
            files: 0,
            parsed: 0,
            bytes: 0,
            nodes: 0,
            skipped: 0,
            unsupported: 0,
            syntax_errors: 0,
            matches: 0,
            rows: Vec::new(),
            row_bytes: 0,
            stopped: None,
        }
    }
}
impl Scan {
    fn add(&mut self, row: String, limit: usize) {
        self.matches += 1;
        if self.rows.len() < limit && self.row_bytes + row.len() < MAX_OUTPUT {
            self.row_bytes += row.len() + 1;
            self.rows.push(row);
        }
    }
}

pub(crate) fn excluded(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            ".git"
                | ".sui"
                | "node_modules"
                | "vendor"
                | "__pycache__"
                | ".venv"
                | "venv"
                | ".next"
        ) || name.starts_with(".env")
            || matches!(name.as_ref(), "auth.json" | "credentials.json")
    }) || path
        .extension()
        .is_some_and(|e| matches!(e.to_str(), Some("pem" | "key" | "p12" | "pfx")))
}

fn scan(root: PathBuf, scope: PathBuf, args: Args, cancel: &AtomicBool) -> Result<ExecOut> {
    let deadline = Instant::now() + SCAN_TIME;
    let query = args.query.to_lowercase();
    let mut stats = Scan::default();
    let files = walk::collect(&root, &scope, &mut stats, cancel, deadline);
    let traversal_stop = stats.stopped.take();
    let mut parser = Parser::new();
    for path in files {
        if !checkpoint(&mut stats, cancel, deadline) {
            break;
        }
        let path = path.as_path();
        let rel = path.strip_prefix(&root)?;
        let Some(display) = rel.to_str() else {
            stats.skipped += 1;
            continue;
        };
        if super::fs::resolve(&root, display).is_err() {
            stats.skipped += 1;
            continue;
        }
        stats.files += 1;
        if matches!(args.action, Action::Files) {
            if display.to_lowercase().contains(&query) {
                stats.add(
                    format!("{} [{}]", escaped(display), language_name(path)),
                    args.limit,
                );
            }
            continue;
        }
        let Some(language) = language(path) else {
            stats.unsupported += 1;
            continue;
        };
        let text = match read_source(&root, path, &mut stats, cancel, deadline) {
            Ok(v) => v,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        parser.set_language(&language)?;
        let mut progress = |_: &tree_sitter::ParseState| {
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                ControlFlow::Break(())
            } else {
                ControlFlow::Continue(())
            }
        };
        let tree = parser.parse_with_options(
            &mut |offset, _| &text.as_bytes()[offset..],
            None,
            Some(ParseOptions::new().progress_callback(&mut progress)),
        );
        let Some(tree) = tree else {
            stats.stopped = Some("time_limit");
            break;
        };
        stats.parsed += 1;
        if tree.root_node().has_error() {
            stats.syntax_errors += 1;
        }
        let mut cursor = tree.walk();
        loop {
            if stats.nodes >= MAX_NODES
                || Instant::now() >= deadline
                || cancel.load(Ordering::Relaxed)
            {
                stats.stopped = Some(if stats.nodes >= MAX_NODES {
                    "node_limit"
                } else {
                    "time_limit"
                });
                break;
            }
            stats.nodes += 1;
            let node = cursor.node();
            if let Some((kind, name)) = symbols::definition(node, text.as_bytes()) {
                if name.to_lowercase().contains(&query) || display.to_lowercase().contains(&query) {
                    stats.add(
                        format!(
                            "{}:{}-{} {kind} {}",
                            escaped(display),
                            node.start_position().row + 1,
                            node.end_position().row + 1,
                            escaped(name)
                        ),
                        args.limit,
                    );
                }
            }
            if cursor.goto_first_child() {
                continue;
            }
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    break;
                }
            }
            if cursor.node() == tree.root_node() {
                break;
            }
        }
        if stats.stopped.is_some() {
            break;
        }
    }
    if stats.stopped.is_none() {
        stats.stopped = traversal_stop;
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(ExecOut::plain(
            "status: cancelled\nerror: inventory cancelled".into(),
            ExecKind::Cancelled,
        ));
    }
    let truncated = stats.matches > stats.rows.len()
        || stats.stopped.is_some()
        || stats.skipped > 0
        || stats.syntax_errors > 0;
    let mut text = format!(
        "status: success\naction: {}\npath: {}\nquery: {}\nsupported_symbols: Rust, JS/JSX, TS/TSX, Python, Go\nscan_complete: {}\nentries_scanned: {}\nfiles_scanned: {}\nfiles_parsed: {}\nbytes_read: {}\nfiles_skipped: {}\nfiles_unsupported: {}\nsyntax_error_files: {}\nmatches_seen: {}\nshowing: {}\ntruncated: {truncated}\nstop_reason: {}\ncontent:\n",
        if matches!(args.action, Action::Files) { "files" } else { "symbols" }, escaped(&args.path), escaped(&args.query), stats.stopped.is_none() && stats.skipped == 0 && stats.syntax_errors == 0, stats.entries, stats.files, stats.parsed, stats.bytes, stats.skipped, stats.unsupported, stats.syntax_errors, stats.matches, stats.rows.len(), stats.stopped.unwrap_or("none")
    );
    if stats.rows.is_empty() {
        text.push_str("<empty>\n");
    } else {
        for row in stats.rows {
            let _ = writeln!(text, "{row}");
        }
    }
    if truncated {
        text.push_str(
            "hint: narrow path/query or use read_file; partial results do not prove absence\n",
        );
    }
    if stats.ignore_errors > 0 {
        text.push_str("hint: ignore rules unavailable; affected subtree skipped\n");
    }
    if stats.unsupported > 0 {
        text.push_str(
            "hint: symbols cover only supported languages; use files/bash for unsupported files\n",
        );
    }
    let mut out = ExecOut::plain(text, ExecKind::Success);
    out.truncated = truncated;
    Ok(out)
}

fn cancelled() -> ExecOut {
    ExecOut::plain(
        "status: cancelled\nerror: inventory cancelled".into(),
        ExecKind::Cancelled,
    )
}

pub(crate) fn checkpoint(stats: &mut Scan, cancel: &AtomicBool, deadline: Instant) -> bool {
    if cancel.load(Ordering::Relaxed) {
        return false;
    }
    if Instant::now() >= deadline {
        stats.stopped = Some("time_limit");
        return false;
    }
    !matches!(
        stats.stopped,
        Some("time_limit" | "byte_limit" | "node_limit")
    )
}

/// Probe each component without following symlinks, including control-file
/// ancestors. This remains a native path guard, not an OS filesystem sandbox.
fn guarded_metadata(
    root: &Path,
    path: &Path,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<Option<std::fs::Metadata>> {
    let rel = path.strip_prefix(root)?;
    let mut probe = root.to_path_buf();
    let mut components = rel.components().peekable();
    while let Some(component) = components.next() {
        anyhow::ensure!(checkpoint(stats, cancel, deadline), "scan stopped");
        probe.push(component);
        let metadata = match probe.symlink_metadata() {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        anyhow::ensure!(!metadata.file_type().is_symlink(), "symlink excluded");
        if components.peek().is_none() {
            return Ok(Some(metadata));
        }
        anyhow::ensure!(metadata.is_dir(), "non-directory ancestor");
    }
    Ok(Some(root.symlink_metadata()?))
}

fn read_regular(
    root: &Path,
    path: &Path,
    per_file_limit: usize,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let metadata =
        guarded_metadata(root, path, stats, cancel, deadline)?.context("file disappeared")?;
    anyhow::ensure!(metadata.is_file(), "not a regular file");
    anyhow::ensure!(metadata.len() <= per_file_limit as u64, "oversize file");
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    anyhow::ensure!(checkpoint(stats, cancel, deadline), "scan stopped");
    let mut file = options.open(path)?;
    let metadata = file.metadata()?;
    anyhow::ensure!(metadata.is_file(), "not a regular file");
    anyhow::ensure!(metadata.len() <= per_file_limit as u64, "oversize file");
    if metadata.len() > stats.byte_limit.saturating_sub(stats.bytes) as u64 {
        stats.stopped = Some("byte_limit");
        bail!("input budget exhausted");
    }
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        anyhow::ensure!(checkpoint(stats, cancel, deadline), "scan stopped");
        let remaining = stats.byte_limit.saturating_sub(stats.bytes);
        if remaining == 0 {
            if file.metadata()?.len() == bytes.len() as u64 {
                break;
            }
            stats.stopped = Some("byte_limit");
            bail!("input budget exhausted");
        }
        let capacity = buffer
            .len()
            .min(remaining)
            .min(per_file_limit + 1 - bytes.len());
        let count = file.read(&mut buffer[..capacity])?;
        stats.bytes += count; // Charge actual bytes even if validation fails.
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        anyhow::ensure!(bytes.len() <= per_file_limit, "oversize file");
    }
    Ok(bytes)
}

pub(crate) fn read_source(
    root: &Path,
    path: &Path,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<String> {
    let bytes = read_regular(root, path, MAX_FILE_BYTES, stats, cancel, deadline)?;
    anyhow::ensure!(!bytes.contains(&0), "binary source");
    Ok(String::from_utf8(bytes)?)
}

/// Shared native guard for bounded language-analysis inputs. Charge even
/// rejected bytes so callers cannot bypass their aggregate input budget.
pub(crate) fn read_for_analysis(
    root: &Path,
    path: &Path,
    byte_budget: usize,
    cancel: &AtomicBool,
    deadline: Instant,
) -> (Result<String>, usize) {
    let mut stats = Scan {
        byte_limit: byte_budget,
        ..Scan::default()
    };
    let result = read_source(root, path, &mut stats, cancel, deadline);
    (result, stats.bytes)
}

/// Reuse inventory traversal without caching ignore decisions or file contents.
pub(crate) fn collect_for_analysis(
    root: &Path,
    scope: &Path,
    stats: &mut Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Vec<PathBuf> {
    walk::collect(root, scope, stats, cancel, deadline)
}

pub(crate) fn syntax_definition<'a>(
    node: tree_sitter::Node<'_>,
    source: &'a [u8],
) -> Option<(&'static str, &'a str)> {
    symbols::definition(node, source)
}

pub(crate) fn language_name(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()) {
        Some("rs") => "rust",
        Some("js" | "jsx" | "mjs" | "cjs") => "javascript",
        Some("ts" | "mts" | "cts") => "typescript",
        Some("tsx") => "tsx",
        Some("py" | "pyi") => "python",
        Some("go") => "go",
        _ => "file",
    }
}

pub(crate) fn language(path: &Path) -> Option<Language> {
    Some(match language_name(path) {
        "rust" => tree_sitter_rust::LANGUAGE.into(),
        "javascript" => tree_sitter_javascript::LANGUAGE.into(),
        "typescript" => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        "tsx" => tree_sitter_typescript::LANGUAGE_TSX.into(),
        "python" => tree_sitter_python::LANGUAGE.into(),
        "go" => tree_sitter_go::LANGUAGE.into(),
        _ => return None,
    })
}

fn escaped(text: &str) -> String {
    if text.chars().any(char::is_control) {
        serde_json::to_string(text).expect("string serialization")
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "sui-inventory-budget-{}-{:x}",
                std::process::id(),
                rand::random::<u128>()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn write(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn ignored_entries_consume_the_enumeration_budget_before_sorting() {
        let fixture = Fixture::new();
        fixture.write(".gitignore", b"*.tmp\n");
        for index in 0..20 {
            fixture.write(&format!("{index:02}.tmp"), b"");
        }
        let mut stats = Scan {
            entry_limit: 4,
            ..Scan::default()
        };
        let files = walk::collect(
            &fixture.0,
            Path::new(""),
            &mut stats,
            &AtomicBool::new(false),
            Instant::now() + SCAN_TIME,
        );
        assert_eq!(stats.entries, 4);
        assert_eq!(stats.stopped, Some("entry_limit"));
        assert!(files
            .iter()
            .all(|p| p.extension().is_none_or(|e| e != "tmp")));
        assert!(files.len() < stats.entries);
    }

    #[test]
    fn rejected_source_bytes_reduce_the_remaining_read_allowance() {
        let fixture = Fixture::new();
        let invalid = fixture.write("invalid.rs", b"\0xx");
        let valid = fixture.write("valid.rs", b"fn ok() {}\n");
        let mut stats = Scan {
            byte_limit: 8,
            ..Scan::default()
        };
        let cancel = AtomicBool::new(false);
        let deadline = Instant::now() + SCAN_TIME;
        assert!(read_source(&fixture.0, &invalid, &mut stats, &cancel, deadline).is_err());
        assert_eq!(stats.bytes, 3);
        assert!(read_source(&fixture.0, &valid, &mut stats, &cancel, deadline).is_err());
        assert_eq!(
            stats.bytes, 3,
            "the next file must be rejected BEFORE reading"
        );
        assert_eq!(stats.stopped, Some("byte_limit"));
    }

    #[test]
    fn exact_byte_allowance_can_finish_a_regular_file() {
        let fixture = Fixture::new();
        let path = fixture.write("last.rs", b"// last\n");
        let mut stats = Scan {
            byte_limit: 8,
            ..Scan::default()
        };
        let bytes = read_regular(
            &fixture.0,
            &path,
            MAX_FILE_BYTES,
            &mut stats,
            &AtomicBool::new(false),
            Instant::now() + SCAN_TIME,
        )
        .unwrap();
        assert_eq!(bytes, b"// last\n");
        assert_eq!(stats.bytes, 8);
        assert!(stats.stopped.is_none());
    }

    #[test]
    fn control_reads_share_the_source_byte_allowance_and_fail_closed() {
        let fixture = Fixture::new();
        fixture.write(".ignore", b"#1234\n");
        fixture.write(".gitignore", b"#abc\n");
        fixture.write("visible.rs", b"fn visible() {}\n");
        let mut stats = Scan {
            byte_limit: 8,
            ..Scan::default()
        };
        let files = walk::collect(
            &fixture.0,
            Path::new(""),
            &mut stats,
            &AtomicBool::new(false),
            Instant::now() + SCAN_TIME,
        );
        assert!(
            files.is_empty(),
            "unavailable rules must prune their subtree"
        );
        assert_eq!(stats.bytes, 6);
        assert_eq!(stats.stopped, Some("byte_limit"));
        assert_eq!(stats.ignore_errors, 1);
    }
}
