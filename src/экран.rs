//! Экранный шейдер: свой GLSL поверх ВСЕГО кадра — как
//! `decoration:screen_shader` в Hyprland.
//!
//! Ручка одна: `set{ screen_shader = "~/.config/parallax/shaders/crt.frag" }`.
//! Пусто — прохода нет вовсе и кадр собирается ровно как раньше; это и есть
//! «выключено».
//!
//! **Как это работает.** Обычно список элементов уходит прямо в
//! `DrmCompositor::render_frame`, то есть на плоскости монитора. Шейдеру нужен
//! готовый кадр как ТЕКСТУРА, поэтому при включённой ручке сцена рисуется
//! сперва в свой офскрин-буфер размером с экран (`OutputDamageTracker` внутри,
//! age = 0 — то есть кадр целиком), а на монитор уходит ОДИН элемент: эта
//! текстура, растянутая на весь экран и нарисованная пользовательской
//! программой.
//!
//! **Чем за это платят.** Полная перерисовка каждый кадр (damage tracking
//! обходится стороной: картинка всего экрана меняется от одного лишь `time`) и
//! лишний полноэкранный проход. Пока ручка пуста — не платят ничем: ни буфера,
//! ни трекера не заводится.
//!
//! **Формат файла — хайпрлендовский**, чтобы чужие шейдеры работали без правок:
//!
//! ```glsl
//! precision mediump float;
//! varying vec2 v_texcoord;
//! uniform sampler2D tex;
//! uniform float time;                  // секунды с запуска
//! uniform vec2 wl_output_resolution;    // размер экрана в пикселях
//! uniform sampler2D bloom;              // свой: размытый кадр (см. ниже)
//! void main() { gl_FragColor = texture2D(tex, v_texcoord); }
//! ```
//!
//! **`bloom` — расширение parallax.** Тот же кадр, ужатый вчетверо и размытый
//! гауссом в четыре прохода (радиус `РАДИУС_СВЕЧЕНИЯ`), в тех же координатах
//! `v_texcoord`. Одним проходом по пикселю широкое мягкое свечение не
//! получить: редкие выборки вокруг точки рисуют её КОПИИ (так и было в
//! haze.frag 17.09 — «размывается копиями, видно на курсоре»), а частые стоят
//! сотни чтений на пиксель. Считается, только если слово `bloom` встречается в
//! тексте шейдера, — остальные не платят ничем.
//!
//! Объявлять `tex`, `v_texcoord`, `time` и `wl_output_resolution` можно, а
//! можно и не объявлять: обвязка (см. `собрать`) их уже завела, повторные
//! объявления из текста вырезаются — иначе GLSL ругается на переопределение.
//! Имя `main` в файле переименовывается в `plx_user_main`, а наш `main` зовёт
//! его и домножает результат на `alpha` элемента, без чего кадр не умел бы
//! затухать (пелена входа, переходы).
//!
//! **ГРАБЛЯ, общая со всеми шейдерами parallax:** имена в GLSL — ТОЛЬКО
//! латиницей (GLSL ES 1.00 не знает не-ASCII идентификаторов), а комментарии
//! могут быть любыми. См. [[parallax-wallpaper-shaders]] в памяти.

use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::{
    Bind, Offscreen, Renderer as _, Texture as _,
    damage::OutputDamageTracker,
    element::{
        Element, Id, Kind, RenderElement, UnderlyingStorage,
        texture::TextureRenderElement,
        utils::{CropRenderElement, Relocate, RelocateRenderElement, RescaleRenderElement},
    },
    utils::{CommitCounter, DamageSet, OpaqueRegions},
    gles::{GlesError, GlesFrame, GlesRenderer, GlesTexProgram, GlesTexture, Uniform, UniformName, UniformType, UniformValue, ffi},
};
use smithay::output::Output;
use smithay::utils::{
    Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Size, Transform,
};
use smithay::utils::user_data::UserDataMap;

/// Обвязка вокруг пользовательского кода: то, чего ждёт от текстурной
/// программы сам smithay (`//_DEFINES_`, `EXTERNAL`, `NO_ALPHA`,
/// `DEBUG_FLAGS`), плюс имена, принятые у экранных шейдеров Hyprland.
const ШАПКА: &str = r#"#version 100

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

