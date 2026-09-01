//! Headless-бэкенд: тот же кадр, что уходит на монитор, но в PNG на диск.
//!
//! **Зачем.** Скругления, обрезка окон, блюр, панель, миникарта, обои — всё это
//! живёт ТОЛЬКО в DRM-пути (`udev::собрать_элементы`). Посмотреть на него можно
//! было единственным способом: занять монитор живого сеанса — то есть выгнать
//! человека из-за машины. Winit-бэкенд не годится: он рисует стоковым
//! `render_output` из smithay и ни скруглений, ни блюра не знает вовсе, поэтому
//! «проверил во вложенном dawn» про эти правки означало «не проверил ничего».
//!
//! **Как.** GLES поднимается на РЕНДЕР-узле (`/dev/dri/renderD128`) — ему не
//! нужен ни DRM master, ни VT, ни libseat: узел доступен всем на чтение-запись
//! (см. `udev::open_render_gbm`, там же объяснение). Выход придумывается сам
//! (`DAWN_HEADLESS_MODE`, по умолчанию 2560×1080@60), кадр собирается тем же
//! `собрать_элементы` и рисуется в offscreen через `screencopy::capture` — тот
//! самый путь, которым снимает экран демонстрация. Ввод — только через
//! управляющий сокет (`ctl.rs`): физической клавиатуры этот экземпляр не видит,
//! то есть чужой сеанс он тронуть не может ни при каких обстоятельствах.
//!
//! Запуск: `dawn --headless` (лучше со своим `XDG_RUNTIME_DIR`, чтобы сокет
//! не путался с живым). Снимок: `dawn-ctl shot /путь/кадр.png`.

use std::{os::unix::fs::OpenOptionsExt, time::Duration};

use smithay::{
    backend::{
        allocator::gbm::GbmDevice,
        drm::DrmDeviceFd,
        egl::{EGLContext, EGLDisplay},
        renderer::gles::GlesRenderer,
    },
    output::{Mode, Output, PhysicalProperties, Scale, Subpixel},
    reexports::calloop::{
        timer::{TimeoutAction, Timer},
        EventLoop,
    },
    utils::{DeviceFd, Transform},
};

use crate::Dawn;

/// Всё, что живёт между кадрами у ОДНОГО выхода: сам выход и его шейдеры.
///
/// Полный аналог `udev::Surface`, только без DRM-компоновщика: показывать
/// кадр некуда, он уходит в файл. Рендерер общий на все выходы — ровно как в
/// udev.rs, где один `GlesRenderer` на устройство обслуживает все его
/// поверхности.
struct Экран {
    output: Output,
    rounded: Option<crate::rounded::Шейдер>,
    blur: Option<crate::blur::Блюр>,
    last_elements: usize,
}

/// Разбирает один режим вида `2560x1080@60`. Мусор — `None`.
fn разобрать_режим(строка: &str) -> Option<(i32, i32, i32)> {
    let (размер, гц) = match строка.split_once('@') {
        Some((s, r)) => (s, r.trim().parse::<i32>().unwrap_or(60)),
        None => (строка, 60),
    };
    let (w, h) = размер.trim().split_once('x')?;
    match (w.trim().parse::<i32>(), h.trim().parse::<i32>()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Some((w, h, гц.max(1))),
        _ => None,
    }
}

/// Разбирает `DAWN_HEADLESS_MODE`: один режим `2560x1080@60` либо НЕСКОЛЬКО
/// через запятую — `2560x1080@60,1920x1280@60`. Каждый даёт свой выход, то
/// есть свой монитор со своей камерой и своим столом.
///
/// Многомониторность иначе нечем проверить: живой сеанс Ярика трогать нельзя,
/// а второй физический монитор не подключишь из скрипта. Мусор — молча на
/// умолчание: заваливать запуск харнесса из-за опечатки в переменной незачем.
fn режимы_из_окружения() -> Vec<(i32, i32, i32)> {
    let по_умолчанию = vec![(2560, 1080, 60)];
    let Ok(строка) = std::env::var("DAWN_HEADLESS_MODE") else {
        return по_умолчанию;
    };
    let список: Vec<_> = строка.split(',')
        .filter_map(|с| разобрать_режим(с.trim()))
        .collect();
    if список.is_empty() { по_умолчанию } else { список }
}

