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
                Kind,
                memory::MemoryRenderBufferRenderElement,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::{WaylandSurfaceRenderElement, render_elements_from_surface_tree},
            },
            gles::GlesRenderer,
        },
        session::{libseat::LibSeatSession, Event as SessionEvent, Session},
        udev::{UdevBackend, UdevEvent},
    },
    desktop::space::SpaceRenderElements,
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
    utils::{DeviceFd, Transform},
    wayland::compositor::with_states,
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
    DefaultCursor = MemoryRenderBufferRenderElement<GlesRenderer>,
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
}

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
                libinput_for_notifier.suspend();
                // Отдаём DRM master — seatd передаёт его другому compositor'у
                for device in state.udev_devices.values_mut() {
                    device.drm.pause();
                }
            }
            SessionEvent::ActivateSession => {
                tracing::info!("dawn/udev: session activated — acquiring DRM master");
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
                            tracing::debug!("dawn/drm: VBlank crtc={:?}", crtc);
                            let mut devices = std::mem::take(&mut state.udev_devices);
                            if let Some(device) = devices.get_mut(&node) {
                                if let Some(surface) = device.surfaces.get_mut(&crtc) {
                                    // ОБЯЗАТЕЛЬНО: без этого compositor думает
                                    // что предыдущий frame ещё в flight
                                    match surface.compositor.frame_submitted() {
                                        Ok(_) => {}
                                        Err(e) => tracing::warn!("dawn/drm: frame_submitted: {:?}", e),
                                    }
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
    device.surfaces.insert(crtc, Surface { output, compositor, damage_tracker });
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
fn build_selection_elements(state: &Dawn) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    let zoom = state.viewport.zoom;

    for window in &state.selected_windows {
        let geo = match state.space.element_geometry(window) { Some(g) => g, None => continue };
        let x = ((geo.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y = ((geo.loc.y as f64 - cam_y) * zoom).round() as i32;
        let w = ((geo.size.w as f64 * zoom).round() as i32).max(1);
        let h = ((geo.size.h as f64 * zoom).round() as i32).max(1);
        let buf = SolidColorBuffer::new((w, h), [1.0f32, 0.7, 0.2, 0.22]);
        elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
            &buf, (x, y), 1.0_f64, 1.0, Kind::Unspecified,
        )));
    }

    if let Some(rect) = state.selection_drag {
        let x = ((rect.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y = ((rect.loc.y as f64 - cam_y) * zoom).round() as i32;
        let w = ((rect.size.w as f64 * zoom).round() as i32).max(1);
        let h = ((rect.size.h as f64 * zoom).round() as i32).max(1);
        let buf = SolidColorBuffer::new((w, h), [0.35f32, 0.6, 1.0, 0.16]);
        elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
            &buf, (x, y), 1.0_f64, 1.0, Kind::Unspecified,
        )));
    }

    elements
}

