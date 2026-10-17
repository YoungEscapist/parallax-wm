//! Автоматическая склейка плавающих окон при ресайзе — по образцу driftwm
//! (~/driftwm, src/layout/cluster.rs и src/state/cluster_snapshot.rs).
//!
//! В driftwm окна образуют "кластер" по касанию краёв (без явной группировки,
//! в отличие от нашего "созвездия", см. selection.rs): ресайз одного окна
//! толкает соседей, чтобы между ними всегда был один и тот же зазор. Здесь
//! перенесена та же трёхфазная схема (статический сдвиг соседей на дельту
//! ресайза → сцепка bond'ами, если сосед уже сдвинут → каскад расталкивания
//! перпендикулярных соседей), но без driftwm-абстракции Stage/StageWindow —
//! окна parallax живут прямо в `smithay::desktop::Window` и `space`.
//!
//! Активируется тумблером Super+M (`is_glue_enabled`, см. state.rs), отдельно
//! от расталкивания (Super+A) и магнитирования (Super+G, было на Super+M).

use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::Hash;

use smithay::{
    desktop::Window,
    utils::{Logical, Point, Rectangle},
};

use crate::Parallax;
use crate::grabs::resize_grab::ResizeEdge;
use crate::tiling::Layout;

/// Зазор, который склейка старается держать между соседями. Плавающие окна
/// parallax обычно стоят впритык (не как driftwm, где есть настраиваемый
/// snap_gap) — 0 достаточно, EPS в `adjacent_side` прощает пиксель-два дрожи.
pub(crate) const GLUE_GAP: f64 = 0.0;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x_low: f64,
    pub x_high: f64,
    pub y_low: f64,
    pub y_high: f64,
}

impl Rect {
    pub(crate) fn from_geo(g: Rectangle<i32, Logical>) -> Self {
        Rect {
            x_low: g.loc.x as f64,
            x_high: (g.loc.x + g.size.w) as f64,
            y_low: g.loc.y as f64,
            y_high: (g.loc.y + g.size.h) as f64,
        }
    }

    fn translated(&self, dx: i32, dy: i32) -> Rect {
        Rect {
            x_low: self.x_low + dx as f64,
            x_high: self.x_high + dx as f64,
            y_low: self.y_low + dy as f64,
            y_high: self.y_high + dy as f64,
        }
    }
}

fn y_overlap(a: &Rect, b: &Rect) -> bool {
    a.y_low < b.y_high && b.y_low < a.y_high
}

