// quake.rs — earthquake effect: real shake + seismic wave + debris.
//
// Four phases over normalized time t ∈ [0,1] (spec §2):
//   Ramp-up → Quake → Crumble-out → Settle
// (see docs/superpowers/specs/2026-08-27-earthquake-design.md).
//
// Render model mirrors ufo.rs/crt.rs: an overlay grid Vec<Option<Ov>> plus a
// burned mask, one write_all per frame. The key difference: rows of the user's
// REAL text are shifted with ICH (ESC[N@) / DCH (ESC[NP) — the shake is
// honest, not faked by drawn elements. ICH/DCH only ever hits intact (not yet
// broken) rows: those have no burned cells and an empty overlay, so there is
// nothing to conflict with (model purity invariant, spec §3.2).

use crate::engine::{terminal_size, Rng};
use crate::palettes::Palette;
use crate::{config::AnimSettings, ESC};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Earthquake phases (spec §2). Order matters — monotonic in t.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    RampUp,
    Quake,
    CrumbleOut,
    Settle,
}

/// Phase boundaries as half-open intervals [lo, hi) (spec §2). Settle includes t == 1.0.
const PHASE_RAMP_END: f32 = 0.12;
const PHASE_QUAKE_END: f32 = 0.70;
const PHASE_CRUMBLE_END: f32 = 0.88;
// PHASE_SETTLE_END = 1.0 (implicit).

/// Phase by normalized time (0..=1). Boundary convention: [lo, hi);
/// phase_at(0.12) → Quake, phase_at(0.70) → CrumbleOut, phase_at(0.88)
/// → Settle. At t == 1.0 → Settle (as in crt.rs).
pub fn phase_at(t01: f32) -> Phase {
    let t = t01.clamp(0.0, 1.0);
    if t < PHASE_RAMP_END {
        Phase::RampUp
    } else if t < PHASE_QUAKE_END {
        Phase::Quake
    } else if t < PHASE_CRUMBLE_END {
        Phase::CrumbleOut
    } else {
        Phase::Settle
    }
}

/// Peak shake amplitude: 1 + height capped at 3 (spec §3.2: height 0..3 →
/// 1..3 columns). Private — reachable through amp().
fn amp_max(height: i32) -> i32 {
    (1 + height.clamp(0, 3)).min(3)
}

/// Shake amplitude by normalized time (spec §3.2): ramps linearly 0 → AMP_MAX
/// over Ramp-up, holds AMP_MAX through Quake, drops to 0 at the Quake /
/// Crumble-out boundary. There is no fade-out phase and cannot be one: R_max
/// strictly exceeds max|ecy − y| (any corner has dx ≥ cols/4 > 0, §3.3), so
/// the wave breaks the last intact row before t = 0.70 — by Crumble-out there
/// is nothing left to shake. The 0 also keeps a post-resize Crumble-out frame
/// from re-shaking reset rows: target 0 compensates them back to 0.
pub fn amp(t01: f32, height: i32) -> i32 {
    let t = t01.clamp(0.0, 1.0);
    let max = amp_max(height) as f32;
    if t < PHASE_RAMP_END {
        (t / PHASE_RAMP_END * max).round() as i32
    } else if t < PHASE_QUAKE_END {
        max as i32
    } else {
        0
    }
}

/// The row-shift command from the accumulated offset `off` toward `target`
/// (spec §3.2/§4): off < target → ICH (row content moves right), off > target
/// → DCH (moves left), equal → None. Also used to compensate a row to 0 at
/// break time: shake_cmd(row_off, 0).
pub fn shake_cmd(off: i32, target: i32) -> Option<(bool, u32)> {
    if off < target {
        Some((true, (target - off) as u32))
    } else if off > target {
        Some((false, (off - target) as u32))
    } else {
        None
    }
}

/// Seismic wave front radius (spec §3.3): `p_quake` is the Quake phase
/// progress ∈ [0,1]; quadratic ease-out (fast start, slowing toward R_max), so
/// the ring visibly decelerates. Clamps p outside [0,1].
pub fn wave_radius(p_quake: f32, r_max: f32) -> f32 {
    let p = p_quake.clamp(0.0, 1.0);
    r_max * (1.0 - (1.0 - p) * (1.0 - p))
}

