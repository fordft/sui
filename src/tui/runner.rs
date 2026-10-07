//! The lane runner: a small slime that hops back and forth along a strip
//! above the composer. The big mascot and the corner companion need spare
//! room that a long transcript takes away, so the layout reserves this lane
//! instead — the slime stays on screen however long the chat grows and never
//! shares a row with text. Pure over (App, clock) like the rest of the
//! motion layer; only `Track` is advanced from the event loop.
use std::f32::consts::{PI, TAU};

use ratatui::layout::Rect;
use ratatui::Frame;

use super::app::{App, Hit, HitZone, Tab};
use super::fx::{Motion, SLEEP_MS};
use super::gfx::{cover, ellipse_d, mix, rgb, scale, smooth, Canvas, Rgb};
use super::hero::{blink, palette, BLUSH, INK, WHITE};
use super::slime::{self, Mood, HAPPY_MS, OOPS_MS};

/// Resting half-width and height of the body in canvas pixels, at the
/// tall lane (`TALL_PX` pixels); shorter lanes scale the slime down.
const AW: f32 = 4.6;
const AH: f32 = 4.8;
const TALL_PX: f32 = 8.0;
const SPRITE_COLS: usize = 16;
const MARGIN: f32 = 1.0;
const HOP_MS: f32 = 420.0;
const HOP_COLS: f32 = 6.0;
/// Extra frames after a mood expires, so the redraw that ends it happens.
const EDGE_MS: u64 = 450;

/// Where the slime stands along its run. Advanced from the event loop; the
/// slime parks wherever the last run left it and resumes from there.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Track {
    /// Unfolded distance in columns walked from the right end, at `t0`.
    pub u: f32,
    pub t0: u64,
    pub running: bool,
}

impl Track {
    /// Edge-triggered: a run starts from the parking spot and parks again
    /// where it ends. `live` is false when motion is off (nothing walked).
    pub fn advance(&mut self, running: bool, now: u64, live: bool) {
        if running == self.running {
            return;
        }
        if !running && live {
            self.u += travel(now.saturating_sub(self.t0));
        }
        self.t0 = now;
        self.running = running;
    }
}

/// Whole hops completed plus eased progress through the current one, and the
/// 0..1 phase within that hop. The slime only travels while airborne.
fn hops(dt: u64) -> (f32, f32) {
    let k = dt as f32 / HOP_MS;
    (k.floor() + smooth(0.14, 0.80, k.fract()), k.fract())
}

fn travel(dt: u64) -> f32 {
    hops(dt).0 * HOP_COLS
}

/// Fold an unfolded distance into a back-and-forth walk along `len`:
/// the offset from the right end, and whether that leg heads left.
fn fold(u: f32, len: f32) -> (f32, bool) {
    let m = u.rem_euclid(2.0 * len);
    if m < len {
        (m, true)
    } else {
        (2.0 * len - m, false)
    }
}

/// Crouch, leap, stretch, land: (lift in px, sx, sy) at a hop phase.
fn hop_pose(ph: f32, peak: f32) -> (f32, f32, f32) {
    let a = ((ph - 0.14) / 0.66).clamp(0.0, 1.0);
    let q = if ph < 0.14 {
        1.0 - smooth(0.0, 0.14, ph)
    } else {
        smooth(0.80, 0.97, ph)
    };
    let air = (PI * a).sin();
    (
        peak * 4.0 * a * (1.0 - a),
        1.0 + 0.16 * q - 0.07 * air,
        1.0 - 0.20 * q + 0.12 * air,
    )
}

struct Pose {
    /// Body centre along the lane, in pixels.
    x: f32,
    lift: f32,
    sx: f32,
    sy: f32,
    /// Where the eyes look: -1 left .. 1 right.
    dir: f32,
    /// 1 → 0 dust cloud after each landing while running.
    puff: f32,
}