/// Разбирает `DAWN_HEADLESS_NAMES` — имена коннекторов, под которыми искать
/// `monitor{}` в config.lua, в том же порядке, что и режимы из
/// `DAWN_HEADLESS_MODE`. Без переменной (или при несовпадении числа) выход `n`
/// зовётся «headless-N+1», как и раньше, и ни один monitor{} под ним не
/// найдётся — раскладка и primary для него останутся по умолчанию.
///
/// **Зачем.** `monitor{ x=, y=, primary= }` — это решения человека по имени
/// РЕАЛЬНОГО коннектора (DP-2, HDMI-A-1). Без этой ручки конфиг из живого
/// сеанса в харнессе проверить нечем: DRM-путь даёт headless-1/headless-2 и
/// primary/раскладка не подключились бы вовсе.
fn имена_из_окружения(n: usize) -> Vec<String> {
    let по_умолчанию: Vec<String> = (0..n).map(|i| format!("headless-{}", i + 1)).collect();
    let Ok(строка) = std::env::var("DAWN_HEADLESS_NAMES") else {
        return по_умолчанию;
    };
    let список: Vec<String> = строка.split(',').map(|с| с.trim().to_string()).collect();
    if список.len() == n { список } else { по_умолчанию }
}

pub fn init_headless(
    event_loop: &mut EventLoop<'static, Dawn>,
    state: &mut Dawn,
) -> Result<(), Box<dyn std::error::Error>> {
    // Узел открываем НАПРЯМУЮ, а не через сессию: рендер-узел не имеет
    // отношения ни к seat'у, ни к DRM master (см. udev::open_render_gbm).
    let путь = std::env::var("DAWN_RENDER_NODE")
        .unwrap_or_else(|_| "/dev/dri/renderD128".to_string());
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC)
        .open(&путь)?;
    let gbm = GbmDevice::new(DrmDeviceFd::new(DeviceFd::from(std::os::fd::OwnedFd::from(file))))?;
    let egl = unsafe { EGLDisplay::new(gbm)? };
    let ctx = EGLContext::new(&egl)?;
    let mut gles = unsafe { GlesRenderer::new(ctx)? };
    tracing::info!("dawn/headless: GLES на {}", путь);

    let режимы = режимы_из_окружения();
    let коннекторы = имена_из_окружения(режимы.len());
    let mut экраны: Vec<Экран> = Vec::new();
    for (n, (w, h, гц)) in режимы.iter().copied().enumerate() {
        let mode = Mode { size: (w, h).into(), refresh: гц * 1000 };
        let имя = format!("headless-{}", n + 1);
        // Имя коннектора для поиска monitor{} — «headless-N» без
        // DAWN_HEADLESS_NAMES, иначе то, что попросили (см. имена_из_окружения).
        let коннектор_имя = коннекторы[n].clone();
        let mon_cfg = state.lua_config.monitors.iter()
            .find(|m| m.name == коннектор_имя)
            .cloned();
        let output = Output::new(
            имя.clone(),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "dawn".into(),
                model: "headless".into(),
                serial_number: "Unknown".into(),
            },
        );
        let _global = output.create_global::<Dawn>(&state.display_handle);
        // Дом на холсте и место в раскладке — ровно как в udev.rs: позиция
        // выхода в space это КАМЕРА, а «где монитор стоит» живёт отдельно.
        let дом = state.свободный_дом();
        let раскладка = match mon_cfg.as_ref().filter(|c| c.layout_set) {
            Some(c) => (c.x, c.y).into(),
            None => state.авто_раскладка(),
        };
        // wl_output.geometry — РАСКЛАДКА, не дом: см. подробный разбор в
        // udev.rs у той же строки (Xwayland берёт эту позицию как физическую
        // и строит по ней RandR root, а `дом` для второго монитора превышает
        // 16-битный диапазон CRTC x/y и заворачивается по модулю 65536).
        output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some(раскладка),
        );
        output.set_preferred(mode);
        state.space.map_output(&output, дом);

        // Отдельный выход только под layer-поверхности — как в udev.rs (там же
        // объяснение: масштаб слоёв обязан быть 1 независимо от зума холста).
        let layer_output = Output::new(
            format!("{}-layers", имя),
            PhysicalProperties {
                size: (0, 0).into(),
                subpixel: Subpixel::Unknown,
                make: "dawn".into(),
                model: "headless".into(),
                serial_number: "Unknown".into(),
            },
        );
        layer_output.change_current_state(
            Some(mode),
            Some(Transform::Normal),
            Some(Scale::Integer(1)),
            Some((0, 0).into()),
        );
        layer_output.set_preferred(mode);

        let тег = state.свободный_тег(mon_cfg.as_ref().map(|c| c.tag).unwrap_or(0));
        let mut вид = crate::state::Viewport::default();
        вид.cam_x = дом.x as f64;
        вид.cam_y = дом.y as f64;
        вид.tagset = [тег, тег];
        let индекс = n;
        state.мониторы.push(crate::monitors::Монитор {
            output: output.clone(),
            layer_output: layer_output.clone(),
            коннектор: коннектор_имя.clone(),
            размер: (w, h).into(),
            раскладка,
            дом,
            viewport: вид,
            обои: crate::monitors::СлайдОбоев::новый(crate::monitors::стол_обоев(тег)),
        });
        state.закрепить_стол(тег, n);
        state.visited_tags.insert(тег);
        state.tag_cameras.insert(тег, (дом.x as f64, дом.y as f64, 1.0));
        if n == 0 {
            state.активный = 0;
            state.курсор_монитор = 0;
            state.viewport = вид;
            state.layer_output = Some(layer_output.clone());
            state.pointer_location = smithay::utils::Point::from((
                дом.x as f64 + w as f64 / 2.0,
                дом.y as f64 + h as f64 / 2.0,
            ));
            state.pointer_warped();
        }
        // `monitor{ primary = true }` — тот же приём, что и в udev::add_surface
        // (см. пояснение там): монитор, увиденный не первым, но помеченный
        // основным, забирает активность и курсор себе.
        if n != 0 && mon_cfg.as_ref().is_some_and(|c| c.primary) {
            state.активировать_монитор(индекс);
            state.pointer_location = smithay::utils::Point::from((
                дом.x as f64 + w as f64 / 2.0,
                дом.y as f64 + h as f64 / 2.0,
            ));
            state.pointer_warped();
        }
        tracing::info!(
            "dawn/headless: выход {} ({}) {}x{}@{}Hz стол {:#b} дом ({},{}) раскладка ({},{})",
            имя, коннектор_имя, w, h, гц, тег, дом.x, дом.y, раскладка.x, раскладка.y,
        );

        экраны.push(Экран {
            rounded: crate::rounded::Шейдер::new(&mut gles),
            blur: crate::blur::Блюр::new(&mut gles),
            output,
            last_elements: 0,
        });
    }
    if state.blur_shape.is_none() {
        state.blur_shape = crate::rounded::Шейдер::new(&mut gles);
    }
    state.apply_camera_all();

    // Такт: рассылка frame callback'ов клиентам (без неё клиент рисует один
    // кадр и замирает навсегда — он ждёт ответа на wl_surface.frame) и снимок,
    // если его попросили. Сцену КАЖДЫЙ такт не собираем: показывать её некуда,
    // а GPU на машине общий с живым сеансом.
    let timer = Timer::from_duration(Duration::from_millis(16));
    event_loop.handle().insert_source(timer, move |_, _, state: &mut Dawn| {
        такт(&mut экраны, &mut gles, state);
        TimeoutAction::ToDuration(Duration::from_millis(16))
    })?;

    Ok(())
}

