use std::time::Duration;

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle, Size},
};

use crate::state::Dawn;
use crate::tiling::Layout;

fn same_window(a: &Window, b: &Window) -> bool {
    a == b
}

impl Dawn {
    pub fn is_selected(&self, window: &Window) -> bool {
        self.selected_windows.iter().any(|w| same_window(w, window))
    }

    /// Финализирует rubber-band выделение (см. grabs/select_grab.rs): выделяет
    /// все окна текущего тега, чья геометрия пересекается с рамкой. Рамка
    /// меньше пары пикселей (обычный клик без протяжки) трактуется как снятие
    /// выделения — как в файловых менеджерах.
    pub fn select_windows_in_rect(&mut self, rect: Option<Rectangle<i32, Logical>>) {
        let rect = match rect {
            Some(r) if r.size.w > 2 && r.size.h > 2 => r,
            _ => {
                self.clear_selection();
                return;
            }
        };
        let current_tags = self.viewport.current_tags();
        self.selected_windows = self.tagged_windows.iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .filter_map(|tw| self.space.element_geometry(&tw.window).map(|g| (tw.window.clone(), g)))
            .filter(|(_, g)| g.intersection(rect).is_some())
            .map(|(w, _)| w)
            .collect();
        tracing::info!("dawn: selected {} windows", self.selected_windows.len());
        self.request_redraw();
    }

    pub fn clear_selection(&mut self) {
        if !self.selected_windows.is_empty() {
            self.selected_windows.clear();
            self.request_redraw();
        }
    }

    fn constellation_index_of(&self, window: &Window) -> Option<usize> {
        self.constellations.iter().position(|g| g.iter().any(|w| same_window(w, window)))
    }

    /// Кого тащить и масштабировать ВМЕСТЕ с `window` — это ВЫДЕЛЕНИЕ, а не
    /// созвездие.
    ///
    /// Созвездие (Super+G) больше не двигается целиком: оно фиксирует взаимное
    /// расположение окон, но хвататься за одно окно и утаскивать всю группу
    /// оказалось неожиданно — тянешь одно, едут пять. Групповой драг/ресайз
    /// теперь ровно там, где его просят явно: если окно входит в выделение
    /// (рамкой по Super+ЛКМ), едет всё выделение; если нет — едет только оно.
    pub fn group_drag_members_excluding(&self, window: &Window) -> Vec<Window> {
        if !self.is_selected(window) {
            return Vec::new();
        }
        self.selected_windows.iter()
            .filter(|w| !same_window(w, window))
            .cloned()
            .collect()
    }

    /// Остальные окна из "созвездия" данного окна (без самого `window`).
    /// Грабами больше НЕ используется (см. group_drag_members_excluding),
    /// оставлено для операций над самим созвездием.
    #[allow(dead_code)]
    pub fn constellation_members_excluding(&self, window: &Window) -> Vec<Window> {
        self.constellation_index_of(window)
            .map(|i| self.constellations[i].iter()
                .filter(|w| !same_window(w, window))
                .cloned()
                .collect())
            .unwrap_or_default()
    }

    /// Super+G: собрать текущее выделение в "созвездие" — двигается и
    /// ресайзится как единое целое (см. grabs/move_grab.rs, grabs/resize_grab.rs).
    /// Работает только в Float — в тайлинге позициями и так управляет arrange().
    pub fn group_selected_into_constellation(&mut self) {
        if self.tile_config.layout != crate::tiling::Layout::Float {
            return;
        }
        if self.selected_windows.len() < 2 {
            tracing::info!("dawn: need 2+ selected windows to form a constellation");
            return;
        }
        // Убираем выбранные окна из групп, в которых они уже состояли —
        // окно не может быть в двух созвездиях одновременно.
        for w in self.selected_windows.clone() {
            if let Some(idx) = self.constellation_index_of(&w) {
                self.constellations[idx].retain(|x| !same_window(x, &w));
            }
        }
        self.constellations.retain(|g| g.len() > 1);
        self.constellations.push(self.selected_windows.clone());
        tracing::info!("dawn: constellation formed ({} windows)", self.selected_windows.len());
    }

