//! Regressions from the interaction audit: exercise public input/state seams.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::{backend::TestBackend, Terminal};
use sui::config::UiSettings;
use sui::events::UiEvent;
use sui::tui::{
    app::{Act, App, Modal, Mode, ReasonPref, SettingsRow, Tab},
    commands::Command,
    draw,
    text::Buf,
    transcript,
};

fn app() -> App {
    let mut app = App::with_state("/tmp".into(), Default::default(), UiSettings::default());
    app.modal = None;
    app.tab = Tab::Chat;
    app.sidebar = false;
    app.on_resize(80, 24);
    app
}
fn key(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}
fn ctrl(app: &mut App, c: char) {
    app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
}
fn notes(app: &mut App) {
    app.groups[0].items = (0..60)
        .map(|i| Act::Note {
            id: i + 100,
            agent: None,
            text: format!("note {i}"),
            err: false,
            at: "12:00".into(),
        })
        .collect();
}
fn draw(app: &App) {
    Terminal::new(TestBackend::new(80, 24))
        .unwrap()
        .draw(|frame| draw::draw(frame, app))
        .unwrap();
}

#[test]
fn editing_recalled_history_preserves_draft_on_down() {
    for edit in 0..4 {
        let mut app = app();
        app.history = vec!["old task".into()];
        key(&mut app, KeyCode::Up);
        match edit {
            0 => app.paste(" with changes"),
            1 => key(&mut app, KeyCode::Backspace),
            2 => {
                key(&mut app, KeyCode::Home);
                key(&mut app, KeyCode::Delete);
            }
            _ => ctrl(&mut app, 'n'),
        }
        let draft = app.input.text();
        assert_eq!(app.hist_i, None);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.input.text(), draft);
    }
}

#[test]
fn activity_navigation_keeps_the_selected_row_visible() {
    let mut app = app();
    notes(&mut app);
    draw(&app);
    key(&mut app, KeyCode::Tab);
    for _ in 0..55 {
        key(&mut app, KeyCode::Up);
        let (group, item) = app.focusables()[app.nav_sel];
        let owner = (
            app.groups[group].id,
            item.map(|i| app.groups[group].items[i].id()),
        );
        let rows = transcript::rows(&app, app.view_w.get());
        let row = rows.iter().position(|row| row.owner == owner).unwrap();
        let top = rows
            .len()
            .saturating_sub(app.view_h.get())
            .saturating_sub(app.scroll);
        assert!((top..top + app.view_h.get()).contains(&row));
    }
}

#[test]
fn focus_list_excludes_hidden_reasoning() {
    let mut app = app();
    app.reasoning = ReasonPref::Hidden;
    app.groups[0].items = vec![Act::Reason {
        id: 10,
        agent: "solo".into(),
        req: 1,
        text: "hidden".into(),
        done: true,
        expanded: false,
        at: "12:00".into(),
    }];
    assert!(app.focusables().is_empty());
}

#[test]
fn scrolling_reverses_immediately_at_content_boundaries() {
    let mut app = app();
    notes(&mut app);
    draw(&app);
    let max = transcript::rows(&app, app.view_w.get()).len() - app.view_h.get();
    app.scroll_by(10_000);
    assert_eq!(app.scroll, max);
    app.scroll_by(-1);
    assert_eq!(app.scroll, max - 1);
    app.scroll_by(-10_000);
    assert_eq!(app.scroll, 0);

    app.modal = Some(Modal::View {
        title: "details".into(),
        text: (0..80)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("\n"),
        scroll: 0,
    });
    for _ in 0..30 {
        key(&mut app, KeyCode::PageDown);
    }
    let max = match app.modal {
        Some(Modal::View { scroll, .. }) => scroll,
        _ => unreachable!(),
    };
    assert_eq!(max, 62); // 20-row modal with 18-row inner viewport.
    key(&mut app, KeyCode::Up);
    assert!(matches!(app.modal, Some(Modal::View { scroll, .. }) if scroll == max - 1));
    app.on_resize(80, 48);
    assert!(matches!(app.modal, Some(Modal::View { scroll: 48, .. })));
}

