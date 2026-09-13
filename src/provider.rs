use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Instant;

use crate::types::{FunctionCall, Message, ToolCall, Usage};

/// Minimal OpenAI-compatible client: POST {base}/chat/completions, SSE stream.
/// Transport is OpenAI-compatible; individual provider/model combinations
/// still require live certification (cache fields, reasoning replay, etc).
/// A `codex://` base URL switches to the ChatGPT-OAuth Responses backend
/// (`src/codex.rs`).
pub struct Provider {
    client: reqwest::Client,
    inner: Inner,
    model: String,
    prompt_cache_key: Option<String>,
}

enum Inner {
    Chat {
        url: String,
        api_key: Option<String>,
    },
    Codex(std::sync::OnceLock<std::sync::Arc<crate::codex::CodexAuth>>),
}

pub struct StreamOutcome {
    pub content: String,
    pub reasoning_content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// None means the stream ended without a finish_reason — i.e. it was
    /// interrupted. Callers must not execute tool calls in that case.
    pub finish_reason: Option<String>,
    /// Model name as reported by the endpoint (may differ from requested).
    pub returned_model: Option<String>,
    /// None = no usage chunk arrived (unknown, not zero).
    pub usage: Option<Usage>,
    /// Milliseconds from request send to first streamed delta.
    pub first_delta_ms: u128,
    /// Total request wall time.
    pub total_ms: u128,
    /// Raw Responses-API items to replay next turn (codex-oauth only;
    /// carries encrypted reasoning across store:false turns). Empty for
    /// chat-completions providers.
    pub response_items: Vec<serde_json::Value>,
}

#[derive(Default)]
struct CallAcc {
    id: String,
    name: String,
    args: String,
}

impl Provider {
    pub fn new(
        base_url: &str,
        api_key: Option<String>,
        model: String,
        prompt_cache_key: Option<String>,
    ) -> Self {
        // Authenticated requests never follow redirects: a redirect would
        // carry credentials to whatever the endpoint points at. Codex's
        // token never travels anywhere but chatgpt.com / auth.openai.com.
        let mut b = reqwest::Client::builder();
        if api_key.is_some() || base_url.starts_with("codex://") {
            b = b.redirect(reqwest::redirect::Policy::none());
        }
        let inner = if base_url.starts_with("codex://") {
            Inner::Codex(std::sync::OnceLock::new())
        } else {
            Inner::Chat {
                url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
                api_key,
            }
        };
        Self {
            client: b.build().unwrap_or_else(|_| reqwest::Client::new()),
            inner,
            model,
            prompt_cache_key,
        }
    }

    /// One streaming request. `on_delta` receives content fragments as they
    /// arrive; `on_reasoning` receives provider-exposed reasoning fragments
    /// (`reasoning_content`, or OpenRouter-style `reasoning` when the former
    /// is absent — never both for the same delta). Opaque reasoning blobs
    /// (e.g. reasoning_details objects) are preserved in the outcome but
    /// never pushed through `on_reasoning` — they aren't displayable text.
    pub async fn stream_chat(
        &self,
        messages: &[Message],
        tools: &[Value],
        mut on_delta: impl FnMut(&str),
        mut on_reasoning: impl FnMut(&str),
    ) -> Result<StreamOutcome> {
        let (url, api_key) = match &self.inner {
            Inner::Codex(auth) => {
                // OnceLock::get_or_try_init is unstable — a benign double
                // discover() just reads the same file twice.
                if auth.get().is_none() {
                    let _ = auth.set(crate::codex::CodexAuth::discover()?);
                }
                let auth = auth.get().unwrap().clone();
                return crate::codex::stream_responses(
                    crate::codex::CodexReq {
                        auth: &auth,
                        client: &self.client,
                        model: &self.model,
                        prompt_cache_key: self.prompt_cache_key.as_deref(),
                    },
                    messages,
                    tools,
                    on_delta,
                    on_reasoning,
                )
                .await;
            }
            Inner::Chat { url, api_key } => (url.clone(), api_key.clone()),
        };
        let start = Instant::now();
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "tools": tools,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if let Some(k) = &self.prompt_cache_key {
            body["prompt_cache_key"] = json!(k);
        }

        let mut req = self.client.post(&url).json(&body);
        if let Some(k) = &api_key {
            req = req.bearer_auth(k);
        }
        let resp = req.send().await.context("send chat request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "provider http {}: {}",
                status.as_u16(),
                truncate(&text, 500)
            );
        }

