//! Mission-mode mock verification. In-process SSE mock + temp git repo;
//! drives the real mission::run driver end to end. All tests serialize on
//! a global lock because the cancellation test raises a real SIGINT.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sui::config::Profile;
use sui::mission::{self, MissionCfg};

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn lock() -> &'static Mutex<()> {
    LOCK.get_or_init(|| Mutex::new(()))
}

// ── scriptable SSE mock ────────────────────────────────────────────────

struct Script {
    plan_payload: Value,
    worker_first: Value,     // tool_calls to emit on a fresh worker turn
    /// (marker in first user msg, tool_calls) — per-task routing
    worker_routes: Vec<(&'static str, Value)>,
    worker_repair: Value,    // on "REPAIR ROUND" / "AUDIT REPAIR"
    audit_verdicts: Vec<String>,
    escalation: Value,       // payload for ESCALATION
}

fn tc(id: &str, name: &str, args: &str) -> Value {
    json!({"id": id, "type": "function",
           "function": {"name": name, "arguments": args}})
}

fn submit(payload: Value) -> Value {
    json!([tc("s1", "submit_result", &json!({"payload": payload}).to_string())])
}

fn sse_tool_calls(calls: Value) -> String {
    let d = json!({"choices": [{"index": 0, "delta": {"role": "assistant",
        "tool_calls": calls}, "finish_reason": "tool_calls"}]});
    let u = json!({"choices": [], "usage": {"prompt_tokens": 100,
        "completion_tokens": 10,
        "prompt_tokens_details": {"cached_tokens": 50}}});
    format!("data: {d}\n\ndata: {u}\n\ndata: [DONE]\n\n")
}

fn sse_text(t: &str) -> String {
    let d = json!({"choices": [{"index": 0, "delta": {"role": "assistant",
        "content": t}, "finish_reason": "stop"}]});
    let u = json!({"choices": [], "usage": {"prompt_tokens": 100,
        "completion_tokens": 5,
        "prompt_tokens_details": {"cached_tokens": 50}}});
    format!("data: {d}\n\ndata: {u}\n\ndata: [DONE]\n\n")
}

fn mock(script: Script) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let audit_left = Mutex::new(script.audit_verdicts);
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let mut s = match conn {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut r = BufReader::new(s.try_clone().unwrap());
            // read headers
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let line = line.trim().to_string();
                if line.is_empty() {
                    break;
                }
                if line.to_lowercase().starts_with("content-length:") {
                    len = line[15..].trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            let _ = r.read_exact(&mut body);
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let msgs = req["messages"].as_array().cloned().unwrap_or_default();
            let system = msgs
                .iter()
                .find(|m| m["role"] == "system")
                .and_then(|m| m["content"].as_str())
                .unwrap_or("")
                .to_string();
            let last = msgs.last().cloned().unwrap_or_default();
            let last_user = msgs
                .iter()
                .rev()
                .find(|m| m["role"] == "user")
                .and_then(|m| m["content"].as_str())
                .unwrap_or("")
                .to_string();

            let body = if system.contains("control plane") {
                if last["role"] == "tool" {
                    // resubmission / post-submit turn
                    if last_user.contains("ESCALATION") {
                        sse_tool_calls(submit(script.escalation.clone()))
                    } else if last_user.contains("auditor") {
                        let v = audit_left
                            .lock()
                            .unwrap()
                            .first()
                            .cloned()
                            .unwrap_or("PASS".into());
                        sse_tool_calls(submit(json!({"verdict": v, "findings": [],
                            "required_fixes": []})))
                    } else {
                        sse_tool_calls(submit(script.plan_payload.clone()))
                    }
                } else if last_user.contains("ESCALATION") {
                    sse_tool_calls(submit(script.escalation.clone()))
                } else if last_user.contains("ROLE: auditor") {
                    let v = {
                        let mut q = audit_left.lock().unwrap();
                        if q.len() > 1 {
                            q.remove(0)
                        } else {
                            q.first().cloned().unwrap_or("PASS".into())
                        }
                    };
                    sse_tool_calls(submit(json!({
                        "verdict": v,
                        "findings": [{"severity":"blocker","detail":"missing marker"}],
                        "required_fixes": ["create out/fix.txt containing fixed"]})))
                } else {
                    sse_tool_calls(submit(script.plan_payload.clone()))
                }
            } else if last["role"] == "tool" {
                sse_text("done")
            } else if last_user.contains("REPAIR ROUND") || last_user.contains("AUDIT REPAIR") {
                match &script.worker_repair {
                    v if v.is_array() => sse_tool_calls(v.clone()),
                    v if v.as_str() == Some("text") => sse_text("nothing to fix"),
                    _ => sse_text("done"),
                }
            } else {
                let routed = script
                    .worker_routes
                    .iter()
                    .find(|(m, _)| last_user.contains(m))
                    .map(|(_, v)| v.clone());
                match routed.or_else(|| {
                    if script.worker_first.is_array() {
                        Some(script.worker_first.clone())
                    } else {
                        None
                    }
                }) {
                    Some(v) => sse_tool_calls(v),
                    None => sse_text("done"),
                }
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.flush();
        }
    });
    port
}

