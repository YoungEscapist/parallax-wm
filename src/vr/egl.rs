//! Привязка OpenXR к тому самому EGL-контексту, в котором parallax уже рисует.
//!
//! **Зачем отдельный файл на полсотни строк обвязки.** Крейт `openxr` умеет
//! четыре графических API, и ни один из них не подходит буквально:
//! `OpenGlEs` собирает `XrGraphicsBindingOpenGLESAndroidKHR` и существует
//! ТОЛЬКО под Android (`#[cfg(target_os = "android")]` прямо в крейте), а
//! `OpenGL` требует настольный GLX/WGL-контекст. У parallax же весь рендер идёт
//! через smithay `GlesRenderer` — это GLES 3 поверх EGL на Linux.
//!
//! Ровно для этого случая Monado (а значит и WiVRn, который на нём построен)
//! держит своё расширение `XR_MNDX_egl_enable`: сессия создаётся с
//! `XrGraphicsBindingEGLMNDX`, в котором лежат наши EGLDisplay, EGLConfig и
//! EGLContext. Рантайм после этого работает В НАШЕМ контексте, и текстуры
//! swapchain'а — это обычные GLES-текстуры, которые можно навесить на FBO и
//! рисовать в них тем же кодом, что и всё остальное в parallax. Никакого
//! копирования кадра между контекстами, никакого dmabuf-моста.
//!
//! Тип `Egl` — реализация `openxr::Graphics` снаружи крейта. Так делать можно:
//! трейт публичный, а его методы помечены `#[doc(hidden)]` только потому, что
//! они деталь реализации для пользователей крейта, а не запрет.
//!
//! **Грабля, ради которой здесь `eglGetProcAddress` из dlsym.** Рантайм просит
//! у нас указатель на `eglGetProcAddress`, чтобы самому дотянуться до функций
//! GL. Брать его из smithay нельзя — там он спрятан за приватным `ffi`, — а
//! линковаться с libEGL напрямую parallax не хочет: EGL приходит с драйвером и
//! грузится динамически. Поэтому `dlopen` уже загруженной библиотеки (RTLD_
//! NOLOAD не нужен: она заведомо в адресном пространстве, dlopen лишь поднимет
//! счётчик) и `dlsym`.

use std::ffi::{c_char, c_void, CString};
use std::ptr;

use openxr::sys;
// `NULL` у ручек OpenXR — константа трейта `Handle`, без него не видна.
use openxr::sys::Handle as _;

/// Графический API «наш EGL-контекст».
pub enum Egl {}

/// То, что рантайм считает требованиями к версии GL ES. Держим как есть: parallax
/// работает на GLES 3.0+, и ни один известный рантайм не просит больше.
#[derive(Copy, Clone, Debug)]
pub struct Требования {
    pub мин_версия: openxr::Version,
    pub макс_версия: openxr::Version,
}

/// Что нужно, чтобы создать сессию: три ручки нашего EGL.
///
/// Все три берутся у `GlesRenderer` (см. `xr.rs`), а не создаются заново —
/// в этом весь смысл: сессия обязана жить в контексте, где лежат текстуры окон.
#[derive(Copy, Clone, Debug)]
pub struct СозданиеСессии {
    pub display: *mut c_void,
    pub config: *mut c_void,
    pub context: *mut c_void,
}

fn проверить(x: sys::Result) -> openxr::Result<sys::Result> {
    if x.into_raw() >= 0 { Ok(x) } else { Err(x) }
}

/// Указатель на `eglGetProcAddress` процесса.
///
/// Рантайм требует его непустым (`XrGraphicsBindingEGLMNDX::getProcAddress
/// cannot be NULL` — ровно эта ошибка и была первой живой на симуляторе
/// Monado): через него он сам достаёт функции GL, чтобы работать в нашем
/// контексте.
///
/// **Почему не хватило `dlopen(NULL)`.** Хэндл главной программы ищет символ в
/// ГЛОБАЛЬНОЙ области видимости, а libEGL приезжает в процесс не линковкой, а
/// `dlopen`'ом изнутри GLVND/smithay — то есть с `RTLD_LOCAL`, и снаружи её
/// символы не видны. Поэтому открываем библиотеку по имени: сначала
/// vendor-neutral `libEGL.so.1` (GLVND, он же у NVIDIA), потом её же без
/// версии, и только затем — глобальную область как последний шанс.
fn eglgetprocaddress() -> Option<unsafe extern "system" fn(*const c_char) -> Option<unsafe extern "system" fn()>> {
    unsafe {
        let имя = CString::new("eglGetProcAddress").ok()?;
        let попытка = |путь: Option<&str>| -> *mut c_void {
            let хэндл = match путь {
                Some(п) => match CString::new(п) {
                    Ok(c) => libc::dlopen(c.as_ptr(), libc::RTLD_LAZY | libc::RTLD_LOCAL),
                    Err(_) => return ptr::null_mut(),
                },
                None => libc::dlopen(ptr::null(), libc::RTLD_LAZY),
            };
            if хэндл.is_null() {
                return ptr::null_mut();
            }
            libc::dlsym(хэндл, имя.as_ptr())
        };
        let s = [Some("libEGL.so.1"), Some("libEGL.so"), None]
            .into_iter()
            .map(попытка)
            .find(|s| !s.is_null())?;
        Some(std::mem::transmute::<
            *mut c_void,
            unsafe extern "system" fn(*const c_char) -> Option<unsafe extern "system" fn()>,
        >(s))
    }
}

