
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
    /// Едет ВЫДЕЛЕНИЕ (рамка по Super+ЛКМ) и СОЗВЕЗДИЕ (Super+G), в котором
    /// состоит окно, — по базе, то есть сохраняя взаимное расположение.
    ///
    /// **История решения.** Сначала созвездие тащилось целиком, и это оказалось
    /// неожиданным: хватаешь одно окно — едут пять, без единого намёка, что так
    /// будет. Тогда групповой драг оставили только выделению, а созвездие
    /// свелось к «метке о родстве». Но фиксировать взаимное расположение и не
    /// двигаться вместе — это ровно половина смысла: за тем его и собирают.
    ///
    /// Поэтому созвездие снова едет целиком, а неожиданность лечится не
    /// отключением, а ПОКАЗОМ: пока идёт драг, dawn рисует, куда встанут
    /// остальные (см. `Dawn::призраки_группы` и `udev::build_ghost_elements`).
    /// Видимое намерение — не сюрприз.
    pub fn group_drag_members_excluding(&self, window: &Window) -> Vec<Window> {
        let mut члены: Vec<Window> = Vec::new();
        if self.is_selected(window) {
            члены.extend(self.selected_windows.iter().cloned());
        }
        члены.extend(self.constellation_members_excluding(window));
        // Одно и то же окно может прийти обоими путями (выделено И в созвездии).
        let mut итог: Vec<Window> = Vec::new();
        for w in члены {
            if !same_window(&w, window) && !итог.iter().any(|x| same_window(x, &w)) {
                итог.push(w);
            }
        }
        итог
    }

    /// Остальные окна из "созвездия" данного окна (без самого `window`).
    /// Двигать группой больше не используется (см.
    /// group_drag_members_excluding) — только для операций над самим
    /// созвездием: драг спрашивает у него, всю ли гроздь увезли (move_grab.rs).
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
        let group = self.selected_windows.clone();
        self.clear_constellation_torn(&group);
        tracing::info!("dawn: constellation formed ({} windows)", group.len());
    }

    /// Рамка, в которой собирается гроздь: то место, которое выделенные окна
    /// занимают ПРЯМО СЕЙЧАС, — их общий bbox.
    ///
    /// У halley рамка кластера — это границы поля, то есть монитор целиком:
    /// там кластер и есть отдельное рабочее место. В dawn холст бесконечный и
    /// гроздей на нём может лежать сколько угодно рядом, поэтому «полем» здесь
    /// служит площадь самого выделения: гроздь перестраивается ровно там, где
    /// её обвели, и не отбирает экран у соседей.
    ///
    /// Пол по обеим сторонам — чтобы гроздь из окон-полосок не выродилась.
    fn constellation_bounds(&self, окна: &[Window]) -> Option<Rectangle<i32, Logical>> {
        let mut итог: Option<Rectangle<i32, Logical>> = None;
        for w in окна {
            let Some(g) = self.space.element_geometry(w) else { continue };
            итог = Some(match итог {
                None => g,
                Some(r) => {
                    let x0 = r.loc.x.min(g.loc.x);
                    let y0 = r.loc.y.min(g.loc.y);
                    let x1 = (r.loc.x + r.size.w).max(g.loc.x + g.size.w);
                    let y1 = (r.loc.y + r.size.h).max(g.loc.y + g.size.h);
                    Rectangle::new(Point::from((x0, y0)), Size::from((x1 - x0, y1 - y0)))
                }
            });
        }
        let r = итог?;
        const ПОЛ: i32 = 240;
        if r.size.w >= ПОЛ && r.size.h >= ПОЛ {
            return Some(r);
        }
        // Слишком тесно — раздвигаем от центра, не сдвигая его.
        let w = r.size.w.max(ПОЛ);
        let h = r.size.h.max(ПОЛ);
        Some(Rectangle::new(
            Point::from((r.loc.x + (r.size.w - w) / 2, r.loc.y + (r.size.h - h) / 2)),
            Size::from((w, h)),
        ))
    }

    /// Win+D по выделению: собрать выделенные окна в созвездие — по-halley,
    /// раскладкой «мастер и стопка» (см. constellation.rs).
    ///
    /// **Чем это отличается от прежней сборки.** Раньше окна просто съезжались
    /// в компактную сетку, СОХРАНЯЯ свои размеры: получалась кучка
    /// разнокалиберных плиток с дырами между ними, и «целая» гроздь от
    /// растащенной отличалась на глаз едва-едва. У halley в кластере есть
    /// порядок и роли: мастер занимает 60% ширины во всю высоту, остальные
    /// делят правую колонку. Гроздь от этого читается как одна вещь, а не как
    /// несколько окон, оказавшихся рядом.
    ///
    /// Мастером становится сфокусированное окно, если оно в выделении, иначе
    /// самое большое: у halley мастер — это `члены[0]`, а кто им станет,
    /// решает тот, кто кластер собирает.
    pub fn gather_selected_into_constellation(&mut self) {
        if self.selected_windows.len() < 2 {
            return;
        }
        // Созвездия живут во Float (свободные позиции) — переводим туда, если
        // сейчас тайлинг, и помечаем выделенные окна плавающими.
        if self.tile_config.layout != crate::tiling::Layout::Float {
            self.set_layout(crate::tiling::Layout::Float);
        }

        // Только те, что реально лежат на холсте: у окна без геометрии нет ни
        // места, ни размера, и класть его в гроздь некуда.
        let mut члены: Vec<Window> = self.selected_windows.iter()
            .filter(|w| self.space.element_geometry(w).is_some())
            .cloned()
            .collect();
        if члены.len() < 2 {
            return;
        }

        // Кто мастер. Фокус главнее размера: человек смотрит на то окно,
        // которое ему сейчас нужно крупным.
        let мастер = self.focused_window()
            .filter(|f| члены.iter().any(|w| same_window(w, f)))
            .or_else(|| {
                члены.iter()
                    .filter_map(|w| self.space.element_geometry(w).map(|g| (w.clone(), g)))
                    .max_by_key(|(_, g)| g.size.w as i64 * g.size.h as i64)
                    .map(|(w, _)| w)
            });
        if let Some(m) = мастер {
            crate::constellation::в_мастера(&mut члены, &m);
        }

        let Some(bounds) = self.constellation_bounds(&члены) else { return };
        let места = crate::constellation::раскладка(
            bounds,
            crate::constellation::зазор(),
            члены.len(),
        );

        // Куда окно вернётся при роспуске. Запоминаем ДО переезда и только если
        // ещё не запомнили: собрать уже собранное созвездие (или собрать его
        // заново после переноса) не должно стирать исходное место — иначе
        // «вернуть как было» возвращало бы в предыдущую гроздь.
        //
        // Размер запоминаем ТЕПЕРЬ ТОЖЕ: halley-раскладка окна ещё и ресайзит,
        // а роспуск обязан быть обратной операцией — вернуть одну позицию
        // значило бы оставить окна навсегда в размерах грозди.
        let прежние: Vec<(Window, Point<i32, Logical>, Size<i32, Logical>)> = члены.iter()
            .filter_map(|w| self.space.element_geometry(w).map(|g| (w.clone(), g.loc, g.size)))
            .collect();
        for (w, место, размер) in прежние {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == w) {
                if tw.pre_constellation.is_none() {
                    tw.pre_constellation = Some(место);
                    tw.pre_constellation_size = Some(размер);
                }
            }
        }

        for место in &места {
            let Some(w) = члены.get(место.индекс).cloned() else { continue };
            let pos = место.rect.loc;
            crate::xwin::set_size(&w, Some(место.rect.size), crate::xwin::Tiled::Keep);
            crate::xwin::configure(&w);
            self.animate_window_to_dur(&w, pos, crate::anim::дуг::созвездие());
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == w) {
                tw.floating = true;
                tw.float_position = pos;
                tw.position = pos;
                tw.float_position_set = true;
                tw.float_size = Some(место.rect.size);
            }
        }

        // Фиксируем как созвездие (двигается/ресайзится как целое).
        for w in &члены {
            if let Some(idx) = self.constellation_index_of(w) {
                self.constellations[idx].retain(|x| !same_window(x, w));
            }
        }
        self.constellations.retain(|g| g.len() > 1);
        self.constellations.push(члены.clone());
        // Гроздь снова сложена нами — метка «растащено» снимается, и следующий
        // Win+D по ней будет означать «распустить».
        self.clear_constellation_torn(&члены);
        tracing::info!("dawn: созвездие собрано ({} окон) в {:?}", члены.len(), bounds);
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
    /// Созвездие в выделении «растащено» — его окна двигали руками после
    /// сборки (см. TaggedWindow::constellation_torn).
    ///
    /// По этому и решается, что означает Super+D по готовому созвездию:
    /// растащенное — собрать заново, целое — разобрать.
    pub fn selection_is_torn(&self) -> bool {
        self.selected_windows.iter().any(|w| {
            self.tagged_windows.iter()
                .any(|tw| same_window(&tw.window, w) && tw.constellation_torn)
        })
    }

    /// Пометить созвездие этого окна растащенным. Метка общая на всю группу:
    /// увели одно окно — нарушено расположение всей грозди.
    pub fn mark_constellation_torn(&mut self, window: &Window) {
        let Some(idx) = self.constellation_index_of(window) else { return };
        let group = self.constellations[idx].clone();
        for tw in self.tagged_windows.iter_mut() {
            if group.iter().any(|w| same_window(w, &tw.window)) {
                tw.constellation_torn = true;
            }
        }
    }

    /// Снять метку «растащено» с окон: гроздь только что сложили заново.
    fn clear_constellation_torn(&mut self, group: &[Window]) {
        for tw in self.tagged_windows.iter_mut() {
            if group.iter().any(|w| same_window(w, &tw.window)) {
                tw.constellation_torn = false;
            }
        }
    }

    /// Win+D по собранному созвездию: распустить его.
    ///
    /// Обратная операция сборке, и потому возвращает не только МЕСТО, но и
    /// РАЗМЕР: halley-раскладка грозди окна ресайзит (см.
    /// `gather_selected_into_constellation`), и вернуть одну позицию значило бы
    /// распустить гроздь наполовину — окна разъехались бы, оставшись в чужих
    /// пропорциях.
    ///
    /// Прежнее место известно не всегда: созвездие могло пережить перезапуск
    /// раскладки, или окно попало в него не через сборку. На такой случай
    /// остаётся разлёт от общего центра — не идеально, но лучше, чем оставить
    /// окна стопкой друг на друге.
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

        const SCATTER: f64 = 2.4;
        const MIN_PUSH: f64 = 320.0;
        let count = group.len();
        for (i, w) in group.iter().enumerate() {
            let g = match self.space.element_geometry(w) {
                Some(g) => g,
                None => continue,
            };
            let (было, размер) = self.tagged_windows.iter()
                .find(|tw| &tw.window == w)
                .map(|tw| (tw.pre_constellation, tw.pre_constellation_size))
                .unwrap_or((None, None));
            // Размер возвращаем ПЕРВЫМ: разлёт от центра ниже считается от
            // размеров окна, и посчитать его по старым, а применить новые
            // значило бы промахнуться на разницу.
            if let Some(размер) = размер {
                crate::xwin::set_size(w, Some(размер), crate::xwin::Tiled::Keep);
                crate::xwin::configure(w);
            }
            let итоговый = размер.unwrap_or(g.size);
            let pos = match было {
                Some(p) => p,
                None => {
                    let wcx = g.loc.x as f64 + g.size.w as f64 / 2.0;
                    let wcy = g.loc.y as f64 + g.size.h as f64 / 2.0;
                    let (mut dx, mut dy) = ((wcx - cx) * SCATTER, (wcy - cy) * SCATTER);
                    if dx.hypot(dy) < 1.0 {
                        let a = std::f64::consts::TAU * (i as f64) / (count.max(1) as f64);
                        dx = a.cos() * MIN_PUSH;
                        dy = a.sin() * MIN_PUSH;
                    }
                    Point::from((
                        (cx + dx - итоговый.w as f64 / 2.0).round() as i32,
                        (cy + dy - итоговый.h as f64 / 2.0).round() as i32,
                    ))
                }
            };
            self.animate_window_to_dur(w, pos, crate::anim::дуг::созвездие());
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == w) {
                tw.floating = true;
                tw.float_position = pos;
                tw.position = pos;
                tw.float_position_set = true;
                tw.float_size = Some(итоговый);
                // Созвездия больше нет — и метки «откуда собрали» тоже не
                // нужны: следующая сборка запишет их заново, от текущего места.
                tw.pre_constellation = None;
                tw.pre_constellation_size = None;
                tw.constellation_torn = false;
            }
        }

        self.constellations.retain(|g| g.len() > 1);
        self.clear_selection();
        tracing::info!("dawn: созвездие распущено ({count} окон)");
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Super+Shift+G: распустить созвездие. Окна остаются там, где лежат, —
    /// этим роспуск и отличается от разборки (`scatter_selected_constellation`),
    /// которая ещё и возвращает окна на прежние места.
    ///
    /// **Почему здесь три способа найти созвездие, а не один.** Раньше был
    /// ровно один: взять сфокусированную ПОВЕРХНОСТЬ и поискать окно с ней в
    /// `tagged_windows`. Обе половины этого способа отваливаются походя —
    /// фокуса может не быть вовсе (кликнули по холсту, по панели, окно только
    /// что закрылось), а сфокусированной может оказаться дочерняя поверхность
    /// (меню, popup), которую `is_surface` с окном не сводит. В обоих случаях
    /// функция молча выходила, ничего не делая. Снаружи это ровно та жалоба,
    /// что роспуск «иногда не срабатывает, но срабатывает потом»: потом —
    /// это когда фокус случайно оказался на подходящем окне.
    ///
    /// Поэтому теперь ищем по очереди: выделение (человек его для того и
    /// обвёл), затем фокус, затем — если на текущем столе созвездие ровно
    /// одно — его. Неоднозначности здесь нет: два созвездия на столе без
    /// выделения и фокуса распустить нечем, и мы честно ничего не делаем,
    /// сказав об этом в лог.
    pub fn ungroup_focused_constellation(&mut self) {
        let по_выделению = self.selected_windows.iter()
            .find_map(|w| self.constellation_index_of(w));

        let по_фокусу = || -> Option<usize> {
            let focused = self.focused_surface()?;
            let window = self.tagged_windows.iter()
                .find(|tw| crate::xwin::is_surface(&tw.window, &focused))
                .map(|tw| tw.window.clone())?;
            self.constellation_index_of(&window)
        };

        let единственное_здесь = || -> Option<usize> {
            let tags = self.viewport.current_tags();
            let здесь: Vec<usize> = self.constellations.iter().enumerate()
                .filter(|(_, g)| g.iter().any(|w| {
                    self.tagged_windows.iter()
                        .any(|tw| same_window(&tw.window, w) && (tw.tags == 0 || tw.tags & tags != 0))
                }))
                .map(|(i, _)| i)
                .collect();
            (здесь.len() == 1).then(|| здесь[0])
        };

        let Some(idx) = по_выделению.or_else(по_фокусу).or_else(единственное_здесь) else {
            tracing::info!("dawn: роспуск: созвездия не нашлось (ни в выделении, ни под фокусом)");
            return;
        };

        let group = self.constellations.remove(idx);
        // Метки чистим обязательно. `constellation_torn` пережил бы роспуск и
        // сбил бы Super+D по СЛЕДУЮЩЕМУ созвездию из этих же окон (он решает
        // «собрать или разобрать» именно по ней), а `pre_constellation`
        // отправил бы окна на позиции из прошлой жизни при первой же разборке.
        for tw in self.tagged_windows.iter_mut() {
            if group.iter().any(|w| same_window(w, &tw.window)) {
                tw.constellation_torn = false;
                tw.pre_constellation = None;
                tw.pre_constellation_size = None;
            }
        }
        self.constellations.retain(|g| g.len() > 1);
        tracing::info!("dawn: созвездие распущено на месте ({} окон)", group.len());
        // Роспуск ничего не двигает, но рисует: подсветка принадлежности к
        // грозди пропадает. Без этого запроса кадр не перерисовался бы вовсе,
        // и роспуск снова выглядел бы как «не сработал».
        self.request_redraw();
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
            self.animate_window_to_dur(w, pos, crate::anim::дуг::толчок_соседа());
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
