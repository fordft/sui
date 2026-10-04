mod common;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{atomic::AtomicBool, Arc, Mutex};
use std::time::Duration;
use sui::config::{Profile, ProfileCfg, UiSettings};
use sui::events::{RequestDetails, UiEvent};
use sui::journal::Journal;
use sui::provider::Provider;
use sui::session::{Header, SavedSession, SessionLock, Signature};
use sui::tui::app::{App, Effect, Modal, PickTarget, Tab};
use sui::tui::commands::Command;

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("sui-resume-{:032x}", rand::random::<u128>()));
    std::fs::create_dir_all(&d).unwrap();
    d
}
fn profile(port: u16) -> Profile {
    Profile {
        name: "mock".into(),
        transport: Default::default(),
        image_input: false,
        base_url: format!("http://127.0.0.1:{port}/v1"),
        model: "mock".into(),
        api_key: None,
        prompt_cache_key: None,
        pricing: None,
    }
}
fn fixture(root: &Path, workspace: &Path, name: &str, opaque: bool) -> PathBuf {
    let d = root.join(name);
    let mut j = Journal::open_named(&d, "solo").unwrap();
    j.log("session", json!({"mode":"solo", "workspace": workspace}));
    j.log(
        "resume_header",
        serde_json::to_value(Header {
            format: 1,
            workspace: workspace.into(),
            profile: Some("mock".into()),
            model: "mock".into(),
            session_id: "stable-session".into(),
            agent_id: "solo".into(),
            signature: Signature::current(&Provider::from_profile(&profile(1)), workspace),
        })
        .unwrap(),
    );
    j.log("turn_start", json!({"run":1}));
    j.log("user", json!({"content":"original task"}));
    let items = if opaque {
        vec![json!({"type":"reasoning","encrypted_content":"private-opaque"})]
    } else {
        vec![]
    };
    let reference = j.store_response_items(&items).unwrap();
    j.log("assistant", json!({"content":"recorded answer", "tool_calls":[], "response_items_count":items.len(), "response_items_ref":reference}));
    j.log("turn_end", json!({"run":1,"outcome":"returned"}));
    d
}

