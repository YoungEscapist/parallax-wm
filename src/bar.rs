//! Панель сверху: три острова — столы слева, часы по центру, состояние справа.
//!
//! Почему острова, а не одна полоса. dawn рисует интерфейс скруглёнными
//! плашками поверх холста (миникарта, меню, полка), и полоса от края до края
//! из этого ряда выпадала бы. Три отдельные таблетки дают то же, что даёт
//! waybar (модули слева/по центру/справа), не ломая вида.
//!
//! **Геометрия живёт здесь и только здесь.** Отрисовка (`udev::build_bar_elements`)
//! и разбор кликов (`Dawn::bar_click`) зовут одну и ту же [`layout`] — второй
//! копии чисел нет намеренно. Ровно так устроена полка (`tray::layout`), и
//! ровно на разъехавшихся копиях геометрии в dawn уже однажды поехали клики по
//! окнам: на экране одно, в проверке другое.
//!
//! Резерв места под окна (`tiling::BAR_RESERVED`) считается от [`TOP`] и [`H`],
//! то есть от самой панели: поменяли высоту — резерв поехал следом.

use crate::text;

/// Высота острова.
pub const H: i32 = 34;
/// Отступ панели от верхнего края экрана.
pub const TOP: i32 = 8;
/// Скругление островов.
pub const RADIUS: i32 = 12;
/// Поле от боковых краёв экрана до крайних островов. Полка состояния
/// равняется по нему же (см. `tray::layout`).
pub const EDGE: i32 = 24;
/// Поля внутри острова.
const PAD: i32 = 14;
/// Зазор между соседними ячейками внутри острова.
const GAP: i32 = 6;
/// Сторона значка стола (он же размер иконки трея).
pub const DOT: i32 = 20;
/// Сторона значка приложения на чипе — то есть та, в которую его надо
/// растрировать.
///
/// Размер один и тот же на обоих концах: ужимает по площади `sni::fit_icon`,
/// а на экран значок идёт без пересчёта. Домасштабирование готового растра
/// на GPU (так было до 24.08.2026) давало мыло рядом с чистым треем.
///
/// 29.08.2026 значок дорос с `DOT − 2·CHIP_PAD` до полного `DOT`: под ним
/// больше нет плашки, от которой его надо было отступом отделять (см.
/// `draw_bar_window_chip`), — а лишние 4 px на стороне это четверть
/// разрешения значка.
pub const CHIP_ICON: i32 = DOT;
/// Столов на панели — столько же, сколько тегов у Super+цифра.
pub const TAGS: i32 = 9;
/// Толщина разделителя между группами внутри острова.
const SEP_W: i32 = 1;
/// Масштаб основного текста панели: высота строки = 13·TEXT px
/// (см. text::height). Кегль шрифта под неё подбирает text.rs.
pub const TEXT: i32 = 2;
/// Масштаб мелкого текста (дата).
pub const TEXT_SMALL: i32 = 1;
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    pub fn hit(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && x < (self.x + self.w) as f64
            && y >= self.y as f64
            && y < (self.y + self.h) as f64
    }
}

/// Что за ячейка и что с ней делает клик.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Cell {
    /// Значок стола: маска тега. Клик — перейти на этот стол.
    Tag(u32),
    /// Окно текущего стола по порядку. Клик — фокус и перелёт к нему,
    /// средний — закрыть. Наведение показывает предпросмотр.
    Window(usize),
    Clock,
    Date,
    /// Раскладка клавиатуры. Клик — следующая группа xkb.
    Kb,
    /// Громкость: клик — меню звука, правый клик — немота.
    Volume,
    /// Заряд. Кликать не на что.
    Battery,
    /// Иконка трея (индекс в списке `sni::Item`): клик — Activate,
    /// правый — ContextMenu, средний — SecondaryActivate.
    Tray(usize),
    /// Полосочка полки состояния: клик открывает и закрывает её.
    Handle,
    /// Мультиюзер: код доступа и сколько гостей подключено. Средний клик —
    /// остановить раздачу.
    ///
    /// Показывается ТОЛЬКО пока раздача идёт, и это её единственный признак на
    /// экране: код нужно кому-то продиктовать, а забыть, что стол всё ещё
    /// раздаётся, — худшее, что тут может случиться.
    Share,
    /// «+N»: столько окон не влезло в остров. Клик — ничего (показ), но
    /// молчать об этом нельзя: иначе панель врёт, будто открыто ровно столько,
    /// сколько видно.
    WindowsMore(usize),
    /// Разделитель групп — только рисуется.
    Sep,
}

/// Чип окна в левом острове.
///
/// Раньше чип был БУКВОЙ, и в коде тут стояло «значка приложения у dawn взять
/// неоткуда». Теперь есть откуда: `app_id` → .desktop → тема значков (см.
/// icons.rs). Буква осталась запасным вариантом — для окна без app_id и для
/// приложения, значка которого в теме нет.
#[derive(Clone, Debug)]
pub struct WindowChip {
    /// Одна заглавная буква — первая буквенно-цифровая из имени приложения.
    /// Показывается, когда значок не нашёлся.
    pub letter: String,
    /// `app_id` окна — ключ, по которому ищется и кэшируется значок.
    pub app_id: String,
    /// Полное имя для предпросмотра.
    pub title: String,
    pub focused: bool,
}

#[derive(Clone, Copy, Debug)]
pub struct Item {
    pub cell: Cell,
    pub rect: Rect,
}

