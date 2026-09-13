//! Rendering. Read-only over App — never mutates, never adds data to any
//! model request. Narrow terminals collapse the sidebar instead of
//! crushing the content.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use super::app::*;

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}
fn acc() -> Style {
    Style::default().fg(Color::Cyan)
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // header
            Constraint::Length(1), // tabs
            Constraint::Min(5),    // body
            Constraint::Length(3), // input
            Constraint::Length(1), // footer
        ])
        .split(area);

    // header
    let ws = app
        .workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    let mode = match app.mode {
        Mode::Solo => "Solo",
        Mode::Mission => "Mission",
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" SUI ", acc().add_modifier(Modifier::BOLD)),
            Span::styled(format!("· workspace: {ws} · Mode: {mode}"), dim()),
            Span::styled(
                if app.running {
                    const SPIN: &[char] = &['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                    let e = app.started.map(|s| s.elapsed().as_secs()).unwrap_or(0);
                    // wall-clock driven — animates on the heartbeat redraw
                    let frame = (std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis()
                        / 100) as usize;
                    format!(" · {} RUNNING {e}s", SPIN[frame % SPIN.len()])
                } else {
                    String::new()
                },
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                if app.auto.load(std::sync::atomic::Ordering::Relaxed) {
                    " · AUTO"
                } else {
                    ""
                },
                Style::default().fg(Color::Magenta),
            ),
        ])),
        rows[0],
    );

    // tab strip
    let tabs: Vec<Span> = Tab::ALL
        .iter()
        .map(|t| {
            if *t == app.tab {
                Span::styled(
                    format!(" {} ", t.name()),
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(format!(" {} ", t.name()), dim())
            }
        })
        .collect();
    f.render_widget(Paragraph::new(Line::from(tabs)), rows[1]);

    // body: content + sidebar (collapsed when narrow)
    let narrow = area.width < 90;
    let show_side = app.sidebar && !narrow;
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(if show_side {
            vec![Constraint::Percentage(70), Constraint::Percentage(30)]
        } else {
            vec![Constraint::Percentage(100)]
        })
        .split(rows[2]);

    match app.tab {
        Tab::Chat => draw_chat(f, app, body[0]),
        Tab::Tasks => draw_tasks(f, app, body[0]),
        Tab::Changes => draw_changes(f, app, body[0]),
        Tab::Usage => draw_usage(f, app, body[0]),
        Tab::Settings => draw_settings(f, app, body[0]),
    }
    if show_side {
        draw_sidebar(f, app, body[1]);
    }

    // input
    let hint = if app.nav && app.tab == Tab::Chat {
        "transcript focused — ↑↓ select · Enter expand · v details · Esc back"
    } else if app.running {
        "running… (Ctrl+S stop · Tab selects activity)"
    } else {
        "type a task — Enter sends · Tab selects activity · Ctrl+N newline · /mission /solo"
    };
    f.render_widget(
        Paragraph::new(app.input.text())
            .block(Block::default().borders(Borders::ALL).title(hint))
            .wrap(Wrap { trim: false }),
        rows[3],
    );

    // footer
    let keys =
        " Ctrl+T tabs · Ctrl+O solo/mission · Ctrl+B sidebar · Ctrl+S stop · F1 help · Ctrl+Q quit";
    let room = (area.width as usize).saturating_sub(keys.len() + 3);
    let st = if app.status.chars().count() > room && room > 12 {
        // middle-truncate long paths/messages instead of clipping the tail
        let keep = room - 1;
        let head = keep / 2;
        let tail = keep - head;
        let mut h: String = app.status.chars().take(head).collect();
        h.push('…');
        let t: String = {
            let cs: Vec<char> = app.status.chars().collect();
            cs[cs.len() - tail..].iter().collect()
        };
        format!("{h}{t}")
    } else {
        app.status.clone()
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(keys, dim()),
            Span::styled(format!("  {st}"), Style::default().fg(Color::Yellow)),
        ])),
        rows[4],
    );

    if let Some(m) = &app.modal {
        draw_modal(f, app, m, area);
    } else if app.tab == Tab::Chat {
        // visible caret — map the cursor char-index through wrap+newlines
        let w = rows[3].width.saturating_sub(2).max(1) as usize;
        let (mut cy, mut cx) = (0usize, 0usize);
        for (i, ch) in app.input.text().chars().enumerate() {
            if i == app.input.cursor {
                break;
            }
            if ch == '\n' {
                cy += 1;
                cx = 0;
            } else {
                cx += 1;
                if cx >= w {
                    cy += 1;
                    cx = 0;
                }
            }
        }
        // clamp inside the box — the paragraph doesn't scroll, so a
        // longer input clips and the caret must stay on its last row
        let cy = cy.min(rows[3].height.saturating_sub(2).saturating_sub(1) as usize);
        f.set_cursor_position((
            rows[3].x + 1 + cx.min(w - 1) as u16,
            rows[3].y + 1 + cy as u16,
        ));
    }
}

