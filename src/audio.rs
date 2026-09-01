//! Звук внутри композитора: список устройств вывода и ввода, громкость, немота.
//!
//! Через `pactl` (модуль совместимости PipeWire — он в сессии уже поднят, см.
//! launch_native.sh). Разбирать узлы PipeWire напрямую значило бы написать
//! сотни строк ради того, что здесь делают три вызова: `pactl list`,
//! `set-default-*`, `move-*`. Одна команда на всё, а не wpctl для громкости и
//! pactl для устройств, — иначе два разных представления об одном и том же.
//!
//! Вывод pactl ЛОКАЛИЗОВАН, поэтому все вызовы идут с LC_ALL=C: иначе на
//! русской локали пропадают ключи `Name:`/`Mute:`, по которым мы и разбираем.

use std::process::Command;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use smithay::reexports::calloop::channel;

const POLL_IDLE: Duration = Duration::from_millis(1200);
const POLL_MENU: Duration = Duration::from_millis(800);
/// Пока ни полка, ни меню не открыты, громкость всё равно нужна: её показывает
/// панель (см. bar.rs). Опрос идёт редко — каждый заход это запуск `pactl`,
/// и раз в секунду ради процентов в баре запускать процесс незачем.
const POLL_BG: Duration = Duration::from_millis(5000);

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    /// Системное имя (им оперируют команды pactl).
    pub name: String,
    /// Человеческое имя для списка.
    pub description: String,
    pub default: bool,
    pub muted: bool,
    /// 0.0..1.0.
    pub volume: f32,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub sinks: Vec<Device>,
    pub sources: Vec<Device>,
}

impl Snapshot {
    pub fn default_sink(&self) -> Option<&Device> {
        self.sinks.iter().find(|d| d.default)
    }

    pub fn default_source(&self) -> Option<&Device> {
        self.sources.iter().find(|d| d.default)
    }

    /// Громкость и немота устройства вывода — их показывает полка.
    pub fn volume(&self) -> Option<(f32, bool)> {
        self.default_sink().map(|d| (d.volume, d.muted))
    }
}

pub enum Event {
    State(Snapshot),
    Notice(String),
}

#[derive(Debug)]
pub enum Cmd {
    Watch { tray: bool, menu: bool },
    SetVolume(f32),
    MuteToggle,
    /// Сделать устройство основным. `sink` — вывод, иначе ввод.
    SetDefault { sink: bool, name: String },
}

// ── Поток опроса ─────────────────────────────────────────────────────────────

pub fn spawn(to_dawn: channel::Sender<Event>) -> Option<mpsc::Sender<Cmd>> {
    let (tx, rx) = mpsc::channel::<Cmd>();
    let ok = std::thread::Builder::new()
        .name("dawn-audio".into())
        .spawn(move || serve(to_dawn, rx))
        .is_ok();
    ok.then_some(tx)
}

fn serve(to_dawn: channel::Sender<Event>, rx: mpsc::Receiver<Cmd>) {
    let mut last: Option<Snapshot> = None;
    let (mut tray, mut menu) = (false, false);
    let mut next_poll = Instant::now();

    loop {
        // Поток больше НЕ засыпает насовсем: проценты громкости висят в панели
        // постоянно (см. bar.rs), и раньше они там застывали намертво — пока
        // полку не открыли, снимков просто не было.
        let cmd = match rx.recv_timeout(next_poll.saturating_duration_since(Instant::now())) {
            Ok(cmd) => Some(cmd),
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        };

        if let Some(cmd) = cmd {
            match cmd {
                Cmd::Watch { tray: t, menu: m } => {
                    tray = t;
                    menu = m;
                    last = None;
                }
                Cmd::SetVolume(level) => {
                    let pct = (level.clamp(0.0, 1.0) * 100.0).round() as i32;
                    pactl(&["set-sink-volume", "@DEFAULT_SINK@", &format!("{pct}%")]);
                }
                Cmd::MuteToggle => {
                    pactl(&["set-sink-mute", "@DEFAULT_SINK@", "toggle"]);
                }
                Cmd::SetDefault { sink, name } => {
                    let text = switch_default(sink, &name);
                    let _ = to_dawn.send(Event::Notice(text));
                }
            }
        }

        let snap = read_state();
        next_poll = Instant::now()
            + if menu {
                POLL_MENU
            } else if tray {
                POLL_IDLE
            } else {
                POLL_BG
            };
        if last.as_ref() != Some(&snap) {
            last = Some(snap.clone());
            if to_dawn.send(Event::State(snap)).is_err() {
                return;
            }
        }
    }
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl")
        .env("LC_ALL", "C")
        .args(args)
        .output()
        .map_err(|e| tracing::warn!("dawn/audio: pactl не запустился: {}", e))
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).to_string())
}

