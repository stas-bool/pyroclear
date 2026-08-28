// blackhole.rs — black hole clear effect: honest text pull + accretion disk.
//
// Four phases over normalized time t ∈ [0,1] (spec §2):
//   Emergence → Attraction → Collapse → Flash
// (see docs/superpowers/specs/2026-08-28-blackhole-design.md).
//
// Render model mirrors ufo.rs/crt.rs/quake.rs: an overlay grid
// Vec<Option<Ov>> plus a burned mask, one write_all per frame. Two
// extensions of that base (spec §3.2): (1) Ov/render here carry a
// background color — the black hole core and the flash are the only
// opaque-bg cells (the samples' render always resets ESC[49m and could
// draw neither); (2) the user's REAL text is pulled toward the center
// vertical by an honest ICH (ESC[N@) + DCH (ESC[NP) pair per Devouring
// row per frame. Rows walk a state machine Untouched → Devouring →
// Devoured; ICH/DCH only ever hits Devouring rows, whose overlay is
// entirely empty, so the current frame's commands never touch cells the
// overlay paints (model purity invariant, spec §3.2).

use crate::engine::{terminal_size, Rng};
use crate::palettes::Palette;
use crate::ufo::ring_cells;
use crate::{config::AnimSettings, ESC};
use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Black hole phases (spec §2). Order matters — monotonic in t.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Emergence,
    Attraction,
    Collapse,
    Flash,
}

/// Phase boundaries as half-open intervals [lo, hi) (spec §2). Flash includes t == 1.0.
const PHASE_EMERGE_END: f32 = 0.12;
const PHASE_ATTRACT_END: f32 = 0.72;
const PHASE_COLLAPSE_END: f32 = 0.88;
// PHASE_FLASH_END = 1.0 (implicit).

/// Phase by normalized time (0..=1). Boundary convention: [lo, hi);
/// phase_at(0.12) → Attraction, phase_at(0.72) → Collapse, phase_at(0.88)
/// → Flash. At t == 1.0 → Flash (as Settle in quake/crt).
pub fn phase_at(t01: f32) -> Phase {
    let t = t01.clamp(0.0, 1.0);
    if t < PHASE_EMERGE_END {
        Phase::Emergence
    } else if t < PHASE_ATTRACT_END {
        Phase::Attraction
    } else if t < PHASE_COLLAPSE_END {
        Phase::Collapse
    } else {
        Phase::Flash
    }
}

// ── Attraction wave + devouring speed (spec §3.3) ─────────────────────

/// Tuning (spec §3.7) — starting values, tuned by eye.
const START_R: i32 = 2; // startup ring: |dy| ≤ START_R is active from frame one
const BONUS_MAX: f32 = 3.0; // proximity bonus at the center
const BUDGET_MARGIN: f32 = 1.25; // far-row frame budget safety factor

/// Attraction wave front radius (spec §3.3): `p_attr` is the Attraction
/// phase progress ∈ [0,1]; quadratic ease-out (fast start, slowing toward
/// FRONT_MAX — as quake's wave_radius). Clamps p outside [0,1].
pub fn pull_front(p_attr: f32, front_max: f32) -> f32 {
    let p = p_attr.clamp(0.0, 1.0);
    front_max * (1.0 - (1.0 - p) * (1.0 - p))
}

/// Has the front reached the row at vertical offset `dy` from the center
/// (spec §3.3)? `front` is the EFFECTIVE front — already max(front, START_R)
/// in the caller — so the startup ring devours from frame one. The boundary
/// is inclusive: |dy| ≤ front.
pub fn row_active(dy: i32, front: f32) -> bool {
    (dy.abs() as f32) <= front
}

/// Devouring speed for the row at vertical offset `dy` (spec §3.3), cells
/// per frame per side. 0 outside the effective front (as row_active). Inside:
/// min_rate + proximity bonus. The guarantee that every row finishes before
/// the Attraction boundary is built into the formula: min_rate is sized to
/// eat cols·BUDGET_MARGIN over the frames_left budget, so the rate only
/// falls with |dy| and is symmetric in sign.
pub fn devour_rate(dy: i32, front: f32, cols: usize, dy_max: i32, frames_left: u32) -> u32 {
    if !row_active(dy, front) {
        return 0;
    }
    // frames_left is clamped ≥ 1: a row activated at the phase boundary or
    // after a late resize has 0 frames left — it must finish in one frame,
    // not divide by zero (spec §3.3/§7).
    let min_rate = ((cols as f32 * BUDGET_MARGIN) / frames_left.max(1) as f32).ceil() as u32;
    // Linear proximity bonus: maximal at the center, 0 at the screen edge.
    let bonus = (BONUS_MAX * (1.0 - dy.abs() as f32 / (dy_max + 1) as f32))
        .round()
        .max(0.0) as u32;
    min_rate + bonus
}

/// The pull command pair for a Devouring row (spec §3.2/§4):
/// (n_ich@0, n_dch@cx). n_ich = min(rate, left) — the left cells that have
/// reached cx; n_dch = n_l + n_r — everything eaten at cx this frame.
pub fn devour_cmds(left: i32, right: i32, rate: u32) -> (u32, u32) {
    let nl = (rate as i32).min(left).max(0) as u32;
    let nr = (rate as i32).min(right).max(0) as u32;
    (nl, nl + nr)
}

// ── Hole growth + flash (spec §3.3/§3.5) ──────────────────────────────

/// The eaten mass that fills the hole to cap by ~2/3 of Attraction
/// (spec §3.3): k = cap / (MASS_FRACTION·cols·rows).
const MASS_FRACTION: f32 = 0.65;

