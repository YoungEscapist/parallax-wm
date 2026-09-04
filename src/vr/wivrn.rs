//! Живой шлем: сервер WiVRn и рантайм OpenXR.
//!
//! Всё, что отделяет «код VR написан» от «Quest 3 показывает окна», — это три
//! вещи снаружи parallax: должен идти `wivrn-server`, загрузчик OpenXR должен найти
//! манифест WiVRn, и человек должен надеть шлем и запустить на нём клиент.
//! Первые две parallax берёт на себя здесь, третью — ждёт (см. `vr::Ожидание`).
//!
//! Раньше это делал скрипт `vr.sh` от root: он клал symlink в
//! `/etc/xdg/openxr/1/active_runtime.json` и поднимал сервер через `runuser`.
//! Здесь всё то же самое, но без root и без скрипта — parallax уже идёт от нужного
//! человека, в нужной сессии и с нужным `XDG_RUNTIME_DIR`.

use std::path::{Path, PathBuf};

/// Имя процесса сервера. Сравниваем именно с `comm` (`/proc/<pid>/comm`), а не
/// ищем подстроку в командной строке: `pgrep -f` находит сам себя и вообще
/// любого, у кого это слово мелькнуло в аргументах, — на этом в parallax уже
/// обжигались не раз.
const ИМЯ_СЕРВЕРА: &str = "wivrn-server";

/// Где лежит бинарь сервера. Порядок — от собранного руками к системному.
const ГДЕ_СЕРВЕР: &[&str] = &[
    "/usr/local/bin/wivrn-server",
    "/usr/bin/wivrn-server",
];

/// Манифесты рантайма WiVRn: файл, по которому загрузчик OpenXR понимает, какую
/// библиотеку открыть.
const ГДЕ_МАНИФЕСТ: &[&str] = &[
    "/usr/local/share/openxr/1/openxr_wivrn.json",
    "/usr/share/openxr/1/openxr_wivrn.json",
];

/// Идёт ли сервер прямо сейчас.
pub fn сервер_идёт() -> bool {
    let Ok(каталог) = std::fs::read_dir("/proc") else {
        return false;
    };
    for вход in каталог.flatten() {
        // Нас интересуют только числовые имена — это процессы.
        if !вход
            .file_name()
            .to_string_lossy()
            .chars()
            .all(|с| с.is_ascii_digit())
        {
            continue;
        }
        if let Ok(comm) = std::fs::read_to_string(вход.path().join("comm")) {
            if comm.trim() == ИМЯ_СЕРВЕРА {
                return true;
            }
        }
    }
    false
}

/// Путь к бинарю сервера, если он вообще установлен.
pub fn сервер_путь() -> Option<PathBuf> {
    ГДЕ_СЕРВЕР
        .iter()
        .map(PathBuf::from)
        .find(|п| п.is_file())
}

/// Манифест рантайма WiVRn, если он установлен.
pub fn манифест() -> Option<PathBuf> {
    // Свой (собранный руками) манифест человека — сильнее системного.
    if let Some(дом) = std::env::var_os("HOME") {
        let свой = PathBuf::from(дом).join(".local/share/openxr/1/openxr_wivrn.json");
        if свой.is_file() {
            return Some(свой);
        }
    }
    ГДЕ_МАНИФЕСТ.iter().map(PathBuf::from).find(|п| п.is_file())
}

/// Найдёт ли загрузчик рантайм САМ, без нашей помощи: либо человек выставил
/// `XR_RUNTIME_JSON`, либо в системе прописан активный рантайм.
fn рантайм_виден_сам() -> bool {
    if std::env::var_os("XR_RUNTIME_JSON").is_some() {
        return true;
    }
    let mut места: Vec<PathBuf> = vec![PathBuf::from("/etc/xdg/openxr/1/active_runtime.json")];
    if let Some(дом) = std::env::var_os("HOME") {
        места.push(PathBuf::from(дом).join(".config/openxr/1/active_runtime.json"));
    }
    места.iter().any(|п| п.exists())
}

/// Есть ли вообще к чему подключаться: либо рантайм виден загрузчику сам, либо
/// мы знаем, где лежит манифест WiVRn.
pub fn рантайм_есть() -> bool {
    рантайм_виден_сам() || манифест().is_some()
}

