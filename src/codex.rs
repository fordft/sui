//! ChatGPT/Codex OAuth backend — reuse an existing `codex login` session
//! (or Sui's own PKCE login) to call `chatgpt.com/backend-api/codex/responses`.
//!
//! Trust boundary: the access token is read from `~/.codex/auth.json` (or
//! Sui's own `~/.config/sui/codex-auth.json`) and is only ever sent to
//! `chatgpt.com` and `auth.openai.com`. It is never written to journals,
//! logs, or model prompts. Refresh tokens rotate server-side — a refresh
//! writes the new token back so the shared chain with the Codex CLI stays
//! alive; a `refresh_token_reused` response means the CLI refreshed first,
//! so we re-read the file and retry once.
//!
//! Subscription access is not API billing: reported usage counts are real
//! tokens, but cost fields stay unknown — never reported as zero-dollar.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::provider::StreamOutcome;
use crate::types::{FunctionCall, Message, ToolCall, Usage};
use futures_util::StreamExt;
use rand::Rng;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ISSUER: &str = "https://auth.openai.com";
const RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const SCOPE: &str = "openid profile email offline_access api.connectors.read api.connectors.invoke";
/// Codex CLI refreshes when the access-token JWT expires within 5 minutes.
const REFRESH_WINDOW_SECS: u64 = 300;

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn rfc3339_now() -> String {
    // Compact UTC timestamp — enough for last_refresh bookkeeping.
    let s = now_secs();
    let days = s / 86400;
    let secs = s % 86400;
    let (y, m, d) = days_to_ymd(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub(crate) fn days_to_ymd(mut days: u64) -> (u64, u64, u64) {
    // Howard Hinnant's civil-from-days algorithm.
    days += 719_468;
    let era = days / 146_097;
    let doe = days % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (y + u64::from(m <= 2), m, d)
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    e.decode(s).ok()
}

fn b64url_encode(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(b)
}

/// Decode a JWT payload without verifying the signature — we only read
/// `exp` and account claims from a token we already possess.
fn jwt_claims(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    serde_json::from_slice(&b64url_decode(payload)?).ok()
}

fn jwt_exp(token: &str) -> Option<u64> {
    jwt_claims(token)?["exp"].as_u64()
}

fn jwt_account_id(token: &str) -> Option<String> {
    jwt_claims(token)?["https://api.openai.com/auth"]["chatgpt_account_id"]
        .as_str()
        .map(String::from)
}

/// Plan type from the id_token (plus/pro/business/…) — diagnostics only.
pub fn plan_type() -> Option<String> {
    let f = read_auth_file(&discover_path()?).ok()?;
    jwt_claims(f.tokens.id_token.as_deref()?)?
        .get("https://api.openai.com/auth")?
        .get("chatgpt_plan_type")?
        .as_str()
        .map(String::from)
}

#[derive(Debug, Deserialize)]
struct AuthFile {
    #[serde(default)]
    tokens: Tokens,
}

#[derive(Debug, Deserialize, Default, Clone)]
struct Tokens {
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    access_token: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
}

fn codex_home() -> PathBuf {
    std::env::var("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            std::env::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".codex")
        })
}

/// Sui's own token store — a login performed by `sui auth codex` writes
/// here so its refresh chain is independent of the Codex CLI's file.
fn own_store_path() -> PathBuf {
    std::env::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config/sui/codex-auth.json")
}

fn discover_path() -> Option<PathBuf> {
    let own = own_store_path();
    if own.exists() {
        return Some(own);
    }
    let codex = codex_home().join("auth.json");
    codex.exists().then_some(codex)
}

fn read_auth_file(path: &std::path::Path) -> Result<AuthFile> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))
}

/// Shared OAuth session. Cheap to clone; the token lives behind a mutex
/// so concurrent missions share one refresh chain.
pub struct CodexAuth {
    path: PathBuf,
    inner: Mutex<TokenSet>,
    client: reqwest::Client,
}

