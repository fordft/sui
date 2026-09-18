//! Run export: persisted JSONL journals → sanitized Markdown/JSON report.
//!
//! Reads `~/.local/share/sui/runs/<run-id>/*.jsonl` incrementally and
//! writes `~/.local/share/sui/exports/<run-id>/report.{md,json}`.
//! Deterministic — no model calls, no network, no workspace writes.
//! The TUI and `sui export` share this module.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq)]
pub enum Format {
    Markdown,
    Json,
}

pub struct ExportOpts {
    /// Run directory name (or unique prefix). `latest` picks for us.
    pub run_id: Option<String>,
    /// `--latest`: newest run dir whose recorded workspace matches.
    pub latest_for_workspace: Option<PathBuf>,
    pub format: Format,
    pub include_diff: bool,
    /// Override roots (tests); defaults under the user's data dir.
    pub runs_root: Option<PathBuf>,
    pub out_root: Option<PathBuf>,
    /// Caller knows the run is live (TUI); snapshot is labeled partial.
    pub running: bool,
}

fn runs_root(o: &ExportOpts) -> PathBuf {
    o.runs_root
        .clone()
        .unwrap_or_else(|| dirs_home().join(".local/share/sui/runs"))
}
fn out_root(o: &ExportOpts) -> PathBuf {
    o.out_root
        .clone()
        .unwrap_or_else(|| dirs_home().join(".local/share/sui/exports"))
}
fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Resolve the run directory: explicit id/prefix, or newest matching the
/// workspace hint (session events record it; falls back to newest).
fn resolve_run_dir(o: &ExportOpts) -> Result<(String, PathBuf)> {
    let root = runs_root(o);
    if !root.is_dir() {
        bail!("no runs recorded yet ({})", root.display());
    }
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&root)?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir() && has_jsonl(p))
        .collect();
    dirs.sort_by_key(|p| {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH)
    });
    if let Some(id) = &o.run_id {
        if let Some(p) = dirs
            .iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(id.as_str()))
        {
            return Ok((id.clone(), p.clone()));
        }
        let hits: Vec<_> = dirs
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(id.as_str()))
                    .unwrap_or(false)
            })
            .collect();
        return match hits.len() {
            1 => Ok((
                hits[0].file_name().unwrap().to_string_lossy().to_string(),
                hits[0].clone(),
            )),
            0 => bail!("no run matching '{id}' under {}", root.display()),
            _ => bail!("run id '{id}' is ambiguous ({} matches)", hits.len()),
        };
    }
    // --latest: prefer the newest dir whose session workspace matches
    if let Some(ws) = &o.latest_for_workspace {
        let want = ws.canonicalize().unwrap_or_else(|_| ws.clone());
        for p in dirs.iter().rev() {
            if let Some(recorded) = recorded_workspace(p) {
                let rec = PathBuf::from(&recorded)
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(&recorded));
                if rec == want {
                    return Ok((
                        p.file_name().unwrap().to_string_lossy().to_string(),
                        p.clone(),
                    ));
                }
            }
        }
    }
    dirs.last()
        .map(|p| {
            (
                p.file_name().unwrap().to_string_lossy().to_string(),
                p.clone(),
            )
        })
        .context("no runs recorded yet")
}

fn has_jsonl(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .any(|e| e.path().extension().and_then(|x| x.to_str()) == Some("jsonl"))
        })
        .unwrap_or(false)
}

/// First `session` event's workspace in any journal under `dir`.
fn recorded_workspace(dir: &Path) -> Option<String> {
    for p in journal_files(dir) {
        if let Ok(f) = std::fs::File::open(&p) {
            for line in BufReader::new(f).lines().take(60).flatten() {
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v["type"] == crate::journal::ev::SESSION {
                    if let Some(w) = v["data"]["workspace"].as_str() {
                        return Some(w.to_string());
                    }
                }
            }
        }
    }
    None
}

/// Journals in a stable order: control/session journals first, workers after.
fn journal_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("jsonl"))
                .collect()
        })
        .unwrap_or_default();
    let rank = |p: &PathBuf| match p.file_name().and_then(|n| n.to_str()).unwrap_or("") {
        "mission.jsonl" => 0,
        "solo.jsonl" => 1,
        "orchestrator.jsonl" => 2,
        "auditor.jsonl" => 3,
        "escalation.jsonl" => 4,
        _ => 5,
    };
    files.sort_by_key(|p| {
        (
            rank(p),
            p.file_name().map(|n| n.to_string_lossy().to_string()),
        )
    });
    files
}

// ── redaction ─────────────────────────────────────────────────────────

pub struct Redactor {
    /// literal secret values pulled from config (api_key fields, env
    /// vars named by key_env) — masked wherever they appear verbatim
    literals: Vec<String>,
    /// category → count of replacements (for the report, never values)
    pub counts: Map<String, Value>,
}

