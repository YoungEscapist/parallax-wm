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
//!
//! # Полный экран — свойство СТОЛА, а не компоновщика
//!
//! Развёрнутых окон может быть несколько — по одному на рабочий стол. Раньше
//! ячейка была одна на всех, и из этого росли сразу две поломки:
//!
//!  * F11 на втором столе сначала сворачивал игру на первом, а если фокус так и
//!    остался за ней (переключение стола фокус не трогает), то не делал вообще
//!    ничего: окно уже числилось развёрнутым, и `set_fullscreen` выходил сразу;
//!  * запрос полного экрана от окна ЧУЖОГО стола (игра переспрашивает его,
//!    когда теряет и возвращает фокус) уводил камеру и зум текущего стола на
//!    это окно — со стороны это выглядело как «Win+цифра во время игры не
//!    работает»: стол переключался и тут же уезжал обратно к игре.
//!
//! Поэтому всё, что меняет камеру, зум и фокус, спрашивает сначала: окно на
//! ТЕКУЩЕМ столе? Если нет — трогаем только его собственную геометрию, а кадр
//! кладём в ячейку его стола (`tag_cameras`), откуда его достанет view_tag.

use smithay::{
    desktop::Window,
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    utils::{IsAlive, Logical, Point, Rectangle, Size},
};

use std::time::Duration;

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
    /// Камера и зум до входа — кадр ТОГО стола, на котором окно развернули.
    prev_cam: (f64, f64),
    prev_zoom: f64,
    /// Пока `Some` — клиенту размер уже отправлен, но кадра нужного размера от
    /// него ещё не было, и холст мы не трогали. Внутри — крайний срок ожидания.
    pending: Option<std::time::Instant>,
}

impl Dawn {
    fn fullscreen_index(&self, window: &Window) -> Option<usize> {
        self.fullscreens.iter().position(|f| &f.window == window)
    }

    /// Теги (столы) окна. У окна, которого нет в списке (override-redirect
    /// меню), тегов нет — такое окно считаем «везде своим», иначе запрос
    /// полного экрана от него не сработал бы нигде.
    fn window_tags(&self, window: &Window) -> u32 {
        self.tagged_windows.iter()
            .find(|tw| &tw.window == window)
            .map(|tw| tw.tags)
            .unwrap_or(0)
    }

    /// Окно живёт на текущем столе (или тегов у него нет — см. выше).
    fn window_here(&self, window: &Window) -> bool {
        let tags = self.window_tags(window);
        tags == 0 || tags & self.viewport.current_tags() != 0
    }

    /// Окно сейчас показано на весь экран?
    ///
    /// Пока переход не доигран (клиент ещё не прислал кадр во весь экран), это
    /// `false`: окно рисуется ровно как рисовалось — со скруглением, тенью и
    /// панелью на месте. Иначе на время ожидания получалась бы третья,
    /// ни на что не похожая картинка.
    pub fn is_fullscreen(&self, window: &Window) -> bool {
        self.fullscreen_index(window)
            .is_some_and(|i| self.fullscreens[i].pending.is_none())
    }

    /// Окну уже заказан полный экран (даже если переход ещё не доигран).
    pub fn fullscreen_requested(&self, window: &Window) -> bool {
        self.fullscreen_index(window).is_some()
    }

    /// Развёрнутое окно ТЕКУЩЕГО стола, если оно есть.
    pub fn fullscreen_window_here(&self) -> Option<Window> {
        self.fullscreens.iter()
            .find(|f| f.pending.is_none() && self.window_here(&f.window))
            .map(|f| f.window.clone())
    }

    /// Полноэкранное окно есть ЗДЕСЬ — на текущем рабочем столе.
    ///
    /// Именно по этому признаку убираются панель столов и миникарта (см.
    /// render_surface), а не по «фуллскрин вообще существует»: полноэкранная
    /// игра на соседнем столе не должна оставлять без панели тот стол, куда
    /// человек переключился.
    pub fn fullscreen_here(&self) -> bool {
        self.fullscreen_window_here().is_some()
    }

