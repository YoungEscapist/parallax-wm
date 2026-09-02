//! Управляющий сокет: дёргать действия dawn и снимать кадры без клавиатуры.
//!
//! **Зачем.** Проверить правку рендера можно было только руками за машиной:
//! действия dawn достижимы исключительно с физической клавиатуры, а второй
//! экземпляр на свободном VT читает те же evdev-устройства, что и живой сеанс,
//! то есть ловит чужие нажатия (и умеет, например, увести VT). Сокет разрывает
//! эту связку: тот же `dispatch_action`, но по строке в UNIX-сокет.
//!
//! **Где включается.** Только по `DAWN_CTL=1` (и всегда — в `--headless`).
//! В живом сеансе по умолчанию выключен: это дыра ровно того размера, что и
//! права на сокет, — кто может писать в него, тот может выполнить любое
//! действие композитора.
//!
//! Формат — строки, ответ — строки, соединение одноразовое:
//! ```text
//! shot /tmp/кадр.png          — собрать кадр и записать PNG (только headless)
//! action spawn cmd="ghostty"  — любое действие из config.lua, аргументы = тело
//! action view_tag tag=2         таблицы Lua (то же, что в bind{})
//! vr [mode|on|off|ar|layout|recenter|status|panels|shot <путь>]
//!                              — шлем: `mode` это весь вход одной командой
//!                                (сервер WiVRn + ожидание Quest + вход),
//!                                остальное — сырое: VR, passthrough,
//!                                раскладка панелей, что и где висит;
//! windows                     — список окон: id, класс, стол, монитор, слот,
//!                               буфер, запрошено
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};

use smithay::reexports::calloop::{
    generic::Generic, Interest, LoopHandle, Mode, PostAction,
};

use crate::Dawn;

/// Путь сокета: `DAWN_CTL_SOCKET`, иначе `$XDG_RUNTIME_DIR/dawn-<сокет>.ctl`.
///
/// Имя вейландовского сокета в пути не случайно: на машине может идти живой
/// сеанс и харнесс разом, и попасть командой не в тот экземпляр — ровно та
/// ошибка, ради избежания которой всё это и написано.
pub fn путь_сокета(state: &Dawn) -> std::path::PathBuf {
    if let Some(явный) = std::env::var_os("DAWN_CTL_SOCKET") {
        return std::path::PathBuf::from(явный);
    }
    let каталог = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    каталог.join(format!("dawn-{}.ctl", state.socket_name.to_string_lossy()))
}

pub fn init(
    handle: &LoopHandle<'static, Dawn>,
    state: &Dawn,
) -> Result<(), Box<dyn std::error::Error>> {
    let путь = путь_сокета(state);
    // Остаток от прошлого запуска: bind по занятому пути падает с EADDRINUSE,
    // а живого слушателя там быть не может — мы его только что не запускали.
    let _ = std::fs::remove_file(&путь);
    let listener = UnixListener::bind(&путь)?;
    listener.set_nonblocking(true)?;
    tracing::info!("dawn/ctl: слушаю {:?}", путь);

    handle.insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        |_, listener, state: &mut Dawn| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => обслужить(stream, state),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!("dawn/ctl: accept: {}", e);
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    )?;
    Ok(())
}

fn обслужить(stream: UnixStream, state: &mut Dawn) {
    // Блокирующее чтение — сознательно: команда это одна короткая строка от
    // своего же харнесса, а неблокирующий разбор по кускам стоил бы буфера
    // состояния на соединение ради нулевой выгоды.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("dawn/ctl: clone: {}", e);
            return;
        }
    });
    let mut ответ = String::new();
    let mut строка = String::new();
    loop {
        строка.clear();
        match reader.read_line(&mut строка) {
            Ok(0) => break,
            Ok(_) => {
                let s = строка.trim();
                if s.is_empty() {
                    continue;
                }
                ответ.push_str(&выполнить(s, state));
                ответ.push('\n');
            }
            Err(_) => break,
        }
    }
    let mut stream = stream;
    let _ = stream.write_all(ответ.as_bytes());
    let _ = stream.flush();
}