impl Redactor {
    /// `extra_literals`: known secret values collected by the caller
    /// (config api_keys, resolved key_env values).
    pub fn new(extra_literals: Vec<String>) -> Self {
        let literals = extra_literals
            .into_iter()
            .filter(|s| s.len() >= 8)
            .collect();
        Self {
            literals,
            counts: Map::new(),
        }
    }

    fn bump(&mut self, cat: &str) {
        let n = self.counts.get(cat).and_then(|v| v.as_u64()).unwrap_or(0);
        self.counts.insert(cat.to_string(), json!(n + 1));
    }

    /// Replace literal secret values first (exact match), then apply the
    /// pattern rules. Returns the sanitized text.
    pub fn text(&mut self, s: &str) -> String {
        let mut s = s.to_string();
        for lit in self.literals.clone() {
            if s.contains(&lit) {
                s = s.replace(&lit, "«redacted:secret»");
                self.bump("literal secret value");
            }
        }
        for (re, cat, repl) in rules() {
            if re.is_match(&s) {
                s = re.replace_all(&s, *repl).to_string();
                self.bump(cat);
            }
        }
        strip_control(&mut s);
        s
    }

    /// Recursively sanitize every string in a JSON value.
    pub fn value(&mut self, v: &mut Value) {
        match v {
            Value::String(s) => {
                let t = self.text(s);
                *s = t;
            }
            Value::Array(a) => a.iter_mut().for_each(|x| self.value(x)),
            Value::Object(m) => m.values_mut().for_each(|x| self.value(x)),
            _ => {}
        }
    }
}

fn strip_control(s: &mut String) {
    // drop CSI/OSC/C0 so reports stay readable text, not terminal control
    let cleaned: String = s
        .chars()
        .filter(|&c| {
            !(c == '\u{1b}'
                || (c < '\u{20}' && !matches!(c, '\n' | '\t' | '\r'))
                || ('\u{7f}'..='\u{9f}').contains(&c))
        })
        .collect();
    *s = cleaned;
}

// (pattern, category, replacement) — applied in order to every string.
use std::sync::OnceLock;
static COMPILED: OnceLock<Vec<(regex::Regex, &'static str, &'static str)>> = OnceLock::new();

fn rules() -> &'static [(regex::Regex, &'static str, &'static str)] {
    COMPILED.get_or_init(|| {
        let pats: &[(&str, &str, &str)] = &[
            // authorization headers / bearer tokens
            (r"(?i)(authorization|x-api-key|x-auth-token)\s*[:=]\s*\S+", "auth header", "$1: «redacted:auth»"),
            (r"(?i)bearer\s+[A-Za-z0-9._~+/=-]{8,}", "bearer token", "Bearer «redacted:token»"),
            (r"(?i)cookie\s*[:=]\s*[^\n;]+", "cookie", "Cookie: «redacted:cookie»"),
            // well-known token shapes
            (r"\b(?:sk|pk|rk|key|ds|or)[-_][A-Za-z0-9]{12,}", "api key", "«redacted:api-key»"),
            (r"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{20,}", "github token", "«redacted:gh-token»"),
            (r"\bgithub_pat_[A-Za-z0-9_]{20,}", "github token", "«redacted:gh-token»"),
            (r"\bglpat-[A-Za-z0-9_-]{15,}", "gitlab token", "«redacted:gl-token»"),
            (r"\bxox[baprs]-[A-Za-z0-9-]{10,}", "slack token", "«redacted:slack-token»"),
            (r"\bAKIA[0-9A-Z]{16}", "aws key id", "«redacted:aws-key»"),
            (r"\bAIza[0-9A-Za-z_-]{20,}", "gcp key", "«redacted:gcp-key»"),
            (r"\beyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{5,}", "jwt", "«redacted:jwt»"),
            (r"-----BEGIN [A-Z ]*PRIVATE KEY-----[^-]*-----END [A-Z ]*PRIVATE KEY-----", "private key", "«redacted:private-key»"),
            // userinfo in URLs: scheme://user:pass@host
            (r"([a-z][a-z0-9+.-]*://)[^/\s:@]+:[^/\s@]+@", "url credential", "$1«redacted:url-cred»@"),
            // generic secret assignments: KEY/TOKEN/SECRET/PASS = value
            (r#"(?i)\b([A-Za-z_][A-Za-z0-9_]*(?:key|token|secret|pass|credential)[A-Za-z0-9_]*)\s*[:=]\s*["']?[^\s"',}]{8,}"#, "secret assignment", "$1=«redacted:secret»"),
        ];
        pats.iter()
            .map(|(p, c, r)| (regex::Regex::new(p).expect("redaction rule compiles"), *c, *r))
            .collect()
    })
}

// ── report build ──────────────────────────────────────────────────────

struct Ctx {
    limitations: Vec<String>,
    malformed_lines: u64,
}

