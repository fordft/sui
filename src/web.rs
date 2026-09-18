//! First-class web research for the native agent loop: `web_search` +
//! `web_fetch`, backed by Exa's hosted MCP service. Sui-owned tools —
//! the model calls ordinary functions; the MCP connection is internal.
//!
//! Boundaries:
//! - Queries and requested URLs leave the machine — access policy
//!   (Off/Ask/Auto) is independent of tool auto-approve. YOLO never
//!   turns web access on.
//! - Retrieved content is untrusted external data, never instructions.
//! - Search snippets are not fetched content; cached results are not
//!   fresh results; agent-reported ACP search activity is never
//!   re-executed. Honesty fields record what actually happened.
//! - The API key rides only on the MCP endpoint URL — never into
//!   journals, tool envelopes, or model context.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rmcp::ClientServiceExt;

use crate::tools::ExecOut;

const DEFAULT_ENDPOINT: &str = "https://mcp.exa.ai/mcp";
/// Per-run upstream request cap (shared across all agents in a run).
const MAX_REQUESTS: usize = 30;
const MAX_CONCURRENT: usize = 4;
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SEARCH_RESULTS: u64 = 10;
const DEFAULT_SEARCH_RESULTS: u64 = 5;
const SNIPPET_CAP: usize = 400;
const SEARCH_TEXT_CAP: usize = 20_000;
const FETCH_TEXT_CAP: usize = 16_000;
const CACHE_CAP: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebAccess {
    /// Never send anything out. YOLO does not change this.
    #[default]
    Off,
    /// Ask through the normal permission gate (session grants apply).
    Ask,
    /// Allowed without prompting.
    Auto,
}
impl WebAccess {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Ask => "ask",
            Self::Auto => "auto",
        }
    }
}

#[derive(Debug, Clone)]
pub struct WebCfg {
    pub access: WebAccess,
    /// Optional Exa API key — anonymous access works but is rate-limited.
    pub api_key: Option<String>,
    pub endpoint: String,
}

/// `[web]` table from user-owned config (global / --config). Project
/// files may not raise network egress — `web` there is ignored.
pub fn load_cfg(config_path: Option<&Path>) -> WebCfg {
    let file_access = |p: &Path| -> Option<Value> {
        std::fs::read_to_string(p)
            .ok()
            .and_then(|s| s.parse::<toml::Value>().ok())
            .and_then(|v| v.get("web").cloned())
            .and_then(|w| serde_json::to_value(w).ok())
    };
    let mut access = WebAccess::default();
    let mut api_key: Option<String> = None;
    let mut endpoint = DEFAULT_ENDPOINT.to_string();
    let sources = [
        crate::config::global_config_path(),
        config_path.map(|p| p.to_path_buf()),
    ];
    for p in sources.into_iter().flatten() {
        if let Some(w) = file_access(&p) {
            if let Some(a) = w.get("access").and_then(|v| v.as_str()) {
                access = match a {
                    "auto" => WebAccess::Auto,
                    "ask" => WebAccess::Ask,
                    _ => WebAccess::Off,
                };
            }
            if let Some(k) = w.get("api_key").and_then(|v| v.as_str()) {
                api_key = Some(k.to_string());
            }
            if let Some(e) = w.get("endpoint").and_then(|v| v.as_str()) {
                endpoint = e.to_string();
            }
            if let Some(k) = w.get("key_env").and_then(|v| v.as_str()) {
                if let Ok(v) = std::env::var(k) {
                    if !v.is_empty() {
                        api_key = Some(v);
                    }
                }
            }
        }
    }
    if let Ok(a) = std::env::var("SUI_WEB_ACCESS") {
        access = match a.as_str() {
            "auto" => WebAccess::Auto,
            "ask" => WebAccess::Ask,
            _ => WebAccess::Off,
        };
    }
    if let Ok(k) = std::env::var("SUI_EXA_API_KEY") {
        if !k.is_empty() {
            api_key = Some(k);
        }
    }
    WebCfg {
        access,
        api_key,
        endpoint,
    }
}

