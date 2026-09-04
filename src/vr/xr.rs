//! Сессия OpenXR: подключение к шлему, кадровый цикл, позы глаз.
//!
//! **Что здесь есть и чего нет.** Здесь — только разговор с рантаймом:
//! инстанс, система, сессия в нашем EGL-контексте, swapchain'ы, ожидание кадра,
//! позы глаз и режим смешивания с реальностью. Ни одного пикселя тут не
//! рисуется (это `render.rs`), и ни одна панель не расставляется (`scene.rs`).
//!
//! **Как это включается.** Никак не автоматически. `Шлем::подключиться`
//! зовётся по действию `vr_toggle` (Super+Alt+V по умолчанию) или ключом
//! командной строки `--vr`. Пока никто не попросил, parallax даже не грузит
//! `libopenxr_loader.so`: VR — гость в этом композиторе, а не условие работы.
//!
//! **Три состояния, которые важно не перепутать.**
//!
//! · сессии нет вовсе — `Parallax::vr == None`, parallax обычный композитор;
//! · сессия есть, но рантайм ещё не сказал «можно рисовать» (`IDLE`/`READY`):
//!   кадры не отдаём, но событийный цикл крутим — иначе не дождёмся `READY`;
//! · сессия идёт (`SYNCHRONIZED`/`VISIBLE`/`FOCUSED`) — рисуем каждый кадр.
//!
//! Состояние держит сам рантайм, мы его только зеркалим в `состояние`.
//!
//! **Почему кадр ждём в главном потоке.** `xrWaitFrame` блокирует до момента,
//! когда рантайм готов принять следующий кадр (у Quest 3 через WiVRn это
//! ~11 мс при 90 Гц). Отдельный рендер-поток потребовал бы второго
//! EGL-контекста и переноса текстур окон между контекстами — то есть ровно той
//! сложности, от которой мы ушли, взяв `XR_MNDX_egl_enable` (см. `egl.rs`).
//! Вместо этого главный цикл в VR-режиме крутится «обслужить клиентов не
//! блокируясь → отдать кадр шлему», и ожидание кадра играет роль vblank'а.

use std::ffi::c_void;

use openxr as xr;
use smithay::backend::renderer::gles::GlesRenderer;

use super::egl::{Egl, СозданиеСессии};
use super::math::{Век3, Кватернион, Поза, Углы};

/// Стереопара — две проекции, левая и правая.
pub const ВИД: xr::ViewConfigurationType = xr::ViewConfigurationType::PRIMARY_STEREO;

/// Формат образов swapchain'а.
///
/// `GL_SRGB8_ALPHA8`. Именно sRGB, а не линейный `GL_RGBA8`: рантайм
/// подмешивает наш кадр к реальному миру и сам работает в линейном
/// пространстве, поэтому преобразование обязано случиться в выборке текстуры —
/// иначе картинка уезжает в темноту. Альфа нужна для passthrough: там, где мы
/// не нарисовали ничего, должна быть видна комната.
const GL_SRGB8_ALPHA8: u32 = 0x8C43;
const GL_RGBA8: u32 = 0x8058;

/// Один глаз: свой swapchain, свой размер, свои готовые текстуры.
pub struct Глаз {
    pub swapchain: xr::Swapchain<Egl>,
    /// GL-имена текстур образов. Индексируются тем, что вернул `acquire_image`.
    pub образы: Vec<u32>,
    pub ширина: u32,
    pub высота: u32,
}

