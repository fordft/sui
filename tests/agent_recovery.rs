//! Native provider → agent → permission → tool journeys. These tests exercise
//! recovery and patch delivery through the real loop, without live credentials.
mod common;

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sui::agent::{Agent, Identity, Limits};
use sui::config::Transport;
use sui::events::{GateChoice, UiEvent};
use sui::journal::{replay_history, Journal};
use sui::permission::Gate;
use sui::provider::Provider;
use sui::tools::ToolContext;
use sui::types::Message;

type Requests = Arc<Mutex<Vec<Value>>>;

fn mock(handler: impl Fn(usize, &Value) -> (u16, String) + Send + 'static) -> (String, Requests) {
    let requests: Requests = Arc::default();
    let captured = requests.clone();
    let port = common::serve_http(move |body, _| {
        let request: Value = serde_json::from_slice(body).unwrap();
        let index = {
            let mut requests = captured.lock().unwrap();
            let index = requests.len();
            requests.push(request.clone());
            index
        };
        handler(index, &request)
    });
    (format!("http://127.0.0.1:{port}/v1"), requests)
}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}

fn response_text(text: &str) -> String {
    event(json!({"type":"response.output_text.delta","delta":text}))
        + &event(
            json!({"type":"response.completed","response":{"model":"fixture",
            "usage":{"input_tokens":100,"output_tokens":5,"input_tokens_details":{"cached_tokens":0}}}}),
        )
}

fn response_tool(name: &str, args: Value) -> String {
    event(json!({"type":"response.output_item.done","item":{
        "type":"function_call","call_id":"call-fixture","name":name,"arguments":args.to_string()
    }})) + &event(json!({"type":"response.completed","response":{"model":"fixture"}}))
}

fn chat_tool(name: &str, args: Value) -> String {
    common::sse_tool_calls(json!([common::tc("call-fixture", name, &args.to_string())]))
}

struct Fixture {
    root: PathBuf,
    workspace: PathBuf,
    run: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("sui-recovery-{:032x}", rand::random::<u128>()));
        let workspace = root.join("workspace");
        let run = root.join("runs/session-a");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&run).unwrap();
        Self {
            root,
            workspace,
            run,
        }
    }

    fn agent(&self, endpoint: &str, transport: Transport, auto: bool, limits: Limits) -> Agent {
        let mut agent = Agent::new(
            Provider::new(
                endpoint,
                Some("fixture-credential-do-not-echo".into()),
                "fixture".into(),
                None,
            )
            .with_transport(transport),
            ToolContext {
                workspace: self.workspace.clone(),
                bash_timeout: Duration::from_secs(2),
                bash_timeout_max: Duration::from_secs(2),
                web: None,
                canon_root: Default::default(),
                ui: Default::default(),
                code_intel: Default::default(),
                code_context: Default::default(),
                tool_outputs: Default::default(),
            },
            Gate::new(auto),
            Journal::open_named(&self.run, "solo").unwrap(),
            limits,
            Identity {
                session_id: "fixture-session".into(),
                agent_id: "solo".into(),
                role: "worker".into(),
                base_url: endpoint.into(),
                model: "fixture".into(),
                cache_key_fingerprint: None,
            },
        );
        agent.set_quiet(true);
        agent
    }

    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.run.join("solo.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn limits(max_turns: usize) -> Limits {
    Limits {
        max_turns,
        context_budget: 120_000,
        context_reserve: 200,
        compact_context: false,
        request_timeout: Duration::from_secs(2),
    }
}

#[tokio::test]
async fn a_connection_dropped_before_headers_retries_instead_of_stopping_the_task() {
    let fixture = Fixture::new();
    let (endpoint, requests) = mock(|index, _| match index {
        0 => (0, String::new()),
        1 => (200, common::sse_text("Recovered and completed.")),
        _ => (401, String::new()),
    });
    let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, true, limits(1));
    tokio::time::timeout(Duration::from_secs(3), agent.run_turn("Complete the task."))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(requests.lock().unwrap().len(), 2);
    assert!(fixture
        .events()
        .iter()
        .any(|e| e["type"] == "provider_retry"));
}

