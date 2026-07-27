use crate::Dawn;
use crate::canvas::VelocityTracker;
use crate::tiling::Layout;
use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle},
};
use std::time::Duration;

const PUSH_ANIM_DURATION: Duration = Duration::from_millis(120);

/// Порог примагничивания (2.1): при отпускании кнопки край окна подравнивается
/// к соседнему, если оказался в пределах этой дистанции.
const SNAP_DISTANCE: i32 = 20;

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<Dawn>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    /// (2.5) уже вырвано из горизонтальной ленты в этом драге — сшивание
    /// соседей делается один раз, при первом пересечении порога по Y
    torn_from_ribbon: bool,
    /// Остальные окна из "созвездия" (см. selection.rs) этого окна и их
    /// позиции на момент начала драга — двигаются той же дельтой, что и
    /// перетаскиваемое окно, чтобы группа перемещалась как единое целое.
    group_initial: Vec<(Window, Point<i32, Logical>)>,
    /// Трекер скорости курсора во время драга — на отпускании даёт инерцию
    /// (окно доезжает по инерции, см. button()).
    velocity: VelocityTracker,
    /// Последняя позиция окна во время драга (для проекции инерции).
    last_loc: Point<i32, Logical>,
}

impl MoveSurfaceGrab {
    pub fn new(
        start_data: PointerGrabStartData<Dawn>,
        window: Window,
        initial_window_location: Point<i32, Logical>,
        group_initial: Vec<(Window, Point<i32, Logical>)>,
    ) -> Self {
        Self {
            start_data,
            window,
            initial_window_location,
            torn_from_ribbon: false,
            group_initial,
            velocity: VelocityTracker::new(),
            last_loc: initial_window_location,
        }
    }
}

/// Ширина Y-полосы, в пределах которой окна считаются "одной горизонтальной
/// лентой" (2.5).
const RIBBON_ROW_TOLERANCE: i32 = 20;

/// Когда окно вырывается из своей ленты (вертикальный вылет > высоты окна),
/// все окна ленты правее вытащенного подтягиваются влево на его ширину —
/// лента "сшивается" без образования дыры.
fn stitch_ribbon_gap(
    data: &mut Dawn,
    dragged: &Window,
    dragged_initial_loc: Point<i32, Logical>,
    dragged_width: i32,
) {
    let row_y = dragged_initial_loc.y;
    let to_shift: Vec<(Window, Point<i32, Logical>)> = data.space.elements()
        .filter(|w| {
            w.toplevel().zip(dragged.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true)
        })
        .filter_map(|w| data.space.element_geometry(w).map(|g| (w.clone(), g)))
        .filter(|(_, g)| {
            (g.loc.y - row_y).abs() <= RIBBON_ROW_TOLERANCE && g.loc.x > dragged_initial_loc.x
        })
        .map(|(w, g)| (w, Point::from((g.loc.x - dragged_width, g.loc.y))))
        .collect();

    if to_shift.is_empty() {
        return;
    }

    for (w, new_loc) in &to_shift {
        data.space.map_element(w.clone(), *new_loc, false);
        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
            tw.window.toplevel().zip(w.toplevel())
                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                .unwrap_or(false)
        }) {
            tw.float_position = *new_loc;
            tw.position = *new_loc;
        }
    }
    data.request_plane_reset();
    tracing::info!("dawn: ribbon stitched ({} windows shifted)", to_shift.len());
}

/// Ищет ближайший "магнитный" край среди остальных окон для каждой оси
/// независимо (X и Y примагничиваются отдельно). Возвращает Some только по
/// тем осям, где нашлось окно в пределах SNAP_DISTANCE.
fn find_snap_target(
    data: &Dawn,
    window: &Window,
    free_loc: Point<i32, Logical>,
) -> Option<Point<i32, Logical>> {
    let size = data.space.element_geometry(window)?.size;
    let (w, h) = (size.w, size.h);

    let mut best_x: Option<(i32, i32)> = None;
    let mut best_y: Option<(i32, i32)> = None;

    for other in data.space.elements() {
        let is_self = other.toplevel().zip(window.toplevel())
            .map(|(a, b)| a.wl_surface() == b.wl_surface())
            .unwrap_or(false);
        if is_self { continue; }
        let og = match data.space.element_geometry(other) { Some(g) => g, None => continue };

        for cand in [
            og.loc.x + og.size.w,       // наш left → их right
            og.loc.x - w,               // наш right → их left
            og.loc.x,                   // наш left → их left
            og.loc.x + og.size.w - w,   // наш right → их right
        ] {
            let dist = (cand - free_loc.x).abs();
            if dist <= SNAP_DISTANCE && best_x.is_none_or(|(_, d)| dist < d) {
                best_x = Some((cand, dist));
            }
        }
        for cand in [
            og.loc.y + og.size.h,
            og.loc.y - h,
            og.loc.y,
            og.loc.y + og.size.h - h,
        ] {
            let dist = (cand - free_loc.y).abs();
            if dist <= SNAP_DISTANCE && best_y.is_none_or(|(_, d)| dist < d) {
                best_y = Some((cand, dist));
            }
        }
    }

    if best_x.is_none() && best_y.is_none() {
        return None;
    }
    let x = best_x.map(|(c, _)| c).unwrap_or(free_loc.x);
    let y = best_y.map(|(c, _)| c).unwrap_or(free_loc.y);
    Some(Point::from((x, y)))
}