/// Parse one journal file into ordered, lightly-typed events. Malformed
/// lines are counted and surfaced; a truncated final line is expected
/// on active runs and tolerated.
fn scan_journal(path: &Path, ctx: &mut Ctx) -> Vec<Value> {
    let f = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return vec![],
    };
    let mut out = Vec::new();
    let mut last_line = 0usize;
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = match line {
            Ok(l) => l,
            Err(_) => {
                ctx.malformed_lines += 1;
                continue;
            }
        };
        last_line = i;
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Value>(&line) {
            Ok(v) => out.push(v),
            Err(_) => {
                ctx.malformed_lines += 1;
            }
        }
    }
    let _ = last_line;
    out
}

/// Tool result status → reviewer-facing verdict. `executed` alone is
/// never success: the status line and exit code decide.
fn tool_verdict(executed: bool, result: &str) -> &'static str {
    let status = result.lines().next().unwrap_or("");
    match status.trim() {
        "status: denied" => "Denied",
        "status: cancelled" => "Cancelled",
        "status: timeout" => "Timed out",
        "status: skipped" => "Skipped",
        "status: failed" => "Executed but failed",
        s if s.starts_with("status: error") && !executed => "Skipped (batch rejected)",
        s if s.starts_with("status: error") => "Executed but failed",
        s if s.starts_with("status:") && executed => "Executed successfully",
        // no `status:` line + not executed = the control-plane interceptor
        // (submit_result) consumed it — that IS the submission mechanism
        _ if !executed => "Intercepted (control plane)",
        _ => "Executed",
    }
}

fn field_str(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(String::from)
}

