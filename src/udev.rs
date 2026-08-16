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
                utils::RescaleRenderElement,
            },
            gles::GlesRenderer,
            utils::CommitCounter,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{UdevBackend, UdevEvent},
    },
    desktop::{Window, space::SpaceRenderElements, layer_map_for_output},
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
use crate::Dawn;

/// ВРЕМЕННАЯ ДИАГНОСТИКА (артефакты на анимированных обоях, 04.08.2026).
///
/// Включается переменной `DAWN_DEBUG_FRAME=1`. Даёт две вещи, которых иначе
/// не увидеть:
///  · строку с damage-прямоугольниками КАЖДОГО кадра (что компоситор реально
///    считает изменившимся);
///  · по появлению файла `/tmp/dawn_dump` — снимок НАСТОЯЩЕГО кадра со
///    сканаута (`blit_frame_result`) в `/tmp/dawn_frame.raw`. Обычный grim
///    сюда не годится: screencopy перерисовывает кадр с нуля свежим damage
///    tracker'ом и артефактов частичной перерисовки не показывает.
fn debug_frame_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("DAWN_DEBUG_FRAME").is_ok_and(|v| v != "0"))
}

// Курсор в dawn всегда client-side (нет server-side cursor протокола) — клиент
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
    // Layer-поверхности (обои, панели, меню dwall): обёрнуты в Rescale, чтобы
    // не масштабироваться вместе с зумом холста (см. build_layer_elements).
    Layer = RescaleRenderElement<WaylandSurfaceRenderElement<GlesRenderer>>,
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
    /// Сколько кадр ждал GPU перед page flip (см. needs_sync в render_surface).
    sync_us: u64,
    sync_max_us: u64,
}

impl RenderStats {
    pub fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            frames: 0, skipped: 0, total_us: 0, max_us: 0, max_elements: 0,
            sync_us: 0, sync_max_us: 0,
        }
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

    fn flush(&mut self) {
        if self.since.elapsed() < Duration::from_secs(1) {
            return;
        }
        let secs = self.since.elapsed().as_secs_f64();
        tracing::debug!(
            "dawn/render: {:.0} кадр/с, средний {:.1} мс, худший {:.1} мс, \
             элементов до {}, пропущено (кадр уже в очереди) {}, \
             ожидание GPU: среднее {:.2} мс, худшее {:.2} мс",
            self.frames as f64 / secs,
            self.total_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.max_us as f64 / 1000.0,
            self.max_elements,
            self.skipped,
            self.sync_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.sync_max_us as f64 / 1000.0,
        );
        *self = Self::new();
    }
}

/// Через сколько «зависший» `frame_queued` перестаёт блокировать рендер.
/// Заметно больше кадра (16.6 мс) и заметно меньше 500-мс хартбита, который
/// в самом плохом случае всё равно перезапустит цепочку.
const FRAME_QUEUE_STALE_MS: u128 = 100;

pub struct Device {
    pub drm: DrmDevice,
    pub gbm: GbmDevice<DrmDeviceFd>,
    pub gles: GlesRenderer,
    pub drm_scanner: DrmScanner,
    pub surfaces: HashMap<crtc::Handle, Surface>,
    pub render_node: DrmNode,
}

