//! Отрисовка сцены в глаза шлема — сырым GL поверх контекста parallax.
//!
//! **Почему не через smithay.** Весь рендер parallax идёт `GlesFrame`'ом, а тот
//! умеет ровно одну проекцию — ортографическую «экран как есть». Для VR нужна
//! перспектива с матрицей на каждый глаз и глубина, поэтому здесь свой
//! маленький конвейер: одна программа на панели, одна на линии, один FBO,
//! один буфер глубины. Всё это живёт в ТОМ ЖЕ EGL-контексте (см. `egl.rs`),
//! поэтому текстуры окон, отрисованные обычным путём parallax, видны напрямую —
//! ни копирований, ни dmabuf-мостов.
//!
//! **Порядок в кадре одного глаза.**
//!
//! 1. привязать текстуру swapchain'а к FBO, включить глубину;
//! 2. очистить. В VR — чёрным, в AR — ПРОЗРАЧНЫМ: там, где мы ничего не
//!    нарисовали, рантайм показывает комнату (см. `xr.rs::дополненная`);
//! 3. панели: непрозрачные, с записью глубины;
//! 4. лучи контроллеров и рамка зоны: линиями, поверх, без записи глубины.
//!
//! **Что здесь легко сломать и как это увидеть.**
//!
//! · Забыть вернуть состояние GL — и следующий кадр монитора уедет (у smithay
//!   свои представления о текущей программе и буфере). Поэтому в конце
//!   `кадр_глаза` состояния возвращаются явно;
//! · перепутать направление V текстуры — картинка окна встанет вверх ногами.
//!   `smithay` рисует в offscreen с проекцией `flip180`, то есть строка 0
//!   текстуры — это ВЕРХ окна; шейдер это учитывает (см. `ВЕРШИННЫЙ`);
//! · рисовать панели без сортировки и без глубины — дальняя панель закрасит
//!   ближнюю. Глубина включена, но панели ещё и сортируются: полупрозрачные
//!   края (скругление) иначе смешиваются в неправильном порядке.

use smithay::backend::renderer::gles::{ffi, GlesRenderer};

use super::math::Мат4;

/// Панель, готовая к отрисовке.
pub struct КПоказу {
    /// Модельная матрица: поза панели и её размер.
    pub модель: Мат4,
    /// GL-имя текстуры с содержимым окна.
    pub текстура: u32,
    pub альфа: f32,
    /// Радиус скругления в долях полуширины (0 — прямые углы).
    pub скругление: f32,
    /// Подсветить рамкой (панель под указкой).
    pub выделена: bool,
}

/// Отрезок для вспомогательной графики: луч указки, рамка зоны, курсор.
pub struct Линия {
    pub от: [f32; 3],
    pub до: [f32; 3],
    pub цвет: [f32; 4],
}

// ВНИМАНИЕ: внутри шейдеров — только латиница. GLSL ES принимает лишь ASCII,
// и кириллическое имя переменной валит компиляцию с бесполезным
// «syntax error, unexpected $undefined» (замер 30.08.2026 на симуляторе
// Monado — на этом VR-режим не включался вовсе). Комментарии по-русски можно:
// их драйвер выбрасывает препроцессором… но и они пусть будут снаружи, в Rust.
const ВЕРШИННЫЙ: &str = r#"#version 100
attribute vec2 pos;
uniform mat4 mvp;
varying vec2 uv;
varying vec2 local;
void main() {
    // Текстура окна лежит «строка 0 — верх» (smithay рисует offscreen с
    // flip180), а панель по Y идёт снизу вверх. Отсюда 1.0 − t.
    uv = vec2((pos.x + 1.0) * 0.5, 1.0 - (pos.y + 1.0) * 0.5);
    local = pos;
    gl_Position = mvp * vec4(pos, 0.0, 1.0);
}
"#;

const ФРАГМЕНТНЫЙ: &str = r#"#version 100
precision mediump float;
uniform sampler2D tex;
uniform float alpha;
uniform float rounding;
uniform float hovered;
varying vec2 uv;
varying vec2 local;

