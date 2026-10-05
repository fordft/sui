//! Native, workspace-scoped Rust language queries. The LSP process belongs to
//! Sui; observations are bounded data, never build/test acceptance evidence.
use anyhow::Result;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};

mod client;
use super::{ExecKind, ExecOut, ToolContext};
pub use client::Service as CodeIntelService;
use client::ServiceError;

const MAX_INPUT: usize = 32 * 1024 * 1024;
const MAX_OUTPUT: usize = 24 * 1024;
const MAX_PROCESSED: usize = 4096;
const RENDER_TIMEOUT: Duration = Duration::from_secs(3);

pub fn schema() -> Value {
    json!({
        "type":"function",
        "function":{
            "name":"code_intel",
            "description":"Query Rust definitions, references or file diagnostics through a Sui-managed language server. Workspace-only results; line/column are 1-based Unicode character positions. Requires installed rust-analyzer. Scripts/proc macros are disabled; diagnostics are not compiler/test proof. Inspect analysis_complete and read_file before editing.",
            "parameters":{
                "type":"object",
                "properties":{
                    "action":{"type":"string","enum":["definition","references","diagnostics"]},
                    "path":{"type":"string","description":"Workspace-relative Rust source file"},
                    "line":{"type":"integer","minimum":1,"description":"1-based line; required for definition/references"},
                    "column":{"type":"integer","minimum":1,"description":"1-based Unicode character column; required for definition/references"},
                    "limit":{"type":"integer","minimum":1,"maximum":200,"description":"Max rows, default 50"}
                },
                "required":["action","path"],
                "additionalProperties":false
            }
        }
    })
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Action {
    Definition,
    References,
    Diagnostics,
}
impl Action {
    fn name(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::References => "references",
            Self::Diagnostics => "diagnostics",
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    action: Action,
    path: String,
    line: Option<usize>,
    column: Option<usize>,
    #[serde(default = "default_limit")]
    limit: usize,
}
fn default_limit() -> usize {
    50
}

pub async fn execute(
    ctx: &ToolContext,
    value: &Value,
    cancel: impl Future<Output = ()>,
) -> Result<ExecOut> {
    tokio::pin!(cancel);
    // Poll/register cancellation before filesystem work. Notify::notify_waiters
    // does not retain a permit if no waiter has been polled yet.
    let cancelled = tokio::select! {
        biased;
        _ = &mut cancel => true,
        _ = std::future::ready(()) => false,
    };
    if cancelled {
        if let Some(service) = ctx.code_intel.get() {
            service.invalidate_idle().await;
        }
        return Ok(error(
            "code intelligence cancelled; idle session closed",
            ExecKind::Cancelled,
        ));
    }
    let args: Args = match serde_json::from_value(value.clone()) {
        Ok(args) => args,
        Err(_) => return Ok(error("invalid code_intel arguments", ExecKind::Error)),
    };
    if args.path.is_empty()
        || args.path.len() > 4096
        || !(1..=200).contains(&args.limit)
        || args.line == Some(0)
        || args.column == Some(0)
        || (!matches!(args.action, Action::Diagnostics)
            && (args.line.is_none() || args.column.is_none()))
    {
        return Ok(error(
            "use a Rust path, positive line/column for definition/references and limit 1-200",
            ExecKind::Error,
        ));
    }
    let root = ctx.workspace.canonicalize()?;
    let path = super::fs::resolve_ctx(ctx, &args.path)?;
    if !allowed(&root, &path) {
        return Ok(error(
            "code_intel requires a regular, non-symlink Rust source within this workspace",
            ExecKind::Error,
        ));
    }
    let read_root = root.clone();
    let read_path = path.clone();
    let stopped = Arc::new(AtomicBool::new(false));
    let _stop_on_drop = StopOnDrop(stopped.clone());
    let read_control = RenderControl {
        stopped: stopped.clone(),
        deadline: Instant::now() + RENDER_TIMEOUT,
    };
    let mut read_worker = tokio::task::spawn_blocking(move || {
        read_control.check()?;
        let result = super::inventory::read_for_analysis(
            &read_root,
            &read_path,
            MAX_INPUT,
            &read_control.stopped,
            read_control.deadline,
        );
        read_control.check()?;
        Ok::<_, RenderError>(result)
    });
    let read_result = tokio::select! {
        biased;
        _ = &mut cancel => {
            stopped.store(true, Ordering::Relaxed);
            let _ = read_worker.await?;
            if let Some(service) = ctx.code_intel.get() { service.invalidate_idle().await; }
            return Ok(error("code intelligence cancelled; idle session closed", ExecKind::Cancelled));
        }
        result = &mut read_worker => result?,
    };
    let (input, read) = match read_result {
        Ok(result) => result,
        Err(failure) => {
            if let Some(service) = ctx.code_intel.get() {
                service.invalidate_idle().await;
            }
            let kind = if matches!(failure, RenderError::Cancelled) {
                ExecKind::Cancelled
            } else {
                ExecKind::Timeout
            };
            return Ok(error(
                "code intelligence source read interrupted; idle session closed",
                kind,
            ));
        }
    };
    let input = match input {
        Ok(text) => text,
        Err(_) => {
            return Ok(error(
                "source unavailable, binary, oversized or invalid UTF-8",
                ExecKind::Error,
            ))
        }
    };
    let service = ctx
        .code_intel
        .get_or_init(|| CodeIntelService::new(root.clone()));
    if service.workspace() != root {
        return Ok(error(
            "language service belongs to a different workspace",
            ExecKind::Error,
        ));
    }
    let response = match service
        .execute(
            args.action.name(),
            &path,
            &input,
            args.line.unwrap_or(1),
            args.column.unwrap_or(1),
            args.limit,
            &mut cancel,
        )
        .await
    {
        Ok(response) => response,
        Err(err) => {
            let kind = match err {
                ServiceError::Cancelled => ExecKind::Cancelled,
                ServiceError::Timeout => ExecKind::Timeout,
                _ => ExecKind::Error,
            };
            return Ok(error(&err.to_string(), kind));
        }
    };
    // URI/range interpretation can read target files. Keep that work off the
    // async runtime and use the same guarded reader as inventory.
    let control = RenderControl {
        stopped: stopped.clone(),
        deadline: Instant::now() + RENDER_TIMEOUT,
    };
    let mut worker = tokio::task::spawn_blocking(move || {
        render(&root, &path, input, read, args, response, &control)
    });
    let result = tokio::select! {
        biased;
        _ = &mut cancel => {
            stopped.store(true, Ordering::Relaxed);
            let _ = worker.await?;
            service.invalidate().await;
            return Ok(error("code intelligence cancelled; session closed", ExecKind::Cancelled));
        }
        result = &mut worker => result?,
    };
    match result {
        Ok(out) => Ok(out),
        Err(failure) => {
            service.invalidate().await;
            let (message, kind) = match failure {
                RenderError::Cancelled => (
                    "code intelligence cancelled; session closed",
                    ExecKind::Cancelled,
                ),
                RenderError::Timeout => (
                    "code intelligence result processing exceeded its deadline; session closed",
                    ExecKind::Timeout,
                ),
                RenderError::Protocol => (
                    "invalid code intelligence semantic response; session closed",
                    ExecKind::Error,
                ),
            };
            Ok(error(message, kind))
        }
    }
}

struct StopOnDrop(Arc<AtomicBool>);
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}
#[derive(Debug)]
enum RenderError {
    Cancelled,
    Timeout,
    Protocol,
}
type RenderResult<T> = std::result::Result<T, RenderError>;
struct RenderControl {
    stopped: Arc<AtomicBool>,
    deadline: Instant,
}
impl RenderControl {
    fn check(&self) -> RenderResult<()> {
        if self.stopped.load(Ordering::Relaxed) {
            return Err(RenderError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(RenderError::Timeout);
        }
        Ok(())
    }
}

fn error(message: &str, kind: ExecKind) -> ExecOut {
    ExecOut::plain(
        format!(
            "status: {}\nerror: {message}",
            if kind == ExecKind::Cancelled {
                "cancelled"
            } else {
                "error"
            }
        ),
        kind,
    )
}

fn allowed(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return false;
    };
    if path.extension().is_none_or(|e| e != "rs") || super::inventory::excluded(relative) {
        return false;
    }
    let mut probe = root.to_path_buf();
    for component in relative.components() {
        probe.push(component);
        let Ok(metadata) = probe.symlink_metadata() else {
            return false;
        };
        if metadata.file_type().is_symlink() {
            return false;
        }
    }
    path.is_file()
}

enum Columns {
    Ascii(usize),
    Utf16(Vec<usize>),
}

/// Index each source and each requested Unicode line once. Repeated ranges
/// cannot multiply full-file/full-line scans after a bounded LSP response.
struct Source {
    text: String,
    lines: Vec<usize>,
    columns: HashMap<usize, Columns>,
}
impl Source {
    fn new(text: String, control: &RenderControl) -> RenderResult<Self> {
        let mut lines = vec![0];
        for (offset, byte) in text.bytes().enumerate() {
            if offset % 4096 == 0 {
                control.check()?;
            }
            if byte == b'\n' {
                lines.push(offset + 1);
            }
        }
        Ok(Self {
            text,
            lines,
            columns: HashMap::new(),
        })
    }

    fn position(
        &mut self,
        value: &Value,
        control: &RenderControl,
    ) -> RenderResult<Option<(usize, usize)>> {
        let Some(line) = value["line"].as_u64().and_then(|n| usize::try_from(n).ok()) else {
            return Err(RenderError::Protocol);
        };
        let Some(offset) = value["character"]
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
        else {
            return Err(RenderError::Protocol);
        };
        let Some(&start) = self.lines.get(line) else {
            return Ok(None);
        };
        if !self.columns.contains_key(&line) {
            let end = self
                .lines
                .get(line + 1)
                .map_or(self.text.len(), |next| next - 1);
            let text = &self.text[start..end];
            let text = text.strip_suffix('\r').unwrap_or(text);
            let columns = if text.is_ascii() {
                Columns::Ascii(text.len())
            } else {
                let mut boundaries = vec![0];
                let mut utf16 = 0;
                for (index, character) in text.chars().enumerate() {
                    if index % 1024 == 0 {
                        control.check()?;
                    }
                    utf16 += character.len_utf16();
                    boundaries.push(utf16);
                }
                Columns::Utf16(boundaries)
            };
            self.columns.insert(line, columns);
        }
        let column = match &self.columns[&line] {
            Columns::Ascii(length) => (offset <= *length).then(|| offset + 1),
            Columns::Utf16(boundaries) => boundaries
                .binary_search(&offset)
                .ok()
                .map(|index| index + 1),
        };
        Ok(column.map(|column| (line + 1, column)))
    }
}

struct Rows {
    matches: usize,
    omitted: usize,
    bytes: usize,
    rows: Vec<String>,
    input_read: usize,
    output_full: bool,
    sources: HashMap<PathBuf, Option<Source>>,
}
impl Rows {
    fn source<'a>(
        &'a mut self,
        root: &Path,
        path: &Path,
        control: &RenderControl,
    ) -> RenderResult<Option<&'a mut Source>> {
        control.check()?;
        if !self.sources.contains_key(path) {
            let remaining = MAX_INPUT.saturating_sub(self.input_read);
            let (text, read) = super::inventory::read_for_analysis(
                root,
                path,
                remaining,
                &control.stopped,
                control.deadline,
            );
            self.input_read += read;
            control.check()?;
            let source = match text {
                Ok(text) => Some(Source::new(text, control)?),
                Err(_) => None,
            };
            self.sources.insert(path.to_path_buf(), source);
        }
        Ok(self.sources.get_mut(path).and_then(Option::as_mut))
    }
    fn full(&self, limit: usize) -> bool {
        self.rows.len() >= limit || self.output_full || self.input_read >= MAX_INPUT
    }
    fn add(&mut self, row: Option<String>, limit: usize) {
        if let Some(row) = row {
            if self.rows.len() < limit && self.bytes + row.len() < MAX_OUTPUT {
                self.bytes += row.len() + 1;
                self.rows.push(row);
                return;
            }
            self.output_full = true;
        }
        self.omitted += 1;
    }
}

