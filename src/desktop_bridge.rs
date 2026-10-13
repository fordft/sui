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
            json!({"type":"permission","run":run,"id":id,"agent":agent,"summary":summary}),
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
pub async fn run(workspace: &Path, profile: &str, resume: Option<&str>) -> Result<()> {
    let workspace = workspace
        .canonicalize()
        .context("workspace does not exist")?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory");
    }
    let prof = crate::config::resolve_profile(profile, None)?;
    let model = prof.model.clone();
    let saved = resume
        .map(|id| crate::session::SavedSession::load(&crate::session::runs_root(), id, &workspace))
        .transpose()?;
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
        session_allow,
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
        &json!({"type":"hello","version":env!("CARGO_PKG_VERSION"),"workspace":workspace,"profile":profile,"model":model,"run_id":dir.file_name().unwrap_or_default().to_string_lossy(),"history":history,"history_truncated":history_truncated}),
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
                        if !solo.send(run, text) { bail!("agent session closed"); }
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
                Some(Err(message)) => send_line(&json!({"type":"error","message":message}))?,
                None => break,
            },
            event = event_rx.recv() => match event {
                Some(event) => {
                    let (value, reply) = project_event(event);
                    if let Some((id, reply)) = reply { pending.insert(id, reply); }
                    if value["type"] == "run_done" { active = None; pending.clear(); }
                    send_projected(&value)?;
                }
                None => break,
            },
        }
    }
    stopped.store(true, Ordering::SeqCst);
    cancel.notify_waiters();
    Ok(())
}
