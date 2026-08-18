// ufo.rs — flying-saucer "disintegration" clear effect.
//
// Saucers enter from the right and sweep left, firing lasers forward and
// along the two diagonals. Every cell touched by a saucer body, a laser, or
// a blast crater is "burned" (erased); untouched terminal text shows through
// until it is destroyed. Because every drawn element is also marked burned,
// nothing leaves a residue — the moment a saucer/laser/flash moves on, the
// cell is blank.
//
// Geometry and rendering reuse the same primitives as the fire effect
// (terminal_size, xorshift PRNG, ANSI cursor-home redraw) but the simulation
// is its own: sprite layers + transient shots rather than a heat grid.

use crate::engine::{terminal_size, Rng};
use crate::{config::AnimSettings, ESC};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

// ── Sprite ────────────────────────────────────────────────────────────
// Classic saucer: glass dome on a metal dish with amber portholes. ~4 rows.
const SAUCER: &[&str] = &[
    "      ╭◉◉◉╮",
    "    ╱▔▔▔▔▔▔▔╲",
    "   ╱ ◯  ◯  ◯ ╲",
    "   ╲▁▁▁▁▁▁▁▁▁╱",
];
const SAUCER_H: usize = 4;

/// Per-character color of the sprite. Spaces return None (transparent).
fn char_color(ch: char) -> Option<(u8, u8, u8)> {
    match ch {
        '╭' | '╮' | '◉' => Some((0x7f, 0xd4, 0xff)), // dome glass
        '╱' | '╲' | '▔' | '▁' => Some((0xc8, 0xd0, 0xd8)), // metal body
        '◯' => Some((0xff, 0xcc, 0x44)),             // amber portholes
        _ => None,
    }
}

// ── Colors / tuning ───────────────────────────────────────────────────
const C_LASER: (u8, u8, u8) = (0x39, 0xff, 0x14); // neon green beam
/// Blast color ramp: white-hot core fading through yellow/orange to a dim
/// ember. Indexed by shot age so the impact visibly cools as it expands.
const C_BLAST: [(u8, u8, u8); 6] = [
    (0xff, 0xff, 0xff), // 0 — white-hot core
    (0xff, 0xf2, 0x99), // 1 — pale yellow
    (0xff, 0xd6, 0x44), // 2 — yellow
    (0xff, 0x9b, 0x22), // 3 — orange
    (0xff, 0x55, 0x22), // 4 — red-orange
    (0xb0, 0x2a, 0x20), // 5 — dark ember
];
/// Crater half-height in rows (diameter ≈ 5). Width radius is doubled to
/// compensate the terminal cell aspect ratio (~2:1 tall) so the blast reads
/// as a circle rather than a narrow vertical streak.
const CRATER_RY: i32 = 2;
const CRATER_RX: i32 = CRATER_RY * 2;
/// Frames a shot stays alive: the beam fires for two frames, then the
/// expanding shockwave + fading ember play out so the impact is obvious
/// rather than a one-cell flicker.
const SHOT_LIFE: u32 = 6;

// ── Pure geometry (unit-tested) ───────────────────────────────────────

/// Cells of an elliptical crater around `(cx, cy)`, clipped to the screen.
/// `rx`/`ry` differ so the erased area looks circular on a non-square cell.
pub fn crater_cells(cx: i32, cy: i32, cols: i32, rows: i32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let rx2 = (CRATER_RX * CRATER_RX) as f32;
    let ry2 = (CRATER_RY * CRATER_RY) as f32;
    for dy in -CRATER_RY..=CRATER_RY {
        let y = cy + dy;
        if !(0..rows).contains(&y) {
            continue;
        }
        for dx in -CRATER_RX..=CRATER_RX {
            let x = cx + dx;
            if !(0..cols).contains(&x) {
                continue;
            }
            let nx = (dx * dx) as f32 / rx2;
            let ny = (dy * dy) as f32 / ry2;
            if nx + ny <= 1.0 {
                out.push((x as usize, y as usize));
            }
        }
    }
    out
}

/// Cells forming the perimeter band of an aspect-compensated ellipse of
/// radius `r` around `(cx, cy)` — the expanding blast shockwave. The width
/// radius is doubled (cells are ~2:1 tall) so the ring reads circular; `r`
/// grows each frame so the ring appears to radiate outward from the impact.
pub fn ring_cells(cx: i32, cy: i32, r: i32, cols: i32, rows: i32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let r = r.max(1);
    let rx = (r * 2) as f32;
    let ry = r as f32;
    let rxsq = rx * rx;
    let rysq = ry * ry;
    for dy in -r..=r {
        let y = cy + dy;
        if !(0..rows).contains(&y) {
            continue;
        }
        for dx in -(r * 2)..=(r * 2) {
            let x = cx + dx;
            if !(0..cols).contains(&x) {
                continue;
            }
            // Normalized distance from center: 0 at (cx,cy), 1 on the ellipse.
            let d = ((dx * dx) as f32 / rxsq + (dy * dy) as f32 / rysq).sqrt();
            // Keep only an outer band → a ring of roughly one-cell thickness.
            if (0.6..=1.2).contains(&d) {
                out.push((x as usize, y as usize));
            }
        }
    }
    out
}

