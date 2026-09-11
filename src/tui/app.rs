//! TUI application state — deliberately terminal-free so tests can drive
//! it headless. `key()` translates input to state changes + Effects;
//! `apply_event()` folds core UiEvents into view state. mod.rs executes
//! Effects against the real runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::config::{self, ProfileCfg, UiSettings};
use crate::events::{GateChoice, UiEvent};
use crate::mission::UsageAgg;
use crate::provider::Probe;
use super::text::Buf;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Screen {
    Setup,
    Main,
}

#[derive(Clone, Copy, PartialEq, PartialOrd)]
pub enum Tab {
    Chat = 0,
    Tasks = 1,
    Changes = 2,
    Usage = 3,
    Settings = 4,
}
impl Tab {
    pub const ALL: [Tab; 5] = [Tab::Chat, Tab::Tasks, Tab::Changes, Tab::Usage, Tab::Settings];
    pub fn name(self) -> &'static str {
        ["Chat", "Tasks", "Changes", "Usage", "Settings"][self as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mode {
    Solo,
    Mission,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProvType {
    DeepSeek,
    OpenRouter,
    Custom,
}
impl ProvType {
    pub const ALL: [ProvType; 3] = [ProvType::DeepSeek, ProvType::OpenRouter, ProvType::Custom];
    pub fn name(self) -> &'static str {
        ["DeepSeek", "OpenRouter", "Custom OpenAI-compatible"][self as usize]
    }
    pub fn default_url(self) -> &'static str {
        match self {
            ProvType::DeepSeek => "https://api.deepseek.com",
            ProvType::OpenRouter => "https://openrouter.ai/api/v1",
            ProvType::Custom => "",
        }
    }
    pub fn default_env(self) -> &'static str {
        match self {
            ProvType::DeepSeek => "DEEPSEEK_API_KEY",
            ProvType::OpenRouter => "OPENROUTER_API_KEY",
            ProvType::Custom => "",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuthMode {
    EnvVar,
    SessionKey,
    None,
}
impl AuthMode {
    pub const ALL: [AuthMode; 3] = [AuthMode::EnvVar, AuthMode::SessionKey, AuthMode::None];
    pub fn name(self) -> &'static str {
        ["Environment variable", "Session API key", "None"][self as usize]
    }
}

/// Provider editor form — fields navigable, typed input, masked key.
pub struct ProvForm {
    pub name: Buf,
    pub ptype: ProvType,
    pub base_url: Buf,
    pub auth: AuthMode,
    pub key_env: Buf,
    pub key: Buf, // session-only key entry; masked in the view
    pub remember: bool,
    pub model: Buf,
    pub focus: usize,
    pub editing: Option<String>, // original name when editing existing
    pub endpoint: String,        // preview of the real request destination
    pub status: String,
}

impl ProvForm {
    pub fn new(ptype: ProvType) -> Self {
        let mut f = Self {
            name: Buf::new(),
            ptype,
            base_url: Buf::from(ptype.default_url()),
            auth: AuthMode::EnvVar,
            key_env: Buf::from(ptype.default_env()),
            key: Buf::new(),
            remember: false,
            model: Buf::new(),
            focus: 0,
            editing: None,
            endpoint: String::new(),
            status: String::new(),
        };
        f.refresh_endpoint();
        f
    }
    pub fn from_existing(name: &str, p: &ProfileCfg) -> Self {
        let mut f = Self::new(ProvType::Custom);
        f.name.set(name);
        f.base_url.set(p.base_url.as_deref().unwrap_or(""));
        f.model.set(p.model.as_deref().unwrap_or(""));
        f.key_env.set(p.key_env.as_deref().unwrap_or(""));
        f.auth = if p.key_env.is_some() { AuthMode::EnvVar } else { AuthMode::None };
        f.editing = Some(name.to_string());
        f.refresh_endpoint();
        f
    }
    pub fn refresh_endpoint(&mut self) {
        self.endpoint = format!("{}/chat/completions", self.base_url.text().trim_end_matches('/'));
    }
    /// 0..8 fields, 9/10/11 = Test/Save/Cancel
    pub const FIELDS: usize = 12;
    fn cur(&mut self) -> Option<&mut Buf> {
        match self.focus {
            0 => Some(&mut self.name),
            2 => Some(&mut self.base_url),
            4 => Some(&mut self.key_env),
            5 => Some(&mut self.key),
            7 => Some(&mut self.model),
            _ => None,
        }
    }
}

pub struct Picker {
    pub title: String,
    pub items: Vec<String>,
    pub filter: Buf,
    pub sel: usize,
    pub target: PickTarget,
    pub loading: bool,
}

#[derive(Clone, PartialEq)]
pub enum PickTarget {
    ProvModel,            // into ProvForm.model
    Role(Role),
    ModelForRole(String), // after profile chosen → pick model
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Role {
    Solo,
    Orchestrator,
    Worker,
    Auditor,
}
impl Role {
    pub const ALL: [Role; 4] = [Role::Solo, Role::Orchestrator, Role::Worker, Role::Auditor];
    pub fn name(self) -> &'static str {
        ["Solo", "Orchestrator", "Workers", "Auditor"][self as usize]
    }
}

#[derive(Clone)]
pub enum ChatItem {
    User(String),
    Assistant { agent: String, text: String, live: bool },
    Tool { agent: String, name: String, summary: String, done: bool, ok: bool, result: String },
    Sys(String),
}

#[derive(Clone)]
pub struct TaskRow {
    pub id: String,
    pub status: String,
    pub owned: String,
    pub sha: String,
}

pub enum Modal {
    Provider(ProvForm),
    Picker(Picker),
    Permission { id: u64, summary: String, reply: Sender<GateChoice> },
    ConfirmTest { name: String }, // warn: probe costs one small request
    Text { title: String, buf: Buf, target: TextTarget },
    Help,
}

#[derive(Clone, PartialEq)]
pub enum TextTarget {
    Workspace,
    Acceptance,
}

/// Side-effect the loop must execute — keeps App pure.
pub enum Effect {
    SendTask { task: String, mode: Mode },
    Stop,
    Quit,
    SaveProfile { name: String, base_url: String, model: String, key_env: Option<String>, key: Option<String>, remember: bool },
    SaveUi,
    FetchModels { base_url: String, key: Option<String>, target: PickTarget },
    Probe { name: String, base_url: String, model: String, key: Option<String> },
    KeyringStore { profile: String, key: String },
}

pub struct App {
    pub screen: Screen,
    pub tab: Tab,
    pub sidebar: bool,
    pub mode: Mode,
    pub input: Buf,
    pub chat: Vec<ChatItem>,
    pub scroll: usize,
    pub tasks: Vec<TaskRow>,
    pub changes: Vec<String>,
    pub diff_text: String,
    pub diff_stale: bool,
    pub accepted_sha: Option<String>,
    pub audit: Option<String>,
    pub usage: BTreeMap<String, (String, UsageAgg)>, // agent → (model, agg)
    pub profiles: BTreeMap<String, ProfileCfg>,
    pub session_keys: BTreeMap<String, String>,
    pub ui: UiSettings,
    pub workspace: PathBuf,
    pub running: bool,
    pub stage: String,
    pub started: Option<Instant>,
    pub outcome: String,
    pub modal: Option<Modal>,
    pub form_stash: Option<ProvForm>,
    pub settings_sel: usize,
    pub status: String,
    pub cancel: Arc<tokio::sync::Notify>,
    pub stop_flag: Arc<AtomicBool>,
    pub effects: Vec<Effect>,
    pub keyring_ok: bool,
}

impl App {
    /// Production constructor: real config + keyring.
    pub fn new(workspace: PathBuf) -> Self {
        Self::with_state(
            workspace,
            config::profiles(None).unwrap_or_default(),
            config::load_ui(),
        )
    }

    /// Injectable constructor — tests pass their own profiles/ui.
    pub fn with_state(
        workspace: PathBuf,
        profiles: BTreeMap<String, ProfileCfg>,
        ui: UiSettings,
    ) -> Self {
        let no_profiles = profiles.is_empty();
        let mut session_keys = BTreeMap::new();
        let mut keyring_ok = true;
        for name in profiles.keys() {
            match keyring::Entry::new("sui", name).and_then(|e| e.get_password()) {
                Ok(k) => {
                    session_keys.insert(name.clone(), k);
                }
                Err(keyring::Error::NoEntry) => {}
                Err(_) => keyring_ok = false,
            }
        }
        Self {
            screen: if no_profiles { Screen::Setup } else { Screen::Main },
            tab: Tab::Chat,
            sidebar: true,
            mode: match ui.mode.as_deref() {
                Some("mission") => Mode::Mission,
                _ => Mode::Solo,
            },
            input: Buf::new(),
            chat: vec![ChatItem::Sys("welcome — configure a provider (Settings → add), pick models per role, then type a task".into())],
            scroll: 0,
            tasks: vec![],
            changes: vec![],
            diff_text: String::new(),
            diff_stale: true,
            accepted_sha: None,
            audit: None,
            usage: BTreeMap::new(),
            profiles,
            session_keys,
            ui,
            workspace,
            running: false,
            stage: "idle".into(),
            started: None,
            outcome: String::new(),
            modal: if no_profiles {
                Some(Modal::Provider(ProvForm::new(ProvType::DeepSeek)))
            } else {
                None
            },
            form_stash: None,
            settings_sel: 0,
            status: String::new(),
            cancel: Arc::new(tokio::sync::Notify::new()),
            stop_flag: Arc::new(AtomicBool::new(false)),
            effects: vec![],
            keyring_ok,
        }
    }

    // ── model resolution ────────────────────────────────────────────
    /// Resolve a profile into a concrete (base_url, key, model). Session
    /// keys override env; nothing is persisted here.
    pub fn resolve(&self, name: &str) -> Option<(String, Option<String>, String)> {
        let p = self.profiles.get(name)?;
        let key = self
            .session_keys
            .get(name)
            .cloned()
            .or_else(|| p.key_env.as_deref().and_then(|e| std::env::var(e).ok()))
            .or_else(|| p.api_key.clone());
        Some((
            p.base_url.clone().unwrap_or_else(|| "https://api.openai.com/v1".into()),
            key,
            p.model.clone().unwrap_or_default(),
        ))
    }

    pub fn role_profile(&self, r: Role) -> Option<String> {
        match r {
            Role::Solo => self.ui.solo_profile.clone(),
            Role::Orchestrator => self.ui.orchestrator_profile.clone(),
            Role::Worker => self.ui.worker_profile.clone(),
            Role::Auditor => self
                .ui
                .auditor_profile
                .clone()
                .or_else(|| self.ui.orchestrator_profile.clone()),
        }
        .or_else(|| self.profiles.keys().next().cloned())
    }

    // ── event application ───────────────────────────────────────────
    /// Bounded in-memory display buffer; full history lives in journals.
    const CHAT_CAP: usize = 2000;

    pub fn apply_event(&mut self, e: UiEvent) {
        if self.chat.len() > Self::CHAT_CAP {
            self.chat.drain(..self.chat.len() - Self::CHAT_CAP);
        }
        match e {
            UiEvent::Delta { agent, text } => {
                let appendable = matches!(
                    self.chat.last(),
                    Some(ChatItem::Assistant { agent: a, live: true, .. }) if *a == agent
                );
                if appendable {
                    if let Some(ChatItem::Assistant { text: t, .. }) = self.chat.last_mut() {
                        t.push_str(&text);
                    }
                } else {
                    self.chat.push(ChatItem::Assistant { agent, text, live: true });
                }
            }
            UiEvent::ToolStart { agent, name, summary } => {
                self.chat.push(ChatItem::Tool {
                    agent, name, summary, done: false, ok: false, result: String::new(),
                });
            }
            UiEvent::ToolDone { agent, name, ms, ok, result } => {
                for it in self.chat.iter_mut().rev() {
                    if let ChatItem::Tool { agent: a, name: n, done, ok: k, result: r, .. } = it {
                        if *a == agent && *n == name && !*done {
                            *done = true;
                            *k = ok;
                            *r = format!("[{ms}ms] {result}");
                            break;
                        }
                    }
                }
            }
            UiEvent::Usage { agent, model, input, cached, written, output, complete } => {
                let ent = self.usage.entry(agent).or_insert_with(|| (model.clone(), UsageAgg::default()));
                ent.0 = model;
                let u = &mut ent.1;
                u.requests += 1;
                if complete { u.telemetry_known += 1; }
                u.input += input.unwrap_or(0);
                u.cache_read += cached.unwrap_or(0);
                u.cache_write += written.unwrap_or(0);
                u.output += output.unwrap_or(0);
            }
            UiEvent::Permission { id, summary, reply } => {
                self.modal = Some(Modal::Permission { id, summary, reply });
            }
            UiEvent::MissionState(s) => {
                self.stage = s;
            }
            UiEvent::TaskRows(v) => {
                self.tasks = v
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(|t| TaskRow {
                        id: t["id"].as_str().unwrap_or("?").into(),
                        status: t["status"].as_str().unwrap_or("planned").into(),
                        owned: t["owned_paths"].as_array().into_iter().flatten()
                            .filter_map(|p| p.as_str()).collect::<Vec<_>>().join(" "),
                        sha: t["sha"].as_str().unwrap_or("").chars().take(8).collect(),
                    })
                    .collect();
            }
            UiEvent::ChangeSet { files, sha } => {
                self.changes = files;
                self.accepted_sha = sha;
                self.diff_stale = true;
            }
            UiEvent::AuditResult(v) => {
                self.audit = Some(serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            UiEvent::RunDone { outcome, accepted_sha } => {
                self.running = false;
                self.outcome = outcome.clone();
                if let Some(s) = accepted_sha {
                    self.accepted_sha = Some(s);
                }
                self.chat.push(ChatItem::Sys(format!("run finished: {outcome}")));
            }
            UiEvent::Error { agent, msg } => {
                self.chat.push(ChatItem::Sys(format!("error [{agent}]: {msg}")));
            }
        }
    }

    // ── input handling → effects ────────────────────────────────────
    pub fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // global quit preempts modal input — Ctrl+Q always works
        if ctrl && k.code == KeyCode::Char('q') {
            self.effects.push(Effect::Quit);
            return;
        }
        if let Some(m) = self.modal.take() {
            // handlers consume the modal and return the next state —
            // Some(m) stays open, a different Some replaces, None closes
            self.modal = self.modal_key(k, m);
            return;
        }
        match (ctrl, k.code) {
            (true, KeyCode::Char('s')) => self.stop(),
            (true, KeyCode::Char('t')) => {
                self.tab = Tab::ALL[((self.tab as usize) + 1) % 5]
            }
            (true, KeyCode::Char('b')) => self.sidebar = !self.sidebar,
            (true, KeyCode::Char('m')) => {
                self.mode = match self.mode {
                    Mode::Solo => Mode::Mission,
                    Mode::Mission => Mode::Solo,
                };
                self.ui.mode = Some(match self.mode {
                    Mode::Solo => "solo".into(),
                    Mode::Mission => "mission".into(),
                });
            }
            (true, KeyCode::Char('j')) => self.input.insert('\n'),
            (_, KeyCode::F(1)) => self.modal = Some(Modal::Help),
            (_, KeyCode::PageUp) => self.scroll = self.scroll.saturating_add(10),
            (_, KeyCode::PageDown) => self.scroll = self.scroll.saturating_sub(10),
            (_, KeyCode::Enter) => {
                if self.tab == Tab::Chat && !self.input.is_empty() && !self.running {
                    let task = self.input.text();
                    self.input.clear();
                    self.chat.push(ChatItem::User(task.clone()));
                    self.running = true;
                    self.started = Some(Instant::now());
                    self.outcome.clear();
                    self.stop_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                    self.effects.push(Effect::SendTask { task, mode: self.mode });
                } else if self.tab == Tab::Settings {
                    self.settings_activate(self.settings_sel);
                }
            }
            (_, KeyCode::Up) => {
                if self.tab == Tab::Settings {
                    self.settings_sel = self.settings_sel.saturating_sub(1);
                } else {
                    self.scroll = self.scroll.saturating_add(1);
                }
            }
            (_, KeyCode::Down) => {
                if self.tab == Tab::Settings {
                    let n = self.settings_rows().len();
                    if self.settings_sel + 1 < n {
                        self.settings_sel += 1;
                    }
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            (_, KeyCode::Esc) => {
                if self.tab == Tab::Settings {
                    // no-op
                }
            }
            (_, KeyCode::Backspace) => {
                if self.tab == Tab::Chat { self.input.backspace(); }
            }
            (_, KeyCode::Delete) => {
                if self.tab == Tab::Chat { self.input.delete(); }
            }
            (_, KeyCode::Left) => {
                if self.tab == Tab::Chat { self.input.left(); }
            }
            (_, KeyCode::Right) => {
                if self.tab == Tab::Chat { self.input.right(); }
            }
            (_, KeyCode::Home) => {
                if self.tab == Tab::Chat { self.input.home(); }
            }
            (_, KeyCode::End) => {
                if self.tab == Tab::Chat { self.input.end(); }
            }
            (_, KeyCode::Char(c)) => {
                if self.tab == Tab::Chat { self.input.insert(c); }
            }
            _ => {}
        }
    }

    pub fn paste(&mut self, s: &str) {
        // modal field wins over chat input — pasting an API key into the
        // setup form must not leak it into the task box
        match &mut self.modal {
            Some(Modal::Provider(f)) => {
                if let Some(b) = f.cur() {
                    b.insert_str(s);
                    f.refresh_endpoint();
                }
            }
            Some(Modal::Picker(p)) => {
                p.filter.insert_str(s);
                p.sel = 0;
            }
            Some(Modal::Text { buf, .. }) => buf.insert_str(s),
            Some(_) => {}
            None => {
                if self.tab == Tab::Chat {
                    self.input.insert_str(s);
                }
            }
        }
    }

    pub fn stop(&mut self) {
        if self.running {
            self.stop_flag.store(true, std::sync::atomic::Ordering::Relaxed);
            self.cancel.notify_waiters();
            self.effects.push(Effect::Stop);
            self.status = "stopping…".into();
        }
    }

    // ── modal keys ──────────────────────────────────────────────────
    /// Handlers consume the modal and return the next modal state.
    fn modal_key(&mut self, k: KeyEvent, m: Modal) -> Option<Modal> {
        match m {
            Modal::Permission { id, summary, reply } => match k.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let _ = reply.send(GateChoice::Once);
                    None
                }
                KeyCode::Char('a') => {
                    let _ = reply.send(GateChoice::Session);
                    None
                }
                KeyCode::Char('n') | KeyCode::Esc => {
                    let _ = reply.send(GateChoice::Deny);
                    None
                }
                _ => Some(Modal::Permission { id, summary, reply }),
            },
            Modal::Help => {
                if matches!(k.code, KeyCode::Esc | KeyCode::Enter | KeyCode::F(1)) {
                    None
                } else {
                    Some(Modal::Help)
                }
            }
            Modal::ConfirmTest { name } => match k.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    if let Some((base, key, model)) = self.resolve(&name) {
                        self.effects.push(Effect::Probe { name, base_url: base, model, key });
                    }
                    None
                }
                KeyCode::Char('n') | KeyCode::Esc => None,
                _ => Some(Modal::ConfirmTest { name }),
            },
            Modal::Provider(f) => self.provider_key(k, f),
            Modal::Picker(p) => self.picker_key(k, p),
            Modal::Text { title, mut buf, target } => match k.code {
                KeyCode::Esc => None,
                KeyCode::Enter => {
                    let v = buf.text();
                    match target {
                        TextTarget::Workspace => {
                            self.ui.workspace = if v.is_empty() { None } else { Some(v) };
                        }
                        TextTarget::Acceptance => {
                            if !v.is_empty() {
                                self.ui.acceptance.push(v);
                            }
                        }
                    }
                    self.effects.push(Effect::SaveUi);
                    None
                }
                KeyCode::Backspace => {
                    buf.backspace();
                    Some(Modal::Text { title, buf, target })
                }
                KeyCode::Delete => {
                    buf.delete();
                    Some(Modal::Text { title, buf, target })
                }
                KeyCode::Left => {
                    buf.left();
                    Some(Modal::Text { title, buf, target })
                }
                KeyCode::Right => {
                    buf.right();
                    Some(Modal::Text { title, buf, target })
                }
                KeyCode::Char(c) => {
                    buf.insert(c);
                    Some(Modal::Text { title, buf, target })
                }
                _ => Some(Modal::Text { title, buf, target }),
            },
        }
    }

    fn provider_key(&mut self, k: KeyEvent, mut f: ProvForm) -> Option<Modal> {
        match k.code {
            KeyCode::Esc => return None,
            KeyCode::Tab | KeyCode::Down => {
                f.focus = (f.focus + 1) % ProvForm::FIELDS;
            }
            KeyCode::BackTab | KeyCode::Up => {
                f.focus = (f.focus + ProvForm::FIELDS - 1) % ProvForm::FIELDS;
            }
            KeyCode::Left | KeyCode::Right if f.focus == 1 => {
                let i = ProvType::ALL.iter().position(|t| *t == f.ptype).unwrap();
                let d = if k.code == KeyCode::Right { 1 } else { ProvType::ALL.len() - 1 };
                f.ptype = ProvType::ALL[(i + d) % ProvType::ALL.len()];
                if f.base_url.text().is_empty()
                    || ProvType::ALL.iter().any(|t| f.base_url.text() == t.default_url())
                {
                    f.base_url.set(f.ptype.default_url());
                    f.key_env.set(f.ptype.default_env());
                }
                f.refresh_endpoint();
            }
            KeyCode::Left | KeyCode::Right if f.focus == 3 => {
                let i = AuthMode::ALL.iter().position(|t| *t == f.auth).unwrap();
                let d = if k.code == KeyCode::Right { 1 } else { AuthMode::ALL.len() - 1 };
                f.auth = AuthMode::ALL[(i + d) % AuthMode::ALL.len()];
            }
            KeyCode::Char(' ') if f.focus == 6 => f.remember = !f.remember,
            KeyCode::Enter => match f.focus {
                8 => {
                    // Test — one small live request
                    let name = f.name.text();
                    if !name.is_empty() {
                        self.status = "test sends one small live request".into();
                        self.effects.push(Effect::Probe {
                            name: name.clone(),
                            base_url: f.base_url.text(),
                            model: f.model.text(),
                            key: if f.auth == AuthMode::SessionKey {
                                Some(f.key.text())
                            } else {
                                std::env::var(f.key_env.text()).ok()
                            },
                        });
                    }
                }
                9 => {
                    if f.name.text().is_empty() {
                        f.status = "name required".into();
                    } else {
                        self.effects.push(Effect::SaveProfile {
                            name: f.name.text(),
                            base_url: f.base_url.text(),
                            model: f.model.text(),
                            key_env: if f.auth == AuthMode::EnvVar && !f.key_env.text().is_empty() {
                                Some(f.key_env.text())
                            } else { None },
                            key: if f.auth == AuthMode::SessionKey && !f.key.text().is_empty() {
                                Some(f.key.text())
                            } else { None },
                            remember: f.remember,
                        });
                        self.screen = Screen::Main;
                        return None;
                    }
                }
                10 => return None,
                7 => {
                    // model field → catalog picker (manual = filter text)
                    let key = if f.auth == AuthMode::SessionKey {
                        Some(f.key.text())
                    } else {
                        std::env::var(f.key_env.text()).ok()
                    };
                    self.effects.push(Effect::FetchModels {
                        base_url: f.base_url.text(),
                        key,
                        target: PickTarget::ProvModel,
                    });
                    self.form_stash = Some(f);
                    return Some(Modal::Picker(Picker {
                        title: "models (type to filter; Enter picks filter text if no match)".into(),
                        items: vec![],
                        filter: Buf::new(),
                        sel: 0,
                        target: PickTarget::ProvModel,
                        loading: true,
                    }));
                }
                _ => {}
            },
            KeyCode::Backspace => {
                if let Some(b) = f.cur() { b.backspace(); }
                f.refresh_endpoint();
            }
            KeyCode::Delete => {
                if let Some(b) = f.cur() { b.delete(); }
                f.refresh_endpoint();
            }
            KeyCode::Left => {
                if let Some(b) = f.cur() { b.left(); }
            }
            KeyCode::Right => {
                if let Some(b) = f.cur() { b.right(); }
            }
            KeyCode::Char(c) => {
                if let Some(b) = f.cur() {
                    b.insert(c);
                    f.refresh_endpoint();
                }
            }
            _ => {}
        }
        Some(Modal::Provider(f))
    }

    fn picker_key(&mut self, k: KeyEvent, mut p: Picker) -> Option<Modal> {
        match k.code {
            KeyCode::Esc => {
                // back to provider form if one was stashed
                return match (p.target.clone(), self.form_stash.take()) {
                    (PickTarget::ProvModel, Some(f)) => Some(Modal::Provider(f)),
                    _ => None,
                };
            }
            KeyCode::Up => p.sel = p.sel.saturating_sub(1),
            KeyCode::Down => {
                let n = self.filtered(&p).len();
                if p.sel + 1 < n { p.sel += 1; }
            }
            KeyCode::Backspace => { p.filter.backspace(); p.sel = 0; }
            KeyCode::Char(c) => { p.filter.insert(c); p.sel = 0; }
            KeyCode::Enter => {
                let list = self.filtered(&p);
                let choice = list.get(p.sel).cloned().or_else(|| {
                    let t = p.filter.text();
                    if t.is_empty() { None } else { Some(t) }
                });
                if let Some(c) = choice {
                    return self.pick(p.target.clone(), c);
                }
            }
            _ => {}
        }
        Some(Modal::Picker(p))
    }

    fn filtered(&self, p: &Picker) -> Vec<String> {
        let f = p.filter.text().to_lowercase();
        p.items
            .iter()
            .filter(|i| f.is_empty() || i.to_lowercase().contains(&f))
            .cloned()
            .collect()
    }

    fn pick(&mut self, target: PickTarget, choice: String) -> Option<Modal> {
        match target {
            PickTarget::ProvModel => {
                let mut f = self.form_stash.take().unwrap_or_else(|| ProvForm::new(ProvType::Custom));
                f.model.set(&choice);
                f.refresh_endpoint();
                Some(Modal::Provider(f))
            }
            PickTarget::Role(r) => {
                if r == Role::Auditor && choice == "(same as orchestrator)" {
                    self.ui.auditor_profile = None;
                    self.effects.push(Effect::SaveUi);
                    self.status = "auditor follows orchestrator".into();
                    return None;
                }
                // profile chosen → fetch its models for a second pick
                if let Some((base, key, _)) = self.resolve(&choice) {
                    let prof = choice.clone();
                    self.set_role_profile(r, choice.clone());
                    self.effects.push(Effect::FetchModels {
                        base_url: base,
                        key,
                        target: PickTarget::ModelForRole(prof.clone()),
                    });
                    return Some(Modal::Picker(Picker {
                        title: format!("model for {} (Enter on filter text = manual)", r.name()),
                        items: vec![],
                        filter: Buf::new(),
                        sel: 0,
                        target: PickTarget::ModelForRole(prof),
                        loading: true,
                    }));
                }
                None
            }
            PickTarget::ModelForRole(prof) => {
                // set model inside the stored profile config
                if let Some(p) = self.profiles.get_mut(&prof) {
                    p.model = Some(choice.clone());
                }
                self.status = format!("{prof} → {choice}");
                self.effects.push(Effect::SaveUi);
                None
            }
        }
    }

    fn set_role_profile(&mut self, r: Role, name: String) {
        match r {
            Role::Solo => self.ui.solo_profile = Some(name),
            Role::Orchestrator => self.ui.orchestrator_profile = Some(name),
            Role::Worker => self.ui.worker_profile = Some(name),
            Role::Auditor => self.ui.auditor_profile = Some(name),
        }
    }

    /// Called by the loop when a model fetch resolves.
    pub fn models_loaded(&mut self, models: Vec<crate::provider::ModelInfo>, err: Option<String>) {
        if let Some(Modal::Picker(p)) = &mut self.modal {
            p.loading = false;
            match err {
                Some(e) => {
                    p.items = vec![];
                    p.title = format!("catalog unavailable ({e}) — type model id, Enter to accept");
                }
                None => {
                    p.items = models
                        .iter()
                        .map(|m| {
                            let ctx = m.context_length.map(|c| format!(" ctx={}", c)).unwrap_or_default();
                            let tools = m.tools_claimed.map(|t| if t { " tools" } else { "" }).unwrap_or_default();
                            let price = match (m.price_in, m.price_out) {
                                (Some(a), Some(b)) => format!(" ${:.2}/${:.2}per-M", a * 1e6, b * 1e6),
                                _ => String::new(),
                            };
                            format!("{}{}{}{}", m.id, ctx, tools, price)
                        })
                        .collect();
                    if p.items.is_empty() {
                        p.title = "empty catalog — type model id, Enter to accept".into();
                    }
                }
            }
        }
    }

    pub fn probe_done(&mut self, name: &str, res: Result<Probe, String>) {
        match res {
            Ok(p) => {
                self.status = format!(
                    "{name}: stream={:?} tools={:?} usage={:?} (model {})",
                    p.streaming, p.tool_calls, p.usage, p.model
                );
            }
            Err(e) => self.status = format!("{name}: probe failed — {e}"),
        }
    }

    /// Settings rows (computed each draw so they stay in sync):
    /// providers first, then roles, then run options.
    pub fn settings_rows(&self) -> Vec<SettingsRow> {
        let mut v = vec![SettingsRow::AddProfile];
        for n in self.profiles.keys() {
            v.push(SettingsRow::EditProfile(n.clone()));
        }
        for r in Role::ALL {
            v.push(SettingsRow::Role(r));
        }
        v.push(SettingsRow::Workers);
        v.push(SettingsRow::Workspace);
        v.push(SettingsRow::Acceptance);
        v
    }

    /// Settings-tab row activation.
    pub fn settings_activate(&mut self, row: usize) {
        match self.settings_rows().get(row).cloned() {
            Some(SettingsRow::AddProfile) => {
                self.modal = Some(Modal::Provider(ProvForm::new(ProvType::DeepSeek)))
            }
            Some(SettingsRow::EditProfile(name)) => {
                if let Some(p) = self.profiles.get(&name).cloned() {
                    self.modal = Some(Modal::Provider(ProvForm::from_existing(&name, &p)));
                }
            }
            Some(SettingsRow::Role(role)) => {
                let mut items: Vec<String> = self.profiles.keys().cloned().collect();
                if role == Role::Auditor {
                    items.insert(0, "(same as orchestrator)".into());
                }
                self.modal = Some(Modal::Picker(Picker {
                    title: format!("profile for {}", role.name()),
                    items,
                    filter: Buf::new(),
                    sel: 0,
                    target: PickTarget::Role(role),
                    loading: false,
                }));
            }
            Some(SettingsRow::Workers) => {
                self.ui.worker_count = Some(match self.ui.worker_count {
                    Some(2) => 1,
                    _ => 2,
                });
                self.effects.push(Effect::SaveUi);
            }
            Some(SettingsRow::Workspace) => {
                self.modal = Some(Modal::Text {
                    title: "workspace path".into(),
                    buf: Buf::from(&self.workspace.to_string_lossy()),
                    target: TextTarget::Workspace,
                });
            }
            Some(SettingsRow::Acceptance) => {
                self.modal = Some(Modal::Text {
                    title: "external acceptance command (added to list)".into(),
                    buf: Buf::new(),
                    target: TextTarget::Acceptance,
                });
            }
            None => {}
        }
    }
}

#[derive(Clone)]
pub enum SettingsRow {
    AddProfile,
    EditProfile(String),
    Role(Role),
    Workers,
    Workspace,
    Acceptance,
}
