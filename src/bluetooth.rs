//! Блютуз внутри композитора: BlueZ по D-Bus, меню устройств и индикатор.
//!
//! Почему в самом dawn. Кроме dawn в сессии нет ничего, что рисовало бы
//! интерфейс: панели, трея и апплетов тут нет by design — рабочий стол это
//! бесконечный холст с окнами. Значит и «подключить наушники» должен уметь сам
//! композитор, иначе за этим приходится уходить в терминал с `bluetoothctl`.
//!
//! Устройство — как у портала (см. portal.rs): D-Bus живёт в СВОЁМ потоке
//! (zbus, блокирующий API), потому что главный цикл dawn — calloop и
//! однопоточный, асинхронному рантайму там места нет. Из потока в композитор
//! идут события каналом calloop, обратно — команды обычным `mpsc`.
//!
//! Два соединения с системной шиной, и это НЕ избыточность:
//! · рабочее — опрос состояния и команды (Connect, Pair, …);
//! · агентское — интерфейс `org.bluez.Agent1`, который BlueZ зовёт САМ, когда
//!   при сопряжении нужно подтвердить код. Обработчик агента обязан
//!   заблокироваться и ждать, пока человек нажмёт Enter, а zbus обслуживает
//!   вызовы соединения одним исполнителем: сиди мы на общем соединении, ответ
//!   на наш собственный `GetManagedObjects` не был бы обработан — взаимный
//!   клинч на всё время ожидания.
//!
//! Состояние берётся опросом `GetManagedObjects`, а не подпиской на сигналы:
//! опрос раз в 1.2 с (0.6 с во время поиска) — это единицы миллисекунд работы,
//! зато нет ни гонок при подписке, ни разъезда локальной копии с шиной.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue};

const BLUEZ: &str = "org.bluez";
const ADAPTER_IFACE: &str = "org.bluez.Adapter1";
const DEVICE_IFACE: &str = "org.bluez.Device1";
const BATTERY_IFACE: &str = "org.bluez.Battery1";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
const AGENT_PATH: &str = "/dawn/bluetooth/agent";

/// Как часто спрашиваем BlueZ о состоянии, когда меню ОТКРЫТО.
const POLL_IDLE: Duration = Duration::from_millis(1200);
const POLL_SCAN: Duration = Duration::from_millis(600);
/// …и когда на него никто не смотрит.
///
/// Раньше опрос шёл 1.2 с всегда: `GetManagedObjects` на всё дерево BlueZ плюс
/// разбор ответа 50 раз в минуту круглые сутки, и столько же строк в лог (в
/// сеансе 05.08.2026 это 652 строки из 741 — весь лог был про блютуз). Меню
/// закрыто — единственный, кому нужен свежий снимок, это автоподключение
/// наушников, и десяти секунд ему хватает с запасом.
const POLL_BG: Duration = Duration::from_secs(10);
/// Пауза перед повторной попыткой, когда bluetoothd не отвечает.
const RETRY: Duration = Duration::from_secs(5);
/// Сколько ждём ответа человека на подтверждение кода сопряжения.
const CONFIRM_TIMEOUT: Duration = Duration::from_secs(30);

// ── Что композитор знает об устройствах ──────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub path: String,
    pub address: String,
    /// Alias (его правит пользователь), иначе Name, иначе адрес.
    pub name: String,
    pub paired: bool,
    pub trusted: bool,
    pub connected: bool,
    /// Иконка BlueZ: audio-headset, input-mouse, phone… — по ней рисуем значок.
    pub icon: String,
    pub rssi: Option<i16>,
    pub battery: Option<u8>,
}

impl Device {
    /// Короткая пометка типа устройства для строки меню.
    pub fn kind(&self) -> &'static str {
        match self.icon.as_str() {
            i if i.starts_with("audio") => "audio",
            "input-mouse" => "mouse",
            "input-keyboard" => "kbd",
            "input-gaming" => "pad",
            "phone" => "phone",
            "computer" => "pc",
            _ => "",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Путь адаптера (`/org/bluez/hci0`); None — адаптера нет вовсе.
    pub adapter: Option<String>,
    pub powered: bool,
    pub discovering: bool,
    /// Сопряжённые сверху, дальше найденные при поиске; внутри групп —
    /// подключённые вперёд, потом по имени.
    pub devices: Vec<Device>,
}

impl Snapshot {
    pub fn connected_count(&self) -> usize {
        self.devices.iter().filter(|d| d.connected).count()
    }

