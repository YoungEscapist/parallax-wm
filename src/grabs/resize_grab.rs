use crate::Parallax;
use crate::tiling::Layout;
use smithay::{
    desktop::{Space, Window},
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::compositor,
};
use std::cell::RefCell;

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub struct ResizeEdge: u32 {
        const TOP          = 0b0001;
        const BOTTOM       = 0b0010;
        const LEFT         = 0b0100;
        const RIGHT        = 0b1000;
        const TOP_LEFT     = Self::TOP.bits() | Self::LEFT.bits();
        const BOTTOM_LEFT  = Self::BOTTOM.bits() | Self::LEFT.bits();
        const TOP_RIGHT    = Self::TOP.bits() | Self::RIGHT.bits();
        const BOTTOM_RIGHT = Self::BOTTOM.bits() | Self::RIGHT.bits();
    }
}

impl From<xdg_toplevel::ResizeEdge> for ResizeEdge {
    fn from(x: xdg_toplevel::ResizeEdge) -> Self {
        Self::from_bits(x as u32).unwrap()
    }
}

pub struct ResizeSurfaceGrab {
    pub start_data: PointerGrabStartData<Parallax>,
    pub window: Window,
    pub edges: ResizeEdge,
    pub initial_rect: Rectangle<i32, Logical>,
    pub last_window_size: Size<i32, Logical>,
    /// Остальные окна из "созвездия" этого окна и их геометрия на момент
    /// начала ресайза — на отпускании масштабируются вокруг общего опорного
    /// угла на тот же коэффициент, что и перетаскиваемое окно (см. button()).
    group_initial: Vec<(Window, Rectangle<i32, Logical>)>,
    /// Суммарная дельта курсора, уже отданная dwindle-ресайзу (Tile mode):
    /// дереву нужен ИНКРЕМЕНТ с прошлого события, а не путь от старта.
    tile_last_delta: Point<f64, Logical>,
    /// Эффективная ширина колонки схваченного окна на старте ресайза (Columns
    /// mode), захватывается на ПЕРВОМ motion. Нужна, чтобы ширину считать как
    /// base + total_delta, а не накапливать (иначе колонка «разгоняется»).
    col_resize_base: Option<f64>,
    /// То же для доли высоты строки схваченного окна (вертикальный ресайз в
    /// Columns): доля = base + total_delta_y/avail_h.
    row_resize_base: Option<f64>,
    /// Склейка driftwm (Super+M, is_glue_enabled, см. cluster.rs): плавающие
    /// окна, касавшиеся краями резайзимого окна на старте хвата, толкаются
    /// вслед за растущим/уменьшающимся краем. Пусто, если склейка выключена,
    /// окно тайловое, или в "созвездии" (group_initial) уже кто-то есть —
    /// два способа тащить соседей друг другу не мешают только порознь.
    glue: crate::cluster::GlueResizeSnapshot,
    /// Соседи по прилипанию к краям экрана (как в KDE): окна, прилипшие
    /// впритык к краю, за который тянут, и их геометрия на старте. Тянешь
    /// границу между двумя половинами — вторая ужимается или растёт следом.
    /// Снимается на первом ходу (в `start` нет доступа к состоянию).
    край_соседи: Option<Vec<(Window, Rectangle<i32, Logical>, ResizeEdge)>>,
}

/// Ширина невидимой полосы снаружи окна (экранные px), за которую его тянут
/// левой кнопкой без Super. Половина обычного зазора тайлинга: между двумя
/// плитками полосы соседей встречаются посередине и не спорят.
pub const КРОМКА_ЗОНА: f64 = 8.0;

/// Отрезок кромки у угла (экранные px), где тянется угол, а не сторона.
pub const КРОМКА_УГОЛ: f64 = 28.0;

