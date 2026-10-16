//! Private JSON-lines transport for the Desktop shell. The Rust agent remains
//! the sole owner of tools, permissions, journals and session recovery.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};

use crate::events::{GateChoice, UiEvent};

const MAX_COMMAND: usize = 128 * 1024;
const MAX_MESSAGE: usize = 64 * 1024;

#[derive(Deserialize)]
struct Setup {
    name: String,
    kind: String,
    base_url: String,
    model: String,
    #[serde(default)]
    api_key: String,
}

/// Configuration uses the existing owner-only global profile store. The key
/// travels only over local stdio or the user's SSH channel, never argv/logs.
pub fn setup() -> Result<()> {
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    if line.len() > 16 * 1024 {
        bail!("profile request exceeds 16 KiB");
    }
    let input: Setup = serde_json::from_str(&line).context("invalid profile request")?;
    if input.name.is_empty()
        || input.name.len() > 64
        || !input
            .name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("profile name must use letters, digits, hyphens or underscores");
    }
    if !matches!(
        input.kind.as_str(),
        "chat-completions" | "openai-responses" | "ollama"
    ) {
        bail!("unsupported desktop model kind");
    }
    let url = reqwest::Url::parse(&input.base_url).context("invalid base URL")?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("base URL must be HTTP(S) without embedded credentials");
    }
    if input.model.trim().is_empty() || input.model.len() > 200 || input.api_key.len() > 8192 {
        bail!("invalid model or API key length");
    }
    crate::config::save_profile(
        &input.name,
        &input.base_url,
        &input.model,
        None,
        Some(&input.api_key),
        Some(&input.kind),
    )?;
    send_line(&json!({"type":"profile_saved","name":input.name}))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Command {
    Send { run: u64, text: String },
    Approve { id: u64, choice: String },
    Stop,
    ListSessions,
    ListProfiles,
    SetApproval { mode: ApprovalMode },
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum ApprovalMode {
    Ask,
    AutoEdits,
}

fn project_event(event: UiEvent) -> (Value, Option<(u64, mpsc::UnboundedSender<GateChoice>)>) {
    match event {
        UiEvent::ReqStart { run, agent, req } => (
            json!({"type":"request_start","run":run,"agent":agent,"req":req}),
            None,
        ),
        UiEvent::Delta {
            run,
            agent,
            req,
            text,
        } => (
            json!({"type":"delta","run":run,"agent":agent,"req":req,"text":text}),
            None,
        ),
        UiEvent::Reason {
            run,
            agent,
            req,
            text,
        } => (
            json!({"type":"reason","run":run,"agent":agent,"req":req,"text":text}),
            None,
        ),
        UiEvent::ReqDone {
            run,
            agent,
            req,
            ms,
            ok,
            reasoning,
        } => (
            json!({"type":"request_done","run":run,"agent":agent,"req":req,"ms":ms,"ok":ok,"reasoning":reasoning}),
            None,
        ),
        UiEvent::ToolStart {
            run,
            agent,
            req,
            call,
            name,
            summary,
        } => (
            json!({"type":"tool_start","run":run,"agent":agent,"req":req,"call":call,"name":name,"summary":summary}),
            None,
        ),
        UiEvent::ToolOut {
            run,
            agent,
            call,
            err,
            text,
        } => (
            json!({"type":"tool_out","run":run,"agent":agent,"call":call,"err":err,"text":text}),
            None,
        ),
        UiEvent::ToolDone {
            run,
            agent,
            call,
            name,
            summary,
            ms,
            status,
            exit,
            result,
            truncated,
            dropped,
        } => (
            json!({"type":"tool_done","run":run,"agent":agent,"call":call,"name":name,"summary":summary,"ms":ms,"status":status.label(),"exit":exit,"result":result,"truncated":truncated,"dropped":dropped}),
            None,
        ),
        UiEvent::Usage {
            run,
            agent,
            model,
            input,
            cached,
            written,
            output,
            complete,
            ..
        } => (
            json!({"type":"usage","run":run,"agent":agent,"model":model,"input":input,"cached":cached,"written":written,"output":output,"complete":complete}),
            None,
        ),
        UiEvent::Permission {
            run,
            id,
            agent,
            summary,
            reply,
        } => (
            json!({"type":"permission","run":run,"id":id,"agent":agent,"allow_session": !["bash:", "terminal:", "code_intel:"].iter().any(|prefix| summary.starts_with(prefix)),"summary":summary}),
            Some((id, reply)),
        ),
        UiEvent::Phase { run, agent, text } => (
            json!({"type":"phase","run":run,"agent":agent,"text":text}),
            None,
        ),
        UiEvent::RunDone {
            run,
            outcome,
            accepted_sha,
        } => (
            json!({"type":"run_done","run":run,"outcome":outcome,"accepted_sha":accepted_sha}),
            None,
        ),
        UiEvent::Error { run, agent, msg } => (
            json!({"type":"error","run":run,"agent":agent,"message":msg}),
            None,
        ),
        UiEvent::MissionState(state) => (json!({"type":"mission_state","state":state}), None),
        UiEvent::TaskRows(rows) => (json!({"type":"task_rows","rows":rows}), None),
        UiEvent::ChangeSet { files, sha } => {
            (json!({"type":"change_set","files":files,"sha":sha}), None)
        }
        UiEvent::AuditResult(result) => (json!({"type":"audit_result","result":result}), None),
    }
}

fn send_line(value: &Value) -> Result<()> {
    let mut stdout = std::io::stdout().lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

fn send_projected(value: &Value) -> Result<()> {
    let Some(content) = value.get("text").and_then(Value::as_str) else {
        return send_line(value);
    };
    if content.len() <= 16 * 1024 {
        return send_line(value);
    }
    let mut start = 0;
    while start < content.len() {
        let mut end = (start + 16 * 1024).min(content.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
        let mut part = value.clone();
        part["text"] = Value::String(content[start..end].to_string());
        send_line(&part)?;
        start = end;
    }
    Ok(())
}

/// One bridge process owns one native Solo session. It may handle many turns.
pub async fn run(
    workspace: &Path,
    profile: &str,
    resume: Option<&str>,
    reasoning_effort: Option<crate::reasoning::Effort>,
    model_override: Option<&str>,
) -> Result<()> {
    let workspace = workspace
        .canonicalize()
        .context("workspace does not exist")?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory");
    }
    let mut prof = crate::config::resolve_profile(profile, None)?;
    let chatgpt = prof.transport == crate::config::Transport::Chatgpt;
    if chatgpt && !crate::chatgpt::connected(profile) {
        bail!("connect this ChatGPT account and enable plan usage in provider setup");
    }
    let saved = resume
        .map(|id| crate::session::SavedSession::load(&crate::session::runs_root(), id, &workspace))
        .transpose()?;
    let catalog = if chatgpt {
        Some(crate::chatgpt::catalog(profile, false).await?)
    } else {
        None
    };
    let selected_model = model_override.or_else(|| {
        saved
            .as_ref()
            .filter(|_| chatgpt)
            .map(|s| s.header.model.as_str())
    });
    if let Some(selected) = selected_model {
        if selected != prof.model {
            prof.reasoning_effort = None;
        }
        if !chatgpt && selected != prof.model {
            bail!("this provider uses the model configured in its profile");
        }
        prof.model = selected.into();
    }
    let catalog_model = if let Some(catalog) = &catalog {
        let chosen = catalog
            .items
            .iter()
            .find(|m| m.id == prof.model)
            .or_else(|| {
                if selected_model.is_none() {
                    catalog.items.first()
                } else {
                    None
                }
            })
            .context(
                "choose a model available to this ChatGPT account; refresh models in the composer",
            )?;
        if chosen.id != prof.model {
            prof.reasoning_effort = None;
        }
        prof.model = chosen.id.clone();
        Some(chosen)
    } else {
        None
    };
    let model = prof.model.clone();
    if model.trim().is_empty() {
        bail!("choose a model in provider setup before connecting");
    }
    let model_name = catalog_model.map(|m| m.name.as_str()).unwrap_or(&model);
    let reasoning_choices = catalog_model
        .map(|m| m.reasoning_choices(&prof))
        .unwrap_or_else(|| crate::reasoning::choices(&prof));
    let reasoning_levels = catalog_model.and_then(|m| m.reasoning_levels.as_ref());
    let default_reasoning = catalog_model
        .and_then(|m| m.default_reasoning.as_ref())
        .and_then(|effort| serde_json::from_value::<crate::reasoning::Effort>(json!(effort)).ok())
        .filter(|e| reasoning_choices.contains(e));
    let requested = reasoning_effort.or(match &saved {
        Some(saved) => saved.header.reasoning_effort,
        None => prof.reasoning_effort,
    });
    if let Some(effort) = requested {
        if !reasoning_choices.contains(&effort) {
            bail!("choose a reasoning level supported by this model and transport");
        }
    }
    // Honor the account catalog's model-specific default, when it publishes one.
    prof.reasoning_effort = if saved.is_some() && reasoning_effort.is_none() {
        requested
    } else {
        match requested {
            None | Some(crate::reasoning::Effort::Default) => default_reasoning.or(requested),
            Some(effort) => Some(effort),
        }
    };
    let reasoning_effort = prof.reasoning_effort.unwrap_or(
        if prof.transport == crate::config::Transport::CodexOauth {
            crate::reasoning::Effort::High
        } else {
            crate::reasoning::Effort::Default
        },
    );
    let (history, history_truncated): (Vec<Value>, bool) = saved
        .as_ref()
        .map(|session| {
            let entries = session.events.iter().filter_map(|event| {
                let role = match event["type"].as_str()? {
                    "task" => "user",
                    "assistant" => "assistant",
                    _ => return None,
                };
                let content = event["data"][if role == "user" { "task" } else { "content" }]
                    .as_str()?;
                (!content.is_empty()).then_some((role, content))
            });
            let all: Vec<_> = entries.collect();
            let omitted = all.len() > 30 || all.iter().any(|(_, content)| content.chars().count() > 4000);
            let rows = all.into_iter().rev().take(30).map(|(role, content)| {
                let preview: String = content.chars().take(4000).collect();
                json!({"role":role,"content":if preview.len() < content.len() { format!("{preview}\n… [preview truncated]") } else { preview }})
            }).collect::<Vec<_>>().into_iter().rev().collect();
            (rows, omitted)
        })
        .unwrap_or_default();
    if let Some(saved) = &saved {
        if saved
            .header
            .profile
            .as_deref()
            .is_some_and(|name| name != profile)
            || saved.header.signature
                != crate::session::Signature::current(
                    &crate::provider::Provider::from_profile(&prof),
                    &workspace,
                )
        {
            bail!("recorded profile, model, tools or project guidance changed");
        }
    }
    let dir = crate::session::new_run_dir()?;
    if let Some(saved) = &saved {
        saved.fork(&dir, "solo")?;
    }
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    let cancel = Arc::new(Notify::new());
    let stopped = Arc::new(AtomicBool::new(false));
    let session_allow = Arc::new(AtomicBool::new(false));
    let solo = crate::tui::start_solo(
        prof,
        workspace.clone(),
        dir.clone(),
        event_tx,
        cancel.clone(),
        stopped.clone(),
        session_allow.clone(),
        Some(crate::web::WebService::new(crate::web::load_cfg(None))),
        saved.as_ref(),
    )?;
    drop(saved);
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<Result<Command, String>>();
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let command = match line {
                Ok(line) if line.len() <= MAX_COMMAND => {
                    serde_json::from_str(&line).map_err(|e| e.to_string())
                }
                Ok(_) => Err("command exceeds 128 KiB".into()),
                Err(e) => {
                    let _ = command_tx.send(Err(e.to_string()));
                    break;
                }
            };
            if command_tx.send(command).is_err() {
                break;
            }
        }
    });
    send_line(
        &json!({"type":"hello","version":env!("CARGO_PKG_VERSION"),"workspace":workspace,"profile":profile,"model":model,"model_name":model_name,"reasoning_levels":reasoning_levels,"catalog_warning":catalog.as_ref().and_then(|c| c.warning.as_ref()),"reasoning_effort":reasoning_effort,"reasoning_choices":reasoning_choices,"approval":"ask","kind":if chatgpt { "chatgpt" } else { "" },"run_id":dir.file_name().unwrap_or_default().to_string_lossy(),"history":history,"history_truncated":history_truncated}),
    )?;
    let mut pending: std::collections::HashMap<u64, mpsc::UnboundedSender<GateChoice>> =
        std::collections::HashMap::new();
    let mut active = None;
    loop {
        tokio::select! {
            command = command_rx.recv() => match command {
                Some(Ok(Command::Send { run, text })) => {
                    if text.trim().is_empty() || text.len() > MAX_MESSAGE || run == 0 {
                        send_line(&json!({"type":"error","message":"message must be 1..65536 bytes and run must be positive"}))?;
                    } else if active.is_some() {
                        send_line(&json!({"type":"error","message":"a turn is already running"}))?;
                    } else {
                        pending.clear();
                        stopped.store(false, Ordering::SeqCst);
                        active = Some(run);
                        if !solo.send_desktop(run, text) { bail!("agent session closed"); }
                    }
                }
                Some(Ok(Command::Approve { id, choice })) => {
                    let choice = match choice.as_str() { "once" => GateChoice::Once, "session" => GateChoice::Session, _ => GateChoice::Deny };
                    if let Some(reply) = pending.remove(&id) { let _ = reply.send(choice); }
                }
                Some(Ok(Command::Stop)) => { stopped.store(true, Ordering::SeqCst); cancel.notify_waiters(); }
                Some(Ok(Command::ListSessions)) => {
                    let sessions = crate::session::recent(&crate::session::runs_root(), &workspace, Some(&dir))?;
                    send_line(&json!({"type":"sessions","items":sessions.into_iter().map(|s| json!({"id":s.id,"label":s.label})).collect::<Vec<_>>()}))?;
                }
                Some(Ok(Command::ListProfiles)) => {
                    let value = match crate::config::profiles(None) {
                        Ok(profiles) => json!({"type":"profiles","truncated":profiles.len() > 200,"items":profiles.into_iter().take(200).map(|(name, p)| json!({"name":name,"model":p.model})).collect::<Vec<_>>()}),
                        Err(error) => json!({"type":"profiles","error":error.to_string()}),
                    };
                    send_line(&value)?;
                }
                Some(Ok(Command::SetApproval { mode })) => {
                    if active.is_some() { send_line(&json!({"type":"settings_error","message":"finish or stop the current turn before changing mode"}))?; }
                    else {
                        session_allow.store(matches!(mode, ApprovalMode::AutoEdits), Ordering::SeqCst);
                        send_line(&json!({"type":"settings","approval":if session_allow.load(Ordering::SeqCst) { "auto-edits" } else { "ask" }}))?;
                    }
                }
                Some(Err(message)) => send_line(&json!({"type":"error","message":message}))?,
                None => break,
            },
            event = event_rx.recv() => match event {
                Some(event) => {
                    let (value, reply) = project_event(event);
                    if let Some((id, reply)) = reply { pending.insert(id, reply); }
                    let done = value["type"] == "run_done";
                    if done { active = None; pending.clear(); }
                    send_projected(&value)?;
                    if done { send_line(&json!({"type":"settings","approval":if session_allow.load(Ordering::SeqCst) { "auto-edits" } else { "ask" }}))?; }
                }
                None => break,
            },
        }
    }
    stopped.store(true, Ordering::SeqCst);
    cancel.notify_waiters();
    Ok(())
}