    /// Самый низкий известный заряд среди подключённых — для индикатора.
    pub fn lowest_battery(&self) -> Option<u8> {
        self.devices.iter().filter(|d| d.connected).filter_map(|d| d.battery).min()
    }
}

/// Событие из потока D-Bus в композитор.
pub enum Event {
    /// Состояние изменилось (или пришёл первый снимок).
    State(Snapshot),
    /// BlueZ просит подтвердить код сопряжения. Пока ответ не придёт в `reply`,
    /// поток агента ждёт (не дольше CONFIRM_TIMEOUT).
    Confirm { name: String, passkey: u32, reply: mpsc::Sender<bool> },
    /// Строка для человека: результат команды или ошибка.
    Notice(String),
}

/// Команда из композитора в поток D-Bus.
#[derive(Debug)]
pub enum Cmd {
    Power(bool),
    Discovery(bool),
    Connect(String),
    Disconnect(String),
    Pair(String),
    Forget(String),
    /// Поднять адаптер и подключить устройство, которым пользовались последним.
    AutoConnect,
    /// На меню кто-то смотрит — опрашивать часто. Закрыли — редко (см. POLL_BG).
    Watch(bool),
}

// ── Поток D-Bus ──────────────────────────────────────────────────────────────

/// Поднять поток. `None` — системной шины нет; блютуза в такой сессии не будет,
/// но это не повод падать (ровно так же ведёт себя портал).
pub fn spawn(to_dawn: channel::Sender<Event>) -> Option<mpsc::Sender<Cmd>> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let ok = std::thread::Builder::new()
        .name("dawn-bluetooth".into())
        .spawn(move || serve(to_dawn, rx))
        .is_ok();
    ok.then_some(tx)
}

fn serve(to_dawn: channel::Sender<Event>, rx: mpsc::Receiver<Cmd>) {
    // Соединение может не подняться (нет системной шины) — тогда ждём и
    // пробуем снова: сессия могла стартовать раньше dbus.
    let conn = loop {
        match zbus::blocking::Connection::system() {
            Ok(c) => break c,
            Err(err) => {
                tracing::warn!("dawn/bt: системная шина недоступна: {}", err);
                // Команды всё это время копятся в канале; если композитор
                // умер, канал закроется и поток пора заканчивать.
                std::thread::sleep(RETRY);
                if matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)) {
                    return;
                }
            }
        }
    };

    register_agent(&to_dawn);

    let mut last: Option<Snapshot> = None;
    let mut next_poll = Instant::now();
    let mut warned_missing = false;
    // Открыто ли меню. Пока закрыто — опрашиваем раз в POLL_BG.
    let mut watching = false;

    loop {
        // Ждём команду ровно до следующего опроса — так поток спит, а не
        // крутится вхолостую.
        let wait = next_poll.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(Cmd::Watch(on)) => {
                watching = on;
                // Меню только что открыли — снимок нужен сразу, а не через
                // десять секунд.
                if on {
                    next_poll = Instant::now();
                }
                continue;
            }
            Ok(cmd) => {
                let snap = last.clone().unwrap_or_default();
                if let Err(err) = run_cmd(&conn, &snap, cmd, &to_dawn) {
                    let _ = to_dawn.send(Event::Notice(short_error(&err)));
                }
                // После команды состояние почти наверняка изменилось.
                next_poll = Instant::now();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }

        match read_state(&conn) {
            Ok(snap) => {
                warned_missing = false;
                next_poll = Instant::now()
                    + if snap.discovering {
                        POLL_SCAN
                    } else if watching {
                        POLL_IDLE
                    } else {
                        POLL_BG
                    };
                if last.as_ref() != Some(&snap) {
                    tracing::debug!(
                        "dawn/bt: состояние: адаптер={:?} powered={} устройств={} подключено={}",
                        snap.adapter, snap.powered, snap.devices.len(),
                        snap.devices.iter().filter(|d| d.connected).count(),
                    );
                    last = Some(snap.clone());
                    if to_dawn.send(Event::State(snap)).is_err() {
                        return;
                    }
                }
            }
            Err(err) => {
                next_poll = Instant::now() + RETRY;
                if !warned_missing {
                    warned_missing = true;
                    tracing::warn!("dawn/bt: BlueZ не отвечает: {}", err);
                    let _ = to_dawn.send(Event::Notice(
                        "bluetoothd is not running (sv up bluetoothd)".into(),
                    ));
                }
            }
        }
    }
}

