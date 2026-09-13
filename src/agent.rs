use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::context::{self, LayerHashes};
use crate::events::{Sink, UiEvent};
use crate::journal::Journal;
use crate::permission::Gate;
use crate::provider::Provider;
use crate::tools::{self, ToolContext};
use crate::types::Message;

/// Hard limits for a single agent trajectory.
pub struct Limits {
    pub max_turns: usize,
    /// Estimated input tokens + reserve above this stops the turn.
    pub context_budget: usize,
    /// Reserved completion capacity counted against the budget.
    pub context_reserve: usize,
    pub request_timeout: Duration,
}

/// Identity fields stamped on every trace record.
pub struct Identity {
    pub session_id: String,
    /// Scenario/agent label, e.g. "fast-path" or a certification scenario.
    pub agent_id: String,
    pub role: String,
    pub base_url: String,
    pub model: String,
    pub cache_key_fingerprint: Option<String>,
}

const KNOWN_TOOLS: &[&str] = &["read_file", "write_file", "edit_file", "bash"];

/// Result of an intercepted tool call (e.g. orchestrator plan submission).
/// Intercepted calls never touch the filesystem.
pub enum Intercept {
    /// Emitted as the tool result; the loop continues.
    Result(String),
    /// Emitted as the tool result, then the turn ends (remaining calls in
    /// the batch get "skipped" envelopes so every call has a response).
    Finish(String),
}

type Interceptor = Arc<dyn Fn(&crate::types::ToolCall) -> Option<Intercept> + Send + Sync>;

/// One tool call's terminal disposition — text is the model-facing
/// envelope; the rest are typed facts for the UI/journal.
struct Disp {
    text: String,
    status: crate::events::ToolStatus,
    exit: Option<i32>,
    truncated: bool,
    /// Preview chunks the UI tap dropped (channel full).
    dropped: u64,
    executed: bool,
}
impl Disp {
    fn new(text: String, status: crate::events::ToolStatus) -> Self {
        Self {
            text,
            status,
            exit: None,
            truncated: false,
            dropped: 0,
            executed: false,
        }
    }
}

/// Largest index ≤ i on a char boundary (stable replacement for the
/// nightly `floor_char_boundary`).
fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// One deterministic worker trajectory: append-only history, serialized
/// tool execution, no planner. This same loop is the fast path and the
/// mission-mode worker.
pub struct Agent {
    provider: Provider,
    tools: ToolContext,
    gate: Gate,
    journal: Journal,
    history: Vec<Message>,
    system: String,
    limits: Limits,
    ident: Identity,
    tool_schemas: Vec<Value>,
    hashes: LayerHashes,
    request_seq: u64,
    /// Activity-run id stamped on every UI event — set by the driver
    /// (one run per submitted task; a mission's agents all share it).
    run_id: u64,
    known: Vec<String>,
    interceptor: Option<Interceptor>,
    quiet: bool,
    events: Option<Sink>,
    cancel: Option<Arc<tokio::sync::Notify>>,
    stop: Arc<AtomicBool>,
}

