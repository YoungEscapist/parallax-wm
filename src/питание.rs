//! Питание ноутбука: режимы (как в GNOME/KDE), заряд, подсветка экрана и
//! клавиатуры — одно меню по клику на заряд в панели (или `power_menu`).
//!
//! **Режимы ведёт power-profiles-daemon**, ровно как у GNOME и KDE:
//! «Производительность / Сбалансированный / Энергосбережение» — это его
//! профили, а он уже сам пишет `platform_profile` и EPP процессора. Свою
//! запись в sysfs не заводили нарочно: она требует root, а демон и так есть в
//! портах и разрешения у него через polkit. На CRUX ноута поставлен 01.10.2026
//! (служба `power-profiles-daemon`, правило polkit для wheel — без logind
//! «активного сеанса» polkit не знает, и allow_active не срабатывает).
//!
//! **Подсветка** — прямо в `/sys/class/backlight/*` и `/sys/class/leds/*kbd*`.
//! Писать туда может только root; доступ группе video даёт правило udev
//! (`/etc/udev/rules.d/90-plx-backlight.rules`, тоже 01.10.2026). Нет прав —
//! меню так и пишет, а не молчит.
//!
//! Всё, что ходит в D-Bus и sysfs, живёт в своём потоке (как звук и вайфай):
//! главный цикл композитора не ждёт ни шины, ни диска.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel;

const СЛУЖБА: &str = "org.freedesktop.UPower.PowerProfiles";
const ПУТЬ: &str = "/org/freedesktop/UPower/PowerProfiles";
/// Старое имя той же службы (до 0.20); демон держит оба.
const СЛУЖБА_СТАРАЯ: &str = "net.hadess.PowerProfiles";
const ПУТЬ_СТАРЫЙ: &str = "/net/hadess/PowerProfiles";

/// Сколько держим сообщение внизу меню.
const СООБЩЕНИЕ_ЖИВЁТ: Duration = Duration::from_secs(4);
/// Как часто меню, пока открыто, перечитывает заряд и расход.
const ОПРОС: Duration = Duration::from_secs(3);

/// Профили в порядке строк меню — сверху «быстрее», как в GNOME.
pub const ПРОФИЛИ: [&str; 3] = ["performance", "balanced", "power-saver"];

pub fn имя_профиля(п: &str) -> &'static str {
    match п {
        "performance" => crate::т!("Производительность", "Performance"),
        "balanced" => crate::т!("Сбалансированный", "Balanced"),
        "power-saver" => crate::т!("Энергосбережение", "Power saver"),
        _ => "?",
    }
}

/// Одна подсветка: путь в sysfs, текущее и наибольшее значения.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Подсветка {
    pub путь: String,
    pub сейчас: u32,
    pub макс: u32,
    /// Можно ли писать (права от правила udev).
    pub пишется: bool,
}

