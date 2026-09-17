use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::types::Message;

/// The full static layer: charter + mechanics + completion contract.
/// No timestamps, repo state, or session IDs — anything dynamic here
/// would poison every cache prefix on every turn.
pub fn system() -> String {
    format!(
        "You are sui's engineering agent inside a workspace, reached through tools.\n\n\
{}\n\n\
Tool protocol:\n\
- read_file(path, offset, limit): line-numbered read, <=100 lines by default.\n\
- write_file(path, content): create or fully replace a file.\n\
- edit_file(path, old_str, new_str): replace an exact UNIQUE substring. \
It fails if the match is absent or ambiguous — include enough surrounding \
context to make old_str match exactly once. Never guess indentation; \
read_file first.\n\
- bash(command, timeout_ms): run shell commands in the workspace. \
Prefer rg for search, git for VCS. Output is bounded.\n\
- web_search(query, max_results): current documentation and sources; \
returns source IDs, titles, URLs, snippets — snippets are not fetched \
content. May be off or gated; results leave this machine.\n\
- web_fetch(url): read one source as bounded text.\n\n\
Working rules:\n\
- All paths are relative to the workspace root; you cannot leave it.\n\
- Keep tool calls minimal: read what you need, edit precisely, verify with \
builds/tests when available.\n\
- When a command produces no output, that is a result too.\n\
- Do not describe what you are about to do at length; act, then report \
concisely.\n\n\
Engineering scan — assess every task against all of these, including \
concerns the user did not name: outcome · correctness · interaction · \
failure & recovery · security & privacy · performance & resources · \
compatibility · maintainability · verification. Do not restrict your \
assessment to explicitly named concerns; address material omissions \
proportionately — never invent requirements or expand scope.\n\n\
Engineering lenses — call skill(name) to load a guide; guides inform \
judgment and never add requirements:\n{}\n\n\
{}",
        crate::charter::CHARTER,
        crate::skills::index(),
        crate::charter::COMPLETION,
    )
}

/// v1 seam: frozen repository-epoch segment (tree-sitter map, build/test
/// commands, conventions). Generated once per epoch, then immutable.
/// Returns None until the repo index lands.
pub fn epoch_segment() -> Option<String> {
    None
}

/// Assemble model-visible context in stable-to-volatile order.
/// [static system] + [epoch?] + [project guidance?] + [append-only
/// history]. `system` is normally `system()`; certification may inject
/// a variant to deliberately invalidate the static layer. Guidance is
/// per-workspace but stable within it — its own segment keeps the
/// shared prefix identical across repos.
pub fn compile(history: &[Message], system: &str, guidance: Option<&str>) -> Vec<Message> {
    let mut out = Vec::with_capacity(history.len() + 3);
    out.push(Message::System {
        content: system.to_string(),
    });
    if let Some(seg) = epoch_segment() {
        out.push(Message::System { content: seg });
    }
    if let Some(g) = guidance {
        out.push(Message::System {
            content: g.to_string(),
        });
    }
    out.extend(history.iter().cloned());
    out
}

/// Per-layer fingerprints: local determinism diagnostics. A changed hash
/// identifies WHICH layer drifted; it is not proof of a provider cache hit.
pub struct LayerHashes {
    pub static_prefix: String,
    pub tool_schema: String,
    pub epoch_prefix: Option<String>,
}

pub fn layer_hashes(tools: &[Value], system: &str) -> LayerHashes {
    LayerHashes {
        static_prefix: sha256_hex(system.as_bytes()),
        tool_schema: sha256_hex(serde_json::to_string(tools).unwrap_or_default().as_bytes()),
        epoch_prefix: epoch_segment().map(|s| sha256_hex(s.as_bytes())),
    }
}

/// Hash of the fully-serialized request — changes every turn as history
/// grows (normal); drift in EARLY layers is what matters.
pub fn request_fingerprint(messages: &[Message]) -> String {
    sha256_hex(
        serde_json::to_string(messages)
            .unwrap_or_default()
            .as_bytes(),
    )
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Rough input estimate (~4 chars/token). Estimate only — providers
/// tokenize differently; used for the context budget guard, never billing.
pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
        .sum::<usize>()
        / 4
}
