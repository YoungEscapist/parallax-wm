//! Режим «cycle»: обои повторяются по холсту бесконечно, как `tile` у driftwm.
//!
//! **Как было и почему переделано (19.10.2026).** Первая версия (08.10) клала
//! на экран сетку копий: каждая копия — картинка, РАСТЯНУТАЯ на весь экран, и
//! соседние копии зеркальные, чтобы спрятать шов. На деле это давало совсем не
//! то, что у driftwm: бесшовную текстуру растягивало и подрезало под пропорцию
//! экрана (ролик ещё и ужимался plx-wall с обрезкой), стык из-за этого
//! появлялся, а зеркала превращали узор в калейдоскоп. Ниже зума 0.25 плитки
//! ещё и переставали мельчать.
//!
//! **Как теперь — ровно driftwm (`tile_bg.glsl`).** Текстура лежит на холсте
//! в СВОЁМ размере: пиксель буфера = единица холста, и масштабируется только
//! зумом. Повтор простой, без зеркал — бесшовная картинка стыкуется сама с
//! собой. Рисуется всё ОДНИМ элементом во весь экран: шейдер берёт для каждой
//! точки экрана `fract(холст / плитка)`, так что число плиток в кадре ничего не
//! стоит — хоть одна, хоть тысяча на птичьем глазе.
//!
//! Начало плитки — в центре экрана дома монитора: на неподвижной камере обои
//! стоят так же, как стояла одиночная копия.

use smithay::backend::renderer::{
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Transform};

/// Фрагментный шейдер. Обвязка (`//_DEFINES_`, `EXTERNAL`, `NO_ALPHA`) — из
/// штатного `texture.frag` smithay, как и в blur.rs: без неё не соберётся
/// вариант под внешний буфер. В GLSL только латиница (см. экран.rs).
///
/// Элемент лежит на экране целиком и берёт текстуру целиком, поэтому
/// `v_coords` здесь — доля пути по экрану, 0…1. Из неё получается пиксель
/// экрана, из него — место внутри плитки.
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

// Size of the element on screen, px.
uniform vec2 tile_screen;
// Size of one tile on screen, px (texture size * zoom * output scale).
uniform vec2 tile_size;
// Screen position of a tile corner, already reduced modulo tile_size.
uniform vec2 tile_origin;

void main() {
    vec2 p = v_coords * tile_screen - tile_origin;
    vec4 color = texture2D(tex, fract(p / tile_size));

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    gl_FragColor = color;
}
"#;

/// Скомпилированная программа. Одна на рендерер, как у куба.
#[derive(Debug, Clone)]
pub struct Шейдер(GlesTexProgram);

impl Шейдер {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [
            UniformName::new("tile_screen", UniformType::_2f),
            UniformName::new("tile_size", UniformType::_2f),
            UniformName::new("tile_origin", UniformType::_2f),
        ];
        match renderer.compile_custom_texture_shader(ФРАГМЕНТ, &uniforms) {
            Ok(program) => Some(Self(program)),
            Err(e) => {
                // Не фатально: без шейдера режим cycle просто не включится, и
                // обои лягут прежней одиночной копией.
                tracing::warn!("plx/wallpaper: tile shader failed to compile: {:?}", e);
                None
            }
        }
    }

    pub fn программа(&self) -> &GlesTexProgram {
        &self.0
    }
}

/// Где на экране лежит сетка плиток, в ФИЗИЧЕСКИХ пикселях.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Раскладка {
    /// Размер плитки на экране.
    pub плитка: (f64, f64),
    /// Угол одной из плиток, приведённый в `[0, плитка)` — так в шейдер не
    /// уходят миллионы (дом второго монитора стоит на 1 000 000), и float
    /// не теряет точность.
    pub начало: (f64, f64),
    /// Размер экрана.
    pub экран: (f64, f64),
}