    /// Обоев на экране не видно ни пикселя: их целиком накрыло полноэкранное
    /// окно этого стола.
    ///
    /// По этому признаку фоновым layer-поверхностям перестают идти кадровые
    /// callback'и (см. udev.rs). Живые обои — это не «пара процентов на фоне»:
    /// замер 12.08.2026 на роликe 3840×2160@60 показал 184% ядра у одного
    /// только ffmpeg (декодирование картой уже включено — платим за пересчёт
    /// 4K→2560 и nv12→bgra), и всё это время под полноэкранной Dota картинка
    /// уходила в никуда. Без callback'ов dwall засыпает, перестаёт читать
    /// кадры — и ffmpeg упирается в переполненную трубу и встаёт следом.
    ///
    /// Зум и обзор проверяем отдельно: в обзоре и при отдалённой камере окно
    /// стоит на холсте миниатюрой, и вокруг него законно видны обои.
    ///
    /// Прозрачность клиента здесь не учитывается: окно, попросившее полный
    /// экран, считается непрозрачным. Полупрозрачная игра увидела бы под собой
    /// застывший кадр обоев — цена, которую мы платим осознанно.
    /// Обои сейчас живые: фоновый слой присылал кадр меньше секунды назад.
    ///
    /// Признак нужен ровно затем, чтобы не будить главный цикл 60 раз в
    /// секунду ради СТАТИЧНОЙ картинки: та коммитит один раз и больше ничего
    /// не просит, и тик ей ни к чему.
    pub fn фон_живой(&self) -> bool {
        self.фон_коммит.is_some_and(|t| t.elapsed() < Duration::from_secs(1))
    }

    /// Ответить кадровым callback'ом фоновым слоям, ничего не рисуя.
    ///
    /// Живые обои крутятся ТОЛЬКО пока композитор отвечает на их кадровые
    /// запросы: dwall, промолчавший 300 мс, объявляет себя закрытым и
    /// перестаёт забирать кадры у декодера (см. dwall: «обои закрыты —
    /// засыпаем»). Callback'и рассылает хвост `render_surface`, то есть они
    /// идут ровно тогда, когда кадр собирается, — а кадр dawn собирает только
    /// по изменению на экране.
    ///
    /// На неподвижном экране это замыкается в круг: спящий dwall ничего не
    /// коммитит → рисовать нечего → callback'а нет → dwall спит дальше.
    /// Снаружи это и есть жалоба Ярика 29.08.2026: «обои анимируются только
    /// при действиях, если ничего не делать — стоят». Разомкнуть круг может
    /// только тот, кто идёт САМ, — 60-герцевый тик (см. `anim::tick`).
    ///
    /// Ответ на callback ещё не значит кадр: dwall в ответ нарисует свой
    /// следующий кадр и закоммитит, коммит поднимет `needs_redraw`, и дальше
    /// цепочка снова идёт обычным путём, по изменениям. То есть кадров
    /// композитора здесь не прибавляется — прибавляется только их повод.
    ///
    /// Под полноэкранным окном (`wallpaper_hidden`) молчим намеренно: сон
    /// обоев там и есть цель, см. доктекст выше.
    ///
    /// Будить приходится и УЖЕ УСНУВШИЕ обои, а не только живые. Иначе сторож
    /// запирает сам себя: он поддерживает круг, пока тот крутится, но стоит
    /// dwall проспать дольше секунды — отметка `фон_коммит` протухает, сторож
    /// умолкает, и обои не проснутся никогда. Дырка не теоретическая: выход из
    /// полноэкранного окна (там молчим намеренно) протухание ГАРАНТИРУЕТ, а
    /// сверх того его дают гашение монитора и любая заминка при старте. Именно
    /// это Ярик видел 29.08.2026 второй раз, уже с правкой в бинаре.
    ///
    /// Спящему хватит одного callback'а: dwall на него нарисует кадр и
    /// закоммитит, коммит освежит `фон_коммит`, и дальше круг крутится сам, на
    /// частом тике. Поэтому холодная побудка идёт РЕДКО (`ХОЛОДНАЯ_ПОБУДКА`):
    /// на неё некому ответить, когда обои статичны или их нет вовсе, и сыпать
    /// такими callback'ами по 60 раз в секунду незачем.
    pub fn будить_фоновые_слои(&mut self) {
        use smithay::desktop::layer_map_for_output;
        use smithay::wayland::shell::wlr_layer::Layer;

        /// Пауза между побудками СПЯЩИХ обоев. Меньше порога dwall (300 мс,
        /// `ОЖИДАНИЕ_КАДРА`), так что уснувший поднимается на следующем же
        /// тике, а не через раз.
        const ХОЛОДНАЯ_ПОБУДКА: Duration = Duration::from_millis(200);

        if !self.фон_живой() {
            // Обои спят: будим редко и не чаще, чем раз в ХОЛОДНАЯ_ПОБУДКА.
            let пора = self.фон_побудка.is_none_or(|t| t.elapsed() >= ХОЛОДНАЯ_ПОБУДКА);
            if !пора {
                return;
            }
            self.фон_побудка = Some(std::time::Instant::now());
        }
        let elapsed = self.start_time.elapsed();
        // По каждому монитору — своей картой слоёв и своим wl_output, ровно
        // как в хвосте render_surface: у второго монитора обои свои, и
        // будить их картой первого значит не разбудить вовсе.
        for i in 0..self.мониторы.len().max(1) {
            let вернуть = self.войти_в_монитор(i);
            if !self.wallpaper_hidden() {
                let выход = self.мониторы.get(i).map(|m| m.output.clone())
                    .or_else(|| self.space.outputs().next().cloned());
                let слои = self.мониторы.get(i).map(|m| m.layer_output.clone())
                    .or_else(|| self.layer_output.clone());
                if let (Some(выход), Some(слои)) = (выход, слои) {
                    for layer_surface in layer_map_for_output(&слои).layers() {
                        if !matches!(layer_surface.layer(), Layer::Background | Layer::Bottom) {
                            continue;
                        }
                        layer_surface.send_frame(
                            &выход, elapsed,
                            Some(Duration::from_millis(16)),
                            |_, _| Some(выход.clone()),
                        );
                    }
                }
            }
            self.покинуть_монитор(вернуть);
        }
    }