fn выполнить(строка: &str, state: &mut Dawn) -> String {
    let (команда, хвост) = match строка.split_once(char::is_whitespace) {
        Some((имя, остаток)) => (имя, остаток.trim()),
        None => (строка, ""),
    };
    match команда {
        "shot" => {
            if хвост.is_empty() {
                return "ошибка: shot без пути".into();
            }
            state.shot_request = Some(std::path::PathBuf::from(хвост));
            state.request_redraw();
            format!("ok: снимок будет в {}", хвост)
        }
        "action" => {
            let (имя, аргументы) = match хвост.split_once(char::is_whitespace) {
                Some((имя, остаток)) => (имя, остаток.trim()),
                None => (хвост, ""),
            };
            match crate::config::action_from_str(имя, аргументы) {
                Some(действие) => {
                    tracing::info!("dawn/ctl: действие {} {{{}}}", имя, аргументы);
                    state.dispatch_action(действие);
                    state.request_redraw();
                    "ok".into()
                }
                None => format!("ошибка: не разобрал действие '{}' {{{}}}", имя, аргументы),
            }
        }
        "windows" => окна(state),
        "mouse" => мышь(хвост, state),
        "key" => клавиша(хвост, state),
        "pointer" => указатель(state),
        "share" => раздача(хвост, state),
        "portal" => портал(хвост, state),
        "vr" => шлем(хвост, state),
        "mine" => шахта(хвост, state),
        "help" => "shot <путь> | action <имя> [k=v ...] | windows | \
                   mouse to X Y | mouse move DX DY | mouse down|up|click [левая|правая|средняя] | \
                   mouse drag X1 Y1 X2 Y2 | mouse scroll N | \
                   key [logo+shift+]<имя> | key down|up <имя> | pointer | \
                   share [start [порт] | stop | status] | \
                   portal pick [типы] | portal cancel | \
                   vr [mode|on|off|ar|layout|status]".into(),
        иное => format!("ошибка: не знаю команду '{}'", иное),
    }
}

/// Демонстрация экрана из терминала: `portal pick [типы]`, `portal cancel`.
///
/// Только для харнесса. В headless портал на шину не выходит вовсе (см.
/// `main.rs`), и подсветку выбора источника — ту самую, что видит человек,
/// когда Discord или OBS просят экран, — иначе можно было посмотреть лишь на
/// живом сеансе. `типы` те же, что в спецификации: 1 — только мониторы (так
/// шлёт OBS), 2 — только окна, 3 — и то и другое.
fn портал(хвост: &str, state: &mut Dawn) -> String {
    let (что, довод) = match хвост.split_once(char::is_whitespace) {
        Some((первое, остаток)) => (первое, остаток.trim()),
        None => (хвост, ""),
    };
    match что {
        "" | "pick" | "выбор" => {
            let типы = довод.parse::<u32>().unwrap_or(3).clamp(1, 3);
            state.portal_pick_debug(типы);
            format!("ok: выбор источника (типы={типы})")
        }
        "cancel" | "отмена" => {
            if !state.portal_picking() {
                return "выбор и так не идёт".into();
            }
            state.portal_pick_click(true);
            "ok: выбор отменён".into()
        }
        иное => format!("ошибка: portal [pick [типы] | cancel], а не '{иное}'"),
    }
}

