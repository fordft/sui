//! Task-directed retrieval proof through the production dispatcher and CLI.
//! Offline byte/read measurements are local observations, not provider tokens
//! or cache-hit claims.
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
            "sui-code-context-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, path: &str, source: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, source).unwrap();
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

async fn execute(ctx: &ToolContext, args: Value) -> ExecOut {
    tools::execute(ctx, "code_context", &args, std::future::pending(), None)
        .await
        .unwrap()
}

fn counter(out: &ExecOut, name: &str) -> usize {
    out.text
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("missing {name}: {}", out.text))
        .parse()
        .unwrap()
}

fn hashes(out: &ExecOut) -> Vec<&str> {
    let hashes: Vec<_> = out
        .text
        .lines()
        .filter_map(|line| line.strip_prefix("source_sha256: "))
        .collect();
    for hash in &hashes {
        assert_eq!(hash.len(), 64, "{hash}");
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()), "{hash}");
    }
    hashes
}

fn line_number(source: &str, needle: &str) -> usize {
    source
        .lines()
        .position(|line| line.contains(needle))
        .unwrap()
        + 1
}

fn assert_line(out: &ExecOut, number: usize, source: &str) {
    let expected = format!("{number} | {source}");
    assert!(
        out.text.lines().any(|line| line == expected),
        "missing exact source line {expected:?}: {}",
        out.text
    );
}

fn assert_success(out: &ExecOut) {
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert!(out.text.starts_with("status: success\n"), "{}", out.text);
}

const ANCHORED: &str = "use std::time::Duration;\n\npub struct FirstBudget;\nimpl FirstBudget {\n    pub fn retry_budget(&self, configured: u32) -> u32 {\n        let first_only = configured + 99;\n        first_only\n    }\n}\n\npub struct SecondBudget;\nimpl SecondBudget {\n    /// Bound the attempts before waiting.\n    #[cfg_attr(\n        feature = \"fast\",\n        inline\n    )]\n    pub fn retry_budget(&self, configured: u32) -> u32 {\n        let retained = configured.min(3);\n        retained\n    }\n\n    pub fn unrelated(&self) -> Duration {\n        Duration::from_secs(999)\n    }\n}\n";

#[tokio::test]
async fn read_anchor_selects_the_right_body_and_attached_syntax_context() {
    let fixture = Fixture::new();
    fixture.write("src/budget.rs", ANCHORED);
    let ctx = fixture.ctx();
    let out = execute(
        &ctx,
        json!({"action":"read","path":"src/budget.rs","line":line_number(ANCHORED,"let retained")}),
    )
    .await;
    assert_success(&out);
    assert_eq!(counter(&out, "selected"), 1);
    assert_eq!(hashes(&out).len(), 1);
    for needle in [
        "use std::time::Duration;",
        "impl SecondBudget {",
        "/// Bound the attempts",
        "#[cfg_attr(",
        "feature = \"fast\"",
        "        inline",
        "    )]",
        "let retained =",
    ] {
        let line = line_number(ANCHORED, needle);
        assert_line(&out, line, ANCHORED.lines().nth(line - 1).unwrap());
    }
    assert!(!out.text.contains("first_only"), "{}", out.text);
    assert!(!out.text.contains("pub fn unrelated"), "{}", out.text);
    assert!(!out.text.contains("Duration::from_secs(999)"));
}

#[tokio::test]
async fn nested_scope_read_includes_its_imports_without_sibling_scope_imports() {
    let fixture = Fixture::new();
    let source = "use crate::shared::Common;\nmod first {\n    use crate::alpha::Thing;\n    pub fn run() { wrong_scope(); }\n}\nmod second {\n    use crate::beta::Thing;\n    pub fn run() { selected_scope(); }\n}\n";
    fixture.write("src/scoped.rs", source);
    let line = line_number(source, "selected_scope");
    let out = execute(
        &fixture.ctx(),
        json!({"action":"read","path":"src/scoped.rs","line":line}),
    )
    .await;
    assert_success(&out);
    assert_line(&out, 1, "use crate::shared::Common;");
    assert_line(&out, 7, "    use crate::beta::Thing;");
    assert_line(&out, line, "    pub fn run() { selected_scope(); }");
    assert!(!out.text.contains("crate::alpha::Thing"), "{}", out.text);
    assert!(!out.text.contains("wrong_scope"), "{}", out.text);
}

#[tokio::test]
async fn enclosing_definition_reads_work_for_supported_non_rust_syntax() {
    let cases = [
        ("budget.py", "def retry_budget(attempts):\n    return min(attempts, 3)\n\ndef unrelated():\n    return 999\n", 2, "    return min(attempts, 3)"),
        ("budget.ts", "export const retry_budget = (attempts: number) => {\n    return Math.min(attempts, 3);\n};\nexport function unrelated() { return 999; }\n", 2, "    return Math.min(attempts, 3);"),
        ("budget.js", "export function retry_budget(attempts) {\n    return Math.min(attempts, 3);\n}\nfunction unrelated() { return 999; }\n", 2, "    return Math.min(attempts, 3);"),
        ("budget.go", "package budget\n\nfunc RetryBudget(attempts int) int {\n    return attempts\n}\n\nfunc unrelated() int { return 999 }\n", 4, "    return attempts"),
    ];
    for (path, source, line, expected) in cases {
        let fixture = Fixture::new();
        fixture.write(path, source);
        let out = execute(
            &fixture.ctx(),
            json!({"action":"read","path":path,"line":line}),
        )
        .await;
        assert_success(&out);
        assert_line(&out, line, expected);
        assert!(!out.text.contains("unrelated"), "{path}: {}", out.text);
        assert!(!out.text.contains("999"), "{path}: {}", out.text);
    }
}

const GOLD: [(&str, &str, &str); 5] = [
    ("src/retry_budget.rs", "use crate::retry_budget_type::RetryBudget;\n\n/// Cap configured attempts.\npub fn retry_budget(attempts: u32) -> u32 {\n    attempts.min(3)\n}\n", "attempts.min(3)"),
    ("src/retry_budget_caller.rs", "use crate::retry_budget::retry_budget;\n\n/// Apply the retry budget cap to configured attempts.\npub fn call_retry_budget() -> u32 {\n    retry_budget(4)\n}\n", "retry_budget(4)"),
    ("src/retry_budget_type.rs", "/// A retry budget cap for configured attempts.\npub struct RetryBudget {\n    pub maximum_attempts: u32,\n}\n", "pub maximum_attempts: u32"),
    ("tests/retry_budget.rs", "use fixture::retry_budget;\n\n#[test]\nfn retry_budget_caps_attempts() {\n    assert_eq!(retry_budget(4), 3);\n}\n", "assert_eq!(retry_budget(4), 3)"),
    ("docs/retry_budget.md", "# Retry budget\n\nA retry budget caps attempts at three, including the initial attempt.\n", "including the initial attempt"),
];

