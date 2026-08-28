// glitch.rs — digital glitch clear effect: honest jolts/tears + noise bands.
//
// Four phases over normalized time t ∈ [0,1] (spec §2):
//   Tremor → Tearing → Chaos → Cutoff
// (see docs/superpowers/specs/2026-08-28-glitch-design.md).
//
// Render model mirrors ufo.rs/crt.rs/quake.rs (fg-only overlay as quake):
// an overlay grid Vec<Option<Ov>> plus a burned mask, one write_all per
// frame. The user's REAL text is damaged honestly by two shift kinds
// (spec §1): JOLT — a short ragged whole-row ICH/DCH burst (1-3 frames,
// quake's shake_cmd logic), and TEAR — a segment shift where the window
// [a, a+w) moves by d, |d| cells in the direction of travel are lost and
// the rest of the row stays (an ICH/DCH pair, as blackhole's pull). Rows
// walk a state machine Intact → Torn (a band touched the row): ICH/DCH
// only ever hits Intact rows — their overlay is empty, so the frame's
// commands never touch cells the overlay paints (model purity invariant,
// spec §3.2). The inversion flash is the real DECSCNM screen mode:
// ESC[?5h heads the flash frame's buffer, ESC[?5l is the FIRST bytes of
// the next frame's buffer — setting and resetting inside one write_all
// never reaches the screen (spec §3.6).

/// Glitch phases (spec §2). Order matters — monotonic in t.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Tremor,
    Tearing,
    Chaos,
    Cutoff,
}

/// Phase boundaries as half-open intervals [lo, hi) (spec §2). Cutoff
/// includes t == 1.0. (NOISE_END = 0.92 — the noise→blank boundary INSIDE
/// Cutoff, spec §3.8 — is introduced in the run-loop task, together with
/// its first use.)
const PHASE_TREMOR_END: f32 = 0.15;
const PHASE_TEARING_END: f32 = 0.50;
const PHASE_CHAOS_END: f32 = 0.85;
// PHASE_CUTOFF_END = 1.0 (implicit).

/// Phase by normalized time (0..=1). Boundary convention: [lo, hi);
/// phase_at(0.15) → Tearing, phase_at(0.50) → Chaos, phase_at(0.85)
/// → Cutoff. At t == 1.0 → Cutoff (as Settle in quake / Flash in blackhole).
pub fn phase_at(t01: f32) -> Phase {
    let t = t01.clamp(0.0, 1.0);
    if t < PHASE_TREMOR_END {
        Phase::Tremor
    } else if t < PHASE_TEARING_END {
        Phase::Tearing
    } else if t < PHASE_CHAOS_END {
        Phase::Chaos
    } else {
        Phase::Cutoff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_boundaries() {
        // [lo, hi) convention: the boundary belongs to the next phase.
        assert_eq!(phase_at(0.0), Phase::Tremor);
        assert_eq!(phase_at(0.149), Phase::Tremor);
        assert_eq!(phase_at(0.15), Phase::Tearing);
        assert_eq!(phase_at(0.499), Phase::Tearing);
        assert_eq!(phase_at(0.50), Phase::Chaos);
        assert_eq!(phase_at(0.849), Phase::Chaos);
        assert_eq!(phase_at(0.85), Phase::Cutoff);
        assert_eq!(phase_at(0.999), Phase::Cutoff);
        // Cutoff includes t == 1.0 (as Settle in quake / Flash in blackhole).
        assert_eq!(phase_at(1.0), Phase::Cutoff);
    }

    #[test]
    fn phase_clamps_out_of_range() {
        // Values outside [0,1] must not panic.
        assert_eq!(phase_at(-0.5), Phase::Tremor);
        assert_eq!(phase_at(1.5), Phase::Cutoff);
    }

    #[test]
    fn phase_monotonic() {
        // The phase sequence never jumps backward as t grows.
        fn order(p: Phase) -> u8 {
            match p {
                Phase::Tremor => 0,
                Phase::Tearing => 1,
                Phase::Chaos => 2,
                Phase::Cutoff => 3,
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
