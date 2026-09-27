use ratatui::{
    backend::TestBackend,
    style::{Color, Modifier},
    Terminal,
};
use sui::{
    config::UiSettings,
    events::ToolStatus,
    tui::{
        app::{Act, App, ReasonPref},
        draw,
        theme::Theme,
        transcript,
    },
};

fn app() -> App {
    let mut app = App::with_state(
        std::env::temp_dir(),
        Default::default(),
        UiSettings::default(),
    );
    app.modal = None;
    app.groups[0].items.clear();
    app
}

fn tool(id: u64, exit: Option<i32>, expanded: bool, result: String) -> Act {
    Act::Tool {
        id,
        agent: "solo".into(),
        call: format!("call-{id}"),
        name: "write_file".into(),
        summary: "README.md".into(),
        status: Some(ToolStatus::Ok),
        exit,
        result,
        truncated: false,
        dropped: 0,
        live: String::new(),
        ms: 3,
        expanded,
        at: String::new(),
    }
}

fn reason(text: String) -> Act {
    Act::Reason {
        id: 10,
        agent: "solo".into(),
        req: 1,
        text,
        done: true,
        expanded: true,
        at: String::new(),
    }
}

#[test]
fn successful_tools_only_display_reported_exit_codes() {
    let mut app = app();
    app.groups[0].items = vec![
        tool(1, None, false, String::new()),
        tool(2, None, false, String::new()),
        tool(3, Some(0), false, String::new()),
    ];
    let rows = transcript::rows(&app, 100);
    let text: Vec<_> = rows.iter().map(|row| row.line.to_string()).collect();
    assert_eq!(
        text.len(),
        2,
        "different exit availability must not be grouped"
    );
    assert!(text[0].contains("write_file · ok ·"));
    assert!(text[0].contains("×2"));
    assert!(!text[0].contains("exit"));
    assert!(text[1].contains("exit 0"));
}

#[test]
fn expanded_reasoning_wraps_without_losing_visible_characters() {
    use unicode_width::UnicodeWidthStr;
    let source = "1234567890界e\u{301}👩‍💻".repeat(8);
    for width in [40, 78] {
        let mut app = app();
        app.groups[0].items.push(reason(source.clone()));
        let rows = transcript::rows(&app, width);
        let body: Vec<_> = rows
            .iter()
            .map(|row| row.line.to_string())
            .filter(|line| line.starts_with("    "))
            .collect();
        assert!(body
            .iter()
            .all(|line| UnicodeWidthStr::width(line.as_str()) <= width));
        assert_eq!(
            body.iter().map(|line| &line[4..]).collect::<String>(),
            source
        );
        let mut terminal = Terminal::new(TestBackend::new(width as u16 + 2, 30)).unwrap();
        terminal.draw(|frame| draw::draw(frame, &app)).unwrap();
        let screen = format!("{}", terminal.backend());
        for line in body {
            assert!(
                screen.contains(&line),
                "visible cells omit part of {line:?}"
            );
        }
    }
}

#[test]
fn bounded_reasoning_and_tool_previews_explain_hidden_content() {
    for count in [200, 201] {
        for reasoning in [true, false] {
            let mut app = app();
            let source = (0..count)
                .map(|_| "retained line")
                .collect::<Vec<_>>()
                .join("\n");
            let item = if reasoning {
                reason(source.clone())
            } else {
                tool(10, None, true, source.clone())
            };
            app.groups[0].items.push(item);
            let rows = transcript::rows(&app, 100);
            let notices: Vec<_> = rows
                .iter()
                .filter(|row| row.line.to_string().contains("display capped"))
                .collect();
            assert_eq!(notices.len(), usize::from(count > 200));
            assert_eq!(
                rows.iter()
                    .filter(|row| row.line.to_string().contains("retained line"))
                    .count(),
                200
            );
            assert!(app.groups[0].items[0].detail().contains(&source));
        }
    }
}

#[test]
fn navigation_targets_match_visible_folded_and_hidden_activity() {
    let mut app = app();
    app.reasoning = ReasonPref::Hidden;
    app.groups[0].items = vec![
        reason("hidden reasoning".into()),
        tool(11, None, false, String::new()),
        tool(12, None, false, String::new()),
        Act::Assistant {
            id: 13,
            agent: "solo".into(),
            req: 1,
            text: "answer".into(),
            done: true,
            at: String::new(),
        },
    ];
    assert_eq!(
        transcript::focusable_items(&app),
        vec![(0, Some(1)), (0, Some(3))]
    );
    app.groups[0].done = true;
    app.groups[0].expanded = false;
    app.groups[0].collapsed = true;
    app.groups[0].task = "task".into();
    assert_eq!(
        transcript::focusable_items(&app),
        vec![(0, None), (0, Some(3))]
    );
}

#[test]
fn terminal_text_uses_native_contrast_and_selection_survives_without_colors() {
    let native = Theme::new(Some("terminal"));
    assert_eq!(native.text, Color::Reset);
    assert_eq!(native.muted, Color::Reset);
    assert_eq!(native.accent, Color::Reset);
    for name in ["terminal", "dark"] {
        let selected = Theme::new(Some(name)).selected();
        assert!(selected.add_modifier.contains(Modifier::UNDERLINED));
        assert!(selected.add_modifier.contains(Modifier::BOLD));
    }
}

#[test]
fn no_color_backend_keeps_selection_visible() {
    const CHILD: &str = "SUI_TEST_NO_COLOR_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "no_color_backend_keeps_selection_visible",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("NO_COLOR", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    use ratatui::{
        backend::{Backend, CrosstermBackend},
        buffer::Cell,
    };
    for name in ["dark", "terminal"] {
        let mut output = Vec::new();
        let mut backend = CrosstermBackend::new(&mut output);
        let mut selected = Cell::default();
        selected
            .set_symbol("X")
            .set_style(Theme::new(Some(name)).selected());
        backend.draw(std::iter::once((0, 0, &selected))).unwrap();
        let bytes = String::from_utf8(output).unwrap();
        assert!(
            bytes.contains("\u{1b}[4m"),
            "selection must emit underline with NO_COLOR: {bytes:?}"
        );
        assert!(
            !bytes.contains("38;") && !bytes.contains("48;"),
            "NO_COLOR suppresses explicit colors: {bytes:?}"
        );
    }
}
