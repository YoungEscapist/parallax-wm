//! niri-подобная модель колонок для Layout::Columns.
//!
//! В отличие от простой раскладки "окно = полэкрана" (была раньше в tiling.rs),
//! здесь колонка — это ВЕРТИКАЛЬНАЯ стопка окон переменной ширины, а холст
//! скроллится по горизонтали к активной колонке (как в niri).
//!
//! Структура (`ColumnLayout`) живёт в `Dawn::columns` и синхронизируется с
//! `tagged_windows` через [`Dawn::columns_reconcile`]: мёртвые/скрытые/плавающие
//! окна выпадают, новые видимые не-плавающие окна текущего тега добавляются
//! отдельными колонками. Вертикальные стопки и ширины колонок переживают
//! добавление/закрытие окон, но НЕ переживают переключение тегов (при возврате
//! на воркспейс окна снова разложатся по одному на колонку) — компромисс ради
//! простоты (одна общая структура, не по структуре на тег).

use std::time::Duration;

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, SERIAL_COUNTER},
};

use crate::anim::CameraAnim;
use crate::state::Dawn;
use crate::tiling::{GAP_INNER, GAP_OUTER, Layout};

fn same_window(a: &Window, b: &Window) -> bool {
    a.toplevel().zip(b.toplevel())
        .map(|(x, y)| x.wl_surface() == y.wl_surface())
        .unwrap_or(false)
}

/// Пресеты ширины колонки (доля ширины экрана), циклятся по Super+R.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    Third,
    Half,
    TwoThirds,
    Full,
}

impl ColumnWidth {
    pub fn factor(self) -> f64 {
        match self {
            ColumnWidth::Third => 1.0 / 3.0,
            ColumnWidth::Half => 0.5,
            ColumnWidth::TwoThirds => 2.0 / 3.0,
            ColumnWidth::Full => 1.0,
        }
    }
    pub fn next(self) -> Self {
        match self {
            ColumnWidth::Third => ColumnWidth::Half,
            ColumnWidth::Half => ColumnWidth::TwoThirds,
            ColumnWidth::TwoThirds => ColumnWidth::Full,
            ColumnWidth::Full => ColumnWidth::Third,
        }
    }
}

/// Одна колонка: вертикальная стопка окон + активная строка + ширина +
/// опциональный непрерывный ресайз (Super+RMB drag, обновляется в motion).
pub struct Column {
    pub windows: Vec<Window>,
    pub active_row: usize,
    pub width: ColumnWidth,
    /// Переопределение ширины, установленное через Super+RMB drag
    /// (непрерывное значение, сбрасывается при columns_cycle_width).
    pub drag_width: Option<f64>,
}

impl Column {
    fn single(w: Window) -> Self {
        Self { windows: vec![w], active_row: 0, width: ColumnWidth::Half, drag_width: None }
    }
    fn clamp_row(&mut self) {
        if self.active_row >= self.windows.len() {
            self.active_row = self.windows.len().saturating_sub(1);
        }
    }
}

#[derive(Default)]
pub struct ColumnLayout {
    pub columns: Vec<Column>,
    pub active: usize,
}

impl ColumnLayout {
    fn clamp_active(&mut self) {
        if self.active >= self.columns.len() {
            self.active = self.columns.len().saturating_sub(1);
        }
    }
}

impl Dawn {
    fn primary_output_geo(&self) -> Option<Rectangle<i32, Logical>> {
        let o = self.space.outputs().next().cloned()?;
        self.space.output_geometry(&o)
    }

    /// Приводит `self.columns` в соответствие с реальным набором видимых
    /// не-плавающих окон текущего тега: выкидывает исчезнувшие, дописывает
    /// новые как отдельные колонки в конец. Сохраняет существующие стопки и
    /// ширины. Вызывается в начале каждой операции над колонками.
    pub fn columns_reconcile(&mut self) {
        let current = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current != 0 && !tw.floating)
            .map(|tw| tw.window.clone())
            .collect();
        let is_visible = |w: &Window| visible.iter().any(|v| same_window(v, w));

        for col in &mut self.columns.columns {
            col.windows.retain(|w| is_visible(w));
            col.clamp_row();
        }
        self.columns.columns.retain(|c| !c.windows.is_empty());

