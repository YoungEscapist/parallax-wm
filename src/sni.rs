//! Трей приложений: StatusNotifierItem (SNI) — тот самый протокол, которым
//! Telegram, Vesktop, Steam и прочие показывают свой значок «в трее».
//!
//! Почему в самом композиторе — по той же причине, что блютуз и полка: в сессии
//! dawn нет панели, а свернувшийся в трей мессенджер иначе пропадает совсем.
//!
//! Как это устроено в мире. Приложение поднимает на СВОЕЙ шине объект
//! `org.kde.StatusNotifierItem` и зовёт `RegisterStatusNotifierItem` у
//! `org.kde.StatusNotifierWatcher`. Watcher — общесистемный «реестр», его
//! обычно держит панель. Значит, чтобы значки появились, dawn должен:
//!
//! 1. **взять имя watcher'а** и вести реестр;
//! 2. **объявить себя хостом** (`IsStatusNotifierHostRegistered = true`) — без
//!    этого Qt-приложения решают, что показывать значок некому, и уходят в
//!    старый XEmbed-трей, которого в вэйланде нет вовсе;
//! 3. читать у каждого предмета `IconPixmap` и рисовать его в панели.
//!
//! Если имя watcher'а УЖЕ занято (запущен waybar со своим треем), dawn не
//! отбирает его, а работает «только хостом»: регистрируется у чужого watcher'а
//! и читает его список. Иначе два реестра дрались бы за имя, и значки моргали
//! бы у обоих.
//!
//! Потоки. Шину обслуживает свой поток с блокирующим zbus — ровно как в
//! bluetooth.rs и wifi.rs, потому что главный цикл dawn однопоточный (calloop).
//! Сигналы шины ловят ещё два коротких потока (по одному на правило подписки) и
//! складывают их В ТОТ ЖЕ канал команд: так рабочий поток ждёт один источник, а
//! не крутит опрос вхолостую.
//!
//! Чего здесь НЕТ и почему. Меню предметов (`com.canonical.dbusmenu`) не
//! рисуется: это отдельное дерево пунктов со своими иконками и переключателями,
//! на него нужен свой рендер. Правый клик поэтому зовёт `ContextMenu` — и
//! приложение показывает СВОЁ окно меню (так делают Telegram и Vesktop).
//! Иконки берутся только из `IconPixmap` (готовые пиксели по шине); для
//! предметов, которые присылают лишь `IconName` (имя в теме значков), рисуется
//! кружок с первой буквой — читать PNG из темы означало бы тащить в dawn
//! распаковщик zlib.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel;
use zbus::zvariant::{OwnedValue, Value};

const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";
const WATCHER_PATH: &str = "/StatusNotifierWatcher";
const WATCHER_IFACE: &str = "org.kde.StatusNotifierWatcher";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
/// Путь объекта по умолчанию: приложение может зарегистрироваться, прислав
/// только имя своей шины, — тогда предмет лежит здесь.
const ITEM_PATH_DEFAULT: &str = "/StatusNotifierItem";

/// Сторона иконки в панели (физические пиксели). Иконку ужимаем на CPU один
/// раз при получении: рендер при сжатии берёт ближайший пиксель, и мелкий
/// значок превратился бы в кашу (та же грабля, что у масок в text.rs).
pub const ICON_PX: i32 = crate::bar::DOT;

/// Пересмотр списка на всякий случай: сигналы теряются (приложение упало, шина
/// перезапустилась), и раз в это время список сверяется целиком.
const RESCAN: Duration = Duration::from_secs(5);
/// Пауза перед повторной попыткой поднять шину.
const RETRY: Duration = Duration::from_secs(3);

// ── Что уходит в композитор ──────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Status {
    #[default]
    Active,
    /// Приложение считает значок неважным. Прячем такие же, как все прочие
    /// хосты: показываем, но приглушённее — пропадающий значок пугает сильнее.
    Passive,
    /// Просит внимания (непрочитанное) — рисуем точку в углу.
    Attention,
}

/// Готовые пиксели значка: premultiplied RGBA, уже ужатые до [`ICON_PX`].
#[derive(Clone, PartialEq)]
pub struct Icon {
    pub w: i32,
    pub h: i32,
    pub rgba: Vec<u8>,
}

impl std::fmt::Debug for Icon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Icon({}x{})", self.w, self.h)
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    /// Ключ предмета: «имя шины + путь». По нему композитор ищет иконку в кэше
    /// и по нему же шлёт команды обратно.
    pub key: String,
    /// `Id` приложения («telegram-desktop»); из него берётся буква для
    /// запасного значка.
    pub id: String,
    /// `Title`/`ToolTip` — показываем в подсказке.
    pub title: String,
    pub status: Status,
    /// None — приложение прислало только имя значка из темы (`icon_name`).
    pub icon: Option<Icon>,
    /// `IconName`/`AttentionIconName` — имя значка в теме. Раньше не читалось
    /// вовсе: рисовать по нему было нечем. Теперь по нему находится настоящий
    /// файл значка (см. icons.rs), и буква в кружке осталась только для тех,
    /// у кого нет ни пикселей, ни имени.
    pub icon_name: String,
    /// `IconThemePath` — свой каталог темы у приложения. Так делают
    /// Electron-программы, кладущие значок рядом с собой, а не в систему.
    pub icon_theme_path: String,
}

