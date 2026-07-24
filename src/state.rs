use std::{collections::HashMap, ffi::OsString, sync::Arc};

use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState},
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    input::pointer::CursorImageStatus,
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        dmabuf::{DmabufGlobal, DmabufState},
        output::OutputManagerState,
        selection::data_device::DataDeviceState,
        shell::xdg::XdgShellState,
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};

// ── Viewport ─────────────────────────────────────────────────────────────────

pub struct Viewport {
    pub cam_x: f64,
    pub cam_y: f64,
    pub zoom: f64,
    pub tagset: [u32; 2],   // два тагсета как в dwl (для toggle)
    pub seltags: usize,      // какой тагсет активен
    pub canvas_mode: bool,
}

impl Default for Viewport {
    fn default() -> Self {
        Self {
            cam_x: 0.0, cam_y: 0.0, zoom: 1.0,
            tagset: [1, 1], // начинаем на tag 1
            seltags: 0,
            canvas_mode: false,
        }
    }
}

impl Viewport {
    pub fn current_tags(&self) -> u32 { self.tagset[self.seltags] }
}

// ── TaggedWindow ─────────────────────────────────────────────────────────────

pub struct TaggedWindow {
    pub window: Window,
    pub tags: u32,                          // bitmask
    pub position: Point<i32, Logical>,      // позиция в пространстве
    pub floating: bool,                     // не тайлить
}

// ── CursorMode ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CursorMode { Normal, Move, Resize, Pan }

// ── Dawn ─────────────────────────────────────────────────────────────────────

pub struct Dawn {
    pub start_time: std::time::Instant,
    pub socket_name: OsString,
    pub display_handle: DisplayHandle,
    pub space: Space<Window>,
    pub loop_signal: LoopSignal,
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
   pub dmabuf_state: DmabufState,
   pub dmabuf_global: Option<DmabufGlobal>,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Dawn>,
    pub data_device_state: DataDeviceState,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    // dawn state
    pub viewport: Viewport,
    pub cursor_mode: CursorMode,
    pub pointer_location: Point<f64, Logical>,
    pub cursor_status: CursorImageStatus,
    pub tagged_windows: Vec<TaggedWindow>,
    pub libinput_handle: Option<smithay::reexports::input::Libinput>,
   pub logo_held: bool,
    pub udev_devices: HashMap<smithay::backend::drm::DrmNode, crate::udev::Device>,
    pub tile_config: crate::tiling::TileConfig,
}

