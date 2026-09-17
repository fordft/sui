//! Engineering lenses: small Agent Skills-style guides selected by
//! evidence, loaded on demand. They address the "the user didn't say
//! it, so I didn't think of it" gap — a compact index is always in the
//! system prompt, relevant bodies are injected at task start, and the
//! `skill` tool loads anything else mid-run.
//!
//! Boundaries:
//! - Guides inform judgment; they never add user requirements, grant
//!   permissions, or override project constraints.
//! - Selection is deterministic (cue + project-fact hits) — no extra
//!   model call, no keyword dump of every language on earth.
//! - Skills are embedded at build time; there is no runtime file
//!   discovery, so a repo cannot inject instructions through this path.

use std::path::Path;

/// One lens: parsed frontmatter + body + inlined reference docs.
pub struct Skill {
    pub name: &'static str,
    pub description: String,
    pub cues: Vec<String>,
    /// Role keys this lens applies to; empty = all roles.
    pub roles: Vec<String>,
    body: &'static str,
    references: &'static [(&'static str, &'static str)],
}

macro_rules! skill {
    ($name:literal, $body:literal, [$($ref:literal),* $(,)?]) => {{
        let raw = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $body));
        let (desc, cues, roles, _body) = parse_frontmatter(raw);
        Skill {
            name: $name,
            description: desc,
            cues,
            roles,
            body: _body,
            references: &[$(
                ($ref, include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", $ref))),
            )*],
        }
    }};
}

/// Embedded library — one entry per skills/<name>/SKILL.md.
pub fn library() -> &'static [Skill] {
    use std::sync::OnceLock;
    static LIB: OnceLock<Vec<Skill>> = OnceLock::new();
    LIB.get_or_init(|| {
        vec![
            skill!(
                "product-exploration",
                "skills/product-exploration/SKILL.md",
                []
            ),
            skill!(
                "debugging",
                "skills/debugging/SKILL.md",
                [
                    "skills/debugging/references/async-responsiveness.md",
                    "skills/debugging/references/subprocess-lifecycle.md",
                ]
            ),
            skill!(
                "code-review",
                "skills/code-review/SKILL.md",
                [
                    "skills/code-review/references/rust.md",
                    "skills/code-review/references/python.md",
                    "skills/code-review/references/typescript.md",
                ]
            ),
            skill!("tui-quality", "skills/tui-quality/SKILL.md", []),
            skill!(
                "first-run-experience",
                "skills/first-run-experience/SKILL.md",
                []
            ),
            skill!(
                "release-verification",
                "skills/release-verification/SKILL.md",
                []
            ),
        ]
    })
}

/// `key: value` and `key: [a, b, c]` frontmatter → (desc, cues, roles, body).
fn parse_frontmatter(raw: &'static str) -> (String, Vec<String>, Vec<String>, &'static str) {
    let mut desc = String::new();
    let mut cues = Vec::new();
    let mut roles = Vec::new();
    let rest = raw.strip_prefix("---\n").unwrap_or(raw);
    let end = rest.find("\n---").map(|i| i + 1).unwrap_or(0);
    for line in rest[..end].lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim();
        let list = |v: &str| -> Vec<String> {
            v.trim_start_matches('[')
                .trim_end_matches(']')
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        };
        match k.trim() {
            "description" => desc = v.to_string(),
            "cues" => cues = list(v),
            "roles" => roles = list(v),
            _ => {}
        }
    }
    let body = rest[end..].trim_start_matches('-').trim_start_matches('\n');
    (desc, cues, roles, body)
}

/// Compact index for the stable system prompt — name + one line each.
pub fn index() -> String {
    library()
        .iter()
        .map(|s| format!("{}: {}", s.name, s.description))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Skill names for error messages.
pub fn names() -> Vec<&'static str> {
    library().iter().map(|s| s.name).collect()
}

/// Look up one skill by name — backs the `skill` tool.
pub fn get(name: &str) -> Option<&'static Skill> {
    library().iter().find(|s| s.name == name)
}

/// Lightweight project facts used for selection: language/runtime plus
/// notable dependencies discovered from top-level manifests. Bounded —
/// a small static dep list, not a full manifest parse.
pub fn facts(workspace: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut scan = |file: &str, tag: &str, deps: &[&str]| {
        let p = workspace.join(file);
        if let Ok(text) = std::fs::read_to_string(p) {
            out.push(tag.to_string());
            for d in deps {
                if text.contains(d) {
                    out.push(d.to_string());
                }
            }
        }
    };
    scan(
        "Cargo.toml",
        "rust",
        &[
            "tokio",
            "ratatui",
            "crossterm",
            "serde",
            "reqwest",
            "axum",
            "clap",
            "rmcp",
        ],
    );
    scan(
        "package.json",
        "typescript",
        &[
            "react",
            "next",
            "vue",
            "express",
            "jest",
            "vitest",
            "playwright",
        ],
    );
    scan(
        "pyproject.toml",
        "python",
        &[
            "django",
            "flask",
            "fastapi",
            "pytest",
            "pydantic",
            "sqlalchemy",
        ],
    );
    scan("requirements.txt", "python", &[]);
    scan("go.mod", "go", &[]);
    out
}

