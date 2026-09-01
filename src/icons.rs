//! Настоящие значки приложений: поиск по теме значков и разбор PNG.
//!
//! **Что было.** dawn рисовал БУКВУ в кружке везде, где нужен значок: и в трее
//! (приложение прислало только `IconName` — имя значка в теме, а не пиксели),
//! и на чипах окон в панели. Причина была записана прямо в коде: «читать PNG
//! из темы значило бы тащить в dawn распаковщик zlib». Ярик 24.08.2026:
//! «сделай, чтобы справа показывались именно иконки приложений» — значит,
//! распаковщик тащим. Крейт `png` (он же `miniz_oxide`) — чистый Rust, без C и
//! без системных библиотек, то есть сборка от него не усложняется.
//!
//! **Что тут неочевидно.**
//!
//! · Спецификация XDG про темы значков (`index.theme`, наследование,
//!   `Context=Applications`, точные каталоги размеров) здесь НЕ реализована, и
//!   это осознанно. Полный обход по спецификации — сотни строк ради того же
//!   результата: имя значка достаточно найти под каталогом темы, а нужный
//!   размер выбрать по ЧИСЛУ В ПУТИ (`.../48x48/apps/telegram.png`). Обход
//!   кэшируется и делается один раз на имя.
//!
//! · SVG читается тоже (жалоба Ярика 29.08.2026 «некоторые иконки
//!   отображаются буквами»). Прежде тут стояло «SVG не читается: это отдельный
//!   движок ради тем, которые почти всегда кладут рядом и PNG» — на этой
//!   машине оказалось наоборот. PNG нет НИ У ОДНОГО приложения GNOME
//!   (Nautilus, Настройки, Текстовый редактор, Калькулятор, Консоль, Loupe —
//!   у всех только `hicolor/scalable/apps/*.svg`), нет у Alacritty,
//!   pavucontrol, dshare, inir; Adwaita с 46-й версии растровых значков не
//!   поставляет вовсе, а Papirus на машине не стоит. Буква в чипе — это ровно
//!   те приложения. `resvg` без default-features (без шрифтов и без растровых
//!   вставок) — usvg + tiny-skia, чистый Rust, как и `png`.
//!
//! · SVG растеризуется СРАЗУ В НУЖНЫЙ РАЗМЕР, поэтому ужимать его нечем и
//!   незачем: он лучше любого PNG, кроме попавшего в размер точно. Отсюда и
//!   его место в `штраф` — сразу за точным попаданием.
//!   Не нашлось ни PNG, ни SVG — остаётся прежняя буква, и это честнее, чем
//!   пустое место.
//!
//! · Результат — уже ГОТОВЫЙ к отрисовке `sni::Icon`: premultiplied RGBA
//!   нужного размера. Ужимает та же `sni::fit_icon`, что и картинки с шины, —
//!   иначе значки из темы и значки от приложений выглядели бы по-разному.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::sni::Icon;

/// Где искать темы значков. Порядок — от «моего» к системному, как в
/// спецификации XDG: пользовательская тема обязана перебивать системную.
fn каталоги_тем() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(&home).join(".icons"));
        out.push(PathBuf::from(&home).join(".local/share/icons"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        out.push(PathBuf::from(d).join("icons"));
    }
    for p in профили_nix() {
        out.push(p.join("icons"));
    }
    out
}

/// Каталоги `share` профилей Nix.
///
/// Nix на не-NixOS (у Ярика Void) ставит приложения в свой профиль и добавляет
/// его `share` в `XDG_DATA_DIRS` только через `nix.sh` — то есть в
/// интерактивном шелле. Сессия композитора поднимается не из него, и в живом
/// dawn 29.08.2026 `XDG_DATA_DIRS` был ровно flatpak + /usr: значок AyuGram
/// лежал в `/nix/store/…-profile/share/icons/hicolor/128x128/apps`, а панель
/// рисовала на его месте букву («значок "com.ayugram.desktop" (20px) — нет»).
///
/// Берём профили из `NIX_PROFILES` (её Nix выставляет и в неинтерактивном
/// окружении — она была на месте) и добавляем к ним `~/.nix-profile` на
/// случай, если переменной нет вовсе. Несуществующие пути ничего не стоят:
/// обход каталогов и так пропускает их молча.
fn профили_nix() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(профили) = std::env::var("NIX_PROFILES") {
        // Разделитель здесь ПРОБЕЛ, а не двоеточие, — так эту переменную
        // составляет сам Nix («/nix/var/nix/profiles/default /home/…/.nix-profile»).
        for p in профили.split_whitespace().filter(|s| !s.is_empty()) {
            out.push(PathBuf::from(p).join("share"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        let свой = PathBuf::from(&home).join(".nix-profile/share");
        if !out.contains(&свой) {
            out.push(свой);
        }
    }
    out
}

/// Каталоги, где значки лежат россыпью, без темы вовсе. Так их кладут
/// .deb/.rpm-пакеты и почти всё, что ставится не из репозитория.
fn каталоги_вразнобой() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        out.push(PathBuf::from(d).join("pixmaps"));
    }
    if let Ok(home) = std::env::var("HOME") {
        out.push(PathBuf::from(&home).join(".local/share/pixmaps"));
    }
    for p in профили_nix() {
        out.push(p.join("pixmaps"));
    }
    out
}