pub fn init_udev(
    event_loop: &mut EventLoop<Dawn>,
    state: &mut Dawn,
) -> Result<(), Box<dyn std::error::Error>> {

    let (session, notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("dawn/udev: seat={}", seat_name);
    state.session = Some(session.clone());

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
                tracing::info!("dawn/udev: session paused");
                state.session_active = false;
                libinput_for_notifier.suspend();
                // Отдаём DRM master — seatd передаёт его другому compositor'у
                for device in state.udev_devices.values_mut() {
                    device.drm.pause();
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("dawn/udev: session activated — acquiring DRM master");
                state.session_active = true;
                let _ = libinput_for_notifier.resume();
                // Берём DRM master обратно — теперь мы активный compositor
                let mut devices = std::mem::take(&mut state.udev_devices);
                for device in devices.values_mut() {
                 // activate(false) = не отключать коннекторы, просто взять master
                match device.drm.activate(false) {
                    Ok(()) => tracing::info!("dawn/udev: DRM master acquired"),
                    Err(e) => tracing::warn!("dawn/udev: activate failed: {:?}", e),
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
                            tracing::trace!("dawn/drm: VBlank crtc={:?}", crtc);
                            let mut devices = std::mem::take(&mut state.udev_devices);
                            if let Some(device) = devices.get_mut(&node) {
                                if let Some(surface) = device.surfaces.get_mut(&crtc) {
                                    // ОБЯЗАТЕЛЬНО: без этого compositor думает
                                    // что предыдущий frame ещё в flight
                                    match surface.compositor.frame_submitted() {
                                        Ok(_) => {}
                                        Err(e) => tracing::warn!("dawn/drm: frame_submitted: {:?}", e),
                                    }
                                    // Показанный кадр отпускает «шлагбаум»: следующий
                                    // рендер разрешён, и делает его прямо этот же
                                    // VBlank — ровно один рендер на показанный кадр.
                                    surface.frame_queued = false;
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
                                    // Кадр собираем ТОЛЬКО когда есть что
                                    // показывать.
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
                                        let gles = &mut device.gles as *mut GlesRenderer;
                                        unsafe { render_surface(surface, &mut *gles, state); }
                                    }
                                }
                            }
                            state.udev_devices = devices;
                        }
                        DrmEvent::Error(e) => tracing::warn!("dawn/drm: error: {:?}", e),
                    }
                }).unwrap();

                state.udev_devices.insert(node, device);

                // Явно пробуем стать master сразу — не ждём ActivateSession,
                // который может не прийти, если сессия уже была активна при старте
                if let Some(dev) = state.udev_devices.get_mut(&node) {
                    match dev.drm.activate(false) {
                        Ok(()) => tracing::info!("dawn/udev: DRM master acquired at startup"),
                        Err(e) => tracing::warn!("dawn/udev: initial activate failed: {:?}", e),
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
                                    .create_global_with_default_feedback::<Dawn>(
                                        &state.display_handle,
                                        &feedback,
                                    );
                                state.dmabuf_global = Some(global);
                                tracing::info!("dawn/udev: DMA-BUF global created");
                            }
                            Err(e) => {
                                tracing::warn!("dawn/udev: dmabuf feedback: {:?}", e);
                            }
                        }
                    }
                }
                let node_render = node;
                event_loop.handle().insert_idle(move |state| {
                    tracing::info!("dawn/udev: initial render (idle)");
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
            Err(e) => tracing::warn!("dawn/udev: skip {:?}: {}", path, e),
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
                        for (_, s) in dev.surfaces {
                            state.space.unmap_output(&s.output);
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
    state: &mut Dawn,
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
    // Из-за этого dawn забирал видеокарту целиком и не мог делить её с чужой
    // сессией (Xorg/другой Wayland): узел один, хозяин может быть только один.
    // Рендер-узел никакого master не требует (права crw-rw-rw-), это штатный
    // путь всех компоновщиков: KMS — на card0, GL — на renderD128.
    let render_gbm = open_render_gbm(&render_node);
    let egl = match render_gbm {
        Some(rgbm) => unsafe { EGLDisplay::new(rgbm)? },
        None => {
            tracing::warn!(
                "dawn/udev: рендер-узел недоступен, EGL на первичном узле \
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
    tracing::info!("dawn/udev: added {:?}", path);
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
        .map_err(|e| tracing::warn!("dawn/udev: не открыть рендер-узел {:?}: {}", path, e))
        .ok()?;
    let gbm = GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(file))))
        .map_err(|e| tracing::warn!("dawn/udev: GBM на рендер-узле {:?}: {}", path, e))
        .ok()?;
    tracing::info!("dawn/udev: рендер на {:?} (скан-аут на первичном узле)", path);
    Some(gbm)
}

fn scan_connectors(device: &mut Device, state: &mut Dawn) {
    let scan = match device.drm_scanner.scan_connectors(&device.drm) {
        Ok(s) => s,
        Err(e) => { tracing::warn!("dawn/udev: scan: {}", e); return; }
    };
    for event in scan {
        match event {
            DrmScanEvent::Connected { connector, crtc: Some(crtc) } => {
                if let Err(e) = add_surface(device, state, &connector, crtc) {
                    tracing::warn!("dawn/udev: add_surface: {}", e);
                }
            }
            DrmScanEvent::Disconnected { crtc: Some(crtc), .. } => {
                if let Some(s) = device.surfaces.remove(&crtc) {
                    state.space.unmap_output(&s.output);
                }
            }
            _ => {}
        }
    }
}

fn add_surface(
    device: &mut Device,
    state: &mut Dawn,
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
                        "dawn/udev: {}: режим из конфига {}x{}@{}Hz",
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
                        "dawn/udev: {}: режима {}x{} в EDID нет — синтезирую CVT {}x{}@{}Hz \
                         (панель {}x{} растянет его сама)",
                        connector_name, cfg.width, cfg.height,
                        свой.size().0, свой.size().1, свой.vrefresh(),
                        родной.size().0, родной.size().1,
                    );
                    (свой, true)
                }
                None => {
                    tracing::warn!(
                        "dawn/udev: {}: {}x{} больше физической матрицы {}x{}, беру PREFERRED",
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
                    "dawn/udev: {}: железо отвергло синтезированный {}x{} ({:?}) — \
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
    let position: smithay::utils::Point<i32, smithay::utils::Logical> =
        mon_cfg.as_ref().map(|c| (c.x, c.y)).unwrap_or((0, 0)).into();

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
            "dawn/udev: {}: масштаб выхода {} → логический стол {}×{}",
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
    let _global = output.create_global::<Dawn>(&state.display_handle);
    output.change_current_state(Some(wl_mode), Some(transform), Some(scale), Some(position));
    output.set_preferred(wl_mode);
    state.space.map_output(&output, position);

    // Отдельный выход ТОЛЬКО для layer-поверхностей (обои, панели, меню dwall).
    //
    // Зум холста у dawn сделан через output scale, а логический размер выхода
    // делится на масштаб — то есть на «птичьем глазе» (zoom 0.2) LayerMap
    // выдавал обоям размер 12800×5400 и требовал от клиента отрисовать буфер в
    // 25 раз больше экрана. dwall на этом просто ложился, обои и меню исчезали.
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
    state.layer_output = Some(layer_output);

    // Ставим курсор в центр экрана при первом output'е
    if state.pointer_location.x == 0.0 && state.pointer_location.y == 0.0 {
        state.pointer_location = smithay::utils::Point::from((
            wl_mode.size.w as f64 / 2.0,
            wl_mode.size.h as f64 / 2.0,
        ));
    }

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
    });
    tracing::info!("dawn/udev: output '{}' {}x{}@{}Hz",
        output_name, wl_mode.size.w, wl_mode.size.h, wl_mode.refresh/1000);
    Ok(())
}

