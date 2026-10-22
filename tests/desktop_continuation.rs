mod common;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::{Arc, Mutex};

struct Bridge {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    hello: Value,
}
impl Bridge {
    fn open(root: &Path, profile: &str, args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_sui"))
            .args(["--desktop-protocol", "5", "desktop-bridge", "--workspace"])
            .arg(root.join("workspace"))
            .args(["--profile", profile])
            .args(args)
            .env("HOME", root)
            .env("SUI_HOME", root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        let hello = serde_json::from_str(&line).unwrap();
        Self {
            child,
            input,
            output,
            hello,
        }
    }
    fn command(&mut self, value: Value) {
        writeln!(self.input.as_mut().unwrap(), "{value}").unwrap();
    }
    fn until(&mut self, kind: &str) -> Value {
        loop {
            let mut line = String::new();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "bridge exited waiting for {kind}"
            );
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["type"] == kind {
                return value;
            }
            assert_ne!(
                value["type"], "permission",
                "continuation must not replay any tools"
            );
            assert_ne!(value["type"], "error", "{value}");
        }
    }
    fn close(mut self) {
        self.input.take();
        assert!(self.child.wait().unwrap().success());
    }
}
impl Drop for Bridge {
    fn drop(&mut self) {
        self.input.take();
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn model_continuation_is_portable_resumable_and_preserves_original_constraints() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let observed = requests.clone();
    let port = common::serve(move |body, _| {
        observed.lock().unwrap().push(body.to_vec());
        common::sse_text("Decision: keep the original constraints and verify changes.")
    });
    let root = std::env::temp_dir().join(format!("sui-handoff-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::write(root.join("config.toml"), format!("[profiles.first]\nkind=\"ollama\"\nmodel=\"model-one\"\nbase_url=\"http://127.0.0.1:{port}/v1\"\n[profiles.second]\nkind=\"ollama\"\nmodel=\"model-two\"\nbase_url=\"http://127.0.0.1:{port}/v1\"\n")).unwrap();
    let mut first = Bridge::open(&root, "first", &[]);
    first.command(json!({"type":"send","run":1,"mode":"mission","text":"Build the Thai storefront. Preserve accessibility and do not change billing."}));
    assert_eq!(first.until("run_done")["outcome"], "done");
    let source = first.hello["run_id"].as_str().unwrap().to_string();
    first.close();
    let source_file = root
        .join(".local/share/sui/runs")
        .join(&source)
        .join("solo.jsonl");
    let before = std::fs::read(&source_file).unwrap();
    let mut second = Bridge::open(&root, "second", &["--continue-from", &source]);
    assert_eq!(second.hello["model"], "model-two");
    assert_eq!(second.hello["approval"], "ask");
    assert_eq!(second.hello["conversation_mode"], "mission");
    assert_eq!(second.hello["continuation"]["source"], source);
    assert!(second.hello["history"]
        .to_string()
        .contains("do not change billing"));
    assert_eq!(
        requests.lock().unwrap().len(),
        1,
        "switching does not call the provider"
    );
    second.command(
        json!({"type":"review_mission","request":5,"directory":"/tmp","file":null,"agent":null}),
    );
    assert!(second.until("mission_review")["error"]
        .as_str()
        .unwrap()
        .contains("this conversation"));
    // A handoff with no subsequent turn is meaningful and resumable.
    let continued = second.hello["run_id"].as_str().unwrap().to_string();
    second.close();
    let mut resumed = Bridge::open(&root, "second", &["--resume", &continued]);
    assert_eq!(resumed.hello["conversation_mode"], "mission");
    resumed.command(json!({"type":"send","run":1,"mode":"mission","text":"Continue the plan"}));
    assert_eq!(resumed.until("run_done")["outcome"], "done");
    let next = resumed.hello["run_id"].as_str().unwrap().to_string();
    resumed.close();
    let observed = requests.lock().unwrap();
    assert_eq!(observed.len(), 2);
    let request = String::from_utf8_lossy(&observed[1]);
    assert!(request.contains("model-two"));
    assert!(request.contains("do not change billing"));
    assert!(request.contains("historical claims"));
    assert!(request.contains("Continue the plan"));
    drop(observed);
    let third = Bridge::open(&root, "first", &["--continue-from", &next]);
    let entries = third.hello["continuation"]["entries"].as_array().unwrap();
    assert_eq!(
        entries
            .iter()
            .filter(|e| e["content"]
                .as_str()
                .unwrap()
                .contains("Build the Thai storefront"))
            .count(),
        1,
        "chained switches do not nest or duplicate context"
    );
    third.close();
    assert_eq!(
        std::fs::read(source_file).unwrap(),
        before,
        "original evidence is immutable"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn portable_context_excludes_private_and_executable_records() {
    let events = vec![
        json!({"type":"task","data":{"task":"Keep all constraints","mode":"solo"}}),
        json!({"type":"user","data":{"content":"duplicate task envelope"}}),
        json!({"type":"assistant","data":{"content":"Read the config","reasoning_content":"private-reasoning","response_items":[{"encrypted_content":"private-wire-state"}],"tool_calls":[{"id":"old-call"}]}}),
        json!({"type":"tool","data":{"result":"raw-tool-output","args":{"command":"never execute"}}}),
        json!({"type":"permission","data":{"choice":"yolo"}}),
    ];
    let text = serde_json::to_string(&sui::continuation::entries(&events)).unwrap();
    assert!(text.contains("Keep all constraints"));
    assert!(text.contains("Read the config"));
    for excluded in [
        "duplicate task envelope",
        "private-reasoning",
        "private-wire-state",
        "raw-tool-output",
        "old-call",
        "never execute",
        "yolo",
    ] {
        assert!(!text.contains(excluded));
    }
}

#[test]
fn oversized_continuation_is_rejected_without_truncating_the_source() {
    let reply = "ก".repeat(30_000);
    let port = common::serve(move |_, _| common::sse_text(&reply));
    let root =
        std::env::temp_dir().join(format!("sui-handoff-limit-{:016x}", rand::random::<u64>()));
    std::fs::create_dir_all(root.join("workspace")).unwrap();
    std::fs::write(root.join("config.toml"), format!("[profiles.test]\nkind=\"ollama\"\nmodel=\"test\"\nbase_url=\"http://127.0.0.1:{port}/v1\"\n")).unwrap();
    let mut source = Bridge::open(&root, "test", &[]);
    source.command(json!({"type":"send","run":1,"text":"constraint ".repeat(5000)}));
    assert_eq!(source.until("run_done")["outcome"], "done");
    let id = source.hello["run_id"].as_str().unwrap().to_string();
    source.close();
    let path = root
        .join(".local/share/sui/runs")
        .join(&id)
        .join("solo.jsonl");
    let before = std::fs::read(&path).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_sui"))
        .args(["desktop-bridge", "--workspace"])
        .arg(root.join("workspace"))
        .args(["--profile", "test", "--continue-from", &id])
        .env("HOME", &root)
        .env("SUI_HOME", &root)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("128 KiB continuation limit"));
    assert!(result.stdout.is_empty());
    assert_eq!(std::fs::read(path).unwrap(), before);
    std::fs::remove_dir_all(root).unwrap();
}
