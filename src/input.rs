use smithay::{
    backend::input::{
        AbsolutePositionEvent, Axis, AxisSource, ButtonState, Event,
        GesturePinchUpdateEvent,
        GestureSwipeUpdateEvent, InputBackend, InputEvent,
        KeyboardKeyEvent, PointerAxisEvent, PointerButtonEvent, PointerMotionEvent,
    },
    input::{
        keyboard::{FilterResult, keysyms},
        pointer::{AxisFrame, ButtonEvent, Focus, GrabStartData, MotionEvent, RelativeMotionEvent},
    },
    utils::{Point, Rectangle, SERIAL_COUNTER},
};

use crate::{
    grabs::{move_grab::MoveSurfaceGrab, resize_grab::{ResizeEdge, ResizeSurfaceGrab}},
    state::Dawn,
    tiling::Layout,
};

const BTN_LEFT:  u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

impl Dawn {
    /// Можно ли сейчас двигать камеру жестами тачпада (пан и зум щипком).
    ///
    /// Запрещено в двух раскладках, где камера принадлежит не пользователю, а
    /// самой раскладке:
    ///   * Columns (niri) — это полоса колонок, а не бесконечный холст. Вид
    ///     ходит по ней свайпом (columns_swipe_scroll/workspace) и по вертикали
    ///     между столами; свободный пан позволял уехать в пустоту мимо колонок,
    ///     а зум щипком раскладку не проверял ВООБЩЕ и ломал её наравне с паном.
    ///     Свайп по полосе при этом остаётся — он отрабатывает раньше, в своей
    ///     ветке, и до камеры дело не доходит.
    ///   * Tile — окна разложены деревом под размер экрана, возить холст под
    ///     ними незачем.
    /// Во Float и Monocle пан и зум работают как раньше.
    ///
    /// Обзор столов и лупа (Super+Space) — исключение поверх всего: там вид уже
    /// оторван от раскладки и ходит свободно, какой бы она ни была.
    ///
    /// Мышь живёт по своим правилам и здесь не участвует: Alt+колесо зумит
    /// только во Float, Alt+ЛКМ панит во Float и в обзоре — те ветки не тронуты.
    fn touchpad_camera_allowed(&self) -> bool {
        if self.overview_active || self.zoom_nav_mode {
            return true;
        }
        !matches!(self.tile_config.layout, Layout::Tile | Layout::Columns)
    }

    pub(crate) fn kill_focused(&mut self) {
        if let Some(surface) = self.focused_surface() {
            // Ищем сперва в space, потом в собственном списке окон.
            //
            // space держит только то, что сейчас РИСУЕТСЯ: скрытые вкладки
            // Columns с него сняты, окна чужих раскладок и чужих тегов тоже.
            // Фокус же спокойно оказывается на таком окне (после закрытия
            // соседа, после переключения вкладки), и Win+Q молча не делал
            // ничего — в логе 20260729_190042 это 28 промахов на 27 попаданий,
            // причём промах всегда стоит вплотную перед повторным нажатием.
            // Закрытие окна не должно зависеть от того, видно его сейчас или нет.
            let w = self.space.elements()
                .find(|w| crate::xwin::is_surface(w, &surface))
                .cloned()
                .or_else(|| self.tagged_windows.iter()
                    .find(|tw| crate::xwin::is_surface(&tw.window, &surface))
                    .map(|tw| tw.window.clone()));
            if let Some(w) = w {
                crate::xwin::close(&w);
                tracing::info!("dawn: kill focused");
            } else {
                tracing::info!("dawn: kill — фокусная поверхность есть, но окна для неё нет ни в space, ни в списке");
            }
        } else {
            // Win+q молча ничего не делает ровно здесь: клавиатурный фокус снят
            // (например, кликом по пустому холсту, см. ниже set_focus(None)).
            tracing::info!("dawn: kill — фокуса нет, закрывать нечего");
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
                                state.super_tap_shift = modifiers.shift;
                            } else {
                                if state.super_tap {
                                    // Обзор открывает чистый тап Super в ЛЮБОЙ
                                    // раскладке. В Columns дополнительно
                                    // работает и Shift+Super — тап с Shift тоже
                                    // считается тапом (см. ветку Shift ниже),
                                    // так что обе комбинации ведут в обзор.
                                    state.toggle_overview();
                                }
                                state.super_tap = false;
                                state.super_tap_shift = false;
                            }
                            return FilterResult::Forward;
                        }

                        // Shift, нажатый ВО ВРЕМЯ удержания Super, тап не
                        // отменяет — он его модификатор (Shift+Super в
                        // Columns). Любая другая клавиша отменяет как раньше.
                        const SHIFT_L: u32 = keysyms::KEY_Shift_L;
                        const SHIFT_R: u32 = keysyms::KEY_Shift_R;
                        if raw == SHIFT_L || raw == SHIFT_R {
                            if pressed && state.super_tap {
                                state.super_tap_shift = true;
                            }
                            return FilterResult::Forward;
                        }

