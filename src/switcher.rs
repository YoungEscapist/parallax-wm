//! Два способа добраться до окна, которого сейчас не видно.
//!
//! · **Alt+Tab** — перебор СТОПКИ: окон, лежащих друг под другом в одном месте
//!   холста. В dawn холст бесконечен и окна расставлены по нему свободно, но
//!   ровно в одной точке их обычно несколько (плавающая раскладка, окна одного
//!   приложения, схлопнутая стопка 2.4), и верхнее закрывает остальные.
//!   Пространственная навигация (Super+стрелки, focus_direction) до них не
//!   добирается вовсе: она ищет соседа В СТОРОНЕ, а тут сосед ПОД.
//!
//! · **Super+F** — поиск по имени: набранные буквы видно на экране, Enter
//!   перелетает камерой к подходящему окну. На холсте, где окно может стоять в
//!   десяти тысячах пикселей от камеры и на чужом столе, это единственный
//!   способ попасть в него, не помня, где оно.

use smithay::desktop::Window;
use smithay::utils::{IsAlive, Logical, Point, Rectangle};

use crate::state::Dawn;

// ── Alt+Tab: перебор стопки ──────────────────────────────────────────────────

/// Сессия перебора: живёт, пока держат Alt.
///
/// Порядок фиксируется на первом Tab и дальше НЕ пересчитывается. Иначе
/// перебор зациклился бы на двух верхних окнах: каждый шаг поднимает окно
/// наверх (`xwin::focus` → `raise_element`), то есть меняет тот самый z-порядок,
/// по которому строится стопка.
pub struct AltTab {
    pub order: Vec<Window>,
    pub idx: usize,
}

impl Dawn {
    /// Окна текущих столов, лежащие в одной точке холста с `pivot`, сверху вниз.
    fn overlap_stack(&self, pivot: &Window) -> Vec<Window> {
        let Some(base) = self.space.element_geometry(pivot) else { return Vec::new() };
        // space.elements() идёт снизу вверх — разворачиваем, чтобы перебор шёл
        // от верхнего окна к нижнему (как ждёт рука от Alt+Tab).
        self.space
            .elements()
            .rev()
            .filter(|w| !crate::xwin::is_override_redirect(w))
            // Окно Minecraft перекрывает собой ВСЁ, поэтому в стопку
            // «что лежит под этим окном» оно попадало всегда — и Alt+Tab
            // упирался в него намертво (см. `mine::в_переборе`).
            .filter(|w| crate::mine::в_переборе(self, w))
            .filter(|w| {
                self.space
                    .element_geometry(w)
                    .is_some_and(|g| overlaps(g, base))
            })
            .cloned()
            .collect()
    }

    /// Alt+Tab: следующее окно стопки под текущим (`dir` = +1 вглубь, −1 назад).
    ///
    /// Если под окном никого нет, перебираем все видимые окна стола — это
    /// привычное поведение Alt+Tab и оно ничего не ломает: в стопке из одного
    /// окна перебирать всё равно нечего.
    pub fn cycle_stack(&mut self, dir: i32) {
        // Первый Tab: собираем стопку и запоминаем порядок.
        if self.alt_tab.is_none() {
            let pivot = self
                .focused_window()
                .or_else(|| self.space.elements().next_back().cloned());
            let Some(pivot) = pivot else { return };

            let mut order = self.overlap_stack(&pivot);
            if order.len() < 2 {
                // Никто под окном не лежит — перебираем всё, что на столе.
                order = self
                    .space
                    .elements()
                    .rev()
                    .filter(|w| !crate::xwin::is_override_redirect(w))
                    .filter(|w| crate::mine::в_переборе(self, w))
                    .cloned()
                    .collect();
            }
            if order.len() < 2 {
                return; // одно окно — переключать не на что
            }
            let idx = order
                .iter()
                .position(|w| crate::dwindle::same_window(w, &pivot))
                .unwrap_or(0);
            tracing::debug!(
                "dawn: alt-tab: стопка из {} окон, старт с {}", order.len(), idx,
            );
            self.alt_tab = Some(AltTab { order, idx });
        }

        // Мёртвые окна из порядка убираем на месте: за время удержания Alt
        // что-то могло закрыться, а фокусировать закрытое нельзя.
        let Some(session) = self.alt_tab.as_mut() else { return };
        let текущее = session.order.get(session.idx).cloned();
        session.order.retain(|w| w.alive());
        if session.order.len() < 2 {
            self.alt_tab = None;
            return;
        }
        session.idx = текущее
            .and_then(|w| session.order.iter().position(|o| crate::dwindle::same_window(o, &w)))
            .unwrap_or(0);

        let n = session.order.len() as i32;
        session.idx = (((session.idx as i32 + dir) % n + n) % n) as usize;
        let next = session.order[session.idx].clone();

        crate::xwin::focus(self, &next);
        // Стопка лежит в одной точке, камеру двигать незачем. Но в запасном
        // режиме (перебор всех окон стола) следующее окно вполне может стоять
        // за краем экрана — тогда подвозим камеру, иначе Alt+Tab уводит фокус
        // в пустоту.
        if !self.window_fully_visible(&next) {
            self.snap_camera_to_window(&next);
        }
        self.request_redraw();
    }

