//! Slime personality: mascot art, animation clock, and cute status copy.
//! Pure presentation — deterministic functions of a millisecond clock, so
//! every frame is reproducible in tests and nothing here touches model
//! traffic or app state.
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Instant;

use super::app::{App, Tab};
use super::fx::{Motion, SLEEP_MS};
use super::theme::Theme;

static FROZEN: AtomicU64 = AtomicU64::new(u64::MAX);

/// Pin the animation clock (None = real time) so a frame can be reproduced
/// exactly — snapshot tests and screenshots use this.
pub fn freeze_clock(ms: Option<u64>) {
    FROZEN.store(ms.unwrap_or(u64::MAX), Ordering::Relaxed);
}

pub fn now_ms() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    match FROZEN.load(Ordering::Relaxed) {
        u64::MAX => START.get_or_init(Instant::now).elapsed().as_millis() as u64,
        t => t,
    }
}

/// Rotating arc — narrow, unambiguous-width glyphs.
pub const SPIN: [&str; 6] = ["◜", "◠", "◝", "◞", "◡", "◟"];

pub fn spinner(ms: u64) -> &'static str {
    SPIN[(ms / 110) as usize % SPIN.len()]
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mood {
    Idle,
    Think,
    Happy,
    Oops,
    Ask,
    Sleep,
}

/// The animation clock a frame should use: frozen at 0 when motion is off,
/// so every frame is static and reproducible.
pub fn clock(app: &App) -> u64 {
    if app.anim.motion == Motion::Off {
        0
    } else {
        now_ms()
    }
}

/// Half-block pixel art needs real RGB colours. NO_COLOR would turn every
/// pixel into a solid glyph, and terminals that announce less than truecolor
/// would show garbage — those keep the text mascot.
fn truecolor_terminal() -> bool {
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| {
        if std::env::var_os("NO_COLOR").is_some() {
            return false;
        }
        match std::env::var("COLORTERM") {
            Ok(v) if !v.is_empty() => matches!(v.as_str(), "truecolor" | "24bit"),
            // Unset is common over SSH; only Apple's Terminal is known to lack it.
            _ => std::env::var("TERM_PROGRAM").as_deref() != Ok("Apple_Terminal"),
        }
    })
}

pub fn pixel_ok(theme: Theme) -> bool {
    super::gfx::rgb(theme.background).is_some() && truecolor_terminal()
}

/// How long a finished run keeps the slime cheering or fretting.
pub const HAPPY_MS: u64 = 6_000;
pub const OOPS_MS: u64 = 12_000;

pub fn mood(app: &App) -> Mood {
    let (a, now) = (&app.anim, now_ms());
    if matches!(app.modal, Some(super::app::Modal::Permission { .. }))
        || !app.pending_perms.is_empty()
    {
        Mood::Ask
    } else if app.running {
        Mood::Think
    } else if let Some(ok) = a.done.and_then(|(t, ok)| {
        (now.saturating_sub(t) < if ok { HAPPY_MS } else { OOPS_MS }).then_some(ok)
    }) {
        if ok {
            Mood::Happy
        } else {
            Mood::Oops
        }
    } else if a.motion != Motion::Off && now.saturating_sub(a.active) > SLEEP_MS {
        Mood::Sleep
    } else {
        Mood::Idle
    }
}

/// No conversation yet: the chat tab shows the home screen instead of a transcript.
pub fn empty_chat(app: &App) -> bool {
    app.groups.len() == 1 && app.groups[0].items.is_empty() && !app.running
}

/// True when the home screen mascot is on screen.
pub fn is_home(app: &App) -> bool {
    app.tab == Tab::Chat && empty_chat(app) && app.modal.is_none()
}