impl Agent {
    pub fn new(
        provider: Provider,
        tools: ToolContext,
        gate: Gate,
        journal: Journal,
        limits: Limits,
        ident: Identity,
    ) -> Self {
        let tool_schemas = tools::schemas();
        let hashes = context::layer_hashes(&tool_schemas, context::SYSTEM);
        let known = KNOWN_TOOLS.iter().map(|s| s.to_string()).collect();
        Self {
            provider,
            tools,
            gate,
            journal,
            history: Vec::new(),
            system: context::SYSTEM.to_string(),
            limits,
            ident,
            tool_schemas,
            hashes,
            request_seq: 0,
            run_id: 0,
            known,
            interceptor: None,
            quiet: false,
            events: None,
            cancel: None,
            stop: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Wire a UI: typed events out, shared cancellation in (Stop button /
    /// mission-level cancel), gate decisions as interactive modals, and an
    /// optional shared session-approval flag so UI policy changes reach
    /// the live gate without respawning the agent.
    pub fn wire_ui(
        &mut self,
        sink: Sink,
        cancel: Arc<tokio::sync::Notify>,
        stop: Arc<AtomicBool>,
        session: Option<Arc<AtomicBool>>,
    ) {
        self.events = Some(sink.clone());
        self.cancel = Some(cancel);
        self.stop = stop.clone();
        self.gate.set_ui(sink, stop, session);
    }

    fn emit(&self, e: UiEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(e);
        }
    }

    /// Register an extra (intercepted-only) tool schema — e.g. submit_result.
    /// Changes the tool layer fingerprint; call before driving.
    pub fn add_tool_schema(&mut self, schema: Value) {
        if let Some(n) = schema["function"]["name"].as_str() {
            self.known.push(n.to_string());
        }
        self.tool_schemas.push(schema);
        self.hashes = context::layer_hashes(&self.tool_schemas, &self.system);
    }

    /// Intercept matching tool calls before execution.
    /// Return None → normal tool execution; Some(Result) → envelope, loop
    /// continues; Some(Finish) → envelope, turn ends.
    pub fn set_interceptor(&mut self, f: Interceptor) {
        self.interceptor = Some(f);
    }

    /// Suppress streamed-token printing (mission workers don't spam stdout).
    pub fn set_quiet(&mut self, quiet: bool) {
        self.quiet = quiet;
    }

    /// Override the static contract — used by certification to force a
    /// deliberate prefix invalidation.
    pub fn set_system(&mut self, system: String) {
        self.hashes = context::layer_hashes(&self.tool_schemas, &system);
        self.system = system;
    }

    /// Stamp subsequent UI events with this activity-run id.
    pub fn set_run_id(&mut self, run: u64) {
        self.run_id = run;
    }

    /// Rehydrate history (e.g. rebuilt from a journal for restart/replay).
    pub fn restore_history(&mut self, msgs: Vec<Message>) {
        self.history = msgs;
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn requests_made(&self) -> u64 {
        self.request_seq
    }

    /// Queue a user message, then drive until completion.
    pub async fn run_turn(&mut self, user_input: &str) -> Result<()> {
        self.push_user(user_input);
        self.drive().await
    }

    pub fn push_user(&mut self, user_input: &str) {
        self.history.push(Message::User {
            content: user_input.to_string(),
        });
        self.journal.log("user", json!({ "content": user_input }));
    }

    /// Direct journal write for run-boundary evidence the agent loop does
    /// not itself produce (task start/end, approval policy). Kept out of
    /// `history` — never model-visible.
    pub fn jlog(&mut self, kind: &str, data: Value) {
        self.journal.log(kind, data);
    }

    /// The agent loop without pushing a new user message — safe to call
    /// again after a failed request (no duplicate user turn).
    pub async fn drive(&mut self) -> Result<()> {
        for _ in 0..self.limits.max_turns {
            let t_asm = Instant::now();
            let req = context::compile(&self.history, &self.system);
            let assembly_ms = t_asm.elapsed().as_millis();
            let est_tokens = context::estimate_tokens(&req);
            let request_fp = context::request_fingerprint(&req);
            let req_id = self.request_seq;
            self.request_seq += 1;
            if std::env::var_os("SUI_DEBUG_REQ").is_some() {
                let _ = std::fs::write(
                    format!("/tmp/sui-req-{}-{}.json", self.ident.agent_id, req_id),
                    serde_json::to_string_pretty(&req).unwrap_or_default(),
                );
            }

            if est_tokens + self.limits.context_reserve > self.limits.context_budget {
                if !self.quiet {
                    eprintln!(
                        "· context budget exceeded (~{} est + {} reserve > {}); start a new session",
                        est_tokens, self.limits.context_reserve, self.limits.context_budget
                    );
                }
                self.journal.log(
                    "budget_exceeded",
                    json!({ "est_tokens": est_tokens, "reserve": self.limits.context_reserve,
                            "budget": self.limits.context_budget }),
                );
                return Ok(());
            }

            let quiet = self.quiet;
            let ev = self.events.clone();
            let aid = self.ident.agent_id.clone();
            let run_id = self.run_id;
            self.emit(UiEvent::ReqStart {
                run: run_id,
                agent: aid.clone(),
                req: req_id,
            });
            let outcome = tokio::select! {
                r = tokio::time::timeout(
                    self.limits.request_timeout,
                    self.provider.stream_chat(&req, &self.tool_schemas, |d| {
                        if !quiet {
                            print!("{d}");
                            let _ = std::io::stdout().flush();
                        }
                        if let Some(tx) = &ev {
                            let _ = tx.send(UiEvent::Delta {
                                run: run_id,
                                agent: aid.clone(),
                                req: req_id,
                                text: d.to_string(),
                            });
                        }
                    }, |r| {
                        if let Some(tx) = &ev {
                            let _ = tx.send(UiEvent::Reason {
                                run: run_id,
                                agent: aid.clone(),
                                req: req_id,
                                text: r.to_string(),
                            });
                        }
                    }),
                ) => match r {
                    Ok(inner) => inner,
                    Err(_) => Err(anyhow!("request deadline exceeded")),
                },
                _ = cancel_wait(self.cancel.clone()) => {
                    if !quiet {
                        eprintln!("\n· interrupted");
                    }
                    self.emit(UiEvent::ReqDone {
                        run: run_id, agent: aid, req: req_id, ms: 0, ok: false, reasoning: false,
                    });
                    self.journal.log("interrupted", json!({ "request_id": req_id, "phase": "request" }));
                    return Ok(());
                }
            };

            let outcome = match outcome {
                Ok(o) => o,
                Err(e) => {
                    self.journal.log(
                        "request",
                        self.trace(
                            req_id,
                            assembly_ms,
                            est_tokens,
                            &request_fp,
                            None,
                            None,
                            None,
                            0,
                            0,
                            Some(error_class(&e)),
                        ),
                    );
                    self.emit(UiEvent::ReqDone {
                        run: run_id,
                        agent: self.ident.agent_id.clone(),
                        req: req_id,
                        ms: 0,
                        ok: false,
                        reasoning: false,
                    });
                    self.emit(UiEvent::Error {
                        run: run_id,
                        agent: self.ident.agent_id.clone(),
                        msg: format!("{e:#}"),
                    });
                    return Err(e);
                }
            };

            self.emit(UiEvent::ReqDone {
                run: run_id,
                agent: self.ident.agent_id.clone(),
                req: req_id,
                ms: outcome.total_ms,
                ok: true,
                reasoning: outcome.reasoning_content.is_some(),
            });

            if let Some(u) = &outcome.usage {
                self.emit(UiEvent::Usage {
                    run: run_id,
                    agent: self.ident.agent_id.clone(),
                    model: outcome
                        .returned_model
                        .clone()
                        .unwrap_or_else(|| self.ident.model.clone()),
                    input: u.input_tokens,
                    cached: u.cache_read_tokens,
                    written: u.cache_write_tokens,
                    output: u.output_tokens,
                    complete: u.complete,
                });
            }

            if !quiet {
                if !outcome.content.is_empty() {
                    println!();
                }
                match &outcome.usage {
                    Some(u) => eprintln!(
                        "· {} in / {} cached / {} out",
                        opt(u.input_tokens),
                        opt(u.cache_read_tokens),
                        opt(u.output_tokens)
                    ),
                    None => eprintln!("· usage: not reported"),
                }
            }
            self.journal.log(
                "request",
                self.trace(
                    req_id,
                    assembly_ms,
                    est_tokens,
                    &request_fp,
                    outcome.usage.as_ref(),
                    outcome.returned_model.as_deref(),
                    outcome.finish_reason.as_deref(),
                    outcome.first_delta_ms,
                    outcome.total_ms,
                    None,
                ),
            );
            self.journal.log(
                "assistant",
                json!({
                    "content": outcome.content,
                    "tool_calls": outcome.tool_calls,
                    "reasoning_content": outcome.reasoning_content,
                }),
            );
            self.history.push(Message::Assistant {
                content: if outcome.content.is_empty() {
                    None
                } else {
                    Some(outcome.content.clone())
                },
                tool_calls: if outcome.tool_calls.is_empty() {
                    None
                } else {
                    Some(outcome.tool_calls.clone())
                },
                reasoning_content: outcome.reasoning_content.clone(),
            });

            if outcome.tool_calls.is_empty() {
                return Ok(());
            }

            // ── Batch validation before ANY side effect ───────────────
            // A batch executes only when the completion is a valid
            // tool-call finish AND every call is a known tool with valid
            // JSON arguments. Any failure rejects the whole batch.
            let valid_completion = outcome.finish_reason.as_deref() == Some("tool_calls");
            if !valid_completion {
                self.journal.log(
                    "warn",
                    json!({ "request_id": req_id,
                            "msg": "tool calls present but finish_reason is not 'tool_calls'",
                            "finish_reason": outcome.finish_reason }),
                );
            }
            let plans: Vec<Result<Value, String>> = outcome
                .tool_calls
                .iter()
                .map(|c| {
                    if !self.known.iter().any(|k| k == &c.function.name) {
                        return Err(format!("unknown tool '{}'", c.function.name));
                    }
                    serde_json::from_str::<Value>(&c.function.arguments)
                        .map_err(|e| format!("malformed arguments: {e}"))
                })
                .collect();
            let batch_ok = valid_completion && plans.iter().all(|p| p.is_ok());

            for (i, (call, plan)) in outcome.tool_calls.iter().zip(plans.iter()).enumerate() {
                let name = call.function.name.as_str();
                let summary = summarize(name, &call.function.arguments);
                // stable display identity: provider id, else positional
                let call_id = if call.id.is_empty() {
                    format!("r{req_id}.{i}")
                } else {
                    call.id.clone()
                };
                let t_tool = Instant::now();
                let mut finish_after = false;
                // One terminal disposition per call — typed, never string-sniffed.
                let disp = if !batch_ok {
                    let why = match (valid_completion, plan) {
                        (false, _) => {
                            "batch rejected: finish_reason was not 'tool_calls'".to_string()
                        }
                        (true, Err(e)) => format!("batch rejected: {e}"),
                        (true, Ok(_)) => "batch rejected: sibling call invalid".to_string(),
                    };
                    Disp::new(
                        format!("status: error\nerror: {why} — call not executed"),
                        crate::events::ToolStatus::Skipped,
                    )
                } else if let Some(hit) = self.interceptor.as_ref().and_then(|f| f(call)) {
                    let (r, fin) = match hit {
                        Intercept::Result(r) => (r, false),
                        Intercept::Finish(r) => (r, true),
                    };
                    finish_after = fin;
                    Disp::new(r, crate::events::ToolStatus::Intercepted)
                } else if needs_approval(name)
                    && !self
                        .gate
                        .check(&summary, &self.ident.agent_id, self.run_id)
                        .await
                {
                    Disp::new(
                        "status: denied\nerror: user rejected the action".to_string(),
                        crate::events::ToolStatus::Denied,
                    )
                } else {
                    if !self.quiet {
                        eprintln!("» {summary}");
                    }
                    self.emit(UiEvent::ToolStart {
                        run: run_id,
                        agent: self.ident.agent_id.clone(),
                        req: req_id,
                        call: call_id.clone(),
                        name: name.to_string(),
                        summary: summary.clone(),
                    });
                    let args = plan.as_ref().expect("batch_ok implies parsed");
                    // Live output tap (bash only): a bounded channel +
                    // forwarder keeps ToolOut strictly ordered before
                    // ToolDone while never blocking the child's pipes.
                    let (obs, fwd) = if name == "bash" {
                        let (o, mut rx) = tools::bash::Observer::new(64);
                        let (sink, a, c) = (
                            self.events.clone(),
                            self.ident.agent_id.clone(),
                            call_id.clone(),
                        );
                        let fwd = tokio::spawn(async move {
                            match sink {
                                Some(tx) => {
                                    while let Some(ch) = rx.recv().await {
                                        // coalesce: fold immediately-pending
                                        // same-stream chunks into one event —
                                        // the UI sees fewer, larger updates
                                        let mut batch = vec![ch];
                                        while batch.len() < 16 {
                                            match rx.try_recv() {
                                                Ok(c) => batch.push(c),
                                                Err(_) => break,
                                            }
                                        }
                                        let mut merged: Vec<tools::bash::OutChunk> = vec![];
                                        for c in batch {
                                            if let Some(last) = merged.last_mut() {
                                                if last.err == c.err
                                                    && last.text.len() + c.text.len() <= 16_384
                                                {
                                                    last.text.push_str(&c.text);
                                                    continue;
                                                }
                                            }
                                            merged.push(c);
                                        }
                                        for ch in merged {
                                            let _ = tx.send(UiEvent::ToolOut {
                                                run: run_id,
                                                agent: a.clone(),
                                                call: c.clone(),
                                                err: ch.err,
                                                text: ch.text,
                                            });
                                        }
                                    }
                                }
                                None => while rx.recv().await.is_some() {},
                            }
                        });
                        (Some(o), Some(fwd))
                    } else {
                        (None, None)
                    };
                    let r = match tools::execute(
                        &self.tools,
                        name,
                        args,
                        cancel_wait(self.cancel.clone()),
                        obs,
                    )
                    .await
                    {
                        Ok(r) => r,
                        Err(e) => tools::ExecOut::plain(
                            format!("status: error\nerror: {e:#}"),
                            tools::ExecKind::Error,
                        ),
                    };
                    if let Some(f) = fwd {
                        let _ = f.await; // flush remaining preview chunks first
                    }
                    let mut d = Disp::new(
                        r.text,
                        match r.kind {
                            tools::ExecKind::Success => crate::events::ToolStatus::Ok,
                            tools::ExecKind::Failed => crate::events::ToolStatus::Failed,
                            tools::ExecKind::Error => crate::events::ToolStatus::Error,
                            tools::ExecKind::Timeout => crate::events::ToolStatus::Timeout,
                            tools::ExecKind::Cancelled => crate::events::ToolStatus::Cancelled,
                        },
                    );
                    d.exit = r.exit;
                    d.truncated = r.truncated;
                    d.dropped = r.preview_dropped;
                    d.executed = true;
                    d
                };
                // Every call gets a terminal event — the UI updates by id
                // and never leaves a row spinning.
                self.emit(UiEvent::ToolDone {
                    run: run_id,
                    agent: self.ident.agent_id.clone(),
                    call: call_id,
                    name: name.to_string(),
                    ms: t_tool.elapsed().as_millis(),
                    status: disp.status,
                    exit: disp.exit,
                    result: if disp.text.len() > 8000 {
                        format!("{}…", &disp.text[..floor_char(&disp.text, 8000)])
                    } else {
                        disp.text.clone()
                    },
                    truncated: disp.truncated,
                    dropped: disp.dropped,
                });
                self.journal.log(
                    "tool",
                    json!({
                        "tool_call_id": call.id,
                        "name": name,
                        "args": call.function.arguments,
                        "executed": disp.executed,
                        "status": disp.status.label(),
                        "exit_code": disp.exit,
                        "execution_ms": t_tool.elapsed().as_millis(),
                        "result": disp.text,
                    }),
                );
                let cancelled = disp.status == crate::events::ToolStatus::Cancelled;
                self.history.push(Message::Tool {
                    tool_call_id: call.id.clone(),
                    content: disp.text,
                });
                if cancelled {
                    if !self.quiet {
                        eprintln!("\n· interrupted during tool execution");
                    }
                    self.journal.log(
                        "interrupted",
                        json!({ "request_id": req_id, "phase": "tool" }),
                    );
                    return Ok(());
                }
                if finish_after {
                    // every remaining call still needs a paired tool response
                    for (j, rest) in outcome.tool_calls[i + 1..].iter().enumerate() {
                        let rest_id = if rest.id.is_empty() {
                            format!("r{req_id}.{}", i + 1 + j)
                        } else {
                            rest.id.clone()
                        };
                        self.emit(UiEvent::ToolDone {
                            run: run_id,
                            agent: self.ident.agent_id.clone(),
                            call: rest_id,
                            name: rest.function.name.clone(),
                            ms: 0,
                            status: crate::events::ToolStatus::Skipped,
                            exit: None,
                            result: "status: skipped\nerror: turn ended by submission".into(),
                            truncated: false,
                            dropped: 0,
                        });
                        self.history.push(Message::Tool {
                            tool_call_id: rest.id.clone(),
                            content: "status: skipped\nerror: turn ended by submission".into(),
                        });
                    }
                    return Ok(());
                }
            }
        }
        if !self.quiet {
            eprintln!("· max_turns reached; stopping");
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn trace(
        &self,
        request_id: u64,
        assembly_ms: u128,
        est_tokens: usize,
        request_fp: &str,
        usage: Option<&crate::types::Usage>,
        returned_model: Option<&str>,
        finish_reason: Option<&str>,
        first_delta_ms: u128,
        total_ms: u128,
        error: Option<&str>,
    ) -> Value {
        json!({
            "request_id": request_id,
            "session_id": self.ident.session_id,
            "agent_id": self.ident.agent_id,
            "role": self.ident.role,
            "provider_profile": self.ident.base_url,
            "requested_model": self.ident.model,
            "returned_model": returned_model,
            "epoch_id": "E0",
            "static_prefix_hash": self.hashes.static_prefix,
            "tool_schema_hash": self.hashes.tool_schema,
            "epoch_prefix_hash": self.hashes.epoch_prefix,
            "request_fingerprint": request_fp,
            "cache_policy": "implicit",
            "cache_controls_sent": if self.ident.cache_key_fingerprint.is_some() {
                json!(["prompt_cache_key"])
            } else {
                json!([])
            },
            "cache_key_fingerprint": self.ident.cache_key_fingerprint,
            "input_size_estimate": est_tokens,
            "estimate_method": "chars/4",
            "usage": usage.map(|u| json!({
                "input_tokens": u.input_tokens,
                "cache_read_tokens": u.cache_read_tokens,
                "cache_write_tokens": u.cache_write_tokens,
                "output_tokens": u.output_tokens,
                "complete": u.complete,
            })),
            "timing": {
                "context_assembly_ms": assembly_ms,
                "first_delta_ms": first_delta_ms,
                "request_total_ms": total_ms,
            },
            "finish_reason": finish_reason,
            "error_class": error,
        })
    }
}

/// Cancellation wait: Ctrl-C (real signal) OR the UI/stop notify.
async fn cancel_wait(notify: Option<Arc<tokio::sync::Notify>>) {
    match notify {
        Some(n) => {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = n.notified() => {}
            }
        }
        None => {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

fn needs_approval(name: &str) -> bool {
    matches!(name, "write_file" | "edit_file" | "bash")
}

fn summarize(name: &str, args: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    match name {
        "bash" => format!("bash: {}", v["command"].as_str().unwrap_or("")),
        "read_file" => format!("read {}", v["path"].as_str().unwrap_or("")),
        "write_file" => format!("write {}", v["path"].as_str().unwrap_or("")),
        "edit_file" => format!("edit {}", v["path"].as_str().unwrap_or("")),
        _ => format!("{name} {args}"),
    }
}

fn opt(v: Option<u64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "?".into())
}

fn error_class(e: &anyhow::Error) -> &'static str {
    let m = format!("{e:#}");
    if m.contains("deadline") {
        "deadline_exceeded"
    } else if m.contains("provider http") {
        "http_error"
    } else if m.contains("stream error") {
        "stream_error"
    } else if m.contains("interrupted") {
        "stream_interrupted"
    } else {
        "transport_error"
    }
}
