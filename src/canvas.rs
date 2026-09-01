use std::collections::VecDeque;
use std::time::Duration;

use smithay::utils::{Logical, Physical, Point, Rectangle, Size};


#[cfg(test)]
/// A position in screen-local coordinates (0,0 = top-left of the output).
#[derive(Debug, Clone, Copy)]
pub struct ScreenPos(pub Point<f64, Logical>);

#[cfg(test)]
/// A position in infinite canvas coordinates (absolute world position).
#[derive(Debug, Clone, Copy)]
pub struct CanvasPos(pub Point<f64, Logical>);

/// screen_pos = (canvas_pos - camera) * zoom  ⟹  canvas = screen / zoom + camera
#[cfg(test)]
#[inline]
pub fn screen_to_canvas(screen: ScreenPos, camera: Point<f64, Logical>, zoom: f64) -> CanvasPos {
    CanvasPos(Point::from((
        screen.0.x / zoom + camera.x,
        screen.0.y / zoom + camera.y,
    )))
}

/// canvas_pos → screen_pos = (canvas - camera) * zoom
#[cfg(test)]
#[inline]
pub fn canvas_to_screen(canvas: CanvasPos, camera: Point<f64, Logical>, zoom: f64) -> ScreenPos {
    ScreenPos(Point::from((
        (canvas.0.x - camera.x) * zoom,
        (canvas.0.y - camera.y) * zoom,
    )))
}

/// Convert internal canvas coords (top-left origin, Y-down) to the user-facing
/// window-rule convention (center, Y-up) used by config rules, the state file, and IPC.
#[cfg(test)]
#[inline]
pub fn internal_to_rule(loc: Point<i32, Logical>, size: Size<i32, Logical>) -> (i32, i32) {
    (loc.x + size.w / 2, -(loc.y + size.h / 2))
}

/// Inverse of [`internal_to_rule`]: window-rule coords (center, Y-up) back to
/// internal top-left, Y-down canvas coords.
#[inline]
#[cfg(test)]
pub fn rule_to_internal(x: i32, y: i32, size: Size<i32, Logical>) -> Point<i32, Logical> {
    Point::from((x - size.w / 2, -y - size.h / 2))
}

/// A screen-pinned window's top-left screen position (output-relative, top-left
/// origin, Y-down) for a window-rule `position` `(x, y)` — window center,
/// output-center origin, Y-up — on an output of `output_size`. Callers clamp the
/// result into the output. Inverse of [`screen_top_left_to_rule`].
#[inline]
#[cfg(test)]
pub fn rule_to_screen_top_left(
    x: i32,
    y: i32,
    size: Size<i32, Logical>,
    output_size: Size<i32, Logical>,
) -> Point<i32, Logical> {
    let internal = rule_to_internal(x, y, size);
    Point::from((
        output_size.w / 2 + internal.x,
        output_size.h / 2 + internal.y,
    ))
}

/// Inverse of [`rule_to_screen_top_left`]: a screen-pinned window's top-left
/// screen position back to window-rule coords (window center, output-center
/// origin, Y-up) — the numbers a `pinned_to_screen` rule's `position` takes, so
/// `driftwm msg state` values paste straight into a rule.
#[inline]
#[cfg(test)]
pub fn screen_top_left_to_rule(
    screen_pos: Point<i32, Logical>,
    size: Size<i32, Logical>,
    output_size: Size<i32, Logical>,
) -> (i32, i32) {
    let internal = Point::from((
        screen_pos.x - output_size.w / 2,
        screen_pos.y - output_size.h / 2,
    ));
    internal_to_rule(internal, size)
}

/// The viewport center in canvas coords, in the user-facing convention (Y-up).
/// Shared by the state file and IPC so they can't drift. Inverse of
/// [`camera_for_center`].
#[inline]
#[cfg(test)]
pub fn viewport_center(
    camera: Point<f64, Logical>,
    zoom: f64,
    viewport: Size<i32, Logical>,
) -> (f64, f64) {
    (
        camera.x + viewport.w as f64 / (2.0 * zoom),
        -(camera.y + viewport.h as f64 / (2.0 * zoom)),
    )
}

/// The camera (internal top-left, Y-down) that centers the viewport on the Y-up
/// point `(x, y)`. Inverse of [`viewport_center`].
#[inline]
#[cfg(test)]
pub fn camera_for_center(
    x: f64,
    y: f64,
    zoom: f64,
    viewport: Size<i32, Logical>,
) -> Point<f64, Logical> {
    Point::from((
        x - viewport.w as f64 / (2.0 * zoom),
        -y - viewport.h as f64 / (2.0 * zoom),
    ))
}

/// Compute the camera position that centers a window at `screen_center` on screen.
/// `screen_center` is the screen-space point where the window center should appear
/// (typically the usable area center, accounting for panel exclusive zones).
#[cfg(test)]
pub fn camera_to_center_window(
    window_loc: Point<i32, Logical>,
    window_size: Size<i32, Logical>,
    screen_center: Point<f64, Logical>,
    zoom: f64,
    bar: i32,
) -> Point<f64, Logical> {
    let window_center_x = window_loc.x as f64 + window_size.w as f64 / 2.0;
    let bar_f = bar as f64;
    let window_center_y = window_loc.y as f64 - bar_f + (window_size.h as f64 + bar_f) / 2.0;
    Point::from((
        window_center_x - screen_center.x / zoom,
        window_center_y - screen_center.y / zoom,
    ))
}

/// Fraction of a rectangle's area visible in the current viewport (0.0–1.0).
/// Returns 0.0 for zero-area rectangles.
#[cfg(test)]
pub fn visible_fraction(
    rect_loc: Point<i32, Logical>,
    rect_size: Size<i32, Logical>,
    camera: Point<f64, Logical>,
    viewport_size: Size<i32, Logical>,
    zoom: f64,
) -> f64 {
    let area = rect_size.w as f64 * rect_size.h as f64;
    if area <= 0.0 {
        return 0.0;
    }

    let vw = viewport_size.w as f64 / zoom;
    let vh = viewport_size.h as f64 / zoom;

    let ix_min = (rect_loc.x as f64).max(camera.x);
    let ix_max = ((rect_loc.x + rect_size.w) as f64).min(camera.x + vw);
    let iy_min = (rect_loc.y as f64).max(camera.y);
    let iy_max = ((rect_loc.y + rect_size.h) as f64).min(camera.y + vh);

    let iw = (ix_max - ix_min).max(0.0);
    let ih = (iy_max - iy_min).max(0.0);

    (iw * ih) / area
}

/// The canvas rectangle visible at the current camera + zoom.
/// Used to cull windows outside the viewport for `render_elements_for_region`.
///
/// `camera_i32` must be `camera.to_i32_round()` — the same rounding used by
/// `update_output_from_camera` — so that element position offsets match the
/// output mapping used for input hit-testing.
pub fn visible_canvas_rect(
    camera_i32: Point<i32, Logical>,
    viewport_size: Size<i32, Logical>,
    zoom: f64,
) -> Rectangle<i32, Logical> {
    let w = (viewport_size.w as f64 / zoom).ceil() as i32 + 2;
    let h = (viewport_size.h as f64 / zoom).ceil() as i32 + 2;
    Rectangle::new(camera_i32, (w, h).into())
}

// `all_windows_bbox` и `zoom_to_fit` жили здесь ради миникарты: она вписывала
// в панель bbox ВСЕХ окон. С переходом на вид вокруг камеры (см. MinimapView)
// вписывать стало нечего, и последний вызывающий исчез. Обзор столов свою
// подгонку считает сам (overview::overview_fit_all) — по ячейкам сетки, а не по
// окнам, так что общего кода тут не остаётся.

