//! Сокеты мультиюзера: приём подключений, чтение, очередь на отправку.
//!
//! **Всё неблокирующее и всё в calloop.** Композитор — однопоточный цикл, и
//! любая блокирующая запись в сокет гостя означает застывший экран У ХОЗЯИНА
//! машины. Поэтому: сокеты в неблокирующем режиме, недописанное копится в
//! очереди гостя, добивается на следующем тике.
//!
//! **Два дескриптора на гостя, и это осознанно.** Источник calloop владеет
//! своим дубликатом (`try_clone`) и читает; состояние держит второй и пишет.
//! Иначе пришлось бы доставать сокет из источника внутри рендера — а
//! источник в этот момент занят самим calloop.
//!
//! **Пишет отдельный поток на гостя.** Сокет отдан ему целиком, главный цикл
//! только КЛАДЁТ готовое сообщение в [`Очередь`] (мьютекс держится ровно на
//! время push, никакого ввода-вывода под ним). Так композитор не может
//! застрять на медленном канале в принципе, а сообщения не могут
//! перемешаться: пишущий один.
//!
//! **Кадры дропаются, соединение — нет.** Гость на медленном канале не имеет
//! права тормозить хоста: как только в очереди больше [`ПРЕДЕЛ_ОЧЕРЕДИ`]
//! сообщений, новые ВИДЕОкадры для него просто не ставятся (флаг
//! `можно_ронять`). Служебные сообщения (участники, камера хоста, прощание)
//! не роняются никогда — они короткие, и именно по ним гость понимает, что
//! происходит.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

use smithay::reexports::calloop::{
    generic::Generic, Interest, Mode, PostAction, RegistrationToken,
};

use super::{proto, Гость};
use crate::Dawn;

/// Сколько СООБЩЕНИЙ разрешено держать неотправленными. Кадр 1080p в h264 —
/// десятки килобайт, 32 кадра это уже секунда отставания; дальше копить
/// бессмысленно — лучше показать свежий кадр с пропуском.
pub const ПРЕДЕЛ_ОЧЕРЕДИ: usize = 32;

/// Исходящая очередь гостя: главный цикл кладёт, пишущий поток забирает.
///
/// Своя, а не `mpsc`: нужно уметь СМОТРЕТЬ длину (чтобы ронять кадры) и
/// закрывать со стороны состояния, не имея приёмника.
pub struct Очередь {
    буфер: Mutex<VecDeque<Vec<u8>>>,
    есть: Condvar,
    закрыта: AtomicBool,
}

impl Очередь {
    pub fn new() -> Self {
        Self {
            буфер: Mutex::new(VecDeque::new()),
            есть: Condvar::new(),
            закрыта: AtomicBool::new(false),
        }
    }

    /// `false` — сообщение уронено (очередь переполнена и ронять разрешено)
    /// либо очередь уже закрыта.
    pub fn положить(&self, сообщение: Vec<u8>, можно_ронять: bool) -> bool {
        if self.закрыта.load(Ordering::Relaxed) {
            return false;
        }
        let mut б = match self.буфер.lock() {
            Ok(б) => б,
            Err(отравлен) => отравлен.into_inner(),
        };
        if можно_ронять && б.len() >= ПРЕДЕЛ_ОЧЕРЕДИ {
            return false;
        }
        б.push_back(сообщение);
        self.есть.notify_one();
        true
    }

    /// Забрать следующее, ожидая появления. `None` — очередь закрыта.
    ///
    /// `pub(crate)`, а не приватный: этой же очередью пишет режим Minecraft
    /// (см. `mine/net.rs`) — заводить ему вторую такую же было бы чистым
    /// дублированием самого тонкого места (условная переменная и закрытие).
    pub(crate) fn взять(&self) -> Option<Vec<u8>> {
        let mut б = match self.буфер.lock() {
            Ok(б) => б,
            Err(отравлен) => отравлен.into_inner(),
        };
        loop {
            if let Some(с) = б.pop_front() {
                return Some(с);
            }
            if self.закрыта.load(Ordering::Relaxed) {
                return None;
            }
            б = match self.есть.wait(б) {
                Ok(б) => б,
                Err(отравлен) => отравлен.into_inner(),
            };
        }
    }

    pub fn закрыть(&self) {
        self.закрыта.store(true, Ordering::Relaxed);
        self.есть.notify_all();
    }

    pub fn длина(&self) -> usize {
        self.буфер.lock().map(|б| б.len()).unwrap_or(0)
    }
}

impl Default for Очередь {
    fn default() -> Self {
        Self::new()
    }
}