/// Живая сессия со шлемом.
pub struct Шлем {
    /// Держим `Entry` живым: в нём загруженная библиотека загрузчика, и её
    /// выгрузка из-под работающего инстанса — сегфолт.
    _entry: xr::Entry,
    pub instance: xr::Instance,
    pub system: xr::SystemId,
    pub session: xr::Session<Egl>,
    ожидание: xr::FrameWaiter,
    поток: xr::FrameStream<Egl>,
    /// Опорное пространство. `STAGE` — пол комнаты (у Quest он выставлен
    /// границей охраны), это и есть «выделенная шлему зона» из просьбы Ярика.
    pub сцена: xr::Space,
    /// Пространство головы — им целится взгляд и к нему привязан «экран перед
    /// лицом», когда человек хочет носить окна с собой.
    pub голова: xr::Space,
    pub глаза: [Глаз; 2],
    /// Как кадр смешивается с реальностью: OPAQUE — VR, ALPHA_BLEND —
    /// дополненная реальность (passthrough Quest 3).
    pub смешивание: xr::EnvironmentBlendMode,
    /// Что рантайм умеет: из этого списка выбирается `смешивание`.
    pub умеет_смешивать: Vec<xr::EnvironmentBlendMode>,
    pub состояние: xr::SessionState,
    /// Сессия начата (`xrBeginSession`) и ещё не закончена.
    идёт: bool,
    /// Ввод: наборы действий и пространства контроллеров (см. `input.rs`).
    pub ввод: Option<super::input::Ввод>,
    /// Последняя предсказанная метка времени — по ней локализуются пространства.
    pub время: xr::Time,
    /// Частота кадров, о которой отчитался рантайм (для лога и статистики).
    pub гц: f32,
    кадров: u64,
}

/// Поза и углы обзора одного глаза на конкретном кадре.
#[derive(Debug, Clone, Copy)]
pub struct ВидГлаза {
    pub поза: Поза,
    pub углы: Углы,
}

/// Что вернуло ожидание кадра.
pub enum Кадр {
    /// Рисовать; внутри — время показа и виды обоих глаз.
    Рисуем { время: xr::Time, виды: [ВидГлаза; 2] },
    /// Рантайм просит кадр пропустить (шлем снят, сессия свернута). Кадр всё
    /// равно обязан быть закрыт — это делает сам `дождаться_кадра`.
    Пропуск,
    /// Сессия ещё не идёт: ждём `READY`.
    Спим,
}

/// Всё, что удалось спросить у рантайма ДО того, как понадобился GL-контекст:
/// инстанс, система, режимы смешивания и размеры видов.
///
/// **Зачем этот раздел вообще существует.** Под WiVRn `xrCreateInstance` — это
/// не «создать объект», а «дождаться шлема»: клиент Monado соединяется с
/// `wivrn-server` по unix-сокету и висит в `recvmsg` до тех пор, пока к серверу
/// не подключится живой Quest. Замер 31.08.2026: главный поток parallax стоял в
/// `ipc_client_setup_shm` минутами, весь композитор был мёртв (ни курсора, ни
/// ctl, ни кадров) — ровно то, что Ярик увидел как «нажал Super+Alt+V и всё
/// зависло». Никакого таймаута у этого ожидания нет и быть не может: оно и есть
/// штатный способ WiVRn сказать «шлема пока нет».
///
/// Поэтому всё блокирующее собрано здесь и вызывается ИЗ ОТДЕЛЬНОГО ПОТОКА
/// (см. `vr::тик_с`), а на главном потоке остаётся только то, что требует
/// живого EGL-контекста, — `Шлем::поднять`. Граница проведена ровно по этому
/// признаку: `Заготовка` не знает про рендерер, `поднять` не делает ни одного
/// вызова, способного заснуть на неопределённый срок.
pub struct Заготовка {
    entry: xr::Entry,
    instance: xr::Instance,
    system: xr::SystemId,
    умеет_смешивать: Vec<xr::EnvironmentBlendMode>,
    виды: Vec<xr::ViewConfigurationView>,
}

