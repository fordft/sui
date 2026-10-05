//! Native output reduction/recovery and offline wire-byte proof.
//! Fixtures execute only inside the parent's isolated verification harness.
//! Local byte reductions do not measure provider tokens, cost or cache hits.
#![cfg(unix)]
mod common;

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sui::tools::{self, ExecKind, ExecOut, ToolContext};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-output-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Self(root);
        fixture.write("cargo", "#!/bin/bash\nprintf 'run\\n' >> runs.txt\nprintf '%s\\n' \"$@\" > argv.txt\nif [ -f sleep.txt ]; then\n  printf '%s\\n' \"$$\" > pid.txt\n  exec /bin/sleep 30\nfi\n/bin/cat stdout.txt\n/bin/cat stderr.txt >&2\nIFS= read -r fixture_status < status.txt\nexit \"$fixture_status\"\n");
        std::fs::set_permissions(
            fixture.0.join("cargo"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fixture.set_output("", "", 0);
        fixture
    }

    fn write(&self, name: &str, text: &str) {
        std::fs::write(self.0.join(name), text).unwrap();
    }

    fn set_output(&self, stdout: &str, stderr: &str, status: i32) {
        self.write("stdout.txt", stdout);
        self.write("stderr.txt", stderr);
        self.write("status.txt", &format!("{status}\n"));
    }

    fn command(&self, action: &str) -> String {
        format!("{} {action}", self.0.join("cargo").display())
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.0.join("runs.txt"))
            .unwrap_or_default()
            .lines()
            .count()
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

fn context(root: &Path) -> ToolContext {
    ToolContext {
        workspace: root.to_path_buf(),
        bash_timeout: Duration::from_secs(2),
        bash_timeout_max: Duration::from_secs(5),
        web: None,
        canon_root: Default::default(),
        ui: Default::default(),
        code_intel: Default::default(),
        code_context: Default::default(),
        tool_outputs: Default::default(),
    }
}

async fn execute(ctx: &ToolContext, name: &str, args: Value) -> ExecOut {
    tokio::time::timeout(
        Duration::from_secs(8),
        tools::execute(ctx, name, &args, std::future::pending(), None),
    )
    .await
    .expect("bounded native tool")
    .unwrap()
}

fn field<'a>(text: &'a str, name: &str) -> &'a str {
    text.lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("missing {name}: {text}"))
}

fn count(text: &str, name: &str) -> usize {
    field(text, name).parse().unwrap()
}

fn passing_suite(passed: usize, ignored: bool, unknown: &str) -> String {
    let ignored_count = usize::from(ignored);
    let mut text = format!("\nrunning {} tests\n", passed + ignored_count);
    for index in 0..passed {
        text.push_str(&format!(
            "test module::independent_contract_{index:03} ... ok\n"
        ));
        if index == passed / 2 {
            text.push_str(unknown);
        }
    }
    if ignored {
        text.push_str("test module::requires_external_service ... ignored, needs fixture\n");
    }
    text.push_str(&format!("\ntest result: ok. {passed} passed; 0 failed; {ignored_count} ignored; 0 measured; 0 filtered out; finished in 0.00s\n\n"));
    text
}

fn progress(lines: usize) -> String {
    (0..lines)
        .map(|index| format!("    Checking fixture_dependency_{index:03} v1.2.3\n"))
        .collect()
}

async fn recover(ctx: &ToolContext, id: &str, max_bytes: usize) -> String {
    let mut original = String::new();
    let mut offset = 0;
    for _ in 0..256 {
        let page = execute(
            ctx,
            "read_tool_output",
            json!({"id":id,"offset":offset,"max_bytes":max_bytes}),
        )
        .await;
        assert_eq!(page.kind, ExecKind::Success, "{}", page.text);
        assert!(page.text.len() <= max_bytes);
        assert_eq!(field(&page.text, "observation"), "original_captured_result");
        assert_eq!(field(&page.text, "capture_truncated"), "false");
        assert_eq!(count(&page.text, "offset"), offset);
        let content = page.text.split_once("content:\n").unwrap().1;
        let end = count(&page.text, "end_offset");
        assert_eq!(end, offset + content.len());
        original.push_str(content);
        let more = field(&page.text, "more") == "true";
        assert_eq!(page.truncated, offset != 0 || more);
        if !more {
            assert_eq!(field(&page.text, "next_offset"), "none");
            assert_eq!(count(&page.text, "total_bytes"), original.len());
            return original;
        }
        assert!(end > offset, "pagination must advance");
        assert_eq!(count(&page.text, "next_offset"), end);
        offset = end;
    }
    panic!("bounded recovery exhausted pages")
}

