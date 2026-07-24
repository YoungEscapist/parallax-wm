mod compositor;
mod xdg_shell;

use crate::state::Dawn;
use smithay::input::dnd::{DnDGrab, DndGrabHandler, GrabType, Source};
use smithay::input::pointer::Focus;
use smithay::input::{Seat, SeatHandler, SeatState};
use smithay::reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface};
use smithay::utils::Serial;
use smithay::wayland::output::OutputHandler;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
};
use smithay::{delegate_data_device, delegate_output, delegate_seat};

impl SeatHandler for Dawn {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Dawn> { &mut self.seat_state }
    fn cursor_image(&mut self, _seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        self.cursor_status = image;
    }
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client);
    }
}
delegate_seat!(Dawn);

impl SelectionHandler for Dawn { type SelectionUserData = (); }

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