/// Снимок состояния: один вызов `GetManagedObjects` на всё дерево BlueZ.
fn read_state(conn: &zbus::blocking::Connection) -> zbus::Result<Snapshot> {
    let om = zbus::blocking::Proxy::new(
        conn, BLUEZ, "/", "org.freedesktop.DBus.ObjectManager",
    )?;
    type Managed = HashMap<OwnedObjectPath, HashMap<String, HashMap<String, OwnedValue>>>;
    let objects: Managed = om.call("GetManagedObjects", &())?;

    let mut snap = Snapshot::default();
    // Адаптер: берём первый — второй BT-контроллер в машине большая редкость.
    for (path, ifaces) in &objects {
        if let Some(props) = ifaces.get(ADAPTER_IFACE) {
            snap.adapter = Some(path.to_string());
            snap.powered = get_bool(props, "Powered");
            snap.discovering = get_bool(props, "Discovering");
            break;
        }
    }

    for (path, ifaces) in &objects {
        let Some(props) = ifaces.get(DEVICE_IFACE) else { continue };
        let address = get_string(props, "Address").unwrap_or_default();
        let name = get_string(props, "Alias")
            .or_else(|| get_string(props, "Name"))
            .unwrap_or_else(|| address.clone());
        snap.devices.push(Device {
            path: path.to_string(),
            address,
            name,
            paired: get_bool(props, "Paired"),
            trusted: get_bool(props, "Trusted"),
            connected: get_bool(props, "Connected"),
            icon: get_string(props, "Icon").unwrap_or_default(),
            rssi: props.get("RSSI").and_then(|v| i16::try_from(v).ok()),
            battery: ifaces.get(BATTERY_IFACE)
                .and_then(|b| b.get("Percentage"))
                .and_then(|v| u8::try_from(v).ok()),
        });
    }

    // Строку пишем только когда снимок ИЗМЕНИЛСЯ (сравнение делает вызывающий,
    // см. serve): опрос идёт по таймеру, и безусловный debug! давал по строке
    // на каждый заход — в неподвижном сеансе это весь лог целиком.
    tracing::trace!(
        "dawn/bt: объектов={} адаптер={:?} powered={} устройств={}",
        objects.len(), snap.adapter, snap.powered, snap.devices.len(),
    );
    // Порядок в меню: подключённые, потом сопряжённые, потом остальные (у
    // найденных при поиске — по силе сигнала). Внутри группы — по имени, чтобы
    // строки не прыгали между опросами.
    snap.devices.sort_by(|a, b| {
        let rank = |d: &Device| if d.connected { 0 } else if d.paired { 1 } else { 2 };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.rssi.unwrap_or(-127).cmp(&a.rssi.unwrap_or(-127)))
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(snap)
}

fn get_bool(props: &HashMap<String, OwnedValue>, key: &str) -> bool {
    props.get(key).and_then(|v| bool::try_from(v).ok()).unwrap_or(false)
}

fn get_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    let s = String::try_from(props.get(key)?.clone()).ok()?;
    (!s.is_empty()).then_some(s)
}

fn run_cmd(
    conn: &zbus::blocking::Connection,
    snap: &Snapshot,
    cmd: Cmd,
    to_dawn: &channel::Sender<Event>,
) -> zbus::Result<()> {
    // Команда может прийти РАНЬШЕ первого опроса — так и происходит с
    // AutoConnect, которую композитор шлёт сразу на старте. Тогда переданный
    // снимок пустой, адаптера в нём нет, и команда молча ничего не делала:
    // именно поэтому автоподключение не срабатывало ни разу.
    let fresh;
    let snap = if snap.adapter.is_none() {
        fresh = read_state(conn)?;
        &fresh
    } else {
        snap
    };
    match cmd {
        Cmd::Power(on) => {
            let Some(adapter) = snap.adapter.clone() else {
                let _ = to_dawn.send(Event::Notice("no adapter".into()));
                return Ok(());
            };
            set_prop(conn, &adapter, ADAPTER_IFACE, "Powered", on)?;
            let _ = to_dawn.send(Event::Notice(
                if on { "adapter on" } else { "adapter off" }.into(),
            ));
        }
        Cmd::Discovery(on) => {
            let Some(adapter) = snap.adapter.clone() else { return Ok(()) };
            let proxy = proxy(conn, &adapter, ADAPTER_IFACE)?;
            // Повторный Start (или Stop, когда поиск уже стоит) BlueZ считает
            // ошибкой — она здесь ожидаема и в лицо человеку не нужна.
            let m = if on { "StartDiscovery" } else { "StopDiscovery" };
            if let Err(err) = proxy.call::<_, _, ()>(m, &()) {
                tracing::debug!("dawn/bt: {}: {}", m, err);
            }
        }
        Cmd::Connect(path) => {
            let name = device_name(snap, &path);
            let _ = to_dawn.send(Event::Notice(format!("{name}: connecting...")));
            // Trusted ставим до Connect: иначе после перезагрузки наушников
            // BlueZ будет спрашивать разрешение на каждый профиль заново.
            let _ = set_prop(conn, &path, DEVICE_IFACE, "Trusted", true);
            proxy(conn, &path, DEVICE_IFACE)?.call::<_, _, ()>("Connect", &())?;
            remember_last(&device_address(snap, &path));
            let _ = to_dawn.send(Event::Notice(format!("{name}: connected")));
        }
        Cmd::Disconnect(path) => {
            let name = device_name(snap, &path);
            proxy(conn, &path, DEVICE_IFACE)?.call::<_, _, ()>("Disconnect", &())?;
            let _ = to_dawn.send(Event::Notice(format!("{name}: disconnected")));
        }
        Cmd::Pair(path) => {
            let name = device_name(snap, &path);
            let _ = to_dawn.send(Event::Notice(format!("{name}: pairing...")));
            proxy(conn, &path, DEVICE_IFACE)?.call::<_, _, ()>("Pair", &())?;
            let _ = set_prop(conn, &path, DEVICE_IFACE, "Trusted", true);
            // Сопряжение само по себе звука в наушниках не даёт — сразу
            // подключаем, этого от «нажал Enter на устройстве» и ждут.
            let _ = proxy(conn, &path, DEVICE_IFACE)?.call::<_, _, ()>("Connect", &());
            remember_last(&device_address(snap, &path));
            let _ = to_dawn.send(Event::Notice(format!("{name}: paired")));
        }
        Cmd::Forget(path) => {
            let Some(adapter) = snap.adapter.clone() else { return Ok(()) };
            let name = device_name(snap, &path);
            let obj = ObjectPath::try_from(path.as_str())
                .map_err(|e| zbus::Error::Failure(e.to_string()))?;
            proxy(conn, &adapter, ADAPTER_IFACE)?
                .call::<_, _, ()>("RemoveDevice", &(obj,))?;
            let _ = to_dawn.send(Event::Notice(format!("{name}: forgotten")));
        }
        // Разбирается в serve (меняет темп опроса), сюда не доходит.
        Cmd::Watch(_) => {}
        Cmd::AutoConnect => {
            let Some(adapter) = snap.adapter.clone() else { return Ok(()) };
            if !snap.powered {
                set_prop(conn, &adapter, ADAPTER_IFACE, "Powered", true)?;
                // Устройства появляются в дереве не мгновенно после подъёма
                // адаптера — даём BlueZ вздохнуть, иначе Connect уйдёт в пустоту.
                std::thread::sleep(Duration::from_millis(700));
            }
            let Some(last) = load_last() else {
                tracing::debug!("dawn/bt: автоподключение: некого подключать");
                return Ok(());
            };
            // Перечитываем: снимок мог быть снят до подъёма адаптера, и тогда
            // списка устройств в нём ещё нет.
            let fresh = read_state(conn)?;
            let Some(dev) = fresh.devices.iter()
                .find(|d| d.address == last && d.paired && !d.connected)
            else {
                tracing::debug!("dawn/bt: автоподключение: {} не найдено или уже подключено", last);
                return Ok(());
            };
            tracing::info!("dawn/bt: автоподключение {} ({})", dev.name, dev.address);
            let _ = to_dawn.send(Event::Notice(format!("{}: connecting...", dev.name)));
            match proxy(conn, &dev.path, DEVICE_IFACE)?.call::<_, _, ()>("Connect", &()) {
                Ok(()) => {
                    let _ = to_dawn.send(Event::Notice(format!("{}: connected", dev.name)));
                }
                // Наушники в кейсе — обычное дело, руганью это сопровождать не надо.
                Err(err) => tracing::info!("dawn/bt: автоподключение не вышло: {}", err),
            }
        }
    }
    Ok(())
}