pub fn run_export(o: &ExportOpts) -> Result<PathBuf> {
    let (run_id, dir) = resolve_run_dir(o)?;
    let mut ctx = Ctx {
        limitations: vec![],
        malformed_lines: 0,
    };
    let mut red = Redactor::new(known_secrets());

    // ── per-journal scan ────────────────────────────────────────────
    let mut session: Option<Value> = None;
    let mut plan: Option<Value> = None;
    let mut accepted: Option<Value> = None;
    let mut result_ev: Option<Value> = None;
    let mut mission_states: Vec<(u64, String)> = vec![];
    let mut gates: Vec<Value> = vec![];
    let mut task_results: Vec<Value> = vec![];
    let mut audits: Vec<Value> = vec![];
    let mut agents: Vec<AgentRec> = vec![];

    for jf in journal_files(&dir) {
        let jname = jf.file_name().unwrap().to_string_lossy().to_string();
        let evs = scan_journal(&jf, &mut ctx);
        if evs.is_empty() && jname != "mission.jsonl" {
            continue;
        }
        let mut agent = AgentRec {
            journal: jname.clone(),
            agent_id: jname.trim_end_matches(".jsonl").to_string(),
            role: None,
            provider: None,
            requested_model: None,
            returned_models: vec![],
            timeline: vec![],
            usage: Usage::default(),
            requests: 0,
            telemetry_complete: 0,
        };
        for e in evs {
            let ts = e["ts_unix"].as_u64().unwrap_or(0);
            let d = &e["data"];
            match e["type"].as_str().unwrap_or("") {
                crate::journal::ev::SESSION => {
                    // run-level metadata — goes to the overview, not an
                    // agent timeline (mission.jsonl would otherwise render
                    // as a phantom zero-request agent)
                    if session.is_none() {
                        session = Some(d.clone());
                    }
                }
                "task" => agent.timeline.push(tl(ts, "task", d.clone())),
                "task_done" => agent.timeline.push(tl(ts, "task_done", d.clone())),
                "acp_model" => agent.timeline.push(tl(ts, "acp_model", d.clone())),
                "journal_error" => agent.timeline.push(tl(ts, "journal_error", d.clone())),
                "user" => agent.timeline.push(tl(ts, "user", d.clone())),
                "assistant" => {
                    let mut d = d.clone();
                    // reasoning content is private — never exported
                    if d.get("reasoning_content")
                        .map(|r| !r.is_null())
                        .unwrap_or(false)
                    {
                        d.as_object_mut().map(|m| m.remove("reasoning_content"));
                        red.bump("model reasoning (omitted)");
                    }
                    agent.timeline.push(tl(ts, "assistant", d));
                }
                "request" => {
                    agent.requests += 1;
                    if agent.role.is_none() {
                        agent.role = field_str(d, "role");
                    }
                    if agent.provider.is_none() {
                        agent.provider = field_str(d, "provider_profile");
                    }
                    if agent.requested_model.is_none() {
                        agent.requested_model = field_str(d, "requested_model");
                    }
                    if let Some(aid) = field_str(d, "agent_id") {
                        agent.agent_id = aid;
                    }
                    if let Some(m) = field_str(d, "returned_model") {
                        if !agent.returned_models.contains(&m) {
                            agent.returned_models.push(m);
                        }
                    }
                    let u = &d["usage"];
                    if u.is_object() {
                        if u["complete"] == true {
                            agent.telemetry_complete += 1;
                        }
                        for (dst, k) in [
                            (&mut agent.usage.input, "input_tokens"),
                            (&mut agent.usage.cache_read, "cache_read_tokens"),
                            (&mut agent.usage.cache_write, "cache_write_tokens"),
                            (&mut agent.usage.output, "output_tokens"),
                        ] {
                            if let Some(n) = u[k].as_u64() {
                                *dst = Some(dst.unwrap_or(0) + n);
                            }
                        }
                    }
                    // keep a compact request record for linkage
                    agent.timeline.push(tl(
                        ts,
                        "request",
                        json!({
                            "request_id": d["request_id"],
                            "model": d["returned_model"].as_str().or(d["requested_model"].as_str()),
                            "finish_reason": d["finish_reason"],
                            "error_class": d["error_class"],
                            "usage": u,
                            "timing": d["timing"],
                            "request_fingerprint": d["request_fingerprint"],
                            "static_prefix_hash": d["static_prefix_hash"],
                            "cache_key_fingerprint": d["cache_key_fingerprint"],
                        }),
                    ));
                }
                "tool" => agent.timeline.push(tl(ts, "tool", d.clone())),
                "warn" | "budget_exceeded" | "interrupted" => {
                    agent
                        .timeline
                        .push(tl(ts, e["type"].as_str().unwrap(), d.clone()))
                }
                "mission" => {
                    mission_states.push((ts, field_str(d, "state").unwrap_or_else(|| "?".into())));
                    if let Some(why) = field_str(d, "why") {
                        mission_states
                            .last_mut()
                            .map(|(_, s)| *s = format!("{s} — {why}"));
                    }
                }
                "plan" => plan = Some(d.clone()),
                "accepted" => accepted = Some(d.clone()),
                "result" => result_ev = Some(d.clone()),
                "audit" => audits.push(d.clone()),
                "gate" => gates.push(d.clone()),
                "task_result" => task_results.push(d.clone()),
                _ => {}
            }
        }
        if !(agent.timeline.is_empty() && agent.requests == 0) {
            agents.push(agent);
        }
    }

    // ── overview fields ─────────────────────────────────────────────
    let workspace = session
        .as_ref()
        .and_then(|s| field_str(s, "workspace"))
        .unwrap_or_else(|| "Not recorded.".into());
    let approval = session
        .as_ref()
        .and_then(|s| field_str(s, "approval"))
        .unwrap_or_else(|| "Not recorded.".into());
    if session.is_none() {
        ctx.limitations
            .push("no session event — workspace, approval mode and version not recorded".into());
    }
    let mode = session
        .as_ref()
        .and_then(|s| field_str(s, "mode"))
        .unwrap_or_else(|| {
            if plan.is_some() || !mission_states.is_empty() {
                "mission".into()
            } else {
                "solo".into()
            }
        });
    let started = journal_files(&dir)
        .iter()
        .filter_map(|p| std::fs::File::open(p).ok())
        .find_map(|f| {
            BufReader::new(f).lines().find_map(|l| {
                l.ok()
                    .and_then(|l| serde_json::from_str::<Value>(&l).ok())
                    .and_then(|v| v["ts_unix"].as_u64())
            })
        });
    let last_ts = agents
        .iter()
        .flat_map(|a| a.timeline.iter().filter_map(|e| e["ts_unix"].as_u64()))
        .chain(mission_states.iter().map(|(t, _)| *t))
        .max();
    let outcome = result_ev
        .as_ref()
        .and_then(|r| field_str(r, "outcome"))
        .or_else(|| mission_states.last().map(|(_, s)| s.clone()))
        .or_else(|| {
            agents
                .iter()
                .flat_map(|a| a.timeline.iter())
                .rev()
                .find_map(|e| {
                    if e["kind"] == "task_done" {
                        field_str(&e["data"], "outcome")
                    } else {
                        None
                    }
                })
        })
        .unwrap_or_else(|| "Not recorded.".into());

    if ctx.malformed_lines > 0 {
        ctx.limitations.push(format!(
            "{} malformed journal line(s) skipped (a truncated final line is expected on live runs)",
            ctx.malformed_lines
        ));
    }

    // ── usage aggregates ────────────────────────────────────────────
    let mut total = Usage::default();
    let mut total_req = 0u64;
    let mut total_complete = 0u64;
    for a in &agents {
        total_req += a.requests;
        total_complete += a.telemetry_complete;
        total.add(&a.usage);
    }

    // ── diff (opt-in) ───────────────────────────────────────────────
    let mut diff_sec: Option<Value> = None;
    if o.include_diff {
        let ws_path = if workspace == "Not recorded." {
            None
        } else {
            Some(PathBuf::from(&workspace))
        };
        let base = plan.as_ref().and_then(|p| field_str(p, "base_commit"));
        let head = accepted
            .as_ref()
            .and_then(|a| field_str(a, "sha"))
            .or_else(|| {
                result_ev
                    .as_ref()
                    .and_then(|r| field_str(r, "accepted_sha"))
            });
        match (ws_path, base, head) {
            (Some(ws), Some(b), Some(h)) if ws.is_dir() => {
                diff_sec = Some(git_diff(&ws, &b, &h, &mut ctx));
            }
            _ => ctx.limitations.push(
                "--include-diff requested but the recorded base/candidate revisions \
                 are unavailable (solo runs track no base, or the mission did not accept)"
                    .into(),
            ),
        }
    }

    // ── assemble report value ───────────────────────────────────────
    let report = json!({
        "run_id": run_id,
        "sui_version": session.as_ref().and_then(|s| field_str(s, "sui_version"))
            .unwrap_or_else(|| env!("CARGO_PKG_VERSION").into()),
        "os_arch": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        "mode": mode,
        "status": if o.running { "RUNNING — partial snapshot".to_string() } else { outcome.clone() },
        "workspace": workspace,
        "approval_mode": approval,
        "started_unix_ms": started,
        "last_event_unix_ms": last_ts,
        "captured_through": last_ts.map(|t| json!(t)).unwrap_or(Value::Null),
        "objective": objective_from(&agents, plan.as_ref()),
        "agents": agents.iter().map(|a| json!({
            "agent_id": a.agent_id, "journal": a.journal,
            "role": a.role, "provider": a.provider,
            "requested_model": a.requested_model,
            "returned_models": a.returned_models,
            "requests": a.requests,
            // self-declared completion state — evidence of the claim,
            // never proof; absent when the agent didn't declare one
            "declared": a.timeline.iter().rev()
                .find(|e| e["kind"] == "assistant")
                .and_then(|e| e["data"]["content"].as_str())
                .and_then(crate::charter::declared_state)
                .map(|d| d.name()),
        })).collect::<Vec<_>>(),
        "plan": plan,
        "mission_states": mission_states.iter().map(|(t, s)| json!({"ts_unix": t, "state": s})).collect::<Vec<_>>(),
        "timeline": agents.iter().map(|a| json!({
            "agent_id": a.agent_id, "journal": a.journal, "events": a.timeline,
        })).collect::<Vec<_>>(),
        "gates": gates,
        "task_results": task_results,
        "audits": audits,
        "accepted": accepted,
        "diff": diff_sec,
        "usage": {
            "per_agent": agents.iter().map(|a| json!({
                "agent_id": a.agent_id, "requests": a.requests,
                "input_tokens": a.usage.input, "cache_read_tokens": a.usage.cache_read,
                "cache_write_tokens": a.usage.cache_write, "output_tokens": a.usage.output,
            })).collect::<Vec<_>>(),
            "totals": {
                "requests": total_req,
                "input_tokens": total.input, "cache_read_tokens": total.cache_read,
                "cache_write_tokens": total.cache_write, "output_tokens": total.output,
            },
            "telemetry": format!("{total_complete}/{total_req} requests reported complete usage"),
        },
        "limitations": ctx.limitations,
    });
    let mut report = report;
    red.value(&mut report); // redact every string in the assembled report
                            // counts go in last — they must reflect the final pass itself
    report["redactions"] = json!(red.counts);

    // ── write ───────────────────────────────────────────────────────
    let outdir = out_root(o).join(&run_id);
    std::fs::create_dir_all(&outdir)?;
    let (file, body) = match o.format {
        Format::Json => (
            outdir.join("report.json"),
            serde_json::to_string_pretty(&report)?,
        ),
        Format::Markdown => (outdir.join("report.md"), render_md(&report)),
    };
    write_private(&file, &body)?;
    Ok(file)
}

