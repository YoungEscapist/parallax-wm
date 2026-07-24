use crate::Dawn;
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
    utils::{Logical, Point},
};

const SNAP_DISTANCE: i32 = 12;

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<Dawn>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
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
            // Floating: свободное перемещение со snap
            let delta = event.location - self.start_data.location;
            let mut new_loc = self.initial_window_location.to_f64() + delta;

            // Infinite canvas: свободное перемещение без ограничений

            data.space.map_element(self.window.clone(), new_loc.to_i32_round(), true);
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