/// One recorded source — stable per-run identity for citations/export.
#[derive(Debug, Clone)]
pub struct Source {
    pub id: String,
    pub title: String,
    pub url: String,
    /// Search snippet (kind=search) or fetched excerpt head (kind=fetch).
    pub excerpt: String,
    pub published: Option<String>,
    pub retrieved: String,
    /// "search" | "fetch" | "cache"
    pub kind: String,
    pub truncated: bool,
}

#[derive(Clone)]
struct Cached {
    text: String,
    fetched_at: Instant,
}

enum CacheEntry {
    Ready(Cached),
    InFlight(Arc<tokio::sync::Notify>),
}

struct State {
    cache: HashMap<String, CacheEntry>,
    sources: Vec<Source>,
    next_id: usize,
    client: Option<Arc<rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>>>,
    /// MCP transport died mid-call — don't silently reconnect into a
    /// possibly-side-effected fetch; a new WebService (next run) retries.
    dead: bool,
}

/// Per-run web service. Shared across a run's agents — request limits
/// span workers by construction, not per-agent bookkeeping.
pub struct WebService {
    cfg: WebCfg,
    st: Mutex<State>,
    reqs: AtomicUsize,
    sem: tokio::sync::Semaphore,
    /// Run id the counters belong to — reset when a new run starts.
    run: Mutex<u64>,
}