/// Мультиюзер из терминала: `share`, `share start [порт]`, `share stop`.
///
/// Отдельная команда, а не `action share_start`, ровно из-за ОТВЕТА. Действие
/// умеет сказать только «ok»: код доступа и адрес, которые человек диктует
/// гостю, остаются в логе композитора, куда из терминала не заглянешь.
/// Здесь всё нужное возвращается прямо в терминал одной строкой.
fn раздача(хвост: &str, state: &mut Dawn) -> String {
    let (что, довод) = match хвост.split_once(char::is_whitespace) {
        Some((первое, остаток)) => (первое, остаток.trim()),
        None => (хвост, ""),
    };
    match что {
        "" | "status" | "стат" => состояние_раздачи(state),
        "start" | "on" | "вкл" => {
            let порт = довод.parse::<u16>().unwrap_or(0);
            match state.раздача_начать(порт) {
                Ok(_) => состояние_раздачи(state),
                Err(причина) => format!("ошибка: {причина}"),
            }
        }
        "stop" | "off" | "выкл" => {
            if !state.раздача_идёт() {
                return "раздача и так выключена".into();
            }
            state.раздача_закончить();
            "ok: раздача выключена".into()
        }
        иное => format!("ошибка: share [start [порт] | stop | status], а не '{иное}'"),
    }
}

fn состояние_раздачи(state: &Dawn) -> String {
    let Some(раздача) = state.раздача.as_ref() else {
        return "раздача выключена".into();
    };
    let mut ответ = format!(
        "раздача идёт: код {}, порт {}, кадр {}×{}",
        раздача.код, раздача.порт, раздача.кадр.0, раздача.кадр.1,
    );
    for адрес in адреса_машины() {
        ответ.push_str(&format!("\n  гостю: dshare {}:{} {}", адрес, раздача.порт, раздача.код));
    }
    let гости: Vec<String> = раздача
        .гости
        .iter()
        .filter(|гость| гость.впущен)
        .map(|гость| {
            format!(
                "{} «{}»{}",
                гость.id,
                гость.имя,
                if гость.за_хостом { ", за хостом" } else { "" },
            )
        })
        .collect();
    ответ.push_str(&format!(
        "\n  гостей: {}{}",
        гости.len(),
        if гости.is_empty() { String::new() } else { format!(" — {}", гости.join(", ")) },
    ));
    ответ
}

/// Адреса этой машины, по которым до неё дотянется гость.
///
/// Читаем `getifaddrs`, а не зовём `ip`: команда есть не везде, а формат её
/// вывода — не договор. Отбрасываемloopback (по нему придёт только тот, кто уже
/// сидит за этой же машиной) и не-IP семейства.
fn адреса_машины() -> Vec<String> {
    let mut список = Vec::new();
    let mut головной: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs заполняет указатель списком, который мы освобождаем
    // freeifaddrs ниже; при отказе (не 0) список не выделен и трогать нечего.
    if unsafe { libc::getifaddrs(&mut головной) } != 0 {
        return список;
    }
    let mut узел = головной;
    while !узел.is_null() {
        // SAFETY: идём по списку, который только что отдал getifaddrs.
        let текущий = unsafe { &*узел };
        узел = текущий.ifa_next;
        if текущий.ifa_addr.is_null() {
            continue;
        }
        // SAFETY: ifa_addr не null, семейство читаем из общей части sockaddr.
        let семейство = unsafe { (*текущий.ifa_addr).sa_family } as i32;
        // 46 — INET6_ADDRSTRLEN; крейт libc эту константу не реэкспортирует, а
        // числом она задана самим RFC и не меняется.
        const ДЛИНА_АДРЕСА: usize = 46;
        let размер = match семейство {
            libc::AF_INET => std::mem::size_of::<libc::sockaddr_in>(),
            libc::AF_INET6 => std::mem::size_of::<libc::sockaddr_in6>(),
            _ => continue,
        };
        let длина = ДЛИНА_АДРЕСА;
        let mut текст = vec![0u8; длина];
        // SAFETY: буфер длиной с INET*_ADDRSTRLEN, размер sockaddr по семейству.
        let вышло = unsafe {
            libc::getnameinfo(
                текущий.ifa_addr,
                размер as libc::socklen_t,
                текст.as_mut_ptr() as *mut libc::c_char,
                длина as libc::socklen_t,
                std::ptr::null_mut(),
                0,
                libc::NI_NUMERICHOST,
            )
        };
        if вышло != 0 {
            continue;
        }
        let конец = текст.iter().position(|&b| b == 0).unwrap_or(текст.len());
        let адрес = String::from_utf8_lossy(&текст[..конец]).into_owned();
        // Локальная петля и link-local (fe80::…%iface) гостю бесполезны.
        if адрес == "127.0.0.1" || адрес == "::1" || адрес.starts_with("fe80") {
            continue;
        }
        if !список.contains(&адрес) {
            список.push(адрес);
        }
    }
    // SAFETY: освобождаем ровно тот список, что выдал getifaddrs.
    unsafe { libc::freeifaddrs(головной) };
    список
}