void main() {
    vec4 c = texture2D(tex, uv);

    // Скругление углов — то же, что у окон на мониторе (см. rounded.rs), но
    // считается в координатах панели: -1..1 по обеим осям.
    if (rounding > 0.001) {
        vec2 a = abs(local) - (1.0 - rounding);
        if (a.x > 0.0 && a.y > 0.0) {
            float d = length(a) / rounding;
            // Мягкий край: жёсткое отсечение на таком масштабе видно
            // ступеньками, панель в шлеме крупная.
            c *= 1.0 - smoothstep(0.98, 1.02, d);
        }
    }

    // Рамка выделения: тонкая светлая кайма по краю панели под указкой.
    if (hovered > 0.5) {
        vec2 e = abs(local);
        float edge = max(e.x, e.y);
        if (edge > 0.985) {
            c = mix(c, vec4(0.62, 0.78, 1.0, 1.0), 0.85);
        }
    }

    gl_FragColor = c * alpha;
}
"#;

const ВЕРШИННЫЙ_ЛИНИЯ: &str = r#"#version 100
attribute vec3 pos;
uniform mat4 vp;
void main() { gl_Position = vp * vec4(pos, 1.0); }
"#;

const ФРАГМЕНТНЫЙ_ЛИНИЯ: &str = r#"#version 100
precision mediump float;
uniform vec4 color;
void main() { gl_FragColor = color; }
"#;

/// Скомпилированный конвейер. Живёт, пока идёт VR-сессия.
pub struct Рендер {
    панель: Программа,
    линия: Программа,
    квадрат: u32,
    отрезок: u32,
    fbo: u32,
    глубина: u32,
    размер_глубины: (i32, i32),
}

struct Программа {
    id: u32,
    /// Позиции uniform'ов: искать их по имени каждый кадр — это лишние вызовы
    /// драйвера на каждую панель.
    места: Vec<(&'static str, i32)>,
}

impl Программа {
    fn место(&self, имя: &str) -> i32 {
        self.места
            .iter()
            .find(|(и, _)| *и == имя)
            .map(|(_, м)| *м)
            .unwrap_or(-1)
    }
}

impl Рендер {
    /// Собрать программы и буферы. Зовётся один раз при входе в VR.
    pub fn собрать(renderer: &mut GlesRenderer) -> Result<Рендер, String> {
        renderer
            .with_context(|gl| unsafe { собрать_в_контексте(gl) })
            .map_err(|e| format!("the context is unavailable: {e:?}"))?
    }

    /// Отдать GL-ресурсы. Явно, а не в `Drop`: удалять объекты GL можно только
    /// в текущем контексте, а `Drop` про контекст ничего не знает.
    pub fn разобрать(self, renderer: &mut GlesRenderer) {
        let _ = renderer.with_context(|gl| unsafe {
            gl.DeleteProgram(self.панель.id);
            gl.DeleteProgram(self.линия.id);
            gl.DeleteBuffers(1, &self.квадрат);
            gl.DeleteBuffers(1, &self.отрезок);
            gl.DeleteFramebuffers(1, &self.fbo);
            if self.глубина != 0 {
                gl.DeleteRenderbuffers(1, &self.глубина);
            }
        });
    }

