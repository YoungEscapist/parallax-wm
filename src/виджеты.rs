//! Виджеты — окна, прибитые к холсту (модель driftwm).
//!
//! Виджет в driftwm — это не отдельный сорт поверхности и не свой протокол, а
//! ПРАВИЛО, навешенное на обычное окно: `window{ app_id = "conky", widget =
//! true }`. Такое окно нельзя увести мышью, оно лежит ниже обычных окон и не
//! участвует ни в переборе Super+Tab, ни в навигации, ни в поиске. Часы, монитор
//! нагрузки, календарь — всё это запускается обычным клиентом (conky, eww, своя
//! поделка на gtk) и становится виджетом от одной строчки в конфиге.
//!
//! Почему так, а не своими виджетами внутри композитора: рисовать часы самому
//! parallax уже умеет (панель), а всё остальное человек хочет своё — и любая
//! встроенная галерея виджетов кончается тем, что нужного в ней нет. Правило же
//! работает с ЛЮБЫМ клиентом, включая те, что ещё не написаны.
//!
//! **Z-порядок.** Виджет лежит ниже окон не флагом в отрисовке, а местом в
//! стопке `Space`: `опустить_виджеты` поднимает над ними все обычные окна, и
//! дальше и кадр (`udev::собрать_элементы` идёт по `space.elements()`), и
//! хит-тест (`space.element_under`) сами дают правильный ответ. Отдельная ветка
//! «виджет рисуем первым» рассогласовалась бы с хит-тестом ровно в тот день,
//! когда её забудут поправить в одном из двух мест.
//!
//! **Перенос.** Мышью с Super, жестом и биндом виджет не двигается — но САМ
//! виджет может попросить перенести себя (`xdg_toplevel.move`): его тянут за
//! тело, как встроенные виджеты стола, и он передаёт хват композитору (так
//! устроен виджет TuiMus, 26.10.2026). Место запоминается по app_id в
//! `~/.local/state/parallax/widget-places.json`: виджет, которого запустили
//! заново, встаёт туда, куда его перетащили, а не туда, куда велит правило.

use std::collections::HashMap;
use std::path::PathBuf;

use smithay::desktop::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
    PointerInnerHandle, RelativeMotionEvent,
};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Point, Size};

use crate::state::Parallax;

impl Parallax {
    /// Правило из `config.lua` для окна — первое подходящее.
    ///
    /// Первое, а не самое точное: порядок в файле человек видит глазами, а
    /// «самое точное» пришлось бы объяснять в документации отдельной главой.
    fn правило_окна(&self, window: &Window) -> Option<crate::config::ПравилоОкна> {
        let app = crate::xwin::app_id(window).unwrap_or_default();
        let title = crate::xwin::title(window).unwrap_or_default();
        if app.is_empty() && title.is_empty() {
            return None;
        }
        self.lua_config
            .window_rules
            .iter()
            .find(|п| п.подходит(&app, &title))
            .cloned()
    }