/// Deterministic selection: score = cue hits across task text + project
/// facts. Role filter applies first. Cap 3 — focused guides beat a
/// library dump; a skill with zero evidence is not loaded.
pub fn select<'a>(task: &str, facts: &[String], role: &str) -> Vec<&'a Skill>
where
    'a: 'static,
{
    let hay = format!("{} {}", task.to_lowercase(), facts.join(" ").to_lowercase());
    let mut scored: Vec<(usize, &Skill)> = library()
        .iter()
        .filter(|s| s.roles.is_empty() || s.roles.iter().any(|r| r == role))
        .map(|s| {
            let score = s.cues.iter().filter(|c| hay.contains(c.as_str())).count();
            (score, s)
        })
        .filter(|(n, _)| *n > 0)
        .collect();
    scored.sort_by_key(|(n, _)| std::cmp::Reverse(*n));
    scored.into_iter().take(3).map(|(_, s)| s).collect()
}

/// Full body + references — the injectable guide text.
pub fn render(s: &Skill) -> String {
    let mut out = format!("── guide: {} ──\n{}", s.name, s.body);
    for (path, content) in s.references {
        out.push_str(&format!("\n── reference: {path} ──\n{content}"));
    }
    out
}

/// The labeled block appended to the first user message. The original
/// task text stays verbatim above it; the block declares its own
/// authority so a guide can't launder requirements into the request.
pub fn guidance_block(_task: &str, facts: &[String], selected: &[&Skill]) -> String {
    let mut out = String::from(
        "\n── sui task guidance (runtime-injected — considerations and \
         procedures, not user requirements or permissions) ──\n",
    );
    let why: Vec<String> = facts.iter().take(6).cloned().collect();
    if !why.is_empty() {
        out.push_str(&format!("project facts: {}\n", why.join(", ")));
    }
    if selected.is_empty() {
        out.push_str("no lens matched this task — apply the general charter.\n");
        return out;
    }
    for s in selected {
        out.push_str(&render(s));
        out.push('\n');
    }
    out
}

/// Map an agent role label to a selection role key.
pub fn role_key(role: &str) -> &'static str {
    if role.contains("audit") {
        "auditor"
    } else if role.contains("worker") || role.contains("W") {
        "worker"
    } else {
        "lead"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_parses_all_skills() {
        for s in library() {
            assert!(!s.description.is_empty(), "{} missing description", s.name);
            assert!(!s.body.trim().is_empty(), "{} missing body", s.name);
        }
        assert_eq!(library().len(), 6);
    }

    #[test]
    fn index_is_compact() {
        let i = index();
        assert!(i.contains("debugging:"));
        assert!(i.contains("code-review:"));
        assert!(i.lines().count() == 6);
    }

    #[test]
    fn select_by_task_cues() {
        let sel = select("fix the keypress bug in the permission modal", &[], "lead");
        assert!(sel.iter().any(|s| s.name == "debugging"));
        // tui cues (keypress, permission modal) should also hit
        assert!(sel.iter().any(|s| s.name == "tui-quality"));
    }

    #[test]
    fn select_uses_facts_not_just_task() {
        let facts = vec!["rust".into(), "ratatui".into(), "crossterm".into()];
        let sel = select("repaint is late", &facts, "lead");
        assert!(sel.iter().any(|s| s.name == "tui-quality"));
    }

    #[test]
    fn select_respects_role_filter() {
        // product-exploration is lead-only
        let sel = select("add a new settings form", &[], "worker");
        assert!(!sel.iter().any(|s| s.name == "product-exploration"));
        let sel = select("add a new settings form", &[], "lead");
        assert!(sel.iter().any(|s| s.name == "product-exploration"));
    }

    #[test]
    fn select_caps_at_three() {
        let facts = vec!["rust".into(), "tokio".into()];
        let sel = select(
            "fix the async keypress freeze, audit the setup flow, ship the release",
            &facts,
            "lead",
        );
        assert!(sel.len() <= 3);
    }

    #[test]
    fn guidance_block_labels_authority() {
        let sel = select("fix the bug", &[], "lead");
        let block = guidance_block("fix the bug", &["rust".into()], &sel);
        assert!(block.contains("runtime-injected"));
        assert!(block.contains("not user requirements"));
        assert!(block.contains("project facts: rust"));
        assert!(block.contains("guide: debugging"));
    }

    #[test]
    fn render_inlines_references() {
        let s = get("debugging").unwrap();
        let r = render(s);
        assert!(r.contains("async-responsiveness.md"));
        assert!(r.contains("subprocess-lifecycle.md"));
    }

    #[test]
    fn facts_detect_rust_workspace() {
        let d = std::env::temp_dir().join(format!("sui-facts-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(
            d.join("Cargo.toml"),
            "[dependencies]\ntokio = \"1\"\nratatui = \"0.29\"",
        )
        .unwrap();
        let f = facts(&d);
        assert!(f.contains(&"rust".to_string()));
        assert!(f.contains(&"tokio".to_string()));
        assert!(f.contains(&"ratatui".to_string()));
    }
}
