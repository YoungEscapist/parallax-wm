use smithay::{
    delegate_xdg_decoration, delegate_xdg_shell,
    desktop::{
        PopupKeyboardGrab, PopupKind, PopupPointerGrab, PopupUngrabStrategy, Window,
        find_popup_root_surface, get_popup_toplevel_coords,
    },
    input::{Seat, pointer::Focus},
    reexports::{
        wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1::Mode as DecorationMode,
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_seat::WlSeat,
        wayland_server::protocol::wl_output::WlOutput,
    },
    utils::{Logical, Point, Rectangle, Serial, Size},
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
        decoration::XdgDecorationHandler,
    },
};

use crate::{state::Dawn, tiling::Layout};

impl XdgShellHandler for Dawn {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Размер предвычисляем ДО первого configure, чтобы arrange() не слал
        // второй (см. Dawn::predict_new_window_size).
        let size = self.predict_new_window_size();
        let is_tile = self.tile_config.layout != Layout::Float;

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            if is_tile {
                state.states.set(xdg_toplevel::State::TiledLeft);
                state.states.set(xdg_toplevel::State::TiledRight);
                state.states.set(xdg_toplevel::State::TiledTop);
                state.states.set(xdg_toplevel::State::TiledBottom);
            }
            state.size = Some(size);
        });
        surface.send_configure();

        // Дальше окно ничем не отличается от X11-го — общий путь в xwin.rs.
        self.insert_new_window(Window::new_wayland_window(surface), size, false);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        let window = self.tagged_windows.iter()
            .map(|tw| tw.window.clone())
            .find(|w| crate::xwin::is_surface(w, surface.wl_surface()));
        if let Some(window) = window {
            self.forget_window(&window);
        }
        tracing::info!("dawn: toplevel_destroyed count={}", self.tagged_windows.len());
    }

    /// Восстановление позиции из сохранённой сессии (4.3): app_id обычно
    /// приходит от клиента ПОСЛЕ new_toplevel (первого коммита ещё не было),
    /// поэтому ждём именно этот колбэк, а не new_toplevel.
    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        // Значок для чипа в панели ищем ЗДЕСЬ по той же причине, по которой
        // здесь же восстанавливается позиция: на `new_toplevel` app_id ещё
        // пустой, а без него искать нечего (см. Dawn::ensure_chip_icon).
        if let Some(window) = self.tagged_windows.iter()
            .find(|tw| crate::xwin::is_surface(&tw.window, surface.wl_surface()))
            .map(|tw| tw.window.clone())
        {
            self.ensure_chip_icon(&window);
        }
        let app_id = match crate::session::toplevel_app_id(&surface) {
            Some(id) => id,
            None => return,
        };
        let saved_pos = match self.pending_session.get_mut(&app_id).and_then(|v| v.pop()) {
            Some(p) => p,
            None => return,
        };
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
            crate::xwin::is_surface(&tw.window, &surface.wl_surface())
        }) {
            tw.position = saved_pos;
            tw.float_position = saved_pos;
            tw.float_position_set = true;
            self.space.map_element(tw.window.clone(), saved_pos, false);
            self.request_plane_reset();
            tracing::info!("dawn/session: восстановлена позиция app_id={} → {:?}", app_id, saved_pos);
            self.request_redraw();
        }
    }

    /// Клиент сам просится на весь экран: полноэкранное видео, игра,
    /// демонстрация экрана. Ведём себя ровно как по F11 — иначе кнопка
    /// «во весь экран» внутри приложения не делает ничего.
    fn fullscreen_request(&mut self, surface: ToplevelSurface, _output: Option<WlOutput>) {
        let window = self.tagged_windows.iter()
            .map(|tw| tw.window.clone())
            .find(|w| crate::xwin::is_surface(w, surface.wl_surface()));
        if let Some(window) = window {
            self.set_fullscreen(&window);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        // Сворачиваем ИМЕННО то окно, которое попросило: развёрнутых окон может
        // быть несколько (по одному на стол), и «свернуть текущее» здесь
        // означало бы свернуть чужое.
        let window = self.tagged_windows.iter()
            .map(|tw| tw.window.clone())
            .find(|w| crate::xwin::is_surface(w, surface.wl_surface()));
        if let Some(window) = window {
            self.unset_fullscreen_window(&window);
        }
    }

    /// Всплывающее окно клиента: выпадающее меню, контекстное меню, подсказка,
    /// список автодополнения.
    ///
    /// Раньше здесь было пусто — и это ломало ВСЕ меню нативных
    /// Wayland-приложений разом. Popup в smithay живёт не сам по себе: и
    /// отрисовка (`Window::render_elements`), и хит-тест
    /// (`Window::surface_under`, `bbox_with_popups`) ходят за списком попапов в
    /// `PopupManager::popups_for_surface`, а туда попадает только то, что
    /// зарегистрировали через `track_popup`. Без регистрации меню не
    /// рисовалось и не получало кликов: снаружи это выглядело как «кнопка,
    /// открывающая меню, не работает» — а такие кнопки почти всегда справа
    /// (гамбургер, «⋮», меню профиля).
    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        if let Err(err) = self.popups.track_popup(PopupKind::Xdg(surface)) {
            tracing::warn!("dawn: не удалось завести попап: {}", err);
        }
    }

    /// Клиент просит захват на время меню: пока оно открыто, ввод принадлежит
    /// ему, а клик мимо — закрывает. Без этого меню не закрывается по щелчку
    /// снаружи и не отдаёт клавиатуру (стрелки, Escape).
    fn grab(&mut self, surface: PopupSurface, seat: WlSeat, serial: Serial) {
        let Some(seat) = Seat::<Dawn>::from_resource(&seat) else { return };
        let kind = PopupKind::Xdg(surface);
        let Some(root) = find_popup_root_surface(&kind).ok().and_then(|root| {
            self.space
                .elements()
                .find(|w| crate::xwin::is_surface(w, &root))
                .cloned()
                .and_then(|w| crate::focus::KeyboardFocusTarget::for_window(&w))
        }) else {
            return;
        };

        let Ok(mut grab) = self.popups.grab_popup(root, kind, &seat, serial) else { return };

        // Чужой захват отдавать нельзя: если клавиатуру/указатель уже держит
        // кто-то другой (не эта же цепочка меню), меню молча закрываем.
        if let Some(keyboard) = seat.get_keyboard() {
            if keyboard.is_grabbed()
                && !(keyboard.has_grab(serial)
                    || keyboard.has_grab(grab.previous_serial().unwrap_or(grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            keyboard.set_focus(self, grab.current_grab(), serial);
            keyboard.set_grab(self, PopupKeyboardGrab::new(&grab), serial);
        }
        if let Some(pointer) = seat.get_pointer() {
            if pointer.is_grabbed()
                && !(pointer.has_grab(serial)
                    || pointer.has_grab(grab.previous_serial().unwrap_or(grab.serial())))
            {
                grab.ungrab(PopupUngrabStrategy::All);
                return;
            }
            pointer.set_grab(self, PopupPointerGrab::new(&grab), serial, Focus::Keep);
        }
    }

    /// Клиент пересчитал место для уже открытого меню (подменю уехало за край).
    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }
}

impl Dawn {
    /// Подвинуть меню так, чтобы оно поместилось на экране.
    ///
    /// Клиент выбирает место сам, относительно своего окна, и без поправки
    /// меню у правого края экрана вылезло бы за него. Прямоугольник, в который
    /// вписываем, задаётся в координатах РОДИТЕЛЬСКОГО окна — поэтому от
    /// экрана вычитается и позиция окна, и смещение попапа внутри цепочки
    /// (у подменю родитель — не toplevel, а меню выше).
    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let kind = PopupKind::Xdg(popup.clone());
        let Ok(root) = find_popup_root_surface(&kind) else { return };
        let Some(window) = self
            .space
            .elements()
            .find(|w| crate::xwin::is_surface(w, &root))
            .cloned()
        else {
            return;
        };
        let Some(window_geo) = self.space.element_geometry(&window) else { return };

        // Вписываем в ВИДИМУЮ часть холста, а не в output_geometry: у второго
        // размер всегда экранный, зум в него не входит (см. visible_canvas_rect).
        let mut target = self.visible_canvas_rect();
        target.loc -= get_popup_toplevel_coords(&kind);
        target.loc -= window_geo.loc;

        let (было, стало, по_центру) = popup.with_pending_state(|state| {
            let требуемое = state.positioner.get_geometry();
            let новое = match center_if_window_sized(target, требуемое.size) {
                Some(loc) => Rectangle::new(loc, требуемое.size),
                None => state.positioner.get_unconstrained_geometry(target),
            };
            state.geometry = новое;
            (требуемое, новое, новое.loc != требуемое.loc && новое.size == требуемое.size)
        });

        tracing::debug!(
            "dawn/popup: окно={:?} рамка={:?} просили={:?}×{:?} стало={:?} по_центру={}",
            window_geo,
            target,
            было.loc,
            было.size,
            стало,
            по_центру,
        );
    }
}

