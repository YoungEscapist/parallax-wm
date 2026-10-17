//! Сокет plx-mine: слушатель, один мод, очередь на отправку.
//!
//! **Один клиент, а не пять.** Minecraft с модом на этой машине один; второй
//! показывал бы те же самые окна теми же панелями и только делил бы полосу.
//! Поэтому новое соединение при живом моде получает отказ закрытием: так
//! перезапуск игры не оставляет за собой призрака, который продолжает тянуть
//! пиксели.
//!
//! **Пишет отдельный поток, как у мультиюзера** (см. `share/net.rs`, оттуда же
//! взята [`Очередь`] — заводить вторую такую же было бы чистым дублированием).
//! Причина та же и главная: композитор однопоточный, и блокирующая запись в
//! сокет мода означала бы застывший экран на мониторе. Полосы пикселей
//! роняются, когда мод не успевает; панели и прощание — никогда.
//!
//! **Сокет живёт в `$XDG_RUNTIME_DIR`,** а не в `/tmp`: это каталог сеанса,
//! права там уже правильные (0700 на пользователя), и он чистится сам при
//! выходе. Осиротевший файл (parallax упал, не убрав) снимаем перед bind —
//! проверив, что в него никто не слушает, чтобы не отобрать сокет у живого
//! композитора соседнего сеанса.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use smithay::reexports::calloop::{
    generic::Generic, Interest, Mode, PostAction, RegistrationToken,
};

use super::proto;
use crate::share::net::Очередь;
use crate::Parallax;

/// Где лежит сокет. `$XDG_RUNTIME_DIR/plx-mine.sock`, а без переменной —
/// `/run/user/<uid>`, который в Void и есть настоящий рантайм-каталог.
pub fn путь_сокета() -> PathBuf {
    let каталог = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    каталог.join(proto::СОКЕТ)
}

/// Живое соединение с модом.
pub struct Мод {
    /// Очередь исходящих: главный цикл кладёт, пишущий поток забирает.
    pub исходящие: Arc<Очередь>,
    /// Источник чтения — снять при отключении.
    pub токен: Option<RegistrationToken>,
    /// Рукопожатие прошло: до него не шлём ни панелей, ни пикселей.
    pub впущен: bool,
    /// Имя, которым представился мод (для `mine state`).
    pub имя: String,
    /// Процесс на том конце сокета — он же процесс игры (мод живёт в её JVM).
    /// По нему находится окно Minecraft, см. `mine::окно_игры`.
    pub pid: Option<u32>,
    /// Помечен мёртвым — уберём в тике, а не посреди разбора сообщения.
    pub жив: bool,
    /// Мод умеет принимать dmabuf (бит из его «привета»).
    pub dmabuf: bool,
    /// Дескрипторы к сообщениям `Буфер`, в том же порядке, что и сами
    /// сообщения в очереди. Пишущий поток, встретив `Буфер`, берёт отсюда
    /// первый и прикладывает его к сообщению (SCM_RIGHTS). Порядок держится
    /// потому, что `Буфер` никогда не роняется — см. [`послать_буфер`].
    pub дескрипторы: Arc<Mutex<VecDeque<OwnedFd>>>,
}

