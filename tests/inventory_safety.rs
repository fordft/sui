//! Regression proof for inventory traversal, guarded ignore loading and cleanup.
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::Duration;
use sui::tools::{self, ExecKind, ExecOut, ToolContext};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-inventory-safety-{}-{:x}",
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
        code_intel: Default::default(),
        code_context: Default::default(),
    }
}

async fn inventory(ctx: &ToolContext, args: Value) -> ExecOut {
    tools::execute(ctx, "inventory", &args, std::future::pending(), None)
        .await
        .unwrap()
}

fn assert_unsafe_ignore(out: &ExecOut) {
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert!(
        out.text.to_lowercase().contains("ignore"),
        "unsafe ignore file needs an explicit reason: {}",
        out.text
    );
    assert!(
        out.truncated,
        "unsafe ignore files cannot prove completeness"
    );
    assert!(out.text.contains("scan_complete: false"), "{}", out.text);
    assert!(counter(out, "files_skipped") > 0, "{}", out.text);
}

fn counter(out: &ExecOut, name: &str) -> usize {
    out.text
        .lines()
        .find_map(|line| line.strip_prefix(&format!("{name}: ")))
        .unwrap_or_else(|| panic!("missing {name}: {}", out.text))
        .parse()
        .unwrap()
}

#[tokio::test]
async fn source_directories_named_build_and_target_are_searchable() {
    let fixture = Fixture::new();
    fixture.write("src/build/planner.rs", "fn build_plan() {}\n");
    fixture.write("src/target/lower.rs", "fn lower_target() {}\n");
    fixture.write("target/generated.rs", "fn generated_artifact() {}\n");
    fixture.write("build/planner.rs", "fn root_build_plan() {}\n");
    fixture.write("dist/export.rs", "fn distribution_export() {}\n");
    fixture.write("coverage/result.rs", "fn coverage_result() {}\n");
    fixture.write(
        "node_modules/package/index.js",
        "function installed_dependency() {}\n",
    );
    let ctx = fixture.ctx();
    let all = inventory(&ctx, json!({"action":"symbols"})).await;
    assert_eq!(all.kind, ExecKind::Success);
    assert!(!all.truncated, "{}", all.text);
    assert!(all
        .text
        .contains("src/build/planner.rs:1-1 function build_plan"));
    assert!(all
        .text
        .contains("src/target/lower.rs:1-1 function lower_target"));
    assert!(!all.text.contains("generated_artifact"));
    assert!(!all.text.contains("root_build_plan"));
    assert!(!all.text.contains("distribution_export"));
    assert!(!all.text.contains("coverage_result"));
    assert!(!all.text.contains("installed_dependency"));

    let file = inventory(
        &ctx,
        json!({"action":"symbols","path":"src/build/planner.rs"}),
    )
    .await;
    assert_eq!(file.kind, ExecKind::Success);
    assert!(!file.truncated, "{}", file.text);
    assert!(file.text.contains("function build_plan"));
    assert!(!file.text.contains("function lower_target"));

    let directory = inventory(&ctx, json!({"action":"symbols","path":"src/target"})).await;
    assert_eq!(directory.kind, ExecKind::Success);
    assert!(!directory.truncated, "{}", directory.text);
    assert!(directory.text.contains("function lower_target"));
    assert!(!directory.text.contains("function build_plan"));

    for (path, symbol) in [
        ("target/generated.rs", "generated_artifact"),
        ("build", "root_build_plan"),
        ("dist", "distribution_export"),
        ("coverage", "coverage_result"),
    ] {
        let scoped = inventory(&ctx, json!({"action":"symbols","path":path})).await;
        assert_eq!(scoped.kind, ExecKind::Success, "{}", scoped.text);
        assert!(!scoped.truncated, "{}", scoped.text);
        assert!(
            scoped.text.contains(&format!("function {symbol}")),
            "{}",
            scoped.text
        );
    }
    let dependency = inventory(
        &ctx,
        json!({"action":"symbols","path":"node_modules/package/index.js"}),
    )
    .await;
    assert!(!dependency.text.contains("function installed_dependency"));
    assert!(
        dependency.kind == ExecKind::Error || dependency.text.contains("content:\n<empty>"),
        "{}",
        dependency.text
    );
}