/// The hole's vertical semi-radius by normalized time and devoured mass
/// (spec §3.3); the horizontal radius is ×2 (2:1 cell aspect, as the ufo
/// craters). Emergence: linear 1 → 2. Attraction: min(k·eaten, cap) — the
/// devoured mass grows the hole; cap = max(rows/(7 − height), 2) (bigger
/// with height, floored for mini terminals). Collapse: quadratic ease-in
/// cap → 1. Flash: linear 1 → 0. Monotone within each phase.
pub fn hole_ry(t01: f32, eaten: u32, cols: usize, rows: usize, height: i32) -> f32 {
    let t = t01.clamp(0.0, 1.0);
    let cap = ((rows as i32) / (7 - height.clamp(0, 3))).max(2) as f32;
    if t < PHASE_EMERGE_END {
        1.0 + t / PHASE_EMERGE_END
    } else if t < PHASE_ATTRACT_END {
        let k = cap / (MASS_FRACTION * (cols * rows) as f32);
        (k * eaten as f32).min(cap)
    } else if t < PHASE_COLLAPSE_END {
        let p = (t - PHASE_ATTRACT_END) / (PHASE_COLLAPSE_END - PHASE_ATTRACT_END);
        cap + (1.0 - cap) * p * p
    } else {
        1.0 - (t - PHASE_COLLAPSE_END) / (1.0 - PHASE_COLLAPSE_END)
    }
}

/// The flash ring radius (spec §3.5): `p_flash` is the Flash phase progress
/// ∈ [0,1]; quadratic ease-out (as the wave front). 0 → 0, 1 → r_max.
pub fn flash_radius(p_flash: f32, r_max: f32) -> f32 {
    let p = p_flash.clamp(0.0, 1.0);
    r_max * (1.0 - (1.0 - p) * (1.0 - p))
}

// ── Disk glyphs + colors (spec §3.4) ──────────────────────────────────

/// Weighted disk glyph table (spec §3.4): '·'×4, '∙'×2, '•'×2, '⋆'×2,
/// '˙'×2, '✦'×1. Sum of weights = 13.
const DISK_TABLE: &[char] = &[
    '·', '·', '·', '·',
    '∙', '∙',
    '•', '•',
    '⋆', '⋆',
    '˙', '˙',
    '✦',
];
const DISK_TABLE_LEN: usize = DISK_TABLE.len();

/// Disk glyph by index into the weighted table (spec §4). Safe for any idx:
/// taken modulo the length, so the inclusive Rng::range can never go out of
/// bounds (same safety as glyph_at in crt.rs, spec §3.4).
pub fn disk_glyph(idx: usize) -> char {
    DISK_TABLE[idx % DISK_TABLE_LEN]
}

/// Disk particle color (spec §3.4): idx is clamped into 9..=36 — the bright
/// palette half 18..=36 at the disk's inner edge, the dim band 9..=17 at the
/// outer edge. Direct index, no soften() — the palette is already softened
/// in config::build_palette.
pub fn disk_color(palette: &Palette, idx: i32) -> (u8, u8, u8) {
    palette[idx.clamp(9, 36) as usize]
}

// ── Elliptic geometry (spec §4) ───────────────────────────────────────

/// Elliptical distance with a 2:1 aspect (spec §4) — a circle on ~2:1
/// terminal cells, the same metric as ufo::ring_cells. The hole core is the
/// set aspect_dist ≤ hole_ry; the minimum over a row sits on the center
/// vertical, where dx = 0.
pub fn aspect_dist(x: i32, y: i32, cx: i32, cy: i32) -> f32 {
    let dx = (x - cx) as f32 / 2.0;
    let dy = (y - cy) as f32;
    dx.hypot(dy)
}

/// Position on a 2:1 ellipse orbit (spec §3.4):
/// (cx + 2·radius·cos(angle), cy + radius·sin(angle)).
pub fn orbit_pos(cx: i32, cy: i32, angle: f32, radius: f32) -> (f32, f32) {
    (
        cx as f32 + 2.0 * radius * angle.cos(),
        cy as f32 + radius * angle.sin(),
    )
}

// ── Particles: the accretion disk (spec §3.4) ─────────────────────────

/// Disk tuning (spec §3.7) — starting values, tuned by eye.
const INFALL: f32 = 0.03; // radial drift toward the hole, cells/frame
const K_KEPLER: f32 = 1.0; // Keplerian spin-up factor

/// One disk particle (spec §3.4): polar mechanics on a 2:1 ellipse — an
/// orbit, not quake's ballistic debris. `age` is incremented in run(), the
/// MAX_AGE comparison lives there too (as in quake).
pub struct Particle {
    angle: f32,  // θ on the 2:1 ellipse
    radius: f32, // current orbit radius (in "round" units — the ry)
    speed: f32,  // base angular velocity
    ch: char,
    color: (u8, u8, u8),
    age: u32, // frames alive; death by MAX_AGE — a run() check
}

/// One frame of orbital mechanics (spec §3.4): the Keplerian spin-up — the
/// angular velocity grows toward the center, factor (1 + K/radius) — plus
/// the infall drift. Physics only; death is decided by particle_dies in run().
pub fn step_particle(p: &mut Particle, dir: i32) {
    // The max() guards the division: a particle may still orbit a hole that
    // has already collapsed (Flash shrinks hole_ry to 0) — the next death
    // pass in run() reaps it; meanwhile the step must not produce inf/NaN.
    let kepler = 1.0 + K_KEPLER / p.radius.max(0.001);
    p.angle += dir as f32 * p.speed * kepler;
    p.radius -= INFALL;
}

/// Particle death by the horizon (spec §3.4): radius ≤ hole_ry — the
/// particle dove behind it and goes out.
pub fn particle_dies(p: &Particle, hole_ry: f32) -> bool {
    p.radius <= hole_ry
}

// ── Overlay cell + grid primitives (as in ufo.rs, extended with bg) ────

