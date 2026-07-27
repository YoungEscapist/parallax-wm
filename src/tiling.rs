use std::time::Duration;

use smithay::{
    desktop::Window,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::anim::CameraAnim;
use crate::state::Dawn;

/// Псевдослучайный угол из seed (LCG)
fn lcg_f64(seed: u64) -> f64 {
    let x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (x >> 33) as f64 / (u32::MAX as f64)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layout {
    Tile,    // dwindle горизонтальный
    Float,
    Monocle,
    Columns, // niri-подобные колонки по полэкрана, скролл камерой вправо
}

impl Layout {
    pub fn symbol(&self) -> &'static str {
        match self {
            Layout::Tile    => "[H]",
            Layout::Float   => "><>",
            Layout::Monocle => "[M]",
            Layout::Columns => "|||",
        }
    }
}

/// Отступ между окнами в Tile (логические пиксели; итоговый зазор между
/// соседними окнами = GAP_INNER, т.к. каждое окно ужимается на GAP_INNER/2).
pub const GAP_INNER: i32 = 16;
/// Отступ от краёв экрана до крайних окон в Tile.
pub const GAP_OUTER: i32 = 18;

/// Сжимает прямоугольник на `inset` с каждой стороны (не уходя в отрицательный размер).
fn inset_rect(rect: Rectangle<i32, Logical>, inset: i32) -> Rectangle<i32, Logical> {
    let w = (rect.size.w - inset * 2).max(1);
    let h = (rect.size.h - inset * 2).max(1);
    Rectangle::new((rect.loc.x + inset, rect.loc.y + inset).into(), (w, h).into())
}

pub struct TileConfig {
    pub nmaster:     usize,
    pub mfact:       f32,
    pub layout:      Layout,
    pub prev_layout: Layout,
}

impl Default for TileConfig {
    fn default() -> Self {
        Self { nmaster: 1, mfact: 0.55, layout: Layout::Tile, prev_layout: Layout::Float }
    }
}

// ── Dwindle: рекурсивный горизонтальный split ─────────────────────────────────
// n=1: [  A  ]
// n=2: [ A ][ B ]   ← горизонтальный split (лево/право)
// n=3: [ A ][ B ]   ← B делится вертикально (верх/низ)
//           [ C ]
// n=4: [ A ][ B ]   ← C делится горизонтально
//           [C][D]
// Чередуем горизонталь/вертикаль на каждом уровне

pub fn dwindle_rects(
    rect: Rectangle<i32, Logical>,
    n: usize,
    split_horizontal: bool, // true = лево/право, false = верх/низ
) -> Vec<Rectangle<i32, Logical>> {
    if n == 0 { return vec![]; }
    if n == 1 { return vec![rect]; }

    let (first, rest) = if split_horizontal {
        // Делим лево/право
        let w_first = (rect.size.w as f32 * 0.5).round() as i32;
        let w_rest  = rect.size.w - w_first;
        let first = Rectangle::new(
            rect.loc,
            (w_first, rect.size.h).into(),
        );
        let rest = Rectangle::new(
            (rect.loc.x + w_first, rect.loc.y).into(),
            (w_rest, rect.size.h).into(),
        );
        (first, rest)
    } else {
        // Делим верх/низ
        let h_first = (rect.size.h as f32 * 0.5).round() as i32;
        let h_rest  = rect.size.h - h_first;
        let first = Rectangle::new(
            rect.loc,
            (rect.size.w, h_first).into(),
        );
        let rest = Rectangle::new(
            (rect.loc.x, rect.loc.y + h_first).into(),
            (rect.size.w, h_rest).into(),
        );
        (first, rest)
    };

    let mut result = vec![first];
    // Следующий уровень — противоположное направление
    result.extend(dwindle_rects(rest, n - 1, !split_horizontal));
    result
}

impl Dawn {
    pub fn arrange(&mut self) {
        // В обзоре столов ленту раскладывает overview.rs — обычный arrange
        // не должен её перетасовывать.
        if self.overview_active {
            return;
        }
        self.request_plane_reset();
        match self.tile_config.layout {
            Layout::Tile    => self.apply_tile_layout(),
            Layout::Monocle => self.apply_monocle_layout(),
            Layout::Columns => self.apply_columns_layout(),
            Layout::Float   => {}
        }
    }

    // apply_columns_layout вынесена в columns.rs (полная niri-модель колонок).

    pub fn apply_tile_layout(&mut self) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };
        // Раскладку строим от НАЧАЛА ХОЛСТА (0,0), а НЕ от output_geometry.loc
        // (= позиция камеры). Иначе тайлинг "уезжает" вслед за камерой; при
        // фиксированном (0,0) камере можно красиво перелетать к нему (set_layout).
        let geo = match self.space.output_geometry(&output) {
            Some(g) => Rectangle::new((0, 0).into(), g.size),
            None => return,
        };

        let current_tags = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows
            .iter()
            .filter(|tw| tw.tags & current_tags != 0 && !tw.floating)
            .map(|tw| tw.window.clone())
            .collect();

        let n = visible.len();
        if n == 0 { return; }

        // Внешний отступ — от краёв экрана до крайних окон.
        let geo = inset_rect(geo, GAP_OUTER);
        // Первый split — горизонтальный (лево/право)
        let rects = dwindle_rects(geo, n, true);
        // Внутренний отступ — между соседними окнами (половина зазора с каждой стороны).
        let rects: Vec<Rectangle<i32, Logical>> = rects.into_iter()
            .map(|r| inset_rect(r, GAP_INNER / 2))
            .collect();

        for (window, rect) in visible.iter().zip(rects.iter()) {
            self.resize_window(window, *rect);
        }
    }

    pub fn apply_monocle_layout(&mut self) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };
        // От (0,0), не от камеры (см. apply_tile_layout).
        let geo = match self.space.output_geometry(&output) {
            Some(g) => Rectangle::new((0, 0).into(), g.size),
            None => return,
        };
        let current_tags = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows
            .iter()
            .filter(|tw| tw.tags & current_tags != 0 && !tw.floating)
            .map(|tw| tw.window.clone())
            .collect();
        let geo = inset_rect(geo, GAP_OUTER);
        for window in &visible {
            self.resize_window(window, geo);
        }
    }

    pub fn resize_window(&mut self, window: &Window, rect: Rectangle<i32, Logical>) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(rect.size);
                state.states.set(xdg_toplevel::State::TiledLeft);
                state.states.set(xdg_toplevel::State::TiledRight);
                state.states.set(xdg_toplevel::State::TiledTop);
                state.states.set(xdg_toplevel::State::TiledBottom);
            });
            toplevel.send_pending_configure();
        }
        // Размер применяется сразу (клиент сам не умеет анимировать resize),
        // а позиция едет плавным LERP — "сборка" в тайлинг (см. anim::tick).
        self.animate_window_to(window, rect.loc);
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
            tw.window.toplevel().zip(window.toplevel())
                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                .unwrap_or(false)
        }) {
            tw.position = rect.loc;
        }
    }

    /// Запускает плавный LERP окна из его текущей позиции в `target` вместо
    /// мгновенного space.map_element — используется при разлёте/сборке
    /// tiling/floating. Заменяет уже идущую анимацию этого окна, если была.
    pub(crate) fn animate_window_to_dur(&mut self, window: &Window, target: Point<i32, Logical>, dur: Duration) {
        let from = self.space.element_geometry(window)
            .map(|g| g.loc.to_f64())
            .unwrap_or(target.to_f64());
        self.window_pos_anims.retain(|(w, _)| {
            w.toplevel().zip(window.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true)
        });
        // Плавный ease-out-cubic без overshoot: пружина (new_spring) визуально
        // "подлагивала" в тайлинге из-за лишних кадров перелёта за цель.
        self.window_pos_anims.push((
            window.clone(),
            CameraAnim::new(from, target.to_f64(), dur),
        ));
    }

    fn animate_window_to(&mut self, window: &Window, target: Point<i32, Logical>) {
        // Сокращено с 240ms→180ms — snappy, без ощущения "подлагивания"
        // при сборке/разлёте окон. Ещё короче — будет заметно глазу.
        self.animate_window_to_dur(window, target, Duration::from_millis(180));
    }

    pub fn set_layout(&mut self, layout: Layout) {
        let prev = self.tile_config.layout;
        self.tile_config.prev_layout = prev;
        self.tile_config.layout = layout;
        if layout == Layout::Float {
            self.scatter_to_float(prev);
        } else {
            // Любой переход в tiling/columns (Win+D/Win+T/Win+N): камера в (0,0),
            // zoom=1, arrange раскладывает окна. Окна плывут от текущей позиции
            // к тайловой через window_pos_anims (animate_window_to, 180ms).
            // Без slide-in (N map_element на кадр + пустой кадр выброса влево).
            self.momentum.stop();
            self.camera_anim = None;
            self.zoom_anim = None;
            self.viewport.zoom = 1.0;
            self.viewport.cam_x = 0.0;
            self.viewport.cam_y = 0.0;
            self.apply_camera();
            if layout == Layout::Columns {
                self.columns_set_active_to_focus();
            }
            self.arrange();
        }
        tracing::info!("dawn: layout → {}", layout.symbol());
    }

    /// Hyprland-style "slide-in": устарел — arrange + animate_window_to (180ms)
    /// дают плавный LERP от текущей позиции к целевой без пустых кадров и
    /// лишних map_element на кадр. Оставлено для обратной совместимости
    /// (не используется).
    #[allow(dead_code)]
    fn slide_in_tiling(&mut self, dur: Duration) {
        let _ = dur;
    }

    /// Разлёт окон при переходе в Float.
    /// Если окно уже размещалось в Float → восстановить позицию и float_size.
    /// Иначе → кольцеобразный разлёт (30-45% от min(w,h)) вокруг центра.
    fn scatter_to_float(&mut self, _prev: Layout) {
        let output = match self.space.outputs().next().cloned() {
            Some(o) => o,
            None => return,
        };
        let output_geo = match self.space.output_geometry(&output) {
            Some(g) => g,
            None => return,
        };
        let current_tags = self.viewport.current_tags();

        // Центр экрана в canvas-координатах
        let cx = self.viewport.cam_x + output_geo.size.w as f64 / 2.0;
        let cy = self.viewport.cam_y + output_geo.size.h as f64 / 2.0;

        let time_seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(42) as u64;

        // Кольцо: 30%–45% от наименьшего измерения экрана
        let min_dim = output_geo.size.w.min(output_geo.size.h) as f64;
        let min_r = min_dim * 0.40;
        let max_r = min_dim * 0.62;
        let tau = std::f64::consts::TAU; // 2π

        let visible_count = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .count();
        // Равномерно делим круг, добавляем случайный jitter
        let angle_step = if visible_count > 0 { tau / visible_count as f64 } else { tau };

        // Собираем данные (borrow checker: space и tagged_windows — разные поля)
        let updates: Vec<(Window, Option<Size<i32, Logical>>, smithay::utils::Point<i32, Logical>)> =
            self.tagged_windows.iter()
                .filter(|tw| tw.tags & current_tags != 0)
                .enumerate()
                .map(|(idx, tw)| {
                    let win_size = self.space.element_geometry(&tw.window)
                        .map(|g| g.size)
                        .unwrap_or((400, 300).into());
                    let pos = if tw.float_position_set {
                        tw.float_position
                    } else {
                        let base_angle = angle_step * idx as f64;
                        // jitter ± 40% от шага угла
                        let jitter = (lcg_f64(time_seed.wrapping_add(idx as u64)) - 0.5)
                            * angle_step * 0.8;
                        let angle = base_angle + jitter;
                        let r = min_r + lcg_f64(time_seed.wrapping_add(idx as u64 + 100))
                            * (max_r - min_r);
                        let x = (cx + angle.cos() * r - win_size.w as f64 / 2.0) as i32;
                        let y = (cy + angle.sin() * r - win_size.h as f64 / 2.0) as i32;
                        smithay::utils::Point::from((x, y))
                    };
                    (tw.window.clone(), tw.float_size, pos)
                })
                .collect();

        // Применяем: убираем tiled-состояния, восстанавливаем float_size
        for (window, float_size, pos) in &updates {
            if let Some(t) = window.toplevel() {
                t.with_pending_state(|s| {
                    s.size = *float_size; // None → клиент выбирает сам
                    s.states.unset(xdg_toplevel::State::TiledLeft);
                    s.states.unset(xdg_toplevel::State::TiledRight);
                    s.states.unset(xdg_toplevel::State::TiledTop);
                    s.states.unset(xdg_toplevel::State::TiledBottom);
                });
                t.send_pending_configure();
            }
            // Плавный "разлёт" в кольцо вместо мгновенного прыжка (см. anim::tick) —
            // подольше и заметнее, чем сборка в тайлинг ("красивая" анимация).
            self.animate_window_to_dur(window, *pos, Duration::from_millis(600));
        }

        // Обновляем tagged_windows
        for (window, _, pos) in &updates {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                tw.window.toplevel().zip(window.toplevel())
                    .map(|(a, b)| a.wl_surface() == b.wl_surface())
                    .unwrap_or(false)
            }) {
                tw.float_position = *pos;
                tw.position = *pos;
                // ВАЖНО: фиксируем позицию как "выбранную" — иначе при каждом
                // следующем переходе в Float scatter_to_float заново
                // рандомизирует угол/радиус и окна разлетаются по-новому.
                // Теперь после первого разлёта окна возвращаются на свои места.
                tw.float_position_set = true;
            }
        }
    }

    /// Подтягивает камеру к сфокусированному `window` при смене фокуса.
    /// Float — центрирует окно; Columns — niri-скролл: выравнивает колонку в
    /// видимую область БЕЗ центрирования (иначе полэкранное окно оставляет
    /// пустоту по бокам), слева направо, cam_x ≥ 0. Tile/Monocle — камера
    /// зафиксирована на (0,0), не трогаем.
    fn snap_camera_to_window(&mut self, window: &Window) {
        let output = match self.space.outputs().next().cloned() {
            Some(o) => o,
            None => return,
        };
        let out_geo = match self.space.output_geometry(&output) {
            Some(g) => g,
            None => return,
        };
        let geo = match self.space.element_geometry(window) {
            Some(g) => g,
            None => return,
        };

        let (to_x, to_y) = match self.tile_config.layout {
            Layout::Float => (
                geo.loc.x as f64 + geo.size.w as f64 / 2.0 - out_geo.size.w as f64 / 2.0,
                geo.loc.y as f64 + geo.size.h as f64 / 2.0 - out_geo.size.h as f64 / 2.0,
            ),
            Layout::Columns => {
                // Выравниваем сфокусированную колонку в кадр, не центрируя.
                let view_w = out_geo.size.w as f64; // zoom=1 в Columns
                let left = geo.loc.x as f64 - GAP_INNER as f64 / 2.0;
                let right = (geo.loc.x + geo.size.w) as f64 + GAP_INNER as f64 / 2.0;
                let mut cam_x = self.viewport.cam_x;
                if right > cam_x + view_w { cam_x = right - view_w; }
                if left < cam_x { cam_x = left; }
                if cam_x < 0.0 { cam_x = 0.0; }
                (cam_x, 0.0) // колонки на всю высоту от y=0
            }
            _ => return,
        };

        let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let to = Point::from((to_x, to_y));
        if (to.x - from.x).abs() > 0.5 || (to.y - from.y).abs() > 0.5 {
            self.camera_anim = Some(CameraAnim::new(from, to, Duration::from_millis(220)));
        }
    }

    pub fn toggle_layout(&mut self) {
        let prev = self.tile_config.prev_layout;
        self.set_layout(prev);
    }

    pub fn inc_nmaster(&mut self, delta: i32) {
        let n = self.tile_config.nmaster as i32 + delta;
        self.tile_config.nmaster = n.max(0) as usize;
        self.arrange();
    }

    pub fn set_mfact(&mut self, delta: f32) {
        let new = (self.tile_config.mfact + delta).clamp(0.1, 0.9);
        self.tile_config.mfact = new;
        self.arrange();
    }

    /// Схлопывание группы окон в стопку (2.4): все видимые окна текущего тега
    /// съезжаются в одну точку с вертикальным сдвигом заголовков на 30px.
    /// Повторный вызов (когда что-то уже схлопнуто) — разворачивает обратно
    /// на сохранённые float_position.
    pub fn toggle_fold_stack(&mut self) {
        if self.tile_config.layout != Layout::Float {
            return;
        }
        let current_tags = self.viewport.current_tags();
        let any_folded = self.tagged_windows.iter()
            .any(|tw| tw.tags & current_tags != 0 && tw.folded);

        if any_folded {
            let restores: Vec<(Window, Point<i32, Logical>)> = self.tagged_windows.iter()
                .filter(|tw| tw.tags & current_tags != 0 && tw.folded)
                .map(|tw| (tw.window.clone(), tw.float_position))
                .collect();
            for (w, pos) in &restores {
                self.space.map_element(w.clone(), *pos, false);
            }
            for tw in self.tagged_windows.iter_mut() {
                if tw.tags & current_tags != 0 {
                    tw.folded = false;
                }
            }
            tracing::info!("dawn: stack unfolded");
        } else {
            const FOLD_OFFSET: i32 = 30;
            let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());

            let visible_idxs: Vec<usize> = self.tagged_windows.iter().enumerate()
                .filter(|(_, tw)| tw.tags & current_tags != 0)
                .map(|(i, _)| i)
                .collect();
            if visible_idxs.len() < 2 {
                return;
            }

            let anchor = focused.as_ref()
                .and_then(|fs| self.tagged_windows.iter().find(|tw| {
                    tw.tags & current_tags != 0
                        && tw.window.toplevel().map(|t| t.wl_surface() == fs).unwrap_or(false)
                }))
                .or_else(|| self.tagged_windows.get(visible_idxs[0]))
                .and_then(|tw| self.space.element_geometry(&tw.window).map(|g| g.loc))
                .unwrap_or((0, 0).into());

            let updates: Vec<(Window, Point<i32, Logical>)> = visible_idxs.iter().enumerate()
                .map(|(stack_i, &idx)| {
                    let w = self.tagged_windows[idx].window.clone();
                    let pos = Point::from((anchor.x, anchor.y + stack_i as i32 * FOLD_OFFSET));
                    (w, pos)
                })
                .collect();
            for (w, pos) in &updates {
                self.space.map_element(w.clone(), *pos, true);
            }
            for &idx in &visible_idxs {
                self.tagged_windows[idx].folded = true;
            }
            tracing::info!("dawn: stack folded ({} windows)", visible_idxs.len());
        }
        self.request_plane_reset();
        self.request_redraw();
    }

    pub fn toggle_floating(&mut self) {
        if let Some(focused) = self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                tw.window.toplevel()
                    .map(|t| t.wl_surface() == &focused)
                    .unwrap_or(false)
            }) {
                tw.floating = !tw.floating;
            }
        }
        self.arrange();
    }

    /// Пространственная навигация: фокус на ближайшее окно в заданном направлении.
    /// dx: -1=влево, +1=вправо; dy: -1=вверх, +1=вниз
    pub fn focus_direction(&mut self, dx: i32, dy: i32) {
        let current_tags = self.viewport.current_tags();
        let focused_surface = self.seat.get_keyboard().and_then(|kb| kb.current_focus());

        struct WinPos { window: Window, cx: f64, cy: f64, focused: bool }

        let wins: Vec<WinPos> = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .filter_map(|tw| {
                self.space.element_geometry(&tw.window).map(|g| {
                    let cx = g.loc.x as f64 + g.size.w as f64 / 2.0;
                    let cy = g.loc.y as f64 + g.size.h as f64 / 2.0;
                    let focused = focused_surface.as_ref()
                        .zip(tw.window.toplevel())
                        .map(|(fs, t)| t.wl_surface() == fs)
                        .unwrap_or(false);
                    WinPos { window: tw.window.clone(), cx, cy, focused }
                })
            })
            .collect();

        let focused_win = wins.iter().find(|w| w.focused);
        let (fx, fy) = match focused_win {
            Some(w) => (w.cx, w.cy),
            None => { self.focus_stack(1); return; }
        };

        let dir_x = dx as f64;
        let dir_y = dy as f64;

        // score = проекция на направление / расстояние → ближайшее в нужном направлении
        let best = wins.iter()
            .filter(|w| !w.focused)
            .filter_map(|w| {
                let ddx = w.cx - fx;
                let ddy = w.cy - fy;
                let dot = ddx * dir_x + ddy * dir_y;
                let dist = (ddx * ddx + ddy * ddy).sqrt();
                if dot > 0.0 && dist > 1.0 { Some((w.window.clone(), dot / dist)) } else { None }
            })
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(w, _)| w);

        let next = match best {
            Some(w) => w,
            None => return, // нет окон в этом направлении
        };

        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        self.space.raise_element(&next, true);
        next.set_activated(true);
        for w in self.space.elements() {
            if w.toplevel().zip(next.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true)
            {
                w.set_activated(false);
                if let Some(t) = w.toplevel() { t.send_pending_configure(); }
            }
        }
        if let Some(t) = next.toplevel() {
            self.seat.get_keyboard().unwrap()
                .set_focus(self, Some(t.wl_surface().clone()), serial);
            t.send_pending_configure();
        }

        // Умное центрирование камеры (1.2)
        self.snap_camera_to_window(&next);
    }

    pub fn focus_stack(&mut self, direction: i32) {
        let current_tags = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows
            .iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .map(|tw| tw.window.clone())
            .collect();
        if visible.is_empty() { return; }

        let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        let current_idx = focused.as_ref().and_then(|fs| {
            visible.iter().position(|w| {
                w.toplevel().map(|t| t.wl_surface() == fs).unwrap_or(false)
            })
        });
        let next_idx = match current_idx {
            Some(idx) => if direction > 0 { (idx + 1) % visible.len() }
                         else { (idx + visible.len() - 1) % visible.len() },
            None => 0,
        };
        let next = &visible[next_idx];
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        self.space.raise_element(next, true);
        next.set_activated(true);
        for w in self.space.elements() {
            if w.toplevel().zip(next.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true)
            {
                w.set_activated(false);
                if let Some(t) = w.toplevel() { t.send_pending_configure(); }
            }
        }
        if let Some(t) = next.toplevel() {
            self.seat.get_keyboard().unwrap()
                .set_focus(self, Some(t.wl_surface().clone()), serial);
            t.send_pending_configure();
        }

        // Умное центрирование камеры (1.2)
        self.snap_camera_to_window(next);
    }

    pub fn zoom(&mut self) {
        let current_tags = self.viewport.current_tags();
        let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        if let Some(fs) = focused {
            let idx = self.tagged_windows.iter().position(|tw| {
                tw.tags & current_tags != 0 && !tw.floating
                    && tw.window.toplevel().map(|t| t.wl_surface() == &fs).unwrap_or(false)
            });
            if let Some(idx) = idx {
                if idx != 0 {
                    self.tagged_windows.swap(0, idx);
                    self.arrange();
                }
            }
        }
    }

    /// Переместить сфокусированное окно на (dx, dy) пикселей (Float-режим)
    pub fn move_focused_window(&mut self, dx: i32, dy: i32) {
        let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        let fs = match focused { Some(f) => f, None => return };
        let w = match self.space.elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == &fs).unwrap_or(false))
            .cloned()
        {
            Some(w) => w, None => return,
        };
        let loc = match self.space.element_location(&w) { Some(l) => l, None => return };
        let new_loc = (loc.x + dx, loc.y + dy).into();
        self.space.map_element(w.clone(), new_loc, false);
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
            tw.window.toplevel().zip(w.toplevel())
                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                .unwrap_or(false)
        }) {
            tw.position = new_loc;
            tw.float_position = new_loc;
            tw.float_position_set = true;
        }
    }

    /// Перемещение окна в тайлинге (Hyprland movewindow): меняет местами
    /// сфокусированное окно с соседним в порядке dwindle по направлению
    /// (dx>0/dy>0 → следующее, иначе предыдущее) и перераскладывает.
    pub fn move_tiled_window(&mut self, dx: i32, dy: i32) {
        let current = self.viewport.current_tags();
        // Индексы видимых тайловых окон в tagged_windows (порядок = dwindle).
        let visible: Vec<usize> = self.tagged_windows.iter().enumerate()
            .filter(|(_, tw)| tw.tags & current != 0 && !tw.floating)
            .map(|(i, _)| i)
            .collect();
        if visible.len() < 2 {
            return;
        }
        let focused = match self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            Some(f) => f,
            None => return,
        };
        let cur = match visible.iter().position(|&i| {
            self.tagged_windows[i].window.toplevel()
                .map(|t| t.wl_surface() == &focused).unwrap_or(false)
        }) {
            Some(c) => c,
            None => return,
        };
        let dir = if dx > 0 || dy > 0 { 1i32 } else { -1 };
        let next = cur as i32 + dir;
        if next < 0 || next >= visible.len() as i32 {
            return;
        }
        self.tagged_windows.swap(visible[cur], visible[next as usize]);
        self.arrange();
        self.request_redraw();
    }

    /// Изменить размер сфокусированного окна на (dw, dh) пикселей (Float-режим)
    pub fn keyboard_resize_focused(&mut self, dw: i32, dh: i32) {
        let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        let fs = match focused { Some(f) => f, None => return };
        let w = match self.space.elements()
            .find(|w| w.toplevel().map(|t| t.wl_surface() == &fs).unwrap_or(false))
            .cloned()
        {
            Some(w) => w, None => return,
        };
        if let Some(t) = w.toplevel() {
            let cur = t.with_committed_state(|s| s.and_then(|s| s.size)).unwrap_or((200, 200).into());
            let new_w = (cur.w + dw).max(50);
            let new_h = (cur.h + dh).max(50);
            t.with_pending_state(|s| { s.size = Some((new_w, new_h).into()); });
            t.send_pending_configure();
        }
    }
}
