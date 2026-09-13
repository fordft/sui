//! TUI integration: drive App state through real agent/mission core
//! against an in-process SSE mock. No terminal required — App is pure
//! state; the run loop is exercised via its public seams.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use sui::config::{Profile, ProfileCfg, UiSettings};
use sui::events::{GateChoice, UiEvent};
use sui::mission::{self, MissionCfg};
use sui::tui::app::{
    Act, App, AuthMode, Effect, Field, Modal, Mode, ProvForm, ProvType, Role, SettingsRow, Tab,
};
use sui::tui::text::Buf;

fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

// ── scriptable SSE mock ────────────────────────────────────────────────

fn tc(id: &str, name: &str, args: &str) -> Value {
    json!({"id": id, "type": "function",
           "function": {"name": name, "arguments": args}})
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

/// Mock routing: "control plane" system → submit_result(plan); worker with
/// last msg role=tool → text; user msg containing WRITEME → write_file;
/// containing LONGTASK → bash sleep; else text "done".
/// WRITEME3 issues three sequential write_file calls; a denial result or
/// three tool messages ends the turn. `gates` can hold each follow-up
/// call on a flag so tests control exactly when the next check happens.
fn mock(plan: Value) -> u16 {
    mock_inner(plan, None)
}

fn mock_gated(plan: Value, gates: [std::sync::Arc<std::sync::atomic::AtomicBool>; 2]) -> u16 {
    mock_inner(plan, Some(gates))
}

fn mock_inner(
    plan: Value,
    gates: Option<[std::sync::Arc<std::sync::atomic::AtomicBool>; 2]>,
) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
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
                if line.trim().is_empty() {
                    break;
                }
                if line.trim().to_lowercase().starts_with("content-length:") {
                    len = line.trim()[15..].trim().parse().unwrap_or(0);
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

            let n_tools = msgs.iter().filter(|m| m["role"] == "tool").count();
            let body = if system.contains("control plane") {
                if last_user.contains("auditor") {
                    let sub = json!({"payload": {"verdict": "PASS", "findings": [],
                        "required_fixes": []}});
                    sse_tool_calls(json!([tc("s1", "submit_result", &sub.to_string())]))
                } else {
                    let sub = json!({"payload": plan});
                    sse_tool_calls(json!([tc("s1", "submit_result", &sub.to_string())]))
                }
            } else if last_user.contains("WRITEME3") {
                // three sequential protected writes — exercises live policy
                // changes mid-run without respawning the agent
                let last_tool = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "tool")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("");
                if n_tools >= 3 || last_tool.contains("denied") {
                    sse_text("done")
                } else {
                    if let Some(g) = &gates {
                        // hold write#2 on gates[0], write#3 on gates[1]
                        if (1..=2).contains(&n_tools) {
                            let flag = &g[n_tools - 1];
                            let t0 = std::time::Instant::now();
                            while !flag.load(std::sync::atomic::Ordering::Relaxed)
                                && t0.elapsed() < Duration::from_secs(10)
                            {
                                std::thread::sleep(Duration::from_millis(5));
                            }
                        }
                    }
                    sse_tool_calls(json!([tc(
                        "w",
                        "write_file",
                        &json!({"path": format!("out/tui{}.txt", n_tools + 1),
                                "content": "written"})
                        .to_string()
                    )]))
                }
            } else if last["role"] == "tool" {
                sse_text("done")
            } else if last_user.contains("WRITEME") {
                sse_tool_calls(json!([tc(
                    "w1",
                    "write_file",
                    &json!({"path": "out/tui.txt", "content": "written by worker"}).to_string()
                )]))
            } else if last_user.contains("LONGTASK") {
                sse_tool_calls(json!([tc(
                    "b1",
                    "bash",
                    &json!({"command": "sleep 30"}).to_string()
                )]))
            } else {
                sse_text("ack from mock")
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

fn fixture_repo() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "sui-tui-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let git = |a: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(a)
            .output()
            .unwrap();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(dir.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    dir
}

fn head(repo: &PathBuf) -> String {
    String::from_utf8_lossy(
        &std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap()
            .stdout,
    )
    .trim()
    .to_string()
}

fn jdir() -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "sui-tui-j-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn app_with_mock(repo: &PathBuf, port: u16) -> App {
    let mut profiles = BTreeMap::new();
    let pc = |model: &str| ProfileCfg {
        base_url: Some(format!("http://127.0.0.1:{port}/v1")),
        model: Some(model.into()),
        key_env: None,
        api_key: None,
        prompt_cache_key: None,
        pricing: None,
    };
    profiles.insert("mock-ctrl".into(), pc("ctrl-model"));
    profiles.insert("mock-worker".into(), pc("work-model"));
    let ui = UiSettings {
        workspace: Some(repo.to_string_lossy().into()),
        mode: Some("solo".into()),
        solo_profile: Some("mock-worker".into()),
        orchestrator_profile: Some("mock-ctrl".into()),
        worker_profile: Some("mock-worker".into()),
        auditor_profile: None,
        worker_count: Some(1),
        reasoning: None,
        mouse: None,
        acceptance: vec![],
    };
    App::with_state(repo.clone(), profiles, ui)
}

/// Drain core events into the app until RunDone (or timeout).
async fn pump(app: &mut App, rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
            Ok(Some(e)) => {
                let done = matches!(e, UiEvent::RunDone { .. });
                app.apply_event(e);
                if done {
                    return true;
                }
            }
            _ => {
                if std::time::Instant::now() > deadline {
                    return false;
                }
            }
        }
    }
}

