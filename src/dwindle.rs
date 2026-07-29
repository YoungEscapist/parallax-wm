//! Hyprland-совместимый dwindle: BSP-дерево вместо фиксированной цепочки.
//!
//! Порт логики из hyprwm/Hyprland,
//! `src/layout/algorithm/tiled/dwindle/DwindleAlgorithm.cpp` (ветка main):
//! `addTarget` / `removeTarget` / `recalcSizePosRecursive` / `resizeTarget`
//! (smart_resizing) / `moveTargetInDirection` / `moveToRoot`.
//!
//! Чем это отличается от старой раскладки dawn (master + рекурсивный
//! `dwindle_rects` по порядку списка окон):
//!
//!  · новое окно ДЕЛИТ ПОПОЛАМ слот сфокусированного окна, а не встаёт в конец
//!    цепочки — порядок `tagged_windows` больше не определяет геометрию;
//!  · направление деления берётся из пропорций слота (шире, чем высокий →
//!    делим лево/право, иначе верх/низ);
//!  · какая половина достаётся новому окну, решает курсор (force_split = 0);
//!  · при закрытии окна его место забирает СОСЕД ПО УЗЛУ, остальная раскладка
//!    не шевелится;
//!  · ресайз меняет `split_ratio` узлов-предков, а не глобальный mfact.
//!
//! Дерево хранится ареной (`Vec<Option<Node>>` + free-list): узлы ссылаются
//! друг на друга индексами, потому что в Rust родитель↔ребёнок циклом по
//! владению не выражается.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle};

/// Сравнение окон. `Window` — это `Arc<WindowInner>` с внутренним id, поэтому
/// PartialEq корректен и для клонов, и для X11-окон (у которых нет
/// xdg-toplevel, и сравнение «по wl_surface» всегда давало false).
pub fn same_window(a: &Window, b: &Window) -> bool {
    a == b
}

/// Содержимое листа. Дерево параметризовано, чтобы его можно было проверять
/// тестами на «окнах» из одного числа: настоящий [`Window`] без живого
/// wayland-дисплея не создать.
pub trait Leaf: Clone {
    fn same(&self, other: &Self) -> bool;
}

impl Leaf for Window {
    fn same(&self, other: &Self) -> bool {
        same_window(self, other)
    }
}

// ── Геометрия ────────────────────────────────────────────────────────────────

/// Прямоугольник в вещественных координатах: дерево делит слоты пополам, и
/// округление на каждом уровне копило бы ошибку в несколько пикселей.
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct FBox {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl FBox {
    pub fn from_rect(r: Rectangle<i32, Logical>) -> Self {
        Self { x: r.loc.x as f64, y: r.loc.y as f64, w: r.size.w as f64, h: r.size.h as f64 }
    }

    /// Округляем КРАЯ, а не (loc, size) по отдельности — иначе соседние окна
    /// расходятся/накладываются на пиксель.
    pub fn to_rect(self) -> Rectangle<i32, Logical> {
        let x0 = self.x.round() as i32;
        let y0 = self.y.round() as i32;
        let x1 = (self.x + self.w).round() as i32;
        let y1 = (self.y + self.h).round() as i32;
        Rectangle::new((x0, y0).into(), ((x1 - x0).max(1), (y1 - y0).max(1)).into())
    }

    pub fn center(self) -> Point<f64, Logical> {
        Point::from((self.x + self.w / 2.0, self.y + self.h / 2.0))
    }

    pub fn contains(self, p: Point<f64, Logical>) -> bool {
        p.x >= self.x && p.x <= self.x + self.w && p.y >= self.y && p.y <= self.y + self.h
    }

    /// Квадрат расстояния от точки до прямоугольника (0 внутри) —
    /// `vecToRectDistanceSquared` из Hyprland.
    fn dist_sq(self, p: Point<f64, Logical>) -> f64 {
        let dx = if p.x < self.x {
            self.x - p.x
        } else if p.x > self.x + self.w {
            p.x - (self.x + self.w)
        } else {
            0.0
        };
        let dy = if p.y < self.y {
            self.y - p.y
        } else if p.y > self.y + self.h {
            p.y - (self.y + self.h)
        } else {
            0.0
        };
        dx * dx + dy * dy
    }
}

/// Направление для movewindow/preselect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Left,
    Right,
    Up,
    Down,
}

impl Dir {
    fn from_delta(dx: i32, dy: i32) -> Option<Self> {
        match (dx.signum(), dy.signum()) {
            (-1, _) => Some(Dir::Left),
            (1, _) => Some(Dir::Right),
            (0, -1) => Some(Dir::Up),
            (0, 1) => Some(Dir::Down),
            _ => None,
        }
    }