    pub fn wallpaper_hidden(&self) -> bool {
        if self.overview_active || (self.viewport.zoom - 1.0).abs() > 0.001 {
            return false;
        }
        let Some(window) = self.fullscreen_window_here() else { return false };
        let экран = self.screen_size();
        let Some(geo) = self.space.element_geometry(&window) else { return false };
        // Окно ровно в кадре и ровно в размер монитора — тот самый договор,
        // который каждый кадр поддерживает resync_fullscreen_frame.
        geo.size.w >= экран.w
            && geo.size.h >= экран.h
            && (geo.loc.x as f64 - self.viewport.cam_x).abs() < 1.0
            && (geo.loc.y as f64 - self.viewport.cam_y).abs() < 1.0
    }

    /// Курсор рисует клиент, чьё окно сейчас занимает весь экран?
    ///
    /// По этому признаку с курсора снимается потолок размера (см. udev.rs):
    /// потолок нужен, чтобы стрелка не прыгала в размере на границах окон, а у
    /// полноэкранного окна границ на экране нет — там курсор целиком его дело
    /// (прицел в игре и должен быть таким, каким его нарисовала игра).
    ///
    /// Сравниваем КЛИЕНТА, а не поверхность: курсор — это отдельная
    /// поверхность, и общего с окном у них ровно одно — тот, кто их создал. Для
    /// X11-окон это всегда XWayland, то есть под полноэкранной игрой потолок
    /// снимается со всех X11-курсоров разом; на экране в этот момент всё равно
    /// только её курсор.
    pub fn cursor_owned_by_fullscreen(&self, cursor: &WlSurface) -> bool {
        let Some(чей) = cursor.client().map(|c| c.id()) else { return false };
        self.fullscreens.iter()
            .filter(|f| f.pending.is_none() && self.window_here(&f.window))
            .filter_map(|f| crate::xwin::surface(&f.window))
            .any(|s| s.client().map(|c| c.id()) == Some(чей.clone()))
    }

    /// F11: развернуть сфокусированное окно на весь экран или вернуть обратно.
    pub fn toggle_fullscreen(&mut self) {
        // В обзоре столов камерой и раскладкой распоряжается overview.rs —
        // фуллскрин из-под него сломал бы и то, и другое (так же поступают
        // остальные действия, меняющие камеру).
        if self.overview_active {
            return;
        }
        // Свернуть можно только то, что развёрнуто НА ЭТОМ СТОЛЕ: полноэкранная
        // игра на соседнем столе живёт своей жизнью и F11 здесь не касается.
        if let Some(window) = self.fullscreen_window_here() {
            self.unset_fullscreen_window(&window);
            return;
        }
        let Some(window) = self.window_to_fullscreen() else {
            tracing::debug!("dawn/fullscreen: нет окна для разворота");
            return;
        };
        self.set_fullscreen(&window);
    }

