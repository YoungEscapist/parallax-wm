use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event,
        GestureBeginEvent, GestureEndEvent, GesturePinchUpdateEvent,
        GestureSwipeUpdateEvent, InputBackend, InputEvent,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, keysyms},
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, RelativeMotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
};

use std::time::{Duration, Instant};

use crate::{
    canvas::VelocityTracker,
    grabs::{move_grab::MoveSurfaceGrab, resize_grab::{ResizeEdge, ResizeSurfaceGrab}},
    state::Dawn,
    tiling::Layout,
};

const BTN_LEFT:  u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
/// Средний клик по значку трея — SecondaryActivate (см. sni.rs).
const BTN_MIDDLE: u32 = 0x112;

/// Скорость переноса: единицы скролла тачпада мельче пиксельной дельты мыши.
const TOUCHPAD_MOVE_SPEED: f64 = 2.5;

/// Пауза, после которой кадры тачпада считаются РАЗНЫМИ жестами. Внутри одного
/// жеста libinput шлёт кадры каждые ~7 мс, так что запас здесь двадцатикратный.
const TOUCHPAD_GESTURE_GAP: Duration = Duration::from_millis(150);

// ── Доводка курсора у края тачпада (edge motion) ─────────────────────────────
//
// Тачпад кончается раньше, чем экран: окно едет к дальнему краю, пальцы
// упираются в бортик панели, и перенос обрывается на полпути. Приходится
// отпускать, возвращать пальцы и начинать заново — а в раскладках, где отпускание
// что-то ЗАВЕРШАЕТ (вставка в ленту, обмен местами в тайлинге), ещё и с риском
// уронить окно не туда.
//
// Лечится это тем, что в X11 называлось EdgeMotion (драйвер xf86-input-synaptics,
// опции EdgeMotionMinSpeed/MaxSpeed/UseAlways): «когда палец доходит до края
// тачпада, указатель продолжает двигаться, пока палец не поднят», причём по
// умолчанию — только во время перетаскивания, чтобы не мешать обычной работе.
// libinput этой возможности не унаследовал, поэтому здесь она своя.
//
// Чего нам не хватает по сравнению с synaptics: тот видел АБСОЛЮТНУЮ позицию
// пальца на панели и её нажим, и потому просто проверял «палец в краевой зоне».
// Для скролла двумя пальцами libinput отдаёт одни лишь дельты — ни координат, ни
// давления в этих событиях нет. Значит, упор надо узнавать по косвенным
// признакам, и вот они:
//
//   * жест НЕ закончен — кадра с нулевой амплитудой, которым libinput отмечает
//     отпускание пальцев, ещё не было;
//   * до этого пальцы ехали быстро (см. `peak`) — то есть человек вёл окно, а
//     не подкручивал его по пикселю;
//   * а сейчас движение прекратилось: либо событий нет вовсе, либо приходит
//     мелкая дрожь от прижатого к бортику пальца.

/// Сколько «неподвижности» внутри идущего жеста считаем упором в край панели.
const EDGE_IDLE: Duration = Duration::from_millis(80);
/// Ниже этой скорости (canvas-px/с) кадр жеста считается не движением, а
/// дрожанием прижатого пальца: упор в бортик почти никогда не даёт чистого нуля,
/// палец продолжает елозить по последним миллиметрам панели.
///
/// Именно на этом доводка и ломалась раньше. Порог был один на всё, скорость
/// бралась из скользящего окна последних 80 мс, и эта самая дрожь вытесняла из
/// окна быстрые кадры: в логе 20260816_131327 видно, как за полсекунды упора
/// скорость сползает 352 → 253 и проваливается под порог, а каждый такой кадр
/// вдобавок сбрасывал разгон в ноль. Доводка успевала прожить один кадр из
/// шестидесяти — со стороны «не работает вообще».
const EDGE_MOVING: f64 = 200.0;
/// Кадр короче этого (canvas-px) не может считаться движением, какой бы скорость
/// ни показывало окно выборки.
///
/// Нужен ради отзывчивости, а не ради правильности. Окно выборки длиной 80 мс
/// после остановки пальца ещё столько же помнит быстрые кадры, и без этой
/// проверки упор замечался бы только через 80 мс окна + 80 мс простоя. С ней
/// дрожь отсекается сразу, и доводка трогается ровно через [`EDGE_IDLE`].
const EDGE_JITTER_STEP: f64 = 0.5;
/// Каким быстрым должен быть жест, чтобы доводка вообще включилась.
const EDGE_MIN_PEAK: f64 = 300.0;
/// Сколько холста жест обязан пройти до упора. Отсекает короткий тычок: у него
/// пиковая скорость бывает высокой, а вести после него нечего.
const EDGE_MIN_TRAVEL: f64 = 24.0;
/// Пик скорости живёт не дольше этого — иначе разгон в начале длинного жеста
/// задавал бы темп доводки через минуту после того, как человек замедлился.
const EDGE_PEAK_TTL: Duration = Duration::from_millis(500);
/// С какой скорости доводка стартует и за сколько разгоняется до пиковой.
/// Оба порога — прямой аналог EdgeMotionMinSpeed/MaxSpeed у synaptics; там темп
/// рос с нажимом пальца, здесь — со временем упора, потому что нажима нам не
/// показывают.
const EDGE_START_SPEED: f64 = 250.0;
const EDGE_RAMP: Duration = Duration::from_millis(200);
/// Потолок скорости доводки, canvas-px/с.
const EDGE_MAX_SPEED: f64 = 1200.0;
/// Предохранитель: дольше этого одна доводка не едет.
///
/// Конец жеста мы узнаём из кадра с нулевой амплитудой, который libinput шлёт
/// на отпускание пальцев. Если такой кадр по какой-то причине не придёт
/// (жест оборвали, устройство переоткрыли), доводка иначе тянула бы окно по
/// холсту бесконечно — а остановить её было бы уже нечем.
const EDGE_MAX_RUN: Duration = Duration::from_secs(5);

/// «1»…«9» → номер монитора с нуля. Берём и цифровой ряд, и цифры на
/// дополнительной клавиатуре: выбор монитора для демонстрации экрана делается
/// одной клавишей, и от того, включён ли NumLock, он зависеть не должен.
fn цифра_монитора(raw: u32) -> Option<usize> {
    let n = match raw {
        keysyms::KEY_1..=keysyms::KEY_9 => raw - keysyms::KEY_1,
        keysyms::KEY_KP_1..=keysyms::KEY_KP_9 => raw - keysyms::KEY_KP_1,
        _ => return None,
    };
    Some(n as usize)
}

/// Продолжение жеста, когда пальцы упёрлись в край тачпада и дальше не едут.
pub struct EdgeDrift {
    /// Скорость жеста по последним кадрам.
    vel: VelocityTracker,
    /// Самая высокая скорость, замеченная за последние [`EDGE_PEAK_TTL`], и
    /// направление в тот момент. Именно они задают темп и курс доводки.
    ///
    /// Пик, а не текущая скорость: у бортика палец физически ТОРМОЗИТ, и брать
    /// темп из последних кадров — значит брать его из самого торможения. Курс
    /// оттуда же и по той же причине: последние миллиметры палец ёрзает, и
    /// направление в них случайное.
    peak: f64,
    peak_dir: Point<f64, Logical>,
    peak_at: Instant,
    /// Когда пальцы в последний раз ДВИГАЛИСЬ (скорость выше [`EDGE_MOVING`]).
    moving_at: Instant,
    /// Сколько холста жест прошёл суммарно — против случайного тычка.
    travel: f64,
    /// Когда доводка фактически началась — от неё считается разгон.
    started: Option<Instant>,
    /// Про этот простой уже отчитались в лог — чтобы не сыпать 60 строк в
    /// секунду, пока пальцы стоят. Сбрасывается движением пальцев.
    reported: bool,
}

impl EdgeDrift {
    fn new(now: Instant) -> Self {
        Self {
            vel: VelocityTracker::new(),
            peak: 0.0,
            peak_dir: Point::from((0.0, 0.0)),
            peak_at: now,
            moving_at: now,
            travel: 0.0,
            started: None,
            reported: false,
        }
    }

    /// Учесть очередной кадр жеста.
    ///
    /// Ключевое здесь — что делает МЕДЛЕННЫЙ кадр. Он копится в скорости и в
    /// пройденном пути, но не объявляет пальцы движущимися и не сбрасывает
    /// разгон: у бортика панели палец не замирает намертво, он продолжает
    /// елозить, и если считать эту дрожь движением, доводка не проживёт ни
    /// одного кадра (см. EDGE_MOVING).
    ///
    /// `time` — отметка libinput в мс (не время обработки, см. VelocityTracker),
    /// `now` — часы для порогов простоя; в тестах оба задаются вручную.
    fn note(&mut self, step: Point<f64, Logical>, time: u32, now: Instant) {
        self.vel.push(time, step);
        let длина = (step.x * step.x + step.y * step.y).sqrt();
        self.travel += длина;
        // Дрожь прижатого пальца в скорость идёт (жест ею и заканчивается), а
        // движением не считается — см. EDGE_JITTER_STEP.
        if длина < EDGE_JITTER_STEP {
            return;
        }

        let v = self.vel.launch_velocity();
        let speed = (v.x * v.x + v.y * v.y).sqrt();
        if speed < EDGE_MOVING {
            return;
        }

        // Пальцы реально едут: доводка не нужна, а если шла — обрывается.
        self.moving_at = now;
        self.started = None;
        self.reported = false;
        // Пик обновляем либо когда побит, либо когда протух: иначе разгон в
        // начале длинного жеста задавал бы темп доводки через минуту после
        // того, как человек замедлился.
        if speed >= self.peak || now.duration_since(self.peak_at) > EDGE_PEAK_TTL {
            self.peak = speed;
            self.peak_dir = Point::from((v.x / speed, v.y / speed));
            self.peak_at = now;
        }
    }

    /// Что доводка хочет сделать в этот тик: шаг по холсту либо ничего.
    fn advance(&mut self, dt: Duration, now: Instant) -> Option<Point<f64, Logical>> {
        // Пальцы ещё едут сами — доводка не нужна.
        if now.duration_since(self.moving_at) < EDGE_IDLE {
            return None;
        }
        // Жест был слишком вялым или слишком коротким, чтобы его продолжать:
        // человек возит окно по мелочи или только коснулся панели.
        if self.peak < EDGE_MIN_PEAK || self.travel < EDGE_MIN_TRAVEL {
            // Один раз на простой, иначе это 60 строк в секунду.
            if !self.reported {
                self.reported = true;
                tracing::debug!(
                    "ДОВОДКА нет: простой={}мс пик={:.0} (нужно {}) путь={:.0} (нужно {})",
                    now.duration_since(self.moving_at).as_millis(),
                    self.peak, EDGE_MIN_PEAK, self.travel, EDGE_MIN_TRAVEL,
                );
            }
            return None;
        }
        let fresh = self.started.is_none();
        let started = *self.started.get_or_insert(now);
        if fresh {
            tracing::debug!(
                "ДОВОДКА старт: простой={}мс пик={:.0} курс=({:.2},{:.2}) путь={:.0}",
                now.duration_since(self.moving_at).as_millis(),
                self.peak, self.peak_dir.x, self.peak_dir.y, self.travel,
            );
        }
        let держится = now.duration_since(started);
        if держится > EDGE_MAX_RUN {
            if !self.reported {
                self.reported = true;
                tracing::debug!("ДОВОДКА стоп: предохранитель {:?}", EDGE_MAX_RUN);
            }
            return None;
        }
        // Плавный разгон от EDGE_START_SPEED к пиковой: рывка в момент упора
        // быть не должно, но и ползти после быстрого жеста доводка не обязана.
        let ramp = (держится.as_secs_f64() / EDGE_RAMP.as_secs_f64()).clamp(0.0, 1.0);
        let цель = self.peak.min(EDGE_MAX_SPEED);
        let speed = EDGE_START_SPEED + (цель - EDGE_START_SPEED).max(0.0) * ramp;
        let want = speed * dt.as_secs_f64();
        let step = Point::from((self.peak_dir.x * want, self.peak_dir.y * want));
        if step.x == 0.0 && step.y == 0.0 {
            return None;
        }
        Some(step)
    }
}

impl Dawn {
    /// Забирает ли окно в фокусе себе всю клавиатуру
    /// (`set{ keyboard_grab_apps = {...} }`).
    ///
    /// Сравнение без учёта регистра: класс окна пишут кто во что горазд, а
    /// человек в конфиге напишет так, как читает в заголовке.
    pub fn клавиши_забирает_окно(&self) -> bool {
        if self.lua_config.keyboard_grab_apps.is_empty() {
            return false;
        }
        let Some(окно) = self.focused_window() else { return false };
        let Some(класс) = crate::xwin::app_id(&окно) else { return false };
        self.lua_config
            .keyboard_grab_apps
            .iter()
            .any(|имя| имя.eq_ignore_ascii_case(&класс))
    }