fn x_overlap(a: &Rect, b: &Rect) -> bool {
    a.x_low < b.x_high && b.x_low < a.x_high
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

/// Какой стороной `a` касается `b` (с точностью EPS до `gap`). Диагональные
/// касания углом в кластер не входят — ровно как в driftwm.
pub fn adjacent_side(a: &Rect, b: &Rect, gap: f64) -> Option<Side> {
    const EPS: f64 = 2.0;

    if y_overlap(a, b) {
        if ((a.x_high + gap) - b.x_low).abs() <= EPS {
            return Some(Side::Right);
        }
        if (a.x_low - (b.x_high + gap)).abs() <= EPS {
            return Some(Side::Left);
        }
    }
    if x_overlap(a, b) {
        if ((a.y_high + gap) - b.y_low).abs() <= EPS {
            return Some(Side::Bottom);
        }
        if (a.y_low - (b.y_high + gap)).abs() <= EPS {
            return Some(Side::Top);
        }
    }
    None
}

/// Связная компонента `root` по касанию краёв (BFS). Всегда содержит `root`.
pub fn cluster_of<W>(root: &W, windows: &[(W, Rect)], gap: f64) -> HashSet<W>
where
    W: Clone + Eq + Hash,
{
    let rects: HashMap<&W, &Rect> = windows.iter().map(|(w, r)| (w, r)).collect();

    let mut visited: HashSet<W> = HashSet::new();
    let mut queue: VecDeque<W> = VecDeque::new();
    visited.insert(root.clone());
    queue.push_back(root.clone());

    while let Some(w) = queue.pop_front() {
        let Some(w_rect) = rects.get(&w) else { continue };
        for (other, other_rect) in windows {
            if visited.contains(other) {
                continue;
            }
            if adjacent_side(w_rect, other_rect, gap).is_some() {
                visited.insert(other.clone());
                queue.push_back(other.clone());
            }
        }
    }

    visited
}

#[derive(Clone, Copy, Debug)]
struct ResizeClassification {
    axis_x: Option<Side>,
    axis_y: Option<Side>,
    initial_rect: Rect,
}

type ShiftsAndBonds = (HashMap<usize, (i32, i32)>, Vec<(usize, usize)>);

/// Три фазы (см. driftwm `resolve_cluster_shifts`):
///  1. статический сдвиг — прямые/нижестоящие соседи по растущей стороне
///     едут на ту же дельту, что и она;
///  2. bond-сдвиг — сосед, однажды пойманный краем в фазе 3, дальше
///     безусловно следует за этим краем ± gap, даже если драг развернулся;
///  3. каскад расталкивания — пока кто-то ещё сдвигается, толкаем тех, на
///     кого он наехал (в т.ч. от самого resize-окна), запоминая новые bond'ы.
fn resolve_cluster_shifts(
    members: &[ResizeClassification],
    width_delta: i32,
    height_delta: i32,
    gap: f64,
    existing_bonds: &[(usize, usize)],
    primary: Option<(Rect, Rect)>,
) -> ShiftsAndBonds {
    let mut shifts: HashMap<usize, (i32, i32)> = HashMap::new();
    if members.is_empty() {
        return (shifts, Vec::new());
    }

    for (i, m) in members.iter().enumerate() {
        let dx = match m.axis_x {
            Some(Side::Right) => width_delta,
            Some(Side::Left) => -width_delta,
            _ => 0,
        };
        let dy = match m.axis_y {
            Some(Side::Bottom) => height_delta,
            Some(Side::Top) => -height_delta,
            _ => 0,
        };
        if dx != 0 || dy != 0 {
            shifts.insert(i, (dx, dy));
        }
    }

    for &(m_idx, n_idx) in existing_bonds {
        if m_idx >= members.len() || n_idx >= members.len() {
            continue;
        }
        let (mdx, mdy) = shifts.get(&m_idx).copied().unwrap_or((0, 0));
        let m_init = &members[m_idx].initial_rect;
        let m_cur = m_init.translated(mdx, mdy);
        let n_init = &members[n_idx].initial_rect;
        let (ndx, ndy) = shifts.get(&n_idx).copied().unwrap_or((0, 0));
        let n_cur = n_init.translated(ndx, ndy);

        let mut new_ndx = ndx;
        let mut new_ndy = ndy;

        if n_init.x_low >= m_init.x_high && y_overlap(&m_cur, &n_cur) {
            new_ndx = (m_cur.x_high + gap - n_init.x_low).ceil() as i32;
        } else if n_init.x_high <= m_init.x_low && y_overlap(&m_cur, &n_cur) {
            new_ndx = (m_cur.x_low - gap - n_init.x_high).floor() as i32;
        }
        if n_init.y_low >= m_init.y_high && x_overlap(&m_cur, &n_cur) {
            new_ndy = (m_cur.y_high + gap - n_init.y_low).ceil() as i32;
        } else if n_init.y_high <= m_init.y_low && x_overlap(&m_cur, &n_cur) {
            new_ndy = (m_cur.y_low - gap - n_init.y_high).floor() as i32;
        }

        if (new_ndx, new_ndy) != (ndx, ndy) {
            shifts.insert(n_idx, (new_ndx, new_ndy));
        }
    }

    if shifts.is_empty() && primary.is_none() {
        return (shifts, Vec::new());
    }

    let mut new_bonds: Vec<(usize, usize)> = Vec::new();
    let mut new_bonds_set: HashSet<(usize, usize)> = HashSet::new();

    for _ in 0..(members.len() * 2) {
        let mut changed = false;

        if let Some((p_init, p_cur)) = primary {
            for (j, n_entry) in members.iter().enumerate() {
                let (jdx, jdy) = shifts.get(&j).copied().unwrap_or((0, 0));
                let n_cur = n_entry.initial_rect.translated(jdx, jdy);
                let push = compute_push_from_primary(&p_init, &p_cur, &n_entry.initial_rect, &n_cur, gap);
                if push != (0, 0) {
                    let new = (jdx + push.0, jdy + push.1);
                    if new != (jdx, jdy) {
                        shifts.insert(j, new);
                        changed = true;
                    }
                }
            }
        }

        for (i, m_entry) in members.iter().enumerate() {
            let Some(&(idx, idy)) = shifts.get(&i) else { continue };
            if idx == 0 && idy == 0 {
                continue;
            }
            let m_init = m_entry.initial_rect;
            let m_cur = m_init.translated(idx, idy);

            for (j, n_entry) in members.iter().enumerate() {
                if j == i {
                    continue;
                }
                let (jdx, jdy) = shifts.get(&j).copied().unwrap_or((0, 0));
                let n_init = n_entry.initial_rect;
                let n_cur = n_init.translated(jdx, jdy);

                let push = compute_push(&m_init, &m_cur, &n_init, &n_cur, (idx, idy), gap);
                if push == (0, 0) {
                    continue;
                }
                let new = (jdx + push.0, jdy + push.1);
                if new != (jdx, jdy) {
                    shifts.insert(j, new);
                    if new_bonds_set.insert((i, j)) {
                        new_bonds.push((i, j));
                    }
                    changed = true;
                }
            }
        }

        if !changed {
            break;
        }
    }

    (shifts, new_bonds)
}

fn compute_push(
    m_init: &Rect,
    m_cur: &Rect,
    n_init: &Rect,
    n_cur: &Rect,
    (mdx, mdy): (i32, i32),
    gap: f64,
) -> (i32, i32) {
    let mut push = (0, 0);

    if mdx > 0 && n_init.x_low >= m_init.x_high && y_overlap(m_cur, n_cur) {
        let encroach = m_cur.x_high + gap - n_cur.x_low;
        if encroach > 0.0 {
            push.0 = encroach.ceil() as i32;
        }
    } else if mdx < 0 && n_init.x_high <= m_init.x_low && y_overlap(m_cur, n_cur) {
        let encroach = m_cur.x_low - gap - n_cur.x_high;
        if encroach < 0.0 {
            push.0 = encroach.floor() as i32;
        }
    }

    if mdy > 0 && n_init.y_low >= m_init.y_high && x_overlap(m_cur, n_cur) {
        let encroach = m_cur.y_high + gap - n_cur.y_low;
        if encroach > 0.0 {
            push.1 = encroach.ceil() as i32;
        }
    } else if mdy < 0 && n_init.y_high <= m_init.y_low && x_overlap(m_cur, n_cur) {
        let encroach = m_cur.y_low - gap - n_cur.y_high;
        if encroach < 0.0 {
            push.1 = encroach.floor() as i32;
        }
    }

    push
}

fn compute_push_from_primary(
    p_init: &Rect,
    p_cur: &Rect,
    n_init: &Rect,
    n_cur: &Rect,
    gap: f64,
) -> (i32, i32) {
    let mut push = (0i32, 0i32);

    if n_init.x_low >= p_init.x_high && y_overlap(p_cur, n_cur) {
        let encroach = p_cur.x_high + gap - n_cur.x_low;
        if encroach > 0.0 {
            push.0 = encroach.ceil() as i32;
        }
    }
    if n_init.x_high <= p_init.x_low && y_overlap(p_cur, n_cur) {
        let encroach = p_cur.x_low - gap - n_cur.x_high;
        if encroach < 0.0 {
            push.0 = encroach.floor() as i32;
        }
    }
    if n_init.y_low >= p_init.y_high && x_overlap(p_cur, n_cur) {
        let encroach = p_cur.y_high + gap - n_cur.y_low;
        if encroach > 0.0 {
            push.1 = encroach.ceil() as i32;
        }
    }
    if n_init.y_high <= p_init.y_low && x_overlap(p_cur, n_cur) {
        let encroach = p_cur.y_low - gap - n_cur.y_high;
        if encroach < 0.0 {
            push.1 = encroach.floor() as i32;
        }
    }

    push
}

// ── Бросок при склейке ───────────────────────────────────────────────────────
//
// Окно, отпущенное с разгона, летит по инерции на |v|/ω. Без склейки его
// путь никак не связан с соседями: оно пролетало сквозь край, в который
// целилось, или останавливалось в стороне, и кластер не складывался — прилипало
// только медленно подведённое окно (магнит на отпускании). Здесь путь полёта
// считается заранее, целиком (swept AABB), и цель пружины ставится ровно туда,
// где окно (или вся едущая группа) первый раз коснётся соседа.

/// Первое касание на пути броска: сдвиг, который надо применить ко всей
/// едущей группе, чтобы она остановилась впритык к препятствию.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Contact {
    pub dx: f64,
    pub dy: f64,
    /// Удар пришёлся по вертикальному краю (движение по X упёрлось).
    pub axis_x: bool,
    /// Какой член группы упёрся и во что — для подравнивания.
    pub mover: usize,
    pub obstacle: usize,
}

