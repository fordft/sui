//! Typed events from core (agent loop, mission driver) to the UI.
//! The UI renders and sends user commands only — it never re-implements
//! the agent loop. Events are append-only observations of real core state.

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

/// Channel the core uses to report to a UI. `None` = headless path.
pub type Sink = UnboundedSender<UiEvent>;

/// What the user chose on a permission modal.
#[derive(Debug, Clone, Copy)]
pub enum GateChoice {
    Once,
    Session,
    Deny,
}

#[derive(Debug)]
pub enum UiEvent {
    /// Streamed assistant text fragment.
    Delta { agent: String, text: String },
    /// A tool call is about to run (post-approval).
    ToolStart { agent: String, name: String, summary: String },
    /// Tool finished (or was denied/skipped).
    ToolDone { agent: String, name: String, ms: u128, ok: bool, result: String },
    /// Provider-reported usage for one request.
    Usage {
        agent: String,
        model: String,
        input: Option<u64>,
        cached: Option<u64>,
        written: Option<u64>,
        output: Option<u64>,
        complete: bool,
    },
    /// A mutating tool needs a decision; reply goes on `reply`.
    Permission {
        id: u64,
        summary: String,
        reply: tokio::sync::mpsc::UnboundedSender<GateChoice>,
    },
    /// Mission state machine transition.
    MissionState(String),
    /// Task-contract status row (from plan / worker results).
    TaskRows(Value),
    /// Files changed on a candidate (+ branch/sha when known).
    ChangeSet { files: Vec<String>, sha: Option<String> },
    /// Auditor verdict payload.
    AuditResult(Value),
    /// The whole run finished.
    RunDone { outcome: String, accepted_sha: Option<String> },
    /// Non-fatal error line (per-agent or driver).
    Error { agent: String, msg: String },
}