pub enum Event {
    /// Полный список — композитор просто заменяет им свой.
    Items(Vec<Item>),
}

// ── Что приходит от композитора и от шины ────────────────────────────────────

/// Сигналы шины идут ТЕМ ЖЕ каналом, что и команды: рабочий поток ждёт один
/// источник (`recv_timeout`), а не опрашивает несколько.
#[derive(Debug)]
pub enum Note {
    /// `RegisterStatusNotifierItem`: имя шины и путь объекта.
    Registered { service: String, path: String },
    /// У предмета что-то поменялось (NewIcon/NewTitle/NewStatus) — перечитать.
    Changed { service: String },
    /// У имени не стало владельца: приложение закрылось.
    Gone { service: String },
}

#[derive(Debug)]
pub enum Cmd {
    /// Левый клик по значку.
    Activate { key: String, x: i32, y: i32 },
    /// Правый клик — просим приложение показать своё меню.
    Context { key: String, x: i32, y: i32 },
    /// Средний клик.
    Secondary { key: String, x: i32, y: i32 },
    Bus(Note),
}

// ── Реестр (объект на шине) ──────────────────────────────────────────────────

/// Список зарегистрированных предметов в формате спецификации: «имя+путь»
/// одной строкой (`:1.42/StatusNotifierItem`). Его же отдаёт свойство
/// `RegisteredStatusNotifierItems`, на которое смотрят чужие хосты.
type Registry = Arc<Mutex<Vec<String>>>;

struct Watcher {
    registry: Registry,
    notes: mpsc::Sender<Cmd>,
}

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    /// Приложение просится в трей. Аргументом бывает и имя шины
    /// (`org.kde.StatusNotifierItem-1234-1`), и просто путь объекта (`/…`) —
    /// во втором случае именем служит отправитель сообщения. Обе формы
    /// встречаются в живых приложениях, и хост обязан понимать обе.
    fn register_status_notifier_item(
        &mut self,
        service: &str,
        #[zbus(header)] hdr: zbus::message::Header<'_>,
    ) {
        let sender = hdr.sender().map(|s| s.to_string()).unwrap_or_default();
        let (service, path) = if service.starts_with('/') {
            (sender, service.to_string())
        } else {
            (service.to_string(), ITEM_PATH_DEFAULT.to_string())
        };
        if service.is_empty() {
            return;
        }
        let ключ = format!("{service}{path}");
        if let Ok(mut reg) = self.registry.lock() {
            if !reg.contains(&ключ) {
                reg.push(ключ);
            }
        }
        tracing::info!("dawn/sni: предмет зарегистрирован: {}{}", service, path);
        let _ = self.notes.send(Cmd::Bus(Note::Registered { service, path }));
    }

    /// Хосты (панели) отмечаются здесь. Свой список мы не ведём: единственное,
    /// что от него зависит, — свойство ниже, а оно у нас всегда true.
    fn register_status_notifier_host(&mut self, service: &str) {
        tracing::debug!("dawn/sni: хост зарегистрирован: {}", service);
    }

    #[zbus(property)]
    fn registered_status_notifier_items(&self) -> Vec<String> {
        self.registry.lock().map(|r| r.clone()).unwrap_or_default()
    }

    /// Главное свойство протокола: пока оно false, Qt-приложения считают, что
    /// показывать значок некому, и не поднимают предмет вовсе.
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> bool {
        true
    }

    #[zbus(property)]
    fn protocol_version(&self) -> i32 {
        0
    }
}

// ── Поток ────────────────────────────────────────────────────────────────────

pub fn spawn(to_dawn: channel::Sender<Event>) -> Option<mpsc::Sender<Cmd>> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let свой = tx.clone();
    let ok = std::thread::Builder::new()
        .name("dawn-sni".into())
        .spawn(move || serve(to_dawn, rx, свой))
        .is_ok();
    ok.then_some(tx)
}