// ── tests ──────────────────────────────────────────────────────────────

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_solo_write_with_permission_modal() {
    let repo = fixture_repo();
    let port = mock(json!({}));
    let mut app = app_with_mock(&repo, port);

    // type a task and send it
    app.input.set("WRITEME please");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.running);
    let eff = app.effects.pop().expect("SendTask effect");
    let (task, run) = match eff {
        Effect::SendTask { task, mode, run } => {
            assert_eq!(mode, Mode::Solo);
            (task, run)
        }
        _ => panic!("expected SendTask"),
    };

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
    let solo = sui::tui::spawn_solo(
        prof,
        repo.clone(),
        jdir(),
        ev_tx,
        app.cancel.clone(),
        app.stop_flag.clone(),
        app.auto.clone(),
    );
    solo.send(run, task);

    // pump until the write_file permission modal appears
    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        if matches!(ev, UiEvent::RunDone { .. }) {
            break;
        }
        app.apply_event(ev);
        n += 1;
    }
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));

    // approve once via the modal key path
    app.key(key('y'));
    assert!(app.modal.is_none());

    assert!(pump(&mut app, &mut ev_rx).await, "run never finished");
    assert!(repo.join("out/tui.txt").exists(), "tool ran: file written");
    assert!(!app.running);
    // assistant text + tool record landed in the run's activity group
    assert!(
        app.groups
            .iter()
            .flat_map(|g| g.items.iter())
            .any(|it| matches!(
                it,
                Act::Tool {
                    status: Some(_),
                    ..
                }
            )),
        "a finished tool record landed in the transcript"
    );
    assert!(app.usage.values().any(|(_, u)| u.requests > 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_solo_permission_denied() {
    let repo = fixture_repo();
    let port = mock(json!({}));
    let mut app = app_with_mock(&repo, port);

    app.input.set("WRITEME");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Effect::SendTask { task, run, .. } = app.effects.pop().unwrap() else {
        panic!()
    };

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
    let solo = sui::tui::spawn_solo(
        prof,
        repo.clone(),
        jdir(),
        ev_tx,
        app.cancel.clone(),
        app.stop_flag.clone(),
        app.auto.clone(),
    );
    solo.send(run, task);

    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply_event(ev);
        n += 1;
    }
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));
    app.key(key('n')); // deny

    assert!(pump(&mut app, &mut ev_rx).await);
    assert!(
        !repo.join("out/tui.txt").exists(),
        "denied: file not written"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_stop_cancels_running_tool() {
    let repo = fixture_repo();
    let port = mock(json!({}));
    let mut app = app_with_mock(&repo, port);

    app.input.set("LONGTASK");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Effect::SendTask { task, run, .. } = app.effects.pop().unwrap() else {
        panic!()
    };

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
    let solo = sui::tui::spawn_solo(
        prof,
        repo.clone(),
        jdir(),
        ev_tx,
        app.cancel.clone(),
        app.stop_flag.clone(),
        app.auto.clone(),
    );
    solo.send(run, task);

    // wait for the permission modal (bash needs approval), approve, then stop
    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply_event(ev);
        n += 1;
    }
    app.key(key('y'));
    tokio::time::sleep(Duration::from_millis(300)).await;
    app.stop(); // Ctrl+S path
    assert!(pump(&mut app, &mut ev_rx).await, "stop should end the run");
    assert!(!app.running);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_mission_events_flow() {
    let repo = fixture_repo();
    let base = head(&repo);
    let plan = json!({
        "objective": "t",
        "base_commit": base,
        "tasks": [{
            "id": "W1", "objective": "create out/m.txt",
            "owned_paths": ["out/**"], "read_paths": [], "depends_on": [],
            "acceptance": ["test -f out/m.txt"]
        }],
        "integration_checks": ["test -f out/m.txt"]
    });
    // worker script: WRITEME marker writes out/tui.txt — mission task asks
    // for out/m.txt; route on task content instead: the worker_first
    // writes via the write_file tool when the task text mentions m.txt
    let port = {
        // local mock variant: worker writes out/m.txt when objective mentions it
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let p = l.local_addr().unwrap().port();
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
                    if line.trim().is_empty() {
                        break;
                    }
                    if line.trim().to_lowercase().starts_with("content-length:") {
                        len = line.trim()[15..].trim().parse().unwrap_or(0);
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
                    if last_user.contains("auditor") {
                        let sub = json!({"payload": {"verdict": "PASS", "findings": [], "required_fixes": []}});
                        sse_tool_calls(json!([tc("s1", "submit_result", &sub.to_string())]))
                    } else {
                        let sub = json!({"payload": plan});
                        sse_tool_calls(json!([tc("s1", "submit_result", &sub.to_string())]))
                    }
                } else if last["role"] == "tool" {
                    sse_text("done")
                } else if last_user.contains("m.txt") {
                    sse_tool_calls(json!([tc(
                        "w1",
                        "write_file",
                        &json!({"path": "out/m.txt", "content": "ok"}).to_string()
                    )]))
                } else {
                    sse_text("done")
                };
                let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
                let _ = s.write_all(resp.as_bytes());
                let _ = s.flush();
            }
        });
        p
    };

    let mut app = app_with_mock(&repo, port);
    app.mode = Mode::Mission;
    app.input.set("build the thing");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Effect::SendTask { task, mode, run } = app.effects.pop().unwrap() else {
        panic!()
    };
    assert_eq!(mode, Mode::Mission);

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = |name: &str, model: &str| Profile {
        name: name.into(),
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: model.into(),
        api_key: None,
        prompt_cache_key: None,
        pricing: None,
    };
    let cfg = MissionCfg {
        repo: repo.clone(),
        run_dir: jdir(),
        control: prof("ctrl", "ctrl-model"),
        worker: prof("work", "work-model"),
        objective: task,
        max_workers: 1,
        session: "tuitest".into(),
        keep_worktrees: false,
        request_timeout: Duration::from_secs(10),
        task_timeout: Duration::from_secs(60),
        context_budget: 120_000,
        context_reserve: 8_192,
        control_max_turns: 10,
        worker_max_turns: 10,
        events: Some(ev_tx),
        cancel: Some((app.cancel.clone(), app.stop_flag.clone())),
        session_approve: Some(app.auto.clone()),
        run,
    };
    tokio::spawn(async move {
        let _ = mission::run(cfg).await;
    });

    assert!(pump(&mut app, &mut ev_rx).await, "mission never finished");
    assert!(!app.tasks.is_empty(), "plan tasks shown in Tasks tab");
    assert!(app.accepted_sha.is_some(), "accepted SHA surfaced");
    assert!(app.audit.is_some(), "audit payload surfaced");
    assert!(!app.usage.is_empty(), "usage rows aggregated");
    assert_eq!(app.outcome, "accepted");
}

#[test]
fn tui_settings_roles_and_forms() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);

    // first-run with no profiles → Setup + provider-type picker → form
    let mut bare = App::with_state(repo.clone(), BTreeMap::new(), UiSettings::default());
    assert!(matches!(bare.screen, sui::tui::app::Screen::Setup));
    assert!(matches!(bare.modal, Some(Modal::Picker(_))));
    bare.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // pick DeepSeek
    assert!(matches!(bare.modal, Some(Modal::Provider(_))));

    // role assignment: solo → mock-ctrl
    let rows = app.settings_rows();
    let role_idx = rows
        .iter()
        .position(|r| matches!(r, sui::tui::app::SettingsRow::Role(Role::Solo)))
        .unwrap();
    app.tab = Tab::Settings;
    app.settings_sel = role_idx;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(app.modal, Some(Modal::Picker(_))));

    // pick "mock-ctrl" from the picker
    if let Some(Modal::Picker(p)) = &mut app.modal {
        p.sel = p.items.iter().position(|i| i == "mock-ctrl").unwrap();
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    // → second picker for model; type manual id and accept via filter
    if let Some(Modal::Picker(p)) = &mut app.modal {
        p.filter.set("manual-model-x");
        p.sel = 999; // no match → filter text wins
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.role_profile(Role::Solo).as_deref(), Some("mock-ctrl"));
    assert_eq!(
        app.profiles["mock-ctrl"].model.as_deref(),
        Some("manual-model-x")
    );

    // worker concurrency toggle
    let rows = app.settings_rows();
    let w_idx = rows
        .iter()
        .position(|r| matches!(r, sui::tui::app::SettingsRow::Workers))
        .unwrap();
    app.settings_sel = w_idx;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.ui.worker_count, Some(2));

    // unicode input round-trip
    app.tab = Tab::Chat;
    app.input.set("");
    app.paste("สวัสดี ครับ — ไทย + emoji 🦀 works");
    assert!(app.input.text().contains("ไทย"));
}

/// Regression: pasting while a modal is open must land in the modal's
/// focused field — an API key pasted into setup must not reach the chat box.
#[test]
fn tui_paste_targets_modal_field() {
    let repo = fixture_repo();
    let mut bare = App::with_state(repo.clone(), BTreeMap::new(), UiSettings::default());
    // first-run: provider-type picker → choose DeepSeek → form
    bare.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(bare.modal, Some(Modal::Provider(_))));

    // focus the API-key field, paste a key
    if let Some(Modal::Provider(f)) = &mut bare.modal {
        f.focus = f.fields().iter().position(|x| *x == Field::ApiKey).unwrap();
    }
    bare.paste("sk-test-pasted-key-123");
    if let Some(Modal::Provider(f)) = &bare.modal {
        assert_eq!(f.key.text(), "sk-test-pasted-key-123");
    } else {
        panic!("provider modal vanished");
    }
    assert!(bare.input.text().is_empty(), "paste leaked into chat input");

    // DeepSeek form hides Base URL / Auth / env-var rows entirely
    if let Some(Modal::Provider(f)) = &bare.modal {
        assert!(f.fields().iter().all(|x| *x != Field::BaseUrl));
        assert!(f.fields().iter().all(|x| *x != Field::Auth));
        assert!(f.fields().iter().all(|x| *x != Field::KeyEnv));
    }

    // Custom → BaseUrl row exists and receives paste
    let mut cf = ProvForm::new(ProvType::Custom);
    assert_eq!(cf.auth, AuthMode::None); // custom endpoints default to no auth
                                         // None: no key rows at all
    assert!(cf.fields().iter().all(|x| !matches!(
        x,
        Field::ApiKey | Field::Store | Field::CredSrc | Field::KeyEnv
    )));
    cf.auth = AuthMode::ApiKey;
    let fs = cf.fields();
    assert!(fs.iter().any(|x| *x == Field::ApiKey));
    assert!(fs.iter().any(|x| *x == Field::Store));
    assert!(fs.iter().all(|x| *x != Field::KeyEnv));
    cf.auth = AuthMode::Advanced;
    let fs = cf.fields();
    assert!(fs.iter().any(|x| *x == Field::CredSrc));
    assert!(fs.iter().any(|x| *x == Field::KeyEnv));
    assert!(fs.iter().all(|x| *x != Field::ApiKey));
    assert!(fs.iter().all(|x| *x != Field::Store));

    cf.auth = AuthMode::None;
    cf.focus = cf
        .fields()
        .iter()
        .position(|x| *x == Field::BaseUrl)
        .unwrap();
    let mut app2 = App::with_state(repo.clone(), BTreeMap::new(), UiSettings::default());
    app2.modal = Some(Modal::Provider(cf));
    app2.paste("https://api.example.test/v1");
    if let Some(Modal::Provider(f)) = &app2.modal {
        assert_eq!(f.base_url.text(), "https://api.example.test/v1");
    }
    assert!(app2.input.text().is_empty());
}