fn такт(экраны: &mut [Экран], gles: &mut GlesRenderer, state: &mut Dawn) {
    // Шлем: рендерер здесь — единственный на весь headless, и другого места,
    // откуда его видно, нет. Пока VR не просили, это одна проверка Option
    // (см. vr::тик_с).
    crate::vr::тик_с(state, gles);
    // Minecraft: тот же рендерер и та же причина — другого места, откуда его
    // видно, в headless нет. Благодаря этому вся dawn-сторона dmine
    // проверяется харнессом без самой игры.
    crate::mine::тик_с(state, gles);

    if let Some(путь) = state.shot_request.take() {
        снимок(экраны, gles, state, &путь);
    }

    // Мультиюзер: кадры гостям. На живом железе их шлёт `render_surface` после
    // кадра на монитор, а здесь монитора нет вовсе — поэтому прямо из такта.
    // Частоту держит сам кодировщик (`Кодировщик::пора`), а не этот таймер.
    if state.раздача_идёт() {
        for экран in экраны.iter_mut() {
            crate::share::render::кадры_гостям(
                state, gles, &экран.output.clone(), экран.rounded.as_ref(),
            );
        }
    }

    // Frame callbacks — окнам и слоям, ровно как в winit.rs/udev.rs.
    let теперь = state.start_time.elapsed();
    let окна: Vec<_> = state.space.elements().cloned().collect();
    for экран in экраны.iter() {
        let выход = экран.output.clone();
        for window in &окна {
            window.send_frame(&выход, теперь, Some(Duration::ZERO), |_, _| {
                Some(выход.clone())
            });
        }
        // Слои спрашиваем у карты ЭТОГО выхода: обои второго монитора живут
        // в своей карте, и без этого они не получили бы ни одного frame
        // callback — то есть застряли бы на первом кадре навсегда.
        let слои = state.слои_выход(&выход);
        let map = smithay::desktop::layer_map_for_output(&слои);
        for layer_surface in map.layers() {
            layer_surface.send_frame(&выход, теперь, Some(Duration::ZERO), |_, _| {
                Some(выход.clone())
            });
        }
    }
    let _ = state.display_handle.flush_clients();
}

