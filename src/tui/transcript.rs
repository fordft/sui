//! Pure transcript projection: activity groups → styled, display-width
//! wrapped, sanitized rows. Shared by the renderer (draw.rs) and App's
//! scroll-anchor math — one source of truth for "which row is which item".
//!
//! Everything here is a VIEW. Folding changes only which rows exist;
//! captured text on the items is never shortened or deleted.

use ratatui::style::{Modifier, Style};
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

fn dim(app: &App) -> Style {
    Style::default().fg(app.theme().muted)
}
fn acc(app: &App) -> Style {
    Style::default().fg(app.theme().accent)
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
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\t' => out.push_str("    "),
            c if (c as u32) < 0x20 && c != '\n' => {}
            c if (0x7f..=0x9f).contains(&(c as u32)) => {}
            c => out.push(c),
        }
    }
    out
}

/// Clean `text` once, then lazily wrap its lines to width `w` — `take(n)`
/// stops the wrap work early. Cleaning the whole text first (not per
/// raw line) keeps escape-stripping identical to the eager path: an OSC
/// sequence containing '\n' is removed whole rather than split apart.
fn wrapped(text: &str, w: usize) -> impl Iterator<Item = String> {
    clean(text)
        .split('\n')
        .map(str::to_string)
        .collect::<Vec<_>>()
        .into_iter()
        .flat_map(move |l| wrap(&l, w))
}

