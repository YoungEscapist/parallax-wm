//! Настоящие скруглённые углы окон — вырезанием по альфе, а не закрашиванием.
//!
//! **Что было и почему это баг.** Скругление в dawn делала плитка-маска из
//! `decor.rs`: квадратик размером с радиус, в котором всё, что вне окружности,
//! залито ЦВЕТОМ. Цвет брался один — `CLEAR_COLOR`, то есть заливка пустого
//! холста. Пока под окном был голый холст, обман не читался: маска подставляла
//! ровно тот цвет, что и так был бы виден. Но под окном почти всегда обои — и
//! тогда в каждом углу оказывался непрозрачный тёмный кусок вместо картинки.
//! Снаружи это выглядит как «вместо закругления показываются чёрные остатки
//! фона», и починить это подбором цвета нельзя в принципе: правильный цвет
//! свой в каждом пикселе угла.
//!
//! **Что теперь.** Угол не закрашивается, а вырезается: окно рисуется своим
//! текстурным шейдером, который за границей скруглённого прямоугольника гасит
//! альфу в ноль. Под вырезанным углом честно видно то, что там и лежит, —
//! обои, соседнее окно, что угодно.
//!
//! Шейдер ставится не глобально, а на время отрисовки ОДНОГО окна:
//! [`GlesFrame::override_default_tex_program`] живёт до конца кадра, поэтому
//! [`Rounded::draw`] обязательно снимает его за собой — иначе скруглением
//! поехали бы курсор, панель и всё, что рисуется следом.
//!
//! Две тонкости, без которых угол остаётся квадратным или чёрным:
//!
//! · `underlying_storage` возвращает `None`. Иначе элемент имеет право уехать
//!   на отдельную плоскость DRM (direct scanout) — а плоскость показывает
//!   буфер клиента КАК ЕСТЬ, мимо всякого шейдера. Окно во весь экран, которое
//!   scanout любит больше всего, получило бы прямые углы;
//! · `opaque_regions` пустой. Непрозрачная область — это обещание компоситору,
//!   что под ней рисовать нечего; поверив ему, он оставил бы под вырезанными
//!   углами неочищенный мусор предыдущего кадра.

use smithay::backend::renderer::{
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, Uniform, UniformName, UniformType},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Transform};
use smithay::utils::user_data::UserDataMap;

/// Исходник фрагментного шейдера.
///
/// Собран из штатного `texture.frag` smithay (та же обвязка `//_DEFINES_`,
/// `EXTERNAL`, `NO_ALPHA`, `DEBUG_FLAGS` — без них рендер не соберёт вариант
/// программы и упадёт при первом же внешнем текстурном буфере) плюс расчёт
/// принадлежности пикселя скруглённому прямоугольнику.
///
/// Координаты берём из `gl_FragCoord`, а НЕ из `v_coords`. Разница
/// принципиальная: `v_coords` — координаты внутри ТЕКСТУРЫ, а у окна их
/// несколько (субповерхности: видеослой у плеера, popup-меню, клиентские
/// рамки), и каждая получила бы своё собственное скругление по краям. Экранные
/// же координаты одни на всех, и рамка скругления задаётся прямоугольником
/// самого окна.
///
/// **Y НЕ переворачиваем, и это не небрежность.** Проекция smithay
/// (`GlesRenderer::render`: `flip180 * transform * ortho`) уже кладёт
/// логический ноль в `gl_FragCoord.y = 0` — то есть экранные координаты dawn и
/// координаты фрагмента совпадают по обеим осям. Стоявший здесь переворот
/// `fb_size.y - gl_FragCoord.y` ЗЕРКАЛИЛ маску по вертикали: рамка окна в
/// верхней половине экрана уезжала в нижнюю, пересечения с самим окном не
/// оставалось вовсе — и скругление просто не появлялось. Заметить это было
/// трудно ровно по одной причине: окно ВО ВЕСЬ ЭКРАН симметрично относительно
/// середины кадра, его зеркальная рамка совпадает с настоящей, и углы у него
/// скруглялись правильно. Замер 25.08.2026 в харнессе: у окна 358x203 на
/// (26,26) все четыре угла квадратные, у окна во весь экран — круглые; жёсткая
/// обрезка (`hard_clip`) по той же причине срезала окно ЦЕЛИКОМ вместо его
/// нижней части.
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

