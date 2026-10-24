//! Звуки зажатия: тон, который растёт, пока держат «сон», «перезагрузку» или
//! «выключение», звон на подтверждение и короткий спад на отмену.
//!
//! **Файлов в репозитории нет** — волны считаются здесь же (синусы с
//! огибающей) и один раз за запуск выкладываются WAV-ами в XDG_RUNTIME_DIR.
//! Тон зажатия обязан длиться ровно столько, сколько держать кнопку
//! ([`crate::зажим::ДЕРЖАТЬ`]): заливка кнопки и высота тона доходят до
//! верха вместе, и записанный заранее файл с этим разъехался бы при первой
//! же правке длительности.
//!
//! **Проигрывает** тот же `pw-play`/`paplay`, что и тон уведомления (см.
//! notify.rs): своего микшера у композитора нет. Тон зажатия — отдельный
//! процесс, и отпущенную раньше времени кнопку слышно сразу: процесс
//! убивается, вместо него звучит спад.

use std::io::Write;
use std::path::PathBuf;
use std::process::Child;

use crate::notify::Проигрыватель;
use crate::tray::PowerAction;

const ЧАСТОТА: u32 = 48_000;
/// Громкость проигрывателя. Звуки короткие и тихие сами по себе (пик волны
/// ниже половины шкалы), поэтому громче тона уведомления (0.35).
const ГРОМКОСТЬ: f32 = 0.55;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Звук {
    /// Растущий тон, пока кнопку держат.
    Зажим,
    /// Додержали: у каждого действия своя короткая фраза — сон уходит вниз
    /// мягко, перезагрузка вверх, выключение спускается на три ноты.
    Готово(PowerAction),
    /// Отпустили раньше времени.
    Отмена,
}

impl Звук {
    fn имя(self) -> &'static str {
        match self {
            Звук::Зажим => "plx-hold.wav",
            Звук::Готово(PowerAction::Sleep) => "plx-hold-sleep.wav",
            Звук::Готово(PowerAction::Reboot) => "plx-hold-reboot.wav",
            Звук::Готово(PowerAction::Off) => "plx-hold-off.wav",
            Звук::Отмена => "plx-hold-cancel.wav",
        }
    }

    const ВСЕ: [Звук; 5] = [
        Звук::Зажим,
        Звук::Готово(PowerAction::Sleep),
        Звук::Готово(PowerAction::Reboot),
        Звук::Готово(PowerAction::Off),
        Звук::Отмена,
    ];
}

// ── Волны ────────────────────────────────────────────────────────────────────

/// Тон зажатия длиной `сек`: высота едет вверх по экспоненте (на слух — ровно),
/// громкость нарастает, а лёгкая пульсация ускоряется к концу — «заряжается».
pub fn волна_зажима(сек: f64) -> Vec<f32> {
    let n = (сек * ЧАСТОТА as f64) as usize;
    let (f0, f1) = (262.0_f64, 622.0_f64);
    let mut фаза = 0.0_f64;
    let mut пульс = 0.0_f64;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / ЧАСТОТА as f64;
        let доля = (t / сек).min(1.0);
        let f = f0 * (f1 / f0).powf(доля);
        фаза += std::f64::consts::TAU * f / ЧАСТОТА as f64;
        // Пульсация с 5 до 14 Гц.
        пульс += std::f64::consts::TAU * (5.0 + 9.0 * доля) / ЧАСТОТА as f64;
        let тембр = фаза.sin() + 0.32 * (2.0 * фаза).sin() + 0.10 * (3.0 * фаза).sin();
        let дрожь = 1.0 - 0.18 * (0.5 + 0.5 * пульс.sin());
        let громкость = 0.30 + 0.25 * доля;
        out.push((тембр / 1.42 * дрожь * громкость * огибающая(t, сек, 0.03, 0.02)) as f32);
    }
    out
}

/// Колокольчик: основной тон и слегка расстроенная октава, затухание по
/// экспоненте.
fn нота(out: &mut [f32], с: f64, f: f64, громкость: f64, тау: f64) {
    let начало = (с * ЧАСТОТА as f64) as usize;
    for (i, s) in out.iter_mut().enumerate().skip(начало) {
        let t = (i - начало) as f64 / ЧАСТОТА as f64;
        let атака = (t / 0.004).min(1.0);
        let тело = (std::f64::consts::TAU * f * t).sin()
            + 0.22 * (std::f64::consts::TAU * f * 2.006 * t).sin()
            + 0.06 * (std::f64::consts::TAU * f * 3.01 * t).sin();
        *s += (тело / 1.28 * громкость * атака * (-t / тау).exp()) as f32;
    }
}