    /// Alt отпустили — сессия перебора закончилась.
    pub fn cycle_stack_end(&mut self) {
        if self.alt_tab.take().is_some() {
            self.request_redraw();
        }
    }

    /// Целиком ли окно в кадре (видимая часть холста).
    fn window_fully_visible(&self, window: &Window) -> bool {
        let Some(g) = self.space.element_geometry(window) else { return false };
        let vis = self.visible_canvas_size();
        let кадр = Rectangle::<f64, Logical>::new(
            Point::from((self.viewport.cam_x, self.viewport.cam_y)),
            (vis.w, vis.h).into(),
        );
        let g = g.to_f64();
        g.loc.x >= кадр.loc.x
            && g.loc.y >= кадр.loc.y
            && g.loc.x + g.size.w <= кадр.loc.x + кадр.size.w
            && g.loc.y + g.size.h <= кадр.loc.y + кадр.size.h
    }
}

fn overlaps(a: Rectangle<i32, Logical>, b: Rectangle<i32, Logical>) -> bool {
    a.loc.x < b.loc.x + b.size.w
        && b.loc.x < a.loc.x + a.size.w
        && a.loc.y < b.loc.y + b.size.h
        && b.loc.y < a.loc.y + a.size.h
}

// ── Super+F: поиск окна по имени ─────────────────────────────────────────────

/// Одна строка выдачи.
pub struct Hit {
    pub window: Window,
    /// Что показываем: заголовок окна (или app_id, если заголовка нет).
    pub title: String,
    /// Приложение — второй строкой справа.
    pub app: String,
    /// Стол окна (битовая маска тегов) — чтобы Enter умел на него переключить.
    pub tags: u32,
    /// Чем лучше совпадение, тем МЕНЬШЕ (см. score).
    pub score: i32,
}

pub struct SearchUi {
    pub query: String,
    pub sel: usize,
    pub hits: Vec<Hit>,
    /// Прямоугольники строк на экране — заполняет отрисовка, читает хит-тест.
    pub rows: Vec<crate::tray::Rect>,
}

impl SearchUi {
    fn new() -> Self {
        Self { query: String::new(), sel: 0, hits: Vec::new(), rows: Vec::new() }
    }
}

/// Совпадение подстроки без учёта регистра. Возвращает «цену»: чем раньше в
/// строке нашлось, тем дешевле; совпадение с начала слова дешевле середины.
fn substring_score(haystack: &str, needle: &str) -> Option<i32> {
    let h = haystack.to_lowercase();
    let pos = h.find(needle)?;
    let начало_слова = pos == 0
        || h[..pos].chars().next_back().is_some_and(|c| !c.is_alphanumeric());
    Some(pos as i32 + if начало_слова { 0 } else { 40 })
}

