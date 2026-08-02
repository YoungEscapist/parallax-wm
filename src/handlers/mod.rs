mod compositor;
mod layer_shell;
mod xdg_shell;

use crate::focus::KeyboardFocusTarget;
use crate::state::Dawn;
use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface};
use smithay::utils::Serial;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::{SelectionHandler, SelectionSource, SelectionTarget};
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::{delegate_data_device, delegate_output, delegate_seat};

impl SeatHandler for Dawn {
    // Клавиатурный фокус — не просто поверхность: X11-окну нужен ещё и фокус
    // на стороне X-сервера, см. focus.rs.
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Dawn> { &mut self.seat_state }
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.cursor_status = image;
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KeyboardFocusTarget>) {
        let dh = &self.display_handle;
        let client = focused
            .and_then(|f| f.surface())
            .and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}
delegate_seat!(Dawn);

impl SelectionHandler for Dawn {
    type SelectionUserData = ();

    /// Wayland-клиент положил что-то в буфер обмена — сообщаем об этом
    /// X-серверу, иначе Ctrl+C в wayland-окне не виден X11-приложениям.
    fn new_selection(&mut self, ty: SelectionTarget, source: Option<SelectionSource>, _seat: Seat<Self>) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.new_selection(ty, source.map(|s| s.mime_types())) {
                tracing::warn!("dawn/xwayland: не удалось отдать буфер обмена X11: {}", err);
            }
        }
    }

    /// Обратное направление: содержимое запросил wayland-клиент, а владеет им
    /// X11-приложение — просим XWM выгрузить данные в этот fd.
    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::unix::io::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        if let Some(xwm) = self.xwm.as_mut() {
            if let Err(err) = xwm.send_selection(ty, mime_type, fd) {
                tracing::warn!("dawn/xwayland: чтение буфера обмена X11: {}", err);
            }
        }
    }
}

impl DataDeviceHandler for Dawn {
    fn data_device_state(&mut self) -> &mut DataDeviceState { &mut self.data_device_state }
}

impl DndGrabHandler for Dawn {}

impl WaylandDndGrabHandler for Dawn {
    fn dnd_requested<S: Source>(&mut self, source: S, _icon: Option<WlSurface>,
        seat: Seat<Self>, serial: Serial, type_: GrabType) {
        match type_ {
            GrabType::Pointer => {
                let ptr = seat.get_pointer().unwrap();
                let start_data = ptr.grab_start_data().unwrap();
                let grab = DnDGrab::new_pointer(&self.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            GrabType::Touch => { source.cancel(); }
        }
    }
}
delegate_data_device!(Dawn);

impl OutputHandler for Dawn {}
delegate_output!(Dawn);
