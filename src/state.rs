use std::{collections::HashMap, ffi::OsString, sync::Arc, time::Duration};

use crate::anim::{CameraAnim, ZoomAnim};
use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::element::memory::MemoryRenderBuffer,
        session::libseat::LibSeatSession,
    },
    desktop::{PopupManager, Space, Window, WindowSurfaceType, layer_map_for_output},
    input::{Seat, SeatState},
    reexports::{
        calloop::{EventLoop, Interest, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    input::pointer::{CursorIcon, CursorImageStatus},
    utils::{Logical, Physical, Point, Rectangle, Size, Transform},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        cursor_shape::CursorShapeManagerState,
        dmabuf::{DmabufGlobal, DmabufState},
        output::OutputManagerState,
        pointer_constraints::PointerConstraintsState,
        relative_pointer::RelativePointerManagerState,
        selection::data_device::DataDeviceState,
        selection::wlr_data_control::DataControlState,
        shell::xdg::{XdgShellState, decoration::XdgDecorationState},
        shell::wlr_layer::WlrLayerShellState,
        viewporter::ViewporterState,
        shell::wlr_layer::Layer as WlrLayer,
        shm::ShmState,
        socket::ListeningSocketSource,
        xwayland_shell::XWaylandShellState,
        image_capture_source::{ImageCaptureSourceState, OutputCaptureSourceState},
        image_copy_capture::{ImageCopyCaptureState, Session},
    },
    xwayland::X11Wm,
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
    /// Плавающее НАМЕРЕННО: X11-диалог или явный toggle_floating. Такое окно
    /// переживает переход Float→tiling и остаётся поверх раскладки. Всё
    /// остальное, что стало floating попутно (окно подвинули мышью/жестом во
    /// Float — см. input.rs), при сборке в тайлинг обязано вернуться в
    /// раскладку: иначе «в tiling переносятся не все окна».
    pub float_pinned: bool,
    pub folded: bool,                        // схлопнуто в стопку (2.4)
    /// Где окно стояло ДО того, как его собрали в созвездие.
    ///
    /// Созвездие — операция обратимая: разобрал — окна обязаны вернуться туда,
    /// откуда их собрали, а не разлететься куда попало. Пишется один раз, при
    /// сборке (повторная сборка уже собранного не затирает исходное место), и
    /// снимается при разборке. См. selection.rs.
    pub pre_constellation: Option<Point<i32, Logical>>,
    /// Созвездие, в котором состоит окно, «растащено»: одно из его окон увели
    /// руками (драг мышью или Super+стрелки), и взаимное расположение, ради
    /// которого созвездие и заводили, нарушено.
    ///
    /// По этой метке Super+D решает, что делать с выделенным созвездием:
    /// растащенное — собрать заново, целое — разобрать. Раньше это решалось
    /// геометрией (сложены ли окна вплотную), и метрика ошибалась ровно там,
    /// где окна никто не двигал: стоило поменять размер ОДНОГО окна, как между
    /// ним и соседом появлялась дыра, гроздь считалась раскиданной и Super+D
    /// вместо разборки собирал её заново — «созвездие не разбирается обратно».
    /// Метка ставится по ДЕЙСТВИЮ, а не по картинке, и ресайз ей не мешает.
    /// См. selection.rs.
    pub constellation_torn: bool,
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
    /// xdg-decoration: без этого глобала GTK-клиенты рисуют свой заголовок и
    /// получают из-за него большой минимальный размер (см. xdg_shell.rs).
    pub xdg_decoration_state: XdgDecorationState,
    /// Выход-«призрак» для layer-поверхностей: масштаб всегда 1, глобала нет.
    /// Нужен, чтобы обои и панели считались в экранных пикселях и не зависели
    /// от зума холста (см. scan_connectors в udev.rs).
    pub layer_output: Option<smithay::output::Output>,
    /// Слой, которому сейчас отдана клавиатура (лаунчер). Нужен, чтобы вернуть
    /// фокус окну, когда слой закроется.
    pub layer_keyboard: Option<WlSurface>,
    pub shm_state: ShmState,
   pub dmabuf_state: DmabufState,
   pub dmabuf_global: Option<DmabufGlobal>,
    pub output_manager_state: OutputManagerState,
    pub seat_state: SeatState<Dawn>,
    pub data_device_state: DataDeviceState,
    /// wlr-data-control: менеджеры буфера обмена (cliphist за Super+C) читают
    /// и пишут буфер, не имея фокуса. См. handlers::DataControlHandler.
    pub data_control_state: DataControlState,
    /// Протокол xwayland_shell — по нему XWayland связывает свои wl_surface с
    /// X11-окнами (см. xwayland.rs).
    pub xwayland_shell_state: XWaylandShellState,
    /// Оконный менеджер X11. None пока Xwayland не поднялся (или если он
    /// отвалился) — тогда dawn просто чисто wayland-компоситор.
    pub xwm: Option<X11Wm>,
    /// Номер X-дисплея (`DISPLAY=:N`) поднятого Xwayland.
    pub xdisplay: Option<u32>,
    pub popups: PopupManager,
    pub seat: Seat<Self>,
    // dawn state
    pub viewport: Viewport,
    pub cursor_mode: CursorMode,
    pub pointer_location: Point<f64, Logical>,
    /// Экранная (физическая) позиция курсора — то, где пользователь видит
    /// стрелку. Ведущая величина при движении камеры: canvas-позиция
    /// пересчитывается из неё, а не наоборот. См. sync_pointer_to_camera.
    pub pointer_screen: Point<f64, Physical>,
    /// Камера (cam_x, cam_y, zoom), при которой pointer_screen был посчитан.
    /// Расхождение с текущей = камера уехала сама, без мыши.
    pub pointer_cam_ref: (f64, f64, f64),
    /// Когда последний раз писали диагностику синхронизации курсора (троттлинг:
    /// иначе это 60 строк в секунду на всю длину любой анимации камеры).
    pub pointer_sync_logged: std::time::Instant,
    /// То же для покадровой диагностики курсора в render_surface, плюс камера
    /// прошлого кадра — по ней видно, шевелится ли она вообще.
    pub render_cursor_logged: std::time::Instant,
    pub render_cam_logged: (f64, f64),
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
    /// Запрошенный размер курсора (см. cursor_size_from). Именно он, а не
    /// фактический размер картинки, — потолок для курсоров клиентов.
    pub cursor_size: i32,
    /// Потолок размера для курсоров, которые клиент рисует сам
    /// (`set{ cursor_client_max = ... }`); 0 — не ограничивать.
    pub cursor_client_max: i32,
    /// Курсоры темы по имени формы (wp_cursor_shape_v1). Тема читается с
    /// диска, поэтому каждую форму грузим один раз; None = в теме её нет.
    pub cursor_named_cache: HashMap<String, Option<ThemeCursor>>,
    pub session: Option<LibSeatSession>,
    /// false между SessionEvent::PauseSession и ActivateSession — рендер-хартбит
    /// и VBlank-хендлер в udev.rs должны пропускать render_all()/render_surface()
    /// пока это так, иначе PrepareFrame бесполезно долбится в DrmError(DeviceInactive).
    pub session_active: bool,
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
    /// Доезд зума колесом (см. anim::ZoomGlide). Живёт отдельно от zoom_anim:
    /// тот везёт камеру по заданной траектории, а этот просто догоняет цель,
    /// которую двигает колесо.
    pub zoom_glide: Option<crate::anim::ZoomGlide>,
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
    /// Окна, развёрнутые на весь экран (F11 или запрос клиента), и всё, что
    /// нужно вернуть при выходе, — см. fullscreen.rs. Не более одного НА СТОЛ:
    /// полноэкранная игра на первом столе не мешает развернуть окно на втором.
    pub fullscreens: Vec<crate::fullscreen::Fullscreen>,
    /// Идёт выбор источника для демонстрации экрана: портал ждёт, пока
    /// пользователь ткнёт в окно (или в пустой холст — тогда весь экран).
    /// См. portal.rs.
    pub portal_pick: Option<crate::portal::Pick>,
    /// Что выбрали в прошлый раз — по нему режется кадр для потока.
    pub portal_capture: Option<crate::portal::Capture>,
    /// Живой поток кадров в PipeWire (демонстрация экрана идёт прямо сейчас).
    pub portal_cast: Option<crate::portal_stream::Cast>,
    /// Блютуз: последний снимок BlueZ, канал команд и состояние меню.
    /// None — поток не поднялся (нет системной шины), меню не откроется.
    pub bt: Option<crate::bluetooth::BtUi>,
    /// Полка состояния у панели столов: вайфай, звук, батарея, питание
    /// (см. tray.rs). None — поток опроса не поднялся.
    pub tray: Option<crate::tray::TrayUi>,
    /// Вайфай: снимок NetworkManager и состояние меню сетей (см. wifi.rs).
    pub wifi: Option<crate::wifi::WifiUi>,
    /// Звук: устройства вывода/ввода и состояние меню (см. audio.rs).
    pub audio: Option<crate::audio::AudioUi>,
    /// Идущий прямо сейчас перебор стопки по Alt+Tab (см. switcher.rs).
    /// Живёт, пока держат Alt.
    pub alt_tab: Option<crate::switcher::AltTab>,
    /// Открытый поиск окон по Super+F: запрос, выдача, выбранная строка.
    pub search: Option<crate::switcher::SearchUi>,
    /// Кадр каждого воркспейса (тега): `(cam_x, cam_y, zoom)`. Сохраняется при
    /// уходе со стола, восстанавливается при возврате на него (см. view_tag).
    ///
    /// Зум здесь наравне с камерой: стол — это ОКНО В ХОЛСТ, а окно задаётся и
    /// точкой, и масштабом. Раньше хранилась только точка, и стол, на котором
    /// пользователь отъехал зумом, при возврате показывался в масштабе того
    /// стола, откуда пришли.
    pub tag_cameras: HashMap<u32, (f64, f64, f64)>,
    /// Раскладка (Tile/Float/Monocle/Columns) КАЖДОГО стола по отдельности:
    /// уходя со стола, запоминаем его layout, приходя — восстанавливаем. Так
    /// стол 1 может быть тайловым, а стол 2 плавающим, и переключение столов
    /// переключает и режим. Новый (ещё не посещённый) стол всегда открывается
    /// в Tile — см. view_tag.
    pub tag_layouts: HashMap<u32, crate::tiling::Layout>,
    // ── Module 5 ──────────────────────────────────────────────────────────
    /// Focus Aura (5.3): последняя цель (позиция, размер) — для детекта смены фокуса.
    pub focus_aura_target: Option<(Point<f64, Logical>, (f64, f64))>,
    /// Focus Aura: текущая интерполированная (позиция, размер) для отрисовки.
    pub focus_aura_current: Option<(Point<f64, Logical>, (f64, f64))>,
    pub focus_aura_anim: Option<crate::anim::RectAnim>,
    /// Анимации позиций окон (разлёт/сборка tiling↔floating, свапы при драге,
    /// толчок соседей, инерция броска) — (окно, пружина к целевой позиции).
    /// Пружина, а не LERP с длительностью: цель здесь меняется на лету, и
    /// только пружина умеет перенацелиться без разрыва скорости (см. PosAnim).
    pub window_pos_anims: Vec<(Window, crate::anim::PosAnim)>,
    /// Окно, которое ПРЯМО СЕЙЧАС таскают мышью (MoveSurfaceGrab). Его позицию
    /// каждый motion задаёт курсор, поэтому анимации его не трогают: толчок от
    /// прилетевшего соседа дрался бы с мышью, и окно дрожало бы под курсором.
    pub dragged_window: Option<Window>,
    /// Момент прошлого anim::tick — источник реального dt (тик зовут и таймер,
    /// и VBlank, интервал плавает).
    pub anim_last_tick: Option<std::time::Instant>,
    /// Анимации появления новых окон "с ростом" (Float-режим).
    pub window_open_anims: Vec<(Window, crate::anim::OpenAnim)>,
    /// Плитки декораций (тень окна, маска угла, полоса параллакса) и пулы их
    /// буферов — см. decor.rs.
    pub decor: crate::decor::DecorCache,
    /// Камера (cam_x, cam_y, zoom) на момент входа Float→тайлинг. Выход обратно
    /// во Float возвращает холст сюда — см. Dawn::set_layout.
    pub pre_tiling_view: Option<(f64, f64, f64)>,
    /// Super+2-палец swipe → перемещение окна (новый жест, под курсором на начале).
    pub gesture_move_window: Option<Window>,
    /// Super+pinch → resize окна (новый жест, под курсором на начале).
    pub gesture_resize_window: Option<Window>,
    /// Размеры окон на НАЧАЛО pinch-жеста: само окно под курсором плюс всё
    /// выделение, если оно в него входит (выделенные окна ресайзятся вместе).
    /// Размер считается от этого снимка по абсолютному scale жеста (libinput
    /// отдаёт scale относительно начала), а не накоплением шагов от текущего
    /// размера: клиент применяет configure с задержкой, и несколько кадров
    /// жеста подряд читали один и тот же старый размер — множители терялись,
    /// ресайз выходил вялым и «залипал».
    pub gesture_resize_group: Vec<(Window, Size<i32, Logical>)>,
    /// Копится вместо немедленного render_all() на каждый чих (клавиша,
    /// motion, commit) — фактический рендер выполняется один раз после
    /// event_loop.dispatch() в main.rs, схлопывая пачку событий одного тика
    /// в один проход по CRTC вместо N (см. request_redraw()).
    pub needs_redraw: bool,
    /// Счётчик кадров: раз в секунду печатает, сколько кадров реально ушло на
    /// экран, сколько это стоило по времени и из скольких элементов кадр собран.
    /// Нужен, чтобы «лагает» можно было проверить числом, а не глазом.
    pub render_stats: crate::udev::RenderStats,
    pub lua_config: crate::config::Config,
    /// Пулы стабильных `Id` для процедурных solid-элементов: маски скруглённых
    /// углов, тени окон, фон обзора, параллакс-фон, миникарта, выделение, портал.
    /// Id выдаётся по порядковому номеру элемента в кадре — подробно про то,
    /// почему без этого damage tracking не работал вовсе, см. `pooled_solid` в
    /// udev.rs. Растут до максимума, достигнутого за сессию, и не очищаются:
    /// Id — это просто счётчик, память копеечная.
    ///
    /// Отдельный пул на каждый слой обязателен: номер элемента уникален только
    /// внутри своего пула, а два элемента с одним Id в кадре damage tracker
    /// схлопывает в один.
    pub shadow_ids: Vec<crate::udev::SolidSlot>,
    /// Был ли зажат Shift во время текущего «тапа» Super — в Columns обзор
    /// открывает Shift+Super, а не чистый Super (см. input.rs).
    pub super_tap_shift: bool,
    /// Сколько строк замера пана осталось напечатать (обнуляется, чтобы лог
    /// не рос: 60 строк на жест хватает, чтобы увидеть, стоит ли курсор).
    pub pan_log_left: u32,
    /// Экранная точка курсора в момент НАЧАЛА пана Alt+ЛКМ. Нужна ровно для
    /// одного вопроса: за жест стрелка вернулась на место или осталась
    /// смещённой? Построчный замер этого не отвечает — он печатает ту же
    /// величину, которую сам пан и удерживает.
    pub pan_start_screen: Option<Point<f64, Physical>>,
    /// Направление начатого перехода по столам в Columns (-1 вверх, +1 вниз,
    /// 0 нет) — по нему columns_slide_in_workspace делает вертикальный въезд.
    pub columns_ws_slide: i32,
    /// Камера, под которую уже подвинуты плавающие окна ленты
    /// (см. columns_pin_floating).
    pub columns_float_cam: (f64, f64),
    /// Пока тащат окно в Columns — canvas-x курсора, по нему рисуется
    /// подсказка вставки (niri insert hint). None — драга нет.
    pub columns_drag_hint: Option<f64>,
    /// Накопленный вертикальный ход свайпа в Columns — по нему листаются
    /// столы (см. columns_swipe_workspace).
    pub columns_swipe_dy: f64,
    /// Пул под полоски вкладок Columns (niri tab indicator).
    pub tab_ids: Vec<crate::udev::SolidSlot>,
    /// Пул под подсказку вставки при драге в Columns.
    pub hint_ids: Vec<crate::udev::SolidSlot>,
    pub overview_bg_ids: Vec<crate::udev::SolidSlot>,
    pub minimap_ids: Vec<crate::udev::SolidSlot>,
    pub selection_ids: Vec<crate::udev::SolidSlot>,
    pub portal_ids: Vec<crate::udev::SolidSlot>,
    /// Позиция курсора в последнем ОТРИСОВАННОМ кадре и его время — по ним
    /// видно, отстаёт ли картинка от мыши (см. PTR HIT в input.rs).
    pub frame_cursor: Point<f64, Logical>,
    pub frame_drawn_at: std::time::Instant,
    /// Пул прямоугольников для подсветки выбора источника (см. portal.rs).
    pub portal_pick_ids: Vec<crate::udev::SolidSlot>,
    /// Пул под панель рабочих столов (бар снизу).
    pub bar_ids: Vec<crate::udev::SolidSlot>,
    /// Пул прямоугольников меню блютуза (фон, строки, подсветка).
    pub bt_ids: Vec<crate::udev::SolidSlot>,
    /// Отдельный пул под значок блютуза у панели столов: он виден и когда меню
    /// открыто, поэтому делить слоты с меню нельзя.
    pub bt_ind_ids: Vec<crate::udev::SolidSlot>,
    /// Пул под полку состояния (полосочка, фон ряда, шкала громкости).
    pub tray_ids: Vec<crate::udev::SolidSlot>,
    /// Пул под списочные меню (вайфай, звук): открыто всегда не больше одного,
    /// поэтому слоты у них общие.
    pub menu_ids: Vec<crate::udev::SolidSlot>,
    /// Растеризованные строки текста для оверлеев (см. text.rs).
    pub text_cache: crate::text::TextCache,
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
    /// Полосы колонок ОСТАЛЬНЫХ столов (текущая лежит в `columns`). У niri
    /// каждый воркспейс держит свои колонки — см. columns_save_for/load_for.
    pub columns_by_tag: HashMap<u32, crate::columns::ColumnLayout>,
    /// BSP-деревья dwindle для Layout::Tile — по одному на набор видимых тегов
    /// (ключ = viewport.current_tags()), чтобы у каждого воркспейса своя
    /// раскладка переживала переключение тегов. См. dwindle.rs.
    pub dwindle_trees: std::collections::HashMap<u32, crate::dwindle::DwindleTree>,
    /// Обзор рабочих столов активен (тап Super, см. overview.rs).
    pub overview_active: bool,
    /// Обзор открыт в ленточном (niri) режиме: окна НЕ перекладываются в сетку
    /// миниатюр, камера просто отъезжает и показывает ленту как она есть — как
    /// Super+Space, только с вписыванием всех этажей и обзорными кликами/драгом.
    pub overview_strip: bool,
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
    /// Позиции и размеры окон ДО входа в обзор — обзор сжимает окна в миниатюры
    /// и раскладывает их по сетке столов, а refresh_tags на выходе сохраняет
    /// эти обзорные координаты в tw.position. Для тайловых столов их чинит
    /// arrange(), а для плавающих чинить нечем — поэтому снимок делается на
    /// входе и накатывается обратно на выходе (см. restore_pre_overview_geometry).
    pub overview_saved_geo: Vec<(Window, Point<i32, Logical>, Size<i32, Logical>)>,
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
    // ── Захват экрана (screencast, см. screencopy.rs) ─────────────────────────
    pub image_capture_source_state: ImageCaptureSourceState,
    pub output_capture_source_state: OutputCaptureSourceState,
    pub image_copy_capture_state: ImageCopyCaptureState,
    /// Живые сессии захвата. Держим владение: Session при Drop шлёт клиенту
    /// `stopped`, и демонстрация экрана у него обрывается.
    pub capture_sessions: Vec<Session>,
    /// Запрошенные, но ещё не снятые кадры — обслуживаются в udev::render_surface
    /// сразу после отрисовки обычного кадра (см. screencopy::serve_pending).
    pub pending_frames: Vec<crate::screencopy::PendingFrame>,
    // ── wlr-layer-shell ──────────────────────────────────────────────────────
    /// Состояние протокола wlr-layer-shell (фоновые обои, панели и т.п.).
    pub layer_shell_state: WlrLayerShellState,

    /// Состояние wp_viewporter. Держим ради глобала: пока структура жива, жив
    /// и он.
    ///
    /// Протокол позволяет клиенту сказать «мой буфер такого-то размера, но
    /// показывай его вот в этом прямоугольнике» — масштабирование делает GPU
    /// при отрисовке. Заведён ради dwall: живые обои 1280×720 он растягивал до
    /// 3840×2160 на процессоре и держал этим полъядра. Smithay применяет src и
    /// dst сам, при построении вида поверхности (SurfaceView::from_states), так
    /// что рендеру дополнительной работы не досталось.
    pub viewporter_state: ViewporterState,
}

