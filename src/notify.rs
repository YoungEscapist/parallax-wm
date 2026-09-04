//! Звук уведомления: короткий тон на каждое всплывающее сообщение.
//!
//! **Почему это в композиторе, а не в mako.** Демон уведомлений умеет звать
//! команду на уведомление (`on-notify=exec …`), но тогда звук есть только у
//! того, кто так настроил СВОЙ демон: сменил mako на dunst — тишина. parallax
//! же знает про уведомления и без демона: они идут по сессионной шине
//! методом `org.freedesktop.Notifications.Notify`, и шину видно целиком.
//!
//! **Как слушаем.** Своим соединением с шиной, переведённым в режим монитора
//! (`org.freedesktop.DBus.Monitoring.BecomeMonitor`) с одним правилом отбора —
//! вызовы `Notify`. Монитор ВИДИТ чужие сообщения, но сам ничего послать уже
//! не может, поэтому соединение отдельное и живёт в своём потоке: остальные
//! наши разговоры с шиной (портал, трей, синезуб) через него не пройдут.
//!
//! Тем же приёмом звук получается независимым от демона вовсе: он звучит,
//! даже если демон ещё не поднялся, — сообщение по шине всё равно прошло.
//!
//! **Проигрывание** — отдельным процессом (`pw-play`, при его отсутствии
//! `paplay`), как и весь остальной звук в parallax (см. audio.rs): своего
//! микшера у композитора нет, а тянуть декодер ogg и клиент PipeWire в
//! зависимости ради одного тона незачем.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use zbus::zvariant::OwnedValue;

/// Встроенный тон: `assets/sounds/notify-glass.ogg` (CC0, см. README рядом).
///
/// Вшит, а не прочитан с диска, по той же причине, что и шрифт: переносимая
/// сборка (`build_portable.sh`) — это один файл, у которого нет ни каталога
/// установки, ни ресурсов рядом.
const ВСТРОЕННЫЙ: &[u8] = include_bytes!("../assets/sounds/notify-glass.ogg");

/// Пачка уведомлений (пять сообщений подряд от одного приложения) не должна
/// превращаться в пулемётную очередь: тон длится 0.87 с, и чаще чем раз в
/// треть секунды он всё равно сливается в кашу.
const ПАУЗА: Duration = Duration::from_millis(350);

/// Настройки, с которыми поднимается поток.
#[derive(Clone, Debug)]
pub struct Настройки {
    /// Путь к звуковому файлу; пусто — встроенный тон.
    pub файл: String,
    /// 0.0…1.0.
    pub громкость: f32,
}

/// Поднять слежение за уведомлениями. Возвращает `false`, если ничего не
/// поднялось (звук выключен настройкой либо нет проигрывателя) — тогда о нём
/// больше никто не вспоминает.
///
/// Отказ в любой точке — молчаливый в смысле последствий: звука нет, всё
/// остальное работает как работало. Уведомление без звука — мелочь, а вот
/// композитор, не вставший из-за звука, — нет.
pub fn поднять(настройки: Настройки) -> bool {
    if настройки.файл.eq_ignore_ascii_case("off")
        || настройки.файл.eq_ignore_ascii_case("none")
        || настройки.файл == "-"
    {
        tracing::info!("plx/notify: the notification sound is off");
        return false;
    }
    let проигрыватель = match Проигрыватель::найти() {
        Some(p) => p,
        None => {
            tracing::warn!("plx/notify: neither pw-play nor paplay found — no sound");
            return false;
        }
    };
    let файл = match подготовить_файл(&настройки.файл) {
        Some(f) => f,
        None => return false,
    };
    let громкость = настройки.громкость.clamp(0.0, 1.0);
    std::thread::Builder::new()
        .name("plx-notify".into())
        .spawn(move || поток(проигрыватель, файл, громкость))
        .is_ok()
}

/// Где лежит звук: заданный настройкой файл либо вшитый, выложенный в
/// XDG_RUNTIME_DIR (он свой у каждого сеанса и чистится системой).
fn подготовить_файл(заданный: &str) -> Option<PathBuf> {
    if !заданный.is_empty() {
        let путь = PathBuf::from(shellexpand(заданный));
        if путь.is_file() {
            return Some(путь);
        }
        tracing::warn!(
            "plx/notify: notify_sound = {:?} — no such file, falling back to the built-in tone",
            заданный,
        );
    }
    let каталог = std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir());
    let путь = каталог.join("plx-notify.ogg");
    // Перезаписываем каждый запуск: файл маленький, а несовпадение с бинарём
    // (обновили parallax, тон в рантайме остался старый) искалось бы долго.
    match std::fs::File::create(&путь).and_then(|mut f| f.write_all(ВСТРОЕННЫЙ)) {
        Ok(()) => Some(путь),
        Err(e) => {
            tracing::warn!("plx/notify: could not write {:?}: {}", путь, e);
            None
        }
    }
}

