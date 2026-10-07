//! Shared geometry for rendering, resize anchors, and input hit testing.
use super::{
    app::{App, Tab},
    text::InputView,
};
use ratatui::layout::{Constraint, Direction, Layout, Rect};

pub struct Regions {
    pub header: Rect,
    pub content: Rect,
    pub sidebar: Option<Rect>,
    /// Runway for the lane slime, directly above the composer.
    pub lane: Option<Rect>,
    pub composer: Rect,
    pub metadata: Rect,
    pub footer: Rect,
    pub input: InputView,
}

/// Terminal rows of the lane the slime runs along: 0 (none) once there is no
/// transcript, where the terminal is too small to spare the rows, or where
/// pixel art cannot be drawn. Tall terminals get headroom for bigger hops.
fn lane_rows(area: Rect, app: &App) -> u16 {
    if app.tab != Tab::Chat
        || area.width < 50
        || super::slime::empty_chat(app)
        || !super::slime::pixel_ok(app.theme())
    {
        return 0;
    }
    match area.height {
        34.. => 4,
        26..=33 => 3,
        _ => 0,
    }
}

pub fn regions(area: Rect, app: &App) -> Regions {
    let side = app.sidebar && area.width >= 110 && area.height >= 12;
    let width = area.width.saturating_sub(if side { 31 } else { 0 });
    let input = app.input.view(width.saturating_sub(4).max(1) as usize);
    let chat = app.tab == Tab::Chat;
    let text_rows = input.lines.len().clamp(1, 6) as u16;
    let composer_height = if chat {
        (text_rows + 2).min(area.height.saturating_sub(5))
    } else {
        0
    };
    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(composer_height),
        Constraint::Length(u16::from(chat && area.height >= 6)),
        Constraint::Length(1),
    ])
    .split(area);
    let body = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(if side { 31 } else { 0 }),
        ])
        .split(rows[1]);
    let mut content = inset(body[0], 1, 1);
    let composer = Rect::new(
        rows[2].x + u16::from(rows[2].width > 0),
        rows[2].y,
        width.saturating_sub(2),
        rows[2].height,
    );
    // The lane takes the blank gutter above the composer plus the rows it
    // needs on top of that, so the transcript never shares a row with it.
    let lane = Some(lane_rows(area, app)).filter(|&n| n > 0).map(|n| {
        content.height = content.height.saturating_sub(n - 1);
        Rect::new(composer.x, composer.y - n, composer.width, n)
    });
    Regions {
        header: rows[0],
        content,
        sidebar: side.then(|| {
            let r = inset(body[1], 0, 1);
            Rect::new(r.x, r.y, r.width.saturating_sub(1), r.height)
        }),
        lane,
        composer,
        metadata: Rect::new(rows[3].x, rows[3].y, width, rows[3].height),
        footer: rows[4],
        input,
    }
}

pub fn inset(r: Rect, x: u16, y: u16) -> Rect {
    let x = x.min(r.width / 2);
    let y = y.min(r.height / 2);
    Rect::new(
        r.x + x,
        r.y + y,
        r.width.saturating_sub(x * 2),
        r.height.saturating_sub(y * 2),
    )
}
