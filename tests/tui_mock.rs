//! TUI integration: drive App state through real agent/mission core
//! against an in-process SSE mock. No terminal required — App is pure
//! state; the run loop is exercised via its public seams.

use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use sui::config::{Profile, ProfileCfg, UiSettings};
use sui::events::UiEvent;
use sui::mission::{self, MissionCfg};
use sui::tui::app::{App, AuthMode, ChatItem, Effect, Field, Modal, Mode, ProvForm, ProvType, Role, Tab};

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
fn mock(plan: Value) -> u16 {
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

            let body = if system.contains("control plane") {
                if last_user.contains("auditor") {
                    let sub = json!({"payload": {"verdict": "PASS", "findings": [],
                        "required_fixes": []}});
                    sse_tool_calls(json!([tc("s1", "submit_result", &sub.to_string())]))
                } else {
                    let sub = json!({"payload": plan});
                    sse_tool_calls(json!([tc("s1", "submit_result", &sub.to_string())]))
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
    let solo = sui::tui::spawn_solo(prof, repo.clone(), jdir(), ev_tx, app.cancel.clone(), app.stop_flag.clone());
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
    let solo = sui::tui::spawn_solo(prof, repo.clone(), jdir(), ev_tx, app.cancel.clone(), app.stop_flag.clone());
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