/// Camera position that keeps `anchor_canvas` at `anchor_screen` after a zoom change.
/// Derived from: screen = (canvas - camera) * zoom  ⟹  camera = canvas - screen / zoom.
pub fn zoom_anchor_camera(
    anchor_canvas: Point<f64, Logical>,
    anchor_screen: Point<f64, Logical>,
    new_zoom: f64,
) -> Point<f64, Logical> {
    Point::from((
        anchor_canvas.x - anchor_screen.x / new_zoom,
        anchor_canvas.y - anchor_screen.y / new_zoom,
    ))
}

/// Sliding-window velocity tracker for scroll/gesture input.
/// Computes launch velocity from recent displacement over a fixed time window,
/// avoiding the EMA bias where the last 1-2 events dominate.
///
/// Timestamps are libinput event times (ms), not processing time: under CPU
/// load the event loop can drain a burst of events with near-identical
/// processing times, which collapses `elapsed` and explodes the launch velocity.
/// Event times are stamped when the input occurred, so they retain real spacing.
#[derive(Clone, Default)]
pub struct VelocityTracker {
    samples: VecDeque<(u32, Point<f64, Logical>)>,
}

const VELOCITY_WINDOW_MS: u32 = 80;

impl VelocityTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, time_ms: u32, delta: Point<f64, Logical>) {
        self.samples.push_back((time_ms, delta));
        // wrapping_sub keeps eviction correct across the u32 ms wrap (~49.7 days).
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| time_ms.wrapping_sub(*t) > VELOCITY_WINDOW_MS)
        {
            self.samples.pop_front();
        }
    }

    /// Total displacement / elapsed time = px/sec. Zero if < 2 samples.
    pub fn launch_velocity(&self) -> Point<f64, Logical> {
        if self.samples.len() < 2 {
            return Point::from((0.0, 0.0));
        }
        let first_time = self.samples.front().unwrap().0;
        let last_time = self.samples.back().unwrap().0;
        let elapsed_ms = last_time.wrapping_sub(first_time);
        // Event times are ms-quantized, so a sub-ms window (only reachable by a
        // sub-millisecond flick on a >1000Hz device) would divide by zero. Guard
        // the clock resolution, not a device rate: any real fling spans many ms,
        // so no device is throttled.
        if elapsed_ms == 0 {
            return Point::from((0.0, 0.0));
        }
        let elapsed = elapsed_ms as f64 / 1000.0;
        let total: Point<f64, Logical> = self
            .samples
            .iter()
            .fold(Point::from((0.0, 0.0)), |acc, (_, d)| {
                Point::from((acc.x + d.x, acc.y + d.y))
            });
        Point::from((total.x / elapsed, total.y / elapsed))
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }
}

/// Stop threshold in px/sec (15 px/sec ≈ 0.25 px/frame at 60Hz)
const MOMENTUM_STOP_THRESHOLD: f64 = 15.0;

/// Scroll momentum physics with time-based drift.
/// Velocity is in px/sec; drift is applied via `powf(dt * 60)` for
/// frame-rate independence.
#[derive(Clone)]
pub struct MomentumState {
    pub velocity: Point<f64, Logical>,
    pub tracker: VelocityTracker,
    pub drift: f64,
    pub coasting: bool,
}

impl MomentumState {
    pub fn new(drift: f64) -> Self {
        Self {
            velocity: Point::from((0.0, 0.0)),
            tracker: VelocityTracker::new(),
            drift,
            coasting: false,
        }
    }

    /// Record an input delta. Resets coasting — we're receiving live input.
    /// `time_ms` is the libinput event timestamp, not processing time.
    pub fn accumulate(&mut self, delta: Point<f64, Logical>, time_ms: u32) {
        self.tracker.push(time_ms, delta);
        self.coasting = false;
    }

    /// Snapshot launch velocity from the tracker and begin coasting.
    pub fn launch(&mut self) {
        self.velocity = self.tracker.launch_velocity();
        self.coasting = true;
        self.tracker.clear();
    }

    /// Advance momentum by `dt`. Returns Some(canvas delta) to apply, or None.
    pub fn tick(&mut self, dt: Duration) -> Option<Point<f64, Logical>> {
        if !self.coasting {
            return None;
        }
        let speed = (self.velocity.x.powi(2) + self.velocity.y.powi(2)).sqrt();
        if speed < MOMENTUM_STOP_THRESHOLD {
            self.velocity = Point::from((0.0, 0.0));
            self.coasting = false;
            return None;
        }

        let dt_secs = dt.as_secs_f64();

        // Speed-dependent drift: gentle scrolls stop quickly, fast flings coast longer
        let effective_drift = speed_dependent_drift(self.drift, speed);
        let decay = effective_drift.powf(dt_secs * 60.0);
        let delta = Point::from((self.velocity.x * dt_secs, self.velocity.y * dt_secs));
        self.velocity = Point::from((self.velocity.x * decay, self.velocity.y * decay));
        Some(delta)
    }

    pub fn stop(&mut self) {
        self.velocity = Point::from((0.0, 0.0));
        self.tracker.clear();
        self.coasting = false;
    }
}

/// Per-frame velocity retention for momentum coasting, from the user's `drift`
/// knob (0 = off … 1 = floatiest) and the current `speed`.
///
/// The knob is log-spaced in coast time: each step multiplies how long a fling
/// coasts by a roughly constant factor, so the slider feels perceptually even
/// instead of cramming every usable value into 0.9–1.0. Gentle scrolls (low
/// speed) stop sooner than hard flings (high speed). The result is normalized to
/// 60fps; `tick` applies `powf(dt * 60)` for frame-rate independence.
fn speed_dependent_drift(drift: f64, speed: f64) -> f64 {
    if drift <= 0.0 {
        return 0.0; // momentum disabled
    }
    // Fling coast time as a velocity half-life (seconds), spaced geometrically
    // across the knob. Endpoints and the default (0.5) are tuned so 0.5
    // reproduces the original feel (≈0.88 slow / ≈0.965 fast retention).
    const FLING_HALFLIFE_MIN: f64 = 0.05;
    const FLING_HALFLIFE_MAX: f64 = 2.3;
    const SLOW_COAST_RATIO: f64 = 0.28; // gentle scrolls coast ~1/3.6 as long
    let fling = FLING_HALFLIFE_MIN * (FLING_HALFLIFE_MAX / FLING_HALFLIFE_MIN).powf(drift.min(1.0));
    let reference_speed = 2500.0; // px/sec; at or above this, full fling coast
    let t = (speed / reference_speed).min(1.0);
    let half_life = fling * SLOW_COAST_RATIO.powf(1.0 - t);
    // Retention that halves the velocity every `half_life` seconds.
    0.5_f64.powf(1.0 / (60.0 * half_life)).min(0.995)
}

// ── Миникарта (Module 3) ────────────────────────────────────────────────────
// Карта — фиксированного физического (экранного) размера, независимо от zoom
// холста (как курсор — вручную позиционируется в Physical, а не идёт через
// Space/output scale).

pub const MINIMAP_MARGIN_PX: i32 = 24;

/// Доля экрана под карточку карты.
///
/// **25.08.2026: карта переехала из угла в почти полноэкранную карточку.** До
/// этого панель была 320×200 в правом верхнем углу, и в неё всё упиралось: при
/// потолке зума 8.0 масштаб выходил ~0.33, то есть окно 1600 px рисовалось на
/// 530 px — в 288 px содержимого не влезало и уж точно не читалось. Ярик
/// 25.08.2026 (видео-образец): карта — это «Открытые окна» во весь стол, где
/// зум доводится до 1:1 и миниатюра превращается в НАСТОЯЩИЙ предпросмотр
/// живого окна с подписью-заголовком. На той же ширине содержимого (2165 px
/// при экране 2560) зум 8.0 даёт масштаб 2.26 — предпросмотр крупнее оригинала.
const MINIMAP_CARD_W_FRAC: f64 = 0.86;
const MINIMAP_CARD_H_FRAC: f64 = 0.74;
/// Не давать карточке выродиться на крошечном выходе (headless-тесты, 640×480).
const MINIMAP_CARD_MIN_W: i32 = 320;
const MINIMAP_CARD_MIN_H: i32 = 200;