impl Подсветка {
    pub fn доля(&self) -> f32 {
        if self.макс == 0 { 0.0 } else { self.сейчас as f32 / self.макс as f32 }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Снимок {
    /// Демон режимов отвечает.
    pub служба: bool,
    pub активный: String,
    /// Какие профили демон знает (на машине без platform_profile «производительности» нет).
    pub профили: Vec<String>,
    /// Почему «производительность» урезана (`lap-detected`, `high-operating-temperature`), или пусто.
    pub ограничение: String,
    /// Заряд, %; `None` — батареи нет.
    pub заряд: Option<u8>,
    pub заряжается: bool,
    /// Расход (или ток заряда), ватты.
    pub ватт: Option<f32>,
    /// Сколько осталось до разряда / до полного, минуты.
    pub минут: Option<u32>,
    pub экран: Option<Подсветка>,
    pub клавиатура: Option<Подсветка>,
}

pub enum Event {
    Снимок(Снимок),
    Сообщение(String),
}

pub enum Cmd {
    /// Меню открыли или закрыли: открытое опрашивается раз в `ОПРОС`.
    Смотреть(bool),
    Профиль(String),
    /// Яркость экрана: доля 0…1.
    Экран(f32),
    /// Шаг яркости экрана: ±доля.
    ЭкранШаг(f32),
    /// Подсветка клавиатуры: уровень.
    Клавиатура(u32),
    /// Следующий уровень подсветки клавиатуры по кругу.
    КлавиатураКруг,
}

pub fn spawn(to_plx: channel::Sender<Event>) -> Option<mpsc::Sender<Cmd>> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    std::thread::Builder::new()
        .name("plx-power".into())
        .spawn(move || serve(to_plx, rx))
        .ok()?;
    Some(tx)
}

fn serve(to_plx: channel::Sender<Event>, rx: mpsc::Receiver<Cmd>) {
    let mut conn: Option<zbus::blocking::Connection> = None;
    let mut смотрят = false;
    let mut прошлый: Option<Снимок> = None;
    let отправить = |conn: &mut Option<zbus::blocking::Connection>, прошлый: &mut Option<Снимок>| {
        if conn.is_none() {
            *conn = zbus::blocking::Connection::system().ok();
        }
        let с = прочитать(conn.as_ref());
        if прошлый.as_ref() != Some(&с) {
            *прошлый = Some(с.clone());
            let _ = to_plx.send(Event::Снимок(с));
        }
    };
    отправить(&mut conn, &mut прошлый);
    loop {
        let ждать = if смотрят { ОПРОС } else { Duration::from_secs(60) };
        let cmd = match rx.recv_timeout(ждать) {
            Ok(c) => Some(c),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if let Some(cmd) = cmd {
            let снимок = прошлый.clone().unwrap_or_default();
            let ошибка = match cmd {
                Cmd::Смотреть(да) => {
                    смотрят = да;
                    None
                }
                Cmd::Профиль(п) => {
                    if conn.is_none() {
                        conn = zbus::blocking::Connection::system().ok();
                    }
                    поставить_профиль(conn.as_ref(), &п).err()
                }
                Cmd::Экран(доля) => снимок.экран.as_ref().and_then(|п| записать_долю(п, доля, 0.01).err()),
                Cmd::ЭкранШаг(шаг) => снимок
                    .экран
                    .as_ref()
                    .and_then(|п| записать_долю(п, доля_на_глаз(п) + шаг, 0.01).err()),
                Cmd::Клавиатура(у) => снимок.клавиатура.as_ref().and_then(|п| записать(п, у.min(п.макс)).err()),
                Cmd::КлавиатураКруг => снимок.клавиатура.as_ref().and_then(|п| {
                    let след = if п.сейчас >= п.макс { 0 } else { п.сейчас + 1 };
                    записать(п, след).err()
                }),
            };
            if let Some(текст) = ошибка {
                tracing::warn!("plx/power: {текст}");
                let _ = to_plx.send(Event::Сообщение(текст));
            }
        }
        отправить(&mut conn, &mut прошлый);
    }
}

fn прокси<'a>(conn: &'a zbus::blocking::Connection) -> Option<zbus::blocking::Proxy<'a>> {
    for (служба, путь) in [(СЛУЖБА, ПУТЬ), (СЛУЖБА_СТАРАЯ, ПУТЬ_СТАРЫЙ)] {
        if let Ok(p) = zbus::blocking::Proxy::new(conn, служба, путь, служба) {
            if p.get_property::<String>("ActiveProfile").is_ok() {
                return Some(p);
            }
        }
    }
    None
}

fn прочитать(conn: Option<&zbus::blocking::Connection>) -> Снимок {
    let mut с = Снимок::default();
    if let Some(p) = conn.and_then(прокси) {
        с.служба = true;
        с.активный = p.get_property::<String>("ActiveProfile").unwrap_or_default();
        с.ограничение = p.get_property::<String>("PerformanceDegraded").unwrap_or_default();
        if let Ok(список) = p.get_property::<Vec<std::collections::HashMap<String, zbus::zvariant::OwnedValue>>>("Profiles") {
            с.профили = список
                .iter()
                .filter_map(|d| d.get("Profile").and_then(|v| String::try_from(v.clone()).ok()))
                .collect();
        }
    }
    батарея(&mut с);
    с.экран = найти_подсветку("/sys/class/backlight", |_| true);
    с.клавиатура = найти_подсветку("/sys/class/leds", |имя| имя.contains("kbd_backlight"));
    с
}

fn поставить_профиль(conn: Option<&zbus::blocking::Connection>, профиль: &str) -> Result<(), String> {
    let p = conn
        .and_then(прокси)
        .ok_or_else(|| crate::т!("power-profiles-daemon не запущен", "power-profiles-daemon is not running").to_string())?;
    p.set_property("ActiveProfile", профиль).map_err(|e| {
        let e = e.to_string();
        if e.contains("AccessDenied") || e.contains("Not Authorized") {
            crate::т!("polkit не разрешил смену режима", "polkit refused the profile change").to_string()
        } else {
            e
        }
    })?;
    tracing::info!("plx/power: profile → {профиль}");
    Ok(())
}