/// Какой край окна `рамка` лежит под точкой `pos`, если она в полосе `зона`
/// СНАРУЖИ рамки. Внутри рамки и дальше полосы — `None`.
pub fn край_кромки(
    рамка: Rectangle<f64, Logical>,
    pos: Point<f64, Logical>,
    зона: f64,
    угол: f64,
) -> Option<ResizeEdge> {
    let (x0, y0) = (рамка.loc.x, рамка.loc.y);
    let (x1, y1) = (x0 + рамка.size.w, y0 + рамка.size.h);
    if pos.x < x0 - зона || pos.x >= x1 + зона || pos.y < y0 - зона || pos.y >= y1 + зона {
        return None;
    }
    let (лево, право, верх, низ) = (pos.x < x0, pos.x >= x1, pos.y < y0, pos.y >= y1);
    let mut край = ResizeEdge::empty();
    край.set(ResizeEdge::LEFT, лево);
    край.set(ResizeEdge::RIGHT, право);
    край.set(ResizeEdge::TOP, верх);
    край.set(ResizeEdge::BOTTOM, низ);
    if край.is_empty() {
        return None;
    }
    // У угла сторона тянется вместе с соседней — но не больше трети стороны,
    // иначе у узкого окна от «просто стороны» ничего не останется.
    let (ух, уу) = (угол.min(рамка.size.w / 3.0), угол.min(рамка.size.h / 3.0));
    if !верх && !низ {
        край.set(ResizeEdge::TOP, pos.y < y0 + уу);
        край.set(ResizeEdge::BOTTOM, pos.y >= y1 - уу);
    }
    if !лево && !право {
        край.set(ResizeEdge::LEFT, pos.x < x0 + ух);
        край.set(ResizeEdge::RIGHT, pos.x >= x1 - ух);
    }
    Some(край)
}

/// Стрелка ресайза для края.
pub fn значок_кромки(край: ResizeEdge) -> smithay::input::pointer::CursorIcon {
    use smithay::input::pointer::CursorIcon::*;
    match край {
        ResizeEdge::TOP_LEFT => NwResize,
        ResizeEdge::TOP_RIGHT => NeResize,
        ResizeEdge::BOTTOM_LEFT => SwResize,
        ResizeEdge::BOTTOM_RIGHT => SeResize,
        ResizeEdge::TOP => NResize,
        ResizeEdge::BOTTOM => SResize,
        ResizeEdge::LEFT => WResize,
        _ => EResize,
    }
}

/// Меньше этого сосед по прилипанию не ужимается: дальше тянущее окно
/// упирается, а не выдавливает соседа в полоску.
const МИН_СОСЕДА: i32 = 120;

/// Какими из тянущихся краёв `edges` окно `хват` примыкает к `сосед`.
fn примыкает(
    хват: Rectangle<i32, Logical>,
    сосед: Rectangle<i32, Logical>,
    edges: ResizeEdge,
    допуск: i32,
) -> ResizeEdge {
    let по_вертикали = хват.loc.y < сосед.loc.y + сосед.size.h && сосед.loc.y < хват.loc.y + хват.size.h;
    let по_горизонтали = хват.loc.x < сосед.loc.x + сосед.size.w && сосед.loc.x < хват.loc.x + хват.size.w;
    let mut стороны = ResizeEdge::empty();
    if по_вертикали {
        if edges.contains(ResizeEdge::RIGHT) && (сосед.loc.x - (хват.loc.x + хват.size.w)).abs() <= допуск {
            стороны |= ResizeEdge::RIGHT;
        }
        if edges.contains(ResizeEdge::LEFT) && (сосед.loc.x + сосед.size.w - хват.loc.x).abs() <= допуск {
            стороны |= ResizeEdge::LEFT;
        }
    }
    if по_горизонтали {
        if edges.contains(ResizeEdge::BOTTOM) && (сосед.loc.y - (хват.loc.y + хват.size.h)).abs() <= допуск {
            стороны |= ResizeEdge::BOTTOM;
        }
        if edges.contains(ResizeEdge::TOP) && (сосед.loc.y + сосед.size.h - хват.loc.y).abs() <= допуск {
            стороны |= ResizeEdge::TOP;
        }
    }
    стороны
}

/// Геометрия соседа, когда тянущее окно выросло на (`dw`, `dh`): сосед
/// отдаёт ровно столько же, зазор между окнами не меняется.
fn подвинуть_соседа(
    сосед: Rectangle<i32, Logical>,
    стороны: ResizeEdge,
    dw: i32,
    dh: i32,
) -> Rectangle<i32, Logical> {
    let mut r = сосед;
    if стороны.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
        r.size.w = (сосед.size.w - dw).max(1);
        if стороны.contains(ResizeEdge::RIGHT) {
            r.loc.x = сосед.loc.x + сосед.size.w - r.size.w;
        }
    }
    if стороны.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
        r.size.h = (сосед.size.h - dh).max(1);
        if стороны.contains(ResizeEdge::BOTTOM) {
            r.loc.y = сосед.loc.y + сосед.size.h - r.size.h;
        }
    }
    r
}

