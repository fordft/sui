//! Code-location proof through the production dispatcher and native wire loop.
mod common;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sui::tools::{self, ExecKind, ExecOut, ToolContext};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-inventory-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
    fn write(&self, path: &str, content: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }
    fn ctx(&self) -> ToolContext {
        context(&self.0)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn context(path: &Path) -> ToolContext {
    ToolContext {
        workspace: path.to_path_buf(),
        bash_timeout: Duration::from_secs(1),
        bash_timeout_max: Duration::from_secs(1),
        web: None,
        canon_root: Default::default(),
        ui: Default::default(),
    }
}
async fn inventory(ctx: &ToolContext, args: Value) -> ExecOut {
    tools::execute(ctx, "inventory", &args, std::future::pending(), None)
        .await
        .unwrap()
}

#[tokio::test]
async fn syntax_locations_cover_languages_without_comment_or_string_matches() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "// fn Pretend() {}\nconst EXAMPLE: &str = \"fn Fake() {}\";\npub struct Engine;\npub async fn launch(\n    engine: Engine,\n) {}\ntrait Service { fn run(&self); }\n");
    fixture.write("web/main.ts", "// function Pretend() {}\nexport interface Engine {}\nexport type State = string;\nexport const launch = (x: State) => x;\n");
    fixture.write(
        "web/view.tsx",
        "export const View = () => <button>go</button>;\n",
    );
    fixture.write("web/main.js", "const text = 'function Pretend() {}';\nexport class Engine { run() {} }\nfunction launch() {}\n");
    fixture.write("script.py", "text = 'def Pretend(): pass'\nclass Engine:\n    def run(self):\n        pass\n\ndef launch():\n    pass\n");
    fixture.write(
        "main.go",
        "package main\ntype Engine struct {}\nfunc launch() {}\nfunc (e Engine) run() {}\n",
    );
    let out = inventory(&fixture.ctx(), json!({"action":"symbols","limit":200})).await;
    assert_eq!(out.kind, ExecKind::Success);
    assert!(!out.truncated, "{}", out.text);
    for expected in [
        "src/lib.rs:3-3 struct Engine",
        "src/lib.rs:4-6 function launch",
        "web/main.ts:2-2 interface Engine",
        "web/main.ts:3-3 type State",
        "web/main.ts:4-4 function launch",
        "web/view.tsx:1-1 function View",
        "web/main.js:2-2 class Engine",
        "web/main.js:2-2 method run",
        "script.py:2-4 class Engine",
        "script.py:6-7 function launch",
        "main.go:2-2 type Engine",
        "main.go:3-3 function launch",
        "main.go:4-4 method run",
    ] {
        assert!(
            out.text.contains(expected),
            "missing {expected}: {}",
            out.text
        );
    }
    assert!(!out.text.contains("Pretend"));
    assert!(!out.text.contains("Fake"));
    let again = inventory(&fixture.ctx(), json!({"action":"symbols","limit":200})).await;
    assert_eq!(
        out.text, again.text,
        "unchanged files give deterministic output"
    );
    let query = inventory(
        &fixture.ctx(),
        json!({"action":"symbols","query":"LAUNCH","path":"web"}),
    )
    .await;
    assert!(query.text.contains("web/main.ts:4-4 function launch"));
    assert!(!query.text.contains("src/lib.rs:"));
    assert!(!query.text.contains("class Engine"));
}