    /// Точка сразу ЗА соответствующим краем окна: по ней ищется сосед, рядом с
    /// которым окно переедет (`focalPointForDir` в Hyprland).
    fn focal_point(self, b: FBox) -> Point<f64, Logical> {
        const OUT: f64 = 1.0;
        match self {
            Dir::Left => Point::from((b.x - OUT, b.y + b.h / 2.0)),
            Dir::Right => Point::from((b.x + b.w + OUT, b.y + b.h / 2.0)),
            Dir::Up => Point::from((b.x + b.w / 2.0, b.y - OUT)),
            Dir::Down => Point::from((b.x + b.w / 2.0, b.y + b.h + OUT)),
        }
    }

    /// Новое окно встаёт первым ребёнком (сверху/слева)?
    fn takes_first_child(self) -> bool {
        matches!(self, Dir::Left | Dir::Up)
    }

    fn is_vertical(self) -> bool {
        matches!(self, Dir::Up | Dir::Down)
    }
}

/// Углы/края, за которые тянут при ресайзе (для smart_resizing).
#[derive(Clone, Copy, Debug, Default)]
pub struct Corner {
    pub left: bool,
    pub top: bool,
    pub right: bool,
    pub bottom: bool,
}

impl Corner {
    fn is_none(&self) -> bool {
        !self.left && !self.top && !self.right && !self.bottom
    }
}

// ── Настройки (dwindle:* из hyprland.conf) ───────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub struct DwindleConfig {
    /// `dwindle:split_width_multiplier` — во сколько раз слот должен быть шире,
    /// чем высок, чтобы делиться лево/право.
    pub split_width_multiplier: f32,
    /// `dwindle:preserve_split` — сохранять направление деления узла. false
    /// (как в Hyprland по умолчанию) = направление каждый раз пересчитывается
    /// из пропорций слота, и раскладка «переворачивается» сама при закрытии
    /// соседей. true = деление остаётся там, где его создали.
    pub preserve_split: bool,
    /// `dwindle:force_split`: 0 — сторону выбирает курсор, 1 — всегда
    /// слева/сверху, 2 — всегда справа/снизу.
    pub force_split: u8,
    /// `dwindle:default_split_ratio` — с каким соотношением создаётся новый узел.
    pub default_split_ratio: f32,
}

impl Default for DwindleConfig {
    fn default() -> Self {
        Self {
            split_width_multiplier: 1.0,
            preserve_split: false,
            force_split: 0,
            default_split_ratio: 1.0,
        }
    }
}

// ── Дерево ───────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct Node<T> {
    parent: Option<usize>,
    /// None у листа, Some([first, second]) у внутреннего узла.
    children: Option<[usize; 2]>,
    /// Some только у листа.
    window: Option<T>,
    /// true — делим верх/низ, false — лево/право.
    split_top: bool,
    /// Доля первого ребёнка: размер = половина * split_ratio, 0.1..1.9.
    split_ratio: f32,
    b: FBox,
}

pub struct DwindleTree<T = Window> {
    nodes: Vec<Option<Node<T>>>,
    free: Vec<usize>,
}

// derive(Default) потребовал бы T: Default — у Window его нет.
impl<T> Default for DwindleTree<T> {
    fn default() -> Self {
        Self { nodes: Vec::new(), free: Vec::new() }
    }
}

impl<T: Leaf> DwindleTree<T> {
    // ── арена ────────────────────────────────────────────────────────────────

    fn alloc(&mut self, node: Node<T>) -> usize {
        match self.free.pop() {
            Some(i) => {
                self.nodes[i] = Some(node);
                i
            }
            None => {
                self.nodes.push(Some(node));
                self.nodes.len() - 1
            }
        }
    }

    fn release(&mut self, i: usize) {
        if self.nodes.get(i).map(|n| n.is_some()).unwrap_or(false) {
            self.nodes[i] = None;
            self.free.push(i);
        }
    }

    fn n(&self, i: usize) -> &Node<T> {
        self.nodes[i].as_ref().expect("dwindle: обращение к освобождённому узлу")
    }

    fn nm(&mut self, i: usize) -> &mut Node<T> {
        self.nodes[i].as_mut().expect("dwindle: обращение к освобождённому узлу")
    }

    fn alive(&self, i: usize) -> bool {
        self.nodes.get(i).map(|n| n.is_some()).unwrap_or(false)
    }

    // ── запросы ──────────────────────────────────────────────────────────────

    pub fn is_empty(&self) -> bool {
        self.nodes.iter().all(|n| n.is_none())
    }

    pub fn node_of(&self, window: &T) -> Option<usize> {
        self.nodes.iter().enumerate().find_map(|(i, n)| {
            n.as_ref()
                .and_then(|n| n.window.as_ref())
                .filter(|w| w.same(window))
                .map(|_| i)
        })
    }

    pub fn contains(&self, window: &T) -> bool {
        self.node_of(window).is_some()
    }