/// Прилипшие к краям окна текущего стола, примыкающие к тянущемуся краю
/// `window`. Пусто, если само окно не прилипшее или раскладка не тайловая.
fn прилипшие_соседи(
    data: &Parallax,
    window: &Window,
    edges: ResizeEdge,
    rect: Rectangle<i32, Logical>,
) -> Vec<(Window, Rectangle<i32, Logical>, ResizeEdge)> {
    if data.tile_config.layout == Layout::Float || data.overview_active
        || !data.до_прилипания.iter().any(|(w, _)| w == window)
    {
        return Vec::new();
    }
    let tags = data.viewport.current_tags();
    let допуск = crate::tiling::GAP_INNER() + ADJACENCY_TOLERANCE;
    data.до_прилипания.iter()
        .map(|(w, _)| w)
        .filter(|w| *w != window)
        .filter(|w| data.tagged_windows.iter().any(|tw| &tw.window == *w && tw.floating && tw.tags & tags != 0))
        .filter_map(|w| {
            let g = data.space.element_geometry(w)?;
            let r = Rectangle::new(g.loc, crate::xwin::видимый_размер(w, g.size));
            let стороны = примыкает(rect, r, edges, допуск);
            (!стороны.is_empty()).then(|| (w.clone(), r, стороны))
        })
        .collect()
}

impl ResizeSurfaceGrab {
    pub fn start(
        start_data: PointerGrabStartData<Parallax>,
        window: Window,
        edges: ResizeEdge,
        initial_window_rect: Rectangle<i32, Logical>,
        group_initial: Vec<(Window, Rectangle<i32, Logical>)>,
        glue: crate::cluster::GlueResizeSnapshot,
    ) -> Self {
        // У X11-окна поверхности может ещё не быть — ResizeSurfaceState живёт
        // в её данных, но для X11 он и не нужен: позицию при ресайзе от
        // LEFT/TOP там правим сами, не дожидаясь коммита (см. motion).
        if let Some(surface) = crate::xwin::surface(&window) {
            ResizeSurfaceState::with(&surface, |state| {
                *state = ResizeSurfaceState::Resizing {
                    edges,
                    initial_rect: initial_window_rect,
                    last_rect: initial_window_rect,
                };
            });
        }
        Self {
            start_data,
            window,
            edges,
            initial_rect: initial_window_rect,
            last_window_size: initial_window_rect.size,
            group_initial,
            tile_last_delta: Point::from((0.0, 0.0)),
            col_resize_base: None,
            row_resize_base: None,
            glue,
            край_соседи: None,
        }
    }