#[tokio::test]
async fn default_auto_preserves_unknown_warning_ignored_summary_and_recovers_exact_raw_once() {
    let fixture = Fixture::new();
    let unknown = "UNKNOWN observation: exact stdout payload Ω🦀\n";
    let warning =
        "warning: preserve this exact diagnostic\n --> src/lib.rs:7:3\n  | original context\n";
    let stdout = passing_suite(80, true, unknown);
    let stderr = format!(
        "{}{warning}    Finished `test` profile [unoptimized] target(s) in 0.01s\n",
        progress(10)
    );
    fixture.set_output(&stdout, &stderr, 0);
    let ctx = fixture.ctx();
    let command = fixture.command("test");
    let auto = execute(&ctx, "bash", json!({"command":command})).await;
    assert_eq!(fixture.runs(), 1, "one Bash call executes the command once");
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("argv.txt")).unwrap(),
        "test\n"
    );
    assert_eq!(auto.kind, ExecKind::Success);
    assert_eq!(auto.exit, Some(0));
    assert!(!auto.truncated);
    assert_eq!(field(&auto.text, "output_compacted"), "true");
    assert_eq!(field(&auto.text, "strategy"), "cargo-success");
    assert!(count(&auto.text, "passed_lines_collapsed") > 0);
    assert!(count(&auto.text, "passed_lines_collapsed") <= 80);
    assert!(count(&auto.text, "progress_lines_collapsed") > 0);
    assert!(count(&auto.text, "progress_lines_collapsed") <= 10);
    for exact in [unknown, warning, "test module::requires_external_service ... ignored, needs fixture\n", "test result: ok. 80 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 0.00s\n"] {
        assert!(auto.text.contains(exact), "missing exact evidence: {exact:?}");
    }
    let id = field(&auto.text, "raw_output_id").to_owned();
    assert_eq!(id.len(), 49);
    let recovered = recover(&ctx, &id, 1024).await;
    assert_eq!(fixture.runs(), 1, "recovery must not dispatch Bash");
    let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
    assert_eq!(fixture.runs(), 2, "only explicit baseline call reruns");
    assert_eq!(recovered, raw.text);
    assert!(!raw.text.contains("output_compacted:"));
    assert_eq!(count(&auto.text, "original_bytes"), raw.text.len());
    assert_eq!(count(&auto.text, "rendered_bytes"), auto.text.len());
    assert!(auto.text.len() < raw.text.len());
}

#[tokio::test]
async fn all_supported_cargo_actions_reduce_only_known_progress_and_keep_diagnostics() {
    let fixture = Fixture::new();
    let exact = "warning: original warning\nUNKNOWN stderr observation\n    Finished `dev` profile [unoptimized] target(s) in 0.01s\n";
    fixture.set_output(
        "unrecognized stdout retained verbatim\n",
        &format!("{}{exact}", progress(30)),
        0,
    );
    let ctx = fixture.ctx();
    for action in ["test", "build", "check", "clippy"] {
        let before = fixture.runs();
        let out = execute(
            &ctx,
            "bash",
            json!({"command":fixture.command(action),"output":"auto"}),
        )
        .await;
        assert_eq!(out.kind, ExecKind::Success);
        assert_eq!(fixture.runs(), before + 1);
        assert_eq!(field(&out.text, "output_compacted"), "true");
        assert_eq!(count(&out.text, "passed_lines_collapsed"), 0);
        assert!(out.text.contains("unrecognized stdout retained verbatim\n"));
        assert!(out.text.contains(exact));
    }
}

#[tokio::test]
async fn failed_unknown_incomplete_and_inconsistent_test_results_are_identical_to_raw() {
    let complete = passing_suite(30, false, "");
    let cases = [
        (
            complete.clone(),
            "error: exact failure, despite passing stdout\n".to_owned(),
            17,
        ),
        (
            "opaque output: preserve every byte Ω🦀\n".to_owned(),
            "unrecognized stderr\n".to_owned(),
            0,
        ),
        (
            complete.split("test result:").next().unwrap().to_owned(),
            String::new(),
            0,
        ),
        (complete.replace("30 passed", "29 passed"), String::new(), 0),
        (
            complete.replace(" ... ok\n", " ... unexpected\n"),
            String::new(),
            0,
        ),
    ];
    for (stdout, stderr, code) in cases {
        let fixture = Fixture::new();
        fixture.set_output(&stdout, &stderr, code);
        let ctx = fixture.ctx();
        let command = fixture.command("test");
        let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
        let auto = execute(&ctx, "bash", json!({"command":command,"output":"auto"})).await;
        assert_eq!(auto.text, raw.text);
        assert_eq!(auto.kind, raw.kind);
        assert_eq!(auto.exit, Some(code));
        assert_eq!(auto.truncated, raw.truncated);
        assert!(!auto.text.contains("raw_output_id:"));
        assert_eq!(fixture.runs(), 2);
    }
}

