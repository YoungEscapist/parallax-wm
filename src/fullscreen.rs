//! Полноэкранный режим (F11 и запросы клиентов).
//!
//! Экран у dawn — это окно в бесконечный холст: то, что видно, задаётся камерой
//! и зумом. Поэтому «на весь экран» здесь не просто размер окна, а три вещи
//! разом:
//!
//!  1. окно получает размер монитора и встаёт в ту точку холста, которая
//!     сейчас в левом верхнем углу экрана (а это ровно позиция камеры);
//!  2. зум сбрасывается в 1 — иначе окно размера экрана было бы нарисовано
//!     крупнее или мельче него, и «весь экран» не получился бы. Заодно кадр
//!     становится пиксель-в-пиксель, что и нужно для видео и демонстрации
//!     экрана;
//!  3. окно уходит из раскладки (floating + pinned), иначе следующий arrange()
//!     тут же вернул бы его в сетку.
//!
//! Скругление углов и тень для такого окна не рисуются (см. udev.rs): у края
//! экрана они смотрелись бы как рамка вокруг «полного экрана».
//!
//! Всё, что было до входа, запоминается в [`Fullscreen`] и возвращается при
//! выходе — включая камеру и зум.

use smithay::{
    desktop::Window,
    utils::{IsAlive, Point, Size, Logical},
};

use crate::state::Dawn;

/// Сколько ждём от клиента кадр во весь экран, прежде чем переключить холст
/// без него. Верхняя граница на случай клиента, который наш размер не примет
/// точь-в-точь (или вовсе проигнорирует): лучше показать «как получилось»,
/// чем залипнуть в полупереходе.
const WAIT_FOR_BUFFER: std::time::Duration = std::time::Duration::from_millis(300);

/// Что было до входа в полноэкранный режим — чтобы вернуть всё как было.
pub struct Fullscreen {
    pub window: Window,
    /// Геометрия окна на холсте до входа.
    prev_loc: Point<i32, Logical>,
    prev_size: Size<i32, Logical>,
    /// Флаги раскладки до входа (окно на время фуллскрина плавающее).
    prev_floating: bool,
    prev_pinned: bool,
    /// Камера и зум до входа.
    prev_cam: (f64, f64),
    prev_zoom: f64,
    /// Пока `Some` — клиенту размер уже отправлен, но кадра нужного размера от
    /// него ещё не было, и холст мы не трогали. Внутри — крайний срок ожидания.
    pending: Option<std::time::Instant>,
}

impl Dawn {
    /// Окно сейчас показано на весь экран?
    ///
    /// Пока переход не доигран (клиент ещё не прислал кадр во весь экран), это
    /// `false`: окно рисуется ровно как рисовалось — со скруглением, тенью и
    /// панелью на месте. Иначе на время ожидания получалась бы третья,
    /// ни на что не похожая картинка.
    pub fn is_fullscreen(&self, window: &Window) -> bool {
        self.fullscreen
            .as_ref()
            .is_some_and(|f| &f.window == window && f.pending.is_none())
    }

    /// Окну уже заказан полный экран (даже если переход ещё не доигран).
    pub fn fullscreen_requested(&self, window: &Window) -> bool {
        self.fullscreen.as_ref().is_some_and(|f| &f.window == window)
    }

    /// Полноэкранное окно есть ЗДЕСЬ — на текущем рабочем столе.
    ///
    /// Именно по этому признаку убираются панель столов и миникарта (см.
    /// render_surface), а не по «фуллскрин вообще существует»: полноэкранная
    /// игра на соседнем столе не должна оставлять без панели тот стол, куда
    /// человек переключился.
    pub fn fullscreen_here(&self) -> bool {
        let Some(f) = self.fullscreen.as_ref() else { return false };
        // Недоигранный переход экран ещё не занял — панель и миникарта на месте.
        if f.pending.is_some() {
            return false;
        }
        let current = self.viewport.current_tags();
        self.tagged_windows
            .iter()
            .find(|tw| tw.window == f.window)
            .is_some_and(|tw| tw.tags & current != 0)
    }