fn build_minimap_elements(state: &Dawn, output: &Output) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    let mode = match output.current_mode() { Some(m) => m, None => return elements };

    let current_tags = state.viewport.current_tags();
    let focused = state.seat.get_keyboard().and_then(|kb| kb.current_focus());
    let windows: Vec<(smithay::utils::Point<i32, smithay::utils::Logical>, smithay::utils::Size<i32, smithay::utils::Logical>, bool)> =
        state.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .filter_map(|tw| state.space.element_geometry(&tw.window).map(|g| {
                let is_focused = focused.as_ref()
                    .zip(tw.window.toplevel())
                    .map(|(fs, t)| t.wl_surface() == fs)
                    .unwrap_or(false);
                (g.loc, g.size, is_focused)
            }))
            .collect();

    let proj = crate::canvas::project_minimap(&windows);
    let origin = crate::canvas::minimap_panel_origin(mode.size);

    let bg = SolidColorBuffer::new(
        (crate::canvas::MINIMAP_PANEL_W, crate::canvas::MINIMAP_PANEL_H),
        [0.05f32, 0.05, 0.08, 0.75],
    );
    elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
        &bg, origin, 1.0_f64, 1.0, Kind::Unspecified,
    )));

    for b in &proj.boxes {
        let color: [f32; 4] = if b.focused { [0.35, 0.55, 0.95, 0.9] } else { [0.6, 0.6, 0.65, 0.75] };
        let buf = SolidColorBuffer::new((b.size.w, b.size.h), color);
        let loc = smithay::utils::Point::<i32, smithay::utils::Physical>::from((
            origin.x + b.loc.x, origin.y + b.loc.y,
        ));
        elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
            &buf, loc, 1.0_f64, 1.0, Kind::Unspecified,
        )));
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
    for anchor in state.camera_bookmarks.values() {
        let p = crate::canvas::project_point_minimap(*anchor, proj.bbox, proj.scale);
        if p.x < 0 || p.y < 0
            || p.x >= crate::canvas::MINIMAP_PANEL_W
            || p.y >= crate::canvas::MINIMAP_PANEL_H
        {
            continue;
        }
        let color = [1.0f32, 0.30, 0.45, 0.95];
        // горизонтальная перекладина
        let hbuf = SolidColorBuffer::new((CROSS_ARM * 2 + 1, CROSS_TH), color);
        let hloc = smithay::utils::Point::<i32, smithay::utils::Physical>::from((
            origin.x + p.x - CROSS_ARM, origin.y + p.y - CROSS_TH / 2,
        ));
        elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
            &hbuf, hloc, 1.0_f64, 1.0, Kind::Unspecified,
        )));
        // вертикальная перекладина
        let vbuf = SolidColorBuffer::new((CROSS_TH, CROSS_ARM * 2 + 1), color);
        let vloc = smithay::utils::Point::<i32, smithay::utils::Physical>::from((
            origin.x + p.x - CROSS_TH / 2, origin.y + p.y - CROSS_ARM,
        ));
        elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
            &vbuf, vloc, 1.0_f64, 1.0, Kind::Unspecified,
        )));
    }

    elements
}

/// Бесшовный параллакс-фон (5.1): редкая сетка точек на самом заднем слое,
/// сдвигается на camera*0.3 вместо camera*1.0 — создаёт эффект глубины
/// (фон "отстаёт" от окон при панорамировании).
const PARALLAX_FACTOR: f64 = 0.3;
const PARALLAX_SPACING_PX: i32 = 160;
const PARALLAX_DOT_PX: i32 = 3;

fn build_parallax_elements(state: &Dawn, mode: Mode) -> Vec<OutputRenderElements> {
    let mut out = Vec::new();
    let zoom = state.viewport.zoom;

    let shift_x = state.viewport.cam_x * zoom * PARALLAX_FACTOR;
    let shift_y = state.viewport.cam_y * zoom * PARALLAX_FACTOR;
    let offset_x = shift_x.rem_euclid(PARALLAX_SPACING_PX as f64);
    let offset_y = shift_y.rem_euclid(PARALLAX_SPACING_PX as f64);

    let buf = SolidColorBuffer::new((PARALLAX_DOT_PX, PARALLAX_DOT_PX), [1.0f32, 1.0, 1.0, 0.08]);

    let mut y = -(offset_y as i32);
    while y < mode.size.h {
        let mut x = -(offset_x as i32);
        while x < mode.size.w {
            out.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                &buf, (x, y), 1.0_f64, 1.0, Kind::Unspecified,
            )));
            x += PARALLAX_SPACING_PX;
        }
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

/// Возвращает 1×`width` буфер цвета `CLEAR_COLOR` из `state.corner_mask_cache`,
/// создавая его при первом обращении. Ширина среза (`corner_cutout_widths`)
/// принимает не больше 24 разных значений (r_px ∈ [2,24]), так что кэш растёт
/// до пары десятков записей и никогда не чистится — раньше этот буфер
/// пересоздавался заново на КАЖДУЮ строку КАЖДОГО угла КАЖДОГО окна КАЖДЫЙ
/// кадр (до 4*24 аллокаций на окно на кадр), хотя содержимое зависит только
/// от ширины среза.
fn cached_corner_buf(cache: &mut HashMap<i32, SolidColorBuffer>, width: i32) -> &SolidColorBuffer {
    cache.entry(width).or_insert_with(|| SolidColorBuffer::new((width, 1), CLEAR_COLOR))
}

