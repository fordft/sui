//! ACP client driver: one external coding-agent subprocess per session.
//!
//! Spawn hygiene is the trust boundary: the child starts with an EMPTY
//! environment plus a safe whitelist (and any `env_allow` names the user
//! configured), so Sui's provider keys and unrelated credentials can never
//! leak into an external agent. Each child is its own process-group leader
//! so cancellation can take down wrapper launchers (`npx`, `uvx`) too.
//!
//! Session lifetime = the `connect_with` closure: it loops on a command
//! channel, so follow-up prompts reuse the same session. On Close or
//! channel drop the closure returns, the transport ends, and the process
//! group gets a bounded wait then SIGKILL — protocol `session/cancel` is
//! always requested first for in-flight prompts.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agent_client_protocol::schema::v1::{
    CancelNotification, ContentBlock, Implementation, InitializeRequest, McpServer,
    NewSessionRequest, PromptRequest, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome, SessionConfigOption, SessionModeState,
    SessionNotification, SetSessionConfigOptionRequest, StopReason, TextContent,
};
use agent_client_protocol::schema::ProtocolVersion;
use agent_client_protocol::{Agent, ByteStreams, Client, ConnectionTo};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

use crate::config::AcpSpec;
use crate::journal::Journal;

use super::norm::{pick_option, Norm};

/// Environment names always safe to inherit — identity, locale, terminal,
/// XDG dirs (Devin/Codex auth live under HOME/XDG_CONFIG_HOME), and
/// corporate-proxy config. Anything key/token/secret-shaped is refused.
const ENV_BASE: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_STATE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    "NPM_CONFIG_USERCONFIG",
    // agents may run git inside their worktree
    "SSH_AUTH_SOCK",
    "GIT_SSH_COMMAND",
    "GIT_AUTHOR_NAME",
    "GIT_AUTHOR_EMAIL",
    "GIT_COMMITTER_NAME",
    "GIT_COMMITTER_EMAIL",
    // windows process baseline
    "SYSTEMROOT",
    "COMSPEC",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
];

fn secretish(name: &str) -> bool {
    let n = name.to_uppercase();
    n.contains("KEY")
        || n.contains("TOKEN")
        || n.contains("SECRET")
        || n.contains("PASS")
        || n.contains("CREDENTIAL")
        || n.starts_with("SUI_")
        || n.starts_with("AWS_")
        || n.starts_with("OPENAI")
        || n.starts_with("ANTHROPIC")
        || n.starts_with("OPENROUTER")
        || n.starts_with("DEEPSEEK")
        || n.starts_with("AZURE_")
        || n.starts_with("GCP_")
}

/// Resolved child environment: whitelist ∩ present, plus user-allowed
/// names minus anything secret-shaped. Returns (env, refused names).
pub fn filtered_env(spec: &AcpSpec) -> (Vec<(String, String)>, Vec<String>) {
    let mut env: Vec<(String, String)> = Vec::new();
    let mut refused = Vec::new();
    for name in ENV_BASE
        .iter()
        .copied()
        .chain(spec.env_allow.iter().map(|s| s.as_str()))
    {
        if secretish(name) {
            refused.push(name.to_string());
            continue;
        }
        if let Ok(v) = std::env::var(name) {
            if !env.iter().any(|(n, _)| n == name) {
                env.push((name.to_string(), v));
            }
        }
    }
    (env, refused)
}

/// How long an EOF'd/cancelled child gets before the process group is
/// SIGKILLed. Matches the "protocol cancel first, bounded kill second" rule.
const KILL_GRACE: Duration = Duration::from_secs(3);
/// initialize + session/new must complete promptly or the agent is wedged.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

pub struct PromptEnd {
    pub stop: StopReason,
    /// Raw `_meta` on the prompt response — journaled, never interpreted
    /// as per-request tokens (an ACP prompt is not one LLM request).
    pub meta: Option<Value>,
}

