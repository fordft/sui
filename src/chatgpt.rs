//! Public Sign in with ChatGPT: dynamic registration, verified OIDC identity,
//! protected per-profile credentials, and serialized rotating refreshes.
//! Independent of the legacy Codex CLI session and its private API endpoint.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use jsonwebtoken::{decode, decode_header, jwk::JwkSet, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const ISSUER: &str = "https://auth.openai.com";
const AUTHORIZE: &str = "https://auth.openai.com/api/accounts/authorize";
const TOKEN: &str = "https://auth.openai.com/api/accounts/oauth/token";
const RESOURCE: &str = "https://api.openai.com/v1";
const SCOPE: &str = "openid profile email offline_access resource.invoke chatgpt.tokens.use.direct";
const PLAN_SCOPE: &str = "chatgpt.tokens.use.direct";
const DYNAMIC: &str = "dynamic_agent_client";

// Deliberately no Debug: this record must never be formatted into diagnostics.
#[derive(Default, Serialize, Deserialize)]
pub(crate) struct Account {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    subject: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default)]
    expires_at: u64,
    #[serde(default)]
    earliest_refresh_at: u64,
}
impl Account {
    fn sharing(&self) -> bool {
        !self.access_token.is_empty() && self.scopes.iter().any(|s| s == PLAN_SCOPE)
    }
    pub(crate) fn summary(&self) -> Value {
        json!({"connected":!self.access_token.is_empty(),"sharing":self.sharing(),"email":self.email})
    }
    fn clear_tokens(&mut self) {
        self.access_token.clear();
        self.refresh_token.clear();
        self.id_token.clear();
        self.scopes.clear();
        self.expires_at = 0;
        self.earliest_refresh_at = 0;
    }
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        bail!("profile name must use 1..64 letters, digits, hyphens or underscores");
    }
    Ok(())
}
fn root() -> Result<PathBuf> {
    Ok(std::env::var_os("SUI_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|p| p.join(".config/sui")))
        .context("no home directory for ChatGPT sign-in")?
        .join("chatgpt"))
}
fn path(name: &str) -> Result<PathBuf> {
    validate_name(name)?;
    Ok(root()?.join(format!("{name}.json")))
}
fn read_json(path: &std::path::Path) -> Result<Value> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    }
    let file = options
        .open(path)
        .context("cannot open ChatGPT account store")?;
    if !file.metadata()?.is_file() {
        bail!("ChatGPT account store must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take(65_537).read_to_end(&mut bytes)?;
    if bytes.len() > 65_536 {
        bail!("ChatGPT account store exceeds 64 KiB");
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT account store; sign in again"))
}
fn load(name: &str) -> Result<Account> {
    serde_json::from_value(read_json(&path(name)?)?)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT account store; sign in again"))
}
fn save(name: &str, account: &Account) -> Result<()> {
    let bytes = serde_json::to_vec(account)?;
    if bytes.len() > 65_536 {
        bail!("ChatGPT account store exceeds 64 KiB");
    }
    crate::auth::write_secret(&path(name)?, &bytes)
}
async fn lock(name: &str) -> Result<crate::auth::StoreLock> {
    crate::auth::StoreLock::acquire_path(&path(name)?.with_extension("lock")).await
}
fn random() -> String {
    URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>())
}
async fn host_id() -> Result<String> {
    let root = root()?;
    let _lock = crate::auth::StoreLock::acquire_path(&root.join("host.lock")).await?;
    let path = root.join("host.json");
    if path.exists() {
        return read_json(&path)?["id"]
            .as_str()
            .filter(|s| s.starts_with("urn:uuid:") && s.len() == 45)
            .map(String::from)
            .context("invalid ChatGPT host ID");
    }
    let mut bytes = rand::random::<[u8; 16]>();
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!(
        "urn:uuid:{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    crate::auth::write_secret(&path, &serde_json::to_vec(&json!({"id":id}))?)?;
    Ok(id)
}
pub fn connected(name: &str) -> bool {
    load(name).is_ok_and(|a| a.sharing())
}
pub(crate) fn summary(name: &str) -> Value {
    load(name)
        .map(|a| a.summary())
        .unwrap_or_else(|_| json!({"connected":false,"sharing":false}))
}
pub(crate) fn known_secrets() -> Vec<String> {
    let mut secrets = Vec::new();
    if let Ok(root) = root() {
        if let Ok(entries) = std::fs::read_dir(root) {
            for entry in entries.flatten() {
                if let Ok(value) = read_json(&entry.path()) {
                    for key in ["access_token", "refresh_token", "id_token"] {
                        if let Some(token) = value[key].as_str().filter(|s| !s.is_empty()) {
                            secrets.push(token.into());
                        }
                    }
                }
            }
        }
    }
    secrets
}

async fn json_response(mut response: reqwest::Response) -> Result<Value> {
    // Never expose server error bodies: they can echo credentials or a code.
    if !response.status().is_success() {
        bail!("ChatGPT request failed (HTTP {}); reconnect the account or manage usage in ChatGPT Settings", response.status().as_u16());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(crate::provider::failure::Failure::transport)?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            bail!("ChatGPT response exceeds 1 MiB");
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| anyhow::anyhow!("invalid ChatGPT response"))
}
async fn token_request(
    client: &reqwest::Client,
    endpoint: &str,
    params: &[(&str, &str)],
) -> Result<Value> {
    json_response(
        client
            .post(endpoint)
            .form(params)
            .send()
            .await
            .map_err(crate::provider::failure::Failure::transport)?,
    )
    .await
}
#[derive(Clone, Deserialize)]
struct Identity {
    sub: String,
    #[serde(default)]
    email: String,
    #[serde(default)]
    nonce: String,
}
fn verify_identity(
    token: &str,
    client: &str,
    nonce: Option<&str>,
    keys: &JwkSet,
) -> Result<Identity> {
    let header =
        decode_header(token).map_err(|_| anyhow::anyhow!("invalid ChatGPT identity token"))?;
    if header.alg != Algorithm::RS256 {
        bail!("unsupported ChatGPT identity signature");
    }
    let jwk = keys
        .find(
            header
                .kid
                .as_deref()
                .context("identity token has no key ID")?,
        )
        .context("unknown ChatGPT signing key")?;
    let key = DecodingKey::from_jwk(jwk).context("invalid ChatGPT signing key")?;
    let mut validation = Validation::new(Algorithm::RS256);
    validation.set_issuer(&[ISSUER]);
    validation.set_audience(&[client]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.validate_nbf = true;
    validation.leeway = 30;
    let identity = decode::<Identity>(token, &key, &validation)
        .map_err(|_| anyhow::anyhow!("ChatGPT identity verification failed"))?
        .claims;
    if identity.sub.is_empty() || nonce.is_some_and(|n| n != identity.nonce) {
        bail!("ChatGPT identity does not match this sign-in attempt");
    }
    Ok(identity)
}
async fn keys(client: &reqwest::Client) -> Result<JwkSet> {
    let value = json_response(
        client
            .get(format!("{ISSUER}/.well-known/jwks.json"))
            .send()
            .await
            .map_err(crate::provider::failure::Failure::transport)?,
    )
    .await?;
    serde_json::from_value(value).context("invalid ChatGPT signing keys")
}
fn update_tokens(account: &mut Account, value: &Value, refresh: bool) -> Result<()> {
    let access = value["access_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("ChatGPT did not return an access token")?;
    let replacement = value["refresh_token"]
        .as_str()
        .filter(|s| !s.is_empty())
        .context("ChatGPT did not return a rotating refresh token")?;
    if !value["token_type"]
        .as_str()
        .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
    {
        bail!("unsupported ChatGPT token type");
    }
    let expires = value["expires_in"]
        .as_u64()
        .filter(|n| *n > 0 && *n <= 86400)
        .context("invalid ChatGPT token expiry")?;
    let scopes = match value["scope"].as_str() {
        Some(scope) => scope.split_whitespace().map(String::from).collect(),
        None if refresh => account.scopes.clone(),
        _ => bail!("ChatGPT did not return granted permissions"),
    };
    account.access_token = access.into();
    account.refresh_token = replacement.into();
    if let Some(token) = value["id_token"].as_str() {
        account.id_token = token.into();
    }
    account.scopes = scopes;
    account.expires_at = crate::auth::now() + expires;
    account.earliest_refresh_at = value["earliest_refresh_at"].as_u64().unwrap_or(0);
    Ok(())
}

pub(crate) struct Login {
    pub url: String,
    listener: TcpListener,
    redirect: String,
    state: String,
    nonce: String,
    verifier: String,
    name: String,
    account: Account,
    _lock: crate::auth::StoreLock,
}
impl Login {
    pub(crate) async fn begin(name: &str, registration: Option<Account>) -> Result<Self> {
        validate_name(name)?;
        if crate::config::profiles(None)?
            .get(name)
            .is_some_and(|p| p.kind.as_deref() != Some("chatgpt"))
        {
            bail!("this profile belongs to another provider; choose a new profile name");
        }
        let _lock = lock(name).await?;
        let account = if let Some(registration) = registration {
            registration
        } else if path(name)?.exists() {
            load(name)?
        } else {
            Account::default()
        };
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let redirect = format!(
            "http://127.0.0.1:{}/auth/callback",
            listener.local_addr()?.port()
        );
        let state = random();
        let nonce = random();
        let verifier = random();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let host = host_id().await?;
        let mut url = reqwest::Url::parse(AUTHORIZE)?;
        {
            let mut pairs = url.query_pairs_mut();
            pairs.extend_pairs([
                (
                    "client_id",
                    if account.client_id.is_empty() {
                        DYNAMIC
                    } else {
                        &account.client_id
                    },
                ),
                ("ext_agent_host_id", &host),
                ("redirect_uri", &redirect),
                ("response_type", "code"),
                ("scope", SCOPE),
                ("resource", RESOURCE),
                ("state", &state),
                ("nonce", &nonce),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
            ]);
            if account.client_id.is_empty() {
                pairs.append_pair("agent_name_hint", "SUI");
            }
            if !account.id_token.is_empty() {
                pairs.append_pair("id_token_hint", &account.id_token);
            }
            if !account.email.is_empty() {
                pairs.append_pair("login_hint", &account.email);
            }
        }
        Ok(Self {
            url: url.into(),
            listener,
            redirect,
            state,
            nonce,
            verifier,
            name: name.into(),
            account,
            _lock,
        })
    }
    pub(crate) async fn finish(mut self, transfer: bool) -> Result<Value> {
        let callback = tokio::time::timeout(
            Duration::from_secs(600),
            receive_callback(&self.listener, &self.state),
        )
        .await
        .context("ChatGPT sign-in timed out; try again")??;
        let (code, client_id) = parse_callback(&callback, &self.state, &self.account.client_id)?;
        if self.account.client_id.is_empty() {
            self.account.client_id = client_id;
            // Retain registration before exchange, even if the code expires.
            save(&self.name, &self.account)?;
        }
        let client = crate::auth::client()?;
        let value = token_request(
            &client,
            TOKEN,
            &[
                ("grant_type", "authorization_code"),
                ("client_id", &self.account.client_id),
                ("code", &code),
                ("code_verifier", &self.verifier),
                ("redirect_uri", &self.redirect),
                ("resource", RESOURCE),
            ],
        )
        .await?;
        let identity = verify_identity(
            value["id_token"]
                .as_str()
                .context("missing ChatGPT identity token")?,
            &self.account.client_id,
            Some(&self.nonce),
            &keys(&client).await?,
        )?;
        if !self.account.subject.is_empty() && self.account.subject != identity.sub {
            bail!("ChatGPT account changed; add a separate profile instead");
        }
        update_tokens(&mut self.account, &value, false)?;
        self.account.subject = identity.sub;
        self.account.email = identity.email;
        if transfer {
            let record = serde_json::to_value(&self.account)?;
            self.account.clear_tokens();
            save(&self.name, &self.account)?;
            // Private host-to-host relay only. Never forward this value to a renderer.
            Ok(json!({"type":"transfer","record":record}))
        } else {
            save(&self.name, &self.account)?;
            Ok(json!({"type":"account","account":self.account.summary()}))
        }
    }
}
fn parse_callback(
    url: &reqwest::Url,
    state: &str,
    previous_client: &str,
) -> Result<(String, String)> {
    let mut values = std::collections::BTreeMap::new();
    for (key, value) in url.query_pairs() {
        if values
            .insert(key.into_owned(), value.into_owned())
            .is_some()
        {
            bail!("duplicate OAuth callback parameter");
        }
    }
    if values.get("state").map(String::as_str) != Some(state) {
        bail!("ChatGPT sign-in state mismatch");
    }
    if values.contains_key("error") {
        bail!("ChatGPT sign-in was declined; try again to enable ChatGPT plan usage");
    }
    let code = values
        .get("code")
        .filter(|s| !s.is_empty())
        .context("missing authorization code")?
        .clone();
    let client = values
        .get("client_id")
        .map(String::as_str)
        .unwrap_or(previous_client);
    if client.is_empty()
        || client == DYNAMIC
        || client.len() > 200
        || (!previous_client.is_empty() && client != previous_client)
    {
        bail!("ChatGPT registration does not match this account");
    }
    Ok((code, client.into()))
}
async fn receive_callback(listener: &TcpListener, state: &str) -> Result<reqwest::Url> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let request = tokio::time::timeout(Duration::from_secs(5), async {
            let mut bytes = Vec::new();
            while !bytes.ends_with(b"\r\n\r\n") {
                if bytes.len() >= 16384 {
                    bail!("OAuth callback exceeds 16 KiB");
                }
                bytes.push(stream.read_u8().await?);
            }
            String::from_utf8(bytes).context("invalid callback request")
        })
        .await;
        let Ok(Ok(request)) = request else {
            continue;
        };
        let Some(target) = request
            .lines()
            .next()
            .and_then(|line| line.strip_prefix("GET "))
            .and_then(|line| line.strip_suffix(" HTTP/1.1"))
        else {
            continue;
        };
        let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        let valid = url.path() == "/auth/callback"
            && url
                .query_pairs()
                .any(|(key, value)| key == "state" && value == state);
        let status = if valid { "200 OK" } else { "400 Bad Request" };
        let body = "Return to SUI to finish your sign-in.\n";
        let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}", body.len());
        let _ = tokio::time::timeout(
            Duration::from_secs(2),
            stream.write_all(response.as_bytes()),
        )
        .await;
        if valid {
            return Ok(url);
        }
    }
}

pub(crate) fn registration(name: &str) -> Result<Value> {
    validate_name(name)?;
    if crate::config::profiles(None)?
        .get(name)
        .is_some_and(|p| p.kind.as_deref() != Some("chatgpt"))
    {
        bail!("choose a new ChatGPT profile name");
    }
    if !path(name)?.exists() {
        return Ok(json!({"type":"registration","record":null}));
    }
    let mut account = load(name)?;
    account.access_token.clear();
    account.refresh_token.clear();
    account.scopes.clear();
    account.expires_at = 0;
    Ok(json!({"type":"registration","record":account}))
}

pub(crate) async fn import(name: &str, value: Value) -> Result<Value> {
    validate_name(name)?;
    let _host = host_id().await?; // Preserve the VM's host identity; never copy the laptop's.
    let _lock = lock(name).await?;
    if crate::config::profiles(None)?
        .get(name)
        .is_some_and(|p| p.kind.as_deref() != Some("chatgpt"))
    {
        bail!("choose a new ChatGPT profile name");
    }
    let account: Account = serde_json::from_value(value)
        .map_err(|_| anyhow::anyhow!("invalid ChatGPT credential transfer"))?;
    if account.client_id.is_empty()
        || account.client_id == DYNAMIC
        || account.refresh_token.is_empty()
        || account.access_token.is_empty()
    {
        bail!("incomplete ChatGPT credential transfer");
    }
    let identity = verify_identity(
        &account.id_token,
        &account.client_id,
        None,
        &keys(&crate::auth::client()?).await?,
    )?;
    if identity.sub != account.subject {
        bail!("transferred account identity does not match");
    }
    if path(name)?.exists() {
        let previous = load(name)?;
        if !previous.client_id.is_empty()
            && (previous.client_id != account.client_id
                || (!previous.subject.is_empty() && previous.subject != account.subject))
        {
            bail!(
                "this profile already belongs to another ChatGPT registration; choose a new name"
            );
        }
    }
    let summary = account.summary();
    save(name, &account)?;
    Ok(json!({"type":"account","account":summary}))
}

pub(crate) async fn access(name: &str) -> Result<String> {
    let _lock = lock(name).await?;
    let mut account = load(name).context("Connect your ChatGPT account in provider setup")?;
    if !account.sharing() {
        bail!("Enable ChatGPT plan usage by reconnecting this account");
    }
    let now = crate::auth::now();
    if account.expires_at <= now + 120 && now >= account.earliest_refresh_at {
        let client = crate::auth::client()?;
        refresh(&mut account, &client, TOKEN).await?;
        save(name, &account)?;
    }
    if !account.sharing() {
        bail!("ChatGPT plan permission changed; reconnect this account");
    }
    if account.expires_at <= now {
        bail!("ChatGPT session expired; reconnect this account");
    }
    Ok(account.access_token)
}
async fn refresh(account: &mut Account, client: &reqwest::Client, endpoint: &str) -> Result<()> {
    let value = token_request(
        client,
        endpoint,
        &[
            ("grant_type", "refresh_token"),
            ("client_id", &account.client_id),
            ("refresh_token", &account.refresh_token),
            ("resource", RESOURCE),
        ],
    )
    .await?;
    // Keep the original verified ID token as the returning-account hint.
    let hint = account.id_token.clone();
    update_tokens(account, &value, true)?;
    account.id_token = hint;
    Ok(())
}
pub(crate) async fn logout(name: &str) -> Result<Value> {
    let _lock = lock(name).await?;
    let mut account = load(name)?;
    let confirmed = if account.refresh_token.is_empty() {
        true
    } else {
        let client = crate::auth::client()?;
        client
            .post(format!("{ISSUER}/api/accounts/oauth/revoke"))
            .form(&[
                ("token", account.refresh_token.as_str()),
                ("token_type_hint", "refresh_token"),
                ("client_id", account.client_id.as_str()),
            ])
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    };
    account.clear_tokens();
    save(name, &account)?;
    Ok(json!({"type":"account","account":account.summary(),"revocation_confirmed":confirmed}))
}
pub(crate) async fn models(name: &str) -> Result<Vec<(String, String)>> {
    let access = access(name).await?;
    let client = crate::auth::client()?;
    let value = json_response(
        client
            .get(format!("{RESOURCE}/models"))
            .bearer_auth(access)
            .send()
            .await
            .map_err(crate::provider::failure::Failure::transport)?,
    )
    .await?;
    parse_models(&value)
}
fn parse_models(value: &Value) -> Result<Vec<(String, String)>> {
    let rows = value["models"]
        .as_array()
        .context("ChatGPT model catalog is missing")?;
    if rows.len() > 1000 {
        bail!("ChatGPT model catalog exceeds 1000 entries");
    }
    Ok(rows
        .iter()
        .filter(|row| row["visibility"] == "list")
        .filter_map(|row| {
            let id = row["slug"]
                .as_str()
                .filter(|s| !s.is_empty() && s.len() <= 200)?;
            Some((
                id.into(),
                row["display_name"]
                    .as_str()
                    .unwrap_or(id)
                    .chars()
                    .take(200)
                    .collect(),
            ))
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn claims() -> Value {
        json!({"iss":ISSUER,"aud":"oaiapp_test","sub":"test-subject","email":"fixture@example.com","nonce":"test-nonce","exp":crate::auth::now()+600})
    }
    fn signed(value: &Value) -> String {
        let mut header = jsonwebtoken::Header::new(Algorithm::RS256);
        header.kid = Some("sui-test-key".into());
        jsonwebtoken::encode(
            &header,
            value,
            &jsonwebtoken::EncodingKey::from_rsa_der(include_bytes!(
                "../tests/fixtures/chatgpt/test-key.der"
            )),
        )
        .unwrap()
    }
    fn jwks() -> JwkSet {
        serde_json::from_str(include_str!("../tests/fixtures/chatgpt/jwks.json")).unwrap()
    }
    fn account() -> Account {
        Account {
            client_id: "oaiapp_test".into(),
            subject: "test-subject".into(),
            email: "fixture@example.com".into(),
            access_token: "private-access-fixture".into(),
            refresh_token: "private-refresh-fixture".into(),
            id_token: signed(&claims()),
            scopes: SCOPE.split_whitespace().map(String::from).collect(),
            expires_at: crate::auth::now() + 3600,
            earliest_refresh_at: 0,
        }
    }
    #[test]
    fn identity_requires_signature_issuer_audience_expiry_subject_and_nonce() {
        assert_eq!(
            verify_identity(
                &signed(&claims()),
                "oaiapp_test",
                Some("test-nonce"),
                &jwks()
            )
            .unwrap()
            .sub,
            "test-subject"
        );
        for (key, value) in [
            ("iss", json!("https://example.com")),
            ("aud", json!("another-client")),
            ("exp", json!(1)),
            ("sub", json!("")),
            ("nonce", json!("another-nonce")),
        ] {
            let mut value_claims = claims();
            value_claims[key] = value;
            assert!(
                verify_identity(
                    &signed(&value_claims),
                    "oaiapp_test",
                    Some("test-nonce"),
                    &jwks()
                )
                .is_err(),
                "{key}"
            );
        }
        let valid = signed(&claims());
        let mut parts: Vec<_> = valid.split('.').map(String::from).collect();
        parts[1] = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&json!({"iss":ISSUER,"aud":"oaiapp_test","sub":"attacker","exp":crate::auth::now()+600})).unwrap());
        assert!(verify_identity(&parts.join("."), "oaiapp_test", None, &jwks()).is_err());
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims(),
            &jsonwebtoken::EncodingKey::from_secret(b"fixture-secret"),
        )
        .unwrap();
        assert!(verify_identity(&token, "oaiapp_test", None, &jwks()).is_err());
        let mut missing = claims();
        missing.as_object_mut().unwrap().remove("exp");
        assert!(verify_identity(&signed(&missing), "oaiapp_test", None, &jwks()).is_err());
    }
    #[test]
    fn callback_rejects_missing_or_changed_registration_state_duplicates_and_denial() {
        let parse = |query: &str, previous: &str| {
            parse_callback(
                &reqwest::Url::parse(&format!("http://127.0.0.1/auth/callback?{query}")).unwrap(),
                "expected",
                previous,
            )
        };
        assert_eq!(
            parse("state=expected&code=fixture&client_id=oaiapp_test", "").unwrap(),
            ("fixture".into(), "oaiapp_test".into())
        );
        assert_eq!(
            parse("state=expected&code=fixture", "oaiapp_test")
                .unwrap()
                .1,
            "oaiapp_test"
        );
        for query in [
            "state=wrong&code=x&client_id=oaiapp_test",
            "state=expected&code=x",
            "state=expected&code=x&client_id=dynamic_agent_client",
            "state=expected&code=x&code=y&client_id=oaiapp_test",
            "state=expected&error=access_denied&client_id=oaiapp_test",
        ] {
            assert!(parse(query, "").is_err());
        }
        assert!(parse(
            "state=expected&code=x&client_id=oaiapp_other",
            "oaiapp_test"
        )
        .is_err());
    }
    #[test]
    fn local_registration_reuses_host_and_client_without_leaking_tokens() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let _home = crate::test_http::AuthHome::new();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let first = Login::begin("chatgpt", None).await.unwrap();
            let url = reqwest::Url::parse(&first.url).unwrap();
            let values: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(url.as_str().split('?').next().unwrap(), AUTHORIZE);
            assert_eq!(values["client_id"], DYNAMIC);
            assert_eq!(values["agent_name_hint"], "SUI");
            assert_eq!(values["resource"], RESOURCE);
            assert_eq!(values["scope"], SCOPE);
            assert_eq!(
                values["code_challenge"],
                URL_SAFE_NO_PAD.encode(Sha256::digest(first.verifier.as_bytes()))
            );
            assert_ne!(first.state, first.nonce);
            assert_eq!(
                reqwest::Url::parse(&first.redirect).unwrap().host_str(),
                Some("127.0.0.1")
            );
            let host = values["ext_agent_host_id"].clone();
            let old_state = first.state.clone();
            drop(first);
            let account = account();
            save("chatgpt", &account).unwrap();
            let second = Login::begin("chatgpt", None).await.unwrap();
            let url = reqwest::Url::parse(&second.url).unwrap();
            let values: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
            assert_eq!(values["client_id"], "oaiapp_test");
            assert_eq!(values["ext_agent_host_id"], host);
            assert_eq!(values["id_token_hint"], account.id_token);
            assert!(!values.contains_key("agent_name_hint"));
            assert_ne!(second.state, old_state);
            let safe = account.summary().to_string();
            assert!(!safe.contains("private-"));
            let registration = registration("chatgpt").unwrap();
            assert_eq!(registration["record"]["access_token"], "");
            assert_eq!(registration["record"]["refresh_token"], "");
            assert_eq!(registration["record"]["id_token"], account.id_token);
            assert!(known_secrets().contains(&account.access_token));
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(
                    std::fs::metadata(path("chatgpt").unwrap())
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    0o600
                );
            }
        });
    }
    #[test]
    fn model_catalog_uses_visible_slugs_preserves_order_and_keeps_cost_unknown() {
        let models = parse_models(&json!({"models":[{"slug":"second","display_name":"Second model","visibility":"list"},{"slug":"hidden","visibility":"hidden"},{"slug":"first","display_name":"First model","visibility":"list"},{"display_name":"Missing ID","visibility":"list"}]})).unwrap();
        assert_eq!(
            models,
            vec![
                ("second".into(), "Second model".into()),
                ("first".into(), "First model".into())
            ]
        );
        assert!(parse_models(&json!({"data":[]})).is_err());
    }
    #[test]
    fn refresh_rotates_tokens_as_one_record_and_never_sends_the_dynamic_client_id() {
        let (base, requests, task) = crate::test_http::server(vec![(200,"application/json",json!({"access_token":"new-private-access","refresh_token":"new-private-refresh","expires_in":3600,"token_type":"Bearer","scope":SCOPE,"earliest_refresh_at":42}).to_string())]);
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            let mut account = account();
            let hint = account.id_token.clone();
            refresh(&mut account, &crate::auth::client().unwrap(), &base)
                .await
                .unwrap();
            assert_eq!(account.access_token, "new-private-access");
            assert_eq!(account.refresh_token, "new-private-refresh");
            assert_eq!(account.id_token, hint);
            assert_eq!(account.earliest_refresh_at, 42);
            let requests = requests.lock().unwrap();
            let body = &requests[0].1;
            assert!(body.contains("client_id=oaiapp_test"));
            assert!(body.contains("grant_type=refresh_token"));
            assert!(body.contains("refresh_token=private-refresh-fixture"));
            assert!(!body.contains(DYNAMIC));
        });
        task.join().unwrap();
    }
    #[test]
    fn identity_only_permission_never_enables_inference_and_invalid_tokens_do_not_replace_credentials(
    ) {
        let mut account = account();
        let old_access = account.access_token.clone();
        let mut response = json!({"access_token":"changed","refresh_token":"replacement","token_type":"Bearer","expires_in":3600,"scope":"openid email"});
        response["token_type"] = json!("unsupported");
        assert!(update_tokens(&mut account, &response, false).is_err());
        assert_eq!(account.access_token, old_access);
        response["token_type"] = json!("Bearer");
        update_tokens(&mut account, &response, false).unwrap();
        assert!(!account.sharing());
        account.clear_tokens();
        assert_eq!(account.client_id, "oaiapp_test");
        assert_eq!(account.subject, "test-subject");
        assert!(account.id_token.is_empty());
    }
    #[test]
    fn profile_names_cannot_escape_credential_directory() {
        for name in ["", "../other", "a/b", "a.b", "name with spaces"] {
            assert!(validate_name(name).is_err());
        }
        assert!(validate_name("chatgpt-work_2").is_ok());
    }
}
