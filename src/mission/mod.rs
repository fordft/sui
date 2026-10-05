//! Bounded mission execution: strong-plan → cheap-implement →
//! deterministic-validate → integrate → strong-audit.
//!
//! The state machine lives here in Rust — models produce typed artifacts
//! (plans, verdicts); the runtime owns scheduling, transitions, budgets.
//! Nothing model-driven manages the process.

pub mod plan;
pub mod prompts;
pub mod worktree;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::agent::{Agent, Identity, Intercept, Limits};
use crate::config::Profile;
use crate::context;
use crate::journal::Journal;
use crate::permission::Gate;
use crate::provider::Provider;
use crate::tools::bash::spawn_bounded;
use crate::tools::ToolContext;
use plan::{MissionPlan, TaskContract};

#[derive(Debug, Clone, Copy, PartialEq)]
enum S {
    Planning,
    Dispatching,
    Integrating,
    Auditing,
    Repairing,
    Accepted,
    Failed,
    Cancelled,
}

/// Mission terminal state produced by the driver body.
enum Flow {
    Accepted,
    Failed(String),
}

#[derive(Clone)]
pub struct MissionCfg {
    pub repo: PathBuf,
    pub run_dir: PathBuf,
    /// Orchestrator/escalation plane.
    pub control: Profile,
    /// Implementation plane.
    pub worker: Profile,
    /// Auditor plane — None = the control profile.
    pub auditor: Option<Profile>,
    pub objective: String,
    /// 1..=2. Concurrency only activates on a 2-task independent wave.
    pub max_workers: usize,
    pub session: String,
    pub keep_worktrees: bool,
    pub request_timeout: Duration,
    pub task_timeout: Duration,
    pub context_budget: usize,
    pub context_reserve: usize,
    pub control_max_turns: usize,
    pub worker_max_turns: usize,
    /// UI wiring: events out, shared cancel in. None = headless.
    pub events: Option<crate::events::Sink>,
    pub cancel: Option<(Arc<tokio::sync::Notify>, Arc<std::sync::atomic::AtomicBool>)>,
    /// Shared session-approval flag from the UI (see Gate::set_ui).
    pub session_approve: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Per-mission web-research service — shared across control, every
    /// worker, and the auditor so run limits are global, not per-agent.
    pub web: Option<Arc<crate::web::WebService>>,
    /// Activity-run id stamped on UI events — one mission = one run
    /// group in the transcript. Headless callers pass 1.
    pub run: u64,
}

#[derive(Default)]
pub struct UsageAgg {
    pub requests: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub telemetry_known: u64,
    pub input_known: u64,
    pub cache_read_known: u64,
    pub cache_write_known: u64,
    pub output_known: u64,
}

impl UsageAgg {
    /// Fold one request's usage. Missing counts fold to 0 — the
    /// unknown-vs-zero rule lives here once; `telemetry_known` is what
    /// gates "0" from being read as "the provider reported zero".
    pub fn add(
        &mut self,
        complete: bool,
        input: Option<u64>,
        cache_read: Option<u64>,
        cache_write: Option<u64>,
        output: Option<u64>,
    ) {
        self.requests += 1;
        if complete {
            self.telemetry_known += 1;
        }
        self.input_known += u64::from(complete && input.is_some());
        self.cache_read_known += u64::from(complete && cache_read.is_some());
        self.cache_write_known += u64::from(complete && cache_write.is_some());
        self.output_known += u64::from(complete && output.is_some());
        self.input += input.unwrap_or(0);
        self.cache_read += cache_read.unwrap_or(0);
        self.cache_write += cache_write.unwrap_or(0);
        self.output += output.unwrap_or(0);
    }

    /// Fold a journal `request` event's `data.usage` object.
    pub fn add_journal(&mut self, u: &serde_json::Value) {
        self.add(
            u["complete"] == true && u["estimated"] != true,
            u["input_tokens"].as_u64(),
            u["cache_read_tokens"].as_u64(),
            u["cache_write_tokens"].as_u64(),
            u["output_tokens"].as_u64(),
        );
    }
}

pub struct MissionReport {
    pub outcome: String,
    pub plan: Option<MissionPlan>,
    pub tasks: Vec<Value>,
    pub audit: Option<Value>,
    pub branch: Option<String>,
    /// Commit sha of the accepted integration candidate — durable even
    /// after worktree cleanup; audit + gate records bind to this.
    pub accepted_sha: Option<String>,
    pub control_usage: UsageAgg,
    pub worker_usage: UsageAgg,
    pub elapsed_ms: u128,
    pub repairs: usize,
    pub escalations: usize,
    pub run_dir: PathBuf,
}

struct Cap {
    payload: Option<Value>,
    rejects: usize,
    last_error: Option<String>,
}

#[allow(clippy::too_many_arguments)]
fn mk_agent(
    cfg: &MissionCfg,
    prof: &Profile,
    workspace: &Path,
    system: &str,
    journal: Journal,
    agent_id: &str,
    role: &str,
    session: &str,
    max_turns: usize,
    req_timeout: Duration,
    budget: usize,
    reserve: usize,
    al: &crate::config::AgentLimits,
) -> Result<Agent> {
    // [agent] bash timeouts resolve against the source repo, not the
    // worktree — sui.toml precedence still applies via cfg.repo. Parsed
    // once per mission in MissionRt, not per spawn.
    let mut a = Agent::new(
        Provider::from_profile(prof),
        ToolContext {
            workspace: workspace.to_path_buf(),
            bash_timeout: Duration::from_millis(al.bash_timeout_ms),
            bash_timeout_max: Duration::from_millis(al.bash_timeout_max_ms),
            web: cfg.web.clone(),
            canon_root: std::sync::OnceLock::new(),
            ui: std::sync::OnceLock::new(),
            code_intel: Default::default(),
            code_context: Default::default(),
            tool_outputs: Default::default(),
        },
        Gate::new(true), // worktrees are disposable; bounds still apply
        journal,
        Limits {
            max_turns,
            context_budget: budget,
            context_reserve: reserve,
            compact_context: al.context_compaction,
            request_timeout: req_timeout,
        },
        Identity {
            session_id: session.to_string(),
            agent_id: agent_id.to_string(),
            role: role.to_string(),
            base_url: prof.base_url.clone(),
            model: prof.model.clone(),
            cache_key_fingerprint: prof
                .prompt_cache_key
                .as_ref()
                .map(|k| context::sha256_hex(k.as_bytes())),
        },
    );
    a.set_quiet(true);
    a.set_system(system.to_string());
    a.set_run_id(cfg.run);
    if let (Some(sink), Some((n, f))) = (&cfg.events, &cfg.cancel) {
        a.wire_ui(
            sink.clone(),
            n.clone(),
            f.clone(),
            cfg.session_approve.clone(),
        );
    }
    Ok(a)
}