// Имена Hyprland: чужой шейдер обязан работать без единой правки.
#define v_texcoord v_coords
uniform float time;
uniform vec2 wl_output_resolution;
// Размытый кадр (расширение parallax, см. шапку экран.rs).
uniform sampler2D bloom;
"#;

/// Хвост: наш `main` зовёт пользовательский и доводит результат до того, чего
/// ждёт кадр — предумноженной альфы элемента и отладочной подкраски smithay.
const ХВОСТ: &str = r#"

void main() {
    plx_user_main();
    gl_FragColor *= alpha;
#if defined(DEBUG_FLAGS)
    if (tint == 1.0) {
        gl_FragColor = vec4(0.0, 0.3, 0.0, 0.2) + gl_FragColor * 0.8;
    }
#endif
}
"#;

/// Собрать исходник программы из текста файла.
///
/// Вырезаем повторные объявления того, что уже завела шапка: чужие шейдеры
/// объявляют `varying vec2 v_texcoord;` и `uniform sampler2D tex;` сами, а
/// второе объявление того же имени — ошибка компиляции, то есть чёрный экран.
pub fn собрать(исходник: &str) -> String {
    let mut тело = String::with_capacity(исходник.len() + 256);
    for строка in исходник.lines() {
        let s = строка.trim();
        // `#version` из чужого файла выбрасываем: он у нас уже есть и обязан
        // стоять самой первой строкой.
        if s.starts_with("#version") {
            continue;
        }
        let свой = s.starts_with("precision ")
            || объявление(s, "varying", "v_texcoord")
            || объявление(s, "uniform", "tex")
            || объявление(s, "uniform", "time")
            || объявление(s, "uniform", "wl_output_resolution")
            || объявление(s, "uniform", "bloom");
        if свой {
            continue;
        }
        тело.push_str(строка);
        тело.push('\n');
    }
    // Пользовательский main переезжает под своё имя — наш зовёт его.
    let тело = переименовать_main(&тело);
    format!("{ШАПКА}{тело}{ХВОСТ}")
}

/// Строка вида `uniform sampler2D tex;` — объявление того, что уже есть в шапке.
fn объявление(строка: &str, слово: &str, имя: &str) -> bool {
    if !строка.starts_with(слово) {
        return false;
    }
    // Имя — последнее слово перед `;` (или перед `[`, если это массив).
    let без_точки = строка.trim_end_matches(';').trim();
    без_точки
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .map(|последнее| последнее.trim_end_matches("[]") == имя)
        .unwrap_or(false)
}

/// `void main()` пользователя → `void plx_user_main()`.
///
/// Заменяется ПЕРВОЕ вхождение `main` после слова `void`: имя `main` может
/// встретиться и в комментарии, и в чужой строке, но объявление у шейдера одно.
fn переименовать_main(тело: &str) -> String {
    let mut out = String::with_capacity(тело.len() + 16);
    let mut заменено = false;
    for строка in тело.lines() {
        if !заменено && строка.contains("main") && строка.contains("void") {
            out.push_str(&строка.replacen("main", "plx_user_main", 1));
            заменено = true;
        } else {
            out.push_str(строка);
        }
        out.push('\n');
    }
    if !заменено {
        // Ни одного `void main` — шейдер всё равно не заработает, но пусть он
        // хотя бы скомпилируется с понятной ошибкой линковки, а не с молчанием.
        tracing::warn!("plx/screen: shader has no `void main`");
    }
    out
}

/// Элемент кадра: готовая картинка экрана, нарисованная пользовательской
/// программой. Устроен как `rounded::Rounded` — программа ставится на время
/// отрисовки ОДНОГО элемента и обязательно снимается.
#[derive(Debug)]
pub struct Экранный {
    inner: CropRenderElement<RelocateRenderElement<RescaleRenderElement<TextureRenderElement<GlesTexture>>>>,
    программа: GlesTexProgram,
    uniforms: Vec<Uniform<'static>>,
    /// Размытый кадр для `bloom`; None — шейдер его не просит.
    свечение: Option<GlesTexture>,
}