#[derive(Clone)]
struct TokenSet {
    access: String,
    refresh: String,
    account_id: Option<String>,
}

impl CodexAuth {
    /// Locate credentials: Sui's own store first, then `~/.codex/auth.json`.
    pub fn discover() -> Result<Arc<Self>> {
        let path = discover_path().ok_or_else(|| {
            anyhow::anyhow!(
                "no codex oauth session — run `codex login` (ChatGPT sign-in) \
                 or `sui auth codex`"
            )
        })?;
        Self::from_file(&path)
    }

    pub fn from_file(path: &std::path::Path) -> Result<Arc<Self>> {
        let f = read_auth_file(path)?;
        let access = f
            .tokens
            .access_token
            .clone()
            .context("auth file has no access_token — log in again")?;
        let refresh = f
            .tokens
            .refresh_token
            .clone()
            .context("auth file has no refresh_token — log in again")?;
        let account_id = f
            .tokens
            .account_id
            .clone()
            .or_else(|| jwt_account_id(&f.tokens.id_token.clone().unwrap_or_default()));
        Ok(Arc::new(Self {
            path: path.to_path_buf(),
            inner: Mutex::new(TokenSet {
                access,
                refresh,
                account_id,
            }),
            client: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30))
                .build()?,
        }))
    }

    /// A valid access token — refreshes when the JWT expires within the
    /// Codex CLI's 5-minute window (or already has).
    pub async fn access_token(&self) -> Result<(String, Option<String>)> {
        let t = self.inner.lock().unwrap().clone();
        let expired = match jwt_exp(&t.access) {
            Some(exp) => exp <= now_secs() + REFRESH_WINDOW_SECS,
            // No parseable exp — trust it and let a 401 trigger refresh.
            None => return Ok((t.access, t.account_id)),
        };
        if !expired {
            return Ok((t.access, t.account_id));
        }
        self.refresh(t).await
    }

    async fn refresh(&self, t: TokenSet) -> Result<(String, Option<String>)> {
        match self.refresh_once(&t.refresh).await {
            Ok(new) => {
                self.persist(&t, &new)?;
                let out = (new.access.clone(), new.account_id.clone());
                *self.inner.lock().unwrap() = new;
                Ok(out)
            }
            Err(e) => {
                // The Codex CLI may have rotated the chain under us —
                // re-read the file and retry once with the stored token.
                let msg = e.to_string();
                if !msg.contains("reused")
                    && !msg.contains("invalidated")
                    && !msg.contains("expired")
                {
                    return Err(e);
                }
                let disk = read_auth_file(&self.path).ok();
                let Some(disk) = disk else { return Err(e) };
                let Some(rt) = disk.tokens.refresh_token else {
                    return Err(e);
                };
                if rt == t.refresh {
                    return Err(e);
                }
                let new = self.refresh_once(&rt).await?;
                self.persist(&t, &new)?;
                let out = (new.access.clone(), new.account_id.clone());
                *self.inner.lock().unwrap() = new;
                Ok(out)
            }
        }
    }

    async fn refresh_once(&self, refresh_token: &str) -> Result<TokenSet> {
        let resp = self
            .client
            .post(format!("{ISSUER}/oauth/token"))
            .json(&json!({
                "client_id": CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": refresh_token,
            }))
            .send()
            .await
            .context("token refresh request")?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let text = resp.text().await.unwrap_or_default();
            bail!(
                "token refresh failed (http {status}): {}",
                crate::provider::truncate(&text, 300)
            );
        }
        let body: Value = resp.json().await.context("parse refresh response")?;
        let access = body["access_token"]
            .as_str()
            .context("refresh response missing access_token")?
            .to_string();
        let cur = self.inner.lock().unwrap().clone();
        Ok(TokenSet {
            account_id: jwt_account_id(&access).or_else(|| cur.account_id.clone()),
            access,
            // Rotated refresh token must persist — the old one is dead.
            refresh: body["refresh_token"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| refresh_token.to_string()),
        })
    }

    /// Merge new tokens back into the on-disk file (preserving other
    /// fields like OPENAI_API_KEY) with a tmp+rename so a crash mid-write
    /// can't truncate the shared auth file.
    fn persist(&self, old: &TokenSet, new: &TokenSet) -> Result<()> {
        let mut disk: Value = std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| json!({}));
        // Another writer (codex CLI) may have rotated first — only write
        // when the file still holds the token we refreshed from, otherwise
        // our new chain would overwrite a newer one.
        let disk_rt = disk["tokens"]["refresh_token"].as_str().unwrap_or("");
        if !disk_rt.is_empty() && disk_rt != old.refresh {
            return Ok(());
        }
        disk["auth_mode"] = json!("chatgpt");
        disk["tokens"]["access_token"] = json!(new.access);
        disk["tokens"]["refresh_token"] = json!(new.refresh);
        if let Some(a) = &new.account_id {
            disk["tokens"]["account_id"] = json!(a);
        }
        disk["last_refresh"] = json!(rfc3339_now());
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&disk)?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Chat-completions `Message` list → Responses-API `input` items +
/// `instructions`. Reasoning items captured on prior turns replay verbatim
/// (store:false requires it); ids/status are stripped.
fn build_input(messages: &[Message]) -> (String, Vec<Value>) {
    let mut instructions = String::new();
    let mut input = Vec::new();
    for m in messages {
        match m {
            Message::System { content } => {
                if !instructions.is_empty() {
                    instructions.push_str("\n\n");
                }
                instructions.push_str(content);
            }
            Message::User { content } => input.push(json!({
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": content}],
            })),
            Message::Assistant {
                content,
                tool_calls,
                response_items,
                ..
            } => {
                // Replay raw items (encrypted reasoning) first so the
                // model's chain-of-thought context stays intact.
                for item in response_items {
                    let mut item = item.clone();
                    if let Some(o) = item.as_object_mut() {
                        o.remove("id");
                        o.remove("status");
                    }
                    input.push(item);
                }
                if let Some(c) = content {
                    if !c.is_empty() {
                        input.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": c}],
                        }));
                    }
                }
                for tc in tool_calls.iter().flatten() {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.function.name,
                        "arguments": tc.function.arguments,
                    }));
                }
            }
            Message::Tool {
                tool_call_id,
                content,
            } => input.push(json!({
                "type": "function_call_output",
                "call_id": tool_call_id,
                "output": content,
            })),
        }
    }
    if instructions.is_empty() {
        instructions = "You are a coding assistant.".into();
    }
    (instructions, input)
}

