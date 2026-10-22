use serde_json::json;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[tokio::test]
async fn failed_mission_review_reads_retained_changes_without_accepting_or_applying_them() {
    let root =
        std::env::temp_dir().join(format!("sui-review-failed-{:016x}", rand::random::<u64>()));
    let workspace = root.join("workspace");
    let run = root.join("run");
    let worker = run.join("worktrees/worker-a");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(worker.parent().unwrap()).unwrap();
    git(&workspace, &["init", "-b", "main"]);
    git(&workspace, &["config", "user.email", "test@example.com"]);
    git(&workspace, &["config", "user.name", "Test"]);
    std::fs::write(workspace.join("page.txt"), "before\n").unwrap();
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-m", "base"]);
    let base = git(&workspace, &["rev-parse", "HEAD"]);
    git(
        &workspace,
        &[
            "worktree",
            "add",
            "-b",
            "worker-a",
            worker.to_str().unwrap(),
            &base,
        ],
    );
    std::fs::write(worker.join("page.txt"), "unfinished\n").unwrap();
    std::fs::write(worker.join("new.txt"), "new untracked work\n").unwrap();
    std::fs::write(
        workspace.join(".git/info/attributes"),
        "*.txt filter=proof\n",
    )
    .unwrap();
    git(
        &workspace,
        &[
            "config",
            "filter.proof.clean",
            "touch clean-filter-ran; cat",
        ],
    );
    git(
        &workspace,
        &[
            "config",
            "filter.proof.process",
            "touch process-filter-ran; exit 1",
        ],
    );
    git(&workspace, &["config", "filter.proof.required", "true"]);
    let events = [
        json!({"type":"session","data":{"workspace":workspace}}),
        json!({"type":"mission_base","data":{"sha":base}}),
        json!({"type":"gate","data":{"cmd":"test","exit_code":1}}),
        json!({"type":"result","data":{"outcome":"failed: tests failed","accepted_sha":null}}),
    ];
    std::fs::write(
        run.join("mission.jsonl"),
        events.iter().map(|e| format!("{e}\n")).collect::<String>(),
    )
    .unwrap();
    let summary = sui::mission::review::load(&workspace, &run, None, None)
        .await
        .unwrap();
    assert!(summary["candidate"].is_null());
    assert_eq!(summary["worktree"], "worker-a");
    assert_eq!(summary["files"], json!(["new.txt", "page.txt"]));
    let diff = sui::mission::review::load_selection(
        &workspace,
        &run,
        Some("page.txt"),
        None,
        Some("worker-a"),
    )
    .await
    .unwrap();
    assert!(diff["diff"].as_str().unwrap().contains("+unfinished"));
    let new = sui::mission::review::load_selection(
        &workspace,
        &run,
        Some("new.txt"),
        None,
        Some("worker-a"),
    )
    .await
    .unwrap();
    assert!(new["diff"].as_str().unwrap().contains("new untracked work"));
    for directory in [&workspace, &worker] {
        assert!(
            !directory.join("clean-filter-ran").exists(),
            "review must never execute clean filters"
        );
        assert!(
            !directory.join("process-filter-ran").exists(),
            "review must never execute process filters"
        );
    }
    git(&workspace, &["config", "--remove-section", "filter.proof"]);
    std::fs::write(worker.join("large.txt"), "a".repeat(130 * 1024)).unwrap();
    let large = sui::mission::review::load_selection(
        &workspace,
        &run,
        Some("large.txt"),
        None,
        Some("worker-a"),
    )
    .await
    .unwrap();
    assert_eq!(large["truncated"], true);
    assert!(large["diff"].as_str().unwrap().len() < 129 * 1024);
    assert!(sui::mission::review::load_selection(
        &workspace,
        &run,
        None,
        None,
        Some("../../workspace")
    )
    .await
    .is_err());
    let foreign = run.join("worktrees/foreign");
    std::fs::create_dir(&foreign).unwrap();
    git(&foreign, &["init"]);
    assert!(
        sui::mission::review::load_selection(&workspace, &run, None, None, Some("foreign"))
            .await
            .unwrap_err()
            .to_string()
            .contains("another repository")
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(workspace.join("page.txt"), worker.join("outside.txt")).unwrap();
        assert!(sui::mission::review::load_selection(
            &workspace,
            &run,
            Some("outside.txt"),
            None,
            Some("worker-a")
        )
        .await
        .is_err());
        std::os::unix::fs::symlink(&workspace, run.join("worktrees/escape")).unwrap();
        assert!(
            sui::mission::review::load_selection(&workspace, &run, None, None, Some("escape"))
                .await
                .is_err()
        );
    }
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), base);
    assert!(git(&workspace, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(workspace.join("page.txt")).unwrap(),
        "before\n"
    );
    assert!(git(&worker, &["status", "--porcelain"]).contains("?? new.txt"));
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn recorded_mission_review_reads_pinned_diff_and_evidence_without_changing_checkout() {
    let root = std::env::temp_dir().join(format!("sui-review-{:016x}", rand::random::<u64>()));
    let workspace = root.join("workspace");
    let run = root.join("run");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::create_dir_all(&run).unwrap();
    git(&workspace, &["init", "-b", "main"]);
    git(&workspace, &["config", "user.email", "test@example.com"]);
    git(&workspace, &["config", "user.name", "Test"]);
    std::fs::write(workspace.join("page.txt"), "before\n").unwrap();
    git(&workspace, &["add", "."]);
    git(&workspace, &["commit", "-m", "base"]);
    let base = git(&workspace, &["rev-parse", "HEAD"]);
    git(&workspace, &["checkout", "-b", "candidate"]);
    std::fs::write(
        workspace.join("page.txt"),
        "after <script>not markup</script>\n",
    )
    .unwrap();
    git(&workspace, &["commit", "-am", "candidate"]);
    let candidate = git(&workspace, &["rev-parse", "HEAD"]);
    git(&workspace, &["checkout", "main"]);
    let events = [
        json!({"type":"session","data":{"workspace":workspace}}),
        json!({"type":"mission_base","data":{"sha":base}}),
        json!({"type":"plan","data":{"base_commit":"HEAD"}}),
        json!({"type":"gate","data":{"cmd":"test -f page.txt","exit_code":0}}),
        json!({"type":"audit","data":{"verdict":"accept"}}),
        json!({"type":"result","data":{"outcome":"accepted","accepted_sha":candidate,"branch":"candidate"}}),
    ];
    std::fs::write(
        run.join("mission.jsonl"),
        events.iter().map(|e| format!("{e}\n")).collect::<String>(),
    )
    .unwrap();
    std::fs::write(run.join("w-one.jsonl"), format!("{}\n{}\n", json!({"type":"assistant","data":{"content":"Updated page"}}), json!({"type":"tool","data":{"name":"bash","status":"ok","exit_code":0,"result":"test passed"}}))).unwrap();
    let summary = sui::mission::review::load(&workspace, &run, None, None)
        .await
        .unwrap();
    assert_eq!(summary["files"], json!(["page.txt"]));
    assert_eq!(summary["candidate"], candidate);
    assert!(summary["evidence"].to_string().contains("exit_code"));
    let diff = sui::mission::review::load(&workspace, &run, Some("page.txt"), None)
        .await
        .unwrap();
    assert!(diff["diff"].as_str().unwrap().contains("-before"));
    assert!(diff["diff"].as_str().unwrap().contains("+after"));
    let worker = sui::mission::review::load(&workspace, &run, None, Some("w-one.jsonl"))
        .await
        .unwrap();
    assert!(worker["entries"].to_string().contains("test passed"));
    assert!(
        sui::mission::review::load(&workspace, &run, Some("../secret"), None)
            .await
            .is_err()
    );
    assert!(
        sui::mission::review::load(&workspace, &run, None, Some("../secret.jsonl"))
            .await
            .is_err()
    );
    assert!(sui::mission::review::load(&root, &run, None, None)
        .await
        .is_err());
    assert_eq!(git(&workspace, &["rev-parse", "HEAD"]), base);
    assert!(git(&workspace, &["status", "--porcelain"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(workspace.join("page.txt")).unwrap(),
        "before\n"
    );
    std::fs::remove_dir_all(root).unwrap();
}
