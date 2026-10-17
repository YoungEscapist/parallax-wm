//! Встроенные виджеты рабочего стола: часы, календарь, система, заметка.
//!
//! **Не путать с виджетами-окнами** (виджеты.rs): там правило `window{ widget =
//! true }` прибивает к холсту ЧУЖОЕ окно (conky, eww), а здесь плашку рисует сам
//! компоновщик — без клиента, без процесса и без строчки в конфиге. Одно другому
//! не мешает: кому мало встроенных, у того по-прежнему есть правило.
//!
//! Устроены ровно как иконки (рабстол.rs) и живут рядом с ними:
//! * координаты — ХОЛСТА: плашка едет с паном и мельчает на отдалении, у каждого
//!   стола свои виджеты, потому что столы — разные места холста;
//! * лежат между обоями и окнами, под иконками;
//! * ставятся по ПКМ на пустом месте (меню «Иконка / Виджет»), двигаются ЛКМ,
//!   свойства — ПКМ по самой плашке;
//! * сохраняются в `~/.local/state/parallax/widgets.json` после каждой правки.
//!
//! Геометрия плашки и её содержимого считается ЗДЕСЬ (`Виджет::рамка`), а
//! рисует её udev.rs (`build_desktop_widgets`): по той же рамке идёт хит-тест,
//! второй копии чисел нет.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use smithay::utils::{Logical, Point, Rectangle, Size};

use crate::state::Parallax;

/// Какой виджет.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Род {
    #[serde(rename = "clock")]
    Часы,
    #[serde(rename = "calendar")]
    Календарь,
    #[serde(rename = "system")]
    Система,
    #[serde(rename = "note")]
    Заметка,
}

impl Род {
    /// Порядок — тот, в котором они стоят в меню.
    pub const ВСЕ: [Род; 4] = [Род::Часы, Род::Календарь, Род::Система, Род::Заметка];

    pub fn имя(self) -> &'static str {
        match self {
            Род::Часы => crate::т!("Часы", "Clock"),
            Род::Календарь => crate::т!("Календарь", "Calendar"),
            Род::Система => crate::т!("Система", "System"),
            Род::Заметка => crate::т!("Заметка", "Note"),
        }
    }

    /// Подсказка в меню выбора — что внутри.
    pub fn описание(self) -> &'static str {
        match self {
            Род::Часы => crate::т!("время и дата", "time and date"),
            Род::Календарь => crate::т!("месяц, сегодня отмечено", "month, today marked"),
            Род::Система => crate::т!("процессор, память, аптайм", "CPU, memory, uptime"),
            Род::Заметка => crate::т!("свой текст", "your own text"),
        }
    }

    /// Имя для `plx ctl desk` и файла.
    pub fn ключ(self) -> &'static str {
        match self {
            Род::Часы => "clock",
            Род::Календарь => "calendar",
            Род::Система => "system",
            Род::Заметка => "note",
        }
    }

    pub fn из_ключа(s: &str) -> Option<Род> {
        match s {
            "clock" | "часы" => Some(Род::Часы),
            "calendar" | "календарь" => Some(Род::Календарь),
            "system" | "система" => Some(Род::Система),
            "note" | "заметка" => Some(Род::Заметка),
            _ => None,
        }
    }

    /// Размер плашки на холсте при масштабе 100%.
    ///
    /// Размеры подобраны под свой шрифт: он растрируется кратными 13 px
    /// (`text::GLYPH_H`), и строки в плашке должны влезать при целых кеглях.
    pub fn базовый_размер(self) -> (i32, i32) {
        match self {
            Род::Часы => (320, 150),
            Род::Календарь => (300, 290),
            Род::Система => (320, 160),
            Род::Заметка => (300, 200),
        }
    }

    fn сдвиг(self, шаг: i32) -> Род {
        let n = Род::ВСЕ.len() as i32;
        let i = Род::ВСЕ.iter().position(|р| *р == self).unwrap_or(0) as i32;
        Род::ВСЕ[(i + шаг).rem_euclid(n) as usize]
    }
}

/// Пределы масштаба, в процентах.
pub const МИН_МАСШТАБ: i32 = 50;
pub const МАКС_МАСШТАБ: i32 = 300;

/// Предел длины заметки: плашка не резиновая, а длинный текст всё равно не
/// влезет — и растрировать его каждую перерисовку незачем.
pub const МАКС_ТЕКСТ: usize = 400;

