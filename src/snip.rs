//! Снимок экрана областью — то, что в Windows делает Win+Shift+S.
//!
//! **Почему не grim/slurp.** Раньше PrtScr был `grim - | wl-copy`, и на двух
//! мониторах это ломалось дважды. Во-первых, `grim` без `-o` снимает ВЕСЬ
//! layout, а холст parallax бесконечен и дома мониторов разнесены на
//! [`crate::monitors::ШАГ_ДОМА`] = 1 000 000 пикселей — «весь экран» в таких
//! координатах не имеет смысла. Во-вторых, выделение области рисует `slurp`
//! своей layer-поверхностью поверх ОДНОГО выхода и в тех же координатах
//! layout'а. Оба инструмента считают мир плоским прямоугольником, а у parallax это
//! неверно по построению.
//!
//! Поэтому выделение живёт внутри композитора: он один знает, где сейчас стоит
//! камера каждого монитора и какой кусок холста накрыт каким выходом.
//!
//! **Как это выглядит.** PrtScr затемняет экран, стрелка тянет рамку, отпустил
//! — прямоугольник ушёл в буфер обмена и в файл. Клик без протяжки (рамка
//! меньше [`МИНИМУМ`]) = весь монитор под курсором: так на одной клавише
//! остаётся и старое поведение «снимок всего экрана», ради которого не пришлось
//! заводить второй бинд.
//!
//! Сверху экрана — панель режимов, как у «Ножниц» Windows (26.09.2026):
//! «Область», «Окно» (подсвечивается окно под курсором, клик снимает его
//! видимую рамку), «Весь экран» (снимок сразу) и «×» — отмена.
//!
//! **Кадр снимается не здесь.** Пиксели живут в GlesRenderer, который
//! принадлежит циклу отрисовки в udev.rs, — тем же приёмом, что и
//! `screencopy::serve_pending`, запрос кладётся в [`Parallax::snip_ждёт`] и
//! обслуживается сразу после того, как кадр ушёл на монитор. Заодно это решает
//! вопрос «затемнение не должно попасть в снимок»: к моменту захвата выделение
//! уже снято, и рисуется чистый кадр.

use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Rectangle, Size};

use crate::state::Parallax;
use crate::тф;

/// Меньше этого по любой стороне — считаем, что протяжки не было и человек
/// просто щёлкнул: снимаем монитор целиком.
const МИНИМУМ: f64 = 8.0;

/// Что снимаем — как в «Ножницах» Windows: панель режимов сверху экрана.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Режим {
    /// Обвести прямоугольник мышью (клик без протяжки — весь экран).
    Область,
    /// Окно под курсором подсвечивается, клик снимает его видимую рамку.
    Окно,
    /// Весь монитор — сразу, по кнопке панели.
    Экран,
}

/// Кнопка панели режимов.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Кнопка {
    Режим(Режим),
    Отмена,
}

/// Идущее выделение области.
pub struct Выделение {
    /// Где нажали, в координатах ХОЛСТА. `None` — рамку ещё не начали тянуть
    /// (экран уже затемнён, кнопку не нажимали).
    pub начало: Option<Point<f64, Logical>>,
    /// Монитор, на котором началось выделение: рамку разрешено тянуть только
    /// по нему. Кадр берётся с одного выхода, и растянутая на два экрана рамка
    /// означала бы склейку двух снимков — этого сознательно не делаем.
    pub монитор: usize,
    /// Текущий режим панели.
    pub режим: Режим,
    /// Кнопка панели, на которой нажали ЛКМ. Срабатывает она на отпускании и
    /// только если курсор с неё не ушёл — как любая кнопка. Заодно отпускание
    /// не утекает клиенту: выделение живо, пока кнопку не отпустили.
    pub нажата_кнопка: Option<Кнопка>,
}

/// Масштаб шрифта панели и подписей.
pub const МАСШТАБ: i32 = 2;