impl Element for Экранный {
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

    fn damage_since(&self, scale: Scale<f64>, _commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        // Весь экран каждый кадр: картинку считает шейдер, и она меняется от
        // одного лишь `time` — повреждений сцены под ней для этого мало.
        DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)])
    }

    /// Пусто — как у всех шейдерных элементов parallax: непрозрачные области
    /// увели бы часть кадра мимо программы (см. шапку rounded.rs).
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

impl RenderElement<GlesRenderer> for Экранный {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        // Вторая текстура — на блок 1: блок 0 smithay занимает сам под `tex`
        // и выставляет его перед каждой отрисовкой. Снимаем за собой, иначе
        // чужая программа, читающая блок 1, увидела бы наш кадр.
        let id = self.свечение.as_ref().map(|t| t.tex_id()).unwrap_or(0);
        let _ = frame.with_context(|gl| unsafe {
            gl.ActiveTexture(ffi::TEXTURE1);
            gl.BindTexture(ffi::TEXTURE_2D, id);
            if id != 0 {
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
                gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            }
            gl.ActiveTexture(ffi::TEXTURE0);
        });
        frame.override_default_tex_program(self.программа.clone(), self.uniforms.clone());
        let r = RenderElement::<GlesRenderer>::draw(&self.inner, frame, src, dst, damage, opaque_regions, cache);
        frame.clear_tex_program_override();
        let _ = frame.with_context(|gl| unsafe {
            gl.ActiveTexture(ffi::TEXTURE1);
            gl.BindTexture(ffi::TEXTURE_2D, 0);
            gl.ActiveTexture(ffi::TEXTURE0);
        });
        r
    }

    /// `None` обязателен: с настоящим хранилищем элемент уехал бы на плоскость
    /// DRM мимо шейдера — и пользователь не увидел бы вообще ничего.
    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}

/// Экранный шейдер одного выхода: программа, офскрин-кадр и свой трекер.
pub struct Экран {
    /// Какой файл сейчас скомпилирован (пусто — ручка выключена).
    путь: String,
    /// Время правки файла: по нему шейдер перечитывается на лету, без
    /// перезапуска сеанса — правишь .frag, и экран меняется.
    правка: Option<SystemTime>,
    программа: Option<GlesTexProgram>,
    /// Файл уже пробовали и он не собрался — не пытаемся каждый кадр.
    отказ: bool,
    начало: Instant,
    цель: Option<GlesTexture>,
    размер: Size<i32, Physical>,
    трекер: Option<OutputDamageTracker>,
    id: Id,
    /// Шейдер читает `bloom` — значит, каждый кадр размываем.
    нужно_свечение: bool,
    блюр: Option<crate::blur::Блюр>,
    /// Программа размытия не собралась — не пробуем каждый кадр.
    блюр_отказ: bool,
}

/// Радиус размытия для `bloom` в пикселях экрана (на один проход; проходов
/// два, так что итоговый ореол примерно в полтора раза шире).
pub const РАДИУС_СВЕЧЕНИЯ: f32 = 40.0;

impl std::fmt::Debug for Экран {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Screen")
            .field("path", &self.путь)
            .field("size", &self.размер)
            .finish()
    }
}

impl Default for Экран {
    fn default() -> Self {
        Self::new()
    }
}

impl Экран {
    pub fn new() -> Self {
        Self {
            путь: String::new(),
            правка: None,
            программа: None,
            отказ: false,
            начало: Instant::now(),
            цель: None,
            размер: Size::from((0, 0)),
            трекер: None,
            id: Id::new(),
            нужно_свечение: false,
            блюр: None,
            блюр_отказ: false,
        }
    }