/// Синтетическая мышь. Все ветки идут через `process_input_event` — ту же
/// дверь, в которую входит настоящая libinput-мышь (см. synth.rs, там же
/// объяснение, почему не через `pointer.motion` напрямую).
///
/// Координаты — ЭКРАННЫЕ физические, как их видит глаз на снимке `shot`.
/// Перевод в холст делает сам dawn (`pointer_location` += дельта ⁄ зум),
/// поэтому «навести в точку» — это ровно одно событие с разностью координат.
fn мышь(хвост: &str, state: &mut Dawn) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    let число = |i: usize| слова.get(i).and_then(|s| s.parse::<f64>().ok());

    match слова.first().copied() {
        Some("to") => {
            let (Some(x), Some(y)) = (число(1), число(2)) else {
                return "ошибка: mouse to X Y".into();
            };
            навести(state, x, y);
            указатель(state)
        }
        Some("move") => {
            let (Some(dx), Some(dy)) = (число(1), число(2)) else {
                return "ошибка: mouse move DX DY".into();
            };
            state.process_input_event(двинуть(dx, dy));
            указатель(state)
        }
        Some(движение @ ("down" | "up" | "click")) => {
            let код = match слова.get(1) {
                None => crate::synth::КНОПКА_ЛКМ,
                Some(имя) => match crate::synth::кнопка_по_имени(имя) {
                    Some(к) => к,
                    None => return format!("ошибка: не знаю кнопку '{}'", имя),
                },
            };
            match движение {
                "down" => нажать(state, код, true),
                "up" => нажать(state, код, false),
                _ => {
                    нажать(state, код, true);
                    нажать(state, код, false);
                }
            }
            "ok".into()
        }
        Some("drag") => {
            let (Some(x1), Some(y1), Some(x2), Some(y2)) =
                (число(1), число(2), число(3), число(4))
            else {
                return "ошибка: mouse drag X1 Y1 X2 Y2".into();
            };
            навести(state, x1, y1);
            нажать(state, crate::synth::КНОПКА_ЛКМ, true);
            // Дробим на шаги, а не прыгаем разом: dawn считает драг по потоку
            // событий (порог 4 px у миникарты, инерция у пана, расталкивание
            // окон), и один прыжок прошёл бы мимо всей этой логики.
            const ШАГОВ: i32 = 12;
            for шаг in 1..=ШАГОВ {
                let t = шаг as f64 / ШАГОВ as f64;
                let цель_x = x1 + (x2 - x1) * t;
                let цель_y = y1 + (y2 - y1) * t;
                навести(state, цель_x, цель_y);
            }
            нажать(state, crate::synth::КНОПКА_ЛКМ, false);
            указатель(state)
        }
        Some("scroll") => {
            let Some(n) = число(1) else {
                return "ошибка: mouse scroll N (зубцы, вниз > 0)".into();
            };
            state.process_input_event(smithay::backend::input::InputEvent::PointerAxis::<
                crate::synth::Синтетика,
            > {
                event: crate::synth::Колесо::new(n),
            });
            state.request_redraw();
            "ok".into()
        }
        _ => "ошибка: mouse to|move|down|up|click|drag|scroll".into(),
    }
}

