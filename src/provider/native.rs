//! Native wire adapters. The agent loop continues to consume one StreamOutcome.
mod anthropic;
mod copilot;
mod gemini;
use super::*;
use crate::auth::{self, LoginProvider};
use crate::config::Transport;

pub(super) struct Request<'a, 'm> {
    pub client: &'a reqwest::Client,
    pub base: &'a str,
    pub api_key: Option<&'a str>,
    pub model: &'a str,
    pub session_id: &'a str,
    pub messages: &'a crate::context::Compiled<'m>,
    pub tools: &'a [Value],
    pub catalog: &'a tokio::sync::OnceCell<Value>,
}
pub(super) async fn stream(
    transport: Transport,
    request: Request<'_, '_>,
    on_delta: impl FnMut(&str),
    on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    match transport {
        Transport::Anthropic => anthropic::stream(request, on_delta, on_reasoning).await,
        Transport::Gemini | Transport::GeminiOauth => {
            gemini::stream(
                request,
                transport == Transport::GeminiOauth,
                on_delta,
                on_reasoning,
            )
            .await
        }
        Transport::Copilot => copilot::stream(request, on_delta, on_reasoning).await,
        _ => unreachable!(),
    }
}
pub(super) async fn models(
    transport: Transport,
    base: &str,
    key: Option<&str>,
) -> Result<Vec<ModelInfo>> {
    match transport {
        Transport::Anthropic => anthropic::models(base, key).await,
        Transport::Gemini | Transport::GeminiOauth => {
            gemini::models(base, key, transport == Transport::GeminiOauth).await
        }
        Transport::Copilot => copilot::models().await,
        _ => unreachable!(),
    }
}
fn model(id: String) -> ModelInfo {
    ModelInfo {
        id,
        context_length: None,
        price_in: None,
        price_out: None,
        tools_claimed: None,
    }
}
// Lines are decoded only once complete, preserving split UTF-8. A truncated
// stream stays incomplete unless the adapter saw its terminal provider event.
async fn events(
    response: reqwest::Response,
    mut handle: impl FnMut(Value) -> Result<()>,
) -> Result<()> {
    if !response.status().is_success() {
        return Err(failure::Failure::response(response).await.into());
    }
    let mut bytes = response.bytes_stream();
    let mut pending = Vec::new();
    let mut received = 0usize;
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk.map_err(failure::Failure::transport)?;
        received = received.saturating_add(chunk.len());
        if received > 16_777_216 {
            bail!("native provider stream exceeds 16 MiB");
        }
        pending.extend_from_slice(&chunk);
        let mut pos = 0;
        while let Some(nl) = pending[pos..].iter().position(|b| *b == b'\n') {
            let end = pos + nl;
            if end - pos > 1_048_576 {
                bail!("provider event exceeds 1 MiB");
            }
            event_line(&pending[pos..end], &mut handle)?;
            pos = end + 1;
        }
        pending.drain(..pos);
        if pending.len() > 1_048_576 {
            bail!("provider event exceeds 1 MiB");
        }
    }
    if !pending.is_empty() {
        event_line(&pending, &mut handle)?;
    }
    Ok(())
}
fn event_line(line: &[u8], handle: &mut impl FnMut(Value) -> Result<()>) -> Result<()> {
    let line = std::str::from_utf8(line)
        .context("provider event is not UTF-8")?
        .trim_end_matches('\r');
    if let Some(data) = line.strip_prefix("data:") {
        if data.trim() == "[DONE]" {
            return Ok(());
        }
        let value = serde_json::from_str(data.trim()).context("invalid native provider event")?;
        handle(value)?;
    }
    Ok(())
}
fn append(out: &mut String, fragment: &str) -> Result<()> {
    if out.len().saturating_add(fragment.len()) > 4_194_304 {
        bail!("provider output exceeds 4 MiB");
    }
    out.push_str(fragment);
    Ok(())
}
fn finish(mut out: StreamOutcome, start: Instant, first: Option<u128>) -> StreamOutcome {
    out.first_delta_ms = first.unwrap_or(0);
    out.total_ms = start.elapsed().as_millis();
    if let Some(usage) = &mut out.usage {
        usage.complete = out.finish_reason.is_some();
    }
    out
}
fn empty() -> StreamOutcome {
    StreamOutcome {
        content: String::new(),
        reasoning_content: None,
        tool_calls: vec![],
        finish_reason: None,
        returned_model: None,
        usage: None,
        first_delta_ms: 0,
        total_ms: 0,
        response_items: vec![],
    }
}
fn image_source(url: &str) -> Result<(&str, &str)> {
    let rest = url
        .strip_prefix("data:")
        .context("native provider images require a data URL")?;
    let (mime, data) = rest
        .split_once(";base64,")
        .context("native provider images require base64")?;
    if !matches!(
        mime,
        "image/png" | "image/jpeg" | "image/webp" | "image/gif"
    ) {
        bail!("unsupported native provider image format");
    }
    Ok((mime, data))
}
