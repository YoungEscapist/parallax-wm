//! niri-подобный обзор рабочих столов (тап Super).
//!
//! Тап по Super открывает/закрывает обзор: рабочие столы (воркспейсы с окнами)
//! раскладываются 2D-сеткой ВОКРУГ ЦЕНТРАЛЬНОГО (текущего) стола, холст
//! отдаляется. В обзоре:
//!  · ОКНА не трогаются — только ПАН (ЛКМ-драг по пустому месту / 2-пальца) и
//!    ЗУМ (колесо);
//!  · САМИ СТОЛЫ не двигаются: ячейку каждому выдаёт обзор (кольцами вокруг
//!    центрального, см. first_free_cell) — перетаскивания столов нет;
//!  · повторный тап Super или ПКМ → выйти на стол ПОД КУРСОРОМ (плавный перелёт);
//!  · LMB-клик по столу → плавный перелёт к нему.
//!
//! В ленте (Layout::Columns, niri) обзор устроен иначе — см. «Обзор ленты»
//! ниже: там столы уже лежат этажами одного холста, поэтому окна не
//! перекладываются вовсе, а камера просто отъезжает и вписывает ленту целиком
//! (как Super+Space, только с точным кадром). Клики, драг окон, зум колесом и
//! выход работают там так же, как в сеточном обзоре.
//!
//! При выходе на ДРУГОЙ стол: камера летит к его ячейке в обзоре, затем
//! финализируется выход (восстановление layout/зума). При выходе на тот же
//! стол → просто выход (без анимации).


use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::anim::CameraAnim;
use crate::state::Dawn;
use crate::tiling::{GAP_INNER, GAP_OUTER, Layout};

/// Зазор между ячейками сетки столов (canvas px).
const BAND_GAP: i32 = 140;
/// Уровень отдаления в обзоре.
const OVERVIEW_ZOOM: f64 = 0.5;
/// Длительность анимации перелёта между столами.

fn same_window(a: &Window, b: &Window) -> bool {
    a == b
}

impl Dawn {
    /// Можно ли СЕЙЧАС войти в обзор. Во Float — нет: там окна и так свободно
    /// разбросаны по бесконечному холсту, камера ходит куда угодно, и обзор
    /// столов ничего не добавляет — зато случайный тап по Super (он ловится при
    /// каждом отпускании Super без другой клавиши) перекладывал окна в сетку
    /// миниатюр и сбивал ручную раскладку. В тайловых режимах обзор, наоборот,
    /// единственный способ увидеть остальные столы — там он разрешён.
    pub fn overview_allowed(&self) -> bool {
        self.tile_config.layout != Layout::Float
    }

    /// Помнит ли стол `tag` ПЛАВАЮЩУЮ раскладку. Правило то же, что у
    /// columns_is_strip_tag: закрепление (стол 3 = Float) сильнее памяти, а у
    /// ТЕКУЩЕГО стола источник правды — живая раскладка.
    ///
    /// Плавающие столы в обзор не попадают ВООБЩЕ (ни рамкой, ни окнами): их
    /// окна разбросаны вручную по бесконечному холсту, в ячейку сетки они
    /// влезают только сжатыми в миниатюры, а на выходе их приходится
    /// восстанавливать снимком — то есть обзор для них не показывает ничего
    /// полезного и только рискует ручной раскладкой.
    pub fn overview_is_float_tag(&self, tag: u32) -> bool {
        match self.tag_layouts.get(&tag) {
            Some(l) => *l == Layout::Float,
            None if tag == self.viewport.current_tags() => {
                self.tile_config.layout == Layout::Float
            }
            None => false,
        }
    }