pub struct Layout {
    pub left: Rect,
    /// None — на экран не влез даже центральный остров (очень узкий выход).
    pub center: Option<Rect>,
    pub right: Rect,
    pub cells: Vec<Item>,
}

impl Layout {
    /// Ячейка под точкой экрана. Разделители пропускаем — нажимать на них
    /// нечего, а «съеденный» ими клик выглядел бы как мёртвая зона.
    pub fn hit(&self, x: f64, y: f64) -> Option<Cell> {
        self.cells.iter()
            .find(|i| i.cell != Cell::Sep && i.rect.hit(x, y))
            .map(|i| i.cell)
    }

    /// Попал ли клик в любой из островов (даже мимо ячеек). Такой клик панель
    /// съедает: под ней окно, и промах по краю таблетки не должен уходить ему.
    pub fn inside(&self, x: f64, y: f64) -> bool {
        self.left.hit(x, y)
            || self.center.is_some_and(|c| c.hit(x, y))
            || self.right.hit(x, y)
    }
}

/// Насколько панель убрана за верхний край: 0 — стоит на месте, 1 — ушла
/// целиком. Отступ считается от `TOP + H` плюс запас под полку состояния,
/// иначе у кромки экрана оставалась бы полоска.
///
/// Хит-тест от этого получается ДАРОМ и всегда согласованным: убранная панель
/// стоит выше нуля по Y, а курсор туда не заходит — значит и клики она не
/// ловит. Второго условия «а видна ли она сейчас» заводить не пришлось.
pub fn island_y(slide: f64) -> i32 {
    // Три высоты острова, а не одна: под панелью висит ещё и полка состояния
    // (`tray::layout`), и уехать надо так, чтобы у кромки не осталось и её
    // (тест `убранная_панель_уходит_за_верхний_край` держит это число).
    let прячем = (TOP + H * 3) as f64;
    TOP - (slide.clamp(0.0, 1.0) * прячем).round() as i32
}

/// Всё, что панели нужно знать о мире. Собирается один раз (см.
/// `Dawn::bar_data`) и идёт и в отрисовку, и в хит-тест.
#[derive(Clone, Debug, Default)]
pub struct Data {
    /// Доля УБРАННОСТИ панели: 0 — на месте, 1 — уехала за верхний край.
    /// Ведёт её `anim::tick`, цель ставит обзор столов и полный экран.
    pub hide: f64,
    pub screen_w: i32,
    /// Окна текущего стола: по одному чипу на окно, в порядке их появления.
    ///
    /// Раньше здесь жил ТЕКСТОВЫЙ ЗАГОЛОВОК активного окна. Он занимал полострова
    /// и сообщал ровно одну вещь — как называется то, на что ты и так смотришь.
    /// Чипы вместо него отвечают на вопрос, ответа на который на экране не было:
    /// что вообще открыто на этом столе и где оно.
    pub windows: Vec<WindowChip>,
    /// «14:32». Пусто — часов нет.
    pub clock: String,
    /// «Пн 23.08».
    pub date: String,
    /// «RU»/«EN»; пусто — раскладка одна, показывать нечего.
    pub kb: String,
    /// Проценты громкости и немота.
    pub volume: Option<(u8, bool)>,
    /// Проценты заряда и «заряжается».
    pub battery: Option<(u8, bool)>,
    /// Сколько иконок в трее.
    pub tray: usize,
    /// Мультиюзер: код доступа и число впущенных гостей. None — раздачи нет.
    pub share: Option<(String, usize)>,
}

/// Подпись чипа раздачи. Одна функция и для замера ширины, и для отрисовки —
/// иначе остров считался бы по одной строке, а показывал другую.
pub fn share_text(код: &str, гостей: usize) -> String {
    if гостей > 0 {
        format!("код {код} · {гостей}")
    } else {
        format!("код {код}")
    }
}

/// Начертания панели — часть её геометрии, а не дело отрисовки.
///
/// Nunito пропорциональный, и у SemiBold адвансы шире Regular: измерить ячейку
/// одним начертанием, а нарисовать другим — значит промахнуться мимо
/// собственного острова. Поэтому пара «что чем набрано» объявлена здесь, рядом
/// с размерами, и `udev::build_bar_elements` берёт её отсюда же.
///
/// STRONG — короткие подписи состояния (часы, раскладка, проценты, буква
/// стола): они лежат поверх полупрозрачной плашки, сквозь которую видно обои, и
/// тонкий штрих на таком кегле о них размывается. BODY — то, что читают
/// текстом: заголовок окна и дата.
pub const STRONG: text::Weight = text::Weight::Semi;
pub const BODY: text::Weight = text::Weight::Regular;

/// Ширина текста в пикселях панели (начертанием BODY).
fn tw(s: &str, scale: i32) -> i32 {
    text::width_of(s, BODY, scale)
}

/// Ширина короткой подписи состояния (начертанием STRONG).
fn tws(s: &str, scale: i32) -> i32 {
    text::width_of(s, STRONG, scale)
}