/// Темы в порядке предпочтения: заданная человеком, затем ходовые, затем
/// hicolor (по спецификации это общая свалка, куда кладут все приложения).
fn темы() -> Vec<String> {
    let mut out = Vec::new();
    // Своей ручки у dawn нет, но GTK-приложения читают ровно эту переменную,
    // и держать тему значков компоновщика отдельно от них незачем.
    if let Ok(t) = std::env::var("DAWN_ICON_THEME").or_else(|_| std::env::var("GTK_ICON_THEME")) {
        if !t.trim().is_empty() {
            out.push(t.trim().to_string());
        }
    }
    for t in ["Papirus", "Adwaita", "breeze", "Numix", "gnome", "hicolor"] {
        out.push(t.to_string());
    }
    out
}

/// Размер значка, вытащенный из пути: `.../48x48/apps/x.png` → 48,
/// `.../scalable/...` → 0 (не растр). Ноль же возвращается, если размера в
/// пути нет вовсе (pixmaps).
fn размер_из_пути(p: &Path) -> u32 {
    for кусок in p.iter().filter_map(|c| c.to_str()) {
        // «48x48» и «48@2x» — оба вида встречаются.
        let число = кусок.split(['x', '@']).next().unwrap_or("");
        if !число.is_empty() && число.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = число.parse::<u32>() {
                if (8..=1024).contains(&n) {
                    return n;
                }
            }
        }
    }
    0
}

/// Штраф вектора: хуже точного попадания в размер (0) и лучше всего
/// остального. Растеризуем сразу в нужный размер, пересчитывать нечего.
const ШТРАФ_ВЕКТОРА: u32 = 1;

/// Насколько путь хорош при желаемом размере `цель`. Меньше — лучше.
///
/// Уменьшать честнее, чем растягивать: значок 48→20 остаётся значком (ужимаем
/// усреднением по площади, см. `sni::fit_icon`), а 16→20 превращается в мыло.
/// Поэтому порядок такой: сперва всё, что НЕ МЕНЬШЕ цели, — ближайшее сверху;
/// потом всё, что мельче, — самое крупное из них; в самом конце — файлы, у
/// которых размера в пути нет вовсе (pixmaps).
///
/// Разряды («штраф ступенькой», а не пропорционально) намеренно: пропорция
/// давала бы, что 512 при цели 20 хуже, чем 16, — а это ровно наоборот.
///
/// Векторам этот штраф не считается: у них свой, `ШТРАФ_ВЕКТОРА`.
fn штраф(размер: u32, цель: u32) -> u32 {
    const МЕЛЬЧЕ_ЦЕЛИ: u32 = 10_000;
    const РАЗМЕР_НЕИЗВЕСТЕН: u32 = 100_000;
    if размер == 0 {
        return РАЗМЕР_НЕИЗВЕСТЕН;
    }
    if размер < цель {
        return МЕЛЬЧЕ_ЦЕЛИ + (цель - размер);
    }
    // Среди тех, что не меньше цели, «ближайший сверху» — НЕ лучший, и это
    // главная причина мыльных значков (жалоба Ярика 29.08.2026 «сделай иконки
    // в хорошем качестве»). `fit_icon` усредняет по площади: на каждый
    // выходной пиксель приходится (размер/цель)² исходных. При 22→20 это
    // всего 1.2 пикселя, причём у соседних выходных пикселей их РАЗНОЕ число
    // (1 или 2) — края идут ступеньками и «плывут». При 40→20 их ровно
    // четыре на каждый, при 48→20 — пять-шесть. Поэтому порядок такой:
    //
    //   1:1              — идеал, пересчёта нет вовсе;
    //   кратный ≥2       — точное усреднение (40, 60, 80 при цели 20);
    //   ≥2 некратный     — усреднять есть из чего, остаток лишь ранжирует;
    //   от цели до 2·цели— ужимать почти нечем, худшее из пригодных.
    // Внутри разряда выигрывает НАИМЕНЬШИЙ достаточный: 1024×1024 ужимается в
    // 20 не лучше, чем 128×128, а распаковать его стоит в шестьдесят раз
    // дороже. Исключение — последний разряд, там наоборот: чем крупнее, тем
    // больше исходных пикселей на выходной.
    let кратность = размер / цель;
    let остаток = размер % цель;
    if размер == цель {
        0
    } else if кратность >= 2 && остаток == 0 {
        100 + кратность
    } else if кратность >= 2 {
        200 + кратность
    } else {
        1_000 + (2 * цель - размер)
    }
}

