use crate::Dawn;
use crate::tiling::Layout;
use smithay::{
    desktop::{Space, Window},
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{compositor, shell::xdg::SurfaceCachedState},
};
use std::cell::RefCell;

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;
        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();
        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

pub struct ResizeSurfaceGrab {
    pub start_data: PointerGrabStartData<Dawn>,
    pub window: Window,
    pub edges: ResizeEdge,
    pub initial_rect: Rectangle<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
    /// Остальные окна из "созвездия" этого окна и их геометрия на момент
    /// начала ресайза — на отпускании масштабируются вокруг общего опорного
    /// угла на тот же коэффициент, что и перетаскиваемое окно (см. button()).
    group_initial: Vec<(Window, Rectangle<i32, Logical>)>,
    /// Начальный mfact при старте ресайза (Tile mode) — чтобы delta.x
    /// правильно менял master-фактор относительно исходного значения.
    initial_mfact: f32,
}

impl ResizeSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<Dawn>,
        window: Window,
        edges: ResizeEdge,
        initial_window_rect: Rectangle<i32, Logical>,
        group_initial: Vec<(Window, Rectangle<i32, Logical>)>,
        initial_mfact: f32,
    ) -> Self {
        ResizeSurfaceState::with(window.toplevel().unwrap().wl_surface(), |state| {
            *state = ResizeSurfaceState::Resizing {
                edges,
                initial_rect: initial_window_rect,
                last_rect: initial_window_rect,
            };
        });
        Self {
            start_data,
            window,
            edges,
            initial_rect: initial_window_rect,
            last_window_size: initial_window_rect.size,
            group_initial,
            initial_mfact,
        }
    }
}