fn serve(to_dawn: channel::Sender<Event>, rx: mpsc::Receiver<Cmd>, notes: mpsc::Sender<Cmd>) {
    let registry: Registry = Arc::new(Mutex::new(Vec::new()));

    // Соединение поднимаем вместе с объектом реестра: сессионной шины может
    // ещё не быть (dawn стартует раньше dbus) — тогда ждём и пробуем снова.
    let conn = loop {
        let построено = zbus::blocking::connection::Builder::session().and_then(|b| {
            b.serve_at(
                WATCHER_PATH,
                Watcher { registry: registry.clone(), notes: notes.clone() },
            )
            .and_then(|b| b.build())
        });
        match построено {
            Ok(c) => break c,
            Err(err) => {
                tracing::warn!("dawn/sni: сессионная шина недоступна: {}", err);
                std::thread::sleep(RETRY);
                if matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                    return;
                }
            }
        }
    };

    // Имя реестра берём без вытеснения: если трей уже кто-то ведёт (waybar),
    // отбирать имя нельзя — оба реестра начали бы моргать.
    let свой_реестр = match conn.request_name_with_flags(
        WATCHER_NAME,
        zbus::fdo::RequestNameFlags::DoNotQueue.into(),
    ) {
        Ok(zbus::fdo::RequestNameReply::PrimaryOwner) => true,
        Ok(other) => {
            tracing::warn!("dawn/sni: {} уже занят ({:?}) — работаю только хостом", WATCHER_NAME, other);
            false
        }
        Err(err) => {
            tracing::warn!("dawn/sni: имя реестра не взято: {} — работаю только хостом", err);
            false
        }
    };

    // Хостом объявляемся в любом случае: и своему реестру (ради сигнала
    // StatusNotifierHostRegistered, которого ждут Qt-приложения), и чужому.
    let host_name = format!("org.kde.StatusNotifierHost-{}", std::process::id());
    let _ = conn.request_name_with_flags(
        host_name.as_str(),
        zbus::fdo::RequestNameFlags::DoNotQueue.into(),
    );
    if let Ok(p) = zbus::blocking::Proxy::new(&conn, WATCHER_NAME, WATCHER_PATH, WATCHER_IFACE) {
        let _ = p.call::<_, _, ()>("RegisterStatusNotifierHost", &(host_name.as_str(),));
    }
    if свой_реестр {
        // Сигнал ОБЯЗАН уйти: приложение, запущенное раньше dawn, ждёт именно
        // его, чтобы поднять свой предмет заново.
        let _ = conn.emit_signal(
            None::<&str>,
            WATCHER_PATH,
            WATCHER_IFACE,
            "StatusNotifierHostRegistered",
            &(),
        );
    }
    tracing::info!(
        "dawn/sni: трей поднят ({}), хост {}",
        if свой_реестр { "свой реестр" } else { "чужой реестр" },
        host_name,
    );

    watch_signals(&conn, notes.clone());

    let mut items: Vec<Item> = Vec::new();
    let mut известные: Vec<(String, String)> = Vec::new(); // (service, path)
    let mut next_rescan = Instant::now();

    loop {
        let ждать = next_rescan.saturating_duration_since(Instant::now());
        let cmd = match rx.recv_timeout(ждать) {
            Ok(cmd) => Some(cmd),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        };

        let mut надо_перечитать = false;
        match cmd {
            Some(Cmd::Bus(Note::Registered { service, path })) => {
                if !известные.iter().any(|(s, p)| *s == service && *p == path) {
                    известные.push((service, path));
                }
                надо_перечитать = true;
            }
            Some(Cmd::Bus(Note::Changed { service })) => {
                надо_перечитать = известные.iter().any(|(s, _)| *s == service);
            }
            Some(Cmd::Bus(Note::Gone { service })) => {
                let было = известные.len();
                известные.retain(|(s, _)| *s != service);
                if известные.len() != было {
                    if let Ok(mut reg) = registry.lock() {
                        reg.retain(|k| !k.starts_with(&service));
                    }
                    if свой_реестр {
                        let _ = conn.emit_signal(
                            None::<&str>,
                            WATCHER_PATH,
                            WATCHER_IFACE,
                            "StatusNotifierItemUnregistered",
                            &(service.as_str(),),
                        );
                    }
                    надо_перечитать = true;
                }
            }
            Some(cmd) => {
                run_cmd(&conn, &известные, cmd);
                continue;
            }
            None => {
                // Плановая сверка: подтягиваем чужой реестр (в режиме хоста) и
                // выбрасываем предметы, чьи приложения молча ушли.
                if !свой_реестр {
                    известные = чужой_реестр(&conn);
                } else if let Ok(reg) = registry.lock() {
                    известные = reg.iter().filter_map(|k| разобрать_ключ(k)).collect();
                }
                надо_перечитать = true;
                next_rescan = Instant::now() + RESCAN;
            }
        }

        if !надо_перечитать {
            continue;
        }
        let свежие: Vec<Item> = известные
            .iter()
            .filter_map(|(service, path)| read_item(&conn, service, path))
            .collect();
        // Предмет, который не отвечает, из списка убираем — иначе мёртвый
        // значок висел бы в панели до перезапуска.
        известные.retain(|(s, p)| {
            let key = format!("{s}{p}");
            свежие.iter().any(|i| i.key == key)
        });
        if свежие.len() != items.len()
            || свежие.iter().zip(items.iter()).any(|(a, b)| {
                a.key != b.key || a.title != b.title || a.status != b.status
                    || a.icon != b.icon || a.icon_name != b.icon_name
            })
        {
            items = свежие;
            tracing::debug!("dawn/sni: значков в трее: {}", items.len());
            if to_dawn.send(Event::Items(items.clone())).is_err() {
                return;
            }
        }
    }
}

/// Ключ реестра («:1.42/StatusNotifierItem») обратно в имя и путь.
fn разобрать_ключ(key: &str) -> Option<(String, String)> {
    let i = key.find('/')?;
    Some((key[..i].to_string(), key[i..].to_string()))
}

/// Список предметов у ЧУЖОГО реестра (режим «только хост»).
fn чужой_реестр(conn: &zbus::blocking::Connection) -> Vec<(String, String)> {
    let Ok(p) = zbus::blocking::Proxy::new(conn, WATCHER_NAME, WATCHER_PATH, PROPS_IFACE) else {
        return Vec::new();
    };
    let v: Result<OwnedValue, _> =
        p.call("Get", &(WATCHER_IFACE, "RegisteredStatusNotifierItems"));
    let Ok(v) = v else { return Vec::new() };
    Vec::<String>::try_from(v)
        .map(|list| list.iter().filter_map(|k| разобрать_ключ(k)).collect())
        .unwrap_or_default()
}

