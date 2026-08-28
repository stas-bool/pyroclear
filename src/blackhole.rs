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
}