/// Кнопки панели режимов в экранных пикселях монитора шириной `ширина` и
/// высотой `высота`, слева направо. Одна функция на отрисовку (udev.rs) и на
/// хит-тест (`snip_click`) — иначе кнопка нажималась бы не там, где нарисована.
pub fn панель(ширина: i32, высота: i32) -> Vec<(Rectangle<i32, Physical>, Кнопка, &'static str)> {
    const ПОЛЕ_X: i32 = 18;
    const ПОЛЕ_Y: i32 = 10;
    const ЗАЗОР: i32 = 6;
    let кнопки: [(Кнопка, &'static str); 4] = [
        (Кнопка::Режим(Режим::Область), crate::т!("Область", "Region")),
        (Кнопка::Режим(Режим::Окно), crate::т!("Окно", "Window")),
        (Кнопка::Режим(Режим::Экран), crate::т!("Весь экран", "Full screen")),
        (Кнопка::Отмена, "×"),
    ];
    let в = crate::text::height(МАСШТАБ) + ПОЛЕ_Y * 2;
    let ширины: Vec<i32> = кнопки.iter()
        .map(|(_, т)| crate::text::width_of(т, crate::text::Weight::Semi, МАСШТАБ) + ПОЛЕ_X * 2)
        .collect();
    let всего: i32 = ширины.iter().sum::<i32>() + ЗАЗОР * (кнопки.len() as i32 - 1);
    let mut x = (ширина - всего) / 2;
    let y = (высота / 24).max(12);
    кнопки.iter().zip(ширины).map(|(&(к, т), ш)| {
        let r = Rectangle::new((x, y).into(), (ш, в).into());
        x += ш + ЗАЗОР;
        (r, к, т)
    }).collect()
}

/// Рамка всей панели (подложка под кнопками).
pub fn рамка_панели(ширина: i32, высота: i32) -> Option<Rectangle<i32, Physical>> {
    const ПОЛЕ: i32 = 6;
    let кнопки = панель(ширина, высота);
    let (первая, последняя) = (кнопки.first()?.0, кнопки.last()?.0);
    Some(Rectangle::new(
        (первая.loc.x - ПОЛЕ, первая.loc.y - ПОЛЕ).into(),
        (
            последняя.loc.x + последняя.size.w - первая.loc.x + ПОЛЕ * 2,
            первая.size.h + ПОЛЕ * 2,
        ).into(),
    ))
}

/// Готовый запрос на кадр: ждёт ближайшей отрисовки нужного выхода.
pub struct Запрос {
    /// С какого выхода снимать.
    pub output: Output,
    /// Что вырезать, в ФИЗИЧЕСКИХ пикселях этого выхода.
    pub область: Rectangle<i32, Physical>,
}

impl Parallax {
    /// Идёт ли выделение прямо сейчас (по нему input.rs забирает себе мышь и
    /// Escape, а udev.rs рисует затемнение).
    pub fn snip_идёт(&self) -> bool {
        self.snip.is_some()
    }

    /// PrtScr: включить режим выделения. Второе нажатие — отмена, как у всех
    /// остальных тумблеров parallax (лаунчер, меню).
    pub fn snip_start(&mut self) {
        if self.snip.take().is_some() {
            tracing::info!("plx/snip: selection cancelled by pressing again");
            self.request_redraw();
            return;
        }
        self.snip = Some(Выделение {
            начало: None,
            монитор: self.курсор_монитор,
            режим: Режим::Область,
            нажата_кнопка: None,
        });
        tracing::info!("plx/snip: region selection, monitor {}", self.курсор_монитор + 1);
        self.request_redraw();
    }

    /// Камера и размер экрана монитора `номер`: у активного живая копия вида
    /// лежит в `viewport`, у остальных — в самом мониторе (monitors.rs). Без
    /// таблицы мониторов (winit/headless) — первый выход и живой вид.
    fn snip_вид(&self, номер: usize) -> Option<(Output, f64, f64, f64)> {
        let (output, вид) = match self.мониторы.get(номер) {
            Some(м) if номер != self.активный => (м.output.clone(), м.viewport.clone()),
            Some(м) => (м.output.clone(), self.viewport.clone()),
            None => (self.space.outputs().next()?.clone(), self.viewport.clone()),
        };
        Some((output, вид.cam_x, вид.cam_y, вид.zoom.max(0.01)))
    }

    /// Холст → экранные пиксели монитора `номер`.
    pub fn snip_в_экран(&self, номер: usize, p: Point<f64, Logical>) -> Option<(f64, f64)> {
        let (_, cx, cy, zoom) = self.snip_вид(номер)?;
        Some(((p.x - cx) * zoom, (p.y - cy) * zoom))
    }

    /// Размер экрана монитора `номер` в физических пикселях.
    fn snip_экран(&self, номер: usize) -> Option<(Output, Size<i32, Physical>)> {
        let (output, ..) = self.snip_вид(номер)?;
        let mode = output.current_mode()?;
        Some((output, mode.size))
    }

    /// Кнопка панели под курсором (панель живёт на мониторе выделения).
    pub fn snip_кнопка_под_курсором(&self) -> Option<Кнопка> {
        let в = self.snip.as_ref()?;
        if в.монитор != self.курсор_монитор {
            return None;
        }
        let (_, экран) = self.snip_экран(в.монитор)?;
        let (x, y) = self.snip_в_экран(в.монитор, self.pointer_location)?;
        let точка = Point::<i32, Physical>::from((x.floor() as i32, y.floor() as i32));
        панель(экран.w, экран.h).into_iter()
            .find(|(r, _, _)| r.contains(точка))
            .map(|(_, к, _)| к)
    }

    /// Режим «Окно»: видимая рамка окна под курсором в экранных пикселях
    /// монитора выделения — то, что уйдёт в снимок и что подсвечивается.
    ///
    /// Рамка ВИДИМАЯ (`xwin::видимый_размер`), а не то, что нарисовал клиент:
    /// упрямое окно (GTK, Electron) рисует буфер больше слота и режется
    /// рендером, и снимок по полному размеру прихватил бы кусок соседа.
    pub fn snip_окно(&self) -> Option<Rectangle<i32, Physical>> {
        let в = self.snip.as_ref()?;
        if в.режим != Режим::Окно || в.монитор != self.курсор_монитор {
            return None;
        }
        if self.snip_кнопка_под_курсором().is_some() {
            return None;
        }
        let (окно, _) = self.окно_под(self.pointer_location)?;
        let g = self.space.element_geometry(&окно)?;
        let размер = if self.is_fullscreen(&окно) {
            g.size
        } else {
            crate::xwin::видимый_размер(&окно, g.size)
        };
        let (_, экран) = self.snip_экран(в.монитор)?;
        let (x0, y0) = self.snip_в_экран(в.монитор, g.loc.to_f64())?;
        let (_, _, _, zoom) = self.snip_вид(в.монитор)?;
        let рамка = Rectangle::<i32, Physical>::new(
            (x0.round() as i32, y0.round() as i32).into(),
            (
                (размер.w as f64 * zoom).round().max(1.0) as i32,
                (размер.h as f64 * zoom).round().max(1.0) as i32,
            ).into(),
        );
        рамка.intersection(Rectangle::from_size(экран))
            .filter(|r| r.size.w > 0 && r.size.h > 0)
    }

    /// Курсор сдвинулся. Пока рамку не начали тянуть, панель и затемнение
    /// переезжают на монитор под курсором — снимать будем там, куда смотрят.
    pub fn snip_движение(&mut self) {
        let монитор = self.курсор_монитор;
        if let Some(в) = self.snip.as_mut() {
            if в.начало.is_none() && в.нажата_кнопка.is_none() {
                в.монитор = монитор;
            }
        }
    }

    /// Кнопка мыши во время выделения. `true` — клик съеден.
    ///
    /// ЛКМ по кнопке панели меняет режим (срабатывает на отпускании). Дальше
    /// по режиму: «Область» — нажали и тянем рамку, «Окно» — клик по окну,
    /// «Весь экран» — клик где угодно. ПКМ отменяет.
    pub fn snip_click(&mut self, левая: bool, нажата: bool) -> bool {
        if self.snip.is_none() {
            return false;
        }
        if !левая {
            // ПКМ (и любая другая кнопка) — отмена. Только на нажатии: иначе
            // отпускание той же кнопки прилетело бы уже в пустоту.
            if нажата {
                self.snip = None;
                tracing::info!("plx/snip: cancelled");
                self.request_redraw();
            }
            return true;
        }
        let кнопка = self.snip_кнопка_под_курсором();
        if нажата {
            let точка = self.pointer_location;
            let монитор = self.курсор_монитор;
            if let Some(в) = self.snip.as_mut() {
                if кнопка.is_some() {
                    в.нажата_кнопка = кнопка;
                } else if в.режим == Режим::Область {
                    в.начало = Some(точка);
                    в.монитор = монитор;
                }
            }
            self.request_redraw();
            return true;
        }

        // Отпускание.
        let (режим, нажатая) = match self.snip.as_mut() {
            Some(в) => (в.режим, в.нажата_кнопка.take()),
            None => return true,
        };
        if let Some(нажатая) = нажатая {
            // Кнопка срабатывает, только если с неё не ушли.
            if кнопка == Some(нажатая) {
                match нажатая {
                    Кнопка::Отмена => {
                        self.snip = None;
                        tracing::info!("plx/snip: cancelled (panel)");
                    }
                    Кнопка::Режим(Режим::Экран) => self.snip_снять(None),
                    Кнопка::Режим(новый) => {
                        if let Some(в) = self.snip.as_mut() {
                            в.режим = новый;
                            в.начало = None;
                        }
                        tracing::info!("plx/snip: mode {:?}", новый);
                    }
                }
            }
            self.request_redraw();
            return true;
        }
        match режим {
            Режим::Область => self.snip_finish(),
            Режим::Экран => self.snip_снять(None),
            Режим::Окно => match self.snip_окно() {
                Some(рамка) => self.snip_снять(Some(рамка)),
                // Мимо окон — ничего не снимаем и ждём клика по окну, как
                // «Ножницы» в режиме окна.
                None => self.request_redraw(),
            },
        }
        true
    }

    /// Закончить выделение и попросить кадр: `область` в экранных пикселях
    /// монитора выделения, `None` — весь монитор.
    fn snip_снять(&mut self, область: Option<Rectangle<i32, Physical>>) {
        let Some(выделение) = self.snip.take() else { return };
        self.request_redraw();
        let Some((output, экран)) = self.snip_экран(выделение.монитор) else { return };
        let полный = Rectangle::from_size(экран);
        let область = область
            .and_then(|r| r.intersection(полный))
            .filter(|r| r.size.w > 0 && r.size.h > 0)
            .unwrap_or(полный);
        tracing::info!(
            "plx/snip: capturing {}×{} from {} (position {},{})",
            область.size.w, область.size.h, output.name(), область.loc.x, область.loc.y,
        );
        self.snip_ждёт = Some(Запрос { output, область });
        // Кадр снимается ПОСЛЕ отрисовки, а отрисовку надо ещё попросить:
        // на неподвижном экране parallax кадров не рисует вовсе.
        self.request_redraw();
    }

    /// Отпустили кнопку в режиме «Область»: превращаем рамку в запрос кадра.
    fn snip_finish(&mut self) {
        let Some(в) = self.snip.as_ref() else { return };
        let монитор = в.монитор;
        let рамка = в.начало.and_then(|начало| {
            let (x0, y0) = self.snip_в_экран(монитор, начало)?;
            let (x1, y1) = self.snip_в_экран(монитор, self.pointer_location)?;
            let (w, h) = ((x1 - x0).abs(), (y1 - y0).abs());
            // Клик без протяжки — весь монитор.
            (w >= МИНИМУМ && h >= МИНИМУМ).then(|| Rectangle::<i32, Physical>::new(
                (x0.min(x1).round() as i32, y0.min(y1).round() as i32).into(),
                (w.round().max(1.0) as i32, h.round().max(1.0) as i32).into(),
            ))
        });
        // Рамку могли увести за край экрана (курсор у parallax ходит по
        // холсту, а не по монитору) — `snip_снять` режет её по выходу.
        self.snip_снять(рамка);
    }

    /// Escape во время выделения.
    pub fn snip_cancel(&mut self) {
        if self.snip.take().is_some() {
            tracing::info!("plx/snip: cancelled (Escape)");
            self.request_redraw();
        }
    }
}

/// Отдать ждущий снимок: зовётся из цикла отрисовки сразу после кадра, теми же
/// элементами, что ушли на монитор (но БЕЗ курсора — как в Windows).
pub fn serve<E>(
    state: &mut Parallax,
    output: &Output,
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    elements: &[E],
) where
    E: smithay::backend::renderer::element::RenderElement<
        smithay::backend::renderer::gles::GlesRenderer,
    >,
{
    // Снимаем только с того выхода, для которого запрос и делался: на втором
    // мониторе кадр другой, и без этой проверки снимок доставался бы тому, кто
    // отрисовался первым.
    if state.snip_ждёт.as_ref().is_none_or(|з| з.output != *output) {
        return;
    }
    let Some(запрос) = state.snip_ждёт.take() else { return };
    let Some(mode) = output.current_mode() else { return };
    let экран = Size::<i32, smithay::utils::Buffer>::from((mode.size.w, mode.size.h));
    let Some(кадр) = crate::screencopy::capture(renderer, output, elements, экран) else {
        tracing::warn!("plx/snip: the frame was not captured");
        return;
    };
    let (w, h) = (запрос.область.size.w, запрос.область.size.h);
    let вырез = вырезать(&кадр, экран.w, экран.h, запрос.область);
    сохранить(вырез, w as u32, h as u32, state);
}

/// Вырезать прямоугольник из плотного BGRA-кадра.
fn вырезать(кадр: &[u8], sw: i32, sh: i32, обл: Rectangle<i32, Physical>) -> Vec<u8> {
    let (x0, y0) = (обл.loc.x.clamp(0, sw), обл.loc.y.clamp(0, sh));
    let (w, h) = (обл.size.w.min(sw - x0).max(0), обл.size.h.min(sh - y0).max(0));
    let mut out = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let src = (((y0 + y) * sw + x0) * 4) as usize;
        let dst = (y * w * 4) as usize;
        out[dst..dst + (w * 4) as usize].copy_from_slice(&кадр[src..src + (w * 4) as usize]);
    }
    out
}