    /// Применить правило к только что появившемуся окну.
    ///
    /// Зовётся ДВАЖДЫ: из `xwin::insert_new_window` (там уже известен WM_CLASS
    /// X11-окна) и из `app_id_changed` (wayland-клиент говорит app_id позже
    /// первого коммита). Повторный вызов безвреден — правило применяется один
    /// раз на окно, метка `виджет` служит и признаком «уже применяли».
    pub fn применить_правила(&mut self, window: &Window) {
        let Some(правило) = self.правило_окна(window) else { return };
        let уже = self.виджет_ли(window);
        if уже && правило.widget {
            return;
        }

        let размер: Option<Size<i32, Logical>> = match (правило.w, правило.h) {
            (Some(w), Some(h)) => Some(Size::from((w.max(1), h.max(1)))),
            // Половинчатый размер (задана одна сторона) берём от нынешнего:
            // сказать «шириной 400» и получить квадрат 400×400 человек не
            // просил.
            (Some(w), None) => {
                let h = window.geometry().size.h.max(1);
                Some(Size::from((w.max(1), h)))
            }
            (None, Some(h)) => {
                let w = window.geometry().size.w.max(1);
                Some(Size::from((w, h.max(1))))
            }
            (None, None) => None,
        };

        let место: Option<Point<i32, Logical>> = match (правило.x, правило.y) {
            (Some(x), Some(y)) => Some(Point::from((x, y))),
            (Some(x), None) => self.space.element_location(window).map(|l| (x, l.y).into()),
            (None, Some(y)) => self.space.element_location(window).map(|l| (l.x, y).into()),
            (None, None) => None,
        };
        // Куда виджет перетащили руками — важнее места из правила: правило
        // задаёт, где он встаёт впервые, а дальше место выбрал человек.
        let место = if правило.widget {
            запомненное_место(&crate::xwin::app_id(window).unwrap_or_default()).or(место)
        } else {
            место
        };

        let плавает = правило.float || правило.widget;
        // Окно, которое всплыло плавающим только по правилу и без места,
        // ставим посередине. Без этого оно оставалось там, куда его успела
        // положить раскладка до того, как пришёл app_id: в тайлинге — в
        // левый верхний угол слота, под панелью; во Float — у центра по
        // ПРЕДСКАЗАННОМУ размеру (полэкрана), то есть со сдвигом. Плававшее
        // и до правила (диалог, повторный app_id) не трогаем.
        let по_центру;
        if let Some(tw) = self
            .tagged_windows
            .iter_mut()
            .find(|tw| crate::dwindle::same_window(&tw.window, window))
        {
            по_центру = плавает && место.is_none() && !tw.floating;
            tw.виджет = правило.widget;
            tw.без_декора = правило.без_декора;
            if плавает {
                tw.floating = true;
                // Намеренно плавающее: сборка в тайлинг такое окно не трогает
                // (см. TaggedWindow::float_pinned).
                tw.float_pinned = true;
            }
            if let Some(размер) = размер {
                tw.float_size = Some(размер);
            }
            if let Some(место) = место {
                tw.position = место;
                tw.float_position = место;
                tw.float_position_set = true;
            }
        } else {
            return;
        }

        if let Some(размер) = размер {
            crate::xwin::set_size(window, Some(размер), crate::xwin::Tiled::Unset);
            crate::xwin::configure(window);
        }
        if let Some(место) = место {
            self.space.map_element(window.clone(), место, false);
        }
        // Wayland-клиент говорит app_id раньше первого буфера: размера ещё
        // нет, центрировать нечего — тогда на первом кадре (см. commit в
        // handlers/compositor.rs).
        if по_центру && !self.поставить_по_центру(window) {
            self.по_центру_на_первом_кадре.push(window.clone());
        }
        if правило.widget {
            self.опустить_виджеты();
            // Виджет фокус не берёт: `insert_new_window` отдал его новому окну
            // автоматически, и часы, перехватившие клавиатуру при запуске
            // сессии, — ровно то, чего от виджета не ждут.
            if self
                .focused_window()
                .is_some_and(|w| crate::dwindle::same_window(&w, window))
            {
                self.снять_фокус_с_виджета(window);
            }
        }
        // «Поверх» держится не подъёмом после каждого фокуса, а местом в
        // стопке: `Space` при каждой вставке сортирует окна по z_index, так что
        // окно выше Shell не перекроют ни raise_element от фокуса, ни
        // map_element раскладок (а их по коду десятки). Порядок в стопке — это
        // и кадр, и хит-тест (см. «Z-порядок» вверху файла).
        if правило.above {
            window.override_z_index(crate::fullscreen::Z_ПОВЕРХ);
            self.полный_экран_под_поверх();
            self.space.raise_element(window, false);
        }
        if плавает || размер.is_some() || место.is_some() {
            self.arrange();
        }
        self.request_redraw();
        tracing::info!(
            "plx/widget: rule applied to app_id={:?} widget={} float={} above={} pos={:?} size={:?}",
            crate::xwin::app_id(window),
            правило.widget,
            плавает,
            правило.above,
            место,
            размер,
        );
    }