enum Cmd {
    Prompt {
        text: String,
        reply: oneshot::Sender<Result<PromptEnd>>,
    },
    Cancel,
    SetConfig {
        id: String,
        value: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Close,
}

/// Session-scoped artifact bridge config — attached via `session/new`.
#[derive(Clone)]
pub struct BridgeCfg {
    pub dir: std::path::PathBuf,
    /// plan | verdict | decision | any — the shape `submit_result` enforces.
    pub expect: String,
}

struct SessionInfo {
    id: Option<String>,
    #[allow(dead_code)]
    modes: Option<SessionModeState>,
    config_options: Vec<SessionConfigOption>,
}

struct DriverState {
    session: Option<SessionInfo>,
    /// Set once the transport/protocol died — a session whose side
    /// effects can no longer be observed must never be silently reused.
    err: Option<String>,
}

/// Cloneable session handle — the mission pool shares these; dropping the
/// last sender ends the session loop and tears the child down.
#[derive(Clone)]
pub struct AcpSession {
    pub key: String,
    cmd: mpsc::UnboundedSender<Cmd>,
    join: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
    state: Arc<Mutex<DriverState>>,
    dead: Arc<AtomicBool>,
}

impl AcpSession {
    /// Spawn the agent subprocess, run initialize + session/new, and start
    /// the command loop. `gate` is the same permission Gate mission uses.
    #[allow(clippy::too_many_arguments)]
    pub async fn spawn(
        key: String,
        spec: AcpSpec,
        cwd: &Path,
        bridge: Option<BridgeCfg>,
        norm: Arc<Mutex<Norm>>,
        gate: Arc<tokio::sync::Mutex<crate::permission::Gate>>,
        journal: Arc<Mutex<Journal>>,
        run: u64,
    ) -> Result<Self> {
        if !spec.approved {
            bail!("agent '{}' is not approved", spec.name);
        }
        let (env, refused) = filtered_env(&spec);
        let env_names: Vec<String> = env.iter().map(|(n, _)| n.clone()).collect();
        let (child_stdin, child_stdout, child_stderr, child) = spawn_scrubbed(&spec, cwd, env)
            .with_context(|| {
                format!(
                    "spawn agent '{}' ({} {}) — install it yourself, then set approved = true",
                    spec.name,
                    spec.command,
                    spec.args.join(" ")
                )
            })?;
        let pid = child.id();

        journal.lock().unwrap().log(
            "acp_spawn",
            json!({
                "agent": spec.name,
                "command": spec.command,
                "args": spec.args,
                "cwd": cwd,
                "pid": pid,
                // env names only — values never leave the child
                "env_names": env_names,
                "env_refused": refused,
                "model": spec.model,
            }),
        );

        let state = Arc::new(Mutex::new(DriverState {
            session: None,
            err: None,
        }));
        let dead = Arc::new(AtomicBool::new(false));
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();

        let (ready_tx, ready_rx) = oneshot::channel();
        let join = tokio::spawn(run_conn(ConnArgs {
            child,
            child_stdin,
            child_stdout,
            child_stderr,
            spec: spec.clone(),
            cwd: cwd.to_path_buf(),
            bridge,
            norm,
            gate,
            journal,
            state: state.clone(),
            dead: dead.clone(),
            cmd_rx,
            ready: ready_tx,
            run,
        }));

        // Don't hand out a session that isn't live: wait for
        // initialize + session/new (or a fast failure).
        match tokio::time::timeout(HANDSHAKE_TIMEOUT, ready_rx).await {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => bail!("acp handshake: {e}"),
            Ok(Err(_)) => bail!("acp agent died before session/new"),
            Err(_) => bail!("acp handshake timed out"),
        }

        Ok(Self {
            key,
            cmd: cmd_tx,
            join: Arc::new(Mutex::new(Some(join))),
            state,
            dead,
        })
    }

    /// Session id once `session/new` has completed.
    pub fn session_id(&self) -> Option<String> {
        self.state
            .lock()
            .unwrap()
            .session
            .as_ref()
            .and_then(|s| s.id.clone())
    }

    /// True once the transport died or the session loop ended.
    pub fn dead(&self) -> bool {
        self.dead.load(Ordering::Relaxed)
            || self
                .join
                .lock()
                .unwrap()
                .as_ref()
                .map(|h| h.is_finished())
                .unwrap_or(true)
    }