/// Рекурсивный поиск `<имя>.png` и `<имя>.svg` под каталогом. Глубина
/// ограничена: у тем она три-четыре уровня, а без предела сюда однажды заедет
/// символическая ссылка на корень.
fn найти_файл(корень: &Path, имя: &str, цель: u32, глубина: u32) -> Option<(PathBuf, u32)> {
    if глубина == 0 {
        return None;
    }
    let png = format!("{имя}.png");
    let svg = format!("{имя}.svg");
    let mut лучшее: Option<(PathBuf, u32)> = None;
    let Ok(чтение) = std::fs::read_dir(корень) else { return None };
    for запись in чтение.flatten() {
        let путь = запись.path();
        // symlink_metadata, а не metadata: по ссылке на каталог ходить не надо
        // — в темах ими часто связаны размеры между собой, и один и тот же
        // подкаталог обошёлся бы десяток раз.
        let Ok(мета) = запись.metadata() else { continue };
        if мета.is_dir() {
            if let Some(найдено) = найти_файл(&путь, имя, цель, глубина - 1) {
                if лучшее.as_ref().is_none_or(|(_, ш)| найдено.1 < *ш) {
                    лучшее = Some(найдено);
                }
            }
        } else {
            let имя_файла = запись.file_name();
            let имя_файла = имя_файла.to_str().unwrap_or_default();
            let ш = if имя_файла == png {
                штраф(размер_из_пути(&путь), цель)
            } else if имя_файла == svg {
                ШТРАФ_ВЕКТОРА
            } else {
                continue;
            };
            if лучшее.as_ref().is_none_or(|(_, было)| ш < *было) {
                лучшее = Some((путь, ш));
            }
        }
    }
    лучшее
}

/// Найти файл значка по имени: сперва по темам в порядке предпочтения, потом
/// россыпью в pixmaps, потом — по всем остальным темам, какие есть.
fn путь_значка(имя: &str, цель: u32) -> Option<PathBuf> {
    if имя.is_empty() {
        return None;
    }
    // Приложение вправе прислать вместо имени полный путь к файлу — так делают
    // AppImage и всё, что живёт вне системных каталогов.
    let как_путь = Path::new(имя);
    if как_путь.is_absolute() && как_путь.is_file() {
        return Some(как_путь.to_path_buf());
    }

    let базы = каталоги_тем();
    for тема in темы() {
        for база in &базы {
            let каталог = база.join(&тема);
            if !каталог.is_dir() {
                continue;
            }
            if let Some((p, _)) = найти_файл(&каталог, имя, цель, 5) {
                return Some(p);
            }
        }
    }
    for каталог in каталоги_вразнобой() {
        if let Some((p, _)) = найти_файл(&каталог, имя, цель, 2) {
            return Some(p);
        }
    }
    // Последняя попытка: тема с непредсказуемым именем (своя, из AUR, из
    // архива). Обходим все каталоги тем целиком — дорого, но один раз на имя,
    // и только когда ничего не нашлось привычным путём.
    for база in &базы {
        if let Some((p, _)) = найти_файл(база, имя, цель, 6) {
            return Some(p);
        }
    }
    None
}

/// Прочитать PNG в premultiplied RGBA и ужать до `цель` по большей стороне.
fn прочитать_png(путь: &Path, цель: u32) -> Option<Icon> {
    let файл = std::fs::File::open(путь).ok()?;
    let mut декодер = png::Decoder::new(std::io::BufReader::new(файл));
    // RGBA8 на выходе независимо от того, что лежит в файле (палитра, серый,
    // 16 бит): разбирать все варианты руками — это ровно та работа, ради
    // которой крейт и взят.
    декодер.set_transformations(png::Transformations::normalize_to_color8());
    let mut чтение = декодер.read_info().ok()?;
    let инфо = чтение.info();
    let (sw, sh) = (инфо.width, инфо.height);
    // Гигантский PNG в панели не нужен, а память под него настоящая.
    if sw == 0 || sh == 0 || sw > 2048 || sh > 2048 {
        return None;
    }
    let цвет = инфо.color_type;
    let mut буфер = vec![0u8; чтение.output_buffer_size()?];
    let кадр = чтение.next_frame(&mut буфер).ok()?;
    let байт = кадр.buffer_size() / (sw as usize * sh as usize);

    // Приводим к тому же виду, в каком значок приходит по шине SNI: ARGB,
    // НЕ premultiplied, байты в сетевом порядке. Тогда ужать его можно той же
    // `sni::fit_icon`, что и картинки от приложений, — и значок из темы
    // получится ровно такой же, как присланный.
    let mut argb = vec![0u8; sw as usize * sh as usize * 4];
    for i in 0..(sw as usize * sh as usize) {
        let s = i * байт;
        let Some(px) = буфер.get(s..s + байт) else { break };
        let (r, g, b, a) = match (цвет, байт) {
            (png::ColorType::Rgba, 4) => (px[0], px[1], px[2], px[3]),
            (png::ColorType::Rgb, 3) => (px[0], px[1], px[2], 255),
            (png::ColorType::GrayscaleAlpha, 2) => (px[0], px[0], px[0], px[1]),
            (png::ColorType::Grayscale, 1) => (px[0], px[0], px[0], 255),
            _ => return None,
        };
        let o = i * 4;
        argb[o] = a;
        argb[o + 1] = r;
        argb[o + 2] = g;
        argb[o + 3] = b;
    }
    Some(crate::sni::fit_icon(&argb, sw as i32, sh as i32, цель as i32))
}