fn pose(
    mood: Mood,
    ms: u64,
    tr: &Track,
    lane_w: f32,
    u: f32,
    live: bool,
    poke: Option<u64>,
) -> Pose {
    let lo = MARGIN + AW * u * 1.15;
    let hi = (lane_w - lo).max(lo);
    let t = ms as f32 / 1000.0;
    let running = mood == Mood::Think && live;
    let dt = ms.saturating_sub(tr.t0);
    let dist = if running { tr.u + travel(dt) } else { tr.u };
    let (off, leftward) = fold(dist, (hi - lo).max(1.0));
    let mut p = Pose {
        x: hi - off,
        lift: 0.0,
        sx: 1.0,
        sy: 1.0,
        dir: if hi - off > lane_w / 2.0 { -0.6 } else { 0.6 },
        puff: 0.0,
    };
    match mood {
        Mood::Think if live => {
            let ph = hops(dt).1;
            (p.lift, p.sx, p.sy) = hop_pose(ph, 2.0 * u);
            p.dir = if leftward { -1.0 } else { 1.0 };
            p.puff = (1.0 - ph / 0.3).max(0.0);
        }
        Mood::Happy if live => (p.lift, p.sx, p.sy) = hop_pose((t / 0.55).fract(), 2.0 * u),
        Mood::Ask if live => {
            let b = (t * TAU * 1.5).sin().abs();
            (p.lift, p.sx, p.sy) = (b * u, 1.04 - 0.06 * b, 0.92 + 0.12 * b);
        }
        Mood::Oops => (p.sx, p.sy) = (1.08, 0.86),
        Mood::Sleep => (p.sx, p.sy) = (1.05, 0.9),
        _ => {}
    }
    if let Some(ms) = poke {
        let tau = ms as f32 / 1000.0;
        let q = 0.3 * (-tau / 0.28).exp() * (tau * TAU * 4.2).cos();
        p.sy -= q;
        p.sx += 0.75 * q;
    }
    p
}

/// The slime itself, drawn into a small canvas around `cx`. Face details
/// are whole pixels so they stay crisp while the body glides.
fn sprite(rows: usize, cx: f32, p: &Pose, mood: Mood, ms: u64, bg: Rgb, poked: bool) -> Canvas {
    let mut cv = Canvas::new(SPRITE_COLS, rows);
    let h = cv.h as f32;
    let (pal, t, u) = (palette(mood), ms as f32 / 1000.0, h / TALL_PX);
    let ground = h - 0.8;
    let air = 1.0 - 0.45 * (p.lift / (2.0 * u)).clamp(0.0, 1.0);
    let (aw, ah) = (AW * u, AH * u);

    if p.puff > 0.0 {
        let bx = cx - p.dir * (aw * p.sx + 0.8);
        cv.ellipse(bx, ground - 0.4, 1.0, 0.7, pal.rim, 0.4 * p.puff);
        cv.ellipse(
            bx - p.dir * 1.4,
            ground - 0.9,
            0.7,
            0.5,
            pal.rim,
            0.28 * p.puff,
        );
    }
    cv.ellipse(
        cx,
        ground + 0.3,
        aw * 1.1 * air,
        0.7,
        mix(bg, [0.0; 3], 0.62),
        0.65 * air,
    );

    let (rx, hh) = (aw * p.sx, ah * p.sy);
    let (base, ry) = (ground - p.lift, hh - 0.5);
    let (top, cyc) = (base - hh, base - 0.5);
    cv.paint(
        cx - rx - 1.0,
        top - 1.0,
        cx + rx + 1.0,
        base + 1.0,
        |x, y| {
            let d = ellipse_d(x - cx, y - cyc, rx, ry).max(y - (base + 0.15));
            let k = cover(d);
            if k <= 0.0 {
                return None;
            }
            let v = ((y - top) / hh).clamp(0.0, 1.0);
            let mut c = mix(pal.light, pal.body, smooth(0.0, 0.5, v));
            c = mix(c, pal.deep, smooth(0.5, 1.0, v) * 0.6);
            let depth = (-d).max(0.0);
            c = mix(
                c,
                scale(pal.deep, 0.75),
                (1.0 - smooth(0.0, 0.8, depth)) * 0.5,
            );
            let spec = smooth(
                1.0,
                0.3,
                ((x - (cx - 0.42 * rx)) / (1.3 * u.max(0.8)))
                    .hypot((y - (top + 0.2 * hh + 0.3)) / 0.7),
            );
            Some((mix(c, WHITE, spec * 0.8), k))
        },
    );

    let (cxi, exi, eh) = (
        cx.floor() as i32,
        if u >= 1.0 { 2 } else { 1 },
        if u >= 1.0 { 2 } else { 1 },
    );
    let ey = (base - 0.6 * hh).round() as i32;
    let gaze = if p.dir.abs() > 0.8 {
        p.dir.signum() as i32
    } else {
        0
    };
    let closed = match mood {
        Mood::Sleep => 1.0,
        Mood::Idle => 0.0,
        _ => blink(t),
    } > 0.5;
    for side in [-1, 1] {
        let ex = cxi + side * exi + gaze;
        match mood {
            Mood::Happy if eh >= 2 => {
                cv.over(ex - 1, ey + 1, INK, 0.95);
                cv.over(ex, ey, INK, 0.95);
                cv.over(ex + 1, ey + 1, INK, 0.95);
            }
            Mood::Happy => cv.over(ex, ey, INK, 0.95),
            _ => {
                let rows = if closed {
                    1
                } else {
                    eh + i32::from(poked && eh >= 2)
                };
                let y0 = ey + eh - rows;
                for dy in 0..rows {
                    cv.over(ex, y0 + dy, INK, 1.0);
                }
            }
        }
        if mood != Mood::Oops {
            let a = if mood == Mood::Happy { 0.8 } else { 0.6 };
            cv.over(cxi + side * (exi + 1) + gaze, ey + eh, BLUSH, a);
        }
    }
    match mood {
        Mood::Happy => {
            let mouth = [92.0, 22.0, 46.0];
            for dx in if eh >= 2 { -1..=1 } else { 0..=0 } {
                cv.over(cxi + gaze + dx, ey + eh, mouth, 0.9);
            }
        }
        Mood::Oops => {
            let ph = (t / 1.4).fract();
            cv.over(
                cxi + exi + 1 + gaze,
                ey + 1 + (2.0 * ph) as i32,
                [150.0, 210.0, 255.0],
                0.95 * (1.0 - ph * ph),
            );
        }
        _ => {}
    }
    cv
}