    /// F11: развернуть сфокусированное окно на весь экран или вернуть обратно.
    pub fn toggle_fullscreen(&mut self) {
        // В обзоре столов камерой и раскладкой распоряжается overview.rs —
        // фуллскрин из-под него сломал бы и то, и другое (так же поступают
        // остальные действия, меняющие камеру).
        if self.overview_active {
            return;
        }
        // Свернуть можно только то, что развёрнуто НА ЭТОМ СТОЛЕ. Раньше
        // проверялось одно лишь «фуллскрин вообще есть», и F11 на втором столе
        // сворачивал окно первого — заодно возвращая камеру и зум ТОГО стола
        // прямо здесь. Со стороны это и выглядело как «столы смешиваются»: на
        // экране чужой кадр, а своё окно так и не развернулось.
        match self.fullscreen.as_ref().map(|f| f.window.clone()) {
            Some(_) if self.fullscreen_here() => self.unset_fullscreen(),
            _ => {
                let Some(window) = self.focused_window()
                    .or_else(|| self.space.element_under(self.pointer_location).map(|(w, _)| w.clone()))
                else {
                    tracing::debug!("dawn/fullscreen: нет окна для разворота");
                    return;
                };
                self.set_fullscreen(&window);
            }
        }
    }

    /// Развернуть конкретное окно (F11, а также запрос клиента —
    /// полноэкранное видео, демонстрация экрана, игры).
    pub fn set_fullscreen(&mut self, window: &Window) {
        if self.overview_active || self.fullscreen_requested(window) {
            return;
        }
        // Уже развёрнутое другое окно сначала сворачиваем: одновременно на
        // весь экран может быть только одно.
        if self.fullscreen.is_some() {
            self.unset_fullscreen();
        }
        let Some(geo) = self.space.element_geometry(window) else { return };

        // Анимации камеры доигрывать нельзя: они уведут её из-под уже
        // выставленного окна (та же причина, что и в ToggleLayoutFloatTile).
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;

        let size = self.screen_size();

        let (prev_floating, prev_pinned) = self.tagged_windows.iter()
            .find(|tw| &tw.window == window)
            .map(|tw| (tw.floating, tw.float_pinned))
            .unwrap_or((false, false));

        self.fullscreen = Some(Fullscreen {
            window: window.clone(),
            prev_loc: geo.loc,
            prev_size: geo.size,
            prev_floating,
            prev_pinned,
            prev_cam: (self.viewport.cam_x, self.viewport.cam_y),
            prev_zoom: self.viewport.zoom,
            pending: Some(std::time::Instant::now() + WAIT_FOR_BUFFER),
        });

        // Вне раскладки: иначе ближайший arrange() тут же отресайзит окно
        // обратно под сетку — прямо поверх только что отправленного размера
        // во весь экран. Позицию при этом НЕ трогаем: окно должно остаться
        // ровно там, где оно есть, до конца перехода (см. ниже).
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) {
            tw.floating = true;
            tw.float_pinned = true;
        }

