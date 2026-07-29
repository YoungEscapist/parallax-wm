//! niri-подобный обзор рабочих столов (тап Super).
//!
//! Тап по Super открывает/закрывает обзор: рабочие столы (воркспейсы с окнами)
//! раскладываются 2D-сеткой ВОКРУГ ЦЕНТРАЛЬНОГО (текущего) стола, холст
//! отдаляется. В обзоре:
//!  · ОКНА не трогаются — только ПАН (ЛКМ-драг по пустому месту / 2-пальца) и
//!    ЗУМ (колесо);
//!  · Win+Alt+ЛКМ-драг → перетащить сам стол: он переезжает по ячейкам сетки
//!    вслед за курсором, на отпускании встаёт в ближайшую (потянул влево →
//!    слева от центрального); занятая ячейка → столы меняются местами;
//!  · повторный тап Super или ПКМ → выйти на стол ПОД КУРСОРОМ (плавный перелёт);
//!  · LMB-клик по столу → плавный перелёт к нему.
//!
//! При выходе на ДРУГОЙ стол: камера летит к его ячейке в обзоре, затем
//! финализируется выход (восстановление layout/зума). При выходе на тот же
//! стол → просто выход (без анимации).

use std::collections::HashMap;
use std::time::Duration;

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::anim::CameraAnim;
use crate::state::Dawn;
use crate::tiling::{dwindle_rects, GAP_INNER, GAP_OUTER, Layout};

