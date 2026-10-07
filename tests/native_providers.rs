use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use sui::config::Transport;
use sui::provider::{list_models_with_transport, Provider};
use sui::types::{Message, UserContent};

type Requests = Arc<Mutex<Vec<(String, Value)>>>;
fn server(replies: Vec<(&'static str, String)>) -> (String, Requests, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let requests: Requests = Arc::default();
    let captured = requests.clone();
    let task = std::thread::spawn(move || {
        for (content_type, reply) in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            let headers = String::from_utf8(headers).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            captured.lock().unwrap().push((
                headers,
                if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                },
            ));
            write!(stream,"HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",reply.len()).unwrap();
            // Split every byte, including inside Thai UTF-8 and JSON fragments.
            for byte in reply.as_bytes() {
                if let Err(error) = stream.write_all(&[*byte]) {
                    assert!(matches!(
                        error.kind(),
                        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
                    ));
                    break;
                }
            }
        }
    });
    (base, requests, task)
}
fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn tools() -> Vec<Value> {
    vec![json!({"type":"function","function":{
        "name":"get_number","description":"Get number","parameters":{"type":"object","properties":{},"additionalProperties":false},
    }})]
}
fn history() -> Vec<Message> {
    vec![
        Message::System {
            content: "Use tools when needed.".into(),
        },
        Message::User {
            content: "Get the number.".into(),
        },
    ]
}
fn append(messages: &mut Vec<Message>, out: &sui::provider::StreamOutcome) {
    let call = &out.tool_calls[0];
    messages.push(Message::Assistant {
        content: Some(out.content.clone()),
        tool_calls: Some(out.tool_calls.clone()),
        reasoning_content: out.reasoning_content.clone(),
        response_items: out.response_items.clone(),
    });
    messages.push(Message::Tool {
        tool_call_id: call.id.clone(),
        content: "42".into(),
    });
}
fn anthropic_end(reason: &str) -> String {
    event(
        json!({"type":"message_delta","delta":{"stop_reason":reason},"usage":{"output_tokens":7}}),
    ) + &event(json!({"type":"message_stop"}))
}
#[tokio::test]
async fn anthropic_stream_tools_replay_images_and_usage_work_across_turns() {
    let start = event(
        json!({"type":"message_start","message":{"model":"claude-test","usage":{
            "input_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":10,"output_tokens":0,
        }}}),
    );
    let first = start.clone()
        + &event(json!({"type":"content_block_start","index":0,
        "content_block":{"type":"thinking","thinking":"","signature":""}}))
        + &event(json!({"type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"Consider the tool."}}))
        + &event(json!({"type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":"opaque-"}}))
        + &event(json!({"type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":"signature"}}))
        + &event(json!({"type":"content_block_start","index":1,
        "content_block":{"type":"tool_use","id":"c1","name":"get_number","input":{}}}))
        + &event(
            json!({"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        )
        + &anthropic_end("tool_use");
    let second = start
        + &event(
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        )
        + &event(
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"สวัสดี 42"}}),
        )
        + &anthropic_end("end_turn");
    let (base, captured, task) = server(vec![
        ("text/event-stream", first),
        ("text/event-stream", second),
    ]);
    let provider = Provider::new(&base, Some("test-key".into()), "claude-test".into(), None)
        .with_transport(Transport::Anthropic);
    let mut messages = history();
    messages.push(Message::User {
        content: UserContent::image("screen".into(), "data:image/png;base64,YQ==".into()),
    });
    let out = provider
        .stream_chat(
            &sui::context::Compiled::view(&messages),
            &tools(),
            |_| {},
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
    assert_eq!(out.usage.as_ref().unwrap().input_tokens, Some(130));
    assert!(out.usage.as_ref().unwrap().complete);
    append(&mut messages, &out);
    let out = provider
        .stream_chat(
            &sui::context::Compiled::view(&messages),
            &tools(),
            |_| {},
            |_| {},
        )
        .await
        .unwrap();
    assert_eq!(out.content, "สวัสดี 42");
    assert_eq!(out.finish_reason.as_deref(), Some("stop"));
    task.join().unwrap();
    let requests = captured.lock().unwrap();
    assert!(requests[0].0.contains("POST /v1/messages"));
    assert!(requests[0]
        .0
        .to_ascii_lowercase()
        .contains("x-api-key: test-key"));
    assert!(!requests[0]
        .0
        .to_ascii_lowercase()
        .contains("authorization: bearer"));
    assert_eq!(
        requests[0].1["messages"][0]["content"][2]["source"]["media_type"],
        "image/png"
    );
    assert_eq!(
        requests[1].1["messages"][2]["content"][0]["tool_use_id"],
        "c1"
    );
    assert_eq!(requests[0].1["system"], requests[1].1["system"]);
    assert_eq!(requests[0].1["tools"], requests[1].1["tools"]);
    assert_eq!(
        requests[1].1["messages"][1]["content"][0]["signature"],
        "opaque-signature"
    );
}
#[tokio::test]
async fn anthropic_truncated_tool_call_never_reports_completion() {
    let reply = event(json!({"type":"content_block_start","index":0,
        "content_block":{"type":"tool_use","id":"c1","name":"get_number","input":{}}}))
        + &event(
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":3}}),
        );
    let (base, _, task) = server(vec![("text/event-stream", reply)]);
    let provider = Provider::new(&base, Some("key".into()), "claude-test".into(), None)
        .with_transport(Transport::Anthropic);
    let out = provider
        .stream_chat(
            &sui::context::Compiled::view(&history()),
            &tools(),
            |_| {},
            |_| {},
        )
        .await
        .unwrap();
    assert!(out.finish_reason.is_none());
    assert!(!out.usage.unwrap().complete);
    task.join().unwrap();
}
#[tokio::test]
async fn gemini_signed_tool_calls_and_unknown_cache_usage_survive_continuation() {
    for wire_id in [Some("c1"), None] {
        let mut part = json!({"functionCall":{"name":"get_number","args":{}},"thoughtSignature":"opaque-signature"});
        if let Some(id) = wire_id {
            part["functionCall"]["id"] = json!(id);
        }
        let first = event(
            json!({"modelVersion":"gemini-test","candidates":[{"content":{"parts":[part]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":100,"candidatesTokenCount":5}}),
        );
        let second = event(
            json!({"candidates":[{"content":{"parts":[{"text":"42 สวัสดี"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":120,"cachedContentTokenCount":100,"candidatesTokenCount":7,"totalTokenCount":150}}),
        );
        let (base, captured, task) = server(vec![
            ("text/event-stream", first),
            ("text/event-stream", second),
        ]);
        let provider = Provider::new(&base, Some("gemini-key".into()), "gemini-test".into(), None)
            .with_transport(Transport::Gemini);
        let mut messages = history();
        let out = provider
            .stream_chat(
                &sui::context::Compiled::view(&messages),
                &tools(),
                |_| {},
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(out.finish_reason.as_deref(), Some("tool_calls"));
        assert!(out.usage.as_ref().unwrap().cache_read_tokens.is_none());
        assert!(out.usage.as_ref().unwrap().cache_write_tokens.is_none());
        assert!(out.usage.as_ref().unwrap().output_tokens.is_none());
        append(&mut messages, &out);
        let out = provider
            .stream_chat(
                &sui::context::Compiled::view(&messages),
                &tools(),
                |_| {},
                |_| {},
            )
            .await
            .unwrap();
        assert_eq!(out.content, "42 สวัสดี");
        let usage = out.usage.unwrap();
        assert_eq!(usage.cache_read_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(30));
        task.join().unwrap();
        let requests = captured.lock().unwrap();
        assert!(requests[0]
            .0
            .contains("POST /v1/models/gemini-test:streamGenerateContent?alt=sse"));
        assert!(requests[0]
            .0
            .to_ascii_lowercase()
            .contains("x-goog-api-key: gemini-key"));
        assert_eq!(
            requests[1].1["contents"][1]["parts"][0]["thoughtSignature"],
            "opaque-signature"
        );
        assert_eq!(
            requests[1].1["contents"][2]["parts"][0]["functionResponse"]["name"],
            "get_number"
        );
        assert_eq!(
            requests[1].1["contents"][2]["parts"][0]["functionResponse"]["id"].as_str(),
            wire_id
        );
        assert_eq!(
            requests[1].1["contents"][2]["parts"][0]["functionResponse"]["response"]["output"],
            "42"
        );
        assert_eq!(
            requests[0].1["systemInstruction"],
            requests[1].1["systemInstruction"]
        );
        assert_eq!(requests[0].1["tools"], requests[1].1["tools"]);
    }
}
#[tokio::test]
async fn native_catalogs_use_provider_auth_and_follow_pagination() {
    let (base, captured, task) = server(vec![
        (
            "application/json",
            json!({"data":[{"id":"claude-a"}],"has_more":true,"last_id":"claude-a"}).to_string(),
        ),
        (
            "application/json",
            json!({"data":[{"id":"claude-b"}],"has_more":false}).to_string(),
        ),
    ]);
    let models = list_models_with_transport(&base, Some("key"), Transport::Anthropic)
        .await
        .unwrap();
    assert_eq!(models.len(), 2);
    task.join().unwrap();
    assert!(captured.lock().unwrap()[1].0.contains("after_id=claude-a"));
    let (base,_,task) = server(vec![("application/json",json!({"models":[
        {"name":"models/gemini-a","supportedGenerationMethods":["generateContent"],"inputTokenLimit":1000},
        {"name":"models/embedding","supportedGenerationMethods":["embedContent"]},
    ]}).to_string())]);
    let models = list_models_with_transport(&base, Some("key"), Transport::Gemini)
        .await
        .unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gemini-a");
    assert_eq!(models[0].context_length, Some(1000));
    task.join().unwrap();
}
#[test]
fn ollama_and_custom_cli_setup_are_keyless_and_preserve_configuration() {
    let root = std::env::temp_dir().join(format!("sui-login-cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, "[ui]\ntheme = \"terminal\"\n").unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .env("SUI_HOME", &root)
        .args(["auth", "ollama", "--model", "local-coder"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: toml::Value = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(doc["profiles"]["ollama"]["kind"].as_str(), Some("ollama"));
    assert_eq!(doc["ui"]["theme"].as_str(), Some("terminal"));
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .env("SUI_HOME", &root)
        .args([
            "login",
            "--provider",
            "openai-compatible",
            "--base-url",
            "http://127.0.0.1:1234/v1",
            "--model",
            "custom-model",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let doc: toml::Value = std::fs::read_to_string(&config).unwrap().parse().unwrap();
    assert_eq!(
        doc["profiles"]["custom"]["model"].as_str(),
        Some("custom-model")
    );
    assert!(doc["profiles"]["custom"].get("api_key").is_none());
    assert!(doc["profiles"].get("ollama").is_some());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
        .env("SUI_HOME", &root)
        .args(["auth", "ollama"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("--model"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn malformed_native_events_cannot_be_skipped_before_completion() {
    for (transport, end) in [
        (Transport::Anthropic, anthropic_end("end_turn")),
        (
            Transport::Gemini,
            event(json!({"candidates":[{"finishReason":"STOP"}]})),
        ),
    ] {
        let (base, _, task) = server(vec![(
            "text/event-stream",
            format!("data: {{broken\n\n{end}"),
        )]);
        let provider = Provider::new(&base, Some("key".into()), "test-model".into(), None)
            .with_transport(transport);
        let error = provider
            .stream_chat(
                &sui::context::Compiled::view(&history()),
                &tools(),
                |_| {},
                |_| {},
            )
            .await
            .err()
            .expect("malformed provider event must fail");
        assert!(format!("{error:#}").contains("invalid native provider event"));
        task.join().unwrap();
    }
}

#[test]
fn claude_cli_setup_keeps_credentials_in_the_environment_and_preserves_the_model() {
    let root = std::env::temp_dir().join(format!("sui-claude-cli-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let config = root.join("config.toml");
    std::fs::write(&config, "[ui]\ntheme = \"terminal\"\n").unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
            .env("SUI_HOME", &root)
            .env("SUI_TEST_CLAUDE_KEY", "private-api-key-for-cli-test")
            .args(args)
            .output()
            .unwrap()
    };
    for args in [
        vec![
            "auth",
            "claude",
            "--model",
            "claude-custom",
            "--key-env",
            "SUI_TEST_CLAUDE_KEY",
        ],
        vec!["login", "claude", "--key-env", "SUI_TEST_CLAUDE_KEY"],
    ] {
        let output = run(&args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("private-api-key-for-cli-test"));
        let text = std::fs::read_to_string(&config).unwrap();
        assert!(!text.contains("private-api-key-for-cli-test"));
        let doc: toml::Value = text.parse().unwrap();
        assert_eq!(
            doc["profiles"]["claude"]["kind"].as_str(),
            Some("anthropic")
        );
        assert_eq!(
            doc["profiles"]["claude"]["key_env"].as_str(),
            Some("SUI_TEST_CLAUDE_KEY")
        );
        assert_eq!(
            doc["profiles"]["claude"]["model"].as_str(),
            Some("claude-custom")
        );
        assert_eq!(doc["ui"]["theme"].as_str(), Some("terminal"));
    }
    let before = std::fs::read_to_string(&config).unwrap();
    for args in [
        vec![
            "auth",
            "claude",
            "--base-url",
            "file:///tmp/api",
            "--key-env",
            "SUI_TEST_CLAUDE_KEY",
        ],
        vec![
            "auth",
            "claude",
            "--model",
            " ",
            "--key-env",
            "SUI_TEST_CLAUDE_KEY",
        ],
    ] {
        assert!(!run(&args).status.success());
        assert_eq!(std::fs::read_to_string(&config).unwrap(), before);
    }
    std::fs::remove_dir_all(root).unwrap();
}
