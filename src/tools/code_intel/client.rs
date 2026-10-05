//! A managed, bounded Rust language-server client. Source and returned-path
//! validation belong to the public tool wrapper; subprocess policy stays here.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

const FRAME_CAP: usize = 4 * 1024 * 1024;
const HEADER_CAP: usize = 8192;
const TRAFFIC_CAP: usize = 16 * 1024 * 1024;
const MESSAGE_CAP: usize = 1024;
const TEXT_CAP: usize = 512 * 1024;
const INIT_TIMEOUT: Duration = Duration::from_secs(60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const REAP_TIMEOUT: Duration = Duration::from_secs(3);
const QUERY_RETRIES: usize = 3;
const RETRY_DELAY: Duration = Duration::from_millis(25);
const CONFIG_ENTRIES: usize = 10_000;
const CONFIG_DEPTH: usize = 128;
const CONFIG_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceError {
    Unavailable,
    Timeout,
    Cancelled,
    Protocol,
    Unsupported,
    UnsafeConfiguration,
    ConfigCoverageUnknown,
    AnalysisChanged,
    InvalidPosition,
}
impl fmt::Display for ServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unavailable => "rust-analyzer is unavailable or failed to initialize; install the compiler component and retry",
            Self::Timeout => "rust-analyzer exceeded its deadline; session closed",
            Self::Cancelled => "code intelligence cancelled; session closed",
            Self::Protocol => "rust-analyzer returned an invalid, oversized or failed protocol response; session closed",
            Self::Unsupported => "this language-server operation is unsupported",
            Self::UnsafeConfiguration => "workspace rust-analyzer.toml configuration is unsupported; remove it before using code intelligence",
            Self::ConfigCoverageUnknown => "workspace analyzer configuration could not be safely verified within its scan limits; session closed",
            Self::AnalysisChanged => "rust-analyzer analysis changed repeatedly; session closed, retry after the server settles",
            Self::InvalidPosition => "line/column must identify a valid one-based Unicode character position",
        })
    }
}
impl std::error::Error for ServiceError {}
type Result<T> = std::result::Result<T, ServiceError>;

/// One lazy language-server session for one canonical workspace/worktree.
/// Trusted Rust callers may inject an executable for deterministic fixtures;
/// tool arguments never select a program or change the server configuration.
pub struct Service {
    root: PathBuf,
    program: PathBuf,
    args: Vec<String>,
    state: Mutex<Option<Client>>,
}
impl Service {
    pub fn new(root: PathBuf) -> Self {
        Self::with_program(root, PathBuf::from("rust-analyzer"), Vec::new())
    }

    pub fn with_program(root: PathBuf, program: PathBuf, args: Vec<String>) -> Self {
        let root = root.canonicalize().unwrap_or(root);
        Self {
            root,
            program,
            args,
            state: Mutex::new(None),
        }
    }

    pub fn workspace(&self) -> &Path {
        &self.root
    }

    /// A mutation invalidates every semantic observation, including other
    /// files and project metadata. Reconnect lazily for the next observation.
    pub async fn invalidate(&self) {
        let mut state = self.state.lock().await;
        if let Some(mut client) = state.take() {
            client.stop().await;
        }
    }

    /// Cancel pre-LSP work without waiting for or interrupting another caller
    /// that currently owns this service's request slot.
    pub async fn invalidate_idle(&self) {
        if let Ok(mut state) = self.state.try_lock() {
            if let Some(mut client) = state.take() {
                client.stop().await;
            }
        }
    }

