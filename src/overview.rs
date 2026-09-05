//! niri-подобный обзор рабочих столов (тап Super).
//!
//! Тап по Super открывает/закрывает обзор: рабочие столы (воркспейсы с окнами)
//! раскладываются 2D-сеткой ВОКРУГ ЦЕНТРАЛЬНОГО (текущего) стола, холст
//! отдаляется. В обзоре:
//!  · ОКНА не трогаются — только ПАН (ЛКМ-драг по пустому месту / 2-пальца) и
//!    ЗУМ (колесо);
//!  · САМИ СТОЛЫ не двигаются: ячейку каждому выдаёт обзор (кольцами вокруг
//!    центрального, см. first_free_cell) — перетаскивания столов нет;
//!  · повторный тап Super или ПКМ → выйти на стол ПОД КУРСОРОМ (плавный перелёт);
//!  · LMB-клик по столу → плавный перелёт к нему.
//!
//! В ленте (Layout::Columns, niri) обзор устроен иначе — см. «Обзор ленты»
//! ниже: там столы уже лежат этажами одного холста, поэтому окна не
//! перекладываются вовсе, а камера просто отъезжает и вписывает ленту целиком
//! (как Super+Space, только с точным кадром). Клики, драг окон, зум колесом и
//! выход работают там так же, как в сеточном обзоре.
//!
//! При выходе на ДРУГОЙ стол: камера летит к его ячейке в обзоре, затем
//! финализируется выход (восстановление layout/зума). При выходе на тот же
//! стол → просто выход (без анимации).


use smithay::{
    desktop::Window,
    utils::{Logical, Physical, Point, Rectangle, Size},
};

use crate::anim::CameraAnim;
use crate::state::Parallax;
use crate::tiling::{GAP_INNER, GAP_OUTER, Layout};

/// Зазор между ячейками сетки столов (canvas px).
const BAND_GAP: i32 = 140;
/// Уровень отдаления в обзоре.
const OVERVIEW_ZOOM: f64 = 0.5;
/// Длительность анимации перелёта между столами.

fn same_window(a: &Window, b: &Window) -> bool {
    a == b
}

impl Parallax {
    /// Можно ли СЕЙЧАС войти в обзор. Во Float — нет: там окна и так свободно
    /// разбросаны по бесконечному холсту, камера ходит куда угодно, и обзор
    /// столов ничего не добавляет — зато случайный тап по Super (он ловится при
    /// каждом отпускании Super без другой клавиши) перекладывал окна в сетку
    /// миниатюр и сбивал ручную раскладку. В тайловых режимах обзор, наоборот,
    /// единственный способ увидеть остальные столы — там он разрешён.
    pub fn overview_allowed(&self) -> bool {
        self.tile_config.layout != Layout::Float
    }

    /// Помнит ли стол `tag` ПЛАВАЮЩУЮ раскладку. Правило то же, что у
    /// columns_is_strip_tag: закрепление (стол 3 = Float) сильнее памяти, а у
    /// ТЕКУЩЕГО стола источник правды — живая раскладка.
    ///
    /// Плавающие столы в обзор не попадают ВООБЩЕ (ни рамкой, ни окнами): их
    /// окна разбросаны вручную по бесконечному холсту, в ячейку сетки они
    /// влезают только сжатыми в миниатюры, а на выходе их приходится
    /// восстанавливать снимком — то есть обзор для них не показывает ничего
    /// полезного и только рискует ручной раскладкой.
    pub fn overview_is_float_tag(&self, tag: u32) -> bool {
        match self.tag_layouts.get(&tag) {
            Some(l) => *l == Layout::Float,
            None if tag == self.viewport.current_tags() => {
                self.tile_config.layout == Layout::Float
            }
            None => false,
        }
    }

    pub fn toggle_overview(&mut self) {
        // Если идёт exit-анимация — не трогаем, даём завершиться.
        if self.overview_exit_pending {
            return;
        }
        // Лента (niri): обзор — это отъезд камеры, а не пересборка окон, и
        // выходим мы просто на этаж под курсором (см. exit_overview_strip).
        if self.overview_strip {
            let mask = self.overview_workspace_at(self.pointer_location);
            self.exit_overview_strip(mask, None);
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
            // КУБ: столов на холсте под курсором нет — они на гранях, и
            // `overview_workspace_at` отвечает про клетку сетки, которой в
            // кадре не видно. Уходим на грань под курсором, а если курсор мимо
            // куба — на ту, что смотрит на зрителя. Раньше сюда шла клетка, и
            // тап Super выбрасывал НЕ на тот стол, на котором куб остановлен.
            if self.куб_активен() {
                let mask = self
                    .куб_стол_в_точке(self.pointer_screen_physical())
                    .or_else(|| self.куб_передний_стол())
                    .filter(|&m| m != self.viewport.current_tags());
                // Выход из куба не мгновенный: сначала грань наезжает на
                // экран (см. куб_выход_начать), и уже `anim::tick` доводит
                // дело до конца — вместе со снятием фокуса.
                if self.куб_выход_начать(mask, true) {
                    return;
                }
            }
            let mask = self.overview_workspace_at(self.pointer_location);
            self.exit_overview_immediate(mask);
            self.обзор_снять_фокус();
        } else {
            self.enter_overview();
        }
    }

    /// Выйти из обзора на стол под курсором с плавным перелётом.
    pub fn exit_overview_to_cursor(&mut self) {
        if !self.overview_active || self.overview_exit_pending {
            return;
        }
        // КУБ: столы стоят не на холсте, а на гранях, и «стол под курсором»
        // там значит другое — тот, что смотрит на зрителя. Выходим на него, а
        // камеру возвращаем как была: она всё это время стояла на своём столе
        // (куб рисуется в экранных координатах и камеры не касается).
        if self.куб_активен() {
            let цель = self
                .куб_стол_в_точке(self.pointer_screen_physical())
                .or_else(|| self.куб_передний_стол())
                .filter(|&m| m != self.viewport.current_tags());
            // Наезд на выбранную грань, выход — по его окончании.
            if !self.куб_выход_начать(цель, false) {
                self.exit_overview_immediate(цель);
            }
            return;
        }
        let mask = self.overview_workspace_at(self.pointer_location);
        if self.overview_strip {
            self.exit_overview_strip(mask, None);
            return;
        }

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
            self.camera_anim = Some(CameraAnim::new(from, target_cam, crate::anim::дуг::обзор()));
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
        if self.overview_strip {
            self.exit_overview_strip(switch_to, None);
            return;
        }
        self.overview_active = false;
        self.overview_exit_pending = false;
        self.overview_exit_target_ws = None;
        // Незаконченное закрытие куба тут и кончается: обзора больше нет, а
        // `anim::tick` иначе доиграл бы его и вышел из обзора ВТОРОЙ раз.
        self.куб_выход = None;
        self.куб_масштаб = 1.0;
        self.куб_масштаб_цель = 1.0;

        if let Some((tag, cam_x, cam_y, zoom, layout)) = self.overview_prev.take() {
            self.viewport.tagset[self.viewport.seltags] = tag;
            self.tile_config.layout = layout;
            // Кадр (зум и камеру) возвращаем ПЕРВЫМ делом — до refresh_tags и
            // arrange. Рабочая область тайлинга считается от output_geometry, а
            // та делится на зум: при обзорном зуме 0.45 экран 2560×1080
            // притворяется холстом 5689×2400, и arrange раскладывал окна по
            // нему — второе окно уезжало на x≈4288, третье и четвёртое ещё
            // дальше. Дальше зум возвращался в 1.0, и на экране оставалось одно
            // окно (а то и ни одного), причём эти координаты оседали в
            // tw.position: следующий заход на такой стол по Super+N уже не
            // звал arrange (стол посещён, раскладка та же) и показывал ровно ту
            // же пустоту. Ровно это и выглядело как «столы теряют окна».
            self.viewport.zoom = zoom;
            self.viewport.cam_x = cam_x;
            self.viewport.cam_y = cam_y;
            self.apply_camera();
            self.refresh_tags();
            // ВАЖНО: строго после refresh_tags — он сам переписывает tw.position
            // текущей (обзорной) позицией окна, так что снимок надо накатывать
            // поверх него, а не до.
            self.restore_pre_overview_geometry();
            self.arrange();
        }
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        if let Some(mask) = switch_to {
            self.view_tag(mask);
            // Обзор (overview_layout) сдвигает окна в сетку и сжимает их, а
            // refresh_tags сохраняет эти обзорные позиции в tw.position. Для
            // Tile/Monocle view_tag посещённого стола НЕ вызывает arrange и
            // улетает камерой к устаревшей per-tag позиции → на экране одно
            // мелкое окно под курсором вместо всего стола. Показываем стол
            // ЦЕЛИКОМ как собран: стандартный кадр (zoom 1, камера в углу стола)
            // + переразметка.
            // Columns раскладывается из своей модели прямо в view_tag — не трогаем.
            if matches!(self.tile_config.layout, Layout::Tile | Layout::Monocle) {
                // Угол СВОЕГО монитора, а не ноль холста: стол мог оказаться на
                // втором мониторе (его дом — (1 000 000, 0), см.
                // `monitors::ШАГ_ДОМА`), и нулевая камера показала бы пустоту
                // вместо стола. Дом спрашиваем ПОСЛЕ view_tag: он мог сменить
                // активный монитор вместе со столом.
                let дом = self.монитор_дом();
                self.camera_anim = None;
                self.zoom_anim = None;
                self.zoom_glide = None;
                self.viewport.zoom = 1.0;
                self.viewport.cam_x = дом.x as f64;
                self.viewport.cam_y = дом.y as f64;
                self.apply_camera();
                self.arrange();
            }
        }
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("plx: overview off");
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
        // Тянуть можно и за левый/верхний край — тогда вместе с размером
        // поехала позиция. Обзор показывает окна сдвигом от дома, так что
        // не записанный дом на следующей же переразметке вернул бы окно назад.
        let mask = self.overview_mask_of_window(window);
        let loc = match (mask, self.space.element_geometry(window)) {
            (Some(mask), Some(g)) => Some(g.loc - self.overview_offset(mask)),
            _ => None,
        };
        if let Some(e) = self.overview_saved_geo.iter_mut()
            .find(|(w, _, _)| same_window(w, window))
        {
            e.2 = size;
            if let Some(loc) = loc {
                e.1 = loc;
            }
        }
    }

