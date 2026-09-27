//! Rendering. Read-only over App — never mutates, never adds data to any
//! model request. Narrow terminals collapse the sidebar instead of
//! crushing the content.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};
use ratatui::Frame;

use super::app::*;
use super::text::{ellipsize, Buf};
use crate::events::GateChoice;

fn dim(app: &App) -> Style {
    Style::default().fg(app.theme().muted)
}
fn acc(app: &App) -> Style {
    Style::default().fg(app.theme().accent)
}
fn panel(app: &App) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(app.theme().border))
        .title_style(Style::default().fg(app.theme().text))
        .style(app.theme().panel())
}

pub fn draw(f: &mut Frame, app: &App) {
    let area = f.area();
    app.viewport.set(area);
    app.hits.borrow_mut().clear();
    app.chat_geom.set(ChatGeom::default());
    let layout = super::layout::regions(area, app);
    f.render_widget(Block::default().style(app.theme().base()), area);
    let ws = app
        .workspace
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    let mode = if app.mode == Mode::Mission {
        "Mission"
    } else {
        "Solo"
    };
    let state =
        if matches!(app.modal, Some(Modal::Permission { .. })) || !app.pending_perms.is_empty() {
            "APPROVAL".to_string()
        } else if app.running {
            match app.started {
                Some(started) => {
                    let elapsed = started.elapsed();
                    let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                    format!(
                        "{} RUNNING {}s",
                        frames[(elapsed.as_millis() / 100 % 10) as usize],
                        elapsed.as_secs()
                    )
                }
                None => "RUNNING".into(),
            }
        } else {
            "IDLE".into()
        };
    let auto = if app.auto.load(std::sync::atomic::Ordering::Relaxed) {
        "AUTO · "
    } else {
        ""
    };
    let nav = if area.width < 70 {
        format!(" {} ▾ ^P ", app.tab.name())
    } else {
        format!(" {} ▾  Commands ^P ", app.tab.name())
    };
    let nav_width = unicode_width::UnicodeWidthStr::width(nav.as_str()) as u16;
    let header = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(nav_width.min(area.width / 2)),
    ])
    .split(layout.header);
    // Status comes before the workspace so AUTO survives ordinary narrow widths.
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" SUI ", acc(app).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!("{auto}{mode} · {state}"),
                Style::default().fg(if !auto.is_empty() {
                    app.theme().warning
                } else if app.mode == Mode::Mission {
                    app.theme().mission
                } else {
                    app.theme().muted
                }),
            ),
            Span::styled(format!("  {ws}"), dim(app)),
        ])),
        header[0],
    );
    f.render_widget(
        Paragraph::new(nav)
            .style(acc(app))
            .alignment(ratatui::layout::Alignment::Right),
        header[1],
    );
    app.hits.borrow_mut().push(HitZone {
        x: header[1].x,
        y: header[1].y,
        w: header[1].width,
        h: header[1].height,
        hit: Hit::Commands,
    });

    match app.tab {
        Tab::Chat => draw_chat(f, app, layout.content),
        Tab::Tasks => draw_tasks(f, app, layout.content),
        Tab::Changes => draw_changes(f, app, layout.content),
        Tab::Usage => draw_usage(f, app, layout.content),
        Tab::Settings => draw_settings(f, app, layout.content),
    }
    if let Some(side) = layout.sidebar {
        draw_sidebar(f, app, side);
    }
    if app.tab == Tab::Chat {
        let r = layout.composer;
        let inner = panel(app).inner(r);
        let height = inner.height as usize;
        let top = layout
            .input
            .cursor_row
            .saturating_sub(height.saturating_sub(1));
        let lines: Vec<Line> = if app.input.is_empty() {
            vec![Line::from(Span::styled(
                "Ask Sui to build, fix, or investigate…",
                dim(app),
            ))]
        } else {
            layout
                .input
                .lines
                .iter()
                .skip(top)
                .take(height)
                .cloned()
                .map(Line::from)
                .collect()
        };
        let mut composer = panel(app)
            .border_style(Style::default().fg(if app.nav {
                app.theme().border
            } else {
                app.theme().accent
            }))
            .title(Span::styled(
                if app.running {
                    " Draft · run in progress "
                } else {
                    " Message "
                },
                dim(app),
            ));
        if layout.input.lines.len() > height && height > 0 {
            composer = composer.title_bottom(
                Line::from(Span::styled(
                    format!(
                        " ↑ {top} · ↓ {} ",
                        layout.input.lines.len().saturating_sub(top + height)
                    ),
                    dim(app),
                ))
                .right_aligned(),
            );
        }
        f.render_widget(Paragraph::new(lines).block(composer), r);
        app.hits.borrow_mut().push(HitZone {
            x: r.x,
            y: r.y,
            w: r.width,
            h: r.height,
            hit: Hit::Input,
        });
        let role = if app.mode == Mode::Mission {
            Role::Orchestrator
        } else {
            Role::Solo
        };
        let profile = app
            .role_profile(role)
            .unwrap_or_else(|| "no profile — open Settings".into());
        let model = app
            .profiles
            .get(&profile)
            .and_then(|p| p.model.as_deref())
            .unwrap_or("—");
        f.render_widget(
            Paragraph::new(format!("  {profile} · {model}  /  {mode}")).style(dim(app)),
            layout.metadata,
        );
        if app.modal.is_none() && !app.nav && inner.width > 0 && inner.height > 0 {
            f.set_cursor_position((
                inner.x + layout.input.cursor_col.min(inner.width as usize - 1) as u16,
                inner.y + layout.input.cursor_row.saturating_sub(top).min(height - 1) as u16,
            ));
        }
    }
    draw_footer(f, app, layout.footer);
    if let Some(m) = &app.modal {
        // Keep context visible, but give the active dialog visual priority.
        for cell in &mut f.buffer_mut().content {
            cell.set_fg(app.theme().muted);
        }
        draw_modal(f, app, m, area);
    }
    // No clipped or invisible control may remain clickable after a resize.
    for hit in app.hits.borrow_mut().iter_mut() {
        let r = Rect::new(hit.x, hit.y, hit.w, hit.h).intersection(area);
        hit.x = r.x;
        hit.y = r.y;
        hit.w = r.width;
        hit.h = r.height;
    }
}

