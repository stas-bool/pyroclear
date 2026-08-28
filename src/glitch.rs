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

use crate::palettes::Palette;

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

/// One ICH/DCH command of a tear pair (spec §3.4). Logical coordinates are
/// 0-based as everywhere in the model; the emitter writes x + 1 (as y + 1
/// in quake).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cmd {
    pub x: i32,       // 0-based column the command runs at
    pub is_ich: bool, // true → ESC[n@ (insert), false → ESC[nP (delete)
    pub n: u32,       // cell count
}

/// The tear command pair (spec §3.4): the window [a, a+w) of a row shifts
/// by d; the cells outside the window stay, |d| cells in the direction of
/// travel are lost. Unified formula: the first command runs at
/// x1 = min(a, a+d), the second at x2 = a+w+d (the window's new end), both
/// with n = |d|. d > 0 → (ICH, DCH); d < 0 → (DCH, ICH). Safe on any
/// input: a → [0, cols−1], w → [1, cols−a], |d| → [1, (cols/4).max(2)]
/// (a bigger shift is not "segmental"), x → [0, cols−1], n → [1, cols−x];
/// d == 0 or a degenerate window (w ≤ 0, cols ≤ 0) → (None, None).
/// Coordinate clamping at an edge is a degradation, not an error: "the
/// tail is partially lost" is correct for a glitch.
pub fn tear_cmds(a: i32, w: i32, d: i32, cols: i32) -> (Option<Cmd>, Option<Cmd>) {
    if d == 0 || w <= 0 || cols <= 0 {
        return (None, None);
    }
    let a = a.clamp(0, cols - 1);
    let w = w.clamp(1, cols - a);
    let n = d.abs().min((cols / 4).max(2)).max(1);
    let d = if d > 0 { n } else { -n };
    let x1 = a.min(a + d).clamp(0, cols - 1);
    let x2 = (a + w + d).clamp(0, cols - 1);
    let n1 = n.min(cols - x1).max(1);
    let n2 = n.min(cols - x2).max(1);
    if d > 0 {
        (
            Some(Cmd { x: x1, is_ich: true, n: n1 as u32 }),
            Some(Cmd { x: x2, is_ich: false, n: n2 as u32 }),
        )
    } else {
        (
            Some(Cmd { x: x1, is_ich: false, n: n1 as u32 }),
            Some(Cmd { x: x2, is_ich: true, n: n2 as u32 }),
        )
    }
}

/// Damage front radius in rows (spec §3.7): `p` is the progress over
/// Tearing+Chaos ∈ [0,1]; quadratic ease-out (fast start, slowing — the
/// same curve as quake's wave_radius and blackhole's pull_front). p = 0 →
/// 0, p = 1 → the full screen height; monotone. Clamps p outside [0,1].
pub fn damage_front(p: f32, rows: i32) -> f32 {
    let p = p.clamp(0.0, 1.0);
    rows as f32 * (1.0 - (1.0 - p) * (1.0 - p))
}

/// Does the row belong to the damage front band (spec §3.7)? `direction`:
/// 0 (default) bottom → up, 1 top → down, 2 center → out, 3 edges →
/// center. Boundaries are inclusive (as row_reached in quake). Deliberate
/// deviation from the global setting labels (config.rs: 2 = Left→Right,
/// 3 = Right→Left): the glitch wave stays vertical — the same precedent
/// as blackhole reading `direction != 0` as a spin flag.
pub fn wave_row_bias(y: i32, rows: i32, direction: u8, front: f32) -> bool {
    let fy = y as f32;
    let frows = rows as f32;
    match direction {
        1 => fy <= front, // top → down
        2 => (fy - (frows - 1.0) / 2.0).abs() <= front, // center → out
        3 => fy.min(frows - 1.0 - fy) <= front,         // edges → center
        _ => frows - 1.0 - fy <= front,                 // 0 (default): bottom → up
    }
}

// ── Noise glyphs + artifact colors (spec §3.5) ─────────────────────────

/// Weighted noise glyph table (spec §3.5): '▓'×3, '▒'×3, '░'×2, '█'×2,
/// '▄'×2, '▀'×2, '#'×1, '%'×1, '&'×1, '@'×1. Sum of weights = 18.
const NOISE_TABLE: &[char] = &[
    '▓', '▓', '▓',
    '▒', '▒', '▒',
    '░', '░',
    '█', '█',
    '▄', '▄',
    '▀', '▀',
    '#', '%', '&', '@',
];
pub const NOISE_TABLE_LEN: usize = NOISE_TABLE.len();

/// Noise glyph by index into the weighted table (spec §4). Safe for any
/// idx: taken modulo the length, so the inclusive Rng::range can never go
/// out of bounds (same safety as debris_glyph in quake, disk_glyph in
/// blackhole).
pub fn noise_glyph(idx: usize) -> char {
    NOISE_TABLE[idx % NOISE_TABLE_LEN]
}