#[tokio::test]
async fn files_respect_nested_ignores_generated_directories_and_symlinks() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "ignored.rs\nprivate/\n");
    fixture.write("src/.gitignore", "nested.rs\n");
    fixture.write("src/visible.rs", "fn visible() {}\n");
    fixture.write("src/nested.rs", "fn hidden() {}\n");
    fixture.write("ignored.rs", "fn hidden() {}\n");
    fixture.write("private/secret.rs", "fn hidden() {}\n");
    fixture.write("target/generated.rs", "fn hidden() {}\n");
    fixture.write("node_modules/package/index.js", "function hidden() {}\n");
    fixture.write(".env.rs", "fn hidden() {}\n");
    fixture.write(".github/workflows/ci.yml", "name: CI\n");
    fixture.write("notes.txt", "manual\n");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/passwd", fixture.0.join("escape.rs")).unwrap();
        std::os::unix::fs::symlink("src", fixture.0.join("alias")).unwrap();
    }
    let out = inventory(&fixture.ctx(), json!({"action":"files"})).await;
    assert!(out.text.contains("src/visible.rs [rust]"));
    assert!(out.text.contains("notes.txt [file]"));
    assert!(out.text.contains(".github/workflows/ci.yml [file]"));
    for hidden in [
        "nested.rs",
        "ignored.rs",
        "secret.rs",
        "generated.rs",
        "index.js",
        ".env.rs",
        "escape.rs",
        "alias/",
    ] {
        assert!(!out.text.contains(hidden), "leaked {hidden}: {}", out.text);
    }
    let scoped = inventory(&fixture.ctx(), json!({"action":"files","path":"src"})).await;
    assert!(scoped.text.contains("src/visible.rs [rust]"));
    assert!(!scoped.text.contains("nested.rs"));
    fixture.write(".gitignore", "ignored.rs\nprivate/\nsrc/visible.rs\n");
    let refreshed = inventory(&fixture.ctx(), json!({"action":"files"})).await;
    assert!(
        !refreshed.text.contains("src/visible.rs"),
        "ignore changes must be observed"
    );
}

#[tokio::test]
async fn current_content_and_limits_never_claim_an_exhaustive_missing_symbol() {
    let fixture = Fixture::new();
    fixture.write("current.rs", "fn before() {}\nfn second() {}\n");
    let ctx = fixture.ctx();
    let before = inventory(&ctx, json!({"action":"symbols"})).await;
    assert!(before.text.contains("function before"));
    fixture.write("current.rs", "// moved\nfn after() {}\n");
    fixture.write("added.py", "def added():\n    pass\n");
    let after = inventory(&ctx, json!({"action":"symbols"})).await;
    assert!(after.text.contains("current.rs:2-2 function after"));
    assert!(after.text.contains("function added"));
    assert!(!after.text.contains("function before"));
    std::fs::remove_file(fixture.0.join("current.rs")).unwrap();
    let removed = inventory(&ctx, json!({"action":"symbols"})).await;
    assert!(!removed.text.contains("current.rs:"));
    fixture.write("broken.rs", "fn broken(\n");
    fixture.write("oversize.rs", &" ".repeat(512 * 1024 + 1));
    fixture.write("binary.rs", "\0binary");
    fixture.write("unknown.c", "void unsupported() {}\n");
    let partial = inventory(&ctx, json!({"action":"symbols","limit":1})).await;
    assert!(partial.truncated);
    assert!(partial.text.contains("scan_complete: false"));
    assert!(partial.text.contains("syntax_error_files: 1"));
    assert!(partial.text.contains("files_skipped: 2"));
    assert!(partial.text.contains("files_unsupported: 1"));
    assert!(partial
        .text
        .contains("partial results do not prove absence"));
    let cancelled = tools::execute(
        &ctx,
        "inventory",
        &json!({"action":"symbols"}),
        std::future::ready(()),
        None,
    )
    .await
    .unwrap();
    assert_eq!(cancelled.kind, ExecKind::Cancelled);
    for args in [
        json!({}),
        json!({"action":"references"}),
        json!({"action":"files","limit":0}),
        json!({"action":"files","limit":201}),
        json!({"action":"files","path":123}),
        json!({"action":"files","query":"x".repeat(257)}),
    ] {
        let error = inventory(&ctx, args).await;
        assert_eq!(error.kind, ExecKind::Error);
    }
    assert!(tools::execute(
        &ctx,
        "inventory",
        &json!({"action":"files","path":"../"}),
        std::future::pending(),
        None
    )
    .await
    .is_err());
}

