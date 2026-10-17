//! Управляющий сокет: дёргать действия parallax и снимать кадры без клавиатуры.
//!
//! **Зачем.** Проверить правку рендера можно было только руками за машиной:
//! действия parallax достижимы исключительно с физической клавиатуры, а второй
//! экземпляр на свободном VT читает те же evdev-устройства, что и живой сеанс,
//! то есть ловит чужие нажатия (и умеет, например, увести VT). Сокет разрывает
//! эту связку: тот же `dispatch_action`, но по строке в UNIX-сокет.
//!
//! **Где включается.** Только по `PLX_CTL=1` (и всегда — в `--headless`).
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

use crate::Parallax;
use crate::{т, тф};

/// Путь сокета: `PLX_CTL_SOCKET`, иначе `$XDG_RUNTIME_DIR/plx-<сокет>.ctl`.
///
/// Имя вейландовского сокета в пути не случайно: на машине может идти живой
/// сеанс и харнесс разом, и попасть командой не в тот экземпляр — ровно та
/// ошибка, ради избежания которой всё это и написано.
pub fn путь_сокета(state: &Parallax) -> std::path::PathBuf {
    if let Some(явный) = std::env::var_os("PLX_CTL_SOCKET") {
        return std::path::PathBuf::from(явный);
    }
    let каталог = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    каталог.join(format!("plx-{}.ctl", state.socket_name.to_string_lossy()))
}

pub fn init(
    handle: &LoopHandle<'static, Parallax>,
    state: &Parallax,
) -> Result<(), Box<dyn std::error::Error>> {
    let путь = путь_сокета(state);
    // Остаток от прошлого запуска: bind по занятому пути падает с EADDRINUSE,
    // а живого слушателя там быть не может — мы его только что не запускали.
    let _ = std::fs::remove_file(&путь);
    let listener = UnixListener::bind(&путь)?;
    listener.set_nonblocking(true)?;
    tracing::info!("plx/ctl: listening on {:?}", путь);

    handle.insert_source(
        Generic::new(listener, Interest::READ, Mode::Level),
        |_, listener, state: &mut Parallax| {
            loop {
                match listener.accept() {
                    Ok((stream, _)) => обслужить(stream, state),
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        tracing::warn!("plx/ctl: accept: {}", e);
                        break;
                    }
                }
            }
            Ok(PostAction::Continue)
        },
    )?;
    Ok(())
}

fn обслужить(stream: UnixStream, state: &mut Parallax) {
    // Блокирующее чтение — сознательно: команда это одна короткая строка от
    // своего же харнесса, а неблокирующий разбор по кускам стоил бы буфера
    // состояния на соединение ради нулевой выгоды.
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(500)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("plx/ctl: clone: {}", e);
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

fn выполнить(строка: &str, state: &mut Parallax) -> String {
    let (команда, хвост) = match строка.split_once(char::is_whitespace) {
        Some((имя, остаток)) => (имя, остаток.trim()),
        None => (строка, ""),
    };
    match команда {
        "shot" => {
            if хвост.is_empty() {
                return т!("ошибка: shot без пути", "error: shot without a path").into();
            }
            state.shot_request = Some(std::path::PathBuf::from(хвост));
            state.request_redraw();
            тф!("ok: снимок будет в {}", "ok: the screenshot will go to {}", хвост)
        }
        "action" => {
            let (имя, аргументы) = match хвост.split_once(char::is_whitespace) {
                Some((имя, остаток)) => (имя, остаток.trim()),
                None => (хвост, ""),
            };
            match crate::config::action_from_str(имя, аргументы) {
                Some(действие) => {
                    tracing::info!("plx/ctl: action {} {{{}}}", имя, аргументы);
                    state.dispatch_action(действие);
                    state.request_redraw();
                    "ok".into()
                }
                None => тф!("ошибка: не разобрал действие '{}' {{{}}}", "error: could not parse the action '{}' {{{}}}", имя, аргументы),
            }
        }
        "windows" => окна(state),
        "mouse" => мышь(хвост, state),
        "touch" => сенсор(хвост, state),
        "key" => клавиша(хвост, state),
        "pointer" => указатель(state),
        "cube" => куб(state),
        "tilt" => state.наклон_отчёт(),
        "carousel" => карусель(state),
        "share" => раздача(хвост, state),
        "portal" => портал(хвост, state),
        "vr" => шлем(хвост, state),
        "mine" => шахта(хвост, state),
        "widget" => виджет(хвост, state),
        "icon" => иконка(хвост, state),
        "desk" => стол_виджеты(хвост, state),
        "picture" => картинка(хвост, state),
        "wallpaper" => обои(хвост, state),
        "help" => т!(
            "shot <путь> | action <имя> [k=v ...] | windows | pointer | cube | carousel | \
             mouse to X Y | mouse move DX DY | mouse down|up|click [левая|правая|средняя] | \
             mouse drag X1 Y1 X2 Y2 | mouse scroll N | \
             touch down|move <слот> X Y | touch up <слот> | touch tap X Y | \
             touch cancel | touch status | \
             key [logo+shift+]<имя> | key down|up <имя> | \
             share [start [порт] | stop | status] | \
             portal [pick [типы] | cancel] | \
             vr [mode|on|off|ar|layout|recenter|menu|keys|pults|press <пульт> <n>|\
             status|panels|input|gestures|shot <путь>] | \
             mine [on|off|mode|layout|status|panels|ray x y z dx dy dz|pin ключ x y|game номер] | \
             widget [list | on|off [app_id] | move X Y [app_id]] | \
             icon [list | add имя [cmd=…] [image=…] [x=…] [y=…] [size=…] [desktop=…] | rm имя | move имя X Y | desktop имя N] | \
             desk [list | add clock|calendar|system|note [x=…] [y=…] [scale=…] [text=…] [desktop=…] | rm N | move N X Y | desktop N СТОЛ] | \
             picture drop X Y <uri|путь> | wallpaper [cycle on|off|toggle]",
            "shot <path> | action <name> [k=v ...] | windows | pointer | cube | carousel | \
             mouse to X Y | mouse move DX DY | mouse down|up|click [left|right|middle] | \
             mouse drag X1 Y1 X2 Y2 | mouse scroll N | \
             touch down|move <slot> X Y | touch up <slot> | touch tap X Y | \
             touch cancel | touch status | \
             key [logo+shift+]<name> | key down|up <name> | \
             share [start [port] | stop | status] | \
             portal [pick [types] | cancel] | \
             vr [mode|on|off|ar|layout|recenter|menu|keys|pults|press <panel> <n>|\
             status|panels|input|gestures|shot <path>] | \
             mine [on|off|mode|layout|status|panels|ray x y z dx dy dz|pin key x y|game number] | \
             widget [list | on|off [app_id] | move X Y [app_id]] | \
             icon [list | add name [cmd=…] [image=…] [x=…] [y=…] [size=…] [desktop=…] | rm name | move name X Y | desktop name N] | \
             desk [list | add clock|calendar|system|note [x=…] [y=…] [scale=…] [text=…] [desktop=…] | rm N | move N X Y | desktop N СТОЛ] | \
             picture drop X Y <uri|path> | wallpaper [cycle on|off|toggle]"
        )
        .into(),
        иное => тф!("ошибка: не знаю команду '{}'", "error: unknown command '{}'", иное),
    }
}

/// Демонстрация экрана из терминала: `portal pick [типы]`, `portal cancel`.
///
/// Только для харнесса. В headless портал на шину не выходит вовсе (см.
/// `main.rs`), и подсветку выбора источника — ту самую, что видит человек,
/// когда Discord или OBS просят экран, — иначе можно было посмотреть лишь на
/// живом сеансе. `типы` те же, что в спецификации: 1 — только мониторы (так
/// шлёт OBS), 2 — только окна, 3 — и то и другое.
fn портал(хвост: &str, state: &mut Parallax) -> String {
    let (что, довод) = match хвост.split_once(char::is_whitespace) {
        Some((первое, остаток)) => (первое, остаток.trim()),
        None => (хвост, ""),
    };
    match что {
        "" | "pick" | "выбор" => {
            let типы = довод.parse::<u32>().unwrap_or(3).clamp(1, 3);
            state.portal_pick_debug(типы);
            тф!("ok: выбор источника (типы={типы})", "ok: source picker (types={типы})")
        }
        "cancel" | "отмена" => {
            if !state.portal_picking() {
                return т!("выбор и так не идёт", "no pick is running anyway").into();
            }
            state.portal_pick_click(true);
            т!("ok: выбор отменён", "ok: pick cancelled").into()
        }
        иное => тф!("ошибка: portal [pick [типы] | cancel], а не '{иное}'", "error: portal [pick [types] | cancel], not '{иное}'"),
    }
}

/// Мультиюзер из терминала: `share`, `share start [порт]`, `share stop`.
///
/// Отдельная команда, а не `action share_start`, ровно из-за ОТВЕТА. Действие
/// умеет сказать только «ok»: код доступа и адрес, которые человек диктует
/// гостю, остаются в логе композитора, куда из терминала не заглянешь.
/// Здесь всё нужное возвращается прямо в терминал одной строкой.
fn раздача(хвост: &str, state: &mut Parallax) -> String {
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
                Err(причина) => тф!("ошибка: {причина}", "error: {причина}"),
            }
        }
        "stop" | "off" | "выкл" => {
            if !state.раздача_идёт() {
                return т!("раздача и так выключена", "sharing is already off").into();
            }
            state.раздача_закончить();
            т!("ok: раздача выключена", "ok: sharing stopped").into()
        }
        иное => тф!("ошибка: share [start [порт] | stop | status], а не '{иное}'", "error: share [start [port] | stop | status], not '{иное}'"),
    }
}