/// Поле между краем карточки и содержимым. В проекции оно НЕ масштабируется:
/// [`project_minimap`] считает координаты ОТНОСИТЕЛЬНО содержимого, а поле
/// прибавляет уже отрисовка — поэтому живые миниатюры в udev берут точкой
/// масштабирования угол содержимого, а не пририсовывают поле к холсту.
pub const MINIMAP_PADDING_PX: f64 = 18.0;
/// Полоса заголовка внутри карточки: слева «Открытые окна», справа подсказка
/// про сброс. Съедается сверху у содержимого, а не рисуется поверх него.
pub const MINIMAP_HEADER_PX: i32 = 30;
/// Где карточка и где внутри неё карта.
///
/// Обе величины обязаны считаться в одном месте: по `content` идёт и проекция
/// окон, и хит-тест клика, и обои, и сетка. Разъехавшись, они снова уносят
/// клик мимо окна — той же граблёй, из-за которой у `project_minimap` общий
/// параметр вида.
#[derive(Clone, Copy, Debug)]
pub struct MinimapGeom {
    /// Карточка целиком (плашка со скруглением и заголовком).
    pub panel: Rectangle<i32, Physical>,
    /// Область карты внутри карточки: панель минус поля минус заголовок.
    pub content: Rectangle<i32, Physical>,
}

/// Карточка карты — по центру экрана, чуть ниже середины по вертикали не
/// уходит: бар сверху остаётся виден, настоящие окна видно по краям (так и в
/// образце — карта лежит ПОВЕРХ стола, а не заменяет его).
pub fn minimap_geom(output_physical_size: Size<i32, Physical>) -> MinimapGeom {
    let экран_w = output_physical_size.w.max(1);
    let экран_h = output_physical_size.h.max(1);
    let w = ((экран_w as f64 * MINIMAP_CARD_W_FRAC).round() as i32)
        .clamp(MINIMAP_CARD_MIN_W.min(экран_w), экран_w);
    let h = ((экран_h as f64 * MINIMAP_CARD_H_FRAC).round() as i32)
        .clamp(MINIMAP_CARD_MIN_H.min(экран_h), экран_h);
    // По вертикали центрируем в полосе ПОД баром, а не в экране целиком.
    let верх = (crate::bar::TOP + crate::bar::H + MINIMAP_MARGIN_PX).min(экран_h - h).max(0);
    let свободно = (экран_h - верх - h).max(0);
    let panel = Rectangle::new(
        Point::from(((экран_w - w) / 2, верх + свободно / 2)),
        Size::from((w, h)),
    );

    let поле = MINIMAP_PADDING_PX.round() as i32;
    let content = Rectangle::new(
        Point::from((panel.loc.x + поле, panel.loc.y + поле + MINIMAP_HEADER_PX)),
        Size::from((
            (w - поле * 2).max(1),
            (h - поле * 2 - MINIMAP_HEADER_PX).max(1),
        )),
    );
    MinimapGeom { panel, content }
}

/// Насколько карта ужата к своему центру в начале раскрытия. 0.88 — карточка
/// стартует чуть меньше себя и «распахивается»; ниже этого движение читается
/// уже не как раскрытие, а как прилёт откуда-то сбоку.
const MINIMAP_REVEAL_MIN_K: f64 = 0.88;

/// Раскрытие/сворачивание карты: `slide = 1` — карточка целиком, `slide = 0` —
/// от неё не осталось ничего.
///
/// **26.08.2026: диафрагма вместо шторки.** До этого росла только ВЫСОТА
/// (карточка раскрывалась сверху вниз), потому что живое содержимое окон нечем
/// было гасить: считалось, что `RescaleRenderElement`/`CropRenderElement` альфы
/// не несут. Несёт её сам источник — `Window::render_elements(.., alpha)`, и с
/// ним карта проявляется целиком. Поэтому теперь карточка распахивается ОТ
/// ЦЕНТРА (`MINIMAP_REVEAL_MIN_K` → 1.0) и одновременно проявляется, а половины
/// карточки в кадре не бывает ни на одном кадре анимации.
///
/// Считать это должны все, кто имеет дело с картой: и отрисовка, и хит-тест
/// (см. `Dawn::minimap_hit`). Нераскрытая карточка не имеет права ни рисовать
/// то, что под ней, ни ловить по нему клики.
pub fn minimap_reveal(panel: Rectangle<i32, Physical>, slide: f64) -> Rectangle<i32, Physical> {
    let доля = slide.clamp(0.0, 1.0);
    if доля <= 0.0 {
        return Rectangle::new(panel.loc, Size::from((0, 0)));
    }
    let k = MINIMAP_REVEAL_MIN_K + (1.0 - MINIMAP_REVEAL_MIN_K) * crate::anim::ease_out_cubic(доля);
    let w = ((panel.size.w as f64 * k).round() as i32).clamp(1, panel.size.w);
    let h = ((panel.size.h as f64 * k).round() as i32).clamp(1, panel.size.h);
    Rectangle::new(
        Point::from((
            panel.loc.x + (panel.size.w - w) / 2,
            panel.loc.y + (panel.size.h - h) / 2,
        )),
        Size::from((w, h)),
    )
}

/// Насколько карта видна (альфа всего, что в ней нарисовано) при этой доле
/// раскрытия. Отдельно от [`minimap_reveal`]: размер догоняет цель мягко, а
/// проявление обязано быть быстрее — иначе карточка кажется мутной ровно
/// столько, сколько едет.
pub fn minimap_fade(slide: f64) -> f32 {
    let доля = slide.clamp(0.0, 1.0);
    crate::anim::ease_out_cubic(доля.powf(0.7)) as f32
}

/// Сколько ЭКРАНОВ по ширине холста видно в миникарте при её зуме 1.0.
pub const MINIMAP_SPAN_SCREENS: f64 = 3.0;
/// Границы собственного зума миникарты. Раздвинуты (было 0.25…6.0) вместе с
/// переходом на АВТОМАТИЧЕСКИЙ зум: колесо крутил человек и в край упирался
/// сам, а подгонка под разлетевшийся холст обязана дотягиваться дальше.
pub const MINIMAP_ZOOM_MIN: f64 = 0.02;
pub const MINIMAP_ZOOM_MAX: f64 = 8.0;

/// Запас вокруг показываемого куска холста при автоподгонке: 12% — чтобы окно
/// у самого края не упиралось в кромку панели.
const MINIMAP_FIT_MARGIN: f64 = 1.12;

/// Зум миникарты, при котором `union` (кусок холста, который надо показать)
/// целиком влезает в карту.
///
/// Обратная арифметика к [`project_minimap`]: там `видно_w = экран_w ·
/// MINIMAP_SPAN_SCREENS / зум`, а `scale = содержимое_w / видно_w`. Значит,
/// чтобы влезла и высота, показанная ширина обязана быть не меньше
/// `H · содержимое_w / содержимое_h` — иначе карта обрежет верх и низ.
pub fn minimap_auto_zoom(
    union: Size<f64, Logical>,
    screen: Size<i32, Logical>,
    content: Size<i32, Physical>,
) -> f64 {
    let cw = (content.w as f64).max(1.0);
    let ch = (content.h as f64).max(1.0);
    let экран_w = (screen.w as f64).max(1.0);
    let w = (union.w * MINIMAP_FIT_MARGIN).max(1.0);
    let h = (union.h * MINIMAP_FIT_MARGIN).max(1.0);
    let видно_w = w.max(h * cw / ch);
    let зум = экран_w * MINIMAP_SPAN_SCREENS / видно_w;
    if !зум.is_finite() {
        return 1.0;
    }
    зум.clamp(MINIMAP_ZOOM_MIN, MINIMAP_ZOOM_MAX)
}

