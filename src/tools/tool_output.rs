//! Ephemeral recovery of the complete, bounded result captured by Bash.
//! This never reconstructs uncaptured process output or executes a command.
use anyhow::{ensure, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::{ExecKind, ExecOut, ToolContext};

pub const ID_LEN: usize = 49;
const MAX_ENTRIES: usize = 8;
const MAX_BYTES: usize = 512 * 1024;
const MAX_ENTRY_BYTES: usize = 128 * 1024;
const DEFAULT_OUTPUT_BYTES: usize = 12_000;

struct Entry {
    id: String,
    raw: Arc<str>,
}

#[derive(Default)]
struct State {
    entries: VecDeque<Entry>,
    bytes: usize,
    counter: u64,
}

/// Owned by one ToolContext. Handles are random per context and are never
/// restored from journals; retained observations are memory-only and bounded.
pub struct OutputStore {
    workspace: PathBuf,
    root: PathBuf,
    nonce: u128,
    state: Mutex<State>,
}

impl OutputStore {
    fn new(workspace: PathBuf, root: PathBuf) -> Self {
        Self {
            workspace,
            root,
            nonce: rand::random(),
            state: Mutex::new(State::default()),
        }
    }

    fn validate_workspace(&self, ctx: &ToolContext) -> Result<()> {
        ensure!(
            self.workspace == ctx.workspace,
            "tool output workspace changed"
        );
        ensure!(
            self.root == ctx.workspace.canonicalize()?,
            "tool output workspace changed"
        );
        Ok(())
    }

    fn insert(&self, raw: &str) -> Result<String> {
        let raw: Arc<str> = Arc::from(raw);
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("tool output store unavailable"))?;
        let counter = state
            .counter
            .checked_add(1)
            .context("tool output handle counter exhausted")?;
        let id = format!("{:032x}-{counter:016x}", self.nonce);
        while state.entries.len() >= MAX_ENTRIES || state.bytes + raw.len() > MAX_BYTES {
            let Some(oldest) = state.entries.pop_front() else {
                break;
            };
            state.bytes -= oldest.raw.len();
        }
        state.counter = counter;
        state.bytes += raw.len();
        state.entries.push_back(Entry {
            id: id.clone(),
            raw,
        });
        Ok(id)
    }

    fn get(&self, id: &str) -> Result<Option<Arc<str>>> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("tool output store unavailable"))?;
        Ok(state
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .map(|entry| entry.raw.clone()))
    }
}

/// Retain only an already bounded original Bash envelope. The caller decides
/// eligibility and falls back to that original result on every storage error.
pub fn retain(ctx: &ToolContext, raw: &str) -> Result<Option<String>> {
    if raw.len() > MAX_ENTRY_BYTES {
        return Ok(None);
    }
    let root = ctx.workspace.canonicalize()?;
    let store = ctx
        .tool_outputs
        .get_or_init(|| OutputStore::new(ctx.workspace.clone(), root));
    store.validate_workspace(ctx)?;
    store.insert(raw).map(Some)
}

