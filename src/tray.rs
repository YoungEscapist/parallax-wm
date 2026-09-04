//! Полка состояния: вертикальная полосочка справа от панели столов и ряд,
//! который из неё выезжает — блютуз, вайфай, звук, батарея, питание.
//!
//! Почему в самом композиторе — по той же причине, что и блютуз (см.
//! bluetooth.rs): в сессии parallax нет ни панели, ни трея, а «убавить звук» и
//! «уйти в сон» нужны каждый день.
//!
//! Сама полка НЕ ходит в железо: вайфай живёт в wifi.rs, звук в audio.rs,
//! блютуз в bluetooth.rs — здесь только раскладка, отрисовка и разбор кликов.
//! Своё у полки одно — батарея: её отдаёт `/sys/class/power_supply`, никакой
//! шины и никакого отдельного модуля ради двух файлов не нужно.
//!
//! Опрос вайфая и блютуза идёт, только пока полка (или соответствующее меню)
//! открыта, — см. `sync_watch`. Заряд и звук опрашиваются ВСЕГДА, просто редко:
//! их показывает панель сверху (см. bar.rs), и застывший на панели процент
//! выглядит как сломанный индикатор.

use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel;

/// Как часто перечитываем заряд, пока полка открыта.
const POLL: Duration = Duration::from_millis(2000);
/// И как часто — пока закрыта. Заряд теперь показывает панель (см. bar.rs),
/// то есть он нужен всегда; но два файла в /sys ради процента, который за
/// полминуты меняется на единицу, чаще читать незачем.
const POLL_BG: Duration = Duration::from_secs(30);
/// Сколько кнопка питания остаётся взведённой до второго клика.
const ARM_TTL: Duration = Duration::from_secs(4);
/// Сколько держим сообщение о результате команды.
const NOTICE_TTL: Duration = Duration::from_secs(4);

#[derive(Clone, Debug, PartialEq)]
pub struct Battery {
    pub percent: u8,
    /// Заряжается или уже заряжена.
    pub charging: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    /// None — не ноутбук: батареи в `/sys/class/power_supply` нет.
    pub battery: Option<Battery>,
}

/// Что можно сделать с машиной из полки.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction {
    Off,
    Reboot,
    Sleep,
}

impl PowerAction {
    fn command(self) -> (&'static str, &'static str) {
        match self {
            PowerAction::Off => ("poweroff", "poweroff"),
            PowerAction::Reboot => ("reboot", "reboot"),
            // zzz — усыпляющая обёртка elogind, она же есть в Void без systemd.
            PowerAction::Sleep => ("suspend", "zzz"),
        }
    }

    pub fn human(self) -> &'static str {
        match self {
            PowerAction::Off => "power off",
            PowerAction::Reboot => "reboot",
            PowerAction::Sleep => "sleep",
        }
    }
}

pub enum Event {
    State(Snapshot),
    Notice(String),
}

#[derive(Debug)]
pub enum Cmd {
    /// Опрашивать заряд (полка открыта) или спать (закрыта).
    Watch(bool),
    Power(PowerAction),
}

// ── Поток опроса ─────────────────────────────────────────────────────────────

pub fn spawn(to_plx: channel::Sender<Event>) -> Option<mpsc::Sender<Cmd>> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let ok = std::thread::Builder::new()
        .name("plx-tray".into())
        .spawn(move || serve(to_plx, rx))
        .is_ok();
    ok.then_some(tx)
}

fn serve(to_plx: channel::Sender<Event>, rx: mpsc::Receiver<Cmd>) {
    let mut last: Option<Snapshot> = None;
    let mut watching = false;
    let mut next_poll = Instant::now();

    loop {
        // Насовсем поток больше не засыпает: заряд висит в панели постоянно, а
        // раньше при закрытой полке снимков не было вовсе — процент застывал
        // на том, что успели прочитать при последнем открытии.
        let cmd = match rx.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
            Ok(cmd) => Some(cmd),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        };

        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Watch(on) => {
                    watching = on;
                    last = None;
                }
                Cmd::Power(action) => {
                    let text = do_power(action);
                    let _ = to_plx.send(Event::Notice(text));
                }
            }
        }

        let snap = Snapshot { battery: read_battery() };
        next_poll = Instant::now() + if watching { POLL } else { POLL_BG };
        if last.as_ref() != Some(&snap) {
            last = Some(snap.clone());
            if to_plx.send(Event::State(snap)).is_err() {
                return;
            }
        }
    }
}