pub fn волна_готово(действие: PowerAction) -> Vec<f32> {
    let мелодия: &[(f64, f64)] = match действие {
        // A5 → E5: вниз и тише — засыпает.
        PowerAction::Sleep => &[(0.0, 880.0), (0.11, 659.3)],
        // E5 → B5: вверх — сейчас вернётся.
        PowerAction::Reboot => &[(0.0, 659.3), (0.09, 987.8)],
        // G5 → C5 → G4: спускается и гаснет.
        PowerAction::Off => &[(0.0, 784.0), (0.10, 523.3), (0.20, 392.0)],
    };
    let длина = мелодия.last().map(|(с, _)| с).unwrap_or(&0.0) + 0.75;
    let mut out = vec![0.0_f32; (длина * ЧАСТОТА as f64) as usize];
    let громкость = if действие == PowerAction::Sleep { 0.32 } else { 0.40 };
    for (i, (с, f)) in мелодия.iter().enumerate() {
        // Последняя нота звучит дольше: на ней фраза и кончается.
        let тау = if i + 1 == мелодия.len() { 0.26 } else { 0.14 };
        нота(&mut out, *с, *f, громкость, тау);
    }
    let n = out.len();
    let сек = n as f64 / ЧАСТОТА as f64;
    for (i, s) in out.iter_mut().enumerate() {
        *s *= огибающая(i as f64 / ЧАСТОТА as f64, сек, 0.0, 0.03) as f32;
    }
    out
}

/// Спад: короткое «вниз», чтобы отпущенная кнопка не обрывалась тишиной.
pub fn волна_отмены() -> Vec<f32> {
    let сек = 0.18;
    let n = (сек * ЧАСТОТА as f64) as usize;
    let mut фаза = 0.0_f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / ЧАСТОТА as f64;
            let f = 480.0 * (300.0_f64 / 480.0).powf(t / сек);
            фаза += std::f64::consts::TAU * f / ЧАСТОТА as f64;
            let тело = фаза.sin() + 0.25 * (2.0 * фаза).sin();
            (тело / 1.25 * 0.30 * (-t / 0.07).exp() * огибающая(t, сек, 0.005, 0.02)) as f32
        })
        .collect()
}

/// Линейные подъём и спад по краям — без них начало и конец щёлкают.
fn огибающая(t: f64, сек: f64, подъём: f64, спад: f64) -> f64 {
    let a = if подъём > 0.0 { (t / подъём).min(1.0) } else { 1.0 };
    let b = if спад > 0.0 { ((сек - t) / спад).clamp(0.0, 1.0) } else { 1.0 };
    a * b
}

/// 16-битный моно WAV.
pub fn wav(волна: &[f32]) -> Vec<u8> {
    let данных = (волна.len() * 2) as u32;
    let mut b = Vec::with_capacity(44 + данных as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + данных).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes()); // PCM
    b.extend_from_slice(&1u16.to_le_bytes()); // моно
    b.extend_from_slice(&ЧАСТОТА.to_le_bytes());
    b.extend_from_slice(&(ЧАСТОТА * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&данных.to_le_bytes());
    for s in волна {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes());
    }
    b
}

// ── Проигрывание ─────────────────────────────────────────────────────────────

/// Проигрыватель и выложенные файлы. Готовится при первом звуке, а не на
/// старте: зажатие случается раз в день, и считать волны каждому запуску
/// незачем.
#[derive(Default)]
pub struct Звуки {
    /// None — ещё не пробовали; Some(None) — пробовали, звука не будет.
    готово: Option<Option<(Проигрыватель, PathBuf)>>,
    /// Тон зажатия — его обрывают, если кнопку отпустили.
    зажим: Option<Child>,
    /// Остальные: ждём их только затем, чтобы не копить зомби.
    прочие: Vec<Child>,
}