    /// Inputs use one-based Unicode scalar positions. The result carries raw
    /// zero-based UTF-16 LSP ranges and honest server health/readiness metadata.
    #[allow(clippy::too_many_arguments)]
    pub async fn execute(
        &self,
        action: &str,
        path: &Path,
        text: &str,
        line: usize,
        column: usize,
        _limit: usize,
        cancel: impl Future<Output = ()>,
    ) -> Result<Value> {
        if text.len() > TEXT_CAP || !path.starts_with(&self.root) {
            return Err(ServiceError::Protocol);
        }
        let position = if action == "diagnostics" {
            Value::Null
        } else {
            position(text, line, column)?
        };
        if !matches!(action, "definition" | "references" | "diagnostics") {
            return Err(ServiceError::Unsupported);
        }
        tokio::pin!(cancel);
        let mut state = tokio::select! {
            biased;
            state = self.state.lock() => state,
            _ = &mut cancel => return Err(ServiceError::Cancelled),
        };
        // Acquire an available cache before checking a ready cancellation so
        // it is closed too. A cancelled waiter never interrupts another owner.
        let cancelled = tokio::select! {
            biased;
            _ = &mut cancel => true,
            _ = std::future::ready(()) => false,
        };
        if cancelled {
            if let Some(mut client) = state.take() {
                client.stop().await;
            }
            return Err(ServiceError::Cancelled);
        }
        // Take ownership out of the shared slot during the operation. Dropping
        // this tool future drops Client and kills/reaps its subprocess too.
        let mut cached = state.take();
        // Local RA configuration can override diagnostic policy even below
        // the workspace root. Inspect the bounded, non-excluded tree first.
        if let Err(error) = verify_configuration(self.root.clone(), &mut cancel).await {
            if let Some(client) = &mut cached {
                client.stop().await;
            }
            return Err(error);
        }
        let mut client = match cached {
            Some(client) => client,
            None => Client::spawn(&self.root, &self.program, &self.args)?,
        };
        let operation = async {
            let change_detached_source = client
                .detached_source
                .as_deref()
                .is_some_and(|source| source != path);
            if !client.initialized || change_detached_source {
                tokio::time::timeout(INIT_TIMEOUT, async {
                    // Detached files belong to the crate graph configured at
                    // initialization. A different file needs a fresh graph.
                    if change_detached_source {
                        client.stop().await;
                        client = Client::spawn(&self.root, &self.program, &self.args)?;
                    }
                    client.initialize(&self.root, path).await
                })
                .await
                .map_err(|_| ServiceError::Timeout)??;
            }
            tokio::time::timeout(
                REQUEST_TIMEOUT,
                client.query(action, &self.root, path, text, position),
            )
            .await
            .map_err(|_| ServiceError::Timeout)?
        };
        let result = tokio::select! {
            biased;
            _ = &mut cancel => Err(ServiceError::Cancelled),
            result = operation => result,
        };
        match result {
            Ok(value) => {
                *state = Some(client);
                Ok(value)
            }
            Err(error) => {
                client.stop().await;
                Err(error)
            }
        }
    }
}