/// Размер курсора компоновщика: `set{ cursor_size = ... }`, иначе XCURSOR_SIZE
/// (тот же env var читают GTK/Qt/wayland-cursor у клиентов), иначе 24.
pub fn cursor_size_from(cfg: &crate::config::Config) -> i32 {
    if cfg.cursor_size > 0 {
        return cfg.cursor_size;
    }
    std::env::var("XCURSOR_SIZE").ok()
        .and_then(|v| v.parse::<i32>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(24)
}

/// Курсор из темы: картинка, горячая точка и размер после ужатия.
pub type ThemeCursor = (MemoryRenderBuffer, Point<i32, Logical>, Size<i32, Logical>);

/// Грузит курсор темы по списку имён (первое найденное побеждает) и ужимает
/// его до `want` пикселей.
///
/// Имён несколько, потому что имя формы приходит из wp_cursor_shape_v1 в
/// современном виде ("text", "ns-resize"), а темы вроде Vanilla-DMZ-AA знают
/// только старые X11-имена ("xterm", "sb_v_double_arrow"). CursorIcon отдаёт и
/// те, и другие (name + alt_names), поэтому просто перебираем.
pub fn load_theme_cursor(names: &[&str], want: i32) -> Option<ThemeCursor> {
    use xcursor::{CursorTheme, parser::parse_xcursor};

    let try_load = || -> Option<ThemeCursor> {
        // CursorTheme::load() берёт имя темы аргументом, а не из окружения —
        // без этого она искала буквально тему с именем "default", которой
        // почти нигде нет (есть Adwaita/Breeze/...), и курсор вне окон был
        // невидим (клиенты рисуют свой курсор сами через Surface, поэтому
        // внутри окон баг не проявлялся). XCURSOR_THEME — тот же env var,
        // что читают GTK/Qt/wayland-cursor на стороне клиентов.
        let theme_name = std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".to_string());
        let theme = CursorTheme::load(&theme_name);
        let path = names.iter().find_map(|n| theme.load_icon(n))?;
        let bytes = std::fs::read(path).ok()?;
        let images = parse_xcursor(&bytes)?;
        let want = if want > 0 { want } else { 24 };
        let image = images.iter().min_by_key(|i| (i.size as i32 - want).abs())?;
        let k = (want as f64 / (image.width as f64).max(1.0)).min(1.0);
        // Если картинка крупнее запрошенного, ужимаем САМИ ПИКСЕЛИ, а не
        // растягиваем элемент при отрисовке.
        //
        // Курсор уходит на аппаратный слой (Kind::Cursor), а тот масштаб не
        // применяет: он берёт буфер как есть и обрезает его по заданному
        // размеру — стрелка выходила обрезанной. Ужатый буфер решает это раз и
        // навсегда: на слой уезжает ровно то, что нужно показать.
        let (пиксели, шир, выс) = if k < 1.0 {
            let nw = ((image.width as f64 * k).round() as usize).max(1);
            let nh = ((image.height as f64 * k).round() as usize).max(1);
            let (ow, oh) = (image.width as usize, image.height as usize);
            let mut out = vec![0u8; nw * nh * 4];
            // Усреднение по исходному блоку (box filter): у стрелки тонкий
            // контур, и «ближайший сосед» рвал бы его в клочья.
            for y in 0..nh {
                for x in 0..nw {
                    let x0 = x * ow / nw;
                    let x1 = (((x + 1) * ow).div_ceil(nw)).min(ow).max(x0 + 1);
                    let y0 = y * oh / nh;
                    let y1 = (((y + 1) * oh).div_ceil(nh)).min(oh).max(y0 + 1);
                    let (mut a, mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
                    for sy in y0..y1 {
                        for sx in x0..x1 {
                            let i = (sy * ow + sx) * 4;
                            r += image.pixels_rgba[i] as u32;
                            g += image.pixels_rgba[i + 1] as u32;
                            b += image.pixels_rgba[i + 2] as u32;
                            a += image.pixels_rgba[i + 3] as u32;
                            n += 1;
                        }
                    }
                    let o = (y * nw + x) * 4;
                    out[o] = (r / n) as u8;
                    out[o + 1] = (g / n) as u8;
                    out[o + 2] = (b / n) as u8;
                    out[o + 3] = (a / n) as u8;
                }
            }
            (out, nw as i32, nh as i32)
        } else {
            (image.pixels_rgba.clone(), image.width as i32, image.height as i32)
        };
        let buf = MemoryRenderBuffer::from_slice(
            &пиксели,
            Fourcc::Abgr8888,
            (шир, выс),
            1,
            Transform::Normal,
            None,
        );
        // В теме может не быть картинки ровно запрошенного размера (у
        // Vanilla-DMZ-AA минимальная — 24). Тогда берём ближайшую и ужимаем её
        // при отрисовке до нужного: масштабируем и размер, и горячую точку,
        // иначе остриё разъедется с картинкой.
        let hotspot = Point::from((
            (image.xhot as f64 * k).round() as i32,
            (image.yhot as f64 * k).round() as i32,
        ));
        let size = Size::from((шир, выс));
        Some((buf, hotspot, size))
    };

    let loaded = try_load();
    match &loaded {
        Some((_, _, sz)) => tracing::info!(
            "dawn: курсор {:?} загружен {}x{} (тема {:?}, запрошен {})",
            names.first().copied().unwrap_or("?"), sz.w, sz.h,
            std::env::var("XCURSOR_THEME").unwrap_or_else(|_| "default".into()), want,
        ),
        None => tracing::warn!("dawn: в теме нет курсора {:?}", names),
    }
    loaded
}

/// Стрелка компоновщика — то, что видно вне окон.
fn load_default_cursor(want: i32) -> (Option<MemoryRenderBuffer>, Point<i32, Logical>, Size<i32, Logical>) {
    match load_theme_cursor(&["left_ptr", "default", "arrow"], want) {
        Some((buf, hs, sz)) => (Some(buf), hs, sz),
        None => {
            tracing::warn!("dawn: could not load xcursor 'left_ptr', cursor will be invisible");
            (None, Point::from((0, 0)), Size::from((24, 24)))
        }
    }
}

impl Dawn {
    /// Курсор темы для формы, которую попросил клиент через
    /// wp_cursor_shape_v1. Тема лежит на диске, поэтому каждую форму читаем
    /// один раз и держим здесь; None = такой формы в теме нет (тогда рисуем
    /// обычную стрелку, см. render_surface).
    pub fn cursor_for_icon(&mut self, icon: CursorIcon) -> Option<&ThemeCursor> {
        let key = icon.name();
        if !self.cursor_named_cache.contains_key(key) {
            // Сначала современное имя, потом старые X11-синонимы: тема может
            // знать только "xterm", но не "text".
            let mut names: Vec<&str> = vec![key];
            names.extend_from_slice(icon.alt_names());
            let loaded = load_theme_cursor(&names, self.cursor_size);
            self.cursor_named_cache.insert(key.to_string(), loaded);
        }
        self.cursor_named_cache.get(key).and_then(|o| o.as_ref())
    }

    pub fn new(event_loop: &mut EventLoop<Self>, display: Display<Self>) -> Self {
        let dh = display.handle();
        let compositor_state = CompositorState::new::<Self>(&dh);
        let xdg_shell_state = XdgShellState::new::<Self>(&dh);
        let xdg_decoration_state = XdgDecorationState::new::<Self>(&dh);
        let shm_state = ShmState::new::<Self>(&dh, vec![]);
        let output_manager_state = OutputManagerState::new_with_xdg_output::<Self>(&dh);
        let data_device_state = DataDeviceState::new::<Self>(&dh);
        // primary_selection нам не нужен (его глобала у dawn нет), фильтр
        // клиентов — «пускать всех»: сессия своя, посторонних клиентов в ней
        // не бывает.
        let data_control_state = DataControlState::new::<Self, _>(&dh, None, |_| true);
        let xwayland_shell_state = XWaylandShellState::new::<Self>(&dh);
        let layer_shell_state = WlrLayerShellState::new::<Self>(&dh);
        let viewporter_state = ViewporterState::new::<Self>(&dh);
        // Захват экрана (ext-image-copy-capture): без этих глобалов
        // xdg-desktop-portal-wlr не видит у компоситора способа снять картинку,
        // и демонстрация экрана в Discord/OBS остаётся чёрной. См. screencopy.rs.
        // Захват курсора приложением (см. capture.rs). Оба глобала нужны
        // ВМЕСТЕ: constraints запирает стрелку, relative_pointer отдаёт клиенту
        // сырые дельты мыши — без них запертый курсор просто перестал бы
        // сообщать движение, и мышиный обзор в играх умер бы совсем.
        // Сами объекты состояния держать не нужно: глобал живёт в display.
        PointerConstraintsState::new::<Self>(&dh);
        RelativePointerManagerState::new::<Self>(&dh);
        let image_capture_source_state = ImageCaptureSourceState::new();
        let output_capture_source_state = OutputCaptureSourceState::new::<Self>(&dh);
        let image_copy_capture_state = ImageCopyCaptureState::new::<Self>(&dh);
        let lua_config = crate::config::Config::load();
        let mut seat_state = SeatState::new();
        let mut seat: Seat<Self> = seat_state.new_wl_seat(&dh, "dawn");
        seat.add_keyboard(lua_config.xkb_config(), 200, 25).unwrap();
        seat.add_pointer();
        // wp_cursor_shape_v1: клиент называет ФОРМУ курсора ("text", "pointer"),
        // а рисует её компоновщик — своей темой и своего размера. Это и есть
        // штатный способ не давать курсору скакать в размере над каждым окном:
        // GTK4, Qt6 и Chromium, увидев глобал, перестают присылать собственные
        // картинки на 24-32 px и просто просят форму. Курсоры, которые клиент
        // всё-таки рисует сам (прицел в игре, инструмент в редакторе), остаются
        // его собственным делом — см. cursor_client_max в udev.rs.
        CursorShapeManagerState::new::<Self>(&dh);
        let socket_name = Self::init_wayland_listener(display, event_loop);
        let loop_signal = event_loop.get_signal();
        let cursor_size = cursor_size_from(&lua_config);
        let cursor_client_max = if lua_config.cursor_client_max >= 0 {
            lua_config.cursor_client_max
        } else {
            cursor_size
        };
        let (cursor_default_buffer, cursor_default_hotspot, cursor_default_size) =
            load_default_cursor(cursor_size);
        Self {
            start_time: std::time::Instant::now(),
            display_handle: dh,
            socket_name,
            space: Space::default(),
            loop_signal,
            compositor_state,
            xdg_shell_state,
            xdg_decoration_state,
            layer_output: None,
            layer_keyboard: None,
            shm_state,
           dmabuf_state: DmabufState::new(),
           dmabuf_global: None,
            output_manager_state,
            seat_state,
            data_device_state,
            data_control_state,
            xwayland_shell_state,
            xwm: None,
            xdisplay: None,
            popups: PopupManager::default(),
            seat,
            viewport: Viewport::default(),
            cursor_mode: CursorMode::Normal,
            pointer_location: Point::from((0.0, 0.0)),
            pointer_screen: Point::from((0.0, 0.0)),
            pointer_cam_ref: (0.0, 0.0, 1.0),
            pointer_sync_logged: std::time::Instant::now(),
            render_cursor_logged: std::time::Instant::now(),
            render_cam_logged: (0.0, 0.0),
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
            cursor_size,
            cursor_client_max,
            cursor_named_cache: HashMap::new(),
            session: None,
            session_active: true,
            plane_reset_frames: 0,
            momentum: crate::canvas::MomentumState::new(0.5),
            camera_anim: None,
            zoom_anim: None,
            zoom_glide: None,
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
            fullscreens: Vec::new(),
            portal_pick: None,
            portal_capture: None,
            portal_cast: None,
            bt: None,
            tray: None,
            wifi: None,
            audio: None,
            alt_tab: None,
            search: None,
            tag_cameras: HashMap::new(),
            tag_layouts: HashMap::new(),
            focus_aura_target: None,
            focus_aura_current: None,
            focus_aura_anim: None,
            window_pos_anims: Vec::new(),
            dragged_window: None,
            anim_last_tick: None,
            window_open_anims: Vec::new(),
            decor: crate::decor::DecorCache::new(),
            pre_tiling_view: None,
            gesture_move_window: None,
            gesture_resize_window: None,
            gesture_resize_group: Vec::new(),
            needs_redraw: true,
            render_stats: crate::udev::RenderStats::new(),
            lua_config,
            selection_drag: None,
            selected_windows: Vec::new(),
            constellations: Vec::new(),
            columns: crate::columns::ColumnLayout::default(),
            columns_by_tag: HashMap::new(),
            dwindle_trees: HashMap::new(),
            overview_active: false,
            overview_strip: false,
            super_tap: false,
            overview_prev: None,
            overview_order: Vec::new(),
            overview_slots: HashMap::new(),
            overview_saved_geo: Vec::new(),
            overview_exit_pending: false,
            overview_exit_target_ws: None,
            touchpad_move_window: None,
            prev_layout_before_niri: crate::tiling::Layout::Tile,
            shadow_ids: Vec::new(),
            super_tap_shift: false,
            pan_log_left: 60,
            pan_start_screen: None,
            columns_ws_slide: 0,
            columns_float_cam: (0.0, 0.0),
            columns_drag_hint: None,
            columns_swipe_dy: 0.0,
            tab_ids: Vec::new(),
            hint_ids: Vec::new(),
            overview_bg_ids: Vec::new(),
            minimap_ids: Vec::new(),
            selection_ids: Vec::new(),
            portal_ids: Vec::new(),
            portal_pick_ids: Vec::new(),
            frame_cursor: Point::from((0.0, 0.0)),
            frame_drawn_at: std::time::Instant::now(),
            bar_ids: Vec::new(),
            bt_ids: Vec::new(),
            bt_ind_ids: Vec::new(),
            tray_ids: Vec::new(),
            menu_ids: Vec::new(),
            text_cache: crate::text::TextCache::new(),
            image_capture_source_state,
            output_capture_source_state,
            image_copy_capture_state,
            capture_sessions: Vec::new(),
            pending_frames: Vec::new(),
            layer_shell_state,
            viewporter_state,
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
            !crate::xwin::is_surface(w, &surface)
        });
        self.window_open_anims.retain(|(w, _)| {
            !crate::xwin::is_surface(w, &surface)
        });
    }

    /// Позиция, В КОТОРОЙ окно окажется, когда доедет текущая анимация (если
    /// анимация идёт). Коллизии и свапы при драге должны считаться именно от
    /// неё: от текущего кадра полёта решение зависело бы от фазы анимации —
    /// сосед, которого уже толкнули, толкался бы снова и снова, пока летит.
    pub fn window_anim_target(&self, window: &Window) -> Option<Point<i32, Logical>> {
        self.window_pos_anims.iter()
            .find(|(w, _)| crate::dwindle::same_window(w, window))
            .map(|(_, anim)| anim.target.to_i32_round())
    }

    /// Снять анимацию позиции с окна, оставив его там, где оно сейчас.
    /// Нужно перед тем, как окно начнут двигать напрямую (драг мышью, ресайз):
    /// иначе каждый тик анимация возвращала бы окно на свою траекторию и оно
    /// «резинилось» под курсором.
    pub fn freeze_window_anim(&mut self, window: &Window) {
        self.window_pos_anims.retain(|(w, _)| !crate::dwindle::same_window(w, window));
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
    // ── Координаты: холст ↔ экран ───────────────────────────────────────────
    //
    // Ровно две системы, и путать их нельзя (порт подхода driftwm/canvas.rs):
    //  · ХОЛСТ — бесконечная плоскость, где живут окна (tw.position, камера);
    //  · ЭКРАН — физические пиксели монитора, где живут бар, миникарта,
    //    layer-поверхности и курсор.
    //
    //   экран = (холст − камера) × зум      холст = экран ⁄ зум + камера
    //
    // Зум НЕ меняет масштаб выхода (клиентам он не виден) — он применяется
    // только при отрисовке, см. render_surface. Поэтому логический размер
    // выхода всегда равен размеру экрана, а ВИДИМАЯ часть холста при отдалении
    // больше экрана — её даёт visible_canvas_size().

    /// Размер экрана в логических пикселях (режим монитора).
    pub fn screen_size(&self) -> Size<i32, Logical> {
        self.space.outputs().next()
            .and_then(|o| o.current_mode())
            .map(|m| Size::from((m.size.w, m.size.h)))
            .unwrap_or_else(|| (1920, 1080).into())
    }

    /// Сколько холста влезает в экран при текущем зуме.
    pub fn visible_canvas_size(&self) -> Size<f64, Logical> {
        let z = self.viewport.zoom.max(0.01);
        let s = self.screen_size();
        Size::from((s.w as f64 / z, s.h as f64 / z))
    }

    /// Видимая часть холста прямоугольником: угол — камера, размер — экран,
    /// делённый на зум.
    ///
    /// Не то же самое, что `space.output_geometry()`: там loc тоже равен камере
    /// (её задаёт map_output в apply_camera), а вот размер всегда экранный,
    /// потому что зум в масштаб выхода не идёт. На отдалённом зуме видно
    /// БОЛЬШЕ холста, чем говорит output_geometry, на приближенном — меньше.
    pub fn visible_canvas_rect(&self) -> Rectangle<i32, Logical> {
        let size = self.visible_canvas_size();
        Rectangle::new(
            Point::from((
                self.viewport.cam_x.round() as i32,
                self.viewport.cam_y.round() as i32,
            )),
            Size::from((size.w.round() as i32, size.h.round() as i32)),
        )
    }

    /// Кадр `(cam_x, cam_y, zoom)`, к которому вид ЕДЕТ прямо сейчас: если
    /// анимация камеры или зума ещё летит — её конечная точка, иначе текущая.
    ///
    /// Нужен там, где кадр запоминают (уход с воркспейса): взять на лету
    /// `viewport.*` значило бы запомнить полпути, и стол при возврате
    /// открывался бы между двумя местами. Зум и камера берутся из ОДНОГО
    /// источника: zoom_anim умеет вести камеру за собой (см. ZoomAnim::new_pan),
    /// и его цель важнее отдельного camera_anim.
    pub fn view_frame_target(&self) -> (f64, f64, f64) {
        if let Some(z) = self.zoom_anim.as_ref() {
            let (zoom, cam) = z.target();
            return (cam.x, cam.y, zoom);
        }
        let cam = self.camera_anim.as_ref()
            .map(|c| c.to)
            .unwrap_or_else(|| Point::from((self.viewport.cam_x, self.viewport.cam_y)));
        (cam.x, cam.y, self.viewport.zoom)
    }

    /// Точка холста → точка экрана.
    pub fn canvas_to_screen(&self, p: Point<f64, Logical>) -> Point<f64, Logical> {
        let z = self.viewport.zoom;
        Point::from(((p.x - self.viewport.cam_x) * z, (p.y - self.viewport.cam_y) * z))
    }

    /// Точка экрана → точка холста.
    pub fn screen_to_canvas(&self, p: Point<f64, Logical>) -> Point<f64, Logical> {
        let z = self.viewport.zoom.max(0.01);
        Point::from((p.x / z + self.viewport.cam_x, p.y / z + self.viewport.cam_y))
    }

    pub fn apply_camera(&mut self) {
        // Плавающие окна ленты держатся экрана — двигаем их на ту же дельту,
        // что и камеру (только в Columns, см. columns_pin_floating).
        self.columns_pin_floating();
        let cam_x = self.viewport.cam_x.round() as i32;
        let cam_y = self.viewport.cam_y.round() as i32;
        let zoom = self.viewport.zoom;
        let output = self.space.outputs().next().cloned();
        if let Some(output) = output {
            // Zoom через fractional scale — правильный способ.
            //
            // ВАЖНО: только когда zoom РЕАЛЬНО изменился. change_current_state()
            // в smithay ничего не сравнивает: он всегда рассылает wl_output.scale
            // + wl_output.done() КАЖДОМУ клиенту на каждый вызов. А зовётся он
            // отсюда из anim::tick — то есть 60 раз в секунду на всём протяжении
            // любой анимации камеры и инерции, где zoom вообще не меняется. Клиенты
            // (ghostty/GTK) на каждый done() пересчитывают масштаб и перерисовываются,
            // их коммиты повреждают экран и заставляют компоситор рисовать ещё —
            // самоподдерживающаяся нагрузка ровно во время анимаций.
            // ЗУМ БОЛЬШЕ НЕ ЕДЕТ ЧЕРЕЗ МАСШТАБ ВЫХОДА.
            //
            // Раньше здесь стояло change_current_state(Scale::Fractional(zoom)),
            // и это отравляло всё, что зависит от логического размера экрана:
            // клиенты получали wl_output.scale и перерисовывались под чужой
            // масштаб, layer-поверхностям выдавались размеры вроде 12800×5400
            // (обои на этом ложились), screencopy отдавал снимки того же
            // размера, хит-тест разъезжался. Зум теперь применяется ТОЛЬКО при
            // отрисовке (RescaleRenderElement, см. render_surface) — ровно как
            // в driftwm, где output scale отвечает лишь за DPI.
            let _ = zoom;
            // map_output дешевле, но тоже незачем звать, когда камера стоит
            // на месте (в тике она округляется до целых пикселей).
            let mapped = self.space.output_geometry(&output).map(|g| g.loc);
            if mapped != Some(Point::from((cam_x, cam_y))) {
                self.space.map_output(&output, (cam_x, cam_y));
            }
        }
    }

    /// Сдвинуть камеру рукой (пан мышью/тачпадом) так, чтобы под остриём
    /// осталась та же ТОЧКА ХОЛСТА — «схватил и потащил карту».
    ///
    /// Раньше здесь было обратное: стрелка прибивалась к точке ЭКРАНА, весь ход
    /// мыши уходил в камеру, содержимое ехало из-под неподвижного острия. Замеры
    /// подтверждали, что это работает (`ИТОГ ПАН` = 0, `КАДР: курсор_экран`
    /// неизменен весь жест) — но пользователю нужно ровно противоположное
    /// поведение, поэтому пиннинг снят. Теперь `pointer_location` (canvas) не
    /// трогаем вовсе: экранная позиция стрелки = (ptr − cam)·zoom сдвигается
    /// ровно на ход мыши, то есть стрелка едет по монитору вместе с холстом.
    ///
    /// `warp_pointer` тут нужен ради двух вещей: зажать стрелку краем монитора
    /// (иначе при упоре мыши в край она уехала бы за видимую область — у точки
    /// холста такого ограничения нет) и вызвать `pointer_warped`, который
    /// сообщает `sync_pointer_to_camera`, что камера уехала НАМЕРЕННО. Без
    /// этого отложенная синхронизация вернула бы стрелку в старую точку экрана
    /// и вернула бы старое поведение. Поверхность под курсором не меняется
    /// (canvas-точка та же), так что лишних motion не будет — `set_pointer_canvas`
    /// выходит сразу, когда позиция не изменилась.
    ///
    /// Раньше ветки пана двигали только камеру и полагались на отложенную
    /// `sync_pointer_to_camera`. Между этими двумя моментами помещается целый
    /// кадр: на 4K он длится 16-45 мс, а события мыши приходят каждые ~7 мс,
    /// то есть к моменту отрисовки накапливалось 2-6 необработанных дельт по
    /// 15-20 px. Стрелка успевала уехать с холстом на десятки пикселей, кадр
    /// рисовался уже с уехавшей стрелкой, и следующая синхронизация дёргала её
    /// назад. Это и есть «курсор ездит при пане»: не снос (за жест он около
    /// нуля), а дрожь. В логе 20260729_190042 видно ровно это — путь стрелки
    /// 100-670 px за жест при итоговом смещении в единицы пикселей.
    ///
    /// `repin_pointer_to_screen` рассылает `pointer.motion`, поэтому hit-test и
    /// фокус тоже правильные (прежний комментарий в input.rs утверждал
    /// обратное — он старше `repin`/`warp_pointer`). `pointer_warped` в конце
    /// сообщает синхронизации, что камера уехала НАМЕРЕННО и курсор уже
    /// приведён в порядок — иначе она повторит ту же работу.
    pub fn pan_camera_by(&mut self, dcam_x: f64, dcam_y: f64) {
        self.viewport.cam_x -= dcam_x;
        self.viewport.cam_y -= dcam_y;
        self.apply_camera();
        let pos = self.pointer_location;
        self.warp_pointer(pos);
    }

    /// Вторая половина `pan_camera_by` для тех мест, где камера считается не
    /// дельтой (прокрутка ленты зажимает cam_x нулём и берёт cam_y от этажа).
    /// Порядок обязателен: сначала `apply_camera`, потом это.
    pub fn pin_pointer_after_camera(&mut self, screen: Point<f64, Physical>) {
        self.repin_pointer_to_screen(screen);
        self.pointer_warped();
    }

    /// Позиция курсора в ФИЗИЧЕСКИХ пикселях монитора — то, куда реально
    /// смотрит пользователь (камера и зум уже применены).
    pub fn pointer_screen_physical(&self) -> Point<f64, Physical> {
        let zoom = self.viewport.zoom;
        Point::from((
            (self.pointer_location.x - self.viewport.cam_x) * zoom,
            (self.pointer_location.y - self.viewport.cam_y) * zoom,
        ))
    }

    /// Вернуть курсор в ту же точку ЭКРАНА после того, как камера/зум уехали
    /// сами, и пересчитать, что теперь под ним.
    ///
    /// Мышь — устройство экрана, а не холста. Когда камера едет без участия
    /// мыши (перелёт к столу, вход/выход из обзора, инерция, зум), стрелка
    /// обязана остаться там же на мониторе. Раньше pointer_location при этом
    /// не трогали, и получалось худшее из двух: стрелку утаскивало вместе с
    /// холстом (при длинном перелёте — вообще за край экрана), а pointer.motion
    /// никто не слал, так что под курсором оставалась СТАРАЯ поверхность.
    /// Клик после этого уходил не туда, куда показывает стрелка — это и есть
    /// «странный хитбокс».
    pub fn repin_pointer_to_screen(&mut self, screen: Point<f64, Physical>) {
        let zoom = self.viewport.zoom.max(0.01);
        let pos = Point::from((
            screen.x / zoom + self.viewport.cam_x,
            screen.y / zoom + self.viewport.cam_y,
        ));
        self.set_pointer_canvas(pos);
    }

    /// Переставить курсор в canvas-точку НАМЕРЕННО (телепорт по миникарте,
    /// курсор едет за окном в жесте, драг стола в обзоре) — с рассылкой motion
    /// и фиксацией новой экранной позиции как эталонной.
    ///
    /// Единственный законный способ двигать курсор не по вводу мыши. Прямая
    /// запись в `pointer_location` разводит два курсора: стрелка рисуется по
    /// `pointer_location`, а клики, захваты и якорь зума идут по
    /// `pointer.current_location()` внутри smithay, которая обновляется ТОЛЬКО
    /// из `pointer.motion`. Разъехавшись, они дают ровно тот «странный
    /// хитбокс»: видно одно, нажимается другое.
    pub fn warp_pointer(&mut self, pos: Point<f64, Logical>) {
        self.set_pointer_canvas(pos);
        self.pointer_warped();
    }

    /// Общая часть: зажать точку экраном, записать её и разослать motion.
    fn set_pointer_canvas(&mut self, mut pos: Point<f64, Logical>) {
        // За пределы монитора курсор не выпускаем — там его не видно, а клики
        // всё равно уходили бы в окна, которых на экране нет.
        // Зажимаем по ВИДИМОЙ ЧАСТИ ХОЛСТА, а не по размеру выхода: при зуме
        // 0.5 в экран влезает вдвое больше холста, и курсор обязан доставать до
        // его краёв. Раньше здесь стоял размер выхода — он совпадал с видимой
        // областью только потому, что зум ехал через масштаб выхода. Теперь
        // зум живёт в отрисовке (см. render_surface), размер выхода равен
        // экрану, и без этой поправки курсор упирался в невидимую стену на
        // половине экрана (тот самый «кривой хитбокс»).
        let vis = self.visible_canvas_size();
        pos.x = pos.x.clamp(self.viewport.cam_x, self.viewport.cam_x + vis.w);
        pos.y = pos.y.clamp(self.viewport.cam_y, self.viewport.cam_y + vis.h);
        if pos == self.pointer_location {
            return;
        }
        self.pointer_location = pos;

        let under = self.surface_under(pos);
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        let time = self.start_time.elapsed().as_millis() as u32;
        let pointer = match self.seat.get_pointer() { Some(p) => p, None => return };
        pointer.motion(self, under, &smithay::input::pointer::MotionEvent {
            location: pos, serial, time,
        });
        pointer.frame(self);
    }

    /// Принять текущую позицию курсора как намеренную: запомнить её экранную
    /// проекцию вместе с текущей камерой. Нужно там, где курсор переносят
    /// СПЕЦИАЛЬНО одновременно со сменой камеры (телепорт по миникарте) —
    /// иначе sync_pointer_to_camera примет это за уехавшую камеру и вернёт
    /// стрелку на прежнее место экрана.
    /// Щелчок колеса зума: двигает ЦЕЛЬ, а не сам масштаб — доезд до неё
    /// делает anim::ZoomGlide. Раньше зум переставлялся здесь же, и каждый
    /// щелчок был мгновенным скачком на 10%.
    pub fn zoom_step_at_cursor(&mut self, factor: f64) {
        let cursor = self.pointer_location;
        let zoom = self.viewport.zoom;
        // Экранная точка курсора: её и держим неподвижной весь доезд.
        let screen = Point::from((
            (cursor.x - self.viewport.cam_x) * zoom,
            (cursor.y - self.viewport.cam_y) * zoom,
        ));
        // Цель копится от ПРЕДЫДУЩЕЙ цели, а не от текущего масштаба: иначе
        // быстрый прокрут терял бы щелчки — каждый следующий считался бы от
        // ещё не доехавшего зума.
        let base = self.zoom_glide.as_ref().map(|g| g.target).unwrap_or(zoom);
        let target = (base * factor).clamp(0.05, 5.0);
        // Жест пользователя главнее перелёта камеры: иначе обе анимации
        // писали бы zoom по очереди и картинка дрожала бы.
        self.zoom_anim = None;
        match self.zoom_glide.as_mut() {
            Some(g) => g.retarget(target, cursor, screen),
            None => self.zoom_glide = Some(crate::anim::ZoomGlide::new(target, cursor, screen)),
        }
        self.request_redraw();
    }

    pub fn pointer_warped(&mut self) {
        self.pointer_cam_ref = (self.viewport.cam_x, self.viewport.cam_y, self.viewport.zoom);
        self.pointer_screen = self.pointer_screen_physical();
    }

    /// Свести курсор с камерой — один раз за итерацию главного цикла, ПОСЛЕ
    /// того как отработали ввод и анимации (см. main.rs).
    ///
    /// Две ситуации, и различаются они только тем, шевелилась ли камера:
    /// * камера стоит — курсор двигался (или нет) сам, его экранная позиция
    ///   просто пересчитывается из canvas-позиции и запоминается;
    /// * камера уехала — значит уехала БЕЗ мыши (анимация перелёта, инерция,
    ///   зум, вход/выход из обзора, переключение стола). Тогда ведущей
    ///   становится запомненная экранная позиция: стрелка обязана остаться в
    ///   той же точке монитора, а под неё подставляется новая canvas-точка.
    ///
    /// Одна точка вызова вместо правки полутора десятков мест, где меняется
    /// камера: любое движение камеры, откуда бы оно ни пришло, здесь будет
    /// замечено сравнением с pointer_cam_ref.
    pub fn sync_pointer_to_camera(&mut self) {
        let cam = (self.viewport.cam_x, self.viewport.cam_y, self.viewport.zoom);
        if cam == self.pointer_cam_ref {
            self.pointer_screen = self.pointer_screen_physical();
            return;
        }
        self.pointer_cam_ref = cam;
        let screen = self.pointer_screen;
        // Диагностика «курсор ездит при пане»: не чаще 4 строк в секунду,
        // и только пока камера реально движется. Если стрелку сносит, здесь
        // будет видно расхождение «хотели / получилось» в экранных пикселях.
        if self.pointer_sync_logged.elapsed().as_millis() >= 250 {
            self.pointer_sync_logged = std::time::Instant::now();
            let got = self.pointer_screen_physical();
            tracing::debug!(
                "СИНХ КУРСОР: держим экран=({:.0},{:.0}) было=({:.0},{:.0}) снос=({:.1},{:.1}) камера=({:.0},{:.0}) zoom={:.2}",
                screen.x, screen.y, got.x, got.y, got.x - screen.x, got.y - screen.y,
                cam.0, cam.1, cam.2,
            );
        }
        self.repin_pointer_to_screen(screen);
        // repin зажимает курсор краем монитора, так что фактическая экранная
        // позиция могла и сдвинуться — перечитываем её, а не верим желаемой.
        self.pointer_screen = self.pointer_screen_physical();
    }

    pub fn surface_under(&self, pos: Point<f64, Logical>) -> Option<(WlSurface, Point<f64, Logical>)> {
        if let Some(portal) = &self.portal {
            if let Some(hit) = self.portal_hit_test(portal, pos) {
                return Some(hit);
            }
        }
        // Overlay/Top — НАД окнами, поэтому спрашиваем их первыми. Background и
        // Bottom намеренно не опрашиваем: в dawn пустой холст — это орган
        // управления (пан, жесты, рамка выделения), и обои, растянутые на весь
        // выход, съедали бы каждый такой клик.
        if let Some(hit) = self.layer_surface_under(pos, &[WlrLayer::Overlay, WlrLayer::Top]) {
            return Some(hit);
        }
        self.space.element_under(pos).and_then(|(window, location)| {
            window.surface_under(pos - location.to_f64(), WindowSurfaceType::ALL)
                .map(|(s, p)| (s, (p + location).to_f64()))
        })
    }

    /// Стоит ли курсор над layer-поверхностью верхних слоёв (Overlay/Top).
    ///
    /// Нужно там, где «под курсором нет ОКНА» ошибочно принималось за «под
    /// курсором пусто»: layer-поверхности не лежат в `space`, поэтому меню
    /// обоев (dwall) или лаунчер считались голым холстом — клик по ним снимал
    /// фокус и запускал рамку выделения, а до клиента не доходил.
    pub fn курсор_над_слоем(&self, pos: Point<f64, Logical>) -> bool {
        self.layer_surface_under(pos, &[WlrLayer::Overlay, WlrLayer::Top])
            .is_some()
    }

    /// Попадание курсора в layer-поверхность (wlr-layer-shell).
    ///
    /// Layer-поверхности приколоты к ЭКРАНУ, а не к холсту: рендер кладёт их по
    /// `layer_geometry` без камеры и зума (см. build_layer_elements в udev.rs).
    /// Значит и хит-тест обязан идти в экранных координатах — иначе панель
    /// нажималась бы там, где её нарисовали бы при zoom=1 и камере в нуле.
    ///
    /// Возвращает (surface, «виртуальный origin») с тем же контрактом, что и
    /// обычная ветка surface_under: `pos - origin` даёт surface-локальную точку.
    fn layer_surface_under(
        &self,
        pos: Point<f64, Logical>,
        layers: &[WlrLayer],
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let output = self.layer_output.clone().or_else(|| self.space.outputs().next().cloned())?;
        let map = layer_map_for_output(&output);
        // Экранная точка в тех же координатах, в которых слой РИСУЕТСЯ: он
        // приклеен к экрану и от зума не зависит (см. build_layer_elements),
        // поэтому положение курсора переводим в экранные пиксели зумом.
        let zoom = self.viewport.zoom;
        let screen = Point::<f64, Logical>::from((
            (pos.x - self.viewport.cam_x) * zoom,
            (pos.y - self.viewport.cam_y) * zoom,
        ));
        for &layer in layers {
            let Some(surface) = map.layer_under(layer, screen) else { continue };
            let Some(geo) = map.layer_geometry(surface) else { continue };
            let surface_local = screen - geo.loc.to_f64();
            // surface_under уважает input region клиента: поверхность, которая
            // отказалась от ввода, вернёт None, и клик уйдёт ниже — к окнам.
            if let Some((wl, offset)) = surface.surface_under(surface_local, WindowSurfaceType::ALL) {
                let local = surface_local - offset.to_f64();
                return Some((wl, pos - local));
            }
        }
        None
    }

    /// Замер «промаха ВНУТРИ окна»: окно под курсором то самое, но нажимается
    /// точка со сдвигом. Печатает всё, что участвует в переводе canvas-точки в
    /// координаты клиента:
    ///  · `локальная` — ровно та точка, которую клиент получит в wl_pointer;
    ///  · `окно` — размер, который МЫ считаем размером окна (по нему строится
    ///    раскладка и обзор);
    ///  · `поверхность` — размер, который клиент реально закоммитил.
    ///
    /// Читается так: `локальная` вне `поверхность` или `окно ≠ поверхность` —
    /// виноват композитор (шлём координаты не в тот кадр). Всё сходится, а
    /// промах есть — координаты доходят верные, и мажет уже клиент, который
    /// видит наш зум как `wl_output.scale`.
    pub fn log_pointer_local(&self, pos: Point<f64, Logical>) {
        let Some((surface, origin)) = self.surface_under(pos) else {
            tracing::debug!("PTR ЛОКАЛЬ: под курсором нет поверхности");
            return;
        };
        let local = pos - origin;
        let win = self.space.element_under(pos).map(|(w, _)| w.clone());
        let win_geo = win.as_ref()
            .and_then(|w| self.space.element_geometry(w))
            .map(|g| g.size);
        // Поля клиентских рамок (тень, невидимые бортики ресайза): у окна с
        // ними начало дерева поверхностей и начало видимой части разнесены на
        // эту точку. Если тут не (0,0), а картинка и клики расходятся — смотреть
        // на неё: она и есть величина расхождения (см. цикл окон в udev.rs).
        let поля = win.as_ref().map(|w| w.geometry().loc);
        let surf_size = smithay::backend::renderer::utils::with_renderer_surface_state(
            &surface, |s| s.surface_size(),
        ).flatten();
        tracing::debug!(
            "PTR ЛОКАЛЬ: локальная=({:.1},{:.1}) начало=({:.1},{:.1}) окно={:?} \
             поля_рамок={:?} поверхность={:?} zoom={:.2} обзор={}",
            local.x, local.y, origin.x, origin.y, win_geo, поля, surf_size,
            self.viewport.zoom, self.overview_active,
        );
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
            .find(|tw| crate::xwin::is_surface(&tw.window, &portal.surface))
            .map(|tw| &tw.window)?;
        let geo = self.space.element_geometry(window)?;

        let frac_x = local_x / portal.box_size.w as f64;
        let frac_y = local_y / portal.box_size.h as f64;
        // Доля рамки — это доля ВИДИМОЙ части окна, а surface_under ждёт точку
        // от НАЧАЛА ДЕРЕВА ПОВЕРХНОСТЕЙ. У клиентов с клиентскими рамками эти
        // два начала разнесены на window.geometry().loc (поля под тень), и без
        // слагаемого клик по порталу уходил выше-левее видимого.
        let поля = window.geometry().loc;
        let surface_local = Point::from((
            поля.x as f64 + frac_x * geo.size.w as f64,
            поля.y as f64 + frac_y * geo.size.h as f64,
        ));

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
        let focused = match self.focused_surface() {
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
    /// наименьшего перекрытия, в сторону от центра mover'а, с зазором.
    /// Реакция ЦЕПНАЯ: отодвинутое окно само толкает своих соседей (BFS), иначе
    /// оно просто накрывало бы того, кто стоял за ним. Каждое окно волна двигает
    /// не более одного раза — этим цепочка и завершается.
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
            a == b
        };
        let mut others: Vec<(Window, Rectangle<i32, Logical>)> = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current != 0)
            .map(|tw| tw.window.clone())
            .filter(|w| !same(w, mover))
            .filter_map(|w| self.space.element_geometry(&w).map(|g| (w, g)))
            .collect();

        const MARGIN: i32 = 8;
        let mut moved = vec![false; others.len()];
        let mut queue: std::collections::VecDeque<Rectangle<i32, Logical>> =
            std::collections::VecDeque::new();
        queue.push_back(mover_geo);

        while let Some(m) = queue.pop_front() {
            let mcx = m.loc.x + m.size.w / 2;
            let mcy = m.loc.y + m.size.h / 2;
            for i in 0..others.len() {
                if moved[i] { continue; }
                let g = others[i].1;
                let inter = match m.intersection(g) {
                    Some(i) => i,
                    None => continue,
                };
                // Толкаем по оси наименьшего перекрытия — так окно "выскальзывает"
                // в ближайшую свободную сторону.
                let (mut nx, mut ny) = (g.loc.x, g.loc.y);
                if inter.size.w <= inter.size.h {
                    nx = if g.loc.x + g.size.w / 2 >= mcx {
                        m.loc.x + m.size.w + MARGIN
                    } else {
                        m.loc.x - g.size.w - MARGIN
                    };
                } else {
                    ny = if g.loc.y + g.size.h / 2 >= mcy {
                        m.loc.y + m.size.h + MARGIN
                    } else {
                        m.loc.y - g.size.h - MARGIN
                    };
                }
                let new_geo = Rectangle::new(Point::from((nx, ny)), g.size);
                others[i].1 = new_geo;
                moved[i] = true;
                queue.push_back(new_geo);
            }
        }

        for (i, (w, g)) in others.into_iter().enumerate() {
            if !moved[i] { continue; }
            let new_loc = g.loc;
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
        use crate::tiling::Layout;
        // Из обзора столов переход по столам = ВЫХОД из обзора на этот стол.
        // Guard стоит здесь, а не в dispatch_action, потому что сюда приходят и
        // другие бинды: workspace_step (Super+PageUp/Down),
        // move_column_to_workspace. Раньше они меняли теги и дёргали
        // set_layout/arrange, пока обзор ещё активен, а его снимок геометрии
        // (overview_prev/overview_saved_geo) оставался от прежнего стола — на
        // выходе окна восстанавливались не туда, и «бинды столов ломались».
        // Рекурсии нет: exit_overview_immediate зовёт view_tag уже с
        // overview_active = false.
        if self.overview_active {
            self.exit_overview_immediate(Some(tag));
            return;
        }
        // У каждого воркспейса свой КАДР (камера + зум) И своя раскладка —
        // запоминаем текущие перед уходом, восстанавливаем сохранённые для
        // нового тега.
        let old_tag = self.viewport.current_tags();
        self.tag_cameras.insert(old_tag, self.view_frame_target());
        self.tag_layouts.insert(old_tag, self.tile_config.layout);

        // НОВЫЙ (ещё не посещённый) воркспейс → включаем tiling и раскладываем
        // от (0,0). Повторные заходы восстанавливают запомненный за этим столом
        // layout и позицию камеры. Считаем ДО всех изменений: от этого зависит,
        // в какую изоляцию мы едем.
        let is_new = !self.visited_tags.contains(&tag);
        let target_layout = if is_new {
            // Новые столы всегда открываются в Tile.
            Layout::Tile
        } else {
            *self.tag_layouts.get(&tag).unwrap_or(&Layout::Tile)
        };

        // Полоса колонок у каждого стола СВОЯ, и переезд между изоляциями её не
        // тащит: полосу покидаемого стола прячем на его полку (текущая после
        // этого пуста), полосу нового достаём — но только если новый стол
        // ленточный. Раньше и то и другое делалось лишь когда ЛЕНТОЙ БЫЛ
        // ПОКИДАЕМЫЙ стол, поэтому вход в ленту с тайлового стола отдавал ей
        // чужую полосу: колонки, стопки и вкладки нового стола пересобирались
        // из остатков предыдущего.
        if self.tile_config.layout == Layout::Columns {
            self.columns_save_for(old_tag);
        }
        if target_layout == Layout::Columns {
            self.columns_load_for(tag);
        }

        // Стол переходит в свою группу ДО refresh_tags: видимость окон (лента
        // показывает все свои этажи, tiling/floating — только свой стол)
        // считается по tag_layouts, см. columns_is_strip_tag.
        self.visited_tags.insert(tag);
        self.tag_layouts.insert(tag, target_layout);
        self.viewport.tagset[self.viewport.seltags] = tag;
        self.refresh_tags();
        // Фокус за окном, которое только что ушло с экрана, — это клавиатура,
        // проваливающаяся в никуда: полноэкранная игра с первого стола
        // продолжала съедать набор на втором, а F11 разворачивал её же вместо
        // окна перед глазами. Видимость спрашиваем у space (refresh_tags уже
        // отработал), а не у тегов: в ленте на экране законно живут окна
        // соседних этажей, и отбирать у них фокус не за что.
        self.refocus_visible();

        // Режим — свойство СТОЛА, а не глобальный тумблер: уходя с ленты на
        // стол, который помнит себя тайловым, выходим из Columns (ниже общей
        // веткой через set_layout). Раньше здесь стояло «текущий Columns ИЛИ
        // целевой Columns», из-за чего лента прилипала ко всем столам подряд.
        if target_layout == Layout::Columns {
            self.tile_config.layout = Layout::Columns;
            self.arrange();
            // Пока нас не было, состав ленты мог смениться (стол вышел из неё
            // через Win+N, новый стол занял бит в середине) — этажи ниже
            // съехали. Правим их ДО перелёта, чтобы камера приезжала на готовый
            // стол, а не на пустое место.
            self.columns_relayout_strip();
            self.columns_set_active_to_focus();
            self.columns_scroll_to_active();
            // Перелёт на этаж нового стола (вертикальная лента). Строго ПОСЛЕ
            // scroll_to_active и arrange: окна нового стола уже разложены на
            // своём этаже, и камера летит к готовой картинке.
            self.columns_fly_to_workspace(tag);
            // Зум у ленточного стола тоже свой. Перелёт выше ведёт только
            // камеру, поэтому, если запомненный зум другой, ПОДМЕНЯЕМ его
            // анимацию на общую (камера + зум одним ZoomAnim): две анимации
            // разом писали бы cam_x/cam_y по очереди и дрались бы за камеру.
            if let Some(&(_, _, zoom)) = self.tag_cameras.get(&tag) {
                if (zoom - self.viewport.zoom).abs() > 0.001 {
                    let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
                    let to = self.camera_anim.as_ref().map(|a| a.to).unwrap_or(from);
                    self.camera_anim = None;
                    self.zoom_anim = Some(ZoomAnim::new_pan(
                        from, to, self.viewport.zoom, zoom, Duration::from_millis(220),
                    ));
                }
            }
            tracing::info!("dawn: view_tag → {:#b} (columns workspace)", tag);
            return;
        }

        if is_new {
            // Стол ещё не видели: показывать его неоткуда — ставим кадр в
            // начало координат сами. Раньше это делал restore_layout, но
            // теперь он камеру при ПЕРЕХОДЕ НА СТОЛ не трогает (иначе затирал
            // бы запомненный кадр, см. tiling::set_layout_inner).
            self.restore_layout(target_layout);
            self.momentum.stop();
            self.camera_anim = None;
            self.zoom_anim = None;
            self.viewport.zoom = 1.0;
            self.viewport.cam_x = 0.0;
            self.viewport.cam_y = 0.0;
            self.apply_camera();
            tracing::info!("dawn: view_tag → {:#b} (new workspace → {})", tag, target_layout.symbol());
            return;
        }

        // Стол помнит свой режим: если он отличается от текущего — переключаем.
        // Камеру при этом restore_layout не двигает, кадр ниже ставим сами: он
        // у стола свой независимо от раскладки.
        if target_layout != self.tile_config.layout {
            // Именно restore_layout: переход на стол не двигает его окна
            // (см. tiling::restore_layout — иначе плавающие слетались в точку).
            self.restore_layout(target_layout);
            tracing::info!("dawn: view_tag → {:#b} (layout {})", tag, target_layout.symbol());
        }

        // Плавный "перелёт" в кадр стола вместо мгновенного прыжка. Кадр — это
        // камера И зум: стол, с которого ушли отдалённым, обязан таким же и
        // открыться. Пока зум не хранился, возврат показывал стол в масштабе
        // того стола, откуда пришли.
        //
        // Когда зум меняется, камеру ведёт ZoomAnim::new_pan: две отдельные
        // анимации (camera_anim + zoom_anim) писали бы cam_x/cam_y по очереди
        // каждый кадр и дрались бы между собой.
        let frame = self.tag_cameras.get(&tag).copied()
            // Стол посещали ДО того, как кадры начали запоминаться (или он
            // потерял запись) — открываем его в начале координат, как раньше.
            .unwrap_or((0.0, 0.0, 1.0));
        let (x, y, zoom) = frame;
        let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let to = Point::from((x, y));
        let сдвиг = (to.x - from.x).abs() > 0.5 || (to.y - from.y).abs() > 0.5;
        let зум_иной = (zoom - self.viewport.zoom).abs() > 0.001;
        self.momentum.stop();
        if зум_иной {
            self.camera_anim = None;
            self.zoom_anim = Some(ZoomAnim::new_pan(
                from, to, self.viewport.zoom, zoom, Duration::from_millis(300),
            ));
        } else if сдвиг {
            self.zoom_anim = None;
            self.camera_anim = Some(CameraAnim::new(from, to, Duration::from_millis(300)));
        } else {
            self.camera_anim = None;
            self.zoom_anim = None;
            self.viewport.cam_x = x;
            self.viewport.cam_y = y;
            self.viewport.zoom = zoom;
            self.apply_camera();
        }
        tracing::info!(
            "dawn: view_tag → {:#b} (кадр {:.0},{:.0} zoom {:.2})", tag, x, y, zoom,
        );
    }

    /// Число активных воркспейсов в niri-модели: индекс последнего занятого
    /// стола + 1 (всегда один пустой снизу). Кэп 9 (ограничение битовой маски
    /// тегов). Учитывает и текущий стол, даже если он пуст, чтобы навигация не
    /// «схлопывалась» при переходе на свежесозданный пустой.
    pub fn niri_ws_count(&self) -> i32 {
        // Считаем ЭТАЖИ ленты, а не биты тегов, и только по своим столам:
        // чужой (тайловый) стол в середине не должен ни занимать этаж, ни
        // удлинять ленту (см. columns_tag_foreign).
        let mut highest = self.columns_floor_index(self.viewport.current_tags()) + 1;
        for tw in &self.tagged_windows {
            if !tw.floating && tw.tags != 0 && !self.columns_tag_foreign(tw.tags) {
                let idx = self.columns_floor_index(tw.tags) + 1;
                if idx > highest { highest = idx; }
            }
        }
        (highest + 1).min(9)
    }

    /// Число доступных НЕ-ленточных воркспейсов: позиция последнего занятого
    /// среди неленточных тегов (по порядку 1..9) + 2 (сам стол + один пустой
    /// снизу). Используется как верхний лимит для workspace_step вне Columns,
    /// аналогично niri_ws_count для ленты. Текущий стол всегда учитывается —
    /// навигация не «схлопывается» при переходе на свежесозданный пустой.
    pub fn tiling_ws_count(&self) -> usize {
        let tiling_tags: Vec<u32> = (0..9u32)
            .map(|i| 1u32 << i)
            .filter(|&m| !self.columns_is_strip_tag(m))
            .collect();
        if tiling_tags.is_empty() { return 0; }
        let cur = self.viewport.current_tags();
        let mut highest = tiling_tags.iter()
            .position(|&m| m == cur)
            .unwrap_or(0);
        for tw in &self.tagged_windows {
            if tw.tags == 0 || self.columns_is_strip_tag(tw.tags) { continue; }
            if let Some(idx) = tiling_tags.iter().position(|&m| m == tw.tags) {
                if idx > highest { highest = idx; }
            }
        }
        // Пустой стол снизу, но не дальше доступной линейки.
        (highest + 2).min(tiling_tags.len())
    }

    /// niri-воркспейсы: перейти на пред/след воркспейс.
    ///
    /// Лента ИЗОЛИРОВАНА от остальных столов, и изоляция двусторонняя: в
    /// Columns шаг идёт по своим этажам (диапазон динамический — последний
    /// занятый + пустой снизу), чужие столы пропускаются целиком; вне Columns
    /// пропускаются, наоборот, ленточные — иначе PageUp/PageDown с тайлового
    /// стола втягивал бы в niri-режим, пересобирая окна чужой раскладкой.
    pub fn workspace_step(&mut self, dir: i32) {
        use crate::tiling::Layout;
        if dir == 0 { return; }
        if self.tile_config.layout == Layout::Columns {
            let target = match self.columns_strip_neighbor(self.viewport.current_tags(), dir) {
                Some(t) => t,
                None => return,
            };
            // Направление перехода — для вертикального въезда нового стола
            // (см. columns_slide_in_workspace); view_tag сам его подхватит.
            self.columns_ws_slide = dir.signum();
            self.view_tag(target);
            return;
        }
        // Вне ленты столы тоже динамические: шаг ходит по неленточным тегам, но
        // не дальше «последний занятый + один пустой». Раньше PageDown уводил на
        // любой из 9 тегов подряд — с тайлового стола 1 можно было уехать на
        // пустой стол 7, минуя пять таких же пустых, и обратной дороги по
        // столам с окнами не было.
        let tiling_tags: Vec<u32> = (0..9u32)
            .map(|i| 1u32 << i)
            .filter(|&m| !self.columns_is_strip_tag(m))
            .collect();
        let cur = self.viewport.current_tags();
        let Some(pos) = tiling_tags.iter().position(|&m| m == cur) else { return };
        let step = dir.signum();
        let limit = self.tiling_ws_count() as i32;
        let new = pos as i32 + step;
        if new < 0 || new >= limit {
            return;
        }
        let Some(&m) = tiling_tags.get(new as usize) else { return };
        self.columns_ws_slide = step;
        self.view_tag(m);
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
        // Смена набора видимых тегов на живом обзоре ломает его сетку (она
        // построена по столам на момент входа) — сперва выходим, см. view_tag.
        if self.overview_active {
            self.exit_overview_immediate(None);
        }
        let new = self.viewport.current_tags() ^ tag;
        if new != 0 {
            self.viewport.tagset[self.viewport.seltags] = new;
            self.refresh_tags();
            tracing::info!("dawn: toggle_view → {:#b}", new);
        }
    }

    /// Назначить тег focused окну (Super+Shift+N)
    pub fn tag_window(&mut self, tag: u32) {
        // Переезд окна на другой стол прямо в обзоре оставил бы его миниатюру
        // в чужой ячейке, а снимок геометрии — от старого стола: выходим,
        // потом переносим (см. view_tag). Перетаскивание мышью в обзоре — это
        // отдельный путь (overview_reassign), он сетку обновляет сам.
        if self.overview_active {
            self.exit_overview_immediate(None);
        }
        let Some(focused) = self.focused_window_surface() else {
            self.refresh_tags();
            return;
        };
        let Some((window, floating, old_tag)) = self.tagged_windows.iter()
            .find(|tw| crate::xwin::is_surface(&tw.window, &focused))
            .map(|tw| (tw.window.clone(), tw.floating, tw.tags))
        else {
            self.refresh_tags();
            return;
        };
        if old_tag == tag {
            return;
        }

        // Стол-приёмник — лента: окно обязано войти в её МОДЕЛЬ КОЛОНОК, а не
        // просто сменить тег. Иначе полосе о нём известно только через ленивый
        // columns_reconcile, который допишет его колонкой в самый конец при
        // следующем заходе на стол. Плавающие окна не трогаем: в ленте они
        // остаются плавающими поверх полосы (см. columns_pin_floating).
        if !floating && self.columns_is_strip_tag(tag) {
            if self.columns_adopt_window(&window, tag) {
                tracing::info!("dawn: tag_window → {:#b} (в ленту)", tag);
                return;
            }
        }

        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
            crate::xwin::is_surface(&tw.window, &focused)
        }) {
            tw.tags = tag;
            tracing::info!("dawn: tag_window → {:#b}", tag);
        }
        // Окно ушло со СВОЕГО стола — донор обязан сомкнуться сразу. Полоса
        // держит его в колонке, дерево dwindle — в листе, и вычищают их только
        // columns_reconcile / sync_dwindle_tree, то есть arrange. Без него на
        // месте уехавшего окна оставалась дыра до следующей операции с
        // раскладкой: refresh_tags лишь снимает окно с холста.
        self.refresh_tags();
        self.arrange();
    }

    /// Toggle тег на focused окне (Super+Ctrl+Shift+N)
    pub fn toggle_tag(&mut self, tag: u32) {
        if self.overview_active {
            self.exit_overview_immediate(None);
        }
        if let Some(focused) = self.focused_window_surface() {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                crate::xwin::is_surface(&tw.window, &focused)
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

        // Добавляем видимые. В ленте (Columns) видимы окна ВСЕХ ленточных
        // столов: столы там не подменяют друг друга, а лежат этажами одной
        // вертикальной ленты (стол N на высоте N × экран, см. columns.rs), и
        // соседние этажи обязаны существовать — иначе переход между столами
        // показывал бы пустоту, а не уезжающий стол. За кадром они ничего не
        // стоят: damage tracking и eco-mode отсекают всё вне вида.
        // Лента ли ТЕКУЩИЙ стол — спрашиваем у него самого, а не у глобального
        // тумблера: view_tag меняет тег раньше, чем layout, и по тумблеру
        // тайловый стол на миг считался ленточным — окна соседних этажей
        // оставались на экране поверх него (нулевой этаж лежит ровно на нём).
        let strip = self.columns_is_strip_tag(current);
        let visible: Vec<(Window, Point<i32, Logical>)> = self.tagged_windows.iter()
            .filter(|tw| {
                tw.tags & current != 0
                    || (strip && tw.tags != 0 && self.columns_is_strip_tag(tw.tags))
            })
            .map(|tw| (tw.window.clone(), tw.position))
            .collect();
        for (w, pos) in visible {
            self.space.map_element(w, pos, false);
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

    /// Увести клавиатуру с окна, которого больше нет на экране.
    ///
    /// Зовётся после смены стола (см. view_tag). Фокус переходит верхнему
    /// видимому окну; если видимых окон нет — снимается совсем, как при клике
    /// по пустому холсту.
    pub fn refocus_visible(&mut self) {
        let Some(фокус) = self.focused_surface() else { return };
        // Клавиатуру может держать не окно, а слой (панель, лончер) или меню —
        // они со сменой стола никуда не деваются, и отбирать у них фокус не за
        // что. Разбираемся только с окнами.
        let это_окно = self.tagged_windows.iter()
            .any(|tw| crate::xwin::is_surface(&tw.window, &фокус));
        if !это_окно {
            return;
        }
        if self.space.elements().any(|w| crate::xwin::is_surface(w, &фокус)) {
            return;
        }
        // space.elements() идёт снизу вверх; меню и тултипы (override-redirect)
        // фокус не принимают — их отсеивает сам xwin::focus.
        let верхнее = self.space.elements()
            .filter(|w| self.tagged_windows.iter().any(|tw| &tw.window == *w))
            .next_back()
            .cloned();
        match верхнее {
            Some(window) => crate::xwin::focus(self, &window),
            None => {
                if let Some(kb) = self.seat.get_keyboard() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    kb.set_focus(self, None, serial);
                }
            }
        }
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
        // Условия ровно те же, при которых миникарта РИСУЕТСЯ (см. render_surface):
        // невидимая панель не имеет права есть клики. Раньше здесь стояла
        // проверка только на `is_minimap_visible`, а в кадре миникарту прятали
        // ещё и под полноэкранным окном и в обзоре — то есть в полноэкранной
        // игре её панель 460×300 висела в правом верхнем углу НЕВИДИМОЙ ловушкой:
        // выстрел в ту зону экрана съедался компоновщиком и уносил камеру к
        // окну «под точкой клика».
        if !self.is_minimap_visible || self.overview_active || self.fullscreen_here() {
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
        // Курсор здесь переносится НАМЕРЕННО (вместе с камерой): warp_pointer
        // разошлёт motion и зафиксирует новую экранную позицию, иначе
        // sync_pointer_to_camera увидит уехавшую камеру и вернёт стрелку туда,
        // где она была на панели миникарты.
        self.warp_pointer(target_point);
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

    /// Alt+Super+B: убрать ближайшую к курсору закладку.
    ///
    /// «Ближайшая» считается по холсту от точки под курсором — то есть от того
    /// же места, куда её ставит Alt+B (pin_bookmark_at_cursor). Так удаление
    /// зеркально постановке: подвёл курсор к ненужному крестику — убрал.
    pub fn delete_nearest_bookmark(&mut self) {
        let точка = self.pointer_location;
        let Some((&slot, _)) = self.camera_bookmarks.iter()
            .min_by(|(_, a), (_, b)| {
                let da = (a.x - точка.x).powi(2) + (a.y - точка.y).powi(2);
                let db = (b.x - точка.x).powi(2) + (b.y - точка.y).powi(2);
                da.total_cmp(&db)
            })
        else {
            tracing::info!("dawn: закладок нет — удалять нечего");
            return;
        };
        self.camera_bookmarks.remove(&slot);
        tracing::info!("dawn: удалена закладка {} (ближайшая к курсору)", slot);
        self.request_plane_reset();
        self.request_redraw();
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
        self.focused_surface()
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Курсор темы приходит РОВНО запрошенного размера, и форма ищется не
    /// только по современному имени.
    ///
    /// Обе половины проверки — про то, из-за чего курсор над окнами и был
    /// крупнее: тема Vanilla-DMZ-AA держит картинки от 24 px и знает только
    /// старые X11-имена ("xterm", но не "text"). Если бы load_theme_cursor не
    /// ужимал картинку или не перебирал синонимы, wp_cursor_shape_v1 отдавал
    /// бы либо курсор чужого размера, либо стрелку вместо каретки.
    #[test]
    fn курсор_темы_нужного_размера_и_по_синониму() {
        let Some((_, _, размер)) = load_theme_cursor(&["left_ptr"], 16) else {
            eprintln!("темы курсоров в системе нет — проверка пропущена");
            return;
        };
        assert!(размер.w <= 16 && размер.h <= 16, "курсор не ужат: {:?}", размер);

        let текст = CursorIcon::Text;
        let mut имена: Vec<&str> = vec![текст.name()];
        имена.extend_from_slice(текст.alt_names());
        assert!(имена.contains(&"xterm"), "синонимы формы потерялись: {:?}", имена);
        if let Some((_, _, размер)) = load_theme_cursor(&имена, 16) {
            assert!(размер.w <= 16 && размер.h <= 16, "каретка не ужата: {:?}", размер);
        }
    }
}
