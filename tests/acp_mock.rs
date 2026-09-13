//! ACP backend verification: a scenario-driven mock agent
// the LOCK mutex intentionally serializes env-mutating tests across awaits
#![allow(clippy::await_holding_lock)]
//! (tests/mock_acp.py) over real JSON-RPC stdio — the same wire the
//! official SDK speaks. Covers spawn hygiene, permission routing,
//! cancellation, crash poisoning, artifact bridge, and the mission-level
//! native-planner → ACP-worker → native-auditor certification.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use sui::acp::bridge;
use sui::acp::driver::{AcpSession, BridgeCfg};
use sui::acp::norm::Norm;
use sui::backend::Backend;
use sui::config::AcpSpec;
use sui::events::{GateChoice, UiEvent};
use sui::journal::Journal;
use sui::mission::{self, MissionCfg};
use sui::permission::Gate;

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
fn lock() -> &'static Mutex<()> {
    LOCK.get_or_init(|| Mutex::new(()))
}

fn mock_py() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/mock_acp.py")
}

/// current_exe() inside tests is the test binary — the bridge must be
/// the real `sui` binary built alongside the test profile.
fn ensure_bridge_exe() {
    let exe = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/debug/sui");
    unsafe { std::env::set_var("SUI_ACP_BRIDGE_EXE", &exe) };
}

fn spec(scenario: &str, extra_env: &[(&str, &str)]) -> AcpSpec {
    ensure_bridge_exe();
    // SAFETY: tests serialize on LOCK; env mutation is contained to the
    // child env whitelist (the process env itself is only read).
    let mut env_allow = vec!["MOCK_ACP_SCENARIO".to_string()];
    for (k, v) in extra_env {
        env_allow.push(k.to_string());
        unsafe { std::env::set_var(k, v) };
    }
    unsafe { std::env::set_var("MOCK_ACP_SCENARIO", scenario) };
    AcpSpec {
        name: "mock-acp".into(),
        command: "python3".into(),
        args: vec![mock_py().to_string_lossy().to_string()],
        env_allow,
        model: None,
        approved: true,
    }
}

struct Ctx {
    dir: PathBuf,
    journal: Arc<Mutex<Journal>>,
}

fn ctx(name: &str) -> Ctx {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("sui-acp-{name}-{ts}"));
    std::fs::create_dir_all(&dir).unwrap();
    Ctx {
        journal: Arc::new(Mutex::new(Journal::open_named(&dir, "acp-test").unwrap())),
        dir,
    }
}

async fn spawn(
    scenario: &str,
    extra_env: &[(&str, &str)],
    gate: Gate,
    events: Option<tokio::sync::mpsc::UnboundedSender<UiEvent>>,
    bridge: Option<BridgeCfg>,
    model: Option<String>,
) -> (AcpSession, Ctx) {
    let c = ctx(scenario);
    let mut s = spec(scenario, extra_env);
    s.model = model;
    let norm = Arc::new(Mutex::new(Norm::new(
        7,
        "acp:mock".into(),
        events,
        c.journal.clone(),
    )));
    let sess = AcpSession::spawn(
        "t1".into(),
        s,
        &c.dir,
        bridge,
        norm,
        Arc::new(tokio::sync::Mutex::new(gate)),
        c.journal.clone(),
        7,
    )
    .await
    .unwrap();
    (sess, c)
}