#[cfg(not(feature = "share"))]
fn состояние_раздачи(_state: &Parallax) -> String {
    crate::share::нет_фичи().into()
}

#[cfg(feature = "share")]
fn состояние_раздачи(state: &Parallax) -> String {
    let Some(раздача) = state.раздача.as_ref() else {
        return т!("раздача выключена", "sharing is off").into();
    };
    let mut ответ = тф!(
        "раздача идёт: код {}, порт {}, кадр {}×{}", "sharing is on: code {}, port {}, frame {}×{}",
        раздача.код, раздача.порт, раздача.кадр.0, раздача.кадр.1,
    );
    for адрес in адреса_машины() {
        ответ.push_str(&тф!("\n  гостю: plx-share {}:{} {}", "\n  for the guest: plx-share {}:{} {}", адрес, раздача.порт, раздача.код));
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
                if гость.за_хостом { т!(", за хостом", ", following the host") } else { "" },
            )
        })
        .collect();
    ответ.push_str(&тф!(
        "\n  гостей: {}{}", "\n  guests: {}{}",
        гости.len(),
        if гости.is_empty() { String::new() } else { format!(" — {}", гости.join(", ")) },
    ));
    ответ
}

/// Адреса этой машины, по которым до неё дотянется гость.
///
/// Читаем `getifaddrs`, а не зовём `ip`: команда есть не везде, а формат её
/// вывода — не договор. Отбрасываем loopback (по нему придёт только тот, кто
/// уже сидит за этой же машиной) и не-IP семейства.
#[cfg(feature = "share")]
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
/// Перевод в холст делает сам parallax (`pointer_location` += дельта ⁄ зум),
/// поэтому «навести в точку» — это ровно одно событие с разностью координат.
fn мышь(хвост: &str, state: &mut Parallax) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    let число = |i: usize| слова.get(i).and_then(|s| s.parse::<f64>().ok());

    match слова.first().copied() {
        Some("to") => {
            let (Some(x), Some(y)) = (число(1), число(2)) else {
                return т!("ошибка: mouse to X Y", "error: mouse to X Y").into();
            };
            навести(state, x, y);
            указатель(state)
        }
        Some("move") => {
            let (Some(dx), Some(dy)) = (число(1), число(2)) else {
                return т!("ошибка: mouse move DX DY", "error: mouse move DX DY").into();
            };
            state.process_input_event(двинуть(dx, dy));
            указатель(state)
        }
        Some(движение @ ("down" | "up" | "click")) => {
            let код = match слова.get(1) {
                None => crate::synth::КНОПКА_ЛКМ,
                Some(имя) => match crate::synth::кнопка_по_имени(имя) {
                    Some(к) => к,
                    None => return тф!("ошибка: не знаю кнопку '{}'", "error: unknown button '{}'", имя),
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
                return т!("ошибка: mouse drag X1 Y1 X2 Y2", "error: mouse drag X1 Y1 X2 Y2").into();
            };
            навести(state, x1, y1);
            нажать(state, crate::synth::КНОПКА_ЛКМ, true);
            // Дробим на шаги, а не прыгаем разом: parallax считает драг по потоку
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
                return т!("ошибка: mouse scroll N (зубцы, вниз > 0)", "error: mouse scroll N (detents, down > 0)").into();
            };
            state.process_input_event(smithay::backend::input::InputEvent::PointerAxis::<
                crate::synth::Синтетика,
            > {
                event: crate::synth::Колесо::new(n),
            });
            state.request_redraw();
            "ok".into()
        }
        _ => т!("ошибка: mouse to|move|down|up|click|drag|scroll", "error: mouse to|move|down|up|click|drag|scroll").into(),
    }
}