/// Ov carries a BACKGROUND color — the one extension of the shared base
/// model (spec §3.2): the samples' render always resets ESC[49m, which
/// makes the black core and the white flash undrawable. Ov/render are
/// private in every module, so the neighbors are unaffected.
#[derive(Clone, Copy)]
struct Ov {
    ch: char,
    color: Option<(u8, u8, u8)>, // fg; None ⇒ default fg (the erase space)
    bg: Option<(u8, u8, u8)>, // bg; None ⇒ default bg; Some — core/flash only
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

/// Render the overlay grid into a String (as in ufo.rs, BUT the buffer is
/// NOT cleared here: a blackhole frame starts with the ICH/DCH pull prefix
/// that run() writes into the same buf before calling render — one String,
/// one write_all per frame, spec §3.2). fg and bg are batched independently:
/// each is emitted only on change and always re-emitted after a cursor
/// move. None cells are skipped so the original terminal text shows through
/// until the effect reaches it.
fn render(buf: &mut String, grid: &[Option<Ov>], cols: usize, rows: usize) {
    let mut last_color: Option<Option<(u8, u8, u8)>> = None;
    let mut last_bg: Option<Option<(u8, u8, u8)>> = None;
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
                last_color = None; // colors must be re-emitted after a cursor move
                last_bg = None;
                need_move = false;
                wrow = y;
                wcol = x;
            }
            if last_color != Some(ov.color) {
                match ov.color {
                    Some((r, g, b)) => {
                        let _ = write!(buf, "{ESC}[38;2;{r};{g};{b}m");
                    }
                    None => {
                        let _ = write!(buf, "{ESC}[39m");
                    }
                }
                last_color = Some(ov.color);
            }
            if last_bg != Some(ov.bg) {
                match ov.bg {
                    Some((r, g, b)) => {
                        let _ = write!(buf, "{ESC}[48;2;{r};{g};{b}m");
                    }
                    None => {
                        let _ = write!(buf, "{ESC}[49m");
                    }
                }
                last_bg = Some(ov.bg);
            }
            buf.push(ov.ch);
            wcol += 1;
        }
    }
    let _ = write!(buf, "{ESC}[0m");
}

/// Linear interpolation between two RGB colors — the rim → white ramp of
/// the Collapse and the flash ramp (spec §3.4/§3.5); `t` is clamped to [0,1].
fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
}

// ── Main-loop tuning constants (spec §3.7) ─────────────────────────────

const MAX_AGE: u32 = 120; // hang-proof particle lifetime, frames
const SPARKLE_FRAMES: u32 = 2; // horizon-death bright dot lease, frames (spec §3.4)
const SPARKLE_DRIFT: f32 = 0.6; // the dot sits just outside the horizon, at the rim's inner edge
const MAX_PARTICLES: usize = 400; // live particle cap
const DISK_SPAWN_PER_FRAME: usize = 2; // particles born at the disk edge per frame
const DISK_SPEED_LO: f32 = 0.05; // base angular velocity range, rad/frame
const DISK_SPEED_HI: f32 = 0.15;
const DISK_COMPRESS: f32 = 0.9; // Collapse shrinks the disk band to 10% by Flash
const FLASH_CORE_FRAMES: u32 = 3; // white core hold at the flash point (§3.5)
const FLASH_CORE_RY: f32 = 1.5; // white core semi-radius at the flash point
const FLASH_RAMP_LEN: usize = 6; // flash ring color steps: white → palette[30]

/// The only opaque backgrounds (spec §3.2/§3.5).
const CORE_BG: (u8, u8, u8) = (0, 0, 0);
const WHITE: (u8, u8, u8) = (255, 255, 255);

/// Per-row state machine (spec §3.2), parallel to the time phases:
/// Untouched (the front has not arrived) → Devouring (left/right counters
/// of live text) → Devoured (empty; only such rows carry drawn cells — the
/// core, the rim and the particles are stamped into Devoured cells only).
#[derive(Clone, Copy, PartialEq, Eq)]
enum RowState {
    Untouched,
    Devouring,
    Devoured,
}

/// Random float in [0, 1) on top of the integer PRNG (there is no rand crate).
fn rand_f01(rng: &mut Rng) -> f32 {
    (rng.next_u64() % 1_000_000) as f32 / 1_000_000.0
}

/// Random float in [lo, hi).
fn rand_frng(rng: &mut Rng, lo: f32, hi: f32) -> f32 {
    lo + rand_f01(rng) * (hi - lo)
}