    /// Собрать кадр через пользовательский шейдер.
    ///
    /// `None` означает «шейдера в этом кадре нет» — вызывающий рисует сцену как
    /// обычно. Именно так выглядит и выключенная ручка, и любой отказ: пустая
    /// строка, нечитаемый файл, несобравшаяся программа, отсутствие памяти.
    pub fn кадр<E>(
        &mut self,
        renderer: &mut GlesRenderer,
        output: &Output,
        путь: &str,
        элементы: &[E],
        фон: [f32; 4],
    ) -> Option<Экранный>
    where
        E: RenderElement<GlesRenderer>,
    {
        let путь = путь.trim();
        if путь.is_empty() {
            self.выключить();
            return None;
        }
        self.перечитать(renderer, путь);
        let программа = self.программа.clone()?;

        let mode = output.current_mode()?;
        let экран = mode.size;
        if экран.w <= 0 || экран.h <= 0 {
            return None;
        }
        if self.цель.is_none() || self.размер != экран {
            let s = Size::<i32, BufferCoords>::from((экран.w, экран.h));
            let текстура = Offscreen::<GlesTexture>::create_buffer(renderer, Fourcc::Abgr8888, s)
                .map_err(|e| tracing::warn!("plx/screen: buffer {:?}: {:?}", s, e))
                .ok()?;
            self.цель = Some(текстура);
            self.размер = экран;
            // Трекер заводится вместе с буфером: его размер обязан совпадать с
            // буфером, иначе render_output выставит вьюпорт не по кадру.
            self.трекер = Some(OutputDamageTracker::new(
                экран,
                output.current_scale().fractional_scale(),
                output.current_transform(),
            ));
        }
        let mut цель = self.цель.clone()?;
        {
            let mut fb = renderer.bind(&mut цель).ok()?;
            let трекер = self.трекер.as_mut()?;
            // age = 0 — рисуем кадр целиком: буфер свой, и прошлого содержимого
            // у него для damage tracking всё равно нет.
            if let Err(e) = трекер.render_output(renderer, &mut fb, 0, элементы, фон) {
                tracing::warn!("plx/screen: render_output: {:?}", e);
                return None;
            }
        }

        let свечение = if self.нужно_свечение {
            self.свечение(renderer, &цель, экран)
        } else {
            None
        };

        let ctx = renderer.context_id();
        let текстура = self.цель.clone()?;
        let src = Rectangle::<f64, smithay::utils::Logical>::from_size(Size::from((
            текстура.width() as f64,
            текстура.height() as f64,
        )));
        let el = TextureRenderElement::from_static_texture(
            self.id.clone(),
            ctx,
            Point::<f64, Physical>::from((0.0, 0.0)),
            текстура,
            1,
            Transform::Normal,
            Some(1.0),
            Some(src),
            Some(Size::<i32, smithay::utils::Logical>::from((экран.w, экран.h))),
            None,
            Kind::Unspecified,
        );
        // Rescale/Relocate — нейтральные: они здесь только затем, чтобы тип
        // элемента совпал с объявленным в `Экранный` (и с тем, что уже умеет
        // кадр). Ничего не двигают и не масштабируют.
        let el = RescaleRenderElement::from_element(el, Point::from((0, 0)), 1.0);
        let el = RelocateRenderElement::from_element(el, Point::from((0, 0)), Relocate::Relative);
        let кадр = Rectangle::<i32, Physical>::new((0, 0).into(), экран);
        let inner = CropRenderElement::from_element(el, 1.0, кадр)?;

        Some(Экранный {
            inner,
            программа,
            uniforms: vec![
                Uniform::new("time", self.начало.elapsed().as_secs_f32()),
                Uniform::new("wl_output_resolution", [экран.w as f32, экран.h as f32]),
                Uniform::new("bloom", UniformValue::_1i(1)),
            ],
            свечение,
        })
    }

    /// Размытый кадр для `bloom`. Отказ не фатален: шейдер тогда читает пустой
    /// блок (чёрное), то есть свечения просто нет.
    fn свечение(
        &mut self,
        renderer: &mut GlesRenderer,
        кадр: &GlesTexture,
        экран: Size<i32, Physical>,
    ) -> Option<GlesTexture> {
        if self.блюр.is_none() && !self.блюр_отказ {
            self.блюр = crate::blur::Блюр::new(renderer);
            self.блюр_отказ = self.блюр.is_none();
        }
        let блюр = self.блюр.as_mut()?;
        блюр.размыть(renderer, кадр, crate::blur::Сцена::Плитки(Vec::new()), экран, РАДИУС_СВЕЧЕНИЯ)?;
        блюр.ещё_проход(renderer, РАДИУС_СВЕЧЕНИЯ)
    }