/// Толкает окна, в которые "врезалось" перетаскиваемое (коллизия, пока кнопка
/// зажата — магнитирование отдельно, на отпускании). Каждый пересекающийся
/// сосед сдвигается по оси наименьшего перекрытия на величину перекрытия —
/// стандартная эвристика minimum-translation-vector, ощущается как толчок.
fn push_colliding_windows(data: &mut Dawn, dragged: &Window, dragged_loc: Point<i32, Logical>) {
    let size = match data.space.element_geometry(dragged) { Some(g) => g.size, None => return };
    let dragged_rect = Rectangle::new(dragged_loc, size);
    let dcx = dragged_loc.x + size.w / 2;
    let dcy = dragged_loc.y + size.h / 2;

    let others: Vec<(Window, Rectangle<i32, Logical>)> = data.space.elements()
        .filter(|w| {
            w.toplevel().zip(dragged.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true)
        })
        .filter_map(|w| data.space.element_geometry(w).map(|g| (w.clone(), g)))
        .collect();

    for (other, geo) in others {
        let overlap = match dragged_rect.intersection(geo) { Some(o) => o, None => continue };
        if overlap.size.w <= 0 || overlap.size.h <= 0 { continue; }

        let ocx = geo.loc.x + geo.size.w / 2;
        let ocy = geo.loc.y + geo.size.h / 2;

        let (push_x, push_y) = if overlap.size.w < overlap.size.h {
            (if ocx >= dcx { overlap.size.w } else { -overlap.size.w }, 0)
        } else {
            (0, if ocy >= dcy { overlap.size.h } else { -overlap.size.h })
        };

        let new_loc = Point::from((geo.loc.x + push_x, geo.loc.y + push_y));
        // Плавный LERP вместо телепорта — ощущается как инерция толчка
        // (переанимируется на каждый кадр, пока коллизия продолжается).
        data.animate_window_to_dur(&other, new_loc, PUSH_ANIM_DURATION);
        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
            tw.window.toplevel().zip(other.toplevel())
                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                .unwrap_or(false)
        }) {
            tw.float_position = new_loc;
            tw.position = new_loc;
        }
    }
}

