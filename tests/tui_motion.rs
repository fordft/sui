//! Motion, pixel-art and effect behaviour of the slime TUI. The animation
//! clock is process-global, so every test pins it under one lock.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{backend::TestBackend, style::Color, Terminal};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use sui::config::{ProfileCfg, UiSettings};
use sui::events::UiEvent;
use sui::tui::app::{App, Effect, Hit, Modal, Tab};
use sui::tui::commands::Command;
use sui::tui::draw;
use sui::tui::fx::{self, Motion};
use sui::tui::slime;
use sui::tui::theme::Theme;

static LOCK: Mutex<()> = Mutex::new(());

fn pin(ms: u64) -> MutexGuard<'static, ()> {
    let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    slime::freeze_clock(Some(ms));
    guard
}

fn app() -> App {
    let dir = std::env::temp_dir().join("sui-motion-tests");
    std::fs::create_dir_all(&dir).unwrap();
    let mut profiles = BTreeMap::new();
    profiles.insert(
        "p".to_string(),
        ProfileCfg {
            base_url: Some("http://127.0.0.1:1/v1".into()),
            model: Some("m".into()),
            kind: None,
            key_env: None,
            api_key: None,
            prompt_cache_key: None,
            pricing: None,
        },
    );
    let ui = UiSettings {
        solo_profile: Some("p".into()),
        orchestrator_profile: Some("p".into()),
        worker_profile: Some("p".into()),
        ..Default::default()
    };
    let mut a = App::with_state(dir, profiles, ui);
    a.anim.motion = Motion::Full;
    a.anim.born = 0;
    a.anim.skipped = true;
    a.anim.active = slime::now_ms();
    a
}

fn frame(app: &App, w: u16, h: u16) -> Terminal<TestBackend> {
    let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
    t.draw(|f| draw::draw(f, app)).unwrap();
    t
}

fn pixels(t: &Terminal<TestBackend>) -> usize {
    t.backend()
        .buffer()
        .content
        .iter()
        .filter(|c| c.symbol() == "▀")
        .count()
}

fn pixels_in_rows(t: &Terminal<TestBackend>, rows: std::ops::Range<u16>) -> usize {
    let buf = t.backend().buffer();
    let w = buf.area.width;
    rows.map(|y| (0..w).filter(|&x| buf[(x, y)].symbol() == "▀").count())
        .sum()
}

fn text(t: &Terminal<TestBackend>) -> String {
    format!("{}", t.backend())
}

/// Pixel art needs a truecolour terminal (and no NO_COLOR); other
/// environments legitimately draw the text mascot instead.
fn pixel_env() -> bool {
    slime::pixel_ok(Theme::new(None))
}

fn submit(a: &mut App, task: &str) {
    a.input.set(task);
    a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
}

#[test]
fn home_keeps_its_words_and_draws_pixel_art_where_it_fits() {
    if !pixel_env() {
        return;
    }
    let _g = pin(60_000);
    let a = app();
    for (w, h, art) in [
        (150, 46, true),
        (100, 30, true),
        (80, 24, true),
        (60, 16, false),
    ] {
        let t = frame(&a, w, h);
        let screen = text(&t);
        assert!(screen.contains("What are we building?"), "{w}x{h}");
        assert!(screen.contains("Enter"), "{w}x{h}");
        if art {
            assert!(pixels(&t) > 60, "{w}x{h} shows the slime");
        }
    }
    let t = frame(&a, 100, 30);
    assert!(
        a.hits.borrow().iter().any(|z| z.hit == Hit::Mascot),
        "the slime is a click target"
    );
    assert!(text(&t).contains("your squishy coding buddy"));
    slime::freeze_clock(None);
}

#[test]
fn tiny_and_odd_terminals_never_panic() {
    let _g = pin(1_234);
    let mut a = app();
    for &w in &[1u16, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144] {
        for &h in &[1u16, 2, 3, 5, 8, 13, 21, 34, 55] {
            for motion in [Motion::Full, Motion::Calm, Motion::Off] {
                a.anim.motion = motion;
                frame(&a, w, h);
            }
        }
    }
    slime::freeze_clock(None);
}

