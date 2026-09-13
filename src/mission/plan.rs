use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashSet, VecDeque};
use std::path::Path;

/// Typed, validated task-plan artifact the orchestrator must produce.
/// The runtime owns everything downstream of this point.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct MissionPlan {
    pub objective: String,
    /// Commit the whole mission branches from; must resolve in the repo.
    pub base_commit: String,
    pub tasks: Vec<TaskContract>,
    /// Deterministic gates run on the integration candidate.
    #[serde(default)]
    pub integration_checks: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TaskContract {
    pub id: String,
    pub objective: String,
    /// Paths this task exclusively owns. A trailing "/**" or "/" means a
    /// directory prefix; anything else is an exact file.
    pub owned_paths: Vec<String>,
    #[serde(default)]
    pub read_paths: Vec<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    /// Deterministic acceptance commands run in the worker worktree.
    pub acceptance: Vec<String>,
    pub max_turns: Option<usize>,
}

const MAX_TASKS: usize = 8;

pub fn validate(plan: &MissionPlan, repo: &Path) -> Result<()> {
    if plan.objective.trim().is_empty() {
        bail!("plan: empty objective");
    }
    if plan.tasks.is_empty() {
        bail!("plan: no tasks");
    }
    if plan.tasks.len() > MAX_TASKS {
        bail!("plan: {} tasks exceeds cap {MAX_TASKS}", plan.tasks.len());
    }
    // base revision must resolve
    let out = std::process::Command::new("git")
        .args(["-C"])
        .arg(repo)
        .args([
            "rev-parse",
            "--verify",
            &format!("{}^{{commit}}", plan.base_commit),
        ])
        .output()
        .context("git rev-parse")?;
    if !out.status.success() {
        bail!("plan: base_commit '{}' does not resolve", plan.base_commit);
    }

    let mut ids = HashSet::new();
    for t in &plan.tasks {
        if t.id.trim().is_empty() || !ids.insert(t.id.clone()) {
            bail!("plan: duplicate or empty task id '{}'", t.id);
        }
        if t.owned_paths.is_empty() {
            bail!("plan: task {} has no owned_paths", t.id);
        }
        if t.acceptance.is_empty() {
            bail!("plan: task {} has no acceptance commands", t.id);
        }
        for p in &t.owned_paths {
            let p = p.trim_end_matches("/**").trim_end_matches('/');
            if p.is_empty() || p == "." || p == "**" {
                bail!("plan: task {} owns the whole tree — must be bounded", t.id);
            }
        }
    }
    for t in &plan.tasks {
        for d in &t.depends_on {
            if !ids.contains(d) {
                bail!("plan: task {} depends on unknown task {}", t.id, d);
            }
            if d == &t.id {
                bail!("plan: task {} depends on itself", t.id);
            }
        }
    }
    // acyclic (Kahn)
    {
        let mut indeg: std::collections::HashMap<&str, usize> = Default::default();
        for t in &plan.tasks {
            indeg.entry(&t.id).or_insert(0);
            for _ in &t.depends_on {
                *indeg.entry(&t.id).or_insert(0) += 1;
            }
        }
        let mut q: VecDeque<&str> = indeg
            .iter()
            .filter(|(_, &d)| d == 0)
            .map(|(&k, _)| k)
            .collect();
        let mut seen = 0;
        while let Some(k) = q.pop_front() {
            seen += 1;
            for t in &plan.tasks {
                if t.depends_on.iter().any(|d| d == k) {
                    let e = indeg.get_mut(t.id.as_str()).unwrap();
                    *e -= 1;
                    if *e == 0 {
                        q.push_back(&t.id);
                    }
                }
            }
        }
        if seen != plan.tasks.len() {
            bail!("plan: dependency cycle");
        }
    }
    // ownership must be pairwise disjoint — this is what prevents conflicts
    for (i, a) in plan.tasks.iter().enumerate() {
        for b in &plan.tasks[i + 1..] {
            for pa in &a.owned_paths {
                for pb in &b.owned_paths {
                    if patterns_overlap(pa, pb) {
                        bail!(
                            "plan: tasks {} and {} overlap on ownership ({} vs {})",
                            a.id,
                            b.id,
                            pa,
                            pb
                        );
                    }
                }
            }
        }
    }
    Ok(())
}

/// Does a changed file path fall inside an owned pattern?
/// "x/**" or "x/" → directory prefix; otherwise exact file.
pub fn path_owned(path: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|pat| {
        if let Some(dir) = pat.strip_suffix("/**") {
            path.starts_with(&format!("{dir}/"))
        } else if let Some(dir) = pat.strip_suffix('/') {
            path.starts_with(&format!("{dir}/"))
        } else {
            path == pat
        }
    })
}

/// Do two owned patterns possibly cover the same file?
fn patterns_overlap(a: &str, b: &str) -> bool {
    let pa = a.trim_end_matches("/**").trim_end_matches('/');
    let pb = b.trim_end_matches("/**").trim_end_matches('/');
    let a_dir = a.ends_with("/**") || a.ends_with('/');
    let b_dir = b.ends_with("/**") || b.ends_with('/');
    if a == b || pa == pb {
        return true;
    }
    // dir prefix vs anything under it
    if a_dir && (pb.starts_with(&format!("{pa}/")) || pb == pa) {
        return true;
    }
    if b_dir && (pa.starts_with(&format!("{pb}/")) || pa == pb) {
        return true;
    }
    // file pattern can't overlap a different file pattern
    // dir pattern vs file: dir owns files beneath it
    if a_dir && !b_dir && b.starts_with(&format!("{pa}/")) {
        return true;
    }
    if b_dir && !a_dir && a.starts_with(&format!("{pb}/")) {
        return true;
    }
    false
}

/// Execution waves: tasks grouped so every task's deps are in earlier waves.
pub fn waves(plan: &MissionPlan) -> Vec<Vec<usize>> {
    let mut done: HashSet<&str> = HashSet::new();
    let mut remaining: Vec<usize> = (0..plan.tasks.len()).collect();
    let mut out = Vec::new();
    while !remaining.is_empty() {
        let wave: Vec<usize> = remaining
            .iter()
            .copied()
            .filter(|&i| {
                plan.tasks[i]
                    .depends_on
                    .iter()
                    .all(|d| done.contains(d.as_str()))
            })
            .collect();
        if wave.is_empty() {
            break; // cycle — validate() already rejects
        }
        for &i in &wave {
            done.insert(&plan.tasks[i].id);
        }
        remaining.retain(|i| !wave.contains(i));
        out.push(wave);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overlap_rules() {
        assert!(patterns_overlap("src/a.rs", "src/a.rs"));
        assert!(patterns_overlap("src/**", "src/a.rs"));
        assert!(patterns_overlap("src/**", "src/sub/**"));
        assert!(patterns_overlap("src/", "src/a.rs"));
        assert!(!patterns_overlap("src/a.rs", "src/b.rs"));
        assert!(!patterns_overlap("src/**", "lib/x.rs"));
    }

    #[test]
    fn path_owned_rules() {
        let own = vec!["src/**".to_string(), "exact.txt".to_string()];
        assert!(path_owned("src/deep/x.rs", &own));
        assert!(path_owned("exact.txt", &own));
        assert!(!path_owned("other/x.rs", &own));
        assert!(!path_owned("exact2.txt", &own));
    }
}