    /// Корень (единственный узел без родителя).
    fn root(&self) -> Option<usize> {
        self.nodes
            .iter()
            .enumerate()
            .find(|(_, n)| n.as_ref().map(|n| n.parent.is_none()).unwrap_or(false))
            .map(|(i, _)| i)
    }

    /// Все окна дерева (порядок обхода арены, не раскладки).
    pub fn windows(&self) -> Vec<T> {
        self.nodes
            .iter()
            .filter_map(|n| n.as_ref().and_then(|n| n.window.clone()))
            .collect()
    }

    /// Листья с их прямоугольниками после последнего [`recalc`].
    pub fn leaf_rects(&self) -> Vec<(T, Rectangle<i32, Logical>)> {
        self.nodes
            .iter()
            .filter_map(|n| n.as_ref())
            .filter(|n| n.children.is_none())
            .filter_map(|n| n.window.clone().map(|w| (w, n.b.to_rect())))
            .collect()
    }

    pub fn box_of(&self, window: &T) -> Option<FBox> {
        self.node_of(window).map(|i| self.n(i).b)
    }

    /// Ближайший к точке лист (`getClosestNode`).
    pub fn closest_node(&self, p: Point<f64, Logical>, skip: Option<&T>) -> Option<usize> {
        let mut best: Option<(usize, f64)> = None;
        for (i, node) in self.nodes.iter().enumerate() {
            let Some(node) = node else { continue };
            let Some(w) = node.window.as_ref() else { continue };
            if node.children.is_some() {
                continue;
            }
            if skip.map(|s| s.same(w)).unwrap_or(false) {
                continue;
            }
            let d = node.b.dist_sq(p);
            if best.map(|(_, bd)| d < bd).unwrap_or(true) {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i)
    }

    // ── пересчёт геометрии (recalcSizePosRecursive) ──────────────────────────

    /// Раскладывает всё дерево внутри `area`.
    pub fn recalc(&mut self, area: Rectangle<i32, Logical>, cfg: &DwindleConfig) {
        let Some(root) = self.root() else { return };
        self.nm(root).b = FBox::from_rect(area);
        self.recalc_from(root, cfg, false, false);
    }

    /// Пересчёт поддерева от `idx` вниз, его собственный `b` уже задан.
    /// `h_override`/`v_override` действуют только на сам `idx` (как в Hyprland).
    fn recalc_from(&mut self, idx: usize, cfg: &DwindleConfig, h_override: bool, v_override: bool) {
        let mut stack = vec![(idx, h_override, v_override)];
        while let Some((i, ho, vo)) = stack.pop() {
            if !self.alive(i) {
                continue;
            }
            let b = self.n(i).b;
            let Some([c0, c1]) = self.n(i).children else { continue };

            let split_top = {
                let node = self.nm(i);
                if !cfg.preserve_split {
                    node.split_top = b.h * cfg.split_width_multiplier as f64 > b.w;
                }
                if vo {
                    node.split_top = true;
                } else if ho {
                    node.split_top = false;
                }
                node.split_top
            };
            let ratio = self.n(i).split_ratio as f64;

            if !split_top {
                // лево/право
                let first = (b.w / 2.0 * ratio).clamp(0.0, b.w);
                self.nm(c0).b = FBox { x: b.x, y: b.y, w: first, h: b.h };
                self.nm(c1).b = FBox { x: b.x + first, y: b.y, w: b.w - first, h: b.h };
            } else {
                // верх/низ
                let first = (b.h / 2.0 * ratio).clamp(0.0, b.h);
                self.nm(c0).b = FBox { x: b.x, y: b.y, w: b.w, h: first };
                self.nm(c1).b = FBox { x: b.x, y: b.y + first, w: b.w, h: b.h - first };
            }

            stack.push((c0, false, false));
            stack.push((c1, false, false));
        }
    }

    // ── вставка (addTarget) ──────────────────────────────────────────────────

    /// Вставляет окно, разделив пополам слот `opening_on` (обычно — узел
    /// сфокусированного окна). `focal` решает, какая половина достанется
    /// новому окну при `force_split = 0`. `override_dir` — принудительная
    /// сторона (используется movewindow'ом).
    pub fn insert(
        &mut self,
        window: T,
        opening_on: Option<usize>,
        focal: Point<f64, Logical>,
        work_area: Rectangle<i32, Logical>,
        cfg: &DwindleConfig,
        override_dir: Option<Dir>,
    ) {
        if self.contains(&window) {
            return;
        }
        let pnode = self.alloc(Node {
            parent: None,
            children: None,
            window: Some(window),
            split_top: false,
            split_ratio: 1.0,
            b: FBox::from_rect(work_area),
        });

        // Слот, который делим: живой лист, не сам вставляемый узел.
        let opening_on = opening_on
            .filter(|&i| i != pnode && self.alive(i) && self.n(i).children.is_none() && self.n(i).window.is_some())
            // Fail-safe как в Hyprland: кандидат не годится, а дерево не пустое —
            // берём любой другой лист. Иначе узел остался бы без родителя, то
            // есть вторым корнем, и его поддерево выпало бы из раскладки.
            .or_else(|| {
                self.nodes.iter().enumerate().find_map(|(i, n)| {
                    let n = n.as_ref()?;
                    (i != pnode && n.children.is_none() && n.window.is_some()).then_some(i)
                })
            });
        let Some(op) = opening_on else {
            // Первое окно — занимает всю рабочую область.
            return;
        };

        let op_box = self.n(op).b;
        let op_parent = self.n(op).parent;
        let newparent = self.alloc(Node {
            parent: op_parent,
            children: None,
            window: None,
            split_top: false,
            split_ratio: cfg.default_split_ratio.clamp(0.1, 1.9),
            b: op_box,
        });

        // Пропорции слота решают ось деления (шире → лево/право).
        let side_by_side = op_box.w > op_box.h * cfg.split_width_multiplier as f64;
        let (mut h_override, mut v_override) = (false, false);

        let new_is_first = match override_dir {
            Some(dir) => {
                if dir.is_vertical() {
                    v_override = true;
                } else {
                    h_override = true;
                }
                dir.takes_first_child()
            }
            None => match cfg.force_split {
                1 => true,
                2 => false,
                // Курсор в первой половине слота → новое окно туда же.
                _ => {
                    if side_by_side {
                        focal.x < op_box.x + op_box.w / 2.0
                    } else {
                        focal.y < op_box.y + op_box.h / 2.0
                    }
                }
            },
        };

        self.nm(newparent).split_top = if v_override {
            true
        } else if h_override {
            false
        } else {
            !side_by_side
        };
        self.nm(newparent).children =
            Some(if new_is_first { [pnode, op] } else { [op, pnode] });

        // Перевешиваем ссылку у деда.
        if let Some(gp) = op_parent {
            let ch = self.nm(gp).children.as_mut().expect("dwindle: у родителя нет детей");
            if ch[0] == op {
                ch[0] = newparent;
            } else {
                ch[1] = newparent;
            }
        }
        self.nm(op).parent = Some(newparent);
        self.nm(pnode).parent = Some(newparent);

        self.recalc_from(newparent, cfg, h_override, v_override);
    }

    // ── удаление (removeTarget) ─────────────────────────────────────────────

    /// Убирает окно; его место целиком забирает сосед по узлу.
    pub fn remove(&mut self, window: &T) {
        let Some(idx) = self.node_of(window) else { return };
        let Some(parent) = self.n(idx).parent else {
            self.release(idx);
            return;
        };
        let ch = self.n(parent).children.expect("dwindle: у родителя нет детей");
        let sibling = if ch[0] == idx { ch[1] } else { ch[0] };
        let grandparent = self.n(parent).parent;

        self.nm(sibling).parent = grandparent;
        if let Some(gp) = grandparent {
            let gch = self.nm(gp).children.as_mut().expect("dwindle: у деда нет детей");
            if gch[0] == parent {
                gch[0] = sibling;
            } else {
                gch[1] = sibling;
            }
        }
        // Сосед занимает слот исчезнувшего родителя (до следующего recalc).
        let pbox = self.n(parent).b;
        self.nm(sibling).b = pbox;

        self.release(parent);
        self.release(idx);
    }

    // ── операции над узлами ──────────────────────────────────────────────────

    /// Меняет местами два окна, не трогая форму дерева (`swapTargets`).
    pub fn swap(&mut self, a: &T, b: &T) {
        let (Some(ia), Some(ib)) = (self.node_of(a), self.node_of(b)) else { return };
        let wa = self.n(ia).window.clone();
        let wb = self.n(ib).window.clone();
        self.nm(ia).window = wb;
        self.nm(ib).window = wa;
    }

    /// `dwindle:togglesplit` — перевернуть ось ближайшего деления.
    /// Имеет смысл только при `preserve_split = true` (иначе следующий recalc
    /// вернёт ось из пропорций слота).
    pub fn toggle_split(&mut self, window: &T) -> bool {
        let Some(idx) = self.node_of(window) else { return false };
        let Some(parent) = self.n(idx).parent else { return false };
        let v = self.n(parent).split_top;
        self.nm(parent).split_top = !v;
        true
    }

    /// `dwindle:swapsplit` — поменять половины местами.
    pub fn swap_split(&mut self, window: &T) -> bool {
        let Some(idx) = self.node_of(window) else { return false };
        let Some(parent) = self.n(idx).parent else { return false };
        if let Some(ch) = self.nm(parent).children.as_mut() {
            ch.swap(0, 1);
        }
        true
    }

    /// `dwindle:splitratio` — подвинуть ближайшее деление.
    pub fn split_ratio_delta(&mut self, window: &T, delta: f32) -> bool {
        let Some(idx) = self.node_of(window) else { return false };
        let Some(parent) = self.n(idx).parent else { return false };
        // Для второго ребёнка «увеличить себя» = уменьшить долю первого.
        let first = self.n(parent).children.map(|c| c[0] == idx).unwrap_or(true);
        let signed = if first { delta } else { -delta };
        let new = (self.n(parent).split_ratio + signed).clamp(0.1, 1.9);
        self.nm(parent).split_ratio = new;
        true
    }

    /// `dwindle:movetoroot` — поднять окно на верхний уровень дерева (аналог
    /// dwm-шного zoom: окно занимает половину экрана целиком).
    pub fn move_to_root(&mut self, window: &T, stable: bool) -> bool {
        let Some(x) = self.node_of(window) else { return false };
        let Some(parent) = self.n(x).parent else { return false };
        if self.n(parent).parent.is_none() {
            return false; // уже ребёнок корня
        }

        // Идём вверх до корня, запоминая, какой его ребёнок — наш предок.
        let (mut ancestor, mut root) = (parent, parent);
        while let Some(p) = self.n(root).parent {
            ancestor = root;
            root = p;
        }

        let root_ch = self.n(root).children.expect("dwindle: корень без детей");
        let swap_slot = if root_ch[0] == ancestor { 1 } else { 0 };
        let swap_idx = root_ch[swap_slot];
        let x_slot = if self.n(parent).children.map(|c| c[0] == x).unwrap_or(false) { 0 } else { 1 };

        self.nm(parent).children.as_mut().unwrap()[x_slot] = swap_idx;
        self.nm(root).children.as_mut().unwrap()[swap_slot] = x;
        self.nm(x).parent = Some(root);
        self.nm(swap_idx).parent = Some(parent);

        // stable: фокус остаётся на той же стороне экрана.
        if stable {
            if let Some(ch) = self.nm(root).children.as_mut() {
                ch.swap(0, 1);
            }
        }
        true
    }

    /// `movewindow <dir>` — вынуть окно из дерева и вставить его рядом с
    /// соседом за соответствующим краем (`moveTargetInDirection`).
    /// Возвращает false, если в этом направлении никого нет.
    pub fn move_in_direction(
        &mut self,
        window: &T,
        dx: i32,
        dy: i32,
        work_area: Rectangle<i32, Logical>,
        cfg: &DwindleConfig,
    ) -> bool {
        let Some(dir) = Dir::from_delta(dx, dy) else { return false };
        let Some(idx) = self.node_of(window) else { return false };
        let b = self.n(idx).b;
        let focal = dir.focal_point(b);

        // За краем рабочей области двигать некуда.
        if !FBox::from_rect(work_area).contains(focal) {
            return false;
        }

        // Если едем прямо в ближайшее деление, а по ту сторону — одно окно,
        // форсируем сторону: иначе мы бы встали не туда, куда шли.
        let override_dir = self.n(idx).parent.and_then(|p| {
            let ch = self.n(p).children?;
            let split_top = self.n(p).split_top;
            let sibling = if ch[0] == idx { ch[1] } else { ch[0] };
            let sibling_is_leaf = self.n(sibling).children.is_none();
            let toward_divider = match dir {
                Dir::Up => split_top && ch[1] == idx,
                Dir::Down => split_top && ch[0] == idx,
                Dir::Left => !split_top && ch[1] == idx,
                Dir::Right => !split_top && ch[0] == idx,
            };
            (toward_divider && sibling_is_leaf).then_some(dir)
        });

        let w = window.clone();
        self.remove(&w);
        // Соседа ищем уже по актуальной геометрии (место удалённого окна
        // забрал его сосед по узлу).
        self.recalc(work_area, cfg);
        let opening_on = self.closest_node(focal, None);
        self.insert(w, opening_on, focal, work_area, cfg, override_dir);
        self.recalc(work_area, cfg);
        true
    }

    // ── ресайз (resizeTarget, smart_resizing) ───────────────────────────────

    /// Тянем окно за `corner` на `delta` (ИНКРЕМЕНТ с прошлого события):
    /// меняются `split_ratio` ближайших предков по каждой оси. `PVOUTER`/
    /// `PHOUTER` — деления, которые двигаются; `PVINNER`/`PHINNER` —
    /// компенсирующие, чтобы соседи не «плыли» вместе с окном.
    pub fn resize(
        &mut self,
        window: &T,
        delta: Point<f64, Logical>,
        corner: Corner,
        work_area: Rectangle<i32, Logical>,
        cfg: &DwindleConfig,
    ) {
        let Some(node) = self.node_of(window) else { return };
        let b = self.n(node).b;
        let area = FBox::from_rect(work_area);

        const STICK: f64 = 5.0;
        let display_left = (b.x - area.x).abs() < STICK;
        let display_right = ((b.x + b.w) - (area.x + area.w)).abs() < STICK;
        let display_top = (b.y - area.y).abs() < STICK;
        let display_bottom = ((b.y + b.h) - (area.y + area.h)).abs() < STICK;

        let mut mv = delta;
        if display_left && display_right {
            mv.x = 0.0;
        }
        if display_top && display_bottom {
            mv.y = 0.0;
        }
        if mv.x == 0.0 && mv.y == 0.0 {
            return;
        }

        let left = corner.left || display_right;
        let top = corner.top || display_bottom;
        let right = corner.right || display_left;
        let bottom = corner.bottom || display_top;
        let none = corner.is_none();

        let (mut pv_outer, mut pv_inner, mut ph_outer, mut ph_inner) = (None, None, None, None);
        let mut cur = node;
        while let Some(parent) = self.n(cur).parent {
            let ch = self.n(parent).children.expect("dwindle: у родителя нет детей");
            let split_top = self.n(parent).split_top;

            if pv_outer.is_none()
                && split_top
                && (none || (top && ch[1] == cur) || (bottom && ch[0] == cur))
            {
                pv_outer = Some(cur);
            } else if pv_outer.is_none() && pv_inner.is_none() && split_top {
                pv_inner = Some(cur);
            } else if ph_outer.is_none()
                && !split_top
                && (none || (left && ch[1] == cur) || (right && ch[0] == cur))
            {
                ph_outer = Some(cur);
            } else if ph_outer.is_none() && ph_inner.is_none() && !split_top {
                ph_inner = Some(cur);
            }

            if pv_outer.is_some() && ph_outer.is_some() {
                break;
            }
            cur = parent;
        }

        if let Some(outer) = ph_outer {
            let p = self.n(outer).parent.expect("dwindle: outer без родителя");
            let pw = self.n(p).b.w.max(1.0);
            let ratio = (self.n(p).split_ratio as f64 + mv.x * 2.0 / pw).clamp(0.1, 1.9);
            self.nm(p).split_ratio = ratio as f32;

            if let Some(inner) = ph_inner {
                let original = self.n(inner).b.w;
                self.recalc_from(p, cfg, false, false);
                let ip = self.n(inner).parent.expect("dwindle: inner без родителя");
                let ipw = self.n(ip).b.w.max(1.0);
                let is_first = self.n(ip).children.map(|c| c[0] == inner).unwrap_or(false);
                let ratio = if is_first {
                    (original - mv.x) / ipw * 2.0
                } else {
                    2.0 - (original + mv.x) / ipw * 2.0
                };
                self.nm(ip).split_ratio = ratio.clamp(0.1, 1.9) as f32;
                self.recalc_from(ip, cfg, false, false);
            } else {
                self.recalc_from(p, cfg, false, false);
            }
        }

        if let Some(outer) = pv_outer {
            let p = self.n(outer).parent.expect("dwindle: outer без родителя");
            let ph = self.n(p).b.h.max(1.0);
            let ratio = (self.n(p).split_ratio as f64 + mv.y * 2.0 / ph).clamp(0.1, 1.9);
            self.nm(p).split_ratio = ratio as f32;

            if let Some(inner) = pv_inner {
                let original = self.n(inner).b.h;
                self.recalc_from(p, cfg, false, false);
                let ip = self.n(inner).parent.expect("dwindle: inner без родителя");
                let iph = self.n(ip).b.h.max(1.0);
                let is_first = self.n(ip).children.map(|c| c[0] == inner).unwrap_or(false);
                let ratio = if is_first {
                    (original - mv.y) / iph * 2.0
                } else {
                    2.0 - (original + mv.y) / iph * 2.0
                };
                self.nm(ip).split_ratio = ratio.clamp(0.1, 1.9) as f32;
                self.recalc_from(ip, cfg, false, false);
            } else {
                self.recalc_from(p, cfg, false, false);
            }
        }
    }

    /// Размер, который получит следующее окно, если разделить слот `window`
    /// (`predictSizeForNewTarget`) — чтобы не слать клиенту лишний configure.
    pub fn predict_split_size(&self, window: &T, cfg: &DwindleConfig) -> Option<(f64, f64)> {
        let b = self.box_of(window)?;
        let split_top = b.h * cfg.split_width_multiplier as f64 > b.w;
        Some(if split_top { (b.w, b.h / 2.0) } else { (b.w / 2.0, b.h) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct W(u32);

    impl Leaf for W {
        fn same(&self, other: &Self) -> bool {
            self.0 == other.0
        }
    }

    /// 1600x900 — шире, чем высок, значит первый split будет лево/право.
    fn area() -> Rectangle<i32, Logical> {
        Rectangle::new((0, 0).into(), (1600, 900).into())
    }

    fn p(x: f64, y: f64) -> Point<f64, Logical> {
        Point::from((x, y))
    }

    /// Вставка "как из-под курсора": делим слот `on`, курсор в точке `focal`.
    fn add(tree: &mut DwindleTree<W>, w: u32, on: Option<W>, focal: Point<f64, Logical>) {
        let cfg = DwindleConfig::default();
        let node = on.and_then(|o| tree.node_of(&o));
        tree.insert(W(w), node, focal, area(), &cfg, None);
        tree.recalc(area(), &cfg);
    }

    fn rect_of(tree: &DwindleTree<W>, w: u32) -> Rectangle<i32, Logical> {
        tree.leaf_rects().into_iter().find(|(x, _)| x.0 == w).expect("нет такого окна").1
    }

    /// Окна не налезают друг на друга и покрывают всю рабочую область.
    fn assert_tiles_cover_area(tree: &DwindleTree<W>) {
        let rects = tree.leaf_rects();
        let total: i64 = rects.iter().map(|(_, r)| r.size.w as i64 * r.size.h as i64).sum();
        assert_eq!(total, 1600 * 900, "сумма площадей ≠ площадь экрана: {rects:?}");
        for (i, (_, a)) in rects.iter().enumerate() {
            for (_, b) in rects.iter().skip(i + 1) {
                assert!(a.intersection(*b).is_none(), "окна пересекаются: {a:?} и {b:?}");
            }
        }
    }

    #[test]
    fn single_window_takes_whole_area() {
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        assert_eq!(rect_of(&t, 1), area());
    }

    #[test]
    fn second_window_splits_focused_side_by_side() {
        // Экран шире, чем высок → делим лево/право; курсор справа → новое окно
        // занимает правую половину.
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        assert_eq!(rect_of(&t, 1), Rectangle::new((0, 0).into(), (800, 900).into()));
        assert_eq!(rect_of(&t, 2), Rectangle::new((800, 0).into(), (800, 900).into()));
        assert_tiles_cover_area(&t);
    }

    #[test]
    fn cursor_side_decides_which_half_is_new() {
        // Тот же случай, но курсор слева → новое окно уходит в левую половину.
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(100.0, 450.0));
        assert_eq!(rect_of(&t, 2), Rectangle::new((0, 0).into(), (800, 900).into()));
        assert_eq!(rect_of(&t, 1), Rectangle::new((800, 0).into(), (800, 900).into()));
    }

    #[test]
    fn third_window_splits_only_the_focused_slot() {
        // Ключевое отличие от старой цепочки: делится ИМЕННО слот
        // сфокусированного окна (2), окно 1 не шевелится. Слот 800x900 выше,
        // чем шире → делим верх/низ.
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        add(&mut t, 3, Some(W(2)), p(1500.0, 800.0));
        assert_eq!(rect_of(&t, 1), Rectangle::new((0, 0).into(), (800, 900).into()));
        assert_eq!(rect_of(&t, 2), Rectangle::new((800, 0).into(), (800, 450).into()));
        assert_eq!(rect_of(&t, 3), Rectangle::new((800, 450).into(), (800, 450).into()));
        assert_tiles_cover_area(&t);
    }

    #[test]
    fn splitting_the_first_window_leaves_the_rest_alone() {
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        add(&mut t, 3, Some(W(1)), p(400.0, 800.0)); // делим ЛЕВЫЙ слот
        assert_eq!(rect_of(&t, 2), Rectangle::new((800, 0).into(), (800, 900).into()));
        assert_eq!(rect_of(&t, 1), Rectangle::new((0, 0).into(), (800, 450).into()));
        assert_eq!(rect_of(&t, 3), Rectangle::new((0, 450).into(), (800, 450).into()));
        assert_tiles_cover_area(&t);
    }

    #[test]
    fn closing_a_window_gives_its_space_to_its_sibling() {
        // Закрыли 3 — его место забирает сосед по узлу (2), окно 1 не двигается.
        let cfg = DwindleConfig::default();
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        add(&mut t, 3, Some(W(2)), p(1500.0, 800.0));
        t.remove(&W(3));
        t.recalc(area(), &cfg);
        assert_eq!(rect_of(&t, 1), Rectangle::new((0, 0).into(), (800, 900).into()));
        assert_eq!(rect_of(&t, 2), Rectangle::new((800, 0).into(), (800, 900).into()));
        assert_tiles_cover_area(&t);
    }

    #[test]
    fn removing_the_last_window_empties_the_tree() {
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        t.remove(&W(1));
        assert!(t.is_empty());
        assert!(t.leaf_rects().is_empty());
        // Дерево должно снова работать после полного опустошения.
        add(&mut t, 7, None, p(0.0, 0.0));
        assert_eq!(rect_of(&t, 7), area());
    }

    #[test]
    fn move_in_direction_swaps_with_the_neighbour() {
        // 1 слева, 2 справа. Двигаем 1 вправо → они меняются местами.
        let cfg = DwindleConfig::default();
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        assert!(t.move_in_direction(&W(1), 1, 0, area(), &cfg));
        assert_eq!(rect_of(&t, 2), Rectangle::new((0, 0).into(), (800, 900).into()));
        assert_eq!(rect_of(&t, 1), Rectangle::new((800, 0).into(), (800, 900).into()));
        assert_tiles_cover_area(&t);
    }

    #[test]
    fn move_past_the_screen_edge_is_a_noop() {
        let cfg = DwindleConfig::default();
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        assert!(!t.move_in_direction(&W(1), -1, 0, area(), &cfg));
        assert_eq!(rect_of(&t, 1), Rectangle::new((0, 0).into(), (800, 900).into()));
    }

    #[test]
    fn move_to_root_promotes_a_deep_window_to_half_the_screen() {
        let cfg = DwindleConfig::default();
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        add(&mut t, 3, Some(W(2)), p(1500.0, 800.0));
        assert_eq!(rect_of(&t, 3).size, (800, 450).into());
        assert!(t.move_to_root(&W(3), true));
        t.recalc(area(), &cfg);
        assert_eq!(rect_of(&t, 3).size, (800, 900).into());
        assert_tiles_cover_area(&t);
        // Уже на верхнем уровне — повторный вызов ничего не даёт.
        assert!(!t.move_to_root(&W(3), true));
    }

    #[test]
    fn split_ratio_delta_grows_the_focused_window_either_way() {
        // Деление двигается "в пользу" сфокусированного окна независимо от
        // того, первое оно в узле или второе.
        let cfg = DwindleConfig::default();
        for focused in [1u32, 2u32] {
            let mut t = DwindleTree::<W>::default();
            add(&mut t, 1, None, p(0.0, 0.0));
            add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
            t.split_ratio_delta(&W(focused), 0.1);
            t.recalc(area(), &cfg);
            assert!(
                rect_of(&t, focused).size.w > 800,
                "окно {focused} не выросло: {:?}",
                t.leaf_rects()
            );
            assert_tiles_cover_area(&t);
        }
    }

    #[test]
    fn resize_moves_the_divider_between_two_windows() {
        // Тянем правый край левого окна на +100 → деление уезжает вправо.
        let cfg = DwindleConfig::default();
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 1, None, p(0.0, 0.0));
        add(&mut t, 2, Some(W(1)), p(1500.0, 450.0));
        let corner = Corner { right: true, ..Default::default() };
        t.resize(&W(1), p(100.0, 0.0), corner, area(), &cfg);
        assert_eq!(rect_of(&t, 1), Rectangle::new((0, 0).into(), (900, 900).into()));
        assert_eq!(rect_of(&t, 2), Rectangle::new((900, 0).into(), (700, 900).into()));
        assert_tiles_cover_area(&t);
    }

    #[test]
    fn many_windows_always_tile_without_gaps_or_overlap() {
        // Стресс: 12 окон в разные слоты, потом половину закрываем.
        let cfg = DwindleConfig::default();
        let mut t = DwindleTree::<W>::default();
        add(&mut t, 0, None, p(0.0, 0.0));
        for i in 1..12u32 {
            let on = W(i.saturating_sub(1) % i.max(1));
            let focal = p((i as f64 * 137.0) % 1600.0, (i as f64 * 311.0) % 900.0);
            add(&mut t, i, Some(on), focal);
            assert_tiles_cover_area(&t);
        }
        for i in (0..12u32).step_by(2) {
            t.remove(&W(i));
            t.recalc(area(), &cfg);
            assert_tiles_cover_area(&t);
        }
        assert_eq!(t.leaf_rects().len(), 6);
    }

    #[test]
    fn preserve_split_keeps_the_axis_a_toggle_put_there() {
        let cfg = DwindleConfig { preserve_split: true, ..Default::default() };
        let mut t = DwindleTree::<W>::default();
        t.insert(W(1), None, p(0.0, 0.0), area(), &cfg, None);
        let n1 = t.node_of(&W(1));
        t.insert(W(2), n1, p(1500.0, 450.0), area(), &cfg, None);
        t.recalc(area(), &cfg);
        assert_eq!(rect_of(&t, 1).size, (800, 900).into()); // лево/право
        assert!(t.toggle_split(&W(1)));
        t.recalc(area(), &cfg);
        assert_eq!(rect_of(&t, 1).size, (1600, 450).into()); // стало верх/низ
    }
}