#[test]
fn terminal_theme_falls_back_to_the_text_mascot() {
    let _g = pin(60_000);
    let mut a = app();
    a.ui.theme = Some("terminal".into());
    let t = frame(&a, 100, 30);
    assert!(
        t.backend()
            .buffer()
            .content
            .iter()
            .all(|c| { !matches!(c.fg, Color::Rgb(..)) && !matches!(c.bg, Color::Rgb(..)) }),
        "terminal theme never emits truecolour"
    );
    let screen = text(&t);
    assert!(screen.contains("What are we building?") && screen.contains('█'));
    slime::freeze_clock(None);
}

#[test]
fn motion_off_is_perfectly_static() {
    let _g = pin(1_000);
    let mut a = app();
    a.anim.motion = Motion::Off;
    let first = frame(&a, 100, 30).backend().buffer().clone();
    slime::freeze_clock(Some(9_777));
    let later = frame(&a, 100, 30).backend().buffer().clone();
    assert_eq!(first, later);
    slime::freeze_clock(None);
}

#[test]
fn full_motion_animates_but_a_pinned_clock_reproduces_frames() {
    let _g = pin(60_000);
    let a = app();
    let x = frame(&a, 100, 30).backend().buffer().clone();
    let same = frame(&a, 100, 30).backend().buffer().clone();
    assert_eq!(x, same);
    slime::freeze_clock(Some(60_700));
    let moved = frame(&a, 100, 30).backend().buffer().clone();
    assert_ne!(x, moved);
    slime::freeze_clock(None);
}

#[test]
fn splash_plays_once_and_any_input_ends_it() {
    if !pixel_env() {
        return;
    }
    let _g = pin(0);
    let mut a = app();
    a.anim.skipped = false;
    assert_eq!(a.anim.boot(500), Some(500));
    assert_eq!(a.anim.boot(fx::BOOT_MS + 1), None);
    let drop = pixels(&frame(&a, 100, 30));
    slime::freeze_clock(Some(1_600));
    let slime_up = pixels(&frame(&a, 100, 30));
    assert!(drop < slime_up, "a lone drop precedes the slime");
    a.anim.touch(1_700);
    assert_eq!(a.anim.boot(1_800), None);
    for m in [Motion::Calm, Motion::Off] {
        let mut b = app();
        b.anim.skipped = false;
        b.anim.motion = m;
        assert_eq!(b.anim.boot(500), None, "{m:?} has no splash");
    }
    slime::freeze_clock(None);
}

#[test]
fn clicking_the_slime_pokes_it() {
    if !pixel_env() {
        return;
    }
    let _g = pin(60_000);
    let mut a = app();
    frame(&a, 100, 30);
    let zone = *a
        .hits
        .borrow()
        .iter()
        .find(|z| z.hit == Hit::Mascot)
        .expect("mascot zone");
    assert!(a.anim.poke.is_none());
    let (col, row) = (zone.x + zone.w / 2, zone.y + zone.h / 2);
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        a.mouse(MouseEvent {
            kind,
            column: col,
            row,
            modifiers: KeyModifiers::NONE,
        });
    }
    assert_eq!(a.anim.poke, Some(60_000));
    let before = app();
    slime::freeze_clock(Some(60_090));
    let poked = frame(&a, 100, 30).backend().buffer().clone();
    let calm = frame(&before, 100, 30).backend().buffer().clone();
    assert_ne!(poked, calm, "the slime visibly reacts");
    slime::freeze_clock(None);
}

