use std::time::{Duration, Instant};

use smithay::utils::{Logical, Point};

use crate::canvas::zoom_anchor_camera;
use crate::state::Dawn;
use crate::tiling::Layout;

#[inline]
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Ease-out cubic: fast start, gentle settle.
#[inline]
pub fn ease_out_cubic(t: f64) -> f64 {
    let inv = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inv * inv * inv
}

/// Ease-out-back: fast start, slight overshoot past the target, gentle settle
/// back — a "spring"/Hyprland-ish feel. Used only for window tiling
/// assembly/scatter (see CameraAnim::new_spring), NOT for camera pan/zoom
/// (overshoot on the camera itself would be disorienting).
#[inline]
pub fn ease_out_back(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    const C1: f64 = 1.70158;
    const C3: f64 = C1 + 1.0;
    let inv = t - 1.0;
    1.0 + C3 * inv * inv * inv + C1 * inv * inv
}

/// Camera-only fly-to (no zoom change). Used by focus snapping, bookmarks, minimap clicks.
pub struct CameraAnim {
    pub from: Point<f64, Logical>,
    pub to: Point<f64, Logical>,
    pub start: Instant,
    pub duration: Duration,
    spring: bool,
}

impl CameraAnim {
    pub fn new(from: Point<f64, Logical>, to: Point<f64, Logical>, duration: Duration) -> Self {
        Self { from, to, start: Instant::now(), duration, spring: false }
    }

    /// Same as `new`, but eases with a slight spring overshoot — used for
    /// window position LERPs (tiling assembly/reflow, float scatter) to feel
    /// more like Hyprland's default bezier instead of a flat settle.
    pub fn new_spring(from: Point<f64, Logical>, to: Point<f64, Logical>, duration: Duration) -> Self {
        Self { from, to, start: Instant::now(), duration, spring: true }
    }

    /// Eased progress in [0, 1] (can exceed 1 briefly if spring overshoots).
    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        let raw = elapsed / dur;
        if self.spring { ease_out_back(raw) } else { ease_out_cubic(raw) }
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    pub fn current(&self) -> Point<f64, Logical> {
        let t = self.t();
        Point::from((lerp(self.from.x, self.to.x, t), lerp(self.from.y, self.to.y, t)))
    }
}

/// Позиционная анимация окна — критически задемпфированная пружина.
///
/// Почему не ease-out-cubic с фиксированной длительностью (как было): цель у
/// окна меняется НА ЛЕТУ и часто (arrange() на каждый свап при драге, толчок
/// соседей на каждый кадр коллизии). Анимация с фиксированной длительностью
/// умеет только перезапуститься с нуля: скорость мгновенно падает до
/// «начальной для нового ease», прогресс обнуляется — окно ползёт
/// асимптотически, дёргается и до цели фактически не доезжает, пока цель
/// шевелится. Пружина же просто меняет цель, сохраняя текущую скорость:
/// перенацеливание непрерывно по скорости, а значит незаметно глазу — ровно
/// то, что даёт «hyprland-овское» ощущение.
///
/// Интегрируется точным решением (не Эйлером), поэтому стабильна при любом dt
/// и при любой частоте вызова tick (VBlank + 60Гц-таймер зовут её вперемешку).
pub struct PosAnim {
    /// Текущая позиция top-left окна в canvas-координатах.
    pub pos: Point<f64, Logical>,
    /// Текущая скорость, px/сек — то, что переживает смену цели.
    pub vel: Point<f64, Logical>,
    /// Куда едем. Публично: коллизия/свап считают геометрию по ЦЕЛИ, а не по
    /// текущему кадру полёта (иначе решение зависит от фазы анимации).
    pub target: Point<f64, Logical>,
    /// Частота пружины (1/сек): больше — резче. См. omega_for.
    omega: f64,
    last: Instant,
    done: bool,
}

/// Порог «приехали» по позиции (px) и по скорости (px/сек).
const POS_EPS: f64 = 0.4;
const POS_VEL_EPS: f64 = 8.0;

/// ω, при котором критическая пружина проходит ~99% пути за `dur`
/// (для критического демпфирования остаток ~ (1+ωt)·e^{-ωt}, при ωt=7 это ~0.7%).
fn omega_for(dur: Duration) -> f64 {
    7.0 / dur.as_secs_f64().max(0.016)
}

/// Один шаг критически задемпфированной пружины, точное решение
/// x'' = -ω²x - 2ωx' : x(t) = (x₀ + (v₀ + ωx₀)t)·e^{-ωt}.
fn spring_step(x: f64, v: f64, omega: f64, dt: f64) -> (f64, f64) {
    let e = (-omega * dt).exp();
    let c = v + omega * x;
    ((x + c * dt) * e, (v - omega * c * dt) * e)
}