/// Куда класть снимки. Тот же каталог, что у plx-wall и прочих: `~/Pictures`.
fn каталог() -> std::path::PathBuf {
    // parallax запускается через `sudo openvt`, поэтому HOME=/root — файл лёг бы
    // туда, где его никто не найдёт. Та же поправка, что в config::config_path.
    let дом = match std::env::var("SUDO_USER") {
        Ok(u) if !u.is_empty() && u != "root" => format!("/home/{u}"),
        _ => std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()),
    };
    std::path::PathBuf::from(дом).join("Pictures/Screenshots")
}

/// «screenshot-20260831-174204.png». Имя всегда английское и от языка
/// интерфейса НЕ зависит: файлы потом ищут глазами в каталоге и сортируют,
/// и переехавшее посреди истории имя сломало бы и то, и другое.
fn имя_файла() -> String {
    match crate::bar::local_time() {
        Some(tm) => format!(
            "screenshot-{:04}{:02}{:02}-{:02}{:02}{:02}.png",
            tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday,
            tm.tm_hour, tm.tm_min, tm.tm_sec,
        ),
        None => "screenshot.png".to_string(),
    }
}

/// PNG из пикселей `capture` (Argb8888 — в памяти B,G,R,A).
pub(crate) fn png_bytes(пиксели: &[u8], w: u32, h: u32) -> Option<Vec<u8>> {
    let mut rgba = пиксели.to_vec();
    for px in rgba.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header()
            .map_err(|e| tracing::warn!("plx/snip: PNG: {}", e)).ok()?;
        writer.write_image_data(&rgba)
            .map_err(|e| tracing::warn!("plx/snip: PNG: {}", e)).ok()?;
    }
    Some(out)
}

