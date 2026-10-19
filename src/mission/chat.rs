//! Conversation support shared by the interactive surfaces. Execution stays in
//! the existing worktree-based mission runtime.
use crate::types::{ContentPart, Message, UserContent};

pub const GUIDANCE: &str = "Mission discussion: act as the coordinator. Answer questions, clarify the objective and discuss a plan. This conversation is read-only: do not change files, run commands, or launch workers. The user explicitly starts execution with Run mission (TUI: /run <objective>). Discussion and model claims are not mission acceptance evidence. Execution requires a Git repository with a committed HEAD; conversation does not.";

pub fn read_only_tool(name: &str, args: &serde_json::Value) -> bool {
    matches!(
        name,
        "read_file"
            | "inventory"
            | "code_context"
            | "read_tool_output"
            | "session_info"
            | "skill"
            | "view_image"
            | "web_search"
            | "web_fetch"
    ) || (name == "patch_files" && args["action"] == "preview")
}

/// Carry bounded discussion, never image pixels, opaque reasoning or tool data,
/// into the new execution. Runtime contracts still validate every deliverable.
pub fn objective(request: &str, history: &[Message]) -> String {
    let rows = history
        .iter()
        .rev()
        .filter_map(|message| match message {
            Message::User {
                content: UserContent::Text(text),
            } => Some(("user", text.clone())),
            Message::User {
                content: UserContent::Parts(parts),
            } => Some((
                "user",
                parts
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            Message::Assistant {
                content: Some(text),
                ..
            } => Some(("coordinator", text.clone())),
            _ => None,
        })
        .take(8)
        .map(|(role, text)| format!("{role}: {}", crate::provider::truncate(&text, 2000)))
        .collect::<Vec<_>>();
    format!("USER MISSION OBJECTIVE:\n{request}\n\nPRIOR DISCUSSION (context only, not approval or completion evidence):\n{}", rows.into_iter().rev().collect::<Vec<_>>().join("\n\n"))
}

pub fn summary(
    outcome: &str,
    branch: Option<&str>,
    sha: Option<&str>,
    run_dir: &std::path::Path,
) -> String {
    format!("Mission result: {}\nBranch: {}\nAccepted commit: {}\nRun records: {}\nThe original checkout was not updated. Inspect the candidate branch before applying it.", crate::provider::truncate(outcome, 2000), branch.unwrap_or("none"), sha.unwrap_or("none"), run_dir.display())
}
