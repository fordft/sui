//! Task-directed syntax context. Policy walks and source reads are always fresh;
//! only bounded, content-validated parse facts are cached per ToolContext.
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

use super::{inventory, ExecKind, ExecOut, ToolContext};
use facts::{Facts, Region};

mod facts;
mod output;
mod privacy;

const MAX_NODES: usize = 500_000;
const SCAN_TIME: Duration = Duration::from_secs(3);
const CACHE_ENTRIES: usize = 128;
const CACHE_BYTES: usize = 2 * 1024 * 1024;

pub fn schema() -> Value {
    json!({
        "type":"function",
        "function":{
            "name":"code_context",
            "description":"Retrieve task-directed workspace code context without a process, model or network. Search ranks literal query terms in paths, syntax names and source; read expands an anchored line into syntax context. Returns exact numbered excerpts and current source hashes. Fresh inventory ignore/symlink policy applies. Local parse cache telemetry is separate from provider caching; coverage is partial when bounded or unsupported. Syntax relevance is not semantic dependency or verification proof.",
            "parameters":{
                "type":"object",
                "properties":{
                    "action":{"type":"string","enum":["search","read"]},
                    "query":{"type":"string","description":"Required for search: nonempty case-insensitive literal whitespace-separated terms, <=256 bytes"},
                    "path":{"type":"string","description":"Workspace-relative search file/directory (default .); read requires a regular UTF-8 source file"},
                    "line":{"type":"integer","minimum":1,"description":"Required for read: 1-based source line"},
                    "limit":{"type":"integer","minimum":1,"maximum":20,"description":"Search regions, default 5; one region per file"},
                    "max_bytes":{"type":"integer","minimum":1024,"maximum":24000,"description":"Whole output byte budget, default 12000"}
                },
                "required":["action"],
                "additionalProperties":false
            }
        }
    })
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Search,
    Read,
}
impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::Read => "read",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: Action,
    #[serde(default)]
    query: String,
    path: Option<String>,
    line: Option<usize>,
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default = "default_bytes")]
    max_bytes: usize,
}
fn default_limit() -> usize {
    5
}
fn default_bytes() -> usize {
    12_000
}

struct CacheEntry {
    hash: String,
    facts: Arc<Facts>,
    bytes: usize,
    tick: u64,
}
#[derive(Default)]
struct Cache {
    entries: HashMap<(PathBuf, PathBuf, &'static str), CacheEntry>,
    bytes: usize,
    tick: u64,
}

/// Workspace-local in-memory parse facts. Source contents and policy acceptance
/// are never cached; canonical root/path/language/hash must match on every call.
#[derive(Default)]
pub struct CodeContextService {
    cache: Mutex<Cache>,
}
impl CodeContextService {
    pub fn new() -> Self {
        Self::default()
    }

    fn get(&self, root: &Path, path: &Path, hash: &str) -> Option<Arc<Facts>> {
        let key = (
            root.to_path_buf(),
            path.to_path_buf(),
            inventory::language_name(path),
        );
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        cache.tick = cache.tick.wrapping_add(1);
        let tick = cache.tick;
        let entry = cache.entries.get_mut(&key)?;
        if entry.hash != hash {
            return None;
        }
        entry.tick = tick;
        Some(entry.facts.clone())
    }

