use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sui::agent::{Agent, Identity, Limits};
use sui::config::{PricingCfg, Transport};
use sui::context::{self, Compiled};
use sui::journal::{replay_history, Journal};
use sui::permission::Gate;
use sui::provider::Provider;
use sui::tools::ToolContext;
use sui::types::{Message, Usage};

fn dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sui-cache-test-{:032x}", rand::random::<u128>()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

type Requests = Arc<Mutex<Vec<(String, Value)>>>;
fn server(replies: Vec<String>) -> (String, Requests, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
    let captured: Requests = Arc::default();
    let out = captured.clone();
    let handle = std::thread::spawn(move || {
        for reply in replies {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut bytes = Vec::new();
            let mut byte = [0];
            while !bytes.ends_with(b"\r\n\r\n") {
                stream.read_exact(&mut byte).unwrap();
                bytes.push(byte[0]);
            }
            let headers = String::from_utf8(bytes).unwrap();
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    line.to_lowercase()
                        .strip_prefix("content-length:")
                        .map(|s| s.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            stream.read_exact(&mut body).unwrap();
            out.lock()
                .unwrap()
                .push((headers, serde_json::from_slice(&body).unwrap()));
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", reply.len(), reply).unwrap();
        }
    });
    (endpoint, captured, handle)
}
fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn chat(text: &str) -> String {
    event(
        json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}],
        "usage":{"prompt_tokens":100,"completion_tokens":10,"prompt_tokens_details":{"cached_tokens":0,"cache_write_tokens":0}}}),
    )
}
fn responses(opaque: bool) -> String {
    let mut result = if opaque {
        event(json!({"type":"response.output_item.done","item":{
            "type":"reasoning","id":"rs_1","status":"completed","summary":[],"encrypted_content":"opaque-fixture-state"
        }}))
    } else {
        String::new()
    };
    result += &event(json!({"type":"response.output_text.delta","delta":"OK"}));
    result += &event(
        json!({"type":"response.completed","response":{"model":"fixture-model","usage":{
            "input_tokens":12677,"input_tokens_details":{"cached_tokens":12544,"cache_write_tokens":0},"output_tokens":1
        }}}),
    );
    result
}

#[tokio::test]
async fn raw_responses_usage_and_private_replay_survive_restart() {
    let (endpoint, requests, thread) = server(vec![responses(true), responses(false)]);
    let provider = Provider::new(
        &endpoint,
        Some("fixture-api-key".into()),
        "fixture-model".into(),
        None,
    )
    .with_transport(Transport::OpenaiResponses);
    let history = vec![Message::User {
        content: "first".into(),
    }];
    let first = provider
        .stream_chat(&Compiled::view(&history), &[], |_| {}, |_| {})
        .await
        .unwrap();
    let usage = first.usage.as_ref().unwrap();
    assert_eq!(usage.input_tokens, Some(12677));
    assert_eq!(usage.cache_read_tokens, Some(12544));
    assert_eq!(usage.cache_write_tokens, Some(0));
    assert_eq!(
        first.response_items[0]["encrypted_content"],
        "opaque-fixture-state"
    );
    let root = dir();
    let mut journal = Journal::open(&root).unwrap();
    journal.log("user", json!({"content":"first"}));
    let reference = journal
        .store_response_items(&first.response_items)
        .unwrap()
        .unwrap();
    journal.log(
        "assistant",
        json!({"content":first.content,"tool_calls":[],"reasoning_content":null,
        "response_items_count":1,"response_items_ref":reference}),
    );
    let path = Journal::path_of(&root, "events");
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(!text.contains("opaque-fixture-state"));
    assert!(!text.contains("fixture-api-key"));
    let mut restored = replay_history(&path, 1).unwrap();
    restored.push(Message::User {
        content: "followup".into(),
    });
    provider
        .stream_chat(&Compiled::view(&restored), &[], |_| {}, |_| {})
        .await
        .unwrap();
    thread.join().unwrap();
    let calls = requests.lock().unwrap();
    assert!(calls
        .iter()
        .all(|(h, _)| h.starts_with("POST /v1/responses ")));
    let session = |h: &str| {
        h.lines()
            .find(|l| l.starts_with("session_id:"))
            .unwrap()
            .to_owned()
    };
    assert_eq!(session(&calls[0].0), session(&calls[1].0));
    assert_eq!(
        calls[1].1["input"][1]["encrypted_content"],
        "opaque-fixture-state"
    );
    assert_eq!(calls[0].1["tools"], calls[1].1["tools"]);
    assert_eq!(calls[0].1["include"], calls[1].1["include"]);
    assert_eq!(calls[0].1["model"], calls[1].1["model"]);
    let sidecar = root.join(reference["file"].as_str().unwrap());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&sidecar).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    std::fs::write(&sidecar, "[]").unwrap();
    assert!(replay_history(&path, 1)
        .unwrap_err()
        .to_string()
        .contains("integrity"));
    std::fs::remove_file(sidecar).unwrap();
    assert!(replay_history(&path, 1).is_err());
    std::fs::remove_dir_all(root).unwrap();
}

