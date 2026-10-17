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

use smithay::desktop::Window;
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

        let плавает = правило.float || правило.widget;
        if let Some(tw) = self
            .tagged_windows
            .iter_mut()
            .find(|tw| crate::dwindle::same_window(&tw.window, window))
        {
            tw.виджет = правило.widget;
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
        if плавает || размер.is_some() || место.is_some() {
            self.arrange();
        }
        self.request_redraw();
        tracing::info!(
            "plx/widget: rule applied to app_id={:?} widget={} float={} pos={:?} size={:?}",
            crate::xwin::app_id(window),
            правило.widget,
            плавает,
            место,
            размер,
        );
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

    /// `plx ctl widget move X Y` — единственный способ подвинуть виджет.
    ///
    /// Руками он не двигается нарочно (в этом и смысл), но подбирать место
    /// цифрами вслепую невыносимо, поэтому команда есть — как и в driftwm,
    /// где `driftwm msg move` тоже работает на виджетах, а перетаскивание нет.
    /// `false` — окно не виджет: команда `widget move` его не трогает.
    /// Обычные окна двигаются мышью, Super+стрелками и жестами, и молча
    /// телепортировать чужое окно (например, то, что просто в фокусе) — худший
    /// из возможных ответов на опечатку в имени.
    pub fn виджет_передвинуть(&mut self, window: &Window, x: i32, y: i32) -> bool {
        if !self.виджет_ли(window) {
            return false;
        }
        let место: Point<i32, Logical> = (x, y).into();
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
        true
    }
}