fn write_private(path: &Path, body: &str) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    use std::io::Write;
    f.write_all(body.as_bytes())?;
    Ok(())
}

fn tl(ts: u64, kind: &str, data: Value) -> Value {
    json!({ "ts_unix": ts, "kind": kind, "data": data })
}

#[derive(Default)]
struct Usage {
    input: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
}
impl Usage {
    fn add(&mut self, o: &Usage) {
        for (dst, v) in [
            (&mut self.input, o.input),
            (&mut self.cache_read, o.cache_read),
            (&mut self.cache_write, o.cache_write),
            (&mut self.output, o.output),
        ] {
            if let Some(n) = v {
                *dst = Some(dst.unwrap_or(0) + n);
            }
        }
    }
}

struct AgentRec {
    journal: String,
    agent_id: String,
    role: Option<String>,
    provider: Option<String>,
    requested_model: Option<String>,
    returned_models: Vec<String>,
    timeline: Vec<Value>,
    usage: Usage,
    requests: u64,
    telemetry_complete: u64,
}

/// User objective: the first submitted task — solo `task`/`user`, else
/// the plan objective.
fn objective_from(agents: &[AgentRec], plan: Option<&Value>) -> Value {
    if let Some(p) = plan {
        if let Some(o) = field_str(p, "objective") {
            return json!(o);
        }
    }
    for a in agents {
        for e in &a.timeline {
            if e["kind"] == "task" {
                if let Some(t) = field_str(&e["data"], "task") {
                    return json!(t);
                }
            }
            if e["kind"] == "user" {
                if let Some(t) = field_str(&e["data"], "content") {
                    return json!(t);
                }
            }
        }
    }
    json!("Not recorded.")
}