/// Отрезок времени (в долях пути), когда проекции пересекаются по одной оси.
/// None — по этой оси они не пересекутся никогда (движения нет, и уже порознь).
fn axis_times(m_lo: f64, m_hi: f64, o_lo: f64, o_hi: f64, d: f64) -> Option<(f64, f64)> {
    if d > 0.0 {
        Some(((o_lo - m_hi) / d, (o_hi - m_lo) / d))
    } else if d < 0.0 {
        Some(((o_hi - m_lo) / d, (o_lo - m_hi) / d))
    } else if m_lo < o_hi && o_lo < m_hi {
        Some((f64::NEG_INFINITY, f64::INFINITY))
    } else {
        None
    }
}

/// Где группа `movers`, едущая на (dx, dy), первый раз упрётся в одно из
/// `obstacles` (с зазором `gap`). Пары, которые перекрываются уже на старте,
/// пропускаются: из такого нахлёста окно выпускаем, а не держим. Касание
/// краем, вдоль которого окно скользит, ударом не считается — по стыку
/// кластера окно можно протащить.
pub fn first_contact(movers: &[Rect], obstacles: &[Rect], dx: f64, dy: f64, gap: f64) -> Option<Contact> {
    if dx == 0.0 && dy == 0.0 {
        return None;
    }
    // Проникновение меньше EPS не считаем: окна, стоящие впритык с дрожью в
    // пиксель (EPS как в adjacent_side), иначе «врезались» бы друг в друга
    // на старте.
    const EPS: f64 = 1.0;
    let mut best: Option<(f64, Contact)> = None;
    for (mi, m) in movers.iter().enumerate() {
        for (oi, o) in obstacles.iter().enumerate() {
            let o = Rect {
                x_low: o.x_low - gap + EPS,
                x_high: o.x_high + gap - EPS,
                y_low: o.y_low - gap + EPS,
                y_high: o.y_high + gap - EPS,
            };
            let Some((ex, xx)) = axis_times(m.x_low, m.x_high, o.x_low, o.x_high, dx) else { continue };
            let Some((ey, xy)) = axis_times(m.y_low, m.y_high, o.y_low, o.y_high, dy) else { continue };
            let entry = ex.max(ey);
            let exit = xx.min(xy);
            if entry >= exit || entry < 0.0 || entry > 1.0 {
                continue;
            }
            if best.is_some_and(|(t, _)| t <= entry) {
                continue;
            }
            let axis_x = ex >= ey;
            let (orig, t) = (&obstacles[oi], entry);
            let mut c = Contact { dx: dx * t, dy: dy * t, axis_x, mover: mi, obstacle: oi };
            // Сдвиг по оси удара — точный, до края соседа, а не t·d:
            // иначе EPS и округление оставляли щель в пиксель, и окно не
            // входило в кластер (adjacent_side прощает 2 px, но копится).
            if axis_x {
                c.dx = if dx > 0.0 { orig.x_low - gap - m.x_high } else { orig.x_high + gap - m.x_low };
            } else {
                c.dy = if dy > 0.0 { orig.y_low - gap - m.y_high } else { orig.y_high + gap - m.y_low };
            }
            best = Some((entry, c));
        }
    }
    best.map(|(_, c)| c)
}