#[test]
fn private_replay_forks_without_modifying_the_original() {
    let root = dir();
    let ws = dir();
    let source = fixture(&root, &ws, "source", true);
    let original = std::fs::read(source.join("solo.jsonl")).unwrap();
    let saved = SavedSession::load(&root, "source", &ws).unwrap();
    let child = root.join("child");
    saved.fork(&child, "solo").unwrap();
    assert_eq!(std::fs::read(source.join("solo.jsonl")).unwrap(), original);
    assert_eq!(std::fs::read(child.join("solo.jsonl")).unwrap(), original);
    let restored = sui::journal::replay_history(&child.join("solo.jsonl"), usize::MAX).unwrap();
    assert_eq!(
        sui::context::request_fingerprint(&sui::context::Compiled::view(&saved.history)),
        sui::context::request_fingerprint(&sui::context::Compiled::view(&restored))
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in std::fs::read_dir(&child).unwrap() {
            assert_eq!(
                entry.unwrap().metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    drop(saved);
    let sidecar = std::fs::read_dir(&source)
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("replay-")
        })
        .unwrap();
    std::fs::write(sidecar, b"tampered").unwrap();
    assert!(SavedSession::load(&root, "source", &ws)
        .err()
        .unwrap()
        .to_string()
        .contains("integrity"));
}

#[test]
fn recovery_refuses_active_incomplete_foreign_and_older_sessions() {
    let root = dir();
    let ws = dir();
    let source = fixture(&root, &ws, "source", false);
    let lock = SessionLock::acquire(&source).unwrap();
    assert!(SavedSession::load(&root, "source", &ws)
        .err()
        .unwrap()
        .to_string()
        .contains("still open"));
    drop(lock);
    assert!(SavedSession::load(&root, "source", &dir()).is_err());
    assert!(SavedSession::load(&root, "../source", &ws).is_err());
    let mut j = Journal::open_named(&source, "solo").unwrap();
    j.log("turn_start", json!({"run":2}));
    j.log("user", json!({"content":"unfinished"}));
    assert!(SavedSession::load(&root, "source", &ws)
        .err()
        .unwrap()
        .to_string()
        .contains("unfinished"));
    let old = root.join("older");
    let mut j = Journal::open_named(&old, "solo").unwrap();
    j.log("session", json!({"mode":"solo","workspace":ws}));
    j.log("user", json!({"content":"old"}));
    assert!(SavedSession::load(&root, "older", &ws)
        .err()
        .unwrap()
        .to_string()
        .contains("older sessions"));
    let recent = sui::session::recent(&root, &ws, None).unwrap();
    assert!(recent.iter().any(|s| s.label.contains("older journal")));
    assert!(sui::session::recent(&root, &dir(), None)
        .unwrap()
        .is_empty());
    let bad = fixture(&root, &ws, "unmatched", false);
    let mut j = Journal::open_named(&bad, "solo").unwrap();
    j.log("turn_start", json!({"run":2}));
    j.log(
        "assistant",
        json!({"content":"","tool_calls":[common::tc("t","write_file","{}")]}),
    );
    j.log("turn_end", json!({"run":2}));
    assert!(SavedSession::load(&root, "unmatched", &ws)
        .err()
        .unwrap()
        .to_string()
        .contains("tool call"));
}

fn details(n: u64, epoch: &str) -> RequestDetails {
    RequestDetails {
        request_id: n,
        session_id: "s".into(),
        epoch_id: epoch.into(),
        requested_model: "m".into(),
        static_prefix_hash: "static".into(),
        tool_schema_hash: "tools".into(),
        guidance_hash: None,
        cache_key_fingerprint: None,
        purpose: "agent".into(),
        first_delta_ms: Some(10),
    }
}
#[test]
fn cache_inspector_uses_paired_weighted_usage_and_exposes_epoch_changes() {
    let mut app = App::with_state(dir(), Default::default(), UiSettings::default());
    app.modal = None;
    for (n, input, cached, complete, epoch) in [
        (0, Some(100), Some(0), true, "E0"),
        (1, Some(10000), Some(9900), true, "E0"),
        (2, Some(7), None, true, "E0"),
        (3, Some(1), Some(2), true, "E0"),
        (4, Some(100), Some(100), false, "E0"),
        (5, Some(100), Some(0), true, "E1"),
    ] {
        app.apply_event(UiEvent::Usage {
            run: 1,
            agent: "solo".into(),
            model: "m".into(),
            input,
            cached,
            written: None,
            output: None,
            complete,
            request: Some(details(n, epoch)),
        });
    }
    let totals = &app.usage[&("solo".into(), "m".into())];
    assert_eq!(totals.cache.measured, 3);
    assert_eq!(totals.cache.requests, 6);
    assert!((totals.cache.percent().unwrap() - 9900.0 / 10200.0 * 100.0).abs() < 1e-8);
    assert_eq!(totals.first.percent(), Some(0.0));
    assert_eq!(totals.subsequent.percent(), Some(99.0));
    assert_eq!(totals.changes, vec!["epoch"]);
    app.tab = Tab::Usage;
    let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 35)).unwrap();
    term.draw(|f| sui::tui::draw::draw(f, &app)).unwrap();
    let screen = term
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    for expected in [
        "97.06%",
        "measured 3/6",
        "subsequent 99.00%",
        "changed: epoch",
        "TTL/routing stay unknown",
    ] {
        assert!(screen.contains(expected), "{expected}");
    }
}

#[test]
fn recent_sessions_picker_preserves_drafts_and_ignores_stale_results() {
    let mut app = App::with_state(dir(), Default::default(), UiSettings::default());
    app.modal = None;
    app.input.set("draft");
    app.command(Command::Sessions);
    let request = match app.effects.pop().unwrap() {
        Effect::ListSessions { request } => request,
        _ => panic!(),
    };
    app.sessions_loaded(
        request + 1,
        Ok(vec![sui::session::Summary {
            id: "wrong".into(),
            label: "wrong".into(),
        }]),
    );
    assert!(
        matches!(&app.modal,Some(Modal::Picker(p)) if p.loading && p.target==PickTarget::Session)
    );
    app.sessions_loaded(
        request,
        Ok(vec![sui::session::Summary {
            id: "source".into(),
            label: "A recorded session".into(),
        }]),
    );
    app.key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(matches!(app.effects.pop(),Some(Effect::ResumeSession{id}) if id=="source"));
    assert_eq!(app.input.text(), "draft");
    assert!(app.resume_pending);
    app.key(crossterm::event::KeyEvent::new(
        crossterm::event::KeyCode::Enter,
        crossterm::event::KeyModifiers::NONE,
    ));
    assert!(app.effects.is_empty());
    assert_eq!(app.input.text(), "draft");
}