/// Один виджет на холсте.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Виджет {
    #[serde(rename = "kind")]
    pub род: Род,
    /// Левый верхний угол плашки на ХОЛСТЕ.
    pub x: i32,
    pub y: i32,
    /// Масштаб в процентах от `Род::базовый_размер`.
    #[serde(rename = "scale", default = "сто")]
    pub масштаб: i32,
    /// Текст заметки (у остальных пусто).
    #[serde(rename = "text", default)]
    pub текст: String,
    /// На каких столах видна плашка: маска тегов, 0 — на всех. Ровно как у
    /// иконки (см. `рабстол::виден_на`), и по той же причине: столы parallax —
    /// это теги, а не только места холста.
    #[serde(rename = "tags", default = "crate::рабстол::стол_по_умолчанию")]
    pub столы: u32,
}

fn сто() -> i32 {
    100
}

impl Виджет {
    pub fn новый(род: Род, x: i32, y: i32) -> Self {
        Self { род, x, y, масштаб: 100, текст: String::new(), столы: 1 }
    }

    /// Прямоугольник плашки на холсте — по нему и рисуется, и ловятся клики.
    pub fn рамка(&self) -> Rectangle<i32, Logical> {
        let (w, h) = self.род.базовый_размер();
        let k = self.масштаб.clamp(МИН_МАСШТАБ, МАКС_МАСШТАБ);
        Rectangle::new(
            Point::from((self.x, self.y)),
            Size::from(((w * k / 100).max(1), (h * k / 100).max(1))),
        )
    }
}

/// Поле свойств виджета.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Поле {
    Род,
    Масштаб,
    Текст,
    Столы,
}

impl Поле {
    /// Поля, которые есть у этого рода: текст бывает только у заметки.
    pub fn для(род: Род) -> &'static [Поле] {
        match род {
            Род::Заметка => &[Поле::Текст, Поле::Род, Поле::Масштаб, Поле::Столы],
            _ => &[Поле::Род, Поле::Масштаб, Поле::Столы],
        }
    }
}

/// Окошко свойств виджета — устроено как у иконки (см. `рабстол::Редактор`).
pub struct Редактор {
    /// `None` — новый, ещё не добавленный (так ставится заметка: сперва текст).
    pub индекс: Option<usize>,
    pub черновик: Виджет,
    pub поле: Поле,
    pub строки: Vec<crate::bar::Rect>,
}

/// Снимок системы для виджета «Система».
#[derive(Clone, Copy, Debug, Default)]
pub struct Замер {
    /// Загрузка процессора за последний интервал, 0…1.
    pub цп: f32,
    pub память_занято: u64,
    pub память_всего: u64,
    pub аптайм: u64,
    pub нагрузка: f32,
}

/// Как часто снимать замер. Чаще незачем: цифра в плашке, дёргающаяся раз в
/// полсекунды, читается хуже, а кадр ради неё собирается целиком.
const ПЕРИОД_ЗАМЕРА: Duration = Duration::from_secs(2);

/// Всё, что держат виджеты, кроме самого списка.
#[derive(Default)]
pub struct Состояние {
    pub список: Vec<Виджет>,
    pub редактор: Option<Редактор>,
    pub замер: Option<Замер>,
    /// Счётчики процессора прошлого замера: (простой, всего).
    пред_цп: Option<(u64, u64)>,
    когда: Option<Instant>,
}

fn путь_файла() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/parallax/widgets.json"))
}

// ── Замер системы ────────────────────────────────────────────────────────────

/// (простой, всего) из первой строки `/proc/stat`.
fn счётчики_цп(stat: &str) -> Option<(u64, u64)> {
    let строка = stat.lines().next()?;
    let mut поля = строка.split_whitespace();
    if поля.next()? != "cpu" {
        return None;
    }
    let числа: Vec<u64> = поля.take(8).filter_map(|s| s.parse().ok()).collect();
    if числа.len() < 4 {
        return None;
    }
    // idle + iowait — простой; всё прочее, включая steal, — работа.
    let простой = числа[3] + числа.get(4).copied().unwrap_or(0);
    Some((простой, числа.iter().sum()))
}

/// (занято, всего) в байтах из `/proc/meminfo`. «Занято» — это всего минус
/// MemAvailable: кэш страниц ядро отдаст по первому требованию, и считать его
/// занятым значило бы показывать «память кончилась» на любой живой машине.
fn память(meminfo: &str) -> Option<(u64, u64)> {
    let поле = |имя: &str| -> Option<u64> {
        meminfo
            .lines()
            .find(|l| l.starts_with(имя))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|кб| кб * 1024)
    };
    let всего = поле("MemTotal:")?;
    let свободно = поле("MemAvailable:")?;
    Some((всего.saturating_sub(свободно), всего))
}