// ── fixtures ───────────────────────────────────────────────────────────

fn git(repo: &PathBuf, args: &[&str]) {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(o.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&o.stderr));
}

fn fixture_repo() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sui-mtest-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    dir
}

fn repo_clean(repo: &PathBuf) -> bool {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    o.status.success() && String::from_utf8_lossy(&o.stdout).trim().is_empty()
}

fn head(repo: &PathBuf) -> String {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&o.stdout).trim().to_string()
}

fn cfg(port: u16, repo: &PathBuf) -> MissionCfg {
    let prof = |name: &str| Profile {
        name: name.into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "mock".into(),
        api_key: None,
        prompt_cache_key: None,
        pricing: None,
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    MissionCfg {
        repo: repo.clone(),
        run_dir: std::env::temp_dir().join(format!("sui-mrun-{ts}")),
        control: prof("strong"),
        worker: prof("cheap"),
        objective: "test objective".into(),
        max_workers: 1,
        session: format!("t{ts}"),
        keep_worktrees: false,
        request_timeout: Duration::from_secs(10),
        task_timeout: Duration::from_secs(60),
        context_budget: 120_000,
        context_reserve: 8_192,
        control_max_turns: 10,
        worker_max_turns: 10,
        events: None,
        cancel: None,
        session_approve: None,
    }
}

fn good_plan(base: &str) -> Value {
    json!({
        "objective": "test objective",
        "base_commit": base,
        "tasks": [{
            "id": "W1",
            "objective": "create out/ok.txt",
            "owned_paths": ["out/**"],
            "read_paths": [],
            "depends_on": [],
            "acceptance": ["test -f out/ok.txt && grep -q ok out/ok.txt"]
        }],
        "integration_checks": ["test -f out/ok.txt"]
    })
}

fn worker_writes(file: &str, content: &str) -> Value {
    json!([tc("w1", "write_file", &json!({"path": file, "content": content}).to_string())])
}

// ── tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn mission_success() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let port = mock(Script {
        plan_payload: good_plan(&base),
        worker_routes: vec![],
        worker_first: worker_writes("out/ok.txt", "ok\n"),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert_eq!(r.outcome, "accepted");
    assert!(repo_clean(&repo), "original checkout must be untouched");
    assert_eq!(head(&repo), base, "original HEAD must not move");
    assert_eq!(r.worker_usage.requests, 2, "worker: write + done");
    assert!(r.control_usage.requests >= 2, "orchestrator + auditor");
    // accepted candidate is a durable ref even after worktree cleanup —
    // the audit + gate records in the journal bind to this sha
    let sha = r.accepted_sha.expect("accepted sha recorded");
    let o = std::process::Command::new("git")
        .arg("-C").arg(&repo)
        .args(["rev-parse", "--verify", &format!("{}^{{commit}}", r.branch.unwrap())])
        .output().unwrap();
    assert!(o.status.success(), "integration branch must survive cleanup");
    assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), sha);
}

