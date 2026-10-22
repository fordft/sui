//! Read-only review of runtime-owned Mission records and immutable Git commits.
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;
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
    let mut child = tokio::process::Command::new("git")
        .arg("--no-pager")
        .arg("--no-optional-locks")
        .args(["-c", "core.fsmonitor=false", "-C"])
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
    let mut files = Vec::new();
    let mut files_truncated = false;
    if let (Some(base), Some(candidate)) = (base, candidate) {
        if ![base, candidate]
            .iter()
            .all(|sha| (40..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            bail!("Invalid recorded commit IDs");
        }
        let (names, truncated) = git(
            workspace,
            &[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-only",
                "-z",
                base,
                candidate,
                "--",
            ],
        )
        .await?;
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
        if let Some(file) = file {
            if !files.iter().any(|name| name == file) {
                bail!("Choose a file in this candidate diff");
            }
            let (diff, truncated) = git(
                workspace,
                &[
                    "--literal-pathspecs",
                    "diff",
                    "--no-ext-diff",
                    "--no-textconv",
                    "--no-color",
                    "--unified=3",
                    base,
                    candidate,
                    "--",
                    file,
                ],
            )
            .await?;
            return Ok(
                json!({"file":file,"base":base,"candidate":candidate,"diff":String::from_utf8_lossy(&diff),"truncated":truncated}),
            );
        }
        files_truncated = files.len() > 200;
        files.truncate(200);
    } else if file.is_some() {
        bail!("No accepted candidate is recorded for this mission");
    }
    let gates: Vec<_> = events
        .iter()
        .filter(|e| matches!(e["type"].as_str(), Some("gate" | "task_result" | "audit")))
        .collect();
    let omitted = gates.len().saturating_sub(40);
    Ok(
        json!({"result":result,"base":base,"candidate":candidate,"files":files,"files_truncated":files_truncated,"agents":agents,
        "evidence":gates.into_iter().rev().take(40).map(|e| json!({"kind":e["type"],"text":preview(&e["data"],2000)})).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>(),"omitted":omitted}),
    )
}