const RETRIEVAL_TASK: &str = "retry budget cap attempts";

fn retrieval_fixture() -> Fixture {
    let fixture = Fixture::new();
    for (path, source, _) in GOLD {
        fixture.write(path, source);
    }
    fixture.write(
        "src/ordinary.rs",
        "pub fn retry_budget() -> u32 {\n    99\n}\n",
    );
    fixture.write("src/examples.rs", "// retry budget retry budget retry budget retry budget\npub fn example() -> &'static str {\n    \"retry budget is just example prose\"\n}\n");
    fixture
}

fn assert_gold(out: &ExecOut) {
    for (path, _, fact) in GOLD {
        assert!(
            out.text.contains(&format!("path: {path}\n")),
            "missing {path}: {}",
            out.text
        );
        assert!(
            out.text.contains(fact),
            "missing {path} fact {fact}: {}",
            out.text
        );
    }
}

#[tokio::test]
async fn task_terms_rank_implementation_caller_type_tests_and_docs_before_decoys() {
    let fixture = retrieval_fixture();
    let out = execute(
        &fixture.ctx(),
        json!({"action":"search","query":RETRIEVAL_TASK,"limit":5}),
    )
    .await;
    assert_success(&out);
    assert_gold(&out);
    assert_eq!(counter(&out, "selected"), 5);
    assert!(
        !out.text.contains("path: src/ordinary.rs\n"),
        "{}",
        out.text
    );
    assert!(
        !out.text.contains("path: src/examples.rs\n"),
        "{}",
        out.text
    );
    assert!(
        counter(&out, "omitted") > 0,
        "ranked selection must report omitted candidates"
    );
    assert!(out.truncated);
    assert!(out.text.contains("selection_reason:"));
    assert_eq!(hashes(&out).len(), 5);
}

#[tokio::test]
async fn ambiguous_lexical_search_preserves_both_same_named_candidates() {
    let fixture = retrieval_fixture();
    let out = execute(
        &fixture.ctx(),
        json!({"action":"search","query":"retry budget","limit":20}),
    )
    .await;
    assert_success(&out);
    assert!(
        out.text.contains("path: src/retry_budget.rs\n"),
        "{}",
        out.text
    );
    assert!(out.text.contains("path: src/ordinary.rs\n"), "{}", out.text);
    assert!(out.text.contains("attempts.min(3)"));
    assert!(out.text.contains("99"));
    assert!(out.text.contains("selection_reason:"));
    assert!(!out.text.contains("analysis_complete: true"));
}

