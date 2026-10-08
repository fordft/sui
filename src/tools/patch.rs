//! Guarded multi-file exact replacements. Preview and apply use the same
//! preflight; no target changes if any replacement is ambiguous or stale.
//! Cross-file rename is not an OS transaction: rollback is best effort on
//! ordinary I/O errors, not guaranteed across crashes or concurrent writers.
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, OpenOptions, Permissions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{fs::resolve_ctx, ToolContext};

const MAX_EDITS: usize = 20;
const MAX_INPUT: usize = 128 * 1024;
const MAX_SOURCE: usize = 512 * 1024;
const MAX_TOTAL: usize = 2 * 1024 * 1024;
const MAX_PREVIEW: usize = 12_000;
static NEXT_TMP: AtomicU64 = AtomicU64::new(0);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    action: String,
    edits: Vec<Replacement>,
    preview_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Replacement {
    path: String,
    old_str: String,
    new_str: String,
}

struct FileChange {
    path: String,
    target: PathBuf,
    original: Vec<u8>,
    updated: Vec<u8>,
    permissions: Permissions,
}

struct Prepared {
    files: Vec<FileChange>,
    preview_id: String,
    preview: String,
    edits: usize,
}

pub fn schema() -> Value {
    json!({"type":"function","function":{
        "name":"patch_files",
        "description":"Preview or apply up to 20 exact replacements across workspace files. Preview returns a bounded replacement diff and preview_id; apply requires the same edits and preview_id and rejects changed files. All edits validate before writing; writes stage and roll back on ordinary errors (not crash-atomic). Apply needs local mutation approval.",
        "parameters":{"type":"object","properties":{
            "action":{"type":"string","enum":["preview","apply"],"description":"Preview reads only; apply requires a matching preview_id"},
            "edits":{"type":"array","minItems":1,"maxItems":20,"description":"Exact replacements, in order; multiple edits to one file are applied sequentially","items":{"type":"object","properties":{
                "path":{"type":"string","description":"Workspace-relative existing UTF-8 regular file (no symlinks)"},
                "old_str":{"type":"string","description":"Nonempty exact text matching once at this step; include context"},
                "new_str":{"type":"string","description":"Replacement text, different from old_str"}
            },"required":["path","old_str","new_str"],"additionalProperties":false}},
            "preview_id":{"type":"string","description":"Exact ID returned by preview; required for apply"}
        },"required":["action","edits"],"additionalProperties":false}
    }})
}

pub fn execute(ctx: &ToolContext, args: &Value) -> String {
    match run(ctx, args) {
        Ok(text) => text,
        Err(e) => format!("status: error\nerror: {e:#}"),
    }
}

fn run(ctx: &ToolContext, args: &Value) -> Result<String> {
    // Reject oversized text before allocating owned replacements. Deserializing
    // the borrowed value also avoids cloning unknown oversized fields.
    if let Some(edits) = args["edits"].as_array() {
        if edits.is_empty() || edits.len() > MAX_EDITS {
            bail!("edits must contain 1..={MAX_EDITS} replacements");
        }
        let bytes = edits
            .iter()
            .flat_map(|edit| {
                ["path", "old_str", "new_str"].map(|key| edit[key].as_str().map_or(0, str::len))
            })
            .fold(0usize, usize::saturating_add);
        if bytes > MAX_INPUT {
            bail!("patch input exceeds {MAX_INPUT} bytes");
        }
    }
    let req = Request::deserialize(args).context("invalid patch_files arguments")?;
    if req.edits.is_empty() || req.edits.len() > MAX_EDITS {
        bail!("edits must contain 1..={MAX_EDITS} replacements");
    }
    if !matches!(req.action.as_str(), "preview" | "apply") {
        bail!("action must be preview or apply");
    }
    if req.action == "preview" && req.preview_id.is_some() {
        bail!("preview_id is only for apply");
    }
    // Validate IDs before any file is opened, even on a malformed apply.
    if req.action == "apply" && req.preview_id.as_deref().unwrap_or("").len() != 64 {
        bail!("apply requires the 64-character preview_id returned by preview");
    }
    let prepared = prepare(ctx, &req.edits)?;
    if req.action == "preview" {
        return Ok(format!(
            "status: success\naction: preview\nfiles: {}\nedits: {}\npreview_id: {}\nreplacement_diff:\n{}",
            prepared.files.len(), prepared.edits, prepared.preview_id, prepared.preview
        ));
    }
    if req.preview_id.as_deref() != Some(prepared.preview_id.as_str()) {
        bail!("stale preview_id: files or edits changed; preview again before applying");
    }
    commit(&prepared.files, &ctx.workspace.canonicalize()?)?;
    Ok(format!(
        "status: success\naction: apply\nfiles: {}\nedits: {}\npreview_id: {}",
        prepared.files.len(),
        prepared.edits,
        prepared.preview_id
    ))
}

