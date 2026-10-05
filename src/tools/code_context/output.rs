use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use super::{Action, Args, Candidate, ExecKind, ExecOut, Region, Stats};

const HINT: &str = "hint: code_context read path+line expands; narrow search when partial.\n";
const GAP: &str = "... source lines omitted ...\n";

fn escaped(text: &str) -> String {
    if text.chars().any(char::is_control) {
        serde_json::to_string(text).expect("string serialization")
    } else {
        text.into()
    }
}

fn header(
    args: &Args,
    stats: &Stats,
    selected: usize,
    omitted: usize,
    omitted_lines: usize,
    truncated: bool,
    timed_out: bool,
) -> String {
    let scan = &stats.scan;
    let scan_complete =
        scan.stopped.is_none() && scan.skipped == 0 && scan.ignore_errors == 0 && !timed_out;
    let parse_complete = scan_complete && scan.unsupported == 0 && scan.syntax_errors == 0;
    format!(
        "status: success\naction: {}\nranking: lexical\nscan_complete: {scan_complete}\nparse_complete: {parse_complete}\nentries_scanned: {}\nfiles_scanned: {}\nfiles_read: {}\nfiles_parsed: {}\nbytes_read: {}\nfiles_skipped: {}\nfiles_unsupported: {}\nsyntax_error_files: {}\nignore_rule_errors: {}\nlocal_parse_cache_hits: {}\nlocal_parse_cache_misses: {}\nmatches_seen: {}\nselected: {selected}\nomitted: {omitted}\nomitted_source_lines: {omitted_lines}\ntruncated: {truncated}\nstop_reason: {}\ncontent:\n",
        args.action.name(), scan.entries, scan.files, stats.read, scan.parsed, scan.bytes, scan.skipped,
        scan.unsupported, scan.syntax_errors, scan.ignore_errors, stats.hits, stats.misses, stats.matches,
        if timed_out { "time_limit" } else { scan.stopped.unwrap_or("none") }
    )
}

fn clipped(region: Region, anchor: usize, limit: usize) -> Region {
    if region.end.saturating_sub(region.start) < limit {
        return region;
    }
    let start = anchor
        .saturating_sub(8)
        .max(region.start)
        .min(region.end.saturating_sub(limit - 1));
    Region {
        start,
        end: start + limit - 1,
    }
}

fn union_lines(mut regions: Vec<Region>) -> usize {
    regions.sort_by_key(|r| (r.start, r.end));
    let mut count = 0usize;
    let mut current: Option<Region> = None;
    for region in regions {
        if let Some(previous) = &mut current {
            if region.start <= previous.end.saturating_add(1) {
                previous.end = previous.end.max(region.end);
            } else {
                count += previous.end - previous.start + 1;
                *previous = region;
            }
        } else {
            current = Some(region);
        }
    }
    count + current.map_or(0, |r| r.end - r.start + 1)
}