// ── permission modal key semantics ────────────────────────────────────

fn perm_app(
    repo: &PathBuf,
) -> (
    App,
    tokio::sync::mpsc::UnboundedReceiver<sui::events::GateChoice>,
) {
    let mut app = app_with_mock(repo, 1); // port unused — modal injected directly
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.modal = Some(Modal::Permission {
        id: 1,
        agent: "solo".into(),
        summary: "write out/x".into(),
        reply: tx,
    });
    (app, rx)
}

#[test]
fn perm_uppercase_variants_decide() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('Y'));
    assert!(matches!(
        rx.try_recv().unwrap(),
        sui::events::GateChoice::Once
    ));

    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('A'));
    assert!(matches!(
        rx.try_recv().unwrap(),
        sui::events::GateChoice::Session
    ));
    assert!(
        app.auto.load(std::sync::atomic::Ordering::Relaxed),
        "[a] must raise the Auto badge"
    );

    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('N'));
    assert!(matches!(
        rx.try_recv().unwrap(),
        sui::events::GateChoice::Deny
    ));
}

#[test]
fn perm_enter_does_not_approve() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        matches!(app.modal, Some(Modal::Permission { .. })),
        "bare Enter must not grant approval — no selected action exists"
    );
    assert!(rx.try_recv().is_err());
}

#[test]
fn perm_release_kind_fires_shortcut() {
    // Transports that only report Release-kind char events must still
    // drive the modal; the gate is parked on this reply either way.
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(matches!(
        rx.try_recv().unwrap(),
        sui::events::GateChoice::Once
    ));
}

#[test]
fn perm_release_of_unrelated_key_ignored() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('x'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));
    assert!(rx.try_recv().is_err());
}

#[test]
fn perm_keys_never_leak_to_chat_input() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('y'));
    assert!(rx.try_recv().is_ok());
    assert!(
        app.input.text().is_empty(),
        "decision key leaked into chat input"
    );
    // the trailing Release on press+release terminals must not type either
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(
        app.input.text().is_empty(),
        "release event typed into chat input"
    );
}

#[test]
fn ctrl_s_stops_with_permission_modal_open() {
    let repo = fixture_repo();
    let (mut app, _rx) = perm_app(&repo);
    app.running = true;
    app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(
        app.effects.iter().any(|e| matches!(e, Effect::Stop)),
        "Ctrl+S was swallowed by the open modal"
    );
}

/// One physical keypress, one approval: on Press+Release terminals the
/// trailing Release must not approve the NEXT prompt.
#[test]
fn perm_press_release_counts_once() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Press,
    ));
    assert!(matches!(
        rx.try_recv().unwrap(),
        sui::events::GateChoice::Once
    ));
    assert!(app.modal.is_none());

    // tool 2's prompt opens; the Release tail of the same keypress arrives
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    app.modal = Some(Modal::Permission {
        id: 2,
        agent: "solo".into(),
        summary: "write out/y".into(),
        reply: tx2,
    });
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(
        matches!(app.modal, Some(Modal::Permission { .. })),
        "a Release matching an earlier Press approved the next prompt"
    );
    assert!(rx2.try_recv().is_err());

    // a genuinely new keypress still approves
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Press,
    ));
    assert!(matches!(
        rx2.try_recv().unwrap(),
        sui::events::GateChoice::Once
    ));
}

/// Repeat events (held key) never decide a permission prompt.
#[test]
fn perm_repeat_never_approves() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Repeat,
    ));
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));
    assert!(rx.try_recv().is_err());
}

/// A Release with no prior Press still works (release-only transports),
/// including a release typed into chat before the modal opened.
#[test]
fn perm_release_without_press_is_compat_path() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    // 'y' was pressed while no modal existed (typed into chat), then
    // released over the open modal — the press is outstanding, so this
    // release is NOT a fresh decision
    app.key(KeyEvent::new_with_kind(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
        KeyEventKind::Release,
    ));
    assert!(
        rx.try_recv().is_ok(),
        "release-only transport must still decide"
    );
}

