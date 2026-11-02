//! Сторож Xwayland: зависший X-сервер не должен останавливать весь композитор.
//!
//! **Что было (замер 02.11.2026, дважды за вечер).** Под ULTRAKILL (Proton,
//! DXVK, X11-окно; Xwayland рисует через zink) nouveau ловит mmu fault и
//! убивает GPU-канал самого Xwayland (`Xwayland[PID]: channel 7 killed!`).
//! Поток zink в Xwayland (`zfq0`) после этого вечно ждёт в
//! `drm_syncobj_array_wait_timeout`, главный поток сервера — на futex, и X
//! перестаёт читать клиентов. А parallax — тоже его клиент: XWM (smithay
//! `X11Wm`, x11rb `RustConnection`) каждую итерацию досылает окнам позиции
//! (`sync_x11_geometry` → `X11Surface::configure`). Сокет неблокирующий, и
//! когда буфер заполнился (Send-Q ~53 КБ), x11rb ждёт возможности писать в
//! `poll(1 fd, без таймаута)` — прямо на главном потоке. Встаёт всё: кадры,
//! ввод, ctl, снимки экрана. Звук идёт (он в своих процессах).
//!
//! **Что делает сторож.** Главный цикл на каждой итерации бьёт `пульс()`.
//! Отдельный поток раз в [`ШАГ`] смотрит на счётчик; если тот стоит дольше
//! [`ЗАВИС`], а главный поток всё это время ([`НА_X_ПОДРЯД`] замеров подряд)
//! заблокирован в системном вызове на одном из сокетов X-сервера (poll/ppoll —
//! по самим `struct pollfd`, read/write/sendmsg — по первому аргументу; всё это
//! видно в `/proc/self/task/<tid>/syscall`), то зависание именно то самое.
//! Тогда он убивает Xwayland (SIGKILL через pidfd — чужой процесс с тем же pid
//! не заденет) и делает `shutdown()` своих концов X-сокетов: даже если сервер
//! застрял в ядре в непрерываемом ожидании и SIGKILL до него ещё не дошёл,
//! poll главного потока проснётся сразу, x11rb вернёт ошибку, XWM отвалится, и
//! `xwayland.rs` поднимет сервер заново.
//!
//! **Почему не /dev/kmsg.** Строка nouveau `Xwayland[PID]: channel N killed`
//! дала бы снять сервер ещё до заморозки, но у NixithOS
//! `kernel.dmesg_restrict = 1`: процессу сеанса без CAP_SYSLOG журнал ядра не
//! прочесть. А сторож по главному потоку ловит любую причину, по которой X
//! перестал читать (GPU, SIGSTOP, взаимная блокировка Xwayland↔композитор), и
//! только тогда, когда из-за неё стоит сам композитор, — ложных срабатываний
//! на просто медленный X у него нет.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Как часто поток сторожа смотрит на главный цикл.
const ШАГ: Duration = Duration::from_millis(250);
/// Сколько главный цикл должен стоять, чтобы сторож вмешался. Зависание,
/// ради которого всё это написано, вечное — запас здесь стоит только секунд
/// заморозки, а ложное срабатывание убило бы все X11-программы (и игру).
const ЗАВИС: Duration = Duration::from_secs(5);
/// Сколько замеров подряд главный поток обязан провести именно на X-сокете
/// (3 с при шаге 250 мс): долгая работа самого композитора, на миг заглянувшая
/// в X, сервер не убьёт.
const НА_X_ПОДРЯД: u32 = 12;

