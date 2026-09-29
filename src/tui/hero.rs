//! The slime: a procedurally shaded mascot, its wordmark, and ambient
//! bubbles, all drawn into pixel canvases. Every function is pure over its
//! inputs (clock included), so any frame is reproducible in tests.
use std::f32::consts::{PI, TAU};

use super::gfx::{cover, ellipse_d, mix, noise, scale, smin, smooth, Canvas, Rgb};
use super::slime::Mood;

/// Seconds into the splash when the first drop hits the ground.
pub const BOOT_LAND: f32 = 0.62;

const WHITE: Rgb = [246.0, 250.0, 255.0];
const INK: Rgb = [8.0, 16.0, 44.0];
const BLUSH: Rgb = [255.0, 130.0, 178.0];

#[derive(Clone, Copy, Debug)]
pub struct Scene {
    pub mood: Mood,
    /// Animation clock in ms.
    pub ms: u64,
    /// Milliseconds into the splash while it plays.
    pub boot: Option<u64>,
    /// Milliseconds since the last keystroke.
    pub tap: Option<u64>,
    /// Milliseconds since the slime was clicked.
    pub poke: Option<u64>,
    /// Where the eyes look, each axis -1..1.
    pub gaze: (f32, f32),
}

struct Pal {
    light: Rgb,
    body: Rgb,
    deep: Rgb,
    rim: Rgb,
}

fn palette(m: Mood) -> Pal {
    match m {
        Mood::Think => Pal {
            light: [186.0, 240.0, 255.0],
            body: [64.0, 204.0, 238.0],
            deep: [14.0, 96.0, 150.0],
            rim: [220.0, 248.0, 255.0],
        },
        Mood::Ask => Pal {
            light: [255.0, 250.0, 170.0],
            body: [232.0, 226.0, 92.0],
            deep: [138.0, 128.0, 34.0],
            rim: [255.0, 252.0, 210.0],
        },
        Mood::Oops => Pal {
            light: [255.0, 196.0, 206.0],
            body: [255.0, 122.0, 146.0],
            deep: [148.0, 46.0, 84.0],
            rim: [255.0, 225.0, 230.0],
        },
        // the house colour: azure gel over deep royal blue
        _ => Pal {
            light: [178.0, 214.0, 255.0],
            body: [66.0, 128.0, 248.0],
            deep: [20.0, 48.0, 142.0],
            rim: [208.0, 230.0, 255.0],
        },
    }
}

struct Pose {
    sx: f32,
    sy: f32,
    lift: f32,
    wob: f32,
    /// Seconds since the last touchdown, while a ripple is alive.
    land: Option<f32>,
}