fn journal_lines(c: &Ctx) -> Vec<Value> {
    std::fs::read_to_string(c.dir.join("acp-test.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .collect()
}

#[tokio::test]
async fn acp_happy_path_streams_events() {
    let _g = lock().lock().unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let (s, c) = spawn("happy", &[], Gate::new(true), Some(tx), None, None).await;
    assert_eq!(s.session_id().as_deref(), Some("mock-sess-1"));
    assert!(!s.config_options().is_empty(), "model option advertised");
    let end = s.prompt("do a thing").await.unwrap();
    assert!(matches!(
        end.stop,
        agent_client_protocol::schema::v1::StopReason::EndTurn
    ));
    s.shutdown().await;
    // collected events: ReqStart, Reason, Delta(s), ReqDone
    let mut saw_reason = false;
    let mut saw_delta = false;
    let mut saw_done = false;
    while let Ok(e) = rx.try_recv() {
        match e {
            UiEvent::Reason { .. } => saw_reason = true,
            UiEvent::Delta { .. } => saw_delta = true,
            UiEvent::ReqDone { ok, .. } => {
                saw_done = true;
                assert!(ok);
            }
            _ => {}
        }
    }
    assert!(saw_reason && saw_delta && saw_done, "stream normalized");
    let kinds: Vec<String> = journal_lines(&c)
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(kinds.contains(&"acp_spawn".to_string()));
    assert!(kinds.contains(&"acp_session".to_string()));
    assert!(kinds.contains(&"acp_prompt".to_string()));
}

#[tokio::test]
async fn acp_env_scrubbed_no_secrets() {
    let _g = lock().lock().unwrap();
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-should-not-leak");
        std::env::set_var("AWS_SECRET_ACCESS_KEY", "x");
    }
    let (s, c) = spawn("happy", &[], Gate::new(true), None, None, None).await;
    s.shutdown().await;
    let spawn_ev = journal_lines(&c)
        .into_iter()
        .find(|e| e["type"] == "acp_spawn")
        .unwrap();
    let names: Vec<String> = spawn_ev["data"]["env_names"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    assert!(
        !names
            .iter()
            .any(|n| n.contains("KEY") || n.contains("SECRET")),
        "leaked: {names:?}"
    );
    assert!(names.contains(&"PATH".to_string()));
}

#[tokio::test]
async fn acp_permission_allow_and_deny() {
    let _g = lock().lock().unwrap();
    // allow: auto gate → allow_always picked; mock completes end_turn
    let (s, c) = spawn("permission", &[], Gate::new(true), None, None, None).await;
    let end = s.prompt("go").await.unwrap();
    assert!(
        matches!(
            end.stop,
            agent_client_protocol::schema::v1::StopReason::EndTurn
        ),
        "stop: {:?}",
        end.stop
    );
    s.shutdown().await;
    let perm = journal_lines(&c)
        .into_iter()
        .find(|e| e["type"] == "acp_permission")
        .expect("permission decision journaled");
    assert_eq!(perm["data"]["decision"].as_str().unwrap(), "Session");

    // deny: UI gate answered with Deny → cancelled outcome → mock still
    // ends the turn; the journal records the denial
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut gate = Gate::new(false);
    let cancel = Arc::new(AtomicBool::new(false));
    gate.set_ui(tx, cancel, None);
    tokio::spawn(async move {
        while let Some(e) = rx.recv().await {
            if let UiEvent::Permission { reply, .. } = e {
                let _ = reply.send(GateChoice::Deny);
            }
        }
    });
    let (tx2, _rx2) = tokio::sync::mpsc::unbounded_channel();
    let (s2, c2) = spawn("deny", &[], gate, Some(tx2), None, None).await;
    let end = s2.prompt("go").await.unwrap();
    assert!(matches!(
        end.stop,
        agent_client_protocol::schema::v1::StopReason::EndTurn
    ));
    s2.shutdown().await;
    let perm = journal_lines(&c2)
        .into_iter()
        .find(|e| e["type"] == "acp_permission")
        .unwrap();
    assert_eq!(perm["data"]["decision"].as_str().unwrap(), "Deny");
}

#[tokio::test]
async fn acp_cancel_protocol_first() {
    let _g = lock().lock().unwrap();
    let (s, _c) = spawn("hang", &[], Gate::new(true), None, None, None).await;
    let s2 = s.clone();
    let h = tokio::spawn(async move { s2.prompt("never ends").await });
    tokio::time::sleep(Duration::from_millis(800)).await;
    s.cancel();
    let end = tokio::time::timeout(Duration::from_secs(10), h)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(
        end.stop,
        agent_client_protocol::schema::v1::StopReason::Cancelled
    ));
    s.shutdown().await;
    assert!(s.dead());
}

#[tokio::test]
async fn acp_crash_poisons_session() {
    let _g = lock().lock().unwrap();
    let (s, c) = spawn("crash", &[], Gate::new(true), None, None, None).await;
    let r = s.prompt("boom").await;
    assert!(r.is_err(), "mid-prompt crash surfaces");
    // bounded wait for teardown
    for _ in 0..50 {
        if s.dead() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(s.dead(), "session marked dead after crash");
    let err = s.err().unwrap_or_default();
    assert!(
        err.contains("stderr") || !err.is_empty(),
        "stderr tail kept: {err}"
    );
    let kinds: Vec<String> = journal_lines(&c)
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    assert!(kinds.contains(&"acp_exit".to_string()));
}

#[tokio::test]
async fn acp_artifact_bridge_roundtrip() {
    let _g = lock().lock().unwrap();
    let c0 = ctx("artifacts");
    let dir = c0.dir.join("drops");
    let bc = BridgeCfg {
        dir: dir.clone(),
        expect: "verdict".into(),
    };
    let (s, c) = spawn(
        "artifact",
        &[(
            "MOCK_ACP_ARTIFACT",
            r#"{"verdict":"PASS","findings":[],"required_fixes":[]}"#,
        )],
        Gate::new(true),
        None,
        Some(bc),
        None,
    )
    .await;
    s.prompt("submit your verdict").await.unwrap();
    s.shutdown().await;
    let arts = bridge::read_artifacts(&dir);
    assert_eq!(arts.len(), 1, "one artifact dropped");
    assert_eq!(arts[0]["verdict"], "PASS");
    let _ = c;
}

#[tokio::test]
async fn acp_bad_artifact_rejected_by_bridge() {
    let _g = lock().lock().unwrap();
    let c0 = ctx("bad-art");
    let dir = c0.dir.join("drops");
    let bc = BridgeCfg {
        dir: dir.clone(),
        expect: "verdict".into(),
    };
    let (s, _c) = spawn("bad_artifact", &[], Gate::new(true), None, Some(bc), None).await;
    s.prompt("submit").await.unwrap();
    s.shutdown().await;
    assert!(
        bridge::read_artifacts(&dir).is_empty(),
        "invalid payload must not drop"
    );
}

#[tokio::test]
async fn acp_quota_error_surfaces() {
    let _g = lock().lock().unwrap();
    let (s, _c) = spawn("quota", &[], Gate::new(true), None, None, None).await;
    let e = s.prompt("go").await.err().expect("quota error");
    assert!(e.to_string().contains("quota"), "{e}");
    s.shutdown().await;
}

#[tokio::test]
async fn acp_model_via_session_config() {
    let _g = lock().lock().unwrap();
    // spec.model + advertised Model option → set_config_option applied
    let (s, _c) = spawn(
        "happy",
        &[],
        Gate::new(true),
        None,
        None,
        Some("mock-2".into()),
    )
    .await;
    // and explicit set_config on the live session
    s.set_config("model", "mock-1").await.unwrap();
    s.shutdown().await;

    // no advertised options → no crash, no request
    let (s2, _c2) = spawn(
        "no_model_opts",
        &[],
        Gate::new(true),
        None,
        None,
        Some("mock-2".into()),
    )
    .await;
    assert!(s2.config_options().is_empty());
    s2.prompt("hi").await.unwrap();
    s2.shutdown().await;
}

#[tokio::test]
async fn acp_unapproved_agent_refused() {
    let _g = lock().lock().unwrap();
    let c = ctx("unapproved");
    let mut s = spec("happy", &[]);
    s.approved = false;
    let norm = Arc::new(Mutex::new(Norm::new(
        7,
        "a".into(),
        None,
        c.journal.clone(),
    )));
    let r = AcpSession::spawn(
        "x".into(),
        s,
        &c.dir,
        None,
        norm,
        Arc::new(tokio::sync::Mutex::new(Gate::new(true))),
        c.journal.clone(),
        7,
    )
    .await;
    match r {
        Ok(_) => panic!("unapproved agent must not spawn"),
        Err(e) => assert!(e.to_string().contains("not approved")),
    }
}

// ── mission-level certification ──────────────────────────────────────
// native planner (in-process SSE mock) → ACP worker (mock_acp.py writes
// real files in the worktree) → native auditor. Proves the mission driver
// gates an external agent's work with the same ownership/acceptance/
// audit pipeline — the agent's end_turn is never the proof.

mod sse {
    // reuse the in-process SSE mock pattern: a tiny one here to keep the
    // ACP test file self-contained
    use serde_json::{json, Value};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    pub fn tool_calls(calls: Value) -> String {
        let d = json!({"choices": [{"index": 0, "delta": {"role": "assistant",
            "tool_calls": calls}, "finish_reason": "tool_calls"}]});
        format!("data: {d}\n\ndata: [DONE]\n\n")
    }
    pub fn text(t: &str) -> String {
        let d = json!({"choices": [{"index": 0, "delta": {"role": "assistant",
            "content": t}, "finish_reason": "stop"}]});
        format!("data: {d}\n\ndata: [DONE]\n\n")
    }
    pub fn submit(payload: Value) -> Value {
        json!([{"id":"s1","type":"function","function":{"name":"submit_result",
            "arguments": json!({"payload": payload}).to_string()}}])
    }

    /// plan on first control turn, verdict thereafter; workers get text.
    pub fn serve(plan: Value, verdict: &'static str) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let plan = Mutex::new(plan);
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let mut s = match conn {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 {
                        break;
                    }
                    let t = line.trim().to_string();
                    if t.is_empty() {
                        break;
                    }
                    if t.to_lowercase().starts_with("content-length:") {
                        len = t[15..].trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let req: Value = serde_json::from_slice(&body).unwrap_or_default();
                let msgs = req["messages"].as_array().cloned().unwrap_or_default();
                let last_user = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                let is_tool = msgs.last().map(|m| m["role"] == "tool").unwrap_or(false);
                let body = if last_user.contains("ROLE: auditor")
                    || is_tool && last_user.contains("audit")
                {
                    tool_calls(submit(json!({"verdict": verdict, "findings": [],
                        "required_fixes": []})))
                } else if last_user.contains("objective") || is_tool {
                    // orchestrator plans; post-submit turns just end
                    if is_tool {
                        text("done")
                    } else {
                        tool_calls(submit(plan.lock().unwrap().clone()))
                    }
                } else {
                    text("done")
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body);
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        port
    }
}

fn git(repo: &PathBuf, args: &[&str]) {
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        o.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&o.stderr)
    );
}

fn fixture_repo() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("sui-acpm-{ts}"));
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q"]);
    git(&dir, &["config", "user.email", "t@t"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-qm", "init"]);
    dir
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

#[tokio::test]
async fn mission_native_plan_acp_worker_native_audit() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let plan = json!({
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
    });
    let port = sse::serve(plan, "PASS");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prof = |name: &str| sui::config::Profile {
        name: name.into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "mock".into(),
        api_key: None,
        prompt_cache_key: None,
        pricing: None,
    };
    // worker does real work in its worktree via MOCK_ACP_CMD
    let mut wspec = spec(
        "worker",
        &[("MOCK_ACP_CMD", "mkdir -p out && echo ok > out/ok.txt")],
    );
    wspec.name = "mock-worker".into();
    let cfg = MissionCfg {
        repo: repo.clone(),
        run_dir: std::env::temp_dir().join(format!("sui-acpmrun-{ts}")),
        control: Backend::Native(prof("strong")),
        worker: Backend::Acp(wspec),
        auditor: None,
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
        run: 1,
    };
    let r = mission::run(cfg).await.unwrap();
    assert_eq!(r.outcome, "accepted", "tasks: {:?}", r.tasks);
    assert_eq!(head(&repo), base, "original HEAD must not move");
    // ACP journal exists with spawn+prompt evidence; usage is NOT folded
    // into native request aggregates
    let acp_journal = r.run_dir.join("acp-W1.jsonl");
    assert!(acp_journal.exists(), "acp worker journal written");
    assert_eq!(
        r.worker_usage.requests, 0,
        "external prompts are not native requests"
    );
}

#[tokio::test]
async fn mission_acp_worker_ownership_violation_fails() {
    let _g = lock().lock().unwrap();
    let repo = fixture_repo();
    let base = head(&repo);
    let plan = json!({
        "objective": "test objective",
        "base_commit": base,
        "tasks": [{
            "id": "W1", "objective": "create out/ok.txt",
            "owned_paths": ["out/**"], "read_paths": [], "depends_on": [],
            "acceptance": ["test -f out/ok.txt"]
        }],
        "integration_checks": ["test -f out/ok.txt"]
    });
    let port = sse::serve(plan, "PASS");
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let prof = |name: &str| sui::config::Profile {
        name: name.into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "mock".into(),
        api_key: None,
        prompt_cache_key: None,
        pricing: None,
    };
    // worker writes BOTH the owned file and an out-of-scope file — the
    // deterministic ownership gate must catch it regardless of ACP claims
    let mut wspec = spec(
        "worker",
        &[(
            "MOCK_ACP_CMD",
            "mkdir -p out && echo ok > out/ok.txt && echo x > evil.txt",
        )],
    );
    wspec.name = "mock-worker".into();
    let cfg = MissionCfg {
        repo: repo.clone(),
        run_dir: std::env::temp_dir().join(format!("sui-acpmrun2-{ts}")),
        control: Backend::Native(prof("strong")),
        worker: Backend::Acp(wspec),
        auditor: None,
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
        run: 1,
    };
    let r = mission::run(cfg).await.unwrap();
    assert!(r.outcome.starts_with("failed"), "{}", r.outcome);
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).trim().is_empty(),
        "original checkout untouched"
    );
    assert_eq!(head(&repo), base);
}

