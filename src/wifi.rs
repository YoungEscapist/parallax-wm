//! Вайфай внутри композитора: NetworkManager по D-Bus, меню сетей и значок.
//!
//! Устроено как блютуз (см. bluetooth.rs) и по той же причине: панели и трея в
//! сессии parallax нет, а «подключись к сети» нужно каждый день. Поток D-Bus —
//! свой (zbus блокирующий, главный цикл однопоточный calloop), состояние течёт
//! в композитор каналом calloop, команды обратно обычным `mpsc`.
//!
//! Опрос идёт, только пока что-то из этого открыто: полка состояния или само
//! меню. Закрытая сессия не должна дёргать NM ни разу — в меню опрос чаще
//! (SCAN_POLL), потому что список точек живой.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel;
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

const NM: &str = "org.freedesktop.NetworkManager";
const NM_PATH: &str = "/org/freedesktop/NetworkManager";
const NM_DEVICE: &str = "org.freedesktop.NetworkManager.Device";
const NM_WIRELESS: &str = "org.freedesktop.NetworkManager.Device.Wireless";
const NM_AP: &str = "org.freedesktop.NetworkManager.AccessPoint";
const NM_SETTINGS: &str = "org.freedesktop.NetworkManager.Settings";
const NM_SETTINGS_PATH: &str = "/org/freedesktop/NetworkManager/Settings";
const NM_CONNECTION: &str = "org.freedesktop.NetworkManager.Settings.Connection";
const PROPS_IFACE: &str = "org.freedesktop.DBus.Properties";
/// NM_DEVICE_TYPE_WIFI.
const DEVICE_TYPE_WIFI: u32 = 2;
/// NM_802_11_AP_FLAGS_PRIVACY.
const AP_FLAG_PRIVACY: u32 = 0x1;
/// NM_802_11_AP_SEC_KEY_MGMT_SAE — WPA3.
const AP_SEC_SAE: u32 = 0x400;
/// NM_802_11_AP_SEC_KEY_MGMT_802_1X — сеть с логином (Enterprise).
const AP_SEC_8021X: u32 = 0x200;
/// NM_802_11_AP_SEC_KEY_MGMT_OWE — Enhanced Open.
const AP_SEC_OWE: u32 = 0x800;
const NM_IP4: &str = "org.freedesktop.NetworkManager.IP4Config";
const NM_IP6: &str = "org.freedesktop.NetworkManager.IP6Config";

/// Опрос, когда открыта только полка (нужны лишь имя сети и уровень).
const POLL_IDLE: Duration = Duration::from_millis(2000);
/// Опрос при открытом меню: список точек живой.
const POLL_MENU: Duration = Duration::from_millis(1200);
/// Как часто просить NM пересканировать эфир при открытом меню.
const RESCAN: Duration = Duration::from_secs(10);
const RETRY: Duration = Duration::from_secs(5);

// ── Состояние ────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq)]
pub struct Ap {
    pub ssid: String,
    /// 0..100.
    pub strength: u8,
    pub secure: bool,
    /// WPA3 — ключ заводится с key-mgmt=sae, а не wpa-psk.
    pub sae: bool,
    /// 802.1X (Enterprise): вместо пароля — identity, EAP, сертификаты.
    pub eap: bool,
    /// Enhanced Open (OWE): шифрование без пароля.
    pub owe: bool,
    /// Именно к этой точке мы сейчас подключены.
    pub active: bool,
    /// Для сети есть сохранённый профиль — пароль спрашивать не надо.
    pub saved: bool,
    pub path: String,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// Беспроводного устройства нет вовсе.
    pub present: bool,
    /// Радиомодуль включён (`WirelessEnabled`).
    pub enabled: bool,
    /// Идёт подключение (состояние устройства между 30 и 100).
    pub connecting: bool,
    /// Имя сети, к которой подключены.
    pub ssid: Option<String>,
    /// Уровень активной сети, 0..100.
    pub strength: u8,
    /// Список точек — заполняется только при открытом меню.
    pub aps: Vec<Ap>,
    /// Состояние устройства NM и причина последней смены — по ним видно, что
    /// подключение сорвалось на пароле (см. `handle_wifi_event`).
    pub dev_state: u32,
    pub reason: u32,
}

pub enum Event {
    State(Snapshot),
    Notice(String),
    /// Ответ на `Cmd::Load`: профиль сети и сведения о ней.
    Profile(Box<crate::wifi_form::Профиль>, Option<crate::wifi_form::Сведения>),
    /// Профиль сохранён или подключение запущено.
    Saved(String),
}

#[derive(Debug)]
pub enum Cmd {
    /// Опрашивать состояние: (полка открыта, меню открыто).
    Watch { tray: bool, menu: bool },
    Scan,
    Radio(bool),
    /// Подключиться. `psk` нужен только для защищённой сети без профиля.
    Connect { ssid: String, ap: String, psk: Option<String>, sae: bool },
    Disconnect,
    Forget(String),
    /// Прочитать профиль сети (и сведения о точке `ap`) для формы.
    Load { ssid: String, ap: String },
    /// Сохранить профиль из формы и, если `connect`, подключиться.
    Save { profile: Box<crate::wifi_form::Профиль>, ap: String, connect: bool },
}

// ── Поток D-Bus ──────────────────────────────────────────────────────────────

pub fn spawn(to_plx: channel::Sender<Event>) -> Option<mpsc::Sender<Cmd>> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let ok = std::thread::Builder::new()
        .name("plx-wifi".into())
        .spawn(move || serve(to_plx, rx))
        .is_ok();
    ok.then_some(tx)
}