impl Звуки {
    fn приготовить(&mut self) -> Option<(Проигрыватель, PathBuf)> {
        if let Some(г) = &self.готово {
            return г.clone();
        }
        let г = (|| {
            let п = Проигрыватель::найти()?;
            let каталог = std::env::var("XDG_RUNTIME_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir());
            for з in Звук::ВСЕ {
                let волна = match з {
                    Звук::Зажим => волна_зажима(crate::зажим::ДЕРЖАТЬ.as_secs_f64()),
                    Звук::Готово(д) => волна_готово(д),
                    Звук::Отмена => волна_отмены(),
                };
                let путь = каталог.join(з.имя());
                if let Err(e) = std::fs::File::create(&путь).and_then(|mut f| f.write_all(&wav(&волна))) {
                    tracing::warn!("plx/sounds: could not write {:?}: {}", путь, e);
                    return None;
                }
            }
            Some((п, каталог))
        })();
        if г.is_none() {
            tracing::warn!("plx/sounds: no pw-play/paplay or no place for the files — hold is silent");
        }
        self.готово = Some(г.clone());
        г
    }

    pub fn играть(&mut self, з: Звук) {
        if !crate::bar::вид().sounds {
            return;
        }
        let Some((п, каталог)) = self.приготовить() else { return };
        if з == Звук::Зажим {
            self.оборвать();
        }
        match п.команда(&каталог.join(з.имя()), ГРОМКОСТЬ).spawn() {
            Ok(c) if з == Звук::Зажим => self.зажим = Some(c),
            Ok(c) => self.прочие.push(c),
            Err(e) => tracing::warn!("plx/sounds: {}: {}", п.имя(), e),
        }
    }

    /// Оборвать тон зажатия (кнопку отпустили).
    pub fn оборвать(&mut self) {
        if let Some(mut c) = self.зажим.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }

    /// Прибрать доигравшие процессы. Зовётся из тика зажима.
    pub fn прибрать(&mut self) {
        self.прочие.retain_mut(|c| matches!(c.try_wait(), Ok(None)));
        if self.зажим.as_mut().is_some_and(|c| !matches!(c.try_wait(), Ok(None))) {
            self.зажим = None;
        }
    }
}

#[cfg(test)]
mod тесты {
    use super::*;

    /// Частота по числу переходов через ноль на отрезке.
    fn частота(в: &[f32], с: f64, по: f64) -> f64 {
        let (a, b) = ((с * ЧАСТОТА as f64) as usize, (по * ЧАСТОТА as f64) as usize);
        let нулей = в[a..b].windows(2).filter(|w| w[0] < 0.0 && w[1] >= 0.0).count();
        нулей as f64 / (по - с)
    }

    #[test]
    fn тон_зажатия_растёт_и_длится_сколько_держать() {
        let в = волна_зажима(1.0);
        assert_eq!(в.len(), ЧАСТОТА as usize);
        let низ = частота(&в, 0.05, 0.15);
        let верх = частота(&в, 0.85, 0.95);
        assert!(низ < 320.0 && верх > 520.0, "тон не растёт: {низ:.0} → {верх:.0} Гц");
        // Края без щелчка: первые и последние отсчёты у нуля.
        assert!(в[0].abs() < 1e-3 && в[в.len() - 1].abs() < 1e-2);
        assert!(в.iter().all(|s| s.abs() <= 1.0), "перегруз");
    }

    #[test]
    fn звоны_не_перегружают() {
        for д in [PowerAction::Sleep, PowerAction::Reboot, PowerAction::Off] {
            let в = волна_готово(д);
            let пик = в.iter().fold(0.0_f32, |м, s| м.max(s.abs()));
            assert!(пик > 0.1 && пик < 0.9, "{д:?}: пик {пик}");
        }
        let пик = волна_отмены().iter().fold(0.0_f32, |м, s| м.max(s.abs()));
        assert!(пик > 0.05 && пик < 0.9);
    }

    #[test]
    fn заголовок_wav() {
        let b = wav(&[0.0, 0.5, -0.5]);
        assert_eq!(&b[0..4], b"RIFF");
        assert_eq!(&b[8..16], b"WAVEfmt ");
        assert_eq!(b.len(), 44 + 6);
        assert_eq!(u32::from_le_bytes(b[40..44].try_into().unwrap()), 6);
    }
}