/// Обрезает строку так, чтобы она влезла в `max_w`, добавляя «…».
///
/// Раньше здесь делили `max_w` на ширину символа: шрифт был моноширинный
/// 7×13, и это работало. Nunito пропорциональный — «Ш» и «i» разной ширины, —
/// так что место под хвост считается настоящей мерой, а число символов
/// подбирает `text::fits`. Заодно вернулось настоящее многоточие: в старой
/// таблице глифов «…» не было и превратилось бы в «?», поэтому ставили три
/// точки; в Nunito 0x2026 есть (покрытие проверено, см. text.rs).
pub fn fit_text(s: &str, scale: i32, max_w: i32) -> String {
    if tw(s, scale) <= max_w {
        return s.to_string();
    }
    let хвост = "…";
    let под_текст = max_w - tw(хвост, scale);
    if под_текст <= 0 {
        return String::new();
    }
    let влезает = text::fits(s, BODY, scale, под_текст);
    if влезает == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(влезает).collect();
    out.push_str(хвост);
    out
}

/// Ширина острова столов без заголовка.
fn tags_w() -> i32 {
    TAGS * DOT + (TAGS - 1) * GAP
}

pub fn layout(d: &Data) -> Layout {
    let y = island_y(d.hide);
    let mut cells: Vec<Item> = Vec::new();

    // ── Центр: часы и дата ───────────────────────────────────────────────────
    // Считаем первым: этот остров стоит ровно по центру экрана и ни от чего не
    // зависит, а левый потом обрезает свой заголовок так, чтобы в него не
    // упереться.
    let clock_w = tws(&d.clock, TEXT);
    let date_w = tw(&d.date, TEXT_SMALL);
    let center = if d.clock.is_empty() && d.date.is_empty() {
        None
    } else {
        let inner = clock_w + if date_w > 0 { GAP * 2 + date_w } else { 0 };
        let w = inner + PAD * 2;
        let x = ((d.screen_w - w) / 2).max(0);
        let r = Rect { x, y, w, h: H };
        cells.push(Item { cell: Cell::Clock, rect: Rect { x: x + PAD, y, w: clock_w, h: H } });
        if date_w > 0 {
            cells.push(Item {
                cell: Cell::Date,
                rect: Rect { x: x + PAD + clock_w + GAP * 2, y, w: date_w, h: H },
            });
        }
        Some(r)
    };

    // ── Право: раскладка, звук, заряд, трей, полка ───────────────────────────
    // Собираем ширины по порядку, а потом раскладываем ОТ ЛЕВОГО края острова:
    // сам остров прижат к правому краю экрана, поэтому его ширину надо знать
    // до того, как ставить ячейки.
    let handle_w = (H / 3).max(6);
    let mut right_cells: Vec<(Cell, i32)> = Vec::new();
    // Раздача — первой в острове: она временная и должна бросаться в глаза,
    // а не теряться между постоянными раскладкой, звуком и зарядом.
    // Разделитель за ней ставится только если дальше что-то есть — иначе он
    // сдвоился бы с тем, что ставят перед треем и перед полкой.
    if let Some((код, гостей)) = d.share.as_ref() {
        right_cells.push((Cell::Share, tws(&share_text(код, *гостей), TEXT)));
        if !d.kb.is_empty() || d.volume.is_some() || d.battery.is_some() {
            right_cells.push((Cell::Sep, SEP_W + GAP * 2));
        }
    }
    if !d.kb.is_empty() {
        right_cells.push((Cell::Kb, tws(&d.kb, TEXT)));
    }
    if let Some((percent, _)) = d.volume {
        // Значок + проценты: значок квадратный со стороной в высоту строки.
        right_cells.push((Cell::Volume, DOT + GAP + tws(&percent_text(percent), TEXT)));
    }
    if let Some((percent, _)) = d.battery {
        right_cells.push((Cell::Battery, DOT + GAP + tws(&percent_text(percent), TEXT)));
    }
    if d.tray > 0 {
        if !right_cells.is_empty() {
            right_cells.push((Cell::Sep, SEP_W + GAP * 2));
        }
        for i in 0..d.tray {
            right_cells.push((Cell::Tray(i), DOT));
        }
    }
    if !right_cells.is_empty() {
        right_cells.push((Cell::Sep, SEP_W + GAP * 2));
    }
    right_cells.push((Cell::Handle, handle_w));

    let right_inner: i32 = right_cells.iter().map(|(_, w)| *w).sum::<i32>()
        + GAP * (right_cells.len() as i32 - 1);
    let right_w = right_inner + PAD * 2;
    let right_x = (d.screen_w - EDGE - right_w).max(0);
    let right = Rect { x: right_x, y, w: right_w, h: H };
    {
        let mut cx = right_x + PAD;
        for (cell, w) in right_cells {
            cells.push(Item { cell, rect: Rect { x: cx, y, w, h: H } });
            cx += w + GAP;
        }
    }

    // ── Лево: столы и окна текущего стола ────────────────────────────────────
    let left_x = EDGE;
    let tags = tags_w();
    // Сколько места осталось до центрального острова (а если его нет — до
    // правого). Остров обязан в него уложиться: наехав на часы, он превращает
    // панель в кашу.
    let предел = center.map(|c| c.x).unwrap_or(right.x) - GAP * 2;
    let без_окон = left_x + PAD * 2 + tags;
    let место_под_окна = предел - без_окон - (SEP_W + GAP * 2 + GAP);
    // Сколько чипов влезает. Чип квадратный, стороной в значок стола.
    let mut влезает = if место_под_окна <= 0 {
        0
    } else {
        (((место_под_окна + GAP) / (DOT + GAP)).max(0) as usize).min(d.windows.len())
    };
    let окна_w = if влезает > 0 {
        влезает as i32 * DOT + (влезает as i32 - 1) * GAP
    } else {
        0
    };

    let left_inner = tags + if окна_w > 0 { SEP_W + GAP * 2 + GAP + окна_w } else { 0 };
    let left = Rect { x: left_x, y, w: left_inner + PAD * 2, h: H };
    {
        let mut cx = left_x + PAD;
        for i in 0..TAGS {
            cells.push(Item {
                cell: Cell::Tag(1u32 << i),
                rect: Rect { x: cx, y, w: DOT, h: H },
            });
            cx += DOT + GAP;
        }
        // Последний зазор лишний — он ушёл в цикле после девятого стола.
        cx -= GAP;
        if окна_w > 0 {
            cells.push(Item {
                cell: Cell::Sep,
                rect: Rect { x: cx + GAP, y, w: SEP_W + GAP * 2, h: H },
            });
            // Сразу за разделителем: свой зазор у него уже есть с обеих сторон
            // (SEP_W + GAP*2). Лишний GAP здесь съедал бы поле у правого края
            // острова — ширина считается по этой же арифметике чуть выше.
            // Не всё влезло — последний слот занимает счётчик остатка. Ширина
            // острова при этом не меняется: слот тот же самый.
            let всего = d.windows.len();
            let (чипов, остаток) = if влезает < всего {
                (влезает.saturating_sub(1), всего - влезает.saturating_sub(1))
            } else {
                (влезает, 0)
            };
            let mut wx = cx + GAP + SEP_W + GAP * 2;
            for i in 0..чипов {
                cells.push(Item {
                    cell: Cell::Window(i),
                    rect: Rect { x: wx, y, w: DOT, h: H },
                });
                wx += DOT + GAP;
            }
            if остаток > 0 {
                cells.push(Item {
                    cell: Cell::WindowsMore(остаток),
                    rect: Rect { x: wx, y, w: DOT, h: H },
                });
            }
            влезает = чипов;
        }
    }

    // Сколько чипов показано и сколько не влезло, наружу отдельным полем НЕ
    // отдаём: это уже сказано ячейками (`Cell::Window(i)` и `Cell::WindowsMore`).
    // Второе представление того же числа — вторая копия геометрии, ровно то,
    // из-за чего в dawn однажды разъехались клики по окнам.
    let _ = влезает;
    Layout { left, center, right, cells }
}

