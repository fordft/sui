//! Motion policy and screen-level effects: flowing border, text shimmer,
//! completion confetti, dialog fade, and the gel backdrop.
//!
//! Bookkeeping lives in `Anim` and is advanced from the event loop
//! (`observe`); `draw` only reads it, so a frame is a pure function of
//! (App, clock). Nothing here touches model traffic or persisted state
//! except the user's own motion preference.
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use std::hash::{Hash, Hasher};

use super::app::App;
use super::gfx::{color, mix, noise, rgb, smooth, Canvas};
use super::slime::{is_home, lerp};
use super::theme::Theme;

/// How much the interface is allowed to move.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Motion {
    /// ~30 fps: hop, splash, bubbles, confetti.
    Full,
    /// ~8 fps, no splash/bubbles/confetti — friendlier over SSH.
    Calm,
    /// Nothing moves; frames are static.
    Off,
}

impl Motion {
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Calm => "calm",
            Self::Off => "off",
        }
    }

    pub fn next(self) -> Self {
        match self {
            Self::Full => Self::Calm,
            Self::Calm => Self::Off,
            Self::Off => Self::Full,
        }
    }

    fn from_name(s: &str) -> Option<Self> {
        match s {
            "full" => Some(Self::Full),
            "calm" => Some(Self::Calm),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    /// `SUI_MOTION` beats the saved preference; with neither, SSH sessions
    /// start calm and everything else starts full.
    pub fn resolve(saved: Option<&str>) -> Self {
        std::env::var("SUI_MOTION")
            .ok()
            .as_deref()
            .and_then(Self::from_name)
            .or_else(|| saved.and_then(Self::from_name))
            .unwrap_or_else(|| {
                if std::env::var_os("SSH_CONNECTION").is_some()
                    || std::env::var_os("SSH_TTY").is_some()
                {
                    Self::Calm
                } else {
                    Self::Full
                }
            })
    }
}

pub const BOOT_MS: u64 = 2200;
pub const SLEEP_MS: u64 = 45_000;
pub const CONFETTI_MS: u64 = 1900;
pub const FADE_MS: u64 = 180;

#[derive(Clone, Debug)]
pub struct Anim {
    pub motion: Motion,
    /// Clock reading when the app started (drives the splash).
    pub born: u64,
    pub skipped: bool,
    /// Last user activity (drives the nap).
    pub active: u64,
    pub tap: Option<u64>,
    pub poke: Option<u64>,
    /// Last finished run: when, and whether it succeeded.
    pub done: Option<(u64, bool)>,
    /// When the open dialog appeared.
    pub modal: Option<u64>,
    seen_running: bool,
    seen_input: u64,
    seen_modal: bool,
}

impl Anim {
    pub fn new(saved: Option<&str>) -> Self {
        let now = super::slime::now_ms();
        Self {
            motion: Motion::resolve(saved),
            born: now,
            skipped: false,
            active: now,
            tap: None,
            poke: None,
            done: None,
            modal: None,
            seen_running: false,
            seen_input: text_hash(""),
            seen_modal: false,
        }
    }

    /// Any user input wakes the slime and ends the splash.
    pub fn touch(&mut self, now: u64) {
        self.active = now;
        self.skipped = true;
    }

    pub fn poke(&mut self, now: u64) {
        self.touch(now);
        self.poke = Some(now);
    }

    /// Milliseconds into the splash while it is still playing.
    pub fn boot(&self, now: u64) -> Option<u64> {
        let age = now.saturating_sub(self.born);
        (self.motion == Motion::Full && !self.skipped && age < BOOT_MS).then_some(age)
    }

    pub fn since(&self, t: Option<u64>, now: u64) -> Option<u64> {
        t.map(|t| now.saturating_sub(t))
    }
}

fn text_hash(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Advance animation bookkeeping from observed app state.
pub fn observe(app: &mut App, now: u64) {
    let input = text_hash(&app.input.text());
    let (running, modal) = (app.running, app.modal.is_some());
    let verdict = match app.outcome.as_str() {
        "done" | "accepted" => Some(true),
        o if o.starts_with("error") => Some(false),
        _ => None,
    };
    let a = &mut app.anim;
    if input != a.seen_input {
        a.seen_input = input;
        a.tap = Some(now);
        a.active = now;
    }
    if a.seen_running && !running {
        a.done = verdict.map(|ok| (now, ok));
    }
    a.seen_running = running;
    if modal != a.seen_modal {
        a.seen_modal = modal;
        a.modal = modal.then_some(now);
    }
}

/// Delay until the next frame is worth drawing, or None when nothing moves.
pub fn frame_ms(app: &App, now: u64) -> Option<u64> {
    let a = &app.anim;
    let fx_live = a
        .done
        .is_some_and(|(t, _)| now.saturating_sub(t) < CONFETTI_MS)
        || a.modal
            .is_some_and(|t| now.saturating_sub(t) < FADE_MS + 40);
    // Full motion draws at 30 fps only while something is reacting (splash,
    // poke, keystroke, celebration, dialog fade); ambient motion runs at 20.
    let hot = fx_live
        || a.boot(now).is_some()
        || a.poke.is_some_and(|t| now.saturating_sub(t) < 1_500)
        || a.tap.is_some_and(|t| now.saturating_sub(t) < 600);
    match a.motion {
        Motion::Off => app.running.then_some(250),
        Motion::Calm => (app.running || is_home(app)).then_some(120),
        Motion::Full => {
            (app.running || is_home(app) || fx_live).then_some(if hot { 33 } else { 50 })
        }
    }
}

/// Blend towards `target` from `bg`; terminal colours pass through unchanged.
pub fn fade(bg: Color, target: Color, k: f32) -> Color {
    match (rgb(bg), rgb(target)) {
        (Some(a), Some(b)) => color(mix(a, b, k)),
        _ => target,
    }
}

/// Backdrop colour of a screen row. Deliberately constant along the row:
/// adjacent cells sharing one background keep words contiguous in the
/// terminal byte stream and keep repaints cheap.
fn backdrop_row(area: Rect, th: Theme, y: u16) -> Option<[f32; 3]> {
    let base = rgb(th.background)?;
    if !th.gel {
        return Some(base);
    }
    let glow = [base[0] + 7.0, base[1] + 18.0, base[2] + 42.0];
    let fy = (y - area.y) as f32 / area.height.max(1) as f32;
    Some(mix(base, glow, smooth(0.25, 1.0, fy).powf(1.4) * 0.9))
}

/// Soft vertical gel gradient, lightest near the composer. Static, so it
/// costs nothing after the first frame.
pub fn backdrop(buf: &mut Buffer, area: Rect, th: Theme) {
    if !th.gel {
        return;
    }
    for y in area.top()..area.bottom() {
        if let Some(c) = backdrop_row(area, th, y) {
            let c = color(c);
            for x in area.left()..area.right() {
                buf[(x, y)].set_bg(c);
            }
        }
    }
}

/// Dissolve freshly drawn cells into the backdrop: `k` 0 = invisible, 1 = as drawn.
pub fn fade_in(buf: &mut Buffer, r: Rect, screen: Rect, th: Theme, k: f32) {
    for y in r.top()..r.bottom() {
        let Some(back) = backdrop_row(screen, th, y) else {
            return;
        };
        for x in r.left()..r.right() {
            let cell = &mut buf[(x, y)];
            if let Some(fg) = rgb(cell.fg) {
                cell.set_fg(color(mix(back, fg, k)));
            }
            if let Some(bg) = rgb(cell.bg) {
                cell.set_bg(color(mix(back, bg, k)));
            }
        }
    }
}

/// One glowing highlight travelling around a bordered box.
pub fn flow_border(buf: &mut Buffer, r: Rect, base: Color, hi: Color, ms: u64) {
    if r.width < 3 || r.height < 2 || rgb(base).is_none() || rgb(hi).is_none() {
        return;
    }
    let (w, h) = (r.width as f32, r.height as f32);
    let per = 2.0 * (w + h);
    let mut put = |x: u16, y: u16, p: f32| {
        let cell = &mut buf[(x, y)];
        if !"─│╭╮╰╯".contains(cell.symbol()) {
            return;
        }
        let phase = p / per - ms as f32 / 2200.0;
        let k = (0.5 + 0.5 * (phase * std::f32::consts::TAU).sin()).powi(2);
        // five brightness steps: neighbouring frames mostly repaint nothing
        cell.set_fg(lerp(base, hi, (k * 4.0).round() / 4.0));
    };
    for x in r.left()..r.right() {
        put(x, r.top(), (x - r.x) as f32);
        put(x, r.bottom() - 1, w + h + (r.right() - 1 - x) as f32);
    }
    for y in r.top() + 1..r.bottom() - 1 {
        put(r.right() - 1, y, w + (y - r.y) as f32);
        put(r.left(), y, 2.0 * w + h + (r.bottom() - 1 - y) as f32);
    }
}

/// Text with a bright band sweeping through it.
pub fn shimmer(text: &str, base: Color, hi: Color, ms: u64) -> Vec<Span<'static>> {
    let n = text.chars().count() as f32;
    let head = (ms as f32 / 45.0) % (n + 14.0) - 7.0;
    text.chars()
        .enumerate()
        .map(|(i, ch)| {
            let d = i as f32 - head;
            Span::styled(
                ch.to_string(),
                Style::default().fg(lerp(base, hi, (-d * d / 18.0).exp())),
            )
        })
        .collect()
}

/// Colour-cycling text for the brand mark.
pub fn gradient(text: &str, a: Color, b: Color, ms: u64) -> Vec<Span<'static>> {
    let n = text.chars().count().max(1) as f32;
    text.chars()
        .enumerate()
        .map(|(i, ch)| {
            let k = 0.5 + 0.5 * (i as f32 / n * std::f32::consts::TAU - ms as f32 / 900.0).sin();
            Span::styled(
                ch.to_string(),
                Style::default()
                    .fg(lerp(a, b, k))
                    .add_modifier(Modifier::BOLD),
            )
        })
        .collect()
}

