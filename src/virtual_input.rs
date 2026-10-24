//! Виртуальный ввод для программ: `zwlr_virtual_pointer_v1` и
//! `zwp_virtual_keyboard_v1`.
//!
//! **Зачем.** Удалённое управление (Nixith Connect, wayvnc), набор текста
//! (wtype) и прочие «пульты» говорят с композитором этими двумя протоколами.
//! Без них такой клиент может разве что смотреть экран.
//!
//! **Указатель** входит через ту же дверь, что и синтетика харнесса
//! (`synth.rs`) — `process_input_event`: пан у края, зажим, sloppy focus и
//! бинды мыши работают для него ровно как для настоящей мыши. Абсолютная
//! точка (`motion_absolute`) — доли выхода; в физических пикселях выхода она
//! превращается в одно движение-разность, как `mouse to` у ctl.
//!
//! **Клавиатура** приходит со своей раскладкой, и тут два случая:
//!  · раскладка та же, что у сеанса (клиент взял её у wl_keyboard) — коды
//!    пропускаются сквозь `process_input_event`: срабатывают бинды parallax
//!    (Super+…) и переключение языка, как у живой клавиатуры;
//!  · раскладка своя (wtype собирает её под строку: каждому символу — свой
//!    код) — символ переводится в нажатия сеансовой раскладки: ищем код и
//!    уровень с этим keysym, при нужде жмём Shift и временно переключаем
//!    группу. Символ, которого в раскладке сеанса нет вовсе, не набирается.
//!
//! Smithay свою виртуальную клавиатуру отдаёт прямо клиенту в фокусе, мимо
//! композитора, — поэтому протокол реализован здесь целиком.

use std::collections::HashMap;
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

use smithay::backend::input::InputEvent;
use smithay::input::keyboard::{xkb, Keycode, Layout};
use smithay::output::Output;
use smithay::reexports::wayland_protocols_misc::zwp_virtual_keyboard_v1::server::{
    zwp_virtual_keyboard_manager_v1::{self, ZwpVirtualKeyboardManagerV1},
    zwp_virtual_keyboard_v1::{self, ZwpVirtualKeyboardV1},
};
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::{
    zwlr_virtual_pointer_manager_v1::{self, ZwlrVirtualPointerManagerV1},
    zwlr_virtual_pointer_v1::{self, ZwlrVirtualPointerV1},
};
use smithay::reexports::wayland_server::protocol::wl_pointer;
use smithay::reexports::wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use crate::state::Parallax;
use crate::synth::{Движение, Клавиша, Кнопка, Колесо, Синтетика};

/// Объявить оба глобала. Версия указателя 2 — с привязкой к выходу.
pub fn init(dh: &DisplayHandle) {
    dh.create_global::<Parallax, ZwlrVirtualPointerManagerV1, ()>(2, ());
    dh.create_global::<Parallax, ZwpVirtualKeyboardManagerV1, ()>(1, ());
}

const KEY_LEFTSHIFT: u32 = 42;

// ── Указатель ────────────────────────────────────────────────────────────────

pub struct Указатель {
    /// Выход, к которому привязаны абсолютные координаты; None — монитор под курсором.
    выход: Option<Output>,
    /// Прокрутка копится до `frame`: (верт, гор) в пикселях и в зубцах.
    ось: Mutex<Ось>,
}

#[derive(Default)]
struct Ось {
    верт: f64,
    гор: f64,
    зубцы_верт: i32,
    зубцы_гор: i32,
}

impl GlobalDispatch<ZwlrVirtualPointerManagerV1, ()> for Parallax {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for Parallax {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_virtual_pointer_manager_v1::Request;
        match request {
            Request::CreateVirtualPointer { seat: _, id } => {
                data_init.init(id, Указатель { выход: None, ось: Mutex::new(Ось::default()) });
            }
            Request::CreateVirtualPointerWithOutput { seat: _, output, id } => {
                let выход = output.as_ref().and_then(Output::from_resource);
                data_init.init(id, Указатель { выход, ось: Mutex::new(Ось::default()) });
            }
            Request::Destroy => {}
            _ => {}
        }
    }
}

