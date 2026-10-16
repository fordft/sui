//! Exercise file saves through tool dispatch, including hostile staging paths.
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use sui::tools::{self, ExecKind, ToolContext};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "sui-fs-atomic-{}-{:x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir_all(root.join("workspace")).unwrap();
        Self(root)
    }

    fn ctx(&self) -> ToolContext {
        ToolContext {
            workspace: self.0.join("workspace"),
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

fn save_args(tool: &str, path: &str) -> Value {
    match tool {
        "write_file" => json!({"path": path, "content": "new content\n"}),
        "edit_file" => json!({"path": path, "old_str": "old", "new_str": "new"}),
        _ => unreachable!(),
    }
}

async fn save(ctx: &ToolContext, tool: &str, path: &str) {
    let out = tools::execute(
        ctx,
        tool,
        &save_args(tool, path),
        std::future::pending(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert_eq!(
        std::fs::read_to_string(ctx.workspace.join(path)).unwrap(),
        "new content\n"
    );
}

#[tokio::test]
async fn saves_do_not_clobber_existing_staging_name() {
    for tool in ["write_file", "edit_file"] {
        let fixture = Fixture::new();
        let ctx = fixture.ctx();
        std::fs::write(ctx.workspace.join("file.rs"), "old content\n").unwrap();
        let unrelated = ctx.workspace.join("file.rs.sui-tmp");
        std::fs::write(&unrelated, "unrelated content").unwrap();
        save(&ctx, tool, "file.rs").await;
        assert_eq!(
            std::fs::read_to_string(&unrelated).unwrap(),
            "unrelated content"
        );
        assert_eq!(std::fs::read_dir(&ctx.workspace).unwrap().count(), 2);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn saves_do_not_follow_staging_symlinks_outside_workspace() {
    use std::os::unix::fs::symlink;

    for tool in ["write_file", "edit_file"] {
        let fixture = Fixture::new();
        let ctx = fixture.ctx();
        std::fs::write(ctx.workspace.join("file.rs"), "old content\n").unwrap();
        let outside = fixture.0.join("outside.txt");
        std::fs::write(&outside, "keep me").unwrap();
        let link = ctx.workspace.join("file.rs.sui-tmp");
        symlink(&outside, &link).unwrap();
        save(&ctx, tool, "file.rs").await;
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me");
        assert_eq!(std::fs::read_link(&link).unwrap(), outside);
        assert_eq!(std::fs::read_dir(&ctx.workspace).unwrap().count(), 2);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn saves_preserve_existing_permissions() {
    use std::os::unix::fs::PermissionsExt;

    for tool in ["write_file", "edit_file"] {
        for mode in [0o751, 0o600] {
            let fixture = Fixture::new();
            let ctx = fixture.ctx();
            let target = ctx.workspace.join("script.sh");
            std::fs::write(&target, "old content\n").unwrap();
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode)).unwrap();
            save(&ctx, tool, "script.sh").await;
            assert_eq!(
                std::fs::metadata(&target).unwrap().permissions().mode() & 0o777,
                mode
            );
            assert_eq!(std::fs::read_dir(&ctx.workspace).unwrap().count(), 1);
        }
    }
}

#[tokio::test]
async fn failed_save_cleans_staging_file_and_keeps_target() {
    let fixture = Fixture::new();
    let ctx = fixture.ctx();
    let target = ctx.workspace.join("directory");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(target.join("keep.txt"), "keep me").unwrap();
    let result = tools::execute(
        &ctx,
        "write_file",
        &save_args("write_file", "directory"),
        std::future::pending(),
        None,
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        std::fs::read_to_string(target.join("keep.txt")).unwrap(),
        "keep me"
    );
    assert_eq!(std::fs::read_dir(&ctx.workspace).unwrap().count(), 1);
}

#[tokio::test]
async fn new_nested_file_can_be_saved_and_read_back() {
    let fixture = Fixture::new();
    let ctx = fixture.ctx();
    save(&ctx, "write_file", "nested/file").await;
    let out = tools::execute(
        &ctx,
        "read_file",
        &json!({"path": "nested/file"}),
        std::future::pending(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(out.kind, ExecKind::Success, "{}", out.text);
    assert!(out.text.contains("    1  new content"), "{}", out.text);
    assert_eq!(
        std::fs::read_dir(ctx.workspace.join("nested"))
            .unwrap()
            .count(),
        1
    );
}
