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

// ── Overlay cell + grid-примитивы (как в ufo.rs) ──────────────────────

#[derive(Clone, Copy)]
struct Ov {
    ch: char,
    color: Option<(u8, u8, u8)>, // None ⇒ default fg/bg (используется для erase-пробела)
}

/// Поставить ячейку overlay в grid по координатам, с проверкой границ.
fn stamp(grid: &mut [Option<Ov>], cols: i32, rows: i32, x: i32, y: i32, ov: Ov) {
    if (0..cols).contains(&x) && (0..rows).contains(&y) {
        grid[(y as usize) * (cols as usize) + (x as usize)] = Some(ov);
    }
}

// Примечание: в отличие от ufo.rs, функции `burn()` здесь НЕТ. В CRT `burned`
// всегда all-true (инвариант §3.2: весь экран принадлежит эффекту с первого
// кадра Static), поэтому помечать отдельные ячейки не нужно — Layer-1 стирает
// весь экран безусловно. Добавлять `burn()` не надо: она стала бы dead_code.

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

/// Perceived luminance по ITU-R BT.601: 0.299·R + 0.587·G + 0.114·B.
/// Чистая функция — используется только в brightest().
fn luminance(c: (u8, u8, u8)) -> f32 {
    0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32
}

/// Самая яркая ступень палитры по perceived luminance (§3.3/§4). Это цвет
/// финальной вспышки (Flash). Универсальна для любого --from/--to: для дефолтной
/// огневой палитры (тёмный→яркий) совпадает с palette[36]; для инвертированной
/// --from "#00f0ff" --to "#002080" — с бирюзовым концом (индекс 0). NOT хардкод
/// palette[36].
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

/// Один глиф шума: символ + цвет (§3.5). ' ' возвращает (0,0,0) — вызывающий
/// код пропускает отрисовку пробела (дыры в шуме). Цвет: '█' → чистый белый;
/// '░▒▓' → серый с равными каналами в диапазоне 170..=240; ' ' → любой
/// (всё равно не рисуется). Сам выбор случаен и нетестируем, но опирается на
/// детерминированную glyph_at (§4).
fn static_glyph(rng: &mut Rng) -> (char, (u8, u8, u8)) {
    // Инклюзивный Rng::range может вернуть GLYPH_TABLE_LEN; glyph_at берёт по
    // модулю, поэтому безопасен (см. R3).
    let idx = rng.range(0, GLYPH_TABLE_LEN as i32) as usize;
    let ch = glyph_at(idx);
    let color = match ch {
        ' ' => (0, 0, 0),
        '█' => (255, 255, 255),
        _ => {
            let v = rng.range(170, 240) as u8;
            (v, v, v)
        }
    };
    (ch, color)
}

/// Рендер overlay-grid в один String с последующим write_all (как в ufo.rs).
/// None-ячейки пропускаются — на их месте остаётся оригинальный текст терминала
/// (но в CRT весь экран после Static помечен burned, поэтому erase в run()
/// сначала закроет их пробелом).
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
                last_color = None; // после move цвет нужно переизлучить
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

// ── Главный цикл ──────────────────────────────────────────────────────