/// Сенсорный экран из сокета: `touch down 0 X Y`, `touch move 0 X Y`,
/// `touch up 0`, `touch tap X Y`, `touch cancel`, `touch status`.
///
/// Координаты — ЭКРАННЫЕ пиксели, а не холст (в отличие от `mouse to`): палец
/// лежит на стекле, и харнесс обязан задавать его там же, где его задаёт
/// драйвер. Другого способа проверить сенсор нет вовсе — синтетический бэкенд
/// харнесса объявляет типы касаний `UnusedEvent` (см. synth.rs), то есть
/// событие тачскрина в нём невыразимо в принципе.
fn сенсор(хвост: &str, state: &mut Parallax) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    let число = |i: usize| слова.get(i).and_then(|s| s.parse::<f64>().ok());
    let слот = |i: usize| слова.get(i).and_then(|s| s.parse::<i32>().ok());
    let время = || std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u32)
        .unwrap_or(0);

    match слова.first().copied() {
        Some("down") | Some("вниз") => {
            let (Some(с), Some(x), Some(y)) = (слот(1), число(2), число(3)) else {
                return т!("ошибка: touch down <слот> X Y", "error: touch down <slot> X Y").into();
            };
            state.сенсор_вниз(с, (x, y).into(), время());
            state.сенсор_состояние()
        }
        Some("move") | Some("движение") => {
            let (Some(с), Some(x), Some(y)) = (слот(1), число(2), число(3)) else {
                return т!("ошибка: touch move <слот> X Y", "error: touch move <slot> X Y").into();
            };
            state.сенсор_движение(с, (x, y).into(), время());
            state.сенсор_состояние()
        }
        Some("up") | Some("вверх") => {
            let Some(с) = слот(1) else {
                return т!("ошибка: touch up <слот>", "error: touch up <slot>").into();
            };
            state.сенсор_вверх(с, время());
            state.сенсор_состояние()
        }
        Some("tap") | Some("тык") => {
            let (Some(x), Some(y)) = (число(1), число(2)) else {
                return т!("ошибка: touch tap X Y", "error: touch tap X Y").into();
            };
            state.сенсор_вниз(0, (x, y).into(), время());
            state.сенсор_вверх(0, время());
            state.сенсор_состояние()
        }
        Some("cancel") | Some("отмена") => {
            state.сенсор_отмена();
            state.сенсор_состояние()
        }
        Some("status") | Some("") | None => state.сенсор_состояние(),
        Some(иное) => тф!(
            "ошибка: не знаю 'touch {}'", "error: unknown 'touch {}'", иное
        ),
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
fn шахта(хвост: &str, state: &mut Parallax) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    match слова.first().copied() {
        None | Some("status") => crate::mine::состояние(state),
        Some("mode") => {
            crate::mine::режим(state);
            crate::mine::состояние(state)
        }
        Some("on") => match crate::mine::включить(state) {
            Ok(()) => crate::mine::состояние(state),
            Err(e) => тф!("ошибка: {e}", "error: {e}"),
        },
        Some("off") => {
            crate::mine::выключить(state);
            т!("ok: режим выключен", "ok: mode off").into()
        }
        Some("layout") => {
            let новая = crate::mine::сменить_раскладку(state);
            тф!("ok: раскладка {}", "ok: layout {}", новая.имя())
        }
        Some("panels") => crate::mine::панели_строкой(state),
        Some("ray") => {
            let числа: Vec<f32> = слова[1..]
                .iter()
                .filter_map(|сл| сл.parse::<f32>().ok())
                .collect();
            if числа.len() != 6 {
                return т!("ошибка: mine ray <x y z> <dx dy dz>", "error: mine ray <x y z> <dx dy dz>").into();
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
            let ключ: u64 = слова.get(1).and_then(|сл| сл.parse().ok()).unwrap_or(0);
            let x: f32 = слова.get(2).and_then(|сл| сл.parse().ok()).unwrap_or(0.0);
            let y: f32 = слова.get(3).and_then(|сл| сл.parse().ok()).unwrap_or(0.0);
            crate::mine::закрепить_снаружи(state, ключ, x, y)
        }
        // `mine game <номер из windows>` — объявить окно окном игры («-» снять).
        // Живьём оно находится по pid мода; у харнесса мода-процесса с окном нет
        // вовсе, и без этой команды правило «игра всегда сверху и клавиатура её»
        // проверялось бы только запуском Minecraft.
        Some("game") => {
            let номер = слова.get(1).and_then(|сл| сл.parse::<usize>().ok());
            crate::mine::назначить_игру(state, номер)
        }
        иное => тф!(
            "ошибка: mine on|off|mode|layout|status|panels|ray x y z dx dy dz|pin ключ x y|\
             game номер (было '{:?}')",
            "error: mine on|off|mode|layout|status|panels|ray x y z dx dy dz|pin key x y|\
             game number (got '{:?}')",
            иное
        ),
    }
}

/// шлеме: сколько панелей, какая раскладка, идёт ли passthrough.
fn шлем(хвост: &str, state: &mut Parallax) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    match слова.first().copied() {
        None | Some("status") => crate::vr::состояние(state),
        Some("on") => match crate::vr::включить(state) {
            Ok(()) => т!("ok: шлем запрошен, подключение на ближайшем тике", "ok: headset requested, it will connect on the next tick").into(),
            Err(e) => тф!("ошибка: {e}", "error: {e}"),
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
                return т!("ошибка: шлем не включён", "error: the headset is not on").into();
            }
            use crate::vr::АР;
            match crate::vr::переключить_ар(state) {
                АР::Включена => т!("ok: дополненная реальность включена", "ok: passthrough on").into(),
                АР::Выключена => т!("ok: дополненная реальность выключена", "ok: passthrough off").into(),
                АР::НеУмеет => тф!(
                    "ok: рантайм не показывает комнату — остались в VR ({})", "ok: the runtime has no passthrough — staying in VR ({})",
                    crate::vr::смешивание_строкой(state)
                ),
            }
        }
        Some("layout") => {
            let раскл = crate::vr::сменить_раскладку(state);
            тф!("ok: раскладка «{}»", "ok: layout '{}'", раскл.имя())
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
                Ok(открыт) => тф!(
                    "ok: пульт «{}» {}", "ok: control panel '{}' {}",
                    вид.имя(),
                    if открыт { т!("открыт", "opened") } else { т!("спрятан", "hidden") }
                ),
                Err(e) => тф!("ошибка: {e}", "error: {e}"),
            }
        }
        Some("pults") => crate::vr::пульты_строкой(state),
        // `vr press menu|keys <номер>` — нажать кнопку пульта без шлема: в
        // харнессе луча указки нет, а проверять пульты чем-то надо.
        Some("press") => {
            let вид = match слова.get(1).copied() {
                Some("menu") | Some("пуск") => crate::vr::ui::Вид::Пуск,
                Some("keys") | Some("клавиатура") => crate::vr::ui::Вид::Клавиатура,
                _ => return т!("ошибка: vr press menu|keys <номер кнопки>", "error: vr press menu|keys <button number>").into(),
            };
            let Some(номер) = слова.get(2).and_then(|слово| слово.parse::<usize>().ok()) else {
                return т!("ошибка: vr press menu|keys <номер кнопки>", "error: vr press menu|keys <button number>").into();
            };
            match crate::vr::нажать(state, вид, номер) {
                Ok(подпись) => тф!("ok: нажата «{подпись}»", "ok: pressed '{подпись}'"),
                Err(e) => тф!("ошибка: {e}", "error: {e}"),
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
                return т!("ошибка: vr shot <путь.png>", "error: vr shot <path.png>").into();
            };
            match crate::vr::снимок(state, std::path::PathBuf::from(путь)) {
                Ok(()) => тф!("ok: снимок глаза будет в {путь}", "ok: the eye screenshot will go to {путь}"),
                Err(e) => тф!("ошибка: {e}", "error: {e}"),
            }
        }
        иное => тф!(
            "ошибка: vr mode|on|off|ar|layout|recenter|menu|keys|pults|press <пульт> <n>|\
             status|panels|input|gestures|shot <путь> (было '{:?}')",
            "error: vr mode|on|off|ar|layout|recenter|menu|keys|pults|press <panel> <n>|\
             status|panels|input|gestures|shot <path> (got '{:?}')",
            иное
        ),
    }
}

