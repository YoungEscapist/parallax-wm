//! Панель приложений (док): значки для запуска, точки запущенных, правка
//! прямо на самой панели.
//!
//! Просьба Ярика 30.09.2026: «панель приложений для их открытия, которая
//! включается в конфиге и настраивается прямо на ней графически и также в
//! конфиге (её положение, размер, сами приложения на ней)».
//!
//! **Конфиг** — `dock{}` в config.lua:
//!
//! ```lua
//! dock{
//!     enabled = true,          -- показывать панель
//!     position = "bottom",     -- bottom | top | left | right
//!     size = 48,               -- сторона значка, px
//!     margin = 10,             -- отступ от края экрана
//!     autohide = false,        -- прятать, пока курсор не на её месте
//!     align = "center",        -- center | start: значки по центру или с начала
//!     full_width = false,      -- плашка во всю сторону экрана, как в Windows
//!     opacity = 55,            -- непрозрачность плашки, %
//!     locked = false,          -- закрепить: мышью панель не правится
//!     show_running = true,     -- показывать запущенные без значка
//!     apps = {
//!         "ghostty",                                   -- .desktop-файл по имени
//!         "org.telegram.desktop",
//!         { name = "Btop", cmd = "ghostty -e btop", icon = "utilities-system-monitor" },
//!     },
//! }
//! ```
//!
//! **Правка на панели** — всё, что можно задать в конфиге, можно сделать и
//! мышью, и результат ПИШЕТСЯ ОБРАТНО в `config.lua` (блок `dock{}` заменяется
//! целиком, остальной файл не трогается, см. [`заменить_блок`]):
//!
//!  · ЛКМ по значку — запустить; уже запущено — показать его окно;
//!  · СКМ — всегда новое окно;
//!  · перетаскивание значка вдоль панели — поменять порядок; утащить его
//!    с панели подальше — убрать;
//!  · перетаскивание пустого места панели к другому краю экрана — переставить;
//!  · колесо над панелью — размер значков;
//!  · ПКМ по значку — меню приложения, ПКМ по пустому месту — меню панели
//!    (край, размер, автоскрытие, «Добавить приложение…»);
//!  · «+» в конце — список всех установленных приложений с поиском.
//!
//! Геометрию считает ОДНА функция, [`раскладка`]: по ней и рисует
//! `udev::build_dock_elements`, и ловит клики `Parallax::dock_button` —
//! та же схема, что у панели столов (`bar::layout`), иначе рисунок и клики
//! разъезжаются.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use smithay::backend::renderer::element::memory::MemoryRenderBuffer;
use smithay::desktop::Window;
use smithay::utils::{Logical, Physical, Point, Size};

use crate::bar::Rect;
use crate::state::Parallax;

// ── Конфиг ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Выравнивание {
    Центр,
    Начало,
}

impl Выравнивание {
    pub fn из_строки(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "center" | "centre" | "центр" | "по центру" => Выравнивание::Центр,
            "start" | "left" | "top" | "начало" | "слева" => Выравнивание::Начало,
            _ => return None,
        })
    }

    pub fn в_строку(self) -> &'static str {
        match self {
            Выравнивание::Центр => "center",
            Выравнивание::Начало => "start",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Край {
    Низ,
    Верх,
    Лево,
    Право,
}

impl Край {
    pub fn из_строки(s: &str) -> Option<Self> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "bottom" | "низ" | "снизу" => Край::Низ,
            "top" | "верх" | "сверху" => Край::Верх,
            "left" | "лево" | "слева" => Край::Лево,
            "right" | "право" | "справа" => Край::Право,
            _ => return None,
        })
    }

    pub fn в_строку(self) -> &'static str {
        match self {
            Край::Низ => "bottom",
            Край::Верх => "top",
            Край::Лево => "left",
            Край::Право => "right",
        }
    }

    pub fn вертикальный(self) -> bool {
        matches!(self, Край::Лево | Край::Право)
    }
}

/// Приложение, как его записали в конфиге. Всё необязательное:
/// `"ghostty"` — это `id` и больше ничего, остальное найдётся в .desktop.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct Запись {
    /// Имя .desktop-файла без расширения (или app_id окна).
    pub id: String,
    pub name: Option<String>,
    pub cmd: Option<String>,
    pub icon: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DockConfig {
    pub enabled: bool,
    pub position: Край,
    pub size: i32,
    pub margin: i32,
    pub autohide: bool,
    pub align: Выравнивание,
    pub full_width: bool,
    /// Непрозрачность плашки, % (0…100).
    pub opacity: i32,
    /// Закреплена: перетаскивание, колесо и крестики правки не действуют.
    pub locked: bool,
    /// Запущенные приложения без значка — после разделителя.
    pub show_running: bool,
    pub apps: Vec<Запись>,
}

impl Default for DockConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            position: Край::Низ,
            size: 48,
            margin: 10,
            autohide: false,
            align: Выравнивание::Центр,
            full_width: false,
            opacity: ПРОЗРАЧНОСТЬ_ИСХОДНАЯ,
            locked: false,
            show_running: true,
            apps: Vec::new(),
        }
    }
}

pub const SIZE_MIN: i32 = 24;
pub const SIZE_MAX: i32 = 128;
pub const MARGIN_MAX: i32 = 400;
/// Та же доля, что была зашита в отрисовку до настроек (`c[3].min(0.55)`).
pub const ПРОЗРАЧНОСТЬ_ИСХОДНАЯ: i32 = 55;

/// Разобрать `dock{}` из Lua поверх `cfg`: отсутствующий ключ оставляет
/// прежнее значение (та же грабля, что у булевых ключей `set{}`: mlua
/// отдаёт `Ok(false)` на отсутствующий ключ, поэтому только через Option).
pub fn из_lua(cfg: &mut DockConfig, tbl: &mlua::Table) {
    if let Ok(Some(v)) = tbl.get::<Option<bool>>("enabled") {
        cfg.enabled = v;
    }
    if let Ok(Some(v)) = tbl.get::<Option<bool>>("autohide") {
        cfg.autohide = v;
    }
    if let Ok(Some(v)) = tbl.get::<Option<bool>>("full_width") {
        cfg.full_width = v;
    }
    if let Ok(Some(v)) = tbl.get::<Option<bool>>("locked") {
        cfg.locked = v;
    }
    if let Ok(Some(v)) = tbl.get::<Option<bool>>("show_running") {
        cfg.show_running = v;
    }
    if let Ok(Some(s)) = tbl.get::<Option<String>>("align") {
        match Выравнивание::из_строки(&s) {
            Some(в) => cfg.align = в,
            None => tracing::warn!("plx/config: dock{{ align = {s:?} }} — expected center/start"),
        }
    }
    if let Ok(Some(v)) = tbl.get::<Option<f64>>("opacity") {
        if v.is_finite() {
            // Долю 0…1 тоже понимаем: так её пишут в других местах конфига.
            let v = if v > 0.0 && v <= 1.0 && v.fract() != 0.0 { v * 100.0 } else { v };
            cfg.opacity = (v.round() as i32).clamp(0, 100);
        }
    }
    if let Ok(Some(s)) = tbl.get::<Option<String>>("position") {
        match Край::из_строки(&s) {
            Some(к) => cfg.position = к,
            None => tracing::warn!("plx/config: dock{{ position = {s:?} }} — expected bottom/top/left/right"),
        }
    }
    if let Ok(Some(v)) = tbl.get::<Option<f64>>("size") {
        if v.is_finite() {
            cfg.size = (v.round() as i32).clamp(SIZE_MIN, SIZE_MAX);
        }
    }
    if let Ok(Some(v)) = tbl.get::<Option<f64>>("margin") {
        if v.is_finite() {
            cfg.margin = (v.round() as i32).clamp(0, MARGIN_MAX);
        }
    }
    if let Ok(Some(apps)) = tbl.get::<Option<mlua::Table>>("apps") {
        cfg.apps.clear();
        for v in apps.sequence_values::<mlua::Value>().flatten() {
            match v {
                mlua::Value::String(s) => {
                    let s = s.to_string_lossy().to_string();
                    if !s.trim().is_empty() {
                        cfg.apps.push(Запись { id: s.trim().to_string(), ..Default::default() });
                    }
                }
                mlua::Value::Table(t) => {
                    let get = |k: &str| {
                        t.get::<Option<String>>(k)
                            .ok()
                            .flatten()
                            .filter(|s| !s.trim().is_empty())
                    };
                    let запись = Запись {
                        id: get("id").unwrap_or_default(),
                        name: get("name"),
                        cmd: get("cmd"),
                        icon: get("icon"),
                    };
                    if запись.id.is_empty() && запись.cmd.is_none() {
                        tracing::warn!("plx/config: dock{{ apps }} entry without id and cmd skipped");
                        continue;
                    }
                    cfg.apps.push(запись);
                }
                _ => {}
            }
        }
    }
}

