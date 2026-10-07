//! Motion, pixel-art and effect behaviour of the slime TUI. The animation
//! clock is process-global, so every test pins it under one lock.
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{backend::TestBackend, layout::Rect, style::Color, Terminal};
use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};
use sui::config::{ProfileCfg, UiSettings};
use sui::events::UiEvent;
use sui::tui::app::{App, Effect, Hit, HitZone, Modal, Tab};
use sui::tui::commands::Command;
use sui::tui::draw;
use sui::tui::fx::{self, Motion};
use sui::tui::layout;
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
            image_input: None,
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

/// Send a task and return its run id (the run stays "running").
fn start(a: &mut App, task: &str) -> u64 {
    submit(a, task);
    let Some(Effect::SendTask { run, .. }) = a.effects.pop() else {
        panic!("no SendTask effect")
    };
    run
}

fn finish(a: &mut App, run: u64, outcome: &str) {
    for ev in [
        UiEvent::ReqStart {
            run,
            agent: "solo".into(),
            req: 0,
        },
        UiEvent::Delta {
            run,
            agent: "solo".into(),
            req: 0,
            text: format!("reply {run}: the slime squishes, springs and lands.\nA second line."),
        },
        UiEvent::ReqDone {
            run,
            agent: "solo".into(),
            req: 0,
            ms: 5,
            ok: true,
            reasoning: false,
        },
        UiEvent::RunDone {
            run,
            outcome: outcome.into(),
            accepted_sha: None,
        },
    ] {
        a.apply_event(ev);
    }
}

/// A transcript long enough that no spare room is left for the corner pet.
fn long_chat() -> App {
    let mut a = app();
    for i in 0..8 {
        let run = start(&mut a, &format!("question number {i}: how does it hop?"));
        finish(&mut a, run, "done");
    }
    a
}

fn lane_of(a: &App, w: u16, h: u16) -> Option<Rect> {
    layout::regions(Rect::new(0, 0, w, h), a).lane
}

