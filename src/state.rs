use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};

use crate::anim::{CameraAnim, ZoomAnim};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::element::memory::MemoryRenderBuffer,
        session::libseat::LibSeatSession,
    },
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
    utils::{Logical, Physical, Point, Size, Transform},
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
    pub tags: u32,                           // bitmask
    pub position: Point<i32, Logical>,       // текущая позиция (tile или float)
    pub float_position: Point<i32, Logical>, // позиция в Float-режиме (сохраняется)
    pub float_size: Option<Size<i32, Logical>>, // размер в Float-режиме (None = клиент выбирает)
    pub float_position_set: bool,            // пользователь вручную размещал в Float
    pub floating: bool,                      // не тайлить
    pub folded: bool,                        // схлопнуто в стопку (2.4)
}

// ── Portal (4.4) ─────────────────────────────────────────────────────────────

/// Зеркальный портал: живая копия удалённого окна, закреплённая в фиксированной
/// точке экрана. Клик/движение мыши внутри рамки портала перенаправляются на
/// оригинальную поверхность (см. Dawn::surface_under).
pub struct Portal {
    pub surface: WlSurface,
    pub screen_pos: Point<i32, Physical>,
    pub box_size: Size<i32, Physical>,
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
   pub pinch_last_scale: f64,
   pub pan_button_held: bool,
    pub udev_devices: HashMap<smithay::backend::drm::DrmNode, crate::udev::Device>,
    pub tile_config: crate::tiling::TileConfig,
    pub cursor_default_buffer: Option<MemoryRenderBuffer>,
    pub cursor_default_hotspot: Point<i32, Logical>,
    pub cursor_default_size: Size<i32, Logical>,
    pub session: Option<LibSeatSession>,
    /// Сколько ещё кадров подряд форсировать reset_buffer_ages() после
    /// структурного изменения (смена тега, arrange, начало/конец драга и
    /// т.п.). Один сброс покрывает только ОДИН буфер из DRM swap chain — при
    /// двойной/тройной буферизации "тень" предыдущего кадра иначе остаётся в
    /// ещё не отрисованных буферах ещё несколько кадров. См. request_plane_reset().
    pub plane_reset_frames: u8,
    // ── Анимация (Module 1) ──────────────────────────────────────────────────
    pub momentum: crate::canvas::MomentumState,
    pub camera_anim: Option<crate::anim::CameraAnim>,
    pub zoom_anim: Option<crate::anim::ZoomAnim>,
    /// true пока Super+Space зажат (bird's-eye view, устар. — см. zoom_nav_mode)
    pub bird_eye_active: bool,
    /// Режим лупы (Super+Space, тумблер): зум к центру экрана, навигация
    /// стрелками, повторный Super+Space сбрасывает. См. enter/exit_zoom_nav.
    pub zoom_nav_mode: bool,
    /// Закладки камеры: слот 1-9 → позиция camera (cam_x, cam_y)
    pub camera_bookmarks: HashMap<u32, Point<f64, Logical>>,
    /// Режим закладок камеры (Super+M toggle): true → Super+[1-9] управляют
    /// закладками камеры вместо тегов/воркспейсов
    pub bookmarks_mode: bool,
    /// Теги (воркспейсы), на которые уже переключались хотя бы раз. Первый
    /// вход в воркспейс включает Tile, повторные — не трогают layout (см.
    /// view_tag).
    pub visited_tags: std::collections::HashSet<u32>,
    // ── Module 2 ──────────────────────────────────────────────────────────
    /// Магнитирование окон при перетаскивании (Super+S toggle)
    pub is_snapping_enabled: bool,
    // ── Module 3 ──────────────────────────────────────────────────────────
    /// Оверлей миникарты (Super+` toggle)
    pub is_minimap_visible: bool,
    // ── Module 4 ──────────────────────────────────────────────────────────
    /// Загруженная при старте топология (4.3): app_id → очередь позиций,
    /// расходуется в XdgShellHandler::app_id_changed по мере появления окон.
    pub pending_session: HashMap<String, Vec<Point<i32, Logical>>>,
    /// Активный оконный портал (4.4), не более одного одновременно.
    pub portal: Option<Portal>,
    /// Позиция камеры для каждого воркспейса (тега) — восстанавливается при
    /// переключении на него, сохраняется при уходе (см. view_tag).
    pub tag_cameras: HashMap<u32, (f64, f64)>,
    // ── Module 5 ──────────────────────────────────────────────────────────
    /// Focus Aura (5.3): последняя цель (позиция, размер) — для детекта смены фокуса.
    pub focus_aura_target: Option<(Point<f64, Logical>, (f64, f64))>,
    /// Focus Aura: текущая интерполированная (позиция, размер) для отрисовки.
    pub focus_aura_current: Option<(Point<f64, Logical>, (f64, f64))>,
    pub focus_aura_anim: Option<crate::anim::RectAnim>,
    /// Анимации позиций окон при переходах tiling/floating (разлёт/сборка) —
    /// (окно, LERP из старой позиции в новую).
    pub window_pos_anims: Vec<(Window, CameraAnim)>,
    /// Анимации появления новых окон "с ростом" (Float-режим).
    pub window_open_anims: Vec<(Window, crate::anim::OpenAnim)>,
    /// Super+2-палец swipe → перемещение окна (новый жест, под курсором на начале).
    pub gesture_move_window: Option<Window>,
    /// Super+pinch → resize окна (новый жест, под курсором на начале).
    pub gesture_resize_window: Option<Window>,
    /// Копится вместо немедленного render_all() на каждый чих (клавиша,
    /// motion, commit) — фактический рендер выполняется один раз после
    /// event_loop.dispatch() в main.rs, схлопывая пачку событий одного тика
    /// в один проход по CRTC вместо N (см. request_redraw()).
    pub needs_redraw: bool,
    pub lua_config: crate::config::Config,
    /// Кэш 1×N буферов для скруглённых углов (см. build_corner_mask_elements
    /// в udev.rs), ключ — ширина среза в физических пикселях. Не больше ~24
    /// записей (радиус clamp(2,24)), никогда не очищается.
    pub corner_mask_cache: HashMap<i32, smithay::backend::renderer::element::solid::SolidColorBuffer>,
    // ── Мультивыделение / "созвездия" ────────────────────────────────────────
    /// Рамка rubber-band выделения в процессе протяжки (canvas-координаты) —
    /// см. grabs/select_grab.rs.
    pub selection_drag: Option<smithay::utils::Rectangle<i32, Logical>>,
    /// Текущее выделение окон (Super+клик по пустому месту + протяжка).
    pub selected_windows: Vec<Window>,
    /// Группы окон ("созвездия", Super+G): двигаются/ресайзятся как единое
    /// целое в Float-режиме — см. selection.rs.
    pub constellations: Vec<Vec<Window>>,
    /// niri-подобная модель колонок для Layout::Columns (см. columns.rs).
    pub columns: crate::columns::ColumnLayout,
    /// Обзор рабочих столов активен (тап Super, см. overview.rs).
    pub overview_active: bool,
    /// Кандидат на "тап Super": true с нажатия Super, сбрасывается любым другим
    /// вводом; если дожил до отпускания Super — это тап → toggle_overview.
    pub super_tap: bool,
    /// Сохранённое состояние до входа в обзор: (тег, cam_x, cam_y, zoom, layout).
    pub overview_prev: Option<(u32, f64, f64, f64, crate::tiling::Layout)>,
    /// Занятые столы в обзоре (маски тегов).
    pub overview_order: Vec<u32>,
    /// Позиции столов в 2D-сетке обзора: маска тега → (col, row) относительно
    /// центрального (0,0). Сбрасывается при каждом входе в обзор.
    pub overview_slots: std::collections::HashMap<u32, (i32, i32)>,
    /// Стол, который сейчас тащим мышью в обзоре (ЛКМ по столу).
    pub overview_drag_ws: Option<u32>,
    /// Плавный выход из обзора: true → анимация запущена, после завершения
    /// (camera_anim+zoom_anim оба None) anim::tick финализирует (restore layout).
    pub overview_exit_pending: bool,
    /// Если Some — после завершения exit-анимации переключиться на этот стол.
    pub overview_exit_target_ws: Option<u32>,
    /// Окно, которое таскает жест Super+2-пальца (тачпад-скролл source=Finger):
    /// латчится на первом кадре жеста и едет, пока пальцы не отпущены, даже
    /// если "выскользнуло" из-под неподвижного курсора (см. input.rs).
    pub touchpad_move_window: Option<Window>,
    /// Layout до входа в niri-режим (Columns). При повторном Win+N восстановить
    /// его, а не всегда Tile.
    pub prev_layout_before_niri: crate::tiling::Layout,
}

