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
    utils::{Logical, Point, Rectangle},
};

use crate::anim::CameraAnim;
use crate::state::Dawn;
use crate::tiling::{GAP_INNER, GAP_OUTER, Layout};

fn same_window(a: &Window, b: &Window) -> bool {
    a == b
}

/// Зазор между колонками и вокруг них. У niri это `layout { gaps }`, по
/// умолчанию 16 — совпадает с dawn'овским GAP_INNER, так что берём его.
pub const COL_GAP: f64 = GAP_INNER as f64;

/// Ширина колонки — ровно модель niri: либо доля рабочей области, либо
/// фиксированное число пикселей (после ручного ресайза).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ColumnWidth {
    Proportion(f64),
    Fixed(f64),
}

/// Пресеты ширины (niri: `preset-column-widths`, по умолчанию ⅓, ½, ⅔).
pub const PRESET_WIDTHS: [f64; 3] = [1.0 / 3.0, 0.5, 2.0 / 3.0];
/// Пресеты высоты окна в колонке (niri: `preset-window-heights`).
pub const PRESET_HEIGHTS: [f64; 3] = [1.0 / 3.0, 0.5, 2.0 / 3.0];

impl ColumnWidth {
    /// Пиксели по формуле niri (`resolve_column_width`): доля берётся от
    /// рабочей ширины БЕЗ одного зазора, и ещё один зазор вычитается — так
    /// колонка на всю ширину (доля 1.0) оставляет поля слева и справа, а две
    /// половинки ровно делят экран вместе с зазором между ними.
    pub fn resolve(self, working_w: f64) -> f64 {
        match self {
            ColumnWidth::Proportion(p) => ((working_w - COL_GAP) * p - COL_GAP).max(50.0),
            ColumnWidth::Fixed(px) => px.clamp(50.0, working_w.max(50.0)),
        }
    }

    /// Текущая доля (для ±% и для перехода Fixed → Proportion).
    pub fn proportion(self, working_w: f64) -> f64 {
        match self {
            ColumnWidth::Proportion(p) => p,
            ColumnWidth::Fixed(px) => {
                let full = working_w - COL_GAP;
                if full <= 0.0 { 1.0 } else { (px + COL_GAP) / full }
            }
        }
    }
}

/// Как вести вид относительно активной колонки (niri: `center-focused-column`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum CenterFocusedColumn {
    /// Подтягивать вид минимально — только чтобы колонка попала в кадр.
    #[default]
    Never,
    /// Всегда ставить активную колонку по центру.
    Always,
    /// Центрировать, только если переход не влезает в экран целиком.
    OnOverflow,
}

/// Одна колонка: вертикальная стопка окон + активная строка + ширина +
/// опциональный непрерывный ресайз (Super+RMB drag, обновляется в motion).
pub struct Column {
    pub windows: Vec<Window>,
    pub active_row: usize,
    pub width: ColumnWidth,
    /// Номер пресета, если ширина сейчас ровно на нём. None — ширина задана
    /// вручную (drag/±%), и следующий Super+R начнёт цикл с начала: так же
    /// ведёт себя `switch-preset-column-width` в niri.
    pub preset_idx: Option<usize>,
    /// Переопределение ширины, установленное через Super+RMB drag
    /// (непрерывное значение, сбрасывается при columns_cycle_width).
    pub drag_width: Option<f64>,
    /// Вкладочный показ (niri: `toggle-column-tabbed-display`): вместо стопки
    /// видно только активное окно во всю высоту колонки, слева — полоска
    /// вкладок. Остальные окна колонки снимаются со space, то есть не
    /// рисуются, не получают frame callback и не ловят клики — как в niri.
    pub tabbed: bool,
    /// Веса высот строк (параллельно `windows`), niri-стиль: высота строки
    /// пропорциональна весу. Пусто/неверная длина → равные высоты (self-heal
    /// в row_fraction/apply_columns_layout). Меняется вертикальным Super+RMB drag.
    pub row_weights: Vec<f64>,
}

impl Column {
    fn single(w: Window) -> Self {
        Self {
            windows: vec![w],
            active_row: 0,
            // niri: `default-column-width` = ½ экрана.
            width: ColumnWidth::Proportion(0.5),
            preset_idx: Some(1),
            drag_width: None,
            tabbed: false,
            row_weights: Vec::new(),
        }
    }

    /// Ширина колонки в пикселях (без зазора).
    fn width_px(&self, working_w: f64) -> f64 {
        match self.drag_width {
            Some(f) => ColumnWidth::Proportion(f).resolve(working_w),
            None => self.width.resolve(working_w),
        }
    }
    fn clamp_row(&mut self) {
        if self.active_row >= self.windows.len() {
            self.active_row = self.windows.len().saturating_sub(1);
        }
    }

    /// Доля высоты (0..1) строки `row`. Если веса не заданы/рассинхронены —
    /// равные доли (1/n).
    pub fn row_fraction(&self, row: usize) -> f64 {
        let n = self.windows.len();
        if n == 0 || row >= n {
            return 0.0;
        }
        if self.row_weights.len() != n {
            return 1.0 / n as f64;
        }
        let s: f64 = self.row_weights.iter().sum();
        if s <= 0.0 { 1.0 / n as f64 } else { self.row_weights[row] / s }
    }

    /// Задаёт строке `row` долю высоты `f`, остальные строки масштабируются,
    /// чтобы заполнить `1-f`, сохраняя свои пропорции. Для колонок из 1 окна —
    /// no-op (окно и так на всю высоту).
    pub fn set_row_fraction(&mut self, row: usize, f: f64) {
        let n = self.windows.len();
        if n <= 1 || row >= n {
            return;
        }
        if self.row_weights.len() != n {
            self.row_weights = vec![1.0 / n as f64; n];
        }
        let f = f.clamp(0.1, 0.9);
        let others: f64 = (0..n).filter(|&i| i != row).map(|i| self.row_weights[i]).sum();
        if others <= 0.0 {
            let rest = (1.0 - f) / (n - 1) as f64;
            for i in 0..n {
                self.row_weights[i] = if i == row { f } else { rest };
            }
        } else {
            let scale = (1.0 - f) / others;
            for i in 0..n {
                self.row_weights[i] = if i == row { f } else { self.row_weights[i] * scale };
            }
        }
    }
}

#[derive(Default)]
pub struct ColumnLayout {
    pub columns: Vec<Column>,
    pub active: usize,
    /// Поведение вида при смене активной колонки (niri: center-focused-column).
    pub center_focused: CenterFocusedColumn,
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

    /// Экран для арифметики ленты. В обзоре у output'а масштаб = зум, поэтому
    /// его ЛОГИЧЕСКАЯ геометрия «шире» настоящей — а этажи ленты разложены по
    /// НАСТОЯЩЕМУ размеру экрана. Берём режим вывода, чтобы ресайз колонки в
    /// обзоре считал ту же ширину, что и вне его.
    fn columns_screen_geo(&self) -> Option<Rectangle<i32, Logical>> {
        if self.overview_active {
            let mode = self.space.outputs().next()?.current_mode()?;
            return Some(Rectangle::new((0, 0).into(), (mode.size.w, mode.size.h).into()));
        }
        self.primary_output_geo()
    }

    /// Этаж (тег), на полосе которого лежит `window`.
    fn columns_tag_of_window(&self, window: &Window) -> Option<u32> {
        self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, window))
            .map(|tw| tw.tags)
    }

    /// Полоса этажа `tag`: текущий стол живёт в `self.columns`, чужие — в
    /// `columns_by_tag`. Нужно обзору ленты — там видны все этажи сразу, и
    /// Super+ПКМ может схватить окно чужого стола.
    fn columns_layout_of_tag_mut(&mut self, tag: u32) -> Option<&mut ColumnLayout> {
        if tag == self.viewport.current_tags() {
            self.columns_reconcile();
            Some(&mut self.columns)
        } else {
            self.columns_by_tag.get_mut(&tag)
        }
    }

    /// Разложить полосу этажа `tag` на месте. В обзоре идём мимо arrange: тот
    /// в обзоре ничего не делает (раскладку держит overview.rs), а лента там
    /// лежит на холсте по-настоящему и обязана пересобраться сразу.
    fn columns_relayout_floor(&mut self, tag: u32) {
        let cur = self.viewport.current_tags();
        if tag == cur && !self.overview_active {
            self.arrange();
            return;
        }
        let Some(geo) = self.columns_screen_geo() else { return };
        let floor_y = self.columns_ws_y(tag).round() as i32;
        let plan = if tag == cur {
            plan_columns(&self.columns, geo, floor_y)
        } else {
            // Полосу вынимаем из полки на время планирования (заимствование).
            let Some(layout) = self.columns_by_tag.remove(&tag) else { return };
            let plan = plan_columns(&layout, geo, floor_y);
            self.columns_by_tag.insert(tag, layout);
            plan
        };
        for (w, rect) in plan {
            self.resize_window(&w, rect);
        }
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
        // Этаж текущего стола в вертикальной ленте: вся раскладка ниже
        // считается от него, а не от нуля холста.
        let floor_y = self.columns_cur_y().round() as i32;
        let plan = plan_columns(&self.columns, geo, floor_y);
        for (w, rect) in plan {
            self.resize_window(&w, rect);
        }
        self.columns_apply_tabbed_visibility();
    }

    /// Разложить полосу ЧУЖОГО стола прямо на его этаже, не переключаясь на
    /// него. Нужно niri-обзору: туда можно бросить окно на соседний этаж, и та
    /// полоса обязана пересобраться сразу — иначе брошенное окно так и висело
    /// бы в точке отпускания до следующего захода на этот стол.
    pub fn columns_layout_tag(&mut self, tag: u32) {
        if tag == self.viewport.current_tags() {
            self.arrange();
            return;
        }
        let Some(geo) = self.primary_output_geo() else { return };
        // Полосу вынимаем из полки на время планирования (заимствование), затем
        // кладём обратно ровно там же.
        let Some(layout) = self.columns_by_tag.remove(&tag) else { return };
        let floor_y = self.columns_ws_y(tag).round() as i32;
        let plan = plan_columns(&layout, geo, floor_y);
        self.columns_by_tag.insert(tag, layout);
        for (w, rect) in plan {
            self.resize_window(&w, rect);
        }
    }
}

