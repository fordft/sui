//! sui-certify: bounded live-provider certification.
//!
//! Drives the PRODUCTION agent loop (provider stream, tools, journal)
//! against a named profile and records per-request trace evidence.
//! Scenarios: tool continuation, identical replay, changed tail,
//! append-only growth, restart/replay from journal, deliberate prefix
//! invalidation. ≤ --max-requests per profile, one retry on transport
//! errors, no credentials → run anyway, verdict UNVERIFIED.

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::{json, Value};
use std::fmt::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use sui::agent::{Agent, Identity, Limits};
use sui::config::{self, Profile};
use sui::context;
use sui::journal::Journal;
use sui::permission::Gate;
use sui::provider::Provider;
use sui::tools::ToolContext;
use sui::types::Message;

#[derive(Parser)]
#[command(
    name = "sui-certify",
    version,
    about = "bounded live-provider certification"
)]
struct Cli {
    /// Profiles to certify, in order (cheap worker first, then strong).
    #[arg(long, required = true)]
    profile: Vec<String>,
    /// Explicit config file for profiles (else ~/.config/sui/config.toml)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Hard cap on provider requests per profile
    #[arg(long, default_value = "20")]
    max_requests: u64,
    /// Keep the generated fixture workspace for inspection
    #[arg(long)]
    keep_fixture: bool,
}

// Prompts: T_READ forces a bounded big read so the shared prefix clears
// typical cache minimums organically (fixture content, not padding).
const T_READ: &str = "Use the read_file tool to read fixture/big.txt (offset 1, limit 400), \
                      then reply with exactly: READ-DONE";
const T_FOLLOWUP: &str = "What was the last line number in the file you just read? \
                          Answer with only the number.";
const T_ALT: &str = "Use write_file to create notes.txt containing exactly: done";

struct Ctx {
    prof: Profile,
    ws: PathBuf,
    run_dir: PathBuf,
    session: String,
    used: u64,
    max: u64,
    failures: Vec<String>,
}

impl Ctx {
    fn agent(&self, scenario: &str) -> Result<Agent> {
        Ok(Agent::new(
            Provider::from_profile(&self.prof),
            ToolContext {
                workspace: self.ws.clone(),
                bash_timeout: Duration::from_secs(120),
                bash_timeout_max: Duration::from_secs(600),
                web: Some(sui::web::WebService::new(sui::web::load_cfg(None))),
                canon_root: std::sync::OnceLock::new(),
                ui: std::sync::OnceLock::new(),
                code_intel: Default::default(),
                code_context: Default::default(),
                tool_outputs: Default::default(),
            },
            Gate::new(true), // fixture workspace is disposable
            Journal::open_named(&self.run_dir, scenario)?,
            Limits {
                max_turns: 8,
                context_budget: 120_000,
                context_reserve: 8_192,
                compact_context: false,
                request_timeout: Duration::from_secs(300),
            },
            Identity {
                session_id: self.session.clone(),
                agent_id: scenario.to_string(),
                role: "worker".into(),
                base_url: self.prof.base_url.clone(),
                model: self.prof.model.clone(),
                cache_key_fingerprint: self
                    .prof
                    .prompt_cache_key
                    .as_ref()
                    .map(|k| context::sha256_hex(k.as_bytes())),
            },
        ))
    }

    fn budget_ok(&self) -> bool {
        self.used < self.max
    }

    async fn turn(&mut self, a: &mut Agent, input: &str, tag: &str) {
        a.push_user(input);
        let before = a.requests_made();
        if let Err(e) = a.drive().await {
            if retryable(&e) {
                eprintln!("  [{tag}] retrying after: {e:#}");
                tokio::time::sleep(Duration::from_secs(1)).await;
                if let Err(e2) = a.drive().await {
                    self.failures.push(format!("{tag}: {e2:#}"));
                }
            } else {
                self.failures.push(format!("{tag}: {e:#}"));
            }
        }
        self.used += a.requests_made() - before;
    }
}