/// Live policy: [a] raises the shared flag (next tool unprompted), the
/// Settings toggle revokes it, and the next tool prompts again — same
/// agent, no respawn, conversation untouched. The mock holds each
/// follow-up tool call on a latch so the flag flip is deterministic.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_session_policy_live_revocation() {
    use std::sync::atomic::Ordering::Relaxed;
    let repo = fixture_repo();
    let g2 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let g3 = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let port = mock_gated(json!({}), [g2.clone(), g3.clone()]);
    let mut app = app_with_mock(&repo, port);

    app.input.set("WRITEME3");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Effect::SendTask { task, run, .. } = app.effects.pop().unwrap() else {
        panic!()
    };

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
    let solo = sui::tui::spawn_solo(
        prof,
        repo.clone(),
        jdir(),
        ev_tx,
        app.cancel.clone(),
        app.stop_flag.clone(),
        app.auto.clone(),
    );
    solo.send(run, task);

    // first protected call prompts (Ask is the default)
    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply_event(ev);
        n += 1;
    }
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));

    // [a] → session flag up → tool 1 runs; tool 2's call waits on the latch
    app.key(key('a'));
    assert!(app.auto.load(Relaxed));
    let t1 = repo.join("out/tui1.txt");
    let t0 = std::time::Instant::now();
    while !t1.exists() && t0.elapsed() < Duration::from_secs(15) {
        while let Ok(ev) = ev_rx.try_recv() {
            app.apply_event(ev);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(t1.exists());

    // Ask→Auto was live: release the latch — tool 2 must run unprompted
    g2.store(true, Relaxed);
    let t2 = repo.join("out/tui2.txt");
    let t0 = std::time::Instant::now();
    while !t2.exists() && t0.elapsed() < Duration::from_secs(15) {
        while let Ok(ev) = ev_rx.try_recv() {
            app.apply_event(ev);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(t2.exists(), "session flag should auto-allow tool 2");
    assert!(app.modal.is_none(), "tool 2 ran without a prompt");

    // Auto→Ask: revoke while the mock holds tool 3's call on its latch —
    // the live gate must see Ask before the next dispatch
    app.auto.store(false, Relaxed);
    g3.store(true, Relaxed);
    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 30 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
            .await
            .unwrap()
            .unwrap();
        app.apply_event(ev);
        n += 1;
    }
    assert!(
        matches!(app.modal, Some(Modal::Permission { .. })),
        "revoking Auto must re-prompt on the very next tool"
    );
    assert!(!app.auto.load(Relaxed), "badge reflects effective policy");

    app.key(key('n'));
    assert!(pump(&mut app, &mut ev_rx).await);
    assert!(
        !repo.join("out/tui3.txt").exists(),
        "denied tool must not run"
    );
}

/// Workspace change resets the session flag — Ask is the default there.
#[test]
fn workspace_change_resets_auto() {
    let repo = fixture_repo();
    let (mut app, _rx) = perm_app(&repo);
    app.modal = None;
    app.auto.store(true, std::sync::atomic::Ordering::Relaxed);
    app.modal = Some(Modal::Text {
        title: "workspace path".into(),
        buf: Buf::from("/tmp/other"),
        target: sui::tui::app::TextTarget::Workspace,
    });
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.auto.load(std::sync::atomic::Ordering::Relaxed));
    assert_eq!(app.ui.workspace.as_deref(), Some("/tmp/other"));
}

/// Regression: saving a provider form must persist the key to
/// session_keys so the settings row stops showing MISSING.
#[test]
fn provider_form_save_keeps_key() {
    use sui::tui::app::ProvType;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.modal = Some(Modal::Provider(ProvForm::new(ProvType::Custom)));
    // fields: Name, BaseUrl, Model, Auth, Test, Save, Cancel
    // move focus to Auth (3) and switch to ApiKey
    for _ in 0..3 {
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
    if let Some(Modal::Provider(f)) = &app.modal {
        assert_eq!(f.auth, sui::tui::app::AuthMode::ApiKey);
    } else {
        panic!();
    }
    // ApiKey mode adds ApiKey + Store rows: focus 4 = key field
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.paste("sk-test-123");
    // tab past Store+Test to Save (7)
    for _ in 0..3 {
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Some(Effect::SaveProfile {
        key,
        key_env,
        store,
        ..
    }) = app.effects.pop()
    else {
        panic!("no SaveProfile effect");
    };
    assert_eq!(
        key.as_deref(),
        Some("sk-test-123"),
        "pasted key must reach the save effect"
    );
    assert!(
        key_env.is_none(),
        "ApiKey mode must not persist an env name"
    );
    assert_eq!(store, sui::tui::app::Store::Keychain);
}

/// Config-file store writes api_key into the profile so it survives
/// restart — the only durable path on headless boxes with no keyring.
#[test]
fn provider_save_config_file_persists() {
    use sui::tui::app::{ProvType, Store};
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let mut f = ProvForm::new(ProvType::Custom);
    f.name.set("mine");
    f.base_url.set("http://127.0.0.1:9");
    f.store = Store::ConfigFile;
    // fields for Custom+ApiKey — set auth + key directly
    f.auth = sui::tui::app::AuthMode::ApiKey;
    f.key.set("sk-persist");
    app.modal = Some(Modal::Provider(f));
    // navigate straight to Save: fields are Name,BaseUrl,Model,Auth,ApiKey,Store,Test,Save,Cancel
    for _ in 0..7 {
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    }
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Some(Effect::SaveProfile {
        name,
        base_url,
        model,
        key_env,
        key,
        store,
    }) = app.effects.pop()
    else {
        panic!("no SaveProfile effect");
    };
    assert_eq!(store, Store::ConfigFile);
    assert_eq!(key.as_deref(), Some("sk-persist"));
    // run the same persistence path the event loop runs, against a fixture file
    let cfg = repo.join("saved-config.toml");
    let inline = if store == Store::ConfigFile {
        key.clone()
    } else {
        None
    };
    sui::config::save_profile_at(
        &cfg,
        &name,
        &base_url,
        &model,
        key_env.as_deref(),
        inline.as_deref(),
    )
    .unwrap();
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(
        text.contains("api_key = \"sk-persist\""),
        "config must carry the key: {text}"
    );
    // and a reload sees it (what the restarted TUI sees)
    let doc: toml::Value = text.parse().unwrap();
    let p = &doc["profiles"]["mine"];
    assert_eq!(p["api_key"].as_str(), Some("sk-persist"));
}

/// Mission mode must be reachable without Ctrl+M — that chord is byte
/// 0x0D == Enter in most terminals, so it can never fire there.
#[test]
fn mission_mode_alternate_paths() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    assert_eq!(app.mode, Mode::Solo);

    // Ctrl+O — portable chord
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.mode, Mode::Mission);
    assert!(
        app.effects.iter().any(|e| matches!(e, Effect::SaveUi)),
        "mode change must persist ui.mode"
    );
    app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert_eq!(app.mode, Mode::Solo);

    // kitty/CSI-u terminals do deliver real Ctrl+M — keep it working
    app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL));
    assert_eq!(app.mode, Mode::Mission);
    app.key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::CONTROL));
    assert_eq!(app.mode, Mode::Solo);

    // slash commands in the chat input
    app.input.insert_str("/mission");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.mode, Mode::Mission);
    assert!(app.input.is_empty());
    app.input.insert_str("/solo");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.mode, Mode::Solo);
    // unknown slash command must not dispatch a task
    app.input.insert_str("/nonsense");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.input.is_empty(), "unknown command keeps the input");
    assert!(app
        .effects
        .iter()
        .all(|e| !matches!(e, Effect::SendTask { .. })));

    // settings row: run mode toggles on Enter
    app.tab = Tab::Settings;
    let i = app
        .settings_rows()
        .iter()
        .position(|r| matches!(r, SettingsRow::Mode))
        .expect("Mode row exists");
    app.settings_sel = i;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.mode, Mode::Mission);
}

/// Settings → export row pushes the effect; /export does the same.
#[test]
fn export_run_triggers() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.tab = Tab::Settings;
    let i = app
        .settings_rows()
        .iter()
        .position(|r| matches!(r, SettingsRow::Export))
        .expect("Export row exists");
    app.settings_sel = i;
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(app.effects.iter().any(|e| matches!(e, Effect::ExportRun)));

    app.tab = Tab::Chat;
    app.input.insert_str("/export");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.effects
            .iter()
            .filter(|e| matches!(e, Effect::ExportRun))
            .count(),
        2
    );
}

/// Two concurrent permission asks must queue, not overwrite: the parked
/// request keeps its reply channel alive and surfaces next.
#[test]
fn permission_asks_queue_in_order() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let (tx1, mut rx1) = tokio::sync::mpsc::unbounded_channel();
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    app.apply_event(UiEvent::Permission {
        run: 1,
        id: 1,
        agent: "w-W1".into(),
        summary: "bash: rm a".into(),
        reply: tx1,
    });
    app.apply_event(UiEvent::Permission {
        run: 1,
        id: 2,
        agent: "w-W2".into(),
        summary: "bash: rm b".into(),
        reply: tx2,
    });
    match &app.modal {
        Some(Modal::Permission { agent, summary, .. }) => {
            assert_eq!(agent, "w-W1");
            assert_eq!(summary, "bash: rm a");
        }
        _ => panic!("first ask should be the modal"),
    }
    assert_eq!(app.pending_perms.len(), 1, "second ask parks, not lost");

    // 'y' decides the first; the second becomes the modal — nothing denied
    app.key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE));
    assert!(matches!(rx1.try_recv(), Ok(GateChoice::Once)));
    match &app.modal {
        Some(Modal::Permission { agent, .. }) => assert_eq!(agent, "w-W2"),
        _ => panic!("queued ask should surface after decision"),
    }
    assert!(rx2.try_recv().is_err(), "queued ask still undecided");
    app.key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
    assert!(matches!(rx2.try_recv(), Ok(GateChoice::Deny)));
    assert!(app.modal.is_none());
}

/// Enter during a run must not send or lose the draft — it warns instead.
#[test]
fn enter_while_running_warns_keeps_text() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.running = true;
    app.input.insert_str("next task");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "next task", "draft preserved");
    assert!(app.status.contains("in progress"));
    assert!(app.effects.is_empty(), "nothing sent");
}