#[tokio::test]
async fn provider_failures_retry_without_repeating_completed_tools_or_consuming_iterations() {
    let fixture = Fixture::new();
    std::fs::write(fixture.workspace.join("counter"), "old").unwrap();
    let (endpoint, requests) = mock(|index, _| match index {
        0 => (
            200,
            response_tool(
                "edit_file",
                json!({"path":"counter","old_str":"old","new_str":"updated"}),
            ),
        ),
        1 => (
            200,
            event(
                json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"partial",
                "name":"write_file","arguments":"{\"path\":\"unsafe\",\"content\":\"bad\"}"}}),
            ) + &event(
                json!({"type":"response.failed","response":{"error":{"code":"server_error",
                "message":"fixture-credential-do-not-echo"}}}),
            ),
        ),
        2 => (503, "fixture-credential-do-not-echo".into()),
        3 => (200, response_text("Task completed.")),
        _ => (401, String::new()),
    });
    let mut agent = fixture.agent(&endpoint, Transport::OpenaiResponses, true, limits(2));
    tokio::time::timeout(
        Duration::from_secs(6),
        agent.run_turn("Update the counter."),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.workspace.join("counter")).unwrap(),
        "updated"
    );
    assert!(!fixture.workspace.join("unsafe").exists());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert_eq!(requests[1], requests[2]);
    assert_eq!(requests[2], requests[3]);
    assert!(requests.iter().all(|r| r["tools"] == requests[0]["tools"]));
    let events = fixture.events();
    assert_eq!(events.iter().filter(|e| e["type"] == "tool").count(), 1);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["type"] == "provider_retry")
            .count(),
        2
    );
    let failed: Vec<_> = events
        .iter()
        .filter(|e| e["type"] == "request" && e["data"]["error_class"].is_string())
        .collect();
    assert_eq!(failed.len(), 2);
    assert!(failed
        .iter()
        .all(|e| e["data"]["usage"].is_null() && e["data"]["timing"]["first_delta_ms"].is_null()));
    let journal = std::fs::read_to_string(fixture.run.join("solo.jsonl")).unwrap();
    assert!(!journal.contains("fixture-credential-do-not-echo"));
    assert_eq!(
        serde_json::to_value(replay_history(&fixture.run.join("solo.jsonl"), usize::MAX).unwrap())
            .unwrap(),
        serde_json::to_value(agent.history()).unwrap()
    );
}

#[tokio::test]
async fn permanent_errors_remain_visible_to_the_next_turn_without_echoing_credentials() {
    for (status, stream_error) in [(401, false), (429, false), (200, true)] {
        let fixture = Fixture::new();
        let (endpoint, requests) = mock(move |index, _| match index {
            0 if stream_error => (
                200,
                event(json!({"type":"error","error":{"code":"invalid_api_key",
                "message":"fixture-credential-do-not-echo"}})),
            ),
            0 => {
                std::thread::sleep(Duration::from_millis(20));
                (
                    status,
                    if status == 429 {
                        json!({"error":{"code":"insufficient_quota","message":"fixture-credential-do-not-echo"}}).to_string()
                    } else {
                        "fixture-credential-do-not-echo".into()
                    },
                )
            }
            1 => (200, response_text("Authentication needs attention.")),
            _ => (401, String::new()),
        });
        let mut agent = fixture.agent(&endpoint, Transport::OpenaiResponses, true, limits(3));
        assert!(agent.run_turn("Do the task.").await.is_err());
        assert_eq!(requests.lock().unwrap().len(), 1);
        agent.run_turn("What happened?").await.unwrap();
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let next = requests[1].to_string();
        assert!(next.contains("runtime_observation"));
        assert!(next.contains(if stream_error {
            "invalid_api_key"
        } else if status == 429 {
            "provider http 429: insufficient_quota"
        } else {
            "provider http 401"
        }));
        assert!(!next.contains("fixture-credential-do-not-echo"));
        let events = fixture.events();
        assert_eq!(events.iter().filter(|e| e["type"] == "user").count(), 2);
        assert!(!events.iter().any(|e| e["type"] == "provider_retry"));
        let failed = events.iter().find(|e| e["type"] == "request").unwrap();
        if !stream_error {
            assert!(
                failed["data"]["timing"]["request_total_ms"]
                    .as_u64()
                    .unwrap()
                    >= 20
            );
        }
        assert!(failed["data"]["timing"]["first_delta_ms"].is_null());
    }
}

