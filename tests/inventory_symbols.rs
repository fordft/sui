//! Regression proof for syntax forms that previously vanished from inventory.
use serde_json::json;
use std::path::PathBuf;
use std::time::Duration;
use sui::tools::{self, ExecKind, ExecOut, ToolContext};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-inventory-symbols-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    fn write(&self, path: &str, content: &str) {
        std::fs::write(self.0.join(path), content).unwrap();
    }

    async fn symbols(&self, path: &str) -> ExecOut {
        tools::execute(
            &ToolContext {
                workspace: self.0.clone(),
                bash_timeout: Duration::from_secs(1),
                bash_timeout_max: Duration::from_secs(1),
                web: None,
                canon_root: Default::default(),
                ui: Default::default(),
                code_intel: Default::default(),
                code_context: Default::default(),
            },
            "inventory",
            &json!({"action":"symbols","path":path,"limit":200}),
            std::future::pending(),
            None,
        )
        .await
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn rows(out: &ExecOut) -> Vec<&str> {
    out.text
        .split_once("content:\n")
        .unwrap()
        .1
        .lines()
        .filter(|line| !line.starts_with("hint:"))
        .collect()
}

#[tokio::test]
async fn trait_signatures_and_go_python_aliases_are_locations() {
    let fixture = Fixture::new();
    fixture.write(
        "traits.rs",
        "pub trait Service {\n    fn required(&self);\n    fn defaulted(&self) {}\n}\n",
    );
    fixture.write(
        "aliases.go",
        "package main\ntype Engine struct {}\ntype Alias = Engine\ntype Slice[T any] = []T\n",
    );
    fixture.write(
        "aliases.py",
        "type Alias = int\ntype Pair[T] = tuple[T, T]\n",
    );
    let out = fixture.symbols(".").await;
    assert_eq!(out.kind, ExecKind::Success);
    assert!(out.text.contains("scan_complete: true"), "{}", out.text);
    assert!(out.text.contains("syntax_error_files: 0"), "{}", out.text);
    for expected in [
        "traits.rs:1-4 interface Service",
        "traits.rs:2-2 function required",
        "traits.rs:3-3 function defaulted",
        "aliases.go:3-3 type Alias",
        "aliases.go:4-4 type Slice",
        "aliases.py:1-1 type Alias",
        "aliases.py:2-2 type Pair",
    ] {
        assert!(
            rows(&out).contains(&expected),
            "missing {expected}: {}",
            out.text
        );
    }
}

#[tokio::test]
async fn javascript_direct_bindings_include_wrappers_properties_and_classes() {
    let fixture = Fixture::new();
    fixture.write(
        "bindings.js",
        "const parenthesized = (/* comment */ () => 1);\n\
         const handlers = { process: (() => 2), generator: function* () {} };\n\
         exports.assigned = (function () {});\n\
         const Client = class { method() {} field = () => 1; };\n\
         const same = function same() {};\n\
         const outer = function inner() {};\n\
         const SameClass = class SameClass {};\n\
         const OuterClass = class InnerClass {};\n",
    );
    let out = fixture.symbols("bindings.js").await;
    assert!(!out.truncated, "{}", out.text);
    let rows = rows(&out);
    for expected in [
        "bindings.js:1-1 function parenthesized",
        "bindings.js:2-2 function process",
        "bindings.js:2-2 function generator",
        "bindings.js:3-3 function assigned",
        "bindings.js:4-4 class Client",
        "bindings.js:4-4 method method",
        "bindings.js:4-4 method field",
        "bindings.js:5-5 function same",
        "bindings.js:6-6 function outer",
        "bindings.js:6-6 function inner",
        "bindings.js:7-7 class SameClass",
        "bindings.js:8-8 class OuterClass",
        "bindings.js:8-8 class InnerClass",
    ] {
        assert!(rows.contains(&expected), "missing {expected}: {}", out.text);
        assert_eq!(rows.iter().filter(|row| **row == expected).count(), 1);
    }
    assert!(out.text.contains("matches_seen: 13"), "{}", out.text);
}

#[tokio::test]
async fn typescript_assertions_preserve_only_direct_initializer_bindings() {
    let fixture = Fixture::new();
    fixture.write(
        "assertions.ts",
        "type Handler = () => number;\n\
         const cast = (() => 1) as Handler;\n\
         const checked = (() => 2) satisfies Handler;\n\
         const asserted = <Handler>(() => 3);\n\
         const nonNull = (() => 4)!;\n\
         const nested = (((() => 5) as Handler) satisfies Handler)!;\n\
         class Service { run = (() => 6) as Handler; }\n",
    );
    let out = fixture.symbols("assertions.ts").await;
    assert!(!out.truncated, "{}", out.text);
    for expected in [
        "assertions.ts:2-2 function cast",
        "assertions.ts:3-3 function checked",
        "assertions.ts:4-4 function asserted",
        "assertions.ts:5-5 function nonNull",
        "assertions.ts:6-6 function nested",
        "assertions.ts:7-7 method run",
    ] {
        assert!(
            rows(&out).contains(&expected),
            "missing {expected}: {}",
            out.text
        );
    }
}

#[tokio::test]
async fn syntax_names_do_not_infer_callable_results_or_destructured_bindings() {
    let fixture = Fixture::new();
    fixture.write(
        "controls.js",
        "// function commentPretend() {}\n\
         const text = 'function stringPretend() {}';\n\
         const wrappedCall = memo(() => 1);\n\
         const { destructured } = (() => ({}));\n\
         const sequence = (0, () => 2);\n\
         const conditional = flag ? (() => 3) : (() => 4);\n\
         const handlers = { [computed]: () => 5 };\n\
         const real = (() => 6);\n",
    );
    let out = fixture.symbols("controls.js").await;
    assert!(!out.truncated, "{}", out.text);
    assert_eq!(rows(&out), ["controls.js:8-8 function real"]);
    assert!(out.text.contains("matches_seen: 1"), "{}", out.text);
}

#[tokio::test]
async fn long_names_are_reported_or_accounted_for_by_output_limits() {
    let fixture = Fixture::new();
    let long = format!("symbol_{}", "x".repeat(400));
    fixture.write("long.rs", &format!("fn {long}() {{}}\n"));
    let out = fixture.symbols("long.rs").await;
    assert!(!out.truncated, "{}", out.text);
    assert_eq!(rows(&out), [format!("long.rs:1-1 function {long}")]);
    assert!(out.text.contains("matches_seen: 1"), "{}", out.text);

    let huge = format!("symbol_{}", "x".repeat(25 * 1024));
    fixture.write("huge.rs", &format!("fn {huge}() {{}}\n"));
    let bounded = fixture.symbols("huge.rs").await;
    assert!(bounded.truncated);
    assert!(bounded.text.contains("matches_seen: 1"), "{}", bounded.text);
    assert!(bounded.text.contains("showing: 0"), "{}", bounded.text);
    assert!(
        bounded.text.contains("scan_complete: true"),
        "{}",
        bounded.text
    );
    assert!(bounded.text.len() < 25 * 1024);
}