impl WebService {
    pub fn new(cfg: WebCfg) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            st: Mutex::new(State {
                cache: HashMap::new(),
                sources: Vec::new(),
                next_id: 1,
                client: None,
                dead: false,
            }),
            reqs: AtomicUsize::new(0),
            sem: tokio::sync::Semaphore::new(MAX_CONCURRENT),
            run: Mutex::new(0),
        })
    }

    /// Fresh counters for a new run (solo turns share one service).
    pub fn begin_run(&self, run: u64) {
        let mut r = self.run.lock().unwrap();
        if *r != run {
            *r = run;
            self.reqs.store(0, Ordering::Relaxed);
        }
    }

    pub fn access(&self) -> WebAccess {
        self.cfg.access
    }

    /// Sources recorded this run — for journal/export provenance.
    pub fn sources(&self) -> Vec<Source> {
        self.st.lock().unwrap().sources.clone()
    }

    /// Model-facing dispatch. Safety + policy are enforced inside too —
    /// the tool can never bypass them through another caller.
    pub async fn exec(self: &Arc<Self>, name: &str, args: &Value) -> ExecOut {
        if self.cfg.access == WebAccess::Off {
            return err_out("web research is Off (Settings → Web research → Ask/Auto)");
        }
        match name {
            "web_search" => self.search(args).await,
            "web_fetch" => self.fetch(args).await,
            _ => err_out("unknown web tool"),
        }
    }

    /// Settings "test search" — one tiny upstream call, explicit user
    /// action so it bypasses the access gate (Off included). Counts
    /// against the request budget like any other call.
    pub async fn test(self: &Arc<Self>) -> Result<String> {
        let text = self
            .upstream_call(
                "web_search_exa",
                json!({"query": "model context protocol", "objective": "official documentation", "numResults": 2}),
            )
            .await?;
        let sources = parse_search(&text, 2).sources;
        let n = sources.len();
        let first = sources
            .first()
            .map(|s| s.title.clone())
            .unwrap_or_else(|| "no results".into());
        Ok(format!("web ok — {n} sources · {first}"))
    }

    async fn search(self: &Arc<Self>, args: &Value) -> ExecOut {
        let query = args["query"].as_str().unwrap_or("").trim();
        if query.is_empty() {
            return err_out("web_search requires a non-empty `query`");
        }
        if let Some(why) = secret_flag(query) {
            return err_out(&format!(
                "rejected: query looks like it contains {why} — searches leave this machine"
            ));
        }
        let n = args["max_results"]
            .as_u64()
            .unwrap_or(DEFAULT_SEARCH_RESULTS)
            .clamp(1, MAX_SEARCH_RESULTS);
        let key = format!("s\u{0}{query}\u{0}{n}");
        self.cached_call(&key, |svc| async move {
            svc.upstream_call(
                "web_search_exa",
                json!({
                    "query": query,
                    // Exa requires an `objective` ranking hint — the query
                    // itself is the honest one; we don't invent a brief.
                    "objective": format!("authoritative documentation and primary sources for: {query}"),
                    "numResults": n,
                }),
            )
            .await
            .map(|text| parse_search(&text, n as usize))
        })
        .await
    }

    async fn fetch(self: &Arc<Self>, args: &Value) -> ExecOut {
        let url = args["url"].as_str().unwrap_or("").trim();
        if url.is_empty() {
            return err_out("web_fetch requires a non-empty `url`");
        }
        if let Err(why) = check_url(url) {
            return err_out(&format!("rejected: {why}"));
        }
        let key = format!("f\u{0}{url}");
        self.cached_call(&key, |svc| async move {
            svc.upstream_call(
                "web_fetch_exa",
                json!({
                    "urls": [url],
                    "maxCharacters": FETCH_TEXT_CAP as u64 * 2,
                }),
            )
            .await
            .map(|text| parse_fetch(&text, url))
        })
        .await
    }

    /// Dedup identical in-flight requests, reuse completed results
    /// within the run. A cached hit is labeled — never sold as fresh.
    async fn cached_call<F, Fut>(self: &Arc<Self>, key: &str, f: F) -> ExecOut
    where
        F: FnOnce(Arc<Self>) -> Fut,
        Fut: std::future::Future<Output = Result<Parsed>>,
    {
        enum Action {
            Hit(String, Instant),
            Miss,
            Wait(Arc<tokio::sync::Notify>),
        }
        let action = {
            let mut st = self.st.lock().unwrap();
            match st.cache.get(key) {
                Some(CacheEntry::Ready(c)) => Action::Hit(c.text.clone(), c.fetched_at),
                Some(CacheEntry::InFlight(n)) => Action::Wait(n.clone()),
                None => {
                    if st.cache.len() >= CACHE_CAP {
                        Action::Miss
                    } else {
                        let n = Arc::new(tokio::sync::Notify::new());
                        st.cache
                            .insert(key.to_string(), CacheEntry::InFlight(n.clone()));
                        Action::Miss
                    }
                }
            }
        };
        match action {
            Action::Hit(text, at) => ExecOut::plain(
                format!(
                    "{text}\ncache: hit (retrieved {}s ago this run)",
                    at.elapsed().as_secs()
                ),
                crate::tools::ExecKind::Success,
            ),
            Action::Wait(notify) => {
                // Wait for the in-flight twin, then read the ready entry.
                let _ =
                    tokio::time::timeout(CALL_TIMEOUT + Duration::from_secs(10), notify.notified())
                        .await;
                let text = {
                    let st = self.st.lock().unwrap();
                    match st.cache.get(key) {
                        Some(CacheEntry::Ready(c)) => Some(c.text.clone()),
                        _ => None,
                    }
                };
                match text {
                    Some(t) => ExecOut::plain(
                        format!("{t}\ncache: hit (shared with a concurrent request)"),
                        crate::tools::ExecKind::Success,
                    ),
                    None => err_out("concurrent request failed — retry"),
                }
            }
            Action::Miss => {
                let result = f(Arc::clone(self)).await;
                let mut st = self.st.lock().unwrap();
                let notify = match st.cache.remove(key) {
                    Some(CacheEntry::InFlight(n)) => Some(n),
                    _ => None,
                };
                match result {
                    Ok(parsed) => {
                        let mut sources = parsed.sources;
                        for s in &mut sources {
                            s.id = format!("S{}", st.next_id);
                            st.next_id += 1;
                            st.sources.push(s.clone());
                        }
                        let text = (parsed.render)(&sources);
                        st.cache.insert(
                            key.to_string(),
                            CacheEntry::Ready(Cached {
                                text: text.clone(),
                                fetched_at: Instant::now(),
                            }),
                        );
                        if let Some(n) = notify {
                            n.notify_waiters();
                        }
                        ExecOut::plain(text, crate::tools::ExecKind::Success)
                    }
                    Err(e) => {
                        // Failures are not cached — a retry is a fresh ask.
                        if let Some(n) = notify {
                            n.notify_waiters();
                        }
                        err_out(&format!("web request failed: {e:#}"))
                    }
                }
            }
        }
    }

    /// One bounded upstream call: run cap + semaphore + timeout + one
    /// retry on transport-class failures. MCP client is lazy — the first
    /// real call performs initialize, not startup.
    async fn upstream_call(&self, tool: &str, args: Value) -> Result<String> {
        if self.reqs.fetch_add(1, Ordering::Relaxed) >= MAX_REQUESTS {
            bail!("run web-request limit ({MAX_REQUESTS}) reached");
        }
        let _permit = self.sem.acquire().await?;
        let client = self.client().await?;
        let arguments = args.as_object().cloned().unwrap_or_default();
        let call = || async {
            let res = tokio::time::timeout(
                CALL_TIMEOUT,
                client.call_tool(
                    rmcp::model::CallToolRequestParams::new(tool.to_string())
                        .with_arguments(arguments.clone()),
                ),
            )
            .await;
            match res {
                Ok(Ok(r)) => {
                    if r.is_error == Some(true) {
                        bail!("{}", text_of(&r.content))
                    } else {
                        Ok(text_of(&r.content))
                    }
                }
                Ok(Err(e)) => bail!("mcp: {e}"),
                Err(_) => bail!("upstream timeout ({}s)", CALL_TIMEOUT.as_secs()),
            }
        };
        match call().await {
            Ok(t) => Ok(t),
            Err(e) => {
                // One retry on transport failures — not on tool errors,
                // which already reached the server.
                if e.to_string().starts_with("mcp:") || e.to_string().contains("timeout") {
                    tokio::time::sleep(Duration::from_millis(800)).await;
                    match call().await {
                        Err(e2)
                            if e2.to_string().starts_with("mcp:")
                                || e2.to_string().contains("timeout") =>
                        {
                            // Confirmed transport death — latch dead so
                            // later calls don't silently reconnect into a
                            // possibly-side-effected fetch (State.dead).
                            let mut st = self.st.lock().unwrap();
                            st.dead = true;
                            st.client = None;
                            Err(e2)
                        }
                        r => r,
                    }
                } else {
                    Err(e)
                }
            }
        }
    }

    async fn client(
        &self,
    ) -> Result<Arc<rmcp::service::RunningService<rmcp::RoleClient, rmcp::model::ClientConfig>>>
    {
        {
            let st = self.st.lock().unwrap();
            if st.dead {
                bail!("web backend connection is down for this run");
            }
            if let Some(c) = &st.client {
                return Ok(c.clone());
            }
        }
        let uri = match &self.cfg.api_key {
            Some(k) => format!("{}?exaApiKey={k}", self.cfg.endpoint),
            None => self.cfg.endpoint.clone(),
        };
        let transport = rmcp::transport::StreamableHttpClientTransport::from_uri(uri.as_str());
        let info = rmcp::model::ClientConfig::new(
            rmcp::model::ClientCapabilities::default(),
            rmcp::model::Implementation::new("sui", env!("CARGO_PKG_VERSION")),
        );
        // Classic initialize handshake — Exa doesn't implement rmcp's
        // server/discover extension and Auto's probe kills the session.
        let svc = info
            .serve_with_lifecycle(transport, rmcp::ClientLifecycleMode::Initialize)
            .await
            .context("mcp initialize")?;
        let mut st = self.st.lock().unwrap();
        Ok(st.client.insert(Arc::new(svc)).clone())
    }
}