        // Окна, ещё не попавшие ни в одну колонку → новые колонки в конец.
        for w in &visible {
            let present = self.columns.columns.iter()
                .any(|c| c.windows.iter().any(|x| same_window(x, w)));
            if !present {
                self.columns.columns.push(Column::single(w.clone()));
            }
        }
        self.columns.clamp_active();
    }

    /// Раскладка колонок: слева направо, каждая колонка своей ширины (доля
    /// экрана), окна внутри делят высоту поровну. Размер применяется сразу,
    /// позиция едет плавным LERP (см. resize_window). Заменяет старую
    /// "окно = полэкрана" раскладку.
    pub fn apply_columns_layout(&mut self) {
        self.columns_reconcile();
        let geo = match self.primary_output_geo() {
            Some(g) => g,
            None => return,
        };
        if self.columns.columns.is_empty() {
            return;
        }

        let avail_h = (geo.size.h - GAP_OUTER * 2).max(1);

        // Собираем план, чтобы не держать &self.columns во время resize_window (&mut self).
        let mut plan: Vec<(Window, Rectangle<i32, Logical>)> = Vec::new();
        let mut x = 0i32;
        for col in &self.columns.columns {
            // drag_width переопределяет пресет, если установлен через Super+RMB.
            let factor = col.drag_width.unwrap_or_else(|| col.width.factor());
            let col_full_w = (factor * geo.size.w as f64).round() as i32;
            let win_w = (col_full_w - GAP_INNER).max(1);
            let n = col.windows.len().max(1) as i32;
            let cell_h = avail_h / n;
            for (ri, w) in col.windows.iter().enumerate() {
                let top = GAP_OUTER + ri as i32 * cell_h;
                // Последнее окно дотягивается до низа (компенсируем остаток от деления).
                let bottom = if ri as i32 == n - 1 {
                    GAP_OUTER + avail_h
                } else {
                    top + cell_h
                };
                let ry = top + GAP_INNER / 2;
                let rh = (bottom - GAP_INNER / 2 - ry).max(1);
                let rect = Rectangle::new(
                    (x + GAP_INNER / 2, ry).into(),
                    (win_w, rh).into(),
                );
                plan.push((w.clone(), rect));
            }
            x += col_full_w;
        }

        for (w, rect) in plan {
            self.resize_window(&w, rect);
        }
    }

    fn columns_active_window(&self) -> Option<Window> {
        let col = self.columns.columns.get(self.columns.active)?;
        col.windows.get(col.active_row).cloned()
    }

    /// Отдаёт клавиатурный фокус окну и поднимает его (общий хелпер операций
    /// над колонками — повторяет логику tiling.rs::focus_stack).
    fn columns_give_focus(&mut self, window: &Window) {
        let serial = SERIAL_COUNTER.next_serial();
        self.space.raise_element(window, true);
        window.set_activated(true);
        for w in self.space.elements() {
            let other = w.toplevel().zip(window.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true);
            if other {
                w.set_activated(false);
                if let Some(t) = w.toplevel() { t.send_pending_configure(); }
            }
        }
        if let Some(t) = window.toplevel() {
            if let Some(kb) = self.seat.get_keyboard() {
                kb.set_focus(self, Some(t.wl_surface().clone()), serial);
            }
            t.send_pending_configure();
        }
    }

    /// Синхронизирует активную колонку/строку с текущим клавиатурным фокусом
    /// (например после Super+N или sloppy-focus мышью).
    pub fn columns_set_active_to_focus(&mut self) {
        self.columns_reconcile();
        let focused = match self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            Some(f) => f,
            None => return,
        };
        for (ci, col) in self.columns.columns.iter().enumerate() {
            if let Some(ri) = col.windows.iter().position(|w| {
                w.toplevel().map(|t| t.wl_surface() == &focused).unwrap_or(false)
            }) {
                self.columns.active = ci;
                self.columns.columns[ci].active_row = ri;
                return;
            }
        }
    }

    /// Плавно подтягивает камеру так, чтобы активная колонка попала в кадр
    /// (без центрирования; если колонка шире экрана — выравнивает по левому
    /// краю). Колонки живут при zoom=1, cam_y=0.
    pub fn columns_scroll_to_active(&mut self) {
        let geo = match self.primary_output_geo() {
            Some(g) => g,
            None => return,
        };
        if self.columns.columns.is_empty() {
            return;
        }
        let view_w = geo.size.w as f64;
        let mut left = 0i32;
        for col in self.columns.columns.iter().take(self.columns.active) {
            left += (col.width.factor() * geo.size.w as f64).round() as i32;
        }
        let active_w = (self.columns.columns[self.columns.active].width.factor()
            * geo.size.w as f64).round() as i32;
        let left = left as f64;
        let right = left + active_w as f64;

        let mut cam_x = self.viewport.cam_x;
        if right > cam_x + view_w { cam_x = right - view_w; }
        if left < cam_x { cam_x = left; }
        if cam_x < 0.0 { cam_x = 0.0; }

        let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let to = Point::from((cam_x, 0.0));
        if (to.x - from.x).abs() > 0.5 || (to.y - from.y).abs() > 0.5 {
            self.camera_anim = Some(CameraAnim::new(from, to, Duration::from_millis(220)));
        }
    }

    /// Super+←/→ (dcol), Super+↑/↓ (drow): сдвиг фокуса по колонкам/строкам.
    /// Без заворачивания (как niri) — упор в край просто ничего не делает.
    pub fn columns_focus(&mut self, dcol: i32, drow: i32) {
        // Отталкиваемся от реально сфокусированного окна (мог смениться мышью).
        self.columns_set_active_to_focus();
        if self.columns.columns.is_empty() {
            return;
        }
        if dcol != 0 {
            let n = self.columns.columns.len() as i32;
            self.columns.active = (self.columns.active as i32 + dcol).clamp(0, n - 1) as usize;
        }
        if drow != 0 {
            let col = &mut self.columns.columns[self.columns.active];
            let n = col.windows.len() as i32;
            col.active_row = (col.active_row as i32 + drow).clamp(0, n - 1) as usize;
        }
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active();
    }

    /// Super+J/K/Tab в Columns: линейный обход всех окон в порядке
    /// колонка-за-колонкой (сверху вниз внутри колонки), с заворачиванием.
    pub fn columns_focus_flattened(&mut self, dir: i32) {
        self.columns_set_active_to_focus();
        let flat: Vec<(usize, usize)> = self.columns.columns.iter().enumerate()
            .flat_map(|(ci, col)| (0..col.windows.len()).map(move |ri| (ci, ri)))
            .collect();
        if flat.is_empty() {
            return;
        }
        let cur = flat.iter().position(|&(ci, ri)| {
            ci == self.columns.active && ri == self.columns.columns[ci].active_row
        }).unwrap_or(0);
        let next = if dir >= 0 {
            (cur + 1) % flat.len()
        } else {
            (cur + flat.len() - 1) % flat.len()
        };
        let (ci, ri) = flat[next];
        self.columns.active = ci;
        self.columns.columns[ci].active_row = ri;
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active();
    }

    /// Super+Ctrl+←/→: переставить активную колонку влево/вправо.
    pub fn columns_move_column(&mut self, dir: i32) {
        self.columns_set_active_to_focus();
        let n = self.columns.columns.len() as i32;
        let a = self.columns.active as i32;
        let j = a + dir.signum();
        if j < 0 || j >= n {
            return;
        }
        self.columns.columns.swap(a as usize, j as usize);
        self.columns.active = j as usize;
        self.arrange();
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// Super+Ctrl+↑/↓: переставить активное окно вверх/вниз внутри его колонки.
    pub fn columns_move_window(&mut self, dir: i32) {
        self.columns_set_active_to_focus();
        let col = match self.columns.columns.get_mut(self.columns.active) {
            Some(c) => c,
            None => return,
        };
        let n = col.windows.len() as i32;
        let r = col.active_row as i32;
        let j = r + dir.signum();
        if j < 0 || j >= n {
            return;
        }
        col.windows.swap(r as usize, j as usize);
        col.active_row = j as usize;
        self.arrange();
        self.request_redraw();
    }

    /// Super+Comma (consume): забрать верхнее окно СЛЕДУЮЩЕЙ колонки в низ
    /// активной (строит вертикальную стопку). Опустевшая колонка удаляется.
    pub fn columns_consume(&mut self) {
        self.columns_set_active_to_focus();
        let a = self.columns.active;
        if a + 1 >= self.columns.columns.len() {
            return;
        }
        let w = self.columns.columns[a + 1].windows.remove(0);
        if self.columns.columns[a + 1].windows.is_empty() {
            self.columns.columns.remove(a + 1);
        } else {
            self.columns.columns[a + 1].clamp_row();
        }
        self.columns.columns[a].windows.push(w);
        self.columns.columns[a].active_row = self.columns.columns[a].windows.len() - 1;
        self.arrange();
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// Super+Period (expel): вытолкнуть активное окно из стопки в НОВУЮ колонку
    /// справа (разбивает стопку). Одиночное окно не выталкивается.
    pub fn columns_expel(&mut self) {
        self.columns_set_active_to_focus();
        let a = self.columns.active;
        let col = match self.columns.columns.get_mut(a) {
            Some(c) => c,
            None => return,
        };
        if col.windows.len() <= 1 {
            return;
        }
        let r = col.active_row;
        let w = col.windows.remove(r);
        col.clamp_row();
        let width = col.width;
        let mut newcol = Column::single(w);
        newcol.width = width;
        self.columns.columns.insert(a + 1, newcol);
        self.columns.active = a + 1;
        self.arrange();
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// Super+R: сменить пресет ширины активной колонки.
    /// Сбрасывает drag_width (непрерывный ресайз от Super+RMB).
    pub fn columns_cycle_width(&mut self) {
        self.columns_set_active_to_focus();
        if let Some(col) = self.columns.columns.get_mut(self.columns.active) {
            col.drag_width = None;
            col.width = col.width.next();
        }
        self.arrange();
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// Super+RMB drag (в Columns-режиме): непрерывный ресайз ширины активной
    /// колонки. delta_px — смещение мыши по X от начала драга (может быть
    /// отрицательным). Ширина зажимается в [15%, 100%] экрана.
    pub fn columns_resize_active_width(&mut self, delta_px: f64) {
        self.columns_set_active_to_focus();
        let geo = match self.primary_output_geo() {
            Some(g) => g,
            None => return,
        };
        if let Some(col) = self.columns.columns.get_mut(self.columns.active) {
            let current = col.drag_width.unwrap_or_else(|| col.width.factor());
            let delta = delta_px / geo.size.w as f64;
            col.drag_width = Some((current + delta).clamp(0.15, 1.0));
        }
        self.arrange();
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// niri-воркспейсы: перенести АКТИВНУЮ КОЛОНКУ (все её окна) на соседний
    /// воркспейс (dir=-1/+1). Только в Columns; окна меняют тег и уходят из
    /// текущего вида, фокус/скролл переезжают на оставшуюся активную колонку.
    pub fn columns_move_to_workspace(&mut self, dir: i32) {
        if self.tile_config.layout != Layout::Columns {
            return;
        }
        self.columns_set_active_to_focus();
        let idx = self.viewport.current_tags().trailing_zeros() as i32 + 1;
        let new = (idx + dir).clamp(1, 9);
        if new == idx {
            return;
        }
        let mask = 1u32 << (new - 1);
        let wins: Vec<Window> = match self.columns.columns.get(self.columns.active) {
            Some(c) => c.windows.clone(),
            None => return,
        };
        if wins.is_empty() {
            return;
        }
        for w in &wins {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                tw.window.toplevel().zip(w.toplevel())
                    .map(|(a, b)| a.wl_surface() == b.wl_surface())
                    .unwrap_or(false)
            }) {
                tw.tags = mask;
            }
        }
        self.columns.columns.remove(self.columns.active);
        if self.columns.active >= self.columns.columns.len() {
            self.columns.active = self.columns.columns.len().saturating_sub(1);
        }
        self.refresh_tags();
        self.arrange();
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active();
        self.request_redraw();
        tracing::info!("dawn: moved column ({} win) → workspace {}", wins.len(), new);
    }

    /// Вызывается из new_toplevel в Columns-режиме: только что добавленное окно
    /// (уже в tagged_windows, не плавающее) делаем НОВОЙ колонкой сразу справа
    /// от активной и переносим на неё фокус/скролл (как открытие окна в niri).
    pub fn columns_insert_new(&mut self, window: &Window) {
        // reconcile допишет новое окно как колонку в конец — заберём её оттуда.
        self.columns_reconcile();
        let last = match self.columns.columns.len().checked_sub(1) {
            Some(l) => l,
            None => return,
        };
        // Убеждаемся, что последняя колонка — это наше новое окно.
        let is_new_last = self.columns.columns[last].windows.len() == 1
            && same_window(&self.columns.columns[last].windows[0], window);
        if !is_new_last || last == 0 {
            // Единственная/первая колонка — просто делаем её активной.
            self.columns.active = last;
            return;
        }
        let target = (self.columns.active + 1).min(last);
        if target != last {
            let col = self.columns.columns.remove(last);
            self.columns.columns.insert(target, col);
        }
        self.columns.active = target;
    }
}