    /// Поставить плавающее окно посередине того, что сейчас на экране: рамки
    /// плавающих поверх тайлинга либо кадра камеры (Float, лента). `false` —
    /// размер окна ещё неизвестен (первого буфера не было).
    pub fn поставить_по_центру(&mut self, window: &Window) -> bool {
        let размер = window.geometry().size;
        if размер.w <= 0 || размер.h <= 0 {
            return false;
        }
        let рамка = self.рамка_плавающих();
        let кадр = рамка.unwrap_or_else(|| self.visible_canvas_rect());
        let mut место = Point::from((
            кадр.loc.x + (кадр.size.w - размер.w) / 2,
            кадр.loc.y + (кадр.size.h - размер.h) / 2,
        ));
        // Над тайлингом окно обязано целиком влезать в рамку (см.
        // `удержать_на_экране`); холст Float бесконечный, там жать некуда.
        if let Some(рамка) = рамка {
            место = crate::tiling::зажать_в_рамку(рамка, место, размер);
        }
        if let Some(tw) = self
            .tagged_windows
            .iter_mut()
            .find(|tw| crate::dwindle::same_window(&tw.window, window))
        {
            tw.position = место;
            tw.float_position = место;
        }
        // Окно успело встать в раскладку до правила, и анимация въезда в слот
        // ещё идёт: не сняв её, следующие тики увозили окно из центра обратно
        // в угол слота (замер 25.10: 696 → 980 → 968,26 за полсекунды).
        self.freeze_window_anim(window);
        self.space.map_element(window.clone(), место, false);
        self.request_redraw();
        tracing::info!(
            "plx/widget: {:?} centred at {:?} ({}x{})",
            crate::xwin::app_id(window),
            место,
            размер.w,
            размер.h,
        );
        true
    }

    /// Окно без декора (`window{ decor = false }`): без тени, стекла и
    /// скругления углов.
    pub fn без_декора(&self, window: &Window) -> bool {
        self.tagged_windows
            .iter()
            .any(|tw| tw.без_декора && crate::dwindle::same_window(&tw.window, window))
    }

    /// Окно помечено виджетом?
    pub fn виджет_ли(&self, window: &Window) -> bool {
        self.tagged_windows
            .iter()
            .any(|tw| tw.виджет && crate::dwindle::same_window(&tw.window, window))
    }

    /// Есть ли виджеты вообще. Нужен обходам, которым иначе пришлось бы
    /// спрашивать про каждое окно (`опустить_виджеты` на каждое всплытие).
    pub fn есть_виджеты(&self) -> bool {
        self.tagged_windows.iter().any(|tw| tw.виджет)
    }

    /// Поднять над виджетами все обычные окна — сохранив их взаимный порядок.
    ///
    /// Обратной операции (`lower_element`) в `Space` нет, поэтому опускаем
    /// «сверху»: проходим обычные окна в том же порядке, в каком они лежат
    /// сейчас, и поднимаем каждое. Виджеты остаются внизу все вместе, их
    /// собственный порядок между собой не меняется.
    ///
    /// `activate = false` — всплытие НЕ должно отдавать фокус: иначе появление
    /// виджета уводило бы клавиатуру у того, кто ей пользуется.
    pub fn опустить_виджеты(&mut self) {
        if !self.есть_виджеты() {
            return;
        }
        let обычные: Vec<Window> = self
            .space
            .elements()
            .filter(|w| {
                !self
                    .tagged_windows
                    .iter()
                    .any(|tw| tw.виджет && crate::dwindle::same_window(&tw.window, w))
            })
            .cloned()
            .collect();
        for окно in обычные {
            self.space.raise_element(&окно, false);
        }
    }

