//! Настоящая перспектива для миниатюр окон в обзоре: карточка развёрнута
//! вокруг вертикальной оси и уходит вдаль дальним краем.
//!
//! **Почему это делается шейдером, а не геометрией.** Элемент кадра в smithay
//! рисуется прямоугольником: у него есть `dst` на экране, и вершин, которые
//! можно было бы развернуть в пространстве, у нас нет. Зато есть фрагментный
//! шейдер — а перспективное преобразование обратимо в замкнутой форме, и
//! обратное отображение это ровно то, что фрагментному шейдеру и нужно: «дай
//! точку исходника для этой точки экрана».
//!
//! **Как считается.** Карточка — прямоугольник W×H в плоскости Z=0, центр в
//! начале координат. Точка на ней: `(t, s, 0)`, где `t ∈ [−W/2, W/2]`. Поворот
//! вокруг оси Y на угол θ:
//!
//! ```text
//! X' = t·cos θ,   Z' = −t·sin θ
//! ```
//!
//! Перспектива с фокусным расстоянием F — деление на глубину:
//! `f = F / (F + Z') = F / (F − t·sin θ)`, экранные координаты
//! `x = X'·f`, `y = s·f`.
//!
//! Шейдеру нужно обратное. Из `x = t·cos θ · F / (F − t·sin θ)` выражается
//! `t` без всяких итераций:
//!
//! ```text
//! t = x·F / (F·cos θ + x·sin θ)
//! ```
//!
//! дальше `f` считается по `t`, а `s = y / f`. Точки, у которых `t` или `s`
//! вышли за пределы карточки, — это фон за её краем: их гасим.
//!
//! **Карточка вписывается в свой прямоугольник, а не вылезает за него.**
//! Развернувшись, она стала бы шире по горизонтали у ближнего края — но
//! рисовать за пределами `dst` нечем (фрагментов там нет). Поэтому вся фигура
//! ужимается ровно настолько, чтобы уместиться: ближний край занимает всю
//! высоту прямоугольника, дальний — сколько ему осталось. Снаружи это и
//! читается как «карточка повёрнута», а не «карточка обрезана».

use smithay::backend::renderer::{
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Transform};

/// Исходник фрагментного шейдера.
///
/// Обвязка (`//_DEFINES_`, `EXTERNAL`, `NO_ALPHA`, `DEBUG_FLAGS`) — та же, что
/// у `texture.frag` smithay и у `rounded.rs`: без неё рендер не соберёт
/// вариант программы под внешний буфер и упадёт на первом же клиенте с
/// dmabuf.
///
/// Координаты берём из `gl_FragCoord` и НЕ переворачиваем Y — по той же
/// причине, что разобрана в шапке `rounded.rs`: проекция smithay уже кладёт
/// логический ноль в нулевую строку фрагментов.
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

// Прямоугольник карточки на экране: x, y, ширина, высота (физические пиксели).
uniform vec4 card_rect;
// Угол поворота вокруг вертикальной оси, радианы. Знак задаёт, какой край
// ближе: положительный — правый.
uniform float tilt_angle;
// Фокусное расстояние в долях ШИРИНЫ карточки. Меньше — резче перспектива.
// Ноль и меньше выключают наклон целиком.
uniform float tilt_focal;
// Затемнение дальнего края, 0…1: без него разворот читается плоско, потому
// что глазу не за что зацепиться, кроме формы.
uniform float tilt_shade;