    /// Шаг ресайза к точке курсора `location` (canvas). Общий для Win+ПКМ
    /// (из `motion`) и жеста Super+3 пальца, который ведёт grab сам, мимо
    /// указателя — так жест ресайзит ровно как мышь во всех раскладках.
    pub fn resize_to(&mut self, data: &mut Parallax, location: Point<f64, Logical>) {
        // Окно в тайлинге? В обзоре — ровно та же проверка: ресайз там обязан
        // работать «как будто обзора нет», то есть тянуть деления раскладки, а
        // не менять размер окна свободно.
        let is_tiled = data.tile_config.layout != Layout::Float
            && data.tagged_windows.iter().any(|tw| {
                !tw.floating
                    && tw.window == self.window
            });

        if is_tiled {
            let delta = location - self.start_data.location;
            if data.tile_config.layout == Layout::Tile {
                // Tile: тянем деления BSP-дерева (Hyprland resizeTarget,
                // smart_resizing). Дереву отдаём ИНКРЕМЕНТ с прошлого события —
                // оно двигает split_ratio, а не пересчитывает от старта.
                let step = delta - self.tile_last_delta;
                self.tile_last_delta = delta;
                let corner = crate::dwindle::Corner {
                    left: self.edges.intersects(ResizeEdge::LEFT),
                    top: self.edges.intersects(ResizeEdge::TOP),
                    right: self.edges.intersects(ResizeEdge::RIGHT),
                    bottom: self.edges.intersects(ResizeEdge::BOTTOM),
                };
                let win = self.window.clone();
                data.dwindle_resize_focused(&win, step, corner);
            } else if data.tile_config.layout == Layout::Columns {
                // Columns: drag по X — ширина колонки, по Y — доля высоты строки
                // СХВАЧЕННОГО окна. Обе оси считаются как base + total_delta от
                // старта (база захватывается на первом motion) — без накопления
                // и без скролла камеры (иначе «разгон» и дёрганье вида).
                let win = self.window.clone();
                let экран = data.screen_size();
                let (out_w, out_h) = (экран.w as f64, экран.h as f64);
                let avail_h = (out_h - (crate::tiling::GAP_OUTER() * 2) as f64).max(1.0);
                let base_w = match self.col_resize_base {
                    Some(b) => b,
                    None => {
                        let b = data.columns_effective_width_of_window(&win).unwrap_or(0.5);
                        self.col_resize_base = Some(b);
                        b
                    }
                };
                let base_h = match self.row_resize_base {
                    Some(b) => b,
                    None => {
                        let b = data.columns_effective_row_fraction_of_window(&win).unwrap_or(0.5);
                        self.row_resize_base = Some(b);
                        b
                    }
                };
                // Потолок общий с клавиатурным путём (см. columns::MAX_WIDTH_FACTOR):
                // мышью колонка растягивается ровно настолько же, насколько
                // Super+клавишами, а не до края экрана.
                let width_factor = (base_w + delta.x / out_w).clamp(
                    crate::columns::MIN_SIZE_FACTOR,
                    crate::columns::MAX_WIDTH_FACTOR,
                );
                let row_fraction = base_h + delta.y / avail_h; // клампится в set_row_fraction
                data.columns_resize_of_window(&win, width_factor, row_fraction);
            }
            // Monocle: не поддерживается.
            return;
        }

        // Floating: обычный resize
        let mut delta = location - self.start_data.location;
        let mut new_w = self.initial_rect.size.w;
        let mut new_h = self.initial_rect.size.h;

        if self.edges.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
            if self.edges.intersects(ResizeEdge::LEFT) { delta.x = -delta.x; }
            new_w = (self.initial_rect.size.w as f64 + delta.x) as i32;
        }
        if self.edges.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
            if self.edges.intersects(ResizeEdge::TOP) { delta.y = -delta.y; }
            new_h = (self.initial_rect.size.h as f64 + delta.y) as i32;
        }

        // МИНИМУМ клиента здесь больше не применяется — ровно тот же размен,
        // что и в тайлинге (см. tiling::fit_to_constraints): окно ужимается до
        // чего угодно, а клиент, который не умеет, просто нарисуется больше
        // рамки. Раньше Discord (min 940×500) упирался на середине потяга — это
        // и было «у окон всё ещё есть лимит на размер». Пол — 1 px: нулевой
        // размер поверхности недопустим.
        //
        // МАКСИМУМ остаётся: это не наше ограничение, а физика клиента (окно с
        // max 800 шире 800 не нарисуется никогда). Без него мы слали бы
        // configure на размер, который клиент заведомо не примет, и потяг за
        // левый край уезжал бы мимо — позиция X11-окна ниже считается ровно от
        // last_window_size.
        //
        // Рамкой стола ресайз в обзоре тоже НЕ ограничен: плавающее окно
        // тянется там ровно так же, как вне обзора (у остальных ресайз идёт
        // тайловой веткой выше).
        let (_, max_size) = crate::xwin::size_constraints(&self.window);
        let mut max_w = if max_size.w <= 0 { i32::MAX } else { max_size.w };
        let mut max_h = if max_size.h <= 0 { i32::MAX } else { max_size.h };
        // Соседи по прилипанию к краям: окно растёт только за их счёт, и не
        // дальше, чем сосед может ужаться.
        if self.край_соседи.is_none() {
            let соседи = прилипшие_соседи(data, &self.window, self.edges, self.initial_rect);
            for (w, _, _) in &соседи {
                data.freeze_window_anim(w);
            }
            self.край_соседи = Some(соседи);
        }
        for (_, r, стороны) in self.край_соседи.iter().flatten() {
            if стороны.intersects(ResizeEdge::LEFT | ResizeEdge::RIGHT) {
                max_w = max_w.min(self.initial_rect.size.w + (r.size.w - МИН_СОСЕДА).max(0));
            }
            if стороны.intersects(ResizeEdge::TOP | ResizeEdge::BOTTOM) {
                max_h = max_h.min(self.initial_rect.size.h + (r.size.h - МИН_СОСЕДА).max(0));
            }
        }
        self.last_window_size = Size::from((
            new_w.clamp(1, max_w),
            new_h.clamp(1, max_h),
        ));

        crate::xwin::set_resizing(&self.window, true);
        crate::xwin::set_size(&self.window, Some(self.last_window_size), crate::xwin::Tiled::Keep);
        crate::xwin::configure(&self.window);
        data.отметить_живой_размер(&self.window);

        // Тянем за левый/верхний край → окно должно ещё и ехать. Двигаем САМИ
        // и СРАЗУ — и X11 (он нового размера не «подтверждает»), и wayland.
        // У wayland это раньше делал handle_commit по факту применения размера
        // клиентом, и левый край прыгал вслед за курсором по коммиту на ступень.
        // Теперь кадр рисует окно в запрошенном размере (растяжка или обрезка,
        // см. `живая_растяжка`), значит и место считается от запрошенного —
        // противоположный угол стоит, как прибитый.
        {
            let mut loc = self.initial_rect.loc;
            if self.edges.intersects(ResizeEdge::LEFT) {
                loc.x += self.initial_rect.size.w - self.last_window_size.w;
            }
            if self.edges.intersects(ResizeEdge::TOP) {
                loc.y += self.initial_rect.size.h - self.last_window_size.h;
            }
            data.space.map_element(self.window.clone(), loc, false);
        }

        // Прилипшие соседи отдают или забирают ровно то, на что изменилось
        // это окно: граница между половинами экрана едет одна на двоих.
        if self.край_соседи.as_ref().is_some_and(|с| !с.is_empty()) {
            let dw = self.last_window_size.w - self.initial_rect.size.w;
            let dh = self.last_window_size.h - self.initial_rect.size.h;
            for (сосед, было, стороны) in self.край_соседи.iter().flatten() {
                let r = подвинуть_соседа(*было, *стороны, dw, dh);
                crate::xwin::set_size(сосед, Some(r.size), crate::xwin::Tiled::Keep);
                crate::xwin::configure(сосед);
                data.отметить_живой_размер(сосед);
                data.space.map_element(сосед.clone(), r.loc, false);
                if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| &tw.window == сосед) {
                    tw.float_position = r.loc;
                    tw.position = r.loc;
                    tw.float_size = Some(r.size);
                    tw.float_position_set = true;
                }
            }
            data.request_redraw();
        }

        // Склейка driftwm (Super+M): соседи, касавшиеся резайзимого окна на
        // старте хвата, толкаются вслед за растущим краем — не масштаб, а
        // сдвиг позиции, ровно как в driftwm (см. cluster.rs).
        if !self.glue.is_empty() {
            let width_delta = self.last_window_size.w - self.initial_rect.size.w;
            let height_delta = self.last_window_size.h - self.initial_rect.size.h;
            for (member, new_pos) in self.glue.compute_member_positions(width_delta, height_delta) {
                data.space.map_element(member.clone(), new_pos, false);
                if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| tw.window == member) {
                    tw.float_position = new_pos;
                    tw.position = new_pos;
                    tw.float_position_set = true;
                }
            }
            data.request_redraw();
        }

        // "Созвездие": остальные окна группы масштабируются ЖИВО вместе с
        // перетаскиваемым (раньше это делалось только на отпускании — соседи
        // лишь перестраивались, но не меняли размер во время драга).
        if !self.group_initial.is_empty() {
            let scale_x = self.last_window_size.w as f64 / self.initial_rect.size.w.max(1) as f64;
            let scale_y = self.last_window_size.h as f64 / self.initial_rect.size.h.max(1) as f64;
            let anchor_x = if self.edges.intersects(ResizeEdge::LEFT) {
                self.initial_rect.loc.x + self.initial_rect.size.w
            } else {
                self.initial_rect.loc.x
            };
            let anchor_y = if self.edges.intersects(ResizeEdge::TOP) {
                self.initial_rect.loc.y + self.initial_rect.size.h
            } else {
                self.initial_rect.loc.y
            };
            for (member, geo) in self.group_initial.clone() {
                let new_w = ((geo.size.w as f64) * scale_x).round().max(1.0) as i32;
                let new_h = ((geo.size.h as f64) * scale_y).round().max(1.0) as i32;
                let new_x = (anchor_x as f64 + (geo.loc.x - anchor_x) as f64 * scale_x).round() as i32;
                let new_y = (anchor_y as f64 + (geo.loc.y - anchor_y) as f64 * scale_y).round() as i32;
                crate::xwin::set_size(&member, Some((new_w, new_h).into()), crate::xwin::Tiled::Keep);
                crate::xwin::configure(&member);
                data.отметить_живой_размер(&member);
                let new_loc: Point<i32, Logical> = (new_x, new_y).into();
                data.space.map_element(member.clone(), new_loc, false);
            }
            data.request_redraw();
        }
    }

    /// Конец ресайза: отпустили ПКМ или пальцы.
    pub fn finish(&mut self, data: &mut Parallax) {
        // Только для floating (и обзора) — финализируем resize.
        // В tiling/columns: сбрасываем ResizeSurfaceState (commit-хендлер
        // не должен корректировать позицию — ей управляет layout).
        let is_tiled = data.tile_config.layout != Layout::Float
            && data.tagged_windows.iter().any(|tw| {
                !tw.floating
                    && tw.window == self.window
            });

        if is_tiled {
            if let Some(surface) = crate::xwin::surface(&self.window) {
                ResizeSurfaceState::with(&surface, |state| {
                    *state = ResizeSurfaceState::Idle;
                });
            }
        } else {
            crate::xwin::set_resizing(&self.window, false);
            crate::xwin::set_size(&self.window, Some(self.last_window_size), crate::xwin::Tiled::Keep);
            crate::xwin::configure(&self.window);

            // Сохраняем float_size
            if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                tw.window == self.window
            }) {
                tw.float_size = Some(self.last_window_size);
            }
            // В обзоре ресайз должен пережить выход — правим и снимок
            // до-обзорной геометрии (иначе он вернёт прежний размер).
            if data.overview_active {
                data.overview_note_resize(&self.window, self.last_window_size);
            }

            if let Some(surface) = crate::xwin::surface(&self.window) {
                ResizeSurfaceState::with(&surface, |state| {
                    *state = ResizeSurfaceState::WaitingForLastCommit {
                        edges: self.edges,
                        initial_rect: self.initial_rect,
                    };
                });
            }

            // "Созвездие" (Super+G): остальные окна группы масштабируются
            // вокруг общего неподвижного угла на тот же коэффициент.
            if !self.group_initial.is_empty() {
                let scale_x = self.last_window_size.w as f64 / self.initial_rect.size.w.max(1) as f64;
                let scale_y = self.last_window_size.h as f64 / self.initial_rect.size.h.max(1) as f64;
                let anchor_x = if self.edges.intersects(ResizeEdge::LEFT) {
                    self.initial_rect.loc.x + self.initial_rect.size.w
                } else {
                    self.initial_rect.loc.x
                };
                let anchor_y = if self.edges.intersects(ResizeEdge::TOP) {
                    self.initial_rect.loc.y + self.initial_rect.size.h
                } else {
                    self.initial_rect.loc.y
                };
                for (member, geo) in self.group_initial.clone() {
                    let new_w = ((geo.size.w as f64) * scale_x).round().max(1.0) as i32;
                    let new_h = ((geo.size.h as f64) * scale_y).round().max(1.0) as i32;
                    let new_x = (anchor_x as f64 + (geo.loc.x - anchor_x) as f64 * scale_x).round() as i32;
                    let new_y = (anchor_y as f64 + (geo.loc.y - anchor_y) as f64 * scale_y).round() as i32;
                    crate::xwin::set_size(&member, Some((new_w, new_h).into()), crate::xwin::Tiled::Keep);
                    crate::xwin::configure(&member);
                    data.animate_window_to_dur(&member, (new_x, new_y).into(), crate::anim::дуг::сборка_тайлинга());
                    if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                        tw.window == member
                    }) {
                        tw.float_size = Some((new_w, new_h).into());
                        tw.float_position = (new_x, new_y).into();
                    }
                }
                data.request_plane_reset();
            }
        }
    }
}

