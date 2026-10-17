//! Win+ЛКМ+ПКМ → поворот окна в плоскости экрана (крен), как фотографию на
//! столе: окно крутится вокруг своей середины вслед за курсором.
//!
//! Отдельного жеста «нажать обе сразу» нет: человек жмёт Super+ЛКМ (перенос)
//! или Super+ПКМ (ресайз), и вторая кнопка, пришедшая в тот хват, закрывает
//! его и просит этот (`Parallax::поворот_ждёт`, ставит input.rs после
//! `pointer.button`). Угол считается по приращениям — разница углов курсора
//! вокруг центра окна между соседними движениями, — поэтому окно не прыгает
//! в момент хвата и спокойно крутится больше чем на пол-оборота. С Shift угол
//! защёлкивается на 15°. Отпустил любую из двух кнопок — поворот кончился.

use crate::Parallax;
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

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// Ближе к середине окна угол курсора не считаем: там он скачет на каждом
/// пикселе, и окно дёргалось бы на десятки градусов от дрожи руки.
const МЁРТВЫЙ_РАДИУС: f64 = 12.0;

/// Шаг защёлки с Shift, градусы.
const ЗАЩЁЛКА_ГРАДУСОВ: f32 = 15.0;

pub struct TiltGrab {
    start_data: PointerGrabStartData<Parallax>,
    window: Window,
    /// Крен окна в момент хвата (цель, а не показ: если окно ещё
    /// доворачивалось колесом, крутим от того, куда оно ехало).
    крен_с: f32,
    /// Накопленный поворот курсора вокруг центра с начала хвата.
    накоплено: f32,
    /// Угол курсора на прошлом движении; `None` — курсор в мёртвой зоне.
    прошлый_угол: Option<f32>,
}

impl TiltGrab {
    pub fn new(data: &Parallax, start_data: PointerGrabStartData<Parallax>, window: Window) -> Self {
        let прошлый_угол = угол_курсора(data, &window, start_data.location);
        Self {
            крен_с: data.цель_наклона(&window).крен,
            start_data,
            window,
            накоплено: 0.0,
            прошлый_угол,
        }
    }

    fn повернуть_к(&mut self, data: &mut Parallax, pos: Point<f64, Logical>) {
        let Some(угол) = угол_курсора(data, &self.window, pos) else {
            self.прошлый_угол = None;
            return;
        };
        if let Some(прошлый) = self.прошлый_угол {
            self.накоплено += crate::наклон::в_круг(угол - прошлый);
        }
        self.прошлый_угол = Some(угол);
        let mut крен = self.крен_с + self.накоплено;
        let shift = data.seat.get_keyboard().is_some_and(|k| k.modifier_state().shift);
        if shift {
            let шаг = ЗАЩЁЛКА_ГРАДУСОВ.to_radians();
            крен = (крен / шаг).round() * шаг;
        }
        data.задать_крен(&self.window, крен);
    }
}

/// Угол курсора вокруг середины окна (радианы, по часовой — плюс, как у крена).
fn угол_курсора(data: &Parallax, window: &Window, pos: Point<f64, Logical>) -> Option<f32> {
    let g = data.space.element_geometry(window)?;
    let центр = (g.loc.x as f64 + g.size.w as f64 * 0.5, g.loc.y as f64 + g.size.h as f64 * 0.5);
    let (dx, dy) = (pos.x - центр.0, pos.y - центр.1);
    let zoom = data.viewport.zoom.max(0.01);
    if (dx * dx + dy * dy).sqrt() * zoom < МЁРТВЫЙ_РАДИУС {
        return None;
    }
    Some(dy.atan2(dx) as f32)
}

impl PointerGrab<Parallax> for TiltGrab {
    fn motion(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.повернуть_к(data, event.location);
    }

    fn relative_motion(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        event: &ButtonEvent,
    ) {
        // Клиенту кнопки поворота не нужны: обе нажаты ради компоновщика.
        let нажаты = handle.current_pressed();
        if !нажаты.contains(&BTN_LEFT) || !нажаты.contains(&BTN_RIGHT) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            tracing::debug!("plx/tilt: drag rotation finished");
        }
    }

    fn axis(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, details: AxisFrame) {
        handle.axis(data, details)
    }
    fn frame(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>) {
        handle.frame(data);
    }
    fn gesture_swipe_begin(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureSwipeBeginEvent) {
        handle.gesture_swipe_begin(data, event)
    }
    fn gesture_swipe_update(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureSwipeUpdateEvent) {
        handle.gesture_swipe_update(data, event)
    }
    fn gesture_swipe_end(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureSwipeEndEvent) {
        handle.gesture_swipe_end(data, event)
    }
    fn gesture_pinch_begin(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GesturePinchBeginEvent) {
        handle.gesture_pinch_begin(data, event)
    }
    fn gesture_pinch_update(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GesturePinchUpdateEvent) {
        handle.gesture_pinch_update(data, event)
    }
    fn gesture_pinch_end(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GesturePinchEndEvent) {
        handle.gesture_pinch_end(data, event)
    }
    fn gesture_hold_begin(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureHoldBeginEvent) {
        handle.gesture_hold_begin(data, event)
    }
    fn gesture_hold_end(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureHoldEndEvent) {
        handle.gesture_hold_end(data, event)
    }
    fn start_data(&self) -> &PointerGrabStartData<Parallax> {
        &self.start_data
    }
    fn unset(&mut self, data: &mut Parallax) {
        data.request_redraw();
    }
}