fn поле(путь: &std::path::Path, имя: &str) -> String {
    std::fs::read_to_string(путь.join(имя)).map(|s| s.trim().to_string()).unwrap_or_default()
}

/// Батарея ноутбука: заряд, расход и сколько осталось. Мыши и наушники
/// (`scope = Device`) не в счёт — как в tray.rs.
fn батарея(с: &mut Снимок) {
    let Ok(dir) = std::fs::read_dir("/sys/class/power_supply") else { return };
    for e in dir.flatten() {
        let п = e.path();
        if поле(&п, "type") != "Battery" || поле(&п, "scope") == "Device" {
            continue;
        }
        let Ok(заряд) = поле(&п, "capacity").parse::<u8>() else { continue };
        с.заряд = Some(заряд.min(100));
        let статус = поле(&п, "status");
        с.заряжается = matches!(статус.as_str(), "Charging" | "Full");
        // Энергия (мкВт·ч, мкВт) или, у части батарей, заряд и ток (мкА·ч, мкА).
        let число = |имя: &str| поле(&п, имя).parse::<f64>().ok();
        let (сейчас, полный, поток) = match (число("energy_now"), число("energy_full"), число("power_now")) {
            (Some(a), Some(b), Some(c)) => (a, b, c),
            _ => match (число("charge_now"), число("charge_full"), число("current_now")) {
                (Some(a), Some(b), Some(c)) => (a, b, c),
                _ => return,
            },
        };
        if число("power_now").is_some() && поток > 0.0 {
            с.ватт = Some((поток / 1e6) as f32);
        } else if let (Some(ток), Some(напр)) = (число("current_now"), число("voltage_now")) {
            с.ватт = Some((ток * напр / 1e12) as f32);
        }
        if поток > 0.0 {
            let часы = match статус.as_str() {
                "Discharging" => Some(сейчас / поток),
                "Charging" => Some((полный - сейчас).max(0.0) / поток),
                _ => None,
            };
            с.минут = часы.filter(|ч| ч.is_finite() && *ч < 48.0).map(|ч| (ч * 60.0).round() as u32);
        }
        return;
    }
}

fn найти_подсветку(каталог: &str, подходит: impl Fn(&str) -> bool) -> Option<Подсветка> {
    let mut лучшая: Option<Подсветка> = None;
    for e in std::fs::read_dir(каталог).ok()?.flatten() {
        let имя = e.file_name().to_string_lossy().into_owned();
        if !подходит(&имя) {
            continue;
        }
        let п = e.path();
        let (Ok(сейчас), Ok(макс)) = (поле(&п, "brightness").parse::<u32>(), поле(&п, "max_brightness").parse::<u32>()) else {
            continue;
        };
        if макс == 0 {
            continue;
        }
        let пишется = std::fs::OpenOptions::new().write(true).open(п.join("brightness")).is_ok();
        let эта = Подсветка { путь: п.to_string_lossy().into_owned(), сейчас, макс, пишется };
        // Несколько экранов подсветки (intel + acpi_video) — берём с самой
        // мелкой ступенью, это и есть «родная».
        if лучшая.as_ref().is_none_or(|л| эта.макс > л.макс) {
            лучшая = Some(эта);
        }
    }
    лучшая
}

fn записать(п: &Подсветка, значение: u32) -> Result<(), String> {
    std::fs::write(std::path::Path::new(&п.путь).join("brightness"), значение.to_string()).map_err(|e| {
        if e.kind() == std::io::ErrorKind::PermissionDenied {
            crate::т!(
                "нет прав на подсветку (нужно правило udev для группы video)",
                "no permission for the backlight (needs the udev rule for group video)"
            )
            .to_string()
        } else {
            format!("{}: {e}", п.путь)
        }
    })
}

/// Доля яркости → значение. Не ниже `пол`: экран в ноль на ноутбуке — это
/// «экран сломался», а не «темно».
fn записать_долю(п: &Подсветка, доля: f32, пол: f32) -> Result<(), String> {
    let доля = доля.clamp(пол, 1.0);
    // Глаз видит яркость примерно по степени: шаг в 5% у тёмного края почти
    // целый свет, у светлого — незаметен. Поэтому доля в меню — «на глаз»,
    // а в sysfs уходит её квадрат.
    let значение = ((доля * доля) * п.макс as f32).round().max(1.0) as u32;
    записать(п, значение.min(п.макс))
}

