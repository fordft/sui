//! PKCE browser flows and GitHub's device authorization flow.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::Instant;

const CODEX_CLIENT: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

#[derive(Clone, Debug)]
pub struct Prompt {
    pub url: String,
    pub user_code: Option<String>,
    pub input_required: bool,
}
pub struct Session {
    pub prompt: Prompt,
    provider: LoginProvider,
    verifier: String,
    state: String,
    redirect: String,
    listener: Option<TcpListener>,
    device_code: Option<String>,
    interval: u64,
    expires: u64,
    token_endpoint: String,
    device_endpoint: String,
}
fn random_value() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
fn authorization_url(base: &str, params: &[(&str, &str)]) -> Result<String> {
    let mut url = reqwest::Url::parse(base)?;
    url.query_pairs_mut().extend_pairs(params.iter().copied());
    Ok(url.to_string())
}
impl Session {
    pub async fn begin(provider: LoginProvider, manual: bool) -> Result<Self> {
        let verifier = random_value();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_value();
        let mut listener = None;
        let mut redirect = String::new();
        let mut user_code = None;
        let mut device_code = None;
        let mut interval = 5;
        let mut expires = 600;
        let input_required = manual;
        let url = match provider {
            LoginProvider::Codex => {
                redirect = "http://localhost:1455/auth/callback".into();
                if !manual {
                    listener = Some(
                        TcpListener::bind("127.0.0.1:1455")
                            .await
                            .context("callback port is busy; use sui auth codex --manual")?,
                    );
                }
                authorization_url("https://auth.openai.com/oauth/authorize", &[
                    ("response_type","code"), ("client_id",CODEX_CLIENT), ("redirect_uri",&redirect),
                    ("scope","openid profile email offline_access api.connectors.read api.connectors.invoke"),
                    ("code_challenge",&challenge), ("code_challenge_method","S256"), ("state",&state),
                    ("id_token_add_organizations","true"), ("codex_cli_simplified_flow","true"), ("originator","sui"),
                ])?
            }
            LoginProvider::Gemini => {
                if manual {
                    redirect = "https://codeassist.google.com/authcode".into();
                } else {
                    let socket = TcpListener::bind("127.0.0.1:0").await?;
                    redirect = format!(
                        "http://127.0.0.1:{}/oauth2callback",
                        socket.local_addr()?.port()
                    );
                    listener = Some(socket);
                }
                authorization_url("https://accounts.google.com/o/oauth2/v2/auth", &[
                    ("response_type","code"), ("client_id",GEMINI_CLIENT_ID), ("redirect_uri",&redirect),
                    ("scope","https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile"),
                    ("code_challenge",&challenge), ("code_challenge_method","S256"), ("state",&state),
                    ("access_type","offline"), ("prompt","consent"),
                ])?
            }
            LoginProvider::Copilot => {
                let response = client()?
                    .post("https://github.com/login/device/code")
                    .header("Accept", "application/json")
                    .form(&[("client_id", COPILOT_CLIENT_ID), ("scope", "read:user")])
                    .send()
                    .await
                    .map_err(crate::provider::failure::Failure::transport)?;
                let body = json_response(response, "GitHub device authorization").await?;
                device_code = Some(
                    body["device_code"]
                        .as_str()
                        .context("missing device code")?
                        .into(),
                );
                user_code = Some(
                    body["user_code"]
                        .as_str()
                        .context("missing user code")?
                        .into(),
                );
                interval = body["interval"].as_u64().unwrap_or(5).clamp(1, 30);
                expires = body["expires_in"].as_u64().unwrap_or(600).min(900);
                let verification = body["verification_uri"]
                    .as_str()
                    .context("missing verification URL")?;
                let url = reqwest::Url::parse(verification)?;
                if url.scheme() != "https"
                    || url.host_str() != Some("github.com")
                    || url.path() != "/login/device"
                {
                    bail!("untrusted GitHub verification URL");
                }
                verification.into()
            }
        };
        Ok(Self {
            prompt: Prompt {
                url,
                user_code,
                input_required: input_required && provider != LoginProvider::Copilot,
            },
            provider,
            verifier,
            state,
            redirect,
            listener,
            device_code,
            interval,
            expires,
            token_endpoint: match provider {
                LoginProvider::Gemini => GOOGLE_TOKEN_URL,
                LoginProvider::Copilot => COPILOT_TOKEN_URL,
                LoginProvider::Codex => "https://auth.openai.com/oauth/token",
            }
            .into(),
            device_endpoint: "https://github.com/login/oauth/access_token".into(),
        })
    }