fn serve(to_plx: channel::Sender<Event>, rx: mpsc::Receiver<Cmd>) {
    let mut conn = zbus::blocking::Connection::system().ok();
    let mut last: Option<Snapshot> = None;
    let (mut tray, mut menu) = (false, false);
    let mut next_poll = Instant::now();
    let mut next_scan = Instant::now();
    let mut next_retry = Instant::now() + RETRY;

    loop {
        let watching = tray || menu;
        let cmd = if watching {
            match rx.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
                Ok(cmd) => Some(cmd),
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => None,
            }
        } else {
            match rx.recv() {
                Ok(cmd) => Some(cmd),
                Err(_) => return,
            }
        };

        if let Some(cmd) = cmd {
            if let Cmd::Watch { tray: t, menu: m } = cmd {
                // Меню только что открыли — список надо показать сразу, а не
                // через секунду, и сразу же попросить свежий скан.
                if m && !menu {
                    next_scan = Instant::now();
                }
                tray = t;
                menu = m;
                last = None;
            } else if let Some(c) = conn.as_ref() {
                match &cmd {
                    Cmd::Load { ssid, ap } => {
                        let (профиль, сведения) = load_profile(c, ssid, ap);
                        let _ = to_plx.send(Event::Profile(Box::new(профиль), сведения));
                    }
                    Cmd::Save { profile, ap, connect } => match save_profile(c, profile, ap, *connect) {
                        Ok(()) => {
                            let _ = to_plx.send(Event::Saved(profile.ssid.clone()));
                        }
                        Err(err) => {
                            let _ = to_plx.send(Event::Notice(human_error(&err)));
                        }
                    },
                    _ => {
                        if let Err(err) = run_cmd(c, &cmd) {
                            let _ = to_plx.send(Event::Notice(human_error(&err)));
                        }
                    }
                }
                last = None;
            } else {
                let _ = to_plx.send(Event::Notice("NetworkManager is not available".into()));
            }
            next_poll = Instant::now();
            if !(tray || menu) {
                continue;
            }
        }

        if conn.is_none() && Instant::now() >= next_retry {
            conn = zbus::blocking::Connection::system().ok();
            next_retry = Instant::now() + RETRY;
        }
        let Some(c) = conn.as_ref() else {
            next_poll = Instant::now() + RETRY;
            continue;
        };

        // Скан просим редко: он поднимает радио и на секунду роняет пропускную
        // способность, а список NM отдаёт из своего кэша и между сканами.
        if menu && Instant::now() >= next_scan {
            next_scan = Instant::now() + RESCAN;
            if let Some(dev) = wifi_device(c) {
                let _ = request_scan(c, &dev);
            }
        }

        match read_state(c, menu) {
            Some(snap) => {
                next_poll = Instant::now() + if menu { POLL_MENU } else { POLL_IDLE };
                if last.as_ref() != Some(&snap) {
                    last = Some(snap.clone());
                    if to_plx.send(Event::State(snap)).is_err() {
                        return;
                    }
                }
            }
            None => next_poll = Instant::now() + RETRY,
        }
    }
}

fn proxy<'a>(
    conn: &'a zbus::blocking::Connection,
    path: &str,
    iface: &'static str,
) -> zbus::Result<zbus::blocking::Proxy<'a>> {
    zbus::blocking::Proxy::new(conn, NM, path.to_string(), iface)
}

fn prop(
    conn: &zbus::blocking::Connection,
    path: &str,
    iface: &str,
    key: &str,
) -> zbus::Result<OwnedValue> {
    proxy(conn, path, PROPS_IFACE)?.call("Get", &(iface, key))
}

fn wifi_device(conn: &zbus::blocking::Connection) -> Option<String> {
    let devices: Vec<OwnedObjectPath> = proxy(conn, NM_PATH, NM).ok()?.call("GetDevices", &()).ok()?;
    devices.into_iter().find_map(|d| {
        let path = d.as_str().to_string();
        let kind = prop(conn, &path, NM_DEVICE, "DeviceType")
            .ok()
            .and_then(|v| u32::try_from(v).ok())?;
        (kind == DEVICE_TYPE_WIFI).then_some(path)
    })
}

fn request_scan(conn: &zbus::blocking::Connection, dev: &str) -> zbus::Result<()> {
    let opts: HashMap<&str, Value> = HashMap::new();
    proxy(conn, dev, NM_WIRELESS)?.call::<_, _, ()>("RequestScan", &(opts,))
}

/// SSID у NM — байты, а не строка: сети с не-UTF8 именем существуют.
fn ssid_of(v: OwnedValue) -> Option<String> {
    let raw = Vec::<u8>::try_from(v).ok()?;
    (!raw.is_empty()).then(|| String::from_utf8_lossy(&raw).to_string())
}

/// Сохранённые профили: ssid → путь профиля. Нужны, чтобы не спрашивать
/// пароль там, где он уже известен, и чтобы уметь «забыть» сеть.
fn saved_profiles(conn: &zbus::blocking::Connection) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let Ok(list) = proxy(conn, NM_SETTINGS_PATH, NM_SETTINGS)
        .and_then(|p| p.call::<_, _, Vec<OwnedObjectPath>>("ListConnections", &()))
    else {
        return out;
    };
    for path in list {
        let path = path.as_str().to_string();
        let Ok(settings) = proxy(conn, &path, NM_CONNECTION).and_then(|p| {
            p.call::<_, _, HashMap<String, HashMap<String, OwnedValue>>>("GetSettings", &())
        }) else {
            continue;
        };
        if let Some(ssid) = settings
            .get("802-11-wireless")
            .and_then(|w| w.get("ssid"))
            .cloned()
            .and_then(ssid_of)
        {
            out.insert(ssid, path);
        }
    }
    out
}

