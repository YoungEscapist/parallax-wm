mod anim;
mod canvas;
mod columns;
mod config;
mod grabs;
mod handlers;
mod input;
mod overview;
mod selection;
mod session;
mod state;
mod udev;
mod tiling;
mod winit;

use smithay::reexports::{
    calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    wayland_server::Display,
};
pub use state::Dawn;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    tracing::info!("dawn starting");

    let mut event_loop: EventLoop<Dawn> = EventLoop::try_new()?;
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
