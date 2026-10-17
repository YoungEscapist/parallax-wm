//! Полотна-dmabuf: окно едет в Minecraft, не покидая видеопамяти.
//!
//! **Зачем.** Старый путь (он остался запасным, см. `mod.rs`) рисовал окно в
//! текстуру, вычитывал её `copy_framebuffer` + `map_texture` — с остановкой
//! GPU на каждый кусок, — гнал сырые RGBA по сокету, а Java копировала их ещё
//! раз и заливала обратно в видеокарту. Окно 2560×1080 с видео — 11 МиБ на
//! кадр туда и обратно через процессор. Так устроен образец, Hypr3D: снимок
//! окна у него никогда не покидает GPU, потому что движок живёт в самом
//! композиторе. У нас движок — чужой процесс (Minecraft), и ближе всего к
//! этому — общий буфер: полотно окна САМО является dmabuf, его дескриптор
//! один раз уходит моду, мод импортирует его в свой GL через EGL и рисует
//! панель прямо из него.
//!
//! **Почему линейный буфер в одну плоскость.** Плиточный буфер с компрессией
//! (на Intel это CCS, у него вторая плоскость под служебные данные) пришлось
//! бы описывать моду по плоскостям и надеяться, что его EGL примет тот же
//! модификатор. Линейный принимает любой драйвер, и он же читается
//! процессором, если импорт не удастся. Цена — чуть медленнее отрисовка в него,
//! на окнах это незаметно.
//!
//! **Синхронизация — неявная.** Пишет parallax, читает Minecraft, оба через
//! один и тот же драйвер (Mesa): ядро само упорядочивает доступ к dmabuf по
//! заборам (dma-resv). От нас требуется одно — чтобы команды отрисовки были
//! отданы драйверу (`glFlush`), прежде чем мод узнает про новый кадр.

use std::collections::HashMap;
use std::os::fd::OwnedFd;

use smithay::backend::allocator::dmabuf::{AsDmabuf, Dmabuf};
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::{Allocator, Buffer, Fourcc, Modifier};
use smithay::backend::drm::DrmDeviceFd;
use smithay::backend::egl::EGLDevice;
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::Bind;
use smithay::desktop::Window;
use smithay::utils::{DeviceFd, Physical, Point, Scale, Size, Transform};

use crate::vr::scene::{Ключ, ОкноВСцене};

/// Полотно одного окна в виде dmabuf.
pub struct ПолотноДма {
    pub буфер: Dmabuf,
    pub размер: (i32, i32),
    следы: OutputDamageTracker,
}

/// Чем заводятся полотна. Заводится лениво на первом кадре режима: узел
/// рендера берём у того EGL, которым рисует parallax, — так буфер гарантированно
/// живёт на той же видеокарте, что и отрисовка в него.
pub struct Дма {
    выделитель: GbmAllocator<DrmDeviceFd>,
    pub полотна: HashMap<Ключ, ПолотноДма>,
}

/// Что вышло с полотном за кадр.
pub enum Итог {
    /// Полотно заведено заново и в него целиком нарисован первый кадр: моду
    /// надо отдать новый буфер (дескриптор + описание).
    Новое,
    /// Клиент нарисовал что-то новое — буфер у мода тот же, содержимое уже в нём.
    Кадр,
    /// Ничего не поменялось.
    Тишина,
    /// Не вышло (нет памяти, bind не удался). Окно поедет старым путём.
    Мимо,
}