    /// Снять клавиатурный фокус с окна, ставшего виджетом, отдав его любому
    /// обычному окну на этом столе (или никому).
    fn снять_фокус_с_виджета(&mut self, window: &Window) {
        let теги = self.viewport.current_tags();
        let другое = self
            .tagged_windows
            .iter()
            .find(|tw| {
                !tw.виджет
                    && tw.tags & теги != 0
                    && tw.window.alive()
                    && !crate::dwindle::same_window(&tw.window, window)
            })
            .map(|tw| tw.window.clone());
        match другое {
            Some(w) => crate::xwin::focus(self, &w),
            None => {
                // Отдавать некому — снимаем фокус совсем, тем же порядком, что
                // клик по пустому холсту (см. input.rs).
                window.set_activated(false);
                crate::xwin::configure(window);
                if let Some(kb) = self.seat.get_keyboard() {
                    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                    kb.set_focus(self, None, serial);
                }
            }
        }
    }

    /// `plx ctl widget on|off` — пометить/снять пометку на лету.
    ///
    /// Нужна ровно потому, что правило срабатывает при ПОЯВЛЕНИИ окна: подобрать
    /// место и размер виджета, перезапуская его после каждой правки конфига, —
    /// это и есть та работа, которую команда снимает.
    pub fn виджет_пометить(&mut self, window: &Window, включить: bool) {
        let Some(tw) = self
            .tagged_windows
            .iter_mut()
            .find(|tw| crate::dwindle::same_window(&tw.window, window))
        else {
            return;
        };
        if tw.виджет == включить {
            return;
        }
        tw.виджет = включить;
        if включить {
            tw.floating = true;
            tw.float_pinned = true;
        }
        if включить {
            self.опустить_виджеты();
            if self
                .focused_window()
                .is_some_and(|w| crate::dwindle::same_window(&w, window))
            {
                self.снять_фокус_с_виджета(window);
            }
        }
        self.arrange();
        self.request_redraw();
        tracing::info!(
            "plx/widget: {:?} widget={}",
            crate::xwin::app_id(window),
            включить
        );
    }

    /// `plx ctl widget move X Y` — подвинуть виджет цифрами.
    ///
    /// Мышью с Super он не двигается нарочно (в этом и смысл), а тянуть себя
    /// за тело умеет не всякий виджет (см. «Перенос» вверху файла), поэтому
    /// команда есть — как и в driftwm, где `driftwm msg move` тоже работает
    /// на виджетах.
    /// `false` — окно не виджет: команда `widget move` его не трогает.
    /// Обычные окна двигаются мышью, Super+стрелками и жестами, и молча
    /// телепортировать чужое окно (например, то, что просто в фокусе) — худший
    /// из возможных ответов на опечатку в имени.
    pub fn виджет_передвинуть(&mut self, window: &Window, x: i32, y: i32) -> bool {
        if !self.виджет_ли(window) {
            return false;
        }
        self.поставить_виджет(window, (x, y).into());
        self.запомнить_место_виджета(window);
        true
    }

    /// Поставить виджет в точку холста — и снова под обычные окна:
    /// `map_element` поднимает окно наверх стопки.
    fn поставить_виджет(&mut self, window: &Window, место: Point<i32, Logical>) {
        if let Some(tw) = self
            .tagged_windows
            .iter_mut()
            .find(|tw| crate::dwindle::same_window(&tw.window, window))
        {
            tw.position = место;
            tw.float_position = место;
            tw.float_position_set = true;
        }
        self.space.map_element(window.clone(), место, false);
        self.опустить_виджеты();
        self.request_redraw();
    }

    /// Начать перенос виджета по его же просьбе (`xdg_toplevel.move`).
    pub fn начать_перенос_виджета(
        &mut self,
        window: Window,
        start: PointerGrabStartData<Parallax>,
    ) -> Option<ПереносВиджета> {
        let окно = self.space.element_location(&window)?;
        self.freeze_window_anim(&window);
        Some(ПереносВиджета { курсор: start.location, start_data: start, window, окно })
    }

