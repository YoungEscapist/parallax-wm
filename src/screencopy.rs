//! Захват экрана (screencast) — `ext-image-copy-capture-v1`.
//!
//! Без этого протокола демонстрация экрана в Discord/Vesktop показывает чёрный
//! прямоугольник с курсором: Electron не находит у компоситора ни одного
//! способа захвата, падает на X11-путь и снимает root-окно rootless Xwayland,
//! а там ничего нет — wayland-окна в него не попадают.
//!
//! Реализованы три глобала:
//!  · `ext-image-capture-source-v1` + `ext-output-image-capture-source-manager-v1`
//!    — клиент выбирает ЧТО снимать (у нас: wl_output целиком);
//!  · `ext-image-copy-capture-v1` — сессия захвата и кадры.
//!
//! Кадр НЕ снимается синхронно в момент запроса: рендер живёт в udev.rs и
//! владеет GlesRenderer, поэтому запрос кладётся в очередь `pending_frames`, а
//! обслуживается в `serve_pending` сразу после отрисовки обычного кадра — теми
//! же render-элементами, что ушли на экран. Так захват всегда показывает ровно
//! то, что видит пользователь, и не требует второго прохода по состоянию.

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            Bind, ExportMem, Offscreen,
            damage::OutputDamageTracker,
            element::RenderElement,
            gles::{GlesRenderbuffer, GlesRenderer},
        },
    },
    output::{Output, WeakOutput},
    reexports::wayland_server::protocol::{wl_pointer::WlPointer, wl_shm},
    utils::{Buffer as BufferCoords, IsAlive, Rectangle, Size, Transform},
    wayland::{
        image_capture_source::{
            ImageCaptureSource, ImageCaptureSourceHandler, OutputCaptureSourceHandler,
            OutputCaptureSourceState,
        },
        image_copy_capture::{
            BufferConstraints, CaptureFailureReason, Frame, ImageCopyCaptureHandler,
            ImageCopyCaptureState, Session, SessionRef,
        },
        shm::with_buffer_contents_mut,
    },
};

use crate::state::Dawn;

/// Форматы shm, которые мы умеем отдавать. Argb8888 у GLES читается
/// нативно (GL_BGRA_EXT), Xrgb8888 — тот же байтовый порядок без альфы.
const SHM_FORMATS: [wl_shm::Format; 2] = [wl_shm::Format::Argb8888, wl_shm::Format::Xrgb8888];

/// Отложенный запрос кадра: пара (сессия, кадр) ждёт ближайшего рендера.
pub struct PendingFrame {
    pub session: SessionRef,
    pub frame: Frame,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

impl ImageCaptureSourceHandler for Dawn {}

impl OutputCaptureSourceHandler for Dawn {
    fn output_capture_source_state(&mut self) -> &mut OutputCaptureSourceState {
        &mut self.output_capture_source_state
    }

    /// Запоминаем в user_data источника, какому output он соответствует —
    /// иначе позже, в capture_constraints/serve_pending, связь не восстановить.
    fn output_source_created(&mut self, source: ImageCaptureSource, output: &Output) {
        source.user_data().insert_if_missing(|| output.downgrade());
    }
}

impl ImageCopyCaptureHandler for Dawn {
    fn image_copy_capture_state(&mut self) -> &mut ImageCopyCaptureState {
        &mut self.image_copy_capture_state
    }

    fn capture_constraints(&mut self, source: &ImageCaptureSource) -> Option<BufferConstraints> {
        let output = source_output(source)?;
        let mode = output.current_mode()?;
        Some(BufferConstraints {
            // Снимаем в физическом разрешении режима, а НЕ в логическом:
            // логический размер output'а в dawn плавает вместе с зумом холста
            // (zoom проброшен как fractional scale, см. Dawn::apply_camera), и
            // размер буфера у зрителя дёргался бы на каждый зум.
            size: mode.size.to_logical(1).to_buffer(1, Transform::Normal),
            shm: SHM_FORMATS.to_vec(),
            // Только shm: dmabuf-путь потребовал бы согласования модификаторов с
            // GBM-аллокатором конкретной карты, а xdg-desktop-portal-wlr
            // прекрасно работает и через shm.
            dma: None,
        })
    }

    /// Курсор мы и так вкомпозичиваем в кадр (см. serve_pending), отдельная
    /// cursor-сессия не нужна — вернув None, говорим клиенту не заводить её.
    fn cursor_capture_constraints(
        &mut self,
        _source: &ImageCaptureSource,
        _pointer: &WlPointer,
    ) -> Option<BufferConstraints> {
        None
    }

    fn new_session(&mut self, session: Session) {
        tracing::info!("dawn/screencopy: новая сессия захвата");
        // Держим сессию живой: Session при Drop шлёт клиенту `stopped`, а
        // клиент (портал) на это закрывает демонстрацию.
        self.capture_sessions.push(session);
    }