impl Заготовка {
    /// Спросить рантайм обо всём, что можно спросить без графики.
    ///
    /// **Блокирующая.** Звать только из потока, которому не жалко заснуть
    /// навсегда: под WiVRn возврата не будет, пока не подключится шлем.
    pub fn нащупать() -> Result<Заготовка, Ошибка> {
        // `load` небезопасен по одной причине: он делает dlopen чужой библиотеки,
        // и та вправе выполнить свой код инициализации. Ровно то же самое делает
        // любой драйвер GL, который parallax уже грузит.
        let entry = unsafe { xr::Entry::load() }
            .map_err(|e| Ошибка::Загрузчик(format!("{e:?}")))?;
        let доступные = entry
            .enumerate_extensions()
            .map_err(|e| Ошибка::Xr("extension list", e))?;

        // ── Расширения ──────────────────────────────────────────────────────
        // `mndx_egl_enable` — то, чем мы вообще живём (см. egl.rs). Без него
        // сессию в GLES-контексте на Linux не создать, и это НЕ мягкий отказ:
        // дальше идти незачем, лучше честно сказать человеку.
        if !доступные.mndx_egl_enable {
            return Err(Ошибка::НетEgl);
        }
        let mut просим = xr::ExtensionSet::default();
        просим.mndx_egl_enable = true;
        просим.khr_opengl_es_enable = доступные.khr_opengl_es_enable;
        // Passthrough Quest'а. Просим, только если рантайм умеет: на голом
        // Monado с симулятором его нет, и требовать его значило бы не дать
        // проверить VR-режим вовсе.
        просим.fb_passthrough = доступные.fb_passthrough;
        просим.ext_hand_tracking = доступные.ext_hand_tracking;
        просим.fb_display_refresh_rate = доступные.fb_display_refresh_rate;

        let инфо = xr::ApplicationInfo {
            application_name: "parallax",
            application_version: 1,
            engine_name: "parallax",
            engine_version: 1,
            api_version: xr::Version::new(1, 0, 0),
        };
        let instance = entry
            .create_instance(&инфо, &просим, &[])
            .map_err(|e| Ошибка::Xr("instance creation", e))?;
        let свойства = instance
            .properties()
            .map_err(|e| Ошибка::Xr("runtime properties", e))?;
        tracing::info!(
            "plx/vr: runtime {} {}",
            свойства.runtime_name,
            свойства.runtime_version
        );

        let system = instance
            .system(xr::FormFactor::HEAD_MOUNTED_DISPLAY)
            .map_err(|e| Ошибка::Xr("headset lookup", e))?;

        let умеет_смешивать = instance
            .enumerate_environment_blend_modes(system, ВИД)
            .map_err(|e| Ошибка::Xr("blend modes", e))?;

        // Требования обязаны быть спрошены до создания сессии (см. egl.rs).
        let треб = <Egl as xr::Graphics>::requirements(&instance, system)
            .map_err(|e| Ошибка::Xr("graphics requirements", e))?;
        tracing::info!(
            "plx/vr: the runtime wants GLES {}..{}",
            треб.мин_версия,
            треб.макс_версия
        );

        let виды = instance
            .enumerate_view_configuration_views(system, ВИД)
            .map_err(|e| Ошибка::Xr("view configuration", e))?;
        if виды.len() < 2 {
            return Err(Ошибка::Странно("the runtime returned fewer than two views"));
        }

        Ok(Заготовка { entry, instance, system, умеет_смешивать, виды })
    }
}

impl Шлем {
    /// Спросить рантайм и сразу поднять сессию — как было до разделения.
    ///
    /// Годится там, где блокировка безобидна: в тестах и в headless-прогоне без
    /// WiVRn. Живой вход в VR идёт через `Заготовка::нащупать` в отдельном
    /// потоке и `Шлем::поднять` на главном (см. `vr::тик_с`).
    pub fn подключиться(renderer: &mut GlesRenderer) -> Result<Шлем, Ошибка> {
        Шлем::поднять(Заготовка::нащупать()?, renderer)
    }