impl Dispatch<ZwlrVirtualPointerV1, Указатель> for Parallax {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        data: &Указатель,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwlr_virtual_pointer_v1::Request;
        match request {
            Request::Motion { time: _, dx, dy } => {
                state.process_input_event(InputEvent::PointerMotion::<Синтетика> {
                    event: Движение::new(dx, dy),
                });
            }
            Request::MotionAbsolute { time: _, x, y, x_extent, y_extent } => {
                if x_extent == 0 || y_extent == 0 {
                    return;
                }
                let доля_x = x as f64 / x_extent as f64;
                let доля_y = y as f64 / y_extent as f64;
                state.виртуально_навести(data.выход.as_ref(), доля_x, доля_y);
            }
            Request::Button { time: _, button, state: кнопка } => {
                let нажата = matches!(кнопка, WEnum::Value(wl_pointer::ButtonState::Pressed));
                state.process_input_event(InputEvent::PointerButton::<Синтетика> {
                    event: Кнопка::new(button, нажата),
                });
            }
            Request::Axis { time: _, axis, value } => {
                let mut о = data.ось.lock().unwrap();
                match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => о.гор += value,
                    _ => о.верт += value,
                }
            }
            Request::AxisDiscrete { time: _, axis, value, discrete } => {
                let mut о = data.ось.lock().unwrap();
                match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => {
                        о.гор += value;
                        о.зубцы_гор += discrete;
                    }
                    _ => {
                        о.верт += value;
                        о.зубцы_верт += discrete;
                    }
                }
            }
            Request::Frame => {
                let о = std::mem::take(&mut *data.ось.lock().unwrap());
                if о.верт == 0.0 && о.гор == 0.0 && о.зубцы_верт == 0 && о.зубцы_гор == 0 {
                    return;
                }
                // Зубцы, если клиент их назвал; иначе пиксели по 15 на зубец —
                // то же соотношение, что у синтетики (synth::Колесо).
                let в = if о.зубцы_верт != 0 { о.зубцы_верт as f64 } else { о.верт / 15.0 };
                let г = if о.зубцы_гор != 0 { о.зубцы_гор as f64 } else { о.гор / 15.0 };
                state.process_input_event(InputEvent::PointerAxis::<Синтетика> {
                    event: Колесо::по_осям(г, в),
                });
            }
            Request::AxisSource { .. } | Request::AxisStop { .. } | Request::Destroy => {}
            _ => {}
        }
        state.request_redraw();
    }
}

impl Parallax {
    /// Поставить курсор в долю (0..1) выхода. Монитор, если он не тот, где
    /// сейчас курсор, сначала делаем активным — как при переезде мышью.
    fn виртуально_навести(&mut self, выход: Option<&Output>, доля_x: f64, доля_y: f64) {
        if let Some(i) = выход.and_then(|o| self.монитор_по_выходу(o)) {
            if i != self.курсор_монитор {
                self.активировать_монитор(i);
            }
        }
        let Some(выход) = выход.cloned().or_else(|| self.мониторы.get(self.курсор_монитор).map(|m| m.output.clone())) else {
            return;
        };
        let Some(режим) = выход.current_mode() else { return };
        let цель_x = доля_x.clamp(0.0, 1.0) * (режим.size.w as f64 - 1.0);
        let цель_y = доля_y.clamp(0.0, 1.0) * (режим.size.h as f64 - 1.0);
        let сейчас = self.pointer_screen_physical();
        self.process_input_event(InputEvent::PointerMotion::<Синтетика> {
            event: Движение::new(цель_x - сейчас.x, цель_y - сейчас.y),
        });
    }
}

// ── Клавиатура ───────────────────────────────────────────────────────────────

pub struct Клава {
    сост: Mutex<КлаваСост>,
}

#[derive(Default)]
struct КлаваСост {
    раскладка: Option<Своя>,
    /// Раскладка совпала с сеансовой — коды идут как есть.
    сквозь: bool,
    /// Что мы нажали вместо каждого кода клиента — чтобы так же отпустить.
    нажато: HashMap<u32, u32>,
}

