//! Картинка, брошенная на рабочий стол, становится окном.
//!
//! Просьба Ярика 02.10.2026: «перетащить картинку на рабочий стол — она
//! появится окном, и её можно закрепить как виджет и ресайзить».
//!
//! **Как устроено.** Перетаскивание (DnD) в Wayland — разговор двух клиентов
//! через композитор: источник предлагает типы данных, цель под курсором
//! соглашается и забирает. Рабочий стол у parallax — не клиент (обои рисует
//! фоновый слой, и бросок ему ни к чему), поэтому брошенное мимо окон раньше
//! просто отменялось. Теперь эту роль берёт сам композитор:
//!
//!  1. источник броска оборачивается (`ОбёрткаDnd`): если он предлагает
//!     что-то похожее на картинку, его отмену придерживаем — после
//!     `cancelled` клиент вправе тут же выкинуть данные;
//!  2. на отпускании (`dnd_брошен`) смотрим, куда бросили: ни одного окна под
//!     курсором и цель не согласилась — значит, на стол. Просим у источника
//!     данные в трубу (`send`), читаем их в своём потоке;
//!  3. из `text/uri-list` берём файл (или качаем http-картинку браузера), из
//!     `image/*` сохраняем байты в кэш — и открываем окно-просмотрщик;
//!  4. окно — это mpv с app_id `plx-picture` (`картинка_ли`): плавает поверх
//!     раскладки, встаёт серединой туда, куда бросили, двигается обычной ЛКМ
//!     (mpv сам просит xdg move), Super+Y закрепляет его на экране, как
//!     виджет, а ресайз закреплённого держит пропорции снимка (pin.rs).
//!
//! Брошенное НА окно идёт этому окну, как и раньше: мы вмешиваемся только
//! туда, где цели не было вовсе.

use std::any::Any;
use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::input::dnd::{DndAction, Source, SourceMetadata};
use smithay::reexports::calloop::channel;
use smithay::utils::{IsAlive, Logical, Point, Size};

use crate::state::Parallax;

/// app_id окна-картинки — по нему его узнают раскладка, закрепление и ресайз.
pub const APP_ID: &str = "plx-picture";

/// Сколько ждём данные от источника броска.
const ЖДАТЬ_ДАННЫЕ: Duration = Duration::from_secs(5);

/// Окно-картинка (mpv, открытый нами по броску)?
pub fn картинка_ли(window: &Window) -> bool {
    crate::xwin::app_id(window).as_deref() == Some(APP_ID)
}

/// Типы данных, которые умеем превратить в картинку, по убыванию желания.
fn выбрать_тип(типы: &[String]) -> Option<String> {
    let есть = |t: &str| типы.iter().find(|m| m.as_str() == t).cloned();
    есть("text/uri-list")
        .or_else(|| типы.iter().find(|m| m.starts_with("image/") && !m.contains("svg")).cloned())
        .or_else(|| есть("text/x-moz-url"))
        .or_else(|| есть("text/plain;charset=utf-8"))
        .or_else(|| есть("UTF8_STRING"))
        .or_else(|| есть("text/plain"))
}

/// Источник броска, у которого отмену можно придержать.
pub struct ОбёрткаDnd {
    inner: Arc<dyn Source>,
    отмена: Arc<Mutex<Option<Instant>>>,
}

impl IsAlive for ОбёрткаDnd {
    fn alive(&self) -> bool {
        self.inner.alive()
    }
}

impl Source for ОбёрткаDnd {
    fn is_client_local(&self, target: &dyn Any) -> bool {
        self.inner.is_client_local(target)
    }
    fn metadata(&self) -> Option<SourceMetadata> {
        self.inner.metadata()
    }
    fn choose_action(&self, action: DndAction) {
        self.inner.choose_action(action)
    }
    fn send(&self, mime_type: &str, fd: OwnedFd) {
        self.inner.send(mime_type, fd)
    }
    fn drop_performed(&self) {
        self.inner.drop_performed()
    }
    fn cancel(&self) {
        // Есть что забрать — отмену решит `dnd_брошен` (он зовётся сразу
        // следом) или, если броска не было вовсе, `dnd_tick`.
        let годится = self.inner.metadata()
            .is_some_and(|m| выбрать_тип(&m.mime_types).is_some());
        if годится {
            if let Ok(mut о) = self.отмена.lock() {
                *о = Some(Instant::now());
                return;
            }
        }
        self.inner.cancel()
    }
    fn finished(&self) {
        self.inner.finished()
    }
}