#[tokio::test]
async fn paired_offline_retrieval_records_calls_bytes_and_critical_recall() {
    let fixture = retrieval_fixture();
    let ctx = fixture.ctx();
    let listing = tools::execute(
        &ctx,
        "inventory",
        &json!({"action":"files","query":"retry_budget"}),
        std::future::pending(),
        None,
    )
    .await
    .unwrap();
    assert_success(&listing);
    let mut baseline_bytes = listing.text.len();
    let mut baseline_source_bytes = counter(&listing, "bytes_read");
    for (path, source, fact) in GOLD {
        assert!(listing.text.contains(path));
        let read = tools::execute(
            &ctx,
            "read_file",
            &json!({"path":path,"offset":1,"limit":source.lines().count()}),
            std::future::pending(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(read.kind, ExecKind::Success, "{}", read.text);
        assert!(read.text.contains(fact), "baseline lost {path}");
        baseline_bytes += read.text.len();
        // read_file reads each immutable, regular fixture file in full. This
        // is an observed fixture byte count, not estimated provider usage.
        baseline_source_bytes += std::fs::metadata(fixture.0.join(path)).unwrap().len() as usize;
    }
    let candidate = execute(
        &ctx,
        json!({"action":"search","query":RETRIEVAL_TASK,"limit":5}),
    )
    .await;
    assert_success(&candidate);
    assert_gold(&candidate);
    println!(
        "{}",
        json!({
            "evaluation":"offline_paired_retrieval",
            "task":RETRIEVAL_TASK,
            "critical_regions":5,
            "baseline_critical_recall":5,
            "candidate_critical_recall":5,
            "baseline_tool_calls":6,
            "candidate_tool_calls":1,
            "baseline_returned_bytes":baseline_bytes,
            "candidate_returned_bytes":candidate.text.len(),
            "baseline_source_and_control_bytes":baseline_source_bytes,
            "candidate_source_and_control_bytes":counter(&candidate,"bytes_read"),
            "baseline_source_files_read":5,
            "candidate_source_read_attempts":counter(&candidate,"files_scanned"),
            "candidate_source_files_read":counter(&candidate,"files_read"),
            "provider_tokens":null,
            "provider_cache_hits":null
        })
    );
}

async fn paired_native_trajectory(fixture: &Fixture, candidate: bool) -> Vec<(Vec<u8>, Value)> {
    let requests = Arc::new(Mutex::new(Vec::<(Vec<u8>, Value)>::new()));
    let saved = requests.clone();
    let port = common::serve(move |body, _| {
        let mut requests = saved.lock().unwrap();
        requests.push((body.to_vec(), serde_json::from_slice(body).unwrap()));
        match (candidate, requests.len()) {
            (true, 1) => common::sse_tool_calls(json!([common::tc(
                "context", "code_context",
                &json!({"action":"search","query":RETRIEVAL_TASK,"limit":5}).to_string(),
            )])),
            (false, 1) => common::sse_tool_calls(json!([common::tc(
                "inventory", "inventory", r#"{"action":"files","query":"retry_budget"}"#,
            )])),
            (false, 2) => common::sse_tool_calls(json!(GOLD
                .iter().enumerate().map(|(index, (path, source, _))| common::tc(
                    &format!("read_{index}"), "read_file",
                    &json!({"path":path,"offset":1,"limit":source.lines().count()}).to_string(),
                )).collect::<Vec<_>>())),
            _ => common::sse_text("state: flow-verified\nverified: offline retrieval observations collected\nunverified: live model reasoning, provider tokens and cache hits"),
        }
    });
    let label = if candidate { "candidate" } else { "baseline" };
    let mut agent = sui::agent::Agent::new(
        sui::provider::Provider::new(
            &format!("http://127.0.0.1:{port}"),
            None,
            "paired-context-fixture".into(),
            None,
        ),
        fixture.ctx(),
        // Both trajectories use only native observation tools. Their flow
        // should not borrow an execution approval or interactive stdin.
        sui::permission::Gate::new(false),
        sui::journal::Journal::open(&fixture.0.join(".sui").join(label)).unwrap(),
        sui::agent::Limits {
            max_turns: 3,
            context_budget: 50000,
            context_reserve: 1000,
            compact_context: false,
            request_timeout: Duration::from_secs(5),
        },
        sui::agent::Identity {
            session_id: format!("paired-context-{label}"),
            agent_id: "worker".into(),
            role: "worker".into(),
            base_url: format!("http://127.0.0.1:{port}"),
            model: "paired-context-fixture".into(),
            cache_key_fingerprint: None,
        },
    );
    agent.set_quiet(true);
    agent.run_turn(RETRIEVAL_TASK).await.unwrap();
    let captured = requests.lock().unwrap().clone();
    assert_eq!(captured.len(), if candidate { 2 } else { 3 });
    let initial = captured[0].1["messages"].as_array().unwrap();
    for (_, request) in &captured[1..] {
        assert_eq!(request["tools"], captured[0].1["tools"]);
        assert_eq!(
            &request["messages"].as_array().unwrap()[..initial.len()],
            initial
        );
    }
    let final_request = &captured.last().unwrap().1;
    for (index, (path, _, fact)) in GOLD.iter().enumerate() {
        let id = if candidate {
            "context".to_owned()
        } else {
            format!("read_{index}")
        };
        let result = final_request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .find(|message| message["role"] == "tool" && message["tool_call_id"] == id)
            .unwrap_or_else(|| panic!("{label} lost tool history for {path}"))["content"]
            .as_str()
            .unwrap();
        assert!(
            result.contains("status: success"),
            "{label} {path}: {result}"
        );
        assert!(
            result.contains(fact),
            "{label} lost critical fact for {path}: {result}"
        );
        if candidate {
            assert!(
                result.contains(&format!("path: {path}\n")),
                "candidate lost {path}"
            );
        }
    }
    captured
}

#[tokio::test]
async fn paired_native_discovery_reduces_requests_and_serialized_input_without_losing_gold() {
    let fixture = retrieval_fixture();
    let baseline = paired_native_trajectory(&fixture, false).await;
    let candidate = paired_native_trajectory(&fixture, true).await;
    // Same task, workspace, schemas and static prefix: only the discovery
    // trajectory changes. The baseline batches all five targeted reads.
    assert_eq!(baseline[0].1["messages"], candidate[0].1["messages"]);
    assert_eq!(baseline[0].1["tools"], candidate[0].1["tools"]);
    let input_bytes = |requests: &[(Vec<u8>, Value)]| -> usize {
        requests
            .iter()
            .map(|(_, request)| {
                serde_json::to_vec(&request["messages"]).unwrap().len()
                    + serde_json::to_vec(&request["tools"]).unwrap().len()
            })
            .sum()
    };
    let baseline_input = input_bytes(&baseline);
    let candidate_input = input_bytes(&candidate);
    assert!(candidate.len() < baseline.len());
    assert!(
        candidate_input < baseline_input,
        "candidate input {candidate_input} did not improve baseline {baseline_input}"
    );
    println!(
        "{}",
        json!({
            "evaluation":"offline_paired_native_discovery", "task":RETRIEVAL_TASK,
            "critical_regions":5, "baseline_critical_recall":5, "candidate_critical_recall":5,
            "baseline_model_requests":baseline.len(), "candidate_model_requests":candidate.len(),
            "baseline_total_http_body_bytes":baseline.iter().map(|(body, _)| body.len()).sum::<usize>(),
            "candidate_total_http_body_bytes":candidate.iter().map(|(body, _)| body.len()).sum::<usize>(),
            "baseline_total_serialized_messages_and_schemas_bytes":baseline_input,
            "candidate_total_serialized_messages_and_schemas_bytes":candidate_input,
            "provider_tokens":null, "provider_cache_hits":null,
            "limitations":"scripted discovery flow; repeated prefix bytes may be provider-cached; not live reasoning or billing proof"
        })
    );
}

#[tokio::test]
async fn complete_cached_excerpts_survive_new_history_and_same_mtime_edits() {
    let fixture = Fixture::new();
    let original = "pub fn retry_budget() -> u32 {\n    1\n}\n";
    let changed = "pub fn retry_budget() -> u32 {\n    2\n}\n";
    fixture.write("src/budget.rs", original);
    let ctx = fixture.ctx();
    let args = json!({"action":"read","path":"src/budget.rs","line":2});
    let first = execute(&ctx, args.clone()).await;
    assert_success(&first);
    assert!(counter(&first, "local_parse_cache_misses") > 0);
    let initial_hash = hashes(&first)[0].to_owned();
    let fresh_history = fixture.ctx();
    assert!(fresh_history
        .code_context
        .set(ctx.code_context.get().unwrap().clone())
        .is_ok());
    let hit = execute(&fresh_history, args.clone()).await;
    assert_success(&hit);
    assert!(counter(&hit, "local_parse_cache_hits") > 0);
    assert_line(&hit, 2, "    1");
    assert_eq!(hashes(&hit), vec![initial_hash.as_str()]);
    assert!(
        counter(&hit, "bytes_read") >= original.len(),
        "a syntax cache hit still validates source"
    );
    let path = fixture.0.join("src/budget.rs");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    fixture.write("src/budget.rs", changed);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().modified().unwrap(),
        modified
    );
    assert_eq!(original.len(), changed.len());
    let updated = execute(&ctx, args).await;
    assert_success(&updated);
    assert_line(&updated, 2, "    2");
    assert!(!updated.text.contains("2 |     1"));
    assert_ne!(hashes(&updated)[0], initial_hash);
    assert!(counter(&updated, "local_parse_cache_misses") > 0);
}

#[tokio::test]
async fn repeated_seventy_file_scans_keep_warm_facts_and_refresh_only_changed_source() {
    let fixture = Fixture::new();
    for index in 0..70 {
        fixture.write(
            &format!("src/cache_entry_{index:03}.rs"),
            &format!("pub fn cache_target_{index:03}() -> u32 {{\n    // stable state\n    {index}\n}}\n"),
        );
    }
    let ctx = fixture.ctx();
    let args = json!({"action":"search","query":"cache target","limit":1});
    let first = execute(&ctx, args.clone()).await;
    assert_success(&first);
    assert_eq!(counter(&first, "local_parse_cache_misses"), 70);
    assert_eq!(counter(&first, "local_parse_cache_hits"), 0);
    let warm = execute(&ctx, args).await;
    assert_success(&warm);
    assert_eq!(counter(&warm, "local_parse_cache_hits"), 70);
    assert_eq!(counter(&warm, "local_parse_cache_misses"), 0);
    assert_eq!(hashes(&warm), hashes(&first));
    assert!(warm.text.contains("path: src/cache_entry_000.rs\n"));
    assert_line(&warm, 3, "    0");

    let path = fixture.0.join("src/cache_entry_042.rs");
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    let original = std::fs::read_to_string(&path).unwrap();
    let changed = original.replace("stable state", "edited state");
    assert_eq!(original.len(), changed.len());
    fixture.write("src/cache_entry_042.rs", &changed);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let updated = execute(
        &ctx,
        json!({"action":"search","query":"cache target edited","limit":1}),
    )
    .await;
    assert_success(&updated);
    assert_eq!(counter(&updated, "local_parse_cache_hits"), 69);
    assert_eq!(counter(&updated, "local_parse_cache_misses"), 1);
    assert!(updated.text.contains("path: src/cache_entry_042.rs\n"));
    assert_line(&updated, 2, "    // edited state");
    assert!(!updated.text.contains("// stable state"));
}

#[tokio::test]
async fn search_rechecks_new_deleted_and_newly_ignored_files_after_cache_hits() {
    let fixture = Fixture::new();
    fixture.write("src/initial.rs", "pub fn retry_budget_initial() {}\n");
    let ctx = fixture.ctx();
    let args = json!({"action":"search","query":"retry budget"});
    assert!(execute(&ctx, args.clone())
        .await
        .text
        .contains("path: src/initial.rs\n"));
    fixture.write("src/new.rs", "pub fn retry_budget_added() {}\n");
    let added = execute(&ctx, args.clone()).await;
    assert_success(&added);
    assert!(added.text.contains("path: src/new.rs\n"));
    std::fs::remove_file(fixture.0.join("src/initial.rs")).unwrap();
    let deleted = execute(&ctx, args.clone()).await;
    assert_success(&deleted);
    assert!(!deleted.text.contains("path: src/initial.rs\n"));
    assert!(deleted.text.contains("path: src/new.rs\n"));
    fixture.write(".ignore", "src/new.rs\n");
    let ignored = execute(&ctx, args).await;
    assert_success(&ignored);
    assert_eq!(counter(&ignored, "selected"), 0);
    assert!(!ignored.text.contains("retry_budget_added"));
    assert!(!ignored.text.contains("path: src/new.rs\n"));
}

#[tokio::test]
async fn roots_are_isolated_even_when_a_caller_reuses_the_same_cache_service() {
    let first = Fixture::new();
    let second = Fixture::new();
    first.write(
        "src/budget.rs",
        "pub fn retry_budget() { first_workspace_only(); }\n",
    );
    second.write(
        "src/budget.rs",
        "pub fn retry_budget() { second_workspace_only(); }\n",
    );
    let first_ctx = first.ctx();
    let args = json!({"action":"read","path":"src/budget.rs","line":1});
    let initial = execute(&first_ctx, args.clone()).await;
    assert_success(&initial);
    let second_ctx = second.ctx();
    assert!(second_ctx
        .code_context
        .set(first_ctx.code_context.get().unwrap().clone())
        .is_ok());
    let isolated = execute(&second_ctx, args).await;
    assert!(
        !isolated.text.contains("first_workspace_only"),
        "{}",
        isolated.text
    );
    if isolated.kind == ExecKind::Success {
        assert!(
            isolated.text.contains("second_workspace_only"),
            "{}",
            isolated.text
        );
        assert_ne!(hashes(&initial), hashes(&isolated));
    } else {
        assert_eq!(isolated.kind, ExecKind::Error, "{}", isolated.text);
    }
}

#[tokio::test]
async fn whole_output_budget_handles_unicode_and_oversized_single_lines_honestly() {
    let fixture = Fixture::new();
    let source = format!(
        "/// Unicode budget example.\npub fn retry_budget() {{\n    let text = \"{}\";\n}}\n",
        "猫🦀".repeat(6000)
    );
    fixture.write("src/unicode.rs", &source);
    for action in ["read", "search"] {
        let mut args = json!({"action":action,"max_bytes":1024});
        if action == "read" {
            args["path"] = json!("src/unicode.rs");
            args["line"] = json!(3);
        } else {
            args["query"] = json!("retry budget");
        }
        let out = execute(&fixture.ctx(), args).await;
        assert_success(&out);
        assert!(
            out.text.len() <= 1024,
            "WHOLE output used {} bytes",
            out.text.len()
        );
        assert!(out.truncated, "{}", out.text);
        assert!(out.text.contains("truncated: true"), "{}", out.text);
        assert!(
            !out.text.contains(&source),
            "oversized line was returned in full"
        );
        assert!(std::str::from_utf8(out.text.as_bytes()).is_ok());
    }
}

#[tokio::test]
async fn a_small_budget_keeps_the_requested_anchor_near_the_end_of_a_long_body() {
    let fixture = Fixture::new();
    let mut source = "pub fn retry_budget() {\n".to_owned();
    for index in 0..100 {
        source.push_str(&format!("    let preceding_{index:03} = {index};\n"));
    }
    source.push_str("    let anchor_kept = preceding_099;\n}\n");
    fixture.write("src/long.rs", &source);
    let line = line_number(&source, "let anchor_kept");
    let out = execute(
        &fixture.ctx(),
        json!({"action":"read","path":"src/long.rs","line":line,"max_bytes":1024}),
    )
    .await;
    assert_success(&out);
    assert!(out.text.len() <= 1024);
    assert_line(&out, line, "    let anchor_kept = preceding_099;");
    assert!(out.truncated);
    assert!(counter(&out, "omitted_source_lines") > 0);
}

#[tokio::test]
async fn arguments_scopes_and_sensitive_paths_do_not_expose_source() {
    let fixture = Fixture::new();
    let source = "pub fn retry_budget() { sensitive_fixture_marker(); }\n";
    fixture.write("src/public.rs", "pub fn retry_budget() {}\n");
    for path in [
        ".env.rs",
        "auth.json",
        "credentials.json",
        ".sui/private.rs",
        "node_modules/private.rs",
    ] {
        fixture.write(path, source);
    }
    let ctx = fixture.ctx();
    let cases = [
        json!({}),
        json!({"action":"search"}),
        json!({"action":"search","query":"   "}),
        json!({"action":"search","query":"x".repeat(257)}),
        json!({"action":"search","query":123}),
        json!({"action":"search","query":"retry","limit":0}),
        json!({"action":"search","query":"retry","limit":21}),
        json!({"action":"search","query":"retry","max_bytes":1023}),
        json!({"action":"search","query":"retry","max_bytes":24001}),
        json!({"action":"search","query":"retry","unexpected":true}),
        json!({"action":"read","path":"src/public.rs"}),
        json!({"action":"read","line":1}),
        json!({"action":"read","path":"src/public.rs","line":0}),
        json!({"action":"read","path":"src/public.rs","line":999}),
        json!({"action":"read","path":"../escape.rs","line":1}),
        json!({"action":"read","path":".env.rs","line":1}),
        json!({"action":"read","path":"auth.json","line":1}),
        json!({"action":"read","path":".sui/private.rs","line":1}),
        json!({"action":"read","path":"node_modules/private.rs","line":1}),
    ];
    for args in cases {
        if let Ok(out) =
            tools::execute(&ctx, "code_context", &args, std::future::pending(), None).await
        {
            assert_eq!(out.kind, ExecKind::Error, "{args}: {}", out.text);
            assert!(!out.text.contains("sensitive_fixture_marker"));
        }
    }
    let search = execute(&ctx, json!({"action":"search","query":"retry budget"})).await;
    assert_success(&search);
    assert!(search.text.contains("path: src/public.rs\n"));
    assert!(!search.text.contains("sensitive_fixture_marker"));
    fixture.write("other/retry.rs", "pub fn retry_budget_outside_scope() {}\n");
    let scoped = execute(
        &ctx,
        json!({"action":"search","query":"retry budget","path":"src"}),
    )
    .await;
    assert_success(&scoped);
    assert!(!scoped.text.contains("outside_scope"));
}

fn config_value(name: &str, value: &str) -> String {
    format!("{} = {}\n", name, serde_json::to_string(value).unwrap())
}

fn rust_literal(name: &str, value: &str) -> String {
    format!(
        "pub fn privacy_probe() {{\n    let {} = {};\n}}\n",
        name,
        serde_json::to_string(value).unwrap()
    )
}

fn assert_sensitive_context_withheld(out: &ExecOut, value: &str) {
    assert!(
        !out.text.contains(value),
        "credential-like fixture escaped into tool output"
    );
    assert!(
        !out.text
            .lines()
            .any(|line| line.starts_with("source_sha256: ")),
        "withheld contents must not publish a source hash: {}",
        out.text
    );
    match out.kind {
        ExecKind::Error => assert!(out.text.starts_with("status: error\n")),
        ExecKind::Success => {
            assert!(
                out.truncated,
                "withheld context must report partial coverage"
            );
            assert!(out.text.contains("scan_complete: false"), "{}", out.text);
            assert_eq!(counter(out, "selected"), 0);
        }
        other => panic!("unexpected privacy result {other:?}: {}", out.text),
    }
}

#[tokio::test]
async fn innocently_named_files_with_credential_like_content_are_withheld_before_hashing() {
    // Build synthetic values at runtime so this regression source remains
    // inspectable. These values are never valid external credentials.
    let long_key = ["sk-", &"Q7vJ9mR2nL8x".repeat(6)].concat();
    let pass_value = ["V7q", "N9r", "C3m", "P8z"].concat().repeat(3);
    let opaque_value = ["U8m", "L2q", "R9p", "T4v"].concat().repeat(3);
    let stripe_live = ["sk_live_", &"P8rN3qV9mT2x".repeat(4)].concat();
    let stripe_test = ["sk_test_", &"J7vR4nL8qM2p".repeat(4)].concat();
    let stripe_org = ["sk_org_", &"J7vR4nL8qM2p".repeat(4)].concat();
    let stripe_restricted = ["rk_live_", &"J7vR4nL8qM2p".repeat(4)].concat();
    let url_value = [
        "https",
        ":",
        "//",
        "agent",
        ":",
        pass_value.as_str(),
        "@",
        "service.invalid",
        "/v1",
    ]
    .concat();
    let ssh_with_password = [
        "ssh",
        ":",
        "//",
        "git",
        ":",
        pass_value.as_str(),
        "@",
        "service.invalid",
        "/repo",
    ]
    .concat();
    let pem_begin = ["-----BEGIN ", "PRIVATE", " KEY-----"].concat();
    let pem_end = ["-----END ", "PRIVATE", " KEY-----"].concat();
    let pem_body = "A".repeat(64);
    let pem_source =
        format!("client_material = \"\"\"\n{pem_begin}\n{pem_body}\n{pem_end}\n\"\"\"\n");
    let cases = [
        (
            "config.toml",
            config_value("api_key", &long_key),
            long_key.as_str(),
            "api",
        ),
        (
            "config.toml",
            config_value("password", &pass_value),
            pass_value.as_str(),
            "password",
        ),
        (
            "config.toml",
            config_value("api_key", &opaque_value),
            opaque_value.as_str(),
            "api",
        ),
        (
            "config.toml",
            config_value("endpoint", &url_value),
            url_value.as_str(),
            "endpoint",
        ),
        ("config.toml", pem_source, pem_body.as_str(), "client"),
        (
            "source.rs",
            rust_literal("token", &long_key),
            long_key.as_str(),
            "privacy",
        ),
        (
            "source.rs",
            rust_literal("password", &pass_value),
            pass_value.as_str(),
            "privacy",
        ),
        (
            "source.rs",
            rust_literal("API_KEY", &opaque_value),
            opaque_value.as_str(),
            "privacy",
        ),
        (
            "source.rs",
            rust_literal("note", &stripe_live),
            stripe_live.as_str(),
            "privacy",
        ),
        (
            "source.rs",
            rust_literal("note", &stripe_test),
            stripe_test.as_str(),
            "privacy",
        ),
        (
            "source.rs",
            rust_literal("note", &stripe_org),
            stripe_org.as_str(),
            "privacy",
        ),
        (
            "source.rs",
            rust_literal("note", &stripe_restricted),
            stripe_restricted.as_str(),
            "privacy",
        ),
        (
            "config.toml",
            config_value("endpoint", &ssh_with_password),
            ssh_with_password.as_str(),
            "endpoint",
        ),
    ];
    for (path, source, value, query) in cases {
        let fixture = Fixture::new();
        fixture.write(path, &source);
        let ctx = fixture.ctx();
        let searched = execute(&ctx, json!({"action":"search","query":query,"path":path})).await;
        assert_sensitive_context_withheld(&searched, value);
        let read = execute(&ctx, json!({"action":"read","path":path,"line":1})).await;
        assert_sensitive_context_withheld(&read, value);
    }
}

#[tokio::test]
async fn harmless_markers_types_and_config_placeholders_remain_retrievable() {
    let empty_config = config_value("api_key", "");
    let example_config = config_value("api_key", "example");
    let environment_config = config_value("api_key", "${API_KEY}");
    let sk_placeholder = config_value("api_key", "sk-...");
    let exa_placeholder = config_value("api_key", "exa-…");
    let named_placeholder = config_value("api_key", "YOUR_API_KEY_HERE");
    let ssh_repository = config_value("repository", "ssh://git@service.invalid/repo");
    let git_ssh_repository = config_value("repository", "git+ssh://git@service.invalid/repo");
    let public_key = ["pk_live_", &"J7vR4nL8qM2p".repeat(4)].concat();
    let public_source = rust_literal("note", &public_key);
    let cases = [
        ("source.rs", "pub fn privacy_probe() -> &'static str { \"sk-\" }\n", "privacy", "\"sk-\""),
        ("source.rs", "pub struct CredentialShape {\n    pub api_key: String,\n    pub password: Option<String>,\n}\n", "api", "pub api_key: String"),
        ("config.toml", empty_config.as_str(), "api", "api_key"),
        ("config.toml", example_config.as_str(), "api", "example"),
        ("config.toml", environment_config.as_str(), "api", "${API_KEY}"),
        ("config.toml", sk_placeholder.as_str(), "api", "sk-..."),
        ("config.toml", exa_placeholder.as_str(), "api", "exa-…"),
        ("config.toml", named_placeholder.as_str(), "api", "YOUR_API_KEY_HERE"),
        ("config.toml", ssh_repository.as_str(), "repository", "ssh://git@service.invalid/repo"),
        ("config.toml", git_ssh_repository.as_str(), "repository", "git+ssh://git@service.invalid/repo"),
        ("source.rs", public_source.as_str(), "privacy", public_key.as_str()),
    ];
    for (path, source, query, expected) in cases {
        let fixture = Fixture::new();
        fixture.write(path, source);
        let ctx = fixture.ctx();
        for args in [
            json!({"action":"search","query":query,"path":path}),
            json!({"action":"read","path":path,"line":1}),
        ] {
            let out = execute(&ctx, args).await;
            assert_success(&out);
            assert_eq!(counter(&out, "selected"), 1, "{}", out.text);
            assert!(out.text.contains(expected), "{}", out.text);
            assert_eq!(hashes(&out).len(), 1);
        }
    }
}

fn triple_literal(name: &str, value: &str, quote: char, multiline: bool, prefix: &str) -> String {
    let delimiter = quote.to_string().repeat(3);
    let body = if multiline {
        format!("\n{value}\n")
    } else {
        value.to_owned()
    };
    format!("{name} = {prefix}{delimiter}{body}{delimiter}\n")
}

#[tokio::test]
async fn opaque_config_value_punctuation_is_not_treated_as_a_comment_or_separator() {
    let suffix = ["Q8r", "M2v", "P7n", "L9t"].concat().repeat(4);
    for (path, separator, prefix) in [
        ("config.yaml", ": ", "example#"),
        ("config.properties", "=", "test;"),
        ("config.yaml", ": ", "example,"),
    ] {
        let fixture = Fixture::new();
        let value = format!("{prefix}{suffix}");
        let source = format!("{}{separator}{value}\n", "password");
        fixture.write(path, &source);
        let ctx = fixture.ctx();
        let search = execute(
            &ctx,
            json!({"action":"search","query":"password","path":path}),
        )
        .await;
        assert_sensitive_context_withheld(&search, &value);
        let read = execute(&ctx, json!({"action":"read","path":path,"line":1})).await;
        assert_sensitive_context_withheld(&read, &value);
    }
}

fn rust_raw_literal(name: &str, value: &str) -> String {
    format!(
        "pub fn privacy_probe() {{\n    let {} = r#{}{}{}#;\n}}\n",
        name, '"', value, '"'
    )
}

#[tokio::test]
async fn rust_raw_strings_use_matching_delimiters_for_credential_detection() {
    let suffix = ["N7v", "L2r", "Q9p", "M4t"].concat().repeat(4);
    let value = ["example", "\"", suffix.as_str()].concat();
    let fixture = Fixture::new();
    fixture.write("source.rs", &rust_raw_literal("password", &value));
    let ctx = fixture.ctx();
    for args in [
        json!({"action":"search","query":"privacy","path":"source.rs"}),
        json!({"action":"read","path":"source.rs","line":1}),
    ] {
        let out = execute(&ctx, args).await;
        assert_sensitive_context_withheld(&out, &value);
    }
    for placeholder in ["", "example", "${API_KEY}"] {
        let source = rust_raw_literal("password", placeholder);
        fixture.write("source.rs", &source);
        let out = execute(&ctx, json!({"action":"read","path":"source.rs","line":2})).await;
        assert_success(&out);
        assert_eq!(hashes(&out).len(), 1);
        assert_line(&out, 2, source.lines().nth(1).unwrap());
    }
}

#[tokio::test]
async fn triple_quoted_config_and_python_credentials_are_withheld_before_hash_or_cache() {
    let value = ["R8v", "L2q", "N7p", "T9m"].concat().repeat(4);
    for (path, prefix) in [
        ("config.toml", ""),
        ("source.py", ""),
        ("source.py", "r"),
        ("source.py", "R"),
    ] {
        for quote in ['\'', '"'] {
            for multiline in [false, true] {
                let fixture = Fixture::new();
                fixture.write(
                    path,
                    &triple_literal("password", &value, quote, multiline, prefix),
                );
                let ctx = fixture.ctx();
                let searched = execute(
                    &ctx,
                    json!({"action":"search","query":"password","path":path}),
                )
                .await;
                assert_sensitive_context_withheld(&searched, &value);
                if searched.kind == ExecKind::Success {
                    assert_eq!(counter(&searched, "local_parse_cache_hits"), 0);
                    assert_eq!(counter(&searched, "local_parse_cache_misses"), 0);
                }
                let read = execute(&ctx, json!({"action":"read","path":path,"line":1})).await;
                assert_sensitive_context_withheld(&read, &value);
            }
        }
    }
}

#[tokio::test]
async fn completed_same_line_triple_quoted_placeholders_stay_retrievable() {
    for (path, prefix) in [
        ("config.toml", ""),
        ("source.py", ""),
        ("source.py", "r"),
        ("source.py", "R"),
    ] {
        for quote in ['\'', '"'] {
            for value in ["", "example", "${API_KEY}"] {
                let fixture = Fixture::new();
                let source = triple_literal("password", value, quote, false, prefix);
                fixture.write(path, &source);
                let ctx = fixture.ctx();
                for args in [
                    json!({"action":"search","query":"password","path":path}),
                    json!({"action":"read","path":path,"line":1}),
                ] {
                    let out = execute(&ctx, args).await;
                    assert_success(&out);
                    assert_eq!(counter(&out, "selected"), 1, "{}", out.text);
                    assert_line(&out, 1, source.trim_end_matches('\n'));
                    assert_eq!(hashes(&out).len(), 1);
                }
            }
        }
    }
}

#[tokio::test]
async fn python_string_prefixes_withhold_credentials_but_preserve_safe_placeholders() {
    let value = ["V7m", "Q2r", "N9p", "L4t"].concat().repeat(4);
    for prefix in [
        "r", "R", "u", "U", "b", "B", "br", "bR", "Br", "BR", "rb", "rB", "Rb", "RB", "f", "F",
        "fr", "fR", "Fr", "FR", "rf", "rF", "Rf", "RF",
    ] {
        for delimiter in ["'", "\"\"\""] {
            let fixture = Fixture::new();
            let ctx = fixture.ctx();
            let source = format!("{} = {prefix}{delimiter}{value}{delimiter}\n", "password");
            fixture.write("source.py", &source);
            for args in [
                json!({"action":"search","query":"password","path":"source.py"}),
                json!({"action":"read","path":"source.py","line":1}),
            ] {
                let out = execute(&ctx, args).await;
                assert_sensitive_context_withheld(&out, &value);
                if out.kind == ExecKind::Success {
                    assert_eq!(counter(&out, "local_parse_cache_hits"), 0);
                    assert_eq!(counter(&out, "local_parse_cache_misses"), 0);
                }
            }
            for placeholder in ["", "example", "${API_KEY}"] {
                let source = format!(
                    "{} = {prefix}{delimiter}{placeholder}{delimiter}\n",
                    "password"
                );
                fixture.write("source.py", &source);
                let out = execute(&ctx, json!({"action":"read","path":"source.py","line":1})).await;
                assert_success(&out);
                assert_eq!(
                    counter(&out, "selected"),
                    1,
                    "prefix {prefix}, delimiter {delimiter}: {}",
                    out.text
                );
                assert_line(&out, 1, source.trim_end_matches('\n'));
                assert_eq!(hashes(&out).len(), 1);
            }
        }
    }
}

#[tokio::test]
async fn python_class_headers_do_not_copy_the_preceding_unrelated_method_body() {
    let sources = [
        "import os\nclass Client:\n    def unrelated_before(self):\n        preceding_body = 999\n        return preceding_body\n\n    def selected(self):\n        return selected_method_value\n",
        "from support import Base, registered\n@registered\nclass Client(\n    Base,\n):\n    def unrelated_before(self):\n        preceding_body = 999\n        return preceding_body\n\n    def selected(self):\n        return selected_method_value\n",
    ];
    for source in sources {
        let fixture = Fixture::new();
        fixture.write("client.py", source);
        let line = line_number(source, "return selected_method_value");
        let out = execute(
            &fixture.ctx(),
            json!({"action":"read","path":"client.py","line":line}),
        )
        .await;
        assert_success(&out);
        assert_line(&out, 1, source.lines().next().unwrap());
        let class = line_number(source, "class Client");
        assert_line(&out, class, source.lines().nth(class - 1).unwrap());
        assert_line(&out, line - 1, "    def selected(self):");
        assert_line(&out, line, "        return selected_method_value");
        assert!(!out.text.contains("unrelated_before"), "{}", out.text);
        assert!(!out.text.contains("preceding_body"), "{}", out.text);
        assert!(!out.text.contains("999"), "{}", out.text);
        if source.contains("@registered") {
            for expected in ["@registered", "    Base,", "):"] {
                let line = line_number(source, expected);
                assert_line(&out, line, source.lines().nth(line - 1).unwrap());
            }
        }
    }
}

#[tokio::test]
async fn warmed_safe_source_becoming_sensitive_is_withheld_then_safe_source_is_fresh_again() {
    let fixture = Fixture::new();
    let safe_source = "pub fn privacy_probe() -> &'static str { \"sk-\" }\n";
    fixture.write("source.rs", safe_source);
    let ctx = fixture.ctx();
    let args = json!({"action":"read","path":"source.rs","line":1});
    let first = execute(&ctx, args.clone()).await;
    assert_success(&first);
    let original_hash = hashes(&first)[0].to_owned();
    let warm = execute(&ctx, args.clone()).await;
    assert!(counter(&warm, "local_parse_cache_hits") > 0);
    let value = ["sk-", &"N8pR3vL9qT2m".repeat(6)].concat();
    fixture.write("source.rs", &rust_literal("token", &value));
    let withheld = execute(&ctx, args.clone()).await;
    assert_sensitive_context_withheld(&withheld, &value);
    let searched = execute(
        &ctx,
        json!({"action":"search","query":"privacy","path":"source.rs"}),
    )
    .await;
    assert_sensitive_context_withheld(&searched, &value);
    fixture.write("source.rs", safe_source);
    let restored = execute(&ctx, args).await;
    assert_success(&restored);
    assert_eq!(hashes(&restored), vec![original_hash.as_str()]);
    assert_line(&restored, 1, safe_source.trim_end_matches('\n'));
    assert!(!restored.text.contains(&value));
}

#[cfg(unix)]
#[tokio::test]
async fn unsafe_ignore_controls_prune_only_the_affected_subtree_and_symlinks_are_excluded() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    fixture.write("public.rs", "pub fn guarded_public() {}\n");
    fixture.write("private/hidden.rs", "pub fn guarded_private() {}\n");
    outside.write("rules", "");
    outside.write("outside.rs", "pub fn guarded_outside() {}\n");
    std::os::unix::fs::symlink(outside.0.join("rules"), fixture.0.join("private/.ignore")).unwrap();
    std::os::unix::fs::symlink(outside.0.join("outside.rs"), fixture.0.join("alias.rs")).unwrap();
    let out = execute(&fixture.ctx(), json!({"action":"search","query":"guarded"})).await;
    assert_success(&out);
    assert!(out.text.contains("guarded_public"));
    assert!(!out.text.contains("guarded_private"));
    assert!(!out.text.contains("guarded_outside"));
    assert!(!out.text.contains("path: alias.rs\n"));
    assert!(out.text.contains("scan_complete: false"), "{}", out.text);
    assert!(out.truncated);
    let read = tools::execute(
        &fixture.ctx(),
        "code_context",
        &json!({"action":"read","path":"alias.rs","line":1}),
        std::future::pending(),
        None,
    )
    .await;
    if let Ok(read) = read {
        assert_eq!(read.kind, ExecKind::Error, "{}", read.text);
        assert!(!read.text.contains("guarded_outside"));
    }
}