    /// Нарисовать один глаз в текстуру `цель`.
    ///
    /// `vp` — проекция × вид этого глаза; панели приходят с уже посчитанными
    /// модельными матрицами (сцена не знает про GL, а рендер — про метры).
    #[allow(clippy::too_many_arguments)]
    pub fn кадр_глаза(
        &mut self,
        renderer: &mut GlesRenderer,
        цель: u32,
        ширина: i32,
        высота: i32,
        vp: Мат4,
        панели: &[КПоказу],
        линии: &[Линия],
        прозрачный_фон: bool,
    ) -> Result<(), String> {
        // Буфер глубины держим ровно под размер глаза и пересоздаём только при
        // смене (у swapchain он постоянный, так что практически один раз).
        if self.размер_глубины != (ширина, высота) {
            let (fbo_глубина, ошибка) = renderer
                .with_context(|gl| unsafe { пересоздать_глубину(gl, self.глубина, ширина, высота) })
                .map_err(|e| format!("context: {e:?}"))?;
            if let Some(e) = ошибка {
                return Err(e);
            }
            self.глубина = fbo_глубина;
            self.размер_глубины = (ширина, высота);
        }

        let панель = &self.панель;
        let линия = &self.линия;
        let fbo = self.fbo;
        let глубина = self.глубина;
        let квадрат = self.квадрат;
        let отрезок = self.отрезок;

        renderer
            .with_context(|gl| unsafe {
                нарисовать(
                    gl, fbo, глубина, квадрат, отрезок, панель, линия, цель, ширина, высота, vp,
                    панели, линии, прозрачный_фон,
                )
            })
            .map_err(|e| format!("context: {e:?}"))?
    }
}

// ── Внутренности: всё, что трогает GL ───────────────────────────────────────

unsafe fn собрать_в_контексте(gl: &ffi::Gles2) -> Result<Рендер, String> {
    let панель = unsafe {
        программа(
            gl,
            ВЕРШИННЫЙ,
            ФРАГМЕНТНЫЙ,
            &["mvp", "tex", "alpha", "rounding", "hovered"],
            &["pos"],
        )?
    };
    let линия = unsafe {
        программа(gl, ВЕРШИННЫЙ_ЛИНИЯ, ФРАГМЕНТНЫЙ_ЛИНИЯ, &["vp", "color"], &["pos"])?
    };

    // Квадрат панели: две треугольные полосы, координаты −1..1.
    let вершины: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
    let mut квадрат = 0u32;
    unsafe {
        gl.GenBuffers(1, &mut квадрат);
        gl.BindBuffer(ffi::ARRAY_BUFFER, квадрат);
        gl.BufferData(
            ffi::ARRAY_BUFFER,
            std::mem::size_of_val(&вершины) as isize,
            вершины.as_ptr() as *const _,
            ffi::STATIC_DRAW,
        );
    }

    // Буфер под отрезки — перезаливается каждый кадр, их единицы.
    let mut отрезок = 0u32;
    unsafe {
        gl.GenBuffers(1, &mut отрезок);
    }

    let mut fbo = 0u32;
    unsafe {
        gl.GenFramebuffers(1, &mut fbo);
        gl.BindBuffer(ffi::ARRAY_BUFFER, 0);
    }

    Ok(Рендер {
        панель,
        линия,
        квадрат,
        отрезок,
        fbo,
        глубина: 0,
        размер_глубины: (0, 0),
    })
}

unsafe fn пересоздать_глубину(
    gl: &ffi::Gles2,
    старый: u32,
    ширина: i32,
    высота: i32,
) -> (u32, Option<String>) {
    unsafe {
        if старый != 0 {
            gl.DeleteRenderbuffers(1, &старый);
        }
        let mut rb = 0u32;
        gl.GenRenderbuffers(1, &mut rb);
        gl.BindRenderbuffer(ffi::RENDERBUFFER, rb);
        gl.RenderbufferStorage(ffi::RENDERBUFFER, ffi::DEPTH_COMPONENT16, ширина, высота);
        gl.BindRenderbuffer(ffi::RENDERBUFFER, 0);
        if rb == 0 {
            (0, Some("the depth buffer was not created".into()))
        } else {
            (rb, None)
        }
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn нарисовать(
    gl: &ffi::Gles2,
    fbo: u32,
    глубина: u32,
    квадрат: u32,
    отрезок: u32,
    панель: &Программа,
    линия: &Программа,
    цель: u32,
    ширина: i32,
    высота: i32,
    vp: Мат4,
    панели: &[КПоказу],
    линии: &[Линия],
    прозрачный_фон: bool,
) -> Result<(), String> {
    unsafe {
        // ── Что было до нас ─────────────────────────────────────────────────
        // smithay продолжит рисовать мониторы этим же контекстом, и оставить
        // ему чужие настройки — это чёрный экран на столе, самый дорогой из
        // возможных багов (см. заметку про блюр в blur.rs).
        let mut прежний_fbo = 0i32;
        let mut прежняя_программа = 0i32;
        let mut прежний_буфер = 0i32;
        let mut прежний_вьюпорт = [0i32; 4];
        gl.GetIntegerv(ffi::FRAMEBUFFER_BINDING, &mut прежний_fbo);
        gl.GetIntegerv(ffi::CURRENT_PROGRAM, &mut прежняя_программа);
        gl.GetIntegerv(ffi::ARRAY_BUFFER_BINDING, &mut прежний_буфер);
        gl.GetIntegerv(ffi::VIEWPORT, прежний_вьюпорт.as_mut_ptr());
        let прежний_тест_глубины = gl.IsEnabled(ffi::DEPTH_TEST) == ffi::TRUE;
        let прежний_блендинг = gl.IsEnabled(ffi::BLEND) == ffi::TRUE;

        // ── sRGB: не давать драйверу кодировать наш кадр второй раз ─────────
        //
        // Swapchain шлема — `GL_SRGB8_ALPHA8` (иначе рантайм затемняет
        // картинку при подмешивании к миру), а текстуры окон у нас уже
        // ЗАКОДИРОВАНЫ в sRGB: это обычные буферы клиентов, которые parallax
        // рисует один в один и на монитор. Записывая их в sRGB-таргет с
        // включённым `GL_FRAMEBUFFER_SRGB`, драйвер кодирует значения ЕЩЁ раз,
        // и тёмный фон терминала уезжает в серый — ровно это и было видно на
        // первом снимке глаза (30.08.2026, симулятор Monado): панель светлая,
        // текст еле различим.
        //
        // Константа — из `EXT_sRGB_write_control`; в списке расширений
        // NVIDIA она есть. Если драйвер её не знает, `Disable` просто выставит
        // GL_INVALID_ENUM, который мы тут же и съедаем: хуже, чем «чуть светлее
        // картинка», это не сделает.
        const FRAMEBUFFER_SRGB_EXT: u32 = 0x8DB9;
        let прежний_srgb = gl.IsEnabled(FRAMEBUFFER_SRGB_EXT) == ffi::TRUE;
        gl.Disable(FRAMEBUFFER_SRGB_EXT);
        while gl.GetError() != ffi::NO_ERROR {}

        gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
        gl.FramebufferTexture2D(
            ffi::FRAMEBUFFER,
            ffi::COLOR_ATTACHMENT0,
            ffi::TEXTURE_2D,
            цель,
            0,
        );
        gl.FramebufferRenderbuffer(
            ffi::FRAMEBUFFER,
            ffi::DEPTH_ATTACHMENT,
            ffi::RENDERBUFFER,
            глубина,
        );
        let статус = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
        if статус != ffi::FRAMEBUFFER_COMPLETE {
            gl.BindFramebuffer(ffi::FRAMEBUFFER, прежний_fbo as u32);
            return Err(format!("FBO is not complete: 0x{:X}", статус));
        }

        gl.Viewport(0, 0, ширина, высота);
        gl.Disable(ffi::SCISSOR_TEST);
        gl.Enable(ffi::DEPTH_TEST);
        gl.DepthFunc(ffi::LEQUAL);
        gl.DepthMask(ffi::TRUE);
        gl.Enable(ffi::BLEND);
        // Премультиплицированная альфа — та же договорённость, что и во всём
        // остальном parallax (см. заметку про pooled_solid): текстуры окон приходят
        // премультиплицированными, и SRC_ALPHA здесь дал бы светлую кайму.
        gl.BlendFunc(ffi::ONE, ffi::ONE_MINUS_SRC_ALPHA);

        // Фон. В AR прозрачный — сквозь него рантайм покажет комнату.
        if прозрачный_фон {
            gl.ClearColor(0.0, 0.0, 0.0, 0.0);
        } else {
            // Не чистый чёрный: в шлеме абсолютная чернота выглядит «дырой»,
            // и человек теряет ощущение пространства. Очень тёмный синий —
            // то же, чем parallax заливает пустой холст.
            gl.ClearColor(0.02, 0.02, 0.04, 1.0);
        }
        gl.Clear(ffi::COLOR_BUFFER_BIT | ffi::DEPTH_BUFFER_BIT);

        // ── Панели ──────────────────────────────────────────────────────────
        gl.UseProgram(панель.id);
        gl.BindBuffer(ffi::ARRAY_BUFFER, квадрат);
        let поз = 0; // атрибут привязан к нулю при линковке (см. `программа`)
        gl.EnableVertexAttribArray(поз);
        gl.VertexAttribPointer(поз, 2, ffi::FLOAT, ffi::FALSE, 0, std::ptr::null());
        gl.ActiveTexture(ffi::TEXTURE0);
        gl.Uniform1i(панель.место("tex"), 0);

        for п in панели {
            let mvp = vp.умножить(&п.модель);
            gl.UniformMatrix4fv(панель.место("mvp"), 1, ffi::FALSE, mvp.как_массив().as_ptr());
            gl.Uniform1f(панель.место("alpha"), п.альфа);
            gl.Uniform1f(панель.место("rounding"), п.скругление);
            gl.Uniform1f(панель.место("hovered"), if п.выделена { 1.0 } else { 0.0 });
            gl.BindTexture(ffi::TEXTURE_2D, п.текстура);
            // Линейная фильтрация: панель в шлеме почти всегда не пиксель в
            // пиксель, и ближайший сосед даёт рябь на тексте.
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MIN_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_MAG_FILTER, ffi::LINEAR as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_S, ffi::CLAMP_TO_EDGE as i32);
            gl.TexParameteri(ffi::TEXTURE_2D, ffi::TEXTURE_WRAP_T, ffi::CLAMP_TO_EDGE as i32);
            gl.DrawArrays(ffi::TRIANGLE_STRIP, 0, 4);
        }
        gl.DisableVertexAttribArray(поз);

        // ── Линии: лучи указок и рамка зоны ─────────────────────────────────
        if !линии.is_empty() {
            gl.UseProgram(линия.id);
            gl.UniformMatrix4fv(линия.место("vp"), 1, ffi::FALSE, vp.как_массив().as_ptr());
            gl.BindBuffer(ffi::ARRAY_BUFFER, отрезок);
            gl.EnableVertexAttribArray(0);
            gl.VertexAttribPointer(0, 3, ffi::FLOAT, ffi::FALSE, 0, std::ptr::null());
            // Луч не должен прятаться в панели, в которую упирается: пишем
            // цвет, но не глубину.
            gl.DepthMask(ffi::FALSE);
            for л in линии {
                let точки: [f32; 6] = [л.от[0], л.от[1], л.от[2], л.до[0], л.до[1], л.до[2]];
                gl.BufferData(
                    ffi::ARRAY_BUFFER,
                    std::mem::size_of_val(&точки) as isize,
                    точки.as_ptr() as *const _,
                    ffi::DYNAMIC_DRAW,
                );
                gl.Uniform4f(линия.место("color"), л.цвет[0], л.цвет[1], л.цвет[2], л.цвет[3]);
                gl.DrawArrays(ffi::LINES, 0, 2);
            }
            gl.DisableVertexAttribArray(0);
            gl.DepthMask(ffi::TRUE);
        }

        // ── Вернуть всё как было ────────────────────────────────────────────
        gl.BindTexture(ffi::TEXTURE_2D, 0);
        gl.BindBuffer(ffi::ARRAY_BUFFER, прежний_буфер as u32);
        gl.UseProgram(прежняя_программа as u32);
        gl.BindFramebuffer(ffi::FRAMEBUFFER, прежний_fbo as u32);
        gl.Viewport(
            прежний_вьюпорт[0],
            прежний_вьюпорт[1],
            прежний_вьюпорт[2],
            прежний_вьюпорт[3],
        );
        if !прежний_тест_глубины {
            gl.Disable(ffi::DEPTH_TEST);
        }
        if !прежний_блендинг {
            gl.Disable(ffi::BLEND);
        }
        if прежний_srgb {
            gl.Enable(FRAMEBUFFER_SRGB_EXT);
            while gl.GetError() != ffi::NO_ERROR {}
        }
        Ok(())
    }
}

/// Прочитать содержимое только что нарисованного глаза.
///
/// **Зачем это в композиторе.** Кадр, ушедший в шлем, снаружи не виден ничем:
/// ни `grim`, ни screencopy до него не достают — он живёт в swapchain'е
/// рантайма. Без этой функции единственным измерителем VR-режима был бы сам
/// шлем на голове, то есть проверять пришлось бы вслепую и только вдвоём с
/// человеком. С ней кадр глаза ложится в PNG по команде из сокета
/// (`vr shot`), и расстановку панелей видно так же обычно, как раскладку окон
/// на мониторе.
///
/// Возвращает RGBA, строка 0 — ВЕРХ кадра (glReadPixels отдаёт снизу вверх,
/// поэтому переворачиваем здесь же).
pub fn прочитать_глаз(
    renderer: &mut GlesRenderer,
    fbo: u32,
    цель: u32,
    ширина: i32,
    высота: i32,
) -> Result<Vec<u8>, String> {
    renderer
        .with_context(|gl| unsafe {
            let mut прежний = 0i32;
            gl.GetIntegerv(ffi::FRAMEBUFFER_BINDING, &mut прежний);
            gl.BindFramebuffer(ffi::FRAMEBUFFER, fbo);
            gl.FramebufferTexture2D(
                ffi::FRAMEBUFFER,
                ffi::COLOR_ATTACHMENT0,
                ffi::TEXTURE_2D,
                цель,
                0,
            );
            let статус = gl.CheckFramebufferStatus(ffi::FRAMEBUFFER);
            if статус != ffi::FRAMEBUFFER_COMPLETE {
                gl.BindFramebuffer(ffi::FRAMEBUFFER, прежний as u32);
                return Err(format!("FBO for the screenshot: 0x{:X}", статус));
            }
            let mut пиксели = vec![0u8; (ширина * высота * 4) as usize];
            gl.PixelStorei(ffi::PACK_ALIGNMENT, 1);
            gl.ReadPixels(
                0,
                0,
                ширина,
                высота,
                ffi::RGBA,
                ffi::UNSIGNED_BYTE,
                пиксели.as_mut_ptr() as *mut _,
            );
            gl.BindFramebuffer(ffi::FRAMEBUFFER, прежний as u32);
            // Переворот по вертикали: у GL начало координат внизу, у PNG вверху.
            let строка = (ширина * 4) as usize;
            let mut перевёрнутые = vec![0u8; пиксели.len()];
            for y in 0..высота as usize {
                let из = y * строка;
                let в = (высота as usize - 1 - y) * строка;
                перевёрнутые[в..в + строка].copy_from_slice(&пиксели[из..из + строка]);
            }
            Ok(перевёрнутые)
        })
        .map_err(|e| format!("context: {e:?}"))?
}

impl Рендер {
    /// FBO, в который рисуются глаза, — нужен снимку (см. `прочитать_глаз`).
    pub fn fbo(&self) -> u32 {
        self.fbo
    }
}

unsafe fn программа(
    gl: &ffi::Gles2,
    вершинный: &str,
    фрагментный: &str,
    uniform_имена: &[&'static str],
    атрибуты: &[&str],
) -> Result<Программа, String> {
    unsafe {
        let вш = шейдер(gl, ffi::VERTEX_SHADER, вершинный)?;
        let фш = шейдер(gl, ffi::FRAGMENT_SHADER, фрагментный)?;
        let p = gl.CreateProgram();
        gl.AttachShader(p, вш);
        gl.AttachShader(p, фш);
        // Атрибут привязываем к нулю ДО линковки: так не нужно спрашивать его
        // место и можно писать `0` в отрисовке.
        for (i, имя) in атрибуты.iter().enumerate() {
            let c = std::ffi::CString::new(*имя).map_err(|_| "attribute name".to_string())?;
            gl.BindAttribLocation(p, i as u32, c.as_ptr() as *const _);
        }
        gl.LinkProgram(p);
        let mut ок = 0i32;
        gl.GetProgramiv(p, ffi::LINK_STATUS, &mut ок);
        // Шейдеры после линковки живут внутри программы.
        gl.DeleteShader(вш);
        gl.DeleteShader(фш);
        if ок == 0 {
            let mut длина = 0i32;
            gl.GetProgramiv(p, ffi::INFO_LOG_LENGTH, &mut длина);
            let mut буфер = vec![0u8; длина.max(1) as usize];
            gl.GetProgramInfoLog(
                p,
                длина,
                std::ptr::null_mut(),
                буфер.as_mut_ptr() as *mut _,
            );
            gl.DeleteProgram(p);
            return Err(format!(
                "the program did not link: {}",
                String::from_utf8_lossy(&буфер)
            ));
        }
        let места = uniform_имена
            .iter()
            .map(|имя| {
                let c = std::ffi::CString::new(*имя).unwrap();
                (*имя, gl.GetUniformLocation(p, c.as_ptr() as *const _))
            })
            .collect();
        Ok(Программа { id: p, места })
    }
}

unsafe fn шейдер(gl: &ffi::Gles2, вид: u32, исходник: &str) -> Result<u32, String> {
    unsafe {
        let ш = gl.CreateShader(вид);
        let указатель = исходник.as_ptr() as *const _;
        let длина = исходник.len() as i32;
        gl.ShaderSource(ш, 1, &указатель, &длина);
        gl.CompileShader(ш);
        let mut ок = 0i32;
        gl.GetShaderiv(ш, ffi::COMPILE_STATUS, &mut ок);
        if ок == 0 {
            let mut длина = 0i32;
            gl.GetShaderiv(ш, ffi::INFO_LOG_LENGTH, &mut длина);
            let mut буфер = vec![0u8; длина.max(1) as usize];
            gl.GetShaderInfoLog(ш, длина, std::ptr::null_mut(), буфер.as_mut_ptr() as *mut _);
            gl.DeleteShader(ш);
            return Err(format!(
                "the shader did not compile: {}",
                String::from_utf8_lossy(&буфер)
            ));
        }
        Ok(ш)
    }
}
