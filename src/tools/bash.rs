use anyhow::Result;
use serde_json::Value;
use std::collections::VecDeque;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::time::timeout;

use super::ToolContext;

/// Max bytes of a stream that may enter model-visible history.
/// Over the limit: keep head 60% + tail 40% with an omission marker.
/// Enforced DURING collection — the pipe is always drained (a full buffer
/// must never block the child) but retained bytes never exceed the cap.
const STREAM_CAP: usize = 30_000;
const HEAD: usize = STREAM_CAP * 6 / 10;
const TAIL: usize = STREAM_CAP - HEAD;

/// Explicit child-environment allowlist. The child does NOT inherit the
/// parent environment — credentials and unrelated secrets never leak in.
/// (Still not a sandbox: the process runs with full user privileges.)
const ENV_ALLOW: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "TZ",
    "RUSTUP_HOME",
    "CARGO_HOME",
    "GOPATH",
    "GOROOT",
    "NVM_DIR",
    "PYENV_ROOT",
    "XDG_CACHE_HOME",
    "XDG_CONFIG_HOME",
    "XDG_DATA_HOME",
];

/// One live output chunk observed mid-run.
#[derive(Debug)]
pub struct OutChunk {
    /// true = stderr, false = stdout.
    pub err: bool,
    pub text: String,
}

/// Bounded live-output tap for the UI. `try_send` only: a slow renderer
/// can never block the child's pipes; dropped chunks are counted so the
/// transcript can say "preview dropped N chunks" honestly.
#[derive(Clone)]
pub struct Observer {
    pub tx: tokio::sync::mpsc::Sender<OutChunk>,
    pub dropped: Arc<AtomicU64>,
}

impl Observer {
    /// Channel capacity for the preview tap. ~8KB reads → ≤512KB backlog.
    pub fn new(cap: usize) -> (Self, tokio::sync::mpsc::Receiver<OutChunk>) {
        let (tx, rx) = tokio::sync::mpsc::channel(cap);
        (
            Self {
                tx,
                dropped: Arc::new(AtomicU64::new(0)),
            },
            rx,
        )
    }
}

/// Raw outcome of a bounded child process — used by deterministic gates
/// that need the exit status rather than a model envelope.
pub struct ProcOut {
    /// None = killed (timeout or cancel).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    /// Preview chunks the UI tap dropped (channel full). Capture itself is
    /// unaffected — `truncated` covers the model-facing record.
    pub preview_dropped: u64,
}

/// Bounded retained buffer: head fills once, tail is a ring of the last
/// TAIL bytes. `total` tracks everything seen so the omission marker is
/// exact. Equivalent output to the old post-hoc head+tail bound.
struct CapBuf {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: usize,
}
impl CapBuf {
    fn new() -> Self {
        Self {
            head: Vec::new(),
            tail: VecDeque::new(),
            total: 0,
        }
    }
    fn append(&mut self, mut b: &[u8]) {
        self.total += b.len();
        let want = HEAD.saturating_sub(self.head.len());
        if want > 0 {
            let n = want.min(b.len());
            self.head.extend_from_slice(&b[..n]);
            b = &b[n..];
        }
        if !b.is_empty() {
            if self.tail.len() + b.len() > TAIL {
                let over = (self.tail.len() + b.len()) - TAIL;
                self.tail.drain(..over.min(self.tail.len()));
            }
            self.tail.extend(b.iter().copied());
        }
    }
    fn render(&self) -> (String, bool) {
        let mut all = Vec::with_capacity(self.head.len() + self.tail.len());
        all.extend_from_slice(&self.head);
        if self.total <= self.head.len() + self.tail.len() {
            all.extend(self.tail.iter().copied());
            return (String::from_utf8_lossy(&all).into_owned(), false);
        }
        let omitted = self.total - self.head.len() - self.tail.len();
        let mut s = String::from_utf8_lossy(&self.head).into_owned();
        s.push_str(&format!("\n... <{omitted} bytes omitted> ...\n"));
        s.push_str(&String::from_utf8_lossy(
            self.tail.iter().copied().collect::<Vec<u8>>().as_slice(),
        ));
        (s, true)
    }
}

/// Read a pipe to EOF: retained bytes go into `cap` (bounded), a bounded
/// preview copy goes to `obs` (best-effort, drop-counted).
fn pipe_task<R>(
    mut r: R,
    cap: Arc<Mutex<CapBuf>>,
    obs: Option<Observer>,
    err: bool,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let b = &chunk[..n];
                    cap.lock().unwrap().append(b);
                    if let Some(o) = &obs {
                        if o.tx
                            .try_send(OutChunk {
                                err,
                                text: String::from_utf8_lossy(b).into_owned(),
                            })
                            .is_err()
                        {
                            o.dropped.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }
        // rendering happens in spawn_bounded after both pipes hit EOF
    })
}