fn draw_chat(f: &mut Frame, app: &App, a: Rect) {
    let inner_w = a.width.saturating_sub(2).max(1) as usize;
    let inner_h = a.height.saturating_sub(2) as usize;
    // the transcript projection needs the real viewport for wrap math;
    // the cells feed App's scroll-anchor math (read-only here)
    app.view_w.set(inner_w);
    app.view_h.set(inner_h);
    let rows = super::transcript::rows(app, inner_w);
    let total = rows.len();
    let top = total.saturating_sub(inner_h).saturating_sub(app.scroll);
    let view: Vec<Line> = rows
        .into_iter()
        .skip(top)
        .take(inner_h)
        .map(|r| r.line)
        .collect();
    let title = if app.nav {
        "activity — ↑↓ select · Enter/Space expand/collapse · v details · Esc/Tab input · End live"
            .into()
    } else if app.scroll > 0 {
        format!(
            "activity — scrolled ▲ {} rows · End/PgDn to live · Tab selects",
            app.scroll
        )
    } else {
        "activity — Tab selects · Enter sends".into()
    };
    f.render_widget(
        Paragraph::new(view).block(Block::default().borders(Borders::ALL).title(title)),
        a,
    );
}

fn draw_tasks(f: &mut Frame, app: &App, a: Rect) {
    let items: Vec<ListItem> = if app.tasks.is_empty() {
        vec![ListItem::new(Span::styled(
            "no plan yet — mission plans appear here",
            dim(),
        ))]
    } else {
        app.tasks
            .iter()
            .map(|t| {
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:>4} ", t.id), acc()),
                    Span::styled(format!("{:<24}", t.status), Style::default()),
                    Span::styled(t.owned.clone(), dim()),
                    Span::styled(format!(" {}", t.sha), dim()),
                ]))
            })
            .collect()
    };
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("tasks")),
        a,
    );
}

fn draw_changes(f: &mut Frame, app: &App, a: Rect) {
    let mut items: Vec<ListItem> = app
        .changes
        .iter()
        .map(|c| ListItem::new(c.clone()))
        .collect();
    if items.is_empty() {
        items.push(ListItem::new(Span::styled("no changes recorded", dim())));
    }
    if let Some(s) = &app.accepted_sha {
        items.push(ListItem::new(Span::styled(
            format!("accepted: {s}"),
            Style::default().fg(Color::Green),
        )));
    }
    if let Some(au) = &app.audit {
        items.push(ListItem::new(""));
        items.push(ListItem::new(Span::styled("audit:", acc())));
        for l in au.lines().take(12) {
            items.push(ListItem::new(Span::styled(format!("  {l}"), dim())));
        }
    }
    if !app.diff_text.is_empty() {
        items.push(ListItem::new(""));
        items.push(ListItem::new(Span::styled(
            "working tree vs HEAD (mission edits live on sui-mission-* branches):",
            acc(),
        )));
        for l in app.diff_text.lines().take(30) {
            items.push(ListItem::new(Span::styled(format!("  {l}"), dim())));
        }
    }
    f.render_widget(
        List::new(items).block(Block::default().borders(Borders::ALL).title("changes")),
        a,
    );
}

fn draw_usage(f: &mut Frame, app: &App, a: Rect) {
    let mut lines = vec![Line::from(Span::styled(
        format!(
            "{:<14} {:<22} {:>5} {:>8} {:>8} {:>8} {:>8}",
            "agent", "model", "reqs", "in", "cached", "wr", "out"
        ),
        acc(),
    ))];
    for (agent, (model, u)) in &app.usage {
        lines.push(Line::from(format!(
            "{:<14} {:<22} {:>5} {:>8} {:>8} {:>8} {:>8}",
            agent, model, u.requests, u.input, u.cache_read, u.cache_write, u.output
        )));
    }
    if app.usage.is_empty() {
        lines.push(Line::from(Span::styled("no usage yet", dim())));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "costs: — when pricing unknown (never shown as zero)",
        dim(),
    )));
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("usage")),
        a,
    );
}