    /// Можно ли сейчас двигать камеру жестами тачпада (пан и зум щипком).
    ///
    /// Запрещено в двух раскладках, где камера принадлежит не пользователю, а
    /// самой раскладке:
    ///   * Columns (niri) — это полоса колонок, а не бесконечный холст. Вид
    ///     ходит по ней свайпом (columns_swipe_scroll/workspace) и по вертикали
    ///     между столами; свободный пан позволял уехать в пустоту мимо колонок,
    ///     а зум щипком раскладку не проверял ВООБЩЕ и ломал её наравне с паном.
    ///     Свайп по полосе при этом остаётся — он отрабатывает раньше, в своей
    ///     ветке, и до камеры дело не доходит.
    ///   * Tile — окна разложены деревом под размер экрана, возить холст под
    ///     ними незачем.
    /// Во Float и Monocle пан и зум работают как раньше.
    ///
    /// Обзор столов и лупа (Super+Space) — исключение поверх всего: там вид уже
    /// оторван от раскладки и ходит свободно, какой бы она ни была.
    ///
    /// Мышь живёт по своим правилам и здесь не участвует: Alt+колесо зумит
    /// только во Float, Alt+ЛКМ панит во Float и в обзоре — те ветки не тронуты.
    pub(crate) fn touchpad_camera_allowed(&self) -> bool {
        if self.overview_active || self.zoom_nav_mode {
            return true;
        }
        !matches!(self.tile_config.layout, Layout::Tile | Layout::Columns)
    }

    /// Sloppy focus: фокус идёт за курсором (как `sloppyfocus=1` в dwl).
    ///
    /// Три случая, когда он ОБЯЗАН молчать, — и раньше они были разбросаны по
    /// двум веткам движения, каждая со своим неполным набором:
    ///
    ///   * **Columns** — вид ездит по полосе от клавиш и жестов, курсор при
    ///     этом стоит на месте экрана, и под стрелкой оказывается чужая
    ///     колонка. Первое же шевеление мыши перебрасывало фокус туда, куда
    ///     человек не смотрел. У niri focus-follows-mouse выключен по той же
    ///     причине.
    ///   * **Меню клиента держит захват** (XdgShellHandler::grab). Перевод
    ///     фокуса сорвал бы захват и закрыл меню — этим болели меню Steam.
    ///   * **Клавиатуру держит слой** — открыт fuzzel (лаунчер, строка поиска
    ///     dwall, меню его фильтров). Иначе первое же движение мыши отдаёт
    ///     ввод окну под курсором, слой теряет клавиатуру и ЗАКРЫВАЕТСЯ:
    ///     «строка поиска пропадает, стоит подвинуть мышь». В меню фильтров
    ///     dwall это выглядело как «фильтры не применяются» — выбрать пункт
    ///     мышью было физически нельзя, меню исчезало по дороге к нему.
    ///   * **Курсор над картой окон** — 25.08.2026, вместе с переездом карты из
    ///     угла в карточку во весь стол. Карта лежит ПОВЕРХ окон, и под ней
    ///     всегда чьё-то окно: без этой оговорки простое ведение мыши по карте
    ///     перебрасывало фокус на всё, над чем она проехала, — а рамка фокуса
    ///     на самой карте прыгала следом. Маленькая панель 320×200 в углу этим
    ///     почти не болела, большая карточка болеет постоянно.
    ///
    /// Проверка на слой стояла только в ветке АБСОЛЮТНОГО ввода (планшет, VM),
    /// а обычная мышь ходит через относительную — там её не было. Теперь
    /// правило одно на оба пути.
    pub(crate) fn sloppy_focus(&mut self, pos: Point<f64, Logical>, under: Option<&WlSurface>) {
        if self.tile_config.layout == Layout::Columns
            || self.layer_keyboard.is_some()
            || self.minimap_hit().is_some()
            || self.seat.get_keyboard().is_some_and(|k| k.is_grabbed())
        {
            return;
        }
        let Some(surface) = under else { return };
        if self.focused_surface().as_ref() == Some(surface) {
            return;
        }
        if let Some((window, _)) = self.space.element_under(pos).map(|(w, l)| (w.clone(), l)) {
            crate::xwin::focus(self, &window);
        }
    }

    /// Начать перетаскивание окна под курсором жестом «Super + два пальца».
    ///
    /// Собираем ровно тот же `MoveSurfaceGrab`, что и Win+ЛКМ (см. ветку
    /// Super+ЛКМ в PointerButton), поэтому дальше жест ведёт себя во всех
    /// раскладках так же, как мышь: в Tile меняет окна местами, в Columns
    /// показывает шов вставки и переносит между столами, во Float возит
    /// свободно и толкает соседей.
    fn start_touchpad_drag(&mut self) {
        let pos = self.pointer_location;
        let Some((window, loc)) = self.space.element_under(pos).map(|(w, l)| (w.clone(), l))
        else {
            return;
        };
        // Снимаем анимации с окна и со всех, кто поедет с ним: недоигранный
        // доезд от прошлого действия иначе каждый тик тянет окно на свою
        // траекторию, и под пальцами оно «резинится».
        self.freeze_window_anim(&window);
        let members = self.group_drag_members_excluding(&window);
        let mut group_initial = Vec::with_capacity(members.len());
        for m in members {
            self.freeze_window_anim(&m);
            if let Some(l) = self.space.element_location(&m) {
                group_initial.push((m, l));
            }
        }
        let focus = crate::xwin::surface(&window).map(|s| (s, loc.to_f64()));
        self.touchpad_drag = Some(MoveSurfaceGrab::new(
            GrabStartData { focus, button: BTN_LEFT, location: pos },
            window,
            loc,
            group_initial,
        ));
        self.request_plane_reset();
    }

    /// Запомнить шаг жеста для доводки у края (см. [`EdgeDrift::note`]).
    fn note_gesture_step(&mut self, step: Point<f64, Logical>, time: u32) {
        let now = Instant::now();
        self.edge_drift
            .get_or_insert_with(|| EdgeDrift::new(now))
            .note(step, time, now);
    }

    /// Продвинуть активный жест на `step` canvas-пикселей: курсор едет, а вместе
    /// с ним — перетаскиваемое окно или рамка выделения.
    ///
    /// Курсор ведём ЗА окном намеренно: жест таскает окно, и стрелка обязана
    /// оставаться в той же его точке, как при обычном драге мышью. Иначе окно
    /// уезжает из-под курсора, и следующий клик цепляет уже не его.
    fn gesture_advance(&mut self, step: Point<f64, Logical>, time: u32) {
        let want = Point::from((
            self.pointer_location.x + step.x,
            self.pointer_location.y + step.y,
        ));
        // Во Float курсор у края экрана «переливается» в камеру — ровно тем же
        // правилом, что и при движении настоящей мыши (см. PointerMotion).
        // Без этого жест упирался бы в границу экрана там, где мышь продолжает
        // тянуть холст из-под окна. В ленте и тайлинге камера принадлежит
        // раскладке, и трогать её жест не должен.
        if self.tile_config.layout == Layout::Float
            && !self.overview_active
            && !self.fullscreen_here()
        {
            let vis = self.visible_canvas_size();
            let sx = want.x - self.viewport.cam_x;
            let sy = want.y - self.viewport.cam_y;
            let over_x = sx - sx.clamp(0.0, vis.w);
            let over_y = sy - sy.clamp(0.0, vis.h);
            if over_x != 0.0 || over_y != 0.0 {
                let zoom = self.viewport.zoom;
                self.viewport.cam_x += over_x * 0.6 / zoom;
                self.viewport.cam_y += over_y * 0.6 / zoom;
                self.apply_camera();
            }
        }
        self.warp_pointer(want);
        let cursor = self.pointer_location;
        // take/вернуть: drag_to берёт весь Dawn, а grab лежит внутри него.
        if let Some(mut grab) = self.touchpad_drag.take() {
            grab.drag_to(self, cursor, time);
            self.touchpad_drag = Some(grab);
        } else if let Some(start) = self.touchpad_select_start {
            self.selection_drag = Some(crate::grabs::rect_from_points(start, cursor));
        }
        self.request_redraw();
    }

    /// Один шаг доводки курсора у края тачпада. Зовётся из `anim::tick` (~60 Гц).
    pub fn edge_drift_tick(&mut self, dt: Duration) {
        // Доводить нечего: ни окна на пальцах, ни рамки выделения.
        if self.touchpad_drag.is_none() && self.touchpad_select_start.is_none() {
            self.edge_drift = None;
            return;
        }
        let Some(drift) = self.edge_drift.as_mut() else { return };
        let Some(step) = drift.advance(dt, Instant::now()) else { return };
        let time = self.start_time.elapsed().as_millis() as u32;
        self.gesture_advance(step, time);
    }