impl PointerGrab<Parallax> for ResizeSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.resize_to(data, event.location);
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

    fn button(
        &mut self,
        data: &mut Parallax,
        handle: &mut PointerInnerHandle<'_, Parallax>,
        event: &ButtonEvent,
    ) {
        // Вторая кнопка при Super — поворот (см. tilt_grab.rs): ресайз
        // доводится до того размера, что успел набраться.
        if event.button == 0x110 && event.state == smithay::backend::input::ButtonState::Pressed
            && data.seat.get_keyboard().is_some_and(|k| k.modifier_state().logo)
        {
            handle.unset_grab(self, data, event.serial, event.time, false);
            self.finish(data);
            data.поворот_ждёт = Some(self.window.clone());
            return;
        }
        handle.button(data, event);
        // Хват держит та кнопка, что его начала: ПКМ при Super+ПКМ и у жеста,
        // ЛКМ при ресайзе за кромку.
        if !handle.current_pressed().contains(&self.start_data.button) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            self.finish(data);
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
    fn unset(&mut self, data: &mut Parallax) {
        data.кромка_курсор = None;
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Default)]
enum ResizeSurfaceState {
    #[default]
    Idle,
    Resizing {
        edges: ResizeEdge,
        initial_rect: Rectangle<i32, Logical>,
        /// Прямоугольник на прошлом коммите этого же resize-жеста — от него
        /// считается инкрементальная дельта для эластичного расталкивания (2.3).
        last_rect: Rectangle<i32, Logical>,
    },
    WaitingForLastCommit {
        edges: ResizeEdge,
        initial_rect: Rectangle<i32, Logical>,
    },
}