/// Подравнять прилипшее окно `m` вдоль стыка с `o`: край к краю того же
/// направления (верх к верху, низ к низу — или левый к левому при ударе
/// сверху/снизу), если он ближе `dist`. Возвращает сдвиг по перпендикулярной
/// оси. Угол к углу (верх к низу) не берём — это увело бы окно со стыка.
pub fn align_along(m: &Rect, o: &Rect, axis_x: bool, dist: f64) -> f64 {
    let (m_lo, m_hi, o_lo, o_hi) = if axis_x {
        (m.y_low, m.y_high, o.y_low, o.y_high)
    } else {
        (m.x_low, m.x_high, o.x_low, o.x_high)
    };
    [o_lo - m_lo, o_hi - m_hi]
        .into_iter()
        .filter(|d| d.abs() <= dist)
        .min_by(|a, b| a.abs().total_cmp(&b.abs()))
        .unwrap_or(0.0)
}

/// Место окна в сетке склейки.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridCell {
    /// Индекс во входном списке.
    pub index: usize,
    pub loc: Point<i32, Logical>,
    pub size: smithay::utils::Size<i32, Logical>,
}

/// Сетка впритык для `rects` (текущие места окон), центрированная там же, где
/// стояла группа. Порядок — «как читают»: по рядам сверху вниз, в ряду слева
/// направо, поэтому окна почти не перелетают через всю кучу.
///
/// Окна НЕ ужимаются: колонка шириной с самое широкое своё окно, ряд высотой с
/// самое высокое, и каждое окно растягивается на свою ячейку. Ужимать нельзя —
/// клиент с минимумом (Discord 940×500) не сядет в ячейку и наедет на соседа;
/// растягивать можно всегда. Зазора нет: сетка сразу кластер склейки.
pub fn glue_grid(rects: &[Rectangle<i32, Logical>]) -> Vec<GridCell> {
    let n = rects.len();
    if n == 0 {
        return Vec::new();
    }
    let cols = (n as f64).sqrt().ceil() as usize;
    let rows = n.div_ceil(cols);

    let center = |r: &Rectangle<i32, Logical>| (r.loc.x as f64 + r.size.w as f64 / 2.0, r.loc.y as f64 + r.size.h as f64 / 2.0);
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by(|&a, &b| center(&rects[a]).1.total_cmp(&center(&rects[b]).1));
    for row in order.chunks_mut(cols) {
        row.sort_by(|&a, &b| center(&rects[a]).0.total_cmp(&center(&rects[b]).0));
    }

    let mut col_w = vec![0i32; cols];
    let mut row_h = vec![0i32; rows];
    for (k, &i) in order.iter().enumerate() {
        let (r, c) = (k / cols, k % cols);
        col_w[c] = col_w[c].max(rects[i].size.w);
        row_h[r] = row_h[r].max(rects[i].size.h);
    }
    let total_w: i32 = col_w.iter().sum();
    let total_h: i32 = row_h.iter().sum();

    let bbox = rects.iter().copied().reduce(|a, b| a.merge(b)).unwrap_or_default();
    let x0 = bbox.loc.x + (bbox.size.w - total_w) / 2;
    let y0 = bbox.loc.y + (bbox.size.h - total_h) / 2;

    order
        .iter()
        .enumerate()
        .map(|(k, &i)| {
            let (r, c) = (k / cols, k % cols);
            let x = x0 + col_w[..c].iter().sum::<i32>();
            let y = y0 + row_h[..r].iter().sum::<i32>();
            GridCell { index: i, loc: (x, y).into(), size: (col_w[c], row_h[r]).into() }
        })
        .collect()
}

