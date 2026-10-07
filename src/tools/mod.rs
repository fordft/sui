pub mod bash;
pub mod code_context;
pub mod code_intel;
pub mod fs;
pub mod inventory;
pub mod patch;
pub mod session_info;
pub mod tool_output;
pub mod ui;

use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

pub struct ToolContext {
    pub workspace: PathBuf,
    pub bash_timeout: Duration,
    pub bash_timeout_max: Duration,
    /// Per-run web research service (None = tools report "not configured").
    pub web: Option<std::sync::Arc<crate::web::WebService>>,
    /// Lazily canonicalized workspace root — fs::resolve() realpath()s
    /// the root once per context instead of once per tool call.
    pub canon_root: std::sync::OnceLock<PathBuf>,
    pub ui: std::sync::OnceLock<ui::UiService>,
    /// Lazy language server, owned by this agent/worktree only.
    pub code_intel: std::sync::OnceLock<code_intel::CodeIntelService>,
    /// Bounded syntax facts, owned by this agent/worktree; fresh source is
    /// checked on every context query.
    pub code_context: std::sync::OnceLock<std::sync::Arc<code_context::CodeContextService>>,
    /// Ephemeral originals of compacted command results, never shared or journaled.
    pub tool_outputs: std::sync::OnceLock<tool_output::OutputStore>,
}

/// Frozen tool schemas. Changing names/descriptions/order invalidates
/// provider prompt caches — treat as a versioned interface.
pub fn schemas() -> Vec<Value> {
    let mut schemas = vec![
        json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file with line numbers. Returns at most `limit` lines starting at `offset` (1-based).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":   { "type": "string", "description": "Path relative to workspace root" },
                        "offset": { "type": "integer", "description": "1-based line to start at (default 1)" },
                        "limit":  { "type": "integer", "description": "Max lines to return (default 100, max 400)" }
                    },
                    "required": ["path"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Create or fully replace a file. For partial edits prefer edit_file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":    { "type": "string", "description": "Path relative to workspace root" },
                        "content": { "type": "string", "description": "Complete new file contents" }
                    },
                    "required": ["path", "content"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "edit_file",
                "description": "Replace an exact unique substring. Fails if old_str matches zero or more than one location.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path":    { "type": "string", "description": "Path relative to workspace root" },
                        "old_str": { "type": "string", "description": "Exact text to replace; must match exactly once" },
                        "new_str": { "type": "string", "description": "Replacement text" }
                    },
                    "required": ["path", "old_str", "new_str"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "web_search",
                "description": "Search the web for current documentation and sources. Returns source IDs, titles, URLs, and snippets — snippets are not fetched content. Use web_fetch to read a source. Results leave this machine.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query":       { "type": "string", "description": "Search query — prefer the exact version/topic, e.g. 'ratatui 0.29 paragraph scroll'" },
                        "max_results": { "type": "integer", "description": "Results to return (default 5, max 10)" }
                    },
                    "required": ["query"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "skill",
                "description": "Load an engineering guide by name from the lens list in the system prompt. Guides inform judgment; they add no requirements.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "name": { "type": "string", "description": "Lens name from the system prompt index" }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "web_fetch",
                "description": "Fetch a webpage's content as markdown (public http(s) URLs only). Returns the page text plus retrieval timestamp; may be truncated.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "url": { "type": "string", "description": "Public http(s) URL to read" }
                    },
                    "required": ["url"]
                }
            }
        }),
        json!({
            "type": "function",
            "function": {
                "name": "bash",
                "description": "Run a shell command in the workspace. Prefer rg for search. Output is bounded head+tail. Successful Cargo progress/pass records and Git diff --stat graphs may be compacted with a read_tool_output handle; output=raw preserves the captured text.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command":    { "type": "string", "description": "Shell command (bash -c)" },
                        "timeout_ms": { "type": "integer", "description": "Wall-clock timeout in ms (default 120000)" },
                        "output": { "type": "string", "enum": ["auto", "raw"], "description": "auto (default): compact recognized successful Cargo or Git diff --stat output; raw: return original bounded capture" }
                    },
                    "required": ["command"]
                }
            }
        }),
    ];
    schemas.extend(ui::schemas());
    // Append new names to preserve existing order. Deliberate schema changes
    // require a new session signature.
    schemas.push(inventory::schema());
    schemas.push(code_intel::schema());
    schemas.push(code_context::schema());
    schemas.push(tool_output::schema());
    schemas.push(patch::schema());
    schemas.push(session_info::schema());
    schemas
}

/// How a tool execution ended — typed at the source. `text` is the
/// model-facing envelope; `kind`/`exit`/`truncated` are facts callers may
/// rely on without re-parsing the envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecKind {
    Success,
    /// Ran and exited nonzero.
    Failed,
    /// Timeout / cancellation / runtime or argument error.
    Error,
    Timeout,
    Cancelled,
}