/// A celebratory spray of bubbles rising from the bottom centre of `area`.
/// Only blank cells are touched, so it can never hide transcript text.
pub fn burst(buf: &mut Buffer, area: Rect, age_ms: u64, th: Theme) {
    if area.width < 8 || area.height < 4 || age_ms >= CONFETTI_MS {
        return;
    }
    let Some(bg) = rgb(th.background) else {
        return;
    };
    let hues: Vec<_> = [th.accent, th.glow, th.warning, th.mission]
        .into_iter()
        .filter_map(rgb)
        .chain([[255.0, 150.0, 200.0], [140.0, 230.0, 255.0]])
        .collect();
    let mut cv = Canvas::new(area.width as usize, area.height as usize);
    let (w, h) = (cv.w as f32, cv.h as f32);
    let t = age_ms as f32 / 1000.0;
    for i in 0..44u32 {
        let ti = t - 0.2 * noise(i * 7 + 6);
        let life = 1.0 + 0.7 * noise(i * 7 + 3);
        if ti < 0.0 || ti > life {
            continue;
        }
        let ang = -std::f32::consts::FRAC_PI_2 + (noise(i * 7 + 1) - 0.5) * 1.9;
        let v = 30.0 + 56.0 * noise(i * 7 + 2);
        let x = w * (0.5 + (noise(i * 7 + 4) - 0.5) * 0.2) + ang.cos() * v * ti;
        let y = h - 1.0 + ang.sin() * v * ti + 0.5 * 44.0 * ti * ti;
        let c = hues[(noise(i * 7 + 5) * hues.len() as f32) as usize % hues.len()];
        let a = (1.0 - ti / life).powi(2);
        cv.over(x as i32, y as i32, c, a);
        cv.over(x as i32 + 1, y as i32, c, a * 0.6);
    }
    cv.blit(buf, area, bg, true);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn th() -> Theme {
        Theme::new(None)
    }

    #[test]
    fn motion_cycles_and_parses() {
        assert_eq!(Motion::Full.next(), Motion::Calm);
        assert_eq!(Motion::Calm.next(), Motion::Off);
        assert_eq!(Motion::Off.next(), Motion::Full);
        for m in [Motion::Full, Motion::Calm, Motion::Off] {
            assert_eq!(Motion::from_name(m.name()), Some(m));
        }
        assert_eq!(Motion::from_name("wild"), None);
    }

    #[test]
    fn shimmer_and_gradient_preserve_text() {
        for ms in [0, 500, 4000] {
            let spans = shimmer("squishing bugs", th().muted, th().accent, ms);
            assert_eq!(
                spans.iter().map(|s| s.content.as_ref()).collect::<String>(),
                "squishing bugs"
            );
            let g = gradient("SUI", th().accent, th().glow, ms);
            assert_eq!(
                g.iter().map(|s| s.content.as_ref()).collect::<String>(),
                "SUI"
            );
        }
    }

    #[test]
    fn terminal_theme_colours_never_shimmer() {
        let t = Theme::new(Some("terminal"));
        for s in shimmer("abc", t.muted, t.accent, 300) {
            assert_eq!(s.style.fg, Some(Color::Reset));
        }
        assert_eq!(fade(t.background, Color::Red, 0.2), Color::Red);
    }

    #[test]
    fn flow_border_only_recolours_border_glyphs() {
        let mut b = Buffer::empty(Rect::new(0, 0, 12, 4));
        let r = b.area;
        ratatui::widgets::Widget::render(
            ratatui::widgets::Block::bordered()
                .border_type(ratatui::widgets::BorderType::Rounded)
                .title("hi"),
            r,
            &mut b,
        );
        let before = b.clone();
        flow_border(&mut b, r, th().border, th().accent, 400);
        let mut changed = 0;
        for y in 0..4 {
            for x in 0..12 {
                assert_eq!(b[(x, y)].symbol(), before[(x, y)].symbol());
                if b[(x, y)].fg != before[(x, y)].fg {
                    changed += 1;
                    assert!("─│╭╮╰╯".contains(b[(x, y)].symbol()));
                }
            }
        }
        assert!(changed > 0);
        assert_eq!(
            b[(1, 0)].fg,
            before[(1, 0)].fg,
            "title text keeps its colour"
        );
    }

    #[test]
    fn burst_leaves_text_alone_and_expires() {
        let mut b = Buffer::empty(Rect::new(0, 0, 40, 10));
        for x in 0..40 {
            b[(x, 9)].set_char('T');
        }
        for age in [150, 400, 800, 1200] {
            burst(&mut b, Rect::new(0, 0, 40, 10), age, th());
        }
        assert!((0..40).all(|x| b[(x, 9)].symbol() == "T"));
        assert!(
            b.content.iter().any(|c| c.symbol() == "▀"),
            "something sparkled"
        );
        let mut late = Buffer::empty(Rect::new(0, 0, 40, 10));
        burst(&mut late, Rect::new(0, 0, 40, 10), CONFETTI_MS, th());
        assert!(late.content.iter().all(|c| c.symbol() == " "));
    }

    #[test]
    fn backdrop_only_changes_backgrounds() {
        let mut b = Buffer::empty(Rect::new(0, 0, 20, 10));
        b[(3, 3)].set_char('x');
        let area = b.area;
        backdrop(&mut b, area, th());
        assert_eq!(b[(3, 3)].symbol(), "x");
        assert_ne!(
            b[(10, 9)].bg,
            b[(0, 0)].bg,
            "gradient varies down the screen"
        );
        assert_eq!(b[(0, 5)].bg, b[(19, 5)].bg, "but never along a row");
        let mut t = Buffer::empty(Rect::new(0, 0, 4, 4));
        let area = t.area;
        backdrop(&mut t, area, Theme::new(Some("terminal")));
        assert_eq!(t[(1, 1)].bg, Color::Reset);
    }
}