/// Один член кластера, захваченный на старте ресайза.
pub struct GlueMember {
    pub window: Window,
    pub initial_pos: Point<i32, Logical>,
    initial_rect: Rect,
    axis_x: Option<Side>,
    axis_y: Option<Side>,
}

/// Снимок кластера на старте ресайза — живёт ровно на время хвата
/// (`ResizeSurfaceGrab`), bond'ы не переживают отпускание кнопки.
pub struct GlueResizeSnapshot {
    members: Vec<GlueMember>,
    bonds: Vec<(usize, usize)>,
    bonds_set: HashSet<(usize, usize)>,
    primary_rect: Rect,
    /// Битовая маска активных краёв ресайза — те же биты, что и у
    /// `ResizeEdge` (top=1, bottom=2, left=4, right=8), так что можно взять
    /// `edges.bits()` напрямую.
    resize_edges: u32,
}

impl GlueResizeSnapshot {
    pub fn empty() -> Self {
        Self {
            members: Vec::new(),
            bonds: Vec::new(),
            bonds_set: HashSet::new(),
            primary_rect: Rect { x_low: 0.0, x_high: 0.0, y_low: 0.0, y_high: 0.0 },
            resize_edges: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Пересчитывает сдвиги для текущей дельты размера и возвращает
    /// `(окно, новая_позиция)` для каждого сдвинутого члена.
    pub fn compute_member_positions(
        &mut self,
        width_delta: i32,
        height_delta: i32,
    ) -> Vec<(Window, Point<i32, Logical>)> {
        if self.members.is_empty() {
            return Vec::new();
        }

        let mut p_cur = self.primary_rect;
        if self.resize_edges & 8 != 0 {
            p_cur.x_high += width_delta as f64;
        }
        if self.resize_edges & 4 != 0 {
            p_cur.x_low -= width_delta as f64;
        }
        if self.resize_edges & 2 != 0 {
            p_cur.y_high += height_delta as f64;
        }
        if self.resize_edges & 1 != 0 {
            p_cur.y_low -= height_delta as f64;
        }
        let primary = Some((self.primary_rect, p_cur));

        let classifications: Vec<ResizeClassification> = self
            .members
            .iter()
            .map(|m| ResizeClassification {
                axis_x: m.axis_x,
                axis_y: m.axis_y,
                initial_rect: m.initial_rect,
            })
            .collect();

        let (shifts, new_bonds) = resolve_cluster_shifts(
            &classifications,
            width_delta,
            height_delta,
            GLUE_GAP,
            &self.bonds,
            primary,
        );
        for bond in new_bonds {
            if self.bonds_set.insert(bond) {
                self.bonds.push(bond);
            }
        }

        shifts
            .into_iter()
            .map(|(i, (dx, dy))| {
                let m = &self.members[i];
                (m.window.clone(), m.initial_pos + Point::from((dx, dy)))
            })
            .collect()
    }
}

impl Parallax {
    /// Super+M: с выделением из двух и больше окон — собрать их в сетку
    /// впритык (см. `glue_grid`) и включить склейку, чтобы сетка сразу жила
    /// кластером (ресайз одного толкает соседей). Без выделения — просто
    /// тумблер склейки, как было.
    ///
    /// Выделение, которое целиком и есть уже собранная сетка, — РАСПУСТИТЬ её
    /// (окна возвращаются на места и в размеры до сборки), ровно как Win+D
    /// разбирает целое созвездие (просьба Ярика 30.09.2026).
    pub fn glue_toggle_or_grid(&mut self) {
        if self.selected_windows.len() >= 2 {
            if let Some(idx) = self.selection_glue_grid() {
                self.dissolve_glue_grid(idx);
                return;
            }
            if self.glue_grid_selected() {
                return;
            }
        }
        self.is_glue_enabled = !self.is_glue_enabled;
        tracing::info!("plx: is_glue_enabled={}", self.is_glue_enabled);
    }

    /// Сетка из выделенных окон. false — собирать нечего (меньше двух окон
    /// с геометрией на холсте), тогда Super+M работает обычным тумблером.
    fn glue_grid_selected(&mut self) -> bool {
        let члены: Vec<(Window, Rectangle<i32, Logical>)> = self.selected_windows.iter()
            .filter(|w| !self.виджет_ли(w) && !self.is_fullscreen(w))
            .filter_map(|w| {
                // Место — где окно остановится, а не кадр полёта.
                let g = self.space.element_geometry(w)?;
                let loc = self.window_anim_target(w).unwrap_or(g.loc);
                Some((w.clone(), Rectangle::new(loc, g.size)))
            })
            .collect();
        if члены.len() < 2 {
            return false;
        }
        // Склейка живёт на свободном холсте (как и созвездия, см.
        // gather_selected_into_constellation): в тайлинге сетку тут же
        // переложила бы раскладка.
        if self.tile_config.layout != Layout::Float {
            self.set_layout(Layout::Float);
        }

        let rects: Vec<Rectangle<i32, Logical>> = члены.iter().map(|(_, r)| *r).collect();
        let ячейки = glue_grid(&rects);
        // Откуда окна взяли — для роспуска. Только если ещё не запомнено:
        // пересборка уже собранной сетки не должна стирать исходное место.
        for (w, было) in &члены {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == w) {
                if tw.pre_glue.is_none() {
                    tw.pre_glue = Some((было.loc, было.size));
                }
            }
        }
        // Окно не может жить в двух сетках: старые членства снимаем.
        for (w, _) in &члены {
            for g in &mut self.glue_grids {
                g.retain(|x| x != w);
            }
        }
        self.glue_grids.retain(|g| g.len() > 1);
        self.glue_grids.push(члены.iter().map(|(w, _)| w.clone()).collect());
        for я in &ячейки {
            let (w, было) = &члены[я.index];
            if было.size != я.size {
                crate::xwin::set_size(w, Some(я.size), crate::xwin::Tiled::Keep);
                crate::xwin::configure(w);
            }
            self.animate_window_to_dur(w, я.loc, crate::anim::дуг::созвездие());
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == w) {
                tw.floating = true;
                tw.float_position = я.loc;
                tw.position = я.loc;
                tw.float_position_set = true;
                tw.float_size = Some(я.size);
            }
        }
        self.is_glue_enabled = true;
        tracing::info!("plx: glue grid — {} окон в сетку, склейка включена", ячейки.len());
        self.selected_windows.clear();
        self.request_plane_reset();
        self.request_redraw();
        true
    }

