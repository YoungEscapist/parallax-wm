use std::{collections::HashMap, os::unix::fs::OpenOptionsExt, time::Duration};

use smithay::{
    backend::{
        allocator::{
            Fourcc,
            gbm::{GbmAllocator, GbmBufferFlags, GbmDevice},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEvent, DrmNode, NodeType,
            compositor::{DrmCompositor, FrameError, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
        },
        egl::{EGLContext, EGLDisplay},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            damage::OutputDamageTracker,
            element::{
                AsRenderElements,
                Id,
                Kind,
                memory::MemoryRenderBufferRenderElement,
                solid::SolidColorRenderElement,
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
                texture::TextureRenderElement,
                utils::{CropRenderElement, RescaleRenderElement},
            },
            gles::{GlesRenderer, GlesTexture},
            utils::CommitCounter,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{UdevBackend, UdevEvent},
    },
    desktop::{Window, WindowSurface, PopupManager, space::SpaceRenderElements, layer_map_for_output},
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop,
            timer::{TimeoutAction, Timer},
        },
        drm::control::{ModeTypeFlags, connector, crtc},
        input::Libinput,
        rustix::fs::OFlags,
    },
    utils::{DeviceFd, Logical, Physical, Point, Rectangle, Size, Transform},
    wayland::{
        compositor::with_states,
        shell::wlr_layer::Layer as WlrLayer,
    },
};
use smithay::wayland::dmabuf::DmabufFeedbackBuilder;
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};

use smithay::backend::renderer::ImportDma;
use crate::Parallax;
use crate::{т, тф};

/// ВРЕМЕННАЯ ДИАГНОСТИКА (артефакты на анимированных обоях, 04.08.2026).
///
/// Включается переменной `PLX_DEBUG_FRAME=1`. Даёт две вещи, которых иначе
/// не увидеть:
///  · строку с damage-прямоугольниками КАЖДОГО кадра (что компоситор реально
///    считает изменившимся);
///  · по появлению файла `/tmp/plx_dump` — снимок НАСТОЯЩЕГО кадра со
///    сканаута (`blit_frame_result`) в `/tmp/plx_frame.raw`. Обычный grim
///    сюда не годится: screencopy перерисовывает кадр с нуля свежим damage
///    tracker'ом и артефактов частичной перерисовки не показывает.
fn debug_frame_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("PLX_DEBUG_FRAME").is_ok_and(|v| v != "0"))
}

// Курсор в parallax всегда client-side (нет server-side cursor протокола) — клиент
// сам рисует картинку в отдельном wl_surface через wl_pointer.set_cursor, и
// компоситор обязан вставить эту поверхность как ещё один render-элемент
// поверх обычных окон. SpaceRenderElements (окна) и WaylandSurfaceRenderElement
// (курсор) — разные конкретные типы, поэтому render_frame нужен один общий
// enum, оборачивающий оба варианта (как в anvil: render.rs::OutputRenderElements).
smithay::backend::renderer::element::render_elements! {
    pub OutputRenderElements<=GlesRenderer>;
    Space = SpaceRenderElements<GlesRenderer, WaylandSurfaceRenderElement<GlesRenderer>>,
    Cursor = WaylandSurfaceRenderElement<GlesRenderer>,
    // Текстурные элементы: курсор темы и плитки декораций (см. decor.rs).
    Memory = MemoryRenderBufferRenderElement<GlesRenderer>,
    Solid = SolidColorRenderElement,
    // Layer-поверхности (обои, панели, меню plx-wall): обёрнуты в Rescale, чтобы
    // не масштабироваться вместе с зумом холста (см. build_layer_elements).
    Layer = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
    // Живая миникарта: то же окно, что и на холсте, но ужатое до масштаба
    // панели и обрезанное её краями (см. build_minimap_elements). Один и тот
    // же surface законно попадает в кадр дважды — smithay ведёт состояние
    // ПОЭКЗЕМПЛЯРНО (ElementState::last_instances), см. damage/mod.rs.
    Minimap = CropRenderElement<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    // Окно со скруглёнными углами: та же поверхность, но нарисованная своим
    // текстурным шейдером, который вырезает углы по альфе (см. rounded.rs).
    Rounded = crate::rounded::Rounded<RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>>,
    // Обои бесконечного холста: текстура фонового слоя, положенная со сдвигом
    // за камерой (см. build_wallpaper_backdrop). Своя, а не Layer, именно ради
    // Id: у элемента он обязан быть свой, а не поверхностный.
    Wallpaper = CropRenderElement<TextureRenderElement<GlesTexture>>,
    // Размытый фон под островом панели: та же текстура, но обрезанная
    // скруглением плашки тем же шейдером, что и углы окон (см. blur.rs).
    Blur = crate::rounded::Rounded<CropRenderElement<TextureRenderElement<GlesTexture>>>,
}

type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    (),
    DrmDeviceFd,
>;

pub struct Surface {
    pub output: Output,
    pub compositor: GbmDrmCompositor,
    pub damage_tracker: OutputDamageTracker,
    /// Кадр уже отдан в `queue_frame` и ждёт своего VBlank.
    ///
    /// Без этого флага во время анимации рендер шёл ДВАЖДЫ на каждый показанный
    /// кадр: один раз из VBlank-хендлера и ещё раз из главного цикла по
    /// `needs_redraw`, который выставляет 60-герцевый anim-таймер. Второй рендер
    /// не выбрасывался в никуда молча — `DrmCompositor::queue_frame` кладёт кадр
    /// в `queued_frame`, а следующий queue_frame ЗАТИРАЕТ его, — то есть половина
    /// полностью отрисованных 4K-кадров (по 9-17 мс каждый) просто выбрасывалась.
    /// Бюджет 16.6 мс на кадр от этого удваивался, и анимации ехали ~30 fps
    /// рывками. Плюс каждый рендер шлёт клиентам frame callback (см. хвост
    /// render_surface), так что клиенты тоже рисовали вдвое чаще, чем нужно.
    pub frame_queued: bool,
    /// Когда флаг выше был выставлен. Страховка от вечной заморозки экрана:
    /// если VBlank почему-то не пришёл (потеря page flip, гонка при VT-switch),
    /// через FRAME_QUEUE_STALE_MS флаг игнорируется и рендер идёт как раньше.
    pub frame_queued_at: std::time::Instant,
    /// Сколько элементов было в прошлом кадре — под столько и резервируем
    /// список следующего (см. render_surface).
    pub last_elements: usize,
    /// Шейдер скруглённых углов окон (см. rounded.rs). Компилируется один раз
    /// на поверхность — программа принадлежит контексту EGL своего рендерера,
    /// и общая на все устройства она быть не может. `None` — не собрался, окна
    /// останутся с прямыми углами.
    pub rounded: Option<crate::rounded::Шейдер>,
    /// Размытие фона под плашками. None — шейдер не собрался или блюр выключен
    /// (`set{ blur = ... }`, по умолчанию выключен — см. blur.rs).
    pub blur: Option<crate::blur::Блюр>,
    /// Сколько раз подряд `render_frame` вернул ошибку (см. `отказ_до`).
    pub отказов_подряд: u32,
    /// До какого момента не пытаться рисовать после отказа. `None` — можно.
    pub отказ_до: Option<std::time::Instant>,
    /// Когда об отказах в последний раз писали в лог (см. ОТКАЗ_ЛОГ_МС).
    pub отказ_лог: Option<std::time::Instant>,
}

/// Посекундная сводка по рендеру — дешёвая замена профайлеру, которого на этой
/// машине нет. Печатается на уровне `debug!` (штатный RUST_LOG в launch_tty.zsh),
/// одна строка в секунду, и только когда за секунду был хоть один кадр: на
/// неподвижном экране лог остаётся пустым, и это само по себе сигнал, что
/// damage tracking работает.
#[derive(Debug)]
pub struct RenderStats {
    since: std::time::Instant,
    frames: u32,
    skipped: u32,
    total_us: u64,
    max_us: u64,
    max_elements: usize,
    /// Разбивка max_elements по группам сцены — чтобы «элементов до 259» можно
    /// было прочитать как «из них столько-то окна, столько-то тени». Без неё
    /// понять, что именно набивает список, можно только гаданием.
    max_ui: usize,
    max_windows: usize,
    max_decor: usize,
    max_bg: usize,
    /// Сколько кадр ждал GPU перед page flip (см. needs_sync в render_surface).
    sync_us: u64,
    sync_max_us: u64,
    /// Размытие фона под плашками. Считается отдельной строкой, а не внутри
    /// `total_us`, по итогам замера 03.09.2026: свёртка стояла ДО всех гейтов
    /// отрисовки, то есть в `средний ... мс` не попадала вовсе — сводка
    /// показывала 0.3 мс на кадр, а процесс ел 33% ядра. Свой счётчик нужен,
    /// чтобы этот разрыв больше нельзя было не заметить.
    blur_us: u64,
    blur_max_us: u64,
    blur_count: u32,
}

impl RenderStats {
    pub fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            frames: 0, skipped: 0, total_us: 0, max_us: 0, max_elements: 0,
            max_ui: 0, max_windows: 0, max_decor: 0, max_bg: 0,
            sync_us: 0, sync_max_us: 0,
            blur_us: 0, blur_max_us: 0, blur_count: 0,
        }
    }

    /// Границы групп берутся по длине списка в четырёх точках сборки сцены
    /// (см. render_surface): интерфейс поверх всего, затем окна, затем декор
    /// (тени и фоны обзора), затем фоновый слой с параллаксом.
    fn record_breakdown(&mut self, ui: usize, windows: usize, decor: usize, bg: usize) {
        self.max_ui = self.max_ui.max(ui);
        self.max_windows = self.max_windows.max(windows);
        self.max_decor = self.max_decor.max(decor);
        self.max_bg = self.max_bg.max(bg);
    }

    fn record(&mut self, us: u64, elements: usize) {
        self.frames += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        self.max_elements = self.max_elements.max(elements);
        self.flush();
    }

    fn record_sync(&mut self, us: u64) {
        self.sync_us += us;
        self.sync_max_us = self.sync_max_us.max(us);
    }

    fn record_skip(&mut self) {
        self.skipped += 1;
    }

    fn record_blur(&mut self, us: u64) {
        self.blur_us += us;
        self.blur_max_us = self.blur_max_us.max(us);
        self.blur_count += 1;
    }

    fn flush(&mut self) {
        if self.since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let secs = self.since.elapsed().as_secs_f64();
        tracing::debug!(
            "plx/render: {:.0} кадр/с, средний {:.1} мс, худший {:.1} мс, \
             элементов до {} (интерфейс {}, окна {}, декор {}, фон {}), \
             пропущено (кадр уже в очереди) {}, \
             ожидание GPU: среднее {:.2} мс, худшее {:.2} мс, \
             блюр: {} раз, средний {:.2} мс, худший {:.2} мс",
            self.frames as f64 / secs,
            self.total_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.max_us as f64 / 1000.0,
            self.max_elements,
            self.max_ui,
            self.max_windows,
            self.max_decor,
            self.max_bg,
            self.skipped,
            self.sync_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.sync_max_us as f64 / 1000.0,
            self.blur_count,
            self.blur_us as f64 / self.blur_count.max(1) as f64 / 1000.0,
            self.blur_max_us as f64 / 1000.0,
        );
        *self = Self::new();
    }
}

/// Через сколько «зависший» `frame_queued` перестаёт блокировать рендер.
/// Заметно больше кадра (16.6 мс) и заметно меньше 500-мс хартбита, который
/// в самом плохом случае всё равно перезапустит цепочку.
const FRAME_QUEUE_STALE_MS: u128 = 100;

/// ── Откат после отказа `render_frame` ────────────────────────────────────────
///
/// Кадру нужен буфер из swapchain, а буфер выделяет GPU. На этой машине
/// (RTX 5060, 8 ГБ) видеопамять кончается на ровном месте: dota2 занимает
/// 2.4 ГБ, plx-wall с NVDEC ещё полгигабайта, — и `gbm_bo_create` начинает
/// возвращать EINVAL, а ядро сыпать `nv_drm_gem_alloc_nvkms_memory_ioctl:
/// Failed to allocate NVKMS memory for GEM object`. Само по себе это внешняя
/// беда и проходит за секунду-другую.
///
/// Ломало же нас СОБСТВЕННОЕ поведение: отрисовка кончалась ошибкой, но хвост
/// `render_surface` всё равно рассылал клиентам frame callback, клиенты тут же
/// коммитили новый кадр, коммит просил перерисовку — и круг замыкался на
/// скорости процессора. Замер по логам 23.08.2026: 22057 отказов за 95 секунд,
/// в пике 871 штука за 0.6 с (~1400 попыток выделения в секунду). Каждая
/// попытка — ioctl в nvidia-drm и две строки в лог, а лог из launch_native.sh
/// идёт через `tee` синхронной записью на диск ПРЯМО ИЗ ПОТОКА РЕНДЕРА. То
/// есть нехватка памяти на полсекунды превращалась в затык на секунды, и
/// именно он виден как «замерло намертво».
///
/// Поэтому после отказа поверхность молчит нарастающую паузу: 16 мс, 32, 64 …
/// до полусекунды. Запрос на кадр при этом не теряется (`needs_redraw`), а
/// клиентам не уходят callback'и — они перестают крутить нас вхолостую.
const ОТКАЗ_ПАУЗА_МС: u64 = 16;
const ОТКАЗ_ПАУЗА_МАКС_МС: u64 = 500;
/// Не чаще одной строки в секунду на поверхность: см. про `tee` выше.
const ОТКАЗ_ЛОГ_МС: u128 = 1000;

pub struct Device {
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub gles: GlesRenderer,
    pub drm_scanner: DrmScanner,
    pub surfaces: HashMap<crtc::Handle, Surface>,
    pub render_node: DrmNode,
}

pub fn init_udev(
    event_loop: &mut EventLoop<Parallax>,
    state: &mut Parallax,
) -> Result<(), Box<dyn std::error::Error>> {

    let (session, notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("plx/udev: seat={}", seat_name);
    state.session = Some(session.clone());

    // Тачпады открываем ВТОРЫМ читателем поверх libinput: сырых координат
    // пальцев libinput не отдаёт, а автодоводу по краям накладки они и есть
    // всё содержание (см. touchpad.rs). `EVIOCGRAB` не берём — иначе отняли бы
    // тачпад у самого libinput. И открываем СВОИМ `open`, не через сеанс:
    // libseat отдаёт устройство ровно одному читателю, и открытый через него
    // тачпад libinput не получал вовсе — курсор в сеансе был мёртв.
    state.тачпады = crate::touchpad::найти();

    let mut libinput = Libinput::new_with_udev(
        LibinputSessionInterface::from(session.clone()),
    );
    libinput.udev_assign_seat(&seat_name).unwrap();
    let libinput_backend = LibinputInputBackend::new(libinput.clone());

    // ── Session notifier ─────────────────────────────────────────────────────
    // КАК В NIRI/HYPRLAND: master берём ТОЛЬКО при ActivateSession,
    // не при старте. Seatd сам координирует передачу между compositor'ами.
    let mut libinput_for_notifier = libinput.clone();
    event_loop.handle().insert_source(notifier, move |event, _, state| {
        match event {
            SessionEvent::PauseSession => {
                tracing::info!("plx/udev: session paused");
                state.session_active = false;
                // Отпускание кнопки, случившееся на чужом VT, до нас не
                // доедет, а счётчик удержания запрещает переход курсора на
                // соседний монитор (см. input.rs). Один потерянный Released —
                // и край экрана залипает навсегда до перезапуска.
                state.кнопок_нажато = 0;
                libinput_for_notifier.suspend();
                // Отдаём DRM master — seatd передаёт его другому compositor'у
                for device in state.udev_devices.values_mut() {
                    device.drm.pause();
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("plx/udev: session activated — acquiring DRM master");
                state.session_active = true;
                let _ = libinput_for_notifier.resume();
                // Берём DRM master обратно — теперь мы активный compositor
                let mut devices = std::mem::take(&mut state.udev_devices);
                for device in devices.values_mut() {
                 // activate(false) = не отключать коннекторы, просто взять master
                match device.drm.activate(false) {
                    Ok(()) => tracing::info!("plx/udev: DRM master acquired"),
                    Err(e) => tracing::warn!("plx/udev: activate failed: {:?}", e),
                }
                // Сбрасываем состояние compositor'а после VT switch
                for surface in device.surfaces.values_mut() {
                    let _ = surface.compositor.reset_state();
                    let _ = surface.compositor.frame_submitted();
                    // Кадр, поставленный в очередь до VT-switch, своего VBlank уже
                    // не дождётся — иначе экран после возврата остался бы мёртвым
                    // до срабатывания страховки по FRAME_QUEUE_STALE_MS.
                    surface.frame_queued = false;
                }
                // Рендерим все поверхности
                let crtcs: Vec<_> = device.surfaces.keys().cloned().collect();
                for crtc in crtcs {
                    if let Some(surface) = device.surfaces.get_mut(&crtc) {
                        let gles = &mut device.gles as *mut GlesRenderer;
                        unsafe { render_surface(surface, &mut *gles, state); }
                    }
                }
                }
                state.udev_devices = devices;

            }
        }
    }).unwrap();

    // ── Libinput ─────────────────────────────────────────────────────────────
    event_loop.handle().insert_source(libinput_backend, |event, _, state| {
        state.process_input_event(event);
    }).unwrap();

    let udev_backend = UdevBackend::new(&seat_name)?;

    // ── Добавляем DRM устройства ──────────────────────────────────────────────
    // false = НЕ берём master при инициализации.
    // Master придёт через ActivateSession когда seatd будет готов.
    let mut session_clone = session.clone();
    for (dev_id, path) in udev_backend.device_list() {
        match add_device(&mut session_clone, state, dev_id, path) {
            Ok((device, drm_notifier, node)) => {

                // ── VBlank handler ────────────────────────────────────────────
                event_loop.handle().insert_source(drm_notifier, move |event, _, state| {
                    match event {
                        DrmEvent::VBlank(crtc) => {
                            // trace!, а не debug!: одна строка на КАЖДЫЙ кадр, а лог
                            // из launch_tty.zsh пишется через tee синхронно прямо из
                            // единственного потока рендера (см. queue_frame ниже).
                            tracing::trace!("plx/drm: VBlank crtc={:?}", crtc);
                            let mut devices = std::mem::take(&mut state.udev_devices);
                            if let Some(device) = devices.get_mut(&node) {
                                if let Some(surface) = device.surfaces.get_mut(&crtc) {
                                    // ОБЯЗАТЕЛЬНО: без этого compositor думает
                                    // что предыдущий frame ещё в flight
                                    match surface.compositor.frame_submitted() {
                                        Ok(_) => {}
                                        Err(e) => tracing::warn!("plx/drm: frame_submitted: {:?}", e),
                                    }
                                    // Показанный кадр отпускает «шлагбаум»: следующий
                                    // рендер разрешён.
                                    surface.frame_queued = false;
                                }
                            }
                            state.udev_devices = devices;
                            // Досчитываем анимации ПРЯМО ПЕРЕД кадром:
                            // 60Гц-таймер из main.rs тикает независимо
                            // от VBlank, и между ними набегала расфазировка
                            // до целого кадра — позиция окна на экране
                            // отставала/забегала то на кадр, то на ноль,
                            // что и читается как «дёрганая» анимация.
                            // Тик по времени (Instant), так что лишний
                            // вызов ничего не ломает — он просто
                            // сэмплирует анимацию в момент отрисовки.
                            crate::anim::tick(state);
                            // Кадр собираем ТОЛЬКО когда есть что показывать —
                            // и на ВСЕХ выходах разом, а не только на том, чей
                            // VBlank сейчас пришёл.
                            //
                            // Раньше здесь звался render_surface одного этого
                            // CRTC, а `state.needs_redraw` — ОБЩИЙ на все
                            // мониторы — гасился тут же. На двух мониторах с
                            // разной частотой обновления (или просто с фазовым
                            // сдвигом VBlank) это раздавало ход только тому,
                            // чей VBlank прозвонил первым: второй монитор в
                            // ту же самую итерацию видел needs_redraw уже
                            // снятым и пропускал кадр целиком — вместе с ним
                            // пропускал и рассылку frame callback своим
                            // layer-поверхностям (см. хвост render_surface).
                            // Замер жалобы Ярика («обои анимируются только на
                            // зуме/пане»): plx-wall на неактивном мониторе тактуется
                            // именно этими callback'ами, и без них засыпает —
                            // а просыпался только когда движение камеры на
                            // ДРУГОМ мониторе гоняло needs_redraw достаточно
                            // часто, чтобы иногда попасть в его VBlank первым.
                            // render_all() уже устроен ровно под этот случай:
                            // рендерит все CRTC и каждый раз, когда какой-то
                            // из них ещё ждёт свой предыдущий VBlank
                            // (`frame_queued`), сам возвращает needs_redraw в
                            // true — соседний монитор ничего не теряет.
                            //
                            // Раньше каждый VBlank безусловно собирал
                            // весь список элементов (у Ярика это до
                            // 225 штук) и звал render_frame. На мониторе
                            // 200 Гц это 200 полных сборок сцены в
                            // секунду по 0.9 мс — около 18% ядра, и
                            // добрая половина из них заканчивалась
                            // EmptyFrame: показывать было нечего.
                            //
                            // Пропуск ничего не подвешивает: состояние
                            // «не рисуем, ждём изменений» уже
                            // существует и работает — ровно в него
                            // приходит EmptyFrame, когда сцена не
                            // изменилась. Любой источник изменений
                            // (коммит клиента, ввод, анимация, тик
                            // полки) зовёт request_redraw — на это
                            // опирается и главный цикл, который тоже
                            // рисует только по needs_redraw.
                            if state.needs_redraw {
                                state.needs_redraw = false;
                                render_all(state);
                            }
                        }
                        DrmEvent::Error(e) => tracing::warn!("plx/drm: error: {:?}", e),
                    }
                }).unwrap();

                state.udev_devices.insert(node, device);

                // Явно пробуем стать master сразу — не ждём ActivateSession,
                // который может не прийти, если сессия уже была активна при старте
                if let Some(dev) = state.udev_devices.get_mut(&node) {
                    match dev.drm.activate(false) {
                        Ok(()) => tracing::info!("plx/udev: DRM master acquired at startup"),
                        Err(e) => tracing::warn!("plx/udev: initial activate failed: {:?}", e),
                    }
                }

                // linux-dmabuf global — нужен kitty/firefox/GTK для GPU buffers.
                // Без этого fd=-1 и клиенты не могут рисоваться через GPU.
                if state.dmabuf_global.is_none() {
                    if let Some(dev) = state.udev_devices.get(&node) {
                        let formats = dev.gles.dmabuf_formats();
                        match DmabufFeedbackBuilder::new(
                            dev.render_node.dev_id(), formats
                        ).build() {
                            Ok(feedback) => {
                                let global = state.dmabuf_state
                                    .create_global_with_default_feedback::<Parallax>(
                                        &state.display_handle,
                                        &feedback,
                                    );
                                state.dmabuf_global = Some(global);
                                tracing::info!("plx/udev: DMA-BUF global created");
                            }
                            Err(e) => {
                                tracing::warn!("plx/udev: dmabuf feedback: {:?}", e);
                            }
                        }
                    }
                }
                let node_render = node;
                event_loop.handle().insert_idle(move |state| {
                    tracing::info!("plx/udev: initial render (idle)");
                    let mut devices = std::mem::take(&mut state.udev_devices);
                    if let Some(dev) = devices.get_mut(&node_render) {
                        let crtcs: Vec<_> = dev.surfaces.keys().cloned().collect();
                        for crtc in crtcs {
                            if let Some(surface) = dev.surfaces.get_mut(&crtc) {
                                let gles = &mut dev.gles as *mut GlesRenderer;
                                unsafe { render_surface(surface, &mut *gles, state); }
                            }
                        }
                    }
                    state.udev_devices = devices;
                });
            }
            Err(e) => tracing::warn!("plx/udev: skip {:?}: {}", path, e),
        }
    }

    // ── Hotplug ───────────────────────────────────────────────────────────────
    let mut session_hp = session.clone();
    event_loop.handle().insert_source(udev_backend, move |event, _, state| {
        match event {
            UdevEvent::Added { device_id, path } => {
                if let Ok((device, _, node)) = add_device(&mut session_hp, state, device_id, &path) {
                    state.udev_devices.insert(node, device);
                }
            }
            UdevEvent::Changed { device_id } => {
                // Коннектор мог не устаканиться к моменту первого scan_connectors
                // в add_device (гонка с хэндовером DRM master от предыдущего
                // compositor'а) — тогда он просто не появится в device.surfaces
                // навсегда. Здесь пересканируем при каждом udev-событии "changed"
                // (их шлют штатно при (пере)стабилизации коннекторов), чтобы
                // подхватить экран, пропущенный при старте.
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    if let Some(mut device) = state.udev_devices.remove(&node) {
                        scan_connectors(&mut device, state);
                        state.udev_devices.insert(node, device);
                    }
                }
            }
            UdevEvent::Removed { device_id } => {
                if let Ok(node) = DrmNode::from_dev_id(device_id) {
                    if let Some(dev) = state.udev_devices.remove(&node) {
                        // Симметрично scan_connectors::Disconnected — иначе
                        // при выдёргивании ЦЕЛОГО устройства (не одного
                        // коннектора: eGPU, докстанция) монитор оставался в
                        // Parallax::мониторы навсегда: столы у него не отвязать,
                        // apply_camera_all продолжал бы гонять камеру
                        // несуществующему выходу, а «активный» индекс мог до
                        // конца сессии указывать в пустоту. Раньше отсюда
                        // только снимался wl_output у клиентов — сам монитор
                        // parallax считал живым.
                        for (_, s) in dev.surfaces {
                            state.space.unmap_output(&s.output);
                            state.снять_монитор(&s.output);
                        }
                    }
                }
            }
        }
    }).unwrap();

    // ── Render heartbeat ─────────────────────────────────────────────────────
    // Рендер реактивный (VBlank/input/commit): если очередной render_frame не
    // находит изменений, queue_frame ничего не коммитит (FrameError::EmptyFrame)
    // и цепочка VBlank обрывается насовсем — экран замирает до следующего
    // внешнего события, а если и оно не долетело (например после session
    // pause/resume), зависает навсегда. Хартбит регулярно дёргает render_all(),
    // так что цепочка сама себя перезапускает на ближайшем тике.
    let timer = Timer::from_duration(Duration::from_millis(500));
    event_loop.handle().insert_source(timer, |_, _, state| {
        render_all(state);
        TimeoutAction::ToDuration(Duration::from_millis(500))
    }).unwrap();

    Ok(())
}

fn add_device(
    session: &mut LibSeatSession,
    state: &mut Parallax,
    dev_id: libc::dev_t,
    path: &std::path::Path,
) -> Result<(Device, smithay::backend::drm::DrmDeviceNotifier, DrmNode), Box<dyn std::error::Error>> {
    let drm_node = DrmNode::from_dev_id(dev_id)?;
    let render_node = drm_node
        .node_with_type(NodeType::Render)
        .and_then(|n| n.ok())
        .unwrap_or(drm_node);

    let fd = session.open(
        path,
        OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK,
    )?;
    let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

    // false = не берём master при инициализации
    // Seatd даст его нам через ActivateSession
    let (drm, notifier) = DrmDevice::new(device_fd.clone(), false)?;
    // GBM на ПЕРВИЧНОМ узле — только для буферов скан-аута: их выделяет и
    // экспортирует в KMS-фреймбуферы тот же узел, что владеет дисплеем.
    let gbm = GbmDevice::new(device_fd.clone())?;

    // А рисуем на РЕНДЕР-узле (/dev/dri/renderD128), отдельным fd. Раньше
    // EGL/GLES строились поверх card0 — то есть весь рендер шёл через узел,
    // который требует DRM master и монопольно принадлежит владельцу экрана.
    // Из-за этого parallax забирал видеокарту целиком и не мог делить её с чужой
    // сессией (Xorg/другой Wayland): узел один, хозяин может быть только один.
    // Рендер-узел никакого master не требует (права crw-rw-rw-), это штатный
    // путь всех компоновщиков: KMS — на card0, GL — на renderD128.
    let render_gbm = open_render_gbm(&render_node);
    let egl = match render_gbm {
        Some(rgbm) => unsafe { EGLDisplay::new(rgbm)? },
        None => {
            tracing::warn!(
                "plx/udev: рендер-узел недоступен, EGL на первичном узле \
                 (видеокарта будет занята монопольно)"
            );
            unsafe { EGLDisplay::new(gbm.clone())? }
        }
    };
    let ctx = EGLContext::new(&egl)?;
    let gles = unsafe { GlesRenderer::new(ctx)? };

    let mut device = Device {
        drm, gbm, gles,
        drm_scanner: DrmScanner::new(),
        surfaces: HashMap::new(),
        render_node,
    };

    scan_connectors(&mut device, state);
    tracing::info!("plx/udev: added {:?}", path);
    Ok((device, notifier, drm_node))
}

/// Открывает GBM-устройство на рендер-узле карты. Узел открываем НАПРЯМУЮ, а
/// не через сессию (libseat): рендер-узел не имеет отношения ни к seat'у, ни к
/// DRM master — он доступен всем на чтение-запись и переживает смену VT, в
/// отличие от card0, который сессия отбирает на паузе.
///
/// None — узла нет (проприетарный драйвер без render node, vgem и т.п.);
/// вызывающий откатывается на первичный узел.
fn open_render_gbm(render_node: &DrmNode) -> Option<GbmDevice<DrmDeviceFd>> {
    if render_node.ty() != NodeType::Render {
        return None;
    }
    let path = render_node.dev_path()?;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&path)
        .map_err(|e| tracing::warn!("plx/udev: cannot open render node {:?}: {}", path, e))
        .ok()?;
    let gbm = GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(file))))
        .map_err(|e| tracing::warn!("plx/udev: GBM on render node {:?}: {}", path, e))
        .ok()?;
    tracing::info!("plx/udev: rendering on {:?} (scanout on the primary node)", path);
    Some(gbm)
}

fn scan_connectors(device: &mut Device, state: &mut Parallax) {
    let scan = match device.drm_scanner.scan_connectors(&device.drm) {
        Ok(s) => s,
        Err(e) => { tracing::warn!("plx/udev: scan: {}", e); return; }
    };
    for event in scan {
        match event {
            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => {
                if let Err(e) = add_surface(device, state, &connector, crtc) {
                    tracing::warn!("plx/udev: add_surface: {}", e);
                }
            }
            DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => {
                if let Some(s) = device.surfaces.remove(&crtc) {
                    state.space.unmap_output(&s.output);
                    state.снять_монитор(&s.output);
                }
            }
            _ => {}
        }
    }
}

fn add_surface(
    device: &mut Device,
    state: &mut Parallax,
    connector: &connector::Info,
    crtc: crtc::Handle,
) -> Result<(), Box<dyn std::error::Error>> {
    // Имя коннектора в терминах ядра/DRM — "DP-2", "HDMI-A-1". Именно оно
    // пишется в monitor{ name = ... }: модель из EDID ("Redmi 30 HFCW") тоже
    // принимается, но коннектор стабильнее и виден в /sys/class/drm.
    let connector_name = format!("{}-{}", connector.interface().as_str(), connector.interface_id());

    let display = display_info::for_connector(&device.drm, connector.handle());
    let model = display.as_ref().and_then(|d| d.model())
        .unwrap_or_else(|| format!("{:?}", connector.interface()));
    let output_name = format!("{}-{}", model, connector.interface_id());

    // monitor{} из config.lua: ищем по имени коннектора, по модели из EDID или
    // по полному имени выхода. Первый подошедший выигрывает.
    let mon_cfg = state.lua_config.monitors.iter()
        .find(|m| m.name == connector_name || m.name == model || m.name == output_name)
        .cloned();

    // Режим, который коннектор предлагает сам: PREFERRED, иначе первый. Он же
    // страховка, если наш собственный режим железо не примет.
    let родной = connector.modes().iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .copied()
        .ok_or("no modes")?;

    // Режим: если в конфиге задан размер/частота — берём ТОЧНОЕ совпадение,
    // иначе ближайший по частоте среди подходящих по размеру. Без конфига —
    // прежнее поведение (PREFERRED, затем первый).
    //
    // Флаг «синтетический» = режим придуман нами и в EDID его нет. Такой режим
    // единственный, который вправе не подойти железу, — и только для него ниже
    // предусмотрен откат на `родной`.
    let (mode_info, синтетический) = match &mon_cfg {
        Some(cfg) if cfg.width > 0 && cfg.height > 0 => {
            let подходящие: Vec<_> = connector.modes().iter()
                .filter(|m| m.size().0 as i32 == cfg.width && m.size().1 as i32 == cfg.height)
                .collect();
            let выбран = if cfg.refresh > 0 {
                подходящие.iter()
                    .min_by_key(|m| (m.vrefresh() as i32 - cfg.refresh).abs())
                    .copied()
            } else {
                подходящие.iter()
                    .max_by_key(|m| m.vrefresh())
                    .copied()
            };
            match выбран {
                Some(m) => {
                    tracing::info!(
                        "plx/udev: {}: mode from the config {}x{}@{}Hz",
                        connector_name, m.size().0, m.size().1, m.vrefresh(),
                    );
                    (*m, false)
                }
                // Такого режима у коннектора нет — строим его сами.
                //
                // Раньше здесь был warn и молчаливый возврат на PREFERRED, и на
                // встроенной панели это означало «FullHD получить нельзя»:
                // eDP отдаёт РОВНО один режим, свой родной, и никакой другой в
                // списке не появится никогда. Между тем меньший режим панель
                // прекрасно показывает — его растягивает панельный скейлер в
                // самом дисплейном контроллере, и композитору достаётся вчетверо
                // меньше пикселей (см. src/mode.rs).
                //
                // Вверх не масштабируем: больше физической матрицы всё равно не
                // покажешь, и такой режим железо просто отвергнет — незачем
                // тратить на это модесет.
                None if cfg.width <= родной.size().0 as i32
                     && cfg.height <= родной.size().1 as i32 => {
                    // Частоту, если её не задали, берём у родного режима: панель
                    // всё равно работает на своей, и рассинхрон в логе только
                    // путал бы.
                    let hz = if cfg.refresh > 0 { cfg.refresh as f64 } else { родной.vrefresh() as f64 };
                    let свой = crate::mode::cvt(cfg.width as u16, cfg.height as u16, hz);
                    tracing::info!(
                        "plx/udev: {}: режима {}x{} в EDID нет — синтезирую CVT {}x{}@{}Hz \
                         (панель {}x{} растянет его сама)",
                        connector_name, cfg.width, cfg.height,
                        свой.size().0, свой.size().1, свой.vrefresh(),
                        родной.size().0, родной.size().1,
                    );
                    (свой, true)
                }
                None => {
                    tracing::warn!(
                        "plx/udev: {}: {}x{} is larger than the physical panel {}x{}, using PREFERRED",
                        connector_name, cfg.width, cfg.height,
                        родной.size().0, родной.size().1,
                    );
                    (родной, false)
                }
            }
        }
        // Задана только частота: тот же размер, что у PREFERRED, но с нужным Hz.
        Some(cfg) if cfg.refresh > 0 => {
            let м = connector.modes().iter()
                .filter(|m| m.size() == родной.size())
                .min_by_key(|m| (m.vrefresh() as i32 - cfg.refresh).abs())
                .copied()
                .unwrap_or(родной);
            (м, false)
        }
        _ => (родной, false),
    };

    // Модесет пробуем ЗДЕСЬ, до создания wl_output: только теперь известно,
    // какой режим железо действительно приняло, а объявлять клиентам режим,
    // которого не будет, нельзя.
    //
    // Синтетический режим — единственный, который может не подойти: ядро
    // прогоняет его через drm_mode_validate_* и atomic TEST_ONLY, и отказ здесь
    // штатный исход, а не ошибка запуска. Молча возвращаемся на родной, иначе
    // одна строка в config.lua оставляла бы человека без экрана вовсе.
    let (mode_info, drm_surface) =
        match device.drm.create_surface(crtc, mode_info, &[connector.handle()]) {
            Ok(s) => (mode_info, s),
            Err(e) if синтетический => {
                tracing::warn!(
                    "plx/udev: {}: железо отвергло синтезированный {}x{} ({:?}) — \
                     возвращаюсь на родной {}x{}@{}Hz",
                    connector_name, mode_info.size().0, mode_info.size().1, e,
                    родной.size().0, родной.size().1, родной.vrefresh(),
                );
                let s = device.drm.create_surface(crtc, родной, &[connector.handle()])?;
                (родной, s)
            }
            Err(e) => return Err(e.into()),
        };

    let wl_mode = Mode {
        size: (mode_info.size().0 as i32, mode_info.size().1 as i32).into(),
        refresh: (mode_info.vrefresh() * 1000) as i32,
    };

    let transform = match mon_cfg.as_ref().map(|c| c.transform.as_str()) {
        Some("90") => Transform::_90,
        Some("180") => Transform::_180,
        Some("270") => Transform::_270,
        Some("flipped") => Transform::Flipped,
        Some("flipped-90") => Transform::Flipped90,
        Some("flipped-180") => Transform::Flipped180,
        Some("flipped-270") => Transform::Flipped270,
        _ => Transform::Normal,
    };
    // `monitor{ x =, y = }` — место монитора В РАСКЛАДКЕ (кто слева, кто
    // справа), а не позиция выхода в space: ту задаёт камера. Подробности —
    // ниже, у `дом`.

    // scale = ... из monitor{}: логический размер стола делится на масштаб, то
    // есть на 4K-панели scale = 2.0 даёт стол 1920×1080 — «FullHD» по размеру
    // интерфейса при родном режиме 3840×2160. Сканаут при этом остаётся 4K.
    //
    // Раньше это поле парсилось в config.rs и никем не читалось: масштаб выхода
    // был занят зумом холста. Зум с него снят (см. apply_camera в state.rs —
    // «ЗУМ БОЛЬШЕ НЕ ЕДЕТ ЧЕРЕЗ МАСШТАБ ВЫХОДА», теперь он только в
    // RescaleRenderElement при отрисовке), так что scale снова отвечает за DPI
    // и только за него — умножать на зум ничего не надо.
    //
    // Целые значения отдаём как Integer: wl_output и так умеет только целые, а
    // Fractional(2.0) вдобавок к тому же числу заставил бы клиентов тянуть
    // wp_fractional_scale ради ровно того же результата.
    let scale_val = mon_cfg.as_ref().map(|c| c.scale).unwrap_or(1.0);
    let scale = if scale_val.fract() == 0.0 {
        smithay::output::Scale::Integer(scale_val as i32)
    } else {
        smithay::output::Scale::Fractional(scale_val)
    };
    if scale_val != 1.0 {
        tracing::info!(
            "plx/udev: {}: output scale {} → logical desktop {}×{}",
            connector_name, scale_val,
            (wl_mode.size.w as f64 / scale_val).round() as i32,
            (wl_mode.size.h as f64 / scale_val).round() as i32,
        );
    }

    let output = Output::new(output_name.clone(), PhysicalProperties {
        size: connector.size().map(|(w,h)| (w as i32, h as i32)).unwrap_or((0,0)).into(),
        subpixel: Subpixel::Unknown,
        make: "Unknown".into(),
        model: model.clone(),
        serial_number: "Unknown".into(),
    });
    let _global = output.create_global::<Parallax>(&state.display_handle);
    // Позиция ВЫХОДА В SPACE — это камера (см. Parallax::apply_camera), а не место
    // монитора в раскладке: холст бесконечен, и «где стоит монитор» задаётся
    // отдельно (Монитор::раскладка). Поэтому в map_output идёт дом монитора —
    // угол его собственного прямоугольника холста, — а `monitor{ x =, y = }`
    // уходит в раскладку, ровно как в hyprland. Раньше сюда шёл `position` из
    // конфига, и заданный там сдвиг молча дрался с камерой: первый же
    // apply_camera его затирал.
    let дом = state.свободный_дом();
    // Место в раскладке: из `monitor{ x =, y = }`, иначе справа от самого
    // правого — то же правило, что `auto` в hyprland. Считается ЗДЕСЬ (раньше
    // было ниже, после change_current_state) — geometry-позиция wl_output
    // нужна уже сейчас.
    let раскладка: smithay::utils::Point<i32, smithay::utils::Logical> =
        match mon_cfg.as_ref().filter(|c| c.layout_set) {
            Some(c) => (c.x, c.y).into(),
            None => state.авто_раскладка(),
        };
    // wl_output.geometry (позиция, которую change_current_state рассылает по
    // протоколу) — РАСКЛАДКА, а не дом. Нативным Wayland-клиентам эта позиция
    // почти безразлична (место им назначает compositor через xdg_surface
    // configure), а вот Xwayland строит по ней СВОЙ RandR root-экран и берёт
    // её буквально, как физическую координату. Дом второго монитора на холсте
    // разнесён на `ШАГ_ДОМА` = 1 000 000 — величина, которую X11/RandR не
    // умеет хранить (CRTC x/y — 16-битные, 0..65535): Xwayland тихо брал её по
    // модулю 65536 (замер 27.08.2026: дом (1000000,0) → CRTC оказался на
    // x=16960 = 1000000 mod 65536, а root-экран раздувался вслед за этим до
    // 19520 вместо разумных чисел). Итог — root Xwayland жил не там, где parallax
    // рисует монитор, и указатель после определённых координат зажимался на
    // мусорный край («в Dota 2 после определённых координат курсор не
    // работает» — она всегда полноэкранная, то есть всегда X11). `дом` в
    // `map_output` не трогаем: он нужен внутренней арифметике камеры/тайлинга
    // и раздельного хранения от geometry не боится (Space держит своё
    // положение выхода отдельно от Output::current_state).
    output.change_current_state(Some(wl_mode), Some(transform), Some(scale), Some(раскладка));
    output.set_preferred(wl_mode);
    state.space.map_output(&output, дом);

    // Отдельный выход ТОЛЬКО для layer-поверхностей (обои, панели, меню plx-wall).
    //
    // Зум холста у parallax сделан через output scale, а логический размер выхода
    // делится на масштаб — то есть на «птичьем глазе» (zoom 0.2) LayerMap
    // выдавал обоям размер 12800×5400 и требовал от клиента отрисовать буфер в
    // 25 раз больше экрана. plx-wall на этом просто ложился, обои и меню исчезали.
    // У этого выхода масштаб всегда 1, поэтому слои всегда считаются в
    // ЭКРАННЫХ пикселях, независимо от зума. Глобал ему не создаём: клиенты о
    // нём не знают и знать не должны, он нужен только как ключ для LayerMap.
    let layer_output = Output::new(format!("{}-layers", output_name), PhysicalProperties {
        size: connector.size().map(|(w,h)| (w as i32, h as i32)).unwrap_or((0,0)).into(),
        subpixel: Subpixel::Unknown,
        make: "Unknown".into(),
        model,
        serial_number: "Unknown".into(),
    });
    layer_output.change_current_state(
        Some(wl_mode),
        Some(Transform::Normal),
        Some(smithay::output::Scale::Integer(1)),
        Some((0, 0).into()),
    );
    layer_output.set_preferred(wl_mode);

    // ── Запись монитора ──────────────────────────────────────────────────────
    // Раньше здесь стояло `state.layer_output = Some(layer_output)` — ОДНО
    // поле на весь компоновщик. Второй коннектор его затирал, и обои с панелью
    // пропадали на обоих мониторах: слои лежали в карте первого выхода, а
    // спрашивали их у второго («на выходе ...-layers нет слоя Background»).
    // Теперь призрак слоёв принадлежит монитору (см. src/monitors.rs).
    let логический = smithay::utils::Size::<i32, smithay::utils::Logical>::from((
        (wl_mode.size.w as f64 / scale_val).round() as i32,
        (wl_mode.size.h as f64 / scale_val).round() as i32,
    ));
    // `раскладка` уже посчитана выше (нужна была для wl_output.geometry).
    let первый = state.мониторы.is_empty();
    let mut вид = crate::state::Viewport::default();
    вид.cam_x = дом.x as f64;
    вид.cam_y = дом.y as f64;
    // Стол монитора: заданный в `monitor{ tag = N }`, иначе N-й по счёту —
    // монитор 1 открывает стол 1, монитор 2 стол 2, и так далее. В hyprland то
    // же самое делает правило по умолчанию «первый свободный воркспейс».
    let тег = state.свободный_тег(mon_cfg.as_ref().map(|c| c.tag).unwrap_or(0));
    вид.tagset = [тег, тег];
    let индекс = state.мониторы.len();
    state.мониторы.push(crate::monitors::Монитор {
        output: output.clone(),
        layer_output: layer_output.clone(),
        коннектор: connector_name.clone(),
        размер: логический,
        раскладка,
        дом,
        viewport: вид,
        // Монитор поднимается уже на своём столе — обои обязаны стоять там же,
        // а не приезжать туда с первым же кадром.
        обои: crate::monitors::СлайдОбоев::новый(crate::monitors::стол_обоев(тег)),
    });
    state.закрепить_стол(тег, индекс);
    state.visited_tags.insert(тег);
    state.tag_cameras.insert(тег, (дом.x as f64, дом.y as f64, 1.0));
    tracing::info!(
        "plx/monitors: {} → monitor {} workspace {:#b} home ({},{}) layout ({},{}) {}×{}",
        connector_name, индекс, тег, дом.x, дом.y,
        раскладка.x, раскладка.y, логический.w, логический.h,
    );
    if первый {
        // Первый монитор — он же активный: его вид и есть Parallax::viewport.
        state.активный = 0;
        state.viewport = вид;
        state.layer_output = Some(layer_output);
        state.pointer_location = smithay::utils::Point::from((
            дом.x as f64 + логический.w as f64 / 2.0,
            дом.y as f64 + логический.h as f64 / 2.0,
        ));
        state.pointer_warped();
    }
    // `monitor{ primary = true }` — этот монитор обязан стать активным, даже
    // если DRM отдал его коннектор не первым (порядок сканирования не
    // постоянен, см. MonitorConfig::primary). Заявка приходит ПОСЛЕ ветки
    // `первый` намеренно: если основной монитор увиделся вторым, здесь мы
    // переключаемся на него, забирая курсор с временно активного первого.
    if !первый && mon_cfg.as_ref().is_some_and(|c| c.primary) {
        state.активировать_монитор(индекс);
        state.pointer_location = smithay::utils::Point::from((
            дом.x as f64 + логический.w as f64 / 2.0,
            дом.y as f64 + логический.h as f64 / 2.0,
        ));
        state.pointer_warped();
    }
    if первый {
        // Экрану впервые есть что показать — отсюда и начинается появление
        // рабочего места. Место ниже ветки `primary` намеренно: заявка
        // основного монитора подменяет активный вид целиком, и начатый до неё
        // вход она бы обнулила.
        state.начать_вход();
    }
    state.apply_camera();

    let allocator = GbmAllocator::new(
        device.gbm.clone(),
        GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
    );
    let exporter = GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into());
    let color_formats = [Fourcc::Xrgb8888, Fourcc::Argb8888];

    let compositor = DrmCompositor::new(
        &output, drm_surface, None, allocator, exporter,
        color_formats.iter().copied(),
        device.gles.egl_context().dmabuf_render_formats().clone(),
        device.drm.cursor_size(),
        Some(device.gbm.clone()),
    )?;

    let damage_tracker = OutputDamageTracker::from_output(&output);
    device.surfaces.insert(crtc, Surface {
        output, compositor, damage_tracker,
        frame_queued: false,
        frame_queued_at: std::time::Instant::now(),
        last_elements: 0,
        rounded: crate::rounded::Шейдер::new(&mut device.gles),
        blur: crate::blur::Блюр::new(&mut device.gles),
        отказов_подряд: 0,
        отказ_до: None,
        отказ_лог: None,
    });
    // Тот же шейдер скругления, но доступный из сборки элементов панели: она
    // получает `state`, а не `Surface`. Компилируется один раз на выход и
    // потом только клонируется (внутри — Arc на программу).
    if state.blur_shape.is_none() {
        state.blur_shape = crate::rounded::Шейдер::new(&mut device.gles);
    }
    tracing::info!("plx/udev: output '{}' {}x{}@{}Hz",
        output_name, wl_mode.size.w, wl_mode.size.h, wl_mode.refresh/1000);
    Ok(())
}

/// Строит render-элементы миникарты (Module 3): фоновая панель, окна как
/// прямоугольники, рамка текущего viewport — всё в фиксированных физических
/// координатах экрана (независимо от zoom холста, как курсор).
/// Rubber-band рамка выделения (в процессе протяжки) + подсветка уже
/// выделенных окон (Super+G группирует их в "созвездие") — рисуются поверх
/// окон полупрозрачными заливками, тем же приёмом, что и Focus Aura/фон портала.
///
/// Выделенное окно раньше заливалось сплошным оранжевым `[1.0, 0.7, 0.2, 0.22]`
/// во всю площадь: поверх обоев это читалось не как «окно выбрано», а как «окно
/// перекрасили», и тем сильнее, чем окно больше. Теперь заливка почти
/// невидимая и нейтральная, а работу делает тонкий светлый кант по краю —
/// заметный ровно там, где взгляд ищет границу выбора.
fn build_selection_elements(state: &mut Parallax) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if state.selected_windows.is_empty() && state.selection_drag.is_none() {
        return elements;
    }

    /// Плёнка и кант выделенного окна.
    const SELECT_FILL: [f32; 4] = [0.82, 0.87, 0.96, 0.06];
    const SELECT_RIM: [f32; 4] = [0.86, 0.91, 1.0, 0.45];
    /// Рамка протяжки — тот же холодный тон, что у подсказки вставки в Columns.
    const BAND_FILL: [f32; 4] = [0.35, 0.6, 1.0, 0.10];
    const BAND_RIM: [f32; 4] = [0.55, 0.75, 1.0, 0.55];
    /// Толщина канта в логических px: на экране умножается на zoom, но тоньше
    /// пикселя не бывает — иначе на отдалённой камере выделение просто исчезает.
    const RIM_LOGICAL: f64 = 1.5;

    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    let zoom = state.viewport.zoom;
    let rim = ((RIM_LOGICAL * zoom).round() as i32).clamp(1, 6);

    // Геометрию собираем до заимствования пула: space принадлежит тому же state.
    let mut rects: Vec<((i32, i32), (i32, i32), [f32; 4])> = Vec::new();
    // Плёнка + четыре полоски канта. Полоски идут ПОСЛЕ заливки, чтобы кант
    // ложился поверх неё, а не смешивался под ней.
    let mut рамка = |x: i32, y: i32, w: i32, h: i32, fill: [f32; 4], edge: [f32; 4]| {
        rects.push(((x, y), (w, h), fill));
        let t = rim.min(w.max(1)).min(h.max(1));
        rects.push(((x, y), (w, t), edge));
        rects.push(((x, y + h - t), (w, t), edge));
        let inner = h - 2 * t;
        if inner > 0 {
            rects.push(((x, y + t), (t, inner), edge));
            rects.push(((x + w - t, y + t), (t, inner), edge));
        }
    };

    for window in &state.selected_windows {
        let geo = match state.space.element_geometry(window) { Some(g) => g, None => continue };
        let x = ((geo.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y = ((geo.loc.y as f64 - cam_y) * zoom).round() as i32;
        let w = ((geo.size.w as f64 * zoom).round() as i32).max(1);
        let h = ((geo.size.h as f64 * zoom).round() as i32).max(1);
        рамка(x, y, w, h, SELECT_FILL, SELECT_RIM);
    }

    if let Some(rect) = state.selection_drag {
        let x = ((rect.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y = ((rect.loc.y as f64 - cam_y) * zoom).round() as i32;
        let w = ((rect.size.w as f64 * zoom).round() as i32).max(1);
        let h = ((rect.size.h as f64 * zoom).round() as i32).max(1);
        рамка(x, y, w, h, BAND_FILL, BAND_RIM);
    }

    let pool = &mut state.selection_ids;
    let mut idx = 0usize;
    for (loc, size, color) in rects {
        elements.push(pooled_solid(pool, &mut idx, loc, size, color));
    }

    elements
}

/// Подсказка группового драга: куда встанут окна выделения/созвездия.
///
/// Только контуры, без заливки. Заливка здесь была бы вредна: окна группы под
/// подсказкой ЖИВЫЕ (они уже едут вместе с перетаскиваемым), и плёнка поверх
/// них просто притушила бы содержимое, ничего не сообщив. Контур же добавляет
/// именно то, чего не видно, — границу грозди целиком, включая ту её часть,
/// что ушла за край экрана или под чужое окно.
///
/// Последний прямоугольник в `призраки_группы` — общая рамка вокруг всей
/// грозди (см. `MoveSurfaceGrab::обновить_призраков`), и рисуется он ярче.
fn build_ghost_elements(state: &mut Parallax) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if state.призраки_группы.is_empty() {
        return elements;
    }
    /// Контур члена грозди и контур всей грозди.
    const ЧЛЕН: [f32; 4] = [0.86, 0.91, 1.0, 0.40];
    const ГРОЗДЬ: [f32; 4] = [0.62, 0.80, 1.0, 0.75];
    const ТОЛЩИНА: f64 = 1.5;

    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    let zoom = state.viewport.zoom;
    let t = ((ТОЛЩИНА * zoom).round() as i32).clamp(1, 6);
    let последний = state.призраки_группы.len() - 1;

    let mut rects: Vec<((i32, i32), (i32, i32), [f32; 4])> = Vec::new();
    for (i, r) in state.призраки_группы.iter().enumerate() {
        let цвет = if i == последний { ГРОЗДЬ } else { ЧЛЕН };
        let x = ((r.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y = ((r.loc.y as f64 - cam_y) * zoom).round() as i32;
        let w = ((r.size.w as f64 * zoom).round() as i32).max(1);
        let h = ((r.size.h as f64 * zoom).round() as i32).max(1);
        let t = t.min(w).min(h);
        rects.push(((x, y), (w, t), цвет));
        rects.push(((x, y + h - t), (w, t), цвет));
        let inner = h - 2 * t;
        if inner > 0 {
            rects.push(((x, y + t), (t, inner), цвет));
            rects.push(((x + w - t, y + t), (t, inner), цвет));
        }
    }

    let pool = &mut state.ghost_ids;
    let mut idx = 0usize;
    for (loc, size, color) in rects {
        elements.push(pooled_solid(pool, &mut idx, loc, size, color));
    }
    elements
}

/// Снять кадр для потока демонстрации и отдать его в PipeWire.
///
/// Экран снимается целиком (это же ровно то, что видит пользователь), а для
/// выбранного ОКНА кадр вырезается по его текущему месту на экране. Размер
/// потока зафиксирован при старте: PipeWire согласует формат один раз, поэтому
/// уехавшее или изменившее размер окно даёт частично чёрный кадр, а не рассыпавшийся
/// поток.
fn push_cast_frame<E>(
    state: &mut Parallax,
    output: &Output,
    renderer: &mut GlesRenderer,
    elements: &[E],
) where
    E: smithay::backend::renderer::element::RenderElement<GlesRenderer>,
{
    let Some(mode) = output.current_mode() else { return };
    let screen: Size<i32, smithay::utils::Buffer> = (mode.size.w, mode.size.h).into();
    // Кадр отдаёт ТОЛЬКО выход выбранного источника. Без этой проверки на двух
    // мониторах в один поток по очереди уезжали кадры обоих экранов: функция
    // зовётся из отрисовки каждого выхода, а `due()` пропускает первого, кто
    // успел. Собеседник в Discord видел мигающую склейку двух рабочих столов.
    if !state.cast_output_matches(output) {
        return;
    }
    let Some(cast) = state.portal_cast.as_ref() else { return };
    let (cw, ch) = (cast.width as i32, cast.height as i32);
    // Прямоугольник, который уйдёт в поток, в экранных пикселях.
    let crop = match &cast.source {
        crate::portal::Capture::Output(_) => None,
        crate::portal::Capture::Window(window) => {
            let zoom = state.viewport.zoom;
            let geo = state.space.element_geometry(window);
            geo.map(|g| (
                ((g.loc.x as f64 - state.viewport.cam_x) * zoom).round() as i32,
                ((g.loc.y as f64 - state.viewport.cam_y) * zoom).round() as i32,
            ))
        }
    };

    let Some(shot) = crate::screencopy::capture(renderer, output, elements, screen) else {
        return;
    };

    let frame = match crop {
        // Весь экран: если размеры совпали — отдаём как есть.
        None if screen.w == cw && screen.h == ch => shot,
        // Иначе вырезаем окно (или подгоняем экран под согласованный размер).
        _ => {
            let (ox, oy) = crop.unwrap_or((0, 0));
            let mut out = vec![0u8; (cw * ch * 4) as usize];
            for y in 0..ch {
                let sy = oy + y;
                if sy < 0 || sy >= screen.h {
                    continue;
                }
                // Пересечение строки окна с экраном — копируем одним куском.
                let x0 = ox.max(0);
                let x1 = (ox + cw).min(screen.w);
                if x1 <= x0 {
                    continue;
                }
                let src = ((sy * screen.w + x0) * 4) as usize;
                let dst = ((y * cw + (x0 - ox)) * 4) as usize;
                let len = ((x1 - x0) * 4) as usize;
                out[dst..dst + len].copy_from_slice(&shot[src..src + len]);
            }
            out
        }
    };

    if let Some(cast) = state.portal_cast.as_mut() {
        cast.push(frame);
    }
}

/// Подсветка выбора источника для демонстрации экрана (portal.rs).
///
/// Пока портал ждёт ответа, подсвечивается то, что уйдёт в поток: окно под
/// курсором — рамкой по его границам, пустой холст — рамкой по всему экрану.
///
/// **Рисуется на КАЖДОМ мониторе, и это здесь главное.** Раньше рамка «весь
/// экран» строилась от `screen_size()` активного монитора и уезжала в кадр
/// обоих выходов разом: на соседнем экране она оказывалась чужого размера и
/// ничего осмысленного не подсвечивала, а сам соседний монитор выбрать было
/// нечем. Теперь каждый выход подписан своим номером («Монитор 1», «Монитор
/// 2»), и цифра на клавиатуре выбирает его напрямую (`portal_pick_monitor`) —
/// не перевозя стрелку и не заводя новых биндов.
fn build_portal_pick_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if !state.portal_picking() {
        return elements;
    }
    let Some(mode) = output.current_mode() else { return elements };
    let (ширина, высота) = (mode.size.w, mode.size.h);
    // Номер рисуемого монитора и тот ли это, где стрелка. Окно подсвечиваем
    // только на своём: `pointer_location` — точка ХОЛСТА, и на чужом выходе
    // окно под курсором лежит за миллион пикселей отсюда.
    let номер = state.монитор_по_выходу(output).unwrap_or(0);
    let свой = state.монитор_по_выходу(output).is_none_or(|i| i == state.курсор_монитор);
    // Клиент мог попросить только мониторы (OBS шлёт `типы=1`) — тогда окна не
    // подсвечиваем вовсе, иначе подсветка обещала бы то, чего выбор не отдаст.
    let окна_можно = state.portal_pick_types() & 2 != 0;

    let окно = (свой && окна_можно)
        .then(|| {
            state.space.element_under(state.pointer_location)
                .and_then(|(w, _)| state.space.element_geometry(w))
        })
        .flatten();
    let (x, y, w, h) = match окно {
        Some(geo) => {
            let zoom = state.viewport.zoom;
            (
                ((geo.loc.x as f64 - state.viewport.cam_x) * zoom).round() as i32,
                ((geo.loc.y as f64 - state.viewport.cam_y) * zoom).round() as i32,
                ((geo.size.w as f64 * zoom).round() as i32).max(1),
                ((geo.size.h as f64 * zoom).round() as i32).max(1),
            )
        }
        None => (0, 0, ширина, высота),
    };

    const РАМКА: i32 = 4;
    // Цвет ОБЫЧНЫЙ (straight) — домножает на альфу `pooled_solid`. Раньше эти
    // две константы были домножены вручную (единственное место в файле, где
    // правило соблюдали); теперь домножение общее, и хранить их надо как все.
    const ЦВЕТ: [f32; 4] = [0.35, 0.75, 1.0, 0.85];
    const ЗАЛИВКА: [f32; 4] = [0.33, 0.73, 1.0, 0.15];
    // Экран без стрелки подсвечен слабее: он не «под курсором», но выбрать его
    // цифрой можно — совсем гасить его значило бы спрятать эту возможность.
    let тускло = |c: [f32; 4]| [c[0], c[1], c[2], c[3] * 0.45];
    let (цвет, заливка) = if свой { (ЦВЕТ, ЗАЛИВКА) } else { (тускло(ЦВЕТ), тускло(ЗАЛИВКА)) };

    {
        let pool = &mut state.portal_pick_ids;
        let mut idx = 0usize;
        // Четыре полосы рамки + лёгкая заливка: сплошными прямоугольниками, как
        // остальные оверлеи parallax (своего шейдера у нас нет).
        elements.push(pooled_solid(pool, &mut idx, (x, y), (w, РАМКА), цвет));
        elements.push(pooled_solid(pool, &mut idx, (x, y + h - РАМКА), (w, РАМКА), цвет));
        elements.push(pooled_solid(pool, &mut idx, (x, y), (РАМКА, h), цвет));
        elements.push(pooled_solid(pool, &mut idx, (x + w - РАМКА, y), (РАМКА, h), цвет));
        elements.push(pooled_solid(pool, &mut idx, (x, y), (w, h), заливка));
    }

    // ── Подпись ──────────────────────────────────────────────────────────────
    // Плашка по центру экрана: что именно выбирается и чем выбрать. Тексты
    // короткие намеренно — плашка висит поверх чужого рабочего стола, и читать
    // её человек будет одну-две секунды.
    let одинокий = state.мониторы.len() < 2;
    let заголовок = match (&окно, одинокий) {
        (Some(_), _) => т!("Окно под курсором", "Window under the cursor").to_string(),
        (None, true) => т!("Весь экран", "Whole screen").to_string(),
        (None, false) => тф!("Монитор {}", "Monitor {}", номер + 1),
    };
    let подсказка = if одинокий {
        т!("ЛКМ — показать, ПКМ или Esc — отмена", "LMB — share, RMB or Esc — cancel").to_string()
    } else {
        тф!(
            "ЛКМ — показать · {} — монитор · ПКМ/Esc — отмена", "LMB — share · {} — monitor · RMB/Esc — cancel",
            (1..=state.мониторы.len()).map(|n| n.to_string()).collect::<Vec<_>>().join("/"),
        )
    };
    const МАСШТАБ_З: i32 = 3;
    const МАСШТАБ_П: i32 = 2;
    let ш_з = crate::text::width_of(&заголовок, crate::text::Weight::Semi, МАСШТАБ_З);
    let ш_п = crate::text::width(&подсказка, МАСШТАБ_П);
    let в_з = crate::text::height(МАСШТАБ_З);
    let в_п = crate::text::height(МАСШТАБ_П);
    const ОТСТУП: i32 = 18;
    const ЗАЗОР: i32 = 8;
    let пш = ш_з.max(ш_п) + ОТСТУП * 2;
    let пв = в_з + ЗАЗОР + в_п + ОТСТУП * 2;
    let пx = (ширина - пш) / 2;
    let пy = (высота - пв) / 2;
    {
        let pool = &mut state.portal_pick_ids;
        let mut idx = 5usize;
        elements.push(pooled_solid(
            pool, &mut idx, (пx, пy), (пш, пв), [0.05, 0.07, 0.09, if свой { 0.82 } else { 0.5 }],
        ));
    }
    let текст = if свой { 1.0 } else { 0.6 };
    draw_text_w(
        state, renderer, пx + (пш - ш_з) / 2, пy + ОТСТУП, &заголовок,
        crate::text::Weight::Semi, МАСШТАБ_З, [0.92, 0.96, 1.0, текст], 0, &mut elements,
    );
    draw_text(
        state, renderer, пx + (пш - ш_п) / 2, пy + ОТСТУП + в_з + ЗАЗОР, &подсказка,
        МАСШТАБ_П, [0.72, 0.80, 0.88, текст], 1, &mut elements,
    );
    elements
}

/// Затемнение экрана и рамка выделения под снимок области (PrtScr, snip.rs).
///
/// Затемняются ВСЕ мониторы — пока идёт выделение, ничего другого на экранах не
/// происходит. А вот рамка живёт только на своём выходе: кадр снимается с
/// одного монитора (см. `snip_finish`), и растянутая на два экрана рамка
/// обещала бы склейку, которой не будет.
///
/// Затемнение кладётся не одним прямоугольником с дыркой (сплошной элемент
/// дырок не умеет), а четырьмя полосами вокруг выделения: так внутри рамки
/// остаётся неискажённая картинка — по ней и целятся.
fn build_snip_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if !state.snip_идёт() {
        return elements;
    }
    let Some(mode) = output.current_mode() else { return elements };
    let (ширина, высота) = (mode.size.w, mode.size.h);
    let номер = state.монитор_по_выходу(output);
    let свой = номер.is_none_or(|i| state.snip.as_ref().is_some_and(|в| в.монитор == i));

    // Рамка в экранных пикселях ЭТОГО выхода. Считается ровно тем же
    // преобразованием, что и в `snip_finish`, — иначе снимок уехал бы
    // относительно того, что человек обвёл.
    let рамка = свой
        .then(|| {
            let начало = state.snip.as_ref()?.начало?;
            let вид = match номер {
                Some(i) if i != state.активный => state.мониторы.get(i)?.viewport.clone(),
                _ => state.viewport.clone(),
            };
            let zoom = вид.zoom.max(0.01);
            let в_экран = |p: Point<f64, Logical>| ((p.x - вид.cam_x) * zoom, (p.y - вид.cam_y) * zoom);
            let (x0, y0) = в_экран(начало);
            let (x1, y1) = в_экран(state.pointer_location);
            let (x, y) = (x0.min(x1).round() as i32, y0.min(y1).round() as i32);
            let (w, h) = ((x1 - x0).abs().round() as i32, (y1 - y0).abs().round() as i32);
            (w > 0 && h > 0).then_some((x, y, w, h))
        })
        .flatten();

    // Цвета ОБЫЧНЫЕ (straight): `pooled_solid` домножает на альфу сам.
    const ТЕНЬ: [f32; 4] = [0.0, 0.0, 0.0, 0.45];
    const ЦВЕТ: [f32; 4] = [0.35, 0.75, 1.0, 0.9];
    const ТОЛЩИНА: i32 = 2;

    let pool = &mut state.snip_ids;
    let mut idx = 0usize;
    match рамка {
        None => {
            elements.push(pooled_solid(pool, &mut idx, (0, 0), (ширина, высота), ТЕНЬ));
        }
        Some((x, y, w, h)) => {
            let (x, y) = (x.clamp(0, ширина), y.clamp(0, высота));
            let (w, h) = (w.min(ширина - x), h.min(высота - y));
            elements.push(pooled_solid(pool, &mut idx, (0, 0), (ширина, y), ТЕНЬ));
            elements.push(pooled_solid(pool, &mut idx, (0, y + h), (ширина, высота - y - h), ТЕНЬ));
            elements.push(pooled_solid(pool, &mut idx, (0, y), (x, h), ТЕНЬ));
            elements.push(pooled_solid(pool, &mut idx, (x + w, y), (ширина - x - w, h), ТЕНЬ));
            // Рамка рисуется ВНУТРЬ выделения: снаружи она легла бы на
            // затемнение и визуально сдвинула границу на пару пикселей.
            elements.push(pooled_solid(pool, &mut idx, (x, y), (w, ТОЛЩИНА), ЦВЕТ));
            elements.push(pooled_solid(pool, &mut idx, (x, y + h - ТОЛЩИНА), (w, ТОЛЩИНА), ЦВЕТ));
            elements.push(pooled_solid(pool, &mut idx, (x, y), (ТОЛЩИНА, h), ЦВЕТ));
            elements.push(pooled_solid(pool, &mut idx, (x + w - ТОЛЩИНА, y), (ТОЛЩИНА, h), ЦВЕТ));
        }
    }

    // Подпись: пока рамку не начали — что делать, дальше — размер выделения.
    // Размер читают на лету, поэтому он висит над рамкой, а не в центре экрана.
    const МАСШТАБ: i32 = 2;
    let (подпись, пx, пy) = match рамка {
        None if свой => (
            т!("Обведите область · клик — весь экран · Esc — отмена", "Drag out a region · click — whole screen · Esc — cancel").to_string(),
            None,
            высота / 12,
        ),
        None => return elements,
        Some((x, y, w, h)) => {
            let текст = format!("{}×{}", w, h);
            let ш = crate::text::width_of(&текст, crate::text::Weight::Semi, МАСШТАБ);
            let в = crate::text::height(МАСШТАБ);
            // Над рамкой, а если она у верхнего края — внутри неё.
            let сверху = y - в - 14;
            (текст, Some((x + w / 2 - ш / 2, ш)), if сверху >= 0 { сверху } else { y + 6 })
        }
    };
    let ш = crate::text::width_of(&подпись, crate::text::Weight::Semi, МАСШТАБ);
    let в = crate::text::height(МАСШТАБ);
    let пx = пx.map(|(x, _)| x.clamp(4, (ширина - ш - 4).max(4))).unwrap_or((ширина - ш) / 2);
    const ОТСТУП: i32 = 8;
    {
        let pool = &mut state.snip_ids;
        let mut idx = 16usize;
        elements.push(pooled_solid(
            pool, &mut idx,
            (пx - ОТСТУП, пy - ОТСТУП / 2), (ш + ОТСТУП * 2, в + ОТСТУП),
            [0.05, 0.07, 0.09, 0.75],
        ));
    }
    draw_text_w(
        state, renderer, пx, пy, &подпись,
        crate::text::Weight::Semi, МАСШТАБ, [0.92, 0.96, 1.0, 1.0], 0, &mut elements,
    );
    elements
}

/// Цифра 3×5 «пикселей» из сплошных прямоугольников, увеличенная в PX раз.
/// Своего шрифта у parallax нет, а подпись нужна крошечная — этого хватает.
///
/// Точки идут через переданную `полоса` — ту же, которой карта режет всё
/// остальное шторкой: цифра у края карточки обязана обрезаться вместе с ней, а
/// не торчать поверх стола.
fn draw_digit_clipped<F>(
    x: i32, y: i32, digit: u32, color: [f32; 4],
    кадр: Rectangle<i32, Physical>,
    полоса: &mut F,
    out: &mut Vec<OutputRenderElements>,
)
where
    F: FnMut(i32, i32, i32, i32, [f32; 4], Rectangle<i32, Physical>, &mut Vec<OutputRenderElements>),
{
    // Каждая строка — 3 бита, старший бит слева.
    const ЦИФРЫ: [[u8; 5]; 10] = [
        [0b111, 0b101, 0b101, 0b101, 0b111], // 0
        [0b010, 0b110, 0b010, 0b010, 0b111], // 1
        [0b111, 0b001, 0b111, 0b100, 0b111], // 2
        [0b111, 0b001, 0b111, 0b001, 0b111], // 3
        [0b101, 0b101, 0b111, 0b001, 0b001], // 4
        [0b111, 0b100, 0b111, 0b001, 0b111], // 5
        [0b111, 0b100, 0b111, 0b101, 0b111], // 6
        [0b111, 0b001, 0b010, 0b010, 0b010], // 7
        [0b111, 0b101, 0b111, 0b101, 0b111], // 8
        [0b111, 0b101, 0b111, 0b001, 0b111], // 9
    ];
    const PX: i32 = 2; // сторона «пикселя» цифры
    let Some(glyph) = ЦИФРЫ.get((digit % 10) as usize) else { return };
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..3i32 {
            if bits & (1 << (2 - col)) != 0 {
                полоса(x + col * PX, y + row as i32 * PX, PX, PX, color, кадр, out);
            }
        }
    }
}

/// Окно на миникарте: и проекция прямоугольника, и живая миниатюра его
/// содержимого считаются по одному и тому же набору.
struct МиникартаОкно {
    window: Window,
    /// Где стоит ВИДИМАЯ часть окна (`element_geometry`) — по ней считаются
    /// проекция, рамка фокуса и подложка.
    loc: Point<i32, Logical>,
    size: Size<i32, Logical>,
    /// Начало дерева поверхностей: `loc − geometry().loc`. У клиентов с
    /// клиентскими рамками (GTK, Electron) оно левее и выше видимой части —
    /// ровно та же пара точек, что и в render_surface.
    root: Point<i32, Logical>,
    focused: bool,
    /// Курсор сейчас над этой миниатюрой (подсветка + подпись поярче).
    hovered: bool,
    /// Заголовок окна для подписи под миниатюрой; пусто — подписи не будет.
    подпись: String,
}

fn build_minimap_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    let mode = match output.current_mode() { Some(m) => m, None => return elements };

    // Геометрия одна на всех — и на отрисовку, и на ввод (`Parallax::minimap_hit`,
    // `minimap_window_at`). Раскрытие режет ВСЁ: что не нарисовано, то и не
    // кликается.
    let g = crate::canvas::minimap_geom(mode.size);
    let видимая_часть = crate::canvas::minimap_reveal(g.panel, state.minimap_slide);
    if видимая_часть.size.h <= 0 || видимая_часть.size.w <= 0 {
        return elements;
    }
    // Проявление: карта не только распахивается от центра, но и проступает.
    // Гасится ВСЁ, включая живое содержимое окон, — альфу несёт сам источник
    // (`Window::render_elements(.., alpha)`), а не обёртки Rescale/Crop.
    let видимость = crate::canvas::minimap_fade(state.minimap_slide);
    let Some(карта) = g.content.intersection(видимая_часть) else {
        // Раскрытие ещё не дошло до области карты — видна одна плашка.
        let mut pool = std::mem::take(&mut state.minimap_ids);
        let mut idx = 0usize;
        elements.push(pooled_solid(
            &mut pool, &mut idx, (видимая_часть.loc.x, видимая_часть.loc.y),
            (видимая_часть.size.w, видимая_часть.size.h), с_альфой(PANEL_BG, видимость),
        ));
        state.minimap_ids = pool;
        return elements;
    };

    let current_tags = state.viewport.current_tags();
    let focused = state.focused_surface();
    // Под курсором — считаем ТЕМ ЖЕ методом, которым ввод решает, куда лететь
    // по клику: подсветка обязана показывать именно то окно, которое откроется.
    let наведено = state.minimap_hit().and_then(|p| state.minimap_window_at(p));
    // Окна берём из space, а не из tagged_windows: нужен ТОТ ЖЕ порядок
    // наложения, что и на холсте. space.elements() идёт снизу вверх, список
    // кадра — от переднего плана к заднему, поэтому обходим в обратном порядке
    // (как в render_surface). Живые миниатюры обязаны перекрывать друг друга
    // так же, как оригиналы, иначе карта показывает не то, что на экране.
    let окна: Vec<МиникартаОкно> = state.space.elements().rev()
        .filter(|w| state.tagged_windows.iter()
            .any(|tw| &tw.window == *w && tw.tags & current_tags != 0))
        .filter_map(|w| state.space.element_geometry(w).map(|g| МиникартаОкно {
            root: g.loc - w.geometry().loc,
            loc: g.loc,
            size: g.size,
            focused: focused.as_ref()
                .map(|fs| crate::xwin::is_surface(w, fs))
                .unwrap_or(false),
            hovered: наведено.as_ref() == Some(w),
            подпись: crate::xwin::title(w)
                .or_else(|| crate::xwin::app_id(w))
                .unwrap_or_default(),
            window: w.clone(),
        }))
        .collect();
    let windows: Vec<(Point<i32, Logical>, Size<i32, Logical>, bool)> =
        окна.iter().map(|w| (w.loc, w.size, w.focused)).collect();

    let proj = crate::canvas::project_minimap(
        &windows, state.minimap_view(), state.minimap_screen(), g.content.size,
    );
    let остриё = g.content.loc;

    // Живые миниатюры собираем ДО заимствования пула solid-слотов: дальше
    // `state` занят изменяемой ссылкой на `minimap_ids`.
    let миниатюры = build_minimap_thumbnails(renderer, &окна, &proj, остриё, карта, видимость);
    // Подписи под миниатюрами и заголовок карточки — им нужен `state` целиком
    // (кэш текста), поэтому тоже ДО пула.
    let (подписи, плашки_подписей) =
        build_minimap_labels(state, renderer, &окна, &proj, остриё, карта, видимость);
    let шапка = build_minimap_header(state, renderer, g, видимая_часть, видимость);

    // Закладки камеры читаем до заимствования пула — обе части лежат в state.
    // Берём ПАРАМИ со слотом: номер рисуется рядом с точкой, чтобы было видно,
    // какая цифра куда прыгает (Super+N в режиме закладок).
    let mut bookmarks: Vec<(u32, Point<f64, Logical>)> =
        state.camera_bookmarks.iter().map(|(s, p)| (*s, *p)).collect();
    bookmarks.sort_by_key(|(s, _)| *s);
    // Видимый кусок холста — для рамки «где я» ниже. Считаем ДО заимствования
    // пула: дальше `state` занят изменяемой ссылкой на `minimap_ids`.
    let видимое = state.visible_canvas_size();
    let камера = Point::<f64, Logical>::from((state.viewport.cam_x, state.viewport.cam_y));
    let кнопка = state.minimap_reset_button();
    let ручной = state.minimap_manual;
    // Пул забираем НАСОВСЕМ (и возвращаем в конце): после сборки solid-элементов
    // карте нужен ещё и `state` целиком — под ней лежит матовое стекло.
    let mut пул = std::mem::take(&mut state.minimap_ids);
    let pool = &mut пул;
    let mut idx = 0usize;

    // Все solid-полоски карты обрезаются по раскрытой части: рисовать за её краем нельзя,
    // а `pooled_solid` кропа не знает — режем прямоугольник заранее.
    let mut полоса = |x: i32, y: i32, w: i32, h: i32, color: [f32; 4],
                      кадр: Rectangle<i32, Physical>,
                      out: &mut Vec<OutputRenderElements>| {
        let r = Rectangle::<i32, Physical>::new(
            Point::from((x, y)), Size::from((w.max(0), h.max(0))),
        );
        if let Some(i) = r.intersection(кадр) {
            if i.size.w > 0 && i.size.h > 0 {
                out.push(pooled_solid(
                    pool, &mut idx, (i.loc.x, i.loc.y), (i.size.w, i.size.h),
                    с_альфой(color, видимость),
                ));
            }
        }
    };

    // ── Закладки камеры (bookmarks_mode): крестик на карте за каждую точку ────
    // Проецируем якорь закладки тем же bbox/scale, что и окна; рисуем крест из
    // двух перекладин. Точки вне карты (сильный зум/далеко) пропускаем.
    const CROSS_ARM: i32 = 5; // длина луча от центра, px
    const CROSS_TH: i32 = 2;  // толщина перекладины, px
    for (slot, anchor) in bookmarks {
        let p = crate::canvas::project_point_minimap(anchor, proj.bbox, proj.scale);
        if p.x < 0 || p.y < 0 || p.x >= g.content.size.w || p.y >= g.content.size.h {
            continue;
        }
        let (px, py) = (остриё.x + p.x, остриё.y + p.y);
        let color = [1.0f32, 0.30, 0.45, 0.95];
        полоса(px - CROSS_ARM, py - CROSS_TH / 2, CROSS_ARM * 2 + 1, CROSS_TH, color, карта, &mut elements);
        полоса(px - CROSS_TH / 2, py - CROSS_ARM, CROSS_TH, CROSS_ARM * 2 + 1, color, карта, &mut elements);
        // Номер слота — справа сверху от крестика, тем же цветом. Цифра из
        // точек, каждая точка идёт через ту же обрезку.
        draw_digit_clipped(
            px + CROSS_ARM + 2, py - CROSS_ARM - 1, slot, color, карта,
            &mut полоса, &mut elements,
        );
    }

    // Дальше — от переднего плана к заднему: список кадра идёт именно так, и
    // то, что добавлено позже, лежит ниже. Закладки выше всего (раньше они шли
    // последними и тонули под полупрозрачными окнами), затем шапка карточки,
    // рамки окон и подписи, затем живые миниатюры, под ними подложки окон,
    // сетка холста, обои и в самом низу плашка карточки.

    // ── Шапка карточки ───────────────────────────────────────────────────────
    elements.extend(шапка);

    // Кнопка сброса вида: сама подпись уже в шапке, здесь — её плашка.
    {
        let цвет = if ручной {
            [0.35, 0.55, 0.95, 0.85]
        } else {
            [1.0, 1.0, 1.0, 0.10]
        };
        полоса(кнопка.loc.x, кнопка.loc.y, кнопка.size.w, кнопка.size.h, цвет, видимая_часть, &mut elements);
    }

    // ── Рамки окон: фокус и наведение ────────────────────────────────────────
    // Заливкой их больше не показать: под ней живое содержимое, и сплошной
    // прямоугольник просто закрыл бы его. Рисуем рамку из четырёх полос.
    // Наведение — толще и белее фокуса: это «сейчас кликнешь сюда», и оно
    // должно читаться поверх любого содержимого окна.
    // Обычному окну достаётся тонкий контур: он и есть «здесь окно» там, где
    // содержимое прозрачно почти целиком (терминалы Ярика — все такие).
    const FOCUS_TH: i32 = 2;
    const HOVER_TH: i32 = 3;
    const EDGE_TH: i32 = 1;
    const FOCUS_COLOR: [f32; 4] = [0.35, 0.55, 0.95, 0.95];
    const HOVER_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.95];
    const EDGE_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.22];
    for (окно, b) in окна.iter().zip(proj.boxes.iter()) {
        let (th, color) = if окно.hovered {
            (HOVER_TH, HOVER_COLOR)
        } else if b.focused {
            (FOCUS_TH, FOCUS_COLOR)
        } else {
            (EDGE_TH, EDGE_COLOR)
        };
        let (x, y) = (остриё.x + b.loc.x, остриё.y + b.loc.y);
        let (w, h) = (b.size.w, b.size.h);
        for (лx, лy, лw, лh) in [
            (x, y, w, th),                // верх
            (x, y + h - th, w, th),       // низ
            (x, y, th, h),                // лево
            (x + w - th, y, th, h),       // право
        ] {
            полоса(лx, лy, лw.max(1), лh.max(1), color, карта, &mut elements);
        }
    }

    // ── Подписи-заголовки под миниатюрами ────────────────────────────────────
    elements.extend(подписи);
    for (r, цвет) in плашки_подписей {
        полоса(r.loc.x, r.loc.y, r.size.w, r.size.h, цвет, карта, &mut elements);
    }

    // ── Живое содержимое окон ────────────────────────────────────────────────
    elements.extend(миниатюры);

    // ── Контур и подложка окон ───────────────────────────────────────────────
    // Подложка нужна там, где содержимого нет: окно без буфера (ещё не прислало
    // кадр) обязано остаться видно как прямоугольник.
    //
    // **Подложка ТЁМНАЯ, и это правка 25.08.2026 по прямой жалобе «белые
    // квадраты на миникарте».** Была `[0.6,0.6,0.65,0.75]` — светло-серая, из
    // расчёта «прозрачный терминал читается на серой подложке, а не на фоне
    // карты». Но у Ярика ПРОЗРАЧНЫ ВСЕ терминалы (см. заметки про темы и
    // LD_PRELOAD-шим), поэтому серую плиту было видно сквозь каждый из них:
    // карта превращалась в набор белёсых плашек вместо живых окон. Замер в
    // харнессе (`h_trans.png`, alacritty с opacity 0.45) показывает ровно это.
    // Тёмная подложка того же семейства, что и карточка, даёт прозрачному окну
    // выглядеть в карте так же, как на настоящем столе.
    //
    // Контур каждого окна рисуется ВЫШЕ (вместе с рамками фокуса и наведения),
    // чтобы его было видно и сквозь прозрачное содержимое.
    const BACK_IDLE: [f32; 4] = [0.09, 0.10, 0.13, 0.55];
    const BACK_FOCUS: [f32; 4] = [0.10, 0.16, 0.28, 0.65];
    for b in &proj.boxes {
        полоса(
            остриё.x + b.loc.x, остриё.y + b.loc.y, b.size.w, b.size.h,
            if b.focused { BACK_FOCUS } else { BACK_IDLE }, карта, &mut elements,
        );
    }

    // ── Рамка текущего экрана ────────────────────────────────────────────────
    // «Где я на этой карте». Раньше её убрали как «жёлтый квадрат»: карта
    // показывала окрестность камеры, и рамка на отдалении разрасталась на всю
    // панель. Теперь кадр подбирается автоматически по объединению «все окна
    // стола ∪ видимый экран» — рамка гарантированно влезает и никогда не
    // занимает карту целиком. Тонкая и приглушённая: это подсказка, а не
    // главное на карте.
    {
        const VIEW_TH: i32 = 1;
        const VIEW_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.35];
        let a = crate::canvas::project_point_minimap(камера, proj.bbox, proj.scale);
        let b = crate::canvas::project_point_minimap(
            Point::from((камера.x + видимое.w, камера.y + видимое.h)),
            proj.bbox, proj.scale,
        );
        let (x, y) = (остриё.x + a.x, остриё.y + a.y);
        let (w, h) = ((b.x - a.x).max(1), (b.y - a.y).max(1));
        for (лx, лy, лw, лh) in [
            (x, y, w, VIEW_TH),
            (x, y + h - VIEW_TH, w, VIEW_TH),
            (x, y, VIEW_TH, h),
            (x + w - VIEW_TH, y, VIEW_TH, h),
        ] {
            полоса(лx, лy, лw, лh, VIEW_COLOR, карта, &mut elements);
        }
    }

    // ── Сетка холста ─────────────────────────────────────────────────────────
    // Разметка бесконечного холста: без неё на большой карте не видно ни
    // масштаба, ни того, что вид вообще движется, когда в кадре пусто. Шаг
    // выбирается так, чтобы на экране линии стояли не чаще ~90 px — иначе на
    // отдалении сетка вырождается в заливку и разносит damage-tracking.
    {
        const GRID_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.07];
        const GRID_MIN_PX: f64 = 90.0;
        const ЛЕСТНИЦА: [i32; 9] = [100, 200, 500, 1000, 2000, 5000, 10_000, 20_000, 50_000];
        if let Some(&шаг) = ЛЕСТНИЦА.iter()
            .find(|s| **s as f64 * proj.scale >= GRID_MIN_PX)
        {
            let первый = |от: i32| -> i32 { от.div_euclid(шаг) * шаг + шаг };
            let mut x = первый(proj.bbox.loc.x);
            while x < proj.bbox.loc.x + proj.bbox.size.w {
                let px = остриё.x + ((x - proj.bbox.loc.x) as f64 * proj.scale).round() as i32;
                полоса(px, карта.loc.y, 1, карта.size.h, GRID_COLOR, карта, &mut elements);
                x += шаг;
            }
            let mut y = первый(proj.bbox.loc.y);
            while y < proj.bbox.loc.y + proj.bbox.size.h {
                let py = остриё.y + ((y - proj.bbox.loc.y) as f64 * proj.scale).round() as i32;
                полоса(карта.loc.x, py, карта.size.w, 1, GRID_COLOR, карта, &mut elements);
                y += шаг;
            }
        }
    }

    // ── Плашка карточки ──────────────────────────────────────────────────────
    //
    // **26.08.2026: фон карты — МАТОВОЕ СТЕКЛО, а не обои.** Раньше внутрь
    // карты укладывались настоящие обои — той же плиткой, что на холсте, только
    // в масштабе карты: на отдалении это превращалось в мелкую повторяющуюся
    // мозаику, поверх которой шло затемнение 0.55, и всё вместе спорило с
    // миниатюрами за внимание. Ярик 26.08.2026: «сделай фон миникарты просто
    // заблюренным, без обоев». Теперь под карточкой — та же размытая заплата,
    // что под панелью, полкой и меню (`стекло`), а плашка поверх неё
    // полупрозрачная: получается тёмное матовое стекло с ровным фоном, на
    // котором читаются и сетка, и миниатюры.
    //
    // Порядок — не косметика: список кадра идёт от ПЕРЕДНЕГО плана к заднему,
    // поэтому плашка идёт после всего содержимого, а стекло — в самом конце,
    // под плашкой.
    let радиус = MINIMAP_RADIUS.min(видимая_часть.size.h / 2).min(видимая_часть.size.w / 2).max(0);
    let есть_стекло = state.blur_tex.is_some();
    // Со стеклом плашка приглушённая, без него — почти глухая: ровно та же
    // развилка, что у меню и карточки предпросмотра.
    let фон = if есть_стекло { PANEL_BG_GLASS } else { PANEL_BG };
    rounded_solid(
        pool, &mut idx,
        видимая_часть.loc.x, видимая_часть.loc.y, видимая_часть.size.w, видимая_часть.size.h, радиус,
        с_альфой(фон, видимость), &mut elements,
    );
    state.minimap_ids = пул;

    if let Some(el) = стекло(
        state, renderer,
        видимая_часть.loc.x, видимая_часть.loc.y, видимая_часть.size.w, видимая_часть.size.h, радиус, БЛЮР_КАРТА,
    ) {
        elements.push(el);
    }

    elements
}

/// Фон карточки карты: без стекла — почти глухой, со стеклом — приглушённый.
const PANEL_BG: [f32; 4] = [0.05, 0.05, 0.08, 0.94];
const PANEL_BG_GLASS: [f32; 4] = [0.05, 0.05, 0.08, 0.72];
/// Скругление карточки карты — то же, что у меню и карточки предпросмотра.
const MINIMAP_RADIUS: i32 = 16;

/// Цвет с домноженной альфой — так гасится ВСЁ, что рисует карта, пока она
/// раскрывается или сворачивается.
fn с_альфой(c: [f32; 4], a: f32) -> [f32; 4] {
    [c[0], c[1], c[2], c[3] * a]
}

/// Шапка карточки: слева — сколько окон открыто, справа — подпись кнопки
/// сброса вида. Обе строки живут в кэше текста, поэтому им нужен `state`.
///
/// Плашку кнопки рисует вызывающий (`build_minimap_elements`): она идёт через
/// общий пул solid-слотов, а сюда попадает только текст.
fn build_minimap_header(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    g: crate::canvas::MinimapGeom,
    видимая_часть: Rectangle<i32, Physical>,
    видимость: f32,
) -> Vec<OutputRenderElements> {
    let mut out = Vec::new();
    let поле = crate::canvas::MINIMAP_PADDING_PX.round() as i32;
    let h = crate::text::height(bar::TEXT);
    let y = g.panel.loc.y + поле + (crate::canvas::MINIMAP_HEADER_PX - h) / 2;
    // Карточка ещё не раскрылась до шапки — текст рисовать нельзя, обрезать его
    // нечем (у буфера памяти кропа нет). Проверяем ОБА края: раскрытие идёт от
    // центра, и в начале движения верх шапки лежит выше видимой части.
    if y < видимая_часть.loc.y || y + h > видимая_часть.loc.y + видимая_часть.size.h {
        return out;
    }
    let сколько = {
        let current = state.viewport.current_tags();
        state.tagged_windows.iter().filter(|tw| tw.tags & current != 0).count()
    };
    let заголовок = match сколько {
        0 => т!("Открытых окон нет", "No open windows").to_string(),
        n => тф!("Открытые окна · {n}", "Open windows · {n}"),
    };
    draw_text_w(
        state, renderer, g.panel.loc.x + поле, y, &заголовок,
        crate::text::Weight::Semi, bar::TEXT, с_альфой([1.0, 1.0, 1.0, 0.92], видимость), 0, &mut out,
    );

    let кнопка = state.minimap_reset_button();
    let ty = кнопка.loc.y + (кнопка.size.h - crate::text::height(bar::TEXT_SMALL)) / 2;
    if ty >= видимая_часть.loc.y
        && ty + crate::text::height(bar::TEXT_SMALL) <= видимая_часть.loc.y + видимая_часть.size.h
    {
        let цвет = с_альфой(if state.minimap_manual {
            [1.0, 1.0, 1.0, 0.95]
        } else {
            [1.0, 1.0, 1.0, 0.45]
        }, видимость);
        let tw = crate::text::width_of(
            crate::state::minimap_reset_label(), bar::STRONG, bar::TEXT_SMALL,
        );
        draw_text_w(
            state, renderer,
            кнопка.loc.x + (кнопка.size.w - tw) / 2, ty,
            crate::state::minimap_reset_label(),
            crate::text::Weight::Semi, bar::TEXT_SMALL, цвет, 0, &mut out,
        );
    }
    out
}

/// Подписи-заголовки под миниатюрами окон.
///
/// Возвращает пару «готовые строки текста» и «прямоугольники их плашек»:
/// плашки идут через общий пул solid-слотов, до которого отсюда не дотянуться
/// (`state` занят кэшем текста), поэтому их рисует вызывающий.
///
/// Подпись — это половина того, ради чего карта стала большой: миниатюра
/// отвечает «что там нарисовано», а подпись — «что это за окно», когда
/// содержимое ещё не пришло или окно ужато до пары сантиметров.
#[allow(clippy::type_complexity)]
fn build_minimap_labels(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    окна: &[МиникартаОкно],
    proj: &crate::canvas::MinimapProjection,
    остриё: Point<i32, Physical>,
    карта: Rectangle<i32, Physical>,
    видимость: f32,
) -> (Vec<OutputRenderElements>, Vec<(Rectangle<i32, Physical>, [f32; 4])>) {
    const LABEL_PAD: i32 = 5;
    /// Уже этого миниатюра не подписывается: плашка была бы шире самого окна.
    const LABEL_MIN_W: i32 = 60;
    let mut текст = Vec::new();
    let mut плашки = Vec::new();
    let высота = crate::text::height(bar::TEXT_SMALL);

    for (slot, (окно, b)) in окна.iter().zip(proj.boxes.iter()).enumerate() {
        if окно.подпись.is_empty() || b.size.w < LABEL_MIN_W {
            continue;
        }
        let x = остриё.x + b.loc.x;
        let y = остриё.y + b.loc.y + b.size.h + 3;
        let полная = Rectangle::<i32, Physical>::new(
            Point::from((x, y)),
            Size::from((b.size.w, высота + LABEL_PAD * 2)),
        );
        // Текст обрезать нечем (буфер памяти кропа не несёт) — подпись, которая
        // не помещается в карту целиком, просто не рисуется.
        if карта_вмещает(карта, полная).is_none() {
            continue;
        }
        let под_текст = (b.size.w - LABEL_PAD * 2).max(1);
        let влезает = crate::text::fits(&окно.подпись, bar::BODY, bar::TEXT_SMALL, под_текст);
        if влезает == 0 {
            continue;
        }
        let строка: String = if влезает < окно.подпись.chars().count() {
            let обрез: String = окно.подпись.chars().take(влезает.saturating_sub(1)).collect();
            format!("{обрез}…")
        } else {
            окно.подпись.clone()
        };
        // Плашки подписи рисует вызывающий — он же домножает их альфу; текст
        // идёт мимо него, поэтому проявление здесь считаем сами.
        let цвет = с_альфой(if окно.hovered || окно.focused {
            [1.0, 1.0, 1.0, 0.95]
        } else {
            [1.0, 1.0, 1.0, 0.72]
        }, видимость);
        let фон = if окно.hovered {
            [0.13, 0.22, 0.37, 0.92]
        } else {
            [0.04, 0.04, 0.06, 0.80]
        };
        draw_text_w(
            state, renderer, x + LABEL_PAD, y + LABEL_PAD, &строка,
            crate::text::Weight::Regular, bar::TEXT_SMALL, цвет, slot, &mut текст,
        );
        плашки.push((полная, фон));
    }
    (текст, плашки)
}

/// Помещается ли прямоугольник в карту ЦЕЛИКОМ (для того, что нельзя обрезать
/// — текстовых буферов).
fn карта_вмещает(
    карта: Rectangle<i32, Physical>,
    r: Rectangle<i32, Physical>,
) -> Option<()> {
    (карта.intersection(r) == Some(r)).then_some(())
}

/// Живые миниатюры: то же содержимое окон, что и на холсте, ужатое масштабом
/// миникарты и обрезанное краями панели.
///
/// Как считается место. `canvas::project_minimap` кладёт окно в
/// `поле + (холст − bbox) * scale`, причём поле НЕ масштабируется. Значит, если
/// взять точкой масштабирования угол СОДЕРЖИМОГО панели (её угол плюс поле) и
/// положить окно до масштабирования в `остриё + (холст − bbox)`, то
/// `RescaleRenderElement` (он считает `остриё + (loc − остриё) * scale`) даст
/// ровно ту же точку, куда проекция кладёт прямоугольник окна. Иначе миниатюра
/// разъезжалась бы с подложкой и рамкой фокуса.
fn build_minimap_thumbnails(
    renderer: &mut GlesRenderer,
    окна: &[МиникартаОкно],
    proj: &crate::canvas::MinimapProjection,
    остриё: Point<i32, Physical>,
    карта: Rectangle<i32, Physical>,
    видимость: f32,
) -> Vec<OutputRenderElements> {
    let панель = карта;

    let mut out = Vec::new();
    for окно in окна {
        // Масштаб выхода здесь 1.0 (как и для окон на холсте): весь зум панели
        // накладывает обёртка Rescale ниже, а не сам рендер поверхностей.
        //
        // Альфа — ПОСЛЕДНИМ аргументом, и она же гасит живое содержимое, пока
        // карта раскрывается. Раньше считалось, что гасить его нечем (обёртки
        // Rescale/Crop альфы не несут), и из-за этого раскрытие было шторкой;
        // альфу несёт сам источник, и с ней карта проявляется целиком.
        let els: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = окно.window.render_elements(
            renderer,
            Point::<i32, Physical>::from((
                остриё.x + окно.root.x - proj.bbox.loc.x,
                остриё.y + окно.root.y - proj.bbox.loc.y,
            )),
            smithay::utils::Scale::from(1.0),
            видимость,
        );
        out.extend(els.into_iter().filter_map(|el| {
            let ужатое = RescaleRenderElement::from_element(el, остриё, proj.scale);
            // Обрезка обязательна: панель показывает весь холст с запасом 20%,
            // но окно у края bbox своей рамкой из неё торчит — без Crop оно
            // рисовалось бы поверх экрана рядом с панелью.
            CropRenderElement::from_element(ужатое, 1.0, панель).map(OutputRenderElements::Minimap)
        }));
    }
    out
}

/// Бесшовный параллакс-фон (5.1): редкая сетка точек на самом заднем слое,
/// сдвигается на camera*0.3 вместо camera*1.0 — создаёт эффект глубины
/// (фон "отстаёт" от окон при панорамировании).
const PARALLAX_FACTOR: f64 = 0.3;
const PARALLAX_SPACING_PX: i32 = 160;
const PARALLAX_DOT_PX: i32 = 3;

const PARALLAX_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.08];

/// ВАЖНО про damage tracking: раньше здесь на каждый кадр создавался один
/// `SolidColorBuffer`, и все ~375 точек сетки (3840x2160 при шаге 160) шли
/// через `from_buffer`, то есть делили ОДИН Id, у которого вдобавок каждый
/// кадр было новое значение. Это обе болезни из `pooled_solid` разом, причём
/// в самом заднем слое, который строится безусловно на каждый кадр: экран
/// повреждался целиком всегда, `queue_frame` никогда не отдавал `EmptyFrame`,
/// цепочка VBlank не прерывалась и 4K перерисовывалось ~60 раз в секунду на
/// неподвижной картинке. Починка теней и масок углов до этого ничего не
/// меняла, пока параллакс оставался таким.
fn build_parallax_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    mode: Mode,
) -> Vec<OutputRenderElements> {
    let mut out = Vec::new();
    let zoom = state.viewport.zoom.max(0.01);

    let shift_x = state.viewport.cam_x * zoom * PARALLAX_FACTOR;
    let shift_y = state.viewport.cam_y * zoom * PARALLAX_FACTOR;
    let offset_x = shift_x.rem_euclid(PARALLAX_SPACING_PX as f64);
    let offset_y = shift_y.rem_euclid(PARALLAX_SPACING_PX as f64);

    // Один элемент на РЯД сетки вместо точки: точки ряда лежат в одной текстуре
    // (см. decor.rs). Было ~24×14 ≈ 340 элементов на кадр, стало ~15.
    let row_w = mode.size.w + PARALLAX_SPACING_PX;
    // Ширина/высота задаются в логических единицах и умножаются рендером на
    // zoom, а сетка живёт в экранных пикселях — поэтому делим на zoom заранее.
    let dst = Size::<i32, Logical>::from((
        (row_w as f64 / zoom).round().max(1.0) as i32,
        (PARALLAX_DOT_PX as f64 / zoom).round().max(1.0) as i32,
    ));

    let mut slot = 0usize;
    let mut y = -(offset_y as i32);
    while y < mode.size.h {
        let buf = state.decor.parallax_row(
            slot, row_w, PARALLAX_DOT_PX, PARALLAX_SPACING_PX, PARALLAX_COLOR,
        );
        let loc = Point::<f64, Physical>::from((-offset_x, y as f64));
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer, loc, buf, None, None, Some(dst), Kind::Unspecified,
        ) {
            Ok(el) => out.push(OutputRenderElements::Memory(el)),
            Err(e) => tracing::warn!("plx/udev: parallax: {:?}", e),
        }
        slot += 1;
        y += PARALLAX_SPACING_PX;
    }
    out
}

/// Цвет "clear" компоситора (см. render_frame ниже): то, что видно там, где не
/// нарисовано вообще ничего. Углы окон им когда-то ЗАКРАШИВАЛИСЬ — отсюда
/// чёрные куски поверх обоев; теперь их режет шейдер (`rounded.rs`), и цвет
/// этот к скруглению отношения не имеет.
const CLEAR_COLOR: [f32; 4] = [0.1, 0.1, 0.1, 1.0];
/// Радиус скругления в логических пикселях (до умножения на zoom).
const CORNER_RADIUS_LOGICAL: f64 = 8.0;
/// В Tile-режиме окна крупнее и без хаотичного разброса Float — заметнее
/// более крупное скругление (по просьбе пользователя).
const CORNER_RADIUS_LOGICAL_TILE: f64 = 16.0;

/// Ширина "среза" для каждой строки квадратного угла радиуса `radius_px`
/// (в физических пикселях), от уравнения окружности x = r - sqrt(r² - y²).
/// Индекс 0 — самая крайняя строка угла, последний индекс — ближе к прямому краю.
fn corner_cutout_widths(radius_px: i32) -> Vec<i32> {
    (0..radius_px).map(|row| {
        let dy = (radius_px - row) as f64;
        let inside = (radius_px * radius_px) as f64 - dy * dy;
        let cutoff = if inside <= 0.0 { radius_px as f64 } else { radius_px as f64 - inside.sqrt() };
        cutoff.round().max(0.0) as i32
    }).collect()
}

/// Слот пула процедурных solid-элементов: стабильный между кадрами `Id` плюс
/// последний отданный цвет и счётчик коммитов (см. `pooled_solid`).
#[derive(Debug)]
pub struct SolidSlot {
    id: Id,
    commit: CommitCounter,
    color: [f32; 4],
}

impl SolidSlot {
    fn new() -> Self {
        // Цвет-«невозможное значение», чтобы первый же кадр записал настоящий
        // и не считал совпадение с чёрным прозрачным за «цвет не менялся».
        Self { id: Id::new(), commit: CommitCounter::default(), color: [f32::NAN; 4] }
    }
}

/// Выдаёт solid-элемент со стабильным между кадрами `Id` из пула `pool`.
///
/// Зачем не `SolidColorBuffer` + `from_buffer`: `SolidColorBuffer::new()` внутри
/// вызывает `Id::new()`, а `from_buffer` берёт id прямо из буфера. Отсюда две
/// противоположные болезни, обе ломавшие damage tracking:
///
/// * тени (`build_shadow_elements`) и фон обзора собирали НОВЫЙ буфер на каждую
///   полоску каждый кадр → у сотен элементов на окно каждый кадр новый Id →
///   damage tracker считал их все новыми и помечал повреждённым весь экран.
///   В логе на 10 МБ не было ни одного `is_empty=true`: 4K-кадр перерисовывался
///   целиком по 60 раз в секунду даже на неподвижном экране, а `queue_frame`
///   никогда не возвращал `EmptyFrame`. Это и есть основной источник лагов;
/// * маски углов, наоборот, брали ОДИН кэшированный буфер (значит один Id) сразу
///   на десятки элементов, а damage tracker индексирует состояние по Id и
///   схлопывает такие элементы в один — повреждения считались неверно.
///
/// Пул выдаёт Id по порядковому номеру: постоянный от кадра к кадру и
/// уникальный внутри кадра. Порядок обхода окон детерминирован, так что
/// элемент N — это из кадра в кадр одна и та же полоска. Если состав или
/// геометрия окон меняется, элемент под тем же номером просто получает другую
/// геометрию, и damage tracker честно повреждает старый и новый прямоугольник.
///
/// Цвет слота хранится в пуле и при смене двигает `CommitCounter`: раньше здесь
/// стоял `CommitCounter::default()` в расчёте на то, что цвет для номера
/// постоянен (константы слоёв тени / `CLEAR_COLOR` / `BG_COLOR`), но пулом
/// пользуются и миникарта с выделением, где цвет элемента под тем же номером
/// зависит от фокуса и состава выделения.
///
/// **Цвет сюда отдают ОБЫЧНЫЙ (straight), домножает на альфу эта функция.**
/// smithay ждёт premultiplied (`Color32F` — «pre-multiplied RGBA»), шейдер
/// `solid.frag` отдаёт `gl_FragColor = color` как есть, а рисуется всё с
/// `BlendFunc(ONE, ONE_MINUS_SRC_ALPHA)` — то есть на экран идёт
/// `цвет + фон·(1−alpha)`. Пока домножения не было, `[1,1,1,0.08]` давало не
/// лёгкую дымку, а `1.0 + 0.92·фон` — НАСЫЩЕННО БЕЛЫЙ прямоугольник. Это и есть
/// «белые квадраты»: полоска вкладок в Columns (`TAB_IDLE`), пустая ячейка
/// стола в обзоре (`EMPTY_COLOR`), рамка «где я» на карте (`VIEW_COLOR`),
/// разделители панели. Замер 25.08.2026 (харнесс, `h_c2.png`): рамка карты,
/// заданная `[1,1,1,0.35]`, приходила на экран ровно (255,255,255).
///
/// Домножение стоит ЗДЕСЬ, а не у вызывающих, нарочно: solid-элементы во всём
/// parallax рождаются только в этой функции, и 34 места из 36 писали цвет обычным.
/// Раньше `premul` звали руками ровно в двух (полка и `rounded_tex`) — то есть
/// правило существовало, но соблюдалось в 6% случаев.
fn pooled_solid(
    pool: &mut Vec<SolidSlot>,
    idx: &mut usize,
    loc: (i32, i32),
    size: (i32, i32),
    color: [f32; 4],
) -> OutputRenderElements {
    let color = premul(color);
    while pool.len() <= *idx {
        pool.push(SolidSlot::new());
    }
    let slot = &mut pool[*idx];
    *idx += 1;
    // Геометрию damage tracker сравнивает сам, а вот содержимое элемента для него
    // непрозрачно — об изменении цвета при той же геометрии он узнаёт ТОЛЬКО по
    // счётчику коммитов. Без этого, например, прямоугольник миникапы, потерявший
    // фокус, остался бы на экране старым цветом до ближайшего чужого повреждения.
    if slot.color != color {
        slot.color = color;
        slot.commit.increment();
    }
    let geo: Rectangle<i32, Physical> = Rectangle::new(Point::from(loc), Size::from(size));
    OutputRenderElements::Solid(SolidColorRenderElement::new(
        slot.id.clone(), geo, slot.commit, color, Kind::Unspecified,
    ))
}

/// Полоска вкладок слева от вкладочной колонки (niri: tab indicator).
/// Рисуется ТОЛЬКО в режиме Columns — остальные раскладки про вкладки не знают.
fn build_tab_indicators(state: &mut Parallax) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if state.tile_config.layout != crate::tiling::Layout::Columns {
        return els;
    }
    /// Цвет неактивной и активной вкладки.
    const TAB_IDLE: [f32; 4] = [1.0, 1.0, 1.0, 0.22];
    const TAB_ACTIVE: [f32; 4] = [0.55, 0.75, 1.0, 0.95];

    let zoom = state.viewport.zoom;
    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    let strips: Vec<(i32, i32, i32, i32, usize, usize)> = (0..state.columns.columns.len())
        .filter_map(|ci| state.columns_tab_strip(ci))
        .collect();
    if strips.is_empty() {
        return els;
    }
    let pool = &mut state.tab_ids;
    let mut idx = 0usize;
    for (x, y, w, tab_h, n, active) in strips {
        for i in 0..n {
            let color = if i == active { TAB_ACTIVE } else { TAB_IDLE };
            // Между вкладками — маленький просвет, чтобы читались как отдельные.
            let top = y + tab_h * i as i32 + 2;
            let h = (tab_h - 4).max(2);
            let px = (((x as f64) - cam_x) * zoom).round() as i32;
            let py = (((top as f64) - cam_y) * zoom).round() as i32;
            let pw = ((w as f64) * zoom).round().max(1.0) as i32;
            let ph = ((h as f64) * zoom).round().max(1.0) as i32;
            els.push(pooled_solid(pool, &mut idx, (px, py), (pw, ph), color));
        }
    }
    els
}

/// Подсказка вставки при перетаскивании окна в Columns (niri: insert hint) —
/// показывает шов между колонками или стопку, куда окно встанет на отпускании.
fn build_insert_hint(state: &mut Parallax) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if state.tile_config.layout != crate::tiling::Layout::Columns {
        return els;
    }
    let Some(pos_x) = state.columns_drag_hint else { return els };
    let Some(rect) = state.columns_insert_hint_rect(pos_x) else { return els };
    const HINT_COLOR: [f32; 4] = [0.35, 0.6, 1.0, 0.35];

    let zoom = state.viewport.zoom;
    let x = ((rect.loc.x as f64 - state.viewport.cam_x) * zoom).round() as i32;
    let y = ((rect.loc.y as f64 - state.viewport.cam_y) * zoom).round() as i32;
    let w = (rect.size.w as f64 * zoom).round().max(1.0) as i32;
    let h = (rect.size.h as f64 * zoom).round().max(1.0) as i32;
    let pool = &mut state.hint_ids;
    let mut idx = 0usize;
    els.push(pooled_solid(pool, &mut idx, (x, y), (w, h), HINT_COLOR));
    els
}

/// Радиус скругления окон для текущей раскладки (логические px).
fn corner_radius_logical(state: &Parallax) -> i32 {
    let r = if state.tile_config.layout == crate::tiling::Layout::Tile {
        CORNER_RADIUS_LOGICAL_TILE
    } else {
        CORNER_RADIUS_LOGICAL
    };
    (r.round() as i32).clamp(2, 32)
}

/// Геометрия окна на экране: (x0, y0, ширина, высота) в физических пикселях.
/// Виден ли на экране прямоугольник (в физических пикселях кадра) с запасом
/// `margin` по краям.
///
/// Холст в parallax бесконечен, а декорации (тени) строились для ВСЕХ окон текущих
/// тегов — включая те, что стоят в тысячах пикселей от камеры.
/// Каждое такое окно — это 11 элементов тени, которые
/// создаются, попадают в список кадра и сравниваются damage tracker'ом с
/// прошлым кадром 190 раз в секунду, чтобы затем быть обрезанными по краю
/// экрана. Ровно ту же проверку окна проходят перед отрисовкой (см. `видимое`
/// в render_surface) — декорации просто про неё забыли.
fn on_screen(screen: Size<i32, Logical>, r: (f64, f64, f64, f64), margin: f64) -> bool {
    let (x, y, w, h) = r;
    x + w + margin > 0.0
        && y + h + margin > 0.0
        && x - margin < screen.w as f64
        && y - margin < screen.h as f64
}

fn window_screen_rect(state: &Parallax, window: &Window) -> Option<(f64, f64, f64, f64)> {
    let geo = state.space.element_geometry(window)?;
    let zoom = state.viewport.zoom;
    // Размер берём ВИДИМЫЙ, а не тот, что нарисовал клиент. Упрямое окно
    // (GTK/Electron с внутренним минимумом) рисует буфер больше запрошенного и
    // режется в кадре по запрошенной рамке (см. "нужен_кроп"), а тень считалась
    // по факту клиента — и торчала из обрезанного окна во все стороны, выдавая
    // его настоящий размер. Замер 25.08.2026 в харнессе: окно 310x163 при
    // запрошенных 208x108 — тень на все 310x163.
    let видимый = видимый_размер(window, geo.size);
    Some((
        (geo.loc.x as f64 - state.viewport.cam_x) * zoom,
        (geo.loc.y as f64 - state.viewport.cam_y) * zoom,
        видимый.w as f64 * zoom,
        видимый.h as f64 * zoom,
    ))
}

/// Какого размера окно ВИДНО на экране: меньшее из «что клиент нарисовал» и
/// «что у него запросили». Одна точка правды для всех, кто рисует вокруг окна
/// (тень, а дальше — всё, что захочет совпасть с его рамкой).
fn видимый_размер(window: &Window, факт: Size<i32, Logical>) -> Size<i32, Logical> {
    match crate::xwin::requested_size(window) {
        Some(t) if t.w > 0 && t.h > 0 => Size::from((t.w.min(факт.w), t.h.min(факт.h))),
        _ => факт,
    }
}

/// Мягкая нейтральная тень-«гало» позади каждого окна.
///
/// Девятипатчевая раскладка (см. decor.rs): 4 угловые плитки + 4 кромки,
/// растянутые вдоль сторон, + однотонная середина тремя прямоугольниками.
/// 11 элементов на окно вместо ~225 построчных полосок, вид тот же — форма и
/// альфы слоёв запечены в плитки по той же формуле.
///
/// Части НЕ перекрываются: полупрозрачные куски, наложившись, дали бы двойное
/// затемнение по швам.
fn build_shadow_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if state.tagged_windows.is_empty() {
        return els;
    }
    let zoom = state.viewport.zoom;
    let screen = state.screen_size();
    let radius = corner_radius_logical(state);
    state.decor.ensure(radius);

    // Всё в физических пикселях экрана; плитки заданы в логических и
    // домножаются рендером на zoom — поэтому и здесь масштабируем на него.
    let s = crate::decor::SPREAD as f64 * zoom;       // ширина кромки
    let r = radius as f64 * zoom;                     // радиус скругления

    let drop = crate::decor::DROP as f64 * zoom;      // сдвиг тени вниз
    let center = {
        let a = crate::decor::center_alpha();
        [0.0f32, 0.0, 0.0, a]
    };

    let windows: Vec<Window> = state.tagged_windows.iter().map(|tw| tw.window.clone()).collect();
    let mut slot = 0usize;
    for window in windows {
        // Тень у полноэкранного окна рисовать негде и не нужно — см. выше.
        if state.is_fullscreen(&window) {
            continue;
        }
        let Some((x0, y0raw, w, h)) = window_screen_rect(state, &window) else { continue };
        if w < 8.0 || h < 8.0 || w < 2.0 * r || h < 2.0 * r {
            continue;
        }
        // Окно далеко за краем экрана — тени от него не видно (см. on_screen).
        // Запас берём с ширину самой тени: она вылезает за окно на s и уезжает
        // вниз на drop.
        if !on_screen(screen, (x0, y0raw, w, h), s + drop) {
            continue;
        }
        let y0 = y0raw + drop;

        // ── Углы ─────────────────────────────────────────────────────────────
        let corners = [
            (crate::decor::TL, x0 - s,     y0 - s),
            (crate::decor::TR, x0 + w - r, y0 - s),
            (crate::decor::BL, x0 - s,     y0 + h - r),
            (crate::decor::BR, x0 + w - r, y0 + h - r),
        ];
        // Тот же явный размер, что и у масок: плитка угла тени — это
        // (кромка + радиус) логических пикселей, на экране она должна занять
        // столько же, умноженное на зум (s и r уже с зумом).
        let dst_corner = Size::<i32, Logical>::from((
            ((s + r).round() as i32).max(1), ((s + r).round() as i32).max(1),
        ));
        for (corner, x, y) in corners {
            let buf = state.decor.shadow_corner(corner, slot);
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer, Point::<f64, Physical>::from((x, y)), buf,
                None, None, Some(dst_corner), Kind::Unspecified,
            ) {
                Ok(el) => els.push(OutputRenderElements::Memory(el)),
                Err(e) => tracing::warn!("plx/udev: shadow corner: {:?}", e),
            }
        }


        // ── Кромки ───────────────────────────────────────────────────────────
        // Текстура толщиной в пиксель растягивается вдоль стороны через dst:
        // альфа вдоль стороны постоянна, так что растяжение точное.
        // Длина стороны уже в экранных пикселях (w и r посчитаны с зумом), а
        // толщина кромки — логическая, её домножаем на зум сами: раньше это
        // делал за нас масштаб выхода, отсюда и деление на зум, которое теперь
        // не нужно.
        let side_w = (w - 2.0 * r).round().max(1.0) as i32;
        let side_h = (h - 2.0 * r).round().max(1.0) as i32;
        let толщина = (s.round() as i32).max(1);
        let edges = [
            (crate::decor::TOP,    x0 + r,     y0 - s,     side_w, толщина),
            (crate::decor::BOTTOM, x0 + r,     y0 + h,     side_w, толщина),
            (crate::decor::LEFT,   x0 - s,     y0 + r,     толщина, side_h),
            (crate::decor::RIGHT,  x0 + w,     y0 + r,     толщина, side_h),
        ];
        for (edge, x, y, dw, dh) in edges {
            let buf = state.decor.shadow_edge(edge, slot);
            let dst = Size::<i32, Logical>::from((dw, dh));
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer, Point::<f64, Physical>::from((x, y)), buf,
                None, None, Some(dst), Kind::Unspecified,
            ) {
                Ok(el) => els.push(OutputRenderElements::Memory(el)),
                Err(e) => tracing::warn!("plx/udev: shadow edge: {:?}", e),
            }
        }

        // ── Середина ─────────────────────────────────────────────────────────
        // Три непересекающихся прямоугольника: широкий по центру и две вставки
        // между угловыми плитками по бокам.
        let pool = &mut state.shadow_ids;
        let mut idx = slot * 3;
        let mid_x = (x0 + r).round() as i32;
        let mid_w = (w - 2.0 * r).round() as i32;
        if mid_w > 0 {
            els.push(pooled_solid(
                pool, &mut idx, (mid_x, y0.round() as i32), (mid_w, h.round() as i32), center,
            ));
        }
        let side_hh = (h - 2.0 * r).round() as i32;
        if side_hh > 0 && r >= 1.0 {
            let y = (y0 + r).round() as i32;
            els.push(pooled_solid(
                pool, &mut idx, (x0.round() as i32, y), (r.round() as i32, side_hh), center,
            ));
            els.push(pooled_solid(
                pool, &mut idx, ((x0 + w - r).round() as i32, y), (r.round() as i32, side_hh), center,
            ));
        }

        slot += 1;
    }
    els
}

/// Полупрозрачный «заметный» фон + тень под каждым воркспейсом — ТОЛЬКО в обзоре
/// (тап Super), чтобы столы визуально читались как отдельные карточки.
fn build_overview_bg_elements(state: &mut Parallax) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if !state.overview_active {
        return els;
    }
    // Тень за каждым бэндом — многослойная растушёвка, как у окон.
    const SHADOW_LAYERS: [(i32, f32); 3] = [(3, 0.10), (7, 0.05), (12, 0.02)];
    // Фон бэнда — тёмный полупрозрачный прямоугольник со скруглёнными углами.
    const BG_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.28];
    // ПУСТОЙ стол (ни одного окна) карточкой не заливается — только светлый
    // контур. В ленте этажи стоят вплотную, и хвост пустых столов (niri всегда
    // держит пустой снизу, а при заходе на дальний стол их набирается несколько
    // подряд) сливался в одно чёрное пятно под лентой — та самая «тень».
    const EMPTY_COLOR: [f32; 4] = [1.0, 1.0, 1.0, 0.08];
    const ROUNDING: i32 = 12;
    let zoom = state.viewport.zoom;
    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    // Собираем прямоугольники до взятия &mut на пул: overview_band_rects()
    // читает state целиком, а пул живёт в нём же.
    let bands: Vec<(Rectangle<i32, Logical>, bool)> = state.overview_band_rects();
    // Лента (niri): этажи стоят ВПЛОТНУЮ друг к другу, без зазора, и тени у неё
    // нет вовсе. Слои тени — это СПЛОШНЫЕ прямоугольники позади карточек, а
    // карточки полупрозрачные (BG_COLOR alpha 0.28): общая тень на всю полосу
    // просвечивала сквозь этажи чёрным пятном и вылезала ореолом под нижним
    // этажом. В сетке столов (не лента) карточки разнесены зазорами, там тень
    // читается как ореол и остаётся.
    //
    // Пустые столы тени не отбрасывают тем более: их карточки нет, а тень —
    // сплошной чёрный прямоугольник, то есть ровно то пятно, от которого мы
    // и уходим.
    let strip = state.overview_strip;
    let shadow_rects: Vec<Rectangle<i32, Logical>> = if strip {
        Vec::new()
    } else {
        bands.iter().filter(|(_, empty)| !empty).map(|(r, _)| *r).collect()
    };
    let to_screen = |r: Rectangle<i32, Logical>| -> (i32, i32, i32, i32) {
        (
            ((r.loc.x as f64 - cam_x) * zoom).round() as i32,
            ((r.loc.y as f64 - cam_y) * zoom).round() as i32,
            (r.size.w as f64 * zoom).round() as i32,
            (r.size.h as f64 * zoom).round() as i32,
        )
    };
    let pool = &mut state.overview_bg_ids;
    let mut idx = 0usize;

    // ── Фоны столов ──
    // ВАЖНО: сначала фоны, тени — ниже. Список элементов идёт спереди назад,
    // так что запушенное РАНЬШЕ рисуется ПОВЕРХ. Тени, стоявшие до фона своего
    // же стола, ложились полупрозрачным пятном на всю его карточку и делали её
    // темнее задуманного вместо ореола по краям.
    for &(r, empty) in &bands {
        let (sx, sy, sw, sh) = to_screen(r);
        if sw <= 0 || sh <= 0 {
            continue;
        }
        // Пустой стол — только рамка: место под ним видно (туда бросают окно,
        // чтобы завести новый стол), но чёрной заливки нет.
        if empty {
            let t = ((2.0 * zoom).round() as i32).max(1);
            if sw <= 2 * t || sh <= 2 * t {
                continue;
            }
            els.push(pooled_solid(pool, &mut idx, (sx, sy), (sw, t), EMPTY_COLOR));
            els.push(pooled_solid(pool, &mut idx, (sx, sy + sh - t), (sw, t), EMPTY_COLOR));
            els.push(pooled_solid(pool, &mut idx, (sx, sy + t), (t, sh - 2 * t), EMPTY_COLOR));
            els.push(pooled_solid(
                pool, &mut idx, (sx + sw - t, sy + t), (t, sh - 2 * t), EMPTY_COLOR,
            ));
            continue;
        }
        let r_px = (ROUNDING as f64 * zoom).round() as i32;
        let widths = corner_cutout_widths(r_px);
        let rr = widths.len() as i32;
        if sh <= 2 * rr || sw <= 2 * rr {
            els.push(pooled_solid(pool, &mut idx, (sx, sy), (sw.max(1), sh.max(1)), BG_COLOR));
            continue;
        }
        for (i, &cw) in widths.iter().enumerate() {
            let rw = sw - 2 * cw;
            if rw <= 0 { continue; }
            let i = i as i32;
            els.push(pooled_solid(pool, &mut idx, (sx + cw, sy + i), (rw, sh - 2 * i), BG_COLOR));
        }
    }

    // ── Тени (позади карточек) ──
    for r in &shadow_rects {
        let (sx, sy, sw, sh) = to_screen(*r);
        if sw <= 0 || sh <= 0 {
            continue;
        }
        for (spread, alpha) in SHADOW_LAYERS {
            let color = [0.0f32, 0.0, 0.0, alpha];
            els.push(pooled_solid(
                pool, &mut idx,
                (sx - spread, sy - spread),
                ((sw + 2 * spread).max(1), (sh + 2 * spread).max(1)),
                color,
            ));
        }
    }
    els
}

/// Нарисовать скруглённый прямоугольник solid-элементами (строчный corner-cutout).
/// Каждый ряд — один `pooled_solid`; для больших радиусов генерирует ~2×radius
/// элементов. Если радиус 0 или ≥ половины меньшей стороны — рисует обычный rect.
fn rounded_solid(
    pool: &mut Vec<SolidSlot>, idx: &mut usize,
    x: i32, y: i32, w: i32, h: i32, radius: i32, color: [f32; 4],
    out: &mut Vec<OutputRenderElements>,
) {
    if radius <= 0 || radius > w.min(h) / 2 {
        out.push(pooled_solid(pool, idx, (x, y), (w, h), color));
        return;
    }
    let widths = corner_cutout_widths(radius);
    let rr = widths.len() as i32; // радиус в пикселях (число строк)
    // Верхняя половина: отрезаем углы построчно
    for i in 0..rr {
        let cw = widths[i as usize];
        let rw = w - 2 * cw;
        if rw > 0 {
            out.push(pooled_solid(pool, idx, (x + cw, y + i), (rw, 1), color));
        }
    }
    // Средняя часть: сплошной прямоугольник на всю ширину
    let mid_h = h - 2 * rr;
    if mid_h > 0 {
        out.push(pooled_solid(pool, idx, (x, y + rr), (w, mid_h), color));
    }
    // Нижняя половина — ЗЕРКАЛО верхней: вырезы идут в обратном порядке.
    // Раньше здесь стоял тот же widths[i], что и наверху, то есть снизу
    // рисовалась ВТОРАЯ ВЕРХНЯЯ дуга: широкий вырез оказывался у шва, узкий —
    // у нижнего края. У точки бара 28×28 с радиусом 14 средней части нет
    // совсем, поэтому кружок вырождался в две одинаковые полудуги со швом
    // посередине — весь бар выглядел сломанным. Задевало это всё, что рисуется
    // скруглённым: бар, миникарту, индикаторы вкладок, подсказку вставки.
    for i in 0..rr {
        let cw = widths[(rr - 1 - i) as usize];
        let rw = w - 2 * cw;
        if rw > 0 {
            out.push(pooled_solid(pool, idx, (x + cw, y + h - rr + i), (rw, 1), color));
        }
    }
}

// Геометрия панели живёт в bar.rs — там же, где её считает хит-тест кликов.
// Здесь остались только цвета и то, что нужно рисованию.
//
// Имена BAR_H/BAR_TOP/BAR_W когда-то лежали тут, и от них считали себя ещё
// двое: полка состояния и резерв места под окна (tiling::BAR_RESERVED). Теперь
// панель — три отдельных острова, «ширины бара» у неё нет вовсе, а высота и
// отступ переехали в bar::H и bar::TOP.
use crate::bar;

const BAR_RADIUS: i32 = bar::RADIUS;
const DOT: i32 = bar::DOT;
const DOT_RADIUS: i32 = 10;        // скругление точек = круги
/// Высота бара — её всё ещё спрашивает полка и меню рядом.
pub const BAR_H: i32 = bar::H;
/// Отступ бара от верхнего края экрана.
pub const BAR_TOP: i32 = bar::TOP;
const PAD_V: i32 = (BAR_H - DOT) / 2;
/// Фон островов и значка блютуза — тёмный полупрозрачный.
const BAR_BG: [f32; 4] = [0.04, 0.04, 0.07, 0.65];
/// Основной цвет текста панели и приглушённый для второстепенного (дата,
/// разделители, неактивные столы).
const BAR_TEXT: [f32; 4] = [0.93, 0.95, 1.0, 0.96];
const BAR_DIM: [f32; 4] = [0.85, 0.87, 0.95, 0.55];
const BAR_SEP: [f32; 4] = [1.0, 1.0, 1.0, 0.16];

/// Скруглённый прямоугольник ПЛИТКАМИ: четыре угла-текстуры и до трёх сплошных
/// кусков — семь элементов кадра вместо двадцати пяти у `rounded_solid` (см.
/// `text::TextCache::corner`). Круг (w = h = 2r) выходит вовсе из четырёх.
///
/// `color` передаётся обычным, не premultiplied: текстуре домножение делает
/// растеризатор, сплошным кускам — `premul` здесь же. Раньше на этом уже
/// путались: сплошные прямоугольники ждут домноженных компонент, маски — нет.
#[allow(clippy::too_many_arguments)]
fn rounded_tex(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    pool: &mut Vec<SolidSlot>,
    idx: &mut usize,
    x: i32, y: i32, w: i32, h: i32, radius: i32,
    color: [f32; 4],
    slot: &mut usize,
    out: &mut Vec<OutputRenderElements>,
) {
    let r = radius.min(w / 2).min(h / 2).max(0);
    if r == 0 {
        out.push(pooled_solid(pool, idx, (x, y), (w.max(1), h.max(1)), color));
        return;
    }
    // Углы: 0=ЛВ, 1=ПВ, 2=ЛН, 3=ПН — тот же порядок, что у text::corner.
    for (n, (cx, cy)) in [
        (x, y), (x + w - r, y), (x, y + h - r), (x + w - r, y + h - r),
    ].into_iter().enumerate()
    {
        let (buf, side) = state.text_cache.corner(r, n, color, *slot);
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((cx as f64, cy as f64)),
            buf,
            None, None,
            Some(Size::<i32, Logical>::from((side, side))),
            Kind::Unspecified,
        ) {
            Ok(el) => out.push(OutputRenderElements::Memory(el)),
            Err(e) => tracing::warn!("plx/udev: tile corner: {:?}", e),
        }
    }
    *slot += 1;
    // Цвет отдаём обычным: домножит на альфу `pooled_solid`.
    let c = color;
    // Середина во всю ширину, а сверху и снизу — полосы между углами.
    if h - 2 * r > 0 {
        out.push(pooled_solid(pool, idx, (x, y + r), (w, h - 2 * r), c));
    }
    if w - 2 * r > 0 {
        out.push(pooled_solid(pool, idx, (x + r, y), (w - 2 * r, r), c));
        out.push(pooled_solid(pool, idx, (x + r, y + h - r), (w - 2 * r, r), c));
    }
}

/// Панель сверху: три острова (см. bar.rs).
///
/// Слева — девять столов и заголовок активного окна; значок стола показывает
/// его раскладку: круг=Tile, две колонки=Columns (niri), две тильды=Float,
/// квадрат=Monocle, серый кружок=ещё не посещённый стол.
/// По центру — часы и дата. Справа — раскладка клавиатуры, звук, заряд,
/// значки трея (sni.rs) и полосочка полки состояния.
///
/// Порядок сборки — ОТ ЗАДНЕГО ПЛАНА К ПЕРЕДНЕМУ, в конце список
/// переворачивается: в кадре первым идёт то, что ближе к зрителю. Так же
/// собирается полка. Раньше бар клал фон первым и не переворачивал — фоновая
/// плашка с альфой 0.65 лежала ПОВЕРХ значков и гасила их на две трети; с
/// текстом это стало бы совсем нечитаемо.
fn build_bar_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if output.current_mode().is_none() {
        return els;
    }

    let data = state.bar_data();
    let lay = bar::layout(&data);
    let current_tags = state.viewport.current_tags();
    let shelf_open = state.tray_open();

    let mut idx = 0usize;
    let mut pool = std::mem::take(&mut state.bar_ids);
    // Счётчик буферов ОДИН на всю панель: и углы плиток, и строки берут из
    // своих пулов по этому номеру, а два элемента кадра с одним буфером ломают
    // подсчёт повреждений (см. заметку про Id в text.rs).
    let mut slot = 0usize;

    // ── Размытый фон под островами ───────────────────────────────────────────
    //
    // Идёт ПЕРЕД заливкой островов: список здесь собирается от заднего плана к
    // переднему и разворачивается в конце (см. els.reverse()), значит
    // добавленное раньше окажется ниже. Обрезается тем же шейдером, что и углы
    // окон: прямоугольная текстура под скруглённой плашкой торчала бы углами.
    if state.blur_tex.is_some() {
        // Номер заплаты закреплён ЗА ОСТРОВОМ (0 — левый, 1 — центральный,
        // 2 — правый), а не за порядком в списке: центрального острова может
        // не быть вовсе (узкий выход), и сквозная нумерация отдала бы правому
        // острову Id центрального — то есть чужую историю повреждений.
        for (i, r) in [Some(lay.left), lay.center, Some(lay.right)].into_iter().enumerate() {
            let Some(r) = r else { continue };
            let i = БЛЮР_ОСТРОВ + i;
            if let Some(el) = build_blur_patch(state, renderer, r, BAR_RADIUS as f32, 0.85, i) {
                els.push(el);
            }
        }
    }

    // ── Фоны островов ────────────────────────────────────────────────────────
    for r in [Some(lay.left), lay.center, Some(lay.right)].into_iter().flatten() {
        rounded_tex(
            state, renderer, &mut pool, &mut idx,
            r.x, r.y, r.w, r.h, BAR_RADIUS, BAR_BG, &mut slot, &mut els,
        );
    }

    // ── Столы, разделители, полка, заряд ─────────────────────────────────────
    for item in &lay.cells {
        let r = item.rect;
        match item.cell {
            bar::Cell::Tag(tag) => {
                let layout = state.tag_layouts.get(&tag).copied();
                let посещённый = layout.is_some()
                    || tag == current_tags
                    || state.visited_tags.contains(&tag);
                let y = r.y + PAD_V;
                if !посещённый {
                    // Новый, ещё не посещённый стол — серая круглая точка.
                    const DOT_GRAY: [f32; 4] = [0.5, 0.5, 0.55, 0.6];
                    rounded_tex(
                        state, renderer, &mut pool, &mut idx,
                        r.x, y, DOT, DOT, DOT_RADIUS, DOT_GRAY, &mut slot, &mut els,
                    );
                    continue;
                }
                let base = if tag == current_tags {
                    [1.0, 1.0, 1.0, 0.95]
                } else {
                    [1.0, 1.0, 1.0, 0.40]
                };
                // Иконка = САМА ФИГУРА, без круглой подложки: подложка тем же
                // цветом сливалась с фигурой, и все столы выглядели
                // одинаковыми кружками — режим по ним не читался вовсе.
                match layout.unwrap_or(crate::tiling::Layout::Tile) {
                    crate::tiling::Layout::Tile => {
                        rounded_tex(
                            state, renderer, &mut pool, &mut idx,
                            r.x, y, DOT, DOT, DOT_RADIUS, base, &mut slot, &mut els,
                        );
                    }
                    // niri — две колонки рядом, во всю высоту.
                    crate::tiling::Layout::Columns => {
                        let cw = (DOT * 2 / 5).max(2);          // 8 из 20
                        let gap = DOT - 2 * cw;                 // 4 между ними
                        let rr = (cw / 2).max(1);
                        rounded_tex(state, renderer, &mut pool, &mut idx, r.x, y, cw, DOT, rr, base, &mut slot, &mut els);
                        rounded_tex(state, renderer, &mut pool, &mut idx, r.x + cw + gap, y, cw, DOT, rr, base, &mut slot, &mut els);
                    }
                    // Float — две горизонтальные тильды одна под другой.
                    crate::tiling::Layout::Float => {
                        let th = (DOT / 4).max(2);              // 5 из 20
                        let gap = (DOT / 5).max(2);             // 4 между ними
                        let rr = (th / 2).max(1);
                        let top = y + (DOT - (2 * th + gap)) / 2;
                        rounded_tex(state, renderer, &mut pool, &mut idx, r.x, top, DOT, th, rr, base, &mut slot, &mut els);
                        rounded_tex(state, renderer, &mut pool, &mut idx, r.x, top + th + gap, DOT, th, rr, base, &mut slot, &mut els);
                    }
                    // Monocle — одно окно на весь стол: скруглённый квадрат.
                    crate::tiling::Layout::Monocle => {
                        let rr = (DOT / 5).max(1);
                        rounded_tex(
                            state, renderer, &mut pool, &mut idx,
                            r.x, y, DOT, DOT, rr, base, &mut slot, &mut els,
                        );
                    }
                }
            }
            bar::Cell::Sep => {
                // Волосок в один пиксель: скруглять нечего, идёт сплошным.
                let h = BAR_H / 2;
                els.push(pooled_solid(
                    &mut pool, &mut idx,
                    (r.x + r.w / 2, r.y + (BAR_H - h) / 2), (1, h), BAR_SEP,
                ));
            }
            bar::Cell::Handle => {
                // Хват: короткая черта посреди полосочки. Без неё полоска
                // читается как обрубок острова, а не как то, на что нажимают.
                let w = (r.w / 3).max(2);
                let h = r.h / 2;
                let color = if shelf_open { TRAY_ON } else { TRAY_DIM };
                rounded_tex(
                    state, renderer, &mut pool, &mut idx,
                    r.x + (r.w - w) / 2, r.y + (r.h - h) / 2, w, h, w / 2,
                    color, &mut slot, &mut els,
                );
            }
            bar::Cell::Battery => {
                // Заливка внутри обводки значка — тем же приёмом, что и в полке.
                let Some((percent, charging)) = data.battery else { continue };
                let значок = bar::Rect { x: r.x, y: r.y, w: DOT, h: r.h };
                let (bx, by, bw, bh) = battery_box_fit(значок, DOT);
                let (ix, iy, iw, ih) = BATTERY_INNER;
                let x = bx + bw * ix / BATTERY_W;
                let y = by + bh * iy / BATTERY.len() as i32;
                let h = (bh * ih / BATTERY.len() as i32).max(1);
                let w = (bw * iw / BATTERY_W) * percent as i32 / 100;
                let c = if charging {
                    TRAY_GOOD
                } else if percent <= 20 {
                    TRAY_WARN
                } else {
                    TRAY_DIM
                };
                if w > 0 {
                    els.push(pooled_solid(&mut pool, &mut idx, (x, y), (w, h), c));
                }
            }
            bar::Cell::Tray(i) => {
                let (есть_картинка, внимание) = state
                    .tray_apps
                    .as_ref()
                    .and_then(|a| a.items.get(i))
                    .map(|item| (item.icon.is_some(), item.status == crate::sni::Status::Attention))
                    .unwrap_or((false, false));
                // Подложка под значком приложения, которое не прислало
                // картинку: на ней рисуется первая буква (см. draw_tray_icon).
                if !есть_картинка {
                    rounded_tex(
                        state, renderer, &mut pool, &mut idx,
                        r.x, r.y + PAD_V, DOT, DOT, DOT / 3,
                        [1.0, 1.0, 1.0, 0.14], &mut slot, &mut els,
                    );
                }
                // «Просит внимания» — точка в правом верхнем углу значка.
                if внимание {
                    let d = (DOT / 4).max(4);
                    rounded_tex(
                        state, renderer, &mut pool, &mut idx,
                        r.x + DOT - d, r.y + PAD_V, d, d, d / 2,
                        TRAY_WARN, &mut slot, &mut els,
                    );
                }
            }
            _ => {}
        }
    }
    state.bar_ids = pool;

    // ── Текст и значки ───────────────────────────────────────────────────────
    // Текст ставится по вертикали от своей высоты, а не «на глаз»: у мелкой
    // даты она другая, и общая константа увела бы её вниз.
    let по_центру = |scale: i32| BAR_TOP + (BAR_H - crate::text::height(scale)) / 2;
    for item in &lay.cells {
        let r = item.rect;
        match item.cell {
            bar::Cell::Window(i) => {
                let Some(чип) = data.windows.get(i).cloned() else { continue };
                draw_bar_window_chip(state, renderer, r, &чип, slot, &mut els);
                slot += 1;
            }
            bar::Cell::WindowsMore(n) => {
                // Счётчик остатка — единственный чип без окна за спиной:
                // значка у него быть не может, поэтому app_id пустой.
                let чип = bar::WindowChip {
                    letter: format!("+{n}"),
                    app_id: String::new(),
                    title: String::new(),
                    focused: false,
                };
                draw_bar_window_chip(state, renderer, r, &чип, slot, &mut els);
                slot += 1;
            }
            bar::Cell::Clock => {
                let clock = data.clock.clone();
                draw_text_w(state, renderer, r.x, по_центру(bar::TEXT), &clock, bar::STRONG, bar::TEXT, BAR_TEXT, slot, &mut els);
                slot += 1;
            }
            bar::Cell::Date => {
                let date = data.date.clone();
                draw_text(state, renderer, r.x, по_центру(bar::TEXT_SMALL), &date, bar::TEXT_SMALL, BAR_DIM, slot, &mut els);
                slot += 1;
            }
            bar::Cell::Kb => {
                let kb = data.kb.clone();
                draw_text_w(state, renderer, r.x, по_центру(bar::TEXT), &kb, bar::STRONG, bar::TEXT, BAR_TEXT, slot, &mut els);
                slot += 1;
            }
            bar::Cell::Share => {
                let Some((код, гостей)) = data.share.clone() else { continue };
                // Цвет хоста из палитры участников (см. share::ЦВЕТА): чип
                // раздачи и стрелка самого хозяина у гостей на экране обязаны
                // быть одного цвета, иначе «жёлтый — это кто?».
                let цвет = крась(crate::share::цвет(0));
                let текст = bar::share_text(&код, гостей);
                draw_text_w(
                    state, renderer, r.x, по_центру(bar::TEXT),
                    &текст, bar::STRONG, bar::TEXT, цвет, slot, &mut els,
                );
                slot += 1;
            }
            bar::Cell::Volume => {
                let Some((percent, muted)) = data.volume else { continue };
                // Имена масок у панели СВОИ («bar-…»), хотя картинки те же, что
                // в полке. Ключ кэша в text.rs собирается из имени, размера и
                // цвета: совпади они с полкой — один и тот же буфер попал бы в
                // кадр дважды, а damage tracker индексируется по его Id. Полка
                // и панель видны одновременно.
                let (name, mask): (&str, &[u32]) = if muted {
                    ("bar-vol-muted", &VOLUME_MUTED)
                } else {
                    ("bar-vol", &VOLUME)
                };
                let color = if muted { TRAY_WARN } else { BAR_TEXT };
                let значок = bar::Rect { x: r.x, y: r.y, w: DOT, h: r.h };
                // Маска звука 24×18 — при высоте BAR_H/2 она шире ячейки в DOT
                // и залезала бы на проценты рядом.
                let h = mask_h_fit(VOLUME_W, VOLUME.len() as i32, BAR_H / 2, DOT);
                draw_mask(state, renderer, name, mask, VOLUME_W, значок, h, color, &mut els);
                let text = bar::percent_text(percent);
                draw_text_w(
                    state, renderer, r.x + DOT + BAR_H / 6, по_центру(bar::TEXT),
                    &text, bar::STRONG, bar::TEXT,
                    if muted { TRAY_WARN } else { BAR_DIM }, slot, &mut els,
                );
                slot += 1;
            }
            bar::Cell::Battery => {
                let Some((percent, charging)) = data.battery else { continue };
                let тревога = percent <= 20 && !charging;
                let color = if тревога { TRAY_WARN } else { BAR_TEXT };
                let значок = bar::Rect { x: r.x, y: r.y, w: DOT, h: r.h };
                let (_, _, _, h) = battery_box_fit(значок, DOT);
                draw_mask(state, renderer, "bar-battery", &BATTERY, BATTERY_W, значок, h, color, &mut els);
                let text = bar::percent_text(percent);
                draw_text_w(
                    state, renderer, r.x + DOT + BAR_H / 6, по_центру(bar::TEXT),
                    &text, bar::STRONG, bar::TEXT,
                    if тревога { TRAY_WARN } else { BAR_DIM }, slot, &mut els,
                );
                slot += 1;
            }
            bar::Cell::Tray(i) => {
                draw_tray_icon(state, renderer, i, r, slot, &mut els);
                slot += 1;
            }
            _ => {}
        }
    }

    // Разворот по той же причине, что и в полке: собрано от заднего плана к
    // переднему, а в кадре порядок обратный.
    els.reverse();
    els
}

// ── Предпросмотр по наведению на панель ──────────────────────────────────────

/// Предпросмотр окна или стола под курсором на панели.
///
/// **Что здесь показывается и почему именно так.** Наведение на чип окна даёт
/// не сам этот кадр окна, а его ОКРУЖЕНИЕ: стол целиком, с наведённым окном,
/// подсвеченным рамкой. Чип отвечает на вопрос «где оно и что рядом», а не «как
/// оно выглядит» — как выглядит, видно и так, стоит переключиться. Наведение на
/// значок стола показывает тот же кадр без подсветки: весь стол.
///
/// **26.08.2026 — карточка стала маленькой миникартой.** Три правки по прямым
/// жалобам, и все три об одном: карточка обязана показывать то же, что увидишь,
/// перейдя на этот стол.
/// 1. Кадр строится вокруг ПОСЛЕДНЕЙ КАМЕРЫ ЭТОГО СТОЛА (`Parallax::preview_base`),
///    а не вокруг начала координат. Раньше сюда шёл `screen_area()` —
///    прямоугольник (0,0)…(экран); на бесконечном холсте это почти никогда не
///    то место, где стоят окна.
/// 2. Всё, что рисуется, ОБРЕЗАНО полем карточки. Раньше подложки окон резались
///    только по нижней кромке (шторкой), поэтому окно, стоящее за пределами
///    показанного куска холста, вылезало серым квадратом прямо на стол — это и
///    была жалоба «показывает квадратами другие окна за пределами обзора».
/// 3. По карточке можно панить (ЛКМ), зумить (колесо) и кликать по окнам, как
///    по карте: вид живёт в `Parallax::preview_*`, геометрия — в
///    `Parallax::preview_view`. ОДНА точка правды на кадр и на ввод: разъехавшись,
///    они унесут клик мимо окна.
///
/// Содержимое — ЖИВОЕ, тем же приёмом, что и миниатюры карты: рендер
/// поверхностей окна в точку, посчитанную до масштабирования
/// (`MiniView::pre_scale`), потом `RescaleRenderElement` от угла поля и
/// `CropRenderElement` по его краю. Совпасть эти две арифметики обязаны точно,
/// иначе содержимое разъезжается с рамкой.
fn build_bar_preview(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    // Ячейку берём НЕ из `bar_hover`, а из `preview_cell`: курсор мог уже уйти
    // с панели, а карточке ещё ехать (см. anim::tick).
    let Some(ячейка) = state.preview_cell else { return els };
    // Геометрия и вид — из state: по ним же считаются хит-тест окна под
    // курсором, пан и зум карточки.
    let Some((g, вид)) = state.preview_view() else { return els };
    let видимость = crate::canvas::preview_fade(state.preview_anim);
    if видимость <= 0.004 {
        return els;
    }
    let кадры = state.preview_frames();
    if кадры.is_empty() {
        return els;
    }

    // Что подсвечивать: у чипа — его окно, плюс всегда то, что под курсором на
    // самой карточке («сейчас кликнешь сюда» — как в карте окон).
    let выделить = match ячейка {
        bar::Cell::Window(i) => state.bar_window_at(i),
        _ => None,
    };
    let наведено = state.preview_hit().and_then(|p| state.preview_window_at(p));

    let поле = g.content;
    let карточка = g.card;

    // ── Живое содержимое ─────────────────────────────────────────────────────
    let mut содержимое = Vec::new();
    for кадр in &кадры {
        // Окно целиком за пределами показанного куска холста не рисуем вовсе:
        // сурфейсы всё равно обрежутся, а работа была бы настоящая.
        if вид.rect(поле.loc, поле.size, кадр.rect).intersection(поле).is_none() {
            continue;
        }
        let сурфейсы: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = кадр.window.render_elements(
            renderer,
            вид.pre_scale(поле.loc, поле.size, кадр.root),
            smithay::utils::Scale::from(1.0),
            видимость,
        );
        содержимое.extend(сурфейсы.into_iter().filter_map(|el| {
            let ужатое = RescaleRenderElement::from_element(el, поле.loc, вид.scale);
            CropRenderElement::from_element(ужатое, 1.0, поле).map(OutputRenderElements::Minimap)
        }));
    }

    // Подложки и рамки — списком «прямоугольник + цвет», обрезаются ПОЛЕМ
    // карточки (см. пункт 2 в шапке функции).
    let mut рамки: Vec<(Rectangle<i32, Physical>, [f32; 4])> = Vec::new();
    let mut подложки: Vec<(Rectangle<i32, Physical>, [f32; 4])> = Vec::new();
    const РАМКА_ЧИП: [f32; 4] = [0.55, 0.75, 1.0, 0.95];
    const РАМКА_НАВЕДЕНИЕ: [f32; 4] = [1.0, 1.0, 1.0, 0.95];
    const КОНТУР: [f32; 4] = [1.0, 1.0, 1.0, 0.22];
    const ПОДЛОЖКА: [f32; 4] = [0.09, 0.10, 0.13, 0.55];
    for кадр in &кадры {
        let r = вид.rect(поле.loc, поле.size, кадр.rect);
        if r.intersection(поле).is_none() {
            continue;
        }
        let (толщина, цвет) = if наведено.as_ref() == Some(&кадр.window) {
            (2, РАМКА_НАВЕДЕНИЕ)
        } else if выделить.as_ref() == Some(&кадр.window) {
            (2, РАМКА_ЧИП)
        } else {
            (1, КОНТУР)
        };
        for (x, y, w, h) in [
            (r.loc.x, r.loc.y, r.size.w, толщина),
            (r.loc.x, r.loc.y + r.size.h - толщина, r.size.w, толщина),
            (r.loc.x, r.loc.y, толщина, r.size.h),
            (r.loc.x + r.size.w - толщина, r.loc.y, толщина, r.size.h),
        ] {
            рамки.push((
                Rectangle::new(Point::from((x, y)), Size::from((w.max(1), h.max(1)))),
                цвет,
            ));
        }
        // Подложка — ПОД содержимым: окно без буфера иначе не видно вовсе, а
        // прозрачный терминал сливается с карточкой (та же правка, что в карте
        // окон: подложка ТЁМНАЯ, а не серая).
        подложки.push((r, ПОДЛОЖКА));
    }

    // ── Сборка: от переднего плана к заднему ─────────────────────────────────
    //
    // Порядок здесь не косметика: список кадра идёт от ПЕРЕДНЕГО плана к
    // заднему, и всё, что добавлено позже, лежит НИЖЕ. Подпись поэтому первая
    // (иначе фон карточки накрыл бы её), стекло — последнее.
    let подпись = match ячейка {
        bar::Cell::Window(i) => выделить.as_ref()
            .and_then(|w| crate::xwin::app_id(w).or_else(|| crate::xwin::title(w)))
            .unwrap_or_else(|| тф!("Окно {}", "Window {}", i + 1)),
        bar::Cell::Tag(m) => тф!("Стол {}", "Workspace {}", m.trailing_zeros() + 1),
        _ => String::new(),
    };
    let высота_подписи = crate::text::height(bar::TEXT_SMALL);
    let подпись_y = поле.loc.y + поле.size.h
        + ((карточка.loc.y + карточка.size.h - поле.loc.y - поле.size.h - высота_подписи) / 2)
            .max(0);
    // Строку не обрежешь по половине (текст рисуется целым буфером) — пока
    // карточка мала, подписи просто нет.
    if !подпись.is_empty() && подпись_y + высота_подписи <= карточка.loc.y + карточка.size.h {
        let ширина = (карточка.size.w - crate::canvas::PREVIEW_PAD * 2).max(1);
        let подпись = bar::fit_text(&подпись, bar::TEXT_SMALL, ширина);
        let tw = crate::text::width(&подпись, bar::TEXT_SMALL);
        draw_text(
            state, renderer,
            карточка.loc.x + (карточка.size.w - tw) / 2,
            подпись_y,
            &подпись, bar::TEXT_SMALL,
            с_альфой([0.86, 0.90, 0.96, 0.85], видимость), 0, &mut els,
        );
    }

    let mut pool = std::mem::take(&mut state.preview_ids);
    let mut idx = 0usize;
    let mut плашка = |r: Rectangle<i32, Physical>, цвет: [f32; 4],
                      out: &mut Vec<OutputRenderElements>| {
        if let Some(i) = r.intersection(поле) {
            if i.size.w > 0 && i.size.h > 0 {
                out.push(pooled_solid(
                    &mut pool, &mut idx, (i.loc.x, i.loc.y), (i.size.w, i.size.h),
                    с_альфой(цвет, видимость),
                ));
            }
        }
    };
    for (r, цвет) in &рамки {
        плашка(*r, *цвет, &mut els);
    }
    els.extend(содержимое);
    for (r, цвет) in &подложки {
        плашка(*r, *цвет, &mut els);
    }

    // Радиус — не больше половины стороны: у только начавшей раскрываться
    // карточки скругление в 12 вырождается и мигает.
    let радиус = 12.min(карточка.size.h / 2).min(карточка.size.w / 2).max(0);
    // Со стеклом плашка карточки полупрозрачная, без него — почти глухая:
    // ровно та же развилка, что у меню (см. `меню_фон`) и у карты окон.
    let есть_стекло = state.blur_tex.is_some();
    rounded_solid(
        &mut pool, &mut idx,
        карточка.loc.x, карточка.loc.y, карточка.size.w, карточка.size.h, радиус,
        с_альфой(
            if есть_стекло { [0.05, 0.05, 0.08, 0.55] } else { [0.05, 0.05, 0.08, 0.90] },
            видимость,
        ),
        &mut els,
    );
    state.preview_ids = pool;
    // Стекло — в самом низу списка: здесь он идёт от ПЕРЕДНЕГО плана к заднему
    // и не разворачивается (в отличие от панели и полки).
    if let Some(el) = стекло(
        state, renderer,
        карточка.loc.x, карточка.loc.y, карточка.size.w, карточка.size.h,
        радиус, БЛЮР_ПРЕДПРОСМОТР,
    ) {
        els.push(el);
    }
    els
}

/// Чип окна в левом острове: скруглённая плашка со значком приложения.
///
/// Плашка нужна не для красоты: чипов на столе может быть девять подряд, и
/// голые значки слились бы в строку без границ. Активное окно выделено ярче —
/// это единственное, что осталось от прежнего текстового заголовка (он и
/// сообщал-то ровно «какое окно активно»).
///
/// Значок приходит готовым буфером из `Parallax::chip_icons` — он собирается на
/// появлении окна, а не здесь: поиск по теме значков лезет в файловую систему,
/// и делать это внутри кадра нельзя. Не нашлось — рисуем букву, как раньше.
fn draw_bar_window_chip(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    cell: bar::Rect,
    чип: &bar::WindowChip,
    slot: usize,
    out: &mut Vec<OutputRenderElements>,
) {
    const АКЦЕНТ: [f32; 4] = [0.55, 0.75, 1.0, 0.95];
    const БУКВА: [f32; 4] = [0.86, 0.90, 0.96, 0.85];
    const БУКВА_АКТИВ: [f32; 4] = [0.72, 0.85, 1.0, 1.0];
    /// Полоска под значком активного окна: ширина и толщина.
    const ЧЕРТА_W: i32 = 10;
    const ЧЕРТА_H: i32 = 2;
    /// Зазор между низом значка и полоской.
    const ЧЕРТА_ЗАЗОР: i32 = 2;

    let сторона = DOT;
    let x = cell.x;
    // Значок сдвинут вверх на половину полоски: без этого пара «значок +
    // полоска» стояла бы в клетке ниже середины, и чипы читались бы как
    // съехавшие относительно значков столов слева.
    let y = cell.y + (cell.h - сторона) / 2 - (ЧЕРТА_H + ЧЕРТА_ЗАЗОР) / 2;

    // Активное окно отмечает ПОЛОСКА ПОД значком, а не плашка вокруг него.
    //
    // 29.08.2026, прямая просьба Ярика: «сделай чистые иконки без обводки».
    // Плашка (белая 10%, у активного — голубая) была той самой обводкой: она
    // же и съедала 4 px стороны под отступ, и подменяла собой фон цветного
    // значка. Сведений при этом не теряем — «какое окно активно» полоска
    // сообщает ровно так же, — а значок остаётся чистой картинкой.
    if чип.focused {
        let mut pool = std::mem::take(&mut state.bar_ids);
        let mut idx = pool.len();
        rounded_solid(
            &mut pool, &mut idx,
            x + (сторона - ЧЕРТА_W) / 2, y + сторона + ЧЕРТА_ЗАЗОР,
            ЧЕРТА_W, ЧЕРТА_H, ЧЕРТА_H / 2, АКЦЕНТ, out,
        );
        state.bar_ids = pool;
    }
    if let Some((buf, (iw, ih))) = state.chip_icons.get(&чип.app_id) {
        // ОДИН В ОДИН, без пересчёта размера — ровно как значок трея рядом.
        // Буфер уже растрирован в `bar::CHIP_ICON` (см. ensure_chip_icon), то
        // есть поле внутри плашки заложено в сам значок. Раньше здесь стояло
        // домасштабирование к тому же полю: GPU тянул готовый растр билинейно,
        // и чипы выглядели мылом рядом с чистым треем.
        let (dw, dh) = (*iw, *ih);
        let ix = x + (сторона - dw) / 2;
        let iy = y + (сторона - dh) / 2;
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((ix as f64, iy as f64)),
            buf,
            None,
            None,
            Some(Size::<i32, Logical>::from((dw, dh))),
            Kind::Unspecified,
        ) {
            Ok(el) => {
                out.push(OutputRenderElements::Memory(el));
                return;
            }
            Err(e) => tracing::warn!("plx/udev: chip icon: {:?}", e),
        }
    }

    let цвет = if чип.focused { БУКВА_АКТИВ } else { БУКВА };
    let w = crate::text::width_of(&чип.letter, bar::STRONG, bar::TEXT_SMALL);
    let tx = x + (сторона - w) / 2;
    let ty = y + (сторона - crate::text::height(bar::TEXT_SMALL)) / 2;
    draw_text_w(state, renderer, tx, ty, &чип.letter, bar::STRONG, bar::TEXT_SMALL, цвет, slot, out);
}

/// Значок приложения в трее: готовая текстура из sni.rs.
///
/// Текстура собирается один раз на смену списка предметов (`handle_sni_event`)
/// и приходит сюда уже нужного размера — из пикселей приложения (`IconPixmap`)
/// либо из файла значка темы, найденного по `IconName` (см. icons.rs).
/// Буква в кружке осталась только на случай, когда нет ни того, ни другого:
/// раньше её получал КАЖДЫЙ, кто не прислал пиксели.
fn draw_tray_icon(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    index: usize,
    cell: bar::Rect,
    slot: usize,
    out: &mut Vec<OutputRenderElements>,
) {
    let Some(apps) = state.tray_apps.as_ref() else { return };
    let Some(item) = apps.items.get(index) else { return };
    // Приглушённый значок у «неважных» предметов (Status = Passive): по
    // спецификации хост вправе их прятать, но пропадающий значок пугает
    // сильнее, чем бледный.
    let alpha = if item.status == crate::sni::Status::Passive { 0.55 } else { 1.0 };

    // Размер берём У БУФЕРА, а не у `item.icon`: у значка, найденного в теме,
    // поле icon пустое — картинка живёт только в буфере.
    if let Some((buf, (iw, ih))) = apps.buffer(&item.key) {
        let x = cell.x + (DOT - iw) / 2;
        let y = cell.y + (cell.h - ih) / 2;
        match MemoryRenderBufferRenderElement::from_buffer(
            renderer,
            Point::<f64, Physical>::from((x as f64, y as f64)),
            buf,
            Some(alpha),
            None,
            Some(Size::<i32, Logical>::from((iw, ih))),
            Kind::Unspecified,
        ) {
            Ok(el) => out.push(OutputRenderElements::Memory(el)),
            Err(e) => tracing::warn!("plx/udev: tray icon: {:?}", e),
        }
        return;
    }

    // Запасной значок: первая буква Id заглавной.
    let буква: String = item
        .id
        .chars()
        .find(|c| c.is_alphanumeric())
        .map(|c| c.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    let w = crate::text::width_of(&буква, bar::STRONG, bar::TEXT);
    let x = cell.x + (DOT - w) / 2;
    let y = cell.y + (cell.h - crate::text::height(bar::TEXT)) / 2;
    let color = [BAR_TEXT[0], BAR_TEXT[1], BAR_TEXT[2], BAR_TEXT[3] * alpha];
    draw_text_w(state, renderer, x, y, &буква, bar::STRONG, bar::TEXT, color, slot, out);
}

/// Закрыт ли экран целиком фоновой layer-поверхностью (обои plx-wall).
///
/// Если да, то параллакс-сетка под ней невидима, и строить её незачем: это
/// ~8 элементов на КАЖДЫЙ кадр, которые damage tracker потом ещё и сравнивает
/// с прошлым кадром. Обои — самый обычный случай, так что экономия постоянная.
fn background_covers_output(state: &Parallax, output: &Output, screen: Size<i32, Logical>) -> bool {
    let output = state.layer_output.clone().unwrap_or_else(|| output.clone());
    let map = layer_map_for_output(&output);
    let covered = map
        .layers()
        .filter(|l| l.layer() == WlrLayer::Background)
        .any(|l| {
            map.layer_geometry(l).is_some_and(|g| {
                g.loc.x <= 0 && g.loc.y <= 0
                    && g.loc.x + g.size.w >= screen.w
                    && g.loc.y + g.size.h >= screen.h
            })
        });
    covered
}

/// Рендер layer-поверхностей (wlr-layer-shell) для заданных слоёв.
/// Каждая layer-поверхность рисуется через render_elements_from_surface_tree
/// в позиции, которую ей назначил LayerMap.
/// Обои на бесконечном холсте: ОДНА копия, едущая за камерой с затуханием.
///
/// **Что было и почему поменялось.** Обои — это обычная layer-поверхность
/// (`plx-wall`), приклеенная к экрану: она всегда ровно в размер выхода и никуда
/// не двигается. На бесконечном холсте это читается как «картинка нарисована на
/// стекле монитора» — окна уезжают, обои стоят. Поэтому картинку положили на
/// холст и повторили сеткой во все стороны: холст покрыт целиком, обои едут с
/// камерой один в один.
///
/// Ценой была ВИДИМАЯ ПОВТОРЯЕМОСТЬ. Стоило камере уйти от дома монитора — а
/// уходит она от любой прокрутки ленты, — как в экран попадали два-четыре куска
/// одной фотографии со швом посередине. Замер 26.08.2026: камера 2856 на экране
/// 1920 давала шов на x≈984, а в логе живого сеанса стояло «фон 4», то есть 2×2
/// плитки в каждом кадре. Снаружи это и есть «обои дублируются». (Годами не
/// вылезало только потому, что `infinite_wallpaper` из-за грабли с булевыми
/// ключами Lua молча стоял в false и обои были приклеены к экрану, см.
/// `config.rs`.)
///
/// **Что теперь.** Копия ровно одна. Она чуть больше экрана (`ОБОИ_ЗАПАС` с
/// каждой стороны) и ездит за камерой ЗАТУХАЮЩЕ: сдвиг равен запасу, умноженному
/// на `tanh` пути камеры. `tanh` строго меньше единицы, поэтому картинка не
/// отрывается от края экрана ни при какой камере — шва не бывает по построению,
/// а ощущение «обои лежат на холсте, а не на стекле» сохраняется: на первом
/// экране хода обои проходят почти весь свой запас.
///
/// **Зум обои не масштабирует** — намеренно. Отдалиться можно до птичьего
/// глаза, и любая привязка к зуму означала бы либо картинку меньше экрана (то
/// есть снова повтор, чтобы закрыть дыры), либо разъезд с блюром. Обои —
/// задник; он и должен вести себя как бесконечно далёкий план.
///
/// **Почему не проще — не отрисовать layer-поверхность в нужном месте.** Потому
/// что damage tracker индексируется по `Element::id()`, а
/// `render_elements_from_surface_tree` берёт Id у самой поверхности и кладёт её
/// туда, куда назначил LayerMap. Поэтому текстура фонового слоя достаётся
/// напрямую и рисуется своим элементом со своим Id — тем же приёмом, что и
/// сплошные прямоугольники (`pooled_solid`).
///
/// Если текстуры нет (буфер ещё не пришёл, чужой формат) — возвращаем None, и
/// вызывающий рисует слой по-старому, приклеенным к экрану. Отказ здесь обязан
/// быть мягким: без обоев экран чёрный, а это самое заметное, что может
/// сломаться.
/// Куда лечь единственной копии обоев: прямоугольник в ФИЗИЧЕСКИХ пикселях
/// экрана, начало отсчёта — его левый верхний угол.
///
/// Вынесено отдельно и без рендера НАРОЧНО: это единственная арифметика во всей
/// затее, и ошибиться в ней легко (см. тест `обои_укрывают_экран_на_любой_камере`).
struct МестоОбоев {
    x: f64,
    y: f64,
    w: f64,
    h: f64,
}

/// Запас картинки за каждым краем экрана — он же предел хода обоев.
/// 5% от экрана: на 2560 это 128 px в каждую сторону — заметно глазу при
/// панорамировании и не требует заметного увеличения картинки.
const ОБОИ_ЗАПАС: f64 = 0.05;

/// Сколько экранов холста уходит на почти весь ход. Полтора: первый экран
/// панорамирования даёт ~0.58 запаса, три экрана — 0.96, дальше обои стоят.
const ОБОИ_ДАЛЬНОСТЬ: f64 = 1.5;

/// Какую долю запаса обои ВПРАВЕ пройти. Не единица нарочно: `tanh` для больших
/// аргументов даёт ровно 1.0, и на пределе хода край картинки встал бы ровно в
/// край экрана — а размер элемента округляется до целого пикселя, и одного
/// такого округления хватило бы на чёрную нитку по краю. Замер 26.08.2026:
/// у монитора, чья камера ушла от дома на миллион (`monitors::ШАГ_ДОМА`),
/// картинка вставала краем ровно в x=0. Десятая часть запаса в резерве стоит
/// 13 px хода из 128 и убирает этот класс ошибок целиком.
const ОБОИ_ДОЛЯ_ХОДА: f64 = 0.9;

/// Сколько экранов «пути» стоит один рабочий стол. Столы лежат в ОДНОМ
/// прямоугольнике холста (`tiling::screen_area`), камера при Super+N не
/// сдвигается ни на пиксель — значит, и обои сами по себе стоят намертво. Это
/// и была жалоба Ярика 26.08.2026: «обои двигаются только если панить либо
/// зумить». Полэкрана на стол даёт первым переходам заметный ход (tanh(0.5/1.5)
/// = 0.32 запаса — на 2560 это ~40 px за переход), а дальним столам — затухание,
/// то же самое, что у панорамирования.
const ОБОИ_ШАГ_СТОЛА: f64 = 0.5;

fn wallpaper_placement(
    камера: (f64, f64),
    стол: f64,
    картинка: (i32, i32),
    экран: (i32, i32),
) -> Option<МестоОбоев> {
    if картинка_негодна(картинка)
        || экран.0 <= 0
        || экран.1 <= 0
        || !камера.0.is_finite()
        || !камера.1.is_finite()
        || !стол.is_finite()
    {
        return None;
    }
    let (эw, эh) = (экран.0 as f64, экран.1 as f64);
    // Накрываем экран целиком — тот же закон «заполнить», по которому plx-wall
    // кроит кадр своим viewport'ом, — и добавляем запас с каждой стороны:
    // именно в нём и живёт весь ход.
    let покрытие = (эw / картинка.0 as f64).max(эh / картинка.1 as f64);
    let к = покрытие * (1.0 + 2.0 * ОБОИ_ЗАПАС);
    let (w, h) = (картинка.0 as f64 * к, картинка.1 as f64 * к);
    // Ход по оси — то, что вылезло за края экрана, пополам и с резервом.
    let ход_x = (w - эw) / 2.0 * ОБОИ_ДОЛЯ_ХОДА;
    let ход_y = (h - эh) / 2.0 * ОБОИ_ДОЛЯ_ХОДА;
    // Стол добавляется к пути камеры ВИРТУАЛЬНЫМ ходом, а не отдельным
    // слагаемым к сдвигу: так предел хода остаётся один на всех (его держит
    // `tanh`), и сумма двух источников не может вытолкнуть картинку за край.
    // Столы в parallax стоят на одном месте холста — камера при Super+N не едет
    // никуда, — поэтому «расстояние» между ними приходится назначить.
    let путь_x = камера.0 + стол * эw * ОБОИ_ШАГ_СТОЛА;
    // Знак минус: холст уезжает вправо — обои уходят влево, как и окна.
    let сдвиг_x = -ход_x * (путь_x / (эw * ОБОИ_ДАЛЬНОСТЬ)).tanh();
    let сдвиг_y = -ход_y * (камера.1 / (эh * ОБОИ_ДАЛЬНОСТЬ)).tanh();
    Some(МестоОбоев {
        x: (эw - w) / 2.0 + сдвиг_x,
        y: (эh - h) / 2.0 + сдвиг_y,
        w,
        h,
    })
}

fn картинка_негодна(картинка: (i32, i32)) -> bool {
    картинка.0 <= 0 || картинка.1 <= 0
}

/// Где обои лежат НА ЭКРАНЕ прямо сейчас — тем же расчётом, что и кадр.
///
/// Пустой список значит «обоев в кадре нет»: бесконечные обои выключены, место
/// не посчиталось или текстуры ещё нет. Размытие тогда растягивает исходник на
/// весь кадр — прежнее поведение, которое верно ровно для приклеенного к экрану
/// фонового слоя (`build_layer_elements` вместо своей отрисовки).
///
/// Список, а не одна штука, — потому что этого ждёт `blur::Блюр::размыть`, и
/// менять его форму ради одного элемента незачем: обоев в кадре бывает ноль или
/// одна.
fn wallpaper_screen_place(
    state: &Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
    экран: Size<i32, Physical>,
) -> Vec<crate::blur::Плитка> {
    if !state.lua_config.infinite_wallpaper {
        return Vec::new();
    }
    // Слой берём тот же, что и сам кадр (`build_wallpaper_backdrop`): свой у
    // этого монитора, а на время отрисовки «свой» — это активный, потому что
    // `render_surface` уже перешёл на точку зрения рисуемого выхода
    // (`monitors::войти_в_монитор` подменяет и `layer_output`). Разъехаться
    // этим двум нельзя — блюр размывал бы не то, что нарисовано.
    let _ = output;
    let Some(поверхность) = state.фоновая_поверхность() else {
        return Vec::new();
    };
    let Some((_, вид)) = wallpaper_texture_sized(&поверхность, renderer) else {
        return Vec::new();
    };
    // Размер — по ПОВЕРХНОСТИ, ровно как в build_wallpaper_backdrop, и отсчёт
    // от ДОМА своего монитора: оба расчёта обязаны совпадать до пикселя.
    let размер = вид.dst;
    let дом = state.монитор_дом();
    let Some(место) = wallpaper_placement(
        (
            state.viewport.cam_x - дом.x as f64,
            state.viewport.cam_y - дом.y as f64,
        ),
        state.обои_фаза(),
        (размер.w, размер.h),
        (экран.w, экран.h),
    ) else {
        return Vec::new();
    };
    vec![crate::blur::Плитка {
        x: место.x,
        y: место.y,
        w: место.w,
        h: место.h,
    }]
}

/// Текстура фонового слоя (обои) вместе с её логическим размером.
///
/// Одна точка добычи на всех: её зовут и плитки бесконечных обоев, и размытие
/// фона. Разъехаться этим двум нельзя — они обязаны показывать одну картинку.
/// Кусок размытого фона под одну плашку: та же текстура кадра, но показанная
/// ровно в её прямоугольнике и обрезанная её же скруглением.
///
/// Текстура размыта в уменьшенном виде (см. blur::УЖАТИЕ), поэтому исходный
/// прямоугольник делится на то же число: `src` задаётся в координатах САМОЙ
/// текстуры, а не экрана. Промахнуться здесь — значит показать под панелью
/// кусок фона из другого места экрана, и заметно это будет сразу.
///
/// **Номер заплаты закреплён за плашкой** (см. `БЛЮР_*` ниже): по нему берётся
/// постоянный Id из `Parallax::blur_ids`. Две плашки с одним номером в одном кадре
/// недопустимы — damage tracker индексируется по Id, и вторая затёрла бы
/// историю первой.
fn build_blur_patch(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    r: bar::Rect,
    radius: f32,
    // Насколько плотно заплата перекрывает резкий фон под собой. У плашек
    // интерфейса — 0.85 (лёгкое затемнение, как в macOS); под окном — 1.0:
    // там смешивание делает сама прозрачность окна, и просвечивающие сквозь
    // заплату 15% РЕЗКИХ обоев смазали бы весь эффект.
    alpha: f32,
    слот: usize,
) -> Option<OutputRenderElements> {
    let текстура = state.blur_tex.clone()?;
    let шейдер = state.blur_shape.clone()?;
    if r.w <= 0 || r.h <= 0 {
        return None;
    }
    let ctx = {
        use smithay::backend::renderer::Renderer as _;
        renderer.context_id()
    };
    let к = crate::blur::УЖАТИЕ as f64;
    let src = Rectangle::<f64, Logical>::new(
        (r.x as f64 / к, r.y as f64 / к).into(),
        ((r.w as f64 / к).max(1.0), (r.h as f64 / к).max(1.0)).into(),
    );
    let прямоугольник = Rectangle::<i32, Physical>::new(
        (r.x, r.y).into(),
        (r.w, r.h).into(),
    );
    // Id — из пула по номеру заплаты, а не свежий на кадр (см. Parallax::blur_ids).
    while state.blur_ids.len() <= слот {
        state.blur_ids.push(Id::new());
    }
    let id = state.blur_ids[слот].clone();
    let el = TextureRenderElement::from_static_texture(
        id,
        ctx,
        Point::<f64, Physical>::from((r.x as f64, r.y as f64)),
        текстура,
        1,
        Transform::Normal,
        Some(alpha),
        Some(src),
        Some(Size::<i32, Logical>::from((r.w, r.h))),
        None,
        Kind::Unspecified,
    );
    let обрезанное = CropRenderElement::from_element(el, 1.0, прямоугольник)?;
    Some(OutputRenderElements::Blur(crate::rounded::Rounded::from_rect(
        обрезанное, &шейдер, прямоугольник, radius,
    )))
}

// Номера заплат размытия. Один номер — одна плашка на экране; острова панели
// занимают три подряд, потому что их три и они видны одновременно.
const БЛЮР_ОСТРОВ: usize = 0; // 0 — левый, 1 — центральный, 2 — правый
const БЛЮР_ПОЛКА: usize = 3;
const БЛЮР_МЕНЮ: usize = 4;
const БЛЮР_ПРЕДПРОСМОТР: usize = 5;
/// Карта окон: её карточка тоже стоит на матовом стекле (26.08.2026 — вместо
/// обоев внутри карты, см. `build_minimap_elements`).
const БЛЮР_КАРТА: usize = 6;
/// С этого номера идут заплаты ПОД ОКНАМИ — по одной на прозрачное окно в
/// кадре. Номер стабилен внутри кадра (порядок обхода окон детерминирован), а
/// значит стабилен и `Id` заплаты: ровно то, чего требует damage tracking
/// (см. `pooled_solid`).
const БЛЮР_ОКНО: usize = 8;

/// «Матовое стекло» под плашку интерфейса: заплата размытого фона в её
/// прямоугольнике.
///
/// Зачем отдельная обёртка над `build_blur_patch`: плашки задают себя обычными
/// числами (полка, меню, карточка предпросмотра), а не `bar::Rect`, и размер
/// буфера кадра каждой пришлось бы добывать у выхода самой. Здесь это делается
/// один раз.
///
/// Отказ мягкий и обязан таким быть: без размытия (выключено в конфиге,
/// шейдер не собрался, обоев нет) плашка просто останется прежней
/// полупрозрачной заливкой.
#[allow(clippy::too_many_arguments)]
fn стекло(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    x: i32, y: i32, w: i32, h: i32,
    radius: i32,
    слот: usize,
) -> Option<OutputRenderElements> {
    if state.blur_tex.is_none() {
        return None;
    }
    build_blur_patch(state, renderer, bar::Rect { x, y, w, h }, radius as f32, 0.85, слот)
}

/// Текстура фоновой поверхности вместе с её ВИДОМ.
///
/// Вид (`SurfaceView`) — это то, что клиент задал через `wp_viewporter`: `src`
/// — какой кусок буфера показывать, `dst` — в какой логический размер его
/// растягивать. Раньше отсюда возвращался `buffer_size`, и это было верно ровно
/// до того дня, когда plx-wall перешёл на viewporter: буфер у него теперь равен
/// КАДРУ ВИДЕО (1920×1080), а поверхность — экрану (1920×1280), растягивает
/// композитор. Обои-плитки, считавшие шаг сетки по буферу, из-за этого
/// повторялись поперёк экрана, не совпадая с ним ни размером, ни пропорцией.
fn wallpaper_texture_sized(
    поверхность: &smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
    renderer: &mut GlesRenderer,
) -> Option<(GlesTexture, smithay::backend::renderer::utils::SurfaceView)> {
    let ctx = {
        use smithay::backend::renderer::Renderer as _;
        renderer.context_id()
    };
    // Буфер обоев импортируем в ЭТОТ рендерер САМИ, а не надеемся на прошлый
    // кадр. Раньше текстуру просто читали: она появлялась побочным действием
    // отрисовки фонового слоя (smithay импортирует буфер при сборке элементов).
    // Пока plx-wall крутит ВИДЕО, каждый его коммит роняет прежнюю текстуру, и
    // если между двумя кадрами композитора пришёл новый буфер — на этом кадре
    // текстуры нет вовсе. У живого сеанса (200 кадр/с против 30 кадров видео)
    // это редкость, а вот в headless-харнессе, где кадр рисуется только по
    // команде `shot`, новый буфер успевает прийти ВСЕГДА — блюра не было ни
    // разу (замер 24.08.2026: «у слоя Background нет текстуры, размер буфера
    // 1920x1080», то есть буфер на месте, а импорта нет).
    // `import_surface` при уже импортированной текстуре — пустышка (проверяет
    // `Entry::Vacant`), поэтому лишней работы на горячем пути не появляется.
    let _ = with_states(поверхность, |states| {
        smithay::backend::renderer::utils::import_surface(renderer, states)
    });
    with_states(поверхность, |states| {
        let data = states.data_map
            .get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()?;
        let сост = data.lock().ok()?;
        let tex = сост.texture(ctx.clone())?.clone();
        let вид = сост.view()?;
        Some((tex, вид))
    })
}

/// Та же текстура, но по выходу: сама находит фоновый слой.
fn wallpaper_texture(
    state: &Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Option<GlesTexture> {
    let слой_выход = state.layer_output.clone().unwrap_or_else(|| output.clone());
    // Фоновый слой ищем ПО ВСЕМ мониторам, а не только в своей карте: plx-wall
    // вешает обои на один-единственный выход (см. `Parallax::фоновая_поверхность`).
    let Some(поверхность) = state.фоновая_поверхность() else {
        let слоёв = layer_map_for_output(&слой_выход).layers().count();
        почему_нет_блюра(&format!(
            "there is no Background layer anywhere (output {:?} has {} layers)", слой_выход.name(), слоёв,
        ));
        return None;
    };
    if let Some((tex, _)) = wallpaper_texture_sized(&поверхность, renderer) {
        return Some(tex);
    }
    // Сюда попадаем, только когда текстуры нет и ПОСЛЕ импорта — то есть
    // виноват не порядок кадра. Разбор по частям: «клиент не отдал буфер»
    // (состояние=false / размер=None) и «буфер есть, а импорт не удался»
    // лечатся по-разному.
    let (есть_состояние, размер) = with_states(&поверхность, |states| {
        let Some(data) = states.data_map.get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()
        else {
            return (false, None);
        };
        let Ok(сост) = data.lock() else { return (true, None) };
        (true, сост.buffer_size())
    });
    почему_нет_блюра(&format!(
        "the Background layer has no texture even after the import (state={} buffer size={:?})",
        есть_состояние, размер,
    ));
    None
}

fn build_wallpaper_backdrop(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Option<Vec<OutputRenderElements>> {
    if !state.lua_config.infinite_wallpaper {
        return None;
    }
    let mode = output.current_mode()?;
    let экран = (mode.size.w, mode.size.h);

    // Текстура фонового слоя. Берём ПЕРВЫЙ Background-слой ЛЮБОГО монитора:
    // обои у plx-wall одни на весь сеанс и лежат в карте одного выхода, а если их
    // вдруг несколько, тайлить стопку смысла нет.
    let поверхность = state.фоновая_поверхность()?;
    // ContextId — у трейта Renderer, а он в этом файле не в области видимости
    // (импортирован лишь ImportDma). Импортируем точечно, чтобы не тащить сюда
    // весь трейт ради одного вызова.
    let ctx = {
        use smithay::backend::renderer::Renderer as _;
        renderer.context_id()
    };
    let (текстура, вид) = wallpaper_texture_sized(&поверхность, renderer)?;
    // Размер картинки — это ПОВЕРХНОСТЬ обоев (`dst`), а не буфер: с viewporter
    // буфер равен кадру видео и экрану не соответствует (см.
    // wallpaper_texture_sized).
    let размер = вид.dst;
    if размер.w <= 0 || размер.h <= 0 || вид.src.size.w <= 0.0 || вид.src.size.h <= 0.0 {
        return None;
    }

    // Камеру берём ОТНОСИТЕЛЬНО ДОМА своего монитора, а не от нуля холста: дом
    // второго монитора — (1 000 000, 0) (см. `monitors::ШАГ_ДОМА`), и от нуля
    // обои второго экрана были бы сдвинуты на весь свой запас всегда.
    let дом = state.монитор_дом();
    // Слайд по столам ещё идёт — значит, следующий кадр обязан состояться, даже
    // если в сцене больше ничего не шевелится: анимацию обоев некому двигать,
    // кроме самой отрисовки.
    if state.обои_едут() {
        state.request_redraw();
    }
    let место = wallpaper_placement(
        (
            state.viewport.cam_x - дом.x as f64,
            state.viewport.cam_y - дом.y as f64,
        ),
        state.обои_фаза(),
        (размер.w, размер.h),
        экран,
    )?;

    let кадр = Rectangle::<i32, Physical>::new((0, 0).into(), (экран.0, экран.1).into());
    let id = state.wallpaper_id.get_or_insert_with(Id::new).clone();
    // Со СВОИМ снимком повреждений, а не `from_static_texture`: у статического
    // элемента счётчик коммитов не растёт никогда, damage tracker считает обои
    // неизменными и новый кадр видео на экран не попадает, пока не поедет
    // камера (см. `Parallax::wallpaper_damage`).
    let снимок = state.wallpaper_damage.snapshot();
    let el = TextureRenderElement::from_texture_with_damage(
        id,
        ctx,
        Point::<f64, Physical>::from((место.x, место.y)),
        текстура,
        1,
        Transform::Normal,
        Some(1.0),
        // Крой из viewporter: plx-wall берёт из кадра видео центральный кусок
        // нужной пропорции, и без этого обои показывали бы кадр целиком,
        // растянутым под экран.
        //
        // `TextureRenderElement` переводит этот прямоугольник в координаты
        // буфера через свои scale/transform (1 и Normal), то есть один в один.
        // У обоев так и есть — буфер без масштаба и без поворота; для
        // повёрнутого буфера пересчёт пришлось бы делать по размеру буфера, как
        // это делает сам smithay в `WaylandSurfaceRenderElement::src`.
        Some(вид.src),
        // Размер в ЛОГИЧЕСКИХ единицах: масштаб элемента 1, поэтому логические
        // единицы здесь равны физическим пикселям экрана, в которых и посчитано
        // место.
        Some(Size::<i32, Logical>::from((
            место.w.round().max(1.0) as i32,
            место.h.round().max(1.0) as i32,
        ))),
        None,
        снимок,
        Kind::Unspecified,
    );
    // Обои заведомо крупнее экрана, поэтому обрезка обязана дать элемент;
    // `None` тут значит вырожденный кадр — тогда честнее отдать отрисовку
    // прежнему пути, чем показать пустой список.
    let обрезанное = CropRenderElement::from_element(el, 1.0, кадр)?;
    Some(vec![OutputRenderElements::Wallpaper(обрезанное)])
}

/// Дать что-нибудь сделать с ГЛАВНЫМ рендерером вне пути отрисовки.
///
/// Нужно ровно там, где картинку надо получить не в кадр, а в текстуру:
/// снимок закрывающегося окна (см. close.rs). Возврат `None` означает «рендерера
/// нет» — winit-бэкенд, отладочный запуск, момент до подъёма устройства; все
/// такие места обязаны уметь обойтись без него.
///
/// `unsafe` тут ровно тот же и по той же причине, что в обработчике VBlank:
/// `state` и рендерер лежат в одной структуре, а разделить заимствование
/// нечем. Устройства на время вызова ВЫНУТЫ из `state` (`mem::take`), так что
/// добраться до того же рендерера вторым путём изнутри замыкания невозможно.
pub fn with_primary_renderer<R>(
    state: &mut Parallax,
    f: impl FnOnce(&mut Parallax, &mut GlesRenderer) -> R,
) -> Option<R> {
    let mut devices = std::mem::take(&mut state.udev_devices);
    let итог = devices.values_mut().next().map(|device| {
        let gles = &mut device.gles as *mut GlesRenderer;
        unsafe { f(state, &mut *gles) }
    });
    state.udev_devices = devices;
    итог
}

/// Гаснущие снимки закрытых окон (см. close.rs). Кладутся туда же, где стояли
/// окна, — то есть на холст, а не на экран: пока снимок гаснет, камера
/// продолжает ездить, и привязка к экрану уводила бы картинку с места.
fn build_closing_elements(state: &mut Parallax, screen: Size<i32, Physical>) -> Vec<OutputRenderElements> {
    if state.закрытия.is_empty() {
        return Vec::new();
    }
    let zoom = state.viewport.zoom.max(0.01);
    let cam = (state.viewport.cam_x, state.viewport.cam_y);
    let кадр = Rectangle::<i32, Physical>::new((0, 0).into(), screen);
    let mut out = Vec::new();
    for уход in &state.закрытия {
        let (alpha, k) = уход.alpha_scale();
        // Сжимаем к ЦЕНТРУ окна: угол как точка опоры читался бы как «окно
        // уползло», а не «погасло».
        let w = уход.rect.size.w as f64 * k;
        let h = уход.rect.size.h as f64 * k;
        let cx = уход.rect.loc.x as f64 + уход.rect.size.w as f64 / 2.0;
        let cy = уход.rect.loc.y as f64 + уход.rect.size.h as f64 / 2.0;
        let loc = Point::<f64, Physical>::from((
            (cx - w / 2.0 - cam.0) * zoom,
            (cy - h / 2.0 - cam.1) * zoom,
        ));
        let размер = Size::<i32, Logical>::from((
            (w * zoom).round().max(1.0) as i32,
            (h * zoom).round().max(1.0) as i32,
        ));
        let el = TextureRenderElement::from_static_texture(
            уход.id.clone(),
            уход.контекст.clone(),
            loc,
            уход.текстура.clone(),
            1,
            Transform::Normal,
            Some(alpha),
            None,
            Some(размер),
            None,
            Kind::Unspecified,
        );
        if let Some(обрезанное) = CropRenderElement::from_element(el, 1.0, кадр) {
            out.push(OutputRenderElements::Wallpaper(обрезанное));
        }
    }
    out
}

fn build_layer_elements(
    _state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
    layers: &[WlrLayer],
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    // Слои живут на своём выходе с масштабом 1 (см. scan_connectors).
    let output = &_state.layer_output.clone().unwrap_or_else(|| output.clone());
    let map = layer_map_for_output(output);
    // Собираем все подходящие layer-поверхности (сортируем по вложению).
    let to_render: Vec<_> = map.layers().filter(|l| layers.contains(&l.layer())).cloned().collect();
    for layer_surface in to_render {
        let Some(geo) = map.layer_geometry(&layer_surface) else { continue };
        // Геометрия слоёв ЛОГИЧЕСКАЯ, а логический размер выхода у parallax
        // делится на зум (зум сделан через output scale, см. apply_camera).
        // Раньше её клали в кадр как физическую и рисовали в масштабе 1:1 —
        // при отдалении обои сжимались в угол экрана, а меню Win+W уезжало
        // вместе с ними. Переводим в физические координаты тем же зумом и им
        // же масштабируем содержимое: слой остаётся приклеенным к экрану на
        // любом зуме — обои во весь экран, меню по центру.
        // Слои приклеены к ЭКРАНУ и зумом не масштабируются: их геометрия уже
        // в экранных пикселях (масштаб выхода всегда 1, зум живёт в отрисовке
        // окон — см. render_surface). Обёртка Rescale здесь не нужна.
        let phys_loc: Point<i32, Physical> = (geo.loc.x, geo.loc.y).into();
        els.extend(render_elements_from_surface_tree(
            renderer,
            layer_surface.wl_surface(),
            phys_loc,
            1.0,
            1.0,
            Kind::Unspecified,
        ));
    }
    els
}

/// ВРЕМЕННАЯ ДИАГНОСТИКА: выкладывает в `/tmp/plx_frame.raw` содержимое того
/// буфера, который РЕАЛЬНО уходит на монитор (`blit_frame_result` копирует сам
/// сканаут, а не перерисовывает сцену). Формат — плотный RGBA, размер экрана.
fn dump_scanout<B, F, E>(
    res: &smithay::backend::drm::compositor::RenderFrameResult<'_, B, F, E>,
    renderer: &mut GlesRenderer,
    output: &Output,
    idx: usize,
)
where
    B: smithay::backend::allocator::Buffer + smithay::backend::allocator::dmabuf::AsDmabuf,
    <B as smithay::backend::allocator::dmabuf::AsDmabuf>::Error: std::fmt::Debug,
    F: smithay::backend::drm::Framebuffer,
    E: smithay::backend::renderer::element::Element
        + smithay::backend::renderer::element::RenderElement<GlesRenderer>,
{
    use smithay::backend::renderer::{Bind, ExportMem, Offscreen, gles::GlesRenderbuffer};
    use smithay::utils::Buffer as BufferCoords;

    let Some(mode) = output.current_mode() else { return };
    let size = mode.size;
    let bsize: Size<i32, BufferCoords> = (size.w, size.h).into();

    let mut target: GlesRenderbuffer = match renderer.create_buffer(Fourcc::Abgr8888, bsize) {
        Ok(t) => t,
        Err(e) => { tracing::warn!("plx/dbg: create_buffer: {:?}", e); return }
    };
    let mut fb = match renderer.bind(&mut target) {
        Ok(fb) => fb,
        Err(e) => { tracing::warn!("plx/dbg: bind: {:?}", e); return }
    };
    if let Err(e) = res.blit_frame_result(
        size, Transform::Normal, 1.0, renderer, &mut fb,
        [Rectangle::from_size(size)], [],
    ) {
        tracing::warn!("plx/dbg: blit_frame_result: {:?}", e);
        return;
    }
    let mapping = match renderer.copy_framebuffer(&fb, Rectangle::from_size(bsize), Fourcc::Abgr8888) {
        Ok(m) => m,
        Err(e) => { tracing::warn!("plx/dbg: copy_framebuffer: {:?}", e); return }
    };
    drop(fb);
    match renderer.map_texture(&mapping) {
        Ok(data) => {
            let _ = std::fs::write(format!("/tmp/plx_frame_{:02}.raw", idx), data);
            tracing::debug!("plx/dbg: scanout dump #{} {}x{} written", idx, size.w, size.h);
        }
        Err(e) => tracing::warn!("plx/dbg: map_texture: {:?}", e),
    }
}

/// Разрешаем ли DRM раскладывать элементы по аппаратным слоям.
///
/// Здесь стоял `FrameFlags::empty()` — с самого первого коммита и, судя по
/// всему, по недосмотру: слои были запрещены ЦЕЛИКОМ. Это противоречило даже
/// собственному `Cargo.toml`, где ради слоя курсора включён `renderer_pixman`
/// с комментарием «иначе курсор подмешивается в кадр и заставляет
/// перерисовывать его на каждое движение мыши» — ровно то, что и происходило.
///
/// Что даёт `DEFAULT` (он же ALLOW_SCANOUT):
///   * **курсор — на своём слое**: движение мыши больше не пересобирает сцену,
///     а меняет позицию слоя;
///   * **полноэкранный клиент — прямо на экран**: буфер игры или плеера уходит
///     на primary/overlay в обход композиции. Ровно то, что нужно Dota и
///     видео.
///
/// Ничего не «режется»: если элемент на слой не годится (у нас это всё, что
/// прошло через RescaleRenderElement, — то есть любое окно при зуме), smithay
/// молча возвращает его в обычную композицию. Слои — это дополнительная
/// быстрая дорожка, а не замена отрисовке.
///
/// `SKIP_CURSOR_ONLY_UPDATES` не берём намеренно: он превращает кадр, в
/// котором сдвинулся только курсор, в EmptyFrame — стрелка начала бы отставать
/// от руки.
///
/// Запасной выход без пересборки: `PLX_NO_PLANES=1` возвращает прежнее
/// поведение.
fn flags_кадра() -> FrameFlags {
    static ФЛАГИ: std::sync::OnceLock<FrameFlags> = std::sync::OnceLock::new();
    *ФЛАГИ.get_or_init(|| {
        if std::env::var_os("PLX_NO_PLANES").is_some() {
            tracing::info!("plx/udev: hardware planes disabled (PLX_NO_PLANES)");
            FrameFlags::empty()
        } else {
            FrameFlags::DEFAULT
        }
    })
}

/// Пересчитывает размытую текстуру фона под плашками — ровно так, как это
/// делал `render_surface` до 24.08.2026 (логика не менялась, только вынесена,
/// чтобы тот же шаг делал headless-бэкенд, см. `собрать_элементы`).
///
/// Отказ на ОТДЕЛЬНОМ кадре текстуру НЕ сбрасывает, и это не мелочь: живые обои
/// (plx-wall крутит видео) отдают буфер не на каждый кадр композитора, и в такие
/// кадры `wallpaper_texture` возвращает None — остров панели остался бы без
/// заплаты, то есть выглядел бы иначе, а на следующем кадре заплата вернулась
/// бы. Снаружи это и есть «мигает маска скругления на баре»: моргает не маска,
/// а то, что под ней. Держим прошлую размытую картинку — она отстаёт на кадр,
/// чего под панелью не видно.
pub fn пересчитать_блюр(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    блюр: Option<&mut crate::blur::Блюр>,
    output: &Output,
) {
    match (state.lua_config.blur, блюр, output.current_mode()) {
        (true, Some(блюр), Some(mode)) => {
            // Раскладка обоев — ТА ЖЕ, что и в кадре (`build_wallpaper_backdrop`).
            // Без неё размытие сэмплировалось бы так, будто камера в нуле, —
            // см. заметку у `blur::Блюр::размыть`.
            let плитки = wallpaper_screen_place(state, renderer, output, mode.size);
            match wallpaper_texture(state, renderer, output) {
                Some(исходник) => {
                    match блюр.размыть(
                        renderer, &исходник, &плитки, mode.size, crate::blur::РАДИУС,
                    ) {
                        Some(новая) => state.blur_tex = Some(новая),
                        None => почему_нет_блюра("the convolution failed"),
                    }
                }
                // Молчать здесь нельзя: «блюра нет» выглядит снаружи одинаково
                // при выключенном блюре, несобранном шейдере и отсутствующей
                // текстуре обоев — а причина каждый раз разная (замер
                // 24.08.2026: блюр был ВКЛЮЧЁН, шейдер собран, а фона у
                // размытия не было вовсе).
                None => почему_нет_блюра("no texture on the background layer (wallpaper)"),
            }
        }
        // Блюра нет совсем (выключен, не завёлся, выход без режима) — вот
        // ЗДЕСЬ текстуру сбросить обязаны: держать её от прежнего устройства
        // после смены выхода или VT нельзя.
        (вкл, шейдер, режим) => {
            почему_нет_блюра(&format!(
                "set{{blur}}={} shader={} mode={}",
                вкл, шейдер.is_some(), режим.is_some(),
            ));
            state.blur_tex = None;
        }
    }
}

/// Пишет причину отсутствия блюра НЕ ЧАЩЕ раза в две секунды: зовётся из
/// каждого кадра, а на 200 Гц это 200 одинаковых строк в секунду.
fn почему_нет_блюра(причина: &str) {
    use std::sync::Mutex;
    static ПОСЛЕДНЕЕ: Mutex<Option<(String, std::time::Instant)>> = Mutex::new(None);
    let Ok(mut последнее) = ПОСЛЕДНЕЕ.lock() else { return };
    let свежо = последнее.as_ref().is_some_and(|(п, t)| {
        п == причина && t.elapsed().as_secs() < 2
    });
    if свежо {
        return;
    }
    *последнее = Some((причина.to_string(), std::time::Instant::now()));
    tracing::debug!("plx/blur: no blur in this frame: {}", причина);
}

/// Стрелка чужого курсора. Ширина маски — 12, строк 18.
///
/// Своя, а не курсор темы: тема отдаёт готовый растр одного цвета, а чужие
/// стрелки обязаны различаться цветом участника — иначе на холсте пять
/// одинаковых указателей, и непонятно, чей какой.
#[cfg(feature = "share")]
const ЧУЖОЙ_КУРСОР_W: i32 = 12;
#[cfg(feature = "share")]
const ЧУЖОЙ_КУРСОР: [u32; 18] = [
    0b100000000000,
    0b110000000000,
    0b111000000000,
    0b111100000000,
    0b111110000000,
    0b111111000000,
    0b111111100000,
    0b111111110000,
    0b111111111000,
    0b111111111100,
    0b111111111110,
    0b111111100000,
    0b110011110000,
    0b100001111000,
    0b000001111000,
    0b000000111100,
    0b000000111100,
    0b000000011000,
];

/// Высота чужой стрелки в ЭКРАННЫХ пикселях. Не зависит от зума — ровно как
/// свой курсор: указатель принадлежит человеку, а не холсту, и на отдалённом
/// зуме съёжившаяся в точку стрелка была бы просто не видна.
#[cfg(feature = "share")]
const ЧУЖОЙ_КУРСОР_H: i32 = 22;

/// Стрелки гостей на экране хозяина: где кто водит мышью, своим цветом и с
/// именем.
///
/// Элементы кладутся В БЛОК КУРСОРА (до `cursor_elements`), и это важно:
/// запись экрана «без курсора» и кадр, уходящий гостям, отрезают ровно этот
/// блок. Гость рисует ВСЕ стрелки у себя сам по списку участников — они
/// приходят отдельными короткими сообщениями, которые не роняются, тогда как
/// видео и роняется, и отстаёт. Вмороженный в видеокадр курсор дёргался бы
/// вместе с потоком.
// Курсоры гостей: в минимальной сборке гостей не бывает, и элементов не
// возникает вовсе. Двойником, а не `#[cfg]` по месту вызова, — чтобы кадр
// собирался в обеих сборках одним и тем же кодом (см. share_stub/).
#[cfg(not(feature = "share"))]
fn build_guest_cursors(
    _state: &mut Parallax,
    _renderer: &mut GlesRenderer,
) -> Vec<OutputRenderElements> {
    Vec::new()
}

#[cfg(feature = "share")]
fn build_guest_cursors(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    let Some(раздача) = state.раздача.as_ref() else { return els };
    let zoom = state.viewport.zoom.max(0.01);
    let (cam_x, cam_y) = (state.viewport.cam_x, state.viewport.cam_y);
    let гости: Vec<(u8, String, u32, (f64, f64))> = раздача
        .гости
        .iter()
        .filter(|гость| гость.жив && гость.впущен)
        .map(|гость| (гость.id, гость.имя.clone(), гость.цвет, гость.курсор))
        .collect();
    if гости.is_empty() {
        return els;
    }

    let w = (ЧУЖОЙ_КУРСОР_H * ЧУЖОЙ_КУРСОР_W / ЧУЖОЙ_КУРСОР.len() as i32).max(1);
    for (слот, (_id, имя, цвет, курсор)) in гости.into_iter().enumerate() {
        let x = ((курсор.0 - cam_x) * zoom).round() as i32;
        let y = ((курсор.1 - cam_y) * zoom).round() as i32;
        let цвет = крась(цвет);

        // Порядок — front-to-back: подпись и стрелка впереди, тень позади.
        // Подпись сдвинута от острия вправо-вниз, чтобы не закрывать то, на
        // что показывают.
        draw_text_w(
            state, renderer, x + w + 4, y + ЧУЖОЙ_КУРСОР_H / 2,
            &имя, bar::STRONG, bar::TEXT_SMALL, цвет, слот, &mut els,
        );
        for (сдвиг, краска) in [(0, цвет), (1, [0.0, 0.0, 0.0, 0.55])] {
            let (buf, bw, bh) = state.text_cache.bitmap_fit(
                "чужой-курсор", &ЧУЖОЙ_КУРСОР, ЧУЖОЙ_КУРСОР_W, w, ЧУЖОЙ_КУРСОР_H,
                краска, слот,
            );
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer,
                Point::<f64, Physical>::from(((x + сдвиг) as f64, (y + сдвиг) as f64)),
                buf,
                None,
                None,
                Some(Size::<i32, Logical>::from((bw, bh))),
                Kind::Unspecified,
            ) {
                Ok(el) => els.push(OutputRenderElements::Memory(el)),
                Err(e) => tracing::warn!("plx/udev: guest cursor: {:?}", e),
            }
        }
    }
    els
}

/// 0xAARRGGBB (как в `share::ЦВЕТА`) → цвет отрисовки.
fn крась(argb: u32) -> [f32; 4] {
    [
        ((argb >> 16) & 0xff) as f32 / 255.0,
        ((argb >> 8) & 0xff) as f32 / 255.0,
        (argb & 0xff) as f32 / 255.0,
        ((argb >> 24) & 0xff) as f32 / 255.0,
    ]
}

/// Собирает СПИСОК ЭЛЕМЕНТОВ кадра — всё, что видно на экране, от курсора
/// сверху до обоев снизу, в порядке front-to-back (как принимает smithay).
///
/// Вынесено из `render_surface` 24.08.2026 без единого изменения логики. Причина
/// — проверяемость: DRM-путь единственный, где живут скругления, обрезка окон и
/// блюр, а посмотреть на него можно было, только заняв монитор живого сеанса.
/// Теперь тот же кадр строит headless-бэкенд (`headless.rs`), рисует его в
/// offscreen и кладёт PNG на диск — без VT, без DRM-мастера и без чужого ввода.
///
/// `output` — выход, для которого считается кадр; `скругление` — шейдер
/// скруглённых углов (None = прямые углы, см. rounded.rs); `ёмкость` — сколько
/// элементов было в прошлом кадре (резерв под вектор, см. Surface::last_elements).
///
/// Возвращает список и число ПЕРВЫХ элементов, рисующих курсор: демонстрация
/// экрана снимает кадр с курсором и без него из одного и того же списка
/// (см. screencopy::serve_pending).
pub fn собрать_элементы(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
    скругление: Option<&crate::rounded::Шейдер>,
    ёмкость: usize,
) -> (Vec<OutputRenderElements>, usize) {
    let mut elements: Vec<OutputRenderElements> =
        Vec::with_capacity(ёмкость.max(64));

    // ── Cursor (front layer) ─────────────────────────────────────────────────
    // Только для диагностики ниже: сами ветки курсора считают свою точку
    // каждая по своему хотспоту (см. match по cursor_status).
    let cursor_pos_physical: Point<i32, Physical> = {
        let p = state.pointer_location - state.cursor_default_hotspot.to_f64();
        let output_local = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
            p.x - state.viewport.cam_x,
            p.y - state.viewport.cam_y,
        ));
        output_local.to_physical(state.viewport.zoom).to_i32_round()
    };

    // Решающий замер по жалобе «курсор тянется за паном»: печатаем то, что
    // РЕАЛЬНО уходит в кадр — экранную точку курсора и точку привязки окон
    // (output_geo.loc, от неё smithay раскладывает все окна). Если при пане
    // стрелка стоит, а привязка едет — курсор от содержимого не отстаёт и
    // причина где-то ещё. Если едут обе — курсор тащит вместе с холстом.
    // Не чаще 4 строк в секунду и только когда камера шевелится.
    {
        let cam = (state.viewport.cam_x, state.viewport.cam_y);
        let moving = (cam.0 - state.render_cam_logged.0).abs() > 0.01
            || (cam.1 - state.render_cam_logged.1).abs() > 0.01;
        if moving && state.render_cursor_logged.elapsed().as_millis() >= 250 {
            state.render_cursor_logged = std::time::Instant::now();
            let anchor = state.space.output_geometry(output).map(|g| g.loc);
            tracing::debug!(
                "plx/frame: cursor_screen=({},{}) window_anchor={:?} camera=({:.1},{:.1}) zoom={:.2}",
                cursor_pos_physical.x, cursor_pos_physical.y, anchor, cam.0, cam.1,
                state.viewport.zoom,
            );
        }
        state.render_cam_logged = cam;
    }

    // Клонируем статус: ветка Named дочитывает тему через &mut state
    // (cursor_for_icon кэширует прочитанное), а match по &state.cursor_status
    // держал бы state занятым. Клон — это Arc у WlSurface либо Copy-енум.
    //
    // Над картой окон и карточкой предпросмотра курсор — НАШ, а не клиентский.
    // Плашка лежит поверх холста, под ней всегда чьё-то окно, и оно ставит свою
    // форму: над терминалом стрелка превращалась в текстовый курсор, хотя мышь
    // в этот момент работает с картой. Пока идёт пан — «схваченная рука», как в
    // любой карте.
    let статус = if crate::mine::прячем_курсор(state) {
        // Режим Minecraft: указка — взгляд игрока, стрелку рисует мод в мире
        // (см. mine::прячем_курсор). Хозяйская поверх игры была бы второй.
        CursorImageStatus::Hidden
    } else if !state.курсор_здесь() {
        // Стрелка на другом мониторе. Без этой ветки она рисовалась бы на
        // ОБОИХ: позиция курсора считается от камеры, а камеры у мониторов
        // разные — на чужом экране получался бы второй курсор, живущий своей
        // жизнью где-то у края.
        CursorImageStatus::Hidden
    } else if state.minimap_drag || state.preview_drag {
        CursorImageStatus::Named(smithay::input::pointer::CursorIcon::Grabbing)
    } else if state.minimap_hit().is_some() || state.preview_hit().is_some() {
        CursorImageStatus::Named(smithay::input::pointer::CursorIcon::Default)
    } else {
        state.cursor_status.clone()
    };
    match статус {
        CursorImageStatus::Surface(ref cursor_surface) => {
            let hotspot = with_states(cursor_surface, |states| {
                states.data_map.get::<CursorImageSurfaceData>()
                    .map(|d| d.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            // Хотспот вычитается в ФИЗИЧЕСКИХ пикселях, уже ПОСЛЕ умножения на
            // zoom: поверхность курсора рисуется 1:1 (scale = 1.0 ниже), зум её
            // не масштабирует. Раньше вычитание шло в canvas-координатах, до
            // зума, то есть фактически вычитался hotspot*zoom — остриё уезжало
            // от точки попадания на hotspot*(zoom-1). В обзоре (zoom 0.5) клик
            // из-за этого уходил не туда, куда показывает стрелка.
            // Курсор из темы (ветка Named) — наоборот, масштабируется вместе с
            // зумом через dst, там hotspot*zoom и есть правильное смещение.
            let output_local = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                state.pointer_location.x - state.viewport.cam_x,
                state.pointer_location.y - state.viewport.cam_y,
            ));
            let p = output_local.to_physical(state.viewport.zoom);
            let pos = smithay::utils::Point::<f64, smithay::utils::Physical>::from((
                p.x - hotspot.x as f64,
                p.y - hotspot.y as f64,
            )).to_i32_round();
            // Потолок размера для картинки, которую прислал клиент.
            //
            // Клиенты, ничего специально не просившие, берут курсор ИЗ ТЕМЫ, но
            // своего размера: XWayland и GTK3 — 24, Chromium и Firefox — 32-33
            // (замер по логам сеансов 03-10.08.2026), тогда как стрелка
            // компоновщика 16. Отсюда и жалоба «курсор на окнах слишком
            // большой»: он прыгал в размере на каждой границе окна.
            //
            // Правильное место починки — не размер картинки, а протокол:
            // wp_cursor_shape_v1 (см. Parallax::new) уводит все такие «просто дай
            // мне стрелку» в ветку Named, где рисуем МЫ. Здесь остаётся
            // страховка для тех, кто протокола не знает (XWayland, GTK3):
            // ужимаем к cursor_client_max, по умолчанию равному нашему размеру.
            //
            // Из-под потолка выведено полноэкранное окно: границ окон на экране
            // в этот момент нет, прыгать курсору не о что, и размер стрелки
            // целиком дело клиента. Иначе игра, попросившая свой крупный
            // прицел (Dota 2 рисует курсоры по 32-64 px), получала стрелку в
            // 16 px — «курсор не меняет размер, когда просит игра».
            // Насовсем потолок снимает `set{ cursor_client_max = 0 }` в
            // config.lua, число побольше — поднимает.
            let масштаб = {
                let макс = if state.cursor_owned_by_fullscreen(cursor_surface) {
                    0
                } else {
                    state.cursor_client_max
                };
                let размер = crate::xwin::surface_buffer_size(cursor_surface);
                match (макс > 0, размер) {
                    (true, Some(sz)) if sz.w.max(sz.h) > макс => {
                        макс as f64 / sz.w.max(sz.h) as f64
                    }
                    _ => 1.0,
                }
            };
            // Kind::Cursor и здесь: DrmCompositor рисует элемент курсора в
            // отдельный буфер аппаратного слоя, и ужатый элемент туда только
            // легче помещается (растянутый — наоборот, слой бы его отбросил).
            // Строка на КАЖДЫЙ кадр, а под курсором клиента (браузер, Steam,
            // игра) это 190 строк в секунду синхронной записью на диск прямо из
            // потока рендера: в сеансе 05.08.2026 таких строк набежало 17 632 на
            // каждые 20 МБ лога, а лог за сеанс вырос до 775 МБ. RUST_LOG у parallax
            // штатно стоит в debug, поэтому уровня мало — включаем только по
            // PLX_DEBUG_FRAME, вместе с остальной покадровой диагностикой.
            if debug_frame_enabled() {
                let размер = crate::xwin::surface_buffer_size(cursor_surface);
                let buf_scale = smithay::wayland::compositor::with_states(
                    cursor_surface, |st| st.cached_state.get::<
                        smithay::wayland::compositor::SurfaceAttributes
                    >().current().buffer_scale,
                );
                tracing::debug!(
                    "КУРСОР КЛИЕНТА: хотспот=({},{}) буфер={:?} buffer_scale={} \
                     элемент=({},{}) остриё=({:.1},{:.1}) zoom={:.2} ужатие={:.2}",
                    hotspot.x, hotspot.y, размер, buf_scale,
                    pos.x, pos.y, p.x, p.y, state.viewport.zoom, масштаб,
                );
            }
            let cursor_els: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                render_elements_from_surface_tree(
                    renderer, cursor_surface, pos, 1.0, 1.0, Kind::Cursor,
                );
            if масштаб < 1.0 {
                // Ужимаем ОТНОСИТЕЛЬНО ОСТРИЯ (точка p), а не левого верхнего
                // угла картинки: тогда хотспот съезжает ровно на столько же, на
                // сколько ужалась картинка, и остриё остаётся там, куда
                // приходятся клики. Считать смещённый хотспот вручную не надо.
                let остриё = p.to_i32_round();
                elements.extend(cursor_els.into_iter().map(|el| {
                    OutputRenderElements::Layer(
                        RescaleRenderElement::from_element(el, остриё, масштаб),
                    )
                }));
            } else {
                elements.extend(cursor_els.into_iter().map(OutputRenderElements::Cursor));
            }
        }
        CursorImageStatus::Named(icon) => {
            // Форма пришла от клиента (wp_cursor_shape_v1) либо от нас самих —
            // в обоих случаях рисуем курсор ТЕМЫ нашего размера. Формы, которой
            // в теме нет, не бывает фатальной: показываем обычную стрелку.
            let подобранный = state.cursor_for_icon(icon)
                .map(|(buf, hs, _)| (buf.clone(), *hs));
            let (буфер, хотспот) = match подобранный {
                Some((buf, hs)) => (Some(buf), hs),
                None => (state.cursor_default_buffer.clone(), state.cursor_default_hotspot),
            };
            // Хотспот вычитаем в ФИЗИЧЕСКИХ пикселях, как и у курсора клиента:
            // картинка темы рисуется 1:1 и зумом не растягивается, поэтому
            // вычитать hotspot ДО умножения на zoom (как делает
            // cursor_pos_physical выше) значило бы вычесть hotspot*zoom. У
            // стрелки с её хотспотом в углу разницы почти нет, а вот у форм
            // вроде "text" остриё посередине — там промах был бы заметным.
            let pos = {
                let ol = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                    state.pointer_location.x - state.viewport.cam_x,
                    state.pointer_location.y - state.viewport.cam_y,
                ));
                let p = ol.to_physical(state.viewport.zoom);
                smithay::utils::Point::<f64, smithay::utils::Physical>::from((
                    p.x - хотспот.x as f64,
                    p.y - хотспот.y as f64,
                )).to_i32_round()
            };
            if let Some(buf) = буфер {
                // Размер НЕ трогаем: буфер уже нужного размера (курсор
                // ужимается при загрузке, см. load_theme_cursor). Любое
                // растяжение здесь обрезалось бы аппаратным слоем курсора.
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer, pos, &buf, None, None, None, Kind::Cursor,
                ) {
                    Ok(el) => elements.push(OutputRenderElements::Memory(el)),
                    Err(e) => tracing::warn!("plx/udev: cursor render element: {:?}", e),
                }
            }
        }
        CursorImageStatus::Hidden => {}
    }

    // Стрелки гостей мультиюзера — тем же блоком, что и свой курсор: их так же
    // отрезают и запись экрана, и кадр, уходящий гостям (см. share/render.rs).
    if state.раздача_идёт() {
        let чужие = build_guest_cursors(state, renderer);
        elements.extend(чужие);
    }

    // Всё, что добавлено выше, рисует курсор — screencopy отбрасывает ровно эти
    // элементы, когда сессия просит кадр без курсора (см. serve_pending).
    let cursor_elements = elements.len();

    // ── Overlay-слой (wlr-layer-shell): выше всего, ниже курсора ──────────────
    elements.extend(build_layer_elements(state, renderer, output, &[WlrLayer::Overlay]));

    // Тот ли это монитор, на котором стоит стрелка. Нужен всему, что живёт в
    // ОДНОМ поле на весь `Parallax` и потому не подменяется `войти_в_монитор`:
    // карточке предпросмотра и миникарте. Сверяемся с `курсор_монитор`, а не с
    // `активный` — `активный` здесь уже равен рисуемому монитору (его
    // подменяет `войти_в_монитор` чуть выше по стеку), а `курсор_монитор`
    // сборкой кадра не трогается и всегда показывает на монитор человека.
    let свой_монитор = state.монитор_по_выходу(output)
        .is_none_or(|i| i == state.курсор_монитор);

    // ── Панель рабочих столов (поверх окон, под курсором) ──────────────────
    // Под полноэкранным окном (F11, игра, видео) и в обзоре столов панель
    // уходит вверх: «на весь экран» значит на весь экран, а обзор — сам себе
    // главный. Условие теперь по ДОЛЕ ухода, а не по флагу: пока панель едет,
    // её надо рисовать, иначе уезжать нечему (ровно так же устроена миникарта
    // ниже). Куда именно она уехала, знает `bar::island_y` — одна точка и для
    // отрисовки, и для кликов.
    if state.bar_hide < 1.0 {
        let output = output.clone();
        // Карточка предпросмотра — ВЫШЕ панели: она из панели и выезжает,
        // и заезжать под неё ей незачем. Список кадра идёт от переднего плана
        // к заднему, поэтому она добавляется раньше самой панели.
        //
        // Только на мониторе с курсором, ровно по тем же граблям, что и у
        // миникарты ниже: `preview_cell`/`preview_anim` — одно поле на весь
        // `Parallax`, и без гейта карточка выезжала на ОБОИХ экранах разом.
        // Мало того, что синхронно: на чужом мониторе она рисуется его
        // подменённым видом, то есть показывает стол ЧУЖОГО экрана над
        // здешней панелью — это и читается как «столы смешиваются».
        if свой_монитор {
            elements.extend(build_bar_preview(state, renderer));
        }
        elements.extend(build_bar_elements(state, renderer, &output));
    }

    // ── Блютуз: меню поверх всего интерфейса, значок — рядом с панелью ───────
    // Меню выше панели и миникарты намеренно: пока оно открыто, оно и есть
    // главное на экране, и клавиши принадлежат ему (см. input.rs).
    //
    // Все пять — тот же случай, что и карточка выше: состояние меню одно на
    // весь `Parallax` (`bt_menu`, `wifi_open`, `audio_open`, …), а `output` им
    // нужен только ради размера экрана. Без гейта одна команда открывала меню
    // на ОБОИХ мониторах разом — замер 30.08.2026 двухмониторным харнессом:
    // `action audio_menu` при курсоре на первом мониторе рисовал список
    // выходов и на втором.
    if свой_монитор {
        elements.extend(build_bluetooth_elements(state, renderer, output));
        elements.extend(build_search_elements(state, renderer, output));
        elements.extend(build_wifi_elements(state, renderer, output));
        elements.extend(build_audio_elements(state, renderer, output));
        elements.extend(build_tray_elements(state, renderer, output));
        elements.extend(build_share_panel_elements(state, renderer, output));
    }

    // ── Миникарта (3.1, поверх окон, под курсором) ───────────────────────────
    // Не показываем во время обзора столов (перекрывает ленту).
    //
    // Условие — по доле выезда, а не по тумблеру: выключенной панели надо ещё
    // доехать до края экрана, и пока она в пути, её обязаны рисовать (см.
    // anim::tick). По тумблеру она бы просто исчезала на месте.
    //
    // `minimap_slide` — ОДНО поле на весь Parallax, а не своё у каждого монитора
    // (в отличие от viewport/layer_output, которые войти_в_монитор подменяет
    // на время сборки этого самого кадра). Без явной проверки монитора карта
    // рисовалась на КАЖДОМ выходе разом — жалоба Ярика «мини-карта и plx-wall
    // открываются на втором мониторе»: пока он работал на первом, карта той
    // же командой всплывала и на втором, который в этот момент никто не
    // смотрел. Гейт — общий `свой_монитор`, посчитанный выше (см. панель).
    if свой_монитор
        && state.minimap_slide > 0.0
        && !state.overview_active
        && !state.fullscreen_here()
    {
        let output = output.clone();
        elements.extend(build_minimap_elements(state, renderer, &output));
    }

    // ── Оконный портал (4.4): живая копия удалённого окна в фикс. точке экрана ─
    if let Some(portal) = &state.portal {
        if let Some(window) = state.tagged_windows.iter()
            .find(|tw| crate::xwin::is_surface(&tw.window, &portal.surface))
            .map(|tw| tw.window.clone())
        {
            if let Some(geo) = state.space.element_geometry(&window) {
                let scale_x = portal.box_size.w as f64 / geo.size.w.max(1) as f64;
                let scale_y = portal.box_size.h as f64 / geo.size.h.max(1) as f64;
                let scale = scale_x.min(scale_y);

                let mut idx = 0usize;
                elements.push(pooled_solid(
                    &mut state.portal_ids, &mut idx,
                    (portal.screen_pos.x, portal.screen_pos.y),
                    (portal.box_size.w, portal.box_size.h),
                    [0.0, 0.0, 0.0, 0.85],
                ));

                // Та же поправка на клиентские рамки, что и в главном цикле
                // окон: render_elements кладёт по этой точке НАЧАЛО ДЕРЕВА
                // ПОВЕРХНОСТЕЙ, а в рамку портала должна попасть ВИДИМАЯ часть
                // окна. Без вычитания geometry().loc копия окна в портале
                // съезжала вниз-вправо на поля тени клиента, и клики по ней
                // (portal_hit_test) считались по несъехавшей.
                let гло = window.geometry().loc;
                let сдвиг: Point<i32, Physical> = (
                    (гло.x as f64 * scale).round() as i32,
                    (гло.y as f64 * scale).round() as i32,
                ).into();
                let els: Vec<OutputRenderElements> = window.render_elements(
                    renderer,
                    (portal.screen_pos.x - сдвиг.x, portal.screen_pos.y - сдвиг.y).into(),
                    smithay::utils::Scale::from(scale),
                    1.0f32,
                );
                elements.extend(els);
            }
        }
    }

    // ── Скруглённые углы окон ────────────────────────────────────────────────
    // Плиток-масок здесь БОЛЬШЕ НЕТ. Они закрашивали угол цветом clear color —
    // «упрощение, приемлемое пока под углом обычно просто холст/фон», как и
    // было тут написано. Под обоями это допущение неверно, и каждый угол
    // становился тёмным квадратиком поверх картинки. Теперь угол не
    // закрашивается, а вырезается по альфе своим шейдером прямо при отрисовке
    // окна (см. rounded.rs и ветку Rounded выше).

    // ── Полоски вкладок и подсказка вставки (только Columns/niri) ────────────
    elements.extend(build_tab_indicators(state));
    elements.extend(build_insert_hint(state));

    // ── Мультивыделение (rubber-band + подсветка "созвездий") ───────────────
    elements.extend(build_ghost_elements(state));
    elements.extend(build_selection_elements(state));

    // ── Выбор источника для демонстрации экрана (портал) ─────────────────────
    // Выше окон и выделения: пока идёт выбор, он и есть главное на экране.
    elements.extend(build_portal_pick_elements(state, renderer, output));

    // ── Выделение области под снимок (PrtScr) ────────────────────────────────
    // Ещё выше: пока тянут рамку, экран затемнён целиком, и любой оверлей под
    // затемнением был бы не виден.
    elements.extend(build_snip_elements(state, renderer, output));

    // ── Top-слой (wlr-layer-shell): поверх окон, под UI -----------------------
    elements.extend(build_layer_elements(state, renderer, output, &[WlrLayer::Top]));

    // Граница групп для посекундной сводки: всё, что добавлено выше, —
    // интерфейс (курсор, полки, меню, маски углов, индикаторы).
    let счёт_интерфейс = elements.len();

    // ── Space elements (behind cursor) ───────────────────────────────────────
    // ВАЖНО: используем штатный space.render_elements_for_output (проверенный,
    // работавший путь), а не ручной per-window цикл — на живом тесте ручной
    // цикл ломал рендер содержимого окон (см. историю правок). Frustum culling
    // (4.1) и per-window fog (5.2) через этот путь недоступны (единый alpha
    // на весь батч) — оставлены как заготовки на будущее в этом комментарии,
    // но не подключены, чтобы не рисковать существующим рендером.
    // Окна рисуем САМИ, а не через space.render_elements_for_output.
    //
    // Тот путь считает всё относительно output_geometry и обрезает по нему —
    // он был пригоден, только пока зум ехал через масштаб выхода (и логический
    // размер экрана сам собой рос при отдалении). Теперь зум живёт здесь:
    // позиция окна — это (холст − камера) в экранных пикселях ДО зума, а сам
    // зум накладывается обёрткой RescaleRenderElement вокруг начала экрана.
    // Ровно так это устроено в driftwm (render/mod.rs, PixelSnapRescaleElement).
    //
    // Отсечение — по ВИДИМОЙ части холста (экран ⁄ зум): при отдалении в кадр
    // попадает больше холста, чем сам экран.
    {
        let zoom = state.viewport.zoom.max(0.01);
        let cam = Point::<f64, Logical>::from((state.viewport.cam_x, state.viewport.cam_y));
        let vis = state.visible_canvas_size();
        let видимое = Rectangle::<f64, Logical>::new(cam, vis);
        // space.elements() идёт снизу вверх, а список кадра — от переднего
        // плана к заднему, поэтому обходим в обратном порядке.
        //
        // ВАЖНО, две РАЗНЫЕ точки у одного окна:
        // · `g.loc` (element_geometry) — где стоит ВИДИМАЯ часть окна. По ней
        //   считают тайлинг, тени, скруглённые углы и хит-тест;
        // · `g.loc − window.geometry().loc` — куда класть НАЧАЛО ДЕРЕВА
        //   ПОВЕРХНОСТЕЙ. Клиент с клиентскими рамками (GTK, Firefox, Chromium,
        //   Electron) рисует вокруг окна невидимые поля под тень и рамки
        //   ресайза и объявляет их через xdg_surface.set_window_geometry —
        //   тогда `window.geometry().loc` равен (26,26) и подобному.
        //   Ровно это делает smithay в `InnerElement::render_location()`, и
        //   ровно эту точку возвращает `space.element_under`, по которой мы
        //   переводим клик в surface-локальные координаты (см. surface_under).
        //
        // Раньше сюда шёл `g.loc`, то есть дерево поверхностей клалось на
        // начало ВИДИМОЙ части: картинка уезжала на +geometry().loc, а клики
        // считались так, будто она не уехала. Клиент получал координаты на те
        // же 26 px больше — курсор показывает на ссылку, а нажимается то, что
        // ниже и правее. У ghostty и у X11-окон geometry().loc = (0,0), потому
        // баг и не воспроизводился ни в терминале, ни в firefox под Xwayland.
        let окна: Vec<(Window, Point<i32, Logical>)> = state.space.elements()
            .rev()
            .filter_map(|w| state.space.element_geometry(w).map(|g| (w.clone(), g)))
            .filter(|(_, g)| видимое.overlaps(g.to_f64()))
            .map(|(w, g)| { let loc = g.loc - w.geometry().loc; (w, loc) })
            .collect();
        let радиус_лог = corner_radius_logical(state);
        // Сколько заплат размытия под окнами уже выдано в этом кадре — по ней
        // берётся номер слота в пуле `blur_ids` (см. БЛЮР_ОКНО).
        let mut окон_с_блюром = 0usize;
        for (window, loc) in окна {
            let экран = Point::<f64, Logical>::from((loc.x as f64 - cam.x, loc.y as f64 - cam.y));
            let phys: Point<i32, Physical> = (экран.x.round() as i32, экран.y.round() as i32).into();
            // Скругляем не всё подряд: у окна во весь экран дуга у самого края
            // монитора читается как рамка вокруг «полного экрана».
            //
            // **Рамку маски считаем от `phys`, а не от `element_geometry`.**
            // Это и была «баганая мигающая маска»: элемент кадра встаёт в
            // ОКРУГЛЁННУЮ до целого точку `phys` (и уже её домножает на зум
            // RescaleRenderElement), а маска бралась из `window_screen_rect`,
            // то есть из НЕокруглённого `(холст − камера)·зум`. Камера почти
            // всегда дробная — пружина, инерция, доводка зума, — и эти двое
            // расходились на доли пикселя, каждый кадр на новые. Дуга по краю
            // окна от этого дрожит, а на движении холста ещё и не совпадает с
            // самим окном.
            //
            // Вторая половина мигания — порог «окно мельче двух радиусов не
            // скругляем вовсе». На зуме `r` растёт непрерывно, и окно у порога
            // переключалось между скруглённым и прямым от кадра к кадру.
            // Теперь порога нет: радиус просто зажимается половиной меньшей
            // стороны — маленькое окно выходит «таблеткой», а не мигает.
            let geo = window.geometry();
            // Кадр, который у окна ЗАПРОСИЛИ (xwin::set_size), против того,
            // что клиент реально нарисовал (`geo.size`). Расходятся они
            // ровно тогда, когда клиент не умеет ужиматься дальше своего
            // внутреннего минимума — Wayland это разрешает: configure для
            // xdg_toplevel просьба, а не приказ, и GTK/Electron ею честно
            // пользуются. Тайлинг и ресайз (правки 23–24.08.2026) давно
            // ужимают СЛОТ до чего угодно (пол — 1px), не хватало только
            // этого: экран показывал СТАРЫЙ, слишком большой кадр клиента —
            // жалоба Ярика 24.08.2026 ночью «сжимаются до предела и дальше
            // просто не ресайзятся». X11 сюда не попадает: там `set_size`
            // бьёт по `ConfigureWindow` немедленно (см. xwin::requested_size),
            // и geo.size СТАНЕТ запрошенным размером за один такт X-сервера.
            //
            // **X11 сюда тоже попадает** (правка 24.08.2026 вечером). Прежняя
            // здешняя заметка говорила, что X11 не нужен: `set_size` бьёт по
            // `ConfigureWindow` немедленно, и `geometry()` СТАНЕТ запрошенным
            // размером за такт X-сервера. Первое верно, второе обманчиво:
            // `X11Surface::geometry()` — это то, что мы САМИ туда записали
            // (smithay, xwm/surface.rs: `state.geometry = logical_rect` сразу
            // после configure), а вот БУФЕР клиент отдаёт своего размера и
            // менять его не обязан — приложение с min-size hints (Steam, всё
            // под wine) просто продолжает рисовать по-старому. То есть у X11
            // расхождение не видно ни в чём, кроме размера самого буфера, —
            // по нему и сравниваем. Отсюда и «лимит никуда не ушёл»: половина
            // окон Ярика — X11, и для них обрезки не включалось НИКОГДА.
            let (целевой, факт) = match window.underlying_surface() {
                WindowSurface::Wayland(_) => (crate::xwin::requested_size(&window), geo.size),
                WindowSurface::X11(s) => (
                    Some(geo.size),
                    s.wl_surface()
                        .and_then(|wl| crate::xwin::surface_buffer_size(&wl))
                        .unwrap_or(geo.size),
                ),
            };
            let нужен_кроп = !state.is_fullscreen(&window)
                && целевой.is_some_and(|t| {
                    t.w > 0 && t.h > 0 && (t.w < факт.w || t.h < факт.h)
                });
            if нужен_кроп {
                // Разово на кадр и только на debug: без этой строки «почему
                // окно всё ещё не ужимается» опять пришлось бы искать снаружи.
                tracing::debug!(
                    "plx/crop: {:?} asked for {:?}, the client draws {:?}",
                    crate::xwin::app_id(&window).unwrap_or_default(),
                    целевой.unwrap(), факт,
                );
            }

            // Считает (шейдер, прямоугольник, радиус, hard_clip) под заданный
            // размер — один расчёт на два разных кадра: обрезаемый (окно) и
            // полный (попапы, см. ниже).
            let скругление_для = |size: Size<i32, Logical>, hard: bool| {
                скругление.and_then(|ш| {
                    if state.is_fullscreen(&window) || size.w <= 0 || size.h <= 0 {
                        return None;
                    }
                    // Начало ДЕРЕВА ПОВЕРХНОСТЕЙ после зума — ровно то, что
                    // RescaleRenderElement сделает с элементом, — плюс сдвиг
                    // до видимой части (клиентские рамки CSD).
                    let x0 = (phys.x as f64 * zoom).round() + geo.loc.x as f64 * zoom;
                    let y0 = (phys.y as f64 * zoom).round() + geo.loc.y as f64 * zoom;
                    let w = size.w as f64 * zoom;
                    let h = size.h as f64 * zoom;
                    let r = (радиус_лог as f64 * zoom).min(w / 2.0).min(h / 2.0);
                    // Порог «мельче двух радиусов не скругляем» — только для
                    // обычного (не режущего) случая: жёсткая обрезка обязана
                    // сработать, даже если дуга по углам вышла бы нулевой.
                    if r < 0.5 && !hard {
                        return None;
                    }
                    Some((ш.clone(), [x0 as f32, y0 as f32, w as f32, h as f32], r.max(0.0) as f32, hard))
                })
            };
            let скруглить = скругление_для(
                if нужен_кроп { целевой.unwrap() } else { geo.size },
                нужен_кроп,
            );

            // Свечение краёв в цвет обоев. Считается один раз на окно: цвет и
            // сила у всех его поверхностей общие, как и рамка скругления.
            //
            // «В фокусе» сравниваем по КОРНЕВОЙ поверхности окна: фокус может
            // держать его попап (открытое меню), и окно от этого перестать
            // быть тем, с которым работают, не должно.
            let в_фокусе = state
                .focused_surface()
                .map(|s| crate::xwin::is_surface(&window, &s))
                .unwrap_or(false);
            let свечение = crate::rounded::Свечение::для_окна(
                state.палитра_обоев,
                state.lua_config.glow,
                state.lua_config.glow_width,
                zoom,
                в_фокусе,
            );

            // Список элементов окна. Без обрезки — как раньше, один вызов.
            // С обрезкой — РАЗДЕЛЯЕМ попапы и основное дерево поверхностей:
            // `Window::render_elements` кладёт их в ОДИН список с ОДНИМ
            // win_rect (см. smithay, space/wayland/window.rs), а попапы (меню,
            // выпадающие списки) по смыслу вылезают ЗА рамку окна — резать их
            // по кадру, который у окна ЗАПРОСИЛИ (а он ещё и МЕНЬШЕ факта),
            // значило бы вернуть баг «окна срезаются невидимыми стенами»
            // (жалоба 24.08.2026), только острее. Bool — «этот элемент из
            // основного дерева и подлежит обрезке» (для попапов — false).
            let mut window_els: Vec<(WaylandSurfaceRenderElement<GlesRenderer>, bool)> = Vec::new();
            if нужен_кроп {
                match window.underlying_surface() {
                    WindowSurface::Wayland(t) => {
                        let wl = t.wl_surface();
                        for (popup, popup_offset) in PopupManager::popups_for_surface(wl) {
                            let o = geo.loc + popup_offset - popup.geometry().loc;
                            let offset: Point<i32, Physical> = (o.x, o.y).into();
                            let pel: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                                render_elements_from_surface_tree(
                                    renderer, popup.wl_surface(), phys + offset,
                                    smithay::utils::Scale::from(1.0), 1.0f32, Kind::Unspecified,
                                );
                            window_els.extend(pel.into_iter().map(|el| (el, false)));
                        }
                        let mel: Vec<WaylandSurfaceRenderElement<GlesRenderer>> =
                            render_elements_from_surface_tree(
                                renderer, wl, phys,
                                smithay::utils::Scale::from(1.0), 1.0f32, Kind::Unspecified,
                            );
                        window_els.extend(mel.into_iter().map(|el| (el, true)));
                    }
                    WindowSurface::X11(_) => {
                        // У X11 разделять попапы не нужно и нечего: меню там —
                        // ОТДЕЛЬНЫЕ окна (override-redirect), они приходят в
                        // этот цикл сами по себе и режутся (или не режутся)
                        // каждое по своему размеру. Значит всё дерево целиком
                        // подлежит обрезке.
                        let els: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
                            renderer, phys, smithay::utils::Scale::from(1.0), 1.0f32,
                        );
                        window_els.extend(els.into_iter().map(|el| (el, true)));
                    }
                }
            } else {
                let els: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
                    renderer, phys, smithay::utils::Scale::from(1.0), 1.0f32,
                );
                window_els.extend(els.into_iter().map(|el| (el, false)));
            }
            // Маска для попапов: полный (НЕобрезанный) кадр окна, обычные
            // правила порога — считаем один раз на все попапы этого окна.
            let скруглить_попап = if нужен_кроп { скругление_для(geo.size, false) } else { None };

            elements.extend(window_els.into_iter().map(|(el, обрезать)| {
                let el = RescaleRenderElement::from_element(el, (0, 0).into(), zoom);
                let маска = if обрезать || !нужен_кроп { &скруглить } else { &скруглить_попап };
                match маска {
                    Some((ш, rect, r, hard)) => OutputRenderElements::Rounded(
                        crate::rounded::Rounded::new(el, ш, *rect, *r, *hard, свечение),
                    ),
                    None => OutputRenderElements::Layer(el),
                }
            }));

            // ── Размытый фон ПОД окном (блюр фона терминала) ─────────────────
            // Кладётся сразу за элементами САМОГО окна, то есть перекрывает
            // только то, что лежит ниже него: обои и окна под ним. Порядок в
            // списке идёт от переднего плана к заднему, поэтому «сразу после» —
            // это и значит «сразу под».
            //
            // Прямоугольник и радиус — ТЕ ЖЕ, что у маски скругления окна
            // (`скругление_для`), иначе размытие вылезло бы из-под скруглённых
            // углов светлой каймой.
            //
            // В обзоре не рисуем: там окна ужаты в миниатюры, заплата под
            // каждой ничего не добавляет, а стоит по текстуре на окно.
            if state.lua_config.blur
                && state.blur_tex.is_some()
                && !state.overview_active
                && crate::xwin::has_transparency(&window)
            {
                let размер = if нужен_кроп { целевой.unwrap() } else { geo.size };
                let x0 = (phys.x as f64 * zoom).round() + geo.loc.x as f64 * zoom;
                let y0 = (phys.y as f64 * zoom).round() + geo.loc.y as f64 * zoom;
                let w = размер.w as f64 * zoom;
                let h = размер.h as f64 * zoom;
                if w >= 1.0 && h >= 1.0 {
                    let r = if state.is_fullscreen(&window) {
                        0.0
                    } else {
                        (радиус_лог as f64 * zoom).min(w / 2.0).min(h / 2.0).max(0.0)
                    };
                    let слот = БЛЮР_ОКНО + окон_с_блюром;
                    окон_с_блюром += 1;
                    if let Some(el) = build_blur_patch(
                        state, renderer,
                        bar::Rect {
                            x: x0.round() as i32, y: y0.round() as i32,
                            w: w.round() as i32, h: h.round() as i32,
                        },
                        r as f32, 1.0, слот,
                    ) {
                        elements.push(el);
                    }
                }
            }
        }
    }

    // ── Гаснущие снимки закрытых окон ────────────────────────────────────────
    // Сразу ЗА живыми окнами: закрытое окно уже не может быть поверх открытого,
    // а под ним ему самое место — оно как раз перестаёт быть.
    {
        let экран = output.current_mode()
            .map(|m| m.size)
            .unwrap_or_else(|| {
                let s = state.screen_size();
                Size::<i32, Physical>::from((s.w, s.h))
            });
        elements.extend(build_closing_elements(state, экран));
    }

    // Граница групп: всё, что добавлено между этой строкой и предыдущей, — окна.
    let счёт_окна = elements.len() - счёт_интерфейс;

    // ── Bottom-слой (wlr-layer-shell): под окнами, над фоном ──────────────────
    elements.extend(build_layer_elements(state, renderer, output, &[WlrLayer::Bottom]));

    // ── Тени окон (полупрозрачные, скруглённые), сразу ПОЗАДИ окон ──────────
    // В обзоре тоже рисуем: раньше их там отключали из-за цены (225 элементов
    // на окно), но после перехода на плитки это 11 элементов, и в обзоре тень
    // как раз нужна — она отделяет окна от фона стола.
    elements.extend(build_shadow_elements(state, renderer));

    // Focus Aura (голубое свечение) УБРАНА — её принимали за "голубую тень".
    // Глубину теперь даёт нейтральная мягкая build_shadow_elements выше.

    // ── Фон рабочих столов в обзоре (только при тапе Super), позади окон ────
    elements.extend(build_overview_bg_elements(state));

    // Граница групп: bottom-слой, тени и фоны обзора — декор.
    let счёт_декор = elements.len() - счёт_интерфейс - счёт_окна;

    // ── Background-слой (wlr-layer-shell) и за ним параллакс ───────────────────
    //
    // Порядок важен: список идёт ОТ ПЕРЕДНЕГО ПЛАНА К ЗАДНЕМУ, и раньше
    // параллакс добавлялся ПЕРЕД фоновым слоем — то есть его точки лежали
    // ПОВЕРХ обоев и просвечивали сквозь любую картинку. Теперь обои идут
    // первыми, а сетка точек — за ними: без обоев она видна как прежде, с
    // обоями честно скрыта под ними.
    // Обои: сперва пробуем положить их НА ХОЛСТ плитками (бесконечные обои,
    // едут с камерой). Не вышло — рисуем фоновый слой как раньше, приклеенным
    // к экрану. Отказ обязан быть мягким: без обоев экран чёрный.
    let плитками = match build_wallpaper_backdrop(state, renderer, output) {
        Some(плитки) => {
            elements.extend(плитки);
            true
        }
        None => {
            elements.extend(
                build_layer_elements(state, renderer, output, &[WlrLayer::Background]),
            );
            false
        }
    };
    if let Some(mode) = output.current_mode() {
        // Под обоями во весь экран сетку не строим — её всё равно не видно.
        // Плитки кроют холст по определению, а вот `background_covers_output`
        // спрашивает СВОЮ карту слоёв: у монитора, которому plx-wall поверхность не
        // вешал, она пуста, и сетка строилась бы под чужими обоями впустую.
        if !плитками && !background_covers_output(state, output, state.screen_size()) {
            elements.extend(build_parallax_elements(state, renderer, mode));
        }
    }

    // ── Пелена входа: поверх ВСЕГО, включая курсор ──────────────────────────
    //
    // Вставка в начало, а не push: список идёт от переднего плана к заднему.
    // Курсор тоже под ней намеренно — в первые полсекунды сеанса стрелка,
    // висящая над чернотой, выдаёт, что «темнота» это плашка, а не ещё не
    // нарисованный экран.
    if let Some(альфа) = state.вход.as_ref().map(|в| в.пелена()).filter(|a| *a > 0.001) {
        // Размер — режим ВЫХОДА (физические пиксели этого экрана), как у всех
        // прочих полноэкранных плашек кадра: логический размер с зумом здесь
        // не при чём, пелена приклеена к стеклу, а не к холсту.
        let (ширина, высота) = match output.current_mode() {
            Some(m) => (m.size.w, m.size.h),
            None => (0, 0),
        };
        let mut idx = 0;
        let плашка = pooled_solid(
            &mut state.вход_ids, &mut idx,
            (0, 0), (ширина, высота),
            // Тот же цвет, которым чистится кадр: пелена обязана быть
            // НЕОТЛИЧИМОЙ от пустого экрана, иначе в начале входа видна её
            // граница на фоне незакрытых ею краёв.
            [CLEAR_COLOR[0], CLEAR_COLOR[1], CLEAR_COLOR[2], альфа],
        );
        elements.insert(0, плашка);
    }

    let element_count = elements.len();
    state.render_stats.record_breakdown(
        счёт_интерфейс,
        счёт_окна,
        счёт_декор,
        element_count - счёт_интерфейс - счёт_окна - счёт_декор,
    );

    (elements, cursor_elements)
}

pub fn render_surface(surface: &mut Surface, renderer: &mut GlesRenderer, state: &mut Parallax) {
    // Кадр собирается ТОЧКОЙ ЗРЕНИЯ СВОЕГО монитора: своя камера, свой зум,
    // свой рабочий стол, своя карта слоёв (см. monitors::войти_в_монитор).
    // Без этого второй монитор рисовал бы вид первого — то самое «второй
    // монитор зеркалит», с которого начался разбор.
    // Курсор сводим с камерой ЗДЕСЬ — в самой нижней точке, через которую
    // проходят ВСЕ пути отрисовки. Раньше вызов стоял в render_all, но
    // VBlank-хендлер (см. init_udev) зовёт anim::tick и render_surface напрямую,
    // мимо него — то есть каждый кадр анимации (зум, обзор, перелёт, инерция)
    // рисовался с курсором от предыдущего положения камеры. Стрелку тащило
    // вместе с холстом, а главный цикл возвращал её назад уже после показа:
    // в логе это «СИНХ КУРСОР ... снос=(-30.0,-38.0)» — 30-38 px за один
    // отрисованный кадр.
    //
    // Строго ДО перехода на точку зрения рисуемого монитора: курсор живёт на
    // АКТИВНОМ мониторе, и сводить его с чужой камерой значило бы телепортом
    // утаскивать стрелку на соседний экран на каждом кадре.
    state.sync_pointer_to_camera();

    // Режим Minecraft: игра обязана быть верхней в стопке ИМЕННО СЕЙЧАС, до
    // сборки кадра. Тот же вызов стоит в `mine::тик_с`, но такт режима идёт в
    // конце итерации главного цикла — ПОСЛЕ `render_all`. Порядок за одну
    // итерацию был такой: окно смапилось поверх игры → монитор нарисован с ним
    // сверху → и только потом игра вернулась наверх. Один показанный кадр
    // рабочего стола — это и есть «между открытием окон просвечивают окна
    // поверх игры» (жалоба 01.09.2026). Здесь же ловятся и все прочие
    // поднимающие пути (клик, Alt+Tab, меню X11), включая VBlank-хендлер,
    // который в `render_all` не заходит вовсе. Пока режим выключен — одна
    // проверка `Option`.
    crate::mine::игру_наверх(state);

    let свой = state.монитор_по_выходу(&surface.output);
    let вернуть = свой.and_then(|i| state.войти_в_монитор(i));
    рисовать_поверхность(surface, renderer, state);
    state.покинуть_монитор(вернуть);
}

fn рисовать_поверхность(surface: &mut Surface, renderer: &mut GlesRenderer, state: &mut Parallax) {
    // Не рисуем чаще, чем экран показывает: пока предыдущий кадр ждёт VBlank,
    // рисовать второй бессмысленно — queue_frame его же и затрёт (см. поле
    // Surface::frame_queued). Запрос на перерисовку не теряем: возвращаем
    // needs_redraw, и кадр будет отрисован либо ближайшим VBlank (он всё равно
    // зовёт render_surface), либо следующим проходом главного цикла.
    if surface.frame_queued {
        if surface.frame_queued_at.elapsed().as_millis() < FRAME_QUEUE_STALE_MS {
            state.needs_redraw = true;
            state.render_stats.record_skip();
            return;
        }
        // Страховка: VBlank не пришёл слишком долго — считаем цепочку порванной
        // и рисуем, иначе экран замёрзнет навсегда.
        tracing::debug!("plx/udev: frame_queued stuck for {:?}, forcing a redraw",
            surface.frame_queued_at.elapsed());
        surface.frame_queued = false;
    }

    // Прошлый кадр не выделился (нет видеопамяти) — держим паузу, см.
    // ОТКАЗ_ПАУЗА_МС. Запрос не теряем: needs_redraw поднимает частоту тика до
    // 16 мс (anim_busy смотрит на него), и попытка повторится сама.
    if let Some(до) = surface.отказ_до {
        if std::time::Instant::now() < до {
            state.needs_redraw = true;
            state.render_stats.record_skip();
            return;
        }
        surface.отказ_до = None;
    }

    // ── Размытый фон под плашками ────────────────────────────────────────────
    // Считается ОДИН раз на кадр и до сборки элементов: под островами панели,
    // меню и миникартой лежит одна и та же картинка, и размывать её по разу на
    // плашку было бы чистым перерасходом. Готовая текстура живёт в state, её
    // берут все, кому надо. Любой отказ (шейдер не собрался, обоев ещё нет,
    // буфер не завёлся) даёт None — и плашки просто рисуются как раньше.
    //
    // Отказ на ОТДЕЛЬНОМ кадре текстуру НЕ сбрасывает, и это не мелочь.
    // Раньше здесь стояло `blur_tex = None` перед попыткой: живые обои
    // (plx-wall крутит видео) отдают буфер не на каждый кадр композитора, и в
    // такие кадры `wallpaper_texture` возвращала None — остров панели
    // оставался без заплаты, то есть выглядел иначе, а на следующем кадре
    // заплата возвращалась. Снаружи это и есть «мигает маска скругления на
    // баре»: моргает не маска, а то, что под ней. Держим прошлую размытую
    // картинку — она отстаёт на кадр, чего под панелью не видно.
    //
    // Стоит СТРОГО ПОСЛЕ гейтов отрисовки, и это не косметика (замер
    // 03.09.2026). Раньше вызов был первой строкой функции — то есть свёртка
    // (три офскрин-буфера, сборка сцены обоев плюс два прохода) считалась и на
    // тех заходах, которые тут же выходили по `frame_queued`. При живой Dota
    // это 500-900 пропусков в секунду против ~190 показанных кадров: до 4/5
    // всей работы размытия уходило в кадр, который никто не собирал. Сводка
    // это скрывала — таймер начинается ниже, и блюр в `средний ... мс` не
    // попадал (0.3 мс на кадр в логе против 33% ядра у процесса).
    let blur_started = std::time::Instant::now();
    пересчитать_блюр(state, renderer, surface.blur.as_mut(), &surface.output);
    state.render_stats.record_blur(blur_started.elapsed().as_micros() as u64);

    let render_started = std::time::Instant::now();
    // Где курсор был в ЭТОМ кадре — то, что пользователь реально увидит на
    // мониторе. Клик сравнивается с этим значением (см. PTR HIT): если человек
    // жалуется на промах, а расхождение здесь ненулевое, значит мажет не
    // хит-тест, а устаревшая картинка — кадр нарисован до того, как курсор
    // доехал.
    state.frame_cursor = state.pointer_location;
    state.frame_drawn_at = std::time::Instant::now();

    // Сбрасываем plane-кэш только когда окна реально поменялись (тайлинг/
    // создание/уничтожение/переключение тега) — несколько кадров подряд
    // (см. request_plane_reset), не постоянно: полный редрав каждый кадр
    // убивает производительность.
    if state.plane_reset_frames > 0 {
        surface.compositor.reset_buffer_ages();
        state.plane_reset_frames -= 1;
    }

    // Smithay принимает элементы в порядке front-to-back (первый = ближе к зрителю).
    // Курсор должен быть ПЕРВЫМ чтобы рендериться поверх окон.
    //
    // Ёмкость берём по прошлому кадру: список набирается двумя десятками
    // `extend` и на 300 элементах успевал переехать в новую память с десяток
    // раз за кадр, то есть 190 раз в секунду.
    let (elements, cursor_elements) = собрать_элементы(
        state,
        renderer,
        &surface.output,
        surface.rounded.as_ref(),
        surface.last_elements,
    );
    let element_count = elements.len();
    surface.last_elements = element_count;
    let output_name = surface.output.name();
    match surface.compositor.render_frame(
        renderer, &elements, CLEAR_COLOR, flags_кадра()
    ) {
        Ok(res) => {
            // Кадр выделился — память вернулась, откат снимаем.
            if surface.отказов_подряд > 0 {
                tracing::info!("plx/udev: render_frame[{}]: drawing again after {} failures",
                    output_name, surface.отказов_подряд);
                surface.отказов_подряд = 0;
                surface.отказ_лог = None;
            }
            // trace!, а не debug!: это две строки на КАЖДЫЙ кадр (при 60 Гц —
            // ~50 КБ/с), а лог из launch_tty.zsh идёт через tee синхронной
            // записью на диск прямо из потока рендера, который у parallax один.
            tracing::trace!("plx/udev: render_frame[{}]: is_empty={}", output_name, res.is_empty);

            // ── ВРЕМЕННАЯ ДИАГНОСТИКА (см. debug_frame_enabled) ──────────────
            if debug_frame_enabled() {
                if let Ok((damage, _states)) =
                    res.damage_from_age(&mut surface.damage_tracker, 1, [])
                {
                    match damage {
                        Some(rects) => {
                            let area: i64 = rects.iter()
                                .map(|r| r.size.w as i64 * r.size.h as i64).sum();
                            tracing::debug!(
                                "plx/dbg: damage[{}]: n={} area={} needs_sync={} {:?}",
                                output_name, rects.len(), area, res.needs_sync(),
                                rects.iter().take(6).collect::<Vec<_>>(),
                            );
                        }
                        None => tracing::debug!("plx/dbg: damage[{}]: empty", output_name),
                    }
                }
                // Флаг взводит ПАЧКУ снимков подряд: одиночный кадр не поймает
                // мерцание, а артефакт может жить один кадр из десяти.
                static BURST: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                use std::sync::atomic::Ordering;
                if std::path::Path::new("/tmp/plx_dump").exists() {
                    let _ = std::fs::remove_file("/tmp/plx_dump");
                    BURST.store(16, Ordering::Relaxed);
                }
                let n = BURST.load(Ordering::Relaxed);
                if n > 0 {
                    BURST.store(n - 1, Ordering::Relaxed);
                    dump_scanout(&res, renderer, &surface.output, 16 - n);
                }
            }
            // ── Дождаться GPU, прежде чем ставить кадр на показ ──────────────
            //
            // `needs_sync()` значит, что smithay НЕ может отдать драйверу fence
            // отрисовки вместе с флипом (либо плоскость не умеет IN_FENCE_FD,
            // либо EGL-fence не экспортируется), и по контракту ждать обязан
            // сам компоситор — ровно это делает `DrmOutput::render_frame` в
            // smithay. В parallax этого шага не было: `queue_frame` ставил буфер на
            // page flip, пока GPU его ещё дорисовывал, и на монитор уходил
            // НЕДОРИСОВАННЫЙ кадр — прямоугольные чёрные блоки размером с тайл
            // растеризатора.
            //
            // Почему это било именно по анимированным обоям на неподвижной
            // камере: каждый кадр обоев повреждает экран целиком, то есть
            // рисуется всё поле 2560×1080, а видео идёт 15 fps — недорисованный
            // кадр висит на экране до 66 мс и прекрасно виден. При движении
            // камеры кадры идут по 60 Гц, и тот же артефакт живёт 16 мс, теряясь
            // в движении. Замер 04.08.2026: needs_sync=true на 616 кадрах из 616.
            if res.needs_sync() {
                if let smithay::backend::drm::compositor::PrimaryPlaneElement::Swapchain(ref el) =
                    res.primary_element
                {
                    let ждём = std::time::Instant::now();
                    let _ = el.sync.wait();
                    state.render_stats.record_sync(ждём.elapsed().as_micros() as u64);
                }
            }

            match surface.compositor.queue_frame(()) {
                Ok(()) => {
                    // Кадр ушёл на показ — до его VBlank новые рендеры не нужны.
                    surface.frame_queued = true;
                    surface.frame_queued_at = std::time::Instant::now();
                    tracing::trace!("plx/udev: queue_frame[{}]: committed", output_name);
                }
                // EmptyFrame — на экране ничего не изменилось, VBlank НЕ придёт;
                // шлагбаум обязан остаться открытым, иначе следующее изменение
                // упрётся в него и будет ждать страховочные 100 мс.
                Err(FrameError::EmptyFrame) => tracing::trace!("plx/udev: queue_frame[{}]: EmptyFrame", output_name),
                Err(e) => tracing::warn!("plx/udev: queue_frame[{}]: {:?}", output_name, e),
            }
        }
        Err(e) => {
            surface.отказов_подряд = surface.отказов_подряд.saturating_add(1);
            let пауза = ОТКАЗ_ПАУЗА_МС
                .saturating_mul(1u64 << surface.отказов_подряд.min(6))
                .min(ОТКАЗ_ПАУЗА_МАКС_МС);
            surface.отказ_до =
                Some(std::time::Instant::now() + Duration::from_millis(пауза));
            // Первый отказ печатаем сразу — он и есть точка входа в разбор;
            // дальше не чаще раза в секунду, с накопленным счётчиком.
            let пора = surface.отказ_лог
                .is_none_or(|t| t.elapsed().as_millis() >= ОТКАЗ_ЛОГ_МС);
            if пора {
                surface.отказ_лог = Some(std::time::Instant::now());
                tracing::warn!(
                    "plx/udev: render_frame[{}]: {:?} ({} failures in a row, backoff {} ms)",
                    output_name, e, surface.отказов_подряд, пауза,
                );
            }
            state.render_stats.record(render_started.elapsed().as_micros() as u64, element_count);
            // Дальше по функции идут захват экрана и РАССЫЛКА FRAME CALLBACK.
            // Ни то, ни другое делать нельзя: кадра нет, а callback'и вернутся
            // коммитами клиентов и закрутят тот самый цикл (см. ОТКАЗ_ПАУЗА_МС).
            // Запрос на кадр держим, чтобы попытка повторилась после паузы.
            state.needs_redraw = true;
            return;
        }
    }

    state.render_stats.record(render_started.elapsed().as_micros() as u64, element_count);

    // ── Захват экрана ────────────────────────────────────────────────────────
    // Строго ПОСЛЕ render_frame и теми же элементами: демонстрация экрана
    // обязана показывать ровно то, что ушло на монитор. См. screencopy.rs.
    crate::screencopy::serve_pending(
        state, &surface.output.clone(), renderer, &elements, cursor_elements,
    );

    // ── Снимок области (PrtScr) ──────────────────────────────────────────────
    // Тем же кадром и там же, где screencopy: сцена собрана, renderer свободен.
    // Курсор отрезаем (`elements[cursor_elements..]`) — в снимке его быть не
    // должно, как и в Windows. Затемнение с рамкой сюда уже не попадает: к
    // моменту отпускания кнопки `snip` снят, и кадр рисуется чистым.
    if state.snip_ждёт.is_some() {
        crate::snip::serve(
            state, &surface.output.clone(), renderer, &elements[cursor_elements..],
        );
    }

    // ── Демонстрация экрана: кадр в PipeWire ─────────────────────────────────
    // Тем же снимком, что и screencopy, и строго после кадра на монитор.
    // Частоту держит сам Cast (30 fps): гнать 60 кадров по 11 МБ незачем —
    // Discord всё равно перекодирует поток.
    if state.portal_cast.as_ref().is_some_and(|c| c.due()) {
        push_cast_frame(state, &surface.output.clone(), renderer, &elements);
    }

    // ── Мультиюзер: кадр каждому гостю его собственной камерой ───────────────
    // Здесь же, после кадра на монитор, и по той же причине: сцена собрана,
    // текстуры импортированы, renderer свободен. Своих элементов гостям не
    // хватает (у каждого своя точка зрения), поэтому сцена пересобирается —
    // см. share/render.rs.
    if state.раздача_идёт() {
        crate::share::render::кадры_гостям(
            state, renderer, &surface.output.clone(), surface.rounded.as_ref(),
        );
    }

    // Eco-mode (4.2): окна дальше 2 экранов от текущего viewport не получают
    // frame callback — клиент (браузер/плеер) перестаёт рендерить кадры и
    // снижает нагрузку на CPU/GPU, пока не окажется снова рядом с камерой.
    let eco_rect = {
        let out_geo = state.space.output_geometry(&surface.output).unwrap_or_default();
        let cam_i32 = smithay::utils::Point::from((
            state.viewport.cam_x.round() as i32,
            state.viewport.cam_y.round() as i32,
        ));
        let mut r = crate::canvas::visible_canvas_rect(cam_i32, out_geo.size, state.viewport.zoom);
        let buf_w = out_geo.size.w.max(1) * 2;
        let buf_h = out_geo.size.h.max(1) * 2;
        r.loc.x -= buf_w;
        r.loc.y -= buf_h;
        r.size.w += buf_w * 2;
        r.size.h += buf_h * 2;
        r
    };

    let elapsed = state.start_time.elapsed();
    let space = &state.space;
    space.elements().for_each(|window| {
        let awake = space.element_geometry(window)
            .map(|g| eco_rect.overlaps(g))
            .unwrap_or(true);
        if awake {
            window.send_frame(
                &surface.output, elapsed,
                Some(Duration::from_millis(16)),
                |_, _| Some(surface.output.clone()),
            );
        }
    });
    // Frame callbacks для layer-поверхностей
    {
        // Обои под полноэкранным окном не видно вовсе — и будить их незачем:
        // plx-wall тактуется кадровыми callback'ами, без них он засыпает, а за ним
        // встаёт и ffmpeg (см. Parallax::wallpaper_hidden). Запрос на callback при
        // этом никуда не девается: он ждёт в поверхности и уедет клиенту первым
        // же кадром, когда обои снова покажутся.
        let фон_скрыт = state.wallpaper_hidden();
        let layer_out = state.layer_output.clone().unwrap_or_else(|| surface.output.clone());
        let map = layer_map_for_output(&layer_out);
        for layer_surface in map.layers() {
            let фон = matches!(
                layer_surface.layer(),
                smithay::wayland::shell::wlr_layer::Layer::Background
                    | smithay::wayland::shell::wlr_layer::Layer::Bottom
            );
            if фон_скрыт && фон {
                continue;
            }
            layer_surface.send_frame(
                &surface.output, elapsed,
                Some(Duration::from_millis(16)),
                |_, _| Some(surface.output.clone()),
            );
        }
    }
}

/// Рендерит все CRTC на всех устройствах прямо сейчас.
///
/// Основной цикл рендера — реактивный: кадр перерисовывается только в ответ
/// на DrmEvent::VBlank (см. обработчик выше). Если render_frame не находит
/// изменений, queue_frame ничего не коммитит (FrameError::EmptyFrame) — и
/// следующий VBlank никогда не приходит, цепочка обрывается насовсем.
/// Поэтому любой источник новых изменений на экране (commit клиента, новое
/// окно, движение курсора) обязан сам дёрнуть рендер через эту функцию —
/// иначе изменение останется в state, но никогда не попадёт на экран.
pub fn render_all(state: &mut Parallax) {
    // Пока сессия не активна (VT-переключение, DRM master у другого
    // compositor'а), PrepareFrame гарантированно вернёт DrmError(DeviceInactive) —
    // не тратим кадры и не спамим лог, просто ждём ActivateSession.
    if !state.session_active {
        return;
    }
    // Синхронизация курсора живёт в render_surface — она общая для всех путей
    // отрисовки, включая VBlank-хендлер, который сюда не заходит.
    let mut devices = std::mem::take(&mut state.udev_devices);
    for device in devices.values_mut() {
        let crtcs: Vec<_> = device.surfaces.keys().cloned().collect();
        for crtc in crtcs {
            if let Some(surface) = device.surfaces.get_mut(&crtc) {
                let gles = &mut device.gles as *mut GlesRenderer;
                unsafe { render_surface(surface, &mut *gles, state); }
            }
        }
    }
    state.udev_devices = devices;
}

// ── Блютуз: меню устройств и индикатор ───────────────────────────────────────

/// Руна блютуза 11×16: вертикаль с двумя треугольниками, как на устройствах.
const BT_RUNE: [u32; 16] = [
    0b00001100000,
    0b00001110000,
    0b00001101100,
    0b00001100110,
    0b00001100011,
    0b01100110011,
    0b00110110110,
    0b00011111100,
    0b00011111100,
    0b00110110110,
    0b01100110011,
    0b00001100011,
    0b00001100110,
    0b00001101100,
    0b00001110000,
    0b00001100000,
];


// ── Маски значков полки ──────────────────────────────────────────────────────
// Рисуются математикой (дуги, окружности) генератором из истории задачи и
// вшиты сюда таблицей: по строке на пиксель, старший бит из ширины — левый
// столбец. На экран кладутся через text.rs::bitmap_fit, который сжимает маску
// до нужного размера со сглаживанием, — поэтому сетка нарочно крупнее, чем
// значок на экране: у сжатия есть из чего усреднять.
/// Ширина сетки вайфая: точка и три дуги лежат в ОДНОЙ сетке, чтобы гореть
/// по отдельности, но складываться в один значок.
const WIFI_W: i32 = 25;
const VOLUME_W: i32 = 24;
const BATTERY_W: i32 = 26;
/// Внутреннее окно значка батареи (x, y, ширина, высота) в сетке BATTERY —
/// по нему рисуется заливка заряда.
const BATTERY_INNER: (i32, i32, i32, i32) = (2, 3, 19, 8);
/// Кнопки питания, сна и перезагрузки живут в одной квадратной сетке.
const POWER_W: i32 = 20;

const WIFI_DOT: [u32; 17] = [
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000011100000000000,
        0b0000000000111110000000000,
        0b0000000000111110000000000,
        0b0000000000011100000000000,
    ];

const WIFI_ARC1: [u32; 17] = [
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000111110000000000,
        0b0000000011111111100000000,
        0b0000000011111111100000000,
        0b0000000001000001000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
    ];

const WIFI_ARC2: [u32; 17] = [
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000111110000000000,
        0b0000000111111111110000000,
        0b0000011111111111111100000,
        0b0000011110000000111100000,
        0b0000001000000000001000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
    ];

const WIFI_ARC3: [u32; 17] = [
        0b0000001111111111111000000,
        0b0000111111111111111110000,
        0b0011111100000000011111100,
        0b0011110000000000000111100,
        0b0011000000000000000001100,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
        0b0000000000000000000000000,
    ];

const VOLUME: [u32; 18] = [
        0b000000000000000000000000,
        0b000000000010000000000000,
        0b000000000110000001000000,
        0b000000001110000011100000,
        0b000000011110000001100000,
        0b000000111110001101110000,
        0b111111111110011100110000,
        0b111111111110001110110000,
        0b111111111110001110111000,
        0b111111111110001110111000,
        0b111111111110001110110000,
        0b111111111110011100110000,
        0b000000111110001101110000,
        0b000000011110000001100000,
        0b000000001110000011100000,
        0b000000000110000001000000,
        0b000000000010000000000000,
        0b000000000000000000000000,
    ];

const VOLUME_MUTED: [u32; 18] = [
        0b000000000000000000000000,
        0b000000000010000000000000,
        0b000000000110000000000000,
        0b000000001110000000000000,
        0b000000011110001100001100,
        0b000000111110001110011100,
        0b111111111110001110011100,
        0b111111111110000111111000,
        0b111111111110000011110000,
        0b111111111110000011110000,
        0b111111111110000111111000,
        0b111111111110001110011100,
        0b000000111110001110011100,
        0b000000011110001100001100,
        0b000000001110000000000000,
        0b000000000110000000000000,
        0b000000000010000000000000,
        0b000000000000000000000000,
    ];

const BATTERY: [u32; 14] = [
        0b00000000000000000000000000,
        0b11111111111111111111111000,
        0b11111111111111111111111000,
        0b11000000000000000000011000,
        0b11000000000000000000011000,
        0b11000000000000000000011111,
        0b11000000000000000000011111,
        0b11000000000000000000011111,
        0b11000000000000000000011111,
        0b11000000000000000000011000,
        0b11000000000000000000011000,
        0b11111111111111111111111000,
        0b11111111111111111111111000,
        0b00000000000000000000000000,
    ];

const POWER: [u32; 20] = [
        0b00000000011000000000,
        0b00000000011000000000,
        0b00000000011000000000,
        0b00000011111111000000,
        0b00000111111111100000,
        0b00011111011011111000,
        0b00011100011000111000,
        0b00111000011000011100,
        0b00110000011000001100,
        0b01110000011000001110,
        0b01110000011000001110,
        0b01110000011000001110,
        0b01110000000000001110,
        0b01110000000000001110,
        0b00110000000000001100,
        0b00111000000000011100,
        0b00011100000000111000,
        0b00011110000001111000,
        0b00000100000000100000,
        0b00000000000000000000,
    ];

const REBOOT: [u32; 20] = [
        0b00000000000000000000,
        0b00000000001100000000,
        0b00000001111111000000,
        0b00000111111111110000,
        0b00001111111111111100,
        0b00011110001111110000,
        0b00111100001111000000,
        0b00111000001100000000,
        0b01110000000000001110,
        0b01110000000000001110,
        0b01110000000000001110,
        0b01110000000000001110,
        0b01110000000000001110,
        0b01110000000000001110,
        0b00111000000000011100,
        0b00111100000000111100,
        0b00011110000001111000,
        0b00001111111111110000,
        0b00000111111111100000,
        0b00000001111110000000,
    ];

const SLEEP: [u32; 20] = [
        0b00000000000000000000,
        0b00000000000000000000,
        0b00000100000000000000,
        0b00001100000000000000,
        0b00011100000000000000,
        0b00111100000000000000,
        0b01111100000000000000,
        0b01111100000000000000,
        0b01111100000000000000,
        0b01111110000000000000,
        0b01111111000000000000,
        0b01111111000000000000,
        0b01111111100000000000,
        0b01111111111000000000,
        0b00111111111111000000,
        0b00011111111111110000,
        0b00001111111111100000,
        0b00000111111111000000,
        0b00000001111100000000,
        0b00000000000000000000,
    ];

/// Масштаб шрифта в меню: «пиксель» глифа стороной в 2 экранных.
const BT_TEXT: i32 = 2;
/// Высота строки устройства.
const BT_ROW_H: i32 = 34;
/// Ширина панели меню.
///
/// Было 760 — и подвал в неё не влезал: строка подсказки
/// «Enter connect  D disconnect  F forget  S scan  P power  Esc» это 58
/// символов, а при тогдашнем моноширинном шрифте 7×13 и BT_TEXT=2 это 812 px,
/// то есть шире всей панели. Хвост уезжал за край и обрезался. Теперь подвал
/// собран из кнопок с переносом по строкам (см. build_bluetooth_elements), а
/// панель заодно стала шире, чтобы перенос случался пореже.
const BT_MENU_W: i32 = 900;
/// Поле панели слева и справа.
const BT_SIDE: i32 = 16;
/// Кнопка подвала: высота, зазор между соседними, внутреннее поле по бокам.
const BT_BTN_H: i32 = 30;
const BT_BTN_GAP: i32 = 8;
const BT_BTN_PAD: i32 = 12;
/// Зазор между подписью клавиши и подписью действия внутри кнопки.
const BT_KEY_GAP: i32 = 8;

/// Нарисовать строку текста одним элементом (см. text.rs). Возвращает ширину.
/// Начертание — Regular: им набрано всё, что читают глазами (заголовки окон,
/// имена устройств, подписи пунктов).
fn draw_text(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    x: i32, y: i32,
    text: &str,
    scale: i32,
    color: [f32; 4],
    slot: usize,
    out: &mut Vec<OutputRenderElements>,
) -> i32 {
    draw_text_w(state, renderer, x, y, text, crate::text::Weight::Regular, scale, color, slot, out)
}

/// То же, но заданным начертанием. SemiBold отдан коротким подписям поверх
/// полупрозрачных плашек — часам, букве раскладки, значку стола, заголовкам
/// меню и горячим клавишам: тонкий штрих Regular на таком кегле размывается о
/// то, что просвечивает снизу, и подпись читается хуже подложки.
#[allow(clippy::too_many_arguments)]
fn draw_text_w(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    x: i32, y: i32,
    text: &str,
    weight: crate::text::Weight,
    scale: i32,
    color: [f32; 4],
    slot: usize,
    out: &mut Vec<OutputRenderElements>,
) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let (buf, w, h) = state.text_cache.buffer_w(text, weight, scale, color, slot);
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer, Point::<f64, Physical>::from((x as f64, y as f64)), buf,
        None, None, Some(Size::<i32, Logical>::from((w, h))), Kind::Unspecified,
    ) {
        Ok(el) => out.push(OutputRenderElements::Memory(el)),
        Err(e) => tracing::warn!("plx/udev: text line: {:?}", e),
    }
    w
}

/// Меню блютуза: список устройств, состояние адаптера и подсказка по клавишам.
///
/// Приклеено к ЭКРАНУ, как панель столов: камера и зум на него не влияют, и
/// хит-тест (см. bluetooth.rs::bt_click) считает в тех же экранных пикселях.
fn build_bluetooth_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if !state.bt_menu_open() {
        return els;
    }
    let Some(mode) = output.current_mode() else { return els };
    let Some(bt) = state.bt.as_ref() else { return els };

    let devices = bt.snap.devices.clone();
    let powered = bt.snap.powered;
    let discovering = bt.snap.discovering;
    let has_adapter = bt.snap.adapter.is_some();
    let sel = bt.sel;
    let notice = bt.notice_text().map(|s| s.to_string());
    let confirm = bt.confirm.as_ref().map(|c| (c.name.clone(), c.passkey));

    // ── Подвал: кнопки, а не строка подсказки ────────────────────────────────
    // Раскладываем слева направо и переносим на новую строку, как только
    // очередная кнопка не влезает в ширину панели. Поэтому подписи любой длины
    // помещаются ВСЕГДА — в отличие от прежней однострочной шпаргалки, которая
    // при шести действиях просто уезжала за край панели (см. BT_MENU_W).
    let specs = bt.button_specs();
    let text_h = crate::text::height(BT_TEXT);
    let avail = BT_MENU_W - 2 * BT_SIDE;
    // (смещение по X внутри строки, номер строки, ширина кнопки)
    let mut plan: Vec<(i32, i32, i32)> = Vec::with_capacity(specs.len());
    let mut cx = 0;
    let mut btn_line = 0;
    for (_, key, label, _) in &specs {
        let bw = 2 * BT_BTN_PAD
            + crate::text::width(key, BT_TEXT)
            + BT_KEY_GAP
            + crate::text::width(label, BT_TEXT);
        if cx > 0 && cx + bw > avail {
            btn_line += 1;
            cx = 0;
        }
        plan.push((cx, btn_line, bw));
        cx += bw + BT_BTN_GAP;
    }
    let btn_lines = btn_line + 1;

    // Высота: шапка + строки + подвал (строка состояния и ряды кнопок).
    // Список режем по экрану, а не по числу устройств: при поиске их набегает
    // десяток за минуту.
    let head_h = 46;
    // Строка состояния (подсказка/код сопряжения) + ряды кнопок + поля.
    let foot_h = text_h + 10 + btn_lines * BT_BTN_H + (btn_lines - 1) * BT_BTN_GAP + 14;
    let max_rows = (((mode.size.h - 200 - foot_h) / BT_ROW_H).max(3) as usize).min(12);
    let shown = devices.len().min(max_rows);
    let menu_h = head_h + (shown.max(1) as i32) * BT_ROW_H + foot_h + 12;
    let x = (mode.size.w - BT_MENU_W) / 2;
    let y = (mode.size.h - menu_h) / 2;

    // Цвета premultiplied: рендер складывает компоненты с фоном по (1 − alpha)
    // и сам на альфу их не домножает (см. заметку в build_portal_pick_elements).
    // Без домножения фон меню светился сквозь обои — ровно это и было видно на
    // первом снимке.
    const BG: [f32; 4] = [0.030, 0.030, 0.045, 0.96];
    const SEL_BG: [f32; 4] = [0.130, 0.230, 0.330, 0.88];
    const WHITE: [f32; 4] = [0.95, 0.95, 0.97, 1.0];
    const DIM: [f32; 4] = [0.60, 0.62, 0.68, 1.0];
    const ACCENT: [f32; 4] = [0.35, 0.75, 0.95, 1.0];
    const WARN: [f32; 4] = [0.95, 0.55, 0.30, 1.0];
    /// Подложка кнопки подвала: доступной и недоступной.
    const BTN_BG: [f32; 4] = [0.105, 0.145, 0.205, 0.95];
    const BTN_BG_OFF: [f32; 4] = [0.055, 0.058, 0.075, 0.90];

    let mut idx = 0usize;
    let mut pool = std::mem::take(&mut state.bt_ids);
    let фон = меню_фон(state, renderer, x, y, BT_MENU_W, menu_h, BG, &mut els);
    rounded_solid(&mut pool, &mut idx, x, y, BT_MENU_W, menu_h, 16, фон, &mut els);

    // Шапка: состояние адаптера. Точка слева — включён/выключен.
    let dot_color = if !has_adapter { DIM } else if powered { ACCENT } else { DIM };
    rounded_solid(&mut pool, &mut idx, x + 18, y + 16, 14, 14, 7, dot_color, &mut els);
    state.bt_ids = pool;

    // Про клавиши в шапке больше не пишем: они подписаны на кнопках подвала.
    let head = if !has_adapter {
        "BLUETOOTH - no adapter".to_string()
    } else if !powered {
        "BLUETOOTH - off".to_string()
    } else if discovering {
        "BLUETOOTH - scanning...".to_string()
    } else {
        format!("BLUETOOTH - {} connected", devices.iter().filter(|d| d.connected).count())
    };
    let mut slot = 0usize;
    draw_text_w(state, renderer, x + 44, y + 12, &head, crate::text::Weight::Semi, BT_TEXT, WHITE, slot, &mut els);
    slot += 1;

    // Строки устройств.
    let mut rows = Vec::new();
    // Окно прокрутки: держим выбранную строку внутри видимой части.
    let first = if sel >= shown { sel + 1 - shown } else { 0 };
    for (i, dev) in devices.iter().enumerate().skip(first).take(shown) {
        let ry = y + head_h + (i - first) as i32 * BT_ROW_H;
        if i == sel {
            let mut pool = std::mem::take(&mut state.bt_ids);
            rounded_solid(
                &mut pool, &mut idx, x + 10, ry, BT_MENU_W - 20, BT_ROW_H - 4, 8,
                SEL_BG, &mut els,
            );
            state.bt_ids = pool;
        }
        // Точка состояния: подключено — акцент, сопряжено — тускло, чужое — нет.
        if dev.connected || dev.paired {
            let c = if dev.connected { ACCENT } else { DIM };
            let mut pool = std::mem::take(&mut state.bt_ids);
            rounded_solid(&mut pool, &mut idx, x + 22, ry + 12, 10, 10, 5, c, &mut els);
            state.bt_ids = pool;
        }

        // Имя режем по ширине панели, чтобы правые метки не съезжали.
        let name: String = dev.name.chars().take(28).collect();
        let color = if dev.connected { WHITE } else { DIM };
        draw_text(state, renderer, x + 44, ry + 7, &name, BT_TEXT, color, slot, &mut els);
        slot += 1;

        // Справа: заряд, тип, сила сигнала — что известно.
        let mut right = String::new();
        if let Some(b) = dev.battery {
            right.push_str(&format!("{b}%  "));
        }
        let kind = dev.kind();
        if !kind.is_empty() {
            right.push_str(kind);
            right.push_str("  ");
        }
        if !dev.paired {
            if let Some(rssi) = dev.rssi {
                right.push_str(&format!("{rssi}dBm"));
            }
        }
        let right = right.trim_end().to_string();
        if !right.is_empty() {
            let w = crate::text::width(&right, BT_TEXT);
            let c = if dev.battery.is_some_and(|b| b <= 20) { WARN } else { DIM };
            draw_text(
                state, renderer, x + BT_MENU_W - 24 - w, ry + 7, &right,
                BT_TEXT, c, slot, &mut els,
            );
            slot += 1;
        }
        rows.push(crate::bluetooth::Row {
            x: x + 10, y: ry, w: BT_MENU_W - 20, h: BT_ROW_H - 4, device: i,
        });
    }

    if devices.is_empty() {
        let text = if powered { "no devices found yet" } else { "adapter is off" };
        draw_text(state, renderer, x + 44, y + head_h + 8, text, BT_TEXT, DIM, slot, &mut els);
        slot += 1;
    }

    // ── Подвал ───────────────────────────────────────────────────────────────
    // Сверху строка состояния: код сопряжения, результат последней команды или
    // имя выбранного устройства. Под ней — ряды кнопок.
    let foot_y = y + menu_h - foot_h + 2;
    match (&confirm, &notice) {
        (Some((name, passkey)), _) => {
            let text = if *passkey > 0 {
                format!("pairing code {passkey:06} - confirm with Enter, reject with Esc ({name})")
            } else {
                format!("allow pairing with {name}?")
            };
            draw_text(state, renderer, x + BT_SIDE, foot_y, &text, BT_TEXT, WARN, slot, &mut els);
        }
        (None, Some(text)) => {
            draw_text(state, renderer, x + BT_SIDE, foot_y, text, BT_TEXT, ACCENT, slot, &mut els);
        }
        // Нечего сообщить — показываем, на что подействуют кнопки. Раньше здесь
        // стояла шпаргалка по клавишам, но теперь клавиши подписаны на самих
        // кнопках, и повторять их незачем.
        (None, None) => {
            let text = devices
                .get(sel)
                .map(|d| format!("selected: {}", d.name.chars().take(46).collect::<String>()))
                .unwrap_or_else(|| "no device selected".to_string());
            draw_text(state, renderer, x + BT_SIDE, foot_y, &text, BT_TEXT, DIM, slot, &mut els);
        }
    }
    slot += 1;

    // Кнопки. Клик по ним разбирает bt_click по этим же прямоугольникам —
    // поэтому геометрия и складывается здесь, в момент отрисовки.
    let btn_y0 = foot_y + text_h + 10;
    let mut buttons = Vec::with_capacity(specs.len());
    for ((action, key, label, enabled), (dx, ln, bw)) in specs.iter().zip(plan.iter()) {
        let bx = x + BT_SIDE + dx;
        let by = btn_y0 + ln * (BT_BTN_H + BT_BTN_GAP);
        let mut pool = std::mem::take(&mut state.bt_ids);
        rounded_solid(
            &mut pool, &mut idx, bx, by, *bw, BT_BTN_H, 8,
            if *enabled { BTN_BG } else { BTN_BG_OFF }, &mut els,
        );
        state.bt_ids = pool;

        let ty = by + (BT_BTN_H - text_h) / 2;
        let key_color = if *enabled { ACCENT } else { DIM };
        let kw = draw_text_w(state, renderer, bx + BT_BTN_PAD, ty, key, crate::text::Weight::Semi, BT_TEXT, key_color, slot, &mut els);
        slot += 1;
        let label_color = if *enabled { WHITE } else { DIM };
        draw_text(
            state, renderer, bx + BT_BTN_PAD + kw + BT_KEY_GAP, ty, label,
            BT_TEXT, label_color, slot, &mut els,
        );
        slot += 1;

        buttons.push(crate::bluetooth::Button {
            x: bx, y: by, w: *bw, h: BT_BTN_H,
            action: *action, key, label: label.clone(), enabled: *enabled,
        });
    }

    if let Some(bt) = state.bt.as_mut() {
        bt.rows = rows;
        bt.buttons = buttons;
    }
    // Список кадра идёт ОТ ПЕРЕДНЕГО ПЛАНА К ЗАДНЕМУ (см. render_surface), а
    // собирали мы естественно: фон, подсветка, текст. Без разворота фон панели
    // ложится ПОВЕРХ текста — ровно это и было видно на первом снимке: читалась
    // только та часть подсказки, что вылезала за край панели.
    els.reverse();
    els
}


// ── Списочные меню (вайфай, звук) ────────────────────────────────────────────
//
// Один каркас на оба: панель по центру, шапка, строки с прокруткой, подвал с
// подсказкой. Блютуз старше и рисуется своим кодом — переписывать рабочее меню
// ради общего каркаса значило бы чинить то, что не сломано.

/// Строка списочного меню.
struct MenuRow {
    /// Заголовок раздела: не выбирается и не кликается.
    header: bool,
    left: String,
    right: String,
    /// Точка слева: 2 — активно (акцент), 1 — известно (тускло), 0 — нет точки.
    dot: u8,
    /// Уровень сигнала 0..100 — рисуется столбиками у правого края.
    strength: Option<u8>,
}

impl MenuRow {
    fn head(text: &str) -> Self {
        Self { header: true, left: text.to_string(), right: String::new(), dot: 0, strength: None }
    }
}

const MENU_W: i32 = 880;
const MENU_ROW_H: i32 = 34;
const MENU_TEXT: i32 = 2;
const MENU_HEAD_H: i32 = 46;
const MENU_FOOT_H: i32 = 30;

/// Фон плашки меню, когда под ней ЕСТЬ стекло (см. `стекло`): полупрозрачный,
/// иначе размытия под ним не видно вовсе. Без стекла остаётся прежний почти
/// глухой `BG` — меню поверх пёстрых обоев иначе не читается.
const MENU_BG_СТЕКЛО: [f32; 4] = [0.030, 0.030, 0.045, 0.55];

/// Плашка меню: стекло под ней (если размытие есть) и подходящий цвет заливки.
/// Возвращает цвет, которым дальше рисуется сама плашка.
#[allow(clippy::too_many_arguments)]
fn меню_фон(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    x: i32, y: i32, w: i32, h: i32,
    глухой: [f32; 4],
    els: &mut Vec<OutputRenderElements>,
) -> [f32; 4] {
    // Стекло идёт ПЕРЕД заливкой: списки меню собираются от заднего плана к
    // переднему и разворачиваются в конце.
    match стекло(state, renderer, x, y, w, h, 16, БЛЮР_МЕНЮ) {
        Some(el) => {
            els.push(el);
            MENU_BG_СТЕКЛО
        }
        None => глухой,
    }
}

/// Каркас меню. Возвращает элементы кадра и прямоугольники строк — по ним
/// хит-тест ловит клики (порядок совпадает с `rows`).
#[allow(clippy::too_many_arguments)]
fn build_list_menu(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
    title: &str,
    rows: &[MenuRow],
    sel: usize,
    foot: (&str, [f32; 4]),
) -> (Vec<OutputRenderElements>, Vec<crate::tray::Rect>) {
    const BG: [f32; 4] = [0.030, 0.030, 0.045, 0.96];
    const SEL_BG: [f32; 4] = [0.130, 0.230, 0.330, 0.88];
    const WHITE: [f32; 4] = [0.95, 0.95, 0.97, 0.98];
    const DIM: [f32; 4] = [0.62, 0.62, 0.70, 0.75];
    const ACCENT: [f32; 4] = [0.35, 0.75, 0.95, 0.95];

    let mut els = Vec::new();
    let mut hits = Vec::new();
    let Some(mode) = output.current_mode() else { return (els, hits) };

    // Список режем по экрану, а не по числу строк: точек в эфире набегает
    // десяток за минуту.
    let max_rows = (((mode.size.h - 220) / MENU_ROW_H).max(3) as usize).min(14);
    let shown = rows.len().min(max_rows);
    let menu_h = MENU_HEAD_H + (shown.max(1) as i32) * MENU_ROW_H + MENU_FOOT_H + 12;
    let x = (mode.size.w - MENU_W) / 2;
    let y = (mode.size.h - menu_h) / 2;

    let mut idx = 0usize;
    let mut pool = std::mem::take(&mut state.menu_ids);
    let фон = меню_фон(state, renderer, x, y, MENU_W, menu_h, BG, &mut els);
    rounded_solid(&mut pool, &mut idx, x, y, MENU_W, menu_h, 16, фон, &mut els);
    state.menu_ids = pool;

    let mut slot = 0usize;
    draw_text_w(state, renderer, x + 22, y + 12, title, crate::text::Weight::Semi, MENU_TEXT, WHITE, slot, &mut els);
    slot += 1;

    // Окно прокрутки: держим выбранную строку внутри видимой части.
    let first = if sel >= shown { sel + 1 - shown } else { 0 };
    for (i, row) in rows.iter().enumerate() {
        if i < first || i >= first + shown {
            hits.push(crate::tray::Rect { x: 0, y: 0, w: 0, h: 0 });
            continue;
        }
        let ry = y + MENU_HEAD_H + (i - first) as i32 * MENU_ROW_H;
        let rect = crate::tray::Rect { x: x + 10, y: ry, w: MENU_W - 20, h: MENU_ROW_H - 4 };
        hits.push(rect);

        if row.header {
            draw_text(
                state, renderer, x + 22, ry + 9, &row.left, 1, DIM, slot, &mut els,
            );
            slot += 1;
            continue;
        }

        let mut pool = std::mem::take(&mut state.menu_ids);
        if i == sel {
            rounded_solid(
                &mut pool, &mut idx, rect.x, rect.y, rect.w, rect.h, 8, SEL_BG, &mut els,
            );
        }
        if row.dot > 0 {
            let c = if row.dot >= 2 { ACCENT } else { DIM };
            rounded_solid(&mut pool, &mut idx, x + 22, ry + 12, 10, 10, 5, c, &mut els);
        }
        // Уровень сигнала — четыре столбика у правого края: цифра рядом есть,
        // но глазом ряд читается быстрее.
        if let Some(s) = row.strength {
            let bx = x + MENU_W - 24 - 4 * 7;
            for n in 0..4i32 {
                let lit = s as i32 > n * 25;
                let h = 5 + n * 4;
                let c = if lit { ACCENT } else { [0.5, 0.5, 0.55, 0.30] };
                rounded_solid(
                    &mut pool, &mut idx, bx + n * 7, ry + 8 + (17 - h), 5, h, 2, c, &mut els,
                );
            }
        }
        state.menu_ids = pool;

        let left: String = row.left.chars().take(34).collect();
        let color = if row.dot >= 2 { WHITE } else { DIM };
        draw_text(state, renderer, x + 44, ry + 7, &left, MENU_TEXT, color, slot, &mut els);
        slot += 1;

        if !row.right.is_empty() {
            let bars = if row.strength.is_some() { 4 * 7 + 10 } else { 0 };
            let w = crate::text::width(&row.right, MENU_TEXT);
            draw_text(
                state, renderer, x + MENU_W - 24 - bars - w, ry + 7, &row.right,
                MENU_TEXT, DIM, slot, &mut els,
            );
            slot += 1;
        }
    }

    if rows.is_empty() {
        draw_text(
            state, renderer, x + 44, y + MENU_HEAD_H + 8, "nothing here yet",
            MENU_TEXT, DIM, slot, &mut els,
        );
        slot += 1;
    }

    draw_text(
        state, renderer, x + 22, y + menu_h - MENU_FOOT_H + 2, foot.0,
        MENU_TEXT, foot.1, slot, &mut els,
    );

    // Список кадра идёт от переднего плана к заднему (см. render_surface), а
    // собирали мы естественно: фон, подсветка, текст.
    els.reverse();
    (els, hits)
}

#[cfg(feature = "share")]
const SHARE_W: i32 = 660;
#[cfg(feature = "share")]
const SHARE_ROW_H: i32 = 40;
#[cfg(feature = "share")]
const SHARE_TEXT: i32 = 2;

/// Цвет участника (0xAARRGGBB из `share::ЦВЕТА`) в цвет заливки.
///
/// Альфа единица, поэтому домножать на неё нечего — но помнить про
/// premultiplied всё равно надо: возьми кто-нибудь отсюда полупрозрачный
/// цвет, точка засветилась бы сквозь панель (см. `pooled_solid`).
#[cfg(feature = "share")]
fn цвет_участника(c: u32) -> [f32; 4] {
    [
        ((c >> 16) & 0xff) as f32 / 255.0,
        ((c >> 8) & 0xff) as f32 / 255.0,
        (c & 0xff) as f32 / 255.0,
        1.0,
    ]
}

/// Панель управления раздачей: кто подключён, кого выгнать, кого забанить.
///
/// Открывается ПОВТОРНЫМ Super+Shift+S у хозяина машины. У гостя то же
/// сочетание значит «выйти» и до хоста не доходит вовсе — его перехватывает
/// сам `plx-share` (единственная клавиша, которую он оставляет себе).
///
/// Приклеена к экрану, как остальные меню: камера и зум на неё не влияют.
// Панель раздачи — то же самое: без фичи её нечем открыть.
#[cfg(not(feature = "share"))]
fn build_share_panel_elements(
    _state: &mut Parallax,
    _renderer: &mut GlesRenderer,
    _output: &Output,
) -> Vec<OutputRenderElements> {
    Vec::new()
}

#[cfg(feature = "share")]
fn build_share_panel_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if !state.раздача_панель_открыта() {
        return els;
    }
    let Some(mode) = output.current_mode() else { return els };

    // Снимок состояния целиком: ниже `state` нужен отрисовке изменяемо, и
    // держать на нём ссылку чтения уже нельзя.
    let (код, порт, выбран, забанено, строки) = {
        let Some(раздача) = state.раздача.as_ref() else { return els };
        let строки: Vec<(String, [f32; 4], String, String, bool)> = раздача
            .гости
            .iter()
            .map(|гость| {
                (
                    гость.имя.clone(),
                    цвет_участника(гость.цвет),
                    гость.адрес.to_string(),
                    if !гость.впущен {
                        т!("здоровается…", "saying hello…").to_string()
                    } else {
                        // Долг очереди показываем только когда он есть: это
                        // единственный видимый признак «гость не успевает»,
                        // и в норме он должен быть пуст.
                        let долг = гость.долг();
                        match долг {
                            0 => format!("{}×{}", гость.кадр_кодировщика.0, гость.кадр_кодировщика.1),
                            n => тф!("{}×{}  очередь {n}", "{}×{}  queue {n}", гость.кадр_кодировщика.0, гость.кадр_кодировщика.1),
                        }
                    },
                    гость.впущен,
                )
            })
            .collect();
        (раздача.код.clone(), раздача.порт, раздача.выбран, раздача.бан.len(), строки)
    };

    // Цвета premultiplied — см. заметку в build_bluetooth_elements.
    const BG: [f32; 4] = [0.030, 0.030, 0.045, 0.96];
    const SEL_BG: [f32; 4] = [0.130, 0.230, 0.330, 0.88];
    const WHITE: [f32; 4] = [0.95, 0.95, 0.97, 1.0];
    const DIM: [f32; 4] = [0.60, 0.62, 0.68, 1.0];
    const WARN: [f32; 4] = [0.95, 0.55, 0.30, 1.0];

    let head_h = 54;
    let foot_h = 46;
    let видимых = строки.len().max(1) as i32;
    let menu_h = head_h + видимых * SHARE_ROW_H + foot_h;
    let x = (mode.size.w - SHARE_W) / 2;
    let y = (mode.size.h - menu_h) / 2;

    let mut idx = 0usize;
    let mut pool = std::mem::take(&mut state.share_ids);
    let фон = меню_фон(state, renderer, x, y, SHARE_W, menu_h, BG, &mut els);
    rounded_solid(&mut pool, &mut idx, x, y, SHARE_W, menu_h, 16, фон, &mut els);

    // Подсветка выбранной строки — до текста, он ляжет поверх.
    if !строки.is_empty() && выбран < строки.len() {
        let ry = y + head_h + выбран as i32 * SHARE_ROW_H;
        rounded_solid(
            &mut pool, &mut idx, x + 10, ry, SHARE_W - 20, SHARE_ROW_H - 4, 8,
            SEL_BG, &mut els,
        );
    }
    // Точки цвета участников — тоже заливки, пока пул в руках.
    for (i, (_, цвет, _, _, впущен)) in строки.iter().enumerate() {
        let ry = y + head_h + i as i32 * SHARE_ROW_H;
        let c = if *впущен { *цвет } else { DIM };
        rounded_solid(&mut pool, &mut idx, x + 22, ry + 14, 12, 12, 6, c, &mut els);
    }
    state.share_ids = pool;

    let mut slot = 0usize;
    let шапка = тф!("РАЗДАЧА — код {код}, порт {порт}", "SHARING — code {код}, port {порт}");
    draw_text_w(
        state, renderer, x + 22, y + 16, &шапка,
        crate::text::Weight::Semi, SHARE_TEXT, WHITE, slot, &mut els,
    );
    slot += 1;

    if строки.is_empty() {
        draw_text(
            state, renderer, x + 22, y + head_h + 10, т!("никто не подключён", "nobody is connected"),
            SHARE_TEXT, DIM, slot, &mut els,
        );
        slot += 1;
    }
    for (i, (имя, _, адрес, состояние, впущен)) in строки.iter().enumerate() {
        let ry = y + head_h + i as i32 * SHARE_ROW_H + 10;
        let цвет_имени = if *впущен { WHITE } else { DIM };
        // Имя режем по длине: гость называется сам, и длинная строка съехала бы
        // на адрес.
        let имя: String = имя.chars().take(22).collect();
        draw_text(state, renderer, x + 46, ry, &имя, SHARE_TEXT, цвет_имени, slot, &mut els);
        slot += 1;
        draw_text(state, renderer, x + 300, ry, адрес, SHARE_TEXT, DIM, slot, &mut els);
        slot += 1;
        let w = crate::text::width(состояние, SHARE_TEXT);
        draw_text(
            state, renderer, x + SHARE_W - 24 - w, ry, состояние,
            SHARE_TEXT, DIM, slot, &mut els,
        );
        slot += 1;
    }

    let подвал = if забанено > 0 {
        тф!("x выгнать   b забанить   s закончить   Esc закрыть        в бане: {забанено}", "x kick   b ban   s stop   Esc close        banned: {забанено}")
    } else {
        т!("x выгнать   b забанить   s закончить   Esc закрыть", "x kick   b ban   s stop   Esc close").to_string()
    };
    draw_text(
        state, renderer, x + 22, y + menu_h - foot_h + 12, &подвал,
        SHARE_TEXT, if забанено > 0 { WARN } else { DIM }, slot, &mut els,
    );

    // Список кадра идёт от переднего плана к заднему (см. render_surface), а
    // собирали мы естественно: фон, подсветка, текст.
    els.reverse();
    els
}

/// Меню вайфая: список сетей, ввод пароля и подсказка по клавишам.
fn build_wifi_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    const WARN: [f32; 4] = [0.95, 0.55, 0.35, 0.95];
    const ACCENT: [f32; 4] = [0.35, 0.75, 0.95, 0.95];
    const DIM: [f32; 4] = [0.62, 0.62, 0.70, 0.75];

    if !state.wifi_menu_open() {
        return Vec::new();
    }
    let Some(w) = state.wifi.as_ref() else { return Vec::new() };
    let snap = w.snap.clone();
    let sel = w.sel;
    let notice = w.notice_text().map(str::to_string);
    // Пароль на экране — звёздочками: за спиной бывают люди.
    let ask = w.ask.as_ref().map(|a| {
        (format!("Password for {}: {}", a.ssid, "*".repeat(a.text.chars().count())), a.text.is_empty())
    });

    let title = if !snap.present {
        "WI-FI - no wireless device".to_string()
    } else if !snap.enabled {
        "WI-FI - radio off, press P".to_string()
    } else if snap.connecting {
        "WI-FI - connecting...".to_string()
    } else {
        match snap.ssid.as_deref() {
            Some(s) => format!("WI-FI - {s}"),
            None => "WI-FI - not connected".to_string(),
        }
    };

    let rows: Vec<MenuRow> = snap
        .aps
        .iter()
        .map(|ap| {
            let mut right = String::new();
            if ap.active {
                right.push_str("connected  ");
            } else if ap.saved {
                right.push_str("saved  ");
            }
            right.push_str(if !ap.secure {
                "open"
            } else if ap.sae {
                "WPA3"
            } else {
                "WPA2"
            });
            MenuRow {
                header: false,
                left: ap.ssid.clone(),
                right,
                dot: if ap.active { 2 } else if ap.saved { 1 } else { 0 },
                strength: Some(ap.strength),
            }
        })
        .collect();

    let foot: (String, [f32; 4]) = match (&ask, &notice) {
        (Some((line, empty)), _) => (
            format!("{line}{}   Enter connect  Esc cancel", if *empty { "_" } else { "" }),
            WARN,
        ),
        (None, Some(text)) => (text.clone(), ACCENT),
        (None, None) => (
            "Enter connect  D disconnect  F forget  S scan  P radio  Esc".to_string(),
            DIM,
        ),
    };

    let (els, hits) = build_list_menu(
        state, renderer, output, &title, &rows, sel, (&foot.0, foot.1),
    );
    if let Some(w) = state.wifi.as_mut() {
        w.rows = hits.into_iter().enumerate().map(|(i, r)| (r, i)).collect();
    }
    els
}

/// Меню звука: устройства вывода и ввода двумя разделами.
fn build_audio_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    const ACCENT: [f32; 4] = [0.35, 0.75, 0.95, 0.95];
    const DIM: [f32; 4] = [0.62, 0.62, 0.70, 0.75];

    if !state.audio_menu_open() {
        return Vec::new();
    }
    let Some(a) = state.audio.as_ref() else { return Vec::new() };
    let snap = a.snap.clone();
    let picks = a.picks();
    let sel = a.sel;
    let notice = a.notice_text().map(str::to_string);

    // Строки идут вперемешку с заголовками разделов, поэтому храним, какой
    // строке какой pick соответствует, — по этому же порядку придут и
    // прямоугольники клика.
    let mut rows = Vec::new();
    let mut row_pick: Vec<Option<crate::audio::Pick>> = Vec::new();
    let mut sel_row = 0usize;

    for (sink, list) in [(true, &snap.sinks), (false, &snap.sources)] {
        rows.push(MenuRow::head(if sink { "OUTPUT" } else { "INPUT" }));
        row_pick.push(None);
        for (index, dev) in list.iter().enumerate() {
            let pick = crate::audio::Pick { sink, index };
            let mut right = String::new();
            if dev.default {
                right.push_str("default  ");
            }
            right.push_str(&format!("{}%", (dev.volume * 100.0).round() as i32));
            if dev.muted {
                right.push_str("  muted");
            }
            if picks.get(sel) == Some(&pick) {
                sel_row = rows.len();
            }
            rows.push(MenuRow {
                header: false,
                left: dev.description.clone(),
                right,
                dot: if dev.default { 2 } else { 0 },
                strength: None,
            });
            row_pick.push(Some(pick));
        }
    }

    let title = match snap.default_sink() {
        Some(d) => format!("SOUND - {}", d.description.chars().take(40).collect::<String>()),
        None => "SOUND - no output device".to_string(),
    };
    let foot = match &notice {
        Some(text) => (text.clone(), ACCENT),
        None => (
            "Enter use  M mute  -/+ volume  Esc".to_string(),
            DIM,
        ),
    };

    let (els, hits) = build_list_menu(
        state, renderer, output, &title, &rows, sel_row, (&foot.0, foot.1),
    );
    if let Some(a) = state.audio.as_mut() {
        a.rows = hits
            .into_iter()
            .zip(row_pick)
            .filter_map(|(r, p)| p.map(|p| (r, p)))
            .collect();
    }
    els
}

/// Поиск окон (Super+F): набранная строка в шапке и подходящие окна списком.
///
/// Каркас общий с меню вайфая и звука (build_list_menu) — намеренно: поиск
/// ведёт себя как ещё одно приклеенное к экрану меню, и человеку не приходится
/// заново учить, где тут выбранная строка и где подсказка.
fn build_search_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    const ACCENT: [f32; 4] = [0.35, 0.75, 0.95, 0.95];
    const DIM: [f32; 4] = [0.62, 0.62, 0.70, 0.75];

    let Some(ui) = state.search.as_ref() else { return Vec::new() };
    let query = ui.query.clone();
    let sel = ui.sel;
    let current = state.viewport.current_tags();

    let rows: Vec<MenuRow> = ui
        .hits
        .iter()
        .map(|h| {
            // Справа — приложение и номер стола: два окна с одинаковым
            // заголовком («Telegram», «Терминал») иначе не различить.
            let mut right = String::new();
            if !h.app.is_empty() {
                // app_id вида «com.mitchellh.ghostty» показываем последним
                // куском: обрезанное по ширине «com.mitchellh.ghos» не говорит
                // человеку ничего, а «ghostty» — говорит всё.
                let короткий = h.app.rsplit('.').next().unwrap_or(&h.app);
                right.push_str(&короткий.chars().take(18).collect::<String>());
            }
            if h.tags != 0 {
                if !right.is_empty() {
                    right.push_str("  ");
                }
                right.push_str(&тф!("стол {}", "workspace {}", h.tags.trailing_zeros() + 1));
            }
            MenuRow {
                header: false,
                left: h.title.clone(),
                right,
                // Точка слева: окно на текущем столе — акцентом, чужое — тускло.
                dot: if h.tags & current != 0 { 2 } else { 1 },
                strength: None,
            }
        })
        .collect();

    // Курсор в конце строки — видно, что поле принимает ввод, даже когда пусто.
    let title = тф!("ПОИСК ОКНА: {query}_", "FIND WINDOW: {query}_");
    // Стрелок ↑↓ в битмап-шрифте parallax нет — на экране вместо них выходили «??»
    // (проверено снимком 05.08.2026). Пишем словами.
    let foot = if rows.is_empty() && !query.is_empty() {
        (т!("ничего не нашлось  -  Esc отмена", "nothing found  -  Esc cancel").to_string(), DIM)
    } else {
        (т!("Enter перейти  Tab выбор  Esc отмена", "Enter go  Tab select  Esc cancel").to_string(), ACCENT)
    };

    let (els, hits) = build_list_menu(
        state, renderer, output, &title, &rows, sel, (&foot.0, foot.1),
    );
    if let Some(ui) = state.search.as_mut() {
        ui.rows = hits;
    }
    els
}

/// Цвета полки. Сплошные прямоугольники ждут premultiplied-компоненты
/// (см. заметку в build_bluetooth_elements), поэтому в `rounded_solid` они
/// уходят через `premul`, а в маски значков — как есть: там домножением
/// занимается растеризатор.
const TRAY_DIM: [f32; 4] = [0.80, 0.80, 0.85, 0.62];
const TRAY_OFF: [f32; 4] = [0.75, 0.75, 0.80, 0.20];
const TRAY_ON: [f32; 4] = [0.35, 0.75, 0.95, 0.95];
const TRAY_WARN: [f32; 4] = [0.95, 0.45, 0.30, 0.95];
const TRAY_GOOD: [f32; 4] = [0.45, 0.85, 0.55, 0.95];

fn premul(c: [f32; 4]) -> [f32; 4] {
    [c[0] * c[3], c[1] * c[3], c[2] * c[3], c[3]]
}

/// Значок-маска по центру ячейки: высота задана, ширина берётся из пропорций
/// самой маски (см. text.rs::bitmap_fit).
fn draw_mask(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    name: &str,
    rows: &[u32],
    mask_w: i32,
    cell: crate::tray::Rect,
    h: i32,
    color: [f32; 4],
    out: &mut Vec<OutputRenderElements>,
) {
    let w = (h * mask_w / rows.len() as i32).max(1);
    let x = cell.x + (cell.w - w) / 2;
    let y = cell.y + (cell.h - h) / 2;
    let (buf, _, _) = state.text_cache.bitmap_fit(name, rows, mask_w, w, h, color, 0);
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer,
        Point::<f64, Physical>::from((x as f64, y as f64)),
        buf,
        None,
        None,
        Some(Size::<i32, Logical>::from((w, h))),
        Kind::Unspecified,
    ) {
        Ok(el) => out.push(OutputRenderElements::Memory(el)),
        Err(e) => tracing::warn!("plx/udev: shelf icon {}: {:?}", name, e),
    }
}

/// Высота значка батареи и его рамка внутри ячейки. Нужна дважды — обводке
/// (маска) и заливке (прямоугольник), поэтому считается одним местом.
fn battery_box(cell: crate::tray::Rect) -> (i32, i32, i32, i32) {
    battery_box_fit(cell, i32::MAX)
}

/// То же, но значок обязан влезть в `max_w` по ширине.
///
/// Нужно панели: там под значок отведён квадрат в [`bar::DOT`] (20 px), а
/// значок батареи лежит на боку — 26×14, то есть ШИРЕ своей ячейки. Без
/// вписывания он вылезал на проценты рядом и вдобавок вставал на 3 px левее
/// середины: раскладка (bar.rs) считает ширину ячейки по DOT, а рисование
/// брало высоту от бара и ширину от пропорций маски — две разные арифметики.
fn battery_box_fit(cell: crate::tray::Rect, max_w: i32) -> (i32, i32, i32, i32) {
    let rows = BATTERY.len() as i32;
    let mut h = (BAR_H * 7 / 16).max(6);
    let mut w = h * BATTERY_W / rows;
    if w > max_w {
        w = max_w;
        h = (w * rows / BATTERY_W).max(4);
    }
    (cell.x + (cell.w - w) / 2, cell.y + (cell.h - h) / 2, w, h)
}

/// Высота маски, при которой она не станет шире `max_w`. `want_h` — сколько
/// хотелось бы (маску рисуют от высоты, ширина идёт из её пропорций, см.
/// `draw_mask`).
fn mask_h_fit(mask_w: i32, rows: i32, want_h: i32, max_w: i32) -> i32 {
    want_h.min(max_w * rows / mask_w.max(1)).max(4)
}

/// Полка состояния справа от панели столов: вертикальная полосочка, а по клику
/// из неё выезжает ряд — блютуз, вайфай, звук, батарея, питание.
///
/// Раскладку ячеек даёт `tray::layout`, и ТА ЖЕ функция считает попадания
/// клика (см. tray.rs::tray_click). Второй копии геометрии в хит-тесте нет
/// намеренно: именно так однажды разъехались клики по окнам — на экране одно,
/// в проверке другое.
fn build_tray_elements(
    state: &mut Parallax,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    use crate::tray::{CellKind, PowerAction};

    let mut els = Vec::new();
    // По ТЕКУЩЕМУ столу, как и панель рядом: полноэкранная игра на соседнем
    // столе не повод оставлять этот стол без полки (см.
    // build_workspace_bar_elements — там на этом же месте была та же ошибка).
    // Условие по ДОЛЕ ухода панели: полка висит под ней и уезжает вместе с ней
    // (см. tray::layout и bar::island_y).
    if state.bar_hide >= 1.0 {
        return els;
    }
    let Some(mode) = output.current_mode() else { return els };
    let Some(tray) = state.tray.as_ref() else { return els };

    // Всё нужное снимаем сразу: дальше state занят пулом и кэшем текста.
    let open = tray.open;
    let snap = tray.snap.clone();
    let armed = [PowerAction::Off, PowerAction::Reboot, PowerAction::Sleep]
        .into_iter()
        .find(|a| tray.is_armed(*a));
    // Тире тут нарочно нет: в шрифте (см. text.rs) только ASCII и кириллица,
    // и на месте «—» получался вопросительный знак.
    let hint = armed
        .map(|a| format!("press again: {}", a.human()))
        .or_else(|| tray.notice_text().map(str::to_string));
    // Доля выезда — с той же сглаживающей кривой, что у карточки предпросмотра:
    // полка выезжает быстро и мягко тормозит у своего места.
    let выезд = crate::anim::ease_out_cubic(state.shelf_anim.clamp(0.0, 1.0));
    let lay = crate::tray::layout(
        open, snap.battery.is_some(), mode.size.w, state.bar_hide, выезд,
    );

    // Полка только ПОКАЗЫВАЕТ: вайфай живёт в wifi.rs, звук в audio.rs,
    // блютуз в bluetooth.rs.
    let wifi = state.wifi_snapshot().cloned();
    let volume = state.audio_snapshot().and_then(|s| s.volume());
    let bt = state.bt.as_ref().map(|b| {
        (
            b.snap.powered,
            b.snap.connected_count(),
            b.snap.lowest_battery(),
            b.snap.discovering,
        )
    });
    let bt_color = match bt {
        Some((_, _, Some(bat), _)) if bat <= 20 => TRAY_WARN,
        Some((_, n, _, _)) if n > 0 => TRAY_ON,
        Some((_, _, _, true)) => [0.75, 0.75, 0.35, 0.90],
        Some((true, ..)) => TRAY_DIM,
        _ => TRAY_OFF,
    };

    // ── Подложки и всё, что рисуется прямоугольниками ────────────────────────
    let mut idx = 0usize;
    let mut pool = std::mem::take(&mut state.tray_ids);

    // Полосочки-хвата здесь больше нет — она стала ячейкой правого острова
    // панели (`bar::Cell::Handle`, рисуется в build_bar_elements). Раньше её
    // геометрию брали из `lay.cells[0]`, и закрытая полка возвращала бы теперь
    // пустой список: обращение по нулевому индексу уронило бы кадр.
    if let Some(p) = lay.panel {
        // Стекло ПЕРЕД заливкой: список полки собирается от заднего плана к
        // переднему и разворачивается в конце (els.reverse()), значит
        // добавленное раньше окажется ниже.
        if let Some(el) = стекло(state, renderer, p.x, p.y, p.w, p.h, BAR_RADIUS, БЛЮР_ПОЛКА) {
            els.push(el);
        }
        rounded_solid(&mut pool, &mut idx, p.x, p.y, p.w, p.h, BAR_RADIUS, BAR_BG, &mut els);
    }

    for cell in &lay.cells {
        let r = cell.rect;
        match cell.kind {
            CellKind::VolumeSlider => {
                let track_h = (BAR_H / 5).max(3);
                let ty = r.y + (r.h - track_h) / 2;
                rounded_solid(
                    &mut pool, &mut idx, r.x, ty, r.w, track_h, track_h / 2,
                    [1.0, 1.0, 1.0, 0.13], &mut els,
                );
                if let Some((level, muted)) = volume {
                    let fill = (r.w as f32 * level.clamp(0.0, 1.0)).round() as i32;
                    if fill >= track_h {
                        let c = if muted { TRAY_OFF } else { TRAY_ON };
                        rounded_solid(
                            &mut pool, &mut idx, r.x, ty, fill, track_h, track_h / 2,
                            c, &mut els,
                        );
                    }
                }
            }
            CellKind::Battery => {
                let Some(b) = snap.battery.as_ref() else { continue };
                let (bx, by, bw, bh) = battery_box(r);
                // Окно внутри обводки — в координатах маски BATTERY.
                let (ix, iy, iw, ih) = BATTERY_INNER;
                let x = bx + bw * ix / BATTERY_W;
                let y = by + bh * iy / BATTERY.len() as i32;
                let h = (bh * ih / BATTERY.len() as i32).max(1);
                let w = (bw * iw / BATTERY_W) * b.percent as i32 / 100;
                let c = if b.charging {
                    TRAY_GOOD
                } else if b.percent <= 20 {
                    TRAY_WARN
                } else {
                    TRAY_DIM
                };
                if w > 0 {
                    rounded_solid(&mut pool, &mut idx, x, y, w, h, 0, c, &mut els);
                }
            }
            // Взведённая кнопка питания подсвечена: видно, что следующий клик
            // уже сработает.
            CellKind::Power(a) if armed == Some(a) => {
                rounded_solid(
                    &mut pool, &mut idx, r.x, r.y, r.w, r.h, BAR_RADIUS / 2,
                    [0.95, 0.45, 0.30, 0.30], &mut els,
                );
            }
            _ => {}
        }
    }
    state.tray_ids = pool;

    // ── Значки ───────────────────────────────────────────────────────────────
    for cell in &lay.cells {
        let r = cell.rect;
        match cell.kind {
            CellKind::Bluetooth => {
                draw_mask(state, renderer, "bt-rune", &BT_RUNE, 11, r, BAR_H * 2 / 3, bt_color, &mut els);
            }
            CellKind::Wifi => {
                // Дуг горит столько же, сколько на любом другом индикаторе
                // сигнала: 1 из 3 при слабом, все три при сильном. Выключенный
                // радиомодуль — оранжевая точка при погашенных дугах.
                let h = BAR_H / 2;
                let (lit, dot) = match wifi.as_ref() {
                    None => (0, TRAY_OFF),
                    Some(w) if !w.present => (0, TRAY_OFF),
                    Some(w) if !w.enabled => (0, TRAY_WARN),
                    Some(w) if w.ssid.is_none() => (0, TRAY_DIM),
                    Some(w) => (1 + (w.strength as i32 / 34).min(2), TRAY_ON),
                };
                draw_mask(state, renderer, "wifi-dot", &WIFI_DOT, WIFI_W, r, h, dot, &mut els);
                for (n, mask) in [(1, &WIFI_ARC1), (2, &WIFI_ARC2), (3, &WIFI_ARC3)] {
                    let color = if n <= lit { TRAY_ON } else { TRAY_OFF };
                    let name = ["wifi-arc1", "wifi-arc2", "wifi-arc3"][n as usize - 1];
                    draw_mask(state, renderer, name, mask, WIFI_W, r, h, color, &mut els);
                }
            }
            CellKind::Volume => {
                let muted = volume.is_some_and(|(_, m)| m);
                let (name, mask): (&str, &[u32]) = if muted {
                    ("vol-muted", &VOLUME_MUTED)
                } else {
                    ("vol", &VOLUME)
                };
                let color = match volume {
                    None => TRAY_OFF,
                    Some(_) if muted => TRAY_WARN,
                    Some(_) => TRAY_DIM,
                };
                draw_mask(state, renderer, name, mask, VOLUME_W, r, BAR_H / 2, color, &mut els);
            }
            CellKind::Battery => {
                let Some(b) = snap.battery.as_ref() else { continue };
                let color = if b.percent <= 20 && !b.charging { TRAY_WARN } else { TRAY_DIM };
                let (_, _, _, h) = battery_box(r);
                draw_mask(state, renderer, "battery", &BATTERY, BATTERY_W, r, h, color, &mut els);
            }
            CellKind::Power(a) => {
                let (name, mask): (&str, &[u32]) = match a {
                    PowerAction::Off => ("pwr-off", &POWER),
                    PowerAction::Reboot => ("pwr-reboot", &REBOOT),
                    PowerAction::Sleep => ("pwr-sleep", &SLEEP),
                };
                let color = if armed == Some(a) { TRAY_WARN } else { TRAY_DIM };
                draw_mask(state, renderer, name, mask, POWER_W, r, BAR_H * 3 / 5, color, &mut els);
            }
            CellKind::VolumeSlider => {}
        }
    }

    // Подсказка под рядом: чем закончилась команда и, главное, что взведённая
    // кнопка ждёт второго клика.
    if let (Some(text), Some(panel)) = (hint, lay.panel) {
        let color = if armed.is_some() { TRAY_WARN } else { TRAY_DIM };
        let w = crate::text::width(&text, 1);
        draw_text(
            state, renderer,
            (panel.x + panel.w - w).max(0), panel.y + panel.h + BAR_H / 6,
            &text, 1, color, 0, &mut els,
        );
    }

    // Разворот по той же причине, что и в меню: раньше в списке = выше в кадре.
    els.reverse();
    els
}

#[cfg(test)]
mod tests {
    /// ОДНА копия обязана укрывать экран целиком при любой камере И любом столе.
    ///
    /// Это и есть весь смысл затеи: пока картинка не отрывается ни от одного
    /// края, шва на экране не бывает, а значит и «обои дублируются» не бывает.
    /// Проверка ровно про то место, где легко ошибиться, — что ход обоев строго
    /// меньше запаса, на который они больше экрана. Стол сюда добавлен НЕ для
    /// полноты: он входит в тот же `путь_x`, что и камера, и слагаемое,
    /// прибавленное мимо `tanh`, вытолкнуло бы картинку за край на дальнем
    /// столе — то есть вернуло бы ровно ту чёрную полосу, ради которой всё
    /// затевалось.
    #[test]
    fn обои_укрывают_экран_на_любой_камере() {
        // Разные пропорции картинки и экрана: ультраширокий, вертикальный,
        // квадратный — крой считается по большей стороне и не должен оставлять
        // дыру ни в одной комбинации.
        for картинка in [(2560, 1080), (1920, 1080), (1080, 1920), (1000, 1000)] {
            for экран in [(2560, 1080), (1920, 1280), (1080, 1920)] {
                for cam in [-1e9_f64, -9999.5, -2560.0, -1.0, 0.0, 1.0, 1234.5, 1e9] {
                    // Столы: свой, соседний, середина переезда (фаза дробная),
                    // край девятки и заведомо недостижимый — предел один на всех.
                    for стол in [0.0_f64, 0.5, 1.0, 8.0, -3.0, 1e6] {
                        let м = super::wallpaper_placement(
                            (cam, cam * 0.37), стол, картинка, экран,
                        ).expect("годные входы, а места нет");
                        // Не «>= 0», а «с запасом в пиксель»: размер элемента
                        // округляется до целого, и картинка, вставшая краем ровно
                        // в край экрана, оставила бы чёрную нитку (см.
                        // `ОБОИ_ДОЛЯ_ХОДА`).
                        assert!(м.x <= -1.0,
                            "слева впритык: x={} (камера {cam}, стол {стол}, экран {экран:?})",
                            м.x);
                        assert!(м.y <= -1.0,
                            "сверху впритык: y={} (камера {cam}, стол {стол})", м.y);
                        assert!(м.x + м.w >= экран.0 as f64 + 1.0,
                            "справа впритык: {} < {} (камера {cam}, стол {стол})",
                            м.x + м.w, экран.0);
                        assert!(м.y + м.h >= экран.1 as f64 + 1.0,
                            "снизу впритык: {} < {} (камера {cam}, стол {стол})",
                            м.y + м.h, экран.1);
                    }
                }
            }
        }

        // Обои ДВИГАЮТСЯ, а не стоят: иначе это просто приклеенный к экрану
        // слой, и правка бессмысленна. Направление — против камеры, как холст.
        let экран = (2560, 1080);
        let дома = super::wallpaper_placement((0.0, 0.0), 0.0, экран, экран).unwrap();
        let справа = super::wallpaper_placement((экран.0 as f64, 0.0), 0.0, экран, экран).unwrap();
        assert!(справа.x < дома.x - 1.0, "обои не поехали за камерой");
        // Смена стола обязана двигать обои САМА, без панорамирования: это и была
        // жалоба «обои двигаются только если панить либо зумить». Соседний стол —
        // не символические полпикселя, а заметный глазу ход.
        let сосед = super::wallpaper_placement((0.0, 0.0), 1.0, экран, экран).unwrap();
        assert!(дома.x - сосед.x > 20.0,
            "стол не сдвинул обои: {} → {}", дома.x, сосед.x);
        // Направление у стола то же, что у камеры: вправо по столам — обои влево.
        // Разъезд знаков был бы не виден в статике и читался бы как рывок назад
        // ровно в момент, когда слайд стола сменяется панорамированием.
        let назад = super::wallpaper_placement((0.0, 0.0), -1.0, экран, экран).unwrap();
        assert!(назад.x > дома.x, "стол влево не увёл обои вправо");
        // Ход ограничен запасом при любом удалении — на этом всё и держится.
        // 1e12 даёт tanh ровно 1.0, то есть предельный ход: именно здесь и
        // проверяется, что резерв `ОБОИ_ДОЛЯ_ХОДА` живой.
        let далеко = super::wallpaper_placement((1e12, 0.0), 0.0, экран, экран).unwrap();
        assert!(далеко.x <= -1.0 && далеко.x + далеко.w >= экран.0 as f64 + 1.0,
            "на пределе хода картинка встала впритык: x={}", далеко.x);

        // Вырожденные входы отбиваются, а не делят на ноль.
        assert!(super::wallpaper_placement((0.0, 0.0), 0.0, (0, 1080), экран).is_none());
        assert!(super::wallpaper_placement((0.0, 0.0), 0.0, экран, (0, 0)).is_none());
        assert!(super::wallpaper_placement((f64::NAN, 0.0), 0.0, экран, экран).is_none());
        assert!(super::wallpaper_placement((f64::INFINITY, 0.0), 0.0, экран, экран).is_none());
        // Фаза слайда — тоже вход извне (её считает `СлайдОбоев::фаза` от
        // времени), и нечисло в ней означало бы NaN в координатах элемента.
        assert!(super::wallpaper_placement((0.0, 0.0), f64::NAN, экран, экран).is_none());
    }

    use super::{BATTERY, BATTERY_W, VOLUME, VOLUME_W, battery_box, battery_box_fit, mask_h_fit,
                on_screen};
    use smithay::utils::Size;

    /// Ячейка панели: квадрат в bar::DOT по ширине во всю высоту острова.
    fn ячейка_панели() -> crate::tray::Rect {
        crate::tray::Rect { x: 100, y: crate::bar::TOP, w: crate::bar::DOT, h: crate::bar::H }
    }

    fn экран() -> Size<i32, smithay::utils::Logical> {
        Size::from((2560, 1080))
    }

    #[test]
    fn окно_в_кадре_видно() {
        assert!(on_screen(экран(), (100.0, 100.0, 800.0, 600.0), 0.0));
    }

    #[test]
    fn окно_за_краем_не_видно() {
        // Ушло влево целиком и вправо целиком.
        assert!(!on_screen(экран(), (-900.0, 100.0, 800.0, 600.0), 0.0));
        assert!(!on_screen(экран(), (2600.0, 100.0, 800.0, 600.0), 0.0));
        assert!(!on_screen(экран(), (100.0, -700.0, 800.0, 600.0), 0.0));
        assert!(!on_screen(экран(), (100.0, 1100.0, 800.0, 600.0), 0.0));
    }

    #[test]
    fn окно_наполовину_за_краем_видно() {
        assert!(on_screen(экран(), (-400.0, 100.0, 800.0, 600.0), 0.0));
        assert!(on_screen(экран(), (2400.0, 100.0, 800.0, 600.0), 0.0));
    }

    /// Тень вылезает за окно, поэтому у неё запас: окно уже за краем, а её
    /// кромку ещё видно.
    #[test]
    fn запас_под_тень_учитывается() {
        let чуть_за_краем = (-810.0, 100.0, 800.0, 600.0);
        assert!(!on_screen(экран(), чуть_за_краем, 0.0));
        assert!(on_screen(экран(), чуть_за_краем, 40.0));
    }

    /// Значок батареи лежит на боку (26×14) и от высоты бара выходит ШИРЕ
    /// отведённой ему ячейки в 20 px — вылезал на проценты рядом. Со
    /// вписыванием он влезает и стоит ровно посередине ячейки.
    #[test]
    fn значок_батареи_вписывается_в_ячейку_панели() {
        let cell = ячейка_панели();
        let (_, _, без_вписывания, _) = battery_box(cell);
        assert!(
            без_вписывания > cell.w,
            "тест бессмысленный: значок и так влезал ({} при ячейке {})",
            без_вписывания, cell.w,
        );

        let (x, y, w, h) = battery_box_fit(cell, cell.w);
        assert!(w <= cell.w, "значок шире ячейки: {w} при {}", cell.w);
        assert!(h > 0 && h <= cell.h);
        assert_eq!(x, cell.x + (cell.w - w) / 2, "значок не по центру ячейки");
        assert_eq!(y, cell.y + (cell.h - h) / 2);
        // Пропорции маски сохранены: 26 колонок на 14 рядов.
        let ожидаемая_h = w * BATTERY.len() as i32 / BATTERY_W;
        assert!(
            (h - ожидаемая_h).abs() <= 1,
            "значок растянут: {w}×{h} против пропорций {BATTERY_W}×{}", BATTERY.len(),
        );
    }

    /// В полке (там ячейка широкая) вписывание ничего не меняет — значок
    /// остаётся того же размера, что и был до правки.
    #[test]
    fn в_широкой_ячейке_значок_не_ужимается() {
        let широкая = crate::tray::Rect { x: 0, y: 0, w: 64, h: 48 };
        assert_eq!(battery_box(широкая), battery_box_fit(широкая, широкая.w));
    }

    /// Маску звука рисуют от высоты, а ширина идёт из пропорций (24×18): при
    /// половине высоты бара она выходила шире ячейки. mask_h_fit подрезает
    /// высоту ровно настолько, чтобы ширина влезла.
    #[test]
    fn маска_звука_не_шире_ячейки() {
        let ячейка = ячейка_панели();
        let rows = VOLUME.len() as i32;
        let хотелось = crate::bar::H / 2;
        let ширина = |h: i32| (h * VOLUME_W / rows).max(1);
        assert!(
            ширина(хотелось) > ячейка.w,
            "тест бессмысленный: маска и так влезала ({} при ячейке {})",
            ширина(хотелось), ячейка.w,
        );

        let h = mask_h_fit(VOLUME_W, rows, хотелось, ячейка.w);
        assert!(h <= хотелось, "маска выросла: {h} против {хотелось}");
        assert!(ширина(h) <= ячейка.w, "маска шире ячейки: {} при {}", ширина(h), ячейка.w);
        // Просторной ячейке подрезать нечего.
        assert_eq!(mask_h_fit(VOLUME_W, rows, хотелось, 64), хотелось);
    }
}