fn text_of(content: &[rmcp::model::ContentBlock]) -> String {
    content
        .iter()
        .filter_map(|c| c.as_text().map(|t| t.text.clone()))
        .collect::<Vec<_>>()
        .join("\n")
}

fn err_out(msg: &str) -> ExecOut {
    ExecOut::plain(
        format!("status: error\nerror: {msg}"),
        crate::tools::ExecKind::Error,
    )
}

/// Parse output: raw sources + the renderer that produces tool-visible
/// text once global S-ids have been assigned by the service.
type Render = Box<dyn Fn(&[Source]) -> String + Send>;

struct Parsed {
    sources: Vec<Source>,
    render: Render,
}

/// Exa search text → compact source list. Snippets are labeled —
/// they are not fetched content.
fn parse_search(text: &str, want: usize) -> Parsed {
    let mut sources = Vec::new();
    // Each block starts with a "Title: <title>" line; "\nTitle: " splits
    // blocks. The first line of a block IS the title text.
    let mut rest = text.trim_start();
    while let Some(body) = rest.strip_prefix("Title: ") {
        let (block, tail) = match body.find("\nTitle: ") {
            Some(i) => (&body[..i], &body[i + 1..]),
            None => (body, ""),
        };
        rest = tail;
        let mut title = String::new();
        let mut url = String::new();
        let mut published = None;
        let mut hl = String::new();
        let mut in_hl = false;
        for (i, line) in block.lines().enumerate() {
            if i == 0 {
                title = line.trim().to_string();
            } else if let Some(u) = line.strip_prefix("URL: ") {
                url = u.trim().to_string();
            } else if let Some(p) = line.strip_prefix("Published: ") {
                let p = p.trim();
                published = (p != "N/A" && !p.is_empty()).then(|| p.to_string());
            } else if line.starts_with("Highlights:") {
                in_hl = true;
            } else if in_hl {
                hl.push_str(line);
                hl.push('\n');
            }
        }
        if url.is_empty() {
            continue;
        }
        let excerpt: String = hl.trim().chars().take(SNIPPET_CAP).collect();
        sources.push(Source {
            id: String::new(), // assigned by the service
            title,
            url,
            excerpt: excerpt.clone(),
            published,
            retrieved: rfc_now(),
            kind: "search".into(),
            truncated: hl.trim().chars().count() > SNIPPET_CAP,
        });
        if sources.len() >= want {
            break;
        }
    }
    Parsed {
        sources,
        render: Box::new(render_search),
    }
}