fn run(bin: &str, args: &[&str]) -> bool {
    match Command::new(bin).args(args).status() {
        Ok(st) => st.success(),
        Err(err) => {
            tracing::warn!("plx/tray: {} did not start: {}", bin, err);
            false
        }
    }
}

/// Выключение/перезагрузка/сон. Сначала через loginctl (elogind спросит
/// polkit), при неудаче — прямой командой: в сессии без polkit-агента
/// loginctl молча возвращает ошибку, и кнопка выглядела бы сломанной.
fn do_power(action: PowerAction) -> String {
    let (sub, fallback) = action.command();
    if run("loginctl", &[sub]) {
        return action.human().to_string();
    }
    tracing::warn!("plx/tray: loginctl {} did not work — trying {}", sub, fallback);
    if run(fallback, &[]) {
        return action.human().to_string();
    }
    format!("{} failed: neither loginctl nor {}", action.human(), fallback)
}

fn read_battery() -> Option<Battery> {
    for entry in std::fs::read_dir("/sys/class/power_supply").ok()?.flatten() {
        let path = entry.path();
        let field = |name: &str| {
            std::fs::read_to_string(path.join(name))
                .map(|s| s.trim().to_string())
                .unwrap_or_default()
        };
        if field("type") != "Battery" {
            continue;
        }
        // Батарейки мышей и наушников тоже лежат тут, но со scope=Device —
        // показывать их как батарею НОУТБУКА нельзя.
        if field("scope") == "Device" {
            continue;
        }
        let Ok(percent) = field("capacity").parse::<u8>() else { continue };
        let status = field("status");
        return Some(Battery {
            percent: percent.min(100),
            charging: matches!(status.as_str(), "Charging" | "Full"),
        });
    }
    None
}

// ── Раскладка ────────────────────────────────────────────────────────────────
//
// ОДНА функция на всё: и рисование (udev.rs), и попадание клика считают ячейки
// ею. Держать вторую копию геометрии в хит-тесте — ровно тот способ, каким уже
// однажды разъехались клики по окнам: на экране одно, в проверке другое.

/// Прямоугольник экрана. Один на панель и полку — они соседи и считаются
/// одними и теми же долями высоты (см. `crate::bar`).
pub use crate::bar::Rect;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum CellKind {
    Bluetooth,
    Wifi,
    /// Значок звука: слева — меню устройств, справа — немота.
    Volume,
    /// Шкала громкости — клик ставит уровень по месту нажатия.
    VolumeSlider,
    Battery,
    Power(PowerAction),
}

#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub kind: CellKind,
    pub rect: Rect,
}

pub struct Layout {
    /// Фон выехавшего ряда; None — полка закрыта.
    pub panel: Option<Rect>,
    pub cells: Vec<Cell>,
}