/// «85%» — проценты рисуются одинаково у звука и у заряда.
pub fn percent_text(percent: u8) -> String {
    format!("{percent}%")
}

// ── Часы ─────────────────────────────────────────────────────────────────────
//
// Своего форматирования времени в std нет, а тащить chrono ради двух строк не
// хочется. libc уже в зависимостях (курсор, uinput), и localtime_r делает ровно
// то, что нужно: переводит epoch в местное время по TZ/etc/localtime.

pub(crate) fn local_time() -> Option<libc::tm> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: обе стороны — свои переменные на стеке, localtime_r пишет только
    // в `tm` и не хранит ссылок.
    let ok = unsafe { libc::localtime_r(&secs, &mut tm) };
    (!ok.is_null()).then_some(tm)
}

/// «14:32». Секунд намеренно нет: они будили бы перерисовку раз в секунду
/// круглые сутки, а панель и так висит поверх всего (см. anim::tick).
pub fn clock_text() -> String {
    match local_time() {
        Some(tm) => format!("{:02}:{:02}", tm.tm_hour, tm.tm_min),
        None => String::new(),
    }
}

/// «Пн 23.08».
pub fn date_text() -> String {
    const ДНИ: [&str; 7] = ["Вс", "Пн", "Вт", "Ср", "Чт", "Пт", "Сб"];
    match local_time() {
        Some(tm) => {
            let день = ДНИ[(tm.tm_wday.clamp(0, 6)) as usize];
            format!("{} {:02}.{:02}", день, tm.tm_mday, tm.tm_mon + 1)
        }
        None => String::new(),
    }
}

// ── Сторона композитора ──────────────────────────────────────────────────────

impl crate::state::Dawn {
    /// Всё, что панель показывает, собирается ЗДЕСЬ и одинаково идёт и в
    /// отрисовку, и в хит-тест.
    pub fn bar_data(&self) -> Data {
        Data {
            hide: self.bar_hide,
            screen_w: self.screen_size().w,
            windows: self.bar_windows(),
            clock: self.bar_clock.clone(),
            date: self.bar_date.clone(),
            kb: self.bar_kb.clone(),
            volume: self
                .audio_snapshot()
                .and_then(|s| s.volume())
                .map(|(level, muted)| ((level.clamp(0.0, 1.0) * 100.0).round() as u8, muted)),
            battery: self
                .tray
                .as_ref()
                .and_then(|t| t.snap.battery.as_ref())
                .map(|b| (b.percent, b.charging)),
            tray: self.sni_items().len(),
            // Имена переменных ЦЕЛИКОМ: односимвольные кириллические `р` и `г`
            // рядом с латинскими `p`/`r` из раскладки островов — та самая пара,
            // на которой rustc ругается «found both … which look alike», а
            // человек не видит разницы вовсе.
            share: self.раздача.as_ref().map(|раздача| {
                let гостей = раздача.гости.iter().filter(|гость| гость.впущен).count();
                (раздача.код.clone(), гостей)
            }),
        }
    }