fn lerp1(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Quantise a -1..1 wave to {-1, 0, 1}: resting motion advances in a few
/// distinct poses, like a sprite, so an idle screen repaints only a couple of
/// times per second instead of every frame.
fn stepped(x: f32) -> f32 {
    x.round()
}

fn pose(s: &Scene) -> Pose {
    let t = s.ms as f32 / 1000.0;
    let mut p = Pose {
        sx: 1.0,
        sy: 1.0,
        lift: 0.0,
        wob: 0.0,
        land: None,
    };
    match s.mood {
        Mood::Idle | Mood::Happy if s.boot.is_none() => {
            let (period, rest) = if s.mood == Mood::Happy {
                (2.1, 0.5)
            } else {
                (5.0, 3.4)
            };
            let c = t % period;
            if c < rest {
                let b = stepped((t * TAU / 3.0).sin() * 1.2);
                p.sy += 0.035 * b;
                p.sx -= 0.022 * b;
            } else if c < rest + 0.22 {
                let k = smooth(0.0, 1.0, (c - rest) / 0.22);
                p.sx += 0.15 * k;
                p.sy -= 0.20 * k;
            } else if c < rest + 0.72 {
                let a = (c - rest - 0.22) / 0.5;
                p.lift = 12.0 * a * (1.0 - a);
                let stretch = (PI * a).cos().powi(2);
                p.sy = lerp1(0.8, 1.0 + 0.17 * stretch, smooth(0.0, 0.18, a));
                p.sx = 1.0 - 0.6 * (p.sy - 1.0);
            } else {
                let tau = c - rest - 0.72;
                let q = (-tau / 0.11).exp() * (tau * TAU * 3.2).cos();
                p.sy -= 0.24 * q;
                p.sx += 0.18 * q;
                p.land = Some(tau);
                p.wob += 1.2 * (-tau / 0.3).exp();
            }
        }
        Mood::Idle | Mood::Happy => {
            let b = stepped((t * TAU / 3.0).sin() * 1.2);
            p.sy += 0.035 * b;
            p.sx -= 0.022 * b;
        }
        Mood::Think => {
            let b = stepped((t * TAU * 0.9).sin() * 1.2);
            p.sy += 0.05 * b;
            p.sx -= 0.03 * b;
            p.lift = 0.7 * ((t * TAU * 0.9).cos() * 1.2).round().abs().min(1.0);
        }
        Mood::Ask => {
            p.wob = 0.5 + 0.8 * (t * TAU * 6.0).sin().abs();
            p.sy -= 0.03;
        }
        Mood::Oops => {
            p.sy = 0.9 + 0.02 * (t * TAU * 0.8).sin();
            p.sx = 1.07;
            p.wob = 0.25;
        }
        Mood::Sleep => {
            let b = stepped((t * TAU / 3.4).sin() * 1.2);
            p.sy += 0.05 * b;
            p.sx -= 0.03 * b;
            p.wob = 0.0;
        }
    }
    if let Some(ms) = s.tap {
        let tau = ms as f32 / 1000.0;
        let q = 0.10 * (-tau / 0.09).exp() * (tau * TAU * 7.0).sin();
        p.sy -= q;
        p.sx += 0.7 * q;
    }
    if let Some(ms) = s.poke {
        let tau = ms as f32 / 1000.0;
        let q = 0.30 * (-tau / 0.28).exp() * (tau * TAU * 4.2).cos();
        p.sy -= q;
        p.sx += 0.75 * q;
        p.wob += 2.0 * (-tau / 0.35).exp();
    }
    if let Some(bt) = s.boot {
        let tau = bt as f32 / 1000.0 - BOOT_LAND;
        if tau >= 0.0 {
            let q = 0.55 * (-tau / 0.16).exp() * (tau * TAU * 2.4).cos();
            p.sy -= q;
            p.sx += 0.9 * q;
            p.wob += 1.5 * (-tau / 0.4).exp();
            p.land = Some(tau);
        }
    }
    p
}

/// 0..1 eyelid closure: a blink every few seconds, sometimes doubled.
fn blink(t: f32) -> f32 {
    let n = (t / 3.3).floor();
    let start = n * 3.3 + 0.9 + 1.6 * noise(n as u32 + 7);
    let k = t - start;
    let one = if (0.0..0.14).contains(&k) {
        (PI * k / 0.14).sin()
    } else {
        0.0
    };
    let k2 = k - 0.24;
    let two = if noise(n as u32 + 91) > 0.65 && (0.0..0.12).contains(&k2) {
        (PI * k2 / 0.12).sin()
    } else {
        0.0
    };
    one.max(two)
}

const Q_GLYPH: [&str; 5] = [".##.", "#..#", "..#.", "....", "..#."];
const Z_GLYPH: [&str; 5] = ["###", "..#", ".#.", "#..", "###"];
const HEART: [&str; 4] = [".#.#.", "#####", ".###.", "..#.."];

/// Design grid: 34 × 24 pixels per 12 rows; `rows` scales it uniformly.
pub fn cols_for(rows: usize) -> usize {
    (rows as f32 * 34.0 / 12.0).round() as usize
}

pub fn render(rows: usize, s: &Scene, bg: Rgb) -> Canvas {
    let mut cv = Canvas::new(cols_for(rows), rows);
    let (w, h) = (cv.w as f32, cv.h as f32);
    let u = h / 24.0;
    let (cx, ground) = (w / 2.0, h - 3.6 * u);
    let t = s.ms as f32 / 1000.0;
    let pal = palette(s.mood);
    let shadow = mix(bg, [0.0, 0.0, 0.0], 0.62);

    // Splash: a single drop falls before the slime exists.
    let boot_t = s.boot.map(|b| b as f32 / 1000.0);
    if let Some(tb) = boot_t {
        if tb < BOOT_LAND {
            let k = tb / BOOT_LAND;
            let y = -3.0 * u + (ground + 2.5 * u) * k * k;
            let ry = (2.2 + 2.2 * k) * u;
            cv.ellipse(
                cx,
                ground + 0.7 * u,
                6.0 * u * k,
                1.2 * u * k,
                shadow,
                0.5 * k,
            );
            cv.ellipse(cx, y, 1.6 * u, ry, pal.body, 0.95);
            cv.disc(cx - 0.5 * u, y - ry * 0.35, 0.5 * u, WHITE, 0.8);
            return cv;
        }
    }

    let p = pose(s);
    let (aw, hh, fh) = (12.2 * u, 11.8 * u, 1.6 * u);
    let base_y = ground - p.lift * u;

    // Soft aura so the gel glows against the dark.
    cv.paint(0.0, 0.0, w, h, |x, y| {
        let r = ((x - cx) / (w * 0.5)).powi(2) + ((y - (ground - 7.0 * u)) / (h * 0.55)).powi(2);
        let a = 0.11 * (-r * 2.4).exp();
        (a > 0.004).then_some((pal.body, a))
    });
    // Contact shadow and a little wet puddle; both shrink while airborne.
    let air = 1.0 - 0.4 * (p.lift / 4.0).clamp(0.0, 1.0);
    let sr = aw * p.sx * 1.06 * air;
    cv.paint(
        cx - sr - 3.0 * u,
        ground - 3.0 * u,
        cx + sr + 3.0 * u,
        ground + 4.0 * u,
        |x, y| {
            let d = ellipse_d(x - cx, y - (ground + 0.7 * u), sr, 1.7 * u);
            let a = 0.55 * (1.0 - smooth(-2.0 * u, 1.4 * u, d));
            (a > 0.01).then_some((shadow, a))
        },
    );
    cv.ellipse(
        cx,
        ground + 0.6 * u,
        sr * 0.92,
        1.1 * u,
        pal.deep,
        0.28 * air,
    );
    cv.ellipse(
        cx - sr * 0.35,
        ground + 0.4 * u,
        sr * 0.22,
        0.35 * u,
        WHITE,
        0.16 * air,
    );

    // Ripple after every touchdown.
    if let Some(tau) = p.land.filter(|&x| x < 0.75) {
        let r = aw * (0.85 + 0.9 * tau / 0.75);
        let a = 0.42 * (1.0 - tau / 0.75).powi(2);
        cv.ring(cx, ground + 0.7 * u, r, r * 0.13, 0.9, pal.rim, a);
    }
    // Splat droplets thrown out by the first landing.
    if let Some(tb) = boot_t {
        let tau = tb - BOOT_LAND;
        if (0.0..0.8).contains(&tau) {
            for i in 0..10u32 {
                let ang = PI * (0.08 + 0.84 * i as f32 / 9.0) + (noise(i + 3) - 0.5) * 0.3;
                let v = (10.0 + 14.0 * noise(i + 40)) * u;
                let x = cx + ang.cos() * v * tau * 1.8;
                let y = ground - 1.0 * u - ang.sin() * v * 1.9 * tau + 0.5 * 60.0 * u * tau * tau;
                if y < ground + u {
                    cv.disc(
                        x,
                        y,
                        (0.6 + 0.6 * noise(i + 90)) * u,
                        pal.light,
                        1.0 - tau / 0.8,
                    );
                }
            }
        }
    }

    // ── body ─────────────────────────────────────────────────────────
    let phase = t * TAU * 1.25;
    let shear = |by: f32| p.wob * u * (phase + by / (2.2 * u)).sin() * (by / hh).clamp(0.0, 1.2);
    let to_screen = |bx: f32, by: f32| (cx + (bx + shear(by)) * p.sx, base_y - by * p.sy);
    let (tip_x, tip_y) = (
        0.12 * aw + 0.5 * u * (phase * 1.3).sin() * p.wob.min(1.5),
        hh * 0.98 + 0.2 * u,
    );
    let k_dist = 0.5 * (p.sx + p.sy);
    let (x0, x1) = (
        cx - aw * p.sx * 1.4 - 4.0 * u,
        cx + aw * p.sx * 1.4 + 4.0 * u,
    );
    let (y0, y1) = (base_y - (hh + 5.0 * u) * p.sy, base_y + fh * p.sy + 2.0 * u);
    // Fade the body in over the first frames of a splat so the drop merges.
    let born = boot_t.map_or(1.0, |tb| smooth(0.0, 0.05, tb - BOOT_LAND));
    cv.paint(x0, y0, x1, y1, |x, y| {
        let by = (base_y - y) / p.sy;
        let bx = (x - cx) / p.sx - shear(by);
        let tt = (by / hh).clamp(0.0, 1.0);
        let taper = 1.0 + 0.10 * tt;
        let ry = if by >= 0.0 { hh } else { fh };
        let body = ellipse_d(bx * taper, by, aw, ry);
        let tip = ellipse_d(bx - tip_x, by - tip_y, 1.7 * u, 2.1 * u);
        let d = smin(body, tip, 2.0 * u) * k_dist;
        let cov = cover(d);
        if cov <= 0.0 {
            return None;
        }
        let depth = (-d).max(0.0);
        let low = 1.0 - tt;
        let mut c = mix(pal.light, pal.body, smooth(0.0, 0.5, low));
        c = mix(c, pal.deep, smooth(0.5, 1.0, low) * 0.62);
        // tube light: bright band just inside a darker outline
        let rim = (-((depth - 1.6 * u) / (1.3 * u)).powi(2)).exp();
        c = mix(c, pal.rim, rim * 0.42);
        c = mix(
            c,
            scale(pal.deep, 0.7),
            (1.0 - smooth(0.0, 1.1 * u, depth)) * 0.6,
        );
        let side = 0.5 + 0.5 * (bx / aw).clamp(-1.0, 1.0);
        c = mix(
            c,
            pal.light,
            (-depth / (2.6 * u)).exp() * low * low * side * 0.4,
        );
        // bubbles drifting up inside the gel
        for k in 0..3u32 {
            let ph = (t * 0.11 + k as f32 * 0.34).fract();
            let bxk = (-0.45 + 0.45 * k as f32) * aw + 1.4 * u * (t * 0.9 + k as f32 * 2.0).sin();
            let byk = (0.08 + 0.62 * ph) * hh;
            let r = (0.9 + 0.35 * k as f32) * u;
            let ring = smooth(0.9, 0.2, ((bx - bxk).hypot(by - byk) - r).abs());
            c = mix(
                c,
                WHITE,
                ring * 0.35 * (1.0 - smooth(0.7, 1.0, ph)) * smooth(0.0, 3.0 * u, depth),
            );
        }
        // glossy highlights
        let (hx, hy) = (
            (bx + 0.40 * aw) / (0.19 * aw),
            (by - 0.72 * hh) / (0.085 * hh),
        );
        let spec = smooth(1.0, 0.45, hx.hypot(hy));
        c = mix(c, WHITE, spec * 0.75);
        let dot = (bx + 0.12 * aw).hypot(by - 0.86 * hh) / u;
        c = mix(c, WHITE, smooth(1.0, 0.5, dot) * 0.8);
        Some((c, (0.86 + 0.14 * rim + spec * 0.2).min(1.0) * cov * born))
    });
    if born < 0.5 {
        return cv;
    }

    // ── face ─────────────────────────────────────────────────────────
    let poked = s.poke.is_some_and(|ms| ms < 550);
    let top_y = base_y - hh * p.sy;
    let booting = boot_t.map(|tb| tb - BOOT_LAND);
    let mut closed = blink(t);
    match s.mood {
        Mood::Sleep => closed = 1.0,
        _ => {
            if let Some(tau) = booting.filter(|&x| x < 0.62) {
                closed = closed.max(1.0 - smooth(0.42, 0.6, tau));
            }
        }
    }
    let wide = matches!(s.mood, Mood::Ask) || poked;
    let (ex, ey) = (0.32 * aw, 0.47 * hh);
    let (gx, gy) = match s.mood {
        Mood::Think => (
            ((t * 1.7).cos() * 1.6).round() / 2.0,
            ((t * 1.7).sin() * 1.0).round() / 2.0 - 0.3,
        ),
        _ => s.gaze,
    };
    for side in [-1.0f32, 1.0] {
        let (scx, scy) = to_screen(side * ex, ey);
        match s.mood {
            Mood::Happy => {
                let pts: Vec<_> = (0..=8)
                    .map(|i| {
                        let a = PI * i as f32 / 8.0;
                        to_screen(
                            side * ex + a.cos() * 2.3 * u,
                            ey + a.sin() * 1.7 * u - 0.6 * u,
                        )
                    })
                    .collect();
                cv.path(&pts, 1.1 * u.max(0.8), INK, 0.95);
            }
            Mood::Oops => {
                let r = 1.8 * u;
                let (a, b) = (
                    to_screen(side * ex - r, ey - r),
                    to_screen(side * ex + r, ey + r),
                );
                let (c, d) = (
                    to_screen(side * ex - r, ey + r),
                    to_screen(side * ex + r, ey - r),
                );
                cv.line(a.0, a.1, b.0, b.1, 1.0 * u.max(0.8), INK, 0.95);
                cv.line(c.0, c.1, d.0, d.1, 1.0 * u.max(0.8), INK, 0.95);
            }
            _ if closed > 0.9 => {
                let pts: Vec<_> = (0..=8)
                    .map(|i| {
                        let a = PI * i as f32 / 8.0;
                        to_screen(
                            side * ex + a.cos() * 2.3 * u,
                            ey - a.sin() * 1.1 * u + 0.4 * u,
                        )
                    })
                    .collect();
                cv.path(&pts, 1.1 * u.max(0.8), INK, 0.95);
            }
            _ => {
                let (rx, ry0) = if wide {
                    (2.5 * u, 3.4 * u)
                } else {
                    (1.9 * u, 2.7 * u)
                };
                let ry = (ry0 * (1.0 - 0.9 * closed)).max(0.35 * u);
                let off_x = gx * 0.9 * u * p.sx;
                let off_y = gy * 0.7 * u * p.sy;
                let (ecx, ecy) = (scx + off_x, scy + off_y);
                cv.ellipse(ecx, ecy, rx * p.sx, ry * p.sy, INK, 1.0);
                let g = u.max(0.7);
                cv.disc(
                    ecx - 0.55 * g * p.sx,
                    ecy - 0.95 * g * p.sy * (1.0 - 0.5 * closed),
                    0.62 * g,
                    WHITE,
                    0.95 * (1.0 - closed),
                );
                cv.disc(
                    ecx + 0.5 * g,
                    ecy + 0.9 * g * p.sy,
                    0.3 * g,
                    WHITE,
                    0.55 * (1.0 - closed),
                );
            }
        }
    }
    // cheeks
    let blush = match s.mood {
        Mood::Happy => 0.78,
        Mood::Oops => 0.14,
        _ => 0.62,
    };
    for side in [-1.0f32, 1.0] {
        let (bx, by) = to_screen(side * 0.56 * aw, 0.27 * hh);
        cv.ellipse(bx, by, 2.2 * u * p.sx, 1.2 * u * p.sy, BLUSH, blush);
    }
    // mouth
    let my = 0.22 * hh;
    let thick = 1.0 * u.max(0.8);
    let arc = |from: f32, to: f32, r: f32, cy: f32, flip: f32| -> Vec<(f32, f32)> {
        (0..=8)
            .map(|i| {
                let a = from + (to - from) * i as f32 / 8.0;
                to_screen(a.cos() * r, cy + flip * a.sin() * r * 0.75)
            })
            .collect()
    };
    if poked || s.mood == Mood::Think {
        let (mx, mys) = to_screen(0.0, my);
        let r = if poked { 1.7 } else { 1.0 } * u;
        cv.ellipse(mx, mys, r * p.sx, r * 1.25 * p.sy, INK, 0.95);
    } else {
        match s.mood {
            Mood::Happy => {
                let (mx, mys) = to_screen(0.0, my + 0.8 * u);
                let (rx, ry) = (2.9 * u * p.sx, 2.5 * u * p.sy);
                cv.paint(
                    mx - rx - 1.0,
                    mys - 1.0,
                    mx + rx + 1.0,
                    mys + ry + 1.0,
                    |x, y| {
                        if y < mys {
                            return None;
                        }
                        let k = cover(ellipse_d(x - mx, y - mys, rx, ry));
                        (k > 0.0).then_some(([92.0, 22.0, 46.0], k))
                    },
                );
                cv.ellipse(
                    mx,
                    mys + ry * 0.72,
                    rx * 0.5,
                    ry * 0.32,
                    [255.0, 138.0, 160.0],
                    0.95,
                );
            }
            Mood::Oops => {
                let pts = arc(0.35, PI - 0.35, 2.2 * u, my - 0.6 * u, 1.0);
                cv.path(&pts, thick, INK, 0.9);
            }
            Mood::Sleep => {
                let (mx, mys) = to_screen(0.0, my);
                let r = (0.8 + 0.25 * (t * TAU / 3.4).sin()) * u;
                cv.ellipse(mx, mys, r * p.sx, r * p.sy, INK, 0.85);
            }
            Mood::Ask => {
                let pts: Vec<_> = (0..=8)
                    .map(|i| {
                        let x = -2.0 * u + 4.0 * u * i as f32 / 8.0;
                        to_screen(x, my + 0.45 * u * (i as f32 * 1.6 + t * 8.0).sin())
                    })
                    .collect();
                cv.path(&pts, thick * 0.9, INK, 0.9);
            }
            _ => {
                let pts = arc(PI + 0.5, TAU - 0.5, 2.7 * u, my + 1.3 * u, 1.0);
                cv.path(&pts, thick, INK, 0.9);
            }
        }
    }

    // ── mood extras ──────────────────────────────────────────────────
    let head_x = cx + 0.55 * aw;
    match s.mood {
        Mood::Ask => {
            let bounce = 1.3 * u * (t * TAU * 1.6).sin().abs();
            cv.bitmap(
                &Q_GLYPH,
                head_x,
                top_y - 8.0 * u - bounce,
                (1.4 * u).max(1.0),
                [255.0, 236.0, 120.0],
                1.0,
            );
        }
        Mood::Sleep => {
            for i in 0..3u32 {
                let ph = (t * 0.3 + i as f32 / 3.0).fract();
                let a = (PI * ph).sin() * 0.9;
                let sz = (1.0 + 0.6 * ph) * u.max(0.8);
                cv.bitmap(
                    &Z_GLYPH,
                    head_x + i as f32 * 2.2 * u + ph * 3.0 * u,
                    top_y - 1.0 * u - ph * 9.0 * u,
                    sz,
                    pal.rim,
                    a,
                );
            }
        }
        Mood::Happy => {
            for i in 0..4u32 {
                let ph = (t * 0.7 + noise(i + 5)).fract();
                let x = cx + (noise(i + 11) - 0.5) * 2.0 * (aw + 3.0 * u);
                let y = top_y + 4.0 * u - ph * 12.0 * u;
                cv.bitmap(
                    &HEART,
                    x,
                    y,
                    (0.7 * u).max(0.7),
                    [255.0, 150.0, 190.0],
                    (PI * ph).sin(),
                );
            }
        }
        Mood::Oops => {
            let ph = (t / 1.4).fract();
            let (sx_, sy_) = (cx + 0.75 * aw, top_y + 3.0 * u + ph * 8.0 * u);
            cv.ellipse(
                sx_,
                sy_,
                0.9 * u,
                1.4 * u,
                [150.0, 210.0, 255.0],
                0.9 * (1.0 - ph * ph),
            );
        }
        Mood::Think => {
            for i in 0..3u32 {
                let lit = ((t * 2.4 - i as f32 * 0.35).fract() < 0.5) as u8 as f32;
                let r = (0.7 + 0.35 * i as f32) * u;
                cv.disc(
                    head_x + i as f32 * 2.6 * u,
                    top_y - (1.5 + 1.8 * i as f32) * u,
                    r,
                    pal.rim,
                    0.35 + 0.55 * lit,
                );
            }
        }
        Mood::Idle => {
            for i in 0..3u32 {
                let ph = (t * 0.33 + noise(i + 21)).fract();
                let a = ((PI * ph).sin().powi(2) * 3.0).round() / 3.0;
                let x = (cx + (noise(i + 31) - 0.5) * 2.0 * (aw + 2.0 * u)).round();
                let y = top_y + (noise(i + 41) * 8.0 - 3.0) * u;
                let l = (1.0 + 1.6 * a) * u.max(0.8);
                for k in -(l as i32)..=(l as i32) {
                    let f = 1.0 - k.abs() as f32 / (l + 1.0);
                    cv.over((x + k as f32) as i32, y as i32, pal.rim, a * f);
                    cv.over(x as i32, (y + k as f32 * 0.6) as i32, pal.rim, a * f);
                }
            }
        }
    }
    cv
}

/// Background bubbles rising through the whole area, faint by design.
pub fn bubbles(cols: usize, rows: usize, ms: u64, tint: Rgb) -> Canvas {
    let mut cv = Canvas::new(cols, rows);
    let (w, h) = (cv.w as f32, cv.h as f32);
    let t = ms as f32 / 1000.0;
    let n = ((w * h) / 1500.0).clamp(4.0, 14.0) as u32;
    for i in 0..n {
        let r = 1.0 + 1.9 * noise(i * 3 + 1);
        let speed = 1.6 + 3.4 * noise(i * 3 + 2);
        let span = h + 2.0 * r + 8.0;
        // whole-pixel positions: a bubble repaints only when it crosses a
        // pixel boundary, never shimmers between frames
        let y = (h + r - (t * speed + noise(i * 11 + 5) * span) % span).round();
        let x = (noise(i * 3 + 3) * w
            + (t * 0.8 + i as f32).sin() * (1.0 + 2.0 * noise(i * 7 + 1)))
        .round();
        let a = 0.22 + 0.2 * noise(i * 5 + 9);
        cv.ring(x, y, r, r, 0.8, tint, a);
        cv.disc(
            x - r * 0.35,
            y - r * 0.35,
            (r * 0.25).max(0.35),
            [190.0, 220.0, 255.0],
            a * 1.4,
        );
    }
    cv
}

const S_GLYPH: [&str; 7] = [
    ".####.", "##..##", "##....", ".####.", "....##", "##..##", ".####.",
];
const U_GLYPH: [&str; 7] = [
    "##..##", "##..##", "##..##", "##..##", "##..##", "##..##", ".####.",
];
const I_GLYPH: [&str; 7] = [
    ".####.", "..##..", "..##..", "..##..", "..##..", "..##..", ".####.",
];

fn dot(g: &[&str; 7], i: i32, j: i32) -> f32 {
    if !(0..7).contains(&j) || i < 0 {
        return 0.0;
    }
    (g[j as usize].as_bytes().get(i as usize) == Some(&b'#')) as u8 as f32
}

/// Bilinear sample so hard bitmap dots melt into blobby, rounded strokes.
fn soft(g: &[&str; 7], gx: f32, gy: f32) -> f32 {
    let (fx, fy) = (gx - 0.5, gy - 0.5);
    let (i, j) = (fx.floor(), fy.floor());
    let (wx, wy) = (fx - i, fy - j);
    let (i, j) = (i as i32, j as i32);
    let top = dot(g, i, j) * (1.0 - wx) + dot(g, i + 1, j) * wx;
    let bot = dot(g, i, j + 1) * (1.0 - wx) + dot(g, i + 1, j + 1) * wx;
    top * (1.0 - wy) + bot * wy
}

pub fn word_cols(rows: usize) -> usize {
    (23.0 * (rows * 2) as f32 / 9.0).ceil() as usize
}

/// Glossy gel "SUI". `reveal` rises the letters 0..1 out of the puddle.
pub fn wordmark(rows: usize, reveal: f32, ms: u64, pal_mood: Mood, bg: Rgb) -> Canvas {
    let mut cv = Canvas::new(word_cols(rows), rows);
    let (w, h) = (cv.w as f32, cv.h as f32);
    let sc = h / 9.0;
    let pal = palette(pal_mood);
    let glyphs = [&S_GLYPH, &U_GLYPH, &I_GLYPH];
    let sweep = (ms % 4600) as f32 / 4600.0 * (w + h + 30.0) - 15.0;
    let shadow = mix(bg, [0.0, 0.0, 0.0], 0.7);
    let sample = |gx: f32, gy: f32| {
        glyphs
            .iter()
            .enumerate()
            .map(|(l, g)| soft(g, gx - l as f32 * 7.5, gy))
            .fold(0.0f32, f32::max)
    };
    cv.paint(0.0, 0.0, w, h, |x, y| {
        let mask = smooth(0.0, 0.12, reveal * 1.12 - (1.0 - y / h));
        if mask <= 0.0 {
            return None;
        }
        let (gx, gy) = (x / sc - 1.0, y / sc - 1.0);
        let v = sample(gx, gy);
        let cov = smooth(0.30, 0.62, v);
        let under = smooth(0.30, 0.62, sample(gx - 0.5, gy - 0.6));
        if cov <= 0.01 {
            return (under > 0.01).then_some((shadow, under * 0.3 * mask));
        }
        let row = (gy / 7.0).clamp(0.0, 1.0);
        let mut c = mix(pal.light, pal.body, smooth(0.0, 0.5, row));
        c = mix(c, pal.deep, smooth(0.45, 1.0, row) * 0.85);
        c = mix(c, pal.rim, (1.0 - smooth(0.62, 0.95, v)) * 0.55);
        let gloss = smooth(0.85, 1.0, v) * (1.0 - smooth(0.0, 0.42, row));
        c = mix(c, WHITE, gloss * 0.45);
        let band = (-((x + y * 0.7 - sweep) / (3.0 * sc)).powi(2) * 0.5).exp();
        c = mix(c, WHITE, band * 0.55);
        Some((scale(c, 1.0), cov * mask))
    });
    cv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene(mood: Mood, ms: u64) -> Scene {
        Scene {
            mood,
            ms,
            boot: None,
            tap: None,
            poke: None,
            gaze: (0.0, 0.0),
        }
    }

    fn lit(cv: &Canvas, bg: Rgb) -> usize {
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(
            0,
            0,
            cv.w as u16,
            (cv.h / 2) as u16,
        ));
        let area = buf.area;
        cv.blit(&mut buf, area, bg, false);
        buf.content.iter().filter(|c| c.symbol() == "▀").count()
    }

    #[test]
    fn every_mood_renders_something_and_never_panics() {
        for mood in [
            Mood::Idle,
            Mood::Think,
            Mood::Happy,
            Mood::Oops,
            Mood::Ask,
            Mood::Sleep,
        ] {
            for rows in [6, 9, 12, 16] {
                for ms in (0..8000).step_by(97) {
                    let cv = render(rows, &scene(mood, ms), [12.0, 26.0, 22.0]);
                    assert_eq!(cv.w, cols_for(rows));
                    assert_eq!(cv.h, rows * 2);
                }
            }
        }
        let bg = [12.0, 26.0, 22.0];
        assert!(lit(&render(12, &scene(Mood::Idle, 500), bg), bg) > 100);
    }

    #[test]
    fn splash_starts_with_only_a_drop_then_a_slime() {
        let bg = [12.0, 26.0, 22.0];
        let mut early = scene(Mood::Idle, 0);
        early.boot = Some(100);
        let mut late = early;
        late.boot = Some(1400);
        let drop = lit(&render(12, &early, bg), bg);
        let slime = lit(&render(12, &late, bg), bg);
        assert!(drop > 0 && drop * 8 < slime, "drop {drop} vs slime {slime}");
    }

    #[test]
    fn the_house_slime_is_blue() {
        use ratatui::style::Color;
        let bg = [5.0, 9.0, 20.0];
        let cv = render(12, &scene(Mood::Idle, 500), bg);
        let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(
            0,
            0,
            cv.w as u16,
            (cv.h / 2) as u16,
        ));
        let area = buf.area;
        cv.blit(&mut buf, area, bg, false);
        let (mut sum, mut n) = ([0.0f32; 3], 0.0f32);
        for cell in &buf.content {
            for c in [cell.fg, cell.bg] {
                if let Color::Rgb(r, g, b) = c {
                    let (r, g, b) = (r as f32, g as f32, b as f32);
                    // vivid pixels only: skip the near-black backdrop and white glints
                    if r.max(g).max(b) - r.min(g).min(b) > 70.0 && b > 90.0 {
                        sum = [sum[0] + r, sum[1] + g, sum[2] + b];
                        n += 1.0;
                    }
                }
            }
        }
        assert!(n > 100.0, "the body has vivid pixels");
        let (r, g, b) = (sum[0] / n, sum[1] / n, sum[2] / n);
        assert!(
            b > g + 40.0 && g > r,
            "expected a blue slime, got ({r:.0},{g:.0},{b:.0})"
        );
    }

    #[test]
    fn frames_are_deterministic() {
        let bg = [12.0, 26.0, 22.0];
        let a = render(10, &scene(Mood::Happy, 1234), bg);
        let b = render(10, &scene(Mood::Happy, 1234), bg);
        assert_eq!(a.w, b.w);
        let mut ba =
            ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, a.w as u16, 10));
        let mut bb = ba.clone();
        let area = ba.area;
        a.blit(&mut ba, area, bg, false);
        b.blit(&mut bb, area, bg, false);
        assert_eq!(ba, bb);
    }

    #[test]
    fn hop_leaves_the_ground_and_lands_with_a_ripple() {
        let mut max_lift = 0.0f32;
        let mut rippled = false;
        for ms in (0..5200).step_by(10) {
            let p = pose(&scene(Mood::Idle, ms));
            max_lift = max_lift.max(p.lift);
            rippled |= p.land.is_some();
            assert!(p.sx > 0.5 && p.sy > 0.5);
        }
        assert!(max_lift > 2.5 && rippled);
    }

    #[test]
    fn poke_and_tap_squash_the_body() {
        let base = pose(&scene(Mood::Sleep, 0));
        let mut s = scene(Mood::Sleep, 0);
        s.poke = Some(0);
        let poked = pose(&s);
        assert!((poked.sy - base.sy).abs() > 0.05);
        assert!(poked.wob > base.wob);
    }

    #[test]
    fn wordmark_reveals_from_nothing() {
        let bg = [12.0, 26.0, 22.0];
        let none = wordmark(6, 0.0, 0, Mood::Idle, bg);
        let full = wordmark(6, 1.0, 0, Mood::Idle, bg);
        assert_eq!(lit(&none, bg), 0);
        assert!(lit(&full, bg) > 20);
    }
}