impl ResizeSurfaceState {
    fn with<F, T>(surface: &WlSurface, cb: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        compositor::with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<Self>::default);
            let state = states.data_map.get::<RefCell<Self>>().unwrap();
            cb(&mut state.borrow_mut())
        })
    }

    fn commit(&mut self) -> Option<(ResizeEdge, Rectangle<i32, Logical>)> {
        match *self {
            Self::Resizing { edges, initial_rect, .. } => Some((edges, initial_rect)),
            Self::WaitingForLastCommit { edges, initial_rect } => {
                *self = Self::Idle;
                Some((edges, initial_rect))
            }
            Self::Idle => None,
        }
    }

    /// Только для активного (не финального) resize-коммита: возвращает
    /// прямоугольник с прошлого коммита и запоминает `new_rect` как новый
    /// "прошлый" — на его основе (2.3) считается инкрементальный push соседей.
    fn take_last_rect_for_elastic(
        &mut self,
        new_rect: Rectangle<i32, Logical>,
    ) -> Option<Rectangle<i32, Logical>> {
        match self {
            Self::Resizing { last_rect, .. } => {
                let old = *last_rect;
                *last_rect = new_rect;
                Some(old)
            }
            _ => None,
        }
    }
}

/// Эластичное расталкивание соседей (2.3): окна, чей край примыкал (в
/// пределах ADJACENCY_TOLERANCE) к соответствующему краю `old`, сдвигаются на
/// ту же дельту, на которую этот край сместился в `new`. При уменьшении окна
/// дельта отрицательная — соседи стягиваются обратно.
const ADJACENCY_TOLERANCE: i32 = 20;