#[tokio::test]
async fn capture_truncation_remains_honest_and_never_gets_a_recovery_id() {
    let fixture = Fixture::new();
    fixture.set_output(&passing_suite(1200, false, ""), "", 0);
    let ctx = fixture.ctx();
    let command = fixture.command("test");
    let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
    let auto = execute(&ctx, "bash", json!({"command":command,"output":"auto"})).await;
    assert_eq!(raw.kind, ExecKind::Success);
    assert!(raw.truncated);
    assert_eq!(auto.text, raw.text);
    assert_eq!(auto.truncated, raw.truncated);
    assert!(auto.text.contains("bytes omitted"));
    assert!(!auto.text.contains("raw_output_id:"));
    assert_eq!(fixture.runs(), 2);
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn incomplete_pipe_drain_never_promotes_a_complete_looking_suite_to_recoverable_output() {
    struct EscapedChild(PathBuf);
    impl Drop for EscapedChild {
        fn drop(&mut self) {
            if let Ok(text) = std::fs::read_to_string(&self.0) {
                if let Ok(pid) = text.trim().parse::<i32>() {
                    if pid > 1 {
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                        }
                    }
                }
            }
        }
    }
    let fixture = Fixture::new();
    fixture.set_output(&passing_suite(80, false, ""), "", 0);
    fixture.write("cargo", "#!/bin/bash\nprintf 'run\\n' >> runs.txt\n/bin/cat stdout.txt\n/usr/bin/setsid /bin/bash -c 'printf \"%s\\n\" \"$$\" > escaped-pid.txt; exec /bin/sleep 30' &\nwhile [ ! -f escaped-pid.txt ]; do /bin/sleep 0.01; done\nexit 0\n");
    let escaped = EscapedChild(fixture.0.join("escaped-pid.txt"));
    let ctx = fixture.ctx();
    let args = json!({"command":fixture.command("test")});
    let out = tokio::time::timeout(
        Duration::from_secs(15),
        tools::execute(&ctx, "bash", &args, std::future::pending(), None),
    )
    .await
    .expect("non-EOF pipe holder must not hang the tool")
    .unwrap();
    assert_eq!(
        out.kind,
        ExecKind::Success,
        "parent command itself exited zero"
    );
    assert_eq!(out.exit, Some(0));
    assert!(
        out.truncated,
        "missing EOF is incomplete capture, despite matching suite summary"
    );
    assert!(out.text.contains("truncated: true"));
    assert!(out.text.contains("80 passed; 0 failed"));
    assert!(!out.text.contains("output_compacted:"));
    assert!(!out.text.contains("raw_output_id:"));
    assert_eq!(fixture.runs(), 1);
    drop(escaped);
}

#[tokio::test]
async fn shell_syntax_and_machine_output_flags_are_not_rewritten_or_compacted() {
    let fixture = Fixture::new();
    fixture.set_output(&passing_suite(30, false, ""), "", 0);
    let ctx = fixture.ctx();
    for suffix in [
        "test --message-format=json",
        "test -- --list",
        "test -- --format json",
        "test -- --nocapture",
        "test -- --no-capture",
        "test && true",
        "test; true",
    ] {
        let command = fixture.command(suffix);
        let before = fixture.runs();
        let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
        let auto = execute(&ctx, "bash", json!({"command":command,"output":"auto"})).await;
        assert_eq!(auto.text, raw.text, "{suffix}");
        assert_eq!(fixture.runs(), before + 2);
    }
}

#[tokio::test]
async fn invalid_output_mode_is_rejected_before_execution() {
    let fixture = Fixture::new();
    let ctx = fixture.ctx();
    for mode in [
        json!("compact"),
        Value::Null,
        json!(false),
        json!(3),
        json!({}),
    ] {
        let out = execute(
            &ctx,
            "bash",
            json!({"command":fixture.command("test"),"output":mode}),
        )
        .await;
        assert_eq!(out.kind, ExecKind::Error);
        assert_eq!(out.exit, None);
        assert!(out.text.starts_with("status: error\n"));
        assert!(!out.text.contains("raw_output_id:"));
    }
    assert_eq!(fixture.runs(), 0);
}