#[test]
fn panel_scrolling_is_independent_and_clamped() {
    let mut app = app();
    for (i, tab) in [Tab::Tasks, Tab::Changes, Tab::Usage]
        .into_iter()
        .enumerate()
    {
        app.command(Command::View(tab));
        app.panel_max_scroll[i].set(100);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.panel_scroll[i].get(), 1);
        key(&mut app, KeyCode::End);
        assert_eq!(app.panel_scroll[i].get(), 100);
        app.mouse(MouseEvent {
            kind: MouseEventKind::ScrollUp,
            column: 3,
            row: 4,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(app.panel_scroll[i].get(), 97);
    }
    assert_eq!(app.scroll, 0);
    app.command(Command::View(Tab::Tasks));
    assert_eq!(app.panel_scroll[0].get(), 97);
    key(&mut app, KeyCode::Home);
    assert_eq!(app.panel_scroll[0].get(), 0);
    assert_eq!(app.panel_scroll[1].get(), 97);
}

#[test]
fn mode_cannot_change_mid_run_from_any_entrypoint() {
    let mut app = app();
    app.mode = Mode::Solo;
    app.running = true;
    ctrl(&mut app, 'o');
    assert_eq!(app.mode, Mode::Solo);
    app.command(Command::Mode(Mode::Mission));
    assert_eq!(app.mode, Mode::Solo);
    app.set_mode(Mode::Mission);
    assert_eq!(app.mode, Mode::Solo);
    let row = app
        .settings_rows()
        .iter()
        .position(|r| matches!(r, SettingsRow::Mode))
        .unwrap();
    app.settings_activate(row);
    assert_eq!(app.mode, Mode::Solo);
    app.paste("/mission");
    key(&mut app, KeyCode::Enter);
    assert_eq!(app.mode, Mode::Solo);
    assert_eq!(app.input.text(), "/mission");
    app.running = false;
    app.input.clear();
    ctrl(&mut app, 'o');
    assert_eq!(app.mode, Mode::Mission);
}

#[test]
fn edits_that_merge_graphemes_keep_caret_on_a_boundary() {
    let mut input = Buf::from("👩👩");
    input.left();
    input.insert('\u{200d}');
    assert_eq!(input.cursor, 3);
    assert_eq!(input.view(20).cursor_col, 2);
    input.backspace();
    assert!(input.is_empty());

    let mut input = Buf::from("👩👩");
    input.left();
    input.insert_str("\u{200d}");
    input.backspace();
    assert!(input.is_empty());

    let mut input = Buf::from("🇺x🇸");
    input.home();
    input.right();
    input.delete();
    assert_eq!(input.text(), "🇺🇸");
    assert_eq!(input.cursor, 2);
    input.backspace();
    assert!(input.is_empty());
}

#[test]
fn completing_a_run_clears_only_transient_run_status() {
    for (before, after) in [
        ("stopping…", ""),
        ("stop the current task first", ""),
        ("run in progress — Ctrl+S stops it; text kept", ""),
        ("failed to export", "failed to export"),
    ] {
        let mut app = app();
        app.status = before.into();
        app.apply_event(UiEvent::RunDone {
            run: 1,
            outcome: "done".into(),
            accepted_sha: None,
        });
        assert_eq!(app.status, after);
    }
}

#[test]
fn view_shortcuts_work_while_activity_has_focus() {
    let mut app = app();
    notes(&mut app);
    app.input.set("keep this draft");
    key(&mut app, KeyCode::Tab);
    assert!(app.nav);
    ctrl(&mut app, 'b');
    assert!(app.sidebar);
    let next = app.reasoning.next();
    ctrl(&mut app, 'r');
    assert_eq!(app.reasoning, next);
    app.running = true;
    let mode = app.mode;
    ctrl(&mut app, 'o');
    assert_eq!(app.mode, mode);
    assert_eq!(app.status, "stop the current task first");
    app.running = false;
    ctrl(&mut app, 'o');
    assert_ne!(app.mode, mode);
    key(&mut app, KeyCode::F(1));
    assert!(matches!(app.modal, Some(Modal::Help)));
    key(&mut app, KeyCode::Esc);
    assert!(app.nav);
    assert_eq!(app.input.text(), "keep this draft");
}

#[test]
fn reasoning_visibility_preserves_selected_visible_owner() {
    let mut app = app();
    notes(&mut app);
    app.groups[0].items.insert(
        0,
        Act::Reason {
            id: 10,
            agent: "solo".into(),
            req: 1,
            text: "reasoning".into(),
            done: true,
            expanded: false,
            at: "12:00".into(),
        },
    );
    app.reasoning = ReasonPref::Auto;
    app.nav = true;
    app.nav_sel = 10;
    let owner = app.focusables()[app.nav_sel];
    app.command(Command::Reasoning);
    assert_eq!(app.reasoning, ReasonPref::Hidden);
    assert_eq!(app.focusables()[app.nav_sel], owner);
    app.command(Command::Reasoning);
    assert_eq!(app.focusables()[app.nav_sel], owner);
}
