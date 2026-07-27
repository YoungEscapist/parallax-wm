use smithay::{
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

use crate::Dawn;

pub struct PanGrab {
    pub start_data: PointerGrabStartData<Dawn>,
    pub last_canvas_pos: Point<f64, Logical>,
}

impl PointerGrab<Dawn> for PanGrab {
    fn motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        // event.location = ptr_canvas (PointerMotion уже добавил +delta_canvas к ptr).
        // Нам нужно: cursor_screen = const, canvas движется в направлении drag.
        //
        // Для canvas в направлении drag: cam -= delta_canvas
        //   screen_window = (W - cam_new) = (W - cam + delta) → окна движутся вправо ✓
        //
        // Для cursor_screen = const при cam -= delta_canvas:
        //   (ptr_new - cam_new)*zoom = (ptr_old - cam_old)*zoom
        //   ptr_new = ptr_old + (cam_new - cam_old) = ptr_old - delta_canvas
        //   Но PointerMotion уже поставил ptr_new = ptr_old + delta_canvas
        //   → нужна коррекция ptr -= 2*delta_canvas
        let delta_x = event.location.x - self.last_canvas_pos.x;
        let delta_y = event.location.y - self.last_canvas_pos.y;

        // Холст движется в направлении drag
        data.viewport.cam_x -= delta_x;
        data.viewport.cam_y -= delta_y;

        // Курсор фиксирован на экране (чистая коррекция: -2 * canvas_delta)
        data.pointer_location.x -= 2.0 * delta_x;
        data.pointer_location.y -= 2.0 * delta_y;

        data.apply_camera();
        data.request_redraw();

        // last_canvas_pos = скорректированная позиция (ptr после коррекции)
        self.last_canvas_pos = Point::from((
            data.pointer_location.x,
            data.pointer_location.y,
        ));

        // Синхронизируем smithay со скорректированной позицией
        let corrected = MotionEvent {
            location: data.pointer_location,
            serial: event.serial,
            time: event.time,
        };
        handle.motion(data, None, &corrected);
    }

    fn button(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn relative_motion(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, f: Option<(WlSurface, Point<f64, Logical>)>, e: &RelativeMotionEvent) { h.relative_motion(d, f, e); }
    fn axis(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, det: AxisFrame) { h.axis(d, det) }
    fn frame(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>) { h.frame(d); }
    fn gesture_swipe_begin(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GestureSwipeBeginEvent) { h.gesture_swipe_begin(d, e) }
    fn gesture_swipe_update(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GestureSwipeUpdateEvent) { h.gesture_swipe_update(d, e) }
    fn gesture_swipe_end(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GestureSwipeEndEvent) { h.gesture_swipe_end(d, e) }
    fn gesture_pinch_begin(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GesturePinchBeginEvent) { h.gesture_pinch_begin(d, e) }
    fn gesture_pinch_update(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GesturePinchUpdateEvent) { h.gesture_pinch_update(d, e) }
    fn gesture_pinch_end(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GesturePinchEndEvent) { h.gesture_pinch_end(d, e) }
    fn gesture_hold_begin(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GestureHoldBeginEvent) { h.gesture_hold_begin(d, e) }
    fn gesture_hold_end(&mut self, d: &mut Dawn, h: &mut PointerInnerHandle<'_, Dawn>, e: &GestureHoldEndEvent) { h.gesture_hold_end(d, e) }
    fn start_data(&self) -> &PointerGrabStartData<Dawn> { &self.start_data }
    fn unset(&mut self, _data: &mut Dawn) {}
}