/// Растеризовать SVG прямо в нужный размер.
///
/// Пересчёта размеров тут нет и быть не должно: `usvg` даёт собственный размер
/// картинки, а мы вписываем её в квадрат `цель` одним масштабом (тем же по
/// обеим осям — иначе непрямоугольные значки поедут). `tiny_skia::Pixmap`
/// хранит RGBA **premultiplied** — ровно то, что ждёт `Icon`, поэтому байты
/// уходят как есть, без пути через `fit_icon`.
fn прочитать_svg(путь: &Path, цель: u32) -> Option<Icon> {
    let данные = std::fs::read(путь).ok()?;
    // Шрифты выключены (`default-features = false`), поэтому `Options` — это
    // только каталог для относительных ссылок внутри файла.
    let опции = resvg::usvg::Options {
        resources_dir: путь.parent().map(|p| p.to_path_buf()),
        ..Default::default()
    };
    let дерево = resvg::usvg::Tree::from_data(&данные, &опции).ok()?;
    let размер = дерево.size();
    let (sw, sh) = (размер.width(), размер.height());
    if !(sw.is_finite() && sh.is_finite()) || sw <= 0.0 || sh <= 0.0 {
        return None;
    }
    let масштаб = цель as f32 / sw.max(sh);
    let (w, h) = (
        ((sw * масштаб).round() as u32).clamp(1, цель),
        ((sh * масштаб).round() as u32).clamp(1, цель),
    );
    let mut холст = resvg::tiny_skia::Pixmap::new(w, h)?;
    resvg::render(
        &дерево,
        resvg::tiny_skia::Transform::from_scale(масштаб, масштаб),
        &mut холст.as_mut(),
    );
    Some(Icon { w: w as i32, h: h as i32, rgba: холст.take() })
}

/// Значок по имени из темы. `None` — не нашёлся или не разобрался.
pub fn найти(имя: &str, цель: u32) -> Option<Icon> {
    let путь = путь_значка(имя, цель)?;
    let вектор = путь.extension().is_some_and(|e| e.eq_ignore_ascii_case("svg"));
    let итог = if вектор { прочитать_svg(&путь, цель) } else { прочитать_png(&путь, цель) };
    match &итог {
        // Путь ИСХОДНИКА в логе — то, чем проверяется жалоба «значок мыльный»:
        // по числу в пути сразу видно, во сколько раз его ужимали (см. `штраф`).
        Some(_) => tracing::debug!("dawn/icons: {:?} → {}px из {:?}", имя, цель, путь),
        None => tracing::debug!("dawn/icons: {:?} не разобрался", путь),
    }
    итог
}

/// Имя значка приложения по его `app_id`: читает `Icon=` из .desktop-файла.
///
/// Нужно потому, что `app_id` и имя значка совпадают далеко не всегда:
/// у Telegram это `org.telegram.desktop` против `telegram`, у Steam —
/// `steam` против `steam_icon_...`. Ищем файл `<app_id>.desktop`, а если его
/// нет — файл, чьё имя кончается на `.<app_id>.desktop` (обратный DNS).
pub fn имя_значка_приложения(app_id: &str) -> Option<String> {
    if app_id.is_empty() {
        return None;
    }
    let базы = базы_каталогов();

    let точное = format!("{app_id}.desktop");
    let хвост = format!(".{}.desktop", app_id.to_ascii_lowercase());
    for база in базы {
        let прямой = база.join(&точное);
        if прямой.is_file() {
            if let Some(имя) = icon_из_desktop(&прямой) {
                return Some(имя);
            }
        }
        let Ok(чтение) = std::fs::read_dir(&база) else { continue };
        for запись in чтение.flatten() {
            let имя_файла = запись.file_name();
            let Some(имя_файла) = имя_файла.to_str() else { continue };
            let нижний = имя_файла.to_ascii_lowercase();
            if нижний == точное.to_ascii_lowercase() || нижний.ends_with(&хвост) {
                if let Some(имя) = icon_из_desktop(&запись.path()) {
                    return Some(имя);
                }
            }
        }
    }
    None
}

// ── Значок из самого X11-окна (`_NET_WM_ICON`) ───────────────────────────────

/// Соединение с Xwayland под чтение свойств — своё, а не то, которым правит
/// окнами XWM (оно у smithay внутри и наружу не отдаётся).
///
/// Заводится один раз и живёт до конца сеанса; `DISPLAY` к моменту первого
/// окна уже выставлен (`xwayland.rs`). Ошибку соединения запоминаем как
/// «нет X11» — повторять попытку на каждое окно незачем.
fn x11_связь() -> Option<&'static (
    smithay::reexports::x11rb::rust_connection::RustConnection,
    u32,
)> {
    use smithay::reexports::x11rb::{self, protocol::xproto::ConnectionExt as _};
    static СВЯЗЬ: std::sync::OnceLock<Option<(
        x11rb::rust_connection::RustConnection,
        u32,
    )>> = std::sync::OnceLock::new();
    СВЯЗЬ.get_or_init(|| {
        let (conn, _) = x11rb::connect(None)
            .map_err(|e| tracing::debug!("dawn/icons: X11 для значков недоступен: {e}"))
            .ok()?;
        let atom = conn.intern_atom(false, b"_NET_WM_ICON").ok()?.reply().ok()?.atom;
        Some((conn, atom))
    }).as_ref()
}