    pub(crate) fn kill_focused(&mut self) {
        if let Some(surface) = self.focused_surface() {
            // Ищем сперва в space, потом в собственном списке окон.
            //
            // space держит только то, что сейчас РИСУЕТСЯ: скрытые вкладки
            // Columns с него сняты, окна чужих раскладок и чужих тегов тоже.
            // Фокус же спокойно оказывается на таком окне (после закрытия
            // соседа, после переключения вкладки), и Win+Q молча не делал
            // ничего — в логе 20260729_190042 это 28 промахов на 27 попаданий,
            // причём промах всегда стоит вплотную перед повторным нажатием.
            // Закрытие окна не должно зависеть от того, видно его сейчас или нет.
            let w = self.space.elements()
                .find(|w| crate::xwin::is_surface(w, &surface))
                .cloned()
                .or_else(|| self.tagged_windows.iter()
                    .find(|tw| crate::xwin::is_surface(&tw.window, &surface))
                    .map(|tw| tw.window.clone()));
            if let Some(w) = w {
                crate::xwin::close(&w);
                tracing::info!("dawn: kill focused");
            } else {
                tracing::info!("dawn: kill — фокусная поверхность есть, но окна для неё нет ни в space, ни в списке");
            }
        } else {
            // Win+q молча ничего не делает ровно здесь: клавиатурный фокус снят
            // (например, кликом по пустому холсту, см. ниже set_focus(None)).
            tracing::info!("dawn: kill — фокуса нет, закрывать нечего");
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial    = SERIAL_COUNTER.next_serial();
                let time      = Event::time_msec(&event);
                let key_state = event.state();
                let keycode   = event.key_code();
                // Трекаем Super вручную (как driftwm для logo_held)
                let pressed = key_state == smithay::backend::input::KeyState::Pressed;
                // XKB keysyms для Super
                const SUPER_L: u32 = keysyms::KEY_Super_L;
                const SUPER_R: u32 = keysyms::KEY_Super_R;

                self.seat.get_keyboard().unwrap().input::<(), _>(
                    self,
                    keycode,
                    key_state,
                    serial,
                    time,
                    |state, modifiers, handle| {
                        разобрать_клавишу(state, modifiers, handle, pressed, false)
                    },
                );
                // Любое нажатие клавиши → обновляем экран (переключение тегов не лагает)
                self.request_redraw();
            }

            InputEvent::PointerMotion { event, .. } => {
               let delta = event.delta();
               tracing::trace!("PTR MOTION: delta=({:.2},{:.2})", delta.x, delta.y);
               let zoom = self.viewport.zoom;

               // ── Захват курсора приложением (см. capture.rs) ───────────────
               // Относительные дельты уходят клиенту ВСЕГДА, а не только при
               // захвате: игры подписываются на них заранее и ведут по ним
               // обзор, пока курсор ещё свободен.
               //
               // Ускоренная дельта делится на зум — она в тех же единицах, что
               // и движение курсора по холсту (surface-локальных). Сырая
               // (delta_unaccel) идёт как есть: это «сколько прошла мышь»,
               // масштаб холста к ней отношения не имеет, и игры считают по ней
               // своё ускорение.
               let под = self.surface_under(self.pointer_location);
               let захват = self.pointer_constraint_at(под.as_ref());
               {
                   let pointer = self.seat.get_pointer().unwrap();
                   let unaccel = event.delta_unaccel();
                   pointer.relative_motion(self, под, &RelativeMotionEvent {
                       delta: (delta.x / zoom, delta.y / zoom).into(),
                       delta_unaccel: (unaccel.x, unaccel.y).into(),
                       utime: event.time(),
                   });
                   // Курсор заперт на месте (мышиный обзор в игре): позицию не
                   // трогаем вовсе — ни стрелку, ни камеру. Клиент уже получил
                   // всё, что ему нужно, дельтами выше.
                   if захват.locked {
                       pointer.frame(self);
                       return;
                   }
               }

               // Что под курсором на панели — от этого зависит предпросмотр.
               // Считается до всех веток: панель обязана гасить подсказку и
               // тогда, когда курсор просто проехал мимо неё дальше.
               //
               // Позиция берётся БУДУЩАЯ (с уже прибавленной дельтой), а не
               // текущая. Раньше сюда шла `pointer_screen_physical()` до сдвига
               // ниже, и наведение отставало ровно на одно событие мыши: замер
               // 24.08.2026 синтетическим вводом (одно событие на прыжок)
               // показал это в чистом виде — курсор стоит на чипе окна, а
               // панель отвечает «значок стола», то есть подсказка соответствует
               // ПРЕДЫДУЩЕЙ точке. С живой мышью событий сотни в секунду, и
               // отставание видно лишь на кромке ячейки (предпросмотр моргал
               // при въезде на чип), но это тот же баг.
               // Экран здесь = (холст − камера) × зум, поэтому дельта попадает
               // в экранные пиксели как есть, а зажим повторяет тот, что ниже
               // делает сам курсор.
               {
                   let сейчас = self.pointer_screen_physical();
                   let экран = self.screen_size();
                   self.bar_hover_update(smithay::utils::Point::from((
                       (сейчас.x + delta.x).clamp(0.0, экран.w as f64),
                       (сейчас.y + delta.y).clamp(0.0, экран.h as f64),
                   )));
               }

               // Драг по карточке предпросмотра и по карте окон — их
               // собственный пан. Стоит РАНЬШЕ пана холста: обе живут отдельно
               // от камеры, и их драг не имеет права заодно уносить холст.
               //
               // Курсор при этом ЕДЕТ ВМЕСТЕ с содержимым (26.08.2026, прямая
               // жалоба «курсор при пане миникарты остаётся на месте и
               // статичен»). Раньше здесь стоял голый `return`: позиция курсора
               // ниже по функции не менялась вовсе, стрелка примерзала к экрану,
               // а карта уезжала из-под неё — схваченный кусок мира убегал от
               // руки. У обычного пана холста (Alt+ЛКМ ниже) поведение ровно
               // обратное: `pan_camera_by` двигает камеру, оставляя стрелку на
               // той же точке ХОЛСТА, то есть на экране она идёт за рукой.
               // Здесь то же самое, только «холст» — содержимое мини-копии.
               if self.preview_drag_motion(delta.x, delta.y) {
                   self.drag_pointer_by_screen(delta.x, delta.y);
                   return;
               }
               if self.minimap_drag_motion(delta.x, delta.y) {
                   self.drag_pointer_by_screen(delta.x, delta.y);
                   return;
               }

               // Alt+LMB pan (Float) / ЛКМ-пан в обзоре столов: курсор стоит,
               // холст движется в сторону drag.
               if self.pan_button_held
                   && (self.tile_config.layout == Layout::Float || self.overview_active)
               {
                   let dcam_x = delta.x / zoom;
                   let dcam_y = delta.y / zoom;
                   // Камера + курсор одним шагом: стрелка обязана остаться в
                   // той же точке экрана уже в ЭТОМ кадре (см. pan_camera_by).
                   self.pan_camera_by(dcam_x, dcam_y);
                   // Кинетический скролл (1.1): копим дельту для инерции на отпускание
                   self.momentum.accumulate(
                       smithay::utils::Point::from((-dcam_x, -dcam_y)),
                       event.time_msec(),
                   );
                   // Замер строго на ЭТОМ жесте (Alt+ЛКМ): печатаем экранную
                   // точку стрелки на каждом событии пана, не чаще 60 строк за
                   // жест. Если тут число стоит, а стрелку видно едущей —
                   // причина не в позиции курсора.
                   if self.pan_log_left > 0 {
                       self.pan_log_left -= 1;
                       let s = self.pointer_screen_physical();
                       tracing::debug!(
                           "ПАН Alt+ЛКМ: курсор_экран=({:.1},{:.1}) камера=({:.1},{:.1}) дельта=({:.1},{:.1})",
                           s.x, s.y, self.viewport.cam_x, self.viewport.cam_y, delta.x, delta.y,
                       );
                   }
                   self.request_redraw();
                   return;
               }

               // Обычное движение курсора — дельта в canvas-единицах
               let было = self.pointer_location;
               self.pointer_location.x += delta.x / zoom;
               self.pointer_location.y += delta.y / zoom;

               // Курсор не должен выходить за экран: зажимаем, переливаем в камеру.
               // Координаты в ЛОГИЧЕСКИХ единицах (output-local), без умножения на zoom.
               {
                   {
                       // Логическая позиция курсора относительно камеры и предел —
                       // ВИДИМАЯ часть холста (экран ⁄ зум), а не размер выхода:
                       // при отдалении в кадре холста больше, чем экрана.
                       let sx = self.pointer_location.x - self.viewport.cam_x;
                       let sy = self.pointer_location.y - self.viewport.cam_y;
                       let vis = self.visible_canvas_size();
                       // ── Переход на соседний монитор ───────────────────────
                       // Вышли за край, а с той стороны стоит другой монитор —
                       // значит это не «упёрлись в стену», а переезд. Раньше
                       // здесь был только зажим, и на второй монитор курсор не
                       // попадал в принципе: он останавливался у кромки
                       // первого.
                       //
                       // Сторону выбираем по БОЛЬШЕМУ выходу за край: по
                       // диагонали к углу вылезают обе оси сразу, и без этого
                       // правила переход зависел бы от порядка проверок.
                       //
                       // Пока держится кнопка — не переходим вовсе (жалоба
                       // Ярика 26.08.2026: «курсор при удержании в одном
                       // приложении улетает на другой монитор»). Начатый жест
                       // принадлежит одному окну: выделение текста, ползунок,
                       // перетаскивание. Уехавшая за край стрелка меняет
                       // активный монитор и уводит клавиатурный фокус
                       // (`перевести_курсор` зовёт `refocus_visible`) — жест
                       // рвётся на середине, а окно так и остаётся с зажатой
                       // кнопкой. Вместо перехода курсор зажимается краем,
                       // как у одного монитора; отпустил — край снова
                       // проходной.
                       //
                       // Confine клиента (RTS-игры вроде Dota 2 гоняют камеру
                       // краем экрана мышью — им это ОБЯЗАН быть именно
                       // confine, а не lock) отменяет переход целиком: иначе
                       // курсор, доехав до края игрового окна, перепрыгивал
                       // на второй монитор раньше, чем строка 856 успевала
                       // проверить `захват.holds` — переход через
                       // `перевести_курсор` завершается ранним `return`, и
                       // проверка confine ниже попросту не выполнялась.
                       // Исключение из правила «под кнопкой не переходим» —
                       // перетаскивание ОКНА (`dragged_window`). Здесь зажатая
                       // кнопка означает ровно обратное: жест не рвётся краем, а
                       // им и заканчивается — окно переносят на соседний экран.
                       // Само окно переезжает на стол нового монитора в
                       // `перевести_курсор`.
                       if !self.мониторы.is_empty()
                           && (self.кнопок_нажато == 0 || self.dragged_window.is_some())
                           && !захват.confined
                       {
                           let вылет = [
                               (crate::monitors::Сторона::Слева,  -sx),
                               (crate::monitors::Сторона::Справа, sx - vis.w),
                               (crate::monitors::Сторона::Сверху, -sy),
                               (crate::monitors::Сторона::Снизу,  sy - vis.h),
                           ];
                           let худший = вылет.iter()
                               .filter(|(_, d)| *d > 0.0)
                               .max_by(|a, b| a.1.total_cmp(&b.1))
                               .map(|(с, _)| *с);
                           if let Some(сторона) = худший {
                               let доля = match сторона {
                                   crate::monitors::Сторона::Слева
                                   | crate::monitors::Сторона::Справа => sy / vis.h.max(1.0),
                                   _ => sx / vis.w.max(1.0),
                               };
                               if self.перевести_курсор(сторона, доля) {
                                   // Стрелка уже переставлена и motion разослан
                                   // самим переводом — здесь больше нечего
                                   // делать, иначе зажим ниже вернул бы её на
                                   // покинутый монитор.
                                   return;
                               }
                           }
                       }
                       let ow = vis.w;
                       let oh = vis.h;
                       let csx = sx.clamp(0.0, ow);
                       let csy = sy.clamp(0.0, oh);
                       // Зажимаем курсор (canvas coords)
                       self.pointer_location.x = csx + self.viewport.cam_x;
                       self.pointer_location.y = csy + self.viewport.cam_y;
                       // Перелив → плавное движение камеры (только Float).
                       //
                       // Полноэкранное окно — исключение: экран отдан ему
                       // целиком, и «дотолкать» камеру курсором у края значило
                       // бы уехать холстом из-под игры прямо во время игры.
                       // Пока фуллскрин держит экран, камера стоит.
                       if self.tile_config.layout == Layout::Float
                           && !self.fullscreen_here()
                       {
                           let over_x = sx - csx;
                           let over_y = sy - csy;
                           if over_x != 0.0 || over_y != 0.0 {
                               // Скорость pan пропорциональна зуму: при zoom больше — viewport
                               // меньше, поэтому pan нужен медленнее чтобы не улетать.
                               // Коэффициент 0.6 (было 0.2): у края курсор ощутимо
                               // меньше "тормозит", холст тянется за ним быстрее.
                               self.viewport.cam_x += over_x * 0.6 / zoom;
                               self.viewport.cam_y += over_y * 0.6 / zoom;
                               self.apply_camera();
                           }
                       }
                   }
               }

               // Удержание (confine): курсор не выпускается за поверхность и за
               // заданную клиентом область внутри неё. Если шаг вывел наружу —
               // откатываем его целиком. Скользить вдоль границы не пытаемся:
               // область произвольной формы, а откат по обеим осям сразу даёт
               // ровно то поведение, которого ждёт клиент — «дальше некуда».
               if захват.confined && !захват.holds(self, self.pointer_location) {
                   self.pointer_location = было;
               }

               // Пока тянется рамка снимка, движение никому не рассылается:
               // клиенту оно не нужно (кнопка у выделения), а sloppy focus на
               // ходу переключал бы окна под затемнением. Позиция уже
               // обновлена выше — её и читает отрисовка рамки, отдельного
               // состояния «докуда дотянули» для этого не нужно.
               if self.snip_идёт() {
                   self.pointer_warped();
                   self.request_redraw();
                   return;
               }

               let pos = self.pointer_location;
               let serial = SERIAL_COUNTER.next_serial();
               let pointer = self.seat.get_pointer().unwrap();
               let under = self.surface_under(pos);

               // Sloppy focus — но НЕ в Columns: там вид ездит по полосе от
               // клавиш и жестов, курсор при этом стоит на месте экрана (это
               // видно в логе: «КАДР: курсор_экран» неизменен, пока привязка
               // окон уезжает), и под стрелкой оказывается чужая колонка. С
               // sloppy focus первое же шевеление мыши перебрасывало фокус
               // туда, куда пользователь не смотрел. У niri focus-follows-mouse
               // по той же причине выключен по умолчанию; остальные раскладки
               // dawn работают как раньше.
               // Пока открыто меню клиента, оно держит захват (см.
               // XdgShellHandler::grab). Перевод фокуса под курсором в этот
               // момент сорвал бы захват и закрыл меню — ровно то, чем болели
               // меню Steam (там причина была та же, но по стороне X11).
               self.sloppy_focus(pos, under.as_ref().map(|(s, _)| s));
               // Surface-local всегда в логическом пространстве — клиент сам
               // умножает на scale (wl_output.scale / wp_fractional_scale),
               // чтобы получить пиксель буфера. zoom_adjusted_location_motion
               // дважды применял scale — surface-local уходил сжатым.
               pointer.motion(self, under, &MotionEvent {
                   location: pos, serial, time: event.time_msec(),
               });
               pointer.frame(self);
               // Это движение — от настоящей мыши, и оно уже разослано. Фиксируем
               // новую экранную позицию как эталонную, иначе sync_pointer_to_camera
               // в конце итерации сочтёт сдвиг камеры от "перелива" у края (выше)
               // самовольным и оттащит стрелку обратно — курсор резинил бы у края.
               self.pointer_warped();
               // Курсор въехал в область, на которую клиент заранее попросил
               // захват, — включаем его (до этого ограничение висит неактивным).
               self.activate_pointer_constraint();
               // Курсор client-side — его позицию перерисовывает только сам
               // рендер; без явного пинка тут курсор будет виден в последнем
               // отрендеренном кадре, а не там, где мышь реально находится.
               self.request_redraw();
           }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                tracing::trace!("PTR MOTION ABS");
                // Абсолютная позиция (планшет/тачскрин) приходит в долях экрана —
                // разворачиваем её именно по ЭКРАНУ, а дальше переводим в холст
                // общей формулой. Раньше здесь брался размер выхода, который
                // сам уже был поделён на зум, и деление на зум ниже давало
                // двойную поправку: абсолютный ввод мазал тем сильнее, чем
                // дальше зум от единицы.
                let zoom = self.viewport.zoom;
                let cam_x = self.viewport.cam_x;
                let cam_y = self.viewport.cam_y;
                // Курсор заперт клиентом (см. capture.rs) — абсолютный ввод его
                // не двигает: планшет или указатель виртуальной машины иначе
                // выдернул бы стрелку из захвата, и игра потеряла бы обзор.
                let под = self.surface_under(self.pointer_location);
                let захват = self.pointer_constraint_at(под.as_ref());
                if захват.locked {
                    return;
                }
                let screen_pos = event.position_transformed(self.screen_size());
                let mut pos = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                    screen_pos.x / zoom + cam_x,
                    screen_pos.y / zoom + cam_y,
                ));
                // Удержание внутри поверхности: точку вне её просто не
                // принимаем — стрелка остаётся там, где была.
                if захват.confined && !захват.holds(self, pos) {
                    pos = self.pointer_location;
                }
                self.pointer_location = pos;
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
                let under = self.surface_under(pos);

                self.sloppy_focus(pos, under.as_ref().map(|(s, _)| s));

                pointer.motion(self, under, &MotionEvent {
                    location: pos, serial, time: event.time_msec(),
                });
                pointer.frame(self);
                // Абсолютный ввод (планшет/VM) задаёт позицию курсора прямо в
                // экранных координатах — она и есть эталон для синхронизации.
                self.pointer_warped();
                self.activate_pointer_constraint();
                self.request_redraw();
            }

            InputEvent::PointerButton { event, .. } => {
                let pointer    = self.seat.get_pointer().unwrap();
                let keyboard   = self.seat.get_keyboard().unwrap();
                let serial     = SERIAL_COUNTER.next_serial();
                let button     = event.button_code();
                let btn_state  = event.state();
                let kb_mods = keyboard.modifier_state();
                let alt_held = kb_mods.alt;

                // Счётчик удерживаемых кнопок — ДО любых ранних выходов: ниже
                // клик перехватывают меню, полка и обзор, и после каждого из
                // них стоит `return`. Считать позже значило бы, что нажатие,
                // съеденное компоновщиком, навсегда оставит счётчик
                // рассогласованным (кнопку-то отпустят). По нему запрещён
                // переход курсора на соседний монитор — см. движение мыши.
                if ButtonState::Pressed == btn_state {
                    self.кнопок_нажато += 1;
                } else {
                    self.кнопок_нажато = self.кнопок_нажато.saturating_sub(1);
                }

                tracing::debug!(
                    "PTR: button={} state={:?} logo_held={} kb_logo={} kb_alt={}",
                    button,
                    btn_state,
                    self.logo_held,
                    kb_mods.logo,
                    kb_mods.alt,
                );

                // Курсор захвачен приложением (игра запросила pointer-lock или
                // confine, см. capture.rs) — весь ввод принадлежит ему, и ни
                // один оверлей компоновщика клик не перехватывает. При locked
                // стрелка стоит там, где её заперли: если это оказалась зона
                // полки или миникарты, КАЖДЫЙ выстрел в игре уходил бы в
                // компоновщик, а не в игру.
                let курсор_у_клиента = {
                    let под = self.surface_under(self.pointer_location);
                    let захват = self.pointer_constraint_at(под.as_ref());
                    захват.locked || захват.confined
                };

                // Идёт выбор источника для демонстрации экрана: клик выбирает
                // окно под курсором (пустой холст = весь экран), правая кнопка
                // отменяет. Съедаем клик целиком — он не должен ни
                // фокусировать, ни двигать окна. См. portal.rs.
                // Меню блютуза приклеено к экрану, поэтому и клик по нему
                // считается в экранных пикселях (см. bt_click).
                // Замер «клик не сработал»: каждый выход ОТСЮДА означает, что
                // приложение своего клика не увидело. Пока причина промахов не
                // найдена, важно отличать «клик съел компоновщик» (тогда виден
                // виновник и точка экрана) от «клик дошёл, но приложение его
                // проигнорировало» — во втором случае здесь тихо.
                let съел = |кто: &str, s: smithay::utils::Point<f64, smithay::utils::Physical>| {
                    tracing::info!("КЛИК СЪЕДЕН: {} экран=({:.0},{:.0})", кто, s.x, s.y);
                };

                // Выделение области для снимка экрана — ПЕРВЫМ и без оглядки на
                // `курсор_у_клиента`: пока рамка тянется, мышь целиком
                // принадлежит ей. Это единственный оверлей, который человек
                // включает сам и ровно на один жест, поэтому отдавать кнопку
                // ни окну, ни панели нельзя — иначе протяжка по окну заодно
                // выделила бы в нём текст. См. snip.rs.
                if self.snip_идёт() {
                    let нажата = ButtonState::Pressed == btn_state;
                    if self.snip_click(button == BTN_LEFT, нажата) {
                        съел("выделение снимка", self.pointer_screen_physical());
                        return;
                    }
                }

                if ButtonState::Pressed == btn_state && !курсор_у_клиента && self.bt_menu_open() {
                    let screen = self.pointer_screen_physical();
                    if self.bt_click(screen) {
                        съел("меню блютуза", screen);
                        return;
                    }
                }

                // Поиск окон (Super+F) — такое же приклеенное к экрану меню.
                if ButtonState::Pressed == btn_state && !курсор_у_клиента && self.search_open() {
                    let screen = self.pointer_screen_physical();
                    if self.search_click(screen) {
                        съел("поиск окон", screen);
                        return;
                    }
                }

                // Меню вайфая и звука приклеены к экрану, как и блютузное.
                if ButtonState::Pressed == btn_state && !курсор_у_клиента
                    && (self.wifi_menu_open() || self.audio_menu_open()) {
                    let screen = self.pointer_screen_physical();
                    if self.wifi_click(screen) || self.audio_click(screen) {
                        съел("меню вайфая/звука", screen);
                        return;
                    }
                }

                // Полка состояния приклеена к экрану так же, как бар. Клик
                // мимо неё она НЕ съедает (см. tray_click) — окно под курсором
                // получит его как обычно. Правая кнопка по значку — быстрое
                // действие вместо меню (радио, немота).
                if ButtonState::Pressed == btn_state && !курсор_у_клиента {
                    let screen = self.pointer_screen_physical();
                    if self.tray_click(screen, button == BTN_RIGHT) {
                        съел("полка состояния", screen);
                        return;
                    }
                }

                // Панель приклеена к экрану так же, как полка рядом: клик по
                // столу переводит на него, по значку трея — будит приложение.
                // Мимо панели клик не съедается — окно под ней получит его как
                // обычно.
                //
                // Кнопку передаём целиком, а не только левую: значку трея нужны
                // все три (Activate, ContextMenu, SecondaryActivate), и правый
                // клик по звуку глушит его без открытия меню.
                if ButtonState::Pressed == btn_state && !курсор_у_клиента {
                    let screen = self.pointer_screen_physical();
                    if self.bar_click(screen, button == BTN_RIGHT, button == BTN_MIDDLE) {
                        съел("панель", screen);
                        return;
                    }
                }

                if ButtonState::Pressed == btn_state && self.portal_picking() {
                    if self.portal_pick_click(button == BTN_RIGHT) {
                        съел("выбор источника демонстрации", self.pointer_screen_physical());
                        return;
                    }
                }

                // ── Карточка предпросмотра ───────────────────────────────────
                // С 26.08.2026 она — такая же мини-копия мира, как карта, и
                // разбирает клики теми же правилами: ЛКМ по миниатюре — перейти
                // к окну (со сменой стола, если оно на чужом), ЛКМ по пустому
                // месту — пан самой карточки, ПКМ — сброс её вида. Стоит ПЕРЕД
                // картой: карточка висит поверх, и под ней вполне может лежать
                // раскрытая карта окон.
                if !курсор_у_клиента {
                    if ButtonState::Pressed == btn_state {
                        if let Some(точка) = self.preview_hit() {
                            if button == BTN_RIGHT {
                                // ПКМ — «покажи стол целиком»: вместе с видом
                                // забываем и запомненное место, иначе карточка
                                // вернулась бы туда же следующим наведением.
                                self.preview_забыть_вид();
                                self.preview_reset_view();
                                self.request_redraw();
                            } else if !self.preview_activate(точка) {
                                self.preview_begin_drag();
                            }
                            съел("предпросмотр: нажатие", self.pointer_screen_physical());
                            return;
                        }
                    } else if self.preview_drag {
                        self.preview_end_drag();
                        съел("предпросмотр", self.pointer_screen_physical());
                        return;
                    }
                }

                // ── Карта окон ───────────────────────────────────────────────
                // Своя мини-копия мира. ЛКМ по МИНИАТЮРЕ ОКНА — перейти к нему
                // (фокус + перелёт камеры + карта уезжает); ЛКМ по ПУСТОМУ
                // месту — пан самой карты, камеру он не трогает; ПКМ — сброс к
                // автоподгонке. Событие карта ЗАБИРАЕТ себе целиком: под ней
                // холст и окна, и тычок в карту не должен фокусировать
                // спрятанное под ней окно или начинать рамку выделения.
                //
                // Хит-тест окна стоит ПЕРЕД драгом нарочно: карта теперь во
                // весь стол, окон в ней много, и «промахнулся по окну — поехал
                // пан» читается гораздо естественнее обратного порядка.
                if !курсор_у_клиента {
                    if ButtonState::Pressed == btn_state {
                        if let Some(точка) = self.minimap_hit() {
                            if button == BTN_RIGHT || self.minimap_reset_button_hit(точка) {
                                self.minimap_reset();
                            } else if !self.minimap_activate(точка) {
                                self.minimap_begin_drag();
                            }
                            съел("карта окон: нажатие", self.pointer_screen_physical());
                            return;
                        }
                    } else if self.minimap_drag {
                        self.minimap_end_drag();
                        съел("карта окон", self.pointer_screen_physical());
                        return;
                    }
                }

                // Любой клик отменяет ожидающий тап Super (обзор столов).
                if ButtonState::Pressed == btn_state {
                    self.super_tap = false;
                }

                // ── Обзор столов ──────────────────────────────────────────────
                //  · ПКМ → выйти на стол под курсором
                //  · ЛКМ → фокус на окне (основной хендлер), потом exit на стол
                //  · Alt+ЛКМ → pan ленты
                //  · Super+ЛКМ → драг окна (move_grab → overview_reassign)
                // Сами столы в обзоре НЕ двигаются: их порядок задаёт обзор
                // (ячейки выдаются кольцами вокруг текущего, см. overview.rs).
                if self.overview_active {
                    if button == BTN_LEFT {
                        if alt_held {
                            // Alt+ЛКМ → pan (без grab, флаг)
                            self.pan_button_held = ButtonState::Pressed == btn_state;
                            return;
                        }
                        if self.logo_held {
                            // Super+ЛКМ: падаем в основной хендлер (move окна)
                        } else {
                            // ЛКМ без модификаторов: падаем в основной хендлер
                            // (он сделает фокус на окне или сбросит).
                            // После него выйдем из обзора (см. ниже).
                        }
                    }
                    // ПКМ без Super → выход на стол под курсором.
                    // Super+ПКМ → падаем в основной хендлер (resize окна в обзоре).
                    if ButtonState::Pressed == btn_state && button == BTN_RIGHT
                        && !self.logo_held
                    {
                        self.exit_overview_to_cursor();
                        return;
                    }
                }

                // Alt+ЛКМ press → начинаем pan (флаг, без grab)
                if ButtonState::Pressed == btn_state
                    && alt_held && button == BTN_LEFT
                    && self.tile_config.layout == Layout::Float
                {
                    self.pan_button_held = true;
                    // Новый жест — новая порция строк замера.
                    self.pan_log_left = 60;
                    self.pan_start_screen = Some(self.pointer_screen_physical());
                    tracing::debug!("dawn/canvas: pan started");
                    return;
                }
                // Любое отпускание ЛКМ → завершаем pan, запускаем инерцию (1.1)
                if ButtonState::Released == btn_state && button == BTN_LEFT {
                    if self.pan_button_held {
                        self.momentum.launch();
                        // Итог жеста: где стрелка была в начале и где оказалась.
                        // Ненулевая разница здесь — это и есть «уезжает и
                        // остаётся смещённой» (в отличие от дрожи, у которой
                        // итог около нуля).
                        if let Some(start) = self.pan_start_screen.take() {
                            let now = self.pointer_screen_physical();
                            tracing::debug!(
                                "ИТОГ ПАН: старт=({:.1},{:.1}) конец=({:.1},{:.1}) смещение=({:.1},{:.1})",
                                start.x, start.y, now.x, now.y, now.x - start.x, now.y - start.y,
                            );
                        }
                    }
                    self.pan_button_held = false;
                }

                // Висящий pointer-grab съедает Win+ЛКМ/Win+ПКМ целиком: ветка
                // ниже под него не заходит, и клик выглядит "не работает".
                if ButtonState::Pressed == btn_state && pointer.is_grabbed() {
                    // Висящий grab — вторая причина «клик не сработал»: событие
                    // уходит владельцу захвата, а не тому, на что человек
                    // показывает. Печатаем экранную точку, чтобы в логе было
                    // видно, где именно это случается.
                    let s = self.pointer_screen_physical();
                    tracing::info!(
                        "КЛИК В ЗАХВАТ: активен pointer grab, экран=({:.0},{:.0})", s.x, s.y,
                    );
                }

                if ButtonState::Pressed == btn_state && !pointer.is_grabbed() {
                    // Берём НАШУ позицию курсора — ту же, по которой рисуется
                    // стрелка. current_location() внутри smithay обновляется
                    // только из pointer.motion; пока курсор двигали записью в
                    // pointer_location, две позиции расходились, и клик уходил
                    // не туда, куда показывает стрелка. Теперь все переносы идут
                    // через warp_pointer (motion рассылается), так что значения
                    // совпадают — а если разъедутся, это видно в логе ниже.
                    let pos = self.pointer_location;
                    if cfg!(debug_assertions) || tracing::enabled!(tracing::Level::DEBUG) {
                        let smithay_pos = pointer.current_location();
                        let hit = self.space.element_under(pos)
                            .and_then(|(w, _)| self.space.element_geometry(w));
                        // Отставание КАРТИНКИ: где стрелку нарисовал последний
                        // ушедший на монитор кадр против того, где курсор сейчас.
                        // Ненулевое значение здесь и есть «кликаю не туда, где
                        // вижу стрелку» — хит-тест при этом может быть точен.
                        let кадр = self.frame_cursor;
                        tracing::debug!(
                            "PTR КАДР: стрелка_в_кадре=({:.1},{:.1}) сейчас=({:.1},{:.1}) \
                             отставание=({:.1},{:.1}) возраст_кадра={}мс",
                            кадр.x, кадр.y, pos.x, pos.y,
                            pos.x - кадр.x, pos.y - кадр.y,
                            self.frame_drawn_at.elapsed().as_millis(),
                        );
                        tracing::debug!(
                            "PTR HIT: курсор=({:.1},{:.1}) smithay=({:.1},{:.1}) \
                             расхождение=({:.1},{:.1}) камера=({:.1},{:.1}) zoom={:.2} окно={:?}",
                            pos.x, pos.y, smithay_pos.x, smithay_pos.y,
                            pos.x - smithay_pos.x, pos.y - smithay_pos.y,
                            self.viewport.cam_x, self.viewport.cam_y, self.viewport.zoom, hit,
                        );
                        // Промах ВНУТРИ окна (окно то, а кнопка нажимается со
                        // сдвигом) виден только здесь: печатаем точку, которую
                        // клиент получает в СВОИХ координатах, рядом с тем,
                        // какого размера мы это окно считаем и какого размера
                        // поверхность клиент реально закоммитил. Если
                        // локальная точка и размеры сходятся, а промах есть —
                        // мажет клиент (наш zoom он видит как wl_output.scale),
                        // и лечить надо не хит-тест.
                        self.log_pointer_local(pos);
                    }

                    if let Some((window, window_loc)) = self
                        .space.element_under(pos)
                        .map(|(w, l)| (w.clone(), l))
                    {
                        // ПКМ (без Super) по выделенному окну → сбросить выделение.
                        if button == BTN_RIGHT && !kb_mods.logo
                            && !self.selected_windows.is_empty()
                            && self.is_selected(&window)
                        {
                            self.clear_selection();
                            pointer.button(self, &ButtonEvent {
                                button, state: btn_state, serial, time: event.time_msec(),
                            });
                            pointer.frame(self);
                            return;
                        }

                        // Super+ЛКМ → перемещение окна
                        if kb_mods.logo && button == BTN_LEFT {
                            let initial_window_location = window_loc;
                            let focus = crate::xwin::surface(&window)
                                .map(|s| (s, window_loc.to_f64()));
                            // Вместе едет ВЫДЕЛЕНИЕ (если окно в него входит), а не
                            // созвездие: тянешь одно окно созвездия — едет только оно.
                            let group_initial = self.group_drag_members_excluding(&window)
                                .into_iter()
                                .filter_map(|w| self.space.element_location(&w).map(|l| (w, l)))
                                .collect::<Vec<_>>();
                            // Окно (и члены созвездия) могли ещё лететь после
                            // прошлой анимации — снимаем её, иначе она каждый
                            // тик тянула бы окно на свою траекторию и оно
                            // «резинилось» бы под курсором во время драга.
                            self.freeze_window_anim(&window);
                            for (w, _) in &group_initial {
                                self.freeze_window_anim(w);
                            }
                            let grab = MoveSurfaceGrab::new(
                                GrabStartData { focus, button, location: pos },
                                window.clone(),
                                initial_window_location,
                                group_initial,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Keep);
                            self.request_plane_reset();
                            tracing::debug!("dawn: move grab started");
                            return;
                        }

                        // Super+ПКМ → resize. Float: свободный ресайз. Tile:
                        // тянем деления BSP-дерева (Hyprland smart_resizing,
                        // см. dwindle.rs). Columns: ширину активной
                        // колонки. Обзор: свободный ресайз миниатюры (см.
                        // resize_grab.rs, ветка overview_active). Monocle: no-op.
                        if kb_mods.logo && button == BTN_RIGHT {
                            tracing::debug!("dawn: resize grab start");
                            let geo = self.space.element_geometry(&window)
                                .unwrap_or(Rectangle::new(window_loc, (100, 100).into()));
                            let rel = pos - window_loc.to_f64();
                            let edge = match (
                                rel.x < geo.size.w as f64 / 2.0,
                                rel.y < geo.size.h as f64 / 2.0,
                            ) {
                                (true,  true)  => ResizeEdge::TOP_LEFT,
                                (false, true)  => ResizeEdge::TOP_RIGHT,
                                (true,  false) => ResizeEdge::BOTTOM_LEFT,
                                (false, false) => ResizeEdge::BOTTOM_RIGHT,
                            };
                            // Вместе масштабируется ВЫДЕЛЕНИЕ (см. move-грab выше).
                            let group_initial = self.group_drag_members_excluding(&window)
                                .into_iter()
                                .filter_map(|w| self.space.element_geometry(&w).map(|g| (w, g)))
                                .collect::<Vec<_>>();
                            // Ресайз двигает окно напрямую (LEFT/TOP-края) —
                            // недолетевшая анимация позиции дралась бы с ним.
                            self.freeze_window_anim(&window);
                            for (w, _) in &group_initial {
                                self.freeze_window_anim(w);
                            }
                            let grab = ResizeSurfaceGrab::start(
                                GrabStartData { focus: None, button, location: pos },
                                window, edge, geo, group_initial,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Keep);
                            return;
                        }

                        // Клик → focus
                        crate::xwin::focus(self, &window);
                    } else {
                        if kb_mods.logo {
                            // Win+ЛКМ/Win+ПКМ пришли по пустому месту: под нашей
                            // точкой курсора окна нет (хотя стрелка может рисоваться
                            // поверх окна — тогда разъехались координаты).
                            tracing::debug!(
                                "PTR: Win+клик без окна под курсором ({:.1},{:.1})", pos.x, pos.y
                            );
                        }
                        // «Окна под курсором нет» — ещё не «под курсором пусто».
                        // Layer-поверхности (меню обоев dwall, лаунчер) в space
                        // не лежат, а рисуются поверх окон. Раньше клик по ним
                        // попадал сюда: фокус снимался, а ЛКМ вдобавок уходила
                        // в rubber-band c Focus::Clear и до клиента не доходила
                        // ВООБЩЕ — в меню dwall работала только ПКМ, потому что
                        // она grab не создаёт и проваливалась в общую пересылку
                        // ниже. Отдаём такой клик слою и не трогаем фокус.
                        if self.курсор_над_слоем(pos) {
                            pointer.button(self, &ButtonEvent {
                                button, state: btn_state, serial, time: event.time_msec(),
                            });
                            pointer.frame(self);
                            return;
                        }

                        let all: Vec<_> = self.space.elements().cloned().collect();
                        for w in all {
                            w.set_activated(false);
                            crate::xwin::configure(&w);
                        }
                        keyboard.set_focus(self, None, serial);

                        // ЛКМ по пустому холсту в Float → rubber-band мультивыделение
                        // (протяжка выделяет пересекающиеся окна, клик без протяжки —
                        // просто снимает выделение, см. select_grab.rs).
                        if button == BTN_LEFT && !alt_held
                            && self.tile_config.layout == Layout::Float
                        {
                            self.clear_selection();
                            let grab = crate::grabs::SelectGrab {
                                start_data: GrabStartData { focus: None, button, location: pos },
                                start_pos: pos,
                            };
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                        }
                    }
                }

                // ── ЛКМ в обзоре: после основного хендлера выходим ──────────────
                if self.overview_active
                    && ButtonState::Pressed == btn_state && button == BTN_LEFT
                    && !alt_held && !self.logo_held
                {
                    // Наша позиция курсора — та же, по которой рисуется стрелка
                    // (см. комментарий выше о расхождении с smithay).
                    let pos = self.pointer_location;
                    let clicked_window = self.space.element_under(pos).map(|(w, _)| w.clone());
                    if let Some(window) = clicked_window {
                        self.exit_overview_to_window(&window);
                    } else {
                        let mask = self.overview_workspace_at(pos);
                        self.exit_overview_immediate(mask);
                    }
                }

                pointer.button(self, &ButtonEvent {
                    button, state: btn_state, serial, time: event.time_msec(),
                });
                pointer.frame(self);
            }

            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let _h_raw = event.amount(Axis::Horizontal);
                let _v_raw = event.amount(Axis::Vertical);
                let _v120 = event.amount_v120(Axis::Vertical);
                let _alt_check = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                tracing::trace!("SCROLL: h={:?} v={:?} v120={:?} alt={}", _h_raw, _v_raw, _v120, _alt_check);
                let h = event.amount(Axis::Horizontal)
                    .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
                let v = event.amount(Axis::Vertical)
                    .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);

                let alt_held = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);

                // ── Таблица жестов: два пальца ───────────────────────────
                //
                // Два пальца libinput жестом НЕ считает (GestureSwipe/Pinch
                // начинаются с трёх) — он шлёт их прокруткой с
                // `source = Finger`. Поэтому `2-finger-swipe` из `gesture{}`
                // ловится здесь, а снаружи это ровно такой же бинд, как
                // трёхпальцевый: разница видна только в этом месте.
                //
                // Начала и конца у прокрутки нет, есть поток кадров и
                // финальный кадр амплитуды 0 — по нему и закрываем жест.
                // Карточка предпросмотра забирает пальцы ПЕРВОЙ — раньше и
                // таблицы жестов, и всего остального. Она висит поверх холста,
                // и прокрутка над ней обязана водить её, а не камеру под ней —
                // ровно тот же довод, по которому выше от неё закрыто колесо.
                // Карточка первой, карта следом — тот же порядок, что у
                // колеса ниже: карточка висит поверх карты.
                if source == AxisSource::Finger && self.preview_pan_by(h, v) {
                    return;
                }
                if source == AxisSource::Finger && self.minimap_pan_by(h, v) {
                    return;
                }
                if source == AxisSource::Finger {
                    if h == 0.0 && v == 0.0 {
                        if self.жест_конец(false) {
                            return;
                        }
                    } else {
                        if self.жест.is_none()
                            && self.жест_начало(crate::gestures::ОсноваЖеста::Свайп, 2)
                        {
                            // Первый кадр отдаём тому же обработчику, что и
                            // остальные, — иначе он потерялся бы.
                        }
                        if self.жест_свайп_шаг(h, v, event.time_msec()) {
                            return;
                        }
                    }
                }

                // Super + 2-палец тачпад-скролл → таскать окно под курсором.
                // ВАЖНО: 2-пальцевое движение по тачпаду libinput шлёт как
                // scroll с source=Finger (жесты GestureSwipe/Pinch — это 3+
                // пальца), поэтому "Super+2 пальца" ловится именно здесь, а не
                // в GestureSwipe. Курсор при этом стоит на месте, окно едет.
                // Колесо над картой окон и над карточкой предпросмотра крутит ИХ
                // зум, а не холста. Проверка стоит до всего остального: иначе
                // плашка, лежащая поверх холста, всё равно пропускала бы колесо
                // сквозь себя. Карточка первой — она висит поверх карты.
                if source != AxisSource::Finger && self.preview_wheel(-v / 15.0) {
                    return;
                }
                if source != AxisSource::Finger && self.minimap_wheel(-v / 15.0) {
                    return;
                }

                let logo_held = self.logo_held
                    || self.seat.get_keyboard().map(|kb| kb.modifier_state().logo).unwrap_or(false);
                // Жест с зажатым Super отменяет ожидающий тап Super (обзор столов).
                if logo_held {
                    self.super_tap = false;
                }
                if logo_held && source == AxisSource::Finger {
                    // В обзоре столов Super+2пальца ПАНОРАМИРУЮТ ленту (навигация
                    // по столам), а не двигают окно.
                    if self.overview_active {
                        let zoom = self.viewport.zoom;
                        // Тот же класс, что Alt+ЛКМ: пан рукой обязан удержать
                        // стрелку на экране в этом же кадре (см. pan_camera_by).
                        self.pan_camera_by(h * 2.5 / zoom, v * 2.5 / zoom);
                        self.request_redraw();
                        return;
                    }
                    // Жест ведёт ТОТ ЖЕ grab, что и Win+ЛКМ, поэтому и работает
                    // он теперь одинаково во всех раскладках.
                    //
                    // Раньше здесь лежала собственная, куда более бедная
                    // реализация: она умела только свободный сдвиг и потому
                    // явно отказывалась работать со всем, что не плавает, —
                    // в Tile и в ленте Columns жест молча выходил, ничего не
                    // сделав. Отсюда и «в тайлинге и niri окна двумя пальцами
                    // не таскаются». Свап соседей, шов вставки в ленте, перенос
                    // между столами и возврат в слот — всё это живёт в
                    // MoveSurfaceGrab (drag_to/finish), и дублировать его здесь
                    // во второй раз было бы ровно тем же самым багом заново.
                    if h == 0.0 && v == 0.0 {
                        // Финальный кадр амплитуды 0 = пальцы отпущены. Это
                        // единственный конец жеста, который нам показывают, —
                        // и он же единственное, что останавливает доводку
                        // (ровно как EdgeMotion у synaptics вёл указатель
                        // «пока палец не поднят»).
                        tracing::debug!(
                            "ЖЕСТ: нулевой кадр, простой перед ним={}мс, окно={}",
                            self.edge_drift.as_ref()
                                .map(|d| d.moving_at.elapsed().as_millis())
                                .unwrap_or(0),
                            self.touchpad_drag.is_some(),
                        );
                        self.edge_drift = None;
                        self.touchpad_drag_empty = None;
                        if let Some(mut grab) = self.touchpad_drag.take() {
                            grab.finish(self);
                        }
                        return;
                    }
                    // Латчим окно на первом кадре жеста: даже с курсором,
                    // который едет следом, у края экрана он упирается в
                    // границу и окно из-под него всё равно выскользнуло бы —
                    // поэтому двигаем именно залатченное окно, пока пальцы не
                    // отпущены.
                    //
                    // Промах латчим тоже: под курсором может не быть окна, и
                    // тогда пробовать заново на КАЖДОМ кадре бессмысленно —
                    // курсор при этом жесте стоит на месте, ответ не изменится.
                    // Раньше без этого один жест по пустому месту давал сотню
                    // hit-тестов и сотню строк в логе (20260816_131327, 10:14:25
                    // — 30 строк за 200 мс подряд).
                    let пусто_недавно = self.touchpad_drag_empty
                        .is_some_and(|t| t.elapsed() < TOUCHPAD_GESTURE_GAP);
                    if self.touchpad_drag.is_none() && !пусто_недавно {
                        self.start_touchpad_drag();
                        tracing::debug!(
                            "ЖЕСТ: Super+2пальца → перенос окна, окно под курсором={}",
                            self.touchpad_drag.is_some(),
                        );
                    }
                    if self.touchpad_drag.is_none() {
                        // Под курсором нет окна — таскать нечего. Отметку
                        // освежаем каждым кадром: пока жест идёт, кадры сыплются
                        // чаще, чем протухает пауза, а как только он кончился —
                        // отметка отмирает сама, чем бы жест ни завершился.
                        self.touchpad_drag_empty = Some(Instant::now());
                        return;
                    }
                    let zoom = self.viewport.zoom;
                    let step = Point::from((
                        h * TOUCHPAD_MOVE_SPEED / zoom,
                        v * TOUCHPAD_MOVE_SPEED / zoom,
                    ));
                    self.note_gesture_step(step, event.time_msec());
                    self.gesture_advance(step, event.time_msec());
                    return;
                }

                // Alt + 2-палец тачпад-скролл в ленте (Columns/niri) → прокрутка
                // самой полосы: горизонталь везёт вид по колонкам, вертикаль
                // листает столы — ровно то же, что делает голый свайп 3+
                // пальцами (см. GestureSwipeUpdate). Свободного пана в ленте нет
                // (ветка ниже её и исключает), а без модификатора 2 пальца
                // обязаны уходить в приложение — поэтому жест повешен на Alt.
                if alt_held && source == AxisSource::Finger
                    && self.tile_config.layout == Layout::Columns
                    && !self.overview_active
                {
                    // Единицы скролла тачпада мельче пиксельной дельты свайпа —
                    // тот же коэффициент, что у Alt+пана ниже.
                    const TOUCHPAD_PAN_SPEED: f64 = 2.5;
                    if h == 0.0 && v == 0.0 {
                        // Финальный кадр амплитуды 0 = пальцы отпущены →
                        // прилипаем к ближайшей колонке, как в niri.
                        self.columns_swipe_end();
                    } else if h.abs() >= v.abs() {
                        self.columns_swipe_scroll(h * TOUCHPAD_PAN_SPEED);
                    } else {
                        self.columns_swipe_workspace(v * TOUCHPAD_PAN_SPEED);
                    }
                    self.request_redraw();
                    return;
                }

                // Alt + 2-палец тачпад-скролл → pan холста.
                // Отличаем от колеса мыши по source=Finger, поэтому Alt+колесо
                // по-прежнему зумит как раньше (ветка ниже) — старое поведение
                // не тронуто, это отдельная ветка только для тачпада.
                // Разрешено ТОЛЬКО в обзоре и лупе, см. touchpad_camera_allowed.
                if alt_held && source == AxisSource::Finger && self.touchpad_camera_allowed() {
                    // Скролл-единицы тачпада заметно мельче, чем raw pixel delta
                    // мыши при Alt+ЛКМ — усиливаем, чтобы скорость ощущалась так же.
                    const TOUCHPAD_PAN_SPEED: f64 = 2.5;
                    if h != 0.0 || v != 0.0 {
                        let zoom = self.viewport.zoom;
                        let dcam_x = h * TOUCHPAD_PAN_SPEED / zoom;
                        let dcam_y = v * TOUCHPAD_PAN_SPEED / zoom;
                        self.pan_camera_by(dcam_x, dcam_y);
                        self.momentum.accumulate(
                            smithay::utils::Point::from((-dcam_x, -dcam_y)),
                            event.time_msec(),
                        );
                        if self.pan_log_left > 0 {
                            self.pan_log_left -= 1;
                            let s = self.pointer_screen_physical();
                            tracing::debug!(
                                "ПАН Alt+2пальца: курсор_экран=({:.1},{:.1}) камера=({:.1},{:.1}) дельта=({:.1},{:.1})",
                                s.x, s.y, self.viewport.cam_x, self.viewport.cam_y, h, v,
                            );
                        }
                        self.request_redraw();
                    } else {
                        // libinput шлёт финальный кадр с амплитудой 0, когда пальцы
                        // отпущены — это сигнал "стоп", запускаем инерцию отсюда.
                        tracing::debug!("ЖЕСТ: Alt+2пальца → пан холста завершён, инерция");
                        self.momentum.launch();
                        self.pan_log_left = 60;
                    }
                    return;
                }

                // ── Голые два пальца по ПУСТОМУ холсту во Float → выделение ──
                //
                // Тот же жест, что ЛКМ по пустому месту (см. SelectGrab): рамка
                // тянется за курсором, на отпускании выделяются все задетые
                // окна. Условие «под курсором пусто» здесь не придирка, а суть:
                // два пальца без модификаторов — это обычная прокрутка, и
                // отнимать её у окна под курсором нельзя. Ровно поэтому и
                // ЛКМ-выделение начинается только с пустого холста.
                let сюда_можно_выделять = !alt_held
                    && !logo_held
                    && self.tile_config.layout == Layout::Float
                    && !self.overview_active
                    && self.space.element_under(self.pointer_location).is_none()
                    && !self.курсор_над_слоем(self.pointer_location);
                // Проверка на тачпад — снаружи скобок: начатое выделение не
                // должно перехватывать ещё и колесо мыши, если по нему крутнули
                // прямо посреди жеста.
                if source == AxisSource::Finger
                    && (self.touchpad_select_start.is_some() || сюда_можно_выделять)
                {
                    if h == 0.0 && v == 0.0 {
                        // Пальцы отпущены — применяем рамку.
                        self.edge_drift = None;
                        if self.touchpad_select_start.take().is_some() {
                            let rect = self.selection_drag.take();
                            self.select_windows_in_rect(rect);
                            self.request_plane_reset();
                            self.request_redraw();
                        }
                        return;
                    }
                    if self.touchpad_select_start.is_none() {
                        self.clear_selection();
                        self.touchpad_select_start = Some(self.pointer_location);
                        tracing::debug!("ЖЕСТ: 2 пальца по пустому холсту → выделение");
                    }
                    let zoom = self.viewport.zoom;
                    let step = Point::from((
                        h * TOUCHPAD_MOVE_SPEED / zoom,
                        v * TOUCHPAD_MOVE_SPEED / zoom,
                    ));
                    self.note_gesture_step(step, event.time_msec());
                    self.gesture_advance(step, event.time_msec());
                    return;
                }

                // В обзоре столов колесо мыши ЗУМИТ (в обычном tiling zoom нельзя).
                if self.overview_active && v != 0.0 && source != AxisSource::Finger {
                    // Щелчок двигает цель, доезд делает anim::ZoomGlide. Камера
                    // при этом пересчитывается ОТ курсора, поэтому его экранная
                    // точка не меняется — лишнего motion клиенту не уходит
                    // (см. pointer_warped в тике доезда).
                    let factor = if v < 0.0 { 1.1_f64 } else { 0.9_f64 };
                    self.zoom_step_at_cursor(factor);
                    return;
                }

                // Alt+колесо мыши в Columns (niri) → листаем колонки влево/вправо
                // (тачпадный Alt+2-пальца выше уже ушёл в pan холста).
                if alt_held && self.tile_config.layout == Layout::Columns
                    && source != AxisSource::Finger && (v != 0.0 || h != 0.0)
                {
                    let dir = if v != 0.0 {
                        if v > 0.0 { 1 } else { -1 }
                    } else if h > 0.0 { 1 } else { -1 };
                    self.columns_focus(dir, 0);
                    self.request_redraw();
                    return;
                }

                // Alt+Scroll (колесо мыши) → zoom (только в Float режиме)
                if alt_held && v != 0.0 && self.tile_config.layout == Layout::Float {
                    // v120 > 0 = колесо вниз = отдаляем.
                    let factor = if v < 0.0 { 1.1_f64 } else { 0.9_f64 };
                    self.zoom_step_at_cursor(factor);
                    tracing::debug!(
                        "dawn/canvas: цель зума={:.3} (сейчас {:.3})",
                        self.zoom_glide.as_ref().map(|g| g.target).unwrap_or(self.viewport.zoom),
                        self.viewport.zoom,
                    );
                    return;
                }

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if h != 0.0 { frame = frame.value(Axis::Horizontal, h); }
                if v != 0.0 { frame = frame.value(Axis::Vertical, v); }
                // Дискретные щелчки колеса (v120) обязаны уходить вместе со
                // значением. Xwayland переводит колесо в X11-кнопки 4/5 по
                // щелчкам, а не по непрерывной амплитуде: без v120 прокрутка
                // доходит до нативных wayland-клиентов и пропадает во всех
                // X11-приложениях. Тачпад (source=Finger) v120 не имеет —
                // там остаётся только непрерывное значение.
                if source == AxisSource::Wheel || source == AxisSource::WheelTilt {
                    if let Some(dh) = event.amount_v120(Axis::Horizontal) {
                        if dh != 0.0 { frame = frame.v120(Axis::Horizontal, dh as i32); }
                    }
                    if let Some(dv) = event.amount_v120(Axis::Vertical) {
                        if dv != 0.0 { frame = frame.v120(Axis::Vertical, dv as i32); }
                    }
                }
                // Финальный кадр жеста (амплитуда 0) — это axis_stop. Без него
                // клиент считает прокрутку незавершённой и продолжает кинетику.
                if source == AxisSource::Finger {
                    if h == 0.0 { frame = frame.stop(Axis::Horizontal); }
                    if v == 0.0 { frame = frame.stop(Axis::Vertical); }
                }
                let ptr = self.seat.get_pointer().unwrap();
                // Прокрутка уходит ТОЛЬКО в поверхность под указателем. Если
                // здесь `фокус=нет`, колесо «не работает» не из-за самих
                // событий: курсор просто вне окна (например, прижат к краю
                // экрана ниже нижней границы окна) — смотреть надо туда, а не
                // в ветку axis.
                if tracing::enabled!(tracing::Level::DEBUG) {
                    tracing::debug!(
                        "SCROLL→КЛИЕНТ: h={:.2} v={:.2} v120={:?} источник={:?} фокус={}",
                        h, v, event.amount_v120(Axis::Vertical), source,
                        if ptr.current_focus().is_some() { "есть" } else { "нет" },
                    );
                }
                ptr.axis(self, frame);
                ptr.frame(self);
            }

            // ── Pinch → zoom canvas ──────────────────────────────────────
            InputEvent::GesturePinchBegin { event, .. } => {
                // Щипок над карточкой — её зум, и он старше таблицы жестов по
                // той же причине, что и пан выше: под пальцами карточка, а не
                // холст. Отправную точку масштаба ставим здесь.
                self.preview_pinch_last = 1.0;
                if self.preview_hit().is_some() || self.minimap_hit().is_some() {
                    return;
                }
                if self.жест_начало(crate::gestures::ОсноваЖеста::Щипок, event.fingers()) {
                    return;
                }
                tracing::debug!("ЖЕСТ: pinch начат, logo_held={}", self.logo_held);
                self.pinch_last_scale = 1.0;
                // Super+2-пальца pinch → resize окна под курсором (в любом режиме;
                // вытаскиваем из тайлинга во floating, иначе arrange его сожмёт).
                if self.logo_held {
                    let pos = self.pointer_location;
                    self.gesture_resize_window = self.space.element_under(pos).map(|(w, _)| w.clone());
                    if let Some(window) = self.gesture_resize_window.clone() {
                        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                            tw.window == window
                        }) {
                            tw.floating = true;
                        }
                        // Опорные размеры на весь жест — от них считаем
                        // абсолютный scale (см. gesture_resize_group). Вместе с
                        // окном ресайзится выделение, если окно в нём состоит.
                        let mut group = vec![window.clone()];
                        group.extend(self.group_drag_members_excluding(&window));
                        self.gesture_resize_group = group.into_iter()
                            .map(|w| { let s = crate::xwin::current_size(&w); (w, s) })
                            .collect();
                    }
                }
            }

            InputEvent::GesturePinchUpdate { event, .. } => {
                let scale = event.scale();
                // libinput отдаёт масштаб от НАЧАЛА жеста, а зум карточки
                // копится — отсюда деление на прошлый кадр.
                if scale > 0.0 && (self.preview_hit().is_some() || self.minimap_hit().is_some()) {
                    let множитель = scale / self.preview_pinch_last.max(1e-6);
                    self.preview_pinch_last = scale;
                    if self.preview_pinch(множитель) || self.minimap_pinch(множитель) {
                        return;
                    }
                }
                if self.жест_щипок_шаг(scale) {
                    return;
                }
                if scale <= 0.0 { return; }

                if let Some(window) = self.gesture_resize_window.clone() {
                    // Размеры считаются от опорных по АБСОЛЮТНОМУ scale жеста
                    // (см. Dawn::gesture_resize_group), а показатель PINCH_GAIN
                    // усиливает жест: пальцы на тачпаде разводятся максимум
                    // раза в полтора, а окно должно успевать вырасти вдвое.
                    const PINCH_GAIN: f64 = 3.0;
                    self.pinch_last_scale = scale;
                    if self.gesture_resize_group.is_empty() {
                        let s = crate::xwin::current_size(&window);
                        self.gesture_resize_group = vec![(window.clone(), s)];
                    }
                    let factor = scale.powf(PINCH_GAIN).clamp(0.05, 20.0);
                    for (w, base) in self.gesture_resize_group.clone() {
                        // Пол — 1 px (нулевая поверхность недопустима), а не
                        // «приличные» 50: щипок ужимает окно во что угодно, как
                        // и остальные пути ресайза. Потолок оставлен только от
                        // переполнения арифметики размеров.
                        let new_w = (base.w as f64 * factor).round().clamp(1.0, 20000.0) as i32;
                        let new_h = (base.h as f64 * factor).round().clamp(1.0, 20000.0) as i32;
                        crate::xwin::set_size(&w, Some((new_w, new_h).into()), crate::xwin::Tiled::Keep);
                        crate::xwin::configure(&w);
                    }
                    self.request_redraw();
                    return;
                }

                let alt_held = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                if !alt_held { return; }
                // Зум камеры щипком раньше не проверял раскладку ВООБЩЕ: он
                // работал и во Float, и в ленте Columns, где пан для того же
                // жеста давно закрыт. Теперь условие одно на все жесты камеры.
                // Режим лупы (Super+Space) держит максимум отдаления и зум не
                // отдаёт никому — ни колесу, ни пальцам.
                if !self.touchpad_camera_allowed() || self.zoom_locked() {
                    // Масштаб всё равно запоминаем: иначе на следующем жесте
                    // factor посчитается от единицы и камера прыгнет рывком.
                    self.pinch_last_scale = scale;
                    return;
                }
                let factor = scale / self.pinch_last_scale;
                self.pinch_last_scale = scale;

                let cursor = self.pointer_location;

                // Щипок непрерывен сам по себе — сглаживать его нечем и незачем,
                // но начатый колесом доезд надо снять, иначе он тянул бы зум к
                // своей цели поверх пальцев.
                self.zoom_glide = None;
                let old_zoom = self.viewport.zoom;
                let new_zoom = (old_zoom * factor).clamp(Dawn::ZOOM_MIN, Dawn::ZOOM_MAX);
                self.viewport.zoom = new_zoom;

                // Якорь под курсором
                let screen_x = (cursor.x - self.viewport.cam_x) * old_zoom;
                let screen_y = (cursor.y - self.viewport.cam_y) * old_zoom;
                self.viewport.cam_x = cursor.x - screen_x / new_zoom;
                self.viewport.cam_y = cursor.y - screen_y / new_zoom;

                self.apply_camera();
                self.request_redraw();
                tracing::debug!("pinch: zoom={:.3}", new_zoom);
            }

            InputEvent::GesturePinchEnd { event, .. } => {
                if self.жест_конец(event.cancelled()) {
                    return;
                }
                self.pinch_last_scale = 1.0;
                let group = std::mem::take(&mut self.gesture_resize_group);
                tracing::debug!("ЖЕСТ: pinch закончен, окон в группе={}", group.len());
                self.gesture_resize_window = None;
                // Запоминаем итоговый размер КАЖДОМУ окну группы — иначе
                // следующий переход Float→tiling→Float вернул бы соседям
                // выделения их доресайзные размеры.
                for (w, _) in group {
                    let size = crate::xwin::current_size(&w);
                    if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == w) {
                        tw.float_size = Some(size);
                    }
                }
            }

            // ── Swipe (2 пальца, любой режим) → pan canvas ───────────────
            InputEvent::GestureSwipeBegin { event, .. } => {
                // Таблица жестов (`gesture{}`) идёт ПЕРВОЙ и, если нашла свой
                // бинд, забирает жест целиком. Не нашла — ниже всё как было до
                // 30.08.2026, слово в слово. См. gestures.rs.
                if self.жест_начало(crate::gestures::ОсноваЖеста::Свайп, event.fingers()) {
                    return;
                }
                // Super+2-палец → перемещение окна под курсором (новый жест).
                if self.logo_held && self.tile_config.layout == Layout::Float {
                    let pos = self.pointer_location;
                    self.gesture_move_window = self.space.element_under(pos).map(|(w, _)| w.clone());
                }
            }

            InputEvent::GestureSwipeUpdate { event, .. } => {
                let delta = event.delta();
                if self.жест_свайп_шаг(delta.x, delta.y, event.time_msec()) {
                    return;
                }
                if delta.x == 0.0 && delta.y == 0.0 { return; }

                if let Some(window) = self.gesture_move_window.clone() {
                    if let Some(geo) = self.space.element_geometry(&window) {
                        let new_loc = smithay::utils::Point::from((
                            geo.loc.x + delta.x.round() as i32,
                            geo.loc.y + delta.y.round() as i32,
                        ));
                        self.space.map_element(window.clone(), new_loc, true);
                        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                            tw.window == window
                        }) {
                            tw.float_position = new_loc;
                            tw.position = new_loc;
                            tw.float_position_set = true;
                        }
                    }
                    self.request_redraw();
                    return;
                }

                // ── Columns/niri: голый свайп 3+ пальцами листает полосу ─────
                // Горизонталь — прокрутка вида по колонкам (на отпускании
                // прилипаем к ближайшей, см. GestureSwipeEnd), вертикаль —
                // переход по столам. Только в Columns: в остальных раскладках
                // жест остаётся прежним (Alt+пан ниже), их не трогаем.
                if self.tile_config.layout == Layout::Columns && !self.logo_held {
                    if delta.x.abs() >= delta.y.abs() {
                        self.columns_swipe_scroll(delta.x);
                    } else {
                        self.columns_swipe_workspace(delta.y);
                    }
                    self.request_redraw();
                    return;
                }

                let alt = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                if !alt { return; }
                // Камеру свайпом двигаем только в обзоре и лупе — в обычных
                // раскладках жест не должен уводить вид (touchpad_camera_allowed).
                if !self.touchpad_camera_allowed() { return; }
                // Alt + 2-пальца → pan
                let zoom = self.viewport.zoom;
                let dcam_x = delta.x / zoom;
                let dcam_y = delta.y / zoom;
                // Курсор держим на той же screen-позиции ЗДЕСЬ же: отложенная
                // sync_pointer_to_camera отставала на кадр и давала дрожь
                // (см. pan_camera_by). Hit-test при этом не портится — repin
                // шлёт pointer.motion, чего не делала прежняя ручная правка
                // pointer_location, из-за которой этот путь и был отложенным.
                self.pan_camera_by(dcam_x, dcam_y);
                // Кинетический скролл (1.1): копим дельту для инерции на конец жеста
                self.momentum.accumulate(
                    smithay::utils::Point::from((-dcam_x, -dcam_y)),
                    event.time_msec(),
                );
                self.request_redraw();
                tracing::debug!("swipe pan: cam=({:.1},{:.1})", self.viewport.cam_x, self.viewport.cam_y);
            }

            InputEvent::GestureSwipeEnd { event, .. } => {
                if self.жест_конец(event.cancelled()) {
                    return;
                }
                if self.gesture_move_window.take().is_some() {
                    return;
                }
                // В Columns свайп заканчивается «прилипанием» к ближайшей
                // колонке — как в niri, где вид всегда стоит на колонке, а не
                // между ними. Инерцию тут не запускаем: она возит камеру по
                // холсту и мимо модели колонок.
                if self.tile_config.layout == Layout::Columns {
                    self.columns_swipe_end();
                    self.request_redraw();
                    return;
                }
                self.momentum.launch();
            }

            // ── Удержание ────────────────────────────────────────────────
            // Раньше `hold` не обрабатывался вовсе — libinput его слал, dawn
            // ронял в общий `_ =>`. Теперь это полноценный триггер таблицы
            // (`4-finger-hold` и подобные), но только через неё: без бинда обе
            // ветки по-прежнему не делают ничего.
            InputEvent::GestureHoldBegin { event, .. } => {
                self.жест_начало(crate::gestures::ОсноваЖеста::Удержание, event.fingers());
            }
            InputEvent::GestureHoldEnd { event, .. } => {
                self.жест_конец(event.cancelled());
            }

            _ => {}
        }
    }
}

