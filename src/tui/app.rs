//! TUI application state — deliberately terminal-free so tests can drive
//! it headless. `key()` translates input to state changes + Effects;
//! `apply_event()` folds core UiEvents into view state. mod.rs executes
//! Effects against the real runtime.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
use tokio::sync::mpsc::UnboundedSender;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

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

/// Custom-endpoint auth choice. Known providers never see this — they get
/// API-key + store only, with the conventional env var as invisible fallback.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AuthMode {
    None,
    ApiKey,
    /// Headless/CI path: read the key from an environment variable.
    Advanced,
}
impl AuthMode {
    pub const ALL: [AuthMode; 3] = [AuthMode::None, AuthMode::ApiKey, AuthMode::Advanced];
    pub fn name(self) -> &'static str {
        ["None", "API Key", "Advanced…"][self as usize]
    }
}

/// Where a typed API key lives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Store {
    Keychain,
    /// Plaintext api_key in ~/.config/sui/config.toml — the only durable
    /// choice on headless boxes where no OS keyring exists.
    ConfigFile,
    Session,
}
impl Store {
    pub const ALL: [Store; 3] = [Store::Keychain, Store::ConfigFile, Store::Session];
    pub fn name(self) -> &'static str {
        ["OS Keychain", "Config file (plaintext)", "Session only"][self as usize]
    }
}

/// Rows of the provider form — computed per provider type + auth mode so
/// irrelevant rows disappear entirely instead of rendering disabled.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Field {
    Name,
    BaseUrl,
    Model,
    Auth,
    CredSrc, // Advanced: credential source (env var only today)
    KeyEnv,  // Advanced: variable name
    ApiKey,
    Store,
    Test,
    Save,
    Cancel,
}
impl Field {
    pub fn label(self) -> &'static str {
        match self {
            Field::Name => "Profile name",
            Field::BaseUrl => "Base URL",
            Field::Model => "Model",
            Field::Auth => "Authentication",
            Field::CredSrc => "Credential source",
            Field::KeyEnv => "Variable name",
            Field::ApiKey => "API Key",
            Field::Store => "Store",
            Field::Test | Field::Save | Field::Cancel => "",
        }
    }
}

/// Provider editor form — dynamic row set, masked key, selectors cycle
/// with ←/→/Space.
pub struct ProvForm {
    pub name: Buf,
    pub ptype: ProvType,
    pub base_url: Buf,
    pub auth: AuthMode, // custom only
    pub key_env: Buf,
    pub key: Buf, // masked in the view
    pub store: Store,
    pub model: Buf,
    pub focus: usize, // index into fields()
    pub editing: Option<String>, // original name when editing existing
    pub endpoint: String,        // preview of the real request destination
    pub status: String,
}

impl ProvForm {
    pub fn new(ptype: ProvType) -> Self {
        let mut f = Self {
            name: Buf::from(&ptype.name().to_lowercase()),
            ptype,
            base_url: Buf::from(ptype.default_url()),
            auth: AuthMode::None,
            key_env: Buf::from(ptype.default_env()),
            key: Buf::new(),
            store: Store::Keychain,
            model: Buf::new(),
            focus: 0,
            editing: None,
            endpoint: String::new(),
            status: String::new(),
        };
        f.refresh_endpoint();
        f
    }
    /// Rebuild the form for an existing profile. `has_stored_key` tells the
    /// form whether a keyring/session key exists for it.
    pub fn from_existing(name: &str, p: &ProfileCfg, has_stored_key: bool) -> Self {
        let base = p.base_url.as_deref().unwrap_or("");
        let ptype = if base == ProvType::DeepSeek.default_url() {
            ProvType::DeepSeek
        } else if base == ProvType::OpenRouter.default_url() {
            ProvType::OpenRouter
        } else {
            ProvType::Custom
        };
        let mut f = Self::new(ptype);
        f.name.set(name);
        f.base_url.set(base);
        f.model.set(p.model.as_deref().unwrap_or(""));
        if ptype == ProvType::Custom {
            f.auth = if let Some(env) = &p.key_env {
                f.key_env.set(env);
                AuthMode::Advanced
            } else if has_stored_key || p.api_key.is_some() {
                AuthMode::ApiKey
            } else {
                AuthMode::None
            };
        }
        f.editing = Some(name.to_string());
        f.refresh_endpoint();
        f
    }
    pub fn refresh_endpoint(&mut self) {
        self.endpoint = format!("{}/chat/completions", self.base_url.text().trim_end_matches('/'));
    }