/// Elliptical distance with a 2:1 aspect (spec §3.3) — a circle on ~2:1
/// terminal cells (same idea as rx = 2·ry craters in ufo.rs). The minimum
/// over a row is on the epicenter's vertical, where dx = 0.
pub fn epi_dist(x: i32, y: i32, ecx: i32, ecy: i32) -> f32 {
    let dx = (x - ecx) as f32 / 2.0;
    let dy = (y - ecy) as f32;
    dx.hypot(dy)
}

/// Has the wave reached the row at vertical offset `dy` from the epicenter
/// (spec §3.3)? The boundary is inclusive: |dy| ≤ r.
pub fn row_reached(dy: i32, r: f32) -> bool {
    (dy.abs() as f32) <= r
}

// ── Debris glyphs + colors ─────────────────────────────────────────────

/// Weighted debris glyph table (spec §3.4): '·'×4, ','×2, '.'×2, '▘'×2,
/// '▝'×2, '▖'×2, '▗'×2, '░'×2, '▒'×1. Sum of weights = 19.
const DEBRIS_TABLE: &[char] = &[
    '·', '·', '·', '·',
    ',', ',',
    '.', '.',
    '▘', '▘',
    '▝', '▝',
    '▖', '▖',
    '▗', '▗',
    '░', '░',
    '▒',
];
const DEBRIS_TABLE_LEN: usize = DEBRIS_TABLE.len();

/// Debris glyph by index into the weighted table (spec §4). Safe for any idx:
/// taken modulo the length, so the inclusive Rng::range can never go out of
/// bounds (same safety as glyph_at in crt.rs, spec §3.4).
pub fn debris_glyph(idx: usize) -> char {
    DEBRIS_TABLE[idx % DEBRIS_TABLE_LEN]
}

/// Debris color — a random step of the bright palette half (spec §3.5):
/// idx is clamped into 18..=36. Direct index, no soften() — the palette is
/// already softened in config::build_palette.
pub fn debris_color(palette: &Palette, idx: i32) -> (u8, u8, u8) {
    palette[idx.clamp(18, 36) as usize]
}

// ── Particles ──────────────────────────────────────────────────────────

/// Tuning constants (spec §3.7) — starting values, tuned by eye.
const G: f32 = 0.20; // gravity, cells/frame²
const K_WIND: f32 = 0.02; // wind acceleration factor

/// One debris crumb (spec §3.4). Position in cells; y grows downward.
pub struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    ch: char,
    color: (u8, u8, u8),
    age: u32,
}

/// One frame of particle physics (spec §3.4): gravity, wind drift,
/// integration — physics only; death is decided by particle_dies in run().
pub fn step_particle(p: &mut Particle, wind: i32) {
    p.vy += G;
    p.vx += wind as f32 * K_WIND;
    p.x += p.vx;
    p.y += p.vy;
}

/// Particle death by its new row (spec §3.4, causes 1–2): flew past the
/// bottom of the screen, or entered a still-intact row (a crumb dies on
/// contact with surviving text). age > MAX_AGE is a trivial comparison
/// checked inline in run(). A negative row (tossed above the top edge) keeps
/// flying.
pub fn particle_dies(y_new: i32, rows: i32, row_broken: &[bool]) -> bool {
    if y_new >= rows {
        return true;
    }
    y_new >= 0 && !row_broken[y_new as usize]
}

// ── Overlay cell + grid primitives (as in ufo.rs) ──────────────────────

#[derive(Clone, Copy)]
struct Ov {
    ch: char,
    color: Option<(u8, u8, u8)>, // None ⇒ default fg/bg (the erase space)
}

/// Place an overlay cell into the grid at the given coordinates, bounds-checked.
fn stamp(grid: &mut [Option<Ov>], cols: i32, rows: i32, x: i32, y: i32, ov: Ov) {
    if (0..cols).contains(&x) && (0..rows).contains(&y) {
        grid[(y as usize) * (cols as usize) + (x as usize)] = Some(ov);
    }
}

