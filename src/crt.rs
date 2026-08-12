// crt.rs — CRT-TV power-off effect.
//
// Five animation phases over normalized time t ∈ [0,1]:
//   Static → Collapse → Line → Dot → Flash
// (see docs/superpowers/specs/2026-08-12-crt-tv-off-design.md, §2).
//
// Render model mirrors ufo.rs: an overlay grid Vec<Option<Ov>> plus a burned
// mask, one write_all per frame. Key difference: burned = vec![true;
// cols*rows] from the very first Static frame (noise fully replaces the
// signal) and is never reset to false on resize after Static — otherwise the
// original terminal text would leak back in outside the active band.

use crate::engine::{terminal_size, Rng};
use crate::palettes::Palette;
use crate::{config::AnimSettings, ESC};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// CRT power-off animation phases (spec §2). Order matters — monotonic in t.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Static,
    Collapse,
    Line,
    Dot,
    Flash,
}

/// Phase boundaries as half-open intervals [lo, hi) (spec §2). Flash includes t == 1.0.
const PHASE_STATIC_END: f32 = 0.18;
const PHASE_COLLAPSE_END: f32 = 0.50;
const PHASE_LINE_END: f32 = 0.58;
const PHASE_DOT_END: f32 = 0.78;
// PHASE_FLASH_END = 1.0 (implicit).

// ── Overlay cell + grid primitives (as in ufo.rs) ─────────────────────

#[derive(Clone, Copy)]
struct Ov {
    ch: char,
    color: Option<(u8, u8, u8)>, // None ⇒ default fg/bg (used for the erase space)
}

/// Place an overlay cell into the grid at the given coordinates, bounds-checked.
fn stamp(grid: &mut [Option<Ov>], cols: i32, rows: i32, x: i32, y: i32, ov: Ov) {
    if (0..cols).contains(&x) && (0..rows).contains(&y) {
        grid[(y as usize) * (cols as usize) + (x as usize)] = Some(ov);
    }
}

// Note: unlike ufo.rs there is no `burn()` here. In CRT `burned` is always
// all-true (invariant §3.2: the whole screen belongs to the effect from the
// first Static frame), so marking individual cells is pointless — Layer-1
// erases the whole screen unconditionally. Do not add `burn()`: it would be
// dead_code.

/// Phase index/kind by normalized time (0..=1).
/// Boundary convention: [lo, hi); phase_at(0.18) → Collapse, phase_at(0.50)
/// → Line, etc. At t == 1.0 → Flash.
pub fn phase_at(t01: f32) -> Phase {
    let t = t01.clamp(0.0, 1.0);
    if t < PHASE_STATIC_END {
        Phase::Static
    } else if t < PHASE_COLLAPSE_END {
        Phase::Collapse
    } else if t < PHASE_LINE_END {
        Phase::Line
    } else if t < PHASE_DOT_END {
        Phase::Dot
    } else {
        Phase::Flash
    }
}

/// Collapse easing factor: (1-p)^0.5. Holds the full size for most of the
/// phase, then snaps shut rapidly near the end (spec §3.6). At p == 1.0 it
/// yields 0, which the caller then clamps up to 1.
fn ease_hold_then_snap(p: f32) -> f32 {
    (1.0_f32 - p.clamp(0.0, 1.0)).max(0.0).sqrt()
}

/// Apply easing to a full size → current size, no less than 1.
fn ease_size(p: f32, full: usize) -> usize {
    let raw = (full as f32 * ease_hold_then_snap(p)).round() as usize;
    raw.max(1)
}

/// Active height of the vertical band during the Collapse phase (spec §4).
/// Monotonically non-increasing in p; at p == 0 → full, at p == 1 → 1.
pub fn collapse_height(p: f32, full: usize) -> usize {
    ease_size(p, full)
}

/// Length of the horizontal line during the Dot phase (spec §4). Semantically
/// identical to collapse_height — split out as a separate name for readability
/// at the call site.
pub fn line_width(p: f32, full: usize) -> usize {
    ease_size(p, full)
}

/// Rows of the active vertical band of height `h` centered on `cy`. Returns
/// the half-open range [top, top+h) centered on cy (integer division:
/// top = cy - h/2, saturating at 0). Resolves the even-h ambiguity uniformly
/// — the lower edge shifts up. The caller clamps the upper bound to `rows`
/// in run().
pub fn collapse_band(cy: usize, h: usize) -> (usize, usize) {
    let top = cy.saturating_sub(h / 2);
    (top, top + h)
}