#[test]
fn headless_cli_search_edit_and_fresh_read_preserve_the_frozen_request_prefix() {
    use std::process::{Command, Stdio};
    let fixture = Fixture::new();
    fixture.write(
        "src/budget.rs",
        "pub fn retry_budget() -> u32 {\n    1\n}\n",
    );
    fixture.write(
        "sui.toml",
        "[agent]\ncontext_compaction = false\nmax_turns = 4\n",
    );
    let requests = Arc::new(Mutex::new(Vec::<Value>::new()));
    let saved = requests.clone();
    let port = common::serve(move |body, _| {
        let mut requests = saved.lock().unwrap();
        requests.push(serde_json::from_slice(body).unwrap());
        match requests.len() {
            1 => common::sse_tool_calls(json!([common::tc(
                "search", "code_context", r#"{"action":"search","query":"retry budget"}"#,
            )])),
            2 => common::sse_tool_calls(json!([common::tc(
                "edit", "edit_file", r#"{"path":"src/budget.rs","old_str":"    1","new_str":"    2"}"#,
            )])),
            3 => common::sse_tool_calls(json!([common::tc(
                "read", "code_context", r#"{"action":"read","path":"src/budget.rs","line":2}"#,
            )])),
            _ => common::sse_text("state: flow-verified\nverified: current native code context after an exact edit\nunverified: live provider quality, billing and cache hits"),
        }
    });
    std::fs::create_dir(fixture.0.join("home")).unwrap();
    std::fs::create_dir(fixture.0.join("tmp")).unwrap();
    let stdout = fixture.0.join("cli.stdout");
    let stderr = fixture.0.join("cli.stderr");
    let mut child = Command::new(env!("CARGO_BIN_EXE_sui"))
        .current_dir(&fixture.0)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", fixture.0.join("home"))
        .env("TMPDIR", fixture.0.join("tmp"))
        .env("SUI_HOME", fixture.0.join(".sui"))
        .args([
            "--base-url",
            &format!("http://127.0.0.1:{port}/v1"),
            "--model",
            "code-context-offline-fixture",
            "--api-key",
            "dummy-code-context-key",
            "--workspace",
        ])
        .arg(&fixture.0)
        .args(["--yolo", "Find retry budget and update its return value"])
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&stdout).unwrap())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(15);
    let finished = loop {
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
                panic!("CLI wait failed: {error}");
            }
        }
    };
    let status = child.wait().unwrap();
    assert!(
        finished && status.success(),
        "CLI did not finish:\n{}\n{}",
        std::fs::read_to_string(stdout).unwrap(),
        std::fs::read_to_string(stderr).unwrap()
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 4);
    assert!(requests[0]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .any(|tool| tool["function"]["name"] == "code_context"));
    let initial = requests[0]["messages"].as_array().unwrap();
    for request in &requests[1..] {
        assert_eq!(request["tools"], requests[0]["tools"]);
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
    let searched = result(&requests[1], "search");
    assert!(searched.contains("2 |     1"), "{searched}");
    let edited = result(&requests[2], "edit");
    assert!(edited.contains("status: success"), "{edited}");
    let current = result(&requests[3], "read");
    assert!(current.contains("2 |     2"), "{current}");
    assert!(!current.contains("2 |     1"));
    let source_hash = |text: &str| {
        text.lines()
            .find_map(|line| line.strip_prefix("source_sha256: "))
            .unwrap()
            .to_owned()
    };
    assert_ne!(source_hash(&searched), source_hash(&current));
    assert_eq!(
        std::fs::read_to_string(fixture.0.join("src/budget.rs")).unwrap(),
        "pub fn retry_budget() -> u32 {\n    2\n}\n"
    );
}