/// Up with an empty input recalls the last submitted task; typing clears it.
#[test]
fn input_history_recall() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.history = vec!["first".into(), "second".into()];
    app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "second");
    app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "first");
    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "second");
    app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "");
    assert!(app.hist_i.is_none());
    // history present + empty input → Up recalls again rather than scroll
    app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.input.text(), "second");
    app.input.clear();
    app.hist_i = None;
    app.history.clear();
    // no history → Up scrolls the conversation instead
    app.scroll = 0;
    app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
    assert_eq!(app.scroll, 1);
}

/// Esc in Settings returns to Chat.
#[test]
fn esc_settings_back_to_chat() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.tab = Tab::Settings;
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(matches!(app.tab, Tab::Chat));
}

// ── adaptive activity transcript ────────────────────────────────────

use sui::events::ToolStatus;
use sui::tui::app::ReasonPref;

/// Transcript rows as plain text — the same projection the renderer uses.
fn tlines(app: &App, w: usize) -> Vec<String> {
    sui::tui::transcript::rows(app, w)
        .into_iter()
        .map(|r| {
            r.line
                .spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        })
        .collect()
}

fn send_task(app: &mut App, text: &str) -> u64 {
    app.input.set(text);
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Some(Effect::SendTask { run, .. }) = app.effects.pop() else {
        panic!("no SendTask effect")
    };
    run
}

fn req_cycle(app: &mut App, run: u64, agent: &str, req: u64, text: &str) {
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: agent.into(),
        req,
    });
    if !text.is_empty() {
        app.apply_event(UiEvent::Delta {
            run,
            agent: agent.into(),
            req,
            text: text.into(),
        });
    }
    app.apply_event(UiEvent::ReqDone {
        run,
        agent: agent.into(),
        req,
        ms: 5,
        ok: true,
        reasoning: false,
    });
}

fn tool_done(
    run: u64,
    agent: &str,
    call: &str,
    status: ToolStatus,
    exit: Option<i32>,
    result: &str,
) -> UiEvent {
    UiEvent::ToolDone {
        run,
        agent: agent.into(),
        call: call.into(),
        name: "bash".into(),
        ms: 12,
        status,
        exit,
        result: result.into(),
        truncated: false,
        dropped: 0,
    }
}

/// Live command output must be visible BEFORE the process exits, and a
/// finished ok call collapses to a single header row.
#[test]
fn transcript_live_output_then_collapse() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "do it");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "solo".into(),
        req: 0,
    });
    app.apply_event(UiEvent::ToolStart {
        run,
        agent: "solo".into(),
        req: 0,
        call: "c1".into(),
        name: "bash".into(),
        summary: "bash: seq 3".into(),
    });
    app.apply_event(UiEvent::ToolOut {
        run,
        agent: "solo".into(),
        call: "c1".into(),
        err: false,
        text: "1\n2\n".into(),
    });
    let rows = tlines(&app, 80);
    assert!(
        rows.iter()
            .any(|l| l.contains("bash") && l.contains("running")),
        "running state shown:\n{}",
        rows.join("\n")
    );
    assert!(
        rows.iter().any(|l| l.trim() == "2"),
        "live chunk visible before exit:\n{}",
        rows.join("\n")
    );

    app.apply_event(tool_done(
        run,
        "solo",
        "c1",
        ToolStatus::Ok,
        Some(0),
        "status: success\nexit_code: 0\nstdout: 1\n2\n3\nstderr: <empty>",
    ));
    let rows = tlines(&app, 80);
    assert!(
        rows.iter()
            .any(|l| l.contains("bash") && l.contains("exit 0")),
        "finished call = one header row:\n{}",
        rows.join("\n")
    );
    assert!(
        !rows.iter().any(|l| l.trim() == "2"),
        "multiline output collapses:\n{}",
        rows.join("\n")
    );
}

/// A failed step keeps its diagnostic excerpt — even after the run folds.
#[test]
fn transcript_failed_step_keeps_excerpt() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "do it");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "solo".into(),
        req: 0,
    });
    app.apply_event(UiEvent::ToolStart {
        run,
        agent: "solo".into(),
        req: 0,
        call: "c1".into(),
        name: "bash".into(),
        summary: "bash: explode".into(),
    });
    app.apply_event(tool_done(
        run,
        "solo",
        "c1",
        ToolStatus::Failed,
        Some(3),
        "status: failed\nexit_code: 3\nstdout: <empty>\nstderr: boom exploded badly",
    ));
    req_cycle(&mut app, run, "solo", 1, "the command failed; handled it");
    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });

    // run succeeded overall → folds, but the failure keeps its excerpt
    let rows = tlines(&app, 80);
    assert!(
        rows.iter().any(|l| l.contains("1 failed")),
        "summary counts the failure:\n{}",
        rows.join("\n")
    );
    assert!(
        rows.iter().any(|l| l.contains("boom exploded badly")),
        "failure excerpt survives folding:\n{}",
        rows.join("\n")
    );
    assert!(
        rows.iter()
            .any(|l| l.contains("the command failed; handled it")),
        "final answer stays visible:\n{}",
        rows.join("\n")
    );
}

/// Reasoning: live preview while streaming, collapsed header after, and
/// Hidden/Expanded preferences — all view-only.
#[test]
fn transcript_reasoning_streams_collapses() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "think hard");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "solo".into(),
        req: 0,
    });
    app.apply_event(UiEvent::Reason {
        run,
        agent: "solo".into(),
        req: 0,
        text: "thinking step A".into(),
    });
    let rows = tlines(&app, 80);
    assert!(
        rows.iter().any(|l| l.contains("reasoning")),
        "live reasoning block:\n{}",
        rows.join("\n")
    );
    assert!(
        rows.iter().any(|l| l.contains("thinking step A")),
        "streamed preview:\n{}",
        rows.join("\n")
    );

    // block ends → collapses to a header (group still running → open)
    app.apply_event(UiEvent::Delta {
        run,
        agent: "solo".into(),
        req: 0,
        text: "answer".into(),
    });
    app.apply_event(UiEvent::ReqDone {
        run,
        agent: "solo".into(),
        req: 0,
        ms: 9,
        ok: true,
        reasoning: true,
    });
    let rows = tlines(&app, 80);
    assert!(
        rows.iter().any(|l| l.contains("reasoned ·")),
        "collapsed reasoning header:\n{}",
        rows.join("\n")
    );
    assert!(
        !rows.iter().any(|l| l.contains("thinking step A")),
        "reasoning body collapsed:\n{}",
        rows.join("\n")
    );

    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });
    let rows = tlines(&app, 80);
    assert!(
        !rows
            .iter()
            .any(|l| l.contains("reasoned") || l.contains("thinking step A")),
        "folded run hides reasoning entirely:\n{}",
        rows.join("\n")
    );

    // Expanded pref shows it; Hidden removes even the header
    app.groups.last_mut().unwrap().expanded = true;
    app.reasoning = ReasonPref::Expanded;
    let rows = tlines(&app, 80);
    assert!(
        rows.iter().any(|l| l.contains("thinking step A")),
        "Expanded shows full reasoning:\n{}",
        rows.join("\n")
    );
    app.reasoning = ReasonPref::Hidden;
    let rows = tlines(&app, 80);
    assert!(
        !rows
            .iter()
            .any(|l| l.contains("reasoned") || l.contains("thinking step A")),
        "Hidden removes all reasoning rows:\n{}",
        rows.join("\n")
    );
}

/// A provider with no reasoning stream gets an honest activity row —
/// never a fabricated "thinking" block.
#[test]
fn transcript_no_reasoning_honest_wait() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "plain task");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "solo".into(),
        req: 0,
    });
    let rows = tlines(&app, 80);
    assert!(
        rows.iter()
            .any(|l| l.contains("working") && l.contains("solo")),
        "honest waiting indicator:\n{}",
        rows.join("\n")
    );
    assert!(
        !rows
            .iter()
            .any(|l| l.contains("reasoning") || l.contains("reasoned")),
        "no fake reasoning block:\n{}",
        rows.join("\n")
    );
}