/// Шлем: состояние и управление VR-режимом из сокета.
///
/// Ради этой команды VR и умеет включаться в headless (см. vr::тик_с): она
/// единственный способ проверить сцену, панели и ввод, не надевая Quest и не
/// трогая живой сеанс. Ответ `status` печатает то же, что видел бы человек в
/// `mine [on|off|mode|layout|status|panels|ray x y z dx dy dz]` — режим
/// Minecraft (см. mine/).
///
/// Ради `ray` режим и умеет работать в headless: лучи в харнессе слать некому,
/// а проверять, что взгляд попадает в ту панель и в тот её пиксель, надо — это
/// ровно то место, где ошибка выглядит как «клик уходит не туда», и найти её
/// глазами в игре почти невозможно.
fn шахта(хвост: &str, state: &mut Dawn) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    match слова.first().copied() {
        None | Some("status") => crate::mine::состояние(state),
        Some("mode") => {
            crate::mine::режим(state);
            crate::mine::состояние(state)
        }
        Some("on") => match crate::mine::включить(state) {
            Ok(()) => crate::mine::состояние(state),
            Err(e) => format!("ошибка: {e}"),
        },
        Some("off") => {
            crate::mine::выключить(state);
            "ok: режим выключен".into()
        }
        Some("layout") => {
            let новая = crate::mine::сменить_раскладку(state);
            format!("ok: раскладка {}", новая.имя())
        }
        Some("panels") => crate::mine::панели_строкой(state),
        Some("ray") => {
            let числа: Vec<f32> = слова[1..]
                .iter()
                .filter_map(|с| с.parse::<f32>().ok())
                .collect();
            if числа.len() != 6 {
                return "ошибка: mine ray <x y z> <dx dy dz>".into();
            }
            crate::mine::луч_снаружи(
                state,
                [числа[0], числа[1], числа[2]],
                [числа[3], числа[4], числа[5]],
            )
        }
        // `mine pin <ключ> <x> <y>` — то же, что ЛКМ по панели в игре: взять
        // управление одним окном и вести по нему стрелку мышью. `mine pin 0`
        // отпускает. Без этой команды закрепление проверялось бы только живой
        // игрой — а это ровно тот путь ввода, где промах не видно глазами.
        Some("pin") => {
            let ключ: u64 = слова.get(1).and_then(|с| с.parse().ok()).unwrap_or(0);
            let x: f32 = слова.get(2).and_then(|с| с.parse().ok()).unwrap_or(0.0);
            let y: f32 = слова.get(3).and_then(|с| с.parse().ok()).unwrap_or(0.0);
            crate::mine::закрепить_снаружи(state, ключ, x, y)
        }
        // `mine game <номер из windows>` — объявить окно окном игры («-» снять).
        // Живьём оно находится по pid мода; у харнесса мода-процесса с окном нет
        // вовсе, и без этой команды правило «игра всегда сверху и клавиатура её»
        // проверялось бы только запуском Minecraft.
        Some("game") => {
            let номер = слова.get(1).and_then(|с| с.parse::<usize>().ok());
            crate::mine::назначить_игру(state, номер)
        }
        иное => format!(
            "ошибка: mine on|off|mode|layout|status|panels|ray x y z dx dy dz|pin ключ x y|\
             game номер (было '{:?}')",
            иное
        ),
    }
}