/// Secret values already known locally (config api_key + key_env
/// variables) — literal-masked wherever they appear.
fn known_secrets() -> Vec<String> {
    let mut v = vec![];
    if let Ok(profiles) = crate::config::profiles(None) {
        for p in profiles.values() {
            if let Some(k) = &p.api_key {
                v.push(k.clone());
            }
            if let Some(e) = &p.key_env {
                if let Ok(k) = std::env::var(e) {
                    v.push(k);
                }
            }
        }
    }
    v
}

fn git_diff(ws: &Path, base: &str, head: &str, ctx: &mut Ctx) -> Value {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["diff", "--stat", &format!("{base}..{head}")])
        .output();
    let stat = match &out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).to_string(),
        _ => {
            ctx.limitations
                .push("git diff --stat failed — revisions may be gone".into());
            "Not recorded.".into()
        }
    };
    let full = std::process::Command::new("git")
        .arg("-C")
        .arg(ws)
        .args(["diff", &format!("{base}..{head}")])
        .output();
    let (diff, truncated) = match full {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout).to_string();
            if s.len() > 200_000 {
                (
                    format!(
                        "{}…<export truncated at 200KB>",
                        &s[..crate::context::floor_char_boundary(&s, 200_000)]
                    ),
                    true,
                )
            } else {
                (s, false)
            }
        }
        _ => ("Not recorded.".into(), false),
    };
    json!({
        "base": base, "head": head, "stat": stat,
        "diff": diff, "truncated_by_export": truncated,
    })
}

// ── markdown render ───────────────────────────────────────────────────

fn md_str(v: &Value) -> String {
    v.as_str().map(String::from).unwrap_or_else(|| {
        if v.is_null() {
            "Unknown".into()
        } else {
            v.to_string()
        }
    })
}

