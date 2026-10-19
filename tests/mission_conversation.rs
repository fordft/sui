//! Native Desktop protocol regression: discussion, isolated execution and resume.
mod common;
use common::{sse_text, sse_tool_calls, submit, tc};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

struct Bridge {
    child: Child,
    input: Option<ChildStdin>,
    events: mpsc::Receiver<Value>,
}
impl Bridge {
    fn open(root: &Path, resume: Option<&str>) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sui"));
        command
            .args(["--desktop-protocol", "4", "desktop-bridge", "--workspace"])
            .arg(root.join("workspace"))
            .args(["--profile", "test"]);
        if let Some(id) = resume {
            command.args(["--resume", id]);
        }
        let mut child = command
            .env("HOME", root)
            .env("SUI_HOME", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let stdout = child.stdout.take().unwrap();
        let (tx, events) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                if tx
                    .send(serde_json::from_str(&line.unwrap()).unwrap())
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            child,
            input,
            events,
        }
    }
    fn send(&mut self, v: Value) {
        writeln!(self.input.as_mut().unwrap(), "{v}").unwrap();
    }
    fn next(&self) -> Value {
        self.events
            .recv_timeout(Duration::from_secs(30))
            .expect("native bridge event")
    }
    fn until(&self, kind: &str) -> Vec<Value> {
        let mut events = Vec::new();
        loop {
            let e = self.next();
            assert_ne!(e["type"], "error", "{e}");
            let done = e["type"] == kind;
            events.push(e);
            if done {
                return events;
            }
        }
    }
    fn close(mut self) {
        self.input.take();
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
fn fixture(port: u16) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("sui-mission-chat-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::write(root.join("config.toml"), format!("[profiles.test]\nkind=\"ollama\"\nmodel=\"mock\"\nbase_url=\"http://127.0.0.1:{port}/v1\"\n[web]\naccess=\"ask\"\n")).unwrap();
    root
}
fn git(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().into()
}