#[cfg(unix)]
const CHILD_ROOT: &str = "SUI_CODE_CONTEXT_CHILD_ROOT";

#[cfg(unix)]
#[test]
fn fifo_controls_repeated_cancellation_and_runtime_shutdown_are_bounded() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    use std::process::{Command, Stdio};
    let fixture = Fixture::new();
    fixture.write("guarded/hidden.rs", "pub fn guarded_private() {}\n");
    for path in ["guarded/.ignore", "pipe.rs"] {
        let fifo = CString::new(fixture.0.join(path).as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    }
    for index in 0..200 {
        fixture.write(&format!("modules/cancel_fixture_{index:03}.rs"),
            &format!("pub fn cancel_fixture_{index:03}() {{\n    let value = {index};\n    let _ = value;\n}}\n"));
    }
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "code_context_runtime_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(&fixture.0)
        .env_clear()
        .env(CHILD_ROOT, &fixture.0)
        .env("HOME", &fixture.0)
        .env("TMPDIR", &fixture.0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(10);
    let finished = loop {
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
                panic!("runtime child wait failed: {error}");
            }
        }
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "context worker/runtime did not finish:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("context runtime drained"));
}

#[cfg(unix)]
#[test]
fn code_context_runtime_child() {
    let Some(root) = std::env::var_os(CHILD_ROOT) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let ctx = context(Path::new(&root));
        let unsafe_search = execute(&ctx, json!({"action":"search","query":"guarded"})).await;
        assert_success(&unsafe_search);
        assert!(unsafe_search.text.contains("scan_complete: false"));
        assert!(!unsafe_search.text.contains("guarded_private"));
        let fifo_read = tools::execute(
            &ctx,
            "code_context",
            &json!({"action":"read","path":"pipe.rs","line":1}),
            std::future::pending(),
            None,
        )
        .await;
        if let Ok(read) = fifo_read {
            assert_eq!(read.kind, ExecKind::Error, "{}", read.text);
        }
        let args = json!({"action":"search","query":"cancel fixture","path":"modules"});
        let pre_cancelled =
            tools::execute(&ctx, "code_context", &args, std::future::ready(()), None)
                .await
                .unwrap();
        assert_eq!(
            pre_cancelled.kind,
            ExecKind::Cancelled,
            "{}",
            pre_cancelled.text
        );
        for _ in 0..3 {
            let out = tools::execute(
                &ctx,
                "code_context",
                &args,
                tokio::time::sleep(Duration::from_millis(1)),
                None,
            )
            .await
            .unwrap();
            assert!(
                matches!(out.kind, ExecKind::Cancelled | ExecKind::Success),
                "{}",
                out.text
            );
            // With one blocking thread, an abandoned worker would delay this
            // marker or prevent runtime shutdown. Explicit cancel must join.
            tokio::time::timeout(Duration::from_secs(1), tokio::task::spawn_blocking(|| ()))
                .await
                .expect("cancel retained a blocking worker")
                .unwrap();
        }
        let recovered = execute(&ctx, args).await;
        assert_success(&recovered);
        assert!(counter(&recovered, "selected") > 0);

        // Saturate the one-thread blocking pool before starting a new tool.
        // Its own three-second budget expires while queued; no cancellation
        // future ever resolves, so Cancelled would be a false observation.
        let (started, occupied) = tokio::sync::oneshot::channel();
        let (release, held) = std::sync::mpsc::channel();
        let blocker = tokio::task::spawn_blocking(move || {
            let _ = started.send(());
            held.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        occupied.await.unwrap();
        let queued_args = json!({"action":"read","path":"modules/cancel_fixture_000.rs","line":1});
        let delayed_release = async {
            tokio::time::sleep(Duration::from_millis(3300)).await;
            release.send(()).unwrap();
        };
        let (queued, ()) = tokio::join!(
            tools::execute(
                &ctx,
                "code_context",
                &queued_args,
                std::future::pending(),
                None
            ),
            delayed_release,
        );
        blocker.await.unwrap();
        let queued = queued.unwrap();
        assert_eq!(queued.kind, ExecKind::Timeout, "{}", queued.text);
        assert!(
            queued.text.contains("stop_reason: time_limit"),
            "{}",
            queued.text
        );
        assert!(!queued.text.starts_with("status: cancelled"));
    });
    drop(runtime);
    println!("context runtime drained");
}
