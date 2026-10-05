//! Graph-only Git stat reduction, with real Git and native CLI recovery.
//! Offline byte measurements never stand in for provider token/cache usage.
#![cfg(unix)]
mod common;

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sui::tools::{self, ExecKind, ExecOut, ToolContext};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-git-stat-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(root.join("bin")).unwrap();
        let fixture = Self(root.canonicalize().unwrap());
        fixture.write("bin/git", "#!/bin/sh\nprintf 'run\\n' >> runs.txt\nprintf '%s\\n' \"$@\" > argv.txt\n/bin/cat stdout.txt\n/bin/cat stderr.txt >&2\nIFS= read -r fixture_status < status.txt\nexit \"$fixture_status\"\n");
        std::fs::set_permissions(
            fixture.0.join("bin/git"),
            std::fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fixture.output("", "", 0);
        fixture
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.0.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn output(&self, stdout: &str, stderr: &str, code: i32) {
        self.write("stdout.txt", stdout);
        self.write("stderr.txt", stderr);
        self.write("status.txt", &format!("{code}\n"));
    }

    fn command(&self, args: &str) -> String {
        format!("{} {args}", self.0.join("bin/git").display())
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.0.join("runs.txt"))
            .unwrap_or_default()
            .lines()
            .count()
    }

    fn ctx(&self) -> ToolContext {
        ToolContext {
            workspace: self.0.clone(),
            bash_timeout: Duration::from_secs(3),
            bash_timeout_max: Duration::from_secs(5),
            web: None,
            canon_root: Default::default(),
            ui: Default::default(),
            code_intel: Default::default(),
            code_context: Default::default(),
            tool_outputs: Default::default(),
        }
    }

    fn git(&self, args: &[&str]) {
        let output = Command::new("/usr/bin/git")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.0.join("home"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("LC_ALL", "C")
            .current_dir(&self.0)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "fixture Git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn real_repo() -> Self {
        let fixture = Self::new();
        fixture.write("bin/git", "#!/bin/sh\nprintf 'run\\n' >> runs.txt\nprintf '%s\\n' \"$@\" > argv.txt\nexec /usr/bin/git \"$@\"\n");
        fixture.write(".gitignore", "bin/\nstdout.txt\nstderr.txt\nstatus.txt\nruns.txt\nargv.txt\nhome/\ntmp/\n.sui/\ncli*.stdout\ncli*.stderr\nsui.toml\n");
        fixture.git(&["-c", "init.defaultBranch=main", "init", "-q"]);
        for (name, value) in [
            ("user.email", "fixture@example.invalid"),
            ("user.name", "Fixture"),
            ("core.hooksPath", "/dev/null"),
            ("core.quotePath", "false"),
        ] {
            fixture.git(&["config", name, value]);
        }
        let paths: Vec<String> = (0..25)
            .map(|i| format!("assets/file_{i:03}.txt"))
            .chain([
                "assets/méthode_🦀.txt".to_owned(),
                "assets/tab\tname.txt".to_owned(),
                "assets/quote\"name.txt".to_owned(),
                "assets/rename_old.txt".to_owned(),
            ])
            .collect();
        for (index, path) in paths.iter().enumerate() {
            let original: String = (0..40)
                .map(|line| format!("original file {index} line {line}\n"))
                .collect();
            fixture.write(path, &original);
        }
        std::fs::write(fixture.0.join("assets/image.bin"), [0u8; 20]).unwrap();
        fixture.git(&["add", "--", ".gitignore", "assets"]);
        fixture.git(&["commit", "-qm", "fixture baseline"]);
        for (index, path) in paths.iter().enumerate().take(paths.len() - 1) {
            let modified: String = (0..70)
                .map(|line| format!("modified file {index} line {line}\n"))
                .collect();
            fixture.write(path, &modified);
        }
        std::fs::rename(
            fixture.0.join("assets/rename_old.txt"),
            fixture.0.join("assets/rename_new.txt"),
        )
        .unwrap();
        let renamed = std::fs::read_to_string(fixture.0.join("assets/rename_new.txt")).unwrap();
        fixture.write(
            "assets/rename_new.txt",
            &format!("{renamed}one appended line\n"),
        );
        std::fs::write(fixture.0.join("assets/image.bin"), [0u8; 45]).unwrap();
        fixture.git(&["add", "-A", "--", "assets"]);
        fixture
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn execute(ctx: &ToolContext, tool: &str, args: Value) -> ExecOut {
    tokio::time::timeout(
        Duration::from_secs(8),
        tools::execute(ctx, tool, &args, std::future::pending(), None),
    )
    .await
    .expect("bounded native fixture tool")
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

fn stdout(text: &str) -> &str {
    text.split_once("stdout: ")
        .unwrap()
        .1
        .split_once("\nstderr: ")
        .unwrap()
        .0
}

async fn recover(ctx: &ToolContext, id: &str) -> String {
    let mut original = String::new();
    let mut offset = 0;
    for _ in 0..128 {
        let page = execute(
            ctx,
            "read_tool_output",
            json!({"id":id,"offset":offset,"max_bytes":1024}),
        )
        .await;
        assert_eq!(page.kind, ExecKind::Success, "{}", page.text);
        assert!(page.text.len() <= 1024);
        assert_eq!(field(&page.text, "capture_truncated"), "false");
        assert_eq!(count(&page.text, "offset"), offset);
        let content = page.text.split_once("content:\n").unwrap().1;
        original.push_str(content);
        let end = count(&page.text, "end_offset");
        assert_eq!(end, offset + content.len());
        if field(&page.text, "more") == "false" {
            assert_eq!(field(&page.text, "next_offset"), "none");
            assert_eq!(count(&page.text, "total_bytes"), original.len());
            return original;
        }
        assert!(end > offset);
        assert_eq!(count(&page.text, "next_offset"), end);
        offset = end;
    }
    panic!("bounded Git capture recovery did not finish")
}

fn stat_fixture(files: usize) -> String {
    let mut text: String = (0..files)
        .map(|index| {
            format!(
                " src/file_{index:03}.rs | 180 {}{}\n",
                "+".repeat(45),
                "-".repeat(45)
            )
        })
        .collect();
    text.push_str(&format!(
        " {files} files changed, {} insertions(+), {} deletions(-)\n",
        files * 90,
        files * 90
    ));
    text
}

fn assert_only_histograms_removed(raw: &str, compact: &str) -> usize {
    let original: Vec<_> = raw.split_inclusive('\n').collect();
    let rendered: Vec<_> = compact.split_inclusive('\n').collect();
    assert_eq!(
        rendered.len(),
        original.len(),
        "stat file/footer rows must remain one-for-one"
    );
    let mut removed = 0;
    for (before, after) in original.iter().zip(rendered) {
        let body = before.strip_suffix('\n').unwrap_or(before);
        let suffix = body.split_ascii_whitespace().last().unwrap_or("");
        if before.contains(" | ")
            && !suffix.is_empty()
            && suffix.bytes().all(|byte| matches!(byte, b'+' | b'-'))
        {
            let prefix = &body[..body.len() - suffix.len()];
            let ending = if before.ends_with('\n') { "\n" } else { "" };
            assert_eq!(
                after,
                format!("{prefix}{ending}"),
                "only +/- histogram may disappear"
            );
            removed += 1;
        } else {
            assert_eq!(
                after, *before,
                "binary/name/footer/unknown bytes must remain exact"
            );
        }
    }
    removed
}

#[tokio::test]
async fn real_git_stat_preserves_paths_total_counts_footer_binary_and_rename_with_exact_recovery() {
    let fixture = Fixture::real_repo();
    let ctx = fixture.ctx();
    let command = fixture.command("diff --cached --stat --no-color --no-ext-diff");
    let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
    let compact = execute(&ctx, "bash", json!({"command":command})).await;
    assert_eq!(fixture.runs(), 2);
    assert_eq!(raw.kind, ExecKind::Success, "{}", raw.text);
    assert_eq!(compact.kind, raw.kind);
    assert_eq!(compact.exit, Some(0));
    assert!(!compact.truncated);
    assert_eq!(field(&compact.text, "output_compacted"), "true");
    assert_eq!(field(&compact.text, "strategy"), "git-diff-stat");
    assert_eq!(
        field(&compact.text, "per_file_counts"),
        "total_changes_only"
    );
    let original = stdout(&raw.text);
    assert!(original.contains("méthode_🦀.txt"));
    assert!(original.contains("tab\\tname.txt"));
    assert!(original.contains("quote\\\"name.txt"));
    assert!(original.contains("=>"), "real rename is represented");
    assert!(original.contains("Bin 20 -> 45 bytes"));
    let removed = assert_only_histograms_removed(original, stdout(&compact.text));
    assert!(removed >= 25);
    assert_eq!(count(&compact.text, "stat_graphs_removed"), removed);
    assert_eq!(count(&compact.text, "original_bytes"), raw.text.len());
    assert_eq!(count(&compact.text, "rendered_bytes"), compact.text.len());
    assert!(compact.text.len() < raw.text.len());
    let recovered = recover(&ctx, field(&compact.text, "raw_output_id")).await;
    assert_eq!(recovered, raw.text);
    assert_eq!(fixture.runs(), 2, "recovery must not rerun Git");
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("argv.txt")).unwrap(),
        "diff\n--cached\n--stat\n--no-color\n--no-ext-diff\n"
    );
}

#[tokio::test]
async fn supported_staged_and_literal_path_scopes_keep_original_arguments_and_stderr() {
    let fixture = Fixture::new();
    let original = stat_fixture(12);
    let warning = "warning: exact Git diagnostic retained\n";
    fixture.output(&original, warning, 0);
    let ctx = fixture.ctx();
    for args in [
        "diff --stat",
        "diff --stat --cached",
        "diff --stat --staged",
        "diff --stat --no-color --no-ext-diff -- src",
    ] {
        let before = fixture.runs();
        let compact = execute(&ctx, "bash", json!({"command":fixture.command(args)})).await;
        assert_eq!(compact.kind, ExecKind::Success, "{args}: {}", compact.text);
        assert_eq!(field(&compact.text, "strategy"), "git-diff-stat");
        assert!(compact.text.contains(warning));
        assert_only_histograms_removed(&original, stdout(&compact.text));
        assert_eq!(fixture.runs(), before + 1);
        assert_eq!(
            std::fs::read_to_string(fixture.0.join("argv.txt")).unwrap(),
            format!(
                "{}\n",
                args.split_ascii_whitespace().collect::<Vec<_>>().join("\n")
            )
        );
    }
}

#[tokio::test]
async fn incompatible_flags_shell_composition_and_status_output_keep_the_raw_capture() {
    let fixture = Fixture::new();
    fixture.output(&stat_fixture(12), "", 0);
    let ctx = fixture.ctx();
    for args in [
        "diff --stat=80,40,2",
        "diff --stat --stat-count=2",
        "diff --stat --color=always",
        "diff --stat --patch",
        "diff --stat --numstat",
        "diff --stat --name-only",
        "diff --stat --summary",
        "status --short",
        "diff --stat && true",
    ] {
        let command = fixture.command(args);
        let before = fixture.runs();
        let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
        let compact = execute(&ctx, "bash", json!({"command":command})).await;
        assert_eq!(compact.text, raw.text, "{args}");
        assert!(!compact.text.contains("raw_output_id:"));
        assert_eq!(fixture.runs(), before + 2);
    }
}

#[tokio::test]
async fn incomplete_malformed_colored_and_source_patch_outputs_are_never_summarized() {
    let complete = stat_fixture(12);
    let patch = "diff --git a/source.rs b/source.rs\nindex 1111111..2222222 100644\n--- a/source.rs\n+++ b/source.rs\n@@ -1 +1 @@\n-fn old() {}\n+fn new() {}\n";
    let cases = [
        complete.replace("12 files changed", "13 files changed"),
        complete
            .lines()
            .take(12)
            .map(|line| format!("{line}\n"))
            .collect(),
        format!(
            "{} ...\n 12 files changed, 1080 insertions(+), 1080 deletions(-)\n",
            complete.lines().next().unwrap()
        ),
        complete.replace(" | 180 ", " | invalid "),
        complete.replace(" src/", " \u{1b}[32msrc/"),
        format!("{complete}{patch}"),
        format!("UNKNOWN exact stdout observation\n{complete}"),
    ];
    for source in cases {
        let fixture = Fixture::new();
        fixture.output(&source, "unknown stderr stays exact\n", 0);
        let ctx = fixture.ctx();
        let command = fixture.command("diff --stat");
        let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
        let compact = execute(&ctx, "bash", json!({"command":command})).await;
        assert_eq!(
            compact.text, raw.text,
            "original capture must survive malformed stats/patch"
        );
        assert_eq!(compact.kind, raw.kind);
        assert_eq!(fixture.runs(), 2);
    }
}

#[tokio::test]
async fn failed_small_and_capture_truncated_stats_remain_identical_to_raw() {
    let cases = [
        (stat_fixture(12), "fatal: exact Git failure\n", 128),
        (
            " src/one.rs | 1 +\n 1 file changed, 1 insertion(+)\n".to_owned(),
            "",
            0,
        ),
        (stat_fixture(500), "", 0),
    ];
    for (source, stderr, code) in cases {
        let fixture = Fixture::new();
        fixture.output(&source, stderr, code);
        let ctx = fixture.ctx();
        let command = fixture.command("diff --stat");
        let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
        let compact = execute(&ctx, "bash", json!({"command":command})).await;
        assert_eq!(compact.text, raw.text);
        assert_eq!(compact.kind, raw.kind);
        assert_eq!(compact.exit, Some(code));
        assert_eq!(compact.truncated, raw.truncated);
        assert!(!compact.text.contains("raw_output_id:"));
        if source.len() > 30000 {
            assert!(compact.truncated);
        }
        assert_eq!(fixture.runs(), 2);
    }
}

#[tokio::test]
async fn real_source_diff_patch_stays_the_original_bounded_capture() {
    let fixture = Fixture::real_repo();
    let ctx = fixture.ctx();
    let command =
        fixture.command("diff --cached --patch --no-color --no-ext-diff -- assets/file_000.txt");
    let raw = execute(&ctx, "bash", json!({"command":command,"output":"raw"})).await;
    let auto = execute(&ctx, "bash", json!({"command":command})).await;
    assert_eq!(auto.text, raw.text);
    assert!(!auto.truncated);
    assert!(auto.text.contains("@@ -1,40 +1,70 @@"));
    assert!(auto.text.contains("-original file 0 line 0\n"));
    assert!(auto.text.contains("+modified file 0 line 69\n"));
    assert!(!auto.text.contains("raw_output_id:"));
    assert_eq!(fixture.runs(), 2);
}

fn call(id: &str, name: &str, args: Value) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":id,"type":"function","function":{"name":name,"arguments":args.to_string()}}]},"finish_reason":"tool_calls"}]})
    )
}

