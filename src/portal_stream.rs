//! Поток кадров в PipeWire — то, что клиент (Discord, браузер) реально смотрит.
//!
//! Портал только договаривается: заводит сессию, спрашивает источник и отдаёт
//! клиенту НОМЕР НОДЫ. Сами кадры идут мимо D-Bus — через PipeWire, и эта нода
//! наша.
//!
//! Устройство. PipeWire живёт своим циклом, в отдельном потоке: смешивать его с
//! calloop нельзя, а рендер parallax однопоточный и останавливаться ради сети не
//! должен. Кадры уезжают туда `pipewire::channel` — он будит цикл PipeWire
//! изнутри, так что копирование в буфер происходит в его же потоке.
//!
//! Формат — BGRA без сжатия, ровно в том виде, в каком `screencopy::capture`
//! отдаёт снимок (Argb8888 в памяти это и есть байты B,G,R,A). Никакой
//! конверсии на нашей стороне: лишний проход по 11 МБ кадра на 2560×1080 стоил
//! бы дороже всего остального вместе взятого.

use std::sync::mpsc;

use pipewire::{
    context::Context,
    main_loop::MainLoop,
    properties::properties,
    spa::{
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::VideoFormat,
            ParamType,
        },
        pod::{self, ChoiceValue, Object, Pod, Property, Value},
        sys::{SPA_DATA_MemFd, SPA_DATA_MemPtr, SPA_PARAM_BUFFERS_align,
              SPA_PARAM_BUFFERS_blocks, SPA_PARAM_BUFFERS_buffers,
              SPA_PARAM_BUFFERS_dataType, SPA_PARAM_BUFFERS_size, SPA_PARAM_BUFFERS_stride},
        utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{Stream, StreamFlags, StreamState},
};

/// Живой поток: номер ноды для клиента и канал, по которому уезжают кадры.
pub struct Cast {
    /// Номер ноды PipeWire — его портал и отдаёт клиенту.
    pub node_id: u32,
    /// Размер кадра в пикселях; он зафиксирован на всё время сессии.
    pub width: u32,
    pub height: u32,
    /// Что снимаем — весь экран или окно (тогда кадр вырезается по нему).
    pub source: crate::portal::Capture,
    frames: pipewire::channel::Sender<Vec<u8>>,
    /// Когда отдали последний кадр — чтобы не гнать больше, чем просили.
    last_sent: std::time::Instant,
    /// Минимальный интервал между кадрами.
    interval: std::time::Duration,
    /// Когда последний раз печатали, насколько кадр не пустой.
    last_report: std::time::Instant,
}

impl Cast {
    /// Поднять ноду и дождаться её номера. `None`, если PipeWire недоступен —
    /// портал тогда честно ответит отказом вместо чёрного прямоугольника.
    pub fn start(
        width: u32,
        height: u32,
        fps: u32,
        source: crate::portal::Capture,
    ) -> Option<Cast> {
        let (frames_tx, frames_rx) = pipewire::channel::channel::<Vec<u8>>();
        let (id_tx, id_rx) = mpsc::channel::<u32>();

        std::thread::Builder::new()
            .name("plx-pipewire".into())
            .spawn(move || {
                if let Err(err) = serve(width, height, fps, frames_rx, id_tx) {
                    tracing::warn!("plx/pipewire: stream stopped: {}", err);
                }
            })
            .ok()?;

        // Номер ноды известен не сразу: он появляется, когда PipeWire примет
        // подключение и переведёт поток в Paused.
        match id_rx.recv_timeout(std::time::Duration::from_secs(5)) {
            Ok(node_id) => {
                tracing::info!(
                    "plx/pipewire: node {} ready, frame {}×{} @{} fps",
                    node_id, width, height, fps,
                );
                Some(Cast {
                    node_id,
                    width,
                    height,
                    source,
                    frames: frames_tx,
                    last_sent: std::time::Instant::now() - std::time::Duration::from_secs(1),
                    interval: std::time::Duration::from_micros(1_000_000 / fps.max(1) as u64),
                    last_report: std::time::Instant::now(),
                })
            }
            Err(_) => {
                tracing::warn!("plx/pipewire: the node did not come up within 5 s");
                None
            }
        }
    }