/// Draw the slime into its lane and register it as a poke target. Cells
/// outside the sprite are never touched, so the lane stays text-free.
pub fn draw(f: &mut Frame, app: &App, lane: Rect) {
    let th = app.theme();
    let Some(bg) = rgb(th.background).filter(|_| slime::pixel_ok(th)) else {
        return;
    };
    let (wall, ms) = (slime::now_ms(), slime::clock(app));
    let (mood, live) = (slime::mood(app), app.anim.motion != Motion::Off);
    let poke = app
        .anim
        .since(app.anim.poke, wall)
        .filter(|&t| live && t < 1500);
    let u = lane.height as f32 * 2.0 / TALL_PX;
    let p = pose(mood, ms, &app.anim.track, lane.width as f32, u, live, poke);

    let left = (lane.x as i32 + p.x.floor() as i32 - 8).max(0);
    let at = Rect::new(left as u16, lane.y, SPRITE_COLS as u16, lane.height);
    let cx = (lane.x as f32 + p.x) - left as f32;
    let poked = poke.is_some_and(|t| t < 550);
    sprite(lane.height as usize, cx, &p, mood, ms, bg, poked).blit(f.buffer_mut(), at, bg, false);

    let x0 = (lane.x as f32 + p.x - 5.5).floor().max(lane.x as f32) as u16;
    let x1 = ((lane.x as f32 + p.x + 5.5).ceil() as u16).min(lane.right());
    app.hits.borrow_mut().push(HitZone {
        x: x0,
        y: lane.y,
        w: x1.saturating_sub(x0),
        h: lane.height,
        hit: Hit::Mascot,
    });

    let glyph = match mood {
        Mood::Ask => Some(('?', th.warning)),
        Mood::Oops => Some(('!', th.error)),
        Mood::Happy => Some(('*', th.glow)),
        Mood::Sleep => Some(('z', th.muted)),
        _ => None,
    };
    let gx = (lane.x as f32 + p.x + 5.0).floor() as u16;
    if let Some((c, fg)) = glyph.filter(|_| gx < lane.right()) {
        let cell = &mut f.buffer_mut()[(gx, lane.y)];
        if cell.symbol().trim().is_empty() {
            cell.set_char(c);
            cell.set_fg(fg);
        }
    }
}