/// Spawn `bash -c <cmd>` in `dir` with filtered env, own process group,
/// bounded output, hard timeout, and a unified kill+reap path shared by
/// timeout and cancellation. `obs` receives live chunks (bounded).
pub async fn spawn_bounded(
    dir: &Path,
    cmd: &str,
    dur: Duration,
    dur_max: Duration,
    cancel: impl Future<Output = ()>,
    obs: Option<Observer>,
) -> Result<ProcOut> {
    let dur = dur.min(dur_max);

    let mut c = Command::new("bash");
    c.arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0) // own pgid so the whole tree can be killed
        .kill_on_drop(true)
        .env_clear();
    for k in ENV_ALLOW {
        if let Ok(v) = std::env::var(k) {
            c.env(k, v);
        }
    }
    let mut child = c.spawn()?;
    let pid = child.id();
    // If this future is dropped mid-await (e.g. mission-level cancel),
    // kill_on_drop kills bash but NOT its group — the guard covers that.
    let _guard = PgGuard(pid);

    // Stream outputs on reader tasks so `child` stays alive in scope —
    // required to kill the process group and reap on timeout/cancel.
    let out_cap = Arc::new(Mutex::new(CapBuf::new()));
    let err_cap = Arc::new(Mutex::new(CapBuf::new()));
    let so = child.stdout.take().expect("piped stdout");
    let se = child.stderr.take().expect("piped stderr");
    let dropped = obs.as_ref().map(|o| o.dropped.clone());
    let out_h = pipe_task(so, out_cap.clone(), obs.clone(), false);
    let err_h = pipe_task(se, err_cap.clone(), obs, true);

    enum End {
        Done(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }
    let end = {
        tokio::select! {
            r = timeout(dur, child.wait()) => match r {
                Ok(s) => End::Done(s),
                Err(_) => End::TimedOut,
            },
            _ = cancel => End::Cancelled,
        }
    };
    let cancelled = matches!(end, End::Cancelled);
    let timed_out = matches!(end, End::TimedOut);

    let status = match end {
        End::Done(s) => Some(s),
        End::TimedOut | End::Cancelled => {
            kill_tree(pid);
            // reap so no zombie/orphan survives the harness
            let _ = child.wait().await;
            None
        }
    };
    let _ = out_h.await;
    let _ = err_h.await;
    let (stdout, t1) = out_cap.lock().unwrap().render();
    let (stderr, t2) = err_cap.lock().unwrap().render();
    Ok(ProcOut {
        code: status.and_then(|s| s.ok()).and_then(|s| s.code()),
        stdout,
        stderr,
        truncated: t1 || t2,
        timed_out,
        cancelled,
        preview_dropped: dropped.map(|d| d.load(Ordering::Relaxed)).unwrap_or(0),
    })
}

pub async fn run(
    ctx: &ToolContext,
    args: &Value,
    cancel: impl Future<Output = ()>,
    obs: Option<Observer>,
) -> Result<super::ExecOut> {
    use super::ExecKind;
    let cmd = args["command"].as_str().unwrap_or("").to_string();
    if cmd.trim().is_empty() {
        return Ok(super::ExecOut::plain(
            "status: error\nerror: empty command".into(),
            ExecKind::Error,
        ));
    }
    let req_ms = args["timeout_ms"]
        .as_u64()
        .unwrap_or(ctx.bash_timeout.as_millis() as u64);
    let dur = Duration::from_millis(req_ms);
    let out = spawn_bounded(&ctx.workspace, &cmd, dur, ctx.bash_timeout_max, cancel, obs).await?;
    let dropped = out.preview_dropped;

    if out.cancelled {
        return Ok(super::ExecOut {
            text: "status: cancelled\nerror: interrupted by user".into(),
            kind: ExecKind::Cancelled,
            exit: None,
            truncated: out.truncated,
            preview_dropped: dropped,
        });
    }
    if out.timed_out {
        return Ok(super::ExecOut {
            text: format!(
                "status: timeout\nerror: exceeded {}ms\nhint: rerun bounded or pass larger timeout_ms",
                dur.as_millis()
            ),
            kind: ExecKind::Timeout,
            exit: None,
            truncated: out.truncated,
            preview_dropped: dropped,
        });
    }
    let code = out.code.unwrap_or(-1);
    Ok(super::ExecOut {
        text: format!(
            "status: {}\nexit_code: {}\nstdout: {}\nstderr: {}\ntruncated: {}",
            if code == 0 { "success" } else { "failed" },
            code,
            if out.stdout.is_empty() {
                "<empty>".into()
            } else {
                out.stdout
            },
            if out.stderr.is_empty() {
                "<empty>".into()
            } else {
                out.stderr
            },
            out.truncated
        ),
        kind: if code == 0 {
            ExecKind::Success
        } else {
            ExecKind::Failed
        },
        exit: Some(code),
        truncated: out.truncated,
        preview_dropped: dropped,
    })
}

