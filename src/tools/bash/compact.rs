//! Conservative model-facing rendering of completed, untruncated Cargo output.
//! Execution, capture, exit status and raw recovery belong to the caller.

use std::path::Path;

pub(crate) struct Reduction {
    pub stdout: String,
    pub stderr: String,
    pub strategy: &'static str,
    /// Passing test records actually replaced by explicit omission markers.
    pub passed_lines: usize,
    /// Cargo progress records actually replaced by explicit omission markers.
    pub progress_lines: usize,
    pub stat_graphs_removed: usize,
}

/// Recognize a literal Cargo invocation and remove only validated routine output.
/// Unknown commands and renderings without a byte reduction return `None`.
pub(crate) fn reduce(command: &str, stdout: &str, stderr: &str) -> Option<Reduction> {
    if !simple_cargo_command(command) {
        return None;
    }
    // A malformed or incomplete suite keeps the entire stdout unchanged. The
    // independent stderr pass can still recognize ordinary Cargo progress.
    let (stdout, passed_lines) = compact_tests(stdout).unwrap_or_else(|| (stdout.to_owned(), 0));
    let (stderr, progress_lines) = compact_progress(stderr);
    if passed_lines == 0 && progress_lines == 0 {
        return None;
    }
    Some(Reduction {
        stdout,
        stderr,
        strategy: "cargo-success",
        passed_lines,
        progress_lines,
        stat_graphs_removed: 0,
    })
}