fn read_state(conn: &zbus::blocking::Connection, with_list: bool) -> Option<Snapshot> {
    let enabled = prop(conn, NM_PATH, NM, "WirelessEnabled")
        .ok()
        .and_then(|v| bool::try_from(v).ok())
        .unwrap_or(false);
    let Some(dev) = wifi_device(conn) else {
        return Some(Snapshot { present: false, enabled, ..Default::default() });
    };

    // Состояния NM: 100 — подключено, 30..100 — в процессе.
    let dev_state = prop(conn, &dev, NM_DEVICE, "State")
        .ok()
        .and_then(|v| u32::try_from(v).ok())
        .unwrap_or(0);
    let active_ap = prop(conn, &dev, NM_WIRELESS, "ActiveAccessPoint")
        .ok()
        .and_then(|v| OwnedObjectPath::try_from(v).ok())
        .map(|p| p.as_str().to_string())
        .filter(|p| p != "/");

    let reason = prop(conn, &dev, NM_DEVICE, "StateReason")
        .ok()
        .and_then(|v| <(u32, u32)>::try_from(v).ok())
        .map(|(_, r)| r)
        .unwrap_or(0);
    let mut snap = Snapshot {
        present: true,
        enabled,
        connecting: (30..100).contains(&dev_state),
        ssid: None,
        strength: 0,
        aps: Vec::new(),
        dev_state,
        reason,
    };
    if let Some(ap) = active_ap.as_deref() {
        snap.ssid = prop(conn, ap, NM_AP, "Ssid").ok().and_then(ssid_of);
        snap.strength = prop(conn, ap, NM_AP, "Strength")
            .ok()
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(0);
    }
    if !with_list {
        return Some(snap);
    }

    let saved = saved_profiles(conn);
    let aps: Vec<OwnedObjectPath> = proxy(conn, &dev, NM_WIRELESS)
        .and_then(|p| p.call("GetAllAccessPoints", &()))
        .unwrap_or_default();
    for ap in aps {
        let path = ap.as_str().to_string();
        let Some(ssid) = prop(conn, &path, NM_AP, "Ssid").ok().and_then(ssid_of) else {
            continue; // скрытая сеть: имени нет, подключаться не к чему
        };
        let strength = prop(conn, &path, NM_AP, "Strength")
            .ok()
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(0);
        let flags = prop(conn, &path, NM_AP, "Flags").ok().and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        let wpa = prop(conn, &path, NM_AP, "WpaFlags").ok().and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        let rsn = prop(conn, &path, NM_AP, "RsnFlags").ok().and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        // Одна сеть — несколько точек (двухдиапазонный роутер, репитеры).
        // Держим самую сильную, иначе список забит дублями.
        if let Some(prev) = snap.aps.iter_mut().find(|a| a.ssid == ssid) {
            if strength > prev.strength {
                prev.strength = strength;
                prev.path = path;
            }
            continue;
        }
        snap.aps.push(Ap {
            saved: saved.contains_key(&ssid),
            active: active_ap.as_deref() == Some(path.as_str()),
            secure: flags & AP_FLAG_PRIVACY != 0 || wpa != 0 || rsn != 0,
            sae: rsn & AP_SEC_SAE != 0,
            eap: (wpa | rsn) & AP_SEC_8021X != 0,
            owe: rsn & AP_SEC_OWE != 0,
            ssid,
            strength,
            path,
        });
    }
    // Сохранённые сети, которых сейчас нет в эфире (скрытые, дальние), —
    // тоже в список, в самый конец: их надо уметь править и забывать, а к
    // скрытой ещё и подключаться (точка "/" — NM ищет сеть сам).
    for ssid in saved.keys() {
        if snap.aps.iter().any(|a| &a.ssid == ssid) {
            continue;
        }
        snap.aps.push(Ap {
            ssid: ssid.clone(),
            strength: 0,
            secure: true,
            sae: false,
            eap: false,
            owe: false,
            active: false,
            saved: true,
            path: "/".into(),
        });
    }
    // Подключённая сверху, затем известные, дальше по уровню сигнала.
    snap.aps.sort_by(|a, b| {
        let rank = |x: &Ap| if x.active { 0 } else if x.path == "/" { 3 } else if x.saved { 1 } else { 2 };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.strength.cmp(&a.strength))
            .then_with(|| a.ssid.to_lowercase().cmp(&b.ssid.to_lowercase()))
    });
    Some(snap)
}

fn run_cmd(conn: &zbus::blocking::Connection, cmd: &Cmd) -> zbus::Result<()> {
    match cmd {
        Cmd::Watch { .. } | Cmd::Load { .. } | Cmd::Save { .. } => Ok(()),
        Cmd::Radio(on) => proxy(conn, NM_PATH, PROPS_IFACE)?
            .call::<_, _, ()>("Set", &(NM, "WirelessEnabled", Value::from(*on))),
        Cmd::Scan => {
            let Some(dev) = wifi_device(conn) else { return Ok(()) };
            request_scan(conn, &dev)
        }
        Cmd::Disconnect => {
            let Some(dev) = wifi_device(conn) else { return Ok(()) };
            proxy(conn, &dev, NM_DEVICE)?.call::<_, _, ()>("Disconnect", &())
        }
        Cmd::Forget(ssid) => {
            let Some(path) = saved_profiles(conn).get(ssid).cloned() else { return Ok(()) };
            proxy(conn, &path, NM_CONNECTION)?.call::<_, _, ()>("Delete", &())
        }
        Cmd::Connect { ssid, ap, psk, sae } => {
            let Some(dev) = wifi_device(conn) else { return Ok(()) };
            let dev_path = ObjectPath::try_from(dev.as_str())?;
            let ap_path = ObjectPath::try_from(ap.as_str())?;
            let nm = proxy(conn, NM_PATH, NM)?;

            // Известная сеть — поднимаем её профиль: там уже лежит пароль,
            // статический адрес, всё, что человек когда-то настроил.
            if let Some(profile) = saved_profiles(conn).get(ssid) {
                let profile = ObjectPath::try_from(profile.as_str())?;
                return nm.call::<_, _, OwnedObjectPath>(
                    "ActivateConnection", &(&profile, &dev_path, &ap_path),
                ).map(|_| ());
            }

            let mut settings: HashMap<&str, HashMap<&str, Value>> = HashMap::new();
            let mut conn_sec: HashMap<&str, Value> = HashMap::new();
            conn_sec.insert("id", Value::from(ssid.as_str()));
            conn_sec.insert("type", Value::from("802-11-wireless"));
            settings.insert("connection", conn_sec);

            let mut wireless: HashMap<&str, Value> = HashMap::new();
            wireless.insert("ssid", Value::from(ssid.as_bytes().to_vec()));
            wireless.insert("mode", Value::from("infrastructure"));
            settings.insert("802-11-wireless", wireless);

            if let Some(psk) = psk {
                let mut sec: HashMap<&str, Value> = HashMap::new();
                sec.insert("key-mgmt", Value::from(if *sae { "sae" } else { "wpa-psk" }));
                sec.insert("psk", Value::from(psk.as_str()));
                settings.insert("802-11-wireless-security", sec);
            }
            nm.call::<_, _, (OwnedObjectPath, OwnedObjectPath)>(
                "AddAndActivateConnection", &(settings, &dev_path, &ap_path),
            ).map(|_| ())
        }
    }
}