pub fn run(palette: &Palette, settings: &AnimSettings, interrupted: Arc<AtomicBool>) {
    let (mut cols, mut rows) = terminal_size();
    // Слишком мелко — main() всё равно сделает final clear.
    if cols < 8 || rows < 4 {
        return;
    }
    let fps = settings.fps.max(1) as u64;
    let frame_delay = Duration::from_millis(1000 / fps);
    // CRT clamp'ит длительность снизу до 0.5 c (новое для CRT, §7) — иначе фазы
    // могут схлопнуться в <1 кадр. engine::burn и ufo::run этого не делают.
    let t = Duration::from_secs_f32(settings.duration.max(0.5));

    let mut rng = Rng::new();
    // burned всегда all-true: с первого кадра (p=0 ⇒ фаза Static) весь экран
    // принадлежит эффекту — помехи полностью заменяют сигнал (инвариант §3.2).
    // Поэтому Layer-1 ниже стирает весь экран каждый кадр, а активная зона Layer-2
    // перерисовывается поверх. Никогда не сбрасывать в vec![false] (в т.ч. ресайз).
    let mut burned = vec![true; cols * rows];
    let mut grid: Vec<Option<Ov>> = vec![None; cols * rows];
    let mut buf = String::with_capacity(cols * rows * 6);

    let stdout = io::stdout();
    let mut out = stdout.lock();
    let _ = write!(out, "{ESC}[?25l"); // спрятать курсор
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

        // Live resize. burned всегда пересоздаётся как vec![true] (инвариант
        // §3.2/§7) — иначе вне активной полосы всплывёт оригинальный текст.
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

        // Сброс overlay на этот кадр.
        for cell in grid.iter_mut() {
            *cell = None;
        }

        // Нормированное время кадра.
        let p = (elapsed.as_secs_f32() / t.as_secs_f32()).clamp(0.0, 1.0);
        let phase = phase_at(p);
        let cy = rows / 2;
        let cx = cols / 2;

        // Layer 1: erase — закрыть пробелом весь экран (burned всегда all-true,
        // инвариант §3.2). Активная зона Layer 2 перерисуется поверх; ' '-дыры
        // шума в Static остаются erase-пробелом (текст под ними не просвечивает).
        for y in 0..ri {
            for x in 0..ci {
                if burned[(y as usize) * cols + (x as usize)] {
                    stamp(&mut grid, ci, ri, x, y, Ov { ch: ' ', color: None });
                }
            }
        }

        // Layer 2: активная зона текущей фазы (рисуется поверх erase).
        match phase {
            Phase::Static => {
                // burned уже all-true — пометки не требуется. Рисуем плотный
                // мерцающий шум; ' '-дыры не stamp'аются и остаются erase-пробелом.
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
                // Локальный прогресс фазы: 0 на границе 0.18, 1 на границе 0.50.
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
                // Одна центральная строка — сплошная яркая белая линия.
                for x in 0..ci {
                    stamp(&mut grid, ci, ri, x, cy as i32, Ov { ch: '█', color: Some((255, 255, 255)) });
                }
            }
            Phase::Dot => {
                // Линия сжимается по горизонтали к центру.
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
                // Ячейка (cx,cy): короткая белая вспышка → brightest(palette) →
                // линейное затухание яркости до нуля.
                let pp = ((p - PHASE_DOT_END) / (1.0 - PHASE_DOT_END)).clamp(0.0, 1.0);
                let c = if pp < 0.2 {
                    (255, 255, 255)
                } else {
                    let base = brightest(palette);
                    let k = (1.0 - pp).max(0.0);
                    (
                        (base.0 as f32 * k).round() as u8,
                        (base.1 as f32 * k).round() as u8,
                        (base.2 as f32 * k).round() as u8,
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

    // Безусловное восстановление курсора — финальный clear делает main().
    let _ = write!(out, "{ESC}[?25h");
    let _ = out.flush();
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

    #[test]
    fn brightest_is_max_luminance() {
        // Палитра, где ступень 5 ярче остальных по luminance.
        let mut pal = [(0u8, 0u8, 0u8); 37];
        pal[5] = (200, 250, 100);
        pal[36] = (255, 0, 0); // красный — канально ярче, но luminance меньше
        let b = brightest(&pal);
        assert_eq!(b, (200, 250, 100));
        // Явная проверка: luminance brightest'а ≥ любой другой ступени.
        let lb = luminance(b);
        for &c in pal.iter() {
            assert!(lb + 0.001 >= luminance(c), "{c:?} brighter than {b:?}");
        }
    }

    #[test]
    fn brightest_picks_low_index_when_inverted() {
        // Симуляция --from "#00f0ff" --to "#002080" (§10): палитра от яркой
        // бирюзы к тёмному синему — brightest должен быть на индексе 0.
        let mut pal = [(0u8, 0u8, 0u8); 37];
        pal[0] = (0, 240, 255); // бирюза
        pal[36] = (0, 32, 128); // тёмно-синий
        // Промежуточные ступени линейно интерполированы — все тусклее pal[0].
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
        // Для дефолтной огневой палитры (тёмный→яркий) brightest совпадает с palette[36].
        let mut pal = [(0u8, 0u8, 0u8); 37];
        for (i, slot) in pal.iter_mut().enumerate() {
            let v = (i as u8) * 7; // растёт с индексом
            *slot = (v, v / 2, 0);
        }
        let b = brightest(&pal);
        assert_eq!(b, pal[36]);
    }
}