struct Document {
    version: i64,
    digest: [u8; 32],
}
struct Client {
    child: Child,
    group: Option<u32>,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    stderr: tokio::task::JoinHandle<()>,
    initialized: bool,
    capabilities: Value,
    next_id: u64,
    documents: BTreeMap<PathBuf, Document>,
    health: &'static str,
    quiescent: bool,
    configuration: Value,
    detached_source: Option<PathBuf>,
    _config_home: ConfigHome,
}
impl Client {
    fn spawn(root: &Path, program: &Path, args: &[String]) -> Result<Self> {
        let config_home = ConfigHome::new()?;
        let mut command = Command::new(program);
        command
            .args(args)
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .env_clear();
        #[cfg(unix)]
        command.process_group(0);
        for key in [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "LANG",
            "LC_ALL",
            "TMPDIR",
            "RUSTUP_HOME",
            "CARGO_HOME",
            "RUSTUP_TOOLCHAIN",
            "XDG_CACHE_HOME",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("CARGO_NET_OFFLINE", "true")
            .env("RUSTUP_AUTO_INSTALL", "0")
            .env("XDG_CONFIG_HOME", &config_home.0)
            .env("RA_LOG", "error");
        let mut child = command.spawn().map_err(|_| ServiceError::Unavailable)?;
        let group = child.id();
        let stdin = child.stdin.take().ok_or(ServiceError::Protocol)?;
        let stdout = child.stdout.take().ok_or(ServiceError::Protocol)?;
        let mut stderr = child.stderr.take().ok_or(ServiceError::Protocol)?;
        // Always drain stderr so it cannot block the server. Retain nothing:
        // server logs may contain source/configuration and are not tool evidence.
        let stderr = tokio::spawn(async move {
            let mut buffer = [0u8; 8192];
            loop {
                match stderr.read(&mut buffer).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
        });
        Ok(Self {
            child,
            group,
            stdin,
            stdout: BufReader::new(stdout),
            stderr,
            initialized: false,
            capabilities: Value::Null,
            next_id: 1,
            documents: BTreeMap::new(),
            health: "unknown",
            quiescent: false,
            configuration: Value::Null,
            detached_source: None,
            _config_home: config_home,
        })
    }

    async fn initialize(&mut self, root: &Path, path: &Path) -> Result<()> {
        let uri = file_uri(root)?;
        self.configuration = configuration(root, path)?;
        self.detached_source = self.configuration["detachedFiles"]
            .as_array()
            .filter(|files| !files.is_empty())
            .map(|_| path.to_path_buf());
        let result = self.request("initialize", json!({
            "processId": std::process::id(),
            "clientInfo": {"name":"sui", "version":env!("CARGO_PKG_VERSION")},
            "rootUri":uri,
            "workspaceFolders":[{"uri":uri,"name":"workspace"}],
            "capabilities": {
                "general":{"positionEncodings":["utf-16"]},
                "workspace":{"configuration":true,"workspaceFolders":true,
                    "diagnostic":{"refreshSupport":true},"applyEdit":false},
                "textDocument": {
                    "synchronization":{"dynamicRegistration":false,"didSave":false},
                    "definition":{"linkSupport":true},
                    "references":{"dynamicRegistration":false},
                    "diagnostic":{"dynamicRegistration":false,"relatedDocumentSupport":false}
                },
                "experimental":{"serverStatusNotification":true}
            },
            "initializationOptions": self.configuration
        })).await.map_err(|error| if error == ServiceError::Protocol { ServiceError::Unavailable } else { error })?;
        self.capabilities = result
            .get("capabilities")
            .cloned()
            .ok_or(ServiceError::Protocol)?;
        if self
            .capabilities
            .get("positionEncoding")
            .and_then(Value::as_str)
            .is_some_and(|encoding| encoding != "utf-16")
        {
            return Err(ServiceError::Unsupported);
        }
        self.notify("initialized", json!({})).await?;
        self.wait_ready().await?;
        self.initialized = true;
        Ok(())
    }

    async fn query(
        &mut self,
        action: &str,
        root: &Path,
        path: &Path,
        text: &str,
        position: Value,
    ) -> Result<Value> {
        let (method, capability) = match action {
            "definition" => ("textDocument/definition", "definitionProvider"),
            "references" => ("textDocument/references", "referencesProvider"),
            "diagnostics" => ("textDocument/diagnostic", "diagnosticProvider"),
            _ => return Err(ServiceError::Unsupported),
        };
        if self
            .capabilities
            .get(capability)
            .is_none_or(|value| value.is_null() || value == &Value::Bool(false))
        {
            return Err(ServiceError::Unsupported);
        }
        self.sync_document(path, text).await?;
        let uri = file_uri(path)?;
        let params = match action {
            "diagnostics" => json!({"textDocument":{"uri":uri},"identifier":"rust-analyzer"}),
            "references" => {
                json!({"textDocument":{"uri":uri},"position":position,"context":{"includeDeclaration":true}})
            }
            _ => json!({"textDocument":{"uri":uri},"position":position}),
        };
        let result = self.request_with_retry(method, params).await?;
        // Membership is a later observation. Becoming ready while answering
        // it cannot promote a semantic result obtained during loading.
        let semantic_health = self.health;
        let semantic_quiescent = self.quiescent;
        // A healthy Cargo workspace does not imply this file belongs to its
        // graph. RA's typed extension derives the file's actual Cargo target.
        let file_in_project = if self.detached_source.is_some() {
            Some(false)
        } else {
            match self
                .request_with_retry(
                    "experimental/openCargoToml",
                    json!({"textDocument":{"uri":uri}}),
                )
                .await
            {
                Ok(membership) => project_membership(root, &membership),
                Err(ServiceError::Unsupported) => None,
                Err(error) => return Err(error),
            }
        };
        Ok(
            json!({"result":result,"analysis_health":if semantic_health == "ok" { self.health } else { semantic_health },
            "quiescent":semantic_quiescent && self.quiescent,
            "project_mode":if self.detached_source.is_some() { "detached" } else { "cargo" },
            "file_in_project":file_in_project}),
        )
    }

    async fn request_with_retry(&mut self, method: &str, params: Value) -> Result<Value> {
        // All attempts share the outer query deadline and cancellation.
        let mut retries = 0;
        loop {
            match self.request(method, params.clone()).await {
                Err(ServiceError::AnalysisChanged) if retries < QUERY_RETRIES => {
                    retries += 1;
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                result => return result,
            }
        }
    }

    async fn sync_document(&mut self, path: &Path, text: &str) -> Result<()> {
        let uri = file_uri(path)?;
        // Keep only the queried file overlaid. Closed sibling files return to
        // server filesystem watching instead of retaining stale source buffers.
        let inactive: Vec<PathBuf> = self
            .documents
            .keys()
            .filter(|other| other.as_path() != path)
            .cloned()
            .collect();
        for other in inactive {
            self.notify(
                "textDocument/didClose",
                json!({"textDocument":{"uri":file_uri(&other)?}}),
            )
            .await?;
            self.documents.remove(&other);
        }
        let digest: [u8; 32] = Sha256::digest(text.as_bytes()).into();
        if let Some(document) = self.documents.get_mut(path) {
            if document.digest == digest {
                return Ok(());
            }
            document.version = document
                .version
                .checked_add(1)
                .ok_or(ServiceError::Protocol)?;
            document.digest = digest;
            let version = document.version;
            self.notify(
                "textDocument/didChange",
                json!({
                    "textDocument":{"uri":uri,"version":version},"contentChanges":[{"text":text}]
                }),
            )
            .await?;
        } else {
            self.notify(
                "textDocument/didOpen",
                json!({
                    "textDocument":{"uri":uri,"languageId":"rust","version":1,"text":text}
                }),
            )
            .await?;
            self.documents
                .insert(path.to_path_buf(), Document { version: 1, digest });
        }
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or(ServiceError::Protocol)?;
        self.write(&json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await?;
        let mut traffic = 0usize;
        for _ in 0..MESSAGE_CAP {
            let message = self.read(&mut traffic).await?;
            if message.get("method").is_none() && message.get("id") == Some(&json!(id)) {
                if let Some(error) = message.get("error") {
                    return Err(match error["code"].as_i64() {
                        Some(-32601) => ServiceError::Unsupported,
                        Some(-32801) => ServiceError::AnalysisChanged,
                        Some(-32802)
                            if error["data"]["retriggerRequest"].as_bool() == Some(true) =>
                        {
                            ServiceError::AnalysisChanged
                        }
                        _ => ServiceError::Protocol,
                    });
                }
                return message.get("result").cloned().ok_or(ServiceError::Protocol);
            }
            self.handle_message(message).await?;
        }
        Err(ServiceError::Protocol)
    }

    async fn wait_ready(&mut self) -> Result<()> {
        if self.quiescent {
            return Ok(());
        }
        let mut traffic = 0usize;
        for _ in 0..MESSAGE_CAP {
            let message = self.read(&mut traffic).await?;
            self.handle_message(message).await?;
            if self.quiescent {
                return Ok(());
            }
        }
        Err(ServiceError::Protocol)
    }

    async fn handle_message(&mut self, message: Value) -> Result<()> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return Ok(());
        };
        if let Some(id) = message.get("id") {
            let result = match method {
                "workspace/configuration" => {
                    let items = message["params"]["items"]
                        .as_array()
                        .ok_or(ServiceError::Protocol)?;
                    if items.len() > 64 {
                        return Err(ServiceError::Protocol);
                    }
                    let config = &self.configuration;
                    let values: Vec<Value> = items
                        .iter()
                        .map(|item| {
                            let section = item["section"].as_str().unwrap_or("rust-analyzer");
                            if section == "rust-analyzer" {
                                return config.clone();
                            }
                            section
                                .strip_prefix("rust-analyzer.")
                                .and_then(|section| {
                                    section
                                        .split('.')
                                        .try_fold(config, |value, key| value.get(key))
                                })
                                .cloned()
                                .unwrap_or(Value::Null)
                        })
                        .collect();
                    Some(Value::Array(values))
                }
                "window/workDoneProgress/create" | "workspace/diagnostic/refresh" => {
                    Some(Value::Null)
                }
                "workspace/applyEdit" => {
                    Some(json!({"applied":false,"failureReason":"code intelligence is read-only"}))
                }
                _ => None,
            };
            let reply = match result {
                Some(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
                None => {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"unsupported client operation"}})
                }
            };
            self.write(&reply).await?;
        } else if method == "experimental/serverStatus" {
            self.health = match message["params"]["health"].as_str() {
                Some("ok") => "ok",
                Some("warning") => "warning",
                Some("error") => "error",
                _ => "unknown",
            };
            self.quiescent = message["params"]["quiescent"].as_bool().unwrap_or(false);
        }
        // Push diagnostics, logs and progress are drained but never cached as
        // evidence. Only a fresh, correlated pull response reaches the wrapper.
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
            .await
    }