/// Завести слушателя. Токен возвращается наружу: при выходе из режима его
/// снимает `mine::выключить`.
pub fn слушать(state: &mut Parallax) -> std::io::Result<RegistrationToken> {
    let путь = путь_сокета();
    // Осиротевший файл: если в него никто не слушает, connect даст
    // ECONNREFUSED — значит хозяин мёртв и файл можно снять. Живой сокет
    // (успешный connect) не трогаем: это чужой сеанс, и отобрать его — значит
    // молча сломать чужой Minecraft.
    if путь.exists() {
        match UnixStream::connect(&путь) {
            Ok(_) => {
                return Err(std::io::Error::new(
                    ErrorKind::AddrInUse,
                    format!("{} is already being listened on by another parallax", путь.display()),
                ));
            }
            Err(_) => {
                let _ = std::fs::remove_file(&путь);
            }
        }
    }
    let слушатель = UnixListener::bind(&путь)?;
    слушатель.set_nonblocking(true)?;
    tracing::info!("plx/mine: listening on {}", путь.display());
    state
        .петля
        .insert_source(
            Generic::new(слушатель, Interest::READ, Mode::Level),
            |_, слушатель, state: &mut Parallax| {
                loop {
                    match слушатель.accept() {
                        Ok((поток, _)) => принять(state, поток),
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => {
                            tracing::warn!("plx/mine: accept: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| std::io::Error::other(format!("calloop: {e}")))
}

/// Процесс на том конце сокета (`SO_PEERCRED`).
///
/// Руками, а не `UnixStream::peer_cred`: тот до сих пор за `#![feature]`, а
/// собираемся мы стабильным компилятором. Ядро отдаёт `struct ucred` — три
/// 32-битных поля (pid, uid, gid), первое и нужно.
fn pid_соседа(поток: &UnixStream) -> Option<u32> {
    use std::os::fd::AsRawFd;
    let mut ucred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut длина = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let код = unsafe {
        libc::getsockopt(
            поток.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut ucred as *mut libc::ucred).cast(),
            &mut длина,
        )
    };
    if код != 0 {
        tracing::warn!("plx/mine: SO_PEERCRED: {}", std::io::Error::last_os_error());
        return None;
    }
    u32::try_from(ucred.pid).ok()
}

/// Новое соединение: заводим мод, шлём «здравствуй», ставим источник чтения.
fn принять(state: &mut Parallax, поток: UnixStream) {
    let Some(шахта) = state.mine.as_mut() else { return };
    if шахта.мод.is_some() {
        tracing::info!("plx/mine: a mod is already connected — second one refused");
        return;
    }
    if let Err(e) = поток.set_nonblocking(true) {
        tracing::warn!("plx/mine: non-blocking mode: {e}");
        return;
    }
    // Чей это процесс. Нужно не ради статистики: по этому pid находится ОКНО
    // самой игры, а его приходится вычитать из сцены — иначе Minecraft висит
    // панелью в самом себе (человек видит собственный экран «монитором»), а
    // ввод панелей утыкается в него же, потому что оно поверх всех.
    let pid = pid_соседа(&поток);
    tracing::info!("plx/mine: connection from pid {pid:?}");
    let (читалка, писалка) = match (поток.try_clone(), поток) {
        (Ok(ч), п) => (ч, п),
        (Err(e), _) => {
            tracing::warn!("plx/mine: try_clone: {e}");
            return;
        }
    };

    let исходящие = Arc::new(Очередь::new());
    let дескрипторы = Arc::new(Mutex::new(VecDeque::new()));
    шахта.мод = Some(Мод {
        исходящие: исходящие.clone(),
        токен: None,
        впущен: false,
        имя: String::new(),
        pid,
        жив: true,
        dmabuf: false,
        дескрипторы: дескрипторы.clone(),
    });

    // Пишущий поток: сокет принадлежит ему одному, блокировки здесь безопасны.
    {
        let исходящие = исходящие.clone();
        let mut сокет = писалка;
        std::thread::Builder::new()
            .name("plx-mine-write".into())
            .spawn(move || {
                while let Some(сообщение) = исходящие.взять() {
                    let mut ушло = 0;
                    // Сообщение с дескриптором: первый кусок уходит через
                    // sendmsg вместе с ним, остаток — обычной записью ниже.
                    // Ядро прикладывает SCM_RIGHTS к первому байту, и мод на
                    // том конце получает дескриптор вместе с началом `Буфер`.
                    if сообщение.get(4) == Some(&proto::тип::БУФЕР) {
                        let fd = match дескрипторы.lock() {
                            Ok(mut д) => д.pop_front(),
                            Err(отравлен) => отравлен.into_inner().pop_front(),
                        };
                        let Some(fd) = fd else {
                            tracing::warn!("plx/mine: buffer message without a descriptor — dropping the link");
                            return;
                        };
                        match послать_с_дескриптором(&сокет, &сообщение, &fd) {
                            Ok(n) => ушло = n,
                            Err(e) => {
                                tracing::warn!("plx/mine: sendmsg: {e}");
                                return;
                            }
                        }
                    }
                    while ушло < сообщение.len() {
                        match сокет.write(&сообщение[ушло..]) {
                            Ok(0) => return,
                            Ok(n) => ушло += n,
                            // Флаг неблокирующего режима общий с читающим
                            // дескриптором, поэтому ждём вручную коротким сном
                            // вместо busy-loop.
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => {}
                            Err(e) => {
                                tracing::warn!("plx/mine: write: {e}");
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|e| tracing::warn!("plx/mine: writer thread failed to start: {e}"))
            .ok();
    }

    let mut вход = proto::Поток::new();
    let mut буфер = vec![0u8; 64 * 1024];
    let токен = state.петля.insert_source(
        Generic::new(читалка, Interest::READ, Mode::Level),
        move |_, сокет, state: &mut Parallax| {
            // Читаем через общую ссылку (`&UnixStream: Read`), а не через
            // `NoIoDrop::get_mut`: тот unsafe, и запрещает он не чтение, а
            // роняние сокета из колбэка. Общая ссылка того же не позволяет.
            let mut чтение: &UnixStream = сокет;
            loop {
                match чтение.read(&mut буфер) {
                    Ok(0) => {
                        tracing::info!("plx/mine: mod disconnected");
                        пометить_мёртвым(state);
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => вход.дописать(&буфер[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::warn!("plx/mine: read: {e}");
                        пометить_мёртвым(state);
                        return Ok(PostAction::Remove);
                    }
                }
            }
            loop {
                match вход.следующее() {
                    Ok(Some(тело)) => {
                        let Some(сообщение) = proto::ОтМода::из_байт(&тело) else {
                            tracing::warn!("plx/mine: could not parse the message — dropping");
                            пометить_мёртвым(state);
                            return Ok(PostAction::Remove);
                        };
                        if !super::принять_сообщение(state, сообщение) {
                            пометить_мёртвым(state);
                            return Ok(PostAction::Remove);
                        }
                    }
                    Ok(None) => break,
                    Err(()) => {
                        tracing::warn!("plx/mine: message length out of range — dropping");
                        пометить_мёртвым(state);
                        return Ok(PostAction::Remove);
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    );
    match токен {
        Ok(т) => {
            if let Some(м) = state.mine.as_mut().and_then(|ш| ш.мод.as_mut()) {
                м.токен = Some(т);
            }
        }
        Err(e) => {
            tracing::warn!("plx/mine: mod event source failed to start: {e}");
            пометить_мёртвым(state);
        }
    }
}

/// Пометить мод на удаление. Сам он убирается в тике: рвать связи посреди
/// разбора сообщения нельзя — на стеке уже лежит ссылка на это же соединение.
pub fn пометить_мёртвым(state: &mut Parallax) {
    if let Some(м) = state.mine.as_mut().and_then(|ш| ш.мод.as_mut()) {
        м.жив = false;
    }
}

/// Выкинуть мод, если он помечен мёртвым.
pub fn убрать_мёртвого(state: &mut Parallax) {
    let помечен = state
        .mine
        .as_ref()
        .and_then(|ш| ш.мод.as_ref())
        .is_some_and(|м| !м.жив);
    if !помечен {
        return;
    }
    отключить(state);
    // Панели останутся висеть в игре мёртвыми текстурами, пока мод не
    // переподключится, — но parallax об этом уже ничего не знает и рисовать в
    // пустоту не должен.
    if let Some(ш) = state.mine.as_mut() {
        ш.посланные.clear();
    }
}

/// Оборвать соединение с модом (ушёл сам, или режим выключают).
pub fn отключить(state: &mut Parallax) {
    let Some(шахта) = state.mine.as_mut() else { return };
    let Some(мод) = шахта.мод.take() else { return };
    // Пишущий поток висит на условной переменной очереди — без закрытия он
    // остался бы жить и после ухода мода.
    мод.исходящие.закрыть();
    if let Some(т) = мод.токен {
        state.петля.remove(т);
    }
}

/// Отдать начало сообщения вместе с дескриптором. Сокет неблокирующий (флаг
/// общий с читающей половиной), поэтому EAGAIN ждём коротким сном, как и
/// обычная запись. Возвращает, сколько байт сообщения ушло.
fn послать_с_дескриптором(сокет: &UnixStream, сообщение: &[u8], fd: &OwnedFd) -> std::io::Result<usize> {
    let сырой = fd.as_raw_fd();
    loop {
        let mut iov = libc::iovec {
            iov_base: сообщение.as_ptr() as *mut libc::c_void,
            iov_len: сообщение.len(),
        };
        // Место под один int в управляющем сообщении, выровненное как велит
        // CMSG_SPACE. u64-массив — ради выравнивания cmsghdr.
        let mut упр = [0u64; 4];
        let мест = unsafe { libc::CMSG_SPACE(std::mem::size_of::<libc::c_int>() as u32) } as usize;
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = упр.as_mut_ptr().cast();
        msg.msg_controllen = мест as _;
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&msg);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<libc::c_int>() as u32) as _;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), сырой);
        }
        let n = unsafe { libc::sendmsg(сокет.as_raw_fd(), &msg, libc::MSG_NOSIGNAL) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let e = std::io::Error::last_os_error();
        match e.kind() {
            ErrorKind::WouldBlock => std::thread::sleep(std::time::Duration::from_millis(2)),
            ErrorKind::Interrupted => {}
            _ => return Err(e),
        }
    }
}

/// Отдать моду полотно-dmabuf: сообщение и дескриптор. Не роняется никогда —
/// иначе дескриптор приложился бы к чужому сообщению.
pub fn послать_буфер(state: &mut Parallax, сообщение: &proto::ОтДавна, fd: OwnedFd) {
    let Some(мод) = state.mine.as_mut().and_then(|ш| ш.мод.as_mut()) else { return };
    match мод.дескрипторы.lock() {
        Ok(mut д) => д.push_back(fd),
        Err(отравлен) => отравлен.into_inner().push_back(fd),
    }
    мод.исходящие.положить(сообщение.в_байты(), false);
}

/// Поставить сообщение в очередь мода.
///
/// `можно_ронять` — про полосы пикселей: если мод не успевает, свежая картинка
/// важнее полной. Панели, курсор и прощание ставятся всегда: по ним мод
/// понимает, что вообще происходит, и уронить их значит показать окно не там,
/// где оно есть.
pub fn послать(state: &mut Parallax, сообщение: &proto::ОтДавна, можно_ронять: bool) {
    let Some(мод) = state.mine.as_mut().and_then(|ш| ш.мод.as_mut()) else { return };
    мод.исходящие.положить(сообщение.в_байты(), можно_ронять);
}