/// Compact face for the header bar.
pub fn face(mood: Mood, ms: u64) -> &'static str {
    let blink = ms % 3200 > 3050;
    match mood {
        Mood::Think => ["(•o•)", "(•‿•)", "(◕‿◕)", "(•‿•)"][(ms / 260) as usize % 4],
        Mood::Happy => "(^‿^)",
        Mood::Oops => "(×_×)",
        Mood::Ask => ["(•?•)", "(◕?◕)"][(ms / 500) as usize % 2],
        Mood::Sleep => ["(-ᴗ-)z", "(-ᴗ-)Z"][(ms / 900) as usize % 2],
        Mood::Idle if blink => "(-‿-)",
        Mood::Idle => "(•‿•)",
    }
}

pub const WORKING: [&str; 6] = [
    "oozing through your repo",
    "wobbling on it",
    "squishing bugs",
    "jiggling the code",
    "absorbing context",
    "bubbling up an answer",
];

pub fn working_phrase(ms: u64) -> &'static str {
    WORKING[(ms / 3000) as usize % WORKING.len()]
}

pub const TIPS: [&str; 5] = [
    "Ask Sui to build, fix, or investigate…",
    "Try: \"find why the tests are flaky\"",
    "Try: \"add a --json flag to the cli\"",
    "Ctrl+P opens the command palette",
    "Switch to Mission from Ctrl+P for big jobs",
];

pub fn tip(ms: u64) -> &'static str {
    TIPS[(ms / 5000) as usize % TIPS.len()]
}

/// Blend two RGB colors; non-RGB inputs (terminal theme) stay unchanged.
pub fn lerp(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t.clamp(0.0, 1.0)) as u8;
            Color::Rgb(m(ar, br), m(ag, bg), m(ab, bb))
        }
        _ => a,
    }
}

/// Smooth 0..1 breathing wave with the given period.
pub fn pulse(ms: u64, period: u64) -> f32 {
    let x = (ms % period) as f32 / period as f32 * std::f32::consts::TAU;
    (x.sin() + 1.0) / 2.0
}

pub const ART_H: usize = 6;
pub const ART_W: usize = 14;

// E = eye, M = mouth, everything else is body. Rows are ART_W wide.
const NORMAL: [&str; 5] = [
    "     ▄▄▄▄     ",
    "  ▄████████▄  ",
    " ████E██E████ ",
    " █████M██████ ",
    "  ▀▀▀▀▀▀▀▀▀▀  ",
];
const SQUASH: [&str; 4] = [
    "  ▄▄▄▄▄▄▄▄▄▄  ",
    "▄███E████E███▄",
    "██████M███████",
    "▀▀▀▀▀▀▀▀▀▀▀▀▀▀",
];
const STRETCH: [&str; 6] = [
    "      ▄▄      ",
    "    ▄████▄    ",
    "   ██E██E██   ",
    "   ███M████   ",
    "    ▀████▀    ",
    "      ▀▀      ",
];

fn shape(mood: Mood, ms: u64) -> &'static [&'static str] {
    // Only an idle slime hops; a busy one stays put and wiggles its eyes.
    if mood != Mood::Idle && mood != Mood::Happy {
        return &NORMAL;
    }
    match ms % 2600 {
        1700..1850 => &SQUASH,
        1850..2150 => &STRETCH,
        2150..=2300 => &SQUASH,
        _ => &NORMAL,
    }
}

fn eyes(mood: Mood, ms: u64) -> (char, char, char) {
    let blink = ms % 3200 > 3050;
    match mood {
        Mood::Happy => ('^', '^', '‿'),
        Mood::Oops => ('×', '×', '︵'),
        Mood::Ask => ('◉', '◉', 'o'),
        Mood::Think => {
            let look = (ms / 400) % 4;
            match look {
                0 => ('◔', '◔', 'o'),
                2 => ('◕', '◕', 'o'),
                _ => ('●', '●', '‿'),
            }
        }
        Mood::Sleep => ('▬', '▬', 'o'),
        Mood::Idle if blink => ('▬', '▬', '‿'),
        Mood::Idle => ('●', '●', '‿'),
    }
}