/// Раскладка клиента и её состояние (модификаторы из запроса `modifiers`).
struct Своя {
    keymap: xkb::Keymap,
    state: xkb::State,
}

// SAFETY: xkbcommon не потокобезопасен, но всё это живёт под мьютексом и
// трогается только из потока композитора (диспетчер Wayland) — так же
// поступает smithay со своей виртуальной клавиатурой.
unsafe impl Send for Своя {}
unsafe impl Sync for Своя {}

impl GlobalDispatch<ZwpVirtualKeyboardManagerV1, ()> for Parallax {
    fn bind(
        _state: &mut Self,
        _dh: &DisplayHandle,
        _client: &Client,
        resource: New<ZwpVirtualKeyboardManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for Parallax {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &ZwpVirtualKeyboardManagerV1,
        request: zwp_virtual_keyboard_manager_v1::Request,
        _data: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let zwp_virtual_keyboard_manager_v1::Request::CreateVirtualKeyboard { seat: _, id } = request {
            data_init.init(id, Клава { сост: Mutex::new(КлаваСост::default()) });
        }
    }
}

impl Dispatch<ZwpVirtualKeyboardV1, Клава> for Parallax {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &ZwpVirtualKeyboardV1,
        request: zwp_virtual_keyboard_v1::Request,
        data: &Клава,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        use zwp_virtual_keyboard_v1::Request;
        match request {
            Request::Keymap { format, fd, size } => {
                if format != 1 {
                    tracing::debug!("plx/virtual_input: раскладка не xkb_v1 ({format})");
                    return;
                }
                let file = std::fs::File::from(fd);
                let mut buf = vec![0u8; size as usize];
                // read_exact_at: смещение у fd клиента после записи — в конце файла.
                if file.read_exact_at(&mut buf, 0).is_err() {
                    tracing::debug!("plx/virtual_input: раскладку не прочитать");
                    return;
                }
                while buf.last() == Some(&0) {
                    buf.pop();
                }
                let текст = String::from_utf8_lossy(&buf).to_string();
                let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
                let Some(keymap) =
                    xkb::Keymap::new_from_string(&ctx, текст, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)
                else {
                    tracing::debug!("plx/virtual_input: раскладка не собралась");
                    return;
                };
                let своя_строка = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
                let сеанса = state.раскладка_сеанса();
                let mut с = data.сост.lock().unwrap();
                с.сквозь = сеанса.as_deref().map(|s| s.trim_end_matches('\0').trim()) == Some(своя_строка.trim_end_matches('\0').trim());
                let st = xkb::State::new(&keymap);
                с.раскладка = Some(Своя { keymap, state: st });
                tracing::info!(
                    "plx/virtual_input: виртуальная клавиатура, раскладка {}",
                    if с.сквозь { "сеанса — коды сквозь" } else { "своя — перевод в символы" }
                );
            }
            Request::Key { time: _, key, state: нажатие } => {
                let нажата = нажатие == 1;
                let mut с = data.сост.lock().unwrap();
                if с.раскладка.is_none() {
                    resource.post_error(zwp_virtual_keyboard_v1::Error::NoKeymap, "key до keymap");
                    return;
                }
                if с.сквозь {
                    drop(с);
                    state.синт_клавиша(key, нажата);
                    return;
                }
                if !нажата {
                    if let Some(код) = с.нажато.remove(&key) {
                        drop(с);
                        state.синт_клавиша(код, false);
                    }
                    return;
                }
                let своя = с.раскладка.as_ref().unwrap();
                let sym = своя.state.key_get_one_sym(Keycode::new(key + 8));
                drop(с);
                if let Some(код) = state.набрать_символ(sym) {
                    data.сост.lock().unwrap().нажато.insert(key, код);
                }
            }
            Request::Modifiers { mods_depressed, mods_latched, mods_locked, group } => {
                let mut с = data.сост.lock().unwrap();
                if let Some(своя) = с.раскладка.as_mut() {
                    своя.state.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            Request::Destroy => {
                // Отпустить всё, что клиент оставил зажатым: иначе в сеансе
                // залипнет Shift или Super.
                let нажато: Vec<u32> = data.сост.lock().unwrap().нажато.drain().map(|(_, к)| к).collect();
                for к in нажато {
                    state.синт_клавиша(к, false);
                }
            }
            _ => {}
        }
    }
}

fn модификатор(sym: xkb::Keysym) -> bool {
    use xkb::keysyms::*;
    matches!(
        sym.raw(),
        KEY_Shift_L | KEY_Shift_R | KEY_Control_L | KEY_Control_R | KEY_Alt_L | KEY_Alt_R | KEY_Super_L
            | KEY_Super_R | KEY_Meta_L | KEY_Meta_R | KEY_ISO_Level3_Shift | KEY_Hyper_L | KEY_Hyper_R
    )
}

impl Parallax {
    fn синт_клавиша(&mut self, код: u32, нажата: bool) {
        self.process_input_event(InputEvent::Keyboard::<Синтетика> { event: Клавиша::new(код, нажата) });
        self.request_redraw();
    }