fn retryable(e: &anyhow::Error) -> bool {
    let m = format!("{e:#}");
    !(m.contains("provider http 4") || m.contains("provider http 3"))
}

fn replay_history(path: &Path, max_users: usize) -> Result<Vec<Message>> {
    sui::journal::replay_history(path, max_users)
}

/// Collect all `request` events from a scenario journal.
fn request_rows(path: &Path, scenario: &str) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "request")
        .map(|e| {
            let mut d = e["data"].clone();
            d["scenario"] = json!(scenario);
            d
        })
        .collect()
}

/// Fingerprint of the first `request` event following the nth `user`
/// event in a scenario journal — the request whose context began with it.
fn fp_after_nth_user(path: &Path, n: usize) -> Option<String> {
    let mut users = 0;
    for line in std::fs::read_to_string(path)
        .ok()?
        .lines()
        .collect::<Vec<_>>()
    {
        let e: Value = serde_json::from_str(line).ok()?;
        match e["type"].as_str() {
            Some("user") => users += 1,
            Some("request") if users >= n => {
                return e["data"]["request_fingerprint"].as_str().map(String::from)
            }
            _ => {}
        }
    }
    None
}

fn tool_exec_count(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|e| e["type"] == "tool" && e["data"]["executed"] == true)
        .count()
}

async fn run_profile(prof: &Profile, max_req: u64, keep: bool) -> Result<()> {
    eprintln!(
        "\n══ profile '{}' → {} · model {} ══",
        prof.name, prof.base_url, prof.model
    );
    if prof.api_key.is_none() {
        eprintln!("  no credentials resolved — will run but verdict is UNVERIFIED");
    }

    // fixture workspace: a ~8k-token file so shared prefixes can clear
    // typical cache minimums, still far below the context budget.
    let ws = std::env::temp_dir().join(format!("sui-cert-{}", std::process::id()));
    let fx = ws.join("fixture");
    std::fs::create_dir_all(&fx)?;
    let mut big = String::new();
    for i in 1..=400 {
        big.push_str(&format!(
            "{i:04} fixture line alpha beta gamma delta epsilon zeta eta theta iota kappa lambda\n"
        ));
    }
    std::fs::write(fx.join("big.txt"), &big)?;

    let session = format!("cert-{}-{}", prof.name, std::process::id());
    let run_dir = std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/sui/runs")
        .join(&session);
    std::fs::create_dir_all(&run_dir)?;

    let mut ctx = Ctx {
        prof: prof.clone(),
        ws: ws.clone(),
        run_dir: run_dir.clone(),
        session: session.clone(),
        used: 0,
        max: max_req,
        failures: vec![],
    };

    // ── S1 tool_cycle: live tool continuation + append-only growth ──
    let mut s1 = ctx.agent("s1_tool_cycle")?;
    if ctx.budget_ok() {
        ctx.turn(&mut s1, T_READ, "s1.t1").await;
    }
    if ctx.budget_ok() {
        ctx.turn(&mut s1, T_FOLLOWUP, "s1.t2").await;
    }

    // ── S2 identical_replay: same first request, fresh session ──
    if ctx.budget_ok() {
        let mut s2 = ctx.agent("s2_identical_replay")?;
        ctx.turn(&mut s2, T_READ, "s2.t1").await;
    }

    // ── S3 restart_replay: rebuild history from journal, same tail ──
    if ctx.budget_ok() {
        let hist = replay_history(&Journal::path_of(&run_dir, "s1_tool_cycle"), 1)
            .context("replay history")?;
        let mut s3 = ctx.agent("s3_restart_replay")?;
        s3.restore_history(hist);
        ctx.turn(&mut s3, T_FOLLOWUP, "s3.t1").await;
    }

    // ── S4 changed_tail: same rebuilt history, different suffix ──
    if ctx.budget_ok() {
        let hist = replay_history(&Journal::path_of(&run_dir, "s1_tool_cycle"), 1)?;
        let mut s4 = ctx.agent("s4_changed_tail")?;
        s4.restore_history(hist);
        ctx.turn(&mut s4, T_ALT, "s4.t1").await;
    }

    // ── S5 prefix_mutation: same flow, deliberately altered static layer ──
    if ctx.budget_ok() {
        let mut s5 = ctx.agent("s5_prefix_mutation")?;
        s5.set_system(format!("{}\n\ncert-mutation-marker: x", context::system()));
        ctx.turn(&mut s5, T_READ, "s5.t1").await;
    }

    report(&ctx, &run_dir, prof)?;

    if !keep {
        let _ = std::fs::remove_dir_all(&ws);
    } else {
        eprintln!("fixture kept at {}", ws.display());
    }
    Ok(())
}