fn render_search(sources: &[Source]) -> String {
    let mut out = String::from("status: ok\n");
    if sources.is_empty() {
        out.push_str("sources: 0\n(empty result set — rephrase or fetch a known URL)\n");
        return out;
    }
    out.push_str(&format!("sources: {}\n", sources.len()));
    let mut body = String::new();
    for s in sources {
        body.push_str(&format!(
            "[{}] {}\n    {}\n    {}\n",
            s.id,
            s.title,
            s.url,
            s.excerpt.replace('\n', " ")
        ));
    }
    body.push_str("note: snippets ≠ fetched pages — use web_fetch(url) to read a source\n");
    out.push_str(&crate::provider::truncate(&body, SEARCH_TEXT_CAP));
    out
}

/// Exa fetch text ("# title\nURL: …\n\n<markdown>") → bounded page read.
fn parse_fetch(text: &str, want_url: &str) -> Parsed {
    let chars = text.chars().count();
    let truncated = chars > FETCH_TEXT_CAP;
    let body: String = text.chars().take(FETCH_TEXT_CAP).collect();
    let title = text
        .lines()
        .next()
        .unwrap_or("")
        .trim_start_matches('#')
        .trim()
        .to_string();
    let src = Source {
        id: String::new(),
        title,
        url: want_url.to_string(),
        excerpt: body.lines().take(5).collect::<Vec<_>>().join(" "),
        published: None,
        retrieved: rfc_now(),
        kind: "fetch".into(),
        truncated,
    };
    let header = format!(
        "status: ok\nsource: {}\nretrieved: {}\n{}\n",
        want_url,
        rfc_now(),
        if truncated {
            format!(
                "truncated: yes ({} chars shown of {})",
                FETCH_TEXT_CAP, chars
            )
        } else {
            "truncated: no".to_string()
        }
    );
    Parsed {
        sources: vec![src],
        render: Box::new(move |_: &[Source]| -> String { format!("{header}{body}") }),
    }
}

