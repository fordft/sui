//! Regular tests need no browser/download. The ignored test exercises real
//! Chromium + PTY on headless Linux, explicitly opting into runtime setup.
mod common;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use sui::tools::{
    self,
    ui::{BrowserCfg, UiService},
    ExecKind, ToolContext,
};

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