        let mut stream = resp.bytes_stream();
        let mut buf = String::new();
        let mut content = String::new();
        let mut reasoning: Option<String> = None;
        let mut calls: BTreeMap<u32, CallAcc> = BTreeMap::new();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        let mut returned_model: Option<String> = None;
        let mut first_delta_ms: Option<u128> = None;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("stream read failed (interrupted)")?;
            buf.push_str(&String::from_utf8_lossy(&chunk));

            while let Some(nl) = buf.find('\n') {
                let line = buf[..nl].trim_end_matches('\r').to_string();
                buf.drain(..nl + 1);
                if line.is_empty() || line.starts_with(':') {
                    continue; // blank line / comment keep-alive
                }
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data == "[DONE]" {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    continue; // tolerate non-JSON keep-alive lines
                };

                // In-stream error events (e.g. OpenRouter emits these after 200)
                if let Some(err) = ev.get("error") {
                    let msg = err["message"].as_str().unwrap_or("unknown stream error");
                    bail!("provider stream error: {}", truncate(msg, 300));
                }
                if let Some(m) = ev["model"].as_str() {
                    returned_model = Some(m.to_string());
                }
                if let Some(u) = ev.get("usage") {
                    usage = Some(parse_usage(u));
                }
                for ch in ev["choices"].as_array().into_iter().flatten() {
                    if let Some(fr) = ch["finish_reason"].as_str() {
                        finish_reason = Some(fr.to_string());
                    }
                    let d = &ch["delta"];
                    if let Some(t) = d["content"].as_str() {
                        if first_delta_ms.is_none() {
                            first_delta_ms = Some(start.elapsed().as_millis());
                        }
                        content.push_str(t);
                        on_delta(t);
                    }
                    // Normalize reasoning fields: prefer reasoning_content,
                    // fall back to `reasoning` (OpenRouter). Equivalent
                    // fields are never both emitted for one delta.
                    if let Some(r) = d["reasoning_content"]
                        .as_str()
                        .or_else(|| d["reasoning"].as_str())
                    {
                        reasoning.get_or_insert_with(String::new).push_str(r);
                        on_reasoning(r);
                    }
                    for tc in d["tool_calls"].as_array().into_iter().flatten() {
                        if first_delta_ms.is_none() {
                            first_delta_ms = Some(start.elapsed().as_millis());
                        }
                        let idx = tc["index"].as_u64().unwrap_or(0) as u32;
                        let acc = calls.entry(idx).or_default();
                        if let Some(id) = tc["id"].as_str() {
                            acc.id.push_str(id);
                        }
                        if let Some(n) = tc["function"]["name"].as_str() {
                            acc.name.push_str(n);
                        }
                        if let Some(a) = tc["function"]["arguments"].as_str() {
                            acc.args.push_str(a);
                        }
                    }
                }
            }
        }

        let tool_calls = calls
            .into_values()
            .map(|a| ToolCall {
                id: a.id,
                kind: "function".into(),
                function: FunctionCall {
                    name: a.name,
                    arguments: a.args,
                },
            })
            .collect();

        Ok(StreamOutcome {
            content,
            reasoning_content: reasoning,
            tool_calls,
            finish_reason,
            returned_model,
            usage,
            first_delta_ms: first_delta_ms.unwrap_or(0),
            total_ms: start.elapsed().as_millis(),
            response_items: Vec::new(),
        })
    }
}