#[tokio::test]
async fn output_limits_report_omitted_definitions_without_returning_bodies() {
    let fixture = Fixture::new();
    let source: String = (0..205)
        .map(|n| format!("fn symbol_{n:03}() {{ /* private-body */ }}\n"))
        .collect();
    fixture.write("many.rs", &source);
    let limited = inventory(&fixture.ctx(), json!({"action":"symbols","limit":1})).await;
    assert!(limited.truncated);
    assert!(limited.text.contains("scan_complete: true"));
    assert!(limited.text.contains("matches_seen: 205"));
    assert!(limited.text.contains("showing: 1"));
    assert!(!limited.text.contains("symbol_001"));
    assert!(!limited.text.contains("private-body"));
    let source: String = (0..200)
        .map(|n| format!("fn symbol_{n:03}_{}() {{}}\n", "x".repeat(210)))
        .collect();
    fixture.write("many.rs", &source);
    let bounded = inventory(&fixture.ctx(), json!({"action":"symbols","limit":200})).await;
    assert!(bounded.truncated);
    assert!(bounded.text.contains("matches_seen: 200"));
    assert!(bounded.text.len() < 25 * 1024);
    fixture.write("unknown.c", "void unknown() {}\n");
    assert_eq!(
        inventory(
            &fixture.ctx(),
            json!({"action":"symbols","path":"unknown.c"})
        )
        .await
        .kind,
        ExecKind::Error
    );
}

#[test]
fn older_tool_signature_is_rejected_before_request_or_journal_fork() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let fixture = Fixture::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let count = calls.clone();
    let port = common::serve(move |_, _| {
        count.fetch_add(1, Ordering::Relaxed);
        common::sse_text("fixture answer")
    });
    fixture.write(
        "workspace/config.toml",
        &format!("[provider]\nbase_url='http://127.0.0.1:{port}/v1'\nmodel='mock'\n"),
    );
    let workspace = fixture.0.join("workspace");
    let task_home = fixture.0.join("task-home");
    std::fs::create_dir_all(&task_home).unwrap();
    let invoke = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_sui"))
            .current_dir(&workspace)
            .env("HOME", &task_home)
            .env_remove("SUI_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .args(["--config", "config.toml"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
    };
    assert!(invoke(&["start fixture"]).status.success());
    let runs = task_home.join(".local/share/sui/runs");
    let rows = sui::session::recent(&runs, &workspace, None).unwrap();
    assert_eq!(rows.len(), 1);
    let source = runs.join(&rows[0].id).join("headless.jsonl");
    let old: String = std::fs::read_to_string(&source)
        .unwrap()
        .lines()
        .map(|line| {
            let mut event: Value = serde_json::from_str(line).unwrap();
            if event["type"] == "resume_header" {
                // Captured from released v0.5.0: the same original ten schemas.
                event["data"]["signature"]["tools"] =
                    json!("8f670344f77bcd258b8aa6d8108380c2bcbd713d7b688ed276375e7ec7794e80");
            }
            format!("{event}\n")
        })
        .collect();
    std::fs::write(&source, &old).unwrap();
    let result = invoke(&["--resume", &rows[0].id, "continue"]);
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("tools or project guidance changed"));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(std::fs::read_to_string(&source).unwrap(), old);
    // Configuration allocates an empty run directory before signature checks;
    // rejected recovery must not fork any historical journal into it.
    assert_eq!(
        sui::session::recent(&runs, &workspace, None).unwrap().len(),
        1
    );
    for entry in std::fs::read_dir(runs).unwrap() {
        let path = entry.unwrap().path();
        if path.file_name().unwrap() != rows[0].id.as_str() {
            assert_eq!(std::fs::read_dir(path).unwrap().count(), 0);
        }
    }
}