/// Подпоследовательность: «frfx» находит «Firefox». Дороже подстроки — такие
/// совпадения показываем ниже.
fn subsequence_score(haystack: &str, needle: &str) -> Option<i32> {
    let h = haystack.to_lowercase();
    let mut chars = h.chars();
    let mut пропущено = 0i32;
    for c in needle.chars() {
        let mut нашли = false;
        for hc in chars.by_ref() {
            if hc == c {
                нашли = true;
                break;
            }
            пропущено += 1;
        }
        if !нашли {
            return None;
        }
    }
    Some(200 + пропущено)
}

/// Цена совпадения запроса с окном. `None` — не подходит вовсе.
fn score(title: &str, app: &str, query: &str) -> Option<i32> {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let по_заголовку = substring_score(title, &q);
    // Совпадение по приложению чуть дороже: человек чаще помнит заголовок.
    let по_приложению = substring_score(app, &q).map(|s| s + 10);
    let нечёткое = subsequence_score(title, &q).or_else(|| subsequence_score(app, &q));
    [по_заголовку, по_приложению, нечёткое].into_iter().flatten().min()
}

impl Dawn {
    pub fn search_open(&self) -> bool {
        self.search.is_some()
    }

    /// Super+F: открыть/закрыть поиск окон.
    pub fn search_toggle(&mut self) {
        if self.search.take().is_some() {
            // Строки поиска в кэше текста больше не нужны.
            self.text_cache.clear();
            self.request_redraw();
            return;
        }
        let mut ui = SearchUi::new();
        self.search_collect(&mut ui);
        self.search = Some(ui);
        self.request_redraw();
    }

    /// Пересобрать выдачу под текущий запрос.
    ///
    /// Ищем по ВСЕМ окнам, а не только по видимым: смысл поиска ровно в том,
    /// чтобы найти окно на другом столе или в другом конце холста.
    fn search_collect(&self, ui: &mut SearchUi) {
        let раньше = ui.hits.get(ui.sel).map(|h| h.window.clone());
        ui.hits = self
            .tagged_windows
            .iter()
            .filter(|tw| tw.window.alive())
            // Искать в игре саму игру нечего: выбрать её значит отдать фокус
            // окну, которое человек и так видит вокруг себя.
            .filter(|tw| crate::mine::в_переборе(self, &tw.window))
            .filter_map(|tw| {
                let app = crate::xwin::app_id(&tw.window).unwrap_or_default();
                let title = crate::xwin::title(&tw.window)
                    .unwrap_or_else(|| if app.is_empty() { "окно".into() } else { app.clone() });
                let score = score(&title, &app, &ui.query)?;
                Some(Hit { window: tw.window.clone(), title, app, tags: tw.tags, score })
            })
            .collect();
        // Порядок устойчивый: сперва цена совпадения, потом имя — иначе строки
        // прыгали бы под руками при каждой букве.
        ui.hits.sort_by(|a, b| {
            a.score
                .cmp(&b.score)
                .then_with(|| a.title.to_lowercase().cmp(&b.title.to_lowercase()))
        });
        // Выбор держим на том же окне, пока оно в выдаче.
        ui.sel = раньше
            .and_then(|w| ui.hits.iter().position(|h| crate::dwindle::same_window(&h.window, &w)))
            .unwrap_or(0);
        if ui.sel >= ui.hits.len() {
            ui.sel = 0;
        }
    }

    fn search_move(&mut self, delta: i32) {
        let Some(ui) = self.search.as_mut() else { return };
        let n = ui.hits.len() as i32;
        if n == 0 {
            return;
        }
        ui.sel = (((ui.sel as i32 + delta) % n + n) % n) as usize;
        self.request_redraw();
    }

    /// Enter: перелететь к выбранному окну.
    fn search_activate(&mut self) {
        let Some((window, tags)) = self
            .search
            .as_ref()
            .and_then(|ui| ui.hits.get(ui.sel))
            .map(|h| (h.window.clone(), h.tags))
        else {
            return;
        };
        self.search = None;
        self.text_cache.clear();

        // Окно на другом столе — сперва переключаем стол, иначе оно даже не
        // замаплено в space и фокусировать нечего.
        if tags & self.viewport.current_tags() == 0 && tags != 0 {
            let первый = 1u32 << tags.trailing_zeros();
            tracing::debug!("dawn: поиск: окно на столе {:#b}, переключаемся", первый);
            self.view_tag(первый);
        }
        // Камеру ведём к окну ДО фокуса: focus() поднимает окно и активирует
        // его, а лететь потом всё равно пришлось бы.
        self.snap_camera_to_window(&window);
        crate::xwin::focus(self, &window);
        self.request_redraw();
    }