fn elastic_displace(
    space: &mut Space<Window>,
    resized: &Window,
    old: Rectangle<i32, Logical>,
    new: Rectangle<i32, Logical>,
) {
    let old_right = old.loc.x + old.size.w;
    let old_bottom = old.loc.y + old.size.h;
    let new_right = new.loc.x + new.size.w;
    let new_bottom = new.loc.y + new.size.h;

    let delta_left = new.loc.x - old.loc.x;
    let delta_top = new.loc.y - old.loc.y;
    let delta_right = new_right - old_right;
    let delta_bottom = new_bottom - old_bottom;

    if delta_left == 0 && delta_top == 0 && delta_right == 0 && delta_bottom == 0 {
        return;
    }

    let others: Vec<(Window, Rectangle<i32, Logical>)> = space
        .elements()
        .filter(|w| {
            *w != resized
        })
        .filter_map(|w| space.element_geometry(w).map(|g| (w.clone(), g)))
        .collect();

    for (other, geo) in others {
        let mut dx = 0;
        let mut dy = 0;

        let other_right = geo.loc.x + geo.size.w;
        let other_bottom = geo.loc.y + geo.size.h;
        let overlaps_y = geo.loc.y < old_bottom && other_bottom > old.loc.y;
        let overlaps_x = geo.loc.x < old_right && other_right > old.loc.x;

        if delta_right != 0 && overlaps_y && (geo.loc.x - old_right).abs() <= ADJACENCY_TOLERANCE {
            dx += delta_right;
        }
        if delta_left != 0 && overlaps_y && (old.loc.x - other_right).abs() <= ADJACENCY_TOLERANCE {
            dx += delta_left;
        }
        if delta_bottom != 0 && overlaps_x && (geo.loc.y - old_bottom).abs() <= ADJACENCY_TOLERANCE {
            dy += delta_bottom;
        }
        if delta_top != 0 && overlaps_x && (old.loc.y - other_bottom).abs() <= ADJACENCY_TOLERANCE {
            dy += delta_top;
        }

        if dx != 0 || dy != 0 {
            let new_other_loc = Point::from((geo.loc.x + dx, geo.loc.y + dy));
            space.map_element(other, new_other_loc, false);
        }
    }
}

