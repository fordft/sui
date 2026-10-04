use anyhow::Result;
use clap::Parser;
use rustyline::DefaultEditor;
use std::io::IsTerminal;
use std::time::Duration;
use sui::{agent, config, context, journal, permission, provider, tools, web};

#[derive(clap::Subcommand)]
enum Sub {
    /// Terminal UI (also the default when run bare on a terminal)
    Tui,
    /// MCP artifact-submission bridge served over stdio — spawned by ACP
    /// agents via session/new mcp_servers; not for interactive use
    AcpBridge {
        /// Session-scoped artifact drop directory
        #[arg(long)]
        dir: std::path::PathBuf,
        /// Expected payload kind: plan | verdict | decision | any
        #[arg(long, default_value = "any")]
        expect: String,
    },
    /// Sign in to OpenAI with your ChatGPT account (Codex OAuth).
    /// Reuses an existing `codex login` session automatically; this writes
    /// Sui's own token store so its refresh chain never collides with the
    /// Codex CLI's.
    Auth {
        /// Paste the callback URL yourself (headless/SSH — the browser's
        /// localhost is not this machine's localhost)
        #[arg(long)]
        manual: bool,
    },
    /// Export a recorded run's journals into a sanitized report (no API calls)
    Export {
        /// Export the latest run associated with the current workspace
        #[arg(long)]
        latest: bool,
        /// Run id — the runs/<id> directory name or a unique prefix
        #[arg(long)]
        run: Option<String>,
        /// markdown (default) or json
        #[arg(long, default_value = "markdown", value_parser = ["markdown", "json"])]
        format: String,
        /// Include a bounded git diff between recorded base and accepted sha
        #[arg(long)]
        include_diff: bool,
    },
}