    /// Окна текущего стола в том порядке, в каком они лежат в `tagged_windows`
    /// (то есть в порядке появления). Порядок обязан быть УСТОЙЧИВЫМ: чипы —
    /// это цель для клика, и переставлять их, скажем, по фокусу значило бы
    /// водить кнопку из-под пальца.
    pub fn bar_windows(&self) -> Vec<WindowChip> {
        let current = self.viewport.current_tags();
        let focused = self.focused_window();
        self.tagged_windows.iter()
            .filter(|tw| tw.tags & current != 0)
            .map(|tw| {
                let имя = crate::xwin::app_id(&tw.window)
                    .filter(|s| !s.trim().is_empty())
                    .or_else(|| crate::xwin::title(&tw.window))
                    .unwrap_or_default();
                let letter = имя.chars()
                    .find(|c| c.is_alphanumeric())
                    .map(|c| c.to_uppercase().to_string())
                    .unwrap_or_else(|| "?".into());
                WindowChip {
                    letter,
                    app_id: crate::xwin::app_id(&tw.window).unwrap_or_default(),
                    title: имя,
                    focused: focused.as_ref().is_some_and(|f| f == &tw.window),
                }
            })
            .collect()
    }

    /// Приготовить значок приложения для чипа этого окна.
    ///
    /// Зовётся на ПОЯВЛЕНИЕ окна и на смену его `app_id`, а не из отрисовки:
    /// поиск значка обходит каталоги темы значков, и первый же запуск нового
    /// приложения подвесил бы кадр (см. icons.rs). Результат — включая
    /// «значка нет» — кэшируется, поэтому повторный вызов ничего не стоит.
    pub fn ensure_chip_icon(&mut self, window: &smithay::desktop::Window) {
        let Some(app_id) = crate::xwin::app_id(window).filter(|s| !s.trim().is_empty()) else {
            return;
        };
        if self.chip_icons.contains_key(&app_id) {
            return;
        }
        let найденный = self.icon_cache
            .значок_приложения(&app_id, CHIP_ICON as u32)
            .cloned()
            // Среди установленных приложений значка нет — спрашиваем само окно.
            // У игр из Steam (`dota2`) и всего, что запускается мимо .desktop,
            // это единственный источник, и панель иначе рисует букву.
            .or_else(|| match window.underlying_surface() {
                smithay::desktop::WindowSurface::X11(s) => {
                    let значок = crate::icons::значок_окна_x11(s.window_id(), CHIP_ICON as u32);
                    tracing::debug!(
                        "dawn/icons: значок \"{}\" из _NET_WM_ICON — {}",
                        app_id, if значок.is_some() { "нашёлся" } else { "нет" },
                    );
                    значок
                }
                _ => None,
            });
        let Some(значок) = найденный else {
            return;
        };
        let буфер = smithay::backend::renderer::element::memory::MemoryRenderBuffer::from_slice(
            &значок.rgba,
            smithay::backend::allocator::Fourcc::Abgr8888,
            (значок.w, значок.h),
            1,
            smithay::utils::Transform::Normal,
            None,
        );
        self.chip_icons.insert(app_id, (буфер, (значок.w, значок.h)));
        self.request_redraw();
    }

    /// Окно под чипом номер `i` — по тому же списку, что и `bar_windows`.
    pub fn bar_window_at(&self, i: usize) -> Option<smithay::desktop::Window> {
        let current = self.viewport.current_tags();
        self.tagged_windows.iter()
            .filter(|tw| tw.tags & current != 0)
            .nth(i)
            .map(|tw| tw.window.clone())
    }

    /// Клик по чипу: камеру к окну, фокус — окну. Тем же порядком, что и в
    /// поиске (см. switcher::search_activate): камера идёт ПЕРВОЙ, потому что
    /// focus() поднимает и активирует окно, а лететь к нему всё равно надо.
    fn bar_window_focus(&mut self, i: usize) {
        let Some(w) = self.bar_window_at(i) else { return };
        self.snap_camera_to_window(&w);
        crate::xwin::focus(self, &w);
    }

    /// Средняя кнопка по чипу — закрыть окно, как в обычной панели задач.
    fn bar_window_close(&mut self, i: usize) {
        if let Some(w) = self.bar_window_at(i) {
            crate::xwin::close(&w);
        }
    }

