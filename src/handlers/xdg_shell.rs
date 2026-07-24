use smithay::{
    delegate_xdg_shell,
    desktop::Window,
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_seat::WlSeat,
    },
    utils::{Logical, Point, Serial, Size},
    wayland::shell::xdg::{
        PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
    },
};

use crate::{
    state::{Dawn, TaggedWindow},
    tiling::Layout,
};

impl XdgShellHandler for Dawn {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        // Берём размер первого output'а
        let output_size: Size<i32, Logical> = self
            .space
            .outputs()
            .next()
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| g.size)
            .unwrap_or_else(|| (1920, 1080).into());

        // Начальный размер окна — половина экрана
        let win_w = output_size.w / 2;
        let win_h = output_size.h / 2;

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            state.states.set(xdg_toplevel::State::TiledLeft);
            state.states.set(xdg_toplevel::State::TiledRight);
            state.states.set(xdg_toplevel::State::TiledTop);
            state.states.set(xdg_toplevel::State::TiledBottom);
            // Отправляем реальный размер — без этого клиент получает 0x0
            // и падает с "Ignoring resize request for tiny size"
            state.size = Some((win_w, win_h).into());
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        let current_tags = self.viewport.current_tags();

        // Курсор — если (0,0), центрируем
        let cursor: Point<f64, Logical> = self
            .seat
            .get_pointer()
            .map(|p| p.current_location())
            .unwrap_or(self.pointer_location);

        let cursor_i32: Point<i32, Logical> = if cursor.x == 0.0 && cursor.y == 0.0 {
            // В TTY курсор стартует в (0,0) — ставим в центр экрана
            (output_size.w / 4, output_size.h / 4).into()
        } else {
            (cursor.x as i32, cursor.y as i32).into()
        };

        let insert_idx = self
            .space
            .element_under(cursor)
            .and_then(|(w, _)| {
                self.tagged_windows.iter().position(|tw| {
                    tw.window.toplevel().zip(w.toplevel())
                        .map(|(a, b)| a.wl_surface() == b.wl_surface())
                        .unwrap_or(false)
                })
            })
            .unwrap_or(self.tagged_windows.len());

        self.tagged_windows.insert(insert_idx, TaggedWindow {
            window: window.clone(),
            tags: current_tags,
            position: cursor_i32,
            floating: false,
        });

        self.space.map_element(window.clone(), cursor_i32, false);
        self.arrange();

        // Автоматически отдаём фокус новому окну (как sway/hyprland)
        if let Some(kb) = self.seat.get_keyboard() {
            if let Some(top) = window.toplevel() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                kb.set_focus(self, Some(top.wl_surface().clone()), serial);
                tracing::info!("dawn: auto-focus new toplevel");
            }
        }

        tracing::info!(
            "dawn: new_toplevel idx={} tags={:#b} cursor=({},{}) size={}x{}",
            insert_idx, current_tags,
            cursor_i32.x, cursor_i32.y,
            win_w, win_h,
        );

        // map_element сам по себе экран не обновляет — если VBlank-цепочка
        // рендера к этому моменту уже остановилась (нет изменений с прошлого
        // кадра), новое окно так и останется невидимым до явного рендера.
        crate::udev::render_all(self);
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.tagged_windows.retain(|tw| {
            tw.window.toplevel()
                .map(|t| t.wl_surface() != surface.wl_surface())
                .unwrap_or(true)
        });
        self.arrange();
        tracing::info!("dawn: toplevel_destroyed count={}", self.tagged_windows.len());
        crate::udev::render_all(self);
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