#[tokio::test]
async fn timeouts_and_cancellation_keep_typed_outcomes_and_kill_the_original_child() {
    let fixture = Fixture::new();
    fixture.write("sleep.txt", "");
    let ctx = fixture.ctx();
    let args = json!({"command":fixture.command("test"),"timeout_ms":5000,"output":"auto"});
    let cancel = async {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !fixture.0.join("pid.txt").exists() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("fixture child started");
    };
    let out = tokio::time::timeout(
        Duration::from_secs(4),
        tools::execute(&ctx, "bash", &args, cancel, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(out.kind, ExecKind::Cancelled);
    assert_eq!(out.exit, None);
    assert!(!out.text.contains("raw_output_id:"));
    let pid = std::fs::read_to_string(fixture.0.join("pid.txt")).unwrap();
    let pid: i32 = pid.trim().parse().unwrap();
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "cancelled original child reaped"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
    assert_eq!(fixture.runs(), 1);
    for mode in ["auto", "raw"] {
        let out = execute(
            &ctx,
            "bash",
            json!({"command":fixture.command("test"),"timeout_ms":40,"output":mode}),
        )
        .await;
        assert_eq!(out.kind, ExecKind::Timeout);
        assert_eq!(out.exit, None);
        assert!(!out.text.contains("raw_output_id:"));
    }
    assert_eq!(fixture.runs(), 3);
}

#[tokio::test]
async fn recovery_pagination_is_utf8_exact_and_rejects_bad_bounds_foreign_and_moved_contexts() {
    let fixture = Fixture::new();
    let unicode = format!("UNKNOWN Unicode: {}\n", "Ω🦀".repeat(900));
    fixture.set_output(&passing_suite(120, false, &unicode), "", 0);
    let mut ctx = fixture.ctx();
    let compact = execute(&ctx, "bash", json!({"command":fixture.command("test")})).await;
    let id = field(&compact.text, "raw_output_id").to_owned();
    let original = recover(&ctx, &id, 1024).await;
    assert!(original.contains(&unicode));
    assert_eq!(fixture.runs(), 1);
    let inside_unicode = original.find('🦀').unwrap() + 1;
    for args in [
        json!({"id":id,"offset":inside_unicode}),
        json!({"id":id,"offset":original.len()+1}),
        json!({"id":id,"offset":-1}),
        json!({"id":id,"max_bytes":1023}),
        json!({"id":id,"max_bytes":24001}),
        json!({"id":id,"max_bytes":"1024"}),
        json!({"id":"not-an-output-id"}),
        json!({"id":id,"extra":true}),
    ] {
        let out = execute(&ctx, "read_tool_output", args).await;
        assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
        assert!(!out.text.contains("UNKNOWN Unicode:"));
    }
    let end = execute(
        &ctx,
        "read_tool_output",
        json!({"id":id,"offset":original.len()}),
    )
    .await;
    assert_eq!(end.kind, ExecKind::Success);
    assert_eq!(end.text.split_once("content:\n").unwrap().1, "");
    let foreign = execute(&fixture.ctx(), "read_tool_output", json!({"id":id})).await;
    assert_eq!(foreign.kind, ExecKind::Error);
    assert!(!foreign.text.contains(&id));
    let other = Fixture::new();
    ctx.workspace = other.0.clone();
    let moved = execute(&ctx, "read_tool_output", json!({"id":id})).await;
    assert_eq!(moved.kind, ExecKind::Error);
    assert!(!moved.text.contains("UNKNOWN Unicode:"));
    assert_eq!(fixture.runs(), 1);
    assert_eq!(other.runs(), 0);
}

#[tokio::test]
async fn memory_only_recovery_evicts_oldest_at_eight_results_and_honors_ready_cancel() {
    let fixture = Fixture::new();
    fixture.set_output(&passing_suite(30, false, ""), "", 0);
    let ctx = fixture.ctx();
    let mut ids = Vec::new();
    for _ in 0..9 {
        let out = execute(&ctx, "bash", json!({"command":fixture.command("test")})).await;
        ids.push(field(&out.text, "raw_output_id").to_owned());
    }
    assert_eq!(fixture.runs(), 9);
    let old = execute(&ctx, "read_tool_output", json!({"id":ids[0]})).await;
    assert_eq!(old.kind, ExecKind::Error);
    assert!(!old.text.contains(&ids[0]));
    for id in &ids[1..] {
        assert!(recover(&ctx, id, 12000).await.contains("30 passed"));
    }
    let args = json!({"id":ids[8]});
    let cancelled = tools::execute(&ctx, "read_tool_output", &args, async {}, None)
        .await
        .unwrap();
    assert_eq!(cancelled.kind, ExecKind::Cancelled);
    assert!(!cancelled.text.contains("content:\n"));
    assert_eq!(fixture.runs(), 9);
    assert_eq!(
        execute(&ctx, "read_tool_output", args).await.kind,
        ExecKind::Success
    );
}

#[tokio::test]
async fn recovery_refuses_a_warmed_workspace_symlink_retargeted_to_another_root() {
    let parent = Fixture::new();
    let first = Fixture::new();
    let second = Fixture::new();
    first.set_output(&passing_suite(30, false, ""), "", 0);
    let alias = parent.0.join("workspace");
    std::os::unix::fs::symlink(&first.0, &alias).unwrap();
    let ctx = context(&alias);
    let out = execute(&ctx, "bash", json!({"command":first.command("test")})).await;
    let id = field(&out.text, "raw_output_id");
    assert!(recover(&ctx, id, 12000).await.contains("30 passed"));
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&second.0, &alias).unwrap();
    let refused = execute(&ctx, "read_tool_output", json!({"id":id})).await;
    assert_eq!(refused.kind, ExecKind::Error);
    assert!(!refused.text.contains("30 passed"));
    assert_eq!(first.runs(), 1);
    assert_eq!(second.runs(), 0);
}

#[tokio::test]
async fn real_cargo_libtest_output_compacts_and_recovers_without_running_tests_again() {
    let fixture = Fixture::new();
    fixture.write("Cargo.toml", "[package]\nname = \"output_compaction_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n");
    std::fs::create_dir(fixture.0.join("src")).unwrap();
    let mut source =
        "fn unused_fixture_helper() {}\n#[cfg(test)]\nmod tests {\nuse std::io::Write;\n"
            .to_owned();
    for index in 0..40 {
        source.push_str(&format!("#[test]\nfn contract_{index:03}() {{\nlet mut record = std::fs::OpenOptions::new().create(true).append(true).open(\"executed.txt\").unwrap();\nrecord.write_all(b\"{index}\\n\").unwrap();\nassert!(!record.metadata().unwrap().is_dir());\n}}\n"));
    }
    source.push_str("}\n");
    fixture.write("src/lib.rs", &source);
    let mut ctx = fixture.ctx();
    ctx.bash_timeout = Duration::from_secs(60);
    ctx.bash_timeout_max = Duration::from_secs(60);
    let args = json!({"command":format!("cargo test --offline --manifest-path {}", fixture.0.join("Cargo.toml").display())});
    let out = tokio::time::timeout(
        Duration::from_secs(70),
        tools::execute(&ctx, "bash", &args, std::future::pending(), None),
    )
    .await
    .expect("tiny offline Cargo fixture bounded")
    .unwrap();
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert_eq!(out.exit, Some(0));
    assert!(!out.truncated);
    assert_eq!(field(&out.text, "output_compacted"), "true");
    assert!(out.text.contains("40 passed; 0 failed; 0 ignored"));
    assert!(
        out.text
            .contains("warning: function `unused_fixture_helper` is never used"),
        "{}",
        out.text
    );
    let executions = std::fs::read_to_string(fixture.0.join("executed.txt")).unwrap();
    assert_eq!(executions.lines().count(), 40);
    let original = recover(&ctx, field(&out.text, "raw_output_id"), 1024).await;
    for index in 0..40 {
        assert!(original.contains(&format!("test tests::contract_{index:03} ... ok\n")));
    }
    assert_eq!(count(&out.text, "original_bytes"), original.len());
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("executed.txt")).unwrap(),
        executions
    );
    assert_eq!(
        fixture.runs(),
        0,
        "real Cargo comes from trusted harness PATH, not the stub"
    );
}

