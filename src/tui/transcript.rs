//! Pure transcript projection: activity groups → styled, display-width
//! wrapped, sanitized rows. Shared by the renderer (draw.rs) and App's
//! scroll-anchor math — one source of truth for "which row is which item".
//!
//! Everything here is a VIEW. Folding changes only which rows exist;
//! captured text on the items is never shortened or deleted.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use super::app::*;
use crate::events::ToolStatus;

/// One rendered row plus its owner for anchoring/selection.
/// owner = (group id, Some(item id) | None for group-level rows).
pub struct Row {
    pub owner: (u64, Option<u64>),
    pub line: Line<'static>,
}

// ── display defaults (UX, not capture limits) ─────────────────────────
/// Live reasoning preview while streaming.
const REASON_PREVIEW: usize = 4;
/// Live command output preview while running.
const LIVE_PREVIEW: usize = 6;
/// Diagnostic excerpt retained on a failed step.
const FAIL_EXCERPT: usize = 4;
/// Rows a single expanded text block may occupy (display bound).
const TEXT_CAP: usize = 200;
/// User-task header lines when folded.
const TASK_CAP: usize = 3;

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn acc() -> Style {
    Style::default().fg(Color::Cyan)
}

/// Strip terminal-control sequences from model/tool text: ANSI CSI/OSC
/// and charset selects first, then any remaining C0/C1 controls except
/// newline. What renders can never move the real cursor or clear the
/// screen — the TUI owns the terminal.
pub fn clean(s: &str) -> String {
    use std::sync::LazyLock;
    static ANSI: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            "\x1b\\[[0-9;?]*[a-zA-Z]|\x1b\\][^\x07\x1b]*(?:\x07|\x1b\\\\)|\x1b[()][0-9A-B]|\x1b[=>#]",
        )
        .unwrap()
    });
    let s = ANSI.replace_all(s, "");
    s.chars()
        .map(|c| match c {
            '\t' => "    ".to_string(),
            c if (c as u32) < 0x20 && c != '\n' => String::new(),
            c if (0x7f..=0x9f).contains(&(c as u32)) => String::new(),
            c => c.to_string(),
        })
        .collect()
}

/// Wrap `s` to display width — grapheme-cluster aware: a Thai vowel or
/// ZWJ-joined emoji never splits from its base, wide chars count 2,
/// combining marks count 0. Breaks happen on width, not byte count.
pub fn wrap(s: &str, w: usize) -> Vec<String> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let w = w.max(1);
    let mut out = Vec::new();
    for raw in s.split('\n') {
        let mut cur = String::new();
        let mut cw = 0usize;
        for g in raw.graphemes(true) {
            let gw = UnicodeWidthStr::width(g);
            if cw + gw > w && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
                cw = 0;
            }
            cur.push_str(g);
            cw += gw;
        }
        out.push(cur);
    }
    out
}

/// Whole transcript as rows. `width` = inner body width.
pub fn rows(app: &App, width: usize) -> Vec<Row> {
    let mut out: Vec<Row> = Vec::new();
    let fs = app.focusables();
    let sel: Option<(u64, Option<u64>)> = if app.nav {
        fs.get(app.nav_sel)
            .map(|&(gi, ii)| (app.groups[gi].id, ii.map(|i| app.groups[gi].items[i].id())))
    } else {
        None
    };

    for g in &app.groups {
        emit_group(app, g, width, sel, &mut out);
    }
    out
}

fn push(out: &mut Vec<Row>, owner: (u64, Option<u64>), spans: Vec<Span<'static>>) {
    out.push(Row {
        owner,
        line: Line::from(spans),
    });
}