fn proxy<'a>(
    conn: &'a zbus::blocking::Connection,
    path: &str,
    iface: &'a str,
) -> zbus::Result<zbus::blocking::Proxy<'a>> {
    let path = ObjectPath::try_from(path.to_string())
        .map_err(|e| zbus::Error::Failure(e.to_string()))?;
    zbus::blocking::Proxy::new(conn, BLUEZ, path, iface)
}

fn set_prop(
    conn: &zbus::blocking::Connection,
    path: &str,
    iface: &str,
    key: &str,
    value: bool,
) -> zbus::Result<()> {
    let props = proxy(conn, path, PROPS_IFACE)?;
    props.call::<_, _, ()>("Set", &(iface, key, zbus::zvariant::Value::from(value)))
}

fn device_name(snap: &Snapshot, path: &str) -> String {
    snap.devices.iter()
        .find(|d| d.path == path)
        .map(|d| d.name.clone())
        .unwrap_or_else(|| "device".into())
}

fn device_address(snap: &Snapshot, path: &str) -> String {
    snap.devices.iter()
        .find(|d| d.path == path)
        .map(|d| d.address.clone())
        .unwrap_or_default()
}

/// Ошибку BlueZ — на человеческий. Коды вроде `br-connection-page-timeout`
/// точны, но в меню от них толку нет: человеку надо знать, что наушники лежат
/// выключенные, а не какой этап установления канала не прошёл.
fn short_error(err: &zbus::Error) -> String {
    let text = err.to_string();
    let tail = match text.rsplit_once(": ") {
        Some((_, t)) if !t.is_empty() => t,
        _ => text.as_str(),
    };
    let known = [
        ("page-timeout", "device not responding: is it on and nearby?"),
        ("connection-unknown", "device not responding: is it on and nearby?"),
        ("connection-canceled", "connection aborted"),
        ("connection-timeout", "device timed out"),
        ("profile-unavailable", "profile unavailable"),
        ("Protocol not available", "profile not supported"),
        ("Already Connected", "already connected"),
        ("Already Exists", "already paired"),
        ("In Progress", "already in progress"),
        ("Authentication Failed", "pairing rejected"),
        ("Authentication Canceled", "pairing cancelled"),
        ("Device or resource busy", "device busy"),
        ("Not Ready", "adapter not ready"),
        ("No such device", "device is gone"),
        ("Does Not Exist", "device is gone"),
    ];
    for (needle, human) in known {
        if tail.contains(needle) {
            return human.to_string();
        }
    }
    tail.to_string()
}

// ── Кто подключался последним ────────────────────────────────────────────────

fn state_path() -> Option<std::path::PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(std::path::PathBuf::from(home).join(".local/state/dawn/bluetooth"))
}