/// The canonical glitch RGB set (spec §3.5) — an idiom no user palette
/// derives; the same precedent as blackhole's black core and white flash
/// (its spec §1 p.2–3). Pairing for twins: red↔cyan, magenta↔yellow,
/// white→white.
pub const GLITCH_RGB: [(u8, u8, u8); 5] = [
    (255, 60, 60),   // red
    (60, 255, 255),  // cyan
    (255, 60, 255),  // magenta
    (255, 255, 60),  // yellow
    (255, 255, 255), // white
];

/// Artifact color (spec §4): rgb=false → a bright palette step, pick is
/// clamped into 18..=36 (direct index, no soften — the palette is already
/// softened in config::build_palette); rgb=true → GLITCH_RGB[pick mod 5]
/// (rem_euclid, so negative picks are safe too).
pub fn artifact_color(palette: &Palette, pick: i32, rgb: bool) -> (u8, u8, u8) {
    if rgb {
        GLITCH_RGB[pick.rem_euclid(5) as usize]
    } else {
        palette[pick.clamp(18, 36) as usize]
    }
}

/// The RGB-twin color (spec §3.5): red↔cyan, magenta↔yellow, white→white.
/// Anything else (a palette step — twins only spawn on RGB bands in run,
/// so this arm is a safety net) degrades to white.
pub fn twin_color(color: (u8, u8, u8)) -> (u8, u8, u8) {
    match color {
        c if c == GLITCH_RGB[0] => GLITCH_RGB[1],
        c if c == GLITCH_RGB[1] => GLITCH_RGB[0],
        c if c == GLITCH_RGB[2] => GLITCH_RGB[3],
        c if c == GLITCH_RGB[3] => GLITCH_RGB[2],
        _ => GLITCH_RGB[4],
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

    // ── tear simulation: apply Cmd pairs to a Vec<char> as the terminal
    // would (insert blanks at x, truncating at cols; delete n chars at x).

    fn apply_cmd(row: &mut Vec<char>, cmd: &Cmd, cols: usize) {
        let x = cmd.x as usize;
        if cmd.is_ich {
            for _ in 0..cmd.n {
                row.insert(x, '·'); // the blank an ICH inserts, made visible
            }
            row.truncate(cols); // the terminal row is cols wide: pushed-out
                                // cells are lost (accepted degradation, §3.4)
        } else {
            for _ in 0..cmd.n {
                if x < row.len() {
                    row.remove(x);
                }
            }
        }
    }

    fn tear_row(a: i32, w: i32, d: i32, cols: usize, row: &str) -> String {
        let (c1, c2) = tear_cmds(a, w, d, cols as i32);
        let mut v: Vec<char> = row.chars().collect();
        if let Some(c) = c1 {
            apply_cmd(&mut v, &c, cols);
        }
        if let Some(c) = c2 {
            apply_cmd(&mut v, &c, cols);
        }
        v.into_iter().collect()
    }

    #[test]
    fn tear_cmds_right() {
        // cols=12, window DEF ([3,6)), d=+2: ICH@3 2 then DCH@8 2.
        let (c1, c2) = tear_cmds(3, 3, 2, 12);
        let c1 = c1.expect("first cmd");
        let c2 = c2.expect("second cmd");
        assert!(c1.is_ich && c1.x == 3 && c1.n == 2);
        assert!(!c2.is_ich && c2.x == 8 && c2.n == 2);
        // Simulation: the window moves +2, the tail after the eaten zone is
        // back in place (I,J at 8,9 as before), the 2 cells right after the
        // window (G,H) are eaten; K,L fall off the right margin.
        assert_eq!(tear_row(3, 3, 2, 12, "ABCDEFGHIJKL"), "ABC··DEFIJ");
    }

    #[test]
    fn tear_cmds_left() {
        // The spec's own example (§3.4): cols=10, window FG (a=5, w=2),
        // d=−2: DCH@3 2 then ICH@5 2 → "ABCFG··HIJ".
        let (c1, c2) = tear_cmds(5, 2, -2, 10);
        let c1 = c1.expect("first cmd");
        let c2 = c2.expect("second cmd");
        assert!(!c1.is_ich && c1.x == 3 && c1.n == 2);
        assert!(c2.is_ich && c2.x == 5 && c2.n == 2);
        assert_eq!(tear_row(5, 2, -2, 10, "ABCDEFGHIJ"), "ABCFG··HIJ");
        // Window moved −2 (F,G now at 3,4), tail H,I,J on place (7,8,9),
        // D,E (the cells before the window) eaten.
    }

    #[test]
    fn tear_cmds_clamps() {
        // d = 0 or a degenerate window → no commands at all.
        assert_eq!(tear_cmds(5, 2, 0, 10), (None, None));
        assert_eq!(tear_cmds(5, 0, 2, 10), (None, None));
        assert_eq!(tear_cmds(5, -3, 2, 10), (None, None));
        // No panics and in-range results for wild inputs.
        for (a, w, d, cols) in [
            (-5, 2, 2, 10),
            (100, 2, -2, 10),
            (5, 100, 50, 10),
            (5, 2, -50, 8),
            (-20, 40, 30, 20),
            (0, 1, 1, 8),
            (7, 1, -1, 8),
        ] {
            let (c1, c2) = tear_cmds(a, w, d, cols);
            for c in [c1, c2].into_iter().flatten() {
                assert!((0..cols).contains(&c.x), "x={} out of [0,{cols}) for a={a} w={w} d={d}", c.x);
                assert!(c.n >= 1, "n must be ≥ 1");
                assert!(c.x + c.n as i32 <= cols, "x+n must stay inside the row");
            }
        }
        // |d| clamps to (cols/4).max(2): cols=10 → 2, cols=80 → 20. The
        // first input keeps x2 clear of the right edge: at the edge the
        // n → [1, cols−x] clamp legally shrinks n (a=10 would clamp to 9,
        // w=4 to 1, x2=12 to 9 → cols−x2=1 → n=1), and this assertion
        // checks |d|, not the edge clamp.
        let (_, c2) = tear_cmds(2, 4, 50, 10);
        assert_eq!(c2.expect("second cmd").n, 2);
        let (_, c2) = tear_cmds(10, 4, 50, 80);
        assert_eq!(c2.expect("second cmd").n, 20);
        // A tiny width clamps to [1, cols−a] without panicking.
        let (c1, _) = tear_cmds(9, 1, 2, 10);
        let c1 = c1.expect("first cmd");
        assert_eq!((c1.x, c1.n), (9, 1));
    }

    #[test]
    fn tear_cmds_order() {
        // d>0: ICH@min(a, a+d) first, DCH@(a+w+d) second; d<0: DCH@min(a, a+d)
        // first, ICH@(a+w+d) second (§3.4, the unified formula).
        let (c1, c2) = tear_cmds(3, 3, 2, 20);
        let (c1, c2) = (c1.unwrap(), c2.unwrap());
        assert_eq!((c1.is_ich, c1.x), (true, 3));
        assert_eq!((c2.is_ich, c2.x), (false, 8));
        let (c1, c2) = tear_cmds(5, 2, -2, 10);
        let (c1, c2) = (c1.unwrap(), c2.unwrap());
        assert_eq!((c1.is_ich, c1.x), (false, 3));
        assert_eq!((c2.is_ich, c2.x), (true, 5));
    }

    #[test]
    fn damage_front_bounds() {
        // p=0 → 0; p=1 → the full screen height; monotone in between;
        // outside [0,1] clamps.
        assert_eq!(damage_front(0.0, 24), 0.0);
        assert_eq!(damage_front(1.0, 24), 24.0);
        let mut prev = damage_front(0.0, 24);
        for i in 1..=100 {
            let p = i as f32 / 100.0;
            let f = damage_front(p, 24);
            assert!(f >= prev, "front regressed at p={p}: {f} < {prev}");
            prev = f;
        }
        assert_eq!(damage_front(-0.5, 24), 0.0);
        assert_eq!(damage_front(1.5, 24), 24.0);
    }

    #[test]
    fn wave_row_bias_directions() {
        // rows=24, front=6.0 — the four directions give different row sets.
        let (rows, front) = (24, 6.0f32);
        // 0 (default): bottom → up — the bottom band only. The boundary is
        // inclusive: 23−17 = 6 ≤ front → row 17 is ON the front.
        assert!(wave_row_bias(23, rows, 0, front));
        assert!(wave_row_bias(17, rows, 0, front));
        assert!(!wave_row_bias(16, rows, 0, front));
        assert!(!wave_row_bias(0, rows, 0, front));
        // 1: top → down — the top band only.
        assert!(wave_row_bias(0, rows, 1, front));
        assert!(!wave_row_bias(23, rows, 1, front));
        // 2: center → out — a band around the middle, edges untouched.
        assert!(wave_row_bias(11, rows, 2, front));
        assert!(wave_row_bias(17, rows, 2, front));
        assert!(!wave_row_bias(5, rows, 2, front));
        assert!(!wave_row_bias(0, rows, 2, front));
        assert!(!wave_row_bias(23, rows, 2, front));
        // 3: edges → center — both edges damaged, the middle not yet.
        assert!(wave_row_bias(0, rows, 3, front));
        assert!(wave_row_bias(23, rows, 3, front));
        assert!(!wave_row_bias(12, rows, 3, front));
        // The four directions really differ on the same front.
        assert_ne!(
            wave_row_bias(23, rows, 0, front),
            wave_row_bias(23, rows, 1, front)
        );
        assert_ne!(
            wave_row_bias(0, rows, 2, front),
            wave_row_bias(0, rows, 3, front)
        );
        // Growth from front=0: direction 0 starts at the bottom row alone.
        assert!(wave_row_bias(23, rows, 0, 0.0));
        assert!(!wave_row_bias(22, rows, 0, 0.0));
        assert!(!wave_row_bias(0, rows, 0, 0.0));
    }

    #[test]
    fn noise_glyph_valid() {
        let allowed = ['▓', '▒', '░', '█', '▄', '▀', '#', '%', '&', '@'];
        // Every index in [0, LEN) maps into the allowed set.
        for i in 0..NOISE_TABLE_LEN {
            let ch = noise_glyph(i);
            assert!(allowed.contains(&ch), "unexpected glyph {ch:?} at index {i}");
        }
        // Every glyph of the allowed set is present (weight ≥ 1); together
        // with the loop above this also proves the table is non-empty.
        for &ch in &allowed {
            assert!(
                (0..NOISE_TABLE_LEN).any(|i| noise_glyph(i) == ch),
                "glyph {ch:?} missing from table"
            );
        }
        // The weights of §3.5: ▓×3, ▒×3, ░×2, █×2, ▄×2, ▀×2, #×1, %×1, &×1, @×1.
        for (ch, want) in [
            ('▓', 3),
            ('▒', 3),
            ('░', 2),
            ('█', 2),
            ('▄', 2),
            ('▀', 2),
            ('#', 1),
            ('%', 1),
            ('&', 1),
            ('@', 1),
        ] {
            let got = (0..NOISE_TABLE_LEN).filter(|&i| noise_glyph(i) == ch).count();
            assert_eq!(got, want, "weight of {ch:?}: got {got}, want {want}");
        }
        // Out-of-range indices are safe — they wrap modulo the length
        // (the inclusive Rng::range can return LEN itself).
        for i in [NOISE_TABLE_LEN, NOISE_TABLE_LEN + 1, 1_000_000] {
            assert!(allowed.contains(&noise_glyph(i)));
        }
    }

    #[test]
    fn noise_table_len_is_eighteen() {
        // Sum of weights: 3+3+2+2+2+2+1+1+1+1 = 18 (§3.5).
        assert_eq!(NOISE_TABLE_LEN, 18);
    }

    #[test]
    fn artifact_color_palette() {
        let mut pal = [(0u8, 0u8, 0u8); 37];
        for (i, slot) in pal.iter_mut().enumerate() {
            *slot = (i as u8, i as u8, i as u8); // the index is visible in the color
        }
        // rgb=false: idx clamps into the bright half 18..=36.
        for pick in [-5, 0, 17, 37, 40, 99] {
            let c = artifact_color(&pal, pick, false);
            assert!(
                (18..=36).contains(&(c.0 as i32)),
                "pick={pick} escaped the bright half: color {c:?}"
            );
        }
        // In-range picks take the palette step verbatim.
        assert_eq!(artifact_color(&pal, 18, false), pal[18]);
        assert_eq!(artifact_color(&pal, 36, false), pal[36]);
        assert_eq!(artifact_color(&pal, 17, false), pal[18]);
        assert_eq!(artifact_color(&pal, 37, false), pal[36]);
    }

    #[test]
    fn artifact_color_rgb() {
        // rgb=true: only GLITCH_RGB colors, any pick, by modulo (negative
        // picks included — rem_euclid, not %).
        for pick in -7..=17 {
            let c = artifact_color(&pal37(), pick, true);
            assert!(
                GLITCH_RGB.contains(&c),
                "pick={pick} produced a non-glitch color {c:?}"
            );
            assert_eq!(c, GLITCH_RGB[pick.rem_euclid(5) as usize]);
        }
    }

    fn pal37() -> crate::palettes::Palette {
        [(0u8, 0u8, 0u8); 37]
    }

    #[test]
    fn twin_color_pairs() {
        // red↔cyan, magenta↔yellow, white→white (§3.5); anything else (a
        // palette step) degrades to white.
        assert_eq!(twin_color(GLITCH_RGB[0]), GLITCH_RGB[1]);
        assert_eq!(twin_color(GLITCH_RGB[1]), GLITCH_RGB[0]);
        assert_eq!(twin_color(GLITCH_RGB[2]), GLITCH_RGB[3]);
        assert_eq!(twin_color(GLITCH_RGB[3]), GLITCH_RGB[2]);
        assert_eq!(twin_color(GLITCH_RGB[4]), GLITCH_RGB[4]);
        assert_eq!(twin_color((10, 20, 30)), GLITCH_RGB[4]);
    }
}
