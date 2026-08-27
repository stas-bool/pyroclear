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

use crate::palettes::Palette;

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
/// bounds (same safety as glyph_at in crt.rs; see risk R2).
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