/// Weighted noise glyph table (spec §3.5): ' '×2, '░'×3, '▒'×3, '▓'×2, '█'×2.
/// Length = sum of weights = 12. Order does not matter for the visual effect
/// but is fixed for testability.
const GLYPH_TABLE: &[char] = &[
    ' ', ' ',
    '░', '░', '░',
    '▒', '▒', '▒',
    '▓', '▓',
    '█', '█',
];
const GLYPH_TABLE_LEN: usize = GLYPH_TABLE.len();

/// Noise symbol by index into the weighted table (spec §4). Safe for any idx:
/// taken modulo the length, so the inclusive Rng::range (see R3) can never go
/// out of bounds. For idx ∈ [0, GLYPH_TABLE_LEN) it is equivalent to a direct
/// index — which is exactly what the test checks.
pub fn glyph_at(idx: usize) -> char {
    GLYPH_TABLE[idx % GLYPH_TABLE_LEN]
}

/// Perceived luminance per ITU-R BT.601: 0.299·R + 0.587·G + 0.114·B.
/// Pure function — only used inside brightest().
fn luminance(c: (u8, u8, u8)) -> f32 {
    0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32
}

/// Brightest palette step by perceived luminance (spec §3.3/§4). This is the
/// color of the final Flash. Works for any --from/--to: for the default fire
/// palette (dark→bright) it coincides with palette[36]; for the inverted
/// --from "#00f0ff" --to "#002080" it is the cyan end (index 0). NOT a
/// palette[36] hardcode.
pub fn brightest(palette: &Palette) -> (u8, u8, u8) {
    let mut best = palette[0];
    let mut best_lum = luminance(palette[0]);
    for &c in palette.iter().skip(1) {
        let l = luminance(c);
        if l > best_lum {
            best = c;
            best_lum = l;
        }
    }
    best
}

/// One noise glyph: symbol + color (spec §3.5). The caller never stamps
/// spaces (they leave the erase layer visible as a noise hole), so only the
/// actually-drawn glyphs need a meaningful color: '█' → pure white, '░▒▓' →
/// equal-channel gray in 170..=240. The choice itself is random and not under
/// test, but it leans on the deterministic glyph_at (spec §4).
fn static_glyph(rng: &mut Rng) -> (char, (u8, u8, u8)) {
    // Inclusive Rng::range may return GLYPH_TABLE_LEN; glyph_at takes it
    // modulo the length, so it is always safe (see R3).
    let idx = rng.range(0, GLYPH_TABLE_LEN as i32) as usize;
    let ch = glyph_at(idx);
    // Spaces are never stamped by the caller, so their color is unobserved —
    // collapse into a single gray arm instead of carrying a dead ' ' branch.
    // '█' is the only special case.
    let color = if ch == '█' {
        (255, 255, 255)
    } else {
        let v = rng.range(170, 240) as u8;
        (v, v, v)
    };
    (ch, color)
}