/// План раскладки полосы `layout` на этаже `floor_y`: слева направо, каждая
/// колонка своей ширины, окна внутри делят высоту по весам строк. Вынесено из
/// [`Dawn::apply_columns_layout`] отдельной функцией, чтобы ту же геометрию
/// можно было посчитать и для полосы НЕ текущего стола (см. columns_layout_tag).
fn plan_columns(
    layout: &ColumnLayout,
    geo: Rectangle<i32, Logical>,
    floor_y: i32,
) -> Vec<(Window, Rectangle<i32, Logical>)> {
    let mut plan: Vec<(Window, Rectangle<i32, Logical>)> = Vec::new();
    {
        let avail_h = (geo.size.h - GAP_OUTER * 2).max(1);
        let working_w = geo.size.w as f64;
        // Первая колонка стоит на COL_GAP от начала полосы — в niri это поле
        // даёт view_offset, но dawn возит камеру по холсту, поэтому проще
        // заложить его прямо в координаты.
        let mut x = COL_GAP as i32;
        for col in &layout.columns {
            let win_w = col.width_px(working_w).round().max(1.0) as i32;
            let col_full_w = win_w + COL_GAP as i32;
            let n = col.windows.len();
            // Высоты по весам строк (niri-стиль); при рассинхроне — равные.
            let weights: Vec<f64> = if col.row_weights.len() == n && n > 0 {
                col.row_weights.clone()
            } else {
                vec![1.0; n.max(1)]
            };
            let wsum: f64 = weights.iter().sum::<f64>().max(1e-6);
            // Вкладочная колонка: каждое окно — во всю высоту, видно только
            // активное (остальные снимаются со space, см.
            // columns_apply_tabbed_visibility). Размер задаём всем, чтобы при
            // переключении вкладки окно уже было нужного размера.
            if col.tabbed {
                let rect = Rectangle::new(
                    (x, floor_y + GAP_OUTER).into(),
                    (win_w, avail_h).into(),
                );
                for w in &col.windows {
                    plan.push((w.clone(), rect));
                }
                x += col_full_w;
                continue;
            }
            let mut acc_top = floor_y + GAP_OUTER;
            for (ri, w) in col.windows.iter().enumerate() {
                let top = acc_top;
                // Последнее окно дотягивается до низа (компенсируем остаток округления).
                let bottom = if ri + 1 == n {
                    floor_y + GAP_OUTER + avail_h
                } else {
                    top + (avail_h as f64 * weights[ri] / wsum).round() as i32
                };
                acc_top = bottom;
                let ry = top + GAP_INNER / 2;
                let rh = (bottom - GAP_INNER / 2 - ry).max(1);
                // x — левый край окна: зазор между колонками уже заложен в
                // col_full_w, так что половинку GAP_INNER здесь прибавлять не
                // нужно (иначе колонки разъезжались бы с расчётом прокрутки).
                let rect = Rectangle::new((x, ry).into(), (win_w, rh).into());
                plan.push((w.clone(), rect));
            }
            x += col_full_w;
        }
    }
    plan
}

/// Вынуть окно из полосы, схлопнув опустевшие колонки.
fn remove_window_from(layout: &mut ColumnLayout, window: &Window) {
    for col in &mut layout.columns {
        col.windows.retain(|w| !same_window(w, window));
        col.row_weights.clear();
        col.clamp_row();
    }
    layout.columns.retain(|c| !c.windows.is_empty());
    layout.clamp_active();
}

/// Левый край колонки `idx` в полосе `layout` (сумма ширин предыдущих с зазорами).
fn column_x_in(layout: &ColumnLayout, idx: usize, working_w: f64) -> f64 {
    let mut x = COL_GAP;
    for col in layout.columns.iter().take(idx) {
        x += col.width_px(working_w) + COL_GAP;
    }
    x
}

/// Куда встанет окно, брошенное на X = `pos_x` в полосе `layout`: номер колонки
/// и «в стопку ли». Вынесено из [`Dawn::columns_insert_target`], чтобы то же
/// решение можно было принять и для полосы не текущего стола.
fn insert_target_in(layout: &ColumnLayout, working_w: f64, pos_x: f64) -> (usize, bool) {
    for i in 0..layout.columns.len() {
        let x = column_x_in(layout, i, working_w);
        let w = layout.columns[i].width_px(working_w);
        if pos_x < x - COL_GAP / 2.0 {
            return (i, false);
        }
        if pos_x < x + w {
            // Внутри колонки: треть слева/справа — вставка рядом, середина —
            // в стопку.
            let rel = (pos_x - x) / w.max(1.0);
            if rel < 0.25 {
                return (i, false);
            } else if rel > 0.75 {
                return (i + 1, false);
            }
            return (i, true);
        }
    }
    (layout.columns.len(), false)
}

impl Dawn {
    /// Во вкладочных колонках на экране остаётся только активное окно.
    ///
    /// Скрываем снятием со `space` — тогда окно не рисуется, не получает frame
    /// callback (клиент перестаёт тратить кадры) и не ловит клики. Обратное
    /// отображение делает эта же функция: она идемпотентна и зовётся из
    /// apply_columns_layout, то есть после каждого arrange и каждой смены тега.
    fn columns_apply_tabbed_visibility(&mut self) {
        // Собираем заранее: ниже нужен &mut self.space.
        let mut hide: Vec<Window> = Vec::new();
        let mut show: Vec<(Window, Point<i32, Logical>)> = Vec::new();
        for col in &self.columns.columns {
            for (ri, w) in col.windows.iter().enumerate() {
                let visible = !col.tabbed || ri == col.active_row;
                if visible {
                    if self.space.element_location(w).is_none() {
                        let pos = self.tagged_windows.iter()
                            .find(|tw| &tw.window == w)
                            .map(|tw| tw.position)
                            .unwrap_or_default();
                        show.push((w.clone(), pos));
                    }
                } else if self.space.element_location(w).is_some() {
                    hide.push(w.clone());
                }
            }
        }
        if hide.is_empty() && show.is_empty() {
            return;
        }
        for w in hide {
            self.space.unmap_elem(&w);
        }
        for (w, pos) in show {
            self.space.map_element(w, pos, false);
        }
        // Показ/скрытие меняет весь набор окон в кадре — иначе на экране
        // остаётся «тень» скрытой вкладки в закэшированном DRM-плане.
        self.request_plane_reset();
    }

    // ── Раскладка колонок у КАЖДОГО стола своя ───────────────────────────────
    //
    // `Dawn::columns` — это полоса ТЕКУЩЕГО стола; полосы остальных лежат в
    // `columns_by_tag`. В niri каждый воркспейс держит свои колонки, и уход на
    // соседний с возвратом ничего не меняет. Раньше структура была одна на весь
    // композитор: при возврате reconcile разбивал окна заново по одной колонке,
    // теряя стопки, ширины и вкладки.

