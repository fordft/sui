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
use sui::tui::app::{App, AuthMode, ChatItem, Effect, Field, Modal, Mode, ProvForm, ProvType, Role, SettingsRow, Tab};
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
                    sse_tool_calls(json!([tc("w", "write_file",
                        &json!({"path": format!("out/tui{}.txt", n_tools + 1),
                                "content": "written"}).to_string())]))
                }
            } else if last["role"] == "tool" {
                sse_text("done")
            } else if last_user.contains("WRITEME") {
                sse_tool_calls(json!([tc("w1", "write_file",
                    &json!({"path": "out/tui.txt", "content": "written by worker"}).to_string())]))
            } else if last_user.contains("LONGTASK") {
                sse_tool_calls(json!([tc("b1", "bash",
                    &json!({"command": "sleep 30"}).to_string())]))
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
            .arg("-C").arg(&dir).args(a).output().unwrap();
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
            .arg("-C").arg(repo).args(["rev-parse", "HEAD"]).output().unwrap().stdout,
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
    let task = match eff {
        Effect::SendTask { task, mode } => {
            assert_eq!(mode, Mode::Solo);
            task
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
    solo.send(task);

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
    // assistant text + tool record landed in chat
    assert!(app.chat.iter().any(|c| matches!(c, ChatItem::Tool { done: true, .. })));
    assert!(app.usage.values().any(|(_, u)| u.requests > 0));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_solo_permission_denied() {
    let repo = fixture_repo();
    let port = mock(json!({}));
    let mut app = app_with_mock(&repo, port);

    app.input.set("WRITEME");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Effect::SendTask { task, .. } = app.effects.pop().unwrap() else { panic!() };

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
    let solo = sui::tui::spawn_solo(prof, repo.clone(), jdir(), ev_tx, app.cancel.clone(), app.stop_flag.clone(), app.auto.clone());
    solo.send(task);

    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv()).await.unwrap().unwrap();
        app.apply_event(ev);
        n += 1;
    }
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));
    app.key(key('n')); // deny

    assert!(pump(&mut app, &mut ev_rx).await);
    assert!(!repo.join("out/tui.txt").exists(), "denied: file not written");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tui_stop_cancels_running_tool() {
    let repo = fixture_repo();
    let port = mock(json!({}));
    let mut app = app_with_mock(&repo, port);

    app.input.set("LONGTASK");
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Effect::SendTask { task, .. } = app.effects.pop().unwrap() else { panic!() };

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
    let solo = sui::tui::spawn_solo(prof, repo.clone(), jdir(), ev_tx, app.cancel.clone(), app.stop_flag.clone(), app.auto.clone());
    solo.send(task);

    // wait for the permission modal (bash needs approval), approve, then stop
    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv()).await.unwrap().unwrap();
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
                let mut s = match conn { Ok(s) => s, Err(_) => continue };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0usize;
                loop {
                    let mut line = String::new();
                    if r.read_line(&mut line).unwrap_or(0) == 0 { break; }
                    if line.trim().is_empty() { break; }
                    if line.trim().to_lowercase().starts_with("content-length:") {
                        len = line.trim()[15..].trim().parse().unwrap_or(0);
                    }
                }
                let mut body = vec![0u8; len];
                let _ = r.read_exact(&mut body);
                let req: Value = serde_json::from_slice(&body).unwrap_or_default();
                let msgs = req["messages"].as_array().cloned().unwrap_or_default();
                let system = msgs.iter().find(|m| m["role"] == "system")
                    .and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
                let last = msgs.last().cloned().unwrap_or_default();
                let last_user = msgs.iter().rev().find(|m| m["role"] == "user")
                    .and_then(|m| m["content"].as_str()).unwrap_or("").to_string();
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
                    sse_tool_calls(json!([tc("w1", "write_file",
                        &json!({"path": "out/m.txt", "content": "ok"}).to_string())]))
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
    let Effect::SendTask { task, mode } = app.effects.pop().unwrap() else { panic!() };
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
    assert_eq!(app.profiles["mock-ctrl"].model.as_deref(), Some("manual-model-x"));

    // worker concurrency toggle
    let rows = app.settings_rows();
    let w_idx = rows.iter().position(|r| matches!(r, sui::tui::app::SettingsRow::Workers)).unwrap();
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
    assert!(cf.fields().iter().all(|x| !matches!(x, Field::ApiKey | Field::Store | Field::CredSrc | Field::KeyEnv)));
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
    cf.focus = cf.fields().iter().position(|x| *x == Field::BaseUrl).unwrap();
    let mut app2 = App::with_state(repo.clone(), BTreeMap::new(), UiSettings::default());
    app2.modal = Some(Modal::Provider(cf));
    app2.paste("https://api.example.test/v1");
    if let Some(Modal::Provider(f)) = &app2.modal {
        assert_eq!(f.base_url.text(), "https://api.example.test/v1");
    }
    assert!(app2.input.text().is_empty());
}