fn draw_settings(f: &mut Frame, app: &App, a: Rect) {
    let mut items: Vec<ListItem> = vec![];
    for (i, row) in app.settings_rows().iter().enumerate() {
        let sel = i == app.settings_sel;
        let (text, sty) = match row {
            SettingsRow::AddProfile => ("+ add provider…".into(), acc()),
            SettingsRow::EditProfile(n) => {
                let p = &app.profiles[n];
                let has_key = app.session_keys.contains_key(n)
                    || p.key_env.as_deref().map(|e| std::env::var(e).is_ok()).unwrap_or(false)
                    || p.api_key.is_some();
                (
                    format!(
                        "  {:<14} {} · model={} · key={}",
                        n,
                        p.base_url.as_deref().unwrap_or("?"),
                        p.model.as_deref().unwrap_or("—"),
                        if has_key { "set" } else { "MISSING" }
                    ),
                    Style::default(),
                )
            }
            SettingsRow::Role(r) => (
                format!("  {:<14} → {}", r.name(), app.role_profile(*r).unwrap_or("—".into())),
                Style::default(),
            ),
            SettingsRow::Mode => {
                let mission = app.mode == crate::tui::app::Mode::Mission;
                (
                    format!(
                        "  run mode: {} (Enter toggles · Ctrl+O · /mission · /solo · sui tui --mission)",
                        if mission { "mission" } else { "solo" }
                    ),
                    if mission { Style::default().fg(Color::Magenta) } else { Style::default() },
                )
            }
            SettingsRow::Export => (
                "  export run report → exports/<run>/report.md (or /export)".into(),
                Style::default(),
            ),
            SettingsRow::Workers => (
                format!("  worker concurrency: {} (max 2)", app.ui.worker_count.unwrap_or(1)),
                Style::default(),
            ),
            SettingsRow::Reasoning => (
                format!(
                    "  reasoning display: {} (auto hides after streaming · Ctrl+R cycles · view-only)",
                    app.reasoning.name()
                ),
                Style::default(),
            ),
            SettingsRow::Auto => {
                let on = app.auto.load(std::sync::atomic::Ordering::Relaxed);
                (
                    format!(
                        "  auto-approve this session: {} (YOLO — resets on restart/workspace change)",
                        if on { "on" } else { "off" }
                    ),
                    if on { Style::default().fg(Color::Magenta) } else { Style::default() },
                )
            }
            SettingsRow::Workspace => (
                format!("  workspace (applies on next launch): {}", app.workspace.display()),
                Style::default(),
            ),
            SettingsRow::Acceptance => (
                format!("  acceptance cmds: {}", app.ui.acceptance.len()),
                Style::default(),
            ),
        };
        items.push(ListItem::new(Line::from(Span::styled(
            text,
            if sel {
                Style::default().bg(Color::DarkGray)
            } else {
                sty
            },
        ))));
    }
    f.render_widget(
        List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .title("settings — Enter to edit · shell exec is NOT a sandbox"),
        ),
        a,
    );
}

fn draw_sidebar(f: &mut Frame, app: &App, a: Rect) {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(a);

    let mut agents: Vec<ListItem> = vec![];
    let mut push = |label: &str, prof: Option<String>| {
        agents.push(ListItem::new(Line::from(vec![
            Span::styled(format!("{label:<14}"), Style::default()),
            Span::styled(prof.unwrap_or("—".into()), dim()),
        ])));
    };
    match app.mode {
        Mode::Solo => push("solo", app.role_profile(Role::Solo)),
        Mode::Mission => {
            push("orchestrator", app.role_profile(Role::Orchestrator));
            for i in 0..app.ui.worker_count.unwrap_or(1).min(2) {
                push(&format!("worker {}", i + 1), app.role_profile(Role::Worker));
            }
            push("auditor", app.role_profile(Role::Auditor));
        }
    }
    f.render_widget(
        List::new(agents).block(Block::default().borders(Borders::ALL).title("agents")),
        v[0],
    );

    let run_id = app
        .run_dir
        .as_ref()
        .and_then(|d| d.file_name().map(|n| n.to_string_lossy().to_string()))
        .unwrap_or_else(|| "—".into());
    let run_lines = vec![
        Line::from(format!("stage:   {}", app.stage)),
        Line::from(format!(
            "elapsed: {}",
            if app.running {
                format!(
                    "{}s",
                    app.started.map(|s| s.elapsed().as_secs()).unwrap_or(0)
                )
            } else {
                "—".into()
            }
        )),
        Line::from(format!(
            "outcome: {}",
            if app.outcome.is_empty() {
                "—"
            } else {
                &app.outcome
            }
        )),
        Line::from(Span::styled(format!("run: {run_id}"), dim())),
        Line::from(Span::styled("export: /export or sui export", dim())),
    ];
    f.render_widget(
        Paragraph::new(run_lines).block(Block::default().borders(Borders::ALL).title("run")),
        v[1],
    );
}

