//! Синтез видеорежима, которого нет в списке коннектора (VESA CVT 1.2).
//!
//! Зачем это вообще нужно. Встроенная панель отдаёт РОВНО один режим — свой
//! родной (у Ярика 3840x2160@60, `cat /sys/class/drm/card1-eDP-1/modes` — одна
//! строка). Никакого 1920x1080 в списке нет и не будет: eDP-панель это матрица
//! с фиксированными таймингами, EDID перечисляет её же.
//!
//! Отсюда прежний тупик: `monitor{ width = 1920, height = 1080 }` не находил
//! совпадения, писал warn и возвращался на 4K, а «FullHD» приходилось изображать
//! масштабом выхода (`scale = 2.0`) — то есть КОМПОЗИТИТЬ и СКАНИРОВАТЬ всё
//! равно 4K, просто с вдвое более крупным интерфейсом. Ноутбучной видеокарте от
//! этого не легче ни на ватт: пикселей столько же.
//!
//! Выход — тот же, которым пользуются `xrandr --newmode/--addmode` и
//! `wlr_output_set_custom_mode`: режим не обязан приходить из EDID, его можно
//! построить самим и отдать ядру. Дальше работает панельный скейлер (pipe
//! scaler) — блок в самом дисплейном контроллере: CRTC гонит 1920x1080, железо
//! растягивает картинку до физических 3840x2160 уже за пределами композитора.
//! Для i915 это штатный путь (`intel_panel_fitting`), 1080p→2160p к тому же
//! целое двукратное увеличение — каждый пиксель ровно 2x2, без «мыла» от
//! дробной интерполяции.
//!
//! Тайминги строим по CVT, потому что произвольные числа ядро отвергнет:
//! `drm_mode_validate_basic` требует согласованных hsync/vsync/htotal, а
//! `intel_mode_valid` — вменяемой пиксельной частоты. Для eDP реальные тайминги
//! всё равно подменятся панельными (CRTC работает на частоте матрицы), но
//! режим обязан пройти проверку, и для внешнего монитора CVT — то, что он
//! ожидает увидеть.

use smithay::reexports::drm::control::{Mode, ModeFlags, ModeTypeFlags};

/// Минимальное время «vsync + задний порог», мкс (CVT_MIN_VSYNC_BP).
const MIN_VSYNC_BP: f64 = 550.0;
/// Минимальный передний порог по вертикали, строк.
const MIN_V_PORCH: u32 = 3;
/// Гранулярность по горизонтали: всё считается «ячейками» по 8 пикселей.
const H_GRANULARITY: f64 = 8.0;
/// Пиксельную частоту округляем вниз до этого шага, кГц.
const CLOCK_STEP: u32 = 250;
/// Коэффициенты формулы бланкинга CVT: C' = ((C−J)·K/256)+J, M' = K/256·M
/// при C = 40, J = 20, K = 128, M = 600.
const C_PRIME: f64 = 30.0;
const M_PRIME: f64 = 300.0;
/// Доля строки, отводимая под строчный синхроимпульс.
const HSYNC_PERCENT: f64 = 8.0;

/// Ширина кадрового синхроимпульса — по стандарту она зависит от соотношения
/// сторон, а не от разрешения (CVT, таблица 4-1).
fn vsync_width(hdisplay: u32, vdisplay: u32) -> u32 {
    let подходит = |w: u32, h: u32| vdisplay % h == 0 && vdisplay / h * w == hdisplay;
    if подходит(4, 3) {
        4
    } else if подходит(16, 9) {
        5
    } else if подходит(16, 10) {
        6
    } else if подходит(5, 4) || подходит(15, 9) {
        7
    } else {
        // Нестандартное соотношение: стандарт велит брать наибольшее значение.
        10
    }
}