/// Текущий бросок: сам источник и отметка придержанной отмены.
pub struct ТекущийDnd {
    inner: Arc<dyn Source>,
    отмена: Arc<Mutex<Option<Instant>>>,
}

/// Данные броска, прочитанные потоком.
pub struct Принято {
    тип: String,
    данные: Vec<u8>,
    точка: Point<f64, Logical>,
}

/// Чем закончилась доставка данных.
pub enum Весть {
    Данные(Принято),
    /// Картинка из сети скачана (или нет) — открыть файл.
    Файл(Option<PathBuf>, Point<f64, Logical>),
}

/// Окно-картинка, которое надо поставить серединой в точку броска, как только
/// клиент скажет свой размер.
pub struct ЖдётМеста {
    window: Window,
    точка: Point<f64, Logical>,
    размер: Size<i32, Logical>,
    с: Instant,
}

impl Parallax {
    /// Начало броска: обернуть источник (см. заголовок модуля).
    pub fn dnd_начат<S: Source>(&mut self, source: S) -> ОбёрткаDnd {
        // Прошлый бросок, если его отмена ещё висит, отменяем честно.
        self.dnd_отменить_висящий();
        let inner: Arc<dyn Source> = Arc::new(source);
        let отмена = Arc::new(Mutex::new(None));
        self.dnd_текущий = Some(ТекущийDnd { inner: inner.clone(), отмена: отмена.clone() });
        ОбёрткаDnd { inner, отмена }
    }

    fn dnd_отменить_висящий(&mut self) {
        if let Some(т) = self.dnd_текущий.take() {
            if т.отмена.lock().map(|о| о.is_some()).unwrap_or(false) {
                т.inner.cancel();
            }
        }
    }

    /// Бросок завершён (зовётся из `DndGrabHandler::dropped`).
    pub fn dnd_брошен(&mut self, validated: bool, location: Point<f64, Logical>) {
        let Some(т) = self.dnd_текущий.take() else { return };
        let придержано = т.отмена.lock().map(|о| о.is_some()).unwrap_or(false);
        if !придержано {
            return;
        }
        // Цель нашлась и согласилась — бросок не наш (сюда мы и не попадём:
        // отмены тогда нет). Под курсором окно — тоже не наш: окно отказалось.
        let на_стол = !validated
            && self.закреплённое_под(location).is_none()
            && self.окно_под(location).is_none();
        let тип = т.inner.metadata().and_then(|m| выбрать_тип(&m.mime_types));
        let (true, Some(тип)) = (на_стол, тип) else {
            т.inner.cancel();
            return;
        };
        let Ok((читать, писать)) = std::io::pipe() else {
            т.inner.cancel();
            return;
        };
        tracing::info!("plx/picture: dropped on the desktop at ({:.0},{:.0}), asking for {}",
            location.x, location.y, тип);
        // Копируем, а не переносим: файловый менеджер на «move» удалил бы
        // оригинал. Действие шлём только тем, кто про действия знает (v3+).
        let умеет_копию = т.inner.metadata()
            .is_some_and(|m| m.dnd_actions.contains(&DndAction::Copy));
        if умеет_копию {
            т.inner.choose_action(DndAction::Copy);
        }
        т.inner.send(&тип, OwnedFd::from(писать));
        if умеет_копию {
            т.inner.drop_performed();
        }
        let tx = self.картинки_канал();
        let inner = т.inner.clone();
        self.dnd_ждёт_данных = Some((inner, умеет_копию));
        std::thread::spawn(move || {
            let данные = прочитать_с_таймаутом(читать, ЖДАТЬ_ДАННЫЕ);
            let _ = tx.send(Весть::Данные(Принято { тип, данные, точка: location }));
        });
        self.request_redraw();
    }