fn load_default_cursor() -> (Option<MemoryRenderBuffer>, Point<i32, Logical>, Size<i32, Logical>) {
    use xcursor::{CursorTheme, parser::parse_xcursor};

    let try_load = || -> Option<(MemoryRenderBuffer, Point<i32, Logical>, Size<i32, Logical>)> {
        let theme = CursorTheme::load("default");
        let path = theme.load_icon("left_ptr")
            .or_else(|| theme.load_icon("default"))
            .or_else(|| theme.load_icon("arrow"))?;
        let bytes = std::fs::read(path).ok()?;
        let images = parse_xcursor(&bytes)?;
        let image = images.iter().min_by_key(|i| (i.size as i32 - 24).abs())?;
        let buf = MemoryRenderBuffer::from_slice(
            &image.pixels_rgba,
            Fourcc::Abgr8888,
            (image.width as i32, image.height as i32),
            1,
            Transform::Normal,
            None,
        );
        let hotspot = Point::from((image.xhot as i32, image.yhot as i32));
        let size = Size::from((image.width as i32, image.height as i32));
        Some((buf, hotspot, size))
    };

    match try_load() {
        Some((buf, hs, sz)) => {
            tracing::info!("dawn: loaded default cursor {}x{}", sz.w, sz.h);
            (Some(buf), hs, sz)
        }
        None => {
            tracing::warn!("dawn: could not load xcursor 'left_ptr', cursor will be invisible");
            (None, Point::from((0, 0)), Size::from((24, 24)))
        }
    }
}

