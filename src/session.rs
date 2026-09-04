use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use smithay::desktop::Window;
use smithay::utils::{Logical, Point};
use smithay::wayland::shell::xdg::{ToplevelSurface, XdgToplevelSurfaceData};

#[derive(Serialize, Deserialize)]
struct PersistedWindow {
    app_id: String,
    x: i32,
    y: i32,
}

#[derive(Serialize, Deserialize, Default)]
struct PersistedSession {
    windows: Vec<PersistedWindow>,
}

fn session_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/parallax/session.json"))
}

pub fn toplevel_app_id(surface: &ToplevelSurface) -> Option<String> {
    smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgToplevelSurfaceData>()
            .and_then(|d| d.lock().ok())
            .and_then(|attrs| attrs.app_id.clone())
    })
}

pub fn window_app_id(window: &Window) -> Option<String> {
    crate::xwin::app_id(window)
}

/// Сохраняет app_id + позицию (float_position, глобальные canvas-координаты)
/// каждого известного окна в JSON (4.3).
pub fn save(tagged_windows: &[crate::state::TaggedWindow]) {
    let path = match session_path() {
        Some(p) => p,
        None => return,
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let windows = tagged_windows
        .iter()
        .filter_map(|tw| {
            window_app_id(&tw.window).map(|app_id| PersistedWindow {
                app_id,
                x: tw.float_position.x,
                y: tw.float_position.y,
            })
        })
        .collect();
    let session = PersistedSession { windows };
    match serde_json::to_string_pretty(&session) {
        Ok(json) => match std::fs::write(&path, json) {
            Ok(()) => tracing::info!("plx/session: saved to {:?}", path),
            Err(e) => tracing::warn!("plx/session: could not write {:?}: {}", path, e),
        },
        Err(e) => tracing::warn!("plx/session: serialisation: {}", e),
    }
}

/// Загружает сохранённую топологию (4.3): app_id → очередь позиций.
/// Несколько окон с одинаковым app_id сопоставляются по очереди (Vec, а не
/// перезапись одним значением).
pub fn load() -> HashMap<String, Vec<Point<i32, Logical>>> {
    let path = match session_path() {
        Some(p) => p,
        None => return HashMap::new(),
    };
    let data = match std::fs::read_to_string(&path) {
        Ok(d) => d,
        Err(_) => return HashMap::new(),
    };
    let session: PersistedSession = match serde_json::from_str(&data) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("plx/session: parsing {:?}: {}", path, e);
            return HashMap::new();
        }
    };
    let mut map: HashMap<String, Vec<Point<i32, Logical>>> = HashMap::new();
    for w in session.windows {
        map.entry(w.app_id).or_default().push(Point::from((w.x, w.y)));
    }
    tracing::info!("plx/session: loaded {:?}", path);
    map
}
