//! Parallax — оконный менеджер Wayland на бесконечном холсте.
//!
//! Вся программа живёт здесь, в библиотеке; бинари в `bins/` — это по пять
//! строк вокруг [`run`]. Отличаются они только НАБОРОМ ФИЧ:
//!
//! * `plx-standard` — композитор, тайлинг, лента, обзор, обои, панель, трей,
//!   блютуз, вайфай, звук, портал, снимок, X11, жесты;
//! * `plx-extra` — то же плюс шлем (`vr`), окна в Minecraft (`mine`) и
//!   мультиюзер (`share`).
//!
//! Выключенная фича не обкладывает места вызова `#[cfg]`: вместо модуля
//! подставляется ЗАГЛУШКА с тем же внешним видом (`*_stub/`), а `#[path]`
//! ниже — единственное место, где эта подмена происходит. Поэтому `state.rs`,
//! `input.rs` и `udev.rs` про сборку ничего не знают и читаются одинаково.

// Имена в этом крейте русские, и однобуквенные локальные переменные неизбежно
// сталкиваются с латинскими из соседних файлов: `с` и `c`, `р` и `p`, `г` и
// `r`, `о` и `o`. Удовлетворить линт нечем: он ругается на ОДНУ пару за раз,
// и правка одного места просто всплывает в следующем. Многобуквенные имена мы
// и так предпочитаем (см. `рад`, `глуб`, `статус`), а на этом уровне лучше
// сказать прямо: в русском коде гомоглифы — норма, а не опечатка.
#![allow(confusable_idents)]

mod anim;
mod bar;
mod blur;
mod bluetooth;
mod canvas;
mod close;
mod capture;
mod columns;
mod constellation;
mod config;
mod ctl;
mod decor;
mod dwindle;
mod focus;
mod fullscreen;
mod gestures;
/// Мышиные аккорды: команда парой кнопок (модель hevel).
#[path = "аккорды.rs"]
mod аккорды;
mod grabs;
mod handlers;
mod headless;
mod icons;
mod input;
mod lang;
#[cfg(feature = "mine")]
mod mine;
#[cfg(not(feature = "mine"))]
#[path = "mine_stub/mod.rs"]
mod mine;
mod mode;
mod monitors;
mod notify;
/// Палитра обоев: цвета, которыми красятся эффекты рабочего стола.
#[cfg(feature = "shaders")]
#[path = "обои.rs"]
mod обои;
#[cfg(not(feature = "shaders"))]
#[path = "шейдеры_stub/обои.rs"]
mod обои;
/// Куб рабочих столов (Compiz Desktop Cube).
#[cfg(feature = "shaders")]
#[path = "куб.rs"]
mod куб;
#[cfg(not(feature = "shaders"))]
#[path = "шейдеры_stub/куб.rs"]
mod куб;
/// Свет на холсте в цвет обоев: заливка сцены и свет на окнах.
#[cfg(feature = "shaders")]
#[path = "свет.rs"]
mod свет;
#[cfg(not(feature = "shaders"))]
#[path = "шейдеры_stub/свет.rs"]
mod свет;
mod overview;
mod portal;
mod portal_stream;
mod rounded;
mod screencopy;
mod selection;
mod session;
#[cfg(feature = "share")]
mod share;
#[cfg(not(feature = "share"))]
#[path = "share_stub/mod.rs"]
mod share;
mod snip;
mod sni;
mod state;
mod switcher;
mod synth;
mod text;
mod udev;
#[cfg(feature = "vr")]
mod vr;
#[cfg(not(feature = "vr"))]
#[path = "vr_stub/mod.rs"]
mod vr;
mod audio;
mod tiling;
mod touchpad;
/// Сенсорный экран ноутбука: касание клиенту, органам компоновщика и камере.
#[path = "сенсор.rs"]
mod сенсор;
mod tray;
mod wifi;
mod winit;
mod xwayland;
mod xwin;