/// Собирает кадр ТЕМ ЖЕ кодом, что и монитор, и кладёт его в PNG.
///
/// Выходов может быть несколько (`DAWN_HEADLESS_MODE` со списком режимов):
/// первый пишется в запрошенный путь, остальные — рядом с суффиксом `-2`,
/// `-3`. Так один `shot` даёт снимок ВСЕХ мониторов разом, и сравнивать их
/// между собой можно, не гоняя харнесс дважды.
fn снимок(экраны: &mut [Экран], gles: &mut GlesRenderer, state: &mut Dawn, путь: &std::path::Path) {
    state.sync_pointer_to_camera();
    for (n, экран) in экраны.iter_mut().enumerate() {
        let Some(mode) = экран.output.current_mode() else {
            tracing::warn!("dawn/headless: снимок без режима выхода");
            continue;
        };
        // Точка зрения СВОЕГО монитора: своя камера, свой зум, свой стол —
        // ровно то же, что делает render_surface на живом железе.
        let свой = state.монитор_по_выходу(&экран.output);
        let вернуть = свой.and_then(|i| state.войти_в_монитор(i));
        crate::udev::пересчитать_блюр(state, gles, экран.blur.as_mut(), &экран.output);
        let (elements, курсорных) = crate::udev::собрать_элементы(
            state,
            gles,
            &экран.output,
            экран.rounded.as_ref(),
            экран.last_elements,
        );
        экран.last_elements = elements.len();
        // Ждущий снимок области (PrtScr, snip.rs) обслуживается ровно здесь:
        // у харнесса другого «после кадра» нет — кадры он рисует только по
        // команде `shot`. Курсорных элементов у собранного кадра столько же,
        // сколько на живом железе, но резать их не нужно: в headless курсора
        // нет вовсе (`курсорных` = 0), а с ним снимок сравнивать не с чем.
        crate::snip::serve(state, &экран.output.clone(), gles, &elements[курсорных..]);
        let size = (mode.size.w, mode.size.h).into();
        let пиксели = crate::screencopy::capture(gles, &экран.output, &elements, size);
        state.покинуть_монитор(вернуть);

        let Some(пиксели) = пиксели else {
            tracing::warn!("dawn/headless: кадр {} не снялся", n + 1);
            continue;
        };
        let файл = if n == 0 { путь.to_path_buf() } else { с_суффиксом(путь, n + 1) };
        match записать_png(&файл, &пиксели, mode.size.w as u32, mode.size.h as u32) {
            Ok(()) => tracing::info!(
                "dawn/headless: снимок {:?} ({} элементов)", файл, экран.last_elements
            ),
            Err(e) => tracing::warn!("dawn/headless: PNG {:?}: {}", файл, e),
        }
    }
}

/// `/tmp/кадр.png` + 2 → `/tmp/кадр-2.png`.
fn с_суффиксом(путь: &std::path::Path, n: usize) -> std::path::PathBuf {
    let основа = путь.file_stem().map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "кадр".into());
    let расш = путь.extension().map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    путь.with_file_name(format!("{}-{}{}", основа, n, расш))
}

/// `capture` отдаёт Argb8888 — в памяти это B,G,R,A; PNG хочет R,G,B,A.
fn записать_png(
    путь: &std::path::Path,
    пиксели: &[u8],
    w: u32,
    h: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut rgba = пиксели.to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let файл = std::fs::File::create(путь)?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(файл), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&rgba)?;
    Ok(())
}
