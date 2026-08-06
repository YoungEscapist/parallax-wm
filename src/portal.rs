//! Свой бэкенд xdg-desktop-portal — демонстрация экрана из самого dawn.
//!
//! Зачем свой. Портал устроен из двух половин: фронтенд
//! (`xdg-desktop-portal`, общий для всех) и бэкенд, который умеет разговаривать
//! с конкретным композитором. Фронтенд у нас поднимается и отвечает, а вот
//! бэкенда для dawn не было: единственный установленный,
//! `xdg-desktop-portal-hyprland`, снимает кадры через `zwlr_screencopy` и ходит
//! за списком окон в Hyprland IPC. dawn же отдаёт кадры по
//! `ext-image-copy-capture` (см. screencopy.rs) и никакого IPC Hyprland не
//! имеет — поэтому `AvailableSourceTypes` приходил нулём, список источников
//! оказывался пуст, и меню выбора окон не открывалось вовсе.
//!
//! Бэкенд живёт ВНУТРИ композитора намеренно: список окон, их эскизы и меню
//! выбора — это ровно то, что dawn уже рисует (обзор столов), а кадры он и так
//! готовит для screencopy. Отдельному процессу пришлось бы всё это спрашивать
//! по несуществующему IPC.
//!
//! Устройство. D-Bus живёт в своём потоке (zbus, блокирующий API): главный цикл
//! dawn — calloop и однопоточный, пускать в него асинхронный рантайм нельзя.
//! Поток общается с композитором двумя каналами: запрос уходит в calloop
//! (`channel::Sender`), ответ возвращается обычным `mpsc` — вызов D-Bus при этом
//! честно блокируется, как того и ждёт фронтенд.
//!
//! Что уже работает: регистрация имени, интерфейс ScreenCast, сессии, меню
//! выбора источника. Отдача самого потока кадров (PipeWire) — следующий шаг;
//! пока `Start` честно отвечает отказом, а не подсовывает пустой поток.

use std::collections::HashMap;
use std::sync::mpsc;

use smithay::reexports::calloop::channel;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

/// Имя бэкенда на шине. По нему фронтенд его и находит — оно же прописано в
/// `dawn.portal` (см. `install_portal_files`).
pub const BUS_NAME: &str = "org.freedesktop.impl.portal.desktop.dawn";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";

/// Типы источников из спецификации ScreenCast: 1 — монитор, 2 — окно.
const SOURCE_MONITOR: u32 = 1;
const SOURCE_WINDOW: u32 = 2;
/// Режимы курсора: 1 — скрыт, 2 — вшит в кадр.
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;

/// Ответ портала вызывающему (спецификация xdg-desktop-portal).
const RESPONSE_OK: u32 = 0;
const RESPONSE_CANCELLED: u32 = 1;
const RESPONSE_FAILED: u32 = 2;

/// Что пользователь выбрал в меню.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    /// Весь экран (у dawn один выход).
    Output,
    /// Конкретное окно — по индексу в `tagged_windows` на момент выбора.
    Window(usize),
}

/// Запрос из D-Bus-потока в композитор.
pub enum Request {
    /// Показать меню выбора источника. Ответ (или `None`, если отменили)
    /// уходит обратно в `reply`.
    Pick {
        app_id: String,
        types: u32,
        reply: mpsc::Sender<Option<Source>>,
    },
    /// Поднять поток кадров для выбранного источника и вернуть номер ноды
    /// PipeWire — его портал отдаёт клиенту.
    StartCast {
        reply: mpsc::Sender<Option<StreamInfo>>,
    },
    /// Клиент закрыл сессию — гасим поток.
    StopCast,
}

/// Что портал отдаёт клиенту про поднятый поток.
#[derive(Debug, Clone, Copy)]
pub struct StreamInfo {
    pub node_id: u32,
    pub width: u32,
    pub height: u32,
    pub source_type: u32,
}