/// Два потока подписки: сигналы самих предметов и уход их владельцев с шины.
///
/// Почему потоками, а не опросом: иначе иконку пришлось бы перечитывать по
/// таймеру, а это десятки килобайт пикселей по шине каждые несколько секунд на
/// ровном месте.
fn watch_signals(conn: &zbus::blocking::Connection, notes: mpsc::Sender<Cmd>) {
    let правило_предметов = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(ITEM_IFACE)
        .ok()
        .map(|b| b.build());
    let правило_имён = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender("org.freedesktop.DBus")
        .ok()
        .and_then(|b| b.interface("org.freedesktop.DBus").ok())
        .and_then(|b| b.member("NameOwnerChanged").ok())
        .map(|b| b.build());

    if let Some(rule) = правило_предметов {
        let notes = notes.clone();
        if let Ok(iter) = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(64)) {
            let _ = std::thread::Builder::new()
                .name("dawn-sni-items".into())
                .spawn(move || {
                    for msg in iter {
                        let Ok(msg) = msg else { continue };
                        let Some(service) = msg.header().sender().map(|s| s.to_string()) else {
                            continue;
                        };
                        if notes.send(Cmd::Bus(Note::Changed { service })).is_err() {
                            return;
                        }
                    }
                });
        }
    }

    if let Some(rule) = правило_имён {
        if let Ok(iter) = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(64)) {
            let _ = std::thread::Builder::new()
                .name("dawn-sni-names".into())
                .spawn(move || {
                    for msg in iter {
                        let Ok(msg) = msg else { continue };
                        // (имя, прежний владелец, новый владелец); пустой новый
                        // владелец — имя освободилось.
                        let Ok((name, _old, new)) = msg.body().deserialize::<(String, String, String)>()
                        else {
                            continue;
                        };
                        if !new.is_empty() {
                            continue;
                        }
                        if notes.send(Cmd::Bus(Note::Gone { service: name })).is_err() {
                            return;
                        }
                    }
                });
        }
    }
}

fn run_cmd(conn: &zbus::blocking::Connection, известные: &[(String, String)], cmd: Cmd) {
    let (key, метод, x, y) = match cmd {
        Cmd::Activate { key, x, y } => (key, "Activate", x, y),
        Cmd::Context { key, x, y } => (key, "ContextMenu", x, y),
        Cmd::Secondary { key, x, y } => (key, "SecondaryActivate", x, y),
        Cmd::Bus(_) => return,
    };
    let Some((service, path)) = известные.iter().find(|(s, p)| format!("{s}{p}") == key) else {
        tracing::debug!("dawn/sni: {} — предмет {} уже ушёл", метод, key);
        return;
    };
    let Ok(p) = zbus::blocking::Proxy::new(conn, service.as_str(), path.as_str(), ITEM_IFACE)
    else {
        return;
    };
    if let Err(err) = p.call::<_, _, ()>(метод, &(x, y)) {
        tracing::debug!("dawn/sni: {} у {} не сработал: {}", метод, key, err);
    }
}

// ── Чтение предмета ──────────────────────────────────────────────────────────

fn read_item(conn: &zbus::blocking::Connection, service: &str, path: &str) -> Option<Item> {
    let props = zbus::blocking::Proxy::new(conn, service, path, PROPS_IFACE).ok()?;
    // Один вызов на все свойства. Часть приложений отвечает на GetAll ошибкой
    // (свой, неполный, объект) — тогда спрашиваем по одному.
    let all: HashMap<String, OwnedValue> = match props.call("GetAll", &(ITEM_IFACE,)) {
        Ok(all) => all,
        Err(_) => {
            let mut m = HashMap::new();
            for имя in [
                "Id", "Title", "Status", "IconPixmap", "AttentionIconPixmap",
                "IconName", "AttentionIconName", "IconThemePath",
            ] {
                if let Ok(v) = props.call::<_, _, OwnedValue>("Get", &(ITEM_IFACE, имя)) {
                    m.insert(имя.to_string(), v);
                }
            }
            // Ни одного свойства — значит объекта на шине уже нет.
            if m.is_empty() {
                return None;
            }
            m
        }
    };

    let строка = |ключ: &str| -> String {
        all.get(ключ)
            .and_then(|v| String::try_from(v.try_clone().ok()?).ok())
            .unwrap_or_default()
    };
    let id = строка("Id");
    let title = {
        let t = строка("Title");
        if t.is_empty() { id.clone() } else { t }
    };
    let status = match строка("Status").as_str() {
        "Passive" => Status::Passive,
        "NeedsAttention" => Status::Attention,
        _ => Status::Active,
    };
    // При «просит внимания» приложение обычно кладёт мигающий значок в
    // отдельное свойство — показываем именно его, иначе непрочитанное не видно.
    let сначала: &[&str] = if status == Status::Attention {
        &["AttentionIconPixmap", "IconPixmap"]
    } else {
        &["IconPixmap", "AttentionIconPixmap"]
    };
    let icon = сначала.iter().find_map(|ключ| {
        let v = all.get(*ключ)?;
        best_pixmap(v).map(|(w, h, argb)| fit_icon(&argb, w, h, ICON_PX))
    });
    // Имя значка берём в том же порядке, что и пиксели: «просит внимания» —
    // значит и имя нужно то, которое приложение считает тревожным.
    let имена: &[&str] = if status == Status::Attention {
        &["AttentionIconName", "IconName"]
    } else {
        &["IconName", "AttentionIconName"]
    };
    let icon_name = имена.iter()
        .map(|к| строка(к))
        .find(|s| !s.trim().is_empty())
        .unwrap_or_default();
    let icon_theme_path = строка("IconThemePath");

    Some(Item {
        key: format!("{service}{path}"),
        id, title, status, icon, icon_name, icon_theme_path,
    })
}