fn usage_free_text() -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"state: flow-verified\nverified: original command observed\nunverified: provider tokens and cache savings"},"finish_reason":"stop"}]})
    )
}

fn usage_free_call(command: &str, mode: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"verification","type":"function","function":{"name":"bash","arguments":json!({"command":command,"output":mode}).to_string()}}]},"finish_reason":"tool_calls"}]})
    )
}

fn cli_trajectory(fixture: &Fixture, mode: &str) -> Vec<(Vec<u8>, Value)> {
    use std::process::{Command, Stdio};
    let captured = Arc::new(Mutex::new(Vec::<(Vec<u8>, Value)>::new()));
    let saved = captured.clone();
    let command = fixture.command("test");
    let args = if mode == "auto" {
        json!({"command":command})
    } else {
        json!({"command":command,"output":"raw"})
    };
    let reply = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"verification","type":"function","function":{"name":"bash","arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
    );
    let port = common::serve(move |body, _| {
        let mut requests = saved.lock().unwrap();
        requests.push((body.to_vec(), serde_json::from_slice(body).unwrap()));
        if requests.len() == 1 {
            reply.clone()
        } else {
            usage_free_text()
        }
    });
    let stdout = fixture.0.join(format!("cli-{mode}.stdout"));
    let stderr = fixture.0.join(format!("cli-{mode}.stderr"));
    let mut child = Command::new(env!("CARGO_BIN_EXE_sui"))
        .current_dir(&fixture.0)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", fixture.0.join("home"))
        .env("TMPDIR", fixture.0.join("tmp"))
        .env("SUI_HOME", fixture.0.join(format!(".sui/cli-{mode}")))
        .args([
            "--base-url",
            &format!("http://127.0.0.1:{port}/v1"),
            "--model",
            "output-cli-byte-fixture",
            "--api-key",
            "dummy-output-fixture-key",
            "--workspace",
        ])
        .arg(&fixture.0)
        .args([
            "--yolo",
            "Run the fixture verification; keep exact failures and uncertainty.",
        ])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let end = std::time::Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < end => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("headless CLI deadline exceeded");
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("headless CLI wait: {error}");
            }
        }
    };
    assert!(
        status.success(),
        "CLI failed:\n{}\n{}",
        std::fs::read_to_string(stdout).unwrap(),
        std::fs::read_to_string(stderr).unwrap()
    );
    let requests = captured.lock().unwrap().clone();
    requests
}