fn git(path: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .current_dir(path)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[tokio::test]
async fn worktree_inventory_isolated_and_checkout_untouched() {
    let fixture = Fixture::new();
    fixture.write("repo/lib.rs", "fn original() {}\n");
    let repo = fixture.0.join("repo");
    git(&repo, &["init", "-q"]);
    git(&repo, &["add", "lib.rs"]);
    git(
        &repo,
        &[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    );
    let worker = fixture.0.join("worker");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "--detach",
            worker.to_str().unwrap(),
            "HEAD",
        ],
    );
    std::fs::write(worker.join("lib.rs"), "fn worker_only() {}\n").unwrap();
    let original = inventory(&context(&repo), json!({"action":"symbols"})).await;
    let result = inventory(&context(&worker), json!({"action":"symbols"})).await;
    assert!(original.text.contains("function original"));
    assert!(!original.text.contains("worker_only"));
    assert!(result.text.contains("function worker_only"));
    assert!(!result.text.contains("function original"));
    assert_eq!(
        std::fs::read_to_string(repo.join("lib.rs")).unwrap(),
        "fn original() {}\n"
    );
    let status = std::process::Command::new("git")
        .current_dir(&repo)
        .args(["status", "--porcelain"])
        .output()
        .unwrap();
    assert!(status.status.success());
    assert!(status.stdout.is_empty());
}

#[tokio::test]
async fn native_agent_discovers_locations_then_reads_with_frozen_headers() {
    let fixture = Fixture::new();
    fixture.write(
        "workspace/src/service.rs",
        "// bounded fixture\npub fn checkout() -> u32 {\n    42\n}\n",
    );
    let workspace = fixture.0.join("workspace");
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let saved = requests.clone();
    let port = common::serve(move |body, _| {
        let mut saved = saved.lock().unwrap();
        saved.push(serde_json::from_slice(body).unwrap());
        match saved.len() {
            1 => common::sse_tool_calls(json!([common::tc("locate", "inventory", r#"{"action":"symbols","query":"checkout"}"#)])),
            2 => common::sse_tool_calls(json!([common::tc("read", "read_file", r#"{"path":"src/service.rs","offset":2,"limit":3}"#)])),
            _ => common::sse_text("checkout returns 42\nstate: flow-verified\nverified: located and read checkout\nunverified: no semantic reference graph"),
        }
    });
    let mut agent = sui::agent::Agent::new(
        sui::provider::Provider::new(
            &format!("http://127.0.0.1:{port}"),
            None,
            "inventory-fixture".into(),
            None,
        ),
        context(&workspace),
        sui::permission::Gate::new(false),
        sui::journal::Journal::open(&fixture.0.join("run")).unwrap(),
        sui::agent::Limits {
            max_turns: 4,
            context_budget: 50000,
            context_reserve: 1000,
            compact_context: false,
            request_timeout: Duration::from_secs(5),
        },
        sui::agent::Identity {
            session_id: "inventory-test".into(),
            agent_id: "worker".into(),
            role: "worker".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            model: "inventory-fixture".into(),
            cache_key_fingerprint: None,
        },
    );
    agent.set_quiet(true);
    agent
        .run_turn("Locate checkout and explain what it returns")
        .await
        .unwrap();
    assert!(serde_json::to_string(agent.history())
        .unwrap()
        .contains("checkout returns 42"));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "inventory"));
    for request in &requests[1..] {
        assert_eq!(request["tools"], requests[0]["tools"]);
        assert_eq!(request["messages"][0], requests[0]["messages"][0]);
        let first = requests[0]["messages"].as_array().unwrap();
        assert_eq!(
            &request["messages"].as_array().unwrap()[..first.len()],
            first
        );
    }
    let tool_result = |request: &Value, id: &str| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["role"] == "tool" && m["tool_call_id"] == id)
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(tool_result(&requests[1], "locate").contains("src/service.rs:2-4 function checkout"));
    assert!(tool_result(&requests[2], "read").contains("42"));
    assert_eq!(
        std::fs::read_to_string(workspace.join("src/service.rs")).unwrap(),
        "// bounded fixture\npub fn checkout() -> u32 {\n    42\n}\n"
    );
}
