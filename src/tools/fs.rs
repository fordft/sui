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
/// resolve() against a ToolContext — canonicalizes the workspace root
/// once per context, not once per call.
pub fn resolve_ctx(ctx: &ToolContext, rel: &str) -> Result<PathBuf> {
    // Only the canonicalized result is cached — a transient canonicalize
    // failure (fd exhaustion) must not latch the raw path forever.
    let root = match ctx.canon_root.get() {
        Some(r) => r.clone(),
        None => match ctx.workspace.canonicalize() {
            Ok(r) => {
                let _ = ctx.canon_root.set(r.clone());
                r
            }
            Err(_) => ctx.workspace.clone(),
        },
    };
    resolve_in(&root, rel)
}

pub fn resolve(workspace: &Path, rel: &str) -> Result<PathBuf> {
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    resolve_in(&root, rel)
}

fn resolve_in(root: &Path, rel: &str) -> Result<PathBuf> {
    let root = root.to_path_buf();
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
    // try_from: on 32-bit a u64 offset/limit above usize::MAX would wrap
    // to a small number and silently read the wrong window
    let offset = usize::try_from(args["offset"].as_u64().unwrap_or(1).max(1)).unwrap_or(usize::MAX);
    let limit = usize::try_from(args["limit"].as_u64().unwrap_or(READ_DEFAULT as u64))
        .unwrap_or(usize::MAX)
        .clamp(1, READ_MAX); // limit=0 produced "showing: 1-0" nonsense
    let p = resolve_ctx(ctx, path)?;

    use std::fmt::Write;
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(&p).with_context(|| format!("cannot read {}", p.display()))?;
    let mut r = BufReader::new(f);
    let start = offset.saturating_sub(1);
    let mut total = 0usize;
    let mut window = String::new();
    let mut buf = String::new();
    loop {
        buf.clear();
        if r.read_line(&mut buf)
            .with_context(|| format!("cannot read {}", p.display()))?
            == 0
        {
            break;
        }
        total += 1;
        if total > start && total <= start + limit {
            let _ = writeln!(
                window,
                "{:>5}  {}",
                total,
                buf.trim_end_matches(['\n', '\r'])
            );
        }
    }
    if start >= total {
        return Ok(format!(
            "status: success\npath: {path}\nlines: {total}\ncontent: <empty — offset past end>"
        ));
    }
    let end = (start + limit).min(total);
    let mut out = format!(
        "status: success\npath: {path}\nlines: {total}\nshowing: {}-{end}\n",
        start + 1
    );
    out.push_str(&window);
    if end < total {
        let _ = writeln!(out, "truncated: true ({} lines remain)", total - end);
    }
    Ok(out)
}

pub fn write_file(ctx: &ToolContext, args: &Value) -> Result<String> {
    let path = args["path"].as_str().unwrap_or("");
    if path.is_empty() {
        // "" resolves to the workspace root itself — refuse with a clear
        // error instead of letting atomic_write hit EISDIR on a directory
        return Ok("status: error\nerror: path must not be empty".into());
    }
    let content = args["content"].as_str().unwrap_or("");
    let p = resolve_ctx(ctx, path)?;
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
    let p = resolve_ctx(ctx, path)?;
    let text =
        std::fs::read_to_string(&p).with_context(|| format!("cannot read {}", p.display()))?;

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
