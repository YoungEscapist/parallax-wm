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
    pub last_screen_pos: Point<f64, Logical>,
}

impl PointerGrab<Dawn> for PanGrab {
    fn motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        let zoom = data.viewport.zoom;
        
        // Позиция курсора на экране сейчас (исходя из текущей камеры)
        let current_screen_pos = Point::<f64, Logical>::from((
            (event.location.x - data.viewport.cam_x) * zoom,
            (event.location.y - data.viewport.cam_y) * zoom,
        ));

        // На сколько пикселей сдвинулась мышь по коврику
        let delta_screen = current_screen_pos - self.last_screen_pos;

        // Двигаем камеру в обратную сторону (панорамирование)
        // CameraDelta = -DeltaScreen / Zoom
        data.viewport.cam_x -= delta_screen.x / zoom;
        data.viewport.cam_y -= delta_screen.y / zoom;
        
        data.apply_camera();

        // Обновляем экранный якорь
        self.last_screen_pos = current_screen_pos;

        // Посылаем "пустое" движение, чтобы Smithay обновил внутреннее состояние, 
        // но не передаем его окнам (focus=None)
        handle.motion(data, None, event);
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
