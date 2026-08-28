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
}