    /// Канал «поток → главный цикл»; заводится при первом броске.
    fn картинки_канал(&mut self) -> channel::Sender<Весть> {
        if let Some(tx) = &self.картинки_tx {
            return tx.clone();
        }
        let (tx, rx) = channel::channel::<Весть>();
        let _ = self.петля.insert_source(rx, |event, _, state| {
            if let channel::Event::Msg(весть) = event {
                state.картинка_пришла(весть);
            }
        });
        self.картинки_tx = Some(tx.clone());
        tx
    }

    fn картинка_пришла(&mut self, весть: Весть) {
        match весть {
            Весть::Данные(п) => {
                // Источник своё отдал — закрываем бросок.
                if let Some((inner, умеет_копию)) = self.dnd_ждёт_данных.take() {
                    if умеет_копию && !п.данные.is_empty() {
                        inner.finished();
                    } else {
                        inner.cancel();
                    }
                }
                self.разобрать_брошенное(п);
            }
            Весть::Файл(Some(путь), точка) => self.открыть_картинку(&путь, точка),
            Весть::Файл(None, _) => {
                self.уведомить(crate::т!(
                    "Картинку скачать не вышло", "Could not download the picture",
                ));
            }
        }
    }

    /// То же, что бросок `text/uri-list` с этим текстом (для ctl/харнесса).
    pub fn картинка_брошена_текстом(&mut self, текст: &str, точка: Point<f64, Logical>) {
        self.разобрать_брошенное(Принято {
            тип: "text/uri-list".into(),
            данные: текст.as_bytes().to_vec(),
            точка,
        });
    }

    fn разобрать_брошенное(&mut self, п: Принято) {
        if п.данные.is_empty() {
            tracing::warn!("plx/picture: the drag source gave no data for {}", п.тип);
            return;
        }
        if п.тип.starts_with("image/") {
            let расш = п.тип.trim_start_matches("image/").split(';').next().unwrap_or("png");
            let расш = if расш == "jpeg" { "jpg" } else { расш };
            let Some(путь) = путь_в_кэше(&п.данные, расш) else { return };
            if std::fs::write(&путь, &п.данные).is_ok() {
                self.открыть_картинку(&путь, п.точка);
            }
            return;
        }
        let текст = String::from_utf8_lossy(&п.данные);
        let ссылки = разобрать_ссылки(&текст, п.тип == "text/x-moz-url");
        let mut открыто = 0;
        for ссылка in ссылки {
            match ссылка {
                Ссылка::Файл(путь) if похоже_на_картинку(&путь) => {
                    // Несколько файлов — лесенкой, чтобы не легли стопкой.
                    let сдвиг = 40.0 * открыто as f64;
                    let точка = Point::from((п.точка.x + сдвиг, п.точка.y + сдвиг));
                    self.открыть_картинку(&путь, точка);
                    открыто += 1;
                }
                Ссылка::Сеть(url) if открыто == 0 => {
                    self.скачать_картинку(url, п.точка);
                    открыто += 1;
                }
                _ => {}
            }
        }
        if открыто == 0 {
            tracing::info!("plx/picture: nothing that looks like a picture in {:?}", текст);
        }
    }

    /// Картинка браузера приходит ссылкой — качаем её в кэш в своём потоке.
    fn скачать_картинку(&mut self, url: String, точка: Point<f64, Logical>) {
        let tx = self.картинки_канал();
        tracing::info!("plx/picture: downloading {url}");
        std::thread::spawn(move || {
            let путь = путь_в_кэше(url.as_bytes(), &расширение_ссылки(&url));
            let ok = путь.as_ref().is_some_and(|путь| {
                std::process::Command::new("curl")
                    .args(["-fsSL", "--max-time", "20", "-o"])
                    .arg(путь)
                    .arg(&url)
                    .status()
                    .is_ok_and(|s| s.success())
            });
            let _ = tx.send(Весть::Файл(путь.filter(|_| ok), точка));
        });
    }