#[test]
fn recovery_restores_compaction_epoch_and_monotonic_request_ids() {
    let root = dir();
    let ws = dir();
    let source = fixture(&root, &ws, "source", false);
    let mut j = Journal::open_named(&source, "solo").unwrap();
    j.log("turn_start", json!({"run":2}));
    j.log(
        "context_checkpoint",
        json!({"epoch":3,"messages":[{"role":"user","content":"checkpoint context"}]}),
    );
    j.log("request", json!({"request_id":9}));
    j.log("assistant", json!({"content":"continued","tool_calls":[]}));
    j.log("turn_end", json!({"run":2,"outcome":"returned"}));
    let saved = SavedSession::load(&root, "source", &ws).unwrap();
    assert_eq!(saved.epoch, 3);
    assert_eq!(saved.next_request, 10);
    assert_eq!(saved.history.len(), 2);
    assert!(
        matches!(&saved.history[0],sui::types::Message::User{content: sui::types::UserContent::Text(text)} if text == "checkpoint context")
    );
}

async fn finish(rx: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>, app: &mut App) {
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap()
            .unwrap();
        let done = matches!(event, UiEvent::RunDone { .. });
        app.apply_event(event);
        if done {
            break;
        }
    }
}
#[tokio::test]
async fn native_resume_retains_prefix_ids_and_results_without_reexecuting_tools() {
    let captures: Arc<Mutex<Vec<Value>>> = Arc::default();
    let out = captures.clone();
    let port = common::serve(move |raw, _msgs| {
        let value: Value = serde_json::from_slice(raw).unwrap();
        let mut c = out.lock().unwrap();
        let n = c.len();
        c.push(value);
        if n == 0 {
            common::sse_tool_calls(json!([common::tc(
                "write",
                "write_file",
                &json!({"path":"once.txt","content":"written once"}).to_string()
            )]))
        } else {
            common::sse_text("recorded answer")
        }
    });
    let ws = dir();
    let root = dir();
    let source = root.join("source");
    let p = profile(port);
    let profiles = [(
        "mock".into(),
        ProfileCfg {
            base_url: Some(p.base_url.clone()),
            model: Some("mock".into()),
            ..Default::default()
        },
    )]
    .into_iter()
    .collect();
    let mut app = App::with_state(ws.clone(), profiles, UiSettings::default());
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let solo = sui::tui::start_solo(
        p.clone(),
        ws.clone(),
        source.clone(),
        tx,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(true)),
        None,
        None,
    )
    .unwrap();
    assert!(solo.send(1, "first task".into()));
    finish(&mut rx, &mut app).await;
    assert!(SavedSession::load(&root, "source", &ws).is_err());
    drop(solo);
    tokio::time::sleep(Duration::from_millis(30)).await;
    let saved = SavedSession::load(&root, "source", &ws).unwrap();
    assert_eq!(saved.next_request, 2);
    let original = std::fs::read(source.join("solo.jsonl")).unwrap();
    let modified = std::fs::metadata(ws.join("once.txt"))
        .unwrap()
        .modified()
        .unwrap();
    let expected = serde_json::to_value(sui::context::compile(
        &saved.history,
        &sui::context::system(),
        None,
    ))
    .unwrap();
    let child = root.join("child");
    saved.fork(&child, "solo").unwrap();
    app.auto.store(false, std::sync::atomic::Ordering::Relaxed);
    app.outcome = "accepted".into();
    app.stage = "Accepted".into();
    app.accepted_sha = Some("old mission".into());
    app.restore_saved(&saved, "mock", child.clone());
    assert_eq!(app.outcome, "resumed (historical evidence)");
    assert!(app.stage.is_empty() && app.accepted_sha.is_none());
    assert!(!app.auto.load(std::sync::atomic::Ordering::Relaxed));
    assert!(!app.groups.is_empty());
    assert_eq!(app.next_run, 1);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let resumed = sui::tui::start_solo(
        p,
        ws.clone(),
        child.clone(),
        tx,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(AtomicBool::new(false)),
        app.auto.clone(),
        None,
        Some(&saved),
    )
    .unwrap();
    drop(saved);
    assert!(resumed.send(2, "continue the same task".into()));
    finish(&mut rx, &mut app).await;
    assert_eq!(std::fs::read(source.join("solo.jsonl")).unwrap(), original);
    assert_eq!(
        std::fs::metadata(ws.join("once.txt"))
            .unwrap()
            .modified()
            .unwrap(),
        modified
    );
    let captures = captures.lock().unwrap();
    assert_eq!(captures.len(), 3);
    let prefix = expected.as_array().unwrap();
    assert_eq!(
        &captures[2]["messages"].as_array().unwrap()[..prefix.len()],
        prefix
    );
    assert_eq!(captures[0]["tools"], captures[2]["tools"]);
    let journal = std::fs::read_to_string(child.join("solo.jsonl")).unwrap();
    assert!(journal.contains("\"request_id\":2"));
    assert_eq!(
        journal
            .lines()
            .filter(|l| l.contains("\"type\":\"tool\""))
            .count(),
        1
    );
}