    /// Super+D при активном выделении: магнитно стянуть выделенные окна в
    /// компактную "гроздь" вокруг их общего центра и зафиксировать как
    /// созвездие (дальше двигается/ресайзится как единое целое). В отличие от
    /// group_selected_into_constellation (просто фиксирует группу на месте),
    /// здесь окна реально съезжаются вместе плавным LERP.
    pub fn gather_selected_into_constellation(&mut self) {
        if self.selected_windows.len() < 2 {
            return;
        }
        // Созвездия живут во Float (свободные позиции) — переводим туда, если
        // сейчас тайлинг, и помечаем выделенные окна плавающими.
        if self.tile_config.layout != crate::tiling::Layout::Float {
            self.set_layout(crate::tiling::Layout::Float);
        }

        // Размеры выделенных окон.
        let items: Vec<(Window, i32, i32)> = self.selected_windows.iter()
            .filter_map(|w| self.space.element_geometry(w).map(|g| (w.clone(), g.size.w, g.size.h)))
            .collect();
        if items.len() < 2 {
            return;
        }

        // Центр тяжести текущих позиций — вокруг него собираем гроздь.
        let (mut cx, mut cy) = (0.0f64, 0.0f64);
        for w in &self.selected_windows {
            if let Some(g) = self.space.element_geometry(w) {
                cx += g.loc.x as f64 + g.size.w as f64 / 2.0;
                cy += g.loc.y as f64 + g.size.h as f64 / 2.0;
            }
        }
        let n = items.len() as f64;
        cx /= n;
        cy /= n;

        // Компактная сетка: cols ≈ sqrt(n), окна кладём построчно с зазором,
        // выравнивая по высоте строки. Затем блок центрируем на (cx, cy).
        const GAP: i32 = 12;
        let cols = (items.len() as f64).sqrt().ceil() as usize;
        let rows: Vec<&[(Window, i32, i32)]> = items.chunks(cols).collect();

        // Высота каждой строки и её ширина.
        let row_heights: Vec<i32> = rows.iter()
            .map(|row| row.iter().map(|(_, _, h)| *h).max().unwrap_or(0))
            .collect();
        let row_widths: Vec<i32> = rows.iter()
            .map(|row| row.iter().map(|(_, w, _)| *w).sum::<i32>() + GAP * (row.len().saturating_sub(1)) as i32)
            .collect();
        let block_h: i32 = row_heights.iter().sum::<i32>() + GAP * (rows.len().saturating_sub(1)) as i32;
        let block_w: i32 = row_widths.iter().copied().max().unwrap_or(0);

        let start_x = (cx - block_w as f64 / 2.0).round() as i32;
        let mut y = (cy - block_h as f64 / 2.0).round() as i32;

        let mut targets: Vec<(Window, Point<i32, Logical>)> = Vec::new();
        for (ri, row) in rows.iter().enumerate() {
            // Центрируем строку по горизонтали внутри блока.
            let mut x = start_x + (block_w - row_widths[ri]) / 2;
            for (w, ww, _) in row.iter() {
                targets.push((w.clone(), Point::from((x, y))));
                x += ww + GAP;
            }
            y += row_heights[ri] + GAP;
        }

        for (w, pos) in &targets {
            self.animate_window_to_dur(w, *pos, std::time::Duration::from_millis(260));
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                &tw.window == w
            }) {
                tw.floating = true;
                tw.float_position = *pos;
                tw.position = *pos;
                tw.float_position_set = true;
            }
        }

        // Фиксируем как созвездие (двигается/ресайзится как целое).
        let group: Vec<Window> = targets.iter().map(|(w, _)| w.clone()).collect();
        for w in &group {
            if let Some(idx) = self.constellation_index_of(w) {
                self.constellations[idx].retain(|x| !same_window(x, w));
            }
        }
        self.constellations.retain(|g| g.len() > 1);
        self.constellations.push(group);
        tracing::info!("dawn: constellation gathered ({} windows)", targets.len());
        // Выделение больше не нужно — созвездие зафиксировано.
        self.selected_windows.clear();
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Super+D без выделения: строго и безотказно собрать ВСЕ окна текущего
    /// тега в tiling. Снимаем floating (иначе apply_tile_layout их игнорирует —
    /// `!tw.floating`), распускаем созвездия и раскладываем в Tile.
    pub fn gather_all_into_tiling(&mut self) {
        self.clear_selection();
        self.constellations.clear();
        let current = self.viewport.current_tags();
        for tw in self.tagged_windows.iter_mut() {
            if tw.tags & current != 0 {
                tw.floating = false;
                // float_position_set НЕ трогаем: сохранённая float-позиция —
                // это "своё место" окна, куда оно вернётся при выходе из tiling
                // (scatter_to_float его восстановит, а не раскидает заново).
            }
        }
        // set_layout(Tile) сам жёстко обнуляет камеру/zoom и глушит инерцию
        // (см. tiling.rs) — окна собираются от (0,0), в кадре, без "уезжания".
        self.set_layout(Layout::Tile);
        self.request_plane_reset();
        self.request_redraw();
    }

    /// true, если ВСЕ выделенные окна принадлежат одному и тому же созвездию —
    /// значит выделение "поймало" готовое созвездие и Super+D его расцепит.
    pub fn selection_is_constellation(&self) -> bool {
        if self.selected_windows.len() < 2 {
            return false;
        }
        let idx = match self.selected_windows.first().and_then(|w| self.constellation_index_of(w)) {
            Some(i) => i,
            None => return false,
        };
        self.selected_windows.iter()
            .all(|w| self.constellation_index_of(w) == Some(idx))
    }

    /// Super+D по выделенному созвездию: распустить его и РАЗМЕТАТЬ окна в
    /// стороны от общего центра с анимацией (эффект "разлёта"). Окна остаются
    /// плавающими на новых позициях.
    pub fn scatter_selected_constellation(&mut self) {
        let idx = match self.selected_windows.first().and_then(|w| self.constellation_index_of(w)) {
            Some(i) => i,
            None => return,
        };
        let group = self.constellations.remove(idx);

        // Центр тяжести группы.
        let (mut cx, mut cy, mut n) = (0.0f64, 0.0f64, 0.0f64);
        for w in &group {
            if let Some(g) = self.space.element_geometry(w) {
                cx += g.loc.x as f64 + g.size.w as f64 / 2.0;
                cy += g.loc.y as f64 + g.size.h as f64 / 2.0;
                n += 1.0;
            }
        }
        if n == 0.0 {
            self.clear_selection();
            return;
        }
        cx /= n;
        cy /= n;

        // Каждое окно улетает от центра: новый вектор = текущий * SCATTER.
        // Если окно ровно в центре (нулевой вектор) — толкаем по кругу, чтобы
        // одиночная строка/столбец тоже разлетелись равномерно.
        const SCATTER: f64 = 2.4;
        const MIN_PUSH: f64 = 320.0;
        let count = group.len();
        for (i, w) in group.iter().enumerate() {
            let g = match self.space.element_geometry(w) {
                Some(g) => g,
                None => continue,
            };
            let wcx = g.loc.x as f64 + g.size.w as f64 / 2.0;
            let wcy = g.loc.y as f64 + g.size.h as f64 / 2.0;
            let (mut dx, mut dy) = ((wcx - cx) * SCATTER, (wcy - cy) * SCATTER);
            if dx.hypot(dy) < 1.0 {
                let a = std::f64::consts::TAU * (i as f64) / (count.max(1) as f64);
                dx = a.cos() * MIN_PUSH;
                dy = a.sin() * MIN_PUSH;
            }
            let pos = Point::from((
                (cx + dx - g.size.w as f64 / 2.0).round() as i32,
                (cy + dy - g.size.h as f64 / 2.0).round() as i32,
            ));
            self.animate_window_to_dur(w, pos, Duration::from_millis(320));
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                &tw.window == w
            }) {
                tw.floating = true;
                tw.float_position = pos;
                tw.position = pos;
                tw.float_position_set = true;
            }
        }

        self.constellations.retain(|g| g.len() > 1);
        self.clear_selection();
        tracing::info!("dawn: constellation scattered ({} windows)", count);
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Super+Shift+G: разбить созвездие, в котором состоит сфокусированное окно.
    pub fn ungroup_focused_constellation(&mut self) {
        let focused = match self.focused_surface() {
            Some(f) => f,
            None => return,
        };
        let window = match self.tagged_windows.iter()
            .find(|tw| crate::xwin::is_surface(&tw.window, &focused))
            .map(|tw| tw.window.clone())
        {
            Some(w) => w,
            None => return,
        };
        if let Some(idx) = self.constellation_index_of(&window) {
            self.constellations.remove(idx);
            tracing::info!("dawn: constellation disbanded");
        }
    }

    /// Закрыть все выделенные окна разом; если выделения нет — обычное
    /// поведение Kill (закрыть сфокусированное).
    pub fn kill_selected_or_focused(&mut self) {
        if !self.selected_windows.is_empty() {
            let n = self.selected_windows.len();
            for w in self.selected_windows.clone() {
                crate::xwin::close(&w);
            }
            tracing::info!("dawn: killed {} selected windows", n);
            self.clear_selection();
        } else {
            self.kill_focused();
        }
    }

    // ── Win+V: выбранные окна в плавающий слой ───────────────────────────────

    /// Переводит ВЫДЕЛЕННЫЕ окна (а если выделения нет — сфокусированное) в
    /// плавающий слой и обратно. Работает и в тайлинге, и в ленте niri: обе
    /// раскладки сами выкидывают плавающие окна из своей структуры
    /// (`sync_dwindle_tree` и `columns_reconcile` фильтруют по `!floating`) и
    /// принимают их назад, когда флаг снят.
    ///
    /// Окно НЕ ПОКИДАЕТ свой стол: позиция считается внутри прямоугольника
    /// текущего рабочего стола (в ленте это его этаж и текущий кадр прокрутки,
    /// см. workspace_rect) и зажимается в него вместе с размером. Поэтому
    /// всплывшее окно не может оказаться ни на соседнем этаже ленты, ни за
    /// краем экрана.
    ///
    /// Флаг ставится «намеренным» (float_pinned): сборка в тайлинг по Win+D
    /// такие окна не забирает — они для того и подняты, чтобы висеть поверх.
    pub fn float_selected(&mut self) {
        let targets: Vec<Window> = if !self.selected_windows.is_empty() {
            self.selected_windows.clone()
        } else {
            self.focused_window().into_iter().collect()
        };
        if targets.is_empty() {
            return;
        }
        // В Float поднимать некуда — там плавает всё.
        if self.tile_config.layout == Layout::Float {
            tracing::info!("dawn: float_selected — уже во Float, нечего поднимать");
            return;
        }
        let current = self.viewport.current_tags();
        // Тумблер по большинству: если ВСЕ цели уже плавающие — опускаем их
        // обратно в раскладку, иначе поднимаем все.
        let вниз = targets.iter().all(|w| {
            self.tagged_windows.iter()
                .any(|tw| same_window(&tw.window, w) && tw.floating)
        });

        if вниз {
            for w in &targets {
                if let Some(tw) = self.tagged_windows.iter_mut()
                    .find(|tw| same_window(&tw.window, w))
                {
                    tw.floating = false;
                    tw.float_pinned = false;
                }
            }
            self.arrange();
            self.request_plane_reset();
            self.request_redraw();
            tracing::info!("dawn: float_selected — {} окон вернулись в раскладку", targets.len());
            return;
        }

        let Some(стол) = self.workspace_rect() else { return };
        // Размер: не больше 70% стола по каждой стороне, чтобы всплывшее окно
        // оставалось «окном поверх», а не закрывало стол целиком.
        let max_w = (стол.size.w * 7 / 10).max(1);
        let max_h = (стол.size.h * 7 / 10).max(1);

        let mut подняты = 0usize;
        for (i, w) in targets.iter().enumerate() {
            if !self.tagged_windows.iter().any(|tw| {
                same_window(&tw.window, w) && tw.tags & current != 0
            }) {
                continue; // окно не с этого стола — не трогаем
            }
            let текущий = self.space.element_geometry(w)
                .map(|g| g.size)
                .unwrap_or_else(|| (max_w, max_h).into());
            let size: Size<i32, Logical> = (
                текущий.w.min(max_w).max(1),
                текущий.h.min(max_h).max(1),
            ).into();

            // Каскад от центра стола, чтобы поднятые окна не легли стопкой.
            const ШАГ: i32 = 36;
            let cx = стол.loc.x + (стол.size.w - size.w) / 2 + i as i32 * ШАГ;
            let cy = стол.loc.y + (стол.size.h - size.h) / 2 + i as i32 * ШАГ;
            // Зажимаем в границы стола с полем GAP_OUTER.
            let поле = crate::tiling::GAP_OUTER;
            let min_x = стол.loc.x + поле;
            let min_y = стол.loc.y + поле;
            let max_x = (стол.loc.x + стол.size.w - size.w - поле).max(min_x);
            let max_y = (стол.loc.y + стол.size.h - size.h - поле).max(min_y);
            let pos: Point<i32, Logical> = (cx.clamp(min_x, max_x), cy.clamp(min_y, max_y)).into();

            crate::xwin::set_size(w, Some(size), crate::xwin::Tiled::Unset);
            crate::xwin::configure(w);
            self.animate_window_to_dur(w, pos, Duration::from_millis(200));
            if let Some(tw) = self.tagged_windows.iter_mut()
                .find(|tw| same_window(&tw.window, w))
            {
                tw.floating = true;
                tw.float_pinned = true;
                tw.float_size = Some(size);
                tw.float_position = pos;
                tw.float_position_set = true;
                tw.position = pos;
            }
            подняты += 1;
        }

        // Раскладка пересобирается уже без поднятых окон: дерево/колонки
        // сомкнутся, освободив их слоты.
        self.arrange();
        self.clear_selection();
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("dawn: float_selected — поднято {} окон в границах стола {:?}",
            подняты, стол);
    }

    /// Прямоугольник ТЕКУЩЕГО рабочего стола на холсте.
    ///
    /// В ленте niri столы стоят этажами друг под другом, а колонки ещё и
    /// прокручиваются вбок — поэтому там стол это «экран на своём этаже в
    /// текущем кадре прокрутки». В остальных раскладках стол всегда стоит в
    /// начале холста: тайлинг раскладывается от (0,0) при камере в нуле.
    pub(crate) fn workspace_rect(&self) -> Option<Rectangle<i32, Logical>> {
        let screen = self.screen_area()?;
        let loc: Point<i32, Logical> = if self.tile_config.layout == Layout::Columns {
            (self.viewport.cam_x.round() as i32, self.columns_cur_y().round() as i32).into()
        } else {
            (0, 0).into()
        };
        Some(Rectangle::new(loc, screen.size))
    }
}