/// Flatten chat-completions tool specs into the flat Responses shape.
fn build_tools(tools: &[Value]) -> Vec<Value> {
    tools
        .iter()
        .filter_map(|t| {
            let f = &t["function"];
            let name = f["name"].as_str()?;
            Some(json!({
                "type": "function",
                "name": name,
                "description": f["description"].as_str().unwrap_or(""),
                "strict": false,
                "parameters": f["parameters"],
            }))
        })
        .collect()
}

/// Request context for `stream_responses`.
pub struct CodexReq<'a> {
    pub auth: &'a CodexAuth,
    pub client: &'a reqwest::Client,
    pub model: &'a str,
    pub prompt_cache_key: Option<&'a str>,
}

/// One streaming Responses-API request against the Codex backend.
pub async fn stream_responses(
    req: CodexReq<'_>,
    messages: &[Message],
    tools: &[Value],
    mut on_delta: impl FnMut(&str),
    mut on_reasoning: impl FnMut(&str),
) -> Result<StreamOutcome> {
    let start = Instant::now();
    let (instructions, input) = build_input(messages);
    let body = json!({
        "model": req.model,
        "instructions": instructions,
        "input": input,
        "tools": build_tools(tools),
        "tool_choice": "auto",
        "parallel_tool_calls": false,
        "reasoning": {"effort": "high", "summary": "auto"},
        "store": false,
        "stream": true,
        "include": ["reasoning.encrypted_content"],
        "prompt_cache_key": req.prompt_cache_key,
    });
    let (token, account_id) = req.auth.access_token().await?;
    let session_id = req
        .prompt_cache_key
        .map(String::from)
        .unwrap_or_else(|| format!("sui-{}", now_secs()));

    let mut http = req
        .client
        .post(RESPONSES_URL)
        .bearer_auth(&token)
        .header("OpenAI-Beta", "responses=experimental")
        .header("originator", "sui")
        // The backend gates models on a minimum Codex-CLI version — we
        // report a current baseline so newly-released models work.
        .header("version", "0.153.0")
        .header("session_id", &session_id)
        .header("accept", "text/event-stream")
        .json(&body);
    if let Some(a) = &account_id {
        http = http.header("chatgpt-account-id", a);
    }
    let resp = http.send().await.context("send codex request")?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "codex http {status}: {}",
            crate::provider::truncate(&text, 500)
        );
    }

    let mut stream = resp.bytes_stream();
    let mut buf = String::new();
    let mut content = String::new();
    let mut reasoning: Option<String> = None;
    let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
    let mut replay_items: Vec<Value> = Vec::new();
    let mut usage: Option<Usage> = None;
    let mut returned_model: Option<String> = None;
    let mut first_delta_ms: Option<u128> = None;
    let mut terminal = false;
    let mut call_ord = 0u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("codex stream read failed (interrupted)")?;
        buf.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(nl) = buf.find('\n') {
            let line = buf[..nl].trim_end_matches('\r').to_string();
            buf.drain(..nl + 1);
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                continue;
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
                        // Replay verbatim next turn (store:false stateless mode).
                        "reasoning" => replay_items.push(item.clone()),
                        _ => {}
                    }
                }
                "response.completed" | "response.done" => {
                    let r = &ev["response"];
                    if let Some(m) = r["model"].as_str() {
                        returned_model = Some(m.to_string());
                    }
                    if let Some(u) = r.get("usage") {
                        usage = Some(Usage {
                            input_tokens: u["input_tokens"].as_u64(),
                            cache_read_tokens: u["input_tokens_details"]["cached_tokens"].as_u64(),
                            cache_write_tokens: None,
                            output_tokens: u["output_tokens"].as_u64(),
                            complete: true,
                        });
                    }
                    terminal = true;
                }
                "response.incomplete" => {
                    let why = ev["response"]["incomplete_details"]["reason"]
                        .as_str()
                        .unwrap_or("unknown");
                    bail!("codex response incomplete: {why}");
                }
                "response.failed" | "error" => {
                    let msg = ev["response"]["error"]["message"]
                        .as_str()
                        .or_else(|| ev["error"]["message"].as_str())
                        .or_else(|| ev["message"].as_str())
                        .unwrap_or("unknown codex error");
                    bail!(
                        "codex stream error: {}",
                        crate::provider::truncate(msg, 300)
                    );
                }
                _ => {}
            }
            if terminal {
                break;
            }
        }
        if terminal {
            break;
        }
    }

    let tool_calls: Vec<ToolCall> = calls.into_values().collect();
    Ok(StreamOutcome {
        content,
        reasoning_content: reasoning,
        tool_calls: tool_calls.clone(),
        finish_reason: Some(if tool_calls.is_empty() {
            "stop".into()
        } else {
            "tool_calls".into()
        }),
        returned_model,
        usage,
        first_delta_ms: first_delta_ms.unwrap_or(0),
        total_ms: start.elapsed().as_millis(),
        response_items: replay_items,
    })
}