    async fn write(&mut self, message: &Value) -> Result<()> {
        let bytes = serde_json::to_vec(message).map_err(|_| ServiceError::Protocol)?;
        if bytes.len() > FRAME_CAP {
            return Err(ServiceError::Protocol);
        }
        let header = format!("Content-Length: {}\r\n\r\n", bytes.len());
        self.stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|_| ServiceError::Protocol)?;
        self.stdin
            .write_all(&bytes)
            .await
            .map_err(|_| ServiceError::Protocol)?;
        self.stdin.flush().await.map_err(|_| ServiceError::Protocol)
    }

    async fn read(&mut self, traffic: &mut usize) -> Result<Value> {
        let mut header = Vec::new();
        loop {
            let buffer = self
                .stdout
                .fill_buf()
                .await
                .map_err(|_| ServiceError::Protocol)?;
            if buffer.is_empty() {
                return Err(ServiceError::Protocol);
            }
            let length = buffer
                .iter()
                .position(|byte| *byte == b'\n')
                .map_or(buffer.len(), |index| index + 1);
            if header.len() + length > HEADER_CAP {
                return Err(ServiceError::Protocol);
            }
            header.extend_from_slice(&buffer[..length]);
            self.stdout.consume(length);
            if header.ends_with(b"\r\n\r\n") || header.ends_with(b"\n\n") {
                break;
            }
        }
        let text = std::str::from_utf8(&header).map_err(|_| ServiceError::Protocol)?;
        let mut size = None;
        for line in text.lines().filter(|line| !line.is_empty()) {
            let (name, value) = line.split_once(':').ok_or(ServiceError::Protocol)?;
            if name.eq_ignore_ascii_case("Content-Length") {
                if size.is_some() {
                    return Err(ServiceError::Protocol);
                }
                size = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|_| ServiceError::Protocol)?,
                );
            }
        }
        let size = size
            .filter(|size| *size > 0 && *size <= FRAME_CAP)
            .ok_or(ServiceError::Protocol)?;
        *traffic = traffic
            .checked_add(header.len() + size)
            .ok_or(ServiceError::Protocol)?;
        if *traffic > TRAFFIC_CAP {
            return Err(ServiceError::Protocol);
        }
        let mut bytes = vec![0; size];
        self.stdout
            .read_exact(&mut bytes)
            .await
            .map_err(|_| ServiceError::Protocol)?;
        let message: Value = serde_json::from_slice(&bytes).map_err(|_| ServiceError::Protocol)?;
        if !message.is_object() || message["jsonrpc"] != "2.0" {
            return Err(ServiceError::Protocol);
        }
        if let Some(id) = message.get("id") {
            if !(id.is_string() || id.is_number()) {
                return Err(ServiceError::Protocol);
            }
        }
        if let Some(method) = message.get("method") {
            if !method.is_string()
                || message.get("result").is_some()
                || message.get("error").is_some()
            {
                return Err(ServiceError::Protocol);
            }
        } else if message.get("id").is_none()
            || message.get("result").is_some() == message.get("error").is_some()
        {
            return Err(ServiceError::Protocol);
        }
        Ok(message)
    }

    async fn stop(&mut self) {
        self.kill_group();
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(REAP_TIMEOUT, self.child.wait()).await;
        self.stderr.abort();
        let _ = (&mut self.stderr).await;
    }

    fn kill_group(&mut self) {
        if let Some(group) = self.group.take() {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(group as i32), libc::SIGKILL);
            }
            #[cfg(not(unix))]
            let _ = group;
        }
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.kill_group();
        let _ = self.child.start_kill();
        self.stderr.abort();
        // Drop cannot await. SIGKILL followed by bounded nonblocking wait
        // reaps normal local children even after a tool/runtime future is lost.
        let deadline = Instant::now() + Duration::from_millis(500);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Ok(None) => break,
            }
        }
    }
}

