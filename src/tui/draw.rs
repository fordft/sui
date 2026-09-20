//! Rendering. Read-only over App — never mutates, never adds data to any
//! model request. Narrow terminals collapse the sidebar instead of
//! crushing the content.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap};
use ratatui::Frame;

use super::app::*;
use crate::events::GateChoice;

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

    // mouse hitmap is rebuilt every frame — zones below register into it
    app.hits.borrow_mut().clear();

    // tab strip — each label is a click zone
    let mut tx = rows[1].x;
    let tabs: Vec<Span> = Tab::ALL
        .iter()
        .map(|t| {
            let label = format!(" {} ", t.name());
            let w = label.chars().count() as u16;
            app.hits.borrow_mut().push(HitZone {
                x: tx,
                y: rows[1].y,
                w,
                h: 1,
                hit: Hit::Tab(*t),
            });
            tx += w;
            if *t == app.tab {
                Span::styled(
                    label,
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                Span::styled(label, dim())
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
    // clicking the input box focuses it (exits transcript nav)
    app.hits.borrow_mut().push(HitZone {
        x: rows[3].x,
        y: rows[3].y,
        w: rows[3].width,
        h: rows[3].height,
        hit: Hit::Input,
    });

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
    // geometry for mouse hit-testing + drag-select mapping
    app.chat_geom.set(ChatGeom {
        x: a.x + 1,
        y: a.y + 1,
        w: inner_w.min(u16::MAX as usize) as u16,
        h: inner_h.min(u16::MAX as usize) as u16,
        top,
    });
    {
        let mut hits = app.hits.borrow_mut();
        for (vi, r) in rows.iter().skip(top).take(inner_h).enumerate() {
            hits.push(HitZone {
                x: a.x + 1,
                y: a.y + 1 + vi as u16,
                w: inner_w.min(u16::MAX as usize) as u16,
                h: 1,
                hit: Hit::Activity(r.owner.0, r.owner.1),
            });
        }
    }
    let mut view: Vec<Line> = rows
        .into_iter()
        .skip(top)
        .take(inner_h)
        .map(|r| r.line)
        .collect();
    // drag-selection highlight — painted over the projected rows
    if let Some((r0, c0, r1, c1)) = app.sel {
        let hl = Style::default().bg(Color::DarkGray);
        for (vi, line) in view.iter_mut().enumerate() {
            let ri = top + vi;
            if ri < r0 || ri > r1 {
                continue;
            }
            let s = if ri == r0 { c0 } else { 0 };
            let e = if ri == r1 { c1 } else { usize::MAX };
            *line = super::transcript::paint_sel(std::mem::take(line), s, e, hl);
        }
    }
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
    let ok = Style::default().fg(Color::Green);
    let warn = Style::default().fg(Color::Yellow);
    let on = Style::default().fg(Color::Magenta);
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
                Span::styled(format!(" {name} "), acc().add_modifier(Modifier::BOLD)),
                Span::styled("─".repeat(rule), dim()),
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
                    dim(),
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
                                dim(),
                            ));
                        }
                    }
                    None => spans.push(Span::styled("—".to_string(), dim())),
                }
            }
            SettingsRow::Mode => {
                label = "run mode".into();
                let mission = app.mode == crate::tui::app::Mode::Mission;
                spans.push(Span::styled(
                    if mission { "mission" } else { "solo" },
                    if mission { on } else { Style::default() },
                ));
                spans.push(Span::styled("   Enter toggles".to_string(), dim()));
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
                spans.push(Span::styled("→ exports/<run>/report.md".to_string(), dim()));
            }
            SettingsRow::Reasoning => {
                label = "reasoning".into();
                spans.push(Span::raw(app.reasoning.name()));
            }
            SettingsRow::Mouse => {
                label = "mouse".into();
                spans.push(Span::styled(
                    if app.mouse { "on" } else { "off" },
                    if app.mouse { Style::default() } else { dim() },
                ));
            }
            SettingsRow::WebAccess => {
                label = "access".into();
                spans.push(Span::styled(
                    app.web_access.name(),
                    match app.web_access {
                        crate::web::WebAccess::Off => dim(),
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
                    spans.push(Span::styled("none".to_string(), dim()));
                }
            }
            SettingsRow::WebTest => {
                label = "test search".into();
                spans.push(Span::styled("run one query".to_string(), dim()));
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
            Style::default().bg(Color::DarkGray)
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
            Block::default()
                .borders(Borders::ALL)
                .title(" settings ")
                .title_bottom(Line::from(Span::styled(format!(" {hint} "), dim())).right_aligned()),
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
        Modal::Permission {
            id, agent, summary, ..
        } => {
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
            // heredocs/very long commands show head lines + a marker.
            // clean() first: the summary is model/ACP-generated text and
            // raw control sequences would inject escapes into the modal
            // (e.g. repainting the screen to hide the real command).
            let sl: Vec<String> = summary.lines().map(crate::tui::transcript::clean).collect();
            // leave room for the marker + blank + button row — a clipped
            // button row makes the modal undecidable by mouse
            let inner_h = r.height.saturating_sub(2) as usize;
            let show = inner_h.saturating_sub(3).clamp(1, 8);
            let mut lines: Vec<Line> = sl
                .iter()
                .take(show)
                .map(|l| Line::from(l.clone()))
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
            let btn_text = "[y/Y] once   [a/A] session   [n/N/Esc] deny";
            lines.push(Line::from(Span::styled(btn_text, acc())));
            // clickable decision buttons — same actions as the y/a/n keys
            {
                let by = r.y + 1 + (lines.len() - 1) as u16;
                let mut hits = app.hits.borrow_mut();
                for (label, choice) in [
                    ("[y/Y] once", GateChoice::Once),
                    ("[a/A] session", GateChoice::Session),
                    ("[n/N/Esc] deny", GateChoice::Deny),
                ] {
                    let off = btn_text.find(label).unwrap_or(0) as u16;
                    hits.push(HitZone {
                        x: r.x + 1 + off,
                        y: by,
                        w: label.len() as u16,
                        h: 1,
                        hit: Hit::Perm(choice, *id),
                    });
                }
            }
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
                    Line::from("mouse:"),
                    Line::from("  wheel scrolls   click selects/expands   drag copies (osc52)"),
                    Line::from("  shift+drag = native terminal select   Settings → mouse toggles"),
                    Line::from("  paste: Ctrl+V / Shift+Insert (right-click paste needs capture off)"),
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
                if pf.endpoint.starts_with("codex://") {
                    format!("→ {}", pf.endpoint)
                } else {
                    format!("→ POST {}", pf.endpoint)
                },
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
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!("{title} — ↑↓ PgUp/PgDn scroll{more} · Esc close")),
                ),
                r,
            );
        }
        Modal::Text {
            title, buf, target, ..
        } => {
            let r = centered(60, 5, area);
            f.render_widget(Clear, r);
            let shown = if *target == TextTarget::WebKey {
                "•".repeat(buf.text().chars().count())
            } else {
                buf.text()
            };
            f.render_widget(
                Paragraph::new(shown)
                    .block(Block::default().borders(Borders::ALL).title(title.clone())),
                r,
            );
        }
    }
    let _ = app;
}
