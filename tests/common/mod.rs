#![allow(dead_code)]

//! Shared mock chat-server shell — one HTTP POST → one SSE reply per
//! connection. The wire protocol lives here once; each test supplies
//! its scenario as a `handler` closure.
//!
//! Deliberate wire choice: a request whose body cannot be fully read is
//! never answered (skip the connection) — earlier copies either
//! answered a zero-filled body or skipped it; skipping is the honest
//! choice for a mock.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;

/// One tool_call block as OpenAI streams it.
pub fn tc(id: &str, name: &str, args: &str) -> Value {
    json!({"id": id, "type": "function",
           "function": {"name": name, "arguments": args}})
}

/// submit_result tool call wrapping a control-plane payload.
pub fn submit(payload: Value) -> Value {
    json!([tc(
        "s1",
        "submit_result",
        &json!({"payload": payload}).to_string()
    )])
}

/// SSE body for a single text reply.
pub fn sse_text(t: &str) -> String {
    let d = json!({"choices": [{"index": 0, "delta": {"role": "assistant",
        "content": t}, "finish_reason": "stop"}]});
    let u = json!({"choices": [], "usage": {"prompt_tokens": 100,
        "completion_tokens": 5,
        "prompt_tokens_details": {"cached_tokens": 50}}});
    format!("data: {d}\n\ndata: {u}\n\ndata: [DONE]\n\n")
}

/// SSE body for a tool-call reply.
pub fn sse_tool_calls(calls: Value) -> String {
    let d = json!({"choices": [{"index": 0, "delta": {"role": "assistant",
        "tool_calls": calls}, "finish_reason": "tool_calls"}]});
    let u = json!({"choices": [], "usage": {"prompt_tokens": 100,
        "completion_tokens": 10,
        "prompt_tokens_details": {"cached_tokens": 50}}});
    format!("data: {d}\n\ndata: {u}\n\ndata: [DONE]\n\n")
}

/// SSE body for an arbitrary payload chunk.
pub fn sse(payload: Value) -> String {
    let u = json!({"choices": [], "usage": {"prompt_tokens": 5,
        "completion_tokens": 2}});
    format!("data: {payload}\n\ndata: {u}\n\ndata: [DONE]\n\n")
}

/// Serve mock chat completions on 127.0.0.1:0 — returns the port.
/// `handler` sees the raw request body and parsed `messages` array and
/// returns the SSE body. Connections are handled serially on one
/// thread; a request whose body can't be read is skipped, never
/// answered.
pub fn serve<F>(handler: F) -> u16
where
    F: Fn(&[u8], &[Value]) -> String + Send + 'static,
{
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let mut s = match conn {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let t = line.trim();
                if t.is_empty() {
                    break;
                }
                let lt = t.to_lowercase();
                if let Some(v) = lt.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            if r.read_exact(&mut body).is_err() {
                continue;
            }
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let msgs = req["messages"].as_array().cloned().unwrap_or_default();
            let out = handler(&body, &msgs);
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                out.len(),
                out
            );
            if s.write_all(resp.as_bytes()).is_err() {
                continue;
            }
            let _ = s.flush();
        }
    });
    port
}
