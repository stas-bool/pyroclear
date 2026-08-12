// crt.rs — эффект выключения ЭЛТ-телевизора.
//
// Пять фаз анимации по нормированному времени t ∈ [0,1]:
//   Static → Collapse → Line → Dot → Flash
// (см. docs/superpowers/specs/2026-08-12-crt-tv-off-design.md, §2).
//
// Рендер-модель — как в ufo.rs: overlay-grid Vec<Option<Ov>> + маска burned,
// один write_all на кадр. Ключевое отличие: burned = vec![true; cols*rows]
// с первого кадра Static (помехи полностью заменяют сигнал), и не сбрасывается
// в false при ресайзе после Static — иначе вне полосы всплывёт оригинальный
// текст терминала.

use crate::engine::{terminal_size, Rng};
use crate::palettes::Palette;
use crate::{config::AnimSettings, ESC};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Фазы анимации CRT-выключения (§2 спеки). Порядок важен — monotonic по t.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Static,
    Collapse,
    Line,
    Dot,
    Flash,
}

/// Границы фаз как полуоткрытые интервалы [lo, hi) (§2). Flash включает t == 1.0.
const PHASE_STATIC_END: f32 = 0.18;
const PHASE_COLLAPSE_END: f32 = 0.50;
const PHASE_LINE_END: f32 = 0.58;
const PHASE_DOT_END: f32 = 0.78;
// PHASE_FLASH_END = 1.0 (неявно).

/// Индекс/тип фазы по нормированному времени (0..=1).
/// Конвенция границ: [lo, hi); phase_at(0.18) → Collapse, phase_at(0.50) → Line
/// и т.д. При t == 1.0 → Flash.
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

/// Easing-фактор для схлопывания: (1-p)^0.5. Удержание полного размера
/// большую часть фазы → стремительное смыкание к концу (§3.6). При p == 1.0
/// даёт 0, что потом clamp'ится до 1 в вызове.
fn ease_hold_then_snap(p: f32) -> f32 {
    (1.0_f32 - p.clamp(0.0, 1.0)).max(0.0).sqrt()
}

/// Применить easing к полному размеру → текущий размер, не менее 1.
fn ease_size(p: f32, full: usize) -> usize {
    let raw = (full as f32 * ease_hold_then_snap(p)).round() as usize;
    raw.max(1)
}

/// Активная высота вертикальной полосы на фазе Collapse (§4). Монотонно не
/// возрастает по p; на p == 0 → full, на p == 1 → 1.
pub fn collapse_height(p: f32, full: usize) -> usize {
    ease_size(p, full)
}

/// Длина горизонтальной линии на фазе Dot (§4). Семантически идентична
/// collapse_height — вынесено в отдельное имя для читаемости точки вызова.
pub fn line_width(p: f32, full: usize) -> usize {
    ease_size(p, full)
}

/// Строки активной вертикальной полосы высотой `h` вокруг центра `cy`.
/// Возвращает полуоткрытый диапазон [top, top+h), центрированный по cy
/// (целочисленное деление: top = cy - h/2, насыщаясь до 0). Решает
/// неоднозначность чётного h единообразно — нижняя граница сдвигается вверх.
/// Clamp сверху (до rows) делает вызывающий код в run().
pub fn collapse_band(cy: usize, h: usize) -> (usize, usize) {
    let top = cy.saturating_sub(h / 2);
    (top, top + h)
}

/// Взвешенная таблица глифов помех (§3.5): ' '×2, '░'×3, '▒'×3, '▓'×2, '█'×2.
/// Длина = сумма весов = 12. Порядок не важен для визуального эффекта, но
/// фиксируется для тестируемости.
const GLYPH_TABLE: &[char] = &[
    ' ', ' ',
    '░', '░', '░',
    '▒', '▒', '▒',
    '▓', '▓',
    '█', '█',
];
const GLYPH_TABLE_LEN: usize = GLYPH_TABLE.len();

/// Символ шума по индексу во взвешенной таблице (§4). Безопасен для любого
/// idx: берётся по модулю длины, чтобы инклюзивный Rng::range (см. R3) не
/// мог выйти за границы. Для idx ∈ [0, GLYPH_TABLE_LEN) эквивалентно прямому
/// индексу — это и проверяется в тесте.
pub fn glyph_at(idx: usize) -> char {
    GLYPH_TABLE[idx % GLYPH_TABLE_LEN]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_boundaries() {
        // Конвенция [lo, hi): граница принадлежит следующей фазе.
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
        // Flash включает t == 1.0 (особый случай, не Static).
        assert_eq!(phase_at(1.0), Phase::Flash);
    }

    #[test]
    fn phase_clamps_out_of_range() {
        // Выход за [0,1] не должен паниковать.
        assert_eq!(phase_at(-0.5), Phase::Static);
        assert_eq!(phase_at(1.5), Phase::Flash);
    }

    #[test]
    fn phase_monotonic() {
        // Последовательность фаз не «прыгает назад» при росте t.
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
        // Монотонно не возрастает по p — края + общий тренд (формулу не фиксируем).
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
        // На экране 24 строки, центр cy=12, h=4 → top=10, диапазон [10,14).
        let (top, bot) = collapse_band(12, 4);
        assert_eq!(bot - top, 4, "height must equal h");
        assert!(top <= 12 && 12 < bot, "range must contain cy");
        assert_eq!(top, 10);
    }

    #[test]
    fn collapse_band_saturates_near_top() {
        // cy меньше h/2 → top насыщается до 0, высота при этом сохраняется.
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
        // При p=0.5 высота ещё ~0.71·full; при p=0.9 — ~0.32·full (§3.6).
        let h50 = collapse_height(0.5, 100) as f32 / 100.0;
        let h90 = collapse_height(0.9, 100) as f32 / 100.0;
        assert!(h50 > 0.65 && h50 < 0.78, "expected ~0.71 at p=0.5, got {h50}");
        assert!(h90 > 0.25 && h90 < 0.40, "expected ~0.32 at p=0.9, got {h90}");
    }

    #[test]
    fn glyph_table_valid() {
        // Все индексы [0, LEN) отображаются в допустимое множество глифов.
        let allowed = [' ', '░', '▒', '▓', '█'];
        for i in 0..GLYPH_TABLE_LEN {
            let ch = glyph_at(i);
            assert!(allowed.contains(&ch), "unexpected glyph {ch:?} at index {i}");
        }
        // Каждый глиф из допустимого множества присутствует в таблице.
        for &ch in &allowed {
            assert!(
                (0..GLYPH_TABLE_LEN).any(|i| glyph_at(i) == ch),
                "glyph {ch:?} missing from table"
            );
        }
        // Минимальный вес ≥ 2 (спека §3.5 ставит минимальный вес 2) → каждый
        // глиф встречается хотя бы дважды.
        for &ch in &allowed {
            let count = (0..GLYPH_TABLE_LEN).filter(|&i| glyph_at(i) == ch).count();
            assert!(count >= 2, "glyph {ch:?} has weight {count}, expected ≥ 2");
        }
    }

    #[test]
    fn glyph_table_len_is_twelve() {
        // Сумма весов: 2+3+3+2+2 = 12 (§3.5).
        assert_eq!(GLYPH_TABLE_LEN, 12);
    }

    #[test]
    fn glyph_at_wraps_safely() {
        // Выход за границы таблицы безопасен — зацикливается по модулю.
        let allowed = [' ', '░', '▒', '▓', '█'];
        for i in [GLYPH_TABLE_LEN, GLYPH_TABLE_LEN + 1, 1_000_000] {
            assert!(allowed.contains(&glyph_at(i)));
        }
    }
}
