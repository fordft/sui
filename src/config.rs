use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

/// Provider transport is trusted configuration, independent of agent-loop policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Transport {
    #[default]
    ChatCompletions,
    OpenaiResponses,
    CodexOauth,
    Copilot,
    Ollama,
}

impl Transport {
    pub(crate) fn from_kind(kind: Option<&str>) -> Result<Self> {
        match kind {
            None | Some("chat-completions") => Ok(Self::ChatCompletions),
            Some("openai-responses") => Ok(Self::OpenaiResponses),
            Some("codex-oauth") => Ok(Self::CodexOauth),
            Some("copilot") => Ok(Self::Copilot),
            Some("ollama") => Ok(Self::Ollama),
            Some(other) => bail!("unsupported provider kind: {other}"),
        }
    }

    pub fn is_account(self) -> bool {
        matches!(self, Self::CodexOauth | Self::Copilot)
    }

    pub fn default_url(self) -> &'static str {
        match self {
            Self::CodexOauth => "codex://oauth",
            Self::Copilot => "copilot://oauth",
            Self::Ollama => "http://127.0.0.1:11434/v1",
            Self::ChatCompletions | Self::OpenaiResponses => "https://api.openai.com/v1",
        }
    }

    pub fn default_model(self) -> String {
        match self {
            Self::CodexOauth => crate::codex::CodexAuth::cli_default_model()
                .unwrap_or_else(|| "gpt-5.3-codex".into()),
            Self::Copilot => "gpt-4.1".into(),
            Self::Ollama => String::new(),
            Self::ChatCompletions | Self::OpenaiResponses => "gpt-5".into(),
        }
    }

    pub fn default_image_input(self) -> bool {
        self == Self::CodexOauth
    }

    pub fn resolve_url(self, configured: Option<&str>) -> String {
        if self.is_account() {
            self.default_url().into()
        } else {
            norm_url(configured.unwrap_or(self.default_url()))
        }
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub transport: Transport,
    pub base_url: String,
    /// None = no key configured. Never sourced from project-level sui.toml.
    pub api_key: Option<String>,
    pub model: String,
    pub image_input: bool,
    pub prompt_cache_key: Option<String>,
    pub workspace: PathBuf,
    pub run_dir: PathBuf,
    pub session_id: String,
    pub auto_approve: bool,
    pub max_turns: usize,
    pub bash_timeout_ms: u64,
    pub bash_timeout_max_ms: u64,
    pub request_timeout_ms: u64,
    /// Conservative context guard: estimated input tokens (chars/4) plus
    /// reserved completion capacity above this stops the turn cleanly.
    pub context_token_budget: usize,
    pub context_reserve_tokens: usize,
    pub context_compaction: bool,
}

/// A named provider profile. Credentials live in the user's own config or
/// environment — profiles may ONLY be defined in global/--config files,
/// never in project sui.toml.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ProfileCfg {
    pub base_url: Option<String>,
    pub model: Option<String>,
    /// Explicit model capability; absent defaults true for native Codex
    /// and false for other transports.
    pub image_input: Option<bool>,
    /// Selects a native wire adapter or account transport; absent selects
    /// standard Chat Completions. See sui.example.toml for all kinds.
    pub kind: Option<String>,
    /// Name of the env var holding this profile's API key.
    pub key_env: Option<String>,
    /// Inline key allowed only in user-owned (global/--config) files.
    pub api_key: Option<String>,
    pub prompt_cache_key: Option<String>,
    /// USD per million tokens, for certification cost estimates.
    pub pricing: Option<PricingCfg>,
}

#[derive(Debug, Deserialize, Default, Clone)]
pub struct PricingCfg {
    pub input: Option<f64>,
    pub cached: Option<f64>,
    pub cache_write: Option<f64>,
    pub output: Option<f64>,
}

impl PricingCfg {
    /// Total input includes both cache reads and writes. Missing buckets or
    /// prices stay unknown; a zero-sized bucket needs no configured price.
    pub fn estimate(&self, usage: &crate::types::Usage) -> Option<f64> {
        if !usage.complete || usage.estimated {
            return None;
        }
        let input = usage.input_tokens?;
        let read = usage.cache_read_tokens?;
        let write = usage.cache_write_tokens?;
        let ordinary = input.checked_sub(read.checked_add(write)?)?;
        let price = |tokens: u64, rate: Option<f64>| -> Option<f64> {
            if tokens == 0 {
                return Some(0.0);
            }
            let rate = rate.filter(|r| r.is_finite() && *r >= 0.0)?;
            Some(tokens as f64 * rate / 1e6)
        };
        Some(
            price(ordinary, self.input)?
                + price(read, self.cached)?
                + price(write, self.cache_write)?
                + price(usage.output_tokens?, self.output)?,
        )
    }
}