/// Доля «на глаз» из значения sysfs — обратная `записать_долю`.
pub fn доля_на_глаз(п: &Подсветка) -> f32 {
    п.доля().clamp(0.0, 1.0).sqrt()
}

// ── Меню ────────────────────────────────────────────────────────────────────

/// Строки меню: что под ними.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Строка {
    Профиль(usize),
    Экран,
    Клавиатура,
}

pub struct ПитаниеUi {
    tx: mpsc::Sender<Cmd>,
    pub снимок: Снимок,
    pub открыто: bool,
    pub выбрана: usize,
    сообщение: Option<(String, Instant)>,
    /// Прямоугольники строк на экране — заполняет отрисовка.
    pub rows: Vec<(crate::tray::Rect, Строка)>,
}

impl ПитаниеUi {
    /// Выбираемые строки по порядку (заголовки разделов не в счёт).
    pub fn строки(&self) -> Vec<Строка> {
        let mut v: Vec<Строка> = (0..ПРОФИЛИ.len()).map(Строка::Профиль).collect();
        if self.снимок.экран.is_some() {
            v.push(Строка::Экран);
        }
        if self.снимок.клавиатура.is_some() {
            v.push(Строка::Клавиатура);
        }
        v
    }

    pub fn сообщение(&self) -> Option<&str> {
        self.сообщение
            .as_ref()
            .filter(|(_, t)| t.elapsed() < СООБЩЕНИЕ_ЖИВЁТ)
            .map(|(s, _)| s.as_str())
    }
}

impl crate::state::Parallax {
    pub fn init_питание(&mut self, tx: mpsc::Sender<Cmd>) {
        self.питание = Some(ПитаниеUi {
            tx,
            снимок: Снимок::default(),
            открыто: false,
            выбрана: 0,
            сообщение: None,
            rows: Vec::new(),
        });
    }

    pub fn питание_событие(&mut self, event: Event) {
        let Some(п) = self.питание.as_mut() else { return };
        match event {
            Event::Снимок(с) => п.снимок = с,
            Event::Сообщение(т) => п.сообщение = Some((т, Instant::now())),
        }
        self.request_redraw();
    }

    pub fn питание_открыто(&self) -> bool {
        self.питание.as_ref().is_some_and(|п| п.открыто)
    }

    pub fn питание_послать(&self, cmd: Cmd) {
        if let Some(п) = self.питание.as_ref() {
            if п.tx.send(cmd).is_err() {
                tracing::warn!("plx/power: thread not responding");
            }
        }
    }

    pub fn питание_меню(&mut self) {
        let Some(п) = self.питание.as_mut() else { return };
        п.открыто = !п.открыто;
        let открыто = п.открыто;
        if открыто {
            // Курсор — на текущий режим: первым делом его и меняют.
            let активный = п.снимок.активный.clone();
            п.выбрана = ПРОФИЛИ.iter().position(|x| *x == активный).unwrap_or(0);
        } else {
            self.text_cache.clear();
        }
        self.питание_послать(Cmd::Смотреть(открыто));
        self.request_redraw();
    }

    fn питание_строка(&self) -> Option<Строка> {
        let п = self.питание.as_ref()?;
        п.строки().get(п.выбрана).copied()
    }

    /// Enter / клик по строке.
    fn питание_нажать(&mut self, строка: Строка) {
        match строка {
            Строка::Профиль(i) => self.питание_послать(Cmd::Профиль(ПРОФИЛИ[i].to_string())),
            Строка::Экран => self.питание_послать(Cmd::ЭкранШаг(0.1)),
            Строка::Клавиатура => self.питание_послать(Cmd::КлавиатураКруг),
        }
    }

    /// Влево/вправо по ползунку выбранной строки.
    fn питание_сдвиг(&mut self, знак: i32) {
        match self.питание_строка() {
            Some(Строка::Экран) => self.питание_послать(Cmd::ЭкранШаг(0.05 * знак as f32)),
            Some(Строка::Клавиатура) => {
                let сейчас = self
                    .питание
                    .as_ref()
                    .and_then(|п| п.снимок.клавиатура.as_ref())
                    .map(|к| к.сейчас as i32)
                    .unwrap_or(0);
                self.питание_послать(Cmd::Клавиатура((сейчас + знак).max(0) as u32));
            }
            _ => {}
        }
    }