    /// Поднять сессию поверх контекста, в котором рисует parallax.
    ///
    /// `renderer` нужен ровно за одним: у него берутся EGLDisplay, EGLConfig и
    /// EGLContext. Ничего в нём не меняется.
    ///
    /// Зовётся с ГЛАВНОГО потока — и только он: EGL-контекст parallax привязан к
    /// нему, `xrCreateSession` обязан видеть его текущим. Всё, что могло
    /// заснуть надолго, к этому моменту уже сделано в `Заготовка::нащупать`.
    pub fn поднять(заготовка: Заготовка, renderer: &mut GlesRenderer) -> Result<Шлем, Ошибка> {
        let Заготовка { entry, instance, system, умеет_смешивать, виды } = заготовка;
        // По умолчанию — обычный VR. AR включается отдельно (`vr_ar`), потому
        // что «окна поверх комнаты» и «окна в пустоте» — разные рабочие режимы,
        // и выбирать за человека нечестно.
        let смешивание = xr::EnvironmentBlendMode::OPAQUE;

        // ── Сессия в НАШЕМ контексте ────────────────────────────────────────
        let (display, config, context) = ручки_egl(renderer);
        let (session, ожидание, поток) = unsafe {
            instance.create_session::<Egl>(
                system,
                &СозданиеСессии { display, config, context },
            )
        }
        .map_err(|e| Ошибка::Xr("session creation", e))?;

        let сцена = session
            .create_reference_space(xr::ReferenceSpaceType::STAGE, xr::Posef::IDENTITY)
            .or_else(|_| {
                // У шлема без охраняемой зоны (или у симулятора) STAGE может не
                // быть — тогда LOCAL, начало координат в точке старта.
                tracing::warn!("plx/vr: STAGE unavailable, falling back to LOCAL");
                session.create_reference_space(xr::ReferenceSpaceType::LOCAL, xr::Posef::IDENTITY)
            })
            .map_err(|e| Ошибка::Xr("reference space", e))?;
        let голова = session
            .create_reference_space(xr::ReferenceSpaceType::VIEW, xr::Posef::IDENTITY)
            .map_err(|e| Ошибка::Xr("head space", e))?;

        // ── Swapchain на каждый глаз ────────────────────────────────────────
        let форматы = session
            .enumerate_swapchain_formats()
            .map_err(|e| Ошибка::Xr("swapchain formats", e))?;
        let формат = if форматы.contains(&GL_SRGB8_ALPHA8) {
            GL_SRGB8_ALPHA8
        } else if форматы.contains(&GL_RGBA8) {
            GL_RGBA8
        } else {
            // Берём первый предложенный: спецификация обещает, что список
            // непуст и отсортирован по предпочтению рантайма.
            *форматы.first().ok_or(Ошибка::Странно("the runtime returned an empty swapchain format list"))?
        };
        tracing::info!(
            "plx/vr: swapchain format 0x{:X} (sRGB={})",
            формат,
            формат == GL_SRGB8_ALPHA8
        );

        let создать_глаз = |i: usize| -> Result<Глаз, Ошибка> {
            let в = &виды[i];
            let (ш, вы) = (
                в.recommended_image_rect_width,
                в.recommended_image_rect_height,
            );
            let swapchain = session
                .create_swapchain(&xr::SwapchainCreateInfo {
                    create_flags: xr::SwapchainCreateFlags::EMPTY,
                    usage_flags: xr::SwapchainUsageFlags::COLOR_ATTACHMENT
                        | xr::SwapchainUsageFlags::SAMPLED,
                    format: формат,
                    sample_count: 1,
                    width: ш,
                    height: вы,
                    face_count: 1,
                    array_size: 1,
                    mip_count: 1,
                })
                .map_err(|e| Ошибка::Xr("swapchain creation", e))?;
            let образы = swapchain
                .enumerate_images()
                .map_err(|e| Ошибка::Xr("swapchain images", e))?;
            tracing::info!("plx/vr: eye {} — {}×{}, {} swapchain images", i, ш, вы, образы.len());
            Ok(Глаз { swapchain, образы, ширина: ш, высота: вы })
        };
        let глаза = [создать_глаз(0)?, создать_глаз(1)?];

        let mut шлем = Шлем {
            _entry: entry,
            instance,
            system,
            session,
            ожидание,
            поток,
            сцена,
            голова,
            глаза,
            смешивание,
            умеет_смешивать,
            состояние: xr::SessionState::IDLE,
            идёт: false,
            ввод: None,
            время: xr::Time::from_nanos(0),
            гц: 0.0,
            кадров: 0,
        };
        // Ввод поднимаем сразу: привязки действий обязаны быть предложены ДО
        // xrAttachSessionActionSets, а он делается один раз на сессию.
        match super::input::Ввод::поднять(&шлем.instance, &шлем.session) {
            Ok(в) => шлем.ввод = Some(в),
            Err(e) => tracing::warn!("plx/vr: input did not start ({e:?}) — mouse and keyboard remain"),
        }
        Ok(шлем)
    }