/// «5.1» — гибибайты с одним знаком.
pub fn гиб(байт: u64) -> String {
    format!("{:.1}", байт as f64 / (1u64 << 30) as f64)
}

/// «3 ч 12 мин» / «2 д 4 ч».
pub fn аптайм_текст(сек: u64) -> String {
    let (д, ч, м) = (сек / 86400, сек % 86400 / 3600, сек % 3600 / 60);
    if д > 0 {
        crate::тф!("{} д {} ч", "{}d {}h", д, ч)
    } else if ч > 0 {
        crate::тф!("{} ч {} мин", "{}h {}m", ч, м)
    } else {
        crate::тф!("{} мин", "{}m", м)
    }
}

// ── Календарь ────────────────────────────────────────────────────────────────

fn високосный(год: i32) -> bool {
    (год % 4 == 0 && год % 100 != 0) || год % 400 == 0
}

pub fn дней_в_месяце(год: i32, месяц0: i32) -> i32 {
    match месяц0 {
        1 => if високосный(год) { 29 } else { 28 },
        3 | 5 | 8 | 10 => 30,
        _ => 31,
    }
}

/// Сетка месяца 6×7 с понедельника: число дня или 0 для пустой клетки, и номер
/// клетки «сегодня».
///
/// `день_недели` — у сегодняшнего числа, от воскресенья (как `tm_wday`).
pub fn сетка_месяца(год: i32, месяц0: i32, число: i32, день_недели: i32) -> ([i32; 42], usize) {
    // День недели первого числа, от понедельника.
    let первый = (день_недели - (число - 1) % 7 + 6 + 7 * 5).rem_euclid(7);
    let mut сетка = [0i32; 42];
    let n = дней_в_месяце(год, месяц0);
    for д in 1..=n {
        let i = (первый + д - 1) as usize;
        if i < 42 {
            сетка[i] = д;
        }
    }
    (сетка, (первый + число - 1).clamp(0, 41) as usize)
}

pub fn имя_месяца(месяц0: i32) -> &'static str {
    const RU: [&str; 12] = [
        "Январь", "Февраль", "Март", "Апрель", "Май", "Июнь",
        "Июль", "Август", "Сентябрь", "Октябрь", "Ноябрь", "Декабрь",
    ];
    const EN: [&str; 12] = [
        "January", "February", "March", "April", "May", "June",
        "July", "August", "September", "October", "November", "December",
    ];
    crate::т!(RU, EN)[месяц0.clamp(0, 11) as usize]
}

/// «понедельник, 14 сентября» / «Monday, 14 September».
pub fn длинная_дата() -> String {
    const ДНИ_RU: [&str; 7] = ["воскресенье", "понедельник", "вторник", "среда", "четверг", "пятница", "суббота"];
    const ДНИ_EN: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
    const МЕС_RU: [&str; 12] = [
        "января", "февраля", "марта", "апреля", "мая", "июня",
        "июля", "августа", "сентября", "октября", "ноября", "декабря",
    ];
    let Some(tm) = crate::bar::local_time() else { return String::new() };
    let день = tm.tm_wday.clamp(0, 6) as usize;
    let мес = tm.tm_mon.clamp(0, 11);
    if crate::lang::русский() {
        format!("{}, {} {}", ДНИ_RU[день], tm.tm_mday, МЕС_RU[мес as usize])
    } else {
        format!("{}, {} {}", ДНИ_EN[день], tm.tm_mday, имя_месяца(мес))
    }
}

/// Разбить текст на строки не шире `ширина` при кегле `scale`. Слово, которое
/// не влезает и одно, режется по буквам — иначе оно уехало бы за плашку.
pub fn перенос(текст: &str, ширина: i32, scale: i32) -> Vec<String> {
    let шир = |s: &str| crate::text::width_of(s, crate::text::Weight::Regular, scale);
    let mut строки = Vec::new();
    for абзац in текст.split('\n') {
        let mut строка = String::new();
        for слово in абзац.split_whitespace() {
            let проба = if строка.is_empty() { слово.to_string() } else { format!("{строка} {слово}") };
            if шир(&проба) <= ширина {
                строка = проба;
                continue;
            }
            if !строка.is_empty() {
                строки.push(std::mem::take(&mut строка));
            }
            if шир(слово) <= ширина {
                строка = слово.to_string();
            } else {
                for ch in слово.chars() {
                    строка.push(ch);
                    if шир(&строка) > ширина && строка.chars().count() > 1 {
                        let последний = строка.pop().unwrap();
                        строки.push(std::mem::take(&mut строка));
                        строка.push(последний);
                    }
                }
            }
        }
        строки.push(строка);
    }
    строки
}

