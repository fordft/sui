use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .context("spawn git")?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(stdout.trim().to_string())
}

pub fn head(repo: &Path) -> Result<String> {
    git(repo, &["rev-parse", "HEAD"])
}

/// Resolve any ref to a commit sha.
pub fn git_rev(repo: &Path, r: &str) -> Result<String> {
    git(repo, &["rev-parse", &format!("{r}^{{commit}}")])
}

/// `git worktree add -b <branch> <path> <base>`; deletes a stale branch
/// of the same name first (re-dispatch after escalation reuses names).
pub fn add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    let _ = git(repo, &["branch", "-D", branch]);
    git(
        repo,
        &[
            "worktree",
            "add",
            "-b",
            branch,
            &path.to_string_lossy(),
            base,
        ],
    )?;
    Ok(())
}

/// Stage everything in the worktree and commit. Returns the commit sha.
/// A clean worktree returns the current HEAD (repair rounds may produce
/// no new diff).
pub fn commit_all(wt: &Path, msg: &str) -> Result<String> {
    git(wt, &["add", "-A"])?;
    let status = git(wt, &["status", "--porcelain"])?;
    if !status.is_empty() {
        git(wt, &["commit", "-m", msg])?;
    }
    git(wt, &["rev-parse", "HEAD"])
}

/// Files changed vs `base` in a worktree (staged or not).
pub fn changed_files(wt: &Path, base: &str) -> Result<Vec<String>> {
    let out = git(wt, &["diff", "--name-only", base])?;
    let mut v: Vec<String> = out.lines().map(|s| s.to_string()).collect();
    // untracked files count too
    let untracked = git(wt, &["ls-files", "--others", "--exclude-standard"])?;
    v.extend(untracked.lines().map(|s| s.to_string()));
    v.sort();
    v.dedup();
    Ok(v)
}

/// Diff base..wt (bounded by caller).
pub fn diff(wt: &Path, base: &str) -> Result<String> {
    git(wt, &["diff", base])
}

/// Merge `branch` into the integration worktree. On conflict, abort the
/// merge and report the conflicted files.
pub fn merge(integration_wt: &Path, branch: &str) -> Result<()> {
    match git(
        integration_wt,
        &["merge", "--no-ff", "-m", &format!("merge {branch}"), branch],
    ) {
        Ok(_) => Ok(()),
        Err(e) => {
            let conflicts = git(integration_wt, &["diff", "--name-only", "--diff-filter=U"])
                .unwrap_or_default();
            let _ = git(integration_wt, &["merge", "--abort"]);
            bail!("merge conflict on {branch}: {conflicts} ({e:#})")
        }
    }
}

/// Reset a worktree hard to a ref (re-integration after repair).
pub fn reset_hard(wt: &Path, to: &str) -> Result<()> {
    git(wt, &["reset", "--hard", to])?;
    Ok(())
}

pub fn remove(repo: &Path, path: &Path) {
    let _ = git(
        repo,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    );
}

pub fn worktrees_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("worktrees")
}