fn simple_cargo_command(command: &str) -> bool {
    // These characters introduce shell interpretation rather than literal
    // whitespace-separated arguments. Globs/comments/escapes also fail closed.
    if command.chars().any(|c| {
        (c.is_control() && c != '\t')
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
    if executable != "cargo"
        && !(Path::new(executable).is_absolute()
            && Path::new(executable).file_name().and_then(|s| s.to_str()) == Some("cargo"))
    {
        return false;
    }
    if !matches!(words.next(), Some("test" | "build" | "check" | "clippy")) {
        return false;
    }
    // Machine-oriented, listing and captured-output modes are not this grammar.
    // Reject both `--flag=value` and `--flag value`, including after `--`.
    !words.any(|word| {
        matches!(
            word.split('=').next().unwrap_or(word),
            "--message-format"
                | "--format"
                | "--list"
                | "--show-output"
                | "--nocapture"
                | "--no-capture"
                | "--build-plan"
                | "--unit-graph"
        )
    })
}

fn body(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

fn running(line: &str) -> Option<usize> {
    let rest = body(line).strip_prefix("running ")?;
    let (count, noun) = rest.split_once(' ')?;
    let count = number(count)?;
    if noun != if count == 1 { "test" } else { "tests" } {
        return None;
    }
    Some(count)
}

fn number(value: &str) -> Option<usize> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    value.parse().ok()
}

fn duration(value: &str) -> bool {
    let Some(value) = value.strip_suffix('s') else {
        return false;
    };
    let mut parts = value.split('.');
    let Some(whole) = parts.next() else {
        return false;
    };
    if whole.is_empty() || !whole.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if let Some(fraction) = parts.next() {
        if fraction.is_empty() || !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    parts.next().is_none()
}

fn summary(line: &str) -> Option<(usize, usize)> {
    let mut fields = body(line).strip_prefix("test result: ok. ")?.split("; ");
    let passed = number(fields.next()?.strip_suffix(" passed")?)?;
    let failed = number(fields.next()?.strip_suffix(" failed")?)?;
    let ignored = number(fields.next()?.strip_suffix(" ignored")?)?;
    let measured = number(fields.next()?.strip_suffix(" measured")?)?;
    let _filtered = number(fields.next()?.strip_suffix(" filtered out")?)?;
    if failed != 0 || measured != 0 || !duration(fields.next()?.strip_prefix("finished in ")?) {
        return None;
    }
    // Newer doctest runners may append their aggregate runtime. Its contents
    // remain intact; only this known suffix is accepted.
    if let Some(extra) = fields.next() {
        if !duration(extra.strip_prefix("all doctests ran in ")?) {
            return None;
        }
    }
    if fields.next().is_some() {
        return None;
    }
    Some((passed, ignored))
}

#[derive(Clone, Copy)]
enum Record {
    Passing,
    Ignored,
    Uncertain,
    Unknown,
}

fn record(line: &str) -> Record {
    let Some(rest) = body(line).strip_prefix("test ") else {
        return Record::Unknown;
    };
    let Some((name, result)) = rest.rsplit_once(" ... ") else {
        return Record::Unknown;
    };
    if name.is_empty() || name.chars().any(char::is_control) {
        return Record::Uncertain;
    }
    match result {
        "ok" => Record::Passing,
        "ignored" => Record::Ignored,
        s if s.starts_with("ignored, ") && s.len() > "ignored, ".len() => Record::Ignored,
        _ => Record::Uncertain,
    }
}

fn compact_tests(stdout: &str) -> Option<(String, usize)> {
    let lines: Vec<_> = stdout.split_inclusive('\n').collect();
    let mut passing = vec![false; lines.len()];
    let mut suite: Option<(usize, usize, usize)> = None;
    for (index, line) in lines.iter().enumerate() {
        if body(line).starts_with("running ")
            && (body(line).ends_with(" test") || body(line).ends_with(" tests"))
            && running(line).is_none()
        {
            return None;
        }
        if let Some(count) = running(line) {
            if suite.is_some() {
                return None;
            }
            suite = Some((count, 0, 0));
            continue;
        }
        if body(line).starts_with("test result:") {
            let (expected, passed, ignored) = suite.take()?;
            let (reported_passed, reported_ignored) = summary(line)?;
            if reported_passed != passed
                || reported_ignored != ignored
                || passed.checked_add(ignored)? != expected
            {
                return None;
            }
            continue;
        }
        if let Some((_, passed, ignored)) = suite.as_mut() {
            match record(line) {
                Record::Passing => {
                    *passed += 1;
                    passing[index] = true;
                }
                Record::Ignored => *ignored += 1,
                Record::Uncertain => return None,
                Record::Unknown => {}
            }
        }
    }
    if suite.is_some() {
        return None;
    }
    Some(collapse(&lines, &passing, "passing test"))
}

fn version(value: &str) -> bool {
    let Some(value) = value.strip_prefix('v') else {
        return false;
    };
    let boundary = value.find(['-', '+']).unwrap_or(value.len());
    let mut pieces = value[..boundary].split('.');
    if !(0..3).all(|_| pieces.next().is_some_and(|p| number(p).is_some()))
        || pieces.next().is_some()
    {
        return false;
    }
    let suffix = &value[boundary..];
    suffix.is_empty()
        || (suffix.len() > 1
            && suffix
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'+' | b'.')))
}

fn progress(line: &str) -> bool {
    let Some(rest) = body(line)
        .strip_prefix("   Compiling ")
        .or_else(|| body(line).strip_prefix("    Checking "))
    else {
        return false;
    };
    let Some((name, rest)) = rest.split_once(' ') else {
        return false;
    };
    if name.is_empty()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
    {
        return false;
    }
    let (value, trailing) = rest.split_once(' ').unwrap_or((rest, ""));
    if !version(value) {
        return false;
    }
    trailing.is_empty()
        || (trailing.starts_with('(')
            && trailing.ends_with(')')
            && !trailing.chars().any(char::is_control))
}

fn compact_progress(stderr: &str) -> (String, usize) {
    let lines: Vec<_> = stderr.split_inclusive('\n').collect();
    let selected: Vec<_> = lines.iter().map(|line| progress(line)).collect();
    collapse(&lines, &selected, "Cargo progress")
}

/// Replace only consecutive eligible records, preserving every other byte.
/// A short run stays verbatim when its explicit marker would be larger.
fn collapse(lines: &[&str], selected: &[bool], label: &str) -> (String, usize) {
    let mut rendered = String::new();
    let mut collapsed = 0;
    let mut index = 0;
    while index < lines.len() {
        if !selected[index] {
            rendered.push_str(lines[index]);
            index += 1;
            continue;
        }
        let start = index;
        let mut bytes = 0;
        while index < lines.len() && selected[index] {
            bytes += lines[index].len();
            index += 1;
        }
        let count = index - start;
        let end = if lines[index - 1].ends_with("\r\n") {
            "\r\n"
        } else if lines[index - 1].ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let marker = format!("... {count} {label} records collapsed ...{end}");
        if marker.len() < bytes {
            rendered.push_str(&marker);
            collapsed += count;
        } else {
            for line in &lines[start..index] {
                rendered.push_str(line);
            }
        }
    }
    (rendered, collapsed)
}

#[cfg(test)]
mod tests {
    use super::reduce;

    fn progress_records() -> String {
        (0..20)
            .map(|index| format!("    Checking crate_{index} v1.2.3\n"))
            .collect()
    }

    #[test]
    fn cargo_progress_preserves_unindented_lookalike_diagnostics() {
        let unknown: String = (0..20)
            .map(|index| {
                format!("Checking diagnostic_{index} v1.2.3 (missing required observation)\n")
            })
            .collect();
        assert!(reduce("cargo check", "", &unknown).is_none());

        let stderr = format!(
            "{}{unknown}warning: keep this warning exactly\n",
            progress_records()
        );
        let reduced = reduce("cargo check", "opaque stdout\n", &stderr).unwrap();
        assert_eq!(reduced.stdout, "opaque stdout\n");
        assert_eq!(reduced.strategy, "cargo-success");
        assert_eq!(reduced.stat_graphs_removed, 0);
        assert_eq!(reduced.progress_lines, 20);
        assert_eq!(reduced.passed_lines, 0);
        assert!(reduced
            .stderr
            .ends_with(&format!("{unknown}warning: keep this warning exactly\n")));
    }

    #[test]
    fn malformed_later_suite_preserves_all_stdout() {
        let stdout = concat!(
            "running 3 tests\n",
            "test first::alpha ... ok\n",
            "test first::beta ... ok\n",
            "test first::gamma ... ok\n",
            "test result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
            "\nrunning 2 tests\n",
            "test second::alpha ... ok\n",
            "test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n",
        );
        let reduced = reduce("cargo test", stdout, &progress_records()).unwrap();
        assert_eq!(reduced.stdout, stdout);
        assert_eq!(reduced.passed_lines, 0);
        assert_eq!(reduced.progress_lines, 20);
    }
}