#[test]
fn actual_headless_cli_default_auto_and_raw_override_preserve_prefix_and_reduce_wire_bytes() {
    let fixture = Fixture::new();
    fixture.write(
        "sui.toml",
        "[agent]\ncontext_compaction = false\nmax_turns = 3\n",
    );
    std::fs::create_dir(fixture.0.join("home")).unwrap();
    std::fs::create_dir(fixture.0.join("tmp")).unwrap();
    let unknown = "UNKNOWN CLI observation exact Ω🦀\n";
    let warning = "warning: original CLI diagnostic\n --> src/lib.rs:5:2\n";
    fixture.set_output(&passing_suite(160, true, unknown), warning, 0);
    let baseline = cli_trajectory(&fixture, "raw");
    let candidate = cli_trajectory(&fixture, "auto");
    assert_eq!(fixture.runs(), 2);
    assert_eq!(baseline.len(), 2);
    assert_eq!(candidate.len(), 2);
    assert_eq!(baseline[0].1["messages"], candidate[0].1["messages"]);
    assert_eq!(baseline[0].1["tools"], candidate[0].1["tools"]);
    let mut results = Vec::new();
    for trajectory in [&baseline, &candidate] {
        let initial = trajectory[0].1["messages"].as_array().unwrap();
        let final_messages = trajectory[1].1["messages"].as_array().unwrap();
        assert_eq!(&final_messages[..initial.len()], initial);
        assert_eq!(trajectory[1].1["tools"], trajectory[0].1["tools"]);
        let result = final_messages
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == "verification")
            .unwrap()["content"]
            .as_str()
            .unwrap();
        for exact in [
            unknown,
            warning,
            "160 passed; 0 failed; 1 ignored",
            "test module::requires_external_service ... ignored, needs fixture\n",
        ] {
            assert!(result.contains(exact));
        }
        let call = final_messages
            .iter()
            .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
            .unwrap();
        let args: Value = serde_json::from_str(
            call["tool_calls"][0]["function"]["arguments"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(args["command"], fixture.command("test"));
        results.push(result);
    }
    assert!(!results[0].contains("output_compacted:"));
    assert_eq!(field(results[1], "output_compacted"), "true");
    assert_eq!(count(results[1], "original_bytes"), results[0].len());
    let bytes =
        |requests: &[(Vec<u8>, Value)]| requests.iter().map(|(body, _)| body.len()).sum::<usize>();
    let input = |requests: &[(Vec<u8>, Value)]| {
        requests
            .iter()
            .map(|(_, request)| {
                serde_json::to_vec(&request["messages"]).unwrap().len()
                    + serde_json::to_vec(&request["tools"]).unwrap().len()
            })
            .sum::<usize>()
    };
    assert!(bytes(&candidate) < bytes(&baseline));
    assert!(input(&candidate) < input(&baseline));
    println!(
        "{}",
        json!({"evaluation":"offline_paired_headless_cli_output", "baseline_model_requests":baseline.len(), "candidate_model_requests":candidate.len(), "baseline_total_http_body_bytes":bytes(&baseline), "candidate_total_http_body_bytes":bytes(&candidate), "baseline_serialized_messages_and_schemas_bytes":input(&baseline), "candidate_serialized_messages_and_schemas_bytes":input(&candidate), "baseline_returned_bytes":results[0].len(), "candidate_returned_bytes":results[1].len(), "command_executions_per_flow":1, "critical_observations_preserved":true, "provider_tokens":null, "provider_cache_hits":null})
    );
}

async fn native_trajectory(
    fixture: &Fixture,
    mode: &str,
    deny: bool,
) -> (Vec<(Vec<u8>, Value)>, Vec<sui::types::Message>) {
    let requests = Arc::new(Mutex::new(Vec::<(Vec<u8>, Value)>::new()));
    let capture = requests.clone();
    let command = fixture.command("test");
    let reply = usage_free_call(&command, mode);
    let port = common::serve(move |body, _| {
        let mut saved = capture.lock().unwrap();
        saved.push((body.to_vec(), serde_json::from_slice(body).unwrap()));
        if saved.len() == 1 {
            reply.clone()
        } else {
            usage_free_text()
        }
    });
    let (events, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut gate = sui::permission::Gate::new(!deny);
    if deny {
        gate.set_ui(
            events,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
            None,
        );
    }
    let mut agent = sui::agent::Agent::new(
        sui::provider::Provider::new(
            &format!("http://127.0.0.1:{port}"),
            None,
            "output-byte-fixture".into(),
            None,
        ),
        fixture.ctx(),
        gate,
        sui::journal::Journal::open(&fixture.0.join(".sui").join(if deny {
            "denied"
        } else {
            mode
        }))
        .unwrap(),
        sui::agent::Limits {
            max_turns: 3,
            context_budget: 50000,
            context_reserve: 1000,
            compact_context: false,
            request_timeout: Duration::from_secs(5),
        },
        sui::agent::Identity {
            session_id: "output-byte-fixture".into(),
            agent_id: "worker".into(),
            role: "worker".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            model: "output-byte-fixture".into(),
            cache_key_fingerprint: None,
        },
    );
    agent.set_quiet(true);
    if deny {
        let reject = async {
            while let Some(event) = rx.recv().await {
                if let sui::events::UiEvent::Permission { summary, reply, .. } = event {
                    assert_eq!(summary, format!("bash: {command}"));
                    reply.send(sui::events::GateChoice::Deny).unwrap();
                    return;
                }
            }
            panic!("permission request missing")
        };
        tokio::time::timeout(Duration::from_secs(8), async {
            let (result, ()) = tokio::join!(
                agent
                    .run_turn("Run the fixture verification; keep exact failures and uncertainty."),
                reject
            );
            result.unwrap();
        })
        .await
        .unwrap();
    } else {
        agent
            .run_turn("Run the fixture verification; keep exact failures and uncertainty.")
            .await
            .unwrap();
        agent
            .run_turn("Continue using the recorded result; do not rerun the command.")
            .await
            .unwrap();
    }
    let captured = requests.lock().unwrap().clone();
    (captured, agent.history().to_vec())
}

#[tokio::test]
async fn native_permission_denial_preserves_original_command_and_never_executes() {
    let fixture = Fixture::new();
    fixture.set_output(&passing_suite(30, false, ""), "", 0);
    let (requests, _) = native_trajectory(&fixture, "auto", true).await;
    assert_eq!(fixture.runs(), 0);
    assert_eq!(requests.len(), 2);
    let tool = requests[1].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .unwrap();
    assert!(tool["content"]
        .as_str()
        .unwrap()
        .starts_with("status: denied\n"));
    assert!(!tool["content"].as_str().unwrap().contains("raw_output_id:"));
}

#[tokio::test]
async fn paired_native_wire_bytes_shrink_with_frozen_schemas_append_only_history_and_replay() {
    let fixture = Fixture::new();
    let unknown = "UNKNOWN verified observation must survive exactly\n";
    let warning = "warning: retained original diagnostic\n --> src/lib.rs:19:4\n";
    fixture.set_output(&passing_suite(160, true, unknown), warning, 0);
    let (baseline, _) = native_trajectory(&fixture, "raw", false).await;
    let (candidate, history) = native_trajectory(&fixture, "auto", false).await;
    assert_eq!(
        fixture.runs(),
        2,
        "one execution per independently paired flow"
    );
    assert_eq!(baseline.len(), 3);
    assert_eq!(candidate.len(), 3);
    assert_eq!(baseline[0].1["messages"], candidate[0].1["messages"]);
    assert_eq!(baseline[0].1["tools"], candidate[0].1["tools"]);
    let schemas = candidate[0].1["tools"].as_array().unwrap();
    assert!(schemas
        .iter()
        .any(|tool| tool["function"]["name"] == "read_tool_output"));
    let bash_schema = schemas
        .iter()
        .find(|tool| tool["function"]["name"] == "bash")
        .unwrap();
    assert_eq!(
        bash_schema["function"]["parameters"]["properties"]["output"]["enum"],
        json!(["auto", "raw"])
    );
    for trajectory in [&baseline, &candidate] {
        for index in 1..trajectory.len() {
            let previous = trajectory[index - 1].1["messages"].as_array().unwrap();
            assert_eq!(
                &trajectory[index].1["messages"].as_array().unwrap()[..previous.len()],
                previous
            );
            assert_eq!(trajectory[index].1["tools"], trajectory[0].1["tools"]);
            let tools = trajectory[index].1["messages"].as_array().unwrap();
            let result = tools
                .iter()
                .find(|message| {
                    message["role"] == "tool" && message["tool_call_id"] == "verification"
                })
                .unwrap()["content"]
                .as_str()
                .unwrap();
            for fact in [
                unknown,
                warning,
                "160 passed; 0 failed; 1 ignored",
                "test module::requires_external_service ... ignored, needs fixture\n",
            ] {
                assert!(result.contains(fact), "critical observation lost");
            }
            let call = tools
                .iter()
                .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
                .unwrap();
            let args: Value = serde_json::from_str(
                call["tool_calls"][0]["function"]["arguments"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(args["command"], fixture.command("test"));
        }
    }
    let original_journal = sui::journal::Journal::path_of(&fixture.0.join(".sui/auto"), "events");
    let replay = sui::journal::replay_history(&original_journal, usize::MAX).unwrap();
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        serde_json::to_value(&history).unwrap()
    );
    let tool = history
        .iter()
        .find_map(|message| match message {
            sui::types::Message::Tool { content, .. } => Some(content),
            _ => None,
        })
        .unwrap();
    let id = field(tool, "raw_output_id");
    let resumed = execute(&fixture.ctx(), "read_tool_output", json!({"id":id})).await;
    assert_eq!(
        resumed.kind,
        ExecKind::Error,
        "raw IDs are ephemeral, not replayed execution proof"
    );
    assert_eq!(fixture.runs(), 2);
    let wire_bytes =
        |requests: &[(Vec<u8>, Value)]| requests.iter().map(|(body, _)| body.len()).sum::<usize>();
    let input_bytes = |requests: &[(Vec<u8>, Value)]| {
        requests
            .iter()
            .map(|(_, request)| {
                serde_json::to_vec(&request["messages"]).unwrap().len()
                    + serde_json::to_vec(&request["tools"]).unwrap().len()
            })
            .sum::<usize>()
    };
    assert!(wire_bytes(&candidate) < wire_bytes(&baseline));
    assert!(input_bytes(&candidate) < input_bytes(&baseline));
    println!(
        "{}",
        json!({"evaluation":"offline_paired_native_output", "baseline_model_requests":baseline.len(), "candidate_model_requests":candidate.len(), "baseline_total_http_body_bytes":wire_bytes(&baseline), "candidate_total_http_body_bytes":wire_bytes(&candidate), "baseline_serialized_messages_and_schemas_bytes":input_bytes(&baseline), "candidate_serialized_messages_and_schemas_bytes":input_bytes(&candidate), "critical_observations_preserved":true, "command_executions_per_flow":1, "provider_tokens":null, "provider_cache_hits":null, "limitations":"scripted local wire-byte proof; not live reasoning or provider billing/cache evidence"})
    );
}