/// Переключить основное устройство И перевести на него уже играющие потоки:
/// без второго шага музыка продолжает идти в старые колонки, и человек решает,
/// что переключение не сработало.
fn switch_default(sink: bool, name: &str) -> String {
    let (set, list, mv) = if sink {
        ("set-default-sink", "sink-inputs", "move-sink-input")
    } else {
        ("set-default-source", "source-outputs", "move-source-output")
    };
    if pactl(&[set, name]).is_none() {
        return "could not switch device".into();
    }
    let mut moved = 0;
    if let Some(text) = pactl(&["list", "short", list]) {
        for line in text.lines() {
            let Some(id) = line.split('\t').next() else { continue };
            if id.trim().is_empty() {
                continue;
            }
            if pactl(&[mv, id.trim(), name]).is_some() {
                moved += 1;
            }
        }
    }
    let what = if sink { "output" } else { "input" };
    format!("{what} switched, {moved} stream(s) moved")
}

fn read_state() -> Snapshot {
    let info = pactl(&["info"]).unwrap_or_default();
    let field = |key: &str| {
        info.lines()
            .find_map(|l| l.strip_prefix(key))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    Snapshot {
        sinks: parse_devices(&pactl(&["list", "sinks"]).unwrap_or_default(), &field("Default Sink:")),
        sources: parse_devices(&pactl(&["list", "sources"]).unwrap_or_default(), &field("Default Source:")),
    }
}

/// Разбор `pactl list sinks|sources`: блоки, разделённые пустой строкой.
fn parse_devices(text: &str, default_name: &str) -> Vec<Device> {
    let mut out = Vec::new();
    let mut cur: Option<Device> = None;
    let push = |cur: &mut Option<Device>, out: &mut Vec<Device>| {
        if let Some(d) = cur.take() {
            // Мониторы — это «то, что играет в колонках», обратно к нам. Как
            // микрофон они бесполезны и только засоряют список.
            if !d.name.ends_with(".monitor") && !d.name.is_empty() {
                out.push(d);
            }
        }
    };

    for line in text.lines() {
        let t = line.trim();
        if t.starts_with("Sink #") || t.starts_with("Source #") {
            push(&mut cur, &mut out);
            cur = Some(Device {
                name: String::new(),
                description: String::new(),
                default: false,
                muted: false,
                volume: 0.0,
            });
            continue;
        }
        let Some(d) = cur.as_mut() else { continue };
        if let Some(v) = t.strip_prefix("Name: ") {
            d.name = v.trim().to_string();
            d.default = d.name == default_name;
        } else if let Some(v) = t.strip_prefix("Description: ") {
            d.description = v.trim().to_string();
        } else if let Some(v) = t.strip_prefix("Mute: ") {
            d.muted = v.trim() == "yes";
        } else if t.starts_with("Volume:") && d.volume == 0.0 {
            // «Volume: front-left: 42127 /  64% / -13.36 dB, ...» — берём
            // первый процент: каналы у нас всегда двигаются вместе.
            d.volume = t
                .split('/')
                .nth(1)
                .and_then(|p| p.trim().trim_end_matches('%').parse::<f32>().ok())
                .map(|p| p / 100.0)
                .unwrap_or(0.0);
        }
    }
    push(&mut cur, &mut out);
    out
}

// ── Меню ─────────────────────────────────────────────────────────────────────

const NOTICE_TTL: Duration = Duration::from_secs(5);

/// Строка меню: устройство вывода или ввода.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pick {
    pub sink: bool,
    pub index: usize,
}

pub struct AudioUi {
    tx: mpsc::Sender<Cmd>,
    pub snap: Snapshot,
    pub open: bool,
    pub sel: usize,
    pub notice: Option<(String, Instant)>,
    /// Прямоугольники строк и что в них лежит — заполняет отрисовка.
    pub rows: Vec<(crate::tray::Rect, Pick)>,
}

impl AudioUi {
    pub fn notice_text(&self) -> Option<&str> {
        self.notice
            .as_ref()
            .filter(|(_, at)| at.elapsed() < NOTICE_TTL)
            .map(|(t, _)| t.as_str())
    }

    /// Плоский список строк меню: сначала вывод, потом ввод.
    pub fn picks(&self) -> Vec<Pick> {
        let sinks = (0..self.snap.sinks.len()).map(|index| Pick { sink: true, index });
        let sources = (0..self.snap.sources.len()).map(|index| Pick { sink: false, index });
        sinks.chain(sources).collect()
    }