fn draw_footer(f: &mut Frame, app: &App, area: Rect) {
    let compact = area.width < 100;
    let hint = if app.nav && app.tab == Tab::Chat {
        if compact {
            " ↑↓ select · Enter expand · v details · Esc input"
        } else {
            " ↑↓ select · Enter expand · v details · Esc input · End live"
        }
    } else if app.running {
        " Ctrl+S stop · Ctrl+P commands"
    } else if app.tab == Tab::Chat {
        if compact {
            " Enter send · ^N newline · ^P commands"
        } else {
            " Enter send · Ctrl+N newline · Ctrl+P commands"
        }
    } else {
        " Ctrl+T views · Ctrl+P commands · Esc chat"
    };
    let status = if app.scroll > 0 && app.tab == Tab::Chat {
        format!("↑ {} rows · End live", app.scroll)
    } else if !app.status.is_empty() {
        super::transcript::clean(&app.status).replace('\n', " ")
    } else if !app.outcome.is_empty() && !app.running {
        format!(
            "Run ended · {}",
            super::transcript::clean(&app.outcome).replace('\n', " ")
        )
    } else {
        String::new()
    };
    let width = area.width as usize;
    let status_width = if status.is_empty() {
        0
    } else {
        unicode_width::UnicodeWidthStr::width(status.as_str()).min(width / 2)
    };
    let hint_width = width.saturating_sub(status_width + usize::from(status_width > 0));
    f.render_widget(
        Paragraph::new(ellipsize(hint, hint_width)).style(dim(app)),
        Rect::new(area.x, area.y, hint_width as u16, area.height),
    );
    f.render_widget(
        Paragraph::new(ellipsize(&status, status_width))
            .style(if app.status.is_empty() {
                dim(app)
            } else {
                Style::default().fg(app.theme().warning)
            })
            .alignment(ratatui::layout::Alignment::Right),
        Rect::new(
            area.right().saturating_sub(status_width as u16),
            area.y,
            status_width as u16,
            area.height,
        ),
    );
}

fn draw_chat(f: &mut Frame, app: &App, a: Rect) {
    let (inner_w, inner_h) = (a.width as usize, a.height as usize);
    app.view_w.set(inner_w);
    app.view_h.set(inner_h);
    if app.groups.len() == 1 && app.groups[0].items.is_empty() && !app.running {
        let r = centered(60, 8, a);
        f.render_widget(
            Paragraph::new(vec![
                Line::from(Span::styled("S U I", acc(app).add_modifier(Modifier::BOLD))),
                Line::from(""),
                Line::from("What are we building?"),
                Line::from(""),
                Line::from(Span::styled("Type a task below. Enter sends it.", dim(app))),
                Line::from(Span::styled(
                    "Ctrl+P opens commands, views, and settings.",
                    dim(app),
                )),
            ])
            .alignment(ratatui::layout::Alignment::Center),
            r,
        );
        return;
    }
    let rows = super::transcript::rows(app, inner_w);
    let top = rows
        .len()
        .saturating_sub(inner_h)
        .saturating_sub(app.scroll);
    app.chat_geom.set(ChatGeom {
        x: a.x,
        y: a.y,
        w: a.width,
        h: a.height,
        top,
    });
    for (vi, r) in rows.iter().skip(top).take(inner_h).enumerate() {
        app.hits.borrow_mut().push(HitZone {
            x: a.x,
            y: a.y + vi as u16,
            w: a.width,
            h: 1,
            hit: Hit::Activity(r.owner.0, r.owner.1),
        });
    }
    let mut view: Vec<Line> = rows
        .into_iter()
        .skip(top)
        .take(inner_h)
        .map(|r| r.line)
        .collect();
    if let Some((r0, c0, r1, c1)) = app.sel {
        for (vi, line) in view.iter_mut().enumerate() {
            let ri = top + vi;
            if ri >= r0 && ri <= r1 {
                *line = super::transcript::paint_sel(
                    std::mem::take(line),
                    if ri == r0 { c0 } else { 0 },
                    if ri == r1 { c1 } else { usize::MAX },
                    app.theme().selected(),
                );
            }
        }
    }
    // Line styles otherwise color only occupied glyphs. Paint the row surface
    // separately so fenced code forms a rectangle without padding copied text.
    for (row, line) in view.iter().enumerate() {
        if line.style.bg.is_some() {
            f.render_widget(
                Block::default().style(line.style),
                Rect::new(a.x, a.y + row as u16, a.width, 1),
            );
        }
    }
    f.render_widget(Paragraph::new(view), a);
}

