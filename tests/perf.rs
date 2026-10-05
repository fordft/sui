//! Perf harness — run with:
//!   cargo test --release --test perf -- --nocapture
//! Measures the per-turn context-assembly path at realistic session
//! sizes, plus a prefix-stability check (the cache-eligibility
//! invariant: request N's leading messages must be byte-identical to
//! request N-1's or provider prompt caching cannot hit).

use serde_json::json;
use std::time::Instant;
use sui::types::{FunctionCall, Message, ToolCall};

/// One turn's worth of history: user → assistant(+tool call) → tool.
fn push_turn(h: &mut Vec<Message>, i: usize) {
    h.push(Message::User {
        content: format!(
            "task step {i}: please update the module and verify the build {}",
            "x".repeat(160)
        )
        .into(),
    });
    h.push(Message::Assistant {
        content: Some(format!("I'll read the file first. {}", "y".repeat(300))),
        tool_calls: Some(vec![ToolCall {
            id: format!("call_{i}"),
            kind: "function".into(),
            function: FunctionCall {
                name: "read_file".into(),
                arguments: json!({"path": format!("src/m{i}.rs"), "limit": 400}).to_string(),
            },
        }]),
        reasoning_content: None,
        response_items: vec![],
    });
    h.push(Message::Tool {
        tool_call_id: format!("call_{i}"),
        content: format!("    1|use std::io;\n{}", "z".repeat(4000)),
    });
}

fn history(turns: usize) -> Vec<Message> {
    let mut h = Vec::new();
    for i in 0..turns {
        push_turn(&mut h, i);
    }
    h
}

fn ms(i: Instant) -> f64 {
    i.elapsed().as_secs_f64() * 1000.0
}

#[test]
fn per_turn_context_cost() {
    let system = sui::context::system();
    let guidance = Some("project rules here".to_string());
    for turns in [10, 40, 100] {
        let hist = history(turns);
        let bytes: usize = hist
            .iter()
            .map(|m| serde_json::to_string(m).unwrap().len())
            .sum();
        // what drive() does every turn: compile + estimate + fingerprint
        // (+ provider serializes messages into the request body)
        let t = Instant::now();
        let req = sui::context::compile(&hist, &system, guidance.as_deref());
        let c_compile = ms(t);
        let t = Instant::now();
        let est = sui::context::estimate_tokens(&req);
        let c_est = ms(t);
        let t = Instant::now();
        let _fp = sui::context::request_fingerprint(&req);
        let c_fp = ms(t);
        let t = Instant::now();
        let body = json!({"model": "m", "messages": req, "tools": [],
            "stream": true});
        let _wire = serde_json::to_string(&body).unwrap();
        let c_send = ms(t);
        eprintln!(
            "turns={turns:3} ctx={bytes:6}B (~{est} tok) | compile {c_compile:7.3}ms \
             est {c_est:7.3}ms fp {c_fp:7.3}ms send {c_send:7.3}ms | total {:.3}ms",
            c_compile + c_est + c_fp + c_send
        );
    }
}

/// Cache eligibility: request N's prefix must equal request N-1's
/// serialization byte-for-byte, or the provider cache cannot hit.
#[test]
fn prefix_is_cache_stable() {
    let system = sui::context::system();
    let mut hist = Vec::new();
    let mut prev_wire = String::new();
    for turn in 0..3 {
        push_turn(&mut hist, turn);
        let req = sui::context::compile(&hist, &system, None);
        let wire = serde_json::to_string(&req).unwrap();
        if !prev_wire.is_empty() {
            assert!(
                wire.starts_with(&prev_wire[..prev_wire.len() - 1]),
                "turn {turn}: request prefix drifted — provider cache would miss"
            );
        }
        prev_wire = wire;
    }
}

