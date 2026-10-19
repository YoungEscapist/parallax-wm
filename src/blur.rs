//! Размытие фона под полупрозрачными плашками parallax (Hyprland/macOS).
//!
//! **Что это делает.** Берёт текстуру (сейчас — обои фонового слоя), сворачивает
//! её гауссом в два прохода и отдаёт готовую текстуру. Дальше отрисовка кладёт
//! её под островами панели, меню и миникартой, обрезанную по их прямоугольникам:
//! получается тот самый эффект «матового стекла», где сквозь плашку видно
//! размытый фон, а не резкую картинку и не сплошную заливку.
//!
//! **Почему два прохода, а не один.** Гаусс разделим: свёртка 2D-ядром N×N
//! равна двум свёрткам одномерными ядрами по N. Для радиуса 16 это 32 выборки
//! вместо 1024 — разница не в проценте, а в порядке, и на 190 кадрах в секунду
//! она решающая. Плюс исходник сперва ужимается вчетверо: на размытой картинке
//! разрешение не видно, а выборок становится в шестнадцать раз меньше.
//!
//! **Что здесь ОСОЗНАННО не сделано.** Размывается ФОН (обои), а не вся сцена
//! под плашкой. Настоящий backdrop-blur требует рисовать весь кадр в offscreen
//! и размывать его — то есть лишний полноэкранный проход на КАЖДЫЙ кадр плюс
//! память под кадр целиком. Для панели поверх обоев разницы не видно, а цена
//! известна и постоянна.
//!
//! **ПО УМОЛЧАНИЮ ВЫКЛЮЧЕНО** (`set{ blur = false }`). Причина честная: код
//! написан, но живьём не отсмотрен ни разу — сеанс, в котором его можно
//! проверить, идёт на старом бинаре. Ошибка в проходе рендера стоит дороже
//! ошибки в любом другом месте parallax: это чёрный экран без окон. Включать —
//! когда есть под рукой tty.

use smithay::backend::renderer::{
    Bind, Frame as _, Offscreen, Renderer as _, Texture as _,
    gles::{GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType},
};
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Size, Transform};

/// Во сколько раз ужимается исходник перед размытием.
///
/// Четыре — не «чтобы быстрее», а часть самого размытия: уменьшение с
/// билинейной фильтрацией уже усредняет по 2×2, то есть даёт первую ступень
/// свёртки бесплатно. Радиус в шейдере поэтому считается в пикселях УЖАТОЙ
/// картинки и на экране выглядит вчетверо шире.
pub const УЖАТИЕ: i32 = 4;

/// Радиус под островами панели (в пикселях экрана). Под окнами — ручка
/// `blur_radius` из конфига (19.10.2026).
pub const РАДИУС: f32 = 12.0;

/// Фрагментный шейдер одного прохода.
///
/// Обвязка (`//_DEFINES_`, `EXTERNAL`, `NO_ALPHA`, `DEBUG_FLAGS`) взята из
/// штатного `texture.frag` smithay и обязательна: без неё рендер не соберёт
/// вариант программы под внешний текстурный буфер и упадёт на первом же.
///
/// Ядро — девять выборок с весами Гаусса. Направление задаётся `dir` (1,0) или
/// (0,1), шаг — `texel`, то есть 1/размер текстуры: в пикселях считать нельзя,
/// координаты текстуры нормированы.
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

// Направление прохода: (1,0) — горизонтальный, (0,1) — вертикальный.
uniform vec2 blur_dir;
// Размер одного тексела: 1/ширина, 1/высота.
uniform vec2 blur_texel;
// Радиус в текселах ужатой картинки.
uniform float blur_radius;