                        // Отпустили Alt — перебор стопки (Alt+Tab) закончен, и
                        // следующий Tab начнёт новую стопку с текущего окна.
                        // Ловим ДО общего `if !pressed`: отпускание клавиши
                        // ниже никуда не доходит.
                        const ALT_L: u32 = keysyms::KEY_Alt_L;
                        const ALT_R: u32 = keysyms::KEY_Alt_R;
                        const META_L: u32 = keysyms::KEY_Meta_L;
                        const META_R: u32 = keysyms::KEY_Meta_R;
                        if matches!(raw, ALT_L | ALT_R | META_L | META_R) && !pressed {
                            state.cycle_stack_end();
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

                        // Escape во время выбора источника для демонстрации
                        // экрана — отмена (см. portal.rs). Перехватываем до
                        // всех биндов и до клиента: пока идёт выбор, клавиша
                        // принадлежит выбору.
                        if state.portal_picking() && raw == keysyms::KEY_Escape {
                            state.portal_pick_click(true);
                            return FilterResult::Intercept(());
                        }

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

                        // Открытый поиск окон (Super+F) — это поле ввода: пока
                        // он на экране, ГОЛЫЕ клавиши принадлежат ему целиком,
                        // включая буквы, Enter, Tab и Backspace. Комбинации с
                        // модификаторами не трогаем — тем же Super+F поиск и
                        // закрывается, и переключение стола из него работает.
                        //
                        // Символ берём из ТЕКУЩЕЙ раскладки (modified_sym), а не
                        // из латинской: в поле ввода буква — это буква, и
                        // русское имя окна надо уметь набрать по-русски.
                        if state.search_open() && !logo && !ctrl && !alt {
                            let ch = handle.modified_sym().key_char();
                            if state.search_key(raw_latin, ch) {
                                return FilterResult::Intercept(());
                            }
                        }

                        // Открытое меню блютуза забирает ГОЛЫЕ клавиши: стрелки,
                        // Enter и буквы действий принадлежат ему, а не клиенту
                        // под курсором (см. bluetooth.rs). Комбинации с
                        // модификаторами не трогаем — иначе тот же Super+Shift+B,
                        // которым меню открыли, не смог бы его закрыть, а вместе
                        // с ним отвалились бы и переключение стола, и VT.
                        // Раскладка латинская: в русской иначе не сработали бы
                        // D/F/S/P.
                        if state.bt_menu_open() && !logo && !ctrl && !alt {
                            if state.bt_key(raw_latin) {
                                return FilterResult::Intercept(());
                            }
                        }

                        // Меню вайфая и звука — как блютузное: голые клавиши
                        // принадлежат им. Вайфаю нужен ещё и СИМВОЛ: в поле
                        // пароля буквы это буквы, а не команды.
                        if state.wifi_menu_open() && !logo && !ctrl && !alt {
                            let ch = handle.modified_sym().key_char();
                            if state.wifi_key(raw_latin, ch) {
                                return FilterResult::Intercept(());
                            }
                        }
                        if state.audio_menu_open() && !logo && !ctrl && !alt {
                            if state.audio_key(raw_latin) {
                                return FilterResult::Intercept(());
                            }
                        }

                        // Открытая полка забирает только Esc — остальные
                        // клавиши ей не нужны, и отнимать их у клиента незачем.
                        if !logo && !ctrl && !alt && state.tray_key(raw_latin) {
                            return FilterResult::Intercept(());
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

               // ── Захват курсора приложением (см. capture.rs) ───────────────
               // Относительные дельты уходят клиенту ВСЕГДА, а не только при
               // захвате: игры подписываются на них заранее и ведут по ним
               // обзор, пока курсор ещё свободен.
               //
               // Ускоренная дельта делится на зум — она в тех же единицах, что
               // и движение курсора по холсту (surface-локальных). Сырая
               // (delta_unaccel) идёт как есть: это «сколько прошла мышь»,
               // масштаб холста к ней отношения не имеет, и игры считают по ней
               // своё ускорение.
               let под = self.surface_under(self.pointer_location);
               let захват = self.pointer_constraint_at(под.as_ref());
               {
                   let pointer = self.seat.get_pointer().unwrap();
                   let unaccel = event.delta_unaccel();
                   pointer.relative_motion(self, под, &RelativeMotionEvent {
                       delta: (delta.x / zoom, delta.y / zoom).into(),
                       delta_unaccel: (unaccel.x, unaccel.y).into(),
                       utime: event.time(),
                   });
                   // Курсор заперт на месте (мышиный обзор в игре): позицию не
                   // трогаем вовсе — ни стрелку, ни камеру. Клиент уже получил
                   // всё, что ему нужно, дельтами выше.
                   if захват.locked {
                       pointer.frame(self);
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
                   // Камера + курсор одним шагом: стрелка обязана остаться в
                   // той же точке экрана уже в ЭТОМ кадре (см. pan_camera_by).
                   self.pan_camera_by(dcam_x, dcam_y);
                   // Кинетический скролл (1.1): копим дельту для инерции на отпускание
                   self.momentum.accumulate(
                       smithay::utils::Point::from((-dcam_x, -dcam_y)),
                       event.time_msec(),
                   );
                   // Замер строго на ЭТОМ жесте (Alt+ЛКМ): печатаем экранную
                   // точку стрелки на каждом событии пана, не чаще 60 строк за
                   // жест. Если тут число стоит, а стрелку видно едущей —
                   // причина не в позиции курсора.
                   if self.pan_log_left > 0 {
                       self.pan_log_left -= 1;
                       let s = self.pointer_screen_physical();
                       tracing::debug!(
                           "ПАН Alt+ЛКМ: курсор_экран=({:.1},{:.1}) камера=({:.1},{:.1}) дельта=({:.1},{:.1})",
                           s.x, s.y, self.viewport.cam_x, self.viewport.cam_y, delta.x, delta.y,
                       );
                   }
                   self.request_redraw();
                   return;
               }

               // Обычное движение курсора — дельта в canvas-единицах
               let было = self.pointer_location;
               self.pointer_location.x += delta.x / zoom;
               self.pointer_location.y += delta.y / zoom;

               // Курсор не должен выходить за экран: зажимаем, переливаем в камеру.
               // Координаты в ЛОГИЧЕСКИХ единицах (output-local), без умножения на zoom.
               {
                   {
                       // Логическая позиция курсора относительно камеры и предел —
                       // ВИДИМАЯ часть холста (экран ⁄ зум), а не размер выхода:
                       // при отдалении в кадре холста больше, чем экрана.
                       let sx = self.pointer_location.x - self.viewport.cam_x;
                       let sy = self.pointer_location.y - self.viewport.cam_y;
                       let vis = self.visible_canvas_size();
                       let ow = vis.w;
                       let oh = vis.h;
                       let csx = sx.clamp(0.0, ow);
                       let csy = sy.clamp(0.0, oh);
                       // Зажимаем курсор (canvas coords)
                       self.pointer_location.x = csx + self.viewport.cam_x;
                       self.pointer_location.y = csy + self.viewport.cam_y;
                       // Перелив → плавное движение камеры (только Float).
                       //
                       // Полноэкранное окно — исключение: экран отдан ему
                       // целиком, и «дотолкать» камеру курсором у края значило
                       // бы уехать холстом из-под игры прямо во время игры.
                       // Пока фуллскрин держит экран, камера стоит.
                       if self.tile_config.layout == Layout::Float
                           && self.fullscreen.is_none()
                       {
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

               // Удержание (confine): курсор не выпускается за поверхность и за
               // заданную клиентом область внутри неё. Если шаг вывел наружу —
               // откатываем его целиком. Скользить вдоль границы не пытаемся:
               // область произвольной формы, а откат по обеим осям сразу даёт
               // ровно то поведение, которого ждёт клиент — «дальше некуда».
               if захват.confined && !захват.holds(self, self.pointer_location) {
                   self.pointer_location = было;
               }

               let pos = self.pointer_location;
               let serial = SERIAL_COUNTER.next_serial();
               let pointer = self.seat.get_pointer().unwrap();
               let under = self.surface_under(pos);

               // Sloppy focus — но НЕ в Columns: там вид ездит по полосе от
               // клавиш и жестов, курсор при этом стоит на месте экрана (это
               // видно в логе: «КАДР: курсор_экран» неизменен, пока привязка
               // окон уезжает), и под стрелкой оказывается чужая колонка. С
               // sloppy focus первое же шевеление мыши перебрасывало фокус
               // туда, куда пользователь не смотрел. У niri focus-follows-mouse
               // по той же причине выключен по умолчанию; остальные раскладки
               // dawn работают как раньше.
               // Пока открыто меню клиента, оно держит захват (см.
               // XdgShellHandler::grab). Перевод фокуса под курсором в этот
               // момент сорвал бы захват и закрыл меню — ровно то, чем болели
               // меню Steam (там причина была та же, но по стороне X11).
               let меню_захватило = self.seat.get_keyboard().is_some_and(|k| k.is_grabbed());
               let sloppy = self.tile_config.layout != Layout::Columns && !меню_захватило;
               if sloppy {
                   if let Some((surface, _)) = &under {
                       let same = self.focused_surface().as_ref() == Some(surface);
                       if !same {
                           if let Some((window, _)) = self.space
                               .element_under(pos).map(|(w, l)| (w.clone(), l))
                           {
                               crate::xwin::focus(self, &window);
                           }
                       }
                   }
               }
               // Surface-local всегда в логическом пространстве — клиент сам
               // умножает на scale (wl_output.scale / wp_fractional_scale),
               // чтобы получить пиксель буфера. zoom_adjusted_location_motion
               // дважды применял scale — surface-local уходил сжатым.
               pointer.motion(self, under, &MotionEvent {
                   location: pos, serial, time: event.time_msec(),
               });
               pointer.frame(self);
               // Это движение — от настоящей мыши, и оно уже разослано. Фиксируем
               // новую экранную позицию как эталонную, иначе sync_pointer_to_camera
               // в конце итерации сочтёт сдвиг камеры от "перелива" у края (выше)
               // самовольным и оттащит стрелку обратно — курсор резинил бы у края.
               self.pointer_warped();
               // Курсор въехал в область, на которую клиент заранее попросил
               // захват, — включаем его (до этого ограничение висит неактивным).
               self.activate_pointer_constraint();
               // Курсор client-side — его позицию перерисовывает только сам
               // рендер; без явного пинка тут курсор будет виден в последнем
               // отрендеренном кадре, а не там, где мышь реально находится.
               self.request_redraw();
           }

            InputEvent::PointerMotionAbsolute { event, .. } => {
                tracing::trace!("PTR MOTION ABS");
                // Абсолютная позиция (планшет/тачскрин) приходит в долях экрана —
                // разворачиваем её именно по ЭКРАНУ, а дальше переводим в холст
                // общей формулой. Раньше здесь брался размер выхода, который
                // сам уже был поделён на зум, и деление на зум ниже давало
                // двойную поправку: абсолютный ввод мазал тем сильнее, чем
                // дальше зум от единицы.
                let zoom = self.viewport.zoom;
                let cam_x = self.viewport.cam_x;
                let cam_y = self.viewport.cam_y;
                // Курсор заперт клиентом (см. capture.rs) — абсолютный ввод его
                // не двигает: планшет или указатель виртуальной машины иначе
                // выдернул бы стрелку из захвата, и игра потеряла бы обзор.
                let под = self.surface_under(self.pointer_location);
                let захват = self.pointer_constraint_at(под.as_ref());
                if захват.locked {
                    return;
                }
                let screen_pos = event.position_transformed(self.screen_size());
                let mut pos = smithay::utils::Point::<f64, smithay::utils::Logical>::from((
                    screen_pos.x / zoom + cam_x,
                    screen_pos.y / zoom + cam_y,
                ));
                // Удержание внутри поверхности: точку вне её просто не
                // принимаем — стрелка остаётся там, где была.
                if захват.confined && !захват.holds(self, pos) {
                    pos = self.pointer_location;
                }
                self.pointer_location = pos;
                let serial = SERIAL_COUNTER.next_serial();
                let pointer = self.seat.get_pointer().unwrap();
                let under = self.surface_under(pos);

                // sloppyfocus: focus follows cursor (как в dwl sloppyfocus=1).
                // В Columns выключен — см. подробный комментарий в ветке
                // PointerMotion выше.
                // Пока клавиатуру держит слой (открыт лаунчер), sloppy-focus
                // молчит: иначе первое же движение мыши отдавало бы ввод окну
                // под курсором, и лаунчер снова оставался немым.
                if self.tile_config.layout != Layout::Columns && self.layer_keyboard.is_none() {
                    if let Some((surface, _)) = &under {
                    let same = self.focused_surface().as_ref() == Some(surface);
                    if !same {
                        // Найти окно под курсором и активировать
                        if let Some((window, _)) = self.space
                            .element_under(pos)
                            .map(|(w, l)| (w.clone(), l))
                        {
                            crate::xwin::focus(self, &window);
                        }
                    }
                    }
                }

                pointer.motion(self, under, &MotionEvent {
                    location: pos, serial, time: event.time_msec(),
                });
                pointer.frame(self);
                // Абсолютный ввод (планшет/VM) задаёт позицию курсора прямо в
                // экранных координатах — она и есть эталон для синхронизации.
                self.pointer_warped();
                self.activate_pointer_constraint();
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

                // Курсор захвачен приложением (игра запросила pointer-lock или
                // confine, см. capture.rs) — весь ввод принадлежит ему, и ни
                // один оверлей компоновщика клик не перехватывает. При locked
                // стрелка стоит там, где её заперли: если это оказалась зона
                // полки или миникарты, КАЖДЫЙ выстрел в игре уходил бы в
                // компоновщик, а не в игру.
                let курсор_у_клиента = {
                    let под = self.surface_under(self.pointer_location);
                    let захват = self.pointer_constraint_at(под.as_ref());
                    захват.locked || захват.confined
                };

                // Идёт выбор источника для демонстрации экрана: клик выбирает
                // окно под курсором (пустой холст = весь экран), правая кнопка
                // отменяет. Съедаем клик целиком — он не должен ни
                // фокусировать, ни двигать окна. См. portal.rs.
                // Меню блютуза приклеено к экрану, поэтому и клик по нему
                // считается в экранных пикселях (см. bt_click).
                // Замер «клик не сработал»: каждый выход ОТСЮДА означает, что
                // приложение своего клика не увидело. Пока причина промахов не
                // найдена, важно отличать «клик съел компоновщик» (тогда виден
                // виновник и точка экрана) от «клик дошёл, но приложение его
                // проигнорировало» — во втором случае здесь тихо.
                let съел = |кто: &str, s: smithay::utils::Point<f64, smithay::utils::Physical>| {
                    tracing::info!("КЛИК СЪЕДЕН: {} экран=({:.0},{:.0})", кто, s.x, s.y);
                };

                if ButtonState::Pressed == btn_state && !курсор_у_клиента && self.bt_menu_open() {
                    let screen = self.pointer_screen_physical();
                    if self.bt_click(screen) {
                        съел("меню блютуза", screen);
                        return;
                    }
                }

                // Поиск окон (Super+F) — такое же приклеенное к экрану меню.
                if ButtonState::Pressed == btn_state && !курсор_у_клиента && self.search_open() {
                    let screen = self.pointer_screen_physical();
                    if self.search_click(screen) {
                        съел("поиск окон", screen);
                        return;
                    }
                }

                // Меню вайфая и звука приклеены к экрану, как и блютузное.
                if ButtonState::Pressed == btn_state && !курсор_у_клиента
                    && (self.wifi_menu_open() || self.audio_menu_open()) {
                    let screen = self.pointer_screen_physical();
                    if self.wifi_click(screen) || self.audio_click(screen) {
                        съел("меню вайфая/звука", screen);
                        return;
                    }
                }

                // Полка состояния приклеена к экрану так же, как бар. Клик
                // мимо неё она НЕ съедает (см. tray_click) — окно под курсором
                // получит его как обычно. Правая кнопка по значку — быстрое
                // действие вместо меню (радио, немота).
                if ButtonState::Pressed == btn_state && !курсор_у_клиента {
                    let screen = self.pointer_screen_physical();
                    if self.tray_click(screen, button == BTN_RIGHT) {
                        съел("полка состояния", screen);
                        return;
                    }
                }

                // Панель столов приклеена к экрану так же, как полка рядом:
                // клик по столбику переводит на этот стол. Мимо панели клик не
                // съедается — окно под ней получит его как обычно.
                if ButtonState::Pressed == btn_state && !курсор_у_клиента && button == BTN_LEFT {
                    let screen = self.pointer_screen_physical();
                    if self.bar_click(screen) {
                        съел("панель столов", screen);
                        return;
                    }
                }

                if ButtonState::Pressed == btn_state && self.portal_picking() {
                    if self.portal_pick_click(button == BTN_RIGHT) {
                        съел("выбор источника демонстрации", self.pointer_screen_physical());
                        return;
                    }
                }

                // Клик по миникарте: телепорт к окну под курсором.
                if ButtonState::Pressed == btn_state && !курсор_у_клиента
                    && self.try_handle_minimap_click() {
                    съел("миникарта", self.pointer_screen_physical());
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
                // Сами столы в обзоре НЕ двигаются: их порядок задаёт обзор
                // (ячейки выдаются кольцами вокруг текущего, см. overview.rs).
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
                    // ПКМ без Super → выход на стол под курсором.
                    // Super+ПКМ → падаем в основной хендлер (resize окна в обзоре).
                    if ButtonState::Pressed == btn_state && button == BTN_RIGHT
                        && !self.logo_held
                    {
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
                    // Новый жест — новая порция строк замера.
                    self.pan_log_left = 60;
                    self.pan_start_screen = Some(self.pointer_screen_physical());
                    tracing::debug!("dawn/canvas: pan started");
                    return;
                }
                // Любое отпускание ЛКМ → завершаем pan, запускаем инерцию (1.1)
                if ButtonState::Released == btn_state && button == BTN_LEFT {
                    if self.pan_button_held {
                        self.momentum.launch();
                        // Итог жеста: где стрелка была в начале и где оказалась.
                        // Ненулевая разница здесь — это и есть «уезжает и
                        // остаётся смещённой» (в отличие от дрожи, у которой
                        // итог около нуля).
                        if let Some(start) = self.pan_start_screen.take() {
                            let now = self.pointer_screen_physical();
                            tracing::debug!(
                                "ИТОГ ПАН: старт=({:.1},{:.1}) конец=({:.1},{:.1}) смещение=({:.1},{:.1})",
                                start.x, start.y, now.x, now.y, now.x - start.x, now.y - start.y,
                            );
                        }
                    }
                    self.pan_button_held = false;
                }

                // Висящий pointer-grab съедает Win+ЛКМ/Win+ПКМ целиком: ветка
                // ниже под него не заходит, и клик выглядит "не работает".
                if ButtonState::Pressed == btn_state && pointer.is_grabbed() {
                    // Висящий grab — вторая причина «клик не сработал»: событие
                    // уходит владельцу захвата, а не тому, на что человек
                    // показывает. Печатаем экранную точку, чтобы в логе было
                    // видно, где именно это случается.
                    let s = self.pointer_screen_physical();
                    tracing::info!(
                        "КЛИК В ЗАХВАТ: активен pointer grab, экран=({:.0},{:.0})", s.x, s.y,
                    );
                }

                if ButtonState::Pressed == btn_state && !pointer.is_grabbed() {
                    // Берём НАШУ позицию курсора — ту же, по которой рисуется
                    // стрелка. current_location() внутри smithay обновляется
                    // только из pointer.motion; пока курсор двигали записью в
                    // pointer_location, две позиции расходились, и клик уходил
                    // не туда, куда показывает стрелка. Теперь все переносы идут
                    // через warp_pointer (motion рассылается), так что значения
                    // совпадают — а если разъедутся, это видно в логе ниже.
                    let pos = self.pointer_location;
                    if cfg!(debug_assertions) || tracing::enabled!(tracing::Level::DEBUG) {
                        let smithay_pos = pointer.current_location();
                        let hit = self.space.element_under(pos)
                            .and_then(|(w, _)| self.space.element_geometry(w));
                        // Отставание КАРТИНКИ: где стрелку нарисовал последний
                        // ушедший на монитор кадр против того, где курсор сейчас.
                        // Ненулевое значение здесь и есть «кликаю не туда, где
                        // вижу стрелку» — хит-тест при этом может быть точен.
                        let кадр = self.frame_cursor;
                        tracing::debug!(
                            "PTR КАДР: стрелка_в_кадре=({:.1},{:.1}) сейчас=({:.1},{:.1}) \
                             отставание=({:.1},{:.1}) возраст_кадра={}мс",
                            кадр.x, кадр.y, pos.x, pos.y,
                            pos.x - кадр.x, pos.y - кадр.y,
                            self.frame_drawn_at.elapsed().as_millis(),
                        );
                        tracing::debug!(
                            "PTR HIT: курсор=({:.1},{:.1}) smithay=({:.1},{:.1}) \
                             расхождение=({:.1},{:.1}) камера=({:.1},{:.1}) zoom={:.2} окно={:?}",
                            pos.x, pos.y, smithay_pos.x, smithay_pos.y,
                            pos.x - smithay_pos.x, pos.y - smithay_pos.y,
                            self.viewport.cam_x, self.viewport.cam_y, self.viewport.zoom, hit,
                        );
                        // Промах ВНУТРИ окна (окно то, а кнопка нажимается со
                        // сдвигом) виден только здесь: печатаем точку, которую
                        // клиент получает в СВОИХ координатах, рядом с тем,
                        // какого размера мы это окно считаем и какого размера
                        // поверхность клиент реально закоммитил. Если
                        // локальная точка и размеры сходятся, а промах есть —
                        // мажет клиент (наш zoom он видит как wl_output.scale),
                        // и лечить надо не хит-тест.
                        self.log_pointer_local(pos);
                    }

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
                            let focus = crate::xwin::surface(&window)
                                .map(|s| (s, window_loc.to_f64()));
                            // Вместе едет ВЫДЕЛЕНИЕ (если окно в него входит), а не
                            // созвездие: тянешь одно окно созвездия — едет только оно.
                            let group_initial = self.group_drag_members_excluding(&window)
                                .into_iter()
                                .filter_map(|w| self.space.element_location(&w).map(|l| (w, l)))
                                .collect::<Vec<_>>();
                            // Окно (и члены созвездия) могли ещё лететь после
                            // прошлой анимации — снимаем её, иначе она каждый
                            // тик тянула бы окно на свою траекторию и оно
                            // «резинилось» бы под курсором во время драга.
                            self.freeze_window_anim(&window);
                            for (w, _) in &group_initial {
                                self.freeze_window_anim(w);
                            }
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

                        // Super+ПКМ → resize. Float: свободный ресайз. Tile:
                        // тянем деления BSP-дерева (Hyprland smart_resizing,
                        // см. dwindle.rs). Columns: ширину активной
                        // колонки. Обзор: свободный ресайз миниатюры (см.
                        // resize_grab.rs, ветка overview_active). Monocle: no-op.
                        if kb_mods.logo && button == BTN_RIGHT {
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
                            // Вместе масштабируется ВЫДЕЛЕНИЕ (см. move-грab выше).
                            let group_initial = self.group_drag_members_excluding(&window)
                                .into_iter()
                                .filter_map(|w| self.space.element_geometry(&w).map(|g| (w, g)))
                                .collect::<Vec<_>>();
                            // Ресайз двигает окно напрямую (LEFT/TOP-края) —
                            // недолетевшая анимация позиции дралась бы с ним.
                            self.freeze_window_anim(&window);
                            for (w, _) in &group_initial {
                                self.freeze_window_anim(w);
                            }
                            let grab = ResizeSurfaceGrab::start(
                                GrabStartData { focus: None, button, location: pos },
                                window, edge, geo, group_initial,
                            );
                            pointer.set_grab(self, grab, serial, Focus::Keep);
                            return;
                        }

                        // Клик → focus
                        crate::xwin::focus(self, &window);
                    } else {
                        if kb_mods.logo {
                            // Win+ЛКМ/Win+ПКМ пришли по пустому месту: под нашей
                            // точкой курсора окна нет (хотя стрелка может рисоваться
                            // поверх окна — тогда разъехались координаты).
                            tracing::debug!(
                                "PTR: Win+клик без окна под курсором ({:.1},{:.1})", pos.x, pos.y
                            );
                        }
                        // «Окна под курсором нет» — ещё не «под курсором пусто».
                        // Layer-поверхности (меню обоев dwall, лаунчер) в space
                        // не лежат, а рисуются поверх окон. Раньше клик по ним
                        // попадал сюда: фокус снимался, а ЛКМ вдобавок уходила
                        // в rubber-band c Focus::Clear и до клиента не доходила
                        // ВООБЩЕ — в меню dwall работала только ПКМ, потому что
                        // она grab не создаёт и проваливалась в общую пересылку
                        // ниже. Отдаём такой клик слою и не трогаем фокус.
                        if self.курсор_над_слоем(pos) {
                            pointer.button(self, &ButtonEvent {
                                button, state: btn_state, serial, time: event.time_msec(),
                            });
                            pointer.frame(self);
                            return;
                        }

                        let all: Vec<_> = self.space.elements().cloned().collect();
                        for w in all {
                            w.set_activated(false);
                            crate::xwin::configure(&w);
                        }
                        keyboard.set_focus(self, None, serial);

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
                    // Наша позиция курсора — та же, по которой рисуется стрелка
                    // (см. комментарий выше о расхождении с smithay).
                    let pos = self.pointer_location;
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
                        // Тот же класс, что Alt+ЛКМ: пан рукой обязан удержать
                        // стрелку на экране в этом же кадре (см. pan_camera_by).
                        self.pan_camera_by(h * 2.5 / zoom, v * 2.5 / zoom);
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
                    // Латчим окно на первом кадре жеста: даже с курсором,
                    // который едет следом (ниже), у края экрана он упирается в
                    // границу и окно из-под него всё равно выскользнуло бы —
                    // поэтому двигаем именно залатченное окно, пока пальцы не
                    // отпущены.
                    let window = match self.touchpad_move_window.clone() {
                        Some(w) => Some(w),
                        None => {
                            let w = self.space.element_under(self.pointer_location)
                                .map(|(w, _)| w.clone());
                            // Снимаем анимации с окна И со всех, кто поедет с
                            // ним (выделение, созвездие). Иначе недоигранный
                            // доезд от прошлого действия каждый тик тянет окно
                            // на свою траекторию, и под пальцами оно
                            // «резинится» — то самое ощущение инерции.
                            //
                            // При перетаскивании мышью это уже делалось (см.
                            // ветку Super+ЛКМ), а тачпадный жест был забыт.
                            if let Some(ref w) = w {
                                self.freeze_window_anim(w);
                                for m in self.group_drag_members_excluding(w) {
                                    self.freeze_window_anim(&m);
                                }
                            }
                            self.touchpad_move_window = w.clone();
                            tracing::debug!(
                                "ЖЕСТ: Super+2пальца → перенос окна, окно под курсором={} \
                                 выделено={}",
                                w.is_some(),
                                w.as_ref().map(|w| self.is_selected(w)).unwrap_or(false),
                            );
                            w
                        }
                    };
                    if let Some(window) = window {
                        // Свободно возить окно можно ТОЛЬКО когда оно и так
                        // плавающее: либо вся раскладка Float, либо окно
                        // сделали плавающим руками (Super+V, float_selected).
                        //
                        // Раньше проверки не было вовсе, а ниже стояло
                        // `tw.floating = true` — жест молча вырывал окно из
                        // тайлинга в любой раскладке. В Tile и в ленте Columns
                        // это ломало саму модель: раскладка перестаёт быть
                        // раскладкой, если любое случайное движение двумя
                        // пальцами выкидывает из неё окно.
                        let плавающее = self.tile_config.layout == Layout::Float
                            || self.tagged_windows.iter()
                                .find(|tw| tw.window == window)
                                .map(|tw| tw.floating)
                                .unwrap_or(false);
                        if !плавающее {
                            self.touchpad_move_window = None;
                            return;
                        }
                        let zoom = self.viewport.zoom;
                        let dx = (h * TOUCHPAD_MOVE_SPEED / zoom).round() as i32;
                        let dy = (v * TOUCHPAD_MOVE_SPEED / zoom).round() as i32;
                        if let Some(geo) = self.space.element_geometry(&window) {
                            let new_loc = smithay::utils::Point::from((geo.loc.x + dx, geo.loc.y + dy));
                            self.space.map_element(window.clone(), new_loc, true);
                            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                                tw.window == window
                            }) {
                                tw.float_position = new_loc;
                                tw.position = new_loc;
                                tw.float_position_set = true;
                                // floating НЕ ставим: до сюда доходят только уже
                                // плавающие окна (см. проверку выше).
                            }
                            // Выделение едет вместе с окном — тем же смещением
                            // (см. group_drag_members_excluding).
                            for w in self.group_drag_members_excluding(&window) {
                                let Some(g) = self.space.element_geometry(&w) else { continue };
                                let loc = smithay::utils::Point::from((g.loc.x + dx, g.loc.y + dy));
                                self.space.map_element(w.clone(), loc, false);
                                if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == w) {
                                    tw.float_position = loc;
                                    tw.position = loc;
                                    tw.float_position_set = true;
                                    // floating НЕ ставим — по той же причине.
                                }
                            }
                            // Курсор едет ЗА окном: жест таскает окно, и стрелка
                            // должна оставаться в той же его точке, как при
                            // обычном драге мышью. Иначе окно уезжает из-под
                            // курсора, а следующий клик/жест цепляет уже не его.
                            self.warp_pointer(Point::from((
                                self.pointer_location.x + dx as f64,
                                self.pointer_location.y + dy as f64,
                            )));
                            // Режим коллизии (Super+S): расталкиваем задетые окна.
                            self.push_colliding_windows(&window);
                            self.request_redraw();
                        }
                    }
                    return;
                }

                // Alt + 2-палец тачпад-скролл в ленте (Columns/niri) → прокрутка
                // самой полосы: горизонталь везёт вид по колонкам, вертикаль
                // листает столы — ровно то же, что делает голый свайп 3+
                // пальцами (см. GestureSwipeUpdate). Свободного пана в ленте нет
                // (ветка ниже её и исключает), а без модификатора 2 пальца
                // обязаны уходить в приложение — поэтому жест повешен на Alt.
                if alt_held && source == AxisSource::Finger
                    && self.tile_config.layout == Layout::Columns
                    && !self.overview_active
                {
                    // Единицы скролла тачпада мельче пиксельной дельты свайпа —
                    // тот же коэффициент, что у Alt+пана ниже.
                    const TOUCHPAD_PAN_SPEED: f64 = 2.5;
                    if h == 0.0 && v == 0.0 {
                        // Финальный кадр амплитуды 0 = пальцы отпущены →
                        // прилипаем к ближайшей колонке, как в niri.
                        self.columns_swipe_end();
                    } else if h.abs() >= v.abs() {
                        self.columns_swipe_scroll(h * TOUCHPAD_PAN_SPEED);
                    } else {
                        self.columns_swipe_workspace(v * TOUCHPAD_PAN_SPEED);
                    }
                    self.request_redraw();
                    return;
                }

                // Alt + 2-палец тачпад-скролл → pan холста.
                // Отличаем от колеса мыши по source=Finger, поэтому Alt+колесо
                // по-прежнему зумит как раньше (ветка ниже) — старое поведение
                // не тронуто, это отдельная ветка только для тачпада.
                // Разрешено ТОЛЬКО в обзоре и лупе, см. touchpad_camera_allowed.
                if alt_held && source == AxisSource::Finger && self.touchpad_camera_allowed() {
                    // Скролл-единицы тачпада заметно мельче, чем raw pixel delta
                    // мыши при Alt+ЛКМ — усиливаем, чтобы скорость ощущалась так же.
                    const TOUCHPAD_PAN_SPEED: f64 = 2.5;
                    if h != 0.0 || v != 0.0 {
                        let zoom = self.viewport.zoom;
                        let dcam_x = h * TOUCHPAD_PAN_SPEED / zoom;
                        let dcam_y = v * TOUCHPAD_PAN_SPEED / zoom;
                        self.pan_camera_by(dcam_x, dcam_y);
                        self.momentum.accumulate(
                            smithay::utils::Point::from((-dcam_x, -dcam_y)),
                            event.time_msec(),
                        );
                        if self.pan_log_left > 0 {
                            self.pan_log_left -= 1;
                            let s = self.pointer_screen_physical();
                            tracing::debug!(
                                "ПАН Alt+2пальца: курсор_экран=({:.1},{:.1}) камера=({:.1},{:.1}) дельта=({:.1},{:.1})",
                                s.x, s.y, self.viewport.cam_x, self.viewport.cam_y, h, v,
                            );
                        }
                        self.request_redraw();
                    } else {
                        // libinput шлёт финальный кадр с амплитудой 0, когда пальцы
                        // отпущены — это сигнал "стоп", запускаем инерцию отсюда.
                        tracing::debug!("ЖЕСТ: Alt+2пальца → пан холста завершён, инерция");
                        self.momentum.launch();
                        self.pan_log_left = 60;
                    }
                    return;
                }

                // В обзоре столов колесо мыши ЗУМИТ (в обычном tiling zoom нельзя).
                if self.overview_active && v != 0.0 && source != AxisSource::Finger {
                    let cursor_canvas = self.pointer_location;
                    let old_zoom = self.viewport.zoom;
                    let screen_x = (cursor_canvas.x - self.viewport.cam_x) * old_zoom;
                    let screen_y = (cursor_canvas.y - self.viewport.cam_y) * old_zoom;
                    let factor = if v < 0.0 { 1.1_f64 } else { 0.9_f64 };
                    let new_zoom = (old_zoom * factor).clamp(0.05, 5.0);
                    self.viewport.zoom = new_zoom;
                    self.viewport.cam_x = cursor_canvas.x - screen_x / new_zoom;
                    self.viewport.cam_y = cursor_canvas.y - screen_y / new_zoom;
                    self.apply_camera();
                    // Камера пересчитана ОТ курсора — его экранная точка не
                    // изменилась по построению. Помечаем как намеренную, иначе
                    // sync_pointer_to_camera увидит уехавшую камеру и на каждый
                    // щелчок колеса пошлёт клиенту лишний motion.
                    self.pointer_warped();
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
                    let cursor_canvas = self.pointer_location;

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
                // Дискретные щелчки колеса (v120) обязаны уходить вместе со
                // значением. Xwayland переводит колесо в X11-кнопки 4/5 по
                // щелчкам, а не по непрерывной амплитуде: без v120 прокрутка
                // доходит до нативных wayland-клиентов и пропадает во всех
                // X11-приложениях. Тачпад (source=Finger) v120 не имеет —
                // там остаётся только непрерывное значение.
                if source == AxisSource::Wheel || source == AxisSource::WheelTilt {
                    if let Some(dh) = event.amount_v120(Axis::Horizontal) {
                        if dh != 0.0 { frame = frame.v120(Axis::Horizontal, dh as i32); }
                    }
                    if let Some(dv) = event.amount_v120(Axis::Vertical) {
                        if dv != 0.0 { frame = frame.v120(Axis::Vertical, dv as i32); }
                    }
                }
                // Финальный кадр жеста (амплитуда 0) — это axis_stop. Без него
                // клиент считает прокрутку незавершённой и продолжает кинетику.
                if source == AxisSource::Finger {
                    if h == 0.0 { frame = frame.stop(Axis::Horizontal); }
                    if v == 0.0 { frame = frame.stop(Axis::Vertical); }
                }
                let ptr = self.seat.get_pointer().unwrap();
                // Прокрутка уходит ТОЛЬКО в поверхность под указателем. Если
                // здесь `фокус=нет`, колесо «не работает» не из-за самих
                // событий: курсор просто вне окна (например, прижат к краю
                // экрана ниже нижней границы окна) — смотреть надо туда, а не
                // в ветку axis.
                if tracing::enabled!(tracing::Level::DEBUG) {
                    tracing::debug!(
                        "SCROLL→КЛИЕНТ: h={:.2} v={:.2} v120={:?} источник={:?} фокус={}",
                        h, v, event.amount_v120(Axis::Vertical), source,
                        if ptr.current_focus().is_some() { "есть" } else { "нет" },
                    );
                }
                ptr.axis(self, frame);
                ptr.frame(self);
            }

            // ── Pinch → zoom canvas ──────────────────────────────────────
            InputEvent::GesturePinchBegin { .. } => {
                tracing::debug!("ЖЕСТ: pinch начат, logo_held={}", self.logo_held);
                self.pinch_last_scale = 1.0;
                // Super+2-пальца pinch → resize окна под курсором (в любом режиме;
                // вытаскиваем из тайлинга во floating, иначе arrange его сожмёт).
                if self.logo_held {
                    let pos = self.pointer_location;
                    self.gesture_resize_window = self.space.element_under(pos).map(|(w, _)| w.clone());
                    if let Some(window) = self.gesture_resize_window.clone() {
                        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                            tw.window == window
                        }) {
                            tw.floating = true;
                        }
                        // Опорные размеры на весь жест — от них считаем
                        // абсолютный scale (см. gesture_resize_group). Вместе с
                        // окном ресайзится выделение, если окно в нём состоит.
                        let mut group = vec![window.clone()];
                        group.extend(self.group_drag_members_excluding(&window));
                        self.gesture_resize_group = group.into_iter()
                            .map(|w| { let s = crate::xwin::current_size(&w); (w, s) })
                            .collect();
                    }
                }
            }

            InputEvent::GesturePinchUpdate { event, .. } => {
                let scale = event.scale();
                if scale <= 0.0 { return; }

                if let Some(window) = self.gesture_resize_window.clone() {
                    // Размеры считаются от опорных по АБСОЛЮТНОМУ scale жеста
                    // (см. Dawn::gesture_resize_group), а показатель PINCH_GAIN
                    // усиливает жест: пальцы на тачпаде разводятся максимум
                    // раза в полтора, а окно должно успевать вырасти вдвое.
                    const PINCH_GAIN: f64 = 3.0;
                    self.pinch_last_scale = scale;
                    if self.gesture_resize_group.is_empty() {
                        let s = crate::xwin::current_size(&window);
                        self.gesture_resize_group = vec![(window.clone(), s)];
                    }
                    let factor = scale.powf(PINCH_GAIN).clamp(0.05, 20.0);
                    for (w, base) in self.gesture_resize_group.clone() {
                        let new_w = (base.w as f64 * factor).round().clamp(50.0, 20000.0) as i32;
                        let new_h = (base.h as f64 * factor).round().clamp(50.0, 20000.0) as i32;
                        crate::xwin::set_size(&w, Some((new_w, new_h).into()), crate::xwin::Tiled::Keep);
                        crate::xwin::configure(&w);
                    }
                    self.request_redraw();
                    return;
                }

                let alt_held = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                if !alt_held { return; }
                // Зум камеры щипком раньше не проверял раскладку ВООБЩЕ: он
                // работал и во Float, и в ленте Columns, где пан для того же
                // жеста давно закрыт. Теперь условие одно на все жесты камеры.
                if !self.touchpad_camera_allowed() {
                    // Масштаб всё равно запоминаем: иначе на следующем жесте
                    // factor посчитается от единицы и камера прыгнет рывком.
                    self.pinch_last_scale = scale;
                    return;
                }
                let factor = scale / self.pinch_last_scale;
                self.pinch_last_scale = scale;

                let cursor = self.pointer_location;

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
                let group = std::mem::take(&mut self.gesture_resize_group);
                tracing::debug!("ЖЕСТ: pinch закончен, окон в группе={}", group.len());
                self.gesture_resize_window = None;
                // Запоминаем итоговый размер КАЖДОМУ окну группы — иначе
                // следующий переход Float→tiling→Float вернул бы соседям
                // выделения их доресайзные размеры.
                for (w, _) in group {
                    let size = crate::xwin::current_size(&w);
                    if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == w) {
                        tw.float_size = Some(size);
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
                            tw.window == window
                        }) {
                            tw.float_position = new_loc;
                            tw.position = new_loc;
                            tw.float_position_set = true;
                        }
                    }
                    self.request_redraw();
                    return;
                }