/// Выбирает из `a(iiay)` самый подходящий размер: не меньше [`ICON_PX`] и
/// поближе к нему (сжатие честнее растяжения), а если все мельче — самый
/// крупный.
fn best_pixmap(v: &OwnedValue) -> Option<(i32, i32, Vec<u8>)> {
    let Value::Array(arr) = &**v else { return None };
    let mut лучший: Option<(i32, i32, Vec<u8>)> = None;
    for el in arr.iter() {
        let Value::Structure(s) = el else { continue };
        let поля = s.fields();
        if поля.len() < 3 {
            continue;
        }
        let (Value::I32(w), Value::I32(h)) = (&поля[0], &поля[1]) else { continue };
        let (w, h) = (*w, *h);
        if w <= 0 || h <= 0 {
            continue;
        }
        let Ok(bytes) = Vec::<u8>::try_from(поля[2].try_clone().ok()?) else { continue };
        if bytes.len() < (w * h * 4) as usize {
            continue;
        }
        let лучше = match &лучший {
            None => true,
            Some((bw, _, _)) => {
                let подходит = |x: i32| x >= ICON_PX;
                match (подходит(w), подходит(*bw)) {
                    (true, false) => true,
                    (false, true) => false,
                    // Оба годятся (или оба мелкие): ближе к цели — тот, у кого
                    // меньше разница, среди мелких — тот, кто крупнее.
                    (true, true) => w < *bw,
                    (false, false) => w > *bw,
                }
            }
        };
        if лучше {
            лучший = Some((w, h, bytes));
        }
    }
    лучший
}

