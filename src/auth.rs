//! Native account authentication. Protocol details adapted from jcode;
//! see THIRD_PARTY_NOTICES.md. Tokens never enter profiles or model history.
pub mod login;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use serde_json::json;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoginProvider {
    Codex,
    Gemini,
    Copilot,
}
impl LoginProvider {
    pub fn parse(name: &str) -> Result<Self> {
        match name {
            "codex" | "openai" | "chatgpt" => Ok(Self::Codex),
            "gemini" => Ok(Self::Gemini),
            "copilot" => Ok(Self::Copilot),
            _ => bail!("unknown sign-in provider; choose codex, gemini, or copilot; Claude uses API key setup"),
        }
    }
    pub fn id(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Gemini => "gemini",
            Self::Copilot => "copilot",
        }
    }
    pub fn kind(self) -> &'static str {
        match self {
            Self::Codex => "codex-oauth",
            Self::Gemini => "gemini-oauth",
            Self::Copilot => "copilot",
        }
    }
    pub fn transport(self) -> crate::config::Transport {
        use crate::config::Transport;
        match self {
            Self::Codex => Transport::CodexOauth,
            Self::Gemini => Transport::GeminiOauth,
            Self::Copilot => Transport::Copilot,
        }
    }
    pub fn session_exists(self) -> bool {
        if self == Self::Codex {
            crate::codex::CodexAuth::session_exists()
        } else {
            store_path(self).is_ok_and(|p| p.is_file())
        }
    }
    fn lock_index(self) -> usize {
        match self {
            Self::Gemini => 0,
            Self::Copilot => 1,
            Self::Codex => unreachable!("Codex owns its refresh chain"),
        }
    }
}
pub(crate) const GEMINI_CLIENT_ID: &str =
    "681255809395-oo8ft2oprdrnp9e3aqf6av3hmdib135j.apps.googleusercontent.com";
// Public installed-application OAuth credentials, from Google's Gemini CLI.
// This value identifies the desktop client; it is not a user's credential.
pub(crate) const GEMINI_CLIENT_SECRET: &str = "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";
pub(crate) const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
pub(crate) const COPILOT_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";
pub(crate) const COPILOT_TOKEN_URL: &str = "https://api.github.com/copilot_internal/v2/token";
pub(crate) const GEMINI_BASE: &str = "https://cloudcode-pa.googleapis.com/v1internal";
pub(crate) const COPILOT_BASE: &str = "https://api.githubcopilot.com";

// No Debug implementation: credential-bearing values must not be printable.
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Credentials {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
    #[serde(default)]
    pub project: Option<String>,
    #[serde(default)]
    pub api_base: Option<String>,
}
pub(crate) struct AccountToken {
    pub access: String,
    pub base: String,
    pub project: Option<String>,
}
static REFRESH_LOCKS: LazyLock<[tokio::sync::Mutex<()>; 2]> =
    LazyLock::new(|| std::array::from_fn(|_| tokio::sync::Mutex::new(())));