#[tokio::test]
async fn stop_interrupts_retry_backoff_even_without_a_notify() {
    let fixture = Fixture::new();
    let (endpoint, requests) = mock(|_, _| (503, String::new()));
    let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, true, limits(2));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let stop = Arc::new(AtomicBool::new(false));
    agent.wire_ui(tx, Arc::new(tokio::sync::Notify::new()), stop.clone(), None);
    let task = tokio::spawn(async move {
        let result = agent.run_turn("Keep working.").await;
        (agent, result)
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if matches!(rx.recv().await, Some(UiEvent::Phase { text, .. }) if text.contains("retry")) { break; }
        }
    }).await.unwrap();
    stop.store(true, Ordering::Relaxed);
    let (agent, result) = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    result.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(fixture
        .events()
        .iter()
        .any(|e| e["type"] == "turn_end" && e["data"]["outcome"] == "stopped"));
    assert!(serde_json::to_value(agent.history())
        .unwrap()
        .to_string()
        .contains("task is unfinished"));
}

#[tokio::test]
async fn eof_discards_partial_tool_calls_and_retries_the_same_request() {
    let fixture = Fixture::new();
    let (endpoint, requests) = mock(|index, _| match index {
        0 => (
            200,
            event(
                json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"partial",
            "function":{"name":"write_file","arguments":"{\"path\":\"unsafe\",\"content\":\"bad\"}"}}]}}]}),
            ),
        ),
        1 => (200, common::sse_text("Completed after recovery.")),
        _ => (401, String::new()),
    });
    let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, true, limits(1));
    agent.run_turn("Complete the task.").await.unwrap();
    assert!(!fixture.workspace.join("unsafe").exists());
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0], requests[1]);
    assert!(!fixture.events().iter().any(|e| e["type"] == "tool"));
}