    /// Открыть файл окном-картинкой; встанет серединой в `точка` (холст).
    pub fn открыть_картинку(&mut self, путь: &Path, точка: Point<f64, Logical>) {
        tracing::info!("plx/picture: opening {:?}", путь);
        let имя = путь.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let socket = self.socket_name.to_string_lossy().to_string();
        let mut cmd = std::process::Command::new("mpv");
        cmd.args([
            "--wayland-app-id=plx-picture",
            "--force-window=yes",
            "--keep-open=yes",
            "--image-display-duration=inf",
            // Не `--osc=no`: у сборки без lua такой опции нет, и mpv выходит.
            "--script-opts=osc-visibility=never",
            "--osd-level=0",
            "--border=no",
            "--autofit-larger=45%x45%",
            "--loop-file=inf",
            "--mute=yes",
            "--really-quiet",
        ])
        .arg(format!("--title={имя}"))
        .arg("--")
        .arg(путь)
        .env("WAYLAND_DISPLAY", &socket)
        .stdin(std::process::Stdio::null());
        match cmd.spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                self.картинки_ждут.push((точка, Instant::now()));
            }
            Err(e) => {
                tracing::warn!("plx/picture: mpv did not start: {e}");
                self.уведомить(crate::т!(
                    "Для картинок нужен mpv", "Pictures need mpv installed",
                ));
            }
        }
    }

    /// Новое окно сказало app_id (xdg_shell::app_id_changed): если это наша
    /// картинка — она плавает поверх любой раскладки, размер выбирает сама,
    /// а место — точка броска.
    pub fn картинка_появилась(&mut self, window: &Window) {
        if !картинка_ли(window) {
            return;
        }
        // Самая старая из ждущих — их открывают по одной, и порядок тот же.
        self.картинки_ждут.retain(|(_, с)| с.elapsed() < Duration::from_secs(15));
        let точка = if self.картинки_ждут.is_empty() {
            self.pointer_location
        } else {
            self.картинки_ждут.remove(0).0
        };
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| &tw.window == window) {
            tw.floating = true;
            tw.float_pinned = true;
        }
        // Размер — клиентский: раскладка успела предложить ему слот.
        crate::xwin::set_size(window, None, crate::xwin::Tiled::Unset);
        crate::xwin::configure(window);
        self.картинки_центр.push(ЖдётМеста {
            window: window.clone(),
            точка,
            размер: Size::from((0, 0)),
            с: Instant::now(),
        });
        self.arrange();
        self.request_redraw();
    }

    /// Раз за тик: висящая отмена броска без отпускания и окна-картинки,
    /// которые ещё не встали на место.
    pub fn dnd_tick(&mut self) {
        let протухла = self.dnd_текущий.as_ref().is_some_and(|т| {
            т.отмена.lock().ok()
                .and_then(|о| *о)
                .is_some_and(|с| с.elapsed() > Duration::from_millis(200))
        });
        if протухла {
            self.dnd_отменить_висящий();
        }
        if self.картинки_центр.is_empty() {
            return;
        }
        let mut ждут = std::mem::take(&mut self.картинки_центр);
        ждут.retain(|ж| ж.window.alive() && ж.с.elapsed() < Duration::from_secs(3));
        for ж in ждут.iter_mut() {
            let размер = ж.window.geometry().size;
            if размер.w <= 0 || размер.h <= 0 || размер == ж.размер {
                continue;
            }
            ж.размер = размер;
            let loc: Point<i32, Logical> = (
                (ж.точка.x - размер.w as f64 / 2.0).round() as i32,
                (ж.точка.y - размер.h as f64 / 2.0).round() as i32,
            ).into();
            self.space.map_element(ж.window.clone(), loc, true);
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| tw.window == ж.window) {
                tw.position = loc;
                tw.float_position = loc;
                tw.float_position_set = true;
                tw.float_size = Some(размер);
            }
            self.request_redraw();
        }
        self.картинки_центр = ждут;
    }
}

