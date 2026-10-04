pub(crate) mod responses;

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
    image_input: bool,
    prompt_cache_key: Option<String>,
    session_id: String,
}

enum Inner {
    Responses {
        url: String,
        api_key: Option<String>,
    },
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
            image_input: base_url.starts_with("codex://"),
            client: b.build().unwrap_or_else(|_| reqwest::Client::new()),
            inner,
            model,
            prompt_cache_key,
            session_id: format!("sui-{}-{:032x}", std::process::id(), rand::random::<u128>()),
        }
    }

    pub(crate) fn with_session_id(mut self, session_id: String) -> Self {
        self.session_id = session_id;
        self
    }

    pub fn from_profile(profile: &crate::config::Profile) -> Self {
        Self::new(
            &profile.base_url,
            profile.api_key.clone(),
            profile.model.clone(),
            profile.prompt_cache_key.clone(),
        )
        .with_transport(profile.transport)
        .with_image_input(profile.image_input)
    }

    pub fn with_transport(mut self, transport: crate::config::Transport) -> Self {
        if transport == crate::config::Transport::CodexOauth {
            self.inner = Inner::Codex(Default::default());
        }
        if transport == crate::config::Transport::OpenaiResponses {
            if let Inner::Chat { url, api_key } = self.inner {
                self.inner = Inner::Responses {
                    url: format!("{}/responses", url.trim_end_matches("/chat/completions")),
                    api_key,
                };
            }
        }
        self
    }

    /// Capability belongs to the resolved provider profile, never inferred
    /// from a model-name substring or advertised as verified by the runtime.
    pub fn with_image_input(mut self, enabled: bool) -> Self {
        self.image_input = enabled;
        self
    }
    pub fn image_input(&self) -> bool {
        self.image_input
    }

    /// One streaming request. `on_delta` receives content fragments as they
    /// arrive; `on_reasoning` receives provider-exposed reasoning fragments
    /// (`reasoning_content`, or OpenRouter-style `reasoning` when the former
    /// is absent — never both for the same delta). Opaque Responses items
    /// are preserved by Responses transports; Chat transports retain only
    /// exposed reasoning text and cannot promise opaque-state replay.
    pub async fn stream_chat(
        &self,
        messages: &crate::context::Compiled<'_>,
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
                        session_id: &self.session_id,
                    },
                    messages,
                    tools,
                    on_delta,
                    on_reasoning,
                )
                .await;
            }
            Inner::Responses { url, api_key } => {
                let start = Instant::now();
                let (instructions, input) = crate::codex::build_input(messages);
                let mut body = json!({
                    "model": self.model, "instructions": instructions, "input": input,
                    "tools": crate::codex::build_tools(tools), "tool_choice": "auto",
                    "store": false, "stream": true, "include": ["reasoning.encrypted_content"],
                });
                if let Some(key) = &self.prompt_cache_key {
                    body["prompt_cache_key"] = json!(key);
                }
                let mut http = self
                    .client
                    .post(url)
                    .header(
                        "session_id",
                        self.prompt_cache_key.as_deref().unwrap_or(&self.session_id),
                    )
                    .json(&body);
                if let Some(key) = api_key {
                    http = http.bearer_auth(key);
                }
                let response = http.send().await.context("send Responses request")?;
                if !response.status().is_success() {
                    let status = response.status();
                    // Provider errors may echo credentials; avoid persisting their body.
                    bail!("provider http {status}");
                }
                return responses::read(response, start, on_delta, on_reasoning).await;
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
        // Byte buffer: '\n' can never appear inside a UTF-8 multibyte
        // char, so byte-scanning for newlines and decoding complete LINES
        // is corruption-free — unlike lossy-decoding each raw chunk,
        // which splits a multibyte char at a boundary into two U+FFFDs.
        let mut buf: Vec<u8> = Vec::new();
        let mut content = String::new();
        let mut reasoning: Option<String> = None;
        let mut calls: BTreeMap<u32, CallAcc> = BTreeMap::new();
        let mut usage: Option<Usage> = None;
        let mut finish_reason: Option<String> = None;
        let mut returned_model: Option<String> = None;
        let mut first_delta_ms: Option<u128> = None;

        let mut handle_line = |line: &str| -> Result<()> {
            if line.is_empty() || line.starts_with(':') {
                return Ok(()); // blank line / comment keep-alive
            }
            let Some(data) = line.strip_prefix("data:") else {
                return Ok(());
            };
            let data = data.trim();
            if data == "[DONE]" {
                return Ok(());
            }
            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                return Ok(()); // tolerate non-JSON keep-alive lines
            };

            // In-stream error events (e.g. OpenRouter emits these after
            // 200). `error` must be an object — some providers send an
            // explicit `"error": null` on normal chunks.
            if let Some(err) = ev.get("error").filter(|e| e.is_object()) {
                let msg = err["message"].as_str().unwrap_or("unknown stream error");
                bail!("provider stream error: {}", truncate(msg, 300));
            }
            if let Some(m) = ev["model"].as_str() {
                returned_model = Some(m.to_string());
            }
            // `"usage": null` is a placeholder, not telemetry — keep
            // `usage` None so callers don't mistake it for complete data.
            if let Some(u) = ev.get("usage").filter(|u| u.is_object()) {
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
                    if first_delta_ms.is_none() {
                        first_delta_ms = Some(start.elapsed().as_millis());
                    }
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
            Ok(())
        };

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("stream read failed (interrupted)")?;
            buf.extend_from_slice(&chunk);

            // scan by index — drain once per chunk, no per-line alloc
            let mut pos = 0usize;
            while let Some(nl) = buf[pos..].iter().position(|&b| b == b'\n') {
                let end = pos + nl;
                let line = String::from_utf8_lossy(&buf[pos..end]);
                pos = end + 1;
                handle_line(line.trim_end_matches('\r'))?;
            }
            buf.drain(..pos);
        }
        // A truncated stream can end mid-line — still parse what arrived
        // (a complete final event without its newline still counts).
        if !buf.is_empty() {
            let line = String::from_utf8_lossy(&buf);
            handle_line(line.trim_end_matches('\r'))?;
        }

        // A usage chunk on a truncated stream is partial telemetry, not
        // a completed response — mark it complete only when the stream
        // actually finished.
        if let Some(u) = &mut usage {
            u.complete = finish_reason.is_some();
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
    if base_url.starts_with("codex://") {
        // The ChatGPT-OAuth Responses backend has no anonymous /models
        // catalog — offer the known Codex family. The picker's filter
        // box still accepts free-form names for newly released models.
        return Ok([
            "gpt-5.3-codex",
            "gpt-5.2-codex",
            "gpt-5.1-codex",
            "codex-mini-latest",
        ]
        .iter()
        .map(|id| ModelInfo {
            id: (*id).to_string(),
            context_length: None,
            price_in: None,
            price_out: None,
            tools_claimed: None,
        })
        .collect());
    }
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
    probe_with_transport(
        base_url,
        api_key,
        model,
        crate::config::Transport::default(),
    )
    .await
}

pub async fn probe_with_transport(
    base_url: &str,
    api_key: Option<&str>,
    model: &str,
    transport: crate::config::Transport,
) -> Result<Probe> {
    let p = Provider::new(base_url, api_key.map(String::from), model.to_string(), None)
        .with_transport(transport);
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
            &crate::context::Compiled::view(&msgs),
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
            .or_else(|| g(&u["prompt_tokens_details"]["cache_write_tokens"]))
            .or_else(|| g(&u["prompt_tokens_details"]["cache_creation_tokens"])),
        output_tokens: g(&u["completion_tokens"]),
        // The stream proved the response complete only if a finish_reason
        // arrived — stream_chat stamps this after the loop.
        complete: false,
        estimated: u["estimated"].as_bool().unwrap_or(false),
    }
}

pub(crate) fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        let i = crate::context::floor_char_boundary(s, n);
        format!("{}...", &s[..i])
    }
}
