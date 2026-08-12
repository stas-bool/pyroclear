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
}