// Secondary views keep a bounded projection, with explicit truncation and
// their own scroll state. Wrap before measuring so narrow screens can reach
// every visible column as well as every row.
const PANEL_ROW_CAP: usize = 2000;

fn panel_text(rows: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    if rows.len() > PANEL_ROW_CAP {
        return;
    }
    let cleaned = super::transcript::clean(text);
    for source in cleaned.split('\n') {
        for line in super::transcript::wrap(source, width.max(1)) {
            if rows.len() == PANEL_ROW_CAP {
                rows.push(Line::from(ellipsize(
                    "… panel preview limited to 2000 rows",
                    width,
                )));
                return;
            }
            rows.push(Line::from(line).style(style));
        }
    }
}

fn draw_scrolled_panel(f: &mut Frame, app: &App, a: Rect, title: &str, rows: Vec<Line<'static>>) {
    let inner = panel(app).inner(a);
    let index = app.panel_scroll_index().expect("secondary panel");
    let max = rows.len().saturating_sub(inner.height as usize);
    let offset = app.panel_scroll[index].get().min(max);
    app.panel_scroll[index].set(offset);
    app.panel_max_scroll[index].set(max);
    let mut block = panel(app).title(format!(" {title} "));
    if max > 0 {
        block = block.title_bottom(
            Line::from(Span::styled(
                format!(
                    " ↑↓ PgUp/PgDn · {}–{}/{} ",
                    offset + 1,
                    (offset + inner.height as usize).min(rows.len()),
                    rows.len()
                ),
                dim(app),
            ))
            .right_aligned(),
        );
    }
    f.render_widget(
        Paragraph::new(rows.into_iter().skip(offset).collect::<Vec<_>>()).block(block),
        a,
    );
}

fn draw_tasks(f: &mut Frame, app: &App, a: Rect) {
    let width = a.width.saturating_sub(2) as usize;
    let mut rows = Vec::new();
    if app.tasks.is_empty() {
        panel_text(
            &mut rows,
            "no plan yet — mission plans appear here",
            width,
            dim(app),
        );
    }
    for task in &app.tasks {
        panel_text(
            &mut rows,
            &format!("{} · {}", task.id, task.status),
            width,
            acc(app),
        );
        let detail = if task.sha.is_empty() {
            format!("  {}", task.owned)
        } else {
            format!("  {} · {}", task.owned, task.sha)
        };
        panel_text(&mut rows, &detail, width, dim(app));
    }
    draw_scrolled_panel(f, app, a, "tasks", rows);
}

fn draw_changes(f: &mut Frame, app: &App, a: Rect) {
    let width = a.width.saturating_sub(2) as usize;
    let mut rows = Vec::new();
    if app.changes.is_empty() {
        panel_text(&mut rows, "no changes recorded", width, dim(app));
    }
    for change in &app.changes {
        panel_text(&mut rows, change, width, Style::default());
    }
    if let Some(sha) = &app.accepted_sha {
        panel_text(
            &mut rows,
            &format!("accepted: {sha}"),
            width,
            Style::default().fg(app.theme().success),
        );
    }
    if let Some(audit) = &app.audit {
        panel_text(&mut rows, "\naudit:", width, acc(app));
        panel_text(&mut rows, audit, width, Style::default());
    }
    if !app.diff_text.is_empty() {
        panel_text(
            &mut rows,
            "\nworking tree vs HEAD (mission edits live on sui-mission-* branches):",
            width,
            acc(app),
        );
        panel_text(&mut rows, &app.diff_text, width, Style::default());
    }
    draw_scrolled_panel(f, app, a, "changes", rows);
}