#[test]
fn observe_tracks_finished_runs_dialogs_and_typing() {
    let _g = pin(0);
    let mut a = app();
    fx::observe(&mut a, 50);
    assert_eq!(
        a.anim.tap, None,
        "an empty composer at startup is not a keystroke"
    );
    a.running = true;
    fx::observe(&mut a, 100);
    a.outcome = "done".into();
    a.running = false;
    fx::observe(&mut a, 200);
    assert_eq!(a.anim.done, Some((200, true)));

    a.running = true;
    fx::observe(&mut a, 300);
    a.outcome = "error: boom".into();
    a.running = false;
    fx::observe(&mut a, 400);
    assert_eq!(a.anim.done, Some((400, false)));

    a.running = true;
    fx::observe(&mut a, 500);
    a.outcome = "stopped".into();
    a.running = false;
    fx::observe(&mut a, 600);
    assert_eq!(a.anim.done, None, "a user stop is neither a win nor a fail");

    a.modal = Some(Modal::Help);
    fx::observe(&mut a, 700);
    assert_eq!(a.anim.modal, Some(700));
    fx::observe(&mut a, 750);
    assert_eq!(a.anim.modal, Some(700), "stays anchored to when it opened");
    a.modal = None;
    fx::observe(&mut a, 800);
    assert_eq!(a.anim.modal, None);

    a.input.set("x");
    fx::observe(&mut a, 900);
    assert_eq!(a.anim.tap, Some(900));
    fx::observe(&mut a, 950);
    assert_eq!(a.anim.tap, Some(900), "no edit, no tap");
    slime::freeze_clock(None);
}

#[test]
fn frame_pacing_follows_motion_and_activity() {
    let _g = pin(100);
    let mut a = app();
    assert_eq!(fx::frame_ms(&a, 100), Some(50), "home animates");
    a.anim.poke = Some(100);
    assert_eq!(
        fx::frame_ms(&a, 200),
        Some(33),
        "reactions run at full rate"
    );
    a.anim.poke = None;
    a.anim.motion = Motion::Calm;
    assert_eq!(fx::frame_ms(&a, 100), Some(120));
    a.anim.motion = Motion::Off;
    assert_eq!(fx::frame_ms(&a, 100), None, "nothing moves");
    a.running = true;
    assert_eq!(
        fx::frame_ms(&a, 100),
        Some(250),
        "clock still ticks while running"
    );
    a.running = false;
    a.anim.motion = Motion::Full;
    submit(&mut a, "hello");
    a.running = false;
    assert_eq!(fx::frame_ms(&a, 100), None, "busy-free chat is idle");
    a.anim.done = Some((100, true));
    assert_eq!(fx::frame_ms(&a, 200), Some(33), "confetti needs frames");
    assert_eq!(fx::frame_ms(&a, 100 + fx::CONFETTI_MS + 10), None);
    slime::freeze_clock(None);
}

#[test]
fn companion_fills_spare_room_but_never_covers_the_transcript() {
    if !pixel_env() {
        return;
    }
    let _g = pin(10_000);
    let mut a = app();
    submit(&mut a, "write a haiku about slime");
    fx::observe(&mut a, 10_000);
    let roomy = frame(&a, 100, 30);
    assert!(text(&roomy).contains("write a haiku about slime"));
    assert!(text(&roomy).contains("slime is working"));
    assert!(
        pixels_in_rows(&roomy, 12..24) > 40,
        "a thinking slime sits below the short transcript"
    );
    assert!(
        a.hits.borrow().iter().any(|z| z.hit == Hit::Mascot),
        "and can be poked"
    );
    // No spare rows: the pet must step aside rather than overlap text.
    let cramped = frame(&a, 100, 12);
    assert_eq!(pixels(&cramped), 0);
    assert!(text(&cramped).contains("write a haiku about slime"));
    slime::freeze_clock(None);
}

#[test]
fn confetti_celebrates_success_only() {
    if !pixel_env() {
        return;
    }
    let mk = |outcome: &str| {
        slime::freeze_clock(Some(10_000));
        let mut a = app();
        submit(&mut a, "write a haiku about slime");
        fx::observe(&mut a, 10_000);
        slime::freeze_clock(Some(10_300));
        a.apply_event(UiEvent::RunDone {
            run: 1,
            outcome: outcome.into(),
            accepted_sha: None,
        });
        fx::observe(&mut a, 10_300);
        slime::freeze_clock(Some(10_900));
        a
    };
    let _g = pin(10_000);
    let win = mk("done");
    let fail = mk("error: boom");
    let (w, l) = (frame(&win, 100, 30), frame(&fail, 100, 30));
    assert!(
        pixels_in_rows(&w, 5..15) > 5,
        "confetti flies through the upper transcript"
    );
    assert_eq!(pixels_in_rows(&l, 5..15), 0, "failure gets no party");
    assert!(text(&w).contains("Run ended · done") || text(&w).contains("all done"));
    slime::freeze_clock(None);
}

