//! ACP `session/update` stream → Sui's typed events + journal records.
//!
//! Everything here is AGENT-REPORTED evidence: a `tool_call` update means
//! the external agent says it ran a tool — Sui never executes it again,
//! and the deterministic gates (ownership, acceptance) stay authoritative.
//! `UsageUpdate` context/cost figures are journaled raw and never folded
//! into per-request token fields — one ACP prompt is not one LLM request.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use agent_client_protocol::schema::v1::{
    ContentBlock, SessionUpdate, ToolCallContent, ToolCallStatus,
};
use serde_json::{json, Value};

use crate::events::{Sink, ToolStatus, UiEvent};
use crate::journal::Journal;

/// Per-call display metadata remembered between `tool_call` and its
/// `tool_call_update`s.
struct CallMeta {
    name: String,
    started: Instant,
    /// Count of content items already surfaced as ToolOut previews —
    /// updates carry snapshots, so we only emit the new tail.
    emitted: usize,
}

pub struct Norm {
    run: u64,
    agent: String,
    events: Option<Sink>,
    journal: Arc<Mutex<Journal>>,
    /// Current prompt sequence — ACP turns have no request id; the driver
    /// assigns one per `session/prompt` so transcript identity stays stable.
    req: u64,
    /// Whether this turn streamed displayable reasoning.
    saw_reasoning: bool,
    calls: HashMap<String, CallMeta>,
    /// The tool-call id currently awaiting a permission decision — the
    /// update for it is suppressed until the decision lands.
    pub pending_perm: Option<String>,
}

fn clip(s: &str, cap: usize) -> String {
    if s.len() > cap {
        let end = crate::context::floor_char_boundary(s, cap.min(s.len()));
        format!("{}…<truncated>", &s[..end])
    } else {
        s.to_string()
    }
}

fn clip_val(v: &Value, cap: usize) -> Value {
    match v {
        Value::String(s) => json!(clip(s, cap)),
        other => {
            let s = serde_json::to_string(other).unwrap_or_default();
            json!(clip(&s, cap))
        }
    }
}

fn block_text(b: &ContentBlock) -> String {
    match b {
        ContentBlock::Text(t) => t.text.clone(),
        ContentBlock::Image(_) => "[image]".into(),
        ContentBlock::Audio(_) => "[audio]".into(),
        ContentBlock::ResourceLink(r) => format!("[link: {}]", r.uri),
        ContentBlock::Resource(_) => "[resource]".into(),
        _ => "[content]".into(),
    }
}

/// Flatten tool-call content into a bounded display excerpt.
fn content_excerpt(items: &[ToolCallContent]) -> String {
    let mut out = String::new();
    for it in items {
        match it {
            ToolCallContent::Content(c) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&block_text(&c.content));
            }
            ToolCallContent::Diff(d) => {
                out.push_str(&format!(
                    "\ndiff {} (+{} -{})",
                    d.path.display(),
                    d.new_text.lines().count(),
                    d.old_text.as_deref().unwrap_or("").lines().count()
                ));
            }
            ToolCallContent::Terminal(t) => {
                out.push_str(&format!("\n[terminal {}]", t.terminal_id));
            }
            _ => {}
        }
    }
    out.trim_start_matches('\n').to_string()
}

fn tool_name(kind: &agent_client_protocol::schema::v1::ToolKind, title: &str) -> String {
    // Title is human text like `Edit src/foo.rs`; the kind is the stable
    // bucket the transcript groups on.
    let k = format!("{kind:?}").to_lowercase();
    if title.is_empty() {
        k
    } else {
        format!("{k}: {}", clip(title, 120))
    }
}

impl Norm {
    pub fn new(
        run: u64,
        agent: String,
        events: Option<Sink>,
        journal: Arc<Mutex<Journal>>,
    ) -> Self {
        Self {
            run,
            agent,
            events,
            journal,
            req: 0,
            saw_reasoning: false,
            calls: HashMap::new(),
            pending_perm: None,
        }
    }