// ── Профиль для формы ────────────────────────────────────────────────────────

type Settings = crate::wifi_form::Настройки;

/// Настройки профиля целиком, с секретами. Секреты NM отдаёт отдельно и
/// только тем, кому можно; не отдал — форма покажет пустые поля, а
/// сохранение их не тронет (см. `wifi_form::в_nm`).
fn full_settings(conn: &zbus::blocking::Connection, path: &str) -> Option<Settings> {
    let p = proxy(conn, path, NM_CONNECTION).ok()?;
    let mut settings: Settings = p.call("GetSettings", &()).ok()?;
    for section in ["802-11-wireless-security", "802-1x"] {
        if !settings.contains_key(section) {
            continue;
        }
        if let Ok(secrets) = p.call::<_, _, Settings>("GetSecrets", &(section,)) {
            for (name, values) in secrets {
                settings.entry(name).or_default().extend(values);
            }
        }
    }
    Some(settings)
}

fn prop_str(conn: &zbus::blocking::Connection, path: &str, iface: &str, key: &str) -> String {
    prop(conn, path, iface, key)
        .ok()
        .and_then(|v| String::try_from(v).ok())
        .unwrap_or_default()
}

fn prop_path(conn: &zbus::blocking::Connection, path: &str, iface: &str, key: &str) -> Option<String> {
    prop(conn, path, iface, key)
        .ok()
        .and_then(|v| OwnedObjectPath::try_from(v).ok())
        .map(|p| p.as_str().to_string())
        .filter(|p| p != "/")
}

/// Адреса из IPxConfig.AddressData: «адрес/префикс».
fn ip_addresses(conn: &zbus::blocking::Connection, cfg: &str, iface: &str) -> Vec<String> {
    let Ok(v) = prop(conn, cfg, iface, "AddressData") else { return Vec::new() };
    let Ok(list) = Vec::<HashMap<String, OwnedValue>>::try_from(v) else { return Vec::new() };
    list.iter()
        .filter_map(|a| {
            let addr = String::try_from(a.get("address")?.try_clone().ok()?).ok()?;
            let prefix = u32::try_from(a.get("prefix")?.try_clone().ok()?).ok()?;
            Some(format!("{addr}/{prefix}"))
        })
        .collect()
}

fn security_label(conn: &zbus::blocking::Connection, ap: &str) -> String {
    let get = |k: &str| prop(conn, ap, NM_AP, k).ok().and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
    let (flags, wpa, rsn) = (get("Flags"), get("WpaFlags"), get("RsnFlags"));
    let mut parts: Vec<&str> = Vec::new();
    if (wpa | rsn) & AP_SEC_8021X != 0 {
        parts.push(if rsn != 0 { "WPA2/WPA3 Enterprise" } else { "WPA Enterprise" });
    } else if rsn & AP_SEC_SAE != 0 {
        parts.push(if rsn & 0x100 != 0 { "WPA2/WPA3 Personal" } else { "WPA3 Personal" });
    } else if rsn & AP_SEC_OWE != 0 {
        parts.push("OWE");
    } else if rsn != 0 {
        parts.push("WPA2 Personal");
    } else if wpa != 0 {
        parts.push("WPA Personal");
    } else if flags & AP_FLAG_PRIVACY != 0 {
        parts.push("WEP");
    } else {
        parts.push(crate::т!("открытая", "open"));
    }
    parts.join(", ")
}

/// Сведения о точке `ap` и — если через неё подключены — о подключении.
fn details(conn: &zbus::blocking::Connection, ap: &str) -> Option<crate::wifi_form::Сведения> {
    let dev = wifi_device(conn)?;
    let mut с = crate::wifi_form::Сведения::default();
    с.интерфейс = prop_str(conn, &dev, NM_DEVICE, "Interface");
    с.мак = prop_str(conn, &dev, NM_WIRELESS, "HwAddress");
    if ap != "/" && !ap.is_empty() {
        с.сигнал = prop(conn, ap, NM_AP, "Strength").ok().and_then(|v| u8::try_from(v).ok());
        с.частота = prop(conn, ap, NM_AP, "Frequency").ok().and_then(|v| u32::try_from(v).ok());
        с.bssid = prop_str(conn, ap, NM_AP, "HwAddress");
        с.защита = security_label(conn, ap);
        с.скорость = prop(conn, ap, NM_AP, "MaxBitrate").ok().and_then(|v| u32::try_from(v).ok());
    }
    let active = prop_path(conn, &dev, NM_WIRELESS, "ActiveAccessPoint");
    let same_ssid = active.as_deref().is_some_and(|a| {
        a == ap || prop(conn, a, NM_AP, "Ssid").ok().and_then(ssid_of)
            == prop(conn, ap, NM_AP, "Ssid").ok().and_then(ssid_of)
    });
    if same_ssid {
        с.подключена = true;
        if let Some(bitrate) = prop(conn, &dev, NM_WIRELESS, "Bitrate").ok().and_then(|v| u32::try_from(v).ok()) {
            с.скорость = Some(bitrate);
        }
        if let Some(cfg) = prop_path(conn, &dev, NM_DEVICE, "Ip4Config") {
            с.ipv4 = ip_addresses(conn, &cfg, NM_IP4);
            с.шлюз4 = prop_str(conn, &cfg, NM_IP4, "Gateway");
            if let Ok(v) = prop(conn, &cfg, NM_IP4, "NameserverData") {
                if let Ok(list) = Vec::<HashMap<String, OwnedValue>>::try_from(v) {
                    с.dns4 = list.iter()
                        .filter_map(|d| String::try_from(d.get("address")?.try_clone().ok()?).ok())
                        .collect();
                }
            }
        }
        if let Some(cfg) = prop_path(conn, &dev, NM_DEVICE, "Ip6Config") {
            с.ipv6 = ip_addresses(conn, &cfg, NM_IP6);
            с.шлюз6 = prop_str(conn, &cfg, NM_IP6, "Gateway");
            if let Ok(v) = prop(conn, &cfg, NM_IP6, "Nameservers") {
                if let Ok(list) = Vec::<Vec<u8>>::try_from(v) {
                    с.dns6 = list.iter()
                        .filter_map(|b| <[u8; 16]>::try_from(b.as_slice()).ok())
                        .map(|b| std::net::Ipv6Addr::from(b).to_string())
                        .collect();
                }
            }
        }
    }
    Some(с)
}

