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
}