struct Общее {
    пульс: AtomicU64,
    /// Наши концы соединений с X-сервером: XWM и связь значков (icons.rs) —
    /// с подписью для лога.
    сокеты: Mutex<Vec<(RawFd, &'static str)>>,
    /// Нынешний процесс Xwayland.
    процесс: Mutex<Option<Процесс>>,
    /// Сколько раз сторож снимал зависший сервер за жизнь композитора.
    снятий: AtomicU64,
}

struct Процесс {
    pid: i32,
    /// pidfd: сигнал по нему уходит ровно этому процессу, даже если smithay уже
    /// подобрал его (`wait`) и pid достался кому-то ещё.
    pidfd: Option<OwnedFd>,
}

static ОБЩЕЕ: OnceLock<Общее> = OnceLock::new();

/// Поднять сторожа. Звать с главного потока — его tid и будет «главным».
pub fn поднять() {
    if ОБЩЕЕ.get().is_some() {
        return;
    }
    // tid главного потока — того, где крутится цикл событий.
    let главный = unsafe { libc::syscall(libc::SYS_gettid) as i32 };
    let _ = ОБЩЕЕ.set(Общее {
        пульс: AtomicU64::new(0),
        сокеты: Mutex::new(Vec::new()),
        процесс: Mutex::new(None),
        снятий: AtomicU64::new(0),
    });
    let запуск = std::thread::Builder::new()
        .name("plx-xwatch".into())
        .spawn(move || сторожить(главный));
    match запуск {
        Ok(_) => tracing::info!("plx/xwatch: watching the main loop (tid {главный}) for an X server hang"),
        Err(e) => tracing::warn!("plx/xwatch: could not start the watchdog thread: {e}"),
    }
}

/// Главный цикл жив. Звать раз за итерацию: это одна атомарная операция.
#[inline]
pub fn пульс() {
    if let Some(о) = ОБЩЕЕ.get() {
        о.пульс.fetch_add(1, Ordering::Relaxed);
    }
}

/// Запомнить наш конец соединения с X-сервером (`чей` — для лога).
pub fn сокет_x(fd: RawFd, чей: &'static str) {
    if let Some(о) = ОБЩЕЕ.get() {
        let mut с = о.сокеты.lock().unwrap();
        с.retain(|(f, _)| *f != fd);
        с.push((fd, чей));
        tracing::info!("plx/xwatch: X server socket fd {fd} ({чей})");
    }
}

/// Сервер ушёл: его сокеты больше не наши (номера дескрипторов скоро займут).
pub fn забыть_сокеты() {
    if let Some(о) = ОБЩЕЕ.get() {
        о.сокеты.lock().unwrap().clear();
    }
}

/// Новый процесс Xwayland (или `None` — нет никакого). Возвращает копию
/// pidfd — по ней цикл событий узнаёт о смерти процесса (см. xwayland.rs).
pub fn процесс_xwayland(pid: Option<i32>) -> Option<OwnedFd> {
    let о = ОБЩЕЕ.get()?;
    let новый = pid.map(|pid| Процесс { pid, pidfd: pidfd_open(pid) });
    let копия = новый
        .as_ref()
        .and_then(|п| п.pidfd.as_ref())
        .and_then(|fd| fd.try_clone().ok());
    *о.процесс.lock().unwrap() = новый;
    копия
}

/// pid нынешнего Xwayland, если он есть.
pub fn pid() -> Option<i32> {
    ОБЩЕЕ.get()?.процесс.lock().unwrap().as_ref().map(|п| п.pid)
}

/// Сколько раз сторож снимал зависший сервер.
pub fn снятий() -> u64 {
    ОБЩЕЕ.get().map_or(0, |о| о.снятий.load(Ordering::Relaxed))
}

/// Убить нынешний Xwayland и оборвать свои концы X-сокетов.
///
/// Обрыв — не довесок: SIGKILL до процесса в непрерываемом ожидании (а GPU-
/// зависание в ядре ровно так и выглядит) доходит, только когда ожидание
/// кончится, а `shutdown()` будит наш poll немедленно. Дескрипторы при этом
/// не закрываются — их по-прежнему держат x11rb и smithay, номер не
/// освободится и не уйдёт чужому, пока они сами не закроют его.
pub fn снять(причина: &str) -> bool {
    let Some(о) = ОБЩЕЕ.get() else { return false };
    let mut сделано = false;
    if let Some(п) = о.процесс.lock().unwrap().as_ref() {
        let r = match &п.pidfd {
            Some(fd) => unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    fd.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                ) as i32
            },
            None => unsafe { libc::kill(п.pid, libc::SIGKILL) },
        };
        if r == 0 {
            tracing::warn!("plx/xwatch: SIGKILL → Xwayland {} ({причина})", п.pid);
            сделано = true;
        } else {
            let e = std::io::Error::last_os_error();
            tracing::info!("plx/xwatch: Xwayland {} is already gone ({e})", п.pid);
        }
    }
    for &(fd, _) in о.сокеты.lock().unwrap().iter() {
        if unsafe { libc::shutdown(fd, libc::SHUT_RDWR) } == 0 {
            сделано = true;
        }
    }
    сделано
}

