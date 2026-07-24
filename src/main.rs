mod canvas;
mod grabs;
mod handlers;
mod input;
mod state;
mod udev;
mod tiling;
mod winit;

use smithay::reexports::{calloop::EventLoop, wayland_server::Display};
pub use state::Dawn;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt().init();
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

    if tty_mode && state.udev_devices.is_empty() {
        eprintln!("dawn: ERROR — нет DRM устройств!");
        std::process::exit(1);
    }

    tracing::info!("dawn socket: {:?}", state.socket_name);

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
    }

    Ok(())
}