/// Масштаб карты (физические пиксели карты на один логический пиксель
/// холста) при данном зуме — та же формула, что внутри [`project_minimap`],
/// нужна отдельно: драг карты переводит дельту курсора в сдвиг ЕЁ вида, а
/// не строит проекцию окон.
pub fn minimap_scale(zoom: f64, screen_w: i32, content_w: i32) -> f64 {
    let cw = (content_w as f64).max(1.0);
    let экран_w = (screen_w as f64).max(1.0);
    let зум = zoom.clamp(MINIMAP_ZOOM_MIN, MINIMAP_ZOOM_MAX);
    let видно_w = (экран_w * MINIMAP_SPAN_SCREENS / зум).max(1.0);
    cw / видно_w
}

/// Что миникарта показывает — СВОЙ кадр, отдельный от камеры холста.
///
/// Раньше кадра не было вовсе: панель строила bbox по всем окнам текущего стола
/// и вписывала его целиком. Из-за этого миникарта жила своей жизнью — стоило
/// утащить одно окно далеко в сторону, и масштаб падал так, что остальные
/// превращались в точки. Теперь панель показывает окрестность камеры, а
/// подробность выбирает пользователь.
#[derive(Clone, Copy, Debug)]
pub struct MinimapView {
    /// Центр показываемой области холста: центр камеры плюс собственный пан.
    pub center: Point<f64, Logical>,
    /// Собственный зум: 1.0 = MINIMAP_SPAN_SCREENS экранов по ширине.
    pub zoom: f64,
}

pub struct MinimapBox {
    /// Координаты относительно левого-верхнего угла СОДЕРЖИМОГО карты
    /// (`MinimapGeom::content.loc`), не экрана и не карточки целиком.
    pub loc: Point<i32, Physical>,
    pub size: Size<i32, Physical>,
    pub focused: bool,
}

pub struct MinimapProjection {
    pub boxes: Vec<MinimapBox>,
    /// Для обратного клика (3.3)
    pub bbox: Rectangle<i32, Logical>,
    pub scale: f64,
}

/// Проекция окон на миникарту — вид на окрестность камеры в собственном
/// масштабе панели (см. [`MinimapView`]).
///
/// Возвращаемые `bbox`/`scale` — это по-прежнему «что и во сколько раз
/// показано»: обратный пересчёт клика ([`minimap_click_to_canvas`]), проекция
/// точки ([`project_point_minimap`]) и живые миниатюры в `udev.rs` считают по
/// ним и менять их не пришлось. Изменилось только КАК они выбираются: раньше —
/// bbox всех окон с запасом 20%, теперь — прямоугольник вокруг центра вида.
///
/// Закладки камеры больше не расширяют кадр (`extra` исчез). Раньше это было
/// нужно, чтобы одинокая закладка в стороне не выпадала за панель; теперь кадр
/// вообще не обязан вмещать всё, и закладка вне вида честно не показывается —
/// зато её видно, стоит подъехать. Хит-тест клика и отрисовка обязаны звать
/// проекцию с ОДНИМ И ТЕМ ЖЕ видом: разъехавшись, они снова унесут камеру мимо.
pub fn project_minimap(
    windows: &[(Point<i32, Logical>, Size<i32, Logical>, bool)],
    вид: MinimapView,
    screen: Size<i32, Logical>,
    content: Size<i32, Physical>,
) -> MinimapProjection {
    let содержимое_w = (content.w as f64).max(1.0);
    let содержимое_h = (content.h as f64).max(1.0);

    // Сколько холста влезает по ширине — отсюда и масштаб. Экран нулевой ширины
    // (выход ещё не настроен) не должен давать деления на ноль.
    let экран_w = (screen.w as f64).max(1.0);
    let зум = вид.zoom.clamp(MINIMAP_ZOOM_MIN, MINIMAP_ZOOM_MAX);
    let видно_w = экран_w * MINIMAP_SPAN_SCREENS / зум;
    let scale = содержимое_w / видно_w;

    // Центр СОДЕРЖИМОГО панели должен приходиться ровно на центр вида: проекция
    // считает `поле + (точка − bbox.loc) * scale`, значит
    // `bbox.loc = центр − (половина содержимого) / scale`.
    let bbox = Rectangle::new(
        Point::from((
            (вид.center.x - содержимое_w / (2.0 * scale)).round() as i32,
            (вид.center.y - содержимое_h / (2.0 * scale)).round() as i32,
        )),
        Size::from((
            (содержимое_w / scale).round().max(1.0) as i32,
            (содержимое_h / scale).round().max(1.0) as i32,
        )),
    );

    let project = |loc: Point<i32, Logical>, size: Size<i32, Logical>| -> (Point<i32, Physical>, Size<i32, Physical>) {
        let x = (loc.x - bbox.loc.x) as f64 * scale;
        let y = (loc.y - bbox.loc.y) as f64 * scale;
        let w = (size.w as f64 * scale).max(1.0);
        let h = (size.h as f64 * scale).max(1.0);
        (
            Point::from((x.round() as i32, y.round() as i32)),
            Size::from((w.round() as i32, h.round() as i32)),
        )
    };

    let boxes = windows.iter().map(|(loc, size, focused)| {
        let (p, s) = project(*loc, *size);
        MinimapBox { loc: p, size: s, focused: *focused }
    }).collect();

    MinimapProjection { boxes, bbox, scale }
}

/// Обратное преобразование клика по карте: физические координаты клика
/// ОТНОСИТЕЛЬНО СОДЕРЖИМОГО (`MinimapGeom::content.loc`) → точка на бесконечном
/// холсте.
///
/// Живого вызывающего у неё нет и с возвратом клика по окну не появилось:
/// `Dawn::minimap_window_at` ищет окно не по координате холста, а по ПРОЕКЦИИ —
/// перебирает те же `MinimapBox`, которые отрисовка кладёт на экран, потому что
/// сверять надо именно нарисованное. Оставлена как обратная к проекции — ею
/// проверяется, что карта показывает то место, которое обещала.
#[cfg(test)]
pub fn minimap_click_to_canvas(
    click_in_content: Point<f64, Physical>,
    bbox: Rectangle<i32, Logical>,
    scale: f64,
) -> Point<f64, Logical> {
    Point::from((
        bbox.loc.x as f64 + click_in_content.x / scale,
        bbox.loc.y as f64 + click_in_content.y / scale,
    ))
}

// ── Карточка предпросмотра (наведение на панель) ─────────────────────────────

/// Размер карточки предпросмотра в физических пикселях экрана и поля внутри неё.
pub const PREVIEW_W: i32 = 380;
pub const PREVIEW_H: i32 = 232;
pub const PREVIEW_PAD: i32 = 10;
/// Зазор между панелью и карточкой.
pub const PREVIEW_GAP: i32 = 8;
/// Полоса под подпись у нижнего края карточки.
pub const PREVIEW_LABEL_H: i32 = 18;
/// С какого размера карточка начинает раскрываться и насколько высоко висит в
/// начале движения: она выезжает ИЗ-ПОД панели, а не всплывает на месте.
const PREVIEW_MIN_K: f64 = 0.84;
const PREVIEW_RISE: f64 = 10.0;

/// Где карточка предпросмотра и где внутри неё кадр стола.
///
/// Одна точка правды на отрисовку и на ввод — ровно по той же причине, что у
/// [`MinimapGeom`]: по карточке теперь не только смотрят, но и панят, зумят и
/// кликают, и разъехавшись, эти двое унесут клик мимо окна. Геометрия
/// АНИМИРОВАННАЯ (зависит от доли раскрытия): что нарисовано, то и кликается.
#[derive(Clone, Copy, Debug)]
pub struct PreviewGeom {
    pub card: Rectangle<i32, Physical>,
    pub content: Rectangle<i32, Physical>,
    /// Доля раскрытия, с которой посчитана эта геометрия (для альфы).
    pub anim: f64,
}