void main() {
    // Веса нормального распределения при sigma = radius/2. Считаем на месте:
    // радиус приходит в uniform, таблицу под него не заготовить.
    float sigma = max(blur_radius * 0.5, 0.0001);
    vec4 sum = vec4(0.0);
    float norm = 0.0;
    for (int i = -4; i <= 4; i++) {
        float d = float(i) * blur_radius * 0.25;
        float w = exp(-(d * d) / (2.0 * sigma * sigma));
        vec2 off = blur_dir * blur_texel * d;
        sum += texture2D(tex, v_coords + off) * w;
        norm += w;
    }
    vec4 color = sum / norm;

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

/// Скомпилированная программа плюс два промежуточных буфера.
///
/// Буферы держим ПОСТОЯННЫМИ и пересоздаём только при смене размера: создавать
/// текстуру на каждый кадр — это аллокация в видеопамяти шестьдесят раз в
/// секунду, ровно та ошибка, из-за которой в parallax уже заводили пул буферов для
/// строк текста (см. text.rs).
pub struct Блюр {
    программа: GlesTexProgram,
    /// Три буфера: сборка сцены, горизонтальный проход, вертикальный.
    буферы: Option<(GlesTexture, GlesTexture, GlesTexture)>,
    размер: Size<i32, Physical>,
}

/// Обои на ЭКРАНЕ в физических пикселях — ровно там же, где их кладёт
/// `udev::build_wallpaper_backdrop`. Имя осталось от времён, когда обои
/// повторялись сеткой; сцена собирается из списка, а в списке бывает ноль или
/// одна штука.
#[derive(Clone, Copy, Debug)]
pub struct Плитка {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// Поворот плитки (сейчас всегда Normal).
    pub поворот: smithay::utils::Transform,
}

/// Как обои лежат на экране — то, что `Блюр::размыть` собирает перед свёрткой.
pub enum Сцена {
    /// Список мест (пусто — исходник растягивается на весь кадр).
    Плитки(Vec<Плитка>),
    /// Режим cycle: вся текстура повторяется шейдером плитки, ровно как в
    /// кадре (см. src/плитка_обоев.rs).
    Мозаика {
        программа: GlesTexProgram,
        раскладка: crate::плитка_обоев::Раскладка,
    },
}

impl Блюр {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [
            UniformName::new("blur_dir", UniformType::_2f),
            UniformName::new("blur_texel", UniformType::_2f),
            UniformName::new("blur_radius", UniformType::_1f),
        ];
        match renderer.compile_custom_texture_shader(ФРАГМЕНТ, &uniforms) {
            Ok(программа) => Some(Self { программа, буферы: None, размер: (0, 0).into() }),
            Err(e) => {
                // Не фатально: без шейдера плашки просто останутся без блюра.
                // Молчать нельзя — иначе «блюр не работает» пришлось бы искать
                // снаружи, а причина уже здесь.
                tracing::warn!("plx/blur: blur shader failed to compile: {:?}", e);
                None
            }
        }
    }

    /// Размыть обои ТАК, КАК ОНИ ЛЕЖАТ НА ЭКРАНЕ, и вернуть готовую текстуру
    /// (та же, что и в прошлый раз, если размер не менялся).
    ///
    /// `экран` — кадр монитора; `плитки` — раскладка обоев в его физических
    /// пикселях (пусто — исходник растягивается на весь кадр, как было раньше).
    /// Возврат None означает «блюра в этом кадре не будет» — вызывающий обязан
    /// просто нарисовать плашку как раньше.
    ///
    /// **Зачем сборка сцены, а не просто свёртка исходника.** Заплаты
    /// (`udev::build_blur_patch`) сэмплируют результат по ЭКРАННЫМ координатам:
    /// пятно под плашкой в точке (x,y) берётся из (x/УЖАТИЕ, y/УЖАТИЕ). Пока
    /// сюда приходила голая текстура обоев, это молча предполагало, что обои
    /// лежат ровно в кадре экрана — то есть камера в нуле и зум единица. У parallax
    /// холст бесконечный, обои едут вместе с ним (`build_wallpaper_backdrop`), и
    /// размытие уезжало от того, что под ним, тем сильнее, чем дальше уехала
    /// камера. Под тонкими островами панели это было незаметно, а под окном во
    /// весь экран (блюр фона терминала) стало бы видно сразу. Собирая плитки в
    /// свой буфер тем же расчётом, что и кадр, мы получаем совпадение по
    /// построению — и заодно чиним прежний перекос под панелью.
    pub fn размыть(
        &mut self,
        renderer: &mut GlesRenderer,
        исходник: &GlesTexture,
        сцена: Сцена,
        экран: Size<i32, Physical>,
        радиус: f32,
    ) -> Option<GlesTexture> {
        if экран.w <= 0 || экран.h <= 0 {
            return None;
        }
        let мал = Size::<i32, Physical>::from((
            (экран.w / УЖАТИЕ).max(1),
            (экран.h / УЖАТИЕ).max(1),
        ));
        if self.буферы.is_none() || self.размер != мал {
            let буфер = |r: &mut GlesRenderer| -> Option<GlesTexture> {
                let s = Size::<i32, BufferCoords>::from((мал.w, мал.h));
                Offscreen::<GlesTexture>::create_buffer(r, Fourcc::Abgr8888, s)
                    .map_err(|e| tracing::warn!("plx/blur: buffer {:?}: {:?}", s, e))
                    .ok()
            };
            let (a, b, c) = (буфер(renderer)?, буфер(renderer)?, буфер(renderer)?);
            self.буферы = Some((a, b, c));
            self.размер = мал;
        }
        let (мут_a, мут_b, мут_c) = self.буферы.clone()?;

        let полный = Rectangle::<i32, Physical>::new((0, 0).into(), мал);
        let texel = [1.0 / мал.w as f32, 1.0 / мал.h as f32];
        // Радиус задан в пикселях ЭКРАНА, а свёртка идёт по ужатой картинке.
        let радиус = (радиус / УЖАТИЕ as f32).max(0.5);

        let src_full = Rectangle::<f64, BufferCoords>::from_size(Size::from((
            исходник.width() as f64,
            исходник.height() as f64,
        )));

        // ── Проход 0: сборка сцены обоев в C, в масштабе 1/УЖАТИЕ ───────────
        let mut цель0 = мут_c.clone();
        {
            let mut fb = renderer.bind(&mut цель0).ok()?;
            let mut frame = renderer.render(&mut fb, мал, Transform::Normal).ok()?;
            // Чистим: плитки могут не покрыть кадр целиком (обои уже экрана,
            // сетка не сложилась), и остаток прошлого кадра тогда просвечивал бы.
            frame.clear(smithay::backend::renderer::Color32F::BLACK, &[полный]).ok()?;
            let к = УЖАТИЕ as f64;
            match сцена {
                Сцена::Мозаика { программа, раскладка } => {
                    // Та же раскладка, ужатая в УЖАТИЕ раз: экран, плитка и
                    // угол — всё в пикселях маленького буфера.
                    let р = crate::плитка_обоев::Раскладка {
                        плитка: (раскладка.плитка.0 / к, раскладка.плитка.1 / к),
                        начало: (раскладка.начало.0 / к, раскладка.начало.1 / к),
                        экран: (мал.w as f64, мал.h as f64),
                    };
                    frame.render_texture_from_to(
                        исходник, src_full, полный, &[полный], &[], Transform::Normal, 1.0,
                        Some(&программа), &р.uniforms(),
                    ).ok()?;
                }
                Сцена::Плитки(плитки) if плитки.is_empty() => {
                    frame.render_texture_from_to(
                        исходник, src_full, полный, &[полный], &[], Transform::Normal, 1.0, None, &[],
                    ).ok()?;
                }
                Сцена::Плитки(плитки) => {
                    for п in плитки {
                        let dst = Rectangle::<i32, Physical>::new(
                            Point::from(((п.x / к).round() as i32, (п.y / к).round() as i32)),
                            Size::from((
                                (п.w / к).round().max(1.0) as i32,
                                (п.h / к).round().max(1.0) as i32,
                            )),
                        );
                        let Some(видно) = dst.intersection(полный) else { continue };
                        frame.render_texture_from_to(
                            исходник, src_full, dst, &[видно], &[], п.поворот, 1.0, None, &[],
                        ).ok()?;
                    }
                }
            }
            let _ = frame.finish().ok()?;
        }

        // ── Проход 1: C → A, по горизонтали ─────────────────────────────────
        let src_мал_1 = Rectangle::<f64, BufferCoords>::from_size(Size::from((
            мал.w as f64, мал.h as f64,
        )));
        let mut цель = мут_a.clone();
        {
            let mut fb = renderer.bind(&mut цель).ok()?;
            let mut frame = renderer.render(&mut fb, мал, Transform::Normal).ok()?;
            frame.render_texture_from_to(
                &мут_c, src_мал_1, полный, &[полный], &[], Transform::Normal, 1.0,
                Some(&self.программа),
                &[
                    Uniform::new("blur_dir", [1.0f32, 0.0]),
                    Uniform::new("blur_texel", texel),
                    Uniform::new("blur_radius", радиус),
                ],
            ).ok()?;
            let _ = frame.finish().ok()?;
        }

        // ── Проход 2: A → B, по вертикали ───────────────────────────────────
        let src_мал = Rectangle::<f64, BufferCoords>::from_size(Size::from((
            мал.w as f64, мал.h as f64,
        )));
        let mut цель2 = мут_b.clone();
        {
            let mut fb = renderer.bind(&mut цель2).ok()?;
            let mut frame = renderer.render(&mut fb, мал, Transform::Normal).ok()?;
            frame.render_texture_from_to(
                &мут_a, src_мал, полный, &[полный], &[], Transform::Normal, 1.0,
                Some(&self.программа),
                &[
                    Uniform::new("blur_dir", [0.0f32, 1.0]),
                    Uniform::new("blur_texel", texel),
                    Uniform::new("blur_radius", радиус),
                ],
            ).ok()?;
            let _ = frame.finish().ok()?;
        }

        Some(мут_b)
    }

    /// Ещё одна пара проходов (по горизонтали и по вертикали) поверх
    /// результата прошлого `размыть`. Нужна широкому свечению экранного
    /// шейдера: девять выборок одного прохода на большом радиусе стоят редко,
    /// и яркая точка расплывается не в ореол, а в «бусы». Второй проход
    /// складывает гауссы (σ растёт в √2 раза), и шаг между выборками
    /// замазывается. Буферы те же, аллокаций нет.
    pub fn ещё_проход(&mut self, renderer: &mut GlesRenderer, радиус: f32) -> Option<GlesTexture> {
        let (мут_a, мут_b, _) = self.буферы.clone()?;
        let мал = self.размер;
        let полный = Rectangle::<i32, Physical>::new((0, 0).into(), мал);
        let texel = [1.0 / мал.w as f32, 1.0 / мал.h as f32];
        let радиус = (радиус / УЖАТИЕ as f32).max(0.5);
        let src = Rectangle::<f64, BufferCoords>::from_size(Size::from((мал.w as f64, мал.h as f64)));
        for (из, в, dir) in [(&мут_b, &мут_a, [1.0f32, 0.0]), (&мут_a, &мут_b, [0.0f32, 1.0])] {
            let mut цель = в.clone();
            let mut fb = renderer.bind(&mut цель).ok()?;
            let mut frame = renderer.render(&mut fb, мал, Transform::Normal).ok()?;
            frame.render_texture_from_to(
                из, src, полный, &[полный], &[], Transform::Normal, 1.0,
                Some(&self.программа),
                &[
                    Uniform::new("blur_dir", dir),
                    Uniform::new("blur_texel", texel),
                    Uniform::new("blur_radius", радиус),
                ],
            ).ok()?;
            let _ = frame.finish().ok()?;
        }
        Some(мут_b)
    }
}