fn prepare(ctx: &ToolContext, edits: &[Replacement]) -> Result<Prepared> {
    let root = ctx
        .workspace
        .canonicalize()
        .context("cannot resolve workspace")?;
    let mut files: Vec<FileChange> = Vec::new();
    let mut positions: HashMap<PathBuf, usize> = HashMap::new();
    let mut input_bytes = 0usize;
    let mut source_bytes = 0usize;
    let mut preview = String::new();
    for (index, edit) in edits.iter().enumerate() {
        if edit.path.is_empty() || edit.old_str.is_empty() || edit.old_str == edit.new_str {
            bail!(
                "edit {}: path and old_str must be nonempty and old_str must differ from new_str",
                index + 1
            );
        }
        input_bytes =
            input_bytes.saturating_add(edit.path.len() + edit.old_str.len() + edit.new_str.len());
        if input_bytes > MAX_INPUT {
            bail!("patch input exceeds {MAX_INPUT} bytes");
        }
        let target = resolve_ctx(ctx, &edit.path)?;
        reject_symlinks(&root, &target)?;
        let pos = if let Some(&pos) = positions.get(&target) {
            pos
        } else {
            let meta = fs::symlink_metadata(&target)
                .with_context(|| format!("cannot stat {}", edit.path))?;
            if !meta.is_file() || meta.len() > MAX_SOURCE as u64 {
                bail!(
                    "{} must be a regular file of at most {MAX_SOURCE} bytes",
                    edit.path
                );
            }
            let original =
                fs::read(&target).with_context(|| format!("cannot read {}", edit.path))?;
            source_bytes = source_bytes.saturating_add(original.len());
            if source_bytes > MAX_TOTAL {
                bail!("total source exceeds {MAX_TOTAL} bytes");
            }
            std::str::from_utf8(&original)
                .with_context(|| format!("{} is not UTF-8", edit.path))?;
            let pos = files.len();
            files.push(FileChange {
                path: edit.path.clone(),
                target: target.clone(),
                updated: original.clone(),
                original,
                permissions: meta.permissions(),
            });
            positions.insert(target, pos);
            pos
        };
        let file = &mut files[pos];
        let text = std::str::from_utf8(&file.updated)?;
        let count = text.matches(&edit.old_str).count();
        if count != 1 {
            bail!("edit {} ({}): old_str matches {count} locations; include unique context or preview again", index + 1, edit.path);
        }
        file.updated = text.replacen(&edit.old_str, &edit.new_str, 1).into_bytes();
        if file.updated.len() > MAX_SOURCE {
            bail!("updated {} exceeds {MAX_SOURCE} bytes", edit.path);
        }
        use std::fmt::Write as _;
        let _ = writeln!(
            preview,
            "{}: {} (replacement {})",
            edit.path,
            index + 1,
            index + 1
        );
        for line in edit.old_str.split_inclusive('\n') {
            let _ = writeln!(preview, "-{}", line.trim_end_matches('\n'));
        }
        for line in edit.new_str.split_inclusive('\n') {
            let _ = writeln!(preview, "+{}", line.trim_end_matches('\n'));
        }
        if preview.len() > MAX_PREVIEW {
            bail!("replacement diff exceeds {MAX_PREVIEW} bytes; split into smaller patches");
        }
    }
    // Hash original bytes and the ordered edits with length delimiters. A
    // preview ID is an optimistic concurrency guard, not authorization.
    let mut hash = Sha256::new();
    hash.update(b"sui-patch-v1");
    let serialized = serde_json::to_vec(
        &edits
            .iter()
            .map(|e| json!({"path":e.path,"old_str":e.old_str,"new_str":e.new_str}))
            .collect::<Vec<_>>(),
    )?;
    digest_part(&mut hash, &serialized);
    for file in &files {
        digest_part(&mut hash, file.target.to_string_lossy().as_bytes());
        digest_part(&mut hash, &file.original);
    }
    Ok(Prepared {
        files,
        preview_id: format!("{:x}", hash.finalize()),
        preview,
        edits: edits.len(),
    })
}

