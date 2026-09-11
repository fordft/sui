use anyhow::Result;
use serde_json::Value;
use std::future::Future;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio::time::timeout;

use super::ToolContext;

/// Max chars of a stream that may enter model-visible history.
/// Over the limit: keep head 60% + tail 40% with an omission marker.
const STREAM_CAP: usize = 30_000;

/// Explicit child-environment allowlist. The child does NOT inherit the
/// parent environment — credentials and unrelated secrets never leak in.
/// (Still not a sandbox: the process runs with full user privileges.)
const ENV_ALLOW: &[&str] = &[
    "PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "LC_CTYPE",
    "TERM", "TMPDIR", "TZ", "RUSTUP_HOME", "CARGO_HOME", "GOPATH", "GOROOT",
    "NVM_DIR", "PYENV_ROOT", "XDG_CACHE_HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME",
];

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
}

/// Spawn `bash -c <cmd>` in `dir` with filtered env, own process group,
/// bounded output, hard timeout, and a unified kill+reap path shared by
/// timeout and cancellation.
pub async fn spawn_bounded(
    dir: &Path,
    cmd: &str,
    dur: Duration,
    dur_max: Duration,
    cancel: impl Future<Output = ()>,
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
    let mut so = child.stdout.take().expect("piped stdout");
    let mut se = child.stderr.take().expect("piped stderr");
    let out_h = tokio::spawn(async move {
        let mut v = Vec::new();
        let _ = so.read_to_end(&mut v).await;
        v
    });
    let err_h = tokio::spawn(async move {
        let mut v = Vec::new();
        let _ = se.read_to_end(&mut v).await;
        v
    });

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
    let stdout = out_h.await.unwrap_or_default();
    let stderr = err_h.await.unwrap_or_default();

    let (stdout, t1) = bound(&String::from_utf8_lossy(&stdout));
    let (stderr, t2) = bound(&String::from_utf8_lossy(&stderr));
    Ok(ProcOut {
        code: status.and_then(|s| s.ok()).and_then(|s| s.code()),
        stdout,
        stderr,
        truncated: t1 || t2,
        timed_out,
        cancelled,
    })
}

pub async fn run(
    ctx: &ToolContext,
    args: &Value,
    cancel: impl Future<Output = ()>,
) -> Result<String> {
    let cmd = args["command"].as_str().unwrap_or("").to_string();
    if cmd.trim().is_empty() {
        return Ok("status: error\nerror: empty command".into());
    }
    let req_ms = args["timeout_ms"].as_u64().unwrap_or(ctx.bash_timeout.as_millis() as u64);
    let dur = Duration::from_millis(req_ms);
    let out = spawn_bounded(&ctx.workspace, &cmd, dur, ctx.bash_timeout_max, cancel).await?;

    if out.cancelled {
        return Ok("status: cancelled\nerror: interrupted by user".into());
    }
    if out.timed_out {
        return Ok(format!(
            "status: timeout\nerror: exceeded {}ms\nhint: rerun bounded or pass larger timeout_ms",
            dur.as_millis()
        ));
    }
    let code = out.code.unwrap_or(-1);
    Ok(format!(
        "status: {}\nexit_code: {}\nstdout: {}\nstderr: {}\ntruncated: {}",
        if code == 0 { "success" } else { "failed" },
        code,
        if out.stdout.is_empty() { "<empty>".into() } else { out.stdout },
        if out.stderr.is_empty() { "<empty>".into() } else { out.stderr },
        out.truncated
    ))
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

fn bound(s: &str) -> (String, bool) {
    if s.len() <= STREAM_CAP {
        return (s.to_string(), false);
    }
    let head = STREAM_CAP * 6 / 10;
    let tail = STREAM_CAP - head;
    let omitted = s.len() - head - tail;
    (
        format!(
            "{}\n... <{omitted} bytes omitted> ...\n{}",
            &s[..floor_char(s, head)],
            &s[floor_char(s, s.len() - tail)..]
        ),
        true,
    )
}

fn floor_char(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
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
        )
        .await
        .unwrap();
        assert!(r.contains("KEY=[unset]"), "secret leaked: {r}");
        assert!(r.contains("PATH=[set]"), "PATH missing: {r}");
    }

    #[tokio::test]
    async fn timeout_kills_tree() {
        let r = run(
            &ctx(),
            &json!({"command": "sleep 60", "timeout_ms": 300}),
            pending(),
        )
        .await
        .unwrap();
        assert!(r.contains("status: timeout"), "{r}");
    }

    #[tokio::test]
    async fn cancel_kills_tree() {
        let r = run(&ctx(), &json!({"command": "sleep 60"}), async {}).await.unwrap();
        assert!(r.contains("status: cancelled"), "{r}");
    }
}