    /// Короткое имя активной раскладки («RU»). Пересчитывается на смене
    /// раскладки и на перечитывании конфига, а не на каждый кадр: xkb-состояние
    /// живёт под мьютексом клавиатуры, и дёргать его из отрисовки — лишняя
    /// блокировка шестьдесят раз в секунду.
    ///
    /// Имя берётся из конфига (`xkb{ layout = "us,ru" }`), а не из xkb: тот
    /// зовёт раскладки «English (US)» и «Russian», и коротко их не сократить —
    /// «Ru» вышло бы и у русской, и у румынской.
    pub fn refresh_kb_layout(&mut self) {
        let раскладки: Vec<String> = self
            .lua_config
            .xkb
            .layout
            .split(',')
            .map(|s| s.trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        // Одна раскладка — показывать нечего.
        if раскладки.len() < 2 {
            self.bar_kb = String::new();
            return;
        }
        let Some(kb) = self.seat.get_keyboard() else { return };
        // Активная группа лежит в самом Xkb под мьютексом — XkbContext отдаёт
        // его наружу (менять состояние здесь незачем, только прочитать).
        let индекс = kb.with_xkb_state(self, |ctx| {
            ctx.xkb().lock().map(|x| x.active_layout().0 as usize).unwrap_or(0)
        });
        self.bar_kb = раскладки.get(индекс).cloned().unwrap_or_default();
    }

    /// Курсор поехал: обновить, над какой ячейкой панели он стоит.
    ///
    /// Зовётся на КАЖДОЕ движение, поэтому дешёвая часть (попал ли вообще в
    /// панель) идёт первой, а раскладка считается только при попадании. Кадр
    /// просим лишь на СМЕНЕ ячейки — иначе предпросмотр перерисовывал бы
    /// экран на каждое дрожание мыши.
    pub fn bar_hover_update(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Physical>) {
        let было = self.bar_hover;
        let стало = if self.fullscreen_here() || self.overview_active {
            None
        } else {
            let lay = layout(&self.bar_data());
            lay.hit(pos.x, pos.y)
        };
        // Курсор на самой КАРТОЧКЕ предпросмотра — она держится раскрытой (по
        // ней панят и зумят, как по карте, см. `Dawn::preview_*`). Считать это
        // надо ЗДЕСЬ: карточка стоит впритык под панелью, и путь мыши с ячейки
        // на карточку — это ровно один кадр, в котором `bar_hover` уже None.
        let на_карточке = стало.is_none() && self.preview_hover_zone(pos);
        if self.preview_hover != на_карточке {
            self.preview_hover = на_карточке;
            self.request_redraw();
        }
        if было != стало {
            self.bar_hover = стало;
            self.request_redraw();
        }
    }

    /// Клик по панели. `true` — клик съеден: под панелью окно, и промах по краю
    /// таблетки не должен уходить ему.
    pub fn bar_click(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Physical>, right: bool, middle: bool) -> bool {
        // Под полноэкранным окном панели на экране нет — значит нет и кликов
        // по ней. Условие ровно то же, что у отрисовки.
        if self.fullscreen_here() {
            return false;
        }
        let l = layout(&self.bar_data());
        let Some(cell) = l.hit(pos.x, pos.y) else {
            return l.inside(pos.x, pos.y);
        };
        match cell {
            Cell::Tag(mask) => {
                // Переход делаем не своим кодом, а тем же действием, что и
                // Super+цифра: у него своя логика для ленты, обзора и закладок
                // камеры, и вторая её копия здесь разъехалась бы с первой.
                if mask != self.viewport.current_tags() {
                    self.dispatch_action(crate::config::Action::ViewTag(mask));
                }
            }
            Cell::Kb => {
                self.dispatch_action(crate::config::Action::LayoutNext);
                self.refresh_kb_layout();
            }
            Cell::Volume if right => self.audio_send(crate::audio::Cmd::MuteToggle),
            Cell::Volume => self.audio_toggle_menu(),
            Cell::Tray(i) => self.sni_click(i, right, middle, pos.x as i32, pos.y as i32),
            Cell::Handle => self.tray_toggle(),
            Cell::Window(i) if middle => self.bar_window_close(i),
            Cell::Window(i) => self.bar_window_focus(i),
            // Часы, дата, заряд и счётчик остатка — только показ.
            // Остановка — только средним: раздачу выключают редко и намеренно,
            // а обычный клик по чипу случается, когда человек тычет в код,
            // диктуя его вслух.
            Cell::Share if middle => self.dispatch_action(crate::config::Action::ShareStop),
            Cell::Share => {}
            Cell::Clock | Cell::Date | Cell::Battery | Cell::Sep | Cell::WindowsMore(_) => {}
        }
        self.request_redraw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn окна(n: usize) -> Vec<WindowChip> {
        (0..n).map(|i| WindowChip {
            letter: ((b'A' + i as u8) as char).to_string(),
            app_id: format!("app{i}"),
            title: format!("Окно {i}"),
            focused: i == 0,
        }).collect()
    }

    fn данные() -> Data {
        Data {
            hide: 0.0,
            screen_w: 2560,
            windows: окна(4),
            clock: "14:32".into(),
            date: "Пн 23.08".into(),
            kb: "RU".into(),
            volume: Some((45, false)),
            battery: Some((85, true)),
            tray: 3,
            share: None,
        }
    }

    /// Чип раздачи влезает в правый остров и не выталкивает его за экран.
    /// Проверяется вместе с обычной раскладкой: остров справа прижат к краю и
    /// растёт ВЛЕВО, поэтому лишняя ячейка в нём — это первый кандидат на
    /// наезд на часы.
    #[test]
    fn чип_раздачи_не_ломает_островов() {
        let mut d = данные();
        d.share = Some(("123456".into(), 3));
        let l = layout(&d);
        let чип = l.cells.iter().find(|c| c.cell == Cell::Share).expect("чип раздачи есть");
        assert!(
            чип.rect.x >= l.right.x && чип.rect.x + чип.rect.w <= l.right.x + l.right.w,
            "чип {:?} вылез из острова {:?}", чип.rect, l.right,
        );
        assert!(l.right.x + l.right.w <= d.screen_w, "остров вылез за экран");
        let центр = l.center.expect("часы есть");
        assert!(центр.x + центр.w <= l.right.x, "правый остров наехал на часы");
        // И клик по чипу попадает именно в него.
        let (x, y) = (чип.rect.x as f64 + 2.0, чип.rect.y as f64 + 2.0);
        assert_eq!(l.hit(x, y), Some(Cell::Share));
    }

    /// Без гостей счётчик не показываем: «код 123456 · 0» читается как ошибка.
    #[test]
    fn подпись_раздачи_считает_гостей() {
        assert_eq!(share_text("123456", 0), "код 123456");
        assert_eq!(share_text("123456", 2), "код 123456 · 2");
    }

    /// Острова не налезают друг на друга и не вылезают за экран. Это главное
    /// свойство раскладки: заголовок окна растёт как хочет, а часы стоят точно
    /// по центру — единственное, что их разводит, это обрезка заголовка.
    #[test]
    fn острова_не_пересекаются_и_держатся_в_экране() {
        for screen_w in [1280, 1920, 2560, 3840] {
            let d = Data { screen_w, ..данные() };
            let l = layout(&d);
            assert!(l.left.x >= 0, "левый остров за краем: {:?}", l.left);
            assert!(
                l.right.x + l.right.w <= screen_w,
                "правый остров за краем: {:?} при экране {}", l.right, screen_w,
            );
            let центр = l.center.expect("часы есть всегда");
            assert!(
                l.left.x + l.left.w <= центр.x,
                "заголовок наехал на часы при экране {}: {:?} и {:?}",
                screen_w, l.left, центр,
            );
            assert!(
                центр.x + центр.w <= l.right.x,
                "часы наехали на правый остров при экране {}", screen_w,
            );
        }
    }

    /// Часы стоят ПО ЦЕНТРУ экрана, а не по центру свободного места: иначе они
    /// ездили бы туда-сюда от того, сколько окон открыто в соседнем острове.
    #[test]
    fn часы_ровно_по_центру_экрана() {
        for n in [0usize, 1, 5, 40] {
            let d = Data { windows: окна(n), ..данные() };
            let центр = layout(&d).center.expect("часы есть");
            let перекос = (центр.x + центр.w / 2) - d.screen_w / 2;
            assert!(перекос.abs() <= 1, "часы уехали на {перекос} px при {n} окнах");
        }
    }

    /// Чипы окон не лезут под часы: сколько не влезло — столько и не показано.
    /// Проверка ровно про то, ради чего `windows_shown` вообще существует —
    /// «влезает ли» решает раскладка, а не отрисовка.
    #[test]
    fn чипы_окон_не_наезжают_на_часы() {
        for (screen_w, n) in [(2560, 40), (1600, 12), (800, 6), (640, 30)] {
            let d = Data { screen_w, windows: окна(n), ..данные() };
            let l = layout(&d);
            let показано = l.cells.iter().filter(|i| matches!(i.cell, Cell::Window(_))).count();
            let остаток: usize = l.cells.iter()
                .filter_map(|i| match i.cell { Cell::WindowsMore(k) => Some(k), _ => None })
                .sum();
            assert!(показано <= n);
            assert!(остаток == 0 || показано + остаток == n, "счётчик остатка врёт: {показано}+{остаток} != {n}");
            // На совсем узком экране не влезают уже сами СТОЛЫ (девять значков
            // — это 228 px плюс поля), и остров налезает на часы ещё до всяких
            // окон. Это отдельная история; здесь проверяем ровно своё: чипы не
            // имеют права УХУДШИТЬ положение. Считаем от того же острова без них.
            let без_окон = layout(&Data { windows: Vec::new(), ..d.clone() });
            if показано == 0 && остаток == 0 {
                assert_eq!(l.left.w, без_окон.left.w, "нечего показывать — остров не растёт");
            }
            if let Some(центр) = l.center {
                if без_окон.left.x + без_окон.left.w <= центр.x {
                    assert!(
                        l.left.x + l.left.w <= центр.x,
                        "при {n} окнах на {screen_w}px левый остров наехал на часы",
                    );
                }
            }
            // Показанные чипы лежат ВНУТРИ своего острова и не пересекаются.
            let чипы: Vec<Rect> = l.cells.iter()
                .filter(|i| matches!(i.cell, Cell::Window(_)))
                .map(|i| i.rect)
                .collect();
            assert_eq!(чипы.len(), показано);
            for c in &чипы {
                assert!(c.x >= l.left.x && c.x + c.w <= l.left.x + l.left.w);
            }
            for пара in чипы.windows(2) {
                assert!(пара[0].x + пара[0].w <= пара[1].x, "чипы налезли друг на друга");
            }
        }
    }

    /// Номера чипов идут подряд от нуля: по ним отрисовка берёт данные окна, а
    /// клик — само окно (`bar_window_at`). Дырка в нумерации означала бы, что
    /// кликнули по одному окну, а сфокусировали другое.
    #[test]
    fn номера_чипов_идут_подряд() {
        let l = layout(&Data { windows: окна(7), ..данные() });
        let номера: Vec<usize> = l.cells.iter()
            .filter_map(|i| match i.cell { Cell::Window(n) => Some(n), _ => None })
            .collect();
        let показано = номера.len();
        assert_eq!(номера, (0..показано).collect::<Vec<_>>());
    }

    /// Клик по значку стола возвращает его тег, а по разделителю — ничего.
    #[test]
    fn клик_попадает_в_нужный_стол() {
        let l = layout(&данные());
        for i in 0..TAGS {
            let ячейка = l.cells.iter()
                .find(|c| c.cell == Cell::Tag(1u32 << i))
                .expect("стол на месте");
            let x = (ячейка.rect.x + ячейка.rect.w / 2) as f64;
            let y = (TOP + H / 2) as f64;
            assert_eq!(l.hit(x, y), Some(Cell::Tag(1u32 << i)));
        }
        let sep = l.cells.iter().find(|c| c.cell == Cell::Sep).expect("разделитель есть");
        let x = (sep.rect.x + sep.rect.w / 2) as f64;
        assert_eq!(l.hit(x, (TOP + H / 2) as f64), None, "разделитель съел клик");
    }

    /// Иконки трея нумеруются подряд и не наезжают друг на друга.
    #[test]
    fn иконки_трея_идут_подряд() {
        let l = layout(&Data { tray: 5, ..данные() });
        let иконки: Vec<Rect> = (0..5)
            .map(|i| l.cells.iter().find(|c| c.cell == Cell::Tray(i)).expect("иконка").rect)
            .collect();
        for пара in иконки.windows(2) {
            assert!(
                пара[0].x + пара[0].w <= пара[1].x,
                "иконки наехали: {:?} и {:?}", пара[0], пара[1],
            );
        }
        // Полка — всегда последняя ячейка правого острова, за иконками.
        let handle = l.cells.iter().find(|c| c.cell == Cell::Handle).expect("полка");
        assert!(handle.rect.x > иконки.last().unwrap().x);
        assert!(handle.rect.x + handle.rect.w <= l.right.x + l.right.w);
    }

    /// Пустые модули не оставляют дырок: без раскладки, звука и заряда правый
    /// остров сжимается до полосочки полки.
    #[test]
    fn правый_остров_сжимается_без_модулей() {
        let полный = layout(&данные()).right;
        let пустой = layout(&Data {
            kb: String::new(),
            volume: None,
            battery: None,
            tray: 0,
            ..данные()
        }).right;
        assert!(пустой.w < полный.w, "остров не сжался: {} против {}", пустой.w, полный.w);
        assert!(пустой.w > 0);
    }

    /// Каждая ячейка лежит ЦЕЛИКОМ внутри своего острова. Иначе получается то,
    /// что и было поймано этой проверкой: заголовок вставал на 6 px правее, чем
    /// заложено в ширину острова, и поле у правого края съедалось почти вдвое.
    #[test]
    fn ячейки_не_вылезают_из_островов() {
        for screen_w in [1280, 1920, 2560] {
            let d = Data { screen_w, ..данные() };
            let l = layout(&d);
            let острова: Vec<Rect> =
                [Some(l.left), l.center, Some(l.right)].into_iter().flatten().collect();
            for item in &l.cells {
                let внутри = острова.iter().any(|остров| {
                    item.rect.x >= остров.x
                        && item.rect.x + item.rect.w <= остров.x + остров.w
                        && item.rect.y >= остров.y
                        && item.rect.y + item.rect.h <= остров.y + остров.h
                });
                assert!(внутри, "ячейка {:?} вне островов при экране {}", item, screen_w);
            }
        }
    }

    /// Убранная панель обязана уйти за верхний край ЦЕЛИКОМ — вместе с полкой
    /// состояния, которая висит под ней. Проверять надо именно край, а не сам
    /// сдвиг: подняв панель ровно на её высоту, мы оставили бы у кромки
    /// полоску (та же грабля, что у выезда миникарты).
    ///
    /// Из этого же свойства даром получается хит-тест: убранная панель стоит
    /// выше нуля по Y, курсор туда не заходит, и клики она не ловит.
    #[test]
    fn убранная_панель_уходит_за_верхний_край() {
        let на_месте = layout(&Data { hide: 0.0, ..данные() });
        assert_eq!(на_месте.left.y, TOP);

        let убрана = layout(&Data { hide: 1.0, ..данные() });
        let полка_y = crate::tray::layout(true, true, 2560, 1.0, 1.0)
            .panel.map(|p| p.y + p.h)
            .unwrap_or(i32::MIN);
        assert!(убрана.left.y + убрана.left.h < 0, "панель торчит: {:?}", убрана.left);
        assert!(полка_y <= 0, "полка осталась на экране: низ {полка_y}");
        assert_eq!(убрана.hit(100.0, 1.0), None, "убранная панель ловит клики");

        // Ход монотонный, а доля вне [0,1] (перелёт анимации) панель дальше не
        // уносит.
        let половина = layout(&Data { hide: 0.5, ..данные() });
        assert!(убрана.left.y < половина.left.y && половина.left.y < на_месте.left.y);
        assert_eq!(layout(&Data { hide: 1.6, ..данные() }).left.y, убрана.left.y);
        assert_eq!(layout(&Data { hide: -0.3, ..данные() }).left.y, на_месте.left.y);
    }

    /// Часы и дата — это разбор строки, а не форматирование времени: проверяем
    /// форму, а не значение (оно зависит от того, когда гоняли тест).
    #[test]
    fn часы_и_дата_нужной_формы() {
        let clock = clock_text();
        assert_eq!(clock.len(), 5, "часы не ЧЧ:ММ: {clock:?}");
        assert_eq!(clock.as_bytes()[2], b':');
        let date = date_text();
        assert!(date.contains('.'), "в дате нет разделителя: {date:?}");
        assert_eq!(date.chars().count(), 8, "дата не «Пн 23.08»: {date:?}");
    }
}
