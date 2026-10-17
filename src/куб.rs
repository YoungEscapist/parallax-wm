//! Куб рабочих столов — тот самый, из Compiz.
//!
//! Столы стоят на гранях правильной призмы, ось вертикальная, зритель снаружи.
//! Куб крутится: в обзоре — колесом и драгом, при переходе на соседний стол —
//! сам, доворачиваясь на одну грань.
//!
//! **Куб бесконечен.** Граней у него всегда столько, сколько задано
//! (`cube_faces`, по умолчанию четыре — как в Compiz), а столов в кольце
//! сколько угодно: слоты получают столы по кольцу и переназначаются на ЗАДНЕЙ
//! грани, которой не видно (см. `куб_math::стол_грани`). Крутить можно вечно,
//! и на десятом столе куб остаётся кубом, а не двадцатигранной стеной.
//!
//! **Как это считается.** У элемента кадра в smithay есть только
//! прямоугольник `dst`, вершин, которые можно развернуть в пространстве, нет,
//! зато есть фрагментный шейдер — а ему нужно ОБРАТНОЕ отображение, «какая
//! точка исходника приходится на эту точку экрана». Плоскость грани стоит на
//! радиусе `r` от оси, и обратное отображение решается одной строкой.
//!
//! Точка грани, повёрнутой на угол `α`, с локальной координатой `u` вдоль
//! грани и `v` по вертикали:
//!
//! ```text
//! X = r·sin α + u·cos α
//! Z = r·cos α − u·sin α          (Z — на зрителя, камера стоит в Z = D)
//! глубина d = D − Z
//! x = F·X/d,  y = F·v/d
//! ```
//!
//! Обратное решается ОДНОЙ строкой, без итераций, — уравнение линейно по `u`:
//!
//! ```text
//! u = (F·r·sin α + x·(r·cos α − D)) / (x·sin α − F·cos α)
//! d = D − r·cos α + u·sin α
//! v = y·d / F
//! ```
//!
//! **Единицы.** Внутри — физические пиксели экрана: сторона грани равна
//! ширине экрана, `F` задаётся в её долях (фокус), а расстояние до камеры
//! подбирается из условия «передняя грань занимает долю `доля` ширины
//! экрана» — так куб одинаково смотрится на 1080p и на 4K, и настраивать
//! приходится не пиксели, а вид.
//!
//! **Чего здесь нет.** Отсечения задних граней в шейдере: грань, повёрнутая
//! от зрителя, отбрасывается на CPU (см. [`Куб::видна`]) — незачем гонять
//! фрагменты ради `discard`. И крышек куба: сверху и снизу он открыт, как и в
//! Compiz при взгляде сбоку.

use smithay::backend::renderer::{
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Transform};

// Геометрия — общий файл с минимальной сборкой (см. его шапку).
#[path = "куб_math.rs"]
mod math;
pub use math::{Куб, Плоскость, оборот, стол_грани, шагов_до};

/// Исходник фрагментного шейдера.
///
/// Обвязка (`//_DEFINES_`, `EXTERNAL`, `NO_ALPHA`, `DEBUG_FLAGS`) — та же, что
/// у `texture.frag` smithay и `rounded.rs`: без неё не соберётся
/// вариант программы под внешний буфер, и первый же клиент с dmabuf уронит
/// отрисовку.
///
/// ВНИМАНИЕ: имена внутри GLSL — только латиницей. GLSL ES 1.00 не знает
/// не-ASCII идентификаторов, и кириллическое имя переменной здесь значит не
/// «непривычно», а «шейдер не скомпилируется вовсе» — куб молча останется
/// плоским, а причина будет видна только в логе.
const ФРАГМЕНТ: &str = r#"#version 100

//_DEFINES_

#if defined(EXTERNAL)
#extension GL_OES_EGL_image_external : require
#endif

precision highp float;
#if defined(EXTERNAL)
uniform samplerExternalOES tex;
#else
uniform sampler2D tex;
#endif

uniform float alpha;
varying vec2 v_coords;

#if defined(DEBUG_FLAGS)
uniform float tint;
#endif

