//! Regression cases from the TUI usability audit.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, Terminal};
use sui::config::UiSettings;
use sui::tui::{
    app::{App, AuthMode, Field, Modal, ProvForm, ProvType, Tab, TaskRow, TextTarget},
    draw,
    text::Buf,
    usage::UsageTotals,
};

fn app() -> App {
    let mut app = App::with_state("/tmp".into(), Default::default(), UiSettings::default());
    app.modal = None;
    app
}

fn render(app: &App, w: u16, h: u16) -> (String, Terminal<TestBackend>) {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| draw::draw(f, app)).unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content
        .chunks(w as usize)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    (screen, terminal)
}

fn key(app: &mut App, code: KeyCode) {
    app.key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn secondary_views_reach_their_tail_and_keep_independent_positions() {
    let mut app = app();
    app.tab = Tab::Tasks;
    app.tasks = (0..40)
        .map(|i| TaskRow {
            id: format!("task_{i}"),
            status: "done".into(),
            owned: format!("file_{i}"),
            sha: "abcdef".into(),
        })
        .collect();
    let (first, _) = render(&app, 40, 12);
    assert!(first.contains("PgUp/PgDn"));
    key(&mut app, KeyCode::End);
    let (last, _) = render(&app, 40, 12);
    assert!(last.contains("file_39"));
    let task_offset = app.panel_scroll[0].get();
    app.tab = Tab::Changes;
    app.audit = Some(
        (0..24)
            .map(|i| format!("audit_{i:02}"))
            .collect::<Vec<_>>()
            .join("\n"),
    );
    app.diff_text = (0..40)
        .map(|i| format!("diff_{i:02}"))
        .collect::<Vec<_>>()
        .join("\n");
    let (first, _) = render(&app, 80, 24);
    assert!(first.contains("audit_00"));
    key(&mut app, KeyCode::End);
    let (last, _) = render(&app, 80, 24);
    assert!(last.contains("diff_39"));
    assert_eq!(app.panel_scroll[0].get(), task_offset);
    let (all, _) = render(&app, 160, 100);
    assert!(all.contains("audit_23"));
    assert!(all.contains("diff_39"));
    assert_eq!(
        app.panel_scroll[1].get(),
        0,
        "resize clamps obsolete offsets"
    );
}

#[test]
fn bounded_panels_explain_truncation() {
    let mut app = app();
    app.tab = Tab::Changes;
    app.audit = Some("audit line\n".repeat(2100));
    render(&app, 80, 24);
    key(&mut app, KeyCode::End);
    let (screen, _) = render(&app, 80, 24);
    assert!(screen.contains("preview limited to 2000 rows"));
    assert!(app.panel_max_scroll[1].get() <= 2001);
}

#[test]
fn usage_keeps_measurements_visible_on_narrow_screens() {
    let mut app = app();
    app.tab = Tab::Usage;
    let mut totals = UsageTotals::default();
    totals.add(Some(100), None, Some(0), Some(77));
    totals.add(None, None, None, Some(23));
    app.usage
        .insert(("worker".into(), "model-long-name".into()), totals);
    for width in [40, 80, 110] {
        let (screen, _) = render(&app, width, 24);
        assert!(screen.contains("model-long-name"));
        assert!(screen.contains("partial"));
        assert!(screen.contains("out 100"));
        assert!(screen.contains("cached —"));
    }
}

#[test]
fn long_settings_fields_show_the_caret_and_secret_values_stay_masked() {
    let mut app = app();
    let mut form = ProvForm::new(ProvType::Custom);
    form.base_url
        .set(&format!("https://{}/界TAIL", "example".repeat(12)));
    form.focus = form
        .fields()
        .iter()
        .position(|f| *f == Field::BaseUrl)
        .unwrap();
    app.modal = Some(Modal::Provider(form));
    let (screen, mut terminal) = render(&app, 40, 12);
    assert!(screen.contains("TAIL"));
    assert!(screen.contains('界'));
    let caret = terminal.get_cursor_position().unwrap();
    assert!(caret.x < 40 && caret.y < 12);
    key(&mut app, KeyCode::Home);
    let (screen, _) = render(&app, 40, 12);
    assert!(screen.contains("https://"));

    let mut form = ProvForm::new(ProvType::Custom);
    form.auth = AuthMode::ApiKey;
    form.key.set("SECRET_should_never_render_0123456789");
    form.focus = form
        .fields()
        .iter()
        .position(|f| *f == Field::ApiKey)
        .unwrap();
    app.modal = Some(Modal::Provider(form));
    let (screen, _) = render(&app, 40, 12);
    assert!(!screen.contains("SECRET"));
    assert!(screen.contains('•'));

    app.modal = Some(Modal::Text {
        title: "Workspace".into(),
        buf: Buf::from(&format!("/{}/界TAIL", "directory".repeat(12))),
        target: TextTarget::Workspace,
    });
    let (screen, mut terminal) = render(&app, 40, 12);
    assert!(screen.contains("TAIL"));
    assert!(screen.contains('界'));
    let caret = terminal.get_cursor_position().unwrap();
    assert!(caret.x < 40 && caret.y < 12);
}
