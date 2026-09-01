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

/// Меню на время своего захвата забирает клавиатуру себе (Escape, стрелки,
/// набор в поле поиска). Требуется `PopupManager::grab_popup`.
impl From<smithay::desktop::PopupKind> for KeyboardFocusTarget {
    fn from(popup: smithay::desktop::PopupKind) -> Self {
        Self::Wayland(popup.wl_surface().clone())
    }
}

/// Цель клавиатуры → цель указателя (у dawn это просто поверхность).
///
/// Нужно ровно одному месту: захвату меню (`PopupPointerGrab`), который по
/// цепочке попапов вычисляет, кому отдавать движения мыши. Требование
/// `PointerFocus: From<KeyboardFocus>` идёт из smithay.
///
/// Ветка X11 сюда не попадает и попасть не может: xdg_popup — это протокол
/// Wayland, корнем цепочки меню всегда оказывается wayland-toplevel, а сами
/// звенья цепочки приходят через `From<PopupKind>` выше. X11-меню живут
/// override-redirect окнами и захвата smithay не заводят (см. xwayland.rs).
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(target: KeyboardFocusTarget) -> Self {
        match target {
            KeyboardFocusTarget::Wayland(s) => s,
            KeyboardFocusTarget::X11(s) => s
                .wl_surface()
                .expect("X11-окно не бывает целью захвата xdg-меню, см. focus.rs"),
        }
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
        // ── Режим Minecraft ──────────────────────────────────────────────────
        //
        // Пока мод на связи, фокус хозяйского места принадлежит ИГРЕ (иначе она
        // не получила бы ни клавиши движения, ни F7), а «окно, с которым сейчас
        // работают» — это панель под взглядом, и держит её место `dmine`.
        //
        // Без этой развилки из игры работала только та половина биндов, которой
        // окно не нужно (переключить стол, открыть терминал). Всё, что метит в
        // «сфокусированное окно» — закрыть, ресайз, перенос, полный экран,
        // плавающий режим, отправить на тег — молча било по окну Minecraft:
        // Super+Q из игры закрывал бы саму игру.
        let шахта = self.mine.as_ref().filter(|ш| ш.мод_на_связи());
        if let Some(место) = шахта.and_then(|ш| ш.место.as_ref()) {
            if let Some(s) = место.get_keyboard().and_then(|kb| kb.current_focus()).and_then(|f| f.surface()) {
                return Some(s);
            }
        }
        let хозяйский = self
            .seat
            .get_keyboard()
            .and_then(|kb| kb.current_focus())
            .and_then(|f| f.surface());
        // Место мода без фокуса — это «взгляд ещё ни разу не заходил на панель»
        // (или та панель закрылась). Хозяйский фокус в этот момент — ОКНО ИГРЫ,
        // и отдавать его биндам нельзя: `Super+Q` из игры закрыл бы Minecraft, а
        // ресайз молча менял бы её размер под полным экраном — ровно так и
        // выглядела жалоба 01.09.2026 «в dmine не работает ресайз и часть
        // биндов» (замер на харнессе: окно «игры» 2560×1050 → 2560×1020, панель
        // не тронута). Лучше пусть бинд не сделает ничего, чем сделает это с
        // игрой: окна нет — значит, работать не с чем.
        if let (Some(шахта), Some(s)) = (шахта, хозяйский.as_ref()) {
            if шахта.окно_игры.as_ref().is_some_and(|и| crate::xwin::is_surface(и, s)) {
                return None;
            }
        }
        хозяйский
    }
}