/// Запомнить адрес устройства, к которому подключались вручную — по нему потом
/// работает автоподключение при старте сессии.
fn remember_last(address: &str) {
    if address.is_empty() {
        return;
    }
    let Some(path) = state_path() else { return };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(err) = std::fs::write(&path, address) {
        tracing::warn!("dawn/bt: {}: {}", path.display(), err);
    }
}

fn load_last() -> Option<String> {
    let text = std::fs::read_to_string(state_path()?).ok()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some(text)
}

// ── Агент сопряжения ─────────────────────────────────────────────────────────

/// Реализация `org.bluez.Agent1`: BlueZ зовёт её, когда при сопряжении нужно
/// участие человека.
struct Agent {
    to_dawn: channel::Sender<Event>,
}

#[zbus::interface(name = "org.bluez.Agent1")]
impl Agent {
    fn release(&self) {}

    /// Числовое сравнение (Secure Simple Pairing): на экране устройства и у нас
    /// один и тот же код, человек подтверждает совпадение.
    fn request_confirmation(&self, device: OwnedObjectPath, passkey: u32) -> zbus::fdo::Result<()> {
        let (tx, rx) = mpsc::channel();
        let name = device.as_str().rsplit('/').next().unwrap_or("device")
            .trim_start_matches("dev_").replace('_', ":");
        if self.to_dawn.send(Event::Confirm { name, passkey, reply: tx }).is_err() {
            return Err(zbus::fdo::Error::Failed("compositor is not responding".into()));
        }
        match rx.recv_timeout(CONFIRM_TIMEOUT) {
            Ok(true) => Ok(()),
            Ok(false) => Err(zbus::fdo::Error::AuthFailed("rejected".into())),
            Err(_) => Err(zbus::fdo::Error::AuthFailed("timed out".into())),
        }
    }