    /// Why the driver is dead, if it is.
    pub fn err(&self) -> Option<String> {
        self.state.lock().unwrap().err.clone()
    }

    /// Send one prompt on the live session. Resolves when the agent's
    /// turn ends (stop_reason in PromptEnd) — `end_turn` is a turn end,
    /// not mission-contract proof.
    pub async fn prompt(&self, text: &str) -> Result<PromptEnd> {
        if self.dead() {
            bail!("acp session dead: {}", self.err().unwrap_or_default());
        }
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::Prompt {
                text: text.to_string(),
                reply: tx,
            })
            .map_err(|_| anyhow!("acp session loop gone"))?;
        rx.await
            .map_err(|_| anyhow!("acp session dropped mid-prompt"))?
    }

    /// Ask the backend to cancel the in-flight prompt (protocol-first).
    pub fn cancel(&self) {
        let _ = self.cmd.send(Cmd::Cancel);
    }

    /// Apply a session-advertised config option (e.g. model selection).
    pub async fn set_config(&self, id: &str, value: &str) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd
            .send(Cmd::SetConfig {
                id: id.to_string(),
                value: value.to_string(),
                reply: tx,
            })
            .map_err(|_| anyhow!("acp session loop gone"))?;
        rx.await.map_err(|_| anyhow!("acp session dropped"))?
    }

    /// The session's advertised config option ids (for model pickers).
    pub fn config_options(&self) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .session
            .as_ref()
            .map(|s| s.config_options.iter().map(|o| o.id.to_string()).collect())
            .unwrap_or_default()
    }

    /// Graceful close → bounded wait → process-group kill. Other clones
    /// stay valid but see the session as dead afterwards.
    pub async fn shutdown(&self) {
        let _ = self.cmd.send(Cmd::Close);
        let join = self.join.lock().unwrap().take();
        if let Some(h) = join {
            let _ = tokio::time::timeout(KILL_GRACE + Duration::from_secs(2), h).await;
        }
    }
}

/// Spawn with a scrubbed environment and its own process group.
fn spawn_scrubbed(
    spec: &AcpSpec,
    cwd: &Path,
    env: Vec<(String, String)>,
) -> std::io::Result<(
    async_process::ChildStdin,
    async_process::ChildStdout,
    async_process::ChildStderr,
    async_process::Child,
)> {
    let mut c = std::process::Command::new(&spec.command);
    c.args(&spec.args).env_clear().envs(env).current_dir(cwd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // own group so the whole tree (incl. npx/uvx grandchildren) dies together
        c.process_group(0);
    }
    // stdio must be configured on the async_process::Command — the
    // std→async conversion does not carry it over.
    let mut cmd = async_process::Command::from(c);
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn()?;
    Ok((
        child.stdin.take().unwrap(),
        child.stdout.take().unwrap(),
        child.stderr.take().unwrap(),
        child,
    ))
}

/// Drain stderr into a bounded tail for crash diagnostics.
async fn drain_stderr(mut s: async_process::ChildStderr, tx: oneshot::Sender<String>) {
    use futures_util::AsyncReadExt;
    const CAP: usize = 16 * 1024;
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match s.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > CAP {
                    buf = buf.split_off(buf.len() - CAP);
                }
            }
        }
    }
    let _ = tx.send(String::from_utf8_lossy(&buf).to_string());
}

/// Bounded process-tree teardown: stdin EOF already delivered → short
/// grace → SIGKILL the whole group → reap. Returns the stderr tail.
async fn teardown(mut child: async_process::Child, stderr_rx: oneshot::Receiver<String>) -> String {
    #[cfg(unix)]
    let pid = child.id();
    let exited = tokio::time::timeout(KILL_GRACE, child.status()).await;
    if exited.is_err() {
        #[cfg(unix)]
        if let Some(p) = rustix::process::Pid::from_raw(pid.cast_signed()) {
            let _ = rustix::process::kill_process_group(p, rustix::process::Signal::KILL);
        }
        let _ = child.kill();
        let _ = child.status().await;
    }
    stderr_rx.await.unwrap_or_default()
}