/// Manual expansion overrides auto-fold until the user closes it.
#[test]
fn transcript_manual_expand_persists() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "t");
    req_cycle(&mut app, run, "solo", 0, "answer");
    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });
    let gi = app.groups.len() - 1;
    assert!(app.groups[gi].folded(), "successful run auto-collapses");

    // nav mode: Tab in, Enter on the folded group expands it
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.nav);
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(!app.groups[gi].folded(), "manual expand opens the group");
    // a later event must not refold what the user opened
    app.apply_event(UiEvent::Phase {
        run,
        agent: "m".into(),
        text: "later note".into(),
    });
    assert!(
        !app.groups[gi].folded(),
        "later events never refold a user-opened group"
    );
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert!(!app.nav, "Esc returns focus to the input");
}

/// Two workers streaming interleaved text into one run must land in two
/// separate messages — matched by (agent, req), never merged by position.
#[test]
fn transcript_workers_attributed_by_id() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.mode = Mode::Mission;
    let run = send_task(&mut app, "big task");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "w-W1".into(),
        req: 0,
    });
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "w-W2".into(),
        req: 0,
    });
    app.apply_event(UiEvent::Delta {
        run,
        agent: "w-W1".into(),
        req: 0,
        text: "alpha".into(),
    });
    app.apply_event(UiEvent::Delta {
        run,
        agent: "w-W2".into(),
        req: 0,
        text: "beta".into(),
    });
    app.apply_event(UiEvent::Delta {
        run,
        agent: "w-W1".into(),
        req: 0,
        text: "-more".into(),
    });
    let g = app.groups.last().unwrap();
    let texts: Vec<&str> = g
        .items
        .iter()
        .filter_map(|it| match it {
            Act::Assistant { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        texts,
        ["alpha-more", "beta"],
        "interleaved text must not merge"
    );
    let rows = tlines(&app, 80);
    assert!(
        rows.iter().any(|l| l.contains("w-W1")) && rows.iter().any(|l| l.contains("w-W2")),
        "both agents attributed:\n{}",
        rows.join("\n")
    );
}

/// Scroll anchors on a row, not a line offset: folding/appends keep the
/// same content at the top of the viewport.
#[test]
fn transcript_scroll_anchors_across_folds() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.view_w.set(80);
    app.view_h.set(5);
    // three runs; first two done+folded, third still running
    for t in ["t1", "t2"] {
        let run = send_task(&mut app, t);
        req_cycle(&mut app, run, "solo", 0, &format!("answer {t}"));
        app.apply_event(UiEvent::RunDone {
            run,
            outcome: "done".into(),
            accepted_sha: None,
        });
    }
    let run3 = send_task(&mut app, "t3");
    req_cycle(&mut app, run3, "solo", 0, "partial");

    // scroll up: anchor = the top visible row's owner
    app.scroll_by(2);
    assert!(app.scroll > 0);
    let rows = sui::tui::transcript::rows(&app, 80);
    let top = rows.len().saturating_sub(5).saturating_sub(app.scroll);
    let owner = rows[top].owner;

    // more activity arrives in the running group
    app.apply_event(UiEvent::Delta {
        run: run3,
        agent: "solo".into(),
        req: 0,
        text: "\nextra streamed content\nmore lines\nand more".into(),
    });
    app.apply_event(UiEvent::Phase {
        run: run3,
        agent: "m".into(),
        text: "phase note".into(),
    });

    let rows2 = sui::tui::transcript::rows(&app, 80);
    let top2 = rows2.len().saturating_sub(5).saturating_sub(app.scroll);
    assert_eq!(
        rows2[top2.min(rows2.len() - 1)].owner.0,
        owner.0,
        "anchored group must stay at the top of the viewport"
    );

    // End returns to live-follow
    app.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
    assert_eq!(app.scroll, 0);
}

/// Model/tool text can carry terminal escapes — they must be stripped
/// before they ever reach the framebuffer.
#[test]
fn transcript_sanitizes_control_seqs() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "t");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "solo".into(),
        req: 0,
    });
    app.apply_event(UiEvent::ToolStart {
        run,
        agent: "solo".into(),
        req: 0,
        call: "c1".into(),
        name: "bash".into(),
        summary: "bash: x".into(),
    });
    app.apply_event(UiEvent::ToolOut {
        run,
        agent: "solo".into(),
        call: "c1".into(),
        err: false,
        text: "\x1b[2J\x1b[Hrm -rf /\x07done".into(),
    });
    let rows = tlines(&app, 80);
    let joined = rows.join("\n");
    assert!(
        !joined.contains('\x1b') && !joined.contains('\x07'),
        "control sequences stripped:\n{joined:?}"
    );
    assert!(joined.contains("rm -rf /"), "content survives: {joined}");
}

/// Grapheme-aware wrap: Thai combining marks and wide emoji never split
/// mid-cluster; every row respects the display width.
#[test]
fn transcript_wrap_respects_graphemes() {
    use sui::tui::transcript::wrap;
    use unicode_width::UnicodeWidthStr;
    let thai = "ภาษาไทยที่รัก";
    let lines = wrap(thai, 4);
    for l in &lines {
        assert!(
            UnicodeWidthStr::width(l.as_str()) <= 4,
            "row too wide: {l:?}"
        );
    }
    assert_eq!(lines.concat(), thai, "no cluster lost or reordered");
    let em = "🦀🦀🦀";
    let lines = wrap(em, 4);
    assert_eq!(
        lines.len(),
        2,
        "wide chars wrap on width, not count: {lines:?}"
    );
    assert_eq!(lines.concat(), em);
}

/// The live preview tail is bounded — a flood of chunks never grows
/// memory unboundedly (capture caps are enforced at the source too).
#[test]
fn transcript_live_preview_bounded() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "t");
    app.apply_event(UiEvent::ToolStart {
        run,
        agent: "solo".into(),
        req: 0,
        call: "c1".into(),
        name: "bash".into(),
        summary: "bash: flood".into(),
    });
    let chunk = "x".repeat(4096);
    for _ in 0..64 {
        app.apply_event(UiEvent::ToolOut {
            run,
            agent: "solo".into(),
            call: "c1".into(),
            err: false,
            text: chunk.clone(),
        });
    }
    let g = app.groups.last().unwrap();
    let live = g
        .items
        .iter()
        .find_map(|it| match it {
            Act::Tool { live, .. } => Some(live.len()),
            _ => None,
        })
        .unwrap();
    assert!(live <= 12_000, "live tail bounded: {live}");
}

/// A pending permission is never hidden by folding or run completion.
#[test]
fn transcript_permission_never_hidden() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "t");
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    app.apply_event(UiEvent::Permission {
        run,
        id: 1,
        agent: "w-W1".into(),
        summary: "bash: late ask".into(),
        reply: tx,
    });
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));
    // run ends while the ask is open — the modal stays; nothing is
    // auto-decided or hidden
    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });
    assert!(
        matches!(app.modal, Some(Modal::Permission { .. })),
        "pending permission survived run end"
    );
}

/// Enter/Space expand only in nav mode; in the editor Enter still sends.
#[test]
fn transcript_nav_enter_semantics() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "t");
    req_cycle(&mut app, run, "solo", 0, "answer");
    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });

    // nav mode: Enter toggles the group, never sends
    app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    assert!(app.nav);
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.effects
            .iter()
            .all(|e| !matches!(e, Effect::SendTask { .. })),
        "Enter in nav mode must not send"
    );
    assert!(
        !app.groups.last().unwrap().folded(),
        "Enter expanded the group"
    );

    // back to editor: Enter sends again
    app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    app.input.set("next task");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(
        app.effects
            .iter()
            .any(|e| matches!(e, Effect::SendTask { .. })),
        "Enter sends when the editor has focus"
    );
}