impl PointerGrab<Dawn> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);

        // Проверяем — окно в тайлинге?
        let is_tiled = data.tile_config.layout != Layout::Float
            && data.tagged_windows.iter().any(|tw| {
                !tw.floating
                    && tw.window.toplevel().zip(self.window.toplevel())
                        .map(|(a, b)| a.wl_surface() == b.wl_surface())
                        .unwrap_or(false)
            });

        if is_tiled {
            let delta = event.location - self.start_data.location;
            if data.tile_config.layout == Layout::Tile {
                // Tile: горизонтальный drag меняет mfact (master-фактор).
                let out_w = data.space.outputs().next()
                    .and_then(|o| data.space.output_geometry(o))
                    .map(|g| g.size.w as f64)
                    .unwrap_or(1920.0);
                let delta_mfact = (delta.x / out_w) as f32;
                data.tile_config.mfact = (self.initial_mfact + delta_mfact).clamp(0.1, 0.9);
                data.arrange();
                data.request_redraw();
            } else if data.tile_config.layout == Layout::Columns {
                // Columns: горизонтальный drag меняет ширину активной колонки
                // непрерывно (drag_width от 15% до 100% экрана).
                data.columns_resize_active_width(delta.x);
            }
            // Monocle: не поддерживается.
            return;
        }

        // Floating: обычный resize
        let mut delta = event.location - self.start_data.location;
        let mut new_w = self.initial_rect.size.w;
        let mut new_h = self.initial_rect.size.h;

        if self.edges.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
            if self.edges.intersects(ResizeEdge::LEFT) { delta.x = -delta.x; }
            new_w = (self.initial_rect.size.w as f64 + delta.x) as i32;
        }
        if self.edges.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
            if self.edges.intersects(ResizeEdge::TOP) { delta.y = -delta.y; }
            new_h = (self.initial_rect.size.h as f64 + delta.y) as i32;
        }

        let (min_size, max_size) =
            compositor::with_states(self.window.toplevel().unwrap().wl_surface(), |states| {
                let mut guard = states.cached_state.get::<SurfaceCachedState>();
                let data = guard.current();
                (data.min_size, data.max_size)
            });

        let min_w = min_size.w.max(1);
        let min_h = min_size.h.max(1);
        let max_w = if max_size.w == 0 { i32::MAX } else { max_size.w };
        let max_h = if max_size.h == 0 { i32::MAX } else { max_size.h };

        self.last_window_size = Size::from((
            new_w.max(min_w).min(max_w),
            new_h.max(min_h).min(max_h),
        ));

        let xdg = self.window.toplevel().unwrap();
        xdg.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Resizing);
            state.size = Some(self.last_window_size);
        });
        xdg.send_pending_configure();

        // "Созвездие": остальные окна группы масштабируются ЖИВО вместе с
        // перетаскиваемым (раньше это делалось только на отпускании — соседи
        // лишь перестраивались, но не меняли размер во время драга).
        if !self.group_initial.is_empty() {
            let scale_x = self.last_window_size.w as f64 / self.initial_rect.size.w.max(1) as f64;
            let scale_y = self.last_window_size.h as f64 / self.initial_rect.size.h.max(1) as f64;
            let anchor_x = if self.edges.intersects(ResizeEdge::LEFT) {
                self.initial_rect.loc.x + self.initial_rect.size.w
            } else {
                self.initial_rect.loc.x
            };
            let anchor_y = if self.edges.intersects(ResizeEdge::TOP) {
                self.initial_rect.loc.y + self.initial_rect.size.h
            } else {
                self.initial_rect.loc.y
            };
            for (member, geo) in self.group_initial.clone() {
                let new_w = ((geo.size.w as f64) * scale_x).round().max(50.0) as i32;
                let new_h = ((geo.size.h as f64) * scale_y).round().max(50.0) as i32;
                let new_x = (anchor_x as f64 + (geo.loc.x - anchor_x) as f64 * scale_x).round() as i32;
                let new_y = (anchor_y as f64 + (geo.loc.y - anchor_y) as f64 * scale_y).round() as i32;
                if let Some(t) = member.toplevel() {
                    t.with_pending_state(|s| { s.size = Some((new_w, new_h).into()); });
                    t.send_pending_configure();
                }
                let new_loc: Point<i32, Logical> = (new_x, new_y).into();
                data.space.map_element(member.clone(), new_loc, false);
            }
            data.request_redraw();
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
        const BTN_RIGHT: u32 = 0x111;
        if !handle.current_pressed().contains(&BTN_RIGHT) {
            handle.unset_grab(self, data, event.serial, event.time, true);

            // Только для floating — финализируем resize.
            // В tiling/columns: сбрасываем ResizeSurfaceState (commit-хендлер
            // не должен корректировать позицию — ей управляет layout).
            let is_tiled = data.tile_config.layout != Layout::Float
                && data.tagged_windows.iter().any(|tw| {
                    !tw.floating
                        && tw.window.toplevel().zip(self.window.toplevel())
                            .map(|(a, b)| a.wl_surface() == b.wl_surface())
                            .unwrap_or(false)
                });

            if is_tiled {
                if let Some(t) = self.window.toplevel() {
                    ResizeSurfaceState::with(t.wl_surface(), |state| {
                        *state = ResizeSurfaceState::Idle;
                    });
                }
            } else {
                let xdg = self.window.toplevel().unwrap();
                xdg.with_pending_state(|state| {
                    state.states.unset(xdg_toplevel::State::Resizing);
                    state.size = Some(self.last_window_size);
                });
                xdg.send_pending_configure();

                // Сохраняем float_size
                if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                    tw.window.toplevel().zip(self.window.toplevel())
                        .map(|(a, b)| a.wl_surface() == b.wl_surface())
                        .unwrap_or(false)
                }) {
                    tw.float_size = Some(self.last_window_size);
                }

                ResizeSurfaceState::with(xdg.wl_surface(), |state| {
                    *state = ResizeSurfaceState::WaitingForLastCommit {
                        edges: self.edges,
                        initial_rect: self.initial_rect,
                    };
                });

                // "Созвездие" (Super+G): остальные окна группы масштабируются
                // вокруг общего неподвижного угла на тот же коэффициент.
                if !self.group_initial.is_empty() {
                    let scale_x = self.last_window_size.w as f64 / self.initial_rect.size.w.max(1) as f64;
                    let scale_y = self.last_window_size.h as f64 / self.initial_rect.size.h.max(1) as f64;
                    let anchor_x = if self.edges.intersects(ResizeEdge::LEFT) {
                        self.initial_rect.loc.x + self.initial_rect.size.w
                    } else {
                        self.initial_rect.loc.x
                    };
                    let anchor_y = if self.edges.intersects(ResizeEdge::TOP) {
                        self.initial_rect.loc.y + self.initial_rect.size.h
                    } else {
                        self.initial_rect.loc.y
                    };
                    for (member, geo) in self.group_initial.clone() {
                        let new_w = ((geo.size.w as f64) * scale_x).round().max(50.0) as i32;
                        let new_h = ((geo.size.h as f64) * scale_y).round().max(50.0) as i32;
                        let new_x = (anchor_x as f64 + (geo.loc.x - anchor_x) as f64 * scale_x).round() as i32;
                        let new_y = (anchor_y as f64 + (geo.loc.y - anchor_y) as f64 * scale_y).round() as i32;
                        if let Some(t) = member.toplevel() {
                            t.with_pending_state(|s| { s.size = Some((new_w, new_h).into()); });
                            t.send_pending_configure();
                        }
                        data.animate_window_to_dur(&member, (new_x, new_y).into(), std::time::Duration::from_millis(180));
                        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                            tw.window.toplevel().zip(member.toplevel())
                                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                                .unwrap_or(false)
                        }) {
                            tw.float_size = Some((new_w, new_h).into());
                            tw.float_position = (new_x, new_y).into();
                        }
                    }
                    data.request_plane_reset();
                }
            }
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

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum ResizeSurfaceState {
    #[default]
    Idle,
    Resizing {
        edges: ResizeEdge,
        initial_rect: Rectangle<i32, Logical>,
        /// Прямоугольник на прошлом коммите этого же resize-жеста — от него
        /// считается инкрементальная дельта для эластичного расталкивания (2.3).
        last_rect: Rectangle<i32, Logical>,
    },
    WaitingForLastCommit {
        edges: ResizeEdge,
        initial_rect: Rectangle<i32, Logical>,
    },
}