#[tokio::test]
async fn explicit_scopes_preserve_sensitive_file_and_metadata_denials() {
    let fixture = Fixture::new();
    fixture.write("visible.rs", "fn visible() {}\n");
    let sensitive_paths = [
        ".env.rs",
        "src/.env.local.rs",
        "src/auth.json",
        "src/credentials.json",
        "src/client.key",
        "src/certificate.pem",
        ".git/private.rs",
        ".sui/private.rs",
    ];
    for path in sensitive_paths {
        fixture.write(path, "fn private_marker() {}\n");
    }
    let ctx = fixture.ctx();
    let all = inventory(&ctx, json!({"action":"files"})).await;
    assert!(all.text.contains("visible.rs [rust]"));
    for hidden in sensitive_paths {
        assert!(!all.text.contains(hidden), "exposed {hidden}: {}", all.text);
        let scoped = inventory(&ctx, json!({"action":"files","path":hidden})).await;
        assert!(
            scoped.kind == ExecKind::Error || scoped.text.contains("content:\n<empty>"),
            "sensitive explicit scope was exposed: {}",
            scoped.text
        );
        assert!(!scoped.text.contains("private_marker"));
    }
}

#[tokio::test]
async fn exact_file_scopes_keep_parent_and_nested_ignore_rules() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "src/root_hidden.rs\n");
    fixture.write("src/.gitignore", "nested_hidden.rs\n");
    fixture.write("src/.ignore", "custom_hidden.rs\n");
    fixture.write("src/root_hidden.rs", "fn root_hidden() {}\n");
    fixture.write("src/nested_hidden.rs", "fn nested_hidden() {}\n");
    fixture.write("src/custom_hidden.rs", "fn custom_hidden() {}\n");
    fixture.write("src/visible.rs", "fn visible() {}\n");
    let ctx = fixture.ctx();
    for filename in ["root_hidden", "nested_hidden", "custom_hidden"] {
        let out = inventory(
            &ctx,
            json!({"action":"symbols","path":format!("src/{filename}.rs")}),
        )
        .await;
        assert!(
            out.kind == ExecKind::Error || out.text.contains("content:\n<empty>"),
            "explicit scope bypassed ignore rules: {}",
            out.text
        );
        assert!(!out.text.contains(&format!("function {filename}")));
    }
    let visible = inventory(&ctx, json!({"action":"symbols","path":"src/visible.rs"})).await;
    assert_eq!(visible.kind, ExecKind::Success);
    assert!(!visible.truncated, "{}", visible.text);
    assert!(visible.text.contains("function visible"));
}

#[tokio::test]
async fn ignore_precedence_bom_and_parent_directory_negation_are_preserved() {
    let fixture = Fixture::new();
    fixture.write(".gitignore", "\u{feff}*.rs\nblocked/\n!blocked/keep.rs\n");
    fixture.write(".ignore", "!family_visible.rs\n");
    fixture.write("src/.gitignore", "!nearest_visible.rs\nfamily_visible.rs\n");
    fixture.write("src/.ignore", "local_hidden.rs\n!local_visible.rs\n");
    for name in [
        "family_visible",
        "nearest_visible",
        "local_visible",
        "local_hidden",
        "bom_hidden",
    ] {
        fixture.write(&format!("src/{name}.rs"), &format!("fn {name}() {{}}\n"));
    }
    fixture.write("blocked/keep.rs", "fn blocked_child() {}\n");
    let out = inventory(&fixture.ctx(), json!({"action":"symbols"})).await;
    assert_eq!(out.kind, ExecKind::Success);
    assert!(!out.truncated, "{}", out.text);
    for visible in ["family_visible", "nearest_visible", "local_visible"] {
        assert!(
            out.text.contains(&format!("function {visible}")),
            "{}",
            out.text
        );
    }
    for hidden in ["local_hidden", "bom_hidden", "blocked_child"] {
        assert!(
            !out.text.contains(&format!("function {hidden}")),
            "{}",
            out.text
        );
    }
}