    /// Пора ли отдавать следующий кадр (ограничение по fps).
    pub fn due(&self) -> bool {
        self.last_sent.elapsed() >= self.interval
    }

    /// Отдать кадр. Пиксели должны быть BGRA, плотно упакованные,
    /// `width × height`.
    pub fn push(&mut self, pixels: Vec<u8>) {
        self.last_sent = std::time::Instant::now();

        // Раз в секунду — насколько кадр вообще НЕ чёрный.
        //
        // «Зритель видит чёрный экран» — это две совершенно разные беды:
        // либо мы сняли черноту (виноват рендер/захват), либо сняли картинку,
        // а до зрителя она не доехала (виноват формат буферов или их тип).
        // Различить их без этой строки нельзя, а искать при этом приходится
        // в разных концах кода.
        if self.last_report.elapsed() >= std::time::Duration::from_secs(1) {
            self.last_report = std::time::Instant::now();
            // Считаем по каждому 997-му пикселю — простое число, чтобы шаг не
            // попал в такт строке кадра и не мерил один и тот же столбец.
            let непустых = pixels
                .chunks_exact(4)
                .step_by(997)
                .filter(|px| px[0] > 8 || px[1] > 8 || px[2] > 8)
                .count();
            let всего = pixels.len() / 4 / 997 + 1;
            tracing::info!(
                "plx/cast: captured frame {} bytes, non-empty pixels {}/{} ({}%)",
                pixels.len(),
                непустых,
                всего,
                непустых * 100 / всего.max(1),
            );
        }

        // Ошибка отправки = поток умер; молчим, сессию закроет клиент.
        let _ = self.frames.send(pixels);
    }
}