impl Dawn {
    pub fn new(event_loop: &mut EventLoop<Self>, display: Display<Self>) -> Self {
        let dh = display.handle();
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        let lua_config = crate::config::Config::load();
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "dawn");
        seat.add_keyboard(lua_config.xkb_config(), 200, 25).unwrap();
        seat.add_pointer();
        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();
        let (cursor_default_buffer, cursor_default_hotspot, cursor_default_size) = load_default_cursor();
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
           pinch_last_scale: 1.0,
           pan_button_held: false,
            udev_devices: HashMap::new(),
            tile_config: crate::tiling::TileConfig::default(),
            cursor_default_buffer,
            cursor_default_hotspot,
            cursor_default_size,
            session: None,
            plane_reset_frames: 0,
            momentum: crate::canvas::MomentumState::new(0.5),
            camera_anim: None,
            zoom_anim: None,
            bird_eye_active: false,
            zoom_nav_mode: false,
            camera_bookmarks: HashMap::new(),
            bookmarks_mode: false,
            // Начальный воркспейс (tag 1) считается уже посещённым — при
            // возврате на него после ухода layout не сбрасывается.
            visited_tags: std::collections::HashSet::from([1u32]),
            is_snapping_enabled: false,
            is_minimap_visible: false,
            pending_session: crate::session::load(),
            portal: None,
            tag_cameras: HashMap::new(),
            focus_aura_target: None,
            focus_aura_current: None,
            focus_aura_anim: None,
            window_pos_anims: Vec::new(),
            window_open_anims: Vec::new(),
            gesture_move_window: None,
            gesture_resize_window: None,
            needs_redraw: true,
            lua_config,
            selection_drag: None,
            selected_windows: Vec::new(),
            constellations: Vec::new(),
            columns: crate::columns::ColumnLayout::default(),
            overview_active: false,
            super_tap: false,
            overview_prev: None,
            overview_order: Vec::new(),
            overview_slots: HashMap::new(),
            overview_drag_ws: None,
            overview_exit_pending: false,
            overview_exit_target_ws: None,
            touchpad_move_window: None,
            prev_layout_before_niri: crate::tiling::Layout::Tile,
            corner_mask_cache: HashMap::new(),
        }
    }

    /// Помечает кадр "грязным" вместо немедленного render_all() — см. поле
    /// needs_redraw. Дешёвая запись одного bool на каждое событие ввода
    /// вместо полного прохода по всем CRTC/окнам/corner-mask буферам.
    pub fn request_redraw(&mut self) {
        self.needs_redraw = true;
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

    /// Убирает все анимации, ссылающиеся на уничтоженную поверхность —
    /// иначе anim::tick() продолжит слать configure/map_element мёртвому окну.
    pub fn cancel_window_anims(&mut self, surface: &WlSurface) {
        self.window_pos_anims.retain(|(w, _)| {
            w.toplevel().map(|t| t.wl_surface() != surface).unwrap_or(true)
        });
        self.window_open_anims.retain(|(w, _)| {
            w.toplevel().map(|t| t.wl_surface() != surface).unwrap_or(true)
        });
    }

    /// Запросить принудительный полный редрав на несколько следующих кадров
    /// (не один!) — один reset_buffer_ages() покрывает только один буфер в
    /// DRM swap chain, при двойной/тройной буферизации "тень" предыдущего
    /// содержимого иначе остаётся ещё 1-2 кадра в необновлённых буферах.
    pub fn request_plane_reset(&mut self) {
        self.plane_reset_frames = 4;
    }

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
        if let Some(portal) = &self.portal {
            if let Some(hit) = self.portal_hit_test(portal, pos) {
                return Some(hit);
            }
        }
        self.space.element_under(pos).and_then(|(window, location)| {
            window.surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
    }

    /// Проверяет попадание курсора в рамку портала (4.4) и, если попал,
    /// пересчитывает координаты в surface-локальные оригинального окна.
    /// Возвращает (surface, "виртуальный origin") — origin подобран так,
    /// чтобы `pos - origin` дало правильную surface-локальную точку (тот же
    /// контракт, что и у обычной ветки surface_under).
    fn portal_hit_test(
        &self,
        portal: &Portal,
        pos: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let zoom = self.viewport.zoom;
        let screen_x = (pos.x - self.viewport.cam_x) * zoom;
        let screen_y = (pos.y - self.viewport.cam_y) * zoom;
        let local_x = screen_x - portal.screen_pos.x as f64;
        let local_y = screen_y - portal.screen_pos.y as f64;
        if local_x < 0.0 || local_y < 0.0
            || local_x > portal.box_size.w as f64 || local_y > portal.box_size.h as f64
        {
            return None;
        }

        let window = self.tagged_windows.iter()
            .find(|tw| tw.window.toplevel().map(|t| t.wl_surface() == &portal.surface).unwrap_or(false))
            .map(|tw| &tw.window)?;
        let geo = self.space.element_geometry(window)?;

        let frac_x = local_x / portal.box_size.w as f64;
        let frac_y = local_y / portal.box_size.h as f64;
        let surface_local = Point::from((frac_x * geo.size.w as f64, frac_y * geo.size.h as f64));

        window.surface_under(surface_local, WindowSurfaceType::ALL)?;
        let origin = pos - surface_local;
        Some((portal.surface.clone(), origin))
    }

    /// Super+P (4.4): открыть портал из сфокусированного окна, либо закрыть
    /// уже открытый.
    pub fn toggle_portal(&mut self) {
        if self.portal.is_some() {
            self.portal = None;
            tracing::info!("dawn: portal closed");
            self.request_redraw();
            return;
        }
        let focused = match self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            Some(f) => f,
            None => return,
        };
        const PORTAL_W: i32 = 320;
        const PORTAL_H: i32 = 240;
        const MARGIN: i32 = 20;
        let output = match self.space.outputs().next() { Some(o) => o.clone(), None => return };
        let mode = match output.current_mode() { Some(m) => m, None => return };
        self.portal = Some(Portal {
            surface: focused,
            screen_pos: Point::from((mode.size.w - PORTAL_W - MARGIN, MARGIN)),
            box_size: Size::from((PORTAL_W, PORTAL_H)),
        });
        tracing::info!("dawn: portal opened");
        self.request_redraw();
    }

    /// Режим коллизии (Super+S, is_snapping_enabled): расталкивает окна,
    /// которые перекрываются с только что сдвинутым `mover`. Толкаем по оси
    /// наименьшего перекрытия, в сторону от центра mover'а, с зазором. Один
    /// проход (без каскада) — визуально достаточно для "толкания".
    pub fn push_colliding_windows(&mut self, mover: &Window) {
        if !self.is_snapping_enabled {
            return;
        }
        let mover_geo = match self.space.element_geometry(mover) {
            Some(g) => g,
            None => return,
        };
        let current = self.viewport.current_tags();
        let same = |a: &Window, b: &Window| {
            a.toplevel().zip(b.toplevel())
                .map(|(x, y)| x.wl_surface() == y.wl_surface())
                .unwrap_or(false)
        };
        let others: Vec<Window> = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current != 0)
            .map(|tw| tw.window.clone())
            .filter(|w| !same(w, mover))
            .collect();

        const MARGIN: i32 = 8;
        let mcx = mover_geo.loc.x + mover_geo.size.w / 2;
        let mcy = mover_geo.loc.y + mover_geo.size.h / 2;
        for w in others {
            let g = match self.space.element_geometry(&w) {
                Some(g) => g,
                None => continue,
            };
            let inter = match mover_geo.intersection(g) {
                Some(i) => i,
                None => continue,
            };
            // Толкаем по оси наименьшего перекрытия — так окно "выскальзывает"
            // в ближайшую свободную сторону.
            let (mut nx, mut ny) = (g.loc.x, g.loc.y);
            if inter.size.w <= inter.size.h {
                nx = if g.loc.x + g.size.w / 2 >= mcx {
                    mover_geo.loc.x + mover_geo.size.w + MARGIN
                } else {
                    mover_geo.loc.x - g.size.w - MARGIN
                };
            } else {
                ny = if g.loc.y + g.size.h / 2 >= mcy {
                    mover_geo.loc.y + mover_geo.size.h + MARGIN
                } else {
                    mover_geo.loc.y - g.size.h - MARGIN
                };
            }
            let new_loc = Point::from((nx, ny));
            self.space.map_element(w.clone(), new_loc, false);
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| same(&tw.window, &w)) {
                tw.float_position = new_loc;
                tw.position = new_loc;
                tw.float_position_set = true;
                tw.floating = true;
            }
        }
    }

    // ── Tag operations ───────────────────────────────────────────────────────

    /// Переключиться на тег (Super+N)
    pub fn view_tag(&mut self, tag: u32) {
        // У каждого воркспейса своя позиция камеры — запоминаем текущую перед
        // уходом и восстанавливаем сохранённую (если была) для нового тега.
        let old_tag = self.viewport.current_tags();
        self.tag_cameras.insert(old_tag, (self.viewport.cam_x, self.viewport.cam_y));

        self.viewport.tagset[self.viewport.seltags] = tag;
        self.refresh_tags();

        // НОВЫЙ (ещё не посещённый) воркспейс → включаем tiling и раскладываем
        // от (0,0). Повторные заходы layout/камеру не трогают (восстанавливаем
        // сохранённую позицию камеры ниже).
        let is_new = !self.visited_tags.contains(&tag);
        self.visited_tags.insert(tag);

        // niri-режим (Columns): воркспейс меняется, но layout остаётся Columns —
        // перекладываем колонки под новый тег и подъезжаем к активной колонке.
        if self.tile_config.layout == crate::tiling::Layout::Columns {
            self.arrange();
            self.columns_set_active_to_focus();
            self.columns_scroll_to_active();
            tracing::info!("dawn: view_tag → {:#b} (columns workspace)", tag);
            return;
        }

        if is_new {
            self.set_layout(crate::tiling::Layout::Tile); // сам обнуляет камеру
            tracing::info!("dawn: view_tag → {:#b} (new workspace → tiling)", tag);
            return;
        }

        // Плавный "перелёт" камеры между воркспейсами вместо мгновенного прыжка.
        if let Some(&(x, y)) = self.tag_cameras.get(&tag) {
            let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
            let to = Point::from((x, y));
            if (to.x - from.x).abs() > 0.5 || (to.y - from.y).abs() > 0.5 {
                self.camera_anim = Some(CameraAnim::new(from, to, Duration::from_millis(300)));
            } else {
                self.viewport.cam_x = x;
                self.viewport.cam_y = y;
                self.apply_camera();
            }
        }
        tracing::info!("dawn: view_tag → {:#b}", tag);
    }

    /// niri-воркспейсы: перейти на пред/след воркспейс (тег idx±1, clamp 1-9).
    /// Работает в любом режиме; в Columns остаётся Columns (см. view_tag).
    pub fn workspace_step(&mut self, dir: i32) {
        let idx = self.viewport.current_tags().trailing_zeros() as i32 + 1;
        let new = (idx + dir).clamp(1, 9);
        if new == idx { return; }
        self.view_tag(1u32 << (new - 1));
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

        // Без этого при переключении тега на экране остаётся "тень" —
        // предыдущий набор окон закэширован в DRM plane и не перерисовывается
        // заново (тот же баг, что чинили для arrange(), см. память проекта).
        self.request_plane_reset();
        // ВАЖНО: request_plane_reset() сам по себе не планирует кадр — main.rs
        // рендерит только когда needs_redraw=true (см. request_redraw()).
        // Без этого вызова смена тега не перерисовывалась немедленно: экран
        // обновлялся только на следующий heartbeat (~500мс) или случайный
        // редрав от другого события — отсюда "тень" старого фокуса и
        // недорисованные углы/края, которые "доедались" только при панорамировании
        // (частые кадры от пана быстро съедали plane_reset_frames).
        self.request_redraw();
    }


    // ── Hold-to-zoom / bird's-eye (1.3) ──────────────────────────────────────

    pub(crate) fn screen_center_and_anchor(&self) -> Option<(Point<f64, Logical>, Point<f64, Logical>)> {
        let output = self.space.outputs().next()?;
        let out_geo = self.space.output_geometry(output)?;
        let screen_center = Point::from((out_geo.size.w as f64 / 2.0, out_geo.size.h as f64 / 2.0));
        let anchor_canvas = Point::from((
            self.viewport.cam_x + screen_center.x / self.viewport.zoom,
            self.viewport.cam_y + screen_center.y / self.viewport.zoom,
        ));
        Some((anchor_canvas, screen_center))
    }

    pub fn start_bird_eye(&mut self) {
        self.bird_eye_active = true;
        if let Some((anchor_canvas, screen_center)) = self.screen_center_and_anchor() {
            self.zoom_anim = Some(ZoomAnim::new(
                anchor_canvas, screen_center, self.viewport.zoom, 0.6, Duration::from_millis(250),
            ));
        }
        tracing::info!("dawn: bird's-eye on");
    }

    pub fn end_bird_eye(&mut self) {
        self.bird_eye_active = false;
        if let Some((anchor_canvas, screen_center)) = self.screen_center_and_anchor() {
            self.zoom_anim = Some(ZoomAnim::new(
                anchor_canvas, screen_center, self.viewport.zoom, 1.0, Duration::from_millis(250),
            ));
        }
        tracing::info!("dawn: bird's-eye off");
    }

    // ── Режим обзора (Super+Space, тумблер) ──────────────────────────────────
    /// Уровень зума в режиме обзора: ОТДАЛЕНИЕ к центру экрана (обзор сверху).
    /// 0.2 ≈ область в 5× viewport; можно уменьшить для ещё большего отдаления.
    pub const ZOOM_NAV_LEVEL: f64 = 0.2;
    /// Шаг панорамирования стрелками в режиме обзора (экранные px за нажатие).
    pub const ZOOM_NAV_PAN_STEP: f64 = 220.0;

    /// Super+Space: включить/выключить режим лупы. Вкл — плавный зум к
    /// `ZOOM_NAV_LEVEL` с якорем в центре экрана; выкл — обратно к zoom=1.
    pub fn toggle_zoom_nav(&mut self) {
        if self.zoom_nav_mode {
            self.zoom_nav_mode = false;
            if let Some((anchor_canvas, screen_center)) = self.screen_center_and_anchor() {
                self.zoom_anim = Some(ZoomAnim::new(
                    anchor_canvas, screen_center, self.viewport.zoom, 1.0, Duration::from_millis(220),
                ));
            }
            tracing::info!("dawn: zoom-nav off");
        } else {
            self.zoom_nav_mode = true;
            self.momentum.stop();
            if let Some((anchor_canvas, screen_center)) = self.screen_center_and_anchor() {
                self.zoom_anim = Some(ZoomAnim::new(
                    anchor_canvas, screen_center, self.viewport.zoom, Self::ZOOM_NAV_LEVEL,
                    Duration::from_millis(220),
                ));
            }
            tracing::info!("dawn: zoom-nav on");
        }
        self.request_redraw();
    }

    /// Панорамирование камеры стрелками в режиме обзора (dx,dy — направление).
    /// Плавно, с "инерцией": каждое нажатие — ease-out CameraAnim, затухающий
    /// после отпускания. Автоповтор клавиши складывает цели в непрерывный глайд
    /// (цель отсчитывается от текущей ЦЕЛИ анимации, старт — от реальной cam).
    pub fn zoom_nav_pan(&mut self, dx: f64, dy: f64) {
        self.zoom_anim = None; // фиксируем zoom, чтобы якорный zoom_anim не перебил cam
        let zoom = self.viewport.zoom.max(0.01);
        let step = Self::ZOOM_NAV_PAN_STEP / zoom;
        let base = self.camera_anim.as_ref()
            .map(|a| a.to)
            .unwrap_or_else(|| Point::from((self.viewport.cam_x, self.viewport.cam_y)));
        let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let to = Point::from((base.x + dx * step, base.y + dy * step));
        self.camera_anim = Some(CameraAnim::new(from, to, Duration::from_millis(360)));
        self.request_redraw();
    }

    // ── Миникарта (3.3) ───────────────────────────────────────────────────────

    /// Если сейчас видима миникарта и клик попал в её панель — находит окно
    /// под точкой клика (по canvas-координатам) и телепортирует камеру + курсор
    /// к его центру. Если клик пришёлся на пустое место — центрирует камеру
    /// на этой точке. Возвращает true (клик съеден, не должен доходить до окон).
    pub fn try_handle_minimap_click(&mut self) -> bool {
        if !self.is_minimap_visible {
            return false;
        }
        let output = match self.space.outputs().next() { Some(o) => o.clone(), None => return false };
        let mode = match output.current_mode() { Some(m) => m, None => return false };

        let zoom = self.viewport.zoom;
        // Позиция курсора в screen-координатах (физических)
        let screen_logical_x = self.pointer_location.x - self.viewport.cam_x;
        let screen_logical_y = self.pointer_location.y - self.viewport.cam_y;
        let screen_physical_x = screen_logical_x * zoom;
        let screen_physical_y = screen_logical_y * zoom;

        // Проверяем попадание в панель миникарты
        let origin = crate::canvas::minimap_panel_origin(mode.size);
        let click_px = screen_physical_x - origin.x as f64;
        let click_py = screen_physical_y - origin.y as f64;
        if click_px < 0.0 || click_py < 0.0
            || click_px > crate::canvas::MINIMAP_PANEL_W as f64
            || click_py > crate::canvas::MINIMAP_PANEL_H as f64
        {
            return false;
        }

        // Собираем окна текущего тега (те же, что и в рендере)
        let current_tags = self.viewport.current_tags();
        let windows: Vec<(Point<i32, Logical>, Size<i32, Logical>, bool)> = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .filter_map(|tw| self.space.element_geometry(&tw.window).map(|g| (g.loc, g.size, false)))
            .collect();

        // Та же проекция, что и в build_minimap_elements
        let proj = crate::canvas::project_minimap(&windows);

        // Клик в панели → canvas-координаты
        let click_in_panel = smithay::utils::Point::<f64, smithay::utils::Physical>::from((click_px, click_py));
        let canvas_point = crate::canvas::minimap_click_to_canvas(click_in_panel, proj.bbox, proj.scale);

        // Ищем окно, в которое попал клик (по canvas-координатам)
        let target = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .filter_map(|tw| self.space.element_geometry(&tw.window).map(|g| (tw.window.clone(), g)))
            .find(|(_, g)| {
                canvas_point.x >= g.loc.x as f64
                    && canvas_point.x <= (g.loc.x + g.size.w) as f64
                    && canvas_point.y >= g.loc.y as f64
                    && canvas_point.y <= (g.loc.y + g.size.h) as f64
            })
            .map(|(w, g)| {
                // Центр окна — туда полетит камера и курсор
                let cx = g.loc.x as f64 + g.size.w as f64 / 2.0;
                let cy = g.loc.y as f64 + g.size.h as f64 / 2.0;
                (w, Point::from((cx, cy)))
            });

        let (target_point, cam_target) = if let Some((_window, center)) = target {
            // Кликнули по окну — летим к его центру
            let cam: Point<f64, Logical> = Point::from((
                center.x - mode.size.w as f64 / (2.0 * zoom),
                center.y - mode.size.h as f64 / (2.0 * zoom),
            ));
            (center, cam)
        } else {
            // Кликнули по пустому месту — центрируем на точке клика
            let cam: Point<f64, Logical> = Point::from((
                canvas_point.x - mode.size.w as f64 / (2.0 * zoom),
                canvas_point.y - mode.size.h as f64 / (2.0 * zoom),
            ));
            (canvas_point, cam)
        };

        // Мгновенный телепорт (без анимации — курсор уже стоит на панели,
        // хочется чтобы реакция была моментальной)
        self.camera_anim = None;
        self.viewport.cam_x = cam_target.x;
        self.viewport.cam_y = cam_target.y;
        self.apply_camera();
        self.pointer_location = target_point;
        self.request_redraw();
        tracing::info!("dawn: minimap click → teleport to ({:.0},{:.0})", target_point.x, target_point.y);
        true
    }

    // ── Пространственные закладки камеры (1.5) ───────────────────────────────

    /// Закладки хранят "якорь" — canvas-точку, которую прыжок помещает в центр
    /// экрана. По умолчанию закладок нет (camera_bookmarks пуст) — их надо
    /// сначала закрепить (Alt+B на позиции курсора или Super+Shift+N в
    /// bookmarks-режиме на центре экрана).
    fn bookmark_anchor_for_screen_center(&self) -> Point<f64, Logical> {
        let (w, h) = self.space.outputs().next()
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| (g.size.w as f64, g.size.h as f64))
            .unwrap_or((0.0, 0.0));
        Point::from((
            self.viewport.cam_x + w / (2.0 * self.viewport.zoom),
            self.viewport.cam_y + h / (2.0 * self.viewport.zoom),
        ))
    }

    pub fn save_camera_bookmark(&mut self, slot: u32) {
        let anchor = self.bookmark_anchor_for_screen_center();
        self.camera_bookmarks.insert(slot, anchor);
        tracing::info!("dawn: camera bookmark {} saved (center)", slot);
    }

    /// Alt+B: закрепить закладку камеры на текущей позиции курсора, в наименьший
    /// свободный слот 1-9 (при полном наборе перезаписываем слот 1 по кругу).
    pub fn pin_bookmark_at_cursor(&mut self) {
        let anchor = self.pointer_location;
        let slot = (1u32..=9).find(|s| !self.camera_bookmarks.contains_key(s))
            .unwrap_or(((self.camera_bookmarks.len() as u32) % 9) + 1);
        self.camera_bookmarks.insert(slot, anchor);
        tracing::info!("dawn: camera bookmark {} pinned at cursor ({:.0},{:.0})", slot, anchor.x, anchor.y);
    }

    pub fn jump_to_camera_bookmark(&mut self, slot: u32) {
        if let Some(&anchor) = self.camera_bookmarks.get(&slot) {
            let (w, h) = self.space.outputs().next()
                .and_then(|o| self.space.output_geometry(o))
                .map(|g| (g.size.w as f64, g.size.h as f64))
                .unwrap_or((0.0, 0.0));
            // Центрируем камеру на якоре закладки.
            let target = Point::from((
                anchor.x - w / (2.0 * self.viewport.zoom),
                anchor.y - h / (2.0 * self.viewport.zoom),
            ));
            let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
            self.camera_anim = Some(CameraAnim::new(from, target, Duration::from_millis(320)));
            tracing::info!("dawn: jump to camera bookmark {}", slot);
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