    fn put(&self, root: &Path, path: &Path, hash: &str, facts: Arc<Facts>) {
        let key = (
            root.to_path_buf(),
            path.to_path_buf(),
            inventory::language_name(path),
        );
        let bytes =
            facts.estimated_bytes() + root.as_os_str().len() + path.as_os_str().len() + hash.len();
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = cache.entries.remove(&key) {
            cache.bytes -= old.bytes;
        }
        if bytes > CACHE_BYTES {
            return;
        }
        while cache.entries.len() >= CACHE_ENTRIES || cache.bytes + bytes > CACHE_BYTES {
            let Some(oldest) = cache
                .entries
                .iter()
                .min_by_key(|(_, e)| e.tick)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(old) = cache.entries.remove(&oldest) {
                cache.bytes -= old.bytes;
            }
        }
        cache.tick = cache.tick.wrapping_add(1);
        let tick = cache.tick;
        cache.bytes += bytes;
        cache.entries.insert(
            key,
            CacheEntry {
                hash: hash.into(),
                facts,
                bytes,
                tick,
            },
        );
    }
}

struct Stop(Arc<AtomicBool>);
impl Drop for Stop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

pub async fn execute(
    ctx: &ToolContext,
    value: &Value,
    cancel: impl Future<Output = ()>,
) -> Result<ExecOut> {
    tokio::pin!(cancel);
    // Register Notify cancellation before starting any blocking filesystem work.
    if tokio::select! { biased; _ = &mut cancel => true, _ = std::future::ready(()) => false } {
        return Ok(cancelled());
    }
    let args: Args = match serde_json::from_value(value.clone()) {
        Ok(args) => args,
        Err(_) => return Ok(error("invalid code_context arguments")),
    };
    if args.query.len() > 256
        || args
            .path
            .as_ref()
            .is_some_and(|p| p.is_empty() || p.len() > 4096)
        || !(1..=20).contains(&args.limit)
        || !(1024..=24_000).contains(&args.max_bytes)
        || args.line == Some(0)
        || (matches!(args.action, Action::Search) && args.query.trim().is_empty())
        || (matches!(args.action, Action::Read) && (args.path.is_none() || args.line.is_none()))
    {
        return Ok(error("search requires nonempty query <=256 bytes; read requires path and positive line; limit 1-20, max_bytes 1024-24000, path <=4096 bytes"));
    }
    let workspace = ctx.workspace.clone();
    let service = ctx
        .code_context
        .get_or_init(|| Arc::new(CodeContextService::new()))
        .clone();
    let stop = Stop(Arc::new(AtomicBool::new(false)));
    let flag = stop.0.clone();
    let deadline = Instant::now() + SCAN_TIME;
    let mut worker =
        tokio::task::spawn_blocking(move || run(workspace, args, &service, &flag, deadline));
    tokio::select! {
        biased;
        _ = &mut cancel => {
            stop.0.store(true, Ordering::Relaxed);
            let _ = worker.await;
            Ok(cancelled())
        }
        result = &mut worker => result.context("code_context worker failed")?,
    }
}

fn error(message: &str) -> ExecOut {
    ExecOut::plain(format!("status: error\nerror: {message}"), ExecKind::Error)
}
fn cancelled() -> ExecOut {
    ExecOut::plain(
        "status: cancelled\nerror: code_context cancelled".into(),
        ExecKind::Cancelled,
    )
}

struct Source {
    path: String,
    hash: String,
    text: String,
    offsets: Vec<usize>,
}
impl Source {
    fn new(path: String, text: String) -> Self {
        let hash = format!("{:x}", Sha256::digest(text.as_bytes()));
        let mut offsets = Vec::new();
        if !text.is_empty() {
            offsets.push(0);
            for (index, byte) in text.bytes().enumerate() {
                if byte == b'\n' && index + 1 < text.len() {
                    offsets.push(index + 1);
                }
            }
        }
        Self {
            path,
            hash,
            text,
            offsets,
        }
    }
    fn line(&self, line: usize) -> &str {
        let start = self.offsets[line - 1];
        let end = self.offsets.get(line).copied().unwrap_or(self.text.len());
        self.text[start..end]
            .strip_suffix('\n')
            .unwrap_or(&self.text[start..end])
    }
    fn lines(&self) -> usize {
        self.offsets.len()
    }
}

struct Candidate {
    source: Arc<Source>,
    facts: Option<Arc<Facts>>,
    definition: Option<usize>,
    region: Region,
    anchor: usize,
    score: (u32, u32, bool),
    reasons: Vec<&'static str>,
}
impl Candidate {
    fn better_than(&self, other: &Self) -> bool {
        self.score > other.score
            || (self.score == other.score
                && (
                    self.source.path.as_str(),
                    self.region.end - self.region.start,
                    self.anchor,
                ) < (
                    other.source.path.as_str(),
                    other.region.end - other.region.start,
                    other.anchor,
                ))
    }
}

#[derive(Default)]
struct Stats {
    scan: inventory::Scan,
    read: usize,
    hits: usize,
    misses: usize,
    matches: usize,
}

fn run(
    workspace: PathBuf,
    args: Args,
    service: &CodeContextService,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Result<ExecOut> {
    let mut stats = Stats::default();
    if !inventory::checkpoint(&mut stats.scan, cancel, deadline) {
        return Ok(if cancel.load(Ordering::Relaxed) {
            cancelled()
        } else {
            ExecOut::plain(
                "status: timeout\nerror: code_context deadline expired before work began\nstop_reason: time_limit".into(),
                ExecKind::Timeout,
            )
        });
    }
    let root = match workspace.canonicalize() {
        Ok(root) => root,
        Err(_) => return Ok(error("workspace is unavailable")),
    };
    let path = match super::fs::resolve(&root, args.path.as_deref().unwrap_or(".")) {
        Ok(path) => path,
        Err(_) => return Ok(error("path is outside the workspace or unsafe")),
    };
    let scope = path.strip_prefix(&root)?;
    if matches!(args.action, Action::Read)
        && !path
            .symlink_metadata()
            .is_ok_and(|m| m.is_file() && !m.file_type().is_symlink())
    {
        return Ok(error("read requires a regular non-symlink source file"));
    }
    if !path.symlink_metadata().is_ok() {
        return Ok(error("path is unavailable"));
    }
    let files = inventory::collect_for_analysis(&root, scope, &mut stats.scan, cancel, deadline);
    let traversal_stop = stats.scan.stopped.take();
    let terms: Vec<String> = args
        .query
        .split_whitespace()
        .map(str::to_lowercase)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut selected: Vec<Candidate> = Vec::new();
    for path in files {
        if !inventory::checkpoint(&mut stats.scan, cancel, deadline) {
            break;
        }
        let Some(display) = path.strip_prefix(&root)?.to_str() else {
            stats.scan.skipped += 1;
            continue;
        };
        stats.scan.files += 1;
        let source = match inventory::read_source(&root, &path, &mut stats.scan, cancel, deadline) {
            Ok(source) => source,
            Err(_) => {
                stats.scan.skipped += 1;
                continue;
            }
        };
        stats.read += 1;
        let Some(sensitive) = privacy::contains_credentials(&path, &source, cancel, deadline)
        else {
            stats.scan.stopped = Some("time_limit");
            break;
        };
        if sensitive {
            stats.scan.skipped += 1;
            if matches!(args.action, Action::Read) {
                return Ok(error(
                    "source contains credential-like material; context withheld",
                ));
            }
            continue;
        }
        let source = Arc::new(Source::new(display.into(), source));
        let parsed = if inventory::language(&path).is_some() {
            if let Some(cached) = service.get(&root, &path, &source.hash) {
                if stats.scan.nodes.saturating_add(cached.nodes) > MAX_NODES {
                    stats.scan.stopped = Some("node_limit");
                    break;
                }
                stats.scan.nodes += cached.nodes;
                stats.scan.syntax_errors += usize::from(cached.syntax_error);
                stats.hits += 1;
                Some(cached)
            } else {
                stats.misses += 1;
                let parsed = facts::parse(&path, &source.text, &mut stats.scan, cancel, deadline)?
                    .map(Arc::new);
                if let Some(facts) = &parsed {
                    service.put(&root, &path, &source.hash, facts.clone());
                }
                parsed
            }
        } else {
            stats.scan.unsupported += 1;
            None
        };
        if !inventory::checkpoint(&mut stats.scan, cancel, deadline) {
            break;
        }
        let candidate = if matches!(args.action, Action::Read) {
            let line = args.line.unwrap_or(1);
            if line > source.lines() {
                return Ok(error("line is outside the current source"));
            }
            Some(read_candidate(source, parsed, line))
        } else {
            search_candidate(source, parsed, &terms, &mut stats.scan, cancel, deadline)
        };
        if let Some(candidate) = candidate {
            stats.matches += 1;
            selected.push(candidate);
            selected.sort_by(|a, b| {
                b.score
                    .cmp(&a.score)
                    .then_with(|| a.source.path.cmp(&b.source.path))
                    .then(a.anchor.cmp(&b.anchor))
            });
            selected.truncate(args.limit);
        }
    }
    if cancel.load(Ordering::Relaxed) {
        return Ok(cancelled());
    }
    if stats.scan.stopped.is_none() {
        stats.scan.stopped = traversal_stop;
    }
    if matches!(args.action, Action::Read) && stats.matches == 0 && stats.scan.stopped.is_none() {
        return Ok(error(
            "source is ignored, excluded, binary, oversized, unreadable or invalid UTF-8",
        ));
    }
    Ok(output::render(&args, selected, &stats, cancel, deadline))
}

fn read_candidate(source: Arc<Source>, facts: Option<Arc<Facts>>, anchor: usize) -> Candidate {
    let definition = facts.as_ref().and_then(|facts| {
        facts
            .definitions
            .iter()
            .enumerate()
            .filter(|(_, d)| d.attached_start <= anchor && anchor <= d.body.end)
            .min_by_key(|(_, d)| (d.body.end - d.attached_start, d.body.start))
            .map(|(index, _)| index)
    });
    let region = definition.map_or(
        Region {
            start: anchor.saturating_sub(12).max(1),
            end: anchor.saturating_add(24).min(source.lines()),
        },
        |index| {
            let d = &facts.as_ref().expect("definition facts").definitions[index];
            Region {
                start: d.attached_start,
                end: d.body.end,
            }
        },
    );
    Candidate {
        source,
        facts,
        definition,
        region,
        anchor,
        score: (0, 0, definition.is_some()),
        reasons: vec![
            "anchored_line",
            if definition.is_some() {
                "enclosing_syntax_definition"
            } else {
                "plain_text_window"
            },
        ],
    }
}

fn mask(
    text: &str,
    terms: &[String],
    stats: &mut inventory::Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> u128 {
    let mut mask = 0;
    for (index, term) in terms.iter().enumerate() {
        if !inventory::checkpoint(stats, cancel, deadline) {
            break;
        }
        if text.contains(term) {
            mask |= 1u128 << index;
        }
    }
    mask
}

fn search_candidate(
    source: Arc<Source>,
    facts: Option<Arc<Facts>>,
    terms: &[String],
    stats: &mut inventory::Scan,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Option<Candidate> {
    let path_mask = mask(&source.path.to_lowercase(), terms, stats, cancel, deadline);
    let mut matching = Vec::new();
    for line in 1..=source.lines() {
        if !inventory::checkpoint(stats, cancel, deadline) {
            return None;
        }
        let found = mask(
            &source.line(line).to_lowercase(),
            terms,
            stats,
            cancel,
            deadline,
        );
        if found != 0 {
            matching.push((line, found));
        }
    }
    let body_matches = |region: Region| -> (u128, Option<usize>) {
        let start = matching.partition_point(|(line, _)| *line < region.start);
        let end = matching.partition_point(|(line, _)| *line <= region.end);
        let found = matching[start..end]
            .iter()
            .fold(0, |bits, (_, found)| bits | found);
        (
            found,
            matching
                .get(start)
                .filter(|(line, _)| *line <= region.end)
                .map(|(line, _)| *line),
        )
    };
    let mut best: Option<Candidate> = None;
    let mut consider = |region: Region,
                        definition: Option<usize>,
                        name_mask: u128,
                        body_mask: u128,
                        anchor: usize| {
        let all = path_mask | name_mask | body_mask;
        if all == 0 {
            return;
        }
        let mut reasons = Vec::new();
        if path_mask != 0 {
            reasons.push("path_terms");
        }
        if name_mask != 0 {
            reasons.push("syntax_name_terms");
        }
        if body_mask != 0 {
            reasons.push("source_terms");
        }
        let candidate = Candidate {
            source: source.clone(),
            facts: facts.clone(),
            definition,
            region,
            anchor,
            score: (
                all.count_ones(),
                path_mask.count_ones() * 8
                    + name_mask.count_ones() * 12
                    + body_mask.count_ones() * 2,
                definition.is_some(),
            ),
            reasons,
        };
        if best.as_ref().is_none_or(|old| candidate.better_than(old)) {
            best = Some(candidate);
        }
    };
    if let Some(facts) = &facts {
        for (index, d) in facts.definitions.iter().enumerate() {
            if !inventory::checkpoint(stats, cancel, deadline) {
                return None;
            }
            let name_mask = mask(&d.name.to_lowercase(), terms, stats, cancel, deadline);
            let region = Region {
                start: d.attached_start,
                end: d.body.end,
            };
            let (body_mask, anchor) = body_matches(region);
            consider(
                region,
                Some(index),
                name_mask,
                body_mask,
                anchor.unwrap_or(d.body.start),
            );
        }
    }
    for &(line, _) in &matching {
        if !inventory::checkpoint(stats, cancel, deadline) {
            return None;
        }
        let region = Region {
            start: line.saturating_sub(8).max(1),
            end: line.saturating_add(16).min(source.lines()),
        };
        consider(region, None, 0, body_matches(region).0, line);
    }
    if source.lines() > 0 {
        consider(
            Region {
                start: 1,
                end: 25.min(source.lines()),
            },
            None,
            0,
            body_matches(Region {
                start: 1,
                end: 25.min(source.lines()),
            })
            .0,
            1,
        );
    }
    best
}