/// The file lock coordinates separate Sui processes; the mutex coordinates
/// workers inside one process. Closing the file releases the OS lock.
struct StoreLock {
    _file: std::fs::File,
}
impl StoreLock {
    async fn acquire(provider: LoginProvider) -> Result<Self> {
        let path = store_path(provider)?.with_extension("lock");
        std::fs::create_dir_all(path.parent().context("missing token directory")?)?;
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let deadline = std::time::Instant::now() + Duration::from_secs(30);
            loop {
                // The descriptor remains owned by file for the lock lifetime.
                if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
                    break;
                }
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::WouldBlock {
                    return Err(error.into());
                }
                if std::time::Instant::now() >= deadline {
                    bail!("another Sui process is refreshing this account; retry");
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
        Ok(Self { _file: file })
    }
}

pub(crate) fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
pub(crate) fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .user_agent(concat!("sui/", env!("CARGO_PKG_VERSION")))
        .build()?)
}
pub(crate) fn store_path(provider: LoginProvider) -> Result<PathBuf> {
    let root = std::env::var_os("SUI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|h| h.join(".config/sui")))
        .context("no home directory for sign-in")?;
    Ok(root.join(format!("{}-auth.json", provider.id())))
}
fn read_credentials(provider: LoginProvider) -> Result<Credentials> {
    use std::io::Read;
    let path = store_path(provider)?;
    let file = std::fs::File::open(path).with_context(|| {
        format!(
            "no {} session; run sui auth {}",
            provider.id(),
            provider.id()
        )
    })?;
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        bail!("sign-in store exceeds 64 KiB");
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid sign-in store; sign in again"))
}
pub(crate) fn save_credentials(
    provider: LoginProvider,
    credentials: &Credentials,
) -> Result<PathBuf> {
    let path = store_path(provider)?;
    let bytes = serde_json::to_vec(credentials)?;
    if bytes.len() > 65_536 {
        bail!("sign-in store exceeds 64 KiB");
    }
    write_secret(&path, &bytes)?;
    Ok(path)
}
pub(crate) fn write_secret(path: &std::path::Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path.parent().context("missing token directory")?;
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".sui-auth-{:032x}.tmp", rand::random::<u128>()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result?;
    Ok(())
}
pub(crate) async fn json_response(response: reqwest::Response, label: &str) -> Result<Value> {
    if !response.status().is_success() {
        return Err(anyhow::Error::from(
            crate::provider::failure::Failure::response(response).await,
        ))
        .context(label.to_string());
    }
    bounded_json(response, label).await
}

pub(crate) async fn save_login(
    provider: LoginProvider,
    credentials: &Credentials,
) -> Result<PathBuf> {
    let _guard = REFRESH_LOCKS[provider.lock_index()].lock().await;
    let _file = StoreLock::acquire(provider).await?;
    save_credentials(provider, credentials)
}
pub(crate) async fn bounded_json(mut response: reqwest::Response, label: &str) -> Result<Value> {
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(crate::provider::failure::Failure::transport)?
    {
        if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
            bail!("{label}: response exceeds 1 MiB");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("{label}: invalid JSON response"))
}
pub(crate) fn credentials_from_json(
    v: &Value,
    previous_refresh: Option<&str>,
) -> Result<Credentials> {
    let access = v["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("missing access token")?;
    let refresh = v["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or(previous_refresh)
        .context("missing refresh token; sign in again")?;
    let expires = v["expires_in"]
        .as_u64()
        .filter(|n| *n > 0)
        .context("missing token expiry")?;
    Ok(Credentials {
        access_token: access.into(),
        refresh_token: refresh.into(),
        expires_at: now().saturating_add(expires),
        project: None,
        api_base: None,
    })
}
pub(crate) fn copilot_headers(request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    request
        .header("User-Agent", concat!("sui/", env!("CARGO_PKG_VERSION")))
        .header("Editor-Version", concat!("sui/", env!("CARGO_PKG_VERSION")))
        .header(
            "Editor-Plugin-Version",
            concat!("sui/", env!("CARGO_PKG_VERSION")),
        )
        .header("Copilot-Integration-Id", "vscode-chat")
}
fn copilot_base(base: Option<&str>) -> Result<String> {
    let base = base.unwrap_or(COPILOT_BASE);
    let url = reqwest::Url::parse(base).context("invalid Copilot API endpoint")?;
    let allowed = url
        .host_str()
        .is_some_and(|h| h == "api.githubcopilot.com" || h.ends_with(".githubcopilot.com"));
    if url.scheme() != "https"
        || !allowed
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("refusing untrusted Copilot API endpoint");
    }
    Ok(base.trim_end_matches('/').into())
}
pub(crate) async fn account_token(
    provider: LoginProvider,
    http: &reqwest::Client,
) -> Result<AccountToken> {
    let endpoint = match provider {
        LoginProvider::Gemini => GOOGLE_TOKEN_URL,
        LoginProvider::Copilot => COPILOT_TOKEN_URL,
        LoginProvider::Codex => unreachable!(),
    };
    account_token_at(provider, http, endpoint).await
}
async fn account_token_at(
    provider: LoginProvider,
    http: &reqwest::Client,
    endpoint: &str,
) -> Result<AccountToken> {
    let _lock = REFRESH_LOCKS[provider.lock_index()].lock().await;
    let _file = StoreLock::acquire(provider).await?;
    let mut stored = read_credentials(provider)?;
    if stored.expires_at <= now().saturating_add(60) {
        let request = match provider {
            LoginProvider::Gemini => http.post(endpoint).form(&[
                ("grant_type", "refresh_token"),
                ("client_id", GEMINI_CLIENT_ID),
                ("client_secret", GEMINI_CLIENT_SECRET),
                ("refresh_token", &stored.refresh_token),
            ]),
            LoginProvider::Copilot => copilot_headers(
                http.get(endpoint)
                    .header("Authorization", format!("Token {}", stored.refresh_token)),
            ),
            LoginProvider::Codex => unreachable!(),
        };
        let body = json_response(
            request
                .send()
                .await
                .map_err(crate::provider::failure::Failure::transport)?,
            "account token refresh",
        )
        .await?;
        let mut refreshed = if provider == LoginProvider::Copilot {
            Credentials {
                access_token: body["token"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .context("missing Copilot token")?
                    .into(),
                refresh_token: stored.refresh_token.clone(),
                expires_at: body["expires_at"]
                    .as_u64()
                    .filter(|n| *n > now())
                    .context("invalid Copilot expiry")?,
                project: None,
                api_base: Some(copilot_base(body["endpoints"]["api"].as_str())?),
            }
        } else {
            credentials_from_json(&body, Some(&stored.refresh_token))?
        };
        refreshed.project = stored.project.take();
        save_credentials(provider, &refreshed)?;
        stored = refreshed;
    }
    let base = match provider {
        LoginProvider::Gemini => GEMINI_BASE.into(),
        LoginProvider::Copilot => copilot_base(stored.api_base.as_deref())?,
        LoginProvider::Codex => unreachable!(),
    };
    Ok(AccountToken {
        access: stored.access_token,
        base,
        project: stored.project,
    })
}
pub(crate) async fn save_gemini_project(project: &str) -> Result<()> {
    let _lock = REFRESH_LOCKS[LoginProvider::Gemini.lock_index()]
        .lock()
        .await;
    let _file = StoreLock::acquire(LoginProvider::Gemini).await?;
    let mut stored = read_credentials(LoginProvider::Gemini)?;
    stored.project = Some(project.into());
    save_credentials(LoginProvider::Gemini, &stored)?;
    Ok(())
}
pub(crate) fn known_secrets() -> Vec<String> {
    [LoginProvider::Gemini, LoginProvider::Copilot]
        .into_iter()
        .filter_map(|p| read_credentials(p).ok())
        .flat_map(|c| [c.access_token, c.refresh_token])
        .filter(|s| !s.is_empty())
        .collect()
}

/// API keys are entered without echoing and without command-line arguments.
pub fn read_api_key() -> Result<String> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
    use std::io::{IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        bail!("set ANTHROPIC_API_KEY or pass --key-env for non-interactive setup");
    }
    print!("Anthropic API key (hidden; saved to private Sui config)> ");
    std::io::stdout().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    struct Raw;
    impl Drop for Raw {
        fn drop(&mut self) {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    let raw = Raw;
    let mut key = String::new();
    loop {
        if let Event::Key(event) = crossterm::event::read()? {
            if event.kind == KeyEventKind::Release {
                continue;
            }
            match event.code {
                KeyCode::Enter => break,
                KeyCode::Esc => bail!("API key setup canceled"),
                KeyCode::Char('c' | 'd') if event.modifiers.contains(KeyModifiers::CONTROL) => {
                    bail!("API key setup canceled")
                }
                KeyCode::Backspace => {
                    key.pop();
                }
                KeyCode::Char(c) if !event.modifiers.contains(KeyModifiers::CONTROL) => {
                    if key.len().saturating_add(c.len_utf8()) > 4096 {
                        bail!("API key exceeds 4096 bytes");
                    }
                    key.push(c);
                }
                _ => {}
            }
        }
    }
    drop(raw);
    println!();
    if key.trim().is_empty() {
        bail!("API key required");
    }
    Ok(key.trim().into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn separate_store_handles_wait_for_the_os_refresh_lock() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let _home = crate::test_http::AuthHome::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let first = StoreLock::acquire(LoginProvider::Gemini).await.unwrap();
            assert!(tokio::time::timeout(
                Duration::from_millis(100),
                StoreLock::acquire(LoginProvider::Gemini)
            )
            .await
            .is_err());
            drop(first);
            assert!(tokio::time::timeout(
                Duration::from_secs(1),
                StoreLock::acquire(LoginProvider::Gemini)
            )
            .await
            .unwrap()
            .is_ok());
        });
    }
    #[test]
    fn concurrent_workers_refresh_once_and_write_a_private_rotated_store() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let _home = crate::test_http::AuthHome::new();
        let stored = Credentials {
            access_token: "expired-access".into(),
            refresh_token: "initial-refresh".into(),
            expires_at: 0,
            project: Some("project-1".into()),
            api_base: None,
        };
        let path = save_credentials(LoginProvider::Gemini, &stored).unwrap();
        let (endpoint,captured,task) = crate::test_http::server(vec![(200,"application/json",
            json!({"access_token":"rotated-access","refresh_token":"rotated-refresh","expires_in":3600}).to_string())]);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let client = client().unwrap();
            let requests =
                (0..8).map(|_| account_token_at(LoginProvider::Gemini, &client, &endpoint));
            for result in futures_util::future::join_all(requests).await {
                assert_eq!(result.unwrap().access, "rotated-access");
            }
        });
        task.join().unwrap();
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].1.contains("grant_type=refresh_token"));
        assert!(requests[0].1.contains("refresh_token=initial-refresh"));
        assert_eq!(
            read_credentials(LoginProvider::Gemini)
                .unwrap()
                .refresh_token,
            "rotated-refresh"
        );
        assert_eq!(
            read_credentials(LoginProvider::Gemini)
                .unwrap()
                .project
                .as_deref(),
            Some("project-1")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
    #[tokio::test]
    async fn authentication_errors_never_echo_provider_response_secrets() {
        let (endpoint, _, task) = crate::test_http::server(vec![(
            400,
            "application/json",
            json!({"error":"invalid_grant","message":"secret-token-that-must-not-be-shown"})
                .to_string(),
        )]);
        let response = client().unwrap().get(endpoint).send().await.unwrap();
        let error = json_response(response, "token exchange").await.unwrap_err();
        assert!(error
            .downcast_ref::<crate::provider::failure::Failure>()
            .is_some());
        let report = format!("{error:#}");
        assert!(report.contains("token exchange") && report.contains("http 400"));
        assert!(!report.contains("secret-token"));
        task.join().unwrap();
    }
    #[test]
    fn rotating_refresh_tokens_and_expiry_are_preserved() {
        let fresh = credentials_from_json(
            &json!({"access_token":"new","expires_in":3600}),
            Some("old-refresh"),
        )
        .unwrap();
        assert_eq!(fresh.refresh_token, "old-refresh");
        assert!(fresh.expires_at > now());
        assert!(
            credentials_from_json(&json!({"access_token":"a","refresh_token":"r"}), None).is_err()
        );
        assert!(copilot_base(Some("https://attacker.example")).is_err());
        assert!(copilot_base(Some("https://api.githubcopilot.com@attacker.example")).is_err());
        assert!(copilot_base(Some("https://api.individual.githubcopilot.com")).is_ok());
    }
}
