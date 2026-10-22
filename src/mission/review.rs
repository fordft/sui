//! Read-only review of runtime-owned records, accepted commits and retained work.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tokio::io::{AsyncRead, AsyncReadExt};

fn records(path: &Path) -> Result<Vec<Value>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = options.open(path)?;
    if file.metadata()?.len() > 32 * 1024 * 1024 {
        bail!("Review journal exceeds 32 MiB; use the run export.");
    }
    let mut events = Vec::new();
    for line in BufReader::new(file.take(32 * 1024 * 1024 + 1)).lines() {
        if events.len() == 50_000 {
            bail!("Review journal exceeds 50000 events; use the run export.");
        }
        events.push(serde_json::from_str(&line?)?);
    }
    Ok(events)
}

fn preview(value: &Value, limit: usize) -> String {
    let text = value
        .as_str()
        .map(String::from)
        .unwrap_or_else(|| value.to_string());
    let short: String = text.chars().take(limit).collect();
    if short.len() < text.len() {
        format!("{short}\n… [truncated; full record in run journal]")
    } else {
        short
    }
}

async fn capture(mut pipe: impl AsyncRead + Unpin) -> std::io::Result<(Vec<u8>, bool)> {
    let mut result = Vec::new();
    let mut truncated = false;
    let mut buffer = [0; 8192];
    loop {
        let count = pipe.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min((128 * 1024usize).saturating_sub(result.len()));
        result.extend_from_slice(&buffer[..keep]);
        truncated |= keep != count;
    }
    Ok((result, truncated))
}

async fn git(workspace: &Path, args: &[&str]) -> Result<(Vec<u8>, bool)> {
    // Worktree diffs otherwise run repository-defined clean/process filters,
    // even with --no-ext-diff and --no-textconv. Inspect names only: never read
    // configuration values (which can contain credentials) into review output.
    let mut options = Vec::new();
    if args.contains(&"diff") {
        let (keys, truncated) =
            capture_git(workspace, &["config", "--null", "--list", "--name-only"]).await?;
        if truncated {
            bail!("Git configuration exceeds review limit");
        }
        let mut filters = std::collections::BTreeSet::new();
        for key in std::str::from_utf8(&keys)?.split('\0') {
            if let Some((name, _)) = key.strip_prefix("filter.").and_then(|s| s.rsplit_once('.')) {
                filters.insert(name);
            }
        }
        if filters.len() > 100 {
            bail!("Too many Git filters for read-only review");
        }
        for name in filters {
            for (field, value) in [("clean", ""), ("process", ""), ("required", "false")] {
                options.extend(["-c".to_string(), format!("filter.{name}.{field}={value}")]);
            }
        }
    }
    options.extend(args.iter().map(|s| s.to_string()));
    capture_git(
        workspace,
        &options.iter().map(String::as_str).collect::<Vec<_>>(),
    )
    .await
}

async fn capture_git(workspace: &Path, args: &[&str]) -> Result<(Vec<u8>, bool)> {
    let mut child = tokio::process::Command::new("git")
        .arg("--no-pager")
        .arg("--no-optional-locks")
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "diff.submodule=short",
            "-C",
        ])
        .arg(workspace)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take().context("Git output missing")?;
    let stderr = child.stderr.take().context("Git diagnostics missing")?;
    let operation = async {
        let (status, output, error) =
            tokio::try_join!(child.wait(), capture(stdout), capture(stderr))?;
        if !status.success() {
            bail!("Git review failed: {}", String::from_utf8_lossy(&error.0));
        }
        Ok(output)
    };
    tokio::time::timeout(std::time::Duration::from_secs(10), operation)
        .await
        .context("Git review timed out")?
}

/// `directory` must come from the bridge's runtime result allowlist, never a
/// user-selected path. Even then, bind its recorded workspace before reading.
pub async fn load(
    workspace: &Path,
    directory: &Path,
    file: Option<&str>,
    agent: Option<&str>,
) -> Result<Value> {
    load_selection(workspace, directory, file, agent, None).await
}

