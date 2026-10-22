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