                // ── Columns/niri: голый свайп 3+ пальцами листает полосу ─────
                // Горизонталь — прокрутка вида по колонкам (на отпускании
                // прилипаем к ближайшей, см. GestureSwipeEnd), вертикаль —
                // переход по столам. Только в Columns: в остальных раскладках
                // жест остаётся прежним (Alt+пан ниже), их не трогаем.
                if self.tile_config.layout == Layout::Columns && !self.logo_held {
                    if delta.x.abs() >= delta.y.abs() {
                        self.columns_swipe_scroll(delta.x);
                    } else {
                        self.columns_swipe_workspace(delta.y);
                    }
                    self.request_redraw();
                    return;
                }

                let alt = self.seat.get_keyboard()
                    .map(|kb| kb.modifier_state().alt)
                    .unwrap_or(false);
                if !alt { return; }
                // Камеру свайпом двигаем только в обзоре и лупе — в обычных
                // раскладках жест не должен уводить вид (touchpad_camera_allowed).
                if !self.touchpad_camera_allowed() { return; }
                // Alt + 2-пальца → pan
                let zoom = self.viewport.zoom;
                let dcam_x = delta.x / zoom;
                let dcam_y = delta.y / zoom;
                // Курсор держим на той же screen-позиции ЗДЕСЬ же: отложенная
                // sync_pointer_to_camera отставала на кадр и давала дрожь
                // (см. pan_camera_by). Hit-test при этом не портится — repin
                // шлёт pointer.motion, чего не делала прежняя ручная правка
                // pointer_location, из-за которой этот путь и был отложенным.
                self.pan_camera_by(dcam_x, dcam_y);
                // Кинетический скролл (1.1): копим дельту для инерции на конец жеста
                self.momentum.accumulate(
                    smithay::utils::Point::from((-dcam_x, -dcam_y)),
                    event.time_msec(),
                );
                self.request_redraw();
                tracing::debug!("swipe pan: cam=({:.1},{:.1})", self.viewport.cam_x, self.viewport.cam_y);
            }

            InputEvent::GestureSwipeEnd { .. } => {
                if self.gesture_move_window.take().is_some() {
                    return;
                }
                // В Columns свайп заканчивается «прилипанием» к ближайшей
                // колонке — как в niri, где вид всегда стоит на колонке, а не
                // между ними. Инерцию тут не запускаем: она возит камеру по
                // холсту и мимо модели колонок.
                if self.tile_config.layout == Layout::Columns {
                    self.columns_swipe_end();
                    self.request_redraw();
                    return;
                }
                self.momentum.launch();
            }

            _ => {}
        }
    }
}