#[tokio::test]
async fn workspace_git_excludes_and_nested_repository_boundaries_are_preserved() {
    let fixture = Fixture::new();
    fixture.write(".git/HEAD", "ref: refs/heads/main\n");
    fixture.write(".git/info/exclude", "root_info_hidden.rs\n");
    fixture.write(".gitignore", "root_blocked.rs\nnested/*.rs\n");
    fixture.write(".ignore", "always_hidden.rs\n");
    fixture.write("root_visible.rs", "fn root_visible() {}\n");
    fixture.write("root_info_hidden.rs", "fn root_info_hidden() {}\n");
    fixture.write("root_blocked.rs", "fn root_blocked() {}\n");
    fixture.write("nested/.git/HEAD", "ref: refs/heads/main\n");
    fixture.write("nested/.git/info/exclude", "nested_info_hidden.rs\n");
    fixture.write("nested/local.rs", "fn nested_visible() {}\n");
    fixture.write(
        "nested/nested_info_hidden.rs",
        "fn nested_info_hidden() {}\n",
    );
    fixture.write("nested/always_hidden.rs", "fn always_hidden() {}\n");
    let ctx = fixture.ctx();
    for args in [
        json!({"action":"symbols"}),
        json!({"action":"symbols","path":"nested"}),
    ] {
        let out = inventory(&ctx, args).await;
        assert_eq!(out.kind, ExecKind::Success);
        assert!(!out.truncated, "{}", out.text);
        assert!(out.text.contains("function nested_visible"), "{}", out.text);
        for hidden in [
            "root_info_hidden",
            "root_blocked",
            "nested_info_hidden",
            "always_hidden",
        ] {
            assert!(
                !out.text.contains(&format!("function {hidden}")),
                "{}",
                out.text
            );
        }
    }
    let direct = inventory(&ctx, json!({"action":"symbols","path":"nested/local.rs"})).await;
    assert!(
        direct.text.contains("function nested_visible"),
        "{}",
        direct.text
    );
    assert!(!direct.truncated, "{}", direct.text);
}

#[tokio::test]
async fn exact_file_scope_does_not_enumerate_wide_sibling_directories() {
    let fixture = Fixture::new();
    for n in 0..300 {
        fixture.write(&format!("src/deep/sibling_{n:03}.txt"), "ignored sibling\n");
        fixture.write(&format!("root_sibling_{n:03}.txt"), "ignored sibling\n");
    }
    fixture.write(".gitignore", "src/deep/*.txt\n");
    fixture.write("src/deep/selected.rs", "fn selected() {}\n");
    let out = inventory(
        &fixture.ctx(),
        json!({"action":"symbols","path":"src/deep/selected.rs"}),
    )
    .await;
    assert_eq!(out.kind, ExecKind::Success);
    assert!(!out.truncated, "{}", out.text);
    assert!(out.text.contains("function selected"), "{}", out.text);
    assert!(
        counter(&out, "entries_scanned") <= 8,
        "exact scope must inspect its ancestor chain, not hundreds of siblings: {}",
        out.text
    );
}

#[tokio::test]
async fn read_budget_accounts_for_rejected_content_and_skips_oversize_before_reading() {
    let fixture = Fixture::new();
    let rules = "# inventory fixture\n";
    let visible = "fn visible() {}\n";
    let binary = b"\0binary";
    let invalid_utf8 = [0xffu8; 16];
    fixture.write(".gitignore", rules);
    fixture.write("visible.rs", visible);
    std::fs::write(fixture.0.join("binary.rs"), binary).unwrap();
    std::fs::write(fixture.0.join("invalid.rs"), invalid_utf8).unwrap();
    let oversized = std::fs::File::create(fixture.0.join("oversized.rs")).unwrap();
    oversized.set_len(512 * 1024 + 1).unwrap();
    let out = inventory(&fixture.ctx(), json!({"action":"symbols"})).await;
    assert_eq!(out.kind, ExecKind::Success);
    assert!(out.truncated);
    assert!(out.text.contains("scan_complete: false"), "{}", out.text);
    assert!(out.text.contains("function visible"), "{}", out.text);
    assert_eq!(counter(&out, "files_skipped"), 3, "{}", out.text);
    assert_eq!(
        counter(&out, "bytes_read"),
        rules.len() + visible.len() + binary.len() + invalid_utf8.len(),
        "all source/control bytes count, including rejected content; metadata-known oversized files are not read: {}",
        out.text
    );
}

