use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event,
        GesturePinchUpdateEvent,
        GestureSwipeUpdateEvent, InputBackend, InputEvent,
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
    grabs::{move_grab::MoveSurfaceGrab, resize_grab::{ResizeEdge, ResizeSurfaceGrab}},
    state::Dawn,
    tiling::Layout,
};

const BTN_LEFT:  u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

impl Dawn {
    pub(crate) fn kill_focused(&mut self) {
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

                        // Трекаем Super по keysym + детект "тапа" (обзор столов)
                        if raw == SUPER_L || raw == SUPER_R {
                            state.logo_held = pressed;
                            if pressed {
                                // Кандидат на тап — сбросится любым другим вводом.
                                state.super_tap = true;
                            } else {
                                if state.super_tap {
                                    state.toggle_overview(); // чистый тап Super
                                }
                                state.super_tap = false;
                            }
                            return FilterResult::Forward;
                        }

                        // Любое другое нажатие клавиши отменяет ожидающий тап Super
                        // (Super+D, Super+1 и т.п. — это не тап).
                        if pressed {
                            state.super_tap = false;
                        }

                        // ── Super+Space (тумблер) → режим лупы (zoom-nav) ──
                        // Настраивается через set{bird_eye_key=...} (по умолчанию space).
                        // Раньше это был hold-жест bird's-eye; теперь тумблер: вкл —
                        // зум к центру, стрелки панорамируют, повторный Super+Space
                        // сбрасывает (см. Dawn::toggle_zoom_nav).
                        if raw == state.lua_config.bird_eye_key {
                            if pressed && state.logo_held {
                                state.toggle_zoom_nav();
                            }
                            if state.logo_held {
                                return FilterResult::Intercept(());
                            }
                            return FilterResult::Forward;
                        }

                        if !pressed { return FilterResult::Forward; }

                        let alt   = modifiers.alt;
                        let shift = modifiers.shift;
                        let ctrl  = modifiers.ctrl;
                        let logo  = modifiers.logo || state.logo_held;

                        // Для layout-independence: берём latin sym если отличается
                        // (важно теперь вдвойне — с несколькими XKB-раскладками
                        // биндинги должны срабатывать независимо от активной)
                        let raw_latin = handle.raw_latin_sym_or_raw_current_sym()
                            .map(|s| s.raw())
                            .unwrap_or(raw);

                        tracing::debug!(
                            "KEY: key={} alt={} shift={} ctrl={} logo={}", raw_latin, alt, shift, ctrl, logo
                        );

                        // Режим лупы (Super+Space): голые стрелки панорамируют
                        // увеличенный вид — перехватываем раньше обычных биндов
                        // (focus_direction), чтобы не уводить фокус.
                        if state.zoom_nav_mode {
                            let step = if raw_latin == keysyms::KEY_Left {
                                Some((-1.0, 0.0))
                            } else if raw_latin == keysyms::KEY_Right {
                                Some((1.0, 0.0))
                            } else if raw_latin == keysyms::KEY_Up {
                                Some((0.0, -1.0))
                            } else if raw_latin == keysyms::KEY_Down {
                                Some((0.0, 1.0))
                            } else {
                                None
                            };
                            if let Some((dx, dy)) = step {
                                state.zoom_nav_pan(dx, dy);
                                return FilterResult::Intercept(());
                            }
                        }

                        // ── Биндинги из Lua-конфига (см. src/config.rs, default_config.lua) ──
                        let mods = crate::config::ModMask { ctrl, alt, shift, logo };
                        if let Some(action) = state.lua_config.find_action(mods, raw_latin) {
                            state.dispatch_action(action);
                            return FilterResult::Intercept(());
                        }

                        FilterResult::Forward
                    },
                );
                // Любое нажатие клавиши → обновляем экран (переключение тегов не лагает)
                self.request_redraw();
            }

            InputEvent::PointerMotion { event, .. } => {
               let delta = event.delta();
               tracing::trace!("PTR MOTION: delta=({:.2},{:.2})", delta.x, delta.y);
               let zoom = self.viewport.zoom;

               // В обзоре: Super+ЛКМ драг стола — двигаем окна стола, не камеру
               if self.overview_active {
                   if let Some(mask) = self.overview_drag_ws {
                       let dcam_x = delta.x / zoom;
                       let dcam_y = delta.y / zoom;
                       self.pointer_location.x += dcam_x;
                       self.pointer_location.y += dcam_y;
                       self.overview_move_workspace_windows(mask, dcam_x, dcam_y);
                       return;
                   }
               }

               // Alt+LMB pan (Float) / ЛКМ-пан в обзоре столов: курсор стоит,
               // холст движется в сторону drag.
               if self.pan_button_held
                   && (self.tile_config.layout == Layout::Float || self.overview_active)
               {
                   let dcam_x = delta.x / zoom;
                   let dcam_y = delta.y / zoom;
                   self.viewport.cam_x -= dcam_x;
                   self.viewport.cam_y -= dcam_y;
                   // Кинетический скролл (1.1): копим дельту для инерции на отпускание
                   self.momentum.accumulate(
                       smithay::utils::Point::from((-dcam_x, -dcam_y)),
                       event.time_msec(),
                   );
                   self.apply_camera();
                   self.request_redraw();
                   return;
               }

               // Обычное движение курсора — дельта в canvas-единицах
               self.pointer_location.x += delta.x / zoom;
               self.pointer_location.y += delta.y / zoom;

               // Курсор не должен выходить за экран: зажимаем, переливаем в камеру.
               // Координаты в ЛОГИЧЕСКИХ единицах (output-local), без умножения на zoom.
               {
                   let out_geo_opt = {
                       let o = self.space.outputs().next().cloned();
                       o.and_then(|o| self.space.output_geometry(&o))
                   };
                   if let Some(out_geo) = out_geo_opt {
                       // Логическая позиция курсора относительно левого-верхнего угла output'а
                       let sx = self.pointer_location.x - self.viewport.cam_x;
                       let sy = self.pointer_location.y - self.viewport.cam_y;
                       let ow = out_geo.size.w as f64;
                       let oh = out_geo.size.h as f64;
                       let csx = sx.clamp(0.0, ow);
                       let csy = sy.clamp(0.0, oh);
                       // Зажимаем курсор (canvas coords)
                       self.pointer_location.x = csx + self.viewport.cam_x;
                       self.pointer_location.y = csy + self.viewport.cam_y;
                       // Перелив → плавное движение камеры (только Float)
                       if self.tile_config.layout == Layout::Float {
                           let over_x = sx - csx;
                           let over_y = sy - csy;
                           if over_x != 0.0 || over_y != 0.0 {
                               // Скорость pan пропорциональна зуму: при zoom больше — viewport
                               // меньше, поэтому pan нужен медленнее чтобы не улетать.
                               // Коэффициент 0.6 (было 0.2): у края курсор ощутимо
                               // меньше "тормозит", холст тянется за ним быстрее.
                               self.viewport.cam_x += over_x * 0.6 / zoom;
                               self.viewport.cam_y += over_y * 0.6 / zoom;
                               self.apply_camera();
                           }
                       }
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
               self.request_redraw();
           }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                tracing::trace!("PTR MOTION ABS");
                let output = self.space.outputs().next().unwrap();
                let output_geo = self.space.output_geometry(output).unwrap();
                // Абсолютная позиция в screen-пикселях → конвертируем в canvas
                let zoom = self.viewport.zoom;
                let cam_x = self.viewport.cam_x;
                let cam_y = self.viewport.cam_y;
                let screen_pos = event.position_transformed(output_geo.size);
                let pos = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                    screen_pos.x / zoom + cam_x,
                    screen_pos.y / zoom + cam_y,
                ));
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
                self.request_redraw();
            }

            InputEvent::PointerButton { event, .. } => {
                let pointer    = self.seat.get_pointer().unwrap();
                let keyboard   = self.seat.get_keyboard().unwrap();
                let serial     = SERIAL_COUNTER.next_serial();
                let button     = event.button_code();
                let btn_state  = event.state();
                let kb_mods = keyboard.modifier_state();
                let alt_held = kb_mods.alt;

                tracing::debug!(
                    "PTR: button={} state={:?} logo_held={} kb_logo={} kb_alt={}",
                    button,
                    btn_state,
                    self.logo_held,
                    kb_mods.logo,
                    kb_mods.alt,
                );

                // Клик по миникарте: телепорт к окну под курсором.
                if ButtonState::Pressed == btn_state && self.try_handle_minimap_click() {
                    return;
                }

                // Любой клик отменяет ожидающий тап Super (обзор столов).
                if ButtonState::Pressed == btn_state {
                    self.super_tap = false;
                }

                // ── Обзор столов ──────────────────────────────────────────────
                //  · ПКМ → выйти на стол под курсором
                //  · ЛКМ → фокус на окне (основной хендлер), потом exit на стол
                //  · Alt+ЛКМ → pan ленты
                //  · Super+ЛКМ → драг окна (move_grab → overview_reassign)
                //  · Super+Alt+ЛКМ по пустому → драг стола (snap на отпускании).
                if self.overview_active {
                    if button == BTN_LEFT {
                        if alt_held {
                            // Alt+ЛКМ → pan (без grab, флаг)
                            self.pan_button_held = ButtonState::Pressed == btn_state;
                            return;
                        }
                        if self.logo_held {
                            // Super+ЛКМ: падаем в основной хендлер (move окна)
                        } else {
                            // ЛКМ без модификаторов: падаем в основной хендлер
                            // (он сделает фокус на окне или сбросит).
                            // После него выйдем из обзора (см. ниже).
                        }
                    }
                    if ButtonState::Pressed == btn_state && button == BTN_RIGHT {
                        self.exit_overview_to_cursor();
                        return;
                    }
                }

                // Alt+ЛКМ press → начинаем pan (флаг, без grab)
                if ButtonState::Pressed == btn_state
                    && alt_held && button == BTN_LEFT
                    && self.tile_config.layout == Layout::Float
                {
                    self.pan_button_held = true;
                    tracing::debug!("dawn/canvas: pan started");
                    return;
                }
                // Любое отпускание ЛКМ → завершаем pan, запускаем инерцию (1.1)
                if ButtonState::Released == btn_state && button == BTN_LEFT {
                    // Перетаскивание стола в обзоре: snap в ближайшую ячейку сетки
                    if let Some(from) = self.overview_drag_ws.take() {
                        self.overview_snap_workspace(from, self.pointer_location);
                        return;
                    }
                    if self.pan_button_held {
                        self.momentum.launch();
                    }
                    self.pan_button_held = false;
                }

                if ButtonState::Pressed == btn_state && !pointer.is_grabbed() {
                    let pos = pointer.current_location();

                    if let Some((window, window_loc)) = self
                        .space.element_under(pos)
                        .map(|(w, l)| (w.clone(), l))
                    {
                        // ПКМ (без Super) по выделенному окну → сбросить выделение.
                        if button == BTN_RIGHT && !kb_mods.logo
                            && !self.selected_windows.is_empty()
                            && self.is_selected(&window)
                        {
                            self.clear_selection();
                            pointer.button(self, &ButtonEvent {
                                button, state: btn_state, serial, time: event.time_msec(),
                            });
                            pointer.frame(self);
                            return;
                        }

                        // Super+ЛКМ → перемещение окна
                        if kb_mods.logo && button == BTN_LEFT {
                            let initial_window_location = window_loc;
                            let focus = window.toplevel()
                                .map(|t| (t.wl_surface().clone(), window_loc.to_f64()));
                            // "Созвездие" (Super+G): остальные окна группы едут вместе.
                            let group_initial = self.constellation_members_excluding(&window)
                                .into_iter()
                                .filter_map(|w| self.space.element_location(&w).map(|l| (w, l)))
                                .collect::<Vec<_>>();
                            let grab = MoveSurfaceGrab::new(
                                GrabStartData { focus, button, location: pos },
                                window.clone(),
                                initial_window_location,
                                group_initial,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Keep);
                            self.request_plane_reset();
                            tracing::debug!("dawn: move grab started");
                            return;
                        }

                        // Super+ПКМ → resize (только в Float)
                        if kb_mods.logo && button == BTN_RIGHT
                            && self.tile_config.layout == Layout::Float
                        {
                            tracing::debug!("dawn: resize grab start");
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
                            // "Созвездие": остальные окна группы масштабируются вместе.
                            let group_initial = self.constellation_members_excluding(&window)
                                .into_iter()
                                .filter_map(|w| self.space.element_geometry(&w).map(|g| (w, g)))
                                .collect::<Vec<_>>();
                            let grab = ResizeSurfaceGrab::start(
                                GrabStartData { focus: None, button, location: pos },
                                window, edge, geo, group_initial,
                                self.tile_config.mfact,
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

                        // Super+Alt+ЛКМ по пустому в обзоре → начать драг стола
                        if self.overview_active && self.logo_held && alt_held && button == BTN_LEFT {
                            if let Some(mask) = self.overview_workspace_at(pos) {
                                self.overview_drag_ws = Some(mask);
                                return;
                            }
                        }

                        // ЛКМ по пустому холсту в Float → rubber-band мультивыделение
                        // (протяжка выделяет пересекающиеся окна, клик без протяжки —
                        // просто снимает выделение, см. select_grab.rs).
                        if button == BTN_LEFT && !alt_held
                            && self.tile_config.layout == Layout::Float
                        {
                            self.clear_selection();
                            let grab = crate::grabs::SelectGrab {
                                start_data: GrabStartData { focus: None, button, location: pos },
                                start_pos: pos,
                            };
                            pointer.set_grab(self, grab, serial, Focus::Clear);
                        }
                    }
                }

                // ── ЛКМ в обзоре: после основного хендлера выходим ──────────────
                if self.overview_active
                    && ButtonState::Pressed == btn_state && button == BTN_LEFT
                    && !alt_held && !self.logo_held
                {
                    let pos = pointer.current_location();
                    let clicked_window = self.space.element_under(pos).map(|(w, _)| w.clone());
                    if let Some(window) = clicked_window {
                        self.exit_overview_to_window(&window);
                    } else {
                        let mask = self.overview_workspace_at(pos);
                        self.exit_overview_immediate(mask);
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
                tracing::trace!("SCROLL: h={:?} v={:?} v120={:?} alt={}", _h_raw, _v_raw, _v120, _alt_check);
                let h = event.amount(Axis::Horizontal)
                    .unwrap_or_else(|| event.amount_v120(Axis::Horizontal).unwrap_or(0.0) * 15.0 / 120.0);
                let v = event.amount(Axis::Vertical)
                    .unwrap_or_else(|| event.amount_v120(Axis::Vertical).unwrap_or(0.0) * 15.0 / 120.0);

                let alt_held = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);

                // Super + 2-палец тачпад-скролл → таскать окно под курсором.
                // ВАЖНО: 2-пальцевое движение по тачпаду libinput шлёт как
                // scroll с source=Finger (жесты GestureSwipe/Pinch — это 3+
                // пальца), поэтому "Super+2 пальца" ловится именно здесь, а не
                // в GestureSwipe. Курсор при этом стоит на месте, окно едет.
                let logo_held = self.logo_held
                    || self.seat.get_keyboard().map(|kb| kb.modifier_state().logo).unwrap_or(false);
                // Жест с зажатым Super отменяет ожидающий тап Super (обзор столов).
                if logo_held {
                    self.super_tap = false;
                }
                if logo_held && source == AxisSource::Finger {
                    // В обзоре столов Super+2пальца ПАНОРАМИРУЮТ ленту (навигация
                    // по столам), а не двигают окно.
                    if self.overview_active {
                        let zoom = self.viewport.zoom;
                        self.viewport.cam_x -= h * 2.5 / zoom;
                        self.viewport.cam_y -= v * 2.5 / zoom;
                        self.apply_camera();
                        self.request_redraw();
                        return;
                    }
                    const TOUCHPAD_MOVE_SPEED: f64 = 2.5;
                    if h == 0.0 && v == 0.0 {
                        // Финальный кадр амплитуды 0 = пальцы отпущены → снимаем латч.
                        // В обзоре — переносим окно на воркспейс бэнда, куда попало.
                        if let Some(w) = self.touchpad_move_window.take() {
                            if self.overview_active {
                                self.overview_reassign(&w);
                            }
                        }
                        return;
                    }
                    // Латчим окно на первом кадре жеста: курсор стоит, а окно
                    // едет и может "выскользнуть" из-под него — поэтому двигаем
                    // именно залатченное окно, пока пальцы не отпущены.
                    let window = match self.touchpad_move_window.clone() {
                        Some(w) => Some(w),
                        None => {
                            let w = self.space.element_under(self.pointer_location)
                                .map(|(w, _)| w.clone());
                            self.touchpad_move_window = w.clone();
                            w
                        }
                    };
                    if let Some(window) = window {
                        let zoom = self.viewport.zoom;
                        let dx = (h * TOUCHPAD_MOVE_SPEED / zoom).round() as i32;
                        let dy = (v * TOUCHPAD_MOVE_SPEED / zoom).round() as i32;
                        if let Some(geo) = self.space.element_geometry(&window) {
                            let new_loc = smithay::utils::Point::from((geo.loc.x + dx, geo.loc.y + dy));
                            self.space.map_element(window.clone(), new_loc, true);
                            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                                tw.window.toplevel().zip(window.toplevel())
                                    .map(|(a, b)| a.wl_surface() == b.wl_surface())
                                    .unwrap_or(false)
                            }) {
                                tw.float_position = new_loc;
                                tw.position = new_loc;
                                tw.float_position_set = true;
                                tw.floating = true; // вытащили из тайлинга — теперь плавающее
                            }
                            // Режим коллизии (Super+S): расталкиваем задетые окна.
                            self.push_colliding_windows(&window);
                            self.request_redraw();
                        }
                    }
                    return;
                }

                // Alt + 2-палец тачпад-скролл → pan холста (новое, как Alt+ЛКМ).
                // Отличаем от колеса мыши по source=Finger, поэтому Alt+колесо
                // по-прежнему зумит как раньше (ветка ниже) — старое поведение
                // не тронуто, это отдельная ветка только для тачпада.
                if alt_held && source == AxisSource::Finger {
                    // Скролл-единицы тачпада заметно мельче, чем raw pixel delta
                    // мыши при Alt+ЛКМ — усиливаем, чтобы скорость ощущалась так же.
                    const TOUCHPAD_PAN_SPEED: f64 = 2.5;
                    if h != 0.0 || v != 0.0 {
                        let zoom = self.viewport.zoom;
                        let dcam_x = h * TOUCHPAD_PAN_SPEED / zoom;
                        let dcam_y = v * TOUCHPAD_PAN_SPEED / zoom;
                        self.viewport.cam_x -= dcam_x;
                        self.viewport.cam_y -= dcam_y;
                        self.momentum.accumulate(
                            smithay::utils::Point::from((-dcam_x, -dcam_y)),
                            event.time_msec(),
                        );
                        self.apply_camera();
                        self.request_redraw();
                    } else {
                        // libinput шлёт финальный кадр с амплитудой 0, когда пальцы
                        // отпущены — это сигнал "стоп", запускаем инерцию отсюда.
                        self.momentum.launch();
                    }
                    return;
                }

                // В обзоре столов колесо мыши ЗУМИТ (в обычном tiling zoom нельзя).
                if self.overview_active && v != 0.0 && source != AxisSource::Finger {
                    let pointer = self.seat.get_pointer().unwrap();
                    let cursor_canvas = pointer.current_location();
                    let old_zoom = self.viewport.zoom;
                    let screen_x = (cursor_canvas.x - self.viewport.cam_x) * old_zoom;
                    let screen_y = (cursor_canvas.y - self.viewport.cam_y) * old_zoom;
                    let factor = if v < 0.0 { 1.1_f64 } else { 0.9_f64 };
                    let new_zoom = (old_zoom * factor).clamp(0.05, 5.0);
                    self.viewport.zoom = new_zoom;
                    self.viewport.cam_x = cursor_canvas.x - screen_x / new_zoom;
                    self.viewport.cam_y = cursor_canvas.y - screen_y / new_zoom;
                    self.apply_camera();
                    self.request_redraw();
                    return;
                }

                // Alt+колесо мыши в Columns (niri) → листаем колонки влево/вправо
                // (тачпадный Alt+2-пальца выше уже ушёл в pan холста).
                if alt_held && self.tile_config.layout == Layout::Columns
                    && source != AxisSource::Finger && (v != 0.0 || h != 0.0)
                {
                    let dir = if v != 0.0 {
                        if v > 0.0 { 1 } else { -1 }
                    } else if h > 0.0 { 1 } else { -1 };
                    self.columns_focus(dir, 0);
                    self.request_redraw();
                    return;
                }

                // Alt+Scroll (колесо мыши) → zoom (только в Float режиме)
                if alt_held && v != 0.0 && self.tile_config.layout == Layout::Float {
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
                    self.request_redraw();
                    tracing::debug!("dawn/canvas: zoom={:.3} cam=({:.1},{:.1})",
                        new_zoom, self.viewport.cam_x, self.viewport.cam_y);
                    return;
                }

                let mut frame = AxisFrame::new(event.time_msec()).source(source);
                if h != 0.0 { frame = frame.value(Axis::Horizontal, h); }
                if v != 0.0 { frame = frame.value(Axis::Vertical, v); }
                self.seat.get_pointer().unwrap().axis(self, frame);
                self.seat.get_pointer().unwrap().frame(self);
            }

            // ── Pinch → zoom canvas ──────────────────────────────────────
            InputEvent::GesturePinchBegin { .. } => {
                self.pinch_last_scale = 1.0;
                // Super+2-пальца pinch → resize окна под курсором (в любом режиме;
                // вытаскиваем из тайлинга во floating, иначе arrange его сожмёт).
                if self.logo_held {
                    let pos = self.pointer_location;
                    self.gesture_resize_window = self.space.element_under(pos).map(|(w, _)| w.clone());
                    if let Some(window) = self.gesture_resize_window.clone() {
                        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                            tw.window.toplevel().zip(window.toplevel())
                                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                                .unwrap_or(false)
                        }) {
                            tw.floating = true;
                        }
                    }
                }
            }

            InputEvent::GesturePinchUpdate { event, .. } => {
                let scale = event.scale();
                if scale <= 0.0 { return; }

                if let Some(window) = self.gesture_resize_window.clone() {
                    let factor = scale / self.pinch_last_scale;
                    self.pinch_last_scale = scale;
                    if let Some(t) = window.toplevel() {
                        let cur = t.with_committed_state(|s| s.and_then(|s| s.size)).unwrap_or((200, 200).into());
                        let new_w = (cur.w as f64 * factor).round().max(50.0) as i32;
                        let new_h = (cur.h as f64 * factor).round().max(50.0) as i32;
                        t.with_pending_state(|s| { s.size = Some((new_w, new_h).into()); });
                        t.send_pending_configure();
                    }
                    self.request_redraw();
                    return;
                }

                let alt_held = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                if !alt_held { return; }
                let factor = scale / self.pinch_last_scale;
                self.pinch_last_scale = scale;

                let pointer = self.seat.get_pointer().unwrap();
                let cursor = pointer.current_location();

                let old_zoom = self.viewport.zoom;
                let new_zoom = (old_zoom * factor).clamp(0.05, 5.0);
                self.viewport.zoom = new_zoom;

                // Якорь под курсором
                let screen_x = (cursor.x - self.viewport.cam_x) * old_zoom;
                let screen_y = (cursor.y - self.viewport.cam_y) * old_zoom;
                self.viewport.cam_x = cursor.x - screen_x / new_zoom;
                self.viewport.cam_y = cursor.y - screen_y / new_zoom;

                self.apply_camera();
                self.request_redraw();
                tracing::debug!("pinch: zoom={:.3}", new_zoom);
            }

            InputEvent::GesturePinchEnd { .. } => {
                self.pinch_last_scale = 1.0;
                if let Some(window) = self.gesture_resize_window.take() {
                    if let Some(t) = window.toplevel() {
                        let size = t.with_committed_state(|s| s.and_then(|s| s.size)).unwrap_or((200, 200).into());
                        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                            tw.window.toplevel().zip(window.toplevel())
                                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                                .unwrap_or(false)
                        }) {
                            tw.float_size = Some(size);
                        }
                    }
                }
            }

            // ── Swipe (2 пальца, любой режим) → pan canvas ───────────────
            InputEvent::GestureSwipeBegin { .. } => {
                // Super+2-палец → перемещение окна под курсором (новый жест).
                if self.logo_held && self.tile_config.layout == Layout::Float {
                    let pos = self.pointer_location;
                    self.gesture_move_window = self.space.element_under(pos).map(|(w, _)| w.clone());
                }
            }

            InputEvent::GestureSwipeUpdate { event, .. } => {
                let delta = event.delta();
                if delta.x == 0.0 && delta.y == 0.0 { return; }

                if let Some(window) = self.gesture_move_window.clone() {
                    if let Some(geo) = self.space.element_geometry(&window) {
                        let new_loc = smithay::utils::Point::from((
                            geo.loc.x + delta.x.round() as i32,
                            geo.loc.y + delta.y.round() as i32,
                        ));
                        self.space.map_element(window.clone(), new_loc, true);
                        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                            tw.window.toplevel().zip(window.toplevel())
                                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                                .unwrap_or(false)
                        }) {
                            tw.float_position = new_loc;
                            tw.position = new_loc;
                            tw.float_position_set = true;
                        }
                    }
                    self.request_redraw();
                    return;
                }

                let alt = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                if !alt { return; }
                // Alt + 2-пальца → pan
                let zoom = self.viewport.zoom;
                let dcam_x = delta.x / zoom;
                let dcam_y = delta.y / zoom;
                self.viewport.cam_x -= dcam_x;
                self.viewport.cam_y -= dcam_y;
                // Курсор остаётся на той же screen-позиции: ptr -= dcam
                self.pointer_location.x -= dcam_x;
                self.pointer_location.y -= dcam_y;
                // Кинетический скролл (1.1): копим дельту для инерции на конец жеста
                self.momentum.accumulate(
                    smithay::utils::Point::from((-dcam_x, -dcam_y)),
                    event.time_msec(),
                );
                self.apply_camera();
                self.request_redraw();
                tracing::debug!("swipe pan: cam=({:.1},{:.1})", self.viewport.cam_x, self.viewport.cam_y);
            }

            InputEvent::GestureSwipeEnd { .. } => {
                if self.gesture_move_window.take().is_some() {
                    return;
                }
                self.momentum.launch();
            }

            _ => {}
        }
    }
}