    fn emit(&self, e: UiEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(e);
        }
    }

    pub fn jlog(&self, kind: &str, data: Value) {
        if let Ok(mut j) = self.journal.lock() {
            j.log(kind, data);
        }
    }

    /// A human-facing phase note (session up, plan progress, usage hint).
    pub fn phase(&self, text: String) {
        self.emit(UiEvent::Phase {
            run: self.run,
            agent: self.agent.clone(),
            text,
        });
    }

    /// Driver marks a new `session/prompt` turn. Returns the req id.
    pub fn prompt_start(&mut self) -> u64 {
        self.req += 1;
        self.saw_reasoning = false;
        self.emit(UiEvent::ReqStart {
            run: self.run,
            agent: self.agent.clone(),
            req: self.req,
        });
        self.req
    }

    pub fn prompt_done(&mut self, ms: u128, ok: bool, stop: &str) {
        self.emit(UiEvent::ReqDone {
            run: self.run,
            agent: self.agent.clone(),
            req: self.req,
            ms,
            ok,
            reasoning: self.saw_reasoning,
        });
        self.jlog(
            "acp_prompt",
            json!({ "req": self.req, "ms": ms, "ok": ok, "stop_reason": stop }),
        );
    }

    /// One `session/update` notification → typed events + journal.
    /// Non-exhaustive enum: unknown variants land in the journal only.
    pub fn on_update(&mut self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(c) => {
                let text = block_text(&c.content);
                if !text.is_empty() {
                    self.emit(UiEvent::Delta {
                        run: self.run,
                        agent: self.agent.clone(),
                        req: self.req,
                        text,
                    });
                }
            }
            SessionUpdate::AgentThoughtChunk(c) => {
                let text = block_text(&c.content);
                if !text.is_empty() {
                    self.saw_reasoning = true;
                    self.emit(UiEvent::Reason {
                        run: self.run,
                        agent: self.agent.clone(),
                        req: self.req,
                        text,
                    });
                }
            }
            SessionUpdate::UserMessageChunk(_) => {}
            SessionUpdate::ToolCall(tc) => {
                let id = tc.tool_call_id.to_string();
                let name = tool_name(&tc.kind, &tc.title);
                self.calls.insert(
                    id.clone(),
                    CallMeta {
                        name: name.clone(),
                        started: Instant::now(),
                        emitted: 0,
                    },
                );
                let loc = tc
                    .locations
                    .first()
                    .map(|l| format!(" @ {}", l.path.display()))
                    .unwrap_or_default();
                self.jlog(
                    "acp_update",
                    json!({ "kind": "tool_call", "id": id, "title": clip(&tc.title, 300),
                            "tool_kind": format!("{:?}", tc.kind), "locations": tc.locations.len(),
                            "raw_input": tc.raw_input.as_ref().map(|v| clip_val(v, 4000)) }),
                );
                self.emit(UiEvent::ToolStart {
                    run: self.run,
                    agent: self.agent.clone(),
                    req: self.req,
                    call: id.clone(),
                    name,
                    summary: format!("{}{}", clip(&tc.title, 200), loc),
                });
                // a ToolCall may arrive already terminal
                self.finish_call(&id, &tc.status, tc.raw_output.as_ref(), &tc.content);
            }
            SessionUpdate::ToolCallUpdate(u) => {
                let id = u.tool_call_id.to_string();
                let f = &u.fields;
                if let Some(t) = &f.title {
                    if let Some(m) = self.calls.get_mut(&id) {
                        m.name = tool_name(&f.kind.unwrap_or_default(), t);
                    }
                }
                // non-terminal updates: surface new content as preview chunks
                let terminal = matches!(
                    f.status,
                    Some(ToolCallStatus::Completed | ToolCallStatus::Failed)
                );
                if !terminal {
                    if let Some(items) = &f.content {
                        let m = self.calls.entry(id.clone()).or_insert_with(|| CallMeta {
                            name: "tool".into(),
                            started: Instant::now(),
                            emitted: 0,
                        });
                        if items.len() > m.emitted {
                            let new = content_excerpt(&items[m.emitted..]);
                            m.emitted = items.len();
                            if !new.is_empty() {
                                self.emit(UiEvent::ToolOut {
                                    run: self.run,
                                    agent: self.agent.clone(),
                                    call: id.clone(),
                                    err: false,
                                    text: new,
                                });
                            }
                        }
                    }
                }
                if let Some(st) = &f.status {
                    self.finish_call(
                        &id,
                        st,
                        f.raw_output.as_ref(),
                        f.content.as_deref().unwrap_or(&[]),
                    );
                }
                self.jlog(
                    "acp_update",
                    json!({ "kind": "tool_call_update", "id": id,
                            "status": f.status.map(|s| format!("{s:?}")),
                            "raw_output": f.raw_output.as_ref().map(|v| clip_val(v, 4000)) }),
                );
            }
            // Agent-reported progress plan — display + journal, never proof.
            SessionUpdate::Plan(p) => {
                let done = p
                    .entries
                    .iter()
                    .filter(|e| {
                        matches!(
                            e.status,
                            agent_client_protocol::schema::v1::PlanEntryStatus::Completed
                        )
                    })
                    .count();
                self.jlog(
                    "acp_update",
                    json!({ "kind": "plan", "entries": p.entries.iter().map(|e| json!({
                        "content": clip(&e.content, 200),
                        "status": format!("{:?}", e.status),
                        "priority": format!("{:?}", e.priority),
                    })).collect::<Vec<_>>() }),
                );
                self.emit(UiEvent::Phase {
                    run: self.run,
                    agent: self.agent.clone(),
                    text: format!("agent plan: {done}/{} done (reported)", p.entries.len()),
                });
            }
            SessionUpdate::UsageUpdate(u) => {
                // Context-window occupancy + optional reported cost. NOT
                // per-request tokens — journaled raw, never summed.
                self.jlog(
                    "acp_usage",
                    json!({ "used": u.used, "size": u.size,
                            "cost": u.cost.as_ref().map(|c| json!({
                                "amount": c.amount, "currency": c.currency })) }),
                );
                self.emit(UiEvent::Phase {
                    run: self.run,
                    agent: self.agent.clone(),
                    text: match &u.cost {
                        Some(c) => format!(
                            "ctx {}/{} · {} {} (agent-reported)",
                            u.used, u.size, c.amount, c.currency
                        ),
                        None => format!("ctx {}/{} (agent-reported)", u.used, u.size),
                    },
                });
            }
            SessionUpdate::AvailableCommandsUpdate(c) => {
                self.jlog(
                    "acp_update",
                    json!({ "kind": "available_commands",
                            "commands": c.available_commands.iter()
                                .map(|c| c.name.clone()).collect::<Vec<_>>() }),
                );
            }
            SessionUpdate::CurrentModeUpdate(m) => {
                self.jlog(
                    "acp_update",
                    json!({ "kind": "mode", "current": m.current_mode_id.to_string() }),
                );
            }
            SessionUpdate::ConfigOptionUpdate(c) => {
                self.jlog(
                    "acp_update",
                    json!({ "kind": "config_options",
                            "options": c.config_options.iter().map(|o| o.id.to_string())
                                .collect::<Vec<_>>() }),
                );
            }
            SessionUpdate::SessionInfoUpdate(i) => {
                self.jlog(
                    "acp_update",
                    json!({ "kind": "session_info",
                    "title": format!("{:?}", i.title) }),
                );
            }
            _ => {
                self.jlog(
                    "acp_update",
                    json!({ "kind": "other", "update": clip_val(
                        &serde_json::to_value(update).unwrap_or_default(), 2000) }),
                );
            }
        }
    }

    /// Terminal tool status → ToolDone. `raw_output`/content become the
    /// bounded result excerpt; a call we never saw start still gets a
    /// paired row (ToolDone-without-Start renders as such upstream).
    fn finish_call(
        &mut self,
        id: &str,
        status: &ToolCallStatus,
        raw_output: Option<&Value>,
        content: &[ToolCallContent],
    ) {
        let (done, ts) = match status {
            ToolCallStatus::Completed => (true, ToolStatus::Ok),
            ToolCallStatus::Failed => (true, ToolStatus::Failed),
            _ => (false, ToolStatus::Ok),
        };
        if !done {
            return;
        }
        let m = self.calls.remove(id).unwrap_or(CallMeta {
            name: "tool".into(),
            started: Instant::now(),
            emitted: 0,
        });
        let mut result = content_excerpt(content);
        if result.is_empty() {
            if let Some(v) = raw_output {
                result = clip(&serde_json::to_string_pretty(v).unwrap_or_default(), 8000);
            }
        }
        let truncated = result.len() > 8000;
        self.emit(UiEvent::ToolDone {
            run: self.run,
            agent: self.agent.clone(),
            call: id.to_string(),
            name: m.name,
            ms: m.started.elapsed().as_millis(),
            status: ts,
            exit: None,
            result: clip(&result, 8000),
            truncated,
            dropped: 0,
        });
    }
}