fn runner_zone(a: &App) -> Option<HitZone> {
    a.hits
        .borrow()
        .iter()
        .find(|z| z.hit == Hit::Mascot)
        .copied()
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
    assert_eq!(
        fx::frame_ms(&a, 100 + fx::CONFETTI_MS + 10),
        Some(50),
        "the lane slime keeps cheering after the confetti"
    );
    assert_eq!(
        fx::frame_ms(&a, 100 + slime::HAPPY_MS + 1_000),
        None,
        "and the screen settles once the cheer is over"
    );
    a.anim.done = Some((100, false));
    assert_eq!(fx::frame_ms(&a, 5_000), None, "fretting is a still pose");
    assert_eq!(
        fx::frame_ms(&a, 100 + slime::OOPS_MS + 100),
        Some(50),
        "one last frame wipes the mood away"
    );
    a.anim.done = None;
    let nap = a.anim.active + fx::SLEEP_MS + 100;
    assert_eq!(fx::frame_ms(&a, nap), Some(50), "a frame lets it doze off");
    assert_eq!(fx::frame_ms(&a, nap + 5_000), None);
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
    assert_eq!(pixels_in_rows(&l, 5..13), 0, "failure gets no party");
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

#[test]
fn a_long_chat_keeps_a_runner_in_its_own_lane() {
    if !pixel_env() {
        return;
    }
    let _g = pin(30_000);
    let mut a = long_chat();
    start(&mut a, "now do something long");
    fx::observe(&mut a, 30_000);
    for (w, h) in [(60, 26), (80, 30), (100, 34), (150, 50)] {
        let t = frame(&a, w, h);
        let r = layout::regions(Rect::new(0, 0, w, h), &a);
        let lane = r.lane.expect("lane");
        assert!(
            r.content.bottom() <= lane.y,
            "{w}x{h}: transcript ends above the lane"
        );
        assert_eq!(
            lane.bottom(),
            r.composer.y,
            "{w}x{h}: the lane sits on the composer"
        );
        assert!(text(&t).contains("now do something long"), "{w}x{h}");
        let (buf, rows) = (t.backend().buffer(), lane.y..lane.bottom());
        assert!(
            pixels_in_rows(&t, rows.clone()) > 10,
            "{w}x{h}: the slime is in its lane"
        );
        assert_eq!(
            pixels_in_rows(&t, r.content.y..r.content.bottom()),
            0,
            "{w}x{h}: no pixels over the transcript"
        );
        for y in rows {
            for x in 0..w {
                let c = buf[(x, y)].symbol();
                assert!(
                    c == "▀" || c.trim().is_empty() || "?!*z".contains(c),
                    "{w}x{h}: lane cell ({x},{y}) holds {c:?}"
                );
            }
        }
        let z = runner_zone(&a).expect("poke target");
        assert!(
            z.x >= lane.x && z.x + z.w <= lane.right() && z.y == lane.y && z.h == lane.height,
            "{w}x{h}: {z:?} not inside {lane:?}"
        );
    }
    slime::freeze_clock(None);
}

#[test]
fn the_runner_runs_both_ways_and_stays_in_the_lane() {
    if !pixel_env() {
        return;
    }
    let _g = pin(30_000);
    let mut a = long_chat();
    start(&mut a, "go");
    fx::observe(&mut a, 30_000);
    let lane = lane_of(&a, 100, 34).expect("lane");
    let (mut xs, mut left, mut right) = (vec![], false, false);
    for ms in (30_000..42_000).step_by(100) {
        slime::freeze_clock(Some(ms));
        frame(&a, 100, 34);
        let z = runner_zone(&a).expect("runner");
        assert!(z.x >= lane.x && z.x + z.w <= lane.right());
        let x = z.x as i32 + z.w as i32 / 2;
        if let Some(&p) = xs.last() {
            left |= x < p;
            right |= x > p;
        }
        xs.push(x);
    }
    let (lo, hi) = (xs.iter().min().unwrap(), xs.iter().max().unwrap());
    assert!(hi - lo > 60, "it crosses most of the lane: {lo}..{hi}");
    assert!(left && right, "and turns around at the ends");
    slime::freeze_clock(None);
}

#[test]
fn the_runner_parks_where_the_run_ended() {
    if !pixel_env() {
        return;
    }
    let _g = pin(30_000);
    let mut a = long_chat();
    let run = start(&mut a, "go");
    fx::observe(&mut a, 30_000);
    slime::freeze_clock(Some(34_100));
    finish(&mut a, run, "done");
    fx::observe(&mut a, 34_100);
    slime::freeze_clock(Some(34_200));
    frame(&a, 100, 34);
    let cheering = runner_zone(&a).unwrap();
    let start_x = lane_of(&a, 100, 34).unwrap().right();
    assert!(
        cheering.x + cheering.w < start_x - 8,
        "it ran away from the start"
    );
    slime::freeze_clock(Some(80_000));
    frame(&a, 100, 34);
    let napping = runner_zone(&a).unwrap();
    assert_eq!(
        (napping.x, napping.w),
        (cheering.x, cheering.w),
        "it rests in place"
    );
    slime::freeze_clock(Some(80_500));
    start(&mut a, "again");
    fx::observe(&mut a, 80_500);
    frame(&a, 100, 34);
    let resumed = runner_zone(&a).unwrap();
    assert!(
        resumed.x.abs_diff(napping.x) <= 2,
        "the next run resumes from the same spot"
    );
    slime::freeze_clock(None);
}

#[test]
fn the_lane_only_exists_where_it_fits() {
    if !pixel_env() {
        return;
    }
    let _g = pin(30_000);
    let mut a = long_chat();
    for (w, h, rows) in [
        (100, 25, 0),
        (49, 40, 0),
        (100, 26, 3),
        (100, 33, 3),
        (100, 34, 4),
        (100, 60, 4),
    ] {
        let lane = lane_of(&a, w, h);
        assert_eq!(lane.map_or(0, |l| l.height), rows, "{w}x{h}");
        if lane.is_none() {
            let t = frame(&a, w, h);
            assert!(runner_zone(&a).is_none(), "{w}x{h} has no runner");
            assert_eq!(pixels(&t), 0, "{w}x{h}");
        }
    }
    let fresh = app();
    assert!(
        lane_of(&fresh, 100, 40).is_none(),
        "the home screen has its own hero"
    );
    for tab in [Tab::Tasks, Tab::Changes, Tab::Usage, Tab::Settings] {
        a.tab = tab;
        assert!(lane_of(&a, 100, 40).is_none(), "{tab:?}");
    }
    a.tab = Tab::Chat;
    a.ui.theme = Some("terminal".into());
    assert!(lane_of(&a, 100, 40).is_none(), "no pixel art, no lane");
    let rows_before = layout::regions(Rect::new(0, 0, 100, 40), &a).content.height;
    a.ui.theme = None;
    let rows_after = layout::regions(Rect::new(0, 0, 100, 40), &a).content.height;
    assert_eq!(
        rows_before - rows_after,
        3,
        "a 4-row lane costs 3 transcript rows"
    );
    slime::freeze_clock(None);
}

#[test]
fn the_runner_stands_still_when_motion_is_off() {
    if !pixel_env() {
        return;
    }
    let _g = pin(30_000);
    let mut a = long_chat();
    a.anim.motion = Motion::Off;
    let run = start(&mut a, "go");
    fx::observe(&mut a, 30_000);
    let first = frame(&a, 100, 34).backend().buffer().clone();
    slime::freeze_clock(Some(37_000));
    let later = frame(&a, 100, 34).backend().buffer().clone();
    assert_eq!(first, later, "still while running");
    assert!(pixels(&frame(&a, 100, 34)) > 10, "but still there");
    slime::freeze_clock(Some(37_100));
    finish(&mut a, run, "done");
    fx::observe(&mut a, 37_100);
    let calm = frame(&a, 100, 34).backend().buffer().clone();
    slime::freeze_clock(Some(41_000));
    assert_eq!(calm, frame(&a, 100, 34).backend().buffer().clone());
    slime::freeze_clock(None);
}

#[test]
fn clicking_the_runner_pokes_it_and_moods_reach_the_lane() {
    if !pixel_env() {
        return;
    }
    let _g = pin(30_000);
    let mut a = long_chat();
    frame(&a, 100, 34);
    let z = runner_zone(&a).expect("runner");
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        a.mouse(MouseEvent {
            kind,
            column: z.x + z.w / 2,
            row: z.y + z.h / 2,
            modifiers: KeyModifiers::NONE,
        });
    }
    assert_eq!(a.anim.poke, Some(30_000));
    let lane = lane_of(&a, 100, 34).unwrap();
    let at = |a: &App| {
        let t = frame(a, 100, 34);
        let buf = t.backend().buffer().clone();
        let mut ch = String::new();
        for y in lane.y..lane.bottom() {
            for x in lane.x..lane.right() {
                let c = buf[(x, y)].symbol();
                if "?!*z".contains(c) && !c.trim().is_empty() {
                    ch.push_str(c);
                }
            }
        }
        ch
    };
    let mut ok = long_chat();
    let run = start(&mut ok, "go");
    fx::observe(&mut ok, 30_000);
    slime::freeze_clock(Some(31_000));
    finish(&mut ok, run, "done");
    fx::observe(&mut ok, 31_000);
    assert_eq!(at(&ok), "*", "success sparkles");
    let mut bad = long_chat();
    let run = start(&mut bad, "go");
    fx::observe(&mut bad, 31_000);
    finish(&mut bad, run, "error: boom");
    fx::observe(&mut bad, 31_000);
    assert_eq!(at(&bad), "!", "failure frets");
    slime::freeze_clock(Some(31_000 + fx::SLEEP_MS + 5_000));
    let mut idle = long_chat();
    idle.anim.active = 31_000;
    assert_eq!(at(&idle), "z", "it naps when left alone");
    slime::freeze_clock(None);
}

#[test]
fn the_lane_survives_every_size_and_mood() {
    let _g = pin(30_000);
    let mut a = long_chat();
    for (n, outcome) in ["done", "error: boom"].into_iter().enumerate() {
        let base = 30_000 + 10_000 * n as u64;
        slime::freeze_clock(Some(base));
        let run = start(&mut a, "go");
        fx::observe(&mut a, base);
        for state in 0..3 {
            match state {
                0 => {}
                1 => finish(&mut a, run, outcome),
                _ => a.modal = Some(Modal::Help),
            }
            slime::freeze_clock(Some(base + state));
            fx::observe(&mut a, base + state);
            for &w in &[1u16, 20, 49, 50, 51, 80, 110, 111, 160] {
                for &h in &[1u16, 6, 12, 25, 26, 27, 33, 34, 35, 70] {
                    for motion in [Motion::Full, Motion::Calm, Motion::Off] {
                        a.anim.motion = motion;
                        frame(&a, w, h);
                    }
                }
            }
        }
        a.modal = None;
    }
    slime::freeze_clock(None);
}