fn двинуть(dx: f64, dy: f64) -> smithay::backend::input::InputEvent<crate::synth::Синтетика> {
    smithay::backend::input::InputEvent::PointerMotion {
        event: crate::synth::Движение::new(dx, dy),
    }
}

fn навести(state: &mut Parallax, x: f64, y: f64) {
    let сейчас = state.pointer_screen_physical();
    state.process_input_event(двинуть(x - сейчас.x, y - сейчас.y));
}

fn нажать(state: &mut Parallax, код: u32, нажата: bool) {
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
fn клавиша(хвост: &str, state: &mut Parallax) -> String {
    if хвост.is_empty() {
        return т!("ошибка: key [logo+shift+]<имя>", "error: key [logo+shift+]<name>").into();
    }
    // `key down logo` / `key up logo` — клавиша ОСТАЁТСЯ зажатой между
    // командами. Без этого из сокета нельзя воспроизвести ни одного жеста с
    // удержанием: Super+ЛКМ (перетаскивание окна), Super+ПКМ (ресайз) — всё,
    // что начинается модификатором и продолжается мышью.
    if let Some((что, имя)) = хвост.split_once(char::is_whitespace) {
        let нажата = match что {
            "down" => true,
            "up" => false,
            _ => return тф!("ошибка: key down|up <имя>, а не '{}'", "error: key down|up <name>, not '{}'", что),
        };
        let Some(код) = код_клавиши(имя.trim()) else {
            return тф!("ошибка: не знаю клавишу '{}'", "error: unknown key '{}'", имя.trim());
        };
        нажать_клавишу(state, код, нажата);
        return "ok".into();
    }
    let части: Vec<&str> = хвост.split('+').map(|s| s.trim()).collect();
    let (последняя, модификаторы) = части.split_last().unwrap();
    let Some(основная) = код_клавиши(последняя) else {
        return тф!("ошибка: не знаю клавишу '{}'", "error: unknown key '{}'", последняя);
    };
    let mut коды = Vec::new();
    for м in модификаторы {
        match код_клавиши(м) {
            Some(к) => коды.push(к),
            None => return тф!("ошибка: не знаю модификатор '{}'", "error: unknown modifier '{}'", м),
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

fn нажать_клавишу(state: &mut Parallax, код: u32, нажата: bool) {
    state.process_input_event(smithay::backend::input::InputEvent::Keyboard::<
        crate::synth::Синтетика,
    > {
        event: crate::synth::Клавиша::new(код, нажата),
    });
    state.request_redraw();
}

/// Имя → скан-код evdev. Числом можно задать любой код напрямую.
fn код_клавиши(имя: &str) -> Option<u32> {
    // Имена клавиш в config.lua пишутся как keysym'ы xkb — `Right`, `Return`,
    // `Print`, — и рука сама набирает их в сокет так же. Регистр здесь ничего
    // не значит, а «не знаю клавишу 'Right'» выглядит как поломка бинда.
    let строчными = имя.to_lowercase();
    let имя = строчными.as_str();
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
        // Знаки верхнего ряда и пунктуация: без них с сокета не проверить ни
        // зум камеры (`minus`/`equal`), ни наклон колеса (`comma`/`period`),
        // ни миникарту (`grave`) — а именно на них висят бинды по умолчанию.
        "minus" => 12,
        "equal" => 13,
        "bracketleft" => 26,
        "bracketright" => 27,
        "semicolon" => 39,
        "grave" => 41,
        "comma" => 51,
        "period" => 52,
        "slash" => 53,
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
        // PrtScr: в evdev это KEY_SYSRQ (xkb называет её Print).
        "print" | "sysrq" => 99,
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
/// Состояние куба: рисуется ли он, какие столы на гранях, куда повёрнут, как
/// отмасштабирован и какая грань под курсором.
///
/// Без этой строки проверка куба сводилась к гаданию по снимку: плоский обзор с
/// перспективой миниатюр на кадре был бы похож на куб, стоящий лицом,
/// и «колесо не сработало» неотличимо от «куб вообще не включён» (ленточная
/// раскладка граней не получает — см. `куб_активен`).
/// `wallpaper cycle on|off|toggle` — бесконечные обои плиткой по холсту. Шлёт
/// plx-wall, когда в его меню ставят или снимают галочку «cycle» и когда
/// применяют обои. Без аргументов — нынешнее состояние.
fn обои(хвост: &str, state: &mut Parallax) -> String {
    let слова: Vec<&str> = хвост.split_whitespace().collect();
    match слова.as_slice() {
        [] | ["cycle"] => {}
        ["cycle", "on"] => state.обои_цикл = true,
        ["cycle", "off"] => state.обои_цикл = false,
        ["cycle", "toggle"] => state.обои_цикл = !state.обои_цикл,
        _ => return т!(
            "ошибка: wallpaper [cycle on|off|toggle]",
            "error: wallpaper [cycle on|off|toggle]"
        ).into(),
    }
    state.request_redraw();
    format!("cycle={}", if state.обои_цикл { "on" } else { "off" })
}

fn куб(state: &mut Parallax) -> String {
    let грань = state
        .куб_стол_в_точке(state.pointer_screen_physical())
        .map(|m| format!("{}", m.trailing_zeros() + 1))
        .unwrap_or_else(|| "-".into());
    let передний = state
        .куб_передний_стол()
        .map(|m| format!("{}", m.trailing_zeros() + 1))
        .unwrap_or_else(|| "-".into());
    let столы: Vec<String> = state
        .куб_столы
        .iter()
        .map(|m| format!("{}", m.trailing_zeros() + 1))
        .collect();
    // Что на КАЖДОЙ грани прямо сейчас. Кольцо столов длиннее круга граней, и
    // по одному переднему столу не видно, ту ли пару соседей несёт куб —
    // а именно там и ломается бесконечное вращение.
    let грани: Vec<String> = (0..state.куб_граней())
        .map(|i| {
            state
                .куб_стол_грани(i)
                .map(|m| format!("{}", m.trailing_zeros() + 1))
                .unwrap_or_else(|| "-".into())
        })
        .collect();
    тф!(
        "куб={} стол={} столы=[{}] грани=[{}] угол={:.3} цель={:.3} масштаб={:.2} заполнение={:.2} \
         передний={} под_курсором={} драг={} подъём={:.2} подъём_px={:.0} наклон={:.3} наклон_цель={:.3}",
        "cube={} desktop={} desktops=[{}] faces=[{}] angle={:.3} target={:.3} scale={:.2} fill={:.2} \
         front={} under_cursor={} drag={} lift={:.2} lift_px={:.0} tilt={:.3} tilt_target={:.3}",
        state.куб_активен(),
        // Текущий стол рядом с передней гранью: в покое они обязаны совпадать,
        // и без этой пары «куб показывает не тот стол» ловится только глазами.
        state.viewport.current_tags().trailing_zeros() + 1,
        столы.join(","),
        грани.join(","),
        state.куб_угол,
        state.куб_цель,
        state.куб_масштаб,
        state.куб_заполнение(),
        передний,
        грань,
        state.куб_драг.is_some(),
        // Подъём окон над гранью: доля анимации и во что она выливается для
        // НИЖНЕГО окна стопки. Без пары «доля + пиксели» «окна не поднялись»
        // неотличимо от «подъём выключен ручкой».
        state.куб_подъём,
        state.куб_подъём_пикселей(0, state.screen_size().w as f32),
        state.куб_наклон,
        state.куб_наклон_цель,
    )
}

/// Состояние карусели ленты: этажи, раскрытие, наклон, фронты барабанов и
/// колонка под курсором (этаж + окно). Без этой строки «клик уехал не туда» и
/// «барабан не доехал» различались бы только на глаз.
fn карусель(state: &mut Parallax) -> String {
    if let Some(к) = state.карусель_столов.clone() {
        let столы: Vec<String> = к.столы.iter().map(|m| format!("{}", m.trailing_zeros() + 1)).collect();
        let форма = match crate::карусель_столов::форма(к.столы.len()) {
            crate::карусель_столов::Форма::Квадрат { на_сторону } => format!("square:{на_сторону}"),
            crate::карусель_столов::Форма::Круг => "circle".to_string(),
            crate::карусель_столов::Форма::Книга => "book".to_string(),
        };
        let под = state
            .карусель_столов_под_курсором()
            .map(|m| format!("{}", m.trailing_zeros() + 1))
            .unwrap_or_else(|| "-".into());
        return format!(
            "desk_carousel=yes active={} shape={} desks=[{}] selected={} flat={} turn={:.3}->{:.3} open={:.3}->{:.3} tilt={:.3}->{:.3} drag={} exit={} under_cursor={}",
            state.карусель_активна(),
            форма,
            столы.join(","),
            к.столы.get(к.выбран).map(|m| m.trailing_zeros() + 1).unwrap_or(0),
            к.столы.get(к.плоский).map(|m| m.trailing_zeros() + 1).unwrap_or(0),
            к.поворот, к.поворот_цель,
            к.раскрытие, к.раскрытие_цель,
            к.наклон, к.наклон_цель,
            к.драг.is_some(),
            к.выход.map(|(m, _)| format!("{}", m.trailing_zeros() + 1)).unwrap_or_else(|| "-".into()),
            под,
        );
    }
    let Some(к) = state.карусель.clone() else {
        return тф!("карусель=нет доступна={}", "carousel=none available={}", state.карусель_доступна());
    };
    let этажи: Vec<String> = к.этажи.iter().map(|m| format!("{}", m.trailing_zeros() + 1)).collect();
    let фронты: Vec<String> = к.фронт.iter().map(|f| format!("{:.0}->{:.0}", f.0, f.1)).collect();
    let колонок: Vec<String> = к.этажи.iter().map(|&m| format!("{}", state.карусель_колонки(m).0.len())).collect();
    let под = state
        .карусель_под_курсором()
        .map(|(m, w)| {
            let id = w
                .and_then(|w| state.space.elements().position(|e| e == &w))
                .map(|i| i.to_string())
                .unwrap_or_else(|| "-".into());
            format!("{}:{}", m.trailing_zeros() + 1, id)
        })
        .unwrap_or_else(|| "-".into());
    тф!(
        "карусель=да активна={} этажи=[{}] колонок=[{}] этаж={:.3}->{:.3} раскрытие={:.3}->{:.3} наклон={:.3}->{:.3} фронт=[{}] кадр_x={:.0} драг={} выход={} под_курсором={}",
        "carousel=yes active={} floors=[{}] columns=[{}] floor={:.3}->{:.3} open={:.3}->{:.3} tilt={:.3}->{:.3} front=[{}] frame_x={:.0} drag={} exit={} under_cursor={}",
        state.карусель_активна(),
        этажи.join(","),
        колонок.join(","),
        к.этаж, к.этаж_цель,
        к.раскрытие, к.раскрытие_цель,
        к.наклон, к.наклон_цель,
        фронты.join(","),
        к.кадр_x,
        к.драг.is_some(),
        к.выход.as_ref().map(|(m, _)| format!("{}", m.trailing_zeros() + 1)).unwrap_or_else(|| "-".into()),
        под,
    )
}

fn указатель(state: &mut Parallax) -> String {
    let экран = state.pointer_screen_physical();
    let холст = state.pointer_location;
    let под = state
        .surface_under(холст)
        .map(|(_, точка)| тф!("есть, локально ({:.0},{:.0})", "yes, locally ({:.0},{:.0})", точка.x, точка.y))
        .unwrap_or_else(|| т!("нет", "no").into());
    тф!(
        "экран=({:.1},{:.1}) холст=({:.1},{:.1}) камера=({:.1},{:.1}) зум={:.2} \
         поверхность={} панель={:?} миникарта_ручная={}",
        "screen=({:.1},{:.1}) canvas=({:.1},{:.1}) camera=({:.1},{:.1}) zoom={:.2} \
         surface={} bar={:?} minimap_manual={}",
        экран.x, экран.y, холст.x, холст.y,
        state.viewport.cam_x, state.viewport.cam_y, state.viewport.zoom,
        под, state.bar_hover, state.minimap_manual,
    )
}

/// `icon [list | add | rm | move | desktop]` — иконки рабочего стола из
/// терминала.
///
/// Тот же список, что правится мышью (см. рабстол.rs), — команда нужна там, где
/// мышь неуместна: скрипт установки, раскладка ярлыков одним махом, проверка на
/// харнессе. Без `x`/`y` иконка ложится в середину видимой части холста — туда,
/// куда человек сейчас смотрит.
fn иконка(хвост: &str, state: &mut Parallax) -> String {
    let (что, довод) = match хвост.split_once(char::is_whitespace) {
        Some((первое, остаток)) => (первое, остаток.trim()),
        None => (хвост, ""),
    };
    match что {
        "" | "list" | "список" => {
            if state.рабстол.иконки.is_empty() {
                return т!("иконок нет", "no icons").into();
            }
            state.рабстол.иконки.iter()
                .map(|и| format!(
                    "{} {},{} {}px desktop={} image={} cmd={}\n",
                    и.имя, и.x, и.y, и.размер,
                    crate::рабстол::столы_текстом(и.столы),
                    if и.картинка.is_empty() { "—" } else { &и.картинка },
                    if и.команда.is_empty() { "—" } else { &и.команда },
                ))
                .collect()
        }
        "add" | "добавить" => {
            let mut части = довод.split_whitespace();
            let Some(имя) = части.next().map(|s| s.to_string()).filter(|s| !s.is_empty()) else {
                return т!("ошибка: icon add имя [ключ=значение ...]",
                          "error: icon add name [key=value ...]").into();
            };
            let mut иконка = crate::рабстол::Иконка {
                имя,
                картинка: String::new(),
                команда: String::new(),
                x: 0,
                y: 0,
                размер: 64,
                // Без `desktop=` иконка достаётся столу, на котором стоим:
                // команда ставит её туда же, куда поставила бы рука.
                столы: state.viewport.current_tags(),
            };
            let mut место_задано = false;
            for кусок in части {
                let Some((ключ, значение)) = кусок.split_once('=') else { continue };
                match ключ {
                    "cmd" | "команда" => иконка.команда = значение.to_string(),
                    "image" | "icon" | "картинка" => иконка.картинка = значение.to_string(),
                    "x" => {
                        иконка.x = значение.parse().unwrap_or(0);
                        место_задано = true;
                    }
                    "y" => {
                        иконка.y = значение.parse().unwrap_or(0);
                        место_задано = true;
                    }
                    "size" | "размер" => {
                        иконка.размер = значение.parse::<i32>().unwrap_or(64)
                            .clamp(crate::рабстол::МИН_РАЗМЕР, crate::рабстол::МАКС_РАЗМЕР);
                    }
                    // `desktop=3` — только на третьем столе, `desktop=all` (или
                    // 0) — на всех.
                    "desktop" | "стол" => {
                        иконка.столы = match значение {
                            "all" | "все" | "*" => 0,
                            v => crate::рабстол::маска_стола(v.parse::<u32>().unwrap_or(1)),
                        };
                    }
                    _ => {}
                }
            }
            if !место_задано {
                // Середина видимой части холста: «куда человек смотрит».
                let vis = state.visible_canvas_size();
                иконка.x = (state.viewport.cam_x + vis.w / 2.0).round() as i32 - иконка.размер / 2;
                иконка.y = (state.viewport.cam_y + vis.h / 2.0).round() as i32 - иконка.размер / 2;
            }
            let (имя, x, y) = (иконка.имя.clone(), иконка.x, иконка.y);
            state.рабстол_добавить(иконка);
            тф!("ok: {} {},{}", "ok: {} {},{}", имя, x, y)
        }
        "rm" | "remove" | "удалить" => {
            let Some(i) = state.рабстол.иконки.iter().position(|и| и.имя == довод) else {
                return тф!("ошибка: иконки '{}' нет", "error: no icon '{}'", довод);
            };
            state.рабстол.иконки.remove(i);
            state.рабстол_сохранить();
            state.request_redraw();
            тф!("ok: {} удалена", "ok: {} removed", довод)
        }
        "move" | "двинуть" => {
            let mut части = довод.split_whitespace();
            let (Some(имя), Some(x), Some(y)) = (
                части.next().map(|s| s.to_string()),
                части.next().and_then(|s| s.parse::<i32>().ok()),
                части.next().and_then(|s| s.parse::<i32>().ok()),
            ) else {
                return т!("ошибка: icon move имя X Y", "error: icon move name X Y").into();
            };
            let Some(и) = state.рабстол.иконки.iter_mut().find(|и| и.имя == имя) else {
                return тф!("ошибка: иконки '{}' нет", "error: no icon '{}'", имя);
            };
            и.x = x;
            и.y = y;
            state.рабстол_сохранить();
            state.request_redraw();
            тф!("ok: {} {},{}", "ok: {} {},{}", имя, x, y)
        }
        // `icon desktop имя N` — перенести иконку на другой стол (0 — на все).
        "desktop" | "стол" => {
            let mut части = довод.split_whitespace();
            let (Some(имя), Some(номер)) = (
                части.next().map(|s| s.to_string()),
                части.next().map(|s| match s {
                    "all" | "все" | "*" => 0,
                    v => v.parse::<u32>().unwrap_or(1),
                }),
            ) else {
                return т!("ошибка: icon desktop имя N|all", "error: icon desktop name N|all").into();
            };
            let Some(и) = state.рабстол.иконки.iter_mut().find(|и| и.имя == имя) else {
                return тф!("ошибка: иконки '{}' нет", "error: no icon '{}'", имя);
            };
            и.столы = crate::рабстол::маска_стола(номер);
            let текст = crate::рабстол::столы_текстом(и.столы);
            state.рабстол_сохранить();
            state.request_redraw();
            тф!("ok: {} на столе {}", "ok: {} on desktop {}", имя, текст)
        }
        иное => тф!(
            "ошибка: icon [list | add | rm | move | desktop], а не '{иное}'",
            "error: icon [list | add | rm | move | desktop], not '{иное}'"
        ),
    }
}

/// `desk [list | add РОД [x=…] [y=…] [scale=…] [text=…] | rm N | move N X Y]` —
/// встроенные виджеты рабочего стола (см. виджеты_стола.rs). Тот же список, что
/// ставится мышью по ПКМ; номер — позиция в `desk list`.
fn стол_виджеты(хвост: &str, state: &mut Parallax) -> String {
    use crate::виджеты_стола::{Род, Виджет, МИН_МАСШТАБ, МАКС_МАСШТАБ};
    let (что, довод) = match хвост.split_once(char::is_whitespace) {
        Some((первое, остаток)) => (первое, остаток.trim()),
        None => (хвост, ""),
    };
    let номер = |s: Option<&str>, state: &Parallax| -> Option<usize> {
        s?.parse::<usize>().ok().filter(|i| *i < state.рабстол.виджеты.список.len())
    };
    match что {
        "" | "list" => {
            if state.рабстол.виджеты.список.is_empty() {
                return т!("виджетов нет", "no widgets").into();
            }
            state.рабстол.виджеты.список.iter().enumerate()
                .map(|(i, в)| {
                    let р = в.рамка();
                    format!(
                        "{} {} {},{} {}x{} {}% desktop={}\n",
                        i, в.род.ключ(), в.x, в.y, р.size.w, р.size.h, в.масштаб,
                        crate::рабстол::столы_текстом(в.столы),
                    )
                })
                .collect()
        }
        "add" => {
            let mut части = довод.split_whitespace();
            let Some(род) = части.next().and_then(Род::из_ключа) else {
                return т!("ошибка: desk add clock|calendar|system|note [x=…] [y=…] [scale=…] [text=…]",
                          "error: desk add clock|calendar|system|note [x=…] [y=…] [scale=…] [text=…]").into();
            };
            let vis = state.visible_canvas_size();
            let mut в = Виджет::новый(род, 0, 0);
            в.столы = state.viewport.current_tags();
            let (mut x, mut y) = (None, None);
            for кусок in части {
                let Some((ключ, значение)) = кусок.split_once('=') else { continue };
                match ключ {
                    "x" => x = значение.parse().ok(),
                    "y" => y = значение.parse().ok(),
                    "scale" => в.масштаб = значение.parse::<i32>().unwrap_or(100).clamp(МИН_МАСШТАБ, МАКС_МАСШТАБ),
                    // Пробелы в заметке — подчёркиванием: строку команды режем по пробелам.
                    "text" => в.текст = значение.replace('_', " "),
                    "desktop" | "стол" => {
                        в.столы = match значение {
                            "all" | "все" | "*" => 0,
                            v => crate::рабстол::маска_стола(v.parse::<u32>().unwrap_or(1)),
                        };
                    }
                    _ => {}
                }
            }
            let р = в.рамка();
            в.x = x.unwrap_or((state.viewport.cam_x + vis.w / 2.0).round() as i32 - р.size.w / 2);
            в.y = y.unwrap_or((state.viewport.cam_y + vis.h / 2.0).round() as i32 - р.size.h / 2);
            let (xx, yy) = (в.x, в.y);
            state.виджет_добавить(в);
            тф!("ok: {} {},{}", "ok: {} {},{}", род.ключ(), xx, yy)
        }
        "rm" => {
            let Some(i) = номер(довод.split_whitespace().next(), state) else {
                return т!("ошибка: desk rm N", "error: desk rm N").into();
            };
            state.рабстол.виджеты.список.remove(i);
            state.виджеты_сохранить();
            state.request_redraw();
            "ok".into()
        }
        "move" => {
            let mut части = довод.split_whitespace();
            let i = номер(части.next(), state);
            let x = части.next().and_then(|s| s.parse::<i32>().ok());
            let y = части.next().and_then(|s| s.parse::<i32>().ok());
            let (Some(i), Some(x), Some(y)) = (i, x, y) else {
                return т!("ошибка: desk move N X Y", "error: desk move N X Y").into();
            };
            let в = &mut state.рабстол.виджеты.список[i];
            в.x = x;
            в.y = y;
            state.виджеты_сохранить();
            state.request_redraw();
            "ok".into()
        }
        // `desk desktop N СТОЛ` — перенести плашку на другой стол (0 — на все).
        "desktop" | "стол" => {
            let mut части = довод.split_whitespace();
            let i = номер(части.next(), state);
            let стол = части.next().map(|s| match s {
                "all" | "все" | "*" => 0,
                v => v.parse::<u32>().unwrap_or(1),
            });
            let (Some(i), Some(стол)) = (i, стол) else {
                return т!("ошибка: desk desktop N СТОЛ|all", "error: desk desktop N DESKTOP|all").into();
            };
            state.рабстол.виджеты.список[i].столы = crate::рабстол::маска_стола(стол);
            state.виджеты_сохранить();
            state.request_redraw();
            "ok".into()
        }
        иное => тф!(
            "ошибка: desk [list | add | rm | move | desktop], а не '{иное}'",
            "error: desk [list | add | rm | move | desktop], not '{иное}'"
        ),
    }
}

/// `widget [list | on|off [app_id] | move X Y [app_id]]` — виджеты на лету.
///
/// Правило `window{ widget = true }` срабатывает при появлении окна, и
/// подбирать виджету место, перезапуская его после каждой правки конфига, —
/// та самая работа, ради которой команда и написана. Без `app_id` берётся
/// окно в фокусе; с `app_id` — первое подходящее (тот же разбор шаблона со
/// звёздочкой, что в правилах, см. config::ПравилоОкна).
/// Бросок картинки на стол без живого источника DnD: `picture drop X Y
/// <uri|путь>` (экранные пиксели). Данные идут тем же разбором, что и
/// настоящий бросок `text/uri-list` (см. картинка.rs), — проверяется всё,
/// кроме самого разговора с источником.
fn картинка(хвост: &str, state: &mut Parallax) -> String {
    let mut части = хвост.splitn(4, char::is_whitespace);
    let (Some("drop"), Some(x), Some(y), Some(что)) = (части.next(), части.next(), части.next(), части.next())
    else {
        return т!("ошибка: picture drop X Y <uri|путь>", "error: picture drop X Y <uri|path>").into();
    };
    let (Ok(x), Ok(y)) = (x.parse::<f64>(), y.parse::<f64>()) else {
        return т!("ошибка: X и Y — числа", "error: X and Y must be numbers").into();
    };
    let zoom = state.viewport.zoom.max(0.01);
    let точка = smithay::utils::Point::from((
        state.viewport.cam_x + x / zoom,
        state.viewport.cam_y + y / zoom,
    ));
    state.картинка_брошена_текстом(что.trim(), точка);
    "ok".into()
}

fn виджет(хвост: &str, state: &mut Parallax) -> String {
    let (что, довод) = match хвост.split_once(char::is_whitespace) {
        Some((первое, остаток)) => (первое, остаток.trim()),
        None => (хвост, ""),
    };
    match что {
        "" | "list" | "список" => {
            let mut out = String::new();
            for tw in state.tagged_windows.iter().filter(|tw| tw.виджет) {
                let место = state.space.element_location(&tw.window)
                    .map(|l| format!("{},{}", l.x, l.y))
                    .unwrap_or_else(|| "—".into());
                let размер = tw.window.geometry().size;
                out.push_str(&format!(
                    "{} {} {}x{}\n",
                    crate::xwin::app_id(&tw.window).unwrap_or_else(|| "—".into()),
                    место, размер.w, размер.h,
                ));
            }
            if out.is_empty() {
                return т!("виджетов нет", "no widgets").into();
            }
            out
        }
        "on" | "off" | "вкл" | "выкл" => {
            let включить = matches!(что, "on" | "вкл");
            let Some(окно) = окно_по_имени(state, довод) else {
                return if довод.is_empty() {
                    т!("ошибка: нет окна в фокусе — укажите app_id",
                       "error: no focused window — name an app_id").into()
                } else {
                    тф!("ошибка: окна '{}' нет", "error: no window '{}'", довод)
                };
            };
            state.виджет_пометить(&окно, включить);
            тф!("ok: {} widget={}", "ok: {} widget={}",
                crate::xwin::app_id(&окно).unwrap_or_default(), включить)
        }
        "move" | "двинуть" => {
            let mut части = довод.split_whitespace();
            let (Some(x), Some(y)) = (
                части.next().and_then(|s| s.parse::<i32>().ok()),
                части.next().and_then(|s| s.parse::<i32>().ok()),
            ) else {
                return т!("ошибка: widget move X Y [app_id]", "error: widget move X Y [app_id]").into();
            };
            let имя = части.next().unwrap_or("");
            let Some(окно) = окно_по_имени(state, имя) else {
                return if имя.is_empty() {
                    т!("ошибка: нет окна в фокусе — укажите app_id",
                       "error: no focused window — name an app_id").into()
                } else {
                    тф!("ошибка: окна '{}' нет", "error: no window '{}'", имя)
                };
            };
            if !state.виджет_передвинуть(&окно, x, y) {
                return тф!(
                    "ошибка: '{}' — не виджет; пометьте его widget on",
                    "error: ‘{}’ is not a widget; mark it with widget on",
                    crate::xwin::app_id(&окно).unwrap_or_default()
                );
            }
            тф!("ok: {},{}", "ok: {},{}", x, y)
        }
        иное => тф!(
            "ошибка: widget [list | on|off [app_id] | move X Y [app_id]], а не '{иное}'",
            "error: widget [list | on|off [app_id] | move X Y [app_id]], not '{иное}'"
        ),
    }
}

/// Окно по `app_id` (шаблон со звёздочкой допускается) либо, если имя пустое,
/// окно в фокусе.
fn окно_по_имени(state: &Parallax, имя: &str) -> Option<smithay::desktop::Window> {
    if имя.is_empty() {
        return state.focused_window();
    }
    state.tagged_windows.iter()
        .map(|tw| tw.window.clone())
        .find(|w| {
            let app = crate::xwin::app_id(w).unwrap_or_default();
            let title = crate::xwin::title(w).unwrap_or_default();
            crate::config::ПравилоОкна {
                app_id: Some(имя.to_string()),
                ..Default::default()
            }
            .подходит(&app, &title)
                || title.to_lowercase().contains(&имя.to_lowercase())
        })
}

/// Список окон в том виде, в каком его надо мерить: что попросили у клиента и
/// что клиент реально нарисовал — расхождение этих двух чисел и есть «окно не
/// ужимается» (см. udev.rs, поиск "нужен_кроп").
fn окна(state: &Parallax) -> String {
    use smithay::desktop::WindowSurface;
    let mut out = String::new();
    let фокус = state.focused_surface();
    for (i, window) in state.space.elements().enumerate() {
        let geo = state.space.element_geometry(window)
            .map(|g| format!("{},{} {}x{}", g.loc.x, g.loc.y, g.size.w, g.size.h))
            .unwrap_or_else(|| т!("нет", "no").into());
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
        out.push_str(&тф!(
            "{}{} {} app={} стол={} монитор={} слот={} своя={}x{} запрошено={} буфер={}\n", "{}{} {} app={} workspace={} monitor={} slot={} own={}x{} asked={} buffer={}\n",
            активно, i, класс,
            crate::xwin::app_id(window).unwrap_or_else(|| "—".into()),
            стол, монитор,
            geo, свой.w, свой.h, запрошено, буфер,
        ));
    }
    if out.is_empty() {
        out.push_str(т!("окон нет", "no windows"));
    }
    out.trim_end().to_string()
}