/// Last `n` display rows of `text` wrapped to `w` — cleaning is once
/// over the (bounded) buffer; wrap work happens only on tail lines.
fn tail_rows(text: &str, w: usize, n: usize) -> Vec<String> {
    let cleaned = clean(text);
    let mut out: Vec<String> = Vec::new();
    for l in cleaned.rsplit('\n') {
        if out.len() >= n {
            break;
        }
        let mut rows = wrap(l, w);
        // append this source line's display rows bottom-up
        while let Some(r) = rows.pop() {
            out.push(r);
            if out.len() >= n {
                break;
            }
        }
    }
    out.reverse();
    out
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

/// Slice a plain-text row by terminal display columns [c0, c1) —
/// grapheme-aware so a selection edge never splits a cluster.
pub fn slice_cols(s: &str, c0: usize, c1: usize) -> String {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut out = String::new();
    let mut col = 0usize;
    for g in s.graphemes(true) {
        let gw = UnicodeWidthStr::width(g);
        if col + gw > c0 && col < c1 {
            out.push_str(g);
        }
        col += gw;
        if col >= c1 {
            break;
        }
    }
    out
}

/// Paint a mouse-drag selection onto a rendered row: cells in [c0, c1)
/// get the selection background, spans split at the edges. Display-width
/// math — matches what the terminal actually shows.
pub fn paint_sel(line: Line<'static>, c0: usize, c1: usize, sty: Style) -> Line<'static> {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut out: Vec<Span> = Vec::new();
    let mut col = 0usize;
    for sp in line.spans {
        let mut cur = String::new();
        let mut cur_sel = false;
        for g in sp.content.graphemes(true) {
            let gw = UnicodeWidthStr::width(g);
            let in_sel = col + gw > c0 && col < c1;
            if in_sel != cur_sel && !cur.is_empty() {
                out.push(Span::styled(
                    std::mem::take(&mut cur),
                    if cur_sel {
                        sp.style.patch(sty)
                    } else {
                        sp.style
                    },
                ));
            }
            cur_sel = in_sel;
            cur.push_str(g);
            col += gw;
        }
        if !cur.is_empty() {
            out.push(Span::styled(
                cur,
                if cur_sel {
                    sp.style.patch(sty)
                } else {
                    sp.style
                },
            ));
        }
    }
    Line::from(out).style(line.style)
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
        if g.id != 0 && !out.is_empty() {
            push(&mut out, (g.id, None), vec![]);
        }
        emit_group(app, g, width, sel, &mut out);
    }
    out
}

/// Navigation follows visible content without projecting all text a second time.
pub fn focusable_items(app: &App) -> Vec<(usize, Option<usize>)> {
    let mut targets = Vec::new();
    for (gi, g) in app.groups.iter().enumerate() {
        if !g.task.is_empty() || g.done {
            targets.push((gi, None));
        }
        if g.folded() {
            for (ii, item) in g.items.iter().enumerate() {
                if matches!(
                    item,
                    Act::Tool {
                        status: Some(ToolStatus::Failed | ToolStatus::Error | ToolStatus::Timeout),
                        ..
                    }
                ) {
                    targets.push((gi, Some(ii)));
                }
            }
            if let Some(ii) = g
                .items
                .iter()
                .rposition(|it| matches!(it, Act::Assistant { .. }))
            {
                targets.push((gi, Some(ii)));
            }
            continue;
        }
        let mut ii = 0;
        while ii < g.items.len() {
            let item = &g.items[ii];
            if !matches!(item, Act::Req { .. })
                && !(app.reasoning == ReasonPref::Hidden && matches!(item, Act::Reason { .. }))
            {
                targets.push((gi, Some(ii)));
            }
            let first = ii;
            ii += 1;
            while ii < g.items.len() && same_folded_tool(item, &g.items[ii]) {
                ii += 1;
            }
            debug_assert!(ii > first);
        }
    }
    targets
}

fn same_folded_tool(first: &Act, next: &Act) -> bool {
    match (first, next) {
        (
            Act::Tool {
                agent,
                name,
                summary,
                exit,
                status: Some(ToolStatus::Ok),
                expanded: false,
                ..
            },
            Act::Tool {
                agent: a,
                name: n,
                summary: s,
                exit: e,
                status: Some(ToolStatus::Ok),
                expanded: false,
                ..
            },
        ) => agent == a && name == n && summary == s && exit == e,
        _ => false,
    }
}

fn success_label(exit: Option<i32>) -> String {
    exit.map(|code| format!("exit {code}"))
        .unwrap_or_else(|| "ok".into())
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
    let width = w.max(1);
    let gid = g.id;
    let sel_style = |owner: (u64, Option<u64>)| -> Option<Style> {
        if sel == Some(owner) {
            Some(app.theme().selected())
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
                    format!("{}You", mark(owner)),
                    sel_style(owner).unwrap_or_else(|| {
                        Style::default()
                            .fg(app.theme().text)
                            .add_modifier(Modifier::BOLD)
                    }),
                ),
                Span::styled(format!("  {}", g.at), dim(app)),
            ],
        );
        let task_lines: Vec<String> = wrapped(&g.task, width.saturating_sub(2)).collect();
        let show = if g.folded() {
            TASK_CAP
        } else {
            task_lines.len().min(20)
        };
        for l in task_lines.iter().take(show) {
            push(
                out,
                owner,
                vec![
                    Span::styled("│ ", sel_style(owner).unwrap_or_else(|| acc(app))),
                    Span::styled(l.clone(), sel_style(owner).unwrap_or_default()),
                ],
            );
        }
        if task_lines.len() > show {
            push(
                out,
                owner,
                vec![Span::styled(
                    format!("  … {} more line(s)", task_lines.len() - show),
                    dim(app),
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
                format!("  ▸ Activity · {}", g.summary()),
                sel_style(owner).unwrap_or_else(|| {
                    if g.failed {
                        Style::default().fg(app.theme().error)
                    } else {
                        dim(app)
                    }
                }),
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
            ("✗", Style::default().fg(app.theme().error))
        } else {
            ("✓", Style::default().fg(app.theme().success))
        };
        push(
            out,
            owner,
            vec![Span::styled(
                format!("  ▾ Activity · {glyph} {}", g.summary()),
                sel_style(owner).unwrap_or(sty),
            )],
        );
    }

    // items, with adjacent identical ok-tools grouped as ×N
    let mut i = 0;
    while i < g.items.len() {
        if let Act::Tool {
            status: Some(ToolStatus::Ok),
            expanded: false,
            ..
        } = &g.items[i]
        {
            let mut n = 1usize;
            while i + n < g.items.len() {
                if same_folded_tool(&g.items[i], &g.items[i + n]) {
                    n += 1;
                } else {
                    break;
                }
            }
            if n > 1 {
                let owner = (gid, Some(g.items[i].id()));
                let (a2, n2, s2, ms2, exit) = match &g.items[i] {
                    Act::Tool {
                        agent,
                        name,
                        summary,
                        ms,
                        exit,
                        ..
                    } => (agent, name, summary, *ms, *exit),
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
                            "  {}✓ {who}{n2} · {} · {}ms — {}  ×{n}",
                            mark(owner),
                            success_label(exit),
                            ms2,
                            clean(s2.lines().next().unwrap_or(""))
                        ),
                        sel_style(owner).unwrap_or_else(|| Style::default().fg(app.theme().muted)),
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
            let ms = super::slime::clock(app);
            let text = format!(
                "  {}{} {agent} working… {}",
                mark(owner),
                super::slime::spinner(ms),
                super::slime::working_phrase(ms)
            );
            let spans = match sel_style(owner) {
                Some(style) => vec![Span::styled(text, style)],
                None if app.anim.motion != super::fx::Motion::Off => {
                    super::fx::shimmer(&text, app.theme().muted, app.theme().glow, ms)
                }
                None => vec![Span::styled(text, dim(app))],
            };
            push(out, owner, spans);
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
    let sty = |s: Style| {
        if sel {
            s.patch(app.theme().selected())
        } else {
            s
        }
    };
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
            push(out, owner, vec![]);
            push(
                out,
                owner,
                vec![
                    Span::styled(
                        format!(
                            "{mark}{}",
                            if agent == "solo" {
                                "Sui".into()
                            } else {
                                clean(agent)
                            }
                        ),
                        sty(Style::default()
                            .fg(if agent == "solo" {
                                app.theme().accent
                            } else {
                                app.theme().mission
                            })
                            .add_modifier(Modifier::BOLD)),
                    ),
                    Span::styled(
                        format!("  {at}{}", if *done { "" } else { "  ◜" }),
                        dim(app),
                    ),
                ],
            );
            let mut code_fence: Option<(char, usize)> = None;
            let mut count = 0;
            for raw in clean(text).split('\n') {
                let trimmed = raw.trim_start();
                let fence = ['`', '~'].into_iter().find_map(|ch| {
                    let n = trimmed.chars().take_while(|&c| c == ch).count();
                    (n >= 3 && raw.len() - trimmed.len() <= 3).then_some((ch, n))
                });
                let in_code = code_fence.is_some();
                let mut fence_line = false;
                if let Some((ch, n)) = fence {
                    if let Some((open_ch, open_n)) = code_fence {
                        if ch == open_ch && n >= open_n && trimmed[n..].trim().is_empty() {
                            code_fence = None;
                            fence_line = true;
                        }
                    } else {
                        code_fence = Some((ch, n));
                        fence_line = true;
                    }
                }
                let heading = trimmed.chars().take_while(|&c| c == '#').count();
                let style = if fence_line {
                    dim(app).bg(app.theme().surface)
                } else if in_code {
                    app.theme().panel()
                } else if (1..=6).contains(&heading)
                    && trimmed.as_bytes().get(heading) == Some(&b' ')
                {
                    acc(app).add_modifier(Modifier::BOLD)
                } else if trimmed.starts_with("> ") {
                    dim(app).add_modifier(Modifier::ITALIC)
                } else {
                    Style::default()
                };
                for line in wrap(raw, body_w) {
                    if count == TEXT_CAP {
                        push(
                            out,
                            owner,
                            vec![Span::styled(
                                "  … display capped — v opens captured details",
                                sty(dim(app)),
                            )],
                        );
                        return;
                    }
                    out.push(Row {
                        owner,
                        line: Line::from(format!("  {line}")).style(sty(style)),
                    });
                    count += 1;
                }
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
                format!("{mark}  ◜ reasoning — {agent}")
            };
            push(
                out,
                owner,
                vec![
                    Span::styled(label, sty(Style::default().fg(app.theme().mission))),
                    Span::styled(format!("  {at}"), dim(app)),
                ],
            );
            if full {
                let mut lines = wrapped(text, width.saturating_sub(4));
                for l in lines.by_ref().take(TEXT_CAP) {
                    push(
                        out,
                        owner,
                        vec![Span::styled(format!("    {l}"), sty(dim(app)))],
                    );
                }
                if lines.next().is_some() {
                    push(
                        out,
                        owner,
                        vec![Span::styled(
                            "    … display capped — v opens captured details",
                            sty(dim(app)),
                        )],
                    );
                }
            } else if !*done {
                // live tail preview only while streaming
                for l in tail_rows(text, body_w.saturating_sub(4), REASON_PREVIEW) {
                    push(
                        out,
                        owner,
                        vec![Span::styled(format!("    {l}"), sty(dim(app)))],
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
            // summary is model/ACP-generated text — strip control
            // sequences before it reaches a header row (escape-sequence
            // injection into the transcript).
            let head = clean(summary.lines().next().unwrap_or(""));
            let (glyph, label, gsty) = match status {
                None => ("◜", "…", Style::default().fg(app.theme().mission)),
                Some(ToolStatus::Ok) => ("✓", "ok", Style::default().fg(app.theme().muted)),
                Some(ToolStatus::Denied) => {
                    ("⊘", "denied", Style::default().fg(app.theme().warning))
                }
                Some(ToolStatus::Skipped) => ("·", "skipped", dim(app)),
                Some(ToolStatus::Intercepted) => ("·", "control plane", dim(app)),
                Some(_) => (
                    "✗",
                    status.unwrap().label(),
                    Style::default().fg(app.theme().error),
                ),
            };
            let tail_info = match status {
                None => " · running".to_string(),
                Some(ToolStatus::Ok) => format!(" · {} · {}ms", success_label(*exit), ms),
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
                    Span::styled(format!("  {at}"), dim(app)),
                ],
            );
            match status {
                None => {
                    // genuine live output — bounded tail preview
                    for l in tail_rows(live, body_w.saturating_sub(4), LIVE_PREVIEW) {
                        push(
                            out,
                            owner,
                            vec![Span::styled(format!("      {l}"), sty(dim(app)))],
                        );
                    }
                }
                Some(s)
                    if !s.ok() && *s != ToolStatus::Skipped && *s != ToolStatus::Intercepted =>
                {
                    // failure keeps a diagnostic excerpt visible
                    let show = if *expanded { TEXT_CAP } else { FAIL_EXCERPT };
                    let cleaned = clean(result);
                    let mut lines: Vec<String> = Vec::new();
                    let mut capped = false;
                    for l in cleaned.rsplit('\n') {
                        if l.starts_with("status:") || l.is_empty() {
                            continue;
                        }
                        if lines.len() >= show {
                            capped = true;
                            break;
                        }
                        let mut rows = wrap(l, body_w.saturating_sub(4));
                        while let Some(r) = rows.pop() {
                            lines.push(r);
                            if lines.len() >= show {
                                capped = !rows.is_empty();
                                break;
                            }
                        }
                    }
                    lines.reverse();
                    for l in &lines {
                        push(
                            out,
                            owner,
                            vec![Span::styled(
                                format!("      {l}"),
                                sty(Style::default().fg(app.theme().error)),
                            )],
                        );
                    }
                    if *expanded && capped {
                        push(
                            out,
                            owner,
                            vec![Span::styled(
                                "      … display capped — full captured result in the run export",
                                sty(dim(app)),
                            )],
                        );
                    }
                }
                Some(ToolStatus::Ok) if *expanded => {
                    let mut lines = wrapped(result, width.saturating_sub(6));
                    for l in lines.by_ref().take(TEXT_CAP) {
                        push(
                            out,
                            owner,
                            vec![Span::styled(format!("      {l}"), sty(dim(app)))],
                        );
                    }
                    if lines.next().is_some() {
                        push(
                            out,
                            owner,
                            vec![Span::styled(
                                "      … display capped — v opens captured details",
                                sty(dim(app)),
                            )],
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
                push(out, owner, vec![Span::styled(note, sty(dim(app)))]);
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
                Style::default().fg(app.theme().error)
            } else {
                dim(app)
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
                            Span::styled(format!("  {at}"), dim(app)),
                        ],
                    );
                } else {
                    push(out, owner, vec![Span::styled(format!("    {l}"), sty(s))]);
                }
            }
        }
    }
}
