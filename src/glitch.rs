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

/// Jolt amplitude by normalized time and height (spec §3.3/§4): Tremor → 1
/// (rare single-cell jolts), Tearing/Chaos → min(1 + height, 3) (quake's
/// amp_max), Cutoff → 0 (every row is Torn / the cleanup runs — nothing to
/// jolt). Heights outside 0..=3 clamp.
pub fn amp(t01: f32, height: i32) -> i32 {
    let t = t01.clamp(0.0, 1.0);
    if t >= PHASE_CHAOS_END {
        0
    } else if t < PHASE_TREMOR_END {
        1
    } else {
        (1 + height.clamp(0, 3)).min(3)
    }
}

/// The row-shift command from the accumulated offset `off` toward `target`
/// (spec §3.3/§4, quake's logic verbatim): off < target → ICH (row content
/// moves right), off > target → DCH (moves left), equal → None. Also used
/// to compensate a row to 0 when it turns Torn: shake_cmd(row_off, 0).
pub fn shake_cmd(off: i32, target: i32) -> Option<(bool, u32)> {
    if off < target {
        Some((true, (target - off) as u32))
    } else if off > target {
        Some((false, (off - target) as u32))
    } else {
        None
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

    #[test]
    fn amp_shape() {
        // Tremor → a fixed 1 (rare single jolts); Tearing/Chaos →
        // min(1 + height, 3); Cutoff → 0 (nothing left to jolt, §3.3).
        assert_eq!(amp(0.0, 2), 1);
        assert_eq!(amp(0.149, 2), 1);
        assert_eq!(amp(0.15, 2), 3);
        assert_eq!(amp(0.50, 2), 3);
        assert_eq!(amp(0.849, 2), 3);
        assert_eq!(amp(0.85, 2), 0);
        assert_eq!(amp(0.92, 2), 0);
        assert_eq!(amp(1.0, 2), 0);
        // Monotone in height over Tearing/Chaos, clamped at 3.
        for t in [0.15f32, 0.5, 0.7, 0.849] {
            let mut prev = amp(t, -1);
            for height in 0..=3 {
                let a = amp(t, height);
                assert!(a >= prev, "amp decreased at t={t}, h={height}: {a} < {prev}");
                prev = a;
            }
            assert_eq!(prev, 3);
        }
        // Out-of-range heights clamp instead of panicking.
        assert_eq!(amp(0.5, -1), 1);
        assert_eq!(amp(0.5, 9), 3);
    }

    #[test]
    fn shake_cmd_signs() {
        // off < target → ICH (content moves right); off > target → DCH
        // (left); equal → no command (quake's logic verbatim, §3.3).
        assert_eq!(shake_cmd(0, 2), Some((true, 2)));
        assert_eq!(shake_cmd(2, 0), Some((false, 2)));
        assert_eq!(shake_cmd(-1, 1), Some((true, 2)));
        assert_eq!(shake_cmd(1, -1), Some((false, 2)));
        assert_eq!(shake_cmd(0, 0), None);
        assert_eq!(shake_cmd(3, 3), None);
    }
}