    pub fn device(&self, pick: Pick) -> Option<&Device> {
        if pick.sink {
            self.snap.sinks.get(pick.index)
        } else {
            self.snap.sources.get(pick.index)
        }
    }
}

impl crate::state::Dawn {
    pub fn init_audio(&mut self, tx: mpsc::Sender<Cmd>) {
        self.audio = Some(AudioUi {
            tx,
            snap: Snapshot::default(),
            open: false,
            sel: 0,
            notice: None,
            rows: Vec::new(),
        });
    }

    pub fn handle_audio_event(&mut self, event: Event) {
        let Some(a) = self.audio.as_mut() else { return };
        match event {
            Event::State(snap) => {
                a.snap = snap;
                let n = a.picks().len();
                a.sel = a.sel.min(n.saturating_sub(1));
            }
            Event::Notice(text) => {
                tracing::info!("dawn/audio: {}", text);
                a.notice = Some((text, Instant::now()));
            }
        }
        self.request_redraw();
    }

    pub fn audio_menu_open(&self) -> bool {
        self.audio.as_ref().is_some_and(|a| a.open)
    }

    pub fn audio_snapshot(&self) -> Option<&Snapshot> {
        self.audio.as_ref().map(|a| &a.snap)
    }

    pub fn audio_send(&mut self, cmd: Cmd) {
        if let Some(a) = self.audio.as_ref() {
            if a.tx.send(cmd).is_err() {
                tracing::warn!("dawn/audio: поток pactl не отвечает");
            }
        }
        self.request_redraw();
    }

    pub fn audio_sync_watch(&mut self) {
        let tray = self.tray_open();
        let menu = self.audio_menu_open();
        self.audio_send(Cmd::Watch { tray, menu });
    }

    pub fn audio_toggle_menu(&mut self) {
        let Some(a) = self.audio.as_mut() else { return };
        a.open = !a.open;
        if !a.open {
            self.text_cache.clear();
        }
        self.audio_sync_watch();
        self.request_redraw();
    }

    fn audio_activate(&mut self) {
        let Some((pick, name)) = self.audio.as_ref().and_then(|a| {
            let pick = *a.picks().get(a.sel)?;
            Some((pick, a.device(pick)?.name.clone()))
        }) else {
            return;
        };
        self.audio_send(Cmd::SetDefault { sink: pick.sink, name });
    }

    fn audio_move_sel(&mut self, delta: i32) {
        let Some(a) = self.audio.as_mut() else { return };
        let n = a.picks().len() as i32;
        if n == 0 {
            return;
        }
        a.sel = (((a.sel as i32 + delta) % n + n) % n) as usize;
        self.request_redraw();
    }

    /// Клавиша при открытом меню. `true` — съедена.
    pub fn audio_key(&mut self, keysym: u32) -> bool {
        use smithay::input::keyboard::keysyms;
        if !self.audio_menu_open() {
            return false;
        }
        match keysym {
            keysyms::KEY_Escape => self.audio_toggle_menu(),
            keysyms::KEY_Down | keysyms::KEY_j => self.audio_move_sel(1),
            keysyms::KEY_Up | keysyms::KEY_k => self.audio_move_sel(-1),
            keysyms::KEY_Return | keysyms::KEY_KP_Enter => self.audio_activate(),
            keysyms::KEY_m => self.audio_send(Cmd::MuteToggle),
            keysyms::KEY_minus => self.audio_nudge(-0.05),
            keysyms::KEY_equal | keysyms::KEY_plus => self.audio_nudge(0.05),
            _ => {}
        }
        true
    }

    /// Подвинуть громкость на шаг от текущей.
    pub fn audio_nudge(&mut self, delta: f32) {
        let now = self
            .audio_snapshot()
            .and_then(|s| s.volume())
            .map(|(v, _)| v)
            .unwrap_or(0.0);
        self.audio_send(Cmd::SetVolume((now + delta).clamp(0.0, 1.0)));
    }

    pub fn audio_click(&mut self, pos: smithay::utils::Point<f64, smithay::utils::Physical>) -> bool {
        if !self.audio_menu_open() {
            return false;
        }
        let hit = self
            .audio
            .as_ref()
            .and_then(|a| a.rows.iter().find(|(r, _)| r.hit(pos.x, pos.y)).map(|(_, p)| *p));
        match hit {
            Some(pick) => {
                if let Some(a) = self.audio.as_mut() {
                    if let Some(i) = a.picks().iter().position(|p| *p == pick) {
                        a.sel = i;
                    }
                }
                self.audio_activate();
            }
            None => self.audio_toggle_menu(),
        }
        true
    }
}