fn render(
    root: &Path,
    path: &Path,
    input: String,
    read: usize,
    args: Args,
    response: Value,
    control: &RenderControl,
) -> RenderResult<ExecOut> {
    control.check()?;
    let result = &response["result"];
    let items: &[Value] = if matches!(args.action, Action::Diagnostics) {
        if result["kind"] != "full" || result.get("resultId").is_some_and(|id| !id.is_string()) {
            return Err(RenderError::Protocol);
        }
        result["items"].as_array().ok_or(RenderError::Protocol)?
    } else if result.is_null() {
        &[]
    } else if let Some(items) = result.as_array() {
        items
    } else if matches!(args.action, Action::Definition) && result.is_object() {
        std::slice::from_ref(result)
    } else {
        return Err(RenderError::Protocol);
    };
    let mut rows = Rows {
        matches: items.len(),
        omitted: 0,
        bytes: 0,
        rows: Vec::new(),
        input_read: read,
        output_full: false,
        sources: HashMap::from([(path.to_path_buf(), Some(Source::new(input, control)?))]),
    };
    let mut redact = crate::export::Redactor::new(Vec::new());
    for item in items.iter().take(MAX_PROCESSED) {
        control.check()?;
        validate_item(args.action, item)?;
        if rows.full(args.limit) {
            rows.omitted += 1;
            continue;
        }
        let row = if matches!(args.action, Action::Diagnostics) {
            location(root, path, &item["range"], &mut rows, control)?.map(|location| {
                let severity = match item["severity"].as_u64() {
                    Some(1) => "error",
                    Some(2) => "warning",
                    Some(3) => "information",
                    Some(4) => "hint",
                    _ => "unknown",
                };
                let message = crate::provider::truncate(
                    &redact.text(item["message"].as_str().expect("validated message")),
                    1024,
                );
                format!("{location} {severity} {}", escaped(&message))
            })
        } else {
            let uri = item["targetUri"].as_str().or_else(|| item["uri"].as_str());
            let range = if item.get("targetUri").is_some() {
                &item["targetSelectionRange"]
            } else {
                &item["range"]
            };
            let target = uri
                .and_then(|uri| reqwest::Url::parse(uri).ok())
                .filter(|uri| uri.query().is_none() && uri.fragment().is_none())
                .and_then(|uri| uri.to_file_path().ok())
                .and_then(|target| super::fs::resolve(root, target.to_str()?).ok());
            match target {
                Some(target) => location(root, &target, range, &mut rows, control)?,
                None => None,
            }
        };
        rows.add(row, args.limit);
    }
    rows.omitted += items.len().saturating_sub(MAX_PROCESSED);
    control.check()?;
    let health = match response["analysis_health"].as_str() {
        Some("ok") => "ok",
        Some("warning") => "warning",
        Some("error") => "error",
        _ => "unknown",
    };
    let ready = health == "ok" && response["quiescent"].as_bool() == Some(true);
    let mode = match response["project_mode"].as_str() {
        Some("cargo") => "cargo",
        Some("detached") => "detached",
        _ => "unknown",
    };
    let file_in_project = response["file_in_project"].as_bool();
    let membership = match file_in_project {
        Some(true) => "true",
        Some(false) => "false",
        None => "unknown",
    };
    let complete = ready && rows.omitted == 0 && mode == "cargo" && file_in_project == Some(true);
    let truncated = !complete;
    let mut text=format!("status: success\naction: {}\npath: {}\nproject_mode: {mode}\nfile_in_project: {membership}\nanalysis_health: {health}\nanalysis_complete: {complete}\nmatches_seen: {}\nshowing: {}\nresults_omitted: {}\ntruncated: {truncated}\nlimitations: build scripts and proc macros disabled; diagnostics are not compiler/test proof\ncontent:\n",args.action.name(),escaped(&args.path),rows.matches,rows.rows.len(),rows.omitted);
    if rows.rows.is_empty() {
        text.push_str("<empty>\n");
    } else {
        for row in rows.rows {
            let _ = writeln!(text, "{row}");
        }
    }
    if truncated {
        text.push_str("hint: analysis unavailable, loading or results omitted; empty results do not prove absence\n");
    }
    if mode == "detached" {
        text.push_str("hint: standalone file analysis; other project files may be outside the semantic graph\n");
    }
    if mode == "cargo" && file_in_project != Some(true) {
        text.push_str("hint: queried file's Cargo target membership is absent or unknown; semantic coverage is partial\n");
    }
    let mut out = ExecOut::plain(text, ExecKind::Success);
    out.truncated = truncated;
    Ok(out)
}