/// шлеме: сколько панелей, какая раскладка, идёт ли passthrough.
fn шлем(хвост: &str, state: &mut Dawn) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    match слова.first().copied() {
        None | Some("status") => crate::vr::состояние(state),
        Some("on") => match crate::vr::включить(state) {
            Ok(()) => "ok: шлем запрошен, подключение на ближайшем тике".into(),
            Err(e) => format!("ошибка: {e}"),
        },
        Some("off") => {
            crate::vr::выключить(state);
            "ok".into()
        }
        // То же самое, что Super+Alt+V: сервер, ожидание шлема, вход.
        Some("mode") => {
            crate::vr::режим(state);
            crate::vr::состояние(state)
        }
        Some("ar") => {
            if state.vr.is_none() {
                return "ошибка: шлем не включён".into();
            }
            use crate::vr::Ар;
            match crate::vr::переключить_ар(state) {
                Ар::Включена => "ok: дополненная реальность включена".into(),
                Ар::Выключена => "ok: дополненная реальность выключена".into(),
                Ар::НеУмеет => format!(
                    "ok: рантайм не показывает комнату — остались в VR ({})",
                    state
                        .vr
                        .as_ref()
                        .map(|вр| вр.шлем.смешивание_строкой())
                        .unwrap_or_default()
                ),
            }
        }
        Some("layout") => {
            let р = crate::vr::сменить_раскладку(state);
            format!("ok: раскладка «{}»", р.имя())
        }
        Some("recenter") => {
            crate::vr::пересобрать(state);
            "ok".into()
        }
        // Пульты в шлеме: «Пуск» и клавиатура. То же, что кнопка меню на
        // контроллере, ладонь вверх и бинды vr_launcher/vr_keyboard.
        Some("menu") | Some("keys") => {
            let вид = if слова[0] == "menu" {
                crate::vr::ui::Вид::Пуск
            } else {
                crate::vr::ui::Вид::Клавиатура
            };
            match crate::vr::пульт(state, вид) {
                Ok(открыт) => format!(
                    "ok: пульт «{}» {}",
                    вид.имя(),
                    if открыт { "открыт" } else { "спрятан" }
                ),
                Err(e) => format!("ошибка: {e}"),
            }
        }
        Some("pults") => crate::vr::пульты_строкой(state),
        // `vr press menu|keys <номер>` — нажать кнопку пульта без шлема: в
        // харнессе луча указки нет, а проверять пульты чем-то надо.
        Some("press") => {
            let вид = match слова.get(1).copied() {
                Some("menu") | Some("пуск") => crate::vr::ui::Вид::Пуск,
                Some("keys") | Some("клавиатура") => crate::vr::ui::Вид::Клавиатура,
                _ => return "ошибка: vr press menu|keys <номер кнопки>".into(),
            };
            let Some(номер) = слова.get(2).and_then(|слово| слово.parse::<usize>().ok()) else {
                return "ошибка: vr press menu|keys <номер кнопки>".into();
            };
            match crate::vr::нажать(state, вид, номер) {
                Ok(подпись) => format!("ok: нажата «{подпись}»"),
                Err(e) => format!("ошибка: {e}"),
            }
        }
        Some("panels") => crate::vr::панели_строкой(state),
        Some("input") => crate::vr::ввод_строкой(state),
        // `vr gestures` — вся раскладка жестов разом: имя для конфига, что жест
        // делает сейчас и переназначен ли он. Без неё единственный способ
        // узнать, что делает кулак, — надеть шлем и сжать его.
        Some("gestures") => crate::vr::жесты_строкой(state),
        Some("shot") => {
            let Some(путь) = слова.get(1) else {
                return "ошибка: vr shot <путь.png>".into();
            };
            match crate::vr::снимок(state, std::path::PathBuf::from(путь)) {
                Ok(()) => format!("ok: снимок глаза будет в {путь}"),
                Err(e) => format!("ошибка: {e}"),
            }
        }
        иное => format!(
            "ошибка: vr mode|on|off|ar|layout|recenter|menu|keys|pults|press <пульт> <n>|\
             status|panels|input|gestures|shot <путь> (было '{:?}')",
            иное
        ),
    }
}

fn двинуть(dx: f64, dy: f64) -> smithay::backend::input::InputEvent<crate::synth::Синтетика> {
    smithay::backend::input::InputEvent::PointerMotion {
        event: crate::synth::Движение::new(dx, dy),
    }
}

fn навести(state: &mut Dawn, x: f64, y: f64) {
    let сейчас = state.pointer_screen_physical();
    state.process_input_event(двинуть(x - сейчас.x, y - сейчас.y));
}