fn pidfd_open(pid: i32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if fd < 0 {
        tracing::warn!(
            "plx/xwatch: pidfd_open({pid}): {} — falling back to a plain pid",
            std::io::Error::last_os_error()
        );
        return None;
    }
    // pidfd_open ставит O_CLOEXEC сам.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn сторожить(главный: i32) {
    let Some(о) = ОБЩЕЕ.get() else { return };
    // Память своего же процесса: по ней читаются `struct pollfd`, на которых
    // стоит главный поток. Через файл, а не по указателю — указатель из чужого
    // стека в момент чтения может уже ничего не значить, а pread просто вернёт
    // ошибку.
    let память = std::fs::File::open("/proc/self/mem").ok();
    let путь = format!("/proc/self/task/{главный}/syscall");
    let mut прошлый = о.пульс.load(Ordering::Relaxed);
    let mut стоит_с = Instant::now();
    let mut на_x = 0u32;
    loop {
        std::thread::sleep(ШАГ);
        let п = о.пульс.load(Ordering::Relaxed);
        if п != прошлый {
            прошлый = п;
            стоит_с = Instant::now();
            на_x = 0;
            continue;
        }
        let сокеты = о.сокеты.lock().unwrap().clone();
        if сокеты.is_empty() {
            на_x = 0;
            continue;
        }
        let номера: Vec<RawFd> = сокеты.iter().map(|(f, _)| *f).collect();
        let Some((вызов, fd)) = ждёт_на(&путь, память.as_ref(), &номера) else {
            на_x = 0;
            continue;
        };
        на_x += 1;
        let стоит = стоит_с.elapsed();
        if на_x < НА_X_ПОДРЯД || стоит < ЗАВИС {
            continue;
        }
        // Последняя проверка прямо перед ударом: цикл мог как раз сдвинуться.
        if о.пульс.load(Ordering::Relaxed) != прошлый {
            continue;
        }
        let чей = сокеты.iter().find(|(f, _)| *f == fd).map_or("?", |(_, ч)| *ч);
        tracing::error!(
            "plx/xwatch: the main loop has stood still for {:.1} s inside syscall {вызов} on the X \
             server socket (fd {fd}, {чей}) — Xwayland is not answering; killing it. Xwayland \
             threads: {}",
            стоит.as_secs_f32(),
            потоки_xwayland()
        );
        о.снятий.fetch_add(1, Ordering::Relaxed);
        снять("hang");
        // Следующая попытка — не раньше, чем через полный срок: после обрыва
        // цикл должен пойти сам.
        на_x = 0;
        стоит_с = Instant::now();
    }
}

/// Где стоят потоки Xwayland — для лога: `имя:wchan:вызов` через пробел. По
/// этому видно, почему сервер не отвечает (GPU — `drm_syncobj…`, свой wayland-
/// roundtrip к нам — poll на сокете композитора, SIGSTOP — `do_signal_stop`).
fn потоки_xwayland() -> String {
    let Some(pid) = pid() else { return "unknown pid".into() };
    let Ok(задачи) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return format!("{pid} is gone");
    };
    let mut все: Vec<String> = задачи
        .flatten()
        .map(|з| {
            let п = з.path();
            let прочесть = |ф: &str| {
                std::fs::read_to_string(п.join(ф)).unwrap_or_default().trim().to_string()
            };
            let вызов = прочесть("syscall");
            let номер = вызов.split_whitespace().next().unwrap_or("?").to_string();
            format!("{}:{}:{}", прочесть("comm"), прочесть("wchan"), номер)
        })
        .collect();
    все.sort();
    format!("{pid} [{}]", все.join(" "))
}

/// На каком из `сокеты` главный поток стоит прямо сейчас: (номер вызова, fd).
fn ждёт_на(путь: &str, память: Option<&std::fs::File>, сокеты: &[RawFd]) -> Option<(i64, RawFd)> {
    // Формат: `<номер> <арг0> … <арг5> <sp> <pc>`, аргументы в hex; `running`,
    // если поток сейчас не в системном вызове.
    let строка = std::fs::read_to_string(путь).ok()?;
    let mut поля = строка.split_whitespace();
    let вызов: i64 = поля.next()?.parse().ok()?;
    let арг: Vec<u64> = поля
        .take(2)
        .map(|a| u64::from_str_radix(a.trim_start_matches("0x"), 16))
        .collect::<Result<_, _>>()
        .ok()?;
    if арг.len() < 2 {
        return None;
    }
    match вид_вызова(вызов)? {
        Вид::Poll => {
            let память = память?;
            let n = (арг[1] as usize).min(16);
            let mut буфер = vec![0u8; n * 8];
            память.read_exact_at(&mut буфер, арг[0]).ok()?;
            буфер
                .chunks_exact(8)
                .map(|p| i32::from_ne_bytes([p[0], p[1], p[2], p[3]]))
                .find(|fd| сокеты.contains(fd))
                .map(|fd| (вызов, fd))
        }
        Вид::Дескриптор => {
            let fd = арг[0] as i32;
            сокеты.contains(&fd).then_some((вызов, fd))
        }
    }
}