fn record(
    candidate: &Candidate,
    read: bool,
    budget: usize,
    cancel: &AtomicBool,
    deadline: Instant,
) -> Option<(String, usize, bool)> {
    if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return None;
    }
    let main = clipped(
        candidate.region,
        candidate.anchor,
        if read { 160 } else { 48 },
    );
    let mut preferred = vec![main];
    let mut full = vec![candidate.region];
    let mut imports = Vec::new();
    let definition = candidate.definition.and_then(|index| {
        candidate
            .facts
            .as_ref()
            .map(|facts| &facts.definitions[index])
    });
    if let Some(definition) = definition {
        // Add the actual definition's beginning even when its anchored body is
        // far away, then preceding source comments/attributes (not semantics).
        preferred.push(Region {
            start: definition.body.start,
            end: (definition.body.start + 7).min(definition.body.end),
        });
        if definition.attached_start < definition.body.start {
            preferred.push(Region {
                start: definition.attached_start,
                end: (definition.attached_start + 23).min(definition.body.start - 1),
            });
        }
        if let Some(parent) = definition.parent {
            full.push(parent);
            preferred.push(Region {
                start: parent.start,
                end: (parent.start + 7).min(parent.end),
            });
        }
    }
    if let Some(facts) = &candidate.facts {
        imports = facts
            .imports
            .iter()
            .filter(|import| {
                import.scope.start <= candidate.anchor && candidate.anchor <= import.scope.end
            })
            .collect::<Vec<_>>();
        imports.sort_by_key(|import| (import.scope.end - import.scope.start, import.region.start));
        full.extend(imports.iter().map(|import| import.region));
        let mut remaining = 20;
        for import in imports.iter().take(4) {
            if remaining == 0 {
                break;
            }
            let region = Region {
                start: import.region.start,
                end: import.region.end.min(import.region.start + remaining - 1),
            };
            remaining -= region.end - region.start + 1;
            preferred.push(region);
        }
    }
    let total_lines = union_lines(full);
    let symbol = definition.map_or_else(
        || "plain_text_window".into(),
        |d| format!("{} {}", d.kind, escaped(&d.name)),
    );
    let prefix = format!(
        "\npath: {}\nsource_sha256: {}\nselection_reason: {}\nsymbol: {symbol}\nregion: {}-{}\nanchor_line: {}\n",
        escaped(&candidate.source.path), candidate.source.hash, candidate.reasons.join(", "), candidate.region.start, candidate.region.end, candidate.anchor
    );
    let reserved_suffix = format!(
        "showing: {}-{}\nanchor_shown: true\nexcerpt_complete: false\nomitted_source_lines: {total_lines}\nscoped_imports_seen: {}\nscoped_imports_omitted: {}\nsource:\n",
        candidate.source.lines(), candidate.source.lines(), imports.len(), imports.len()
    );
    let fixed = prefix.len() + reserved_suffix.len();
    if fixed >= budget {
        return None;
    }
    let mut chosen = BTreeSet::new();
    let mut source_bytes = 0;
    // A selected record always includes its advertised anchor. If one exact
    // source line cannot fit, omit the record rather than clip or imply it was
    // returned. Other source/context lines are chosen only after this anchor.
    let anchor_bytes =
        candidate.source.line(candidate.anchor).len() + candidate.anchor.to_string().len() + 4;
    if fixed + anchor_bytes > budget {
        return None;
    }
    chosen.insert(candidate.anchor);
    source_bytes += anchor_bytes;
    for region in preferred {
        for line in region.start..=region.end.min(candidate.source.lines()) {
            if cancel.load(Ordering::Relaxed) || Instant::now() >= deadline {
                return None;
            }
            if chosen.contains(&line) {
                continue;
            }
            let bytes = candidate.source.line(line).len() + line.to_string().len() + 4;
            chosen.insert(line);
            let mut previous = None;
            let gaps = chosen
                .iter()
                .filter(|&&line| {
                    let gap = previous.is_some_and(|previous: usize| previous + 1 != line);
                    previous = Some(line);
                    gap
                })
                .count();
            if fixed + source_bytes + bytes + gaps * GAP.len() <= budget {
                source_bytes += bytes;
            } else {
                chosen.remove(&line);
            }
        }
    }
    if chosen.is_empty() {
        return None;
    }
    let omitted_lines = total_lines.saturating_sub(chosen.len());
    let omitted_imports = imports
        .iter()
        .filter(|import| {
            // Test bounded chosen lines rather than iterate a potentially enormous
            // import region. Full import source is present only when counts agree.
            chosen
                .range(import.region.start..=import.region.end)
                .count()
                != import.region.end - import.region.start + 1
        })
        .count();
    let mut text = prefix;
    let _ = writeln!(
        text,
        "showing: {}-{}",
        chosen.first().expect("chosen line"),
        chosen.last().expect("chosen line")
    );
    let _ = writeln!(text, "anchor_shown: true\nexcerpt_complete: {}\nomitted_source_lines: {omitted_lines}\nscoped_imports_seen: {}\nscoped_imports_omitted: {omitted_imports}\nsource:", omitted_lines == 0, imports.len());
    let mut previous = None;
    for line in chosen {
        if previous.is_some_and(|previous: usize| previous + 1 != line) {
            text.push_str(GAP);
        }
        let _ = writeln!(text, "{line} | {}", candidate.source.line(line));
        previous = Some(line);
    }
    (text.len() <= budget).then_some((text, omitted_lines, omitted_lines != 0))
}

pub(super) fn render(
    args: &Args,
    candidates: Vec<Candidate>,
    stats: &Stats,
    cancel: &AtomicBool,
    deadline: Instant,
) -> ExecOut {
    if cancel.load(Ordering::Relaxed) {
        return super::cancelled();
    }
    let provisional = header(args, stats, 0, stats.matches, 0, true, false);
    // Whole-envelope cap: metadata, hint, empty marker and changed counter
    // widths are reserved before any exact source line is included.
    let mut remaining = args
        .max_bytes
        .saturating_sub(provisional.len() + HINT.len() + 32);
    let mut content = String::new();
    let mut selected = 0;
    let mut omitted_lines = 0;
    let mut partial_excerpts = false;
    let mut timed_out = false;
    let candidate_count = candidates.len();
    for (index, candidate) in candidates.into_iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return super::cancelled();
        }
        if Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        let share = (remaining / (candidate_count - index))
            .max(512)
            .min(remaining);
        if let Some((text, omitted, partial)) = record(
            &candidate,
            matches!(args.action, Action::Read),
            share,
            cancel,
            deadline,
        ) {
            remaining -= text.len();
            content.push_str(&text);
            selected += 1;
            omitted_lines += omitted;
            partial_excerpts |= partial;
        }
    }
    timed_out |= Instant::now() >= deadline;
    let omitted = stats.matches.saturating_sub(selected);
    let mut truncated = omitted > 0
        || partial_excerpts
        || timed_out
        || stats.scan.stopped.is_some()
        || stats.scan.skipped > 0
        || stats.scan.syntax_errors > 0
        || stats.scan.unsupported > 0;
    let mut text = header(
        args,
        stats,
        selected,
        omitted,
        omitted_lines,
        truncated,
        timed_out,
    );
    if content.is_empty() {
        text.push_str("<empty>\n");
    } else {
        text.push_str(&content);
    }
    text.push_str(HINT);
    // All components are bounded above before assembly. This final fallback
    // protects future header changes without emitting clipped source lines.
    if text.len() > args.max_bytes {
        truncated = true;
        text = header(args, stats, 0, stats.matches, 0, true, timed_out);
        text.push_str("<empty>\n");
        text.push_str(HINT);
    }
    debug_assert!(text.len() <= args.max_bytes);
    let mut out = ExecOut::plain(text, ExecKind::Success);
    out.truncated = truncated || selected == 0 && stats.matches > 0;
    out
}