impl ResizeSurfaceState {
    fn with<F, T>(surface: &WlSurface, cb: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<Self>::default);
            let state = states.data_map.get::<RefCell<Self>>().unwrap();
            cb(&mut state.borrow_mut())
        })
    }

    fn commit(&mut self) -> Option<(ResizeEdge, Rectangle<i32, Logical>)> {
        match *self {
            Self::Resizing { edges, initial_rect, .. } => Some((edges, initial_rect)),
            Self::WaitingForLastCommit { edges, initial_rect } => {
                *self = Self::Idle;
                Some((edges, initial_rect))
            }
            Self::Idle => None,
        }
    }

    /// Только для активного (не финального) resize-коммита: возвращает
    /// прямоугольник с прошлого коммита и запоминает `new_rect` как новый
    /// "прошлый" — на его основе (2.3) считается инкрементальный push соседей.
    fn take_last_rect_for_elastic(
        &mut self,
        new_rect: Rectangle<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        match self {
            Self::Resizing { last_rect, .. } => {
                let old = *last_rect;
                *last_rect = new_rect;
                Some(old)
            }
            _ => None,
        }
    }
}

/// Эластичное расталкивание соседей (2.3): окна, чей край примыкал (в
/// пределах ADJACENCY_TOLERANCE) к соответствующему краю `old`, сдвигаются на
/// ту же дельту, на которую этот край сместился в `new`. При уменьшении окна
/// дельта отрицательная — соседи стягиваются обратно.
const ADJACENCY_TOLERANCE: i32 = 20;

