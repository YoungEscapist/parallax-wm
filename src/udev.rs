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
                Kind,
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

    let (mut session, notifier) = LibSeatSession::new()?;
    let seat_name = session.seat();
    tracing::info!("dawn/udev: seat={}", seat_name);

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
            UdevEvent::Changed { .. } => {}
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

pub fn render_surface(surface: &mut Surface, renderer: &mut GlesRenderer, state: &mut Dawn) {
    let mut elements: Vec<OutputRenderElements> =
        match state.space.render_elements_for_output::<GlesRenderer>(renderer, &surface.output, 1.0f32) {
            Ok(els) => els.into_iter().map(OutputRenderElements::from).collect(),
            Err(e) => { tracing::warn!("dawn/udev: render_elements: {:?}", e); return; }
        };

    // Named/Hidden курсор пока не рисуем (нет xcursor-фолбэка) — это касается
    // только клиентов, которые ничего не задали через set_cursor. Как только
    // клиент (foot, kitty, ...) выставляет Surface — рисуем её поверх окон.
    if let CursorImageStatus::Surface(ref cursor_surface) = state.cursor_status {
        let hotspot = with_states(cursor_surface, |states| {
            states.data_map.get::<CursorImageSurfaceData>()
                .map(|d| d.lock().unwrap().hotspot)
                .unwrap_or_default()
        });
        let cursor_pos = (state.pointer_location - hotspot.to_f64())
            .to_physical(1.0)
            .to_i32_round();
        let cursor_elements: Vec<OutputRenderElements> =
            render_elements_from_surface_tree(
                renderer, cursor_surface, cursor_pos, 1.0, 1.0, Kind::Cursor,
            );
        elements.extend(cursor_elements);
    }

    match surface.compositor.render_frame(
        renderer, &elements, [0.1f32, 0.1, 0.1, 1.0], FrameFlags::empty()
    ) {
        Ok(_) => {
            match surface.compositor.queue_frame(()) {
                Ok(()) => {}
                Err(FrameError::EmptyFrame) => {}
                Err(e) => tracing::warn!("dawn/udev: queue_frame: {:?}", e),
            }
        }
        Err(e) => tracing::warn!("dawn/udev: render_frame: {:?}", e),
    }

    let elapsed = state.start_time.elapsed();
    state.space.elements().for_each(|window| {
        window.send_frame(
            &surface.output, elapsed,
            Some(Duration::from_millis(16)),
            |_, _| Some(surface.output.clone()),
        );
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