    /// Клавиша при открытом меню. `true` — съедена.
    pub fn питание_клавиша(&mut self, keysym: u32) -> bool {
        use smithay::input::keyboard::keysyms;
        if !self.питание_открыто() {
            return false;
        }
        let n = self.питание.as_ref().map(|п| п.строки().len()).unwrap_or(0) as i32;
        let mut сдвинуть = |d: i32| {
            if let Some(п) = self.питание.as_mut() {
                if n > 0 {
                    п.выбрана = ((п.выбрана as i32 + d).rem_euclid(n)) as usize;
                }
            }
        };
        match keysym {
            keysyms::KEY_Escape => self.питание_меню(),
            keysyms::KEY_Down | keysyms::KEY_j => сдвинуть(1),
            keysyms::KEY_Up | keysyms::KEY_k => сдвинуть(-1),
            keysyms::KEY_Left | keysyms::KEY_h | keysyms::KEY_minus => self.питание_сдвиг(-1),
            keysyms::KEY_Right | keysyms::KEY_l | keysyms::KEY_equal | keysyms::KEY_plus => {
                self.питание_сдвиг(1)
            }
            keysyms::KEY_1 | keysyms::KEY_2 | keysyms::KEY_3 => {
                let i = (keysym - keysyms::KEY_1) as usize;
                if let Some(п) = self.питание.as_mut() {
                    п.выбрана = i;
                }
                self.питание_нажать(Строка::Профиль(i));
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                if let Some(с) = self.питание_строка() {
                    self.питание_нажать(с);
                }
            }
            _ => {}
        }
        self.request_redraw();
        true
    }

    /// Клик при открытом меню. По строке ползунка — яркость по месту клика
    /// (левая треть строки занята подписью). Мимо меню — закрыть.
    pub fn питание_клик(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Physical>) -> bool {
        if !self.питание_открыто() {
            return false;
        }
        let попал = self.питание.as_ref().and_then(|п| {
            п.rows.iter().find(|(r, _)| r.hit(pos.x, pos.y)).map(|(r, с)| (*r, *с))
        });
        match попал {
            Some((r, строка)) => {
                if let Some(п) = self.питание.as_mut() {
                    if let Some(i) = п.строки().iter().position(|с| *с == строка) {
                        п.выбрана = i;
                    }
                }
                let доля = ((pos.x - r.x as f64) / r.w as f64 - ПОЛЗУНОК_С) / (1.0 - ПОЛЗУНОК_С - 0.04);
                match строка {
                    Строка::Экран if доля >= -0.02 => {
                        self.питание_послать(Cmd::Экран(доля.clamp(0.0, 1.0) as f32))
                    }
                    Строка::Клавиатура if доля >= -0.02 => {
                        let макс = self
                            .питание
                            .as_ref()
                            .and_then(|п| п.снимок.клавиатура.as_ref())
                            .map(|к| к.макс)
                            .unwrap_or(1);
                        let у = (доля.clamp(0.0, 1.0) * макс as f64).round() as u32;
                        self.питание_послать(Cmd::Клавиатура(у));
                    }
                    с => self.питание_нажать(с),
                }
            }
            None => self.питание_меню(),
        }
        self.request_redraw();
        true
    }

    /// Колесо над открытым меню: яркость выбранной (или под курсором) строки.
    pub fn питание_колесо(&mut self, вверх: bool) -> bool {
        if !self.питание_открыто() {
            return false;
        }
        self.питание_сдвиг(if вверх { 1 } else { -1 });
        true
    }
}

/// С какой доли ширины строки начинается ползунок (до неё — подпись).
pub const ПОЛЗУНОК_С: f64 = 0.36;

/// «4 ч 12 мин».
pub fn время(минут: u32) -> String {
    let (ч, м) = (минут / 60, минут % 60);
    if ч == 0 {
        crate::тф!("{} мин", "{} min", м)
    } else {
        crate::тф!("{} ч {} мин", "{} h {} min", ч, м)
    }
}

#[cfg(test)]
mod тесты {
    use super::*;

    #[test]
    fn доля_на_глаз_обратна_записи() {
        let п = Подсветка { путь: String::new(), сейчас: 4848, макс: 19393, пишется: true };
        let д = доля_на_глаз(&п);
        let значение = ((д * д) * п.макс as f32).round() as u32;
        assert!((значение as i64 - 4848).abs() <= 1);
    }

    #[test]
    fn время_по_русски_и_без_часов() {
        assert!(время(252).contains('4'));
        assert!(время(5).contains('5'));
    }
}