/// The mascot as ART_H styled lines, ART_W wide, bottom-aligned so the
/// hop moves it up without shifting the layout.
pub fn mascot(theme: Theme, mood: Mood, ms: u64) -> Vec<Line<'static>> {
    let body_c = match mood {
        Mood::Oops => theme.error,
        Mood::Ask => theme.warning,
        Mood::Think => lerp(theme.glow, theme.accent, pulse(ms, 1400)),
        _ => theme.accent,
    };
    let body = Style::default().fg(body_c);
    let face = Style::default()
        .fg(theme.background)
        .bg(body_c)
        .add_modifier(Modifier::BOLD);
    let (l, r, m) = eyes(mood, ms);
    let rows = shape(mood, ms);
    let mut out: Vec<Line<'static>> = Vec::with_capacity(ART_H);
    for _ in rows.len()..ART_H {
        out.push(Line::from(" ".repeat(ART_W)));
    }
    for row in rows {
        let mut seen_eye = false;
        let spans: Vec<Span<'static>> = row
            .chars()
            .map(|c| match c {
                'E' => {
                    let ch = if seen_eye { r } else { l };
                    seen_eye = true;
                    Span::styled(ch.to_string(), face)
                }
                'M' => Span::styled(m.to_string(), face),
                c => Span::styled(c.to_string(), body),
            })
            .collect();
        out.push(Line::from(spans));
    }
    out
}

/// Twinkling sparkle line drawn above/around the mascot.
pub fn sparkles(theme: Theme, width: usize, ms: u64) -> Line<'static> {
    const G: [&str; 4] = ["✦", "·", "˙", "✧"];
    let mut s = String::new();
    let mut i = 0usize;
    while s.chars().count() < width {
        let phase = ((ms / 500) as usize + i * 3) % 8;
        s.push_str(if phase < 4 { G[phase] } else { " " });
        s.push_str("   ");
        i += 1;
    }
    let s: String = s.chars().take(width).collect();
    Line::from(Span::styled(
        s,
        Style::default().fg(lerp(theme.muted, theme.accent, pulse(ms, 2000))),
    ))
}

/// Border/accent color for a busy composer: breathes between border and accent.
pub fn breathing(theme: Theme, ms: u64) -> Color {
    lerp(theme.border, theme.accent, pulse(ms, 1200))
}

/// Horizontal ooze bar: a bright blob sliding through a dim gel trough.
pub fn ooze_bar(theme: Theme, width: usize, ms: u64) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let head = (ms / 70) as usize % (width + 6);
    let spans = (0..width)
        .map(|x| {
            let d = head as isize - x as isize;
            let (ch, c) = match d {
                0 => ("█", theme.accent),
                1 => ("▓", lerp(theme.accent, theme.border, 0.35)),
                2 => ("▒", lerp(theme.accent, theme.border, 0.6)),
                3 => ("░", lerp(theme.accent, theme.border, 0.85)),
                _ => ("─", theme.border),
            };
            Span::styled(ch, Style::default().fg(c))
        })
        .collect::<Vec<_>>();
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn art_is_rectangular_for_every_frame() {
        for mood in [Mood::Idle, Mood::Think, Mood::Happy, Mood::Oops, Mood::Ask] {
            for ms in (0..6000).step_by(50) {
                let lines = mascot(Theme::new(None), mood, ms);
                assert_eq!(lines.len(), ART_H);
                for l in lines {
                    let w: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
                    assert_eq!(w, ART_W, "mood {mood:?} ms {ms}");
                }
            }
        }
    }

    #[test]
    fn ooze_bar_keeps_width() {
        for ms in (0..2000).step_by(70) {
            let l = ooze_bar(Theme::new(None), 20, ms);
            assert_eq!(l.spans.len(), 20);
        }
    }

    #[test]
    fn lerp_ends_and_terminal_passthrough() {
        let (a, b) = (Color::Rgb(0, 0, 0), Color::Rgb(100, 200, 50));
        assert_eq!(lerp(a, b, 0.0), a);
        assert_eq!(lerp(a, b, 1.0), b);
        assert_eq!(lerp(Color::Reset, b, 0.5), Color::Reset);
    }
}