    /// Устройству нечем показать код — оно просто просит разрешения.
    fn request_authorization(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<()> {
        let (tx, rx) = mpsc::channel();
        let _ = self.to_dawn.send(Event::Confirm {
            name: "pairing".into(), passkey: 0, reply: tx,
        });
        match rx.recv_timeout(CONFIRM_TIMEOUT) {
            Ok(true) => Ok(()),
            _ => Err(zbus::fdo::Error::AuthFailed("rejected".into())),
        }
    }

    /// Профиль на уже сопряжённом устройстве (наушники просят A2DP и HFP
    /// отдельно). Спрашивать про каждый — только злить: раз устройство
    /// доверенное, сервисы разрешаем молча.
    fn authorize_service(&self, _device: OwnedObjectPath, _uuid: String) -> zbus::fdo::Result<()> {
        Ok(())
    }

    /// Код показывает НАМ устройство, набирать его надо на нём — показываем
    /// строкой в меню.
    fn display_passkey(&self, _device: OwnedObjectPath, passkey: u32, _entered: u16) {
        let _ = self.to_dawn.send(Event::Notice(format!("code on the device: {passkey:06}")));
    }

    fn display_pin_code(&self, _device: OwnedObjectPath, pincode: String) -> zbus::fdo::Result<()> {
        let _ = self.to_dawn.send(Event::Notice(format!("PIN on the device: {pincode}")));
        Ok(())
    }

    /// Ввода цифр в меню нет: клавиатура во время сопряжения принадлежит меню,
    /// а не строке ввода. Устройства, которым нужен набранный PIN (старые
    /// гарнитуры с «0000»), придётся сопрягать через bluetoothctl.
    fn request_pin_code(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<String> {
        Err(zbus::fdo::Error::NotSupported("PIN entry needed - use bluetoothctl".into()))
    }

    fn request_passkey(&self, _device: OwnedObjectPath) -> zbus::fdo::Result<u32> {
        Err(zbus::fdo::Error::NotSupported("passkey entry needed - use bluetoothctl".into()))
    }

    fn cancel(&self) {
        let _ = self.to_dawn.send(Event::Notice("pairing cancelled by the device".into()));
    }
}

/// Поднять агента на ОТДЕЛЬНОМ соединении (см. шапку модуля) и объявить его
/// агентом по умолчанию.
fn register_agent(to_dawn: &channel::Sender<Event>) {
    let to_dawn = to_dawn.clone();
    let spawned = std::thread::Builder::new()
        .name("dawn-bt-agent".into())
        .spawn(move || {
            let conn = match zbus::blocking::connection::Builder::system()
                .and_then(|b| b.serve_at(AGENT_PATH, Agent { to_dawn }))
                .and_then(|b| b.build())
            {
                Ok(c) => c,
                Err(err) => {
                    tracing::warn!("dawn/bt: агент не поднялся: {}", err);
                    return;
                }
            };
            // AgentManager появляется вместе с bluetoothd — если его ещё нет,
            // пробуем снова, пока не выйдет.
            loop {
                let ok = zbus::blocking::Proxy::new(
                    &conn, BLUEZ, "/org/bluez", "org.bluez.AgentManager1",
                )
                .and_then(|m| {
                    let path = ObjectPath::try_from(AGENT_PATH).expect("путь агента");
                    // KeyboardDisplay: у нас есть чем показать код и чем
                    // подтвердить — тогда BlueZ выбирает числовое сравнение.
                    m.call::<_, _, ()>("RegisterAgent", &(&path, "KeyboardDisplay"))?;
                    m.call::<_, _, ()>("RequestDefaultAgent", &(&path,))
                });
                match ok {
                    Ok(()) => {
                        tracing::info!("dawn/bt: агент сопряжения зарегистрирован");
                        break;
                    }
                    Err(err) => tracing::debug!("dawn/bt: агент ждёт bluetoothd: {}", err),
                }
                std::thread::sleep(RETRY);
            }
            loop {
                std::thread::park();
            }
        })
        .is_ok();
    if !spawned {
        tracing::warn!("dawn/bt: не удалось создать поток агента");
    }
}

// ── Сторона композитора ──────────────────────────────────────────────────────

/// Строка меню и её место на экране — рендер заполняет, ввод по ней попадает.
#[derive(Clone, Copy, Debug)]
pub struct Row {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Индекс устройства в снимке.
    pub device: usize,
}

/// Что делает кнопка в подвале меню.
///
/// Раньше подвал был ОДНОЙ строкой текста — «Enter connect  D disconnect …» —
/// то есть шпаргалкой, а не управлением: мышь по ней не работала вовсе (в
/// `bt_click` разбирались только строки устройств, всё остальное считалось
/// «мимо меню» и меню просто закрывалось). Отсюда и «кнопки не работают»:
/// нажимать было нечего, там был текст.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Enter по выбранному: сопрячь / подключить / отключить — по состоянию.
    Activate,
    /// D — отключить, не забывая сопряжение.
    Disconnect,
    /// F — забыть устройство.
    Forget,
    /// S — начать или остановить поиск.
    Scan,
    /// P — питание адаптера.
    Power,
    /// Esc — закрыть меню.
    Close,
}

/// Кнопка подвала: прямоугольник на экране, действие и то, как её рисовать.
#[derive(Clone, Debug)]
pub struct Button {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub action: Action,
    /// Подпись клавиши («Enter», «P», …) — рисуется акцентом слева.
    pub key: &'static str,
    /// Что кнопка делает сейчас: «connect» / «disconnect» / «power off»…
    pub label: String,
    /// Действие сейчас невозможно (нет устройства, адаптер выключен) —
    /// кнопка тусклая и клик по ней ничего не делает.
    pub enabled: bool,
}

impl Button {
    pub fn hit(&self, x: f64, y: f64) -> bool {
        x >= self.x as f64
            && x < (self.x + self.w) as f64
            && y >= self.y as f64
            && y < (self.y + self.h) as f64
    }
}

/// Всё, что композитор знает о блютузе: последний снимок, канал команд и
/// состояние меню.
pub struct BtUi {
    pub tx: mpsc::Sender<Cmd>,
    pub snap: Snapshot,
    pub open: bool,
    /// Выбранная строка (индекс устройства в снимке).
    pub sel: usize,
    /// Подсказка внизу меню: результат последней команды.
    pub notice: Option<(String, Instant)>,
    /// Идущее подтверждение кода сопряжения — пока оно есть, меню показывает
    /// код, а Enter/Esc отвечают BlueZ, а не двигают выбор.
    pub confirm: Option<Confirm>,
    /// Геометрия строк последнего кадра (экранные пиксели) — для мыши.
    pub rows: Vec<Row>,
    /// Кнопки подвала последнего кадра — тоже для мыши (см. Button).
    pub buttons: Vec<Button>,
}

pub struct Confirm {
    pub name: String,
    pub passkey: u32,
    pub reply: mpsc::Sender<bool>,
}

/// Сколько показываем подсказку о результате команды.
const NOTICE_TTL: Duration = Duration::from_secs(6);

impl BtUi {
    fn new(tx: mpsc::Sender<Cmd>) -> Self {
        Self {
            tx,
            snap: Snapshot::default(),
            open: false,
            sel: 0,
            notice: None,
            confirm: None,
            rows: Vec::new(),
            buttons: Vec::new(),
        }
    }

    /// Текст подсказки, если она ещё не протухла.
    pub fn notice_text(&self) -> Option<&str> {
        self.notice.as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_TTL)
            .map(|(t, _)| t.as_str())
    }

    pub fn selected(&self) -> Option<&Device> {
        self.snap.devices.get(self.sel)
    }