fn location(
    root: &Path,
    path: &Path,
    range: &Value,
    rows: &mut Rows,
    control: &RenderControl,
) -> RenderResult<Option<String>> {
    control.check()?;
    if !allowed(root, path) {
        return Ok(None);
    }
    let Some(source) = rows.source(root, path, control)? else {
        return Ok(None);
    };
    let Some((line, column)) = source.position(&range["start"], control)? else {
        return Ok(None);
    };
    let Some((end_line, end_column)) = source.position(&range["end"], control)? else {
        return Ok(None);
    };
    if (end_line, end_column) < (line, column) {
        return Ok(None);
    }
    let Some(relative) = path
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.to_str())
    else {
        return Ok(None);
    };
    Ok(Some(format!(
        "{}:{line}:{column}-{end_line}:{end_column}",
        escaped(relative)
    )))
}

fn range_bounds(value: &Value) -> RenderResult<((u64, u64), (u64, u64))> {
    let position = |point: &Value| -> RenderResult<(u64, u64)> {
        Ok((
            point["line"].as_u64().ok_or(RenderError::Protocol)?,
            point["character"].as_u64().ok_or(RenderError::Protocol)?,
        ))
    };
    let start = position(&value["start"])?;
    let end = position(&value["end"])?;
    if start > end {
        return Err(RenderError::Protocol);
    }
    Ok((start, end))
}

