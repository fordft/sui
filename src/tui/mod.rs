//! `sui tui` — terminal UI over the v0.2 core.
//!
//! One event loop: crossterm input + core UiEvents + internal control
//! replies, folded through App (pure state) → Effects executed here.
//! Rendering is capped at ~30fps and only happens when state is dirty.

pub mod app;
pub mod commands;
pub mod draw;
pub mod fx;
pub mod gfx;
pub mod hero;
pub mod layout;
pub mod runner;
pub mod slime;
pub mod text;
pub mod theme;
pub mod transcript;
pub mod usage;

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
use std::sync::atomic::AtomicBool;
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
    LoginPrompt(u64, crate::auth::login::Prompt),
    LoginDone(u64, Result<(), String>),
    Models(u64, Result<Vec<ModelInfo>, String>),
    ProbeDone(String, Result<provider::Probe, String>),
    ProfileSaved,
    Diff(String),
    WebTest(Result<String, String>),
    Sessions(u64, Result<Vec<crate::session::Summary>, String>),
    Resumed(Result<Box<(crate::session::SavedSession, Profile, PathBuf)>, String>),
}

struct LoginJob {
    request: u64,
    input: Option<tokio::sync::oneshot::Sender<String>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for LoginJob {
    fn drop(&mut self) {
        self.task.abort();
    }
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
    tx: UnboundedSender<SoloCommand>,
    sig: String, // profile+model signature; change → respawn
    pub conversation_id: String,
}
enum SoloCommand {
    Turn(SoloTask),
    Mission(Box<mission::MissionCfg>),
}

struct SoloTask {
    run: u64,
    msg: String,
    web: WebUpdate,
    mission_chat: bool,
    desktop_yolo: Option<Arc<AtomicBool>>,
    images: Vec<crate::types::ContentPart>,
}
enum WebUpdate {
    Keep,
    Replace(Option<Arc<crate::web::WebService>>),
}
impl Solo {
    pub fn send(&self, run: u64, msg: String) -> bool {
        self.tx
            .send(SoloCommand::Turn(SoloTask {
                run,
                msg,
                web: WebUpdate::Keep,
                mission_chat: false,
                desktop_yolo: None,
                images: Vec::new(),
            }))
            .is_ok()
    }

    /// Desktop approval policy stays live while a turn is running.
    pub fn send_desktop(
        &self,
        run: u64,
        msg: String,
        images: Vec<crate::types::ContentPart>,
        yolo: Arc<AtomicBool>,
    ) -> bool {
        self.tx
            .send(SoloCommand::Turn(SoloTask {
                run,
                msg,
                web: WebUpdate::Keep,
                mission_chat: false,
                desktop_yolo: Some(yolo),
                images,
            }))
            .is_ok()
    }

    /// Apply current UI web policy without replacing the conversation.
    pub fn send_with_web(
        &self,
        run: u64,
        msg: String,
        web: Option<Arc<crate::web::WebService>>,
    ) -> bool {
        self.tx
            .send(SoloCommand::Turn(SoloTask {
                run,
                msg,
                web: WebUpdate::Replace(web),
                mission_chat: false,
                desktop_yolo: None,
                images: Vec::new(),
            }))
            .is_ok()
    }
    pub fn send_mission_chat(
        &self,
        run: u64,
        msg: String,
        web: Option<Arc<crate::web::WebService>>,
        images: Vec<crate::types::ContentPart>,
        desktop_yolo: Option<Arc<AtomicBool>>,
    ) -> bool {
        self.tx
            .send(SoloCommand::Turn(SoloTask {
                run,
                msg,
                web: WebUpdate::Replace(web),
                mission_chat: true,
                desktop_yolo,
                images,
            }))
            .is_ok()
    }