// ── .desktop-файлы ───────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default)]
pub struct Ярлык {
    pub id: String,
    pub name: String,
    pub exec: String,
    pub icon: String,
    pub wm_class: String,
    pub terminal: bool,
    pub hidden: bool,
}

/// Каталоги с .desktop-файлами — те же, где ищет значки icons.rs.
fn каталоги() -> Vec<PathBuf> {
    crate::icons::базы_каталогов()
}

fn разобрать_desktop(путь: &Path, id: &str, ru: bool) -> Option<Ярлык> {
    let текст = std::fs::read_to_string(путь).ok()?;
    let mut я = Ярлык { id: id.to_string(), ..Default::default() };
    let mut в_записи = false;
    let mut тип_приложение = true;
    let mut имя_ru: Option<String> = None;
    for строка in текст.lines() {
        let строка = строка.trim();
        if строка.starts_with('[') {
            в_записи = строка == "[Desktop Entry]";
            continue;
        }
        if !в_записи {
            continue;
        }
        let Some((k, v)) = строка.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim());
        match k {
            "Name" => я.name = v.to_string(),
            "Name[ru]" | "Name[ru_RU]" if ru => имя_ru = Some(v.to_string()),
            "Exec" => я.exec = v.to_string(),
            "Icon" => я.icon = v.to_string(),
            "StartupWMClass" => я.wm_class = v.to_string(),
            "Terminal" => я.terminal = v.eq_ignore_ascii_case("true"),
            "NoDisplay" | "Hidden" => я.hidden |= v.eq_ignore_ascii_case("true"),
            "Type" => тип_приложение = v == "Application",
            _ => {}
        }
    }
    if let Some(n) = имя_ru {
        я.name = n;
    }
    if !тип_приложение || я.exec.is_empty() {
        return None;
    }
    if я.name.is_empty() {
        я.name = id.to_string();
    }
    Some(я)
}

/// Все приложения системы. Свои (`~/.local/share/applications`) перебивают
/// системные с тем же id — порядок каталогов от «моего» к общему.
fn все_ярлыки() -> Vec<Ярлык> {
    let ru = crate::lang::русский();
    let mut по_id: HashMap<String, Ярлык> = HashMap::new();
    for база in каталоги() {
        let Ok(чтение) = std::fs::read_dir(&база) else { continue };
        for запись in чтение.flatten() {
            let путь = запись.path();
            if путь.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = путь.file_stem().and_then(|s| s.to_str()).map(str::to_string) else { continue };
            if по_id.contains_key(&id) {
                continue;
            }
            if let Some(я) = разобрать_desktop(&путь, &id, ru) {
                по_id.insert(id, я);
            }
        }
    }
    let mut v: Vec<Ярлык> = по_id.into_values().collect();
    v.sort_by_key(|я| я.name.to_lowercase());
    v
}

/// `Exec=` без кодов полей (`%U`, `%f`…): запускаем без файлов.
fn команда_из_exec(exec: &str, terminal: bool) -> String {
    let mut out = String::new();
    let mut it = exec.chars().peekable();
    while let Some(c) = it.next() {
        if c == '%' {
            match it.next() {
                Some('%') => out.push('%'),
                _ => {}
            }
            continue;
        }
        out.push(c);
    }
    let cmd = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if terminal {
        // Терминальное приложение без терминала запустилось бы в пустоту.
        format!("ghostty -e {cmd}")
    } else {
        cmd
    }
}

// ── Состояние ────────────────────────────────────────────────────────────────