pub fn schema() -> Value {
    json!({
        "type":"function",
        "function":{
            "name":"read_tool_output",
            "description":"Read an exact UTF-8 byte slice of the bounded original Bash result retained before compaction. Does not rerun the command or recover bytes lost at the Bash capture cap. Handles are memory-only in this agent/workspace and may be evicted; they are unavailable after resume. Whole output is capped by max_bytes. Continue at next_offset when more is true.",
            "parameters":{
                "type":"object",
                "properties":{
                    "id":{"type":"string","minLength":ID_LEN,"maxLength":ID_LEN,"pattern":"^[0-9a-f]{32}-[0-9a-f]{16}$"},
                    "offset":{"type":"integer","minimum":0,"description":"UTF-8 byte offset, default 0; must be a character boundary"},
                    "max_bytes":{"type":"integer","minimum":1024,"maximum":24000,"description":"Whole output byte cap, default 12000"}
                },
                "required":["id"],
                "additionalProperties":false
            }
        }
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    id: String,
    #[serde(default)]
    offset: usize,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
}

fn default_bytes() -> usize {
    DEFAULT_OUTPUT_BYTES
}

fn valid_id(id: &str) -> bool {
    id.len() == ID_LEN
        && id.as_bytes()[32] == b'-'
        && id.bytes().enumerate().all(|(index, byte)| {
            index == 32 || byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
        })
}

fn error(message: &str) -> ExecOut {
    ExecOut::plain(format!("status: error\nerror: {message}"), ExecKind::Error)
}

fn cancelled() -> ExecOut {
    ExecOut::plain(
        "status: cancelled\nerror: read_tool_output cancelled".into(),
        ExecKind::Cancelled,
    )
}

fn header(id: &str, total: usize, offset: usize, end: usize, next: &str, more: bool) -> String {
    format!(
        "status: success\nobservation: original_captured_result\ncapture_truncated: false\nrerun: false\nid: {id}\ntotal_bytes: {total}\noffset: {offset}\nend_offset: {end}\nnext_offset: {next}\nmore: {more}\ncontent:\n"
    )
}

pub async fn execute(
    ctx: &ToolContext,
    value: &Value,
    cancel: impl Future<Output = ()>,
) -> Result<ExecOut> {
    tokio::pin!(cancel);
    if tokio::select! { biased; _ = &mut cancel => true, _ = std::future::ready(()) => false } {
        return Ok(cancelled());
    }
    // Validate the bounded identifier before allocating owned arguments. The
    // borrowed JSON deserializer also avoids cloning unknown, oversized fields.
    if !value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(valid_id)
    {
        return Ok(error("invalid read_tool_output id"));
    }
    let args = match Args::deserialize(value) {
        Ok(args) => args,
        Err(_) => return Ok(error("invalid read_tool_output arguments")),
    };
    if !(1024..=24_000).contains(&args.max_bytes) {
        return Ok(error("max_bytes must be 1024-24000"));
    }
    let Some(store) = ctx.tool_outputs.get() else {
        return Ok(error(
            "tool output unavailable: unknown, evicted, foreign or resumed handle",
        ));
    };
    if store.validate_workspace(ctx).is_err() {
        return Ok(error(
            "tool output unavailable: workspace changed or inaccessible",
        ));
    }
    let raw = match store.get(&args.id) {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            return Ok(error(
                "tool output unavailable: unknown, evicted, foreign or resumed handle",
            ))
        }
        Err(_) => return Ok(error("tool output store unavailable")),
    };
    if args.offset > raw.len() || !raw.is_char_boundary(args.offset) {
        return Ok(error(
            "offset must be in range and on a UTF-8 character boundary",
        ));
    }
    if tokio::select! { biased; _ = &mut cancel => true, _ = std::future::ready(()) => false } {
        return Ok(cancelled());
    }
    // Reserve the longest possible header: end/next use the total's digit
    // width, "none" may be longer than that number, and false is longer than true.
    let total_text = raw.len().to_string();
    let reserved_next = if total_text.len() < 4 {
        "none"
    } else {
        &total_text
    };
    let reserved = header(
        &args.id,
        raw.len(),
        args.offset,
        raw.len(),
        reserved_next,
        false,
    )
    .len();
    let available = args.max_bytes.saturating_sub(reserved);
    let mut end = args.offset.saturating_add(available).min(raw.len());
    while !raw.is_char_boundary(end) {
        end -= 1;
    }
    let more = end < raw.len();
    let next = if more { end.to_string() } else { "none".into() };
    let mut text = header(&args.id, raw.len(), args.offset, end, &next, more);
    text.push_str(&raw[args.offset..end]);
    debug_assert!(text.len() <= args.max_bytes);
    if tokio::select! { biased; _ = &mut cancel => true, _ = std::future::ready(()) => false } {
        return Ok(cancelled());
    }
    let mut out = ExecOut::plain(text, ExecKind::Success);
    out.truncated = args.offset != 0 || more;
    Ok(out)
}
