//! Интеграция XWayland: запуск сервера, оконный менеджер X11 (XWM) и сшивка
//! X11-окон с моделью dawn (теги, раскладки, бесконечный холст).
//!
//! Как это устроено:
//!   * [`start`] поднимает процесс `Xwayland` и вешает его в event loop; когда
//!     сервер готов, поднимается [`X11Wm`] — наш X11 window manager;
//!   * X11-окна становятся обычными `desktop::Window` (`new_x11_window`) и
//!     дальше живут в тех же `tagged_windows`/`space`, что и Wayland-окна
//!     (см. xwin.rs — там абстракция над обоими протоколами);
//!   * позиции X11-окнам досылает [`Dawn::sync_x11_geometry`] один раз за
//!     итерацию главного цикла — см. комментарий у функции.
//!
//! Ограничение бесконечного холста: у X-сервера корневое окно конечно (размер
//! выхода), а указатель вне него стоять не может — X зажимает его на край.
//! Поэтому позиции X11-окон зажимаются в экран, а координаты, которые клиент
//! выбирает сам (меню, тултипы), переводятся обратно через [`Dawn::x11_to_canvas`].
//! Сами окна от этого не двигаются: на холсте их рисуем мы.

use std::process::Stdio;

use smithay::{
    delegate_xwayland_shell,
    desktop::Window,
    input::pointer::Focus,
    reexports::calloop::EventLoop,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER, Size},
    wayland::{
        selection::{
            SelectionTarget,
            data_device::{
                clear_data_device_selection, current_data_device_selection_userdata,
                request_data_device_client_selection, set_data_device_selection,
            },
        },
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
    },
};

use crate::{
    grabs::{move_grab::MoveSurfaceGrab, resize_grab::ResizeEdge, resize_grab::ResizeSurfaceGrab},
    state::Dawn,
    xwin,
};

/// Запустить XWayland. Всё асинхронно: процесс поднимается сразу, а XWM
/// стартует уже по событию `Ready` — до этого X11-клиенты просто не смогут
/// подключиться.
pub fn start(event_loop: &mut EventLoop<'static, Dawn>, state: &mut Dawn) {
    let (xwayland, client) = match XWayland::spawn(
        &state.display_handle,
        None,
        std::iter::empty::<(String, String)>(),
        true,
        Stdio::null(),
        Stdio::null(),
        |_| (),
    ) {
        Ok(v) => v,
        Err(err) => {
            // Не фатально: без XWayland dawn просто остаётся чисто
            // wayland-компоситором.
            tracing::warn!("dawn/xwayland: не удалось запустить Xwayland: {}", err);
            return;
        }
    };

    let handle = event_loop.handle();
    let display_handle = state.display_handle.clone();
    let ret = handle.clone().insert_source(xwayland, move |event, _, state| match event {
        XWaylandEvent::Ready {
            x11_socket,
            display_number,
        } => {
            let wm = match X11Wm::start_wm(handle.clone(), &display_handle, x11_socket, client.clone())
            {
                Ok(wm) => wm,
                Err(err) => {
                    tracing::error!("dawn/xwayland: не удалось поднять XWM: {}", err);
                    return;
                }
            };
            state.xwm = Some(wm);
            state.xdisplay = Some(display_number);
            // DISPLAY нужен всему, что мы запускаем из компоситора (спавн
            // терминала по Super+Enter и т.п.), иначе X11-клиенты пойдут в
            // чужой сервер или никуда.
            unsafe { std::env::set_var("DISPLAY", format!(":{display_number}")) };
            tracing::info!("dawn/xwayland: XWM готов, DISPLAY=:{}", display_number);
            // Повторяем экспорт уже с DISPLAY: порталу он нужен, чтобы X11-часть
            // (например файловый диалог из Xwayland-клиента) шла в НАШ сервер.
            crate::export_session_env();
        }
        XWaylandEvent::Error => {
            tracing::warn!("dawn/xwayland: Xwayland не смог стартовать");
        }
    });
    if let Err(err) = ret {
        tracing::error!("dawn/xwayland: insert_source: {}", err);
    }
}