/// One tool call's outcome: model-facing envelope + typed status.
pub struct ExecOut {
    pub text: String,
    pub kind: ExecKind,
    pub exit: Option<i32>,
    /// Observation bytes were omitted by capture limits, incomplete pipe
    /// capture or a paged view. Compaction alone does not set this flag.
    pub truncated: bool,
    /// Live-preview chunks dropped because the UI tap was full.
    pub preview_dropped: u64,
    /// In-memory image observation; never placed in journal/tool text.
    pub image: Option<crate::types::UserContent>,
}
impl ExecOut {
    pub fn plain(text: String, kind: ExecKind) -> Self {
        Self {
            text,
            kind,
            exit: None,
            truncated: false,
            preview_dropped: 0,
            image: None,
        }
    }
}

/// Execute one tool call with already-validated arguments.
/// Callers must JSON-parse arguments first; malformed args never reach here.
/// `cancel` aborts in-flight execution (e.g. Ctrl-C) with the same
/// kill-and-reap cleanup path as a timeout. `obs` is a bounded live-output
/// tap — only bash produces chunks; fs tools finish atomically.
pub async fn execute(
    ctx: &ToolContext,
    name: &str,
    args: &Value,
    cancel: impl std::future::Future<Output = ()>,
    obs: Option<bash::Observer>,
) -> Result<ExecOut> {
    // Disk mutations invalidate the language process before execution. Failed
    // commands may also have changed files; never reuse their old observations.
    if matches!(name, "write_file" | "edit_file" | "bash")
        || (name == "patch_files" && args["action"] == "apply")
        || (name == "terminal"
            && matches!(args["action"].as_str(), Some("start" | "type" | "press")))
    {
        if let Some(service) = ctx.code_intel.get() {
            service.invalidate().await;
        }
    }
    match name {
        "read_file" | "write_file" | "edit_file" => {
            let _ = (cancel, obs);
            let text = match name {
                "read_file" => fs::read_file(ctx, args)?,
                "write_file" => fs::write_file(ctx, args)?,
                _ => fs::edit_file(ctx, args)?,
            };
            let kind = if text.starts_with("status: success") {
                ExecKind::Success
            } else {
                ExecKind::Error
            };
            Ok(ExecOut::plain(text, kind))
        }
        "patch_files" => {
            let _ = (cancel, obs);
            let text = patch::execute(ctx, args);
            let kind = if text.starts_with("status: success") {
                ExecKind::Success
            } else {
                ExecKind::Error
            };
            Ok(ExecOut::plain(text, kind))
        }
        "bash" => bash::run(ctx, args, cancel, obs).await,
        "inventory" => inventory::execute(ctx, args, cancel).await,
        "code_intel" => code_intel::execute(ctx, args, cancel).await,
        "code_context" => code_context::execute(ctx, args, cancel).await,
        "read_tool_output" => tool_output::execute(ctx, args, cancel).await,
        "browser" | "terminal" => ui::service(ctx)?.execute(ctx, name, args, cancel).await,
        "view_image" => ui::view_image(ctx, args).await,
        "skill" => {
            let _ = (cancel, obs);
            let n = args["name"].as_str().unwrap_or("").trim();
            match crate::skills::get(n) {
                Some(s) => Ok(ExecOut::plain(crate::skills::render(s), ExecKind::Success)),
                None => Ok(ExecOut::plain(
                    format!(
                        "status: error\nerror: unknown lens '{n}' — available: {}",
                        crate::skills::names().join(", ")
                    ),
                    ExecKind::Error,
                )),
            }
        }
        "web_search" | "web_fetch" => {
            let _ = (cancel, obs);
            match &ctx.web {
                Some(svc) => Ok(svc.exec(name, args).await),
                None => Ok(ExecOut::plain(
                    "status: error\nerror: web research is not configured".into(),
                    ExecKind::Error,
                )),
            }
        }
        other => {
            let _ = (cancel, obs);
            Ok(ExecOut::plain(
                format!("status: error\nerror: unknown tool '{other}'"),
                ExecKind::Error,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn skill_tool_loads_and_rejects_unknown() {
        let ctx = super::ToolContext {
            workspace: std::env::temp_dir(),
            bash_timeout: std::time::Duration::from_secs(1),
            bash_timeout_max: std::time::Duration::from_secs(2),
            web: None,
            canon_root: std::sync::OnceLock::new(),
            ui: std::sync::OnceLock::new(),
            code_intel: Default::default(),
            code_context: Default::default(),
            tool_outputs: Default::default(),
        };
        let ok = super::execute(
            &ctx,
            "skill",
            &serde_json::json!({"name": "debugging"}),
            std::future::pending(),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(ok.kind, super::ExecKind::Success));
        assert!(ok.text.contains("Reproduce before diagnosing"));

        let bad = super::execute(
            &ctx,
            "skill",
            &serde_json::json!({"name": "alchemy"}),
            std::future::pending(),
            None,
        )
        .await
        .unwrap();
        assert!(matches!(bad.kind, super::ExecKind::Error));
        assert!(bad.text.contains("available"));
    }
}