/// Reject non-web schemes, credential-bearing URLs, and non-public
/// targets before anything leaves the machine. This guards what we SEND
/// to the remote fetch service — Sui never fetches pages itself.
pub fn check_url(url: &str) -> Result<(), String> {
    let u = reqwest::Url::parse(url).map_err(|_| "not a valid URL".to_string())?;
    match u.scheme() {
        "http" | "https" => {}
        s => return Err(format!("scheme {s:?} is not http(s)")),
    }
    if !u.username().is_empty() || u.password().is_some() {
        return Err("URL carries credentials".into());
    }
    let host = u
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_lowercase();
    if host == "localhost" || host.ends_with(".localhost") || host.ends_with(".local") {
        return Err("local hostnames are not public web targets".into());
    }
    // host_str keeps [] around IPv6 literals — strip before IpAddr::parse
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = bare.parse::<IpAddr>() {
        let bad = match ip {
            IpAddr::V4(v4) => {
                v4.is_private()
                    || v4.is_loopback()
                    || v4.is_link_local()
                    || v4.is_multicast()
                    || v4.is_unspecified()
                    || v4.is_documentation()
                    || v4.octets()[0] == 100 && (v4.octets()[1] & 0xC0) == 64 // CGNAT
                    || v4.octets()[0] == 198 && (v4.octets()[1] & 0xFE) == 18 // bench
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()
                    || v6.is_multicast()
                    || v6.is_unspecified()
                    || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 unique-local
                    || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
            }
        };
        if bad {
            return Err("private/link-local/metadata IP targets are not allowed".into());
        }
    }
    if let Some(p) = secret_flag(url) {
        return Err(format!("URL looks like it contains {p}"));
    }
    Ok(())
}

/// High-confidence secret shapes — a mitigation, not a guarantee.
fn secret_flag(s: &str) -> Option<&'static str> {
    const PATTERNS: &[(&str, &str)] = &[
        ("sk-", "an API-key-looking token"),
        ("sk_", "an API-key-looking token"),
        ("ghp_", "a GitHub token"),
        ("github_pat_", "a GitHub token"),
        ("xoxb-", "a Slack token"),
        ("xoxp-", "a Slack token"),
        ("AKIA", "an AWS key id"),
        ("AIza", "a Google API key"),
        ("-----BEGIN", "private key material"),
        ("eyJhbGci", "a JWT"),
        ("api_key=", "an embedded credential"),
        ("apikey=", "an embedded credential"),
        ("access_token=", "an embedded credential"),
        ("Bearer ", "an embedded credential"),
    ];
    PATTERNS
        .iter()
        .find(|(p, _)| s.contains(p))
        .map(|(_, why)| *why)
}