fn build_corner_mask_elements(state: &mut Dawn, output: &Output) -> Vec<OutputRenderElements> {
    let mut elements = Vec::new();
    if state.tagged_windows.is_empty() {
        return elements;
    }
    let zoom = state.viewport.zoom;
    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;

    let radius_logical = if state.tile_config.layout == crate::tiling::Layout::Tile {
        CORNER_RADIUS_LOGICAL_TILE
    } else {
        CORNER_RADIUS_LOGICAL
    };
    let r_px = ((radius_logical * zoom).round() as i32).clamp(2, 32);
    let widths = corner_cutout_widths(r_px);
    let _ = output;

    let space = &state.space;
    let tagged_windows = &state.tagged_windows;
    let cache = &mut state.corner_mask_cache;

    for tw in tagged_windows {
        let geo = match space.element_geometry(&tw.window) { Some(g) => g, None => continue };
        let w_px = (geo.size.w as f64 * zoom).round() as i32;
        let h_px = (geo.size.h as f64 * zoom).round() as i32;
        if w_px < r_px * 2 || h_px < r_px * 2 {
            continue; // окно слишком маленькое для радиуса — не портим его совсем
        }
        let x0 = ((geo.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y0 = ((geo.loc.y as f64 - cam_y) * zoom).round() as i32;

        for (row, &cw) in widths.iter().enumerate() {
            if cw <= 0 {
                continue;
            }
            let row = row as i32;
            // top-left / top-right — сверху окна
            let buf = cached_corner_buf(cache, cw);
            elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                buf, (x0, y0 + row), 1.0_f64, 1.0, Kind::Unspecified,
            )));
            let buf = cached_corner_buf(cache, cw);
            elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                buf, (x0 + w_px - cw, y0 + row), 1.0_f64, 1.0, Kind::Unspecified,
            )));
            // bottom-left / bottom-right — снизу окна
            let by = y0 + h_px - 1 - row;
            let buf = cached_corner_buf(cache, cw);
            elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                buf, (x0, by), 1.0_f64, 1.0, Kind::Unspecified,
            )));
            let buf = cached_corner_buf(cache, cw);
            elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                buf, (x0 + w_px - cw, by), 1.0_f64, 1.0, Kind::Unspecified,
            )));
        }
    }

    elements
}

/// Мягкая нейтральная тень-«гало» позади каждого окна — НЕСКОЛЬКО слоёв разного
/// размера и убывающей прозрачности дают перо/растушёвку (эффект «блюра фона»),
/// а не резкий контур. Нейтральный чёрный (НЕ голубой). Скругление — строками
/// (corner_cutout_widths), но строки рисуют чёрным (углы тени прозрачны).
fn build_shadow_elements(state: &Dawn) -> Vec<OutputRenderElements> {
    // Многослойная растушёвка: 5 слоёв от плотного ядра до широкого ореола
    // с низкой альфой — имитация размытия как "замазали в фотошопе".
    const LAYERS: [(i32, f32); 5] = [(2, 0.09), (5, 0.06), (9, 0.035), (14, 0.02), (20, 0.01)];
    const DROP: i32 = 4;
    let mut els = Vec::new();
    if state.tagged_windows.is_empty() {
        return els;
    }
    let zoom = state.viewport.zoom;
    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    let radius_logical = if state.tile_config.layout == crate::tiling::Layout::Tile {
        CORNER_RADIUS_LOGICAL_TILE
    } else {
        CORNER_RADIUS_LOGICAL
    };
    let r_px_base = (radius_logical * zoom).round() as i32;
    for tw in &state.tagged_windows {
        let geo = match state.space.element_geometry(&tw.window) { Some(g) => g, None => continue };
        let w_px = (geo.size.w as f64 * zoom).round() as i32;
        let h_px = (geo.size.h as f64 * zoom).round() as i32;
        if w_px < 8 || h_px < 8 {
            continue;
        }
        let x0 = ((geo.loc.x as f64 - cam_x) * zoom).round() as i32;
        let y0 = ((geo.loc.y as f64 - cam_y) * zoom).round() as i32;

        for (spread, alpha) in LAYERS {
            let color = [0.0f32, 0.0, 0.0, alpha];
            let sx = x0 - spread;
            let sy = y0 - spread + DROP;
            let sw = w_px + 2 * spread;
            let sh = h_px + 2 * spread;
            let r_sh = (r_px_base + spread).clamp(2, sw.min(sh) / 2);
            let widths = corner_cutout_widths(r_sh);
            let rr = widths.len() as i32;
            if sh <= 2 * rr || sw <= 2 * rr {
                let buf = SolidColorBuffer::new((sw.max(1), sh.max(1)), color);
                els.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                    &buf, (sx, sy), 1.0_f64, 1.0, Kind::Unspecified,
                )));
                continue;
            }
            for (i, &cw) in widths.iter().enumerate() {
                let rw = sw - 2 * cw;
                if rw <= 0 { continue; }
                let i = i as i32;
                let buf = SolidColorBuffer::new((rw, 1), color);
                els.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                    &buf, (sx + cw, sy + i), 1.0_f64, 1.0, Kind::Unspecified,
                )));
                let by = sy + sh - 1 - i;
                let buf = SolidColorBuffer::new((rw, 1), color);
                els.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                    &buf, (sx + cw, by), 1.0_f64, 1.0, Kind::Unspecified,
                )));
            }
            let midh = sh - 2 * rr;
            if midh > 0 {
                let buf = SolidColorBuffer::new((sw, midh), color);
                els.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                    &buf, (sx, sy + rr), 1.0_f64, 1.0, Kind::Unspecified,
                )));
            }
        }
    }
    els
}

