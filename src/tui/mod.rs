//! `sui tui` — terminal UI over the v0.2 core.
//!
//! One event loop: crossterm input + core UiEvents + internal control
//! replies, folded through App (pure state) → Effects executed here.
//! Rendering is capped at ~30fps and only happens when state is dirty.

pub mod app;
pub mod draw;
pub mod text;
pub mod transcript;

use anyhow::{Context, Result};
use crossterm::event::{DisableBracketedPaste, EnableBracketedPaste};
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{stdout, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

use crate::agent::{Agent, Identity, Limits};
use crate::config::{self, Profile};
use crate::context;
use crate::events::{Sink, UiEvent};
use crate::journal::Journal;
use crate::mission;
use crate::permission::Gate;
use crate::provider::{self, ModelInfo, Provider};
use crate::tools::ToolContext;
use app::*;

/// Internal replies from async effects back into the app.
enum Ctl {
    Models(Result<Vec<ModelInfo>, String>),
    ProbeDone(String, Result<provider::Probe, String>),
    ProfileSaved,
    Diff(String),
    WebTest(Result<String, String>),
}

/// Mouse capture: clicks+drags (1000/1002) + SGR encoding (1006).
/// Deliberately not `EnableMouseCapture` — it also sets 1003 (report
/// every mouse move), which we don't need: it floods SSH sessions with
/// motion bytes and wakes the event loop for nothing. Drag events for
/// selection still arrive via 1002.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1002l\x1b[?1000l\x1b[?1006l";

/// Restore the terminal no matter how we leave (drop, panic, error).
struct Term;
impl Drop for Term {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let mut s = stdout();
        let _ = s.write_all(MOUSE_OFF.as_bytes());
        let _ = execute!(s, LeaveAlternateScreen, DisableBracketedPaste);
    }
}

fn run_dir() -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let d = std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(format!(
            ".local/share/sui/runs/tui-{ts}-{}",
            std::process::id()
        ));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// A long-lived solo agent: one conversation session (append-only