/// Сессия захвата, какой её видит фронтенд. Живёт по своему пути на шине,
/// закрывается методом `Close` (или когда клиент отвалился).
struct Session {
    to_dawn: channel::Sender<Request>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.Session")]
impl Session {
    fn close(&self) {
        let _ = self.to_dawn.send(Request::StopCast);
        tracing::info!("dawn/portal: сессия закрыта");
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

/// Собственно бэкенд ScreenCast.
struct ScreenCast {
    to_dawn: channel::Sender<Request>,
    /// Что выбрано в каждой сессии: SelectSources спрашивает пользователя,
    /// Start потом отдаёт по этому выбору поток.
    selected: std::sync::Mutex<HashMap<OwnedObjectPath, Source>>,
}

#[zbus::interface(name = "org.freedesktop.impl.portal.ScreenCast")]
impl ScreenCast {
    /// Клиент собрался что-то показывать. Заводим объект сессии — с этого
    /// момента фронтенд общается с нами по её пути.
    async fn create_session(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        _options: HashMap<String, OwnedValue>,
        #[zbus(object_server)] server: &zbus::ObjectServer,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let session = Session { to_dawn: self.to_dawn.clone() };
        match server.at(&session_handle, session).await {
            Ok(_) => {
                tracing::info!("dawn/portal: новая сессия захвата для «{}»", app_id);
                (RESPONSE_OK, HashMap::new())
            }
            Err(err) => {
                tracing::warn!("dawn/portal: не завёл объект сессии: {}", err);
                (RESPONSE_FAILED, HashMap::new())
            }
        }
    }

    /// Здесь и показывается меню выбора: спецификация отводит под выбор
    /// источника именно SelectSources, а Start только отдаёт поток.
    async fn select_sources(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        app_id: String,
        options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        // Клиент говорит, что готов принять: маска типов источников. Обычный
        // Discord/браузер просит и монитор, и окно.
        let types = options.get("types")
            .and_then(|v| u32::try_from(v.clone()).ok())
            .unwrap_or(SOURCE_MONITOR | SOURCE_WINDOW);

        let (tx, rx) = mpsc::channel();
        let request = Request::Pick { app_id: app_id.clone(), types, reply: tx };
        if self.to_dawn.send(request).is_err() {
            tracing::warn!("dawn/portal: композитор не принял запрос выбора");
            return (RESPONSE_FAILED, HashMap::new());
        }
        // Ждём человека. Таймаут — чтобы висящее меню не держало вызов вечно.
        let choice = rx.recv_timeout(std::time::Duration::from_secs(120));
        match choice {
            Ok(Some(source)) => {
                tracing::info!("dawn/portal: выбран источник {:?} для «{}»", source, app_id);
                self.selected.lock().unwrap().insert(session_handle, source);
                (RESPONSE_OK, HashMap::new())
            }
            Ok(None) => {
                tracing::info!("dawn/portal: выбор отменён");
                (RESPONSE_CANCELLED, HashMap::new())
            }
            Err(_) => {
                tracing::warn!("dawn/portal: меню выбора не ответило за 120 с");
                (RESPONSE_CANCELLED, HashMap::new())
            }
        }
    }

    /// Отдать поток: поднимаем ноду PipeWire и возвращаем её номер. Дальше
    /// клиент забирает кадры уже из PipeWire, минуя D-Bus.
    async fn start(
        &self,
        _handle: OwnedObjectPath,
        session_handle: OwnedObjectPath,
        _app_id: String,
        _parent_window: String,
        _options: HashMap<String, OwnedValue>,
    ) -> (u32, HashMap<String, OwnedValue>) {
        let choice = self.selected.lock().unwrap().get(&session_handle).cloned();
        let Some(source) = choice else {
            tracing::warn!("dawn/portal: Start без выбранного источника");
            return (RESPONSE_FAILED, HashMap::new());
        };
        let (tx, rx) = mpsc::channel();
        if self.to_dawn.send(Request::StartCast { reply: tx }).is_err() {
            return (RESPONSE_FAILED, HashMap::new());
        }
        // Нода поднимается в своём потоке; ждём её номер.
        let info = match rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(Some(info)) => info,
            _ => {
                tracing::warn!("dawn/portal: поток не поднялся для {:?}", source);
                return (RESPONSE_FAILED, HashMap::new());
            }
        };

        // streams — массив (u, a{sv}): номер ноды и его свойства. Размер и тип
        // источника клиент показывает в своём интерфейсе.
        let mut props: HashMap<String, Value> = HashMap::new();
        props.insert("size".into(), Value::from((info.width as i32, info.height as i32)));
        props.insert("position".into(), Value::from((0i32, 0i32)));
        props.insert("source_type".into(), Value::from(info.source_type));
        // (ua{sv}) собираем структурой: кортеж с Dict внутрь Value напрямую
        // не заворачивается — Dict не реализует Type.
        let structure = zbus::zvariant::StructureBuilder::new()
            .add_field(info.node_id)
            .append_field(Value::from(zbus::zvariant::Dict::from(props)));
        let stream = match structure.build() {
            Ok(st) => Value::from(st),
            Err(err) => {
                tracing::warn!("dawn/portal: не собрал структуру потока: {}", err);
                return (RESPONSE_FAILED, HashMap::new());
            }
        };
        // Массив собираем с ЯВНОЙ сигнатурой элемента `(ua{sv})`. Из Vec<Value>
        // получился бы `av` (массив вариантов), и фронтенд портала такой ответ
        // отвергает — клиент видит просто «не удалось».
        let element = zbus::zvariant::Signature::try_from("(ua{sv})").expect("сигнатура потока");
        let mut array = zbus::zvariant::Array::new(&element);
        if let Err(err) = array.append(stream) {
            tracing::warn!("dawn/portal: streams: {}", err);
            return (RESPONSE_FAILED, HashMap::new());
        }
        let mut results = HashMap::new();
        match OwnedValue::try_from(Value::from(array)) {
            Ok(streams) => {
                results.insert("streams".to_string(), streams);
                tracing::info!(
                    "dawn/portal: поток отдан клиенту: нода {}, {}×{}",
                    info.node_id, info.width, info.height,
                );
                (RESPONSE_OK, results)
            }
            Err(err) => {
                tracing::warn!("dawn/portal: не собрал ответ streams: {}", err);
                (RESPONSE_FAILED, HashMap::new())
            }
        }
    }

    /// Что мы умеем показывать. Именно это значение фронтенд отдаёт клиенту как
    /// `AvailableSourceTypes`; ноль здесь означал бы «источников нет» — ровно
    /// то, что клиент видел без бэкенда.
    #[zbus(property, name = "AvailableSourceTypes")]
    fn available_source_types(&self) -> u32 {
        SOURCE_MONITOR | SOURCE_WINDOW
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn available_cursor_modes(&self) -> u32 {
        CURSOR_HIDDEN | CURSOR_EMBEDDED
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }
}

/// Поднять бэкенд в отдельном потоке. Возвращает `false`, если сессионной шины
/// нет (dawn запущен без dbus-run-session) — это не повод падать, просто
/// демонстрации экрана в такой сессии не будет.
pub fn spawn(to_dawn: channel::Sender<Request>) -> bool {
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none() {
        tracing::warn!(
            "dawn/portal: нет DBUS_SESSION_BUS_ADDRESS — портал не поднимаю \
             (запускайте через launch_native.sh, он поднимает шину)"
        );
        return false;
    }
    std::thread::Builder::new()
        .name("dawn-portal".into())
        .spawn(move || {
            if let Err(err) = serve(to_dawn) {
                tracing::warn!("dawn/portal: бэкенд остановлен: {}", err);
            }
        })
        .is_ok()
}

fn serve(to_dawn: channel::Sender<Request>) -> zbus::Result<()> {
    let backend = ScreenCast {
        to_dawn,
        selected: std::sync::Mutex::new(HashMap::new()),
    };
    // Имя берём ПОСЛЕ того, как объект выставлен: фронтенд, увидев имя на шине,
    // сразу спрашивает свойства, и объект обязан уже отвечать.
    let _conn = zbus::blocking::connection::Builder::session()?
        .serve_at(PORTAL_PATH, backend)?
        .name(BUS_NAME)?
        .build()?;
    tracing::info!("dawn/portal: бэкенд ScreenCast на шине как {}", BUS_NAME);
    // Соединение живёт, пока жив поток; zbus обслуживает вызовы своим
    // исполнителем в фоне.
    loop {
        std::thread::park();
    }
}

/// Разложить файлы, по которым фронтенд узнаёт про наш бэкенд.
///
/// `dawn.portal` объявляет, какие интерфейсы мы реализуем, а `dawn-portals.conf`
/// говорит, кого предпочесть для ScreenCast в сессии с
/// `XDG_CURRENT_DESKTOP=dawn`. Без второго файла фронтенд взял бы правило по
/// умолчанию (`default=*`) и мог уйти к hyprland-бэкенду, который с dawn
/// разговаривать не умеет.
pub fn install_portal_files() {
    let home = match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => h,
        _ => return,
    };
    let portal = format!(
        "[portal]\nDBusName={BUS_NAME}\n\
         Interfaces=org.freedesktop.impl.portal.ScreenCast;\n\
         UseIn=dawn;\n"
    );
    let conf = "[preferred]\n\
                default=dawn;gtk\n\
                org.freedesktop.impl.portal.ScreenCast=dawn\n";
    let files = [
        (format!("{home}/.local/share/xdg-desktop-portal/portals/dawn.portal"), portal),
        (format!("{home}/.config/xdg-desktop-portal/dawn-portals.conf"), conf.to_string()),
    ];
    for (path, body) in files {
        let path = std::path::PathBuf::from(path);
        if let Some(dir) = path.parent() {
            if let Err(err) = std::fs::create_dir_all(dir) {
                tracing::warn!("dawn/portal: {}: {}", dir.display(), err);
                continue;
            }
        }
        // Переписываем только при отличии: файл читает фронтенд при старте, и
        // трогать его на каждом запуске незачем.
        if std::fs::read_to_string(&path).ok().as_deref() == Some(body.as_str()) {
            continue;
        }
        if let Err(err) = std::fs::write(&path, &body) {
            tracing::warn!("dawn/portal: {}: {}", path.display(), err);
        } else {
            tracing::info!("dawn/portal: записан {}", path.display());
        }
    }
}

/// Значение для vardict — короче, чем расписывать конверсию на каждом месте.
#[allow(dead_code)]
fn v(value: impl Into<Value<'static>>) -> OwnedValue {
    OwnedValue::try_from(value.into()).expect("значение портала")
}

// ── Сторона композитора ──────────────────────────────────────────────────────

/// Идущий выбор источника: кому отвечать, когда пользователь ткнёт.
pub struct Pick {
    pub app_id: String,
    pub types: u32,
    reply: mpsc::Sender<Option<Source>>,
}

/// Что выбрано и будет показано: конкретное окно или весь экран.
#[derive(Clone)]
pub enum Capture {
    Output,
    Window(smithay::desktop::Window),
}

impl crate::state::Dawn {
    /// Пришёл запрос от бэкенда (он ждёт в своём потоке).
    pub fn handle_portal_request(&mut self, request: Request) {
        match request {
            Request::Pick { app_id, types, reply } => {
                // Второй запрос поверх первого: старый отменяем, иначе его
                // вызов провисит до таймаута.
                if let Some(prev) = self.portal_pick.take() {
                    let _ = prev.reply.send(None);
                }
                tracing::info!(
                    "dawn/portal: «{}» просит выбрать источник (типы={})", app_id, types,
                );
                self.portal_pick = Some(Pick { app_id, types, reply });
                self.request_redraw();
            }
            Request::StartCast { reply } => {
                let info = self.start_portal_cast();
                if info.is_none() {
                    tracing::warn!("dawn/portal: не поднял поток кадров");
                }
                let _ = reply.send(info);
            }
            Request::StopCast => {
                if self.portal_cast.take().is_some() {
                    tracing::info!("dawn/portal: поток кадров остановлен");
                }
                self.portal_capture = None;
            }
        }
    }