    pub async fn finish(self, input: Option<String>) -> Result<PathBuf> {
        let provider = self.provider;
        tokio::time::timeout(Duration::from_secs(self.expires), self.finish_inner(input))
            .await
            .context("sign-in timed out; start sign-in again")?
            .with_context(|| format!("{} sign-in failed", provider.id()))
    }
    async fn finish_inner(self, input: Option<String>) -> Result<PathBuf> {
        let http = client()?;
        if self.provider == LoginProvider::Copilot {
            return self.finish_device(&http).await;
        }
        let code = if let Some(listener) = &self.listener {
            callback(
                listener,
                &self.state,
                reqwest::Url::parse(&self.redirect)?.path(),
            )
            .await?
        } else {
            parse_input(
                input
                    .as_deref()
                    .context("paste the authorization code or callback URL")?,
                &self.state,
                self.provider == LoginProvider::Gemini,
            )?
        };
        if self.provider == LoginProvider::Codex {
            return crate::codex::complete_login(&http, &code, &self.verifier).await;
        }
        let request = http.post(&self.token_endpoint).form(&[
            ("grant_type", "authorization_code"),
            ("client_id", GEMINI_CLIENT_ID),
            ("client_secret", GEMINI_CLIENT_SECRET),
            ("code", &code),
            ("redirect_uri", &self.redirect),
            ("code_verifier", &self.verifier),
        ]);
        let body = json_response(
            request
                .send()
                .await
                .map_err(crate::provider::failure::Failure::transport)?,
            "OAuth code exchange",
        )
        .await?;
        let credentials = credentials_from_json(&body, None)?;
        save_login(self.provider, &credentials).await
    }
    async fn finish_device(&self, http: &reqwest::Client) -> Result<PathBuf> {
        let deadline = Instant::now() + Duration::from_secs(self.expires);
        let mut interval = self.interval;
        loop {
            if Instant::now() >= deadline {
                bail!("GitHub device code expired");
            }
            tokio::time::sleep(Duration::from_secs(interval)).await;
            let response = http
                .post(&self.device_endpoint)
                .header("Accept", "application/json")
                .form(&[
                    ("client_id", COPILOT_CLIENT_ID),
                    (
                        "device_code",
                        self.device_code.as_deref().context("missing device code")?,
                    ),
                    ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ])
                .send()
                .await
                .map_err(crate::provider::failure::Failure::transport)?;
            let body = json_response(response, "GitHub device token").await?;
            match body["error"].as_str() {
                Some("authorization_pending") => continue,
                Some("slow_down") => {
                    interval = interval.saturating_add(5).min(60);
                    continue;
                }
                Some("access_denied") => bail!("GitHub authorization denied"),
                Some("expired_token") => bail!("GitHub device code expired"),
                Some(_) => bail!("GitHub device authorization failed"),
                None => {
                    let token = body["access_token"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .context("missing GitHub token")?;
                    let credentials = Credentials {
                        access_token: String::new(),
                        refresh_token: token.into(),
                        expires_at: 0,
                        project: None,
                        api_base: None,
                    };
                    // Exchange before persisting: GitHub login alone does not prove
                    // this account has a Copilot subscription.
                    let response = copilot_headers(
                        http.get(&self.token_endpoint)
                            .header("Authorization", format!("Token {token}")),
                    )
                    .send()
                    .await
                    .map_err(crate::provider::failure::Failure::transport)?;
                    let api = json_response(response, "Copilot subscription authorization").await?;
                    let credentials = Credentials {
                        access_token: api["token"]
                            .as_str()
                            .filter(|s| !s.is_empty())
                            .context("missing Copilot token")?
                            .into(),
                        expires_at: api["expires_at"]
                            .as_u64()
                            .filter(|n| *n > now())
                            .context("invalid Copilot expiry")?,
                        api_base: Some(copilot_base(api["endpoints"]["api"].as_str())?),
                        ..credentials
                    };
                    return save_login(LoginProvider::Copilot, &credentials).await;
                }
            }
        }
    }
}

pub(crate) fn parse_input(input: &str, expected: &str, allow_code: bool) -> Result<String> {
    let trimmed = input.trim();
    if trimmed.len() > 16_384 || trimmed.is_empty() {
        bail!("invalid authorization input");
    }
    if let Ok(url) = reqwest::Url::parse(trimmed) {
        let mut code = None;
        let mut state = None;
        for (key, value) in url.query_pairs() {
            match key.as_ref() {
                "code" if code.is_none() => code = Some(value.into_owned()),
                "state" if state.is_none() => state = Some(value.into_owned()),
                "code" | "state" => bail!("duplicate authorization parameters"),
                _ => {}
            }
        }
        if state.as_deref() != Some(expected) {
            bail!("OAuth state mismatch; use the latest callback URL");
        }
        return code
            .filter(|s| !s.is_empty())
            .context("callback is missing authorization code");
    }
    if allow_code {
        let (code, state) = trimmed
            .split_once('#')
            .map_or((trimmed, None), |(c, s)| (c, Some(s)));
        if state.is_some_and(|s| s != expected) {
            bail!("OAuth state mismatch");
        }
        if !code.is_empty() && !code.chars().any(char::is_whitespace) {
            return Ok(code.into());
        }
    }
    bail!("paste the full callback URL including code and state")
}
async fn callback(listener: &TcpListener, state: &str, expected_path: &str) -> Result<String> {
    loop {
        let (mut socket, _) = listener.accept().await?;
        let mut bytes = Vec::new();
        let read = tokio::time::timeout(Duration::from_secs(5), async {
            while bytes.len() < 16_384 {
                let mut byte = [0u8; 1];
                if socket.read(&mut byte).await? == 0 {
                    break;
                }
                bytes.push(byte[0]);
                if bytes.ends_with(b"\r\n\r\n") {
                    break;
                }
            }
            Ok::<(), std::io::Error>(())
        })
        .await;
        if !matches!(read, Ok(Ok(()))) {
            continue;
        }
        let request = String::from_utf8_lossy(&bytes);
        let line = request.lines().next().unwrap_or("");
        let mut parts = line.split_whitespace();
        let method = parts.next();
        let path = parts.next().unwrap_or("");
        let url = reqwest::Url::parse(&format!("http://localhost{path}"));
        let code = url
            .ok()
            .filter(|u| method == Some("GET") && u.path() == expected_path)
            .and_then(|u| parse_input(u.as_str(), state, false).ok());
        let (status, body) = if code.is_some() {
            ("200 OK", "Sui sign-in received. Return to Sui to finish.")
        } else {
            ("400 Bad Request", "Invalid sign-in callback.")
        };
        let reply = format!("HTTP/1.1 {status}\r\ncontent-type: text/plain\r\nconnection: close\r\ncontent-length: {}\r\n\r\n{body}",body.len());
        let _ =
            tokio::time::timeout(Duration::from_secs(5), socket.write_all(reply.as_bytes())).await;
        if let Some(code) = code {
            return Ok(code);
        }
    }
}
pub fn open_browser(url: &str) {
    let (program, args): (&str, Vec<&str>) = if cfg!(target_os = "macos") {
        ("open", vec![url])
    } else if cfg!(target_os = "windows") {
        ("rundll32", vec!["url.dll,FileProtocolHandler", url])
    } else {
        ("xdg-open", vec![url])
    };
    let mut command = std::process::Command::new(program);
    command.args(args).env_clear();
    for name in [
        "PATH",
        "HOME",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "DBUS_SESSION_BUS_ADDRESS",
        "XDG_RUNTIME_DIR",
    ] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Ok(mut child) = command.spawn() {
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
}
pub async fn cli(provider: LoginProvider, manual: bool) -> Result<PathBuf> {
    use std::io::Write;
    let session = Session::begin(provider, manual).await?;
    println!("Open this URL to sign in:\n\n{}\n", session.prompt.url);
    if let Some(code) = &session.prompt.user_code {
        println!("Enter code: {code}\n");
    }
    if !manual {
        open_browser(&session.prompt.url);
    }
    let input = if session.prompt.input_required {
        print!("Authorization code or callback URL> ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().read_line(&mut line)?;
        Some(line)
    } else {
        None
    };
    session.finish(input).await
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gemini_exchanges_pkce_codes_into_a_private_store() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let _home = crate::test_http::AuthHome::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            for provider in [LoginProvider::Gemini] {
                let mut session = Session::begin(provider,true).await.unwrap();
                let url = reqwest::Url::parse(&session.prompt.url).unwrap();
                assert!(url.query_pairs().any(|(k,v)| k == "code_challenge" && !v.is_empty()));
                let (endpoint,captured,task) = crate::test_http::server(vec![(200,"application/json",
                    json!({"access_token":"fake-access","refresh_token":"fake-refresh","expires_in":3600}).to_string())]);
                session.token_endpoint = endpoint;
                let path = session.finish(Some("fake-code".into())).await.unwrap();
                assert_eq!(path,store_path(provider).unwrap());
                assert!(provider.session_exists());
                task.join().unwrap();
                let requests = captured.lock().unwrap();
                assert!(requests[0].1.contains("code_verifier="));
                assert!(requests[0].1.contains("grant_type=authorization_code"));
            }
            let profiles = crate::config::profiles(None).unwrap();
            assert!(profiles.contains_key("gemini"));
        });
    }
    #[test]
    fn github_device_login_waits_for_authorization_and_validates_copilot_access() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let _home = crate::test_http::AuthHome::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut session = Session::begin(LoginProvider::Gemini, true).await.unwrap();
            session.provider = LoginProvider::Copilot;
            session.device_code = Some("fake-device-code".into());
            session.interval = 0;
            let (endpoint, captured, task) = crate::test_http::server(vec![
                (
                    200,
                    "application/json",
                    json!({"error":"authorization_pending"}).to_string(),
                ),
                (
                    200,
                    "application/json",
                    json!({"access_token":"github-test-token"}).to_string(),
                ),
                (
                    200,
                    "application/json",
                    json!({"token":"copilot-test-token","expires_at":now()+3600}).to_string(),
                ),
            ]);
            session.device_endpoint = endpoint.clone();
            session.token_endpoint = endpoint;
            session.finish(None).await.unwrap();
            task.join().unwrap();
            let requests = captured.lock().unwrap();
            assert_eq!(requests.len(), 3);
            assert!(requests[0].1.contains("device_code=fake-device-code"));
            assert!(requests[2].0.contains("Token github-test-token"));
            assert_eq!(
                read_credentials(LoginProvider::Copilot)
                    .unwrap()
                    .access_token,
                "copilot-test-token"
            );
        });
    }
    #[test]
    fn callback_state_is_required_and_duplicate_parameters_are_rejected() {
        assert_eq!(
            parse_input(
                "http://localhost:1455/auth/callback?code=c%2Bd&state=s",
                "s",
                false
            )
            .unwrap(),
            "c+d"
        );
        for input in [
            "http://localhost/?code=c&state=wrong",
            "http://localhost/?code=c",
            "http://localhost/?code=c&state=s&state=wrong",
            "c",
        ] {
            assert!(parse_input(input, "s", false).is_err());
        }
        assert_eq!(parse_input("code#state", "state", true).unwrap(), "code");
        assert!(parse_input("code#wrong", "state", true).is_err());
    }
    #[tokio::test]
    async fn callback_skips_invalid_requests_then_accepts_matching_state() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let handle =
            tokio::spawn(async move { callback(&listener, "expected", "/oauth2callback").await });
        let client = client().unwrap();
        let response = client
            .get(format!(
                "http://{address}/oauth2callback?code=x&state=wrong"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        let response = client
            .get(format!(
                "http://{address}/oauth2callback?code=ok&state=expected"
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(handle.await.unwrap().unwrap(), "ok");
    }
}