/// Kills the spawned process group when dropped — the backstop for
/// future-drop cancellation where no cancel path ever runs.
struct PgGuard(Option<u32>);
impl Drop for PgGuard {
    fn drop(&mut self) {
        kill_tree(self.0);
    }
}

/// SIGKILL the child's whole process group (process_group(0) made it the
/// group leader). Single cleanup path shared by timeout and cancel.
fn kill_tree(pid: Option<u32>) {
    if let Some(pid) = pid {
        let _ = std::process::Command::new("kill")
            .args(["-9", &format!("-{pid}")])
            .status();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::future::pending;

    fn ctx() -> ToolContext {
        ToolContext {
            workspace: std::env::temp_dir(),
            bash_timeout: Duration::from_secs(30),
            bash_timeout_max: Duration::from_secs(60),
            web: None,
        }
    }

    #[tokio::test]
    async fn env_is_filtered() {
        // SAFETY: single test process; no concurrent env readers here.
        unsafe { std::env::set_var("SUI_API_KEY", "secret-marker") };
        let r = run(
            &ctx(),
            &json!({"command": "echo \"KEY=[${SUI_API_KEY:-unset}]\"; echo \"PATH=[${PATH:+set}]\""}),
            pending(),
            None,
        )
        .await
        .unwrap();
        assert!(r.text.contains("KEY=[unset]"), "secret leaked: {}", r.text);
        assert!(r.text.contains("PATH=[set]"), "PATH missing: {}", r.text);
    }

    #[tokio::test]
    async fn timeout_kills_tree() {
        let r = run(
            &ctx(),
            &json!({"command": "sleep 60", "timeout_ms": 300}),
            pending(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(r.kind, crate::tools::ExecKind::Timeout, "{}", r.text);
        assert!(r.text.contains("status: timeout"), "{}", r.text);
    }

    #[tokio::test]
    async fn cancel_kills_tree() {
        let r = run(&ctx(), &json!({"command": "sleep 60"}), async {}, None)
            .await
            .unwrap();
        assert_eq!(r.kind, crate::tools::ExecKind::Cancelled, "{}", r.text);
        assert!(r.text.contains("status: cancelled"), "{}", r.text);
    }

    /// Live chunks arrive BEFORE the process exits — real incremental
    /// observation, not post-hoc capture.
    #[tokio::test]
    async fn output_observed_before_exit() {
        let (obs, mut rx) = Observer::new(64);
        let cmd = "echo one; sleep 0.4; echo two; sleep 0.4; echo three";
        let dir = std::env::temp_dir();
        let t0 = std::time::Instant::now();
        let handle = tokio::spawn(async move {
            spawn_bounded(
                &dir,
                cmd,
                Duration::from_secs(10),
                Duration::from_secs(10),
                pending(),
                Some(obs),
            )
            .await
        });
        let first = rx.recv().await.expect("chunk before exit");
        let at = t0.elapsed();
        assert!(first.text.contains("one"), "{}", first.text);
        assert!(at < Duration::from_secs(10), "chunk only arrived at exit");
        // drain; process finishes
        let out = handle.await.unwrap().unwrap();
        assert_eq!(out.code, Some(0));
        while let Ok(c) = rx.try_recv() {
            let _ = c;
        }
    }

    /// Huge output: retained capture stays bounded, the pipe keeps
    /// draining (process exits instead of blocking on a full buffer),
    /// and preview drops are counted honestly.
    #[tokio::test]
    async fn huge_output_bounded_live() {
        let (obs, mut rx) = Observer::new(8); // tiny tap → drops counted
        let cmd = "seq 1 200000"; // ~1.4MB
        let dir = std::env::temp_dir();
        let out = tokio::time::timeout(
            Duration::from_secs(30),
            spawn_bounded(
                &dir,
                cmd,
                Duration::from_secs(25),
                Duration::from_secs(25),
                pending(),
                Some(obs),
            ),
        )
        .await
        .expect("process finished — pipes drained")
        .unwrap();
        assert_eq!(out.code, Some(0));
        assert!(out.truncated, "1.4MB must be marked truncated");
        assert!(
            out.stdout.len() < STREAM_CAP + 200,
            "capture bounded: {}",
            out.stdout.len()
        );
        // consumer never read → every preview chunk dropped or still queued
        let mut got = 0;
        while rx.try_recv().is_ok() {
            got += 1;
        }
        assert!(got <= 8, "bounded channel holds at most cap");
        assert!(out.preview_dropped > 0, "dropped previews counted");
    }

    /// Nonzero exit is a typed failure, not a string guess.
    #[tokio::test]
    async fn nonzero_exit_is_failed() {
        let r = run(
            &ctx(),
            &json!({"command": "echo oops; exit 3"}),
            pending(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(r.kind, crate::tools::ExecKind::Failed);
        assert_eq!(r.exit, Some(3));
        assert!(r.text.contains("status: failed"), "{}", r.text);
    }
}