/// Native runtime settings resolved once per mission.
struct MissionRt {
    limits: crate::config::AgentLimits,
}

/// Run the native worker inside its task worktree. Ownership and
/// acceptance gates validate the result after the turn returns.
async fn drive_task(
    cfg: &MissionCfg,
    rt: &MissionRt,
    c: &TaskContract,
    wt: &Path,
    prompt: String,
    agent_id: &str,
    journal_name: &str,
) -> Result<()> {
    let mut agent = mk_agent(
        cfg,
        &cfg.worker,
        wt,
        &prompts::worker_system(),
        Journal::open_named(&cfg.run_dir, journal_name)?,
        agent_id,
        "worker",
        &cfg.session,
        c.max_turns.unwrap_or(cfg.worker_max_turns),
        cfg.request_timeout,
        cfg.context_budget,
        cfg.context_reserve,
        &rt.limits,
    )?;
    tokio::time::timeout(cfg.task_timeout, agent.run_turn(&prompt))
        .await
        .context("task deadline")?
}

/// Shape validator for a control payload (plan, verdict, escalation).
type PayloadCheck = Arc<dyn Fn(&Value) -> Result<()> + Send + Sync>;

/// Capture a runtime-validated native submit_result payload.
#[allow(clippy::too_many_arguments)]
async fn run_control(
    cfg: &MissionCfg,
    rt: &MissionRt,
    prof: &Profile,
    agent_id: &str,
    workspace: &Path,
    prompt: String,
    check: PayloadCheck,
) -> Result<Cap> {
    let (mut a, cap) = control_agent(cfg, rt, prof, workspace, agent_id, check)?;
    tokio::time::timeout(cfg.task_timeout, a.run_turn(&prompt))
        .await
        .context("control deadline")??;
    let c = cap.lock().unwrap();
    Ok(Cap {
        payload: c.payload.clone(),
        rejects: c.rejects,
        last_error: c.last_error.clone(),
    })
}

/// Control-plane agent (orchestrator / auditor / escalation) with
/// submit_result interception. `check` validates payloads; rejected
/// payloads get one resubmission before the turn is ended.
fn control_agent(
    cfg: &MissionCfg,
    rt: &MissionRt,
    prof: &Profile,
    workspace: &Path,
    agent_id: &str,
    check: PayloadCheck,
) -> Result<(Agent, Arc<Mutex<Cap>>)> {
    let mut a = mk_agent(
        cfg,
        prof,
        workspace,
        prompts::CONTROL_SYSTEM,
        Journal::open_named(&cfg.run_dir, agent_id)?,
        agent_id,
        "control",
        &cfg.session,
        cfg.control_max_turns,
        cfg.request_timeout,
        cfg.context_budget,
        cfg.context_reserve,
        &rt.limits,
    )?;
    a.add_tool_schema(prompts::submit_result_schema());
    let cap = Arc::new(Mutex::new(Cap {
        payload: None,
        rejects: 0,
        last_error: None,
    }));
    let cap2 = cap.clone();
    a.set_interceptor(Arc::new(move |call| {
        if call.function.name != "submit_result" {
            return None;
        }
        let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or_default();
        let payload = args["payload"].clone();
        let mut c = cap2.lock().unwrap();
        match check(&payload) {
            Ok(()) => {
                c.payload = Some(payload);
                Some(Intercept::Finish("status: success\nresult accepted".into()))
            }
            Err(e) => {
                c.rejects += 1;
                c.last_error = Some(format!("{e:#}"));
                if c.rejects >= 2 {
                    Some(Intercept::Finish(format!(
                        "status: error\npayload rejected: {e:#}"
                    )))
                } else {
                    Some(Intercept::Result(format!(
                        "status: error\npayload rejected: {e:#}\nfix and resubmit"
                    )))
                }
            }
        }
    }));
    Ok((a, cap))
}

/// Failure capsule: bounded evidence handed to repair/escalation —
/// never a history dump.
fn capsule(kind: &str, evidence: &str) -> String {
    let ev = if evidence.len() > 4000 {
        let i = crate::context::floor_char_boundary(evidence, 4000);
        format!("{}…<truncated>", &evidence[..i])
    } else {
        evidence.to_string()
    };
    format!("kind={kind}\n{ev}")
}

pub struct TaskOut {
    pub ok: bool,
    pub branch: String,
    pub sha: String,
    pub changed: Vec<String>,
    pub capsule: String,
    /// The revision this task's worktree was actually created from —
    /// ownership diffs must compare against this, not a recomputed tip.
    pub task_base: String,
    /// Deterministic gate evidence (ownership + acceptance commands) —
    /// journaled as part of task_result for the run export.
    pub gates: Vec<Value>,
}

/// One gate record: a command the runtime itself executed, not a model
/// claim. Output tails are bounded; truncation is flagged.
fn gate_rec(kind: &str, cmd: &str, cwd: &Path, out: &crate::tools::bash::ProcOut) -> Value {
    let tail = |s: &str| -> String {
        if s.len() > 4000 {
            let i = crate::context::floor_char_boundary(s, 4000);
            format!("{}…<truncated>", &s[..i])
        } else {
            s.to_string()
        }
    };
    json!({
        "kind": kind,
        "cmd": cmd,
        "cwd": cwd.file_name().map(|n| format!("worktrees/{}", n.to_string_lossy()))
            .unwrap_or_else(|| cwd.display().to_string()),
        "exit_code": out.code,
        "ok": out.code == Some(0),
        "timed_out": out.timed_out,
        "truncated": out.truncated,
        "stdout_tail": tail(&out.stdout),
        "stderr_tail": tail(&out.stderr),
    })
}