/// Map a gate decision onto the offered permission options.
/// Prefers the least-privilege matching option; `None` → Cancelled.
pub fn pick_option(
    options: &[agent_client_protocol::schema::v1::PermissionOption],
    choice: crate::events::GateChoice,
) -> Option<agent_client_protocol::schema::v1::PermissionOptionId> {
    use crate::events::GateChoice as G;
    use agent_client_protocol::schema::v1::PermissionOptionKind as K;
    let kinds: &[K] = match choice {
        G::Once => &[K::AllowOnce, K::AllowAlways],
        G::Session => &[K::AllowAlways, K::AllowOnce],
        G::Deny => &[K::RejectOnce, K::RejectAlways],
    };
    for k in kinds {
        if let Some(o) = options.iter().find(|o| o.kind == *k) {
            return Some(o.option_id.clone());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::v1::{PermissionOption, PermissionOptionKind as K};

    #[test]
    fn pick_prefers_least_privilege() {
        let opts = vec![
            PermissionOption::new("aa", "always", K::AllowAlways),
            PermissionOption::new("ro", "reject once", K::RejectOnce),
            PermissionOption::new("ao", "once", K::AllowOnce),
        ];
        assert_eq!(
            pick_option(&opts, crate::events::GateChoice::Once)
                .unwrap()
                .to_string(),
            "ao"
        );
        assert_eq!(
            pick_option(&opts, crate::events::GateChoice::Session)
                .unwrap()
                .to_string(),
            "aa"
        );
        assert_eq!(
            pick_option(&opts, crate::events::GateChoice::Deny)
                .unwrap()
                .to_string(),
            "ro"
        );
        // missing kind → falls back / cancels
        let only_always = vec![PermissionOption::new("aa", "always", K::AllowAlways)];
        assert!(pick_option(&only_always, crate::events::GateChoice::Deny).is_none());
    }
}