    /// Записать место виджета по его app_id (см. «Перенос» вверху файла).
    fn запомнить_место_виджета(&self, window: &Window) {
        let app = crate::xwin::app_id(window).unwrap_or_default().to_lowercase();
        let (Some(место), Some(путь)) = (self.space.element_location(window), файл_мест()) else { return };
        if app.is_empty() {
            return;
        }
        let mut места = прочитать_места();
        места.insert(app, [место.x, место.y]);
        let записано = путь
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&путь, serde_json::to_string_pretty(&места).unwrap_or_default()));
        match записано {
            Ok(()) => tracing::info!("plx/widget: {:?} remembered at {:?}", crate::xwin::app_id(window), место),
            Err(err) => tracing::warn!("plx/widget: could not save {}: {}", путь.display(), err),
        }
    }
}

fn файл_мест() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".local/state/parallax/widget-places.json"))
}

fn прочитать_места() -> HashMap<String, [i32; 2]> {
    файл_мест()
        .and_then(|путь| std::fs::read_to_string(путь).ok())
        .and_then(|текст| serde_json::from_str(&текст).ok())
        .unwrap_or_default()
}

fn запомненное_место(app_id: &str) -> Option<Point<i32, Logical>> {
    if app_id.is_empty() {
        return None;
    }
    прочитать_места().get(&app_id.to_lowercase()).map(|&[x, y]| (x, y).into())
}

/// Перенос виджета мышью — по просьбе самого виджета. Отдельный хват, а не
/// `MoveSurfaceGrab`: тот умеет раскладки, склейку, прилипание к соседям и
/// подъём окна наверх — виджету не нужно ничего из этого, только ехать за
/// курсором по холсту и оставаться под окнами.
pub struct ПереносВиджета {
    start_data: PointerGrabStartData<Parallax>,
    window: Window,
    /// Курсор и окно в начале переноса — в координатах холста.
    курсор: Point<f64, Logical>,
    окно: Point<i32, Logical>,
}

impl PointerGrab<Parallax> for ПереносВиджета {
    fn motion(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let сдвиг = event.location - self.курсор;
        let место = self.окно + Point::from((сдвиг.x.round() as i32, сдвиг.y.round() as i32));
        let w = self.window.clone();
        data.поставить_виджет(&w, место);
    }

    fn relative_motion(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &ButtonEvent) {
        handle.button(data, event);
        if handle.current_pressed().is_empty() {
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    fn axis(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, details: AxisFrame) {
        handle.axis(data, details)
    }
    fn frame(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>) {
        handle.frame(data);
    }
    fn gesture_swipe_begin(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureSwipeBeginEvent) {
        handle.gesture_swipe_begin(data, event)
    }
    fn gesture_swipe_update(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureSwipeUpdateEvent) {
        handle.gesture_swipe_update(data, event)
    }
    fn gesture_swipe_end(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureSwipeEndEvent) {
        handle.gesture_swipe_end(data, event)
    }
    fn gesture_pinch_begin(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GesturePinchBeginEvent) {
        handle.gesture_pinch_begin(data, event)
    }
    fn gesture_pinch_update(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GesturePinchUpdateEvent) {
        handle.gesture_pinch_update(data, event)
    }
    fn gesture_pinch_end(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GesturePinchEndEvent) {
        handle.gesture_pinch_end(data, event)
    }
    fn gesture_hold_begin(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureHoldBeginEvent) {
        handle.gesture_hold_begin(data, event)
    }
    fn gesture_hold_end(&mut self, data: &mut Parallax, handle: &mut PointerInnerHandle<'_, Parallax>, event: &GestureHoldEndEvent) {
        handle.gesture_hold_end(data, event)
    }
    fn start_data(&self) -> &PointerGrabStartData<Parallax> {
        &self.start_data
    }

    /// Отпустили — запоминаем место.
    fn unset(&mut self, data: &mut Parallax) {
        data.запомнить_место_виджета(&self.window);
    }
}