fn report(ctx: &Ctx, run_dir: &Path, prof: &Profile) -> Result<()> {
    let scenarios = [
        "s1_tool_cycle",
        "s2_identical_replay",
        "s3_restart_replay",
        "s4_changed_tail",
        "s5_prefix_mutation",
    ];
    let mut rows: Vec<Value> = vec![];
    for s in scenarios {
        rows.extend(request_rows(&Journal::path_of(run_dir, s), s));
    }

    // checks
    let fp_of = |scen: &str, rid: u64| -> Option<String> {
        rows.iter()
            .find(|r| r["scenario"] == scen && r["request_id"] == rid)
            .and_then(|r| r["request_fingerprint"].as_str().map(String::from))
    };
    let hash_of = |scen: &str| -> Option<String> {
        rows.iter()
            .find(|r| r["scenario"] == scen)
            .and_then(|r| r["static_prefix_hash"].as_str().map(String::from))
    };
    let cred_of = |scen: &str, rid: u64| -> Option<u64> {
        rows.iter()
            .find(|r| r["scenario"] == scen && r["request_id"] == rid)
            .and_then(|r| r["usage"]["cache_read_tokens"].as_u64())
    };

    let mut out = String::new();
    let w = &mut out;
    macro_rules! p { ($($a:tt)*) => {{ let _ = writeln!(w, $($a)*); }} }

    p!("# sui-certify — {}", prof.name);
    p!("endpoint: {}  model: {}", prof.base_url, prof.model);
    p!("");

    p!("| scenario | req | model | est_in | in | cached | wr | out | ttfd_ms | total_ms | finish | err |");
    p!("|---|---|---|---|---|---|---|---|---|---|---|---|");
    let mut telemetry_ok = 0usize;
    for r in &rows {
        let u = &r["usage"];
        if u["complete"] == true {
            telemetry_ok += 1;
        }
        let g = |k: &str| u[k].as_u64().map(|v| v.to_string()).unwrap_or("?".into());
        p!(
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            r["scenario"].as_str().unwrap_or(""),
            r["request_id"].as_u64().unwrap_or(0),
            r["returned_model"].as_str().unwrap_or("?"),
            r["input_size_estimate"].as_u64().unwrap_or(0),
            g("input_tokens"),
            g("cache_read_tokens"),
            g("cache_write_tokens"),
            g("output_tokens"),
            r["timing"]["first_delta_ms"].as_u64().unwrap_or(0),
            r["timing"]["request_total_ms"].as_u64().unwrap_or(0),
            r["finish_reason"].as_str().unwrap_or("none"),
            r["error_class"].as_str().unwrap_or(""),
        );
    }
    p!("");

    let s1_tools = tool_exec_count(&Journal::path_of(run_dir, "s1_tool_cycle"));
    p!("## checks");
    p!("- live tool continuation: {} tool(s) executed", s1_tools);
    match (fp_of("s1_tool_cycle", 0), fp_of("s2_identical_replay", 0)) {
        (Some(a), Some(b)) => p!(
            "- identical replay fingerprint: {}",
            if a == b { "EQUAL ✓" } else { "DIFFERENT ✗" }
        ),
        _ => p!("- identical replay fingerprint: incomplete data"),
    }
    // s3 replays s1's first turn then sends s1's second user turn → its
    // req0 fingerprint must equal s1's request right after that user msg.
    let s1_target = fp_after_nth_user(&Journal::path_of(run_dir, "s1_tool_cycle"), 2);
    match (s1_target, fp_of("s3_restart_replay", 0)) {
        (Some(a), Some(b)) => p!(
            "- restart/replay fingerprint: {}",
            if a == b {
                "EQUAL ✓ (journal reconstructs identical request)"
            } else {
                "DIFFERENT ✗ (persistence altered serialization)"
            }
        ),
        _ => p!("- restart/replay fingerprint: incomplete data"),
    }
    match (hash_of("s1_tool_cycle"), hash_of("s5_prefix_mutation")) {
        (Some(a), Some(b)) => p!(
            "- prefix mutation detection: {}",
            if a != b {
                "DETECTED ✓"
            } else {
                "NOT DETECTED ✗"
            }
        ),
        _ => p!("- prefix mutation detection: incomplete data"),
    }
    if let (Some(a), Some(b)) = (
        cred_of("s1_tool_cycle", 0),
        cred_of("s2_identical_replay", 0),
    ) {
        p!(
            "- replay cache_read: first={} replay={} (Δ {:+})",
            a,
            b,
            b as i64 - a as i64
        );
    }
    p!("");

    let n = rows.len();
    let errors = rows.iter().filter(|r| r["error_class"].is_string()).count();
    p!("## summary");
    p!("- requests: {n} (cap {})", ctx.max);
    p!("- telemetry complete: {telemetry_ok}/{n}");
    p!("- errored requests: {errors}");
    let paired: Vec<_> = rows
        .iter()
        .filter_map(|r| {
            let usage = &r["usage"];
            if usage["complete"] != true || usage["estimated"] == true {
                return None;
            }
            Some((
                usage["input_tokens"].as_u64()?,
                usage["cache_read_tokens"].as_u64()?,
            ))
        })
        .collect();
    let input: u64 = paired.iter().map(|(i, _)| *i).sum();
    let cached: u64 = paired.iter().map(|(_, c)| *c).sum();
    if input > 0 && cached <= input {
        p!(
            "- token cache rate: {:.2}% ({} / {} tokens; paired coverage {}/{n} requests)",
            cached as f64 * 100.0 / input as f64,
            cached,
            input,
            paired.len()
        );
    } else {
        p!("- token cache rate: unknown (no valid paired usage)");
    }

    // estimated cost from profile pricing, if provided
    if let Some(pr) = &prof.pricing {
        let costs: Option<Vec<f64>> = rows
            .iter()
            .map(|r| {
                let usage = serde_json::from_value::<sui::types::Usage>(r["usage"].clone()).ok()?;
                pr.estimate(&usage)
            })
            .collect();
        match costs.filter(|c| !c.is_empty()) {
            Some(costs) => p!("- estimated cost: ${:.6}", costs.iter().sum::<f64>()),
            None => p!("- estimated cost: n/a (incomplete usage or pricing)"),
        }
    } else {
        p!("- estimated cost: n/a (no pricing in profile)");
    }

    if !ctx.failures.is_empty() {
        p!("- failures:");
        for f in &ctx.failures {
            p!("  - {}", f);
        }
    }
    p!("");
    let verdict = if prof.api_key.is_none() {
        "UNVERIFIED — no credentials for profile"
    } else if n == 0 {
        "UNVERIFIED — no requests completed"
    } else if errors == n {
        "UNVERIFIED — all requests failed"
    } else {
        "COMPLETED — see checks above"
    };
    p!("**verdict: {verdict}**");

    print!("{out}");
    let rfile = run_dir.join(format!("cert-{}.md", prof.name));
    std::fs::write(&rfile, &out)?;
    eprintln!("report: {}", rfile.display());
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    for name in &cli.profile {
        let prof = config::resolve_profile(name, cli.config.as_deref())?;
        run_profile(&prof, cli.max_requests, cli.keep_fixture).await?;
    }
    Ok(())
}