/// Which revision a task's worktree branches from: the mission base for
/// independent tasks, the integration tip once dependencies have merged.
async fn base_for(
    cfg: &MissionCfg,
    c: &TaskContract,
    base: &str,
    integ_branch: &str,
) -> Result<String> {
    if c.depends_on.is_empty() {
        Ok(base.to_string())
    } else {
        worktree::git_rev(&cfg.repo, integ_branch)
            .await
            .with_context(|| format!("integration branch missing for dependent task {}", c.id))
    }
}

/// Worker lifecycle: worktree → agent run → commit → ownership →
/// acceptance gates. Never panics on task failure — returns evidence.
/// The native loop implements the task inside its isolated worktree;
/// the gates are not.
async fn spawn_task(
    cfg: &MissionCfg,
    rt: &MissionRt,
    c: &TaskContract,
    base: &str,
    integ_branch: &str,
) -> Result<TaskOut> {
    let wt_dir = worktree::worktrees_dir(&cfg.run_dir);
    let wt = wt_dir.join(&c.id);
    let branch = format!("sui-task-{}-{}", cfg.session, c.id);
    // idempotent: a re-dispatch reuses the path
    worktree::remove(&cfg.repo, &wt).await;
    let _ = std::fs::remove_dir_all(&wt);
    let task_base = base_for(cfg, c, base, integ_branch).await?;
    worktree::add(&cfg.repo, &wt, &branch, &task_base).await?;
    drive_task(
        cfg,
        rt,
        c,
        &wt,
        prompts::worker_task(c, &wt.to_string_lossy()),
        &format!("w-{}", c.id),
        &format!("w-{}", c.id),
    )
    .await?;
    finish_task(cfg, &wt, &branch, c, &task_base).await
}