    fn glue_grid_index_of(&self, window: &Window) -> Option<usize> {
        self.glue_grids.iter().position(|g| g.iter().any(|w| w == window))
    }

    /// Выделение целиком — одна собранная сетка склейки. Её номер или `None`.
    pub fn selection_glue_grid(&self) -> Option<usize> {
        let first = self.selected_windows.first()?;
        let idx = self.glue_grid_index_of(first)?;
        self.selected_windows
            .iter()
            .all(|w| self.glue_grid_index_of(w) == Some(idx))
            .then_some(idx)
    }

    /// Распустить сетку: окна едут на места и в размеры до сборки
    /// (`TaggedWindow::pre_glue`) — та же обратная операция, что
    /// `scatter_selected_constellation` у созвездия. Окна, чьё прежнее место
    /// неизвестно, остаются как стоят.
    pub fn dissolve_glue_grid(&mut self, idx: usize) {
        if idx >= self.glue_grids.len() {
            return;
        }
        let group = self.glue_grids.remove(idx);
        let mut вернули = 0usize;
        for w in &group {
            let было = self
                .tagged_windows
                .iter_mut()
                .find(|tw| &tw.window == w)
                .and_then(|tw| tw.pre_glue.take());
            let Some((место, размер)) = было else { continue };
            // Размер первым — как у созвездия: позиция считается под него.
            crate::xwin::set_size(w, Some(размер), crate::xwin::Tiled::Keep);
            crate::xwin::configure(w);
            self.animate_window_to_dur(w, место, crate::anim::дуг::созвездие());
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == w) {
                tw.floating = true;
                tw.float_position = место;
                tw.position = место;
                tw.float_position_set = true;
                tw.float_size = Some(размер);
            }
            вернули += 1;
        }
        tracing::info!(
            "plx: glue grid dissolved ({} windows, {} returned to their places)",
            group.len(), вернули,
        );
        self.selected_windows.clear();
        self.request_plane_reset();
        self.request_redraw();
    }

    /// Super+Shift+G без созвездия: распустить сетку НА МЕСТЕ — окна стоят,
    /// где стоят, забывается только членство (как роспуск созвездия на месте).
    /// Ищет по выделению, затем по фокусу. `true` — было что распускать.
    pub fn ungroup_glue_grid_in_place(&mut self) -> bool {
        let idx = self
            .selected_windows
            .iter()
            .find_map(|w| self.glue_grid_index_of(w))
            .or_else(|| self.focused_window().and_then(|f| self.glue_grid_index_of(&f)));
        let Some(idx) = idx else { return false };
        let group = self.glue_grids.remove(idx);
        for tw in self.tagged_windows.iter_mut() {
            if group.iter().any(|w| w == &tw.window) {
                tw.pre_glue = None;
            }
        }
        tracing::info!("plx: glue grid dissolved in place ({} windows)", group.len());
        self.request_redraw();
        true
    }

    /// Снимок кластера окна `window` для интерактивного ресайза с краями
    /// `edges`: прямые соседи по растущей стороне (плюс нижестоящие через
    /// BFS по графу касаний) — статический сдвиг-набор, остальной кластер
    /// едет следом только если каскад в них упрётся.
    ///
    /// Кандидаты — плавающие немаксимизированные окна на тех же тегах, что
    /// и `window` (виджеты и полноэкранные исключены, как и в снимках драга
    /// созвездия, см. selection.rs). Пусто, если склейка выключена, окно не
    /// нашло своей геометрии, или в кластере, кроме него, никого нет.
    #[allow(clippy::mutable_key_type)]
    pub fn glue_cluster_snapshot_for_resize(
        &self,
        window: &Window,
        edges: ResizeEdge,
    ) -> GlueResizeSnapshot {
        if !self.is_glue_enabled {
            return GlueResizeSnapshot::empty();
        }

        let my_tags = self
            .tagged_windows
            .iter()
            .find(|tw| &tw.window == window)
            .map(|tw| tw.tags)
            .unwrap_or(0);

        // Плавающее — либо помечено индивидуально (tw.floating), либо весь
        // текущий layout — Float (тогда тайлинг не трогает никого вовсе, но
        // tw.floating у обычных окон при этом остаётся false, см. arrange()).
        // Та же проверка, что в resize_grab.rs::is_tiled, только наоборот.
        let layout_is_float = self.tile_config.layout == Layout::Float;

        let mut rects: Vec<(Window, Rect)> = Vec::new();
        for tw in &self.tagged_windows {
            if !layout_is_float && !tw.floating {
                continue;
            }
            if tw.window != *window && tw.tags & my_tags == 0 {
                continue;
            }
            if self.виджет_ли(&tw.window) || self.is_fullscreen(&tw.window) {
                continue;
            }
            let Some(geo) = self.space.element_geometry(&tw.window) else { continue };
            rects.push((tw.window.clone(), Rect::from_geo(geo)));
        }

        let Some(primary_rect) = rects.iter().find(|(w, _)| w == window).map(|(_, r)| *r) else {
            return GlueResizeSnapshot::empty();
        };

        let full = cluster_of(window, &rects, GLUE_GAP);
        if full.len() <= 1 {
            return GlueResizeSnapshot::empty();
        }

        let rect_of: HashMap<Window, Rect> = rects.iter().cloned().collect();

        let compute_shift_set = |side: Side| -> HashSet<Window> {
            let mut set: HashSet<Window> = HashSet::new();
            let mut queue: VecDeque<Window> = VecDeque::new();
            for (w, r) in &rects {
                if w == window {
                    continue;
                }
                if adjacent_side(&primary_rect, r, GLUE_GAP) == Some(side) {
                    set.insert(w.clone());
                    queue.push_back(w.clone());
                }
            }

            let is_downstream = |r: &Rect| -> bool {
                match side {
                    Side::Right => r.x_low >= primary_rect.x_high,
                    Side::Left => r.x_high <= primary_rect.x_low,
                    Side::Bottom => r.y_low >= primary_rect.y_high,
                    Side::Top => r.y_high <= primary_rect.y_low,
                }
            };

            while let Some(current) = queue.pop_front() {
                let Some(cur_rect) = rect_of.get(&current).copied() else { continue };
                for (w, r) in &rects {
                    if w == window || set.contains(w) || !full.contains(w) {
                        continue;
                    }
                    if adjacent_side(&cur_rect, r, GLUE_GAP).is_none() {
                        continue;
                    }
                    if !is_downstream(r) {
                        continue;
                    }
                    set.insert(w.clone());
                    queue.push_back(w.clone());
                }
            }

            set
        };

        let right_set = if edges.intersects(ResizeEdge::RIGHT) { compute_shift_set(Side::Right) } else { HashSet::new() };
        let left_set = if edges.intersects(ResizeEdge::LEFT) { compute_shift_set(Side::Left) } else { HashSet::new() };
        let bottom_set = if edges.intersects(ResizeEdge::BOTTOM) { compute_shift_set(Side::Bottom) } else { HashSet::new() };
        let top_set = if edges.intersects(ResizeEdge::TOP) { compute_shift_set(Side::Top) } else { HashSet::new() };

        let mut members = Vec::new();
        for m in full {
            if &m == window {
                continue;
            }
            let Some(initial_pos) = self.space.element_geometry(&m).map(|g| g.loc) else { continue };
            let Some(initial_rect) = rect_of.get(&m).copied() else { continue };
            let axis_x = if right_set.contains(&m) {
                Some(Side::Right)
            } else if left_set.contains(&m) {
                Some(Side::Left)
            } else {
                None
            };
            let axis_y = if bottom_set.contains(&m) {
                Some(Side::Bottom)
            } else if top_set.contains(&m) {
                Some(Side::Top)
            } else {
                None
            };
            members.push(GlueMember { window: m, initial_pos, initial_rect, axis_x, axis_y });
        }

        GlueResizeSnapshot {
            members,
            bonds: Vec::new(),
            bonds_set: HashSet::new(),
            primary_rect,
            resize_edges: edges.bits(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x_low: x, x_high: x + w, y_low: y, y_high: y + h }
    }

    #[test]
    fn бросок_встаёт_впритык() {
        // Окно 100×100 в (0,0) летит вправо на 500, сосед стоит с x=300.
        let c = first_contact(&[r(0., 0., 100., 100.)], &[r(300., 20., 100., 100.)], 500., 37., 0.).unwrap();
        assert!(c.axis_x);
        assert_eq!(c.dx, 200.0);
        // Время касания считается по соседу, ужатому на EPS, — отсюда доли пикселя.
        assert!((c.dy - 37. * 0.4).abs() < 0.5);
    }

    #[test]
    fn мимо_и_вдоль_стыка_не_удар() {
        // Пролетает выше соседа.
        assert!(first_contact(&[r(0., 0., 100., 100.)], &[r(300., 200., 100., 100.)], 500., 0., 0.).is_none());
        // Стоит впритык справа и скользит вниз вдоль стыка.
        assert!(first_contact(&[r(0., 0., 100., 100.)], &[r(100., 0., 100., 100.)], 0., 300., 0.).is_none());
        // Уже перекрываются — выпускаем.
        assert!(first_contact(&[r(0., 0., 100., 100.)], &[r(50., 0., 100., 100.)], 300., 0., 0.).is_none());
    }

    #[test]
    fn группа_упирается_ближним() {
        // Второй член группы ближе к стене — он и определяет остановку.
        let c = first_contact(&[r(0., 0., 100., 100.), r(150., 0., 100., 100.)], &[r(400., 0., 50., 50.)], 1000., 0., 0.).unwrap();
        assert_eq!(c.mover, 1);
        assert_eq!(c.dx, 150.0);
    }

    #[test]
    fn подравнивание_вдоль_стыка() {
        assert_eq!(align_along(&r(0., 13., 100., 100.), &r(100., 0., 100., 300.), true, 20.), -13.0);
        assert_eq!(align_along(&r(0., 50., 100., 100.), &r(100., 0., 100., 300.), true, 20.), 0.0);
    }

    #[test]
    fn сетка_впритык_без_ужатия() {
        let g = |x, y, w, h| Rectangle::<i32, Logical>::new((x, y).into(), (w, h).into());
        let cells = glue_grid(&[g(0, 0, 400, 300), g(900, 20, 600, 200), g(10, 700, 300, 500)]);
        assert_eq!(cells.len(), 3);
        let rects: Vec<(usize, Rect)> = cells.iter()
            .map(|c| (c.index, Rect::from_geo(Rectangle::new(c.loc, c.size))))
            .collect();
        // Никто не ужат.
        for c in &cells {
            let was = [(400, 300), (600, 200), (300, 500)][c.index];
            assert!(c.size.w >= was.0 && c.size.h >= was.1);
        }
        // Всё — один кластер.
        let all = cluster_of(&rects[0].0, &rects, 0.0);
        assert_eq!(all.len(), 3);
    }
}