// Геометрия куба: радиус вписанной окружности, расстояние до камеры,
// фокусное расстояние (четвёртое число не используется).
uniform vec4 cube_geom;
// Плоскость ЭТОЙ поверхности в пространстве куба: центр и два
// ортонормированных направления вдоль u и вдоль v (x вправо, y вниз, z на
// зрителя). Поворот куба и его наклон уже в них — шейдеру всё равно, грань
// это, крышка или развёрнутое окно.
uniform vec3 cube_o;
uniform vec3 cube_eu;
uniform vec3 cube_ev;
// Прямоугольник этой поверхности В ПЛОСКОСТИ ГРАНИ: u0, v0, u1, v1.
// Начало координат — центр грани, v растёт ВНИЗ (как gl_FragCoord у smithay,
// см. разбор в rounded.rs — проекция уже кладёт логический ноль в нулевую
// строку фрагментов).
uniform vec4 cube_rect;
// Видимая часть поверхности в тех же координатах: всё, что за ней, гасится.
// Обычно совпадает с cube_rect. Отличается у обрезанных поверхностей — окно
// с буфером больше слота, колонка ленты за краем грани: координаты текстуры
// обязаны идти от ЦЕЛОЙ поверхности, иначе весь буфер сжимался бы в вырез.
uniform vec4 cube_clip;
// Точка экрана, в которую проецируется ОСЬ куба (физические пиксели).
uniform vec2 cube_axis;
// Затемнение дальнего края, 0…1: без него грани сливаются и куб читается
// плоской мозаикой.
uniform float cube_shade;
// Крышка: число граней (0 — это не крышка), апофема угольника и яркость.
uniform vec3 cube_poly;
// Скругление углов наклонённого окна: его видимая рамка в координатах
// плоскости (u0, v0, u1, v1) и радиус дуги. Радиус 0 — углы прямые.
uniform vec4 cube_round;
uniform float cube_radius;

void main() {
    float r = cube_geom.x;
    float D = cube_geom.y;
    float F = cube_geom.z;

    if (F <= 0.0) {
        // Куба нет — обычная отрисовка, ровно как без обёртки.
        vec4 c = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
        c = vec4(c.rgb, 1.0) * alpha;
#else
        c = c * alpha;
#endif
        gl_FragColor = c;
        return;
    }

    // Луч из камеры (0, 0, D) через этот пиксель и его пересечение с
    // плоскостью. Вывод — у `Куб::обратно_на` в куб_math.rs, строка в строку.
    vec3 w = vec3(gl_FragCoord.xy - cube_axis, -F);
    vec3 n = cross(cube_eu, cube_ev);
    float denom = dot(n, w);
    if (abs(denom) < 1e-4) {
        discard;
    }
    vec3 toO = cube_o - vec3(0.0, 0.0, D);
    float t = dot(n, toO) / denom;
    float d = t * F;
    if (d <= 1.0) {
        // Точка за камерой или в её плоскости: показывать нечего.
        gl_FragColor = vec4(0.0);
        return;
    }
    vec3 rel = w * t - toO;
    float u = dot(rel, cube_eu);
    float v = dot(rel, cube_ev);

    // Крышка — правильный угольник: точка внутри, если её проекция на нормаль
    // ближайшей грани не дальше апофемы. Нормаль грани i в координатах
    // крышки — (sin iθ, cos iθ), ближайшая — по углу точки.
    if (cube_poly.x > 0.5) {
        float sector = 6.2831853 / cube_poly.x;
        float k = floor(atan(u, v) / sector + 0.5);
        if (u * sin(k * sector) + v * cos(k * sector) > cube_poly.y) {
            gl_FragColor = vec4(0.0);
            return;
        }
    }

    // За краем этой поверхности — пусто. Гасим ЦЕЛИКОМ, а не только альфу:
    // кадр смешивается с предумноженной альфой, и живой цвет при нулевой альфе
    // остался бы виден (та же грабля разобрана в rounded.rs).
    if (u < cube_clip.x || u > cube_clip.z || v < cube_clip.y || v > cube_clip.w) {
        gl_FragColor = vec4(0.0);
        return;
    }

    vec2 uv = vec2(
        (u - cube_rect.x) / max(cube_rect.z - cube_rect.x, 1e-4),
        (v - cube_rect.y) / max(cube_rect.w - cube_rect.y, 1e-4)
    );
    vec4 color = texture2D(tex, uv);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    // Скруглённые углы — расстояние до скруглённого прямоугольника. Край
    // сглаживается на ширину ОДНОГО ПИКСЕЛЯ ЭКРАНА, а не плоскости: у дальнего
    // края наклонённого окна в пиксель экрана помещается больше плоскости
    // (d/F), и сглаживание в единицах плоскости там шло бы лесенкой.
    // (Имя `hs`, а не `half`: `half` в GLSL ES 1.00 зарезервировано.)
    if (cube_radius > 0.0) {
        vec2 c = 0.5 * (cube_round.xy + cube_round.zw);
        vec2 hs = 0.5 * (cube_round.zw - cube_round.xy);
        float rr = min(cube_radius, min(hs.x, hs.y));
        vec2 q = abs(vec2(u, v) - c) - (hs - vec2(rr));
        float dist = length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - rr;
        color *= clamp(0.5 - dist * F / d, 0.0, 1.0);
    }

    // Глубина: 0 у ближайшей точки куба (D − r), 1 у самой дальней (D + r).
    float depth = clamp((d - (D - r)) / max(2.0 * r, 1e-4), 0.0, 1.0);
    color *= (1.0 - cube_shade * depth) * cube_poly.z;

    gl_FragColor = color;
}
"#;

