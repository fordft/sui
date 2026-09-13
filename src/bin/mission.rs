//! sui-mission: bounded multi-agent execution (v0.2).
//!
//! strong-profile orchestrator → homogeneous cheap-profile workers →
//! deterministic validation → integration → strong-profile audit.
//! Explicitly selected — this is never the default path.
//!
//! --compare runs three strategies (strong-only, cheap-only, mission) in
//! SEPARATE disposable envs cloned from the same committed base, with a
//! fixed external acceptance suite frozen before any strategy runs.
//! Mission-internal checks never define the yardstick.

use anyhow::Result;
use clap::Parser;
use serde_json::Value;
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sui::agent::{Agent, Identity, Limits};
use sui::config::{self, Profile};
use sui::context;
use sui::journal::Journal;
use sui::mission::{self, worktree};
use sui::permission::Gate;
use sui::provider::Provider;
use sui::tools::bash::spawn_bounded;
use sui::tools::ToolContext;

#[derive(Parser)]
#[command(
    name = "sui-mission",
    version,
    about = "bounded multi-agent mission execution"
)]
struct Cli {
    /// Strong model profile (orchestrator + auditor + escalation)
    #[arg(long)]
    control_profile: String,
    /// Cheap model profile (worker pool — homogeneous)
    #[arg(long)]
    worker_profile: String,
    /// Mission objective
    #[arg(long)]
    task: String,
    /// Repository root (original checkout is never modified)
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Config file containing profiles (else ~/.config/sui/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Worker pool size cap (1 or 2; >1 only after serial delegation passes)
    #[arg(long, default_value = "1")]
    max_workers: usize,
    /// Run the three-strategy comparison in isolated disposable envs
    #[arg(long)]
    compare: bool,
    /// External acceptance command, repeatable — frozen BEFORE any
    /// strategy runs; applied identically to every candidate. Without
    /// this, --compare has no independent yardstick.
    #[arg(long)]
    acceptance: Vec<String>,
    /// Paths restored from the candidate's base commit before external
    /// acceptance runs — a candidate that weakens the tests gains no
    /// advantage. Repeatable. (Tests absent from base should be kept
    /// outside the repo and referenced by absolute path instead.)
    #[arg(long)]
    trusted_path: Vec<String>,
    /// Comparison trials; strategy order rotates per trial
    #[arg(long, default_value = "1")]
    trials: usize,
    /// Keep mission worktrees after acceptance
    #[arg(long)]
    keep_worktrees: bool,
}

#[derive(Clone, Copy)]
enum Strat {
    StrongOnly,
    CheapOnly,
    Mission,
}

#[derive(Default)]
struct TrialRow {
    strategy: String,
    outcome: String,
    candidate_sha: Option<String>,
    external: Vec<String>,
    ext_pass: Option<bool>,
    control: mission::UsageAgg,
    worker: mission::UsageAgg,
    elapsed_ms: u128,
    repairs: usize,
    escalations: usize,
}

/// Fresh disposable env: a real clone at the committed base. Linked
/// worktrees share objects/refs, so strategies never share a repo —
/// neither can inspect another's solution branches or history.
fn fresh_env(orig: &Path, dir: &Path) -> Result<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("clone")
        .arg("--quiet")
        .arg(orig)
        .arg(dir)
        .output()?;
    if !out.status.success() {
        anyhow::bail!("env clone failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(dir.to_path_buf())
}

/// Single-agent strategy (strong-only or cheap-only): one agent in its own
/// worktree on the env clone, generic worker contract, no orchestration.
async fn solo(
    env: &PathBuf,
    run_dir: &PathBuf,
    journal_name: &str,
    session: &str,
    prof: &Profile,
    task: &str,
) -> Result<TrialRow> {
    let t0 = Instant::now();
    let base = worktree::head(env)?;
    let wt = worktree::worktrees_dir(run_dir).join(journal_name);
    let branch = format!("sui-{journal_name}-{session}");
    worktree::add(env, &wt, &branch, &base)?;
    let mut a = Agent::new(
        Provider::new(
            &prof.base_url,
            prof.api_key.clone(),
            prof.model.clone(),
            prof.prompt_cache_key.clone(),
        ),
        ToolContext {
            workspace: wt.clone(),
            bash_timeout: Duration::from_secs(120),
            bash_timeout_max: Duration::from_secs(600),
        },
        Gate::new(true),
        Journal::open_named(run_dir, journal_name)?,
        Limits {
            max_turns: 60,
            context_budget: 120_000,
            context_reserve: 8_192,
            request_timeout: Duration::from_secs(300),
        },
        Identity {
            session_id: session.to_string(),
            agent_id: journal_name.to_string(),
            role: "worker".into(), // it implements; family shown by profile
            base_url: prof.base_url.clone(),
            model: prof.model.clone(),
            cache_key_fingerprint: prof
                .prompt_cache_key
                .as_ref()
                .map(|k| context::sha256_hex(k.as_bytes())),
        },
    );
    a.set_quiet(true);
    a.run_turn(&format!(
        "Implement this objective in this repository worktree:\n{task}\n\
         Make the changes, verify they work, and reply with a one-line summary."
    ))
    .await?;
    let changed = worktree::changed_files(&wt, &base).unwrap_or_default();
    let sha = worktree::commit_all(&wt, journal_name).ok();
    let mut usage = mission::UsageAgg::default();
    for line in std::fs::read_to_string(run_dir.join(format!("{journal_name}.jsonl")))
        .unwrap_or_default()
        .lines()
    {
        let e: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if e["type"] != "request" {
            continue;
        }
        usage.requests += 1;
        let u = &e["data"]["usage"];
        if u["complete"] == true {
            usage.telemetry_known += 1;
        }
        usage.input += u["input_tokens"].as_u64().unwrap_or(0);
        usage.cache_read += u["cache_read_tokens"].as_u64().unwrap_or(0);
        usage.cache_write += u["cache_write_tokens"].as_u64().unwrap_or(0);
        usage.output += u["output_tokens"].as_u64().unwrap_or(0);
    }
    Ok(TrialRow {
        strategy: journal_name.to_string(),
        outcome: if changed.is_empty() {
            "no_changes".into()
        } else {
            format!("changed {} files", changed.len())
        },
        candidate_sha: sha,
        external: vec![],
        ext_pass: None,
        control: mission::UsageAgg::default(),
        worker: usage,
        elapsed_ms: t0.elapsed().as_millis(),
        repairs: 0,
        escalations: 0,
    })
}

/// External acceptance suite — the frozen yardstick. Trusted paths are
/// restored from `base` first (tests frozen with the criteria); identical
/// commands run on every candidate directory.
async fn external_acceptance(
    dir: &Path,
    cmds: &[String],
    trusted: &[String],
    base: &str,
) -> (Vec<String>, Option<bool>) {
    let mut rows = vec![];
    for p in trusted {
        let o = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["checkout", base, "--", p])
            .output();
        match o {
            Ok(o) if o.status.success() => {}
            Ok(o) => rows.push(format!(
                "trusted-path {p}: not present at base ({})",
                String::from_utf8_lossy(&o.stderr).trim()
            )),
            Err(e) => rows.push(format!("trusted-path {p}: {e}")),
        }
    }
    let mut all_ok = true;
    for cmd in cmds {
        match spawn_bounded(
            dir,
            cmd,
            Duration::from_secs(120),
            Duration::from_secs(300),
            std::future::pending(),
            None,
        )
        .await
        {
            Ok(o) => {
                if o.code != Some(0) {
                    all_ok = false;
                }
                rows.push(format!("{cmd}: exit {:?}", o.code));
            }
            Err(e) => {
                all_ok = false;
                rows.push(format!("{cmd}: error {e:#}"));
            }
        }
    }
    (rows, if cmds.is_empty() { None } else { Some(all_ok) })
}

fn est_cost(u: &mission::UsageAgg, p: &Profile) -> Option<f64> {
    let pr = p.pricing.as_ref()?;
    if u.telemetry_known == 0 {
        return None;
    }
    Some(
        u.input as f64 * pr.input.unwrap_or(0.0) / 1e6
            + u.cache_read as f64 * pr.cached.unwrap_or(0.0) / 1e6
            + u.cache_write as f64 * pr.cache_write.unwrap_or(0.0) / 1e6
            + u.output as f64 * pr.output.unwrap_or(0.0) / 1e6,
    )
}

fn rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap_or_default()
        .lines()
        .find(|l| l.starts_with("VmRSS"))
        .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        .unwrap_or(0)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let repo = cli
        .workspace
        .unwrap_or(std::env::current_dir()?)
        .canonicalize()?;
    let control = config::resolve_profile(&cli.control_profile, cli.config.as_deref())?;
    let worker = config::resolve_profile(&cli.worker_profile, cli.config.as_deref())?;
    let max_workers = cli.max_workers.clamp(1, 2);

    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let session = format!("m-{ts}-{}", std::process::id());
    let run_dir = std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/sui/runs")
        .join(&session);
    std::fs::create_dir_all(&run_dir)?;

