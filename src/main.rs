use anyhow::Result;
use clap::Parser;
use rustyline::DefaultEditor;
use std::io::IsTerminal;
use std::time::Duration;
use sui::{agent, config, context, journal, permission, provider, tools};

#[derive(clap::Subcommand)]
enum Sub {
    /// Terminal UI: setup, provider/model roles, chat, mission view
    Tui {
        /// Start in mission mode (orchestrator → workers → auditor)
        #[arg(long)]
        mission: bool,
    },
}

#[derive(Parser)]
#[command(name = "sui", version, about = "cache-first multi-agent coding harness (v0: fast path)")]
struct Cli {
    #[command(subcommand)]
    sub: Option<Sub>,
    /// OpenAI-compatible base URL (e.g. http://localhost:8000/v1)
    #[arg(long)]
    base_url: Option<String>,
    /// Model name passed verbatim to the endpoint
    #[arg(long)]
    model: Option<String>,
    /// API key (prefer SUI_API_KEY / OPENAI_API_KEY env vars)
    #[arg(long)]
    api_key: Option<String>,
    /// Config file path (overrides sui.toml discovery)
    #[arg(long)]
    config: Option<std::path::PathBuf>,
    /// Workspace root (default: cwd)
    #[arg(long)]
    workspace: Option<std::path::PathBuf>,
    /// Auto-approve all tool calls
    #[arg(long, short = 'y')]
    yes: bool,
    /// One-shot prompt; omit for interactive REPL
    prompt: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Sub::Tui { mission }) = &cli.sub {
        return sui::tui::run(*mission).await;
    }
    let non_interactive = cli.prompt.is_some() || !std::io::stdin().is_terminal();
    let cfg = config::load(config::Overrides {
        base_url: cli.base_url,
        api_key: cli.api_key,
        model: cli.model,
        auto_approve: cli.yes,
        workspace: cli.workspace,
        config_path: cli.config,
        non_interactive,
    })?;

    eprintln!(
        "sui v0 · model={} · base={} · ws={} · log={}",
        cfg.model,
        cfg.base_url,
        cfg.workspace.display(),
        cfg.run_dir.display()
    );
    if cfg.api_key.is_none() {
        eprintln!("warning: no API key set (SUI_API_KEY / OPENAI_API_KEY)");
    }

    let mut journal = journal::Journal::open(&cfg.run_dir)?;
    journal.log(
        "session_start",
        serde_json::json!({
            "model": cfg.model,
            "base_url": cfg.base_url,
            "workspace": cfg.workspace,
        }),
    );

    let provider = provider::Provider::new(
        &cfg.base_url,
        cfg.api_key.clone(),
        cfg.model.clone(),
        cfg.prompt_cache_key.clone(),
    );
    let tools = tools::ToolContext {
        workspace: cfg.workspace.clone(),
        bash_timeout: Duration::from_millis(cfg.bash_timeout_ms),
        bash_timeout_max: Duration::from_millis(cfg.bash_timeout_max_ms),
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
            request_timeout: Duration::from_millis(cfg.request_timeout_ms),
        },
        agent::Identity {
            session_id: cfg.session_id.clone(),
            agent_id: "fast-path".into(),
            role: "worker".into(),
            base_url: cfg.base_url.clone(),
            model: cfg.model.clone(),
            cache_key_fingerprint: cfg
                .prompt_cache_key
                .as_ref()
                .map(|k| context::sha256_hex(k.as_bytes())),
        },
    );

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
    loop {
        match rl.readline("sui> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                match line {
                    "/quit" | "/q" | "/exit" => break,
                    "/help" => eprintln!("commands: /quit /help — anything else is sent to the agent"),
                    _ => {
                        if let Err(e) = agent.run_turn(line).await {
                            eprintln!("error: {e:#}");
                        }
                    }
                }
            }
            Err(_) => break, // Ctrl-C / Ctrl-D
        }
    }
    if let Some(dir) = hist.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = rl.save_history(&hist);
    Ok(())
}