#[test]
fn dialog_backdrop_fades_in_then_settles_on_muted() {
    let _g = pin(1_000);
    let mut a = app();
    a.modal = Some(Modal::Help);
    a.anim.modal = Some(1_000);
    let early = frame(&a, 100, 30).backend().buffer().clone();
    slime::freeze_clock(Some(1_400));
    let late = frame(&a, 100, 30).backend().buffer().clone();
    let muted = a.theme().muted;
    // header brand cell: still bright at the start, fully dimmed once settled
    let first_text = (0..100u16)
        .find(|&x| late[(x, 0)].symbol() == "S" && late[(x + 1, 0)].symbol() == "U")
        .expect("SUI in header");
    assert_eq!(late[(first_text, 0)].fg, muted);
    assert_ne!(early[(first_text, 0)].fg, muted);
    slime::freeze_clock(None);
}

#[test]
fn motion_command_cycles_and_persists() {
    let _g = pin(0);
    let mut a = app();
    let mut seen = vec![];
    for _ in 0..3 {
        a.command(Command::Motion);
        seen.push(a.anim.motion);
        assert_eq!(a.ui.motion.as_deref(), Some(a.anim.motion.name()));
    }
    assert_eq!(seen, [Motion::Calm, Motion::Off, Motion::Full]);
    assert!(a.effects.iter().any(|e| matches!(e, Effect::SaveUi)));
    let ui = UiSettings {
        motion: Some("calm".into()),
        ..Default::default()
    };
    let round: UiSettings = toml::from_str(&toml::to_string(&ui).unwrap()).unwrap();
    assert_eq!(round.motion.as_deref(), Some("calm"));
    slime::freeze_clock(None);
}

#[test]
fn theme_cycle_visits_all_three_and_defaults_to_slime() {
    assert_eq!(Theme::name(None), "slime");
    let mut cur: Option<String> = None;
    let mut order = vec![];
    for _ in 0..4 {
        let next = Theme::next(cur.as_deref());
        order.push(next);
        cur = Some(next.into());
    }
    assert_eq!(order, ["terminal", "dark", "slime", "terminal"]);
    assert!(Theme::new(None).gel && !Theme::new(Some("dark")).gel);
    assert_eq!(Theme::new(Some("terminal")).background, Color::Reset);
}

#[test]
fn slime_theme_is_black_and_deep_blue() {
    let t = Theme::new(None);
    let rgb = |c: Color| match c {
        Color::Rgb(r, g, b) => (r as u16, g as u16, b as u16),
        other => panic!("slime theme colours are RGB, got {other:?}"),
    };
    for c in [t.background, t.surface, t.border, t.selection] {
        let (r, g, b) = rgb(c);
        assert!(b > g && g >= r, "{c:?} leans blue");
    }
    let (r, g, b) = rgb(t.background);
    assert!(r + g + b < 60, "the backdrop is near black");
    let (r, g, b) = rgb(t.accent);
    assert!(b > 200 && b > g && g > r, "the accent is azure");
    let (r, g, b) = rgb(t.glow);
    assert!(b > g && g > r, "the sheen is a lighter blue");
}

#[test]
fn empty_tabs_get_a_napping_slime() {
    if !pixel_env() {
        return;
    }
    let _g = pin(60_000);
    let mut a = app();
    for tab in [Tab::Tasks, Tab::Changes, Tab::Usage] {
        a.tab = tab;
        let t = frame(&a, 100, 30);
        assert!(pixels(&t) > 30, "{tab:?} shows a sleeper");
    }
    slime::freeze_clock(None);
}