impl Dawn {
    pub fn new(event_loop: &mut EventLoop<Self>, display: Display<Self>) -> Self {
        let dh = display.handle();
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "dawn");
        seat.add_keyboard(Default::default(), 200, 25).unwrap();
        seat.add_pointer();
        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();
        Self {
            start_time: std::time::Instant::now(),
            display_handle: dh,
            socket_name,
            space: Space::default(),
            loop_signal,
            compositor_state,
            xdg_shell_state,
            shm_state,
           dmabuf_state: DmabufState::new(),
           dmabuf_global: None,
            output_manager_state,
            seat_state,
            data_device_state,
            popups: PopupManager::default(),
            seat,
            viewport: Viewport::default(),
            cursor_mode: CursorMode::Normal,
            pointer_location: Point::from((0.0, 0.0)),
            cursor_status: CursorImageStatus::default_named(),
            tagged_windows: Vec::new(),
            libinput_handle: None,
           logo_held: false,
            udev_devices: HashMap::new(),
            tile_config: crate::tiling::TileConfig::default(),
        }
    }

    fn init_wayland_listener(display: Display<Dawn>, event_loop: &mut EventLoop<Self>) -> OsString {
        let listening_socket = ListeningSocketSource::new_auto().unwrap();
        let socket_name = listening_socket.socket_name().to_os_string();
        let loop_handle = event_loop.handle();
        loop_handle.insert_source(listening_socket, move |client_stream, _, state| {
            state.display_handle.insert_client(client_stream, Arc::new(ClientState::default())).unwrap();
        }).expect("Failed to init wayland socket");
        loop_handle.insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            |_, display, state| {
                unsafe { display.get_mut().dispatch_clients(state).unwrap(); }
                Ok(PostAction::Continue)
            },
        ).unwrap();
        socket_name
    }

    // ── Surface under pointer ────────────────────────────────────────────────

    /// Применяем camera к output — ВСЯ магия infinite canvas
    /// space.map_output(&output, camera) двигает весь viewport
    pub fn apply_camera(&mut self) {
        let cam_x = self.viewport.cam_x.round() as i32;
        let cam_y = self.viewport.cam_y.round() as i32;
        let zoom = self.viewport.zoom;
        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            // Zoom через fractional scale — правильный способ
            output.change_current_state(
                None,
                None,
                Some(smithay::output::Scale::Fractional(zoom)),
                None,
            );
            self.space.map_output(&output, (cam_x, cam_y));
        }
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        self.space.element_under(pos).and_then(|(window, location)| {
            window.surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
    }

    // ── Tag operations ───────────────────────────────────────────────────────

    /// Переключиться на тег (Super+N)
    pub fn view_tag(&mut self, tag: u32) {
        self.viewport.tagset[self.viewport.seltags] = tag;
        self.refresh_tags();
        tracing::info!("dawn: view_tag → {:#b}", tag);
    }

    /// Показать все теги (Super+0)
    pub fn view_all_tags(&mut self) {
        let all: u32 = !0;
        self.viewport.tagset[self.viewport.seltags] = all;
        self.refresh_tags();
        tracing::info!("dawn: view_all_tags");
    }

    /// Toggle тег в текущем представлении (Super+Ctrl+N)
    pub fn toggle_view(&mut self, tag: u32) {
        let new = self.viewport.current_tags() ^ tag;
        if new != 0 {
            self.viewport.tagset[self.viewport.seltags] = new;
            self.refresh_tags();
            tracing::info!("dawn: toggle_view → {:#b}", new);
        }
    }

    /// Назначить тег focused окну (Super+Shift+N)
    pub fn tag_window(&mut self, tag: u32) {
        if let Some(focused) = self.focused_window_surface() {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                tw.window.toplevel()
                    .map(|t| t.wl_surface() == &focused)
                    .unwrap_or(false)
            }) {
                tw.tags = tag;
                tracing::info!("dawn: tag_window → {:#b}", tag);
            }
        }
        self.refresh_tags();
    }

    /// Toggle тег на focused окне (Super+Ctrl+Shift+N)
    pub fn toggle_tag(&mut self, tag: u32) {
        if let Some(focused) = self.focused_window_surface() {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                tw.window.toplevel()
                    .map(|t| t.wl_surface() == &focused)
                    .unwrap_or(false)
            }) {
                let new = tw.tags ^ tag;
                if new != 0 {
                    tw.tags = new;
                    tracing::info!("dawn: toggle_tag → {:#b}", new);
                }
            }
        }
        self.refresh_tags();
    }

    /// Обновить space — показать только окна с видимыми тегами
    pub fn refresh_tags(&mut self) {
        let current = self.viewport.current_tags();

        // Сохраняем позиции перед unmapping
        for tw in &mut self.tagged_windows {
            if let Some(loc) = self.space.element_location(&tw.window) {
                tw.position = loc;
            }
        }

        // Убираем всё из space
        for tw in &self.tagged_windows {
            self.space.unmap_elem(&tw.window);
        }

        // Добавляем только видимые
        for tw in &self.tagged_windows {
            if tw.tags & current != 0 {
                self.space.map_element(tw.window.clone(), tw.position, false);
            }
        }
    }


    /// Получить focused surface (для tag_window/close)
    fn focused_window_surface(&self) -> Option<WlSurface> {
        self.seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
    }
}

// ── ClientState ──────────────────────────────────────────────────────────────

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
