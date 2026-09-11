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
    pub control: Profile,
    pub worker: Profile,
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
}

#[derive(Default)]
pub struct UsageAgg {
    pub requests: u64,
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub telemetry_known: u64,
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

fn mk_agent(
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
) -> Result<Agent> {
    let mut a = Agent::new(
        Provider::new(
            &prof.base_url,
            prof.api_key.clone(),
            prof.model.clone(),
            prof.prompt_cache_key.clone(),
        ),
        ToolContext {
            workspace: workspace.to_path_buf(),
            bash_timeout: Duration::from_secs(120),
            bash_timeout_max: Duration::from_secs(600),
        },
        Gate::new(true), // worktrees are disposable; bounds still apply
        journal,
        Limits {
            max_turns,
            context_budget: budget,
            context_reserve: reserve,
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
    Ok(a)
}

/// Control-plane agent (orchestrator / auditor / escalation) with
/// submit_result interception. `check` validates payloads; rejected
/// payloads get one resubmission before the turn is ended.
fn control_agent(
    cfg: &MissionCfg,
    workspace: &Path,
    agent_id: &str,
    check: Arc<dyn Fn(&Value) -> Result<()> + Send + Sync>,
) -> Result<(Agent, Arc<Mutex<Cap>>)> {
    let mut a = mk_agent(
        &cfg.control,
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
                    Some(Intercept::Finish(format!("status: error\npayload rejected: {e:#}")))
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
        format!("{}…<truncated>", &evidence[..4000])
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
}

/// Which revision a task's worktree branches from: the mission base for
/// independent tasks, the integration tip once dependencies have merged.
fn base_for(cfg: &MissionCfg, c: &TaskContract, base: &str, integ_branch: &str) -> Result<String> {
    if c.depends_on.is_empty() {
        Ok(base.to_string())
    } else {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&cfg.repo)
            .args(["rev-parse", integ_branch])
            .output()?;
        if !out.status.success() {
            bail!("integration branch missing for dependent task {}", c.id);
        }
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }
}

/// Worker lifecycle: worktree → agent run → commit → ownership →
/// acceptance gates. Never panics on task failure — returns evidence.
async fn spawn_task(
    cfg: &MissionCfg,
    c: &TaskContract,
    base: &str,
    integ_branch: &str,
) -> Result<TaskOut> {
    let wt_dir = worktree::worktrees_dir(&cfg.run_dir);
    let wt = wt_dir.join(&c.id);
    let branch = format!("sui-task-{}-{}", cfg.session, c.id);
    // idempotent: a re-dispatch reuses the path
    worktree::remove(&cfg.repo, &wt);
    let _ = std::fs::remove_dir_all(&wt);
    let task_base = base_for(cfg, c, base, integ_branch)?;
    worktree::add(&cfg.repo, &wt, &branch, &task_base)?;
    let mut agent = mk_agent(
        &cfg.worker,
        &wt,
        &prompts::worker_system(),
        Journal::open_named(&cfg.run_dir, &format!("w-{}", c.id))?,
        &format!("w-{}", c.id),
        "worker",
        &cfg.session,
        c.max_turns.unwrap_or(cfg.worker_max_turns),
        cfg.request_timeout,
        cfg.context_budget,
        cfg.context_reserve,
    )?;
    tokio::time::timeout(
        cfg.task_timeout,
        agent.run_turn(&prompts::worker_task(c, &wt.to_string_lossy())),
    )
    .await
    .context("task deadline")??;
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
    };
    let changed = worktree::changed_files(wt, base)?;
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
    if !oos.is_empty() {
        out.capsule = capsule("out_of_scope", &format!("changed: {}", oos.join(", ")));
        return Ok(out);
    }
    out.sha = worktree::commit_all(wt, &format!("task {} [{}]", c.id, cfg.session))?;
    for cmd in &c.acceptance {
        let r = spawn_bounded(
            wt,
            cmd,
            Duration::from_secs(120),
            Duration::from_secs(300),
            std::future::pending(),
        )
        .await?;
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
    c: &TaskContract,
    prev: &TaskOut,
    task_base: &str,
) -> Result<TaskOut> {
    let wt = worktree::worktrees_dir(&cfg.run_dir).join(&c.id);
    let mut agent = mk_agent(
        &cfg.worker,
        &wt,
        &prompts::worker_system(),
        Journal::open_named(&cfg.run_dir, &format!("w-{}-repair", c.id))?,
        &format!("w-{}-repair", c.id),
        "worker",
        &cfg.session,
        c.max_turns.unwrap_or(cfg.worker_max_turns),
        cfg.request_timeout,
        cfg.context_budget,
        cfg.context_reserve,
    )?;
    tokio::time::timeout(
        cfg.task_timeout,
        agent.run_turn(&prompts::repair_task(c, &prev.capsule)),
    )
    .await
    .context("repair deadline")??;
    finish_task(cfg, &wt, &prev.branch, c, task_base).await
}

/// Escalation: a fresh control session gets only the task contract +
/// failure capsule and decides retry(revised contract)/abort.
async fn escalate(
    cfg: &MissionCfg,
    c: &TaskContract,
    prev: &TaskOut,
    budget_left: usize,
) -> Result<Option<TaskContract>> {
    let check: Arc<dyn Fn(&Value) -> Result<()> + Send + Sync> = Arc::new(|p| {
        match p["decision"].as_str() {
            Some("abort") => Ok(()),
            Some("retry") => {
                serde_json::from_value::<TaskContract>(p["revised_task"].clone())
                    .context("revised_task is not a contract")?;
                Ok(())
            }
            _ => bail!("decision must be retry or abort"),
        }
    });
    let (mut esc, cap) = control_agent(cfg, &cfg.repo, "escalation", check)?;
    let contract_json = serde_json::to_string_pretty(c).unwrap_or_default();
    esc.run_turn(&prompts::escalation_task(&contract_json, &prev.capsule, budget_left))
        .await
        .context("escalation session")?;
    let c2 = cap.lock().unwrap();
    match &c2.payload {
        Some(p) if p["decision"] == "retry" => {
            Ok(Some(serde_json::from_value(p["revised_task"].clone())?))
        }
        _ => Ok(None),
    }
}

/// Merge all task branches into integration, run integration checks.
async fn integrate_all(
    integ_wt: &Path,
    branches: &[String],
    plan: &MissionPlan,
) -> Result<()> {
    for b in branches {
        worktree::merge(integ_wt, b)?;
    }
    for cmd in &plan.integration_checks {
        let r = spawn_bounded(
            integ_wt,
            cmd,
            Duration::from_secs(120),
            Duration::from_secs(300),
            std::future::pending(),
        )
        .await?;
        if r.code != Some(0) {
            bail!(
                "integration check failed: {cmd}\nexit: {:?}\nstdout:\n{}\nstderr:\n{}",
                r.code, r.stdout, r.stderr
            );
        }
    }
    Ok(())
}

/// Re-run every gate on the integrated candidate; the auditor sees
/// trusted results, not model claims.
async fn gate_summary(plan: &MissionPlan, integ_wt: &Path) -> String {
    let mut s = String::new();
    for t in &plan.tasks {
        for cmd in &t.acceptance {
            let r = spawn_bounded(
                integ_wt,
                cmd,
                Duration::from_secs(120),
                Duration::from_secs(300),
                std::future::pending(),
            )
            .await;
            match r {
                Ok(o) => s.push_str(&format!("{} [{}]: exit {:?}\n", t.id, cmd, o.code)),
                Err(e) => s.push_str(&format!("{} [{cmd}]: error {e:#}\n", t.id)),
            }
        }
    }
    for cmd in &plan.integration_checks {
        let r = spawn_bounded(
            integ_wt,
            cmd,
            Duration::from_secs(120),
            Duration::from_secs(300),
            std::future::pending(),
        )
        .await;
        match r {
            Ok(o) => s.push_str(&format!("integration [{cmd}]: exit {:?}\n", o.code)),
            Err(e) => s.push_str(&format!("integration [{cmd}]: error {e:#}\n")),
        }
    }
    s
}

/// One auditor session over the integrated candidate.
async fn audit_once(
    cfg: &MissionCfg,
    integ_wt: &Path,
    plan: &MissionPlan,
    diff: &str,
    gates: &str,
    risks: &str,
) -> Result<(String, Value)> {
    let check: Arc<dyn Fn(&Value) -> Result<()> + Send + Sync> = Arc::new(|p| {
        match p["verdict"].as_str() {
            Some("PASS") | Some("FAIL") => Ok(()),
            _ => bail!("verdict must be PASS or FAIL"),
        }
    });
    let (mut a, cap) = control_agent(cfg, integ_wt, "auditor", check)?;
    a.run_turn(&prompts::audit_task(plan, diff, gates, risks)).await?;
    let c = cap.lock().unwrap();
    let p = c
        .payload
        .clone()
        .ok_or_else(|| anyhow::anyhow!("auditor produced no verdict"))?;
    Ok((p["verdict"].as_str().unwrap_or("FAIL").to_string(), p))
}

pub async fn run(cfg: MissionCfg) -> Result<MissionReport> {
    let t0 = Instant::now();
    let mut journal = Journal::open_named(&cfg.run_dir, "mission")?;
    std::fs::create_dir_all(worktree::worktrees_dir(&cfg.run_dir))?;

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

    // scoped so the body's borrows release before report finalization
    let mut cancelled = false;
    let flow = {
        let body = body(&cfg, &mut report, &mut journal);
        tokio::pin!(body);
        tokio::select! {
            r = &mut body => r?,
            _ = tokio::signal::ctrl_c() => {
                cancelled = true;
                Flow::Failed("cancelled".into())
            }
        }
    };
    if cancelled {
        journal.log("mission", json!({ "state": format!("{:?}", S::Cancelled) }));
        eprintln!("· mission: cancelled");
    }

    match flow {
        Flow::Accepted => report.outcome = "accepted".into(),
        Flow::Failed(why) => {
            journal.log("mission", json!({ "state": "Failed", "why": why }));
            report.outcome = format!("failed: {why}");
        }
    }

    // usage aggregation: bucket request events by role
    for f in std::fs::read_dir(&cfg.run_dir)? {
        let p = f?.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        for line in std::fs::read_to_string(&p).unwrap_or_default().lines() {
            let e: Value = match serde_json::from_str(line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if e["type"] != "request" {
                continue;
            }
            let role = e["data"]["role"].as_str().unwrap_or("");
            let u = &e["data"]["usage"];
            let agg = if role == "worker" {
                &mut report.worker_usage
            } else {
                &mut report.control_usage
            };
            agg.requests += 1;
            if u["complete"] == true {
                agg.telemetry_known += 1;
            }
            agg.input += u["input_tokens"].as_u64().unwrap_or(0);
            agg.cache_read += u["cache_read_tokens"].as_u64().unwrap_or(0);
            agg.cache_write += u["cache_write_tokens"].as_u64().unwrap_or(0);
            agg.output += u["output_tokens"].as_u64().unwrap_or(0);
        }
    }
    report.elapsed_ms = t0.elapsed().as_millis();

    // worktrees persist on failure/cancel for inspection; cleaned on accept
    if report.outcome == "accepted" && !cfg.keep_worktrees {
        let wt_dir = worktree::worktrees_dir(&cfg.run_dir);
        for f in std::fs::read_dir(&wt_dir)? {
            worktree::remove(&cfg.repo, &f?.path());
        }
    }
    Ok(report)
}

/// The mission state machine. Returns the terminal Flow; every transition
/// is journaled. `report` accumulates evidence as states complete.
async fn body(
    cfg: &MissionCfg,
    report: &mut MissionReport,
    journal: &mut Journal,
) -> Result<Flow> {
    let mut escalations_left = 1usize;
    let mut audit_repairs_left = 1usize;

    macro_rules! state {
        ($s:expr) => {{
            journal.log("mission", json!({ "state": format!("{:?}", $s) }));
            eprintln!("· mission: {:?}", $s);
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
    let base = worktree::head(&cfg.repo)?;
    let repo = cfg.repo.clone();
    let check_plan: Arc<dyn Fn(&Value) -> Result<()> + Send + Sync> =
        Arc::new(move |p| {
            let plan: MissionPlan = serde_json::from_value(p.clone())
                .context("payload is not a mission plan")?;
            plan::validate(&plan, &repo)
        });
    let (mut orch, cap) = control_agent(cfg, &cfg.repo, "orchestrator", check_plan)?;
    let overview = {
        let out = spawn_bounded(
            &cfg.repo,
            "git ls-files | head -200",
            Duration::from_secs(10),
            Duration::from_secs(10),
            std::future::pending(),
        )
        .await;
        out.map(|o| o.stdout).unwrap_or_default()
    };
    if let Err(e) = orch
        .run_turn(&prompts::orchestrator_task(&cfg.objective, &base, &overview))
        .await
    {
        fail!(format!("orchestrator error: {e:#}"));
    }
    let plan: MissionPlan = {
        let c = cap.lock().unwrap();
        match &c.payload {
            Some(p) => match serde_json::from_value(p.clone()) {
                Ok(p) => p,
                Err(e) => fail!(format!("plan decode: {e:#}")),
            },
            None => fail!(format!(
                "orchestrator produced no valid plan{}",
                c.last_error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()
            )),
        }
    };
    journal.log("plan", serde_json::to_value(&plan).unwrap_or_default());
    report.plan = Some(plan.clone());

    // ── integration candidate up front; workers may depend on its tip ──
    let integ_branch = format!("sui-mission-{}", cfg.session);
    let wt_dir = worktree::worktrees_dir(&cfg.run_dir);
    let integ_wt = wt_dir.join("integration");
    worktree::add(&cfg.repo, &integ_wt, &integ_branch, &base)?;
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
            let ba = base_for(cfg, ta, &base, &integ_branch)?;
            let bb = base_for(cfg, tb, &base, &integ_branch)?;
            let (ra, rb) = tokio::join!(
                spawn_task(cfg, ta, &base, &integ_branch),
                spawn_task(cfg, tb, &base, &integ_branch),
            );
            results.push((ta.clone(), ra.map_err(|e| e.to_string()), ba));
            results.push((tb.clone(), rb.map_err(|e| e.to_string()), bb));
        } else {
            for &i in &wave {
                let t = &plan.tasks[i];
                let tb = base_for(cfg, t, &base, &integ_branch)?;
                let r = spawn_task(cfg, t, &base, &integ_branch)
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
                },
            };
            if !out.ok {
                // one repair round
                state!(S::Repairing);
                report.repairs += 1;
                match repair_task(cfg, &contract, &out, &task_base).await {
                    Ok(o2) if o2.ok => out = o2,
                    Ok(o2) => out = o2,
                    Err(e) => {
                        out.capsule = format!("{}\nrepair error: {e:#}", out.capsule)
                    }
                }
            }
            if !out.ok {
                if escalations_left == 0 {
                    fail!(format!(
                        "task {} failed; repair and escalation budget exhausted",
                        contract.id
                    ));
                }
                escalations_left -= 1;
                report.escalations += 1;
                match escalate(cfg, &contract, &out, escalations_left).await {
                    Ok(Some(newc)) => {
                        let r2 = spawn_task(cfg, &newc, &base, &integ_branch)
                            .await
                            .map_err(|e| e.to_string());
                        match r2 {
                            Ok(o3) if o3.ok => {
                                out = o3;
                                report.tasks.push(json!({
                                    "id": newc.id, "status": "ok_after_escalation",
                                    "sha": out.sha, "changed": out.changed }));
                            }
                            _ => fail!(format!(
                                "task {} still failed after escalation",
                                contract.id
                            )),
                        }
                    }
                    Ok(None) => {
                        fail!(format!("orchestrator aborted on task {}", contract.id))
                    }
                    Err(e) => fail!(format!("escalation error: {e:#}")),
                }
            } else {
                report.tasks.push(json!({
                    "id": contract.id, "status": "ok",
                    "sha": out.sha, "changed": out.changed }));
            }
            // serialize integration: merge each passing task immediately
            state!(S::Integrating);
            if let Err(e) = worktree::merge(&integ_wt, &out.branch) {
                let conflict_out = TaskOut {
                    capsule: capsule("merge_conflict", &format!("{e:#}")),
                    ..out
                };
                if escalations_left > 0 {
                    escalations_left -= 1;
                    report.escalations += 1;
                    match escalate(cfg, &contract, &conflict_out, escalations_left).await {
                        Ok(Some(newc)) => {
                            let r3 = spawn_task(cfg, &newc, &base, &integ_branch)
                                .await
                                .map_err(|e| e.to_string());
                            match r3 {
                                Ok(o4) if o4.ok => {
                                    worktree::merge(&integ_wt, &o4.branch).map_err(|e2| {
                                        anyhow::anyhow!("merge after escalation: {e2:#}")
                                    })?;
                                    merged.push(o4.branch.clone());
                                    report.tasks.push(json!({
                                        "id": newc.id,
                                        "status": "ok_after_escalation",
                                        "sha": o4.sha, "changed": o4.changed }));
                                }
                                _ => fail!(format!("merge conflict persists for {}", contract.id)),
                            }
                        }
                        _ => fail!(format!("merge conflict on {}: {e:#}", contract.id)),
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
    for cmd in &plan.integration_checks {
        let r = spawn_bounded(
            &integ_wt,
            cmd,
            Duration::from_secs(120),
            Duration::from_secs(300),
            std::future::pending(),
        )
        .await?;
        if r.code != Some(0) {
            fail!(format!(
                "integration check '{cmd}' failed (exit {:?})\n{}\n{}",
                r.code, r.stdout, r.stderr
            ));
        }
    }

    // ── AUDIT (one repair round on failure) ─────────────────────────
    loop {
        state!(S::Auditing);
        let diff = worktree::diff(&integ_wt, &base).unwrap_or_default();
        let diff = if diff.len() > 30_000 {
            format!("{}…<truncated>", &diff[..30_000])
        } else {
            diff
        };
        let gates = gate_summary(&plan, &integ_wt).await;
        let risks = format!(
            "repairs used: {}; escalations used: {}",
            report.repairs, report.escalations
        );
        let (verdict, payload) = match audit_once(cfg, &integ_wt, &plan, &diff, &gates, &risks).await {
            Ok(v) => v,
            Err(e) => fail!(format!("audit error: {e:#}")),
        };
        report.audit = Some(payload.clone());
        if verdict == "PASS" {
            break;
        }
        if audit_repairs_left == 0 {
            fail!("audit failed; repair budget exhausted");
        }
        audit_repairs_left -= 1;
        report.repairs += 1;
        state!(S::Repairing);
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
            };
            if let Ok(o) = repair_task(cfg, &t, &prev, &tb).await {
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
        worktree::reset_hard(&integ_wt, &base)?;
        if let Err(e) = integrate_all(&integ_wt, &merged, &plan).await {
            fail!(format!("re-integration after audit repair: {e:#}"));
        }
    }

    state!(S::Accepted);
    // bind the validation+audit record to the exact accepted candidate:
    // the branch ref survives worktree cleanup and identifies the code.
    let sha = worktree::git_rev(&cfg.repo, &integ_branch).unwrap_or_default();
    report.accepted_sha = Some(sha.clone());
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