fn elastic_displace(
    space: &mut Space<Window>,
    resized: &Window,
    old: Rectangle<i32, Logical>,
    new: Rectangle<i32, Logical>,
) {
    let old_right = old.loc.x + old.size.w;
    let old_bottom = old.loc.y + old.size.h;
    let new_right = new.loc.x + new.size.w;
    let new_bottom = new.loc.y + new.size.h;

    let delta_left = new.loc.x - old.loc.x;
    let delta_top = new.loc.y - old.loc.y;
    let delta_right = new_right - old_right;
    let delta_bottom = new_bottom - old_bottom;

    if delta_left == 0 && delta_top == 0 && delta_right == 0 && delta_bottom == 0 {
        return;
    }

    let others: Vec<(Window, Rectangle<i32, Logical>)> = space
        .elements()
        .filter(|w| {
            w.toplevel()
                .zip(resized.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true)
        })
        .filter_map(|w| space.element_geometry(w).map(|g| (w.clone(), g)))
        .collect();

    for (other, geo) in others {
        let mut dx = 0;
        let mut dy = 0;

        let other_right = geo.loc.x + geo.size.w;
        let other_bottom = geo.loc.y + geo.size.h;
        let overlaps_y = geo.loc.y < old_bottom && other_bottom > old.loc.y;
        let overlaps_x = geo.loc.x < old_right && other_right > old.loc.x;

        if delta_right != 0 && overlaps_y && (geo.loc.x - old_right).abs() <= ADJACENCY_TOLERANCE {
            dx += delta_right;
        }
        if delta_left != 0 && overlaps_y && (old.loc.x - other_right).abs() <= ADJACENCY_TOLERANCE {
            dx += delta_left;
        }
        if delta_bottom != 0 && overlaps_x && (geo.loc.y - old_bottom).abs() <= ADJACENCY_TOLERANCE {
            dy += delta_bottom;
        }
        if delta_top != 0 && overlaps_x && (old.loc.y - other_bottom).abs() <= ADJACENCY_TOLERANCE {
            dy += delta_top;
        }

        if dx != 0 || dy != 0 {
            let new_other_loc = Point::from((geo.loc.x + dx, geo.loc.y + dy));
            space.map_element(other, new_other_loc, false);
        }
    }
}

pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface) -> Option<()> {
    let window = space
        .elements()
        .find(|w| w.toplevel().unwrap().wl_surface() == surface)
        .cloned()?;

    let mut window_loc = space.element_location(&window)?;
    let geometry = window.geometry();

    let new_loc: Point<Option<i32>, Logical> = ResizeSurfaceState::with(surface, |state| {
        state.commit().and_then(|(edges, initial_rect)| {
            edges.intersects(ResizeEdge::TOP_LEFT).then(|| {
                let new_x = edges.intersects(ResizeEdge::LEFT)
                    .then_some(initial_rect.loc.x + (initial_rect.size.w - geometry.size.w));
                let new_y = edges.intersects(ResizeEdge::TOP)
                    .then_some(initial_rect.loc.y + (initial_rect.size.h - geometry.size.h));
                (new_x, new_y).into()
            })
        }).unwrap_or_default()
    });

    if let Some(new_x) = new_loc.x { window_loc.x = new_x; }
    if let Some(new_y) = new_loc.y { window_loc.y = new_y; }

    if new_loc.x.is_some() || new_loc.y.is_some() {
        space.map_element(window.clone(), window_loc, false);
    }

    // Эластичное расталкивание соседей (2.3) — только на промежуточных
    // коммитах активного resize, не на финальном после отпускания кнопки.
    let new_rect = Rectangle::new(window_loc, geometry.size);
    let old_rect = ResizeSurfaceState::with(surface, |state| state.take_last_rect_for_elastic(new_rect));
    if let Some(old_rect) = old_rect {
        elastic_displace(space, &window, old_rect, new_rect);
    }

    Some(())
}
