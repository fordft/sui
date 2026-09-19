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

/// Borrowed request view: freshly-built head messages (system/epoch/
/// guidance) plus the append-only history slice. Serializes to the exact
/// JSON array `Vec<Message>` would produce, without deep-cloning history
/// every turn.
pub struct Compiled<'a> {
    head: Vec<Message>,
    tail: &'a [Message],
}

impl<'a> Compiled<'a> {
    /// View over a bare history slice — no head messages. Test/debug use.
    pub fn view(tail: &'a [Message]) -> Self {
        Compiled {
            head: Vec::new(),
            tail,
        }
    }

    pub fn len(&self) -> usize {
        self.head.len() + self.tail.len()
    }

    pub fn is_empty(&self) -> bool {
        self.head.is_empty() && self.tail.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Message> {
        self.head.iter().chain(self.tail)
    }
}

impl serde::Serialize for Compiled<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = s.serialize_seq(Some(self.len()))?;
        for m in self.iter() {
            seq.serialize_element(m)?;
        }
        seq.end()
    }
}

/// Assemble model-visible context in stable-to-volatile order.
/// [static system] + [epoch?] + [project guidance?] + [append-only
/// history]. `system` is normally `system()`; certification may inject
/// a variant to deliberately invalidate the static layer. Guidance is
/// per-workspace but stable within it — its own segment keeps the
/// shared prefix identical across repos.
pub fn compile<'a>(history: &'a [Message], system: &str, guidance: Option<&str>) -> Compiled<'a> {
    let mut head = Vec::with_capacity(3);
    head.push(Message::System {
        content: system.to_string(),
    });
    if let Some(seg) = epoch_segment() {
        head.push(Message::System { content: seg });
    }
    if let Some(g) = guidance {
        head.push(Message::System {
            content: g.to_string(),
        });
    }
    Compiled {
        head,
        tail: history,
    }
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
/// grows (normal); drift in EARLY layers is what matters. Serialization
/// streams straight into the hasher — same digest as hashing the
/// to_string bytes, without materializing the full request body twice.
pub fn request_fingerprint(messages: &Compiled<'_>) -> String {
    struct Sink(Sha256);
    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.update(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut h = Sink(Sha256::new());
    let _ = serde_json::to_writer(&mut h, messages);
    format!("{:x}", h.0.finalize())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    format!("{:x}", h.finalize())
}

/// Rough input estimate (~4 chars/token). Estimate only — providers
/// tokenize differently; used for the context budget guard, never billing.
/// Sums field lengths directly — equivalent to the serialized size within
/// a few percent, without a JSON pass per turn.
pub fn estimate_tokens(messages: &Compiled<'_>) -> usize {
    const OVERHEAD: usize = 24; // role tag, keys, escapes — per-message JSON envelope
    messages
        .iter()
        .map(|m| {
            OVERHEAD
                + match m {
                    Message::System { content } | Message::User { content } => content.len(),
                    Message::Assistant {
                        content,
                        tool_calls,
                        reasoning_content,
                        response_items,
                    } => {
                        content.as_deref().unwrap_or("").len()
                            + reasoning_content.as_deref().unwrap_or("").len()
                            + tool_calls
                                .as_deref()
                                .unwrap_or(&[])
                                .iter()
                                .map(|t| {
                                    t.id.len() + t.function.name.len() + t.function.arguments.len()
                                })
                                .sum::<usize>()
                            + response_items
                                .iter()
                                .map(|v| v.to_string().len())
                                .sum::<usize>()
                    }
                    Message::Tool {
                        tool_call_id,
                        content,
                    } => tool_call_id.len() + content.len(),
                }
        })
        .sum::<usize>()
        / 4
}

/// Largest index ≤ `i` that falls on a UTF-8 char boundary of `s`.
/// Use before every `&s[..n]` slice — command output, diffs, and web
/// content carry arbitrary multibyte text.
pub(crate) fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}