fn emit_group(
    app: &App,
    g: &ActGroup,
    w: usize,
    sel: Option<(u64, Option<u64>)>,
    out: &mut Vec<Row>,
) {
    let width = w.max(20);
    let gid = g.id;
    let sel_style = |owner: (u64, Option<u64>)| -> Option<Style> {
        if sel == Some(owner) {
            Some(Style::default().bg(Color::DarkGray))
        } else {
            None
        }
    };
    let mark = |owner: (u64, Option<u64>)| -> &'static str {
        if sel == Some(owner) {
            "›"
        } else {
            " "
        }
    };

    // ── user header ─────────────────────────────────────────────────
    if !g.task.is_empty() {
        let owner = (gid, None);
        push(
            out,
            owner,
            vec![
                Span::styled(
                    format!("{}you", mark(owner)),
                    sel_style(owner).unwrap_or_else(|| {
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD)
                    }),
                ),
                Span::styled(format!("  {}", g.at), dim()),
            ],
        );
        let task_lines: Vec<String> = clean(&g.task)
            .split('\n')
            .flat_map(|l| wrap(l, width.saturating_sub(2)))
            .map(|s| s.to_string())
            .collect();
        let show = if g.folded() {
            TASK_CAP
        } else {
            task_lines.len().min(20)
        };
        for l in task_lines.iter().take(show) {
            push(
                out,
                owner,
                vec![Span::styled(
                    format!("  {l}"),
                    sel_style(owner).unwrap_or_default(),
                )],
            );
        }
        if task_lines.len() > show {
            push(
                out,
                owner,
                vec![Span::styled(
                    format!("  … {} more line(s)", task_lines.len() - show),
                    dim(),
                )],
            );
        }
    }

    if g.folded() {
        // compact summary — final answer stays visible below it
        let owner = (gid, None);
        push(
            out,
            owner,
            vec![Span::styled(
                format!("  ▸ {} — Enter/Space expands", g.summary()),
                sel_style(owner).unwrap_or_else(dim),
            )],
        );
        // failed steps keep their diagnostic excerpt visible even in the
        // collapsed run — a failure is never folded to just a count
        for (i, it) in g.items.iter().enumerate() {
            if let Act::Tool {
                status: Some(s), ..
            } = it
            {
                if matches!(
                    s,
                    ToolStatus::Failed | ToolStatus::Error | ToolStatus::Timeout
                ) {
                    emit_item(app, g, i, width, sel, out);
                }
            }
        }
        // the final answer stays visible — last assistant message
        if let Some(i) = g
            .items
            .iter()
            .rposition(|it| matches!(it, Act::Assistant { .. }))
        {
            emit_item(app, g, i, width, sel, out);
        }
        return;
    }

    // status row on finished groups = the collapse handle
    if g.done {
        let owner = (gid, None);
        let (glyph, sty) = if g.failed {
            ("✗", Style::default().fg(Color::Red))
        } else {
            ("✓", Style::default().fg(Color::Green))
        };
        push(
            out,
            owner,
            vec![Span::styled(
                format!("  {} {} — Enter/Space collapses", glyph, g.summary()),
                sel_style(owner).unwrap_or(sty),
            )],
        );
    }

    // items, with adjacent identical ok-tools grouped as ×N
    let mut i = 0;
    while i < g.items.len() {
        if let Act::Tool {
            agent,
            name,
            summary,
            status: Some(ToolStatus::Ok),
            expanded: false,
            ..
        } = &g.items[i]
        {
            let mut n = 1usize;
            while i + n < g.items.len() {
                match &g.items[i + n] {
                    Act::Tool {
                        agent: a,
                        name: nm,
                        summary: s,
                        status: Some(ToolStatus::Ok),
                        expanded: false,
                        ..
                    } if *a == *agent && *nm == *name && *s == *summary => n += 1,
                    _ => break,
                }
            }
            if n > 1 {
                let owner = (gid, Some(g.items[i].id()));
                let (a2, n2, s2, ms2) = match &g.items[i] {
                    Act::Tool {
                        agent,
                        name,
                        summary,
                        ms,
                        ..
                    } => (agent, name, summary, *ms),
                    _ => unreachable!(),
                };
                let who = if a2 == "solo" {
                    String::new()
                } else {
                    format!("{a2} ")
                };
                push(
                    out,
                    owner,
                    vec![Span::styled(
                        format!(
                            "  {}✓ {who}{n2} · exit 0 · {}ms — {}  ×{n}",
                            mark(owner),
                            ms2,
                            s2.lines().next().unwrap_or("")
                        ),
                        sel_style(owner).unwrap_or_else(|| Style::default().fg(Color::DarkGray)),
                    )],
                );
                i += n;
                continue;
            }
        }
        emit_item(app, g, i, width, sel, out);
        i += 1;
    }

    // honest waiting indicator: an in-flight request with no output yet
    // is real activity — show it instead of claiming reasoning exists
    if !g.done {
        if let Some(Act::Req {
            agent,
            done: false,
            had_output: false,
            ..
        }) = g.items.last()
        {
            let owner = (gid, Some(g.items.last().unwrap().id()));
            const SPIN: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
            let frame = (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
                / 100) as usize;
            push(
                out,
                owner,
                vec![Span::styled(
                    format!(
                        "  {}{} {agent} working…",
                        mark(owner),
                        SPIN[frame % SPIN.len()]
                    ),
                    sel_style(owner).unwrap_or_else(dim),
                )],
            );
        }
    }
}