/// Real `devin acp` smoke — manual: `cargo test --test acp_mock devin_smoke -- --ignored`.
/// Skipped by default; requires the devin CLI installed + authenticated.
#[tokio::test]
#[ignore]
async fn devin_acp_smoke() {
    if std::process::Command::new("devin")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("devin not installed; skipping");
        return;
    }
    let c = ctx("devin");
    let spec = AcpSpec {
        name: "devin".into(),
        command: "devin".into(),
        args: vec!["acp".into()],
        env_allow: vec![],
        model: None,
        approved: true,
    };
    let norm = Arc::new(Mutex::new(Norm::new(
        9,
        "acp:devin".into(),
        None,
        c.journal.clone(),
    )));
    let s = AcpSession::spawn(
        "devin".into(),
        spec,
        &c.dir,
        None,
        norm,
        Arc::new(tokio::sync::Mutex::new(Gate::new(true))),
        c.journal.clone(),
        9,
    )
    .await
    .expect("devin acp handshake");
    let sid = s.session_id();
    println!("session: {sid:?} · config opts: {:?}", s.config_options());
    let end = tokio::time::timeout(Duration::from_secs(240), s.prompt("Reply with exactly: OK"))
        .await
        .expect("prompt timed out")
        .expect("prompt failed");
    println!("stop: {:?}", end.stop);
    s.shutdown().await;
    assert!(!s.err().is_some() || s.session_id().is_some());
}
