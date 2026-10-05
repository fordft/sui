//! Regular tests need no browser/download. The ignored test exercises real
//! Chromium + PTY on headless Linux, explicitly opting into runtime setup.
mod common;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::Duration;
use sui::agent::{Agent, Identity, Limits};
use sui::events::{GateChoice, UiEvent};
use sui::tools::{
    self,
    ui::{BrowserCfg, UiService},
    ExecKind, ToolContext,
};
use sui::web::{WebAccess, WebCfg, WebService};

fn workspace() -> PathBuf {
    let tag = format!(
        "sui-ui-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let path = std::env::temp_dir().join(tag);
    std::fs::create_dir_all(&path).unwrap();
    path
}
fn context(path: PathBuf, approved: bool) -> ToolContext {
    let ctx = ToolContext {
        workspace: path,
        bash_timeout: Duration::from_secs(5),
        bash_timeout_max: Duration::from_secs(5),
        web: None,
        canon_root: std::sync::OnceLock::new(),
        ui: std::sync::OnceLock::new(),
        code_intel: Default::default(),
        code_context: Default::default(),
        tool_outputs: Default::default(),
    };
    ctx.ui
        .set(UiService::new(BrowserCfg {
            approved,
            ..Default::default()
        }))
        .ok();
    ctx
}
fn png() -> Vec<u8> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVQIHWP4z8DwHwAFgAI/ScLbtAAAAABJRU5ErkJggg==").unwrap()
}

fn consent_response(call: Option<Value>) -> String {
    let response = if let Some(mut call) = call {
        call["index"] = json!(0);
        json!({"choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[call]},"finish_reason":"tool_calls"}]})
    } else {
        json!({"choices":[{"index":0,"delta":{"role":"assistant","content":"consent flow finished"},"finish_reason":"stop"}]})
    };
    format!("data: {response}\n\ndata: [DONE]\n\n")
}

fn rejected_web_call(id: &str) -> Value {
    // The production URL guard rejects this before opening the web backend.
    common::tc(
        id,
        "web_fetch",
        &json!({"url":"http://127.0.0.1/consent-proof"}).to_string(),
    )
}

fn consent_web(access: WebAccess) -> Arc<WebService> {
    WebService::new(WebCfg {
        access,
        api_key: None,
        endpoint: "not-a-network-endpoint".into(),
    })
}

fn consent_agent(
    path: &std::path::Path,
    journal_name: &str,
    port: u16,
    auto: bool,
    web: Arc<WebService>,
    session: Arc<AtomicBool>,
) -> (Agent, tokio::sync::mpsc::UnboundedReceiver<UiEvent>) {
    let mut ctx = context(path.to_path_buf(), false);
    ctx.web = Some(web);
    let mut agent = Agent::new(
        sui::provider::Provider::new(
            &format!("http://127.0.0.1:{port}"),
            None,
            "consent-mock".into(),
            None,
        ),
        ctx,
        sui::permission::Gate::new(auto),
        sui::journal::Journal::open(&path.join(journal_name)).unwrap(),
        Limits {
            max_turns: 8,
            context_budget: 50000,
            context_reserve: 1000,
            compact_context: false,
            request_timeout: Duration::from_secs(5),
        },
        Identity {
            session_id: journal_name.into(),
            agent_id: journal_name.into(),
            role: "worker".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            model: "consent-mock".into(),
            cache_key_fingerprint: None,
        },
    );
    agent.set_quiet(true);
    let (events, receiver) = tokio::sync::mpsc::unbounded_channel();
    agent.wire_ui(
        events,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(AtomicBool::new(false)),
        Some(session),
    );
    (agent, receiver)
}

async fn consent_turn(
    agent: &mut Agent,
    receiver: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
    task: &str,
    choices: &[GateChoice],
) -> Vec<String> {
    let mut prompts = Vec::new();
    let drive = agent.run_turn(task);
    tokio::pin!(drive);
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            tokio::select! {
                result = &mut drive => {
                    result.unwrap();
                    return;
                }
                event = receiver.recv() => {
                    let event = event.expect("native Agent event channel remains live");
                    if let UiEvent::Permission { summary, reply, .. } = event {
                        let choice = choices.get(prompts.len()).copied().unwrap_or(GateChoice::Deny);
                        prompts.push(summary);
                        reply.send(choice).unwrap();
                    }
                }
            }
        }
    })
    .await
    .expect("bounded consent flow");
    prompts
}