    /// Из чего собрать подвал: действие, подпись клавиши, подпись действия и
    /// доступно ли оно СЕЙЧАС.
    ///
    /// Одно место на всё: отрисовка берёт отсюда и подписи, и активность, а
    /// `bt_action` выполняет ровно те же действия по тем же правилам. Пока
    /// подвал был текстовой строкой, подписи жили отдельно от поведения и
    /// разъезжались с ним — строка обещала «D disconnect» и для неподключённого
    /// устройства.
    pub fn button_specs(&self) -> Vec<(Action, &'static str, String, bool)> {
        let dev = self.selected();
        let powered = self.snap.powered;
        let has_adapter = self.snap.adapter.is_some();

        // Что сделает Enter — зависит от состояния выбранного устройства, ровно
        // как в bt_activate.
        let (activate_label, activate_on) = match dev {
            Some(d) if d.connected => ("disconnect", true),
            Some(d) if d.paired => ("connect", true),
            Some(_) => ("pair", true),
            None => ("connect", false),
        };

        vec![
            (
                Action::Activate,
                "Enter",
                activate_label.to_string(),
                activate_on && powered,
            ),
            (
                Action::Disconnect,
                "D",
                "disconnect".to_string(),
                dev.is_some_and(|d| d.connected),
            ),
            (
                Action::Forget,
                "F",
                "forget".to_string(),
                dev.is_some_and(|d| d.paired),
            ),
            (
                Action::Scan,
                "S",
                if self.snap.discovering { "stop scan" } else { "scan" }.to_string(),
                powered,
            ),
            (
                Action::Power,
                "P",
                if powered { "power off" } else { "power on" }.to_string(),
                has_adapter,
            ),
            (Action::Close, "Esc", "close".to_string(), true),
        ]
    }
}

impl crate::state::Dawn {
    /// Поднять блютуз-поток. Зовётся один раз при старте; если шины нет,
    /// `bt` останется None и всё меню просто не откроется.
    pub fn init_bluetooth(&mut self, tx: mpsc::Sender<Cmd>, autoconnect: bool) {
        if autoconnect {
            let _ = tx.send(Cmd::AutoConnect);
        }
        self.bt = Some(BtUi::new(tx));
    }

    /// Событие из потока D-Bus.
    pub fn handle_bluetooth_event(&mut self, event: Event) {
        let Some(bt) = self.bt.as_mut() else { return };
        match event {
            Event::State(snap) => {
                // Выбор держим на ТОМ ЖЕ устройстве, а не на том же номере
                // строки: список пересортировывается (подключилось — уехало
                // наверх), и иначе выделение перескакивало бы само.
                let keep = bt.snap.devices.get(bt.sel).map(|d| d.path.clone());
                bt.snap = snap;
                bt.sel = keep
                    .and_then(|p| bt.snap.devices.iter().position(|d| d.path == p))
                    .unwrap_or(bt.sel)
                    .min(bt.snap.devices.len().saturating_sub(1));
            }
            Event::Confirm { name, passkey, reply } => {
                bt.confirm = Some(Confirm { name, passkey, reply });
                // Спрашивать «подтвердите код» на невидимом меню бессмысленно.
                bt.open = true;
            }
            Event::Notice(text) => {
                tracing::info!("dawn/bt: {}", text);
                bt.notice = Some((text, Instant::now()));
            }
        }
        self.request_redraw();
    }

    /// Открыть/закрыть меню (действие `bluetooth_menu`).
    pub fn bt_toggle_menu(&mut self) {
        let Some(bt) = self.bt.as_mut() else {
            tracing::warn!("dawn/bt: блютуз не поднят (нет системной шины?)");
            return;
        };
        bt.open = !bt.open;
        // Пока меню открыто, опрос идёт часто, закрыли — редко (см. POLL_BG).
        let _ = bt.tx.send(Cmd::Watch(bt.open));
        // А вот при закрытии гасим поиск — в фоне он жрёт эфир и батарею
        // наушников — и отпускаем растеризованные строки: имена и проценты всё
        // равно поменяются к следующему открытию.
        if !bt.open {
            if bt.snap.discovering {
                let _ = bt.tx.send(Cmd::Discovery(false));
            }
            self.text_cache.clear();
        }
        self.request_redraw();
    }

    pub fn bt_menu_open(&self) -> bool {
        self.bt.as_ref().is_some_and(|b| b.open)
    }

    /// Клавиша при открытом меню. `true` — клавиша съедена меню.
    pub fn bt_key(&mut self, keysym: u32) -> bool {
        use smithay::input::keyboard::keysyms;
        if !self.bt_menu_open() {
            return false;
        }
        // Подтверждение кода перехватывает Enter/Esc целиком.
        if self.bt.as_ref().is_some_and(|b| b.confirm.is_some()) {
            let yes = matches!(keysym, keysyms::KEY_Return | keysyms::KEY_KP_Enter | keysyms::KEY_y);
            let no = matches!(keysym, keysyms::KEY_Escape | keysyms::KEY_n);
            if yes || no {
                if let Some(bt) = self.bt.as_mut() {
                    if let Some(c) = bt.confirm.take() {
                        let _ = c.reply.send(yes);
                    }
                }
                self.request_redraw();
            }
            return true;
        }

        // Клавиши и кнопки подвала делают одно и то же одним и тем же кодом
        // (см. bt_action и BtUi::button_specs) — подпись на кнопке не может
        // разойтись с тем, что произойдёт по её клавише.
        let count = self.bt.as_ref().map(|b| b.snap.devices.len()).unwrap_or(0);
        match keysym {
            keysyms::KEY_Escape => self.bt_action(Action::Close),
            keysyms::KEY_Down | keysyms::KEY_j => self.bt_move_sel(1, count),
            keysyms::KEY_Up | keysyms::KEY_k => self.bt_move_sel(-1, count),
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => self.bt_action(Action::Activate),
            keysyms::KEY_d => self.bt_action(Action::Disconnect),
            keysyms::KEY_f | keysyms::KEY_Delete => self.bt_action(Action::Forget),
            keysyms::KEY_s => self.bt_action(Action::Scan),
            keysyms::KEY_p => self.bt_action(Action::Power),
            _ => {}
        }
        true
    }