// ════════════════════════════════════════════════════════════════════════════
// Настоящее матовое стекло под окном (05.10.2026)
// ════════════════════════════════════════════════════════════════════════════
//
// Жалоба Ярика: «блюр под окном показывает размытые обои, а не то, что за
// окном, — окно за окном не видно», и «после смены обоев стекло окон не
// меняется». Обе — от одного устройства: заплата под окном брала ОДНУ общую
// текстуру размытых обоев, собранную до кадра, а её Id с постоянным счётчиком
// коммитов не давал damage tracker'у узнать, что картинка внутри сменилась.
//
// Теперь под прозрачным окном лежит элемент-ЭФФЕКТ (`is_framebuffer_effect`,
// штатный механизм smithay): перед его отрисовкой smithay зовёт
// `capture_framebuffer`, и в этот момент в буфере кадра уже нарисовано всё,
// что лежит ПОД ним, — обои, окна ниже, их стекло. Мы ужимаем этот кусок в
// свой буфер, размываем теми же двумя проходами и кладём на место. Повторный
// захват smithay делает сам и ровно тогда, когда под элементом что-то
// изменилось (сменились обои, сдвинулось окно ниже), — отдельной проверки
// свежести не нужно, и починка «стекло не видит новых обоев» выходит даром.

use smithay::backend::renderer::{
    FrameContext as _,
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, ffi},
    utils::CommitCounter,
};
use smithay::utils::{Scale, user_data::UserDataMap};

