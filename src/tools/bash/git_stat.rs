//! Remove only the histogram suffixes of a complete, validated Git diffstat.
//! The caller owns successful capture, exact raw recovery and envelope sizing.

use std::path::Path;

pub(super) struct StatReduction {
    pub stdout: String,
    /// File rows whose nonempty histogram suffix was removed.
    pub graphs_removed: usize,
}

pub(super) fn reduce(command: &str, stdout: &str) -> Option<StatReduction> {
    if stdout.len() > super::STREAM_CAP || !simple_command(command) {
        return None;
    }
    let mut lines = stdout.split_inclusive('\n').peekable();
    let mut rendered = String::with_capacity(stdout.len());
    let mut rows = 0usize;
    let mut changes = 0usize;
    let mut graphs_removed = 0usize;
    let mut footer_seen = false;
    while let Some(line) = lines.next() {
        let content = body(line);
        if content.chars().any(char::is_control) {
            return None;
        }
        if lines.peek().is_none() {
            let (reported_rows, reported_changes) = footer(content)?;
            if reported_rows != rows || reported_changes != changes {
                return None;
            }
            rendered.push_str(line);
            footer_seen = true;
            break;
        }
        let (count, graph_start) = row(content)?;
        rows = rows.checked_add(1)?;
        changes = changes.checked_add(count)?;
        if let Some(start) = graph_start {
            // Everything before the graph, including numeric padding and the
            // graph's separating spaces, stays exact. Preserve CRLF/LF too.
            rendered.push_str(&line[..start]);
            rendered.push_str(&line[content.len()..]);
            graphs_removed += 1;
        } else {
            rendered.push_str(line);
        }
    }
    if !footer_seen || graphs_removed == 0 || rendered.len() >= stdout.len() {
        return None;
    }
    Some(StatReduction {
        stdout: rendered,
        graphs_removed,
    })
}

fn simple_command(command: &str) -> bool {
    if command.chars().any(|c| {
        c.is_control()
            || matches!(
                c,
                '|' | '&'
                    | ';'
                    | '<'
                    | '>'
                    | '\''
                    | '"'
                    | '$'
                    | '`'
                    | '\\'
                    | '('
                    | ')'
                    | '{'
                    | '}'
                    | '['
                    | ']'
                    | '*'
                    | '?'
                    | '#'
                    | '~'
                    | '!'
            )
    }) {
        return false;
    }
    let mut words = command.split_ascii_whitespace();
    let Some(executable) = words.next() else {
        return false;
    };
    if executable != "git"
        && !(Path::new(executable).is_absolute()
            && Path::new(executable).file_name().and_then(|s| s.to_str()) == Some("git"))
    {
        return false;
    }
    if words.next() != Some("diff") {
        return false;
    }
    let mut stat = false;
    let mut staged = false;
    let mut no_color = false;
    let mut no_ext_diff = false;
    let mut pathspecs = false;
    for word in words {
        if pathspecs {
            continue;
        }
        match word {
            "--stat" if !stat => stat = true,
            "--cached" | "--staged" if !staged => staged = true,
            "--no-color" if !no_color => no_color = true,
            "--no-ext-diff" if !no_ext_diff => no_ext_diff = true,
            "--" => pathspecs = true,
            _ => return false,
        }
    }
    stat
}

fn body(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn number(value: &str) -> Option<usize> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

/// One displayed file row; binary byte counts do not contribute line changes.
fn row(line: &str) -> Option<(usize, Option<usize>)> {
    let (name, value) = line.rsplit_once(" | ")?;
    if name.strip_prefix(' ')?.trim().is_empty() {
        return None;
    }
    let value = value.trim_start_matches(' ');
    if let Some(binary) = value.strip_prefix("Bin ") {
        let (before, after) = binary.split_once(" -> ")?;
        number(before)?;
        number(after.strip_suffix(" bytes")?)?;
        return Some((0, None));
    }
    let digits = value.bytes().take_while(u8::is_ascii_digit).count();
    let count = number(&value[..digits])?;
    let suffix = &value[digits..];
    if suffix.is_empty() {
        return Some((count, None));
    }
    if !suffix.starts_with(' ') {
        return None;
    }
    let graph = suffix.trim_start_matches(' ');
    if graph.is_empty() || graph.len() > count {
        return None;
    }
    let mut minus_seen = false;
    for byte in graph.bytes() {
        match byte {
            b'+' if !minus_seen => {}
            b'-' => minus_seen = true,
            _ => return None,
        }
    }
    Some((count, Some(line.len() - graph.len())))
}

fn counted(value: &str, singular: &str, plural: &str) -> Option<usize> {
    if let Some(digits) = value.strip_suffix(singular) {
        let count = number(digits)?;
        return (count == 1).then_some(count);
    }
    let count = number(value.strip_suffix(plural)?)?;
    (count != 1).then_some(count)
}

fn footer(line: &str) -> Option<(usize, usize)> {
    let mut fields = line.strip_prefix(' ')?.split(", ");
    let rows = counted(fields.next()?, " file changed", " files changed")?;
    let first = fields.next()?;
    let (added, deleted) = if let Some(added) = counted(first, " insertion(+)", " insertions(+)") {
        let deleted = match fields.next() {
            Some(field) => counted(field, " deletion(-)", " deletions(-)")?,
            None => 0,
        };
        (added, deleted)
    } else {
        (0, counted(first, " deletion(-)", " deletions(-)")?)
    };
    if fields.next().is_some() {
        return None;
    }
    Some((rows, added.checked_add(deleted)?))
}