impl openxr::Graphics for Egl {
    type Requirements = Требования;
    type SessionCreateInfo = СозданиеСессии;
    /// Формат — GL-константа (`GL_SRGB8_ALPHA8` и подобные).
    type Format = u32;
    /// Образ swapchain'а — имя GLES-текстуры.
    type SwapchainImage = u32;

    fn raise_format(x: i64) -> u32 {
        x as _
    }

    fn lower_format(x: u32) -> i64 {
        x.into()
    }

    /// Спецификация требует спросить требования до создания сессии, иначе
    /// рантайм вправе отказать (`XR_ERROR_GRAPHICS_REQUIREMENTS_CALL_MISSING`).
    /// Спрашиваем через `XR_KHR_opengl_es_enable`: `XR_MNDX_egl_enable` своих
    /// требований не заводит и опирается на него же.
    fn requirements(
        instance: &openxr::Instance,
        system: openxr::SystemId,
    ) -> openxr::Result<Требования> {
        let Some(ext) = instance.exts().khr_opengl_es_enable.as_ref() else {
            // Рантайм без KHR_opengl_es_enable до сюда не дойдёт: расширение
            // запрашивается при создании инстанса (см. xr.rs). Но если вдруг —
            // отвечаем «подходит любая», а не падаем.
            return Ok(Требования {
                мин_версия: openxr::Version::new(3, 0, 0),
                макс_версия: openxr::Version::new(3, 2, 0),
            });
        };
        unsafe {
            let mut треб = sys::GraphicsRequirementsOpenGLESKHR::out(ptr::null_mut());
            проверить((ext.get_open_gles_graphics_requirements)(
                instance.as_raw(),
                system,
                треб.as_mut_ptr(),
            ))?;
            let треб = треб.assume_init();
            Ok(Требования {
                мин_версия: треб.min_api_version_supported,
                макс_версия: треб.max_api_version_supported,
            })
        }
    }

    unsafe fn create_session(
        instance: &openxr::Instance,
        system: openxr::SystemId,
        info: &СозданиеСессии,
    ) -> openxr::Result<sys::Session> {
        let привязка = sys::GraphicsBindingEGLMNDX {
            ty: sys::GraphicsBindingEGLMNDX::TYPE,
            next: ptr::null(),
            get_proc_address: eglgetprocaddress(),
            display: info.display,
            config: info.config,
            context: info.context,
        };
        let создание = sys::SessionCreateInfo {
            ty: sys::SessionCreateInfo::TYPE,
            next: &привязка as *const _ as *const _,
            create_flags: Default::default(),
            system_id: system,
        };
        let mut сессия = sys::Session::NULL;
        проверить(unsafe {
            (instance.fp().create_session)(instance.as_raw(), &создание, &mut сессия)
        })?;
        Ok(сессия)
    }

    /// Двойной вызов «сколько — и дай»: первый узнаёт размер, второй заполняет.
    /// Свой, а не крейтовый `get_arr_init`, — тот приватный.
    fn enumerate_swapchain_images(
        swapchain: &openxr::Swapchain<Self>,
    ) -> openxr::Result<Vec<u32>> {
        unsafe {
            let фп = swapchain.instance().fp().enumerate_swapchain_images;
            let mut сколько = 0u32;
            проверить(фп(swapchain.as_raw(), 0, &mut сколько, ptr::null_mut()))?;
            let mut образы = vec![
                sys::SwapchainImageOpenGLESKHR {
                    ty: sys::SwapchainImageOpenGLESKHR::TYPE,
                    next: ptr::null_mut(),
                    image: 0,
                };
                сколько as usize
            ];
            let mut вышло = 0u32;
            проверить(фп(
                swapchain.as_raw(),
                сколько,
                &mut вышло,
                образы.as_mut_ptr() as *mut _,
            ))?;
            образы.truncate(вышло as usize);
            Ok(образы.into_iter().map(|о| о.image).collect())
        }
    }
}