/// Записать файл и положить картинку в буфер обмена.
///
/// В буфер кладём через `wl-copy`, а не своим источником данных: снимок должен
/// пережить закрытие того, кто его сделал, а «тем, кто сделал», здесь оказался
/// бы сам композитор — свой источник он держал бы до конца сессии, и первая же
/// копия текста в другом приложении молча выкинула бы картинку. `wl-copy` для
/// этого и написан: он форкается, держит буфер и умирает, когда буфер заняли.
fn сохранить(пиксели: Vec<u8>, w: u32, h: u32, state: &Parallax) {
    if w == 0 || h == 0 {
        return;
    }
    let Some(png) = png_bytes(&пиксели, w, h) else { return };
    let каталог = каталог();
    if let Err(e) = std::fs::create_dir_all(&каталог) {
        tracing::warn!("plx/snip: {}: {}", каталог.display(), e);
        return;
    }
    let путь = каталог.join(имя_файла());
    if let Err(e) = std::fs::write(&путь, &png) {
        tracing::warn!("plx/snip: {}: {}", путь.display(), e);
        return;
    }
    // Файл принадлежит пользователю, а не root: parallax крутится под sudo, и
    // иначе снимки было бы не удалить из файлового менеджера.
    вернуть_владельца(&путь);
    tracing::info!("plx/snip: {} ({}×{})", путь.display(), w, h);
    let экранированный = путь.to_string_lossy().replace('\'', "'\\''");
    state.spawn(&format!("wl-copy -t image/png < '{экранированный}'"));
    state.spawn(&тф!(
        "notify-send -a parallax -i '{экранированный}' 'Снимок экрана' '{}×{} — в буфере обмена'", "notify-send -a parallax -i '{экранированный}' 'Screenshot' '{}×{} — copied to the clipboard'",
        w, h,
    ));
}