// ── permission modal key semantics ────────────────────────────────────

fn perm_app(repo: &PathBuf) -> (App, tokio::sync::mpsc::UnboundedReceiver<sui::events::GateChoice>) {
    let mut app = app_with_mock(repo, 1); // port unused — modal injected directly
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    app.modal = Some(Modal::Permission { id: 1, agent: "solo".into(), summary: "write out/x".into(), reply: tx });
    (app, rx)
}

#[test]
fn perm_uppercase_variants_decide() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('Y'));
    assert!(matches!(rx.try_recv().unwrap(), sui::events::GateChoice::Once));

    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('A'));
    assert!(matches!(rx.try_recv().unwrap(), sui::events::GateChoice::Session));
    assert!(app.auto.load(std::sync::atomic::Ordering::Relaxed), "[a] must raise the Auto badge");

    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('N'));
    assert!(matches!(rx.try_recv().unwrap(), sui::events::GateChoice::Deny));
}

#[test]
fn perm_enter_does_not_approve() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(app.modal, Some(Modal::Permission { .. })),
        "bare Enter must not grant approval — no selected action exists");
    assert!(rx.try_recv().is_err());
}

#[test]
fn perm_release_kind_fires_shortcut() {
    // Transports that only report Release-kind char events must still
    // drive the modal; the gate is parked on this reply either way.
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Release));
    assert!(matches!(rx.try_recv().unwrap(), sui::events::GateChoice::Once));
}

#[test]
fn perm_release_of_unrelated_key_ignored() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(KeyCode::Char('x'), KeyModifiers::NONE, KeyEventKind::Release));
    assert!(matches!(app.modal, Some(Modal::Permission { .. })));
    assert!(rx.try_recv().is_err());
}

#[test]
fn perm_keys_never_leak_to_chat_input() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(key('y'));
    assert!(rx.try_recv().is_ok());
    assert!(app.input.text().is_empty(), "decision key leaked into chat input");
    // the trailing Release on press+release terminals must not type either
    app.key(KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Release));
    assert!(app.input.text().is_empty(), "release event typed into chat input");
}

#[test]
fn ctrl_s_stops_with_permission_modal_open() {
    let repo = fixture_repo();
    let (mut app, _rx) = perm_app(&repo);
    app.running = true;
    app.key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
    assert!(app.effects.iter().any(|e| matches!(e, Effect::Stop)),
        "Ctrl+S was swallowed by the open modal");
}

/// One physical keypress, one approval: on Press+Release terminals the
/// trailing Release must not approve the NEXT prompt.
#[test]
fn perm_press_release_counts_once() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Press));
    assert!(matches!(rx.try_recv().unwrap(), sui::events::GateChoice::Once));
    assert!(app.modal.is_none());

    // tool 2's prompt opens; the Release tail of the same keypress arrives
    let (tx2, mut rx2) = tokio::sync::mpsc::unbounded_channel();
    app.modal = Some(Modal::Permission { id: 2, agent: "solo".into(), summary: "write out/y".into(), reply: tx2 });
    app.key(KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Release));
    assert!(matches!(app.modal, Some(Modal::Permission { .. })),
        "a Release matching an earlier Press approved the next prompt");
    assert!(rx2.try_recv().is_err());

    // a genuinely new keypress still approves
    app.key(KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Press));
    assert!(matches!(rx2.try_recv().unwrap(), sui::events::GateChoice::Once));
}