fn emit_item(
    app: &App,
    g: &ActGroup,
    i: usize,
    width: usize,
    sel: Option<(u64, Option<u64>)>,
    out: &mut Vec<Row>,
) {
    let gid = g.id;
    let it = &g.items[i];
    let owner = (gid, Some(it.id()));
    let sel = sel == Some(owner);
    let sty = |s: Style| if sel { s.bg(Color::DarkGray) } else { s };
    let mark = if sel { "›" } else { " " };
    let body_w = width.saturating_sub(2);

    match it {
        Act::Req { .. } => {} // covered by the group's waiting indicator
        Act::Assistant {
            agent,
            text,
            done,
            at,
            ..
        } => {
            push(
                out,
                owner,
                vec![
                    Span::styled(
                        format!("{mark}{agent}"),
                        sty(acc().add_modifier(Modifier::BOLD)),
                    ),
                    Span::styled(format!("  {at}{}", if *done { "" } else { "  ⠋" }), dim()),
                ],
            );
            for l in clean(text)
                .split('\n')
                .flat_map(|l| wrap(l, body_w))
                .take(TEXT_CAP)
            {
                push(
                    out,
                    owner,
                    vec![Span::styled(format!("  {l}"), sty(Style::default()))],
                );
            }
        }
        Act::Reason {
            agent,
            text,
            done,
            expanded,
            at,
            ..
        } => {
            if app.reasoning == ReasonPref::Hidden {
                return;
            }
            let full = *expanded || app.reasoning == ReasonPref::Expanded;
            let label = if *done {
                format!(
                    "{mark}  ▸ reasoned · {} chars — Enter/Space expands",
                    text.chars().count()
                )
            } else {
                format!("{mark}  ⠋ reasoning — {agent}")
            };
            push(
                out,
                owner,
                vec![
                    Span::styled(label, sty(Style::default().fg(Color::Magenta))),
                    Span::styled(format!("  {at}"), dim()),
                ],
            );
            if full {
                for l in clean(text)
                    .split('\n')
                    .flat_map(|l| wrap(l, body_w))
                    .take(TEXT_CAP)
                {
                    push(
                        out,
                        owner,
                        vec![Span::styled(format!("    {l}"), sty(dim()))],
                    );
                }
            } else if !*done {
                // live tail preview only while streaming
                let tail: Vec<String> = clean(text)
                    .split('\n')
                    .flat_map(|l| wrap(l, body_w.saturating_sub(4)))
                    .map(|s| s.to_string())
                    .collect();
                for l in tail.iter().rev().take(REASON_PREVIEW).rev() {
                    push(
                        out,
                        owner,
                        vec![Span::styled(format!("    {l}"), sty(dim()))],
                    );
                }
            }
        }
        Act::Tool {
            agent,
            name,
            summary,
            status,
            exit,
            result,
            truncated,
            dropped,
            live,
            ms,
            expanded,
            at,
            ..
        } => {
            let head = summary.lines().next().unwrap_or("");
            let (glyph, label, gsty) = match status {
                None => ("⠋", "…", Style::default().fg(Color::Magenta)),
                Some(ToolStatus::Ok) => ("✓", "ok", Style::default().fg(Color::DarkGray)),
                Some(ToolStatus::Denied) => ("⊘", "denied", Style::default().fg(Color::Yellow)),
                Some(ToolStatus::Skipped) => ("·", "skipped", dim()),
                Some(ToolStatus::Intercepted) => ("·", "control plane", dim()),
                Some(_) => (
                    "✗",
                    status.unwrap().label(),
                    Style::default().fg(Color::Red),
                ),
            };
            let tail_info = match status {
                None => " · running".to_string(),
                Some(ToolStatus::Ok) => format!(" · exit {} · {}ms", exit.unwrap_or(0), ms),
                Some(ToolStatus::Skipped) | Some(ToolStatus::Intercepted) => format!(" · {label}"),
                Some(_) => format!(
                    " · {label}{} · {}ms",
                    exit.map(|e| format!(" exit {e}")).unwrap_or_default(),
                    ms
                ),
            };
            // attribution: solo's rows don't repeat the name; mission
            // workers' interleaved calls keep their agent id visible
            let who = if agent == "solo" {
                String::new()
            } else {
                format!("{agent} ")
            };
            push(
                out,
                owner,
                vec![
                    Span::styled(
                        format!("{mark}  {glyph} {who}{name}{tail_info} — {head}"),
                        sty(gsty),
                    ),
                    Span::styled(format!("  {at}"), dim()),
                ],
            );
            match status {
                None => {
                    // genuine live output — bounded tail preview
                    let tail: Vec<String> = clean(live)
                        .split('\n')
                        .flat_map(|l| wrap(l, body_w.saturating_sub(4)))
                        .map(|s| s.to_string())
                        .collect();
                    for l in tail.iter().rev().take(LIVE_PREVIEW).rev() {
                        push(
                            out,
                            owner,
                            vec![Span::styled(format!("      {l}"), sty(dim()))],
                        );
                    }
                }
                Some(s)
                    if !s.ok() && *s != ToolStatus::Skipped && *s != ToolStatus::Intercepted =>
                {
                    // failure keeps a diagnostic excerpt visible
                    let lines: Vec<String> = clean(result)
                        .split('\n')
                        .filter(|l| !l.starts_with("status:") && !l.is_empty())
                        .flat_map(|l| wrap(l, body_w.saturating_sub(4)))
                        .map(|s| s.to_string())
                        .collect();
                    let show = if *expanded { TEXT_CAP } else { FAIL_EXCERPT };
                    for l in lines.iter().rev().take(show).rev() {
                        push(
                            out,
                            owner,
                            vec![Span::styled(
                                format!("      {l}"),
                                sty(Style::default().fg(Color::Red)),
                            )],
                        );
                    }
                    if *expanded && lines.len() > TEXT_CAP {
                        push(
                            out,
                            owner,
                            vec![Span::styled(
                                "      … display capped — full captured result in the run export",
                                sty(dim()),
                            )],
                        );
                    }
                }
                Some(ToolStatus::Ok) if *expanded => {
                    for l in clean(result)
                        .split('\n')
                        .flat_map(|l| wrap(l, body_w.saturating_sub(4)))
                        .take(TEXT_CAP)
                    {
                        push(
                            out,
                            owner,
                            vec![Span::styled(format!("      {l}"), sty(dim()))],
                        );
                    }
                }
                _ => {}
            }
            if *truncated || *dropped > 0 {
                let mut note = String::from("      ");
                if *truncated {
                    note.push_str("captured output truncated at the capture cap");
                }
                if *dropped > 0 {
                    if *truncated {
                        note.push_str(" · ");
                    }
                    note.push_str(&format!(
                        "{dropped} preview chunks dropped (capture unaffected)"
                    ));
                }
                push(out, owner, vec![Span::styled(note, sty(dim()))]);
            }
        }
        Act::Note {
            agent,
            text,
            err,
            at,
            ..
        } => {
            let s = if *err {
                Style::default().fg(Color::Red)
            } else {
                dim()
            };
            let who = agent
                .as_deref()
                .map(|a| format!("{a} "))
                .unwrap_or_default();
            for (j, l) in clean(text)
                .split('\n')
                .flat_map(|l| wrap(l, body_w.saturating_sub(2)))
                .enumerate()
            {
                if j == 0 {
                    push(
                        out,
                        owner,
                        vec![
                            Span::styled(format!("{mark}· {who}{l}"), sty(s)),
                            Span::styled(format!("  {at}"), dim()),
                        ],
                    );
                } else {
                    push(out, owner, vec![Span::styled(format!("    {l}"), sty(s))]);
                }
            }
        }
    }
}