fn file_uri(path: &Path) -> Result<String> {
    reqwest::Url::from_file_path(path)
        .map(|uri| uri.to_string())
        .map_err(|_| ServiceError::Protocol)
}

fn position(text: &str, line: usize, column: usize) -> Result<Value> {
    let offset = column.checked_sub(1).ok_or(ServiceError::InvalidPosition)?;
    let line_number = line.checked_sub(1).ok_or(ServiceError::InvalidPosition)?;
    let content = text
        .split('\n')
        .nth(line_number)
        .ok_or(ServiceError::InvalidPosition)?;
    let content = content.strip_suffix('\r').unwrap_or(content);
    if offset > content.chars().count() {
        return Err(ServiceError::InvalidPosition);
    }
    let character: usize = content.chars().take(offset).map(char::len_utf16).sum();
    Ok(json!({"line":line_number,"character":character}))
}

fn configuration(root: &Path, source: &Path) -> Result<Value> {
    let project = root.join("Cargo.toml");
    let linked = match project.symlink_metadata() {
        Ok(metadata) => metadata.file_type().is_file(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(_) => return Err(ServiceError::Unsupported),
    };
    let project = project.to_str().ok_or(ServiceError::Protocol)?;
    let source = source.to_str().ok_or(ServiceError::Protocol)?;
    Ok(json!({
        "cargo": {
            "buildScripts":{"enable":false,"rebuildOnSave":false,"overrideCommand":null},
            "extraArgs":["--locked","--offline"],
            "metadataExtraArgs":["--locked","--offline"],
            "extraEnv":{"CARGO_NET_OFFLINE":"true","RUSTUP_AUTO_INSTALL":"0"}
        },
        "procMacro":{"enable":false},
        "checkOnSave":false,
        "check":{"overrideCommand":null},
        "diagnostics":{"enable":true,"experimental":{"enable":false}},
        "cachePriming":{"enable":false},
        "files":{"watcher":"server"},
        "workspace":{"discoverConfig":null},
        "linkedProjects":if linked { vec![project] } else { Vec::<&str>::new() },
        "detachedFiles":if linked { Vec::<&str>::new() } else { vec![source] },
        "numThreads":2
    }))
}

fn project_membership(root: &Path, result: &Value) -> Option<bool> {
    if result.is_null() {
        return Some(false);
    }
    // The installed RA handler returns one Location. Accept the equivalent
    // single-location forms from compatible servers, with no raw URI output.
    let location = if let Some(items) = result.as_array() {
        if items.len() != 1 {
            return None;
        }
        &items[0]
    } else {
        result
    };
    let range = location
        .get("range")
        .or_else(|| location.get("targetSelectionRange"))?;
    let start = (
        range["start"]["line"].as_u64()?,
        range["start"]["character"].as_u64()?,
    );
    let end = (
        range["end"]["line"].as_u64()?,
        range["end"]["character"].as_u64()?,
    );
    if end < start {
        return None;
    }
    let uri = location["uri"]
        .as_str()
        .or_else(|| location["targetUri"].as_str())?;
    let uri = reqwest::Url::parse(uri).ok()?;
    if uri.query().is_some() || uri.fragment().is_some() {
        return None;
    }
    let path = uri.to_file_path().ok()?;
    let relative = path.strip_prefix(root).ok()?;
    if path.file_name()? != "Cargo.toml"
        || relative
            .components()
            .any(|component| matches!(component, std::path::Component::ParentDir))
        || excluded_configuration_path(relative)
    {
        return None;
    }
    regular_manifest(root, relative).then_some(true)
}

#[cfg(unix)]
fn regular_manifest(root: &Path, relative: &Path) -> bool {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    if relative.components().count() > CONFIG_DEPTH {
        return false;
    }
    let Ok(mut directory) = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(root)
    else {
        return false;
    };
    let mut components = relative.components().peekable();
    while let Some(component) = components.next() {
        let std::path::Component::Normal(name) = component else {
            return false;
        };
        let Ok(name) = std::ffi::CString::new(name.as_bytes()) else {
            return false;
        };
        let last = components.peek().is_none();
        let flags = libc::O_RDONLY
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | if last { 0 } else { libc::O_DIRECTORY };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return false;
        }
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        if last {
            return file.metadata().is_ok_and(|metadata| metadata.is_file());
        }
        directory = file;
    }
    false
}