    /// Поднять поток кадров под выбранный источник.
    fn start_portal_cast(&mut self) -> Option<StreamInfo> {
        let source = self.portal_capture.clone()?;
        // Размер кадра фиксируется на всю сессию: PipeWire согласует формат
        // один раз. Для окна берём его экранный размер на момент старта.
        let (w, h, source_type) = match &source {
            Capture::Output => {
                let s = self.screen_size();
                (s.w as u32, s.h as u32, SOURCE_MONITOR)
            }
            Capture::Window(window) => {
                let zoom = self.viewport.zoom;
                let geo = self.space.element_geometry(window)?;
                (
                    ((geo.size.w as f64 * zoom).round() as u32).max(2),
                    ((geo.size.h as f64 * zoom).round() as u32).max(2),
                    SOURCE_WINDOW,
                )
            }
        };
        // Чётная ширина/высота: кодеки (и сам PipeWire) не любят нечётные
        // размеры, а Discord перекодирует поток в h264.
        let (w, h) = (w & !1, h & !1);
        let cast = crate::portal_stream::Cast::start(w, h, 30, source)?;
        let info = StreamInfo { node_id: cast.node_id, width: w, height: h, source_type };
        self.portal_cast = Some(cast);
        Some(info)
    }

    /// Клик во время выбора: окно под курсором — его и показываем, пустой
    /// холст — весь экран. Возвращает true, если клик съеден выбором.
    pub fn portal_pick_click(&mut self, cancel: bool) -> bool {
        let Some(pick) = self.portal_pick.take() else { return false };
        if cancel {
            let _ = pick.reply.send(None);
            self.request_redraw();
            return true;
        }
        let window = self.space.element_under(self.pointer_location).map(|(w, _)| w.clone());
        let source = match &window {
            Some(w) => {
                let idx = self.tagged_windows.iter().position(|tw| &tw.window == w).unwrap_or(0);
                Source::Window(idx)
            }
            None => Source::Output,
        };
        // Окно держим у себя: индекс в списке — только для протокола, а кадры
        // потом брать по самому окну.
        self.portal_capture = Some(match window {
            Some(w) => Capture::Window(w),
            None => Capture::Output,
        });
        let _ = pick.reply.send(Some(source));
        self.request_redraw();
        true
    }

    /// Идёт ли выбор источника прямо сейчас.
    pub fn portal_picking(&self) -> bool {
        self.portal_pick.is_some()
    }
}
