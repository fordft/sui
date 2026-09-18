//! Export tests: fixture journals → sanitized report. The TUI and CLI
//! share sui::export; these tests exercise the same path the TUI does
//! (run dir on disk, not in-memory state).

use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use sui::export::{run_export, ExportOpts, Format};

fn fixture_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sui-exp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn jline(kind: &str, data: Value) -> String {
    json!({"ts_unix": 1_700_000_000_000_u64, "type": kind, "data": data}).to_string()
}

fn write_journal(dir: &Path, name: &str, lines: &[String]) {
    let mut f = std::fs::File::create(dir.join(format!("{name}.jsonl"))).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

fn opts(runs: &Path, out: &Path, run: &str) -> ExportOpts {
    ExportOpts {
        run_id: Some(run.into()),
        latest_for_workspace: None,
        format: Format::Markdown,
        include_diff: false,
        runs_root: Some(runs.to_path_buf()),
        out_root: Some(out.to_path_buf()),
        running: false,
    }
}

fn solo_run(dir: &Path) {
    // a solo turn: session → task → user → request → assistant w/ tool → tool → done
    write_journal(
        dir,
        "solo",
        &[
            jline(
                "session",
                json!({"mode":"solo","workspace":"/tmp/proj","sui_version":"0.0.0-test","approval":"ask"}),
            ),
            jline("task", json!({"task":"fix the bug","approval":"ask"})),
            jline("user", json!({"content":"fix the bug"})),
            jline(
                "request",
                json!({
                    "request_id":0,"agent_id":"solo","role":"worker",
                    "provider_profile":"http://x/v1","requested_model":"m1","returned_model":"m1",
                    "usage":{"input_tokens":10,"output_tokens":5,"complete":true},
                    "timing":{"request_total_ms":100},"finish_reason":"tool_calls",
                }),
            ),
            jline(
                "assistant",
                json!({
                    "content":"I'll run the tests.",
                    "reasoning_content":"private chain-of-thought must never appear",
                    "tool_calls":[{"id":"c1","function":{"name":"bash","arguments":"{\"command\":\"cargo test -- --api sk-livesecret0123456789\"}"}}],
                }),
            ),
            jline(
                "tool",
                json!({
                    "tool_call_id":"c1","name":"bash",
                    "args":"{\"command\":\"cargo test -- --api sk-livesecret0123456789\"}",
                    "executed":true,"execution_ms":1200,
                    "result":"status: failed\nexit_code: 1\nstdout: test failed\nstderr: hint: use --key sk-livesecret0123456789\ntruncated: false",
                }),
            ),
            jline("task_done", json!({"outcome":"done"})),
        ],
    );
}

#[test]
fn solo_run_renders_and_redacts() {
    let root = fixture_dir("solo");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("tui-1-1");
    std::fs::create_dir_all(&run).unwrap();
    solo_run(&run);

    let p = run_export(&opts(&runs, &out, "tui-1-1")).unwrap();
    let md = std::fs::read_to_string(&p).unwrap();

    assert!(md.contains("fix the bug"), "objective");
    assert!(md.contains("Executed but failed"), "exit 1 → failed");
    assert!(md.contains("exit_code: 1"), "exit code visible");
    assert!(md.contains("ask"), "approval mode");
    assert!(md.contains("**Mode:** solo"));
    // secret must not appear anywhere
    assert!(
        !md.contains("sk-livesecret0123456789"),
        "key leaked in args/result"
    );
    assert!(md.contains("«redacted"), "redaction marker present");
    // private reasoning never exported
    assert!(!md.contains("private chain-of-thought"));
    // file perms 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn json_format_is_structured_and_redacted() {
    let root = fixture_dir("json");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("tui-2-1");
    std::fs::create_dir_all(&run).unwrap();
    solo_run(&run);

    let mut o = opts(&runs, &out, "tui-2-1");
    o.format = Format::Json;
    let p = run_export(&o).unwrap();
    assert!(p.ends_with("report.json"));
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
    assert_eq!(v["run_id"], "tui-2-1");
    assert_eq!(v["mode"], "solo");
    assert!(v["usage"]["per_agent"][0]["input_tokens"].is_number());
    // redaction inside JSON too
    let raw = std::fs::read_to_string(&p).unwrap();
    assert!(!raw.contains("sk-livesecret0123456789"));
}

#[test]
fn malformed_and_incomplete_lines_tolerated() {
    let root = fixture_dir("bad");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("tui-3-1");
    std::fs::create_dir_all(&run).unwrap();
    write_journal(
        &run,
        "solo",
        &[
            jline("user", json!({"content":"do it"})),
            "{ not json".into(),
            "{\"type\":\"request\",\"data\":{\"agent_id\":\"solo\"".to_string(),
        ],
    );
    let p = run_export(&opts(&runs, &out, "tui-3-1")).unwrap();
    let md = std::fs::read_to_string(&p).unwrap();
    assert!(
        md.contains("malformed journal line"),
        "limitations must flag skipped lines"
    );
    assert!(md.contains("do it"));
}

#[test]
fn missing_usage_is_unknown_not_zero() {
    let root = fixture_dir("usage");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("tui-4-1");
    std::fs::create_dir_all(&run).unwrap();
    write_journal(
        &run,
        "solo",
        &[
            jline("user", json!({"content":"x"})),
            jline(
                "request",
                json!({
                    "request_id":0,"agent_id":"solo","role":"worker",
                    "provider_profile":"http://x","requested_model":"m",
                    "usage": null, "finish_reason":"stop",
                }),
            ),
            jline("task_done", json!({"outcome":"done"})),
        ],
    );
    let p = run_export(&opts(&runs, &out, "tui-4-1")).unwrap();
    let md = std::fs::read_to_string(&p).unwrap();
    assert!(
        md.contains("Unknown"),
        "missing usage → Unknown, never zero"
    );
    assert!(!md.contains("| solo | 1 | 0 |"), "must not fabricate zeros");
}

#[test]
fn running_snapshot_labeled() {
    let root = fixture_dir("live");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("tui-5-1");
    std::fs::create_dir_all(&run).unwrap();
    write_journal(
        &run,
        "solo",
        &[
            jline("session", json!({"mode":"solo","workspace":"/w"})),
            jline("user", json!({"content":"go"})),
        ],
    );
    let mut o = opts(&runs, &out, "tui-5-1");
    o.running = true;
    let p = run_export(&o).unwrap();
    let md = std::fs::read_to_string(&p).unwrap();
    assert!(md.contains("RUNNING — partial snapshot"));
}

#[test]
fn unicode_and_cancelled() {
    let root = fixture_dir("uni");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("tui-6-1");
    std::fs::create_dir_all(&run).unwrap();
    write_journal(
        &run,
        "solo",
        &[
            jline("user", json!({"content":"修正 tëst — 🚀 émoji"})),
            jline("interrupted", json!({"request_id":0,"phase":"tool"})),
            jline(
                "tool",
                json!({"tool_call_id":"x","name":"bash","args":"{}","executed":false,"execution_ms":1,"result":"status: cancelled\nerror: interrupted by user"}),
            ),
        ],
    );
    let p = run_export(&opts(&runs, &out, "tui-6-1")).unwrap();
    let md = std::fs::read_to_string(&p).unwrap();
    assert!(md.contains("修正 tëst — 🚀 émoji"));
    assert!(md.contains("Cancelled"));
    assert!(md.contains("interrupted"));
}

#[test]
fn run_id_prefix_resolves() {
    let root = fixture_dir("prefix");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("tui-7777-abc");
    std::fs::create_dir_all(&run).unwrap();
    solo_run(&run);
    let p = run_export(&opts(&runs, &out, "tui-7777")).unwrap();
    assert!(p.to_string_lossy().contains("tui-7777-abc"));
}

#[test]
fn latest_picks_matching_workspace() {
    let root = fixture_dir("latest");
    let runs = root.join("runs");
    let out = root.join("exports");
    // older dir: other workspace
    let a = runs.join("tui-old-1");
    std::fs::create_dir_all(&a).unwrap();
    write_journal(
        &a,
        "solo",
        &[
            jline("session", json!({"mode":"solo","workspace":"/other"})),
            jline("user", json!({"content":"old"})),
        ],
    );
    // newer dir: our workspace
    let b = runs.join("tui-new-1");
    std::fs::create_dir_all(&b).unwrap();
    write_journal(
        &b,
        "solo",
        &[
            jline("session", json!({"mode":"solo","workspace":"/ws"})),
            jline("user", json!({"content":"new"})),
        ],
    );
    std::thread::sleep(std::time::Duration::from_millis(30));
    // touch b later so mtime ordering picks it
    std::fs::write(b.join("marker"), b"x").unwrap();

    let mut o = opts(&runs, &out, "");
    o.run_id = None;
    o.latest_for_workspace = Some(PathBuf::from("/ws"));
    let p = run_export(&o).unwrap();
    assert!(p.to_string_lossy().contains("tui-new-1"), "{p:?}");
}

#[test]
fn headless_journal_exports_session() {
    // Regression: headless used to journal `session_start`, which no
    // reader recognised — exports always carried "no session event".
    let root = fixture_dir("headless");
    let runs = root.join("runs");
    let out = root.join("exports");
    let run = runs.join("run-h1");
    std::fs::create_dir_all(&run).unwrap();
    write_journal(
        &run,
        "headless",
        &[
            jline(
                "session",
                json!({"mode":"headless","workspace":"/w","sui_version":"x","approval":"auto"}),
            ),
            jline("user", json!({"content":"go"})),
        ],
    );
    let p = run_export(&opts(&runs, &out, "run-h1")).unwrap();
    let md = std::fs::read_to_string(&p).unwrap();
    assert!(!md.contains("no session event"), "{md}");
    assert!(md.contains("headless"));

    // --latest must be able to match a headless run by workspace.
    let mut o = opts(&runs, &out, "");
    o.run_id = None;
    o.latest_for_workspace = Some(PathBuf::from("/w"));
    let p = run_export(&o).unwrap();
    assert!(p.to_string_lossy().contains("run-h1"), "{p:?}");
}