#[cfg(not(unix))]
fn regular_manifest(_root: &Path, _relative: &Path) -> bool {
    false
}

struct ConfigurationScanCancel(Arc<AtomicBool>);
impl Drop for ConfigurationScanCancel {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

async fn verify_configuration(root: PathBuf, cancel: impl Future<Output = ()>) -> Result<()> {
    let flag = Arc::new(AtomicBool::new(false));
    let _guard = ConfigurationScanCancel(flag.clone());
    let worker_flag = flag.clone();
    let mut worker = tokio::task::spawn_blocking(move || {
        scan_configuration(&root, &worker_flag, Instant::now() + CONFIG_TIMEOUT)
    });
    tokio::pin!(cancel);
    tokio::select! {
        biased;
        _ = &mut cancel => {
            flag.store(true, Ordering::Relaxed);
            let _ = worker.await;
            Err(ServiceError::Cancelled)
        },
        result = &mut worker => result.unwrap_or(Err(ServiceError::ConfigCoverageUnknown)),
    }
}

fn excluded_configuration_path(relative: &Path) -> bool {
    super::super::inventory::excluded(relative)
        || relative.components().next().is_some_and(|component| {
            matches!(
                component.as_os_str().to_str(),
                Some("target" | "build" | "dist" | "coverage")
            )
        })
}

fn configuration_checkpoint(cancel: &AtomicBool, deadline: Instant) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(ServiceError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(ServiceError::ConfigCoverageUnknown)
    } else {
        Ok(())
    }
}