    /// Ручку выключили (или она пуста) — отпускаем всё, что держали.
    fn выключить(&mut self) {
        if self.программа.is_none() && self.цель.is_none() && self.путь.is_empty() {
            return;
        }
        self.путь.clear();
        self.правка = None;
        self.программа = None;
        self.отказ = false;
        self.цель = None;
        self.трекер = None;
        self.размер = Size::from((0, 0));
        self.нужно_свечение = false;
        self.блюр = None;
    }

    /// Перечитать и пересобрать программу, если поменялся путь или файл.
    fn перечитать(&mut self, renderer: &mut GlesRenderer, путь: &str) {
        let файл = развернуть(путь);
        let правка = std::fs::metadata(&файл).and_then(|m| m.modified()).ok();
        let тот_же = self.путь == путь && self.правка == правка;
        if тот_же && (self.программа.is_some() || self.отказ) {
            return;
        }
        self.путь = путь.to_string();
        self.правка = правка;
        self.программа = None;
        self.отказ = true;

        let текст = match std::fs::read_to_string(&файл) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("plx/screen: cannot read {:?}: {}", файл, e);
                return;
            }
        };
        let исходник = собрать(&текст);
        self.нужно_свечение = текст.contains("bloom");
        let uniforms = [
            UniformName::new("time", UniformType::_1f),
            UniformName::new("wl_output_resolution", UniformType::_2f),
            UniformName::new("bloom", UniformType::_1i),
        ];
        match renderer.compile_custom_texture_shader(исходник.as_str(), &uniforms) {
            Ok(p) => {
                tracing::info!("plx/screen: shader {:?} compiled", файл);
                self.программа = Some(p);
                self.отказ = false;
            }
            // Молчать нельзя: снаружи несобравшийся шейдер выглядит как «ручка
            // не работает», и искать причину пришлось бы гаданием.
            Err(e) => tracing::warn!("plx/screen: {:?} failed to compile: {:?}", файл, e),
        }
    }
}

/// `~/...` в путь от `$HOME`: в конфиге пишут с тильдой.
fn развернуть(путь: &str) -> PathBuf {
    match путь.strip_prefix('~') {
        Some(хвост) => PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(хвост.trim_start_matches('/')),
        None => PathBuf::from(путь),
    }
}

#[cfg(test)]
mod тесты {
    use super::*;

    #[test]
    fn чужой_шейдер_обвязывается() {
        let исходный = "#version 100\nprecision mediump float;\nvarying vec2 v_texcoord;\n\
                        uniform sampler2D tex;\nuniform float time;\n\
                        void main() { gl_FragColor = texture2D(tex, v_texcoord); }\n";
        let s = собрать(исходный);
        // Ровно один #version и ровно одно объявление каждого имени.
        assert_eq!(s.matches("#version").count(), 1);
        assert_eq!(s.matches("uniform sampler2D tex;").count(), 1);
        assert_eq!(s.matches("uniform float time;").count(), 1);
        assert_eq!(s.matches("varying vec2 v_coords;").count(), 1);
        // Пользовательский main переехал, наш зовёт его.
        assert!(s.contains("void plx_user_main()"));
        assert_eq!(s.matches("void main()").count(), 1);
        assert!(s.contains("plx_user_main();"));
    }

    #[test]
    fn объявления_различаются_по_имени() {
        // Своё имя рядом с нашим не режется: `tex_scale` — не `tex`.
        assert!(объявление("uniform sampler2D tex;", "uniform", "tex"));
        assert!(!объявление("uniform float tex_scale;", "uniform", "tex"));
        assert!(объявление("varying vec2 v_texcoord;", "varying", "v_texcoord"));
        assert!(!объявление("uniform float time;", "uniform", "tex"));
    }

    #[test]
    fn тильда_разворачивается() {
        unsafe { std::env::set_var("HOME", "/home/кто-то") };
        assert_eq!(развернуть("~/a.frag"), PathBuf::from("/home/кто-то/a.frag"));
        assert_eq!(развернуть("/etc/a.frag"), PathBuf::from("/etc/a.frag"));
    }
}