#[tokio::test]
async fn patch_preview_apply_approval_denial_staleness_and_stop_use_the_real_agent_path() {
    for choice in ["allow", "deny", "stale", "stop"] {
        let fixture = Fixture::new();
        std::fs::write(fixture.workspace.join("a"), "old-a").unwrap();
        std::fs::write(fixture.workspace.join("b"), "old-b").unwrap();
        let edits = json!([
            {"path":"a","old_str":"old-a","new_str":"new-a"},
            {"path":"b","old_str":"old-b","new_str":"new-b"}
        ]);
        let (endpoint, requests) = mock(move |index, request| match index {
            0 => (
                200,
                chat_tool("patch_files", json!({"action":"preview","edits":edits})),
            ),
            1 => {
                let result = request["messages"].as_array().unwrap().last().unwrap()["content"]
                    .as_str()
                    .unwrap();
                assert!(result.contains("action: preview"));
                let id = result
                    .lines()
                    .find_map(|line| line.strip_prefix("preview_id: "))
                    .unwrap();
                (
                    200,
                    chat_tool(
                        "patch_files",
                        json!({"action":"apply","edits":edits,"preview_id":id}),
                    ),
                )
            }
            2 => (200, common::sse_text("Patch disposition recorded.")),
            _ => (401, String::new()),
        });
        let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, false, limits(4));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        agent.wire_ui(tx, Arc::new(tokio::sync::Notify::new()), stop.clone(), None);
        let task = tokio::spawn(async move {
            let result = agent.run_turn("Patch both files.").await;
            (agent, result)
        });
        let permission = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if let Some(UiEvent::Permission { summary, reply, .. }) = rx.recv().await {
                    break (summary, reply);
                }
            }
        })
        .await
        .unwrap();
        assert!(permission.0.starts_with("patch apply"));
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("a")).unwrap(),
            "old-a"
        );
        assert_eq!(
            std::fs::read_to_string(fixture.workspace.join("b")).unwrap(),
            "old-b"
        );
        if choice == "stale" {
            std::fs::write(fixture.workspace.join("a"), "external-change").unwrap();
        }
        if choice == "stop" {
            stop.store(true, Ordering::Relaxed);
        }
        permission
            .1
            .send(if choice == "deny" {
                GateChoice::Deny
            } else {
                GateChoice::Once
            })
            .unwrap();
        let (_, result) = tokio::time::timeout(Duration::from_secs(3), task)
            .await
            .unwrap()
            .unwrap();
        result.unwrap();
        let a = std::fs::read_to_string(fixture.workspace.join("a")).unwrap();
        let b = std::fs::read_to_string(fixture.workspace.join("b")).unwrap();
        assert_eq!(
            a,
            match choice {
                "allow" => "new-a",
                "stale" => "external-change",
                _ => "old-a",
            }
        );
        assert_eq!(b, if choice == "allow" { "new-b" } else { "old-b" });
        let events = fixture.events();
        let tools: Vec<_> = events.iter().filter(|e| e["type"] == "tool").collect();
        assert_eq!(tools.len(), 2);
        assert_eq!(
            tools[1]["data"]["status"],
            match choice {
                "allow" => "ok",
                "deny" => "denied",
                "stale" => "error",
                _ => "cancelled",
            }
        );
        assert_eq!(
            requests.lock().unwrap().len(),
            if choice == "stop" { 2 } else { 3 }
        );
    }
}

#[tokio::test]
async fn session_info_reports_current_run_and_survives_resume_into_a_new_run() {
    let mut fixture = Fixture::new();
    let (endpoint, _) = mock(|index, _| {
        (
            200,
            if index % 2 == 0 {
                chat_tool("session_info", json!({}))
            } else {
                common::sse_text("Session information recorded.")
            },
        )
    });
    let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, false, limits(3));
    agent.record_session(Some("fixture".into())).unwrap();
    agent
        .run_turn("Where is this conversation saved?")
        .await
        .unwrap();
    let info = |events: Vec<Value>| -> Value {
        let event = events
            .into_iter()
            .find(|e| e["type"] == "tool" && e["data"]["name"] == "session_info")
            .unwrap();
        serde_json::from_str(
            event["data"]["result"]
                .as_str()
                .unwrap()
                .strip_prefix("status: success\n")
                .unwrap(),
        )
        .unwrap()
    };
    let first = info(fixture.events());
    assert_eq!(first["run_id"], "session-a");
    assert_eq!(first["journal_path"], json!(fixture.run.join("solo.jsonl")));
    drop(agent);
    let saved = sui::session::SavedSession::load(
        &fixture.root.join("runs"),
        "session-a",
        &fixture.workspace,
    )
    .unwrap();
    fixture.run = fixture.root.join("runs/session-b");
    saved.fork(&fixture.run, "solo").unwrap();
    let mut resumed = fixture.agent(&endpoint, Transport::ChatCompletions, false, limits(3));
    resumed.restore_session(&saved).unwrap();
    resumed
        .run_turn("Where is the resumed conversation saved?")
        .await
        .unwrap();
    let events = fixture.events();
    let last = events
        .iter()
        .rev()
        .find(|e| e["type"] == "tool" && e["data"]["name"] == "session_info")
        .unwrap();
    let last: Value = serde_json::from_str(
        last["data"]["result"]
            .as_str()
            .unwrap()
            .strip_prefix("status: success\n")
            .unwrap(),
    )
    .unwrap();
    assert_eq!(last["run_id"], "session-b");
    assert_eq!(last["session_id"], first["session_id"]);
    assert_eq!(last["journal_path"], json!(fixture.run.join("solo.jsonl")));
    assert_eq!(last["export_command"], "sui export --run 'session-b'");
    assert_eq!(last["runtime_evidence"]["tool_results"]["session_info"], 1);
}