/// Render the overlay grid into a single String followed by write_all (as in
/// ufo.rs). None cells are skipped — the original terminal text would show
/// through there (but in CRT the whole screen is marked burned after Static,
/// so the erase pass in run() blanks them first).
fn render(buf: &mut String, grid: &[Option<Ov>], cols: usize, rows: usize) {
    use std::fmt::Write as _;
    buf.clear();
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

// ── Main loop ──────────────────────────────────────────────────────────

pub fn run(palette: &Palette, settings: &AnimSettings, interrupted: Arc<AtomicBool>) {
    let (mut cols, mut rows) = terminal_size();
    // Too small to bother — main() does the final clear anyway.
    if cols < 8 || rows < 4 {
        return;
    }
    let fps = settings.fps.max(1) as u64;
    let frame_delay = Duration::from_millis(1000 / fps);
    // CRT clamps the duration from below to 0.5 s (new for CRT, spec §7) —
    // otherwise phases could collapse to <1 frame. engine::burn and ufo::run
    // do not do this.
    let t = Duration::from_secs_f32(settings.duration.max(0.5));

    let mut rng = Rng::new();
    // burned is always all-true: from the very first frame (p=0 ⇒ Static
    // phase) the whole screen belongs to the effect — noise fully replaces
    // the signal (invariant §3.2). So Layer-1 below erases the whole screen
    // every frame, and the Layer-2 active zone is drawn on top. Never reset
    // this to vec![false] (including on resize).
    let mut burned = vec![true; cols * rows];
    let mut grid: Vec<Option<Ov>> = vec![None; cols * rows];
    let mut buf = String::with_capacity(cols * rows * 6);

    // Brightest palette step — precomputed once and reused by the Flash
    // branch every frame, instead of re-scanning all 37 steps per frame.
    let flash_base = brightest(palette);

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

        // Live resize. burned is always recreated as vec![true] (invariant
        // §3.2/§7) — otherwise the original text would leak back in outside
        // the active band.
        let (nc, nr) = terminal_size();
        if nc != cols || nr != rows {
            cols = nc;
            rows = nr;
            if cols < 8 || rows < 4 {
                break;
            }
            burned = vec![true; cols * rows];
            grid = vec![None; cols * rows];
            buf.reserve(cols * rows * 6);
        }
        let (ci, ri) = (cols as i32, rows as i32);

        // Reset the overlay for this frame.
        for cell in grid.iter_mut() {
            *cell = None;
        }

        // Normalized frame time.
        let p = (elapsed.as_secs_f32() / t.as_secs_f32()).clamp(0.0, 1.0);
        let phase = phase_at(p);
        let cy = rows / 2;
        let cx = cols / 2;

        // Layer 1: erase — blank out the whole screen (burned is always
        // all-true, invariant §3.2). The Layer-2 active zone redraws on top;
        // the ' ' holes in Static noise stay as erase spaces (the text under
        // them never shows through).
        for y in 0..ri {
            for x in 0..ci {
                if burned[(y as usize) * cols + (x as usize)] {
                    stamp(&mut grid, ci, ri, x, y, Ov { ch: ' ', color: None });
                }
            }
        }

        // Layer 2: the active zone of the current phase (drawn over the erase).
        match phase {
            Phase::Static => {
                // burned is already all-true — no marking needed. Draw dense
                // flickering noise; ' ' holes are not stamped and stay as the
                // erase space.
                for y in 0..ri {
                    for x in 0..ci {
                        let (ch, color) = static_glyph(&mut rng);
                        if ch != ' ' {
                            stamp(&mut grid, ci, ri, x, y, Ov { ch, color: Some(color) });
                        }
                    }
                }
            }
            Phase::Collapse => {
                // Local phase progress: 0 at the 0.18 boundary, 1 at the 0.50 boundary.
                let pp = ((p - PHASE_STATIC_END) / (PHASE_COLLAPSE_END - PHASE_STATIC_END))
                    .clamp(0.0, 1.0);
                let h = collapse_height(pp, rows);
                let (top, bot) = collapse_band(cy, h);
                let bot = bot.min(rows);
                for y in top..bot {
                    for x in 0..ci {
                        let (ch, color) = static_glyph(&mut rng);
                        if ch != ' ' {
                            stamp(&mut grid, ci, ri, x, y as i32, Ov { ch, color: Some(color) });
                        }
                    }
                }
            }
            Phase::Line => {
                // One central row — a solid bright white line.
                for x in 0..ci {
                    stamp(&mut grid, ci, ri, x, cy as i32, Ov { ch: '█', color: Some((255, 255, 255)) });
                }
            }
            Phase::Dot => {
                // The line contracts horizontally toward the center.
                let pp = ((p - PHASE_LINE_END) / (PHASE_DOT_END - PHASE_LINE_END))
                    .clamp(0.0, 1.0);
                let w = line_width(pp, cols);
                let left = cx.saturating_sub(w / 2);
                let right = (left + w).min(cols);
                for x in left..right {
                    stamp(&mut grid, ci, ri, x as i32, cy as i32, Ov { ch: '█', color: Some((255, 255, 255)) });
                }
            }
            Phase::Flash => {
                // Cell (cx,cy): a short white flash → flash_base (brightest
                // palette step) → linear brightness fade to zero. The fade is
                // renormalized onto the visible segment (spec §5): the first
                // 20% is pure white, then flash_base decays from full
                // intensity at pp=0.2 down to 0 at pp=1.0 — so the brightest
                // color is actually shown at full strength instead of 0.8,
                // and the white→base transition is continuous.
                let pp = ((p - PHASE_DOT_END) / (1.0 - PHASE_DOT_END)).clamp(0.0, 1.0);
                let c = if pp < 0.2 {
                    (255, 255, 255)
                } else {
                    let k = (1.0 - (pp - 0.2) / 0.8).clamp(0.0, 1.0);
                    (
                        (flash_base.0 as f32 * k).round() as u8,
                        (flash_base.1 as f32 * k).round() as u8,
                        (flash_base.2 as f32 * k).round() as u8,
                    )
                };
                stamp(&mut grid, ci, ri, cx as i32, cy as i32, Ov { ch: '█', color: Some(c) });
            }
        }

        render(&mut buf, &grid, cols, rows);
        let _ = out.write_all(buf.as_bytes());
        let _ = out.flush();
        std::thread::sleep(frame_delay);
    }

    // Always restore the cursor — main() does the final clear.
    let _ = write!(out, "{ESC}[?25h");
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_boundaries() {
        // [lo, hi) convention: the boundary belongs to the next phase.
        assert_eq!(phase_at(0.0), Phase::Static);
        assert_eq!(phase_at(0.17), Phase::Static);
        assert_eq!(phase_at(0.18), Phase::Collapse);
        assert_eq!(phase_at(0.499), Phase::Collapse);
        assert_eq!(phase_at(0.50), Phase::Line);
        assert_eq!(phase_at(0.579), Phase::Line);
        assert_eq!(phase_at(0.58), Phase::Dot);
        assert_eq!(phase_at(0.779), Phase::Dot);
        assert_eq!(phase_at(0.78), Phase::Flash);
        assert_eq!(phase_at(0.999), Phase::Flash);
        // Flash includes t == 1.0 (special case, not Static).
        assert_eq!(phase_at(1.0), Phase::Flash);
    }

    #[test]
    fn phase_clamps_out_of_range() {
        // Values outside [0,1] must not panic.
        assert_eq!(phase_at(-0.5), Phase::Static);
        assert_eq!(phase_at(1.5), Phase::Flash);
    }

    #[test]
    fn phase_monotonic() {
        // The phase sequence never jumps backward as t grows.
        fn order(p: Phase) -> u8 {
            match p {
                Phase::Static => 0,
                Phase::Collapse => 1,
                Phase::Line => 2,
                Phase::Dot => 3,
                Phase::Flash => 4,
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
    fn collapse_full_then_one() {
        assert_eq!(collapse_height(0.0, 24), 24);
        assert_eq!(collapse_height(1.0, 24), 1);
    }

    #[test]
    fn collapse_decreasing() {
        // Monotonically non-increasing in p — endpoints + overall trend (the
        // exact formula is not pinned down).
        let mut prev = collapse_height(0.0, 24);
        for i in 1..=100 {
            let p = i as f32 / 100.0;
            let h = collapse_height(p, 24);
            assert!(h <= prev, "collapse_height increased at p={p}: {h} > {prev}");
            prev = h;
        }
        assert_eq!(prev, 1);
    }

    #[test]
    fn collapse_band_centered() {
        // On a 24-row screen, center cy=12, h=4 → top=10, range [10,14).
        let (top, bot) = collapse_band(12, 4);
        assert_eq!(bot - top, 4, "height must equal h");
        assert!(top <= 12 && 12 < bot, "range must contain cy");
        assert_eq!(top, 10);
    }

    #[test]
    fn collapse_band_saturates_near_top() {
        // cy smaller than h/2 → top saturates at 0; the height is preserved.
        let (top, bot) = collapse_band(1, 4);
        assert_eq!(top, 0);
        assert_eq!(bot - top, 4);
    }

    #[test]
    fn line_full_then_one() {
        assert_eq!(line_width(0.0, 80), 80);
        assert_eq!(line_width(1.0, 80), 1);
    }

    #[test]
    fn line_decreasing() {
        let mut prev = line_width(0.0, 80);
        for i in 1..=100 {
            let p = i as f32 / 100.0;
            let w = line_width(p, 80);
            assert!(w <= prev, "line_width increased at p={p}: {w} > {prev}");
            prev = w;
        }
        assert_eq!(prev, 1);
    }

    #[test]
    fn easing_holds_then_snaps() {
        // At p=0.5 the height is still ~0.71·full; at p=0.9 ~0.32·full (§3.6).
        let h50 = collapse_height(0.5, 100) as f32 / 100.0;
        let h90 = collapse_height(0.9, 100) as f32 / 100.0;
        assert!(h50 > 0.65 && h50 < 0.78, "expected ~0.71 at p=0.5, got {h50}");
        assert!(h90 > 0.25 && h90 < 0.40, "expected ~0.32 at p=0.9, got {h90}");
    }

    #[test]
    fn glyph_table_valid() {
        // Every index in [0, LEN) maps into the allowed glyph set.
        let allowed = [' ', '░', '▒', '▓', '█'];
        for i in 0..GLYPH_TABLE_LEN {
            let ch = glyph_at(i);
            assert!(allowed.contains(&ch), "unexpected glyph {ch:?} at index {i}");
        }
        // Every glyph from the allowed set is present in the table.
        for &ch in &allowed {
            assert!(
                (0..GLYPH_TABLE_LEN).any(|i| glyph_at(i) == ch),
                "glyph {ch:?} missing from table"
            );
        }
        // Minimum weight ≥ 2 (spec §3.5 sets the minimum weight to 2) → every
        // glyph appears at least twice.
        for &ch in &allowed {
            let count = (0..GLYPH_TABLE_LEN).filter(|&i| glyph_at(i) == ch).count();
            assert!(count >= 2, "glyph {ch:?} has weight {count}, expected ≥ 2");
        }
    }

    #[test]
    fn glyph_table_len_is_twelve() {
        // Sum of weights: 2+3+3+2+2 = 12 (spec §3.5).
        assert_eq!(GLYPH_TABLE_LEN, 12);
    }

    #[test]
    fn glyph_at_wraps_safely() {
        // Out-of-range indices are safe — they wrap modulo the length.
        let allowed = [' ', '░', '▒', '▓', '█'];
        for i in [GLYPH_TABLE_LEN, GLYPH_TABLE_LEN + 1, 1_000_000] {
            assert!(allowed.contains(&glyph_at(i)));
        }
    }

    #[test]
    fn brightest_is_max_luminance() {
        // A palette where step 5 is brighter than the rest by luminance.
        let mut pal = [(0u8, 0u8, 0u8); 37];
        pal[5] = (200, 250, 100);
        pal[36] = (255, 0, 0); // red — brighter channel-wise, but lower luminance
        let b = brightest(&pal);
        assert_eq!(b, (200, 250, 100));
        // Explicit check: the brightest's luminance is ≥ every other step.
        let lb = luminance(b);
        for &c in pal.iter() {
            assert!(lb + 0.001 >= luminance(c), "{c:?} brighter than {b:?}");
        }
    }

    #[test]
    fn brightest_picks_low_index_when_inverted() {
        // Simulating --from "#00f0ff" --to "#002080" (spec §10): the palette
        // runs from bright cyan to dark blue — brightest must be at index 0.
        let mut pal = [(0u8, 0u8, 0u8); 37];
        pal[0] = (0, 240, 255); // cyan
        pal[36] = (0, 32, 128); // dark blue
        // Intermediate steps are linearly interpolated — all dimmer than pal[0].
        for (i, slot) in pal.iter_mut().enumerate().skip(1).take(35) {
            let t = i as f32 / 36.0;
            *slot = (
                ((1.0 - t) * 0.0 + t * 0.0) as u8,
                ((1.0 - t) * 240.0 + t * 32.0) as u8,
                ((1.0 - t) * 255.0 + t * 128.0) as u8,
            );
        }
        let b = brightest(&pal);
        assert_eq!(b, pal[0], "brightest must be the cyan end, not palette[36]");
    }

    #[test]
    fn brightest_matches_index36_for_default_fire_like() {
        // For the default fire palette (dark→bright) brightest coincides with palette[36].
        let mut pal = [(0u8, 0u8, 0u8); 37];
        for (i, slot) in pal.iter_mut().enumerate() {
            let v = (i as u8) * 7; // grows with the index
            *slot = (v, v / 2, 0);
        }
        let b = brightest(&pal);
        assert_eq!(b, pal[36]);
    }
}
