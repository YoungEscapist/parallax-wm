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
pub use math::{Куб, оборот, стол_грани, шагов_до};

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
// фокусное расстояние, угол ЭТОЙ грани (радианы).
uniform vec4 cube_geom;
// Прямоугольник этой поверхности В ПЛОСКОСТИ ГРАНИ: u0, v0, u1, v1.
// Начало координат — центр грани, v растёт ВНИЗ (как gl_FragCoord у smithay,
// см. разбор в rounded.rs — проекция уже кладёт логический ноль в нулевую
// строку фрагментов).
uniform vec4 cube_rect;
// Точка экрана, в которую проецируется ОСЬ куба (физические пиксели).
uniform vec2 cube_axis;
// Затемнение дальнего края, 0…1: без него грани сливаются и куб читается
// плоской мозаикой.
uniform float cube_shade;

void main() {
    float r = cube_geom.x;
    float D = cube_geom.y;
    float F = cube_geom.z;
    float a = cube_geom.w;

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

    vec2 p = gl_FragCoord.xy - cube_axis;
    float ca = cos(a);
    float sa = sin(a);

    // Обратная перспектива: из экранного x достаём координату вдоль грани.
    float denom = p.x * sa - F * ca;
    if (abs(denom) < 1e-4) {
        discard;
    }
    float u = (F * r * sa + p.x * (r * ca - D)) / denom;
    float d = D - (r * ca - u * sa);
    if (d <= 1.0) {
        // Точка за камерой или в её плоскости: показывать нечего.
        gl_FragColor = vec4(0.0);
        return;
    }
    float v = p.y * d / F;

    // За краем этой поверхности — пусто. Гасим ЦЕЛИКОМ, а не только альфу:
    // кадр смешивается с предумноженной альфой, и живой цвет при нулевой альфе
    // остался бы виден (та же грабля разобрана в rounded.rs).
    if (u < cube_rect.x || u > cube_rect.z || v < cube_rect.y || v > cube_rect.w) {
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

    // Глубина: 0 у ближайшей точки куба (D − r), 1 у самой дальней (D + r).
    float depth = clamp((d - (D - r)) / max(2.0 * r, 1e-4), 0.0, 1.0);
    color *= 1.0 - cube_shade * depth;

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
            UniformName::new("cube_rect", UniformType::_4f),
            UniformName::new("cube_axis", UniformType::_2f),
            UniformName::new("cube_shade", UniformType::_1f),
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

