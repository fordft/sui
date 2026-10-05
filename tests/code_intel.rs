//! Native code intelligence proof through the production dispatcher.
//!
//! The mock verifies protocol/coordinates/freshness/lifecycle. It does not
//! measure rust-analyzer's semantic accuracy; real-backend proof is separate.
mod common;

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use sui::tools::code_intel::CodeIntelService;
use sui::tools::{self, ExecKind, ExecOut, ToolContext};

const SOURCE: &str = "mod first { pub fn same() {} }\nmod second { pub fn same() {} }\n// second::same() is only a comment\nfn main() { second::same(); }\n";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-code-intel-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixture = Self(root);
        fixture.write(
            "Cargo.toml",
            "[package]\nname = \"code_intel_fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
        );
        fixture.write("src/lib.rs", SOURCE);
        fixture
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
    }

    fn context(&self, mode: &str) -> ToolContext {
        mock_context(&self.0, mode, None)
    }

    fn trace(&self) -> Vec<Value> {
        read_trace(&self.0)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn base_context(root: &Path) -> ToolContext {
    ToolContext {
        workspace: root.to_path_buf(),
        bash_timeout: Duration::from_secs(1),
        bash_timeout_max: Duration::from_secs(1),
        web: None,
        canon_root: Default::default(),
        ui: Default::default(),
        code_intel: Default::default(),
        code_context: Default::default(),
    }
}

fn mock_context(root: &Path, mode: &str, outside: Option<&Path>) -> ToolContext {
    let ctx = base_context(root);
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/code_intel_lsp.py");
    let mut args = vec![
        "-u".to_owned(),
        script.to_string_lossy().into_owned(),
        "--trace".to_owned(),
        root.join("lsp-trace.jsonl").to_string_lossy().into_owned(),
        "--mode".to_owned(),
        mode.to_owned(),
    ];
    if let Some(outside) = outside {
        args.extend([
            "--outside-file".to_owned(),
            outside.to_string_lossy().into_owned(),
        ]);
    }
    let service = CodeIntelService::with_program(
        root.canonicalize().unwrap(),
        PathBuf::from("/usr/bin/python3"),
        args,
    );
    assert!(ctx.code_intel.set(service).is_ok());
    ctx
}

fn read_trace(root: &Path) -> Vec<Value> {
    std::fs::read_to_string(root.join("lsp-trace.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

fn position(source: &str, line_marker: &str, name: &str) -> (usize, usize) {
    source
        .lines()
        .enumerate()
        .find_map(|(index, line)| {
            line.contains(line_marker).then(|| {
                let byte = line.find(name).unwrap();
                (index + 1, line[..byte].chars().count() + 1)
            })
        })
        .unwrap()
}

fn query(action: &str, source: &str, marker: &str) -> Value {
    let (line, column) = position(source, marker, "same");
    json!({"action":action,"path":"src/lib.rs","line":line,"column":column})
}

async fn execute(ctx: &ToolContext, args: Value) -> ExecOut {
    tools::execute(ctx, "code_intel", &args, std::future::pending(), None)
        .await
        .unwrap()
}

fn count(out: &ExecOut, key: &str) -> usize {
    out.text
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{key}: ")))
        .unwrap_or_else(|| panic!("missing {key}: {}", out.text))
        .parse()
        .unwrap()
}

fn assert_complete(out: &ExecOut) {
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert!(out.text.contains("analysis_complete: true"), "{}", out.text);
}

#[cfg(unix)]
fn started_pids(root: &Path) -> Vec<i32> {
    read_trace(root)
        .iter()
        .filter(|entry| entry["event"] == "started")
        .map(|entry| entry["pid"].as_i64().unwrap() as i32)
        .collect()
}

#[tokio::test]
async fn definitions_and_references_use_the_backend_semantic_location() {
    let fixture = Fixture::new();
    let ctx = fixture.context("normal");
    let definition = execute(&ctx, query("definition", SOURCE, "fn main")).await;
    assert_complete(&definition);
    let (line, column) = position(SOURCE, "mod second", "same");
    assert!(
        definition
            .text
            .contains(&format!("src/lib.rs:{line}:{column}")),
        "{}",
        definition.text
    );
    assert!(!definition.text.contains("src/lib.rs:1:"));
    assert!(!definition.text.contains("src/lib.rs:3:"));
    let references = execute(&ctx, query("references", SOURCE, "fn main")).await;
    assert_complete(&references);
    let (line, column) = position(SOURCE, "fn main", "same");
    assert!(references
        .text
        .contains(&format!("src/lib.rs:{line}:{column}")));
    assert!(!references.text.contains("src/lib.rs:1:"));
    assert!(!references.text.contains("src/lib.rs:3:"));
    let trace = fixture.trace();
    assert!(trace
        .iter()
        .any(|entry| entry["method"] == "textDocument/definition"));
    assert!(trace
        .iter()
        .any(|entry| entry["method"] == "textDocument/references"));
}

#[tokio::test]
async fn transient_analysis_errors_retry_the_same_query_without_restarting() {
    let fixture = Fixture::new();
    let ctx = fixture.context("transient");
    let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
    assert_complete(&out);
    assert!(out.text.contains("src/lib.rs:2:21-2:25"), "{}", out.text);
    let trace = fixture.trace();
    let requests: Vec<_> = trace
        .iter()
        .filter(|entry| entry["method"] == "textDocument/definition")
        .collect();
    assert_eq!(
        requests.len(),
        3,
        "both retryable error codes must be retried"
    );
    for retry in &requests[1..] {
        assert_eq!(retry["method"], requests[0]["method"]);
        assert_eq!(retry["params"], requests[0]["params"]);
    }
    let ids: std::collections::BTreeSet<_> = requests
        .iter()
        .map(|entry| entry["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), requests.len(), "retries need fresh RPC IDs");
    for method in ["initialize", "textDocument/didOpen"] {
        assert_eq!(
            trace
                .iter()
                .filter(|entry| entry["method"] == method)
                .count(),
            1,
            "retry must reuse its initialized backend and document"
        );
    }
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry["event"] == "started")
            .count(),
        1
    );
    assert!(!trace
        .iter()
        .any(|entry| entry["method"] == "textDocument/didChange"));
}

#[cfg(unix)]
#[tokio::test]
async fn permanent_analysis_changes_have_a_retry_cap_and_reap_the_backend() {
    let fixture = Fixture::new();
    let ctx = fixture.context("permanent_transient");
    let out = tokio::time::timeout(
        Duration::from_secs(3),
        execute(&ctx, query("definition", SOURCE, "fn main")),
    )
    .await
    .expect("permanent transient errors need a bounded retry count");
    assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
    assert!(!out.text.contains("analysis_complete: true"));
    let trace = fixture.trace();
    let requests: Vec<_> = trace
        .iter()
        .filter(|entry| entry["method"] == "textDocument/definition")
        .collect();
    assert_eq!(requests.len(), 4, "one attempt plus at most three retries");
    for retry in &requests[1..] {
        assert_eq!(retry["params"], requests[0]["params"]);
    }
    let started: Vec<_> = trace
        .iter()
        .filter(|entry| entry["event"] == "started")
        .collect();
    assert_eq!(started.len(), 1);
    let pid = started[0]["pid"].as_i64().unwrap() as i32;
    assert!(
        !process_alive(pid),
        "retry exhaustion must kill and reap backend {pid}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn non_retryable_lsp_errors_are_not_retried_and_reap_the_backend() {
    for mode in ["cancel_no_retrigger", "invalid_query"] {
        let fixture = Fixture::new();
        let ctx = fixture.context(mode);
        let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
        assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
        assert!(!out.text.contains("analysis_complete: true"));
        let trace = fixture.trace();
        assert_eq!(
            trace
                .iter()
                .filter(|entry| entry["method"] == "textDocument/definition")
                .count(),
            1,
            "{mode} must not authorize a retry"
        );
        let started: Vec<_> = trace
            .iter()
            .filter(|entry| entry["event"] == "started")
            .collect();
        assert_eq!(started.len(), 1);
        let pid = started[0]["pid"].as_i64().unwrap() as i32;
        assert!(
            !process_alive(pid),
            "non-retryable error must reap backend {pid}"
        );
    }
}

#[tokio::test]
async fn fresh_source_is_sent_as_versioned_did_change() {
    let fixture = Fixture::new();
    let ctx = fixture.context("normal");
    assert_complete(&execute(&ctx, query("definition", SOURCE, "fn main")).await);
    let changed = format!("// source moved\n\n{SOURCE}");
    fixture.write("src/lib.rs", &changed);
    let out = execute(&ctx, query("definition", &changed, "fn main")).await;
    assert_complete(&out);
    let (line, column) = position(&changed, "mod second", "same");
    assert!(
        out.text.contains(&format!("src/lib.rs:{line}:{column}")),
        "{}",
        out.text
    );
    assert!(!out.text.contains("src/lib.rs:2:"));
    let trace = fixture.trace();
    let opened = trace
        .iter()
        .find(|entry| entry["method"] == "textDocument/didOpen")
        .unwrap();
    let updated = trace
        .iter()
        .find(|entry| entry["method"] == "textDocument/didChange")
        .unwrap();
    assert!(
        updated["params"]["textDocument"]["version"]
            .as_u64()
            .unwrap()
            > opened["params"]["textDocument"]["version"]
                .as_u64()
                .unwrap()
    );
    assert_eq!(updated["params"]["contentChanges"][0]["text"], changed);
}

#[tokio::test]
async fn unicode_scalar_columns_are_translated_to_and_from_utf16() {
    let fixture = Fixture::new();
    let source = "mod first { pub fn same() {} }\nmod second { const _: &str = \"🐈\"; pub fn same() {} }\nfn main() { let _ = \"🐈\"; second::same(); }\n";
    fixture.write("src/lib.rs", source);
    let ctx = fixture.context("normal");
    let out = execute(&ctx, query("definition", source, "fn main")).await;
    assert_complete(&out);
    let (line, column) = position(source, "mod second", "same");
    assert!(
        out.text.contains(&format!("src/lib.rs:{line}:{column}")),
        "{}",
        out.text
    );
    let main = source
        .lines()
        .find(|line| line.contains("fn main"))
        .unwrap();
    let expected_utf16 = main[..main.find("same").unwrap()].encode_utf16().count();
    let trace = fixture.trace();
    let request = trace
        .iter()
        .find(|entry| entry["method"] == "textDocument/definition")
        .unwrap();
    assert_eq!(
        request["params"]["position"]["character"].as_u64().unwrap(),
        expected_utf16 as u64
    );
}

#[tokio::test]
async fn diagnostics_are_current_and_an_empty_report_is_honest() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "fn main() { missing; }\n");
    let ctx = fixture.context("normal");
    let args = json!({"action":"diagnostics","path":"src/lib.rs"});
    let broken = execute(&ctx, args.clone()).await;
    assert_complete(&broken);
    assert_eq!(count(&broken, "matches_seen"), 1);
    assert!(broken.text.contains("missing"), "{}", broken.text);
    assert!(broken.text.contains("src/lib.rs:1:13"), "{}", broken.text);
    fixture.write("src/lib.rs", "fn main() {}\n");
    let clean = execute(&ctx, args).await;
    assert_complete(&clean);
    assert_eq!(count(&clean, "matches_seen"), 0);
    assert!(!clean.text.contains("cannot find value"));
    assert!(fixture
        .trace()
        .iter()
        .any(|entry| entry["method"] == "textDocument/didChange"));
}

#[tokio::test]
async fn absent_or_unknown_file_membership_keeps_cargo_observations_partial() {
    for (mode, membership) in [("orphan", "false"), ("membership_unsupported", "unknown")] {
        let fixture = Fixture::new();
        let ctx = fixture.context(mode);
        let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
        assert_eq!(out.kind, ExecKind::Success, "{mode}: {}", out.text);
        assert!(
            out.text.contains("analysis_complete: false"),
            "{}",
            out.text
        );
        assert!(
            out.text.contains(&format!("file_in_project: {membership}")),
            "{}",
            out.text
        );
        assert!(out.truncated, "unproven membership must be partial");
        assert!(out.text.contains("src/lib.rs:2:21-2:25"), "{}", out.text);
        let trace = fixture.trace();
        let membership = trace
            .iter()
            .find(|entry| entry["method"] == "experimental/openCargoToml")
            .expect("a Cargo root alone cannot establish source membership");
        let uri = membership["params"]["textDocument"]["uri"]
            .as_str()
            .unwrap();
        assert_eq!(
            reqwest::Url::parse(uri).unwrap().to_file_path().unwrap(),
            fixture.0.join("src/lib.rs").canonicalize().unwrap()
        );
    }
}

#[tokio::test]
async fn valid_location_links_render_the_selection_inside_the_target_range() {
    let fixture = Fixture::new();
    let ctx = fixture.context("location_link");
    let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
    assert_complete(&out);
    assert_eq!(count(&out, "showing"), 1);
    assert!(out.text.contains("src/lib.rs:2:21-2:25"), "{}", out.text);
}

#[tokio::test]
async fn ready_metadata_cannot_promote_an_earlier_loading_semantic_result() {
    let fixture = Fixture::new();
    let ctx = fixture.context("loading_then_ready");
    let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert!(out.text.contains("src/lib.rs:2:21-2:25"), "{}", out.text);
    assert!(out.text.contains("file_in_project: true"), "{}", out.text);
    assert!(
        out.text.contains("analysis_complete: false"),
        "{}",
        out.text
    );
    assert!(
        out.truncated,
        "loading semantic coverage must remain partial"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn invalid_semantic_payloads_error_reap_and_restart_even_after_row_limit() {
    for (mode, action) in [
        ("semantic_bad_unchanged", "diagnostics"),
        ("semantic_bad_message", "diagnostics"),
        ("semantic_bad_scalar", "definition"),
        ("semantic_bad_references_object", "references"),
        ("semantic_bad_link_range", "definition"),
        ("semantic_bad_link_selection", "definition"),
        ("semantic_bad_link_outside", "definition"),
        ("semantic_bad_after_limit", "references"),
    ] {
        let fixture = Fixture::new();
        let ctx = fixture.context(mode);
        let mut args = query(action, SOURCE, "fn main");
        args["limit"] = json!(1);
        let invalid = tokio::time::timeout(Duration::from_secs(3), execute(&ctx, args.clone()))
            .await
            .expect("invalid semantic payload handling must be bounded");
        assert_eq!(invalid.kind, ExecKind::Error, "{mode}: {}", invalid.text);
        assert!(!invalid.text.contains("analysis_complete: true"));
        let first = started_pids(&fixture.0);
        assert_eq!(first.len(), 1, "{mode}");
        assert!(
            !process_alive(first[0]),
            "{mode}: invalid session was retained"
        );
        let recovered = execute(&ctx, args).await;
        assert_eq!(
            recovered.kind,
            ExecKind::Success,
            "{mode}: {}",
            recovered.text
        );
        let restarted = started_pids(&fixture.0);
        assert_eq!(
            restarted.len(),
            2,
            "{mode}: next query must initialize anew"
        );
        assert_ne!(restarted[0], restarted[1]);
        assert_eq!(
            fixture
                .trace()
                .iter()
                .filter(|entry| entry["method"] == "initialize")
                .count(),
            2,
            "{mode}"
        );
    }
}

#[tokio::test]
async fn output_limits_bound_semantic_reference_results() {
    let fixture = Fixture::new();
    let mut source = SOURCE.to_owned();
    for n in 0..250 {
        source.push_str(&format!("fn call_{n:03}() {{ second::same(); }}\n"));
    }
    fixture.write("src/lib.rs", &source);
    let ctx = fixture.context("normal");
    let mut args = query("references", &source, "fn main");
    args["limit"] = json!(3);
    let out = execute(&ctx, args).await;
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert!(out.truncated, "{}", out.text);
    assert!(count(&out, "matches_seen") >= 251);
    assert_eq!(count(&out, "showing"), 3);
    assert!(out.text.len() < 25 * 1024, "{} bytes", out.text.len());
    assert!(!out.text.contains("fn call_"));
}

#[tokio::test]
async fn invalid_arguments_and_paths_are_rejected_before_backend_start() {
    let fixture = Fixture::new();
    fixture.write("client.ts", "function same() {}\n");
    fixture.write(".env.rs", SOURCE);
    let outside = Fixture::new();
    let ctx = fixture.context("normal");
    let (_, column) = position(SOURCE, "fn main", "same");
    let cases = [
        json!({}),
        json!({"action":"rename","path":"src/lib.rs","line":4,"column":column}),
        json!({"action":"definition","path":"src/lib.rs"}),
        json!({"action":"references","path":"src/lib.rs","line":4}),
        json!({"action":"definition","path":123,"line":4,"column":column}),
        json!({"action":"definition","path":"src/lib.rs","line":0,"column":column}),
        json!({"action":"definition","path":"src/lib.rs","line":4,"column":0}),
        json!({"action":"definition","path":"src/lib.rs","line":9999,"column":column}),
        json!({"action":"definition","path":"src/lib.rs","line":4,"column":9999}),
        json!({"action":"definition","path":"src/lib.rs","line":4,"column":column,"limit":0}),
        json!({"action":"definition","path":"src/lib.rs","line":4,"column":column,"limit":201}),
        json!({"action":"definition","path":"src/lib.rs","line":4,"column":column,"unknown":true}),
        json!({"action":"definition","path":"client.ts","line":1,"column":10}),
        json!({"action":"definition","path":".env.rs","line":4,"column":column}),
        json!({"action":"definition","path":outside.0.join("src/lib.rs"),"line":4,"column":column}),
        json!({"action":"definition","path":"../outside.rs","line":4,"column":column}),
    ];
    for args in cases {
        let out = tools::execute(&ctx, "code_intel", &args, std::future::pending(), None).await;
        if let Ok(out) = out {
            assert_eq!(out.kind, ExecKind::Error, "{args}: {}", out.text);
            assert!(!out.text.contains("analysis_complete: true"));
        }
        assert!(
            fixture.trace().is_empty(),
            "denied argument started a backend: {args}"
        );
    }
    #[cfg(unix)]
    for (name, target) in [
        ("alias.rs", fixture.0.join("src/lib.rs")),
        ("escape.rs", outside.0.join("src/lib.rs")),
    ] {
        std::os::unix::fs::symlink(target, fixture.0.join(name)).unwrap();
        let args = json!({"action":"definition","path":name,"line":4,"column":column});
        let out = tools::execute(&ctx, "code_intel", &args, std::future::pending(), None).await;
        if let Ok(out) = out {
            assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
        }
        assert!(fixture.trace().is_empty());
    }
}

#[tokio::test]
async fn backend_instances_are_isolated_by_tool_context() {
    let first = Fixture::new();
    let second = Fixture::new();
    let shifted = format!("// another workspace\n\n\n{SOURCE}");
    second.write("src/lib.rs", &shifted);
    let first_ctx = first.context("normal");
    let second_ctx = second.context("normal");
    let (first_out, second_out) = tokio::join!(
        execute(&first_ctx, query("definition", SOURCE, "fn main")),
        execute(&second_ctx, query("definition", &shifted, "fn main")),
    );
    assert_complete(&first_out);
    assert_complete(&second_out);
    let (first_line, first_column) = position(SOURCE, "mod second", "same");
    let (second_line, second_column) = position(&shifted, "mod second", "same");
    assert!(first_out
        .text
        .contains(&format!("src/lib.rs:{first_line}:{first_column}")));
    assert!(second_out
        .text
        .contains(&format!("src/lib.rs:{second_line}:{second_column}")));
    for fixture in [&first, &second] {
        let trace = fixture.trace();
        let initialized = trace
            .iter()
            .find(|entry| entry["method"] == "initialize")
            .unwrap();
        let uri = initialized["params"]["rootUri"].as_str().unwrap();
        let root = reqwest::Url::parse(uri).unwrap().to_file_path().unwrap();
        assert_eq!(
            root.canonicalize().unwrap(),
            fixture.0.canonicalize().unwrap()
        );
    }
}

#[tokio::test]
async fn switching_detached_files_reinitializes_the_graph_and_stays_partial() {
    let fixture = Fixture::new();
    std::fs::remove_file(fixture.0.join("Cargo.toml")).unwrap();
    let shifted = format!("// detached second file\n\n{SOURCE}");
    fixture.write("src/a.rs", SOURCE);
    fixture.write("src/b.rs", &shifted);
    let ctx = fixture.context("normal");
    for (path, source) in [("src/a.rs", SOURCE), ("src/b.rs", shifted.as_str())] {
        let mut args = query("definition", source, "fn main");
        args["path"] = json!(path);
        let out = execute(&ctx, args).await;
        assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
        assert!(
            out.truncated,
            "detached graph cannot prove completeness: {}",
            out.text
        );
        assert!(
            out.text.contains("analysis_complete: false"),
            "{}",
            out.text
        );
        let (line, column) = position(source, "mod second", "same");
        assert!(
            out.text.contains(&format!("{path}:{line}:{column}")),
            "{}",
            out.text
        );
    }
    let trace = fixture.trace();
    let initialized: Vec<_> = trace
        .iter()
        .filter(|entry| entry["method"] == "initialize")
        .collect();
    assert_eq!(
        initialized.len(),
        2,
        "a new detached file needs a fresh graph"
    );
    for (request, source) in initialized.iter().zip(["src/a.rs", "src/b.rs"]) {
        let detached = request["params"]["initializationOptions"]["detachedFiles"]
            .as_array()
            .unwrap();
        assert_eq!(detached.len(), 1);
        assert_eq!(
            Path::new(detached[0].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            fixture.0.join(source).canonicalize().unwrap(),
            "initialization must select the current detached source"
        );
    }
    assert_eq!(
        trace
            .iter()
            .filter(|entry| entry["event"] == "started")
            .count(),
        2
    );
}

#[tokio::test]
async fn outside_workspace_backend_locations_are_not_exposed_as_proof() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    outside.write("private-target.rs", SOURCE);
    let ctx = mock_context(
        &fixture.0,
        "outside",
        Some(&outside.0.join("private-target.rs")),
    );
    let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert!(out.truncated, "{}", out.text);
    assert!(
        out.text.contains("analysis_complete: false"),
        "{}",
        out.text
    );
    assert!(!out.text.contains("private-target.rs"), "{}", out.text);
    assert_eq!(count(&out, "showing"), 0);
}

#[cfg(unix)]
#[tokio::test]
async fn backend_uris_to_symlinks_or_sensitive_files_are_filtered() {
    for case in ["sensitive", "inside_alias", "outside_alias"] {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let (mode, target) = match case {
            "inside_alias" => ("outside_symlink_after_scan", fixture.0.join("src/lib.rs")),
            "outside_alias" => ("outside_symlink_after_scan", outside.0.join("src/lib.rs")),
            _ => {
                fixture.write(".env.rs", SOURCE);
                ("outside", fixture.0.join(".env.rs"))
            }
        };
        let ctx = mock_context(&fixture.0, mode, Some(&target));
        let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
        assert_eq!(out.kind, ExecKind::Success, "{case}: {}", out.text);
        assert!(out.truncated, "{}", out.text);
        assert!(
            out.text.contains("analysis_complete: false"),
            "{}",
            out.text
        );
        assert_eq!(count(&out, "showing"), 0);
        assert!(!out.text.contains("alias.rs:"));
        assert!(!out.text.contains(".env.rs:"));
        assert!(fixture
            .trace()
            .iter()
            .any(|entry| entry["method"] == "textDocument/definition"));
        if case != "sensitive" {
            assert_eq!(
                std::fs::read_link(fixture.0.join("alias.rs")).unwrap(),
                target
            );
            assert!(fixture
                .trace()
                .iter()
                .any(|entry| entry["event"] == "alias_created_after_launch"));
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn preexisting_visible_symlinks_deny_configuration_coverage_before_backend_start() {
    for inside in [false, true] {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        let target = if inside {
            fixture.0.join("src/lib.rs")
        } else {
            outside.0.join("src/lib.rs")
        };
        std::os::unix::fs::symlink(target, fixture.0.join("alias.rs")).unwrap();
        let ctx = fixture.context("normal");
        let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
        assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
        assert!(!out.text.contains("analysis_complete: true"));
        assert!(
            fixture.trace().is_empty(),
            "unproven configuration coverage launched LSP"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn malformed_or_oversized_lsp_frames_error_and_reap_before_body_read() {
    for mode in ["malformed", "oversize_frame"] {
        let fixture = Fixture::new();
        let ctx = fixture.context(mode);
        let out = tokio::time::timeout(
            Duration::from_secs(3),
            execute(&ctx, query("definition", SOURCE, "fn main")),
        )
        .await
        .expect("malformed or oversize header handling must not await a body");
        assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
        assert!(!out.text.contains("analysis_complete: true"));
        let trace = fixture.trace();
        let started = trace
            .iter()
            .find(|entry| entry["event"] == "started")
            .unwrap();
        let pid = started["pid"].as_i64().unwrap() as i32;
        assert!(
            !process_alive(pid),
            "protocol failure must kill and reap backend {pid}"
        );
    }
}

#[tokio::test]
async fn workspace_analyzer_configuration_is_rejected_before_backend_start() {
    let fixture = Fixture::new();
    fixture.write(
        "rust-analyzer.toml",
        "[cargo.buildScripts]\nenable = true\n[procMacro]\nenable = true\n",
    );
    let ctx = fixture.context("normal");
    let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
    assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
    assert!(!out.text.contains("analysis_complete: true"));
    assert!(
        fixture.trace().is_empty(),
        "unsafe config started a backend"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn nested_analyzer_configuration_is_denied_before_launch_and_after_warming() {
    for warm in [false, true] {
        let fixture = Fixture::new();
        let ctx = fixture.context("normal");
        let args = query("definition", SOURCE, "fn main");
        if warm {
            assert_complete(&execute(&ctx, args.clone()).await);
        }
        fixture.write(
            "src/rust-analyzer.toml",
            "[diagnostics]\nenable = false\n[cargo.buildScripts]\nenable = true\n",
        );
        let denied = execute(&ctx, args).await;
        assert_eq!(denied.kind, ExecKind::Error, "{}", denied.text);
        assert!(!denied.text.contains("analysis_complete: true"));
        let pids = started_pids(&fixture.0);
        assert_eq!(
            pids.len(),
            usize::from(warm),
            "unsafe config spawned a backend"
        );
        assert!(pids.iter().all(|pid| !process_alive(*pid)));
        assert_eq!(
            fixture
                .trace()
                .iter()
                .filter(|entry| entry["method"] == "textDocument/definition")
                .count(),
            usize::from(warm),
            "new configuration must be rejected before any semantic request"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn already_ready_cancellation_reaps_a_warmed_backend_before_returning() {
    let fixture = Fixture::new();
    let ctx = fixture.context("normal");
    let args = query("definition", SOURCE, "fn main");
    assert_complete(&execute(&ctx, args.clone()).await);
    let first = started_pids(&fixture.0);
    assert_eq!(first.len(), 1);
    assert!(process_alive(first[0]));
    let cancelled = tools::execute(&ctx, "code_intel", &args, std::future::ready(()), None)
        .await
        .unwrap();
    assert_eq!(cancelled.kind, ExecKind::Cancelled, "{}", cancelled.text);
    assert!(
        !process_alive(first[0]),
        "ready cancel retained warmed backend"
    );
    assert_eq!(
        started_pids(&fixture.0).len(),
        1,
        "cancel spawned a backend"
    );
    assert_complete(&execute(&ctx, args).await);
    let restarted = started_pids(&fixture.0);
    assert_eq!(restarted.len(), 2);
    assert_ne!(restarted[0], restarted[1]);
}

#[tokio::test]
async fn native_agent_queries_semantics_then_reads_with_frozen_headers() {
    let fixture = Fixture::new();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let saved = requests.clone();
    let position = query("definition", SOURCE, "fn main").to_string();
    let port = common::serve(move |body, _| {
        let mut requests = saved.lock().unwrap();
        requests.push(serde_json::from_slice(body).unwrap());
        match requests.len() {
            1 => common::sse_tool_calls(json!([common::tc("semantic", "code_intel", &position)])),
            2 => common::sse_tool_calls(json!([common::tc("read", "read_file", r#"{"path":"src/lib.rs","offset":2,"limit":1}"#)])),
            _ => common::sse_text("second::same is defined in the second module\nstate: flow-verified\nverified: requested semantic target and read its source\nunverified: real rust-analyzer accuracy and compiler checks"),
        }
    });
    let mut agent = sui::agent::Agent::new(
        sui::provider::Provider::new(
            &format!("http://127.0.0.1:{port}"),
            None,
            "code-intel-fixture".into(),
            None,
        ),
        fixture.context("transient"),
        sui::permission::Gate::new(true),
        sui::journal::Journal::open(&fixture.0.join("run")).unwrap(),
        sui::agent::Limits {
            max_turns: 4,
            context_budget: 50000,
            context_reserve: 1000,
            compact_context: false,
            request_timeout: Duration::from_secs(5),
        },
        sui::agent::Identity {
            session_id: "code-intel-test".into(),
            agent_id: "worker".into(),
            role: "worker".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            model: "code-intel-fixture".into(),
            cache_key_fingerprint: None,
        },
    );
    agent.set_quiet(true);
    agent
        .run_turn("Locate the definition of second::same and read it")
        .await
        .unwrap();
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "code_intel"));
    let initial = requests[0]["messages"].as_array().unwrap();
    for request in &requests[1..] {
        assert_eq!(request["tools"], requests[0]["tools"]);
        assert_eq!(request["messages"][0], requests[0]["messages"][0]);
        assert_eq!(
            &request["messages"].as_array().unwrap()[..initial.len()],
            initial
        );
    }
    let result = |request: &Value, id: &str| {
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .unwrap()["content"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    assert!(result(&requests[1], "semantic").contains("src/lib.rs:2:21-2:25"));
    assert!(result(&requests[1], "semantic").contains("analysis_complete: true"));
    assert!(result(&requests[2], "read").contains("mod second"));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("src/lib.rs")).unwrap(),
        SOURCE
    );
}

#[cfg(unix)]
#[tokio::test]
async fn stopping_during_provider_wait_reaps_the_idle_backend_with_agent_alive() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let fixture = Fixture::new();
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let saved = requests.clone();
    let arguments = query("definition", SOURCE, "fn main").to_string();
    let (release, blocked) = std::sync::mpsc::channel::<()>();
    let port = common::serve(move |body, _| {
        let number = {
            let mut requests = saved.lock().unwrap();
            requests.push(serde_json::from_slice(body).unwrap());
            requests.len()
        };
        if number == 1 {
            common::sse_tool_calls(json!([common::tc("semantic", "code_intel", &arguments)]))
        } else {
            // Keep the provider request active until Stop has been handled.
            // A finite fallback also prevents a failed assertion leaking a
            // permanently blocked fixture thread.
            let _ = blocked.recv_timeout(Duration::from_secs(5));
            common::sse_text("provider reply released after Stop")
        }
    });
    let mut agent = sui::agent::Agent::new(
        sui::provider::Provider::new(
            &format!("http://127.0.0.1:{port}"),
            None,
            "code-intel-fixture".into(),
            None,
        ),
        fixture.context("normal"),
        sui::permission::Gate::new(true),
        sui::journal::Journal::open(&fixture.0.join("run")).unwrap(),
        sui::agent::Limits {
            max_turns: 3,
            context_budget: 50000,
            context_reserve: 1000,
            compact_context: false,
            request_timeout: Duration::from_secs(5),
        },
        sui::agent::Identity {
            session_id: "code-intel-stop-test".into(),
            agent_id: "worker".into(),
            role: "worker".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            model: "code-intel-fixture".into(),
            cache_key_fingerprint: None,
        },
    );
    agent.set_quiet(true);
    let (sink, _events) = tokio::sync::mpsc::unbounded_channel();
    let cancel = Arc::new(tokio::sync::Notify::new());
    let stop = Arc::new(AtomicBool::new(false));
    agent.wire_ui(sink, cancel.clone(), stop.clone(), None);
    let interrupt = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if requests.lock().unwrap().len() == 2 {
                let pids = started_pids(&fixture.0);
                assert_eq!(pids.len(), 1);
                assert!(process_alive(pids[0]), "backend was not warmed before Stop");
                stop.store(true, Ordering::SeqCst);
                cancel.notify_one();
                return pids[0];
            }
            assert!(
                Instant::now() < deadline,
                "agent never waited on second provider request"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let (outcome, pid) = tokio::time::timeout(Duration::from_secs(4), async {
        tokio::join!(agent.run_turn("Find second::same"), interrupt)
    })
    .await
    .expect("provider Stop and backend cleanup must finish promptly");
    let _ = release.send(());
    outcome.unwrap();
    // Agent is intentionally still alive here: Drop cannot provide this proof.
    assert!(!process_alive(pid), "Stop retained idle backend {pid}");
    let requests = requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "Stop must not issue another provider request"
    );
    assert_eq!(requests[1]["tools"], requests[0]["tools"]);
    let initial = requests[0]["messages"].as_array().unwrap();
    assert_eq!(
        &requests[1]["messages"].as_array().unwrap()[..initial.len()],
        initial
    );
    assert!(requests[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["role"] == "tool"
            && message["tool_call_id"] == "semantic"
            && message["content"]
                .as_str()
                .unwrap()
                .contains("src/lib.rs:2:21-2:25")));
}

#[tokio::test]
async fn unavailable_backends_return_errors_without_lexical_fallback() {
    let fixture = Fixture::new();
    let ctx = base_context(&fixture.0);
    let service = CodeIntelService::with_program(
        fixture.0.canonicalize().unwrap(),
        fixture.0.join("missing-rust-analyzer"),
        Vec::new(),
    );
    assert!(ctx.code_intel.set(service).is_ok());
    let out = execute(&ctx, query("definition", SOURCE, "fn main")).await;
    assert_eq!(out.kind, ExecKind::Error, "{}", out.text);
    assert!(!out.text.contains("analysis_complete: true"));
    assert!(!out.text.contains("src/lib.rs:2:"));
    assert!(fixture.trace().is_empty());
}

#[cfg(unix)]
fn process_alive(pid: i32) -> bool {
    // Fixture-reported PID is inside this test's process namespace and owned by
    // this test. Signal 0 observes liveness without modifying any process.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(unix)]
async fn exercise_cancel_restart_and_drop(root: &Path) {
    let ctx = mock_context(root, "hang_once", None);
    let args = query("definition", SOURCE, "fn main");
    let cancel = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if read_trace(root)
                .iter()
                .any(|entry| entry["event"] == "query_hanging")
            {
                return;
            }
            assert!(Instant::now() < deadline, "mock query never started");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    let cancelled = tools::execute(&ctx, "code_intel", &args, cancel, None)
        .await
        .unwrap();
    assert_eq!(cancelled.kind, ExecKind::Cancelled, "{}", cancelled.text);
    let trace = read_trace(root);
    let started: Vec<_> = trace
        .iter()
        .filter(|entry| entry["event"] == "started")
        .collect();
    assert_eq!(started.len(), 1);
    let first_pid = started[0]["pid"].as_i64().unwrap() as i32;
    assert!(
        !process_alive(first_pid),
        "cancel must kill and reap backend {first_pid}"
    );
    let restarted = execute(&ctx, args).await;
    assert_complete(&restarted);
    let trace = read_trace(root);
    let started: Vec<_> = trace
        .iter()
        .filter(|entry| entry["event"] == "started")
        .collect();
    assert_eq!(started.len(), 2);
    let second_pid = started[1]["pid"].as_i64().unwrap() as i32;
    drop(ctx);
    let deadline = Instant::now() + Duration::from_secs(1);
    while process_alive(second_pid) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !process_alive(second_pid),
        "context drop must reap backend {second_pid}"
    );
}

#[cfg(unix)]
async fn exercise_large_response_cancel_and_runtime_join() {
    let fixture = Fixture::new();
    let source = format!(
        "mod second {{ pub fn same() {{}} }} /*{}*/ fn other() {{ second::same(); }}\n",
        "x".repeat(400_000)
    );
    fixture.write("src/lib.rs", &source);
    let ctx = fixture.context("large_refs");
    let mut args = query("references", &source, "mod second");
    args["limit"] = json!(1);
    let cancel = async {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if fixture
                .trace()
                .iter()
                .any(|entry| entry["event"] == "response_sent")
            {
                // The marker follows both semantic and membership replies.
                // Allow decoding to finish before attempting render cancellation.
                tokio::time::sleep(Duration::from_millis(10)).await;
                return;
            }
            assert!(
                Instant::now() < deadline,
                "large fixture reply was never flushed"
            );
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    let started = Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        tools::execute(&ctx, "code_intel", &args, cancel, None),
    )
    .await
    .expect("large response rendering must join promptly despite limit one")
    .unwrap();
    assert!(started.elapsed() < Duration::from_secs(4));
    let pids = started_pids(&fixture.0);
    assert_eq!(pids.len(), 1);
    match result.kind {
        ExecKind::Cancelled => {
            assert!(
                !process_alive(pids[0]),
                "late cancellation retained backend"
            );
            let next = execute(&ctx, query("diagnostics", &source, "mod second")).await;
            assert_complete(&next);
            assert_eq!(
                started_pids(&fixture.0).len(),
                2,
                "cancel needs fresh initialization"
            );
        }
        ExecKind::Success => {
            // An optimized renderer may finish before the flushed-response
            // marker is observed. Fast, bounded completion is equally valid.
            assert!(result.truncated, "{}", result.text);
            assert_eq!(count(&result, "matches_seen"), 5000);
            assert_eq!(count(&result, "showing"), 1);
            assert!(result.text.len() < 25 * 1024);
        }
        other => panic!("unexpected result {other:?}: {}", result.text),
    }
    ctx.code_intel.get().unwrap().invalidate().await;
    assert!(started_pids(&fixture.0)
        .iter()
        .all(|pid| !process_alive(*pid)));
}

#[cfg(unix)]
const LIFECYCLE_CHILD_ROOT: &str = "SUI_CODE_INTEL_LIFECYCLE_CHILD_ROOT";

#[cfg(unix)]
#[test]
fn cancellation_restart_and_runtime_drop_are_bounded() {
    use std::process::{Command, Stdio};
    let fixture = Fixture::new();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "code_intel_lifecycle_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(&fixture.0)
        .env_clear()
        .env(LIFECYCLE_CHILD_ROOT, &fixture.0)
        .env("TMPDIR", &fixture.0)
        .env("HOME", &fixture.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                break false;
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("cannot wait for lifecycle child: {error}");
            }
        }
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "code-intel lifecycle did not complete:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("code intel runtime drained"));
}

#[cfg(unix)]
#[test]
fn code_intel_lifecycle_child() {
    let Some(root) = std::env::var_os(LIFECYCLE_CHILD_ROOT) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(exercise_cancel_restart_and_drop(Path::new(&root)));
    runtime.block_on(exercise_large_response_cancel_and_runtime_join());
    drop(runtime);
    println!("code intel runtime drained");
}
