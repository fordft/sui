//! Typed events from core (agent loop, mission driver) to the UI.
//! The UI renders and sends user commands only — it never re-implements
//! the agent loop. Events are append-only observations of real core state.
//!
//! Identity: every lifecycle event carries `run` (one per submitted task,
//! assigned by the UI), `agent` (agent id), and where relevant `req`
//! (per-agent request sequence) and `call` (provider tool-call id). The UI
//! updates activity by ID — never by scanning backward for a name match.

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

/// Channel the core uses to report to a UI. `None` = headless path.
pub type Sink = UnboundedSender<UiEvent>;

/// What the user chose on a permission modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateChoice {
    Once,
    Session,
    Deny,
}

/// Terminal status of one tool call — typed at the source so the UI never
/// infers success from envelope strings ("failed"/"denied"/"timeout" are
/// NOT `Ok`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    /// Executed; exit 0 (or fs success).
    Ok,
    /// Executed; nonzero exit.
    Failed,
    /// User denied the gate.
    Denied,
    /// Wall-clock limit hit; tree killed.
    Timeout,
    /// Cancelled by the user (Stop).
    Cancelled,
    /// Runtime/argument error — the call did not produce real work.
    Error,
    /// Not executed: sibling invalid / turn ended by submission.
    Skipped,
    /// Handled by the control plane (e.g. submit_result) — no side effect.
    Intercepted,
}
impl ToolStatus {
    pub fn ok(self) -> bool {
        matches!(self, ToolStatus::Ok)
    }
    pub fn label(self) -> &'static str {
        match self {
            ToolStatus::Ok => "ok",
            ToolStatus::Failed => "failed",
            ToolStatus::Denied => "denied",
            ToolStatus::Timeout => "timeout",
            ToolStatus::Cancelled => "cancelled",
            ToolStatus::Error => "error",
            ToolStatus::Skipped => "skipped",
            ToolStatus::Intercepted => "intercepted",
        }
    }
}

#[derive(Debug)]
pub enum UiEvent {
    /// A model request started (`req` is per-agent monotonic).
    ReqStart { run: u64, agent: String, req: u64 },
    /// Streamed assistant text fragment for request `req`.
    Delta {
        run: u64,
        agent: String,
        req: u64,
        text: String,
    },
    /// Provider-exposed reasoning fragment — displayable text only.
    /// Opaque/signed replay data is never emitted here.
    Reason {
        run: u64,
        agent: String,
        req: u64,
        text: String,
    },
    /// Request finished: assistant message complete (content and/or
    /// tool_calls). `reasoning` = the provider streamed reasoning text.
    ReqDone {
        run: u64,
        agent: String,
        req: u64,
        ms: u128,
        ok: bool,
        reasoning: bool,
    },
    /// A tool call is about to run (post-approval). `call` is the
    /// provider's tool_call id (positional fallback when absent).
    ToolStart {
        run: u64,
        agent: String,
        req: u64,
        call: String,
        name: String,
        summary: String,
    },
    /// Live output chunk from a running tool. Best-effort preview: the
    /// pipe tap is bounded and counts drops — see ToolDone.dropped.
    ToolOut {
        run: u64,
        agent: String,
        call: String,
        err: bool,
        text: String,
    },
    /// Tool finished / denied / skipped / intercepted. Pairs with
    /// ToolStart by (run, agent, call); a ToolDone without a start is a
    /// call that never executed.
    ToolDone {
        run: u64,
        agent: String,
        call: String,
        name: String,
        ms: u128,
        status: ToolStatus,
        exit: Option<i32>,
        /// Bounded display excerpt of the captured result (≤8KB).
        result: String,
        /// The captured result itself was truncated at the capture cap.
        truncated: bool,
        /// Preview chunks dropped because the UI-side channel was full.
        dropped: u64,
    },
    /// Provider-reported usage for one request.
    Usage {
        run: u64,
        agent: String,
        model: String,
        input: Option<u64>,
        cached: Option<u64>,
        written: Option<u64>,
        output: Option<u64>,
        complete: bool,
    },
    /// A mutating tool needs a decision; reply goes on `reply`.
    /// `agent` identifies who is asking — a mission can have several
    /// pending at once, and 'a' approves the whole session.
    Permission {
        run: u64,
        id: u64,
        agent: String,
        summary: String,
        reply: tokio::sync::mpsc::UnboundedSender<GateChoice>,
    },
    /// Agent/task phase note with human detail — repair reasons carry
    /// the failing gate, not just a bare "Repairing" stage.
    Phase {
        run: u64,
        agent: String,
        text: String,
    },
    /// Mission state machine transition.
    MissionState(String),
    /// Task-contract status row (from plan / worker results).
    TaskRows(Value),
    /// Files changed on a candidate (+ branch/sha when known).
    ChangeSet {
        files: Vec<String>,
        sha: Option<String>,
    },
    /// Auditor verdict payload.
    AuditResult(Value),
    /// The whole run finished.
    RunDone {
        run: u64,
        outcome: String,
        accepted_sha: Option<String>,
    },
    /// Non-fatal error line (per-agent or driver).
    Error {
        run: u64,
        agent: String,
        msg: String,
    },
}