/// Построить режим `width`x`height`@`refresh` по CVT 1.2 (без reduced blanking).
///
/// Ширина округляется вниз до 8 пикселей — сетка, в которой считает весь
/// стандарт; для всех практических разрешений (1920, 1600, 1280…) это ничего
/// не меняет.
pub fn cvt(width: u16, height: u16, refresh: f64) -> Mode {
    let hdisplay = (width as u32 / 8) * 8;
    let vdisplay = height as u32;
    let refresh = refresh.max(1.0);
    let vsync = vsync_width(hdisplay, vdisplay);

    // Оценка длительности строки, мкс: из кадра вычитаем обязательный
    // «vsync + задний порог» и делим на строки вместе с передним порогом.
    let h_period = (1_000_000.0 / refresh - MIN_VSYNC_BP) / (vdisplay + MIN_V_PORCH) as f64;

    // Сколько строк уходит на синхроимпульс и задний порог.
    let v_sync_bp = ((MIN_VSYNC_BP / h_period) as u32 + 1).max(vsync + MIN_V_PORCH);

    let vtotal = vdisplay + v_sync_bp + MIN_V_PORCH;
    let vsync_start = vdisplay + MIN_V_PORCH;
    let vsync_end = vsync_start + vsync;

    // Идеальная доля бланкинга в строке — та самая формула CVT, ради которой
    // и существует стандарт: чем короче строка, тем больше её доля уходит на
    // гашение.
    let duty = (C_PRIME - M_PRIME * h_period / 1000.0).max(20.0);
    let mut h_blank = hdisplay as f64 * duty / (100.0 - duty);
    // Гашение выравниваем на ДВЕ ячейки: оно делится пополам между передним и
    // задним порогом, и каждая половина обязана остаться целой ячейкой.
    h_blank -= h_blank % (2.0 * H_GRANULARITY);

    let htotal = hdisplay + h_blank as u32;
    let hsync_end = hdisplay + h_blank as u32 / 2;
    let mut hsync = htotal as f64 / 100.0 * HSYNC_PERCENT;
    hsync -= hsync % H_GRANULARITY;
    let hsync_start = hsync_end - hsync as u32;

    // Частота в кГц, округлённая вниз до шага сетки (как это делает cvt(1)).
    let clock = (htotal as f64 * 1000.0 / h_period) as u32 / CLOCK_STEP * CLOCK_STEP;

    let mut raw: drm_ffi::drm_mode_modeinfo = unsafe { std::mem::zeroed() };
    raw.clock = clock;
    raw.hdisplay = hdisplay as u16;
    raw.hsync_start = hsync_start as u16;
    raw.hsync_end = hsync_end as u16;
    raw.htotal = htotal as u16;
    raw.vdisplay = vdisplay as u16;
    raw.vsync_start = vsync_start as u16;
    raw.vsync_end = vsync_end as u16;
    raw.vtotal = vtotal as u16;
    // Частота кадров считается из таймингов, а не из запрошенной: округление
    // пиксельной частоты до 250 кГц уводит её на доли герца, и в лог должно
    // попасть то, что железо реально покажет.
    raw.vrefresh = ((clock as f64 * 1000.0) / (htotal as f64 * vtotal as f64)).round() as u32;
    // Стандартный CVT — отрицательный строчный, положительный кадровый.
    raw.flags = (ModeFlags::NHSYNC | ModeFlags::PVSYNC).bits();
    // USERDEF: режим придуман нами, а не прочитан из EDID. Ядру этот флаг
    // безразличен, но в отладочных дампах сразу видно, чей режим.
    raw.type_ = ModeTypeFlags::USERDEF.bits();

    let имя = format!("{}x{}", hdisplay, vdisplay);
    for (место, байт) in raw.name.iter_mut().zip(имя.bytes()) {
        *место = байт as std::ffi::c_char;
    }

    Mode::from(raw)
}

#[cfg(test)]
mod tests {

    /// Эталон — вывод `cvt 1920 1080 60`:
    /// `173.00  1920 2048 2248 2576  1080 1083 1088 1120 -hsync +vsync`.
    /// Совпадение до последнего числа означает, что реализована именно CVT, а
    /// не «похожие тайминги»: ядро проверяет их согласованность, а внешний
    /// монитор — принимает или нет.
    #[test]
    fn cvt_1080p60_совпадает_с_эталоном() {
        let m = super::cvt(1920, 1080, 60.0);
        assert_eq!(m.clock(), 173_000);
        assert_eq!(m.size(), (1920, 1080));
        assert_eq!(m.hsync(), (2048, 2248, 2576));
        assert_eq!(m.vsync(), (1083, 1088, 1120));
        assert_eq!(m.vrefresh(), 60);
    }

    /// Нужен для внешних мониторов: 16:10 берёт другую ширину кадрового
    /// импульса (6 строк вместо 5), и перепутать их — значит выдать режим,
    /// который часть мониторов не примет. `cvt 1680 1050 60` даёт
    /// `146.25  1680 1784 1960 2240  1050 1053 1059 1089`.
    #[test]
    fn cvt_1680x1050_16на10() {
        let m = super::cvt(1680, 1050, 60.0);
        assert_eq!(m.clock(), 146_250);
        assert_eq!(m.hsync(), (1784, 1960, 2240));
        assert_eq!(m.vsync(), (1053, 1059, 1089));
    }

    /// Тайминги обязаны быть монотонными — это первое, что проверяет
    /// `drm_mode_validate_basic` в ядре, и на этом отваливается режим,
    /// собранный «на глаз».
    #[test]
    fn тайминги_монотонны() {
        for (w, h, r) in [(1920, 1080, 60.0), (1280, 720, 60.0), (2560, 1440, 120.0)] {
            let m = super::cvt(w, h, r);
            let (hs, he, ht) = m.hsync();
            let (vs, ve, vt) = m.vsync();
            assert!(m.size().0 < hs && hs < he && he <= ht, "строчные тайминги {m:?}");
            assert!(m.size().1 < vs && vs < ve && ve <= vt, "кадровые тайминги {m:?}");
            assert!(m.clock() > 0, "нулевая пиксельная частота {m:?}");
        }
    }
}
