use super::plan::{MissionPlan, TaskContract};
use crate::context::SYSTEM;

/// Shared control-plane contract: identical for orchestrator and auditor
/// so the strong-model domain keeps one stable prefix. Role-specific
/// instructions travel in the first user message (volatile tail).
pub const CONTROL_SYSTEM: &str = "You are the control plane of a Rust coding harness. \
You have read_file, write_file, edit_file, bash (same envelope semantics) plus \
submit_result(payload: object). \
When your deliverable is complete, call submit_result exactly once with the \
required JSON payload; the harness validates it and ends your turn. \
If submit_result returns an error, fix the payload and resubmit — never emit \
the deliverable as prose. Keep all responses terse; spend tokens on judgment.";

/// The mission worker shares the fast-path contract plus ownership rules —
/// identical text for every worker so the cheap-model prefix stays shared.
/// Task identity, worktree paths, and acceptance criteria arrive in the
/// first user message (volatile tail).
pub fn worker_system() -> String {
    format!(
        "{SYSTEM}\n\nMission worker rules: you own ONLY the paths listed in your task. \
         Do not read sibling task directories or edit outside owned_paths — \
         violations are rejected by the runtime. Satisfy the acceptance \
         commands before finishing. Keep diffs minimal."
    )
}

/// Tool schema for the structured deliverable submission.
pub fn submit_result_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "function",
        "function": {
            "name": "submit_result",
            "description": "Submit your final structured deliverable as JSON in `payload`. The harness validates it; on acceptance your turn ends.",
            "parameters": {
                "type": "object",
                "properties": { "payload": { "type": "object" } },
                "required": ["payload"],
                "additionalProperties": false
            }
        }
    })
}

const PLAN_SHAPE: &str = r#"{
  "objective": "<mission objective>",
  "base_commit": "<commit provided below>",
  "tasks": [
    {
      "id": "W1",
      "objective": "<bounded task objective>",
      "owned_paths": ["src/foo.rs" or "src/dir/**"],
      "read_paths": ["<optional context files>"],
      "depends_on": ["<task ids>"],
      "acceptance": ["<shell command that must exit 0>"],
      "max_turns": 40
    }
  ],
  "integration_checks": ["<shell command run on the merged result>"]
}"#;

pub fn orchestrator_task(objective: &str, base: &str, overview: &str) -> String {
    format!(
        "ROLE: orchestrator. Decompose the objective into bounded, \
conflict-free task contracts, then submit via submit_result.\n\
RULES: owned_paths must be pairwise disjoint (never share a file between \
tasks). Every task needs concrete acceptance shell commands. \
Prefer few tasks. Keep each task small and independent.\n\
PAYLOAD SHAPE (exact keys):\n{PLAN_SHAPE}\n\
BASE COMMIT: {base}\nOBJECTIVE: {objective}\nREPOSITORY OVERVIEW:\n{overview}"
    )
}

pub fn worker_task(c: &TaskContract, worktree_name: &str) -> String {
    format!(
        "TASK {}\nobjective: {}\nyou own ONLY: {}\ncontext reads: {}\n\
acceptance (must all exit 0): {}\nworktree: {}\n\
Do not touch files outside owned_paths. When acceptance passes, reply with a \
one-line summary.",
        c.id,
        c.objective,
        c.owned_paths.join(", "),
        if c.read_paths.is_empty() {
            "any".into()
        } else {
            c.read_paths.join(", ")
        },
        c.acceptance.join("; "),
        worktree_name,
    )
}

pub fn repair_task(c: &TaskContract, capsule: &str) -> String {
    format!(
        "REPAIR ROUND for task {}. Your previous candidate failed:\n{}\n\
Fix it inside the same worktree. Owned paths unchanged: {}. \
Acceptance must pass: {}",
        c.id,
        capsule,
        c.owned_paths.join(", "),
        c.acceptance.join("; "),
    )
}

pub fn audit_task(plan: &MissionPlan, diff: &str, gate_results: &str, risks: &str) -> String {
    format!(
        "ROLE: auditor. Review the integrated candidate objectively.\n\
PAYLOAD SHAPE: {{\"verdict\": \"PASS\"|\"FAIL\", \"findings\": [{{\"severity\": \"blocker\"|\"minor\", \"detail\": \"...\"}}], \"required_fixes\": [\"...\"]}}\n\
FAIL only on real correctness/criteria failures.\n\
MISSION OBJECTIVE: {}\nTASK CONTRACTS:\n{}\n\
GATE RESULTS:\n{}\nKNOWN RISKS / REPAIR HISTORY:\n{}\n\
INTEGRATED DIFF:\n{}",
        plan.objective,
        serde_json::to_string_pretty(&plan.tasks.iter().map(|t| serde_json::json!({
            "id": t.id, "objective": t.objective, "owned_paths": t.owned_paths,
            "acceptance": t.acceptance,
        })).collect::<Vec<_>>()).unwrap_or_default(),
        gate_results,
        if risks.is_empty() { "none" } else { risks },
        diff,
    )
}

/// One-shot escalation: a failure capsule for the control plane.
/// Reply payload: {"decision": "retry"|"abort", "revised_task": <contract|null>, "reason": "..."}
pub fn escalation_task(contract_json: &str, capsule: &str, budget_left: usize) -> String {
    format!(
        "ESCALATION. A task failed after its repair round. \
Original contract:\n{contract_json}\n\
Failure capsule:\n{capsule}\n\
Escalations remaining after this decision: {budget_left}.\n\
Submit payload: {{\"decision\": \"retry\"|\"abort\", \"revised_task\": <full contract object or null>, \"reason\": \"<short>\"}}. \
retry = re-dispatch task with the revised contract. abort = fail the mission."
    )
}