/// Значок, который окно принесло с собой в `_NET_WM_ICON`.
///
/// **Зачем.** Значок по `app_id` ищется среди установленных приложений — по
/// теме и по `.desktop`. У игр из Steam ни того, ни другого нет: `dota2`
/// нигде не установлен, в системе про него нет ни файла, и панель рисовала на
/// его месте букву (замер 29.08.2026 по живому логу: «значок "dota2" (20px) —
/// нет»). Но само окно иконку знает — X11-приложения кладут её прямо в
/// свойство окна, и берут её оттуда все панели X11.
///
/// Формат свойства (EWMH): подряд идущие картинки, каждая — `ширина`, `высота`
/// и `ширина*высота` пикселей `0xAARRGGBB`. Выбираем по тому же правилу, что и
/// для файлов: сперва ближайшую НЕ МЕНЬШЕ цели (уменьшать честнее, чем
/// растягивать), иначе самую крупную из мелких.
pub fn значок_окна_x11(окно: u32, цель: u32) -> Option<Icon> {
    use smithay::reexports::x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    let (conn, atom) = x11_связь()?;
    // Длина в 32-битных словах: 4 МиБ пикселей с запасом хватает на любой
    // набор размеров (256×256 — это 64 Ки слов).
    let ответ = conn.get_property(false, окно, *atom, AtomEnum::CARDINAL, 0, 1 << 20)
        .ok()?.reply().ok()?;
    let данные: Vec<u32> = ответ.value32()?.collect();
    разобрать_net_wm_icon(&данные, цель)
}

/// Процесс, которому принадлежит X11-окно (`_NET_WM_PID`).
///
/// **Зачем.** Режим Minecraft (`mine/`) обязан узнать окно САМОЙ игры и
/// вычесть его из сцены: иначе оно висит панелью внутри себя же и перехватывает
/// весь ввод, лежа поверх всех. Мод сидит в JVM игры, поэтому pid на том конце
/// сокета и есть pid этого окна. У Wayland-клиентов то же берётся из
/// `wl_client`, а у Xwayland — только отсюда: с точки зрения Wayland все
/// X11-окна принадлежат одному процессу, самому Xwayland.
pub fn pid_окна_x11(окно: u32) -> Option<u32> {
    use smithay::reexports::x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _};
    let (conn, _) = x11_связь()?;
    static АТОМ: std::sync::OnceLock<Option<u32>> = std::sync::OnceLock::new();
    let atom = (*АТОМ.get_or_init(|| {
        conn.intern_atom(false, b"_NET_WM_PID").ok()?.reply().ok().map(|о| о.atom)
    }))?;
    let ответ = conn
        .get_property(false, окно, atom, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?;
    let значения: Vec<u32> = ответ.value32()?.collect();
    значения.first().copied()
}

/// Разбор свойства `_NET_WM_ICON` — отдельно от X11, чтобы можно было
/// проверить тестом: соединения с сервером в тесте не подделать.
fn разобрать_net_wm_icon(данные: &[u32], цель: u32) -> Option<Icon> {
    let mut лучшая: Option<(u32, u32, usize)> = None; // (w, h, смещение пикселей)
    let mut i = 0usize;
    while i + 2 <= данные.len() {
        let (w, h) = (данные[i], данные[i + 1]);
        let пикселей = (w as usize).saturating_mul(h as usize);
        // Битое свойство: размер не сходится с остатком — дальше читать нечего.
        if w == 0 || h == 0 || i + 2 + пикселей > данные.len() {
            break;
        }
        let сторона = w.max(h);
        лучшая = match лучшая {
            None => Some((w, h, i + 2)),
            Some((лw, лh, лi)) => {
                let лсторона = лw.max(лh);
                // Тот же порядок предпочтений, что и в `штраф`: не меньше цели
                // — ближайшее сверху; всё мельче цели — самое крупное.
                let лучше = match (сторона >= цель, лсторона >= цель) {
                    (true, false) => true,
                    (false, true) => false,
                    (true, true) => сторона < лсторона,
                    (false, false) => сторона > лсторона,
                };
                if лучше { Some((w, h, i + 2)) } else { Some((лw, лh, лi)) }
            }
        };
        i += 2 + пикселей;
    }
    let (w, h, смещение) = лучшая?;
    let пикселей = (w as usize) * (h as usize);
    let mut argb = Vec::with_capacity(пикселей * 4);
    for p in &данные[смещение..смещение + пикселей] {
        // `fit_icon` ждёт байты в порядке A,R,G,B и НЕ premultiplied — ровно
        // то, что лежит в свойстве.
        argb.extend_from_slice(&p.to_be_bytes());
    }
    Some(crate::sni::fit_icon(&argb, w as i32, h as i32, цель as i32))
}

/// Каталоги с .desktop-файлами: свои раньше системных.
fn базы_каталогов() -> Vec<PathBuf> {
    let mut базы: Vec<PathBuf> = Vec::new();
    if let Ok(home) = std::env::var("HOME") {
        базы.push(PathBuf::from(&home).join(".local/share/applications"));
    }
    let data_dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    for d in data_dirs.split(':').filter(|s| !s.is_empty()) {
        базы.push(PathBuf::from(d).join("applications"));
    }
    // Профили Nix — по той же причине, что и в `каталоги_тем`: их `share` в
    // `XDG_DATA_DIRS` сессии не попадает, а `Icon=` у nix-приложения живёт
    // именно там.
    for p in профили_nix() {
        базы.push(p.join("applications"));
    }
    базы
}