/// Mark a cell as touched by the effect, bounds-checked (as burn() in ufo.rs).
fn burn(burned: &mut [bool], cols: i32, rows: i32, x: i32, y: i32) {
    if (0..cols).contains(&x) && (0..rows).contains(&y) {
        burned[(y as usize) * (cols as usize) + (x as usize)] = true;
    }
}

/// Render the overlay grid into a String (as in ufo.rs, BUT the buffer is NOT
/// cleared here: a quake frame starts with the ICH/DCH shake prefix that
/// run() writes into the same buf before calling render — one String, one
/// write_all per frame, spec §3.2). None cells are skipped so the original
/// terminal text shows through until the effect reaches it.
fn render(buf: &mut String, grid: &[Option<Ov>], cols: usize, rows: usize) {
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
                last_color = None; // color must be re-emitted after a cursor move
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

// ── Main-loop tuning constants (spec §3.7) ─────────────────────────────

const MAX_AGE: u32 = 90; // hang-proof particle lifetime, frames
const MAX_PARTICLES: usize = 600; // live particle cap
const SHAKE_HOLD: u32 = 3; // frames between shake retargets (~20 Hz at 60 fps)
const P_DETACH: f32 = 0.15; // per-cell detach probability per frame

/// Probability of a second crumb from a detached cell (spec §3.4):
/// 0.15 + 0.10·height (height 0..3 → 0.15..0.45 — "fewer than two" holds at
/// every height). P_DETACH does not depend on height.
fn p_second(height: i32) -> f32 {
    0.15 + 0.10 * height.clamp(0, 3) as f32
}

/// Random float in [0, 1) on top of the integer PRNG (there is no rand crate).
fn rand_f01(rng: &mut Rng) -> f32 {
    (rng.next_u64() % 1_000_000) as f32 / 1_000_000.0
}

/// Random float in [lo, hi).
fn rand_frng(rng: &mut Rng, lo: f32, hi: f32) -> f32 {
    lo + rand_f01(rng) * (hi - lo)
}

/// Random epicenter in the middle half of the screen (spec §3.3):
/// ecx ∈ [cols/4, 3·cols/4), ecy ∈ [rows/4, 3·rows/4) — a central band of
/// 50% so the wave is guaranteed to reach the corners by t = 0.70.
/// Rng::range is inclusive → the upper bound is 3·cols/4 − 1 to keep the
/// half-open range of spec §3.3.
fn pick_epicenter(cols: i32, rows: i32, rng: &mut Rng) -> (i32, i32) {
    (
        rng.range(cols / 4, 3 * cols / 4 - 1),
        rng.range(rows / 4, 3 * rows / 4 - 1),
    )
}

/// Per-row shake counter phases, randomized so the rows jitter out of sync
/// (spec §3.2: the counter phase differs per row, the frequency does not).
fn fresh_hold_phases(rows: usize, rng: &mut Rng) -> Vec<u32> {
    (0..rows)
        .map(|_| rng.range(0, SHAKE_HOLD as i32 - 1) as u32)
        .collect()
}

/// One crumb out of a cell (spec §3.4): position ± a small jitter,
/// vx ∈ [−0.4, 0.4), vy ∈ [−0.3, 0.1) (a light toss upward), a glyph from
/// the weighted table, a color from the bright half of the palette.
fn new_particle(rng: &mut Rng, palette: &Palette, x: i32, y: i32) -> Particle {
    Particle {
        x: x as f32 + rand_frng(rng, -0.3, 0.3),
        y: y as f32 + rand_frng(rng, -0.3, 0.3),
        vx: rand_frng(rng, -0.4, 0.4),
        vy: rand_frng(rng, -0.3, 0.1),
        ch: debris_glyph(rng.range(0, DEBRIS_TABLE_LEN as i32) as usize),
        color: debris_color(palette, rng.range(18, 36)),
        age: 0,
    }
}

/// Debris spawn from a detached cell (spec §3.4): one crumb, plus a second
/// with probability P_SECOND. Over the cap the spawn is simply skipped —
/// the crumble itself is never blocked.
fn spawn_debris(
    particles: &mut Vec<Particle>,
    rng: &mut Rng,
    palette: &Palette,
    x: i32,
    y: i32,
    height: i32,
) {
    if particles.len() >= MAX_PARTICLES {
        return;
    }
    particles.push(new_particle(rng, palette, x, y));
    if particles.len() < MAX_PARTICLES && rand_f01(rng) < p_second(height) {
        particles.push(new_particle(rng, palette, x, y));
    }
}

// ── Main loop ──────────────────────────────────────────────────────────

pub fn run(palette: &Palette, settings: &AnimSettings, interrupted: Arc<AtomicBool>) {
    let (mut cols, mut rows) = terminal_size();
    // Too small to bother — main() does the final clear anyway (spec §7).
    if cols < 8 || rows < 4 {
        return;
    }
    let fps = settings.fps.max(1) as u64;
    let frame_delay = Duration::from_millis(1000 / fps);
    // Clamp the duration from below to 0.6 s (spec §2) — otherwise the four
    // phases collapse into under a frame each.
    let t = Duration::from_secs_f32(settings.duration.max(0.6));

    let mut rng = Rng::new();
    let (mut ecx, mut ecy) = pick_epicenter(cols as i32, rows as i32, &mut rng);
    let mut burned = vec![false; cols * rows];
    let mut grid: Vec<Option<Ov>> = vec![None; cols * rows];
    let mut row_off = vec![0i32; rows]; // accumulated row shift, in columns
    let mut row_target = vec![0i32; rows];
    let mut row_hold = fresh_hold_phases(rows, &mut rng);
    let mut row_broken = vec![false; rows];
    let mut particles: Vec<Particle> = Vec::new();
    let mut buf = String::with_capacity(cols * rows * 6);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(out, "{ESC}[?25l"); // hide cursor
    let _ = out.flush();

    let start = Instant::now();
    loop {
        if interrupted.load(Ordering::Relaxed) {
            break;
        }
        let elapsed = start.elapsed();
        if elapsed > t {
            break;
        }

        // Live resize (spec §7): the state is recreated, `t` is NOT reset —
        // the animation plays out to the new geometry.
        let (nc, nr) = terminal_size();
        if nc != cols || nr != rows {
            cols = nc;
            rows = nr;
            if cols < 8 || rows < 4 {
                break;
            }
            burned = vec![false; cols * rows];
            grid = vec![None; cols * rows];
            row_off = vec![0; rows];
            row_target = vec![0; rows];
            row_hold = fresh_hold_phases(rows, &mut rng);
            row_broken = vec![false; rows];
            particles.clear();
            (ecx, ecy) = pick_epicenter(cols as i32, rows as i32, &mut rng);
            buf.reserve(cols * rows * 6);
        }
        let (ci, ri) = (cols as i32, rows as i32);

        // Reset the overlay for this frame.
        for cell in grid.iter_mut() {
            *cell = None;
        }

        // Normalized frame time, phase, wave front.
        let t01 = (elapsed.as_secs_f32() / t.as_secs_f32()).clamp(0.0, 1.0);
        let phase = phase_at(t01);
        let p_quake =
            ((t01 - PHASE_RAMP_END) / (PHASE_QUAKE_END - PHASE_RAMP_END)).clamp(0.0, 1.0);
        // R_max — the max elliptical distance from the epicenter to a screen
        // corner (spec §3.3). Strictly exceeds max|ecy − y|: any corner has
        // dx >= cols/4 > 0, so the wave breaks the last intact row before the
        // end of Quake.
        let r_max = epi_dist(0, 0, ecx, ecy)
            .max(epi_dist(ci - 1, 0, ecx, ecy))
            .max(epi_dist(0, ri - 1, ecx, ecy))
            .max(epi_dist(ci - 1, ri - 1, ecx, ecy));
        // The wave exists only from Quake onward (spec §2: during Ramp-up the
        // screen stays intact). With r = 0 the |dy| <= r check would break the
        // epicenter row on the very first frame; r = −1 keeps every row
        // intact and every cell out of the detach radius (spec §3.3: the
        // front only starts growing at the beginning of Quake).
        let r = if matches!(phase, Phase::RampUp) {
            -1.0
        } else {
            wave_radius(p_quake, r_max)
        };
        let a = amp(t01, settings.height);
        let settle = t01 >= PHASE_CRUMBLE_END;

        buf.clear();

        // (1) Row breaking (spec §3.3): first contact is on the epicenter's
        // vertical, where dx = 0 — hence |ecy − y| <= r. The accumulated row
        // shift is compensated to 0 with a single ICH/DCH (spec §3.2); from
        // here on the row lives in the overlay model's clean coordinates.
        for y in 0..ri {
            let yi = y as usize;
            if row_broken[yi] || !row_reached(ecy - y, r) {
                continue;
            }
            if let Some((is_ich, n)) = shake_cmd(row_off[yi], 0) {
                if is_ich {
                    let _ = write!(buf, "{ESC}[{};1H{ESC}[{n}@", y + 1);
                } else {
                    let _ = write!(buf, "{ESC}[{};1H{ESC}[{n}P", y + 1);
                }
            }
            row_off[yi] = 0;
            row_broken[yi] = true;
        }

        // (2) Shaking the intact rows (spec §3.2): the target changes once
        // per SHAKE_HOLD frames (≈20 Hz at 60 fps — a rattle, not white
        // noise). ICH/DCH only ever hits intact rows — there the overlay is
        // empty and no cell is burned, so the bare user text shakes.
        for y in 0..ri {
            let yi = y as usize;
            if row_broken[yi] {
                continue; // a destroyed row cannot shake as a whole
            }
            row_hold[yi] += 1;
            if row_hold[yi] >= SHAKE_HOLD {
                row_hold[yi] = 0;
                // The range itself grows with amp(t) over Ramp-up; after
                // Quake a = 0 → target 0 (compensation, matters after a
                // mid-Crumble-out resize resets the rows).
                row_target[yi] = rng.range(-a, a);
            }
            if let Some((is_ich, n)) = shake_cmd(row_off[yi], row_target[yi]) {
                if is_ich {
                    let _ = write!(buf, "{ESC}[{};1H{ESC}[{n}@", y + 1);
                } else {
                    let _ = write!(buf, "{ESC}[{};1H{ESC}[{n}P", y + 1);
                }
            }
            row_off[yi] = row_target[yi];
        }

        // (3) Crumble. From Settle on, every cell of the screen is
        // force-marked burned with no regard for row_broken (a late resize
        // in Settle resets row_broken — the cleanup must survive it) and
        // with no spawn (spec §3.3). Before that, cells of broken rows
        // inside the front detach with probability P_DETACH per frame.
        if settle {
            for cell in burned.iter_mut() {
                *cell = true;
            }
        } else {
            // A cell with |x − ecx| > 2r is beyond the front on ANY row (the
            // dx/2 term alone exceeds r), so the scan only needs the columns
            // the front can physically reach. The +2 margin covers the
            // f32→i32 truncation and rounding at the boundary; epi_dist below
            // stays the exact gate, so behavior is unchanged.
            let reach = (2.0 * r) as i32 + 2;
            let x_lo = (ecx - reach).max(0);
            let x_hi = (ecx + reach + 1).min(ci);
            for y in 0..ri {
                if !row_broken[y as usize] {
                    continue;
                }
                for x in x_lo..x_hi {
                    let cell = y as usize * cols + x as usize;
                    if burned[cell] || epi_dist(x, y, ecx, ecy) > r {
                        continue;
                    }
                    if rand_f01(&mut rng) < P_DETACH {
                        burned[cell] = true;
                        spawn_debris(
                            &mut particles,
                            &mut rng,
                            palette,
                            x,
                            y,
                            settings.height,
                        );
                    }
                }
            }
        }

        // (4) Particles: step, then death by the frame's final position
        // (checked before drawing — only live crumbs are drawn, spec §3.4).
        for p in particles.iter_mut() {
            step_particle(p, settings.wind);
            p.age += 1;
        }
        particles
            .retain(|p| !(p.age > MAX_AGE || particle_dies(p.y.floor() as i32, ri, &row_broken)));

        // (5) Frame assembly (spec §3.2): the shake prefix is already in
        // buf; now the erase layer over burned cells, crumbs on top, then
        // ONE write_all.
        for y in 0..ri {
            for x in 0..ci {
                if burned[y as usize * cols + x as usize] {
                    stamp(&mut grid, ci, ri, x, y, Ov { ch: ' ', color: None });
                }
            }
        }
        for p in particles.iter() {
            let px = p.x.round() as i32;
            let py = p.y.floor() as i32;
            // A crumb burns every cell it is drawn in (spec §3.4): the last
            // frame's position is erased by the next frame's erase layer —
            // no trails.
            burn(&mut burned, ci, ri, px, py);
            stamp(&mut grid, ci, ri, px, py, Ov { ch: p.ch, color: Some(p.color) });
        }

        render(&mut buf, &grid, cols, rows);
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
        // Settle with no live particles: every cell is burned, so every
        // further frame would be a pixel-identical full-screen rewrite —
        // end early (the screen is already blank, spec §2 "quiet and
        // empty"; the final clear is done by main()).
        if settle && particles.is_empty() {
            break;
        }
        std::thread::sleep(frame_delay);
    }

    // Always restore the cursor — main() does the final clear (it also covers
    // any accumulated row shifts left over after an interrupt, spec §7).
    let _ = write!(out, "{ESC}[?25h");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_boundaries() {
        // [lo, hi) convention: the boundary belongs to the next phase.
        assert_eq!(phase_at(0.0), Phase::RampUp);
        assert_eq!(phase_at(0.11), Phase::RampUp);
        assert_eq!(phase_at(0.12), Phase::Quake);
        assert_eq!(phase_at(0.699), Phase::Quake);
        assert_eq!(phase_at(0.70), Phase::CrumbleOut);
        assert_eq!(phase_at(0.879), Phase::CrumbleOut);
        assert_eq!(phase_at(0.88), Phase::Settle);
        assert_eq!(phase_at(0.999), Phase::Settle);
        // Settle includes t == 1.0 (special case, as in crt.rs).
        assert_eq!(phase_at(1.0), Phase::Settle);
    }

    #[test]
    fn phase_clamps_out_of_range() {
        // Values outside [0,1] must not panic.
        assert_eq!(phase_at(-0.5), Phase::RampUp);
        assert_eq!(phase_at(1.5), Phase::Settle);
    }

    #[test]
    fn phase_monotonic() {
        // The phase sequence never jumps backward as t grows.
        fn order(p: Phase) -> u8 {
            match p {
                Phase::RampUp => 0,
                Phase::Quake => 1,
                Phase::CrumbleOut => 2,
                Phase::Settle => 3,
            }
        }
        let mut prev = 0u8;
        let n = 400;
        for i in 0..=n {
            let t = i as f32 / n as f32;
            let o = order(phase_at(t));
            assert!(o >= prev, "phase regression at t={t}: {o} < {prev}");
            prev = o;
        }
    }

    #[test]
    fn amp_shape() {
        // Zero at the start; the peak sits inside Quake; a hard cut to 0 at
        // the Quake/Crumble-out boundary — there is no fade-out phase (§3.2).
        assert_eq!(amp(0.0, 2), 0);
        assert_eq!(amp(0.12, 2), 3);
        assert_eq!(amp(0.5, 2), 3);
        assert_eq!(amp(0.699, 2), 3);
        assert_eq!(amp(0.70, 2), 0);
        assert_eq!(amp(0.88, 2), 0);
        assert_eq!(amp(1.0, 2), 0);
        // Monotone over the Ramp-up segment.
        let mut prev = amp(0.0, 2);
        for i in 1..=100 {
            let t = PHASE_RAMP_END * i as f32 / 100.0;
            let a = amp(t, 2);
            assert!(a >= prev, "amp decreased at t={t}: {a} < {prev}");
            prev = a;
        }
    }

    #[test]
    fn amp_height_scales() {
        // height 0..3 → peak amplitude 1..3 columns (§3.2: AMP_MAX = 1 + height, cap 3).
        assert_eq!(amp(0.5, 0), 1);
        assert_eq!(amp(0.5, 1), 2);
        assert_eq!(amp(0.5, 2), 3);
        assert_eq!(amp(0.5, 3), 3); // capped
        // Out-of-range heights clamp instead of panicking.
        assert_eq!(amp(0.5, -1), 1);
        assert_eq!(amp(0.5, 9), 3);
    }

    #[test]
    fn shake_cmd_signs() {
        // off < target → ICH (content moves right); off > target → DCH (left);
        // equal → no command needed.
        assert_eq!(shake_cmd(0, 2), Some((true, 2)));
        assert_eq!(shake_cmd(2, 0), Some((false, 2)));
        assert_eq!(shake_cmd(-1, 1), Some((true, 2)));
        assert_eq!(shake_cmd(1, -1), Some((false, 2)));
        assert_eq!(shake_cmd(0, 0), None);
        assert_eq!(shake_cmd(3, 3), None);
    }

    #[test]
    fn wave_radius_bounds() {
        let r_max = 12.5;
        assert_eq!(wave_radius(0.0, r_max), 0.0);
        assert_eq!(wave_radius(1.0, r_max), r_max);
        // Monotone growth over the phase.
        let mut prev = wave_radius(0.0, r_max);
        for i in 1..=100 {
            let p = i as f32 / 100.0;
            let r = wave_radius(p, r_max);
            assert!(r >= prev, "radius regressed at p={p}: {r} < {prev}");
            prev = r;
        }
        // Outside [0,1] clamps.
        assert_eq!(wave_radius(-0.5, r_max), 0.0);
        assert_eq!(wave_radius(1.5, r_max), r_max);
    }

    #[test]
    fn epi_dist_zero_at_epicenter() {
        assert_eq!(epi_dist(10, 5, 10, 5), 0.0);
    }

    #[test]
    fn epi_dist_aspect() {
        // The minimum over a row sits on the epicenter's vertical (dx = 0).
        let (ecx, ecy) = (40, 12);
        let on_vertical = epi_dist(ecx, 4, ecx, ecy);
        for dx in [-6, -2, 2, 6] {
            assert!(
                epi_dist(ecx + dx, 4, ecx, ecy) > on_vertical,
                "dx={dx} must be farther than dx=0"
            );
        }
        // 2:1 aspect: a shift of 2 columns equals a shift of 1 row.
        assert_eq!(
            epi_dist(ecx + 2, ecy, ecx, ecy),
            epi_dist(ecx, ecy + 1, ecx, ecy)
        );
        assert_eq!(
            epi_dist(ecx + 4, ecy, ecx, ecy),
            epi_dist(ecx, ecy + 2, ecx, ecy)
        );
    }

    #[test]
    fn row_reached_edges() {
        // The boundary is included: |dy| == r → true; |dy| == r + ε → false.
        assert!(row_reached(3, 3.0));
        assert!(row_reached(-3, 3.0));
        assert!(!row_reached(4, 3.0));
        assert!(!row_reached(-4, 3.0));
        assert!(!row_reached(4, 3.9999));
        // A zero-radius wave reaches only the epicenter row itself.
        assert!(row_reached(0, 0.0));
        assert!(!row_reached(1, 0.0));
    }

    #[test]
    fn debris_table_len_is_nineteen() {
        // Sum of weights: 4+2+2+2+2+2+2+2+1 = 19 (§3.4).
        assert_eq!(DEBRIS_TABLE_LEN, 19);
    }

    #[test]
    fn debris_glyph_valid() {
        let allowed = ['·', ',', '.', '▘', '▝', '▖', '▗', '░', '▒'];
        // Every index in [0, LEN) maps into the allowed set.
        for i in 0..DEBRIS_TABLE_LEN {
            let ch = debris_glyph(i);
            assert!(allowed.contains(&ch), "unexpected glyph {ch:?} at index {i}");
        }
        // Every glyph of the allowed set is present (weight ≥ 1); together
        // with the loop above this also proves the table is non-empty.
        for &ch in &allowed {
            assert!(
                (0..DEBRIS_TABLE_LEN).any(|i| debris_glyph(i) == ch),
                "glyph {ch:?} missing from table"
            );
        }
        // Out-of-range indices are safe — they wrap modulo the length.
        for i in [DEBRIS_TABLE_LEN, DEBRIS_TABLE_LEN + 1, 1_000_000] {
            assert!(allowed.contains(&debris_glyph(i)));
        }
    }

    #[test]
    fn debris_color_bounds() {
        let mut pal = [(0u8, 0u8, 0u8); 37];
        for (i, slot) in pal.iter_mut().enumerate() {
            *slot = (i as u8, i as u8, i as u8); // the index is visible in the color
        }
        // idx below/above the bright half clamps into 18..=36.
        for idx in [-5, 0, 17, 37, 40, 99] {
            let c = debris_color(&pal, idx);
            assert!(
                (18..=36).contains(&(c.0 as i32)),
                "idx={idx} escaped the bright half: color {c:?}"
            );
        }
        // In-range indices pick the palette step verbatim.
        assert_eq!(debris_color(&pal, 18), pal[18]);
        assert_eq!(debris_color(&pal, 36), pal[36]);
        assert_eq!(debris_color(&pal, 17), pal[18]);
        assert_eq!(debris_color(&pal, 37), pal[36]);
    }

    #[test]
    fn particle_falls() {
        let mut p = Particle {
            x: 10.0,
            y: 5.0,
            vx: 0.0,
            vy: 0.05,
            ch: '·',
            color: (1, 2, 3),
            age: 0,
        };
        let vy0 = p.vy;
        let y0 = p.y;
        step_particle(&mut p, 0);
        assert!(p.vy > vy0, "gravity must grow vy");
        assert!(p.y > y0, "particle must fall");
        // A second step keeps falling.
        let y1 = p.y;
        step_particle(&mut p, 0);
        assert!(p.y > y1);
    }

    #[test]
    fn particle_wind_drift() {
        // wind = +2 → vx grows (pushed right every frame).
        let mut p = Particle {
            x: 0.0,
            y: 0.0,
            vx: 0.0,
            vy: 0.0,
            ch: ',',
            color: (0, 0, 0),
            age: 0,
        };
        step_particle(&mut p, 2);
        assert!(p.vx > 0.0, "vx must grow under wind=+2");
        // wind = 0 → vx does not change.
        let mut q = Particle {
            x: 0.0,
            y: 0.0,
            vx: 0.25,
            vy: 0.0,
            ch: ',',
            color: (0, 0, 0),
            age: 0,
        };
        step_particle(&mut q, 0);
        assert_eq!(q.vx, 0.25, "vx must not change under wind=0");
    }

    #[test]
    fn particle_dies_offscreen() {
        let rb = [false, false, true];
        // Flew past the bottom (rows = 3): y >= rows → true.
        assert!(particle_dies(3, 3, &rb));
        assert!(particle_dies(10, 3, &rb));
        // Still on screen in a broken row → alive.
        assert!(!particle_dies(2, 3, &rb));
    }

    #[test]
    fn particle_dies_on_intact_row() {
        let rb = [false, false, true];
        // An intact row kills the crumb on contact with surviving text.
        assert!(particle_dies(0, 3, &rb));
        assert!(particle_dies(1, 3, &rb));
        // A broken row does not.
        assert!(!particle_dies(2, 3, &rb));
    }

    #[test]
    fn particle_dies_negative_y_survives() {
        // Tossed above the top edge: no row to hit — keeps flying (and must
        // not panic on a negative index).
        let rb = [false, false, false];
        assert!(!particle_dies(-1, 3, &rb));
        assert!(!particle_dies(-5, 3, &rb));
    }
}