struct ConnArgs {
    child: async_process::Child,
    child_stdin: async_process::ChildStdin,
    child_stdout: async_process::ChildStdout,
    child_stderr: async_process::ChildStderr,
    spec: AcpSpec,
    cwd: std::path::PathBuf,
    bridge: Option<BridgeCfg>,
    norm: Arc<Mutex<Norm>>,
    gate: Arc<tokio::sync::Mutex<crate::permission::Gate>>,
    journal: Arc<Mutex<Journal>>,
    state: Arc<Mutex<DriverState>>,
    dead: Arc<AtomicBool>,
    cmd_rx: mpsc::UnboundedReceiver<Cmd>,
    /// Signalled once initialize + session/new land (or the handshake
    /// fails) — spawn() must not return a session that isn't live yet.
    ready: oneshot::Sender<std::result::Result<(), String>>,
    run: u64,
}

async fn run_conn(a: ConnArgs) {
    let (stderr_tx, stderr_rx) = oneshot::channel::<String>();
    let stderr_task = tokio::spawn(drain_stderr(a.child_stderr, stderr_tx));

    let ConnArgs {
        child,
        child_stdin,
        child_stdout,
        child_stderr: _,
        spec,
        cwd,
        bridge,
        norm,
        gate,
        journal,
        state,
        dead,
        cmd_rx,
        ready,
        run,
    } = a;

    let streams = ByteStreams::new(child_stdin, child_stdout);
    let norm_n = norm.clone();
    let norm_p = norm.clone();
    let agent_label = format!("acp:{}", spec.name);
    let run_id = run;

    let result = Client
        .builder()
        // Every session/update → normalized activity + journal evidence.
        .on_receive_notification(
            async move |n: SessionNotification, _cx| {
                norm_n.lock().unwrap().on_update(&n.update);
                Ok(())
            },
            agent_client_protocol::on_receive_notification!(),
        )
        // session/request_permission → the mission Gate → outcome.
        .on_receive_request(
            async move |req: RequestPermissionRequest, responder, _cx| {
                let tc = &req.tool_call;
                let summary = format!(
                    "{:?}: {}",
                    tc.fields.kind.unwrap_or_default(),
                    tc.fields
                        .title
                        .clone()
                        .unwrap_or_else(|| tc.tool_call_id.to_string())
                );
                let choice = gate.lock().await.decide(&summary, &agent_label, run_id).await;
                norm_p.lock().unwrap().jlog(
                    "acp_permission",
                    json!({ "summary": summary, "decision": format!("{choice:?}"),
                            "options": req.options.iter().map(|o| json!({
                                "id": o.option_id.to_string(),
                                "kind": format!("{:?}", o.kind) })).collect::<Vec<_>>() }),
                );
                match pick_option(&req.options, choice) {
                    Some(id) => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(id)),
                    )),
                    None => responder.respond(RequestPermissionResponse::new(
                        RequestPermissionOutcome::Cancelled,
                    )),
                }
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_with(streams, {
            // teardown below still needs these — the closure owns clones
            let norm = norm.clone();
            let state = state.clone();
            let journal = journal.clone();
            let spec = spec.clone();
            let mut cmd_rx = cmd_rx;
            move |conn: ConnectionTo<Agent>| async move {
                // the SDK's wire error type; anyhow inside → mapped at the end
                type AcpErr = agent_client_protocol::Error;
                let mut ready = Some(ready);

                // ── handshake ─────────────────────────────────────────
                // ClientCapabilities::default() = fs off, terminal off:
                // the agent uses ITS OWN tools; Sui never executes agent
                // fs/terminal ops. Ownership is enforced by the diff gate.
                let hs = async {
                    let mut init = InitializeRequest::new(ProtocolVersion::V1);
                    init.client_info =
                        Some(Implementation::new("sui", env!("CARGO_PKG_VERSION")));
                    let init_resp = tokio::time::timeout(
                        HANDSHAKE_TIMEOUT,
                        conn.send_request(init).block_task(),
                    )
                    .await
                    .map_err(|_| AcpErr::new(-32603, "acp initialize timed out"))??;

                    let mut new = NewSessionRequest::new(cwd.clone());
                    if let Some(b) = &bridge {
                        std::fs::create_dir_all(&b.dir).ok();
                        // In production the bridge is our own binary
                        // (`sui acp-bridge`); under cargo test current_exe
                        // is the test binary, so an env override points at
                        // the real sui binary.
                        let exe = std::env::var_os("SUI_ACP_BRIDGE_EXE")
                            .map(std::path::PathBuf::from)
                            .or_else(|| std::env::current_exe().ok())
                            .ok_or_else(|| {
                                AcpErr::new(-32603, "no bridge executable")
                            })?;
                        let mut mcp =
                            agent_client_protocol::schema::v1::McpServerStdio::new(
                                "sui-artifacts",
                                exe,
                            );
                        mcp.args = vec![
                            "acp-bridge".to_string(),
                            "--dir".to_string(),
                            b.dir.to_string_lossy().to_string(),
                            "--expect".to_string(),
                            b.expect.clone(),
                        ];
                        new.mcp_servers.push(McpServer::Stdio(mcp));
                    }
                    let sess = tokio::time::timeout(
                        HANDSHAKE_TIMEOUT,
                        conn.send_request(new).block_task(),
                    )
                    .await
                    .map_err(|_| AcpErr::new(-32603, "acp session/new timed out"))??;
                    Ok::<_, AcpErr>((init_resp, sess))
                }
                .await;
                let (init_resp, sess) = match hs {
                    Ok(x) => {
                        if let Some(t) = ready.take() {
                            let _ = t.send(Ok(()));
                        }
                        x
                    }
                    Err(e) => {
                        if let Some(t) = ready.take() {
                            let _ = t.send(Err(format!("{e}")));
                        }
                        return Err(e);
                    }
                };

                let session_id = sess.session_id.clone();
                let config_options = sess.config_options.clone().unwrap_or_default();
                {
                    let mut st = state.lock().unwrap();
                    st.session = Some(SessionInfo {
                        id: Some(session_id.to_string()),
                        modes: sess.modes.clone(),
                        config_options: config_options.clone(),
                    });
                }
                journal.lock().unwrap().log(
                    "acp_session",
                    json!({
                        "session_id": session_id.to_string(),
                        "protocol": format!("{:?}", init_resp.protocol_version),
                        "agent_info": init_resp.agent_info.map(|i| json!({
                            "name": i.name, "version": i.version })),
                        "capabilities": {
                            "load_session": init_resp.agent_capabilities.load_session,
                            "mcp_http": init_resp.agent_capabilities.mcp_capabilities.http,
                            "mcp_sse": init_resp.agent_capabilities.mcp_capabilities.sse,
                        },
                        "modes": sess.modes.as_ref().map(|m| m.available_modes.iter()
                            .map(|m| m.id.to_string()).collect::<Vec<_>>()),
                        "config_options": config_options.iter()
                            .map(|o| json!({ "id": o.id.to_string(), "name": o.name,
                                "category": o.category.as_ref().map(|c| format!("{c:?}")) }))
                            .collect::<Vec<_>>(),
                        "client_caps": { "fs": "off", "terminal": "off" },
                    }),
                );
                norm.lock().unwrap().phase(format!(
                    "acp:{} session {} (fs/terminal off; perms via gate)",
                    spec.name, session_id
                ));

                // Preferred model via session-advertised config option —
                // "session-advertised configuration where supported".
                if let Some(m) = &spec.model {
                    if let Some(o) = config_options.iter().find(|o| matches!(
                        o.category.as_ref(),
                        Some(agent_client_protocol::schema::v1::SessionConfigOptionCategory::Model)
                    )) {
                        let _ = conn
                            .send_request(SetSessionConfigOptionRequest::new(
                                session_id.clone(),
                                o.id.clone(),
                                m.as_str(),
                            ))
                            .block_task()
                            .await;
                    }
                }

                // ── session loop: one prompt at a time, session-scoped ─
                // The in-flight prompt and the command channel are both
                // polled: session/cancel MUST reach the wire while a
                // prompt is running — that is the whole point of cancel.
                type PromptFut = std::pin::Pin<
                    Box<
                        dyn std::future::Future<
                                Output = Result<
                                    agent_client_protocol::schema::v1::PromptResponse,
                                    AcpErr,
                                >,
                            > + Send,
                    >,
                >;
                let mut pending: Option<(
                    Instant,
                    oneshot::Sender<Result<PromptEnd>>,
                    PromptFut,
                )> = None;
                loop {
                    tokio::select! {
                        r = async { pending.as_mut().unwrap().2.as_mut().await }, if pending.is_some() => {
                            let (t0, reply, _) = pending.take().unwrap();
                            let ms = t0.elapsed().as_millis();
                            let end = match r {
                                Ok(resp) => {
                                    let ok = matches!(resp.stop_reason, StopReason::EndTurn);
                                    norm.lock().unwrap().prompt_done(
                                        ms,
                                        ok,
                                        &format!("{:?}", resp.stop_reason),
                                    );
                                    Ok(PromptEnd {
                                        stop: resp.stop_reason,
                                        meta: resp.meta.map(Value::Object),
                                    })
                                }
                                Err(e) => {
                                    norm.lock().unwrap().prompt_done(ms, false, "error");
                                    if agent_client_protocol::is_incoming_transport_closed(&e) {
                                        // the wire is gone — the session's side
                                        // effects are unobservable from here;
                                        // exit the loop so teardown+poison run
                                        let _ = reply.send(Err(anyhow!(
                                            "acp transport closed mid-prompt: {e}"
                                        )));
                                        return Err(e);
                                    }
                                    Err(anyhow!("acp prompt: {e}"))
                                }
                            };
                            let _ = reply.send(end);
                        }
                        cmd = cmd_rx.recv() => match cmd {
                            None => break,
                            Some(Cmd::Cancel) => {
                                let _ = conn.send_notification(
                                    CancelNotification::new(session_id.clone()));
                            }
                            Some(Cmd::Prompt { text, reply }) => {
                                if pending.is_some() {
                                    let _ = reply.send(Err(anyhow!("prompt already in flight")));
                                    continue;
                                }
                                norm.lock().unwrap().prompt_start();
                                pending = Some((
                                    Instant::now(),
                                    reply,
                                    Box::pin(conn.send_request(PromptRequest::new(
                                        session_id.clone(),
                                        vec![ContentBlock::Text(TextContent::new(text))],
                                    )).block_task()),
                                ));
                            }
                            Some(Cmd::SetConfig { id, value, reply }) => {
                                let r = conn
                                    .send_request(SetSessionConfigOptionRequest::new(
                                        session_id.clone(),
                                        id,
                                        value.as_str(),
                                    ))
                                    .block_task()
                                    .await
                                    .map(|_| ())
                                    .map_err(|e| anyhow!("set_config_option: {e}"));
                                let _ = reply.send(r);
                            }
                            Some(Cmd::Close) => break,
                        }
                    }
                }
                Ok::<(), AcpErr>(())
            }
        })
        .await;

    // ── teardown: transport ended → stdin EOF → grace → kill group ──
    let stderr = teardown(child, stderr_rx).await;
    let _ = stderr_task.await;
    dead.store(true, Ordering::Relaxed);
    if let Err(e) = &result {
        let tail = stderr.trim();
        let msg = if tail.is_empty() {
            format!("{e:#}")
        } else {
            format!(
                "{e:#}\nstderr: {}",
                &tail[crate::context::floor_char_boundary(tail, tail.len().saturating_sub(2000))..]
            )
        };
        state.lock().unwrap().err = Some(msg.clone());
        norm.lock()
            .unwrap()
            .jlog("acp_error", json!({ "error": msg }));
    }
    journal
        .lock()
        .unwrap()
        .log("acp_exit", json!({ "agent": spec.name }));
}