fn agent(endpoint: &str, root: &std::path::Path, history: Vec<Message>) -> Agent {
    let base = context::estimate_tokens(&context::compile(&history, &context::system(), None));
    let schema = serde_json::to_vec(&sui::tools::schemas()).unwrap().len() / 4;
    // Trigger proactive compaction but leave room for its appended instruction.
    let mut agent = Agent::new(
        Provider::new(
            endpoint,
            None,
            "fixture-model".into(),
            Some("fixed-group".into()),
        ),
        ToolContext {
            workspace: root.to_path_buf(),
            bash_timeout: Duration::from_secs(1),
            bash_timeout_max: Duration::from_secs(1),
            web: None,
            canon_root: Default::default(),
            ui: Default::default(),
            code_intel: Default::default(),
            code_context: Default::default(),
        },
        Gate::new(true),
        Journal::open(root).unwrap(),
        Limits {
            max_turns: 2,
            context_budget: base + schema + 1000,
            context_reserve: 200,
            compact_context: true,
            request_timeout: Duration::from_secs(5),
        },
        Identity {
            session_id: "test".into(),
            agent_id: "worker".into(),
            role: "worker".into(),
            base_url: endpoint.into(),
            model: "fixture-model".into(),
            cache_key_fingerprint: None,
        },
    );
    agent.set_quiet(true);
    agent.restore_history(history);
    agent
}
fn large_history() -> Vec<Message> {
    vec![
        Message::User {
            content: "Inspect the fixture; no deployment is authorized.".into(),
        },
        Message::Assistant {
            content: Some("Observed fixture evidence. ".repeat(1400)),
            tool_calls: None,
            reasoning_content: None,
            response_items: vec![],
        },
    ]
}