/// Завести слушателя. Возвращает токен источника — по нему раздача снимет его
/// при остановке.
pub fn слушать(state: &mut Dawn, порт: u16) -> std::io::Result<RegistrationToken> {
    let слушатель = TcpListener::bind(("0.0.0.0", порт))?;
    слушатель.set_nonblocking(true)?;
    let токен = state
        .петля
        .insert_source(
            Generic::new(слушатель, Interest::READ, Mode::Level),
            |_, слушатель, state: &mut Dawn| {
                loop {
                    match слушатель.accept() {
                        Ok((поток, адрес)) => {
                            tracing::info!("dawn/share: стучится {адрес}");
                            принять(state, поток, адрес.ip());
                        }
                        Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                        Err(e) => {
                            tracing::warn!("dawn/share: accept: {e}");
                            break;
                        }
                    }
                }
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| std::io::Error::other(format!("calloop: {e}")))?;
    Ok(токен)
}

/// Новое соединение: заводим гостя, шлём вызов (соль), ставим источник чтения.
fn принять(state: &mut Dawn, поток: TcpStream, адрес: std::net::IpAddr) {
    let Some(раздача) = state.раздача.as_mut() else {
        // Раздачу выключили между accept и этим местом — вежливо закрываем.
        return;
    };
    // Бан проверяем ПЕРВЫМ делом — раньше числа мест, раньше соли, раньше
    // всего. Забаненный не должен получить от нас ни соли (её можно копить для
    // подбора), ни повода думать, что дело в занятых местах.
    if раздача.бан.contains(&адрес) {
        let mut поток = поток;
        let отказ = proto::ОтХоста::Отказ { причина: "доступ закрыт".into() };
        let _ = поток.write_all(&отказ.в_байты());
        tracing::info!("dawn/share: {адрес} в бане — отказ");
        return;
    }
    if раздача.гости.len() >= proto::МАКС_ГОСТЕЙ {
        let mut поток = поток;
        let отказ = proto::ОтХоста::Отказ { причина: "мест нет".into() };
        let _ = поток.write_all(&отказ.в_байты());
        tracing::warn!("dawn/share: отказ — уже {} гостей", раздача.гости.len());
        return;
    }
    if let Err(e) = поток.set_nodelay(true) {
        tracing::warn!("dawn/share: nodelay: {e}");
    }
    if let Err(e) = поток.set_nonblocking(true) {
        tracing::warn!("dawn/share: неблокирующий режим: {e}");
        return;
    }
    let читалка = match поток.try_clone() {
        Ok(п) => п,
        Err(e) => {
            tracing::warn!("dawn/share: try_clone: {e}");
            return;
        }
    };

    let писалка = match поток.try_clone() {
        Ok(п) => п,
        Err(e) => {
            tracing::warn!("dawn/share: try_clone (запись): {e}");
            return;
        }
    };

    let id = раздача.следующий_id;
    раздача.следующий_id = раздача.следующий_id.saturating_add(1);
    let соль = proto::случайные_байты::<16>();
    let исходящие = Arc::new(Очередь::new());
    let гость = Гость {
        id,
        имя: format!("гость {id}"),
        цвет: super::цвет(id),
        адрес,
        сокет: поток,
        токен: None,
        соль,
        впущен: false,
        камера: (0.0, 0.0, 1.0),
        кадр: (1280, 720),
        кадр_кодировщика: (0, 0),
        размер_сменён: Instant::now(),
        за_хостом: false,
        курсор: (0.0, 0.0),
        исходящие: исходящие.clone(),
        номер_кадра: 0,
        последний_кадр: Instant::now(),
        кодировщик: None,
        место: None,
        жив: true,
    };
    раздача.гости.push(гость);

    // Пишущий поток: сокет принадлежит ему одному. Блокирующая запись здесь
    // безопасна — это не поток композитора.
    {
        let исходящие = исходящие.clone();
        let mut сокет = писалка;
        // Блокирующий режим ИМЕННО у этого дескриптора: неблокирующий флаг
        // общий на описание файла, поэтому клонировали ДО set_nonblocking...
        // нет, флаг общий и после клона — поэтому пишем с учётом WouldBlock.
        let _ = сокет.set_nodelay(true);
        std::thread::Builder::new()
            .name(format!("dshare-{id}"))
            .spawn(move || {
                while let Some(сообщение) = исходящие.взять() {
                    let mut ушло = 0;
                    while ушло < сообщение.len() {
                        match сокет.write(&сообщение[ушло..]) {
                            Ok(0) => return,
                            Ok(n) => ушло += n,
                            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                                // Сокет неблокирующий (флаг общий с читающим
                                // дескриптором), поэтому ждём вручную —
                                // короткий сон вместо busy-loop.
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            Err(e) if e.kind() == ErrorKind::Interrupted => {}
                            Err(e) => {
                                tracing::warn!("dawn/share: гость {id}: запись: {e}");
                                return;
                            }
                        }
                    }
                }
            })
            .map_err(|e| tracing::warn!("dawn/share: поток записи не завёлся: {e}"))
            .ok();
    }

    // Вызов — первое, что видит гость.
    исходящие.положить(proto::ОтХоста::Вызов { соль }.в_байты(), false);

    let mut вход = proto::Поток::new();
    let mut буфер = vec![0u8; 64 * 1024];
    let токен = state.петля.insert_source(
        Generic::new(читалка, Interest::READ, Mode::Level),
        move |_, сокет, state: &mut Dawn| {
            // Читаем через `&TcpStream` (у него свой `impl Read`), а не через
            // `NoIoDrop::get_mut`: тот unsafe, и не из-за чтения — calloop так
            // запрещает УРОНИТЬ сокет из колбэка (иначе fd закроется у него под
            // ногами). Общая ссылка того же не позволяет, и unsafe не нужен.
            let mut чтение: &std::net::TcpStream = сокет;
            loop {
                match чтение.read(&mut буфер) {
                    Ok(0) => {
                        tracing::info!("dawn/share: гость {id} отключился");
                        пометить_мёртвым(state, id);
                        return Ok(PostAction::Remove);
                    }
                    Ok(n) => вход.дописать(&буфер[..n]),
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(e) => {
                        tracing::warn!("dawn/share: гость {id}: чтение: {e}");
                        пометить_мёртвым(state, id);
                        return Ok(PostAction::Remove);
                    }
                }
            }
            loop {
                match вход.следующее() {
                    Ok(Some(тело)) => {
                        let Some(сообщение) = proto::ОтГостя::из_байт(&тело) else {
                            tracing::warn!("dawn/share: гость {id}: не разобрал сообщение — рвём");
                            пометить_мёртвым(state, id);
                            return Ok(PostAction::Remove);
                        };
                        if !state.раздача_сообщение(id, сообщение) {
                            пометить_мёртвым(state, id);
                            return Ok(PostAction::Remove);
                        }
                    }
                    Ok(None) => break,
                    Err(()) => {
                        tracing::warn!("dawn/share: гость {id}: длина кадра вне предела — рвём");
                        пометить_мёртвым(state, id);
                        return Ok(PostAction::Remove);
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    );
    match токен {
        Ok(т) => {
            if let Some(г) = state.раздача.as_mut().and_then(|р| р.гость(id)) {
                г.токен = Some(т);
            }
        }
        Err(e) => {
            tracing::warn!("dawn/share: источник гостя {id} не завёлся: {e}");
            пометить_мёртвым(state, id);
        }
    }
}

/// Пометить гостя на удаление. Сам список правится в `убрать_мёртвых` из
/// тика — рвать связи посреди разбора сообщения нельзя, на стеке уже лежит
/// ссылка на этого же гостя.
pub fn пометить_мёртвым(state: &mut Dawn, id: u8) {
    if let Some(г) = state.раздача.as_mut().and_then(|р| р.гость(id)) {
        г.жив = false;
    }
}

/// Выкинуть помеченных: снять источник, закрыть сокет, обновить список.
pub fn убрать_мёртвых(state: &mut Dawn) {
    let Some(раздача) = state.раздача.as_mut() else { return };
    if раздача.гости.iter().all(|г| г.жив) {
        return;
    }
    let (мёртвые, живые): (Vec<Гость>, Vec<Гость>) =
        std::mem::take(&mut раздача.гости).into_iter().partition(|г| !г.жив);
    раздача.гости = живые;
    for mut г in мёртвые {
        // Источник чтения снимает себя сам (`PostAction::Remove`), но если
        // гостя убили извне (раздача кончилась, кик), токен ещё жив.
        if let Some(т) = г.токен {
            state.петля.remove(т);
        }
        // Пишущий поток висит на условной переменной очереди — без закрытия он
        // остался бы жить и после того, как гость ушёл.
        г.исходящие.закрыть();
        if let Some(место) = г.место.take() {
            super::seat::убрать(state, место);
        }
        tracing::info!("dawn/share: гость {} («{}») ушёл", г.id, г.имя);
    }
    state.раздача_разослать_участников();
    state.request_redraw();
}

/// Поставить сообщение в очередь гостя.
///
/// `можно_ронять` — про видеокадры: если гость не успевает, свежий кадр
/// важнее полного потока. Служебные сообщения ставятся всегда.
pub fn поставить_в_очередь(state: &mut Dawn, id: u8, данные: &[u8], можно_ронять: bool) {
    let Some(гость) = state.раздача.as_mut().and_then(|р| р.гость(id)) else { return };
    гость.исходящие.положить(данные.to_vec(), можно_ронять);
}