/// Integer Bresenham line between two points (inclusive of both ends).
pub fn line_cells(x0: i32, y0: i32, x1: i32, y1: i32) -> Vec<(i32, i32)> {
    let mut out = Vec::new();
    let (mut x, mut y) = (x0, y0);
    let dx = (x1 - x0).abs();
    let sx: i32 = if x0 < x1 { 1 } else { -1 };
    let dy = -(y1 - y0).abs();
    let sy: i32 = if y0 < y1 { 1 } else { -1 };
    let mut err = dx + dy;
    loop {
        out.push((x, y));
        if x == x1 && y == y1 {
            break;
        }
        let e2 = 2 * err;
        if e2 >= dy {
            err += dy;
            x += sx;
        }
        if e2 <= dx {
            err += dx;
            y += sy;
        }
    }
    out
}

// ── Overlay cell + renderer ───────────────────────────────────────────

#[derive(Clone, Copy)]
struct Ov {
    ch: char,
    color: Option<(u8, u8, u8)>, // None ⇒ default (used for the erase space)
}

fn stamp(grid: &mut [Option<Ov>], cols: i32, rows: i32, x: i32, y: i32, ov: Ov) {
    if (0..cols).contains(&x) && (0..rows).contains(&y) {
        grid[(y as usize) * (cols as usize) + (x as usize)] = Some(ov);
    }
}

fn burn(burned: &mut [bool], cols: i32, rows: i32, x: i32, y: i32) {
    if (0..cols).contains(&x) && (0..rows).contains(&y) {
        burned[(y as usize) * (cols as usize) + (x as usize)] = true;
    }
}

/// Render an overlay grid via ANSI cursor-home redraw. Cells that are `None`
/// are skipped so the original terminal content shows through untouched.
fn render(buf: &mut String, grid: &[Option<Ov>], cols: usize, rows: usize) {
    use std::fmt::Write as _;
    buf.clear();
    let mut last_color: Option<Option<(u8, u8, u8)>> = None;
    let mut need_move = true;
    let mut wcol = 0usize;
    let mut wrow = 0usize;

    for y in 0..rows {
        for x in 0..cols {
            let Some(ov) = grid[y * cols + x] else {
                need_move = true;
                continue;
            };
            if need_move || wrow != y || wcol != x {
                let _ = write!(buf, "{ESC}[{};{}H", y + 1, x + 1);
                last_color = None;
                need_move = false;
                wrow = y;
                wcol = x;
            }
            if last_color != Some(ov.color) {
                match ov.color {
                    Some((r, g, b)) => {
                        let _ = write!(buf, "{ESC}[38;2;{r};{g};{b}m{ESC}[49m");
                    }
                    None => {
                        let _ = write!(buf, "{ESC}[39m{ESC}[49m");
                    }
                }
                last_color = Some(ov.color);
            }
            buf.push(ov.ch);
            wcol += 1;
        }
    }
    let _ = write!(buf, "{ESC}[0m");
}

// ── Simulation state ──────────────────────────────────────────────────

struct Saucer {
    x: i32,
    y: i32,
    cooldown: u32, // frames until the next shot
}

struct Shot {
    line: Vec<(i32, i32)>, // beam cells (already burned)
    cx: i32,
    cy: i32,
    age: u32,
}

fn sprite_width() -> i32 {
    SAUCER
        .iter()
        .map(|r| r.chars().count())
        .max()
        .unwrap_or(10) as i32
}

/// Vertical positions for the squadron, spread evenly and clamped to fit.
fn squadron_rows(rows: i32) -> Vec<i32> {
    let third = rows / 3;
    let raw = [third / 2, rows / 2 - 2, rows - third / 2 - 2];
    let max_y = (rows - SAUCER_H as i32).max(0);
    raw.iter().map(|&y| y.clamp(0, max_y)).collect()
}

// ── Main loop ─────────────────────────────────────────────────────────