#[derive(Parser)]
#[command(
    name = "sui",
    version,
    about = "cache-first multi-agent coding harness (v0: fast path)"
)]
struct Cli {
    #[command(subcommand)]
    sub: Option<Sub>,
    /// OpenAI-compatible base URL (e.g. http://localhost:8000/v1)
    #[arg(long)]
    base_url: Option<String>,
    /// Model name passed verbatim to the endpoint
    #[arg(long)]
    model: Option<String>,
    /// Named profile from config (e.g. `codex` — auto-registered when a
    /// Codex OAuth session exists). Explicit --base-url/--model/--api-key
    /// flags override the profile's values.
    #[arg(long)]
    profile: Option<String>,
    /// API key (prefer SUI_API_KEY / OPENAI_API_KEY env vars)
    #[arg(long)]
    api_key: Option<String>,
    /// Config file path (overrides sui.toml discovery)
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Workspace root (default: cwd)
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
    /// Auto-approve all tool calls (same as --yolo)
    #[arg(long, short = 'y', global = true)]
    yes: bool,
    /// YOLO mode — auto-approve everything, no permission prompts
    #[arg(long, global = true)]
    yolo: bool,
    /// Start the TUI in mission mode (orchestrator → workers → auditor)
    #[arg(long, global = true)]
    mission: bool,
    /// Resume a recorded native session (exact run ID or "latest"). No old tools are rerun.
    #[arg(long, global = true, conflicts_with = "mission")]
    resume: Option<String>,
    /// One-shot prompt; omit to open the TUI (or REPL when not a terminal)
    prompt: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Sub::Tui) = &cli.sub {
        return sui::tui::run_with_resume(
            cli.mission,
            cli.yes || cli.yolo,
            cli.resume,
            cli.workspace,
        )
        .await;
    }
    if let Some(Sub::AcpBridge { dir, expect }) = &cli.sub {
        return sui::acp::bridge::serve(dir, expect);
    }
    if let Some(Sub::Auth { manual }) = &cli.sub {
        let path = sui::codex::login(*manual).await?;
        println!("Signed in. Token store: {}", path.display());
        println!(
            "The `codex` profile is now registered automatically — pick it \
             in Settings → Solo/Orchestrator role, or pass \
             --control-profile codex / --worker-profile codex to sui-mission.\n\
             To override the default model, add [profiles.codex] model = \"…\" \
             to ~/.config/sui/config.toml."
        );
        return Ok(());
    }
    if let Some(Sub::Export {
        latest,
        run,
        format,
        include_diff,
    }) = &cli.sub
    {
        if !latest && run.is_none() {
            anyhow::bail!("specify --latest or --run <run-id> (see ~/.local/share/sui/runs/)");
        }
        let path = sui::export::run_export(&sui::export::ExportOpts {
            run_id: run.clone(),
            latest_for_workspace: if *latest {
                std::env::current_dir().ok()
            } else {
                None
            },
            format: if format == "json" {
                sui::export::Format::Json
            } else {
                sui::export::Format::Markdown
            },
            include_diff: *include_diff,
            runs_root: None,
            out_root: None,
            running: false,
        })?;
        println!("Report exported:\n  {}", path.display());
        println!("\nReview before sharing: the report may contain project code and commands.");
        return Ok(());
    }
    // Bare `sui` on a terminal opens the TUI — the headless path is for
    // one-shot prompts and non-interactive (piped/scripted) use.
    if cli.prompt.is_none() && std::io::stdin().is_terminal() {
        return sui::tui::run_with_resume(
            cli.mission,
            cli.yes || cli.yolo,
            cli.resume,
            cli.workspace,
        )
        .await;
    }
    let non_interactive = cli.prompt.is_some() || !std::io::stdin().is_terminal();
    // Explicit flags beat a --profile choice — capture before they move.
    let flag_base = cli.base_url.clone();
    let flag_model = cli.model.clone();
    let flag_key = cli.api_key.clone();
    let mut cfg = config::load(config::Overrides {
        base_url: cli.base_url,
        api_key: cli.api_key,
        model: cli.model,
        auto_approve: cli.yes || cli.yolo,
        workspace: cli.workspace.clone(),
        config_path: cli.config.clone(),
        non_interactive,
    })?;
    let saved = cli
        .resume
        .as_ref()
        .map(|id| sui::session::SavedSession::load(&sui::session::runs_root(), id, &cfg.workspace))
        .transpose()?;
    let profile_name = cli
        .profile
        .clone()
        .or_else(|| saved.as_ref().and_then(|s| s.header.profile.clone()));
    if let Some(pname) = &profile_name {
        let p = config::resolve_profile(pname, cli.config.as_deref())
            .map_err(|e| anyhow::anyhow!("--profile {pname}: {e:#}"))?;
        cfg.base_url = flag_base.clone().unwrap_or(p.base_url);
        cfg.model = flag_model.clone().unwrap_or(p.model);
        cfg.image_input = p.image_input;
        cfg.transport = p.transport;
        cfg.api_key = flag_key.or(p.api_key);
        cfg.prompt_cache_key = p.prompt_cache_key.or(cfg.prompt_cache_key);
    }
    if let Some(saved) = &saved {
        let provider = provider::Provider::new(
            &cfg.base_url,
            cfg.api_key.clone(),
            cfg.model.clone(),
            cfg.prompt_cache_key.clone(),
        )
        .with_transport(cfg.transport)
        .with_image_input(cfg.image_input);
        if saved.header.signature != sui::session::Signature::current(&provider, &cfg.workspace) {
            anyhow::bail!("session provider, model, tools or project guidance changed; restore the configuration or start a new session");
        }
        cfg.session_id = saved.header.session_id.clone();
        saved.fork(&cfg.run_dir, "headless")?;
    }
    let _session_lock = sui::session::SessionLock::acquire(&cfg.run_dir)?;

    eprintln!(
        "sui v0 · model={} · base={} · ws={} · log={}",
        cfg.model,
        cfg.base_url,
        cfg.workspace.display(),
        cfg.run_dir.display()
    );
    if cfg.api_key.is_none() && !cfg.base_url.starts_with("codex://") {
        eprintln!("warning: no API key set (SUI_API_KEY / OPENAI_API_KEY)");
    }

    let mut journal = journal::Journal::open_named(&cfg.run_dir, "headless")?;
    journal.log(
        journal::ev::SESSION,
        serde_json::json!({
            "mode": "headless",
            "workspace": cfg.workspace,
            "sui_version": env!("CARGO_PKG_VERSION"),
            "approval": if cfg.auto_approve { "auto" } else { "ask" },
            "model": cfg.model,
            "base_url": cfg.base_url,
        }),
    );

    let provider = provider::Provider::new(
        &cfg.base_url,
        cfg.api_key.clone(),
        cfg.model.clone(),
        cfg.prompt_cache_key.clone(),
    )
    .with_transport(cfg.transport)
    .with_image_input(cfg.image_input);
    let tools = tools::ToolContext {
        workspace: cfg.workspace.clone(),
        bash_timeout: Duration::from_millis(cfg.bash_timeout_ms),
        bash_timeout_max: Duration::from_millis(cfg.bash_timeout_max_ms),
        web: Some(web::WebService::new(web::load_cfg(None))),
        canon_root: std::sync::OnceLock::new(),
        ui: std::sync::OnceLock::new(),
    };
    let mut agent = agent::Agent::new(
        provider,
        tools,
        permission::Gate::new(cfg.auto_approve),
        journal,
        agent::Limits {
            max_turns: cfg.max_turns,
            context_budget: cfg.context_token_budget,
            context_reserve: cfg.context_reserve_tokens,
            compact_context: cfg.context_compaction,
            request_timeout: Duration::from_millis(cfg.request_timeout_ms),
        },
        agent::Identity {
            session_id: cfg.session_id.clone(),
            agent_id: saved
                .as_ref()
                .map(|s| s.header.agent_id.clone())
                .unwrap_or_else(|| "fast-path".into()),
            role: "worker".into(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            cache_key_fingerprint: cfg
                .prompt_cache_key
                .as_ref()
                .map(|k| context::sha256_hex(k.as_bytes())),
        },
    );
    if let Some(saved) = &saved {
        agent.restore_session(saved)?;
        agent.jlog("session_resumed", serde_json::json!({"source": saved.id}));
        eprintln!(
            "resumed {} · recorded checks are historical; no old tools rerun",
            saved.id
        );
    } else {
        agent.record_session(profile_name)?;
    }
    drop(saved);

    match cli.prompt {
        Some(p) => agent.run_turn(&p).await?,
        None => repl(&mut agent).await?,
    }
    Ok(())
}

async fn repl(agent: &mut agent::Agent) -> Result<()> {
    let mut rl = DefaultEditor::new()?;
    let hist = std::env::home_dir()
        .unwrap_or_default()
        .join(".local/share/sui/history.txt");
    let _ = rl.load_history(&hist);
    eprintln!("REPL · /quit to exit · /help for commands");
    while let Ok(line) = rl.readline("sui> ") {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(line);
        match line {
            "/quit" | "/q" | "/exit" => break,
            "/help" => {
                eprintln!("commands: /quit /help — anything else is sent to the agent")
            }
            _ => {
                if let Err(e) = agent.run_turn(line).await {
                    eprintln!("error: {e:#}");
                }
            }
        }
    }
    if let Some(dir) = hist.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = rl.save_history(&hist);
    Ok(())
}