/// Разбор клавиши: всё, что композитор делает с ней ДО клиента — тап по Super,
/// режим лупы, меню, захват клавиш приложением и бинды из `config.lua`.
///
/// Вынесено из замыкания `process_input_event` НЕ ради красоты: этой же дорогой
/// ходят клавиши из Minecraft (`mine::seat::клавиша`), а до 01.09.2026 у режима
/// была своя урезанная копия — один `find_action` и ничего больше. Из игры
/// поэтому не работало ровно то, чего в таблице биндов нет: обзор столов тапом
/// по Super, лупа Super+Space, пан лупы стрелками, Alt+Tab (перебор кончается
/// на ОТПУСКАНИИ Alt), меню полки, поиск окон и аварийное Super+Shift+Escape.
/// Две копии разъезжаются молча — теперь копия одна.
///
/// `из_игры` меняет ровно одно: `mine_mode` не выполняется. Выйти из режима
/// изнутри игры — верный способ остаться без панелей и без клавиатуры, чтобы
/// вернуть их (хозяйская в этот момент у Minecraft).
pub(crate) fn разобрать_клавишу(
    state: &mut Dawn,
    modifiers: &smithay::input::keyboard::ModifiersState,
    handle: smithay::input::keyboard::KeysymHandle<'_>,
    pressed: bool,
    из_игры: bool,
) -> FilterResult<()> {
    const SUPER_L: u32 = keysyms::KEY_Super_L;
    const SUPER_R: u32 = keysyms::KEY_Super_R;
    let sym = handle.modified_sym();
    let raw = sym.raw();

    // Трекаем Super по keysym + детект "тапа" (обзор столов)
    if raw == SUPER_L || raw == SUPER_R {
        state.logo_held = pressed;
        if pressed {
            // Кандидат на тап — сбросится любым другим вводом.
            state.super_tap = true;
            state.super_tap_shift = modifiers.shift;
        } else {
            if state.super_tap {
                // Обзор открывает чистый тап Super в ЛЮБОЙ
                // раскладке. В Columns дополнительно
                // работает и Shift+Super — тап с Shift тоже
                // считается тапом (см. ветку Shift ниже),
                // так что обе комбинации ведут в обзор.
                state.toggle_overview();
            }
            state.super_tap = false;
            state.super_tap_shift = false;
        }
        return FilterResult::Forward;
    }

    // Shift, нажатый ВО ВРЕМЯ удержания Super, тап не
    // отменяет — он его модификатор (Shift+Super в
    // Columns). Любая другая клавиша отменяет как раньше.
    const SHIFT_L: u32 = keysyms::KEY_Shift_L;
    const SHIFT_R: u32 = keysyms::KEY_Shift_R;
    if raw == SHIFT_L || raw == SHIFT_R {
        if pressed && state.super_tap {
            state.super_tap_shift = true;
        }
        return FilterResult::Forward;
    }

    // Отпустили Alt — перебор стопки (Alt+Tab) закончен, и
    // следующий Tab начнёт новую стопку с текущего окна.
    // Ловим ДО общего `if !pressed`: отпускание клавиши
    // ниже никуда не доходит.
    const ALT_L: u32 = keysyms::KEY_Alt_L;
    const ALT_R: u32 = keysyms::KEY_Alt_R;
    const META_L: u32 = keysyms::KEY_Meta_L;
    const META_R: u32 = keysyms::KEY_Meta_R;
    if matches!(raw, ALT_L | ALT_R | META_L | META_R) && !pressed {
        state.cycle_stack_end();
        return FilterResult::Forward;
    }

    // Любое другое нажатие клавиши отменяет ожидающий тап Super
    // (Super+D, Super+1 и т.п. — это не тап).
    if pressed {
        state.super_tap = false;
    }

    // ── Super+Space (тумблер) → режим лупы (zoom-nav) ──
    // Настраивается через set{bird_eye_key=...} (по умолчанию space).
    // Раньше это был hold-жест bird's-eye; теперь тумблер: вкл —
    // зум к центру, стрелки панорамируют, повторный Super+Space
    // сбрасывает (см. Dawn::toggle_zoom_nav).
    if raw == state.lua_config.bird_eye_key {
        if pressed && state.logo_held {
            state.toggle_zoom_nav();
        }
        if state.logo_held {
            return FilterResult::Intercept(());
        }
        return FilterResult::Forward;
    }

    if !pressed { return FilterResult::Forward; }

    // Escape во время выбора источника для демонстрации
    // экрана — отмена (см. portal.rs). Перехватываем до
    // всех биндов и до клиента: пока идёт выбор, клавиша
    // принадлежит выбору.
    if state.portal_picking() && raw == keysyms::KEY_Escape {
        state.portal_pick_click(true);
        return FilterResult::Intercept(());
    }

    // Во время выбора источника цифра выбирает МОНИТОР
    // напрямую: на двух экранах «ткни в пустой холст» даёт
    // тот, где стоит стрелка, а показать часто нужно
    // соседний. Клавиша принадлежит выбору и до биндов не
    // доходит — новых сочетаний в config.lua не заводится.
    if state.portal_picking() {
        if let Some(n) = цифра_монитора(raw) {
            if state.portal_pick_monitor(n) {
                return FilterResult::Intercept(());
            }
        }
    }

    // Escape во время выделения области для снимка экрана
    // (PrtScr, см. snip.rs) — отмена, ровно как у выбора
    // источника выше.
    if state.snip_идёт() && raw == keysyms::KEY_Escape {
        state.snip_cancel();
        return FilterResult::Intercept(());
    }

    let alt   = modifiers.alt;
    let shift = modifiers.shift;
    let ctrl  = modifiers.ctrl;
    let logo  = modifiers.logo || state.logo_held;

    // Для layout-independence: берём latin sym если отличается
    // (важно теперь вдвойне — с несколькими XKB-раскладками
    // биндинги должны срабатывать независимо от активной)
    let raw_latin = handle.raw_latin_sym_or_raw_current_sym()
        .map(|s| s.raw())
        .unwrap_or(raw);

    tracing::debug!(
        "KEY: key={} alt={} shift={} ctrl={} logo={}", raw_latin, alt, shift, ctrl, logo
    );

    // Режим лупы (Super+Space): голые стрелки панорамируют
    // увеличенный вид — перехватываем раньше обычных биндов
    // (focus_direction), чтобы не уводить фокус.
    if state.zoom_nav_mode {
        let step = if raw_latin == keysyms::KEY_Left {
            Some((-1.0, 0.0))
        } else if raw_latin == keysyms::KEY_Right {
            Some((1.0, 0.0))
        } else if raw_latin == keysyms::KEY_Up {
            Some((0.0, -1.0))
        } else if raw_latin == keysyms::KEY_Down {
            Some((0.0, 1.0))
        } else {
            None
        };
        if let Some((dx, dy)) = step {
            state.zoom_nav_pan(dx, dy);
            return FilterResult::Intercept(());
        }
    }

    // Открытый поиск окон (Super+F) — это поле ввода: пока
    // он на экране, ГОЛЫЕ клавиши принадлежат ему целиком,
    // включая буквы, Enter, Tab и Backspace. Комбинации с
    // модификаторами не трогаем — тем же Super+F поиск и
    // закрывается, и переключение стола из него работает.
    //
    // Символ берём из ТЕКУЩЕЙ раскладки (modified_sym), а не
    // из латинской: в поле ввода буква — это буква, и
    // русское имя окна надо уметь набрать по-русски.
    if state.search_open() && !logo && !ctrl && !alt {
        let ch = handle.modified_sym().key_char();
        if state.search_key(raw_latin, ch) {
            return FilterResult::Intercept(());
        }
    }

    // Открытое меню блютуза забирает ГОЛЫЕ клавиши: стрелки,
    // Enter и буквы действий принадлежат ему, а не клиенту
    // под курсором (см. bluetooth.rs). Комбинации с
    // модификаторами не трогаем — иначе тот же Super+Shift+B,
    // которым меню открыли, не смог бы его закрыть, а вместе
    // с ним отвалились бы и переключение стола, и VT.
    // Раскладка латинская: в русской иначе не сработали бы
    // D/F/S/P.
    if state.bt_menu_open() && !logo && !ctrl && !alt {
        if state.bt_key(raw_latin) {
            return FilterResult::Intercept(());
        }
    }

    // Меню вайфая и звука — как блютузное: голые клавиши
    // принадлежат им. Вайфаю нужен ещё и СИМВОЛ: в поле
    // пароля буквы это буквы, а не команды.
    if state.wifi_menu_open() && !logo && !ctrl && !alt {
        let ch = handle.modified_sym().key_char();
        if state.wifi_key(raw_latin, ch) {
            return FilterResult::Intercept(());
        }
    }
    if state.audio_menu_open() && !logo && !ctrl && !alt {
        if state.audio_key(raw_latin) {
            return FilterResult::Intercept(());
        }
    }

    // Открытая полка забирает только Esc — остальные
    // клавиши ей не нужны, и отнимать их у клиента незачем.
    if !logo && !ctrl && !alt && state.tray_key(raw_latin) {
        return FilterResult::Intercept(());
    }

    // Панель управления раздачей (повторное Super+Shift+S):
    // стрелки, j/k, x — выгнать, b — забанить, s —
    // закончить раздачу, Esc — закрыть. Как и у остальных
    // меню, забирает только ГОЛЫЕ клавиши: сочетание с
    // Super должно продолжать работать, иначе панель нельзя
    // было бы закрыть тем же Super+Shift+S, которым открыли.
    if state.раздача_панель_открыта() && !logo && !ctrl && !alt {
        if state.раздача_клавиша(raw_latin) {
            return FilterResult::Intercept(());
        }
    }

    // ── Захват клавиатуры приложением (keyboard_grab_apps) ──
    //
    // Пока в фокусе окно из списка (по умолчанию `dshare`),
    // ВСЕ клавиши принадлежат ему: ни один бинд композитора
    // не срабатывает. Без этого гость мультиюзера не мог бы
    // отдать чужому столу ни Super+D, ни Super+1, ни
    // Super+Q — их съедал бы его собственный dawn.
    //
    // Super+Shift+Escape проверяется ПЕРЕД захватом и
    // никогда ему не отдаётся: это аварийный выход. Повисни
    // окно с захватом — человек остался бы в своём сеансе
    // без единой рабочей команды.
    if logo && shift && raw_latin == keysyms::KEY_Escape {
        state.захват_клавиш_снят = !state.захват_клавиш_снят;
        tracing::info!(
            "dawn: захват клавиш приложением {}",
            if state.захват_клавиш_снят { "СНЯТ вручную" } else { "возвращён" },
        );
        return FilterResult::Intercept(());
    }
    if !state.захват_клавиш_снят && state.клавиши_забирает_окно() {
        return FilterResult::Forward;
    }

    // ── Биндинги из Lua-конфига (см. src/config.rs, default_config.lua) ──
    let mods = crate::config::ModMask { ctrl, alt, shift, logo };
    if let Some(action) = state.lua_config.find_action(mods, raw_latin) {
        if из_игры && matches!(action, crate::config::Action::MineMode) {
            tracing::warn!("dawn/mine: выйти из режима можно только с клавиатуры хозяина");
            return FilterResult::Intercept(());
        }
        state.dispatch_action(action);
        return FilterResult::Intercept(());
    }

    FilterResult::Forward
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Кадры жеста: `шаг` canvas-пикселей каждые `период` мс, `сколько` штук.
    /// Возвращает время последнего кадра.
    fn кадры(
        drift: &mut EdgeDrift,
        t0: Instant,
        мс: u32,
        шаг: f64,
        период: u32,
        сколько: u32,
    ) -> u32 {
        let mut t = мс;
        for _ in 0..сколько {
            drift.note(
                Point::from((шаг, 0.0)),
                t,
                t0 + Duration::from_millis(t as u64),
            );
            t += период;
        }
        t - период
    }

    /// Главная регрессия: палец упёрся в бортик и продолжает елозить по нему
    /// мелкими шагами. Раньше эта дрожь вытесняла быстрые кадры из окна выборки
    /// и сбрасывала разгон — в логе 20260816_131327 скорость сползала 352 → 253
    /// и проваливалась под порог, доводка жила один кадр из шестидесяти.
    /// Теперь она обязана начаться и НЕ прерваться.
    #[test]
    fn дрожь_у_бортика_не_отменяет_доводку() {
        let t0 = Instant::now();
        let mut d = EdgeDrift::new(t0);

        // Полноценный жест: 6 px за кадр каждые 8 мс ≈ 750 canvas-px/с.
        let t = кадры(&mut d, t0, 0, 6.0, 8, 30);
        // Палец в бортике: те же кадры, но по 0.2 px.
        let t = кадры(&mut d, t0, t + 8, 0.2, 8, 20);

        let простой = t + 8 + EDGE_IDLE.as_millis() as u32 + 1;
        let now = t0 + Duration::from_millis(простой as u64);
        let шаг = d
            .advance(Duration::from_millis(16), now)
            .expect("доводка обязана начаться после упора");
        assert!(шаг.x > 0.0 && шаг.y == 0.0, "курс доводки не тот: {шаг:?}");

        // И продолжиться: следующие кадры дрожи её не гасят.
        let t = кадры(&mut d, t0, простой, 0.2, 8, 10);
        let now = t0 + Duration::from_millis((t + 8 + EDGE_IDLE.as_millis() as u32) as u64);
        assert!(
            d.advance(Duration::from_millis(16), now).is_some(),
            "дрожь прижатого пальца оборвала доводку",
        );
    }

    /// Разгон: сразу после упора доводка идёт медленно и лишь потом выходит на
    /// скорость жеста. Рывок в момент упора читается как «окно выстрелило».
    #[test]
    fn доводка_разгоняется_плавно() {
        let t0 = Instant::now();
        let mut d = EdgeDrift::new(t0);
        let t = кадры(&mut d, t0, 0, 8.0, 8, 30);

        let старт = t + 8 + EDGE_IDLE.as_millis() as u32 + 1;
        let dt = Duration::from_millis(16);
        let первый = d
            .advance(dt, t0 + Duration::from_millis(старт as u64))
            .expect("доводка не началась");
        let поздний = d
            .advance(
                dt,
                t0 + Duration::from_millis((старт + EDGE_RAMP.as_millis() as u32) as u64),
            )
            .expect("доводка оборвалась на разгоне");
        assert!(
            поздний.x > первый.x * 1.5,
            "разгона нет: первый шаг {:.2}, поздний {:.2}",
            первый.x, поздний.x,
        );
    }

    /// Человек просто остановился посреди медленного жеста — вести окно дальше
    /// нельзя, он его именно ставит. Это тот самый случай, ради которого нужен
    /// порог по скорости: у упора в бортик скорость до него высокая, у
    /// осмысленной остановки — нет.
    #[test]
    fn медленный_жест_не_едет_сам() {
        let t0 = Instant::now();
        let mut d = EdgeDrift::new(t0);
        // 1 px за кадр ≈ 125 canvas-px/с — ниже EDGE_MIN_PEAK.
        let t = кадры(&mut d, t0, 0, 1.0, 8, 40);
        let now = t0 + Duration::from_millis((t + 8 + 500) as u64);
        assert!(d.advance(Duration::from_millis(16), now).is_none());
    }

    /// Короткий тычок: скорость высокая, а вести после него нечего — окно
    /// уехало бы само от одного касания панели.
    #[test]
    fn короткий_тычок_не_едет() {
        let t0 = Instant::now();
        let mut d = EdgeDrift::new(t0);
        let t = кадры(&mut d, t0, 0, 6.0, 8, 3);
        assert!(d.travel < EDGE_MIN_TRAVEL, "тычок вышел слишком длинным");
        let now = t0 + Duration::from_millis((t + 8 + 500) as u64);
        assert!(d.advance(Duration::from_millis(16), now).is_none());
    }

    /// Пальцы поехали снова — доводка обязана уступить живому вводу и начать
    /// разгон заново, а не складываться с ним.
    #[test]
    fn возобновлённое_движение_обрывает_доводку() {
        let t0 = Instant::now();
        let mut d = EdgeDrift::new(t0);
        let t = кадры(&mut d, t0, 0, 6.0, 8, 30);
        let старт = t + 8 + EDGE_IDLE.as_millis() as u32 + 1;
        assert!(d
            .advance(Duration::from_millis(16), t0 + Duration::from_millis(старт as u64))
            .is_some());

        let t = кадры(&mut d, t0, старт, 6.0, 8, 10);
        let now = t0 + Duration::from_millis((t + 8) as u64);
        assert!(
            d.advance(Duration::from_millis(16), now).is_none(),
            "доводка едет поверх живого жеста",
        );
    }

    /// Предохранитель: без финального кадра libinput доводка не должна тянуть
    /// окно по холсту вечно.
    #[test]
    fn предохранитель_останавливает_бесконечную_доводку() {
        let t0 = Instant::now();
        let mut d = EdgeDrift::new(t0);
        let t = кадры(&mut d, t0, 0, 6.0, 8, 30);
        let старт = t + 8 + EDGE_IDLE.as_millis() as u32 + 1;
        let dt = Duration::from_millis(16);
        assert!(d
            .advance(dt, t0 + Duration::from_millis(старт as u64))
            .is_some());
        let поздно = старт as u64 + EDGE_MAX_RUN.as_millis() as u64 + 100;
        assert!(d.advance(dt, t0 + Duration::from_millis(поздно)).is_none());
    }
}