// Прямоугольник окна на экране: x, y, ширина, высота (физические пиксели,
// начало отсчёта — левый ВЕРХНИЙ угол кадра).
uniform vec4 win_rect;
// Радиус скругления в тех же физических пикселях.
uniform float win_radius;
// 1.0 — резать ещё и СНАРУЖИ win_rect целиком (не только дугу по углам).
// Нужно, когда win_rect — это не факт клиента, а то, что у него ЗАПРОСИЛИ
// (см. Rounded::new, параметр hard_clip): клиент, который ужиматься дальше
// своего внутреннего минимума не умеет, рисует буфер БОЛЬШЕ рамки — без
// этой резки он бы просто торчал из неё, а не ужимался.
uniform float hard_clip;

// Знаковое расстояние до скруглённого прямоугольника: <0 внутри, >0 снаружи.
float rounded_box(vec2 p, vec2 half_size, float r) {
    vec2 q = abs(p) - half_size + vec2(r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2(0.0))) - r;
}

void main() {
    vec4 color = texture2D(tex, v_coords);

#if defined(NO_ALPHA)
    color = vec4(color.rgb, 1.0) * alpha;
#else
    color = color * alpha;
#endif

#if defined(DEBUG_FLAGS)
    if (tint == 1.0)
        color = vec4(0.0, 0.2, 0.0, 0.2) + color * 0.8;
#endif

    if (win_radius > 0.0 || hard_clip > 0.5) {
        vec2 p = gl_FragCoord.xy;
        vec2 half_size = win_rect.zw * 0.5;
        vec2 center = win_rect.xy + half_size;
        vec2 q = abs(p - center) - half_size;

        // hard_clip: резать ЛЮБОЙ пиксель снаружи win_rect целиком — это НЕ
        // элемент окна-хозяина с попапами (см. ниже), а его основное дерево
        // поверхностей САМО ПО СЕБЕ, у которого win_rect — не факт (не то, что
        // клиент нарисовал), а то, что у него ЗАПРОСИЛИ (Rounded::new,
        // hard_clip). Обрубать снаружи можно смело: попапы сюда не попадают —
        // рендер (udev.rs, "нужен_кроп") строит их ОТДЕЛЬНЫМ, необрезанным
        // списком именно ради этого.
        if (hard_clip > 0.5 && (q.x > 0.0 || q.y > 0.0)) {
            // Гасим ЦЕЛИКОМ, а не только альфу. Кадр смешивается с
            // ПРЕДумноженной альфой (ONE, ONE_MINUS_SRC_ALPHA), то есть цвет
            // источника ПРИБАВЛЯЕТСЯ к фону, а альфа лишь говорит, сколько
            // фона оставить. `color.a = 0.0` при живом RGB — это «прибавить
            // цвет окна поверх фона, ничего не пряча»: обрезанная часть
            // оставалась видимой ровно как была, и обрезка выглядела
            // несработавшей (замер 25.08.2026 в харнессе: окно 1358x163 при
            // запрошенных 1358x78 рисовалось до самого низа, при этом в
            // логе честно стояло hard_clip=1.0 и верный win_rect).
            // Скругление углов ниже с самого начала множит ВЕСЬ color — по
            // той же причине, и оно работало.
            color = vec4(0.0);
        } else if (q.x <= 0.0 && q.y <= 0.0 && win_radius > 0.0) {
            // Обычный путь (hard_clip=0): режем ТОЛЬКО ВНУТРИ рамки — там и
            // живут углы. Иначе шейдер работает не вырезателем углов, а
            // ножницами по всей площади: `Window::render_elements` отдаёт в
            // одном списке и окно, и его ПОПАПЫ (меню, выпадающие списки,
            // тултипы — см. smithay, space/wayland/window.rs), а win_rect у
            // них общий, окна-хозяина. Любое меню, вылезшее за край своего
            // окна, обрезалось бы по ровной невидимой линии; туда же уходили
            // клиентские тени CSD и всякая субповерхность за границей
            // geometry(). Снаружи это и выглядит как «окна срезаются
            // невидимыми стенами» (жалоба 24.08.2026).
            //
            // Плитка-маска, которая была до шейдера, красила ровно четыре
            // квадратика в углах и за рамку не выходила НИКОГДА — то есть
            // здесь возвращается её поведение, только вырезанием вместо
            // закрашивания.
            float d = rounded_box(p - center, half_size, win_radius);
            // Растушёвка ровно в один пиксель: без неё дуга выходит ступенькой,
            // с большей — угол выглядит замыленным.
            color *= 1.0 - smoothstep(-0.5, 0.5, d);
        }
    }

    gl_FragColor = color;
}
"#;