/// Попап размером почти во весь экран — это не меню, а окно: просмотрщик
/// картинок (telegram), оверлей, всплывающий плеер. Для него возвращается
/// позиция ПО ЦЕНТРУ видимой области, для обычного меню — None (пусть его
/// двигает позиционер клиента).
///
/// Зачем: `get_unconstrained_geometry` умеет только двигать/переворачивать
/// прямоугольник, чтобы тот влез в рамку. Прямоугольник размером с рамку (или
/// больше) влезть не может, и подгонка прижимает его к углу — то есть к углу
/// экрана. Именно так открывался просмотр картинок.
fn center_if_window_sized(
    target: Rectangle<i32, Logical>,
    requested: Size<i32, Logical>,
) -> Option<Point<i32, Logical>> {
    // 70% хотя бы по одной стороне: меню такой ширины/высоты не бывает, а
    // просмотрщик, наоборот, просит почти весь экран.
    let крупный = requested.w * 10 >= target.size.w * 7
        || requested.h * 10 >= target.size.h * 7;
    if !крупный {
        return None;
    }
    Some(Point::from((
        target.loc.x + (target.size.w - requested.w) / 2,
        target.loc.y + (target.size.h - requested.h) / 2,
    )))
}

#[cfg(test)]
mod tests {
    use super::center_if_window_sized;
    use smithay::utils::{Logical, Point, Rectangle, Size};