fn done() -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({"choices":[{"delta":{"content":"state: flow-verified\nverified: original Git stat recovered\nunverified: provider reasoning, tokens and cache savings"},"finish_reason":"stop"}]})
    )
}

#[test]
fn actual_headless_cli_real_git_stat_and_recovery_preserve_schema_and_prior_history() {
    let fixture = Fixture::real_repo();
    fixture.write(
        "sui.toml",
        "[agent]\ncontext_compaction = false\nmax_turns = 4\n",
    );
    std::fs::create_dir(fixture.0.join("home")).unwrap();
    std::fs::create_dir(fixture.0.join("tmp")).unwrap();
    let command = fixture.command("diff --cached --stat --no-color --no-ext-diff");
    let captured = Arc::new(Mutex::new(Vec::<(Vec<u8>, Value)>::new()));
    let saved = captured.clone();
    let original_command = command.clone();
    let port = common::serve(move |body, _| {
        let mut requests = saved.lock().unwrap();
        let request: Value = serde_json::from_slice(body).unwrap();
        requests.push((body.to_vec(), request.clone()));
        match requests.len() {
            1 => call("stat", "bash", json!({"command":original_command})),
            2 => {
                let compact = request["messages"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|message| message["role"] == "tool" && message["tool_call_id"] == "stat")
                    .unwrap()["content"]
                    .as_str()
                    .unwrap();
                call(
                    "original",
                    "read_tool_output",
                    json!({"id":field(compact,"raw_output_id"),"max_bytes":24000}),
                )
            }
            _ => done(),
        }
    });
    let outpath = fixture.0.join("cli.stdout");
    let errpath = fixture.0.join("cli.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sui")).current_dir(&fixture.0).env_clear()
        .env("PATH", "/usr/bin:/bin").env("HOME", fixture.0.join("home")).env("TMPDIR", fixture.0.join("tmp")).env("SUI_HOME", fixture.0.join(".sui"))
        .args(["--base-url", &format!("http://127.0.0.1:{port}/v1"), "--model", "git-stat-offline-fixture", "--api-key", "dummy-git-stat-key", "--workspace"])
        .arg(&fixture.0).args(["--yolo", "Inspect the staged Git stat and recover its original capture; preserve counts and uncertainty."])
        .stdin(Stdio::null()).stdout(std::fs::File::create(&outpath).unwrap()).stderr(std::fs::File::create(&errpath).unwrap()).spawn().unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
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
        std::fs::read_to_string(outpath).unwrap(),
        std::fs::read_to_string(errpath).unwrap()
    );
    assert_eq!(fixture.runs(), 1, "native recovery does not rerun Git");
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0].1["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "read_tool_output"));
    for index in 1..requests.len() {
        let previous = requests[index - 1].1["messages"].as_array().unwrap();
        assert_eq!(
            &requests[index].1["messages"].as_array().unwrap()[..previous.len()],
            previous
        );
        assert_eq!(requests[index].1["tools"], requests[0].1["tools"]);
    }
    let result = |id: &str| {
        requests[2].1["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .unwrap()["content"]
            .as_str()
            .unwrap()
    };
    let compact = result("stat");
    let page = result("original");
    assert_eq!(field(compact, "strategy"), "git-diff-stat");
    assert_eq!(field(page, "more"), "false");
    let original = page.split_once("content:\n").unwrap().1;
    let removed = assert_only_histograms_removed(stdout(original), stdout(compact));
    assert_eq!(count(compact, "stat_graphs_removed"), removed);
    assert_eq!(count(compact, "original_bytes"), original.len());
    let assistant = requests[1].1["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant" && message["tool_calls"].is_array())
        .unwrap();
    let args: Value = serde_json::from_str(
        assistant["tool_calls"][0]["function"]["arguments"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(args["command"], command);
    println!(
        "{}",
        json!({"evaluation":"offline_headless_real_git_stat_recovery", "model_requests":requests.len(), "command_executions":fixture.runs(), "original_returned_bytes":original.len(), "compact_returned_bytes":compact.len(), "recovery_page_bytes":page.len(), "total_http_body_bytes":requests.iter().map(|(body,_)|body.len()).sum::<usize>(), "original_capture_recovered":true, "history_append_only":true, "provider_tokens":null, "provider_cache_hits":null, "limitations":"recovery adds context; not a net savings or provider billing claim"})
    );
}
