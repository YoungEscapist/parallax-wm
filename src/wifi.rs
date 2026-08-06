//! Вайфай внутри композитора: NetworkManager по D-Bus, меню сетей и значок.
//!
//! Устроено как блютуз (см. bluetooth.rs) и по той же причине: панели и трея в
//! сессии dawn нет, а «подключись к сети» нужно каждый день. Поток D-Bus —
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
}

pub enum Event {
    State(Snapshot),
    Notice(String),
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
}

// ── Поток D-Bus ──────────────────────────────────────────────────────────────

pub fn spawn(to_dawn: channel::Sender<Event>) -> Option<mpsc::Sender<Cmd>> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let ok = std::thread::Builder::new()
        .name("dawn-wifi".into())
        .spawn(move || serve(to_dawn, rx))
        .is_ok();
    ok.then_some(tx)
}

fn serve(to_dawn: channel::Sender<Event>, rx: mpsc::Receiver<Cmd>) {
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
                if let Err(err) = run_cmd(c, &cmd) {
                    let _ = to_dawn.send(Event::Notice(human_error(&err)));
                }
                last = None;
            } else {
                let _ = to_dawn.send(Event::Notice("NetworkManager is not available".into()));
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
                    if to_dawn.send(Event::State(snap)).is_err() {
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

    let mut snap = Snapshot {
        present: true,
        enabled,
        connecting: (30..100).contains(&dev_state),
        ssid: None,
        strength: 0,
        aps: Vec::new(),
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
            ssid,
            strength,
            path,
        });
    }
    // Подключённая сверху, затем известные, дальше по уровню сигнала.
    snap.aps.sort_by(|a, b| {
        let rank = |x: &Ap| if x.active { 0 } else if x.saved { 1 } else { 2 };
        rank(a)
            .cmp(&rank(b))
            .then_with(|| b.strength.cmp(&a.strength))
            .then_with(|| a.ssid.to_lowercase().cmp(&b.ssid.to_lowercase()))
    });
    Some(snap)
}

fn run_cmd(conn: &zbus::blocking::Connection, cmd: &Cmd) -> zbus::Result<()> {
    match cmd {
        Cmd::Watch { .. } => Ok(()),
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

impl crate::state::Dawn {
    pub fn init_wifi(&mut self, tx: mpsc::Sender<Cmd>) {
        self.wifi = Some(WifiUi {
            tx,
            snap: Snapshot::default(),
            open: false,
            sel: 0,
            ask: None,
            notice: None,
            rows: Vec::new(),
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
                tracing::info!("dawn/wifi: {}", text);
                w.notice = Some((text, Instant::now()));
            }
        }
        self.request_redraw();
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
                tracing::warn!("dawn/wifi: поток NetworkManager не отвечает");
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
            tracing::warn!("dawn/wifi: поток не поднят (нет системной шины?)");
            return;
        };
        w.open = !w.open;
        w.ask = None;
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
        if ap.secure && !ap.saved {
            if let Some(w) = self.wifi.as_mut() {
                w.ask = Some(Ask {
                    ssid: ap.ssid.clone(),
                    ap: ap.path.clone(),
                    sae: ap.sae,
                    text: String::new(),
                });
            }
            self.request_redraw();
            return;
        }
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