    eprintln!("mission {session}");
    eprintln!("  control: {} → {}", cli.control_profile, control.model);
    eprintln!("  worker:  {} → {}", cli.worker_profile, worker.model);
    let verified = control.api_key.is_some() && worker.api_key.is_some();
    if !verified {
        eprintln!("  warning: credentials missing — report will be UNVERIFIED");
    }
    if cli.compare && cli.acceptance.is_empty() {
        eprintln!("  warning: --compare without --acceptance has no independent yardstick");
    }

    let mission_cfg = |env: &Path, rd: &Path, sess: &str| mission::MissionCfg {
        repo: env.to_path_buf(),
        run_dir: rd.to_path_buf(),
        control: control.clone(),
        worker: worker.clone(),
        objective: cli.task.clone(),
        max_workers,
        session: sess.to_string(),
        keep_worktrees: true, // compare envs are disposable; keep candidates
        request_timeout: Duration::from_secs(300),
        task_timeout: Duration::from_secs(900),
        context_budget: 120_000,
        context_reserve: 8_192,
        control_max_turns: 40,
        worker_max_turns: 50,
        events: None,
        cancel: None,
        session_approve: None,
        run: 1,
    };

    let mut rows: Vec<TrialRow> = vec![];

    if cli.compare {
        // strategies rotate per trial so order effects are visible
        let order = [Strat::StrongOnly, Strat::CheapOnly, Strat::Mission];
        for trial in 0..cli.trials {
            for k in 0..3 {
                let s = order[(k + trial) % 3];
                let name = match s {
                    Strat::StrongOnly => "strong-only",
                    Strat::CheapOnly => "cheap-only",
                    Strat::Mission => "mission",
                };
                eprintln!("· trial {trial} strategy {name}");
                let env = run_dir.join("envs").join(format!("{name}-t{trial}"));
                let env = fresh_env(&repo, &env)?;
                let env_base = worktree::head(&env)?;
                let mrd = run_dir.join(format!("m{trial}-{name}"));
                std::fs::create_dir_all(&mrd)?;

                let mut row = match s {
                    Strat::StrongOnly => {
                        solo(
                            &env,
                            &mrd,
                            &format!("strong-{trial}"),
                            &session,
                            &control,
                            &cli.task,
                        )
                        .await?
                    }
                    Strat::CheapOnly => {
                        solo(
                            &env,
                            &mrd,
                            &format!("cheap-{trial}"),
                            &session,
                            &worker,
                            &cli.task,
                        )
                        .await?
                    }
                    Strat::Mission => {
                        let r = mission::run(mission_cfg(
                            &env,
                            &mrd,
                            &format!("{session}-{name}-t{trial}"),
                        ))
                        .await?;
                        let mut row = TrialRow {
                            strategy: name.into(),
                            outcome: r.outcome.clone(),
                            candidate_sha: r.accepted_sha.clone(),
                            external: vec![],
                            ext_pass: None,
                            control: r.control_usage,
                            worker: r.worker_usage,
                            elapsed_ms: r.elapsed_ms,
                            repairs: r.repairs,
                            escalations: r.escalations,
                            ..Default::default()
                        };
                        // external acceptance on the integrated candidate
                        let integ = mrd.join("worktrees/integration");
                        let (ext, pass) = external_acceptance(
                            &integ,
                            &cli.acceptance,
                            &cli.trusted_path,
                            &env_base,
                        )
                        .await;
                        row.external = ext;
                        row.ext_pass = pass;
                        row
                    }
                };
                if !matches!(s, Strat::Mission) {
                    // solo candidate dir = its worktree
                    let wt = worktree::worktrees_dir(&mrd).join(match s {
                        Strat::StrongOnly => format!("strong-{trial}"),
                        _ => format!("cheap-{trial}"),
                    });
                    let (ext, pass) =
                        external_acceptance(&wt, &cli.acceptance, &cli.trusted_path, &env_base)
                            .await;
                    row.external = ext;
                    row.ext_pass = pass;
                }
                rows.push(row);
            }
        }
    } else {
        // plain mission in the user's repo: the integration branch IS the
        // deliverable; original checkout stays untouched.
        let cfg = mission::MissionCfg {
            // the external suite needs the candidate dir to still exist
            keep_worktrees: cli.keep_worktrees || !cli.acceptance.is_empty(),
            ..mission_cfg(&repo, &run_dir, &session)
        };
        let repo_base = worktree::head(&repo)?;
        let r = mission::run(cfg).await?;
        let mut row = TrialRow {
            strategy: "mission".into(),
            outcome: r.outcome.clone(),
            candidate_sha: r.accepted_sha.clone(),
            external: vec![],
            ext_pass: None,
            control: r.control_usage,
            worker: r.worker_usage,
            elapsed_ms: r.elapsed_ms,
            repairs: r.repairs,
            escalations: r.escalations,
            ..Default::default()
        };
        if let (Some(b), Some(sha)) = (&r.branch, &r.accepted_sha) {
            eprintln!("· accepted candidate: {b} @ {sha}");
        }
        let integ = run_dir.join("worktrees/integration");
        let (ext, pass) =
            external_acceptance(&integ, &cli.acceptance, &cli.trusted_path, &repo_base).await;
        row.external = ext;
        row.ext_pass = pass;
        rows.push(row);
    }