impl PosAnim {
    pub fn new(from: Point<f64, Logical>, target: Point<f64, Logical>, dur: Duration) -> Self {
        Self {
            pos: from,
            vel: Point::from((0.0, 0.0)),
            target,
            omega: omega_for(dur),
            last: Instant::now(),
            done: false,
        }
    }

    /// Пружина с заданной стартовой скоростью и явной ω — для инерционного
    /// доезда после броска окна мышью. Если цель выбрана как `from + vel/ω`,
    /// то v₀ + ω·x₀ = 0 и решение вырождается в чистое e^{-ωt}: скорость на
    /// старте В ТОЧНОСТИ равна скорости курсора (никакого рывка в момент
    /// отпускания) и нет перелёта за цель.
    pub fn with_velocity(
        from: Point<f64, Logical>,
        target: Point<f64, Logical>,
        vel: Point<f64, Logical>,
        omega: f64,
    ) -> Self {
        Self { pos: from, vel, target, omega, last: Instant::now(), done: false }
    }

    /// Сменить цель, сохранив скорость. Та же цель (в пределах полпикселя) —
    /// no-op: пересоздавать анимацию каждый кадр незачем.
    pub fn retarget(&mut self, target: Point<f64, Logical>, dur: Duration) {
        if (self.target.x - target.x).abs() < 0.5 && (self.target.y - target.y).abs() < 0.5 {
            return;
        }
        self.target = target;
        self.omega = omega_for(dur);
        self.done = false;
    }