    /// Раскладка сеанса строкой — та же, что уходит клиентам в wl_keyboard.keymap.
    fn раскладка_сеанса(&mut self) -> Option<String> {
        let kb = self.seat.get_keyboard()?;
        Some(kb.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            // SAFETY: строка копируется, ссылка на keymap не переживает замок.
            unsafe { xkb.keymap() }.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
        }))
    }

    /// Найти в раскладке сеанса код, уровень и группу для keysym и нажать.
    /// Модификатор нажимается и остаётся зажатым до отпускания (возвращаем
    /// его код); обычный символ — нажать и сразу отпустить.
    fn набрать_символ(&mut self, sym: xkb::Keysym) -> Option<u32> {
        let kb = self.seat.get_keyboard()?;
        // Сравниваем и по keysym, и по символу Unicode: wtype и наш демон
        // присылают «U043C», а в раскладке ru стоит старый Cyrillic_em —
        // это разные keysym одного и того же «м».
        let символ = xkb::keysym_to_utf32(sym);
        let подходит = |s: &xkb::Keysym| *s == sym || (символ != 0 && xkb::keysym_to_utf32(*s) == символ);
        // (evdev-код, группа, уровень); текущая группа — первой.
        let найдено = kb.with_xkb_state(self, |ctx| {
            let xkb = ctx.xkb().lock().unwrap();
            let активная = xkb.active_layout().0;
            // SAFETY: только чтение keymap под замком.
            let keymap = unsafe { xkb.keymap() };
            let групп = keymap.num_layouts();
            let mut порядок: Vec<u32> = vec![активная];
            порядок.extend((0..групп).filter(|g| *g != активная));
            for группа in порядок {
                for уровень in 0..2u32 {
                    for kc in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
                        let syms = keymap.key_get_syms_by_level(Keycode::new(kc), группа, уровень);
                        if syms.iter().any(&подходит) && kc >= 8 {
                            return Some((kc - 8, группа, уровень, активная));
                        }
                    }
                }
            }
            None
        });
        let Some((код, группа, уровень, активная)) = найдено else {
            tracing::debug!("plx/virtual_input: символа {:?} нет в раскладке сеанса", sym);
            return None;
        };
        if модификатор(sym) {
            self.синт_клавиша(код, true);
            return Some(код);
        }
        if группа != активная {
            kb.with_xkb_state(self, |mut ctx| ctx.set_layout(Layout(группа)));
        }
        if уровень == 1 {
            self.синт_клавиша(KEY_LEFTSHIFT, true);
        }
        self.синт_клавиша(код, true);
        self.синт_клавиша(код, false);
        if уровень == 1 {
            self.синт_клавиша(KEY_LEFTSHIFT, false);
        }
        if группа != активная {
            kb.with_xkb_state(self, |mut ctx| ctx.set_layout(Layout(активная)));
        }
        None
    }
}
