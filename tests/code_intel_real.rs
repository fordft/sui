//! Opt-in proof against the actual compiler language-server component.
mod common;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sui::tools::{self, ExecKind, ExecOut, ToolContext};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-code-intel-real-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"semantic_fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::write(
            root.join("Cargo.lock"),
            "version = 4\n\n[[package]]\nname = \"semantic_fixture\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(root.join("src/lib.rs"), SOURCE).unwrap();
        Self(root)
    }
    fn context(&self) -> ToolContext {
        ToolContext {
            workspace: self.0.clone(),
            bash_timeout: Duration::from_secs(1),
            bash_timeout_max: Duration::from_secs(1),
            web: None,
            canon_root: Default::default(),
            ui: Default::default(),
            code_intel: Default::default(),
            code_context: Default::default(),
            tool_outputs: Default::default(),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
const SOURCE: &str = "pub mod first {\n    pub fn same() {}\n}\npub mod second {\n    pub fn same() {}\n}\npub fn usage() {\n    first::same();\n    second::same();\n}\n";

async fn query(ctx: &ToolContext, action: &str) -> ExecOut {
    tools::execute(
        ctx,
        "code_intel",
        &json!({
            "action":action,"path":"src/lib.rs","line":9,"column":13
        }),
        std::future::pending(),
        None,
    )
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires installed rust-analyzer and rust-src compiler components"]
async fn real_rust_queries_resolve_scope_and_refresh_after_native_edits() {
    let fixture = Fixture::new();
    let ctx = fixture.context();
    let definition = query(&ctx, "definition").await;
    assert_eq!(definition.kind, ExecKind::Success, "{}", definition.text);
    assert!(
        definition.text.contains("src/lib.rs:5:12"),
        "{}",
        definition.text
    );
    assert!(
        !definition.text.contains("src/lib.rs:2:12"),
        "{}",
        definition.text
    );
    let references = query(&ctx, "references").await;
    assert_eq!(references.kind, ExecKind::Success, "{}", references.text);
    assert!(
        references.text.contains("src/lib.rs:5:12"),
        "{}",
        references.text
    );
    assert!(
        references.text.contains("src/lib.rs:9:13"),
        "{}",
        references.text
    );
    assert!(
        !references.text.contains("src/lib.rs:2:12"),
        "{}",
        references.text
    );
    assert!(
        !references.text.contains("src/lib.rs:8:12"),
        "{}",
        references.text
    );
    let clean = query(&ctx, "diagnostics").await;
    assert_eq!(clean.kind, ExecKind::Success, "{}", clean.text);
    assert!(clean.text.contains("matches_seen: 0"), "{}", clean.text);
    let broken = format!("{SOURCE}\npub fn broken( {{\n");
    let write = tools::execute(
        &ctx,
        "write_file",
        &json!({"path":"src/lib.rs","content":broken}),
        std::future::pending(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(write.kind, ExecKind::Success);
    let diagnostics = query(&ctx, "diagnostics").await;
    assert_eq!(diagnostics.kind, ExecKind::Success, "{}", diagnostics.text);
    assert!(
        !diagnostics.text.contains("matches_seen: 0"),
        "{}",
        diagnostics.text
    );
    assert!(
        diagnostics.text.contains("src/lib.rs:"),
        "{}",
        diagnostics.text
    );
    assert!(diagnostics.text.contains(" error "), "{}", diagnostics.text);
    // Project metadata is unchanged by observation; source changes are solely
    // the explicitly requested native write_file operation above.
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("Cargo.lock")).unwrap(),
        "version = 4\n\n[[package]]\nname = \"semantic_fixture\"\nversion = \"0.1.0\"\n"
    );
    ctx.code_intel.get().unwrap().invalidate().await;
}

#[tokio::test]
#[ignore = "requires installed rust-analyzer and rust-src compiler components"]
async fn real_standalone_file_switching_rebuilds_the_semantic_graph() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.0.join("Cargo.toml")).unwrap();
    std::fs::remove_file(fixture.0.join("Cargo.lock")).unwrap();
    let other = "pub fn second_value() {}\npub fn use_it() { second_value(); }\n";
    std::fs::write(fixture.0.join("src/other.rs"), other).unwrap();
    let ctx = fixture.context();
    let first = query(&ctx, "definition").await;
    assert_eq!(first.kind, ExecKind::Success, "{}", first.text);
    assert!(first.text.contains("src/lib.rs:5:12"), "{}", first.text);
    assert!(
        first.text.contains("project_mode: detached"),
        "{}",
        first.text
    );
    assert!(
        first.text.contains("analysis_complete: false"),
        "{}",
        first.text
    );
    let column = other.lines().nth(1).unwrap().find("second_value").unwrap() + 1;
    let second = tools::execute(
        &ctx,
        "code_intel",
        &json!({"action":"definition","path":"src/other.rs","line":2,"column":column}),
        std::future::pending(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(second.kind, ExecKind::Success, "{}", second.text);
    assert!(second.text.contains("src/other.rs:1:8"), "{}", second.text);
    assert!(
        second.text.contains("project_mode: detached"),
        "{}",
        second.text
    );
    assert!(
        second.text.contains("analysis_complete: false"),
        "{}",
        second.text
    );
    ctx.code_intel.get().unwrap().invalidate().await;
}

#[tokio::test]
#[ignore = "requires installed rust-analyzer and rust-src compiler components"]
async fn real_orphan_source_is_partial_even_with_a_loaded_cargo_project() {
    let fixture = Fixture::new();
    let ctx = fixture.context();
    let member = query(&ctx, "definition").await;
    assert_eq!(member.kind, ExecKind::Success, "{}", member.text);
    assert!(
        member.text.contains("analysis_complete: true"),
        "{}",
        member.text
    );
    std::fs::write(
        fixture.0.join("orphan.rs"),
        "fn helper() {}\nfn outside_graph() { helper(); missing(); }\n",
    )
    .unwrap();
    let orphan = tools::execute(
        &ctx,
        "code_intel",
        &json!({"action":"diagnostics","path":"orphan.rs"}),
        std::future::pending(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(orphan.kind, ExecKind::Success, "{}", orphan.text);
    assert!(
        orphan.text.contains("project_mode: cargo"),
        "{}",
        orphan.text
    );
    assert!(
        orphan.text.contains("file_in_project: false"),
        "{}",
        orphan.text
    );
    assert!(
        orphan.text.contains("analysis_complete: false"),
        "{}",
        orphan.text
    );
    assert!(
        orphan.truncated,
        "unlinked source cannot prove diagnostic absence"
    );
    ctx.code_intel.get().unwrap().invalidate().await;
}

fn cli_command(root: &Path, port: u16, approved: bool) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_sui"));
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("tmp")).unwrap();
    command
        .current_dir(root)
        .env_clear()
        .env("HOME", root.join("home"))
        .env("TMPDIR", root.join("tmp"))
        .env("SUI_HOME", root.join("sui-state"))
        .env("XDG_CONFIG_HOME", root.join("home/.config"))
        .args([
            "--base-url",
            &format!("http://127.0.0.1:{port}/v1"),
            "--model",
            "code-intel-cli-fixture",
            "--api-key",
            "dummy-code-intel-key",
            "--workspace",
        ])
        .arg(root);
    // Only compiler discovery and locale are inherited. Provider credentials,
    // ambient Sui settings and other session variables never enter this child.
    for key in ["PATH", "RUSTUP_TOOLCHAIN", "LANG", "LC_ALL"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    for (key, directory) in [("RUSTUP_HOME", ".rustup"), ("CARGO_HOME", ".cargo")] {
        if let Some(value) = std::env::var_os(key).or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(directory).into_os_string())
        }) {
            command.env(key, value);
        }
    }
    if approved {
        command.arg("--yes");
    }
    command.arg("Inspect second::same with native code intelligence");
    command
}

fn run_cli(
    mut command: std::process::Command,
    root: &Path,
    deadline: Duration,
) -> std::process::Output {
    use std::process::Stdio;
    // Files avoid filling a pipe while the parent polls the bounded child.
    let stdout = root.join("cli.stdout");
    let stderr = root.join("cli.stderr");
    let mut child = command
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let end = Instant::now() + deadline;
    let completed = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                break false;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("CLI child wait failed: {error}");
            }
        }
    };
    let output = std::process::Output {
        status: child.wait().unwrap(),
        stdout: std::fs::read(stdout).unwrap(),
        stderr: std::fs::read(stderr).unwrap(),
    };
    assert!(
        completed && output.status.success(),
        "CLI failed or exceeded its deadline:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn tool_result(request: &Value, id: &str) -> String {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
        .unwrap()["content"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
#[ignore = "requires installed rust-analyzer and rust-src compiler components"]
fn built_cli_queries_real_definitions_edits_and_diagnostics_with_frozen_headers() {
    let fixture = Fixture::new();
    let changed = format!("// definition moved by native edit\n{SOURCE}\npub fn broken( {{\n");
    let written = changed.clone();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let saved = requests.clone();
    let port = common::serve(move |body, _| {
        let mut requests = saved.lock().unwrap();
        requests.push(serde_json::from_slice(body).unwrap());
        match requests.len() {
            1 => common::sse_tool_calls(json!([common::tc(
                "definition", "code_intel",
                &json!({"action":"definition","path":"src/lib.rs","line":9,"column":13}).to_string(),
            )])),
            2 => common::sse_tool_calls(json!([common::tc(
                "edit", "write_file",
                &json!({"path":"src/lib.rs","content":written}).to_string(),
            )])),
            3 => common::sse_tool_calls(json!([common::tc(
                "fresh_definition", "code_intel",
                &json!({"action":"definition","path":"src/lib.rs","line":10,"column":13}).to_string(),
            )])),
            4 => common::sse_tool_calls(json!([common::tc(
                "diagnostics", "code_intel",
                &json!({"action":"diagnostics","path":"src/lib.rs"}).to_string(),
            )])),
            _ => common::sse_text("state: flow-verified\nverified: native CLI definitions, edits and current diagnostics\nunverified: external provider and compiler acceptance"),
        }
    });
    run_cli(
        cli_command(&fixture.0, port, true),
        &fixture.0,
        Duration::from_secs(120),
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 5);
    let initial = requests[0]["messages"].as_array().unwrap();
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| { tool["function"]["name"] == "code_intel" }));
    for request in &requests[1..] {
        assert_eq!(request["tools"], requests[0]["tools"]);
        assert_eq!(
            &request["messages"].as_array().unwrap()[..initial.len()],
            initial
        );
    }
    let definition = tool_result(&requests[1], "definition");
    assert!(definition.contains("status: success"), "{definition}");
    assert!(definition.contains("src/lib.rs:5:12"), "{definition}");
    assert!(!definition.contains("src/lib.rs:2:12"), "{definition}");
    let edit = tool_result(&requests[2], "edit");
    assert!(edit.contains("status: success"), "{edit}");
    let fresh = tool_result(&requests[3], "fresh_definition");
    assert!(fresh.contains("status: success"), "{fresh}");
    assert!(fresh.contains("src/lib.rs:6:12"), "{fresh}");
    assert!(!fresh.contains("src/lib.rs:5:12"), "{fresh}");
    let diagnostics = tool_result(&requests[4], "diagnostics");
    assert!(diagnostics.contains("status: success"), "{diagnostics}");
    assert!(diagnostics.contains(" error "), "{diagnostics}");
    assert!(!diagnostics.contains("matches_seen: 0"), "{diagnostics}");
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("src/lib.rs")).unwrap(),
        changed
    );
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("Cargo.lock")).unwrap(),
        "version = 4\n\n[[package]]\nname = \"semantic_fixture\"\nversion = \"0.1.0\"\n"
    );
}