// ── Сторона композитора ──────────────────────────────────────────────────────

impl Parallax {
    pub fn виджеты_загрузить(&mut self) {
        let Some(path) = путь_файла() else { return };
        let Ok(данные) = std::fs::read_to_string(&path) else { return };
        match serde_json::from_str::<Vec<Виджет>>(&данные) {
            Ok(список) => {
                self.рабстол.виджеты.список = список
                    .into_iter()
                    .map(|mut в| {
                        в.масштаб = в.масштаб.clamp(МИН_МАСШТАБ, МАКС_МАСШТАБ);
                        в
                    })
                    .collect();
                tracing::info!(
                    "plx/desktop: {} widgets loaded from {:?}",
                    self.рабстол.виджеты.список.len(), path,
                );
            }
            Err(e) => tracing::warn!("plx/desktop: parsing {:?}: {}", path, e),
        }
    }

    pub fn виджеты_сохранить(&self) {
        let Some(path) = путь_файла() else { return };
        if let Some(родитель) = path.parent() {
            let _ = std::fs::create_dir_all(родитель);
        }
        match serde_json::to_string_pretty(&self.рабстол.виджеты.список) {
            Ok(json) => {
                if let Err(e) = std::fs::write(&path, json) {
                    tracing::warn!("plx/desktop: could not write {:?}: {}", path, e);
                }
            }
            Err(e) => tracing::warn!("plx/desktop: serialisation: {}", e),
        }
    }

    /// Виджет под точкой холста — сверху вниз (последний добавленный выше).
    pub fn виджет_в_точке(&self, точка: Point<f64, Logical>) -> Option<usize> {
        let столы = self.viewport.current_tags();
        self.рабстол.виджеты.список.iter().enumerate().rev().find_map(|(i, в)| {
            if !crate::рабстол::виден_на(в.столы, столы) {
                return None;
            }
            let р = в.рамка();
            let внутри = точка.x >= р.loc.x as f64
                && точка.x < (р.loc.x + р.size.w) as f64
                && точка.y >= р.loc.y as f64
                && точка.y < (р.loc.y + р.size.h) as f64;
            внутри.then_some(i)
        })
    }

    /// Поставить виджет в точку холста, где щёлкнули (это его левый верхний
    /// угол — так же ставится иконка). Заметка сперва открывает свойства:
    /// пустая плашка без текста человеку не нужна, а Escape отменяет всё.
    pub fn виджет_поставить(&mut self, род: Род, место: Point<i32, Logical>) {
        let mut виджет = Виджет::новый(род, место.x, место.y);
        // Виджет принадлежит столу, на котором его ставят.
        виджет.столы = self.viewport.current_tags();
        if род == Род::Заметка {
            self.рабстол.виджеты.редактор = Some(Редактор {
                индекс: None,
                черновик: виджет,
                поле: Поле::Текст,
                строки: Vec::new(),
            });
        } else {
            self.виджет_добавить(виджет);
            self.уведомить(&crate::тф!(
                "виджет «{}»: ПКМ — свойства, ЛКМ — перенести",
                "‘{}’ widget: right click for properties, drag to move",
                род.имя()
            ));
        }
        self.text_cache.clear();
        self.request_redraw();
    }

    pub fn виджет_добавить(&mut self, виджет: Виджет) {
        self.рабстол.виджеты.список.push(виджет);
        self.виджеты_сохранить();
        // Замер снимается сразу, а не через период: плашка «Система» с
        // прочерками вместо цифр в первые две секунды выглядела бы сломанной.
        self.рабстол.виджеты.когда = None;
        self.виджеты_тик();
        self.request_redraw();
    }

    pub fn виджет_свойства(&mut self, индекс: usize) {
        let Some(в) = self.рабстол.виджеты.список.get(индекс).cloned() else { return };
        self.рабстол.меню = None;
        self.рабстол.редактор = None;
        let поле = Поле::для(в.род)[0];
        self.рабстол.виджеты.редактор = Some(Редактор {
            индекс: Some(индекс),
            черновик: в,
            поле,
            строки: Vec::new(),
        });
        self.text_cache.clear();
        self.request_redraw();
    }