/// Профиль сети `ssid` для формы: сохранённый — из NM, новый — по точке.
fn load_profile(
    conn: &zbus::blocking::Connection,
    ssid: &str,
    ap: &str,
) -> (crate::wifi_form::Профиль, Option<crate::wifi_form::Сведения>) {
    use crate::wifi_form::{Защита, Профиль};
    let сведения = details(conn, ap);
    if let Some(path) = saved_profiles(conn).get(ssid).cloned() {
        if let Some(settings) = full_settings(conn, &path) {
            return (crate::wifi_form::из_nm(&settings, Some(path)), сведения);
        }
    }
    let mut п = Профиль { ssid: ssid.to_string(), ..Default::default() };
    if ap != "/" && !ap.is_empty() {
        let get = |k: &str| prop(conn, ap, NM_AP, k).ok().and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
        let (flags, wpa, rsn) = (get("Flags"), get("WpaFlags"), get("RsnFlags"));
        п.защита = if (wpa | rsn) & AP_SEC_8021X != 0 {
            Защита::WpaEap
        } else if rsn & AP_SEC_SAE != 0 && rsn & 0x100 == 0 {
            Защита::Sae
        } else if rsn & AP_SEC_OWE != 0 {
            Защита::Owe
        } else if wpa != 0 || rsn != 0 {
            Защита::WpaPsk
        } else if flags & AP_FLAG_PRIVACY != 0 {
            Защита::Wep
        } else {
            Защита::Нет
        };
    }
    (п, сведения)
}

fn current_user() -> String {
    std::env::var("USER")
        .ok()
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| "yarik".into())
}

/// Сохранить профиль и, если просили, подключиться.
fn save_profile(
    conn: &zbus::blocking::Connection,
    п: &crate::wifi_form::Профиль,
    ap: &str,
    connect: bool,
) -> zbus::Result<()> {
    let user = current_user();
    let ap_path = ObjectPath::try_from(if ap.is_empty() { "/" } else { ap })?;
    let nm = proxy(conn, NM_PATH, NM)?;
    let dev = wifi_device(conn);
    // Путь профиля берём заново: форма могла открыться, пока профиля ещё не
    // было, а в NM он с тех пор появился.
    let path = п.путь.clone().or_else(|| saved_profiles(conn).get(&п.ssid).cloned());
    match path {
        Some(path) => {
            let base = full_settings(conn, &path).unwrap_or_default();
            let settings = crate::wifi_form::в_nm(п, base, &user);
            proxy(conn, &path, NM_CONNECTION)?.call::<_, _, ()>("Update", &(settings,))?;
            if connect {
                let Some(dev) = dev else { return Ok(()) };
                let dev_path = ObjectPath::try_from(dev.as_str())?;
                let profile = ObjectPath::try_from(path.as_str())?;
                nm.call::<_, _, OwnedObjectPath>("ActivateConnection", &(&profile, &dev_path, &ap_path))?;
            }
            Ok(())
        }
        None => {
            let settings = crate::wifi_form::в_nm(п, Settings::new(), &user);
            if connect {
                let Some(dev) = dev else { return Ok(()) };
                let dev_path = ObjectPath::try_from(dev.as_str())?;
                nm.call::<_, _, (OwnedObjectPath, OwnedObjectPath)>(
                    "AddAndActivateConnection", &(settings, &dev_path, &ap_path),
                )?;
            } else {
                proxy(conn, NM_SETTINGS_PATH, NM_SETTINGS)?
                    .call::<_, _, OwnedObjectPath>("AddConnection", &(settings,))?;
            }
            Ok(())
        }
    }
}

/// Человеческий текст вместо простыни D-Bus.
fn human_error(err: &zbus::Error) -> String {
    let text = err.to_string();
    let tail = match text.rsplit_once(": ") {
        Some((_, t)) if !t.is_empty() => t,
        _ => text.as_str(),
    };
    let known = [
        ("Secrets were required", "wrong password"),
        ("no secrets provided", "wrong password"),
        ("802-1X supplicant took too long", "authentication timed out"),
        ("not authorized", "not allowed (polkit)"),
        ("NotAuthorized", "not allowed (polkit)"),
        ("device is strictly unmanaged", "device is unmanaged by NetworkManager"),
        ("No suitable device found", "no wireless device"),
        ("Connection was removed", "connection removed"),
        ("wireless is disabled", "wireless is off"),
    ];
    for (needle, human) in known {
        if tail.contains(needle) {
            return human.to_string();
        }
    }
    tail.to_string()
}

// ── Меню ─────────────────────────────────────────────────────────────────────

/// Сколько держим сообщение о результате команды.
const NOTICE_TTL: Duration = Duration::from_secs(6);