fn нажать(state: &mut Dawn, код: u32, нажата: bool) {
    state.process_input_event(smithay::backend::input::InputEvent::PointerButton::<
        crate::synth::Синтетика,
    > {
        event: crate::synth::Кнопка::new(код, нажата),
    });
    state.request_redraw();
}

/// Клавиша или сочетание: `key logo+shift+d`, `key esc`, `key 32` (evdev-код).
/// Модификаторы нажимаются и отпускаются вокруг основной клавиши — иначе
/// бинды, завязанные на удержание Super, не срабатывают.
fn клавиша(хвост: &str, state: &mut Dawn) -> String {
    if хвост.is_empty() {
        return "ошибка: key [logo+shift+]<имя>".into();
    }
    // `key down logo` / `key up logo` — клавиша ОСТАЁТСЯ зажатой между
    // командами. Без этого из сокета нельзя воспроизвести ни одного жеста с
    // удержанием: Super+ЛКМ (перетаскивание окна), Super+ПКМ (ресайз) — всё,
    // что начинается модификатором и продолжается мышью.
    if let Some((что, имя)) = хвост.split_once(char::is_whitespace) {
        let нажата = match что {
            "down" => true,
            "up" => false,
            _ => return format!("ошибка: key down|up <имя>, а не '{}'", что),
        };
        let Some(код) = код_клавиши(имя.trim()) else {
            return format!("ошибка: не знаю клавишу '{}'", имя.trim());
        };
        нажать_клавишу(state, код, нажата);
        return "ok".into();
    }
    let части: Vec<&str> = хвост.split('+').map(|s| s.trim()).collect();
    let (последняя, модификаторы) = части.split_last().unwrap();
    let Some(основная) = код_клавиши(последняя) else {
        return format!("ошибка: не знаю клавишу '{}'", последняя);
    };
    let mut коды = Vec::new();
    for м in модификаторы {
        match код_клавиши(м) {
            Some(к) => коды.push(к),
            None => return format!("ошибка: не знаю модификатор '{}'", м),
        }
    }
    for &к in &коды {
        нажать_клавишу(state, к, true);
    }
    нажать_клавишу(state, основная, true);
    нажать_клавишу(state, основная, false);
    for &к in коды.iter().rev() {
        нажать_клавишу(state, к, false);
    }
    "ok".into()
}

fn нажать_клавишу(state: &mut Dawn, код: u32, нажата: bool) {
    state.process_input_event(smithay::backend::input::InputEvent::Keyboard::<
        crate::synth::Синтетика,
    > {
        event: crate::synth::Клавиша::new(код, нажата),
    });
    state.request_redraw();
}

/// Имя → скан-код evdev. Числом можно задать любой код напрямую.
fn код_клавиши(имя: &str) -> Option<u32> {
    if let Ok(n) = имя.parse::<u32>() {
        // Голое число — это КОД, а не цифровая клавиша: «key 32» = KEY_D.
        // Цифры набираются именами: «key цифра1».
        return Some(n);
    }
    let буквы = "qwertyuiop";
    if имя.chars().count() == 1 {
        let c = имя.chars().next().unwrap().to_ascii_lowercase();
        if let Some(i) = буквы.find(c) {
            return Some(16 + i as u32);
        }
        if let Some(i) = "asdfghjkl".find(c) {
            return Some(30 + i as u32);
        }
        if let Some(i) = "zxcvbnm".find(c) {
            return Some(44 + i as u32);
        }
    }
    let код = match имя {
        "esc" | "escape" => 1,
        "цифра1" | "d1" => 2,
        "цифра2" | "d2" => 3,
        "цифра3" | "d3" => 4,
        "цифра4" | "d4" => 5,
        "цифра5" | "d5" => 6,
        "цифра6" | "d6" => 7,
        "цифра7" | "d7" => 8,
        "цифра8" | "d8" => 9,
        "цифра9" | "d9" => 10,
        "цифра0" | "d0" => 11,
        "backspace" => 14,
        "tab" => 15,
        "enter" | "return" => 28,
        "ctrl" | "control" => 29,
        "shift" => 42,
        "alt" => 56,
        "space" | "пробел" => 57,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "home" => 102,
        "up" | "вверх" => 103,
        "pgup" => 104,
        "left" | "влево" => 105,
        "right" | "вправо" => 106,
        "end" => 107,
        "down" | "вниз" => 108,
        "pgdn" => 109,
        "insert" => 110,
        "delete" => 111,
        "logo" | "super" | "win" => 125,
        _ => return None,
    };
    Some(код)
}