/// Манифест, который РЕАЛЬНО будет открыт: то, что выбрал человек, сильнее
/// того, что нашли мы.
fn действующий_манифест() -> Option<PathBuf> {
    if let Some(явный) = std::env::var_os("XR_RUNTIME_JSON") {
        return Some(PathBuf::from(явный));
    }
    let mut места: Vec<PathBuf> = Vec::new();
    if let Some(дом) = std::env::var_os("HOME") {
        места.push(PathBuf::from(дом).join(".config/openxr/1/active_runtime.json"));
    }
    места.push(PathBuf::from("/etc/xdg/openxr/1/active_runtime.json"));
    места.into_iter().find(|п| п.exists()).or_else(манифест)
}

/// Будет ли рантаймом именно WiVRn — то есть нужно ли вообще поднимать сервер.
///
/// Проверка не праздная: с проводным шлемом через Monado, а тем более с
/// симулятором в харнессе, `wivrn-server` не нужен, и запускать его «на всякий
/// случай» значило бы вешать на человека лишний процесс, который он не просил
/// и не заметит.
///
/// Смотрим и на путь, и на содержимое: `active_runtime.json` — обычно symlink с
/// нейтральным именем, и по одному имени файла WiVRn не узнать.
pub fn это_wivrn() -> bool {
    let Some(путь) = действующий_манифест() else {
        return false;
    };
    let по_пути = std::fs::canonicalize(&путь)
        .unwrap_or(путь.clone())
        .to_string_lossy()
        .to_lowercase()
        .contains("wivrn");
    по_пути
        || std::fs::read_to_string(&путь)
            .map(|текст| текст.to_lowercase().contains("wivrn"))
            .unwrap_or(false)
}

/// Подставить `XR_RUNTIME_JSON`, чтобы загрузчик OpenXR нашёл рантайм.
///
/// Если рантайм и так виден — ничего не трогаем и переменную не заводим.
///
/// **Парная функция обязательна.** Окружение parallax наследуют все запускаемые из
/// него программы. Оставь мы переменную выставленной насовсем — любая игра,
/// запущенная после включения VR, полезла бы в WiVRn вместо своего рантайма,
/// причём молча. Поэтому `убрать_рантайм` зовётся сразу, как только рантайм
/// открыт (`dlopen` уже случился, библиотека никуда не денется) или как только
/// стало ясно, что не откроется.
///
/// **Только с главного потока.** `setenv` переписывает общий `environ`, а его
/// читает каждый `Command::spawn`; менять окружение из потока поиска шлема,
/// пока главный цикл запускает программы, — гонка на ровном месте. Поэтому
/// обе функции зовутся из `vr::mod`, а поток видит уже готовое окружение.
pub fn подставить_рантайм() {
    if рантайм_виден_сам() {
        return;
    }
    // Нечего подставлять — пусть загрузчик отвечает своей ошибкой, она понятнее
    // выдуманной нами.
    let Some(манифест) = манифест() else { return };
    tracing::info!("plx/vr: using runtime {}", манифест.display());
    std::env::set_var("XR_RUNTIME_JSON", &манифест);
    НАШ_РАНТАЙМ.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Вернуть окружение как было (см. `подставить_рантайм`).
///
/// Снимает переменную только если её ставили мы: `рантайм_виден_сам` считает
/// выставленный человеком `XR_RUNTIME_JSON` за «видно само», и в этом случае
/// `подставить_рантайм` ничего не делал — значит и убирать нечего.
pub fn убрать_рантайм() {
    if НАШ_РАНТАЙМ.load(std::sync::atomic::Ordering::Relaxed) {
        НАШ_РАНТАЙМ.store(false, std::sync::atomic::Ordering::Relaxed);
        std::env::remove_var("XR_RUNTIME_JSON");
    }
}

/// Ставили ли `XR_RUNTIME_JSON` мы сами.
static НАШ_РАНТАЙМ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Команда запуска сервера — ровно та, что была в `vr.sh`.
///
/// **Грабля, стоившая скрипту отдельного комментария:** Monado (а WiVRn это
/// Monado) следит за своим stdin через epoll, а `/dev/null` на `epoll_ctl`
/// отвечает отказом — сервер падает на старте. Поэтому stdin обязан быть
/// НАСТОЯЩИЙ, и его даёт бесконечный `sleep` через конвейер.
///
/// `setsid` — чтобы сервер пережил перезапуск parallax по Super+R: шлем при этом
/// не отваливается, а новый parallax находит уже поднятый сервер.
pub fn команда_сервера(бинарь: &Path) -> String {
    format!(
        "setsid sh -c 'sleep infinity | exec {} ' >/tmp/wivrn-server.log 2>&1 &",
        бинарь.display()
    )
}