pub struct WifiUi {
    tx: mpsc::Sender<Cmd>,
    pub snap: Snapshot,
    pub open: bool,
    pub sel: usize,
    /// Ввод пароля: (ssid, точка, набранное, WPA3). Пока он есть, меню
    /// печатает символы, а не разбирает их как команды.
    pub ask: Option<Ask>,
    pub notice: Option<(String, Instant)>,
    /// Прямоугольники видимых строк и номер сети в каждой — заполняет
    /// отрисовка. Номер именно абсолютный: список прокручивается, и позиция
    /// в кадре не равна позиции в списке.
    pub rows: Vec<(crate::tray::Rect, usize)>,
    /// Открытая форма настройки сети (см. wifi_form.rs). Пока она есть,
    /// клавиши и клики принадлежат ей, а не списку.
    pub форма: Option<crate::wifi_form::Форма>,
    /// Сеть, к которой мы только что начали подключаться: по ней ловится
    /// «пароль не подошёл» — вызов D-Bus при этом успешен, срывается уже
    /// сама попытка (см. `handle_wifi_event`).
    ждём: Option<Ожидание>,
}

struct Ожидание {
    ssid: String,
    ap: String,
    /// Устройство уже побывало в «подключаемся».
    шли: bool,
    с: Instant,
}

pub struct Ask {
    pub ssid: String,
    pub ap: String,
    pub sae: bool,
    pub text: String,
}

impl WifiUi {
    pub fn notice_text(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_TTL)
            .map(|(t, _)| t.as_str())
    }

    pub fn selected(&self) -> Option<&Ap> {
        self.snap.aps.get(self.sel)
    }
}

impl crate::state::Parallax {
    pub fn init_wifi(&mut self, tx: mpsc::Sender<Cmd>) {
        self.wifi = Some(WifiUi {
            tx,
            snap: Snapshot::default(),
            open: false,
            sel: 0,
            ask: None,
            notice: None,
            rows: Vec::new(),
            форма: None,
            ждём: None,
        });
    }

    pub fn handle_wifi_event(&mut self, event: Event) {
        let Some(w) = self.wifi.as_mut() else { return };
        match event {
            Event::State(snap) => {
                // Выбор держим на ТОЙ ЖЕ сети, а не на номере строки: список
                // пересортировывается по уровню сигнала на каждом опросе.
                let keep = w.snap.aps.get(w.sel).map(|a| a.ssid.clone());
                w.snap = snap;
                w.sel = keep
                    .and_then(|s| w.snap.aps.iter().position(|a| a.ssid == s))
                    .unwrap_or(w.sel)
                    .min(w.snap.aps.len().saturating_sub(1));
            }
            Event::Notice(text) => {
                tracing::info!("plx/wifi: {}", text);
                // Ошибка сохранения — показываем в самой форме, она открыта.
                if let Some(ф) = w.форма.as_mut() {
                    ф.грузится = false;
                    ф.ошибка = Some(text.clone());
                }
                w.notice = Some((text, Instant::now()));
            }
            Event::Profile(профиль, сведения) => {
                if let Some(ф) = w.форма.as_mut() {
                    // Ответ мог прийти к уже другой форме — сверяемся по SSID.
                    if ф.грузится && ф.п.ssid == профиль.ssid {
                        let показать = ф.показать;
                        let ошибка = ф.ошибка.take();
                        let ap = ф.ap.clone();
                        *ф = crate::wifi_form::Форма::новая(*профиль, ap, false);
                        ф.сведения = сведения;
                        ф.показать = показать;
                        ф.ошибка = ошибка;
                        ф.фокус_на_главное();
                    }
                }
            }
            Event::Saved(ssid) => {
                tracing::info!("plx/wifi: profile {:?} saved", ssid);
                w.форма = None;
                w.notice = Some((crate::тф!("{}: сохранено", "{}: saved", ssid), Instant::now()));
            }
        }
        self.wifi_check_attempt();
        self.request_redraw();
    }

    /// Следим за начатым подключением: сорвалось на пароле или identity —
    /// открываем форму этой сети с понятной ошибкой, как GNOME переспрашивает
    /// пароль. Причины NM: 7 — нет секретов, 8–11 — отказ/таймаут supplicant.
    fn wifi_check_attempt(&mut self) {
        let Some(w) = self.wifi.as_mut() else { return };
        let Some(ж) = w.ждём.as_mut() else { return };
        let snap = &w.snap;
        if snap.ssid.as_deref() == Some(ж.ssid.as_str()) && !snap.connecting && snap.dev_state == 100 {
            w.ждём = None;
            return;
        }
        if snap.connecting {
            ж.шли = true;
            return;
        }
        let сорвалось = ж.шли && matches!(snap.reason, 7..=11);
        if ж.с.elapsed().as_secs() > 90 || (ж.шли && !сорвалось) {
            w.ждём = None;
            return;
        }
        if сорвалось {
            let (ssid, ap) = (ж.ssid.clone(), ж.ap.clone());
            w.ждём = None;
            tracing::info!("plx/wifi: connecting to {:?} failed (reason {}), asking again", ssid, snap.reason);
            self.wifi_open_form(&ssid, &ap, false);
            if let Some(ф) = self.wifi.as_mut().and_then(|w| w.форма.as_mut()) {
                ф.ошибка = Some(crate::т!(
                    "не удалось войти — проверьте пароль или identity",
                    "could not authenticate — check the password or identity",
                ).into());
            }
        }
    }

    fn wifi_ждать(&mut self, ssid: &str, ap: &str) {
        if let Some(w) = self.wifi.as_mut() {
            w.ждём = Some(Ожидание { ssid: ssid.to_string(), ap: ap.to_string(), шли: false, с: Instant::now() });
        }
    }

    /// Открыть форму сети. `ssid_правится` — новая скрытая сеть.
    pub fn wifi_open_form(&mut self, ssid: &str, ap: &str, ssid_правится: bool) {
        let Some(w) = self.wifi.as_mut() else { return };
        let профиль = crate::wifi_form::Профиль {
            ssid: ssid.to_string(),
            скрытая: ssid_правится,
            ..Default::default()
        };
        let mut ф = crate::wifi_form::Форма::новая(профиль, ap.to_string(), ssid_правится);
        if !ssid_правится {
            ф.грузится = true;
        }
        w.форма = Some(ф);
        w.open = true;
        if !ssid_правится {
            self.wifi_send(Cmd::Load { ssid: ssid.to_string(), ap: ap.to_string() });
        }
        self.wifi_sync_watch();
        self.request_redraw();
    }