/// Phase notes (repair reasons, attempt counts) land as visible rows.
#[test]
fn transcript_phase_notes_visible() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "t");
    app.apply_event(UiEvent::Phase {
        run,
        agent: "mission".into(),
        text: "repair attempt 1 for W1 — gate 'cargo test' failed".into(),
    });
    let rows = tlines(&app, 80);
    assert!(
        rows.iter()
            .any(|l| l.contains("repair attempt 1") && l.contains("cargo test")),
        "repair reason + attempt visible:\n{}",
        rows.join("\n")
    );
}

/// Display state must never alter what the model sees: identical task →
/// identical request bytes, even with expansion/reasoning toggles mid-run.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn transcript_display_never_changes_requests() {
    // recording mock: every request body lands in <dir>/req<N>.json
    fn mock_rec(rec: PathBuf) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::fs::create_dir_all(&rec).unwrap();
        std::thread::spawn(move || {
            let n = std::sync::atomic::AtomicUsize::new(0);
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
                    if line.trim().is_empty() {
                        break;
                    }
                    if line.trim().to_lowercase().starts_with("content-length:") {
                        len = line.trim()[15..].trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                if r.read_exact(&mut body).is_err() {
                    continue;
                }
                let i = n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                std::fs::write(rec.join(format!("req{i}.json")), &body).unwrap();
                let req: Value = serde_json::from_slice(&body).unwrap_or_default();
                let msgs = req["messages"].as_array().cloned().unwrap_or_default();
                let last = msgs.last().cloned().unwrap_or_default();
                let last_user = msgs
                    .iter()
                    .rev()
                    .find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str())
                    .unwrap_or("")
                    .to_string();
                let body = if last["role"] == "tool" {
                    sse_text("done")
                } else if last_user.contains("WRITEME") {
                    sse_tool_calls(json!([tc(
                        "w1",
                        "write_file",
                        &json!({"path": "out/rec.txt", "content": "x"}).to_string()
                    )]))
                } else {
                    sse_text("ack")
                };
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body);
                if s.write_all(resp.as_bytes()).is_err() {
                    continue;
                }
                let _ = s.flush();
            }
        });
        port
    }

    async fn one_run(repo: &PathBuf, rec: PathBuf, fiddle: bool) {
        let port = mock_rec(rec);
        let mut app = app_with_mock(repo, port);
        let run = send_task(&mut app, "WRITEME");
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
        let solo = sui::tui::spawn_solo(
            prof,
            repo.clone(),
            jdir(),
            ev_tx,
            app.cancel.clone(),
            app.stop_flag.clone(),
            app.auto.clone(),
        );
        solo.send(run, "WRITEME".into());
        // pump until the permission modal; mid-run display churn on the
        // fiddle arm — expand, cycle reasoning pref, enter nav
        let mut n = 0;
        while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 30 {
            let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv())
                .await
                .unwrap()
                .unwrap();
            app.apply_event(ev);
            n += 1;
            if fiddle && n == 2 {
                app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)); // nav mode
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)); // expand
                app.key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)); // reasoning pref
                app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
        }
        app.key(key('y'));
        assert!(pump(&mut app, &mut ev_rx).await, "run never finished");
    }

    let repo = fixture_repo();
    let tag = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let rec_a = std::env::temp_dir().join(format!("sui-req-a-{tag}"));
    let rec_b = std::env::temp_dir().join(format!("sui-req-b-{tag}"));
    one_run(&repo, rec_a.clone(), false).await;
    one_run(&repo, rec_b.clone(), true).await;
    for i in 0..2 {
        let a = std::fs::read(rec_a.join(format!("req{i}.json")))
            .unwrap_or_else(|_| panic!("req{i} missing in run A"));
        let b = std::fs::read(rec_b.join(format!("req{i}.json")))
            .unwrap_or_else(|_| panic!("req{i} missing in run B"));
        assert_eq!(
            a, b,
            "display state changed request {i} — view must never touch model history"
        );
    }
}

/// Renderer snapshot over a real TestBackend — exercises the full draw()
/// path (layout, borders, sidebar, transcript) not just the projection.
#[test]
fn snapshot_live_then_folded() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "ship it");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "solo".into(),
        req: 0,
    });
    app.apply_event(UiEvent::ToolStart {
        run,
        agent: "solo".into(),
        req: 0,
        call: "c1".into(),
        name: "bash".into(),
        summary: "bash: build".into(),
    });
    app.apply_event(UiEvent::ToolOut {
        run,
        agent: "solo".into(),
        call: "c1".into(),
        err: false,
        text: "compiling crate A\n".into(),
    });
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let s = format!("{}", t.backend());
    assert!(s.contains("compiling crate A"), "live output painted:\n{s}");
    assert!(s.contains("bash: build"));

    // done + folded: summary row, final answer, no activity residue
    app.apply_event(tool_done(
        run,
        "solo",
        "c1",
        ToolStatus::Ok,
        Some(0),
        "status: success\nexit_code: 0\nstdout: ok\nstderr: <empty>",
    ));
    req_cycle(&mut app, run, "solo", 1, "shipped");
    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let s = format!("{}", t.backend());
    assert!(
        s.contains("Enter/Space expands"),
        "folded summary row:\n{s}"
    );
    assert!(s.contains("shipped"), "final answer stays visible:\n{s}");
    assert!(
        !s.contains("compiling crate A"),
        "finished output folded away:\n{s}"
    );
}

/// Snapshot: a failed step paints its excerpt inside the folded group.
#[test]
fn snapshot_failure_in_folded_group() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "try it");
    app.apply_event(UiEvent::ReqStart {
        run,
        agent: "solo".into(),
        req: 0,
    });
    app.apply_event(UiEvent::ToolStart {
        run,
        agent: "solo".into(),
        req: 0,
        call: "c1".into(),
        name: "bash".into(),
        summary: "bash: explode".into(),
    });
    app.apply_event(tool_done(
        run,
        "solo",
        "c1",
        ToolStatus::Failed,
        Some(3),
        "status: failed\nexit_code: 3\nstdout: <empty>\nstderr: exit status 3: kaboom",
    ));
    req_cycle(&mut app, run, "solo", 1, "it failed; recovered");
    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let s = format!("{}", t.backend());
    assert!(s.contains("1 failed"), "summary counts failure:\n{s}");
    assert!(s.contains("kaboom"), "diagnostic excerpt painted:\n{s}");
    assert!(s.contains("it failed; recovered"), "final answer:\n{s}");
}

/// Identical consecutive successful calls collapse to a ×N row — the
/// count is always visible, retries are never hidden.
#[test]
fn transcript_groups_repeated_calls() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "poll it");
    for i in 0..4 {
        let call = format!("c{i}");
        app.apply_event(UiEvent::ToolStart {
            run,
            agent: "solo".into(),
            req: 0,
            call: call.clone(),
            name: "read_file".into(),
            summary: "read src/lib.rs".into(),
        });
        app.apply_event(UiEvent::ToolDone {
            run,
            agent: "solo".into(),
            call,
            name: "read_file".into(),
            ms: 3,
            status: ToolStatus::Ok,
            exit: None,
            result: "status: success\n…".into(),
            truncated: false,
            dropped: 0,
        });
    }
    let rows = tlines(&app, 80);
    let grouped: Vec<&String> = rows.iter().filter(|l| l.contains("×")).collect();
    assert_eq!(grouped.len(), 1, "one grouped row:\n{}", rows.join("\n"));
    assert!(
        grouped[0].contains("×4"),
        "call count visible: {}",
        grouped[0]
    );
    assert!(
        grouped[0].contains("read_file"),
        "tool name kept: {}",
        grouped[0]
    );
    // different names don't group
    app.apply_event(UiEvent::ToolStart {
        run,
        agent: "solo".into(),
        req: 0,
        call: "c9".into(),
        name: "bash".into(),
        summary: "bash: ls".into(),
    });
    app.apply_event(tool_done(
        run,
        "solo",
        "c9",
        ToolStatus::Ok,
        Some(0),
        "status: success",
    ));
    let rows = tlines(&app, 80);
    assert!(
        rows.iter().any(|l| l.contains("bash") && !l.contains("×")),
        "distinct tool renders separately:\n{}",
        rows.join("\n")
    );
}