fn digest_part(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_be_bytes());
    hash.update(bytes);
}

fn reject_symlinks(root: &Path, target: &Path) -> Result<()> {
    let mut part = root.to_path_buf();
    for component in target
        .strip_prefix(root)
        .context("path escapes workspace")?
        .components()
    {
        part.push(component);
        if fs::symlink_metadata(&part)
            .with_context(|| format!("cannot stat {}", part.display()))?
            .file_type()
            .is_symlink()
        {
            bail!(
                "symlinks are not supported by patch_files: {}",
                part.display()
            );
        }
    }
    Ok(())
}

struct Staged {
    target: PathBuf,
    next: PathBuf,
    backup: PathBuf,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.next);
        let _ = fs::remove_file(&self.backup);
    }
}

fn temp_file(target: &Path, bytes: &[u8], permissions: &Permissions) -> Result<PathBuf> {
    let parent = target.parent().context("target has no parent")?;
    for _ in 0..10 {
        let n = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
        let name = format!(".sui-patch-{}-{n}", std::process::id());
        let path = parent.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut f) => {
                let result = (|| {
                    f.write_all(bytes)?;
                    f.sync_all()?;
                    f.set_permissions(permissions.clone())?;
                    Ok::<_, std::io::Error>(())
                })();
                if let Err(e) = result {
                    let _ = fs::remove_file(&path);
                    return Err(e).with_context(|| format!("cannot stage {}", target.display()));
                }
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("cannot stage {}", target.display())),
        }
    }
    bail!("cannot allocate staging file for {}", target.display())
}

fn commit(files: &[FileChange], root: &Path) -> Result<()> {
    let mut staged = stage(files)?;
    commit_staged(files, root, &mut staged, |_, _| Ok(()))
}

fn stage(files: &[FileChange]) -> Result<Vec<Staged>> {
    let mut staged: Vec<Staged> = Vec::new();
    for file in files {
        let next = temp_file(&file.target, &file.updated, &file.permissions)?;
        // RAII cleanup even if the backup cannot be staged.
        let entry = Staged {
            target: file.target.clone(),
            next,
            backup: PathBuf::new(),
        };
        staged.push(entry);
        staged.last_mut().unwrap().backup =
            temp_file(&file.target, &file.original, &file.permissions)?;
    }
    Ok(staged)
}