/// Общее для всех стёкол кадра: программа свёртки. Живёт в `Parallax`
/// (`стекло`), собирается вместе с прочими шейдерами.
#[derive(Clone, Debug)]
pub struct Стекло {
    программа: GlesTexProgram,
}

impl Стекло {
    pub fn new(renderer: &mut GlesRenderer) -> Option<Self> {
        let uniforms = [
            UniformName::new("blur_dir", UniformType::_2f),
            UniformName::new("blur_texel", UniformType::_2f),
            UniformName::new("blur_radius", UniformType::_1f),
        ];
        match renderer.compile_custom_texture_shader(ФРАГМЕНТ, &uniforms) {
            Ok(программа) => Some(Self { программа }),
            Err(e) => {
                tracing::warn!("plx/blur: backdrop shader failed to compile: {:?}", e);
                None
            }
        }
    }

    /// Стекло в прямоугольнике `rect` (физические пиксели кадра) на кадре
    /// размера `экран`. `None` — прямоугольник целиком за краем.
    pub fn подложка(
        &self,
        id: Id,
        rect: Rectangle<i32, Physical>,
        экран: Size<i32, Physical>,
        радиус: f32,
    ) -> Option<Подложка> {
        // Захват читает буфер кадра — за его краем читать нечего, поэтому
        // элемент сразу обрезан по экрану. Скругление и растворение снаружи
        // считают по ПОЛНОЙ рамке окна (`gl_FragCoord`), так что обрезка им не
        // мешает.
        let rect = rect.intersection(Rectangle::from_size(экран))?;
        if rect.size.w <= 0 || rect.size.h <= 0 {
            return None;
        }
        Some(Подложка { id, rect, программа: self.программа.clone(), радиус })
    }
}

