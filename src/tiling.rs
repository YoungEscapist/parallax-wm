use std::time::Duration;

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::anim::{CameraAnim, PosAnim};
use crate::state::Parallax;

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
/// Сколько места сверху занимает бар рабочих столов (высота + отступ). Под ним
/// плавающие окна не размещаем — иначе их заголовок оказывается под баром.
/// Считается от самой панели (см. udev::BAR_H): поменяли её — резерв поехал
/// следом, а не остался прежним литералом.
pub const BAR_RESERVED: i32 = crate::udev::BAR_TOP + crate::udev::BAR_H + 10;

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

impl Parallax {
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
        Some(inset_rect(self.screen_area()?, GAP_OUTER))
    }

    /// Экран как прямоугольник холста от (0,0), размером С САМ ЭКРАН и
    /// НЕЗАВИСИМО ОТ ЗУМА.
    ///
    /// Берём режим монитора, а не space.output_geometry: последний делится на
    /// зум (так камера показывает больше холста при отдалении), и раскладка,
    /// посчитанная при зуме 0.45, растягивалась на 5689×2400 — окна улетали за
    /// правый край экрана и уже не возвращались (см. exit_overview_immediate).
    /// Тайлинг обязан считать по экрану: в него он и попадает.
    /// Прямоугольник холста, в котором собираются столы АКТИВНОГО монитора.
    ///
    /// В parallax все рабочие столы лежат в ОДНОМ прямоугольнике (см. `слайд_столов`
    /// в state.rs): между столами нет пространственного отношения, их разводят
    /// теги. Двум мониторам одного прямоугольника мало — каждый получает свой,
    /// начинающийся с его «дома» (`monitors::ШАГ_ДОМА`). Отсюда и берётся то,
    /// что монитор физически не может нарисовать чужие окна: отсечение в
    /// `udev::собрать_элементы` идёт по видимой части холста, а до чужого дома
    /// миллион пикселей.
    ///
    /// Раньше здесь стоял «первый выход, прямоугольник от (0,0)» — с двумя
    /// мониторами это значило, что тайлинг на втором раскладывает окна поверх
    /// окон первого.
    pub(crate) fn screen_area(&self) -> Option<Rectangle<i32, Logical>> {
        if let Some(m) = self.монитор() {
            return Some(Rectangle::new(m.дом, m.размер));
        }
        let output = self.space.outputs().next()?;
        let size = match output.current_mode() {
            Some(m) => Size::from((m.size.w, m.size.h)),
            None => self.space.output_geometry(output)?.size,
        };
        Some(Rectangle::new((0, 0).into(), size))
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

        // Минимальные размеры клиентов тайлинг НЕ учитывает: окно получает
        // ровно свой слот, каким бы маленьким тот ни был. Дерево такое умеет
        // (см. DwindleTree::set_min_sizes и demands), но мы ему минимумов не
        // сообщаем — у листьев они остаются нулевыми.
        //
        // Цена решения известна и выбрана осознанно: клиент, который ужиматься
        // не умеет (Discord — 940×500), нарисуется больше слота и накроет
        // соседа, как это было до правок 11–12.08.2026.
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
            // Делим слот сфокусированного окна; сторону выбирает курсор.
            let opening_on = focused
                .as_ref()
                .filter(|f| !crate::dwindle::same_window(f, w))
                .and_then(|f| tree.node_of(f))
                .or_else(|| tree.closest_node(mouse, Some(w)));
            tree.insert(w.clone(), opening_on, mouse, area, &cfg, None);
        } else {
            // «Сборка» (Win+D, возврат на тег): окна приходят пачкой и каждое
            // садится к ближайшему по своей позиции соседу.
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
        // От (0,0), не от камеры, и по РАЗМЕРУ ЭКРАНА, а не output_geometry
        // (тот делится на зум — см. screen_area).
        let geo = match self.screen_area() {
            Some(g) => g,
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

    /// Приводит слот к тому, что клиент СОГЛАСЕН принять — теперь только
    /// сверху: остаётся максимум (окно, которое не умеет растягиваться,
    /// центрируется в слоте и не вылезает за рабочую область).
    ///
    /// Минимума здесь больше нет: тайлинг ужимает окно до любого размера,
    /// даже если клиент просил больше (Discord — 940×500, замер 11.08.2026).
    /// Такой клиент просто нарисуется поверх соседа — сознательный размен.
    fn fit_to_constraints(
        &self,
        window: &Window,
        slot: Rectangle<i32, Logical>,
    ) -> Rectangle<i32, Logical> {
        let (_, max) = crate::xwin::size_constraints(window);
        // Ноль в max — «без ограничения» (так это устроено и в xdg_shell, и в
        // приведённых к нему подсказках X11; см. xwin::size_constraints).
        let max_w = if max.w <= 0 { i32::MAX } else { max.w };
        let max_h = if max.h <= 0 { i32::MAX } else { max.h };
        // Минимум клиента НЕ учитываем: окно получает свой слот целиком, каким
        // бы маленьким он ни был. Клиенту уходит configure на этот размер; не
        // умеет ужиматься — нарисуется больше и накроет соседа, это принятая
        // цена (см. sync_dwindle_tree).
        let w = slot.size.w.min(max_w).max(1);
        let h = slot.size.h.min(max_h).max(1);
        if w == slot.size.w && h == slot.size.h {
            return slot;
        }
        let mut loc = Point::from((
            slot.loc.x + (slot.size.w - w) / 2,
            slot.loc.y + (slot.size.h - h) / 2,
        ));
        // За край экрана окно не пускаем: там его не достать ни мышью, ни
        // глазом. Если оно шире самой рабочей области — прижимаем к её началу.
        if let Some(area) = self.tile_work_area() {
            loc.x = loc.x.clamp(area.loc.x, (area.loc.x + area.size.w - w).max(area.loc.x));
            loc.y = loc.y.clamp(area.loc.y, (area.loc.y + area.size.h - h).max(area.loc.y));
        }
        tracing::debug!(
            "plx/tile: slot {:?} does not fit the client (max {:?}) → {:?} at {:?}",
            slot.size, max, Size::<i32, Logical>::from((w, h)), loc,
        );
        Rectangle::new(loc, (w, h).into())
    }

    pub fn resize_window_animated(
        &mut self,
        window: &Window,
        rect: Rectangle<i32, Logical>,
        animate: bool,
    ) {
        // Окно, которое сейчас держит мышь, раскладка не трогает ВООБЩЕ.
        // Иначе выходила драка: каждый свап соседей звал arrange, тот заводил
        // окну анимацию в слот, и она каждый тик тянула окно туда, пока motion
        // тянул его за курсором — окно дёргалось между двумя позициями. Заодно
        // arrange менял ему размер под чужой слот прямо под курсором. Плитка
        // должна спокойно висеть на мыши; в слот её посадит arrange из button()
        // на отпускании, уже с анимацией (к тому моменту dragged_window снят).
        if self.dragged_window.as_ref()
            .is_some_and(|d| crate::dwindle::same_window(d, window))
        {
            return;
        }
        let rect = self.fit_to_constraints(window, rect);
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

    /// Толчок окна «по инерции»: добавляет скорость к тому движению, что у окна
    /// уже есть, и пересчитывает точку остановки (путь доезда = |v|/ω). Окно,
    /// которое стояло, начинает лететь; окно, которое уже летело, ускоряется —
    /// поэтому цепочка толчков складывается, а не перезапускается с нуля.
    /// Возвращает точку остановки (она же пишется во float-позицию).
    pub(crate) fn impulse_window(
        &mut self,
        window: &Window,
        vel: Point<f64, Logical>,
        omega: f64,
    ) -> Point<i32, Logical> {
        let target = match self.window_pos_anims.iter_mut()
            .find(|(w, _)| crate::dwindle::same_window(w, window))
        {
            Some((_, anim)) => {
                let sum = Point::from((anim.vel.x + vel.x, anim.vel.y + vel.y));
                let speed = (sum.x * sum.x + sum.y * sum.y).sqrt();
                anim.coast(sum, crate::anim::glide_omega(speed).max(omega));
                anim.target
            }
            None => {
                let from = self.space.element_geometry(window)
                    .map(|g| g.loc.to_f64())
                    .unwrap_or_default();
                let target = Point::from((from.x + vel.x / omega, from.y + vel.y / omega));
                self.window_pos_anims.push((
                    window.clone(),
                    PosAnim::with_velocity(from, target, vel, omega),
                ));
                target
            }
        };
        let target = target.to_i32_round();
        if let Some(tw) = self.tagged_windows.iter_mut()
            .find(|tw| crate::dwindle::same_window(&tw.window, window))
        {
            tw.float_position = target;
            tw.position = target;
            tw.float_position_set = true;
        }
        target
    }

    fn animate_window_to(&mut self, window: &Window, target: Point<i32, Logical>) {
        self.animate_window_to_dur(window, target, crate::anim::дуг::сборка_тайлинга());
    }

    /// Смена раскладки ПО КОМАНДЕ пользователя (Win+D/Win+T/Win+N): окна
    /// переезжают в новую раскладку — тайлинг их раскладывает, Float
    /// разбрасывает кольцом вокруг центра экрана.
    pub fn set_layout(&mut self, layout: Layout) {
        self.set_layout_inner(layout, true);
    }

    /// Восстановление раскладки ПРИ ПЕРЕХОДЕ НА СТОЛ (view_tag): раскладка у
    /// стола своя, и её надо вернуть, но окна при этом трогать НЕЛЬЗЯ — они уже
    /// лежат там, где их оставили.
    ///
    /// Разница видна именно во Float. Разлёт (scatter_to_float) зажимает каждое
    /// окно в коробку размером с экран вокруг текущей камеры — на бесконечном
    /// холсте всё, что лежало дальше экрана, прижимается к одному и тому же
    /// углу. При смене раскладки руками это правильно (окна обязаны оказаться в
    /// кадре), а при переключении столов давало «все плавающие окна слетелись в
    /// одну точку»: достаточно было уехать камерой, уйти на соседний стол и
    /// вернуться. Плюс камера в этот момент ещё не финальная — свою позицию
    /// стол восстанавливает уже после (tag_cameras), — так что зажималось по
    /// чужому кадру.
    pub(crate) fn restore_layout(&mut self, layout: Layout) {
        self.set_layout_inner(layout, false);
    }

    fn set_layout_inner(&mut self, layout: Layout, move_windows: bool) {
        // Смена раскладки из-под живого обзора: обзор держит на холсте окна
        // ВСЕХ столов сетки и свою камеру/зум, а arrange под ним не работает
        // вовсе (см. arrange). Раньше Win+T/Win+N в обзоре меняли layout молча:
        // сетка оставалась на экране, и после выхода из тайлинга рядом с
        // текущим столом висел чужой. Столы обязаны быть строго разделены —
        // сначала выходим из обзора (он вернёт геометрию и смапит только свой
        // стол), потом уже меняем раскладку. Рекурсии нет: exit_overview_*
        // сам set_layout не зовёт, а overview_active уже сброшен.
        if self.overview_active || self.overview_exit_pending {
            self.exit_overview_immediate(None);
        }
        let prev = self.tile_config.layout;
        self.tile_config.prev_layout = prev;
        self.tile_config.layout = layout;

        // ── Изоляция столов ──────────────────────────────────────────────────
        // Раскладка — свойство СТОЛА, и tag_layouts единственное место, где оно
        // записано (по нему считается граница изоляции, см.
        // columns_is_strip_tag). Пишем СРАЗУ: всё, что ниже (refresh_tags,
        // arrange, геометрия этажей) уже должно видеть стол в его новой группе.
        let cur = self.viewport.current_tags();
        let already_known = self.tag_layouts.get(&cur) == Some(&layout);
        self.tag_layouts.insert(cur, layout);
        self.visited_tags.insert(cur);
        if (prev == Layout::Columns) != (layout == Layout::Columns) {
            // Стол ПЕРЕСЁК границу: вышел из ленты в tiling/floating или вошёл
            // в неё. Полосу колонок он забирает с собой на полку (и получает
            // обратно, вернувшись) — но только если сюда пришли не из view_tag:
            // там полки уже разложены по столам, а `cur` — это УЖЕ новый стол
            // (его раскладку view_tag записал в tag_layouts до вызова, отсюда
            // и признак already_known).
            if !already_known {
                if prev == Layout::Columns {
                    self.columns_save_for(cur);
                } else {
                    self.columns_load_for(cur);
                }
            }
            // Видимость: в ленте на холсте лежат окна ВСЕХ ленточных столов, вне
            // её — только свои. Без этого выход из ленты оставлял окна соседних
            // этажей поверх тайлового стола (нулевой этаж стоит ровно на экране).
            self.refresh_tags();
            // Стол покинул ленту (или вошёл в неё) — этажи ниже сдвинулись на
            // экран, их геометрию надо пересчитать.
            self.columns_relayout_strip();
        } else {
            // Смена раскладки внутри одной изоляции видимость не меняет, но
            // холст мог остаться с чужими окнами: обзор маппит столы сетки
            // рядом, и любой сбой на выходе из него оставлял соседний стол на
            // экране. refresh_tags — единственное место, где состав холста
            // считается заново по тегам, и он дёшев (окна уже смаплены).
            self.refresh_tags();
        }

        if layout == Layout::Float {
            // Возврат из тайлинга — камера туда же, где холст был до входа.
            // Вход в тайлинг насильно ставит камеру в (0,0) при zoom=1 (иначе
            // разложенные по экрану окна оказались бы за кадром), и без этого
            // снимка выход во Float каждый раз выбрасывал в начало координат:
            // окна, к которым пользователь долго ехал по холсту, оставались
            // где-то далеко, а «привычное место» терялось.
            // Обе части — и камера, и разлёт — только для команды
            // пользователя: при переходе на стол камеру ставит view_tag по
            // tag_cameras, а окна остаются на своих местах.
            if move_windows {
                if let Some((cam_x, cam_y, zoom)) = self.pre_tiling_view.take() {
                    self.momentum.stop();
                    self.camera_anim = None;
                    self.zoom_anim = None;
                    // См. симметричную правку в ветке tiling ниже — zoom_glide
                    // это отдельный, третий, механизм анимации зума (доезд
                    // колеса), и он тоже обязан быть погашен здесь, иначе
                    // следующий тик сам пересчитает camera от старого якоря.
                    self.zoom_glide = None;
                    self.viewport.zoom = zoom;
                    self.viewport.cam_x = cam_x;
                    self.viewport.cam_y = cam_y;
                    self.apply_camera();
                }
                // Строго ПОСЛЕ восстановления камеры: разлёт считает кольцо от
                // центра ЭКРАНА, то есть от текущего положения камеры.
                self.scatter_to_float(prev);
            }
        } else {
            // Float → tiling: в раскладку идут ВСЕ окна стола, а не только те,
            // что случайно уцелели с флагом !floating. Во Float любое движение
            // окна мышью/жестом ставит tw.floating = true (см. input.rs,
            // move_grab) — это метка «вытащено из тайлинга», и на обратном пути
            // она обязана сниматься, иначе после десятка перетасканных окон в
            // тайлинг переезжали единицы. Намеренно плавающие (диалоги, явный
            // toggle_floating — float_pinned) остаются поверх раскладки.
            if prev == Layout::Float {
                let cur_tags = self.viewport.current_tags();
                for tw in self.tagged_windows.iter_mut() {
                    if tw.tags & cur_tags != 0 && !tw.float_pinned {
                        tw.floating = false;
                    }
                }
            }
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
            // Запоминаем, КАК СТОЯЛ ХОЛСТ, — место и размер каждого окна на
            // момент ухода во тайлинг. Обратный переход (scatter_to_float)
            // ставит окна ровно сюда, а разлёт кольцом остаётся только для
            // тех, кто во Float ещё не жил ни разу.
            //
            // Раньше запоминалась одна позиция и только у окон, которые
            // двигали руками; размер брался «текущий» — то есть ТАЙЛОВЫЙ, — и
            // ужимался до 70% экрана, после чего позиция ещё и зажималась в
            // экранную коробку. Окна возвращались похоже, но не туда и не
            // того размера.
            // Снимок берём по ЦЕЛИ анимации, если окно ещё летит.
            //
            // Из-за этого окна и «сжимались к центру», если нажать Win+D
            // несколько раз подряд (жалоба 06.08.2026). Разлёт во Float —
            // анимация на 600 мс; нажатие Win+D раньше, чем она доиграла,
            // заставало окно НА ПОЛПУТИ между тайловым местом (у камеры, то
            // есть в середине экрана) и своим float-местом. Именно эта
            // промежуточная точка и записывалась сюда как «своё место», а
            // следующий разлёт вёл окно уже в неё. Каждое нажатие съедало
            // часть пути, и окна шаг за шагом сползались к центру: в логе
            // 05.08.2026 за один цикл окно с (-1539,-391) переезжало на
            // (-389,101), за следующий — ещё ближе, и так до кучи в середине.
            //
            // Цель анимации — это ровно то место, куда окно СОБИРАЛОСЬ встать,
            // то есть его настоящее float-место; окно, стоящее спокойно,
            // анимации не имеет, и берётся его живая геометрия (перетащили
            // мышью — запомним новое место, как и раньше).
            if prev == Layout::Float && move_windows {
                let cur_tags = self.viewport.current_tags();
                let снимок: Vec<(Window, Point<i32, Logical>, Size<i32, Logical>, bool)> =
                    self.tagged_windows.iter()
                        .filter(|tw| tw.tags & cur_tags != 0)
                        .filter_map(|tw| {
                            let летит = self.window_anim_target(&tw.window);
                            self.space.element_geometry(&tw.window)
                                .map(|g| (
                                    tw.window.clone(),
                                    летит.unwrap_or(g.loc),
                                    g.size,
                                    летит.is_some(),
                                ))
                        })
                        .collect();
                for (window, loc, size, летит) in снимок {
                    if let Some(tw) = self.tagged_windows.iter_mut()
                        .find(|tw| tw.window == window)
                    {
                        tracing::debug!(
                            "plx/float: place snapshot {:?} size {:?}{}",
                            loc, size, if летит { " (from the animation target)" } else { "" },
                        );
                        tw.float_position = loc;
                        // Размер трогаем только у осевшего окна. Пока идёт
                        // перелёт, клиент ещё не успел применить configure, и
                        // живая геометрия — это размер ПРЕДЫДУЩЕЙ раскладки;
                        // записав его, мы бы точно так же, шаг за шагом,
                        // подменяли float-размер тайловым.
                        if !летит {
                            tw.float_size = Some(size);
                        }
                        tw.float_position_set = true;
                    }
                }
            }
            // Переход в tiling/columns ПО КОМАНДЕ (Win+D/Win+T/Win+N): камера в
            // угол СВОЕГО монитора, zoom=1, arrange раскладывает окна. Окна
            // плывут от текущей позиции к тайловой через window_pos_anims
            // (animate_window_to, 180ms). Без slide-in (N map_element на кадр +
            // пустой кадр выброса влево).
            //
            // **Угол монитора, а не (0,0) холста.** Тайлинг раскладывает окна в
            // прямоугольнике `screen_area()`, а он начинается в ДОМЕ монитора
            // (`monitors::Монитор::дом`); у второго монитора это (1 000 000, 0)
            // — см. `monitors::ШАГ_ДОМА`. Пока здесь стоял жёсткий ноль, Win+D
            // на втором мониторе уводил камеру на миллион пикселей от только что
            // разложенных окон: экран оставался с одними обоями, хотя чипы окон
            // в панели были на месте. Замер 26.08.2026 на двухмониторном
            // харнессе: после Win+D камера=(0,0), а окно в слоте 1000026,26.
            // Это и есть «Win+D работает криво».
            //
            // При ПЕРЕХОДЕ НА СТОЛ (restore_layout, move_windows = false) камеру
            // не трогаем вовсе: у стола свой запомненный кадр — камера И зум, —
            // и ставит его view_tag сразу после нас. Пока обнуление стояло тут
            // безусловно, кадр стола было не вернуть: сначала его затирали
            // нулями, а потом view_tag для тайловых столов и вовсе выходил
            // раньше восстановления. Отсюда и «камера остаётся на одном месте».
            if move_windows {
                let дом = self.монитор_дом();
                self.momentum.stop();
                self.camera_anim = None;
                self.zoom_anim = None;
                // Доезд колеса (zoom_glide) — ТРЕТЬЯ, отдельная от двух выше
                // анимация: она держит на месте точку холста под курсором и
                // на каждом тике САМА пересчитывает camera из своего якоря
                // (см. ZoomGlide::advance). Без сброса здесь она переживала
                // Win+D и на первом же anim::tick после него утаскивала
                // камеру обратно от дома монитора к своему старому якорю —
                // «Win+D во время зума ставит камеру не в (0,0)».
                self.zoom_glide = None;
                self.viewport.zoom = 1.0;
                self.viewport.cam_x = дом.x as f64;
                // В ленте начало холста — не верх стола, а ЭТАЖ этого стола:
                // столы там стоят друг под другом (стол N на высоте N × экран).
                // Камера в верхнюю точку показала бы первый этаж ленты вместо
                // того стола, на котором её включили. `columns_ws_y` уже
                // отсчитывает этажи от дома монитора, поэтому второй раз его
                // прибавлять не нужно.
                self.viewport.cam_y = if layout == Layout::Columns {
                    self.columns_cur_y()
                } else {
                    дом.y as f64
                };
                self.columns_float_cam = (self.viewport.cam_x, self.viewport.cam_y);
                self.apply_camera();
            }
            if layout == Layout::Columns {
                self.columns_set_active_to_focus();
            }
            self.arrange();
        }
        tracing::info!("plx: layout → {}", layout.symbol());
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
        let current_tags = self.viewport.current_tags();

        // Центр разлёта — середина ТОГО, ЧТО ВИДНО (она же середина экрана),
        // а размер коробки, в которую зажимаем окна, — РОВНО ЭКРАН.
        //
        // Обе части важны. Если считать по видимой области целиком, то на
        // отдалённом зуме (видно 12800×5400 холста) окна разлетаются по всему
        // этому простору. Если же брать экран, но привязывать его к УГЛУ
        // камеры, окна на отдалённом зуме улетают к дальнему краю холста — и
        // при возврате к зуму 1 экран оказывается пуст. Экран по центру видимой
        // области даёт одно и то же место при любом зуме.
        let видимое = self.visible_canvas_size();
        let cx = self.viewport.cam_x + видимое.w / 2.0;
        let cy = self.viewport.cam_y + видимое.h / 2.0;
        let экран = self.screen_size();
        let vis = Size::<f64, Logical>::from((экран.w as f64, экран.h as f64));
        let output_geo = Rectangle::<i32, Logical>::new(
            (0, 0).into(), (vis.w.round() as i32, vis.h.round() as i32).into(),
        );

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
                    // Окно уже жило во Float — возвращаем ровно туда и ровно
                    // таким, каким оно оттуда уходило: ни ужатия до 70%, ни
                    // зажатия в экранную коробку. Холст бесконечный, а камера к
                    // этому моменту уже вернулась на своё место (pre_tiling_view
                    // в set_layout_inner), так что окно окажется в кадре там же,
                    // где его оставили. Кольцевой разлёт ниже — только для тех,
                    // кто во Float ещё не был ни разу.
                    if tw.float_position_set {
                        tracing::debug!("plx/float: back to place {:?} size {:?}",
                            tw.float_position, tw.float_size);
                        return (tw.window.clone(), tw.float_size, tw.float_position);
                    }
                    let текущий = self.space.element_geometry(&tw.window)
                        .map(|g| g.size)
                        .unwrap_or_else(|| (400, 300).into());
                    // Размер во Float: своё запомненное, иначе текущий, но не
                    // больше 70% видимой области. Окно, пришедшее из тайлинга
                    // во весь экран, иначе занимало бы во Float ровно столько
                    // же — три таких окна ложились друг на друга стопкой, и
                    // «разлёта» было не видно.
                    let доступно_w = (vis.w.round() as i32 - 2 * GAP_OUTER).max(200);
                    let доступно_h = (vis.h.round() as i32 - BAR_RESERVED - GAP_OUTER).max(150);
                    let предел_w = (доступно_w as f64 * 0.7).round() as i32;
                    let предел_h = (доступно_h as f64 * 0.7).round() as i32;
                    let желаемый = tw.float_size.unwrap_or(текущий);
                    let k = (предел_w as f64 / желаемый.w.max(1) as f64)
                        .min(предел_h as f64 / желаемый.h.max(1) as f64)
                        .min(1.0);
                    // Пол — 1 px: если окно уходило в тайлинг крошечным, во
                    // Float оно обязано остаться таким же. Прежние 200×150
                    // раздували его обратно — тот самый «лимит по размеру».
                    let win_size: Size<i32, Logical> = (
                        ((желаемый.w as f64 * k).round() as i32).max(1),
                        ((желаемый.h as f64 * k).round() as i32).max(1),
                    ).into();
                    // Куда бы окно ни просилось — оно обязано остаться В КАДРЕ.
                    // Кольцо разлёта считается от центра и не знает про размеры
                    // окна: окно, пришедшее из тайлинга во весь экран, уезжало
                    // наполовину за край и лезло под бар. Зажимаем позицию так,
                    // чтобы окно целиком помещалось в видимую область, а сверху
                    // оставалась полоса под бар.
                    let зажать = |p: smithay::utils::Point<i32, Logical>| {
                        let поле = GAP_OUTER;
                        let верх = BAR_RESERVED;
                        // Коробка размером с экран, центрированная на (cx, cy).
                        let x0 = (cx - vis.w / 2.0).round() as i32;
                        let y0 = (cy - vis.h / 2.0).round() as i32;
                        let min_x = x0 + поле;
                        let min_y = y0 + верх;
                        let max_x = (x0 + vis.w.round() as i32 - поле - win_size.w).max(min_x);
                        let max_y = (y0 + vis.h.round() as i32 - поле - win_size.h).max(min_y);
                        smithay::utils::Point::<i32, Logical>::from((
                            p.x.clamp(min_x, max_x),
                            p.y.clamp(min_y, max_y),
                        ))
                    };
                    let pos = {
                        let base_angle = angle_step * idx as f64;
                        // jitter ± 40% от шага угла
                        let jitter = (lcg_f64(time_seed.wrapping_add(idx as u64)) - 0.5)
                            * angle_step * 0.8;
                        let angle = base_angle + jitter;
                        let r = min_r + lcg_f64(time_seed.wrapping_add(idx as u64 + 100))
                            * (max_r - min_r);
                        let x = (cx + angle.cos() * r - win_size.w as f64 / 2.0) as i32;
                        let y = (cy + angle.sin() * r - win_size.h as f64 / 2.0) as i32;
                        зажать(smithay::utils::Point::from((x, y)))
                    };
                    tracing::debug!("plx/float: SCATTER in a ring at {:?} size {:?}", pos, win_size);
                    (tw.window.clone(), Some(win_size), pos)
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
            self.animate_window_to_dur(window, *pos, crate::anim::дуг::разлёт_во_флоат());
        }

        // Обновляем tagged_windows
        for (window, float_size, pos) in &updates {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                &tw.window == window
            }) {
                tw.float_position = *pos;
                tw.position = *pos;
                // Размер тоже фиксируем: без этого следующий переход во Float
                // считал «желаемый» размер от ТАЙЛОВОЙ геометрии окна и ужимал
                // её до 70% экрана — окно возвращалось на своё место, но
                // каждый раз другого размера.
                if float_size.is_some() {
                    tw.float_size = *float_size;
                }
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
    pub(crate) fn snap_camera_to_window(&mut self, window: &Window) {
        // Кадр = видимая часть холста (экран ⁄ зум).
        let vis = self.visible_canvas_size();
        let out_geo = Rectangle::<i32, Logical>::new(
            (self.viewport.cam_x.round() as i32, self.viewport.cam_y.round() as i32).into(),
            (vis.w.round() as i32, vis.h.round() as i32).into(),
        );
        let geo = match self.space.element_geometry(window) {
            Some(g) => g,
            None => return,
        };

        // Начало полосы колонок — угол СВОЕГО монитора, а не ноль холста:
        // столы второго монитора живут в его доме (см. `monitors::ШАГ_ДОМА`).
        let дом = self.монитор_дом();
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
                if cam_x < дом.x as f64 { cam_x = дом.x as f64; }
                (cam_x, дом.y as f64) // колонки на всю высоту от верха стола
            }
            _ => return,
        };

        let from = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let to = Point::from((to_x, to_y));
        if (to.x - from.x).abs() > 0.5 || (to.y - from.y).abs() > 0.5 {
            self.camera_anim = Some(CameraAnim::new(from, to, crate::anim::дуг::прыжок_к_окну()));
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
        // В обзоре столов дерево окна живёт в рамке СВОЕГО стола (ячейка
        // сетки), а не в рабочей области экрана, и накатывает его
        // overview_layout: обычные tile_work_area/apply_tile_layout там
        // разложили бы стол поверх соседей.
        if self.overview_active {
            let Some(mask) = self.overview_mask_of_window(window) else { return };
            // Дерево стола считается в ДОМАШНЕЙ рабочей области (экран от 0,0) —
            // и в обзоре тоже: обзор только сдвигает готовый стол в его ячейку
            // (см. overview_layout). Раньше дерево пересчитывалось прямо в
            // рамку ячейки, из-за чего раскладка обзора расходилась с настоящей.
            let Some(area) = self.tile_work_area() else { return };
            let cfg = self.lua_config.dwindle;
            if let Some(tree) = self.dwindle_trees.get_mut(&mask) {
                tree.resize(window, delta, corner, area, &cfg);
            }
            self.overview_apply_tree(mask);
            if let Some((w, h)) = self.overview_band_size() {
                self.overview_layout(w, h);
            }
            self.request_plane_reset();
            self.request_redraw();
            return;
        }
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
            tracing::info!("plx: stack unfolded");
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
            tracing::info!("plx: stack folded ({} windows)", visible_idxs.len());
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
                // Явный выбор пользователя — сборка в тайлинг его не отменяет.
                tw.float_pinned = tw.floating;
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
            // Игра занимает весь экран, и «фокус вправо» из панели всегда
            // попадал бы в неё — то есть в никуда (см. `mine::в_переборе`).
            .filter(|tw| crate::mine::в_переборе(self, &tw.window))
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
            // Окно Minecraft из кольца вычитается: попав на него, перебор
            // застревал навсегда (см. `mine::в_переборе`).
            .filter(|tw| crate::mine::в_переборе(self, &tw.window))
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
        // Окно увели с места руками — если оно из созвездия, гроздь растащена
        // (та же метка, что и при драге мышью, см. grabs/move_grab.rs).
        self.mark_constellation_torn(&w);
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
        // Пол — 1 px, а не «удобные» 50: своего нижнего порога у parallax больше
        // нет нигде (см. tiling::fit_to_constraints и dwindle::RATIO_MIN).
        let new_w = (cur.w + dw).max(1);
        let new_h = (cur.h + dh).max(1);
        crate::xwin::set_size(&w, Some((new_w, new_h).into()), crate::xwin::Tiled::Keep);
        crate::xwin::configure(&w);
    }
}
