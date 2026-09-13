//! PTY regression: run the real `sui tui` binary in a pseudo-terminal,
//! drive the permission modal with raw keystrokes (no newlines), and
//! verify tool gating end to end.
//!
//! Regression target: shortcut keys must decide the modal on a single
//! byte — no Enter — and a session approval must release the gate for
//! every later protected tool without another prompt.

use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn tc(id: &str, name: &str, args: &str) -> Value {
    json!({"id": id, "type": "function",
           "function": {"name": name, "arguments": args}})
}

fn sse(payload: Value) -> String {
    let u = json!({"choices": [], "usage": {"prompt_tokens": 5,
        "completion_tokens": 2}});
    format!("data: {payload}\n\ndata: {u}\n\ndata: [DONE]\n\n")
}

fn write_file_call(id: &str, path: &str) -> String {
    sse(
        json!({"choices": [{"index": 0, "delta": {"role": "assistant",
        "tool_calls": [tc(id, "write_file",
            &json!({"path": path, "content": "wrote"}).to_string())]},
        "finish_reason": "tool_calls"}]}),
    )
}

fn text_done() -> String {
    sse(
        json!({"choices": [{"index": 0, "delta": {"role": "assistant",
        "content": "done"}, "finish_reason": "stop"}]}),
    )
}

/// Two protected calls in sequence: request → write_file(perm1); after a
/// successful tool result → write_file(perm2); after a denied result or
/// two tool results → text. Any refusal must come back as a tool message
/// containing "denied" (the gate's denial result text).
fn mock() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let mut s = match conn {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if line.trim().is_empty() {
                    break;
                }
                if line.trim().to_lowercase().starts_with("content-length:") {
                    len = line.trim()[15..].trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            if r.read_exact(&mut body).is_err() {
                continue;
            }
            let req: Value = serde_json::from_slice(&body).unwrap_or_default();
            let msgs = req["messages"].as_array().cloned().unwrap_or_default();
            let tools: Vec<&Value> = msgs.iter().filter(|m| m["role"] == "tool").collect();
            let last_tool = tools
                .last()
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            let body = if last_tool.contains("denied") || tools.len() >= 2 {
                text_done()
            } else if tools.len() == 1 {
                write_file_call("w2", "out/perm2.txt")
            } else {
                write_file_call("w1", "out/perm1.txt")
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            if s.write_all(resp.as_bytes()).is_err() {
                continue;
            }
            let _ = s.flush();
        }
    });
    port
}

