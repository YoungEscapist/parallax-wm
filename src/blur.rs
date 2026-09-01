//! Размытие фона под полупрозрачными плашками dawn (Hyprland/macOS).
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
//! ошибки в любом другом месте dawn: это чёрный экран без окон. Включать —
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

/// Радиус по умолчанию (в пикселях экрана). Вилка из плана — 8..16.
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
/// секунду, ровно та ошибка, из-за которой в dawn уже заводили пул буферов для
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
                tracing::warn!("dawn/blur: шейдер размытия не собрался: {:?}", e);
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
    /// лежат ровно в кадре экрана — то есть камера в нуле и зум единица. У dawn
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
        плитки: &[Плитка],
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
                    .map_err(|e| tracing::warn!("dawn/blur: буфер {:?}: {:?}", s, e))
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
            if плитки.is_empty() {
                frame.render_texture_from_to(
                    исходник, src_full, полный, &[полный], &[], Transform::Normal, 1.0, None, &[],
                ).ok()?;
            } else {
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
                        исходник, src_full, dst, &[видно], &[], Transform::Normal, 1.0, None, &[],
                    ).ok()?;
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
}

impl std::fmt::Debug for Блюр {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Блюр").field("размер", &self.размер).finish()
    }
}