    pub fn wifi_form_open(&self) -> bool {
        self.wifi.as_ref().is_some_and(|w| w.форма.is_some())
    }

    /// Кнопка «Подключить»/«Сохранить» в форме.
    fn wifi_form_submit(&mut self, connect: bool) {
        let Some(ф) = self.wifi.as_mut().and_then(|w| w.форма.as_mut()) else { return };
        if ф.грузится {
            return;
        }
        if let Err(e) = crate::wifi_form::проверить(&ф.п) {
            ф.ошибка = Some(e);
            self.request_redraw();
            return;
        }
        ф.ошибка = None;
        ф.грузится = true;
        let профиль = ф.п.clone();
        let ap = ф.ap.clone();
        tracing::info!("plx/wifi: saving {:?} (connect={})", профиль.ssid, connect);
        if connect {
            self.wifi_ждать(&профиль.ssid, &ap);
        }
        self.wifi_send(Cmd::Save { profile: Box::new(профиль), ap, connect });
    }

    fn wifi_form_press(&mut self, поле: crate::wifi_form::Поле, шаг: i32) {
        use crate::wifi_form::{Вид, Поле};
        match поле {
            Поле::Подключить => self.wifi_form_submit(true),
            Поле::Сохранить => self.wifi_form_submit(false),
            Поле::Забыть => {
                let ssid = self.wifi.as_ref().and_then(|w| w.форма.as_ref()).map(|ф| ф.п.ssid.clone());
                if let Some(ssid) = ssid {
                    self.wifi_send(Cmd::Forget(ssid));
                }
                if let Some(w) = self.wifi.as_mut() {
                    w.форма = None;
                }
            }
            Поле::Отмена => {
                if let Some(w) = self.wifi.as_mut() {
                    w.форма = None;
                }
            }
            п if matches!(п.вид(), Вид::Выбор | Вид::Флаг) => {
                if let Some(ф) = self.wifi.as_mut().and_then(|w| w.форма.as_mut()) {
                    ф.переключить(п, шаг);
                }
            }
            _ => {}
        }
        self.request_redraw();
    }

    /// Клавиша в форме. Всегда съедается.
    fn wifi_form_key(&mut self, keysym: u32, ch: Option<char>) -> bool {
        use crate::wifi_form::{Вид, Поле};
        use smithay::input::keyboard::keysyms;
        let Some(ф) = self.wifi.as_mut().and_then(|w| w.форма.as_mut()) else { return false };
        let фокус = ф.фокус;
        let вид = фокус.вид();
        match keysym {
            keysyms::KEY_Escape => {
                if let Some(w) = self.wifi.as_mut() {
                    w.форма = None;
                }
            }
            keysyms::KEY_Down | keysyms::KEY_Tab => ф.сдвинуть_фокус(1),
            keysyms::KEY_Up | keysyms::KEY_ISO_Left_Tab => ф.сдвинуть_фокус(-1),
            keysyms::KEY_Left | keysyms::KEY_Right => {
                let шаг = if keysym == keysyms::KEY_Left { -1 } else { 1 };
                match вид {
                    Вид::Выбор | Вид::Флаг => ф.переключить(фокус, шаг),
                    Вид::Кнопка => {
                        let кнопки = ф.кнопки();
                        let i = кнопки.iter().position(|к| *к == фокус).unwrap_or(0) as i32;
                        let n = кнопки.len() as i32;
                        ф.фокус = кнопки[(((i + шаг) % n + n) % n) as usize];
                    }
                    Вид::Текст { .. } => {}
                }
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                // Enter на кнопке — эта кнопка; где угодно ещё — «Подключить»,
                // как в диалогах GNOME.
                let поле = if вид == Вид::Кнопка { фокус } else { Поле::Подключить };
                self.wifi_form_press(поле, 1);
                return true;
            }
            keysyms::KEY_space if matches!(вид, Вид::Выбор | Вид::Флаг | Вид::Кнопка) => {
                self.wifi_form_press(фокус, 1);
                return true;
            }
            keysyms::KEY_BackSpace => {
                if let Some(t) = ф.текст_mut(фокус) {
                    t.pop();
                    ф.ошибка = None;
                }
            }
            keysyms::KEY_Delete => {
                if let Some(t) = ф.текст_mut(фокус) {
                    t.clear();
                    ф.ошибка = None;
                }
            }
            _ => {
                if let (Some(c), Some(t)) = (ch, ф.текст_mut(фокус)) {
                    if !c.is_control() && t.chars().count() < 256 {
                        t.push(c);
                        ф.ошибка = None;
                    }
                }
            }
        }
        self.request_redraw();
        true
    }

    pub fn wifi_menu_open(&self) -> bool {
        self.wifi.as_ref().is_some_and(|w| w.open)
    }

    pub fn wifi_snapshot(&self) -> Option<&Snapshot> {
        self.wifi.as_ref().map(|w| &w.snap)
    }

    pub fn wifi_send(&mut self, cmd: Cmd) {
        if let Some(w) = self.wifi.as_ref() {
            if w.tx.send(cmd).is_err() {
                tracing::warn!("plx/wifi: NetworkManager thread not responding");
            }
        }
        self.request_redraw();
    }

    /// Сообщить потоку, кому сейчас нужно состояние. Зовётся при любом
    /// открытии/закрытии полки и меню — опрос живёт ровно столько, сколько на
    /// него кто-то смотрит.
    pub fn wifi_sync_watch(&mut self) {
        let tray = self.tray_open();
        let menu = self.wifi_menu_open();
        self.wifi_send(Cmd::Watch { tray, menu });
    }

    pub fn wifi_toggle_menu(&mut self) {
        let Some(w) = self.wifi.as_mut() else {
            tracing::warn!("plx/wifi: thread not started (no system bus?)");
            return;
        };
        w.open = !w.open;
        w.ask = None;
        w.форма = None;
        if !w.open {
            self.text_cache.clear();
        }
        self.wifi_sync_watch();
        self.request_redraw();
    }