fn draw_usage(f: &mut Frame, app: &App, a: Rect) {
    let width = a.width.saturating_sub(2) as usize;
    let mut rows = Vec::new();
    if app.usage.is_empty() {
        panel_text(&mut rows, "no usage yet", width, dim(app));
    }
    for ((agent, model), usage) in &app.usage {
        panel_text(&mut rows, &format!("{agent} · {model}"), width, acc(app));
        let measures = [
            format!("requests {}", usage.requests),
            format!("in {}", usage.input.display(usage.requests)),
            format!("cached {}", usage.cache_read.display(usage.requests)),
            format!("wr {}", usage.cache_write.display(usage.requests)),
            format!("out {}", usage.output.display(usage.requests)),
        ];
        let mut line = String::new();
        for measure in measures {
            let next = if line.is_empty() {
                measure.clone()
            } else {
                format!("{line} · {measure}")
            };
            if !line.is_empty() && unicode_width::UnicodeWidthStr::width(next.as_str()) > width {
                panel_text(&mut rows, &line, width, Style::default());
                line = measure;
            } else {
                line = next;
            }
        }
        panel_text(&mut rows, &line, width, Style::default());
        panel_text(&mut rows, "", width, Style::default());
    }
    panel_text(
        &mut rows,
        "costs: — when pricing unknown (never shown as zero)",
        width,
        dim(app),
    );
    draw_scrolled_panel(f, app, a, "usage", rows);
}