#[cfg(unix)]
#[test]
fn built_cli_permission_denial_never_starts_the_language_backend() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let bin = fixture.0.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let sentinel = bin.join("rust-analyzer");
    std::fs::write(
        &sentinel,
        "#!/bin/sh\n: > unexpected-backend-start\nexit 91\n",
    )
    .unwrap();
    std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(0o700)).unwrap();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let saved = requests.clone();
    let port = common::serve(move |body, _| {
        let mut requests = saved.lock().unwrap();
        requests.push(serde_json::from_slice(body).unwrap());
        if requests.len() == 1 {
            common::sse_tool_calls(json!([common::tc(
                "denied",
                "code_intel",
                &json!({"action":"definition","path":"src/lib.rs","line":9,"column":13})
                    .to_string(),
            )]))
        } else {
            common::sse_text(
                "state: blocked\nverified: local permission denied\nunverified: code analysis",
            )
        }
    });
    let mut command = cli_command(&fixture.0, port, false);
    let mut paths = vec![bin];
    if let Some(path) = std::env::var_os("PATH") {
        paths.extend(std::env::split_paths(&path));
    }
    command.env("PATH", std::env::join_paths(paths).unwrap());
    run_cli(command, &fixture.0, Duration::from_secs(10));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let denied = tool_result(&requests[1], "denied");
    assert!(denied.contains("status: denied"), "{denied}");
    assert!(!fixture.0.join("unexpected-backend-start").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("src/lib.rs")).unwrap(),
        SOURCE
    );
    assert_eq!(requests[1]["tools"], requests[0]["tools"]);
}