/// ARGB32 из шины (байты идут в СЕТЕВОМ порядке: A, R, G, B) → premultiplied
/// RGBA, ужатый усреднением по площади в квадрат `target` с сохранением
/// пропорций.
///
/// Усреднение, а не ближайший пиксель: значок 64×64, сжатый до 20×20 выборкой,
/// теряет тонкие линии — от иконки Telegram остаётся горсть точек.
pub fn fit_icon(argb: &[u8], sw: i32, sh: i32, target: i32) -> Icon {
    let масштаб = (target as f64 / sw.max(sh) as f64).min(1.0);
    let dw = ((sw as f64 * масштаб).round() as i32).clamp(1, target);
    let dh = ((sh as f64 * масштаб).round() as i32).clamp(1, target);
    let mut out = vec![0u8; (dw * dh * 4) as usize];

    for dy in 0..dh {
        // Границы исходного прямоугольника, который даёт этот пиксель.
        let y0 = (dy as i64 * sh as i64 / dh as i64) as i32;
        let y1 = (((dy + 1) as i64 * sh as i64 / dh as i64) as i32).max(y0 + 1).min(sh);
        for dx in 0..dw {
            let x0 = (dx as i64 * sw as i64 / dw as i64) as i32;
            let x1 = (((dx + 1) as i64 * sw as i64 / dw as i64) as i32).max(x0 + 1).min(sw);

            let (mut a, mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for y in y0..y1 {
                for x in x0..x1 {
                    let o = ((y * sw + x) * 4) as usize;
                    let Some(px) = argb.get(o..o + 4) else { continue };
                    // Пиксели на шине НЕ premultiplied — домножаем на альфу
                    // сами: GL-блендинг в smithay ждёт именно такой буфер.
                    let pa = px[0] as u32;
                    a += pa;
                    r += px[1] as u32 * pa / 255;
                    g += px[2] as u32 * pa / 255;
                    b += px[3] as u32 * pa / 255;
                    n += 1;
                }
            }
            if n == 0 {
                continue;
            }
            let o = ((dy * dw + dx) * 4) as usize;
            out[o] = (r / n) as u8;
            out[o + 1] = (g / n) as u8;
            out[o + 2] = (b / n) as u8;
            out[o + 3] = (a / n) as u8;
        }
    }
    Icon { w: dw, h: dh, rgba: out }
}

// ── Сторона композитора ──────────────────────────────────────────────────────

pub struct TrayApps {
    tx: mpsc::Sender<Cmd>,
    pub items: Vec<Item>,
    /// Готовые к отрисовке буферы значков по ключу предмета. Пересобираются
    /// ТОЛЬКО при смене картинки: у damage tracker состояние индексируется по
    /// Id буфера, и новый буфер каждый кадр повреждал бы экран целиком (см.
    /// заметку об Id в text.rs).
    buffers: HashMap<String, smithay::backend::renderer::element::memory::MemoryRenderBuffer>,
    /// Размер картинки в каждом буфере. Отдельным полем, потому что у значка,
    /// найденного в теме, `Item::icon` пустой — размер брать неоткуда.
    sizes: HashMap<String, (i32, i32)>,
}

impl TrayApps {
    /// Буфер значка вместе с его размером.
    pub fn buffer(
        &self,
        key: &str,
    ) -> Option<(&smithay::backend::renderer::element::memory::MemoryRenderBuffer, (i32, i32))> {
        let буфер = self.buffers.get(key)?;
        let размер = self.sizes.get(key).copied()?;
        Some((буфер, размер))
    }
}

impl crate::state::Dawn {
    pub fn init_sni(&mut self, tx: mpsc::Sender<Cmd>) {
        self.tray_apps = Some(TrayApps {
            tx, items: Vec::new(), buffers: HashMap::new(), sizes: HashMap::new(),
        });
    }

    pub fn handle_sni_event(&mut self, event: Event) {
        let Event::Items(items) = event;
        // Значки из темы достаём ДО заимствования трея: кэш лежит в том же
        // `self`, и одолжить обе половины разом нельзя.
        //
        // Приложение, приславшее только `IconName`, раньше получало букву в
        // кружке. Теперь по имени ищется настоящий файл значка (icons.rs), и
        // буква осталась ровно для тех, у кого нет ни пикселей, ни имени.
        // Поиск лезет в файловую систему, поэтому идёт ЗДЕСЬ — на смене
        // списка предметов, а не на каждый кадр, — и кэшируется по имени.
        let из_темы: Vec<(String, Option<Icon>)> = items.iter()
            .filter(|i| i.icon.is_none() && !i.icon_name.trim().is_empty())
            .map(|i| {
                // Свой каталог темы у приложения (Electron кладёт значок рядом
                // с собой): пробуем его первым, как полный путь.
                let свой = (!i.icon_theme_path.trim().is_empty()).then(|| {
                    format!("{}/{}.png", i.icon_theme_path.trim_end_matches('/'), i.icon_name)
                });
                let значок = свой
                    .and_then(|p| crate::icons::найти(&p, ICON_PX as u32))
                    .or_else(|| self.icon_cache.значок(&i.icon_name, ICON_PX as u32).cloned());
                (i.key.clone(), значок)
            })
            .collect();

        let Some(apps) = self.tray_apps.as_mut() else { return };
        // Буферы держим ровно под нынешний список: ушло приложение — ушла и
        // его текстура.
        apps.buffers.retain(|key, _| items.iter().any(|i| &i.key == key));
        for item in &items {
            let свой = item.icon.clone().or_else(|| {
                из_темы.iter()
                    .find(|(k, _)| k == &item.key)
                    .and_then(|(_, v)| v.clone())
            });
            let Some(icon) = свой else {
                apps.buffers.remove(&item.key);
                continue;
            };
            let прежний = apps.items.iter().find(|i| i.key == item.key);
            // Сравниваем и имя тоже: у предмета со значком ИЗ ТЕМЫ поле `icon`
            // пустое у обоих, и по одному ему смена значка не видна.
            let тот_же = прежний.is_some_and(|p| {
                p.icon == item.icon && p.icon_name == item.icon_name
            });
            if тот_же && apps.buffers.contains_key(&item.key) {
                continue;
            }
            apps.buffers.insert(
                item.key.clone(),
                smithay::backend::renderer::element::memory::MemoryRenderBuffer::from_slice(
                    &icon.rgba,
                    smithay::backend::allocator::Fourcc::Abgr8888,
                    (icon.w, icon.h),
                    1,
                    smithay::utils::Transform::Normal,
                    None,
                ),
            );
            // Размер запоминаем рядом с буфером: отрисовка центрирует значок в
            // ячейке по нему, а `item.icon` у значка из темы пустой.
            apps.sizes.insert(item.key.clone(), (icon.w, icon.h));
        }
        apps.sizes.retain(|key, _| apps.buffers.contains_key(key));
        apps.items = items;
        self.request_redraw();
    }

    pub fn sni_items(&self) -> &[Item] {
        self.tray_apps.as_ref().map(|a| a.items.as_slice()).unwrap_or(&[])
    }

    /// Клик по значку трея. `button` — код кнопки libinput (BTN_LEFT и т.д.).
    /// Координаты уходят приложению: по спецификации это точка, у которой оно
    /// должно показать своё меню.
    pub fn sni_click(&mut self, index: usize, right: bool, middle: bool, x: i32, y: i32) {
        let Some(apps) = self.tray_apps.as_ref() else { return };
        let Some(item) = apps.items.get(index) else { return };
        let key = item.key.clone();
        let cmd = if right {
            Cmd::Context { key, x, y }
        } else if middle {
            Cmd::Secondary { key, x, y }
        } else {
            Cmd::Activate { key, x, y }
        };
        tracing::info!("dawn/sni: {:?}", cmd);
        if apps.tx.send(cmd).is_err() {
            tracing::warn!("dawn/sni: поток трея не отвечает");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ключ реестра — это имя шины и путь, слепленные без разделителя. Разбор
    /// обязан вернуть их обратно: по нему композитор шлёт Activate, и ошибка
    /// здесь означала бы «клик по значку ничего не делает».
    #[test]
    fn ключ_реестра_разбирается_обратно() {
        assert_eq!(
            разобрать_ключ(":1.42/StatusNotifierItem"),
            Some((":1.42".into(), "/StatusNotifierItem".into())),
        );
        assert_eq!(
            разобрать_ключ("org.kde.StatusNotifierItem-1234-1/StatusNotifierItem"),
            Some(("org.kde.StatusNotifierItem-1234-1".into(), "/StatusNotifierItem".into())),
        );
        assert_eq!(разобрать_ключ("без-пути"), None);
    }

    /// Сжатие значка: 64×64 → 20×20 с усреднением. Проверяем и размер, и то,
    /// что цвет не потерялся, и premultiplied-альфу — на неё смотрит блендинг.
    #[test]
    fn значок_ужимается_с_усреднением() {
        let (sw, sh) = (64, 64);
        // Слева непрозрачный красный, справа полностью прозрачный.
        let mut argb = vec![0u8; (sw * sh * 4) as usize];
        for y in 0..sh {
            for x in 0..sw {
                let o = ((y * sw + x) * 4) as usize;
                if x < sw / 2 {
                    argb[o..o + 4].copy_from_slice(&[255, 255, 0, 0]); // A,R,G,B
                }
            }
        }
        let icon = fit_icon(&argb, sw, sh, 20);
        assert_eq!((icon.w, icon.h), (20, 20));
        assert_eq!(icon.rgba.len(), 20 * 20 * 4);

        let пиксель = |x: i32, y: i32| {
            let o = ((y * icon.w + x) * 4) as usize;
            (icon.rgba[o], icon.rgba[o + 1], icon.rgba[o + 2], icon.rgba[o + 3])
        };
        assert_eq!(пиксель(2, 10), (255, 0, 0, 255), "красная половина потерялась");
        assert_eq!(пиксель(17, 10), (0, 0, 0, 0), "прозрачная половина закрасилась");
    }

    /// Полупрозрачный пиксель обязан прийти домноженным на альфу: иначе он
    /// светится на экране ярче, чем задумано (классическая ошибка блендинга).
    #[test]
    fn альфа_домножается() {
        let argb = vec![128u8, 255, 255, 255]; // A=128, белый
        let icon = fit_icon(&argb, 1, 1, 20);
        assert_eq!(&icon.rgba[..4], &[128, 128, 128, 128]);
    }

    /// Значок мельче цели не растягиваем: 16×16 так и остаётся 16×16 (растянутый
    /// битмап выглядит хуже, чем честно мелкий).
    #[test]
    fn мелкий_значок_не_растягивается() {
        let argb = vec![255u8; (16 * 16 * 4) as usize];
        let icon = fit_icon(&argb, 16, 16, 20);
        assert_eq!((icon.w, icon.h), (16, 16));
    }

    /// Неквадратный значок сохраняет пропорции и влезает в квадрат панели.
    #[test]
    fn пропорции_сохраняются() {
        let argb = vec![255u8; (64 * 32 * 4) as usize];
        let icon = fit_icon(&argb, 64, 32, 20);
        assert_eq!((icon.w, icon.h), (20, 10));
    }

    // ── Проверка на живой шине ───────────────────────────────────────────────
    //
    // Протокол трея проверять больше негде: тут нет ни одной чистой функции —
    // есть разговор двух процессов. Поэтому поднимаем СВОЮ сессионную шину
    // (dbus-daemon), запускаем на ней настоящий поток трея и настоящее
    // приложение с настоящим значком, и смотрим, что дошло до композитора.
    // Сессия пользователя при этом не трогается вовсе.

    /// Своя шина на время теста; убивается вместе с проводником.
    struct Шина {
        адрес: String,
        процесс: std::process::Child,
    }

    impl Drop for Шина {
        fn drop(&mut self) {
            let _ = self.процесс.kill();
            let _ = self.процесс.wait();
        }
    }

    fn поднять_шину() -> Option<Шина> {
        use std::io::BufRead;
        let mut процесс = std::process::Command::new("dbus-daemon")
            .args(["--session", "--print-address", "--nofork"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .ok()?;
        let stdout = процесс.stdout.take()?;
        let mut строка = String::new();
        std::io::BufReader::new(stdout).read_line(&mut строка).ok()?;
        let адрес = строка.trim().to_string();
        if адрес.is_empty() {
            let _ = процесс.kill();
            return None;
        }
        Some(Шина { адрес, процесс })
    }

    /// Приложение со значком: тот самый объект, который поднимают Telegram и
    /// Vesktop. Значок — 2×2 красных пикселя, непрозрачных.
    struct ТестовыйЗначок {
        /// Куда пришёл Activate — по нему видно, что клик доехал и с теми
        /// координатами, которые отдал композитор.
        клик: Arc<Mutex<Option<(i32, i32)>>>,
    }

    #[zbus::interface(name = "org.kde.StatusNotifierItem")]
    impl ТестовыйЗначок {
        fn activate(&self, x: i32, y: i32) {
            if let Ok(mut к) = self.клик.lock() {
                *к = Some((x, y));
            }
        }

        #[zbus(property)]
        fn id(&self) -> String {
            "test-app".into()
        }
        #[zbus(property)]
        fn title(&self) -> String {
            "Тестовое приложение".into()
        }
        #[zbus(property)]
        fn status(&self) -> String {
            "Active".into()
        }
        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
            // ARGB, сетевой порядок байтов: A, R, G, B.
            let пиксели = std::iter::repeat([255u8, 255, 0, 0]).take(4).flatten().collect();
            vec![(2, 2, пиксели)]
        }
    }

    #[test]
    fn значок_приложения_доходит_до_композитора() {
        let Some(шина) = поднять_шину() else {
            eprintln!("dbus-daemon недоступен — проверку трея пропускаю");
            return;
        };
        // zbus читает адрес из окружения; свой на весь процесс теста — других
        // желающих ходить на шину среди тестов нет.
        unsafe { std::env::set_var("DBUS_SESSION_BUS_ADDRESS", &шина.адрес) };

        let (to_dawn, канал) = channel::channel::<Event>();
        let _tx = spawn(to_dawn).expect("поток трея не поднялся");

        // Ждём, пока трей возьмёт имя реестра: приложение, пришедшее раньше,
        // просто не найдёт, к кому обратиться.
        let клиент = zbus::blocking::Connection::session().expect("шина теста");
        let dbus = zbus::blocking::Proxy::new(
            &клиент, "org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus",
        )
        .expect("проводник шины");
        let срок = Instant::now() + Duration::from_secs(10);
        let mut поднялся = false;
        while Instant::now() < срок {
            if dbus.call::<_, _, bool>("NameHasOwner", &(WATCHER_NAME,)).unwrap_or(false) {
                поднялся = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(поднялся, "реестр {} не появился на шине", WATCHER_NAME);

        // Главное свойство протокола: без него Qt-приложения даже не пытаются
        // показать значок (см. заголовок модуля).
        let props = zbus::blocking::Proxy::new(&клиент, WATCHER_NAME, WATCHER_PATH, PROPS_IFACE)
            .expect("свойства реестра");
        let есть_хост: OwnedValue = props
            .call("Get", &(WATCHER_IFACE, "IsStatusNotifierHostRegistered"))
            .expect("свойство читается");
        assert_eq!(bool::try_from(есть_хост).ok(), Some(true));

        // Поднимаем приложение и регистрируемся.
        let клик: Arc<Mutex<Option<(i32, i32)>>> = Arc::new(Mutex::new(None));
        let приложение = zbus::blocking::connection::Builder::session()
            .and_then(|b| b.serve_at(ITEM_PATH_DEFAULT, ТестовыйЗначок { клик: клик.clone() }))
            .and_then(|b| b.build())
            .expect("объект приложения");
        let имя_приложения = приложение.unique_name().expect("имя на шине").to_string();
        zbus::blocking::Proxy::new(&клиент, WATCHER_NAME, WATCHER_PATH, WATCHER_IFACE)
            .expect("реестр")
            .call::<_, _, ()>("RegisterStatusNotifierItem", &(имя_приложения.as_str(),))
            .expect("регистрация значка");

        // Событие приходит каллупным каналом — крутим маленький цикл, как это
        // делает сам композитор.
        let mut итог: Vec<Item> = Vec::new();
        let mut loop_ = smithay::reexports::calloop::EventLoop::<Vec<Item>>::try_new()
            .expect("цикл событий");
        loop_
            .handle()
            .insert_source(канал, |event, _, итог: &mut Vec<Item>| {
                if let channel::Event::Msg(Event::Items(items)) = event {
                    *итог = items;
                }
            })
            .expect("источник событий");
        let срок = Instant::now() + Duration::from_secs(10);
        while итог.is_empty() && Instant::now() < срок {
            loop_
                .dispatch(Some(Duration::from_millis(100)), &mut итог)
                .expect("цикл крутится");
        }

        assert_eq!(итог.len(), 1, "значок не дошёл до композитора: {итог:?}");
        let значок = &итог[0];
        assert_eq!(значок.id, "test-app");
        assert_eq!(значок.title, "Тестовое приложение");
        assert_eq!(значок.status, Status::Active);
        assert_eq!(значок.key, format!("{имя_приложения}{ITEM_PATH_DEFAULT}"));
        let картинка = значок.icon.as_ref().expect("картинка значка");
        assert_eq!((картинка.w, картинка.h), (2, 2), "размер не тот: {картинка:?}");
        assert_eq!(&картинка.rgba[..4], &[255, 0, 0, 255], "цвет значка потерялся");

        // ── Клик по значку доходит до приложения ─────────────────────────────
        // Проверяем ровно то, что делает панель: шлём Activate по КЛЮЧУ, а
        // поток сам находит по нему имя и путь (см. run_cmd).
        _tx.send(Cmd::Activate { key: значок.key.clone(), x: 100, y: 34 })
            .expect("команда ушла");
        let срок = Instant::now() + Duration::from_secs(10);
        let mut дошло = None;
        while Instant::now() < срок {
            дошло = *клик.lock().unwrap();
            if дошло.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(дошло, Some((100, 34)), "клик не доехал до приложения");

        // ── Приложение закрылось — значок пропадает ──────────────────────────
        // Это путь через NameOwnerChanged: без него мёртвый значок висел бы в
        // панели до перезапуска композитора.
        drop(приложение);
        let срок = Instant::now() + Duration::from_secs(15);
        while !итог.is_empty() && Instant::now() < срок {
            loop_
                .dispatch(Some(Duration::from_millis(100)), &mut итог)
                .expect("цикл крутится");
        }
        assert!(итог.is_empty(), "значок ушедшего приложения остался: {итог:?}");
    }
}