impl XWaylandShellHandler for Dawn {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
delegate_xwayland_shell!(Dawn);

impl Dawn {
    /// Окно dawn, за которым стоит эта X11-поверхность. Ищем в tagged_windows,
    /// а не в space: окно чужого тега из space убрано (см. refresh_tags), но
    /// продолжает существовать.
    pub fn window_for_x11(&self, surface: &X11Surface) -> Option<Window> {
        self.tagged_windows
            .iter()
            .map(|tw| &tw.window)
            .chain(self.space.elements()) // + override-redirect окна (их нет в тегах)
            .find(|w| w.x11_surface() == Some(surface))
            .cloned()
    }

    /// Поднять X11-окно наверх в стеке X-сервера. Для X11-клиентов важен не
    /// только наш порядок отрисовки: свой стек X держит сам и по нему решает,
    /// например, куда класть меню.
    pub fn raise_x11(&mut self, window: &Window) {
        let Some(surface) = window.x11_surface().cloned() else {
            return;
        };
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.raise_window(&surface) {
                tracing::warn!("dawn/xwayland: raise_window: {}", err);
            }
        }
    }

    /// Куда уехала X-позиция окна относительно его позиции на холсте.
    ///
    /// Ненулевая у окон, которые холст унёс за пределы корневого окна X: их
    /// позицию мы зажимаем (см. [`Dawn::sync_x11_geometry`]). Нужна, чтобы
    /// перевести обратно в холст координаты, которые X11-клиент выбрал сам —
    /// позиции меню и тултипов (override-redirect).
    fn x11_offset(&self, window: &Window) -> Option<Point<i32, Logical>> {
        let surface = window.x11_surface()?;
        let canvas = self.space.element_location(window)?;
        Some(surface.geometry().loc - canvas)
    }

    /// Перевести точку из координат X-сервера в координаты холста.
    ///
    /// Клиент выбирает место для меню в корневых координатах, отсчитывая его
    /// от позиции своего окна — то есть от ЗАЖАТОЙ позиции. Чтобы вернуть меню
    /// под его пункт, надо вычесть ровно то смещение, которое мы этому окну и
    /// дали. Ищем окно, в чей X-прямоугольник попала точка; если не нашли (меню
    /// вылезло за край родителя) — берём смещение окна в фокусе.
    pub fn x11_to_canvas(&self, point: Point<i32, Logical>) -> Point<i32, Logical> {
        let mut focused_offset = None;
        for window in self.space.elements() {
            let Some(surface) = window.x11_surface() else {
                continue;
            };
            if surface.is_override_redirect() {
                continue;
            }
            let Some(offset) = self.x11_offset(window) else {
                continue;
            };
            if surface.geometry().contains(point) {
                return point - offset;
            }
            if focused_offset.is_none()
                && self.focused_surface().is_some_and(|f| crate::xwin::is_surface(window, &f))
            {
                focused_offset = Some(offset);
            }
        }
        point - focused_offset.unwrap_or_default()
    }