    /// Кого разворачивает F11: окно в фокусе, иначе окно под курсором, иначе
    /// верхнее окно стола.
    ///
    /// Все три кандидата обязаны быть НА ЭТОМ СТОЛЕ. Переключение стола фокус
    /// не двигает, поэтому после Win+2 в фокусе запросто остаётся окно первого
    /// стола — развернув его, F11 на втором столе «не работал» (окно уже
    /// числилось развёрнутым) либо разворачивал невидимое отсюда окно.
    ///
    /// Третий кандидат — на случай, когда фокуса нет вовсе (его снимает клик по
    /// пустому холсту и выход из полного экрана): раньше F11 в этот момент
    /// молча не делал ничего.
    fn window_to_fullscreen(&self) -> Option<Window> {
        if let Some(w) = self.focused_window().filter(|w| self.window_here(w)) {
            return Some(w);
        }
        if let Some(w) = self.space.element_under(self.pointer_location)
            .map(|(w, _)| w.clone())
            .filter(|w| self.window_here(w))
        {
            return Some(w);
        }
        // space.elements() идёт снизу вверх — верхнее окно последнее.
        self.space.elements()
            .filter(|w| self.window_here(w) && self.window_tags(w) != 0)
            .next_back()
            .cloned()
    }

    /// Развернуть конкретное окно (F11, а также запрос клиента —
    /// полноэкранное видео, демонстрация экрана, игры).
    pub fn set_fullscreen(&mut self, window: &Window) {
        if self.overview_active || self.fullscreen_requested(window) {
            return;
        }
        // Одновременно на весь экран может быть только одно окно НА СТОЛ:
        // сворачиваем то, что уже развёрнуто на столах этого окна, и не трогаем
        // остальные столы.
        let tags = self.window_tags(window);
        let развёрнутые: Vec<Window> = self.fullscreens.iter()
            .map(|f| f.window.clone())
            .collect();
        for other in развёрнутые {
            if &other != window && (tags == 0 || self.window_tags(&other) & tags != 0) {
                self.unset_fullscreen_window(&other);
            }
        }
        // Окно ЧУЖОГО стола из space убрано (см. refresh_tags), и геометрию
        // там не спросить — берём место из списка окон, а размер у самого окна.
        // Без этого запрос полного экрана от плеера на соседнем столе просто
        // терялся: разворачивать было «нечего».
        let geo = match self.space.element_geometry(window) {
            Some(geo) => geo,
            None => {
                let Some(tw) = self.tagged_windows.iter().find(|tw| &tw.window == window)
                else { return };
                Rectangle::new(tw.position, window.geometry().size)
            }
        };

        let свой_стол = self.window_here(window);

        // Кадр, который вернём при выходе. Для чужого стола берём ЕГО
        // запомненный кадр, а не тот, что сейчас на экране: иначе выход из
        // полного экрана увёз бы чужой стол туда, где случайно стояла камера в
        // момент запроса.
        let (prev_cam, prev_zoom) = if свой_стол {
            // Анимации камеры доигрывать нельзя: они уведут её из-под уже
            // выставленного окна (та же причина, что и в ToggleLayoutFloatTile).
            self.momentum.stop();
            self.camera_anim = None;
            self.zoom_anim = None;
            self.zoom_glide = None;
            ((self.viewport.cam_x, self.viewport.cam_y), self.viewport.zoom)
        } else {
            self.tag_cameras.get(&tags)
                .map(|&(x, y, z)| ((x, y), z))
                .unwrap_or(((0.0, 0.0), 1.0))
        };

        let size = self.screen_size();

        let (prev_floating, prev_pinned) = self.tagged_windows.iter()
            .find(|tw| &tw.window == window)
            .map(|tw| (tw.floating, tw.float_pinned))
            .unwrap_or((false, false));

        self.fullscreens.push(Fullscreen {
            window: window.clone(),
            prev_loc: geo.loc,
            prev_size: geo.size,
            prev_floating,
            prev_pinned,
            prev_cam,
            prev_zoom,
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
        // Фокус отдаём только окну ЭТОГО стола: у окна с соседнего стола экрана
        // сейчас нет, и забрать себе клавиатуру оно не может — иначе Win+цифра
        // уводила бы стол, а печатать продолжало бы в невидимую игру.
        if свой_стол {
            crate::xwin::focus(self, window);
        }
        self.request_redraw();
        tracing::info!(
            "dawn/fullscreen: заказан полный экран {}×{} (стол {:#b}{}), ждём кадр клиента",
            size.w, size.h, tags,
            if свой_стол { "" } else { ", не текущий" },
        );
    }

    /// Доиграть переход в полный экран, когда клиент прислал кадр нужного
    /// размера (или когда ждать его надоело). Зовётся раз за итерацию главного
    /// цикла — строго до отрисовки, чтобы большой буфер и новый холст попали в
    /// ОДИН кадр.
    pub fn apply_pending_fullscreen(&mut self) {
        let size = self.screen_size();
        let now = std::time::Instant::now();
        let готовые: Vec<Window> = self.fullscreens.iter()
            .filter(|f| f.pending.is_some_and(|deadline| {
                f.window.alive()
                    && (crate::xwin::current_size(&f.window) == size || now >= deadline)
            }))
            .map(|f| f.window.clone())
            .collect();
        for window in готовые {
            self.finish_fullscreen(&window, size);
        }
    }

    /// Возвращает развёрнутому окну кадр: окно лежит ровно в левом верхнем углу
    /// кадра, зум 1. Зовётся каждую итерацию главного цикла — сразу за
    /// [`Self::apply_pending_fullscreen`].
    ///
    /// Полный экран в dawn держится на договоре «окно стоит в той точке холста,
    /// где стоит камера» (см. finish_fullscreen). Договор этот односторонний:
    /// окно ставится один раз, а камеру потом двигает кто угодно — и тогда игра
    /// уезжает с экрана, оставаясь развёрнутой.
    ///
    /// Ровно это и было с F11 после обзора (жалоба 12.08.2026): обзор
    /// раскладывает столы по СЕТКЕ холста, а выход с переходом на стол
    /// (`exit_overview_immediate` → ветка Tile/Monocle) ставит кадр в (0,0) и
    /// зовёт arrange — камера уезжает в начало координат, а окно игры остаётся
    /// там, где его застал F11 (в логе — 2532,1207). Возврата к окну не делал
    /// никто: обзор про полный экран не знает вовсе.
    ///
    /// Чинить в самом обзоре мало: камеру двигают ещё и перелёты по столам,
    /// зум, инерция прокрутки. Поэтому договор проверяется каждый кадр в одном
    /// месте — как и переход в полный экран по соседству.
    pub fn resync_fullscreen_frame(&mut self) {
        // В обзоре окно ЗАКОННО стоит миниатюрой в своей ячейке сетки, и камера
        // смотрит на всю сетку разом — не мешаем.
        if self.overview_active {
            return;
        }
        let Some(window) = self.fullscreen_window_here() else { return };
        // Камеру берём ЦЕЛЕВУЮ: если к столу ещё летит анимация, окно надо
        // ставить туда, куда она приедет, а не туда, где она сейчас.
        let (цель_x, цель_y, цель_зум) = self.view_frame_target();
        // Отдалённый холст — это человек СПЕЦИАЛЬНО отъехал посмотреть стол
        // целиком (зум живёт в рендере и полному экрану не мешает). Возвращать
        // ему масштаб силой каждый кадр значило бы отнимать у него зум вовсе;
        // договор «окно в углу кадра» держим только в обычном масштабе.
        if (цель_зум - 1.0).abs() > 0.001 {
            return;
        }
        let loc = Point::<i32, Logical>::from((цель_x.round() as i32, цель_y.round() as i32));
        let текущее = self.space.element_geometry(&window).map(|g| g.loc);
        // Окно ЛЕТИТ в свой дом — и дом этот тот самый, что нам нужен.
        //
        // Так выглядит слайд перелистывания столов (`Dawn::слайд_столов`):
        // приходящее окно ставится за край экрана, на `дом.x + ширина + зазор`,
        // и отпускается в пружину до `tw.position`. Правка в этот момент
        // означала бы телепорт окна в угол на КАЖДОМ кадре слайда: map_element
        // ниже анимацию не снимает, и следующий же тик уводит окно обратно на
        // траекторию — драка на все 340 мс перелистывания. Видно это было и в
        // логе, и глазом: игра на своём столе не въезжала, а дёргалась.
        //
        // Замер 24.08.2026 (лог 11:08): каждая пачка правок начиналась ровно
        // строкой `view_tag → 0b1000` — вход на стол с развёрнутой игрой, окно
        // при этом на x=2680 при экране 2560 и зазоре 120. Другого источника
        // расхождения в том логе не было вовсе.
        //
        // Ждать безопасно: доехав, окно встанет ровно в `loc`, а не доехав
        // (слайд сбит новым перелистыванием) — цель анимации сменится, условие
        // перестанет выполняться, и правка сработает как раньше.
        let летит_домой = self.window_anim_target(&window) == Some(loc);
        let место_сходится = текущее == Some(loc) || (текущее.is_some() && летит_домой);
        let кадр_сходится = место_сходится
            && (self.viewport.cam_x - loc.x as f64).abs() < 0.5
            && (self.viewport.cam_y - loc.y as f64).abs() < 0.5;
        if кадр_сходится {
            return;
        }
        // ── Что именно разошлось ─────────────────────────────────────────────
        //
        // Договор обязан сойтись с ОДНОЙ правки: ниже камера ставится ровно в
        // `loc`, а окно кладётся ровно туда же. Если строка ниже идёт каждый
        // кадр, значит правка не держится, и починить это без ответа «а что
        // именно не сошлось» нельзя — раньше в логе стоял только результат
        // `(0,0)`, одинаковый у всех трёх причин.
        //
        // Замер 24.08.2026 (лог сеанса на 45 минут): 15674 таких строк, до 839
        // за секунду — то есть правка НЕ ДЕРЖИТСЯ и цикл идёт вхолостую на
        // каждой итерации, а каждая строка — синхронная запись на диск через
        // `tee` из главного потока. Причина на 24.08.2026 не найдена: в логе
        // того дня остался только результат.
        let причина = match текущее {
            None => "окна нет в space",
            Some(_) if !место_сходится => "окно не там",
            _ => "камера не там",
        };
        self.fullscreen_resync_счёт += 1;
        let пора = self.fullscreen_resync_лог
            .is_none_or(|t: std::time::Instant| t.elapsed().as_secs() >= 1);
        if пора {
            self.fullscreen_resync_лог = Some(std::time::Instant::now());
            tracing::debug!(
                "dawn/fullscreen: возврат кадра к развёрнутому окну в ({},{}): {} \
                 (окно {:?}, камера {:.1},{:.1}, всего правок {})",
                loc.x, loc.y, причина, текущее,
                self.viewport.cam_x, self.viewport.cam_y, self.fullscreen_resync_счёт,
            );
        }
        // Анимации камеры доигрывать нельзя — они снова уведут её из-под окна
        // (та же причина, что и в set_fullscreen).
        self.momentum.stop();
        self.camera_anim = None;
        self.zoom_anim = None;
        self.zoom_glide = None;
        self.viewport.zoom = 1.0;
        self.viewport.cam_x = loc.x as f64;
        self.viewport.cam_y = loc.y as f64;
        self.apply_camera();
        // Окно, летящее в свой дом, не трогаем — сюда мы дошли только из-за
        // камеры (см. `место_сходится` выше). Подвинуть его здесь значило бы
        // отменить слайд ровно тем способом, от которого мы и уходим.
        if !место_сходится {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == &window) {
                tw.position = loc;
            }
            self.space.map_element(window, loc, true);
        }
        self.request_redraw();
    }

    fn finish_fullscreen(&mut self, window: &Window, size: Size<i32, Logical>) {
        let свой_стол = self.window_here(window);
        let tags = self.window_tags(window);

        // Левый верхний угол экрана — это точка холста, равная позиции камеры
        // (screen = (canvas − cam) × zoom). Округляем её и делаем началом окна,
        // а камеру ставим ровно туда же при зуме 1: тогда окно размера монитора
        // ложится на экран пиксель в пиксель.
        //
        // Для чужого стола «камера» — это его запомненный кадр: свой экран этот
        // стол получит, когда на него перейдут.
        let (cam_x, cam_y) = if свой_стол {
            (self.viewport.cam_x, self.viewport.cam_y)
        } else {
            self.tag_cameras.get(&tags).map(|&(x, y, _)| (x, y)).unwrap_or((0.0, 0.0))
        };
        let loc = Point::<i32, Logical>::from((cam_x.round() as i32, cam_y.round() as i32));

        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) {
            tw.position = loc;
        }
        if свой_стол {
            self.viewport.zoom = 1.0;
            self.viewport.cam_x = loc.x as f64;
            self.viewport.cam_y = loc.y as f64;
            self.apply_camera();
            self.space.map_element(window.clone(), loc, true);
        } else {
            // В space окно НЕ кладём: там живут окна текущего стола, и чужое
            // оказалось бы на экране поверх своих (refresh_tags уберёт его лишь
            // при следующем переходе). Позицию оно получит вместе со своим
            // столом, а кадр стола сразу правим под пиксель-в-пиксель.
            self.tag_cameras.insert(tags, (loc.x as f64, loc.y as f64, 1.0));
        }

        let готов = crate::xwin::current_size(window) == size;
        if let Some(i) = self.fullscreen_index(window) {
            self.fullscreens[i].pending = None;
        }
        self.request_redraw();
        tracing::info!(
            "dawn/fullscreen: окно на весь экран {}×{} в ({},{}){}{}",
            size.w, size.h, loc.x, loc.y,
            if готов { "" } else { " (клиент не успел, переключились по таймауту)" },
            if свой_стол { "" } else { " (стол не текущий — только геометрия)" },
        );
    }