#[test]
fn resolve_cost() {
    let ws = std::env::temp_dir().join("sui-perf-ws");
    std::fs::create_dir_all(ws.join("src/deep")).unwrap();
    let ctx = sui::tools::ToolContext {
        workspace: ws.clone(),
        bash_timeout: std::time::Duration::from_secs(1),
        bash_timeout_max: std::time::Duration::from_secs(1),
        web: None,
        canon_root: std::sync::OnceLock::new(),
        ui: std::sync::OnceLock::new(),
        code_intel: Default::default(),
        code_context: Default::default(),
        tool_outputs: Default::default(),
    };
    let n = 200;
    let t = Instant::now();
    for _ in 0..n {
        let _ = sui::tools::fs::resolve_ctx(&ctx, "src/deep/f.rs").unwrap();
    }
    let total = ms(t);
    eprintln!(
        "resolve_ctx x{n}: {total:.3}ms total, {:.4}ms/call",
        total / n as f64
    );
    let t = Instant::now();
    for _ in 0..n {
        let _ = sui::tools::fs::resolve(&ws, "src/deep/f.rs").unwrap();
    }
    let total2 = ms(t);
    eprintln!(
        "resolve (uncached) x{n}: {total2:.3}ms total, {:.4}ms/call",
        total2 / n as f64
    );
}

/// TUI projection cost: what one draw() pays per frame during streaming.
#[test]
fn transcript_rows_cost() {
    use std::collections::BTreeMap;
    use sui::events::UiEvent;
    use sui::tui::app::App;
    use sui::tui::transcript;

    let ws = std::env::temp_dir().join("sui-perf-tui");
    std::fs::create_dir_all(&ws).unwrap();
    let mut app = App::with_state(ws, BTreeMap::new(), Default::default());

    // 3 finished groups: each 30 tools + a fat final assistant answer
    for g in 0..3u64 {
        let run = g + 1;
        app.apply_event(UiEvent::ReqStart {
            run,
            agent: "solo".into(),
            req: 0,
        });
        for i in 0..30 {
            app.apply_event(UiEvent::ToolStart {
                run,
                agent: "solo".into(),
                req: 0,
                call: format!("c{i}"),
                name: "bash".into(),
                summary: format!("bash: cmd {i}"),
            });
            app.apply_event(UiEvent::ToolDone {
                run,
                agent: "solo".into(),
                call: format!("c{i}"),
                summary: format!("bash: cmd {i}"),
                name: "bash".into(),
                ms: 12,
                status: sui::events::ToolStatus::Ok,
                exit: Some(0),
                result: "ok".repeat(2000),
                truncated: false,
                dropped: 0,
            });
        }
        app.apply_event(UiEvent::Delta {
            run,
            agent: "solo".into(),
            req: 0,
            text: "a".repeat(30_000),
        });
        app.apply_event(UiEvent::RunDone {
            run,
            outcome: "done".into(),
            accepted_sha: None,
        });
    }
    // live group: streaming assistant text
    app.apply_event(UiEvent::ReqStart {
        run: 9,
        agent: "solo".into(),
        req: 0,
    });
    for _ in 0..500 {
        app.apply_event(UiEvent::Delta {
            run: 9,
            agent: "solo".into(),
            req: 0,
            text: "streaming chunk ".into(),
        });
    }

    for w in [80usize, 120] {
        let t = Instant::now();
        let n = 20;
        let mut rows = 0;
        for _ in 0..n {
            rows = transcript::rows(&app, w).len();
        }
        let per = ms(t) / n as f64;
        eprintln!(
            "rows() w={w}: {per:.3}ms/frame ({rows} rows) → {:.1}ms CPU/s at 30fps",
            per * 30.0
        );
    }
}

/// A/B: does Compiled serialize as fast as the old Vec<Message> path?
#[test]
fn compiled_vs_vec_serialize() {
    let system = sui::context::system();
    let hist = history(100);
    let compiled = sui::context::compile(&hist, &system, None);
    let vec: Vec<Message> = compiled.iter().cloned().collect();
    for _ in 0..3 {
        let t = Instant::now();
        let a = serde_json::to_string(&compiled).unwrap();
        let ca = ms(t);
        let t = Instant::now();
        let b = serde_json::to_string(&vec).unwrap();
        let cb = ms(t);
        assert_eq!(a, b, "wire bytes must be identical");
        eprintln!("compiled {ca:.3}ms vs vec {cb:.3}ms");
    }
    // and through the json! body wrap used by providers
    for _ in 0..3 {
        let t = Instant::now();
        let a = serde_json::to_string(&json!({"messages": compiled})).unwrap();
        let ca = ms(t);
        let t = Instant::now();
        let b = serde_json::to_string(&json!({"messages": vec})).unwrap();
        let cb = ms(t);
        assert_eq!(a, b);
        eprintln!("json! compiled {ca:.3}ms vs vec {cb:.3}ms");
    }
}
