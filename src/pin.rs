//! Закреплённые окна: приклеены к камере, не зумятся и не панятся.
//!
//! Просьба Ярика 30.09.2026 — «закрепление окна, чтобы оно оставалось
//! приклеенным к камере». По смыслу это картинка-в-картинке: окно висит на
//! одном и том же месте ЭКРАНА, что бы ни делали с холстом под ним.
//!
//! **Как устроено.** Окно остаётся обычным окном — в `space`, в
//! `tagged_windows`, с фокусом клавиатуры и всем прочим, — но его место на
//! холсте перестаёт что-либо значить. Правду о нём знает
//! `TaggedWindow::закреплено`: левый верхний угол видимой части в экранных
//! пикселях своего монитора.
//!
//!  · рисуется отдельным проходом, поверх окон холста и под интерфейсом, в
//!    масштабе 1:1 (`udev::build_pinned_elements`), а из обычного цикла окон
//!    выброшено — иначе на холсте осталась бы его зумящаяся копия;
//!  · ввод ловит `Parallax::закреплённое_под` в экранных координатах тем же
//!    приёмом «виртуального начала», что и layer-поверхности
//!    (`state::layer_surface_under`): smithay сам вычтет начало из точки
//!    холста и получит правильную точку внутри окна при любом зуме;
//!  · Super+ЛКМ и Super+ПКМ по нему возят и тянут окно в экранных пикселях
//!    (`PinMoveGrab`, `PinResizeGrab`), а не в пикселях холста: на зуме 0.5
//!    обычный хват двигал бы его вдвое быстрее руки;
//!  · смена стола его не прячет: закреплённое окно переезжает на стол,
//!    открытый на его мониторе (`Parallax::закреплённые_на_стол`).
//!
//! Открепление кладёт окно на холст ровно туда, где его видно сейчас.

use smithay::{
    desktop::{Window, WindowSurfaceType},
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::state::Parallax;

/// Где висит закреплённое окно.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Закрепление {
    /// Левый верхний угол ВИДИМОЙ части окна (без полей CSD) в экранных
    /// пикселях монитора.
    pub экран: Point<f64, Logical>,
    /// Номер монитора (`Parallax::мониторы`), на экране которого окно висит.
    pub монитор: usize,
}

/// Меньше этого окно ресайзом не ужимается: за пару пикселей его уже не
/// схватить обратно.
const МИН_СТОРОНА: i32 = 80;

impl Parallax {
    /// Закреплено ли окно.
    pub fn закреплено(&self, window: &Window) -> bool {
        self.tagged_windows
            .iter()
            .any(|tw| &tw.window == window && tw.закреплено.is_some())
    }

    fn закрепление(&self, window: &Window) -> Option<Закрепление> {
        self.tagged_windows
            .iter()
            .find(|tw| &tw.window == window)
            .and_then(|tw| tw.закреплено)
    }

    /// Экранная точка курсора (или любой точки холста) на текущей камере.
    fn на_экран(&self, pos: Point<f64, Logical>) -> Point<f64, Logical> {
        let zoom = self.viewport.zoom.max(0.01);
        Point::from((
            (pos.x - self.viewport.cam_x) * zoom,
            (pos.y - self.viewport.cam_y) * zoom,
        ))
    }

    /// Закреплённые окна монитора `монитор` сверху вниз по стопке (первое —
    /// верхнее) с их видимой рамкой на экране.
    pub fn закреплённые(&self, монитор: usize) -> Vec<(Window, Rectangle<f64, Logical>)> {
        self.space
            .elements()
            .rev()
            .filter_map(|w| {
                let з = self.закрепление(w)?;
                if з.монитор != монитор {
                    return None;
                }
                let g = self.space.element_geometry(w)?;
                let видимо = crate::xwin::видимый_размер(w, g.size);
                Some((w.clone(), Rectangle::new(з.экран, видимо.to_f64())))
            })
            .collect()
    }