/// `Icon=` из .desktop. Разбираем построчно и только до первой группы после
/// `[Desktop Entry]`: у действий (`[Desktop Action ...]`) свои значки, и брать
/// их вместо главного было бы неверно.
fn icon_из_desktop(путь: &Path) -> Option<String> {
    let текст = std::fs::read_to_string(путь).ok()?;
    let mut в_записи = false;
    for строка in текст.lines() {
        let строка = строка.trim();
        if строка.starts_with('[') {
            в_записи = строка == "[Desktop Entry]";
            continue;
        }
        if !в_записи {
            continue;
        }
        if let Some(значение) = строка.strip_prefix("Icon=") {
            let значение = значение.trim();
            if !значение.is_empty() {
                return Some(значение.to_string());
            }
        }
    }
    None
}

/// Кэш найденного: поиск лезет в файловую систему, и делать это на каждый кадр
/// нельзя. `None` в значении — «искали и не нашли», такой ответ тоже кэшируется:
/// иначе ненайденное искалось бы заново вечно.
#[derive(Default)]
pub struct Кэш {
    значки: HashMap<(String, u32), Option<Icon>>,
    имена: HashMap<String, Option<String>>,
}

impl Кэш {
    /// Значок по имени из темы.
    pub fn значок(&mut self, имя: &str, цель: u32) -> Option<&Icon> {
        let ключ = (имя.to_string(), цель);
        if !self.значки.contains_key(&ключ) {
            let найденный = найти(имя, цель);
            tracing::debug!(
                "dawn/icons: значок {:?} ({}px) — {}",
                имя, цель, if найденный.is_some() { "нашёлся" } else { "нет" },
            );
            self.значки.insert(ключ.clone(), найденный);
        }
        self.значки.get(&ключ).and_then(|v| v.as_ref())
    }