    /// Продвинуть на реальный прошедший dt. dt зажат сверху: после паузы
    /// (VT-switch, залипший кадр) окно должно просто оказаться у цели, а не
    /// пролететь её из-за одного гигантского шага.
    pub fn advance(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64().min(0.1);
        self.last = now;
        if self.done || dt <= 0.0 {
            return;
        }
        let (dx, vx) = spring_step(self.pos.x - self.target.x, self.vel.x, self.omega, dt);
        let (dy, vy) = spring_step(self.pos.y - self.target.y, self.vel.y, self.omega, dt);
        if dx.abs() < POS_EPS && dy.abs() < POS_EPS && vx.abs() < POS_VEL_EPS && vy.abs() < POS_VEL_EPS {
            self.pos = self.target;
            self.vel = Point::from((0.0, 0.0));
            self.done = true;
        } else {
            self.pos = Point::from((self.target.x + dx, self.target.y + dy));
            self.vel = Point::from((vx, vy));
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

/// Zoom fly-to that keeps a canvas anchor point pinned to a screen point
/// throughout the animation (e.g. hold-to-zoom keeps the screen center fixed).
pub struct ZoomAnim {
    pub anchor_canvas: Point<f64, Logical>,
    pub anchor_screen: Point<f64, Logical>,
    pub from_zoom: f64,
    pub to_zoom: f64,
    pub start: Instant,
    pub duration: Duration,
}

impl ZoomAnim {
    pub fn new(
        anchor_canvas: Point<f64, Logical>,
        anchor_screen: Point<f64, Logical>,
        from_zoom: f64,
        to_zoom: f64,
        duration: Duration,
    ) -> Self {
        Self { anchor_canvas, anchor_screen, from_zoom, to_zoom, start: Instant::now(), duration }
    }

    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        ease_out_cubic(elapsed / dur)
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// Returns (zoom, camera) for the current instant.
    pub fn current(&self) -> (f64, Point<f64, Logical>) {
        let t = self.t();
        let zoom = lerp(self.from_zoom, self.to_zoom, t);
        let cam = zoom_anchor_camera(self.anchor_canvas, self.anchor_screen, zoom);
        (zoom, cam)
    }
}

/// A rectangle glow (Focus Aura, minimap highlight, ...) fly-to between two
/// canvas-space rectangles (position + size interpolated together).
pub struct RectAnim {
    pub from_pos: Point<f64, Logical>,
    pub from_size: (f64, f64),
    pub to_pos: Point<f64, Logical>,
    pub to_size: (f64, f64),
    pub start: Instant,
    pub duration: Duration,
}

impl RectAnim {
    pub fn new(
        from_pos: Point<f64, Logical>,
        from_size: (f64, f64),
        to_pos: Point<f64, Logical>,
        to_size: (f64, f64),
        duration: Duration,
    ) -> Self {
        Self { from_pos, from_size, to_pos, to_size, start: Instant::now(), duration }
    }

    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        ease_out_cubic(elapsed / dur)
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    pub fn current(&self) -> (Point<f64, Logical>, (f64, f64)) {
        let t = self.t();
        let pos = Point::from((lerp(self.from_pos.x, self.to_pos.x, t), lerp(self.from_pos.y, self.to_pos.y, t)));
        let size = (lerp(self.from_size.0, self.to_size.0, t), lerp(self.from_size.1, self.to_size.1, t));
        (pos, size)
    }
}

/// Открытие окна "с ростом" (Hyprland-style): размер растёт от 40% до 100%
/// целевого, центр остаётся фиксированным. Растим размер настоящими xdg
/// configure (не шейдер) — безопасно для рендер-пайплайна, чуть более
/// "ступенчато" на медленных клиентах, зато не трогает отрисовку контента.
pub struct OpenAnim {
    pub center: Point<f64, Logical>,
    pub target_size: (f64, f64),
    pub start: Instant,
    pub duration: Duration,
}

const OPEN_ANIM_MIN_SCALE: f64 = 0.4;

impl OpenAnim {
    pub fn new(center: Point<f64, Logical>, target_size: (f64, f64), duration: Duration) -> Self {
        Self { center, target_size, start: Instant::now(), duration }
    }

    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        ease_out_cubic(elapsed / dur)
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// (позиция top-left, (w, h)) для текущего момента.
    pub fn current(&self) -> (Point<i32, Logical>, (i32, i32)) {
        let scale = OPEN_ANIM_MIN_SCALE + (1.0 - OPEN_ANIM_MIN_SCALE) * self.t();
        let w = (self.target_size.0 * scale).round().max(1.0) as i32;
        let h = (self.target_size.1 * scale).round().max(1.0) as i32;
        let loc = Point::from((
            (self.center.x - w as f64 / 2.0).round() as i32,
            (self.center.y - h as f64 / 2.0).round() as i32,
        ));
        (loc, (w, h))
    }
}

/// Запасной шаг, если тик зовут первый раз (реального dt ещё нет).
const FRAME_DT: Duration = Duration::from_millis(16);
/// Потолок шага: после паузы (VT-switch, зависший кадр) инерция не должна
/// «телепортировать» камеру одним гигантским dt.
const MAX_DT: Duration = Duration::from_millis(100);

/// Advances momentum coasting and any active camera/zoom LERP by one frame.
/// Called from the ~60Hz timer in main.rs. No-ops (cheaply) when nothing is
/// animating, so it's safe to run unconditionally forever.
pub fn tick(state: &mut Dawn) {
    let mut dirty = false;

    // Реальный dt, а не фиксированные 16мс: тик зовут и 60Гц-таймер из main.rs,
    // и VBlank-хендлер перед кадром (чтобы анимация сэмплировалась ровно в
    // момент отрисовки). С захардкоженным шагом инерция от этого поехала бы
    // вдвое быстрее — она единственная здесь считается по шагу, а не по Instant.
    let now = Instant::now();
    let dt = state.anim_last_tick
        .map(|prev| now.saturating_duration_since(prev).min(MAX_DT))
        .unwrap_or(FRAME_DT);
    state.anim_last_tick = Some(now);

    // Инерция панорамирования применяется ТОЛЬКО когда нет активной анимации
    // камеры/зума. Иначе после конца camera_anim (например перелёта в (0,0) при
    // Win+D→tiling) остаточный momentum снова утаскивал камеру вбок — камера
    // "не доезжала"/уплывала. Пока идёт camera_anim/zoom_anim, momentum молчит.
    // Инерция ТОЛЬКО во Float: pan существует только там, а в tiling/columns
    // поздний momentum.launch() (напр. финальный кадр тачпад-жеста, пришедший
    // уже ПОСЛЕ Win+D) иначе "доезжал" и утаскивал разложенные окна за кадр
    // ("окна уезжают после Win+D").
    if state.tile_config.layout == Layout::Float
        && state.camera_anim.is_none() && state.zoom_anim.is_none()
    {
        if let Some(delta) = state.momentum.tick(dt) {
            state.viewport.cam_x += delta.x;
            state.viewport.cam_y += delta.y;
            // Курсор здесь НЕ трогаем: его тянет за камерой общая
            // синхронизация в конце итерации цикла (Dawn::sync_pointer_to_camera).
            // Раньше сдвиг делался прямо тут — экранная позиция сохранялась, но
            // pointer.motion никто не слал, и под курсором оставалась старая
            // поверхность.
            dirty = true;
        }
    }

    if let Some(anim) = &state.camera_anim {
        let pos = anim.current();
        state.viewport.cam_x = pos.x;
        state.viewport.cam_y = pos.y;
        dirty = true;
        if anim.is_done() {
            state.camera_anim = None;
        }
    }

    if let Some(anim) = &state.zoom_anim {
        let (zoom, cam) = anim.current();
        state.viewport.zoom = zoom;
        state.viewport.cam_x = cam.x;
        state.viewport.cam_y = cam.y;
        dirty = true;
        if anim.is_done() {
            state.zoom_anim = None;
        }
    }

    // Появление окна "с ростом" (Hyprland-style, только Float — см. new_toplevel).
    if !state.window_open_anims.is_empty() {
        let mut finished = Vec::new();
        for (i, (window, anim)) in state.window_open_anims.iter().enumerate() {
            let (loc, (w, h)) = anim.current();
            crate::xwin::set_size(window, Some((w, h).into()), crate::xwin::Tiled::Keep);
            crate::xwin::configure(window);
            state.space.map_element(window.clone(), loc, false);
            if let Some(tw) = state.tagged_windows.iter_mut().find(|tw| {
                &tw.window == window
            }) {
                tw.position = loc;
                tw.float_position = loc;
            }
            if anim.is_done() {
                finished.push(i);
            }
        }
        for i in finished.into_iter().rev() {
            state.window_open_anims.remove(i);
        }
        dirty = true;
    }

    // Разлёт/сборка окон при переходах tiling/floating: плавный LERP позиции
    // вместо мгновенного map_element (см. tiling.rs::scatter_to_float/resize_window).
    if !state.window_pos_anims.is_empty() {
        // Общий `now` на весь кадр: иначе окна одной раскладки интегрируются
        // по чуть разным dt и «расползаются» на доли пикселя.
        for (window, anim) in state.window_pos_anims.iter_mut() {
            anim.advance(now);
            state.space.map_element(window.clone(), anim.pos.to_i32_round(), false);
        }
        state.window_pos_anims.retain(|(_, anim)| !anim.is_done());
        // ВАЖНО: не сбрасывать needs_plane_reset здесь каждый тик — это
        // форсирует полный редрав экрана (reset_buffer_ages) на КАЖДЫЙ кадр
        // анимации вместо инкрементального damage tracking и жёстко лагает
        // (60 полных редравов/сек). Damage tracking сам корректно перерисует
        // только сдвинувшуюся область; plane reset нужен один раз при
        // структурных изменениях (см. arrange()/toggle_fold_stack), не за кадр.
        dirty = true;
    }

    // Focus Aura (5.3): следим за текущим фокусом, при смене — плавный LERP
    // от текущего положения ауры к прямоугольнику нового окна.
    {
        let target = state.focused_surface()
            .and_then(|fs| state.tagged_windows.iter().find(|tw| {
                crate::xwin::is_surface(&tw.window, &fs)
            }))
            .and_then(|tw| state.space.element_geometry(&tw.window))
            .map(|g| (
                Point::from((g.loc.x as f64, g.loc.y as f64)),
                (g.size.w as f64, g.size.h as f64),
            ));

        if let Some((pos, size)) = target {
            let changed = match state.focus_aura_target {
                Some((p, s)) => {
                    (p.x - pos.x).abs() > 0.5 || (p.y - pos.y).abs() > 0.5
                        || (s.0 - size.0).abs() > 0.5 || (s.1 - size.1).abs() > 0.5
                }
                None => true,
            };
            if changed {
                let (from_pos, from_size) = state.focus_aura_current.unwrap_or((pos, size));
                state.focus_aura_anim = Some(RectAnim::new(from_pos, from_size, pos, size, Duration::from_millis(180)));
                state.focus_aura_target = Some((pos, size));
            }
        } else {
            // Фокус ушёл на невидимое окно (например, unmapped после переключения
            // тега — см. state.rs::refresh_tags) — без этого focus_aura_current
            // навсегда оставался на старой позиции и рисовался как "тень"
            // предыдущего выделения поверх нового тега (см. udev.rs::render_surface).
            if state.focus_aura_current.is_some() {
                dirty = true;
            }
            state.focus_aura_target = None;
            state.focus_aura_current = None;
            state.focus_aura_anim = None;
        }

        if let Some(anim) = &state.focus_aura_anim {
            let (p, s) = anim.current();
            state.focus_aura_current = Some((p, s));
            dirty = true;
            if anim.is_done() {
                state.focus_aura_anim = None;
            }
        }
    }

    if dirty {
        state.apply_camera();
        state.request_redraw();
    }
}