impl PointerGrab<Dawn> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        let is_tiled = data.tile_config.layout != Layout::Float
            && data.tagged_windows.iter().any(|tw| {
                !tw.floating
                    && tw.window.toplevel().zip(self.window.toplevel())
                        .map(|(a, b)| a.wl_surface() == b.wl_surface())
                        .unwrap_or(false)
            });

        if is_tiled {
            // Ищем окно под курсором (не то которое тащим)
            let cursor = event.location;
            let target = data.space
                .element_under(cursor)
                .and_then(|(w, _)| {
                    // не само себя
                    let is_self = w.toplevel().zip(self.window.toplevel())
                        .map(|(a, b)| a.wl_surface() == b.wl_surface())
                        .unwrap_or(false);
                    if is_self { None } else { Some(w.clone()) }
                });

            if let Some(target_window) = target {
                // Находим индексы обоих окон в tagged_windows
                let current_tags = data.viewport.current_tags();

                let self_idx = data.tagged_windows.iter().position(|tw| {
                    tw.tags & current_tags != 0
                        && tw.window.toplevel().zip(self.window.toplevel())
                            .map(|(a, b)| a.wl_surface() == b.wl_surface())
                            .unwrap_or(false)
                });

                let target_idx = data.tagged_windows.iter().position(|tw| {
                    tw.tags & current_tags != 0
                        && tw.window.toplevel().zip(target_window.toplevel())
                            .map(|(a, b)| a.wl_surface() == b.wl_surface())
                            .unwrap_or(false)
                });

                if let (Some(si), Some(ti)) = (self_idx, target_idx) {
                    if si != ti {
                        // Swap окон в списке
                        data.tagged_windows.swap(si, ti);
                        // Пересчитываем тайлинг
                        data.arrange();
                        tracing::debug!("dawn: swap {} ↔ {}", si, ti);
                    }
                }
            }
        } else {
            // Floating: свободное перемещение. Окно всегда следует за мышью
            // 1:1 (никакого залипания в процессе) — коллизия (2.1, Super+S)
            // толкает соседей, магнитирование применяется один раз при
            // отпускании кнопки (см. button()), не во время перетаскивания.
            let delta = event.location - self.start_data.location;
            let mut free_loc = (self.initial_window_location.to_f64() + delta).to_i32_round();

            // Разрыв ленты (2.5): вертикальный вылет за высоту окна — тащим
            // окно из его горизонтальной ленты, соседи справа сшиваются один раз.
            if !self.torn_from_ribbon {
                if let Some(win_size) = data.space.element_geometry(&self.window).map(|g| g.size) {
                    let win_size: smithay::utils::Size<i32, Logical> = win_size;
                    let dy: i32 = free_loc.y - self.initial_window_location.y;
                    if dy.abs() > win_size.h {
                        self.torn_from_ribbon = true;
                        stitch_ribbon_gap(data, &self.window, self.initial_window_location, win_size.w);
                    }
                }
            }

            // Трекаем скорость окна (в canvas-пикселях) для инерции на отпускании.
            let step = Point::from((
                (free_loc.x - self.last_loc.x) as f64,
                (free_loc.y - self.last_loc.y) as f64,
            ));
            self.velocity.push(event.time, step);
            self.last_loc = free_loc;

            // ── Overview mode: clamp to workspace + live tag switch ──
            let mut should_move = true;
            if data.overview_active {
                if let Some(mask) = data.overview_workspace_at(event.location) {
                    // Сменить тег окна, если курсор на другом столе
                    if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                        tw.window.toplevel().zip(self.window.toplevel())
                            .map(|(a, b)| a.wl_surface() == b.wl_surface())
                            .unwrap_or(false)
                    }) {
                        if tw.tags != mask {
                            tw.tags = mask;
                        }
                    }
                    // Зажать окно в границы стола под курсором
                    if let Some((bw, bh)) = data.overview_band_size() {
                        let margin = crate::tiling::GAP_OUTER;
                        let slot = *data.overview_slots.get(&mask).unwrap_or(&(0, 0));
                        let band_gap = 140i32;
                        let stride_x = bw + band_gap;
                        let stride_y = bh + band_gap;
                        let brect_loc_x = slot.0 * stride_x;
                        let brect_loc_y = slot.1 * stride_y;
                        if let Some(size) = data.space.element_geometry(&self.window).map(|g| g.size) {
                            free_loc.x = free_loc.x.clamp(
                                brect_loc_x + margin,
                                brect_loc_x + bw - size.w - margin,
                            );
                            free_loc.y = free_loc.y.clamp(
                                brect_loc_y + margin,
                                brect_loc_y + bh - size.h - margin,
                            );
                        }
                    }
                } else {
                    // Курсор вне всех столов — не обновляем позицию окна
                    should_move = false;
                }
            }

            if should_move {
                data.space.map_element(self.window.clone(), free_loc, true);
                if data.is_snapping_enabled {
                    push_colliding_windows(data, &self.window, free_loc);
                }
                // Сохраняем float-позицию и помечаем как вручную размещённое
                if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                    tw.window.toplevel().zip(self.window.toplevel())
                        .map(|(a, b)| a.wl_surface() == b.wl_surface())
                        .unwrap_or(false)
                }) {
                    tw.float_position = free_loc;
                    tw.float_position_set = true;
                }

                // "Созвездие" (Super+G): остальные окна группы едут той же дельтой,
                // что и перетаскиваемое — группа двигается как единое целое.
                if !self.group_initial.is_empty() {
                    for (member, init_loc) in &self.group_initial {
                        let member_loc = (init_loc.to_f64() + delta).to_i32_round();
                        data.space.map_element(member.clone(), member_loc, false);
                        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                            tw.window.toplevel().zip(member.toplevel())
                                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                                .unwrap_or(false)
                        }) {
                            tw.float_position = member_loc;
                            tw.position = member_loc;
                            tw.float_position_set = true;
                        }
                    }
                }
            }
        }
    }

    fn relative_motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        const BTN_LEFT: u32 = 0x110;
        if !handle.current_pressed().contains(&BTN_LEFT) {
            handle.unset_grab(self, data, event.serial, event.time, true);

            // В обзоре столов: отпустили перетаскивание → окно переезжает на
            // воркспейс того бэнда, куда попало (вверх/вниз меняет стол).
            if data.overview_active {
                data.overview_reassign(&self.window);
                handle.frame(data);
                return;
            }

            // Магнитирование (2.1): один раз при отпускании подравниваем край
            // к ближайшему соседу в пределах SNAP_DISTANCE, если коллизия включена.
            let mut snapped_applied = false;
            if data.is_snapping_enabled {
                if let Some(loc) = data.space.element_geometry(&self.window).map(|g| g.loc) {
                    if let Some(snapped) = find_snap_target(data, &self.window, loc) {
                        data.space.map_element(self.window.clone(), snapped, true);
                        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                            tw.window.toplevel().zip(self.window.toplevel())
                                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                                .unwrap_or(false)
                        }) {
                            tw.float_position = snapped;
                        }
                        snapped_applied = true;
                    }
                }
            }

            // Инерция перетаскивания: окно доезжает по инерции после отпускания
            // (не применяется, если сработало магнитирование — там нужна
            // точная посадка на край соседа).
            let is_tiled_win = data.tile_config.layout != Layout::Float
                && data.tagged_windows.iter().any(|tw| {
                    !tw.floating
                        && tw.window.toplevel().zip(self.window.toplevel())
                            .map(|(a, b)| a.wl_surface() == b.wl_surface())
                            .unwrap_or(false)
                });
            if !snapped_applied && !is_tiled_win {
                let v = self.velocity.launch_velocity(); // px/сек в canvas
                let speed = (v.x * v.x + v.y * v.y).sqrt();
                const MIN_FLING: f64 = 120.0;  // ниже — считаем что окно просто положили
                const GLIDE_SECS: f64 = 0.16;  // сколько "проекции" скорости пролетит окно
                if speed > MIN_FLING {
                    let target = Point::from((
                        self.last_loc.x + (v.x * GLIDE_SECS).round() as i32,
                        self.last_loc.y + (v.y * GLIDE_SECS).round() as i32,
                    ));
                    // Длительность масштабируем со скоростью (быстрее бросок —
                    // дольше катится), но с потолком.
                    let dur_ms = (200.0 + speed * 0.12).min(500.0) as u64;
                    data.animate_window_to_dur(&self.window, target, std::time::Duration::from_millis(dur_ms));
                    if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                        tw.window.toplevel().zip(self.window.toplevel())
                            .map(|(a, b)| a.wl_surface() == b.wl_surface())
                            .unwrap_or(false)
                    }) {
                        tw.float_position = target;
                        tw.position = target;
                        tw.float_position_set = true;
                    }
                }
            }

            // Сброс plane-кэша на границе драга — сглаживает возможный
            // "хвост"/тень от предыдущей позиции на некоторых DRM-планах.
            data.request_plane_reset();
            data.request_redraw();
        }
    }

    fn axis(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, details: AxisFrame) {
        handle.axis(data, details)
    }
    fn frame(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>) {
        handle.frame(data);
    }
    fn gesture_swipe_begin(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureSwipeBeginEvent) {
        handle.gesture_swipe_begin(data, event)
    }
    fn gesture_swipe_update(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureSwipeUpdateEvent) {
        handle.gesture_swipe_update(data, event)
    }
    fn gesture_swipe_end(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureSwipeEndEvent) {
        handle.gesture_swipe_end(data, event)
    }
    fn gesture_pinch_begin(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GesturePinchBeginEvent) {
        handle.gesture_pinch_begin(data, event)
    }
    fn gesture_pinch_update(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GesturePinchUpdateEvent) {
        handle.gesture_pinch_update(data, event)
    }
    fn gesture_pinch_end(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GesturePinchEndEvent) {
        handle.gesture_pinch_end(data, event)
    }
    fn gesture_hold_begin(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureHoldBeginEvent) {
        handle.gesture_hold_begin(data, event)
    }
    fn gesture_hold_end(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureHoldEndEvent) {
        handle.gesture_hold_end(data, event)
    }
    fn start_data(&self) -> &PointerGrabStartData<Dawn> {
        &self.start_data
    }
    fn unset(&mut self, _data: &mut Dawn) {}
}