#[derive(Debug, Clone)]
pub struct Profile {
    pub transport: Transport,
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub image_input: bool,
    pub api_key: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub pricing: Option<PricingCfg>,
}

impl Profile {
    /// Availability is a preflight hint, not proof of valid credentials.
    /// The actual request and mission gates own verification.
    pub fn credentials_available(&self) -> bool {
        match self.transport {
            Transport::CodexOauth => crate::codex::CodexAuth::session_exists(),
            Transport::Copilot => crate::auth::LoginProvider::Copilot.session_exists(),
            Transport::Ollama => true,
            _ => {
                self.api_key.is_some()
                    || reqwest::Url::parse(&self.base_url).is_ok_and(|url| {
                        url.host_str().is_some_and(|h| {
                            h == "localhost"
                                || h.parse::<std::net::IpAddr>()
                                    .is_ok_and(|ip| ip.is_loopback())
                        })
                    })
            }
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    provider: Option<ProviderCfg>,
    agent: Option<AgentCfg>,
    profiles: Option<BTreeMap<String, ProfileCfg>>,
}

#[derive(Debug, Deserialize, Default)]
struct ProviderCfg {
    kind: Option<String>,
    image_input: Option<bool>,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    prompt_cache_key: Option<String>,
}

#[derive(Debug, Deserialize, Default)]
struct AgentCfg {
    max_turns: Option<usize>,
    bash_timeout_ms: Option<u64>,
    bash_timeout_max_ms: Option<u64>,
    auto_approve: Option<bool>,
    request_timeout_ms: Option<u64>,
    context_token_budget: Option<usize>,
    context_reserve_tokens: Option<usize>,
    context_compaction: Option<bool>,
}

/// Resolved `[agent]` limits — the values every launcher sources
/// instead of re-typing literals.
#[derive(Debug, Clone)]
pub struct AgentLimits {
    pub max_turns: usize,
    pub bash_timeout_ms: u64,
    pub bash_timeout_max_ms: u64,
    pub request_timeout_ms: u64,
    pub context_token_budget: usize,
    pub context_reserve_tokens: usize,
    pub context_compaction: bool,
}

/// flagged --config > project sui.toml > global config > defaults —
/// the single resolution `load` and `agent_limits` both use.
fn resolve_limits(fa: &AgentCfg, pa: &AgentCfg, ga: &AgentCfg) -> AgentLimits {
    AgentLimits {
        max_turns: fa.max_turns.or(pa.max_turns).or(ga.max_turns).unwrap_or(60),
        bash_timeout_ms: fa
            .bash_timeout_ms
            .or(pa.bash_timeout_ms)
            .or(ga.bash_timeout_ms)
            .unwrap_or(120_000),
        bash_timeout_max_ms: fa
            .bash_timeout_max_ms
            .or(pa.bash_timeout_max_ms)
            .or(ga.bash_timeout_max_ms)
            .unwrap_or(600_000),
        request_timeout_ms: fa
            .request_timeout_ms
            .or(pa.request_timeout_ms)
            .or(ga.request_timeout_ms)
            .unwrap_or(300_000),
        context_token_budget: fa
            .context_token_budget
            .or(pa.context_token_budget)
            .or(ga.context_token_budget)
            .unwrap_or(120_000),
        context_compaction: fa
            .context_compaction
            .or(pa.context_compaction)
            .or(ga.context_compaction)
            .unwrap_or(true),
        context_reserve_tokens: fa
            .context_reserve_tokens
            .or(pa.context_reserve_tokens)
            .or(ga.context_reserve_tokens)
            .unwrap_or(8_192),
    }
}

/// `[agent]` limits for a workspace — project sui.toml over global
/// config, the same precedence `load` uses minus a --config flag file
/// (launchers that don't take one). Read/parse failures default, the
/// same way load_ui does.
pub fn agent_limits(workspace: &Path) -> AgentLimits {
    let ga = global_cfg_path()
        .filter(|p| p.exists())
        .and_then(|p| read_toml(&p).ok())
        .and_then(|f| f.agent)
        .unwrap_or_default();
    let project_path = workspace.join("sui.toml");
    let pa = if project_path.exists() {
        read_toml(&project_path)
            .ok()
            .and_then(|f| f.agent)
            .unwrap_or_default()
    } else {
        AgentCfg::default()
    };
    resolve_limits(&AgentCfg::default(), &pa, &ga)
}

pub struct Overrides {
    pub base_url: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub auto_approve: bool,
    pub workspace: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    /// True when no human can answer a prompt (one-shot mode, piped stdin,
    /// certification runs). Trust decisions become hard failures.
    pub non_interactive: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Source {
    Flag,
    Env,
    FlaggedConfig,
    Project,
    Global,
    Default,
}

fn norm_url(u: &str) -> String {
    u.trim().trim_end_matches('/').to_string()
}

fn global_cfg_path() -> Option<PathBuf> {
    std::env::var_os("SUI_HOME")
        .map(|h| PathBuf::from(h).join("config.toml"))
        .or_else(|| std::env::home_dir().map(|h| h.join(".config/sui/config.toml")))
}

fn read_toml(p: &Path) -> Result<FileConfig> {
    let s = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
    toml::from_str(&s).with_context(|| format!("parse {}", p.display()))
}

/// Reject removed executable-agent configuration before any native agent starts.
/// Read-only consumers, including historical exports, still use read_toml.
pub fn validate_native_config(config_path: Option<&Path>, workspace: &Path) -> Result<()> {
    validate_native_sources(config_path, Some(workspace))
}

fn validate_native_sources(config_path: Option<&Path>, workspace: Option<&Path>) -> Result<()> {
    let mut paths = Vec::new();
    if let Some(p) = global_cfg_path().filter(|p| p.exists()) {
        paths.push(p);
    }
    if let Some(p) = workspace.map(|w| w.join("sui.toml")).filter(|p| p.exists()) {
        paths.push(p);
    }
    if let Some(p) = config_path {
        paths.push(p.to_path_buf());
    }
    for p in paths {
        let contents =
            std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
        let doc: toml::Value = contents
            .parse()
            .with_context(|| format!("parse {}", p.display()))?;
        if doc.get("agents").is_some() {
            bail!("ACP agents have been removed; remove [agents] from {} and use native [profiles.<name>] profiles", p.display());
        }
        if let Some(ui) = doc.get("ui") {
            for role in [
                "solo_profile",
                "orchestrator_profile",
                "worker_profile",
                "auditor_profile",
            ] {
                if ui
                    .get(role)
                    .and_then(toml::Value::as_str)
                    .is_some_and(|name| name.starts_with("acp:"))
                {
                    bail!("ACP agents have been removed; replace [ui].{role} in {} with a native profile name", p.display());
                }
            }
        }
    }
    Ok(())
}

/// Profiles are read ONLY from user-owned files: ~/.config/sui/config.toml
/// and an explicit --config path. Project sui.toml profiles are ignored.
pub fn profiles(config_path: Option<&Path>) -> Result<BTreeMap<String, ProfileCfg>> {
    let mut out = BTreeMap::new();
    if let Some(g) = global_cfg_path().filter(|p| p.exists()) {
        if let Some(ps) = read_toml(&g)?.profiles {
            out.extend(ps);
        }
    }
    if let Some(p) = config_path {
        if let Some(ps) = read_toml(p)?.profiles {
            out.extend(ps); // explicit file wins
        }
    }
    inject_detected_profiles(&mut out);
    Ok(out)
}

/// A discoverable Codex OAuth session (Sui's own store or the Codex
/// CLI's ~/.codex/auth.json) IS a usable profile — registering it
/// automatically means `codex login` alone makes the `codex` profile
/// selectable everywhere (TUI pickers, --worker-profile, missions)
/// with zero TOML. An explicit [profiles.codex] always wins.
fn inject_detected_profiles(out: &mut BTreeMap<String, ProfileCfg>) {
    if crate::codex::CodexAuth::session_exists() {
        // Match what `codex` itself would run before falling back to a
        // generic default — the CLI's config.toml carries the model.
        let model = crate::codex::CodexAuth::cli_default_model()
            .unwrap_or_else(|| "gpt-5.3-codex".to_string());
        out.entry("codex".to_string()).or_insert(ProfileCfg {
            kind: Some("codex-oauth".to_string()),
            model: Some(model),
            ..Default::default()
        });
    }
    let provider = crate::auth::LoginProvider::Copilot;
    if provider.session_exists() {
        out.entry(provider.id().into()).or_insert(ProfileCfg {
            kind: Some(provider.kind().into()),
            model: Some(provider.transport().default_model()),
            ..Default::default()
        });
    }
}

/// The global `[provider]` credential (api_key inline or via key_env) —
/// used by export's known-secrets masking so the run's own key is
/// literal-masked wherever it leaked into a journal.
pub fn global_provider_key() -> Option<String> {
    let p = global_cfg_path().filter(|p| p.exists())?;
    let pc = read_toml(&p).ok()?.provider?;
    pc.api_key.filter(|k| !k.is_empty())
}

/// Resolve a named profile for certification. Key material comes from the
/// profile's key_env env var, or an inline api_key in the user-owned file.
pub fn resolve_profile(name: &str, config_path: Option<&Path>) -> Result<Profile> {
    if name.starts_with("acp:") {
        bail!("ACP agents have been removed; select a native [profiles.<name>] profile");
    }
    validate_native_sources(config_path, None)?;
    let all = profiles(config_path)?;
    let p = all.get(name).with_context(|| {
        format!(
            "unknown profile '{name}' (defined: {})",
            if all.is_empty() {
                "none — add [profiles.<name>] to ~/.config/sui/config.toml".into()
            } else {
                all.keys().cloned().collect::<Vec<_>>().join(", ")
            }
        )
    })?;
    let transport = Transport::from_kind(p.kind.as_deref())?;
    let api_key = p
        .key_env
        .as_deref()
        .and_then(|k| std::env::var(k).ok())
        .filter(|v| !v.is_empty())
        .or_else(|| p.api_key.clone());
    Ok(Profile {
        transport,
        name: name.to_string(),
        base_url: transport.resolve_url(p.base_url.as_deref()),
        model: p.model.clone().unwrap_or_else(|| transport.default_model()),
        image_input: p.image_input.unwrap_or(transport.default_image_input()),
        api_key: if transport.is_account() {
            None
        } else {
            api_key
        },
        prompt_cache_key: p.prompt_cache_key.clone(),
        pricing: if transport.is_account() {
            None
        } else {
            p.pricing.clone()
        },
    })
}

pub fn load(ov: Overrides) -> Result<Config> {
    let workspace = ov
        .workspace
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .context("workspace path does not exist")?;
    validate_native_config(ov.config_path.as_deref(), &workspace)?;

    let mut global = FileConfig::default();
    if let Some(g) = global_cfg_path().filter(|p| p.exists()) {
        global = read_toml(&g)?;
    }
    let project_path = workspace.join("sui.toml");
    let project = if project_path.exists() {
        read_toml(&project_path)?
    } else {
        FileConfig::default()
    };
    let flagged = match &ov.config_path {
        Some(p) => read_toml(p)?,
        None => FileConfig::default(),
    };

    if project
        .provider
        .as_ref()
        .and_then(|p| p.api_key.as_ref())
        .is_some()
    {
        eprintln!("warning: api_key in project sui.toml ignored (use env or global config)");
    }
    if project.profiles.is_some() {
        eprintln!("warning: [profiles] in project sui.toml ignored (profiles are global-only)");
    }
    if project
        .agent
        .as_ref()
        .and_then(|a| a.auto_approve)
        .unwrap_or(false)
    {
        eprintln!(
            "warning: auto_approve in project sui.toml ignored \
             (a repo must never grant itself unattended tool execution)"
        );
    }

    let gp = global.provider.unwrap_or_default();
    let pp = project.provider.unwrap_or_default();
    let fp = flagged.provider.unwrap_or_default();
    let ga = global.agent.unwrap_or_default();
    let pa = project.agent.unwrap_or_default();
    let fa = flagged.agent.unwrap_or_default();

    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());

    // Resolve base_url WITH its source so the trust gate knows whether
    // an untrusted repo file performed the redirect.
    let (base_url, base_src) = [
        (ov.base_url.clone(), Source::Flag),
        (env("SUI_BASE_URL"), Source::Env),
        (env("OPENAI_BASE_URL"), Source::Env),
        (fp.base_url.clone(), Source::FlaggedConfig),
        (pp.base_url.clone(), Source::Project),
        (gp.base_url.clone(), Source::Global),
    ]
    .into_iter()
    .find(|(v, _)| v.is_some())
    .map(|(v, s)| (v.unwrap(), s))
    .unwrap_or_else(|| {
        (
            Transport::from_kind(fp.kind.as_deref().or(gp.kind.as_deref()))
                .unwrap_or_default()
                .default_url()
                .into(),
            Source::Default,
        )
    });
    let base_url = norm_url(&base_url);

    // api_key: flag > env > --config > global. Project file is excluded.
    let api_key = ov
        .api_key
        .or_else(|| env("SUI_API_KEY"))
        .or_else(|| env("OPENAI_API_KEY"))
        .or(fp.api_key.clone())
        .or(gp.api_key.clone());

    // ── Endpoint trust gate ────────────────────────────────────────────
    // A credential may only flow to an endpoint the user already trusts:
    // CLI flag, env vars, --config file, global config, or any named
    // profile. A project file that redirects elsewhere must be approved
    // interactively — and is a hard failure when non-interactive.
    // `-y` NEVER bypasses this gate.
    if api_key.is_some() && base_src == Source::Project {
        let mut trusted: Vec<String> = Vec::new();
        for v in [
            ov.base_url.clone(),
            env("SUI_BASE_URL"),
            env("OPENAI_BASE_URL"),
            fp.base_url.clone(),
            gp.base_url.clone(),
        ]
        .into_iter()
        .flatten()
        {
            trusted.push(norm_url(&v));
        }
        if let Ok(ps) = profiles(ov.config_path.as_deref()) {
            for p in ps.values() {
                if let Some(b) = &p.base_url {
                    trusted.push(norm_url(b));
                }
            }
        }
        if !trusted.iter().any(|t| t == &base_url) {
            if ov.non_interactive {
                bail!(
                    "refusing to send configured credentials to untrusted endpoint {base_url} \
                     (set by project sui.toml). Add a trusted [profiles] entry or set \
                     SUI_BASE_URL explicitly."
                );
            }
            eprint!(
                "!! project sui.toml redirects API calls to untrusted endpoint {base_url}\n   \
                 send configured credentials there? [y/N] "
            );
            let _ = std::io::stderr().flush();
            let mut line = String::new();
            let _ = std::io::stdin().lock().read_line(&mut line);
            if !matches!(line.trim().to_lowercase().as_str(), "y" | "yes") {
                bail!("endpoint not trusted; aborting");
            }
            eprintln!("!! trusting {base_url} for this session only");
        }
    }

    let model = ov
        .model
        .or_else(|| env("SUI_MODEL"))
        .or(fp.model)
        .or(pp.model)
        .or(gp.model)
        .unwrap_or_else(|| {
            Transport::from_kind(fp.kind.as_deref().or(gp.kind.as_deref()))
                .unwrap_or_default()
                .default_model()
        });

    // prompt_cache_key picks the provider-side cache domain — letting a
    // repo file choose it would let a checked-in sui.toml borrow (or
    // poison) another trust context's cache prefix. Trusted sources only.
    if pp.prompt_cache_key.is_some() {
        eprintln!(
            "warning: prompt_cache_key in project sui.toml ignored \
             (cache domains are user-controlled)"
        );
    }
    let prompt_cache_key = env("SUI_CACHE_KEY")
        .or(fp.prompt_cache_key)
        .or(gp.prompt_cache_key);

    let al = resolve_limits(&fa, &pa, &ga);

    let session_id = format!("{}-{}", unix_ts(), std::process::id());
    let run_dir = std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/sui/runs")
        .join(&session_id);
    std::fs::create_dir_all(&run_dir).context("create run dir")?;

    // Project files cannot switch credential-bearing provider transports.
    let transport = if base_url.starts_with("codex://") {
        Transport::CodexOauth
    } else {
        Transport::from_kind(fp.kind.as_deref().or(gp.kind.as_deref()))?
    };
    let base_url = transport.resolve_url(Some(&base_url));
    Ok(Config {
        transport,
        image_input: fp
            .image_input
            .or(gp.image_input)
            .unwrap_or(transport.default_image_input()),
        base_url,
        api_key: if transport.is_account() {
            None
        } else {
            api_key
        },
        model,
        prompt_cache_key,
        workspace,
        run_dir,
        session_id,
        // auto_approve is flag/global-only — same trust class as
        // [profiles]/api_key: a checked-in project file must
        // not be able to turn on unattended bash/write execution.
        auto_approve: ov.auto_approve || fa.auto_approve.or(ga.auto_approve).unwrap_or(false),
        max_turns: al.max_turns,
        bash_timeout_ms: al.bash_timeout_ms,
        bash_timeout_max_ms: al.bash_timeout_max_ms,
        request_timeout_ms: al.request_timeout_ms,
        context_token_budget: al.context_token_budget,
        context_reserve_tokens: al.context_reserve_tokens,
        context_compaction: al.context_compaction,
    })
}

/// TUI state persisted in the global config under [ui]. Secrets never
/// appear here — auth stays env/keyring/session.
/// `serde(default)` matters: every field is Option except `acceptance`,
/// so a [ui] table missing that one key would fail try_into() and drop
/// ALL persisted settings to defaults without it.
#[derive(Debug, serde::Deserialize, serde::Serialize, Default, Clone)]
#[serde(default)]
pub struct UiSettings {
    /// Appearance: "slime" (default), "dark", or terminal-native "terminal".
    pub theme: Option<String>,
    /// Animation level: "full" | "calm" | "off" (None = full, calm over SSH).
    pub motion: Option<String>,
    pub workspace: Option<String>,
    /// "solo" | "mission"
    pub mode: Option<String>,
    pub solo_profile: Option<String>,
    pub orchestrator_profile: Option<String>,
    pub worker_profile: Option<String>,
    /// None = auditor uses the orchestrator profile.
    pub auditor_profile: Option<String>,
    pub worker_count: Option<usize>,
    /// Reasoning display preference: "auto" | "hidden" | "expanded".
    pub reasoning: Option<String>,
    /// Mouse capture (clicks, wheel, drag-select + OSC52 copy).
    /// None = on. When off the terminal keeps native text selection.
    pub mouse: Option<bool>,
    pub acceptance: Vec<String>,
    /// Web research access: "off" | "ask" | "auto" (None = "off").
    pub web_access: Option<String>,
    /// Env var holding the optional web-service API key (headless path).
    pub web_key_env: Option<String>,
}

/// Global config path (same location `load_ui`/`save_ui` use).
pub(crate) fn global_config_path() -> Option<PathBuf> {
    global_path().ok()
}

fn global_path() -> Result<PathBuf> {
    global_cfg_path().context("no home dir")
}

/// Load the [ui] section of the global config (absent → defaults).
pub fn load_ui() -> UiSettings {
    global_path()
        .ok()
        .filter(|p| p.exists())
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| s.parse::<toml::Value>().ok())
        .and_then(|v| v.get("ui")?.clone().try_into().ok())
        .unwrap_or_default()
}

/// Read+parse the config for rewriting. A missing file is a legitimate
/// first write → empty table. A file that exists but does NOT parse is
/// an error — overwriting would silently delete every other section,
/// including persisted API keys.
fn load_doc_for_write(p: &Path) -> Result<toml::Value> {
    match std::fs::read_to_string(p) {
        Ok(s) => s.parse::<toml::Value>().with_context(|| {
            format!(
                "config at {} does not parse — refusing to overwrite; fix or remove it manually",
                p.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            Ok(toml::Value::Table(toml::value::Table::new()))
        }
        Err(e) => Err(e).with_context(|| format!("read {}", p.display())),
    }
}

/// Write the [ui] section, preserving every other table in the file.
pub fn save_ui(ui: &UiSettings) -> Result<()> {
    let p = global_path()?;
    let mut doc = load_doc_for_write(&p)?;
    doc.as_table_mut()
        .context("config root not a table")?
        .insert(
            "ui".into(),
            toml::Value::try_from(ui).context("serialize ui settings")?,
        );
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    write_private(&p, &toml::to_string_pretty(&doc)?)
}

/// Config files can hold an inline api_key — never write them
/// world-readable. mode() covers creation; set_permissions tightens a
/// pre-existing loose file (truncate alone keeps the old mode).
fn write_private(p: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let mut f = o.open(p)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(contents.as_bytes())?;
    Ok(())
}

/// Persist a profile's non-secret fields into [profiles.<name>] of the
/// global config. `key_env` names the env var holding the key — the key
/// itself is never written by this function.
pub fn save_profile(
    name: &str,
    base_url: &str,
    model: &str,
    key_env: Option<&str>,
    api_key: Option<&str>,
    kind: Option<&str>,
) -> Result<()> {
    save_profile_at(
        &global_path()?,
        name,
        base_url,
        model,
        key_env,
        api_key,
        kind,
    )
}

/// save_profile against an explicit path — testable without touching the
/// user's real config.
pub fn save_profile_at(
    p: &Path,
    name: &str,
    base_url: &str,
    model: &str,
    key_env: Option<&str>,
    api_key: Option<&str>,
    kind: Option<&str>,
) -> Result<()> {
    let mut doc = load_doc_for_write(p)?;
    let root = doc.as_table_mut().context("config root not a table")?;
    let profs = root
        .entry("profiles")
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    let entry = profs
        .as_table_mut()
        .context("profiles not a table")?
        .entry(name)
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()));
    let t = entry.as_table_mut().context("profile not a table")?;
    // Account-backed kinds have no configured base_url or API key —
    // write the kind and strip the chat-completions fields entirely so
    // a stale endpoint can't shadow the OAuth backend.
    let transport = Transport::from_kind(kind)?;
    if transport.is_account() {
        let k = kind.context("account transport requires kind")?;
        t.insert("kind".into(), toml::Value::String(k.to_string()));
        t.insert("model".into(), toml::Value::String(model.to_string()));
        t.remove("base_url");
        t.remove("key_env");
        t.remove("api_key");
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        return write_private(p, &toml::to_string_pretty(&doc)?);
    }
    match kind {
        Some(k) => {
            Transport::from_kind(Some(k))?;
            t.insert("kind".into(), toml::Value::String(k.into()));
        }
        None => {
            t.remove("kind");
        }
    }
    t.insert("base_url".into(), toml::Value::String(base_url.to_string()));
    t.insert("model".into(), toml::Value::String(model.to_string()));
    match key_env {
        Some(k) => {
            t.insert("key_env".into(), toml::Value::String(k.to_string()));
            if api_key.is_none() {
                t.remove("api_key");
            }
        }
        None => {
            t.remove("key_env");
        }
    }
    // explicit user choice (Config file store): keep the key in the profile
    // so it survives restarts — the only durable path on headless boxes
    if let Some(k) = api_key {
        t.insert("api_key".into(), toml::Value::String(k.to_string()));
    }
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    write_private(p, &toml::to_string_pretty(&doc)?)
}

fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removed_native_profiles_are_rejected_without_rewriting_configuration() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::test_http::AuthHome::new();
        let path = home.path.join("config.toml");
        for kind in ["anthropic", "gemini", "gemini-oauth"] {
            let before = format!("[profiles.removed]\nkind = '{kind}'\nmodel = 'old-model'\n");
            std::fs::write(&path, &before).unwrap();
            let error = resolve_profile("removed", None).unwrap_err();
            assert!(error.to_string().contains("unsupported provider kind"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), before);
        }
    }

    #[test]
    fn account_configuration_ignores_and_removes_stale_api_credentials() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let home = crate::test_http::AuthHome::new();
        let path = home.path.join("config.toml");
        for kind in ["codex-oauth", "copilot"] {
            let transport = Transport::from_kind(Some(kind)).unwrap();
            std::fs::write(&path, format!(
                "[provider]\nkind = '{kind}'\napi_key = 'stale-api-key'\nbase_url = 'https://stale.invalid/v1'\nmodel = 'chosen-model'\n\n[profiles.account]\nkind = '{kind}'\napi_key = 'stale-profile-key'\nbase_url = 'https://stale.invalid/v1'\nmodel = 'chosen-model'\n\n[profiles.account.pricing]\ninput = 1.0\noutput = 2.0\n",
            )).unwrap();
            let config = load(Overrides {
                base_url: None,
                api_key: None,
                model: None,
                auto_approve: false,
                workspace: Some(home.path.clone()),
                config_path: None,
                non_interactive: true,
            })
            .unwrap();
            assert_eq!(config.base_url, transport.default_url());
            assert_eq!(config.transport, transport);
            assert!(config.api_key.is_none());
            let profile = resolve_profile("account", None).unwrap();
            assert_eq!(profile.base_url, transport.default_url());
            assert!(profile.api_key.is_none() && profile.pricing.is_none());
            save_profile(
                "account",
                "https://ignored.invalid",
                "chosen-model",
                Some("IGNORED_API_KEY"),
                Some("ignored-key"),
                Some(kind),
            )
            .unwrap();
            let saved = profiles(None).unwrap();
            let saved = &saved["account"];
            assert!(saved.base_url.is_none() && saved.api_key.is_none() && saved.key_env.is_none());
        }
    }

    #[test]
    fn save_refuses_to_overwrite_unparseable_config() {
        // Regression: a hand-edit typo used to be silently overwritten,
        // deleting every other section including inline API keys.
        let dir = std::env::temp_dir().join(format!("sui-cfgtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        let garbage = "[profiles.x\napi_key = \"sk-keepme\"";
        std::fs::write(&p, garbage).unwrap();

        let r = save_profile_at(&p, "y", "http://x", "m", None, None, None);
        assert!(r.is_err());
        assert!(format!("{:#}", r.unwrap_err()).contains("refusing to overwrite"));
        // file untouched — the typo'd content survives for manual repair
        assert_eq!(std::fs::read_to_string(&p).unwrap(), garbage);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Point the Codex discovery env at a temp pair; restores on drop.
    struct CodexHomes;
    impl CodexHomes {
        fn with_session(session: bool) -> (Self, PathBuf, PathBuf) {
            let sui = std::env::temp_dir().join(format!("sui-cfgenv-s-{}", std::process::id()));
            let codex = std::env::temp_dir().join(format!("sui-cfgenv-c-{}", std::process::id()));
            // Same pid → same paths across tests in this binary: wipe
            // leftovers so a prior test's auth.json can't leak in.
            let _ = std::fs::remove_dir_all(&sui);
            let _ = std::fs::remove_dir_all(&codex);
            std::fs::create_dir_all(&sui).unwrap();
            std::fs::create_dir_all(&codex).unwrap();
            if session {
                std::fs::write(
                    codex.join("auth.json"),
                    r#"{"tokens":{"access_token":"a","refresh_token":"r"}}"#,
                )
                .unwrap();
            }
            unsafe {
                std::env::set_var("SUI_HOME", &sui);
                std::env::set_var("CODEX_HOME", &codex);
            }
            (Self, sui, codex)
        }
    }
    impl Drop for CodexHomes {
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var("SUI_HOME");
                std::env::remove_var("CODEX_HOME");
            }
        }
    }

    #[test]
    fn codex_session_registers_profile() {
        let _g = crate::TEST_ENV_LOCK.lock().unwrap();
        let (_h, _s, _c) = CodexHomes::with_session(true);
        let mut out = BTreeMap::new();
        inject_detected_profiles(&mut out);
        let codex = out.get("codex").expect("session registers a profile");
        assert_eq!(codex.kind.as_deref(), Some("codex-oauth"));
        assert!(codex.model.is_some());
    }

    #[test]
    fn explicit_codex_profile_beats_detected() {
        let _g = crate::TEST_ENV_LOCK.lock().unwrap();
        let (_h, _s, _c) = CodexHomes::with_session(true);
        let mut out = BTreeMap::new();
        out.insert(
            "codex".to_string(),
            ProfileCfg {
                kind: Some("codex-oauth".into()),
                model: Some("gpt-5.5".into()),
                ..Default::default()
            },
        );
        inject_detected_profiles(&mut out);
        assert_eq!(out["codex"].model.as_deref(), Some("gpt-5.5"));
    }

    #[test]
    fn no_session_no_injection() {
        let _g = crate::TEST_ENV_LOCK.lock().unwrap();
        let (_h, _s, _c) = CodexHomes::with_session(false);
        let mut out = BTreeMap::new();
        inject_detected_profiles(&mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn save_writes_when_file_missing() {
        let dir = std::env::temp_dir().join(format!("sui-cfgtest2-{}", std::process::id()));
        let p = dir.join("config.toml");
        save_profile_at(&p, "y", "http://x", "m", None, None, None).unwrap();
        assert!(std::fs::read_to_string(&p).unwrap().contains("profiles"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod agent_limit_tests {
    use super::*;

    #[test]
    fn agent_limits_reads_project_sui_toml() {
        let dir = std::env::temp_dir().join(format!("sui-al-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("sui.toml"),
            "[agent]\nmax_turns = 7\ncontext_token_budget = 999\n",
        )
        .unwrap();
        let al = agent_limits(&dir);
        // project sui.toml beats global/defaults for set keys
        assert_eq!(al.max_turns, 7);
        assert_eq!(al.context_token_budget, 999);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