    pub fn run_mission(&self, cfg: mission::MissionCfg) -> bool {
        self.tx.send(SoloCommand::Mission(Box::new(cfg))).is_ok()
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
    let backup_sink = sink.clone();
    match start_solo(
        prof, workspace, jdir, sink, cancel, flag, session, web, None,
    ) {
        Ok(solo) => solo,
        Err(e) => {
            let _ = backup_sink.send(UiEvent::Error {
                run: 0,
                agent: "ui".into(),
                msg: format!("session init: {e:#}"),
            });
            let (tx, _) = unbounded_channel();
            Solo {
                conversation_id: String::new(),
                tx,
                sig: String::new(),
            }
        }
    }
}

fn solo_signature(prof: &Profile) -> String {
    format!(
        "{}:{}:{}",
        prof.name,
        Provider::from_profile(prof).resume_fingerprint(),
        context::sha256_hex(prof.api_key.as_deref().unwrap_or("").as_bytes())
    )
}

#[allow(clippy::too_many_arguments)]
pub fn start_solo(
    prof: Profile,
    workspace: PathBuf,
    jdir: PathBuf,
    sink: Sink,
    cancel: Arc<tokio::sync::Notify>,
    flag: Arc<std::sync::atomic::AtomicBool>,
    session: Arc<std::sync::atomic::AtomicBool>,
    web: Option<Arc<crate::web::WebService>>,
    saved: Option<&crate::session::SavedSession>,
) -> Result<Solo> {
    start_solo_with_continuation(
        prof, workspace, jdir, sink, cancel, flag, session, web, saved, None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn start_solo_with_continuation(
    prof: Profile,
    workspace: PathBuf,
    jdir: PathBuf,
    sink: Sink,
    cancel: Arc<tokio::sync::Notify>,
    flag: Arc<std::sync::atomic::AtomicBool>,
    session: Arc<std::sync::atomic::AtomicBool>,
    web: Option<Arc<crate::web::WebService>>,
    saved: Option<&crate::session::SavedSession>,
    continuation: Option<&crate::continuation::Continuation>,
) -> Result<Solo> {
    config::validate_native_config(None, &workspace)?;
    let lock = crate::session::SessionLock::acquire(&jdir)?;
    let (tx, mut rx) = unbounded_channel::<SoloCommand>();
    let sig = solo_signature(&prof);
    let workspace_for_log = workspace.canonicalize()?.display().to_string();
    let al = crate::config::agent_limits(&workspace);
    let conversation_id = saved
        .map(|s| s.header.session_id.clone())
        .unwrap_or_else(|| format!("sui-{:032x}", rand::random::<u128>()));
    let mut agent = Agent::new(
        Provider::from_profile(&prof),
        ToolContext {
            workspace,
            bash_timeout: Duration::from_millis(al.bash_timeout_ms),
            bash_timeout_max: Duration::from_millis(al.bash_timeout_max_ms),
            web,
            canon_root: std::sync::OnceLock::new(),
            ui: std::sync::OnceLock::new(),
            code_intel: Default::default(),
            code_context: Default::default(),
            tool_outputs: Default::default(),
        },
        Gate::new(false), // approvals via modal; session flag is live
        Journal::open_named(&jdir, "solo")?,
        Limits {
            max_turns: al.max_turns,
            context_budget: al.context_token_budget,
            context_reserve: al.context_reserve_tokens,
            compact_context: al.context_compaction,
            request_timeout: Duration::from_millis(al.request_timeout_ms),
        },
        Identity {
            session_id: conversation_id.clone(),
            agent_id: saved
                .as_ref()
                .map(|s| s.header.agent_id.clone())
                .unwrap_or_else(|| "solo".into()),
            role: "worker".into(),
            base_url: prof.base_url.clone(),
            model: prof.model.clone(),
            cache_key_fingerprint: prof
                .prompt_cache_key
                .as_ref()
                .map(|k| context::sha256_hex(k.as_bytes())),
        },
    );
    agent.enable_mission_discussion();
    agent.set_quiet(true);
    agent.wire_ui(sink.clone(), cancel, flag.clone(), Some(session.clone()));
    agent.set_run_id(0);
    if let Some(saved) = saved {
        agent.restore_session(saved)?;
        let mission_chat = crate::continuation::mode(&saved.events) == "mission";
        agent.set_mission_chat(mission_chat);
        agent.jlog("session_resumed", serde_json::json!({"source": saved.id}));
    } else {
        agent.record_session(Some(prof.name.clone()))?;
        if let Some(handoff) = continuation {
            agent.continue_conversation(handoff)?;
        }
    }
    tokio::spawn(async move {
        let _session_lock = lock;
        agent.jlog(
            crate::journal::ev::SESSION,
            serde_json::json!({
                "mode": "solo",
                "workspace": workspace_for_log,
                "sui_version": env!("CARGO_PKG_VERSION"),
            }),
        );
        while let Some(command) = rx.recv().await {
            let SoloTask {
                run,
                msg,
                web,
                mission_chat,
                desktop_yolo,
                images,
            } = match command {
                SoloCommand::Turn(task) => task,
                SoloCommand::Mission(cfg) => {
                    run_interactive_mission(&mut agent, *cfg, &sink).await;
                    continue;
                }
            };
            agent.set_mission_chat(mission_chat);
            agent.set_desktop_approval(desktop_yolo);
            if let WebUpdate::Replace(web) = web {
                agent.set_web_service(web);
            }
            agent.set_run_id(run);
            agent.jlog("task", serde_json::json!({
                "task": msg,
                "run": run,
                "mode": if mission_chat { "mission-chat" } else { "solo" },
                "approval": if session.load(std::sync::atomic::Ordering::Relaxed) { "auto" } else { "ask" },
            }));
            let r = agent.run_turn_with_images(&msg, images).await;
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
    Ok(Solo {
        tx,
        sig,
        conversation_id,
    })
}

/// Serialize execution with coordinator conversation. Journal the runtime result
/// before publishing completion, so reconnect and follow-up chat see it.
async fn run_interactive_mission(agent: &mut Agent, mut cfg: mission::MissionCfg, sink: &Sink) {
    let run = cfg.run;
    let dir = cfg.run_dir.clone();
    let request = cfg.objective.clone();
    let objective = agent.mission_objective(&request);
    agent.set_run_id(run);
    agent.set_mission_chat(true);
    agent.jlog(
        "task",
        serde_json::json!({"task":request,"run":run,"mode":"mission-run"}),
    );
    agent.jlog("turn_start", serde_json::json!({"run":run}));
    agent.push_user(&format!("The user requested Mission execution: {request}"));
    let result = match objective {
        Ok(objective) => {
            cfg.objective = objective;
            let (tx, mut rx) = unbounded_channel();
            cfg.events = Some(tx);
            let out = sink.clone();
            let forward = tokio::spawn(async move {
                while let Some(event) = rx.recv().await {
                    if !matches!(event, UiEvent::RunDone { .. }) {
                        let _ = out.send(event);
                    }
                }
            });
            let result = mission::run(cfg).await;
            let _ = forward.await;
            result
        }
        Err(error) => Err(error),
    };
    let (outcome, branch, sha) = match result {
        Ok(report) => (report.outcome, report.branch, report.accepted_sha),
        Err(error) => (format!("error: {error:#}"), None, None),
    };
    agent.observe_runtime(mission::chat::summary(
        &outcome,
        branch.as_deref(),
        sha.as_deref(),
        &dir,
    ));
    agent.jlog("mission_result", serde_json::json!({"run":run,"outcome":outcome,"branch":branch,"accepted_sha":sha,"run_dir":dir}));
    agent.jlog("turn_end", serde_json::json!({"run":run,"outcome":outcome}));
    let _ = sink.send(UiEvent::MissionResult {
        run,
        outcome: outcome.clone(),
        branch,
        accepted_sha: sha.clone(),
        run_dir: dir,
    });
    let _ = sink.send(UiEvent::RunDone {
        run,
        outcome,
        accepted_sha: sha,
    });
}

pub fn resolve_to_profile(app: &App, name: &str) -> Option<Profile> {
    let (base_url, key, model) = app.resolve(name)?;
    let transport =
        config::Transport::from_kind(app.profiles.get(name).and_then(|p| p.kind.as_deref()))
            .ok()?;
    Some(Profile {
        reasoning_effort: app.profiles.get(name).and_then(|p| p.reasoning_effort),
        transport: config::Transport::from_kind(
            app.profiles.get(name).and_then(|p| p.kind.as_deref()),
        )
        .ok()?,
        name: name.into(),
        image_input: app
            .profiles
            .get(name)
            .and_then(|p| p.image_input)
            .unwrap_or(transport.default_image_input()),
        base_url,
        model,
        api_key: key,
        prompt_cache_key: app
            .profiles
            .get(name)
            .and_then(|p| p.prompt_cache_key.clone()),
        pricing: if transport.is_account() {
            None
        } else {
            app.profiles.get(name).and_then(|p| p.pricing.clone())
        },
    })
}

pub async fn run(force_mission: bool, yolo: bool) -> Result<()> {
    run_with_resume(force_mission, yolo, None, None).await
}

pub async fn run_with_resume(
    force_mission: bool,
    yolo: bool,
    resume: Option<String>,
    workspace: Option<PathBuf>,
) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        anyhow::bail!("sui tui needs a terminal");
    }
    let workspace = workspace
        .or_else(|| config::load_ui().workspace.map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from("."));

    config::validate_native_config(None, &workspace)?;

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
    let mut jdir = run_dir();
    let mut app = App::new(workspace.clone());
    if app.mouse {
        let _ = out.write_all(MOUSE_ON.as_bytes());
    }
    app.run_dir = Some(jdir.clone());
    if force_mission {
        app.mode = app::Mode::Mission;
        app.sidebar = true;
        app.ui.mode = Some("mission".into());
    }
    if yolo {
        app.auto.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(id) = resume {
        app.resume_pending = true;
        app.status = "loading session…".into();
        app.effects.push(Effect::ResumeSession { id });
    }

    let (ev_tx, mut ev_rx) = unbounded_channel::<UiEvent>();
    let (ctl_tx, mut ctl_rx) = unbounded_channel::<Ctl>();
    let mut keys = EventStream::new();
    let mut solo: Option<Solo> = None;
    let mut login_job: Option<LoginJob> = None;
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
        } else if let Some(ms) = fx::frame_ms(&app, slime::now_ms()) {
            Duration::from_millis(ms)
        } else {
            Duration::from_millis(200)
        };
        tokio::select! {
            biased;
            ev = keys.next() => {
                if matches!(ev, Some(Ok(Event::Key(_) | Event::Mouse(_) | Event::Paste(_)))) {
                    app.anim.touch(slime::now_ms());
                }
                if let Some(Ok(Event::Key(k))) = ev {
                    // raw mode: Ctrl+C arrives as a key event. App::key owns
                    // the Press/Release policy — permission shortcuts honor
                    // Release-only transports, text input ignores them.
                    // Press-only here: on Press+Release transports the
                    // Release would otherwise quit after the Press stopped.
                    if k.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                        && matches!(k.code, crossterm::event::KeyCode::Char('c'))
                        && k.kind != crossterm::event::KeyEventKind::Release
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
                        Ctl::LoginPrompt(request,prompt) => app.login_prompt(request,prompt),
                        Ctl::LoginDone(request,result) => {
                            if login_job.as_ref().is_some_and(|j| j.request == request) {
                                if result.is_ok() {app.profiles = config::profiles(None).unwrap_or_else(|_| app.profiles.clone());}
                                app.login_finished(request,result);
                                login_job = None;
                            }
                        }
                        Ctl::Models(request, r) => match r {
                            Ok(ms) => app.models_loaded(request, ms, None),
                            Err(e) => app.models_loaded(request, vec![], Some(e)),
                        },
                        Ctl::ProbeDone(n, r) => app.probe_done(&n, r),
                        Ctl::ProfileSaved => {
                            app.profiles = config::profiles(None).unwrap_or_default();
                        }
                        Ctl::Diff(s) => app.diff_text = s,
                        Ctl::Sessions(request, rows) => app.sessions_loaded(request, rows),
                        Ctl::Resumed(result) => {
                            app.resume_pending = false;
                            match result {
                                Ok(data) => {
                                    let (saved, profile, dir) = *data;
                                    if app.workspace != saved.header.workspace || resolve_to_profile(&app, &profile.name).map(|mut current| {
                                        if let Some(enabled) = saved.header.image_input { current.image_input = enabled; }
                                        if current.transport == config::Transport::Chatgpt { current.model = saved.header.model.clone(); }
                                        if saved.header.reasoning_effort.is_some() { current.reasoning_effort = saved.header.reasoning_effort; }
                                        solo_signature(&current)
                                    }) != Some(solo_signature(&profile)) {
                                        app.status = "workspace or profile changed while loading; select the session again".into();
                                        dirty = true;
                                        continue;
                                    }
                                    match start_solo(profile.clone(), app.workspace.clone(), dir.clone(), ev_tx.clone(), app.cancel.clone(), app.stop_flag.clone(), app.auto.clone(), app.web.clone(), Some(&saved)) {
                                        Ok(new_solo) => {
                                            if let Some(p) = app.profiles.get_mut(&profile.name) {
                                                p.reasoning_effort = profile.reasoning_effort;
                                                p.image_input = Some(profile.image_input);
                                                if profile.transport == config::Transport::Chatgpt { p.model = Some(profile.model.clone()); }
                                            }
                                            app.restore_saved(&saved, &profile.name, dir.clone());
                                            solo = Some(new_solo);
                                            jdir = dir;
                                        }
                                        Err(e) => app.status = format!("resume: {e:#}"),
                                    }
                                }
                                Err(e) => app.status = format!("resume: {e}"),
                            }
                        }
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
                if fx::frame_ms(&app, slime::now_ms()).is_some() || dirty { dirty = true; }
            }
        }

        fx::observe(&mut app, slime::now_ms());

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
                Effect::SignIn { provider, request } => {
                    let (input, receiver) = tokio::sync::oneshot::channel();
                    let tx = ctl_tx.clone();
                    let task = tokio::spawn(async move {
                        let result = async {
                            let session =
                                crate::auth::login::Session::begin(provider, false).await?;
                            let needs_input = session.prompt.input_required;
                            let _ = tx.send(Ctl::LoginPrompt(request, session.prompt.clone()));
                            crate::auth::login::open_browser(&session.prompt.url);
                            let code = if needs_input {
                                Some(
                                    tokio::time::timeout(Duration::from_secs(600), receiver)
                                        .await
                                        .context("sign-in input timed out")?
                                        .context("sign-in canceled")?,
                                )
                            } else {
                                None
                            };
                            session.finish(code).await?;
                            Ok::<(), anyhow::Error>(())
                        }
                        .await
                        .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Ctl::LoginDone(request, result));
                    });
                    login_job = Some(LoginJob {
                        request,
                        input: Some(input),
                        task,
                    });
                }
                Effect::SignInInput { request, input } => {
                    if let Some(job) = login_job.as_mut().filter(|j| j.request == request) {
                        if let Some(sender) = job.input.take() {
                            let _ = sender.send(input);
                        }
                    }
                }
                Effect::CancelSignIn { request } => {
                    if login_job.as_ref().is_some_and(|j| j.request == request) {
                        login_job = None;
                    }
                }
                Effect::Quit => quit = true,
                Effect::ListSessions { request } => {
                    let tx = ctl_tx.clone();
                    let workspace = app.workspace.clone();
                    let exclude = app.run_dir.clone();
                    tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            crate::session::recent(
                                &crate::session::runs_root(),
                                &workspace,
                                exclude.as_deref(),
                            )
                        })
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r.map_err(|e| format!("{e:#}")));
                        let _ = tx.send(Ctl::Sessions(request, result));
                    });
                }
                Effect::ResumeSession { id } => {
                    let workspace = app.workspace.clone();
                    let profiles: Vec<_> = app
                        .profiles
                        .keys()
                        .filter_map(|n| resolve_to_profile(&app, n))
                        .collect();
                    let tx = ctl_tx.clone();
                    tokio::spawn(async move {
                        let result = tokio::task::spawn_blocking(move || -> Result<_> {
                            let saved = crate::session::SavedSession::load(&crate::session::runs_root(), &id, &workspace)?;
                            let mut matches = profiles.into_iter().map(|mut p| {
                                if let Some(enabled) = saved.header.image_input { p.image_input = enabled; }
                                if p.transport == config::Transport::Chatgpt { p.model = saved.header.model.clone(); }
                                if saved.header.reasoning_effort.is_some() { p.reasoning_effort = saved.header.reasoning_effort; }
                                p
                            }).filter(|p| saved.header.profile.as_ref().is_none_or(|name| name == &p.name)
                                && saved.header.signature == crate::session::Signature::current(&Provider::from_profile(p), &workspace));
                            let profile = matches.next().context("recorded profile, model, tools or project guidance changed; restore the configuration or start a new session")?;
                            if matches.next().is_some() { anyhow::bail!("several profiles match; the recorded session needs a unique profile"); }
                            let dir = crate::session::new_run_dir()?;
                            saved.fork(&dir, "solo")?;
                            Ok(Box::new((saved, profile, dir)))
                        }).await.map_err(|e| e.to_string()).and_then(|r| r.map_err(|e| format!("{e:#}")));
                        let _ = tx.send(Ctl::Resumed(result));
                    });
                }
                Effect::Stop => {} // notify+flag already fired in app.stop()
                Effect::SendTask { task, mode, run } => {
                    let workspace = app.workspace.clone();
                    let role = if mode == Mode::Mission {
                        Role::Orchestrator
                    } else {
                        Role::Solo
                    };
                    let prof = app
                        .role_profile(role)
                        .and_then(|name| resolve_to_profile(&app, &name));
                    if let Some(p) = prof {
                        let sig = solo_signature(&p);
                        if solo.as_ref().map(|s| &s.sig) != Some(&sig) {
                            if solo.is_some() {
                                jdir = run_dir();
                            }
                            solo = Some(spawn_solo(
                                p,
                                workspace,
                                jdir.clone(),
                                ev_tx.clone(),
                                app.cancel.clone(),
                                app.stop_flag.clone(),
                                app.auto.clone(),
                                app.web.clone(),
                            ));
                        }
                        app.run_dir = Some(jdir.clone());
                        let sent = if mode == Mode::Mission {
                            solo.as_ref().unwrap().send_mission_chat(
                                run,
                                task,
                                app.web.clone(),
                                Vec::new(),
                                None,
                            )
                        } else {
                            solo.as_ref()
                                .unwrap()
                                .send_with_web(run, task, app.web.clone())
                        };
                        if !sent {
                            app.apply_event(UiEvent::RunDone {
                                run,
                                outcome: "error: conversation unavailable".into(),
                                accepted_sha: None,
                            });
                        }
                    } else {
                        app.apply_event(UiEvent::Error {
                            run,
                            agent: "ui".into(),
                            msg: "configure the conversation role in Settings → roles".into(),
                        });
                        app.apply_event(UiEvent::RunDone {
                            run,
                            outcome: "error: profile missing".into(),
                            accepted_sha: None,
                        });
                    }
                }
                Effect::RunMission { task, run } => {
                    let workspace = app.workspace.clone();
                    let control = app
                        .role_profile(Role::Orchestrator)
                        .and_then(|n| resolve_to_profile(&app, &n));
                    let worker = app
                        .role_profile(Role::Worker)
                        .and_then(|n| resolve_to_profile(&app, &n));
                    let auditor = app
                        .role_profile(Role::Auditor)
                        .and_then(|n| resolve_to_profile(&app, &n));
                    match (control, worker) {
                        (Some(control), Some(worker)) => {
                            let sig = solo_signature(&control);
                            if solo.as_ref().map(|s| &s.sig) != Some(&sig) {
                                if solo.is_some() {
                                    jdir = run_dir();
                                }
                                solo = Some(spawn_solo(
                                    control.clone(),
                                    workspace.clone(),
                                    jdir.clone(),
                                    ev_tx.clone(),
                                    app.cancel.clone(),
                                    app.stop_flag.clone(),
                                    app.auto.clone(),
                                    app.web.clone(),
                                ));
                            }
                            let al = config::agent_limits(&workspace);
                            let mission_dir = jdir.join(format!("mission-{run}"));
                            app.run_dir = Some(mission_dir.clone());
                            let cfg = mission::MissionCfg {
                                repo: workspace,
                                run_dir: mission_dir,
                                control,
                                worker,
                                auditor,
                                objective: task,
                                max_workers: app.ui.worker_count.unwrap_or(1).clamp(1, 2),
                                session: format!("tui-{:032x}", rand::random::<u128>()),
                                keep_worktrees: false,
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
                            if !solo.as_ref().unwrap().run_mission(cfg) {
                                app.apply_event(UiEvent::RunDone {
                                    run,
                                    outcome: "error: conversation unavailable".into(),
                                    accepted_sha: None,
                                });
                            }
                        }
                        _ => {
                            app.apply_event(UiEvent::Error { run, agent:"ui".into(), msg:"Mission execution needs orchestrator and worker profiles in Settings → roles".into() });
                            app.apply_event(UiEvent::RunDone {
                                run,
                                outcome: "error: profiles missing".into(),
                                accepted_sha: None,
                            });
                        }
                    }
                }
                Effect::SaveProfile {
                    name,
                    base_url,
                    model,
                    key_env,
                    key,
                    store,
                    kind,
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
                        kind.as_deref(),
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
                Effect::FetchModels {
                    transport,
                    base_url,
                    key,
                    request,
                } => {
                    let tx = ctl_tx.clone();
                    tokio::spawn(async move {
                        let r = provider::list_models_with_transport(
                            &base_url,
                            key.as_deref(),
                            transport,
                        )
                        .await
                        .map_err(|e| format!("{e:#}"));
                        let _ = tx.send(Ctl::Models(request, r));
                    });
                }
                Effect::Probe {
                    transport,
                    name,
                    base_url,
                    model,
                    key,
                } => {
                    let tx = ctl_tx.clone();
                    tokio::spawn(async move {
                        let r = provider::probe_with_transport(
                            &base_url,
                            key.as_deref(),
                            &model,
                            transport,
                        )
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
