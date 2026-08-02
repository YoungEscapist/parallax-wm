use std::{collections::HashMap, time::Duration};

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
}

impl RenderStats {
    pub fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            frames: 0, skipped: 0, total_us: 0, max_us: 0, max_elements: 0,
        }
    }

    fn record(&mut self, us: u64, elements: usize) {
        self.frames += 1;
        self.total_us += us;
        self.max_us = self.max_us.max(us);
        self.max_elements = self.max_elements.max(elements);
        self.flush();
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
             элементов до {}, пропущено (кадр уже в очереди) {}",
            self.frames as f64 / secs,
            self.total_us as f64 / self.frames.max(1) as f64 / 1000.0,
            self.max_us as f64 / 1000.0,
            self.max_elements,
            self.skipped,
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
                                    let gles = &mut device.gles as *mut GlesRenderer;
                                    unsafe { render_surface(surface, &mut *gles, state); }
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
    let gbm = GbmDevice::new(device_fd.clone())?;
    let egl = unsafe { EGLDisplay::new(gbm.clone())? };
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
    let mode_info = connector.modes().iter()
        .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
        .or_else(|| connector.modes().first())
        .copied()
        .ok_or("no modes")?;

    let wl_mode = Mode {
        size: (mode_info.size().0 as i32, mode_info.size().1 as i32).into(),
        refresh: (mode_info.vrefresh() * 1000) as i32,
    };

    let display = display_info::for_connector(&device.drm, connector.handle());
    let model = display.as_ref().and_then(|d| d.model())
        .unwrap_or_else(|| format!("{:?}", connector.interface()));
    let output_name = format!("{}-{}", model, connector.interface_id());

    let output = Output::new(output_name.clone(), PhysicalProperties {
        size: connector.size().map(|(w,h)| (w as i32, h as i32)).unwrap_or((0,0)).into(),
        subpixel: Subpixel::Unknown,
        make: "Unknown".into(),
        model: model.clone(),
        serial_number: "Unknown".into(),
    });
    let _global = output.create_global::<Dawn>(&state.display_handle);
    output.change_current_state(Some(wl_mode), Some(Transform::Normal), None, Some((0,0).into()));
    output.set_preferred(wl_mode);
    state.space.map_output(&output, (0, 0));

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
    let drm_surface = device.drm.create_surface(crtc, mode_info, &[connector.handle()])?;

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

    let proj = crate::canvas::project_minimap(&windows);
    let origin = crate::canvas::minimap_panel_origin(mode.size);

    // Закладки камеры читаем до заимствования пула — обе части лежат в state.
    let bookmarks: Vec<_> = state.camera_bookmarks.values().copied().collect();
    let pool = &mut state.minimap_ids;
    let mut idx = 0usize;

    elements.push(pooled_solid(
        pool, &mut idx, (origin.x, origin.y),
        (crate::canvas::MINIMAP_PANEL_W, crate::canvas::MINIMAP_PANEL_H),
        [0.05, 0.05, 0.08, 0.75],
    ));

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

    // ── Закладки камеры (bookmarks_mode): крестик на минимапе за каждую точку ─
    // Проецируем якорь закладки тем же bbox/scale, что и окна; рисуем крест из
    // двух перекладин. Точки вне панели (сильный зум/далеко) пропускаем.
    const CROSS_ARM: i32 = 5; // длина луча от центра, px
    const CROSS_TH: i32 = 2;  // толщина перекладины, px
    for anchor in bookmarks {
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
    }

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
    let radius = corner_radius_logical(state);
    state.decor.ensure(radius, [CLEAR_COLOR[0], CLEAR_COLOR[1], CLEAR_COLOR[2]]);
    let r_px = state.decor.mask_px() as f64 * zoom;

    let windows: Vec<Window> = state.tagged_windows.iter().map(|tw| tw.window.clone()).collect();
    let mut slot = 0usize;
    for window in windows {
        let Some((x0, y0, w, h)) = window_screen_rect(state, &window) else { continue };
        if w < r_px * 2.0 || h < r_px * 2.0 {
            continue; // окно слишком маленькое для радиуса — не портим его совсем
        }
        let corners = [
            (crate::decor::TL, x0, y0),
            (crate::decor::TR, x0 + w - r_px, y0),
            (crate::decor::BL, x0, y0 + h - r_px),
            (crate::decor::BR, x0 + w - r_px, y0 + h - r_px),
        ];
        for (corner, x, y) in corners {
            let buf = state.decor.mask_corner(corner, slot);
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer, Point::<f64, Physical>::from((x, y)), buf,
                None, None, None, Kind::Unspecified,
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
        let Some((x0, y0raw, w, h)) = window_screen_rect(state, &window) else { continue };
        if w < 8.0 || h < 8.0 || w < 2.0 * r || h < 2.0 * r {
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
        for (corner, x, y) in corners {
            let buf = state.decor.shadow_corner(corner, slot);
            match MemoryRenderBufferRenderElement::from_buffer(
                renderer, Point::<f64, Physical>::from((x, y)), buf,
                None, None, None, Kind::Unspecified,
            ) {
                Ok(el) => els.push(OutputRenderElements::Memory(el)),
                Err(e) => tracing::warn!("dawn/udev: угол тени: {:?}", e),
            }
        }


        // ── Кромки ───────────────────────────────────────────────────────────
        // Текстура толщиной в пиксель растягивается вдоль стороны через dst:
        // альфа вдоль стороны постоянна, так что растяжение точное.
        let side_w = ((w - 2.0 * r) / zoom).round().max(1.0) as i32;
        let side_h = ((h - 2.0 * r) / zoom).round().max(1.0) as i32;
        let edges = [
            (crate::decor::TOP,    x0 + r,     y0 - s,     side_w, crate::decor::SPREAD),
            (crate::decor::BOTTOM, x0 + r,     y0 + h,     side_w, crate::decor::SPREAD),
            (crate::decor::LEFT,   x0 - s,     y0 + r,     crate::decor::SPREAD, side_h),
            (crate::decor::RIGHT,  x0 + w,     y0 + r,     crate::decor::SPREAD, side_h),
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
    // Нижняя половина
    for i in 0..rr {
        let cw = widths[i as usize];
        let rw = w - 2 * cw;
        if rw > 0 {
            out.push(pooled_solid(pool, idx, (x + cw, y + h - rr + i), (rw, 1), color));
        }
    }
}

/// Панель рабочих столов — скруглённый бар сверху по центру.
/// Круглые точки = новые (непосещённые) столы. Белые иконки для посещённых:
/// круг=Tile, две колонки=Columns (niri), две тильды=Float, рамка=Monocle.
fn build_workspace_bar_elements(state: &mut Dawn, output: &Output) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    let mode = match output.current_mode() { Some(m) => m, None => return els };

    // Размеры: крупный скруглённый бар сверху.
    const BAR_H: i32 = 48;
    const BAR_RADIUS: i32 = 20;        // скругление фона бара
    const DOT: i32 = 28;
    const DOT_RADIUS: i32 = 14;        // скругление точек = круги
    const GAP: i32 = 8;
    const PAD_H: i32 = 20;
    const PAD_V: i32 = (BAR_H - DOT) / 2;
    const BAR_W: i32 = 2 * PAD_H + 9 * DOT + 8 * GAP;

    let ox = (mode.size.w - BAR_W) / 2;
    let oy = 10; // сверху, с отступом 10px

    let pool = &mut state.bar_ids;
    let mut idx = 0usize;

    // Фон бара — тёмный полупрозрачный скруглённый прямоугольник.
    const BG: [f32; 4] = [0.04, 0.04, 0.07, 0.65];
    rounded_solid(pool, &mut idx, ox, oy, BAR_W, BAR_H, BAR_RADIUS, BG, &mut els);

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

            match layout.unwrap_or(crate::tiling::Layout::Tile) {
                crate::tiling::Layout::Tile => {
                    // Круг — сам по себе иконка Tile
                    rounded_solid(pool, &mut idx, cx, oy + PAD_V, DOT, DOT, DOT_RADIUS, base_color, &mut els);
                }
                crate::tiling::Layout::Columns => {
                    // Фон — круг; поверх него две скруглённые колонки
                    rounded_solid(pool, &mut idx, cx, oy + PAD_V, DOT, DOT, DOT_RADIUS, base_color, &mut els);
                    let cw = (DOT * 2 / 5).max(2);
                    let gap = DOT - 2 * cw;
                    let inner_r = (cw / 2).max(1);
                    rounded_solid(pool, &mut idx, cx, oy + PAD_V, cw, DOT, inner_r, base_color, &mut els);
                    rounded_solid(pool, &mut idx, cx + cw + gap, oy + PAD_V, cw, DOT, inner_r, base_color, &mut els);
                }
                crate::tiling::Layout::Float => {
                    // Фон — круг; поверх него две скруглённые тильды
                    rounded_solid(pool, &mut idx, cx, oy + PAD_V, DOT, DOT, DOT_RADIUS, base_color, &mut els);
                    let th = (DOT / 3).max(2);
                    let tgap = DOT - 2 * th;
                    let inner_r = (th / 2).max(1);
                    rounded_solid(pool, &mut idx, cx, oy + PAD_V, DOT, th, inner_r, base_color, &mut els);
                    rounded_solid(pool, &mut idx, cx, oy + PAD_V + th + tgap, DOT, th, inner_r, base_color, &mut els);
                }
                crate::tiling::Layout::Monocle => {
                    // Фон — круг; поверх него скруглённый вложенный квадрат
                    rounded_solid(pool, &mut idx, cx, oy + PAD_V, DOT, DOT, DOT_RADIUS, base_color, &mut els);
                    let inset = (DOT / 4).max(1);
                    let mw = DOT - 2 * inset;
                    let mh = DOT - 2 * inset;
                    let inner_r = (inset / 2).max(1);
                    rounded_solid(pool, &mut idx, cx + inset, oy + PAD_V + inset, mw, mh, inner_r, base_color, &mut els);
                }
            }
        }
    }

    els
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
    let map = layer_map_for_output(output);
    // Собираем все подходящие layer-поверхности (сортируем по вложению).
    let to_render: Vec<_> = map.layers().filter(|l| layers.contains(&l.layer())).cloned().collect();
    for layer_surface in to_render {
        let Some(geo) = map.layer_geometry(&layer_surface) else { continue };
        let phys_loc: Point<i32, Physical> = (geo.loc.x, geo.loc.y).into();
        // Рендерим основную поверхность + дочерние.
        layer_surface.with_surfaces(|surface, _states| {
            let surface_els = render_elements_from_surface_tree(
                renderer, surface, phys_loc, 1.0, 1.0, Kind::Unspecified,
            );
            els.extend(surface_els);
        });
    }
    els
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
    let mut elements: Vec<OutputRenderElements> = Vec::new();

    // ── Cursor (front layer) ─────────────────────────────────────────────────
    let cursor_pos_physical = {
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

    match &state.cursor_status {
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
            let cursor_els: Vec<OutputRenderElements> =
                render_elements_from_surface_tree(
                    renderer, cursor_surface, pos, 1.0, 1.0, Kind::Cursor,
                );
            elements.extend(cursor_els);
        }
        CursorImageStatus::Named(_) => {
            if let Some(ref buf) = state.cursor_default_buffer {
                // Масштабируем курсор пропорционально зуму.
                // dst задаёт логический размер: при zoom=2 логический пиксель = 2 физических,
                // поэтому native_size логических → native_size*zoom физических пикселей.
                let sz = state.cursor_default_size;
                let dst = smithay::utils::Size::<i32, smithay::utils::Logical>::from((sz.w, sz.h));
                match MemoryRenderBufferRenderElement::from_buffer(
                    renderer, cursor_pos_physical, buf, None, None, Some(dst), Kind::Cursor,
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

    // ── Панель рабочих столов (всегда видна, поверх окон, под курсором) ────
    elements.extend(build_workspace_bar_elements(state, &surface.output));

    // ── Миникарта (3.1, поверх окон, под курсором) ───────────────────────────
    // Не показываем во время обзора столов (перекрывает ленту).
    if state.is_minimap_visible && !state.overview_active {
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

                let els: Vec<OutputRenderElements> = window.render_elements(
                    renderer, portal.screen_pos, smithay::utils::Scale::from(scale), 1.0f32,
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

    // ── Top-слой (wlr-layer-shell): поверх окон, под UI -----------------------
    elements.extend(build_layer_elements(state, renderer, &surface.output, &[WlrLayer::Top]));

    // ── Space elements (behind cursor) ───────────────────────────────────────
    // ВАЖНО: используем штатный space.render_elements_for_output (проверенный,
    // работавший путь), а не ручной per-window цикл — на живом тесте ручной
    // цикл ломал рендер содержимого окон (см. историю правок). Frustum culling
    // (4.1) и per-window fog (5.2) через этот путь недоступны (единый alpha
    // на весь батч) — оставлены как заготовки на будущее в этом комментарии,
    // но не подключены, чтобы не рисковать существующим рендером.
    match state.space.render_elements_for_output::<GlesRenderer>(renderer, &surface.output, 1.0f32) {
        Ok(els) => elements.extend(els.into_iter().map(OutputRenderElements::from)),
        Err(e) => { tracing::warn!("dawn/udev: render_elements: {:?}", e); return; }
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

    // ── Parallax + Background-слой (wlr-layer-shell): самый задний план ─────────
    if let Some(mode) = surface.output.current_mode() {
        elements.extend(build_parallax_elements(state, renderer, mode));
    }
    elements.extend(build_layer_elements(state, renderer, &surface.output, &[WlrLayer::Background]));

    let element_count = elements.len();
    let output_name = surface.output.name();
    match surface.compositor.render_frame(
        renderer, &elements, [0.1f32, 0.1, 0.1, 1.0], FrameFlags::empty()
    ) {
        Ok(res) => {
            // trace!, а не debug!: это две строки на КАЖДЫЙ кадр (при 60 Гц —
            // ~50 КБ/с), а лог из launch_tty.zsh идёт через tee синхронной
            // записью на диск прямо из потока рендера, который у dawn один.
            tracing::trace!("dawn/udev: render_frame[{}]: is_empty={}", output_name, res.is_empty);
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
        let map = layer_map_for_output(&surface.output);
        for layer_surface in map.layers() {
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
