//! Плитки декораций: тень окна текстурами, а не сотнями однопиксельных
//! полосок. Скруглением углов модуль больше не занимается — форму окна режет
//! шейдер (`rounded.rs`), см. `Proto::new`.
//!
//! Как было: тень строилась пятью слоями, каждый слой — построчно по уравнению
//! окружности, то есть `2 × радиус` полосок высотой в 1 пиксель на слой, плюс
//! средний прямоугольник. Маска угла — так же построчно. На четырёх окнах это
//! ~1200 элементов из ~1900 в кадре, и при полной перерисовке (камера едет,
//! окна анимируются) кадр 4K стоил 20-45 мс при бюджете 16.6 — то самое
//! «подлагивает при анимациях».
//!
//! Как стало: форма тени зависит только от расстояния до прямоугольника окна,
//! поэтому она раскладывается на 9 частей (как CSS border-image): четыре
//! угловые плитки, четыре кромки (текстура толщиной в 1 пиксель, растянутая
//! вдоль стороны) и однотонная середина. Плитки считаются один раз на радиус и
//! переиспользуются всеми окнами; 289 элементов на окно превращаются в 11.
//!
//! Пиксели premultiplied RGBA — так их ждёт GL-блендинг в smithay.
//!
//! ВАЖНО про Id: `MemoryRenderBufferRenderElement::from_buffer` берёт Id из
//! БУФЕРА, а damage tracker индексирует состояние по Id. Один буфер, отданный в
//! несколько элементов кадра, ломает подсчёт повреждений (ровно этим болели
//! маски углов раньше — см. историю с `cached_corner_buf`). Поэтому здесь пул:
//! у каждого нарисованного экземпляра свой буфер с одинаковыми пикселями.

use smithay::{
    backend::{allocator::Fourcc, renderer::element::memory::MemoryRenderBuffer},
    utils::Transform,
};

/// Слои тени: (растушёвка в логических px, альфа). Совпадают со старыми
/// значениями — вид тени сохраняется один в один.
const LAYERS: [(i32, f32); 5] = [(2, 0.09), (5, 0.06), (9, 0.035), (14, 0.02), (20, 0.01)];
/// Самый широкий слой — толщина кромки девятипатчевой раскладки.
pub const SPREAD: i32 = 20;
/// Сдвиг тени вниз (свет сверху).
pub const DROP: i32 = 4;

/// Индексы углов и кромок: 0 = TL/верх, 1 = TR/низ, 2 = BL/лево, 3 = BR/право.
pub const TL: usize = 0;
pub const TR: usize = 1;
pub const BL: usize = 2;
pub const BR: usize = 3;
pub const TOP: usize = 0;
pub const BOTTOM: usize = 1;
pub const LEFT: usize = 2;
pub const RIGHT: usize = 3;

/// Альфа тени на расстоянии `d` от границы прямоугольника окна (d <= 0 —
/// внутри). Слои складываются как несколько полупрозрачных чёрных
/// прямоугольников друг на друге: 1 - Π(1 - aᵢ) по всем накрывающим слоям —
/// ровно то, что раньше делал блендинг пяти нарисованных слоёв.
fn shadow_alpha(d: f64) -> f32 {
    let mut inv = 1.0f32;
    for (spread, a) in LAYERS {
        if d <= spread as f64 {
            inv *= 1.0 - a;
        }
    }
    1.0 - inv
}

/// Альфа середины тени — там, где накрывают все слои.
pub fn center_alpha() -> f32 {
    shadow_alpha(-1.0)
}

/// Сглаживание: 3×3 подвыборки на пиксель. Старая построчная версия давала
/// ступеньки на скруглениях, здесь они уходят бесплатно (плитка считается раз).
const SS: i32 = 3;

/// Чёрный premultiplied: rgb = 0 при любой альфе, писать нужно только альфу.
fn push_black(buf: &mut Vec<u8>, alpha: f32) {
    let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u8;
    buf.extend_from_slice(&[0, 0, 0, a]);
}

/// Цвет с premultiplied-альфой.
fn push_color(buf: &mut Vec<u8>, rgb: [f32; 3], alpha: f32) {
    let a = alpha.clamp(0.0, 1.0);
    buf.extend_from_slice(&[
        (rgb[0] * a * 255.0).round() as u8,
        (rgb[1] * a * 255.0).round() as u8,
        (rgb[2] * a * 255.0).round() as u8,
        (a * 255.0).round() as u8,
    ]);
}