    /// Досылает X11-окнам их позицию.
    ///
    /// Wayland-клиент про свою позицию ничего не знает — её целиком держит
    /// компоситор. X11-клиент, наоборот, живёт в корневых координатах, и всё,
    /// что двигает окна в dawn (arrange, анимации позиций, драг мышью,
    /// инерция, обзор столов), обязано эту позицию ему сообщать. Вместо того
    /// чтобы дописывать configure к каждому из десятков вызовов
    /// `space.map_element`, один раз за итерацию главного цикла сверяем
    /// позицию в space с последней отосланной и досылаем разницу.
    ///
    /// Размер сюда не входит: его задаёт раскладка через [`xwin::set_size`],
    /// а `space` показывает фактический размер буфера клиента, который
    /// отстаёт на кадр-другой — сверять его здесь значило бы гонять configure
    /// по кругу.
    pub fn sync_x11_geometry(&mut self) {
        // Координаты X-сервера — это координаты холста, ЗАЖАТЫЕ в корневое окно.
        //
        // Сдвиг сам по себе безобиден: событие мыши доходит до X11-клиента через
        // Xwayland, который берёт surface-локальную точку и прибавляет позицию
        // окна, известную X-серверу — ровно ту, что мы отправили. Клиент тут же
        // вычитает её обратно, сверяя с геометрией своего окна, и сдвиг
        // сокращается, каким бы он ни был.
        //
        // Чего он НЕ переживает — выхода за корневое окно. Указатель в X не
        // может стоять вне экрана: всё, что снаружи, X зажимает на край, и до
        // клиента доезжает точка, которая после вычитания даёт мусор. Холст же
        // бесконечен, так что окно, отъехавшее от начала холста дальше экрана,
        // теряло клики целиком (замер 05.08.2026: окно на (-812,831), клик по
        // кнопке даёт root (-567,1433) при экране 2560×1080 — Amnezia не
        // реагирует; то же окно на (137,409) — root (351,1001), клик проходит).
        // Поэтому позицию зажимаем так, чтобы ВСЁ окно лежало внутри экрана.
        //
        // Зажим считается от холста, а не от камеры: иначе панорамирование и
        // зум гнали бы поток configure каждый кадр. Цена — override-redirect
        // окна: место для меню клиент выбирает сам, отсчитывая от зажатой
        // позиции, и обратно в холст мы его переводим через [`x11_to_canvas`].
        let экран = self.screen_size();
        let updates: Vec<(X11Surface, Rectangle<i32, Logical>)> = self
            .space
            .elements()
            .filter_map(|window| {
                let surface = window.x11_surface()?;
                if surface.is_override_redirect() {
                    // Позицию таких окон (меню, тултипы) выбирает сам клиент.
                    return None;
                }
                let loc = self.space.element_location(window)?;
                let current = surface.geometry();
                // Окно шире или выше экрана внутрь не влезет — прижимаем к
                // началу: max(0) вместо отрицательной границы (clamp с min >
                // max паникует).
                let loc = smithay::utils::Point::from((
                    loc.x.clamp(0, (экран.w - current.size.w).max(0)),
                    loc.y.clamp(0, (экран.h - current.size.h).max(0)),
                ));
                (current.loc != loc).then(|| (surface.clone(), Rectangle::new(loc, current.size)))
            })
            .collect();

        for (surface, rect) in updates {
            if let Err(err) = surface.configure(rect) {
                tracing::warn!("dawn/xwayland: configure(pos) failed: {}", err);
            }
        }
    }

}

impl XwmHandler for Dawn {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().unwrap()
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {}
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Err(err) = surface.set_mapped(true) {
            tracing::warn!("dawn/xwayland: set_mapped: {}", err);
            return;
        }
        // Диалоги, сплеши и окна с зафиксированным размером тайлить нельзя —
        // они либо игнорируют наш configure, либо ломаются об него.
        let floating = is_dialog(&surface);
        let size = if floating {
            let geo = surface.geometry().size;
            if geo.w > 0 && geo.h > 0 {
                geo
            } else {
                Size::from((640, 480))
            }
        } else {
            self.predict_new_window_size()
        };

        let window = Window::new_x11_window(surface);
        xwin::set_size(&window, Some(size), xwin::Tiled::Keep);
        self.insert_new_window(window, size, floating);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        // Меню, тултипы, drag-иконки: позицию выбрал клиент в корневых
        // координатах, отсчитав её от зажатой позиции своего окна — переводим
        // обратно в холст (см. sync_x11_geometry). В tagged_windows такие окна
        // не заводим: тегов у них нет, живут они ровно до закрытия меню.
        let location = self.x11_to_canvas(surface.geometry().loc);
        let window = Window::new_x11_window(surface);
        self.space.map_element(window, location, true);
        self.request_redraw();
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.window_for_x11(&surface) {
            self.forget_window(&window);
        }
        if !surface.is_override_redirect() {
            let _ = surface.set_mapped(false);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.window_for_x11(&surface) {
            self.forget_window(&window);
        }
    }