fn render_md(r: &Value) -> String {
    let mut m = String::new();
    m.push_str(&format!(
        "# Sui run report — {}\n\n",
        r["run_id"].as_str().unwrap_or("?")
    ));
    m.push_str("> Review before sharing: this report may contain project code and commands.\n\n");

    m.push_str("## Run overview\n\n");
    for (k, label) in [
        ("sui_version", "Sui version"),
        ("os_arch", "OS/arch"),
        ("mode", "Mode"),
        ("status", "Status"),
        ("workspace", "Workspace"),
        ("approval_mode", "Approval mode"),
    ] {
        m.push_str(&format!("- **{}:** {}\n", label, md_str(&r[k])));
    }
    if let Some(t) = r["started_unix_ms"].as_u64() {
        m.push_str(&format!("- **Started:** {} (unix ms)\n", t));
    }
    if let Some(t) = r["last_event_unix_ms"].as_u64() {
        m.push_str(&format!("- **Last event:** {} (unix ms)\n", t));
    }
    m.push('\n');

    m.push_str("## Task and agents\n\n");
    m.push_str(&format!("**Objective:** {}\n\n", md_str(&r["objective"])));
    if let Some(ags) = r["agents"].as_array() {
        for a in ags {
            m.push_str(&format!(
                "- `{}` — role: {} · provider: {} · model: {} → {} · {} requests\n",
                a["agent_id"].as_str().unwrap_or("?"),
                md_str(&a["role"]),
                md_str(&a["provider"]),
                md_str(&a["requested_model"]),
                a["returned_models"]
                    .as_array()
                    .map(|v| v
                        .iter()
                        .map(|x| x.as_str().unwrap_or("?"))
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default(),
                a["requests"].as_u64().unwrap_or(0),
            ));
            if let Some(d) = a["declared"].as_str() {
                m.push_str(&format!(
                    "  - declared: `{d}` (self-reported, not verified)
"
                ));
            }
        }
    }
    if let Some(plan) = r["plan"].as_object() {
        m.push_str("\n### Mission plan\n\n");
        m.push_str(&format!(
            "- base commit: `{}`\n",
            md_str(&plan["base_commit"])
        ));
        if let Some(tasks) = plan["tasks"].as_array() {
            for t in tasks {
                m.push_str(&format!(
                    "- **{}** — {} — owns `{}` — deps: `{}` — acceptance: {} cmd(s)\n",
                    md_str(&t["id"]),
                    md_str(&t["objective"]),
                    t["owned_paths"]
                        .as_array()
                        .map(|a| a
                            .iter()
                            .map(|x| x.as_str().unwrap_or(""))
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default(),
                    t["depends_on"]
                        .as_array()
                        .map(|a| a
                            .iter()
                            .map(|x| x.as_str().unwrap_or(""))
                            .collect::<Vec<_>>()
                            .join(", "))
                        .unwrap_or_default(),
                    t["acceptance"].as_array().map(|a| a.len()).unwrap_or(0),
                ));
            }
        }
    }
    if let Some(states) = r["mission_states"].as_array().filter(|s| !s.is_empty()) {
        m.push_str("\n**Mission transitions:** ");
        m.push_str(
            &states
                .iter()
                .map(|s| s["state"].as_str().unwrap_or("?").to_string())
                .collect::<Vec<_>>()
                .join(" → "),
        );
        m.push('\n');
    }
    m.push('\n');

    m.push_str("## Execution timeline\n\n");
    if let Some(tl_groups) = r["timeline"].as_array() {
        for g in tl_groups {
            m.push_str(&format!(
                "### Agent `{}` (journal: {})\n\n",
                g["agent_id"].as_str().unwrap_or("?"),
                g["journal"].as_str().unwrap_or("?"),
            ));
            if let Some(evs) = g["events"].as_array() {
                for e in evs {
                    render_event(&mut m, e);
                }
            }
            m.push('\n');
        }
    }

    m.push_str("## Validation and audit\n\n");
    render_gates(&mut m, r);
    if let Some(trs) = r["task_results"].as_array().filter(|a| !a.is_empty()) {
        m.push_str("\n### Task results\n\n");
        for t in trs {
            m.push_str(&format!(
                "- **{}**: {} — sha `{}` — changed: {}\n",
                md_str(&t["id"]),
                if t["ok"] == true { "passed" } else { "failed" },
                md_str(&t["sha"]),
                t["changed"]
                    .as_array()
                    .map(|a| a
                        .iter()
                        .map(|x| x.as_str().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join(", "))
                    .unwrap_or_default(),
            ));
            if let Some(c) = t["capsule"].as_str().filter(|c| !c.is_empty()) {
                m.push_str(&format!(
                    "  - failure capsule:\n\n```\n{}\n```\n",
                    cap(c, 3000)
                ));
            }
            if let Some(gs) = t["gates"].as_array().filter(|g| !g.is_empty()) {
                for g in gs {
                    render_gate(&mut m, g);
                }
            }
        }
    }
    if let Some(auds) = r["audits"].as_array().filter(|a| !a.is_empty()) {
        m.push_str("\n### Auditor verdicts\n\n");
        for a in auds {
            m.push_str(&format!(
                "```json\n{}\n```\n",
                cap(&serde_json::to_string_pretty(a).unwrap_or_default(), 4000)
            ));
        }
    }
    m.push('\n');

    m.push_str("## Repository result\n\n");
    if let Some(acc) = r["accepted"].as_object() {
        m.push_str(&format!(
            "- accepted sha: `{}`\n- branch: `{}`\n",
            md_str(&acc["sha"]),
            md_str(&acc["branch"])
        ));
    } else {
        m.push_str(&format!(
            "- accepted sha: {}\n",
            md_str(&r["accepted"]["sha"])
        ));
    }
    if let Some(d) = r["diff"].as_object() {
        m.push_str(&format!(
            "\n### Diff `{}..{}`\n\n```diff\n{}\n```\n",
            md_str(&d["base"]),
            md_str(&d["head"]),
            md_str(&d["diff"])
        ));
        if d["truncated_by_export"] == true {
            m.push_str("_Diff truncated by export at 200KB._\n");
        }
    }
    m.push('\n');

    m.push_str("## Usage and timing\n\n");
    if let Some(rows) = r["usage"]["per_agent"].as_array() {
        m.push_str(
            "| agent | requests | in | cached | written | out |\n|---|---|---|---|---|---|\n",
        );
        for a in rows {
            m.push_str(&format!(
                "| {} | {} | {} | {} | {} | {} |\n",
                a["agent_id"].as_str().unwrap_or("?"),
                a["requests"].as_u64().unwrap_or(0),
                num_or_unknown(&a["input_tokens"]),
                num_or_unknown(&a["cache_read_tokens"]),
                num_or_unknown(&a["cache_write_tokens"]),
                num_or_unknown(&a["output_tokens"]),
            ));
        }
        m.push_str(&format!(
            "\nTotals ({} requests): in {} · cached {} · written {} · out {} — **{}**\n",
            r["usage"]["totals"]["requests"].as_u64().unwrap_or(0),
            num_or_unknown(&r["usage"]["totals"]["input_tokens"]),
            num_or_unknown(&r["usage"]["totals"]["cache_read_tokens"]),
            num_or_unknown(&r["usage"]["totals"]["cache_write_tokens"]),
            num_or_unknown(&r["usage"]["totals"]["output_tokens"]),
            r["usage"]["telemetry"].as_str().unwrap_or(""),
        ));
        m.push_str("\nCache counters are provider-reported; fingerprints in the timeline are diagnostics, not hit evidence. Cost: Unknown (no pricing recorded).\n");
    }
    m.push('\n');

    m.push_str("## Limitations\n\n");
    if let Some(lims) = r["limitations"].as_array() {
        if lims.is_empty() {
            m.push_str("- none recorded\n");
        }
        for l in lims {
            m.push_str(&format!("- {}\n", l.as_str().unwrap_or("?")));
        }
    }
    if let Some(red) = r["redactions"].as_object().filter(|m| !m.is_empty()) {
        m.push_str("\n### Redactions\n\n");
        for (k, v) in red {
            m.push_str(&format!(
                "- {}: {} occurrence(s)\n",
                k,
                v.as_u64().unwrap_or(0)
            ));
        }
        m.push_str("\n_Redaction is best-effort — review before sharing._\n");
    }
    m
}

fn render_event(m: &mut String, e: &Value) {
    let kind = e["kind"].as_str().unwrap_or("?");
    let d = &e["data"];
    match kind {
        "user" => {
            m.push_str(&format!(
                "**user:** {}\n\n",
                cap(&md_str(&d["content"]), 4000)
            ));
        }
        "task" => {
            m.push_str(&format!(
                "**task submitted** (approval: {})\n\n",
                md_str(&d["approval"])
            ));
        }
        "task_done" => {
            m.push_str(&format!("**task finished:** {}\n\n", md_str(&d["outcome"])));
        }
        "assistant" => {
            if let Some(c) = d["content"].as_str().filter(|c| !c.is_empty()) {
                m.push_str(&format!("**assistant:** {}\n\n", cap(c, 8000)));
            }
            if let Some(calls) = d["tool_calls"].as_array().filter(|c| !c.is_empty()) {
                m.push_str("_requested tools:_ ");
                m.push_str(
                    &calls
                        .iter()
                        .map(|c| c["function"]["name"].as_str().unwrap_or("?").to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                );
                m.push_str("\n\n");
            }
        }
        "tool" => {
            let verdict = tool_verdict(d["executed"] == true, d["result"].as_str().unwrap_or(""));
            m.push_str(&format!(
                "**tool `{}`** · {} · {}ms\n\n",
                d["name"].as_str().unwrap_or("?"),
                verdict,
                d["execution_ms"].as_u64().unwrap_or(0),
            ));
            let args = d["args"].as_str().unwrap_or("");
            m.push_str(&format!("```\n{}\n```\n", cap(args, 1500)));
            if let Some(res) = d["result"].as_str() {
                m.push_str(&format!("```\n{}\n```\n", cap(res, 8000)));
                if res.contains("truncated: true") {
                    m.push_str(
                        "_Output was truncated during capture; omitted content unavailable._\n",
                    );
                }
            }
            m.push('\n');
        }
        "journal_error" => {
            m.push_str(&format!(
                "<sub>journal_error: {} — evidence gap in this journal</sub>\n\n",
                md_str(&d["serialize_failed"]),
            ));
        }
        "acp_model" => {
            m.push_str(&format!(
                "<sub>acp model requested={} applied={}{}</sub>\n\n",
                md_str(&d["requested"]),
                d["applied"].as_bool().unwrap_or(false),
                d["error"]
                    .as_str()
                    .map(|e| format!(" error={e}"))
                    .unwrap_or_default(),
            ));
        }
        "request" => {
            m.push_str(&format!(
                "<sub>req#{} model={} finish={} in={} cached={} out={}</sub>\n\n",
                d["request_id"].as_u64().unwrap_or(0),
                md_str(&d["model"]),
                md_str(&d["finish_reason"]),
                num_or_unknown(&d["usage"]["input_tokens"]),
                num_or_unknown(&d["usage"]["cache_read_tokens"]),
                num_or_unknown(&d["usage"]["output_tokens"]),
            ));
        }
        "warn" | "budget_exceeded" | "interrupted" => {
            m.push_str(&format!(
                "**{}**: {}\n\n",
                kind,
                cap(&serde_json::to_string(d).unwrap_or_default(), 600)
            ));
        }
        crate::journal::ev::SESSION => {}
        _ => {}
    }
}

fn render_gates(m: &mut String, r: &Value) {
    let mut any = false;
    if let Some(gs) = r["gates"].as_array().filter(|g| !g.is_empty()) {
        m.push_str("### Runtime gates (integration checks)\n\n");
        for g in gs {
            render_gate(m, g);
        }
        any = true;
    }
    if !any {
        m.push_str("_No runtime gates recorded for this run._\n");
    }
}

fn render_gate(m: &mut String, g: &Value) {
    m.push_str(&format!(
        "- **{}** `{}` @ `{}` → exit {} · {}\n",
        md_str(&g["kind"]),
        md_str(&g["cmd"]),
        md_str(&g["cwd"]),
        g["exit_code"]
            .as_i64()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "?".into()),
        if g["ok"] == true { "pass" } else { "FAIL" },
    ));
    let err = md_str(&g["stderr_tail"]);
    let out = md_str(&g["stdout_tail"]);
    if err != "Unknown" && !err.is_empty() && g["ok"] != true {
        m.push_str(&format!("  stderr: `{}`\n", cap(&err, 800)));
    }
    if out != "Unknown" && !out.is_empty() && g["ok"] != true {
        m.push_str(&format!("  stdout: `{}`\n", cap(&out, 800)));
    }
    if g["truncated"] == true {
        m.push_str("  _(captured output was truncated at runtime)_\n");
    }
}

fn cap(s: &str, n: usize) -> String {
    if s.len() > n {
        let i = crate::context::floor_char_boundary(s, n);
        format!("{}…<truncated by export>", &s[..i])
    } else {
        s.to_string()
    }
}

fn num_or_unknown(v: &Value) -> String {
    v.as_u64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "Unknown".into())
}