/// Ячейки полки для экрана шириной `screen_w`.
///
/// Полка выезжает ВНИЗ из правого острова панели и прижата к тому же правому
/// краю. Раньше она выезжала вбок от центральной таблетки, но с тех пор бар
/// разъехался на три острова (см. `crate::bar`), правый из которых сам стоит у
/// края: вбок ей теперь просто некуда — там край экрана.
///
/// Полосочки-хвата здесь больше нет: она живёт в правом острове как обычная
/// ячейка панели (`bar::Cell::Handle`), потому что панель и полка не должны
/// спорить за один и тот же клик.
///
/// Все размеры — доли от высоты острова (`bar::H`), поэтому полка уменьшается
/// вместе с панелью и не требует отдельной настройки.
/// `hide` — доля убранности ПАНЕЛИ (см. `bar::island_y`). Полка висит под
/// панелью и обязана уезжать вместе с ней: иначе при входе в обзор столов
/// панель уходит вверх, а её полка остаётся висеть в пустоте.
pub fn layout(open: bool, has_battery: bool, screen_w: i32, hide: f64, выезд: f64) -> Layout {
    let h = crate::bar::H;
    let gap = h / 6; // зазор между ячейками
    let pad = h / 6; // поля внутри выехавшего ряда
    let slider_w = h * 2;
    // Строка полки — под панелью, с тем же зазором, что и внутри неё.
    //
    // `выезд` (0..1, ведёт anim::tick) поднимает её обратно ПОД панель: на нуле
    // ряд стоит ровно там, где остров, то есть спрятан за ним — полка уезжает
    // туда, откуда её открыли, а не пропадает с экрана одним кадром. Ход равен
    // h + gap, ровно расстоянию от низа панели до места полки.
    //
    // Считается ЗДЕСЬ, а не в отрисовке, намеренно: этой же раскладкой ловит
    // клики `tray_click`, и разъехаться им нельзя — иначе полдороги кнопки
    // нажимались бы не там, где нарисованы.
    let выезд = выезд.clamp(0.0, 1.0);
    let сдвиг = ((1.0 - выезд) * (h + gap) as f64).round() as i32;
    let y = crate::bar::island_y(hide) + crate::bar::H + gap - сдвиг;

    let mut cells: Vec<Cell> = Vec::new();
    // Пока полка ЕДЕТ (выезд > 0), она обязана быть в раскладке, даже если
    // тумблер уже выключен: иначе закрытие срезало бы её мгновенно.
    if !open && выезд <= 0.0 {
        return Layout { panel: None, cells };
    }

    // Ширины ячеек по порядку: значки квадратные, шкала громкости длинная.
    let mut kinds = vec![
        (CellKind::Bluetooth, h),
        (CellKind::Wifi, h),
        (CellKind::Volume, h),
        (CellKind::VolumeSlider, slider_w),
    ];
    if has_battery {
        kinds.push((CellKind::Battery, h + h / 3));
    }
    kinds.push((CellKind::Power(PowerAction::Sleep), h));
    kinds.push((CellKind::Power(PowerAction::Reboot), h));
    kinds.push((CellKind::Power(PowerAction::Off), h));

    let inner: i32 = kinds.iter().map(|(_, w)| w).sum::<i32>() + gap * (kinds.len() as i32 - 1);
    let panel_w = inner + pad * 2;
    // Прижата к правому краю, под своим островом. На узком экране (ряд шире
    // экрана) не уезжаем за левый край — там его вовсе не достать мышью.
    let panel_x = (screen_w - crate::bar::EDGE - panel_w).max(0);
    let panel = Rect { x: panel_x, y, w: panel_w, h };

    let mut cx = panel_x + pad;
    for (kind, w) in kinds {
        cells.push(Cell { kind, rect: Rect { x: cx, y, w, h } });
        cx += w + gap;
    }
    Layout { panel: Some(panel), cells }
}

// ── Состояние в композиторе ──────────────────────────────────────────────────

pub struct TrayUi {
    tx: mpsc::Sender<Cmd>,
    pub snap: Snapshot,
    pub open: bool,
    /// Взведённая кнопка питания и когда её взвели: выключение и перезагрузку
    /// делаем со второго клика. Один промах мимо шкалы громкости не должен
    /// стоить человеку сеанса.
    pub armed: Option<(PowerAction, Instant)>,
    notice: Option<(String, Instant)>,
}

impl TrayUi {
    /// Взведена ли эта кнопка прямо сейчас (просроченные не считаются).
    pub fn is_armed(&self, action: PowerAction) -> bool {
        self.armed
            .is_some_and(|(a, at)| a == action && at.elapsed() < ARM_TTL)
    }

    pub fn notice_text(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_TTL)
            .map(|(t, _)| t.as_str())
    }
}

impl crate::state::Parallax {
    pub fn init_tray(&mut self, tx: mpsc::Sender<Cmd>) {
        self.tray = Some(TrayUi {
            tx,
            snap: Snapshot::default(),
            open: false,
            armed: None,
            notice: None,
        });
    }

    pub fn handle_tray_event(&mut self, event: Event) {
        let Some(tray) = self.tray.as_mut() else { return };
        match event {
            Event::State(snap) => tray.snap = snap,
            Event::Notice(text) => {
                tracing::info!("plx/tray: {}", text);
                tray.notice = Some((text, Instant::now()));
            }
        }
        self.request_redraw();
    }

    pub fn tray_open(&self) -> bool {
        self.tray.as_ref().is_some_and(|t| t.open)
    }