fn rfc_now() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = s / 86400;
    let secs = s % 86400;
    let (y, m, d) = crate::codex::days_to_ymd(days);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(access: WebAccess) -> Arc<WebService> {
        WebService::new(WebCfg {
            access,
            api_key: None,
            endpoint: "https://mcp.test/mcp".into(),
        })
    }

    fn ok_parsed(text: &'static str) -> Parsed {
        Parsed {
            sources: vec![Source {
                id: String::new(),
                title: "t".into(),
                url: "https://ex.com".into(),
                excerpt: "e".into(),
                published: None,
                retrieved: "now".into(),
                kind: "search".into(),
                truncated: false,
            }],
            render: Box::new(move |srcs| {
                format!(
                    "{text} [{}]",
                    srcs.first().map(|s| s.id.as_str()).unwrap_or("?")
                )
            }),
        }
    }

    // ── URL safety ────────────────────────────────────────────────
    #[test]
    fn url_rejects_private_and_metadata_targets() {
        for u in [
            "http://127.0.0.1/x",
            "http://localhost/x",
            "http://foo.local/x",
            "http://10.0.0.1/x",
            "http://192.168.1.1/x",
            "http://172.16.0.1/x",
            "http://169.254.169.254/latest/meta-data",
            "http://[::1]/x",
            "http://[fd00::1]/x",
            "http://[fe80::1]/x",
            "http://0.0.0.0/x",
            "http://100.64.0.1/x", // CGNAT
        ] {
            assert!(check_url(u).is_err(), "should reject {u}");
        }
    }

    #[test]
    fn url_rejects_credentials_schemes_and_secrets() {
        for u in [
            "ftp://example.com/x",
            "file:///etc/passwd",
            "https://user:pw@example.com/x",
            "https://example.com/?api_key=abc123",
            "https://example.com/?access_token=tok",
            "https://example.com/eyJhbGciOiJ9.x.y",
            "not a url",
        ] {
            assert!(check_url(u).is_err(), "should reject {u}");
        }
    }

    #[test]
    fn url_accepts_public_targets() {
        assert!(check_url("https://docs.rs/serde").is_ok());
        assert!(check_url("http://8.8.8.8/dns").is_ok());
    }

    // ── secret shapes ─────────────────────────────────────────────
    #[test]
    fn secret_flag_catches_common_key_shapes() {
        assert!(secret_flag("how do I use sk-abc123").is_some());
        assert!(secret_flag("ghp_abcdefghij").is_some());
        assert!(secret_flag("AKIAIOSFODNN7EXAMPLE").is_some());
        assert!(secret_flag("-----BEGIN RSA PRIVATE KEY-----").is_some());
        assert!(secret_flag("token eyJhbGciOiJIUzI1NiJ9").is_some());
        assert!(secret_flag("ratatui paragraph scrolling").is_none());
        // "AIza" alone flags — acceptable false-positive on key-shaped strings
        assert!(secret_flag("tokio runtime").is_none());
    }

    // ── exec gates ────────────────────────────────────────────────
    #[tokio::test]
    async fn exec_off_short_circuits() {
        let out = svc(WebAccess::Off)
            .exec("web_search", &serde_json::json!({"query": "x"}))
            .await;
        assert!(matches!(out.kind, crate::tools::ExecKind::Error));
        assert!(out.text.contains("Off"));
    }

    #[tokio::test]
    async fn exec_unknown_tool_errors() {
        let out = svc(WebAccess::Auto)
            .exec("web_hack", &serde_json::json!({}))
            .await;
        assert!(matches!(out.kind, crate::tools::ExecKind::Error));
    }

    #[tokio::test]
    async fn search_rejects_secret_shaped_query() {
        let s = svc(WebAccess::Auto);
        let out = s
            .exec(
                "web_search",
                &serde_json::json!({"query": "my key is ghp_abcdefghij"}),
            )
            .await;
        assert!(matches!(out.kind, crate::tools::ExecKind::Error));
        assert!(out.text.contains("leave this machine"));
    }

    // ── cache ─────────────────────────────────────────────────────
    #[tokio::test]
    async fn cache_hit_labels_provenance() {
        let s = svc(WebAccess::Auto);
        let first = s
            .cached_call("k", |_| async { Ok(ok_parsed("BODY")) })
            .await;
        assert!(first.text.contains("BODY"));
        let second = s
            .cached_call("k", |_| async { Ok(ok_parsed("OTHER")) })
            .await;
        assert!(second.text.contains("BODY"));
        assert!(second.text.contains("cache: hit"));
        assert!(!second.text.contains("OTHER"));
    }

    #[tokio::test]
    async fn concurrent_same_key_calls_dedup() {
        let s = svc(WebAccess::Auto);
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let s2 = s.clone();
        let t1 = tokio::spawn(async move {
            s2.cached_call("k", move |_| {
                let c = c.clone();
                async move {
                    c.fetch_add(1, Ordering::Relaxed);
                    tokio::time::sleep(Duration::from_millis(60)).await;
                    Ok(ok_parsed("SHARED"))
                }
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(15)).await;
        // twin: if this runs the upstream fn the test must fail — it panics
        let t2 = tokio::spawn(async move {
            s.cached_call("k", |_| async {
                panic!("second upstream call must not run");
            })
            .await
        });
        let r1 = t1.await.unwrap();
        let r2 = t2.await.unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert!(r1.text.contains("SHARED"));
        assert!(r2.text.contains("SHARED"));
        assert!(r2.text.contains("cache: hit"));
    }

    #[tokio::test]
    async fn failures_are_not_cached() {
        let s = svc(WebAccess::Auto);
        let n = Arc::new(AtomicUsize::new(0));
        for _ in 0..2 {
            let n = n.clone();
            let out = s
                .cached_call("k", move |_| {
                    let n = n.clone();
                    async move {
                        n.fetch_add(1, Ordering::Relaxed);
                        Err(anyhow::anyhow!("upstream down"))
                    }
                })
                .await;
            assert!(matches!(out.kind, crate::tools::ExecKind::Error));
        }
        assert_eq!(n.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cache_assigns_global_source_ids() {
        let s = svc(WebAccess::Auto);
        let _ = s.cached_call("a", |_| async { Ok(ok_parsed("A")) }).await;
        let _ = s.cached_call("b", |_| async { Ok(ok_parsed("B")) }).await;
        let srcs = s.sources();
        assert_eq!(srcs.len(), 2);
        assert_eq!(srcs[0].id, "S1");
        assert_eq!(srcs[1].id, "S2");
    }

    // ── parsing ───────────────────────────────────────────────────
    #[test]
    fn parse_search_extracts_blocks() {
        let text = "Title: Ratatui docs\nURL: https://docs.rs/ratatui\nPublished: 2024-01-01\nHighlights:\nparagraph widget\n\nTitle: Second\nURL: https://ex.com\nPublished: N/A\nHighlights:\nother\n";
        let p = parse_search(text, 5);
        assert_eq!(p.sources.len(), 2);
        assert_eq!(p.sources[0].title, "Ratatui docs");
        assert_eq!(p.sources[0].url, "https://docs.rs/ratatui");
        assert_eq!(p.sources[0].published.as_deref(), Some("2024-01-01"));
        assert!(p.sources[0].excerpt.contains("paragraph widget"));
        assert!(p.sources[1].published.is_none());
        let mut srcs = p.sources;
        for (i, s) in srcs.iter_mut().enumerate() {
            s.id = format!("S{}", i + 1);
        }
        let out = (p.render)(&srcs);
        assert!(out.contains("[S1]"));
        assert!(out.contains("snippets ≠ fetched pages"));
    }

    #[test]
    fn parse_search_empty_result_is_honest() {
        let p = parse_search("", 5);
        assert!(p.sources.is_empty());
        let out = (p.render)(&[]);
        assert!(out.contains("sources: 0"));
    }

    #[test]
    fn parse_fetch_captures_title_and_truncation() {
        let text = format!(
            "# Big Page\nURL: https://ex.com\n\n{}",
            "x".repeat(FETCH_TEXT_CAP + 10)
        );
        let p = parse_fetch(&text, "https://ex.com");
        assert_eq!(p.sources.len(), 1);
        assert_eq!(p.sources[0].title, "Big Page");
        assert!(p.sources[0].truncated);
        let out = (p.render)(&p.sources);
        assert!(out.contains("truncated: yes"));
        assert!(out.contains("source: https://ex.com"));
    }

    #[tokio::test]
    async fn dead_latch_blocks_reconnect() {
        let s = svc(WebAccess::Auto);
        s.st.lock().unwrap().dead = true;
        let r = s.client().await;
        assert!(format!("{:#}", r.unwrap_err()).contains("down for this run"));
    }

    #[tokio::test]
    async fn request_budget_is_per_run_not_per_call_site() {
        let s = svc(WebAccess::Auto);
        // burn the budget through the public counter path
        for _ in 0..MAX_REQUESTS {
            s.reqs.fetch_add(1, Ordering::Relaxed);
        }
        let r = s
            .upstream_call("web_search_exa", serde_json::json!({}))
            .await;
        assert!(r.is_err());
        assert!(format!("{:#}", r.unwrap_err()).contains("limit"));
    }
}