// The hook lets tests force a failure after a successful first rename.
fn commit_staged(
    files: &[FileChange],
    root: &Path,
    staged: &mut [Staged],
    mut before_rename: impl FnMut(usize, &Path) -> Result<()>,
) -> Result<()> {
    // No target is changed unless every file still has its previewed bytes.
    for (index, entry) in staged.iter().enumerate() {
        check_original(root, &entry.target, &files[index].original).with_context(|| {
            format!(
                "{} changed during staging; preview again",
                files[index].path
            )
        })?;
    }
    for index in 0..staged.len() {
        // Keep the window small; a concurrent writer may still race a rename.
        let entry = &staged[index];
        let result = check_original(root, &entry.target, &files[index].original)
            .and_then(|()| before_rename(index, &entry.target))
            .and_then(|()| fs::rename(&entry.next, &entry.target).map_err(Into::into));
        if let Err(e) = result {
            let mut failures = Vec::new();
            for previous in staged[..index].iter_mut().rev() {
                if let Err(rollback) = fs::rename(&previous.backup, &previous.target) {
                    // Do not delete the backup on drop when rollback failed.
                    let backup = std::mem::take(&mut previous.backup);
                    failures.push(format!(
                        "{} (backup retained at {}): {rollback}",
                        previous.target.display(),
                        backup.display()
                    ));
                }
            }
            if failures.is_empty() {
                bail!(
                    "commit failed for {}: {e:#}; earlier files rolled back",
                    files[index].path
                );
            }
            bail!(
                "commit failed for {}: {e:#}; ROLLBACK FAILED for {}",
                files[index].path,
                failures.join(", ")
            );
        }
    }
    Ok(())
}