    /// Раз в тик: снять замер для «Системы», если такой виджет есть и период
    /// вышел. Без виджета — ни одного чтения /proc.
    pub fn виджеты_тик(&mut self) {
        let с = &mut self.рабстол.виджеты;
        if !с.список.iter().any(|в| в.род == Род::Система) {
            return;
        }
        if с.когда.is_some_and(|t| t.elapsed() < ПЕРИОД_ЗАМЕРА) {
            return;
        }
        с.когда = Some(Instant::now());
        let mut замер = с.замер.unwrap_or_default();
        if let Some(now) = std::fs::read_to_string("/proc/stat").ok().as_deref().and_then(счётчики_цп) {
            if let Some((простой0, всего0)) = с.пред_цп {
                let всего = now.1.saturating_sub(всего0);
                let простой = now.0.saturating_sub(простой0);
                if всего > 0 {
                    замер.цп = (1.0 - простой as f32 / всего as f32).clamp(0.0, 1.0);
                }
            }
            с.пред_цп = Some(now);
        }
        if let Some((занято, всего)) = std::fs::read_to_string("/proc/meminfo").ok().as_deref().and_then(память) {
            замер.память_занято = занято;
            замер.память_всего = всего;
        }
        if let Some(сек) = std::fs::read_to_string("/proc/uptime")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()))
        {
            замер.аптайм = сек as u64;
        }
        if let Some(н) = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|s| s.split_whitespace().next().and_then(|v| v.parse::<f32>().ok()))
        {
            замер.нагрузка = н;
        }
        с.замер = Some(замер);
        self.request_redraw();
    }

    /// Enter в свойствах виджета.
    pub fn виджет_редактор_применить(&mut self) {
        let Some(ред) = self.рабстол.виджеты.редактор.take() else { return };
        let виджет = ред.черновик;
        match ред.индекс {
            Some(i) if i < self.рабстол.виджеты.список.len() => {
                self.рабстол.виджеты.список[i] = виджет;
                self.виджеты_сохранить();
            }
            Some(_) => {}
            None => self.виджет_добавить(виджет),
        }
        self.text_cache.clear();
        self.request_redraw();
    }

    fn виджет_редактор_удалить(&mut self) {
        let Some(ред) = self.рабстол.виджеты.редактор.take() else { return };
        if let Some(i) = ред.индекс.filter(|i| *i < self.рабстол.виджеты.список.len()) {
            let род = self.рабстол.виджеты.список.remove(i).род;
            self.виджеты_сохранить();
            self.уведомить(&crate::тф!("виджет «{}» убран", "‘{}’ widget removed", род.имя()));
        }
        self.text_cache.clear();
        self.request_redraw();
    }

    /// Клик по строке свойств: выбрать поле. Мимо строк — сохранить и закрыть,
    /// как у иконки.
    pub fn виджет_редактор_клик(&mut self, строка: Option<usize>) {
        let Some(ред) = self.рабстол.виджеты.редактор.as_mut() else { return };
        let поля = Поле::для(ред.черновик.род);
        match строка {
            Some(i) if i < поля.len() => ред.поле = поля[i],
            _ => return self.виджет_редактор_применить(),
        }
        self.text_cache.clear();
        self.request_redraw();
    }

    /// Клавиша в свойствах виджета. `true` — съедена.
    pub fn виджет_клавиша(&mut self, keysym: u32, ch: Option<char>) -> bool {
        use smithay::input::keyboard::keysyms;
        let Some(ред) = self.рабстол.виджеты.редактор.as_mut() else { return false };
        let поля = Поле::для(ред.черновик.род);
        let номер = поля.iter().position(|p| *p == ред.поле).unwrap_or(0) as i32;
        let n = поля.len() as i32;
        match keysym {
            keysyms::KEY_Escape => {
                self.рабстол.виджеты.редактор = None;
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                self.виджет_редактор_применить();
                return true;
            }
            keysyms::KEY_Delete => {
                self.виджет_редактор_удалить();
                return true;
            }
            keysyms::KEY_Down | keysyms::KEY_Tab => {
                ред.поле = поля[(номер + 1).rem_euclid(n) as usize];
            }
            keysyms::KEY_Up | keysyms::KEY_ISO_Left_Tab => {
                ред.поле = поля[(номер - 1).rem_euclid(n) as usize];
            }
            keysyms::KEY_Left | keysyms::KEY_Right => {
                let шаг = if keysym == keysyms::KEY_Left { -1 } else { 1 };
                match ред.поле {
                    Поле::Род => {
                        ред.черновик.род = ред.черновик.род.сдвиг(шаг);
                        // У нового рода может не быть текущего поля (ушли с
                        // заметки) — остаёмся на «Род».
                        ред.поле = Поле::Род;
                    }
                    Поле::Масштаб => {
                        ред.черновик.масштаб =
                            (ред.черновик.масштаб + 10 * шаг).clamp(МИН_МАСШТАБ, МАКС_МАСШТАБ);
                    }
                    // По кругу 0 (все столы), 1…9 — как у иконки.
                    Поле::Столы => {
                        let n = crate::рабстол::номер_стола(ред.черновик.столы) as i32;
                        ред.черновик.столы =
                            crate::рабстол::маска_стола((n + шаг).rem_euclid(10) as u32);
                    }
                    Поле::Текст => {}
                }
            }
            keysyms::KEY_BackSpace => match ред.поле {
                Поле::Текст => {
                    ред.черновик.текст.pop();
                }
                Поле::Масштаб => {
                    ред.черновик.масштаб = (ред.черновик.масштаб / 10).max(1);
                }
                // «Стереть» у столов значит «на всех столах».
                Поле::Столы => ред.черновик.столы = 0,
                Поле::Род => {}
            },
            _ => {
                let Some(c) = ch.filter(|c| !c.is_control()) else { return true };
                match ред.поле {
                    Поле::Текст if ред.черновик.текст.chars().count() < МАКС_ТЕКСТ => {
                        ред.черновик.текст.push(c)
                    }
                    // Цифра — номер стола сразу, а не приписка справа.
                    Поле::Столы => {
                        if let Some(d) = c.to_digit(10) {
                            ред.черновик.столы = crate::рабстол::маска_стола(d);
                        }
                    }
                    Поле::Масштаб => {
                        if let Some(d) = c.to_digit(10) {
                            // Пока набирают, число может быть и меньше
                            // минимума («1» на пути к «150»), зажимаем сверху —
                            // низ зажмёт `рамка` и сохранение.
                            ред.черновик.масштаб =
                                (ред.черновик.масштаб.saturating_mul(10) + d as i32).min(МАКС_МАСШТАБ);
                        }
                    }
                    _ => {}
                }
            }
        }
        self.text_cache.clear();
        self.request_redraw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn сетка_сентября_2026() {
        // 14.09.2026 — понедельник (tm_wday = 1), первое число — вторник.
        let (сетка, сегодня) = сетка_месяца(2026, 8, 14, 1);
        assert_eq!(сетка[0], 0);
        assert_eq!(сетка[1], 1);
        assert_eq!(сетка[сегодня], 14);
        assert_eq!(сетка[30], 30);
        assert_eq!(сетка.iter().filter(|д| **д > 0).count(), 30);
    }

    #[test]
    fn сетка_когда_первое_воскресенье() {
        // 01.03.2026 — воскресенье: последняя клетка первой недели.
        let (сетка, сегодня) = сетка_месяца(2026, 2, 1, 0);
        assert_eq!(сетка[6], 1);
        assert_eq!(сегодня, 6);
        assert_eq!(дней_в_месяце(2028, 1), 29);
    }

    #[test]
    fn счётчики_и_память_разбираются() {
        let stat = "cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3 4\n";
        assert_eq!(счётчики_цп(stat), Some((850, 1000)));
        let mi = "MemTotal:       16000000 kB\nMemFree:  100 kB\nMemAvailable:   4000000 kB\n";
        assert_eq!(память(mi), Some((12000000 * 1024, 16000000 * 1024)));
    }

    #[test]
    fn рамка_по_масштабу() {
        let mut в = Виджет::новый(Род::Часы, 10, 20);
        в.масштаб = 200;
        let р = в.рамка();
        assert_eq!((р.loc.x, р.loc.y, р.size.w, р.size.h), (10, 20, 640, 300));
    }

    #[test]
    fn файл_читается_обратно() {
        let в = vec![Виджет::новый(Род::Система, -5, 7)];
        let json = serde_json::to_string(&в).unwrap();
        assert!(json.contains("\"system\""));
        let назад: Vec<Виджет> = serde_json::from_str(&json).unwrap();
        assert_eq!(назад[0].род, Род::Система);
        // Старый файл без масштаба и текста тоже читается.
        let старый: Vec<Виджет> = serde_json::from_str(r#"[{"kind":"note","x":1,"y":2}]"#).unwrap();
        assert_eq!(старый[0].масштаб, 100);
    }
}