/// Стекло под одним окном (см. заметку выше).
#[derive(Debug)]
pub struct Подложка {
    id: Id,
    rect: Rectangle<i32, Physical>,
    программа: GlesTexProgram,
    радиус: f32,
}

/// Что стекло держит между кадрами — в `cache` эффекта (smithay заводит его
/// по Id элемента и хранит, пока элемент жив). Три буфера, как у `Блюр`:
/// ужатый захват, горизонтальный проход, вертикальный (он же результат).
struct Снимок {
    c: GlesTexture,
    a: GlesTexture,
    b: GlesTexture,
    мал: Size<i32, Physical>,
    /// Захват удался хоть раз — до этого рисовать нечего.
    готов: bool,
}

type ЯчейкаСнимка = std::cell::RefCell<Option<Снимок>>;

impl Подложка {
    fn мал(&self) -> Size<i32, Physical> {
        Size::from((
            ((self.rect.size.w + УЖАТИЕ - 1) / УЖАТИЕ).max(1),
            ((self.rect.size.h + УЖАТИЕ - 1) / УЖАТИЕ).max(1),
        ))
    }

    fn захватить(&self, frame: &mut GlesFrame<'_, '_>, dst: Rectangle<i32, Physical>, cache: &UserDataMap) -> Option<()> {
        let мал = self.мал();
        let ячейка = cache.get_or_insert(|| ЯчейкаСнимка::new(None));

        // Буфер кадра, в который сейчас рисуют, — его и читаем. Узнаём ДО
        // того, как свои буферы перебьют привязку.
        let кадр_fbo = frame.with_context(|gl| unsafe {
            let mut fbo: ffi::types::GLint = 0;
            gl.GetIntegerv(ffi::FRAMEBUFFER_BINDING, &mut fbo);
            fbo as ffi::types::GLuint
        }).ok()?;

        let mut г = frame.renderer();
        let r: &mut GlesRenderer = г.as_mut();

        let нужно_заново = ячейка.borrow().as_ref().is_none_or(|с| с.мал != мал);
        if нужно_заново {
            let буфер = |r: &mut GlesRenderer| -> Option<GlesTexture> {
                let s = Size::<i32, BufferCoords>::from((мал.w, мал.h));
                Offscreen::<GlesTexture>::create_buffer(r, Fourcc::Abgr8888, s)
                    .map_err(|e| tracing::warn!("plx/blur: backdrop buffer {:?}: {:?}", s, e))
                    .ok()
            };
            let (c, a, b) = (буфер(r)?, буфер(r)?, буфер(r)?);
            // Края — ПРИЖАТЬ, а не повторять: по умолчанию у текстуры REPEAT,
            // и выборки Гаусса у нижнего края стекла брали верхний ряд захвата
            // (замер: полосы окна, торчащего из-под заголовка, проступали
            // цветной каймой внизу).
            r.with_context(|gl| unsafe {
                for t in [&c, &a, &b] {
                    gl.BindTexture(ffi::TEXTURE_2D, t.tex_id());
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, ffi::CLAMP_TO_EDGE as i32);
                    gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);
                }
                gl.BindTexture(ffi::TEXTURE_2D, 0);
            }).ok()?;
            *ячейка.borrow_mut() = Some(Снимок { c, a, b, мал, готов: false });
        }
        let (c, a, b) = {
            let с = ячейка.borrow();
            let с = с.as_ref()?;
            (с.c.clone(), с.a.clone(), с.b.clone())
        };

        // ── Захват: кусок кадра под окном → C, сразу ужатый ─────────────────
        // glBlitFramebuffer, а не glCopyTexSubImage2D: копия требует, чтобы у
        // буфера кадра были все каналы текстуры, а сканаут обычно XRGB без
        // альфы — на таком мониторе копия молча не работала бы. Блит же
        // заодно ужимает с линейной фильтрацией — первая ступень размытия.
        // Ось Y та же, что у экрана (см. заметку о `gl_FragCoord` в rounded.rs).
        let полный = Rectangle::<i32, Physical>::new((0, 0).into(), мал);
        {
            // Привязку буфера делает `render`, а не `bind`: блит — изнутри
            // кадра своего буфера, тогда FRAMEBUFFER_BINDING и есть он.
            let mut цель = c.clone();
            let mut fb = r.bind(&mut цель).ok()?;
            let mut f = r.render(&mut fb, мал, Transform::Normal).ok()?;
            f.with_context(|gl| unsafe {
                let mut своё: ffi::types::GLint = 0;
                gl.GetIntegerv(ffi::FRAMEBUFFER_BINDING, &mut своё);
                gl.Disable(ffi::SCISSOR_TEST);
                gl.BindFramebuffer(ffi::READ_FRAMEBUFFER, кадр_fbo);
                gl.BindFramebuffer(ffi::DRAW_FRAMEBUFFER, своё as ffi::types::GLuint);
                gl.BlitFramebuffer(
                    dst.loc.x, dst.loc.y, dst.loc.x + dst.size.w, dst.loc.y + dst.size.h,
                    0, 0, мал.w, мал.h,
                    ffi::COLOR_BUFFER_BIT, ffi::LINEAR,
                );
                gl.BindFramebuffer(ffi::FRAMEBUFFER, своё as ffi::types::GLuint);
                gl.Enable(ffi::SCISSOR_TEST);
            }).ok()?;
            let _ = f.finish().ok()?;
        }

        // ── Проходы Гаусса: C → A → B, затем B → A → B ещё раз и ещё ────────
        // Шейдер берёт 9 выборок на радиус: на большом радиусе (blur_radius)
        // они разошлись бы через тексел и дали полосы. Поэтому пара проходов
        // повторяется k раз с радиусом R/√k — гауссы складываются в один с
        // тем же R, а каждый проход остаётся мелким и гладким.
        let texel = [1.0 / мал.w as f32, 1.0 / мал.h as f32];
        let полный_радиус = (self.радиус / УЖАТИЕ as f32).max(0.5);
        let повторов = ((полный_радиус / 4.0).powi(2).ceil() as usize).clamp(1, 8);
        let радиус = полный_радиус / (повторов as f32).sqrt();
        let src = Rectangle::<f64, BufferCoords>::from_size(Size::from((мал.w as f64, мал.h as f64)));
        let первый = [(&c, &a, [1.0f32, 0.0]), (&a, &b, [0.0f32, 1.0])];
        let дальше = [(&b, &a, [1.0f32, 0.0]), (&a, &b, [0.0f32, 1.0])];
        for (из, в, dir) in (0..повторов).flat_map(|n| if n == 0 { первый } else { дальше }) {
            let mut цель = в.clone();
            let mut fb = r.bind(&mut цель).ok()?;
            let mut f = r.render(&mut fb, мал, Transform::Normal).ok()?;
            f.render_texture_from_to(
                из, src, полный, &[полный], &[], Transform::Normal, 1.0,
                Some(&self.программа),
                &[
                    Uniform::new("blur_dir", dir),
                    Uniform::new("blur_texel", texel),
                    Uniform::new("blur_radius", радиус),
                ],
            ).ok()?;
            let _ = f.finish().ok()?;
        }
        if let Some(с) = ячейка.borrow_mut().as_mut() {
            с.готов = true;
        }
        Some(())
        // `г` уходит здесь: его Drop возвращает привязку к буферу кадра,
        // вьюпорт, ножницы и смешивание (GlesFrameGuard).
    }
}