/// Скомпилированная программа. Компилируется один раз на рендерер.
#[derive(Debug, Clone)]
pub struct Шейдер(GlesTexProgram);

impl Шейдер {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [
            UniformName::new("win_rect", UniformType::_4f),
            UniformName::new("win_radius", UniformType::_1f),
            UniformName::new("hard_clip", UniformType::_1f),
        ];
        match renderer.compile_custom_texture_shader(ФРАГМЕНТ, &uniforms) {
            Ok(program) => Some(Self(program)),
            Err(e) => {
                // Не фатально: без шейдера окна просто останутся с прямыми
                // углами. Молчать нельзя — иначе «скругление пропало» пришлось
                // бы искать снаружи.
                tracing::warn!("dawn/rounded: шейдер скругления не собрался: {:?}", e);
                None
            }
        }
    }
}

/// Обёртка над элементом окна: та же картинка, но нарисованная шейдером
/// скругления.
#[derive(Debug)]
pub struct Rounded<E> {
    inner: E,
    program: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
}

impl<E> Rounded<E> {
    /// `rect` — прямоугольник окна на экране в физических пикселях,
    /// `radius` — радиус там же.
    ///
    /// Рамка приходит ДРОБНОЙ, и это не придирка. Округлив её до целого, мы
    /// получаем маску, которая на дробной камере (а камера почти всегда
    /// дробная — пружина, инерция, зум) прыгает то на пиксель влево, то
    /// вправо — на каждый кадр. Снаружи ровно это и читается как «маска
    /// скругления мигает»: дуга дрожит по краю, пока холст едет.
    /// `hard_clip` — резать ли ещё и снаружи `rect` целиком, не только дугу
    /// по углам (см. uniform `hard_clip` в шейдере выше). Нужно true только
    /// для основного дерева поверхностей окна, которое рисует БОЛЬШЕ, чем у
    /// него запросили (см. udev.rs, "нужен_кроп"); во всех остальных случаях
    /// — false, ровно старое поведение (дуга по углам, снаружи не трогаем).
    pub fn new(inner: E, шейдер: &Шейдер, rect: [f32; 4], radius: f32, hard_clip: bool) -> Self {
        Self {
            inner,
            program: шейдер.0.clone(),
            uniforms: vec![
                Uniform::new("win_rect", rect),
                Uniform::new("win_radius", radius),
                Uniform::new("hard_clip", if hard_clip { 1.0f32 } else { 0.0f32 }),
            ],
        }
    }

    /// То же от целочисленного прямоугольника — для плашек интерфейса, которые
    /// и так стоят на целых пикселях экрана и никуда не едут. `hard_clip`
    /// всегда false — плашки не имеют дела с чужим неподатливым содержимым.
    pub fn from_rect(
        inner: E,
        шейдер: &Шейдер,
        rect: Rectangle<i32, Physical>,
        radius: f32,
    ) -> Self {
        Self::new(
            inner,
            шейдер,
            [
                rect.loc.x as f32,
                rect.loc.y as f32,
                rect.size.w as f32,
                rect.size.h as f32,
            ],
            radius,
            false,
        )
    }
}

impl<E: Element> Element for Rounded<E> {
    /// Id берём У ВЛОЖЕННОГО элемента, а не свежий на каждый кадр: damage
    /// tracker ведёт историю повреждений по Id, и новый Id каждый кадр означал
    /// бы полную перерисовку экрана всегда (ровно эта грабля уже стоила dawn
    /// производительности на плитках декораций, см. decor.rs).
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

    /// Пусто — см. шапку модуля: под вырезанным углом рисовать ЕСТЬ что.
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

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Rounded<E> {
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
        // Снять override обязаны в ЛЮБОМ случае, включая ошибку отрисовки:
        // он живёт до конца кадра, и забытый здесь скруглил бы всё, что
        // рисуется после этого окна.
        let итог = self.inner.draw(frame, src, dst, damage, opaque_regions, cache);
        frame.clear_tex_program_override();
        итог
    }

    /// `None` — окну запрещено уезжать на плоскость DRM мимо шейдера
    /// (см. шапку модуля).
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