    /// Enter по строке: известную поднять, к незнакомой защищённой спросить
    /// пароль, к открытой подключиться сразу, подключённую — отключить.
    fn wifi_activate(&mut self) {
        let Some(ap) = self.wifi.as_ref().and_then(|w| w.selected()).cloned() else { return };
        if ap.active {
            self.wifi_send(Cmd::Disconnect);
            return;
        }
        // Незнакомая защищённая сеть — форма с фокусом на том, что нужно
        // ввести: пароль, а у Enterprise — identity (см. wifi_form.rs).
        if ap.secure && !ap.saved && !ap.owe {
            self.wifi_open_form(&ap.ssid, &ap.path, false);
            return;
        }
        self.wifi_ждать(&ap.ssid, &ap.path);
        self.wifi_send(Cmd::Connect {
            ssid: ap.ssid,
            ap: ap.path,
            psk: None,
            sae: ap.sae,
        });
    }

    fn wifi_move_sel(&mut self, delta: i32) {
        let Some(w) = self.wifi.as_mut() else { return };
        let n = w.snap.aps.len() as i32;
        if n == 0 {
            return;
        }
        w.sel = (((w.sel as i32 + delta) % n + n) % n) as usize;
        self.request_redraw();
    }

    /// Клавиша при открытом меню. `ch` — символ для ввода пароля.
    /// `true` — клавиша съедена меню.
    pub fn wifi_key(&mut self, keysym: u32, ch: Option<char>) -> bool {
        use smithay::input::keyboard::keysyms;
        if !self.wifi_menu_open() {
            return false;
        }
        if self.wifi_form_open() {
            return self.wifi_form_key(keysym, ch);
        }

        // Пока спрашиваем пароль, меню — это поле ввода, и буквы в нём буквы,
        // а не команды.
        if self.wifi.as_ref().is_some_and(|w| w.ask.is_some()) {
            match keysym {
                keysyms::KEY_Escape => {
                    if let Some(w) = self.wifi.as_mut() {
                        w.ask = None;
                    }
                }
                keysyms::KEY_Return | keysyms::KEY_KP_Enter => {
                    let ask = self.wifi.as_mut().and_then(|w| w.ask.take());
                    if let Some(ask) = ask {
                        self.wifi_send(Cmd::Connect {
                            ssid: ask.ssid,
                            ap: ask.ap,
                            psk: Some(ask.text),
                            sae: ask.sae,
                        });
                    }
                }
                keysyms::KEY_BackSpace => {
                    if let Some(a) = self.wifi.as_mut().and_then(|w| w.ask.as_mut()) {
                        a.text.pop();
                    }
                }
                _ => {
                    if let (Some(c), Some(a)) =
                        (ch, self.wifi.as_mut().and_then(|w| w.ask.as_mut()))
                    {
                        if !c.is_control() && a.text.chars().count() < 63 {
                            a.text.push(c);
                        }
                    }
                }
            }
            self.request_redraw();
            return true;
        }

        match keysym {
            keysyms::KEY_Escape => self.wifi_toggle_menu(),
            keysyms::KEY_Down | keysyms::KEY_j => self.wifi_move_sel(1),
            keysyms::KEY_Up | keysyms::KEY_k => self.wifi_move_sel(-1),
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => self.wifi_activate(),
            keysyms::KEY_d => self.wifi_send(Cmd::Disconnect),
            keysyms::KEY_f | keysyms::KEY_Delete => {
                if let Some(ap) = self.wifi.as_ref().and_then(|w| w.selected()).cloned() {
                    self.wifi_send(Cmd::Forget(ap.ssid));
                }
            }
            keysyms::KEY_s => self.wifi_send(Cmd::Scan),
            // Настройки и сведения выбранной сети.
            keysyms::KEY_e | keysyms::KEY_i | keysyms::KEY_Right => {
                if let Some(ap) = self.wifi.as_ref().and_then(|w| w.selected()).cloned() {
                    self.wifi_open_form(&ap.ssid, &ap.path, false);
                }
            }
            // Скрытая сеть: имя вводится руками.
            keysyms::KEY_h => self.wifi_open_form("", "/", true),
            keysyms::KEY_p => {
                let on = self.wifi.as_ref().is_some_and(|w| w.snap.enabled);
                self.wifi_send(Cmd::Radio(!on));
            }
            _ => {}
        }
        true
    }

    /// Клик при открытом меню. `true` — клик съеден.
    pub fn wifi_click(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Physical>) -> bool {
        if !self.wifi_menu_open() {
            return false;
        }
        if self.wifi_form_open() {
            let (hit, внутри) = self
                .wifi
                .as_ref()
                .and_then(|w| w.форма.as_ref())
                .map(|ф| {
                    (
                        ф.rows.iter().find(|(r, _)| r.hit(pos.x, pos.y)).map(|(_, п)| *п),
                        ф.rows.first().is_some(),
                    )
                })
                .unwrap_or((None, false));
            match hit {
                Some(поле) => {
                    if let Some(ф) = self.wifi.as_mut().and_then(|w| w.форма.as_mut()) {
                        ф.фокус = поле;
                    }
                    if !matches!(поле.вид(), crate::wifi_form::Вид::Текст { .. }) {
                        self.wifi_form_press(поле, 1);
                    }
                }
                None => {
                    // Мимо панели формы — назад к списку.
                    let мимо = self.wifi.as_ref().and_then(|w| w.форма.as_ref())
                        .is_some_and(|ф| !ф.панель.hit(pos.x, pos.y));
                    if мимо && внутри {
                        if let Some(w) = self.wifi.as_mut() {
                            w.форма = None;
                        }
                    }
                }
            }
            self.request_redraw();
            return true;
        }
        let hit = self.wifi.as_ref().and_then(|w| {
            w.rows.iter().find(|(r, _)| r.hit(pos.x, pos.y)).map(|(_, i)| *i)
        });
        match hit {
            Some(idx) => {
                if let Some(w) = self.wifi.as_mut() {
                    w.sel = idx;
                }
                self.wifi_activate();
            }
            None => self.wifi_toggle_menu(),
        }
        true
    }
}