impl Дма {
    pub fn завести(renderer: &GlesRenderer) -> Result<Self, String> {
        let устройство = EGLDevice::device_for_display(renderer.egl_context().display())
            .map_err(|e| format!("EGL device: {e:?}"))?;
        let узел = устройство
            .try_get_render_node()
            .map_err(|e| format!("render node: {e:?}"))?
            .ok_or_else(|| "EGL device has no render node".to_string())?;
        let путь = узел.dev_path().ok_or_else(|| "render node has no path".to_string())?;
        let файл = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&путь)
            .map_err(|e| format!("{}: {e}", путь.display()))?;
        let gbm = GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(OwnedFd::from(файл))))
            .map_err(|e| format!("gbm: {e}"))?;
        tracing::info!("plx/mine: dmabuf canvases on {}", путь.display());
        Ok(Self {
            выделитель: GbmAllocator::new(gbm, GbmBufferFlags::RENDERING | GbmBufferFlags::LINEAR),
            полотна: HashMap::new(),
        })
    }

    /// Перерисовать окно в его полотно.
    ///
    /// Тот же расчёт, что у `vr::нарисовать_в_полотно` (минус `geometry().loc`,
    /// трекер повреждений), только целью служит dmabuf. Новое полотно
    /// рисуется ЦЕЛИКОМ (возраст 0) — мод получит его уже готовым.
    pub fn нарисовать(&mut self, renderer: &mut GlesRenderer, окно: &Window, о: ОкноВСцене) -> Итог {
        let размер = (о.ширина_пкс.max(1), о.высота_пкс.max(1));
        let свежее = self.полотна.get(&о.ключ).is_none_or(|п| п.размер != размер);
        if свежее {
            let буфер = match self
                .выделитель
                .create_buffer(размер.0 as u32, размер.1 as u32, Fourcc::Abgr8888, &[Modifier::Linear])
                .map_err(|e| e.to_string())
                .and_then(|bo| bo.export().map_err(|e| e.to_string()))
            {
                Ok(б) => б,
                Err(e) => {
                    tracing::warn!("plx/mine: dmabuf {}×{}: {e}", размер.0, размер.1);
                    return Итог::Мимо;
                }
            };
            if буфер.num_planes() != 1 {
                tracing::warn!("plx/mine: dmabuf has {} planes — the mod takes one", буфер.num_planes());
                return Итог::Мимо;
            }
            // Старое полотно (если было) уходит здесь же. Мод держит СВОЮ
            // ссылку на прежний буфер и показывает его, пока не придёт новый, —
            // память освободится, когда отпустят оба.
            self.полотна.insert(
                о.ключ,
                ПолотноДма {
                    буфер,
                    размер,
                    следы: OutputDamageTracker::new(
                        Size::<i32, Physical>::from(размер),
                        1.0,
                        Transform::Normal,
                    ),
                },
            );
        }
        let сдвиг = окно.geometry().loc;
        let элементы: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = окно.render_elements(
            renderer,
            Point::<i32, Physical>::from((-сдвиг.x, -сдвиг.y)),
            Scale::from(1.0),
            1.0,
        );
        let Some(полотно) = self.полотна.get_mut(&о.ключ) else { return Итог::Мимо };
        let mut цель = полотно.буфер.clone();
        let повреждено = {
            let mut fb = match renderer.bind(&mut цель) {
                Ok(fb) => fb,
                Err(e) => {
                    tracing::warn!("plx/mine: dmabuf bind: {e:?}");
                    self.полотна.remove(&о.ключ);
                    return Итог::Мимо;
                }
            };
            let возраст = if свежее { 0 } else { 1 };
            match полотно
                .следы
                .render_output(renderer, &mut fb, возраст, &элементы, [0.0, 0.0, 0.0, 0.0])
            {
                Ok(итог) => итог.damage.is_some_and(|d| !d.is_empty()),
                Err(e) => {
                    tracing::warn!("plx/mine: dmabuf canvas: {e:?}");
                    return Итог::Мимо;
                }
            }
        };
        if !свежее && !повреждено {
            return Итог::Тишина;
        }
        // Команды — драйверу, иначе мод может прочесть буфер раньше, чем в
        // него что-то попало: неявная синхронизация видит только отданное.
        let _ = renderer.with_context(|gl| unsafe { gl.Flush() });
        if свежее { Итог::Новое } else { Итог::Кадр }
    }

    /// Описание и дескриптор полотна — для `ОтДавна::Буфер`.
    pub fn отдать(&self, ключ: Ключ) -> Option<(super::proto::ОтДавна, OwnedFd)> {
        let п = self.полотна.get(&ключ)?;
        let б = &п.буфер;
        let fd = б.handles().next()?.try_clone_to_owned().ok()?;
        let смещение = б.offsets().next()?;
        let шаг = б.strides().next()?;
        let формат = б.format();
        Some((
            super::proto::ОтДавна::Буфер {
                ключ,
                ш: п.размер.0.clamp(1, u16::MAX as i32) as u16,
                в: п.размер.1.clamp(1, u16::MAX as i32) as u16,
                fourcc: формат.code as u32,
                // GBM с флагом LINEAR заводит буфер старым вызовом и сообщает
                // модификатор «неявный» (замер на харнессе: 0x00ff_ffff_ffff_ffff).
                // Раскладка при этом ЛИНЕЙНАЯ — флаг это гарантирует, — и
                // говорим моду прямо: его EGL не должен гадать по неявному.
                модификатор: match формат.modifier {
                    Modifier::Invalid => u64::from(Modifier::Linear),
                    м => u64::from(м),
                },
                смещение,
                шаг,
            },
            fd,
        ))
    }
}