fn check_original(root: &Path, target: &Path, expected: &[u8]) -> Result<()> {
    reject_symlinks(root, target)?;
    let meta = fs::symlink_metadata(target)?;
    if !meta.is_file() || fs::read(target)? != expected {
        bail!("contents or file type changed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::OnceLock;

    fn fixture() -> (tempfile_like::Workspace, ToolContext) {
        let dir = tempfile_like::Workspace::new();
        let ctx = ToolContext {
            workspace: dir.path.clone(),
            bash_timeout: std::time::Duration::from_secs(1),
            bash_timeout_max: std::time::Duration::from_secs(2),
            web: None,
            canon_root: OnceLock::new(),
            ui: OnceLock::new(),
            code_intel: OnceLock::new(),
            code_context: OnceLock::new(),
            tool_outputs: OnceLock::new(),
        };
        (dir, ctx)
    }

    // No dependency or shared fixture state; remove the test workspace on drop.
    mod tempfile_like {
        use std::path::PathBuf;
        pub struct Workspace {
            pub path: PathBuf,
        }
        impl Workspace {
            pub fn new() -> Self {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                let path = std::env::temp_dir().join(format!(
                    "sui-patch-test-{}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                ));
                std::fs::create_dir(&path).unwrap();
                Self { path }
            }
        }
        impl Drop for Workspace {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.path).unwrap();
            }
        }
    }

    fn edits() -> Value {
        json!({"action":"preview","edits":[
            {"path":"a.txt","old_str":"first\n","new_str":"FIRST\n"},
            {"path":"b.txt","old_str":"second\n","new_str":"SECOND\n"},
            {"path":"a.txt","old_str":"FIRST\n","new_str":"final\n"}
        ]})
    }

    #[test]
    fn preview_apply_multiple_files_and_sequential_edits() {
        let (dir, ctx) = fixture();
        fs::write(dir.path.join("a.txt"), "first\n").unwrap();
        fs::write(dir.path.join("b.txt"), "second\n").unwrap();
        let mut req = edits();
        let preview = execute(&ctx, &req);
        assert!(
            preview.contains("status: success\naction: preview\nfiles: 2\nedits: 3"),
            "{preview}"
        );
        assert_eq!(
            fs::read_to_string(dir.path.join("a.txt")).unwrap(),
            "first\n"
        );
        let id = preview
            .lines()
            .find_map(|l| l.strip_prefix("preview_id: "))
            .unwrap();
        req["action"] = json!("apply");
        req["preview_id"] = json!(id);
        let applied = execute(&ctx, &req);
        assert!(
            applied.contains("status: success\naction: apply"),
            "{applied}"
        );
        assert_eq!(
            fs::read_to_string(dir.path.join("a.txt")).unwrap(),
            "final\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path.join("b.txt")).unwrap(),
            "SECOND\n"
        );
        assert_eq!(
            fs::read_dir(&dir.path).unwrap().count(),
            2,
            "staging files must be removed"
        );
    }

    #[test]
    fn stale_preview_or_invalid_edit_never_writes_any_file() {
        let (dir, ctx) = fixture();
        fs::write(dir.path.join("a.txt"), "first\n").unwrap();
        fs::write(dir.path.join("b.txt"), "second\n").unwrap();
        let mut req = edits();
        let preview = execute(&ctx, &req);
        let id = preview
            .lines()
            .find_map(|l| l.strip_prefix("preview_id: "))
            .unwrap();
        fs::write(dir.path.join("b.txt"), "someone else\n").unwrap();
        req["action"] = json!("apply");
        req["preview_id"] = json!(id);
        assert!(execute(&ctx, &req).starts_with("status: error"));
        assert_eq!(
            fs::read_to_string(dir.path.join("a.txt")).unwrap(),
            "first\n"
        );
        fs::write(dir.path.join("b.txt"), "second\nextra\n").unwrap();
        let result = execute(&ctx, &req);
        assert!(result.contains("stale preview_id"), "{result}");
        assert_eq!(
            fs::read_to_string(dir.path.join("a.txt")).unwrap(),
            "first\n"
        );
    }

    #[test]
    fn failure_after_first_rename_rolls_back_and_cleans_up() {
        let (dir, ctx) = fixture();
        fs::write(dir.path.join("a.txt"), "first\n").unwrap();
        fs::write(dir.path.join("b.txt"), "second\n").unwrap();
        let req = edits();
        let parsed: Request = serde_json::from_value(req).unwrap();
        let prepared = prepare(&ctx, &parsed.edits).unwrap();
        let mut staged = stage(&prepared.files).unwrap();
        let root = ctx.workspace.canonicalize().unwrap();
        let err = commit_staged(&prepared.files, &root, &mut staged, |index, _| {
            if index == 1 {
                bail!("simulated rename failure");
            }
            Ok(())
        })
        .unwrap_err();
        assert!(err.to_string().contains("earlier files rolled back"));
        drop(staged);
        assert_eq!(
            fs::read_to_string(dir.path.join("a.txt")).unwrap(),
            "first\n"
        );
        assert_eq!(
            fs::read_to_string(dir.path.join("b.txt")).unwrap(),
            "second\n"
        );
        assert_eq!(fs::read_dir(&dir.path).unwrap().count(), 2);
    }

    #[test]
    fn ambiguity_and_escape_refused() {
        let (dir, ctx) = fixture();
        fs::write(dir.path.join("a.txt"), "same same").unwrap();
        let mut req = json!({"action":"preview","edits":[{"path":"a.txt","old_str":"same","new_str":"diff"}]});
        assert!(execute(&ctx, &req).contains("matches 2 locations"));
        req["edits"][0]["path"] = json!("../elsewhere.txt");
        assert!(execute(&ctx, &req).contains("path escapes workspace"));
        assert_eq!(
            fs::read_to_string(dir.path.join("a.txt")).unwrap(),
            "same same"
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_refused_and_permissions_preserved() {
        use std::os::unix::fs::{symlink, PermissionsExt};
        let (dir, ctx) = fixture();
        let path = dir.path.join("a.txt");
        fs::write(&path, "first\n").unwrap();
        fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
        symlink("a.txt", dir.path.join("link.txt")).unwrap();
        let mut req = json!({"action":"preview","edits":[{"path":"link.txt","old_str":"first","new_str":"next"}]});
        assert!(execute(&ctx, &req).contains("symlinks are not supported"));
        req["edits"][0]["path"] = json!("a.txt");
        let preview = execute(&ctx, &req);
        let id = preview
            .lines()
            .find_map(|l| l.strip_prefix("preview_id: "))
            .unwrap();
        req["action"] = json!("apply");
        req["preview_id"] = json!(id);
        assert!(execute(&ctx, &req).contains("status: success\naction: apply"));
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
