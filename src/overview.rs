//! niri-подобный обзор рабочих столов (тап Super).
//!
//! Тап по Super открывает/закрывает обзор: рабочие столы (воркспейсы с окнами)
//! раскладываются 2D-сеткой ВОКРУГ ЦЕНТРАЛЬНОГО (текущего) стола, холст
//! отдаляется. В обзоре:
//!  · ОКНА не трогаются — только ПАН (ЛКМ-драг по пустому месту / 2-пальца) и
//!    ЗУМ (колесо);
//!  · ЛКМ-драг ПО СТОЛУ → перетащить сам стол; на отпускании он встаёт в
//!    ближайшую ячейку сетки (потянул влево → слева от центрального);
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
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle},
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
    a.toplevel().zip(b.toplevel())
        .map(|(x, y)| x.wl_surface() == y.wl_surface())
        .unwrap_or(false)
}

impl Dawn {
    pub fn toggle_overview(&mut self) {
        // Если идёт exit-анимация — не трогаем, даём завершиться.
        if self.overview_exit_pending {
            return;
        }
        // Не открываем обзор во Float.
        if !self.overview_active && self.tile_config.layout == Layout::Float {
            return;
        }
        if self.overview_active {
            // Win tap: мгновенный выход на стол под курсором.
            // Сбрасываем фокус с окон — иначе sloppy focus ловит окно
            // под курсором после выхода, а мы хотим просто рабочий стол.
            let mask = self.overview_workspace_at(self.pointer_location);
            self.exit_overview_immediate(mask);
            if let Some(kb) = self.seat.get_keyboard() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                kb.set_focus(self, None, serial);
            }
            for w in self.space.elements() {
                w.set_activated(false);
                if let Some(t) = w.toplevel() { t.send_pending_configure(); }
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
        }
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn: overview off");
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

    pub fn enter_overview(&mut self) {
        if self.overview_active || self.overview_exit_pending {
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

        // ── Columns: niri-стиль — окна остаются в колонках, камера отдаляется
        // так, чтобы все колонки влезли в кадр.
        if self.tile_config.layout == Layout::Columns {
            let cur = self.viewport.current_tags();
            let mut order: Vec<u32> = vec![cur];
            for i in 0..9u32 {
                let m = 1u32 << i;
                if m != cur && self.tagged_windows.iter().any(|tw| tw.tags & m != 0) {
                    order.push(m);
                }
            }
            self.overview_order = order;

            let total_w: f64 = self.columns.columns.iter().map(|col| {
                (col.width.factor() * w as f64).round() + GAP_INNER as f64
            }).sum::<f64>() + GAP_OUTER as f64 * 2.0;
            let fit_zoom = (w as f64 / total_w.max(1.0)).clamp(0.15, 1.0);
            let cam_x = (total_w - w as f64 / fit_zoom).max(0.0) / 2.0;
            let cam_y = 0.0;

            // Плавный перелёт в overview
            self.viewport.zoom = prev_zoom;
            self.viewport.cam_x = prev_cam.x;
            self.viewport.cam_y = prev_cam.y;
            self.apply_camera();
            let target = Point::from((cam_x, cam_y));
            self.camera_anim = Some(CameraAnim::new(
                prev_cam, target, Duration::from_millis(OVERVIEW_FLY_MS),
            ));
            if (fit_zoom - prev_zoom).abs() > 0.01 {
                self.zoom_anim = Some(crate::anim::ZoomAnim::new(
                    Point::from((total_w / 2.0, 0.0)),
                    Point::from((w as f64 / 2.0, h as f64 / 2.0)),
                    prev_zoom, fit_zoom, Duration::from_millis(OVERVIEW_FLY_MS),
                ));
            }
            self.request_plane_reset();
            self.request_redraw();
            tracing::info!("dawn: overview (columns, zoom={:.3})", fit_zoom);
            return;
        }

        // Занятые столы: текущий + все с окнами.
        let cur = self.viewport.current_tags();
        let mut order: Vec<u32> = vec![cur];
        for i in 0..9u32 {
            let m = 1u32 << i;
            if m != cur && self.tagged_windows.iter().any(|tw| tw.tags & m != 0) {
                order.push(m);
            }
        }
        self.overview_order = order.clone();

        self.overview_slots.clear();
        self.overview_slots.insert(cur, (0, 0));
        let mut k = 1;
        for &m in order.iter().skip(1) {
            self.overview_slots.insert(m, (0, k));
            k += 1;
        }

        // Раскладываем окна в overview layout
        self.overview_layout(w, h);

        // Плавный перелёт: zoom+камера летят к обзорной позиции
        let target_cam = self.center_cam_on_slot((0, 0), w, h);
        self.viewport.zoom = prev_zoom;
        self.viewport.cam_x = prev_cam.x;
        self.viewport.cam_y = prev_cam.y;
        self.apply_camera();

        self.camera_anim = Some(CameraAnim::new(
            prev_cam, target_cam, Duration::from_millis(OVERVIEW_FLY_MS),
        ));
        if (OVERVIEW_ZOOM - prev_zoom).abs() > 0.01 {
            self.zoom_anim = Some(crate::anim::ZoomAnim::new(
                Point::from((w as f64 / 2.0, h as f64 / 2.0)),
                Point::from((w as f64 / 2.0, h as f64 / 2.0)),
                prev_zoom, OVERVIEW_ZOOM, Duration::from_millis(OVERVIEW_FLY_MS),
            ));
        }
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn: overview on ({} workspaces)", order.len());
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
            let rects = dwindle_rects(band, n, true);
            for (win, rect) in wins.iter().zip(rects.iter()) {
                let loc: Point<i32, Logical> = (
                    rect.loc.x + GAP_INNER / 2,
                    rect.loc.y + GAP_INNER / 2,
                ).into();
                let size = ((rect.size.w - GAP_INNER).max(1), (rect.size.h - GAP_INNER).max(1));
                if let Some(t) = win.toplevel() {
                    t.with_pending_state(|s| {
                        s.size = Some(size.into());
                        s.states.set(xdg_toplevel::State::TiledLeft);
                        s.states.set(xdg_toplevel::State::TiledRight);
                        s.states.set(xdg_toplevel::State::TiledTop);
                        s.states.set(xdg_toplevel::State::TiledBottom);
                    });
                    t.send_pending_configure();
                }
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
    /// В Tile/Monocle: проверяет slot_rects. В Columns: ищет окно под курсором.
    pub fn overview_workspace_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        if self.tile_config.layout == Layout::Columns {
            // В columns-обзоре окна остаются на своих местах — ищем под курсором.
            let (window, _) = self.space.element_under(pos)?;
            // Находим тег этого окна.
            self.tagged_windows.iter()
                .find(|tw| {
                    tw.window.toplevel().zip(window.toplevel())
                        .map(|(a, b)| a.wl_surface() == b.wl_surface())
                        .unwrap_or(false)
                })
                .map(|tw| tw.tags)
        } else {
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
    }

    /// Живое перетаскивание стола: двигает все его окна на дельту (canvas).
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

    /// В обзоре: перенести окно на тот рабочий стол, под которым оно сейчас висит.
    /// Вызывается при отпускании драга окна (Super+ЛКМ или Super+2пальца).
    /// Центр окна проверяется по overview_band_rects(); если не попадает ни в один
    /// бэнд или это уже его текущий стол — ничего не делаем.
    pub fn overview_reassign(&mut self, window: &Window) {
        let pos = match self.space.element_geometry(window) {
            Some(g) => Point::<f64, Logical>::from((
                (g.loc.x + g.size.w / 2) as f64,
                (g.loc.y + g.size.h / 2) as f64,
            )),
            None => return,
        };
        let target = match self.overview_workspace_at(pos) {
            Some(m) => m,
            None => return,
        };
        // Найти окно в tagged_windows и сменить ему тег.
        if let Some(tw) = self.tagged_windows.iter_mut()
            .find(|tw| same_window(&tw.window, window))
        {
            if tw.tags == target {
                return; // уже на этом столе
            }
            tw.tags = target;
        }
        // Переразметить обзор.
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
        let stride_x = (w + BAND_GAP) as f64;
        let stride_y = (h + BAND_GAP) as f64;
        let raw_cell = (
            (pos.x / stride_x).round() as i32,
            (pos.y / stride_y).round() as i32,
        );

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
        if let Some(t) = window.toplevel() {
            let surface = t.wl_surface().clone();
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            self.space.raise_element(window, true);
            window.set_activated(true);
            for w in self.space.elements() {
                if w.toplevel().zip(window.toplevel())
                    .map(|(a, b)| a.wl_surface() != b.wl_surface())
                    .unwrap_or(true)
                {
                    w.set_activated(false);
                    if let Some(t) = w.toplevel() { t.send_pending_configure(); }
                }
            }
            if let Some(kb) = self.seat.get_keyboard() {
                kb.set_focus(self, Some(surface), serial);
            }
            if let Some(t) = window.toplevel() { t.send_pending_configure(); }
        }
    }
}