// ── Login flows ─────────────────────────────────────────────────────

fn pkce_pair() -> (String, String) {
    use sha2::Digest;
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    let verifier = b64url_encode(&bytes);
    let challenge = b64url_encode(&sha2::Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// Save tokens from a completed login into Sui's own store (an
/// independent refresh chain — never overwrites codex's file).
fn save_own_store(id_token: &str, access: &str, refresh: &str) -> Result<PathBuf> {
    let path = own_store_path();
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }
    let account_id = jwt_account_id(id_token).or_else(|| jwt_account_id(access));
    let doc = json!({
        "auth_mode": "chatgpt",
        "tokens": {
            "id_token": id_token,
            "access_token": access,
            "refresh_token": refresh,
            "account_id": account_id,
        },
        "last_refresh": rfc3339_now(),
    });
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
    Ok(path)
}

struct Exchanged {
    id_token: String,
    access: String,
    refresh: String,
}

async fn exchange_code(client: &reqwest::Client, code: &str, verifier: &str) -> Result<Exchanged> {
    let resp = client
        .post(format!("{ISSUER}/oauth/token"))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", REDIRECT_URI),
            ("client_id", CLIENT_ID),
            ("code_verifier", verifier),
        ])
        .send()
        .await
        .context("code exchange request")?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        bail!(
            "code exchange failed (http {status}): {}",
            crate::provider::truncate(&text, 300)
        );
    }
    let body: Value = resp.json().await.context("parse token response")?;
    Ok(Exchanged {
        id_token: body["id_token"].as_str().context("no id_token")?.into(),
        access: body["access_token"]
            .as_str()
            .context("no access_token")?
            .into(),
        refresh: body["refresh_token"]
            .as_str()
            .context("no refresh_token")?
            .into(),
    })
}

