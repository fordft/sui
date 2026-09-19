use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Mutating ops serialize here — a 2-task wave issues `worktree add` /
/// `branch -D` / `merge` concurrently and they contend on .git/worktrees
/// metadata + ref locks. The ops are milliseconds; the parallel part is
/// the agent run, which stays unlocked. Read-only ops don't take it.
static WRITE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// All git ops are async — they run inside the mission runtime shared
/// with the TUI, so a blocking Command would stall unrelated tasks.
async fn git(repo: &Path, args: &[&str]) -> Result<String> {
    let out = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await
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

pub async fn head(repo: &Path) -> Result<String> {
    git(repo, &["rev-parse", "HEAD"]).await
}

/// Resolve any ref to a commit sha.
pub async fn git_rev(repo: &Path, r: &str) -> Result<String> {
    git(repo, &["rev-parse", &format!("{r}^{{commit}}")]).await
}

/// `git worktree add -b <branch> <path> <base>`; deletes a stale branch
/// of the same name first (re-dispatch after escalation reuses names).
pub async fn add(repo: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    let _g = WRITE_LOCK.lock().await;
    let _ = git(repo, &["branch", "-D", branch]).await;
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
    )
    .await?;
    Ok(())
}

/// Stage everything in the worktree and commit. Returns the commit sha.
/// A clean worktree returns the current HEAD (repair rounds may produce
/// no new diff).
pub async fn commit_all(wt: &Path, msg: &str) -> Result<String> {
    let _g = WRITE_LOCK.lock().await;
    git(wt, &["add", "-A"]).await?;
    let status = git(wt, &["status", "--porcelain"]).await?;
    if !status.is_empty() {
        git(wt, &["commit", "-m", msg]).await?;
    }
    git(wt, &["rev-parse", "HEAD"]).await
}

/// Files changed vs `base` in a worktree (staged or not).
pub async fn changed_files(wt: &Path, base: &str) -> Result<Vec<String>> {
    let out = git(wt, &["diff", "--name-only", base]).await?;
    let mut v: Vec<String> = out.lines().map(|s| s.to_string()).collect();
    // untracked files count too
    let untracked = git(wt, &["ls-files", "--others", "--exclude-standard"]).await?;
    v.extend(untracked.lines().map(|s| s.to_string()));
    v.sort();
    v.dedup();
    Ok(v)
}

/// Diff base..wt (bounded by caller).
pub async fn diff(wt: &Path, base: &str) -> Result<String> {
    git(wt, &["diff", base]).await
}

/// Merge `branch` into the integration worktree. On conflict, abort the
/// merge and report the conflicted files.
pub async fn merge(integration_wt: &Path, branch: &str) -> Result<()> {
    let _g = WRITE_LOCK.lock().await;
    match git(
        integration_wt,
        &["merge", "--no-ff", "-m", &format!("merge {branch}"), branch],
    )
    .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            let conflicts = git(integration_wt, &["diff", "--name-only", "--diff-filter=U"])
                .await
                .unwrap_or_default();
            let _ = git(integration_wt, &["merge", "--abort"]).await;
            bail!("merge conflict on {branch}: {conflicts} ({e:#})")
        }
    }
}

/// Reset a worktree hard to a ref (re-integration after repair).
pub async fn reset_hard(wt: &Path, to: &str) -> Result<()> {
    let _g = WRITE_LOCK.lock().await;
    git(wt, &["reset", "--hard", to]).await?;
    Ok(())
}

pub async fn remove(repo: &Path, path: &Path) {
    let _g = WRITE_LOCK.lock().await;
    let _ = git(
        repo,
        &["worktree", "remove", "--force", &path.to_string_lossy()],
    )
    .await;
}

/// Delete a scratch branch (control worktree) — unlike task/integration
/// branches it carries no deliverable, so it must not linger.
pub async fn branch_delete(repo: &Path, branch: &str) {
    let _g = WRITE_LOCK.lock().await;
    let _ = git(repo, &["branch", "-D", branch]).await;
}

pub fn worktrees_dir(run_dir: &Path) -> PathBuf {
    run_dir.join("worktrees")
}
