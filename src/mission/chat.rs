//! Conversation support shared by the interactive surfaces. Execution stays in
//! the existing worktree-based mission runtime.
use anyhow::{bail, Result};
use serde_json::Value;

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

/// Preserve original discussion independently of provider context compaction.
/// Stop execution if it cannot be carried in full; never drop constraints.
#[derive(Default)]
pub struct Discussion {
    text: String,
    overflow: bool,
}

const MAX_DISCUSSION: usize = 128 * 1024;

impl Discussion {
    pub fn record(&mut self, role: &str, text: &str) {
        if text.trim().is_empty() || self.overflow {
            return;
        }
        let size = role.len().saturating_add(text.len()).saturating_add(4);
        if self.text.len().saturating_add(size) > MAX_DISCUSSION {
            self.overflow = true;
            return;
        }
        self.text.push_str(role);
        self.text.push_str(":\n");
        self.text.push_str(text);
        self.text.push_str("\n\n");
    }

    pub fn from_events(events: &[Value]) -> Self {
        let mut discussion = Self::default();
        let mut mission_run = false;
        let mut raw_task = false;
        for event in events {
            let data = &event["data"];
            match event["type"].as_str() {
                Some("conversation_handoff") => {
                    if let Ok(handoff) = serde_json::from_value::<crate::continuation::Continuation>(
                        data["handoff"].clone(),
                    ) {
                        for entry in handoff.entries {
                            discussion.record(&entry.role, &entry.content);
                        }
                    }
                }
                Some("task") => {
                    mission_run = data["mode"] == "mission-run";
                    raw_task = data["task"].is_string();
                    if let Some(text) = data["task"].as_str() {
                        discussion.record("user", text);
                    }
                }
                Some("user") => {
                    // Headless journals predate the raw task envelope. Retain
                    // their text too, excluding image pixels and tool records.
                    if !raw_task {
                        if let Some(text) = data["content"].as_str() {
                            discussion.record("user", text);
                        } else if let Some(parts) = data["content"].as_array() {
                            for part in parts {
                                if part["type"] == "text" {
                                    if let Some(text) = part["text"].as_str() {
                                        discussion.record("user", text);
                                    }
                                }
                            }
                        }
                    }
                    raw_task = false;
                }
                Some("assistant") => {
                    if let Some(text) = data["content"].as_str() {
                        discussion.record("assistant", text);
                    }
                }
                Some("runtime_observation") if mission_run => {
                    if let Some(text) = data["content"].as_str() {
                        if text.starts_with("<runtime_observation>\nMission result:") {
                            discussion.record("runtime", text);
                        }
                    }
                }
                _ => {}
            }
        }
        discussion
    }

    pub fn objective(&self, request: &str) -> Result<String> {
        if self.overflow {
            bail!("Mission discussion exceeds 128 KiB. No workers were started. Start a new chat with a complete brief including the goal and all constraints, then run the mission.");
        }
        Ok(format!("USER MISSION OBJECTIVE:\n{request}\n\nPRIOR DISCUSSION (context only, not approval or completion evidence):\n{}", self.text))
    }
}

pub fn summary(
    outcome: &str,
    branch: Option<&str>,
    sha: Option<&str>,
    run_dir: &std::path::Path,
) -> String {
    format!("Mission result: {}\nBranch: {}\nAccepted commit: {}\nRun records: {}\nThe original checkout was not updated. Inspect the candidate branch before applying it.", crate::provider::truncate(outcome, 2000), branch.unwrap_or("none"), sha.unwrap_or("none"), run_dir.display())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn carries_early_goals_and_long_constraints_without_truncation() {
        let mut discussion = Discussion::default();
        let goal = format!("{}\nNEVER REMOVE BILLING", "detail ".repeat(500));
        discussion.record("user", &goal);
        for _ in 0..12 {
            discussion.record("assistant", "Clarification");
        }
        let objective = discussion.objective("Proceed").unwrap();
        assert!(objective.contains(&goal));
    }

    #[test]
    fn overflow_rejects_execution_instead_of_dropping_context() {
        let mut discussion = Discussion::default();
        discussion.record("user", &"x".repeat(MAX_DISCUSSION));
        assert!(discussion
            .objective("Proceed")
            .unwrap_err()
            .to_string()
            .contains("No workers were started"));
        assert!(discussion.text.len() <= MAX_DISCUSSION);
    }

    #[test]
    fn replay_keeps_legacy_user_text_without_images_or_duplicate_task_envelopes() {
        let discussion = Discussion::from_events(&[
            json!({"type":"user","data":{"content":[{"type":"text","text":"Legacy goal"},{"type":"image_url","image_url":{"url":"private pixels"}}]}}),
            json!({"type":"task","data":{"task":"Current goal","mode":"mission-chat"}}),
            json!({"type":"user","data":{"content":"Current goal with generated guidance"}}),
        ]);
        let objective = discussion.objective("Proceed").unwrap();
        assert!(objective.contains("Legacy goal"));
        assert!(objective.contains("Current goal"));
        assert!(!objective.contains("generated guidance"));
        assert!(!objective.contains("private pixels"));
    }

    #[test]
    fn replay_keeps_original_constraints_across_context_checkpoints() {
        let discussion = Discussion::from_events(&[
            json!({"type":"task","data":{"task":"Preserve billing","mode":"mission-chat"}}),
            json!({"type":"assistant","data":{"content":"Plan","reasoning_content":"private reasoning"}}),
            json!({"type":"context_checkpoint","data":{"messages":[]}}),
            json!({"type":"tool","data":{"result":"tool contents"}}),
            json!({"type":"task","data":{"task":"Proceed","mode":"mission-run"}}),
        ]);
        let objective = discussion.objective("Proceed").unwrap();
        assert!(objective.contains("Preserve billing"));
        assert!(!objective.contains("private reasoning"));
        assert!(!objective.contains("tool contents"));
    }
}