    /// Плавающие окна в ленте держатся ЭКРАНА, а не холста.
    ///
    /// В niri floating-слой лежит поверх полосы и не уезжает вместе с
    /// колонками: прокрутил ленту — плавающее окно осталось на своём месте
    /// экрана. В dawn же всё живёт в canvas-координатах, поэтому при движении
    /// камеры плавающие окна текущего стола сдвигаются на ту же дельту.
    ///
    /// Зовётся из apply_camera и работает только в Columns — в остальных
    /// раскладках плавающее окно, наоборот, ОБЯЗАНО стоять на холсте (там
    /// камера и есть способ до него доехать).
    pub fn columns_pin_floating(&mut self) {
        // В обзоре камера отъезжает на всю ленту — плавающие окна при этом
        // обязаны остаться на своих местах холста, иначе они уплывали бы за
        // кадр вместе с зумом и возвращались бы уже не туда.
        if self.overview_active {
            self.columns_float_cam = (self.viewport.cam_x, self.viewport.cam_y);
            return;
        }
        if self.tile_config.layout != Layout::Columns {
            // Запоминаем текущую камеру, чтобы при возврате в Columns не
            // «догонять» накопленную за это время разницу одним рывком.
            self.columns_float_cam = (self.viewport.cam_x, self.viewport.cam_y);
            return;
        }
        let dx = self.viewport.cam_x - self.columns_float_cam.0;
        // По Y плавающие окна НЕ таскаем: вертикаль в ленте — это переход
        // между этажами-столами, а плавающее окно принадлежит своему столу и
        // обязано остаться на нём. Держим экрана только по горизонтали, вдоль
        // прокрутки колонок.
        let dy: f64 = 0.0;
        self.columns_float_cam = (self.viewport.cam_x, self.viewport.cam_y);
        if dx.abs() < 0.01 {
            return;
        }
        let current = self.viewport.current_tags();
        let moves: Vec<(Window, Point<i32, Logical>)> = self.tagged_windows.iter()
            .filter(|tw| tw.floating && tw.tags & current != 0)
            .filter_map(|tw| {
                let loc = self.space.element_location(&tw.window)?;
                Some((
                    tw.window.clone(),
                    Point::from((
                        loc.x + dx.round() as i32,
                        loc.y + dy.round() as i32,
                    )),
                ))
            })
            .collect();
        for (w, loc) in moves {
            self.space.map_element(w.clone(), loc, false);
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == w) {
                tw.position = loc;
                tw.float_position = loc;
            }
        }
    }
    // ── Вертикальная лента столов ────────────────────────────────────────────
    //
    // Столы в niri — не подменяемые «страницы», а этажи ОДНОЙ вертикальной
    // ленты: стол N живёт на холсте со сдвигом N × высота экрана, а переход
    // между столами это движение камеры по Y. Поэтому окна всех ленточных
    // столов остаются на space (см. refresh_tags) и полосы соседей
    // действительно существуют: до них можно доехать и видно, как они уезжают.

    /// Номер бита тега (0-based). НЕ номер этажа: между своими столами могут
    /// лежать чужие, для этажа есть columns_floor_index.
    pub fn columns_ws_index(tag: u32) -> i32 {
        tag.trailing_zeros() as i32
    }

    /// Чужой ли стол для ленты: он уже ПОСЕЩЁН и помнит НЕ ленточную раскладку
    /// (Tile/Dwindle/Float/Monocle). Непосещённый стол чужим не считается —
    /// зайдя на него из ленты, он станет ленточным (см. view_tag), именно так
    /// в niri заводят новый стол.
    ///
    /// Это граница изоляции: лента не видит чужие столы, не листается на них,
    /// не нумерует их этажами и не трогает их окна при схлопывании.
    pub fn columns_tag_foreign(&self, tag: u32) -> bool {
        // Все столы живут по одному правилу: visited + layout решают,
        // чужой ли это стол для ленты. Никаких закреплённых номеров.
        self.visited_tags.contains(&tag) && !self.columns_is_strip_tag(tag)
    }

    /// Этажи ленты по порядку: все теги 1..9, кроме чужих.
    pub fn columns_strip_order(&self) -> Vec<u32> {
        (0..9u32)
            .map(|i| 1u32 << i)
            .filter(|&m| !self.columns_tag_foreign(m))
            .collect()
    }

    /// Номер ЭТАЖА стола в ленте (0-based) — позиция среди своих тегов, а не
    /// номер бита. Иначе чужой стол в середине оставлял бы в ленте дыру в целый
    /// экран: камера перелетала бы через пустоту к следующему своему столу.
    /// Для чужого тега возвращает индекс, который он бы занял (в ленте он не
    /// рисуется, значение не используется).
    pub fn columns_floor_index(&self, tag: u32) -> i32 {
        let mut idx = 0;
        for i in 0..9u32 {
            let m = 1u32 << i;
            if m == tag {
                return idx;
            }
            if !self.columns_tag_foreign(m) {
                idx += 1;
            }
        }
        idx
    }

    /// Сосед по ленте в направлении `dir` — следующий СВОЙ стол, чужие
    /// пропускаются целиком. None, если лента в эту сторону кончилась (вверх —
    /// первый этаж, вниз — пустой этаж за последним занятым, см. niri_ws_count).
    pub fn columns_strip_neighbor(&self, tag: u32, dir: i32) -> Option<u32> {
        let order = self.columns_strip_order();
        let pos = order.iter().position(|&m| m == tag)? as i32;
        let limit = (self.niri_ws_count() as usize).min(order.len()) as i32;
        let new = pos + dir.signum();
        if new < 0 || new >= limit {
            return None;
        }
        order.get(new as usize).copied()
    }

    // isolation_order/tag_for_digit (относительная нумерация столов «внутри
    // своей изоляции») удалены: Super+цифра адресует стол ГЛОБАЛЬНЫМ битом
    // тега, а режим записан в tag_layouts для каждого стола отдельно.
    // Изоляция при этом жива и работает как прежде для навигации ЛЕНТОЙ
    // (columns_strip_order, columns_strip_neighbor, workspace_step).

    /// Высота ЭТАЖА ленты в canvas-единицах — всегда «экран при zoom 1».
    ///
    /// ВАЖНО: не primary_output_geo. Та меряет output в ЛОГИЧЕСКИХ единицах, а
    /// apply_camera выставляет output'у scale = текущий зум камеры, так что при
    /// отъезде (обзор ленты — zoom ≈ 0.37, лупа) логическая высота вырастает во
    /// столько же раз. Шаг этажей от неё «разъезжался» ровно тогда, когда он
    /// нужнее всего: в обзоре хит-тест стола под курсором (overview_workspace_at
    /// меряет этажи ростом в mode.size.h) промахивался мимо всех этажей, маска
    /// выходила None — и повторный тап Super просто закрывал обзор на прежнем
    /// столе вместо стола под курсором.
    pub fn columns_floor_h(&self) -> f64 {
        self.space.outputs().next()
            .and_then(|o| o.current_mode())
            .map(|m| m.size.h as f64)
            .unwrap_or(1080.0)
    }

    /// Сдвиг этажа стола по Y на холсте. Считаем по НОМЕРУ ЭТАЖА (позиции среди
    /// своих столов), а не по номеру бита: чужие столы в ленте не существуют и
    /// места в ней не занимают.
    pub fn columns_ws_y(&self, tag: u32) -> f64 {
        self.columns_floor_index(tag) as f64 * self.columns_floor_h()
    }

    /// Y текущего стола — база для горизонтальной прокрутки.
    pub fn columns_cur_y(&self) -> f64 {
        self.columns_ws_y(self.viewport.current_tags())
    }

    /// Сколько этажей у ленты сейчас и какой они высоты — общая мера для всего,
    /// что упирается в её вертикальные границы.
    fn columns_floor_span(&self) -> (i32, f64) {
        (self.niri_ws_count().max(1), self.columns_floor_h())
    }

    /// Тег стола, чей этаж накрывает точку `y` на холсте.
    ///
    /// Этажи идут подряд от нуля вниз (см. columns_ws_y), поэтому номер этажа —
    /// это просто `y / высота_этажа`. Точку выше первого этажа и ниже
    /// последнего прижимаем к крайнему: за пределами ленты столов нет, и
    /// «никуда» окно попасть не может.
    pub fn columns_tag_at_y(&self, y: f64) -> Option<u32> {
        if self.tile_config.layout != Layout::Columns {
            return None;
        }
        let (floors, h) = self.columns_floor_span();
        if h <= 0.0 {
            return None;
        }
        let idx = (y / h).floor().clamp(0.0, (floors - 1) as f64) as usize;
        self.columns_strip_order().get(idx).copied()
    }

    /// Зажать окно в вертикальных границах ленты.
    ///
    /// Лента — это стопка этажей-столов, а не бесконечный холст: выше первого
    /// этажа и ниже последнего просто нет места, куда окно могло бы попасть.
    /// Раньше перетаскивание в Columns ничем не ограничивалось, и окно
    /// улетало в пустоту за экраном — видно это было только в обзоре по Super,
    /// где оно висело между столами и ни одному из них не принадлежало.
    ///
    /// По горизонтали не зажимаем: полоса колонок прокручивается, её ширина не
    /// равна экрану, а курсор и так заперт экраном (см. set_pointer_canvas) —
    /// уехать вбок дальше, чем на экран, окно всё равно не может.
    pub fn columns_clamp_to_strip(&self, y: i32, win_h: i32) -> i32 {
        let (floors, h) = self.columns_floor_span();
        let bottom = (floors as f64 * h).round() as i32;
        y.clamp(0, (bottom - win_h).max(0))
    }

    /// Ленточный ли стол (его раскладка — Columns) — граница между двумя
    /// изоляциями: лента и всё остальное (tiling/floating).
    ///
    /// Источник правды ОДИН — `tag_layouts`: раскладка это свойство СТОЛА, и
    /// set_layout записывает её туда сразу же, как только стол сменил режим.
    /// Раньше текущий стол считался по живому `tile_config.layout`, и в
    /// середине view_tag (тег уже новый, layout ещё старый) изоляция врала:
    /// refresh_tags принимал тайловый стол за ленточный и оставлял на экране
    /// окна чужих этажей.
    ///
    /// Живой layout остаётся запасным ответом только для текущего стола, о
    /// котором tag_layouts ещё ничего не знает (самый первый кадр).
    pub fn columns_is_strip_tag(&self, tag: u32) -> bool {
        // Все столы по единому правилу: стол — лента, если tag_layouts
        // говорит Columns. Для текущего стола источник — живая раскладка.
        match self.tag_layouts.get(&tag) {
            Some(l) => *l == Layout::Columns,
            None if tag == self.viewport.current_tags() => {
                self.tile_config.layout == Layout::Columns
            }
            None => false,
        }
    }

    /// Перелёт камеры на этаж стола `tag`. Горизонталь берём у уже поставленной
    /// прокрутки к активной колонке, иначе перелёт затёр бы её.
    pub fn columns_fly_to_workspace(&mut self, tag: u32) {
        if self.tile_config.layout != Layout::Columns {
            return;
        }
        self.columns_ws_slide = 0;
        let y = self.columns_ws_y(tag);
        let target_x = match &self.camera_anim {
            Some(a) => a.to.x,
            None => self.viewport.cam_x,
        };
        let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let to = Point::from((target_x, y));
        if (to.x - from.x).abs() > 0.5 || (to.y - from.y).abs() > 0.5 {
            self.camera_anim = Some(CameraAnim::new(from, to, Duration::from_millis(220)));
        } else {
            self.viewport.cam_y = y;
            self.apply_camera();
        }
        self.request_redraw();
    }

    /// Убрать текущую полосу на полку тега `tag`. Текущая полоса после этого
    /// ПУСТА — значит стол, уходящий в другую изоляцию (tiling/floating), не
    /// утащит свои колонки за собой и не отдаст их следующему ленточному столу.
    /// Настройка вида (center-focused-column) общая на композитор и остаётся.
    pub fn columns_save_for(&mut self, tag: u32) {
        let layout = std::mem::take(&mut self.columns);
        self.columns.center_focused = layout.center_focused;
        if layout.columns.is_empty() {
            self.columns_by_tag.remove(&tag);
        } else {
            self.columns_by_tag.insert(tag, layout);
        }
    }

    /// Пересобрать ГЕОМЕТРИЮ всех этажей ленты на их нынешних местах.
    ///
    /// Y этажа считается по его НОМЕРУ В ЛЕНТЕ (columns_floor_index), а не по
    /// биту тега, поэтому стоит одному столу войти в ленту или выйти из неё
    /// (Win+N, схлопывание при закрытии окна) — все этажи ниже него уезжают на
    /// экран вверх или вниз. Обычный arrange раскладывает только ТЕКУЩИЙ стол,
    /// так что соседние этажи оставались на старых Y: переключение стола
    /// приводило на пустое место, а окна прежнего жильца висели этажом выше.
    /// Пересобираем только те этажи, которые СЪЕХАЛИ со своего Y: окно ленты
    /// всегда лежит внутри полосы своего этажа, так что достаточно проверить,
    /// попадают ли окна в неё. Иначе каждый переход между столами гонял бы
    /// анимацию позиции у всех окон ленты ради той же самой геометрии.
    pub fn columns_relayout_strip(&mut self) {
        let cur = self.viewport.current_tags();
        let h = self.columns_floor_h();
        let stale: Vec<u32> = self.columns_strip_order().into_iter()
            .filter(|&tag| tag != cur && self.columns_by_tag.contains_key(&tag))
            .filter(|&tag| {
                let y = self.columns_ws_y(tag);
                self.tagged_windows.iter().any(|tw| {
                    tw.tags == tag && !tw.floating
                        && ((tw.position.y as f64) < y || (tw.position.y as f64) >= y + h)
                })
            })
            .collect();
        for tag in stale {
            self.columns_relayout_floor(tag);
        }
    }

    /// Достать полосу тега `tag` как текущую. Настройка вида
    /// (center-focused-column) общая на композитор, поэтому переносится.
    pub fn columns_load_for(&mut self, tag: u32) {
        let center = self.columns.center_focused;
        let mut layout = self.columns_by_tag.remove(&tag).unwrap_or_default();
        layout.center_focused = center;
        self.columns = layout;
    }

    /// Перенести стол с тега `from` на тег `to` (при схлопывании ленты):
    /// полосу колонок И всю память стола — запомненную раскладку, камеру и факт
    /// посещения. Без последнего схлопывание оставляло бы за собой «чужие»
    /// этажи: тег, с которого стол уехал, так и помнил бы Columns, а тег, на
    /// который он приехал, — чужую раскладку от прошлого жильца.
    fn columns_rekey(&mut self, from: u32, to: u32) {
        if from == to {
            return;
        }
        if let Some(l) = self.columns_by_tag.remove(&from) {
            self.columns_by_tag.insert(to, l);
        }
        if let Some(l) = self.tag_layouts.remove(&from) {
            self.tag_layouts.insert(to, l);
        }
        if let Some(c) = self.tag_cameras.remove(&from) {
            self.tag_cameras.insert(to, c);
        }
        if self.visited_tags.remove(&from) {
            self.visited_tags.insert(to);
        }
    }

    /// niri: `switch-focus-between-floating-and-tiling` — перекинуть фокус
    /// между плавающим слоем и полосой колонок. Плавающие окна в Columns уже
    /// живут поверх полосы (columns_reconcile их не берёт), не хватало только
    /// способа попасть в них с клавиатуры и вернуться обратно.
    pub fn columns_focus_other_layer(&mut self) {
        if self.tile_config.layout != Layout::Columns {
            return;
        }
        let current = self.viewport.current_tags();
        let focused_floating = self.focused_surface()
            .and_then(|s| self.tagged_windows.iter().find(|tw| crate::xwin::is_surface(&tw.window, &s)))
            .map(|tw| tw.floating)
            .unwrap_or(false);

        if focused_floating {
            // Обратно в полосу — на активную колонку.
            self.columns_reconcile();
            if let Some(w) = self.columns_active_window() {
                self.columns_give_focus(&w);
                self.columns_scroll_to_active();
                self.request_redraw();
            }
            return;
        }

        // В плавающий слой: берём верхнее плавающее окно этого стола.
        let target = self.tagged_windows.iter()
            .filter(|tw| tw.floating && tw.tags & current != 0)
            .map(|tw| tw.window.clone())
            .next_back();
        if let Some(w) = target {
            self.columns_give_focus(&w);
            self.request_redraw();
        } else {
            tracing::debug!("dawn/columns: плавающих окон на этом столе нет");
        }
    }

    /// Куда встанет окно, если бросить его сейчас: номер колонки, перед которой
    /// оно вставится, и «в стопку ли» (бросили на середину существующей
    /// колонки). Это модель niri: между колонками — новая колонка, поверх
    /// колонки — в её стопку.
    pub fn columns_insert_target(&self, pos_x: f64) -> (usize, bool) {
        let working_w = self.primary_output_geo().map(|g| g.size.w as f64).unwrap_or(1920.0);
        insert_target_in(&self.columns, working_w, pos_x)
    }

    /// То же, но для полосы ЧУЖОГО стола (niri-обзор: окно бросают на соседний
    /// этаж). Полоса текущего стола лежит в `self.columns`, остальные — в
    /// `columns_by_tag`; пустой этаж принимает окно первой колонкой.
    pub fn columns_insert_target_for(&self, tag: u32, pos_x: f64) -> (usize, bool) {
        let working_w = self.primary_output_geo().map(|g| g.size.w as f64).unwrap_or(1920.0);
        if tag == self.viewport.current_tags() {
            return insert_target_in(&self.columns, working_w, pos_x);
        }
        match self.columns_by_tag.get(&tag) {
            Some(l) => insert_target_in(l, working_w, pos_x),
            None => (0, false),
        }
    }

    /// Прямоугольник подсказки вставки (canvas-координаты) для текущей позиции
    /// курсора — рисуется в udev.rs, пока тащат окно.
    pub fn columns_insert_hint_rect(&self, pos_x: f64) -> Option<Rectangle<i32, Logical>> {
        let geo = self.primary_output_geo()?;
        let working_w = geo.size.w as f64;
        let avail_h = (geo.size.h - GAP_OUTER * 2).max(1);
        let (idx, into_stack) = self.columns_insert_target(pos_x);
        if into_stack {
            let col = self.columns.columns.get(idx)?;
            let x = self.columns_column_x(idx, working_w);
            let w = col.width_px(working_w);
            // Подсказка «в стопку» — нижняя половина колонки.
            let h = avail_h / 2;
            Some(Rectangle::new(
                (x.round() as i32, GAP_OUTER + avail_h - h).into(),
                (w.round() as i32, h).into(),
            ))
        } else {
            // Новая колонка: узкая полоса в шов между колонками.
            let x = self.columns_column_x(idx, working_w);
            let w = (COL_GAP as i32).max(6);
            Some(Rectangle::new(
                ((x - COL_GAP) as i32, GAP_OUTER).into(),
                (w, avail_h).into(),
            ))
        }
    }

    /// Бросили окно в полосу: вставить его туда, куда показывала подсказка.
    pub fn columns_drop_window(&mut self, window: &Window, pos_x: f64) {
        if self.tile_config.layout != Layout::Columns {
            return;
        }
        let (idx, into_stack) = self.columns_insert_target(pos_x);
        // Убираем окно с прежнего места в полосе (если оно там было).
        for col in &mut self.columns.columns {
            col.windows.retain(|w| w != window);
            col.row_weights.clear();
            col.clamp_row();
        }
        self.columns.columns.retain(|c| !c.windows.is_empty());
        let idx = idx.min(self.columns.columns.len());
        if into_stack && idx < self.columns.columns.len() {
            let col = &mut self.columns.columns[idx];
            col.windows.push(window.clone());
            col.active_row = col.windows.len() - 1;
        } else {
            self.columns.columns.insert(idx, Column::single(window.clone()));
        }
        self.columns.active = idx.min(self.columns.columns.len().saturating_sub(1));
        // Окно вернулось в полосу — оно больше не плавающее.
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) {
            tw.floating = false;
        }
        self.arrange();
        self.columns_give_focus(&window.clone());
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// Бросили окно в полосу СОСЕДНЕГО этажа (niri-обзор): окно меняет стол и
    /// встаёт в полосу того стола туда, куда показывал курсор по X. Тот же
    /// contract, что у [`Dawn::columns_drop_window`], только приёмник — не
    /// обязательно текущий стол.
    pub fn columns_drop_window_on_ws(&mut self, window: &Window, tag: u32, pos_x: f64) {
        if self.tile_config.layout != Layout::Columns {
            return;
        }
        if tag == self.viewport.current_tags() {
            self.columns_drop_window(window, pos_x);
            return;
        }
        // Куда встанет — считаем ДО того, как окно вынуто из полос: полоса
        // приёмника от этого не меняется, а порядок вычислений так очевиднее.
        let (idx, into_stack) = self.columns_insert_target_for(tag, pos_x);
        // Этаж-донор нужно будет пересобрать: окно могли тащить и с НЕ текущего
        // стола (в обзоре ленты видны все этажи сразу).
        let old_tag = self.tagged_windows.iter()
            .find(|tw| &tw.window == window)
            .map(|tw| tw.tags);

        // Вынимаем окно из полосы-донора (текущей или чужой — окно могло
        // приехать с третьего этажа).
        remove_window_from(&mut self.columns, window);
        for l in self.columns_by_tag.values_mut() {
            remove_window_from(l, window);
        }

        let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) else {
            return;
        };
        tw.tags = tag;
        // Окно вернулось в полосу — оно больше не плавающее (как в drop_window).
        tw.floating = false;

        let dst = self.columns_by_tag.entry(tag).or_default();
        let idx = idx.min(dst.columns.len());
        if into_stack && idx < dst.columns.len() {
            let col = &mut dst.columns[idx];
            col.windows.push(window.clone());
            col.active_row = col.windows.len() - 1;
        } else {
            dst.columns.insert(idx, Column::single(window.clone()));
        }
        dst.active = idx.min(dst.columns.len().saturating_sub(1));

        self.refresh_tags();
        // Обе полосы пересобираются прямо на своих этажах — переключаться на
        // них не надо (см. columns_layout_tag; текущий стол уходит в arrange).
        self.arrange();
        if let Some(old) = old_tag.filter(|&t| t != tag) {
            self.columns_layout_tag(old);
        }
        self.columns_layout_tag(tag);
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn/columns: окно брошено на стол {:#b} (колонка {})", tag, idx);
    }

    /// Отдать окно полосе стола `tag` — программный аналог броска мышью
    /// ([`Dawn::columns_drop_window_on_ws`]), только место в полосе выбирает не
    /// курсор, а правило niri: НОВАЯ КОЛОНКА сразу справа от активной.
    ///
    /// В отличие от drop-версии работает из ЛЮБОЙ раскладки: сюда приходит
    /// Win+Shift+2 с тайлового стола, где `self.columns` пуста, а полоса
    /// приёмника лежит на полке `columns_by_tag`. Без этого окно попадало в
    /// ленту вслепую — только сменой тега, а колонку ему заводил ленивый
    /// `columns_reconcile` при следующем заходе на стол, всегда в КОНЕЦ полосы
    /// и мимо активной колонки.
    ///
    /// Возвращает false, если окна нет в `tagged_windows` (вызывающий тогда
    /// ничего не менял).
    pub fn columns_adopt_window(&mut self, window: &Window, tag: u32) -> bool {
        let cur = self.viewport.current_tags();
        let old_tag = self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, window))
            .map(|tw| tw.tags);

        // Вынимаем из полосы-донора: текущей или лежащей на полке. Окно могло
        // не быть ни в одной (пришло из тайлинга) — тогда это no-op.
        remove_window_from(&mut self.columns, window);
        for l in self.columns_by_tag.values_mut() {
            remove_window_from(l, window);
        }

        let Some(tw) = self.tagged_windows.iter_mut()
            .find(|tw| same_window(&tw.window, window))
        else {
            return false;
        };
        tw.tags = tag;
        // Окно встало в полосу — оно больше не плавающее (как в drop_window).
        tw.floating = false;

        // Живая полоса текущего стола лежит в self.columns, полосы остальных —
        // на полке. Пишем ровно в ту, которой стол пользуется, иначе колонка
        // потерялась бы на ближайшем columns_save_for.
        let dst = if tag == cur && self.tile_config.layout == Layout::Columns {
            &mut self.columns
        } else {
            self.columns_by_tag.entry(tag).or_default()
        };
        let idx = (dst.active + 1).min(dst.columns.len());
        dst.columns.insert(idx, Column::single(window.clone()));
        dst.active = idx;

        self.refresh_tags();
        self.arrange();
        // Этаж-приёмник пересобираем на месте, только если он СЕЙЧАС на холсте:
        // в ленте видны все её этажи сразу (см. refresh_tags). Вне ленты чужой
        // стол не смаплен, и раскладывать его нечего — полосу он соберёт при
        // заходе (columns_load_for + arrange), уже с готовой колонкой.
        if self.tile_config.layout == Layout::Columns {
            if let Some(old) = old_tag.filter(|&t| t != tag && self.columns_is_strip_tag(t)) {
                self.columns_layout_tag(old);
            }
            self.columns_layout_tag(tag);
        }
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn/columns: окно принято столом {:#b} (колонка {})", tag, idx);
        true
    }

    /// Габариты ВСЕЙ ленты на холсте: этажи всех ленточных столов вместе с их
    /// колонками. Это кадр, который niri-обзор вписывает в экран.
    pub fn columns_strip_bbox(&self) -> Option<Rectangle<i32, Logical>> {
        let geo = self.primary_output_geo()?;
        let (mut min_x, mut min_y) = (0, i32::MAX);
        let (mut max_x, mut max_y) = (geo.size.w, i32::MIN);
        for tw in &self.tagged_windows {
            if tw.tags == 0 || !self.columns_is_strip_tag(tw.tags) {
                continue;
            }
            let floor_y = self.columns_ws_y(tw.tags).round() as i32;
            min_y = min_y.min(floor_y);
            max_y = max_y.max(floor_y + geo.size.h);
            if let Some(g) = self.space.element_geometry(&tw.window) {
                min_x = min_x.min(g.loc.x);
                max_x = max_x.max(g.loc.x + g.size.w + COL_GAP as i32);
            }
        }
        // Ни одного окна в ленте — показываем хотя бы текущий этаж.
        if min_y > max_y {
            let y = self.columns_cur_y().round() as i32;
            min_y = y;
            max_y = y + geo.size.h;
        }
        Some(Rectangle::new(
            (min_x, min_y).into(),
            ((max_x - min_x).max(1), (max_y - min_y).max(1)).into(),
        ))
    }

    /// Схлопнуть дыры в ленте столов — niri-модель динамических воркспейсов:
    /// пустой стол в СЕРЕДИНЕ не живёт, лента всегда плотная, а пустой ровно
    /// один и всегда последний.
    ///
    /// В dawn стол — это бит тега, поэтому «схлопнуть» значит перенумеровать
    /// теги окон так, чтобы занятыми оказались 1..k без пропусков. Текущий
    /// просматриваемый стол едет вместе со своим содержимым.
    ///
    /// Работает ТОЛЬКО в Columns: в Tile/Float/Monocle теги — это девять
    /// независимых полок, где пустая середина нормальна, и трогать их нельзя.
    pub fn columns_compact_workspaces(&mut self) {
        if self.tile_config.layout != Layout::Columns {
            return;
        }
        // Занятые СВОИ теги по порядку. Чужие столы лента не трогает: их окна
        // ей не принадлежат, а перенумеровав их, она затащила бы тайловый стол
        // внутрь ленты (ровно то, от чего изоляция и защищает).
        let cur = self.viewport.current_tags();
        let mut occupied: Vec<u32> = Vec::new();
        for tw in &self.tagged_windows {
            if tw.tags != 0 && !self.columns_tag_foreign(tw.tags) && !occupied.contains(&tw.tags) {
                occupied.push(tw.tags);
            }
        }
        // Текущий стол держит свой этаж, даже если опустел (в niri пустой
        // текущий воркспейс живёт, пока с него не ушли). Без этого содержимое
        // следующего стола переехало бы прямо под курсор.
        if !occupied.contains(&cur) {
            occupied.push(cur);
        }
        occupied.sort_unstable();
        // Куда переезжает каждый занятый тег: в СВОИ биты по порядку (первый
        // свободный от чужих, второй, ...), а не в 1,2,3 подряд — иначе стол
        // ленты сел бы на бит чужого.
        let slots = self.columns_strip_order();
        let mut moved = false;
        let mut remap: Vec<(u32, u32)> = Vec::new();
        for (i, &old) in occupied.iter().enumerate() {
            let new = match slots.get(i) {
                Some(&m) => m,
                None => break,
            };
            if new != old {
                moved = true;
            }
            remap.push((old, new));
        }
        if !moved {
            return;
        }
        for tw in &mut self.tagged_windows {
            if let Some(&(_, new)) = remap.iter().find(|(old, _)| *old == tw.tags) {
                tw.tags = new;
            }
        }
        // Текущий стол переезжает вместе со своим содержимым (он есть в remap
        // всегда — его добавили выше).
        let new_cur = remap.iter().find(|(old, _)| *old == cur).map(|&(_, n)| n)
            .unwrap_or(cur);
        // Полки с полосами колонок и ПАМЯТЬ СТОЛА (раскладка, камера, факт
        // посещения) переезжают вместе с тегами, иначе стол приехал бы на чужую
        // раскладку и лента на следующем же переходе решила бы, что этот этаж
        // чужой. Порядок важен: переносим от начала, а номера только
        // УМЕНЬШАЮТСЯ (лента схлопывается вверх), так что затирания не будет.
        for &(old, new) in &remap {
            self.columns_rekey(old, new);
        }
        self.viewport.tagset[self.viewport.seltags] = new_cur;
        self.refresh_tags();
        // Этажи переехали — их окна обязаны переехать вместе с ними. Без этого
        // схлопывание двигало только теги, а геометрия соседних этажей
        // оставалась на прежних Y (см. columns_relayout_strip).
        self.columns_relayout_strip();
        tracing::info!("dawn/columns: лента столов схлопнута, занято {}", occupied.len());
    }

    // ── Жесты тачпада (niri: view scroll / workspace switch) ─────────────────

    /// Свайп по горизонтали: вид едет за пальцами непрерывно, без анимации —
    /// колонка «под пальцем» ощущается физически. Прилипание к ближайшей
    /// колонке делает columns_swipe_end на отпускании.
    pub fn columns_swipe_scroll(&mut self, dx: f64) {
        /// Ускорение жеста: за один и тот же ход пальцами вид должен проезжать
        /// заметно больше, чем прошли пальцы — как в niri.
        const SWIPE_GAIN: f64 = 1.6;
        self.camera_anim = None;
        // Экранную точку стрелки запоминаем ДО сдвига вида: свайп — это пан
        // рукой, и курсор обязан остаться на месте монитора уже в этом кадре,
        // а не после отложенной sync_pointer_to_camera (см. pan_camera_by).
        let screen = self.pointer_screen_physical();
        self.viewport.cam_x -= dx * SWIPE_GAIN;
        // Влево дальше начала полосы не уезжаем.
        if self.viewport.cam_x < 0.0 {
            self.viewport.cam_x = 0.0;
        }
        self.viewport.cam_y = self.columns_cur_y();
        self.apply_camera();
        self.pin_pointer_after_camera(screen);
    }

    /// Свайп по вертикали копит ход и на пороге переключает стол — niri так же
    /// листает воркспейсы вертикально.
    pub fn columns_swipe_workspace(&mut self, dy: f64) {
        /// Сколько пикселей хода нужно на один стол.
        const WS_THRESHOLD: f64 = 120.0;
        self.columns_swipe_dy += dy;
        while self.columns_swipe_dy.abs() >= WS_THRESHOLD {
            let dir = if self.columns_swipe_dy > 0.0 { 1 } else { -1 };
            self.columns_swipe_dy -= WS_THRESHOLD * dir as f64;
            self.workspace_step(dir);
        }
    }

    /// Отпустили пальцы: прилипаем к ближайшей колонке и делаем её активной —
    /// в niri вид всегда стоит на колонке, а не между ними.
    pub fn columns_swipe_end(&mut self) {
        self.columns_swipe_dy = 0.0;
        let geo = match self.primary_output_geo() { Some(g) => g, None => return };
        if self.columns.columns.is_empty() {
            return;
        }
        let working_w = geo.size.w as f64;
        let view_x = self.viewport.cam_x;
        // Ближайшая по левому краю к текущему положению вида.
        let mut best = 0usize;
        let mut best_d = f64::MAX;
        for i in 0..self.columns.columns.len() {
            let d = (self.columns_column_x(i, working_w) - COL_GAP - view_x).abs();
            if d < best_d {
                best_d = d;
                best = i;
            }
        }
        self.columns.active = best;
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active();
    }

    /// niri: `toggle-column-tabbed-display` — переключить показ активной
    /// колонки между стопкой и вкладками.
    pub fn columns_toggle_tabbed(&mut self) {
        self.columns_set_active_to_focus();
        let a = self.columns.active;
        let Some(col) = self.columns.columns.get_mut(a) else { return };
        col.tabbed = !col.tabbed;
        let tabbed = col.tabbed;
        self.arrange();
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.request_redraw();
        tracing::info!("dawn/columns: колонка {} → {}", a, if tabbed { "вкладки" } else { "стопка" });
    }

    /// Геометрия полоски вкладок для колонки: (x, y, ширина, высота одной
    /// вкладки, сколько их, активная). Считается в canvas-координатах, рисуется
    /// в udev.rs. None — колонка не вкладочная или окон меньше двух.
    pub fn columns_tab_strip(&self, ci: usize) -> Option<(i32, i32, i32, i32, usize, usize)> {
        let col = self.columns.columns.get(ci)?;
        if !col.tabbed || col.windows.len() < 2 {
            return None;
        }
        let geo = self.primary_output_geo()?;
        let working_w = geo.size.w as f64;
        let x = self.columns_column_x(ci, working_w);
        let avail_h = (geo.size.h - GAP_OUTER * 2).max(1);
        // Полоска живёт в зазоре слева от колонки, как в niri.
        let strip_w = (COL_GAP as i32 / 2).max(3);
        let n = col.windows.len();
        let tab_h = (avail_h / n as i32).max(4);
        Some((
            x.round() as i32 - strip_w - 2,
            GAP_OUTER,
            strip_w,
            tab_h,
            n,
            col.active_row,
        ))
    }

    fn columns_active_window(&self) -> Option<Window> {
        let col = self.columns.columns.get(self.columns.active)?;
        col.windows.get(col.active_row).cloned()
    }

    /// Отдаёт клавиатурный фокус окну и поднимает его (общий хелпер операций
    /// над колонками — повторяет логику tiling.rs::focus_stack).
    fn columns_give_focus(&mut self, window: &Window) {
        crate::xwin::focus(self, &window.clone());
    }

    /// Сосед закрываемого окна в ленте: ПРЕДЫДУЩЕЕ окно в линейном порядке
    /// (колонка за колонкой, сверху вниз внутри колонки). Если закрывается
    /// самое первое — берём следующее за ним, иначе после закрытия первого
    /// окна фокусу некуда деться и лента остаётся без активного окна.
    ///
    /// Возвращает None, если окна нет в ленте или оно в ней единственное.
    pub fn columns_neighbour_before(&self, window: &Window) -> Option<Window> {
        let flat: Vec<Window> = self.columns.columns.iter()
            .flat_map(|col| col.windows.iter().cloned())
            .collect();
        let idx = flat.iter().position(|w| same_window(w, window))?;
        if flat.len() < 2 {
            return None;
        }
        let сосед = if idx > 0 { idx - 1 } else { 1 };
        flat.get(сосед).cloned()
    }

    /// Закрылось окно ленты: фокус уходит на соседа, камера едет за ним.
    ///
    /// Без этого niri-лента после закрытия окна оставалась смотреть в пустоту
    /// (камера стоит там, где была колонка) и без активного окна — приходилось
    /// руками листать назад. Зовётся из forget_window; `был_в_фокусе` говорит,
    /// нужно ли вообще переносить фокус, — если закрыли фоновое окно, чужой
    /// фокус трогать нельзя, а вот камеру подтянуть всё равно надо: колонки
    /// сомкнулись, и активная могла уехать за край кадра.
    pub fn columns_after_close(&mut self, сосед: Option<Window>, был_в_фокусе: bool) {
        if self.tile_config.layout != Layout::Columns {
            return;
        }
        self.columns_reconcile();
        if self.columns.columns.is_empty() {
            // Стол опустел — возвращаем камеру к началу его этажа, иначе она
            // так и висит на месте закрытой колонки.
            self.viewport.cam_x = 0.0;
            self.columns_float_cam = (self.viewport.cam_x, self.viewport.cam_y);
            self.apply_camera();
            return;
        }
        if был_в_фокусе {
            if let Some(w) = сосед.filter(|w| self.columns_contains(w)) {
                self.columns_give_focus(&w);
            }
        }
        self.columns_set_active_to_focus();
        self.columns_scroll_to_active();
        self.columns_clamp_view_to_strip();
    }

    /// Не даёт кадру висеть ЗА концом ленты.
    ///
    /// После закрытия последней колонки прокрутка честно подтягивалась к новой
    /// активной, но справа от неё оставалась пустота во весь освободившийся
    /// экран — именно это и выглядело как «камера не вернулась». niri в такой
    /// ситуации подтягивает вид назад, к правому краю полосы. Влево дальше
    /// нуля тоже не пускаем: начало ленты — это левый край экрана.
    fn columns_clamp_view_to_strip(&mut self) {
        let Some(geo) = self.primary_output_geo() else { return };
        let Some(last) = self.columns.columns.len().checked_sub(1) else { return };
        let view_w = geo.size.w as f64;
        let right = self.columns_column_x(last, view_w)
            + self.columns.columns[last].width_px(view_w)
            + COL_GAP;
        let max_x = (right - view_w).max(0.0);
        let cur = self.columns_target_view_x();
        if cur > max_x + 0.5 {
            self.columns_animate_view_to(max_x);
        } else if cur < -0.5 {
            self.columns_animate_view_to(0.0);
        }
    }

    /// Есть ли окно в полосе колонок текущего стола.
    fn columns_contains(&self, window: &Window) -> bool {
        self.columns.columns.iter()
            .any(|col| col.windows.iter().any(|w| same_window(w, window)))
    }

    /// Синхронизирует активную колонку/строку с текущим клавиатурным фокусом
    /// (например после Super+N или sloppy-focus мышью).
    pub fn columns_set_active_to_focus(&mut self) {
        self.columns_reconcile();
        let focused = match self.focused_surface() {
            Some(f) => f,
            None => return,
        };
        for (ci, col) in self.columns.columns.iter().enumerate() {
            if let Some(ri) = col.windows.iter().position(|w| {
                crate::xwin::is_surface(w, &focused)
            }) {
                self.columns.active = ci;
                self.columns.columns[ci].active_row = ri;
                return;
            }
        }
    }

    /// Левый край колонки `idx` на бесконечной полосе (niri: `column_x`).
    /// Сумма ширин предыдущих колонок с зазорами; начало полосы смещено на
    /// COL_GAP, как и в apply_columns_layout.
    fn columns_column_x(&self, idx: usize, working_w: f64) -> f64 {
        let mut x = COL_GAP;
        for col in self.columns.columns.iter().take(idx) {
            x += col.width_px(working_w) + COL_GAP;
        }
        x
    }

    /// Куда вид едет СЕЙЧАС: цель анимации, если она идёт, иначе текущая
    /// камера. Важно для серии нажатий подряд — niri считает от цели
    /// (`target_view_pos`), иначе второе нажатие считает от полпути и вид
    /// «не доезжает».
    fn columns_target_view_x(&self) -> f64 {
        match &self.camera_anim {
            Some(a) => a.to.x,
            None => self.viewport.cam_x,
        }
    }

    /// Точный перенос niri::compute_new_view_offset.
    ///
    /// Возвращает позицию вида (левый край экрана на полосе), при которой
    /// колонка `col_x..col_x+col_w` показана по правилам niri:
    ///  · колонка шире экрана — прижимаем к её левому краю;
    ///  · колонка уже целиком видна (с полем в зазор) — вид НЕ трогаем;
    ///  · иначе выравниваем по той стороне, до которой ехать ближе.
    fn columns_fit_view(&self, cur_x: f64, view_w: f64, col_x: f64, col_w: f64) -> f64 {
        if view_w <= col_w {
            return col_x;
        }
        let padding = ((view_w - col_w) / 2.0).clamp(0.0, COL_GAP);
        let new_x = col_x - padding;
        let new_right = col_x + col_w + padding;
        if cur_x <= new_x && new_right <= cur_x + view_w {
            return cur_x;
        }
        let dist_left = (cur_x - new_x).abs();
        let dist_right = ((cur_x + view_w) - new_right).abs();
        if dist_left <= dist_right {
            new_x
        } else {
            new_right - view_w
        }
    }

    /// Вид с активной колонкой по центру (niri: compute_new_view_offset_centered).
    fn columns_center_view(&self, cur_x: f64, view_w: f64, col_x: f64, col_w: f64) -> f64 {
        if view_w <= col_w {
            return self.columns_fit_view(cur_x, view_w, col_x, col_w);
        }
        col_x - (view_w - col_w) / 2.0
    }

    /// Подтянуть вид к активной колонке. `prev` — колонка, С КОТОРОЙ уходим:
    /// она нужна режиму OnOverflow, чтобы понять, влезает ли переход целиком.
    pub fn columns_scroll_to_active_from(&mut self, prev: Option<usize>) {
        let geo = match self.primary_output_geo() {
            Some(g) => g,
            None => return,
        };
        if self.columns.columns.is_empty() {
            return;
        }
        let view_w = geo.size.w as f64;
        let working_w = geo.size.w as f64;
        let idx = self.columns.active;
        let col_x = self.columns_column_x(idx, working_w);
        let col_w = self.columns.columns[idx].width_px(working_w);
        let cur = self.columns_target_view_x();

        let target = match self.columns.center_focused {
            CenterFocusedColumn::Always => self.columns_center_view(cur, view_w, col_x, col_w),
            CenterFocusedColumn::Never => self.columns_fit_view(cur, view_w, col_x, col_w),
            CenterFocusedColumn::OnOverflow => {
                // niri: источником берём соседа цели со стороны, откуда пришли,
                // и если «источник + цель» не влезают в экран разом — центрируем.
                let src = match prev {
                    None => return self.columns_scroll_to_active_fit(),
                    Some(p) if p == idx => return self.columns_scroll_to_active_fit(),
                    Some(p) if p > idx => (idx + 1).min(self.columns.columns.len() - 1),
                    Some(_) => idx.saturating_sub(1),
                };
                let src_x = self.columns_column_x(src, working_w);
                let src_w = self.columns.columns[src].width_px(working_w);
                let total = if src_x < col_x {
                    col_x - src_x + col_w
                } else {
                    src_x - col_x + src_w
                } + COL_GAP * 2.0;
                if total <= view_w {
                    self.columns_fit_view(cur, view_w, col_x, col_w)
                } else {
                    self.columns_center_view(cur, view_w, col_x, col_w)
                }
            }
        };

        self.columns_animate_view_to(target);
    }

    /// Короткая форма: подтянуть вид «как влезет», без учёта откуда пришли.
    pub fn columns_scroll_to_active(&mut self) {
        self.columns_scroll_to_active_from(None);
    }

    fn columns_scroll_to_active_fit(&mut self) {
        let geo = match self.primary_output_geo() { Some(g) => g, None => return };
        let view_w = geo.size.w as f64;
        let idx = self.columns.active;
        let col_x = self.columns_column_x(idx, view_w);
        let col_w = self.columns.columns[idx].width_px(view_w);
        let cur = self.columns_target_view_x();
        let target = self.columns_fit_view(cur, view_w, col_x, col_w);
        self.columns_animate_view_to(target);
    }

    fn columns_animate_view_to(&mut self, target_x: f64) {
        let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let to = Point::from((target_x, self.columns_cur_y()));
        if (to.x - from.x).abs() > 0.5 || (to.y - from.y).abs() > 0.5 {
            self.camera_anim = Some(CameraAnim::new(from, to, Duration::from_millis(220)));
        }
    }

    /// Super+C (niri: `center-column`) — поставить активную колонку по центру.
    pub fn columns_center_active(&mut self) {
        self.columns_set_active_to_focus();
        let geo = match self.primary_output_geo() { Some(g) => g, None => return };
        if self.columns.columns.is_empty() { return; }
        let view_w = geo.size.w as f64;
        let idx = self.columns.active;
        let col_x = self.columns_column_x(idx, view_w);
        let col_w = self.columns.columns[idx].width_px(view_w);
        let cur = self.columns_target_view_x();
        let target = self.columns_center_view(cur, view_w, col_x, col_w);
        self.columns_animate_view_to(target);
        self.request_redraw();
    }

    /// Super+←/→ (dcol), Super+↑/↓ (drow): сдвиг фокуса по колонкам/строкам.
    /// Без заворачивания (как niri) — упор в край просто ничего не делает.
    pub fn columns_focus(&mut self, dcol: i32, drow: i32) {
        // Отталкиваемся от реально сфокусированного окна (мог смениться мышью).
        self.columns_set_active_to_focus();
        if self.columns.columns.is_empty() {
            return;
        }
        // Колонка, С КОТОРОЙ уходим, нужна режиму OnOverflow (см.
        // columns_scroll_to_active_from) — он по ней решает, влезает ли переход.
        let prev = self.columns.active;
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
        self.columns_scroll_to_active_from(Some(prev));
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
        let prev = self.columns.active;
        self.columns.active = ci;
        self.columns.columns[ci].active_row = ri;
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active_from(Some(prev));
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

    /// Super+R (niri: `switch-preset-column-width`): следующий пресет ширины.
    /// Если ширина была задана вручную (drag или ±%), цикл начинается заново с
    /// первого пресета — так же ведёт себя niri.
    pub fn columns_cycle_width(&mut self) {
        self.columns_set_active_to_focus();
        if let Some(col) = self.columns.columns.get_mut(self.columns.active) {
            col.drag_width = None;
            let next = match col.preset_idx {
                Some(i) => (i + 1) % PRESET_WIDTHS.len(),
                None => 0,
            };
            col.preset_idx = Some(next);
            col.width = ColumnWidth::Proportion(PRESET_WIDTHS[next]);
        }
        self.arrange();
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// niri: `switch-preset-window-height` — следующий пресет высоты активного
    /// окна внутри колонки. В колонке из одного окна смысла нет (оно и так во
    /// всю высоту), как и в niri.
    pub fn columns_cycle_height(&mut self) {
        self.columns_set_active_to_focus();
        let a = self.columns.active;
        let Some(col) = self.columns.columns.get_mut(a) else { return };
        if col.windows.len() <= 1 {
            return;
        }
        let row = col.active_row;
        let cur = col.row_fraction(row);
        // Берём следующий пресет строго больше текущего, иначе первый.
        let next = PRESET_HEIGHTS.iter().copied()
            .find(|&p| p > cur + 0.01)
            .unwrap_or(PRESET_HEIGHTS[0]);
        col.set_row_fraction(row, next);
        self.arrange();
        self.request_redraw();
    }

    /// niri: `set-column-width "+10%"` / `"-10%"` — подвинуть ширину активной
    /// колонки на долю экрана. Пресет при этом «слетает»: следующий Super+R
    /// начнёт цикл сначала.
    pub fn columns_adjust_width(&mut self, delta_percent: f64) {
        self.columns_set_active_to_focus();
        let geo = match self.primary_output_geo() { Some(g) => g, None => return };
        let working_w = geo.size.w as f64;
        let a = self.columns.active;
        let Some(col) = self.columns.columns.get_mut(a) else { return };
        let cur = match col.drag_width {
            Some(f) => f,
            None => col.width.proportion(working_w),
        };
        let next = (cur + delta_percent / 100.0).clamp(0.05, 2.0);
        col.drag_width = None;
        col.preset_idx = None;
        col.width = ColumnWidth::Proportion(next);
        self.arrange();
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// niri: `set-window-height "+10%"` — доля высоты активного окна в колонке.
    pub fn columns_adjust_height(&mut self, delta_percent: f64) {
        self.columns_set_active_to_focus();
        let a = self.columns.active;
        let Some(col) = self.columns.columns.get_mut(a) else { return };
        if col.windows.len() <= 1 {
            return;
        }
        let row = col.active_row;
        let cur = col.row_fraction(row);
        col.set_row_fraction(row, cur + delta_percent / 100.0);
        self.arrange();
        self.request_redraw();
    }

    /// niri: `reset-window-height` — вернуть окнам колонки равные высоты.
    pub fn columns_reset_heights(&mut self) {
        self.columns_set_active_to_focus();
        let a = self.columns.active;
        let Some(col) = self.columns.columns.get_mut(a) else { return };
        col.row_weights.clear();
        self.arrange();
        self.request_redraw();
    }

    /// niri: `maximize-column` — колонка на всю рабочую ширину и обратно к
    /// половине. Пресет сбрасывается, как и при ручном ресайзе.
    pub fn columns_maximize(&mut self) {
        self.columns_set_active_to_focus();
        let geo = match self.primary_output_geo() { Some(g) => g, None => return };
        let working_w = geo.size.w as f64;
        let a = self.columns.active;
        let Some(col) = self.columns.columns.get_mut(a) else { return };
        let cur = col.drag_width.unwrap_or_else(|| col.width.proportion(working_w));
        col.drag_width = None;
        if cur >= 0.99 {
            col.preset_idx = Some(1);
            col.width = ColumnWidth::Proportion(PRESET_WIDTHS[1]);
        } else {
            col.preset_idx = None;
            col.width = ColumnWidth::Proportion(1.0);
        }
        self.arrange();
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// niri: `focus-column-first` / `focus-column-last`.
    pub fn columns_focus_edge(&mut self, last: bool) {
        self.columns_set_active_to_focus();
        if self.columns.columns.is_empty() {
            return;
        }
        let prev = self.columns.active;
        self.columns.active = if last { self.columns.columns.len() - 1 } else { 0 };
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active_from(Some(prev));
        self.request_redraw();
    }

    /// niri: `move-column-to-first` / `move-column-to-last`.
    pub fn columns_move_to_edge(&mut self, last: bool) {
        self.columns_set_active_to_focus();
        let n = self.columns.columns.len();
        if n < 2 {
            return;
        }
        let a = self.columns.active;
        let col = self.columns.columns.remove(a);
        let target = if last { n - 1 } else { 0 };
        self.columns.columns.insert(target, col);
        self.columns.active = target;
        self.arrange();
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// niri: `consume-or-expel-window-left/right` — одна клавиша на оба
    /// действия. Если окно в колонке не одно, оно ВЫТАЛКИВАЕТСЯ в соседнюю
    /// колонку с этой стороны (создавая её при надобности); если одно — уходит
    /// в стопку соседней колонки.
    pub fn columns_consume_or_expel(&mut self, dir: i32) {
        self.columns_set_active_to_focus();
        let a = self.columns.active;
        let n = self.columns.columns.len();
        let Some(col) = self.columns.columns.get(a) else { return };
        let single = col.windows.len() <= 1;

        if single {
            // Уходим целой колонкой в стопку соседа.
            let target = if dir < 0 {
                if a == 0 { return; }
                a - 1
            } else {
                if a + 1 >= n { return; }
                a + 1
            };
            let col = self.columns.columns.remove(a);
            let target = if target > a { target - 1 } else { target };
            let w = col.windows.into_iter().next();
            let Some(w) = w else { return };
            let dst = &mut self.columns.columns[target];
            if dir < 0 {
                dst.windows.push(w);
                dst.active_row = dst.windows.len() - 1;
            } else {
                dst.windows.insert(0, w);
                dst.active_row = 0;
            }
            dst.row_weights.clear();
            self.columns.active = target;
        } else {
            // Выталкиваем активное окно в новую колонку с нужной стороны.
            let width = self.columns.columns[a].width;
            let preset = self.columns.columns[a].preset_idx;
            let col = &mut self.columns.columns[a];
            let row = col.active_row;
            let w = col.windows.remove(row);
            col.row_weights.clear();
            col.clamp_row();
            let mut newcol = Column::single(w);
            newcol.width = width;
            newcol.preset_idx = preset;
            let at = if dir < 0 { a } else { a + 1 };
            self.columns.columns.insert(at, newcol);
            self.columns.active = at;
        }

        self.arrange();
        if let Some(w) = self.columns_active_window() {
            self.columns_give_focus(&w);
        }
        self.columns_scroll_to_active();
        self.request_redraw();
    }

    /// Устанавливает активную колонку по КОНКРЕТНОМУ окну (а не по фокусу).
    /// Нужно для Super+RMB-ресайза: граб стартует с focus:None, поэтому активной
    /// должна стать колонка СХВАЧЕННОГО окна, а не сфокусированного.
    pub fn columns_set_active_to_window(&mut self, window: &Window) {
        self.columns_reconcile();
        for (ci, col) in self.columns.columns.iter().enumerate() {
            if let Some(ri) = col.windows.iter().position(|w| {
                w == window
            }) {
                self.columns.active = ci;
                self.columns.columns[ci].active_row = ri;
                return;
            }
        }
    }

    /// Эффективная ширина (доля экрана) колонки, содержащей `window`: текущий
    /// drag_width, иначе фактор пресета. None — окна нет в колонках. Нужна как
    /// база для Super+RMB-ресайза (см. resize_grab.rs).
    pub fn columns_effective_width_of_window(&mut self, window: &Window) -> Option<f64> {
        let tag = self.columns_tag_of_window(window)?;
        let working_w = self.columns_screen_geo().map(|g| g.size.w as f64).unwrap_or(1920.0);
        let layout = self.columns_layout_of_tag_mut(tag)?;
        for col in &layout.columns {
            if col.windows.iter().any(|w| {
                w == window
            }) {
                return Some(col.drag_width.unwrap_or_else(|| col.width.proportion(working_w)));
            }
        }
        None
    }

    /// Эффективная доля высоты (0..1) строки схваченного окна в его колонке.
    /// None — окна нет в колонках. База для вертикального Super+RMB-ресайза.
    pub fn columns_effective_row_fraction_of_window(&mut self, window: &Window) -> Option<f64> {
        let tag = self.columns_tag_of_window(window)?;
        let layout = self.columns_layout_of_tag_mut(tag)?;
        for col in &layout.columns {
            if let Some(ri) = col.windows.iter().position(|w| {
                w == window
            }) {
                return Some(col.row_fraction(ri));
            }
        }
        None
    }

    /// Super+RMB drag (в Columns-режиме): ставит АБСОЛЮТНУЮ ширину колонки
    /// (доля экрана) и долю высоты строки схваченного окна. Активной делает эту
    /// колонку. Раскладывает БЕЗ скролла камеры — во время ресайза камера должна
    /// стоять, иначе вид дёргается (скролл к активной колонке был причиной
    /// «не скроллятся нормально»).
    /// В обзоре ленты правит полосу ТОГО этажа, где лежит окно (там видны все
    /// этажи, и схватить можно окно чужого стола).
    pub fn columns_resize_of_window(&mut self, window: &Window, width_factor: f64, row_fraction: f64) {
        let Some(tag) = self.columns_tag_of_window(window) else { return };
        if tag == self.viewport.current_tags() {
            self.columns_set_active_to_window(window);
        }
        let Some(layout) = self.columns_layout_of_tag_mut(tag) else { return };
        let Some(ci) = layout.columns.iter()
            .position(|c| c.windows.iter().any(|w| same_window(w, window)))
        else {
            return;
        };
        let col = &mut layout.columns[ci];
        let row = col.windows.iter()
            .position(|w| same_window(w, window))
            .unwrap_or(col.active_row);
        col.drag_width = Some(width_factor.clamp(0.15, 1.0));
        col.set_row_fraction(row, row_fraction);
        self.columns_relayout_floor(tag);
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
        // Соседний СВОЙ этаж: диапазон динамический — до пустого стола снизу
        // включительно (niri: перенос вниз на пустой создаёт новый стол), а
        // чужие столы пропускаются, колонка из ленты в них не уезжает.
        let cur = self.viewport.current_tags();
        let mask = match self.columns_strip_neighbor(cur, dir) {
            Some(m) => m,
            None => return,
        };
        let new = Self::columns_ws_index(mask) + 1;
        let wins: Vec<Window> = match self.columns.columns.get(self.columns.active) {
            Some(c) => c.windows.clone(),
            None => return,
        };
        if wins.is_empty() {
            return;
        }
        for w in &wins {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                &tw.window == w
            }) {
                tw.tags = mask;
            }
        }
        // Колонка уезжает на соседний стол ЦЕЛИКОМ — со стопкой, шириной и
        // вкладочностью. Просто отпустить окна было мало: на том столе
        // reconcile разложил бы их по одной колонке (в niri колонка переезжает
        // как есть).
        let moved = self.columns.columns.remove(self.columns.active);
        if self.columns.active >= self.columns.columns.len() {
            self.columns.active = self.columns.columns.len().saturating_sub(1);
        }
        let dst = self.columns_by_tag.entry(mask).or_default();
        dst.columns.push(moved);
        dst.active = dst.columns.len() - 1;
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