// ВНИМАНИЕ: имена внутри шейдера — только латиницей. GLSL ES 1.00 не знает
// не-ASCII идентификаторов, и кириллическое имя переменной здесь означает не
// «непривычно», а «шейдер не скомпилируется вовсе» — обзор молча останется
// плоским, а причина будет видна только в логе.
void main() {
    if (tilt_focal <= 0.0 || card_rect.z <= 0.0 || card_rect.w <= 0.0) {
        // Наклон выключен — обычная отрисовка, ровно как без обёртки.
        vec4 c = texture2D(tex, v_coords);
#if defined(NO_ALPHA)
        c = vec4(c.rgb, 1.0) * alpha;
#else
        c = c * alpha;
#endif
        gl_FragColor = c;
        return;
    }

    float W = card_rect.z;
    float H = card_rect.w;
    vec2 center = card_rect.xy + vec2(W, H) * 0.5;
    // Экранная точка относительно центра карточки.
    vec2 p = gl_FragCoord.xy - center;

    float F = tilt_focal * W;
    float ct = cos(tilt_angle);
    float st = sin(tilt_angle);
    float ast = abs(st);

    // Размер САМОЙ карточки (не её проекции) подбираем так, чтобы проекция
    // ровно вписалась в отведённый прямоугольник. Ближний край даёт наибольший
    // масштаб, по нему и решаем уравнение:
    //   a·cos θ·F / (F − a·|sin θ|) = W/2   →   a = (W/2)·F / (F·cos θ + (W/2)·|sin θ|)
    // Так развёрнутая карточка не вылезает за свой dst (рисовать там нечем) и
    // не приходится ничего обрезать.
    float a = (W * 0.5) * F / max(F * ct + (W * 0.5) * ast, 1e-4);
    // По вертикали то же самое: половина высоты, ужатая масштабом ближнего края.
    float f_near = F / max(F - a * ast, 1e-4);
    float b = (H * 0.5) / max(f_near, 1e-4);

    // Обратная перспектива: из экранного x достаём координату на карточке.
    float denom = F * ct + p.x * st;
    if (abs(denom) < 1e-4) {
        discard;
    }
    float t = p.x * F / denom;
    float f = F / max(F - t * st, 1e-4);
    float s = p.y / f;

    // За краем карточки — пусто. Гасим ЦЕЛИКОМ, а не только альфу: кадр
    // смешивается с предумноженной альфой, и живой цвет при нулевой альфе
    // остался бы виден (та же грабля разобрана в rounded.rs).
    if (abs(t) > a || abs(s) > b) {
        gl_FragColor = vec4(0.0);
        return;
    }

    // Координаты внутри текстуры: v_coords меняется линейно по прямоугольнику,
    // поэтому пересчитываем их из своей точки, а не берём как есть.
    vec2 uv = vec2(t / (2.0 * a) + 0.5, s / (2.0 * b) + 0.5);
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

    // Дальний край темнее: без этого разворот читается плоско — глазу не за
    // что зацепиться, кроме формы. Доля глубины: 1 у ближнего края, 0 у дальнего.
    float f_far = F / max(F + a * ast, 1e-4);
    float depth = clamp((f - f_far) / max(f_near - f_far, 1e-4), 0.0, 1.0);
    color *= 1.0 - tilt_shade * (1.0 - depth);

    gl_FragColor = color;
}
"#;

/// Скомпилированная программа. Одна на рендерер, как и у скругления.
#[derive(Debug, Clone)]
pub struct Шейдер(GlesTexProgram);

impl Шейдер {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [
            UniformName::new("card_rect", UniformType::_4f),
            UniformName::new("tilt_angle", UniformType::_1f),
            UniformName::new("tilt_focal", UniformType::_1f),
            UniformName::new("tilt_shade", UniformType::_1f),
        ];
        match renderer.compile_custom_texture_shader(ФРАГМЕНТ, &uniforms) {
            Ok(program) => Some(Self(program)),
            Err(e) => {
                // Не фатально: без шейдера обзор просто останется плоским.
                tracing::warn!("plx/tilt: perspective shader failed to compile: {:?}", e);
                None
            }
        }
    }
}

/// Насколько и куда развёрнута карточка.
#[derive(Clone, Copy, Debug)]
pub struct Разворот {
    /// Угол в радианах. Ноль — карточка смотрит прямо.
    pub угол: f32,
    /// Фокусное расстояние в долях ширины карточки. 0 — наклона нет вовсе.
    pub фокус: f32,
    /// Насколько затемнён дальний край, 0…1.
    pub тень: f32,
}

impl Разворот {
    pub fn нет() -> Self {
        Self { угол: 0.0, фокус: 0.0, тень: 0.0 }
    }