// Policy inspection intentionally ignores .gitignore/.ignore. These do not
// stop RA from loading configuration. No rule or source contents are read.
#[cfg(unix)]
fn scan_configuration(root: &Path, cancel: &AtomicBool, deadline: Instant) -> Result<()> {
    use std::os::fd::IntoRawFd;
    use std::os::unix::{ffi::OsStrExt, fs::OpenOptionsExt};
    configuration_checkpoint(cancel, deadline)?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(root)
        .map_err(|_| ServiceError::ConfigCoverageUnknown)?;
    struct Frame {
        directory: ConfigurationDirectory,
        relative: PathBuf,
        depth: usize,
    }
    let mut frames = vec![Frame {
        directory: ConfigurationDirectory::from_fd(file.into_raw_fd())?,
        relative: PathBuf::new(),
        depth: 0,
    }];
    let mut entries = 0usize;
    while let Some(frame) = frames.last_mut() {
        configuration_checkpoint(cancel, deadline)?;
        let Some(name) = frame.directory.next()? else {
            frames.pop();
            continue;
        };
        if name.as_bytes() == b"." || name.as_bytes() == b".." {
            continue;
        }
        if entries >= CONFIG_ENTRIES {
            return Err(ServiceError::ConfigCoverageUnknown);
        }
        entries += 1;
        let relative = frame
            .relative
            .join(std::ffi::OsStr::from_bytes(name.as_bytes()));
        if excluded_configuration_path(&relative) {
            continue;
        }
        if name.as_bytes() == b"rust-analyzer.toml" {
            return Err(ServiceError::UnsafeConfiguration);
        }
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // The parent is held open and fstatat never follows the child link.
        let status = unsafe {
            libc::fstatat(
                frame.directory.fd(),
                name.as_ptr(),
                metadata.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status != 0 {
            return Err(ServiceError::ConfigCoverageUnknown);
        }
        let metadata = unsafe { metadata.assume_init() };
        match metadata.st_mode & libc::S_IFMT {
            libc::S_IFLNK => return Err(ServiceError::ConfigCoverageUnknown),
            libc::S_IFDIR => {
                let depth = frame.depth + 1;
                if depth > CONFIG_DEPTH {
                    return Err(ServiceError::ConfigCoverageUnknown);
                }
                // Directory replacement with a link cannot redirect traversal.
                let fd = unsafe {
                    libc::openat(
                        frame.directory.fd(),
                        name.as_ptr(),
                        libc::O_RDONLY
                            | libc::O_DIRECTORY
                            | libc::O_NOFOLLOW
                            | libc::O_NONBLOCK
                            | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(ServiceError::ConfigCoverageUnknown);
                }
                frames.push(Frame {
                    directory: ConfigurationDirectory::from_fd(fd)?,
                    relative,
                    depth,
                });
            }
            _ => {}
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn scan_configuration(_root: &Path, _cancel: &AtomicBool, _deadline: Instant) -> Result<()> {
    Err(ServiceError::ConfigCoverageUnknown)
}

#[cfg(unix)]
struct ConfigurationDirectory(std::ptr::NonNull<libc::DIR>);
#[cfg(unix)]
impl ConfigurationDirectory {
    fn from_fd(fd: std::os::fd::RawFd) -> Result<Self> {
        let directory = unsafe { libc::fdopendir(fd) };
        match std::ptr::NonNull::new(directory) {
            Some(directory) => Ok(Self(directory)),
            None => {
                unsafe {
                    libc::close(fd);
                }
                Err(ServiceError::ConfigCoverageUnknown)
            }
        }
    }
    fn fd(&self) -> std::os::fd::RawFd {
        unsafe { libc::dirfd(self.0.as_ptr()) }
    }
    fn next(&mut self) -> Result<Option<std::ffi::CString>> {
        let errno = configuration_errno().ok_or(ServiceError::ConfigCoverageUnknown)?;
        // POSIX distinguishes EOF from failure through errno. Copy the name
        // before the next readdir invalidates the returned directory entry.
        unsafe {
            *errno = 0;
            let entry = libc::readdir(self.0.as_ptr());
            if entry.is_null() {
                return if *errno == 0 {
                    Ok(None)
                } else {
                    Err(ServiceError::ConfigCoverageUnknown)
                };
            }
            let name = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr());
            if name.to_bytes().len() > 4096 {
                return Err(ServiceError::ConfigCoverageUnknown);
            }
            Ok(Some(name.to_owned()))
        }
    }
}
#[cfg(unix)]
impl Drop for ConfigurationDirectory {
    fn drop(&mut self) {
        unsafe {
            libc::closedir(self.0.as_ptr());
        }
    }
}

#[cfg(unix)]
fn configuration_errno() -> Option<*mut libc::c_int> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        Some(unsafe { libc::__errno_location() })
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly"
    ))]
    {
        Some(unsafe { libc::__error() })
    }
    #[cfg(any(target_os = "netbsd", target_os = "openbsd"))]
    {
        Some(unsafe { libc::__errno() })
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "netbsd",
        target_os = "openbsd"
    )))]
    {
        None
    }
}

/// A private empty global RA configuration directory, outside the repository.
struct ConfigHome(PathBuf);
impl ConfigHome {
    fn new() -> Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "sui-code-intel-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(&path)
            .map_err(|_| ServiceError::Unavailable)?;
        Ok(Self(path))
    }
    fn remove(&self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
impl Drop for ConfigHome {
    fn drop(&mut self) {
        self.remove();
    }
}
