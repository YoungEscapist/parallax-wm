//! Окно «Параметры панели» — графические настройки панели приложений.
//!
//! Просьба Ярика 30.09.2026: «сделай графические настройки панели приложений
//! (те же настройки, что на Windows, перенеси базовые)». Взято из «Параметры →
//! Персонализация → Панель задач» Windows 10/11 то, что имеет смысл у дока:
//!
//!  · показывать панель;
//!  · расположение на экране (снизу/сверху/слева/справа);
//!  · выравнивание значков (по центру / в начале) — Windows 11;
//!  · во всю ширину экрана — классическая полоса Windows вместо плашки;
//!  · размер значков — «маленькие кнопки панели задач» Windows 10, но ползунком;
//!  · отступ от края;
//!  · прозрачность — «эффекты прозрачности»;
//!  · автоматически скрывать;
//!  · закрепить панель — Windows 10: мышью её больше не правят;
//!  · показывать запущенные приложения;
//!  · список приложений на панели: выше/ниже/убрать и «Добавить…».
//!
//! Как в Windows, кнопки «Применить» нет: всё действует сразу и сразу же
//! пишется в `dock{}` config.lua (`Parallax::dock_save`). Ползунок пишет файл
//! только по отпусканию — иначе config.lua переписывался бы на каждый пиксель.
//!
//! Рисует `udev::build_dock_settings_elements`; прямоугольники, по которым
//! ловятся клики, заполняет она же в [`Окно::цели`] — та же схема, что у формы
//! вайфая.

use smithay::utils::{Physical, Point};

use crate::bar::Rect;
use crate::dock::{DockConfig, Выравнивание, Край, MARGIN_MAX, SIZE_MAX, SIZE_MIN};
use crate::state::Parallax;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Параметр {
    Показывать,
    Край,
    Выравнивание,
    ВоВсюШирину,
    Размер,
    Отступ,
    Прозрачность,
    Автоскрытие,
    Закрепить,
    Запущенные,
    /// Закреплённое приложение по индексу в `cfg.apps`.
    Приложение(usize),
    Добавить,
    Сбросить,
    Готово,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ДелоСПриложением {
    Выше,
    Ниже,
    Убрать,
}

/// Во что попал клик.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Цель {
    Строка(Параметр),
    /// Вариант выбора по номеру.
    Сегмент(Параметр, usize),
    /// Дорожка ползунка.
    Дорожка(Параметр),
    Приложение(usize, ДелоСПриложением),
}