    /// Закреплённое окно под точкой холста `pos` и «виртуальное начало» его
    /// дерева поверхностей: `pos - начало` — точка внутри дерева в его
    /// собственных координатах, при любом зуме (см. заголовок модуля).
    pub fn закреплённое_под(&self, pos: Point<f64, Logical>) -> Option<(Window, Point<f64, Logical>)> {
        let экран = self.на_экран(pos);
        let монитор = self.активный;
        for (w, рамка) in self.закреплённые(монитор) {
            let корень = рамка.loc - w.geometry().loc.to_f64();
            let внутри = экран - корень;
            // Попап (меню у края) может вылезать за рамку окна, и клик по
            // нему обязан дойти до меню, а не провалиться на холст.
            let попал = рамка.contains(экран)
                || w.surface_under(внутри, WindowSurfaceType::POPUP).is_some();
            if попал {
                return Some((w, pos - внутри));
            }
        }
        None
    }

    /// `surface_under` для закреплённых окон: поверхность и её начало с тем
    /// же контрактом, что у обычной ветки (`pos - начало` = точка в
    /// поверхности).
    pub fn закреплённая_поверхность_под(
        &self,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let (w, начало) = self.закреплённое_под(pos)?;
        let внутри = pos - начало;
        let (s, сдвиг) = w.surface_under(внутри, WindowSurfaceType::ALL)?;
        Some((s, начало + сдвиг.to_f64()))
    }

    /// Super+Y: закрепить сфокусированное окно на экране или открепить.
    pub fn toggle_pin_focused(&mut self) {
        let Some(window) = self.focused_window() else {
            tracing::info!("plx/pin: no focused window");
            return;
        };
        self.toggle_pin(&window);
    }

    pub fn toggle_pin(&mut self, window: &Window) {
        if self.закреплено(window) {
            self.открепить(window);
        } else {
            self.закрепить(window);
        }
    }