    /// Рамка в координатах родительского окна: окно стоит не в начале холста,
    /// поэтому её угол отрицательный — так это и приходит в unconstrain_popup.
    fn рамка() -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((-400, -250)), Size::from((2560, 1080)))
    }

    #[test]
    fn меню_позиционер_двигает_сам() {
        assert_eq!(center_if_window_sized(рамка(), Size::from((220, 400))), None);
        // Даже широкое меню (половина экрана) остаётся меню.
        assert_eq!(center_if_window_sized(рамка(), Size::from((1200, 300))), None);
    }

    #[test]
    fn просмотрщик_во_весь_экран_встаёт_по_центру() {
        let loc = center_if_window_sized(рамка(), Size::from((2560, 1080)))
            .expect("попап размером с экран должен центрироваться");
        // Ровно рамка: центр совпадает с её углом, а не уезжает в угол экрана.
        assert_eq!(loc, Point::from((-400, -250)));
    }

    #[test]
    fn крупный_попап_ставится_серединой_в_середину_рамки() {
        let requested = Size::from((2000, 900));
        let loc = center_if_window_sized(рамка(), requested).expect("крупный");
        let центр_попапа = (loc.x + requested.w / 2, loc.y + requested.h / 2);
        let центр_рамки = (-400 + 2560 / 2, -250 + 1080 / 2);
        assert_eq!(центр_попапа, центр_рамки);
    }

    #[test]
    fn попап_больше_экрана_не_прижимается_к_углу() {
        // На приближенном зуме видимая область меньше экрана — просмотрщик
        // тогда крупнее рамки. Позиция обязана остаться отрицательной
        // (окно шире рамки и торчит за оба края), а не быть углом рамки.
        let узкая = Rectangle::new(Point::from((0, 0)), Size::from((1200, 600)));
        let loc = center_if_window_sized(узкая, Size::from((2560, 1080))).expect("крупный");
        assert_eq!(loc, Point::from((-680, -240)));
    }
}

delegate_xdg_shell!(Dawn);

/// xdg-decoration: dawn рисует рамки, тени и подсветку фокуса сам (см.
/// decor.rs), поэтому всем клиентам отвечаем «декорации серверные».
///
/// Без этого протокола GTK-приложения (ghostty, nautilus, любой libadwaita)
/// остаются со СВОИМ заголовком, а его содержимое задаёт окну жёсткий
/// минимальный размер: у ghostty это 315 px по ширине. В тайлинге слот
/// становился уже — и окно, не умея сжаться, лезло на соседей (замер: слот
/// 63×115 → окно 315×126, перелив 252 px). Alacritty своего заголовка не
/// рисует, поэтому у него пол ~23 px и проблема не проявлялась.
impl XdgDecorationHandler for Dawn {
    fn new_decoration(&mut self, toplevel: ToplevelSurface) {
        self.set_server_decoration(&toplevel);
    }

    fn request_mode(&mut self, toplevel: ToplevelSurface, _mode: DecorationMode) {
        // Пожелание клиента игнорируем сознательно: две рамки (наша и его)
        // выглядели бы как рамка внутри рамки, а CSD ещё и тянет за собой
        // headerbar с его минимальной шириной.
        self.set_server_decoration(&toplevel);
    }

    fn unset_mode(&mut self, toplevel: ToplevelSurface) {
        self.set_server_decoration(&toplevel);
    }
}

impl Dawn {
    fn set_server_decoration(&mut self, toplevel: &ToplevelSurface) {
        toplevel.with_pending_state(|state| {
            state.decoration_mode = Some(DecorationMode::ServerSide);
        });
        if toplevel.is_initial_configure_sent() {
            toplevel.send_pending_configure();
        }
    }
}

delegate_xdg_decoration!(Dawn);
