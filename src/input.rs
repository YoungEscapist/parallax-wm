use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, ButtonState, Event, InputBackend, InputEvent,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, keysyms},
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Rectangle, SERIAL_COUNTER},
};

use crate::{
    grabs::{move_grab::MoveSurfaceGrab, pan_grab::PanGrab, resize_grab::{ResizeEdge, ResizeSurfaceGrab}},
    state::Dawn,
    tiling::Layout,
};

const BTN_LEFT:  u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

impl Dawn {
    fn kill_focused(&mut self) {
        if let Some(surface) = self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            if let Some(w) = self.space.elements()
                .find(|w| w.toplevel().map(|t| t.wl_surface() == &surface).unwrap_or(false))
                .cloned()
            {
                w.toplevel().unwrap().send_close();
                tracing::info!("dawn: kill focused");
            }
        }
    }

    pub fn process_input_event<I: InputBackend>(&mut self, event: InputEvent<I>) {
        match event {
            InputEvent::Keyboard { event, .. } => {
                let serial    = SERIAL_COUNTER.next_serial();
                let time      = Event::time_msec(&event);
                let key_state = event.state();
                let keycode   = event.key_code();
                let keycode_u32: u32 = keycode.into();

                // Трекаем Super вручную (как driftwm для logo_held)
                let pressed = key_state == smithay::backend::input::KeyState::Pressed;
                // XKB keysyms для Super
                const SUPER_L: u32 = keysyms::KEY_Super_L;
                const SUPER_R: u32 = keysyms::KEY_Super_R;

                self.seat.get_keyboard().unwrap().input::<(), _>(
                    self,
                    keycode,
                    key_state,
                    serial,
                    time,
                    |state, modifiers, handle| {
                        let sym = handle.modified_sym();
                        let raw = sym.raw();

                        // Трекаем Super по keysym
                        if raw == SUPER_L || raw == SUPER_R {
                            state.logo_held = pressed;
                            return FilterResult::Forward;
                        }

                        if !pressed { return FilterResult::Forward; }

                        let alt   = modifiers.alt;
                        let shift = modifiers.shift;
                        let logo  = modifiers.logo || state.logo_held;

                        // Для layout-independence: берём latin sym если отличается
                        let raw_latin = handle.raw_latin_sym_or_raw_current_sym()
                            .map(|s| s.raw())
                            .unwrap_or(raw);

                        // Используем raw_latin для биндингов
                        let key = raw_latin;

                        tracing::debug!(
                            "KEY: key={} alt={} shift={} logo={}", key, alt, shift, logo
                        );

                        // ── Alt+Shift+Q → quit ───────────────────────────
                        if alt && shift && key == keysyms::KEY_q {
                            tracing::info!("dawn: quit");
                            state.loop_signal.stop();
                            return FilterResult::Intercept(());
                        }

                        // ── Alt+Q → kill ─────────────────────────────────
                        if alt && !shift && key == keysyms::KEY_q {
                            state.kill_focused();
                            return FilterResult::Intercept(());
                        }

                        // ── Alt+Enter → spawn kitty ───────────────────────
                        if alt && !shift && key == keysyms::KEY_Return {
                            let socket = state.socket_name.to_string_lossy().to_string();
                            tracing::info!("dawn: spawn kitty");
                            match std::process::Command::new("kitty")
                                .env("WAYLAND_DISPLAY", &socket).env("QT_QPA_PLATFORM", "wayland").env("GDK_BACKEND", "wayland")
                                .env("LIBGL_ALWAYS_SOFTWARE", "1").spawn()
                            {
                                Ok(_) => {}
                                Err(e) => tracing::warn!("dawn: spawn kitty failed: {}", e),
                            }
                            return FilterResult::Intercept(());
                        }

                        // ── Alt+D → toggle Float/Tile (все окна) ─────────
                        if alt && !shift && key == keysyms::KEY_d {
                            if state.tile_config.layout == Layout::Float {
                                state.set_layout(Layout::Tile);
                            } else {
                                state.set_layout(Layout::Float);
                            }
                            return FilterResult::Intercept(());
                        }

                        // ── Alt+1-9 → view tag ────────────────────────────
                        let tag_keys = [
                            keysyms::KEY_1, keysyms::KEY_2, keysyms::KEY_3,
                            keysyms::KEY_4, keysyms::KEY_5, keysyms::KEY_6,
                            keysyms::KEY_7, keysyms::KEY_8, keysyms::KEY_9,
                        ];
                        if alt {
                            if let Some(idx) = tag_keys.iter().position(|&k| k == key) {
                                let tag: u32 = 1 << idx;
                                if shift {
                                    state.tag_window(tag);
                                } else {
                                    state.view_tag(tag);
                                }
                                return FilterResult::Intercept(());
                            }
                        }

                        FilterResult::Forward
                    },
                );
            }

            InputEvent::PointerMotion { event, .. } => {
               let delta = event.delta();
               tracing::info!("PTR MOTION: delta=({:.2},{:.2})", delta.x, delta.y);
               self.pointer_location.x += delta.x;
               self.pointer_location.y += delta.y;

               if let Some(output) = self.space.outputs().next() {
                   if let Some(geo) = self.space.output_geometry(output) {
                       self.pointer_location.x = self.pointer_location.x
                           .clamp(geo.loc.x as f64, (geo.loc.x + geo.size.w - 1) as f64);
                       self.pointer_location.y = self.pointer_location.y
                           .clamp(geo.loc.y as f64, (geo.loc.y + geo.size.h - 1) as f64);
                   }
               }

               let pos = self.pointer_location;
               let serial = SERIAL_COUNTER.next_serial();
               let pointer = self.seat.get_pointer().unwrap();
               let under = self.surface_under(pos);

               if let Some((surface, _)) = &under {
                   let keyboard = self.seat.get_keyboard().unwrap();
                   let same = keyboard.current_focus()
                       .map(|f| f == *surface).unwrap_or(false);
                   if !same {
                       if let Some((window, _)) = self.space
                           .element_under(pos).map(|(w, l)| (w.clone(), l))
                       {
                           self.space.elements().for_each(|w| { w.set_activated(false); });
                           window.set_activated(true);
                           keyboard.set_focus(self, Some(surface.clone()), serial);
                           self.space.elements().for_each(|w| {
                               w.toplevel().unwrap().send_pending_configure();
                           });
                       }
                   }
               }
               pointer.motion(self, under, &MotionEvent {
                   location: pos, serial, time: event.time_msec(),
               });
               pointer.frame(self);
               // Курсор client-side — его позицию перерисовывает только сам
               // рендер; без явного пинка тут курсор будет виден в последнем
               // отрендеренном кадре, а не там, где мышь реально находится.
               crate::udev::render_all(self);
           }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                tracing::info!("PTR MOTION ABS");
                let output = self.space.outputs().next().unwrap();
                let output_geo = self.space.output_geometry(output).unwrap();
                let pos = event.position_transformed(output_geo.size) + output_geo.loc.to_f64();
                self.pointer_location = pos;
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
                let under = self.surface_under(pos);

                // sloppyfocus: focus follows cursor (как в dwl sloppyfocus=1)
                if let Some((surface, _)) = &under {
                    let keyboard = self.seat.get_keyboard().unwrap();
                    let current_focus = keyboard.current_focus();
                    let same = current_focus.as_ref()
                        .map(|f| f == surface)
                        .unwrap_or(false);
                    if !same {
                        // Найти окно под курсором и активировать
                        if let Some((window, _)) = self.space
                            .element_under(pos)
                            .map(|(w, l)| (w.clone(), l))
                        {
                            // Деактивируем остальные
                            self.space.elements().for_each(|w| {
                                w.set_activated(false);
                            });
                            window.set_activated(true);
                            keyboard.set_focus(self, Some(surface.clone()), serial);
                            self.space.elements().for_each(|w| {
                                w.toplevel().unwrap().send_pending_configure();
                            });
                        }
                    }
                }

                pointer.motion(self, under, &MotionEvent {
                    location: pos, serial, time: event.time_msec(),
                });
                pointer.frame(self);
                crate::udev::render_all(self);
            }

            InputEvent::PointerButton { event, .. } => {
                let pointer    = self.seat.get_pointer().unwrap();
                let keyboard   = self.seat.get_keyboard().unwrap();
                let serial     = SERIAL_COUNTER.next_serial();
                let button     = event.button_code();
                let btn_state  = event.state();
                let kb_mods = keyboard.modifier_state();
                let super_held = kb_mods.logo;

                tracing::info!(
                    "PTR: button={} state={:?} logo_held={} kb_logo={} kb_alt={}",
                    button,
                    btn_state,
                    self.logo_held,
                    kb_mods.logo,
                    kb_mods.alt,
                );

                if ButtonState::Pressed == btn_state && !pointer.is_grabbed() {
                    let pos = pointer.current_location();

                    if let Some((window, window_loc)) = self
                        .space.element_under(pos)
                        .map(|(w, l)| (w.clone(), l))
                    {
                        // Super+ЛКМ → move
                        if super_held && button == BTN_LEFT {
                        // Рассчитываем текущий экранный пиксель для якоря пана
                        let sx = (pos.x - self.viewport.cam_x) * self.viewport.zoom;
                        let sy = (pos.y - self.viewport.cam_y) * self.viewport.zoom;
                        
                        let grab = PanGrab {
                            start_data: GrabStartData { focus: None, button, location: pos },
                            last_screen_pos: (sx, sy).into(),
                        };
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        tracing::info!("dawn/canvas: pan grab started");
                        return;
                    }
                        // Super+ПКМ → resize
                        if super_held && button == BTN_RIGHT {
                            tracing::info!("dawn: Super+RMB resize grab start");
                            let geo = self.space.element_geometry(&window)
                                .unwrap_or(Rectangle::new(window_loc, (100, 100).into()));
                            let rel = pos - window_loc.to_f64();
                            let edge = match (
                                rel.x < geo.size.w as f64 / 2.0,
                                rel.y < geo.size.h as f64 / 2.0,
                            ) {
                                (true,  true)  => ResizeEdge::TOP_LEFT,
                                (false, true)  => ResizeEdge::TOP_RIGHT,
                                (true,  false) => ResizeEdge::BOTTOM_LEFT,
                                (false, false) => ResizeEdge::BOTTOM_RIGHT,
                            };
                            let grab = ResizeSurfaceGrab::start(
                                GrabStartData { focus: None, button, location: pos },
                                window, edge, geo,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Keep);
                            return;
                        }

                        // Клик → focus
                        self.space.raise_element(&window, true);
                        window.set_activated(true);
                        keyboard.set_focus(
                            self,
                            Some(window.toplevel().unwrap().wl_surface().clone()),
                            serial,
                        );
                        self.space.elements().for_each(|w| {
                            w.toplevel().unwrap().send_pending_configure();
                        });
                    } else {
                        self.space.elements().for_each(|w| {
                            w.set_activated(false);
                            w.toplevel().unwrap().send_pending_configure();
                        });
                        keyboard.set_focus(self, Option::<WlSurface>::None, serial);

                    // Alt+ЛКМ на пустом canvas → pan
                    if super_held && button == BTN_LEFT {
                        // Рассчитываем текущий экранный пиксель для якоря пана
                        let sx = (pos.x - self.viewport.cam_x) * self.viewport.zoom;
                        let sy = (pos.y - self.viewport.cam_y) * self.viewport.zoom;
                        
                        let grab = PanGrab {
                            start_data: GrabStartData { focus: None, button, location: pos },
                            last_screen_pos: (sx, sy).into(),
                        };
                        pointer.set_grab(self, grab, serial, Focus::Clear);
                        tracing::info!("dawn/canvas: pan grab started");
                        return;
                    }
                    }
                }

                pointer.button(self, &ButtonEvent {
                    button, state: btn_state, serial, time: event.time_msec(),
                });
                pointer.frame(self);
            }

            InputEvent::PointerAxis { event, .. } => {
                let source = event.source();
                let _h_raw = event.amount(Axis::Horizontal);
                let _v_raw = event.amount(Axis::Vertical);
                let _v120 = event.amount_v120(Axis::Vertical);
                let _alt_check = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                tracing::info!("SCROLL: h={:?} v={:?} v120={:?} alt={}", _h_raw, _v_raw, _v120, _alt_check);
                let h = event.amount(Axis::Horizontal)
                    .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
                let v = event.amount(Axis::Vertical)
                    .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);

                // Alt+Scroll → zoom (anchor под курсором)
                let alt_held = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);

                if alt_held && v != 0.0 && self.tile_config.layout == crate::tiling::Layout::Float {
                    let pointer = self.seat.get_pointer().unwrap();
                    let cursor_canvas = pointer.current_location();

                    let old_zoom = self.viewport.zoom;

                    // Screen position of cursor (pixels on screen)
                    // screen = (canvas - camera) * zoom
                    let screen_x = (cursor_canvas.x - self.viewport.cam_x) * old_zoom;
                    let screen_y = (cursor_canvas.y - self.viewport.cam_y) * old_zoom;

                    // v120 > 0 = scroll up = zoom IN
                    // scroll up (v>0) = zoom in = увеличить zoom
                    let factor = if v < 0.0 { 1.1_f64 } else { 0.9_f64 };
                    let new_zoom = (old_zoom * factor).clamp(0.05, 5.0);
                    self.viewport.zoom = new_zoom;

                    // New camera: anchor canvas point stays at same screen pixel
                    // camera = canvas - screen / new_zoom
                    self.viewport.cam_x = cursor_canvas.x - screen_x / new_zoom;
                    self.viewport.cam_y = cursor_canvas.y - screen_y / new_zoom;

                    self.apply_camera();
                    tracing::info!("dawn/canvas: zoom={:.3} cam=({:.1},{:.1})",
                        new_zoom, self.viewport.cam_x, self.viewport.cam_y);
                    return;
                }

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if h != 0.0 { frame = frame.value(Axis::Horizontal, h); }
                if v != 0.0 { frame = frame.value(Axis::Vertical, v); }
                self.seat.get_pointer().unwrap().axis(self, frame);
                self.seat.get_pointer().unwrap().frame(self);
            }

            _ => {}
        }
    }
}