    fn закрепить(&mut self, window: &Window) {
        if self.виджет_ли(window) || self.is_fullscreen(window) {
            tracing::info!("plx/pin: widgets and fullscreen windows are not pinned");
            return;
        }
        let Some(g) = self.space.element_geometry(window) else { return };
        // Место берём там, где окно остановится, а не кадр полёта.
        let loc = self.window_anim_target(window).unwrap_or(g.loc);
        let mut экран = self.на_экран(loc.to_f64());
        // Окно, закреплённое наполовину за краем, нельзя было бы схватить:
        // двигаем его внутрь экрана целиком (насколько влезает).
        let s = self.screen_size();
        экран.x = экран.x.clamp(0.0, (s.w - g.size.w).max(0) as f64);
        экран.y = экран.y.clamp(0.0, (s.h - g.size.h).max(0) as f64);
        self.freeze_window_anim(window);
        let монитор = self.активный;
        let tiled_before = self
            .tagged_windows
            .iter()
            .any(|tw| &tw.window == window && !tw.floating);
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) {
            tw.закреплено = Some(Закрепление { экран, монитор });
            // Раскладкам закреплённое окно не принадлежит: плавает и
            // намеренно (сборка по Win+D его не забирает).
            tw.floating = true;
            tw.float_pinned = true;
            tw.float_size = Some(g.size);
        }
        // Выделение и группы — операции холста; закреплённое в них не живёт.
        self.selected_windows.retain(|w| w != window);
        if tiled_before {
            self.arrange();
        }
        self.space.raise_element(window, true);
        tracing::info!("plx/pin: pinned at screen ({:.0},{:.0}) on monitor {}", экран.x, экран.y, монитор);
        self.request_plane_reset();
        self.request_redraw();
    }

    fn открепить(&mut self, window: &Window) {
        let Some(з) = self.закрепление(window) else { return };
        // Кладём на холст ровно туда, где окно сейчас видно.
        let zoom = self.viewport.zoom.max(0.01);
        let loc: Point<i32, Logical> = (
            (self.viewport.cam_x + з.экран.x / zoom).round() as i32,
            (self.viewport.cam_y + з.экран.y / zoom).round() as i32,
        )
            .into();
        self.space.map_element(window.clone(), loc, true);
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) {
            tw.закреплено = None;
            tw.position = loc;
            tw.float_position = loc;
            tw.float_position_set = true;
        }
        tracing::info!("plx/pin: unpinned to canvas ({},{})", loc.x, loc.y);
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Переставить закреплённое окно в новую точку экрана (зажатую так, чтобы
    /// за окно оставалось за что схватиться).
    pub fn закреплённое_в(&mut self, window: &Window, экран: Point<f64, Logical>) {
        let s = self.screen_size();
        let размер = self
            .space
            .element_geometry(window)
            .map(|g| g.size)
            .unwrap_or_else(|| Size::from((МИН_СТОРОНА, МИН_СТОРОНА)));
        // Хоть 40 пикселей окна обязаны оставаться на экране.
        const ЗАПАС: f64 = 40.0;
        let x = экран.x.clamp(ЗАПАС - размер.w as f64, s.w as f64 - ЗАПАС);
        let y = экран.y.clamp(0.0, s.h as f64 - ЗАПАС);
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) {
            if let Some(з) = tw.закреплено.as_mut() {
                з.экран = Point::from((x, y));
            }
        }
        self.request_redraw();
    }

    /// Закреплённые окна переезжают на стол, который открыт на их мониторе:
    /// «приклеено к камере» значит видно на любом столе. Зовётся из
    /// `refresh_tags` до раскладки окон по столам.
    pub fn закреплённые_на_стол(&mut self) {
        let монитор = self.активный;
        let стол = self.viewport.current_tags();
        if стол == 0 {
            return;
        }
        for tw in self.tagged_windows.iter_mut() {
            if tw.закреплено.is_some_and(|з| з.монитор == монитор) {
                tw.tags = стол;
            }
        }
    }

    /// Начать перенос закреплённого окна (Super+ЛКМ).
    pub fn начать_перенос_закреплённого(
        &mut self,
        window: Window,
        start: PointerGrabStartData<Parallax>,
    ) -> Option<PinMoveGrab> {
        let з = self.закрепление(&window)?;
        let курсор = self.на_экран(start.location);
        Some(PinMoveGrab { start_data: start, window, курсор, окно: з.экран })
    }

    /// Начать ресайз закреплённого окна (Super+ПКМ): тянется ближний к
    /// курсору угол.
    pub fn начать_ресайз_закреплённого(
        &mut self,
        window: Window,
        start: PointerGrabStartData<Parallax>,
    ) -> Option<PinResizeGrab> {
        let з = self.закрепление(&window)?;
        let g = self.space.element_geometry(&window)?;
        let курсор = self.на_экран(start.location);
        let лево = курсор.x < з.экран.x + g.size.w as f64 / 2.0;
        let верх = курсор.y < з.экран.y + g.size.h as f64 / 2.0;
        let пропорция = self.пропорция_картинки(&window, g.size);
        Some(PinResizeGrab {
            start_data: start,
            window,
            курсор,
            окно: з.экран,
            размер: g.size,
            лево,
            верх,
            гор: true,
            верт: true,
            пропорция,
        })
    }

    /// Ресайз закреплённого окна за ЗАДАННЫЙ край — по просьбе самого клиента
    /// (xdg_toplevel.resize: клиент без рамок тянет себя за кромку, как mpv
    /// с `--border=no`). Без этого клиент тянул бы окно хватом холста, и на
    /// зуме 0.5 край убегал бы от руки вдвое.
    pub fn начать_ресайз_закреплённого_за_край(
        &mut self,
        window: Window,
        start: PointerGrabStartData<Parallax>,
        края: crate::grabs::resize_grab::ResizeEdge,
    ) -> Option<PinResizeGrab> {
        use crate::grabs::resize_grab::ResizeEdge;
        let з = self.закрепление(&window)?;
        let g = self.space.element_geometry(&window)?;
        let курсор = self.на_экран(start.location);
        let пропорция = self.пропорция_картинки(&window, g.size);
        Some(PinResizeGrab {
            start_data: start,
            window,
            курсор,
            окно: з.экран,
            размер: g.size,
            лево: края.contains(ResizeEdge::LEFT),
            верх: края.contains(ResizeEdge::TOP),
            гор: края.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT),
            верт: края.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM),
            пропорция,
        })
    }

    /// Окну-картинке (см. картинка.rs) ресайз держит пропорции: иначе
    /// просмотрщик дорисовывал бы вокруг снимка чёрные поля, и «виджет с
    /// фото» превращался бы в фото в рамке.
    fn пропорция_картинки(&self, window: &Window, размер: Size<i32, Logical>) -> Option<f64> {
        (crate::картинка::картинка_ли(window) && размер.w > 0 && размер.h > 0)
            .then(|| размер.w as f64 / размер.h as f64)
    }
}