    /// Разобрать события рантайма. Возвращает `false`, если сессия кончилась и
    /// VR-режим надо снять.
    pub fn события(&mut self) -> bool {
        let mut буфер = xr::EventDataBuffer::new();
        while let Some(событие) = self
            .instance
            .poll_event(&mut буфер)
            .unwrap_or_else(|e| {
                tracing::warn!("plx/vr: poll_event: {e}");
                None
            })
        {
            use xr::Event::*;
            match событие {
                SessionStateChanged(e) => {
                    self.состояние = e.state();
                    tracing::info!("plx/vr: session state → {:?}", self.состояние);
                    match self.состояние {
                        xr::SessionState::READY => {
                            if let Err(e) = self.session.begin(ВИД) {
                                tracing::error!("plx/vr: begin_session: {e}");
                                return false;
                            }
                            self.идёт = true;
                        }
                        xr::SessionState::STOPPING => {
                            if let Err(e) = self.session.end() {
                                tracing::error!("plx/vr: end_session: {e}");
                            }
                            self.идёт = false;
                        }
                        xr::SessionState::EXITING | xr::SessionState::LOSS_PENDING => {
                            return false;
                        }
                        _ => {}
                    }
                }
                InstanceLossPending(_) => {
                    tracing::warn!("plx/vr: the runtime is going away");
                    return false;
                }
                InteractionProfileChanged(_) => {
                    if let Some(ввод) = &mut self.ввод {
                        ввод.профиль_сменился(&self.instance, &self.session);
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// Дождаться своей очереди на кадр и узнать, куда смотрят глаза.
    ///
    /// Обязательно парная штука: после `Рисуем`/`Пропуск` кадр ДОЛЖЕН быть
    /// закрыт (`отдать_кадр` или `закрыть_пустой`), иначе рантайм на следующем
    /// `xrWaitFrame` вернёт ошибку и сессия развалится.
    pub fn дождаться_кадра(&mut self) -> Кадр {
        if !self.идёт {
            return Кадр::Спим;
        }
        let состояние = match self.ожидание.wait() {
            Ok(с) => с,
            Err(e) => {
                tracing::warn!("plx/vr: wait_frame: {e}");
                return Кадр::Спим;
            }
        };
        self.время = состояние.predicted_display_time;
        if let Err(e) = self.поток.begin() {
            tracing::warn!("plx/vr: begin_frame: {e}");
            return Кадр::Спим;
        }
        if !состояние.should_render {
            return Кадр::Пропуск;
        }
        let (флаги, виды) = match self.session.locate_views(ВИД, self.время, &self.сцена) {
            Ok(в) => в,
            Err(e) => {
                tracing::warn!("plx/vr: locate_views: {e}");
                return Кадр::Пропуск;
            }
        };
        // Без обоих флагов позиция и/или ориентация — мусор. Рисовать по мусору
        // хуже, чем не рисовать: это мгновенная дурнота у человека в шлеме.
        let годно = флаги.contains(xr::ViewStateFlags::ORIENTATION_VALID)
            && флаги.contains(xr::ViewStateFlags::POSITION_VALID);
        if !годно || виды.len() < 2 {
            return Кадр::Пропуск;
        }
        self.кадров += 1;
        Кадр::Рисуем {
            время: self.время,
            виды: [вид_глаза(&виды[0]), вид_глаза(&виды[1])],
        }
    }

    /// Закрыть кадр, ничего не показав (пропуск или ошибка отрисовки).
    pub fn закрыть_пустой(&mut self) {
        let пусто: [&xr::CompositionLayerBase<Egl>; 0] = [];
        if let Err(e) = self.поток.end(self.время, self.смешивание, &пусто) {
            tracing::warn!("plx/vr: end_frame (empty): {e}");
        }
    }

    /// Показать нарисованную стереопару.
    ///
    /// Виды передаются те же, что вернуло `дождаться_кадра`: рантайм сверяет
    /// позу слоя с той, по которой мы рисовали, и при расхождении делает
    /// репроекцию. Соврать здесь — значит получить «плавающий» мир.
    pub fn отдать_кадр(&mut self, виды: [ВидГлаза; 2]) {
        let прямоуг = |г: &Глаз| xr::Rect2Di {
            offset: xr::Offset2Di { x: 0, y: 0 },
            extent: xr::Extent2Di {
                width: г.ширина as i32,
                height: г.высота as i32,
            },
        };
        let вид_слоя = |i: usize| -> xr::CompositionLayerProjectionView<Egl> {
            xr::CompositionLayerProjectionView::new()
                .pose(поза_в_xr(виды[i].поза))
                .fov(углы_в_xr(виды[i].углы))
                .sub_image(
                    xr::SwapchainSubImage::new()
                        .swapchain(&self.глаза[i].swapchain)
                        .image_array_index(0)
                        .image_rect(прямоуг(&self.глаза[i])),
                )
        };
        let слои = [вид_слоя(0), вид_слоя(1)];
        let mut проекция = xr::CompositionLayerProjection::new()
            .space(&self.сцена)
            .views(&слои);
        // БЕЗ ЭТОГО ФЛАГА AR НЕ РАБОТАЕТ ВООБЩЕ. По спецификации рантайм
        // игнорирует альфу нашей текстуры и считает её единицей, пока слою не
        // сказано обратное, — то есть прозрачный фон, который рисует
        // `render.rs`, становится непрозрачным чёрным, и комнаты за ним не
        // видно. Причём `xrEndFrame` при этом отвечает успехом: ни ошибки, ни
        // предупреждения, просто чёрный экран вместо passthrough.
        //
        // `UNPREMULTIPLIED_ALPHA` не ставим намеренно: parallax рисует
        // премультиплицированным цветом (`glBlendFunc(ONE,
        // ONE_MINUS_SRC_ALPHA)`), а этот флаг сказал бы рантайму обратное.
        if self.ар_включена() {
            проекция = проекция
                .layer_flags(xr::CompositionLayerFlags::BLEND_TEXTURE_SOURCE_ALPHA);
        }
        if let Err(e) = self
            .поток
            .end(self.время, self.смешивание, &[&proj_as_base(&проекция)])
        {
            tracing::warn!("plx/vr: end_frame: {e}");
        }
    }

    /// Переключить дополненную реальность.
    ///
    /// Возвращает, что получилось на самом деле: рантайм без passthrough
    /// (Monado с симулятором, шлем без камер) остаётся в OPAQUE, и врать об
    /// этом в панели нельзя.
    pub fn дополненная(&mut self, включить: bool) -> bool {
        let хочу = if включить {
            // ALPHA_BLEND — «наш кадр поверх комнаты по альфе», ровно то, что
            // делает passthrough Quest 3 через WiVRn. ADDITIVE — режим
            // полупрозрачных очков (HoloLens): чёрное становится прозрачным.
            // Берём то, что рантайм согласен показать.
            [
                xr::EnvironmentBlendMode::ALPHA_BLEND,
                xr::EnvironmentBlendMode::ADDITIVE,
            ]
            .into_iter()
            .find(|м| self.умеет_смешивать.contains(м))
        } else {
            Some(xr::EnvironmentBlendMode::OPAQUE)
        };
        match хочу {
            Some(м) => {
                self.смешивание = м;
                м != xr::EnvironmentBlendMode::OPAQUE
            }
            None => {
                tracing::warn!("plx/vr: the runtime cannot blend with reality: {:?}", self.умеет_смешивать);
                false
            }
        }
    }

    pub fn ар_включена(&self) -> bool {
        self.смешивание != xr::EnvironmentBlendMode::OPAQUE
    }

    pub fn кадров(&self) -> u64 {
        self.кадров
    }

    /// Режимы смешивания, которые объявил рантайм, — человеческой строкой.
    ///
    /// Измеритель: «AR не работает» бывает двух совершенно разных родов —
    /// рантайм не объявил `ALPHA_BLEND` (нечего включать) или объявил, но
    /// картинка всё равно непрозрачная (наша вина). Различить их иначе нечем.
    pub fn смешивание_строкой(&self) -> String {
        let имя = |м: &xr::EnvironmentBlendMode| match *м {
            xr::EnvironmentBlendMode::OPAQUE => "OPAQUE".to_string(),
            xr::EnvironmentBlendMode::ADDITIVE => "ADDITIVE".to_string(),
            xr::EnvironmentBlendMode::ALPHA_BLEND => "ALPHA_BLEND".to_string(),
            иное => format!("{iное:?}", iное = иное),
        };
        crate::тф!(
            "сейчас {}, умеет [{}]", "now {}, supports [{}]",
            имя(&self.смешивание),
            self.умеет_смешивать
                .iter()
                .map(имя)
                .collect::<Vec<_>>()
                .join(", "),
        )
    }
}

/// Достать из рендерера ручки EGL, на которых он работает.
fn ручки_egl(renderer: &mut GlesRenderer) -> (*mut c_void, *mut c_void, *mut c_void) {
    let ctx = renderer.egl_context();
    let display = ctx.display().get_display_handle().handle as *mut c_void;
    let config = ctx.config_id() as *mut c_void;
    let context = ctx.get_context_handle() as *mut c_void;
    (display, config, context)
}

fn вид_глаза(в: &xr::View) -> ВидГлаза {
    ВидГлаза {
        поза: Поза {
            место: Век3::new(в.pose.position.x, в.pose.position.y, в.pose.position.z),
            поворот: Кватернион::new(
                в.pose.orientation.x,
                в.pose.orientation.y,
                в.pose.orientation.z,
                в.pose.orientation.w,
            ),
        },
        углы: Углы {
            левый: в.fov.angle_left,
            правый: в.fov.angle_right,
            верхний: в.fov.angle_up,
            нижний: в.fov.angle_down,
        },
    }
}

pub fn поза_в_xr(п: Поза) -> xr::Posef {
    xr::Posef {
        orientation: xr::Quaternionf {
            x: п.поворот.x,
            y: п.поворот.y,
            z: п.поворот.z,
            w: п.поворот.w,
        },
        position: xr::Vector3f {
            x: п.место.x,
            y: п.место.y,
            z: п.место.z,
        },
    }
}

pub fn поза_из_xr(п: xr::Posef) -> Поза {
    Поза {
        место: Век3::new(п.position.x, п.position.y, п.position.z),
        поворот: Кватернион::new(
            п.orientation.x,
            п.orientation.y,
            п.orientation.z,
            п.orientation.w,
        ),
    }
}

fn углы_в_xr(уг: Углы) -> xr::Fovf {
    xr::Fovf {
        angle_left: уг.левый,
        angle_right: уг.правый,
        angle_up: уг.верхний,
        angle_down: уг.нижний,
    }
}

/// Слой проекции как «базовый слой» — того требует сигнатура `end`.
///
/// Приведение безопасно по построению: `XrCompositionLayerProjection`
/// начинается с тех же полей, что и `XrCompositionLayerBaseHeader`, и
/// спецификация прямо разрешает передавать указатель на него.
fn proj_as_base<'a>(
    p: &'a xr::CompositionLayerProjection<'a, Egl>,
) -> &'a xr::CompositionLayerBase<'a, Egl> {
    unsafe { std::mem::transmute(p) }
}

/// Почему VR не поднялся. Разделено не для красоты: каждая причина требует
/// РАЗНОГО действия от человека, и панель показывает именно её.
#[derive(Debug)]
pub enum Ошибка {
    /// Загрузчик OpenXR не нашёлся или не открылся — ставить `openxr` и рантайм.
    Загрузчик(String),
    /// Рантайм есть, но без `XR_MNDX_egl_enable` — не Monado/WiVRn.
    НетEgl,
    /// Ошибка самого OpenXR с местом, где она случилась.
    Xr(&'static str, xr::sys::Result),
    Странно(&'static str),
}

impl Ошибка {
    /// Значит ли эта ошибка «шлема пока нет, но он может появиться».
    ///
    /// Ровно два кода: рантайм есть, но не отдаёт систему `HEAD_MOUNTED_DISPLAY`
    /// (WiVRn отвечает так, пока Quest не подключился к серверу), и рантайм
    /// вовсе недоступен (сервер ещё поднимается). Всё остальное — настоящий
    /// отказ, и ждать его бессмысленно: ни отсутствие `XR_MNDX_egl_enable`, ни
    /// не открывшийся загрузчик сами собой не исправятся.
    pub fn подождать_ли(&self) -> bool {
        matches!(
            self,
            Ошибка::Xr(_, код)
                if *код == xr::sys::Result::ERROR_FORM_FACTOR_UNAVAILABLE
                    || *код == xr::sys::Result::ERROR_RUNTIME_UNAVAILABLE
        )
    }
}

impl std::fmt::Display for Ошибка {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ошибка::Загрузчик(e) => write!(
                f,
                "the OpenXR loader did not open ({e}); the openxr package and a runtime (WiVRn or Monado) are required"
            ),
            Ошибка::НетEgl => write!(
                f,
                "the runtime lacks XR_MNDX_egl_enable: parallax draws in GLES/EGL and can only hand a frame to such a runtime (WiVRn, Monado)"
            ),
            Ошибка::Xr(где, e) => write!(f, "{где}: {e}"),
            Ошибка::Странно(с) => write!(f, "{с}"),
        }
    }
}