    // ── report ──────────────────────────────────────────────────────
    let mut out = String::new();
    macro_rules! p {
        ($($a:tt)*) => {{ let _ = writeln!(out, $($a)*); }};
    }
    p!("# mission {session}");
    p!("objective: {}", cli.task);
    p!(
        "repo: {} (compare envs are disposable clones of committed state)",
        repo.display()
    );
    p!(
        "external acceptance: {}",
        if cli.acceptance.is_empty() {
            "none provided".into()
        } else {
            cli.acceptance.join(" && ")
        }
    );
    p!("");

    p!("| strategy | outcome | ext accept | sha | reqs(c/w) | cached(c/w) | cost | elapsed | repairs | escal |");
    p!("|---|---|---|---|---|---|---|---|---|---|");
    for r in &rows {
        let cc = est_cost(&r.control, &control);
        let wc = est_cost(&r.worker, &worker);
        let cost = match (cc, wc) {
            (Some(a), Some(b)) => format!("${:.4}", a + b),
            (Some(a), None) => format!("${:.4}?", a),
            (None, Some(b)) => format!("${:.4}?", b),
            _ => "n/a".into(),
        };
        p!(
            "| {} | {} | {} | {} | {}/{} | {}/{} | {} | {}ms | {} | {} |",
            r.strategy,
            r.outcome,
            r.ext_pass
                .map(|b| if b { "PASS" } else { "FAIL" }.to_string())
                .unwrap_or("—".into()),
            r.candidate_sha
                .as_deref()
                .map(|s| &s[..8.min(s.len())])
                .unwrap_or("—"),
            r.control.requests,
            r.worker.requests,
            r.control.cache_read,
            r.worker.cache_read,
            cost,
            r.elapsed_ms,
            r.repairs,
            r.escalations,
        );
        for e in &r.external {
            p!("  - {} ext: {}", r.strategy, e);
        }
    }
    p!("");
    p!(
        "telemetry: control {}/{} complete, worker {}/{} complete",
        rows.iter().map(|r| r.control.telemetry_known).sum::<u64>(),
        rows.iter().map(|r| r.control.requests).sum::<u64>(),
        rows.iter().map(|r| r.worker.telemetry_known).sum::<u64>(),
        rows.iter().map(|r| r.worker.requests).sum::<u64>()
    );
    p!("host: RSS {}KB (dev build)", rss_kb());
    if !verified {
        p!("");
        p!("**verdict: UNVERIFIED — credentials missing**");
    }
    p!("");
    p!("artifacts: {}", run_dir.display());
    p!("note: failed runs count toward cost; cache numbers are provider-reported usage, not prefix-hash inference.");
    p!("note: compare envs are separate repositories, not a filesystem sandbox — agents could still read sibling dirs via bash; inspect transcripts for cross-strategy access.");
    if !cli.trusted_path.is_empty() {
        p!(
            "trusted paths restored from base before external acceptance: {}",
            cli.trusted_path.join(", ")
        );
    }

    print!("{out}");
    let rfile = run_dir.join("mission-report.md");
    std::fs::write(&rfile, &out)?;
    eprintln!("report: {}", rfile.display());
    Ok(())
}