/// Прочитать трубу до конца, но не дольше `срок` (источник может так и не
/// ответить — ждать его вечно поток не обязан).
fn прочитать_с_таймаутом(mut труба: std::io::PipeReader, срок: Duration) -> Vec<u8> {
    let конец = Instant::now() + срок;
    let mut всё = Vec::new();
    let mut буфер = [0u8; 64 * 1024];
    loop {
        let осталось = конец.saturating_duration_since(Instant::now());
        if осталось.is_zero() || всё.len() > 256 * 1024 * 1024 {
            break;
        }
        let mut pfd = libc::pollfd { fd: труба.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        let r = unsafe { libc::poll(&mut pfd, 1, осталось.as_millis().min(1000) as i32) };
        if r < 0 {
            break;
        }
        if r == 0 {
            continue;
        }
        match труба.read(&mut буфер) {
            Ok(0) => break,
            Ok(n) => всё.extend_from_slice(&буфер[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    всё
}

#[derive(Debug, PartialEq)]
enum Ссылка {
    Файл(PathBuf),
    Сеть(String),
}

/// `text/uri-list` (и просто текст с путями): строки-ссылки, `#` — комментарии.
/// У `text/x-moz-url` ссылка и подпись чередуются построчно — берём ссылки.
fn разобрать_ссылки(текст: &str, moz: bool) -> Vec<Ссылка> {
    let текст = текст.replace('\0', "");
    текст
        .lines()
        .enumerate()
        .filter(|(i, _)| !moz || i % 2 == 0)
        .map(|(_, l)| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            if let Some(путь) = l.strip_prefix("file://") {
                // file://host/path — хост пустой или localhost.
                let путь = путь.strip_prefix("localhost").unwrap_or(путь);
                Some(Ссылка::Файл(PathBuf::from(раскодировать(путь))))
            } else if l.starts_with("http://") || l.starts_with("https://") {
                Some(Ссылка::Сеть(l.to_string()))
            } else if l.starts_with('/') {
                Some(Ссылка::Файл(PathBuf::from(l)))
            } else {
                None
            }
        })
        .collect()
}

/// Процентная раскодировка пути из URI (`%20` → пробел, кириллица — UTF-8).
fn раскодировать(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = std::str::from_utf8(&b[i + 1..i + 3]).ok()
                .and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn похоже_на_картинку(путь: &Path) -> bool {
    const РАСШ: [&str; 13] = [
        "png", "jpg", "jpeg", "gif", "webp", "bmp", "avif", "tif", "tiff", "jxl", "heic", "ico", "svg",
    ];
    путь.extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| РАСШ.contains(&e.as_str()))
        && путь.is_file()
}

fn расширение_ссылки(url: &str) -> String {
    let без_запроса = url.split(['?', '#']).next().unwrap_or(url);
    Path::new(без_запроса)
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or_else(|| "img".to_string())
}

/// Файл в `~/.cache/parallax/pictures`, имя — по содержимому (или ссылке).
fn путь_в_кэше(ключ: &[u8], расш: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};
    let дом = std::env::var_os("HOME")?;
    let каталог = PathBuf::from(дом).join(".cache/parallax/pictures");
    std::fs::create_dir_all(&каталог).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ключ.hash(&mut h);
    Some(каталог.join(format!("{:016x}.{}", h.finish(), расш)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn список_ссылок() {
        let т = "# comment\r\nfile:///home/y/%D0%BA%D0%BE%D1%82%20big.png\r\nhttps://x.org/a.jpg\r\n";
        assert_eq!(
            разобрать_ссылки(т, false),
            vec![
                Ссылка::Файл(PathBuf::from("/home/y/кот big.png")),
                Ссылка::Сеть("https://x.org/a.jpg".into()),
            ]
        );
    }

    #[test]
    fn moz_url_через_строку() {
        let т = "https://x.org/a.png\nподпись\nhttps://x.org/b.png\nещё";
        assert_eq!(разобрать_ссылки(т, true).len(), 2);
    }

    #[test]
    fn выбор_типа() {
        let т = |v: &[&str]| выбрать_тип(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(т(&["text/plain", "text/uri-list"]).as_deref(), Some("text/uri-list"));
        assert_eq!(т(&["image/png", "text/html"]).as_deref(), Some("image/png"));
        assert_eq!(т(&["application/x-kde"]), None);
    }

    #[test]
    fn расширение() {
        assert_eq!(расширение_ссылки("https://a/b/c.JPG?w=1"), "jpg");
        assert_eq!(расширение_ссылки("https://a/b/c"), "img");
    }
}
