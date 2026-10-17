//! Полная настройка подключения Wi-Fi — как в GNOME и KDE.
//!
//! Просьба Ярика 30.09.2026: «полноценное меню подключения с полной
//! настройкой и информацией как в gnome или kde, с полной настройкой identity
//! и других данных для подключения, если они требуются».
//!
//! Здесь живут три вещи:
//!
//!  · **модель** — [`Профиль`]: всё, что человек может задать для сети
//!    (защита, пароль, 802.1X с identity/анонимной identity/сертификатами,
//!    IPv4/IPv6 вручную или автоматически, DNS, MAC, лимитное соединение,
//!    автоподключение, скрытая сеть), и [`Сведения`] — что сейчас происходит
//!    с подключением (адреса, шлюз, DNS, частота, скорость, BSSID);
//!  · **перевод в NetworkManager и обратно** — [`из_nm`] и [`в_nm`]. Правка
//!    сохранённого профиля идёт ПОВЕРХ его настроек: `Update` у NM заменяет
//!    профиль целиком, и собрать словарь с нуля значило бы молча стереть всё,
//!    чего форма не знает (маршруты, прокси, приоритет);
//!  · **форма** — [`Форма`]: какие поля показывать при текущих значениях (EAP
//!    TLS просит сертификаты, PEAP — фазу 2 и identity, «вручную» у IPv4 —
//!    адрес и шлюз) и как их править с клавиатуры и мышью. Отрисовка —
//!    `udev::build_wifi_form_elements`.

use std::collections::HashMap;

use zbus::zvariant::{OwnedValue, Value};

// ── Модель ───────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Защита {
    Нет,
    Wep,
    WpaPsk,
    Sae,
    Owe,
    WpaEap,
    Wpa3Eap,
}

