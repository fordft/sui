use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

use super::ToolContext;

const READ_DEFAULT: usize = 100;
const READ_MAX: usize = 400;

/// Resolve `rel` inside the workspace. Two checks: the lexically-normalized
/// path must stay under root (blocks `..` and absolute escapes), AND the
/// deepest existing ancestor — canonicalized, so symlinks are resolved —
/// must also stay under root (blocks symlink escapes).
///
/// NOTE: this is a validation guard, not a sandbox. There remains a
/// TOCTOU window between validation and open; true containment needs
/// openat2/RESOLVE_BENEATH or an OS sandbox. bash is NOT covered at all.
pub fn resolve(workspace: &Path, rel: &str) -> Result<PathBuf> {
    let root = workspace.canonicalize().unwrap_or_else(|_| workspace.to_path_buf());
    let joined = if Path::new(rel).is_absolute() {
        PathBuf::from(rel)
    } else {
        root.join(rel)
    };

    // lexical normalization: fold away `.` and `..`
    let mut norm = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                norm.pop();
            }
            other => norm.push(other.as_os_str()),
        }
    }
    if !norm.starts_with(&root) {
        bail!("path escapes workspace: {rel}");
    }

    // canonicalize the deepest existing ancestor and re-check containment
    let mut probe = norm.clone();
    loop {
        if probe.exists() {
            let canon = probe.canonicalize().unwrap_or_else(|_| probe.clone());
            if !canon.starts_with(&root) {
                bail!("path escapes workspace via symlink: {rel}");
            }
            break;
        }
        if !probe.pop() {
            break;
        }
    }
    Ok(norm)
}

pub fn read_file(ctx: &ToolContext, args: &Value) -> Result<String> {
    let path = args["path"].as_str().unwrap_or("");
    let offset = args["offset"].as_u64().unwrap_or(1).max(1) as usize;
    let limit = (args["limit"].as_u64().unwrap_or(READ_DEFAULT as u64) as usize).min(READ_MAX);
    let p = resolve(&ctx.workspace, path)?;

    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("cannot read {}", p.display()))?;
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let start = offset.saturating_sub(1);
    if start >= total {
        return Ok(format!(
            "status: success\npath: {path}\nlines: {total}\ncontent: <empty — offset past end>"
        ));
    }
    let end = (start + limit).min(total);
    let mut out = format!("status: success\npath: {path}\nlines: {total}\nshowing: {}-{end}\n", start + 1);
    for (i, l) in lines[start..end].iter().enumerate() {
        out.push_str(&format!("{:>5}  {}\n", start + i + 1, l));
    }
    if end < total {
        out.push_str(&format!("truncated: true ({} lines remain)\n", total - end));
    }
    Ok(out)
}

pub fn write_file(ctx: &ToolContext, args: &Value) -> Result<String> {
    let path = args["path"].as_str().unwrap_or("");
    let content = args["content"].as_str().unwrap_or("");
    let p = resolve(&ctx.workspace, path)?;
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    atomic_write(&p, content.as_bytes())?;
    Ok(format!(
        "status: success\npath: {path}\nbytes: {}",
        content.len()
    ))
}

pub fn edit_file(ctx: &ToolContext, args: &Value) -> Result<String> {
    let path = args["path"].as_str().unwrap_or("");
    let old = args["old_str"].as_str().unwrap_or("");
    let new = args["new_str"].as_str().unwrap_or("");
    if old.is_empty() {
        return Ok("status: error\nerror: old_str must not be empty".into());
    }
    if old == new {
        return Ok("status: error\nerror: old_str equals new_str".into());
    }
    let p = resolve(&ctx.workspace, path)?;
    let text = std::fs::read_to_string(&p)
        .with_context(|| format!("cannot read {}", p.display()))?;

    let count = text.matches(old).count();
    match count {
        0 => Ok(format!(
            "status: error\nerror: old_str not found in {path}\nhint: read_file the region and copy exact text including indentation"
        )),
        n if n > 1 => Ok(format!(
            "status: error\nerror: old_str matches {n} locations in {path}\nhint: include more surrounding context to make it unique"
        )),
        _ => {
            let updated = text.replacen(old, new, 1);
            atomic_write(&p, updated.as_bytes())?;
            Ok(format!(
                "status: success\npath: {path}\nbytes: {}",
                updated.len()
            ))
        }
    }
}

fn atomic_write(p: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = p.with_extension(format!(
        "{}.sui-tmp",
        p.extension().and_then(|e| e.to_str()).unwrap_or("")
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, p)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_allows_inside() {
        let ws = std::env::temp_dir().canonicalize().unwrap();
        assert!(resolve(&ws, "a/b/c.rs").is_ok());
        assert!(resolve(&ws, "./x.txt").is_ok());
        assert!(resolve(&ws, "sub/../ok.rs").is_ok());
    }

    #[test]
    fn resolve_rejects_escapes() {
        let ws = std::env::temp_dir().canonicalize().unwrap();
        assert!(resolve(&ws, "../outside").is_err());
        assert!(resolve(&ws, "a/../../etc/passwd").is_err());
        assert!(resolve(&ws, "/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;
        let ws = std::env::temp_dir()
            .join(format!("sui-test-{}", std::process::id()))
            .canonicalize()
            .unwrap_or_else(|_| {
                let p = std::env::temp_dir().join(format!("sui-test-{}", std::process::id()));
                std::fs::create_dir_all(&p).unwrap();
                p.canonicalize().unwrap()
            });
        let link = ws.join("escape");
        let _ = std::fs::remove_file(&link);
        symlink("/etc", &link).unwrap();
        assert!(resolve(&ws, "escape/passwd").is_err());
        assert!(resolve(&ws, "escape/nonexistent.txt").is_err());
        assert!(resolve(&ws, "legit.rs").is_ok());
        let _ = std::fs::remove_file(&link);
    }
}