    /// X11-клиент просит полный экран (_NET_WM_STATE_FULLSCREEN): игры,
    /// видеоплееры, полноэкранный режим браузера. Тот же путь, что и F11.
    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        if let Some(window) = self.window_for_x11(&surface) {
            self.set_fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        let это_окно = self.window_for_x11(&surface)
            .is_some_and(|w| self.fullscreen_requested(&w));
        if это_окно {
            self.unset_fullscreen();
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // Размер отдаём клиенту (в тайлинге его тут же перебьёт arrange),
        // позицию — нет: где лежат окна, решает раскладка dawn.
        let mut geo = surface.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        if let Err(err) = surface.configure(geo) {
            tracing::warn!("dawn/xwayland: configure_request: {}", err);
        }
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        // Интересны только override-redirect окна: они двигают себя сами, и
        // единственный способ узнать, куда — этот колбэк. Позицию обычных
        // окон мы задаём сами (sync_x11_geometry), и принимать её обратно
        // значило бы драться с собственной раскладкой.
        if !surface.is_override_redirect() {
            return;
        }
        if let Some(window) = self.window_for_x11(&surface) {
            let loc = self.x11_to_canvas(geometry.loc);
            self.space.map_element(window, loc, true);
            self.request_redraw();
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _button: u32,
        edges: X11ResizeEdge,
    ) {
        let Some(window) = self.window_for_x11(&surface) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some(geo) = self.space.element_geometry(&window) else {
            return;
        };
        self.freeze_window_anim(&window);
        let grab = ResizeSurfaceGrab::start(start_data, window, x11_edges(edges), geo, Vec::new());
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        let Some(window) = self.window_for_x11(&surface) else {
            return;
        };
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        let Some(start_data) = pointer.grab_start_data() else {
            return;
        };
        let Some(loc) = self.space.element_location(&window) else {
            return;
        };
        self.freeze_window_anim(&window);
        let grab = MoveSurfaceGrab::new(start_data, window, loc, Vec::new());
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    // ── Буфер обмена: X11 → Wayland ──────────────────────────────────────────

    fn allow_selection_access(&mut self, xwm: XwmId, _selection: SelectionTarget) -> bool {
        // Отдавать выделение наружу можно только пока фокус у X11-окна этого
        // же XWM — иначе любой X11-клиент мог бы читать чужой буфер обмена.
        let Some(focused) = self.focused_surface() else {
            return false;
        };
        self.space
            .elements()
            .filter(|w| xwin::is_surface(w, &focused))
            .filter_map(|w| w.x11_surface())
            .any(|s| s.xwm_id() == Some(xwm))
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        if selection == SelectionTarget::Clipboard {
            set_data_device_selection(&self.display_handle, &self.seat.clone(), mime_types, ());
        }
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
    ) {
        if selection == SelectionTarget::Clipboard {
            if let Err(err) = request_data_device_client_selection(&self.seat, mime_type, fd) {
                tracing::warn!("dawn/xwayland: чтение буфера Wayland для X11: {}", err);
            }
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        if selection == SelectionTarget::Clipboard
            && current_data_device_selection_userdata(&self.seat).is_some()
        {
            clear_data_device_selection(&self.display_handle, &self.seat.clone());
        }
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        tracing::warn!("dawn/xwayland: XWM отвалился");
        self.xwm = None;
    }
}

/// Окна, которые нельзя тайлить: диалоги/сплеши/утилиты по типу окна и всё, у
/// чего минимальный размер совпал с максимальным (клиент запретил ресайз).
fn is_dialog(surface: &X11Surface) -> bool {
    use smithay::xwayland::xwm::WmWindowType;
    if surface.is_popup() {
        return true;
    }
    if let Some(ty) = surface.window_type() {
        if matches!(
            ty,
            WmWindowType::Dialog
                | WmWindowType::Splash
                | WmWindowType::Utility
                | WmWindowType::Toolbar
                | WmWindowType::Menu
                | WmWindowType::DropdownMenu
                | WmWindowType::PopupMenu
                | WmWindowType::Tooltip
                | WmWindowType::Notification
        ) {
            return true;
        }
    }
    matches!((surface.min_size(), surface.max_size()), (Some(min), Some(max)) if min == max)
}

fn x11_edges(edges: X11ResizeEdge) -> ResizeEdge {
    match edges {
        X11ResizeEdge::Top => ResizeEdge::TOP,
        X11ResizeEdge::Bottom => ResizeEdge::BOTTOM,
        X11ResizeEdge::Left => ResizeEdge::LEFT,
        X11ResizeEdge::Right => ResizeEdge::RIGHT,
        X11ResizeEdge::TopLeft => ResizeEdge::TOP_LEFT,
        X11ResizeEdge::TopRight => ResizeEdge::TOP_RIGHT,
        X11ResizeEdge::BottomLeft => ResizeEdge::BOTTOM_LEFT,
        X11ResizeEdge::BottomRight => ResizeEdge::BOTTOM_RIGHT,
    }
}
