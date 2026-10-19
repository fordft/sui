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

use crate::events::{GateChoice, PermissionScope, UiEvent};

const MAX_COMMAND: usize = 12 * 1024 * 1024;
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
    Send {
        run: u64,
        text: String,
        #[serde(default)]
        images: Vec<crate::desktop_images::Image>,
    },
    Approve {
        id: u64,
        choice: String,
    },
    Stop,
    ListSessions,
    ListProfiles,
    SetApproval {
        mode: ApprovalMode,
    },
}

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum ApprovalMode {
    Ask,
    AutoEdits,
    Yolo,
}

impl ApprovalMode {
    fn label(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::AutoEdits => "auto-edits",
            Self::Yolo => "yolo",
        }
    }
}
type PermissionReply = (u64, PermissionScope, mpsc::UnboundedSender<GateChoice>);
fn project_event(event: UiEvent) -> (Value, Option<PermissionReply>) {
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
            scope,
            reply,
        } => (
            json!({"type":"permission","run":run,"id":id,"agent":agent,"allow_session":scope == PermissionScope::Local,"allow_yolo":scope != PermissionScope::Independent,"summary":crate::provider::truncate(&summary, 64 * 1024)}),
            Some((id, scope, reply)),
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
    let resume_id = if resume == Some("latest") {
        crate::session::recent(&crate::session::runs_root(), &workspace, None)?
            .into_iter()
            .find(|s| s.profile.as_deref() == Some(profile))
            .map(|s| s.id)
    } else {
        resume.map(String::from)
    };
    let saved = resume_id
        .as_deref()
        .map(|id| {
            crate::session::SavedSession::load_desktop(&crate::session::runs_root(), id, &workspace)
        })
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
    if let Some(saved) = &saved {
        if let Some(enabled) = saved.header.image_input {
            prof.image_input = enabled;
        }
    } else if crate::config::profiles(None)?
        .get(profile)
        .and_then(|p| p.image_input)
        .is_none()
    {
        if let Some(modalities) = catalog_model.and_then(|m| m.input_modalities.as_ref()) {
            prof.image_input = modalities.iter().any(|m| m == "image");
        }
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
    let image_input = prof.image_input;
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
    let recovered = saved.as_ref().is_some_and(|s| s.recovered);
    drop(saved);
    let (command_tx, mut command_rx) = mpsc::channel::<Result<Command, String>>(4);
    std::thread::spawn(move || {
        use std::io::Read;
        let stdin = std::io::stdin();
        let mut reader = stdin.lock();
        loop {
            let mut line = Vec::new();
            let command = match reader
                .by_ref()
                .take(MAX_COMMAND as u64 + 1)
                .read_until(b'\n', &mut line)
            {
                Ok(0) => break,
                Ok(_) if line.len() <= MAX_COMMAND => {
                    serde_json::from_slice(&line).map_err(|_| "invalid desktop command".to_string())
                }
                Ok(_) => {
                    let _ = command_tx.blocking_send(Err("command exceeds 12 MiB".into()));
                    break;
                }
                Err(_) => {
                    let _ = command_tx.blocking_send(Err("desktop input closed".into()));
                    break;
                }
            };
            if command_tx.blocking_send(command).is_err() {
                break;
            }
        }
    });
    send_line(
        &json!({"type":"hello","version":env!("CARGO_PKG_VERSION"),"workspace":workspace,"profile":profile,"model":model,"model_name":model_name,"reasoning_levels":reasoning_levels,"catalog_warning":catalog.as_ref().and_then(|c| c.warning.as_ref()),"reasoning_effort":reasoning_effort,"reasoning_choices":reasoning_choices,"recovered":recovered,"approval":"ask","image_input":image_input,"kind":if chatgpt { "chatgpt" } else { "" },"run_id":dir.file_name().unwrap_or_default().to_string_lossy(),"history":history,"history_truncated":history_truncated}),
    )?;
    let mut pending: std::collections::HashMap<
        u64,
        (PermissionScope, mpsc::UnboundedSender<GateChoice>),
    > = std::collections::HashMap::new();
    let mut active = None;
    let mut input_open = true;
    let yolo = Arc::new(AtomicBool::new(false));
    let mut approval = ApprovalMode::Ask;
    loop {
        tokio::select! {
            command = command_rx.recv(), if input_open => match command {
                Some(Ok(Command::Send { run, text, images })) => {
                    if (text.trim().is_empty() && images.is_empty()) || text.len() > MAX_MESSAGE || run == 0 {
                        send_line(&json!({"type":"send_error","run":run,"message":"provide text or images, at most 65536 text bytes, and a positive run number"}))?;
                    } else if active.is_some() {
                        send_line(&json!({"type":"error","message":"a turn is already running"}))?;
                    } else {
                        let images = match crate::desktop_images::parts(images, image_input) {
                            Ok(images) => images,
                            Err(error) => { send_line(&json!({"type":"send_error","run":run,"message":error.to_string()}))?; continue; },
                        };
                        pending.clear();
                        stopped.store(false, Ordering::SeqCst);
                        active = Some(run);
                        if !solo.send_desktop(run, text, images, yolo.clone()) { bail!("agent session closed"); }
                    }
                }
                Some(Ok(Command::Approve { id, choice })) => {
                    if let Some((scope, reply)) = pending.remove(&id) {
                        let choice = match choice.as_str() {
                            "once" => GateChoice::Once,
                            "session" if scope == PermissionScope::Local => GateChoice::Session,
                            "session" => GateChoice::Once,
                            "yolo" if scope != PermissionScope::Independent => {
                                approval = ApprovalMode::Yolo;
                                session_allow.store(true, Ordering::SeqCst);
                                yolo.store(true, Ordering::SeqCst);
                                send_line(&json!({"type":"settings","approval":approval.label()}))?;
                                // The explicit click also grants future local commands.
                                // Independent surface permissions remain separate.
                                GateChoice::Once
                            },
                            _ => GateChoice::Deny,
                        };
                        let _ = reply.send(choice);
                        send_line(&json!({"type":"permission_resolved","id":id}))?;
                    }
                }
                Some(Ok(Command::Stop)) => { stopped.store(true, Ordering::SeqCst); cancel.notify_waiters(); }
                Some(Ok(Command::ListSessions)) => {
                    let sessions = crate::session::recent(&crate::session::runs_root(), &workspace, Some(&dir))?;
                    send_line(&json!({"type":"sessions","items":sessions.into_iter().map(|s| json!({"id":s.id,"label":s.label,"profile":s.profile})).collect::<Vec<_>>()}))?;
                }
                Some(Ok(Command::ListProfiles)) => {
                    let value = match crate::config::profiles(None) {
                        Ok(profiles) => json!({"type":"profiles","truncated":profiles.len() > 200,"items":profiles.into_iter().take(200).map(|(name, p)| json!({"name":name,"model":p.model})).collect::<Vec<_>>()}),
                        Err(error) => json!({"type":"profiles","error":error.to_string()}),
                    };
                    send_line(&value)?;
                }
                Some(Ok(Command::SetApproval { mode })) => {
                    if active.is_some() && pending.is_empty() {
                        send_line(&json!({"type":"settings_error","message":"change mode while idle or at an approval prompt"}))?;
                    } else {
                        approval = mode;
                        session_allow.store(mode != ApprovalMode::Ask, Ordering::SeqCst);
                        yolo.store(mode == ApprovalMode::Yolo, Ordering::SeqCst);
                        if mode != ApprovalMode::Ask {
                            let local: Vec<_> = pending.iter().filter(|(_, (scope, _))| *scope == PermissionScope::Local || (mode == ApprovalMode::Yolo && *scope == PermissionScope::Command)).map(|(id, _)| *id).collect();
                            for id in local { if let Some((_, reply)) = pending.remove(&id) { let _ = reply.send(GateChoice::Once); send_line(&json!({"type":"permission_resolved","id":id}))?; } }
                        }
                        send_line(&json!({"type":"settings","approval":approval.label()}))?;
                    }
                }
                Some(Err(message)) => send_line(&json!({"type":"error","message":message}))?,
                None => {
                    input_open = false;
                    stopped.store(true, Ordering::SeqCst);
                    cancel.notify_waiters();
                    if active.is_none() { break; }
                },
            },
            event = event_rx.recv() => match event {
                Some(event) => {
                    let (value, reply) = project_event(event);
                    if let Some((id, scope, reply)) = reply { pending.insert(id, (scope, reply)); }
                    let done = value["type"] == "run_done";
                    if done { active = None; pending.clear(); }
                    send_projected(&value)?;
                    if done {
                        if approval == ApprovalMode::Ask && session_allow.load(Ordering::SeqCst) { approval = ApprovalMode::AutoEdits; }
                        send_line(&json!({"type":"settings","approval":approval.label()}))?;
                        if !input_open { break; }
                    }
                }
                None => break,
            },
        }
    }
    stopped.store(true, Ordering::SeqCst);
    cancel.notify_waiters();
    Ok(())
}