use smithay::reexports::{
    calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    wayland_server::Display,
};
pub use state::Parallax;

/// Перелить переменные сессии в окружение D-Bus-активации и systemd --user.
///
/// Всё, что запускается не нами, а шиной (портал, его бэкенд, pipewire-клиенты),
/// наследует окружение НЕ от parallax, а от systemd --user, который стартовал при
/// логине и про наш wayland-сокет ничего не знает. Отсюда классический симптом:
/// портал есть, а «поделиться экраном» отдаёт чёрный кадр или сразу ошибку.
///
/// Зовётся дважды: сразу после подъёма бэкенда (WAYLAND_DISPLAY уже известен) и
/// после старта Xwayland (появляется DISPLAY). Отсутствие
/// dbus-update-activation-environment не фатально — просто предупреждение.
/// Взводится в main по `--headless`. Смотрит `export_session_env`: харнесс не
/// имеет права трогать окружение D-Bus живого сеанса — иначе портал, pipewire и
/// всё, что поднимается активацией, поедут на ЕГО сокет вместо настоящего.
pub static HEADLESS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn headless() -> bool {
    HEADLESS.load(std::sync::atomic::Ordering::Relaxed)
}

pub fn export_session_env() {
    if headless() {
        tracing::info!("plx: headless — NOT exporting the session environment to D-Bus");
        return;
    }
    const VARS: [&str; 4] = [
        "WAYLAND_DISPLAY",
        "DISPLAY",
        "XDG_CURRENT_DESKTOP",
        "XDG_SESSION_TYPE",
    ];
    let present: Vec<&str> = VARS.iter().copied()
        .filter(|v| std::env::var_os(v).is_some())
        .collect();
    if present.is_empty() {
        return;
    }
    match std::process::Command::new("dbus-update-activation-environment")
        .arg("--systemd")
        .args(&present)
        .status()
    {
        Ok(st) if st.success() => {
            tracing::info!("plx: session environment exported to D-Bus: {:?}", present);
        }
        Ok(st) => {
            tracing::warn!("plx: dbus-update-activation-environment returned {}", st);
        }
        Err(e) => {
            tracing::warn!("plx: dbus-update-activation-environment did not start: {}", e);
        }
    }
}

/// Поднять мягкий лимит открытых файлов до жёсткого.
///
/// Композитору дескрипторы — расходный материал: каждый буфер клиента приезжает
/// набором dmabuf-дескрипторов и живёт, пока клиент его не уничтожит. Замер
/// 24.08.2026 в живом сеансе: 847 дескрипторов при мягком лимите 1024, из них
/// 741 dmabuf от живых обоев (утечка чинилась отдельно, в plx-wall). Упереться в
/// лимит — это не «стало тесно», а отказ всего подряд разом: в логе за тот день
/// «plx/audio: pactl не запустился: Too many open files», провалы
/// `eglCreateImageKHR` («could not bind to DMA buffer») и `plx/dmabuf: import
/// failed`, то есть окна перестают показывать содержимое.
///
/// Жёсткий лимит здесь 4096 и достаётся даром: это ровно то, что делают sway и
/// mutter при старте. Дырявых клиентов это не лечит, но даёт запас в четыре
/// раза, а вместе с ним — время увидеть проблему в логе, а не по мёртвому
/// экрану.
fn поднять_лимит_дескрипторов() {
    unsafe {
        let mut лимит: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut лимит) != 0 {
            tracing::warn!("plx: cannot read RLIMIT_NOFILE: {}",
                std::io::Error::last_os_error());
            return;
        }
        if лимит.rlim_cur >= лимит.rlim_max {
            return;
        }
        let было = лимит.rlim_cur;
        лимит.rlim_cur = лимит.rlim_max;
        if libc::setrlimit(libc::RLIMIT_NOFILE, &лимит) != 0 {
            tracing::warn!("plx: cannot raise RLIMIT_NOFILE: {}",
                std::io::Error::last_os_error());
            return;
        }
        tracing::info!("plx: file descriptor limit {} → {}", было, лимит.rlim_max);
    }
}