fn consent_tool_result<'a>(agent: &'a Agent, id: &str) -> &'a str {
    agent
        .history()
        .iter()
        .find_map(|message| match message {
            sui::types::Message::Tool {
                tool_call_id,
                content,
            } if tool_call_id == id => Some(content.as_str()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing tool result {id}"))
}

#[tokio::test]
async fn image_dispatch_is_multimodal_and_workspace_bounded() {
    let ctx = context(workspace(), false);
    std::fs::write(ctx.workspace.join("tiny.png"), png()).unwrap();
    let out = tools::execute(
        &ctx,
        "view_image",
        &json!({"path":"tiny.png"}),
        std::future::pending(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.kind, ExecKind::Success);
    assert!(!out.text.contains("base64"));
    let value = serde_json::to_value(out.image.unwrap()).unwrap();
    assert_eq!(value[1]["type"], "image_url");
    assert!(value[1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));
    assert!(tools::execute(
        &ctx,
        "view_image",
        &json!({"path":"../outside.png"}),
        std::future::pending(),
        None
    )
    .await
    .is_err());
    std::fs::write(ctx.workspace.join("fake.png"), b"plain text").unwrap();
    assert!(tools::execute(
        &ctx,
        "view_image",
        &json!({"path":"fake.png"}),
        std::future::pending(),
        None
    )
    .await
    .is_err());
    std::fs::write(
        ctx.workspace.join("large.png"),
        vec![0; 4 * 1024 * 1024 + 1],
    )
    .unwrap();
    assert!(tools::execute(
        &ctx,
        "view_image",
        &json!({"path":"large.png"}),
        std::future::pending(),
        None
    )
    .await
    .is_err());
    std::fs::remove_dir_all(ctx.workspace).unwrap();
}

#[tokio::test]
async fn ui_requires_independent_consent_before_any_setup() {
    let ctx = context(workspace(), false);
    for name in ["browser", "terminal"] {
        let out = tools::execute(
            &ctx,
            name,
            &json!({"action":"screenshot"}),
            std::future::pending(),
            None,
        )
        .await
        .unwrap();
        assert!(out.text.starts_with("status: denied"));
        assert!(out.image.is_none());
    }
    std::fs::remove_dir_all(ctx.workspace).unwrap();
}

#[tokio::test]
async fn native_web_ask_still_prompts_under_yolo() {
    let path = workspace();
    let port = common::serve(|_, messages| {
        let call = (!messages.iter().any(|message| message["role"] == "tool"))
            .then(|| rejected_web_call("web-yolo"));
        consent_response(call)
    });
    let session = Arc::new(AtomicBool::new(false));
    let web = consent_web(WebAccess::Ask);
    let (mut agent, mut receiver) =
        consent_agent(&path, "web-yolo", port, true, web.clone(), session.clone());
    let prompts = consent_turn(
        &mut agent,
        &mut receiver,
        "Check web consent under local auto approval.",
        &[GateChoice::Deny],
    )
    .await;
    assert_eq!(prompts.len(), 1, "YOLO must not authorize web Ask");
    assert!(prompts[0].starts_with("web — leaves this machine:"));
    assert!(consent_tool_result(&agent, "web-yolo").starts_with("status: denied\n"));
    assert!(!session.load(Ordering::Relaxed));
    assert!(web.sources().is_empty());
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn native_local_session_approval_does_not_skip_web_ask() {
    let path = workspace();
    let port = common::serve(|_, messages| {
        let tools = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .count();
        let call = match tools {
            0 => Some(common::tc(
                "local-session",
                "write_file",
                &json!({"path":"local-approved.txt","content":"legitimate local approval"})
                    .to_string(),
            )),
            1 => Some(rejected_web_call("web-after-local")),
            _ => None,
        };
        consent_response(call)
    });
    let session = Arc::new(AtomicBool::new(false));
    let (mut agent, mut receiver) = consent_agent(
        &path,
        "local-before-web",
        port,
        false,
        consent_web(WebAccess::Ask),
        session.clone(),
    );
    let prompts = consent_turn(
        &mut agent,
        &mut receiver,
        "Approve a local session, then check independent web consent.",
        &[GateChoice::Session, GateChoice::Deny],
    )
    .await;
    assert_eq!(prompts.len(), 2, "local Session must not authorize web Ask");
    assert_eq!(prompts[0], "write local-approved.txt");
    assert!(prompts[1].starts_with("web — leaves this machine:"));
    assert!(
        session.load(Ordering::Relaxed),
        "legitimate local Session persists"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("local-approved.txt")).unwrap(),
        "legitimate local approval"
    );
    assert!(consent_tool_result(&agent, "web-after-local").starts_with("status: denied\n"));
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn native_web_session_is_agent_owned_and_does_not_approve_local_writes() {
    let path = workspace();
    let port = common::serve(|_, messages| {
        let tools = messages
            .iter()
            .filter(|message| message["role"] == "tool")
            .count();
        let last_user = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .unwrap();
        let call = if last_user.contains("reuse same agent") {
            (tools == 3).then(|| rejected_web_call("web-later-turn"))
        } else {
            match tools {
                0 => Some(rejected_web_call("web-session-first")),
                1 => Some(rejected_web_call("web-session-second")),
                2 => Some(common::tc(
                    "local-after-web",
                    "write_file",
                    &json!({"path":"must-not-be-written.txt","content":"unapproved"}).to_string(),
                )),
                _ => None,
            }
        };
        consent_response(call)
    });
    let session = Arc::new(AtomicBool::new(false));
    let web = consent_web(WebAccess::Ask);
    let (mut agent, mut receiver) = consent_agent(
        &path,
        "web-session-owner",
        port,
        false,
        web.clone(),
        session.clone(),
    );
    let prompts = consent_turn(
        &mut agent,
        &mut receiver,
        "Grant web session consent, then check a local write.",
        &[GateChoice::Session, GateChoice::Deny],
    )
    .await;
    assert_eq!(prompts.len(), 2, "web Session applies only to web requests");
    assert!(prompts[0].starts_with("web — leaves this machine:"));
    assert_eq!(prompts[1], "write must-not-be-written.txt");
    assert!(
        !session.load(Ordering::Relaxed),
        "web Session must not raise local AUTO"
    );
    assert!(!path.join("must-not-be-written.txt").exists());
    for id in ["web-session-first", "web-session-second"] {
        assert!(consent_tool_result(&agent, id).contains("error: rejected:"));
    }
    assert!(consent_tool_result(&agent, "local-after-web").starts_with("status: denied\n"));
    let earlier_history = serde_json::to_value(agent.history()).unwrap();
    let later_prompts = consent_turn(
        &mut agent,
        &mut receiver,
        "reuse same agent web consent in a later turn",
        &[],
    )
    .await;
    assert!(
        later_prompts.is_empty(),
        "same native agent keeps web Session"
    );
    assert!(consent_tool_result(&agent, "web-later-turn").contains("error: rejected:"));
    let current_history = serde_json::to_value(agent.history()).unwrap();
    let earlier = earlier_history.as_array().unwrap();
    assert_eq!(
        &current_history.as_array().unwrap()[..earlier.len()],
        earlier
    );

    let other_port = common::serve(|_, messages| {
        let call = (!messages.iter().any(|message| message["role"] == "tool"))
            .then(|| rejected_web_call("web-other-agent"));
        consent_response(call)
    });
    let (mut other, mut other_receiver) = consent_agent(
        &path,
        "web-session-other",
        other_port,
        false,
        web.clone(),
        Arc::new(AtomicBool::new(false)),
    );
    let other_prompts = consent_turn(
        &mut other,
        &mut other_receiver,
        "An independent agent must obtain its own web consent.",
        &[GateChoice::Deny],
    )
    .await;
    assert_eq!(
        other_prompts.len(),
        1,
        "shared WebService must not share consent"
    );
    assert!(other_prompts[0].starts_with("web — leaves this machine:"));
    assert!(consent_tool_result(&other, "web-other-agent").starts_with("status: denied\n"));
    assert!(web.sources().is_empty());
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn persistent_solo_web_policy_revocation_preserves_history_and_requires_fresh_consent() {
    async fn collect_run(
        receiver: &mut tokio::sync::mpsc::UnboundedReceiver<UiEvent>,
        expected_run: u64,
        choices: &[GateChoice],
    ) -> (Vec<String>, Vec<(String, sui::events::ToolStatus, String)>) {
        tokio::time::timeout(Duration::from_secs(8), async {
            let mut prompts = Vec::new();
            let mut results = Vec::new();
            loop {
                match receiver.recv().await.expect("persistent Solo remains live") {
                    UiEvent::Permission {
                        run,
                        summary,
                        reply,
                        ..
                    } => {
                        assert_eq!(run, expected_run);
                        let choice = choices
                            .get(prompts.len())
                            .copied()
                            .unwrap_or(GateChoice::Deny);
                        prompts.push(summary);
                        reply.send(choice).unwrap();
                    }
                    UiEvent::ToolDone {
                        run,
                        call,
                        status,
                        result,
                        ..
                    } => {
                        assert_eq!(run, expected_run);
                        results.push((call, status, result));
                    }
                    UiEvent::RunDone { run, outcome, .. } => {
                        assert_eq!(run, expected_run);
                        assert_eq!(outcome, "done");
                        return (prompts, results);
                    }
                    UiEvent::Error { msg, .. } => panic!("Solo error: {msg}"),
                    _ => {}
                }
            }
        })
        .await
        .expect("bounded Solo consent run")
    }

    let path = workspace();
    std::fs::write(
        path.join("sui.toml"),
        "[agent]\ncontext_compaction = false\nmax_turns = 4\n",
    )
    .unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let captured = requests.clone();
    let port = common::serve(move |body, messages| {
        captured
            .lock()
            .unwrap()
            .push(serde_json::from_slice(body).unwrap());
        let task = messages
            .iter()
            .rev()
            .find(|message| message["role"] == "user")
            .and_then(|message| message["content"].as_str())
            .unwrap();
        let id = if task.contains("revoked") {
            "solo-web-off"
        } else if task.contains("restored") {
            "solo-web-restored"
        } else {
            "solo-web-first"
        };
        let answered = messages
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == id);
        consent_response((!answered).then(|| rejected_web_call(id)))
    });
    let profile = sui::config::Profile {
        transport: Default::default(),
        name: "solo-consent-mock".into(),
        base_url: format!("http://127.0.0.1:{port}"),
        model: "consent-mock".into(),
        api_key: None,
        prompt_cache_key: None,
        pricing: None,
        image_input: false,
    };
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let session = Arc::new(AtomicBool::new(false));
    let initial_web = consent_web(WebAccess::Ask);
    let solo = sui::tui::spawn_solo(
        profile,
        path.clone(),
        path.join("solo-run"),
        events,
        Arc::new(tokio::sync::Notify::new()),
        Arc::new(AtomicBool::new(false)),
        session.clone(),
        Some(initial_web.clone()),
    );
    assert!(solo.send_with_web(1, "initial web consent".into(), Some(initial_web)));
    let (initial_prompts, initial_results) =
        collect_run(&mut receiver, 1, &[GateChoice::Session]).await;
    assert_eq!(initial_prompts.len(), 1);
    assert!(initial_prompts[0].starts_with("web — leaves this machine:"));
    assert_eq!(initial_results.len(), 1);
    assert_eq!(initial_results[0].0, "solo-web-first");
    assert_eq!(initial_results[0].1, sui::events::ToolStatus::Error);
    assert!(initial_results[0].2.contains("error: rejected:"));
    assert!(!session.load(Ordering::Relaxed));

    assert!(solo.send_with_web(
        2,
        "web access revoked between turns".into(),
        Some(consent_web(WebAccess::Off)),
    ));
    let (off_prompts, off_results) = collect_run(&mut receiver, 2, &[]).await;
    assert!(
        off_prompts.is_empty(),
        "Off denies without opening a prompt"
    );
    assert_eq!(off_results.len(), 1);
    assert_eq!(off_results[0].0, "solo-web-off");
    assert_eq!(off_results[0].1, sui::events::ToolStatus::Denied);
    assert!(off_results[0].2.contains("web research is Off"));

    assert!(solo.send_with_web(
        3,
        "web access restored as Ask".into(),
        Some(consent_web(WebAccess::Ask)),
    ));
    let (restored_prompts, restored_results) =
        collect_run(&mut receiver, 3, &[GateChoice::Deny]).await;
    assert_eq!(
        restored_prompts.len(),
        1,
        "replacement Ask needs fresh consent"
    );
    assert!(restored_prompts[0].starts_with("web — leaves this machine:"));
    assert_eq!(restored_results.len(), 1);
    assert_eq!(restored_results[0].0, "solo-web-restored");
    assert_eq!(restored_results[0].1, sui::events::ToolStatus::Denied);
    assert!(!session.load(Ordering::Relaxed));

    let requests = requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        6,
        "each turn has a call and its final response"
    );
    for pair in requests.windows(2) {
        assert_eq!(pair[0]["tools"], pair[1]["tools"], "schemas remain frozen");
        let earlier = pair[0]["messages"].as_array().unwrap();
        let later = pair[1]["messages"].as_array().unwrap();
        assert_eq!(
            &later[..earlier.len()],
            earlier,
            "policy updates preserve history"
        );
    }
    for id in ["solo-web-first", "solo-web-off", "solo-web-restored"] {
        assert!(requests.last().unwrap()["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["role"] == "tool" && message["tool_call_id"] == id));
    }
    drop(solo);
    tokio::time::timeout(Duration::from_secs(2), async {
        while receiver.recv().await.is_some() {}
    })
    .await
    .expect("Solo worker closes after its sender is dropped");
    std::fs::remove_dir_all(path).unwrap();
}

#[tokio::test]
async fn agent_appends_images_after_all_sibling_tool_results_without_journaling_bytes() {
    for supports_images in [false, true] {
        let path = workspace();
        std::fs::write(path.join("tiny.png"), png()).unwrap();
        let inspected = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen = inspected.clone();
        let port = common::serve(move |_raw, messages| {
            if messages.iter().any(|m| m["role"] == "tool") {
                let tools: Vec<usize> = messages
                    .iter()
                    .enumerate()
                    .filter_map(|(i, m)| (m["role"] == "tool").then_some(i))
                    .collect();
                assert_eq!(tools.len(), 2);
                let image = messages.last().unwrap();
                if supports_images {
                    assert_eq!(image["role"], "user", "messages: {messages:?}");
                    assert_eq!(image["content"][1]["type"], "image_url");
                    assert!(tools[1] < messages.len() - 1);
                } else {
                    assert_eq!(image["role"], "tool");
                    assert!(messages[tools[0]]["content"]
                        .as_str()
                        .unwrap()
                        .contains("no declared image input"));
                }
                seen.store(true, std::sync::atomic::Ordering::Relaxed);
                common::sse_text("state: flow-verified\nverified: image arrived\nunverified: none")
            } else {
                let mut calls = json!([
                    common::tc("image", "view_image", "{\"path\":\"tiny.png\"}"),
                    common::tc("read", "read_file", "{\"path\":\"tiny.png\"}")
                ]);
                calls[0]["index"] = json!(0);
                calls[1]["index"] = json!(1);
                common::sse_tool_calls(calls)
            }
        });
        let run = path.join("run");
        let mut agent = sui::agent::Agent::new(
            sui::provider::Provider::new(
                &format!("http://127.0.0.1:{port}"),
                None,
                "vision-mock".into(),
                None,
            )
            .with_image_input(supports_images),
            context(path.clone(), false),
            sui::permission::Gate::new(true),
            sui::journal::Journal::open(&run).unwrap(),
            sui::agent::Limits {
                max_turns: 3,
                context_budget: 50000,
                context_reserve: 1000,
                compact_context: false,
                request_timeout: Duration::from_secs(10),
            },
            sui::agent::Identity {
                session_id: "ui-test".into(),
                agent_id: "worker".into(),
                role: "worker".into(),
                base_url: "mock".into(),
                model: "vision-mock".into(),
                cache_key_fingerprint: None,
            },
        );
        agent.run_turn("inspect the image").await.unwrap();
        assert!(inspected.load(std::sync::atomic::Ordering::Relaxed));
        let journal = std::fs::read_to_string(run.join("events.jsonl")).unwrap();
        assert!(!journal.contains("data:image/png;base64"));
        assert_eq!(journal.contains("image_observation"), supports_images);
        std::fs::remove_dir_all(path).unwrap();
    }
}

#[tokio::test]
#[ignore = "real headless browser/PTY; may download pinned UI dependencies"]
async fn real_headless_browser_and_terminal_flow() {
    use std::io::{Read, Write};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    let listener = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let saved = Arc::new(Mutex::new(String::new()));
    let state = saved.clone();
    let leaked = Arc::new(AtomicUsize::new(0));
    let leakage = leaked.clone();
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let mut socket = socket.unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut raw = Vec::new();
            let mut chunk = [0; 4096];
            let header_end = loop {
                let n = socket.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break None;
                }
                raw.extend_from_slice(&chunk[..n]);
                if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(i + 4);
                }
            };
            let Some(header_end) = header_end else {
                continue;
            };
            let header = String::from_utf8_lossy(&raw[..header_end]).to_string();
            if header.to_lowercase().contains("host: 127.0.0.2") {
                leakage.fetch_add(1, Ordering::Relaxed);
            }
            let length = header
                .lines()
                .find_map(|l| {
                    l.to_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while raw.len() < header_end + length {
                let n = socket.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                raw.extend_from_slice(&chunk[..n]);
            }
            let (status, extra, body) = if header.starts_with("GET /redirect") {
                (
                    "302 Found",
                    format!("Location: http://127.0.0.2:{port}/external\r\n"),
                    String::new(),
                )
            } else if header.starts_with("POST /save") {
                *state.lock().unwrap() = String::from_utf8_lossy(&raw[header_end..]).into_owned();
                ("200 OK", String::new(), "saved".into())
            } else {
                let previous = state.lock().unwrap().clone();
                let body = format!(
                    r#"<!doctype html><html><body style="font:24px sans-serif;padding:32px"><h1>Sui headless proof</h1><label>Title <input id="title"></label><button id="save">Save</button><p id="saved">{previous}</p><div data-sui-private>UI_SECRET_MARKER</div><script>
                document.querySelector('#save').onclick=async()=>{{let text=document.querySelector('#title').value;await fetch('/save',{{method:'POST',body:text}});document.querySelector('#saved').textContent=text}};
                fetch('http://127.0.0.2:{port}/external').catch(()=>{{}});
                new WebSocket('ws://127.0.0.2:{port}/external');
                </script></body></html>"#
                );
                ("200 OK", String::new(), body)
            };
            let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            let _ = socket.write_all(response.as_bytes());
        }
    });
    let ctx = context(workspace(), true);
    let svc = ctx.ui.get().unwrap();
    async fn call(svc: &UiService, ctx: &ToolContext, tool: &str, args: Value) -> tools::ExecOut {
        let result = svc
            .execute(ctx, tool, &args, std::future::pending())
            .await
            .unwrap();
        assert_eq!(
            result.kind,
            ExecKind::Success,
            "{tool} {args}: {}",
            result.text
        );
        result
    }
    let url = format!("http://127.0.0.1:{port}");
    let initial = call(svc, &ctx, "browser", json!({"action":"open","url":url})).await;
    assert!(!initial.text.contains("UI_SECRET_MARKER"));
    call(
        svc,
        &ctx,
        "browser",
        json!({"action":"fill","role":"textbox","name":"Title","text":"Vision works"}),
    )
    .await;
    call(
        svc,
        &ctx,
        "browser",
        json!({"action":"click","role":"button","name":"Save"}),
    )
    .await;
    for _ in 0..20 {
        if *saved.lock().unwrap() == "Vision works" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(*saved.lock().unwrap(), "Vision works");
    let reload = call(svc, &ctx, "browser", json!({"action":"open","url":url})).await;
    assert!(reload.text.contains("Vision works"), "{}", reload.text);
    let shot = call(
        svc,
        &ctx,
        "browser",
        json!({"action":"screenshot","path":"web-proof.png"}),
    )
    .await;
    assert!(shot.image.is_some());
    call(
        svc,
        &ctx,
        "browser",
        json!({"action":"resize","width":375,"height":667}),
    )
    .await;
    call(
        svc,
        &ctx,
        "browser",
        json!({"action":"screenshot","path":"mobile-proof.png"}),
    )
    .await;
    let redirect = svc
        .execute(
            &ctx,
            "browser",
            &json!({"action":"open","url":format!("{url}/redirect")}),
            std::future::pending(),
        )
        .await
        .unwrap();
    assert_ne!(redirect.kind, ExecKind::Success);
    assert_eq!(
        leaked.load(Ordering::Relaxed),
        0,
        "external fetch/redirect/websocket leaked"
    );
    // Use a real process reading input from a controlling PTY, then redraw ANSI.
    call(svc,&ctx,"terminal",json!({"action":"start","program":"bash","args":["--noprofile","--norc","-c","printf '\\033[2J\\033[HREADY'; read -r line; printf '\\033[2J\\033[HRESULT:%s' \"$line\"; sleep 20"],"cols":80,"rows":24})).await;
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"type","text":"PTY works"}),
    )
    .await;
    let terminal = call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"press","key":"Enter"}),
    )
    .await;
    assert!(
        terminal.text.contains("RESULT:PTY works"),
        "{}",
        terminal.text
    );
    assert!(
        !terminal.text.contains("READY"),
        "ANSI cursor/clear must be interpreted"
    );
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"screenshot","path":"terminal-proof.png"}),
    )
    .await;
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"resize","cols":40,"rows":12}),
    )
    .await;
    call(svc, &ctx, "terminal", json!({"action":"close"})).await;
    // Exercise the actual product TUI through the built-in tool as well.
    call(svc,&ctx,"terminal",json!({"action":"start","program":env!("CARGO_BIN_EXE_sui"),"args":["tui"],"cols":100,"rows":30,"wait_ms":300})).await;
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"screenshot","path":"sui-proof.png"}),
    )
    .await;
    let help = call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"press","key":"F1","wait_ms":200}),
    )
    .await;
    assert!(help.text.to_lowercase().contains("help"), "{}", help.text);
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"press","key":"Escape"}),
    )
    .await;
    let palette = call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"press","key":"Control+p"}),
    )
    .await;
    assert!(
        palette.text.to_lowercase().contains("command"),
        "{}",
        palette.text
    );
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"screenshot","path":"sui-commands-proof.png"}),
    )
    .await;
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"resize","cols":60,"rows":20}),
    )
    .await;
    call(
        svc,
        &ctx,
        "terminal",
        json!({"action":"screenshot","path":"sui-narrow-proof.png"}),
    )
    .await;
    tokio::time::sleep(Duration::from_secs(3)).await;
    call(svc, &ctx, "terminal", json!({"action":"snapshot"})).await;
    let cancelled = svc
        .execute(
            &ctx,
            "terminal",
            &json!({"action":"snapshot","wait_ms":2000}),
            tokio::time::sleep(Duration::from_millis(50)),
        )
        .await
        .unwrap();
    assert_eq!(cancelled.kind, ExecKind::Cancelled);
    let closed = svc
        .execute(
            &ctx,
            "terminal",
            &json!({"action":"snapshot"}),
            std::future::pending(),
        )
        .await
        .unwrap();
    assert_ne!(closed.kind, ExecKind::Success, "cancelled PTY must be gone");
    call(svc, &ctx, "browser", json!({"action":"open","url":url})).await;
    call(svc, &ctx, "browser", json!({"action":"close"})).await;
    eprintln!("headless visual proof: {}", ctx.workspace.display());
}

#[test]
fn headless_yolo_and_project_config_cannot_authorize_browser_at_eof() {
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    let root = workspace();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        root.join("sui.toml"),
        "[browser]\napproved = true\nallow_remote = true",
    )
    .unwrap();
    let denied = Arc::new(AtomicBool::new(false));
    let observed = denied.clone();
    let port = common::serve(move |_, messages| {
        if let Some(tool) = messages.iter().find(|m| m["role"] == "tool") {
            observed.store(
                tool["content"]
                    .as_str()
                    .unwrap()
                    .starts_with("status: denied"),
                Ordering::Relaxed,
            );
            common::sse_text("permission checked")
        } else {
            let mut call = common::tc("browser", "browser", "{\"action\":\"close\"}");
            call["index"] = json!(0);
            common::sse_tool_calls(json!([call]))
        }
    });
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .env_clear()
        .env("HOME", &home)
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .args([
            "--yolo",
            "--base-url",
            &format!("http://127.0.0.1:{port}"),
            "--model",
            "mock",
            "--workspace",
        ])
        .arg(&root)
        .arg("test browser permissions")
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(denied.load(Ordering::Relaxed));
    std::fs::remove_dir_all(root).unwrap();
}