fn validate_item(action: Action, item: &Value) -> RenderResult<()> {
    if !item.is_object() {
        return Err(RenderError::Protocol);
    }
    if matches!(action, Action::Diagnostics) {
        range_bounds(&item["range"])?;
        if !item["message"].is_string()
            || item
                .get("severity")
                .is_some_and(|value| !matches!(value.as_u64(), Some(1..=4)))
        {
            return Err(RenderError::Protocol);
        }
    } else if item.get("targetUri").is_some() {
        if !matches!(action, Action::Definition)
            || item["targetUri"].as_str().is_none_or(str::is_empty)
        {
            return Err(RenderError::Protocol);
        }
        let (target_start, target_end) = range_bounds(&item["targetRange"])?;
        let (selection_start, selection_end) = range_bounds(&item["targetSelectionRange"])?;
        if selection_start < target_start || selection_end > target_end {
            return Err(RenderError::Protocol);
        }
        if let Some(origin) = item.get("originSelectionRange") {
            range_bounds(origin)?;
        }
    } else {
        if item["uri"].as_str().is_none_or(str::is_empty) {
            return Err(RenderError::Protocol);
        }
        range_bounds(&item["range"])?;
    }
    Ok(())
}

fn escaped(text: &str) -> String {
    if text.chars().any(char::is_control) {
        serde_json::to_string(text).expect("string serialization")
    } else {
        text.to_string()
    }
}