/// Строит render-элементы миникарты (Module 3): фоновая панель, окна как
/// прямоугольники, рамка текущего viewport — всё в фиксированных физических
/// координатах экрана (независимо от zoom холста, как курсор).
/// Rubber-band рамка выделения (в процессе протяжки) + подсветка уже
/// выделенных окон (Super+G группирует их в "созвездие") — рисуются поверх
/// окон полупрозрачными заливками, тем же приёмом, что и Focus Aura/фон портала.
fn build_selection_elements(state: &mut Dawn) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if state.selected_windows.is_empty() && state.selection_drag.is_none() {
        return elements;
    }

    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    let zoom = state.viewport.zoom;

    // Геометрию собираем до заимствования пула: space принадлежит тому же state.
    let mut rects: Vec<((i32, i32), (i32, i32), [f32; 4])> = Vec::new();
    for window in &state.selected_windows {
        let geo = match state.space.element_geometry(window) { Some(g) => g, None => continue };
        let x = ((geo.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y = ((geo.loc.y as f64 - cam_y) * zoom).round() as i32;
        let w = ((geo.size.w as f64 * zoom).round() as i32).max(1);
        let h = ((geo.size.h as f64 * zoom).round() as i32).max(1);
        rects.push(((x, y), (w, h), [1.0, 0.7, 0.2, 0.22]));
    }

    if let Some(rect) = state.selection_drag {
        let x = ((rect.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y = ((rect.loc.y as f64 - cam_y) * zoom).round() as i32;
        let w = ((rect.size.w as f64 * zoom).round() as i32).max(1);
        let h = ((rect.size.h as f64 * zoom).round() as i32).max(1);
        rects.push(((x, y), (w, h), [0.35, 0.6, 1.0, 0.16]));
    }

    let pool = &mut state.selection_ids;
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
    state: &mut Dawn,
    output: &Output,
    renderer: &mut GlesRenderer,
    elements: &[E],
) where
    E: smithay::backend::renderer::element::RenderElement<GlesRenderer>,
{
    let Some(mode) = output.current_mode() else { return };
    let screen: Size<i32, smithay::utils::Buffer> = (mode.size.w, mode.size.h).into();
    let Some(cast) = state.portal_cast.as_ref() else { return };
    let (cw, ch) = (cast.width as i32, cast.height as i32);
    // Прямоугольник, который уйдёт в поток, в экранных пикселях.
    let crop = match &cast.source {
        crate::portal::Capture::Output => None,
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
/// Пока портал ждёт ответа, под курсором подсвечивается то, что уйдёт в поток:
/// окно — рамкой по его границам, пустой холст — рамкой по всему экрану («весь
/// экран»). Своего шрифта у dawn нет, поэтому подсказка визуальная, без текста:
/// ЛКМ — выбрать, ПКМ или Escape — отменить.
fn build_portal_pick_elements(state: &mut Dawn) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if !state.portal_picking() {
        return elements;
    }
    let mode = state.space.element_under(state.pointer_location)
        .and_then(|(w, _)| state.space.element_geometry(w));
    let (x, y, w, h) = match mode {
        Some(geo) => {
            let zoom = state.viewport.zoom;
            (
                ((geo.loc.x as f64 - state.viewport.cam_x) * zoom).round() as i32,
                ((geo.loc.y as f64 - state.viewport.cam_y) * zoom).round() as i32,
                ((geo.size.w as f64 * zoom).round() as i32).max(1),
                ((geo.size.h as f64 * zoom).round() as i32).max(1),
            )
        }
        None => {
            let s = state.screen_size();
            (0, 0, s.w, s.h)
        }
    };

    const РАМКА: i32 = 4;
    // Цвет ПРЕМУЛЬТИПЛИЦИРОВАН: рендер берёт компоненты как есть и складывает
    // их с фоном по (1 − alpha). Без домножения на альфу лёгкая заливка 0.15
    // красила весь экран в плотный голубой — обои под ней тонули.
    const ЦВЕТ: [f32; 4] = [0.30, 0.64, 0.85, 0.85];
    const ЗАЛИВКА: [f32; 4] = [0.05, 0.11, 0.15, 0.15];
    let pool = &mut state.portal_pick_ids;
    let mut idx = 0usize;
    // Четыре полосы рамки + лёгкая заливка: сплошными прямоугольниками, как
    // остальные оверлеи dawn (своего шейдера у нас нет).
    elements.push(pooled_solid(pool, &mut idx, (x, y), (w, РАМКА), ЦВЕТ));
    elements.push(pooled_solid(pool, &mut idx, (x, y + h - РАМКА), (w, РАМКА), ЦВЕТ));
    elements.push(pooled_solid(pool, &mut idx, (x, y), (РАМКА, h), ЦВЕТ));
    elements.push(pooled_solid(pool, &mut idx, (x + w - РАМКА, y), (РАМКА, h), ЦВЕТ));
    elements.push(pooled_solid(pool, &mut idx, (x, y), (w, h), ЗАЛИВКА));
    elements
}

/// Цифра 3×5 «пикселей» из сплошных прямоугольников, увеличенная в PX раз.
/// Своего шрифта у dawn нет, а подпись нужна крошечная — этого хватает.
fn draw_digit(
    pool: &mut Vec<SolidSlot>, idx: &mut usize,
    x: i32, y: i32, digit: u32, color: [f32; 4],
    out: &mut Vec<OutputRenderElements>,
) {
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
                out.push(pooled_solid(
                    pool, idx,
                    (x + col * PX, y + row as i32 * PX),
                    (PX, PX), color,
                ));
            }
        }
    }
}

fn build_minimap_elements(state: &mut Dawn, output: &Output) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    let mode = match output.current_mode() { Some(m) => m, None => return elements };

    let current_tags = state.viewport.current_tags();
    let focused = state.focused_surface();
    let windows: Vec<(smithay::utils::Point<i32, smithay::utils::Logical>, smithay::utils::Size<i32, smithay::utils::Logical>, bool)> =
        state.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .filter_map(|tw| state.space.element_geometry(&tw.window).map(|g| {
                let is_focused = focused.as_ref()
                    .map(|fs| crate::xwin::is_surface(&tw.window, fs))
                    .unwrap_or(false);
                (g.loc, g.size, is_focused)
            }))
            .collect();

    // Закладки обязаны попасть в кадр миникарты — иначе поставленная в
    // стороне от окон точка не рисуется вовсе.
    let якоря: Vec<_> = state.camera_bookmarks.values().copied().collect();
    let proj = crate::canvas::project_minimap_with(&windows, &якоря);
    let origin = crate::canvas::minimap_panel_origin(mode.size);

    // Закладки камеры читаем до заимствования пула — обе части лежат в state.
    // Берём ПАРАМИ со слотом: номер рисуется рядом с точкой, чтобы было видно,
    // какая цифра куда прыгает (Super+N в режиме закладок).
    let mut bookmarks: Vec<(u32, Point<f64, Logical>)> =
        state.camera_bookmarks.iter().map(|(s, p)| (*s, *p)).collect();
    bookmarks.sort_by_key(|(s, _)| *s);
    let pool = &mut state.minimap_ids;
    let mut idx = 0usize;

    elements.push(pooled_solid(
        pool, &mut idx, (origin.x, origin.y),
        (crate::canvas::MINIMAP_PANEL_W, crate::canvas::MINIMAP_PANEL_H),
        [0.05, 0.05, 0.08, 0.75],
    ));

    // ── Закладки камеры (bookmarks_mode): крестик на минимапе за каждую точку ─
    // Проецируем якорь закладки тем же bbox/scale, что и окна; рисуем крест из
    // двух перекладин. Точки вне панели (сильный зум/далеко) пропускаем.
    const CROSS_ARM: i32 = 5; // длина луча от центра, px
    const CROSS_TH: i32 = 2;  // толщина перекладины, px
    for (slot, anchor) in bookmarks {
        let p = crate::canvas::project_point_minimap(anchor, proj.bbox, proj.scale);
        if p.x < 0 || p.y < 0
            || p.x >= crate::canvas::MINIMAP_PANEL_W
            || p.y >= crate::canvas::MINIMAP_PANEL_H
        {
            continue;
        }
        let color = [1.0f32, 0.30, 0.45, 0.95];
        // горизонтальная перекладина
        elements.push(pooled_solid(
            pool, &mut idx,
            (origin.x + p.x - CROSS_ARM, origin.y + p.y - CROSS_TH / 2),
            (CROSS_ARM * 2 + 1, CROSS_TH), color,
        ));
        // вертикальная перекладина
        elements.push(pooled_solid(
            pool, &mut idx,
            (origin.x + p.x - CROSS_TH / 2, origin.y + p.y - CROSS_ARM),
            (CROSS_TH, CROSS_ARM * 2 + 1), color,
        ));
        // Номер слота — справа сверху от крестика, тем же цветом.
        draw_digit(
            pool, &mut idx,
            origin.x + p.x + CROSS_ARM + 2,
            origin.y + p.y - CROSS_ARM - 1,
            slot,
            color,
            &mut elements,
        );
    }

    // Прямоугольники окон рисуем ПОСЛЕ закладок: список кадра идёт от
    // переднего плана к заднему, поэтому то, что добавлено позже, лежит ниже.
    // Раньше закладки шли последними и тонули под полупрозрачными окнами —
    // крестики с номерами были едва различимы.
    for b in &proj.boxes {
        let color: [f32; 4] = if b.focused { [0.35, 0.55, 0.95, 0.9] } else { [0.6, 0.6, 0.65, 0.75] };
        elements.push(pooled_solid(
            pool, &mut idx,
            (origin.x + b.loc.x, origin.y + b.loc.y),
            (b.size.w, b.size.h), color,
        ));
    }

    // Рамку текущего viewport (жёлтый прямоугольник) НЕ рисуем: при отдалении
    // камеры она разрасталась в "жёлтый квадрат" вокруг панели и делала
    // миникарту дёрганой. Миникарта теперь статична — просто мини-рисунок
    // всего холста (окна + крестики закладок), без индикатора камеры.


    elements
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
    state: &mut Dawn,
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
            Err(e) => tracing::warn!("dawn/udev: параллакс: {:?}", e),
        }
        slot += 1;
        y += PARALLAX_SPACING_PX;
    }
    out
}