/// A model entry from a provider's catalog (GET {base}/models).
/// Fields are reported values — None where the provider doesn't publish.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    pub context_length: Option<u64>,
    /// USD per token, as reported (OpenRouter shape).
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    /// Provider-claimed tool support (OpenRouter supported_parameters).
    /// NOT a Sui certification — display as catalog metadata only.
    pub tools_claimed: Option<bool>,
}

/// GET {base}/models. Auth header when a key is present. No path munging —
/// whatever the user configured is where we go.
pub async fn list_models(base_url: &str, api_key: Option<&str>) -> Result<Vec<ModelInfo>> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut req = client.get(&url);
    if let Some(k) = api_key {
        req = req.bearer_auth(k);
    }
    let resp = req.send().await.context("list models")?;
    if !resp.status().is_success() {
        bail!("GET /models → http {}", resp.status().as_u16());
    }
    let body: Value = resp.json().await.context("parse /models")?;
    let mut out = vec![];
    for m in body["data"].as_array().into_iter().flatten() {
        let Some(id) = m["id"].as_str() else { continue };
        let tools_claimed = m["supported_parameters"]
            .as_array()
            .map(|a| a.iter().any(|p| p.as_str() == Some("tools")));
        out.push(ModelInfo {
            id: id.to_string(),
            context_length: m["context_length"].as_u64(),
            price_in: m["pricing"]["prompt"].as_str().and_then(|s| s.parse().ok()),
            price_out: m["pricing"]["completion"]
                .as_str()
                .and_then(|s| s.parse().ok()),
            tools_claimed,
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Capability status from an actual test request — not catalog claims.
#[derive(Debug, Clone, PartialEq)]
pub enum CapStatus {
    Verified,
    Unverified,
    Unsupported,
}

/// One small live request with a trivial tool: verifies streaming, tool
/// calling, and usage reporting in a single round trip. Costs one tiny
/// request — the UI must warn before invoking.
pub async fn probe(base_url: &str, api_key: Option<&str>, model: &str) -> Result<Probe> {
    let p = Provider::new(base_url, api_key.map(String::from), model.to_string(), None);
    let msgs = vec![Message::User {
        content: "Reply with the word ok.".into(),
    }];
    let tools = vec![json!({
        "type": "function",
        "function": {"name": "noop", "description": "does nothing",
            "parameters": {"type": "object", "properties": {}}}
    })];
    let mut streamed = false;
    let out = p
        .stream_chat(
            &msgs,
            &tools,
            |_| {
                streamed = true;
            },
            |_| {},
        )
        .await?;
    Ok(Probe {
        streaming: if streamed {
            CapStatus::Verified
        } else {
            CapStatus::Unsupported
        },
        tool_calls: if !out.tool_calls.is_empty()
            || out.finish_reason.as_deref() == Some("tool_calls")
        {
            CapStatus::Verified
        } else {
            CapStatus::Unverified
        },
        usage: if out.usage.is_some() {
            CapStatus::Verified
        } else {
            CapStatus::Unverified
        },
        model: out.returned_model.unwrap_or_else(|| model.to_string()),
    })
}

pub struct Probe {
    pub streaming: CapStatus,
    pub tool_calls: CapStatus,
    pub usage: CapStatus,
    pub model: String,
}

/// Normalize usage across providers. OpenAI nests cached tokens under
/// prompt_tokens_details; DeepSeek reports prompt_cache_hit/miss_tokens.
/// Missing fields stay None — unknown is not zero.
fn parse_usage(u: &Value) -> Usage {
    let g = |v: &Value| v.as_u64();
    Usage {
        input_tokens: g(&u["prompt_tokens"]),
        cache_read_tokens: g(&u["prompt_tokens_details"]["cached_tokens"])
            .or_else(|| g(&u["prompt_cache_hit_tokens"])),
        cache_write_tokens: g(&u["cache_write_tokens"])
            .or_else(|| g(&u["prompt_tokens_details"]["cache_write_tokens"])),
        output_tokens: g(&u["completion_tokens"]),
        complete: true,
    }
}

pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}...", &s[..n])
    }
}