    /// Разворот карточки, стоящей на `доля` от центра обзора (−1 — левый край,
    /// +1 — правый), при силе эффекта `сила` (0…1).
    ///
    /// Карточки поворачиваются К ЦЕНТРУ: левые правым боком к зрителю, правые
    /// — левым. Получается вогнутая стена, как будто окна расставлены по дуге
    /// вокруг смотрящего, — отсюда и ощущение глубины, ради которого всё.
    pub fn по_месту(доля: f32, сила: f32) -> Self {
        if сила <= 0.0 {
            return Self::нет();
        }
        let сила = сила.clamp(0.0, 1.0);
        let доля = доля.clamp(-1.0, 1.0);
        Self {
            // 35° на краю при полной силе: дальше карточка вырождается в
            // полоску, и «обзор» перестаёт что-либо показывать.
            угол: -доля * сила * 35.0f32.to_radians(),
            // Фокус мягкий: при 1.6 ширины перспектива заметна, но прямые
            // линии окна ещё читаются прямыми.
            фокус: 1.6,
            тень: 0.35 * сила,
        }
    }
}

/// Обёртка над элементом: та же картинка, но развёрнутая в перспективе.
#[derive(Debug)]
pub struct Наклон<E> {
    inner: E,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl<E> Наклон<E> {
    /// `rect` — прямоугольник карточки на экране в физических пикселях.
    pub fn new(inner: E, шейдер: &Шейдер, rect: [f32; 4], разворот: Разворот) -> Self {
        Self {
            inner,
            program: шейдер.0.clone(),
            uniforms: vec![
                Uniform::new("card_rect", rect),
                Uniform::new("tilt_angle", разворот.угол),
                Uniform::new("tilt_focal", разворот.фокус),
                Uniform::new("tilt_shade", разворот.тень),
            ],
        }
    }
}

impl<E: Element> Element for Наклон<E> {
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

    /// Пусто: под развёрнутой карточкой видно то, что за ней, — обещать
    /// компоновщику непрозрачность нельзя (см. rounded.rs).
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

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Наклон<E> {
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

    /// `None` — карточке нельзя уезжать на плоскость DRM мимо шейдера.
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

#[cfg(test)]
mod тесты {
    use super::*;

    #[test]
    fn нулевая_сила_не_разворачивает() {
        let р = Разворот::по_месту(1.0, 0.0);
        assert_eq!(р.фокус, 0.0, "фокус 0 выключает наклон в шейдере");
        assert_eq!(р.угол, 0.0);
    }

    #[test]
    fn карточка_в_центре_смотрит_прямо() {
        let р = Разворот::по_месту(0.0, 1.0);
        assert!(р.угол.abs() < 1e-6, "угол в центре: {}", р.угол);
        assert!(р.фокус > 0.0, "но сам наклон включён — соседи развёрнуты");
    }

    /// Левые и правые карточки разворачиваются В РАЗНЫЕ стороны, иначе вместо
    /// вогнутой стены получилась бы косо уехавшая плоскость.
    #[test]
    fn края_смотрят_навстречу() {
        let слева = Разворот::по_месту(-1.0, 1.0);
        let справа = Разворот::по_месту(1.0, 1.0);
        assert!(слева.угол > 0.0, "левая карточка: {}", слева.угол);
        assert!(справа.угол < 0.0, "правая карточка: {}", справа.угол);
        assert!((слева.угол + справа.угол).abs() < 1e-6, "углы обязаны быть зеркальны");
    }

    /// Угол растёт с удалением от центра, но не превышает предела: дальше
    /// карточка вырождается в полоску и обзор перестаёт что-либо показывать.
    #[test]
    fn угол_растёт_но_не_беспредельно() {
        let середина = Разворот::по_месту(0.5, 1.0).угол.abs();
        let край = Разворот::по_месту(1.0, 1.0).угол.abs();
        assert!(край > середина, "{край} должен быть больше {середина}");
        assert!(край <= 35.0f32.to_radians() + 1e-6, "предел 35°, получили {}", край.to_degrees());
        // За краем обзора ничего не меняется — доля зажата.
        assert!((Разворот::по_месту(5.0, 1.0).угол.abs() - край).abs() < 1e-6);
    }
}