/// Deterministic worker-candidate gates: ownership check against the
/// actual diff, then every acceptance command.
async fn finish_task(
    cfg: &MissionCfg,
    wt: &Path,
    branch: &str,
    c: &TaskContract,
    base: &str,
) -> Result<TaskOut> {
    let mut out = TaskOut {
        ok: false,
        branch: branch.to_string(),
        sha: String::new(),
        changed: vec![],
        capsule: String::new(),
        task_base: base.to_string(),
        gates: vec![],
    };
    let changed = worktree::changed_files(wt, base).await?;
    out.changed = changed.clone();
    if changed.is_empty() {
        out.capsule = capsule("empty", "no changes produced");
        return Ok(out);
    }
    let oos: Vec<_> = changed
        .iter()
        .filter(|f| !plan::path_owned(f, &c.owned_paths))
        .cloned()
        .collect();
    let in_scope = oos.is_empty();
    out.gates.push(json!({
        "kind": "ownership",
        "cmd": format!("changed files ⊆ owned_paths ({})", c.owned_paths.join(", ")),
        "cwd": wt.file_name().map(|n| format!("worktrees/{}", n.to_string_lossy()))
            .unwrap_or_else(|| wt.display().to_string()),
        "changed": changed,
        "out_of_scope": oos,
        "ok": in_scope,
    }));
    if !in_scope {
        out.capsule = capsule(
            "out_of_scope",
            &format!(
                "changed: {}",
                out.gates[0]["out_of_scope"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        return Ok(out);
    }
    out.sha = worktree::commit_all(wt, &format!("task {} [{}]", c.id, cfg.session)).await?;
    for cmd in &c.acceptance {
        let r = spawn_bounded(
            wt,
            cmd,
            Duration::from_secs(120),
            Duration::from_secs(300),
            std::future::pending(),
            None,
        )
        .await?;
        out.gates.push(gate_rec("acceptance", cmd, wt, &r));
        if r.code != Some(0) {
            out.capsule = capsule(
                "acceptance",
                &format!(
                    "cmd: {cmd}\nexit: {:?}\nstdout:\n{}\nstderr:\n{}",
                    r.code, r.stdout, r.stderr
                ),
            );
            return Ok(out);
        }
    }
    out.ok = true;
    Ok(out)
}

/// One bounded repair round in the same worktree, fresh worker session
/// carrying only the failure capsule (bounded artifact).
async fn repair_task(
    cfg: &MissionCfg,
    rt: &MissionRt,
    c: &TaskContract,
    prev: &TaskOut,
    task_base: &str,
) -> Result<TaskOut> {
    let wt = worktree::worktrees_dir(&cfg.run_dir).join(&c.id);
    drive_task(
        cfg,
        rt,
        c,
        &wt,
        prompts::repair_task(c, &prev.capsule),
        &format!("w-{}-repair", c.id),
        &format!("w-{}-repair", c.id),
    )
    .await?;
    finish_task(cfg, &wt, &prev.branch, c, task_base).await
}

/// Escalation: a fresh control session gets only the task contract +
/// failure capsule and decides retry(revised contract)/abort.
async fn escalate(
    cfg: &MissionCfg,
    rt: &MissionRt,
    plan: &MissionPlan,
    c: &TaskContract,
    prev: &TaskOut,
    budget_left: usize,
) -> Result<Option<TaskContract>> {
    let orig_id = c.id.clone();
    // A retry contract is validated by substituting it into the plan and
    // running the FULL shape validator — same id (repair/merge/audit
    // key on the original ids), bounded owned_paths, disjointness vs
    // siblings, acyclic depends_on. A bare from_value would admit a
    // contract that breaks the ownership invariant or orphans the
    // repair path under a different id.
    let shape_check = {
        let orig_id = orig_id.clone();
        let plan = plan.clone();
        move |newc: &TaskContract| -> Result<()> {
            if newc.id != orig_id {
                bail!(
                    "revised_task id '{}' must keep the original id '{orig_id}'",
                    newc.id
                );
            }
            let mut p2 = plan.clone();
            for t in &mut p2.tasks {
                if t.id == orig_id {
                    *t = newc.clone();
                }
            }
            plan::validate_shape(&p2).context("revised_task fails plan invariants")
        }
    };
    let check: PayloadCheck = Arc::new(move |p| match p["decision"].as_str() {
        Some("abort") => Ok(()),
        Some("retry") => {
            let newc: TaskContract = serde_json::from_value(p["revised_task"].clone())
                .context("revised_task is not a contract")?;
            shape_check(&newc)
        }
        _ => bail!("decision must be retry or abort"),
    });
    let contract_json = serde_json::to_string_pretty(c).unwrap_or_default();
    let c2 = run_control(
        cfg,
        rt,
        &cfg.control,
        "escalation",
        &worktree::worktrees_dir(&cfg.run_dir).join("control"),
        prompts::escalation_task(&contract_json, &prev.capsule, budget_left),
        check,
    )
    .await
    .context("escalation session")?;
    match &c2.payload {
        Some(p) if p["decision"] == "retry" => {
            Ok(Some(serde_json::from_value(p["revised_task"].clone())?))
        }
        _ => Ok(None),
    }
}

/// Outcome of one escalate → respawn round. Callers map each arm to
/// their own terminal message — abort and escalation-session error are
/// always distinct from "still failing".
enum Esc {
    /// Respawned contract passed — its output.
    Recovered(TaskOut),
    /// Orchestrator declined to retry.
    Aborted,
    /// Respawn still failed (or failed to launch).
    StillFailing,
}

/// The shared half of both escalation paths: decrements the budget,
/// bumps the escalation counter, emits the phase line, and on success
/// journals the ok_after_escalation record + TaskRows update.
/// Err = escalation-session error (never a merge conflict).
#[allow(clippy::too_many_arguments)]
async fn escalate_and_respawn(
    cfg: &MissionCfg,
    rt: &MissionRt,
    report: &mut MissionReport,
    plan: &MissionPlan,
    contract: &TaskContract,
    out: &TaskOut,
    escalations_left: &mut usize,
    base: &str,
    integ_branch: &str,
) -> Result<Esc> {
    *escalations_left -= 1;
    report.escalations += 1;
    if let Some(tx) = &cfg.events {
        let why = out.capsule.lines().take(2).collect::<Vec<_>>().join(" ");
        let _ = tx.send(crate::events::UiEvent::Phase {
            run: cfg.run,
            agent: "mission".into(),
            text: format!("escalating {} to control — {}", contract.id, why),
        });
    }
    let Some(newc) = escalate(cfg, rt, plan, contract, out, *escalations_left).await? else {
        return Ok(Esc::Aborted);
    };
    match spawn_task(cfg, rt, &newc, base, integ_branch)
        .await
        .map_err(|e| e.to_string())
    {
        Ok(o) if o.ok => {
            report.tasks.push(json!({
                "id": newc.id, "status": "ok_after_escalation",
                "sha": o.sha, "changed": o.changed }));
            if let Some(tx) = &cfg.events {
                let _ = tx.send(crate::events::UiEvent::TaskRows(json!(report.tasks)));
            }
            Ok(Esc::Recovered(o))
        }
        _ => Ok(Esc::StillFailing),
    }
}

/// Merge all task branches into integration, run integration checks.
/// Run each integration check once on the integrated worktree:
/// journal a gate record per command, bail on the first failure, and
/// return the auditor-formatted lines so later stages reuse the same
/// results instead of re-running identical commands on identical state.
async fn run_integration_checks(
    integ_wt: &Path,
    plan: &MissionPlan,
    journal: &mut Journal,
) -> Result<Vec<String>> {
    let mut lines = Vec::new();
    for cmd in &plan.integration_checks {
        let r = spawn_bounded(
            integ_wt,
            cmd,
            Duration::from_secs(120),
            Duration::from_secs(300),
            std::future::pending(),
            None,
        )
        .await?;
        journal.log("gate", gate_rec("integration", cmd, integ_wt, &r));
        lines.push(format!("integration [{cmd}]: exit {:?}\n", r.code));
        if r.code != Some(0) {
            bail!(
                "integration check failed: {cmd}\nexit: {:?}\nstdout:\n{}\nstderr:\n{}",
                r.code,
                r.stdout,
                r.stderr
            );
        }
    }
    Ok(lines)
}

async fn integrate_all(
    integ_wt: &Path,
    branches: &[String],
    plan: &MissionPlan,
    journal: &mut Journal,
) -> Result<Vec<String>> {
    for b in branches {
        worktree::merge(integ_wt, b).await?;
    }
    run_integration_checks(integ_wt, plan, journal).await
}

/// Re-run every gate on the integrated candidate; the auditor sees
/// trusted results, not model claims. Task acceptance commands run
/// here because the merged tree differs from each task's worktree;
/// integration checks come pre-run — they already executed on this
/// exact state and re-running would duplicate the most expensive
/// commands in the system verbatim.
async fn gate_summary(
    plan: &MissionPlan,
    integ_wt: &Path,
    integ_lines: &[String],
    journal: &mut Journal,
) -> String {
    let mut s = String::new();
    for t in &plan.tasks {
        for cmd in &t.acceptance {
            let r = spawn_bounded(
                integ_wt,
                cmd,
                Duration::from_secs(120),
                Duration::from_secs(300),
                std::future::pending(),
                None,
            )
            .await;
            match r {
                Ok(o) => {
                    // these re-runs ARE the merged-tree gate evidence —
                    // journal them like every other gate record or the
                    // audit report has no proof they happened
                    journal.log("gate", gate_rec("merged-acceptance", cmd, integ_wt, &o));
                    s.push_str(&format!("{} [{}]: exit {:?}\n", t.id, cmd, o.code))
                }
                Err(e) => s.push_str(&format!("{} [{cmd}]: error {e:#}\n", t.id)),
            }
        }
    }
    for l in integ_lines {
        s.push_str(l);
    }
    s
}

/// One auditor session over the integrated candidate.
async fn audit_once(
    cfg: &MissionCfg,
    rt: &MissionRt,
    integ_wt: &Path,
    plan: &MissionPlan,
    diff: &str,
    gates: &str,
    risks: &str,
) -> Result<(String, Value)> {
    let check: PayloadCheck = Arc::new(|p| match p["verdict"].as_str() {
        Some("PASS") | Some("FAIL") => Ok(()),
        _ => bail!("verdict must be PASS or FAIL"),
    });
    let auditor = cfg.auditor.as_ref().unwrap_or(&cfg.control);
    let c = run_control(
        cfg,
        rt,
        auditor,
        "auditor",
        integ_wt,
        prompts::audit_task(plan, diff, gates, risks),
        check,
    )
    .await?;
    let p = c.payload.clone().ok_or_else(|| {
        anyhow::anyhow!(
            "auditor produced no verdict{}",
            c.last_error
                .as_ref()
                .map(|e| format!(": {e}"))
                .unwrap_or_default()
        )
    })?;
    Ok((p["verdict"].as_str().unwrap_or("FAIL").to_string(), p))
}

pub async fn run(cfg: MissionCfg) -> Result<MissionReport> {
    // Invariant: RunDone reaches event consumers on EVERY exit path —
    // setup errors, body errors, cancellation, success. The TUI's
    // running flag and any future consumer hang without it.
    let res = run_inner(&cfg).await;
    if let Err(e) = &res {
        if let Some(tx) = &cfg.events {
            let _ = tx.send(crate::events::UiEvent::Error {
                run: cfg.run,
                agent: "mission".into(),
                msg: format!("{e:#}"),
            });
            let _ = tx.send(crate::events::UiEvent::RunDone {
                run: cfg.run,
                outcome: format!("error: {e:#}"),
                accepted_sha: None,
            });
        }
    }
    res
}

async fn run_inner(cfg: &MissionCfg) -> Result<MissionReport> {
    crate::config::validate_native_config(None, &cfg.repo)?;
    let t0 = Instant::now();
    let mut journal = Journal::open_named(&cfg.run_dir, "mission")?;
    std::fs::create_dir_all(worktree::worktrees_dir(&cfg.run_dir))?;
    journal.log(crate::journal::ev::SESSION, json!({
        "mode": "mission",
        "workspace": cfg.repo,
        "sui_version": env!("CARGO_PKG_VERSION"),
        "approval": if cfg.session_approve.as_ref().map(|f| f.load(Ordering::Relaxed)).unwrap_or(false) {
            "auto"
        } else if cfg.events.is_some() {
            "ask"
        } else {
            "auto (unattended)"
        },
    }));

    let mut report = MissionReport {
        outcome: "running".into(),
        plan: None,
        tasks: vec![],
        audit: None,
        branch: None,
        accepted_sha: None,
        control_usage: UsageAgg::default(),
        worker_usage: UsageAgg::default(),
        elapsed_ms: 0,
        repairs: 0,
        escalations: 0,
        run_dir: cfg.run_dir.clone(),
    };

    // Native limits stay stable for the lifetime of this mission.
    let rt = MissionRt {
        limits: crate::config::agent_limits(&cfg.repo),
    };
    // scoped so the body's borrows release before report finalization
    let mut cancelled = false;
    let flow = {
        let body = body(cfg, &rt, &mut report, &mut journal);
        tokio::pin!(body);
        let n = cfg.cancel.as_ref().map(|(n, _)| n.clone());
        // catch_unwind: a panic inside body() must still reach the
        // terminal-evidence path below (journal Failed + RunDone) rather
        // than unwinding past it and leaving outcome stuck at "running"
        let guarded = std::panic::AssertUnwindSafe(async {
            tokio::select! {
                r = &mut body => r.map(|f| (f, false)),
                _ = tokio::signal::ctrl_c() => {
                    Ok((Flow::Failed("cancelled".into()), true))
                }
                _ = async move { if let Some(n) = n { n.notified().await } else { std::future::pending().await } } => {
                    if let Some((_, f)) = &cfg.cancel { f.store(true, Ordering::Relaxed); }
                    Ok((Flow::Failed("cancelled".into()), true))
                }
            }
        });
        match futures_util::FutureExt::catch_unwind(guarded).await {
            Ok(Ok((f, was_cancel))) => {
                cancelled = was_cancel;
                Ok(f)
            }
            Ok(Err(e)) => Err(e),
            Err(p) => {
                let msg = p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "unknown".into());
                Ok(Flow::Failed(format!("panic: {msg}")))
            }
        }
    };
    let flow = flow?;
    if cancelled {
        journal.log("mission", json!({ "state": format!("{:?}", S::Cancelled) }));
        if cfg.events.is_none() {
            eprintln!("· mission: cancelled");
        }
    }

    match flow {
        Flow::Accepted => report.outcome = "accepted".into(),
        Flow::Failed(why) => {
            journal.log("mission", json!({ "state": "Failed", "why": why }));
            report.outcome = format!("failed: {why}");
        }
    }
    if let Some(tx) = &cfg.events {
        let _ = tx.send(crate::events::UiEvent::RunDone {
            run: cfg.run,
            outcome: report.outcome.clone(),
            accepted_sha: report.accepted_sha.clone(),
        });
    }

    // usage aggregation: bucket request events by role
    for f in std::fs::read_dir(&cfg.run_dir)? {
        let p = f?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&p).unwrap_or_default().lines() {
            // cheap prefilter — most lines aren't request events; the
            // parsed check below stays authoritative (a payload could
            // legitimately contain this substring)
            if !line.contains("\"type\":\"request\"") {
                continue;
            }
            let e: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if e["type"] != "request" {
                continue;
            }
            let role = e["data"]["role"].as_str().unwrap_or("");
            let agg = if role == "worker" {
                &mut report.worker_usage
            } else {
                &mut report.control_usage
            };
            agg.add_journal(&e["data"]["usage"]);
        }
    }
    report.elapsed_ms = t0.elapsed().as_millis();
    journal.log(
        "result",
        json!({
            "outcome": report.outcome,
            "accepted_sha": report.accepted_sha,
            "branch": report.branch,
            "elapsed_ms": report.elapsed_ms,
            "repairs": report.repairs,
            "escalations": report.escalations,
        }),
    );

    // worktrees persist on failure/cancel for inspection; cleaned on accept
    if report.outcome == "accepted" && !cfg.keep_worktrees {
        let wt_dir = worktree::worktrees_dir(&cfg.run_dir);
        for f in std::fs::read_dir(&wt_dir)? {
            worktree::remove(&cfg.repo, &f?.path()).await;
        }
        // the control scratch branch holds no deliverable — drop it
        worktree::branch_delete(&cfg.repo, &format!("sui-control-{}", cfg.session)).await;
    }
    Ok(report)
}

/// The mission state machine. Returns the terminal Flow; every transition
/// is journaled. `report` accumulates evidence as states complete.
async fn body(
    cfg: &MissionCfg,
    rt: &MissionRt,
    report: &mut MissionReport,
    journal: &mut Journal,
) -> Result<Flow> {
    let mut escalations_left = 1usize;
    let mut audit_repairs_left = 1usize;

    macro_rules! state {
        ($s:expr) => {{
            journal.log("mission", json!({ "state": format!("{:?}", $s) }));
            if cfg.events.is_none() {
                eprintln!("· mission: {:?}", $s);
            }
            if let Some(tx) = &cfg.events {
                let _ = tx.send(crate::events::UiEvent::MissionState(format!("{:?}", $s)));
            }
        }};
    }
    macro_rules! fail {
        ($why:expr) => {{
            state!(S::Failed);
            return Ok(Flow::Failed($why.to_string()));
        }};
    }

    // ── PLANNING ────────────────────────────────────────────────────
    state!(S::Planning);
    let base = match worktree::head(&cfg.repo).await {
        Ok(b) => b,
        Err(e) => fail!(format!("resolve repo HEAD: {e:#}")),
    };
    // Control roles (orchestrator, escalation) get a scratch worktree at
    // the mission base — they can read the real tree for planning
    // context but can never write into the user's checkout. It lives
    // under worktrees_dir so accept-cleanup reclaims it like the rest.
    let control_wt = worktree::worktrees_dir(&cfg.run_dir).join("control");
    worktree::remove(&cfg.repo, &control_wt).await;
    let _ = std::fs::remove_dir_all(&control_wt);
    let control_branch = format!("sui-control-{}", cfg.session);
    if let Err(e) = worktree::add(&cfg.repo, &control_wt, &control_branch, &base).await {
        fail!(format!("control worktree: {e:#}"));
    }
    // Shape invariants only — pure + sync so a malformed plan reprompts
    // inside run_control. base_commit resolution happens async right
    // after capture, where it ALSO has to equal the real HEAD.
    let check_plan: PayloadCheck = Arc::new(move |p| {
        let plan: MissionPlan =
            serde_json::from_value(p.clone()).context("payload is not a mission plan")?;
        plan::validate_shape(&plan)
    });
    let overview = {
        let out = spawn_bounded(
            &cfg.repo,
            "git ls-files | head -200",
            Duration::from_secs(10),
            Duration::from_secs(10),
            std::future::pending(),
            None,
        )
        .await;
        out.map(|o| o.stdout).unwrap_or_default()
    };
    let cap = match run_control(
        cfg,
        rt,
        &cfg.control,
        "orchestrator",
        &control_wt,
        prompts::orchestrator_task(&cfg.objective, &base, &overview),
        check_plan,
    )
    .await
    {
        Ok(c) => c,
        Err(e) => fail!(format!("orchestrator error: {e:#}")),
    };
    let plan: MissionPlan = {
        match &cap.payload {
            Some(p) => match serde_json::from_value(p.clone()) {
                Ok(p) => p,
                Err(e) => fail!(format!("plan decode: {e:#}")),
            },
            None => fail!(format!(
                "orchestrator produced no valid plan{}",
                cap.last_error
                    .as_ref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            )),
        }
    };
    // The plan declares which commit it branched from — validate()
    // already proved it resolves; here it must BE the actual base or the
    // merge window silently shifts. Reject drift instead of ignoring it.
    match worktree::git_rev(&cfg.repo, &format!("{}^{{commit}}", plan.base_commit)).await {
        Ok(pb) if pb == base => {}
        Ok(pb) => fail!(format!(
            "plan base_commit {pb} is not repo HEAD {base} — replan from the real base"
        )),
        Err(e) => fail!(format!("resolve plan base_commit: {e:#}")),
    }
    journal.log("plan", serde_json::to_value(&plan).unwrap_or_default());
    if let Some(tx) = &cfg.events {
        let _ = tx.send(crate::events::UiEvent::TaskRows(
            serde_json::to_value(&plan.tasks).unwrap_or_default(),
        ));
    }
    report.plan = Some(plan.clone());

    // ── integration candidate up front; workers may depend on its tip ──
    let integ_branch = format!("sui-mission-{}", cfg.session);
    let wt_dir = worktree::worktrees_dir(&cfg.run_dir);
    let integ_wt = wt_dir.join("integration");
    if let Err(e) = worktree::add(&cfg.repo, &integ_wt, &integ_branch, &base).await {
        fail!(format!("integration worktree: {e:#}"));
    }
    report.branch = Some(integ_branch.clone());
    let mut merged: Vec<String> = vec![];
    // spawn-time base per task — repair rounds must diff against the
    // revision the worktree actually came from, not a moved tip.
    let mut task_bases: std::collections::HashMap<String, String> = Default::default();

    // ── DISPATCH + per-wave INTEGRATION ─────────────────────────────
    for wave in plan::waves(&plan) {
        state!(S::Dispatching);
        let mut results: Vec<(TaskContract, Result<TaskOut, String>, String)> = vec![];
        if cfg.max_workers >= 2 && wave.len() == 2 {
            let ta = &plan.tasks[wave[0]];
            let tb = &plan.tasks[wave[1]];
            let (ba, bb) = match tokio::join!(
                base_for(cfg, ta, &base, &integ_branch),
                base_for(cfg, tb, &base, &integ_branch),
            ) {
                (Ok(a), Ok(b)) => (a, b),
                (Err(e), _) | (_, Err(e)) => {
                    fail!(format!("task base resolution: {e:#}"))
                }
            };
            let (ra, rb) = tokio::join!(
                spawn_task(cfg, rt, ta, &base, &integ_branch),
                spawn_task(cfg, rt, tb, &base, &integ_branch),
            );
            results.push((ta.clone(), ra.map_err(|e| e.to_string()), ba));
            results.push((tb.clone(), rb.map_err(|e| e.to_string()), bb));
        } else {
            for &i in &wave {
                let t = &plan.tasks[i];
                let tb = match base_for(cfg, t, &base, &integ_branch).await {
                    Ok(b) => b,
                    Err(e) => fail!(format!("task base resolution: {e:#}")),
                };
                let r = spawn_task(cfg, rt, t, &base, &integ_branch)
                    .await
                    .map_err(|e| e.to_string());
                results.push((t.clone(), r, tb));
            }
        }

        for (contract, res, task_base) in results {
            task_bases.insert(contract.id.clone(), task_base.clone());
            let mut out = match res {
                Ok(o) => o,
                Err(e) => TaskOut {
                    ok: false,
                    branch: format!("sui-task-{}-{}", cfg.session, contract.id),
                    sha: String::new(),
                    changed: vec![],
                    capsule: capsule("runtime", &e),
                    task_base: task_base.clone(),
                    gates: vec![],
                },
            };
            if !out.ok {
                // one repair round — the transcript gets the reason and
                // attempt number, not just a bare stage name
                state!(S::Repairing);
                report.repairs += 1;
                if let Some(tx) = &cfg.events {
                    let why = out.capsule.lines().take(2).collect::<Vec<_>>().join(" ");
                    let _ = tx.send(crate::events::UiEvent::Phase {
                        run: cfg.run,
                        agent: "mission".into(),
                        text: format!("repair attempt 1 for {} — {}", contract.id, why),
                    });
                }
                match repair_task(cfg, rt, &contract, &out, &task_base).await {
                    Ok(o2) => out = o2,
                    Err(e) => out.capsule = format!("{}\nrepair error: {e:#}", out.capsule),
                }
            }
            if !out.ok {
                if escalations_left == 0 {
                    fail!(format!(
                        "task {} failed; repair and escalation budget exhausted",
                        contract.id
                    ));
                }
                match escalate_and_respawn(
                    cfg,
                    rt,
                    report,
                    &plan,
                    &contract,
                    &out,
                    &mut escalations_left,
                    &base,
                    &integ_branch,
                )
                .await
                {
                    Ok(Esc::Recovered(o3)) => out = o3,
                    Ok(Esc::Aborted) => {
                        fail!(format!("orchestrator aborted on task {}", contract.id))
                    }
                    Ok(Esc::StillFailing) => fail!(format!(
                        "task {} still failed after escalation",
                        contract.id
                    )),
                    Err(e) => fail!(format!("escalation error: {e:#}")),
                }
            } else {
                report.tasks.push(json!({
                    "id": contract.id, "status": "ok",
                    "sha": out.sha, "changed": out.changed }));
                if let Some(tx) = &cfg.events {
                    let _ = tx.send(crate::events::UiEvent::TaskRows(json!(report.tasks)));
                }
            }
            journal.log(
                "task_result",
                json!({
                    "id": contract.id,
                    "ok": out.ok,
                    "branch": out.branch,
                    "sha": out.sha,
                    "changed": out.changed,
                    "task_base": out.task_base,
                    "capsule": out.capsule,
                    "gates": out.gates,
                }),
            );
            // serialize integration: merge each passing task immediately
            state!(S::Integrating);
            if let Err(e) = worktree::merge(&integ_wt, &out.branch).await {
                let conflict_out = TaskOut {
                    capsule: capsule("merge_conflict", &format!("{e:#}")),
                    ..out
                };
                if escalations_left > 0 {
                    match escalate_and_respawn(
                        cfg,
                        rt,
                        report,
                        &plan,
                        &contract,
                        &conflict_out,
                        &mut escalations_left,
                        &base,
                        &integ_branch,
                    )
                    .await
                    {
                        Ok(Esc::Recovered(o4)) => {
                            if let Err(e2) = worktree::merge(&integ_wt, &o4.branch).await {
                                fail!(format!("merge after escalation: {e2:#}"));
                            }
                            merged.push(o4.branch.clone());
                        }
                        Ok(Esc::Aborted) => {
                            fail!(format!("orchestrator aborted on task {}", contract.id))
                        }
                        Ok(Esc::StillFailing) => {
                            fail!(format!("merge conflict persists for {}", contract.id))
                        }
                        Err(e2) => fail!(format!("escalation error: {e2:#}")),
                    }
                } else {
                    fail!(format!("merge conflict on {}: {e:#}", contract.id));
                }
            } else {
                merged.push(out.branch.clone());
            }
        }
    }

    // deterministic gate on the combined candidate — once, after all merges
    state!(S::Integrating);
    let mut integ_lines = match run_integration_checks(&integ_wt, &plan, journal).await {
        Ok(l) => l,
        Err(e) => fail!(format!("{e:#}")),
    };

    // ── AUDIT (one repair round on failure) ─────────────────────────
    loop {
        state!(S::Auditing);
        let diff = match worktree::diff(&integ_wt, &base).await {
            Ok(d) => d,
            Err(e) => {
                // a diff failure audited as "empty diff" would certify
                // nothing — fail loudly instead of silently passing
                fail!(format!("integration diff: {e:#}"));
            }
        };
        let diff = if diff.len() > 30_000 {
            let i = crate::context::floor_char_boundary(&diff, 30_000);
            format!("{}…<truncated>", &diff[..i])
        } else {
            diff
        };
        let gates = gate_summary(&plan, &integ_wt, &integ_lines, journal).await;
        let risks = format!(
            "repairs used: {}; escalations used: {}",
            report.repairs, report.escalations
        );
        // The auditor runs inside the integration worktree — pin its tip
        // so a model-side `git commit` can't land ungated, un-audited
        // changes in the accepted candidate.
        let pre_audit_tip = match worktree::git_rev(&integ_wt, "HEAD").await {
            Ok(t) => t,
            Err(e) => fail!(format!("pin integration tip: {e:#}")),
        };
        let (verdict, payload) =
            match audit_once(cfg, rt, &integ_wt, &plan, &diff, &gates, &risks).await {
                Ok(v) => v,
                Err(e) => fail!(format!("audit error: {e:#}")),
            };
        match worktree::git_rev(&integ_wt, "HEAD").await {
            Ok(t) if t == pre_audit_tip => {}
            Ok(t) => fail!(format!(
                "auditor modified the integration candidate (tip moved to {t}) — \
                 changes must come through task worktrees"
            )),
            Err(e) => fail!(format!("verify integration tip: {e:#}")),
        }
        report.audit = Some(payload.clone());
        journal.log("audit", payload.clone());
        if let Some(tx) = &cfg.events {
            let _ = tx.send(crate::events::UiEvent::AuditResult(payload.clone()));
        }
        if verdict == "PASS" {
            break;
        }
        if audit_repairs_left == 0 {
            fail!("audit failed; repair budget exhausted");
        }
        audit_repairs_left -= 1;
        report.repairs += 1;
        state!(S::Repairing);
        if let Some(tx) = &cfg.events {
            let _ = tx.send(crate::events::UiEvent::Phase {
                run: cfg.run,
                agent: "mission".into(),
                text: format!(
                    "audit repair round — {} fix(es) required by auditor",
                    payload["required_fixes"]
                        .as_array()
                        .map(|a| a.len())
                        .unwrap_or(0)
                ),
            });
        }
        let fixes = serde_json::to_string_pretty(&payload["required_fixes"]).unwrap_or_default();
        let mut any_ok = false;
        for t in plan.tasks.clone() {
            // route the fix capsule to the owning worktree(s); workers
            // verify their own diffs — with ≤2 workers this stays bounded
            let fixes_msg = format!(
                "AUDIT REPAIR for task {}. Auditor requires:\n{}\n\
                 Your objective: {}\nOwned paths: {}\nAcceptance: {}",
                t.id,
                fixes,
                t.objective,
                t.owned_paths.join(", "),
                t.acceptance.join("; ")
            );
            let Some(tb) = task_bases.get(&t.id).cloned() else {
                continue; // task never dispatched — nothing to repair
            };
            let prev = TaskOut {
                ok: false,
                branch: format!("sui-task-{}-{}", cfg.session, t.id),
                sha: String::new(),
                changed: vec![],
                capsule: capsule("audit", &fixes_msg),
                task_base: tb.clone(),
                gates: vec![],
            };
            if let Ok(o) = repair_task(cfg, rt, &t, &prev, &tb).await {
                if o.ok {
                    any_ok = true;
                }
            }
        }
        if !any_ok {
            fail!("audit repair produced no passing candidate");
        }
        // deterministic re-integration from base
        state!(S::Integrating);
        if let Err(e) = worktree::reset_hard(&integ_wt, &base).await {
            fail!(format!("reset integration tree: {e:#}"));
        }
        match integrate_all(&integ_wt, &merged, &plan, journal).await {
            Ok(l) => integ_lines = l,
            Err(e) => fail!(format!("re-integration after audit repair: {e:#}")),
        }
    }

    state!(S::Accepted);
    // bind the validation+audit record to the exact accepted candidate:
    // the branch ref survives worktree cleanup and identifies the code.
    // An unresolvable sha is a failure, not Some("") — the acceptance
    // evidence must bind to a real commit.
    let sha = match worktree::git_rev(&cfg.repo, &integ_branch).await {
        Ok(s) if !s.is_empty() => s,
        Ok(_) => fail!("accepted branch resolved to an empty revision"),
        Err(e) => fail!(format!("resolve accepted branch: {e:#}")),
    };
    report.accepted_sha = Some(sha.clone());
    if let Some(tx) = &cfg.events {
        let files: Vec<String> = report
            .tasks
            .iter()
            .flat_map(|t| {
                t["changed"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str().map(String::from))
            })
            .collect();
        let _ = tx.send(crate::events::UiEvent::ChangeSet {
            files,
            sha: Some(sha.clone()),
        });
    }
    journal.log(
        "accepted",
        json!({
            "sha": sha,
            "branch": integ_branch,
            "tasks": report.tasks,
            "audit": report.audit,
        }),
    );
    Ok(Flow::Accepted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capsule_truncates_on_char_boundary() {
        // '…' is 3 bytes; a cut inside it previously panicked.
        let mut evidence = "x".repeat(3999);
        evidence.push('…');
        evidence.push_str(&"y".repeat(100));
        let c = capsule("test-fail", &evidence);
        assert!(c.contains("<truncated>"));
        assert!(c.len() <= 4200);
    }

    #[test]
    fn usage_agg_enforces_unknown_vs_zero_once() {
        let mut a = UsageAgg::default();
        a.add_journal(&json!({
            "complete": true, "input_tokens": 10,
            "cache_read_tokens": 5, "output_tokens": 3
        }));
        a.add_journal(&json!({"complete": false})); // unknown → 0, not "known zero"
        assert_eq!(a.requests, 2);
        assert_eq!(a.telemetry_known, 1);
        assert_eq!(a.input, 10);
        assert_eq!(a.cache_read, 5);
        assert_eq!(a.output, 3);
    }

    #[test]
    fn floor_char_boundary_never_splits_a_char() {
        let s = "ab…cdé🙂z";
        for i in 0..=s.len() {
            let j = crate::context::floor_char_boundary(s, i);
            assert!(s.is_char_boundary(j));
            let _ = &s[..j];
        }
    }
}