/// True while the lane slime needs frames although no run is active: the
/// happy hops, plus the one redraw that ends a mood or starts the nap.
pub fn lively(app: &App, now: u64) -> bool {
    let a = &app.anim;
    app.tab == Tab::Chat
        && a.motion != Motion::Off
        && (a.done.is_some_and(|(t, ok)| {
            let age = now.saturating_sub(t);
            if ok {
                age < HAPPY_MS + EDGE_MS
            } else {
                (OOPS_MS..OOPS_MS + EDGE_MS).contains(&age)
            }
        }) || (SLEEP_MS..SLEEP_MS + EDGE_MS).contains(&now.saturating_sub(a.active)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fold_bounces_between_the_ends() {
        let len = 50.0;
        for i in 0..2000 {
            let (off, _) = fold(i as f32 * 0.37, len);
            assert!((0.0..=len).contains(&off), "{off}");
        }
        assert_eq!(fold(10.0, len), (10.0, true));
        assert_eq!(fold(60.0, len), (40.0, false));
        assert_eq!(fold(-10.0, len), (10.0, false));
    }

    #[test]
    fn travel_only_ever_moves_forward_and_lands_whole() {
        let mut last = 0.0;
        for dt in (0..5000).step_by(7) {
            let x = travel(dt);
            assert!(x >= last - 1e-4, "{dt}: {x} < {last}");
            last = x;
        }
        let one = travel(HOP_MS as u64);
        assert!((one - HOP_COLS).abs() < 0.05, "one hop is {one} columns");
    }

    #[test]
    fn hop_poses_are_continuous_across_hops() {
        let (a, b) = (hop_pose(0.0, 2.0), hop_pose(0.999, 2.0));
        assert!((a.1 - b.1).abs() < 0.02 && (a.2 - b.2).abs() < 0.02);
        assert_eq!(a.0, 0.0);
        let (peak, ..) = hop_pose(0.47, 2.0);
        assert!((peak - 2.0).abs() < 0.01, "peak lift is {peak}");
    }

    #[test]
    fn the_slime_stays_inside_the_lane_and_turns_around() {
        let tr = Track::default();
        let mut heads = [false; 2];
        for w in [50.0f32, 80.0, 133.0] {
            for ms in (0..60_000).step_by(37) {
                let p = pose(Mood::Think, ms, &tr, w, 1.0, true, None);
                assert!(p.x >= MARGIN + AW && p.x <= w - MARGIN - AW, "{w}: {}", p.x);
                heads[(p.dir > 0.0) as usize] = true;
            }
        }
        assert_eq!(heads, [true, true], "runs both ways");
    }

    #[test]
    fn track_parks_where_the_run_ended_and_resumes_from_there() {
        let mut tr = Track::default();
        tr.advance(true, 1_000, true);
        assert_eq!((tr.u, tr.t0), (0.0, 1_000));
        tr.advance(true, 1_500, true);
        assert_eq!(tr.t0, 1_000, "no edge, no change");
        tr.advance(false, 3_100, true);
        assert!(tr.u > 20.0, "walked {}", tr.u);
        let parked = tr.u;
        tr.advance(false, 9_999, true);
        assert_eq!(tr.u, parked);
        tr.advance(true, 10_000, false);
        tr.advance(false, 20_000, false);
        assert_eq!(tr.u, parked, "motion off walks nowhere");
    }

    #[test]
    fn motion_off_poses_are_at_rest() {
        let tr = Track::default();
        for mood in [Mood::Think, Mood::Happy, Mood::Ask] {
            let p = pose(mood, 0, &tr, 90.0, 1.0, false, None);
            assert_eq!((p.lift, p.sx, p.sy, p.puff), (0.0, 1.0, 1.0, 0.0));
        }
    }
}