    /// F11 по развёрнутому окну ТЕКУЩЕГО стола.
    pub fn unset_fullscreen(&mut self) {
        let Some(window) = self.fullscreen_window_here() else { return };
        self.unset_fullscreen_window(&window);
    }

    /// Вернуть конкретное развёрнутое окно к прежнему размеру, месту, камере и
    /// зуму.
    pub fn unset_fullscreen_window(&mut self, window: &Window) {
        let Some(i) = self.fullscreen_index(window) else { return };
        let fs = self.fullscreens.remove(i);
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
        let свой_стол = self.window_here(&window);
        if свой_стол {
            self.space.map_element(window.clone(), fs.prev_loc, true);
        }
        // Окно чужого стола в space не кладём совсем: там живут окна текущего
        // стола, и чужое оказалось бы на экране поверх своих. Своё место оно
        // получит вместе со своим столом — позиция уже записана выше.

        // Камеру возвращаем только если окно было развёрнуто НА ЭТОМ СТОЛЕ.
        //
        // У каждого стола свой кадр (см. tag_cameras в view_tag), и кадр,
        // запомненный при развороте, принадлежит СВОЕМУ столу. Применить его,
        // стоя на другом, значит увезти чужую камеру на чужой стол — это и было
        // главным проявлением «столы смешиваются». Поэтому для чужого стола
        // кадр не применяем, а кладём в его ячейку: он восстановится сам, когда
        // на этот стол вернутся.
        if !недоигран {
            if свой_стол {
                self.momentum.stop();
                self.camera_anim = None;
                self.zoom_anim = None;
                self.zoom_glide = None;
                self.viewport.cam_x = fs.prev_cam.0;
                self.viewport.cam_y = fs.prev_cam.1;
                self.viewport.zoom = fs.prev_zoom;
                self.apply_camera();
            } else {
                let tags = self.window_tags(&window);
                self.tag_cameras.insert(tags, (fs.prev_cam.0, fs.prev_cam.1, fs.prev_zoom));
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
        let Some(i) = self.fullscreen_index(window) else { return };
        let свой_стол = self.window_here(window);
        let fs = self.fullscreens.remove(i);
        // Недоигранный переход холст не менял — и возвращать его не надо.
        if fs.pending.is_none() {
            if свой_стол {
                self.viewport.cam_x = fs.prev_cam.0;
                self.viewport.cam_y = fs.prev_cam.1;
                self.viewport.zoom = fs.prev_zoom;
                self.apply_camera();
            } else {
                let tags = self.window_tags(window);
                self.tag_cameras.insert(tags, (fs.prev_cam.0, fs.prev_cam.1, fs.prev_zoom));
            }
        }
        // Панель и полка возвращаются на экран именно этим кадром: закрытие
        // окна само по себе перерисовку не заказывает.
        self.request_redraw();
    }
}