fn serve(
    width: u32,
    height: u32,
    fps: u32,
    frames: pipewire::channel::Receiver<Vec<u8>>,
    id_tx: mpsc::Sender<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    pipewire::init();
    let mainloop = MainLoop::new(None)?;
    let context = Context::new(&mainloop)?;
    let core = context.connect(None)?;

    let stream = std::rc::Rc::new(Stream::new(
        &core,
        "plx-screencast",
        properties! {
            *pipewire::keys::MEDIA_TYPE => "Video",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Screen",
            *pipewire::keys::NODE_NAME => "plx-screencast",
        },
    )?);

    let stride = width as i32 * 4;
    let frame_size = stride * height as i32;

    let id_tx_state = id_tx.clone();
    let _listener = stream
        .add_local_listener_with_user_data(())
        .state_changed(move |stream, _, old, new| {
            tracing::debug!("plx/pipewire: state {:?} → {:?}", old, new);
            // Номер ноды осмыслен, только когда PipeWire её приняла.
            if matches!(new, StreamState::Paused | StreamState::Streaming) {
                let _ = id_tx_state.send(stream.node_id());
            }
        })
        .param_changed(move |stream, _, id, pod| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            if pod.is_none() {
                return;
            }
            // Формат согласован — объявляем, какие буферы нам подходят.
            //
            // MemFd, а НЕ MemPtr. MemPtr — это «указатель в моей памяти»: он
            // осмыслен только внутри одного процесса. Кадры уходят чужому
            // процессу (Discord, браузер), и делить с ним память можно лишь
            // через файловый дескриптор — MemFd. Пока здесь стоял один MemPtr,
            // потребитель получал буферы, отобразить которые у себя не мог, и
            // показывал чёрный прямоугольник вместо экрана. MemPtr оставлен
            // рядом вторым вариантом на случай потребителя в нашем же процессе.
            // DmaBuf был бы быстрее всех, но требует общего аллокатора — это
            // следующий шаг, а не первый.
            //
            // Собираем объект руками: макрос `property!` умеет только ключи с
            // `as_raw()` и типы из spa::utils, а Buffers описывается сырыми
            // константами и целочисленными Choice.
            let mem_fd = 1i32 << SPA_DATA_MemFd;
            let mem_ptr = 1i32 << SPA_DATA_MemPtr;
            let buffers = Object {
                type_: SpaTypes::ObjectParamBuffers.as_raw(),
                id: ParamType::Buffers.as_raw(),
                properties: vec![
                    Property::new(
                        SPA_PARAM_BUFFERS_buffers,
                        Value::Choice(ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Range { default: 3, min: 2, max: 8 },
                        ))),
                    ),
                    Property::new(SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
                    Property::new(SPA_PARAM_BUFFERS_size, Value::Int(frame_size)),
                    Property::new(SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
                    Property::new(SPA_PARAM_BUFFERS_align, Value::Int(16)),
                    Property::new(
                        SPA_PARAM_BUFFERS_dataType,
                        Value::Choice(ChoiceValue::Int(Choice(
                            ChoiceFlags::empty(),
                            ChoiceEnum::Flags {
                                default: mem_fd,
                                flags: vec![mem_fd | mem_ptr],
                            },
                        ))),
                    ),
                ],
            };
            let Ok(bytes) = pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &pod::Value::Object(buffers),
            ) else {
                return;
            };
            let bytes = bytes.0.into_inner();
            let Some(param) = Pod::from_bytes(&bytes) else { return };
            if let Err(err) = stream.update_params(&mut [param]) {
                tracing::warn!("plx/pipewire: update_params: {}", err);
            }
        })
        .register()?;

    // Что мы предлагаем: BGRA нужного размера с фиксированной частотой.
    let format = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::BGRA),
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle { width, height }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: fps, denom: 1 }
        ),
    );
    let bytes = pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pod::Value::Object(format),
    )?
    .0
    .into_inner();
    let param = Pod::from_bytes(&bytes).ok_or("could not build the format POD")?;

    stream.connect(
        Direction::Output,
        None,
        // DRIVER: темп задаём мы, кадры приходят от рендера, а не по запросу
        // клиента. MAP_BUFFERS: буферы нужны нам обычной памятью, чтобы просто
        // копировать в них.
        StreamFlags::DRIVER | StreamFlags::MAP_BUFFERS,
        &mut [param],
    )?;

    // Кадры из композитора: копируем в первый свободный буфер PipeWire.
    // Свободного нет — кадр роняем: показывать устаревший хуже, чем пропустить.
    let stream_frames = stream.clone();
    // Замыкание у `attach` — `Fn`, поэтому флаг «это первый кадр» держим в
    // ячейке, а не в захваченной переменной.
    let первый = std::cell::Cell::new(true);
    let _receiver = frames.attach(mainloop.loop_(), move |pixels: Vec<u8>| {
        let Some(mut buffer) = stream_frames.dequeue_buffer() else {
            tracing::trace!("plx/pipewire: no free buffer — frame dropped");
            return;
        };
        let datas = buffer.datas_mut();
        let Some(data) = datas.first_mut() else { return };
        // Что реально согласовалось — видно только по первому буферу.
        // «Чёрный экран у зрителя» и «буферы не того типа» с этой строкой
        // перестают быть неразличимы: MemPtr означает память, которую чужой
        // процесс отобразить у себя не может (см. объявление dataType выше).
        if первый.get() {
            первый.set(false);
            let тип = data.type_().as_raw();
            let есть_память = data.data().is_some();
            tracing::info!(
                "plx/pipewire: buffers of type {} ({}), mapped={}, expecting {} bytes, buffer {} bytes",
                тип,
                match тип {
                    x if x == SPA_DATA_MemPtr => "MemPtr — cannot be handed to another process",
                    x if x == SPA_DATA_MemFd => "MemFd",
                    _ => "other",
                },
                есть_память,
                frame_size,
                data.data().map(|d| d.len()).unwrap_or(0),
            );
        }
        if let Some(slice) = data.data() {
            let n = slice.len().min(pixels.len());
            slice[..n].copy_from_slice(&pixels[..n]);
        }
        let chunk = data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = stride;
        *chunk.size_mut() = frame_size as u32;
    });

    mainloop.run();
    Ok(())
}