#[test]
fn mission_chat_without_git_is_read_only_even_in_yolo_and_resumes() {
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = captured.clone();
    let port =
        common::serve(move |raw, msgs| {
            let mut requests = requests.lock().unwrap();
            requests.push(serde_json::from_slice(raw).unwrap());
            if requests.len() == 1 {
                sse_tool_calls(json!([tc(
                    "forbidden",
                    "write_file",
                    r#"{"path":"forbidden.txt","content":"must not write"}"#
                )]))
            } else {
                assert!(msgs.iter().any(|m| m["role"] == "tool"
                    && m["content"].as_str().unwrap_or("").contains("denied")));
                sse_text("We can discuss before Git setup.")
            }
        });
    let root = fixture(port);
    let mut bridge = Bridge::open(&root, None);
    let hello = bridge.next();
    assert_eq!(hello["type"], "hello");
    assert_eq!(hello["mission"]["ready"], false);
    let id = hello["run_id"].as_str().unwrap().to_string();
    bridge.send(json!({"type":"set_approval","mode":"yolo"}));
    bridge.until("settings");
    bridge.send(json!({"type":"send","run":1,"mode":"mission","text":"hi"}));
    let events = bridge.until("run_done");
    assert!(!events
        .iter()
        .any(|e| e["type"] == "permission" || e["type"] == "mission_state"));
    assert_eq!(events.last().unwrap()["outcome"], "done");
    assert!(!root.join("workspace/forbidden.txt").exists());
    bridge.send(json!({"type":"run_mission","run":2,"text":"build it"}));
    let error = bridge.until("send_error").pop().unwrap();
    assert!(error["message"]
        .as_str()
        .unwrap()
        .contains("Git repository"));
    assert_eq!(captured.lock().unwrap().len(), 2);
    assert!(!root.join("workspace/.git").exists());
    bridge.close();
    let mut resumed = Bridge::open(&root, Some(&id));
    let hello = resumed.next();
    assert_eq!(hello["type"], "hello", "{hello}");
    assert_eq!(hello["conversation_mode"], "mission");
    assert!(hello["history"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"]
            .as_str()
            .unwrap_or("")
            .contains("We can discuss")));
    assert_eq!(captured.lock().unwrap().len(), 2);
    resumed.send(json!({"type":"send","run":3,"mode":"solo","text":"Switch to Solo"}));
    assert_eq!(resumed.until("run_done").last().unwrap()["outcome"], "done");
    assert!(captured.lock().unwrap().last().unwrap()["messages"]
        .to_string()
        .contains("Conversation mode is now Solo"));
    resumed.close();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mission_runs_workers_with_unique_approvals_persists_results_and_preserves_checkout() {
    let base = Arc::new(Mutex::new(String::new()));
    let plan_base = base.clone();
    let saw_context = Arc::new(Mutex::new(false));
    let saw = saw_context.clone();
    let port = common::serve(move |_, msgs| {
        let system = msgs
            .iter()
            .find(|m| m["role"] == "system")
            .and_then(|m| m["content"].as_str())
            .unwrap_or("");
        let text = msgs
            .iter()
            .filter(|m| m["role"] == "user")
            .map(|m| m["content"].as_str().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        if system.contains("control plane") {
            if text.contains("ROLE: auditor") {
                return sse_tool_calls(submit(
                    json!({"verdict":"PASS","findings":[],"required_fixes":[]}),
                ));
            }
            *saw.lock().unwrap() = text.contains("Keep the outputs tiny");
            let tasks = ["a", "b"].map(|name| json!({"id":name,"objective":format!("Create out/{name}.txt"),"owned_paths":[format!("out/{name}.txt")],"read_paths":[],"depends_on":[],"acceptance":[format!("test -f out/{name}.txt")]}));
            return sse_tool_calls(submit(
                json!({"objective":"two outputs","base_commit":*plan_base.lock().unwrap(),"tasks":tasks,"integration_checks":["test -f out/a.txt && test -f out/b.txt"]}),
            ));
        }
        // Coordinator discussion and post-run follow-up use the same journal.
        if text.contains("Runtime conversation mode") {
            return sse_text("Keep the outputs tiny. Ready when you are.");
        }
        let tools = msgs.iter().filter(|m| m["role"] == "tool").count();
        if tools == 0 {
            return sse_tool_calls(json!([tc(
                "web",
                "web_fetch",
                r#"{"url":"http://127.0.0.1/never-fetch"}"#
            )]));
        }
        if tools == 1 {
            let name = if text.contains("out/a.txt") { "a" } else { "b" };
            return sse_tool_calls(json!([tc(
                "write",
                "write_file",
                &json!({"path":format!("out/{name}.txt"),"content":"ok"}).to_string()
            )]));
        }
        sse_text("done")
    });
    let root = fixture(port);
    let repo = root.join("workspace");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    std::fs::write(repo.join("README"), "original\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "base"]);
    let original = git(&repo, &["rev-parse", "HEAD"]);
    *base.lock().unwrap() = original.clone();
    std::fs::write(repo.join("README"), "uncommitted\n").unwrap();
    let original_status = git(&repo, &["status", "--porcelain"]);
    let mut bridge = Bridge::open(&root, None);
    let hello = bridge.next();
    let id = hello["run_id"].as_str().unwrap().to_string();
    assert_eq!(hello["mission"]["ready"], true);
    bridge.send(json!({"type":"send","run":1,"mode":"mission","text":"Keep the outputs tiny"}));
    bridge.until("run_done");
    bridge.send(json!({"type":"run_mission","run":2,"text":"Create both files","max_workers":2}));
    let mut permissions = Vec::new();
    let mut events = Vec::new();
    loop {
        let e = bridge.next();
        if e["type"] == "permission" {
            permissions.push(e["id"].as_u64().unwrap());
            if permissions.len() == 2 {
                assert_ne!(permissions[0], permissions[1]);
                for id in &permissions {
                    bridge.send(json!({"type":"approve","id":id,"choice":"deny"}));
                }
            }
        }
        let done = e["type"] == "run_done";
        events.push(e);
        if done {
            break;
        }
    }
    assert_eq!(permissions.len(), 2);
    assert!(*saw_context.lock().unwrap());
    let result = events
        .iter()
        .find(|e| e["type"] == "mission_result")
        .unwrap();
    assert_eq!(result["outcome"], "accepted", "{events:#?}");
    assert_eq!(events.iter().filter(|e| e["type"] == "run_done").count(), 1);
    assert!(events.iter().any(|e| e["type"] == "task_rows"));
    assert!(events.iter().any(|e| e["type"] == "audit_result"));
    let sha = result["accepted_sha"].as_str().unwrap();
    for name in ["a", "b"] {
        assert_eq!(
            git(&repo, &["show", &format!("{sha}:out/{name}.txt")]),
            "ok"
        );
    }
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), original);
    assert_eq!(git(&repo, &["status", "--porcelain"]), original_status);
    assert!(!repo.join("out").exists());
    bridge.send(json!({"type":"send","run":3,"mode":"mission","text":"Explain that result"}));
    assert_eq!(bridge.until("run_done").last().unwrap()["outcome"], "done");
    bridge.close();
    let resumed = Bridge::open(&root, Some(&id));
    let hello = resumed.next();
    assert_eq!(hello["type"], "hello", "{hello}");
    assert_eq!(hello["conversation_mode"], "mission");
    assert!(hello["history"].to_string().contains(sha));
    resumed.close();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn mission_cancellation_and_eof_close_the_conversation_without_replaying_workers() {
    let (started_tx, started_rx) = mpsc::channel();
    let port = common::serve(move |_, msgs| {
        let planning = msgs.iter().any(|m| {
            m["role"] == "system"
                && m["content"]
                    .as_str()
                    .unwrap_or("")
                    .contains("control plane")
        });
        if planning {
            let _ = started_tx.send(());
            std::thread::sleep(Duration::from_millis(200));
        }
        sse_text("Discussion remains available")
    });
    for eof in [false, true] {
        let root = fixture(port);
        let repo = root.join("workspace");
        git(&repo, &["init", "-q"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Fixture"]);
        git(&repo, &["commit", "--allow-empty", "-qm", "base"]);
        let mut bridge = Bridge::open(&root, None);
        let hello = bridge.next();
        let id = hello["run_id"].as_str().unwrap().to_string();
        bridge.send(json!({"type":"run_mission","run":1,"text":"Wait for cancellation"}));
        started_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        if eof {
            bridge.input.take();
        } else {
            bridge.send(json!({"type":"stop"}));
        }
        let events = bridge.until("run_done");
        assert_ne!(events.last().unwrap()["outcome"], "accepted");
        assert!(events
            .iter()
            .any(|e| e["type"] == "mission_result" && e["accepted_sha"].is_null()));
        if !eof {
            bridge.send(
                json!({"type":"send","run":2,"mode":"mission","text":"Can we keep talking?"}),
            );
            assert_eq!(bridge.until("run_done").last().unwrap()["outcome"], "done");
        }
        bridge.close();
        let resumed = Bridge::open(&root, Some(&id));
        let hello = resumed.next();
        assert_eq!(hello["type"], "hello", "{hello}");
        assert!(hello["history"].to_string().contains("Mission result"));
        resumed.close();
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[tokio::test]
async fn execution_preflight_does_not_create_a_commit_in_an_empty_repository() {
    let root = fixture(1);
    let repo = root.join("workspace");
    git(&repo, &["init", "-q"]);
    std::fs::write(repo.join("important.txt"), "leave alone").unwrap();
    let before = git(&repo, &["status", "--porcelain"]);
    let error = sui::mission::worktree::preflight(&repo).await.unwrap_err();
    assert!(error.to_string().contains("at least one Git commit"));
    assert_eq!(git(&repo, &["status", "--porcelain"]), before);
    std::fs::remove_dir_all(root).unwrap();
}

fn committed_workspace(root: &Path) {
    let repo = root.join("workspace");
    git(&repo, &["init", "-q"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "user.name", "Fixture"]);
    git(&repo, &["commit", "--allow-empty", "-qm", "base"]);
}

#[test]
fn execution_keeps_early_goals_and_long_constraints_after_discussion_and_resume() {
    let captured = Arc::new(Mutex::new(Vec::<Value>::new()));
    let requests = captured.clone();
    let goal = format!(
        "REAL_GOAL: fix checkout. {}\nUSER CONSTRAINT: preserve billing",
        "details ".repeat(450)
    );
    let plan = format!(
        "{}\nCOORDINATOR CONSTRAINT: preserve authentication",
        "plan ".repeat(500)
    );
    let reply = plan.clone();
    let port = common::serve(move |raw, _| {
        let mut requests = requests.lock().unwrap();
        requests.push(serde_json::from_slice(raw).unwrap());
        sse_text(if requests.len() == 1 {
            &reply
        } else {
            "Clarification recorded"
        })
    });
    let root = fixture(port);
    committed_workspace(&root);
    let mut bridge = Bridge::open(&root, None);
    let hello = bridge.next();
    let id = hello["run_id"].as_str().unwrap().to_string();
    for run in 1..=8 {
        bridge.send(json!({"type":"send","run":run,"mode":"mission","text":if run == 1 { &goal } else { "Clarify the tests" }}));
        bridge.until("run_done");
    }
    bridge.close();
    let mut resumed = Bridge::open(&root, Some(&id));
    assert_eq!(resumed.next()["type"], "hello");
    assert_eq!(
        captured.lock().unwrap().len(),
        8,
        "resume never calls the model"
    );
    resumed.send(json!({"type":"run_mission","run":9,"text":"Proceed"}));
    resumed.until("run_done");
    let requests = captured.lock().unwrap();
    let prompt = requests.last().unwrap()["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["content"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        prompt.contains(&goal),
        "all original user constraints reach execution"
    );
    assert!(
        prompt.contains(&plan),
        "coordinator decisions are not truncated at 2000 bytes"
    );
    drop(requests);
    resumed.close();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn oversized_discussion_stops_before_planning_and_stays_blocked_after_resume() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let requests = Arc::new(AtomicUsize::new(0));
    let count = requests.clone();
    let port = common::serve(move |_, _| {
        count.fetch_add(1, Ordering::SeqCst);
        sse_text("Discussed")
    });
    let root = fixture(port);
    committed_workspace(&root);
    let mut bridge = Bridge::open(&root, None);
    let hello = bridge.next();
    let id = hello["run_id"].as_str().unwrap().to_string();
    for run in 1..=3 {
        bridge.send(json!({"type":"send","run":run,"mode":"mission","text":"x".repeat(60_000)}));
        bridge.until("run_done");
    }
    for resumed in [false, true] {
        if resumed {
            bridge.close();
            bridge = Bridge::open(&root, Some(&id));
            assert_eq!(bridge.next()["type"], "hello");
        }
        bridge.send(json!({"type":"run_mission","run":4,"text":"Proceed"}));
        let events = bridge.until("run_done");
        assert!(events.last().unwrap()["outcome"]
            .as_str()
            .unwrap()
            .contains("No workers were started"));
        assert!(!events.iter().any(|e| e["type"] == "mission_state"));
        assert_eq!(
            requests.load(Ordering::SeqCst),
            3,
            "no planner or workers may run with dropped constraints"
        );
    }
    bridge.close();
    std::fs::remove_dir_all(root).unwrap();
}