    fn frame(&mut self, session: &SessionRef, frame: Frame) {
        // Отдать кадр можно только из рендера (там GlesRenderer) — кладём в
        // очередь и будим рендер.
        self.pending_frames.push(PendingFrame {
            session: session.clone(),
            frame,
        });
        self.request_redraw();
    }

    fn session_destroyed(&mut self, session: SessionRef) {
        self.capture_sessions.retain(|s| s.as_ref() != session);
        self.pending_frames.retain(|p| p.session != session);
        tracing::info!("dawn/screencopy: сессия захвата закрыта");
    }
}

smithay::delegate_image_capture_source!(Dawn);
smithay::delegate_output_capture_source!(Dawn);
smithay::delegate_image_copy_capture!(Dawn);

// ── Съём кадра ───────────────────────────────────────────────────────────────

/// Output, к которому привязан источник захвата (положен в user_data при
/// создании источника, см. output_source_created).
fn source_output(source: &ImageCaptureSource) -> Option<Output> {
    source.user_data().get::<WeakOutput>()?.upgrade()
}

fn shm_format_to_fourcc(format: wl_shm::Format) -> Option<Fourcc> {
    match format {
        wl_shm::Format::Argb8888 => Some(Fourcc::Argb8888),
        wl_shm::Format::Xrgb8888 => Some(Fourcc::Xrgb8888),
        _ => None,
    }
}

/// Обслужить накопившиеся запросы кадров для `output`.
///
/// Зовётся из udev::render_surface сразу после того, как кадр ушёл на экран,
/// и получает ТЕ ЖЕ элементы. `elements` — front-to-back, как их принимает
/// smithay; `cursor_elements` — сколько первых из них рисуют курсор (сессия
/// может попросить кадр без курсора, тогда мы их отбрасываем).
pub fn serve_pending<E>(
    state: &mut Dawn,
    output: &Output,
    renderer: &mut GlesRenderer,
    elements: &[E],
    cursor_elements: usize,
) where
    E: RenderElement<GlesRenderer>,
{
    if state.pending_frames.is_empty() {
        return;
    }

    // Забираем только кадры, адресованные ЭТОМУ output'у; чужие оставляем в
    // очереди — их обслужит рендер их собственного CRTC.
    let mut mine = Vec::new();
    let mut rest = Vec::new();
    for pending in std::mem::take(&mut state.pending_frames) {
        let matches = source_output(&pending.session.source())
            .map(|o| &o == output)
            .unwrap_or(false);
        if matches {
            mine.push(pending);
        } else {
            rest.push(pending);
        }
    }
    state.pending_frames = rest;
    if mine.is_empty() {
        return;
    }

    let Some(mode) = output.current_mode() else {
        for p in mine {
            p.frame.fail(CaptureFailureReason::Unknown);
        }
        return;
    };
    let size: Size<i32, BufferCoords> = (mode.size.w, mode.size.h).into();
    let presented = state.start_time.elapsed();

    // Один offscreen-рендер на все кадры этого прохода: сессий может быть
    // несколько (портал + записывающий клиент), картинка у них одна и та же.
    let with_cursor = mine.iter().any(|p| p.session.draw_cursor());
    let without_cursor = mine.iter().any(|p| !p.session.draw_cursor());

    let mut shot_with: Option<Vec<u8>> = None;
    let mut shot_without: Option<Vec<u8>> = None;
    if with_cursor {
        shot_with = capture(renderer, output, elements, size);
    }
    if without_cursor {
        shot_without = capture(renderer, output, &elements[cursor_elements..], size);
    }

    for pending in mine {
        let shot = if pending.session.draw_cursor() {
            shot_with.as_ref()
        } else {
            shot_without.as_ref()
        };
        let Some(pixels) = shot else {
            pending.frame.fail(CaptureFailureReason::Unknown);
            continue;
        };
        match write_to_buffer(&pending.frame, pixels, size) {
            Ok(()) => pending.frame.success(
                Transform::Normal,
                Some(vec![Rectangle::from_size(size)]),
                presented,
            ),
            Err(reason) => pending.frame.fail(reason),
        }
    }
}

/// Рисует `elements` в offscreen-буфер размера `size` и возвращает пиксели в
/// Argb8888 (плотная упаковка, stride = w*4).
fn capture<E>(
    renderer: &mut GlesRenderer,
    output: &Output,
    elements: &[E],
    size: Size<i32, BufferCoords>,
) -> Option<Vec<u8>>
where
    E: RenderElement<GlesRenderer>,
{
    let mut target: GlesRenderbuffer = match renderer.create_buffer(Fourcc::Abgr8888, size) {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!("dawn/screencopy: create_buffer: {:?}", e);
            return None;
        }
    };
    let mut fb = match renderer.bind(&mut target) {
        Ok(fb) => fb,
        Err(e) => {
            tracing::warn!("dawn/screencopy: bind: {:?}", e);
            return None;
        }
    };