fn draw_settings(f: &mut Frame, app: &App, a: Rect) {
    let ok = Style::default().fg(app.theme().success);
    let warn = Style::default().fg(app.theme().warning);
    let on = Style::default().fg(app.theme().mission);
    const LABEL_W: usize = 16;
    fn strip_scheme(u: &str) -> &str {
        u.trim_start_matches("https://")
            .trim_start_matches("http://")
            .trim_end_matches('/')
    }

    let rows = app.settings_rows();
    // a selection parked on a header renders as the next real row —
    // settings_move normalizes it on the next keypress
    let sel = (app.settings_sel..rows.len())
        .find(|&i| !rows[i].is_header())
        .unwrap_or(app.settings_sel);

    // keep the selection visible: rows above `offset` scroll out of view
    let visible = (a.height as usize).saturating_sub(2).max(1);
    let offset = if sel >= visible { sel + 1 - visible } else { 0 };

    let mut items: Vec<ListItem> = vec![];
    for (i, row) in rows.iter().enumerate().skip(offset) {
        if items.len() >= visible {
            break;
        }
        // section headers: accent name + rule, never selectable/clickable
        if let SettingsRow::Header(name) = row {
            let rule = (a.width as usize).saturating_sub(name.len() + 6);
            items.push(ListItem::new(Line::from(vec![
                Span::styled(format!(" {name} "), acc(app).add_modifier(Modifier::BOLD)),
                Span::styled("─".repeat(rule), dim(app)),
            ])));
            continue;
        }

        // (label, [value spans]) — label column is fixed so values align
        let label: String;
        let mut spans: Vec<Span> = vec![];
        match row {
            SettingsRow::AddProfile => {
                label = "+ add provider".into();
                spans.push(Span::styled(
                    "api-key or ChatGPT sign-in".to_string(),
                    dim(app),
                ));
            }
            SettingsRow::EditProfile(n) => {
                let p = &app.profiles[n];
                label = n.clone();
                if p.kind.as_deref() == Some("codex-oauth") {
                    // OAuth session IS the credential — never render as a
                    // keyless API-key profile.
                    spans.push(Span::styled(
                        format!("ChatGPT OAuth · {}", p.model.as_deref().unwrap_or("—")),
                        Style::default(),
                    ));
                    let session = crate::codex::CodexAuth::session_exists();
                    spans.push(Span::styled(
                        if session {
                            "   session ✓".to_string()
                        } else {
                            "   no session — `codex login` or `sui auth`".to_string()
                        },
                        if session { ok } else { warn },
                    ));
                } else {
                    let has_key = app.session_keys.contains_key(n)
                        || p.key_env
                            .as_deref()
                            .map(|e| std::env::var(e).is_ok())
                            .unwrap_or(false)
                        || p.api_key.is_some();
                    spans.push(Span::styled(
                        format!(
                            "{} · {}",
                            p.base_url.as_deref().map(strip_scheme).unwrap_or("?"),
                            p.model.as_deref().unwrap_or("—")
                        ),
                        Style::default(),
                    ));
                    spans.push(Span::styled(
                        if has_key {
                            "   key ✓"
                        } else {
                            "   key MISSING"
                        },
                        if has_key { ok } else { warn },
                    ));
                }
            }
            SettingsRow::Role(r) => {
                label = r.name().to_lowercase();
                let resolved = app.role_profile(*r);
                let explicit = match r {
                    Role::Solo => &app.ui.solo_profile,
                    Role::Orchestrator => &app.ui.orchestrator_profile,
                    Role::Worker => &app.ui.worker_profile,
                    Role::Auditor => &app.ui.auditor_profile,
                };
                match resolved {
                    Some(p) => {
                        spans.push(Span::styled(p, Style::default()));
                        if explicit.is_none() {
                            spans.push(Span::styled(
                                if *r == Role::Auditor {
                                    "  (via orchestrator)"
                                } else {
                                    "  (auto)"
                                },
                                dim(app),
                            ));
                        }
                    }
                    None => spans.push(Span::styled("—".to_string(), dim(app))),
                }
            }
            SettingsRow::Mode => {
                label = "run mode".into();
                let mission = app.mode == crate::tui::app::Mode::Mission;
                spans.push(Span::styled(
                    if mission { "mission" } else { "solo" },
                    if mission { on } else { Style::default() },
                ));
                spans.push(Span::styled("   Enter toggles".to_string(), dim(app)));
            }
            SettingsRow::Workers => {
                label = "worker slots".into();
                spans.push(Span::raw(app.ui.worker_count.unwrap_or(1).to_string()));
            }
            SettingsRow::Auto => {
                label = "auto-approve".into();
                let yolo = app.auto.load(std::sync::atomic::Ordering::Relaxed);
                spans.push(Span::styled(
                    if yolo { "on" } else { "off" },
                    if yolo { on } else { Style::default() },
                ));
            }
            SettingsRow::Acceptance => {
                label = "acceptance cmds".into();
                spans.push(Span::raw(app.ui.acceptance.len().to_string()));
            }
            SettingsRow::Export => {
                label = "export run".into();
                spans.push(Span::styled(
                    "→ exports/<run>/report.md".to_string(),
                    dim(app),
                ));
            }
            SettingsRow::Theme => {
                label = "theme".into();
                spans.push(Span::raw(super::theme::Theme::name(
                    app.ui.theme.as_deref(),
                )));
            }
            SettingsRow::Reasoning => {
                label = "reasoning".into();
                spans.push(Span::raw(app.reasoning.name()));
            }
            SettingsRow::Mouse => {
                label = "mouse".into();
                spans.push(Span::styled(
                    if app.mouse { "on" } else { "off" },
                    if app.mouse {
                        Style::default()
                    } else {
                        dim(app)
                    },
                ));
            }
            SettingsRow::WebAccess => {
                label = "access".into();
                spans.push(Span::styled(
                    app.web_access.name(),
                    match app.web_access {
                        crate::web::WebAccess::Off => dim(app),
                        crate::web::WebAccess::Auto => on,
                        _ => Style::default(),
                    },
                ));
            }
            SettingsRow::WebKey => {
                label = "exa api key".into();
                if app.web_key.is_some() {
                    spans.push(Span::styled("set".to_string(), ok));
                } else if let Some(e) = &app.web_key_env {
                    spans.push(Span::styled(format!("env {e}"), ok));
                } else {
                    spans.push(Span::styled("none".to_string(), dim(app)));
                }
            }
            SettingsRow::WebTest => {
                label = "test search".into();
                spans.push(Span::styled("run one query".to_string(), dim(app)));
            }
            SettingsRow::Workspace => {
                label = "directory".into();
                spans.push(Span::raw(app.workspace.display().to_string()));
            }
            SettingsRow::Header(_) => unreachable!(),
        }

        let mut line = vec![Span::styled(
            format!("  {label:<LABEL_W$} "),
            Style::default(),
        )];
        line.extend(spans);
        items.push(ListItem::new(Line::from(line)).style(if i == sel {
            app.theme().selected()
        } else {
            Style::default()
        }));
        // click zone per visible row — y accounts for the scroll offset
        let row_y = (i - offset) as u16;
        if row_y + 1 < a.height {
            app.hits.borrow_mut().push(HitZone {
                x: a.x + 1,
                y: a.y + 1 + row_y,
                w: a.width.saturating_sub(2),
                h: 1,
                hit: Hit::Setting(i),
            });
        }
    }

    // selected row explains itself in the bottom border
    let hint = rows.get(sel).map(|r| r.hint()).unwrap_or("");
    f.render_widget(
        List::new(items).block(
            panel(app).title(" settings ").title_bottom(
                Line::from(Span::styled(format!(" {hint} "), dim(app))).right_aligned(),
            ),
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
            Span::styled(prof.unwrap_or("—".into()), dim(app)),
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
    f.render_widget(List::new(agents).block(panel(app).title("agents")), v[0]);

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
        Line::from(Span::styled(format!("run: {run_id}"), dim(app))),
        Line::from(Span::styled("export: /export or sui export", dim(app))),
    ];
    f.render_widget(
        Paragraph::new(run_lines).block(panel(app).title("run")),
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

/// A single-line field keeps its caret visible without exposing secret text.
/// Character offsets remain aligned with Buf, while clipping uses graphemes.
fn field_window(buf: &Buf, width: usize, masked: bool) -> (String, u16) {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let width = width.max(1);
    let text: String = buf
        .text()
        .chars()
        .map(|c| {
            if masked {
                '•'
            } else if c == '\n' {
                '↵'
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();
    let mut char_offset = 0;
    let mut column = 0;
    let mut caret = 0;
    let graphemes: Vec<_> = text
        .graphemes(true)
        .map(|g| {
            if char_offset <= buf.cursor {
                caret = column;
            }
            let start = column;
            char_offset += g.chars().count();
            column += UnicodeWidthStr::width(g);
            (g, start, column)
        })
        .collect();
    if buf.cursor >= char_offset {
        caret = column;
    }
    let start = graphemes
        .iter()
        .map(|(_, start, _)| *start)
        .find(|start| caret.saturating_sub(*start) < width)
        .unwrap_or(caret);
    let shown = graphemes
        .iter()
        .filter(|(_, from, to)| *from >= start && *to <= start + width)
        .map(|(g, _, _)| *g)
        .collect();
    (shown, caret.saturating_sub(start).min(width - 1) as u16)
}

fn draw_modal(f: &mut Frame, app: &App, m: &Modal, area: Rect) {
    match m {
        Modal::Commands { filter, sel } => {
            let matches = super::commands::Command::matching(&filter.text());
            let r = centered(68, (matches.len().max(1) as u16 + 4).min(19), area);
            f.render_widget(Clear, r);
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            let inner = panel(app).inner(r);
            let visible = inner.height.saturating_sub(2) as usize;
            let offset = sel.saturating_sub(visible.saturating_sub(1));
            let query = filter.view(inner.width.saturating_sub(2).max(1) as usize);
            let mut lines = vec![
                Line::from(Span::styled(
                    if filter.is_empty() {
                        "⌕ Search commands…".into()
                    } else {
                        format!(
                            "> {}",
                            query
                                .lines
                                .get(query.cursor_row)
                                .cloned()
                                .unwrap_or_default()
                        )
                    },
                    dim(app),
                )),
                Line::from(""),
            ];
            if matches.is_empty() {
                lines.push(Line::from(Span::styled("No matching commands", dim(app))));
            }
            for (i, command) in matches.iter().enumerate().skip(offset).take(visible) {
                let reason = command.unavailable(app);
                let row_width = inner.width as usize;
                let suffix = reason.unwrap_or(command.shortcut());
                let suffix_width = unicode_width::UnicodeWidthStr::width(suffix).min(row_width / 2);
                let label_width = row_width.saturating_sub(suffix_width + 2);
                let label = ellipsize(
                    &format!("{} {}", if i == *sel { "›" } else { " " }, command.label()),
                    label_width,
                );
                let padding = row_width.saturating_sub(
                    unicode_width::UnicodeWidthStr::width(label.as_str()) + suffix_width,
                );
                let selected = i == *sel;
                let row_style = if selected {
                    app.theme().selected()
                } else if reason.is_some() {
                    dim(app)
                } else {
                    Style::default()
                };
                lines.push(
                    Line::from(vec![
                        Span::raw(label),
                        Span::raw(" ".repeat(padding)),
                        Span::styled(
                            ellipsize(suffix, suffix_width),
                            if selected { row_style } else { dim(app) },
                        ),
                    ])
                    .style(row_style),
                );
                app.hits.borrow_mut().push(HitZone {
                    x: inner.x,
                    y: inner.y + 2 + (i - offset) as u16,
                    w: inner.width,
                    h: 1,
                    hit: Hit::Command(*command),
                });
            }
            f.render_widget(
                Paragraph::new(lines).block(
                    panel(app)
                        .title(if app.pending_perms.is_empty() {
                            " Commands "
                        } else {
                            " Commands · permission waiting · Esc to review "
                        })
                        .title_bottom(" ↑↓ choose · Enter open · Esc back "),
                ),
                r,
            );
            if inner.width > 2 && inner.height > 0 {
                f.set_cursor_position((
                    inner.x + 2 + query.cursor_col.min(inner.width as usize - 3) as u16,
                    inner.y,
                ));
            }
        }
        Modal::Permission {
            id, agent, summary, ..
        } => {
            let preview_width = area.width.min(76).saturating_sub(2).max(1) as usize;
            let cleaned = super::transcript::clean(summary);
            let preview_rows = cleaned
                .lines()
                .flat_map(|line| super::transcript::wrap(line, preview_width))
                .take(8)
                .count()
                .max(1) as u16;
            let button_rows = if preview_width < 44 { 3 } else { 1 };
            let r = centered(76, preview_rows + button_rows + 3, area);
            let more = app.pending_perms.len();
            let title = format!(
                " Permission · {agent}{} ",
                if more > 0 {
                    format!(" · +{more} pending")
                } else {
                    String::new()
                }
            );
            let inner = panel(app).inner(r);
            let stacked = inner.width < 44;
            let buttons_h = if stacked { 3 } else { 1 };
            let body_h = inner.height.saturating_sub(buttons_h + 1);
            let body = Rect::new(inner.x, inner.y, inner.width, body_h);
            let mut lines: Vec<Line> = cleaned
                .lines()
                .flat_map(|l| super::transcript::wrap(l, inner.width.max(1) as usize))
                .take(201)
                .map(Line::from)
                .collect();
            if lines.len() > 200 {
                lines.truncate(200);
                lines.push(Line::from("… preview limited; full command in run export"));
            }
            let max = lines.len().saturating_sub(body_h as usize);
            let scroll = app.dialog_scroll.get().min(max);
            app.dialog_scroll.set(scroll);
            app.dialog_max_scroll.set(max);
            f.render_widget(Clear, r);
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            f.render_widget(
                panel(app)
                    .title(title)
                    .title_bottom(" ↑↓ scroll · Ctrl+S stop · Ctrl+Q quit "),
                r,
            );
            f.render_widget(
                Paragraph::new(lines.into_iter().skip(scroll).collect::<Vec<_>>()),
                body,
            );
            if max > 0 && inner.height > buttons_h {
                f.render_widget(
                    Paragraph::new(format!("↑↓ review · {}/{}", scroll + 1, max + 1))
                        .style(dim(app)),
                    Rect::new(inner.x, inner.y + body_h, inner.width, 1),
                );
            }
            let mut bx = inner.x;
            for (i, (label, choice)) in [
                ("[y/Y] once", GateChoice::Once),
                ("[a/A] session", GateChoice::Session),
                ("[n/N/Esc] deny", GateChoice::Deny),
            ]
            .into_iter()
            .enumerate()
            {
                let by = inner.y + body_h + 1 + if stacked { i as u16 } else { 0 };
                let button = Rect::new(bx, by, label.len() as u16, 1).intersection(inner);
                if button.width == label.len() as u16 && button.height == 1 {
                    f.render_widget(Paragraph::new(label).style(acc(app)), button);
                    app.hits.borrow_mut().push(HitZone {
                        x: button.x,
                        y: button.y,
                        w: button.width,
                        h: 1,
                        hit: Hit::Perm(choice, *id),
                    });
                }
                if !stacked {
                    bx += label.len() as u16 + 3;
                }
            }
        }
        Modal::Help => {
            let r = centered(74, 16, area);
            f.render_widget(Clear, r);
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            let text = "Ctrl+P  Commands and views
Ctrl+T  Cycle views · Esc returns to Chat
Ctrl+O  Solo / Mission · Ctrl+B sidebar
Ctrl+R  Reasoning display
Enter   Send task · Ctrl+N newline
Ctrl+S  Stop task · Ctrl+Q quit
PgUp/PgDn scroll · End returns to live
↑ recalls a task when the composer is empty

Activity
Tab focuses transcript · ↑↓ selects
Enter/Space expands · v opens details
Esc/Tab returns to the composer

Mouse
Wheel scrolls · click expands · drag copies
Shift+drag uses native terminal selection
Settings → mouse turns capture on/off

Commands
/mission /solo /export /help
Settings → theme: dark or terminal

Approvals remain per action. Auto-approve
never enables web access or external agents.";
            let inner = panel(app).inner(r);
            let lines = super::transcript::wrap(text, inner.width.max(1) as usize);
            let max = lines.len().saturating_sub(inner.height as usize);
            let scroll = app.dialog_scroll.get().min(max);
            app.dialog_scroll.set(scroll);
            app.dialog_max_scroll.set(max);
            f.render_widget(
                Paragraph::new(
                    lines
                        .into_iter()
                        .skip(scroll)
                        .map(Line::from)
                        .collect::<Vec<_>>(),
                )
                .block(
                    panel(app)
                        .title(" Help ")
                        .title_bottom(" ↑↓ scroll · Esc close "),
                ),
                r,
            );
        }
        Modal::Provider(pf) => {
            let r = centered(74, 18, area);
            f.render_widget(Clear, r);
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            let fields = pf.fields();
            let mut lines = vec![];
            let mut btn_row: Option<Line> = None;
            let inner = panel(app).inner(r);
            let label_width = 17.min(inner.width.saturating_sub(1) as usize);
            let value_width = (inner.width as usize).saturating_sub(label_width);
            let mut caret = None;
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
                                        app.theme().selected()
                                    } else {
                                        Style::default()
                                    },
                                ));
                            }
                            btn_row = Some(Line::from(row));
                        }
                    }
                    _ => {
                        let editable = match fld {
                            Field::Name => Some(&pf.name),
                            Field::BaseUrl => Some(&pf.base_url),
                            Field::Model => Some(&pf.model),
                            Field::KeyEnv => Some(&pf.key_env),
                            Field::ApiKey => Some(&pf.key),
                            _ => None,
                        };
                        let val = if let Some(buf) = editable {
                            if sel {
                                let (text, col) =
                                    field_window(buf, value_width, *fld == Field::ApiKey);
                                caret = Some((col, i));
                                text
                            } else if *fld == Field::ApiKey {
                                "•".repeat(buf.text().chars().count().min(value_width))
                            } else {
                                ellipsize(
                                    &super::transcript::clean(&buf.text()).replace('\n', "↵"),
                                    value_width,
                                )
                            }
                        } else {
                            match fld {
                                Field::Auth => format!("◀ {} ▶", pf.auth.name()),
                                Field::CredSrc => "Environment variable".to_string(),
                                Field::Store => format!("◀ {} ▶", pf.store.name()),
                                _ => String::new(),
                            }
                        };
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("{:<label_width$}", ellipsize(fld.label(), label_width)),
                                if sel { acc(app) } else { dim(app) },
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
                if pf.endpoint.starts_with("codex://") {
                    format!("→ {}", pf.endpoint)
                } else {
                    format!("→ POST {}", pf.endpoint)
                },
                dim(app),
            )));
            lines.push(Line::from(""));
            if let Some(row) = btn_row {
                lines.push(row);
            }
            if !pf.status.is_empty() {
                lines.push(Line::from(Span::styled(
                    pf.status.clone(),
                    Style::default().fg(app.theme().warning),
                )));
            }
            lines.push(Line::from(Span::styled(
                "test sends one small live request — Store chooses where the key lives",
                dim(app),
            )));
            let value_rows = fields
                .iter()
                .filter(|f| !matches!(f, Field::Test | Field::Save | Field::Cancel))
                .count();
            // The three buttons share one rendered row after endpoint + spacer.
            let focus_row = if pf.focus >= value_rows {
                value_rows + 2
            } else {
                pf.focus
            };
            let offset = focus_row.saturating_sub(r.height.saturating_sub(3) as usize);
            f.render_widget(
                Paragraph::new(lines)
                    .scroll((offset as u16, 0))
                    .block(panel(app).title(pf.ptype.name())),
                r,
            );
            if let Some((col, row)) = caret {
                let y = row.saturating_sub(offset) as u16;
                if value_width > 0 && y < inner.height {
                    f.set_cursor_position((inner.x + label_width as u16 + col, inner.y + y));
                }
            }
        }
        Modal::Picker(p) => {
            let r = centered(70, 16, area);
            f.render_widget(Clear, r);
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            let mut items: Vec<ListItem> =
                vec![ListItem::new(format!("filter: {}", p.filter.text()))];
            if p.loading {
                items.push(ListItem::new(Span::styled("loading…", dim(app))));
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
            let visible = r.height.saturating_sub(3 + u16::from(p.loading)) as usize;
            let offset = p.sel.saturating_sub(visible.saturating_sub(1));
            for (i, it) in list.iter().enumerate().skip(offset).take(visible) {
                items.push(ListItem::new(Line::from(Span::styled(
                    it.clone(),
                    if i == p.sel {
                        app.theme().selected()
                    } else {
                        Style::default()
                    },
                ))));
            }
            f.render_widget(List::new(items).block(panel(app).title(p.title.clone())), r);
        }
        Modal::ConfirmTest { name } => {
            let r = centered(56, 6, area);
            f.render_widget(Clear, r);
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            f.render_widget(
                Paragraph::new(vec![
                    Line::from(format!("probe '{name}' sends one small live request.")),
                    Line::from(Span::styled("[y] proceed   [n] cancel", acc(app))),
                ])
                .block(panel(app).title("test connection")),
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
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ViewScroll,
            });
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
                    panel(app).title(format!("{title} — ↑↓ PgUp/PgDn scroll{more} · Esc close")),
                ),
                r,
            );
        }
        Modal::Text {
            title, buf, target, ..
        } => {
            let r = centered(60, 5, area);
            f.render_widget(Clear, r);
            app.hits.borrow_mut().push(HitZone {
                x: r.x,
                y: r.y,
                w: r.width,
                h: r.height,
                hit: Hit::ModalBody,
            });
            let inner = panel(app).inner(r);
            let (shown, cursor) =
                field_window(buf, inner.width as usize, *target == TextTarget::WebKey);
            f.render_widget(
                Paragraph::new(shown).block(
                    panel(app)
                        .title(title.clone())
                        .title_bottom(" Enter save · Esc cancel "),
                ),
                r,
            );
            if inner.width > 0 && inner.height > 0 {
                f.set_cursor_position((inner.x + cursor, inner.y));
            }
        }
    }
    let _ = app;
}