/// Где сейчас указатель и что под ним. Первое, что нужно после `mouse to`:
/// без этого «клик ушёл не туда» отличить от «клик дошёл, но обработчик
/// промолчал» нечем.
fn указатель(state: &mut Dawn) -> String {
    let экран = state.pointer_screen_physical();
    let холст = state.pointer_location;
    let под = state
        .surface_under(холст)
        .map(|(_, точка)| format!("есть, локально ({:.0},{:.0})", точка.x, точка.y))
        .unwrap_or_else(|| "нет".into());
    format!(
        "экран=({:.1},{:.1}) холст=({:.1},{:.1}) камера=({:.1},{:.1}) зум={:.2} \
         поверхность={} панель={:?} миникарта_ручная={}",
        экран.x, экран.y, холст.x, холст.y,
        state.viewport.cam_x, state.viewport.cam_y, state.viewport.zoom,
        под, state.bar_hover, state.minimap_manual,
    )
}

/// Список окон в том виде, в каком его надо мерить: что попросили у клиента и
/// что клиент реально нарисовал — расхождение этих двух чисел и есть «окно не
/// ужимается» (см. udev.rs, поиск "нужен_кроп").
fn окна(state: &Dawn) -> String {
    use smithay::desktop::WindowSurface;
    let mut out = String::new();
    let фокус = state.focused_surface();
    for (i, window) in state.space.elements().enumerate() {
        let geo = state.space.element_geometry(window)
            .map(|g| format!("{},{} {}x{}", g.loc.x, g.loc.y, g.size.w, g.size.h))
            .unwrap_or_else(|| "нет".into());
        let свой = window.geometry().size;
        let запрошено = match crate::xwin::requested_size(window) {
            Some(s) => format!("{}x{}", s.w, s.h),
            None => "—".into(),
        };
        let буфер = crate::xwin::surface(window)
            .and_then(|wl| crate::xwin::surface_buffer_size(&wl))
            .map(|s| format!("{}x{}", s.w, s.h))
            .unwrap_or_else(|| "—".into());
        let класс = match window.underlying_surface() {
            WindowSurface::X11(_) => "x11",
            WindowSurface::Wayland(_) => "wl",
        };
        let активно = match &фокус {
            Some(f) if crate::xwin::is_surface(window, f) => "*",
            _ => " ",
        };
        // Стол и монитор: без них список окон на двух мониторах читается
        // гаданием — одинаковые терминалы отличаются только координатой, а
        // «чей это стол» и есть предмет проверки (см. columns_геометрия_стола).
        let (стол, монитор) = state.tagged_windows.iter()
            .find(|tw| crate::dwindle::same_window(&tw.window, window))
            .map(|tw| {
                let n = if tw.tags == 0 { 0 } else { tw.tags.trailing_zeros() + 1 };
                let m = state.монитор_стола(tw.tags)
                    .map(|i| (i + 1).to_string())
                    .unwrap_or_else(|| "—".into());
                (n.to_string(), m)
            })
            .unwrap_or_else(|| ("—".into(), "—".into()));
        out.push_str(&format!(
            "{}{} {} app={} стол={} монитор={} слот={} своя={}x{} запрошено={} буфер={}\n",
            активно, i, класс,
            crate::xwin::app_id(window).unwrap_or_else(|| "—".into()),
            стол, монитор,
            geo, свой.w, свой.h, запрошено, буфер,
        ));
    }
    if out.is_empty() {
        out.push_str("окон нет");
    }
    out.trim_end().to_string()
}