/// Цвет "clear" компоситора (см. render_frame ниже) — маски углов красятся
/// этим цветом, чтобы совпадать с тем, что реально видно под окном в
/// подавляющем большинстве случаев (пустой холст).
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
fn pooled_solid(
    pool: &mut Vec<SolidSlot>,
    idx: &mut usize,
    loc: (i32, i32),
    size: (i32, i32),
    color: [f32; 4],
) -> OutputRenderElements {
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
fn build_tab_indicators(state: &mut Dawn) -> Vec<OutputRenderElements> {
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
fn build_insert_hint(state: &mut Dawn) -> Vec<OutputRenderElements> {
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
fn corner_radius_logical(state: &Dawn) -> i32 {
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
/// Холст в dawn бесконечен, а декорации (тени, маски углов) строились для ВСЕХ
/// окон текущих тегов — включая те, что стоят в тысячах пикселей от камеры.
/// Каждое такое окно — это 11 элементов тени плюс 4 маски углов, которые
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

fn window_screen_rect(state: &Dawn, window: &Window) -> Option<(f64, f64, f64, f64)> {
    let geo = state.space.element_geometry(window)?;
    let zoom = state.viewport.zoom;
    Some((
        (geo.loc.x as f64 - state.viewport.cam_x) * zoom,
        (geo.loc.y as f64 - state.viewport.cam_y) * zoom,
        geo.size.w as f64 * zoom,
        geo.size.h as f64 * zoom,
    ))
}

/// Маски скруглённых углов — четыре плитки на окно вместо 4×радиус полосок.
/// Размер плитки НЕ задаём: буфер сделан в логических пикселях, и рендер сам
/// умножит его на масштаб выхода (у нас это zoom) — ровно так же, как окна,
/// поэтому маска не разъезжается с углом при любом зуме.
fn build_corner_mask_elements(
    state: &mut Dawn,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if state.tagged_windows.is_empty() {
        return elements;
    }
    let _ = output;
    let zoom = state.viewport.zoom;
    let screen = state.screen_size();
    let radius = corner_radius_logical(state);
    state.decor.ensure(radius, [CLEAR_COLOR[0], CLEAR_COLOR[1], CLEAR_COLOR[2]]);
    let r_px = state.decor.mask_px() as f64 * zoom;

    let windows: Vec<Window> = state.tagged_windows.iter().map(|tw| tw.window.clone()).collect();
    let mut slot = 0usize;
    for window in windows {
        // Развёрнутое на весь экран окно не скругляем: у края монитора
        // скругление выглядит рамкой вокруг «полного экрана».
        if state.is_fullscreen(&window) {
            continue;
        }
        let Some((x0, y0, w, h)) = window_screen_rect(state, &window) else { continue };
        if w < r_px * 2.0 || h < r_px * 2.0 {
            continue; // окно слишком маленькое для радиуса — не портим его совсем
        }
        // Окно за краем экрана — его углов не видно (см. on_screen).
        if !on_screen(screen, (x0, y0, w, h), 0.0) {
            continue;
        }
        let corners = [
            (crate::decor::TL, x0, y0),
            (crate::decor::TR, x0 + w - r_px, y0),
            (crate::decor::BL, x0, y0 + h - r_px),
            (crate::decor::BR, x0 + w - r_px, y0 + h - r_px),
        ];
        // Размер задаём ЯВНО. Плитка сделана в логических пикселях, и раньше её
        // домножал на зум сам рендер (масштаб выхода был равен зуму). Теперь
        // масштаб выхода всегда 1 (зум живёт в отрисовке окон), и без явного
        // размера маска рисовалась бы во всю величину зума 1 поверх маленького
        // окна — те самые «фантомные» пятна по углам.
        let dst_mask = Size::<i32, Logical>::from((
            (r_px.round() as i32).max(1), (r_px.round() as i32).max(1),
        ));
        for (corner, x, y) in corners {
            let buf = state.decor.mask_corner(corner, slot);
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer, Point::<f64, Physical>::from((x, y)), buf,
                None, None, Some(dst_mask), Kind::Unspecified,
            ) {
                Ok(el) => elements.push(OutputRenderElements::Memory(el)),
                Err(e) => tracing::warn!("dawn/udev: маска угла: {:?}", e),
            }
        }
        slot += 1;
    }

    elements
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
    state: &mut Dawn,
    renderer: &mut GlesRenderer,
) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if state.tagged_windows.is_empty() {
        return els;
    }
    let zoom = state.viewport.zoom;
    let screen = state.screen_size();
    let radius = corner_radius_logical(state);
    state.decor.ensure(radius, [CLEAR_COLOR[0], CLEAR_COLOR[1], CLEAR_COLOR[2]]);

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
                Err(e) => tracing::warn!("dawn/udev: угол тени: {:?}", e),
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
                Err(e) => tracing::warn!("dawn/udev: кромка тени: {:?}", e),
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
fn build_overview_bg_elements(state: &mut Dawn) -> Vec<OutputRenderElements> {
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

// Геометрия панели столов. Лежит на уровне модуля, потому что от неё считают
// себя ещё двое: значок блютуза рядом (build_bluetooth_indicator) и полоса,
// которую под панель резервирует раскладка (tiling::BAR_RESERVED). Раньше эти
// же числа стояли в каждом месте своим литералом — панель уезжала от значка.
/// Высота бара.
pub const BAR_H: i32 = 34;
/// Отступ бара от верхнего края экрана.
pub const BAR_TOP: i32 = 8;
const BAR_RADIUS: i32 = 12;        // скругление фона бара
const DOT: i32 = 20;
const DOT_RADIUS: i32 = 10;        // скругление точек = круги
const GAP: i32 = 6;
const PAD_H: i32 = 14;
const PAD_V: i32 = (BAR_H - DOT) / 2;
/// Ширина бара; от неё полка состояния отсчитывает свою левую границу.
pub const BAR_W: i32 = 2 * PAD_H + 9 * DOT + 8 * GAP;
/// Фон бара и значка блютуза — тёмный полупрозрачный.
const BAR_BG: [f32; 4] = [0.04, 0.04, 0.07, 0.65];

/// Куда пришёлся клик по панели столов.
pub enum BarHit {
    /// Столбик стола: маска тега.
    Tag(u32),
    /// По панели, но мимо столов.
    Background,
}

/// Попадание клика в панель столов; координаты — физические пиксели экрана.
///
/// Геометрия считается ТОЙ ЖЕ арифметикой, что и отрисовка ниже, из тех же
/// констант. Второй копии чисел здесь нет намеренно — по той же причине, что
/// и у полки состояния: разъехавшись, они дают «на экране одно, а в проверке
/// другое».
pub fn bar_hit(screen_w: i32, x: f64, y: f64) -> Option<BarHit> {
    let ox = (screen_w - BAR_W) / 2;
    let oy = BAR_TOP;

    let внутри = x >= ox as f64
        && x < (ox + BAR_W) as f64
        && y >= oy as f64
        && y < (oy + BAR_H) as f64;
    if !внутри {
        return None;
    }

    for i in 0..9i32 {
        let cx = ox + PAD_H + i * (DOT + GAP);
        // Столбик шире самой точки: она всего 20 пикселей, и целиться в неё
        // мышью неудобно. Берём точку вместе с половинами зазоров по бокам и
        // всю высоту панели — промах по вертикали внутри бара всё равно
        // означает «этот стол».
        let left = (cx - GAP / 2) as f64;
        let right = (cx + DOT + GAP / 2) as f64;
        if x >= left && x < right {
            return Some(BarHit::Tag(1u32 << i));
        }
    }

    Some(BarHit::Background)
}

impl crate::state::Dawn {
    /// Клик по панели столов: столбик — перейти на этот стол.
    ///
    /// `true` = клик съеден. Мимо панели — `false`: под ней окно, и оно должно
    /// получить свой клик (так же ведёт себя полка состояния).
    pub fn bar_click(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Physical>) -> bool {
        // Под полноэкранным окном панели на экране нет — значит нет и кликов
        // по ней. Условие ровно то же, что у отрисовки.
        if self.fullscreen_here() {
            return false;
        }

        match bar_hit(self.screen_size().w, pos.x, pos.y) {
            Some(BarHit::Tag(mask)) => {
                // Переход делаем не своим кодом, а тем же действием, что и
                // Super+цифра: у него своя логика для ленты, обзора и закладок
                // камеры, и вторая её копия здесь разъехалась бы с первой.
                if mask != self.viewport.current_tags() {
                    self.dispatch_action(crate::config::Action::ViewTag(mask));
                }
                true
            }
            Some(BarHit::Background) => true,
            None => false,
        }
    }
}

/// Панель рабочих столов — скруглённый бар сверху по центру.
/// Круглые точки = новые (непосещённые) столы. Белые иконки для посещённых:
/// круг=Tile, две колонки=Columns (niri), две тильды=Float, рамка=Monocle.
fn build_workspace_bar_elements(state: &mut Dawn, output: &Output) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    // Про полноэкранное окно проверка ОДНА и стоит на месте вызова
    // (`!fullscreen_here()`), а считается она по текущему столу. Здесь раньше
    // стояла вторая, по «фуллскрин существует вообще», и она молча отменяла
    // первую: развернув окно на одном столе и уйдя на другой, человек оставался
    // без панели везде и навсегда — вернуть её можно было только F11 обратно.
    // Ровно это и было в логе 05.08.2026: F11, потом Super+2, Super+1 — и до
    // конца сеанса ни панели, ни полки.
    let mode = match output.current_mode() { Some(m) => m, None => return els };

    let ox = (mode.size.w - BAR_W) / 2;
    let oy = BAR_TOP;

    let pool = &mut state.bar_ids;
    let mut idx = 0usize;

    // Фон бара — тёмный полупрозрачный скруглённый прямоугольник.
    rounded_solid(pool, &mut idx, ox, oy, BAR_W, BAR_H, BAR_RADIUS, BAR_BG, &mut els);

    let current_tags = state.viewport.current_tags();

    for i in 0..9u32 {
        let tag = 1u32 << i;
        let cx = ox + PAD_H + i as i32 * (DOT + GAP);

        // Определяем layout стола
        let layout = state.tag_layouts.get(&tag).copied();

        if layout.is_none() && tag != current_tags && !state.visited_tags.contains(&tag) {
            // Новый, ещё не посещённый стол — серая круглая точка.
            const DOT_GRAY: [f32; 4] = [0.5, 0.5, 0.55, 0.6];
            rounded_solid(pool, &mut idx, cx, oy + PAD_V, DOT, DOT, DOT_RADIUS, DOT_GRAY, &mut els);
        } else {
            let act = tag == current_tags;
            let base_color: [f32; 4] = if act {
                [1.0, 1.0, 1.0, 0.95]
            } else {
                [1.0, 1.0, 1.0, 0.40]
            };

            // Иконка = САМА ФИГУРА, без круглой подложки. Раньше под каждую
            // фигуру рисовался сплошной круг ТЕМ ЖЕ ЦВЕТОМ, а фигура ложилась
            // поверх — она сливалась с подложкой, и все столы выглядели
            // одинаковыми кружками, по которым режим не читался вообще.
            let y = oy + PAD_V;
            match layout.unwrap_or(crate::tiling::Layout::Tile) {
                // Тайлинг — круг.
                crate::tiling::Layout::Tile => {
                    rounded_solid(pool, &mut idx, cx, y, DOT, DOT, DOT_RADIUS, base_color, &mut els);
                }
                // niri — две колонки рядом, во всю высоту.
                crate::tiling::Layout::Columns => {
                    let cw = (DOT * 2 / 5).max(2);          // 8 из 20
                    let gap = DOT - 2 * cw;                 // 4 между ними
                    let r = (cw / 2).max(1);
                    rounded_solid(pool, &mut idx, cx, y, cw, DOT, r, base_color, &mut els);
                    rounded_solid(pool, &mut idx, cx + cw + gap, y, cw, DOT, r, base_color, &mut els);
                }
                // Float — две горизонтальные тильды одна под другой.
                crate::tiling::Layout::Float => {
                    let th = (DOT / 4).max(2);              // 5 из 20
                    let gap = (DOT / 5).max(2);             // 4 между ними
                    let r = (th / 2).max(1);
                    let top = y + (DOT - (2 * th + gap)) / 2;
                    rounded_solid(pool, &mut idx, cx, top, DOT, th, r, base_color, &mut els);
                    rounded_solid(pool, &mut idx, cx, top + th + gap, DOT, th, r, base_color, &mut els);
                }
                // Monocle — одно окно на весь стол: сплошной скруглённый квадрат.
                crate::tiling::Layout::Monocle => {
                    let r = (DOT / 5).max(1);
                    rounded_solid(pool, &mut idx, cx, y, DOT, DOT, r, base_color, &mut els);
                }
            }
        }
    }

    els
}

/// Закрыт ли экран целиком фоновой layer-поверхностью (обои dwall).
///
/// Если да, то параллакс-сетка под ней невидима, и строить её незачем: это
/// ~8 элементов на КАЖДЫЙ кадр, которые damage tracker потом ещё и сравнивает
/// с прошлым кадром. Обои — самый обычный случай, так что экономия постоянная.
fn background_covers_output(state: &Dawn, output: &Output, screen: Size<i32, Logical>) -> bool {
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
fn build_layer_elements(
    _state: &mut Dawn,
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
        // Геометрия слоёв ЛОГИЧЕСКАЯ, а логический размер выхода у dawn
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

/// ВРЕМЕННАЯ ДИАГНОСТИКА: выкладывает в `/tmp/dawn_frame.raw` содержимое того
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
        Err(e) => { tracing::warn!("dawn/dbg: create_buffer: {:?}", e); return }
    };
    let mut fb = match renderer.bind(&mut target) {
        Ok(fb) => fb,
        Err(e) => { tracing::warn!("dawn/dbg: bind: {:?}", e); return }
    };
    if let Err(e) = res.blit_frame_result(
        size, Transform::Normal, 1.0, renderer, &mut fb,
        [Rectangle::from_size(size)], [],
    ) {
        tracing::warn!("dawn/dbg: blit_frame_result: {:?}", e);
        return;
    }
    let mapping = match renderer.copy_framebuffer(&fb, Rectangle::from_size(bsize), Fourcc::Abgr8888) {
        Ok(m) => m,
        Err(e) => { tracing::warn!("dawn/dbg: copy_framebuffer: {:?}", e); return }
    };
    drop(fb);
    match renderer.map_texture(&mapping) {
        Ok(data) => {
            let _ = std::fs::write(format!("/tmp/dawn_frame_{:02}.raw", idx), data);
            tracing::debug!("dawn/dbg: снимок сканаута #{} {}x{} записан", idx, size.w, size.h);
        }
        Err(e) => tracing::warn!("dawn/dbg: map_texture: {:?}", e),
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
/// Запасной выход без пересборки: `DAWN_NO_PLANES=1` возвращает прежнее
/// поведение.
fn flags_кадра() -> FrameFlags {
    static ФЛАГИ: std::sync::OnceLock<FrameFlags> = std::sync::OnceLock::new();
    *ФЛАГИ.get_or_init(|| {
        if std::env::var_os("DAWN_NO_PLANES").is_some() {
            tracing::info!("dawn/udev: аппаратные слои выключены (DAWN_NO_PLANES)");
            FrameFlags::empty()
        } else {
            FrameFlags::DEFAULT
        }
    })
}

pub fn render_surface(surface: &mut Surface, renderer: &mut GlesRenderer, state: &mut Dawn) {
    // Курсор сводим с камерой ЗДЕСЬ — в самой нижней точке, через которую
    // проходят ВСЕ пути отрисовки. Раньше вызов стоял в render_all, но
    // VBlank-хендлер (см. init_udev) зовёт anim::tick и render_surface напрямую,
    // мимо него — то есть каждый кадр анимации (зум, обзор, перелёт, инерция)
    // рисовался с курсором от предыдущего положения камеры. Стрелку тащило
    // вместе с холстом, а главный цикл возвращал её назад уже после показа:
    // в логе это «СИНХ КУРСОР ... снос=(-30.0,-38.0)» — 30-38 px за один
    // отрисованный кадр.
    state.sync_pointer_to_camera();

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
        tracing::debug!("dawn/udev: frame_queued завис на {:?}, рисуем принудительно",
            surface.frame_queued_at.elapsed());
        surface.frame_queued = false;
    }

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
    let mut elements: Vec<OutputRenderElements> =
        Vec::with_capacity(surface.last_elements.max(64));

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
            let anchor = state.space.output_geometry(&surface.output).map(|g| g.loc);
            tracing::debug!(
                "КАДР: курсор_экран=({},{}) привязка_окон={:?} камера=({:.1},{:.1}) zoom={:.2}",
                cursor_pos_physical.x, cursor_pos_physical.y, anchor, cam.0, cam.1,
                state.viewport.zoom,
            );
        }
        state.render_cam_logged = cam;
    }

    // Клонируем статус: ветка Named дочитывает тему через &mut state
    // (cursor_for_icon кэширует прочитанное), а match по &state.cursor_status
    // держал бы state занятым. Клон — это Arc у WlSurface либо Copy-енум.
    match state.cursor_status.clone() {
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
            // wp_cursor_shape_v1 (см. Dawn::new) уводит все такие «просто дай
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
            // каждые 20 МБ лога, а лог за сеанс вырос до 775 МБ. RUST_LOG у dawn
            // штатно стоит в debug, поэтому уровня мало — включаем только по
            // DAWN_DEBUG_FRAME, вместе с остальной покадровой диагностикой.
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
                    Err(e) => tracing::warn!("dawn/udev: cursor render element: {:?}", e),
                }
            }
        }
        CursorImageStatus::Hidden => {}
    }

    // Всё, что добавлено выше, рисует курсор — screencopy отбрасывает ровно эти
    // элементы, когда сессия просит кадр без курсора (см. serve_pending).
    let cursor_elements = elements.len();

    // ── Overlay-слой (wlr-layer-shell): выше всего, ниже курсора ──────────────
    elements.extend(build_layer_elements(state, renderer, &surface.output, &[WlrLayer::Overlay]));

    // ── Панель рабочих столов (поверх окон, под курсором) ──────────────────
    // Под полноэкранным окном (F11, игра, видео) панель убирается: «на весь
    // экран» значит на весь экран. Считается ПО ТЕКУЩЕМУ СТОЛУ: фуллскрин на
    // соседнем столе панель здесь не трогает. Так же ведёт себя миникарта ниже.
    if !state.fullscreen_here() {
        elements.extend(build_workspace_bar_elements(state, &surface.output));
    }

    // ── Блютуз: меню поверх всего интерфейса, значок — рядом с панелью ───────
    // Меню выше панели и миникарты намеренно: пока оно открыто, оно и есть
    // главное на экране, и клавиши принадлежат ему (см. input.rs).
    elements.extend(build_bluetooth_elements(state, renderer, &surface.output.clone()));
    elements.extend(build_search_elements(state, renderer, &surface.output.clone()));
    elements.extend(build_wifi_elements(state, renderer, &surface.output.clone()));
    elements.extend(build_audio_elements(state, renderer, &surface.output.clone()));
    elements.extend(build_tray_elements(state, renderer, &surface.output.clone()));

    // ── Миникарта (3.1, поверх окон, под курсором) ───────────────────────────
    // Не показываем во время обзора столов (перекрывает ленту).
    if state.is_minimap_visible && !state.overview_active && !state.fullscreen_here() {
        elements.extend(build_minimap_elements(state, &surface.output));
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
    // Безопасный способ без кастомного шейдера (риск сломать рендер контента,
    // как уже было): маленькие непрозрачные "маски" цвета фона поверх каждого
    // угла окна, по строкам, ширина строки — из уравнения окружности. Красят
    // всегда цветом clear color компоситора, а не тем, что реально позади —
    // упрощение, приемлемое пока под углом обычно просто холст/фон.
    elements.extend(build_corner_mask_elements(state, renderer, &surface.output));

    // ── Полоски вкладок и подсказка вставки (только Columns/niri) ────────────
    elements.extend(build_tab_indicators(state));
    elements.extend(build_insert_hint(state));

    // ── Мультивыделение (rubber-band + подсветка "созвездий") ───────────────
    elements.extend(build_selection_elements(state));

    // ── Выбор источника для демонстрации экрана (портал) ─────────────────────
    // Выше окон и выделения: пока идёт выбор, он и есть главное на экране.
    elements.extend(build_portal_pick_elements(state));

    // ── Top-слой (wlr-layer-shell): поверх окон, под UI -----------------------
    elements.extend(build_layer_elements(state, renderer, &surface.output, &[WlrLayer::Top]));

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
        for (window, loc) in окна {
            let экран = Point::<f64, Logical>::from((loc.x as f64 - cam.x, loc.y as f64 - cam.y));
            let phys: Point<i32, Physical> = (экран.x.round() as i32, экран.y.round() as i32).into();
            let els: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
                renderer, phys, smithay::utils::Scale::from(1.0), 1.0f32,
            );
            elements.extend(els.into_iter().map(|el| {
                OutputRenderElements::Layer(
                    RescaleRenderElement::from_element(el, (0, 0).into(), zoom),
                )
            }));
        }
    }

    // ── Bottom-слой (wlr-layer-shell): под окнами, над фоном ──────────────────
    elements.extend(build_layer_elements(state, renderer, &surface.output, &[WlrLayer::Bottom]));

    // ── Тени окон (полупрозрачные, скруглённые), сразу ПОЗАДИ окон ──────────
    // В обзоре тоже рисуем: раньше их там отключали из-за цены (225 элементов
    // на окно), но после перехода на плитки это 11 элементов, и в обзоре тень
    // как раз нужна — она отделяет окна от фона стола.
    elements.extend(build_shadow_elements(state, renderer));

    // Focus Aura (голубое свечение) УБРАНА — её принимали за "голубую тень".
    // Глубину теперь даёт нейтральная мягкая build_shadow_elements выше.

    // ── Фон рабочих столов в обзоре (только при тапе Super), позади окон ────
    elements.extend(build_overview_bg_elements(state));

    // ── Background-слой (wlr-layer-shell) и за ним параллакс ───────────────────
    //
    // Порядок важен: список идёт ОТ ПЕРЕДНЕГО ПЛАНА К ЗАДНЕМУ, и раньше
    // параллакс добавлялся ПЕРЕД фоновым слоем — то есть его точки лежали
    // ПОВЕРХ обоев и просвечивали сквозь любую картинку. Теперь обои идут
    // первыми, а сетка точек — за ними: без обоев она видна как прежде, с
    // обоями честно скрыта под ними.
    elements.extend(build_layer_elements(state, renderer, &surface.output, &[WlrLayer::Background]));
    if let Some(mode) = surface.output.current_mode() {
        // Под обоями во весь экран сетку не строим — её всё равно не видно.
        if !background_covers_output(state, &surface.output, state.screen_size()) {
            elements.extend(build_parallax_elements(state, renderer, mode));
        }
    }

    let element_count = elements.len();
    surface.last_elements = element_count;
    let output_name = surface.output.name();
    match surface.compositor.render_frame(
        renderer, &elements, [0.1f32, 0.1, 0.1, 1.0], flags_кадра()
    ) {
        Ok(res) => {
            // trace!, а не debug!: это две строки на КАЖДЫЙ кадр (при 60 Гц —
            // ~50 КБ/с), а лог из launch_tty.zsh идёт через tee синхронной
            // записью на диск прямо из потока рендера, который у dawn один.
            tracing::trace!("dawn/udev: render_frame[{}]: is_empty={}", output_name, res.is_empty);

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
                                "dawn/dbg: damage[{}]: n={} площадь={} needs_sync={} {:?}",
                                output_name, rects.len(), area, res.needs_sync(),
                                rects.iter().take(6).collect::<Vec<_>>(),
                            );
                        }
                        None => tracing::debug!("dawn/dbg: damage[{}]: пусто", output_name),
                    }
                }
                // Флаг взводит ПАЧКУ снимков подряд: одиночный кадр не поймает
                // мерцание, а артефакт может жить один кадр из десяти.
                static BURST: std::sync::atomic::AtomicUsize =
                    std::sync::atomic::AtomicUsize::new(0);
                use std::sync::atomic::Ordering;
                if std::path::Path::new("/tmp/dawn_dump").exists() {
                    let _ = std::fs::remove_file("/tmp/dawn_dump");
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
            // smithay. В dawn этого шага не было: `queue_frame` ставил буфер на
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
                    tracing::trace!("dawn/udev: queue_frame[{}]: committed", output_name);
                }
                // EmptyFrame — на экране ничего не изменилось, VBlank НЕ придёт;
                // шлагбаум обязан остаться открытым, иначе следующее изменение
                // упрётся в него и будет ждать страховочные 100 мс.
                Err(FrameError::EmptyFrame) => tracing::trace!("dawn/udev: queue_frame[{}]: EmptyFrame", output_name),
                Err(e) => tracing::warn!("dawn/udev: queue_frame[{}]: {:?}", output_name, e),
            }
        }
        Err(e) => tracing::warn!("dawn/udev: render_frame[{}]: {:?}", output_name, e),
    }

    state.render_stats.record(render_started.elapsed().as_micros() as u64, element_count);

    // ── Захват экрана ────────────────────────────────────────────────────────
    // Строго ПОСЛЕ render_frame и теми же элементами: демонстрация экрана
    // обязана показывать ровно то, что ушло на монитор. См. screencopy.rs.
    crate::screencopy::serve_pending(
        state, &surface.output.clone(), renderer, &elements, cursor_elements,
    );

    // ── Демонстрация экрана: кадр в PipeWire ─────────────────────────────────
    // Тем же снимком, что и screencopy, и строго после кадра на монитор.
    // Частоту держит сам Cast (30 fps): гнать 60 кадров по 11 МБ незачем —
    // Discord всё равно перекодирует поток.
    if state.portal_cast.as_ref().is_some_and(|c| c.due()) {
        push_cast_frame(state, &surface.output.clone(), renderer, &elements);
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
        // dwall тактуется кадровыми callback'ами, без них он засыпает, а за ним
        // встаёт и ffmpeg (см. Dawn::wallpaper_hidden). Запрос на callback при
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
pub fn render_all(state: &mut Dawn) {
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
/// символов, при BT_TEXT=2 (7 px на глиф, см. text::GLYPH_W) — 812 px, то
/// есть шире всей панели. Хвост уезжал за край и обрезался. Теперь подвал
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
fn draw_text(
    state: &mut Dawn,
    renderer: &mut GlesRenderer,
    x: i32, y: i32,
    text: &str,
    scale: i32,
    color: [f32; 4],
    slot: usize,
    out: &mut Vec<OutputRenderElements>,
) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let (buf, w, h) = state.text_cache.buffer(text, scale, color, slot);
    match MemoryRenderBufferRenderElement::from_buffer(
        renderer, Point::<f64, Physical>::from((x as f64, y as f64)), buf,
        None, None, Some(Size::<i32, Logical>::from((w, h))), Kind::Unspecified,
    ) {
        Ok(el) => out.push(OutputRenderElements::Memory(el)),
        Err(e) => tracing::warn!("dawn/udev: строка текста: {:?}", e),
    }
    w
}

/// Меню блютуза: список устройств, состояние адаптера и подсказка по клавишам.
///
/// Приклеено к ЭКРАНУ, как панель столов: камера и зум на него не влияют, и
/// хит-тест (см. bluetooth.rs::bt_click) считает в тех же экранных пикселях.
fn build_bluetooth_elements(
    state: &mut Dawn,
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
    let text_h = crate::text::GLYPH_H * BT_TEXT;
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
    rounded_solid(&mut pool, &mut idx, x, y, BT_MENU_W, menu_h, 16, BG, &mut els);

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
    draw_text(state, renderer, x + 44, y + 12, &head, BT_TEXT, WHITE, slot, &mut els);
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
        let kw = draw_text(state, renderer, bx + BT_BTN_PAD, ty, key, BT_TEXT, key_color, slot, &mut els);
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

/// Каркас меню. Возвращает элементы кадра и прямоугольники строк — по ним
/// хит-тест ловит клики (порядок совпадает с `rows`).
#[allow(clippy::too_many_arguments)]
fn build_list_menu(
    state: &mut Dawn,
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
    rounded_solid(&mut pool, &mut idx, x, y, MENU_W, menu_h, 16, BG, &mut els);
    state.menu_ids = pool;

    let mut slot = 0usize;
    draw_text(state, renderer, x + 22, y + 12, title, MENU_TEXT, WHITE, slot, &mut els);
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

/// Меню вайфая: список сетей, ввод пароля и подсказка по клавишам.
fn build_wifi_elements(
    state: &mut Dawn,
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
    state: &mut Dawn,
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
    state: &mut Dawn,
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
                right.push_str(&format!("стол {}", h.tags.trailing_zeros() + 1));
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
    let title = format!("ПОИСК ОКНА: {query}_");
    // Стрелок ↑↓ в битмап-шрифте dawn нет — на экране вместо них выходили «??»
    // (проверено снимком 05.08.2026). Пишем словами.
    let foot = if rows.is_empty() && !query.is_empty() {
        ("ничего не нашлось  -  Esc отмена".to_string(), DIM)
    } else {
        ("Enter перейти  Tab выбор  Esc отмена".to_string(), ACCENT)
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
    state: &mut Dawn,
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
        Err(e) => tracing::warn!("dawn/udev: значок полки {}: {:?}", name, e),
    }
}

/// Высота значка батареи и его рамка внутри ячейки. Нужна дважды — обводке
/// (маска) и заливке (прямоугольник), поэтому считается одним местом.
fn battery_box(cell: crate::tray::Rect) -> (i32, i32, i32, i32) {
    let h = (BAR_H * 7 / 16).max(6);
    let w = h * BATTERY_W / BATTERY.len() as i32;
    (cell.x + (cell.w - w) / 2, cell.y + (cell.h - h) / 2, w, h)
}

/// Полка состояния справа от панели столов: вертикальная полосочка, а по клику
/// из неё выезжает ряд — блютуз, вайфай, звук, батарея, питание.
///
/// Раскладку ячеек даёт `tray::layout`, и ТА ЖЕ функция считает попадания
/// клика (см. tray.rs::tray_click). Второй копии геометрии в хит-тесте нет
/// намеренно: именно так однажды разъехались клики по окнам — на экране одно,
/// в проверке другое.
fn build_tray_elements(
    state: &mut Dawn,
    renderer: &mut GlesRenderer,
    output: &Output,
) -> Vec<OutputRenderElements> {
    use crate::tray::{CellKind, PowerAction};

    let mut els = Vec::new();
    // По ТЕКУЩЕМУ столу, как и панель рядом: полноэкранная игра на соседнем
    // столе не повод оставлять этот стол без полки (см.
    // build_workspace_bar_elements — там на этом же месте была та же ошибка).
    if state.fullscreen_here() {
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
    let lay = crate::tray::layout(open, snap.battery.is_some(), mode.size.w);

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

    let handle = lay.cells[0].rect; // Handle всегда первая ячейка
    rounded_solid(
        &mut pool, &mut idx, handle.x, handle.y, handle.w, handle.h,
        handle.w / 2, BAR_BG, &mut els,
    );
    if let Some(p) = lay.panel {
        rounded_solid(&mut pool, &mut idx, p.x, p.y, p.w, p.h, BAR_RADIUS, BAR_BG, &mut els);
    }
    // Хват: короткая черта посреди полосочки. Без неё полоска читается как
    // обрубок бара, а не как то, на что можно нажать.
    {
        let w = (handle.w / 3).max(2);
        let h = handle.h / 2;
        let color = if open { TRAY_ON } else { TRAY_DIM };
        rounded_solid(
            &mut pool, &mut idx,
            handle.x + (handle.w - w) / 2, handle.y + (handle.h - h) / 2,
            w, h, w / 2, premul(color), &mut els,
        );
    }

    for cell in &lay.cells {
        let r = cell.rect;
        match cell.kind {
            CellKind::VolumeSlider => {
                let track_h = (BAR_H / 5).max(3);
                let ty = r.y + (r.h - track_h) / 2;
                rounded_solid(
                    &mut pool, &mut idx, r.x, ty, r.w, track_h, track_h / 2,
                    premul([1.0, 1.0, 1.0, 0.13]), &mut els,
                );
                if let Some((level, muted)) = volume {
                    let fill = (r.w as f32 * level.clamp(0.0, 1.0)).round() as i32;
                    if fill >= track_h {
                        let c = if muted { TRAY_OFF } else { TRAY_ON };
                        rounded_solid(
                            &mut pool, &mut idx, r.x, ty, fill, track_h, track_h / 2,
                            premul(c), &mut els,
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
                    rounded_solid(&mut pool, &mut idx, x, y, w, h, 0, premul(c), &mut els);
                }
            }
            // Взведённая кнопка питания подсвечена: видно, что следующий клик
            // уже сработает.
            CellKind::Power(a) if armed == Some(a) => {
                rounded_solid(
                    &mut pool, &mut idx, r.x, r.y, r.w, r.h, BAR_RADIUS / 2,
                    premul([0.95, 0.45, 0.30, 0.30]), &mut els,
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
            CellKind::Handle | CellKind::VolumeSlider => {}
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
    use super::on_screen;
    use smithay::utils::Size;

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
}