    /// Клавиша при открытом поиске. `ch` — символ (буквы идут в строку запроса).
    /// `true` — клавиша съедена поиском.
    pub fn search_key(&mut self, keysym: u32, ch: Option<char>) -> bool {
        use smithay::input::keyboard::keysyms;
        if !self.search_open() {
            return false;
        }
        match keysym {
            keysyms::KEY_Escape => {
                self.search = None;
                self.text_cache.clear();
                self.request_redraw();
            }
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => self.search_activate(),
            keysyms::KEY_Down | keysyms::KEY_Tab => self.search_move(1),
            keysyms::KEY_Up | keysyms::KEY_ISO_Left_Tab => self.search_move(-1),
            keysyms::KEY_BackSpace => {
                if let Some(ui) = self.search.as_mut() {
                    ui.query.pop();
                }
                self.search_refresh();
            }
            _ => {
                // Буквы — в строку запроса. Управляющие символы (и всё, что
                // пришло без символа вовсе) игнорируем.
                let Some(c) = ch.filter(|c| !c.is_control()) else { return true };
                if let Some(ui) = self.search.as_mut() {
                    if ui.query.chars().count() < 48 {
                        ui.query.push(c);
                    }
                }
                self.search_refresh();
            }
        }
        true
    }

    fn search_refresh(&mut self) {
        let Some(mut ui) = self.search.take() else { return };
        self.search_collect(&mut ui);
        self.search = Some(ui);
        // Строки в кэше растеризованы по старому запросу — выдача сменилась.
        self.text_cache.clear();
        self.request_redraw();
    }

    /// Клик при открытом поиске. `true` — клик съеден.
    pub fn search_click(&mut self, pos: Point<f64, smithay::utils::Physical>) -> bool {
        if !self.search_open() {
            return false;
        }
        let hit = self.search.as_ref().and_then(|ui| {
            ui.rows.iter().position(|r| r.hit(pos.x, pos.y))
        });
        match hit {
            Some(idx) => {
                if let Some(ui) = self.search.as_mut() {
                    ui.sel = idx;
                }
                self.search_activate();
            }
            // Мимо списка — закрываем, как и все остальные меню dawn.
            None => self.search_toggle(),
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::{score, subsequence_score, substring_score};

    #[test]
    fn пустой_запрос_подходит_всем() {
        assert_eq!(score("Firefox", "firefox", ""), Some(0));
    }

    #[test]
    fn регистр_не_важен() {
        assert!(score("Telegram Desktop", "telegram", "TELE").is_some());
        assert!(score("Telegram Desktop", "telegram", "desk").is_some());
    }

    #[test]
    fn начало_слова_дешевле_середины() {
        let начало = substring_score("green tree", "tree").unwrap();
        let середина = substring_score("evergreen", "green").unwrap();
        assert!(начало < середина, "{начало} должно быть меньше {середина}");
    }

    #[test]
    fn подстрока_дешевле_подпоследовательности() {
        let подстрока = score("Firefox", "firefox", "fire").unwrap();
        let нечёткое = score("Firefox", "firefox", "frfx").unwrap();
        assert!(подстрока < нечёткое, "{подстрока} должно быть меньше {нечёткое}");
    }

    #[test]
    fn чужие_буквы_не_совпадают() {
        assert_eq!(score("Firefox", "firefox", "zzz"), None);
        assert_eq!(subsequence_score("firefox", "xf"), None);
    }

    /// Заголовок весит больше app_id: человек помнит, что было НАПИСАНО в окне.
    #[test]
    fn заголовок_важнее_приложения() {
        let по_заголовку = score("dawn — исходники", "ghostty", "dawn").unwrap();
        let по_приложению = score("исходники", "dawn", "dawn").unwrap();
        assert!(по_заголовку < по_приложению);
    }
}