        // Клиенту размер отправлен — и на этом пока всё. Камера, зум и позиция
        // окна меняются НЕ ЗДЕСЬ, а в apply_pending_fullscreen, когда придёт
        // кадр нужного размера.
        //
        // Почему не сразу: между «мы переключили холст» и «клиент прислал
        // большой буфер» проходит несколько кадров, и всё это время окно
        // старого размера рисуется в новом масштабе — при зуме 0.41 оно
        // прыгало в угол экрана и только потом разворачивалось. Два движения
        // вместо одного. Теперь до последнего момента не меняется ничего, а
        // потом всё меняется разом, одним кадром.
        crate::xwin::set_fullscreen(window, Some(size));
        crate::xwin::configure(window);
        crate::xwin::focus(self, window);
        self.request_redraw();
        tracing::info!(
            "dawn/fullscreen: заказан полный экран {}×{}, ждём кадр клиента",
            size.w, size.h,
        );
    }

    /// Доиграть переход в полный экран, когда клиент прислал кадр нужного
    /// размера (или когда ждать его надоело). Зовётся раз за итерацию главного
    /// цикла — строго до отрисовки, чтобы большой буфер и новый холст попали в
    /// ОДИН кадр.
    pub fn apply_pending_fullscreen(&mut self) {
        let Some(fs) = self.fullscreen.as_ref() else { return };
        let Some(deadline) = fs.pending else { return };
        let window = fs.window.clone();
        if !window.alive() {
            return;
        }
        let size = self.screen_size();
        let готов = crate::xwin::current_size(&window) == size;
        let ждали_довольно = std::time::Instant::now() >= deadline;
        if !готов && !ждали_довольно {
            return;
        }

        // Левый верхний угол экрана — это точка холста, равная позиции камеры
        // (screen = (canvas − cam) × zoom). Округляем её и делаем началом окна,
        // а камеру ставим ровно туда же при зуме 1: тогда окно размера монитора
        // ложится на экран пиксель в пиксель.
        let loc = Point::<i32, Logical>::from((
            self.viewport.cam_x.round() as i32,
            self.viewport.cam_y.round() as i32,
        ));

        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == window) {
            tw.position = loc;
        }
        self.viewport.zoom = 1.0;
        self.viewport.cam_x = loc.x as f64;
        self.viewport.cam_y = loc.y as f64;
        self.apply_camera();
        self.space.map_element(window.clone(), loc, true);

        if let Some(fs) = self.fullscreen.as_mut() {
            fs.pending = None;
        }
        self.request_redraw();
        tracing::info!(
            "dawn/fullscreen: окно на весь экран {}×{} в ({},{}){}",
            size.w, size.h, loc.x, loc.y,
            if готов { "" } else { " (клиент не успел, переключились по таймауту)" },
        );
    }

    /// Вернуть развёрнутое окно к прежнему размеру, месту, камере и зуму.
    pub fn unset_fullscreen(&mut self) {
        let Some(fs) = self.fullscreen.take() else { return };
        let window = fs.window;
        // Переход не доигран: холст мы ещё не трогали, окно с места не
        // сдвигали. Возвращать нечего — только снять с клиента размер и флаги.
        let недоигран = fs.pending.is_some();

        crate::xwin::unset_fullscreen(&window, Some(fs.prev_size));
        crate::xwin::configure(&window);

        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == window) {
            tw.floating = fs.prev_floating;
            tw.float_pinned = fs.prev_pinned;
            tw.position = fs.prev_loc;
        }
        self.space.map_element(window.clone(), fs.prev_loc, true);

        // Камеру возвращаем только если окно было развёрнуто НА ЭТОМ СТОЛЕ.
        //
        // У каждого стола свой кадр (см. tag_cameras в view_tag), и кадр,
        // запомненный при развороте, принадлежит СВОЕМУ столу. Применить его,
        // стоя на другом, значит увезти чужую камеру на чужой стол — это и было
        // главным проявлением «столы смешиваются». Поэтому для чужого стола
        // кадр не применяем, а кладём в его ячейку: он восстановится сам, когда
        // на этот стол вернутся.
        let свой_стол = self.tagged_windows.iter()
            .find(|tw| tw.window == window)
            .map(|tw| tw.tags & self.viewport.current_tags() != 0)
            .unwrap_or(true);
        if !недоигран {
            if свой_стол {
                self.momentum.stop();
                self.camera_anim = None;
                self.zoom_anim = None;
                self.viewport.cam_x = fs.prev_cam.0;
                self.viewport.cam_y = fs.prev_cam.1;
                self.viewport.zoom = fs.prev_zoom;
                self.apply_camera();
            } else if let Some(tw) = self.tagged_windows.iter().find(|tw| tw.window == window) {
                self.tag_cameras.insert(tw.tags, (fs.prev_cam.0, fs.prev_cam.1, fs.prev_zoom));
            }
        }

        // Окно вернулось в раскладку — пересобрать её (в Float это no-op).
        self.arrange();
        self.request_redraw();
        tracing::info!("dawn/fullscreen: возврат из полноэкранного режима");
    }

    /// Окно закрылось: если это оно было развёрнуто, снимаем режим, не трогая
    /// мёртвое окно (камеру и зум всё равно возвращаем).
    pub fn forget_fullscreen(&mut self, window: &Window) {
        if !self.fullscreen_requested(window) {
            return;
        }
        let fs = self.fullscreen.take().expect("проверено выше");
        // Недоигранный переход холст не менял — и возвращать его не надо.
        if fs.pending.is_none() {
            self.viewport.cam_x = fs.prev_cam.0;
            self.viewport.cam_y = fs.prev_cam.1;
            self.viewport.zoom = fs.prev_zoom;
            self.apply_camera();
        }
        // Панель и полка возвращаются на экран именно этим кадром: закрытие
        // окна само по себе перерисовку не заказывает.
        self.request_redraw();
    }
}