#[tokio::test]
async fn compaction_keeps_runtime_results_separate_from_invented_summary_claims() {
    let fixture = Fixture::new();
    let (endpoint, requests) = mock(|index, request| match index {
        0 => (
            200,
            chat_tool("bash", json!({"command":"printf 'runtime-check\\n'"})),
        ),
        1 => (200, common::sse_text("Command completed.")),
        2 => (
            200,
            common::sse_text("The command failed. read_file returned clean — nothing to commit."),
        ),
        3 => {
            let messages = request["messages"].as_array().unwrap();
            let checkpoint = messages[1]["content"].as_str().unwrap();
            let receipt = checkpoint
                .split("<runtime_evidence>\n")
                .nth(1)
                .unwrap()
                .split("\n</runtime_evidence>")
                .next()
                .unwrap();
            let receipt: Value = serde_json::from_str(receipt).unwrap();
            assert_eq!(receipt["records"][0]["name"], "bash");
            assert_eq!(receipt["records"][0]["exit_code"], 0);
            assert_eq!(receipt["records"][0]["status"], "ok");
            assert!(receipt["tool_results"]["read_file"].is_null());
            assert_eq!(messages.last().unwrap()["content"], "What actually passed?");
            (200, common::sse_text("The recorded command exited zero."))
        }
        _ => (401, String::new()),
    });
    let mut base =
        sui::context::estimate_tokens(&sui::context::compile(&[], &sui::context::system(), None))
            + serde_json::to_vec(&sui::tools::schemas()).unwrap().len() / 4;
    let facts = sui::skills::facts(&fixture.workspace);
    let selected = sui::skills::select("Run the fixture check.", &facts, "worker");
    base += sui::skills::guidance_block("Run the fixture check.", &facts, &selected).len() / 4;
    let mut lim = limits(4);
    lim.compact_context = true;
    lim.context_budget = base + 35_000 / 4 + 2000;
    let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, true, lim);
    agent.run_turn("Run the fixture check.").await.unwrap();
    let mut history = agent.history().to_vec();
    history.push(Message::Assistant {
        content: Some("x".repeat(35_000)),
        tool_calls: None,
        reasoning_content: None,
        response_items: vec![],
    });
    agent.restore_history(history);
    agent.run_turn("What actually passed?").await.unwrap();
    assert_eq!(requests.lock().unwrap().len(), 4);
    assert!(fixture
        .events()
        .iter()
        .any(|e| e["type"] == "context_checkpoint"));
    assert_eq!(
        serde_json::to_value(replay_history(&fixture.run.join("solo.jsonl"), usize::MAX).unwrap())
            .unwrap(),
        serde_json::to_value(agent.history()).unwrap()
    );
}

#[tokio::test]
async fn exhausted_limits_report_unfinished_work_instead_of_success() {
    let fixture = Fixture::new();
    let (endpoint, requests) =
        mock(|_, _| (200, chat_tool("read_file", json!({"path":"missing"}))));
    let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, true, limits(1));
    assert!(agent
        .run_turn("Finish the task.")
        .await
        .unwrap_err()
        .to_string()
        .contains("max_turns"));
    assert_eq!(requests.lock().unwrap().len(), 1);
    assert!(fixture
        .events()
        .iter()
        .any(|e| e["type"] == "turn_end" && e["data"]["outcome"] == "error"));
    let mut lim = limits(1);
    lim.context_budget = 1;
    let mut agent = fixture.agent(&endpoint, Transport::ChatCompletions, true, lim);
    assert!(agent
        .run_turn("Finish the task.")
        .await
        .unwrap_err()
        .to_string()
        .contains("context budget"));
    assert_eq!(requests.lock().unwrap().len(), 1);
}