// ── modals ────────────────────────────────────────────────────────────

fn centered(w: u16, h: u16, a: Rect) -> Rect {
    let v = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(h.min(a.height)),
            Constraint::Fill(1),
        ])
        .split(a);
    let h2 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Fill(1),
            Constraint::Length(w.min(a.width)),
            Constraint::Fill(1),
        ])
        .split(v[1]);
    h2[1]
}

fn draw_modal(f: &mut Frame, app: &App, m: &Modal, area: Rect) {
    match m {
        Modal::Permission { agent, summary, .. } => {
            let r = centered(76, 12, area);
            let more = app.pending_perms.len();
            let title = format!(
                "permission — {agent}{} — [y] once [a] session [n/Esc] deny",
                if more > 0 {
                    format!(" · +{more} pending")
                } else {
                    String::new()
                }
            );
            // bounded preview: never approve a command you can't read —
            // heredocs/very long commands show head lines + a marker
            let sl: Vec<&str> = summary.lines().collect();
            let show = 8usize;
            let mut lines: Vec<Line> = sl
                .iter()
                .take(show)
                .map(|l| Line::from((*l).to_string()))
                .collect();
            if sl.len() > show {
                lines.push(Line::from(Span::styled(
                    format!(
                        "… {} more line(s) — review the full command in the run export",
                        sl.len() - show
                    ),
                    dim(),
                )));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "[y/Y] once   [a/A] session   [n/N/Esc] deny",
                acc(),
            )));
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title(title)),
                r,
            );
        }
        Modal::Help => {
            let r = centered(74, 16, area);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from("keys"),
                    Line::from("  Ctrl+T cycle tabs   Ctrl+O solo/mission   Ctrl+B sidebar   Ctrl+R reasoning"),
                    Line::from("  Ctrl+N newline   PgUp/PgDn scroll   End back to live   ↑ recall task"),
                    Line::from("  Enter send/activate   Esc close/back   Ctrl+S stop   Ctrl+Q quit"),
                    Line::from("activity transcript (Chat tab):"),
                    Line::from("  Tab focus transcript   ↑↓ select   Enter/Space expand/collapse"),
                    Line::from("  v full details   Esc/Tab back to input"),
                    Line::from("  /mission /solo /export /help — settings: run mode · export report"),
                    Line::from(""),
                    Line::from("shell execution is NOT a sandbox — approvals are per-action"),
                    Line::from("keys are masked on entry; env var or session storage only"),
                ])
                .block(Block::default().borders(Borders::ALL).title("help")),
                r,
            );
        }
        Modal::Provider(pf) => {
            let r = centered(74, 18, area);
            f.render_widget(Clear, r);
            let fields = pf.fields();
            let mut lines = vec![];
            let mut btn_row: Option<Line> = None;
            for (i, fld) in fields.iter().enumerate() {
                let sel = pf.focus == i;
                match fld {
                    Field::Test | Field::Save | Field::Cancel => {
                        if btn_row.is_none() {
                            // gather the trailing buttons into one row
                            let mut row = vec![];
                            for (j, b) in fields[i..].iter().enumerate() {
                                let label = match b {
                                    Field::Test => "Test conn.",
                                    Field::Save => "Save",
                                    _ => "Cancel",
                                };
                                row.push(Span::styled(
                                    format!(" [ {} ] ", label),
                                    if pf.focus == i + j {
                                        Style::default().bg(Color::Cyan).fg(Color::Black)
                                    } else {
                                        Style::default()
                                    },
                                ));
                            }
                            btn_row = Some(Line::from(row));
                        }
                    }
                    _ => {
                        let val = match fld {
                            Field::Name => pf.name.text(),
                            Field::BaseUrl => pf.base_url.text(),
                            Field::Model => pf.model.text(),
                            Field::KeyEnv => pf.key_env.text(),
                            Field::ApiKey => "•".repeat(pf.key.text().chars().count().min(24)),
                            Field::Auth => format!("◀ {} ▶", pf.auth.name()),
                            Field::CredSrc => "Environment variable".to_string(),
                            Field::Store => format!("◀ {} ▶", pf.store.name()),
                            _ => String::new(),
                        };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{:<17}", fld.label()),
                                if sel { acc() } else { dim() },
                            ),
                            Span::styled(
                                val,
                                if sel {
                                    Style::default().add_modifier(Modifier::UNDERLINED)
                                } else {
                                    Style::default()
                                },
                            ),
                        ]));
                    }
                }
            }
            lines.push(Line::from(Span::styled(
                format!("→ POST {}", pf.endpoint),
                dim(),
            )));
            lines.push(Line::from(""));
            if let Some(row) = btn_row {
                lines.push(row);
            }
            if !pf.status.is_empty() {
                lines.push(Line::from(Span::styled(
                    pf.status.clone(),
                    Style::default().fg(Color::Yellow),
                )));
            }
            lines.push(Line::from(Span::styled(
                "test sends one small live request — Store chooses where the key lives",
                dim(),
            )));
            f.render_widget(
                Paragraph::new(lines).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(pf.ptype.name()),
                ),
                r,
            );
        }
        Modal::Picker(p) => {
            let r = centered(70, 16, area);
            f.render_widget(Clear, r);
            let mut items: Vec<ListItem> =
                vec![ListItem::new(format!("filter: {}", p.filter.text()))];
            if p.loading {
                items.push(ListItem::new(Span::styled("loading…", dim())));
            }
            let list: Vec<String> = p
                .items
                .iter()
                .filter(|i| {
                    let f = p.filter.text().to_lowercase();
                    f.is_empty() || i.to_lowercase().contains(&f)
                })
                .cloned()
                .collect();
            for (i, it) in list.iter().enumerate().take(12) {
                items.push(ListItem::new(Line::from(Span::styled(
                    it.clone(),
                    if i == p.sel {
                        Style::default().bg(Color::DarkGray)
                    } else {
                        Style::default()
                    },
                ))));
            }
            f.render_widget(
                List::new(items).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(p.title.clone()),
                ),
                r,
            );
        }
        Modal::ConfirmTest { name } => {
            let r = centered(56, 6, area);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("probe '{name}' sends one small live request.")),
                    Line::from(Span::styled("[y] proceed   [n] cancel", acc())),
                ])
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title("test connection"),
                ),
                r,
            );
        }
        Modal::View {
            title,
            text,
            scroll,
        } => {
            // full-details viewer: captured text, sanitized + wrapped.
            let r = centered(90, area.height.saturating_sub(4).min(34), area);
            f.render_widget(Clear, r);
            let inner_w = r.width.saturating_sub(2).max(1) as usize;
            let lines: Vec<Line> = text
                .split('\n')
                .flat_map(|l| super::transcript::wrap(&super::transcript::clean(l), inner_w))
                .map(|s| Line::from(s.to_string()))
                .collect();
            let inner_h = r.height.saturating_sub(2) as usize;
            let max_scroll = lines.len().saturating_sub(inner_h);
            let s = (*scroll).min(max_scroll);
            let more = if lines.len() > inner_h + s {
                format!(" ▼ +{}", lines.len() - inner_h - s)
            } else {
                String::new()
            };
            f.render_widget(
                Paragraph::new(lines.into_iter().skip(s).take(inner_h).collect::<Vec<_>>()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("{title} — ↑↓ PgUp/PgDn scroll{more} · Esc close")),
                ),
                r,
            );
        }
        Modal::Text { title, buf, .. } => {
            let r = centered(60, 5, area);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(buf.text())
                    .block(Block::default().borders(Borders::ALL).title(title.clone())),
                r,
            );
        }
    }
    let _ = app;
}