    pub fn toggle_overview(&mut self) {
        // Если идёт exit-анимация — не трогаем, даём завершиться.
        if self.overview_exit_pending {
            return;
        }
        // Лента (niri): обзор — это отъезд камеры, а не пересборка окон, и
        // выходим мы просто на этаж под курсором (см. exit_overview_strip).
        if self.overview_strip {
            let mask = self.overview_workspace_at(self.pointer_location);
            self.exit_overview_strip(mask, None);
            return;
        }
        // Вход разрешён из всех раскладок, кроме Float (см. overview_allowed);
        // проверка стоит в enter_overview, выход же доступен всегда — иначе из
        // обзора нельзя было бы выйти на плавающий стол.
        if self.overview_active {
            // Win tap: выход на стол ПОД КУРСОРОМ, показанный ЦЕЛИКОМ (все окна
            // как разложены). Сбрасываем фокус с окон — иначе sloppy focus
            // активирует/поднимает окно под курсором после выхода, а мы хотим
            // весь рабочий стол без выделения одного окна.
            let mask = self.overview_workspace_at(self.pointer_location);
            self.exit_overview_immediate(mask);
            if let Some(kb) = self.seat.get_keyboard() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                kb.set_focus(self, None, serial);
            }
            let all: Vec<_> = self.space.elements().cloned().collect();
            for w in all {
                w.set_activated(false);
                crate::xwin::configure(&w);
            }
        } else {
            self.enter_overview();
        }
    }

    /// Выйти из обзора на стол под курсором с плавным перелётом.
    pub fn exit_overview_to_cursor(&mut self) {
        if !self.overview_active || self.overview_exit_pending {
            return;
        }
        let mask = self.overview_workspace_at(self.pointer_location);
        if self.overview_strip {
            self.exit_overview_strip(mask, None);
            return;
        }

        if let Some(target_mask) = mask {
            let cur_mask = self.viewport.current_tags();
            if target_mask == cur_mask {
                // Тот же стол — просто выходим (без анимации).
                self.exit_overview_immediate(None);
                return;
            }
            // Другой стол: плавный перелёт.
            let (w, h) = match self.overview_band_size() {
                Some(s) => s,
                None => { self.exit_overview_immediate(None); return; }
            };
            let slot = *self.overview_slots.get(&target_mask).unwrap_or(&(0, 0));
            let target_cam = self.center_cam_on_slot(slot, w, h);
            let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
            self.camera_anim = Some(CameraAnim::new(from, target_cam, crate::anim::дуг::обзор()));
            self.overview_exit_pending = true;
            self.overview_exit_target_ws = Some(target_mask);
        } else {
            // Курсор вне столов — просто выходим.
            self.exit_overview_immediate(None);
        }
    }

    /// Финализировать exit после завершения анимации перелёта.
    /// Вызывается из anim::tick, когда camera_anim/zoom_anim сделались.
    pub fn overview_finalize_exit(&mut self) {
        if !self.overview_exit_pending {
            return;
        }
        let target = self.overview_exit_target_ws.take();
        self.overview_exit_pending = false;
        self.exit_overview_immediate(target);
    }

    /// Мгновенный выход из обзора (без анимации). Если `switch_to` Some —
    /// переключаемся на этот стол.
    pub fn exit_overview_immediate(&mut self, switch_to: Option<u32>) {
        if !self.overview_active {
            return;
        }
        if self.overview_strip {
            self.exit_overview_strip(switch_to, None);
            return;
        }
        self.overview_active = false;
        self.overview_exit_pending = false;
        self.overview_exit_target_ws = None;

        if let Some((tag, cam_x, cam_y, zoom, layout)) = self.overview_prev.take() {
            self.viewport.tagset[self.viewport.seltags] = tag;
            self.tile_config.layout = layout;
            // Кадр (зум и камеру) возвращаем ПЕРВЫМ делом — до refresh_tags и
            // arrange. Рабочая область тайлинга считается от output_geometry, а
            // та делится на зум: при обзорном зуме 0.45 экран 2560×1080
            // притворяется холстом 5689×2400, и arrange раскладывал окна по
            // нему — второе окно уезжало на x≈4288, третье и четвёртое ещё
            // дальше. Дальше зум возвращался в 1.0, и на экране оставалось одно
            // окно (а то и ни одного), причём эти координаты оседали в
            // tw.position: следующий заход на такой стол по Super+N уже не
            // звал arrange (стол посещён, раскладка та же) и показывал ровно ту
            // же пустоту. Ровно это и выглядело как «столы теряют окна».
            self.viewport.zoom = zoom;
            self.viewport.cam_x = cam_x;
            self.viewport.cam_y = cam_y;
            self.apply_camera();
            self.refresh_tags();
            // ВАЖНО: строго после refresh_tags — он сам переписывает tw.position
            // текущей (обзорной) позицией окна, так что снимок надо накатывать
            // поверх него, а не до.
            self.restore_pre_overview_geometry();
            self.arrange();
        }
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        if let Some(mask) = switch_to {
            self.view_tag(mask);
            // Обзор (overview_layout) сдвигает окна в сетку и сжимает их, а
            // refresh_tags сохраняет эти обзорные позиции в tw.position. Для
            // Tile/Monocle view_tag посещённого стола НЕ вызывает arrange и
            // улетает камерой к устаревшей per-tag позиции → на экране одно
            // мелкое окно под курсором вместо всего стола. Показываем стол
            // ЦЕЛИКОМ как собран: стандартный кадр (zoom 1, камера в углу стола)
            // + переразметка.
            // Columns раскладывается из своей модели прямо в view_tag — не трогаем.
            if matches!(self.tile_config.layout, Layout::Tile | Layout::Monocle) {
                // Угол СВОЕГО монитора, а не ноль холста: стол мог оказаться на
                // втором мониторе (его дом — (1 000 000, 0), см.
                // `monitors::ШАГ_ДОМА`), и нулевая камера показала бы пустоту
                // вместо стола. Дом спрашиваем ПОСЛЕ view_tag: он мог сменить
                // активный монитор вместе со столом.
                let дом = self.монитор_дом();
                self.camera_anim = None;
                self.zoom_anim = None;
                self.zoom_glide = None;
                self.viewport.zoom = 1.0;
                self.viewport.cam_x = дом.x as f64;
                self.viewport.cam_y = дом.y as f64;
                self.apply_camera();
                self.arrange();
            }
        }
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn: overview off");
    }

    /// Снимает позиции/размеры ВСЕХ окон перед тем, как обзор разложит их
    /// миниатюрами по сетке столов.
    fn save_pre_overview_geometry(&mut self) {
        self.overview_saved_geo = self.tagged_windows.iter()
            .map(|tw| {
                // Окна ЧУЖИХ столов в space не смаплены (см. refresh_tags), так
                // что позицию берём из их сохранённой tw.position, а размер —
                // прямо у поверхности: обзор трогает и те, и другие.
                let loc = self.space.element_geometry(&tw.window)
                    .map(|g| g.loc)
                    .unwrap_or(tw.position);
                (tw.window.clone(), loc, tw.window.geometry().size)
            })
            .collect();
    }

    /// Накатывает снимок обратно: окно возвращается туда же и такого же
    /// размера, каким было до обзора. Для тайловых столов это всё равно
    /// перебьёт arrange(), а для плавающих — единственный способ не оставить
    /// окна сжатыми миниатюрами в клетке сетки.
    fn restore_pre_overview_geometry(&mut self) {
        let saved = std::mem::take(&mut self.overview_saved_geo);
        let current = self.viewport.current_tags();
        for (win, loc, size) in saved {
            if let Some(tw) = self.tagged_windows.iter_mut()
                .find(|tw| same_window(&tw.window, &win))
            {
                tw.position = loc;
                let visible = tw.tags & current != 0;
                crate::xwin::set_size(&win, Some(size), crate::xwin::Tiled::Unset);
                crate::xwin::configure(&win);
                if visible {
                    self.space.map_element(win, loc, false);
                }
            }
        }
    }

    /// Окно отресайзили прямо в обзоре: новый размер должен пережить выход,
    /// иначе снимок вернул бы «как было до обзора» и ресайз пропал бы. Ячейка
    /// стола в сетке — размером с экран, так что размеры в обзоре настоящие,
    /// а не масштабированные.
    pub fn overview_note_resize(&mut self, window: &Window, size: Size<i32, Logical>) {
        // Тянуть можно и за левый/верхний край — тогда вместе с размером
        // поехала позиция. Обзор показывает окна сдвигом от дома, так что
        // не записанный дом на следующей же переразметке вернул бы окно назад.
        let mask = self.overview_mask_of_window(window);
        let loc = match (mask, self.space.element_geometry(window)) {
            (Some(mask), Some(g)) => Some(g.loc - self.overview_offset(mask)),
            _ => None,
        };
        if let Some(e) = self.overview_saved_geo.iter_mut()
            .find(|(w, _, _)| same_window(w, window))
        {
            e.2 = size;
            if let Some(loc) = loc {
                e.1 = loc;
            }
        }
    }

    /// Физический размер output'а = размер одной ячейки стола.
    pub fn overview_band_size(&self) -> Option<(i32, i32)> {
        let экран = self.screen_size();
        Some((экран.w, экран.h))
    }

    /// Canvas-прямоугольник стола по его слоту (ячейке сетки).
    ///
    /// Сетка строится от ДОМА своего монитора, а не от нуля холста: столы
    /// принадлежат мониторам и лежат в их прямоугольниках (см.
    /// `monitors::ШАГ_ДОМА` и `tiling::screen_area`). Пока здесь стоял голый
    /// ноль, обзор на втором мониторе уводил камеру к столам первого — замер
    /// 26.08.2026: тап Super при камере 1 000 000 давал камеру (−111, −47).
    fn slot_rect(&self, slot: (i32, i32), w: i32, h: i32) -> Rectangle<i32, Logical> {
        let дом = self.монитор_дом();
        Rectangle::new(
            (
                дом.x + slot.0 * (w + BAND_GAP),
                дом.y + slot.1 * (h + BAND_GAP),
            ).into(),
            (w, h).into(),
        )
    }

    /// Первая свободная ячейка сетки — кольцами ВОКРУГ центра (0,0), а не
    /// колонкой вниз: столы должны обступать текущий со всех четырёх сторон
    /// (справа, слева, снизу, сверху), а не выстраиваться в одну полосу.
    /// Порядок внутри кольца — по часовой от правого соседа, так что при 5
    /// столах получается «крест»: центр и по одному соседу с каждой стороны,
    /// и только с шестого стола идут диагонали.
    /// Диапазон ячеек — ±9 (больше девяти столов не бывает).
    fn first_free_cell(taken: &std::collections::HashSet<(i32, i32)>) -> (i32, i32) {
        for cell in Self::spiral_cells() {
            if !taken.contains(&cell) {
                return cell;
            }
        }
        (0, 0)
    }

    /// Ячейки сетки в порядке удаления от центра: (0,0), затем 4 соседа по
    /// сторонам, затем диагонали, затем следующее кольцо и т.д. Ближние к
    /// центру — раньше, при равном расстоянии порядок стабилен (право, низ,
    /// лево, верх).
    fn spiral_cells() -> Vec<(i32, i32)> {
        let mut cells: Vec<(i32, i32)> = Vec::new();
        for col in -9..=9 {
            for row in -9..=9 {
                cells.push((col, row));
            }
        }
        // Ключ сортировки: сначала «манхэттенское» кольцо (крест раньше
        // диагоналей), внутри кольца — фиксированный обход по сторонам света.
        cells.sort_by_key(|&(c, r)| {
            let ring = c.abs().max(r.abs());
            let manhattan = c.abs() + r.abs();
            let side = match (c.signum(), r.signum()) {
                (1, 0) => 0,  // справа
                (0, 1) => 1,  // снизу
                (-1, 0) => 2, // слева
                (0, -1) => 3, // сверху
                _ => 4,
            };
            (ring, manhattan, side, c, r)
        });
        cells
    }

    /// Canvas-прямоугольник стола `mask` (вся его рамка).
    pub fn overview_band_rect(&self, mask: u32) -> Option<Rectangle<i32, Logical>> {
        let (w, h) = self.overview_band_size()?;
        Some(self.slot_rect(*self.overview_slots.get(&mask).unwrap_or(&(0, 0)), w, h))
    }

    /// Область ВНУТРИ стола, в которой разрешено находиться окну: рамка минус
    /// внешний отступ. За её пределы окно в обзоре не выпускает перетаскивание
    /// (move_grab) — стол работает как жёсткая рамка. Ресайз рамкой НЕ
    /// ограничен: он в обзоре идёт по обычной логике раскладки (см.
    /// resize_grab.rs), а тайловому дереву эта же область служит рабочей.
    pub fn overview_window_area(&self, mask: u32) -> Option<Rectangle<i32, Logical>> {
        let r = self.overview_band_rect(mask)?;
        Some(Rectangle::new(
            (r.loc.x + GAP_OUTER, r.loc.y + GAP_OUTER).into(),
            (
                (r.size.w - GAP_OUTER * 2).max(1),
                (r.size.h - GAP_OUTER * 2).max(1),
            ).into(),
        ))
    }

    /// Стол, которому принадлежит окно (по его тегам).
    pub fn overview_mask_of_window(&self, window: &Window) -> Option<u32> {
        self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, window))
            .map(|tw| tw.tags)
    }

    /// Зажимает позицию окна размера `size` в пределах стола `mask`. Окно шире
    /// или выше стола прижимается к левому/верхнему краю (клампить нечем).
    pub fn overview_clamp_loc(
        &self,
        mask: u32,
        loc: Point<i32, Logical>,
        size: Size<i32, Logical>,
    ) -> Point<i32, Logical> {
        // В ленте окно тащат свободно: этажи стоят вплотную, и «жёсткая рамка»
        // стола мешала бы перетащить окно на соседний этаж.
        if self.overview_strip {
            return loc;
        }
        let Some(area) = self.overview_window_area(mask) else { return loc };
        let max_x = area.loc.x + (area.size.w - size.w).max(0);
        let max_y = area.loc.y + (area.size.h - size.h).max(0);
        Point::from((
            loc.x.clamp(area.loc.x, max_x),
            loc.y.clamp(area.loc.y, max_y),
        ))
    }

    /// Camera position to center a given slot on screen at current zoom.
    fn center_cam_on_slot(&self, slot: (i32, i32), w: i32, h: i32) -> Point<f64, Logical> {
        self.center_cam_on_slot_zoom(slot, w, h, self.viewport.zoom)
    }

    /// То же, но для ЗАДАННОГО зума: при входе в обзор камера считается под
    /// будущий (обзорный) зум, а не под текущий — на момент расчёта viewport
    /// ещё держит доовзорный (см. enter_overview).
    fn center_cam_on_slot_zoom(&self, slot: (i32, i32), w: i32, h: i32, zoom: f64) -> Point<f64, Logical> {
        let rect = self.slot_rect(slot, w, h);
        let cx = rect.loc.x as f64 + w as f64 / 2.0;
        let cy = rect.loc.y as f64 + h as f64 / 2.0;
        Point::from((
            cx - (w as f64) / (2.0 * zoom),
            cy - (h as f64) / (2.0 * zoom),
        ))
    }

    /// Зум и позиция камеры, чтобы в кадр влезли ВСЕ столы обзора (их общий
    /// bbox по слотам) с небольшим полем. Возвращает (zoom, cam). Если столов
    /// нет — дефолт OVERVIEW_ZOOM с центром на (0,0).
    fn overview_fit_all(&self, w: i32, h: i32) -> (f64, Point<f64, Logical>) {
        let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
        let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
        for m in &self.overview_order {
            let slot = *self.overview_slots.get(m).unwrap_or(&(0, 0));
            let r = self.slot_rect(slot, w, h);
            min_x = min_x.min(r.loc.x);
            min_y = min_y.min(r.loc.y);
            max_x = max_x.max(r.loc.x + w);
            max_y = max_y.max(r.loc.y + h);
        }
        if min_x > max_x {
            return (OVERVIEW_ZOOM, self.center_cam_on_slot((0, 0), w, h));
        }
        let bw = (max_x - min_x) as f64;
        let bh = (max_y - min_y) as f64;
        // 0.92 — небольшое поле по краям, чтобы столы не липли к рамке экрана.
        let zoom = (0.92 * (w as f64 / bw).min(h as f64 / bh)).clamp(0.12, 1.0);
        let bcx = min_x as f64 + bw / 2.0;
        let bcy = min_y as f64 + bh / 2.0;
        let cam = Point::from((
            bcx - w as f64 / (2.0 * zoom),
            bcy - h as f64 / (2.0 * zoom),
        ));
        (zoom, cam)
    }

    pub fn enter_overview(&mut self) {
        if self.overview_active || self.overview_exit_pending {
            return;
        }
        if !self.overview_allowed() {
            // Текст отставал от кода: запрет давно переехал с Tile на Float
            // (overview_allowed выше), а сообщение осталось прежним и в логе
            // 20260729_190042 одиннадцать раз врало про «тайловый режим».
            tracing::debug!(
                "dawn: обзор не открывается в раскладке {:?} (разрешён везде, кроме Float)",
                self.tile_config.layout,
            );
            return;
        }
        // Лента (niri) — свой обзор: окна остаются в полосе, отъезжает камера.
        if self.tile_config.layout == Layout::Columns {
            self.enter_overview_strip();
            return;
        }
        let (w, h) = match self.overview_band_size() {
            Some(s) => s,
            None => return,
        };
        let prev_cam = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let prev_zoom = self.viewport.zoom;

        self.overview_prev = Some((
            self.viewport.current_tags(),
            self.viewport.cam_x,
            self.viewport.cam_y,
            self.viewport.zoom,
            self.tile_config.layout,
        ));
        self.overview_active = true;
        self.overview_strip = false;
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;

        // Сеточный обзор: каждый стол = ячейка сетки со своими окнами (dwindle),
        // камера вписывает все столы (overview_fit_all). Лента (Columns) сюда не
        // доходит — у неё свой обзор (enter_overview_strip, ветка выше).

        // Занятые столы: текущий + все с окнами, но ТОЛЬКО из СВОЕЙ изоляции.
        //
        // Изоляций две: лента (Columns) и всё остальное — tiling и floating
        // вместе. Ленточные столы в эту сетку не попадают никогда: у ленты свой
        // обзор, её этажи лежат на общем холсте, и выход на такой стол из сетки
        // переключил бы режим целиком — окна доехавшего стола пересобрались бы
        // чужой раскладкой поверх снимка от прежней.
        //
        // А вот Tile, Dwindle и Monocle — соседи по одной изоляции: столы там
        // независимы, и стол, ВЫШЕДШИЙ из ленты (Win+N), обязан оказаться
        // именно здесь. Раньше сравнивались точные раскладки, и обзор тайлового
        // стола не показывал ни monocle-соседей, ни вернувшихся из ленты.
        //
        // ПЛАВАЮЩИЕ столы отсюда исключены совсем (overview_is_float_tag): в
        // обзоре их не должно быть видно ни рамкой, ни окнами.
        let cur = self.viewport.current_tags();
        let cur_layout = self.tile_config.layout;
        let mut order: Vec<u32> = vec![cur];
        // Columns сюда не попадает (у ленты свой обзор), проверка оставлена
        // страховкой: собирать ленточные столы в сетку нельзя.
        if cur_layout != Layout::Columns {
            for i in 0..9u32 {
                let m = 1u32 << i;
                if m == cur || !self.tagged_windows.iter().any(|tw| tw.tags & m != 0) {
                    continue;
                }
                // ТОЛЬКО СВОИ столы. Стол принадлежит монитору
                // (`Dawn::монитор_стола`), а обзор раскладывает окна по ячейкам
                // сетки на холсте СВОЕГО монитора: взяв сюда чужой стол, он
                // физически утаскивал окна соседнего экрана к себе — за миллион
                // пикселей от их дома, — и после выхода они там и оставались.
                // Замер 29.08.2026 (двухмониторный харнесс): удержание Super на
                // мониторе 1 давало «overview on (2 workspaces)», а окно стола 2
                // уезжало из `1000026,26` в `3374,18`. Ничейные столы (ещё не
                // закреплённые) считаем своими — их закрепит первый же заход.
                if self.монитор_стола(m).is_some_and(|i| i != self.активный) {
                    continue;
                }
                // Ленточные (свой обзор) и ПЛАВАЮЩИЕ (обзор им не нужен вовсе,
                // см. overview_is_float_tag) столы в сетку не берём.
                if !self.columns_is_strip_tag(m) && !self.overview_is_float_tag(m) {
                    order.push(m);
                }
            }
        }
        self.overview_order = order.clone();

        // Ячейки сетки ПЕРЕЖИВАЮТ выход из обзора: стол, который в прошлый раз
        // стоял слева от соседа, должен остаться слева и в следующем обзоре.
        // Сохраняем прежнюю ячейку столу, если её не занял кто-то раньше по
        // порядку; остальным выдаём первую свободную.
        let prev_slots = std::mem::take(&mut self.overview_slots);
        let mut taken: std::collections::HashSet<(i32, i32)> = std::collections::HashSet::new();
        for &m in &order {
            let cell = match prev_slots.get(&m) {
                Some(&c) if !taken.contains(&c) => c,
                _ => Self::first_free_cell(&taken),
            };
            taken.insert(cell);
            self.overview_slots.insert(m, cell);
        }

        // Снимок геометрии до обзора — обзор сожмёт окна в миниатюры, на выходе
        // (в т.ч. для плавающих столов) их надо вернуть как было.
        self.save_pre_overview_geometry();

        // Раскладываем окна в overview layout
        self.overview_layout(w, h);

        // Плавный перелёт: zoom+камера летят к обзорной позиции. Раньше зум был
        // фиксированным (OVERVIEW_ZOOM=0.5) с центрированием на слоте (0,0) —
        // при 2+ столах остальные уходили за край экрана («виден только 1»).
        // Теперь вписываем ВСЕ столы: зум под их общий bbox + центр на bbox.
        // Зум берём такой, чтобы столы влезли в кадр целиком, а вот КАМЕРА
        // встаёт не над общим bbox сетки, а строго над ТЕКУЩИМ столом: обзор
        // — это подъём над тем столом, на котором его открыли. При
        // центрировании по bbox активный стол уезжал в сторону тем сильнее,
        // чем больше соседей, и было непонятно, откуда ты вышел.
        let (fit_zoom, _) = self.overview_fit_all(w, h);
        let cur_slot = *self.overview_slots.get(&cur).unwrap_or(&(0, 0));
        let target_cam = self.center_cam_on_slot_zoom(cur_slot, w, h, fit_zoom);
        self.viewport.zoom = prev_zoom;
        self.viewport.cam_x = prev_cam.x;
        self.viewport.cam_y = prev_cam.y;
        self.apply_camera();

        // Зум и перелёт — ОДНОЙ анимацией (new_pan), с явно заданной конечной
        // камерой. Раньше здесь стояли две: CameraAnim везла камеру к цели, а
        // ZoomAnim::new считал камеру САМ, из якоря в центре экрана. В anim::tick
        // зум тикает ПОСЛЕ камеры и переписывает cam_x/cam_y своим значением —
        // то есть перелёт к нужному столу молча выбрасывался на каждом кадре, и
        // обзор всегда всплывал над той точкой, где холст был до него. Ровно
        // поэтому камера «не поднималась над текущим столом». Лента этой беды
        // не знала: там изначально new_pan (см. enter_overview_strip).
        self.camera_anim = None;
        self.zoom_anim = Some(crate::anim::ZoomAnim::new_pan(
            prev_cam, target_cam, prev_zoom, fit_zoom,
            crate::anim::дуг::обзор(),
        ));
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn: overview on ({} workspaces, fit_zoom={:.3})", order.len(), fit_zoom);
    }

    /// «Домашняя» геометрия окна: где и какого размера оно стоит на СВОЁМ столе,
    /// в координатах холста от (0,0) — там же, где раскладка собирает столы
    /// (см. tiling::screen_area). Источник — снимок, снятый при входе в обзор;
    /// у окна, появившегося уже в обзоре, снимка нет, и он заводится на месте.
    fn overview_home(&mut self, win: &Window) -> (Point<i32, Logical>, Size<i32, Logical>) {
        if let Some((_, loc, size)) = self.overview_saved_geo.iter()
            .find(|(w, _, _)| same_window(w, win))
        {
            return (*loc, *size);
        }
        let loc = self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, win))
            .map(|tw| tw.position)
            .unwrap_or_default();
        let size = win.geometry().size;
        self.overview_saved_geo.push((win.clone(), loc, size));
        (loc, size)
    }

    /// Переписать домашнюю геометрию окна. Зовётся, когда окно ПЕРЕЕХАЛО прямо
    /// в обзоре (бросок на другой стол, обмен местами, ресайз): дом — источник
    /// правды и для показа, и для восстановления на выходе.
    fn overview_set_home(
        &mut self,
        win: &Window,
        loc: Point<i32, Logical>,
        size: Option<Size<i32, Logical>>,
    ) {
        self.overview_home(win); // гарантирует наличие записи
        if let Some(e) = self.overview_saved_geo.iter_mut()
            .find(|(w, _, _)| same_window(w, win))
        {
            e.1 = loc;
            if let Some(s) = size {
                e.2 = s;
            }
        }
    }

    /// Сдвиг, который переносит стол `mask` из дома в его ячейку сетки.
    fn overview_offset(&self, mask: u32) -> Point<i32, Logical> {
        self.overview_band_rect(mask).map(|r| r.loc).unwrap_or_default()
    }

    /// Разложить дерево стола `mask` в ДОМАШНЕЙ рабочей области и записать
    /// результат в дома его окон (плюс отдать клиентам новые размеры).
    /// Так правки тайловой раскладки, сделанные в обзоре, живут в тех же
    /// координатах, что и вне его, — обзору остаётся только сдвиг.
    pub(crate) fn overview_apply_tree(&mut self, mask: u32) {
        let Some(area) = self.tile_work_area() else { return };
        let cfg = self.lua_config.dwindle;
        let rects = match self.dwindle_trees.get_mut(&mask) {
            Some(tree) => {
                tree.recalc(area, &cfg);
                tree.leaf_rects()
            }
            None => return,
        };
        for (win, rect) in rects {
            let loc: Point<i32, Logical> = (
                rect.loc.x + GAP_INNER / 2,
                rect.loc.y + GAP_INNER / 2,
            ).into();
            let size: Size<i32, Logical> = (
                (rect.size.w - GAP_INNER).max(1),
                (rect.size.h - GAP_INNER).max(1),
            ).into();
            crate::xwin::set_size(&win, Some(size), crate::xwin::Tiled::Set);
            crate::xwin::configure(&win);
            self.overview_set_home(&win, loc, Some(size));
        }
    }

    /// Расставляет окна по ячейкам столов — КАК ЕСТЬ.
    ///
    /// Раньше обзор пересобирал каждый стол сам: тайловое дерево пересчитывалось
    /// в узкую полосу ячейки, а стол без дерева (monocle, стол с окнами,
    /// поднятыми в плавающий слой через Super+V) раскладывался запасной
    /// dwindle-цепочкой. То есть обзор показывал не столы, а СВОЮ версию столов:
    /// плавающие окна теряли своё место и вставали плитками наравне с
    /// тайловыми, а monocle притворялся тайлингом.
    ///
    /// Теперь окна не перекладываются и не меняют размер вовсе. Все столы
    /// собираются в одном и том же прямоугольнике холста — экран от (0,0)
    /// (см. tiling::screen_area), поэтому «показать стол в его ячейке» — это
    /// ровно СДВИГ на `overview_offset(mask)`, одинаковый для всех его окон.
    /// Взаимное расположение, пропорции и размеры при этом сохраняются точно, а
    /// миниатюрами столы делает отъезд камеры (OVERVIEW_ZOOM), а не сжатие окон.
    pub fn overview_layout(&mut self, w: i32, h: i32) {
        let _ = (w, h); // ячейку даёт overview_band_rect по слоту стола
        // Снимаем с холста только СВОИ окна. Обзор — событие одного монитора:
        // на соседнем экране в этот же момент открыт обычный стол, и его окна
        // обязаны остаться на месте (см. `refresh_tags` и `видимые_теги`, где
        // ровно та же логика). Пока здесь стояли «все окна», вход в обзор
        // сдувал второй экран в пустые обои.
        let свои: Vec<Window> = self.tagged_windows.iter()
            .filter(|tw| !self.монитор_стола(tw.tags).is_some_and(|i| i != self.активный))
            .map(|tw| tw.window.clone())
            .collect();
        for win in &свои {
            self.space.unmap_elem(win);
        }
        self.window_pos_anims.clear();

        let order = self.overview_order.clone();
        let mut placed: Vec<Window> = Vec::new();
        for mask in order {
            let off = self.overview_offset(mask);
            let wins: Vec<Window> = self.tagged_windows.iter()
                .filter(|tw| tw.tags & mask != 0)
                .map(|tw| tw.window.clone())
                .filter(|win| !placed.iter().any(|p| same_window(p, win)))
                .collect();
            for win in wins {
                let (home, size) = self.overview_home(&win);
                // Кламп нужен только тем, кого руками утащили далеко за пределы
                // экрана своего стола (плавающий слой на бесконечном холсте):
                // иначе такое окно въехало бы в чужую ячейку. Тайловые и так
                // лежат внутри рабочей области.
                let loc = self.overview_clamp_loc(mask, home + off, size);
                // Только маппим для отрисовки; tw.position НЕ трогаем (иначе после
                // выхода refresh_tags вернёт окна в позиции обзора).
                self.space.map_element(win.clone(), loc, false);
                placed.push(win);
            }
        }
    }

    /// Canvas-прямоугольники всех столов обзора вместе с признаком «стол ПУСТ»
    /// (ни одного окна). Пустые рисуются не тёмной карточкой, а тонким контуром
    /// — см. build_overview_bg_elements: в ленте этажи стоят вплотную, и подряд
    /// идущие пустые карточки сливались в одно чёрное пятно под столами.
    pub fn overview_band_rects(&self) -> Vec<(Rectangle<i32, Logical>, bool)> {
        let empty = |m: u32| !self.tagged_windows.iter().any(|tw| tw.tags & m != 0);
        if self.overview_strip {
            return self.overview_order.iter()
                .filter_map(|&m| self.overview_strip_floor_rect(m).map(|r| (r, empty(m))))
                .collect();
        }
        let (w, h) = match self.overview_band_size() {
            Some(s) => s,
            None => return Vec::new(),
        };
        self.overview_order.iter()
            .map(|&m| {
                let r = self.slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
                (r, empty(m))
            })
            .collect()
    }

    /// Маска стола, ПОД которым точка `pos` (canvas). None вне столов.
    /// Обзор для всех раскладок — сетка-бэнды, поэтому всегда по slot_rects.
    pub fn overview_workspace_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        if self.overview_strip {
            return self.overview_strip_workspace_at(pos);
        }
        let (w, h) = self.overview_band_size()?;
        for &m in &self.overview_order {
            let r = self.slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
            if pos.x >= r.loc.x as f64 && pos.x <= (r.loc.x + w) as f64
                && pos.y >= r.loc.y as f64 && pos.y <= (r.loc.y + h) as f64
            {
                return Some(m);
            }
        }
        None
    }

    /// Маска БЛИЖАЙШЕГО стола к точке `pos` (canvas) — по расстоянию до центра
    /// слота. Всегда Some, если в обзоре есть хоть один стол. В отличие от
    /// overview_workspace_at (строгое попадание в прямоугольник) не проваливается,
    /// когда точка в зазоре между столами или чуть за краем — нужно для
    /// перетаскивания окна между столами (иначе перенос срабатывает «через раз»).
    pub fn overview_nearest_workspace(&self, pos: Point<f64, Logical>) -> Option<u32> {
        if self.overview_strip {
            return self.overview_strip_nearest_workspace(pos);
        }
        let (w, h) = self.overview_band_size()?;
        let mut best: Option<u32> = None;
        let mut best_d = f64::MAX;
        for &m in &self.overview_order {
            let r = self.slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
            let cx = r.loc.x as f64 + w as f64 / 2.0;
            let cy = r.loc.y as f64 + h as f64 / 2.0;
            let d = (pos.x - cx).powi(2) + (pos.y - cy).powi(2);
            if d < best_d {
                best_d = d;
                best = Some(m);
            }
        }
        best
    }

    /// Окно стола `mask`, под точкой `pos` (canvas), кроме `exclude`.
    fn overview_window_at(
        &self,
        pos: Point<f64, Logical>,
        mask: u32,
        exclude: &Window,
    ) -> Option<Window> {
        self.tagged_windows.iter()
            .filter(|tw| tw.tags & mask != 0 && !same_window(&tw.window, exclude))
            .find(|tw| {
                self.space.element_geometry(&tw.window)
                    .map(|g| g.to_f64().contains(pos))
                    .unwrap_or(false)
            })
            .map(|tw| tw.window.clone())
    }

    /// Поменять местами два окна ОДНОГО стола в обзоре.
    fn overview_swap_windows(&mut self, mask: u32, a: &Window, b: &Window) {
        let in_tree = self.dwindle_trees.get_mut(&mask).is_some_and(|tree| {
            let ok = tree.node_of(a).is_some() && tree.node_of(b).is_some();
            if ok {
                tree.swap(a, b);
            }
            ok
        });
        if in_tree {
            self.overview_apply_tree(mask);
        } else {
            // Окна вне дерева (плавающий слой, monocle) обзор показывает там,
            // где они стоят, — значит и «поменять местами» для них означает
            // обменяться МЕСТАМИ, а не позициями в списке tagged_windows, по
            // которому раньше строилась запасная dwindle-цепочка.
            let (la, _) = self.overview_home(a);
            let (lb, _) = self.overview_home(b);
            self.overview_set_home(a, lb, None);
            self.overview_set_home(b, la, None);
        }
    }

    /// Окно уехало со стола `from` на стол `to`: переносим его и в BSP-деревьях,
    /// вставляя по точке броска. Без этого у стола-приёмника число окон не
    /// сходится с его деревом, и обзор показывает его запасной цепочкой,
    /// перетасовывая заодно все остальные окна этого стола.
    fn overview_move_in_trees(
        &mut self,
        from: u32,
        to: u32,
        window: &Window,
        focal: Point<f64, Logical>,
    ) {
        if let Some(tree) = self.dwindle_trees.get_mut(&from) {
            tree.remove(window);
        }
        // Деревья живут в ДОМАШНИХ координатах (экран от 0,0) — там же, где их
        // считает раскладка вне обзора. Точку броска, снятую с холста обзора,
        // переводим домой тем же сдвигом, каким обзор показывает стол.
        let Some(area) = self.tile_work_area() else { return };
        let off = self.overview_offset(to);
        let focal = Point::<f64, Logical>::from((
            focal.x - off.x as f64,
            focal.y - off.y as f64,
        ));
        let cfg = self.lua_config.dwindle;
        let tree = self.dwindle_trees.entry(to).or_default();
        tree.recalc(area, &cfg);
        let opening_on = tree.closest_node(focal, Some(window));
        tree.insert(window.clone(), opening_on, focal, area, &cfg, None);
        self.overview_apply_tree(to);
        if let Some(tree) = self.dwindle_trees.get(&from) {
            if !tree.windows().is_empty() {
                self.overview_apply_tree(from);
            }
        }
    }

    /// В обзоре: отпустили перетаскивание окна.
    ///  · бросили на ДРУГОЙ стол → окно переезжает на него (и в его дерево);
    ///  · бросили на СОСЕДА по своему столу → окна меняются местами;
    ///  · бросили на пустое место своего стола → тайловое окно возвращается в
    ///    свой слот раскладки, а плавающее (Super+V) остаётся ровно там, куда
    ///    его положили: обзор показывает стол как есть, а значит и правит его
    ///    как есть.
    /// Стол ищем ближайший (не строгое попадание): иначе перенос срабатывает
    /// «через раз», когда окно отпущено в зазоре между столами.
    pub fn overview_reassign(&mut self, window: &Window) {
        if self.overview_strip {
            self.overview_reassign_strip(window);
            return;
        }
        let dropped = match self.space.element_geometry(window) {
            Some(g) => g.loc,
            None => return,
        };
        let pos = match self.space.element_geometry(window) {
            Some(g) => Point::<f64, Logical>::from((
                (g.loc.x + g.size.w / 2) as f64,
                (g.loc.y + g.size.h / 2) as f64,
            )),
            None => return,
        };
        let target = match self.overview_nearest_workspace(pos) {
            Some(m) => m,
            None => return,
        };
        let old = self.overview_mask_of_window(window);
        let floating = self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, window))
            .is_some_and(|tw| tw.floating);

        match old {
            Some(old) if old == target => {
                if let Some(other) = self.overview_window_at(pos, target, window) {
                    self.overview_swap_windows(target, window, &other);
                } else if floating {
                    // Плавающее окно бросили на пустое место своего стола — это
                    // и есть его новое место. Дом считаем от ячейки стола, в
                    // которой оно сейчас лежит.
                    let off = self.overview_offset(target);
                    self.overview_set_home(window, dropped - off, None);
                }
            }
            old => {
                if let Some(tw) = self.tagged_windows.iter_mut()
                    .find(|tw| same_window(&tw.window, window))
                {
                    tw.tags = target;
                } else {
                    return;
                }
                if let (Some(old), false) = (old, floating) {
                    self.overview_move_in_trees(old, target, window, pos);
                } else {
                    // Плавающее окно переезжает на другой стол «как лежит»:
                    // сохраняем его место ОТНОСИТЕЛЬНО нового стола.
                    let off = self.overview_offset(target);
                    self.overview_set_home(window, dropped - off, None);
                }
            }
        }

        // Переразметить обзор — окно встаёт в сетку стола-приёмника.
        if let Some((w, h)) = self.overview_band_size() {
            self.overview_layout(w, h);
        }
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Выйти из обзора на стол окна `window` и сфокусироваться на нём.
    /// Используется при ЛКМ по окну в обзоре.
    pub fn exit_overview_to_window(&mut self, window: &Window) {
        if !self.overview_active {
            return;
        }
        let mask = self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, window))
            .map(|tw| tw.tags);
        if self.overview_strip {
            self.exit_overview_strip(mask, Some(window.clone()));
            return;
        }
        self.exit_overview_immediate(mask);
        // Перефокусируемся на этом окне после view_tag
        crate::xwin::focus(self, &window.clone());
    }

    // ── Обзор ленты (niri): камера отъезжает, окна остаются на местах ────────
    //
    // В Columns столы — это этажи ОДНОЙ вертикальной ленты (см. columns.rs), и
    // окна всех ленточных столов и так лежат на холсте. Значит обзор здесь не
    // должен ничего перекладывать: достаточно отъехать камерой так, чтобы в
    // кадр вошла вся лента — это ровно то, что делает Super+Space (zoom-nav),
    // только с точным вписыванием. Обзорные фишки при этом остаются: клик по
    // окну — выход на него, клик по этажу — выход на этот стол, Super+ЛКМ —
    // перетаскивание окна между колонками и этажами, ПКМ/тап Super — выход.

    /// Этажи, которые показывает ленточный обзор: вся лента столов — занятые
    /// плюс один пустой снизу (niri-модель динамических воркспейсов, см.
    /// niri_ws_count). Пустой этаж тоже нужен: на него бросают окно, чтобы
    /// завести новый стол.
    ///
    /// ЧУЖИЕ столы (те, что помнят НЕ ленточную раскладку — Tile/Dwindle/Float)
    /// в ленточный обзор не попадают вовсе. Их окна на холсте и так не лежат
    /// (см. refresh_tags: в Columns видимы только ленточные столы), так что
    /// рамка такого этажа рисовалась пустой дырой в ленте, а клик по ней
    /// выбрасывал из ленты в чужой режим.
    fn overview_strip_tags(&self) -> Vec<u32> {
        // columns_strip_order уже отсеивает чужие столы, а niri_ws_count меряет
        // ленту в ЭТАЖАХ (занятые + один пустой снизу) — значит первые n этажей
        // порядка и есть вся лента, дырок от чужих тегов в ней нет.
        let n = self.niri_ws_count().clamp(1, 9) as usize;
        let mut tags: Vec<u32> = self.columns_strip_order().into_iter().take(n).collect();
        if tags.is_empty() {
            tags.push(self.viewport.current_tags());
        }
        tags
    }

    /// Прямоугольник этажа `tag` на холсте: во всю ширину его полосы.
    pub fn overview_strip_floor_rect(&self, tag: u32) -> Option<Rectangle<i32, Logical>> {
        let (w, h) = self.overview_band_size()?;
        let y = self.columns_ws_y(tag).round() as i32;
        // Полоса этажа начинается в доме СВОЕГО монитора, а не в нуле холста:
        // на втором мониторе рамка иначе тянулась бы через весь шаг между
        // домами (см. monitors::ШАГ_ДОМА).
        let дом_x = self.монитор_дом().x;
        let mut min_x = дом_x;
        let mut max_x = дом_x + w;
        for tw in self.tagged_windows.iter().filter(|tw| tw.tags & tag != 0) {
            if let Some(g) = self.space.element_geometry(&tw.window) {
                min_x = min_x.min(g.loc.x);
                max_x = max_x.max(g.loc.x + g.size.w + GAP_OUTER);
            }
        }
        Some(Rectangle::new(
            (min_x, y).into(),
            ((max_x - min_x).max(1), h).into(),
        ))
    }

    /// Этаж, на который попала точка `pos` (по Y — этажи идут ровно по высоте
    /// экрана, без зазоров).
    fn overview_strip_workspace_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        let (_, h) = self.overview_band_size()?;
        self.overview_order.iter().copied().find(|&m| {
            let y = self.columns_ws_y(m);
            pos.y >= y && pos.y < y + h as f64
        })
    }

    /// Ближайший этаж по Y — для драга, когда окно отпустили над самым краем.
    fn overview_strip_nearest_workspace(&self, pos: Point<f64, Logical>) -> Option<u32> {
        let (_, h) = self.overview_band_size()?;
        self.overview_order.iter().copied().min_by(|&a, &b| {
            let d = |m: u32| (pos.y - (self.columns_ws_y(m) + h as f64 / 2.0)).abs();
            d(a).total_cmp(&d(b))
        })
    }

    fn enter_overview_strip(&mut self) {
        let Some((w, h)) = self.overview_band_size() else { return };
        // Обзор показывает ленту целиком, поэтому съехавшие этажи (состав ленты
        // менялся, пока на них не смотрели) надо поправить ДО замера кадра —
        // иначе в bbox попадёт пустота от этажа, стоящего не на своём месте.
        self.columns_relayout_strip();
        let prev_cam = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let prev_zoom = self.viewport.zoom;

        self.overview_prev = Some((
            self.viewport.current_tags(),
            prev_cam.x,
            prev_cam.y,
            prev_zoom,
            self.tile_config.layout,
        ));
        self.overview_active = true;
        self.overview_strip = true;
        // Ленточный обзор окна не двигает — снимок геометрии и ячейки сетки
        // (это про сеточный обзор) здесь не нужны и не должны остаться от
        // прошлого захода.
        self.overview_saved_geo.clear();
        self.overview_slots.clear();
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        self.overview_order = self.overview_strip_tags();

        // Кадр обзора: вся лента с небольшим полем по краям. Берём и габариты
        // колонок, и рамки этажей — иначе пустой этаж снизу (куда бросают окно
        // ради нового стола) остался бы за кадром.
        let mut bbox = self.columns_strip_bbox()
            .unwrap_or_else(|| Rectangle::new(self.монитор_дом(), (w, h).into()));
        for m in self.overview_order.clone() {
            if let Some(r) = self.overview_strip_floor_rect(m) {
                bbox = bbox.merge(r);
            }
        }
        let zoom = (0.92
            * (w as f64 / bbox.size.w as f64).min(h as f64 / bbox.size.h as f64))
            .clamp(0.1, 1.0);
        let cx = bbox.loc.x as f64 + bbox.size.w as f64 / 2.0;
        // Камера поднимается над ТЕКУЩИМ этажом, а не над серединой ленты: как
        // и в сеточном обзоре, вход — это подъём над своим столом (по вертикали
        // этажи и различаются). По горизонтали центрируем ленту целиком.
        let cur = self.viewport.current_tags();
        let cy = self.overview_strip_floor_rect(cur)
            .map(|r| r.loc.y as f64 + r.size.h as f64 / 2.0)
            .unwrap_or(bbox.loc.y as f64 + bbox.size.h as f64 / 2.0);
        let to_cam = Point::from((cx - w as f64 / (2.0 * zoom), cy - h as f64 / (2.0 * zoom)));

        self.zoom_anim = Some(crate::anim::ZoomAnim::new_pan(
            prev_cam, to_cam, prev_zoom, zoom, crate::anim::дуг::обзор(),
        ));
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!(
            "dawn: overview (лента) on ({} этажей, zoom={:.3})",
            self.overview_order.len(), zoom,
        );
    }

    /// Выход из ленточного обзора: возвращаем прежний зум и приземляемся на
    /// стол `switch_to` (или на текущий), при `focus_win` — прямо на это окно.
    fn exit_overview_strip(&mut self, switch_to: Option<u32>, focus_win: Option<Window>) {
        if !self.overview_active {
            return;
        }
        self.overview_active = false;
        self.overview_strip = false;
        self.overview_exit_pending = false;
        self.overview_exit_target_ws = None;
        self.momentum.stop();

        let cur_zoom = self.viewport.zoom;
        let cur_cam = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let prev = self.overview_prev.take();
        let prev_zoom = prev.map(|p| p.3).unwrap_or(1.0);
        let prev_cam = prev
            .map(|p| Point::from((p.1, p.2)))
            .unwrap_or(cur_cam);

        // ВАЖНО: сначала возвращаем НОРМАЛЬНЫЙ кадр (зум и камеру, с которыми
        // заходили), и только потом переключаем стол и подтягиваем прокрутку.
        // Вся арифметика ленты (columns_fit_view и т.п.) меряет экран в
        // canvas-единицах — при обзорном зуме он «шире» настоящего, и цели
        // прокрутки посчитались бы не туда.
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        self.viewport.zoom = prev_zoom;
        self.viewport.cam_x = prev_cam.x;
        self.viewport.cam_y = prev_cam.y;
        // Плавающие окна ленты держатся экрана и едут за дельтой камеры
        // (columns_pin_floating). Обзор камеру уже увёз, поэтому базу дельты
        // переставляем на восстановленный кадр — иначе первый же apply_camera
        // после обзора швырнул бы плавающие окна на всю эту дельту.
        self.columns_float_cam = (prev_cam.x, prev_cam.y);
        // И СРАЗУ применяем (строго ПОСЛЕ columns_float_cam — apply_camera тянет
        // за собой плавающие окна): apply_camera выставляет output'у scale = зум,
        // а вся арифметика ленты ниже (arrange в view_tag,
        // columns_scroll_to_active) меряет экран через ЛОГИЧЕСКУЮ геометрию
        // output'а. Без этого вызова она считалась бы по обзорному зуму — экран
        // «шире» втрое, колонки раскладываются на несуществующую ширину, а цель
        // прокрутки уезжает мимо.
        self.apply_camera();

        if let Some(mask) = switch_to {
            if mask != self.viewport.current_tags() {
                self.view_tag(mask);
            }
        }
        if let Some(win) = focus_win {
            self.columns_set_active_to_window(&win);
            crate::xwin::focus(self, &win);
        }
        self.columns_scroll_to_active();
        // Куда в итоге едет лента: цель уже поставленных анимаций (перелёт на
        // этаж + прокрутка к активной колонке) — их мы забираем себе.
        let to_x = self.camera_anim.take().map(|a| a.to.x).unwrap_or(self.viewport.cam_x);
        let to_cam = Point::from((to_x, self.columns_cur_y()));

        // Обратно к столу — одним движением: зум и камера едут вместе.
        self.viewport.zoom = cur_zoom;
        self.viewport.cam_x = cur_cam.x;
        self.viewport.cam_y = cur_cam.y;
        self.columns_float_cam = (cur_cam.x, cur_cam.y);
        self.zoom_anim = Some(crate::anim::ZoomAnim::new_pan(
            cur_cam, to_cam, cur_zoom, prev_zoom, crate::anim::дуг::обзор(),
        ));
        self.apply_camera();
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn: overview (лента) off");
    }

    /// Отпустили перетаскивание окна в ленточном обзоре: окно встаёт в полосу
    /// того этажа, над которым его бросили — новой колонкой или в стопку, как
    /// показывала бы подсказка вставки в niri.
    fn overview_reassign_strip(&mut self, window: &Window) {
        let Some(g) = self.space.element_geometry(window) else { return };
        let pos = Point::<f64, Logical>::from((
            (g.loc.x + g.size.w / 2) as f64,
            (g.loc.y + g.size.h / 2) as f64,
        ));
        let Some(target) = self.overview_strip_workspace_at(pos)
            .or_else(|| self.overview_strip_nearest_workspace(pos))
        else {
            return;
        };
        // Полоса пересобирается сама и тянет за собой прокрутку/фокус — а обзор
        // обязан остаться в своём кадре, поэтому камеру возвращаем как была.
        let cam = (self.viewport.cam_x, self.viewport.cam_y, self.viewport.zoom);
        self.columns_drop_window_on_ws(window, target, pos.x);
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        self.viewport.cam_x = cam.0;
        self.viewport.cam_y = cam.1;
        self.viewport.zoom = cam.2;
        self.apply_camera();
        // Этажи могли появиться/опустеть — пересчитываем набор рамок.
        self.overview_order = self.overview_strip_tags();
        self.request_plane_reset();
        self.request_redraw();
    }
}