/// Перенос закреплённого окна в экранных пикселях.
pub struct PinMoveGrab {
    pub start_data: PointerGrabStartData<Parallax>,
    window: Window,
    /// Курсор на экране в начале хвата.
    курсор: Point<f64, Logical>,
    /// Угол окна на экране в начале хвата.
    окно: Point<f64, Logical>,
}

/// Ресайз закреплённого окна: угол, ближний к курсору в начале хвата.
pub struct PinResizeGrab {
    pub start_data: PointerGrabStartData<Parallax>,
    window: Window,
    курсор: Point<f64, Logical>,
    окно: Point<f64, Logical>,
    размер: Size<i32, Logical>,
    лево: bool,
    верх: bool,
    /// Тянется ли ширина / высота: за боковую кромку меняется только одна.
    гор: bool,
    верт: bool,
    /// Ширина/высота, которую держит ресайз (окно-картинка), иначе свободно.
    пропорция: Option<f64>,
}

impl PinResizeGrab {
    fn применить(&self, data: &mut Parallax, pos: Point<f64, Logical>) {
        let теперь = data.на_экран(pos);
        let d = теперь - self.курсор;
        let (dx, dy) = (d.x.round() as i32, d.y.round() as i32);
        let dx = if self.гор { dx } else { 0 };
        let dy = if self.верт { dy } else { 0 };
        let mut w = if self.лево { self.размер.w - dx } else { self.размер.w + dx }.max(МИН_СТОРОНА);
        let mut h = if self.верх { self.размер.h - dy } else { self.размер.h + dy }.max(МИН_СТОРОНА);
        if let Some(k) = self.пропорция {
            // Ведёт та сторона, которую тянут сильнее (по доле от исходной);
            // за боковую кромку — единственная, что тянется.
            let доля_w = (w - self.размер.w).abs() as f64 / self.размер.w.max(1) as f64;
            let доля_h = (h - self.размер.h).abs() as f64 / self.размер.h.max(1) as f64;
            let по_ширине = if self.гор && self.верт { доля_w >= доля_h } else { self.гор };
            if по_ширине {
                h = ((w as f64 / k).round() as i32).max(МИН_СТОРОНА);
            } else {
                w = ((h as f64 * k).round() as i32).max(МИН_СТОРОНА);
            }
        }
        // Левый/верхний край тянут — правый/нижний стоят на месте.
        let x = if self.лево { self.окно.x + (self.размер.w - w) as f64 } else { self.окно.x };
        let y = if self.верх { self.окно.y + (self.размер.h - h) as f64 } else { self.окно.y };
        let размер: Size<i32, Logical> = (w, h).into();
        crate::xwin::set_size(&self.window, Some(размер), crate::xwin::Tiled::Unset);
        crate::xwin::configure(&self.window);
        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| tw.window == self.window) {
            tw.float_size = Some(размер);
            if let Some(з) = tw.закреплено.as_mut() {
                з.экран = Point::from((x, y));
            }
        }
        data.request_redraw();
    }
}

macro_rules! проброс_жестов {
    () => {
        fn relative_motion(
            &mut self,
            data: &mut Parallax,
            handle: &mut PointerInnerHandle<'_, Parallax>,
            focus: Option<(WlSurface, Point<f64, Logical>)>,
            event: &RelativeMotionEvent,
        ) {
            handle.relative_motion(data, focus, event);
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
        fn unset(&mut self, _data: &mut Parallax) {}
    };
}

impl PointerGrab<Parallax> for PinMoveGrab {
    fn motion(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let теперь = data.на_экран(event.location);
        let цель = self.окно + (теперь - self.курсор);
        let w = self.window.clone();
        data.закреплённое_в(&w, цель);
    }

    fn button(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    проброс_жестов!();
}

impl PointerGrab<Parallax> for PinResizeGrab {
    fn motion(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.применить(data, event.location);
    }

    fn button(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    проброс_жестов!();
}