/// chown на SUDO_UID/SUDO_GID, если parallax поднят через sudo.
fn вернуть_владельца(путь: &std::path::Path) {
    let (Ok(uid), Ok(gid)) = (std::env::var("SUDO_UID"), std::env::var("SUDO_GID")) else {
        return;
    };
    let (Ok(uid), Ok(gid)) = (uid.parse::<u32>(), gid.parse::<u32>()) else { return };
    let Ok(c) = std::ffi::CString::new(путь.as_os_str().as_encoded_bytes()) else { return };
    // SAFETY: строка нуль-терминирована и жива на время вызова.
    unsafe { libc::chown(c.as_ptr(), uid, gid) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Вырез не должен ни выходить за кадр, ни съезжать по строкам: это ровно
    /// та арифметика, из-за которой снимок области получается «в полоску».
    #[test]
    fn вырез_берёт_нужный_прямоугольник() {
        // Кадр 4×3, в каждом пикселе номер = y*4+x во всех четырёх байтах.
        let mut кадр = vec![0u8; 4 * 3 * 4];
        for y in 0..3 {
            for x in 0..4 {
                let n = (y * 4 + x) as u8;
                for b in 0..4 {
                    кадр[((y * 4 + x) * 4 + b) as usize] = n;
                }
            }
        }
        let обл = Rectangle::<i32, Physical>::new((1, 1).into(), (2, 2).into());
        let out = вырезать(&кадр, 4, 3, обл);
        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!(out[0], 5);   // (1,1)
        assert_eq!(out[4], 6);   // (2,1)
        assert_eq!(out[8], 9);   // (1,2)
        assert_eq!(out[12], 10); // (2,2)
    }

    /// Прямоугольник, вылезший за край экрана, режется по кадру, а не читает
    /// чужую память.
    #[test]
    fn вырез_зажимается_экраном() {
        let кадр = vec![7u8; 4 * 3 * 4];
        let обл = Rectangle::<i32, Physical>::new((3, 2).into(), (10, 10).into());
        let out = вырезать(&кадр, 4, 3, обл);
        assert_eq!(out.len(), 1 * 1 * 4);
        assert_eq!(out[0], 7);
    }

    #[test]
    fn png_собирается() {
        let пиксели = vec![255u8; 2 * 2 * 4];
        let png = png_bytes(&пиксели, 2, 2).expect("PNG");
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);
    }
}