/// Зазор между ячейками сетки столов (canvas px).
const BAND_GAP: i32 = 140;
/// Уровень отдаления в обзоре.
const OVERVIEW_ZOOM: f64 = 0.5;
/// Длительность анимации перелёта между столами.
const OVERVIEW_FLY_MS: u64 = 280;

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

    pub fn toggle_overview(&mut self) {
        // Если идёт exit-анимация — не трогаем, даём завершиться.
        if self.overview_exit_pending {
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
            self.camera_anim = Some(CameraAnim::new(from, target_cam, Duration::from_millis(OVERVIEW_FLY_MS)));
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
        self.overview_active = false;
        self.overview_drag_ws = None;
        self.overview_exit_pending = false;
        self.overview_exit_target_ws = None;

        if let Some((tag, cam_x, cam_y, zoom, layout)) = self.overview_prev.take() {
            self.viewport.tagset[self.viewport.seltags] = tag;
            self.tile_config.layout = layout;
            self.refresh_tags();
            // ВАЖНО: строго после refresh_tags — он сам переписывает tw.position
            // текущей (обзорной) позицией окна, так что снимок надо накатывать
            // поверх него, а не до.
            self.restore_pre_overview_geometry();
            self.arrange();
            self.viewport.zoom = zoom;
            self.viewport.cam_x = cam_x;
            self.viewport.cam_y = cam_y;
            self.apply_camera();
        }
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        if let Some(mask) = switch_to {
            self.view_tag(mask);
            // Обзор (overview_layout) сдвигает окна в сетку и сжимает их, а
            // refresh_tags сохраняет эти обзорные позиции в tw.position. Для
            // Tile/Monocle view_tag посещённого стола НЕ вызывает arrange и
            // улетает камерой к устаревшей per-tag позиции → на экране одно
            // мелкое окно под курсором вместо всего стола. Показываем стол
            // ЦЕЛИКОМ как собран: стандартный кадр (zoom 1, cam 0,0) + переразметка.
            // Columns раскладывается из своей модели прямо в view_tag — не трогаем.
            if matches!(self.tile_config.layout, Layout::Tile | Layout::Monocle) {
                self.camera_anim = None;
                self.zoom_anim = None;
                self.viewport.zoom = 1.0;
                self.viewport.cam_x = 0.0;
                self.viewport.cam_y = 0.0;
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
        if let Some(e) = self.overview_saved_geo.iter_mut()
            .find(|(w, _, _)| same_window(w, window))
        {
            e.2 = size;
        }
    }

    /// Физический размер output'а = размер одной ячейки стола.
    pub fn overview_band_size(&self) -> Option<(i32, i32)> {
        let output = self.space.outputs().next()?;
        let mode = output.current_mode()?;
        Some((mode.size.w, mode.size.h))
    }

    /// Canvas-прямоугольник стола по его слоту (ячейке сетки).
    fn slot_rect(slot: (i32, i32), w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(
            (slot.0 * (w + BAND_GAP), slot.1 * (h + BAND_GAP)).into(),
            (w, h).into(),
        )
    }

    /// Первая свободная ячейка сетки — кольцами ВОКРУГ центра (0,0), а не
    /// колонкой вниз: столы должны обступать текущий со всех четырёх сторон
    /// (справа, слева, снизу, сверху), а не выстраиваться в одну полосу.
    /// Порядок внутри кольца — по часовой от правого соседа, так что при 5
    /// столах получается «крест»: центр и по одному соседу с каждой стороны,
    /// и только с шестого стола идут диагонали.
    /// Диапазон тот же, что у снапа перетаскивания стола (±9).
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
        Some(Self::slot_rect(*self.overview_slots.get(&mask).unwrap_or(&(0, 0)), w, h))
    }

    /// Область ВНУТРИ стола, в которой разрешено находиться окну: рамка минус
    /// внешний отступ. За её пределы окно в обзоре не выпускают ни перетаскивание
    /// (move_grab), ни ресайз (resize_grab) — стол работает как жёсткая рамка.
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
        let Some(area) = self.overview_window_area(mask) else { return loc };
        let max_x = area.loc.x + (area.size.w - size.w).max(0);
        let max_y = area.loc.y + (area.size.h - size.h).max(0);
        Point::from((
            loc.x.clamp(area.loc.x, max_x),
            loc.y.clamp(area.loc.y, max_y),
        ))
    }

    /// Ячейка сетки, в которую попадает точка `pos` (canvas).
    fn cell_at(pos: Point<f64, Logical>, w: i32, h: i32) -> (i32, i32) {
        let stride_x = (w + BAND_GAP) as f64;
        let stride_y = (h + BAND_GAP) as f64;
        (
            (pos.x / stride_x).round() as i32,
            (pos.y / stride_y).round() as i32,
        )
    }

    /// Ставит стол `mask` в ячейку `cell`; занята другим столом → столы
    /// меняются ячейками местами. Возвращает true, если расстановка изменилась
    /// (вызывающему нужен overview_layout).
    fn overview_place_workspace(&mut self, mask: u32, cell: (i32, i32)) -> bool {
        let cur = *self.overview_slots.get(&mask).unwrap_or(&(0, 0));
        if cur == cell || cell.0.abs() > 9 || cell.1.abs() > 9 {
            return false;
        }
        let occupant = self.overview_slots.iter()
            .find(|(t, &c)| **t != mask && c == cell)
            .map(|(t, _)| *t);
        if let Some(other) = occupant {
            self.overview_slots.insert(other, cur);
        }
        self.overview_slots.insert(mask, cell);
        tracing::info!(
            "dawn: overview workspace {:#b} → cell ({},{})",
            mask, cell.0, cell.1
        );
        true
    }

    /// Живой драг стола мышью (Win+Alt+ЛКМ): стол переезжает в ячейку под
    /// курсором, вместе со всеми своими окнами. Переклеиваем раскладку только
    /// на смене ячейки — иначе overview_layout молотил бы на каждое событие
    /// движения мыши.
    pub fn overview_drag_workspace_to(&mut self, mask: u32, pos: Point<f64, Logical>) {
        if !self.overview_active {
            return;
        }
        let Some((w, h)) = self.overview_band_size() else { return };
        if self.overview_place_workspace(mask, Self::cell_at(pos, w, h)) {
            self.overview_layout(w, h);
            self.request_plane_reset();
        }
        self.request_redraw();
    }

    /// Переставить ВЕСЬ стол под курсором в соседнюю ячейку сетки обзора с
    /// клавиатуры. По умолчанию ни на что не забиндено (стол таскают мышью,
    /// Win+Alt+ЛКМ), но действие `move_workspace_slot` доступно из config.lua.
    pub fn overview_move_workspace_slot(&mut self, dx: i32, dy: i32) {
        if !self.overview_active || self.overview_exit_pending {
            return;
        }
        let Some((w, h)) = self.overview_band_size() else { return };
        let mask = self.overview_workspace_at(self.pointer_location)
            .or_else(|| self.overview_nearest_workspace(self.pointer_location))
            .unwrap_or_else(|| self.viewport.current_tags());
        let cur = *self.overview_slots.get(&mask).unwrap_or(&(0, 0));
        if self.overview_place_workspace(mask, (cur.0 + dx, cur.1 + dy)) {
            self.overview_layout(w, h);
            self.request_plane_reset();
            self.request_redraw();
        }
    }

    /// Camera position to center a given slot on screen at current zoom.
    fn center_cam_on_slot(&self, slot: (i32, i32), w: i32, h: i32) -> Point<f64, Logical> {
        let rect = Self::slot_rect(slot, w, h);
        let cx = rect.loc.x as f64 + w as f64 / 2.0;
        let cy = rect.loc.y as f64 + h as f64 / 2.0;
        let zoom = self.viewport.zoom;
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
            let r = Self::slot_rect(slot, w, h);
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
            tracing::debug!("dawn: обзор не открывается в тайловом режиме");
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
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.overview_drag_ws = None;

        // Обзор для ВСЕХ раскладок (включая Columns/niri) — единая многостоловая
        // сетка-бэнды: каждый стол = ячейка сетки со своими окнами (dwindle),
        // камера вписывает все столы (overview_fit_all). Раньше Columns имел
        // отдельную ветку, показывавшую только колонки ТЕКУЩЕГО стола — из-за
        // чего в niri другие воркспейсы в обзоре не отображались. На выходе
        // layout восстанавливается в Columns (exit_overview_immediate → view_tag).

        // Занятые столы: текущий + все с окнами, но ТОЛЬКО в том же режиме.
        //
        // Столы помнят свою раскладку (Dawn::tag_layouts), и смешивать их в
        // одном обзоре нельзя: выход на ленточный стол из тайлового переключает
        // режим целиком, окна доехавшего стола пересобираются чужой раскладкой,
        // а снимок геометрии остаётся от прежней. Показываем только совместимые.
        let cur = self.viewport.current_tags();
        let cur_layout = self.tile_config.layout;
        let mut order: Vec<u32> = vec![cur];
        // В niri-режиме обзор показывает ТОЛЬКО текущий стол. Лента и так своя
        // у каждого стола, а переключение между ними — Win+цифра и
        // Super+PageUp/Down; сетка чужих столов в обзоре здесь лишняя.
        if cur_layout != Layout::Columns {
            for i in 0..9u32 {
                let m = 1u32 << i;
                if m == cur || !self.tagged_windows.iter().any(|tw| tw.tags & m != 0) {
                    continue;
                }
                let m_layout = *self.tag_layouts.get(&m).unwrap_or(&Layout::Tile);
                if m_layout == cur_layout {
                    order.push(m);
                }
            }
        }
        self.overview_order = order.clone();

        // Ячейки сетки ПЕРЕЖИВАЮТ выход из обзора: стол, который перетащили
        // влево от соседа (мышью или Win+Alt+стрелка), должен остаться слева и
        // в следующем обзоре. Сохраняем прежнюю ячейку столу, если её не занял
        // кто-то раньше по порядку; остальным выдаём первую свободную.
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
        let (fit_zoom, target_cam) = self.overview_fit_all(w, h);
        self.viewport.zoom = prev_zoom;
        self.viewport.cam_x = prev_cam.x;
        self.viewport.cam_y = prev_cam.y;
        self.apply_camera();

        self.camera_anim = Some(CameraAnim::new(
            prev_cam, target_cam, Duration::from_millis(OVERVIEW_FLY_MS),
        ));
        if (fit_zoom - prev_zoom).abs() > 0.01 {
            self.zoom_anim = Some(crate::anim::ZoomAnim::new(
                Point::from((w as f64 / 2.0, h as f64 / 2.0)),
                Point::from((w as f64 / 2.0, h as f64 / 2.0)),
                prev_zoom, fit_zoom, Duration::from_millis(OVERVIEW_FLY_MS),
            ));
        }
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn: overview on ({} workspaces, fit_zoom={:.3})", order.len(), fit_zoom);
    }

    /// Раскладывает окна каждого стола (dwindle) в его ячейке сетки (по слоту).
    pub fn overview_layout(&mut self, w: i32, h: i32) {
        let all: Vec<Window> = self.tagged_windows.iter().map(|tw| tw.window.clone()).collect();
        for win in &all {
            self.space.unmap_elem(win);
        }
        self.window_pos_anims.clear();

        let order = self.overview_order.clone();
        let slots: HashMap<u32, (i32, i32)> = self.overview_slots.clone();
        let mut placed: Vec<Window> = Vec::new();
        for mask in order {
            let slot = *slots.get(&mask).unwrap_or(&(0, 0));
            let brect = Self::slot_rect(slot, w, h);
            let wins: Vec<Window> = self.tagged_windows.iter()
                .filter(|tw| tw.tags & mask != 0)
                .map(|tw| tw.window.clone())
                .filter(|win| !placed.iter().any(|p| same_window(p, win)))
                .collect();
            let n = wins.len();
            if n == 0 {
                continue;
            }
            let band = Rectangle::new(
                (brect.loc.x + GAP_OUTER, brect.loc.y + GAP_OUTER).into(),
                ((w - GAP_OUTER * 2).max(1), (h - GAP_OUTER * 2).max(1)).into(),
            );
            // Стол показываем ТАК, КАК ОН СОБРАН: если у этого набора тегов
            // есть BSP-дерево (Tile, см. dwindle.rs) и оно описывает ровно эти
            // окна — раскладываем миниатюры по нему. Иначе (стол ни разу не
            // тайлился, часть окон плавающие) — простой dwindle-цепочкой.
            let cfg = self.lua_config.dwindle;
            let tree_rects = self.dwindle_trees.get_mut(&mask).and_then(|tree| {
                let in_tree = tree.windows();
                let matches = in_tree.len() == n
                    && in_tree.iter().all(|w| wins.iter().any(|v| same_window(v, w)));
                matches.then(|| {
                    tree.recalc(band, &cfg);
                    tree.leaf_rects()
                })
            });
            let pairs: Vec<(Window, Rectangle<i32, Logical>)> = match tree_rects {
                Some(v) => v,
                None => wins.iter().cloned().zip(dwindle_rects(band, n, true)).collect(),
            };
            for (win, rect) in &pairs {
                let loc: Point<i32, Logical> = (
                    rect.loc.x + GAP_INNER / 2,
                    rect.loc.y + GAP_INNER / 2,
                ).into();
                let size = ((rect.size.w - GAP_INNER).max(1), (rect.size.h - GAP_INNER).max(1));
                crate::xwin::set_size(win, Some(size.into()), crate::xwin::Tiled::Set);
                crate::xwin::configure(win);
                // Только маппим для отрисовки; tw.position НЕ трогаем (иначе после
                // выхода refresh_tags вернёт окна в позиции обзора).
                self.space.map_element(win.clone(), loc, false);
                placed.push(win.clone());
            }
        }
    }

    /// Canvas-прямоугольники всех столов (для отрисовки фона в обзоре).
    pub fn overview_band_rects(&self) -> Vec<Rectangle<i32, Logical>> {
        let (w, h) = match self.overview_band_size() {
            Some(s) => s,
            None => return Vec::new(),
        };
        self.overview_order.iter()
            .map(|m| Self::slot_rect(*self.overview_slots.get(m).unwrap_or(&(0, 0)), w, h))
            .collect()
    }

    /// Маска стола, ПОД которым точка `pos` (canvas). None вне столов.
    /// Обзор для всех раскладок — сетка-бэнды, поэтому всегда по slot_rects.
    pub fn overview_workspace_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        let (w, h) = self.overview_band_size()?;
        for &m in &self.overview_order {
            let r = Self::slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
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
        let (w, h) = self.overview_band_size()?;
        let mut best: Option<u32> = None;
        let mut best_d = f64::MAX;
        for &m in &self.overview_order {
            let r = Self::slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
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

    /// Двигает все окна стола на дельту (canvas), не выпуская их за его рамку.
    /// Больше не используется живым драгом стола (Win+Alt+ЛКМ) — тот переставляет
    /// стол по ячейкам сетки (см. overview_drag_workspace_to), а сдвиг окон
    /// внутри рамки выглядел как «стол стоит на месте, окна дёргаются».
    #[allow(dead_code)]
    pub fn overview_move_workspace_windows(&mut self, mask: u32, dx: f64, dy: f64) {
        let (dxi, dyi) = (dx.round() as i32, dy.round() as i32);
        if dxi == 0 && dyi == 0 {
            return;
        }
        // Границы бэнда этого воркспейса — окна не должны уходить за них
        let (bw, bh) = match self.overview_band_size() {
            Some(s) => s,
            None => return,
        };
        let slot = *self.overview_slots.get(&mask).unwrap_or(&(0, 0));
        let brect = Self::slot_rect(slot, bw, bh);
        let margin = GAP_OUTER;

        let wins: Vec<Window> = self.tagged_windows.iter()
            .filter(|tw| tw.tags & mask != 0)
            .map(|tw| tw.window.clone())
            .collect();
        for win in wins {
            if let Some(g) = self.space.element_geometry(&win) {
                let x = (g.loc.x + dxi).clamp(brect.loc.x + margin, brect.loc.x + bw - g.size.w - margin);
                let y = (g.loc.y + dyi).clamp(brect.loc.y + margin, brect.loc.y + bh - g.size.h - margin);
                let loc: Point<i32, Logical> = (x, y).into();
                self.space.map_element(win, loc, false);
            }
        }
        self.request_redraw();
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
        if !in_tree {
            // У стола нет BSP-дерева (плавающий или ни разу не тайлившийся): в
            // обзоре его миниатюры раскладываются dwindle-цепочкой по порядку
            // tagged_windows — значит и местами они меняются в этом списке.
            let ia = self.tagged_windows.iter().position(|tw| same_window(&tw.window, a));
            let ib = self.tagged_windows.iter().position(|tw| same_window(&tw.window, b));
            if let (Some(ia), Some(ib)) = (ia, ib) {
                self.tagged_windows.swap(ia, ib);
            }
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
        let Some(area) = self.overview_window_area(to) else { return };
        let cfg = self.lua_config.dwindle;
        let tree = self.dwindle_trees.entry(to).or_default();
        tree.recalc(area, &cfg);
        let opening_on = tree.closest_node(focal, Some(window));
        tree.insert(window.clone(), opening_on, focal, area, &cfg, None);
        tree.recalc(area, &cfg);
    }

    /// В обзоре: отпустили перетаскивание окна.
    ///  · бросили на ДРУГОЙ стол → окно переезжает на него (и в его дерево);
    ///  · бросили на СОСЕДА по своему столу → окна меняются местами;
    ///  · бросили на пустое место своего стола → окно возвращается в сетку.
    /// Стол ищем ближайший (не строгое попадание): иначе перенос срабатывает
    /// «через раз», когда окно отпущено в зазоре между столами.
    pub fn overview_reassign(&mut self, window: &Window) {
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

    /// Поменять местами содержимое двух воркспейсов: все окна с тегом `a`
    /// переезжают на тег `b`, и наоборот. Вызывается при отпускании перетаскивания
    /// стола в обзоре Super+ЛКМ (записан `overview_drag_ws`).
    pub fn overview_swap_workspaces(&mut self, a: u32, b: u32) {
        if a == b {
            return;
        }
        for tw in &mut self.tagged_windows {
            if tw.tags == a {
                tw.tags = b;
            } else if tw.tags == b {
                tw.tags = a;
            }
        }
        if let Some((w, h)) = self.overview_band_size() {
            self.overview_layout(w, h);
        }
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Отпустили перетаскиваемый стол: снапим в ближайшую ячейку сетки по
    /// позиции курсора `pos`. Сначала ищет смежные (col±1/row±1) с другими
    /// столами ячейки — «магнит» на все 4 стороны. Если не нашёл — обычная
    /// сетка. Занятая ячейка → свап содержимого со столом-владельцем.
    pub fn overview_snap_workspace(&mut self, mask: u32, pos: Point<f64, Logical>) {
        let (w, h) = match self.overview_band_size() {
            Some(s) => s,
            None => return,
        };
        let raw_cell = Self::cell_at(pos, w, h);

        // Ищем ближайшую СВОБОДНУЮ ячейку, смежную с занятыми (магнит).
        // Если не нашли — используем raw_cell.
        let cell = self.find_best_snap_cell(mask, raw_cell, w, h);

        // Свап, если ячейка занята другим столом.
        let occupant = self.overview_slots.iter()
            .find(|(t, &c)| **t != mask && c == cell)
            .map(|(t, _)| *t);
        if let Some(other) = occupant {
            let old = *self.overview_slots.get(&mask).unwrap_or(&(0, 0));
            self.overview_slots.insert(other, old);
        }
        self.overview_slots.insert(mask, cell);
        self.overview_layout(w, h);
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Выбирает лучшую ячейку для стола: предпочитает смежные с занятыми
    /// столами ячейки (col±1/row±1) — «магнит» на все 4 стороны. Если
    /// ни одна смежная ячейка не подходит — возвращает raw_cell.
    fn find_best_snap_cell(&self, mask: u32, raw_cell: (i32, i32), _w: i32, _h: i32) -> (i32, i32) {
        let occupied: std::collections::HashSet<(i32, i32)> = self.overview_slots.iter()
            .filter(|(t, _)| **t != mask)
            .map(|(_, &c)| c)
            .collect();
        if occupied.is_empty() {
            return raw_cell;
        }

        // Сначала пробуем raw_cell
        if !occupied.contains(&raw_cell) {
            // Проверяем, смежна ли raw_cell с каким-нибудь занятым столом
            let adj = [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)];
            for &(dc, dr) in &adj {
                if occupied.contains(&(raw_cell.0 + dc, raw_cell.1 + dr)) {
                    return raw_cell; // магнит сработал!
                }
            }
        }

        // Ищем смежные свободные ячейки вокруг занятых, ближайшие к курсору
        let mut best: Option<(i32, (i32, i32))> = None;
        let mut best_dist = i64::MAX;
        for &occ in &occupied {
            for (dc, dr) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
                let cand = (occ.0 + dc, occ.1 + dr);
                if cand == raw_cell {
                    // Точное совпадение с сырой клеткой — отличный кандидат
                    return raw_cell;
                }
                if occupied.contains(&cand) {
                    continue; // занята
                }
                if cand.0 < -9 || cand.0 > 9 || cand.1 < -9 || cand.1 > 9 {
                    continue; // слишком далеко
                }
                let d = (cand.0 as i64 - raw_cell.0 as i64).pow(2)
                    + (cand.1 as i64 - raw_cell.1 as i64).pow(2);
                if d < best_dist {
                    best_dist = d;
                    best = Some((0, cand));
                }
            }
        }

        best.map(|(_, c)| c).unwrap_or(raw_cell)
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
        self.exit_overview_immediate(mask);
        // Перефокусируемся на этом окне после view_tag
        crate::xwin::focus(self, &window.clone());
    }
}