pub fn run(settings: &AnimSettings, interrupted: Arc<AtomicBool>) {
    let (mut cols, mut rows) = terminal_size();
    // Too small to bother — let main() clear the screen.
    if cols < 8 || rows < (SAUCER_H + 2) {
        return;
    }
    let (cols_i, rows_i) = (cols as i32, rows as i32);

    let fps = settings.fps.max(1) as u64;
    let frame_delay = Duration::from_millis(1000 / fps);
    let max_duration = Duration::from_secs_f32(settings.duration * 1.8 + 0.5);

    let sw = sprite_width();
    let distance = (cols_i + sw + 8) as f32;
    let speed = (distance / (settings.fps as f32 * settings.duration))
        .round()
        .max(1.0) as i32;

    let mut rng = Rng::new();
    let mut saucers: Vec<Saucer> = squadron_rows(rows_i)
        .iter()
        .enumerate()
        .map(|(i, &y)| Saucer {
            x: cols_i + (i as i32) * 6,
            y,
            cooldown: rng.range(0, 4) as u32,
        })
        .collect();
    let mut shots: Vec<Shot> = Vec::new();
    let mut burned = vec![false; cols * rows];
    let mut grid: Vec<Option<Ov>> = vec![None; cols * rows];
    let mut buf = String::with_capacity(cols * rows * 6);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(out, "{ESC}[?25l"); // hide cursor
    let _ = out.flush();

    let start = Instant::now();
    'outer: loop {
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        if start.elapsed() > max_duration {
            break;
        }

        // Live resize.
        let (nc, nr) = terminal_size();
        if nc != cols || nr != rows {
            cols = nc;
            rows = nr;
            if cols < 8 || rows < (SAUCER_H + 2) {
                break;
            }
            burned = vec![false; cols * rows];
            grid = vec![None; cols * rows];
            buf.reserve(cols * rows * 6);
        }
        let (ci, ri) = (cols as i32, rows as i32);

        // Reset overlay for this frame.
        for cell in grid.iter_mut() {
            *cell = None;
        }

        // Advance saucers and maybe fire.
        for s in saucers.iter_mut() {
            s.x -= speed;
            if s.cooldown > 0 {
                s.cooldown -= 1;
            }
            // Gun at the saucer's lower-left tip.
            let gx = s.x + 3;
            let gy = s.y + 2;
            if s.x > -sw && s.cooldown == 0 && gx > 0 {
                // Direction: left + one of the three forward diagonals/straight.
                let dy = rng.range(-1, 1); // -1, 0, +1
                let len = rng.range(4, (ci / 2).max(6));
                let tx = gx - len;
                let ty = (gy + dy * (len / 2)).clamp(0, ri - 1);
                let beam = line_cells(gx, gy, tx, ty);
                let mut line = Vec::with_capacity(beam.len());
                for &(bx, by) in &beam {
                    burn(&mut burned, ci, ri, bx, by);
                    line.push((bx, by));
                }
                for &(cx_, cy_) in crater_cells(tx, ty, ci, ri).iter() {
                    burn(&mut burned, ci, ri, cx_ as i32, cy_ as i32);
                }
                shots.push(Shot {
                    line,
                    cx: tx,
                    cy: ty,
                    age: 0,
                });
                s.cooldown = rng.range(2, 5) as u32;
            }
        }

        if saucers.iter().all(|s| s.x <= -sw) && shots.is_empty() {
            break 'outer;
        }

        // Layer 1: erase every burned cell (blank space over destroyed text).
        for y in 0..ri {
            for x in 0..ci {
                if burned[(y as usize) * cols + (x as usize)] {
                    stamp(&mut grid, ci, ri, x, y, Ov { ch: ' ', color: None });
                }
            }
        }
        // Layer 2: the impact itself — a brief beam, a hot core, and an
        // expanding shockwave that radiates outward and cools. This is what
        // makes a "hit" readable: the eye follows the green beam to a bright,
        // growing blast rather than a one-cell flicker.
        for sh in shots.iter_mut() {
            let age = sh.age as usize;
            let blast = C_BLAST[age.min(C_BLAST.len() - 1)];
            // Beam: only the first couple of frames — the shot in flight.
            if sh.age < 2 {
                for &(lx, ly) in &sh.line {
                    stamp(&mut grid, ci, ri, lx, ly, Ov { ch: '█', color: Some(C_LASER) });
                }
            }
            // Hot core sparkle at the impact, fading over the first frames.
            if sh.age < 3 {
                stamp(&mut grid, ci, ri, sh.cx, sh.cy, Ov { ch: '✦', color: Some(blast) });
            }
            // Expanding shockwave ring: grows each frame, dims as it goes.
            let ring_r = ((sh.age as i32) + 1).min(4);
            let ring_ch = if sh.age < 2 {
                '▓'
            } else if sh.age < 4 {
                '▒'
            } else {
                '░'
            };
            for &(bx, by) in &ring_cells(sh.cx, sh.cy, ring_r, ci, ri) {
                stamp(
                    &mut grid,
                    ci,
                    ri,
                    bx as i32,
                    by as i32,
                    Ov {
                        ch: ring_ch,
                        color: Some(blast),
                    },
                );
            }
            sh.age += 1;
        }
        shots.retain(|s| s.age < SHOT_LIFE);
        // Layer 3: saucer sprites on top.
        for s in saucers.iter() {
            for (row, line) in SAUCER.iter().enumerate() {
                for (col, ch) in line.chars().enumerate() {
                    if ch == ' ' {
                        continue;
                    }
                    let Some(color) = char_color(ch) else { continue };
                    stamp(
                        &mut grid,
                        ci,
                        ri,
                        s.x + col as i32,
                        s.y + row as i32,
                        Ov { ch, color: Some(color) },
                    );
                    burn(&mut burned, ci, ri, s.x + col as i32, s.y + row as i32);
                }
            }
        }

        render(&mut buf, &grid, cols, rows);
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();

        std::thread::sleep(frame_delay);
    }

    let _ = write!(out, "{ESC}[?25h"); // always restore cursor
    let _ = out.flush();
}

// ── Tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crater_is_roughly_circular_and_clipped() {
        // Center well inside a 30×12 screen.
        let cells = crater_cells(15, 6, 30, 12);
        // Contained on screen.
        for &(x, y) in &cells {
            assert!(x < 30 && y < 12);
        }
        // The center is always part of the crater.
        assert!(cells.contains(&(15, 6)));
        // Wider than tall (rx = 2·ry) → horizontal extent strictly larger.
        let xs = cells.iter().map(|&(x, _)| x as i32 - 15).collect::<Vec<_>>();
        let ys = cells.iter().map(|&(_, y)| y as i32 - 6).collect::<Vec<_>>();
        assert_eq!(xs.iter().copied().min().unwrap(), -CRATER_RX);
        assert_eq!(xs.iter().copied().max().unwrap(), CRATER_RX);
        assert_eq!(ys.iter().copied().min().unwrap(), -CRATER_RY);
        assert_eq!(ys.iter().copied().max().unwrap(), CRATER_RY);
    }

    #[test]
    fn crater_clips_at_edges() {
        // Corner center: nothing off-screen should appear.
        let cells = crater_cells(0, 0, 20, 10);
        for &(x, y) in &cells {
            assert!(x < 20 && y < 10);
        }
        assert!(cells.contains(&(0, 0)));
    }

    #[test]
    fn line_horizontal_is_exact() {
        let pts = line_cells(0, 3, 5, 3);
        assert_eq!(pts, vec![(0, 3), (1, 3), (2, 3), (3, 3), (4, 3), (5, 3)]);
    }

    #[test]
    fn line_diagonal_45() {
        let pts = line_cells(0, 0, 4, 4);
        assert_eq!(pts, vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 4)]);
    }

    #[test]
    fn line_single_point() {
        let pts = line_cells(2, 2, 2, 2);
        assert_eq!(pts, vec![(2, 2)]);
    }

    #[test]
    fn line_backward() {
        let pts = line_cells(5, 1, 0, 1);
        assert_eq!(pts.len(), 6);
        assert_eq!(pts[0], (5, 1));
        assert_eq!(pts.last().copied().unwrap(), (0, 1));
    }

    #[test]
    fn ring_grows_with_radius() {
        // A bigger radius yields strictly more perimeter cells.
        let small = ring_cells(40, 12, 1, 80, 24);
        let big = ring_cells(40, 12, 3, 80, 24);
        assert!(!small.is_empty());
        assert!(big.len() > small.len());
    }

    #[test]
    fn ring_is_aspect_wide() {
        // rx = 2·ry ⇒ the ring's horizontal span is at least 2× its vertical span.
        let cells = ring_cells(40, 12, 4, 80, 24);
        let (xmin, xmax) = cells
            .iter()
            .fold((i32::MAX, i32::MIN), |(lo, hi), &(x, _)| {
                (lo.min(x as i32), hi.max(x as i32))
            });
        let (ymin, ymax) = cells
            .iter()
            .fold((i32::MAX, i32::MIN), |(lo, hi), &(_, y)| {
                (lo.min(y as i32), hi.max(y as i32))
            });
        let width = xmax - xmin;
        let height = ymax - ymin;
        assert!(width >= 2 * height, "{width} vs {height}");
    }

    #[test]
    fn ring_excludes_center() {
        // The perimeter band leaves the center cell out.
        let cells = ring_cells(40, 12, 3, 80, 24);
        assert!(!cells.contains(&(40, 12)));
    }

    #[test]
    fn ring_clips_at_edge() {
        let cells = ring_cells(0, 0, 3, 80, 24);
        for &(x, y) in &cells {
            assert!(x < 80 && y < 24);
        }
        assert!(!cells.contains(&(0, 0)));
    }
}
