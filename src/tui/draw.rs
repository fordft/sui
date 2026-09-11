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
    let ws = app.workspace.file_name().and_then(|s| s.to_str()).unwrap_or("?");
    let mode = match app.mode {
        Mode::Solo => "Solo",
        Mode::Mission => "Mission",
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" SUI ", acc().add_modifier(Modifier::BOLD)),
            Span::styled(format!("· workspace: {ws} · Mode: {mode}"), dim()),
            Span::styled(
                if app.running { " · RUNNING" } else { "" },
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                if app.auto.load(std::sync::atomic::Ordering::Relaxed) { " · AUTO" } else { "" },
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
    let hint = if app.running { "running… (Ctrl+S stop)" } else { "type a task — Enter sends · Ctrl+J newline" };
    f.render_widget(
        Paragraph::new(app.input.text())
            .block(Block::default().borders(Borders::ALL).title(hint))
            .wrap(Wrap { trim: false }),
        rows[3],
    );

    // footer
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" Ctrl+T tabs · Ctrl+M solo/mission · Ctrl+B sidebar · Ctrl+S stop · F1 help · Ctrl+Q quit", dim()),
            Span::styled(format!("  {}", app.status), Style::default().fg(Color::Yellow)),
        ])),
        rows[4],
    );

    if let Some(m) = &app.modal {
        draw_modal(f, app, m, area);
    }
}

fn draw_chat(f: &mut Frame, app: &App, a: Rect) {
    let mut lines: Vec<Line> = vec![];
    for it in &app.chat {
        match it {
            ChatItem::User(t) => {
                lines.push(Line::from(Span::styled(
                    "you",
                    Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                )));
                for l in t.lines() {
                    lines.push(Line::from(format!("  {l}")));
                }
            }
            ChatItem::Assistant { agent, text, .. } => {
                lines.push(Line::from(Span::styled(
                    format!("{agent}"),
                    acc().add_modifier(Modifier::BOLD),
                )));
                for l in text.lines() {
                    lines.push(Line::from(format!("  {l}")));
                }
            }
            ChatItem::Tool { name, summary, done, ok, result, .. } => {
                let mark = if !*done {
                    "▸ …"
                } else if *ok {
                    "▸ ok"
                } else {
                    "▸ !"
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("  {mark} {name}: "), Style::default().fg(Color::Magenta)),
                    Span::styled(summary.clone(), dim()),
                ]));
                if *done && !result.is_empty() {
                    for l in result.lines().take(4) {
                        lines.push(Line::from(Span::styled(format!("      {l}"), dim())));
                    }
                }
            }
            ChatItem::Sys(t) => {
                lines.push(Line::from(Span::styled(format!("· {t}"), dim())));
            }
        }
    }
    let inner_h = a.height.saturating_sub(2) as usize;
    let total = lines.len();
    let top = total.saturating_sub(inner_h).saturating_sub(app.scroll);
    let view: Vec<Line> = lines.into_iter().skip(top).take(inner_h).collect();
    f.render_widget(
        Paragraph::new(view).block(Block::default().borders(Borders::ALL).title("conversation")),
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
        items.push(ListItem::new(Span::styled("diff:", acc())));
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
        format!("{:<14} {:<22} {:>5} {:>8} {:>8} {:>8} {:>8}", "agent", "model", "reqs", "in", "cached", "wr", "out"),
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
            SettingsRow::Workers => (
                format!("  worker concurrency: {} (max 2)", app.ui.worker_count.unwrap_or(1)),
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
                format!("  workspace: {}", app.workspace.display()),
                Style::default(),
            ),
            SettingsRow::Acceptance => (
                format!("  acceptance cmds: {}", app.ui.acceptance.len()),
                Style::default(),
            ),
        };
        items.push(ListItem::new(Line::from(Span::styled(
            text,
            if sel { Style::default().bg(Color::DarkGray) } else { sty },
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

    let elapsed = app
        .started
        .map(|s| format!("{:?}", s.elapsed()))
        .unwrap_or_else(|| "—".into());
    let run_lines = vec![
        Line::from(format!("stage:   {}", app.stage)),
        Line::from(format!("elapsed: {}", if app.running { elapsed.as_str() } else { "—" })),
        Line::from(format!("outcome: {}", if app.outcome.is_empty() { "—" } else { &app.outcome })),
        Line::from(Span::styled("est. cost: —", dim())),
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
        .constraints([Constraint::Fill(1), Constraint::Length(h.min(a.height)), Constraint::Fill(1)])
        .split(a);
    let h2 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Fill(1), Constraint::Length(w.min(a.width)), Constraint::Fill(1)])
        .split(v[1]);
    h2[1]
}

fn draw_modal(f: &mut Frame, app: &App, m: &Modal, area: Rect) {
    match m {
        Modal::Permission { summary, .. } => {
            let r = centered(60, 7, area);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(summary.clone()),
                    Line::from(""),
                    Line::from(Span::styled("[y/Y] once   [a/A] session   [n/N/Esc] deny", acc())),
                ])
                .block(Block::default().borders(Borders::ALL).title("permission")),
                r,
            );
        }
        Modal::Help => {
            let r = centered(70, 14, area);
            f.render_widget(Clear, r);
            f.render_widget(
                Paragraph::new(vec![
                    Line::from("keys"),
                    Line::from("  Ctrl+T cycle tabs   Ctrl+M solo/mission   Ctrl+B sidebar"),
                    Line::from("  Ctrl+J newline in input   PgUp/PgDn scroll   ↑/↓ select"),
                    Line::from("  Enter send/activate   Esc close modal   Ctrl+S stop run"),
                    Line::from("  Ctrl+Q quit"),
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
                            Field::ApiKey => {
                                "•".repeat(pf.key.text().chars().count().min(24))
                            }
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
            let mut items: Vec<ListItem> = vec![ListItem::new(format!("filter: {}", p.filter.text()))];
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
                    if i == p.sel { Style::default().bg(Color::DarkGray) } else { Style::default() },
                ))));
            }
            f.render_widget(
                List::new(items).block(Block::default().borders(Borders::ALL).title(p.title.clone())),
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
                .block(Block::default().borders(Borders::ALL).title("test connection")),
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