/// Набор фич, с которым собран ЭТОТ бинарь. Не украшение: разница между
/// `plx-standard` и `plx-extra` задаётся только фичами, и по одному имени файла
/// её не видно — бинарь можно переименовать, скопировать, собрать самому.
/// В отчёте об ошибке это первое, что нужно знать.
fn собранные_фичи() -> Vec<&'static str> {
    let mut список = Vec::new();
    if cfg!(feature = "vr") {
        список.push("vr");
    }
    if cfg!(feature = "mine") {
        список.push("mine");
    }
    if cfg!(feature = "share") {
        список.push("share");
    }
    список
}

/// `--version` и `--help`. Возвращает `true`, если ответ напечатан и запускаться
/// не надо.
///
/// Текст ЗДЕСЬ ВСЕГДА АНГЛИЙСКИЙ, в отличие от всего остального, что видит
/// человек. Причина та же, по которой английские логи: этот вывод просят
/// вставить в отчёт об ошибке (см. `.github/ISSUE_TEMPLATE/bug_report.yml`),
/// и читать его будет не только автор. Да и переводить нечем — конфигурация с
/// `set{ lang }` к этому моменту ещё не прочитана.
fn справка_или_версия() -> bool {
    let имя = std::env::args()
        .next()
        .and_then(|путь| {
            std::path::Path::new(&путь)
                .file_name()
                .map(|о| о.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "parallax".into());
    let версия = env!("CARGO_PKG_VERSION");
    // Хеш подставляет build.rs; в сборке из архива без .git его нет.
    let коммит = option_env!("PLX_COMMIT").unwrap_or("unknown commit");
    let фичи = собранные_фичи();
    let фичи = if фичи.is_empty() {
        "none".to_string()
    } else {
        фичи.join(", ")
    };

    let ключи: Vec<String> = std::env::args().skip(1).collect();

    if ключи.iter().any(|a| a == "--version" || a == "-V") {
        println!("{имя} {версия} ({коммит})");
        println!("features: {фичи}");
        return true;
    }

    if ключи.iter().any(|a| a == "--help" || a == "-h") {
        println!("{имя} {версия} ({коммит}) — features: {фичи}");
        println!();
        println!("A Wayland compositor on an infinite canvas.");
        println!();
        println!("Usage: {имя} [OPTIONS]");
        println!();
        println!("Options:");
        println!("      --tty        run on TTY/DRM (the default when not nested)");
        println!("      --winit      run nested inside an existing session, for development");
        println!("      --headless   no output and no input; frames are taken through");
        println!("                   the control socket (see harness.sh)");
        // Ключ есть только там, где есть сам шлем: в plx-standard `--vr` ответил
        // бы «этой сборки не касается», и в справке ему делать нечего.
        if cfg!(feature = "vr") {
            println!("      --vr         put on the headset at startup (needs an OpenXR runtime)");
        }
        println!("  -V, --version    print version and exit");
        println!("  -h, --help       print this and exit");
        println!();
        println!("Configuration: ~/.config/parallax/config.lua");
        println!("  every knob is documented in default_config.lua, next to the line");
        println!("  that sets it; reload a running compositor with Super+Shift+C.");
        println!();
        println!("Environment:");
        println!("  RUST_LOG         log filter (e.g. RUST_LOG=parallax=debug,info)");
        println!("  PLX_CONFIG       path to config.lua, overriding the default");
        println!();
        println!("https://github.com/YoungEscapist/parallax-wm");
        return true;
    }

    false
}

/// Точка входа композитора. Оба бинаря (`plx-standard` и `plx-extra`) — это
/// пять строк вокруг неё; вся разница между сборками задана НАБОРОМ ФИЧ,
/// с которым каждый из них тянет эту библиотеку (см. bins/ и `[features]`).
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    // ДО инициализации логов: иначе `--version` в отчёте об ошибке приезжал бы
    // вперемешку со строками tracing, а его именно копируют и вставляют.
    if справка_или_версия() {
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!("parallax starting");
    поднять_лимит_дескрипторов();

    // Явный 'static: LoopHandle с этим временем жизни нужен X11Wm::start_wm,
    // который сам вешает в цикл источник X11-событий (см. xwayland.rs).
    let mut event_loop: EventLoop<'static, Parallax> = EventLoop::try_new()?;
    let display: Display<Parallax> = Display::new()?;
    let mut state = Parallax::new(&mut event_loop, display);

    let force_tty   = std::env::args().any(|a| a == "--tty");
    let force_winit = std::env::args().any(|a| a == "--winit");
    // Headless: ни экрана, ни ввода — кадр собирается тем же кодом, что и для
    // монитора, и уходит в PNG по команде управляющего сокета (см. headless.rs).
    // Это единственный режим, в котором рендер DRM-пути можно смотреть, не
    // занимая монитор живого сеанса и не деля с ним клавиатуру.
    let headless   = std::env::args().any(|a| a == "--headless");
    HEADLESS.store(headless, std::sync::atomic::Ordering::Relaxed);
    // Не доверяем DISPLAY/WAYLAND_DISPLAY — могут быть унаследованы от родителя
    let tty_mode = !headless && (force_tty || !force_winit);

    tracing::info!("plx: mode={} (force_tty={} force_winit={} headless={})",
        if headless { "Headless" } else if tty_mode { "TTY" } else { "Winit" },
        force_tty, force_winit, headless);

    if headless {
        crate::headless::init_headless(&mut event_loop, &mut state)?;
        unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
    } else if tty_mode {
        tracing::info!("plx: trying TTY/DRM backend...");
        match crate::udev::init_udev(&mut event_loop, &mut state) {
            Ok(()) if !state.udev_devices.is_empty() => {
                tracing::info!("plx: TTY/DRM backend OK");
                unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
            }
            Ok(()) => {
                tracing::warn!("plx: no DRM devices found, falling back to Winit");
                crate::winit::init_winit(&mut event_loop, &mut state)?;
                unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
            }
            Err(e) => {
                tracing::warn!("plx: DRM failed ({}), falling back to Winit", e);
                crate::winit::init_winit(&mut event_loop, &mut state)?;
                unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
            }
        }
    } else {
        tracing::info!("plx: Winit backend");
        crate::winit::init_winit(&mut event_loop, &mut state)?;
        unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
    }

    tracing::info!("parallax socket: {:?}", state.socket_name);

    // Управляющий сокет: в харнессе всегда, в живом сеансе — только по
    // PLX_CTL=1 (см. ctl.rs, там же почему не по умолчанию).
    if headless || std::env::var_os("PLX_CTL").is_some() {
        if let Err(e) = crate::ctl::init(&event_loop.handle(), &state) {
            tracing::warn!("plx/ctl: socket failed to start: {}", e);
        }
    }

    // Отдаём окружение сессии в D-Bus/systemd --user. Без этого
    // xdg-desktop-portal (его поднимает не parallax, а D-Bus-активация) не видит ни
    // WAYLAND_DISPLAY, ни XDG_CURRENT_DESKTOP: подключиться к нам он не может,
    // а бэкенд выбирает по «рабочему столу», которого не знает. Для
    // демонстрации экрана в Discord это обязательный шаг — см. screencopy.rs.
    export_session_env();

    // ── Портал (демонстрация экрана) ─────────────────────────────────────────
    // Бэкенд живёт в своём потоке на сессионной шине, а выбор источника делает
    // сам композитор — запросы приходят сюда каналом calloop. См. portal.rs.
    // Всё, что дальше садится на D-Bus (портал, BlueZ-агент, трей, вайфай,
    // звук), в харнессе НЕ поднимаем: имена на шине одни на всю сессию, и
    // второй экземпляр их бы отобрал у живого parallax — демонстрация экрана,
    // сопряжение и трей поехали бы к нему. Проверять рендер это не мешает.
    if !headless { crate::portal::install_portal_files(); }
    {
        use smithay::reexports::calloop::channel;
        let (to_plx, from_portal) = channel::channel::<crate::portal::Request>();
        if !headless && crate::portal::spawn(to_plx) {
            event_loop.handle().insert_source(from_portal, |event, _, state| {
                if let channel::Event::Msg(request) = event {
                    state.handle_portal_request(request);
                }
            })?;
        }
    }

    // ── Блютуз ───────────────────────────────────────────────────────────────
    // BlueZ живёт на СИСТЕМНОЙ шине, поэтому поток поднимается независимо от
    // сессионной (её может и не быть): меню устройств и автоподключение
    // работают даже в сессии, запущенной без dbus-run-session. См. bluetooth.rs.
    {
        use smithay::reexports::calloop::channel;
        let (to_plx, from_bt) = channel::channel::<crate::bluetooth::Event>();
        if let Some(tx) = (!headless).then(|| crate::bluetooth::spawn(to_plx)).flatten() {
            let autoconnect = state.lua_config.bluetooth_autoconnect;
            state.init_bluetooth(tx, autoconnect);
            event_loop.handle().insert_source(from_bt, |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.handle_bluetooth_event(event);
                }
            })?;
        }
    }

    // ── Полка состояния ──────────────────────────────────────────────────────
    // Вайфай (NetworkManager), звук (wpctl) и батарея (/sys) — см. tray.rs.
    // Поток спит, пока полку не открыли, поэтому поднимаем его всегда.
    //
    // В харнессе полка по умолчанию выключена вместе с остальной шиной (см.
    // выше), но без неё нечего и проверять: `tray_toggle` сразу выходит на
    // `self.tray = None`, то есть ни выезда, ни блюра под полкой в кадре не
    // появится. `PLX_HEADLESS_SERVICES=1` включает её ЯВНО — и только для
    // случая, когда харнесс запущен на СВОЕЙ шине (`dbus-run-session`):
    // на общей он отобрал бы `org.kde.StatusNotifierWatcher` у живого parallax,
    // и значки трея уехали бы в невидимый экземпляр.
    let сервисы_харнесса = std::env::var_os("PLX_HEADLESS_SERVICES").is_some();
    let полка_и_меню = !headless || сервисы_харнесса;
    {
        use smithay::reexports::calloop::channel;
        let (to_plx, from_tray) = channel::channel::<crate::tray::Event>();
        if let Some(tx) = полка_и_меню.then(|| crate::tray::spawn(to_plx)).flatten() {
            state.init_tray(tx);
            event_loop.handle().insert_source(from_tray, |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.handle_tray_event(event);
                }
            })?;
        }
    }

    // ── Вайфай и звук ────────────────────────────────────────────────────────
    // Оба спят, пока не открыта полка или их меню (см. Cmd::Watch).
    {
        use smithay::reexports::calloop::channel;
        let (to_plx, from_wifi) = channel::channel::<crate::wifi::Event>();
        if let Some(tx) = полка_и_меню.then(|| crate::wifi::spawn(to_plx)).flatten() {
            state.init_wifi(tx);
            event_loop.handle().insert_source(from_wifi, |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.handle_wifi_event(event);
                }
            })?;
        }
        let (to_plx, from_audio) = channel::channel::<crate::audio::Event>();
        if let Some(tx) = полка_и_меню.then(|| crate::audio::spawn(to_plx)).flatten() {
            state.init_audio(tx);
            event_loop.handle().insert_source(from_audio, |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.handle_audio_event(event);
                }
            })?;
        }
    }

    // ── Трей приложений (StatusNotifierItem) ─────────────────────────────────
    // Поднимать надо ПРИ СТАРТЕ и безусловно: приложения ищут реестр
    // (org.kde.StatusNotifierWatcher) в момент своего запуска, и если его нет,
    // многие уходят в старый XEmbed-трей и больше не возвращаются (см. sni.rs).
    {
        use smithay::reexports::calloop::channel;
        let (to_plx, from_sni) = channel::channel::<crate::sni::Event>();
        if let Some(tx) = (!headless).then(|| crate::sni::spawn(to_plx)).flatten() {
            state.init_sni(tx);
            event_loop.handle().insert_source(from_sni, |event, _, state| {
                if let channel::Event::Msg(event) = event {
                    state.handle_sni_event(event);
                }
            })?;
        }
    }

    // ── Звук уведомлений ─────────────────────────────────────────────────────
    // Свой поток на сессионной шине, в режиме монитора (см. notify.rs). В
    // харнессе — только вместе с остальными службами: на общей шине лишний
    // монитор безвреден, но звучал бы он на уведомления ЖИВОГО сеанса.
    if полка_и_меню {
        crate::notify::поднять(crate::notify::Настройки {
            файл: state.lua_config.notify_sound.clone(),
            громкость: state.lua_config.notify_volume,
        });
    }

    // Раскладка в панели: до первого нажатия её никто не пересчитает, а
    // показать «EN» надо с первого кадра.
    state.refresh_kb_layout();

    // XWayland поднимаем ПОСЛЕ бэкенда: DISPLAY выставится по готовности
    // сервера, и всё, что мы спавним дальше, увидит уже рабочий X11.
    crate::xwayland::start(&mut event_loop, &mut state);

    // Анимационный тик (~60Hz): двигает камеру/zoom пока есть активные
    // LERP-анимации или инерция скролла; когда всё осело — просто быстро
    // возвращается без рендера (дешёвая проверка нескольких Option/bool).
    // Шаг таймера — не константа: пока всё стоит, будиться 60 раз в секунду
    // незачем (см. anim::tick_interval).
    let anim_timer = Timer::from_duration(crate::anim::TICK_ACTIVE);
    event_loop.handle().insert_source(anim_timer, |_, _, state| {
        crate::anim::tick(state);
        // Пока идёт демонстрация экрана, кадр нужен РОВНО по расписанию.
        // Обычно parallax рисует только по изменениям, и на статичном экране поток
        // проседал до 3 кадров в секунду — зритель видел рывки, а клиент мог
        // счесть поток зависшим. Кадры берутся из отрисованного (см.
        // push_cast_frame), поэтому будим рендер сами, ровно с частотой потока.
        if state.portal_cast.as_ref().is_some_and(|c| c.due()) {
            state.request_redraw();
        }
        TimeoutAction::ToDuration(state.tick_interval())
    })?;

    // `--vr`: надеть шлем сразу при старте, не дожидаясь бинда. Отказ не
    // фатален — сеанс продолжается на мониторах, а причина уходит в лог.
    if std::env::args().any(|a| a == "--vr") || state.lua_config.vr.auto {
        if let Err(e) = crate::vr::включить(&mut state) {
            tracing::warn!("plx/vr: --vr did not work: {e}");
        }
    }

    // Как в anvil — dispatch с timeout чтобы seatd не голодал
    loop {
        // Ждать дольше можно ровно настолько, насколько нечего показывать:
        // событие всё равно будит цикл немедленно, а тик стоит сразу за
        // dispatch — то есть анимация, начатая любым событием, трогается с
        // места в той же итерации и медленный таймер её не задерживает.
        let result = event_loop.dispatch(Some(state.tick_interval()), &mut state);
        if result.is_err() { break; }
        // Выход/перезапуск: строго после dispatch, до всей работы кадра —
        // рисовать и рассылать кадры уходящей сессии незачем. Проверять надо
        // именно флаг: loop_signal.stop() смотрит только EventLoop::run(), а
        // здесь цикл свой, и Super+Shift+Q поэтому не работал вовсе (см.
        // state::ExitAction).
        if state.exit.is_some() { break; }
        // Тик здесь, а не только по таймеру: он и делает редкий таймер
        // безопасным (см. anim::TICK_IDLE). Считается по Instant, поэтому
        // лишний вызов ничего не ломает — он просто сэмплирует анимацию.
        crate::anim::tick(&mut state);
        state.space.refresh();
        state.popups.cleanup();
        // Переход в полный экран доигрывается СТРОГО до отрисовки: кадр
        // клиента во весь экран и переключённый холст обязаны попасть в один
        // и тот же кадр, иначе видно промежуточное состояние (см.
        // fullscreen.rs).
        state.apply_pending_fullscreen();
        // …а уже развёрнутому окну возвращается кадр, если его успел увести
        // обзор или перелёт по столам (см. resync_fullscreen_frame).
        state.resync_fullscreen_frame();
        // X11-клиенты должны узнать, куда мы их передвинули за этот тик
        // (раскладка, анимации, драг) — см. Parallax::sync_x11_geometry.
        state.sync_x11_geometry();
        // Курсор — устройство ЭКРАНА: если камера за эту итерацию уехала сама
        // (анимация, инерция, зум, смена стола), стрелка остаётся на месте
        // монитора, а под ней пересчитывается точка холста и рассылается
        // pointer.motion. Строго после space.refresh()/sync_x11_geometry —
        // hit-test должен видеть уже разложенные окна, и строго до
        // flush_clients, чтобы motion ушёл клиентам этой же итерацией.
        state.sync_pointer_to_camera();
        let _ = state.display_handle.flush_clients();
        // Один render_all() на весь дозреваемый в dispatch() пакет событий
        // (клавиши/мышь/anim-тик) вместо N вызовов, раскиданных по хендлерам —
        // см. Parallax::request_redraw() и комментарии на каждом старом callsite.
        if state.needs_redraw {
            state.needs_redraw = false;
            crate::udev::render_all(&mut state);
        }
        // ── Шлем ────────────────────────────────────────────────────────────
        // Строго ПОСЛЕ кадра мониторов и после flush_clients: внутри лежит
        // `xrWaitFrame`, то есть ожидание своей очереди у шлема (до 11 мс на
        // 90 Гц). Всё, что должно уйти клиентам в этой итерации, обязано уйти
        // до того, как мы уснём. Пока VR выключен — это одна проверка на
        // `Option` за итерацию (см. vr/mod.rs).
        crate::vr::тик(&mut state);
        // ── Minecraft ───────────────────────────────────────────────────────
        // Ровно там же и по той же причине: кадр панелей уезжает моду после
        // того, как всё положенное ушло клиентам. Пока режим выключен — одна
        // проверка `Option` за итерацию (см. mine/mod.rs).
        crate::mine::тик(&mut state);
    }

    // Перезапуск — это код возврата, а не exec: скрипт запуска поднимает нас
    // заново из чистого процесса. Через exec() пришлось бы тащить в новую
    // жизнь чужие дескрипторы — DRM master остался бы занят прежним открытым
    // файлом, а сокет wayland-1 держал бы свой lock, и новый компоновщик сел
    // бы на wayland-2 мимо всех клиентов сессии.
    if state.exit == Some(crate::state::ExitAction::Restart) {
        tracing::info!("plx: exiting with code {} — expecting a restart", crate::state::RESTART_EXIT_CODE);
        // Хвост лога должен успеть лечь на диск: дальше процесс не разворачивает
        // стек, а сразу отдаёт код скрипту.
        drop(state);
        std::process::exit(crate::state::RESTART_EXIT_CODE);
    }

    Ok(())
}