/// Browser login: bind localhost:1455, print the authorize URL, wait for
/// the redirect. `--manual` skips the listener and asks the user to paste
/// the final callback URL (headless/SSH path — the browser's localhost is
/// a different machine than Sui's).
pub async fn login(manual: bool) -> Result<PathBuf> {
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()?;
    let (verifier, challenge) = pkce_pair();
    let state = b64url_encode(&{
        let mut b = [0u8; 16];
        rand::rng().fill_bytes(&mut b);
        b
    });
    let url = format!(
        "{ISSUER}/oauth/authorize?response_type=code&client_id={CLIENT_ID}\
         &redirect_uri={}&scope={}&code_challenge={challenge}\
         &code_challenge_method=S256&id_token_add_organizations=true\
         &codex_cli_simplified_flow=true&state={state}&originator=sui",
        url_encode(REDIRECT_URI),
        url_encode(SCOPE),
    );

    let code = if manual {
        println!("Open this URL, sign in, then paste the final callback URL:");
        println!("\n  {url}\n");
        print!("callback URL> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        parse_callback(line.trim())
            .map(|(c, _)| c)
            .context("could not parse callback URL — expected ...?code=...&state=...")?
    } else {
        println!("Open this URL to sign in with ChatGPT:");
        println!("\n  {url}\n");
        wait_for_callback(&state)?
    };
    let t = exchange_code(&client, &code, &verifier).await?;
    save_own_store(&t.id_token, &t.access, &t.refresh)
}

fn url_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

fn parse_callback(url: &str) -> Option<(String, String)> {
    let q = url.split('?').nth(1)?;
    let mut code = None;
    let mut state = None;
    for kv in q.split('&') {
        let mut it = kv.splitn(2, '=');
        match (it.next(), it.next()) {
            (Some("code"), Some(v)) => code = Some(percent_decode(v)),
            (Some("state"), Some(v)) => state = Some(percent_decode(v)),
            _ => {}
        }
    }
    Some((code?, state?))
}

fn percent_decode(s: &str) -> String {
    let mut out = String::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(h) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(h as char);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { ' ' } else { b[i] as char });
        i += 1;
    }
    out
}