impl Element for Подложка {
    fn id(&self) -> &Id {
        &self.id
    }

    /// Свой счётчик не нужен: о смене того, что ПОД стеклом, smithay узнаёт
    /// сам (эффект перезахватывается по повреждению под ним).
    fn current_commit(&self) -> CommitCounter {
        CommitCounter::default()
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        Rectangle::from_size(Size::from((self.rect.size.w as f64, self.rect.size.h as f64)))
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.rect
    }

    fn kind(&self) -> Kind {
        Kind::Unspecified
    }

    fn is_framebuffer_effect(&self) -> bool {
        true
    }
}

impl RenderElement<GlesRenderer> for Подложка {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        let Some(ячейка) = cache.and_then(|c| c.get::<ЯчейкаСнимка>()) else { return Ok(()) };
        let (текстура, мал) = {
            let с = ячейка.borrow();
            match с.as_ref() {
                Some(с) if с.готов => (с.b.clone(), с.мал),
                _ => return Ok(()),
            }
        };
        let src = Rectangle::<f64, BufferCoords>::from_size(Size::from((мал.w as f64, мал.h as f64)));
        // Программа — None: скругление (и растворение) ставит обёртка
        // `Rounded` через override, как у любой текстуры окна.
        frame.render_texture_from_to(
            &текстура, src, dst, damage, &[], Transform::Normal, 1.0, None, &[],
        )
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        // Отказ мягкий: без захвата стекло просто не нарисуется в этом кадре,
        // а ронять из-за него весь кадр нельзя.
        if self.захватить(frame, dst, cache).is_none() {
            tracing::debug!("plx/blur: backdrop capture failed for {:?}", dst);
        }
        Ok(())
    }
}

impl std::fmt::Debug for Блюр {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Blur").field("size", &self.размер).finish()
    }
}