/// Зеркалирование плитки по горизонтали/вертикали — остальные три угла это те
/// же пиксели, форма симметрична.
fn flip(src: &[u8], w: i32, h: i32, horiz: bool, vert: bool) -> Vec<u8> {
    let mut out = vec![0u8; src.len()];
    for y in 0..h {
        let sy = if vert { h - 1 - y } else { y };
        for x in 0..w {
            let sx = if horiz { w - 1 - x } else { x };
            let d = ((y * w + x) * 4) as usize;
            let s = ((sy * w + sx) * 4) as usize;
            out[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    out
}

/// Эталонные пиксели плиток для одного радиуса скругления.
struct Proto {
    /// Сторона угловой плитки тени: радиус + самая широкая растушёвка.
    corner_px: i32,
    shadow_corner: [Vec<u8>; 4],
    /// Кромки: верх/низ — 1×SPREAD, лево/право — SPREAD×1 (растягиваются вдоль).
    shadow_edge: [Vec<u8>; 4],
}

impl Proto {
    fn new(radius: i32) -> Self {
        let r = radius.max(1);
        let n = r + SPREAD;

        // ── Угол тени ────────────────────────────────────────────────────────
        // Плитка кроет область от -SPREAD до +r по обеим осям относительно угла
        // окна. Центр скругления — точка (r, r) в тех же координатах, и для
        // ЛЮБОЙ точки плитки расстояние до прямоугольника окна равно
        // |p - центр| - r: плитка целиком лежит в угловой четверти.
        let mut tl = Vec::with_capacity((n * n * 4) as usize);
        for y in 0..n {
            for x in 0..n {
                let mut acc = 0.0f32;
                for sy in 0..SS {
                    for sx in 0..SS {
                        let px = (x - SPREAD) as f64 + (sx as f64 + 0.5) / SS as f64;
                        let py = (y - SPREAD) as f64 + (sy as f64 + 0.5) / SS as f64;
                        let dx = px - r as f64;
                        let dy = py - r as f64;
                        acc += shadow_alpha((dx * dx + dy * dy).sqrt() - r as f64);
                    }
                }
                push_black(&mut tl, acc / (SS * SS) as f32);
            }
        }
        let shadow_corner = [
            tl.clone(),
            flip(&tl, n, n, true, false),
            flip(&tl, n, n, false, true),
            flip(&tl, n, n, true, true),
        ];

        // ── Кромки тени ──────────────────────────────────────────────────────
        // Вдоль стороны альфа постоянна, меняется только поперёк — поэтому
        // текстура толщиной в один пиксель, растянутая на всю длину стороны.
        let mut top = Vec::with_capacity((SPREAD * 4) as usize);
        for j in 0..SPREAD {
            let mut acc = 0.0f32;
            for s in 0..SS {
                let depth = (SPREAD - j) as f64 - (s as f64 + 0.5) / SS as f64;
                acc += shadow_alpha(depth);
            }
            push_black(&mut top, acc / SS as f32);
        }
        let bottom = flip(&top, 1, SPREAD, false, true);
        let left = {
            // Та же кривая, но вдоль X: 1 пиксель высотой, SPREAD шириной.
            let mut v = Vec::with_capacity((SPREAD * 4) as usize);
            for i in 0..SPREAD {
                let mut acc = 0.0f32;
                for s in 0..SS {
                    let depth = (SPREAD - i) as f64 - (s as f64 + 0.5) / SS as f64;
                    acc += shadow_alpha(depth);
                }
                push_black(&mut v, acc / SS as f32);
            }
            v
        };
        let right = flip(&left, SPREAD, 1, true, false);
        let shadow_edge = [top, bottom, left, right];

        // Плиток-МАСКИ здесь больше нет намеренно. Угол окна когда-то не
        // вырезался, а закрашивался плиткой цвета фона — допущение «под окном
        // обычно просто холст», неверное под обоями: там свой цвет в каждом
        // пикселе, и угол выходил чёрным куском. Форму окна теперь режет
        // шейдер (см. `rounded.rs`), который гасит АЛЬФУ, а не красит; цвет
        // фона для этого не нужен вовсе.
        Proto { corner_px: n, shadow_corner, shadow_edge }
    }
}

fn buffer_from(pixels: &[u8], w: i32, h: i32) -> MemoryRenderBuffer {
    MemoryRenderBuffer::from_slice(pixels, Fourcc::Abgr8888, (w, h), 1, Transform::Normal, None)
}

/// Кэш плиток + пулы буферов (по буферу на каждый рисуемый экземпляр, см.
/// заметку про Id в шапке модуля).
pub struct DecorCache {
    radius: i32,
    proto: Option<Proto>,
    shadow_corner: [Vec<MemoryRenderBuffer>; 4],
    shadow_edge: [Vec<MemoryRenderBuffer>; 4],
    parallax_w: i32,
    parallax_proto: Vec<u8>,
    parallax_row: Vec<MemoryRenderBuffer>,
}

impl DecorCache {
    pub fn new() -> Self {
        Self {
            radius: 0,
            proto: None,
            shadow_corner: Default::default(),
            shadow_edge: Default::default(),
            parallax_w: 0,
            parallax_proto: Vec::new(),
            parallax_row: Vec::new(),
        }
    }

    /// Пересобрать плитки, если сменился радиус (Tile ↔ Float).
    /// Пулы при этом сбрасываются: в них лежат буферы со старыми пикселями.
    ///
    /// Цвета фона тут больше нет: он нужен был только плиткам-маске, а
    /// скругление ушло в шейдер (см. `Proto::new`). Заодно ушла и пересборка
    /// ВСЕХ плиток при смене фона — тени от него никогда не зависели.
    pub fn ensure(&mut self, radius: i32) {
        if self.proto.is_some() && self.radius == radius {
            return;
        }
        tracing::debug!("dawn/decor: пересборка плиток, радиус={} px", radius);
        self.radius = radius;
        self.proto = Some(Proto::new(radius));
        self.shadow_corner = Default::default();
        self.shadow_edge = Default::default();
    }

    /// Сторона угловой плитки тени (логические px).
    pub fn corner_px(&self) -> i32 {
        self.proto.as_ref().map(|p| p.corner_px).unwrap_or(0)
    }

    // Ниже во всех трёх: обращение к прототипу только когда слот РЕАЛЬНО надо
    // создать. Иначе каждый кадр копировал бы пиксели всех плиток заново —
    // ровно та работа, ради избавления от которой всё это и затевалось.
    pub fn shadow_corner(&mut self, corner: usize, slot: usize) -> &MemoryRenderBuffer {
        if self.shadow_corner[corner].len() <= slot {
            let proto = self.proto.as_ref().expect("ensure() не звали");
            let n = proto.corner_px;
            let px = &proto.shadow_corner[corner];
            while self.shadow_corner[corner].len() <= slot {
                self.shadow_corner[corner].push(buffer_from(px, n, n));
            }
        }
        &self.shadow_corner[corner][slot]
    }

    pub fn shadow_edge(&mut self, edge: usize, slot: usize) -> &MemoryRenderBuffer {
        if self.shadow_edge[edge].len() <= slot {
            let (w, h) = if edge == TOP || edge == BOTTOM { (1, SPREAD) } else { (SPREAD, 1) };
            let proto = self.proto.as_ref().expect("ensure() не звали");
            let px = &proto.shadow_edge[edge];
            while self.shadow_edge[edge].len() <= slot {
                self.shadow_edge[edge].push(buffer_from(px, w, h));
            }
        }
        &self.shadow_edge[edge][slot]
    }

    /// Полоса параллакс-фона: точки вдоль X с шагом `spacing`, высотой `dot`.
    /// Одна такая полоса на ряд сетки вместо ~24 отдельных точек.
    pub fn parallax_row(
        &mut self,
        slot: usize,
        width: i32,
        dot: i32,
        spacing: i32,
        color: [f32; 4],
    ) -> &MemoryRenderBuffer {
        if self.parallax_w != width || self.parallax_proto.is_empty() {
            self.parallax_w = width;
            self.parallax_row.clear();
            let mut px = Vec::with_capacity((width * dot * 4) as usize);
            for _ in 0..dot {
                for x in 0..width {
                    let on = x % spacing < dot;
                    push_color(
                        &mut px,
                        [color[0], color[1], color[2]],
                        if on { color[3] } else { 0.0 },
                    );
                }
            }
            self.parallax_proto = px;
        }
        while self.parallax_row.len() <= slot {
            let buf = buffer_from(&self.parallax_proto, width, dot);
            self.parallax_row.push(buf);
        }
        &self.parallax_row[slot]
    }
}