fn wait_for_callback(expect_state: &str) -> Result<String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", CALLBACK_PORT))
        .context("bind localhost:1455 — is codex login already running? try --manual")?;
    listener.set_nonblocking(false)?;
    println!("Waiting for the callback on http://localhost:{CALLBACK_PORT} ...");
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if Instant::now() > deadline {
            bail!("callback timed out — retry with `sui auth codex --manual`");
        }
        listener.set_nonblocking(true)?;
        match listener.accept() {
            Ok((stream, _)) => {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                reader.read_line(&mut line)?;
                // GET /auth/callback?code=…&state=… HTTP/1.1
                let path = line.split_whitespace().nth(1).unwrap_or("");
                let parsed = parse_callback(path);
                let (body, ok) = match &parsed {
                    Some((_, s)) if s == expect_state => {
                        ("Sign-in complete — you can close this tab.", true)
                    }
                    _ => ("Sign-in failed — state mismatch.", false),
                };
                let mut stream = reader.into_inner();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(resp.as_bytes());
                if ok {
                    return Ok(parsed.unwrap().0);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => return Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FunctionCall;

    #[test]
    fn system_becomes_instructions() {
        let (inst, input) = build_input(&[
            Message::System {
                content: "sys".into(),
            },
            Message::User {
                content: "hi".into(),
            },
        ]);
        assert_eq!(inst, "sys");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn assistant_replays_items_then_calls() {
        let (inst, input) = build_input(&[
            Message::User {
                content: "u".into(),
            },
            Message::Assistant {
                content: Some("ok".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "c1".into(),
                    kind: "function".into(),
                    function: FunctionCall {
                        name: "fs".into(),
                        arguments: "{}".into(),
                    },
                }]),
                reasoning_content: None,
                response_items: vec![
                    json!({"type":"reasoning","id":"rs_9","status":"done","encrypted_content":"blob"}),
                ],
            },
            Message::Tool {
                tool_call_id: "c1".into(),
                content: "out".into(),
            },
        ]);
        assert_eq!(inst, "You are a coding assistant.");
        assert_eq!(input.len(), 5); // user, reasoning, assistant text, function_call, tool
                                    // reasoning item replayed with id/status stripped
        assert_eq!(input[1]["type"], "reasoning");
        assert!(input[1].get("id").is_none() && input[1].get("status").is_none());
        assert_eq!(input[1]["encrypted_content"], "blob");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
        assert_eq!(input[3]["type"], "function_call");
        assert_eq!(input[3]["call_id"], "c1");
        assert_eq!(input[4]["type"], "function_call_output");
    }

    #[test]
    fn tool_specs_flatten() {
        let out = build_tools(&[json!({
            "type": "function",
            "function": {"name": "fs", "description": "d",
                "parameters": {"type": "object"}}
        })]);
        assert_eq!(out[0]["name"], "fs");
        assert_eq!(out[0]["strict"], false);
        assert!(out[0]["parameters"]["type"] == "object");
    }

    #[test]
    fn jwt_roundtrip() {
        // exp + account claim — unsigned payload, we only read fields.
        let payload = b64url_encode(br#"{"exp":2000000000,"https://api.openai.com/auth":{"chatgpt_account_id":"acc_1","chatgpt_plan_type":"pro"}}"#);
        let tok = format!("h.{payload}.s");
        assert_eq!(jwt_exp(&tok), Some(2000000000));
        assert_eq!(jwt_account_id(&tok), Some("acc_1".into()));
        assert!(jwt_exp("not.a.jwt").is_none());
    }

    #[test]
    fn callback_parse() {
        let (c, s) = parse_callback("/auth/callback?code=abc%20123&state=st1").unwrap();
        assert_eq!(c, "abc 123");
        assert_eq!(s, "st1");
        assert!(parse_callback("/auth/callback?code=x").is_none());
    }

    #[test]
    fn date_encoding() {
        assert_eq!(days_to_ymd(0), (1970, 1, 1));
        assert_eq!(days_to_ymd(19723), (2024, 1, 1));
    }
}