fn retained_worktrees(directory: &Path) -> Result<Vec<String>> {
    let root = directory.join("worktrees");
    if !root.exists() {
        return Ok(Vec::new());
    }
    if !std::fs::symlink_metadata(&root)?.is_dir() {
        bail!("Invalid retained worktree directory");
    }
    let mut names = Vec::new();
    for (index, entry) in std::fs::read_dir(root)?.enumerate() {
        if index >= 100 {
            bail!("Too many retained worktrees; inspect the run on its host");
        }
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            names.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    names.sort();
    Ok(names)
}

async fn retained_path(workspace: &Path, directory: &Path, name: &str) -> Result<PathBuf> {
    let root = directory.join("worktrees").canonicalize()?;
    let path = root.join(name).canonicalize()?;
    if path.parent() != Some(root.as_path()) {
        bail!("Worktree escapes this mission");
    }
    let common = |cwd: &Path, bytes: &[u8]| -> Result<PathBuf> {
        Ok(cwd
            .join(std::str::from_utf8(bytes)?.trim())
            .canonicalize()?)
    };
    let (original, _) = git(workspace, &["rev-parse", "--git-common-dir"]).await?;
    let (selected, _) = git(&path, &["rev-parse", "--git-common-dir"]).await?;
    let (top, _) = git(&path, &["rev-parse", "--show-toplevel"]).await?;
    if common(workspace, &original)? != common(&path, &selected)? || common(&path, &top)? != path {
        bail!("Retained worktree belongs to another repository");
    }
    Ok(path)
}

fn untracked_preview(root: &Path, name: &str) -> Result<(String, bool)> {
    let path = root.join(name);
    if !path.canonicalize()?.starts_with(root) {
        bail!("Untracked file escapes the worktree");
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("Choose a regular untracked file");
    }
    let mut bytes = Vec::new();
    file.take(128 * 1024 + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > 128 * 1024;
    bytes.truncate(128 * 1024);
    if bytes.contains(&0) {
        return Ok(("Untracked binary file; no text preview.".into(), truncated));
    }
    Ok((
        format!(
            "Untracked file (current contents): {name}\n\n{}",
            String::from_utf8_lossy(&bytes)
        ),
        truncated,
    ))
}

pub async fn load_selection(
    workspace: &Path,
    directory: &Path,
    file: Option<&str>,
    agent: Option<&str>,
    worktree: Option<&str>,
) -> Result<Value> {
    let events = records(&directory.join("mission.jsonl"))?;
    let recorded = events
        .iter()
        .find(|e| e["type"] == "session")
        .and_then(|e| e["data"]["workspace"].as_str())
        .context("Mission workspace missing")?;
    if Path::new(recorded).canonicalize()? != workspace.canonicalize()? {
        bail!("Mission belongs to another workspace");
    }
    let mut agents = Vec::new();
    for (index, entry) in std::fs::read_dir(directory)?.enumerate() {
        if index >= 200 {
            bail!("Mission directory exceeds 200 entries; use the run export");
        }
        let entry = entry?;
        if entry.file_type()?.is_file() && entry.path().extension().is_some_and(|e| e == "jsonl") {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "mission.jsonl" {
                agents.push(name);
            }
        }
    }
    agents.sort();
    if agents.len() > 100 {
        bail!("Too many agent journals; use the run export");
    }
    if let Some(agent) = agent {
        if file.is_some() || !agents.iter().any(|name| name == agent) {
            bail!("Choose a recorded agent");
        }
        let rows = records(&directory.join(agent))?.into_iter().filter_map(|e| {
            let kind = e["type"].as_str()?;
            let data = &e["data"];
            let text = match kind {
                "user" | "assistant" => data["content"].as_str().map(|s| preview(&json!(s), 4000)),
                "tool" => Some(preview(&json!({"name":data["name"],"args":data["args"],"status":data["status"],"exit":data["exit"],"result":data["result"]}), 4000)),
                "gate" | "task_result" => Some(preview(data, 4000)),
                _ => None,
            }?;
            Some(json!({"kind":kind,"text":text}))
        }).collect::<Vec<_>>();
        let omitted = rows.len().saturating_sub(60);
        return Ok(
            json!({"agent":agent,"entries":rows.into_iter().rev().take(60).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),"omitted":omitted}),
        );
    }
    let plan = events
        .iter()
        .find(|e| e["type"] == "plan")
        .map(|e| &e["data"]);
    let result = events
        .iter()
        .rev()
        .find(|e| e["type"] == "result")
        .map(|e| e["data"].clone())
        .unwrap_or_else(|| json!({"outcome":"No terminal result recorded"}));
    let base = events
        .iter()
        .find(|e| e["type"] == "mission_base")
        .and_then(|e| e["data"]["sha"].as_str())
        .or_else(|| plan.and_then(|p| p["base_commit"].as_str()));
    let candidate = result["accepted_sha"].as_str();
    let worktrees = if candidate.is_none() {
        retained_worktrees(directory)?
    } else {
        Vec::new()
    };
    let selected = worktree.or_else(|| {
        worktrees
            .iter()
            .find(|n| n.as_str() == "integration")
            .or_else(|| worktrees.first())
            .map(String::as_str)
    });
    let retained = if let Some(name) = selected {
        if !worktrees.iter().any(|n| n == name) {
            bail!("Choose a retained worktree from this mission");
        }
        Some(retained_path(workspace, directory, name).await?)
    } else {
        None
    };
    let review_path = retained.as_deref().unwrap_or(workspace);
    let mut files = Vec::new();
    let mut files_truncated = false;
    if let Some(base) = base.filter(|_| candidate.is_some() || retained.is_some()) {
        if !std::iter::once(base)
            .chain(candidate)
            .all(|sha| (40..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            bail!("Invalid recorded commit IDs");
        }
        let mut comparison = vec![base];
        if let Some(candidate) = candidate {
            comparison.push(candidate);
        }
        let mut args = vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--name-only",
            "-z",
        ];
        args.extend(&comparison);
        args.push("--");
        let (names, truncated) = git(review_path, &args).await?;
        if truncated {
            bail!("Changed file list exceeds review limit; inspect the branch with Git");
        }
        let names = String::from_utf8(names)
            .context("File names are not UTF-8; inspect the branch with Git")?;
        files = names
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(String::from)
            .collect();
        let mut untracked = Vec::new();
        if retained.is_some() {
            let (names, truncated) = git(
                review_path,
                &["ls-files", "--others", "--exclude-standard", "-z"],
            )
            .await?;
            if truncated {
                bail!("Untracked file list exceeds review limit");
            }
            untracked = String::from_utf8(names)?
                .split('\0')
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            files.extend(untracked.clone());
            files.sort();
            files.dedup();
        }
        if let Some(file) = file {
            if !files.iter().any(|name| name == file) {
                bail!("Choose a file in this candidate diff");
            }
            let (diff, truncated) = if untracked.iter().any(|n| n == file) {
                let (text, truncated) = untracked_preview(review_path, file)?;
                (text.into_bytes(), truncated)
            } else {
                let mut args = vec![
                    "--literal-pathspecs",
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--unified=3",
                ];
                args.extend(&comparison);
                args.extend(["--", file]);
                git(review_path, &args).await?
            };
            return Ok(
                json!({"file":file,"base":base,"candidate":candidate,"worktree":selected,"diff":String::from_utf8_lossy(&diff),"truncated":truncated}),
            );
        }
        files_truncated = files.len() > 200;
        files.truncate(200);
    } else if file.is_some() {
        bail!("No accepted candidate or retained worktree is available for this mission");
    }
    let gates: Vec<_> = events
        .iter()
        .filter(|e| matches!(e["type"].as_str(), Some("gate" | "task_result" | "audit")))
        .collect();
    let omitted = gates.len().saturating_sub(40);
    Ok(
        json!({"result":result,"base":base,"candidate":candidate,"worktree":selected,"worktrees":worktrees,"files":files,"files_truncated":files_truncated,"agents":agents,
        "evidence":gates.into_iter().rev().take(40).map(|e| json!({"kind":e["type"],"text":preview(&e["data"],2000)})).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),"omitted":omitted}),
    )
}
