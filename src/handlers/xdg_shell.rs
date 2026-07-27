use std::time::Duration;

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
    tiling::{Layout, dwindle_rects, GAP_INNER, GAP_OUTER},
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

        let is_tile = self.tile_config.layout != Layout::Float;

        // Предвычисляем точный размер, чтобы arrange() не слал второй configure.
        let (win_w, win_h) = match self.tile_config.layout {
            // Columns: колонка в полэкрана, полная высота (см. apply_columns_layout).
            Layout::Columns => (
                (output_size.w / 2 - GAP_INNER).max(1),
                (output_size.h - GAP_OUTER * 2).max(1),
            ),
            Layout::Float => (output_size.w / 2, output_size.h / 2),
            // Tile/Monocle — точный размер для n+1 окон по dwindle.
            _ => {
                let current_tags = self.viewport.current_tags();
                let n_visible = self.tagged_windows.iter()
                    .filter(|tw| tw.tags & current_tags != 0 && !tw.floating)
                    .count();
                let n = n_visible + 1;
                let output_geo = smithay::utils::Rectangle::new(
                    (0, 0).into(), output_size
                );
                let rects = dwindle_rects(output_geo, n, true);
                rects.last()
                    .map(|r| (r.size.w, r.size.h))
                    .unwrap_or((output_size.w, output_size.h))
            }
        };

        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
            if is_tile {
                state.states.set(xdg_toplevel::State::TiledLeft);
                state.states.set(xdg_toplevel::State::TiledRight);
                state.states.set(xdg_toplevel::State::TiledTop);
                state.states.set(xdg_toplevel::State::TiledBottom);
            }
            state.size = Some((win_w, win_h).into());
        });
        surface.send_configure();

        let window = Window::new_wayland_window(surface);
        let current_tags = self.viewport.current_tags();

        // Новое окно появляется по центру текущего viewport (позиция камеры),
        // а не под курсором — курсор может быть где угодно на бесконечном холсте.
        let zoom = self.viewport.zoom;
        let camera_center_x = self.viewport.cam_x + output_size.w as f64 / (2.0 * zoom);
        let camera_center_y = self.viewport.cam_y + output_size.h as f64 / (2.0 * zoom);
        let spawn_i32: Point<i32, Logical> = (
            camera_center_x as i32 - win_w / 2,
            camera_center_y as i32 - win_h / 2,
        ).into();

        // Hyprland-dwindle: в тайлинге (Tile/Monocle) новое окно появляется
        // рядом с сфокусированным (делит его слот), а не в конце списка.
        // Columns/Float — в конце (там позицию задаёт columns_insert_new/spawn).
        let insert_idx = if is_tile && self.tile_config.layout != Layout::Columns {
            let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
            focused.as_ref()
                .and_then(|fs| self.tagged_windows.iter().position(|tw| {
                    tw.window.toplevel().map(|t| t.wl_surface() == fs).unwrap_or(false)
                }))
                .map(|i| i + 1)
                .unwrap_or(self.tagged_windows.len())
        } else {
            self.tagged_windows.len()
        };

        self.tagged_windows.insert(insert_idx, TaggedWindow {
            window: window.clone(),
            tags: current_tags,
            position: spawn_i32,
            float_position: spawn_i32,
            float_size: None,
            float_position_set: false,
            floating: false,
            folded: false,
        });

        self.space.map_element(window.clone(), spawn_i32, false);
        // Columns (niri): новое окно — отдельная колонка сразу справа от
        // активной, камера едет к нему (см. columns.rs). Делаем ДО arrange(),
        // чтобы reconcile не дописал его в конец.
        if self.tile_config.layout == Layout::Columns {
            self.columns_insert_new(&window);
        }
        self.arrange();
        if self.tile_config.layout == Layout::Columns {
            self.columns_scroll_to_active();
        }

        // Появление "с ростом" (Hyprland-style) — только Float: в тайлинге
        // размер и так уже точный (см. выше), а позицию уже анимирует arrange().
        if !is_tile {
            let center = Point::from((
                (spawn_i32.x + win_w / 2) as f64,
                (spawn_i32.y + win_h / 2) as f64,
            ));
            self.window_open_anims.push((
                window.clone(),
                crate::anim::OpenAnim::new(center, (win_w as f64, win_h as f64), Duration::from_millis(260)),
            ));
        }

        // Автоматически отдаём фокус новому окну (как sway/hyprland)
        if let Some(kb) = self.seat.get_keyboard() {
            if let Some(top) = window.toplevel() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                kb.set_focus(self, Some(top.wl_surface().clone()), serial);
                tracing::info!("dawn: auto-focus new toplevel");
            }
        }

        tracing::info!(
            "dawn: new_toplevel idx={} tags={:#b} spawn=({},{}) size={}x{}",
            insert_idx, current_tags,
            spawn_i32.x, spawn_i32.y,
            win_w, win_h,
        );

        // map_element сам по себе экран не обновляет — если VBlank-цепочка
        // рендера к этому моменту уже остановилась (нет изменений с прошлого
        // кадра), новое окно так и останется невидимым до явного рендера.
        self.request_redraw();
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        self.tagged_windows.retain(|tw| {
            tw.window.toplevel()
                .map(|t| t.wl_surface() != surface.wl_surface())
                .unwrap_or(true)
        });
        // Иначе анимации продолжат слать configure/map_element мёртвому окну.
        self.cancel_window_anims(surface.wl_surface());
        // Убираем мёртвые ссылки из выделения/созвездий (см. selection.rs).
        self.selected_windows.retain(|w| {
            w.toplevel().map(|t| t.wl_surface() != surface.wl_surface()).unwrap_or(true)
        });
        for group in &mut self.constellations {
            group.retain(|w| {
                w.toplevel().map(|t| t.wl_surface() != surface.wl_surface()).unwrap_or(true)
            });
        }
        self.constellations.retain(|g| g.len() > 1);
        self.arrange();
        self.request_plane_reset();
        tracing::info!("dawn: toplevel_destroyed count={}", self.tagged_windows.len());
        self.request_redraw();
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
            tw.window.toplevel().map(|t| t.wl_surface() == surface.wl_surface()).unwrap_or(false)
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