/// Полупрозрачный «заметный» фон + тень под каждым воркспейсом — ТОЛЬКО в обзоре
/// (тап Super), чтобы столы визуально читались как отдельные карточки.
fn build_overview_bg_elements(state: &Dawn) -> Vec<OutputRenderElements> {
    let mut els = Vec::new();
    if !state.overview_active {
        return els;
    }
    // Тень за каждым бэндом — многослойная растушёвка, как у окон.
    const SHADOW_LAYERS: [(i32, f32); 3] = [(3, 0.10), (7, 0.05), (12, 0.02)];
    // Фон бэнда — тёмный полупрозрачный прямоугольник со скруглёнными углами.
    const BG_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.28];
    const ROUNDING: i32 = 12;
    let zoom = state.viewport.zoom;
    let cam_x = state.viewport.cam_x;
    let cam_y = state.viewport.cam_y;
    for r in state.overview_band_rects() {
        let sx = ((r.loc.x as f64 - cam_x) * zoom).round() as i32;
        let sy = ((r.loc.y as f64 - cam_y) * zoom).round() as i32;
        let sw = (r.size.w as f64 * zoom).round() as i32;
        let sh = (r.size.h as f64 * zoom).round() as i32;
        if sw <= 0 || sh <= 0 {
            continue;
        }

        // ── Тени (позади бэнда) ──
        for (spread, alpha) in SHADOW_LAYERS {
            let sx_sh = sx - spread;
            let sy_sh = sy - spread;
            let sw_sh = sw + 2 * spread;
            let sh_sh = sh + 2 * spread;
            let color = [0.0f32, 0.0, 0.0, alpha];
            let buf = SolidColorBuffer::new((sw_sh.max(1), sh_sh.max(1)), color);
            els.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                &buf, (sx_sh, sy_sh), 1.0_f64, 1.0, Kind::Unspecified,
            )));
        }

        // ── Фон бэнда (скруглённый) ──
        let r_px = (ROUNDING as f64 * zoom).round() as i32;
        let widths = corner_cutout_widths(r_px);
        let rr = widths.len() as i32;
        if sh <= 2 * rr || sw <= 2 * rr {
            let buf = SolidColorBuffer::new((sw.max(1), sh.max(1)), BG_COLOR);
            els.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                &buf, (sx, sy), 1.0_f64, 1.0, Kind::Unspecified,
            )));
            continue;
        }
        for (i, &cw) in widths.iter().enumerate() {
            let rw = sw - 2 * cw;
            if rw <= 0 { continue; }
            let buf = SolidColorBuffer::new((rw, sh - 2 * (i as i32)), BG_COLOR);
            els.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                &buf, (sx + cw, sy + i as i32), 1.0_f64, 1.0, Kind::Unspecified,
            )));
        }
    }
    els
}

