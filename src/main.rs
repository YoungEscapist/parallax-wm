mod anim;
mod canvas;
mod columns;
mod config;
mod decor;
mod dwindle;
mod focus;
mod grabs;
mod handlers;
mod input;
mod overview;
mod screencopy;
mod selection;
mod session;
mod state;
mod udev;
mod tiling;
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
pub use state::Dawn;

/// Перелить переменные сессии в окружение D-Bus-активации и systemd --user.
///
/// Всё, что запускается не нами, а шиной (портал, его бэкенд, pipewire-клиенты),
/// наследует окружение НЕ от dawn, а от systemd --user, который стартовал при
/// логине и про наш wayland-сокет ничего не знает. Отсюда классический симптом:
/// портал есть, а «поделиться экраном» отдаёт чёрный кадр или сразу ошибку.
///
/// Зовётся дважды: сразу после подъёма бэкенда (WAYLAND_DISPLAY уже известен) и
/// после старта Xwayland (появляется DISPLAY). Отсутствие
/// dbus-update-activation-environment не фатально — просто предупреждение.
pub fn export_session_env() {
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
            tracing::info!("dawn: окружение сессии передано в D-Bus: {:?}", present);
        }
        Ok(st) => {
            tracing::warn!("dawn: dbus-update-activation-environment вернул {}", st);
        }
        Err(e) => {
            tracing::warn!("dawn: dbus-update-activation-environment не запустился: {}", e);
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!("dawn starting");

    // Явный 'static: LoopHandle с этим временем жизни нужен X11Wm::start_wm,
    // который сам вешает в цикл источник X11-событий (см. xwayland.rs).
    let mut event_loop: EventLoop<'static, Dawn> = EventLoop::try_new()?;
    let display: Display<Dawn> = Display::new()?;
    let mut state = Dawn::new(&mut event_loop, display);

    let force_tty   = std::env::args().any(|a| a == "--tty");
    let force_winit = std::env::args().any(|a| a == "--winit");
    // Не доверяем DISPLAY/WAYLAND_DISPLAY — могут быть унаследованы от родителя
    let tty_mode = force_tty || !force_winit;

    tracing::info!("dawn: mode={} (force_tty={} force_winit={})",
        if tty_mode { "TTY" } else { "Winit" },
        force_tty, force_winit);

    if tty_mode {
        tracing::info!("dawn: trying TTY/DRM backend...");
        match crate::udev::init_udev(&mut event_loop, &mut state) {
            Ok(()) if !state.udev_devices.is_empty() => {
                tracing::info!("dawn: TTY/DRM backend OK");
                unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
            }
            Ok(()) => {
                tracing::warn!("dawn: no DRM devices found, falling back to Winit");
                crate::winit::init_winit(&mut event_loop, &mut state)?;
                unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
            }
            Err(e) => {
                tracing::warn!("dawn: DRM failed ({}), falling back to Winit", e);
                crate::winit::init_winit(&mut event_loop, &mut state)?;
                unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
            }
        }
    } else {
        tracing::info!("dawn: Winit backend");
        crate::winit::init_winit(&mut event_loop, &mut state)?;
        unsafe { std::env::set_var("WAYLAND_DISPLAY", &state.socket_name) };
    }

    tracing::info!("dawn socket: {:?}", state.socket_name);

    // Отдаём окружение сессии в D-Bus/systemd --user. Без этого
    // xdg-desktop-portal (его поднимает не dawn, а D-Bus-активация) не видит ни
    // WAYLAND_DISPLAY, ни XDG_CURRENT_DESKTOP: подключиться к нам он не может,
    // а бэкенд выбирает по «рабочему столу», которого не знает. Для
    // демонстрации экрана в Discord это обязательный шаг — см. screencopy.rs.
    export_session_env();

    // XWayland поднимаем ПОСЛЕ бэкенда: DISPLAY выставится по готовности
    // сервера, и всё, что мы спавним дальше, увидит уже рабочий X11.
    crate::xwayland::start(&mut event_loop, &mut state);

    // Анимационный тик (~60Hz): двигает камеру/zoom пока есть активные
    // LERP-анимации или инерция скролла; когда всё осело — просто быстро
    // возвращается без рендера (дешёвая проверка нескольких Option/bool).
    let anim_timer = Timer::from_duration(std::time::Duration::from_millis(16));
    event_loop.handle().insert_source(anim_timer, |_, _, state| {
        crate::anim::tick(state);
        TimeoutAction::ToDuration(std::time::Duration::from_millis(16))
    })?;

    // Как в anvil — dispatch с timeout чтобы seatd не голодал
    loop {
        let result = event_loop.dispatch(
            Some(std::time::Duration::from_millis(16)),
            &mut state,
        );
        if result.is_err() { break; }
        state.space.refresh();
        state.popups.cleanup();
        // X11-клиенты должны узнать, куда мы их передвинули за этот тик
        // (раскладка, анимации, драг) — см. Dawn::sync_x11_geometry.
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
        // см. Dawn::request_redraw() и комментарии на каждом старом callsite.
        if state.needs_redraw {
            state.needs_redraw = false;
            crate::udev::render_all(&mut state);
        }
    }

    Ok(())
}
