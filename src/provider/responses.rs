//! Shared stateless Responses streaming decoder; preserves opaque reasoning.
use crate::provider::failure::Failure;
use crate::provider::StreamOutcome;
use crate::types::{FunctionCall, ToolCall, Usage};
use anyhow::{bail, Result};
use futures_util::StreamExt;
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Instant;

pub(crate) async fn read(
    resp: reqwest::Response,
    start: Instant,
    mut on_delta: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    let mut stream = resp.bytes_stream();
    // Byte buffer: '\n' can never appear inside a UTF-8 multibyte char,
    // so byte-scanning for newlines and decoding complete LINES is
    // corruption-free — unlike lossy-decoding each raw chunk, which
    // splits a multibyte char across a boundary into two U+FFFDs.
    let mut buf: Vec<u8> = Vec::new();
    let mut content = String::new();
    let mut reasoning: Option<String> = None;
    let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
    let mut replay_items: Vec<Value> = Vec::new();
    let mut replay_bytes = 0usize;
    let mut usage: Option<Usage> = None;
    let mut returned_model: Option<String> = None;
    let mut first_delta_ms: Option<u128> = None;
    let mut call_ord = 0u64;

    // Returns Ok(true) when the terminal response.completed/done event
    // arrived — kept outside the closure so the scan can check it.
    let mut handle_line = |line: &str| -> Result<bool> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(false);
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return Ok(false);
        }
        let Ok(ev) = serde_json::from_str::<Value>(data) else {
            return Ok(false);
        };
        match ev["type"].as_str().unwrap_or("") {
            "response.output_text.delta" => {
                if let Some(t) = ev["delta"].as_str() {
                    if first_delta_ms.is_none() {
                        first_delta_ms = Some(start.elapsed().as_millis());
                    }
                    content.push_str(t);
                    on_delta(t);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(t) = ev["delta"].as_str() {
                    if first_delta_ms.is_none() {
                        first_delta_ms = Some(start.elapsed().as_millis());
                    }
                    reasoning.get_or_insert_with(String::new).push_str(t);
                    on_reasoning(t);
                }
            }
            "response.output_item.done" => {
                let item = &ev["item"];
                match item["type"].as_str().unwrap_or("") {
                    "function_call" => {
                        if first_delta_ms.is_none() {
                            first_delta_ms = Some(start.elapsed().as_millis());
                        }
                        let call_id = item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap_or_default()
                            .to_string();
                        calls.insert(
                            call_ord,
                            ToolCall {
                                id: call_id,
                                kind: "function".into(),
                                function: FunctionCall {
                                    name: item["name"].as_str().unwrap_or_default().to_string(),
                                    arguments: item["arguments"]
                                        .as_str()
                                        .unwrap_or("{}")
                                        .to_string(),
                                },
                            },
                        );
                        call_ord += 1;
                    }
                    // Replay verbatim next turn (store:false stateless
                    // mode) — strip id/status once here so replay is a
                    // clone, not a mutate, on every later request.
                    "reasoning" => {
                        let mut item = item.clone();
                        if let Some(o) = item.as_object_mut() {
                            o.remove("id");
                            o.remove("status");
                        }
                        replay_bytes += serde_json::to_vec(&item)?.len();
                        if replay_bytes > 4 * 1024 * 1024 {
                            bail!("opaque replay state exceeds 4 MiB");
                        }
                        replay_items.push(item);
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.done" => {
                let r = &ev["response"];
                if let Some(m) = r["model"].as_str() {
                    returned_model = Some(m.to_string());
                }
                if let Some(u) = r.get("usage").filter(|u| u.is_object()) {
                    usage = Some(Usage {
                        input_tokens: u["input_tokens"].as_u64(),
                        cache_read_tokens: u["input_tokens_details"]["cached_tokens"].as_u64(),
                        cache_write_tokens: u["input_tokens_details"]["cache_write_tokens"]
                            .as_u64(),
                        output_tokens: u["output_tokens"].as_u64(),
                        complete: true,
                        estimated: u["estimated"].as_bool().unwrap_or(false),
                    });
                }
                return Ok(true);
            }
            "response.incomplete" => {
                let why = ev["response"]["incomplete_details"]["reason"]
                    .as_str()
                    .unwrap_or("unknown");
                return Err(Failure::incomplete(why).into());
            }
            "response.failed" | "error" => {
                let error = ev.get("error").unwrap_or(&ev["response"]["error"]);
                return Err(Failure::stream(error).into());
            }
            _ => {}
        }
        Ok(false)
    };

    let mut terminal = false;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(Failure::transport)?;
        buf.extend_from_slice(&chunk);
        if buf.len() > 16 * 1024 * 1024 {
            bail!("Responses stream line exceeds 16 MiB");
        }
        // scan by index — drain once per chunk, no per-line alloc
        let mut pos = 0usize;
        while let Some(nl) = buf[pos..].iter().position(|&b| b == b'\n') {
            let end = pos + nl;
            let line = String::from_utf8_lossy(&buf[pos..end]);
            pos = end + 1;
            if handle_line(line.trim_end_matches('\r'))? {
                terminal = true;
                break;
            }
        }
        buf.drain(..pos);
        if terminal {
            break;
        }
    }
    // A truncated stream can end mid-line — still parse what arrived
    // (a complete response.completed line without its newline counts).
    if !terminal && !buf.is_empty() {
        let line = String::from_utf8_lossy(&buf);
        terminal = handle_line(line.trim_end_matches('\r'))?;
    }

    let tool_calls: Vec<ToolCall> = calls.into_values().collect();
    // Honest finish reason: only a terminal event can claim the response
    // completed. A stream that just stops (drop, truncate, RST) is NOT
    // a clean "stop" — reporting None lets the caller refuse to treat
    // partial output as a finished turn.
    let finish_reason = if terminal {
        Some(if tool_calls.is_empty() {
            "stop".to_string()
        } else {
            "tool_calls".to_string()
        })
    } else {
        None
    };
    Ok(StreamOutcome {
        content,
        reasoning_content: reasoning,
        tool_calls,
        finish_reason,
        returned_model,
        usage,
        first_delta_ms: first_delta_ms.unwrap_or(0),
        total_ms: start.elapsed().as_millis(),
        response_items: replay_items,
    })
}