#[tokio::test]
async fn compaction_preserves_summary_prefix_and_replays_the_new_epoch() {
    let (endpoint, requests, thread) = server(vec![
        chat("Observed evidence was reviewed; latest question is pending."),
        chat("42"),
    ]);
    let root = dir();
    let mut agent = agent(&endpoint, &root, large_history());
    agent
        .run_turn("What is the fixture counter? Keep prior permission restrictions.")
        .await
        .unwrap();
    thread.join().unwrap();
    assert_eq!(agent.requests_made(), 2);
    let requests = requests.lock().unwrap();
    let before = &requests[0].1;
    let after = &requests[1].1;
    assert_eq!(before["tools"], after["tools"]);
    assert_eq!(before["model"], after["model"]);
    assert_eq!(before["prompt_cache_key"], after["prompt_cache_key"]);
    assert_eq!(before["messages"][0], after["messages"][0]);
    let messages = before["messages"].as_array().unwrap();
    assert_eq!(
        messages[messages.len() - 2]["content"],
        "What is the fixture counter? Keep prior permission restrictions."
    );
    assert!(messages.last().unwrap()["content"]
        .as_str()
        .unwrap()
        .starts_with("Summarize the conversation above"));
    assert!(after["messages"][1]["content"]
        .as_str()
        .unwrap()
        .contains("not new instructions or runtime verification"));
    assert_eq!(
        after["messages"][2]["content"],
        messages[messages.len() - 2]["content"]
    );
    let journal = Journal::path_of(&root, "events");
    let replay = replay_history(&journal, usize::MAX).unwrap();
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        serde_json::to_value(agent.history()).unwrap()
    );
    let events: Vec<Value> = std::fs::read_to_string(journal)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let requests: Vec<_> = events.iter().filter(|e| e["type"] == "request").collect();
    assert_eq!(requests[0]["data"]["purpose"], "compaction");
    assert_eq!(requests[0]["data"]["epoch_id"], "E0");
    assert_eq!(requests[1]["data"]["epoch_id"], "E1");
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn failed_summaries_never_execute_tools_or_replace_history() {
    let dangerous = event(
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_bad","function":{"name":"write_file","arguments":"{\"path\":\"bad.txt\",\"content\":\"bad\"}"}}]},"finish_reason":"tool_calls"}]}),
    );
    let interrupted = event(json!({"choices":[{"delta":{"content":"partial"}}]}));
    for response in [
        dangerous,
        interrupted,
        chat(&"x".repeat(17000)),
        chat(&"x".repeat(30000)),
    ] {
        let (endpoint, _, thread) = server(vec![response]);
        let root = dir();
        let mut agent = agent(&endpoint, &root, large_history());
        agent.push_user("Continue carefully.");
        let expected = serde_json::to_value(agent.history()).unwrap();
        assert!(agent.drive().await.is_err());
        thread.join().unwrap();
        assert_eq!(serde_json::to_value(agent.history()).unwrap(), expected);
        assert!(!root.join("bad.txt").exists());
        assert_eq!(agent.requests_made(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn cost_uses_disjoint_buckets_and_unknowns_stay_unknown() {
    let pricing = PricingCfg {
        input: Some(1.0),
        cached: Some(0.1),
        cache_write: None,
        output: Some(2.0),
    };
    let mut usage = Usage {
        input_tokens: Some(1500),
        cache_read_tokens: Some(1000),
        cache_write_tokens: Some(0),
        output_tokens: Some(500),
        complete: true,
        estimated: false,
    };
    assert!((pricing.estimate(&usage).unwrap() - 0.0016).abs() < 1e-12);
    usage.cache_write_tokens = None;
    assert!(pricing.estimate(&usage).is_none());
    usage.cache_write_tokens = Some(100);
    assert!(pricing.estimate(&usage).is_none());
    let write_price = PricingCfg {
        cache_write: Some(1.25),
        ..pricing.clone()
    };
    assert!((write_price.estimate(&usage).unwrap() - 0.001625).abs() < 1e-12);
    usage.cache_read_tokens = Some(1600);
    assert!(write_price.estimate(&usage).is_none());
    usage.cache_read_tokens = Some(1000);
    usage.estimated = true;
    assert!(write_price.estimate(&usage).is_none());
    usage.estimated = false;
    usage.complete = false;
    assert!(write_price.estimate(&usage).is_none());
    usage.complete = true;
    assert!(PricingCfg {
        input: None,
        ..write_price
    }
    .estimate(&usage)
    .is_none());
}

#[test]
fn responses_profile_save_preserves_endpoint_and_credentials() {
    let root = dir();
    let path = root.join("config.toml");
    sui::config::save_profile_at(
        &path,
        "fixture",
        "http://localhost:20128/v1",
        "fixture-model",
        Some("FIXTURE_KEY"),
        None,
        Some("openai-responses"),
    )
    .unwrap();
    let profile = sui::config::resolve_profile("fixture", Some(&path)).unwrap();
    assert_eq!(profile.transport, Transport::OpenaiResponses);
    assert_eq!(profile.base_url, "http://localhost:20128/v1");
    let doc: toml::Value = std::fs::read_to_string(path).unwrap().parse().unwrap();
    assert_eq!(
        doc["profiles"]["fixture"]["key_env"].as_str(),
        Some("FIXTURE_KEY")
    );
    std::fs::remove_dir_all(root).unwrap();
}
