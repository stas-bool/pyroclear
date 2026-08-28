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

use crate::palettes::Palette;
use crate::ESC;
use std::fmt::Write as _;

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
#[allow(dead_code)]
#[derive(Clone, Copy)]
struct Ov {
    ch: char,
    color: Option<(u8, u8, u8)>, // fg; None ⇒ default fg (the erase space)
    bg: Option<(u8, u8, u8)>, // bg; None ⇒ default bg; Some — core/flash only
}

/// Place an overlay cell into the grid at the given coordinates, bounds-checked.
#[allow(dead_code)]
fn stamp(grid: &mut [Option<Ov>], cols: i32, rows: i32, x: i32, y: i32, ov: Ov) {
    if (0..cols).contains(&x) && (0..rows).contains(&y) {
        grid[(y as usize) * (cols as usize) + (x as usize)] = Some(ov);
    }
}

/// Mark a cell as touched by the effect, bounds-checked (as burn() in ufo.rs).
#[allow(dead_code)]
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
#[allow(dead_code)]
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
#[allow(dead_code)]
fn lerp_rgb(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    (ch(a.0, b.0), ch(a.1, b.1), ch(a.2, b.2))
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