impl Раскладка {
    /// `камера` — от дома своего монитора, в логических единицах холста;
    /// `сдвиг_стола` — виртуальный ход по столам в пикселях экрана (Super+N
    /// камеру не двигает, см. `udev::ОБОИ_ШАГ_СТОЛА`); `картинка` — размер
    /// буфера обоев.
    pub fn new(
        камера: (f64, f64),
        зум: f64,
        масштаб: f64,
        сдвиг_стола: f64,
        картинка: (i32, i32),
        экран: (i32, i32),
    ) -> Option<Self> {
        let годно = |v: f64| v.is_finite();
        if картинка.0 <= 0
            || картинка.1 <= 0
            || экран.0 <= 0
            || экран.1 <= 0
            || !годно(камера.0)
            || !годно(камера.1)
            || !годно(зум)
            || зум <= 0.0
            || !годно(масштаб)
            || масштаб <= 0.0
            || !годно(сдвиг_стола)
        {
            return None;
        }
        let (эw, эh) = (экран.0 as f64, экран.1 as f64);
        let к = зум * масштаб;
        let (w, h) = (картинка.0 as f64 * к, картинка.1 as f64 * к);
        // Меньше пикселя плитка не бывает осмысленной: шейдер выдал бы шум.
        if w < 1.0 || h < 1.0 {
            return None;
        }
        // Точка холста, что у дома стоит в центре экрана, — центр плитки.
        let центр_x = эw / 2.0 / масштаб;
        let центр_y = эh / 2.0 / масштаб;
        let x = (центр_x - камера.0) * к - w / 2.0 - сдвиг_стола;
        let y = (центр_y - камера.1) * к - h / 2.0;
        Some(Self { плитка: (w, h), начало: (x.rem_euclid(w), y.rem_euclid(h)), экран: (эw, эh) })
    }

    pub fn uniforms(&self) -> Vec<Uniform<'static>> {
        vec![
            Uniform::new("tile_screen", [self.экран.0 as f32, self.экран.1 as f32]),
            Uniform::new("tile_size", [self.плитка.0 as f32, self.плитка.1 as f32]),
            Uniform::new("tile_origin", [self.начало.0 as f32, self.начало.1 as f32]),
        ]
    }
}

/// Обёртка: текстурный элемент обоев во весь экран, нарисованный шейдером
/// плитки.
///
/// Свой счётчик коммитов нужен потому, что картинка меняется не только с
/// новым кадром обоев, но и с КАЖДЫМ сдвигом камеры, а геометрия элемента при
/// этом та же самая — damage tracker иначе не узнал бы, что пора перерисовать.
/// Счётчик ведёт `Parallax::обои_цикл_счёт` (см. `udev::build_wallpaper_cycle`).
#[derive(Debug)]
pub struct Плитка<E> {
    inner: E,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
    коммит: CommitCounter,
}

impl<E> Плитка<E> {
    pub fn new(inner: E, шейдер: &Шейдер, раскладка: &Раскладка, коммит: CommitCounter) -> Self {
        Self { inner, program: шейдер.0.clone(), uniforms: раскладка.uniforms(), коммит }
    }
}

impl<E: Element> Element for Плитка<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.коммит
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

    /// Сменилось хоть что-то — меняется весь экран: и кадр ролика, и сдвиг
    /// камеры двигают все плитки разом.
    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        if commit == Some(self.коммит) {
            return DamageSet::default();
        }
        let g = self.geometry(scale);
        DamageSet::from_slice(&[Rectangle::from_size(g.size)])
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Плитка<E> {
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
        // Снимаем в любом случае: override живёт до конца кадра (см. куб.rs).
        let итог = self.inner.draw(frame, src, dst, damage, opaque_regions, cache);
        frame.clear_tex_program_override();
        итог
    }

    /// `None` — мимо шейдера на плоскость DRM обоям нельзя: там была бы одна
    /// растянутая картинка вместо плиток.
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn у_дома_плитка_в_центре_экрана() {
        let р = Раскладка::new((0.0, 0.0), 1.0, 1.0, 0.0, (1000, 600), (2560, 1080)).unwrap();
        assert_eq!(р.плитка, (1000.0, 600.0));
        // Центр плитки в (1280, 540) → угол в (780, 240).
        assert_eq!(р.начало, (780.0, 240.0));
    }

    #[test]
    fn камера_везёт_плитки_один_в_один_и_зум_их_ужимает() {
        let а = Раскладка::new((0.0, 0.0), 1.0, 1.0, 0.0, (1000, 600), (2560, 1080)).unwrap();
        let б = Раскладка::new((100.0, 0.0), 1.0, 1.0, 0.0, (1000, 600), (2560, 1080)).unwrap();
        assert_eq!(б.начало.0, а.начало.0 - 100.0);
        let в = Раскладка::new((0.0, 0.0), 0.05, 1.0, 0.0, (1000, 600), (2560, 1080)).unwrap();
        assert_eq!(в.плитка, (50.0, 30.0));
        assert!(в.начало.0 >= 0.0 && в.начало.0 < 50.0);
    }

    #[test]
    fn дальний_дом_не_теряет_точность() {
        let р = Раскладка::new((1_000_000.25, -3.0), 1.0, 1.0, 0.0, (1000, 600), (2560, 1080)).unwrap();
        assert!(р.начало.0 >= 0.0 && р.начало.0 < 1000.0);
        assert!((р.начало.0 - (780.0f64 - 1_000_000.25).rem_euclid(1000.0)).abs() < 1e-6);
    }
}