/// `~` в начале пути — единственное, что раскрываем: config.lua пишут руками.
fn shellexpand(путь: &str) -> String {
    match путь.strip_prefix("~/") {
        Some(хвост) => match std::env::var("HOME") {
            Ok(дом) => format!("{}/{}", дом, хвост),
            Err(_) => путь.to_string(),
        },
        None => путь.to_string(),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Проигрыватель {
    PwPlay,
    PaPlay,
}

impl Проигрыватель {
    /// Имя программы — оно же то, что человек наберёт в терминале, проверяя
    /// звук руками (см. предупреждение о молчащем проигрывателе).
    fn имя(self) -> &'static str {
        match self {
            Self::PwPlay => "pw-play",
            Self::PaPlay => "paplay",
        }
    }

    fn найти() -> Option<Self> {
        // pw-play первым: PipeWire в сессии и так поднят (см. launch_native.sh),
        // а paplay идёт через слой совместимости с pulse.
        for п in [Self::PwPlay, Self::PaPlay] {
            if есть_программа(п.имя()) {
                return Some(п);
            }
        }
        None
    }

    fn команда(self, файл: &Path, громкость: f32) -> Command {
        let mut cmd = match self {
            Self::PwPlay => {
                let mut c = Command::new(self.имя());
                c.arg(format!("--volume={громкость:.3}"));
                c
            }
            Self::PaPlay => {
                let mut c = Command::new(self.имя());
                // paplay меряет громкость в единицах pulse: 65536 = 100%.
                c.arg(format!("--volume={}", (громкость * 65536.0) as u32));
                c
            }
        };
        cmd.arg(файл);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        cmd
    }
}

fn есть_программа(имя: &str) -> bool {
    let Ok(пути) = std::env::var("PATH") else {
        return false;
    };
    пути.split(':').any(|d| Path::new(d).join(имя).is_file())
}

/// Тело потока: соединение с шиной, режим монитора, вечное чтение.
fn поток(проигрыватель: Проигрыватель, файл: PathBuf, громкость: f32) {
    let conn = match zbus::blocking::Connection::session() {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("plx/notify: no session bus: {} — no sound", e);
            return;
        }
    };
    // Правило отбора ровно одно: сам вызов Notify. Без правила монитор получал
    // бы ВЕСЬ трафик шины (у живого сеанса это тысячи сообщений в минуту —
    // трей, синезуб, портал), и мы бы их все разбирали ради одного.
    let правило = match zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::MethodCall)
        .interface("org.freedesktop.Notifications")
        .and_then(|b| b.member("Notify"))
        .map(|b| b.build())
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("plx/notify: match rule: {}", e);
            return;
        }
    };
    let монитор = match zbus::blocking::fdo::MonitoringProxy::new(&conn) {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("plx/notify: monitoring proxy: {}", e);
            return;
        }
    };
    if let Err(e) = монитор.become_monitor(&[правило], 0) {
        tracing::warn!("plx/notify: BecomeMonitor: {} — no sound", e);
        return;
    }
    tracing::info!(
        "plx/notify: notification sound is on ({:?}, {:?}, volume {:.2})",
        проигрыватель, файл, громкость,
    );

    let mut последний = Instant::now() - ПАУЗА;
    let mut дети: Vec<Child> = Vec::new();
    // Про молчащий проигрыватель говорим ОДИН раз за сеанс: причина у него
    // всегда одна и та же (нет звукового сервера, нет устройства), а
    // уведомлений за день сотни — повторяй мы на каждое, лог стал бы
    // непригоден. Без этой строки беда невидима вовсе: stderr проигрывателя
    // погашен нарочно, и «звук включён» в логе стоит даже там, где не звучит.
    let mut жаловались = false;
    for сообщение in zbus::blocking::MessageIterator::from(conn) {
        let сообщение = match сообщение {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("plx/notify: the bus is gone: {}", e);
                break;
            }
        };
        // Отжившие проигрыватели убираем здесь: ждать их в этом же потоке
        // нельзя (пока играет тон, мы не читаем шину), а не ждать вовсе —
        // значит копить зомби на всю жизнь сеанса.
        дети.retain_mut(|c| match c.try_wait() {
            Ok(Some(код)) => {
                if !код.success() && !жаловались {
                    жаловались = true;
                    tracing::warn!(
                        "plx/notify: {} exited with {} — the tone is silent \
                         (no sound server? try `{} {}` by hand)",
                        проигрыватель.имя(), код, проигрыватель.имя(), файл.display(),
                    );
                }
                false
            }
            Ok(None) => true,
            Err(_) => false,
        });
        if !звучать(&сообщение) {
            continue;
        }
        if последний.elapsed() < ПАУЗА {
            continue;
        }
        последний = Instant::now();
        match проигрыватель.команда(&файл, громкость).spawn() {
            Ok(c) => {
                tracing::debug!("plx/notify: playing {:?}", файл);
                дети.push(c);
            }
            Err(e) => tracing::warn!("plx/notify: could not play: {}", e),
        }
    }
}

/// Стоит ли звучать на этом уведомлении.
///
/// Единственная причина промолчать — подсказка `suppress-sound`, которой
/// приложение прямо просит тишины (её ставят, например, плееры на смену
/// трека). Разбор тела необязателен: не разобралось — звучим, тишина по
/// молчаливой ошибке хуже лишнего тона.
fn звучать(сообщение: &zbus::Message) -> bool {
    type Тело = (String, u32, String, String, String, Vec<String>, HashMap<String, OwnedValue>, i32);
    let Ok(тело) = сообщение.body().deserialize::<Тело>() else {
        return true;
    };
    let (приложение, .., подсказки, _) = тело;
    let тихо = подсказки
        .get("suppress-sound")
        .and_then(|v| v.downcast_ref::<bool>().ok())
        .unwrap_or(false);
    if тихо {
        tracing::debug!("plx/notify: {} asked for silence (suppress-sound)", приложение);
    }
    !тихо
}