    // Свежий damage tracker на каждый снимок + age=0 → полная перерисовка:
    // буфер только что создан, никакой истории повреждений у него нет.
    let mut dt = OutputDamageTracker::from_output(output);
    if let Err(e) = dt.render_output(renderer, &mut fb, 0, elements, [0.1f32, 0.1, 0.1, 1.0]) {
        tracing::warn!("dawn/screencopy: render_output: {:?}", e);
        return None;
    }

    // Argb8888 читается как GL_BGRA_EXT — это расширение (EXT_read_format_bgra),
    // и оно есть не у всех драйверов. Запасной путь — прочитать нативный для
    // GLES Abgr8888 (GL_RGBA) и переставить R/B на CPU: медленнее на один
    // проход по кадру, но лучше, чем молча отдать зрителю чёрный экран.
    let (mapping, swap_rb) =
        match renderer.copy_framebuffer(&fb, Rectangle::from_size(size), Fourcc::Argb8888) {
            Ok(m) => (m, false),
            Err(e) => {
                tracing::debug!("dawn/screencopy: BGRA-чтение недоступно ({:?}), беру RGBA", e);
                match renderer.copy_framebuffer(&fb, Rectangle::from_size(size), Fourcc::Abgr8888) {
                    Ok(m) => (m, true),
                    Err(e) => {
                        tracing::warn!("dawn/screencopy: copy_framebuffer: {:?}", e);
                        return None;
                    }
                }
            }
        };
    // fb держит &mut target, а map_texture хочет &mut renderer — отпускаем.
    drop(fb);
    let mut pixels = match renderer.map_texture(&mapping) {
        Ok(data) => data.to_vec(),
        Err(e) => {
            tracing::warn!("dawn/screencopy: map_texture: {:?}", e);
            return None;
        }
    };
    if swap_rb {
        for px in pixels.chunks_exact_mut(4) {
            px.swap(0, 2);
        }
    }
    Some(pixels)
}

/// Переливает снятые пиксели в shm-буфер клиента с учётом его stride.
fn write_to_buffer(
    frame: &Frame,
    pixels: &[u8],
    size: Size<i32, BufferCoords>,
) -> Result<(), CaptureFailureReason> {
    let buffer = frame.buffer();
    let src_stride = size.w as usize * 4;
    let rows = size.h as usize;

    let res = with_buffer_contents_mut(&buffer, |ptr, len, data| {
        if data.width != size.w || data.height != size.h {
            return Err(CaptureFailureReason::BufferConstraints);
        }
        if shm_format_to_fourcc(data.format).is_none() {
            return Err(CaptureFailureReason::BufferConstraints);
        }
        let dst_stride = data.stride as usize;
        if dst_stride < src_stride || len < data.offset as usize + dst_stride * rows {
            return Err(CaptureFailureReason::BufferConstraints);
        }
        // SAFETY: with_buffer_contents_mut гарантирует, что ptr..ptr+len —
        // валидная отображённая память клиента; выше проверено, что все строки
        // помещаются в len с учётом offset и stride.
        unsafe {
            let base = ptr.add(data.offset as usize);
            for row in 0..rows {
                std::ptr::copy_nonoverlapping(
                    pixels.as_ptr().add(row * src_stride),
                    base.add(row * dst_stride),
                    src_stride,
                );
            }
        }
        Ok(())
    });

    match res {
        Ok(inner) => inner,
        Err(e) => {
            tracing::warn!("dawn/screencopy: буфер клиента не shm: {:?}", e);
            Err(CaptureFailureReason::BufferConstraints)
        }
    }
}

impl Dawn {
    /// Есть ли сейчас активные сессии захвата (идёт демонстрация экрана).
    pub fn screencast_active(&self) -> bool {
        self.capture_sessions.iter().any(|s| s.alive())
    }

    /// Сообщить всем сессиям новый размер буфера — после смены режима output'а.
    pub fn screencast_update_constraints(&mut self) {
        let mut sessions = std::mem::take(&mut self.capture_sessions);
        sessions.retain(|s| s.alive());
        for session in &sessions {
            let Some(output) = source_output(&session.source()) else { continue };
            let Some(mode) = output.current_mode() else { continue };
            session.update_constraints(BufferConstraints {
                size: mode.size.to_logical(1).to_buffer(1, Transform::Normal),
                shm: SHM_FORMATS.to_vec(),
                dma: None,
            });
        }
        self.capture_sessions = sessions;
        self.image_copy_capture_state.cleanup();
    }
}