    /// Значок приложения по его `app_id` — через .desktop.
    pub fn значок_приложения(&mut self, app_id: &str, цель: u32) -> Option<&Icon> {
        if !self.имена.contains_key(app_id) {
            // Само имя app_id тоже пробуем как имя значка: у части приложений
            // .desktop-файла нет вовсе, зато значок в теме лежит под тем же
            // именем.
            let имя = имя_значка_приложения(app_id)
                .or_else(|| (!app_id.is_empty()).then(|| app_id.to_string()));
            self.имена.insert(app_id.to_string(), имя);
        }
        let имя = self.имена.get(app_id)?.clone()?;
        self.значок(&имя, цель)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Размер берётся из пути, а не из имени файла, и не путается с числами,
    /// которые размером быть не могут.
    #[test]
    fn размер_читается_из_пути() {
        assert_eq!(размер_из_пути(Path::new("/usr/share/icons/h/48x48/apps/x.png")), 48);
        assert_eq!(размер_из_пути(Path::new("/usr/share/icons/h/32@2x/apps/x.png")), 32);
        assert_eq!(размер_из_пути(Path::new("/usr/share/pixmaps/x.png")), 0);
        assert_eq!(размер_из_пути(Path::new("/usr/share/icons/h/scalable/apps/x.png")), 0);
        // Число вне разумного диапазона размером не считаем.
        assert_eq!(размер_из_пути(Path::new("/tmp/4/x.png")), 0);
        assert_eq!(размер_из_пути(Path::new("/tmp/99999/x.png")), 0);
    }

    /// Уменьшать честнее, чем растягивать: при цели 20 значок 48 обязан
    /// выиграть у значка 16, а точное попадание — у всех.
    #[test]
    fn ближайший_размер_выбирается_с_запасом_вверх() {
        assert!(штраф(20, 20) < штраф(24, 20));
        assert!(штраф(48, 20) < штраф(16, 20), "растянутый 16 хуже ужатого 48");
        assert!(штраф(512, 20) < штраф(16, 20), "и очень крупный — тоже лучше мелкого");
        assert!(штраф(24, 20) < штраф(16, 20));
        // Среди тех, что мельче цели, выигрывает самый крупный.
        assert!(штраф(16, 20) < штраф(8, 20));
        assert!(штраф(0, 20) > штраф(512, 20), "неизвестный размер — в последнюю очередь");
        // 29.08.2026: «ближайший сверху» — не лучший. Ужимать 22→20 нечем
        // (1.2 исходных пикселя на выходной), а 40→20 усредняется ровно по
        // четырём, поэтому кратный обязан выигрывать у почти-совпавшего.
        assert!(штраф(40, 20) < штраф(22, 20), "кратный лучше почти-совпавшего");
        assert!(штраф(48, 20) < штраф(22, 20), "вдвое крупнее лучше почти-совпавшего");
        assert!(штраф(20, 20) < штраф(40, 20), "точное попадание не пересчитывается вовсе");
        assert!(штраф(40, 20) < штраф(48, 20), "среди крупных кратный точнее");
        assert!(штраф(128, 20) < штраф(1024, 20), "наименьший достаточный дешевле");
        assert!(штраф(32, 20) < штраф(22, 20), "в последнем разряде наоборот: крупнее лучше");
    }

    /// Вектор идёт сразу за точным попаданием: растеризуем его в нужный
    /// размер, а любой другой PNG пришлось бы пересчитывать.
    #[test]
    fn вектор_уступает_только_точному_размеру() {
        assert!(штраф(20, 20) < ШТРАФ_ВЕКТОРА, "PNG ровно в размер лучше вектора");
        assert!(ШТРАФ_ВЕКТОРА < штраф(40, 20), "вектор лучше ужимаемого PNG");
        assert!(ШТРАФ_ВЕКТОРА < штраф(16, 20), "и лучше растягиваемого");
        assert!(ШТРАФ_ВЕКТОРА < штраф(0, 20), "и лучше значка без размера в пути");
    }

    /// SVG находится там, где PNG нет вовсе, — это и есть жалоба «иконки
    /// показаны буквами»: у приложений GNOME в теме только `scalable/*.svg`.
    #[test]
    fn svg_находится_и_растеризуется_в_нужный_размер() {
        let dir = std::env::temp_dir().join(format!("dawn-svg-test-{}", std::process::id()));
        let scalable = dir.join("тема/scalable/apps");
        std::fs::create_dir_all(&scalable).unwrap();
        let p = scalable.join("зелёный.svg");
        std::fs::write(
            &p,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="64">
                 <rect width="64" height="64" fill="#00ff00"/></svg>"##,
        ).unwrap();

        // Поиск по каталогу темы находит вектор при полном отсутствии растра.
        let (найденный, _) = найти_файл(&dir, "зелёный", 20, 5).expect("SVG обязан найтись");
        assert_eq!(найденный, p);

        // Растеризация даёт ровно целевой размер и непрозрачный зелёный.
        let значок = прочитать_svg(&p, 20).expect("SVG обязан разобраться");
        assert_eq!((значок.w, значок.h), (20, 20));
        assert_eq!(&значок.rgba[..4], &[0, 255, 0, 255]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Непрямоугольный SVG вписывается в квадрат цели БЕЗ растяжения: широкий
    /// значок обязан остаться широким, иначе логотипы поедут.
    #[test]
    fn пропорции_вектора_сохраняются() {
        let dir = std::env::temp_dir().join(format!("dawn-svg-agr-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("широкий.svg");
        std::fs::write(
            &p,
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="32">
                 <rect width="64" height="32" fill="#ff0000"/></svg>"##,
        ).unwrap();
        let значок = прочитать_svg(&p, 20).expect("SVG обязан разобраться");
        assert_eq!((значок.w, значок.h), (20, 10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Точный PNG обязан выигрывать у лежащего рядом вектора, иначе даром
    /// растеризуем то, что уже готово.
    #[test]
    fn точный_png_выигрывает_у_вектора() {
        let dir = std::env::temp_dir().join(format!("dawn-svg-vs-png-{}", std::process::id()));
        let растр = dir.join("20x20/apps");
        let вектор = dir.join("scalable/apps");
        std::fs::create_dir_all(&растр).unwrap();
        std::fs::create_dir_all(&вектор).unwrap();
        std::fs::write(вектор.join("х.svg"), "<svg xmlns=\"http://www.w3.org/2000/svg\"/>").unwrap();
        std::fs::write(растр.join("х.png"), []).unwrap();
        let (найденный, _) = найти_файл(&dir, "х", 20, 5).unwrap();
        assert_eq!(найденный.extension().unwrap(), "png");
        // А при другой цели, куда точного растра нет, выигрывает вектор.
        let (найденный, _) = найти_файл(&dir, "х", 48, 5).unwrap();
        assert_eq!(найденный.extension().unwrap(), "svg");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `Icon=` берётся только из главной группы: у действий свои значки.
    #[test]
    fn icon_читается_из_главной_группы() {
        let dir = std::env::temp_dir().join(format!("dawn-icons-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("t.desktop");
        std::fs::write(
            &p,
            "[Desktop Entry]\nName=T\nIcon=нужный\n\n[Desktop Action New]\nIcon=чужой\n",
        ).unwrap();
        assert_eq!(icon_из_desktop(&p).as_deref(), Some("нужный"));

        // Файл без Icon= — честный None, а не пустая строка.
        std::fs::write(&p, "[Desktop Entry]\nName=T\n").unwrap();
        assert_eq!(icon_из_desktop(&p), None);
        // И группа, начавшаяся раньше [Desktop Entry], не считается.
        std::fs::write(&p, "[Other]\nIcon=чужой\n[Desktop Entry]\nIcon=свой\n").unwrap();
        assert_eq!(icon_из_desktop(&p).as_deref(), Some("свой"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Замер и проверка НА ЖИВОЙ СИСТЕМЕ: что вообще находится по именам,
    /// которые встречаются в трее и в чипах, и сколько это стоит.
    ///
    /// `#[ignore]` — потому что результат зависит от того, какие темы стоят на
    /// машине, и в обычном прогоне это была бы плавающая проверка. Запуск:
    /// `cargo test --release -- --ignored --nocapture значки_на_этой_машине`.
    #[test]
    #[ignore = "смотрит на живую систему"]
    fn значки_на_этой_машине() {
        for имя in [
            "ghostty", "com.mitchellh.ghostty", "firefox", "chromium", "steam",
            "telegram", "org.telegram.desktop", "blueman", "gimp", "Alacritty",
            // Только SVG в теме — те самые «буквы вместо значков» до 29.08.2026.
            "org.gnome.Nautilus", "org.gnome.TextEditor", "org.gnome.Console",
            "org.pulseaudio.pavucontrol", "dshare",
            "нет-такого-значка",
        ] {
            let t = std::time::Instant::now();
            let путь = путь_значка(имя, 20);
            let мс = t.elapsed().as_millis();
            match путь {
                Some(p) => {
                    let разобрался = if p.extension().is_some_and(|e| e == "svg") {
                        прочитать_svg(&p, 20).is_some()
                    } else {
                        прочитать_png(&p, 20).is_some()
                    };
                    println!("{имя:32} → {p:?} ({мс} мс, разбор {разобрался})");
                }
                None => println!("{имя:32} → не нашёлся ({мс} мс)"),
            }
        }
        for app in ["com.mitchellh.ghostty", "chromium", "steam"] {
            println!("{app:32} .desktop Icon= → {:?}", имя_значка_приложения(app));
        }
    }

    /// Разбор PNG: собираем файл сами, читаем обратно и сверяем цвет.
    /// Заодно это проверка, что premultiply и порядок каналов не перепутаны —
    /// ровно там, где значки из темы стыкуются с картинками SNI.
    #[test]
    fn png_разбирается_в_premultiplied_rgba() {
        let dir = std::env::temp_dir().join(format!("dawn-png-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("i.png");
        {
            let файл = std::fs::File::create(&p).unwrap();
            let mut енк = png::Encoder::new(std::io::BufWriter::new(файл), 2, 2);
            енк.set_color(png::ColorType::Rgba);
            енк.set_depth(png::BitDepth::Eight);
            let mut w = енк.write_header().unwrap();
            // Все четыре пикселя — красный с половинной альфой.
            w.write_image_data(&[255, 0, 0, 128].repeat(4)).unwrap();
        }
        let значок = прочитать_png(&p, 2).expect("PNG обязан разобраться");
        assert_eq!((значок.w, значок.h), (2, 2));
        // Premultiplied: красный домножен на альфу (255·128/255 = 128).
        assert_eq!(&значок.rgba[..4], &[128, 0, 0, 128]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Свойство `_NET_WM_ICON` — несколько картинок подряд; собираем такое же,
    /// как присылают приложения (замер на живом дисплее 29.08.2026: у окна
    /// `dota2` там ровно одна картинка 48×48, у Steam — 128×128).
    fn свойство(картинки: &[(u32, u32, u32)]) -> Vec<u32> {
        let mut v = Vec::new();
        for &(w, h, цвет) in картинки {
            v.push(w);
            v.push(h);
            v.extend(std::iter::repeat(цвет).take((w * h) as usize));
        }
        v
    }

    #[test]
    fn из_свойства_окна_берётся_ближайший_сверху_размер() {
        // Непрозрачный красный: 0xAARRGGBB.
        let данные = свойство(&[(16, 16, 0xFF_FF_00_00), (32, 32, 0xFF_FF_00_00), (64, 64, 0xFF_FF_00_00)]);
        let значок = разобрать_net_wm_icon(&данные, 20).expect("значок обязан разобраться");
        // Цель 20: 16 мельче цели, из 32 и 64 берём ближайший сверху — 32,
        // и он ужимается до цели.
        assert_eq!((значок.w, значок.h), (20, 20));
        assert_eq!(&значок.rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn все_картинки_мельче_цели_берём_самую_крупную() {
        let данные = свойство(&[(8, 8, 0xFF_00_FF_00), (16, 16, 0xFF_00_FF_00)]);
        let значок = разобрать_net_wm_icon(&данные, 20).expect("значок обязан разобраться");
        // Растягивать нечего — 16×16 остаётся собой (fit_icon не увеличивает).
        assert_eq!((значок.w, значок.h), (16, 16));
        assert_eq!(&значок.rgba[..4], &[0, 255, 0, 255]);
    }

    #[test]
    fn битое_свойство_не_валит_компоновщик() {
        // Заявлено 64×64, а пикселей нет — читать нечего.
        assert!(разобрать_net_wm_icon(&[64, 64, 0xFF_FF_FF_FF], 20).is_none());
        assert!(разобрать_net_wm_icon(&[], 20).is_none());
        assert!(разобрать_net_wm_icon(&[0, 0], 20).is_none());
        // Первая картинка целая, вторая обрезана — берём что успели прочитать.
        let mut данные = свойство(&[(4, 4, 0xFF_00_00_FF)]);
        данные.extend_from_slice(&[32, 32, 1, 2, 3]);
        let значок = разобрать_net_wm_icon(&данные, 20).expect("целая картинка обязана дойти");
        assert_eq!((значок.w, значок.h), (4, 4));
    }
}