#[cfg(unix)]
#[tokio::test]
async fn ignore_symlinks_fail_closed_without_hiding_unaffected_subtrees() {
    for name in [".gitignore", ".ignore"] {
        for external in [false, true] {
            let fixture = Fixture::new();
            let outside = Fixture::new();
            fixture.write("visible.rs", "fn visible() {}\n");
            fixture.write("nested/private.rs", "fn guarded_private() {}\n");
            outside.write("rules", "private.rs\n");
            fixture.write("rules", "private.rs\n");
            let rules = if external {
                outside.0.join("rules")
            } else {
                fixture.0.join("rules")
            };
            std::os::unix::fs::symlink(&rules, fixture.0.join("nested").join(name)).unwrap();
            let ctx = fixture.ctx();
            let out = inventory(&ctx, json!({"action":"symbols"})).await;
            assert_unsafe_ignore(&out);
            assert!(
                !out.text.contains("function guarded_private"),
                "{}",
                out.text
            );
            assert!(out.text.contains("function visible"), "{}", out.text);
            let scoped =
                inventory(&ctx, json!({"action":"symbols","path":"nested/private.rs"})).await;
            assert_unsafe_ignore(&scoped);
            assert!(!scoped.text.contains("function guarded_private"));
        }
    }
}

#[tokio::test]
async fn invalid_or_non_regular_ignore_files_prune_the_affected_subtree() {
    for name in [".gitignore", ".ignore"] {
        for directory in [false, true] {
            let fixture = Fixture::new();
            fixture.write("visible.rs", "fn visible() {}\n");
            fixture.write("nested/private.rs", "fn guarded_private() {}\n");
            let control = fixture.0.join("nested").join(name);
            if directory {
                std::fs::create_dir(control).unwrap();
            } else {
                std::fs::write(control, [0xff]).unwrap();
            }
            let out = inventory(&fixture.ctx(), json!({"action":"symbols"})).await;
            assert_unsafe_ignore(&out);
            assert!(out.text.contains("function visible"), "{}", out.text);
            assert!(
                !out.text.contains("function guarded_private"),
                "{}",
                out.text
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn git_info_exclude_symlinks_are_guarded_like_other_ignore_files() {
    for through_directory in [false, true] {
        let fixture = Fixture::new();
        let outside = Fixture::new();
        fixture.write(".git/HEAD", "ref: refs/heads/main\n");
        fixture.write("private.rs", "fn guarded_private() {}\n");
        outside.write("exclude", "private.rs\n");
        if through_directory {
            std::os::unix::fs::symlink(&outside.0, fixture.0.join(".git/info")).unwrap();
        } else {
            std::fs::create_dir(fixture.0.join(".git/info")).unwrap();
            std::os::unix::fs::symlink(
                outside.0.join("exclude"),
                fixture.0.join(".git/info/exclude"),
            )
            .unwrap();
        }
        let out = inventory(&fixture.ctx(), json!({"action":"symbols"})).await;
        assert_unsafe_ignore(&out);
        assert!(!out.text.contains("function guarded_private"));
    }
}

#[tokio::test]
async fn external_git_pointer_files_are_not_followed_for_ignore_rules() {
    let fixture = Fixture::new();
    let outside = Fixture::new();
    fixture.write("visible.rs", "fn visible() {}\n");
    outside.write("info/exclude", "visible.rs\n");
    fixture.write(".git", &format!("gitdir: {}\n", outside.0.display()));
    let out = inventory(&fixture.ctx(), json!({"action":"symbols"})).await;
    assert_eq!(out.kind, ExecKind::Success);
    assert!(!out.truncated, "{}", out.text);
    assert!(out.text.contains("function visible"), "{}", out.text);
}

#[tokio::test]
async fn ignore_files_over_64_kib_are_rejected_before_reading() {
    for name in [".gitignore", ".ignore"] {
        let fixture = Fixture::new();
        fixture.write(name, &"#".repeat(64 * 1024 + 1));
        fixture.write("private.rs", "fn guarded_private() {}\n");
        let out = inventory(&fixture.ctx(), json!({"action":"symbols"})).await;
        assert_unsafe_ignore(&out);
        assert!(!out.text.contains("function guarded_private"));
        assert_eq!(
            counter(&out, "bytes_read"),
            0,
            "metadata-known oversized controls must not consume input bytes: {}",
            out.text
        );
    }
}

#[tokio::test]
async fn invalid_ignore_contents_fail_closed_with_an_explicit_warning() {
    let cases = [
        ("line over 4 KiB", "#".repeat(4 * 1024 + 1)),
        ("NUL content", "private.rs\0\n".to_owned()),
        // An unclosed '[' is a valid literal under gitignore compatibility;
        // the descending range is a glob parse error in the locked dependency.
        ("malformed glob", "[z-a]\n".to_owned()),
    ];
    for name in [".gitignore", ".ignore"] {
        for (case, control) in &cases {
            let fixture = Fixture::new();
            fixture.write("visible.rs", "fn visible() {}\n");
            fixture.write("nested/private.rs", "fn guarded_private() {}\n");
            fixture.write(&format!("nested/{name}"), control);
            let out = inventory(&fixture.ctx(), json!({"action":"symbols"})).await;
            assert_unsafe_ignore(&out);
            assert!(
                out.text.contains("function visible"),
                "healthy sibling was pruned for {name} {case}: {}",
                out.text
            );
            assert!(
                !out.text.contains("function guarded_private"),
                "unsafe subtree was exposed for {name} {case}: {}",
                out.text
            );
            assert!(
                counter(&out, "bytes_read") >= control.len(),
                "rejected control bytes still consume the budget for {name} {case}: {}",
                out.text
            );
        }
    }
}

#[cfg(unix)]
const FIFO_CHILD_ROOT: &str = "SUI_INVENTORY_FIFO_CHILD_ROOT";
#[cfg(unix)]
const FIFO_CHILD_CANCEL: &str = "SUI_INVENTORY_FIFO_CHILD_CANCEL";
#[cfg(unix)]
const FIFO_CHILD_ACTION: &str = "SUI_INVENTORY_FIFO_CHILD_ACTION";

#[cfg(unix)]
#[test]
fn fifo_ignore_reads_and_cancellation_finish_the_runtime() {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    for name in [".gitignore", ".ignore"] {
        for action in ["files", "symbols"] {
            let fixture = Fixture::new();
            fixture.write("private.rs", "fn guarded_private() {}\n");
            let fifo = CString::new(fixture.0.join(name).as_os_str().as_bytes()).unwrap();
            // The fixture path owns this FIFO; there is deliberately no writer.
            assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
            for cancel in [false, true] {
                run_fifo_child(&fixture.0, action, cancel);
            }
        }
    }
}

#[cfg(unix)]
fn run_fifo_child(root: &Path, action: &str, cancel: bool) {
    use std::process::{Command, Stdio};
    use std::time::Instant;
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "inventory_fifo_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .current_dir(root)
        .env_clear()
        .env(FIFO_CHILD_ROOT, root)
        .env(FIFO_CHILD_CANCEL, if cancel { "1" } else { "0" })
        .env(FIFO_CHILD_ACTION, action)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let finished = loop {
        match child.try_wait() {
            Ok(Some(_)) => break true,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                break false;
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("cannot wait for inventory child: {err}");
            }
        }
    };
    let output = child.wait_with_output().unwrap();
    assert!(
        finished && output.status.success(),
        "FIFO inventory child did not finish and drain its worker (action={action}, cancel={cancel}, finished={finished}):\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("inventory worker drained"));
}

#[cfg(unix)]
#[test]
fn inventory_fifo_child() {
    let Some(root) = std::env::var_os(FIFO_CHILD_ROOT) else {
        return;
    };
    let cancel = std::env::var(FIFO_CHILD_CANCEL).unwrap() == "1";
    let action = std::env::var(FIFO_CHILD_ACTION).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    let ctx = context(Path::new(&root));
    let args = json!({"action":action});
    let out = runtime.block_on(async {
        if cancel {
            tools::execute(
                &ctx,
                "inventory",
                &args,
                tokio::time::sleep(Duration::from_millis(10)),
                None,
            )
            .await
            .unwrap()
        } else {
            inventory(&ctx, args).await
        }
    });
    if out.kind != ExecKind::Cancelled {
        assert_unsafe_ignore(&out);
        assert!(!out.text.contains("function guarded_private"));
        assert!(!out.text.contains("private.rs [rust]"));
    }
    let immediately_cancelled = runtime
        .block_on(tools::execute(
            &ctx,
            "inventory",
            &json!({"action":"symbols"}),
            std::future::ready(()),
            None,
        ))
        .unwrap();
    assert_eq!(immediately_cancelled.kind, ExecKind::Cancelled);
    // A Cancelled result alone was the old false positive: Runtime::drop still
    // waited forever for the blocked ignore reader. The parent bounds this drop.
    drop(runtime);
    println!("inventory worker drained");
}