pub fn render_surface(surface: &mut Surface, renderer: &mut GlesRenderer, state: &mut Dawn) {
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

    match &state.cursor_status {
        CursorImageStatus::Surface(ref cursor_surface) => {
            let hotspot = with_states(cursor_surface, |states| {
                states.data_map.get::<CursorImageSurfaceData>()
                    .map(|d| d.lock().unwrap().hotspot)
                    .unwrap_or_default()
            });
            let p = state.pointer_location - hotspot.to_f64();
            let output_local = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                p.x - state.viewport.cam_x,
                p.y - state.viewport.cam_y,
            ));
            let pos = output_local.to_physical(state.viewport.zoom).to_i32_round();
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
                    Ok(el) => elements.push(OutputRenderElements::DefaultCursor(el)),
                    Err(e) => tracing::warn!("dawn/udev: cursor render element: {:?}", e),
                }
            }
        }
        CursorImageStatus::Hidden => {}
    }

    // ── Миникарта (3.1, поверх окон, под курсором) ───────────────────────────
    // Не показываем во время обзора столов (перекрывает ленту).
    if state.is_minimap_visible && !state.overview_active {
        elements.extend(build_minimap_elements(state, &surface.output));
    }

    // ── Оконный портал (4.4): живая копия удалённого окна в фикс. точке экрана ─
    if let Some(portal) = &state.portal {
        if let Some(window) = state.tagged_windows.iter()
            .find(|tw| tw.window.toplevel().map(|t| t.wl_surface() == &portal.surface).unwrap_or(false))
            .map(|tw| tw.window.clone())
        {
            if let Some(geo) = state.space.element_geometry(&window) {
                let scale_x = portal.box_size.w as f64 / geo.size.w.max(1) as f64;
                let scale_y = portal.box_size.h as f64 / geo.size.h.max(1) as f64;
                let scale = scale_x.min(scale_y);

                let bg = SolidColorBuffer::new((portal.box_size.w, portal.box_size.h), [0.0f32, 0.0, 0.0, 0.85]);
                elements.push(OutputRenderElements::Solid(SolidColorRenderElement::from_buffer(
                    &bg, portal.screen_pos, 1.0_f64, 1.0, Kind::Unspecified,
                )));

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
    elements.extend(build_corner_mask_elements(state, &surface.output));

    // ── Мультивыделение (rubber-band + подсветка "созвездий") ───────────────
    elements.extend(build_selection_elements(state));

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

    // ── Тени окон (полупрозрачные, скруглённые), сразу ПОЗАДИ окон ──────────
    // Не рендерим в обзоре — при zoom=0.5 тени незаметны, но много элементов.
    if !state.overview_active {
        elements.extend(build_shadow_elements(state));
    }

    // Focus Aura (голубое свечение) УБРАНА — её принимали за "голубую тень".
    // Глубину теперь даёт нейтральная мягкая build_shadow_elements выше.

    // ── Фон рабочих столов в обзоре (только при тапе Super), позади окон ────
    elements.extend(build_overview_bg_elements(state));

    // ── Параллакс-фон (5.1) — самый задний слой, сдвигается медленнее окон ──
    if let Some(mode) = surface.output.current_mode() {
        elements.extend(build_parallax_elements(state, mode));
    }

    let output_name = surface.output.name();
    match surface.compositor.render_frame(
        renderer, &elements, [0.1f32, 0.1, 0.1, 1.0], FrameFlags::empty()
    ) {
        Ok(res) => {
            tracing::debug!("dawn/udev: render_frame[{}]: is_empty={}", output_name, res.is_empty);
            match surface.compositor.queue_frame(()) {
                Ok(()) => tracing::debug!("dawn/udev: queue_frame[{}]: committed", output_name),
                Err(FrameError::EmptyFrame) => tracing::debug!("dawn/udev: queue_frame[{}]: EmptyFrame", output_name),
                Err(e) => tracing::warn!("dawn/udev: queue_frame[{}]: {:?}", output_name, e),
            }
        }
        Err(e) => tracing::warn!("dawn/udev: render_frame[{}]: {:?}", output_name, e),
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
