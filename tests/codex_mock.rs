//! Codex-OAuth backend: message/tool translation and auth handling.
//! The live smoke test runs only with `--ignored` and a real codex login.

use serde_json::json;
use sui::types::{FunctionCall, Message, ToolCall};

#[test]
fn message_response_items_skip_serializing() {
    // Raw replay items must never leak into chat-completions JSON.
    let m = Message::Assistant {
        content: Some("hi".into()),
        tool_calls: None,
        reasoning_content: None,
        response_items: vec![json!({"type":"reasoning","encrypted_content":"x"})],
    };
    let v = serde_json::to_value(&m).unwrap();
    assert!(v.get("response_items").is_none());
    assert_eq!(v["role"], "assistant");
}

#[test]
fn toolcall_to_function_call_shape() {
    let tc = ToolCall {
        id: "call_1".into(),
        kind: "function".into(),
        function: FunctionCall {
            name: "fs".into(),
            arguments: "{}".into(),
        },
    };
    let v = serde_json::to_value(&tc).unwrap();
    assert_eq!(v["function"]["name"], "fs");
}

/// Live: a full agent-loop cycle through `Provider` — force a tool call,
/// feed the result back, expect a text answer. Verifies function_call /
/// function_call_output / reasoning-replay / usage end to end.
/// Run: cargo test --test codex_mock codex_oauth_smoke -- --ignored
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore]
async fn codex_oauth_smoke() {
    use sui::provider::Provider;
    let p = Provider::new(
        "codex://oauth",
        None,
        "gpt-5.5".into(),
        Some("sui-smoke".into()),
    );

    let tools = vec![json!({
        "type": "function",
        "function": {"name": "get_number", "description": "returns the number 42",
            "parameters": {"type": "object", "properties": {}}}
    })];
    let mut msgs = vec![
        Message::System {
            content: "You have tools. Always call get_number when asked.".into(),
        },
        Message::User {
            content: "What is the number? Use the tool.".into(),
        },
    ];

    let out1 = p
        .stream_chat(&msgs, &tools, |_| {}, |_| {})
        .await
        .expect("turn 1");
    eprintln!(
        "t1: model={:?} calls={:?} usage={:?}",
        out1.returned_model,
        out1.tool_calls
            .iter()
            .map(|c| &c.function.name)
            .collect::<Vec<_>>(),
        out1.usage
    );
    assert!(
        !out1.tool_calls.is_empty(),
        "expected a tool call, got: {:?}",
        out1.content
    );
    assert!(out1.usage.is_some());

    // Feed the tool result back — exercises function_call_output +
    // reasoning-item replay on the second turn.
    let call = out1.tool_calls[0].clone();
    msgs.push(Message::Assistant {
        content: if out1.content.is_empty() {
            None
        } else {
            Some(out1.content.clone())
        },
        tool_calls: Some(out1.tool_calls.clone()),
        reasoning_content: out1.reasoning_content.clone(),
        response_items: out1.response_items.clone(),
    });
    msgs.push(Message::Tool {
        tool_call_id: call.id.clone(),
        content: "42".into(),
    });

    let out2 = p
        .stream_chat(&msgs, &tools, |_| {}, |_| {})
        .await
        .expect("turn 2");
    eprintln!("t2: content={:?} usage={:?}", out2.content, out2.usage);
    assert!(out2.content.contains("42"), "got: {}", out2.content);
}