/// Скомпилированная программа. Одна на рендерер, как у скругления.
#[derive(Debug, Clone)]
pub struct Шейдер(GlesTexProgram);

impl Шейдер {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [
            UniformName::new("cube_geom", UniformType::_4f),
            UniformName::new("cube_o", UniformType::_3f),
            UniformName::new("cube_eu", UniformType::_3f),
            UniformName::new("cube_ev", UniformType::_3f),
            UniformName::new("cube_poly", UniformType::_3f),
            UniformName::new("cube_rect", UniformType::_4f),
            UniformName::new("cube_clip", UniformType::_4f),
            UniformName::new("cube_axis", UniformType::_2f),
            UniformName::new("cube_shade", UniformType::_1f),
            UniformName::new("cube_round", UniformType::_4f),
            UniformName::new("cube_radius", UniformType::_1f),
        ];
        match renderer.compile_custom_texture_shader(ФРАГМЕНТ, &uniforms) {
            Ok(program) => Some(Self(program)),
            Err(e) => {
                // Не фатально: без шейдера обзор просто останется плоским.
                tracing::warn!("plx/cube: cube shader failed to compile: {:?}", e);
                None
            }
        }
    }
}

/// Обёртка над элементом: та же картинка, положенная на грань куба.
#[derive(Debug)]
pub struct Грань<E> {
    inner: E,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl<E> Грань<E> {
    /// `rect` — место этой поверхности В ПЛОСКОСТИ ГРАНИ (u0, v0, u1, v1),
    /// `угол` — угол грани с учётом поворота куба.
    pub fn new(
        inner: E,
        шейдер: &Шейдер,
        куб: &Куб,
        угол: f32,
        rect: (f32, f32, f32, f32),
    ) -> Self {
        Self {
            inner,
            program: шейдер.0.clone(),
            uniforms: куб.uniforms(угол, rect),
        }
    }

    /// Та же картинка на произвольной плоскости — крышке куба. `uniforms` —
    /// из `Куб::uniforms_крышки`.
    pub fn на_плоскости(inner: E, шейдер: &Шейдер, uniforms: Vec<Uniform<'static>>) -> Self {
        Self {
            inner,
            program: шейдер.0.clone(),
            uniforms,
        }
    }

    /// Показать только часть поверхности: `clip` — видимый вырез в плоскости
    /// грани, а `rect` из `new` остаётся рамкой ЦЕЛОЙ поверхности (по ней
    /// считаются координаты текстуры).
    pub fn с_отсечением(mut self, clip: (f32, f32, f32, f32)) -> Self {
        for u in &mut self.uniforms {
            if u.name == "cube_clip" {
                *u = Uniform::new("cube_clip", [clip.0, clip.1, clip.2, clip.3]);
            }
        }
        self
    }
}

impl<E: Element> Element for Грань<E> {
    /// Id — вложенного элемента: свой на каждый кадр означал бы полную
    /// перерисовку экрана всегда (та же причина, что в rounded.rs).
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        self.inner.src()
    }

    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    /// Пусто: грань стоит под углом, и за её краем внутри `dst` видно то, что
    /// лежит позади, — обещать компоновщику непрозрачность нельзя.
    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::default()
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Грань<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        frame.override_default_tex_program(self.program.clone(), self.uniforms.clone());
        // Снимаем в любом случае, включая ошибку: override живёт до конца
        // кадра, и забытый здесь развернул бы всё, что рисуется следом.
        let итог = self.inner.draw(frame, src, dst, damage, opaque_regions, cache);
        frame.clear_tex_program_override();
        итог
    }

    /// `None` — грани нельзя уезжать на плоскость DRM мимо шейдера.
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