    /// The visible row set for the current provider type + auth mode.
    pub fn fields(&self) -> Vec<Field> {
        let mut v = vec![Field::Name];
        match self.ptype {
            ProvType::DeepSeek | ProvType::OpenRouter => {
                v.extend([Field::ApiKey, Field::Store, Field::Model]);
            }
            ProvType::Custom => {
                v.extend([Field::BaseUrl, Field::Model, Field::Auth]);
                match self.auth {
                    AuthMode::ApiKey => v.extend([Field::ApiKey, Field::Store]),
                    AuthMode::Advanced => v.extend([Field::CredSrc, Field::KeyEnv]),
                    AuthMode::None => {}
                }
            }
        }
        v.extend([Field::Test, Field::Save, Field::Cancel]);
        v
    }
    pub fn cur_field(&self) -> Field {
        let fs = self.fields();
        fs[self.focus.min(fs.len() - 1)]
    }
    fn cur(&mut self) -> Option<&mut Buf> {
        match self.cur_field() {
            Field::Name => Some(&mut self.name),
            Field::BaseUrl => Some(&mut self.base_url),
            Field::KeyEnv => Some(&mut self.key_env),
            Field::ApiKey => Some(&mut self.key),
            Field::Model => Some(&mut self.model),
            _ => None,
        }
    }
    /// Selector rows cycle on ←/→/Space/Enter.
    fn cycle(&mut self, dir: isize) {
        match self.cur_field() {
            Field::Auth => {
                let i = AuthMode::ALL.iter().position(|a| *a == self.auth).unwrap();
                let n = AuthMode::ALL.len() as isize;
                self.auth = AuthMode::ALL[((i as isize + dir).rem_euclid(n)) as usize];
            }
            Field::Store => {
                let i = Store::ALL.iter().position(|s| *s == self.store).unwrap();
                let n = Store::ALL.len() as isize;
                self.store = Store::ALL[((i as isize + dir).rem_euclid(n)) as usize];
            }
            Field::CredSrc => {} // env var only, for now
            _ => {}
        }
    }
    /// Key the form would use for fetches/probes: typed key, else the
    /// provider's conventional env var, else the advanced env var.
    fn effective_key(&self) -> Option<String> {
        let typed = self.key.text();
        if !typed.is_empty() {
            return Some(typed);
        }
        match self.ptype {
            ProvType::Custom => match self.auth {
                AuthMode::Advanced => std::env::var(self.key_env.text()).ok(),
                _ => None,
            },
            _ => std::env::var(self.ptype.default_env()).ok(),
        }
    }
    /// Map the form onto SaveProfile inputs: (key_env to persist, typed
    /// key, chosen store). Known providers persist their conventional
    /// env-var name so headless/CLI env auth keeps working even though the
    /// form never shows it.
    fn save_inputs(&self) -> (Option<String>, Option<String>, Store) {
        let key = self.key.text();
        let key = if key.is_empty() { None } else { Some(key) };
        match self.ptype {
            ProvType::DeepSeek | ProvType::OpenRouter => (
                Some(self.ptype.default_env().to_string()),
                key,
                self.store,
            ),
            ProvType::Custom => match self.auth {
                AuthMode::None => (None, None, self.store),
                AuthMode::ApiKey => (None, key, self.store),
                AuthMode::Advanced => {
                    let env = self.key_env.text();
                    (if env.is_empty() { None } else { Some(env) }, None, self.store)
                }
            },
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
    NewProvider,          // provider type → open its form
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
    User { text: String, at: String },
    Assistant { agent: String, text: String, live: bool, at: String },
    Tool { agent: String, name: String, summary: String, done: bool, ok: bool, result: String, at: String },
    Sys { text: String, at: String },
}

/// Wall-clock HH:MM UTC label for chat items — dim, and honest about TZ.
pub fn now_hm() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("{:02}:{:02}Z", (secs / 3600) % 24, (secs / 60) % 60)
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
    Permission { id: u64, agent: String, summary: String, reply: UnboundedSender<GateChoice> },
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
    SaveProfile { name: String, base_url: String, model: String, key_env: Option<String>, key: Option<String>, store: Store },
    SaveUi,
    /// Export this session's journals to a sanitized report file.
    ExportRun,
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
    /// Session-scoped auto-approval flag, shared live with every spawned
    /// gate. Raised by [a] on a permission modal or the Settings toggle;
    /// cleared by the toggle, workspace change, and restart. Never
    /// persisted — every launch starts in Ask.
    pub auto: Arc<AtomicBool>,
    /// This TUI session's journal dir (~/.local/share/sui/runs/<id>) —
    /// the exportable run unit. None in tests.
    pub run_dir: Option<PathBuf>,
    /// Permission asks that arrived while one was already open — a
    /// mission can have several workers wanting approval at once.
    /// Replies stay parked here; nothing is denied by overwrite.
    pub pending_perms: std::collections::VecDeque<(u64, String, String, tokio::sync::mpsc::UnboundedSender<crate::events::GateChoice>)>,
    /// Submitted tasks, oldest first — Up recalls when input is empty.
    pub history: Vec<String>,
    pub hist_i: Option<usize>,
    /// Physical keys currently held (Press seen, no Release yet). Used to
    /// deduplicate permission decisions: a Release only counts as a
    /// decision when no matching Press is outstanding — so on terminals
    /// reporting Press+Release, one physical keypress can never approve
    /// two consecutive prompts. Release-only transports (no Press ever
    /// seen) still work.
    held: std::collections::HashSet<KeyCode>,
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
        // authoritative probe: NoEntry on reads proves nothing — a write
        // must round-trip before we call the keyring usable (headless
        // boxes report NoEntry forever and would silently drop keys).
        // Once per process: on a dbus-less box each secret-service call
        // can block tens of seconds, and probing per App would hang CI.
        static KEYRING_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        let keyring_ok = *KEYRING_OK.get_or_init(|| {
            // linux secret-service needs a session bus; without one the
            // dbus connect can block for a very long time on headless CI —
            // no probe at all, the verdict is already "unusable"
            #[cfg(target_os = "linux")]
            if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
                && std::env::var_os("XDG_RUNTIME_DIR").is_none()
            {
                return false;
            }
            // hard timeout: a wedged secret-service call must never stall
            // startup; the probe thread may linger blocked, that's fine
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(
                    keyring::Entry::new("sui", "__probe__")
                        .and_then(|e| {
                            e.set_password("x")?;
                            let _ = e.delete_credential();
                            Ok(())
                        })
                        .is_ok(),
                );
            });
            rx.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap_or(false)
        });
        let mut session_keys = BTreeMap::new();
        if keyring_ok {
            for name in profiles.keys() {
                match keyring::Entry::new("sui", name).and_then(|e| e.get_password()) {
                    Ok(k) => {
                        session_keys.insert(name.clone(), k);
                    }
                    Err(_) => {}
                }
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
            chat: vec![ChatItem::Sys { text: "welcome — configure a provider (Settings → add), pick models per role, then type a task".into(), at: now_hm() }],
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
                Some(Modal::Picker(Picker {
                    title: "add a provider".into(),
                    items: ProvType::ALL.iter().map(|t| t.name().to_string()).collect(),
                    filter: Buf::new(),
                    sel: 0,
                    target: PickTarget::NewProvider,
                    loading: false,
                }))
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
            auto: Arc::new(AtomicBool::new(false)),
            run_dir: None,
            pending_perms: Default::default(),
            history: Vec::new(),
            hist_i: None,
            held: std::collections::HashSet::new(),
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
                    self.chat.push(ChatItem::Assistant { agent, text, live: true, at: now_hm() });
                }
            }
            UiEvent::ToolStart { agent, name, summary } => {
                self.chat.push(ChatItem::Tool {
                    agent, name, summary, done: false, ok: false, result: String::new(), at: now_hm(),
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
            UiEvent::Permission { id, agent, summary, reply } => {
                // never overwrite an open prompt — the dropped reply
                // channel would silently deny the parked request
                if matches!(self.modal, Some(Modal::Permission { .. })) {
                    self.pending_perms.push_back((id, agent, summary, reply));
                } else {
                    self.modal = Some(Modal::Permission { id, agent, summary, reply });
                }
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
                self.chat.push(ChatItem::Sys { text: format!("run finished: {outcome}"), at: now_hm() });
            }
            UiEvent::Error { agent, msg } => {
                self.chat.push(ChatItem::Sys { text: format!("error [{agent}]: {msg}"), at: now_hm() });
            }
        }
    }

    // ── input handling → effects ────────────────────────────────────
    pub fn key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        // Track physical key state before any dispatch: a Release is only a
        // fresh decision when no matching Press is outstanding.
        let held_press = self.held.contains(&k.code);
        match k.kind {
            KeyEventKind::Press => {
                self.held.insert(k.code);
            }
            KeyEventKind::Release => {
                self.held.remove(&k.code);
            }
            _ => {}
        }
        let press = k.kind == KeyEventKind::Press;
        // Global chords preempt modal input — Ctrl+Q always quits, Ctrl+S
        // stops a running task even while a permission modal is parked.
        // Accepted on any event kind: on transports that only deliver
        // Release-kind events these are still the user's way out.
        if ctrl && k.code == KeyCode::Char('q') {
            self.effects.push(Effect::Quit);
            return;
        }
        if ctrl && k.code == KeyCode::Char('s') {
            self.stop();
            return;
        }
        if let Some(m) = self.modal.take() {
            // Permission shortcuts: Press decides. A Release decides only
            // when no matching Press was seen (release-only transports) —
            // a Press+Release pair is ONE keypress and must not approve two
            // consecutive prompts. Repeat never decides. Everywhere else
            // non-Press kinds are dropped so those same terminals don't
            // double-type or leak decision keys into the chat input.
            let usable = if matches!(m, Modal::Permission { .. }) {
                match k.kind {
                    KeyEventKind::Press => true,
                    KeyEventKind::Release => !held_press,
                    _ => false,
                }
            } else {
                press
            };
            if usable {
                // handlers consume the modal and return the next state —
                // Some(m) stays open, a different Some replaces, None closes
                self.modal = self.modal_key(k, m);
            } else {
                self.modal = Some(m);
            }
            return;
        }
        if !press {
            return;
        }
        match (ctrl, k.code) {
            (true, KeyCode::Char('t')) => {
                self.tab = Tab::ALL[((self.tab as usize) + 1) % 5]
            }
            (true, KeyCode::Char('b')) => self.sidebar = !self.sidebar,
            // Ctrl+M is byte 0x0D == Enter in most terminals; Ctrl+O (0x0F)
            // is the portable chord. 'm' stays for kitty/CSI-u keyboards.
            (true, KeyCode::Char('m')) | (true, KeyCode::Char('o')) => self.toggle_mode(),
            // NB: Ctrl+J is 0x0A = Enter on legacy terminals — binding it
            // would submit the task instead of inserting a newline.
            (true, KeyCode::Char('n')) => self.input.insert('\n'),
            (_, KeyCode::F(1)) => self.modal = Some(Modal::Help),
            (_, KeyCode::PageUp) => self.scroll = self.scroll.saturating_add(10),
            (_, KeyCode::PageDown) => self.scroll = self.scroll.saturating_sub(10),
            (_, KeyCode::Enter) => {
                if self.tab == Tab::Chat && !self.input.is_empty() && self.running {
                    self.status = "run in progress — Ctrl+S stops it; text kept".into();
                } else if self.tab == Tab::Chat && !self.input.is_empty() && !self.running {
                    let task = self.input.text();
                    if task.starts_with('/') {
                        match task.as_str() {
                            "/mission" => {
                                self.set_mode(Mode::Mission);
                                self.input.clear();
                            }
                            "/solo" => {
                                self.set_mode(Mode::Solo);
                                self.input.clear();
                            }
                            "/export" => {
                                self.effects.push(Effect::ExportRun);
                                self.input.clear();
                            }
                            "/help" => {
                                self.modal = Some(Modal::Help);
                                self.input.clear();
                            }
                            _ => {
                                self.status = format!("unknown command '{task}' — /mission /solo /export /help");
                            }
                        }
                        return;
                    }
                    self.input.clear();
                    self.chat.push(ChatItem::User { text: task.clone(), at: now_hm() });
                    self.history.push(task.clone());
                    if self.history.len() > 200 { self.history.remove(0); }
                    self.hist_i = None;
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
                } else if self.tab == Tab::Chat
                    && (self.input.is_empty() || self.hist_i.is_some())
                    && !self.history.is_empty()
                {
                    let i = self
                        .hist_i
                        .map(|i| i.saturating_sub(1))
                        .unwrap_or(self.history.len() - 1);
                    self.hist_i = Some(i);
                    self.input.set(&self.history[i]);
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
                } else if self.tab == Tab::Chat && self.hist_i.is_some() {
                    let i = self.hist_i.unwrap() + 1;
                    if i >= self.history.len() {
                        self.hist_i = None;
                        self.input.clear();
                    } else {
                        self.hist_i = Some(i);
                        self.input.set(&self.history[i]);
                    }
                } else {
                    self.scroll = self.scroll.saturating_sub(1);
                }
            }
            (_, KeyCode::Esc) => {
                if self.tab == Tab::Settings {
                    self.tab = Tab::Chat;
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
                if self.tab == Tab::Chat { self.input.insert(c); self.hist_i = None; }
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

    /// Flip solo ↔ mission and persist the choice so the next launch
    /// remembers it (ui.mode in config.toml).
    pub fn toggle_mode(&mut self) {
        self.mode = match self.mode {
            Mode::Solo => Mode::Mission,
            Mode::Mission => Mode::Solo,
        };
        self.ui.mode = Some(match self.mode {
            Mode::Solo => "solo".into(),
            Mode::Mission => "mission".into(),
        });
        self.effects.push(Effect::SaveUi);
        self.status = match self.mode {
            Mode::Solo => "mode: solo (one worker)".into(),
            Mode::Mission => "mode: mission (orchestrator → workers → auditor)".into(),
        };
    }
    pub fn set_mode(&mut self, m: Mode) {
        if self.mode != m {
            self.toggle_mode();
        } else {
            self.status = match m {
                Mode::Solo => "already solo".into(),
                Mode::Mission => "already mission".into(),
            };
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
            Modal::Permission { id, agent, summary, reply } => {
                let decided = match k.code {
                    KeyCode::Char('y') | KeyCode::Char('Y') => Some(GateChoice::Once),
                    KeyCode::Char('a') | KeyCode::Char('A') => Some(GateChoice::Session),
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        Some(GateChoice::Deny)
                    }
                    // Enter does nothing: the modal has no selected action
                    // to confirm — approval must never be granted silently.
                    _ => None,
                };
                match decided {
                    None => Some(Modal::Permission { id, agent, summary, reply }),
                    Some(c) => {
                        if c == GateChoice::Session {
                            self.auto.store(true, std::sync::atomic::Ordering::Relaxed);
                        }
                        let _ = reply.send(c);
                        // next parked ask becomes the modal
                        self.pending_perms.pop_front().map(|(id, agent, summary, reply)| {
                            Modal::Permission { id, agent, summary, reply }
                        })
                    }
                }
            }
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
                            // new workspace → approvals default back to Ask
                            self.auto.store(false, std::sync::atomic::Ordering::Relaxed);
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
        let n = f.fields().len();
        match k.code {
            KeyCode::Esc => return None,
            KeyCode::Tab | KeyCode::Down => f.focus = (f.focus + 1) % n,
            KeyCode::BackTab | KeyCode::Up => f.focus = (f.focus + n - 1) % n,
            KeyCode::Left | KeyCode::Right => {
                if f.cur().is_some() {
                    match k.code {
                        KeyCode::Left => f.cur().unwrap().left(),
                        _ => f.cur().unwrap().right(),
                    }
                } else {
                    f.cycle(if k.code == KeyCode::Right { 1 } else { -1 });
                }
            }
            KeyCode::Char(' ') if f.cur().is_none() => f.cycle(1),
            KeyCode::Enter => match f.cur_field() {
                Field::Test => {
                    let name = f.name.text();
                    if !name.is_empty() {
                        self.status = "test sends one small live request".into();
                        self.effects.push(Effect::Probe {
                            name,
                            base_url: f.base_url.text(),
                            model: f.model.text(),
                            key: f.effective_key(),
                        });
                    }
                }
                Field::Save => {
                    if f.name.text().is_empty() {
                        f.status = "name required".into();
                    } else {
                        let (key_env, key, store) = f.save_inputs();
                        self.effects.push(Effect::SaveProfile {
                            name: f.name.text(),
                            base_url: f.base_url.text(),
                            model: f.model.text(),
                            key_env,
                            key,
                            store,
                        });
                        self.screen = Screen::Main;
                        return None;
                    }
                }
                Field::Cancel => return None,
                Field::Model => {
                    // catalog picker; filter text doubles as manual entry
                    self.effects.push(Effect::FetchModels {
                        base_url: f.base_url.text(),
                        key: f.effective_key(),
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
                _ => f.cycle(1), // selectors advance on Enter too
            },
            KeyCode::Backspace => {
                if let Some(b) = f.cur() { b.backspace(); }
                f.refresh_endpoint();
            }
            KeyCode::Delete => {
                if let Some(b) = f.cur() { b.delete(); }
                f.refresh_endpoint();
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
            PickTarget::NewProvider => {
                let ptype = ProvType::ALL
                    .iter()
                    .find(|t| t.name() == choice)
                    .copied()
                    .unwrap_or(ProvType::Custom);
                let mut f = ProvForm::new(ptype);
                // headless: no working keyring → default to durable storage
                if !self.keyring_ok {
                    f.store = Store::ConfigFile;
                }
                Some(Modal::Provider(f))
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
        v.push(SettingsRow::Mode);
        v.push(SettingsRow::Export);
        v.push(SettingsRow::Workers);
        v.push(SettingsRow::Auto);
        v.push(SettingsRow::Workspace);
        v.push(SettingsRow::Acceptance);
        v
    }

    /// Settings-tab row activation.
    pub fn settings_activate(&mut self, row: usize) {
        match self.settings_rows().get(row).cloned() {
            Some(SettingsRow::AddProfile) => {
                self.modal = Some(Modal::Picker(Picker {
                    title: "add a provider".into(),
                    items: ProvType::ALL.iter().map(|t| t.name().to_string()).collect(),
                    filter: Buf::new(),
                    sel: 0,
                    target: PickTarget::NewProvider,
                    loading: false,
                }));
            }
            Some(SettingsRow::EditProfile(name)) => {
                if let Some(p) = self.profiles.get(&name).cloned() {
                    let has_key = self.session_keys.contains_key(&name) || p.api_key.is_some();
                    let mut f = ProvForm::from_existing(&name, &p, has_key);
                    if !self.keyring_ok {
                        f.store = Store::ConfigFile;
                    }
                    self.modal = Some(Modal::Provider(f));
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
            Some(SettingsRow::Mode) => self.toggle_mode(),
            Some(SettingsRow::Export) => {
                self.effects.push(Effect::ExportRun);
            }
            Some(SettingsRow::Workers) => {
                self.ui.worker_count = Some(match self.ui.worker_count {
                    Some(2) => 1,
                    _ => 2,
                });
                self.effects.push(Effect::SaveUi);
            }
            Some(SettingsRow::Auto) => {
                // session-scoped YOLO toggle — flips the flag the live gate
                // already watches, so Ask→Auto→Ask lands on the very next
                // tool dispatch without touching the agent or its history
                let on = !self.auto.load(std::sync::atomic::Ordering::Relaxed);
                self.auto.store(on, std::sync::atomic::Ordering::Relaxed);
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
    Mode,
    Export,
    Workers,
    Auto,
    Workspace,
    Acceptance,
}