impl Защита {
    pub const ВСЕ: [Защита; 7] = [
        Защита::Нет, Защита::Wep, Защита::WpaPsk, Защита::Sae,
        Защита::Owe, Защита::WpaEap, Защита::Wpa3Eap,
    ];
    pub fn имя(self) -> &'static str {
        match self {
            Защита::Нет => crate::т!("Нет", "None"),
            Защита::Wep => "WEP",
            Защита::WpaPsk => crate::т!("WPA/WPA2 Personal", "WPA/WPA2 Personal"),
            Защита::Sae => crate::т!("WPA3 Personal", "WPA3 Personal"),
            Защита::Owe => crate::т!("Enhanced Open (OWE)", "Enhanced Open (OWE)"),
            Защита::WpaEap => crate::т!("WPA/WPA2 Enterprise", "WPA/WPA2 Enterprise"),
            Защита::Wpa3Eap => crate::т!("WPA3 Enterprise", "WPA3 Enterprise"),
        }
    }
    pub fn личная(self) -> bool {
        matches!(self, Защита::Wep | Защита::WpaPsk | Защита::Sae)
    }
    pub fn предприятие(self) -> bool {
        matches!(self, Защита::WpaEap | Защита::Wpa3Eap)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Eap {
    Peap,
    Ttls,
    Tls,
    Pwd,
    Fast,
    Leap,
}

impl Eap {
    pub const ВСЕ: [Eap; 6] = [Eap::Peap, Eap::Ttls, Eap::Tls, Eap::Pwd, Eap::Fast, Eap::Leap];
    pub fn nm(self) -> &'static str {
        match self {
            Eap::Peap => "peap",
            Eap::Ttls => "ttls",
            Eap::Tls => "tls",
            Eap::Pwd => "pwd",
            Eap::Fast => "fast",
            Eap::Leap => "leap",
        }
    }
    pub fn имя(self) -> &'static str {
        match self {
            Eap::Peap => "PEAP",
            Eap::Ttls => "TTLS",
            Eap::Tls => "TLS",
            Eap::Pwd => "PWD",
            Eap::Fast => "FAST",
            Eap::Leap => "LEAP",
        }
    }
    fn из_nm(s: &str) -> Option<Self> {
        Eap::ВСЕ.into_iter().find(|e| e.nm() == s)
    }
    pub fn фаза2(self) -> bool {
        matches!(self, Eap::Peap | Eap::Ttls | Eap::Fast)
    }
    pub fn анонимная(self) -> bool {
        matches!(self, Eap::Peap | Eap::Ttls | Eap::Fast)
    }
    pub fn пароль(self) -> bool {
        !matches!(self, Eap::Tls)
    }
    pub fn ca(self) -> bool {
        matches!(self, Eap::Peap | Eap::Ttls | Eap::Tls)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Фаза2 {
    Mschapv2,
    Mschap,
    Pap,
    Chap,
    Gtc,
    Md5,
}

impl Фаза2 {
    pub const ВСЕ: [Фаза2; 6] = [Фаза2::Mschapv2, Фаза2::Mschap, Фаза2::Pap, Фаза2::Chap, Фаза2::Gtc, Фаза2::Md5];
    pub fn nm(self) -> &'static str {
        match self {
            Фаза2::Mschapv2 => "mschapv2",
            Фаза2::Mschap => "mschap",
            Фаза2::Pap => "pap",
            Фаза2::Chap => "chap",
            Фаза2::Gtc => "gtc",
            Фаза2::Md5 => "md5",
        }
    }
    pub fn имя(self) -> &'static str {
        match self {
            Фаза2::Mschapv2 => "MSCHAPv2",
            Фаза2::Mschap => "MSCHAP",
            Фаза2::Pap => "PAP",
            Фаза2::Chap => "CHAP",
            Фаза2::Gtc => "GTC",
            Фаза2::Md5 => "MD5",
        }
    }
    fn из_nm(s: &str) -> Option<Self> {
        Фаза2::ВСЕ.into_iter().find(|e| e.nm() == s)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ip {
    Авто,
    ТолькоDhcp,
    Вручную,
    Локальный,
    Выкл,
}

impl Ip {
    pub const V4: [Ip; 4] = [Ip::Авто, Ip::Вручную, Ip::Локальный, Ip::Выкл];
    pub const V6: [Ip; 5] = [Ip::Авто, Ip::ТолькоDhcp, Ip::Вручную, Ip::Локальный, Ip::Выкл];
    pub fn имя(self) -> &'static str {
        match self {
            Ip::Авто => crate::т!("Автоматически (DHCP)", "Automatic (DHCP)"),
            Ip::ТолькоDhcp => crate::т!("Только DHCPv6", "DHCPv6 only"),
            Ip::Вручную => crate::т!("Вручную", "Manual"),
            Ip::Локальный => crate::т!("Только локальная сеть", "Link-local only"),
            Ip::Выкл => crate::т!("Отключено", "Disabled"),
        }
    }
    fn nm(self, v6: bool) -> &'static str {
        match self {
            Ip::Авто => "auto",
            Ip::ТолькоDhcp => "dhcp",
            Ip::Вручную => "manual",
            Ip::Локальный => "link-local",
            Ip::Выкл => if v6 { "disabled" } else { "disabled" },
        }
    }
    fn из_nm(s: &str) -> Self {
        match s {
            "manual" => Ip::Вручную,
            "link-local" => Ip::Локальный,
            "disabled" | "ignore" => Ip::Выкл,
            "dhcp" => Ip::ТолькоDhcp,
            _ => Ip::Авто,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Мак {
    Поумолчанию,
    Свой,
    Случайный,
    Стабильный,
}

impl Мак {
    pub const ВСЕ: [Мак; 4] = [Мак::Поумолчанию, Мак::Свой, Мак::Случайный, Мак::Стабильный];
    pub fn имя(self) -> &'static str {
        match self {
            Мак::Поумолчанию => crate::т!("По умолчанию", "Default"),
            Мак::Свой => crate::т!("Настоящий", "Permanent"),
            Мак::Случайный => crate::т!("Случайный", "Random"),
            Мак::Стабильный => crate::т!("Стабильный для сети", "Stable per network"),
        }
    }
    fn nm(self) -> Option<&'static str> {
        match self {
            Мак::Поумолчанию => None,
            Мак::Свой => Some("permanent"),
            Мак::Случайный => Some("random"),
            Мак::Стабильный => Some("stable"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Лимит {
    Авто,
    Да,
    Нет,
}

impl Лимит {
    pub const ВСЕ: [Лимит; 3] = [Лимит::Авто, Лимит::Да, Лимит::Нет];
    pub fn имя(self) -> &'static str {
        match self {
            Лимит::Авто => crate::т!("Определить само", "Automatic"),
            Лимит::Да => crate::т!("Да", "Yes"),
            Лимит::Нет => crate::т!("Нет", "No"),
        }
    }
}

/// Всё, что настраивается в сети.
#[derive(Clone, Debug, PartialEq)]
pub struct Профиль {
    pub ssid: String,
    pub скрытая: bool,
    pub защита: Защита,
    /// WPA-ключ или WEP-ключ.
    pub пароль: String,
    pub eap: Eap,
    pub фаза2: Фаза2,
    pub identity: String,
    pub анонимная: String,
    pub eap_пароль: String,
    pub ca: String,
    /// Не проверять сервер (без CA). Если CA не задан и флаг снят —
    /// берутся системные сертификаты.
    pub без_ca: bool,
    pub домен: String,
    pub клиент_серт: String,
    pub ключ: String,
    pub ключ_пароль: String,
    pub ipv4: Ip,
    /// «192.168.1.50/24».
    pub ipv4_адрес: String,
    pub ipv4_шлюз: String,
    /// Через запятую.
    pub ipv4_dns: String,
    pub ipv6: Ip,
    pub ipv6_адрес: String,
    pub ipv6_шлюз: String,
    pub ipv6_dns: String,
    pub авто: bool,
    pub всем: bool,
    pub лимит: Лимит,
    pub мак: Мак,
    /// Путь сохранённого профиля в NM, если он есть.
    pub путь: Option<String>,
    /// Имя профиля (connection.id).
    pub имя: String,
}

impl Default for Профиль {
    fn default() -> Self {
        Self {
            ssid: String::new(),
            скрытая: false,
            защита: Защита::WpaPsk,
            пароль: String::new(),
            eap: Eap::Peap,
            фаза2: Фаза2::Mschapv2,
            identity: String::new(),
            анонимная: String::new(),
            eap_пароль: String::new(),
            ca: String::new(),
            без_ca: false,
            домен: String::new(),
            клиент_серт: String::new(),
            ключ: String::new(),
            ключ_пароль: String::new(),
            ipv4: Ip::Авто,
            ipv4_адрес: String::new(),
            ipv4_шлюз: String::new(),
            ipv4_dns: String::new(),
            ipv6: Ip::Авто,
            ipv6_адрес: String::new(),
            ipv6_шлюз: String::new(),
            ipv6_dns: String::new(),
            авто: true,
            всем: true,
            лимит: Лимит::Авто,
            мак: Мак::Поумолчанию,
            путь: None,
            имя: String::new(),
        }
    }
}

/// Что происходит с сетью прямо сейчас — для раздела «Сведения».
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Сведения {
    pub сигнал: Option<u8>,
    pub частота: Option<u32>,
    pub скорость: Option<u32>,
    pub bssid: String,
    pub защита: String,
    pub подключена: bool,
    pub интерфейс: String,
    pub мак: String,
    pub ipv4: Vec<String>,
    pub шлюз4: String,
    pub dns4: Vec<String>,
    pub ipv6: Vec<String>,
    pub шлюз6: String,
    pub dns6: Vec<String>,
}

/// Канал по частоте (МГц): 2.4, 5 и 6 ГГц.
pub fn канал(мгц: u32) -> Option<u32> {
    match мгц {
        2412..=2472 => Some((мгц - 2407) / 5),
        2484 => Some(14),
        5000..=5900 => Some((мгц - 5000) / 5),
        5955..=7115 => Some((мгц - 5950) / 5),
        _ => None,
    }
}

// ── Разбор настроек NM ───────────────────────────────────────────────────────

pub type Настройки = HashMap<String, HashMap<String, OwnedValue>>;

fn строка(н: &Настройки, раздел: &str, ключ: &str) -> Option<String> {
    let v = н.get(раздел)?.get(ключ)?;
    String::try_from(v.try_clone().ok()?).ok()
}

fn флаг(н: &Настройки, раздел: &str, ключ: &str) -> Option<bool> {
    let v = н.get(раздел)?.get(ключ)?;
    bool::try_from(v.try_clone().ok()?).ok()
}

fn число(н: &Настройки, раздел: &str, ключ: &str) -> Option<i64> {
    let v = н.get(раздел)?.get(ключ)?;
    let v = v.try_clone().ok()?;
    i32::try_from(v.try_clone().ok()?).map(i64::from)
        .or_else(|_| u32::try_from(v.try_clone().ok().unwrap()).map(i64::from))
        .ok()
}

fn байты(н: &Настройки, раздел: &str, ключ: &str) -> Option<Vec<u8>> {
    let v = н.get(раздел)?.get(ключ)?;
    Vec::<u8>::try_from(v.try_clone().ok()?).ok()
}

fn строки(н: &Настройки, раздел: &str, ключ: &str) -> Vec<String> {
    н.get(раздел)
        .and_then(|р| р.get(ключ))
        .and_then(|v| v.try_clone().ok())
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default()
}

/// Путь из «file:///путь\0» (так NM хранит сертификаты).
fn путь_из_байт(b: &[u8]) -> String {
    let s = String::from_utf8_lossy(b);
    s.trim_end_matches('\0').strip_prefix("file://").unwrap_or(&s).trim_end_matches('\0').to_string()
}

fn байты_пути(путь: &str) -> Vec<u8> {
    let mut v = format!("file://{}", путь.trim()).into_bytes();
    v.push(0);
    v
}

/// Первый адрес из address-data: «адрес/префикс».
fn адрес_из(н: &Настройки, раздел: &str) -> String {
    let Some(v) = н.get(раздел).and_then(|р| р.get("address-data")) else { return String::new() };
    let Ok(v) = v.try_clone() else { return String::new() };
    let Ok(список) = Vec::<HashMap<String, OwnedValue>>::try_from(v) else { return String::new() };
    список
        .first()
        .and_then(|a| {
            let адрес = String::try_from(a.get("address")?.try_clone().ok()?).ok()?;
            let префикс = u32::try_from(a.get("prefix")?.try_clone().ok()?).ok()?;
            Some(format!("{адрес}/{префикс}"))
        })
        .unwrap_or_default()
}

fn dns4_из(н: &Настройки) -> String {
    let Some(v) = н.get("ipv4").and_then(|р| р.get("dns")).and_then(|v| v.try_clone().ok()) else {
        return String::new();
    };
    let Ok(список) = Vec::<u32>::try_from(v) else { return String::new() };
    список
        .iter()
        .map(|x| std::net::Ipv4Addr::from(x.to_le_bytes()).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn dns6_из(н: &Настройки) -> String {
    let Some(v) = н.get("ipv6").and_then(|р| р.get("dns")).and_then(|v| v.try_clone().ok()) else {
        return String::new();
    };
    let Ok(список) = Vec::<Vec<u8>>::try_from(v) else { return String::new() };
    список
        .iter()
        .filter_map(|b| <[u8; 16]>::try_from(b.as_slice()).ok())
        .map(|b| std::net::Ipv6Addr::from(b).to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Профиль из настроек NM (вместе с секретами — их NM отдаёт отдельно,
/// `GetSecrets`, и сюда они приходят уже вмёрженными).
pub fn из_nm(н: &Настройки, путь: Option<String>) -> Профиль {
    let mut п = Профиль { путь, ..Default::default() };
    п.имя = строка(н, "connection", "id").unwrap_or_default();
    п.ssid = байты(н, "802-11-wireless", "ssid")
        .map(|b| String::from_utf8_lossy(&b).to_string())
        .unwrap_or_default();
    п.скрытая = флаг(н, "802-11-wireless", "hidden").unwrap_or(false);
    п.мак = match строка(н, "802-11-wireless", "cloned-mac-address").as_deref() {
        Some("permanent") => Мак::Свой,
        Some("random") => Мак::Случайный,
        Some("stable") => Мак::Стабильный,
        _ => Мак::Поумолчанию,
    };
    let km = строка(н, "802-11-wireless-security", "key-mgmt");
    п.защита = match km.as_deref() {
        None => Защита::Нет,
        Some("none") | Some("ieee8021x") => Защита::Wep,
        Some("wpa-psk") => Защита::WpaPsk,
        Some("sae") => Защита::Sae,
        Some("owe") => Защита::Owe,
        Some("wpa-eap-suite-b-192") => Защита::Wpa3Eap,
        Some("wpa-eap") => {
            if число(н, "802-11-wireless-security", "pmf") == Some(3) { Защита::Wpa3Eap } else { Защита::WpaEap }
        }
        Some(_) => Защита::WpaPsk,
    };
    п.пароль = match п.защита {
        Защита::Wep => строка(н, "802-11-wireless-security", "wep-key0").unwrap_or_default(),
        _ => строка(н, "802-11-wireless-security", "psk").unwrap_or_default(),
    };
    if let Some(e) = строки(н, "802-1x", "eap").first().and_then(|s| Eap::из_nm(s)) {
        п.eap = e;
    }
    if let Some(f) = строка(н, "802-1x", "phase2-auth")
        .or_else(|| строка(н, "802-1x", "phase2-autheap"))
        .and_then(|s| Фаза2::из_nm(&s))
    {
        п.фаза2 = f;
    }
    п.identity = строка(н, "802-1x", "identity").unwrap_or_default();
    п.анонимная = строка(н, "802-1x", "anonymous-identity").unwrap_or_default();
    п.eap_пароль = строка(н, "802-1x", "password").unwrap_or_default();
    п.ca = байты(н, "802-1x", "ca-cert").map(|b| путь_из_байт(&b)).unwrap_or_default();
    п.без_ca = п.ca.is_empty() && !флаг(н, "802-1x", "system-ca-certs").unwrap_or(false)
        && н.contains_key("802-1x");
    п.домен = строка(н, "802-1x", "domain-suffix-match").unwrap_or_default();
    п.клиент_серт = байты(н, "802-1x", "client-cert").map(|b| путь_из_байт(&b)).unwrap_or_default();
    п.ключ = байты(н, "802-1x", "private-key").map(|b| путь_из_байт(&b)).unwrap_or_default();
    п.ключ_пароль = строка(н, "802-1x", "private-key-password").unwrap_or_default();

    п.ipv4 = Ip::из_nm(&строка(н, "ipv4", "method").unwrap_or_else(|| "auto".into()));
    п.ipv4_адрес = адрес_из(н, "ipv4");
    п.ipv4_шлюз = строка(н, "ipv4", "gateway").unwrap_or_default();
    п.ipv4_dns = dns4_из(н);
    п.ipv6 = Ip::из_nm(&строка(н, "ipv6", "method").unwrap_or_else(|| "auto".into()));
    п.ipv6_адрес = адрес_из(н, "ipv6");
    п.ipv6_шлюз = строка(н, "ipv6", "gateway").unwrap_or_default();
    п.ipv6_dns = dns6_из(н);

    п.авто = флаг(н, "connection", "autoconnect").unwrap_or(true);
    п.всем = строки(н, "connection", "permissions").is_empty();
    п.лимит = match число(н, "connection", "metered") {
        Some(1) | Some(3) => Лимит::Да,
        Some(2) | Some(4) => Лимит::Нет,
        _ => Лимит::Авто,
    };
    п
}

// ── Сборка настроек для NM ───────────────────────────────────────────────────

fn ov(v: Value<'_>) -> OwnedValue {
    OwnedValue::try_from(v).expect("простые значения переводятся всегда")
}

/// Разобрать «a.b.c.d/nn» (без префикса — /24 у IPv4, /64 у IPv6).
fn адрес_префикс(s: &str, v6: bool) -> Option<(String, u32)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (адрес, префикс) = match s.split_once('/') {
        Some((a, p)) => (a.trim(), p.trim().parse::<u32>().ok()?),
        None => (s, if v6 { 64 } else { 24 }),
    };
    if v6 {
        адрес.parse::<std::net::Ipv6Addr>().ok()?;
        (префикс <= 128).then(|| (адрес.to_string(), префикс))
    } else {
        адрес.parse::<std::net::Ipv4Addr>().ok()?;
        (префикс <= 32).then(|| (адрес.to_string(), префикс))
    }
}

/// Проверить профиль до отправки: человеческая ошибка вместо отказа NM.
pub fn проверить(п: &Профиль) -> Result<(), String> {
    if п.ssid.trim().is_empty() {
        return Err(crate::т!("не задано имя сети", "the network name is empty").into());
    }
    if п.ssid.len() > 32 {
        return Err(crate::т!("имя сети длиннее 32 байт", "the network name is longer than 32 bytes").into());
    }
    match п.защита {
        Защита::WpaPsk | Защита::Sae if п.пароль.len() < 8 && п.путь.is_none() => {
            return Err(crate::т!("пароль WPA — не короче 8 символов", "a WPA password is at least 8 characters").into());
        }
        Защита::WpaPsk | Защита::Sae if !п.пароль.is_empty() && п.пароль.len() < 8 => {
            return Err(crate::т!("пароль WPA — не короче 8 символов", "a WPA password is at least 8 characters").into());
        }
        Защита::Wep if п.пароль.is_empty() && п.путь.is_none() => {
            return Err(crate::т!("нужен ключ WEP", "a WEP key is required").into());
        }
        Защита::WpaEap | Защита::Wpa3Eap => {
            if п.identity.trim().is_empty() && п.eap != Eap::Tls {
                return Err(crate::т!("нужна identity (логин)", "an identity (login) is required").into());
            }
            if п.eap == Eap::Tls && (п.клиент_серт.trim().is_empty() || п.ключ.trim().is_empty()) {
                return Err(crate::т!("для TLS нужны сертификат и ключ клиента", "TLS needs a client certificate and key").into());
            }
            for путь in [&п.ca, &п.клиент_серт, &п.ключ] {
                if !путь.trim().is_empty() && !std::path::Path::new(путь.trim()).is_file() {
                    return Err(crate::тф!("нет файла {}", "no such file {}", путь.trim()));
                }
            }
        }
        _ => {}
    }
    if п.ipv4 == Ip::Вручную {
        if адрес_префикс(&п.ipv4_адрес, false).is_none() {
            return Err(crate::т!("IPv4-адрес вида 192.168.1.50/24", "an IPv4 address like 192.168.1.50/24").into());
        }
        if !п.ipv4_шлюз.trim().is_empty() && п.ipv4_шлюз.trim().parse::<std::net::Ipv4Addr>().is_err() {
            return Err(crate::т!("шлюз IPv4 не разобран", "the IPv4 gateway does not parse").into());
        }
    }
    if п.ipv6 == Ip::Вручную {
        if адрес_префикс(&п.ipv6_адрес, true).is_none() {
            return Err(crate::т!("IPv6-адрес вида fd00::10/64", "an IPv6 address like fd00::10/64").into());
        }
        if !п.ipv6_шлюз.trim().is_empty() && п.ipv6_шлюз.trim().parse::<std::net::Ipv6Addr>().is_err() {
            return Err(crate::т!("шлюз IPv6 не разобран", "the IPv6 gateway does not parse").into());
        }
    }
    for d in dns_список(&п.ipv4_dns) {
        if d.parse::<std::net::Ipv4Addr>().is_err() {
            return Err(crate::тф!("DNS IPv4 «{}» не разобран", "IPv4 DNS \"{}\" does not parse", d));
        }
    }
    for d in dns_список(&п.ipv6_dns) {
        if d.parse::<std::net::Ipv6Addr>().is_err() {
            return Err(crate::тф!("DNS IPv6 «{}» не разобран", "IPv6 DNS \"{}\" does not parse", d));
        }
    }
    Ok(())
}

fn dns_список(s: &str) -> Vec<String> {
    s.split([',', ' ', ';'])
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
        .collect()
}

/// Настройки NM из профиля — поверх `база` (сохранённого профиля целиком,
/// с секретами), чтобы не стереть то, чего форма не знает. Для новой сети
/// `база` пустая.
pub fn в_nm(п: &Профиль, база: Настройки, пользователь: &str) -> Настройки {
    let mut н = база;
    let раздел = |н: &mut Настройки, имя: &str| -> () {
        н.entry(имя.to_string()).or_default();
    };

    // connection
    раздел(&mut н, "connection");
    {
        let c = н.get_mut("connection").unwrap();
        if !c.contains_key("id") || c.get("id").is_none() {
            c.insert("id".into(), ov(Value::from(if п.имя.is_empty() { п.ssid.clone() } else { п.имя.clone() })));
        }
        c.insert("type".into(), ov(Value::from("802-11-wireless")));
        c.insert("autoconnect".into(), ov(Value::from(п.авто)));
        let права: Vec<String> = if п.всем { Vec::new() } else { vec![format!("user:{пользователь}:")] };
        c.insert("permissions".into(), ov(Value::from(права)));
        let лимит: i32 = match п.лимит {
            Лимит::Авто => 0,
            Лимит::Да => 1,
            Лимит::Нет => 2,
        };
        c.insert("metered".into(), ov(Value::from(лимит)));
    }

    // 802-11-wireless
    раздел(&mut н, "802-11-wireless");
    {
        let w = н.get_mut("802-11-wireless").unwrap();
        w.insert("ssid".into(), ov(Value::from(п.ssid.as_bytes().to_vec())));
        w.insert("mode".into(), ov(Value::from("infrastructure")));
        w.insert("hidden".into(), ov(Value::from(п.скрытая)));
        match п.мак.nm() {
            Some(m) => {
                w.insert("cloned-mac-address".into(), ov(Value::from(m)));
            }
            None => {
                w.remove("cloned-mac-address");
                w.remove("assigned-mac-address");
            }
        }
        if п.защита == Защита::Нет {
            w.remove("security");
        } else {
            w.insert("security".into(), ov(Value::from("802-11-wireless-security")));
        }
    }

    // Защита
    if п.защита == Защита::Нет {
        н.remove("802-11-wireless-security");
        н.remove("802-1x");
    } else {
        раздел(&mut н, "802-11-wireless-security");
        let s = н.get_mut("802-11-wireless-security").unwrap();
        // Ключи прежней защиты сносим: psk от WPA рядом с WEP сбил бы NM.
        // Секреты (psk, wep-key0) сносим, только если защита сменилась:
        // пустое поле пароля у сохранённой сети значит «не менять», а NM мог
        // и не отдать секрет (нет прав на GetSecrets).
        for k in ["wep-key-type", "wep-tx-keyidx", "auth-alg", "pmf", "proto", "pairwise", "group"] {
            s.remove(k);
        }
        match п.защита {
            Защита::Wep => {
                s.remove("psk");
            }
            Защита::WpaPsk | Защита::Sae => {
                s.remove("wep-key0");
            }
            _ => {
                s.remove("psk");
                s.remove("wep-key0");
            }
        }
        let km = match п.защита {
            Защита::Wep => "none",
            Защита::WpaPsk => "wpa-psk",
            Защита::Sae => "sae",
            Защита::Owe => "owe",
            Защита::WpaEap | Защита::Wpa3Eap => "wpa-eap",
            Защита::Нет => unreachable!(),
        };
        s.insert("key-mgmt".into(), ov(Value::from(km)));
        match п.защита {
            Защита::Wep => {
                if !п.пароль.is_empty() {
                    s.insert("wep-key0".into(), ov(Value::from(п.пароль.as_str())));
                }
                // 1 — ключ как есть (hex/ASCII), 2 — пароль-фраза.
                let тип: u32 = if matches!(п.пароль.len(), 5 | 10 | 13 | 26) { 1 } else { 2 };
                s.insert("wep-key-type".into(), ov(Value::from(тип)));
                s.insert("auth-alg".into(), ov(Value::from("open")));
            }
            Защита::WpaPsk | Защита::Sae => {
                if !п.пароль.is_empty() {
                    s.insert("psk".into(), ov(Value::from(п.пароль.as_str())));
                }
                if п.защита == Защита::Sae {
                    s.insert("pmf".into(), ov(Value::from(3i32)));
                }
            }
            Защита::Wpa3Eap => {
                s.insert("pmf".into(), ov(Value::from(3i32)));
            }
            _ => {}
        }
        if п.защита.предприятие() {
            раздел(&mut н, "802-1x");
            let x = н.get_mut("802-1x").unwrap();
            // Пароли — как psk выше: пустое поле значит «оставить как было».
            for k in [
                "eap", "phase2-auth", "phase2-autheap", "identity", "anonymous-identity",
                "ca-cert", "system-ca-certs", "domain-suffix-match",
                "client-cert", "private-key", "phase1-fast-provisioning",
            ] {
                x.remove(k);
            }
            if !п.eap.пароль() {
                x.remove("password");
            }
            if п.eap != Eap::Tls {
                x.remove("private-key-password");
            }
            x.insert("eap".into(), ov(Value::from(vec![п.eap.nm().to_string()])));
            if п.eap.фаза2() {
                x.insert("phase2-auth".into(), ov(Value::from(п.фаза2.nm())));
            }
            if п.eap == Eap::Fast {
                x.insert("phase1-fast-provisioning".into(), ov(Value::from("3")));
            }
            if !п.identity.trim().is_empty() {
                x.insert("identity".into(), ov(Value::from(п.identity.trim())));
            }
            if п.eap.анонимная() && !п.анонимная.trim().is_empty() {
                x.insert("anonymous-identity".into(), ov(Value::from(п.анонимная.trim())));
            }
            if п.eap.пароль() && !п.eap_пароль.is_empty() {
                x.insert("password".into(), ov(Value::from(п.eap_пароль.as_str())));
            }
            if п.eap.ca() {
                if !п.ca.trim().is_empty() {
                    x.insert("ca-cert".into(), ov(Value::from(байты_пути(&п.ca))));
                } else if !п.без_ca {
                    x.insert("system-ca-certs".into(), ov(Value::from(true)));
                }
                if !п.домен.trim().is_empty() {
                    x.insert("domain-suffix-match".into(), ov(Value::from(п.домен.trim())));
                }
            }
            if п.eap == Eap::Tls {
                x.insert("client-cert".into(), ov(Value::from(байты_пути(&п.клиент_серт))));
                x.insert("private-key".into(), ov(Value::from(байты_пути(&п.ключ))));
                if !п.ключ_пароль.is_empty() {
                    x.insert("private-key-password".into(), ov(Value::from(п.ключ_пароль.as_str())));
                }
            }
        } else {
            н.remove("802-1x");
        }
    }

    // IP
    for (v6, имя) in [(false, "ipv4"), (true, "ipv6")] {
        раздел(&mut н, имя);
        let r = н.get_mut(имя).unwrap();
        let (метод, адрес, шлюз, dns) = if v6 {
            (п.ipv6, &п.ipv6_адрес, &п.ipv6_шлюз, &п.ipv6_dns)
        } else {
            (п.ipv4, &п.ipv4_адрес, &п.ipv4_шлюз, &п.ipv4_dns)
        };
        // Старые формы адресов/маршрутов NM отдаёт рядом с новыми; присылать
        // обе — значит спорить с самим собой. Оставляем только *-data.
        r.remove("addresses");
        r.remove("routes");
        r.insert("method".into(), ov(Value::from(метод.nm(v6))));
        match (метод, адрес_префикс(адрес, v6)) {
            (Ip::Вручную, Some((a, p))) => {
                let mut запись: HashMap<String, OwnedValue> = HashMap::new();
                запись.insert("address".into(), ov(Value::from(a)));
                запись.insert("prefix".into(), ov(Value::from(p)));
                r.insert("address-data".into(), ov(Value::from(vec![запись])));
                if шлюз.trim().is_empty() {
                    r.remove("gateway");
                } else {
                    r.insert("gateway".into(), ov(Value::from(шлюз.trim())));
                }
            }
            _ => {
                r.insert(
                    "address-data".into(),
                    ov(Value::from(Vec::<HashMap<String, OwnedValue>>::new())),
                );
                r.remove("gateway");
            }
        }
        let список = dns_список(dns);
        if список.is_empty() {
            r.remove("dns");
            r.remove("ignore-auto-dns");
        } else {
            if v6 {
                let b: Vec<Vec<u8>> = список
                    .iter()
                    .filter_map(|d| d.parse::<std::net::Ipv6Addr>().ok())
                    .map(|a| a.octets().to_vec())
                    .collect();
                r.insert("dns".into(), ov(Value::from(b)));
            } else {
                let u: Vec<u32> = список
                    .iter()
                    .filter_map(|d| d.parse::<std::net::Ipv4Addr>().ok())
                    .map(|a| u32::from_le_bytes(a.octets()))
                    .collect();
                r.insert("dns".into(), ov(Value::from(u)));
            }
            // Свои DNS заданы — DHCP-шные не подмешиваем (как «Автоматически:
            // выкл» у DNS в GNOME).
            r.insert("ignore-auto-dns".into(), ov(Value::from(true)));
        }
    }
    н
}

// ── Форма ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Поле {
    Ssid,
    Защита,
    Пароль,
    Показать,
    Eap,
    Фаза2,
    Identity,
    Анонимная,
    EapПароль,
    Ca,
    БезCa,
    Домен,
    КлиентСерт,
    Ключ,
    КлючПароль,
    Ipv4,
    Ipv4Адрес,
    Ipv4Шлюз,
    Ipv4Dns,
    Ipv6,
    Ipv6Адрес,
    Ipv6Шлюз,
    Ipv6Dns,
    Авто,
    Всем,
    Лимит,
    Мак,
    Скрытая,
    Подключить,
    Сохранить,
    Забыть,
    Отмена,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Вид {
    Текст { секрет: bool },
    Выбор,
    Флаг,
    Кнопка,
}

impl Поле {
    pub fn вид(self) -> Вид {
        use Поле::*;
        match self {
            Пароль | EapПароль | КлючПароль => Вид::Текст { секрет: true },
            Ssid | Identity | Анонимная | Ca | Домен | КлиентСерт | Ключ | Ipv4Адрес | Ipv4Шлюз
            | Ipv4Dns | Ipv6Адрес | Ipv6Шлюз | Ipv6Dns => Вид::Текст { секрет: false },
            Защита | Eap | Фаза2 | Ipv4 | Ipv6 | Лимит | Мак => Вид::Выбор,
            Показать | БезCa | Авто | Всем | Скрытая => Вид::Флаг,
            Подключить | Сохранить | Забыть | Отмена => Вид::Кнопка,
        }
    }

    pub fn подпись(self) -> &'static str {
        use Поле::*;
        match self {
            Ssid => crate::т!("Имя сети (SSID)", "Network name (SSID)"),
            Защита => crate::т!("Защита", "Security"),
            Пароль => crate::т!("Пароль", "Password"),
            Показать => crate::т!("Показывать пароли", "Show passwords"),
            Eap => crate::т!("Аутентификация (EAP)", "Authentication (EAP)"),
            Фаза2 => crate::т!("Внутренняя (фаза 2)", "Inner (phase 2)"),
            Identity => crate::т!("Identity (логин)", "Identity (username)"),
            Анонимная => crate::т!("Анонимная identity", "Anonymous identity"),
            EapПароль => crate::т!("Пароль", "Password"),
            Ca => crate::т!("Сертификат CA (путь)", "CA certificate (path)"),
            БезCa => crate::т!("Не проверять сертификат", "No CA certificate is required"),
            Домен => crate::т!("Домен сервера", "Server domain"),
            КлиентСерт => crate::т!("Сертификат клиента (путь)", "User certificate (path)"),
            Ключ => crate::т!("Закрытый ключ (путь)", "Private key (path)"),
            КлючПароль => crate::т!("Пароль ключа", "Private key password"),
            Ipv4 => crate::т!("IPv4", "IPv4"),
            Ipv4Адрес => crate::т!("Адрес/префикс", "Address/prefix"),
            Ipv4Шлюз => crate::т!("Шлюз", "Gateway"),
            Ipv4Dns => crate::т!("DNS (пусто — от DHCP)", "DNS (empty = from DHCP)"),
            Ipv6 => crate::т!("IPv6", "IPv6"),
            Ipv6Адрес => crate::т!("Адрес/префикс", "Address/prefix"),
            Ipv6Шлюз => crate::т!("Шлюз", "Gateway"),
            Ipv6Dns => crate::т!("DNS (пусто — автоматически)", "DNS (empty = automatic)"),
            Авто => crate::т!("Подключаться автоматически", "Connect automatically"),
            Всем => crate::т!("Доступно всем пользователям", "Available to all users"),
            Лимит => crate::т!("Лимитное соединение", "Metered connection"),
            Мак => crate::т!("MAC-адрес", "MAC address"),
            Скрытая => crate::т!("Скрытая сеть", "Hidden network"),
            Подключить => crate::т!("Подключить", "Connect"),
            Сохранить => crate::т!("Сохранить", "Save"),
            Забыть => crate::т!("Забыть сеть", "Forget"),
            Отмена => crate::т!("Отмена", "Cancel"),
        }
    }
}

/// Строка формы: заголовок раздела, строка сведений или поле.
#[derive(Clone, Debug, PartialEq)]
pub enum Строка {
    Раздел(String),
    Сведение(String, String),
    Поле(Поле),
}

pub struct Форма {
    pub п: Профиль,
    pub исходный: Профиль,
    pub сведения: Option<Сведения>,
    /// Путь точки доступа (или "/" — скрытая сеть).
    pub ap: String,
    /// Можно ли править SSID (новая скрытая сеть).
    pub ssid_правится: bool,
    pub фокус: Поле,
    pub показать: bool,
    pub ошибка: Option<String>,
    /// Прямоугольники полей и кнопок в кадре — заполняет отрисовка.
    pub rows: Vec<(crate::bar::Rect, Поле)>,
    /// Первая видимая строка (прокрутка).
    pub прокрутка: usize,
    /// Профиль ещё грузится из NM.
    pub грузится: bool,
    /// Вся панель формы в кадре — клик мимо неё закрывает форму.
    pub панель: crate::bar::Rect,
}

impl Форма {
    pub fn новая(п: Профиль, ap: String, ssid_правится: bool) -> Self {
        let фокус = if ssid_правится { Поле::Ssid } else { Поле::Защита };
        Self {
            исходный: п.clone(),
            п,
            сведения: None,
            ap,
            ssid_правится,
            фокус,
            показать: false,
            ошибка: None,
            rows: Vec::new(),
            прокрутка: 0,
            грузится: false,
            панель: crate::bar::Rect::default(),
        }
    }

    /// Поставить фокус туда, куда человеку писать в первую очередь.
    pub fn фокус_на_главное(&mut self) {
        self.фокус = if self.ssid_правится && self.п.ssid.is_empty() {
            Поле::Ssid
        } else if self.п.защита.личная() {
            Поле::Пароль
        } else if self.п.защита.предприятие() {
            if self.п.eap == Eap::Tls { Поле::КлиентСерт } else { Поле::Identity }
        } else {
            Поле::Подключить
        };
    }

    /// Строки формы при текущих значениях — сверху вниз.
    pub fn строки(&self) -> Vec<Строка> {
        use Поле::*;
        let п = &self.п;
        let mut v = Vec::new();
        if let Some(с) = &self.сведения {
            v.push(Строка::Раздел(crate::т!("СВЕДЕНИЯ", "DETAILS").into()));
            let mut сведение = |к: &str, з: String| {
                if !з.is_empty() {
                    v.push(Строка::Сведение(к.to_string(), з));
                }
            };
            сведение(
                crate::т!("Состояние", "Status"),
                if с.подключена { crate::т!("подключено", "connected").into() } else { crate::т!("не подключено", "not connected").into() },
            );
            if let Some(s) = с.сигнал {
                сведение(crate::т!("Сигнал", "Signal"), format!("{s}%"));
            }
            if let Some(f) = с.частота {
                let к = канал(f).map(|к| format!(", {} {к}", crate::т!("канал", "channel"))).unwrap_or_default();
                сведение(crate::т!("Частота", "Frequency"), format!("{:.3} {}{к}", f as f64 / 1000.0, crate::т!("ГГц", "GHz")));
            }
            if let Some(r) = с.скорость {
                сведение(crate::т!("Скорость", "Link speed"), format!("{} {}", r / 1000, crate::т!("Мбит/с", "Mb/s")));
            }
            сведение(crate::т!("Защита", "Security"), с.защита.clone());
            сведение("BSSID", с.bssid.clone());
            сведение(crate::т!("Интерфейс", "Interface"), с.интерфейс.clone());
            сведение(crate::т!("MAC устройства", "Hardware address"), с.мак.clone());
            сведение(crate::т!("IPv4-адрес", "IPv4 address"), с.ipv4.join(", "));
            сведение(crate::т!("Шлюз IPv4", "IPv4 gateway"), с.шлюз4.clone());
            сведение("DNS IPv4", с.dns4.join(", "));
            сведение(crate::т!("IPv6-адрес", "IPv6 address"), с.ipv6.join(", "));
            сведение(crate::т!("Шлюз IPv6", "IPv6 gateway"), с.шлюз6.clone());
            сведение("DNS IPv6", с.dns6.join(", "));
        }
        v.push(Строка::Раздел(crate::т!("СЕТЬ", "NETWORK").into()));
        if self.ssid_правится {
            v.push(Строка::Поле(Ssid));
        }
        v.push(Строка::Поле(Защита));
        if п.защита.личная() {
            v.push(Строка::Поле(Пароль));
        }
        if п.защита.предприятие() {
            v.push(Строка::Поле(Eap));
            if п.eap.фаза2() {
                v.push(Строка::Поле(Фаза2));
            }
            v.push(Строка::Поле(Identity));
            if п.eap.анонимная() {
                v.push(Строка::Поле(Анонимная));
            }
            if п.eap.пароль() {
                v.push(Строка::Поле(EapПароль));
            }
            if п.eap == crate::wifi_form::Eap::Tls {
                v.push(Строка::Поле(КлиентСерт));
                v.push(Строка::Поле(Ключ));
                v.push(Строка::Поле(КлючПароль));
            }
            if п.eap.ca() {
                v.push(Строка::Поле(Ca));
                if п.ca.trim().is_empty() {
                    v.push(Строка::Поле(БезCa));
                }
                v.push(Строка::Поле(Домен));
            }
        }
        if п.защита.личная() || п.защита.предприятие() {
            v.push(Строка::Поле(Показать));
        }
        v.push(Строка::Раздел("IPv4".into()));
        v.push(Строка::Поле(Ipv4));
        if п.ipv4 == Ip::Вручную {
            v.push(Строка::Поле(Ipv4Адрес));
            v.push(Строка::Поле(Ipv4Шлюз));
        }
        if п.ipv4 != Ip::Выкл {
            v.push(Строка::Поле(Ipv4Dns));
        }
        v.push(Строка::Раздел("IPv6".into()));
        v.push(Строка::Поле(Ipv6));
        if п.ipv6 == Ip::Вручную {
            v.push(Строка::Поле(Ipv6Адрес));
            v.push(Строка::Поле(Ipv6Шлюз));
        }
        if п.ipv6 != Ip::Выкл {
            v.push(Строка::Поле(Ipv6Dns));
        }
        v.push(Строка::Раздел(crate::т!("ПРОЧЕЕ", "OTHER").into()));
        v.push(Строка::Поле(Авто));
        v.push(Строка::Поле(Всем));
        v.push(Строка::Поле(Лимит));
        v.push(Строка::Поле(Мак));
        v.push(Строка::Поле(Скрытая));
        v
    }

    /// Кнопки внизу формы.
    pub fn кнопки(&self) -> Vec<Поле> {
        let mut v = vec![Поле::Подключить, Поле::Сохранить];
        if self.п.путь.is_some() {
            v.push(Поле::Забыть);
        }
        v.push(Поле::Отмена);
        v
    }

    /// Все фокусируемые места по порядку: поля, затем кнопки.
    pub fn порядок(&self) -> Vec<Поле> {
        let mut v: Vec<Поле> = self
            .строки()
            .into_iter()
            .filter_map(|с| match с {
                Строка::Поле(п) => Some(п),
                _ => None,
            })
            .collect();
        v.extend(self.кнопки());
        v
    }

    pub fn сдвинуть_фокус(&mut self, шаг: i32) {
        let порядок = self.порядок();
        let n = порядок.len() as i32;
        if n == 0 {
            return;
        }
        let i = порядок.iter().position(|п| *п == self.фокус).unwrap_or(0) as i32;
        self.фокус = порядок[((i + шаг) % n + n) as usize % n as usize];
    }

    /// Строковое поле формы, если `поле` — текстовое.
    pub fn текст_mut(&mut self, поле: Поле) -> Option<&mut String> {
        let п = &mut self.п;
        Some(match поле {
            Поле::Ssid => &mut п.ssid,
            Поле::Пароль => &mut п.пароль,
            Поле::Identity => &mut п.identity,
            Поле::Анонимная => &mut п.анонимная,
            Поле::EapПароль => &mut п.eap_пароль,
            Поле::Ca => &mut п.ca,
            Поле::Домен => &mut п.домен,
            Поле::КлиентСерт => &mut п.клиент_серт,
            Поле::Ключ => &mut п.ключ,
            Поле::КлючПароль => &mut п.ключ_пароль,
            Поле::Ipv4Адрес => &mut п.ipv4_адрес,
            Поле::Ipv4Шлюз => &mut п.ipv4_шлюз,
            Поле::Ipv4Dns => &mut п.ipv4_dns,
            Поле::Ipv6Адрес => &mut п.ipv6_адрес,
            Поле::Ipv6Шлюз => &mut п.ipv6_шлюз,
            Поле::Ipv6Dns => &mut п.ipv6_dns,
            _ => return None,
        })
    }

    /// Значение поля для показа.
    pub fn значение(&self, поле: Поле) -> String {
        let п = &self.п;
        let да_нет = |b: bool| if b { crate::т!("да", "on") } else { crate::т!("нет", "off") }.to_string();
        match поле {
            Поле::Защита => п.защита.имя().into(),
            Поле::Eap => п.eap.имя().into(),
            Поле::Фаза2 => п.фаза2.имя().into(),
            Поле::Ipv4 => п.ipv4.имя().into(),
            Поле::Ipv6 => п.ipv6.имя().into(),
            Поле::Лимит => п.лимит.имя().into(),
            Поле::Мак => п.мак.имя().into(),
            Поле::Показать => да_нет(self.показать),
            Поле::БезCa => да_нет(п.без_ca),
            Поле::Авто => да_нет(п.авто),
            Поле::Всем => да_нет(п.всем),
            Поле::Скрытая => да_нет(п.скрытая),
            _ => {
                let секрет = matches!(поле.вид(), Вид::Текст { секрет: true });
                let mut s = match поле {
                    Поле::Ssid => п.ssid.clone(),
                    Поле::Пароль => п.пароль.clone(),
                    Поле::Identity => п.identity.clone(),
                    Поле::Анонимная => п.анонимная.clone(),
                    Поле::EapПароль => п.eap_пароль.clone(),
                    Поле::Ca => п.ca.clone(),
                    Поле::Домен => п.домен.clone(),
                    Поле::КлиентСерт => п.клиент_серт.clone(),
                    Поле::Ключ => п.ключ.clone(),
                    Поле::КлючПароль => п.ключ_пароль.clone(),
                    Поле::Ipv4Адрес => п.ipv4_адрес.clone(),
                    Поле::Ipv4Шлюз => п.ipv4_шлюз.clone(),
                    Поле::Ipv4Dns => п.ipv4_dns.clone(),
                    Поле::Ipv6Адрес => п.ipv6_адрес.clone(),
                    Поле::Ipv6Шлюз => п.ipv6_шлюз.clone(),
                    Поле::Ipv6Dns => п.ipv6_dns.clone(),
                    _ => String::new(),
                };
                if секрет && !self.показать {
                    s = "•".repeat(s.chars().count());
                }
                s
            }
        }
    }

    /// Следующее значение выбора (или переключение флага). `шаг` ±1.
    pub fn переключить(&mut self, поле: Поле, шаг: i32) {
        fn по_кругу<T: Copy + PartialEq>(все: &[T], сейчас: T, шаг: i32) -> T {
            let n = все.len() as i32;
            let i = все.iter().position(|x| *x == сейчас).unwrap_or(0) as i32;
            все[(((i + шаг) % n + n) % n) as usize]
        }
        let п = &mut self.п;
        match поле {
            Поле::Защита => п.защита = по_кругу(&Защита::ВСЕ, п.защита, шаг),
            Поле::Eap => п.eap = по_кругу(&Eap::ВСЕ, п.eap, шаг),
            Поле::Фаза2 => п.фаза2 = по_кругу(&Фаза2::ВСЕ, п.фаза2, шаг),
            Поле::Ipv4 => п.ipv4 = по_кругу(&Ip::V4, п.ipv4, шаг),
            Поле::Ipv6 => п.ipv6 = по_кругу(&Ip::V6, п.ipv6, шаг),
            Поле::Лимит => п.лимит = по_кругу(&Лимит::ВСЕ, п.лимит, шаг),
            Поле::Мак => п.мак = по_кругу(&Мак::ВСЕ, п.мак, шаг),
            Поле::Показать => self.показать = !self.показать,
            Поле::БезCa => п.без_ca = !п.без_ca,
            Поле::Авто => п.авто = !п.авто,
            Поле::Всем => п.всем = !п.всем,
            Поле::Скрытая => п.скрытая = !п.скрытая,
            _ => {}
        }
        self.ошибка = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn профиль_туда_и_обратно() {
        let п = Профиль {
            ssid: "Сеть".into(),
            защита: Защита::WpaEap,
            eap: Eap::Ttls,
            фаза2: Фаза2::Pap,
            identity: "ivan@uni".into(),
            анонимная: "anon@uni".into(),
            eap_пароль: "секрет".into(),
            домен: "radius.uni".into(),
            ipv4: Ip::Вручную,
            ipv4_адрес: "192.168.7.20/24".into(),
            ipv4_шлюз: "192.168.7.1".into(),
            ipv4_dns: "1.1.1.1, 9.9.9.9".into(),
            ipv6_dns: "2606:4700:4700::1111".into(),
            авто: false,
            всем: false,
            лимит: Лимит::Да,
            мак: Мак::Стабильный,
            скрытая: true,
            ..Default::default()
        };
        let н = в_nm(&п, Настройки::new(), "yarik");
        let назад = из_nm(&н, None);
        assert_eq!(назад.ssid, п.ssid);
        assert_eq!(назад.защита, п.защита);
        assert_eq!(назад.eap, п.eap);
        assert_eq!(назад.фаза2, п.фаза2);
        assert_eq!(назад.identity, п.identity);
        assert_eq!(назад.анонимная, п.анонимная);
        assert_eq!(назад.eap_пароль, п.eap_пароль);
        assert_eq!(назад.домен, п.домен);
        assert_eq!(назад.ipv4, Ip::Вручную);
        assert_eq!(назад.ipv4_адрес, "192.168.7.20/24");
        assert_eq!(назад.ipv4_шлюз, "192.168.7.1");
        assert_eq!(назад.ipv4_dns, "1.1.1.1, 9.9.9.9");
        assert_eq!(назад.ipv6_dns, "2606:4700:4700::1111");
        assert!(!назад.авто && !назад.всем && назад.скрытая);
        assert_eq!(назад.лимит, Лимит::Да);
        assert_eq!(назад.мак, Мак::Стабильный);
        assert!(проверить(&п).is_ok());
    }

    #[test]
    fn смена_защиты_сносит_старые_ключи() {
        let п = Профиль { ssid: "x".into(), пароль: "12345678".into(), ..Default::default() };
        let н = в_nm(&п, Настройки::new(), "y");
        let п2 = Профиль { защита: Защита::Нет, ..п };
        let н2 = в_nm(&п2, н, "y");
        assert!(!н2.contains_key("802-11-wireless-security"));
        assert!(!н2["802-11-wireless"].contains_key("security"));
    }

    #[test]
    fn форма_показывает_нужные_поля() {
        let mut ф = Форма::новая(Профиль { защита: Защита::WpaEap, eap: Eap::Tls, ..Default::default() }, "/".into(), true);
        let поля = ф.порядок();
        assert!(поля.contains(&Поле::КлиентСерт) && поля.contains(&Поле::Ключ));
        assert!(!поля.contains(&Поле::EapПароль), "у TLS пароля нет");
        ф.переключить(Поле::Eap, -1); // TLS → TTLS
        assert!(ф.порядок().contains(&Поле::Фаза2));
        assert!(!ф.порядок().contains(&Поле::Ipv4Адрес));
        ф.переключить(Поле::Ipv4, 1);
        assert!(ф.порядок().contains(&Поле::Ipv4Адрес));
    }

    #[test]
    fn проверка_ловит_ошибки() {
        let п = Профиль { ssid: "x".into(), пароль: "123".into(), ..Default::default() };
        assert!(проверить(&п).is_err());
        let п = Профиль { ssid: "x".into(), пароль: "12345678".into(), ipv4: Ip::Вручную, ipv4_адрес: "300.1.1.1".into(), ..Default::default() };
        assert!(проверить(&п).is_err());
        assert_eq!(канал(2437), Some(6));
        assert_eq!(канал(5180), Some(36));
    }
}