/// Карточка под ячейкой панели: по центру ячейки, не вылезая за края экрана.
///
/// `cell_center_x` — середина ячейки панели, `top` — низ панели плюс зазор
/// (обе величины у вызывающего уже есть), `anim` — доля раскрытия 0…1.
pub fn preview_geom(
    cell_center_x: i32,
    top: i32,
    screen_w: i32,
    anim: f64,
) -> PreviewGeom {
    let t = anim.clamp(0.0, 1.0);
    // Размер идёт кривой с лёгким перелётом — карточка «выскакивает», а не
    // растягивается. Перелёт мелкий (см. ease_out_back), поэтому за свои
    // 380×232 карточка выходит на считанные пиксели.
    let k = PREVIEW_MIN_K + (1.0 - PREVIEW_MIN_K) * crate::anim::ease_out_back(t);
    let w = ((PREVIEW_W as f64 * k).round() as i32).max(1);
    let h = ((PREVIEW_H as f64 * k).round() as i32).max(1);
    let подъём = ((1.0 - crate::anim::ease_out_cubic(t)) * PREVIEW_RISE).round() as i32;
    let x = (cell_center_x - w / 2)
        .clamp(crate::bar::EDGE, (screen_w - crate::bar::EDGE - w).max(crate::bar::EDGE));
    let y = top - подъём;
    let card = Rectangle::new(Point::from((x, y)), Size::from((w, h)));

    let поле = ((PREVIEW_PAD as f64) * k).round() as i32;
    let подпись = ((PREVIEW_LABEL_H as f64) * k).round() as i32;
    let content = Rectangle::new(
        Point::from((x + поле, y + поле)),
        Size::from((
            (w - поле * 2).max(1),
            (h - поле * 2 - подпись).max(1),
        )),
    );
    PreviewGeom { card, content, anim: t }
}

/// Насколько карточка видна при этой доле раскрытия. Как и у карты, проявление
/// чуть обгоняет размер: карточка мелкая, и «выцветшая» она читается хуже, чем
/// просто маленькая.
pub fn preview_fade(anim: f64) -> f32 {
    crate::anim::ease_out_cubic(anim.clamp(0.0, 1.0).powf(0.6)) as f32
}

// ── Мини-копия мира в произвольной коробке (карточки предпросмотра) ──────────

/// Запас вокруг кадра стола в карточке предпросмотра: окно у самой кромки не
/// должно упираться в край карточки.
const MINI_FIT_MARGIN: f64 = 1.04;

/// Что показывает карточка предпросмотра: точка холста, приходящаяся на ЦЕНТР
/// её поля, и масштаб (физических пикселей карточки на логический пиксель
/// холста).
///
/// Отдельно от [`MinimapView`] нарочно. У карты вид задан в «экранах холста»
/// (`MINIMAP_SPAN_SCREENS`) — ей так удобнее, она показывает окрестность
/// камеры. Карточка же показывает КАДР СТОЛА, вписанный в неё целиком, и её
/// собственный зум считается от этого кадра, а не от ширины монитора.
///
/// Одна структура на отрисовку, хит-тест и пан: разъехавшись, они снова унесут
/// клик мимо окна — той же граблёй, из-за которой у карты общий `MinimapView`.
#[derive(Clone, Copy, Debug)]
pub struct MiniView {
    pub center: Point<f64, Logical>,
    pub scale: f64,
}

/// Границы собственного зума карточки: 1.0 — кадр стола влезает целиком.
pub const MINI_ZOOM_MIN: f64 = 0.2;
pub const MINI_ZOOM_MAX: f64 = 12.0;

impl MiniView {
    /// Вписать кусок холста `base` в поле `content` и домножить на собственный
    /// зум карточки (1.0 — ровно вписанный кадр).
    pub fn fit(
        base: Rectangle<f64, Logical>,
        content: Size<i32, Physical>,
        zoom: f64,
        center: Point<f64, Logical>,
    ) -> Self {
        let cw = (content.w as f64).max(1.0);
        let ch = (content.h as f64).max(1.0);
        let bw = (base.size.w * MINI_FIT_MARGIN).max(1.0);
        let bh = (base.size.h * MINI_FIT_MARGIN).max(1.0);
        let вписан = (cw / bw).min(ch / bh);
        let scale = вписан * zoom.clamp(MINI_ZOOM_MIN, MINI_ZOOM_MAX);
        let scale = if scale.is_finite() && scale > 0.0 { scale } else { 1.0 };
        Self { center, scale }
    }

    /// Точка холста, приходящаяся на левый-верхний угол поля карточки.
    pub fn origin(&self, content: Size<i32, Physical>) -> Point<f64, Logical> {
        Point::from((
            self.center.x - content.w as f64 / (2.0 * self.scale),
            self.center.y - content.h as f64 / (2.0 * self.scale),
        ))
    }

    /// Точка холста → физическая точка внутри карточки (`anchor` — левый-верхний
    /// угол её поля на экране).
    pub fn point(
        &self,
        anchor: Point<i32, Physical>,
        content: Size<i32, Physical>,
        p: Point<f64, Logical>,
    ) -> Point<f64, Physical> {
        let o = self.origin(content);
        Point::from((
            anchor.x as f64 + (p.x - o.x) * self.scale,
            anchor.y as f64 + (p.y - o.y) * self.scale,
        ))
    }

    /// Прямоугольник холста → физический прямоугольник внутри карточки.
    pub fn rect(
        &self,
        anchor: Point<i32, Physical>,
        content: Size<i32, Physical>,
        r: Rectangle<i32, Logical>,
    ) -> Rectangle<i32, Physical> {
        let p = self.point(anchor, content, Point::from((r.loc.x as f64, r.loc.y as f64)));
        Rectangle::new(
            Point::from((p.x.round() as i32, p.y.round() as i32)),
            Size::from((
                (r.size.w as f64 * self.scale).round().max(1.0) as i32,
                (r.size.h as f64 * self.scale).round().max(1.0) as i32,
            )),
        )
    }

    /// Обратно: физическая точка экрана → точка холста. По ней ищется окно под
    /// курсором и считается пан карточки.
    pub fn canvas_at(
        &self,
        anchor: Point<i32, Physical>,
        content: Size<i32, Physical>,
        p: Point<f64, Physical>,
    ) -> Point<f64, Logical> {
        let o = self.origin(content);
        Point::from((
            o.x + (p.x - anchor.x as f64) / self.scale,
            o.y + (p.y - anchor.y as f64) / self.scale,
        ))
    }

    /// Куда положить живое содержимое ДО масштабирования, чтобы
    /// `RescaleRenderElement` от `anchor` с множителем `scale` привёл его ровно
    /// туда же, куда [`MiniView::rect`] кладёт рамку окна.
    ///
    /// Та же тонкость, что у миниатюр карты (`udev::build_minimap_thumbnails`):
    /// Rescale умеет только `anchor + (loc − anchor)·scale`, поэтому позиция до
    /// масштабирования обязана быть `anchor + (холст − origin)`.
    pub fn pre_scale(
        &self,
        anchor: Point<i32, Physical>,
        content: Size<i32, Physical>,
        p: Point<i32, Logical>,
    ) -> Point<i32, Physical> {
        let o = self.origin(content);
        Point::from((
            anchor.x + (p.x as f64 - o.x).round() as i32,
            anchor.y + (p.y as f64 - o.y).round() as i32,
        ))
    }
}

