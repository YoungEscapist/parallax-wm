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
use smithay::wayland::selection::wlr_data_control::{DataControlHandler, DataControlState};
use smithay::{delegate_data_control, delegate_data_device, delegate_output, delegate_seat};

impl SeatHandler for Dawn {
    // Клавиатурный фокус — не просто поверхность: X11-окну нужен ещё и фокус
    // на стороне X-сервера, см. focus.rs.
    type KeyboardFocus = KeyboardFocusTarget;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;
    fn seat_state(&mut self) -> &mut SeatState<Dawn> { &mut self.seat_state }
    fn cursor_image(&mut self, seat: &Seat<Self>, image: smithay::input::pointer::CursorImageStatus) {
        // ЧУЖОЕ МЕСТО СВОЮ ФОРМУ КУРСОРА НАМ НЕ НАЗНАЧАЕТ. Мест в dawn больше
        // одного: своё у каждого гостя раздачи (`share/seat.rs`) и своё у мода
        // Minecraft (`mine/seat.rs`). Клиент ставит форму НА МЕСТО, а здесь она
        // молча записывалась в одну общую переменную — и стрелка хозяина
        // менялась от того, что кто-то другой навёл указку на терминал.
        //
        // Живьём это и была жалоба 01.09.2026 «нажал в игре — на экране dawn
        // появился курсор»: клик по панели уводил фокус клиенту, тот ставил
        // курсор месту `dmine`, и спрятанная Minecraft'ом стрелка (Xwayland
        // держит её `Hidden`, пока курсор захвачен) вылезала обратно поверх
        // игры. Гости раздачи рисуют свои стрелки сами (`build_guest_cursors`),
        // мод — свою у себя в мире; хозяйскую задаёт только хозяйское место.
        if seat != &self.seat {
            return;
        }
        // Разбор жалобы «иногда курсор не меняется» (Dota 2, 29.08.2026).
        // Пишем ТОЛЬКО смену вида: клиент шлёт set_cursor редко, а строка на
        // каждый кадр когда-то раздула лог до 775 МБ (см. КУРСОР КЛИЕНТА).
        //
        // Что здесь видно: `клиент` значит форму дал клиент (Xwayland отдаёт
        // сюда картинку игры), `тема "имя"` — форму дали МЫ или клиент через
        // wp_cursor_shape_v1. Стрелка вместо прицела в игре различается прямо
        // по этой строке: `тема "default"` — потеряли форму сами, `клиент` —
        // её прислал Xwayland, и разбираться надо на его стороне.
        let вид = |с: &smithay::input::pointer::CursorImageStatus| match с {
            smithay::input::pointer::CursorImageStatus::Surface(_) => "клиент".to_string(),
            smithay::input::pointer::CursorImageStatus::Named(i) => format!("тема {:?}", i),
            smithay::input::pointer::CursorImageStatus::Hidden => "скрыт".to_string(),
        };
        let (было, стало) = (вид(&self.cursor_status), вид(&image));
        if было != стало {
            tracing::debug!("dawn/курсор: {} → {}", было, стало);
        }
        self.cursor_status = image;
        // Без этого новая форма курсора не доедет до экрана, пока что-нибудь
        // ДРУГОЕ не попросит кадр: dawn рисует по изменениям, а смена формы —
        // изменение ничем не хуже прочих. На неподвижном экране (меню игры,
        // пауза, статичное окно) это ровно «курсор не поменялся».
        self.request_redraw();
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

// wp_cursor_shape_v1 умеет назначать форму не только указателю, но и перу
// планшета, поэтому смитеевский delegate требует и этот трейт. Планшета у нас
// нет — форму пера просто игнорируем (реализация по умолчанию).
impl smithay::wayland::tablet_manager::TabletSeatHandler for Dawn {}

// wp_cursor_shape_v1: клиент присылает не картинку, а имя формы, и оно
// приходит сюда же, в cursor_image, как CursorImageStatus::Named — рисуем её
// своей темой и своего размера (см. state::cursor_for_icon).
smithay::delegate_cursor_shape!(Dawn);

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

/// `wlr-data-control`: доступ к буферу обмена БЕЗ фокуса на окне.
///
/// Обычный `wl_data_device` отдаёт содержимое только тому, у кого клавиатурный
/// фокус, — это защита от подглядывания за буфером. Менеджеру буфера (cliphist,
/// Super+C) она мешает: он должен видеть КАЖДОЕ копирование, ничего при этом не
/// показывая на экране. Для таких клиентов и придуман этот протокол.
///
/// Без него `wl-paste --watch` выходит с «Watch mode requires a compositor that
/// supports the data-control protocol» (замер 12.08.2026) — история буфера не
/// набивалась вовсе.
impl DataControlHandler for Dawn {
    fn data_control_state(&mut self) -> &mut DataControlState { &mut self.data_control_state }
}
delegate_data_control!(Dawn);

impl OutputHandler for Dawn {}
delegate_output!(Dawn);
