pub mod bash;
pub mod fs;

use anyhow::Result;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

pub struct ToolContext {
    pub workspace: PathBuf,
    pub bash_timeout: Duration,
    pub bash_timeout_max: Duration,
}

/// Frozen tool schemas. Changing names/descriptions/order invalidates
/// provider prompt caches — treat as a versioned interface.
pub fn schemas() -> Vec<Value> {
    vec![
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
                "name": "bash",
                "description": "Run a shell command in the workspace. Prefer rg for search. Output is bounded; long output is truncated head+tail.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command":    { "type": "string", "description": "Shell command (bash -c)" },
                        "timeout_ms": { "type": "integer", "description": "Wall-clock timeout in ms (default 120000)" }
                    },
                    "required": ["command"]
                }
            }
        }),
    ]
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
    /// The captured result itself was truncated at the capture cap.
    pub truncated: bool,
    /// Live-preview chunks dropped because the UI tap was full.
    pub preview_dropped: u64,
}
impl ExecOut {
    pub fn plain(text: String, kind: ExecKind) -> Self {
        Self {
            text,
            kind,
            exit: None,
            truncated: false,
            preview_dropped: 0,
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
        "bash" => bash::run(ctx, args, cancel, obs).await,
        other => {
            let _ = (cancel, obs);
            Ok(ExecOut::plain(
                format!("status: error\nerror: unknown tool '{other}'"),
                ExecKind::Error,
            ))
        }
    }
}