#[tokio::test]
async fn mission_malformed_plan() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let port = mock(Script {
        plan_payload: json!({"not": "a plan"}),
        worker_routes: vec![],
        worker_first: worker_writes("out/ok.txt", "ok\n"),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert!(r.outcome.starts_with("failed"), "got {}", r.outcome);
    assert!(r.outcome.contains("no valid plan"), "{}", r.outcome);
    assert!(repo_clean(&repo));
}

#[tokio::test]
async fn mission_overlapping_ownership() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let mut plan = good_plan(&base);
    plan["tasks"] = json!([
        {"id":"W1","objective":"a","owned_paths":["src/**"],"acceptance":["true"]},
        {"id":"W2","objective":"b","owned_paths":["src/x.rs"],"acceptance":["true"]},
    ]);
    let port = mock(Script {
        plan_payload: plan,
        worker_routes: vec![],
        worker_first: worker_writes("out/ok.txt", "ok\n"),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert!(r.outcome.contains("no valid plan"), "{}", r.outcome);
    assert!(repo_clean(&repo));
}

#[tokio::test]
async fn mission_out_of_scope() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let port = mock(Script {
        plan_payload: good_plan(&base),
        // worker writes owned file AND an out-of-scope file; repair does nothing
        worker_routes: vec![],
        worker_first: json!([
            tc("w1","write_file",&json!({"path":"out/ok.txt","content":"ok\n"}).to_string()),
            tc("w2","write_file",&json!({"path":"evil.txt","content":"x\n"}).to_string()),
        ]),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert!(r.outcome.contains("escalat") || r.outcome.contains("aborted")
        || r.outcome.contains("failed"), "{}", r.outcome);
    assert!(repo_clean(&repo), "out-of-scope write must not touch repo");
    assert_eq!(head(&repo), base);
}

#[tokio::test]
async fn mission_acceptance_failure_then_escalation_abort() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let mut plan = good_plan(&base);
    plan["tasks"][0]["acceptance"] = json!(["exit 7"]);
    let port = mock(Script {
        plan_payload: plan,
        worker_routes: vec![],
        worker_first: worker_writes("out/ok.txt", "ok\n"),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort", "reason": "unfixable"}),
    });
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert!(r.outcome.contains("aborted"), "{}", r.outcome);
    assert!(repo_clean(&repo));
}

#[tokio::test]
async fn mission_audit_fail_then_repair_then_pass() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let port = mock(Script {
        plan_payload: good_plan(&base),
        worker_routes: vec![],
        worker_first: worker_writes("out/ok.txt", "ok\n"),
        // audit repair writes the required fix file
        worker_repair: worker_writes("out/fix.txt", "fixed\n"),
        audit_verdicts: vec!["FAIL".into(), "PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert_eq!(r.outcome, "accepted", "tasks: {:?}", r.tasks);
    assert_eq!(r.repairs, 1, "one audit repair round");
    assert!(repo_clean(&repo));
}

#[tokio::test]
async fn mission_repair_budget_exhausted() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let mut plan = good_plan(&base);
    plan["tasks"][0]["acceptance"] = json!(["exit 7"]);
    let port = mock(Script {
        plan_payload: plan,
        worker_routes: vec![],
        worker_first: worker_writes("out/ok.txt", "ok\n"),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        // escalation retries with a contract that still can't pass
        escalation: json!({
            "decision": "retry",
            "revised_task": {
                "id":"W1b","objective":"retry create","owned_paths":["out2/**"],
                "acceptance":["exit 7"]
            },
            "reason": "try different dir"
        }),
    });
    let _ = base;
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert!(r.outcome.starts_with("failed"), "{}", r.outcome);
    assert!(r.escalations >= 1);
    assert!(repo_clean(&repo));
}

#[tokio::test]
async fn mission_cancelled_midtask() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let port = mock(Script {
        plan_payload: good_plan(&base),
        // worker runs a long bash — mission gets SIGINT during it
        worker_routes: vec![],
        worker_first: json!([tc("w1","bash", &json!({"command":"sleep 30"}).to_string())]),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let c = cfg(port, &repo);
    let h = tokio::spawn(async move { mission::run(c).await });
    // wait for the worker's bash to be in flight, then SIGINT ourselves
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let pid = std::process::id().to_string();
    std::process::Command::new("kill")
        .args(["-INT", &pid])
        .status()
        .unwrap();
    let r = h.await.unwrap().unwrap();
    assert!(r.outcome.contains("cancelled") || r.outcome.starts_with("failed"),
        "{}", r.outcome);
    assert!(repo_clean(&repo));
    // no orphaned sleep
    let orphans = std::process::Command::new("pgrep")
        .args(["-x", "sleep"])
        .output()
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&orphans.stdout).contains("sleep")
            || String::from_utf8_lossy(&orphans.stdout).trim().is_empty(),
        "orphaned sleep survived cancel"
    );
}

#[tokio::test]
async fn mission_out_of_scope_after_commit() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    // worker writes the owned file, then STAGES AND COMMITS an
    // out-of-scope file inside its worktree before finishing — the
    // ownership check must still see it (diff vs task base, not index).
    let port = mock(Script {
        plan_payload: good_plan(&base),
        worker_routes: vec![],
        worker_first: json!([
            tc("w1","write_file",&json!({"path":"out/ok.txt","content":"ok\n"}).to_string()),
            tc("w2","bash",&json!({"command":"echo x > evil.txt && git add -A && git -c user.email=t@t -c user.name=t commit -qm sneak"}).to_string()),
        ]),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let r = mission::run(cfg(port, &repo)).await.unwrap();
    assert!(r.outcome.starts_with("failed"), "{}", r.outcome);
    assert!(repo_clean(&repo));
    assert_eq!(head(&repo), base);
}

#[tokio::test]
async fn mission_two_workers_parallel_wave() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let mut plan = good_plan(&base);
    plan["tasks"] = json!([
        {"id":"W1","objective":"create out/ok.txt","owned_paths":["out/**"],
         "acceptance":["test -f out/ok.txt"]},
        {"id":"W2","objective":"create lib/lib.txt","owned_paths":["lib/**"],
         "acceptance":["test -f lib/lib.txt"]},
    ]);
    plan["integration_checks"] = json!(["test -f out/ok.txt && test -f lib/lib.txt"]);
    let port = mock(Script {
        plan_payload: plan,
        worker_first: json!("none"),
        worker_routes: vec![
            ("TASK W2", worker_writes("lib/lib.txt", "l\n")),
            ("TASK W1", worker_writes("out/ok.txt", "ok\n")),
        ],
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let mut c = cfg(port, &repo);
    c.max_workers = 2;
    let r = mission::run(c).await.unwrap();
    assert_eq!(r.outcome, "accepted", "tasks: {:?}", r.tasks);
    assert!(repo_clean(&repo));
}

/// Full pipeline: run a real (mocked) mission, then export its journal
/// directory — the same code path the TUI Export action uses.
#[tokio::test]
async fn mission_export_report() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let port = mock(Script {
        plan_payload: good_plan(&base),
        worker_routes: vec![],
        worker_first: worker_writes("out/ok.txt", "ok\n"),
        worker_repair: json!("text"),
        audit_verdicts: vec!["PASS".into()],
        escalation: json!({"decision": "abort"}),
    });
    let mut c = cfg(port, &repo);
    // run_dir must sit under a "runs" root so the exporter can resolve it
    let runs = std::env::temp_dir().join(format!("sui-expruns-{}", std::process::id()));
    let out = runs.join("../exp-out");
    c.run_dir = runs.join("m-1");
    let r = mission::run(c).await.unwrap();
    assert_eq!(r.outcome, "accepted");

    let p = sui::export::run_export(&sui::export::ExportOpts {
        run_id: Some("m-1".into()),
        latest_for_workspace: None,
        format: sui::export::Format::Markdown,
        include_diff: true,
        runs_root: Some(runs.clone()),
        out_root: Some(out),
        running: false,
    })
    .unwrap();
    let md = std::fs::read_to_string(&p).unwrap();
    for must in [
        "Run overview",
        "test objective",
        "mission",
        "Planning",
        "w-W1",
        "Executed successfully",
        "acceptance",
        "PASS",
        "accepted sha",
        "Usage and timing",
        "Limitations",
    ] {
        assert!(md.contains(must), "report missing {must:?}:\n{md}");
    }
    // gate evidence shows the acceptance command + exit code, not claims
    assert!(md.contains("test -f out/ok.txt"), "acceptance cmd recorded");
    // secrets never appear
    assert!(!md.contains("api_key"));
}