fn fixture() -> (PathBuf, PathBuf) {
    let tag = format!(
        "sui-pty-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let repo = std::env::temp_dir().join(format!("{tag}-repo"));
    let home = std::env::temp_dir().join(format!("{tag}-home"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::create_dir_all(home.join(".config/sui")).unwrap();
    let git = |a: &[&str]| {
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(a)
            .output()
            .unwrap();
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.join("README.md"), "# fixture\n").unwrap();
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
    (repo, home)
}

struct Pty {
    buf: Arc<Mutex<Vec<u8>>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

/// Spawn `sui tui` under a real pty against the mock provider.
fn spawn(port: u16, repo: &PathBuf, home: &PathBuf) -> Pty {
    std::fs::write(
        home.join(".config/sui/config.toml"),
        format!(
            "[profiles.mock]\nbase_url = \"http://127.0.0.1:{port}\"\nmodel = \"mock-model\"\n\n[ui]\nworkspace = \"{}\"\nmode = \"solo\"\nsolo_profile = \"mock\"\n",
            repo.display()
        ),
    )
    .unwrap();
    let pair = NativePtySystem::default()
        .openpty(PtySize {
            rows: 40,
            cols: 140,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_sui"));
    cmd.arg("tui");
    cmd.cwd(repo);
    cmd.env("HOME", home);
    cmd.env("TERM", "xterm-256color");
    let child = pair.slave.spawn_command(cmd).unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let writer = pair.master.take_writer().unwrap();
    let buf = Arc::new(Mutex::new(Vec::new()));
    let sink = buf.clone();
    std::thread::spawn(move || {
        let mut chunk = [0u8; 8192];
        while let Ok(n) = reader.read(&mut chunk) {
            if n == 0 {
                break;
            }
            sink.lock().unwrap().extend_from_slice(&chunk[..n]);
        }
    });
    Pty { buf, writer, child }
}

fn count(buf: &Arc<Mutex<Vec<u8>>>, pat: &str) -> usize {
    let b = buf.lock().unwrap();
    let s = String::from_utf8_lossy(&b);
    s.matches(pat).count()
}

fn wait_for<F: Fn() -> bool>(what: &str, timeout: Duration, f: F) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn send(p: &mut Pty, bytes: &[u8]) {
    p.writer.write_all(bytes).unwrap();
    p.writer.flush().unwrap();
}

/// Wait for the first full paint, send the task, wait for the modal.
fn to_modal(p: &mut Pty) {
    wait_for("initial paint", Duration::from_secs(10), || {
        count(&p.buf, "Enter") > 0
    });
    send(p, b"do it\r");
    wait_for("permission modal", Duration::from_secs(15), || {
        count(&p.buf, "deny") > 0
    });
}

/// 'a' (lowercase) must approve the session on a single byte — no Enter —
/// and the next protected tool must run without another prompt.
#[test]
fn pty_a_session_single_byte() {
    let (repo, home) = fixture();
    let port = mock();
    let mut p = spawn(port, &repo, &home);
    to_modal(&mut p);
    send(&mut p, b"a");
    let f1 = repo.join("out/perm1.txt");
    let f2 = repo.join("out/perm2.txt");
    wait_for("first file", Duration::from_secs(10), || f1.exists());
    // session approval: second protected call runs unprompted — nothing
    // else is sent, so a second modal would deadlock this wait
    wait_for("second file (unprompted)", Duration::from_secs(15), || {
        f2.exists()
    });
    wait_for("AUTO badge", Duration::from_secs(5), || {
        count(&p.buf, "AUTO") > 0
    });
    let _ = p.child.kill();
    let _ = p.child.wait();
}

/// Same for uppercase 'A'.
#[test]
fn pty_upper_a_session_single_byte() {
    let (repo, home) = fixture();
    let port = mock();
    let mut p = spawn(port, &repo, &home);
    to_modal(&mut p);
    send(&mut p, b"A");
    wait_for("first file", Duration::from_secs(10), || {
        repo.join("out/perm1.txt").exists()
    });
    wait_for("second file (unprompted)", Duration::from_secs(15), || {
        repo.join("out/perm2.txt").exists()
    });
    let _ = p.child.kill();
    let _ = p.child.wait();
}

/// 'Y' must approve only the current action: the next protected tool
/// prompts again; a follow-up 'y' then completes the run.
#[test]
fn pty_y_once_still_prompts() {
    let (repo, home) = fixture();
    let port = mock();
    let mut p = spawn(port, &repo, &home);
    to_modal(&mut p);
    send(&mut p, b"Y");
    let f1 = repo.join("out/perm1.txt");
    let f2 = repo.join("out/perm2.txt");
    wait_for("first file", Duration::from_secs(10), || f1.exists());
    // once-only: second write must NOT happen on its own
    std::thread::sleep(Duration::from_secs(2));
    assert!(!f2.exists(), "once-approval leaked into session approval");
    // the second modal is open; one more 'y' finishes the run
    send(&mut p, b"y");
    wait_for("second file", Duration::from_secs(10), || f2.exists());
    let _ = p.child.kill();
    let _ = p.child.wait();
}

/// 'n' denies immediately; the run ends with no file written.
#[test]
fn pty_n_denies() {
    let (repo, home) = fixture();
    let port = mock();
    let mut p = spawn(port, &repo, &home);
    to_modal(&mut p);
    send(&mut p, b"n");
    wait_for("run to settle", Duration::from_secs(10), || {
        count(&p.buf, "run finished") > 0
            || !p.buf.lock().unwrap().is_empty() && count(&p.buf, "denied") > 0
    });
    assert!(!repo.join("out/perm1.txt").exists());
    let _ = p.child.kill();
    let _ = p.child.wait();
}

/// Ctrl+S keeps working while the permission modal is parked: the gate
/// unblocks, the tool is denied, the run ends.
#[test]
fn pty_ctrl_s_stops_during_permission() {
    let (repo, home) = fixture();
    let port = mock();
    let mut p = spawn(port, &repo, &home);
    to_modal(&mut p);
    send(&mut p, b"\x13"); // Ctrl+S
    wait_for("stop to settle the run", Duration::from_secs(10), || {
        count(&p.buf, "run finished") > 0
    });
    if repo.join("out/perm1.txt").exists() {
        // forensic: dump the transcript tail — an approve path would be
        // visible as the decision or the tool result text
        let b = p.buf.lock().unwrap();
        let s = String::from_utf8_lossy(&b);
        panic!(
            "perm1.txt written despite stop — transcript tail:\n{}",
            &s[s.len().saturating_sub(3000)..]
        );
    }
    let _ = p.child.kill();
    let _ = p.child.wait();
}

/// One physical keypress, one approval: a Press+Release pair (kitty-style
/// CSI-u events) must not approve two consecutive prompts.
#[test]
fn pty_release_never_approves_next_modal() {
    let (repo, home) = fixture();
    let port = mock();
    let mut p = spawn(port, &repo, &home);
    to_modal(&mut p);

    // 'y' Press via CSI-u (kitty protocol) — approves tool 1
    send(&mut p, b"\x1b[121;1:1u");
    let f1 = repo.join("out/perm1.txt");
    let f2 = repo.join("out/perm2.txt");
    wait_for("first file", Duration::from_secs(10), || f1.exists());

    // tool 2's gate parks with its modal open (identical cells render a
    // zero-diff paint, so we can't count the text — give it time), then
    // the Release tail of the same keypress arrives — it must be ignored
    std::thread::sleep(Duration::from_millis(1500));
    send(&mut p, b"\x1b[121;1:3u");
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        !f2.exists(),
        "Release tail of 'y' approved the second prompt"
    );

    // a fresh Press still decides the open modal
    send(&mut p, b"y");
    wait_for("second file", Duration::from_secs(10), || f2.exists());
    let _ = p.child.kill();
    let _ = p.child.wait();
}
