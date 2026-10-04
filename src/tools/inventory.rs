//! Read-only code locations. Every call walks current workspace files; no
//! persistent index, repo writes, shell, model call or prompt-prefix mutation.
use anyhow::{bail, Context, Result};
use ignore::WalkBuilder;
use serde::Deserialize;
use serde_json::{json, Value};
use std::fmt::Write as _;
use std::io::Read;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tree_sitter::{Language, Node, ParseOptions, Parser};

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

/// A dropped tool future stops its blocking worker too. Parser progress and
/// traversal check this flag; nothing survives as a background index service.
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
    let task = tokio::task::spawn_blocking(move || scan(root, rel, args, &flag));
    tokio::select! {
        biased;
        _ = cancel => Ok(ExecOut::plain("status: cancelled\nerror: inventory cancelled".into(), ExecKind::Cancelled)),
        result = task => result.context("inventory worker failed")?,
    }
}

fn error(message: &str) -> ExecOut {
    ExecOut::plain(format!("status: error\nerror: {message}"), ExecKind::Error)
}

#[derive(Default)]
struct Scan {
    entries: usize,
    files: usize,
    parsed: usize,
    bytes: usize,
    nodes: usize,
    skipped: usize,
    unsupported: usize,
    syntax_errors: usize,
    matches: usize,
    rows: Vec<String>,
    row_bytes: usize,
    stopped: Option<&'static str>,
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

fn excluded(path: &Path) -> bool {
    path.components().any(|c| {
        let name = c.as_os_str().to_string_lossy();
        matches!(
            name.as_ref(),
            ".git"
                | ".sui"
                | "target"
                | "node_modules"
                | "vendor"
                | "dist"
                | "build"
                | "__pycache__"
                | ".venv"
                | "venv"
                | ".next"
                | "coverage"
        ) || name.starts_with(".env")
            || matches!(name.as_ref(), "auth.json" | "credentials.json")
    }) || path
        .extension()
        .is_some_and(|e| matches!(e.to_str(), Some("pem" | "key" | "p12" | "pfx")))
}

fn scan(root: PathBuf, scope: PathBuf, args: Args, cancel: &AtomicBool) -> Result<ExecOut> {
    let deadline = Instant::now() + SCAN_TIME;
    let filter_root = root.clone();
    let filter_scope = scope.clone();
    let mut walk = WalkBuilder::new(&root);
    walk.hidden(false)
        .parents(false)
        .git_global(false)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_path(|a, b| a.cmp(b))
        .filter_entry(move |entry| {
            let Ok(rel) = entry.path().strip_prefix(&filter_root) else {
                return false;
            };
            !excluded(rel) && (rel.starts_with(&filter_scope) || filter_scope.starts_with(rel))
        });
    let query = args.query.to_lowercase();
    let mut stats = Scan::default();
    let mut parser = Parser::new();
    for entry in walk.build() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(ExecOut::plain(
                "status: cancelled\nerror: inventory cancelled".into(),
                ExecKind::Cancelled,
            ));
        }
        if stats.entries >= MAX_ENTRIES || Instant::now() >= deadline {
            stats.stopped = Some(if stats.entries >= MAX_ENTRIES {
                "entry_limit"
            } else {
                "time_limit"
            });
            break;
        }
        stats.entries += 1;
        let entry = match entry {
            Ok(v) if v.error().is_none() => v,
            _ => {
                stats.skipped += 1;
                continue;
            }
        };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let rel = path.strip_prefix(&root)?;
        // Re-use the native path guard before reading; the walker never follows links.
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
        let text = match read_source(path) {
            Ok(v) => v,
            Err(_) => {
                stats.skipped += 1;
                continue;
            }
        };
        if stats.bytes + text.len() > MAX_SCAN_BYTES {
            stats.stopped = Some("byte_limit");
            break;
        }
        stats.bytes += text.len();
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
            if let Some((kind, name)) = definition(node, text.as_bytes()) {
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
        "status: success\naction: {}\npath: {}\nquery: {}\nsupported_symbols: Rust, JS/JSX, TS/TSX, Python, Go\nscan_complete: {}\nentries_scanned: {}\nfiles_scanned: {}\nfiles_parsed: {}\nfiles_skipped: {}\nfiles_unsupported: {}\nsyntax_error_files: {}\nmatches_seen: {}\nshowing: {}\ntruncated: {truncated}\nstop_reason: {}\ncontent:\n",
        if matches!(args.action, Action::Files) { "files" } else { "symbols" }, escaped(&args.path), escaped(&args.query), stats.stopped.is_none() && stats.skipped == 0 && stats.syntax_errors == 0, stats.entries, stats.files, stats.parsed, stats.skipped, stats.unsupported, stats.syntax_errors, stats.matches, stats.rows.len(), stats.stopped.unwrap_or("none")
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
    if stats.unsupported > 0 {
        text.push_str(
            "hint: symbols cover only supported languages; use files/bash for unsupported files\n",
        );
    }
    let mut out = ExecOut::plain(text, ExecKind::Success);
    out.truncated = truncated;
    Ok(out)
}

fn read_source(path: &Path) -> Result<String> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("not a regular file");
    }
    let mut bytes = Vec::new();
    file.take((MAX_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
        bail!("oversize or binary source");
    }
    Ok(String::from_utf8(bytes)?)
}

fn language_name(path: &Path) -> &'static str {
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

fn language(path: &Path) -> Option<Language> {
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

fn definition<'a>(node: Node<'_>, source: &'a [u8]) -> Option<(&'static str, &'a str)> {
    let kind = match node.kind() {
        "function_item"
        | "function_signature"
        | "function_declaration"
        | "function_definition"
        | "generator_function_declaration" => "function",
        "method_definition"
        | "method_declaration"
        | "method_signature"
        | "abstract_method_signature" => "method",
        "struct_item" => "struct",
        "union_item" => "union",
        "enum_item" | "enum_declaration" => "enum",
        "trait_item" | "interface_declaration" => "interface",
        "class_definition" | "class_declaration" | "abstract_class_declaration" => "class",
        "type_item" | "type_spec" | "type_alias_declaration" => "type",
        "mod_item" | "module" | "internal_module" => "module",
        "macro_definition" => "macro",
        "const_item" | "const_spec" | "static_item" => "constant",
        "variable_declarator"
            if node.child_by_field_name("value").is_some_and(|v| {
                matches!(
                    v.kind(),
                    "arrow_function" | "function_expression" | "generator_function"
                )
            }) =>
        {
            "function"
        }
        _ => return None,
    };
    let name = node.child_by_field_name("name")?;
    if name.is_missing()
        || !matches!(
            name.kind(),
            "identifier" | "type_identifier" | "property_identifier" | "field_identifier"
        )
    {
        return None;
    }
    let text = name.utf8_text(source).ok()?;
    (text.len() <= 256).then_some((kind, text))
}

fn escaped(text: &str) -> String {
    if text.chars().any(char::is_control) {
        serde_json::to_string(text).expect("string serialization")
    } else {
        text.to_string()
    }
}