    fn bt_move_sel(&mut self, delta: i32, count: usize) {
        if count == 0 {
            return;
        }
        if let Some(bt) = self.bt.as_mut() {
            let n = count as i32;
            bt.sel = (((bt.sel as i32 + delta) % n + n) % n) as usize;
        }
        self.request_redraw();
    }

    /// Enter по строке: сопрячь незнакомое, подключить сопряжённое,
    /// отключить уже подключённое.
    fn bt_activate(&mut self) {
        self.bt_cmd_for_selected(|d| {
            Some(if d.connected {
                Cmd::Disconnect(d.path.clone())
            } else if d.paired {
                Cmd::Connect(d.path.clone())
            } else {
                Cmd::Pair(d.path.clone())
            })
        });
    }

    fn bt_cmd_for_selected(&mut self, make: impl Fn(&Device) -> Option<Cmd>) {
        let cmd = self.bt.as_ref().and_then(|b| b.selected()).and_then(&make);
        if let Some(cmd) = cmd {
            self.bt_send(cmd);
        }
    }

    pub fn bt_send(&mut self, cmd: Cmd) {
        if let Some(bt) = self.bt.as_ref() {
            if bt.tx.send(cmd).is_err() {
                tracing::warn!("dawn/bt: поток блютуза не отвечает");
            }
        }
        self.request_redraw();
    }

    /// Выполнить действие подвала. Одна точка входа и для мыши, и для клавиш —
    /// см. BtUi::button_specs.
    pub fn bt_action(&mut self, action: Action) {
        match action {
            Action::Activate => self.bt_activate(),
            Action::Disconnect => {
                self.bt_cmd_for_selected(|d| Some(Cmd::Disconnect(d.path.clone())))
            }
            Action::Forget => self.bt_cmd_for_selected(|d| Some(Cmd::Forget(d.path.clone()))),
            Action::Scan => {
                let scanning = self.bt.as_ref().is_some_and(|b| b.snap.discovering);
                self.bt_send(Cmd::Discovery(!scanning));
            }
            Action::Power => {
                let on = self.bt.as_ref().is_some_and(|b| b.snap.powered);
                self.bt_send(Cmd::Power(!on));
            }
            Action::Close => self.bt_toggle_menu(),
        }
    }

    /// Клик мышью при открытом меню. `true` — клик съеден меню.
    /// `pos` — ЭКРАННЫЕ пиксели (меню приклеено к экрану, как панель).
    pub fn bt_click(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Physical>) -> bool {
        if !self.bt_menu_open() {
            return false;
        }

        // Кнопки подвала — раньше их тут не было вовсе, и клик по ним попадал в
        // ветку «мимо меню», то есть закрывал меню. Со стороны это выглядело
        // ровно как «кнопки не работают».
        let кнопка = self.bt.as_ref().and_then(|b| {
            b.buttons.iter()
                .find(|btn| btn.hit(pos.x, pos.y))
                .map(|btn| (btn.action, btn.enabled))
        });
        if let Some((action, enabled)) = кнопка {
            // Недоступную кнопку не выполняем, но клик всё равно съедаем: он
            // предназначался меню, и «проваливать» его в окно под меню нельзя.
            if enabled {
                self.bt_action(action);
            }
            return true;
        }

        let hit = self.bt.as_ref().and_then(|b| {
            b.rows.iter().find(|r| {
                pos.x >= r.x as f64 && pos.x < (r.x + r.w) as f64
                    && pos.y >= r.y as f64 && pos.y < (r.y + r.h) as f64
            }).map(|r| r.device)
        });
        match hit {
            // Первый клик по строке ВЫДЕЛЯЕТ её, повторный по уже выделенной —
            // выполняет (подключить/отключить/сопрячь).
            //
            // Раньше любой клик подключал сразу же, и это делало кнопки
            // «отключить» и «забыть» бесполезными для мыши: чтобы навести их на
            // устройство, его надо было сперва выделить, а выделение как раз и
            // означало подключение. Теперь выбор и действие разведены, а
            // одно-кликовое подключение никуда не делось — оно во втором клике.
            Some(idx) => {
                let было = self.bt.as_ref().map(|b| b.sel);
                if let Some(bt) = self.bt.as_mut() {
                    bt.sel = idx;
                }
                if было == Some(idx) {
                    self.bt_activate();
                } else {
                    self.request_redraw();
                }
            }
            // Клик мимо меню закрывает его — привычнее, чем ловить Esc.
            None => self.bt_toggle_menu(),
        }
        true
    }
}
