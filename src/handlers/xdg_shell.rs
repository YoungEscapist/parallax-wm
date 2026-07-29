use smithay::{
    delegate_xdg_shell,
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_seat::WlSeat,
    },
    utils::Serial,
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
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

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {}
    fn grab(&mut self, _surface: PopupSurface, _seat: WlSeat, _serial: Serial) {}
    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {}
}

delegate_xdg_shell!(Dawn);