/// Repeat events (held key) never decide a permission prompt.
#[test]
fn perm_repeat_never_approves() {
    let repo = fixture_repo();
    let (mut app, mut rx) = perm_app(&repo);
    app.key(KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Repeat));
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
    app.key(KeyEvent::new_with_kind(KeyCode::Char('y'), KeyModifiers::NONE, KeyEventKind::Release));
    assert!(rx.try_recv().is_ok(), "release-only transport must still decide");
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
    let Effect::SendTask { task, .. } = app.effects.pop().unwrap() else { panic!() };

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let prof = sui::tui::resolve_to_profile(&app, "mock-worker").unwrap();
    let solo = sui::tui::spawn_solo(
        prof, repo.clone(), jdir(), ev_tx,
        app.cancel.clone(), app.stop_flag.clone(), app.auto.clone(),
    );
    solo.send(task);

    // first protected call prompts (Ask is the default)
    let mut n = 0;
    while !matches!(app.modal, Some(Modal::Permission { .. })) && n < 20 {
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv()).await.unwrap().unwrap();
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
        while let Ok(ev) = ev_rx.try_recv() { app.apply_event(ev); }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(t1.exists());

    // Ask→Auto was live: release the latch — tool 2 must run unprompted
    g2.store(true, Relaxed);
    let t2 = repo.join("out/tui2.txt");
    let t0 = std::time::Instant::now();
    while !t2.exists() && t0.elapsed() < Duration::from_secs(15) {
        while let Ok(ev) = ev_rx.try_recv() { app.apply_event(ev); }
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
        let ev = tokio::time::timeout(Duration::from_secs(10), ev_rx.recv()).await.unwrap().unwrap();
        app.apply_event(ev);
        n += 1;
    }
    assert!(matches!(app.modal, Some(Modal::Permission { .. })),
        "revoking Auto must re-prompt on the very next tool");
    assert!(!app.auto.load(Relaxed), "badge reflects effective policy");

    app.key(key('n'));
    assert!(pump(&mut app, &mut ev_rx).await);
    assert!(!repo.join("out/tui3.txt").exists(), "denied tool must not run");
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
    let Some(Effect::SaveProfile { key, key_env, store, .. }) = app.effects.pop() else {
        panic!("no SaveProfile effect");
    };
    assert_eq!(key.as_deref(), Some("sk-test-123"), "pasted key must reach the save effect");
    assert!(key_env.is_none(), "ApiKey mode must not persist an env name");
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
    let Some(Effect::SaveProfile { name, base_url, model, key_env, key, store }) =
        app.effects.pop()
    else {
        panic!("no SaveProfile effect");
    };
    assert_eq!(store, Store::ConfigFile);
    assert_eq!(key.as_deref(), Some("sk-persist"));
    // run the same persistence path the event loop runs, against a fixture file
    let cfg = repo.join("saved-config.toml");
    let inline = if store == Store::ConfigFile { key.clone() } else { None };
    sui::config::save_profile_at(&cfg, &name, &base_url, &model, key_env.as_deref(), inline.as_deref())
        .unwrap();
    let text = std::fs::read_to_string(&cfg).unwrap();
    assert!(text.contains("api_key = \"sk-persist\""), "config must carry the key: {text}");
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
    assert!(app.effects.iter().any(|e| matches!(e, Effect::SaveUi)),
        "mode change must persist ui.mode");
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
    assert!(app.effects.iter().all(|e| !matches!(e, Effect::SendTask { .. })));

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
        app.effects.iter().filter(|e| matches!(e, Effect::ExportRun)).count(),
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
        id: 1, agent: "w-W1".into(), summary: "bash: rm a".into(), reply: tx1,
    });
    app.apply_event(UiEvent::Permission {
        id: 2, agent: "w-W2".into(), summary: "bash: rm b".into(), reply: tx2,
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