    /// Физический размер output'а = размер одной ячейки стола.
    pub fn overview_band_size(&self) -> Option<(i32, i32)> {
        let экран = self.screen_size();
        Some((экран.w, экран.h))
    }

    /// Canvas-прямоугольник стола по его слоту (ячейке сетки).
    ///
    /// Сетка строится от ДОМА своего монитора, а не от нуля холста: столы
    /// принадлежат мониторам и лежат в их прямоугольниках (см.
    /// `monitors::ШАГ_ДОМА` и `tiling::screen_area`). Пока здесь стоял голый
    /// ноль, обзор на втором мониторе уводил камеру к столам первого — замер
    /// 26.08.2026: тап Super при камере 1 000 000 давал камеру (−111, −47).
    fn slot_rect(&self, slot: (i32, i32), w: i32, h: i32) -> Rectangle<i32, Logical> {
        let дом = self.монитор_дом();
        Rectangle::new(
            (
                дом.x + slot.0 * (w + BAND_GAP),
                дом.y + slot.1 * (h + BAND_GAP),
            ).into(),
            (w, h).into(),
        )
    }

    /// Первая свободная ячейка сетки — кольцами ВОКРУГ центра (0,0), а не
    /// колонкой вниз: столы должны обступать текущий со всех четырёх сторон
    /// (справа, слева, снизу, сверху), а не выстраиваться в одну полосу.
    /// Порядок внутри кольца — по часовой от правого соседа, так что при 5
    /// столах получается «крест»: центр и по одному соседу с каждой стороны,
    /// и только с шестого стола идут диагонали.
    /// Диапазон ячеек — ±9 (больше девяти столов не бывает).
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
        Some(self.slot_rect(*self.overview_slots.get(&mask).unwrap_or(&(0, 0)), w, h))
    }

    /// Область ВНУТРИ стола, в которой разрешено находиться окну: рамка минус
    /// внешний отступ. За её пределы окно в обзоре не выпускает перетаскивание
    /// (move_grab) — стол работает как жёсткая рамка. Ресайз рамкой НЕ
    /// ограничен: он в обзоре идёт по обычной логике раскладки (см.
    /// resize_grab.rs), а тайловому дереву эта же область служит рабочей.
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
        // В ленте окно тащат свободно: этажи стоят вплотную, и «жёсткая рамка»
        // стола мешала бы перетащить окно на соседний этаж.
        if self.overview_strip {
            return loc;
        }
        let Some(area) = self.overview_window_area(mask) else { return loc };
        let max_x = area.loc.x + (area.size.w - size.w).max(0);
        let max_y = area.loc.y + (area.size.h - size.h).max(0);
        Point::from((
            loc.x.clamp(area.loc.x, max_x),
            loc.y.clamp(area.loc.y, max_y),
        ))
    }

    /// Camera position to center a given slot on screen at current zoom.
    fn center_cam_on_slot(&self, slot: (i32, i32), w: i32, h: i32) -> Point<f64, Logical> {
        self.center_cam_on_slot_zoom(slot, w, h, self.viewport.zoom)
    }

    /// То же, но для ЗАДАННОГО зума: при входе в обзор камера считается под
    /// будущий (обзорный) зум, а не под текущий — на момент расчёта viewport
    /// ещё держит доовзорный (см. enter_overview).
    fn center_cam_on_slot_zoom(&self, slot: (i32, i32), w: i32, h: i32, zoom: f64) -> Point<f64, Logical> {
        let rect = self.slot_rect(slot, w, h);
        let cx = rect.loc.x as f64 + w as f64 / 2.0;
        let cy = rect.loc.y as f64 + h as f64 / 2.0;
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
            let r = self.slot_rect(slot, w, h);
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

    /// Камера обзора: тянем к `над_столом`, но не дальше, чем позволяет
    /// «все столы видны».
    ///
    /// Обе крайности плохи каждая по-своему. Строго над текущим столом —
    /// соседи за краем экрана, хотя зум подобран ровно для того, чтобы они
    /// влезли. Строго по центру сетки — непонятно, с какого стола ты пришёл, и
    /// тем сильнее, чем больше соседей. Поэтому берём первую и зажимаем её в
    /// отрезок допустимых положений камеры.
    ///
    /// Отрезок берётся из геометрии, а не на глаз. Видно ровно `w/zoom`
    /// логических пикселей по горизонтали, значит камера обязана лежать левее
    /// левого края сетки (`cam ≤ min`) и правее «правый край минус экран»
    /// (`cam ≥ max − w/zoom`). Запас между этими границами — то самое поле
    /// 0.92 из `overview_fit_all`, и он же весь бюджет уклона: по одной оси
    /// (той, что упёрлась в подгонку) его почти нет, по другой обычно больше.
    /// Отрезок пуст (сетка не влезает вовсе) — честно возвращаем центр.
    fn обзор_камера_с_уклоном(
        &self,
        над_столом: Point<f64, Logical>,
        центр: Point<f64, Logical>,
        w: i32,
        h: i32,
        zoom: f64,
    ) -> Point<f64, Logical> {
        let (mut min_x, mut min_y) = (i32::MAX, i32::MAX);
        let (mut max_x, mut max_y) = (i32::MIN, i32::MIN);
        for m in &self.overview_order {
            let slot = *self.overview_slots.get(m).unwrap_or(&(0, 0));
            let r = self.slot_rect(slot, w, h);
            min_x = min_x.min(r.loc.x);
            min_y = min_y.min(r.loc.y);
            max_x = max_x.max(r.loc.x + w);
            max_y = max_y.max(r.loc.y + h);
        }
        if min_x > max_x || zoom <= 0.0 {
            return центр;
        }
        // Уклон берёт не весь запас, а ЧЕТВЕРТЬ его с каждой стороны от
        // центра. Иначе он съедает поле 0.92 целиком: замер на харнессе после
        // первой правки — все столы стали видны, но нижний лёг ровно в рамку
        // экрана, без единого пикселя поля. Отрезок ниже симметричен относительно
        // `центр` и вдвое уже полного, так что поле остаётся всегда, а по
        // ограничивающей оси (той, по которой считалась подгонка) уклон
        // получается почти нулевым — там его и негде взять.
        let ось = |к: f64, ц: f64, мин: i32, макс: i32, видно: f64| -> f64 {
            let слабина = видно - (макс - мин) as f64;
            if слабина <= 0.0 {
                return ц;
            }
            к.clamp(ц - слабина * 0.25, ц + слабина * 0.25)
        };
        Point::from((
            ось(над_столом.x, центр.x, min_x, max_x, w as f64 / zoom),
            ось(над_столом.y, центр.y, min_y, max_y, h as f64 / zoom),
        ))
    }

    pub fn enter_overview(&mut self) {
        if self.overview_active || self.overview_exit_pending {
            return;
        }
        if !self.overview_allowed() {
            // Текст отставал от кода: запрет давно переехал с Tile на Float
            // (overview_allowed выше), а сообщение осталось прежним и в логе
            // 20260729_190042 одиннадцать раз врало про «тайловый режим».
            tracing::debug!(
                "plx: overview does not open in the {:?} layout (allowed everywhere except Float)",
                self.tile_config.layout,
            );
            return;
        }
        // Лента (niri) — свой обзор: окна остаются в полосе, отъезжает камера.
        if self.tile_config.layout == Layout::Columns {
            self.enter_overview_strip();
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
        self.overview_strip = false;
        // Куб (если включён) снимает набор граней здесь: дальше он крутится
        // сам по себе, а сетка столов под ним всё равно не показывается.
        if self.lua_config.cube > 0.0 {
            self.куб_войти();
        }
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;

        // Сеточный обзор: каждый стол = ячейка сетки со своими окнами (dwindle),
        // камера вписывает все столы (overview_fit_all). Лента (Columns) сюда не
        // доходит — у неё свой обзор (enter_overview_strip, ветка выше).

        // Занятые столы: текущий + все с окнами, но ТОЛЬКО из СВОЕЙ изоляции.
        //
        // Изоляций две: лента (Columns) и всё остальное — tiling и floating
        // вместе. Ленточные столы в эту сетку не попадают никогда: у ленты свой
        // обзор, её этажи лежат на общем холсте, и выход на такой стол из сетки
        // переключил бы режим целиком — окна доехавшего стола пересобрались бы
        // чужой раскладкой поверх снимка от прежней.
        //
        // А вот Tile, Dwindle и Monocle — соседи по одной изоляции: столы там
        // независимы, и стол, ВЫШЕДШИЙ из ленты (Win+N), обязан оказаться
        // именно здесь. Раньше сравнивались точные раскладки, и обзор тайлового
        // стола не показывал ни monocle-соседей, ни вернувшихся из ленты.
        //
        // ПЛАВАЮЩИЕ столы отсюда исключены совсем (overview_is_float_tag): в
        // обзоре их не должно быть видно ни рамкой, ни окнами.
        let cur = self.viewport.current_tags();
        let cur_layout = self.tile_config.layout;
        let mut order: Vec<u32> = vec![cur];
        // Columns сюда не попадает (у ленты свой обзор), проверка оставлена
        // страховкой: собирать ленточные столы в сетку нельзя.
        if cur_layout != Layout::Columns {
            for i in 0..9u32 {
                let m = 1u32 << i;
                if m == cur || !self.tagged_windows.iter().any(|tw| tw.tags & m != 0) {
                    continue;
                }
                // ТОЛЬКО СВОИ столы. Стол принадлежит монитору
                // (`Parallax::монитор_стола`), а обзор раскладывает окна по ячейкам
                // сетки на холсте СВОЕГО монитора: взяв сюда чужой стол, он
                // физически утаскивал окна соседнего экрана к себе — за миллион
                // пикселей от их дома, — и после выхода они там и оставались.
                // Замер 29.08.2026 (двухмониторный харнесс): удержание Super на
                // мониторе 1 давало «overview on (2 workspaces)», а окно стола 2
                // уезжало из `1000026,26` в `3374,18`. Ничейные столы (ещё не
                // закреплённые) считаем своими — их закрепит первый же заход.
                if self.монитор_стола(m).is_some_and(|i| i != self.активный) {
                    continue;
                }
                // Ленточные (свой обзор) и ПЛАВАЮЩИЕ (обзор им не нужен вовсе,
                // см. overview_is_float_tag) столы в сетку не берём.
                if !self.columns_is_strip_tag(m) && !self.overview_is_float_tag(m) {
                    order.push(m);
                }
            }
        }
        self.overview_order = order.clone();

        // Ячейки сетки ПЕРЕЖИВАЮТ выход из обзора: стол, который в прошлый раз
        // стоял слева от соседа, должен остаться слева и в следующем обзоре.
        // Сохраняем прежнюю ячейку столу, если её не занял кто-то раньше по
        // порядку; остальным выдаём первую свободную.
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
        // Зум берём такой, чтобы столы влезли в кадр целиком. КАМЕРА при этом
        // тянется к ТЕКУЩЕМУ столу — обзор это подъём над тем столом, на
        // котором его открыли, и над серединой сетки непонятно, откуда ты
        // вышел, — но ровно настолько, насколько это не выпускает соседей за
        // край (см. `обзор_камера_с_уклоном`).
        //
        // ОГОВОРКА НУЖНА. Раньше камера вставала строго над текущим столом, и
        // это прямо противоречило зуму: зум подобран под ОБЩИЙ bbox, а кадр
        // показывал один стол посередине — значит всё, что дальше половины
        // экрана от него, гарантированно обрезано. Замер на харнессе (три
        // стола, ячейки (0,0), (2700,0), (0,1220)): зум 0.432 вписывал bbox
        // целиком, но камера уезжала в (1017,−710) вместо (−333,−100), и в
        // кадре оставался один стол, а два соседних резались краями экрана.
        // Ярик 05.09.2026: «баги с размерами» в обзоре.
        let (fit_zoom, fit_cam) = self.overview_fit_all(w, h);
        let cur_slot = *self.overview_slots.get(&cur).unwrap_or(&(0, 0));
        let над_столом = self.center_cam_on_slot_zoom(cur_slot, w, h, fit_zoom);
        let target_cam = self.обзор_камера_с_уклоном(над_столом, fit_cam, w, h, fit_zoom);
        self.viewport.zoom = prev_zoom;
        self.viewport.cam_x = prev_cam.x;
        self.viewport.cam_y = prev_cam.y;
        self.apply_camera();

        // Зум и перелёт — ОДНОЙ анимацией (new_pan), с явно заданной конечной
        // камерой. Раньше здесь стояли две: CameraAnim везла камеру к цели, а
        // ZoomAnim::new считал камеру САМ, из якоря в центре экрана. В anim::tick
        // зум тикает ПОСЛЕ камеры и переписывает cam_x/cam_y своим значением —
        // то есть перелёт к нужному столу молча выбрасывался на каждом кадре, и
        // обзор всегда всплывал над той точкой, где холст был до него. Ровно
        // поэтому камера «не поднималась над текущим столом». Лента этой беды
        // не знала: там изначально new_pan (см. enter_overview_strip).
        self.camera_anim = None;
        self.zoom_anim = Some(crate::anim::ZoomAnim::new_pan(
            prev_cam, target_cam, prev_zoom, fit_zoom,
            crate::anim::дуг::обзор(),
        ));
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("plx: overview on ({} workspaces, fit_zoom={:.3})", order.len(), fit_zoom);
    }

    /// «Домашняя» геометрия окна: где и какого размера оно стоит на СВОЁМ столе,
    /// в координатах холста от (0,0) — там же, где раскладка собирает столы
    /// (см. tiling::screen_area). Источник — снимок, снятый при входе в обзор;
    /// у окна, появившегося уже в обзоре, снимка нет, и он заводится на месте.
    fn overview_home(&mut self, win: &Window) -> (Point<i32, Logical>, Size<i32, Logical>) {
        if let Some((_, loc, size)) = self.overview_saved_geo.iter()
            .find(|(w, _, _)| same_window(w, win))
        {
            return (*loc, *size);
        }
        let loc = self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, win))
            .map(|tw| tw.position)
            .unwrap_or_default();
        let size = win.geometry().size;
        self.overview_saved_geo.push((win.clone(), loc, size));
        (loc, size)
    }

    /// Переписать домашнюю геометрию окна. Зовётся, когда окно ПЕРЕЕХАЛО прямо
    /// в обзоре (бросок на другой стол, обмен местами, ресайз): дом — источник
    /// правды и для показа, и для восстановления на выходе.
    fn overview_set_home(
        &mut self,
        win: &Window,
        loc: Point<i32, Logical>,
        size: Option<Size<i32, Logical>>,
    ) {
        self.overview_home(win); // гарантирует наличие записи
        if let Some(e) = self.overview_saved_geo.iter_mut()
            .find(|(w, _, _)| same_window(w, win))
        {
            e.1 = loc;
            if let Some(s) = size {
                e.2 = s;
            }
        }
    }

    /// Сдвиг, который переносит стол `mask` из дома в его ячейку сетки.
    fn overview_offset(&self, mask: u32) -> Point<i32, Logical> {
        self.overview_band_rect(mask).map(|r| r.loc).unwrap_or_default()
    }

    /// Разложить дерево стола `mask` в ДОМАШНЕЙ рабочей области и записать
    /// результат в дома его окон (плюс отдать клиентам новые размеры).
    /// Так правки тайловой раскладки, сделанные в обзоре, живут в тех же
    /// координатах, что и вне его, — обзору остаётся только сдвиг.
    pub(crate) fn overview_apply_tree(&mut self, mask: u32) {
        let Some(area) = self.tile_work_area() else { return };
        let cfg = self.lua_config.dwindle;
        let rects = match self.dwindle_trees.get_mut(&mask) {
            Some(tree) => {
                tree.recalc(area, &cfg);
                tree.leaf_rects()
            }
            None => return,
        };
        for (win, rect) in rects {
            let loc: Point<i32, Logical> = (
                rect.loc.x + GAP_INNER / 2,
                rect.loc.y + GAP_INNER / 2,
            ).into();
            let size: Size<i32, Logical> = (
                (rect.size.w - GAP_INNER).max(1),
                (rect.size.h - GAP_INNER).max(1),
            ).into();
            crate::xwin::set_size(&win, Some(size), crate::xwin::Tiled::Set);
            crate::xwin::configure(&win);
            self.overview_set_home(&win, loc, Some(size));
        }
    }

    /// Расставляет окна по ячейкам столов — КАК ЕСТЬ.
    ///
    /// Раньше обзор пересобирал каждый стол сам: тайловое дерево пересчитывалось
    /// в узкую полосу ячейки, а стол без дерева (monocle, стол с окнами,
    /// поднятыми в плавающий слой через Super+V) раскладывался запасной
    /// dwindle-цепочкой. То есть обзор показывал не столы, а СВОЮ версию столов:
    /// плавающие окна теряли своё место и вставали плитками наравне с
    /// тайловыми, а monocle притворялся тайлингом.
    ///
    /// Теперь окна не перекладываются и не меняют размер вовсе. Все столы
    /// собираются в одном и том же прямоугольнике холста — экран от (0,0)
    /// (см. tiling::screen_area), поэтому «показать стол в его ячейке» — это
    /// ровно СДВИГ на `overview_offset(mask)`, одинаковый для всех его окон.
    /// Взаимное расположение, пропорции и размеры при этом сохраняются точно, а
    /// миниатюрами столы делает отъезд камеры (OVERVIEW_ZOOM), а не сжатие окон.
    pub fn overview_layout(&mut self, w: i32, h: i32) {
        let _ = (w, h); // ячейку даёт overview_band_rect по слоту стола
        // Снимаем с холста только СВОИ окна. Обзор — событие одного монитора:
        // на соседнем экране в этот же момент открыт обычный стол, и его окна
        // обязаны остаться на месте (см. `refresh_tags` и `видимые_теги`, где
        // ровно та же логика). Пока здесь стояли «все окна», вход в обзор
        // сдувал второй экран в пустые обои.
        let свои: Vec<Window> = self.tagged_windows.iter()
            .filter(|tw| !self.монитор_стола(tw.tags).is_some_and(|i| i != self.активный))
            .map(|tw| tw.window.clone())
            .collect();
        for win in &свои {
            self.space.unmap_elem(win);
        }
        self.window_pos_anims.clear();

        let order = self.overview_order.clone();
        let mut placed: Vec<Window> = Vec::new();
        for mask in order {
            let off = self.overview_offset(mask);
            let wins: Vec<Window> = self.tagged_windows.iter()
                .filter(|tw| tw.tags & mask != 0)
                .map(|tw| tw.window.clone())
                .filter(|win| !placed.iter().any(|p| same_window(p, win)))
                .collect();
            for win in wins {
                let (home, size) = self.overview_home(&win);
                // Кламп нужен только тем, кого руками утащили далеко за пределы
                // экрана своего стола (плавающий слой на бесконечном холсте):
                // иначе такое окно въехало бы в чужую ячейку. Тайловые и так
                // лежат внутри рабочей области.
                let loc = self.overview_clamp_loc(mask, home + off, size);
                // Только маппим для отрисовки; tw.position НЕ трогаем (иначе после
                // выхода refresh_tags вернёт окна в позиции обзора).
                self.space.map_element(win.clone(), loc, false);
                placed.push(win);
            }
        }
    }

    /// Canvas-прямоугольники всех столов обзора вместе с признаком «стол ПУСТ»
    /// (ни одного окна). Пустые рисуются не тёмной карточкой, а тонким контуром
    /// — см. build_overview_bg_elements: в ленте этажи стоят вплотную, и подряд
    /// идущие пустые карточки сливались в одно чёрное пятно под столами.
    pub fn overview_band_rects(&self) -> Vec<(Rectangle<i32, Logical>, bool)> {
        let empty = |m: u32| !self.tagged_windows.iter().any(|tw| tw.tags & m != 0);
        if self.overview_strip {
            return self.overview_order.iter()
                .filter_map(|&m| self.overview_strip_floor_rect(m).map(|r| (r, empty(m))))
                .collect();
        }
        let (w, h) = match self.overview_band_size() {
            Some(s) => s,
            None => return Vec::new(),
        };
        self.overview_order.iter()
            .map(|&m| {
                let r = self.slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
                (r, empty(m))
            })
            .collect()
    }

    /// Маска стола, ПОД которым точка `pos` (canvas). None вне столов.
    /// Обзор для всех раскладок — сетка-бэнды, поэтому всегда по slot_rects.
    pub fn overview_workspace_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        if self.overview_strip {
            return self.overview_strip_workspace_at(pos);
        }
        let (w, h) = self.overview_band_size()?;
        for &m in &self.overview_order {
            let r = self.slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
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
        if self.overview_strip {
            return self.overview_strip_nearest_workspace(pos);
        }
        let (w, h) = self.overview_band_size()?;
        let mut best: Option<u32> = None;
        let mut best_d = f64::MAX;
        for &m in &self.overview_order {
            let r = self.slot_rect(*self.overview_slots.get(&m).unwrap_or(&(0, 0)), w, h);
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
        if in_tree {
            self.overview_apply_tree(mask);
        } else {
            // Окна вне дерева (плавающий слой, monocle) обзор показывает там,
            // где они стоят, — значит и «поменять местами» для них означает
            // обменяться МЕСТАМИ, а не позициями в списке tagged_windows, по
            // которому раньше строилась запасная dwindle-цепочка.
            let (la, _) = self.overview_home(a);
            let (lb, _) = self.overview_home(b);
            self.overview_set_home(a, lb, None);
            self.overview_set_home(b, la, None);
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
        // Деревья живут в ДОМАШНИХ координатах (экран от 0,0) — там же, где их
        // считает раскладка вне обзора. Точку броска, снятую с холста обзора,
        // переводим домой тем же сдвигом, каким обзор показывает стол.
        let Some(area) = self.tile_work_area() else { return };
        let off = self.overview_offset(to);
        let focal = Point::<f64, Logical>::from((
            focal.x - off.x as f64,
            focal.y - off.y as f64,
        ));
        let cfg = self.lua_config.dwindle;
        let tree = self.dwindle_trees.entry(to).or_default();
        tree.recalc(area, &cfg);
        let opening_on = tree.closest_node(focal, Some(window));
        tree.insert(window.clone(), opening_on, focal, area, &cfg, None);
        self.overview_apply_tree(to);
        if let Some(tree) = self.dwindle_trees.get(&from) {
            if !tree.windows().is_empty() {
                self.overview_apply_tree(from);
            }
        }
    }

    /// В обзоре: отпустили перетаскивание окна.
    ///  · бросили на ДРУГОЙ стол → окно переезжает на него (и в его дерево);
    ///  · бросили на СОСЕДА по своему столу → окна меняются местами;
    ///  · бросили на пустое место своего стола → тайловое окно возвращается в
    ///    свой слот раскладки, а плавающее (Super+V) остаётся ровно там, куда
    ///    его положили: обзор показывает стол как есть, а значит и правит его
    ///    как есть.
    /// Стол ищем ближайший (не строгое попадание): иначе перенос срабатывает
    /// «через раз», когда окно отпущено в зазоре между столами.
    pub fn overview_reassign(&mut self, window: &Window) {
        if self.overview_strip {
            self.overview_reassign_strip(window);
            return;
        }
        let dropped = match self.space.element_geometry(window) {
            Some(g) => g.loc,
            None => return,
        };
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
                } else if floating {
                    // Плавающее окно бросили на пустое место своего стола — это
                    // и есть его новое место. Дом считаем от ячейки стола, в
                    // которой оно сейчас лежит.
                    let off = self.overview_offset(target);
                    self.overview_set_home(window, dropped - off, None);
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
                } else {
                    // Плавающее окно переезжает на другой стол «как лежит»:
                    // сохраняем его место ОТНОСИТЕЛЬНО нового стола.
                    let off = self.overview_offset(target);
                    self.overview_set_home(window, dropped - off, None);
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

    /// Выйти из обзора на стол окна `window` и сфокусироваться на нём.
    /// Используется при ЛКМ по окну в обзоре.
    pub fn exit_overview_to_window(&mut self, window: &Window) {
        if !self.overview_active {
            return;
        }
        let mask = self.tagged_windows.iter()
            .find(|tw| same_window(&tw.window, window))
            .map(|tw| tw.tags);
        if self.overview_strip {
            self.exit_overview_strip(mask, Some(window.clone()));
            return;
        }
        self.exit_overview_immediate(mask);
        // Перефокусируемся на этом окне после view_tag
        crate::xwin::focus(self, &window.clone());
    }

    // ── Обзор ленты (niri): камера отъезжает, окна остаются на местах ────────
    //
    // В Columns столы — это этажи ОДНОЙ вертикальной ленты (см. columns.rs), и
    // окна всех ленточных столов и так лежат на холсте. Значит обзор здесь не
    // должен ничего перекладывать: достаточно отъехать камерой так, чтобы в
    // кадр вошла вся лента — это ровно то, что делает Super+Space (zoom-nav),
    // только с точным вписыванием. Обзорные фишки при этом остаются: клик по
    // окну — выход на него, клик по этажу — выход на этот стол, Super+ЛКМ —
    // перетаскивание окна между колонками и этажами, ПКМ/тап Super — выход.

    /// Этажи, которые показывает ленточный обзор: вся лента столов — занятые
    /// плюс один пустой снизу (niri-модель динамических воркспейсов, см.
    /// niri_ws_count). Пустой этаж тоже нужен: на него бросают окно, чтобы
    /// завести новый стол.
    ///
    /// ЧУЖИЕ столы (те, что помнят НЕ ленточную раскладку — Tile/Dwindle/Float)
    /// в ленточный обзор не попадают вовсе. Их окна на холсте и так не лежат
    /// (см. refresh_tags: в Columns видимы только ленточные столы), так что
    /// рамка такого этажа рисовалась пустой дырой в ленте, а клик по ней
    /// выбрасывал из ленты в чужой режим.
    fn overview_strip_tags(&self) -> Vec<u32> {
        // columns_strip_order уже отсеивает чужие столы, а niri_ws_count меряет
        // ленту в ЭТАЖАХ (занятые + один пустой снизу) — значит первые n этажей
        // порядка и есть вся лента, дырок от чужих тегов в ней нет.
        let n = self.niri_ws_count().clamp(1, 9) as usize;
        let mut tags: Vec<u32> = self.columns_strip_order().into_iter().take(n).collect();
        if tags.is_empty() {
            tags.push(self.viewport.current_tags());
        }
        tags
    }

    /// Прямоугольник этажа `tag` на холсте: во всю ширину его полосы.
    pub fn overview_strip_floor_rect(&self, tag: u32) -> Option<Rectangle<i32, Logical>> {
        let (w, h) = self.overview_band_size()?;
        let y = self.columns_ws_y(tag).round() as i32;
        // Полоса этажа начинается в доме СВОЕГО монитора, а не в нуле холста:
        // на втором мониторе рамка иначе тянулась бы через весь шаг между
        // домами (см. monitors::ШАГ_ДОМА).
        let дом_x = self.монитор_дом().x;
        let mut min_x = дом_x;
        let mut max_x = дом_x + w;
        for tw in self.tagged_windows.iter().filter(|tw| tw.tags & tag != 0) {
            if let Some(g) = self.space.element_geometry(&tw.window) {
                min_x = min_x.min(g.loc.x);
                max_x = max_x.max(g.loc.x + g.size.w + GAP_OUTER);
            }
        }
        Some(Rectangle::new(
            (min_x, y).into(),
            ((max_x - min_x).max(1), h).into(),
        ))
    }

    /// Этаж, на который попала точка `pos` (по Y — этажи идут ровно по высоте
    /// экрана, без зазоров).
    fn overview_strip_workspace_at(&self, pos: Point<f64, Logical>) -> Option<u32> {
        let (_, h) = self.overview_band_size()?;
        self.overview_order.iter().copied().find(|&m| {
            let y = self.columns_ws_y(m);
            pos.y >= y && pos.y < y + h as f64
        })
    }

    /// Ближайший этаж по Y — для драга, когда окно отпустили над самым краем.
    fn overview_strip_nearest_workspace(&self, pos: Point<f64, Logical>) -> Option<u32> {
        let (_, h) = self.overview_band_size()?;
        self.overview_order.iter().copied().min_by(|&a, &b| {
            let d = |m: u32| (pos.y - (self.columns_ws_y(m) + h as f64 / 2.0)).abs();
            d(a).total_cmp(&d(b))
        })
    }

    fn enter_overview_strip(&mut self) {
        let Some((w, h)) = self.overview_band_size() else { return };
        // Обзор показывает ленту целиком, поэтому съехавшие этажи (состав ленты
        // менялся, пока на них не смотрели) надо поправить ДО замера кадра —
        // иначе в bbox попадёт пустота от этажа, стоящего не на своём месте.
        self.columns_relayout_strip();
        let prev_cam = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let prev_zoom = self.viewport.zoom;

        self.overview_prev = Some((
            self.viewport.current_tags(),
            prev_cam.x,
            prev_cam.y,
            prev_zoom,
            self.tile_config.layout,
        ));
        self.overview_active = true;
        self.overview_strip = true;
        // Ленточный обзор окна не двигает — снимок геометрии и ячейки сетки
        // (это про сеточный обзор) здесь не нужны и не должны остаться от
        // прошлого захода.
        self.overview_saved_geo.clear();
        self.overview_slots.clear();
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        self.overview_order = self.overview_strip_tags();

        // Кадр обзора: вся лента с небольшим полем по краям. Берём и габариты
        // колонок, и рамки этажей — иначе пустой этаж снизу (куда бросают окно
        // ради нового стола) остался бы за кадром.
        let mut bbox = self.columns_strip_bbox()
            .unwrap_or_else(|| Rectangle::new(self.монитор_дом(), (w, h).into()));
        for m in self.overview_order.clone() {
            if let Some(r) = self.overview_strip_floor_rect(m) {
                bbox = bbox.merge(r);
            }
        }
        let zoom = (0.92
            * (w as f64 / bbox.size.w as f64).min(h as f64 / bbox.size.h as f64))
            .clamp(0.1, 1.0);
        let cx = bbox.loc.x as f64 + bbox.size.w as f64 / 2.0;
        // Камера поднимается над ТЕКУЩИМ этажом, а не над серединой ленты: как
        // и в сеточном обзоре, вход — это подъём над своим столом (по вертикали
        // этажи и различаются). По горизонтали центрируем ленту целиком.
        let cur = self.viewport.current_tags();
        let cy = self.overview_strip_floor_rect(cur)
            .map(|r| r.loc.y as f64 + r.size.h as f64 / 2.0)
            .unwrap_or(bbox.loc.y as f64 + bbox.size.h as f64 / 2.0);
        let to_cam = Point::from((cx - w as f64 / (2.0 * zoom), cy - h as f64 / (2.0 * zoom)));

        self.zoom_anim = Some(crate::anim::ZoomAnim::new_pan(
            prev_cam, to_cam, prev_zoom, zoom, crate::anim::дуг::обзор(),
        ));
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!(
            "plx: overview (ribbon) on ({} rows, zoom={:.3})",
            self.overview_order.len(), zoom,
        );
    }

    /// Выход из ленточного обзора: возвращаем прежний зум и приземляемся на
    /// стол `switch_to` (или на текущий), при `focus_win` — прямо на это окно.
    fn exit_overview_strip(&mut self, switch_to: Option<u32>, focus_win: Option<Window>) {
        if !self.overview_active {
            return;
        }
        self.overview_active = false;
        self.overview_strip = false;
        self.overview_exit_pending = false;
        self.overview_exit_target_ws = None;
        self.momentum.stop();

        let cur_zoom = self.viewport.zoom;
        let cur_cam = Point::from((self.viewport.cam_x, self.viewport.cam_y));
        let prev = self.overview_prev.take();
        let prev_zoom = prev.map(|p| p.3).unwrap_or(1.0);
        let prev_cam = prev
            .map(|p| Point::from((p.1, p.2)))
            .unwrap_or(cur_cam);

        // ВАЖНО: сначала возвращаем НОРМАЛЬНЫЙ кадр (зум и камеру, с которыми
        // заходили), и только потом переключаем стол и подтягиваем прокрутку.
        // Вся арифметика ленты (columns_fit_view и т.п.) меряет экран в
        // canvas-единицах — при обзорном зуме он «шире» настоящего, и цели
        // прокрутки посчитались бы не туда.
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        self.viewport.zoom = prev_zoom;
        self.viewport.cam_x = prev_cam.x;
        self.viewport.cam_y = prev_cam.y;
        // Плавающие окна ленты держатся экрана и едут за дельтой камеры
        // (columns_pin_floating). Обзор камеру уже увёз, поэтому базу дельты
        // переставляем на восстановленный кадр — иначе первый же apply_camera
        // после обзора швырнул бы плавающие окна на всю эту дельту.
        self.columns_float_cam = (prev_cam.x, prev_cam.y);
        // И СРАЗУ применяем (строго ПОСЛЕ columns_float_cam — apply_camera тянет
        // за собой плавающие окна): apply_camera выставляет output'у scale = зум,
        // а вся арифметика ленты ниже (arrange в view_tag,
        // columns_scroll_to_active) меряет экран через ЛОГИЧЕСКУЮ геометрию
        // output'а. Без этого вызова она считалась бы по обзорному зуму — экран
        // «шире» втрое, колонки раскладываются на несуществующую ширину, а цель
        // прокрутки уезжает мимо.
        self.apply_camera();

        if let Some(mask) = switch_to {
            if mask != self.viewport.current_tags() {
                self.view_tag(mask);
            }
        }
        if let Some(win) = focus_win {
            self.columns_set_active_to_window(&win);
            crate::xwin::focus(self, &win);
        }
        self.columns_scroll_to_active();
        // Куда в итоге едет лента: цель уже поставленных анимаций (перелёт на
        // этаж + прокрутка к активной колонке) — их мы забираем себе.
        let to_x = self.camera_anim.take().map(|a| a.to.x).unwrap_or(self.viewport.cam_x);
        let to_cam = Point::from((to_x, self.columns_cur_y()));

        // Обратно к столу — одним движением: зум и камера едут вместе.
        self.viewport.zoom = cur_zoom;
        self.viewport.cam_x = cur_cam.x;
        self.viewport.cam_y = cur_cam.y;
        self.columns_float_cam = (cur_cam.x, cur_cam.y);
        self.zoom_anim = Some(crate::anim::ZoomAnim::new_pan(
            cur_cam, to_cam, cur_zoom, prev_zoom, crate::anim::дуг::обзор(),
        ));
        self.apply_camera();
        self.request_plane_reset();
        self.request_redraw();
        tracing::info!("plx: overview (ribbon) off");
    }

    /// Отпустили перетаскивание окна в ленточном обзоре: окно встаёт в полосу
    /// того этажа, над которым его бросили — новой колонкой или в стопку, как
    /// показывала бы подсказка вставки в niri.
    fn overview_reassign_strip(&mut self, window: &Window) {
        let Some(g) = self.space.element_geometry(window) else { return };
        let pos = Point::<f64, Logical>::from((
            (g.loc.x + g.size.w / 2) as f64,
            (g.loc.y + g.size.h / 2) as f64,
        ));
        let Some(target) = self.overview_strip_workspace_at(pos)
            .or_else(|| self.overview_strip_nearest_workspace(pos))
        else {
            return;
        };
        // Полоса пересобирается сама и тянет за собой прокрутку/фокус — а обзор
        // обязан остаться в своём кадре, поэтому камеру возвращаем как была.
        let cam = (self.viewport.cam_x, self.viewport.cam_y, self.viewport.zoom);
        self.columns_drop_window_on_ws(window, target, pos.x);
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        self.viewport.cam_x = cam.0;
        self.viewport.cam_y = cam.1;
        self.viewport.zoom = cam.2;
        self.apply_camera();
        // Этажи могли появиться/опустеть — пересчитываем набор рамок.
        self.overview_order = self.overview_strip_tags();
        self.request_plane_reset();
        self.request_redraw();
    }
}

// ── Куб рабочих столов ───────────────────────────────────────────────────────
//
// Здесь только РЕЖИМ куба: какие столы на гранях, куда он повёрнут и когда
// рисуется. Сама геометрия — в `куб_math.rs`, отрисовка — в `udev.rs`
// (`build_cube_elements`), и обе половины одинаково доступны обеим сборкам:
// без фичи `shaders` шейдера просто нет, и куб не включается ни при каких
// настройках.

impl Parallax {
    /// Рисуется ли сейчас куб вместо обычного кадра.
    ///
    /// Два случая: обзор столов (тап Super) и проворот на соседний стол
    /// (Super+PgUp/PgDn при `set{ cube_switch = true }`). Ленточный обзор
    /// (Layout::Columns) куба не получает: там столы — этажи одного холста, и
    /// «по кругу» они не ходят.
    pub fn куб_активен(&self) -> bool {
        if self.lua_config.cube <= 0.0 || self.куб_шейдер.is_none() {
            return false;
        }
        self.куб_переход || (self.overview_active && !self.overview_strip)
    }

    /// Прямоугольник стола на холсте — то, что ляжет на грань.
    ///
    /// В обзоре у каждого стола своя ячейка сетки. Вне обзора столы лежат ДРУГ
    /// НА ДРУГЕ — это теги, а не места, — и рамка у всех одна: домашний экран
    /// монитора. Куб для того при переключении и нужен: он разносит по граням
    /// то, что на холсте занимает одно и то же место.
    pub fn куб_рамка_стола(&self, tag: u32) -> Option<Rectangle<i32, Logical>> {
        if self.overview_active && !self.overview_strip {
            self.overview_band_rect(tag)
        } else {
            self.screen_area()
        }
    }

    /// Сколько у куба ГРАНЕЙ. Не сколько столов — столов в кольце сколько
    /// угодно (см. `куб_math::стол_грани`).
    pub fn куб_граней(&self) -> u32 {
        self.lua_config.cube_faces.clamp(3, 12)
    }

    /// Столы В КОЛЬЦЕ: те, у кого есть окна, плюс текущий.
    ///
    /// Порядок — по номеру бита маски, а не по `overview_order`: тот кладёт
    /// столы кольцами вокруг текущего (для сетки это правильно), а на кубе
    /// соседняя грань обязана быть СОСЕДНИМ столом — иначе Super+PgDn крутит
    /// куб в случайную сторону.
    pub fn куб_список_столов(&self) -> Vec<u32> {
        let текущий = self.viewport.current_tags();
        let mut столы: Vec<u32> = (0..32u32)
            .map(|б| 1u32 << б)
            .filter(|&m| {
                m == текущий
                    || self.tagged_windows.iter().any(|tw| tw.tags & m != 0)
            })
            .collect();
        // Ленточные и плавающие столы на грани не ставим по той же причине,
        // по которой они не попадают в сетку обзора (см. overview_is_float_tag).
        столы.retain(|&m| !self.columns_is_strip_tag(m) && !self.overview_is_float_tag(m));
        if !столы.contains(&текущий) {
            столы.push(текущий);
            столы.sort_unstable();
        }
        // В кольце не меньше столов, чем у куба граней.
        //
        // Это не украшательство: иначе один и тот же стол попал бы сразу на
        // две грани — и ближнюю, и соседнюю, — а поворот перестал бы что-либо
        // менять. Недостающие берём следующими свободными тегами: пустой стол
        // на грани честно показывает, что он пуст, и на него можно уйти.
        let нужно = self.куб_граней() as usize;
        if столы.len() < нужно {
            for б in 0..32u32 {
                let m = 1u32 << б;
                if столы.len() >= нужно {
                    break;
                }
                if !столы.contains(&m)
                    && !self.columns_is_strip_tag(m)
                    && !self.overview_is_float_tag(m)
                {
                    столы.push(m);
                }
            }
            столы.sort_unstable();
        }
        столы
    }

    /// Снять набор граней и повернуть куб текущим столом к зрителю.
    ///
    /// Список фиксируется на входе в куб и дальше не пересчитывается: стол,
    /// опустевший посреди поворота, иначе исчез бы вместе со своей гранью, и
    /// куб дёрнулся бы под руками.
    pub fn куб_войти(&mut self) {
        self.куб_войти_со_столом(None);
    }

    /// То же, но с гарантией, что стол `ещё` в кольце есть.
    ///
    /// Нужно при переходе на ПУСТОЙ стол: в кольцо идут столы с окнами плюс
    /// текущий, и стола, на который мы как раз собрались, там может не быть.
    /// Раньше это кончалось тихим ничем — `Super+PgDn` на пустой стол оставлял
    /// куб стоять, и дальше он показывал уже не тот стол, на котором мы есть
    /// (поймано на харнессе: кольцо [1..7,9], переход на 8 — куб замер на 7).
    pub fn куб_войти_со_столом(&mut self, ещё: Option<u32>) {
        self.куб_столы = self.куб_список_столов();
        if let Some(m) = ещё {
            if !self.куб_столы.contains(&m) {
                self.куб_столы.push(m);
                self.куб_столы.sort_unstable();
            }
        }
        // Зум и незаконченный драг — состояние ОДНОГО захода: куб, оставшийся
        // отодвинутым с прошлого раза, при провороте столов выглядел бы
        // случайной сменой размера экрана.
        //
        // ОТДАЛЕНИЕ. Масштаб начинается не с единицы, а с такого, при котором
        // передняя грань занимает экран ЦЕЛИКОМ, и едет к единице — это и есть
        // вход в куб: рабочий стол отъезжает от зрителя и оказывается гранью.
        // Шва на стыке нет: при нулевом повороте передняя грань стоит строго
        // перпендикулярно взгляду, её глубина постоянна, затемнение `cube_shade`
        // на ней ровно нулевое — то есть первый кадр анимации попиксельно тот
        // же кадр, что был до нажатия.
        self.куб_масштаб = self.куб_масштаб_вход();
        self.куб_масштаб_цель = 1.0;
        self.куб_выход = None;
        self.куб_драг = None;
        let угол = self.куб_угол_стола(self.viewport.current_tags()).unwrap_or(0.0);
        self.куб_угол = угол;
        self.куб_цель = угол;
    }

    /// Шаг поворота: один поворот на грань.
    fn куб_шаг(&self) -> f64 {
        std::f64::consts::TAU / self.куб_граней() as f64
    }

    /// На какой угол надо повернуть куб, чтобы стол `tag` смотрел на зрителя.
    ///
    /// Зовётся только на ВХОДЕ в куб: угол здесь абсолютный, от нулевого
    /// оборота. Доворот с текущего места идёт другим путём — через
    /// `куб_шагов_до`, потому что кольцо столов длиннее круга граней и «тот же
    /// угол плюс полный оборот» — это уже ДРУГОЙ стол.
    pub fn куб_угол_стола(&self, tag: u32) -> Option<f64> {
        let i = self.куб_столы.iter().position(|&m| m == tag)?;
        Some(-(i as f64) * self.куб_шаг())
    }

    /// Сколько столов ВПЕРЁД по кольцу от текущей цели до стола `tag`.
    ///
    /// Ближним путём: столов может быть десять, а граней четыре, и «ближе» тут
    /// считается по кольцу СТОЛОВ, а не по кругу поворота.
    ///
    /// Знак — в столах, не в углах: шаг вперёд УМЕНЬШАЕТ угол на шаг грани
    /// (угол грани `i` равен −i·шаг, см. `куб_угол_стола`), и вызывающий обязан
    /// вычитать. Ровно на этом знаке куб и уехал на харнессе: столы шли вперёд,
    /// а куб крутился назад, и через три перехода передняя грань показывала
    /// стол, на котором мы были два переключения назад.
    pub fn куб_шагов_до(&self, tag: u32) -> Option<f64> {
        let столов = self.куб_столы.len() as i64;
        if столов == 0 {
            return None;
        }
        let j = self.куб_столы.iter().position(|&m| m == tag)?;
        Some(crate::куб::шагов_до(
            crate::куб::оборот(self.куб_цель, self.куб_граней()),
            j,
            столов as usize,
        ) as f64)
    }

    /// Стол на грани `грань` при текущем повороте.
    pub fn куб_стол_грани(&self, грань: u32) -> Option<u32> {
        if self.куб_столы.is_empty() {
            return None;
        }
        let i = crate::куб::стол_грани(
            crate::куб::оборот(self.куб_угол, self.куб_граней()),
            грань,
            self.куб_граней(),
            self.куб_столы.len(),
        );
        Some(self.куб_столы[i])
    }

    /// Стол на грани, которая сейчас смотрит на зрителя.
    pub fn куб_передний_стол(&self) -> Option<u32> {
        let оборот = crate::куб::оборот(self.куб_угол, self.куб_граней());
        let передний = оборот.rem_euclid(self.куб_граней() as i64) as u32;
        self.куб_стол_грани(передний)
    }

    /// Довернуть куб на `шагов` граней (знак задаёт сторону).
    ///
    /// Предела нет: у бесконечного куба кольцо столов не кончается, и колесо
    /// в обзоре крутит его столько, сколько его крутят.
    pub fn куб_повернуть(&mut self, шагов: f64) {
        if self.куб_столы.is_empty() {
            return;
        }
        self.куб_цель += шагов * self.куб_шаг();
        self.request_redraw();
    }

    /// Зум куба: колесо в обзоре отодвигает и приближает ЕГО.
    ///
    /// Не камеру: на кубе холст не виден вовсе (см. `build_cube_elements`), и
    /// щелчок колеса, двигавший `viewport.zoom`, менял только невидимое —
    /// снаружи это выглядело как «куб не даёт зумить». Меняем настоящую ручку
    /// размера — долю экрана под передней гранью (`cube_fill`).
    ///
    /// Предел снизу — чтобы куб не схлопнулся в точку, сверху — чтобы передняя
    /// грань не вылезла за экран настолько, что соседних не останется видно.
    /// Масштаб, при котором передняя грань занимает экран ЦЕЛИКОМ.
    ///
    /// Это крайняя точка обеих анимаций «куб появился» и «куб убрался»:
    /// заполнение при нём равно единице, то есть грань ровно по ширине экрана.
    /// Потолок 4.0 — не вкус, а предел `Куб::новый`: долю там зажимают этим же
    /// числом, и просить больше значит просить масштаб, которого не будет.
    fn куб_масштаб_вход(&self) -> f64 {
        (1.0 / self.lua_config.cube_fill.max(0.05) as f64).clamp(1.0, 4.0)
    }

    /// Зум куба: колесо в обзоре отодвигает и приближает ЕГО.
    ///
    /// Двигаем ЦЕЛЬ, а не сам масштаб: щелчок колеса — событие, а зум глазами
    /// человека — движение, и раньше куб на каждый щелчок менял размер рывком,
    /// в одном кадре. Догоняет цель `куб_тик`, тем же экспоненциальным
    /// сближением, что доворачивает поворот.
    pub fn куб_зум(&mut self, множитель: f64) {
        self.куб_масштаб_цель = (self.куб_масштаб_цель * множитель).clamp(0.35, 2.2);
        self.request_redraw();
    }

    /// Доля экрана под передней гранью с учётом зума — то, что уходит в
    /// геометрию куба вместо голого `cube_fill`.
    pub fn куб_заполнение(&self) -> f32 {
        (self.lua_config.cube_fill as f64 * self.куб_масштаб) as f32
    }

    /// Куб под курсором: геометрия ровно та же, по которой он рисуется.
    ///
    /// Единицы — ЛОГИЧЕСКИЕ пиксели экрана, а рисуется куб в физических. Это
    /// не расхождение: все длины куба (радиус, фокус, расстояние до камеры,
    /// ось) заданы в долях стороны, и общий множитель в обратной проекции
    /// сокращается — грань под точкой выходит та же.
    fn куб_геометрия(&self) -> Option<crate::куб::Куб> {
        if self.куб_столы.len() < 2 {
            return None;
        }
        let экран = self.screen_size();
        let mut куб = crate::куб::Куб::новый(
            self.куб_граней(),
            экран.w as f32,
            self.lua_config.cube_focal,
            self.куб_заполнение(),
            (экран.w as f32 * 0.5, экран.h as f32 * 0.5),
            0.0,
        );
        куб.поворот = self.куб_угол as f32;
        Some(куб)
    }

    /// Стол на грани под курсором. `None` — курсор мимо куба (в пустоту рядом).
    ///
    /// Грани обходим от ближней к дальней и берём ПЕРВОЕ попадание: у ребра
    /// проекции соседних граней сходятся, и без порядка ответ зависел бы от
    /// номера стола.
    pub fn куб_стол_в_точке(&self, экранная: Point<f64, Physical>) -> Option<u32> {
        let куб = self.куб_геометрия()?;
        let экран = self.screen_size();
        let (пол_ш, пол_в) = (экран.w as f32 * 0.5, экран.h as f32 * 0.5);
        let точка = (экранная.x as f32, экранная.y as f32);
        let mut грани: Vec<u32> = (0..self.куб_граней()).filter(|&i| куб.видна(i)).collect();
        грани.sort_by(|&a, &b| {
            let к = |i: u32| куб.угол(i).cos();
            к(b).partial_cmp(&к(a)).unwrap_or(std::cmp::Ordering::Equal)
        });
        for i in грани {
            let Some((u, v)) = куб.обратно(куб.угол(i), точка) else { continue };
            if u.abs() <= пол_ш && v.abs() <= пол_в {
                return self.куб_стол_грани(i);
            }
        }
        None
    }

    /// Начало драга: куб хватают за грань и крутят рукой.
    pub fn куб_драг_начать(&mut self) {
        self.куб_драг = Some((0.0, self.куб_угол, 0.0));
    }

    /// Протяжка: поворот идёт ЗА рукой — грань, за которую взялись, едет в
    /// сторону движения. Полный оборот примерно за две ширины экрана.
    ///
    /// Считаем по НАКОПЛЕННОЙ дельте, а не по позиции стрелки: во время драга
    /// курсор стоит на месте (крутится куб, а не ходит указатель), и разница
    /// «где стрелка сейчас минус где взялись» осталась бы нулевой.
    pub fn куб_драг_движение(&mut self, dx: f64) -> bool {
        let Some((накоплено, старт_угол, путь)) = self.куб_драг else { return false };
        let накоплено = накоплено + dx;
        let ширина = self.screen_size().w.max(1) as f64;
        let угол = старт_угол + накоплено * std::f64::consts::TAU / (ширина * 2.0);
        self.куб_угол = угол;
        self.куб_цель = угол;
        self.куб_драг = Some((накоплено, старт_угол, путь + dx.abs()));
        self.request_redraw();
        true
    }

    /// Конец драга. `true` — это был КЛИК (курсор почти не сдвинулся), и
    /// вызывающий уходит на грань под ним; иначе куб доворачивается к
    /// ближайшей грани.
    pub fn куб_драг_кончить(&mut self) -> bool {
        let Some((_, _, путь)) = self.куб_драг.take() else { return false };
        if путь < 6.0 {
            return true;
        }
        if !self.куб_столы.is_empty() {
            let шаг = self.куб_шаг();
            self.куб_цель = (self.куб_угол / шаг).round() * шаг;
            self.request_redraw();
        }
        false
    }

    /// Довести поворот к цели. Зовётся из `anim::tick`; `true` — куб ещё едет.
    ///
    /// Экспоненциальное сближение, а не пружина: у куба нет «перелёта», грань
    /// обязана встать ровно лицом, иначе на ней не поработать.
    pub fn куб_тик(&mut self, dt: f64) -> bool {
        // 12 — темп сближения: половина пути за ~58 мс, весь поворот
        // укладывается в четверть секунды и при этом не выглядит рывком.
        // Масштаб идёт тем же темпом НАРОЧНО: на выходе из куба поворот к
        // выбранной грани и наезд на неё — одно движение, и разойдись они по
        // скорости, грань доезжала бы до зрителя уже развёрнутой (или
        // наоборот), что читается как два разных перехода подряд.
        let темп = (1.0 - (-12.0 * dt / self.lua_config.anim_speed.max(0.05)).exp()).clamp(0.0, 1.0);

        let mut едет = false;

        let разница = self.куб_цель - self.куб_угол;
        // Порог 1e-3 радиана, а не 1e-4: это 0.06°, на грани шириной в экран —
        // меньше десятой пикселя, то есть невидимо. Прежнее значение растягивало
        // хвост экспоненты ещё на 0.2 с (замер на харнессе: угол шёл −1.563 →
        // −1.571 четыре кадра), и при переключении стола эта задержка ложилась
        // ПЕРЕД обратным наездом — то есть удваивалась.
        if разница.abs() < 1e-3 {
            self.куб_угол = self.куб_цель;
        } else {
            self.куб_угол += разница * темп;
            едет = true;
        }

        // Порог у масштаба свой: он безразмерная доля около единицы, и 1e-4
        // радиана и 1e-4 доли — разные величины. 1e-3 доли от `cube_fill`
        // это меньше пикселя на грани.
        let dм = self.куб_масштаб_цель - self.куб_масштаб;
        if dм.abs() < 1e-3 {
            self.куб_масштаб = self.куб_масштаб_цель;
        } else {
            self.куб_масштаб += dм * темп;
            едет = true;
        }

        if едет {
            self.request_redraw();
        }
        едет
    }

    /// Начать ЗАКРЫТИЕ куба: доворот к нужной грани и наезд на неё во весь
    /// экран. Настоящий выход из обзора сделает `anim::tick`, когда доиграет.
    ///
    /// `false` — куба нет, и вызывающему надо выходить как раньше, сразу.
    pub fn куб_выход_начать(&mut self, стол: Option<u32>, сбросить_фокус: bool) -> bool {
        if !self.куб_активен() || self.куб_выход.is_some() {
            // `is_some` — не лишняя осторожность: Super нажимают дважды подряд
            // чаще, чем кажется, а второй заход перевёл бы цель поворота на
            // грань, которая к тому моменту уже уехала.
            return self.куб_выход.is_some();
        }
        // Доворачиваем к грани стола, на который уходим: наезжать надо на ту
        // грань, которая и станет экраном. По кольцу СТОЛОВ, а не по кругу
        // граней — ровно та же арифметика, что у проворота (см. куб_шагов_до).
        if let Some(m) = стол {
            if let Some(шагов) = self.куб_шагов_до(m) {
                self.куб_цель -= шагов * self.куб_шаг();
            }
        } else {
            // Остаёмся на своём столе: доворачиваем к БЛИЖАЙШЕЙ грани, иначе
            // наезд пошёл бы на куб, стоящий ребром, и в последнем кадре
            // анимации на экране была бы половина одного стола и половина
            // другого.
            let шаг = self.куб_шаг();
            self.куб_цель = (self.куб_угол / шаг).round() * шаг;
        }
        self.куб_масштаб_цель = self.куб_масштаб_вход();
        self.куб_выход = Some(crate::state::КубВыход { стол, сбросить_фокус });
        self.request_redraw();
        true
    }

    /// Доиграл ли куб закрытие — и если да, доделать выход по-настоящему.
    ///
    /// Зовётся из `anim::tick` ровно тогда, когда `куб_тик` сказал «встал».
    /// Здесь две развязки одного механизма: проворот на соседний стол
    /// (`куб_переход`) закрытием ЗАКАНЧИВАЕТСЯ и обзора не касается, а выход
    /// из обзора закрытием заканчивается тоже, но следом гасит обзор.
    pub fn куб_закрытие_доиграло(&mut self) {
        let Some(выход) = self.куб_выход.take() else { return };
        if self.куб_переход {
            self.куб_переход = false;
            return;
        }
        self.exit_overview_immediate(выход.стол);
        if выход.сбросить_фокус {
            self.обзор_снять_фокус();
        }
    }

    /// Снять фокус и активность со всех окон.
    ///
    /// Тап Super показывает стол ЦЕЛИКОМ, как он разложен, без выделенного
    /// окна: иначе sloppy focus сразу после выхода поднимает окно под
    /// курсором, и вместо «вот весь стол» получается «вот одно окно».
    pub fn обзор_снять_фокус(&mut self) {
        if let Some(kb) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            kb.set_focus(self, None, serial);
        }
        let all: Vec<_> = self.space.elements().cloned().collect();
        for w in all {
            w.set_activated(false);
            crate::xwin::configure(&w);
        }
    }
}

impl Parallax {
    /// Начать проворот куба на стол `цель` (переключение вне обзора).
    ///
    /// Ничего не делает, если куб выключен, если проворот отключён
    /// (`set{ cube_switch = false }`) или если мы и так в обзоре — там куб уже
    /// крутится, и второй заводить не на чем.
    pub fn куб_переход_начать(&mut self, цель: u32) {
        if self.lua_config.cube <= 0.0
            || !self.lua_config.cube_switch
            || self.куб_шейдер.is_none()
            || self.overview_active
        {
            return;
        }
        // Кольцо снимаем заново не только на первом шаге, но и когда цели в
        // нём нет: пустой стол в кольцо не попадает сам, а уйти на него можно.
        if !self.куб_переход || !self.куб_столы.contains(&цель) {
            self.куб_войти_со_столом(Some(цель));
        } else if self.куб_выход.take().is_some() {
            // Переход уже шёл и успел начать обратный наезд, а стол переключили
            // ещё раз (Super+PgDn удерживают, а не щёлкают). Отменяем закрытие
            // и отъезжаем обратно: иначе куб доехал бы до экрана и только потом
            // начал крутиться, и вторая грань пролетала бы во весь экран.
            self.куб_масштаб_цель = 1.0;
        }
        // Доворачиваем В БЛИЖАЙШУЮ сторону по кольцу столов: без этого куб на
        // переходе 9→1 крутился бы через все восемь столов. По кольцу, а не по
        // кругу поворота: полный оборот куба — четыре грани, а столов в кольце
        // может быть десять, и «тот же угол» там значит другой стол.
        let Some(шагов) = self.куб_шагов_до(цель) else { return };
        self.куб_цель -= шагов * self.куб_шаг();
        self.куб_переход = true;
        self.request_redraw();
    }
}
