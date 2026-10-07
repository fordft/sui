use super::*;
use crate::types::{ContentPart, UserContent};

fn headers(request: reqwest::RequestBuilder, token: &str) -> reqwest::RequestBuilder {
    request
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", token)
}
fn push(messages: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(previous) = messages.last_mut().filter(|m| m["role"] == role) {
        previous["content"].as_array_mut().unwrap().extend(blocks);
    } else {
        messages.push(json!({"role":role,"content":blocks}));
    }
}
pub(super) fn body(request: &Request<'_, '_>) -> Result<Value> {
    let mut system = Vec::new();
    let mut messages = Vec::new();
    for message in request.messages.iter() {
        match message {
            Message::System { content } => system.push(json!({"type":"text","text":content})),
            Message::User { content } => {
                let parts = match content {
                    UserContent::Text(text) => vec![json!({"type":"text","text":text})],
                    UserContent::Parts(parts) => parts.iter().map(|part| match part {
                        ContentPart::Text {text} => Ok(json!({"type":"text","text":text})),
                        ContentPart::ImageUrl {image_url} => {
                            let (mime,data) = image_source(&image_url.url)?;
                            Ok(json!({"type":"image","source":{"type":"base64","media_type":mime,"data":data}}))
                        }
                    }).collect::<Result<Vec<_>>>()?,
                };
                push(&mut messages, "user", parts);
            }
            Message::Assistant {
                content,
                tool_calls,
                response_items,
                ..
            } => {
                let mut parts = Vec::new();
                // Replay opaque thinking blocks through the existing private
                // sidecar channel; they never become chat-completions JSON.
                parts.extend(
                    response_items
                        .iter()
                        .filter(|p| {
                            matches!(p["type"].as_str(), Some("thinking" | "redacted_thinking"))
                        })
                        .cloned(),
                );
                if let Some(text) = content.as_ref().filter(|s| !s.is_empty()) {
                    parts.push(json!({"type":"text","text":text}));
                }
                for call in tool_calls.iter().flatten() {
                    parts.push(json!({"type":"tool_use","id":call.id,"name":call.function.name,
                        "input":serde_json::from_str::<Value>(&call.function.arguments).context("invalid tool arguments in history")?}));
                }
                push(&mut messages, "assistant", parts);
            }
            Message::Tool {
                tool_call_id,
                content,
            } => push(
                &mut messages,
                "user",
                vec![json!({"type":"tool_result","tool_use_id":tool_call_id,"content":content})],
            ),
        }
    }
    if let Some(last) = system.last_mut() {
        last["cache_control"] = json!({"type":"ephemeral"});
    }
    let mut tools: Vec<_> = request
        .tools
        .iter()
        .map(|t| {
            json!({
                "name":t["function"]["name"],"description":t["function"]["description"],
                "input_schema":t["function"]["parameters"],
            })
        })
        .collect();
    if let Some(last) = tools.last_mut() {
        last["cache_control"] = json!({"type":"ephemeral"});
    }
    Ok(
        json!({"model":request.model,"max_tokens":8192,"system":system,"messages":messages,"tools":tools,"stream":true}),
    )
}
pub(super) async fn stream(
    request: Request<'_, '_>,
    mut on_delta: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    let base = request.base;
    let key = request
        .api_key
        .context("Claude needs an Anthropic API key")?;
    let start = Instant::now();
    let response = headers(
        request
            .client
            .post(format!("{}/messages", base.trim_end_matches('/'))),
        key,
    )
    .json(&body(&request)?)
    .send()
    .await
    .map_err(failure::Failure::transport)?;
    read(response, start, &mut on_delta, &mut on_reasoning).await
}
async fn read(
    response: reqwest::Response,
    start: Instant,
    mut on_delta: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    let mut out = empty();
    let mut first = None;
    let mut calls: BTreeMap<u64, CallAcc> = BTreeMap::new();
    let mut thinking: BTreeMap<u64, Value> = BTreeMap::new();
    let mut stop = None;
    events(response, |event| {
        match event["type"].as_str() {
            Some("error") => return Err(failure::Failure::stream(&event["error"]).into()),
            Some("message_start") => {
                out.returned_model = event["message"]["model"].as_str().map(str::to_string);
                if event["message"]["usage"].is_object() {
                    out.usage = Some(usage(&event["message"]["usage"]));
                }
            }
            Some("content_block_start") => {
                let index = event["index"]
                    .as_u64()
                    .context("missing content block index")?;
                let block = &event["content_block"];
                match block["type"].as_str() {
                    Some("tool_use") => {
                        if calls.len() >= 256 {
                            bail!("too many native tool calls");
                        }
                        calls.insert(
                            index,
                            CallAcc {
                                id: block["id"].as_str().context("missing tool call id")?.into(),
                                name: block["name"].as_str().context("missing tool name")?.into(),
                                args: if block["input"].as_object().is_some_and(|o| !o.is_empty()) {
                                    block["input"].to_string()
                                } else {
                                    String::new()
                                },
                            },
                        );
                        first.get_or_insert_with(|| start.elapsed().as_millis());
                    }
                    Some("thinking" | "redacted_thinking") => {
                        thinking.insert(index, block.clone());
                    }
                    Some("text") => {
                        if let Some(text) = block["text"].as_str() {
                            append(&mut out.content, text)?;
                            if !text.is_empty() {
                                first.get_or_insert_with(|| start.elapsed().as_millis());
                                on_delta(text);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("content_block_delta") => {
                let index = event["index"]
                    .as_u64()
                    .context("missing content block index")?;
                let delta = &event["delta"];
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        if let Some(text) = delta["text"].as_str() {
                            append(&mut out.content, text)?;
                            first.get_or_insert_with(|| start.elapsed().as_millis());
                            on_delta(text);
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(args) = delta["partial_json"].as_str() {
                            append(
                                &mut calls
                                    .get_mut(&index)
                                    .context("tool delta has no content block")?
                                    .args,
                                args,
                            )?;
                        }
                    }
                    Some("thinking_delta") => {
                        if let Some(text) = delta["thinking"].as_str() {
                            append(out.reasoning_content.get_or_insert_with(String::new), text)?;
                            if let Some(block) = thinking.get_mut(&index) {
                                let mut previous =
                                    block["thinking"].as_str().unwrap_or("").to_string();
                                append(&mut previous, text)?;
                                block["thinking"] = json!(previous);
                            }
                            first.get_or_insert_with(|| start.elapsed().as_millis());
                            on_reasoning(text);
                        }
                    }
                    Some("signature_delta") => {
                        if let Some(signature) = delta["signature"].as_str() {
                            if let Some(block) = thinking.get_mut(&index) {
                                let mut previous =
                                    block["signature"].as_str().unwrap_or("").to_string();
                                append(&mut previous, signature)?;
                                block["signature"] = json!(previous);
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("message_delta") => {
                stop = event["delta"]["stop_reason"].as_str().map(|s| {
                    match s {
                        "tool_use" => "tool_calls",
                        "max_tokens" => "length",
                        "end_turn" | "stop_sequence" => "stop",
                        "refusal" => "content_filter",
                        other => other,
                    }
                    .into()
                });
                if let Some(output) = event["usage"]["output_tokens"].as_u64() {
                    out.usage.get_or_insert_with(Usage::default).output_tokens = Some(output);
                }
            }
            Some("message_stop") => {
                out.finish_reason = stop.clone();
            }
            _ => {}
        }
        Ok(())
    })
    .await?;
    out.tool_calls = calls
        .into_values()
        .map(|c| ToolCall {
            id: c.id,
            kind: "function".into(),
            function: FunctionCall {
                name: c.name,
                arguments: if c.args.is_empty() {
                    "{}".into()
                } else {
                    c.args
                },
            },
        })
        .collect();
    out.response_items = thinking.into_values().collect();
    Ok(finish(out, start, first))
}
fn usage(v: &Value) -> Usage {
    let ordinary = v["input_tokens"].as_u64();
    let read = v["cache_read_input_tokens"].as_u64();
    let write = v["cache_creation_input_tokens"].as_u64();
    Usage {
        input_tokens: ordinary.and_then(|n| n.checked_add(read?)?.checked_add(write?)),
        cache_read_tokens: read,
        cache_write_tokens: write,
        output_tokens: v["output_tokens"].as_u64(),
        complete: false,
        estimated: false,
    }
}
pub(super) async fn models(base: &str, key: Option<&str>) -> Result<Vec<ModelInfo>> {
    let client = auth::client()?;
    let key = key.context("Claude needs an Anthropic API key")?;
    let mut models = Vec::new();
    let mut after = None;
    for _ in 0..16 {
        let mut request = headers(
            client.get(format!("{}/models", base.trim_end_matches('/'))),
            key,
        )
        .query(&[("limit", "100")]);
        if let Some(id) = &after {
            request = request.query(&[("after_id", id)]);
        }
        let value = auth::json_response(
            request
                .send()
                .await
                .map_err(crate::provider::failure::Failure::transport)?,
            "Claude model catalog",
        )
        .await?;
        for item in value["data"].as_array().into_iter().flatten() {
            if let Some(id) = item["id"].as_str() {
                models.push(model(id.into()));
            }
        }
        if value["has_more"] != true {
            models.sort_by(|a, b| a.id.cmp(&b.id));
            return Ok(models);
        }
        after = Some(
            value["last_id"]
                .as_str()
                .context("Claude catalog pagination missing cursor")?
                .to_string(),
        );
    }
    bail!("Claude catalog exceeds 1600 models; narrow the endpoint catalog")
}