/// history keeps the provider cache warm across chat turns).
pub struct Solo {
    tx: UnboundedSender<(u64, String)>, // (activity run id, task text)
    sig: String,                        // profile+model signature; change → respawn
}
impl Solo {
    pub fn send(&self, run: u64, msg: String) {
        let _ = self.tx.send((run, msg));
    }
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_solo(
    prof: Profile,
    workspace: PathBuf,
    jdir: PathBuf,
    sink: Sink,
    cancel: Arc<tokio::sync::Notify>,
    flag: Arc<std::sync::atomic::AtomicBool>,
    session: Arc<std::sync::atomic::AtomicBool>,
    web: Option<Arc<crate::web::WebService>>,
) -> Solo {
    let (tx, mut rx) = unbounded_channel::<(u64, String)>();
    let sig = format!("{}:{}:{}", prof.name, prof.base_url, prof.model);
    let workspace_for_log = workspace.display().to_string();
    let al = crate::config::agent_limits(&workspace);
    tokio::spawn(async move {
        let mut agent = Agent::new(
            Provider::new(
                &prof.base_url,
                prof.api_key.clone(),
                prof.model.clone(),
                prof.prompt_cache_key.clone(),
            ),
            ToolContext {
                workspace,
                bash_timeout: Duration::from_millis(al.bash_timeout_ms),
                bash_timeout_max: Duration::from_millis(al.bash_timeout_max_ms),
                web,
            },
            Gate::new(false), // approvals via modal; session flag is live
            match Journal::open_named(&jdir, "solo") {
                Ok(j) => j,
                Err(e) => {
                    let _ = sink.send(UiEvent::RunDone {
                        run: 0,
                        outcome: format!("journal init: {e:#}"),
                        accepted_sha: None,
                    });
                    return;
                }
            },
            Limits {
                max_turns: al.max_turns,
                context_budget: al.context_token_budget,
                context_reserve: al.context_reserve_tokens,
                request_timeout: Duration::from_millis(al.request_timeout_ms),
            },
            Identity {
                session_id: format!("tui-{}", std::process::id()),
                agent_id: "solo".into(),
                role: "worker".into(),
                base_url: prof.base_url.clone(),
                model: prof.model.clone(),
                cache_key_fingerprint: prof
                    .prompt_cache_key
                    .as_ref()
                    .map(|k| context::sha256_hex(k.as_bytes())),
            },
        );
        agent.set_quiet(true);
        agent.wire_ui(sink.clone(), cancel, flag.clone(), Some(session.clone()));
        agent.jlog(
            crate::journal::ev::SESSION,
            serde_json::json!({
                "mode": "solo",
                "workspace": workspace_for_log,
                "sui_version": env!("CARGO_PKG_VERSION"),
            }),
        );
        while let Some((run, msg)) = rx.recv().await {
            agent.set_run_id(run);
            agent.jlog("task", serde_json::json!({
                "task": msg,
                "run": run,
                "approval": if session.load(std::sync::atomic::Ordering::Relaxed) { "auto" } else { "ask" },
            }));
            let r = agent.run_turn(&msg).await;
            // a user stop ends the turn cleanly but is NOT a success —
            // the group must stay open with the interruption visible
            let stopped = flag.load(std::sync::atomic::Ordering::Relaxed);
            match r {
                Ok(()) => {
                    let outcome = if stopped { "stopped" } else { "done" };
                    agent.jlog(
                        "task_done",
                        serde_json::json!({ "outcome": outcome, "run": run }),
                    );
                    let _ = sink.send(UiEvent::RunDone {
                        run,
                        outcome: outcome.into(),
                        accepted_sha: None,
                    });
                }
                Err(e) => {
                    agent.jlog(
                        "task_done",
                        serde_json::json!({ "outcome": format!("error: {e:#}"), "run": run }),
                    );
                    let _ = sink.send(UiEvent::Error {
                        run,
                        agent: "solo".into(),
                        msg: format!("{e:#}"),
                    });
                    let _ = sink.send(UiEvent::RunDone {
                        run,
                        outcome: format!("error: {e:#}"),
                        accepted_sha: None,
                    });
                }
            }
        }
    });
    Solo { tx, sig }
}

pub fn resolve_to_profile(app: &App, name: &str) -> Option<Profile> {
    let (base_url, key, model) = app.resolve(name)?;
    Some(Profile {
        name: name.into(),
        base_url,
        model,
        api_key: key,
        prompt_cache_key: None,
        pricing: None,
    })
}

/// Role name → backend. `acp:<name>` selects a trusted external agent
/// from `[agents.<name>]`; anything else is a native provider profile.
pub fn resolve_to_backend(app: &App, name: &str) -> Option<crate::backend::Backend> {
    if let Some(agent) = name.strip_prefix("acp:") {
        return config::resolve_agent(agent, None)
            .ok()
            .map(crate::backend::Backend::Acp);
    }
    resolve_to_profile(app, name).map(crate::backend::Backend::Native)
}

pub async fn run(force_mission: bool, yolo: bool) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("sui tui needs a terminal");
    }
    let workspace = config::load_ui()
        .workspace
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));

    // terminal init + panic-safe restore
    enable_raw_mode().context("raw mode")?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste).context("alt screen")?;
    let _guard = Term;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |i| {
        let _ = disable_raw_mode();
        let _ = stdout().write_all(MOUSE_OFF.as_bytes());
        let _ = execute!(stdout(), LeaveAlternateScreen, DisableBracketedPaste);
        default_hook(i);
    }));

    let mut term = Terminal::new(CrosstermBackend::new(stdout()))?;
    let jdir = run_dir();
    let mut app = App::new(workspace.clone());
    if app.mouse {
        let _ = out.write_all(MOUSE_ON.as_bytes());
    }
    app.run_dir = Some(jdir.clone());
    if force_mission {
        app.mode = app::Mode::Mission;
        app.ui.mode = Some("mission".into());
    }
    if yolo {
        app.auto.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    let (ev_tx, mut ev_rx) = unbounded_channel::<UiEvent>();
    let (ctl_tx, mut ctl_rx) = unbounded_channel::<Ctl>();
    let mut keys = EventStream::new();
    let mut solo: Option<Solo> = None;
    let mut dirty = true;
    let mut last_draw = Instant::now() - Duration::from_millis(100);
    let mut quit = false;

    while !quit {
        // draw at most ~30fps, only when dirty
        if dirty && last_draw.elapsed() >= Duration::from_millis(33) {
            term.draw(|f| draw::draw(f, &app))?;
            last_draw = Instant::now();
            dirty = false;
        }
        // when a frame is pending but throttled, wake at the frame
        // boundary instead of the full heartbeat — a skipped draw must
        // never wait for the next input event
        let tick = if dirty {
            Duration::from_millis(33).saturating_sub(last_draw.elapsed())
        } else {
            Duration::from_millis(200)
        };
        tokio::select! {
            biased;
            ev = keys.next() => {
                if let Some(Ok(Event::Key(k))) = ev {
                    // raw mode: Ctrl+C arrives as a key event. App::key owns
                    // the Press/Release policy — permission shortcuts honor
                    // Release-only transports, text input ignores them.
                    if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        && matches!(k.code, crossterm::event::KeyCode::Char('c'))
                    {
                        if app.running { app.stop(); } else { quit = true; }
                        dirty = true;
                        continue;
                    }
                    app.key(k);
                    dirty = true;
                } else if let Some(Ok(Event::Mouse(m))) = ev {
                    app.mouse(m);
                    dirty = true;
                } else if let Some(Ok(Event::Paste(s))) = ev {
                    app.paste(&s);
                    dirty = true;
                } else if let Some(Ok(Event::Resize(w, h))) = ev {
                    app.on_resize(w, h);
                    dirty = true;
                }
            }
            ev = ev_rx.recv() => {
                if let Some(e) = ev {
                    app.apply_event(e);
                    dirty = true;
                }
            }
            c = ctl_rx.recv() => {
                if let Some(c) = c {
                    match c {
                        Ctl::Models(r) => match r {
                            Ok(ms) => app.models_loaded(ms, None),
                            Err(e) => app.models_loaded(vec![], Some(e)),
                        },
                        Ctl::ProbeDone(n, r) => app.probe_done(&n, r),
                        Ctl::ProfileSaved => {
                            app.profiles = config::profiles(None).unwrap_or_default();
                        }
                        Ctl::Diff(s) => app.diff_text = s,
                        Ctl::WebTest(r) => {
                            app.status = match r {
                                Ok(s) => s,
                                Err(e) => format!("web test: {e}"),
                            };
                        }
                    }
                    dirty = true;
                }
            }
            _ = tokio::time::sleep(tick) => {
                // heartbeat / pending-frame deadline
                if app.running || dirty { dirty = true; }
            }
        }

        // lazy diff load when the Changes tab becomes visible
        if app.tab == Tab::Changes && app.diff_stale {
            app.diff_stale = false;
            let ws = workspace.clone();
            let tx = ctl_tx.clone();
            tokio::spawn(async move {
                let out = tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&ws)
                    .args(["status", "--porcelain"])
                    .output()
                    .await;
                let diff = tokio::process::Command::new("git")
                    .arg("-C")
                    .arg(&ws)
                    .args(["diff", "--stat", "HEAD"])
                    .output()
                    .await;
                let mut s = String::new();
                if let Ok(o) = out {
                    s.push_str(&String::from_utf8_lossy(&o.stdout));
                }
                if let Ok(o) = diff {
                    s.push_str(&String::from_utf8_lossy(&o.stdout));
                }
                let _ = tx.send(Ctl::Diff(s));
            });
        }

        // execute queued effects
        for e in std::mem::take(&mut app.effects) {
            match e {
                Effect::Quit => quit = true,
                Effect::Stop => {} // notify+flag already fired in app.stop()
                Effect::SendTask { task, mode, run } => match mode {
                    Mode::Solo => {
                        let pname = app.role_profile(Role::Solo);
                        let prof = pname.as_deref().and_then(|n| resolve_to_profile(&app, n));
                        match prof {
                            Some(p) => {
                                let sig = format!("{}:{}:{}", p.name, p.base_url, p.model);
                                if solo.as_ref().map(|s| &s.sig) != Some(&sig) {
                                    solo = Some(spawn_solo(
                                        p,
                                        workspace.clone(),
                                        jdir.clone(),
                                        ev_tx.clone(),
                                        app.cancel.clone(),
                                        app.stop_flag.clone(),
                                        app.auto.clone(),
                                        app.web.clone(),
                                    ));
                                }
                                solo.as_ref().unwrap().send(run, task);
                            }
                            None => {
                                app.apply_event(UiEvent::Error {
                                    run,
                                    agent: "ui".into(),
                                    msg: "no solo profile configured — Settings → roles".into(),
                                });
                                app.apply_event(UiEvent::RunDone {
                                    run,
                                    outcome: "error: no solo profile".into(),
                                    accepted_sha: None,
                                });
                                app.running = false;
                            }
                        }
                    }
                    Mode::Mission => {
                        let control = app
                            .role_profile(Role::Orchestrator)
                            .and_then(|n| resolve_to_backend(&app, &n));
                        let worker = app
                            .role_profile(Role::Worker)
                            .and_then(|n| resolve_to_backend(&app, &n));
                        let auditor = app
                            .role_profile(Role::Auditor)
                            .and_then(|n| resolve_to_backend(&app, &n));
                        match (control, worker) {
                            (Some(control), Some(worker)) => {
                                let al = config::agent_limits(&workspace);
                                let cfg = mission::MissionCfg {
                                    repo: workspace.clone(),
                                    run_dir: jdir.clone(),
                                    control,
                                    worker,
                                    auditor,
                                    objective: task,
                                    max_workers: app.ui.worker_count.unwrap_or(1).clamp(1, 2),
                                    session: format!("tui-{}", std::process::id()),
                                    keep_worktrees: false, // accepted branch survives cleanup
                                    request_timeout: Duration::from_millis(al.request_timeout_ms),
                                    task_timeout: Duration::from_secs(900),
                                    context_budget: al.context_token_budget,
                                    context_reserve: al.context_reserve_tokens,
                                    control_max_turns: al.max_turns,
                                    worker_max_turns: al.max_turns,
                                    events: Some(ev_tx.clone()),
                                    cancel: Some((app.cancel.clone(), app.stop_flag.clone())),
                                    session_approve: Some(app.auto.clone()),
                                    web: app.web.clone(),
                                    run,
                                };
                                tokio::spawn(async move {
                                    let _ = mission::run(cfg).await;
                                });
                            }
                            _ => {
                                app.apply_event(UiEvent::Error {
                                    run,
                                    agent: "ui".into(),
                                    msg: "mission needs orchestrator + worker profiles — Settings"
                                        .into(),
                                });
                                app.apply_event(UiEvent::RunDone {
                                    run,
                                    outcome: "error: profiles missing".into(),
                                    accepted_sha: None,
                                });
                                app.running = false;
                            }
                        }
                    }
                },
                Effect::SaveProfile {
                    name,
                    base_url,
                    model,
                    key_env,
                    key,
                    store,
                } => {
                    // Store::ConfigFile persists the key inline; other
                    // stores keep it out of the file
                    let inline = if store == app::Store::ConfigFile {
                        key.clone()
                    } else {
                        None
                    };
                    match config::save_profile(
                        &name,
                        &base_url,
                        &model,
                        key_env.as_deref(),
                        inline.as_deref(),
                    ) {
                        Ok(()) => {
                            let note = match (&key, store) {
                                (Some(k), app::Store::Keychain) if app.keyring_ok => {
                                    match keyring::Entry::new("sui", &name)
                                        .and_then(|e| e.set_password(k))
                                    {
                                        Ok(()) => "key → OS keyring",
                                        Err(_) => "keyring write failed — key is session-only",
                                    }
                                }
                                (Some(_), app::Store::Keychain) => {
                                    "no OS keyring — key is session-only"
                                }
                                (Some(_), app::Store::ConfigFile) => {
                                    "key → config file (plaintext)"
                                }
                                (Some(_), app::Store::Session) => "key → session only",
                                (None, _) => "no key",
                            };
                            if let Some(k) = key {
                                app.session_keys.insert(name.clone(), k);
                            }
                            app.status = format!("saved {name} · {note}");
                            let _ = ctl_tx.send(Ctl::ProfileSaved);
                        }
                        Err(e) => app.status = format!("save failed: {e:#}"),
                    }
                }
                Effect::ExportRun => {
                    match app
                        .run_dir
                        .clone()
                        .map(|d| d.file_name().unwrap().to_string_lossy().to_string())
                    {
                        Some(id) => {
                            match crate::export::run_export(&crate::export::ExportOpts {
                                run_id: Some(id),
                                latest_for_workspace: None,
                                format: crate::export::Format::Markdown,
                                include_diff: false,
                                runs_root: None,
                                out_root: None,
                                running: app.running,
                            }) {
                                Ok(p) => {
                                    app.status =
                                        format!("report: {} — review before sharing", p.display());
                                }
                                Err(e) => app.status = format!("export: {e:#}"),
                            }
                        }
                        None => app.status = "export: no run dir".into(),
                    }
                }
                Effect::TestWeb => {
                    let tx = ctl_tx.clone();
                    match app.web.clone() {
                        Some(ws) => {
                            tokio::spawn(async move {
                                let r = ws.test().await.map_err(|e| format!("{e:#}"));
                                let _ = tx.send(Ctl::WebTest(r));
                            });
                            app.status = "web test…".into();
                        }
                        None => app.status = "web test: service unavailable".into(),
                    }
                }
                Effect::SaveUi => {
                    if let Err(e) = config::save_ui(&app.ui) {
                        app.status = format!("save ui: {e:#}");
                    }
                }
                Effect::FetchModels { base_url, key, .. } => {
                    let tx = ctl_tx.clone();
                    tokio::spawn(async move {
                        let r = provider::list_models(&base_url, key.as_deref())
                            .await
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Ctl::Models(r));
                    });
                }
                Effect::Probe {
                    name,
                    base_url,
                    model,
                    key,
                } => {
                    let tx = ctl_tx.clone();
                    tokio::spawn(async move {
                        let r = provider::probe(&base_url, key.as_deref(), &model)
                            .await
                            .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Ctl::ProbeDone(name, r));
                    });
                }
                Effect::KeyringStore { profile, key } => {
                    // same feedback contract as SaveProfile — a silently
                    // dropped failure reads as "key disappeared" next launch
                    app.status = match keyring::Entry::new("sui", &profile)
                        .and_then(|e| e.set_password(&key))
                    {
                        Ok(()) => format!("{profile}: key → OS keyring"),
                        Err(_) => format!("{profile}: keyring write failed — key is session-only"),
                    };
                }
                Effect::Clip(text) => {
                    // OSC52 → the local terminal's clipboard, works over
                    // SSH. A deliberate single write, not a stray print.
                    use base64::Engine;
                    let mut s = stdout();
                    let _ = s.write_all(
                        format!(
                            "\x1b]52;c;{}\x07",
                            base64::engine::general_purpose::STANDARD.encode(text)
                        )
                        .as_bytes(),
                    );
                    let _ = s.flush();
                }
                Effect::Mouse(on) => {
                    let mut s = stdout();
                    let _ = s.write_all(if on { MOUSE_ON } else { MOUSE_OFF }.as_bytes());
                    let _ = s.flush();
                }
            }
        }
    }
    Ok(())
}