    fn tray_send(&self, cmd: Cmd) {
        if let Some(tray) = self.tray.as_ref() {
            if tray.tx.send(cmd).is_err() {
                tracing::warn!("plx/tray: polling thread not responding");
            }
        }
    }

    /// Открыть/закрыть полку (действие `tray_menu` и клик по полосочке).
    pub fn tray_toggle(&mut self) {
        let Some(tray) = self.tray.as_mut() else { return };
        tray.open = !tray.open;
        tray.armed = None;
        let open = tray.open;
        self.tray_send(Cmd::Watch(open));
        // Вайфай и звук показывает полка, а спрашивают о них свои модули —
        // им и говорим, что теперь на них смотрят.
        self.wifi_sync_watch();
        self.audio_sync_watch();
        self.request_redraw();
    }

    pub fn tray_set_open(&mut self, open: bool) {
        if self.tray_open() != open {
            self.tray_toggle();
        }
    }

    /// Снять протухший взвод кнопки питания. `true` — что-то изменилось и
    /// кадр надо перерисовать: без этого подсвеченная кнопка так и висела бы
    /// на экране, хотя нажатие на неё уже не сработает (см. `is_armed`).
    /// Зовётся из анимационного тика — он один тикает и на статичном экране.
    pub fn tray_expire_armed(&mut self) -> bool {
        let Some(tray) = self.tray.as_mut() else { return false };
        match tray.armed {
            Some((_, at)) if at.elapsed() >= ARM_TTL => {
                tray.armed = None;
                true
            }
            _ => false,
        }
    }

    /// Клик мышью в экранных пикселях. `right` — правая кнопка.
    /// `true` — клик съеден полкой.
    pub fn tray_click(
        &mut self,
        pos: smithay::utils::Point<f64, smithay::utils::Physical>,
        right: bool,
    ) -> bool {
        let Some(tray) = self.tray.as_ref() else { return false };
        let cells = layout(
            tray.open, tray.snap.battery.is_some(), self.screen_size().w, self.bar_hide,
            self.shelf_anim,
        ).cells;
        let Some(cell) = cells.into_iter().find(|c| c.rect.hit(pos.x, pos.y)) else {
            // Клик мимо открытой полки закрывает её, но НЕ съедается: человек
            // целился в окно, и окно должно получить свой клик.
            if tray.open {
                self.tray_toggle();
            }
            return false;
        };

        // Любой клик не по взведённой кнопке снимает взвод.
        let armed = tray.armed;
        if let Some(t) = self.tray.as_mut() {
            t.armed = None;
        }

        match cell.kind {
            // Меню большие и живут по центру экрана — полку под ними
            // закрываем, чтобы не спорили за клики.
            CellKind::Bluetooth => {
                self.tray_set_open(false);
                self.bt_toggle_menu();
            }
            CellKind::Wifi if right => {
                let on = self.wifi_snapshot().is_some_and(|s| s.enabled);
                self.wifi_send(crate::wifi::Cmd::Radio(!on));
            }
            CellKind::Wifi => {
                self.tray_set_open(false);
                self.wifi_toggle_menu();
            }
            CellKind::Volume if right => self.audio_send(crate::audio::Cmd::MuteToggle),
            CellKind::Volume => {
                self.tray_set_open(false);
                self.audio_toggle_menu();
            }
            CellKind::VolumeSlider => {
                let level = ((pos.x - cell.rect.x as f64) / cell.rect.w as f64).clamp(0.0, 1.0);
                self.audio_send(crate::audio::Cmd::SetVolume(level as f32));
            }
            // Батарея — только показ, нажимать не на что.
            CellKind::Battery => {}
            CellKind::Power(action) => {
                let ready = armed.is_some_and(|(a, at)| a == action && at.elapsed() < ARM_TTL);
                if ready {
                    self.tray_send(Cmd::Power(action));
                } else if let Some(t) = self.tray.as_mut() {
                    t.armed = Some((action, Instant::now()));
                    tracing::info!("plx/tray: {} — waiting for the second click", action.human());
                }
            }
        }
        self.request_redraw();
        true
    }

    /// Esc при открытой полке закрывает её. `true` — клавиша съедена.
    pub fn tray_key(&mut self, keysym: u32) -> bool {
        if !self.tray_open() || keysym != smithay::input::keyboard::keysyms::KEY_Escape {
            return false;
        }
        self.tray_toggle();
        true
    }
}