#[test]
fn cli_resume_uses_a_fresh_run_and_rejects_changed_model() {
    let port = common::serve(|_, _| common::sse_text("CLI recovery verified"));
    let ws = dir();
    let task_home = dir();
    let config = ws.join("config.toml");
    std::fs::write(
        &config,
        format!("[provider]\nbase_url=\"http://127.0.0.1:{port}/v1\"\nmodel=\"mock\"\n"),
    )
    .unwrap();
    let invoke = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
            .current_dir(&ws)
            .env("HOME", &task_home)
            .env_remove("SUI_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args(["--config", config.to_str().unwrap()])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    assert!(invoke(&["remember this task"]).status.success());
    let root = task_home.join(".local/share/sui/runs");
    let rows = sui::session::recent(&root, &ws, None).unwrap();
    assert_eq!(rows.len(), 1);
    let id = &rows[0].id;
    let source = root.join(id).join("headless.jsonl");
    let before = std::fs::read(&source).unwrap();
    let result = invoke(&["--resume", id, "continue"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("CLI recovery verified"));
    assert_eq!(std::fs::read(&source).unwrap(), before);
    assert_eq!(sui::session::recent(&root, &ws, None).unwrap().len(), 2);
    assert!(
        !invoke(&["--resume", id, "--model", "different", "continue"])
            .status
            .success()
    );
}

#[cfg(unix)]
#[test]
fn real_tui_resume_palette_and_usage_work_over_a_headless_pty() {
    use portable_pty::{CommandBuilder, PtySize};
    use std::io::{Read, Write};
    let calls: Arc<Mutex<usize>> = Arc::default();
    let count = calls.clone();
    let port = common::serve(move |_, _| {
        *count.lock().unwrap() += 1;
        common::sse_text("PTY recovery verified")
    });
    let ws = dir();
    let task_home = dir();
    let config_dir = task_home.join(".config/sui");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"),format!("[profiles.mock]\nbase_url=\"http://127.0.0.1:{port}/v1\"\nmodel=\"mock\"\n[ui]\nworkspace={}\nsolo_profile=\"mock\"\ntheme=\"terminal\"\nmotion=\"off\"\n",serde_json::to_string(&ws).unwrap())).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .current_dir(&ws)
        .env("HOME", &task_home)
        .env_remove("SUI_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .args(["--profile", "mock", "a completed task"])
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(result.status.success());
    let root = task_home.join(".local/share/sui/runs");
    let rows = sui::session::recent(&root, &ws, None).unwrap();
    let id = &rows[0].id;
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 40,
            cols: 150,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_sui"));
    cmd.args(["--resume", id]);
    cmd.cwd(&ws);
    cmd.env("HOME", &task_home);
    cmd.env("SUI_MOTION", "off");
    cmd.env("NO_COLOR", "1");
    cmd.env("TERM", "xterm-256color");
    let mut child = pair.slave.spawn_command(cmd).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = [0; 8192];
        while let Ok(n) = reader.read(&mut buf) {
            if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut output = String::new();
    let wait = |needle: &str, output: &mut String| -> bool {
        let until = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < until {
            if let Ok(bytes) = rx.recv_timeout(Duration::from_millis(50)) {
                output.push_str(&String::from_utf8_lossy(&bytes));
            }
            if output.contains(needle) {
                return true;
            }
            if output.len() > 2 * 1024 * 1024 {
                break;
            }
        }
        false
    };
    let recovered = wait("resumed", &mut output);
    // Differential terminal frames can position the cursor between words.
    let displayed = ["PTY", "recovery", "verified"]
        .iter()
        .all(|word| output.contains(word));
    let no_call = *calls.lock().unwrap() == 1;
    writer.write_all(b"\x10Recent\r").unwrap();
    writer.flush().unwrap();
    let palette = wait("workspace)", &mut output);
    writer.write_all(b"\x1b").unwrap();
    writer.flush().unwrap();
    std::thread::sleep(Duration::from_millis(100));
    writer.write_all(b"\x14\x14\x14").unwrap();
    writer.flush().unwrap();
    let cache = wait("1/1", &mut output) && output.contains("measured");
    let rate = output.contains("50.00%");
    let _ = child.kill();
    let _ = child.wait();
    drop(writer);
    drop(pair.master);
    assert!(recovered&&displayed&&no_call&&palette&&cache&&rate,"recovered={recovered} displayed={displayed} no_call={no_call} palette={palette} cache={cache} rate={rate}\n{}",&output[output.len().saturating_sub(2500)..]);
}
