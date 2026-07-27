use smithay::{
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

use crate::Dawn;

/// Rubber-band мультивыделение: ЛКМ, зажатая на пустом холсте (без окна под
/// курсором), протягивает прямоугольник — на отпускании выделяются все окна,
/// пересекающиеся с ним (см. Dawn::select_windows_in_rect).
pub struct SelectGrab {
    pub start_data: PointerGrabStartData<Dawn>,
    pub start_pos: Point<f64, Logical>,
}

fn rect_from_points(a: Point<f64, Logical>, b: Point<f64, Logical>) -> Rectangle<i32, Logical> {
    let x0 = a.x.min(b.x) as i32;
    let y0 = a.y.min(b.y) as i32;
    let x1 = a.x.max(b.x) as i32;
    let y1 = a.y.max(b.y) as i32;
    Rectangle::new((x0, y0).into(), ((x1 - x0).max(0), (y1 - y0).max(0)).into())
}

impl PointerGrab<Dawn> for SelectGrab {
    fn motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        data.selection_drag = Some(rect_from_points(self.start_pos, event.location));
        data.request_redraw();
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
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
            let rect = data.selection_drag.take();
            data.select_windows_in_rect(rect);
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
