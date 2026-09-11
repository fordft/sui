use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    /// None = no key configured. Never sourced from project-level sui.toml.
    pub api_key: Option<String>,
    pub model: String,
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
}

/// A named provider profile. Credentials live in the user's own config or
/// environment — profiles may ONLY be defined in global/--config files,
/// never in project sui.toml.
#[derive(Debug, Deserialize, Default, Clone)]
pub struct ProfileCfg {
    pub base_url: Option<String>,
    pub model: Option<String>,
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

#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
    pub prompt_cache_key: Option<String>,
    pub pricing: Option<PricingCfg>,
}

#[derive(Debug, Deserialize, Default)]
struct FileConfig {
    provider: Option<ProviderCfg>,
    agent: Option<AgentCfg>,
    profiles: Option<BTreeMap<String, ProfileCfg>>,
}

#[derive(Debug, Deserialize, Default)]
struct ProviderCfg {
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
    std::env::home_dir().map(|h| h.join(".config/sui/config.toml"))
}

fn read_toml(p: &Path) -> Result<FileConfig> {
    let s = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
    toml::from_str(&s).with_context(|| format!("parse {}", p.display()))
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
    Ok(out)
}

/// Resolve a named profile for certification. Key material comes from the
/// profile's key_env env var, or an inline api_key in the user-owned file.
pub fn resolve_profile(name: &str, config_path: Option<&Path>) -> Result<Profile> {
    let all = profiles(config_path)?;
    let p = all
        .get(name)
        .with_context(|| {
            format!(
                "unknown profile '{name}' (defined: {})",
                if all.is_empty() {
                    "none — add [profiles.<name>] to ~/.config/sui/config.toml".into()
                } else {
                    all.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            )
        })?;
    let api_key = p
        .key_env
        .as_deref()
        .and_then(|k| std::env::var(k).ok())
        .filter(|v| !v.is_empty())
        .or_else(|| p.api_key.clone());
    Ok(Profile {
        name: name.to_string(),
        base_url: norm_url(p.base_url.as_deref().unwrap_or("https://api.openai.com/v1")),
        model: p.model.clone().unwrap_or_else(|| "gpt-5".into()),
        api_key,
        prompt_cache_key: p.prompt_cache_key.clone(),
        pricing: p.pricing.clone(),
    })
}

pub fn load(ov: Overrides) -> Result<Config> {
    let workspace = ov
        .workspace
        .clone()
        .unwrap_or_else(|| PathBuf::from("."))
        .canonicalize()
        .context("workspace path does not exist")?;

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
    .unwrap_or_else(|| ("https://api.openai.com/v1".into(), Source::Default));
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
        .unwrap_or_else(|| "gpt-5".into());

    let prompt_cache_key = env("SUI_CACHE_KEY")
        .or(fp.prompt_cache_key)
        .or(pp.prompt_cache_key)
        .or(gp.prompt_cache_key);

    let max_turns = fa.max_turns.or(pa.max_turns).or(ga.max_turns).unwrap_or(60);
    let bash_timeout_ms = fa.bash_timeout_ms.or(pa.bash_timeout_ms).or(ga.bash_timeout_ms).unwrap_or(120_000);
    let bash_timeout_max_ms = fa.bash_timeout_max_ms.or(pa.bash_timeout_max_ms).or(ga.bash_timeout_max_ms).unwrap_or(600_000);
    let request_timeout_ms = fa.request_timeout_ms.or(pa.request_timeout_ms).or(ga.request_timeout_ms).unwrap_or(300_000);
    let context_token_budget = fa.context_token_budget.or(pa.context_token_budget).or(ga.context_token_budget).unwrap_or(120_000);
    let context_reserve_tokens = fa.context_reserve_tokens.or(pa.context_reserve_tokens).or(ga.context_reserve_tokens).unwrap_or(8_192);

    let session_id = format!("{}-{}", unix_ts(), std::process::id());
    let run_dir = std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/share/sui/runs")
        .join(&session_id);
    std::fs::create_dir_all(&run_dir).context("create run dir")?;

    Ok(Config {
        base_url,
        api_key,
        model,
        prompt_cache_key,
        workspace,
        run_dir,
        session_id,
        auto_approve: ov.auto_approve || fa.auto_approve.or(pa.auto_approve).or(ga.auto_approve).unwrap_or(false),
        max_turns,
        bash_timeout_ms,
        bash_timeout_max_ms,
        request_timeout_ms,
        context_token_budget,
        context_reserve_tokens,
    })
}

/// TUI state persisted in the global config under [ui]. Secrets never
/// appear here — auth stays env/keyring/session.
#[derive(Debug, serde::Deserialize, serde::Serialize, Default, Clone)]
pub struct UiSettings {
    pub workspace: Option<String>,
    /// "solo" | "mission"
    pub mode: Option<String>,
    pub solo_profile: Option<String>,
    pub orchestrator_profile: Option<String>,
    pub worker_profile: Option<String>,
    /// None = auditor uses the orchestrator profile.
    pub auditor_profile: Option<String>,
    pub worker_count: Option<usize>,
    pub acceptance: Vec<String>,
}

fn global_path() -> Result<PathBuf> {
    std::env::home_dir()
        .map(|h| h.join(".config/sui/config.toml"))
        .context("no home dir")
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

/// Write the [ui] section, preserving every other table in the file.
pub fn save_ui(ui: &UiSettings) -> Result<()> {
    let p = global_path()?;
    let mut doc: toml::Value = std::fs::read_to_string(&p)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| toml::Value::Table(toml::value::Table::new()));
    doc.as_table_mut()
        .context("config root not a table")?
        .insert("ui".into(), toml::Value::try_from(ui).context("serialize ui settings")?);
    if let Some(d) = p.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(&p, toml::to_string_pretty(&doc)?)?;
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
) -> Result<()> {
    save_profile_at(&global_path()?, name, base_url, model, key_env, api_key)
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
) -> Result<()> {
    let mut doc: toml::Value = std::fs::read_to_string(p)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| toml::Value::Table(toml::value::Table::new()));
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
    std::fs::write(&p, toml::to_string_pretty(&doc)?)?;
    Ok(())
}

fn unix_ts() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