/// Прямая проекция одной точки холста на карту — теми же bbox/scale, что и
/// [`project_minimap`]. Возвращает координаты относительно левого-верхнего
/// угла СОДЕРЖИМОГО (не экрана и не карточки). Используется для крестиков
/// закладок камеры.
pub fn project_point_minimap(
    anchor: Point<f64, Logical>,
    bbox: Rectangle<i32, Logical>,
    scale: f64,
) -> Point<i32, Physical> {
    Point::from((
        ((anchor.x - bbox.loc.x as f64) * scale).round() as i32,
        ((anchor.y - bbox.loc.y as f64) * scale).round() as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }
    fn vp(w: i32, h: i32) -> Size<i32, Logical> {
        Size::from((w, h))
    }
    /// Screen center point for a viewport of given size (no panels).
    fn vp_center(w: i32, h: i32) -> Point<f64, Logical> {
        Point::from((w as f64 / 2.0, h as f64 / 2.0))
    }

    #[test]
    fn rule_coords_round_trip() {
        // internal -> rule -> internal is identity, even for odd sizes where
        // integer halving truncates (same truncated half is used both ways).
        for (loc, size) in [
            ((0, 0), (100, 100)),
            ((200, -300), (640, 480)),
            ((-15, 7), (101, 51)),
        ] {
            let loc = Point::<i32, Logical>::from(loc);
            let size = vp(size.0, size.1);
            let (rx, ry) = internal_to_rule(loc, size);
            assert_eq!(rule_to_internal(rx, ry, size), loc);
        }
    }

    #[test]
    fn rule_coords_center_y_up() {
        assert_eq!(internal_to_rule((0, 0).into(), vp(100, 100)), (50, -50));
    }

    #[test]
    fn pinned_rule_screen_round_trip() {
        // rule coords -> screen top-left -> rule coords is identity, including
        // odd window/output sizes where integer halving truncates (the same
        // truncated halves cancel in both directions).
        for (rule, size, out) in [
            ((0, 0), (320, 240), (1920, 1080)),
            ((200, -150), (640, 480), (1920, 1080)),
            ((-37, 61), (101, 51), (1365, 767)),
            ((450, 320), (100, 100), (801, 601)),
        ] {
            let size = vp(size.0, size.1);
            let out = vp(out.0, out.1);
            let screen = rule_to_screen_top_left(rule.0, rule.1, size, out);
            assert_eq!(screen_top_left_to_rule(screen, size, out), rule);
        }
    }

    #[test]
    fn viewport_center_round_trip() {
        let viewport = vp(1920, 1080);
        for (camera, zoom) in [
            (cam(0.0, 0.0), 1.0),
            (cam(-960.0, -540.0), 1.0),
            (cam(123.0, -45.0), 0.5),
            (cam(-200.0, 300.0), 2.0),
        ] {
            let (x, y) = viewport_center(camera, zoom, viewport);
            let back = camera_for_center(x, y, zoom, viewport);
            assert!((back.x - camera.x).abs() < 1e-9 && (back.y - camera.y).abs() < 1e-9);
        }
    }

    #[test]
    fn camera_for_center_centers_origin() {
        let viewport = vp(1000, 800);
        let camera = camera_for_center(0.0, 0.0, 1.0, viewport);
        assert_eq!(viewport_center(camera, 1.0, viewport), (0.0, 0.0));
    }

    #[test]
    fn fully_visible() {
        // 100x100 window at (200, 200), camera at (0,0), viewport 1000x1000, zoom 1.0
        let f = visible_fraction(
            (200, 200).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fully_off_screen() {
        // Window completely to the right of viewport
        let f = visible_fraction(
            (2000, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.0).abs() < 1e-9);
    }

    #[test]
    fn half_off_right_edge() {
        // 100x100 window, right half off-screen
        let f = visible_fraction(
            (950, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.5).abs() < 1e-9);
    }

    #[test]
    fn zero_area_window() {
        let f = visible_fraction(
            (0, 0).into(),
            (0, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.0).abs() < 1e-9);
    }

    #[test]
    fn zoom_affects_viewport() {
        // At zoom 0.5, viewport covers 2000x2000 canvas units.
        // 100x100 window at (1500, 0) is fully visible.
        let f = visible_fraction(
            (1500, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            0.5,
        );
        assert!((f - 1.0).abs() < 1e-9);

        // Same window at zoom 1.0 is fully off-screen.
        let f = visible_fraction(
            (1500, 0).into(),
            (100, 100).into(),
            cam(0.0, 0.0),
            vp(1000, 1000),
            1.0,
        );
        assert!((f - 0.0).abs() < 1e-9);
    }

    // -- Coordinate transform round-trip tests --

    #[test]
    fn screen_canvas_round_trip_zoom_1() {
        let camera = cam(100.0, 200.0);
        let original = ScreenPos(Point::from((400.0, 300.0)));
        let canvas = screen_to_canvas(original, camera, 1.0);
        let back = canvas_to_screen(canvas, camera, 1.0);
        assert!((back.0.x - original.0.x).abs() < 1e-9);
        assert!((back.0.y - original.0.y).abs() < 1e-9);
    }

    #[test]
    fn screen_canvas_round_trip_zoomed_out() {
        let camera = cam(-500.0, -300.0);
        let zoom = 0.25;
        let original = ScreenPos(Point::from((640.0, 480.0)));
        let canvas = screen_to_canvas(original, camera, zoom);
        let back = canvas_to_screen(canvas, camera, zoom);
        assert!((back.0.x - original.0.x).abs() < 1e-9);
        assert!((back.0.y - original.0.y).abs() < 1e-9);
    }

    #[test]
    fn screen_to_canvas_math() {
        // screen = (canvas - camera) * zoom  ⟹  canvas = screen / zoom + camera
        let canvas = screen_to_canvas(ScreenPos(Point::from((100.0, 50.0))), cam(10.0, 20.0), 0.5);
        // 100/0.5 + 10 = 210, 50/0.5 + 20 = 120
        assert!((canvas.0.x - 210.0).abs() < 1e-9);
        assert!((canvas.0.y - 120.0).abs() < 1e-9);
    }

    #[test]
    fn canvas_to_screen_math() {
        // screen = (canvas - camera) * zoom
        let screen = canvas_to_screen(CanvasPos(Point::from((210.0, 120.0))), cam(10.0, 20.0), 0.5);
        // (210 - 10) * 0.5 = 100, (120 - 20) * 0.5 = 50
        assert!((screen.0.x - 100.0).abs() < 1e-9);
        assert!((screen.0.y - 50.0).abs() < 1e-9);
    }

    // -- camera_to_center_window tests --

    #[test]
    fn center_window_zoom_1() {
        // 200x100 window at (300, 400), 1920x1080 viewport, zoom 1.0
        let cam = camera_to_center_window(
            (300, 400).into(),
            (200, 100).into(),
            vp_center(1920, 1080),
            1.0,
            0,
        );
        // window center: (400, 450), viewport center offset: (960, 540)
        assert!((cam.x - (400.0 - 960.0)).abs() < 1e-9);
        assert!((cam.y - (450.0 - 540.0)).abs() < 1e-9);
    }

    #[test]
    fn center_window_zoomed_out() {
        // At zoom 0.5, viewport center = viewport_size / (2 * 0.5) = viewport_size
        let cam = camera_to_center_window(
            (0, 0).into(),
            (100, 100).into(),
            vp_center(1000, 1000),
            0.5,
            0,
        );
        // window center: (50, 50), viewport center offset at 0.5: (1000, 1000)
        assert!((cam.x - (50.0 - 1000.0)).abs() < 1e-9);
        assert!((cam.y - (50.0 - 1000.0)).abs() < 1e-9);
    }

    /// Карточка карты обязана лежать ВНУТРИ экрана, ниже бара, а её содержимое
    /// — внутри карточки. На это опирается всё остальное: обрезка миниатюр по
    /// карточке, хит-тест клика и обои внутри карты.
    #[test]
    fn карточка_карты_лежит_в_экране_под_баром() {
        for экран in [
            Size::<i32, Physical>::from((1920, 1080)),
            Size::from((2560, 1080)),
            Size::from((3840, 2160)),
            Size::from((640, 480)),
        ] {
            let g = minimap_geom(экран);
            assert!(g.panel.loc.x >= 0 && g.panel.loc.y >= 0, "{экран:?}: {:?}", g.panel);
            assert!(
                g.panel.loc.x + g.panel.size.w <= экран.w
                    && g.panel.loc.y + g.panel.size.h <= экран.h,
                "{экран:?}: карточка торчит за край: {:?}", g.panel,
            );
            assert!(
                g.panel.loc.y >= crate::bar::TOP + crate::bar::H,
                "{экран:?}: карточка залезла на бар: {:?}", g.panel,
            );
            assert!(
                g.content.loc.x > g.panel.loc.x
                    && g.content.loc.y > g.panel.loc.y + MINIMAP_HEADER_PX
                    && g.content.loc.x + g.content.size.w < g.panel.loc.x + g.panel.size.w
                    && g.content.loc.y + g.content.size.h <= g.panel.loc.y + g.panel.size.h,
                "{экран:?}: содержимое вылезло из карточки: {:?} в {:?}", g.content, g.panel,
            );
        }
    }

    /// Диафрагма: закрытая карта не показывает НИЧЕГО, открытая — карточку
    /// целиком, а между ними карточка распахивается ОТ ЦЕНТРА, не выходя за
    /// свои края. Перелёт анимации за [0,1] не должен ни оставлять полоску, ни
    /// рисовать больше карточки.
    #[test]
    fn карта_распахивается_от_центра() {
        let g = minimap_geom(Size::from((1920, 1080)));
        let закрыта = minimap_reveal(g.panel, 0.0);
        let открыта = minimap_reveal(g.panel, 1.0);
        assert_eq!(закрыта.size, Size::from((0, 0)), "закрытая карта показывает полоску");
        assert_eq!(открыта, g.panel);

        assert_eq!(minimap_reveal(g.panel, 1.7), открыта);
        assert_eq!(minimap_reveal(g.panel, -0.4), закрыта);

        let центр = |r: Rectangle<i32, Physical>| {
            (r.loc.x + r.size.w / 2, r.loc.y + r.size.h / 2)
        };
        let mut прошлый = 0;
        for шаг in 1..=10 {
            let доля = шаг as f64 / 10.0;
            let r = minimap_reveal(g.panel, доля);
            assert!(
                r.size.w <= g.panel.size.w && r.size.h <= g.panel.size.h,
                "доля {доля}: карточка вылезла за свои края: {r:?} в {:?}", g.panel,
            );
            assert!(
                (центр(r).0 - центр(g.panel).0).abs() <= 1
                    && (центр(r).1 - центр(g.panel).1).abs() <= 1,
                "доля {доля}: раскрытие увело центр карточки: {r:?}",
            );
            assert!(r.size.h >= прошлый, "доля {доля}: раскрытие пошло назад");
            прошлый = r.size.h;
        }

        // Проявление монотонно и доходит до полной непрозрачности.
        assert!(minimap_fade(0.0) < minimap_fade(0.5));
        assert!(minimap_fade(0.5) < minimap_fade(1.0));
        assert_eq!(minimap_fade(1.0), 1.0);
    }

    /// Карточка предпросмотра держится в экране, её поле — внутри карточки, а
    /// раскрытие идёт от маленькой к полной, ниоткуда не вылезая. По этой же
    /// геометрии считается хит-тест (`Dawn::preview_contains`), поэтому «что
    /// нарисовано, то и кликается» проверяется прямо здесь.
    #[test]
    fn карточка_предпросмотра_держится_в_экране() {
        for экран_w in [1920, 2560, 640] {
            for центр in [0, экран_w / 2, экран_w] {
                let полная = preview_geom(центр, 50, экран_w, 1.0);
                assert!(
                    полная.card.loc.x >= 0
                        && полная.card.loc.x + полная.card.size.w <= экран_w.max(PREVIEW_W),
                    "{экран_w}: карточка вылезла за край: {:?}", полная.card,
                );
                assert!(
                    полная.content.loc.x > полная.card.loc.x
                        && полная.content.loc.y > полная.card.loc.y
                        && полная.content.loc.x + полная.content.size.w
                            < полная.card.loc.x + полная.card.size.w
                        && полная.content.loc.y + полная.content.size.h
                            < полная.card.loc.y + полная.card.size.h,
                    "{экран_w}: поле вылезло из карточки: {:?} в {:?}",
                    полная.content, полная.card,
                );
            }
        }

        // Раскрытие: карточка растёт и опускается на своё место, а на нуле её
        // нет вовсе (доля 0 держит `preview_fade` в нуле).
        let мелкая = preview_geom(1280, 50, 2560, 0.0);
        let полная = preview_geom(1280, 50, 2560, 1.0);
        assert!(мелкая.card.size.w < полная.card.size.w);
        assert!(мелкая.card.size.h < полная.card.size.h);
        assert!(мелкая.card.loc.y < полная.card.loc.y, "карточка не выезжает из-под панели");
        assert_eq!(полная.card.size, Size::from((PREVIEW_W, PREVIEW_H)));
        assert_eq!(полная.card.loc.y, 50);
        assert_eq!(preview_fade(0.0), 0.0);
        assert_eq!(preview_fade(1.0), 1.0);
        // Перелёт кривой не имеет права раздуть карточку заметно больше себя.
        for шаг in 0..=20 {
            let g = preview_geom(1280, 50, 2560, шаг as f64 / 20.0);
            assert!(
                g.card.size.w <= PREVIEW_W + PREVIEW_W / 20,
                "доля {шаг}: карточку раздуло до {:?}", g.card.size,
            );
        }
    }

    /// Карточка предпросмотра — та же мини-копия мира: точка холста, положенная
    /// в карточку, обязана вернуться из неё той же самой. На этом держатся и
    /// хит-тест окна под курсором, и пан карточки, и место живого содержимого.
    #[test]
    fn карточка_предпросмотра_кладёт_и_возвращает_точку() {
        let поле = Size::<i32, Physical>::from((360, 212));
        let anchor = Point::<i32, Physical>::from((120, 64));
        let стол = Rectangle::<f64, Logical>::new(
            Point::from((-400.0, 250.0)), Size::from((2560.0, 1080.0)),
        );
        let центр = Point::from((
            стол.loc.x + стол.size.w / 2.0,
            стол.loc.y + стол.size.h / 2.0,
        ));

        for zoom in [1.0, 2.5, 0.5] {
            let вид = MiniView::fit(стол, поле, zoom, центр);
            // Середина поля — это и есть центр вида.
            let середина = Point::<f64, Physical>::from((
                anchor.x as f64 + поле.w as f64 / 2.0,
                anchor.y as f64 + поле.h as f64 / 2.0,
            ));
            let обратно = вид.canvas_at(anchor, поле, середина);
            assert!(
                (обратно.x - центр.x).abs() < 1.0 && (обратно.y - центр.y).abs() < 1.0,
                "zoom={zoom}: середина карточки даёт {обратно:?}, а вид центрирован на {центр:?}",
            );

            // Живое содержимое ложится ровно на рамку своего окна: повторяем
            // арифметику RescaleRenderElement.
            let окно = Rectangle::<i32, Logical>::new(
                Point::from((-100, 400)), Size::from((800, 600)),
            );
            let рамка = вид.rect(anchor, поле, окно);
            let до = вид.pre_scale(anchor, поле, окно.loc);
            let после = Point::<i32, Physical>::from((
                anchor.x + ((до.x - anchor.x) as f64 * вид.scale).round() as i32,
                anchor.y + ((до.y - anchor.y) as f64 * вид.scale).round() as i32,
            ));
            assert!(
                (после.x - рамка.loc.x).abs() <= 1 && (после.y - рамка.loc.y).abs() <= 1,
                "zoom={zoom}: содержимое разъехалось с рамкой: {после:?} против {:?}", рамка.loc,
            );
        }

        // Кадр стола при зуме 1.0 влезает в поле целиком.
        let вид = MiniView::fit(стол, поле, 1.0, центр);
        let рамка = вид.rect(anchor, поле, Rectangle::new(
            Point::from((стол.loc.x as i32, стол.loc.y as i32)),
            Size::from((стол.size.w as i32, стол.size.h as i32)),
        ));
        assert!(
            рамка.loc.x >= anchor.x && рамка.loc.y >= anchor.y
                && рамка.loc.x + рамка.size.w <= anchor.x + поле.w
                && рамка.loc.y + рамка.size.h <= anchor.y + поле.h,
            "кадр стола не влез в карточку: {рамка:?}",
        );
    }

    /// Живая миниатюра обязана лечь ТОЧНО на прямоугольник своего окна.
    ///
    /// Здесь легко ошибиться: проекция кладёт окно в `(холст−bbox)·scale`
    /// относительно содержимого, а `RescaleRenderElement` умеет только
    /// `остриё + (loc−остриё)·scale`. Совпадают они лишь при остриё = угол
    /// содержимого карты и позиции окна до масштабирования `остриё +
    /// (холст−bbox)`. Ровно это и считает `udev::build_minimap_thumbnails`;
    /// повторяем его арифметику и сверяем с проекцией.
    #[test]
    fn миниатюра_ложится_на_прямоугольник_своего_окна() {
        let окна: [(Point<i32, Logical>, Size<i32, Logical>, bool); 3] = [
            ((0, 0).into(), (1600, 900).into(), true),
            ((3000, 1200).into(), (800, 600).into(), false),
            ((-2400, -700).into(), (1280, 1024).into(), false),
        ];
        let вид = MinimapView { center: Point::from((300.0, 400.0)), zoom: 1.0 };
        let g = minimap_geom(Size::from((2560, 1080)));
        let proj = project_minimap(&окна, вид, Size::from((2560, 1080)), g.content.size);
        let остриё = (g.content.loc.x, g.content.loc.y);

        for ((loc, _, _), b) in окна.iter().zip(proj.boxes.iter()) {
            let до = (
                остриё.0 + loc.x - proj.bbox.loc.x,
                остриё.1 + loc.y - proj.bbox.loc.y,
            );
            let после = (
                остриё.0 + ((до.0 - остриё.0) as f64 * proj.scale).round() as i32,
                остриё.1 + ((до.1 - остриё.1) as f64 * proj.scale).round() as i32,
            );
            assert_eq!(
                после,
                (остриё.0 + b.loc.x, остриё.1 + b.loc.y),
                "миниатюра разъехалась с подложкой окна {loc:?}",
            );
        }
    }

    /// Потолок зума обязан доводить карту до НАСТОЯЩЕГО предпросмотра: на
    /// максимуме масштаб не меньше 1:1, то есть окно рисуется в карте не мельче,
    /// чем на экране, и его содержимое читается. Ровно этого не хватало
    /// панели 320×200 (там на потолке выходило ~0.33).
    #[test]
    fn на_потолке_зума_карта_даёт_один_к_одному() {
        for экран in [
            Size::<i32, Physical>::from((1920, 1080)),
            Size::from((2560, 1080)),
            Size::from((3840, 2160)),
        ] {
            let g = minimap_geom(экран);
            let scale = minimap_scale(MINIMAP_ZOOM_MAX, экран.w, g.content.size.w);
            assert!(
                scale >= 1.0,
                "{экран:?}: на потолке зума масштаб {scale:.3} — предпросмотр не читается",
            );
        }
    }

    /// Автоподгонка обязана ВМЕСТИТЬ то, что ей дали, — по обеим осям.
    ///
    /// Тут легко ошибиться ровно в одном месте: зум считается от ШИРИНЫ
    /// (`видно_w`), а панель шире, чем выше, — значит высокий и узкий кусок
    /// холста влезает по ширине и обрезается по высоте, если не пересчитать
    /// его в эквивалентную ширину. Проверяем обратной проекцией: углы
    /// показанного прямоугольника обязаны лечь внутрь содержимого панели.
    #[test]
    fn автоподгонка_вмещает_кусок_холста_по_обеим_осям() {
        let экран = Size::<i32, Logical>::from((2560, 1080));
        let g = minimap_geom(Size::from((2560, 1080)));
        let содержимое = g.content.size;
        let центр = Point::from((1000.0, -500.0));
        // Широкий, высокий, квадратный, крошечный и огромный.
        for (w, h) in [
            (5000.0, 1200.0),
            (900.0, 6000.0),
            (3000.0, 3000.0),
            (10.0, 10.0),
            (400_000.0, 400_000.0),
        ] {
            let зум = minimap_auto_zoom(Size::from((w, h)), экран, содержимое);
            let proj = project_minimap(
                &[], MinimapView { center: центр, zoom: зум }, экран, содержимое,
            );
            // Показанная область холста обязана накрыть запрошенную — если
            // только зум не упёрся в свой предел (очень большой кусок).
            let влезло = proj.bbox.size.w as f64 >= w && proj.bbox.size.h as f64 >= h;
            assert!(
                влезло || зум == MINIMAP_ZOOM_MIN,
                "{w}×{h} не влезло: показано {:?} при зуме {зум}", proj.bbox.size,
            );
            assert!(proj.scale.is_finite() && proj.scale > 0.0);
        }

        // Больше кусок — мельче зум, и наоборот. Монотонность важнее точных
        // чисел: на ней держится ощущение «карта подстраивается сама».
        let мелкий = minimap_auto_zoom(Size::from((2000.0, 1000.0)), экран, содержимое);
        let крупный = minimap_auto_zoom(Size::from((8000.0, 4000.0)), экран, содержимое);
        assert!(крупный < мелкий, "{крупный} должен быть мельче {мелкий}");

        // Вырожденный кусок (окон нет, экран нулевой) не даёт NaN.
        let ноль = minimap_auto_zoom(
            Size::from((0.0, 0.0)), Size::from((0, 0)), Size::from((0, 0)),
        );
        assert!(ноль.is_finite() && ноль > 0.0);
    }

    /// Миникарта обязана показывать окрестность ЦЕНТРА ВИДА, а не bbox всех
    /// окон, и слушаться своего зума. Проверяем через обратный пересчёт: точка
    /// в середине панели — это и есть центр вида.
    #[test]
    fn миникарта_смотрит_вокруг_центра_вида() {
        let экран = Size::<i32, Logical>::from((2560, 1080));
        let g = minimap_geom(Size::from((2560, 1080)));
        let содержимое = g.content.size;
        let центр = Point::from((4321.0, -987.0));
        let середина = Point::<f64, Physical>::from((
            содержимое.w as f64 / 2.0,
            содержимое.h as f64 / 2.0,
        ));

        for zoom in [0.5, 1.0, 2.5] {
            let proj = project_minimap(
                &[], MinimapView { center: центр, zoom }, экран, содержимое,
            );
            let обратно = minimap_click_to_canvas(середина, proj.bbox, proj.scale);
            assert!(
                (обратно.x - центр.x).abs() < 2.0 / proj.scale.max(1e-9) + 2.0
                    && (обратно.y - центр.y).abs() < 2.0 / proj.scale.max(1e-9) + 2.0,
                "zoom={zoom}: середина панели даёт {обратно:?}, а вид центрирован на {центр:?}",
            );
        }

        // Больше зум — крупнее масштаб и уже показанная область.
        let мелко = project_minimap(
            &[], MinimapView { center: центр, zoom: 0.5 }, экран, содержимое,
        );
        let крупно = project_minimap(
            &[], MinimapView { center: центр, zoom: 3.0 }, экран, содержимое,
        );
        assert!(крупно.scale > мелко.scale);
        assert!(крупно.bbox.size.w < мелко.bbox.size.w);

        // Зум зажат: колесо не должно уносить карту в вырождение.
        let за_краем = project_minimap(
            &[], MinimapView { center: центр, zoom: 1e6 }, экран, содержимое,
        );
        let потолок = project_minimap(
            &[], MinimapView { center: центр, zoom: MINIMAP_ZOOM_MAX }, экран, содержимое,
        );
        assert_eq!(за_краем.scale, потолок.scale);
        assert!(за_краем.scale.is_finite() && за_краем.scale > 0.0);

        // Экран нулевой ширины (выход ещё не поднялся) не должен давать
        // деления на ноль и NaN в масштабе.
        let без_экрана = project_minimap(
            &[], MinimapView { center: центр, zoom: 1.0 }, Size::from((0, 0)), содержимое,
        );
        assert!(без_экрана.scale.is_finite() && без_экрана.scale > 0.0);
    }
}
