//! A small software rasterizer. Scenes are drawn into an RGBA canvas at two
//! pixels per terminal row and folded into ratatui cells through half blocks
//! (`▀`: fg = upper pixel, bg = lower pixel). Presentation only — nothing
//! here is ever sent to a model.
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};

pub type Rgb = [f32; 3];

/// Colour quantisation step used when folding pixels into cells.
const STEP: f32 = 5.0;

pub fn rgb(c: Color) -> Option<Rgb> {
    match c {
        Color::Rgb(r, g, b) => Some([r as f32, g as f32, b as f32]),
        _ => None,
    }
}

pub fn color(c: Rgb) -> Color {
    let q = |v: f32| v.clamp(0.0, 255.0).round() as u8;
    Color::Rgb(q(c[0]), q(c[1]), q(c[2]))
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let t = t.clamp(0.0, 1.0);
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

pub fn scale(a: Rgb, k: f32) -> Rgb {
    [a[0] * k, a[1] * k, a[2] * k]
}

pub fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Deterministic hash noise in [0, 1).
pub fn noise(n: u32) -> f32 {
    let mut x = n.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    x = x.wrapping_mul(0x297A_2D39);
    x ^= x >> 15;
    (x >> 8) as f32 / 16_777_216.0
}

/// Polynomial smooth minimum — merges shapes like liquid.
pub fn smin(a: f32, b: f32, k: f32) -> f32 {
    let h = (k - (a - b).abs()).max(0.0) / k;
    a.min(b) - h * h * k * 0.25
}

/// Approximate signed distance to an axis-aligned ellipse (negative inside).
pub fn ellipse_d(px: f32, py: f32, rx: f32, ry: f32) -> f32 {
    let (rx, ry) = (rx.max(1e-3), ry.max(1e-3));
    let k0 = ((px / rx).powi(2) + (py / ry).powi(2)).sqrt();
    let k1 = ((px / (rx * rx)).powi(2) + (py / (ry * ry)).powi(2)).sqrt();
    if k1 < 1e-6 {
        return -rx.min(ry);
    }
    k0 * (k0 - 1.0) / k1
}

/// Distance to a segment, minus its radius (a capsule).
pub fn capsule_d(px: f32, py: f32, ax: f32, ay: f32, bx: f32, by: f32, r: f32) -> f32 {
    let (dx, dy) = (bx - ax, by - ay);
    let len2 = (dx * dx + dy * dy).max(1e-6);
    let t = (((px - ax) * dx + (py - ay) * dy) / len2).clamp(0.0, 1.0);
    ((px - ax - dx * t).powi(2) + (py - ay - dy * t).powi(2)).sqrt() - r
}

/// Anti-aliased coverage from a signed pixel distance.
pub fn cover(d: f32) -> f32 {
    (0.5 - d).clamp(0.0, 1.0)
}

/// Premultiplied RGBA canvas; `h` is in pixels (two per terminal row).
pub struct Canvas {
    pub w: usize,
    pub h: usize,
    px: Vec<[f32; 4]>,
}

impl Canvas {
    pub fn new(cols: usize, rows: usize) -> Self {
        Self {
            w: cols,
            h: rows * 2,
            px: vec![[0.0; 4]; cols * rows * 2],
        }
    }

    /// Erase a rectangle given in terminal cells, relative to the canvas.
    pub fn clear(&mut self, r: Rect) {
        for y in (r.y as usize * 2)..((r.y + r.height) as usize * 2).min(self.h) {
            for x in (r.x as usize)..((r.x + r.width) as usize).min(self.w) {
                self.px[y * self.w + x] = [0.0; 4];
            }
        }
    }

    pub fn over(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if a <= 0.0 || x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return;
        }
        let a = a.min(1.0);
        let p = &mut self.px[y as usize * self.w + x as usize];
        for i in 0..3 {
            p[i] = p[i] * (1.0 - a) + c[i] * a;
        }
        p[3] = p[3] * (1.0 - a) + a;
    }

    /// Evaluate `f` at every pixel centre inside the box and composite.
    pub fn paint(
        &mut self,
        x0: f32,
        y0: f32,
        x1: f32,
        y1: f32,
        mut f: impl FnMut(f32, f32) -> Option<(Rgb, f32)>,
    ) {
        let (ix0, iy0) = (x0.floor().max(0.0) as i32, y0.floor().max(0.0) as i32);
        let ix1 = (x1.ceil().max(0.0) as i32).min(self.w as i32);
        let iy1 = (y1.ceil().max(0.0) as i32).min(self.h as i32);
        for y in iy0..iy1 {
            for x in ix0..ix1 {
                if let Some((c, a)) = f(x as f32 + 0.5, y as f32 + 0.5) {
                    self.over(x, y, c, a);
                }
            }
        }
    }

    pub fn disc(&mut self, cx: f32, cy: f32, r: f32, c: Rgb, a: f32) {
        self.paint(
            cx - r - 1.0,
            cy - r - 1.0,
            cx + r + 1.0,
            cy + r + 1.0,
            |x, y| {
                let k = cover((x - cx).hypot(y - cy) - r);
                (k > 0.0).then_some((c, a * k))
            },
        );
    }

    pub fn ellipse(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, c: Rgb, a: f32) {
        self.paint(
            cx - rx - 1.0,
            cy - ry - 1.0,
            cx + rx + 1.0,
            cy + ry + 1.0,
            |x, y| {
                let k = cover(ellipse_d(x - cx, y - cy, rx, ry));
                (k > 0.0).then_some((c, a * k))
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn ring(&mut self, cx: f32, cy: f32, rx: f32, ry: f32, thick: f32, c: Rgb, a: f32) {
        let m = thick + 1.0;
        self.paint(
            cx - rx - m,
            cy - ry - m,
            cx + rx + m,
            cy + ry + m,
            |x, y| {
                let k = cover(ellipse_d(x - cx, y - cy, rx, ry).abs() - thick * 0.5);
                (k > 0.0).then_some((c, a * k))
            },
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn line(&mut self, ax: f32, ay: f32, bx: f32, by: f32, thick: f32, c: Rgb, a: f32) {
        let m = thick + 1.0;
        self.paint(
            ax.min(bx) - m,
            ay.min(by) - m,
            ax.max(bx) + m,
            ay.max(by) + m,
            |x, y| {
                let k = cover(capsule_d(x, y, ax, ay, bx, by, thick * 0.5));
                (k > 0.0).then_some((c, a * k))
            },
        );
    }

    /// Polyline through `pts`.
    pub fn path(&mut self, pts: &[(f32, f32)], thick: f32, c: Rgb, a: f32) {
        for w in pts.windows(2) {
            self.line(w[0].0, w[0].1, w[1].0, w[1].1, thick, c, a);
        }
    }

    /// Draw a '#'-bitmap with `s` pixels per bitmap dot.
    pub fn bitmap(&mut self, rows: &[&str], x: f32, y: f32, s: f32, c: Rgb, a: f32) {
        let w = rows.iter().map(|r| r.len()).max().unwrap_or(0) as f32 * s;
        let h = rows.len() as f32 * s;
        self.paint(x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0, |px, py| {
            let (gx, gy) = ((px - x) / s, (py - y) / s);
            if gx < 0.0 || gy < 0.0 {
                return None;
            }
            let on = rows
                .get(gy as usize)
                .and_then(|r| r.as_bytes().get(gx as usize))
                == Some(&b'#');
            on.then_some((c, a))
        });
    }

    /// Fold into terminal cells. Transparent pixels leave the cell alone; each
    /// half composites over what is already there (`fallback` when the cell
    /// has no RGB colour). `blank_only` refuses to touch cells holding text.
    pub fn blit(&self, buf: &mut Buffer, at: Rect, fallback: Rgb, blank_only: bool) {
        let area = at.intersection(buf.area);
        for cy in 0..area.height {
            for cx in 0..area.width {
                let (ix, iy) = (cx as usize, cy as usize * 2);
                if ix >= self.w || iy + 1 >= self.h {
                    continue;
                }
                let (t, b) = (self.px[iy * self.w + ix], self.px[(iy + 1) * self.w + ix]);
                if t[3] < 0.012 && b[3] < 0.012 {
                    continue;
                }
                let cell = &mut buf[(area.x + cx, area.y + cy)];
                let half = cell.symbol() == "▀";
                let blank = cell.symbol().trim().is_empty();
                let (ut, ub) = if half {
                    (
                        rgb(cell.fg).unwrap_or(fallback),
                        rgb(cell.bg).unwrap_or(fallback),
                    )
                } else if blank {
                    let u = rgb(cell.bg).unwrap_or(fallback);
                    (u, u)
                } else if blank_only {
                    continue;
                } else {
                    (fallback, fallback)
                };
                // Snap deviations from the underlying colour to coarse steps:
                // sub-visible drift then repaints as an identical cell, which
                // the terminal diff never has to resend.
                let over = |u: Rgb, p: [f32; 4]| {
                    let snap = |u: f32, c: f32| u + ((c - u) / STEP).round() * STEP;
                    [
                        snap(u[0], u[0] * (1.0 - p[3]) + p[0]),
                        snap(u[1], u[1] * (1.0 - p[3]) + p[1]),
                        snap(u[2], u[2] * (1.0 - p[3]) + p[2]),
                    ]
                };
                cell.set_style(Style::reset());
                cell.set_char('▀');
                cell.set_fg(color(over(ut, t)));
                cell.set_bg(color(over(ub, b)));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(w: u16, h: u16) -> Buffer {
        Buffer::empty(Rect::new(0, 0, w, h))
    }

    #[test]
    fn blit_composites_and_skips_transparent() {
        let mut cv = Canvas::new(2, 1);
        cv.over(0, 0, [255.0, 0.0, 0.0], 1.0);
        let mut b = buf(2, 1);
        cv.blit(&mut b, Rect::new(0, 0, 2, 1), [0.0, 0.0, 0.0], false);
        assert_eq!(b[(0, 0)].symbol(), "▀");
        assert_eq!(b[(0, 0)].fg, Color::Rgb(255, 0, 0));
        assert_eq!(b[(0, 0)].bg, Color::Rgb(0, 0, 0));
        assert_eq!(b[(1, 0)].symbol(), " ", "untouched cell stays blank");
    }

    #[test]
    fn blank_only_never_overwrites_text() {
        let mut cv = Canvas::new(1, 1);
        cv.over(0, 0, [9.0, 9.0, 9.0], 1.0);
        let mut b = buf(1, 1);
        b[(0, 0)].set_char('X');
        cv.blit(&mut b, Rect::new(0, 0, 1, 1), [0.0; 3], true);
        assert_eq!(b[(0, 0)].symbol(), "X");
        cv.blit(&mut b, Rect::new(0, 0, 1, 1), [0.0; 3], false);
        assert_eq!(b[(0, 0)].symbol(), "▀");
    }

    #[test]
    fn half_blocks_layer_over_earlier_pixels() {
        let mut back = Canvas::new(1, 1);
        back.over(0, 0, [0.0, 200.0, 0.0], 1.0);
        back.over(0, 1, [0.0, 0.0, 200.0], 1.0);
        let mut front = Canvas::new(1, 1);
        front.over(0, 1, [200.0, 0.0, 0.0], 0.5);
        let mut b = buf(1, 1);
        back.blit(&mut b, Rect::new(0, 0, 1, 1), [0.0; 3], false);
        front.blit(&mut b, Rect::new(0, 0, 1, 1), [0.0; 3], false);
        assert_eq!(b[(0, 0)].fg, Color::Rgb(0, 200, 0), "top half untouched");
        assert_eq!(b[(0, 0)].bg, Color::Rgb(100, 0, 100), "bottom half blended");
    }

    #[test]
    fn shapes_have_sane_distances() {
        assert!(ellipse_d(0.0, 0.0, 4.0, 2.0) < 0.0);
        assert!(ellipse_d(10.0, 0.0, 4.0, 2.0) > 0.0);
        assert!(ellipse_d(4.0, 0.0, 4.0, 2.0).abs() < 0.05);
        assert!(capsule_d(0.0, 0.0, -2.0, 0.0, 2.0, 0.0, 1.0) < 0.0);
        assert!(smin(1.0, 1.0, 2.0) < 1.0, "smooth union bulges");
    }

    #[test]
    fn clear_erases_only_the_requested_cells() {
        let mut cv = Canvas::new(3, 2);
        for x in 0..3 {
            for y in 0..4 {
                cv.over(x, y, [9.0; 3], 1.0);
            }
        }
        cv.clear(Rect::new(1, 0, 1, 1));
        let mut b = buf(3, 2);
        cv.blit(&mut b, Rect::new(0, 0, 3, 2), [0.0; 3], false);
        assert_eq!(b[(1, 0)].symbol(), " ", "cleared");
        assert_eq!(b[(0, 0)].symbol(), "▀");
        assert_eq!(b[(1, 1)].symbol(), "▀", "row below untouched");
    }

    #[test]
    fn noise_is_stable_and_bounded() {
        for n in 0..500 {
            let v = noise(n);
            assert!((0.0..1.0).contains(&v));
            assert_eq!(v, noise(n));
        }
    }
}