/// One disk particle (spec §3.4): born at the outer edge of the disk with a
/// random angle and base angular velocity; the color starts dim (the outer
/// edge, idx 9) and is refreshed each frame in run() as the radius shrinks.
fn new_disk_particle(rng: &mut Rng, palette: &Palette, ry: f32, margin_eff: f32) -> Particle {
    Particle {
        angle: rand_frng(rng, 0.0, std::f32::consts::TAU),
        radius: ry + margin_eff,
        speed: rand_frng(rng, DISK_SPEED_LO, DISK_SPEED_HI),
        ch: disk_glyph(rng.range(0, DISK_TABLE_LEN as i32) as usize),
        color: disk_color(palette, 9),
        age: 0,
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
    // Clamp the duration from below to 1.0 s (spec §2) — the pull needs a
    // frame budget (quake clamps at 0.6, crt at 0.5).
    let t = Duration::from_secs_f32(settings.duration.max(1.0));

    let mut rng = Rng::new();
    // Spin direction (spec §3.6): the sign of wind; wind = 0 → a random
    // direction picked once at start; any non-zero `direction` reverses it.
    let mut dir: i32 = settings.wind.signum();
    if dir == 0 {
        dir = if rand_f01(&mut rng) < 0.5 { -1 } else { 1 };
    }
    if settings.direction != 0 {
        dir = -dir;
    }

    // The center is fixed at the middle of the screen (spec §3.1) — the
    // symmetry is part of the image (unlike quake's random epicenter).
    let mut cx = (cols / 2) as i32;
    let mut cy = (rows / 2) as i32;
    let mut burned = vec![false; cols * rows];
    let mut grid: Vec<Option<Ov>> = vec![None; cols * rows];
    let mut row_state = vec![RowState::Untouched; rows];
    let mut left = vec![cx; rows]; // live text cells left of cx
    let mut right = vec![cols as i32 - cx; rows]; // and right of it
    let mut eaten: u32 = 0; // total devoured cells — the hole's mass
    let mut particles: Vec<Particle> = Vec::new();
    let mut flash_frames: u32 = 0; // white core hold counter (spec §3.5)
    let mut buf = String::with_capacity(cols * rows * 6);

    // Flash ring ramp (spec §3.5): 6 steps white → palette[30] — its own
    // ramp, ufo's C_BLAST is private (spec §9).
    let flash_ramp: [(u8, u8, u8); FLASH_RAMP_LEN] = std::array::from_fn(|i| {
        lerp_rgb(WHITE, palette[30], i as f32 / (FLASH_RAMP_LEN - 1) as f32)
    });

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

        // Live resize (spec §7): the whole state is recreated for the new
        // geometry — (cx, cy) recomputed, new rows start Untouched (the
        // still-growing front picks them up; a resize after 0.72 is covered
        // by the Flash cleanup), burned/grid resized, eaten = 0 (the mass
        // is meaningless without the devoured rows); `t` is NOT reset.
        let (nc, nr) = terminal_size();
        if nc != cols || nr != rows {
            cols = nc;
            rows = nr;
            if cols < 8 || rows < 4 {
                break;
            }
            cx = (cols / 2) as i32;
            cy = (rows / 2) as i32;
            burned = vec![false; cols * rows];
            grid = vec![None; cols * rows];
            row_state = vec![RowState::Untouched; rows];
            left = vec![cx; rows];
            right = vec![cols as i32 - cx; rows];
            eaten = 0;
            particles.clear();
            buf.reserve(cols * rows * 6);
        }
        let (ci, ri) = (cols as i32, rows as i32);
        // MARGIN ≈ rows/4 (spec §3.7) — the disk's outer edge offset.
        let margin = (rows / 4) as f32;

        // Reset the overlay for this frame.
        for cell in grid.iter_mut() {
            *cell = None;
        }

        // Normalized time, phase progresses.
        let t01 = (elapsed.as_secs_f32() / t.as_secs_f32()).clamp(0.0, 1.0);
        let phase = phase_at(t01);
        let p_attr =
            ((t01 - PHASE_EMERGE_END) / (PHASE_ATTRACT_END - PHASE_EMERGE_END)).clamp(0.0, 1.0);
        let p_collapse = ((t01 - PHASE_ATTRACT_END) / (PHASE_COLLAPSE_END - PHASE_ATTRACT_END))
            .clamp(0.0, 1.0);
        let p_flash = ((t01 - PHASE_COLLAPSE_END) / (1.0 - PHASE_COLLAPSE_END)).clamp(0.0, 1.0);
        let flash = matches!(phase, Phase::Flash);

        let dy_max = cy.max(ri - 1 - cy);
        // FRONT_MAX = max|dy| + 1 (spec §3.3); the effective front folds in
        // the startup ring |dy| ≤ START_R, active from frame one — so the
        // same value gates both activation and devour_rate (review point 3).
        let front_eff = pull_front(p_attr, (dy_max + 1) as f32).max(START_R as f32);
        // Frame budget until the end of Attraction (spec §3.3). Clamped ≥ 1:
        // a row activated at the phase boundary or after a late resize must
        // finish in one frame, not divide by zero (§7).
        let frames_left =
            (((PHASE_ATTRACT_END - t01) * t.as_secs_f32() * fps as f32).ceil() as u32).max(1);
        // The hole's vertical semi-radius by time and mass.
        let ry = hole_ry(t01, eaten, cols, rows, settings.height);
        // Collapse compresses the disk (spec §2 "диск сжимается"): the outer
        // edge follows the shrinking band, so the last particles dive by the
        // Flash boundary. step_particle itself stays as spec §4 defines it.
        let margin_eff = margin * (1.0 - DISK_COMPRESS * p_collapse);

        buf.clear();

        // (1) Activation (spec §3.3): first contact is on the center
        // vertical, dx = 0 — hence |cy − y| ≤ front_eff.
        for y in 0..ri {
            let yi = y as usize;
            if row_state[yi] == RowState::Untouched && row_active(cy - y, front_eff) {
                row_state[yi] = RowState::Devouring;
            }
        }

        // (2) The pull (spec §3.2): one ICH@0 + DCH@cx pair per Devouring
        // row per frame — both text halves crawl toward the center vertical
        // and vanish at it. Devouring rows have an entirely empty overlay:
        // the honest crawl of the text IS the row animation (model purity
        // invariant, §3.2). ICH inserts blanks at column 0 (nothing ever
        // leaves the left edge); DCH eats the arrived cells at cx —
        // one-sided loss, harmless as in quake.
        for y in 0..ri {
            let yi = y as usize;
            if row_state[yi] != RowState::Devouring {
                continue;
            }
            let rate = devour_rate(cy - y, front_eff, cols, dy_max, frames_left);
            let (n_ich, n_dch) = devour_cmds(left[yi], right[yi], rate);
            if n_ich > 0 {
                let _ = write!(buf, "{ESC}[{};1H{ESC}[{n_ich}@", y + 1);
            }
            if n_dch > 0 {
                let _ = write!(buf, "{ESC}[{};{}H{ESC}[{n_dch}P", y + 1, cx + 1);
            }
            left[yi] -= n_ich as i32;
            right[yi] -= (n_dch - n_ich) as i32;
            eaten += n_dch; // n_dch = n_l + n_r — the mass devoured this frame
            if left[yi] == 0 && right[yi] == 0 {
                row_state[yi] = RowState::Devoured;
            }
        }

        // (3) The accretion disk (spec §3.4): spawn at the outer edge (none
        // in Flash — the disk is gone by then, §3.5), step, then death by
        // the new radius (checked before drawing — only live particles are
        // drawn). The color follows the current radius: bright 36 at the
        // hole's edge, dim 9 at the outer edge; disk_color clamps the rest.
        if !flash {
            for _ in 0..DISK_SPAWN_PER_FRAME {
                if particles.len() >= MAX_PARTICLES {
                    break;
                }
                particles.push(new_disk_particle(&mut rng, palette, ry, margin_eff));
            }
        }
        for p in particles.iter_mut() {
            step_particle(p, dir);
            p.age += 1;
            p.radius = p.radius.min(ry + margin_eff); // Collapse compression
            let idx = 36.0 - 27.0 * (p.radius - ry).max(0.0) / margin_eff.max(0.001);
            p.color = disk_color(palette, idx as i32);
        }
        // Death (spec §3.4): a horizon dive leaves a short bright dot at the
        // rim's inner edge — the dying particle is respawned in place just
        // outside the horizon with a SPARKLE_FRAMES lease (age set near
        // MAX_AGE, so the age check reaps it right after); an age death
        // simply drops out, no dot. The color needs no fixup: the radius
        // mapping above already yields the brightest step (idx 36) at the
        // horizon. Mechanism fixed by the plan — spec §3.4 mentions the dot
        // in prose only (§4/§5/§8 define nothing), see risk R8.
        let mut i = 0;
        while i < particles.len() {
            if particle_dies(&particles[i], ry) {
                let mut dot = particles.remove(i);
                dot.radius = ry + SPARKLE_DRIFT;
                dot.age = MAX_AGE - SPARKLE_FRAMES; // short lease, reaped by age
                particles.push(dot);
            } else if particles[i].age > MAX_AGE {
                particles.remove(i);
            } else {
                i += 1;
            }
        }

        // (4) Frame assembly (spec §3.2): the pull prefix is already in buf;
        // now erase → rim → black core → particles → flash, then ONE
        // write_all (§5: the flash is stamped after the disk and covers it).
        // From Flash on, the WHOLE screen is force-marked burned
        // with no regard for row states and with no spawn (§3.5) — a late
        // resize must not leave the cleanup dirty; the erase layer below
        // blanks it in this very frame.
        if flash {
            flash_frames += 1;
            for cell in burned.iter_mut() {
                *cell = true;
            }
        }
        for y in 0..ri {
            for x in 0..ci {
                if burned[y as usize * cols + x as usize] {
                    stamp(&mut grid, ci, ri, x, y, Ov { ch: ' ', color: None, bg: None });
                }
            }
        }
        // The rim: a band of cells on the core boundary (spec §3.4) —
        // ufo::ring_cells with r = hole_ry; the core stamped after it covers
        // the inner ~2/3 of the band (its normalized-distance window is
        // 0.6..=1.2), so the visible rim is thin, along the outer edge.
        // The color runs from palette[36] to white over Collapse.
        let rim_color = lerp_rgb(palette[36], WHITE, p_collapse);
        let rim_r = ry.round().max(1.0) as i32;
        for &(x, y) in &ring_cells(cx, cy, rim_r, ci, ri) {
            if row_state[y] != RowState::Devoured {
                continue; // on other rows the void at cx plays the hole (§3.2)
            }
            burn(&mut burned, ci, ri, x as i32, y as i32);
            stamp(&mut grid, ci, ri, x as i32, y as i32, Ov { ch: ' ', color: None, bg: Some(rim_color) });
        }
        // The black core (spec §3.2): an explicit black background over the
        // erase layer — without it the hole is invisible on a light terminal
        // theme. aspect_dist ≤ ry IS the core ellipse in ring_cells' own
        // metric (rx = 2·ry). Never drawn on non-Devoured rows.
        let ry_i = ry.ceil() as i32;
        for dy in -ry_i..=ry_i {
            let y = cy + dy;
            if !(0..ri).contains(&y) || row_state[y as usize] != RowState::Devoured {
                continue;
            }
            for dx in -(2 * ry_i)..=(2 * ry_i) {
                let x = cx + dx;
                if !(0..ci).contains(&x) {
                    continue;
                }
                if aspect_dist(x, y, cx, cy) <= ry {
                    burn(&mut burned, ci, ri, x, y);
                    stamp(&mut grid, ci, ri, x, y, Ov { ch: ' ', color: None, bg: Some(CORE_BG) });
                }
            }
        }
        // Particles over the erase/rim/core layers (spec §3.2/§3.4):
        // position (x.round(), y.floor()), every drawn cell is burned — the
        // next frame's erase layer wipes the previous one, no trails (as the
        // quake crumbs). Cells landing on non-Devoured rows are skipped: the
        // particle stays alive and keeps orbiting, it is just not drawn
        // (§3.2). The flash block below is stamped AFTER the particles — the
        // skeleton order of §5: in Flash the disk glyphs are still alive, so
        // the white core and the ring cover them, not the other way round.
        for p in particles.iter() {
            let (px, py) = orbit_pos(cx, cy, p.angle, p.radius);
            let x = px.round() as i32;
            let y = py.floor() as i32;
            if !(0..ci).contains(&x) || !(0..ri).contains(&y) {
                continue;
            }
            if row_state[y as usize] != RowState::Devoured {
                continue;
            }
            burn(&mut burned, ci, ri, x, y);
            stamp(&mut grid, ci, ri, x, y, Ov { ch: p.ch, color: Some(p.color), bg: None });
        }

        // The flash (spec §3.5): the point floods white for a few frames,
        // then the expanding ring fades white → palette[30].
        if flash {
            if flash_frames <= FLASH_CORE_FRAMES {
                let fry = FLASH_CORE_RY.ceil() as i32;
                for dy in -fry..=fry {
                    let y = cy + dy;
                    if !(0..ri).contains(&y) || row_state[y as usize] != RowState::Devoured {
                        continue;
                    }
                    for dx in -(2 * fry)..=(2 * fry) {
                        let x = cx + dx;
                        if !(0..ci).contains(&x) {
                            continue;
                        }
                        if aspect_dist(x, y, cx, cy) <= FLASH_CORE_RY {
                            burn(&mut burned, ci, ri, x, y);
                            stamp(&mut grid, ci, ri, x, y, Ov { ch: ' ', color: None, bg: Some(WHITE) });
                        }
                    }
                }
            }
            // FLASH_R_MAX: the max elliptical distance from the center to a
            // screen corner — the ring sweeps the whole screen by p = 1.
            let r_max = aspect_dist(0, 0, cx, cy)
                .max(aspect_dist(ci - 1, 0, cx, cy))
                .max(aspect_dist(0, ri - 1, cx, cy))
                .max(aspect_dist(ci - 1, ri - 1, cx, cy));
            let fr = flash_radius(p_flash, r_max).round().max(1.0) as i32;
            let step = (p_flash * (FLASH_RAMP_LEN - 1) as f32).round() as usize;
            let color = flash_ramp[step.min(FLASH_RAMP_LEN - 1)];
            for &(x, y) in &ring_cells(cx, cy, fr, ci, ri) {
                burn(&mut burned, ci, ri, x as i32, y as i32);
                stamp(&mut grid, ci, ri, x as i32, y as i32, Ov { ch: ' ', color: None, bg: Some(color) });
            }
        }

        render(&mut buf, &grid, cols, rows);
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
        std::thread::sleep(frame_delay);
    }

    // Always restore the cursor — main() does the final clear (it also
    // covers any half-pulled rows left over after an interrupt, spec §7).
    let _ = write!(out, "{ESC}[?25h");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_boundaries() {
        // [lo, hi) convention: the boundary belongs to the next phase.
        assert_eq!(phase_at(0.0), Phase::Emergence);
        assert_eq!(phase_at(0.11), Phase::Emergence);
        assert_eq!(phase_at(0.12), Phase::Attraction);
        assert_eq!(phase_at(0.719), Phase::Attraction);
        assert_eq!(phase_at(0.72), Phase::Collapse);
        assert_eq!(phase_at(0.879), Phase::Collapse);
        assert_eq!(phase_at(0.88), Phase::Flash);
        assert_eq!(phase_at(0.999), Phase::Flash);
        // Flash includes t == 1.0 (as Settle in quake/crt, spec §2).
        assert_eq!(phase_at(1.0), Phase::Flash);
    }

    #[test]
    fn phase_clamps_out_of_range() {
        // Values outside [0,1] must not panic.
        assert_eq!(phase_at(-0.5), Phase::Emergence);
        assert_eq!(phase_at(1.5), Phase::Flash);
    }

    #[test]
    fn phase_monotonic() {
        // The phase sequence never jumps backward as t grows.
        fn order(p: Phase) -> u8 {
            match p {
                Phase::Emergence => 0,
                Phase::Attraction => 1,
                Phase::Collapse => 2,
                Phase::Flash => 3,
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
    fn pull_front_bounds() {
        let front_max = 12.0;
        assert_eq!(pull_front(0.0, front_max), 0.0);
        assert_eq!(pull_front(1.0, front_max), front_max);
        // Monotone growth over the phase.
        let mut prev = pull_front(0.0, front_max);
        for i in 1..=100 {
            let p = i as f32 / 100.0;
            let f = pull_front(p, front_max);
            assert!(f >= prev, "front regressed at p={p}: {f} < {prev}");
            prev = f;
        }
        // Outside [0,1] clamps.
        assert_eq!(pull_front(-0.5, front_max), 0.0);
        assert_eq!(pull_front(1.5, front_max), front_max);
    }

    #[test]
    fn row_active_edges() {
        // The boundary is included: |dy| == front → true; |dy| == front + ε → false.
        assert!(row_active(3, 3.0));
        assert!(row_active(-3, 3.0));
        assert!(!row_active(4, 3.0));
        assert!(!row_active(-4, 3.0));
        assert!(!row_active(4, 3.9999));
        // The front argument is the EFFECTIVE front — already max(front, START_R)
        // (spec §3.3): a raw front of 0 still activates the startup ring |dy| ≤ 2.
        assert!(row_active(2, 2.0));
        assert!(!row_active(3, 2.0));
        assert!(row_active(0, 0.0));
        assert!(!row_active(1, 0.0));
    }

    #[test]
    fn devour_rate_zero_outside_front() {
        // Outside the effective front the row is not eating at all.
        assert_eq!(devour_rate(6, 5.0, 80, 12, 60), 0);
        assert_eq!(devour_rate(-6, 5.0, 80, 12, 60), 0);
        // Inside — always ≥ 1 for a usable terminal.
        assert!(devour_rate(5, 5.0, 80, 12, 60) >= 1);
        assert!(devour_rate(0, 5.0, 80, 12, 60) >= 1);
    }

    #[test]
    fn devour_rate_monotonic_in_dy() {
        // The rate falls with |dy| and is symmetric in sign (spec §3.3).
        let (front, cols, dy_max, frames_left) = (100.0, 80, 30, 60);
        for dy in 0..dy_max {
            let near = devour_rate(dy, front, cols, dy_max, frames_left);
            let far = devour_rate(dy + 1, front, cols, dy_max, frames_left);
            assert!(near >= far, "rate must not grow with |dy|: {dy}");
            assert_eq!(near, devour_rate(-dy, front, cols, dy_max, frames_left));
        }
        // The center gets the maximum: min_rate + BONUS_MAX.
        let min_rate = ((cols as f32 * BUDGET_MARGIN) / frames_left as f32).ceil() as u32;
        assert_eq!(
            devour_rate(0, front, cols, dy_max, frames_left),
            min_rate + BONUS_MAX as u32
        );
    }

    #[test]
    fn devour_rate_budget() {
        // The headline guarantee of §3.3: cols = 300, frames_left = 30 →
        // min_rate = ceil(300·1.25/30) = 13 — any in-front row finishes within
        // the phase even without the bonus.
        for dy in [0, 15, 29, -29] {
            let rate = devour_rate(dy, 100.0, 300, 30, 30);
            assert!(rate >= 13, "rate {rate} < 13 at dy={dy}");
        }
    }

    #[test]
    fn devour_rate_frames_left_clamp() {
        // frames_left = 0 (a row activated at the phase boundary or after a
        // late resize, spec §7) must not panic or overflow: the clamp turns
        // it into "finish the row in one frame" — rate ≥ ceil(cols·1.25).
        let rate = devour_rate(10, 100.0, 80, 30, 0);
        assert!(rate >= 100, "frames_left=0 must devour in one frame, got {rate}");
        assert!(devour_rate(10, 100.0, 80, 30, 1) >= 100);
    }

    #[test]
    fn devour_cmds_pair() {
        // left=5, right=7, rate=3 → ICH 3, DCH 6 (spec §8).
        assert_eq!(devour_cmds(5, 7, 3), (3, 6));
        // Rate above the leftovers → everything, in one frame.
        assert_eq!(devour_cmds(2, 3, 10), (2, 5));
        assert_eq!(devour_cmds(4, 0, 10), (4, 4));
        // An empty left side → no ICH, only the right DCH.
        assert_eq!(devour_cmds(0, 7, 3), (0, 3));
        // A drained row → nothing at all.
        assert_eq!(devour_cmds(0, 0, 5), (0, 0));
    }

    #[test]
    fn hole_ry_growth_and_cap() {
        // Emergence: linear 1 → 2, monotone.
        assert_eq!(hole_ry(0.0, 0, 80, 24, 1), 1.0);
        let mut prev = hole_ry(0.0, 0, 80, 24, 1);
        for i in 1..=50 {
            let t = PHASE_EMERGE_END * i as f32 / 50.0;
            let r = hole_ry(t, 5_000, 80, 24, 1);
            assert!(r >= prev, "hole shrank inside Emergence at t={t}: {r} < {prev}");
            prev = r;
        }
        // Attraction: monotone in eaten, saturating at cap = max(rows/(7−h), 2).
        let cap = (24.0f32 / 6.0).floor().max(2.0); // height = 1 → 24/6 = 4
        let mut prev = hole_ry(0.4, 0, 80, 24, 1);
        for eaten in [1u32, 50, 500, 2_000, 10_000, 1_000_000] {
            let r = hole_ry(0.4, eaten, 80, 24, 1);
            assert!(r >= prev, "hole shrank as eaten grew: {r} < {prev}");
            assert!(r <= cap, "cap exceeded: {r} > {cap}");
            prev = r;
        }
        assert_eq!(prev, cap); // saturated at cap
        // Mini terminals rows 4..6: integer division would give 0–1 — the
        // floor keeps cap ≥ 2 (spec §3.3), so Emergence's 1 → 2 never exceeds it.
        for rows in [4usize, 5, 6] {
            for height in 0..=3 {
                assert_eq!(hole_ry(0.5, 1_000_000, 20, rows, height), 2.0);
            }
        }
        // Collapse: ease-in cap → 1, monotone shrink.
        let mut prev = hole_ry(PHASE_ATTRACT_END, 1_000_000, 80, 24, 1);
        for i in 1..=50 {
            let t = PHASE_ATTRACT_END + (PHASE_COLLAPSE_END - PHASE_ATTRACT_END) * i as f32 / 50.0;
            let r = hole_ry(t, 1_000_000, 80, 24, 1);
            assert!(r <= prev, "hole grew inside Collapse at t={t}: {r} > {prev}");
            prev = r;
        }
        assert!((prev - 1.0).abs() < 1e-6, "Collapse must end at 1.0, got {prev}");
        // Flash: 1 → 0.
        assert!((hole_ry(PHASE_COLLAPSE_END, 1_000_000, 80, 24, 1) - 1.0).abs() < 1e-6);
        assert!(hole_ry(1.0, 1_000_000, 80, 24, 1).abs() < 1e-6);
    }

    #[test]
    fn hole_ry_height_scales() {
        // cap = max(rows/(7−height), 2) grows with height (integer division).
        let mut prev = hole_ry(0.5, 1_000_000, 80, 24, -1); // clamped to 0
        for height in 0..=3 {
            let r = hole_ry(0.5, 1_000_000, 80, 24, height);
            assert!(r >= prev, "cap must not shrink as height grows");
            prev = r;
        }
        // Exact caps for rows = 24: h 0..3 → 3, 4, 4, 6.
        assert_eq!(hole_ry(0.5, 1_000_000, 80, 24, 0), 3.0);
        assert_eq!(hole_ry(0.5, 1_000_000, 80, 24, 1), 4.0);
        assert_eq!(hole_ry(0.5, 1_000_000, 80, 24, 2), 4.0);
        assert_eq!(hole_ry(0.5, 1_000_000, 80, 24, 3), 6.0);
    }

    #[test]
    fn flash_radius_bounds() {
        let r_max = 21.5;
        assert_eq!(flash_radius(0.0, r_max), 0.0);
        assert_eq!(flash_radius(1.0, r_max), r_max);
        // Monotone growth over the phase.
        let mut prev = flash_radius(0.0, r_max);
        for i in 1..=100 {
            let p = i as f32 / 100.0;
            let r = flash_radius(p, r_max);
            assert!(r >= prev, "ring regressed at p={p}: {r} < {prev}");
            prev = r;
        }
        // Outside [0,1] clamps.
        assert_eq!(flash_radius(-0.5, r_max), 0.0);
        assert_eq!(flash_radius(1.5, r_max), r_max);
    }

    #[test]
    fn aspect_dist_shape() {
        assert_eq!(aspect_dist(10, 5, 10, 5), 0.0);
        // 2:1 aspect: a shift of 2 columns equals a shift of 1 row.
        assert_eq!(aspect_dist(12, 5, 10, 5), aspect_dist(10, 6, 10, 5));
        assert_eq!(aspect_dist(8, 5, 10, 5), aspect_dist(10, 4, 10, 5));
        assert_eq!(aspect_dist(14, 5, 10, 5), aspect_dist(10, 7, 10, 5));
        // The minimum over a row sits on the center vertical (dx = 0).
        let on_vertical = aspect_dist(10, 3, 10, 5);
        for dx in [-6, -2, 2, 6] {
            assert!(
                aspect_dist(10 + dx, 3, 10, 5) > on_vertical,
                "dx={dx} must be farther than dx=0"
            );
        }
    }

    #[test]
    fn orbit_pos_aspect() {
        // radius = 1: angle 0 → (cx + 2, cy); π/2 → (cx, cy + 1) — an
        // ellipse with rx = 2·ry (spec §8).
        let (x, y) = orbit_pos(40, 12, 0.0, 1.0);
        assert_eq!(x, 42.0);
        assert_eq!(y, 12.0);
        let (x, y) = orbit_pos(40, 12, std::f32::consts::FRAC_PI_2, 1.0);
        assert!((x - 40.0).abs() < 1e-5, "x drifted: {x}");
        assert!((y - 13.0).abs() < 1e-5, "y drifted: {y}");
        // The radius scales both semi-axes (rx = 2r).
        let (x, _) = orbit_pos(40, 12, 0.0, 3.0);
        assert_eq!(x, 46.0);
    }

    #[test]
    fn disk_table_len_is_thirteen() {
        // Sum of weights: 4+2+2+2+2+1 = 13 (§3.4).
        assert_eq!(DISK_TABLE_LEN, 13);
    }

    #[test]
    fn disk_glyph_valid() {
        let allowed = ['·', '∙', '•', '⋆', '˙', '✦'];
        // Every index in [0, LEN) maps into the allowed set.
        for i in 0..DISK_TABLE_LEN {
            let ch = disk_glyph(i);
            assert!(allowed.contains(&ch), "unexpected glyph {ch:?} at index {i}");
        }
        // Every glyph of the allowed set is present (weight ≥ 1); together
        // with the loop above this also proves the table is non-empty.
        for &ch in &allowed {
            assert!(
                (0..DISK_TABLE_LEN).any(|i| disk_glyph(i) == ch),
                "glyph {ch:?} missing from table"
            );
        }
        // Out-of-range indices are safe — they wrap modulo the length
        // (the inclusive Rng::range can return LEN itself).
        for i in [DISK_TABLE_LEN, DISK_TABLE_LEN + 1, 1_000_000] {
            assert!(allowed.contains(&disk_glyph(i)));
        }
    }

    #[test]
    fn disk_color_bounds() {
        let mut pal = [(0u8, 0u8, 0u8); 37];
        for (i, slot) in pal.iter_mut().enumerate() {
            *slot = (i as u8, i as u8, i as u8); // the index is visible in the color
        }
        // idx below/above the band clamps into 9..=36 (bright half 18..=36 at
        // the inner edge, dim band 9..=17 at the outer edge, §3.4).
        for idx in [-5, 0, 8, 37, 40, 99] {
            let c = disk_color(&pal, idx);
            assert!(
                (9..=36).contains(&(c.0 as i32)),
                "idx={idx} escaped the band: color {c:?}"
            );
        }
        // In-range indices pick the palette step verbatim.
        assert_eq!(disk_color(&pal, 9), pal[9]);
        assert_eq!(disk_color(&pal, 36), pal[36]);
        assert_eq!(disk_color(&pal, 8), pal[9]);
        assert_eq!(disk_color(&pal, 37), pal[36]);
    }

    #[test]
    fn particle_spirals_in() {
        let mk = || Particle {
            angle: 0.5,
            radius: 8.0,
            speed: 0.1,
            ch: '·',
            color: (0, 0, 0),
            age: 0,
        };
        let mut p = mk();
        let (a0, r0) = (p.angle, p.radius);
        step_particle(&mut p, 1);
        assert!(p.radius < r0, "infall must shrink the radius");
        assert!(p.angle > a0, "dir=+1 must advance the angle");
        // dir = −1 flips the sign of the angular increment (spec §8).
        let mut q = mk();
        step_particle(&mut q, -1);
        assert!(q.angle < a0, "dir=−1 must retard the angle");
        assert!(q.radius < r0);
    }

    #[test]
    fn particle_dies_at_horizon() {
        let p = Particle {
            angle: 0.0,
            radius: 3.0,
            speed: 0.1,
            ch: '·',
            color: (0, 0, 0),
            age: 0,
        };
        // On the horizon (radius == hole_ry) → dives; the hole grew past the
        // orbit → dives. Still outside → alive.
        assert!(particle_dies(&p, 3.0));
        assert!(particle_dies(&p, 5.0));
        assert!(!particle_dies(&p, 2.9));
    }
}