/// Как показать значение строки.
#[derive(Clone, Debug, PartialEq)]
pub enum Контрол {
    Флаг(bool),
    Выбор { варианты: Vec<&'static str>, текущий: usize },
    Ползунок { доля: f64, текст: String },
    Приложение { первое: bool, последнее: bool },
    Кнопка(&'static str),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Строка {
    Раздел(&'static str),
    Пункт { п: Параметр, подпись: String, к: Контрол },
}

/// Шкала ползунка: предел и шаг клавиш.
fn шкала(п: Параметр) -> Option<(i32, i32, i32)> {
    Some(match п {
        Параметр::Размер => (SIZE_MIN, SIZE_MAX, 4),
        // В конфиге отступ бывает до MARGIN_MAX, но ползунок до 400 px был
        // бы бесполезен: всё полезное умещается в первые сто.
        Параметр::Отступ => (0, 100, 2),
        Параметр::Прозрачность => (0, 100, 5),
        _ => return None,
    })
}

fn значение_шкалы(cfg: &DockConfig, п: Параметр) -> i32 {
    match п {
        Параметр::Размер => cfg.size,
        Параметр::Отступ => cfg.margin,
        Параметр::Прозрачность => cfg.opacity,
        _ => 0,
    }
}

fn задать_шкалу(cfg: &mut DockConfig, п: Параметр, v: i32) {
    match п {
        Параметр::Размер => cfg.size = v.clamp(SIZE_MIN, SIZE_MAX),
        Параметр::Отступ => cfg.margin = v.clamp(0, MARGIN_MAX),
        Параметр::Прозрачность => cfg.opacity = v.clamp(0, 100),
        _ => {}
    }
}

const КРАЯ: [Край; 4] = [Край::Низ, Край::Верх, Край::Лево, Край::Право];
const ВЫРАВНИВАНИЯ: [Выравнивание; 2] = [Выравнивание::Центр, Выравнивание::Начало];

pub struct Окно {
    pub фокус: Параметр,
    /// Первая видимая строка.
    pub прокрутка: usize,
    /// Куда можно попасть кликом — заполняет отрисовка.
    pub цели: Vec<(Rect, Цель)>,
    /// Вся плашка окна — заполняет отрисовка.
    pub панель: Rect,
    /// Ползунок, который сейчас тащат.
    pub тащим: Option<Параметр>,
    /// Держать фокус в кадре. Колесо это снимает: иначе прокрутка тут же
    /// возвращалась бы к строке с фокусом.
    pub следить: bool,
}

impl Окно {
    fn new() -> Self {
        Self {
            фокус: Параметр::Показывать,
            прокрутка: 0,
            цели: Vec::new(),
            панель: Rect::default(),
            тащим: None,
            следить: true,
        }
    }
}

/// Строки окна для этого конфига. `имена` — имена закреплённых приложений в
/// порядке `cfg.apps`.
pub fn строки(cfg: &DockConfig, имена: &[String]) -> Vec<Строка> {
    use crate::т;
    let флаг = |п, ru: &'static str, en: &'static str, v: bool| Строка::Пункт {
        п,
        подпись: т!(ru, en).to_string(),
        к: Контрол::Флаг(v),
    };
    let ползунок = |п, ru: &'static str, en: &'static str, ед: &str| {
        let (min, max, _) = шкала(п).unwrap_or((0, 1, 1));
        let v = значение_шкалы(cfg, п);
        Строка::Пункт {
            п,
            подпись: т!(ru, en).to_string(),
            к: Контрол::Ползунок {
                доля: ((v.clamp(min, max) - min) as f64 / (max - min).max(1) as f64).clamp(0.0, 1.0),
                текст: format!("{v}{ед}"),
            },
        }
    };
    let mut v = vec![
        Строка::Раздел(т!("ПАНЕЛЬ", "DOCK")),
        флаг(Параметр::Показывать, "Показывать панель приложений", "Show the dock", cfg.enabled),
        Строка::Раздел(т!("РАСПОЛОЖЕНИЕ", "POSITION")),
        Строка::Пункт {
            п: Параметр::Край,
            подпись: т!("Расположение на экране", "Position on screen").to_string(),
            к: Контрол::Выбор {
                варианты: vec![т!("Снизу", "Bottom"), т!("Сверху", "Top"), т!("Слева", "Left"), т!("Справа", "Right")],
                текущий: КРАЯ.iter().position(|к| *к == cfg.position).unwrap_or(0),
            },
        },
        Строка::Пункт {
            п: Параметр::Выравнивание,
            подпись: т!("Выравнивание значков", "Alignment").to_string(),
            к: Контрол::Выбор {
                варианты: vec![т!("По центру", "Center"), т!("В начале", "Start")],
                текущий: ВЫРАВНИВАНИЯ.iter().position(|в| *в == cfg.align).unwrap_or(0),
            },
        },
        флаг(Параметр::ВоВсюШирину, "Во всю ширину экрана", "Span the whole edge", cfg.full_width),
        ползунок(Параметр::Отступ, "Отступ от края", "Distance from edge", " px"),
        Строка::Раздел(т!("ВИД", "LOOK")),
        ползунок(Параметр::Размер, "Размер значков", "Icon size", " px"),
        ползунок(Параметр::Прозрачность, "Непрозрачность", "Opacity", " %"),
        Строка::Раздел(т!("ПОВЕДЕНИЕ", "BEHAVIOUR")),
        флаг(Параметр::Автоскрытие, "Автоматически скрывать панель", "Automatically hide the dock", cfg.autohide),
        флаг(Параметр::Закрепить, "Закрепить панель", "Lock the dock", cfg.locked),
        флаг(Параметр::Запущенные, "Показывать запущенные приложения", "Show running apps", cfg.show_running),
        Строка::Раздел(т!("ПРИЛОЖЕНИЯ НА ПАНЕЛИ", "PINNED APPS")),
    ];
    let n = cfg.apps.len();
    for (i, a) in cfg.apps.iter().enumerate() {
        v.push(Строка::Пункт {
            п: Параметр::Приложение(i),
            подпись: имена.get(i).cloned().unwrap_or_else(|| a.id.clone()),
            к: Контрол::Приложение { первое: i == 0, последнее: i + 1 == n },
        });
    }
    v.push(Строка::Пункт {
        п: Параметр::Добавить,
        подпись: т!("Добавить приложение на панель", "Add an app to the dock").to_string(),
        к: Контрол::Кнопка(т!("Добавить…", "Add…")),
    });
    v
}

/// Кнопки подвала.
pub const КНОПКИ: [Параметр; 2] = [Параметр::Сбросить, Параметр::Готово];

pub fn подпись_кнопки(п: Параметр) -> &'static str {
    match п {
        Параметр::Сбросить => crate::т!("Сбросить", "Reset"),
        _ => crate::т!("Готово", "Done"),
    }
}

impl Parallax {
    pub fn dock_settings_is_open(&self) -> bool {
        self.dock.параметры.is_some()
    }

    /// Открыть окно (пункт меню панели, действие `dock_settings`).
    pub fn dock_settings_open(&mut self) {
        self.dock.меню = None;
        if self.dock.параметры.is_none() {
            self.dock.параметры = Some(Окно::new());
        }
        self.request_redraw();
    }

    pub fn dock_settings_toggle(&mut self) {
        if self.dock_settings_is_open() {
            self.dock_settings_close();
        } else {
            self.dock_settings_open();
        }
    }

    pub fn dock_settings_close(&mut self) {
        // Бросили окно посреди ползунка — значение всё равно записать.
        if self.dock.параметры.take().is_some_and(|о| о.тащим.is_some()) {
            self.dock_save();
        }
        self.request_redraw();
    }

    /// Имена закреплённых приложений (как их показывает сама панель).
    pub fn dock_settings_names(&mut self) -> Vec<String> {
        self.dock_items().into_iter().filter(|п| п.закреплён).map(|п| п.имя).collect()
    }

    pub fn dock_settings_rows(&mut self) -> Vec<Строка> {
        let имена = self.dock_settings_names();
        строки(&self.dock.cfg, &имена)
    }

    /// Порядок обхода фокусом: строки сверху вниз, затем кнопки подвала.
    fn dock_settings_order(&mut self) -> Vec<Параметр> {
        let mut v: Vec<Параметр> = self
            .dock_settings_rows()
            .iter()
            .filter_map(|с| match с {
                Строка::Пункт { п, .. } => Some(*п),
                Строка::Раздел(_) => None,
            })
            .collect();
        v.extend(КНОПКИ);
        v
    }

    /// Применить изменённый `cfg`: перераскладка, и — если `записать` — в файл.
    fn dock_settings_applied(&mut self, было: &DockConfig, записать: bool) {
        if было == &self.dock.cfg {
            return;
        }
        if было.size != self.dock.cfg.size {
            self.dock.значки.clear();
        }
        if self.dock.cfg.enabled && !было.enabled {
            // Включили здесь — значит видеть её хотят сейчас, а не после
            // второго нажатия Super+U.
            self.dock.спрятан = false;
        }
        if записать {
            self.dock_save();
        }
        self.arrange();
        self.request_redraw();
    }

    /// Шаг значения: ←/→ по выбору и ползунку, переключение флага, сдвиг
    /// приложения.
    fn dock_settings_step(&mut self, п: Параметр, шаг: i32) {
        let было = self.dock.cfg.clone();
        let cfg = &mut self.dock.cfg;
        let по_кругу = |i: usize, n: usize| ((i as i32 + шаг).rem_euclid(n as i32)) as usize;
        match п {
            Параметр::Показывать => cfg.enabled = !cfg.enabled,
            Параметр::ВоВсюШирину => cfg.full_width = !cfg.full_width,
            Параметр::Автоскрытие => cfg.autohide = !cfg.autohide,
            Параметр::Закрепить => cfg.locked = !cfg.locked,
            Параметр::Запущенные => cfg.show_running = !cfg.show_running,
            Параметр::Край => {
                let i = КРАЯ.iter().position(|к| *к == cfg.position).unwrap_or(0);
                cfg.position = КРАЯ[по_кругу(i, КРАЯ.len())];
            }
            Параметр::Выравнивание => {
                let i = ВЫРАВНИВАНИЯ.iter().position(|в| *в == cfg.align).unwrap_or(0);
                cfg.align = ВЫРАВНИВАНИЯ[по_кругу(i, ВЫРАВНИВАНИЯ.len())];
            }
            Параметр::Размер | Параметр::Отступ | Параметр::Прозрачность => {
                let (min, max, ш) = шкала(п).unwrap_or((0, 100, 1));
                // От текущего — к ближайшей отметке шкалы в сторону шага.
                let v = значение_шкалы(cfg, п).clamp(min, max);
                let v = if шаг > 0 { (v / ш + 1) * ш } else { ((v + ш - 1) / ш - 1) * ш };
                задать_шкалу(cfg, п, v.clamp(min, max));
            }
            Параметр::Приложение(i) => {
                return self.dock_settings_app(i, if шаг < 0 { ДелоСПриложением::Выше } else { ДелоСПриложением::Ниже });
            }
            Параметр::Добавить | Параметр::Сбросить | Параметр::Готово => {
                return self.dock_settings_activate(п);
            }
        }
        self.dock_settings_applied(&было, true);
    }

    fn dock_settings_set(&mut self, п: Параметр, вариант: usize) {
        let было = self.dock.cfg.clone();
        match п {
            Параметр::Край => self.dock.cfg.position = КРАЯ[вариант.min(КРАЯ.len() - 1)],
            Параметр::Выравнивание => self.dock.cfg.align = ВЫРАВНИВАНИЯ[вариант.min(ВЫРАВНИВАНИЯ.len() - 1)],
            _ => return,
        }
        self.dock_settings_applied(&было, true);
    }

    /// Ползунок по доле дорожки 0…1. Пишет в файл только `записать`.
    fn dock_settings_slide(&mut self, п: Параметр, доля: f64, записать: bool) {
        let Some((min, max, ш)) = шкала(п) else { return };
        let было = self.dock.cfg.clone();
        let сырое = min as f64 + доля.clamp(0.0, 1.0) * (max - min) as f64;
        let v = ((сырое / ш as f64).round() as i32 * ш).clamp(min, max);
        задать_шкалу(&mut self.dock.cfg, п, v);
        self.dock_settings_applied(&было, записать);
    }

    fn dock_settings_app(&mut self, i: usize, дело: ДелоСПриложением) {
        let n = self.dock.cfg.apps.len();
        if i >= n {
            return;
        }
        let было = self.dock.cfg.clone();
        match дело {
            ДелоСПриложением::Выше if i > 0 => {
                self.dock.cfg.apps.swap(i, i - 1);
                self.dock_settings_focus(Параметр::Приложение(i - 1));
            }
            ДелоСПриложением::Ниже if i + 1 < n => {
                self.dock.cfg.apps.swap(i, i + 1);
                self.dock_settings_focus(Параметр::Приложение(i + 1));
            }
            ДелоСПриложением::Убрать => {
                self.dock.cfg.apps.remove(i);
                // Фокус остаётся на том же месте списка, если оно ещё есть.
                let сюда = if n > 1 { Параметр::Приложение(i.min(n - 2)) } else { Параметр::Добавить };
                self.dock_settings_focus(сюда);
            }
            _ => {}
        }
        self.dock_settings_applied(&было, true);
    }

    fn dock_settings_focus(&mut self, п: Параметр) {
        if let Some(о) = self.dock.параметры.as_mut() {
            о.фокус = п;
            о.следить = true;
        }
    }

    /// Enter/пробел или клик по строке.
    fn dock_settings_activate(&mut self, п: Параметр) {
        match п {
            Параметр::Готово => self.dock_settings_close(),
            Параметр::Сбросить => {
                // Сброс — внешний вид и поведение; приложения и сам факт
                // включения панели остаются: их человек выбирал поштучно.
                let было = self.dock.cfg.clone();
                self.dock.cfg = DockConfig {
                    enabled: было.enabled,
                    apps: было.apps.clone(),
                    ..DockConfig::default()
                };
                self.dock_settings_applied(&было, true);
            }
            Параметр::Добавить => {
                self.dock_do(crate::dock::Действие::Добавить);
            }
            // По строке приложения Enter ничего не значит — убирают Delete.
            Параметр::Приложение(_) => {}
            _ => self.dock_settings_step(п, 1),
        }
        self.request_redraw();
    }

    /// Клавиши при открытом окне. `true` — съедено.
    pub fn dock_settings_key(&mut self, keysym: u32) -> bool {
        use smithay::input::keyboard::keysyms;
        if !self.dock_settings_is_open() {
            return false;
        }
        let фокус = self.dock.параметры.as_ref().map(|о| о.фокус).unwrap_or(Параметр::Показывать);
        let порядок = self.dock_settings_order();
        let i = порядок.iter().position(|п| *п == фокус).unwrap_or(0);
        let n = порядок.len().max(1);
        match keysym {
            keysyms::KEY_Escape => self.dock_settings_close(),
            keysyms::KEY_Down | keysyms::KEY_Tab | keysyms::KEY_j => {
                self.dock_settings_focus(порядок[(i + 1) % n]);
            }
            keysyms::KEY_Up | keysyms::KEY_ISO_Left_Tab | keysyms::KEY_k => {
                self.dock_settings_focus(порядок[(i + n - 1) % n]);
            }
            keysyms::KEY_Left | keysyms::KEY_h => match фокус {
                Параметр::Сбросить | Параметр::Готово => self.dock_settings_focus(порядок[(i + n - 1) % n]),
                п => self.dock_settings_step(п, -1),
            },
            keysyms::KEY_Right | keysyms::KEY_l => match фокус {
                Параметр::Сбросить | Параметр::Готово => self.dock_settings_focus(порядок[(i + 1) % n]),
                п => self.dock_settings_step(п, 1),
            },
            keysyms::KEY_Delete | keysyms::KEY_BackSpace | keysyms::KEY_x => {
                if let Параметр::Приложение(k) = фокус {
                    self.dock_settings_app(k, ДелоСПриложением::Убрать);
                }
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_space => {
                self.dock_settings_activate(фокус);
            }
            _ => {}
        }
        self.request_redraw();
        true
    }

    fn dock_settings_hit(&self, x: f64, y: f64) -> (Option<(Rect, Цель)>, bool) {
        let Some(о) = self.dock.параметры.as_ref() else { return (None, false) };
        // Цели идут от мелких к крупным (сегменты и кнопки строки раньше самой
        // строки) — первая найденная и есть та, что под курсором.
        let цель = о.цели.iter().find(|(r, _)| r.hit(x, y)).copied();
        (цель, о.панель.hit(x, y))
    }

    /// Кнопка мыши. `true` — съедено окном.
    pub fn dock_settings_button(&mut self, pos: Point<f64, Physical>, pressed: bool) -> bool {
        if !self.dock_settings_is_open() {
            return false;
        }
        let (x, y) = (pos.x, pos.y);
        if !pressed {
            let тащили = self.dock.параметры.as_mut().and_then(|о| о.тащим.take());
            if тащили.is_some() {
                self.dock_save();
            }
            return true;
        }
        // Окно ещё ни разу не нарисовано — целей нет, и любой клик выглядел
        // бы «мимо окна» и закрывал его. Замер харнессом: клик через 100 мс
        // после `action dock_settings` приходил раньше первого кадра.
        if self.dock.параметры.as_ref().is_some_and(|о| о.цели.is_empty()) {
            return true;
        }
        let (цель, внутри) = self.dock_settings_hit(x, y);
        match цель {
            Some((_, Цель::Сегмент(п, k))) => {
                self.dock_settings_focus(п);
                self.dock_settings_set(п, k);
            }
            Some((r, Цель::Дорожка(п))) => {
                self.dock_settings_focus(п);
                if let Some(о) = self.dock.параметры.as_mut() {
                    о.тащим = Some(п);
                }
                self.dock_settings_slide(п, (x - r.x as f64) / r.w.max(1) as f64, false);
            }
            Some((_, Цель::Приложение(i, дело))) => {
                self.dock_settings_focus(Параметр::Приложение(i));
                self.dock_settings_app(i, дело);
            }
            Some((_, Цель::Строка(п))) => {
                self.dock_settings_focus(п);
                match п {
                    // Клик по строке ползунка мимо дорожки только ставит фокус.
                    Параметр::Размер | Параметр::Отступ | Параметр::Прозрачность | Параметр::Приложение(_) => {}
                    _ => self.dock_settings_activate(п),
                }
            }
            None if !внутри => self.dock_settings_close(),
            None => {}
        }
        self.request_redraw();
        true
    }

    /// Движение мыши: тащат ползунок — ведём его. `true` — съедено.
    pub fn dock_settings_motion(&mut self, pos: Point<f64, Physical>) -> bool {
        let Some(о) = self.dock.параметры.as_ref() else { return false };
        let Some(п) = о.тащим else {
            // Подсветка строки под курсором считается при отрисовке.
            if о.панель.hit(pos.x, pos.y) {
                self.request_redraw();
            }
            return false;
        };
        let дорожка = о.цели.iter().find(|(_, ц)| *ц == Цель::Дорожка(п)).map(|(r, _)| *r);
        if let Some(r) = дорожка {
            self.dock_settings_slide(п, (pos.x - r.x as f64) / r.w.max(1) as f64, false);
        }
        true
    }

    /// Колесо над окном — прокрутка списка. `true` — съедено.
    pub fn dock_settings_axis(&mut self, pos: Point<f64, Physical>, v: f64) -> bool {
        let Some(о) = self.dock.параметры.as_mut() else { return false };
        if !о.панель.hit(pos.x, pos.y) {
            return false;
        }
        о.следить = false;
        if v > 0.0 {
            о.прокрутка += 1;
        } else if v < 0.0 {
            о.прокрутка = о.прокрутка.saturating_sub(1);
        }
        self.request_redraw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn строки_на_каждое_приложение() {
        let mut cfg = DockConfig::default();
        cfg.apps = vec![
            crate::dock::Запись { id: "a".into(), ..Default::default() },
            crate::dock::Запись { id: "b".into(), ..Default::default() },
        ];
        let с = строки(&cfg, &["А".into()]);
        let прил: Vec<_> = с
            .iter()
            .filter_map(|с| match с {
                Строка::Пункт { п: Параметр::Приложение(i), подпись, к } => Some((*i, подпись.clone(), к.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(прил.len(), 2);
        assert_eq!(прил[0].1, "А");
        assert_eq!(прил[1].1, "b", "без имени — id");
        assert_eq!(прил[1].2, Контрол::Приложение { первое: false, последнее: true });
    }

    #[test]
    fn шкалы_в_пределах() {
        let mut cfg = DockConfig::default();
        задать_шкалу(&mut cfg, Параметр::Размер, 1000);
        assert_eq!(cfg.size, SIZE_MAX);
        задать_шкалу(&mut cfg, Параметр::Прозрачность, -5);
        assert_eq!(cfg.opacity, 0);
    }
}
