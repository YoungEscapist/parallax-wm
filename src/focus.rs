//! Цель клавиатурного фокуса.
//!
//! Для нативного Wayland-клиента это просто его `wl_surface`. Для X11-клиента
//! так нельзя: кроме wayland-фокуса X-серверу нужен ещё и собственный
//! (`SetInputFocus` / `WM_TAKE_FOCUS` по ICCCM), иначе окно подсвечено как
//! активное, но клавиши в него не идут. Всю эту работу smithay делает в
//! `KeyboardTarget for X11Surface` — значит целью фокуса должна быть именно
//! X11-поверхность, а не её `wl_surface`.
//!
//! Бонус: X11Surface умеет принять фокус ДО того, как XWayland свяжет с окном
//! wl_surface — enter запоминается и доигрывается при связывании. Поэтому
//! только что появившееся X11-окно можно фокусировать сразу, не дожидаясь
//! первого коммита.

use smithay::{
    backend::input::KeyState,
    desktop::Window,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Serial},
    wayland::seat::WaylandFocus,
    xwayland::X11Surface,
};

use crate::state::Dawn;

#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl KeyboardFocusTarget {
    /// Цель фокуса для окна — независимо от протокола.
    pub fn for_window(window: &Window) -> Option<Self> {
        match window.underlying_surface() {
            smithay::desktop::WindowSurface::Wayland(t) => {
                Some(Self::Wayland(t.wl_surface().clone()))
            }
            smithay::desktop::WindowSurface::X11(s) => Some(Self::X11(s.clone())),
        }
    }

    /// Поверхность цели. У X11-окна её может ещё не быть (см. модуль-док).
    pub fn surface(&self) -> Option<WlSurface> {
        match self {
            Self::Wayland(s) => Some(s.clone()),
            Self::X11(s) => s.wl_surface(),
        }
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Wayland(surface)
    }
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Wayland(s) => s.alive(),
            Self::X11(s) => s.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<std::borrow::Cow<'_, WlSurface>> {
        match self {
            Self::Wayland(s) => Some(std::borrow::Cow::Borrowed(s)),
            Self::X11(s) => s.wl_surface().map(std::borrow::Cow::Owned),
        }
    }
}

impl KeyboardTarget<Dawn> for KeyboardFocusTarget {
    fn enter(&self, seat: &Seat<Dawn>, data: &mut Dawn, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<Dawn>, data: &mut Dawn, serial: Serial) {
        match self {
            Self::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(s) => KeyboardTarget::leave(s, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<Dawn>,
        data: &mut Dawn,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: u32,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<Dawn>,
        data: &mut Dawn,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            Self::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
        }
    }
}

impl Dawn {
    /// Поверхность, у которой сейчас клавиатурный фокус. Почти весь код dawn
    /// оперирует именно поверхностями (сравнить с окном, найти окно) — тип
    /// цели фокуса им не интересен.
    pub fn focused_surface(&self) -> Option<WlSurface> {
        self.seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
            .and_then(|f| f.surface())
    }
}