/// Значок на панели — уже разобранный: что показать и что запустить.
#[derive(Clone, Debug)]
pub struct Пункт {
    /// Ключ значка и совпадения с окнами.
    pub ключ: String,
    pub имя: String,
    pub cmd: String,
    pub значок: String,
    /// Из конфига (`true`) или просто запущено (`false`).
    pub закреплён: bool,
    /// Индекс в `cfg.apps` у закреплённых.
    pub индекс: Option<usize>,
    /// Окна этого приложения на всех столах.
    pub окна: Vec<Window>,
    pub в_фокусе: bool,
    /// Имена, по которым окно узнаётся своим (app_id в нижнем регистре).
    совпадения: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Цель {
    Значок(usize),
    Плюс,
    Фон,
}

#[derive(Clone, Copy, Debug)]
struct Нажатие {
    цель: Цель,
    кнопка: u32,
    старт: (f64, f64),
    сейчас: (f64, f64),
    тащим: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Действие {
    Запустить(usize),
    НовоеОкно(usize),
    ЗакрытьОкна(usize),
    Прикрепить(usize),
    Открепить(usize),
    Влево(usize),
    Вправо(usize),
    Край(Край),
    Крупнее,
    Мельче,
    Автоскрытие,
    Закрепить,
    Добавить,
    Спрятать,
    Параметры,
}

pub struct Меню {
    pub пункты: Vec<(String, Действие)>,
    /// Точка, от которой меню раскрывается (экран).
    pub у: (i32, i32),
    pub sel: usize,
    /// Прямоугольники строк — заполняет отрисовка.
    pub rows: Vec<Rect>,
}

pub struct Выбор {
    pub запрос: String,
    pub sel: usize,
    pub rows: Vec<(Rect, usize)>,
}

pub struct Dock {
    pub cfg: DockConfig,
    /// Тумблер Super+U поверх `enabled`.
    pub спрятан: bool,
    pub hover: Option<usize>,
    нажатие: Option<Нажатие>,
    pub меню: Option<Меню>,
    pub выбор: Option<Выбор>,
    /// Окно «Параметры панели» (см. dock_settings.rs).
    pub параметры: Option<crate::dock_settings::Окно>,
    /// Курсор на месте панели — для автоскрытия.
    pub у_края: bool,
    /// Когда курсор ушёл из зоны панели: прячется она не сразу, а через
    /// `ЗАДЕРЖКА_СКРЫТИЯ` — случайный выход на пару пикселей её не гасит.
    pub ушёл_с: Option<Instant>,
    /// Доля выезда 0…1 (0 — спрятана за краем, 1 — на месте). Ведёт её тик
    /// анимаций (`dock_tick`), раскладка сдвигается к краю на остаток.
    pub выезд: f64,
    /// Режим правки (Super+Shift+U): значки покачиваются, у каждого крестик.
    pub правка: bool,
    ярлыки: Option<(Vec<Ярлык>, Instant)>,
    /// Растры значков: (ключ значка, сторона) → буфер.
    pub значки: HashMap<(String, i32), Option<(MemoryRenderBuffer, (i32, i32))>>,
}

impl Dock {
    pub fn new(cfg: DockConfig) -> Self {
        Self { cfg, ..Self::default() }
    }
}

impl Default for Dock {
    fn default() -> Self {
        Self {
            cfg: DockConfig::default(),
            спрятан: false,
            hover: None,
            нажатие: None,
            меню: None,
            выбор: None,
            параметры: None,
            у_края: false,
            ушёл_с: None,
            выезд: 0.0,
            правка: false,
            ярлыки: None,
            значки: HashMap::new(),
        }
    }
}

// ── Раскладка ────────────────────────────────────────────────────────────────

/// Поле вокруг значка внутри клетки.
pub const ПОЛЕ: i32 = 6;
/// Отступ клеток от края плашки.
pub const ОТСТУП: i32 = 8;
/// Промежуток-разделитель между закреплёнными и просто запущенными.
pub const РАЗДЕЛ: i32 = 14;
/// Насколько утащить значок от панели, чтобы он с неё ушёл.
const УТАЩИТЬ: f64 = 90.0;
/// Сдвиг, после которого нажатие становится перетаскиванием.
const ПОРОГ: f64 = 6.0;

#[derive(Clone, Debug, Default)]
pub struct Раскладка {
    pub фон: Rect,
    pub клетки: Vec<Rect>,
    pub плюс: Rect,
    pub вертикальная: bool,
    /// Где рисовать черту-разделитель (если есть незакреплённые).
    pub раздел: Option<Rect>,
}

/// Геометрия панели для `n` значков, из которых `закреплённых` первые.
pub fn раскладка(cfg: &DockConfig, экран: Size<i32, Logical>, n: usize, закреплённых: usize) -> Раскладка {
    let клетка = cfg.size + 2 * ПОЛЕ;
    let верт = cfg.position.вертикальный();
    let есть_раздел = n > закреплённых && закреплённых > 0;
    // Вдоль: отступ, клетки, раздел, «+», отступ.
    let вдоль = 2 * ОТСТУП + (n as i32 + 1) * клетка + if есть_раздел { РАЗДЕЛ } else { 0 };
    let поперёк = клетка + 2 * ОТСТУП;
    // Отрезок вдоль края, в котором панель может лежать: вся сторона без
    // отступов, а у боковых ещё и без полосы панели столов сверху.
    let (от, до) = if верт {
        (cfg.margin + верхний_запас(), экран.h - cfg.margin)
    } else {
        (cfg.margin, экран.w - cfg.margin)
    };
    let место = (до - от).max(вдоль);
    // Во всю ширину плашка занимает весь отрезок, а значки внутри неё встают
    // по выравниванию; иначе по выравниванию встаёт сама плашка.
    let длина = if cfg.full_width { место } else { вдоль };
    let начало = match (cfg.full_width, cfg.align) {
        (true, _) | (false, Выравнивание::Начало) => от,
        (false, Выравнивание::Центр) if верт => (экран.h - длина) / 2,
        (false, Выравнивание::Центр) => (экран.w - длина) / 2,
    };
    let (w, h) = if верт { (поперёк, длина) } else { (длина, поперёк) };
    let (x, y) = match cfg.position {
        Край::Низ => (начало, экран.h - cfg.margin - h),
        Край::Верх => (начало, cfg.margin + верхний_запас()),
        Край::Лево => (cfg.margin, начало),
        Край::Право => (экран.w - cfg.margin - w, начало),
    };
    let фон = Rect { x, y, w, h };
    let mut клетки = Vec::with_capacity(n);
    let mut смещение = match (cfg.full_width, cfg.align) {
        (true, Выравнивание::Центр) => (длина - вдоль) / 2 + ОТСТУП,
        _ => ОТСТУП,
    };
    let mut раздел = None;
    let клетка_в = |смещение: i32| {
        if верт {
            Rect { x: x + ОТСТУП, y: y + смещение, w: клетка, h: клетка }
        } else {
            Rect { x: x + смещение, y: y + ОТСТУП, w: клетка, h: клетка }
        }
    };
    for i in 0..n {
        if есть_раздел && i == закреплённых {
            раздел = Some(if верт {
                Rect { x: x + ОТСТУП + 8, y: y + смещение + РАЗДЕЛ / 2 - 1, w: клетка - 16, h: 2 }
            } else {
                Rect { x: x + смещение + РАЗДЕЛ / 2 - 1, y: y + ОТСТУП + 8, w: 2, h: клетка - 16 }
            });
            смещение += РАЗДЕЛ;
        }
        клетки.push(клетка_в(смещение));
        смещение += клетка;
    }
    let плюс = клетка_в(смещение);
    Раскладка { фон, клетки, плюс, вертикальная: верт, раздел }
}

/// Панель сверху не должна лечь на панель столов.
fn верхний_запас() -> i32 {
    crate::udev::BAR_TOP() + crate::udev::BAR_H()
}

/// Через сколько панель с автоскрытием прячется, когда курсор ушёл.
const ЗАДЕРЖКА_СКРЫТИЯ: std::time::Duration = std::time::Duration::from_millis(400);
/// На сколько px зона, держащая панель, шире самой плашки.
const ЗАПАС_ЗОНЫ: i32 = 12;
/// Длительность выезда и уезда панели, с.
const ВЫЕЗД_СЕК: f64 = 0.20;
const УЕЗД_СЕК: f64 = 0.24;

/// Толщина полосы панели от её края экрана: плашка вместе с отступом.
/// У тайлинга это место больше не отнимается — панель лежит поверх окон.
pub fn резерв(cfg: &DockConfig) -> (Край, i32) {
    (cfg.position, cfg.size + 2 * ПОЛЕ + 2 * ОТСТУП + cfg.margin)
}

// ── Запись в config.lua ──────────────────────────────────────────────────────

fn строка_lua(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Текст блока `dock{}` для config.lua.
pub fn в_lua(cfg: &DockConfig) -> String {
    let mut s = String::new();
    s.push_str("dock{\n");
    s.push_str(&format!("    enabled = {},\n", cfg.enabled));
    s.push_str(&format!("    position = \"{}\",\n", cfg.position.в_строку()));
    s.push_str(&format!("    size = {},\n", cfg.size));
    s.push_str(&format!("    margin = {},\n", cfg.margin));
    s.push_str(&format!("    autohide = {},\n", cfg.autohide));
    s.push_str(&format!("    align = \"{}\",\n", cfg.align.в_строку()));
    s.push_str(&format!("    full_width = {},\n", cfg.full_width));
    s.push_str(&format!("    opacity = {},\n", cfg.opacity));
    s.push_str(&format!("    locked = {},\n", cfg.locked));
    s.push_str(&format!("    show_running = {},\n", cfg.show_running));
    s.push_str("    apps = {\n");
    for a in &cfg.apps {
        if a.name.is_none() && a.cmd.is_none() && a.icon.is_none() {
            s.push_str(&format!("        {},\n", строка_lua(&a.id)));
        } else {
            let mut поля = Vec::new();
            if !a.id.is_empty() {
                поля.push(format!("id = {}", строка_lua(&a.id)));
            }
            if let Some(v) = &a.name {
                поля.push(format!("name = {}", строка_lua(v)));
            }
            if let Some(v) = &a.cmd {
                поля.push(format!("cmd = {}", строка_lua(v)));
            }
            if let Some(v) = &a.icon {
                поля.push(format!("icon = {}", строка_lua(v)));
            }
            s.push_str(&format!("        {{ {} }},\n", поля.join(", ")));
        }
    }
    s.push_str("    },\n}");
    s
}

/// Найти в исходнике Lua вызов `dock{ ... }` (или `dock { ... }`, или
/// `dock({ ... })`) на верхнем уровне строки и вернуть его границы.
/// Скобки считаются мимо строк и комментариев.
pub fn найти_блок(src: &str) -> Option<(usize, usize)> {
    let b = src.as_bytes();
    let mut i = 0usize;
    let mut начало_строки = true;
    while i < b.len() {
        let c = b[i];
        // Комментарий до конца строки (и длинный --[[ ]] тоже).
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            if src[i + 2..].starts_with("[[") {
                i = src[i..].find("]]").map(|p| i + p + 2).unwrap_or(b.len());
            } else {
                i = src[i..].find('\n').map(|p| i + p).unwrap_or(b.len());
            }
            continue;
        }
        if c == b'"' || c == b'\'' {
            i = пропустить_строку(b, i);
            начало_строки = false;
            continue;
        }
        if c == b'\n' {
            начало_строки = true;
            i += 1;
            continue;
        }
        if c == b' ' || c == b'\t' || c == b'\r' {
            i += 1;
            continue;
        }
        if начало_строки && src[i..].starts_with("dock") {
            let после = &src[i + 4..];
            let пробелы = после.len() - после.trim_start_matches([' ', '\t', '(']).len();
            let j = i + 4 + пробелы;
            if b.get(j) == Some(&b'{') {
                let конец = закрыть_скобку(b, j)?;
                // `dock({ ... })` — съесть и закрывающую круглую.
                let mut k = конец;
                while k < b.len() && (b[k] == b' ' || b[k] == b'\t') {
                    k += 1;
                }
                if b.get(k) == Some(&b')') {
                    k += 1;
                } else {
                    k = конец;
                }
                return Some((i, k));
            }
        }
        начало_строки = false;
        i += 1;
    }
    None
}

fn пропустить_строку(b: &[u8], i: usize) -> usize {
    let q = b[i];
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == b'\\' {
            j += 2;
            continue;
        }
        if b[j] == q || b[j] == b'\n' {
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// Позиция сразу за парной `}` к `{` в `j`.
fn закрыть_скобку(b: &[u8], j: usize) -> Option<usize> {
    let mut глубина = 0i32;
    let mut i = j;
    while i < b.len() {
        match b[i] {
            b'"' | b'\'' => {
                i = пропустить_строку(b, i);
                continue;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'{' => глубина += 1,
            b'}' => {
                глубина -= 1;
                if глубина == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Заменить блок `dock{}` в исходнике (или дописать в конец).
pub fn заменить_блок(src: &str, блок: &str) -> String {
    match найти_блок(src) {
        Some((a, b)) => format!("{}{}{}", &src[..a], блок, &src[b..]),
        None => {
            let mut s = src.trim_end().to_string();
            s.push_str("\n\n-- Панель приложений (см. dock.rs). Правится и здесь, и прямо на\n");
            s.push_str("-- панели: ПКМ по ней — меню, перетаскивание — порядок и край.\n");
            s.push_str(блок);
            s.push('\n');
            s
        }
    }
}

// ── Логика ───────────────────────────────────────────────────────────────────

impl Parallax {
    /// Применить `dock{}` из только что прочитанного конфига.
    pub fn dock_apply_config(&mut self) {
        let новый = self.lua_config.dock.clone();
        if новый != self.dock.cfg {
            let размер_сменился = новый.size != self.dock.cfg.size;
            self.dock.cfg = новый;
            if размер_сменился {
                self.dock.значки.clear();
            }
            self.arrange();
        }
        self.request_redraw();
    }

    /// Видна ли панель вообще (без учёта автоскрытия).
    pub fn dock_enabled(&self) -> bool {
        self.dock.cfg.enabled && !self.dock.спрятан
    }

    /// Должна ли панель быть на экране — цель анимации выезда.
    pub fn dock_target(&self) -> bool {
        if !self.dock_enabled() || self.overview_active || self.fullscreen_here() {
            return false;
        }
        !self.dock.cfg.autohide
            || self.dock.у_края
            || self.dock.hover.is_some()
            || self.dock.нажатие.is_some()
            || self.dock.меню.is_some()
            || self.dock.параметры.is_some()
    }

    /// Видна ли панель прямо сейчас (хотя бы краем — пока выезжает или уезжает).
    pub fn dock_visible(&self) -> bool {
        self.dock.cfg.enabled && self.dock.выезд > 0.0
    }

    /// Шаг анимации выезда и отложенное скрытие. Зовётся из `anim::tick`;
    /// `true` — что-то изменилось и нужен кадр.
    pub fn dock_tick(&mut self, dt: std::time::Duration) -> bool {
        let mut dirty = false;
        if let Some(с) = self.dock.ушёл_с {
            if с.elapsed() >= ЗАДЕРЖКА_СКРЫТИЯ {
                self.dock.ушёл_с = None;
                self.dock.у_края = false;
            }
        }
        let цель = if self.dock_target() { 1.0 } else { 0.0 };
        if self.dock.выезд != цель {
            let длит = if цель > 0.0 { ВЫЕЗД_СЕК } else { УЕЗД_СЕК };
            let шаг = dt.as_secs_f64() / длит;
            self.dock.выезд = if цель > self.dock.выезд {
                (self.dock.выезд + шаг).min(1.0)
            } else {
                (self.dock.выезд - шаг).max(0.0)
            };
            if self.dock.выезд == 0.0 {
                self.dock.меню = None;
                self.dock.hover = None;
            }
            dirty = true;
        }
        dirty
    }

    /// Идёт ли анимация панели (для частоты тика).
    pub fn dock_anim_busy(&self) -> bool {
        self.dock.ушёл_с.is_some()
            || self.dock.выезд != if self.dock_target() { 1.0 } else { 0.0 }
    }

    /// Super+U.
    pub fn dock_toggle(&mut self) {
        if !self.dock.cfg.enabled {
            // Выключенную в конфиге панель тумблер включает — и записывает:
            // иначе после перезапуска она снова пропала бы.
            self.dock.cfg.enabled = true;
            self.dock.спрятан = false;
            self.dock_save();
        } else {
            self.dock.спрятан = !self.dock.спрятан;
        }
        self.dock.меню = None;
        self.arrange();
        self.request_redraw();
    }

    /// Super+Shift+U — режим правки.
    pub fn dock_edit_toggle(&mut self) {
        if !self.dock_enabled() {
            self.dock_toggle();
        }
        self.dock.правка = !self.dock.правка;
        self.request_redraw();
    }

    fn dock_ярлыки(&mut self) -> &Vec<Ярлык> {
        let свежие = self
            .dock
            .ярлыки
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed().as_secs() < 30);
        if !свежие {
            self.dock.ярлыки = Some((все_ярлыки(), Instant::now()));
        }
        &self.dock.ярлыки.as_ref().unwrap().0
    }

    fn найти_ярлык(&mut self, id: &str) -> Option<Ярлык> {
        let нижний = id.to_lowercase();
        let хвост = format!(".{нижний}");
        let все = self.dock_ярлыки();
        все.iter()
            .find(|я| я.id == id)
            .or_else(|| все.iter().find(|я| я.id.to_lowercase() == нижний))
            .or_else(|| все.iter().find(|я| я.id.to_lowercase().ends_with(&хвост)))
            .or_else(|| все.iter().find(|я| я.wm_class.to_lowercase() == нижний))
            .cloned()
    }

    /// Значки панели: закреплённые из конфига, затем запущенные без значка.
    pub fn dock_items(&mut self) -> Vec<Пункт> {
        let фокус = self.focused_window();
        // Окна по app_id (нижний регистр). Закреплённые окна и виджеты тоже
        // окна приложения — считаем все.
        let mut окна: Vec<(String, Window)> = Vec::new();
        for tw in &self.tagged_windows {
            if let Some(id) = crate::xwin::app_id(&tw.window).filter(|s| !s.trim().is_empty()) {
                окна.push((id.to_lowercase(), tw.window.clone()));
            }
        }
        let apps = self.dock.cfg.apps.clone();
        let mut пункты: Vec<Пункт> = Vec::new();
        for (i, a) in apps.iter().enumerate() {
            let я = if a.id.is_empty() { None } else { self.найти_ярлык(&a.id) };
            let имя = a.name.clone()
                .or_else(|| я.as_ref().map(|я| я.name.clone()))
                .unwrap_or_else(|| a.id.clone());
            let cmd = a.cmd.clone()
                .or_else(|| я.as_ref().map(|я| команда_из_exec(&я.exec, я.terminal)))
                .unwrap_or_else(|| a.id.clone());
            let значок = a.icon.clone()
                .or_else(|| я.as_ref().map(|я| я.icon.clone()).filter(|s| !s.is_empty()))
                .unwrap_or_else(|| a.id.clone());
            let mut совпадения = vec![a.id.to_lowercase()];
            if let Some(я) = &я {
                совпадения.push(я.id.to_lowercase());
                if !я.wm_class.is_empty() {
                    совпадения.push(я.wm_class.to_lowercase());
                }
                // Хвост обратного DNS: org.telegram.desktop → «desktop» не
                // годится, а вот firefox.desktop → firefox — да.
                if let Some(первое) = команда_из_exec(&я.exec, false).split_whitespace().next() {
                    if let Some(имя) = Path::new(первое).file_name().and_then(|s| s.to_str()) {
                        совпадения.push(имя.to_lowercase());
                    }
                }
            }
            if let Some(c) = &a.cmd {
                if let Some(первое) = c.split_whitespace().next() {
                    if let Some(имя) = Path::new(первое).file_name().and_then(|s| s.to_str()) {
                        совпадения.push(имя.to_lowercase());
                    }
                }
            }
            совпадения.retain(|s| !s.is_empty());
            let мои: Vec<Window> = окна
                .iter()
                .filter(|(id, _)| совпадения.iter().any(|с| с == id))
                .map(|(_, w)| w.clone())
                .collect();
            let в_фокусе = фокус.as_ref().is_some_and(|f| мои.contains(f));
            пункты.push(Пункт {
                ключ: format!("cfg:{i}:{}", a.id),
                имя,
                cmd,
                значок,
                закреплён: true,
                индекс: Some(i),
                окна: мои,
                в_фокусе,
                совпадения,
            });
        }
        // Запущенные, которых на панели нет, — после разделителя.
        let mut чужие: Vec<(String, Vec<Window>)> = Vec::new();
        for (id, w) in окна.iter().filter(|_| self.dock.cfg.show_running) {
            if self.виджет_ли(w) {
                continue;
            }
            if пункты.iter().any(|п| п.совпадения.contains(id)) {
                continue;
            }
            match чужие.iter_mut().find(|(i, _)| i == id) {
                Some((_, v)) => v.push(w.clone()),
                None => чужие.push((id.clone(), vec![w.clone()])),
            }
        }
        for (id, мои) in чужие {
            let я = self.найти_ярлык(&id);
            let в_фокусе = фокус.as_ref().is_some_and(|f| мои.contains(f));
            // Настоящий app_id окна (регистр важен для значка).
            let app_id = мои.first().and_then(crate::xwin::app_id).unwrap_or_else(|| id.clone());
            пункты.push(Пункт {
                ключ: format!("run:{id}"),
                имя: я.as_ref().map(|я| я.name.clone()).unwrap_or_else(|| app_id.clone()),
                cmd: я.as_ref().map(|я| команда_из_exec(&я.exec, я.terminal)).unwrap_or_else(|| app_id.clone()),
                значок: я.as_ref().map(|я| я.icon.clone()).filter(|s| !s.is_empty()).unwrap_or_else(|| app_id.clone()),
                закреплён: false,
                индекс: None,
                окна: мои,
                в_фокусе,
                совпадения: vec![id.clone()],
            });
        }
        пункты
    }

    /// Раскладка панели на месте — без сдвига анимацией выезда.
    pub fn dock_layout_at_rest(&mut self) -> (Раскладка, Vec<Пункт>) {
        let пункты = self.dock_items();
        let закреплённых = пункты.iter().filter(|п| п.закреплён).count();
        let л = раскладка(&self.dock.cfg, self.screen_size(), пункты.len(), закреплённых);
        (л, пункты)
    }

    /// Текущая раскладка панели на экране активного монитора — с учётом
    /// выезда: недоехавшая панель сдвинута к своему краю за экран.
    pub fn dock_layout(&mut self) -> (Раскладка, Vec<Пункт>) {
        let (mut л, пункты) = self.dock_layout_at_rest();
        let в = self.dock.выезд.clamp(0.0, 1.0);
        if в < 1.0 {
            // Выезд — ease-out: быстро трогается, мягко встаёт на место.
            let доля = 1.0 - (1.0 - в).powi(3);
            let путь = резерв(&self.dock.cfg).1 + 2;
            let сдвиг = ((1.0 - доля) * путь as f64).round() as i32;
            let (dx, dy) = match self.dock.cfg.position {
                Край::Низ => (0, сдвиг),
                Край::Верх => (0, -сдвиг),
                Край::Лево => (-сдвиг, 0),
                Край::Право => (сдвиг, 0),
            };
            let двинуть = |r: &mut Rect| {
                r.x += dx;
                r.y += dy;
            };
            двинуть(&mut л.фон);
            двинуть(&mut л.плюс);
            л.клетки.iter_mut().for_each(двинуть);
            if let Some(р) = л.раздел.as_mut() {
                двинуть(р);
            }
        }
        (л, пункты)
    }

    /// Растр значка нужной стороны (кэшируется; `None` — не нашёлся).
    pub fn dock_icon(&mut self, п: &Пункт, сторона: i32) -> Option<(MemoryRenderBuffer, (i32, i32))> {
        let ключ = (п.значок.clone(), сторона);
        if !self.dock.значки.contains_key(&ключ) {
            let путь = Path::new(&п.значок);
            let значок = if путь.is_absolute() {
                crate::icons::прочитать_файл(путь, сторона as u32)
            } else {
                self.icon_cache
                    .значок(&п.значок, сторона as u32)
                    .cloned()
                    .or_else(|| {
                        п.окна.first()
                            .and_then(crate::xwin::app_id)
                            .and_then(|id| self.icon_cache.значок_приложения(&id, сторона as u32).cloned())
                    })
                    // Приложение из /opt кладёт значок рядом с бинарём, а не в
                    // тему (minewire: /opt/minewire/minewire.png).
                    .or_else(|| {
                        let бинарь = п.cmd.split_whitespace().next()?;
                        // Имя из PATH — ищем сам файл и идём по ссылкам до
                        // настоящего места: /usr/bin/x → /opt/x/x.
                        let путь = if бинарь.contains('/') {
                            PathBuf::from(бинарь)
                        } else {
                            std::env::var("PATH").ok()?
                                .split(':')
                                .map(|d| Path::new(d).join(бинарь))
                                .find(|p| p.is_file())?
                        };
                        let путь = std::fs::canonicalize(&путь).unwrap_or(путь);
                        let каталог = путь.parent()?;
                        ["png", "svg"].iter()
                            .map(|e| каталог.join(format!("{}.{e}", п.значок)))
                            .find(|f| f.is_file())
                            .and_then(|f| crate::icons::прочитать_файл(&f, сторона as u32))
                    })
            };
            let буфер = значок.map(|з| {
                (
                    MemoryRenderBuffer::from_slice(
                        &з.rgba,
                        smithay::backend::allocator::Fourcc::Abgr8888,
                        (з.w, з.h),
                        1,
                        smithay::utils::Transform::Normal,
                        None,
                    ),
                    (з.w, з.h),
                )
            });
            self.dock.значки.insert(ключ.clone(), буфер);
        }
        self.dock.значки.get(&ключ).cloned().flatten()
    }

    /// Записать текущие настройки панели в config.lua (блок `dock{}`).
    pub fn dock_save(&mut self) {
        self.lua_config.dock = self.dock.cfg.clone();
        let Some(путь) = crate::config::config_path() else { return };
        let исходник = std::fs::read_to_string(&путь).unwrap_or_default();
        let новый = заменить_блок(&исходник, &в_lua(&self.dock.cfg));
        if новый == исходник {
            return;
        }
        // Проверяем, что файл после правки всё ещё разбирается: сломанный
        // конфиг parallax молча подменил бы встроенным, и человек потерял бы
        // все свои бинды из-за панели.
        if let Err(e) = crate::config::load_from_str(&новый) {
            tracing::warn!("plx/dock: the edited config.lua does not parse ({e}), not saving");
            return;
        }
        let времянка = путь.with_extension("lua.dock-tmp");
        let записано = std::fs::write(&времянка, &новый)
            .and_then(|_| std::fs::rename(&времянка, &путь));
        match записано {
            Ok(()) => tracing::info!("plx/dock: saved dock{{}} to {:?}", путь),
            Err(e) => tracing::warn!("plx/dock: could not write {:?}: {}", путь, e),
        }
    }

    // ── Мышь ────────────────────────────────────────────────────────────────

    fn dock_цель(&mut self, x: f64, y: f64) -> Option<Цель> {
        if !self.dock_visible() {
            return None;
        }
        let (л, _) = self.dock_layout();
        if let Some(i) = л.клетки.iter().position(|к| к.hit(x, y)) {
            return Some(Цель::Значок(i));
        }
        if л.плюс.hit(x, y) {
            return Some(Цель::Плюс);
        }
        л.фон.hit(x, y).then_some(Цель::Фон)
    }

    /// Движение мыши (экранные пиксели). Зовётся на каждое движение.
    pub fn dock_motion(&mut self, pos: Point<f64, Physical>) {
        // Тащат ползунок окна параметров — движение его, а не панели.
        if self.dock_settings_motion(pos) {
            return;
        }
        if !self.dock_enabled() {
            return;
        }
        let (x, y) = (pos.x, pos.y);
        // Автоскрытие: панель выезжает, когда курсор наведён на её место.
        if self.dock.cfg.autohide {
            let s = self.screen_size();
            // Зона — место плашки вместе с полосой до края экрана. Раньше
            // панель звал только край экрана во всю ширину (3 px), и чтобы её
            // достать, мышь приходилось загонять в самый низ (01.10.2026).
            // Выехавшую держит та же зона с запасом: плашка стоит в `margin`
            // от края, и курсор, поднимаясь к значкам, не должен её ронять.
            let на_панели = {
                let (л, _) = self.dock_layout_at_rest();
                let ф = л.фон;
                let з = if self.dock.у_края { ЗАПАС_ЗОНЫ as f64 } else { 0.0 };
                let (x0, y0, x1, y1) = (ф.x as f64 - з, ф.y as f64 - з, (ф.x + ф.w) as f64 + з, (ф.y + ф.h) as f64 + з);
                let (x0, y0, x1, y1) = match self.dock.cfg.position {
                    Край::Низ => (x0, y0, x1, s.h as f64 + 1.0),
                    Край::Верх => (x0, -1.0, x1, y1),
                    Край::Лево => (-1.0, y0, x1, y1),
                    Край::Право => (x0, y0, s.w as f64 + 1.0, y1),
                };
                x >= x0 && x < x1 && y >= y0 && y < y1
            };
            if на_панели {
                self.dock.ушёл_с = None;
                if !self.dock.у_края {
                    self.dock.у_края = true;
                    self.request_redraw();
                }
            } else if self.dock.у_края && self.dock.ушёл_с.is_none() {
                self.dock.ушёл_с = Some(Instant::now());
            }
        }
        if let Some(mut н) = self.dock.нажатие {
            н.сейчас = (x, y);
            // Закреплённую панель мышью не таскают: нажатие остаётся кликом.
            if !н.тащим && !self.dock.cfg.locked && ((x - н.старт.0).hypot(y - н.старт.1)) > ПОРОГ {
                н.тащим = true;
            }
            self.dock.нажатие = Some(н);
            self.request_redraw();
            return;
        }
        let стало = match self.dock_цель(x, y) {
            Some(Цель::Значок(i)) => Some(i),
            _ => None,
        };
        if стало != self.dock.hover {
            self.dock.hover = стало;
            self.request_redraw();
        }
    }

    /// Что под курсором сейчас — для подсказки и перетаскивания в отрисовке.
    pub fn dock_drag_view(&self) -> Option<(usize, (f64, f64), bool)> {
        let н = self.dock.нажатие?;
        match н.цель {
            Цель::Значок(i) if н.тащим && н.кнопка == BTN_LEFT => Some((i, н.сейчас, false)),
            Цель::Фон if н.тащим && н.кнопка == BTN_LEFT => Some((usize::MAX, н.сейчас, true)),
            _ => None,
        }
    }

    /// Кнопка мыши. `true` — съедено панелью.
    pub fn dock_button(&mut self, pos: Point<f64, Physical>, button: u32, pressed: bool) -> bool {
        if !self.dock_enabled() {
            return false;
        }
        let (x, y) = (pos.x, pos.y);

        // Открытое меню: клик по пункту — действие, мимо — закрыть.
        if pressed && self.dock.меню.is_some() {
            let hit = self.dock.меню.as_ref().and_then(|м| {
                м.rows.iter().position(|r| r.hit(x, y)).map(|i| м.пункты[i].1.clone())
            });
            self.dock.меню = None;
            if let Some(д) = hit {
                self.dock_do(д);
            }
            self.request_redraw();
            return true;
        }

        if pressed {
            let Some(цель) = self.dock_цель(x, y) else { return false };
            self.dock.нажатие = Some(Нажатие {
                цель,
                кнопка: button,
                старт: (x, y),
                сейчас: (x, y),
                тащим: false,
            });
            self.request_redraw();
            return true;
        }

        // Отпускание.
        let Some(н) = self.dock.нажатие.take() else { return false };
        if н.кнопка != button {
            self.dock.нажатие = Some(н);
            return true;
        }
        self.request_redraw();
        if н.тащим && button == BTN_LEFT {
            match н.цель {
                Цель::Значок(i) => self.dock_drop_icon(i, (x, y)),
                Цель::Фон => self.dock_drop_panel((x, y)),
                Цель::Плюс => {}
            }
            return true;
        }
        // Закреплённую панель пытались тащить: это не клик — иначе попытка
        // утащить значок запускала бы приложение. Просто отменяем.
        if self.dock.cfg.locked && (x - н.старт.0).hypot(y - н.старт.1) > ПОРОГ {
            return true;
        }
        // Режим правки: крестик в углу закреплённого значка убирает его.
        if let (true, Цель::Значок(i), BTN_LEFT) = (self.dock.правка && !self.dock.cfg.locked, н.цель, button) {
            let (л, пункты) = self.dock_layout();
            if let (Some(к), Some(п)) = (л.клетки.get(i), пункты.get(i)) {
                let крест = Rect { x: к.x + к.w - 18, y: к.y, w: 18, h: 18 };
                if п.закреплён && крест.hit(x, y) {
                    self.dock_do(Действие::Открепить(i));
                    return true;
                }
            }
        }
        match (н.цель, button) {
            (Цель::Значок(i), BTN_LEFT) => self.dock_do(Действие::Запустить(i)),
            (Цель::Значок(i), BTN_MIDDLE) => self.dock_do(Действие::НовоеОкно(i)),
            (Цель::Значок(i), BTN_RIGHT) => self.dock_open_icon_menu(i),
            (Цель::Плюс, BTN_LEFT) => self.dock_do(Действие::Добавить),
            (Цель::Фон | Цель::Плюс, BTN_RIGHT) => self.dock_open_panel_menu((x as i32, y as i32)),
            _ => {}
        }
        true
    }

    /// Колесо над панелью — размер значков. `true` — съедено.
    pub fn dock_axis(&mut self, pos: Point<f64, Physical>, v: f64) -> bool {
        if v == 0.0 || self.dock.нажатие.is_some() {
            return false;
        }
        if self.dock_цель(pos.x, pos.y).is_none() {
            return false;
        }
        // Закреплена — колесо съедаем, но размер не трогаем: иначе под
        // панелью прокручивался бы холст.
        if self.dock.cfg.locked {
            return true;
        }
        let шаг = if v < 0.0 { 4 } else { -4 };
        self.dock_resize(шаг);
        true
    }

    fn dock_resize(&mut self, шаг: i32) {
        let новый = (self.dock.cfg.size + шаг).clamp(SIZE_MIN, SIZE_MAX);
        if новый != self.dock.cfg.size {
            self.dock.cfg.size = новый;
            self.dock_save();
            self.arrange();
            self.request_redraw();
        }
    }

    /// Куда встанет значок, брошенный в точку `(x, y)` (среди закреплённых).
    pub fn dock_drop_slot(&mut self, x: f64, y: f64) -> Option<usize> {
        let (л, пункты) = self.dock_layout();
        let закреплённых = пункты.iter().filter(|п| п.закреплён).count();
        // Слишком далеко от панели — значок уходит с неё.
        let ф = л.фон;
        let dx = if x < ф.x as f64 { ф.x as f64 - x } else if x > (ф.x + ф.w) as f64 { x - (ф.x + ф.w) as f64 } else { 0.0 };
        let dy = if y < ф.y as f64 { ф.y as f64 - y } else if y > (ф.y + ф.h) as f64 { y - (ф.y + ф.h) as f64 } else { 0.0 };
        if dx.max(dy) > УТАЩИТЬ {
            return None;
        }
        let вдоль = |r: &Rect| if л.вертикальная { r.y as f64 + r.h as f64 / 2.0 } else { r.x as f64 + r.w as f64 / 2.0 };
        let точка = if л.вертикальная { y } else { x };
        let слот = л.клетки[..закреплённых].iter().filter(|к| вдоль(к) < точка).count();
        Some(слот.min(закреплённых))
    }

    fn dock_drop_icon(&mut self, i: usize, (x, y): (f64, f64)) {
        let (_, пункты) = self.dock_layout();
        let Some(п) = пункты.get(i).cloned() else { return };
        let слот = self.dock_drop_slot(x, y);
        match (п.индекс, слот) {
            // Закреплённый утащили с панели — убрать.
            (Some(idx), None) => {
                self.dock.cfg.apps.remove(idx);
                tracing::info!("plx/dock: removed {:?} (dragged off)", п.имя);
            }
            // Переставили.
            (Some(idx), Some(s)) => {
                let a = self.dock.cfg.apps.remove(idx);
                let s = if s > idx { s - 1 } else { s }.min(self.dock.cfg.apps.len());
                self.dock.cfg.apps.insert(s, a);
            }
            // Запущенное притащили к закреплённым — прикрепить в это место.
            (None, Some(s)) => {
                let запись = self.dock_запись_для(&п);
                let s = s.min(self.dock.cfg.apps.len());
                self.dock.cfg.apps.insert(s, запись);
            }
            (None, None) => return,
        }
        self.dock_save();
        self.arrange();
    }

    /// Пустое место панели бросили — к какому краю ближе, туда и встаёт.
    fn dock_drop_panel(&mut self, (x, y): (f64, f64)) {
        let s = self.screen_size();
        let (fx, fy) = (x / s.w.max(1) as f64, y / s.h.max(1) as f64);
        let до = [
            (fy, Край::Верх),
            (1.0 - fy, Край::Низ),
            (fx, Край::Лево),
            (1.0 - fx, Край::Право),
        ];
        let край = до.iter().min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, к)| *к).unwrap_or(Край::Низ);
        if край != self.dock.cfg.position {
            self.dock.cfg.position = край;
            self.dock_save();
            self.arrange();
        }
    }

    fn dock_запись_для(&mut self, п: &Пункт) -> Запись {
        // Есть .desktop — хватает его имени: остальное найдётся по нему.
        let id = п.ключ.strip_prefix("run:").unwrap_or(&п.ключ).to_string();
        if self.найти_ярлык(&id).is_some() {
            return Запись { id, ..Default::default() };
        }
        let app_id = п.окна.first().and_then(crate::xwin::app_id).unwrap_or(id);
        Запись { id: app_id, cmd: Some(п.cmd.clone()), ..Default::default() }
    }

    fn dock_open_icon_menu(&mut self, i: usize) {
        let (л, пункты) = self.dock_layout();
        let Some(п) = пункты.get(i) else { return };
        let mut м: Vec<(String, Действие)> = Vec::new();
        м.push((
            if п.окна.is_empty() { crate::т!("Запустить", "Launch") } else { crate::т!("Показать", "Show") }.to_string(),
            Действие::Запустить(i),
        ));
        м.push((crate::т!("Новое окно", "New window").to_string(), Действие::НовоеОкно(i)));
        if !п.окна.is_empty() {
            м.push((
                crate::тф!("Закрыть окна ({})", "Close windows ({})", п.окна.len()),
                Действие::ЗакрытьОкна(i),
            ));
        }
        if п.закреплён {
            м.push((crate::т!("Сдвинуть к началу", "Move back").to_string(), Действие::Влево(i)));
            м.push((crate::т!("Сдвинуть к концу", "Move forward").to_string(), Действие::Вправо(i)));
            м.push((crate::т!("Убрать с панели", "Remove from dock").to_string(), Действие::Открепить(i)));
        } else {
            м.push((crate::т!("Прикрепить к панели", "Keep in dock").to_string(), Действие::Прикрепить(i)));
        }
        let к = л.клетки[i];
        self.dock.меню = Some(Меню {
            пункты: м,
            у: (к.x + к.w / 2, к.y + к.h / 2),
            sel: 0,
            rows: Vec::new(),
        });
        self.request_redraw();
    }

    fn dock_open_panel_menu(&mut self, у: (i32, i32)) {
        let к = self.dock.cfg.position;
        let метка = |свой: Край, ru: &str, en: &str| {
            format!("{}{}", if свой == к { "• " } else { "  " }, crate::т!(ru, en))
        };
        let пункты = vec![
            (метка(Край::Низ, "Снизу", "Bottom"), Действие::Край(Край::Низ)),
            (метка(Край::Верх, "Сверху", "Top"), Действие::Край(Край::Верх)),
            (метка(Край::Лево, "Слева", "Left"), Действие::Край(Край::Лево)),
            (метка(Край::Право, "Справа", "Right"), Действие::Край(Край::Право)),
            (crate::т!("Значки крупнее", "Bigger icons").to_string(), Действие::Крупнее),
            (crate::т!("Значки мельче", "Smaller icons").to_string(), Действие::Мельче),
            (
                format!(
                    "{}{}",
                    if self.dock.cfg.autohide { "• " } else { "  " },
                    crate::т!("Автоскрытие", "Autohide")
                ),
                Действие::Автоскрытие,
            ),
            (
                format!(
                    "{}{}",
                    if self.dock.cfg.locked { "• " } else { "  " },
                    crate::т!("Закрепить панель", "Lock the dock")
                ),
                Действие::Закрепить,
            ),
            (crate::т!("Добавить приложение…", "Add application…").to_string(), Действие::Добавить),
            (crate::т!("Параметры панели…", "Dock settings…").to_string(), Действие::Параметры),
            (crate::т!("Спрятать панель (Super+U)", "Hide the dock (Super+U)").to_string(), Действие::Спрятать),
        ];
        self.dock.меню = Some(Меню { пункты, у, sel: 0, rows: Vec::new() });
        self.request_redraw();
    }

    /// Выполнить пункт меню или действие по значку.
    pub fn dock_do(&mut self, д: Действие) {
        let (_, пункты) = self.dock_layout();
        match д {
            Действие::Запустить(i) => {
                let Some(п) = пункты.get(i) else { return };
                // Запущено — показать окно. Повторный клик по активному
                // перебирает окна приложения по кругу.
                if !п.окна.is_empty() {
                    let фокус = self.focused_window();
                    let следующее = match фокус.and_then(|f| п.окна.iter().position(|w| *w == f)) {
                        Some(k) => п.окна[(k + 1) % п.окна.len()].clone(),
                        None => п.окна[0].clone(),
                    };
                    self.dock_show_window(&следующее);
                } else {
                    self.spawn(&п.cmd);
                }
            }
            Действие::НовоеОкно(i) => {
                if let Some(п) = пункты.get(i) {
                    self.spawn(&п.cmd);
                }
            }
            Действие::ЗакрытьОкна(i) => {
                if let Some(п) = пункты.get(i) {
                    for w in &п.окна {
                        crate::xwin::close(w);
                    }
                }
            }
            Действие::Прикрепить(i) => {
                if let Some(п) = пункты.get(i).cloned() {
                    let з = self.dock_запись_для(&п);
                    self.dock.cfg.apps.push(з);
                    self.dock_save();
                }
            }
            Действие::Открепить(i) => {
                if let Some(idx) = пункты.get(i).and_then(|п| п.индекс) {
                    self.dock.cfg.apps.remove(idx);
                    self.dock_save();
                }
            }
            Действие::Влево(i) | Действие::Вправо(i) => {
                let Some(idx) = пункты.get(i).and_then(|п| п.индекс) else { return };
                let n = self.dock.cfg.apps.len();
                let к = if matches!(д, Действие::Влево(_)) { idx.checked_sub(1) } else { (idx + 1 < n).then_some(idx + 1) };
                if let Some(к) = к {
                    self.dock.cfg.apps.swap(idx, к);
                    self.dock_save();
                }
            }
            Действие::Край(к) => {
                self.dock.cfg.position = к;
                self.dock_save();
            }
            Действие::Крупнее => self.dock_resize(8),
            Действие::Мельче => self.dock_resize(-8),
            Действие::Автоскрытие => {
                self.dock.cfg.autohide = !self.dock.cfg.autohide;
                self.dock_save();
            }
            Действие::Закрепить => {
                self.dock.cfg.locked = !self.dock.cfg.locked;
                self.dock_save();
            }
            Действие::Параметры => self.dock_settings_open(),
            Действие::Добавить => {
                self.dock.ярлыки = None;
                self.dock.выбор = Some(Выбор { запрос: String::new(), sel: 0, rows: Vec::new() });
            }
            Действие::Спрятать => {
                self.dock.спрятан = true;
            }
        }
        self.arrange();
        self.request_redraw();
    }

    fn dock_show_window(&mut self, w: &Window) {
        // Окно на другом столе — сперва туда.
        let теги = self.tagged_windows.iter().find(|tw| &tw.window == w).map(|tw| tw.tags);
        if let Some(t) = теги {
            if t != 0 && t & self.viewport.current_tags() == 0 {
                let стол = 1u32 << t.trailing_zeros();
                self.dispatch_action(crate::config::Action::ViewTag(стол));
            }
        }
        if !self.закреплено(w) {
            self.snap_camera_to_window(w);
        }
        crate::xwin::focus(self, w);
    }

    // ── Список приложений («+») ─────────────────────────────────────────────

    pub fn dock_picker_open(&self) -> bool {
        self.dock.выбор.is_some()
    }

    /// Приложения под запрос, по имени.
    pub fn dock_picker_list(&mut self) -> Vec<Ярлык> {
        let запрос = self.dock.выбор.as_ref().map(|в| в.запрос.to_lowercase()).unwrap_or_default();
        self.dock_ярлыки()
            .iter()
            .filter(|я| !я.hidden)
            .filter(|я| {
                запрос.is_empty()
                    || я.name.to_lowercase().contains(&запрос)
                    || я.id.to_lowercase().contains(&запрос)
            })
            .cloned()
            .collect()
    }

    fn dock_picker_add(&mut self, я: &Ярлык) {
        if !self.dock.cfg.apps.iter().any(|a| a.id == я.id) {
            self.dock.cfg.apps.push(Запись { id: я.id.clone(), ..Default::default() });
            self.dock_save();
            self.arrange();
            tracing::info!("plx/dock: added {:?}", я.id);
        }
        self.dock.выбор = None;
        self.request_redraw();
    }

    /// Клавиши при открытом меню или списке. `true` — съедено.
    pub fn dock_key(&mut self, keysym: u32, ch: Option<char>) -> bool {
        use smithay::input::keyboard::keysyms;
        if self.dock.меню.is_some() {
            let n = self.dock.меню.as_ref().map(|м| м.пункты.len()).unwrap_or(0).max(1);
            match keysym {
                keysyms::KEY_Escape => self.dock.меню = None,
                keysyms::KEY_Down | keysyms::KEY_j => {
                    if let Some(м) = self.dock.меню.as_mut() {
                        м.sel = (м.sel + 1) % n;
                    }
                }
                keysyms::KEY_Up | keysyms::KEY_k => {
                    if let Some(м) = self.dock.меню.as_mut() {
                        м.sel = (м.sel + n - 1) % n;
                    }
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                    let д = self.dock.меню.take().and_then(|м| м.пункты.get(м.sel).map(|p| p.1.clone()));
                    if let Some(д) = д {
                        self.dock_do(д);
                    }
                }
                _ => {}
            }
            self.request_redraw();
            return true;
        }
        if self.dock.выбор.is_none() {
            return false;
        }
        let n = self.dock_picker_list().len();
        match keysym {
            keysyms::KEY_Escape => self.dock.выбор = None,
            keysyms::KEY_Down => {
                if let Some(в) = self.dock.выбор.as_mut() {
                    в.sel = if n == 0 { 0 } else { (в.sel + 1) % n };
                }
            }
            keysyms::KEY_Up => {
                if let Some(в) = self.dock.выбор.as_mut() {
                    в.sel = if n == 0 { 0 } else { (в.sel + n - 1) % n };
                }
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                let sel = self.dock.выбор.as_ref().map(|в| в.sel).unwrap_or(0);
                if let Some(я) = self.dock_picker_list().get(sel).cloned() {
                    self.dock_picker_add(&я);
                }
            }
            keysyms::KEY_BackSpace => {
                if let Some(в) = self.dock.выбор.as_mut() {
                    в.запрос.pop();
                    в.sel = 0;
                }
            }
            _ => {
                if let (Some(c), Some(в)) = (ch, self.dock.выбор.as_mut()) {
                    if !c.is_control() {
                        в.запрос.push(c);
                        в.sel = 0;
                    }
                }
            }
        }
        self.request_redraw();
        true
    }

    /// Клик при открытом списке. `true` — съедено.
    pub fn dock_picker_click(&mut self, pos: Point<f64, Physical>) -> bool {
        if self.dock.выбор.is_none() {
            return false;
        }
        let hit = self.dock.выбор.as_ref().and_then(|в| {
            в.rows.iter().find(|(r, _)| r.hit(pos.x, pos.y)).map(|(_, i)| *i)
        });
        match hit {
            Some(i) => {
                if let Some(я) = self.dock_picker_list().get(i).cloned() {
                    self.dock_picker_add(&я);
                }
            }
            None => self.dock.выбор = None,
        }
        self.request_redraw();
        true
    }
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn блок_находится_и_заменяется() {
        let src = "-- dock{ enabled = false }\nset{ x = 1 }\ndock{\n  apps = { \"a}\", -- }\n  },\n}\nbind{}\n";
        let (a, b) = найти_блок(src).expect("блок");
        assert!(src[a..b].starts_with("dock{"));
        assert!(src[a..b].ends_with('}'));
        let новый = заменить_блок(src, "dock{ enabled = true }");
        assert!(новый.contains("dock{ enabled = true }\nbind{}"));
        assert!(новый.starts_with("-- dock{ enabled = false }"), "комментарий не тронут");
    }

    #[test]
    fn нет_блока_дописывается() {
        let новый = заменить_блок("set{}\n", "dock{ }");
        assert!(новый.trim_end().ends_with("dock{ }"));
    }

    #[test]
    fn lua_обратно_разбирается() {
        let mut cfg = DockConfig {
            enabled: true,
            position: Край::Лево,
            size: 40,
            margin: 6,
            autohide: true,
            align: Выравнивание::Начало,
            full_width: true,
            opacity: 80,
            locked: true,
            show_running: false,
            apps: vec![
                Запись { id: "ghostty".into(), ..Default::default() },
                Запись { id: "x".into(), name: Some("Икс \"1\"".into()), cmd: Some("x --y".into()), icon: None },
            ],
        };
        let текст = в_lua(&cfg);
        let lua = mlua::Lua::new();
        let t: mlua::Table = lua.load(format!("return ({})", текст.replacen("dock", "", 1))).eval().unwrap();
        let mut назад = DockConfig::default();
        из_lua(&mut назад, &t);
        assert_eq!(назад, cfg);
        cfg.apps.clear();
    }

    #[test]
    fn exec_без_кодов() {
        assert_eq!(команда_из_exec("firefox %u", false), "firefox");
        assert_eq!(команда_из_exec("app --x=%%1 %F", false), "app --x=%1");
    }

    #[test]
    fn во_всю_ширину_и_в_начале() {
        let экран = Size::from((1000, 800));
        let cfg = DockConfig { enabled: true, size: 40, margin: 0, full_width: true, ..Default::default() };
        let л = раскладка(&cfg, экран, 3, 3);
        assert_eq!((л.фон.x, л.фон.w), (0, 1000), "плашка во всю ширину");
        // Значки по центру полосы.
        let первый = л.клетки[0].x;
        let конец = л.плюс.x + л.плюс.w;
        assert_eq!(первый - л.фон.x, л.фон.x + л.фон.w - конец);
        let cfg = DockConfig { align: Выравнивание::Начало, ..cfg };
        let л = раскладка(&cfg, экран, 3, 3);
        assert_eq!(л.клетки[0].x, ОТСТУП, "значки с начала");
        let cfg = DockConfig { full_width: false, margin: 10, ..cfg };
        let л = раскладка(&cfg, экран, 3, 3);
        assert_eq!(л.фон.x, 10, "плашка у начала края");
    }

    #[test]
    fn раскладка_снизу_по_центру() {
        let cfg = DockConfig { enabled: true, size: 40, margin: 10, ..Default::default() };
        let л = раскладка(&cfg, Size::from((1000, 800)), 3, 2);
        assert_eq!(л.клетки.len(), 3);
        assert_eq!(л.фон.y + л.фон.h, 790);
        assert_eq!(л.фон.x * 2 + л.фон.w, 1000);
        assert!(л.раздел.is_some());
    }
}
