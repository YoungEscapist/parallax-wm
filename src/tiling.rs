use std::time::Duration;

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::anim::{CameraAnim, PosAnim};
use crate::state::Dawn;

/// Псевдослучайный угол из seed (LCG)
fn lcg_f64(seed: u64) -> f64 {
    let x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (x >> 33) as f64 / (u32::MAX as f64)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layout {
    Tile,    // dwindle как в Hyprland: BSP-дерево, см. dwindle.rs
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
    /// dwm-наследие: в Tile не используется (в dwindle нет мастер-области),
    /// осталось для Monocle/Columns.
    pub nmaster:     usize,
    /// То же: в Tile ширину задают split_ratio узлов дерева, а не общий фактор.
    pub mfact:       f32,
    pub layout:      Layout,
    pub prev_layout: Layout,
}

impl Default for TileConfig {
    fn default() -> Self {
        Self { nmaster: 1, mfact: 0.55, layout: Layout::Tile, prev_layout: Layout::Float }
    }
}

// ── Dwindle-цепочка: рекурсивный split по счётчику окон ───────────────────────
// Раскладка Layout::Tile этим БОЛЬШЕ НЕ пользуется — там настоящее BSP-дерево
// (dwindle.rs). Осталось для обзора столов (overview.rs), где нужна разумная
// сетка миниатюр для стола, у которого своего дерева ещё нет.
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

    /// Рабочая область тайлинга: экран от НАЧАЛА ХОЛСТА (0,0), а НЕ от
    /// output_geometry.loc (= позиция камеры). Иначе тайлинг "уезжает" вслед за
    /// камерой; при фиксированном (0,0) камере можно красиво перелетать к нему
    /// (см. set_layout). Внешний отступ GAP_OUTER уже вычтен.
    pub fn tile_work_area(&self) -> Option<Rectangle<i32, Logical>> {
        let output = self.space.outputs().next()?;
        let geo = self.space.output_geometry(output)?;
        Some(inset_rect(Rectangle::new((0, 0).into(), geo.size), GAP_OUTER))
    }

    /// Окно, за которым сейчас клавиатурный фокус.
    pub(crate) fn focused_window(&self) -> Option<Window> {
        let fs = self.focused_surface()?;
        self.tagged_windows.iter()
            .find(|tw| crate::xwin::is_surface(&tw.window, &fs))
            .map(|tw| tw.window.clone())
    }

    /// Приводит BSP-дерево текущего набора тегов в соответствие со списком
    /// видимых тайловых окон: исчезнувшие удаляются (их место забирает сосед по
    /// узлу), новые ВСТАВЛЯЮТСЯ ДЕЛЕНИЕМ слота — как в Hyprland.
    ///
    /// Одно новое окно за проход = интерактивное открытие: делим слот
    /// СФОКУСИРОВАННОГО окна, сторону выбирает курсор. Несколько сразу
    /// (сборка из Float, возврат на тег) — каждое садится к ближайшему по
    /// своей текущей позиции соседу, чтобы раскладка примерно повторила то,
    /// как окна лежали на холсте.
    fn sync_dwindle_tree(&mut self, area: Rectangle<i32, Logical>) {
        let current_tags = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows
            .iter()
            .filter(|tw| tw.tags & current_tags != 0 && !tw.floating)
            .map(|tw| tw.window.clone())
            .collect();

        if visible.is_empty() {
            self.dwindle_trees.remove(&current_tags);
            return;
        }

        let cfg = self.lua_config.dwindle;
        let focused = self.focused_window();
        let mouse = self.pointer_location;

        // Центры окон нужны для «сборки»; берём до захвата дерева (borrowck).
        let centers: Vec<(Window, Point<f64, Logical>)> = visible.iter()
            .map(|w| {
                let c = self.space.element_geometry(w)
                    .map(|g| Point::from((
                        g.loc.x as f64 + g.size.w as f64 / 2.0,
                        g.loc.y as f64 + g.size.h as f64 / 2.0,
                    )))
                    .unwrap_or(mouse);
                (w.clone(), c)
            })
            .collect();

        let tree = self.dwindle_trees.entry(current_tags).or_default();

        for w in tree.windows() {
            if !visible.iter().any(|v| crate::dwindle::same_window(v, &w)) {
                tree.remove(&w);
            }
        }

        let missing: Vec<(Window, Point<f64, Logical>)> = centers.into_iter()
            .filter(|(w, _)| !tree.contains(w))
            .collect();

        if missing.len() == 1 {
            tree.recalc(area, &cfg);
            let (w, _) = &missing[0];
            let opening_on = focused
                .as_ref()
                .filter(|f| !crate::dwindle::same_window(f, w))
                .and_then(|f| tree.node_of(f))
                .or_else(|| tree.closest_node(mouse, Some(w)));
            tree.insert(w.clone(), opening_on, mouse, area, &cfg, None);
        } else {
            for (w, center) in missing {
                tree.recalc(area, &cfg);
                let opening_on = tree.closest_node(center, Some(&w));
                tree.insert(w, opening_on, center, area, &cfg, None);
            }
        }
        tree.recalc(area, &cfg);
    }

    pub fn apply_tile_layout(&mut self) {
        self.apply_tile_layout_animated(true);
    }

    /// `animate = false` — окна встают на место кадром, без 180ms LERP: нужно
    /// на живом ресайзе (Super+ПКМ), иначе раскладка тянется за курсором.
    pub fn apply_tile_layout_animated(&mut self, animate: bool) {
        let area = match self.tile_work_area() {
            Some(a) => a,
            None => return,
        };
        self.sync_dwindle_tree(area);

        let current_tags = self.viewport.current_tags();
        let rects = match self.dwindle_trees.get(&current_tags) {
            // Внутренний отступ — между соседями (половина зазора с каждой стороны).
            Some(tree) => tree.leaf_rects().into_iter()
                .map(|(w, r)| (w, inset_rect(r, GAP_INNER / 2)))
                .collect::<Vec<_>>(),
            None => return,
        };
        for (window, rect) in rects {
            self.resize_window_animated(&window, rect, animate);
        }
    }

    /// Размер, который получит следующее окно в Tile — чтобы отдать его
    /// клиенту сразу в первом configure и не слать второй после arrange
    /// (Hyprland: predictSizeForNewTarget). Делим слот того окна, чей слот и
    /// будет разделён (сфокусированного), по тем же правилам.
    pub fn predict_tile_size(&self) -> Option<Size<i32, Logical>> {
        let area = self.tile_work_area()?;
        let inner = GAP_INNER / 2;
        let tags = self.viewport.current_tags();
        let tree = self.dwindle_trees.get(&tags);
        let cfg = self.lua_config.dwindle;

        let (w, h) = match (tree, self.focused_window()) {
            (Some(tree), Some(focused)) => tree
                .predict_split_size(&focused, &cfg)
                .or_else(|| {
                    // Фокуса в дереве нет (только что закрыли окно и т.п.) —
                    // берём любой лист, лишь бы масштаб был правдоподобным.
                    tree.leaf_rects().first().map(|(_, r)| {
                        let b = crate::dwindle::FBox::from_rect(*r);
                        if b.h * cfg.split_width_multiplier as f64 > b.w {
                            (b.w, b.h / 2.0)
                        } else {
                            (b.w / 2.0, b.h)
                        }
                    })
                })
                .unwrap_or((area.size.w as f64, area.size.h as f64)),
            _ => (area.size.w as f64, area.size.h as f64),
        };
        Some(Size::from((
            (w.round() as i32 - inner * 2).max(1),
            (h.round() as i32 - inner * 2).max(1),
        )))
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
        self.resize_window_animated(window, rect, true);
    }

    pub fn resize_window_animated(
        &mut self,
        window: &Window,
        rect: Rectangle<i32, Logical>,
        animate: bool,
    ) {
        crate::xwin::set_size(window, Some(rect.size), crate::xwin::Tiled::Set);
        crate::xwin::configure(window);
        // Размер применяется сразу (клиент сам не умеет анимировать resize),
        // а позиция едет плавным LERP — "сборка" в тайлинг (см. anim::tick).
        if animate {
            self.animate_window_to(window, rect.loc);
        } else {
            // Живой ресайз: никакого LERP — снимаем идущую анимацию позиции,
            // иначе она перебьёт map_element на следующем тике.
            self.window_pos_anims.retain(|(w, _)| {
                w != window
            });
            self.space.map_element(window.clone(), rect.loc, false);
        }
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
            &tw.window == window
        }) {
            tw.position = rect.loc;
        }
    }

    /// Запускает плавный LERP окна из его текущей позиции в `target` вместо
    /// мгновенного space.map_element — используется при разлёте/сборке
    /// tiling/floating. Заменяет уже идущую анимацию этого окна, если была.
    pub(crate) fn animate_window_to_dur(&mut self, window: &Window, target: Point<i32, Logical>, dur: Duration) {
        let target_f = target.to_f64();
        if let Some((_, anim)) = self.window_pos_anims.iter_mut()
            .find(|(w, _)| crate::dwindle::same_window(w, window))
        {
            // Окно уже летит — МЕНЯЕМ ЦЕЛЬ, а не заводим анимацию заново.
            // Раньше здесь была пересборка с нуля, и при частых вызовах
            // (arrange() на каждый свап во время драга, толчок соседей на
            // каждый кадр коллизии) ease перезапускался каждые 16мс: окно
            // теряло скорость, ползло по асимптоте и «не доезжало» — это и
            // выглядело как баганная анимация переноса.
            anim.retarget(target_f, dur);
            return;
        }
        let from = self.space.element_geometry(window)
            .map(|g| g.loc.to_f64())
            .unwrap_or(target_f);
        // Уже на месте — незачем заводить анимацию (и жечь кадры на неё).
        if (from.x - target_f.x).abs() < 0.5 && (from.y - target_f.y).abs() < 0.5 {
            return;
        }
        self.window_pos_anims.push((window.clone(), PosAnim::new(from, target_f, dur)));
    }

    /// Инерционный доезд окна после броска мышью: пружина стартует С ТОЙ ЖЕ
    /// скоростью, что была у курсора, и тормозит экспоненциально. Возвращает
    /// точку, где окно остановится (её надо записать в float_position).
    ///
    /// `omega` — жёсткость торможения (1/сек): путь доезда = |v|/ω.
    pub(crate) fn fling_window(
        &mut self,
        window: &Window,
        vel: Point<f64, Logical>,
        omega: f64,
    ) -> Point<i32, Logical> {
        let from = self.space.element_geometry(window)
            .map(|g| g.loc.to_f64())
            .unwrap_or_default();
        let target = Point::from((from.x + vel.x / omega, from.y + vel.y / omega));
        self.window_pos_anims.retain(|(w, _)| !crate::dwindle::same_window(w, window));
        self.window_pos_anims.push((
            window.clone(),
            PosAnim::with_velocity(from, target, vel, omega),
        ));
        target.to_i32_round()
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
            // Возврат из тайлинга — камера туда же, где холст был до входа.
            // Вход в тайлинг насильно ставит камеру в (0,0) при zoom=1 (иначе
            // разложенные по экрану окна оказались бы за кадром), и без этого
            // снимка выход во Float каждый раз выбрасывал в начало координат:
            // окна, к которым пользователь долго ехал по холсту, оставались
            // где-то далеко, а «привычное место» терялось.
            if let Some((cam_x, cam_y, zoom)) = self.pre_tiling_view.take() {
                self.momentum.stop();
                self.camera_anim = None;
                self.zoom_anim = None;
                self.viewport.zoom = zoom;
                self.viewport.cam_x = cam_x;
                self.viewport.cam_y = cam_y;
                self.apply_camera();
            }
            // Строго ПОСЛЕ восстановления камеры: разлёт считает кольцо от
            // центра ЭКРАНА, то есть от текущего положения камеры.
            self.scatter_to_float(prev);
        } else {
            // Снимок берём только на первом входе Float→tiling: переходы
            // между тайловыми раскладками (Tile→Columns→Monocle) идут уже с
            // камерой (0,0) и затёрли бы запомненное место.
            if prev == Layout::Float {
                self.pre_tiling_view = Some((
                    self.viewport.cam_x,
                    self.viewport.cam_y,
                    self.viewport.zoom,
                ));
            }
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
            // float_size = None → размер выбирает клиент (у X11 в этом
            // случае остаётся текущий).
            crate::xwin::set_size(window, *float_size, crate::xwin::Tiled::Unset);
            crate::xwin::configure(window);
            // Плавный "разлёт" в кольцо вместо мгновенного прыжка (см. anim::tick) —
            // подольше и заметнее, чем сборка в тайлинг ("красивая" анимация).
            self.animate_window_to_dur(window, *pos, Duration::from_millis(600));
        }

        // Обновляем tagged_windows
        for (window, _, pos) in &updates {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                &tw.window == window
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
        // В Tile (dwindle) master-области нет: узлы делятся пополам, а не
        // "N окон в мастере". Ближайший по смыслу аналог из Hyprland —
        // togglesplit/swapsplit ближайшего деления (dwindle:layoutmsg).
        if self.tile_config.layout == Layout::Tile {
            if delta > 0 { self.toggle_split_focused(); } else { self.swap_split_focused(); }
            return;
        }
        let n = self.tile_config.nmaster as i32 + delta;
        self.tile_config.nmaster = n.max(0) as usize;
        self.arrange();
    }

    pub fn set_mfact(&mut self, delta: f32) {
        // Tile (dwindle): двигаем ближайшее деление — hyprctl dispatch
        // layoutmsg splitratio ±. Глобального mfact в dwindle не существует.
        if self.tile_config.layout == Layout::Tile {
            let (Some(window), Some(area)) = (self.focused_window(), self.tile_work_area())
                else { return };
            let cfg = self.lua_config.dwindle;
            let tags = self.viewport.current_tags();
            if let Some(tree) = self.dwindle_trees.get_mut(&tags) {
                if tree.split_ratio_delta(&window, delta * 2.0) {
                    tree.recalc(area, &cfg);
                }
            }
            self.arrange(); // через arrange: он один знает про обзор столов
            self.request_redraw();
            return;
        }
        let new = (self.tile_config.mfact + delta).clamp(0.1, 0.9);
        self.tile_config.mfact = new;
        self.arrange();
    }

    /// Перевернуть ось ближайшего деления (Hyprland: layoutmsg togglesplit).
    /// Живёт только при `dwindle{ preserve_split = true }` — иначе следующий
    /// пересчёт снова возьмёт ось из пропорций слота.
    pub fn toggle_split_focused(&mut self) {
        let (Some(window), Some(area)) = (self.focused_window(), self.tile_work_area()) else { return };
        let cfg = self.lua_config.dwindle;
        let tags = self.viewport.current_tags();
        if let Some(tree) = self.dwindle_trees.get_mut(&tags) {
            if tree.toggle_split(&window) {
                tree.recalc(area, &cfg);
            }
        }
        self.arrange(); // через arrange: он один знает про обзор столов
        self.request_redraw();
    }

    /// Поменять половины деления местами (Hyprland: layoutmsg swapsplit).
    pub fn swap_split_focused(&mut self) {
        let (Some(window), Some(area)) = (self.focused_window(), self.tile_work_area()) else { return };
        let cfg = self.lua_config.dwindle;
        let tags = self.viewport.current_tags();
        if let Some(tree) = self.dwindle_trees.get_mut(&tags) {
            if tree.swap_split(&window) {
                tree.recalc(area, &cfg);
            }
        }
        self.arrange(); // через arrange: он один знает про обзор столов
        self.request_redraw();
    }

    /// Живой ресайз тайлового окна мышью (Super+ПКМ): `delta` — ИНКРЕМЕНТ с
    /// прошлого события, меняет split_ratio предков (Hyprland smart_resizing).
    pub fn dwindle_resize_focused(
        &mut self,
        window: &Window,
        delta: Point<f64, Logical>,
        corner: crate::dwindle::Corner,
    ) {
        let Some(area) = self.tile_work_area() else { return };
        let cfg = self.lua_config.dwindle;
        let tags = self.viewport.current_tags();
        if let Some(tree) = self.dwindle_trees.get_mut(&tags) {
            tree.resize(window, delta, corner, area, &cfg);
        }
        self.apply_tile_layout_animated(false);
        self.request_redraw();
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
            let focused = self.focused_surface();

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
                        && crate::xwin::is_surface(&tw.window, &fs)
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
        if let Some(focused) = self.focused_surface() {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                crate::xwin::is_surface(&tw.window, &focused)
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
        let focused_surface = self.focused_surface();

        struct WinPos { window: Window, cx: f64, cy: f64, focused: bool }

        let wins: Vec<WinPos> = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .filter_map(|tw| {
                self.space.element_geometry(&tw.window).map(|g| {
                    let cx = g.loc.x as f64 + g.size.w as f64 / 2.0;
                    let cy = g.loc.y as f64 + g.size.h as f64 / 2.0;
                    let focused = focused_surface.as_ref()
                        .map(|fs| crate::xwin::is_surface(&tw.window, fs))
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

        crate::xwin::focus(self, &next.clone());

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

        let focused = self.focused_surface();
        let current_idx = focused.as_ref().and_then(|fs| {
            visible.iter().position(|w| {
                crate::xwin::is_surface(w, &fs)
            })
        });
        let next_idx = match current_idx {
            Some(idx) => if direction > 0 { (idx + 1) % visible.len() }
                         else { (idx + visible.len() - 1) % visible.len() },
            None => 0,
        };
        let next = &visible[next_idx];
        crate::xwin::focus(self, &next.clone());

        // Умное центрирование камеры (1.2)
        self.snap_camera_to_window(next);
    }

    /// dwm-шный zoom. В Tile (dwindle) master-слота нет, поэтому делаем то же,
    /// что Hyprland'овский `layoutmsg movetoroot`: поднимаем окно на верхний
    /// уровень дерева — оно занимает половину экрана целиком.
    pub fn zoom(&mut self) {
        if self.tile_config.layout == Layout::Tile {
            let (Some(window), Some(area)) = (self.focused_window(), self.tile_work_area())
                else { return };
            let cfg = self.lua_config.dwindle;
            let tags = self.viewport.current_tags();
            if let Some(tree) = self.dwindle_trees.get_mut(&tags) {
                if tree.move_to_root(&window, true) {
                    tree.recalc(area, &cfg);
                }
            }
            self.arrange(); // через arrange: он один знает про обзор столов
            self.request_redraw();
            return;
        }
        let current_tags = self.viewport.current_tags();
        let focused = self.focused_surface();
        if let Some(fs) = focused {
            let idx = self.tagged_windows.iter().position(|tw| {
                tw.tags & current_tags != 0 && !tw.floating
                    && crate::xwin::is_surface(&tw.window, &fs)
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
        let focused = self.focused_surface();
        let fs = match focused { Some(f) => f, None => return };
        let w = match self.space.elements()
            .find(|w| crate::xwin::is_surface(w, &fs))
            .cloned()
        {
            Some(w) => w, None => return,
        };
        let loc = match self.space.element_location(&w) { Some(l) => l, None => return };
        let new_loc = (loc.x + dx, loc.y + dy).into();
        self.space.map_element(w.clone(), new_loc, false);
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
            tw.window == w
        }) {
            tw.position = new_loc;
            tw.float_position = new_loc;
            tw.float_position_set = true;
        }
    }

    /// Меняет местами два тайловых окна — общая точка для перетаскивания мышью
    /// (Super+ЛКМ, см. grabs/move_grab.rs).
    ///
    /// Раскладку в Tile определяет BSP-дерево (dwindle.rs), в Columns —
    /// структура колонок (columns.rs); порядок в `tagged_windows` там ни на что
    /// не влияет. Поэтому свап одного лишь списка (как было раньше) выглядел
    /// как "перетаскивание не работает": arrange() возвращал окна на прежние
    /// места. Меняем местами именно в структуре активной раскладки.
    ///
    /// Возвращает true, если раскладка изменилась (вызывающему нужен arrange).
    pub fn swap_tiled_windows(&mut self, a: &Window, b: &Window) -> bool {
        use crate::dwindle::same_window;
        if same_window(a, b) {
            return false;
        }
        match self.tile_config.layout {
            Layout::Float => false,
            Layout::Tile => {
                let tags = self.viewport.current_tags();
                let Some(tree) = self.dwindle_trees.get_mut(&tags) else { return false };
                if tree.node_of(a).is_none() || tree.node_of(b).is_none() {
                    return false;
                }
                // Геометрию слотов пересчитает arrange (sync_dwindle_tree → recalc).
                tree.swap(a, b);
                true
            }
            Layout::Columns => {
                self.columns_reconcile();
                let pos = |cols: &crate::columns::ColumnLayout, w: &Window| {
                    cols.columns.iter().enumerate().find_map(|(ci, c)| {
                        c.windows.iter().position(|x| same_window(x, w)).map(|ri| (ci, ri))
                    })
                };
                let (Some((ca, ra)), Some((cb, rb))) =
                    (pos(&self.columns, a), pos(&self.columns, b)) else { return false };
                let wa = self.columns.columns[ca].windows[ra].clone();
                let wb = self.columns.columns[cb].windows[rb].clone();
                self.columns.columns[ca].windows[ra] = wb;
                self.columns.columns[cb].windows[rb] = wa;
                // Активным делаем слот, в который уехало перетаскиваемое окно —
                // иначе фокус колонок разъезжается с тем, что видит мышь.
                self.columns.active = cb;
                self.columns.columns[cb].active_row = rb;
                true
            }
            Layout::Monocle => {
                // Монокль: геометрия одна на всех, важен только порядок стопки —
                // тут свап tagged_windows как раз и есть раскладка.
                let tags = self.viewport.current_tags();
                let idx = |data: &Self, w: &Window| {
                    data.tagged_windows.iter().position(|tw| {
                        tw.tags & tags != 0 && same_window(&tw.window, w)
                    })
                };
                let (Some(ia), Some(ib)) = (idx(self, a), idx(self, b)) else { return false };
                self.tagged_windows.swap(ia, ib);
                true
            }
        }
    }

    /// Перемещение окна в тайлинге (Hyprland movewindow).
    ///
    /// Tile: окно ВЫНИМАЕТСЯ из BSP-дерева и вставляется рядом с соседом за
    /// соответствующим краем — ровно как moveTargetInDirection в Hyprland
    /// (а не свап по номеру в списке: в дереве "следующего окна" нет).
    /// Monocle: сохраняем старый свап по порядку — там геометрия одна на всех,
    /// меняется только порядок в стопке.
    pub fn move_tiled_window(&mut self, dx: i32, dy: i32) {
        if self.tile_config.layout == Layout::Tile {
            let (Some(window), Some(area)) = (self.focused_window(), self.tile_work_area())
                else { return };
            let cfg = self.lua_config.dwindle;
            let tags = self.viewport.current_tags();
            let moved = self.dwindle_trees.get_mut(&tags)
                .map(|tree| tree.move_in_direction(&window, dx, dy, area, &cfg))
                .unwrap_or(false);
            if moved {
                self.arrange(); // через arrange: он один знает про обзор столов
                self.request_redraw();
            }
            return;
        }
        let current = self.viewport.current_tags();
        // Индексы видимых тайловых окон в tagged_windows (порядок = dwindle).
        let visible: Vec<usize> = self.tagged_windows.iter().enumerate()
            .filter(|(_, tw)| tw.tags & current != 0 && !tw.floating)
            .map(|(i, _)| i)
            .collect();
        if visible.len() < 2 {
            return;
        }
        let focused = match self.focused_surface() {
            Some(f) => f,
            None => return,
        };
        let cur = match visible.iter().position(|&i| {
            crate::xwin::is_surface(&self.tagged_windows[i].window, &focused)
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
        let focused = self.focused_surface();
        let fs = match focused { Some(f) => f, None => return };
        let w = match self.space.elements()
            .find(|w| crate::xwin::is_surface(w, &fs))
            .cloned()
        {
            Some(w) => w, None => return,
        };
        let cur = crate::xwin::current_size(&w);
        let new_w = (cur.w + dw).max(50);
        let new_h = (cur.h + dh).max(50);
        crate::xwin::set_size(&w, Some((new_w, new_h).into()), crate::xwin::Tiled::Keep);
        crate::xwin::configure(&w);
    }
}