/// `elastic` — расталкивать ли соседей примыкающими краями (2.3).
pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface, elastic: bool) -> Option<()> {
    let window = space
        .elements()
        .find(|w| crate::xwin::is_surface(w, surface))
        .cloned()?;

    let mut window_loc = space.element_location(&window)?;
    let geometry = window.geometry();

    // Пока жест идёт, место считается от ЗАПРОШЕННОГО размера — ровно так же,
    // как его ставит motion (см. там): кадр рисует окно в запрошенном размере,
    // и коммит клиента с отстающим буфером не должен дёргать угол назад. На
    // последнем коммите — по факту: там клиент уже сказал своё последнее слово.
    let запрошенный = crate::xwin::requested_size(&window).filter(|s| s.w > 0 && s.h > 0);
    let new_loc: Point<Option<i32>, Logical> = ResizeSurfaceState::with(surface, |state| {
        let идёт = matches!(state, ResizeSurfaceState::Resizing { .. });
        let size = if идёт { запрошенный.unwrap_or(geometry.size) } else { geometry.size };
        state.commit().and_then(|(edges, initial_rect)| {
            edges.intersects(ResizeEdge::TOP_LEFT).then(|| {
                let new_x = edges.intersects(ResizeEdge::LEFT)
                    .then_some(initial_rect.loc.x + (initial_rect.size.w - size.w));
                let new_y = edges.intersects(ResizeEdge::TOP)
                    .then_some(initial_rect.loc.y + (initial_rect.size.h - size.h));
                (new_x, new_y).into()
            })
        }).unwrap_or_default()
    });

    if let Some(new_x) = new_loc.x { window_loc.x = new_x; }
    if let Some(new_y) = new_loc.y { window_loc.y = new_y; }

    if new_loc.x.is_some() || new_loc.y.is_some() {
        space.map_element(window.clone(), window_loc, false);
    }

    // Эластичное расталкивание соседей (2.3) — только на промежуточных
    // коммитах активного resize, не на финальном после отпускания кнопки.
    let new_rect = Rectangle::new(window_loc, geometry.size);
    let old_rect = ResizeSurfaceState::with(surface, |state| state.take_last_rect_for_elastic(new_rect));
    if let (Some(old_rect), true) = (old_rect, elastic) {
        elastic_displace(space, &window, old_rect, new_rect);
    }

    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new((x, y).into(), (w, h).into())
    }

    /// Две половины экрана с зазором: тянем правый край левой — правая
    /// отдаёт ширину и уезжает, зазор и правый край экрана на месте.
    #[test]
    fn половины_делят_одну_границу() {
        let (лев, прав) = (r(10, 10, 940, 1060), r(960, 10, 950, 1060));
        let стороны = примыкает(лев, прав, ResizeEdge::RIGHT, 30);
        assert_eq!(стороны, ResizeEdge::RIGHT);
        let стал = подвинуть_соседа(прав, стороны, 200, 0);
        assert_eq!(стал, r(1160, 10, 750, 1060));
        // С другой стороны: левый край правой половины тянет левую.
        let стороны = примыкает(прав, лев, ResizeEdge::LEFT, 30);
        assert_eq!(стороны, ResizeEdge::LEFT);
        assert_eq!(подвинуть_соседа(лев, стороны, -100, 0), r(10, 10, 1040, 1060));
    }

    /// Кромка — только полоса снаружи рамки; у угла сторона становится углом.
    #[test]
    fn кромка_снаружи_и_углы() {
        let рамка = Rectangle::new((100.0, 100.0).into(), (400.0, 300.0).into());
        let край = |x: f64, y: f64| край_кромки(рамка, (x, y).into(), 8.0, 28.0);
        assert_eq!(край(300.0, 250.0), None, "внутри окна кромки нет");
        assert_eq!(край(90.0, 250.0), None, "дальше полосы");
        assert_eq!(край(96.0, 250.0), Some(ResizeEdge::LEFT));
        assert_eq!(край(503.0, 250.0), Some(ResizeEdge::RIGHT));
        assert_eq!(край(300.0, 95.0), Some(ResizeEdge::TOP));
        assert_eq!(край(300.0, 404.0), Some(ResizeEdge::BOTTOM));
        assert_eq!(край(96.0, 96.0), Some(ResizeEdge::TOP_LEFT));
        assert_eq!(край(503.0, 390.0), Some(ResizeEdge::BOTTOM_RIGHT), "сторона у угла");
        assert_eq!(край(110.0, 404.0), Some(ResizeEdge::BOTTOM_LEFT));
    }

    /// Край, за который не тянут, и окно по диагонали соседями не считаются.
    #[test]
    fn чужой_край_и_диагональ_не_соседи() {
        let (лев, прав) = (r(10, 10, 940, 1060), r(960, 10, 950, 1060));
        assert!(примыкает(лев, прав, ResizeEdge::BOTTOM | ResizeEdge::LEFT, 30).is_empty());
        let (верх_лев, низ_прав) = (r(10, 10, 940, 520), r(960, 540, 950, 530));
        assert!(примыкает(верх_лев, низ_прав, ResizeEdge::BOTTOM_RIGHT, 30).is_empty());
    }

    /// Четверть тянут за угол: сосед справа отдаёт ширину, сосед снизу — высоту.
    #[test]
    fn угол_четверти_двигает_обоих() {
        let хват = r(10, 10, 940, 520);
        let (справа, снизу) = (r(960, 10, 950, 520), r(10, 540, 940, 530));
        let с = примыкает(хват, справа, ResizeEdge::BOTTOM_RIGHT, 30);
        assert_eq!(подвинуть_соседа(справа, с, 50, 70), r(1010, 10, 900, 520));
        let н = примыкает(хват, снизу, ResizeEdge::BOTTOM_RIGHT, 30);
        assert_eq!(подвинуть_соседа(снизу, н, 50, 70), r(10, 610, 940, 460));
    }
}