enum Вид {
    /// poll/ppoll: первый аргумент — массив `struct pollfd`, второй — длина.
    Poll,
    /// read/write/sendmsg…: первый аргумент — сам дескриптор.
    Дескриптор,
}

fn вид_вызова(вызов: i64) -> Option<Вид> {
    #[cfg(target_arch = "x86_64")]
    if вызов == libc::SYS_poll {
        return Some(Вид::Poll);
    }
    if вызов == libc::SYS_ppoll {
        return Some(Вид::Poll);
    }
    [
        libc::SYS_read,
        libc::SYS_write,
        libc::SYS_writev,
        libc::SYS_readv,
        libc::SYS_sendmsg,
        libc::SYS_recvmsg,
        libc::SYS_sendto,
        libc::SYS_recvfrom,
    ]
    .contains(&вызов)
    .then_some(Вид::Дескриптор)
}

/// Дочерний Xwayland нашего процесса для дисплея `:дисплей` — самый свежий.
///
/// smithay pid наружу не отдаёт (`Child` спрятан в данных клиента), поэтому
/// ищем по /proc: родитель — мы, argv[0] кончается на `Xwayland`, среди
/// аргументов `:N`. Звать сразу после `XWayland::spawn`: `Command::spawn`
/// возвращается только после exec, а подбирает процесс smithay лишь после
/// отключения его wayland-клиента — до этого pid не может достаться другому.
pub fn найти_xwayland(дисплей: u32) -> Option<i32> {
    let мы = std::process::id() as i32;
    let метка = format!(":{дисплей}");
    let mut лучший: Option<(u64, i32)> = None;
    for запись in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = запись.file_name().to_str().and_then(|s| s.parse::<i32>().ok()) else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // comm в скобках может содержать пробелы и скобки — режем по последней.
        let Some(хвост) = stat.rsplit_once(')').map(|(_, х)| х) else { continue };
        let поля: Vec<&str> = хвост.split_whitespace().collect();
        // поля[0] — state, [1] — ppid, [19] — starttime.
        if поля.get(1).and_then(|p| p.parse::<i32>().ok()) != Some(мы) {
            continue;
        }
        let старт: u64 = поля.get(19).and_then(|s| s.parse().ok()).unwrap_or(0);
        let Ok(cmd) = std::fs::read(format!("/proc/{pid}/cmdline")) else { continue };
        let mut арг = cmd.split(|b| *b == 0).filter(|a| !a.is_empty());
        let Some(имя) = арг.next() else { continue };
        if !имя.ends_with(b"Xwayland") {
            continue;
        }
        if !арг.any(|a| a == метка.as_bytes()) {
            continue;
        }
        if лучший.is_none_or(|(с, _)| старт >= с) {
            лучший = Some((старт, pid));
        }
    }
    лучший.map(|(_, pid)| pid)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Разбор /proc/…/syscall на настоящем блокирующем poll: поток ждёт на
    /// одном конце пары сокетов, сторож обязан увидеть именно этот fd.
    #[test]
    fn видит_poll_на_сокете() {
        use std::os::unix::net::UnixStream;
        let (a, _b) = UnixStream::pair().unwrap();
        let fd = a.as_raw_fd();
        let (tx, rx) = std::sync::mpsc::channel();
        let поток = std::thread::spawn(move || {
            tx.send(unsafe { libc::syscall(libc::SYS_gettid) as i32 }).unwrap();
            let mut p = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
            unsafe { libc::poll(&mut p, 1, 3000) };
        });
        let tid = rx.recv().unwrap();
        let путь = format!("/proc/self/task/{tid}/syscall");
        let память = std::fs::File::open("/proc/self/mem").ok();
        let mut нашли = None;
        for _ in 0..100 {
            std::thread::sleep(Duration::from_millis(10));
            нашли = ждёт_на(&путь, память.as_ref(), &[fd]);
            if нашли.is_some() {
                break;
            }
        }
        assert_eq!(нашли.map(|(_, f)| f), Some(fd));
        // Чужой дескриптор — не наш случай.
        assert!(ждёт_на(&путь, память.as_ref(), &[fd + 1000]).is_none());
        drop(_b);
        поток.join().unwrap();
    }
}