// ── mouse ────────────────────────────────────────────────────────────
// Headless: app.mouse() is pure state — a TestBackend draw populates
// the hitmap/geometry first, then events are dispatched by kind+cell.

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use sui::tui::app::{Effect as Fx, Hit};

fn mev(kind: MouseEventKind, col: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: col,
        row,
        modifiers: KeyModifiers::NONE,
    }
}
fn click(app: &mut App, col: u16, row: u16) {
    app.mouse(mev(MouseEventKind::Down(MouseButton::Left), col, row));
    app.mouse(mev(MouseEventKind::Up(MouseButton::Left), col, row));
}
fn zone(app: &App, pred: impl Fn(&Hit) -> bool) -> (u16, u16) {
    let z = app
        .hits
        .borrow()
        .iter()
        .find(|z| pred(&z.hit))
        .expect("hit zone present")
        .clone();
    (z.x + z.w / 2, z.y)
}

/// Wheel over the transcript scrolls up; wheel down returns to live.
#[test]
fn mouse_wheel_scrolls_transcript() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    send_task(&mut app, "x");
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let g = app.chat_geom.get();
    let (cx, cy) = (g.x + 2, g.y + 2);
    app.mouse(mev(MouseEventKind::ScrollUp, cx, cy));
    assert_eq!(app.scroll, 3, "wheel up scrolls the transcript");
    app.mouse(mev(MouseEventKind::ScrollUp, cx, cy));
    assert_eq!(app.scroll, 6);
    app.mouse(mev(MouseEventKind::ScrollDown, cx, cy));
    app.mouse(mev(MouseEventKind::ScrollDown, cx, cy));
    assert_eq!(app.scroll, 0, "wheel down returns to live");
}

/// Clicking a folded group's row expands it — same as Enter in nav mode.
#[test]
fn mouse_click_folded_group_expands() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let run = send_task(&mut app, "ship it");
    req_cycle(&mut app, run, "solo", 0, "done work");
    app.apply_event(UiEvent::RunDone {
        run,
        outcome: "done".into(),
        accepted_sha: None,
    });
    assert!(app.groups[1].folded(), "done group starts folded");
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let (cx, cy) = zone(&app, |h| matches!(h, Hit::Activity(g, None) if *g == run));
    click(&mut app, cx, cy);
    assert!(app.groups[1].expanded, "click expanded the folded group");
    assert!(app.nav, "click focused transcript nav");
    click(&mut app, cx, cy);
    assert!(app.groups[1].collapsed, "second click folds it again");
}

/// Permission buttons are clickable: [y] once, [a] session, [n] deny —
/// identical decisions to the keyboard path.
#[test]
fn mouse_perm_buttons_decide() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    app.apply_event(UiEvent::Permission {
        run: 1,
        id: 9,
        agent: "w1".into(),
        summary: "bash: rm -rf build".into(),
        reply: tx,
    });
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let (cx, cy) = zone(&app, |h| {
        matches!(h, Hit::Perm(sui::events::GateChoice::Once))
    });
    click(&mut app, cx, cy);
    assert_eq!(
        rx.try_recv().unwrap(),
        sui::events::GateChoice::Once,
        "click approved once"
    );
    assert!(app.modal.is_none(), "modal consumed by the decision");

    // session button raises the live auto flag like 'a' does
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    app.apply_event(UiEvent::Permission {
        run: 1,
        id: 10,
        agent: "w2".into(),
        summary: "write_file: a.txt".into(),
        reply: tx2,
    });
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let (cx, cy) = zone(&app, |h| {
        matches!(h, Hit::Perm(sui::events::GateChoice::Session))
    });
    click(&mut app, cx, cy);
    assert_eq!(rx2.try_recv().unwrap(), sui::events::GateChoice::Session);
    assert!(
        app.auto.load(std::sync::atomic::Ordering::Relaxed),
        "session approve sets auto like 'a'"
    );
}

/// Drag across transcript rows selects; release copies via Effect::Clip
/// (OSC52 in mod.rs). The captured text is the rendered row text.
#[test]
fn mouse_drag_selects_and_copies() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let g = app.chat_geom.get();
    // drag from row 0 col 2 to row 1 col 20 — a real two-row selection
    app.mouse(mev(MouseEventKind::Down(MouseButton::Left), g.x + 2, g.y));
    app.mouse(mev(
        MouseEventKind::Drag(MouseButton::Left),
        g.x + 20,
        g.y + 1,
    ));
    assert!(app.sel.is_some(), "drag created a selection");
    app.mouse(mev(
        MouseEventKind::Up(MouseButton::Left),
        g.x + 20,
        g.y + 1,
    ));
    let clip = app
        .effects
        .iter()
        .find_map(|e| match e {
            Fx::Clip(s) => Some(s.clone()),
            _ => None,
        })
        .expect("release emitted a clip effect");
    assert!(clip.contains("welcome"), "selection text: {clip:?}");
    assert!(app.status.contains("copied"), "status reports the copy");

    // a fresh press clears the highlight; Esc clears it too
    app.mouse(mev(MouseEventKind::Down(MouseButton::Left), g.x + 4, g.y));
    assert!(app.sel.is_none());
}

/// Click on a transcript row does NOT select text — it expands.
#[test]
fn mouse_click_without_drag_is_not_a_copy() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let g = app.chat_geom.get();
    click(&mut app, g.x + 5, g.y);
    assert!(
        app.effects.iter().all(|e| !matches!(e, Fx::Clip(_))),
        "plain click must not emit a clip"
    );
}

/// Click outside the Help modal dismisses it; inside keeps it open…
/// and the details-view modal scrolls with the wheel.
#[test]
fn mouse_modal_dismiss_and_view_scroll() {
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.modal = Some(sui::tui::app::Modal::Help);
    let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
    t.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    click(&mut app, 2, 2); // corner — outside the centered modal
    assert!(app.modal.is_none(), "outside click dismissed help");

    // View modal: wheel scrolls its content
    let long = (0..60)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.modal = Some(sui::tui::app::Modal::View {
        title: "t".into(),
        text: long,
        scroll: 0,
    });
    app.mouse(mev(MouseEventKind::ScrollDown, 40, 12));
    match &app.modal {
        Some(sui::tui::app::Modal::View { scroll, .. }) => assert_eq!(*scroll, 3),
        _ => panic!("view modal stayed open and scrolled"),
    }
}

/// Mouse off = events ignored entirely (terminal keeps native select).
#[test]
fn mouse_toggle_off_ignores_events() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    app.mouse = false;
    app.mouse(mev(MouseEventKind::ScrollUp, 10, 10));
    assert_eq!(app.scroll, 0, "no scroll when mouse is off");
}

/// Settings → mouse row toggles the flag and emits Mouse + SaveUi.
#[test]
fn mouse_settings_row_toggles() {
    let repo = fixture_repo();
    let mut app = app_with_mock(&repo, 1);
    let i = app
        .settings_rows()
        .iter()
        .position(|r| matches!(r, sui::tui::app::SettingsRow::Mouse))
        .expect("mouse row exists");
    assert!(app.mouse);
    app.settings_activate(i);
    assert!(!app.mouse);
    assert_eq!(app.ui.mouse, Some(false), "toggle persisted to ui settings");
    assert!(
        app.effects.iter().any(|e| matches!(e, Fx::Mouse(false))),
        "terminal capture disabled live"
    );
}
