use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use mlua::{Lua, Table};
use smithay::backend::session::Session as _;
use smithay::input::keyboard::{xkb, XkbConfig};

use crate::state::Dawn;
use crate::tiling::Layout;

// ── Modifiers ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct ModMask {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub logo: bool,
}

impl ModMask {
    fn parse(spec: &str) -> Self {
        let mut m = ModMask::default();
        for part in spec.split('+') {
            match part.trim().to_ascii_lowercase().as_str() {
                "ctrl" | "control" => m.ctrl = true,
                "alt" | "mod1" => m.alt = true,
                "shift" => m.shift = true,
                "super" | "logo" | "mod4" | "win" => m.logo = true,
                "" => {}
                other => tracing::warn!("dawn/config: unknown modifier '{}'", other),
            }
        }
        m
    }
}

// ── Actions ──────────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub enum Action {
    Spawn(String),
    Quit,
    /// Super+R: перезапуск компоновщика на месте — сессия сохраняется, dawn
    /// выходит с кодом [`crate::state::RESTART_EXIT_CODE`], а launch_native.sh
    /// пересобирает (если исходники новее бинаря) и поднимает его заново.
    /// Нужен, чтобы забирать свежую сборку без перелогина в ly.
    Restart,
    Kill,
    SetLayout(Layout),
    ToggleLayoutFloatTile,
    Zoom,
    ToggleFloatingFocused,
    /// F11: окно на весь экран (без скруглений и теней) и обратно.
    ToggleFullscreen,
    /// Обзор столов тумблером. Метод был с самого начала, но вызывался ТОЛЬКО
    /// тапом по Super из `input.rs` — на бинд его повесить было нельзя.
    /// Понадобился как цель для жеста (`home-toggle` из driftwm).
    ToggleOverview,
    /// Свести камеру на окно в фокусе — то, что в driftwm зовётся
    /// `center-window`. Считает `canvas::camera_to_center_window`.
    CenterWindow,
    /// Win+V: выделенные (или сфокусированное) окна — во floating и обратно,
    /// не покидая свой рабочий стол. См. Dawn::float_selected.
    FloatSelected,
    FocusDirection(i32, i32),
    FocusStack(i32),
    IncNmaster(i32),
    SetMfact(f32),
    MoveFocused(i32, i32),
    ResizeFocused(i32, i32),
    /// Argument is a tag bitmask (1 << (n-1)), not the 1-based tag number.
    ViewTag(u32),
    TagWindow(u32),
    ToggleView(u32),
    ToggleTag(u32),
    ToggleMinimap,
    /// Снимок экрана областью (PrtScr, см. snip.rs).
    Screenshot,
    TogglePortal,
    ToggleBookmarksMode,
    ToggleSnapping,
    ToggleMagnetism,
    ToggleFoldStack,
    VtSwitch(i32),
    LayoutNext,
    LayoutPrev,
    ReloadConfig,
    /// Собрать текущее rubber-band выделение в "созвездие" (двигается/ресайзится
    /// как единое целое, см. selection.rs).
    GroupSelected,
    /// Разбить созвездие, в котором состоит сфокусированное окно.
    UngroupSelected,
    /// Закрепить закладку камеры на текущей позиции курсора (Alt+B).
    PinBookmarkAtCursor,
    /// Alt+Super+B: удалить ближайшую к курсору закладку камеры.
    DeleteNearestBookmark,
    /// Win+Alt+N: прыжок к закладке камеры N без режима закладок
    /// (Super+цифра занят рабочими столами, Super+N — лентой niri).
    JumpBookmark(u32),
    /// Columns (niri): сменить пресет ширины активной колонки (Super+R).
    ColumnWidthCycle,
    // ── Действия колонок как в niri (см. columns.rs) ──────────────────────
    /// `switch-preset-window-height`
    WindowHeightCycle,
    /// `set-column-width "±N%"`
    ColumnWidthAdjust(f64),
    /// `set-window-height "±N%"`
    WindowHeightAdjust(f64),
    /// `reset-window-height`
    WindowHeightReset,
    /// `maximize-column`
    ColumnMaximize,
    /// `center-column`
    ColumnCenter,
    /// `focus-column-first` / `focus-column-last`
    ColumnFocusEdge(bool),
    /// `move-column-to-first` / `move-column-to-last`
    ColumnMoveToEdge(bool),
    /// `consume-or-expel-window-left/right`
    ColumnConsumeOrExpel(i32),
    /// `center-focused-column` — режим поведения вида (never/always/on-overflow)
    ColumnCenterMode(crate::columns::CenterFocusedColumn),
    /// `toggle-column-tabbed-display`
    ColumnToggleTabbed,
    /// `switch-focus-between-floating-and-tiling`
    ColumnFocusOtherLayer,
    /// niri-воркспейсы: перейти на пред/след воркспейс (dir=-1/+1).
    WorkspaceStep(i32),
    /// niri-воркспейсы: перенести активную колонку на соседний воркспейс (Columns).
    MoveColumnToWorkspace(i32),
    /// Тумблер niri-режим (Columns) ↔ Tile (Win+N): в niri — выход в обычный
    /// tile, не в niri — вход в niri.
    ToggleNiriMode,
    /// Меню блютуза (см. bluetooth.rs).
    BluetoothMenu,
    /// Тумблер питания блютуз-адаптера, без открытия меню.
    BluetoothPower,
    /// Полка состояния у бара: вайфай, звук, батарея, питание (см. tray.rs).
    TrayMenu,
    /// Меню выбора сети (см. wifi.rs).
    WifiMenu,
    /// Меню устройств вывода и ввода звука (см. audio.rs).
    AudioMenu,
    // ── Шлем: VR и дополненная реальность (см. vr/) ───────────────────────
    /// Войти в шлем и выйти обратно. Мониторы при этом продолжают работать:
    /// VR — дополнительный «выход», а не замена сеансу.
    VrToggle,
    /// Весь вход в VR одним нажатием: поднять сервер WiVRn, дождаться, когда
    /// наденут шлем, и войти. Повторное нажатие отменяет ожидание или снимает
    /// шлем. Это то действие, которое стоит на биндe; `vr_toggle` — сырое,
    /// без сервера и без ожидания (нужно для симулятора и отладки).
    VrMode,
    /// Passthrough: окна поверх настоящей комнаты вместо пустоты.
    VrAr,
    /// Следующая раскладка панелей в пространстве: дуга → стена → купол →
    /// свободно.
    VrLayout,
    /// Собрать панели заново вокруг того, куда человек смотрит сейчас.
    VrRecenter,
    /// Пульт «Пуск» в шлеме: кнопки приложений и управление шлемом. То же, что
    /// кнопка меню на контроллере и жест «ладонь вверх» (см. vr/ui.rs).
    VrLauncher,
    /// Виртуальная клавиатура в шлеме. То же, что кнопка A/X на контроллере и
    /// щипок большим с безымянным.
    VrKeyboard,
    /// Войти в Minecraft-режим или выйти из него (см. mine/).
    MineMode,
    /// Следующая раскладка панелей в игре: дуга → стена → купол → свободно.
    /// То же, что `VrLayout`, но для сцены `mine` — у режимов свои сцены, и
    /// одно действие на двоих переставляло бы панели не там, где смотрят.
    MineLayout,
    /// Взять панель под взглядом или отпустить её. Тумблером, а не «пока
    /// зажато»: боковая кнопка мыши есть не у всех, а в игре руки заняты.
    MineGrab,
    /// Alt+Tab: перебор окон, лежащих друг под другом (см. switcher.rs).
    /// Аргумент — направление: +1 вглубь стопки, −1 назад.
    CycleStack(i32),
    /// Super+F: поиск окна по имени с перелётом к нему (см. switcher.rs).
    WindowSearch,
    /// Мультиюзер: включить раздачу стола гостям (см. share/mod.rs).
    /// Аргумент — порт; 0 означает «как в протоколе» (7373).
    ShareStart(u16),
    /// Мультиюзер: выключить раздачу и вернуть режим, что был до неё.
    ShareStop,
    /// Мультиюзер: тумблер. Он и повешен на бинд — включать и выключать
    /// раздачу двумя разными сочетаниями незачем, а забыть, что она идёт,
    /// легко (отсюда же чип с кодом на панели).
    ShareToggle(u16),
}

#[derive(Clone, Debug)]
pub struct KeyBinding {
    pub mods: ModMask,
    pub keysym: u32,
    pub action: Action,
}

// ── XKB settings ─────────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct XkbSettings {
    pub rules: String,
    pub model: String,
    pub layout: String,
    pub variant: String,
    pub options: Option<String>,
}

impl Default for XkbSettings {
    fn default() -> Self {
        Self {
            rules: String::new(),
            model: String::new(),
            layout: "us".into(),
            variant: String::new(),
            options: None,
        }
    }
}

impl XkbSettings {
    pub fn to_xkb_config(&self) -> XkbConfig<'_> {
        XkbConfig {
            rules: &self.rules,
            model: &self.model,
            layout: &self.layout,
            variant: &self.variant,
            options: self.options.clone(),
        }
    }
}

// ── Monitor config ───────────────────────────────────────────────────────────

#[derive(Clone, Debug)]
pub struct MonitorConfig {
    pub name: String,
    pub width: i32,
    pub height: i32,
    pub refresh: i32,  // Hz
    pub x: i32,
    pub y: i32,
    /// Задал ли человек `x`/`y` в самом `monitor{}` — отдельно от значения:
    /// `x = 0, y = 0` неотличимо от «не задано» (оба дают 0 через
    /// `unwrap_or(0)`), а `(0,0)` — валидная раскладка (монитор у самого
    /// начала). Без этого флага основной монитор на явном `x=0, y=0`, увиденный
    /// НЕ первым, получал бы авто-раскладку вместо угла — ровно то, что нужно
    /// человеку, задавшему координаты руками.
    pub layout_set: bool,
    pub scale: f64,
    pub transform: String,
    /// `monitor{ tag = N }` — с какого рабочего стола монитор начинает
    /// (1..=9, 0 — «выбери сам»). Столы в dawn принадлежат монитору, как
    /// воркспейсы в hyprland: без этой ручки монитор N открывает стол N.
    pub tag: u32,
    /// `monitor{ primary = true }` — этот монитор становится активным при
    /// старте, даже если DRM отдал его коннектор вторым.
    ///
    /// **Зачем нужен отдельный флаг.** Без него активным становится ПЕРВЫЙ
    /// увиденный коннектор (`add_surface`, `первый = state.мониторы.is_empty()`),
    /// а порядок, в котором ядро отдаёт коннекторы, НЕ постоянен — см.
    /// [[dawn-two-monitors]] в памяти: один и тот же кабель на одном сеансе
    /// поднимался первым, на другом вторым. «Основной монитор» должен быть
    /// решением человека, а не гонкой сканирования шины.
    pub primary: bool,
}

/// `vr{}` — настройки шлема.
///
/// Все они действуют и на уже включённый режим (перечитывание конфига), кроме
/// `auto`: он читается один раз при старте.
#[derive(Clone, Debug)]
pub struct VrConfig {
    /// Раскладка панелей в пространстве: дуга, стена, купол, свободно.
    pub layout: crate::vr::scene::Раскладка,
    /// Метров на пиксель окна: чем больше, тем крупнее панели. 0.0008 —
    /// окно шириной 1920 занимает полтора метра (примерно монитор на столе).
    pub scale: f32,
    /// Радиус, на котором стоят панели. 0 — считать по зоне шлема.
    pub radius: f32,
    /// Входить в дополненную реальность сразу, как только шлем подключился.
    pub ar: bool,
    /// Надевать шлем при старте dawn (то же, что ключ `--vr`).
    pub auto: bool,
    /// Кнопки пульта «Пуск» в шлеме: (подпись, команда). Пусто — набор по
    /// умолчанию (см. `vr::ui::приложения_по_умолчанию`).
    ///
    /// Задаётся так:
    /// ```lua
    /// vr{ apps = { { name = "Терминал", cmd = "foot" },
    ///              { name = "Браузер",  cmd = "waterfox" } } }
    /// ```
    pub apps: Vec<(String, String)>,
    /// Жест или кнопка контроллера → действие dawn. Ключи — имена из
    /// [`crate::vr::input::Жест::имя`] (`fist`, `swipe_left`, `menu_button`, …),
    /// значения — любые действия `bind{}`:
    /// ```lua
    /// vr{ gestures = {
    ///       fist        = "vr_launcher",
    ///       thumb_up    = "toggle_fullscreen",
    ///       swipe_left  = { action = "workspace_step", dir = -1 },
    ///       pinch_little = { action = "spawn", cmd = "foot" },
    ///     } }
    /// ```
    /// Чего в таблице нет — работает по умолчанию
    /// ([`crate::vr::input::Жест::по_умолчанию`]); таблица не заменяет
    /// раскладку целиком, а накрывает её поверх.
    pub gestures: std::collections::HashMap<String, Action>,
}

impl Default for VrConfig {
    fn default() -> Self {
        VrConfig {
            layout: crate::vr::scene::Раскладка::Дуга,
            scale: 0.0008,
            radius: 0.0,
            ar: false,
            auto: false,
            apps: Vec::new(),
            gestures: std::collections::HashMap::new(),
        }
    }
}

/// `mine{}` — dawn внутри Minecraft (см. mine/).
///
/// Отдельно от `vr{}`, хотя расстановка панелей у них общая: зона в игре —
/// это не охраняемая граница комнаты, а сколько блоков вокруг игрока не жалко
/// занять, и подбирается она совсем другими числами.
#[derive(Clone, Debug)]
pub struct MineConfig {
    /// Ширина и глубина зоны панелей В БЛОКАХ (они же метры). По умолчанию
    /// 8×6 — комната, которую видно целиком, не крутя головой на месте.
    pub зона_ширина: f32,
    pub зона_глубина: f32,
    /// Метров (блоков) на пиксель окна. Крупнее, чем в шлеме: в Minecraft на
    /// панель смотрят с нескольких блоков, а не с вытянутой руки, и мелкий
    /// текст на ней не читается вовсе.
    pub метров_на_пиксель: f32,
}

impl Default for MineConfig {
    fn default() -> Self {
        MineConfig { зона_ширина: 8.0, зона_глубина: 6.0, метров_на_пиксель: 0.0016 }
    }
}

// ── Config ───────────────────────────────────────────────────────────────────

pub struct Config {
    pub bindings: Vec<KeyBinding>,
    pub xkb: XkbSettings,
    /// Trigger key for the Super-held bird's-eye zoom gesture (default: space).
    /// This is a hold-gesture, not a plain keybind, so it's handled separately.
    pub bird_eye_key: u32,
    /// `dwindle{}` — Hyprland's dwindle:* knobs for Layout::Tile (see dwindle.rs).
    pub dwindle: crate::dwindle::DwindleConfig,
    /// `set{ bluetooth_autoconnect = ... }` — при старте сессии поднять адаптер
    /// и подключить устройство, которым пользовались последним (см. bluetooth.rs).
    pub bluetooth_autoconnect: bool,
    /// `monitor{}` — конфигурация выходов: имя, разрешение, частота, позиция.
    pub monitors: Vec<MonitorConfig>,
    /// `vr{}` — шлем: раскладка панелей, их размер, зона (см. vr/).
    pub vr: VrConfig,
    /// `mine{}` — режим Minecraft: зона панелей и их размер (см. mine/).
    pub mine: MineConfig,
    /// `set{ cursor_size = ... }` — размер курсора компоновщика в пикселях.
    /// 0 — взять из XCURSOR_SIZE (так же, как его читают клиенты).
    pub cursor_size: i32,
    /// `set{ cursor_client_max = ... }` — потолок для курсоров, которые клиент
    /// рисует сам. -1 (по умолчанию) — потолок равен cursor_size, 0 — потолка
    /// нет вовсе (курсор клиента показывается как прислан).
    pub cursor_client_max: i32,
    /// `set{ anim_speed = ... }` — общий темп анимаций: 1.0 как задумано,
    /// больше — медленнее и спокойнее, меньше — резче. Одна ручка на все
    /// движения сразу; сами длительности живут в `anim::дуг`.
    pub anim_speed: f64,
    /// `set{ pan_drift = ... }` — насколько долго холст едет по инерции после
    /// броска пальцами/мышью: 0 — инерции нет вовсе, 1 — самая «плавучая».
    /// `canvas::speed_dependent_drift` с самого начала называл это «ручкой
    /// пользователя», но самой ручки не было — значение стояло литералом в
    /// state.rs. Теперь оно и правда ручка.
    pub pan_drift: f64,
    /// `set{ fling_distance = ... }` — как далеко улетает БРОШЕННОЕ окно:
    /// 1.0 — как задумано (~2000 px холста на резком броске), 0 — окно встаёт
    /// там же, где его отпустили, 2.0 — вдвое дальше. Ручка-близнец
    /// `pan_drift`: та про инерцию холста, эта про инерцию окна.
    pub fling_distance: f64,
    /// `set{ infinite_wallpaper = ... }` — обои живут на ХОЛСТЕ и едут за
    /// камерой (одной копией, с затуханием) вместо того, чтобы быть
    /// приклеенными к экрану. См. `udev::build_wallpaper_backdrop`.
    pub infinite_wallpaper: bool,
    /// `set{ share_guest_all = ... }` — снять с гостя ПОСЛЕДНИЕ ограничения.
    ///
    /// Гость и так получает все бинды композитора (это прямое требование
    /// Ярика 30.08.2026: «сделай полный доступ»). Четыре действия по умолчанию
    /// всё же оставлены хозяину машины, и не из осторожности вообще, а потому
    /// что каждое из них обрывает саму раздачу вместе с панелью управления, из
    /// которой гостя выгоняют: `Quit` и `Restart` кладут сеанс, `VtSwitch`
    /// уводит экран на другой терминал, `Share*` выключает раздачу изнутри.
    /// Случайный Super+Shift+Q у гостя стоил бы всем остальным рабочего стола.
    ///
    /// `true` — не оставлять и этого: гость может ровно всё, что хозяин.
    pub share_guest_all: bool,
    /// `set{ keyboard_grab_apps = {"dshare"} }` — приложения, которым, пока они
    /// в фокусе, отдаются ВСЕ клавиши: ни один бинд композитора не срабатывает.
    ///
    /// **Зачем.** Гость мультиюзера сидит за своим dawn и работает в чужом
    /// рабочем столе через окно `dshare`. Super у него съедал бы СВОЙ
    /// композитор, и до хоста не доходило бы ничего: ни Super+D, ни Super+1,
    /// ни Super+Q. То же самое нужно виртуалкам и любому удалённому столу.
    ///
    /// **Почему по классу окна, а не протоколом.** В Wayland для этого есть
    /// `zwp_keyboard_shortcuts_inhibit_v1`, и smithay его умеет — но просить
    /// инхибитор должен КЛИЕНТ, а `dshare` написан на winit, который этого
    /// протокола не выставляет вовсе. Класс окна даёт тот же результат, не
    /// требуя от клиента ничего.
    ///
    /// **Выход всегда есть:** Super+Shift+Escape снимает захват и возвращает
    /// бинды, даже если приложение зависло. Без этой лазейки повисший dshare
    /// запирал бы человека в его собственном сеансе намертво.
    pub keyboard_grab_apps: Vec<String>,
    /// Жесты тачпада таблицей — `gesture{}` в `config.lua`. См. `gestures.rs`.
    ///
    /// Пустая таблица = поведение dawn до 30.08.2026: жесты обрабатывают
    /// прежние ветки `input.rs`, слово в слово.
    pub gestures: Vec<crate::gestures::БиндЖеста>,
    /// Пороги распознавания жестов: `set{ swipe_threshold = …,
    /// pinch_in_threshold = …, pinch_out_threshold = … }`. Имена и значения
    /// по умолчанию — как в driftwm, чтобы настройки переносились один в один.
    pub gesture_thresholds: crate::gestures::Пороги,
    /// Автодовод курсора по краям НАКЛАДКИ тачпада:
    /// `set{ touchpad_edge_motion = true, touchpad_edge_zone = 0.08,
    ///       touchpad_edge_speed = 900.0 }`. См. touchpad.rs.
    pub автодовод: crate::touchpad::Автодовод,
    /// `set{ blur = ... }` — размывать фон под островами панели (см. blur.rs).
    /// ПО УМОЛЧАНИЮ ВЫКЛЮЧЕНО: код проходом рендера живьём не отсмотрен, а
    /// ошибка там стоит чёрного экрана.
    pub blur: bool,
    /// `set{ close_anim = ... }` — спокойное угасание закрытого окна
    /// (см. close.rs). Включено.
    ///
    /// Ручка заведена не «на всякий случай»: снимок окна делается через
    /// offscreen-проход рендера (`bind` → `render` → `finish`), а это тот же
    /// приём, что у размытия, и живьём он в dawn ни разу не отсмотрен. Если
    /// закрытие окна начнёт портить кадр — выключается здесь и подхватывается
    /// по Super+Shift+C, без пересборки.
    pub close_anim: bool,
}

impl Config {
    pub fn xkb_config(&self) -> XkbConfig<'_> {
        self.xkb.to_xkb_config()
    }

    /// Looks up the action bound to an exact (mods, keysym) combination.
    /// Requires an EXACT modifier match (see default_config.lua header).
    pub fn find_action(&self, mods: ModMask, keysym: u32) -> Option<Action> {
        self.bindings
            .iter()
            .find(|b| b.mods == mods && b.keysym == keysym)
            .map(|b| b.action.clone())
    }

    /// Loads `~/.config/dawn/config.lua`, writing out the embedded default
    /// config on first run. Falls back to the embedded default (in-memory)
    /// on any read or evaluation error, so a broken user config never
    /// prevents startup.
    pub fn load() -> Config {
        load()
    }

    /// Evaluates a Lua config source string, exposing `bind{}`, `xkb{}` and
    /// `set{}` as globals that accumulate into the returned [`Config`].
    pub fn load_from_str(source: &str) -> mlua::Result<Config> {
        load_from_str(source)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bindings: Vec::new(),
            xkb: XkbSettings::default(),
            bird_eye_key: xkb::keysyms::KEY_space,
            dwindle: crate::dwindle::DwindleConfig::default(),
            bluetooth_autoconnect: true,
            monitors: Vec::new(),
            vr: VrConfig::default(),
            mine: MineConfig::default(),
            cursor_size: 0,
            cursor_client_max: -1,
            anim_speed: 1.0,
            pan_drift: 0.5,
            fling_distance: 1.0,
            infinite_wallpaper: true,
            share_guest_all: false,
            keyboard_grab_apps: vec!["dshare".to_string()],
            gestures: Vec::new(),
            gesture_thresholds: crate::gestures::Пороги::default(),
            автодовод: crate::touchpad::Автодовод::default(),
            blur: false,
            close_anim: true,
        }
    }
}

const DEFAULT_CONFIG_LUA: &str = include_str!("../default_config.lua");

fn config_path() -> Option<PathBuf> {
    // dawn обычно запускается через `sudo openvt` (см. launch.zsh), поэтому
    // HOME=/root и конфиг читался бы из /root/.config/dawn/ — НЕ там, где его
    // правит пользователь. Если задан SUDO_USER — берём конфиг реального
    // пользователя (/home/<user>/.config/dawn/config.lua).
    let home = match std::env::var("SUDO_USER") {
        Ok(user) if !user.is_empty() && user != "root" => format!("/home/{user}"),
        _ => std::env::var("HOME").ok()?,
    };
    Some(PathBuf::from(home).join(".config/dawn/config.lua"))
}

fn keysym_from_name(name: &str) -> Option<u32> {
    let sym = xkb::keysym_from_name(name, xkb::KEYSYM_CASE_INSENSITIVE);
    let raw = sym.raw();
    if raw == xkb::keysyms::KEY_NoSymbol {
        None
    } else {
        Some(raw)
    }
}

/// То же, что `action_from_lua`, но из строки: имя действия и ТЕЛО таблицы Lua
/// (`cmd="ghostty"`, `tag=2`, `dx=1, dy=0`). Нужен управляющему сокету —
/// разбор аргументов там обязан совпадать с `bind{}` до последней мелочи,
/// поэтому таблица не парсится вручную, а строится тем же Lua.
pub fn action_from_str(имя: &str, аргументы: &str) -> Option<Action> {
    let lua = Lua::new();
    let tbl: Table = lua
        .load(format!("return {{{}}}", аргументы))
        .eval()
        .map_err(|e| tracing::warn!("dawn/ctl: аргументы '{}' не разобрались: {}", аргументы, e))
        .ok()?;
    action_from_lua(имя, &tbl)
}

/// Действие жеста: сперва непрерывные и `center-nearest` (их нет среди
/// клавиатурных — клавиша не умеет «ехать»), потом обычное действие dawn.
///
/// Имена непрерывных пишутся через дефис, как в driftwm, и через
/// подчёркивание, как всё остальное в `config.lua`: конфиг переносится оттуда
/// копированием, и заставлять человека переписывать половину строк было бы глупо.
///
/// **Ловушка имени `zoom`.** В driftwm `zoom` — это зум КАМЕРЫ щипком, а в
/// dawn действие с тем же именем — «поднять окно в мастер-слот» (dwm). В жестах
/// побеждает driftwm-смысл, иначе перенос конфига молча менял бы поведение; для
/// оконного зума есть отдельное имя `zoom-master`.
/// Жесты тачпада, как их отдаёт driftwm из коробки (`config/defaults.rs` там).
///
/// Перевод один в один, насколько действия вообще совпадают; чего в dawn нет —
/// перечислено в `default_config.lua` рядом с этим же списком.
const ЖЕСТЫ_ПО_УМОЛЧАНИЮ: &str = r#"
-- Над окном.
-- Ресайз висит на SUPER, а не на alt, как в driftwm: alt в dawn уже занят
-- паном холста, и жест приходилось начинать с пустого места (01.09.2026,
-- жалоба «ресайз на alt работает слабо»).
gesture{ mods = "super", fingers = 3, kind = "swipe", where = "window", action = "resize-window" }
gesture{ mods = "super+shift", fingers = 3, kind = "swipe", where = "window", action = "resize-window-snapped" }
gesture{ mods = "alt", fingers = 3, kind = "pinch-in", where = "window", action = "toggle_fullscreen" }
gesture{ mods = "alt", fingers = 3, kind = "pinch-out", where = "window", action = "toggle_fullscreen" }

-- По холсту. Голый двухпальцевый щипок в dawn не делал НИЧЕГО (встроенный зум
-- просит Alt) — это чистое добавление.
gesture{ fingers = 2, kind = "pinch", where = "canvas", action = "zoom" }

-- Везде.
-- ПЕРЕБИВАЕТ: в раскладке Columns голый свайп тремя пальцами листал полосу.
gesture{ fingers = 3, kind = "swipe", action = "pan-viewport" }
-- ПЕРЕБИВАЕТ: там же четырьмя пальцами листалась полоса наравне с тремя.
gesture{ fingers = 4, kind = "swipe", action = "center-nearest" }
gesture{ mods = "super", fingers = 3, kind = "swipe", action = "center-nearest" }
gesture{ mods = "super", fingers = 2, kind = "pinch", action = "zoom" }
gesture{ fingers = 3, kind = "pinch", action = "zoom" }
gesture{ fingers = 4, kind = "pinch-out", action = "toggle_overview" }
gesture{ mods = "super", fingers = 3, kind = "pinch-out", action = "toggle_overview" }
gesture{ fingers = 4, kind = "hold", action = "center_window" }
gesture{ mods = "super", fingers = 3, kind = "hold", action = "center_window" }
"#;

fn действие_жеста(имя: &str, tbl: &Table) -> Option<crate::gestures::ДействиеЖеста> {
    use crate::gestures::{ДействиеЖеста as Д, Непрерывное as Н};
    let ключ = имя.trim().to_ascii_lowercase().replace('_', "-");
    Some(match ключ.as_str() {
        "pan-viewport" => Д::Непрерывно(Н::ПанВида),
        "zoom" => Д::Непрерывно(Н::Зум),
        "zoom-master" => Д::Порогом(action_from_lua("zoom", tbl)?),
        "move-window" => Д::Непрерывно(Н::ВестиОкно),
        "move-snapped-windows" => Д::Непрерывно(Н::ВестиСоСнапом),
        "resize-window" => Д::Непрерывно(Н::РазмерОкна),
        "resize-window-snapped" => Д::Непрерывно(Н::РазмерСоСнапом),
        "center-nearest" => Д::ЦентрБлижайший,
        _ => Д::Порогом(action_from_lua(имя, tbl)?),
    })
}

/// Можно ли повесить непрерывное действие на этот триггер.
///
/// Непрерывное действие получает дельту каждый кадр, поэтому ему нужен жест,
/// который эту дельту даёт: свайп или щипок в непрерывном виде. Направленные
/// (`swipe-up`) и пороговые (`pinch-in`, `hold`) варианты срабатывают ОДИН раз
/// и потока не дают — вешать на них пан бессмысленно.
fn непрерывное_подходит(
    действие: crate::gestures::Непрерывное,
    триггер: crate::gestures::Триггер,
) -> bool {
    use crate::gestures::{Непрерывное as Н, Триггер as Т};
    match действие {
        Н::Зум => matches!(триггер, Т::Щипок { .. }),
        Н::ПанВида => matches!(триггер, Т::Свайп { .. }),
        Н::ВестиОкно | Н::ВестиСоСнапом | Н::РазмерОкна | Н::РазмерСоСнапом => {
            matches!(триггер, Т::Свайп { .. } | Т::ДвойнойТапСвайп { .. })
        }
    }
}

fn action_from_lua(action: &str, tbl: &Table) -> Option<Action> {
    use Action::*;
    let get_i32 = |k: &str, default: i32| tbl.get::<i32>(k).unwrap_or(default);
    let get_f32 = |k: &str, default: f32| tbl.get::<f32>(k).unwrap_or(default);
    let get_tag_mask = |default: u32| -> u32 {
        let n = tbl.get::<i64>("tag").unwrap_or(default as i64).max(1);
        1u32 << (n - 1).min(31)
    };

    Some(match action {
        "spawn" => Spawn(tbl.get::<String>("cmd").ok()?),
        "quit" => Quit,
        "restart" => Restart,
        "kill" => Kill,
        "set_layout" => {
            let l = tbl.get::<String>("layout").ok()?;
            SetLayout(match l.as_str() {
                "tile" => Layout::Tile,
                "float" => Layout::Float,
                "monocle" => Layout::Monocle,
                "columns" => Layout::Columns,
                other => {
                    tracing::warn!("dawn/config: unknown layout '{}'", other);
                    return None;
                }
            })
        }
        "toggle_layout" => ToggleLayoutFloatTile,
        "zoom" => Zoom,
        "bluetooth_menu" => BluetoothMenu,
        "bluetooth_power" => BluetoothPower,
        "tray_menu" => TrayMenu,
        "wifi_menu" => WifiMenu,
        "audio_menu" => AudioMenu,
        "vr_toggle" => VrToggle,
        "vr_mode" => VrMode,
        "vr_ar" => VrAr,
        "vr_layout" => VrLayout,
        "vr_recenter" => VrRecenter,
        "vr_launcher" | "vr_menu" => VrLauncher,
        "vr_keyboard" => VrKeyboard,
        "mine_mode" | "minecraft" => MineMode,
        "mine_layout" => MineLayout,
        "mine_grab" => MineGrab,
        "toggle_floating" => ToggleFloatingFocused,
        "toggle_fullscreen" => ToggleFullscreen,
        "toggle_overview" => ToggleOverview,
        "center_window" => CenterWindow,
        "float_selected" => FloatSelected,
        "focus_direction" => FocusDirection(get_i32("dx", 0), get_i32("dy", 0)),
        "focus_stack" => FocusStack(get_i32("dir", 1)),
        "cycle_stack" => CycleStack(get_i32("dir", 1)),
        "window_search" => WindowSearch,
        "inc_nmaster" => IncNmaster(get_i32("n", 1)),
        "set_mfact" => SetMfact(get_f32("delta", 0.05)),
        "move_focused" => MoveFocused(get_i32("dx", 0), get_i32("dy", 0)),
        "resize_focused" => ResizeFocused(get_i32("dw", 0), get_i32("dh", 0)),
        "view_tag" => ViewTag(get_tag_mask(1)),
        "tag_window" => TagWindow(get_tag_mask(1)),
        "toggle_view" => ToggleView(get_tag_mask(1)),
        "toggle_tag" => ToggleTag(get_tag_mask(1)),
        "toggle_minimap" => ToggleMinimap,
        "screenshot" | "snip" => Screenshot,
        "toggle_portal" => TogglePortal,
        "toggle_bookmarks_mode" => ToggleBookmarksMode,
        "toggle_snapping" => ToggleSnapping,
        "toggle_magnetism" => ToggleMagnetism,
        "share_start" => ShareStart(get_i32("port", 0).clamp(0, 65535) as u16),
        "share_stop" => ShareStop,
        "share_toggle" => ShareToggle(get_i32("port", 0).clamp(0, 65535) as u16),
        "toggle_fold_stack" => ToggleFoldStack,
        "vt_switch" => VtSwitch(get_i32("vt", 1)),
        "layout_next" => LayoutNext,
        "layout_prev" => LayoutPrev,
        "reload_config" => ReloadConfig,
        "group_selected" => GroupSelected,
        "ungroup_selected" => UngroupSelected,
        "pin_bookmark_at_cursor" => PinBookmarkAtCursor,
        "delete_nearest_bookmark" => DeleteNearestBookmark,
        "jump_bookmark" => JumpBookmark(get_i32("slot", 1).clamp(1, 9) as u32),
        "column_width_cycle" => ColumnWidthCycle,
        // niri-действия колонок. Проценты — как в niri: set-column-width "+10%".
        "window_height_cycle" => WindowHeightCycle,
        "column_width_adjust" => ColumnWidthAdjust(get_f32("percent", 10.0) as f64),
        "window_height_adjust" => WindowHeightAdjust(get_f32("percent", 10.0) as f64),
        "window_height_reset" => WindowHeightReset,
        "column_maximize" => ColumnMaximize,
        "column_center" => ColumnCenter,
        "column_focus_first" => ColumnFocusEdge(false),
        "column_focus_last" => ColumnFocusEdge(true),
        "column_move_to_first" => ColumnMoveToEdge(false),
        "column_move_to_last" => ColumnMoveToEdge(true),
        "consume_or_expel_left" => ColumnConsumeOrExpel(-1),
        "consume_or_expel_right" => ColumnConsumeOrExpel(1),
        "column_toggle_tabbed" => ColumnToggleTabbed,
        "focus_floating_or_tiling" => ColumnFocusOtherLayer,
        "center_focused_column" => {
            use crate::columns::CenterFocusedColumn as C;
            let mode = tbl.get::<String>("mode").unwrap_or_else(|_| "never".into());
            ColumnCenterMode(match mode.as_str() {
                "always" => C::Always,
                "on-overflow" | "on_overflow" => C::OnOverflow,
                _ => C::Never,
            })
        }
        "workspace_step" => WorkspaceStep(get_i32("dir", 1)),
        "move_column_to_workspace" => MoveColumnToWorkspace(get_i32("dir", 1)),
        "toggle_niri_mode" => ToggleNiriMode,
        other => {
            tracing::warn!("dawn/config: unknown action '{}'", other);
            return None;
        }
    })
}

/// Loads `~/.config/dawn/config.lua`, writing out the embedded default config
/// on first run. Falls back to the embedded default (in-memory) on any read
/// or evaluation error, so a broken user config never prevents startup.
pub fn load() -> Config {
    let path = config_path();
    let source = match &path {
        Some(p) => {
            if !p.exists() {
                if let Some(parent) = p.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(p, DEFAULT_CONFIG_LUA) {
                    Ok(()) => tracing::info!("dawn/config: wrote default config to {:?}", p),
                    Err(e) => tracing::warn!("dawn/config: failed to write default config to {:?}: {}", p, e),
                }
            }
            std::fs::read_to_string(p).unwrap_or_else(|e| {
                tracing::warn!("dawn/config: failed to read {:?}: {} — using embedded default", p, e);
                DEFAULT_CONFIG_LUA.to_string()
            })
        }
        None => {
            tracing::warn!("dawn/config: $HOME not set — using embedded default config");
            DEFAULT_CONFIG_LUA.to_string()
        }
    };

    let cfg = match load_from_str(&source) {
        Ok(cfg) => {
            tracing::info!("dawn/config: loaded {} keybinding(s)", cfg.bindings.len());
            cfg
        }
        Err(e) => {
            tracing::error!("dawn/config: error evaluating config.lua: {} — falling back to built-in default", e);
            load_from_str(DEFAULT_CONFIG_LUA).unwrap_or_default()
        }
    };
    // Темп анимаций живёт в самом anim.rs (атомик — см. anim::set_tempo), а не
    // в поле, которое каждый конструктор анимации спрашивал бы у Dawn. Ставим
    // его ЗДЕСЬ, в единственной точке загрузки: Super+Shift+C зовёт тот же
    // load(), так что перечитывание конфига подхватывает новый темп само.
    crate::anim::set_tempo(cfg.anim_speed);
    // Дальность броска окна живёт там же и по той же причине (атомик в anim.rs).
    crate::anim::set_fling(cfg.fling_distance);
    cfg
}

/// Evaluates a Lua config source string, exposing `bind{}`, `xkb{}` and
/// `set{}` as globals that accumulate into the returned [`Config`].
pub fn load_from_str(source: &str) -> mlua::Result<Config> {
    let lua = Lua::new();
    let bindings: Rc<RefCell<Vec<KeyBinding>>> = Rc::new(RefCell::new(Vec::new()));
    /// Жесты, которые человек погасил (`action = "none"`): их не должно
    /// остаться ни от умолчаний, ни от его же прежних строк.
    #[allow(clippy::type_complexity)]
    let отключённые: Rc<RefCell<Vec<(ModMask, crate::gestures::Триггер, crate::gestures::Где)>>> =
        Rc::new(RefCell::new(Vec::new()));
    let gestures: Rc<RefCell<Vec<crate::gestures::БиндЖеста>>> =
        Rc::new(RefCell::new(Vec::new()));
    let gesture_thresholds: Rc<RefCell<crate::gestures::Пороги>> =
        Rc::new(RefCell::new(crate::gestures::Пороги::default()));
    let автодовод: Rc<RefCell<crate::touchpad::Автодовод>> =
        Rc::new(RefCell::new(crate::touchpad::Автодовод::default()));
    let xkb_settings: Rc<RefCell<XkbSettings>> = Rc::new(RefCell::new(XkbSettings::default()));
    let bird_eye_key: Rc<RefCell<u32>> = Rc::new(RefCell::new(xkb::keysyms::KEY_space));
    let dwindle_cfg: Rc<RefCell<crate::dwindle::DwindleConfig>> =
        Rc::new(RefCell::new(crate::dwindle::DwindleConfig::default()));
    let bt_autoconnect: Rc<RefCell<bool>> = Rc::new(RefCell::new(true));
    let monitors: Rc<RefCell<Vec<MonitorConfig>>> = Rc::new(RefCell::new(Vec::new()));
    let vr_cfg: Rc<RefCell<VrConfig>> = Rc::new(RefCell::new(VrConfig::default()));
    let mine_cfg: Rc<RefCell<MineConfig>> = Rc::new(RefCell::new(MineConfig::default()));
    let cursor_size: Rc<RefCell<i32>> = Rc::new(RefCell::new(0));
    let cursor_client_max: Rc<RefCell<i32>> = Rc::new(RefCell::new(-1));
    let anim_speed: Rc<RefCell<f64>> = Rc::new(RefCell::new(1.0));
    let pan_drift: Rc<RefCell<f64>> = Rc::new(RefCell::new(0.5));
    let fling_distance: Rc<RefCell<f64>> = Rc::new(RefCell::new(1.0));
    let infinite_wallpaper: Rc<RefCell<bool>> = Rc::new(RefCell::new(true));
    let share_guest_all: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let keyboard_grab_apps: Rc<RefCell<Vec<String>>> =
        Rc::new(RefCell::new(vec!["dshare".to_string()]));
    let blur: Rc<RefCell<bool>> = Rc::new(RefCell::new(false));
    let close_anim: Rc<RefCell<bool>> = Rc::new(RefCell::new(true));

    {
        let bindings = bindings.clone();
        let bind_fn = lua.create_function(move |_, tbl: Table| {
            let mods_str: String = tbl.get("mods").unwrap_or_default();
            let key_str: String = tbl
                .get("key")
                .map_err(|_| mlua::Error::RuntimeError("bind{} is missing required field 'key'".into()))?;
            let action_str: String = tbl
                .get("action")
                .map_err(|_| mlua::Error::RuntimeError("bind{} is missing required field 'action'".into()))?;

            let keysym = match keysym_from_name(&key_str) {
                Some(k) => k,
                None => {
                    tracing::warn!("dawn/config: unknown key name '{}', skipping bind", key_str);
                    return Ok(());
                }
            };
            let action = match action_from_lua(&action_str, &tbl) {
                Some(a) => a,
                None => return Ok(()),
            };
            bindings.borrow_mut().push(KeyBinding {
                mods: ModMask::parse(&mods_str),
                keysym,
                action,
            });
            Ok(())
        })?;
        lua.globals().set("bind", bind_fn)?;
    }
    {
        // gesture{ mods="alt", fingers=3, kind="swipe", where="window",
        //          action="resize-window" }
        //
        // Устроено как bind{}, и это не совпадение: жест — такой же бинд, просто
        // триггер у него не клавиша, а пальцы. Единственное, чего нет у клавиш,
        // — контекст (`where`): пальцы начинают жест В КАКОМ-ТО МЕСТЕ, и «над
        // окном» против «по пустому холсту» — половина смысла.
        let gestures = gestures.clone();
        let отключённые = отключённые.clone();
        let gesture_fn = lua.create_function(move |_, tbl: Table| {
            let mods_str: String = tbl.get("mods").unwrap_or_default();
            let kind_str: String = tbl.get("kind").unwrap_or_else(|_| "swipe".to_string());
            let where_str: String = tbl.get("where").unwrap_or_default();
            let fingers: u32 = tbl.get::<i32>("fingers").unwrap_or(3).clamp(1, 10) as u32;
            let action_str: String = tbl.get("action").map_err(|_| {
                mlua::Error::RuntimeError("gesture{} is missing required field 'action'".into())
            })?;

            let Some(триггер) = crate::gestures::Триггер::разобрать(&kind_str, fingers) else {
                tracing::warn!("dawn/config: неизвестный вид жеста '{kind_str}', пропускаю");
                return Ok(());
            };
            let Some(где) = crate::gestures::Где::разобрать(&where_str) else {
                tracing::warn!("dawn/config: неизвестный контекст жеста '{where_str}', пропускаю");
                return Ok(());
            };
            // `action = "none"` — не действие, а ОТКАЗ: строка гасит жест,
            // включая встроенное умолчание с тем же триггером. Дальше этот
            // жест уходит в прежние ветки input.rs, как будто в таблице его
            // нет вовсе, — иначе выключить умолчание было бы нечем.
            if matches!(action_str.trim().to_ascii_lowercase().as_str(), "none" | "nop") {
                отключённые.borrow_mut().push((ModMask::parse(&mods_str), триггер, где));
                return Ok(());
            }
            let Some(действие) = действие_жеста(&action_str, &tbl) else {
                tracing::warn!("dawn/config: неизвестное действие жеста '{action_str}', пропускаю");
                return Ok(());
            };
            // Непрерывные действия привязаны к своему типу жеста намертво.
            // Пан и зум — это поток дельт: повесь их на `pinch-in`, который
            // срабатывает один раз, и жест просто не делал бы ничего, а
            // выглядело бы это как «бинд не работает».
            if let crate::gestures::ДействиеЖеста::Непрерывно(н) = &действие {
                if !непрерывное_подходит(*н, триггер) {
                    tracing::warn!(
                        "dawn/config: '{action_str}' непрерывно и на '{kind_str}' не вешается — пропускаю",
                    );
                    return Ok(());
                }
            }
            gestures.borrow_mut().push(crate::gestures::БиндЖеста {
                mods: ModMask::parse(&mods_str),
                триггер,
                где,
                действие,
            });
            Ok(())
        })?;
        lua.globals().set("gesture", gesture_fn)?;
    }
    {
        let xkb_settings = xkb_settings.clone();
        let xkb_fn = lua.create_function(move |_, tbl: Table| {
            let mut s = xkb_settings.borrow_mut();
            if let Ok(v) = tbl.get::<String>("layout") {
                s.layout = v;
            }
            if let Ok(v) = tbl.get::<String>("variant") {
                s.variant = v;
            }
            if let Ok(v) = tbl.get::<String>("model") {
                s.model = v;
            }
            if let Ok(v) = tbl.get::<String>("rules") {
                s.rules = v;
            }
            if let Ok(v) = tbl.get::<String>("options") {
                s.options = Some(v);
            }
            Ok(())
        })?;
        lua.globals().set("xkb", xkb_fn)?;
    }
    {
        let bird_eye_key = bird_eye_key.clone();
        let bt_autoconnect = bt_autoconnect.clone();
        let cursor_size = cursor_size.clone();
        let cursor_client_max = cursor_client_max.clone();
        let anim_speed = anim_speed.clone();
        let pan_drift = pan_drift.clone();
        let fling_distance = fling_distance.clone();
        let infinite_wallpaper = infinite_wallpaper.clone();
        let share_guest_all = share_guest_all.clone();
        let keyboard_grab_apps = keyboard_grab_apps.clone();
        let gesture_thresholds = gesture_thresholds.clone();
        let автодовод = автодовод.clone();
        let blur = blur.clone();
        let close_anim = close_anim.clone();
        // ВАЖНО: булевы ключи спрашиваются как `Option<bool>`, а не как `bool`.
        //
        // mlua переводит в bool ЛЮБОЕ значение по правилу истинности Lua, и
        // отсутствующий ключ (nil) — это `Ok(false)`, а не ошибка. То есть
        // `if let Ok(v) = tbl.get::<bool>("blur")` срабатывает ВСЕГДА, и каждый
        // вызов `set{}` гасил все булевы настройки, которых в нём не назвали.
        // Настоящий config.lua зовёт `set{}` семь раз, последний —
        // `set{ blur = true }` (строка 430), поэтому до сегодня молча стояли в
        // false и `infinite_wallpaper`, и `close_anim`, и
        // `bluetooth_autoconnect`, как бы их ни выставляли выше. Вылезло это
        // как «обои пропали на втором мониторе»: без бесконечных обоев картинка
        // рисуется обычной layer-поверхностью, а она есть только у того выхода,
        // которому её отдал dwall (26.08.2026).
        //
        // `Option<bool>` разводит «ключа нет» (None) и «ключ есть» (Some).
        let set_fn = lua.create_function(move |_, tbl: Table| {
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("bluetooth_autoconnect") {
                *bt_autoconnect.borrow_mut() = v;
            }
            if let Ok(v) = tbl.get::<i32>("cursor_size") {
                *cursor_size.borrow_mut() = v.clamp(0, 256);
            }
            if let Ok(v) = tbl.get::<i32>("cursor_client_max") {
                *cursor_client_max.borrow_mut() = v.clamp(-1, 256);
            }
            if let Ok(v) = tbl.get::<f64>("anim_speed") {
                *anim_speed.borrow_mut() = v;
            }
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("close_anim") {
                *close_anim.borrow_mut() = v;
            }
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("blur") {
                *blur.borrow_mut() = v;
            }
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("infinite_wallpaper") {
                *infinite_wallpaper.borrow_mut() = v;
            }
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("share_guest_all") {
                *share_guest_all.borrow_mut() = v;
            }
            if let Ok(Some(v)) = tbl.get::<Option<Vec<String>>>("keyboard_grab_apps") {
                *keyboard_grab_apps.borrow_mut() = v;
            }
            // Пороги жестов — те же имена, что в driftwm, чтобы настройки
            // переносились между композиторами без перевода.
            if let Ok(v) = tbl.get::<f64>("swipe_threshold") {
                if v > 0.0 {
                    gesture_thresholds.borrow_mut().свайп = v;
                }
            }
            if let Ok(v) = tbl.get::<f64>("pinch_in_threshold") {
                if v > 0.0 && v < 1.0 {
                    gesture_thresholds.borrow_mut().щипок_внутрь = v;
                }
            }
            if let Ok(v) = tbl.get::<f64>("pinch_out_threshold") {
                if v > 1.0 {
                    gesture_thresholds.borrow_mut().щипок_наружу = v;
                }
            }
            // Автодовод по краям накладки тачпада.
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("touchpad_edge_motion") {
                автодовод.borrow_mut().включён = v;
            }
            if let Ok(v) = tbl.get::<f64>("touchpad_edge_zone") {
                // Половина накладки в качестве «края» — это уже не край, а вся
                // накладка: выше 0.4 не пускаем.
                автодовод.borrow_mut().зона = v.clamp(0.0, 0.4);
            }
            if let Ok(v) = tbl.get::<f64>("touchpad_edge_speed") {
                автодовод.borrow_mut().скорость = v.clamp(0.0, 10000.0);
            }
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("touchpad_edge_only_drag") {
                автодовод.borrow_mut().только_при_тяге = v;
            }
            if let Ok(v) = tbl.get::<f64>("pan_drift") {
                *pan_drift.borrow_mut() = if v.is_finite() { v.clamp(0.0, 1.0) } else { 0.5 };
            }
            if let Ok(v) = tbl.get::<f64>("fling_distance") {
                *fling_distance.borrow_mut() = if v.is_finite() { v.clamp(0.0, 8.0) } else { 1.0 };
            }
            if let Ok(v) = tbl.get::<String>("bird_eye_key") {
                if let Some(k) = keysym_from_name(&v) {
                    *bird_eye_key.borrow_mut() = k;
                } else {
                    tracing::warn!("dawn/config: unknown bird_eye_key '{}'", v);
                }
            }
            Ok(())
        })?;
        lua.globals().set("set", set_fn)?;
    }
    {
        // dwindle{} — те же ручки, что dwindle:* в hyprland.conf (см. dwindle.rs).
        let dwindle_cfg = dwindle_cfg.clone();
        let dwindle_fn = lua.create_function(move |_, tbl: Table| {
            let mut d = dwindle_cfg.borrow_mut();
            if let Ok(v) = tbl.get::<f32>("split_width_multiplier") {
                d.split_width_multiplier = v.max(0.1);
            }
            // `Option<bool>`, а не `bool`, — см. разбор у `set{}` выше.
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("preserve_split") {
                d.preserve_split = v;
            }
            if let Ok(v) = tbl.get::<i32>("force_split") {
                d.force_split = v.clamp(0, 2) as u8;
            }
            if let Ok(v) = tbl.get::<f32>("default_split_ratio") {
                d.default_split_ratio = v.clamp(
                    crate::dwindle::RATIO_MIN as f32,
                    crate::dwindle::RATIO_MAX as f32,
                );
            }
            Ok(())
        })?;
        lua.globals().set("dwindle", dwindle_fn)?;
    }
    {
        // mine{} — Minecraft. Зона задаётся в БЛОКАХ: человек, расчищающий
        // место под панели, считает блоки, а не метры, — хотя это одно и то же.
        let mine_cfg = mine_cfg.clone();
        let mine_fn = lua.create_function(move |_, tbl: Table| {
            let mut м = mine_cfg.borrow_mut();
            if let Ok(v) = tbl.get::<f32>("width") {
                м.зона_ширина = v.clamp(2.0, 64.0);
            }
            if let Ok(v) = tbl.get::<f32>("depth") {
                м.зона_глубина = v.clamp(2.0, 64.0);
            }
            if let Ok(v) = tbl.get::<f32>("scale") {
                // Те же честные границы, что у vr{ scale }: 0.0001 м/пкс — окно
                // 1920 шириной в 19 см (не прочитать), 0.01 — в 19 блоков (не
                // поместится в зону).
                м.метров_на_пиксель = v.clamp(0.0001, 0.01);
            }
            Ok(())
        })?;
        lua.globals().set("mine", mine_fn)?;
    }
    {
        // vr{} — шлем. Имена раскладок принимаем и по-русски, и по-английски:
        // config.lua у Ярика русский, а примеры в сети — нет.
        let vr_cfg = vr_cfg.clone();
        let vr_fn = lua.create_function(move |_, tbl: Table| {
            let mut в = vr_cfg.borrow_mut();
            if let Ok(имя) = tbl.get::<String>("layout") {
                use crate::vr::scene::Раскладка::*;
                match имя.trim().to_lowercase().as_str() {
                    "дуга" | "arc" => в.layout = Дуга,
                    "стена" | "wall" => в.layout = Стена,
                    "купол" | "dome" => в.layout = Купол,
                    "свободно" | "free" => в.layout = Свободно,
                    иное => tracing::warn!("dawn/config: vr{{}} не знает раскладку '{}'", иное),
                }
            }
            if let Ok(v) = tbl.get::<f32>("scale") {
                // Границы честные: 0.0001 м/пкс — окно 1920 шириной в 19 см
                // (не прочитать), 0.01 — в девятнадцать метров (не поместится).
                в.scale = v.clamp(0.0001, 0.01);
            }
            if let Ok(v) = tbl.get::<f32>("radius") {
                в.radius = v.clamp(0.0, 10.0);
            }
            // `Option<bool>` — та же грабля с nil, что и у set{} выше.
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("ar") {
                в.ar = v;
            }
            if let Ok(Some(v)) = tbl.get::<Option<bool>>("auto") {
                в.auto = v;
            }
            // apps = { { name = …, cmd = … }, … } — кнопки пульта «Пуск».
            // Список ЗАМЕЩАЕТ прежний, а не дополняет: иначе перечитывание
            // конфига удваивало бы кнопки на каждом Super+R.
            if let Ok(список) = tbl.get::<Table>("apps") {
                let mut свои = Vec::new();
                for пара in список.sequence_values::<Table>().flatten() {
                    let cmd = пара.get::<String>("cmd").unwrap_or_default();
                    if cmd.trim().is_empty() {
                        tracing::warn!("dawn/config: vr{{ apps }} — пункт без cmd, пропущен");
                        continue;
                    }
                    let name = пара.get::<String>("name").unwrap_or_else(|_| cmd.clone());
                    свои.push((name, cmd));
                }
                if !свои.is_empty() {
                    в.apps = свои;
                }
            }
            // gestures = { fist = "vr_launcher", swipe_left = { action = … } }
            // Значение — либо имя действия строкой (когда аргументов нет), либо
            // таблица с `action` и аргументами, ровно как в `bind{}`. Разбор
            // ОДИН на оба случая: строка превращается в пустую таблицу, и
            // дальше работает тот же `action_from_lua`, что у клавиш, — иначе
            // жесты и бинды разошлись бы в мелочах на первой же правке.
            if let Ok(таблица) = tbl.get::<Table>("gestures") {
                for пара in таблица.pairs::<String, mlua::Value>().flatten() {
                    let (имя_жеста, знач) = пара;
                    let имя_действия =
                        |д: &str| д.trim().to_ascii_lowercase().replace('-', "_");
                    let разобрано = match знач {
                        mlua::Value::String(с) => с
                            .to_str()
                            .ok()
                            .and_then(|д| action_from_str(&имя_действия(&д), "")),
                        mlua::Value::Table(t) => match t.get::<String>("action") {
                            Ok(д) => action_from_lua(&имя_действия(&д), &t),
                            Err(_) => {
                                tracing::warn!(
                                    "dawn/config: vr{{ gestures }} — у «{}» нет action",
                                    имя_жеста
                                );
                                None
                            }
                        },
                        _ => None,
                    };
                    let ключ = имя_жеста.trim().to_ascii_lowercase().replace('-', "_");
                    if !crate::vr::input::Жест::все().iter().any(|ж| ж.имя() == ключ) {
                        tracing::warn!("dawn/config: vr{{ gestures }} не знает жест '{}'", ключ);
                        continue;
                    }
                    match разобрано {
                        Some(д) => {
                            в.gestures.insert(ключ, д);
                        }
                        None => tracing::warn!(
                            "dawn/config: vr{{ gestures }} — действие жеста '{}' не разобралось",
                            ключ
                        ),
                    }
                }
            }
            Ok(())
        })?;
        lua.globals().set("vr", vr_fn)?;
    }
    {
        // monitor{} — конфигурация выхода: имя коннектора (DP-2, HDMI-A-1) или
        // модель из EDID. Режим ищется по width/height/refresh; если точного
        // нет — берётся ближайший по частоте среди подходящих по размеру.
        let monitors = monitors.clone();
        let monitor_fn = lua.create_function(move |_, tbl: Table| {
            let name = match tbl.get::<String>("name") {
                Ok(n) => n,
                Err(_) => {
                    tracing::warn!("dawn/config: monitor{{}} без name — пропущен");
                    return Ok(());
                }
            };
            let layout_set = tbl.contains_key("x").unwrap_or(false)
                || tbl.contains_key("y").unwrap_or(false);
            monitors.borrow_mut().push(MonitorConfig {
                name,
                width: tbl.get::<i32>("width").unwrap_or(0),
                height: tbl.get::<i32>("height").unwrap_or(0),
                refresh: tbl.get::<i32>("refresh").unwrap_or(0),
                x: tbl.get::<i32>("x").unwrap_or(0),
                y: tbl.get::<i32>("y").unwrap_or(0),
                layout_set,
                scale: tbl.get::<f64>("scale").unwrap_or(1.0).clamp(0.25, 8.0),
                transform: tbl.get::<String>("transform")
                    .unwrap_or_else(|_| "normal".into()),
                tag: tbl.get::<u32>("tag").unwrap_or(0).min(9),
                primary: tbl.get::<bool>("primary").unwrap_or(false),
            });
            Ok(())
        })?;
        lua.globals().set("monitor", monitor_fn)?;
    }

    lua.load(source).exec()?;

    // ── Жесты driftwm по умолчанию ───────────────────────────────────────────
    //
    // Таблица включается ПОСЛЕ конфига человека и намеренно тем же путём —
    // через ту же функцию `gesture{}`. Так у умолчаний и у ручных биндов один
    // разбор, одни проверки и один список опечаток: разойтись им негде.
    //
    // Приоритет — за человеком: `gestures::найти` берёт ПЕРВОЕ совпадение, а
    // ниже мы выбрасываем умолчание, если такой же триггер с теми же
    // модификаторами и контекстом человек уже задал сам.
    //
    // До 01.09.2026 этот список лежал в `default_config.lua` закомментированным
    // — то есть жестов из driftwm по умолчанию не было ни одного, и Ярик
    // попросил их наконец включить. Что при этом перебивается прежним
    // поведением dawn, сказано в комментариях внутри.
    lua.load(ЖЕСТЫ_ПО_УМОЛЧАНИЮ).exec()?;
    {
        let mut таблица = gestures.borrow_mut();
        let mut видели: Vec<(ModMask, crate::gestures::Триггер, crate::gestures::Где)> =
            Vec::with_capacity(таблица.len());
        таблица.retain(|б| {
            let ключ = (б.mods, б.триггер, б.где);
            if видели.contains(&ключ) {
                return false;
            }
            видели.push(ключ);
            true
        });
        let гашения = отключённые.borrow();
        таблица.retain(|б| !гашения.contains(&(б.mods, б.триггер, б.где)));
        tracing::info!("dawn/config: жестов тачпада {}", таблица.len());
    }

    let result = Config {
        bindings: bindings.borrow().clone(),
        gestures: gestures.borrow().clone(),
        gesture_thresholds: *gesture_thresholds.borrow(),
        автодовод: *автодовод.borrow(),
        xkb: xkb_settings.borrow().clone(),
        bird_eye_key: *bird_eye_key.borrow(),
        dwindle: *dwindle_cfg.borrow(),
        bluetooth_autoconnect: *bt_autoconnect.borrow(),
        monitors: monitors.borrow().clone(),
        vr: vr_cfg.borrow().clone(),
        mine: mine_cfg.borrow().clone(),
        cursor_size: *cursor_size.borrow(),
        cursor_client_max: *cursor_client_max.borrow(),
        anim_speed: *anim_speed.borrow(),
        pan_drift: *pan_drift.borrow(),
        fling_distance: *fling_distance.borrow(),
        infinite_wallpaper: *infinite_wallpaper.borrow(),
        share_guest_all: *share_guest_all.borrow(),
        keyboard_grab_apps: keyboard_grab_apps.borrow().clone(),
        blur: *blur.borrow(),
        close_anim: *close_anim.borrow(),
    };
    Ok(result)
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

impl Dawn {
    /// Runs the effect of one resolved [`Action`]. Called from the keyboard
    /// handler in input.rs once it finds a matching binding in
    /// `self.lua_config.bindings` for the pressed (mods, keysym) pair.
    pub fn dispatch_action(&mut self, action: Action) {
        use Action::*;
        match action {
            Spawn(cmd) => self.spawn(&cmd),
            Quit => {
                tracing::info!("dawn: quit");
                crate::session::save(&self.tagged_windows);
                // Флаг, а не только loop_signal: главный цикл в main.rs
                // крутится вручную и сигнал не смотрит (см. state::ExitAction).
                self.exit = Some(crate::state::ExitAction::Quit);
                self.loop_signal.stop();
            }
            Restart => {
                tracing::info!("dawn: restart");
                crate::session::save(&self.tagged_windows);
                self.exit = Some(crate::state::ExitAction::Restart);
                self.loop_signal.stop();
            }
            Kill => self.kill_selected_or_focused(),
            SetLayout(l) => self.set_layout(l),
            // Super+D:
            //  · нет выделения        → тумблер Float↔Tile (собирает все окна
            //                           в тайлинг или разбрасывает во Float);
            //  · выделение = созвездие → расцепить его (разлёт с анимацией);
            //  · иначе выделение      → магнитно стянуть в новое созвездие.
            ToggleLayoutFloatTile => {
                // В обзоре столов (тап Super) Win+D не работает: обзор сам
                // раскладывает ленту столов и держит камеру/зум, а смена
                // layout'а из-под него ломает и то, и другое. Выйти из обзора
                // можно тапом Super / ПКМ / кликом по столу.
                if self.overview_active {
                    return;
                }
                self.momentum.stop();
                self.camera_anim = None;
                self.zoom_anim = None;
                if self.selected_windows.is_empty() {
                    if self.tile_config.layout == Layout::Tile {
                        // Tile → Float (тумблер)
                        self.set_layout(Layout::Float);
                    } else {
                        // Float/Columns/Monocle → Tile (сборка)
                        self.gather_all_into_tiling();
                    }
                } else if self.selection_is_constellation() && !self.selection_is_torn() {
                    // Решает не картинка, а то, ТРОГАЛИ ли гроздь руками: целое
                    // созвездие нажатие разбирает, растащенное — собирает
                    // заново (иначе его нельзя было бы собрать тем же
                    // нажатием — оно сразу разбиралось).
                    //
                    // Раньше «целое» определялось геометрией (окна лежат
                    // вплотную). Ресайз ОДНОГО окна оставлял между ним и
                    // соседом дыру, гроздь считалась растащенной, и созвездие
                    // переставало разбираться вовсе — см.
                    // TaggedWindow::constellation_torn.
                    self.scatter_selected_constellation();
                } else {
                    self.gather_selected_into_constellation();
                }
            }
            Zoom => self.zoom(),
            ToggleFloatingFocused => self.toggle_floating(),
            ToggleFullscreen => self.toggle_fullscreen(),
            ToggleOverview => self.toggle_overview(),
            CenterWindow => self.center_window(),
            FloatSelected => self.float_selected(),
            // В Columns стрелки листают колонки/строки (niri), в остальных
            // режимах — пространственная навигация как раньше.
            FocusDirection(dx, dy) => {
                if self.tile_config.layout == Layout::Columns {
                    self.columns_focus(dx, dy);
                } else {
                    self.focus_direction(dx, dy);
                }
            }
            FocusStack(dir) => {
                if self.tile_config.layout == Layout::Columns {
                    self.columns_focus_flattened(dir);
                } else {
                    self.focus_stack(dir);
                }
            }
            CycleStack(dir) => self.cycle_stack(dir),
            WindowSearch => self.search_toggle(),
            // Super+Comma/Period: в Columns — consume/expel окна в стопку
            // колонки; иначе — inc/dec nmaster в тайлинге.
            IncNmaster(n) => {
                if self.tile_config.layout == Layout::Columns {
                    if n > 0 { self.columns_consume(); } else { self.columns_expel(); }
                } else {
                    self.inc_nmaster(n);
                }
            }
            SetMfact(delta) => self.set_mfact(delta),
            // Super+Ctrl+стрелки: в Columns — двигают колонку (←→) / окно в
            // колонке (↑↓); иначе — двигают плавающее окно.
            MoveFocused(dx, dy) => {
                if self.tile_config.layout == Layout::Columns {
                    if dx != 0 { self.columns_move_column(dx); }
                    if dy != 0 { self.columns_move_window(dy); }
                } else if self.tile_config.layout != Layout::Float {
                    // Tile/Monocle: двигаем окно в раскладке свапом (Hyprland-style).
                    self.move_tiled_window(dx, dy);
                } else {
                    self.move_focused_window(dx, dy);
                }
            }
            ResizeFocused(dw, dh) => self.keyboard_resize_focused(dw, dh),
            // Super+1-9 double as camera-bookmark slots while bookmarks_mode
            // is on (Super+B) — same override the old hardcoded handler had.
            ViewTag(mask) => {
                // Цифра = бит тега напрямую: нумерация столов ОДНА на весь
                // композитор. Раньше она была относительной («N-й стол своей
                // изоляции»), и из-за этого Win+1 из ленты вёл на первый этаж
                // ленты, а не в тайлинг. Теперь режим задаёт сам стол:
                // 1 — Tile, 2 — Columns (niri), 3 — Float, см.
                if self.overview_active {
                    // Super+1-9 в обзоре столов → выйти из обзора на воркспейс.
                    self.exit_overview_immediate(Some(mask));
                } else if self.bookmarks_mode {
                    self.jump_to_camera_bookmark(mask.trailing_zeros() + 1);
                } else {
                    // Внутри ленты Win+цифра остаётся её собственным переходом
                    // по этажам: направление слайда задаём по разнице этажей,
                    // чтобы стол въезжал вертикально так же, как от
                    // Super+PageUp/Down. Для столов ВНЕ ленты (Win+1, Win+3)
                    // этажа нет — там обычный выход в чужой режим, слайд не
                    // нужен и считался бы по несуществующей позиции.
                    if self.tile_config.layout == Layout::Columns
                        && !self.columns_tag_foreign(mask)
                    {
                        let cur = self.columns_floor_index(self.viewport.current_tags());
                        let dst = self.columns_floor_index(mask);
                        self.columns_ws_slide = (dst - cur).signum();
                    }
                    self.view_tag(mask);
                }
            }
            TagWindow(mask) => {
                if self.bookmarks_mode {
                    self.save_camera_bookmark(mask.trailing_zeros() + 1);
                } else {
                    // Нумерация та же, что у Win+цифра — глобальная. Окно можно
                    // отправить и в чужой режим (Win+Shift+2 кладёт его в
                    // ленту): полоса колонок подберёт его при первом же заходе
                    // на стол, см. columns_reconcile.
                    self.tag_window(mask);
                }
            }
            // Смешанный набор видимых тегов (Win+Ctrl+цифра) ленту ломает: она
            // считает текущий тег ОДНИМ столом-этажом (columns_floor_index,
            // columns_ws_y, полка полосы в columns_by_tag), а с двумя битами в
            // маске «этаж» перестаёт существовать. В niri-режиме эти действия
            // просто не работают — столы там изолированы друг от друга.
            ToggleView(_) | ToggleTag(_) if self.tile_config.layout == Layout::Columns => {
                tracing::debug!("dawn/columns: смешивать столы в ленте нельзя (Win+Ctrl+цифра)");
            }
            ToggleView(mask) => self.toggle_view(mask),
            ToggleTag(mask) => self.toggle_tag(mask),
            ToggleMinimap => {
                self.is_minimap_visible = !self.is_minimap_visible;
                tracing::info!("dawn: is_minimap_visible={}", self.is_minimap_visible);
            }
            Screenshot => self.snip_start(),
            TogglePortal => self.toggle_portal(),
            ToggleBookmarksMode => {
                self.bookmarks_mode = !self.bookmarks_mode;
                tracing::info!("dawn: bookmarks_mode={}", self.bookmarks_mode);
            }
            ToggleSnapping => {
                self.is_snapping_enabled = !self.is_snapping_enabled;
                tracing::info!("dawn: is_snapping_enabled={}", self.is_snapping_enabled);
            }
            ToggleMagnetism => {
                self.is_magnetism_enabled = !self.is_magnetism_enabled;
                tracing::info!("dawn: is_magnetism_enabled={}", self.is_magnetism_enabled);
            }
            ShareStart(порт) => self.раздача_по_команде(порт),
            ShareStop => self.раздача_закончить(),
            // Повторное нажатие НЕ выключает раздачу, а открывает панель
            // управления: выключить всем — там же, отдельной клавишей.
            // Прежнее поведение оставляло только «всё или ничего»: убрать
            // одного назойливого гостя было нельзя, не выгнав заодно всех
            // остальных. См. `share::Dawn::раздача_переключить`.
            ShareToggle(порт) => self.раздача_переключить(порт),
            ToggleFoldStack => self.toggle_fold_stack(),
            VtSwitch(vt) => {
                tracing::info!("dawn: VT switch → {}", vt);
                if let Some(ref mut session) = self.session {
                    let _ = session.change_vt(vt);
                }
            }
            LayoutNext => self.cycle_xkb_layout(true),
            LayoutPrev => self.cycle_xkb_layout(false),
            ReloadConfig => self.reload_config(),
            GroupSelected => self.group_selected_into_constellation(),
            UngroupSelected => self.ungroup_focused_constellation(),
            PinBookmarkAtCursor => self.pin_bookmark_at_cursor(),
            DeleteNearestBookmark => self.delete_nearest_bookmark(),
            JumpBookmark(slot) => self.jump_to_camera_bookmark(slot),
            // Всё, что ниже до ColumnCenterMode, имеет смысл только в Columns:
            // в Tile/Float/Monocle геометрией распоряжается своя раскладка, и
            // «сделать колонку шире» там нечего.
            ColumnWidthCycle | WindowHeightCycle | ColumnWidthAdjust(_)
            | WindowHeightAdjust(_) | WindowHeightReset | ColumnMaximize
            | ColumnCenter | ColumnFocusEdge(_) | ColumnMoveToEdge(_)
            | ColumnConsumeOrExpel(_) | ColumnToggleTabbed | ColumnFocusOtherLayer
                if self.tile_config.layout != Layout::Columns =>
            {
                tracing::debug!("dawn/columns: действие только для режима Columns (Super+N)");
            }
            ColumnWidthCycle => self.columns_cycle_width(),
            WindowHeightCycle => self.columns_cycle_height(),
            ColumnWidthAdjust(p) => self.columns_adjust_width(p),
            WindowHeightAdjust(p) => self.columns_adjust_height(p),
            WindowHeightReset => self.columns_reset_heights(),
            ColumnMaximize => self.columns_maximize(),
            ColumnCenter => self.columns_center_active(),
            ColumnFocusEdge(last) => self.columns_focus_edge(last),
            ColumnMoveToEdge(last) => self.columns_move_to_edge(last),
            ColumnConsumeOrExpel(dir) => self.columns_consume_or_expel(dir),
            ColumnToggleTabbed => self.columns_toggle_tabbed(),
            ColumnFocusOtherLayer => self.columns_focus_other_layer(),
            ColumnCenterMode(mode) => {
                self.columns.center_focused = mode;
                tracing::info!("dawn/columns: center-focused-column = {:?}", mode);
                self.columns_scroll_to_active();
            }
            // ── Шлем ──────────────────────────────────────────────────────
            // Вход и выход — одно действие: человек не должен помнить, в шлеме
            // он сейчас или нет, это и так видно по тому, что на голове.
            VrToggle => {
                if self.vr.is_some() {
                    crate::vr::выключить(self);
                } else if let Err(e) = crate::vr::включить(self) {
                    // Ошибку показываем строкой в панели: в шлеме её никто не
                    // прочитает, а вот на мониторе — ровно тот человек, который
                    // нажал бинд.
                    self.уведомить(&format!("VR: {e}"));
                }
            }
            VrMode => crate::vr::режим(self),
            MineMode => crate::mine::режим(self),
            MineLayout => {
                let р = crate::mine::сменить_раскладку(self);
                self.уведомить(&format!("Minecraft: раскладка «{}»", р.имя()));
            }
            MineGrab => crate::mine::хват_тумблер(self),
            VrAr => {
                if self.vr.is_none() {
                    self.уведомить("VR: сначала включи шлем (Super+Alt+V)");
                } else {
                    use crate::vr::Ар;
                    let исход = crate::vr::переключить_ар(self);
                    // При отказе показываем ИМЕННО то, что объявил рантайм.
                    // «Не показывает комнату» без этого списка неотличимо от
                    // нашей же ошибки, а разница решающая: нет ALPHA_BLEND —
                    // чинить надо passthrough в клиенте WiVRn, а не dawn.
                    let подробно = self
                        .vr
                        .as_ref()
                        .map(|вр| вр.шлем.смешивание_строкой())
                        .unwrap_or_default();
                    self.уведомить(&match исход {
                        Ар::Включена => "VR: дополненная реальность".to_string(),
                        Ар::Выключена => "VR: обычный режим".to_string(),
                        Ар::НеУмеет => {
                            format!("VR: шлем не показывает комнату ({подробно})")
                        }
                    });
                }
            }
            VrLayout => {
                let р = crate::vr::сменить_раскладку(self);
                self.уведомить(&format!("VR: раскладка «{}»", р.имя()));
            }
            VrRecenter => crate::vr::пересобрать(self),
            VrLauncher | VrKeyboard => {
                let вид = match action {
                    VrLauncher => crate::vr::ui::Вид::Пуск,
                    _ => crate::vr::ui::Вид::Клавиатура,
                };
                match crate::vr::пульт(self, вид) {
                    Ok(открыт) => self.уведомить(&format!(
                        "VR: пульт «{}» {}",
                        вид.имя(),
                        if открыт { "открыт" } else { "спрятан" }
                    )),
                    Err(e) => self.уведомить(&format!("VR: {e}")),
                }
            }
            BluetoothMenu => self.bt_toggle_menu(),
            TrayMenu => self.tray_toggle(),
            WifiMenu => self.wifi_toggle_menu(),
            AudioMenu => self.audio_toggle_menu(),
            BluetoothPower => {
                let on = self.bt.as_ref().is_some_and(|b| b.snap.powered);
                self.bt_send(crate::bluetooth::Cmd::Power(!on));
            }
            WorkspaceStep(d) => self.workspace_step(d),
            MoveColumnToWorkspace(d) => self.columns_move_to_workspace(d),
            ToggleNiriMode => {
                if self.tile_config.layout == Layout::Columns {
                    // В niri режиме Win+N → вывести окна во Float и выйти из niri
                    self.set_layout(Layout::Float);
                } else {
                    // Вне niri: Win+N → войти в niri, сохранив предыдущий layout
                    self.prev_layout_before_niri = self.tile_config.layout;
                    self.set_layout(Layout::Columns);
                }
            }
        }
    }

    pub(crate) fn spawn(&self, cmd: &str) {
        let socket = self.socket_name.to_string_lossy().to_string();
        tracing::info!("dawn: spawn '{}'", cmd);
        match std::process::Command::new("sh")
            .arg("-c")
            .arg(cmd)
            .env("WAYLAND_DISPLAY", &socket)
            .env("QT_QPA_PLATFORM", "wayland")
            .env("GDK_BACKEND", "wayland")
            .spawn()
        {
            // Ребёнка надо дождаться, иначе он навсегда останется зомби: ядро
            // держит запись в таблице процессов, пока родитель не забрал код
            // возврата, а родитель здесь — сам компоновщик, и живёт он всю
            // сессию. Прежнее `Ok(_) => {}` роняло Child на месте, и за
            // сессию накапливались десятки <defunct> (в логе 09.08.2026 их
            // было полтора десятка от одного ghostty).
            //
            // Ждём в отдельном потоке: приложение живёт часами, а главный цикл
            // не имеет права остановиться ни на миг. Поток всё это время спит
            // в waitpid и стоит лишь своего стека.
            //
            // SIGCHLD=SIG_IGN, который делает то же самое одной строкой, тут
            // не годится: тогда ломаются Command::status() и ::output() в
            // tray.rs и audio.rs — им нужен код возврата, а с SIG_IGN они
            // получают ECHILD.
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => tracing::warn!("dawn: spawn '{}' failed: {}", cmd, e),
        }
    }

    /// Cycles the active XKB layout group forward/backward (e.g. en → ru),
    /// entirely client-side in xkbcommon — no keymap recompile needed since
    /// all configured layouts were already compiled into the keymap at
    /// startup/reload (see `xkb{ layout = "us,ru" }` in config.lua).
    fn cycle_xkb_layout(&mut self, forward: bool) {
        if let Some(kb) = self.seat.get_keyboard() {
            kb.with_xkb_state(self, |mut ctx| {
                if forward { ctx.cycle_next_layout(); } else { ctx.cycle_prev_layout(); }
            });
        }
        // Панель показывает раскладку (см. bar.rs) и сама её не опрашивает:
        // xkb-состояние живёт под мьютексом клавиатуры, дёргать его на каждый
        // кадр незачем. Значит, обновить надпись обязан тот, кто раскладку и
        // поменял.
        self.refresh_kb_layout();
        self.request_redraw();
    }

    /// Re-reads `~/.config/dawn/config.lua` (Super+Shift+C) and swaps in the
    /// new bindings + xkb settings live, without restarting the compositor.
    fn reload_config(&mut self) {
        let new_cfg = load();
        if let Some(kb) = self.seat.get_keyboard() {
            if let Err(e) = kb.set_xkb_config(self, new_cfg.xkb_config()) {
                tracing::warn!("dawn/config: failed to apply reloaded xkb config: {:?}", e);
            }
        }
        tracing::info!("dawn: config reloaded ({} binds)", new_cfg.bindings.len());
        // Инерция холста живёт в MomentumState, а не спрашивается у конфига на
        // каждый кадр — значит при перечитывании её надо переставить руками.
        self.momentum.drift = new_cfg.pan_drift;
        self.lua_config = new_cfg;
        // Список раскладок мог смениться прямо сейчас — надпись в панели берёт
        // коды именно из конфига (см. Dawn::refresh_kb_layout).
        self.refresh_kb_layout();
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    /// Разбор конфига по-настоящему проверить негде, кроме как разобрав его:
    /// опечатка в имени действия (`window_search` против `search_window`) не
    /// ломает ни сборку, ни запуск — бинд просто молча не работает.
    fn действие(cfg: &Config, mods: ModMask, key: u32) -> String {
        format!("{:?}", cfg.find_action(mods, key))
    }

    #[test]
    fn встроенный_конфиг_разбирается() {
        let cfg = load_from_str(DEFAULT_CONFIG_LUA).expect("default_config.lua не разобрался");
        assert!(cfg.bindings.len() > 40, "биндов подозрительно мало: {}", cfg.bindings.len());
    }

    /// Выход и перезапуск. Проверять стоит именно тут: опечатка в имени
    /// действия оставила бы Super+Shift+Q без обработчика молча, а сам он и
    /// так уже однажды «работал» вхолостую — писал в лог и не выходил
    /// (см. state::ExitAction).
    #[test]
    fn super_shift_q_выходит_а_super_r_перезапускает() {
        let cfg = load_from_str(DEFAULT_CONFIG_LUA).unwrap();
        let logo = ModMask { ctrl: false, alt: false, shift: false, logo: true };
        let logo_shift = ModMask { ctrl: false, alt: false, shift: true, logo: true };
        let logo_alt = ModMask { ctrl: false, alt: true, shift: false, logo: true };
        assert_eq!(действие(&cfg, logo_shift, xkb::keysyms::KEY_q), "Some(Quit)");
        assert_eq!(действие(&cfg, logo, xkb::keysyms::KEY_r), "Some(Restart)");
        // Пресеты ширины колонок уехали с Super+R, но не потерялись.
        assert_eq!(действие(&cfg, logo_alt, xkb::keysyms::KEY_r), "Some(ColumnWidthCycle)");
    }

    /// Ручки темпа и инерции разбираются, зажимаются и доезжают до значений
    /// по умолчанию. Опечатка здесь не ломает ни сборку, ни запуск — настройка
    /// просто молча не применится, ровно как с именами действий.
    #[test]
    fn ручки_анимации_разбираются() {
        let по_умолчанию = load_from_str(DEFAULT_CONFIG_LUA).unwrap();
        assert_eq!(по_умолчанию.anim_speed, 1.0);
        assert_eq!(по_умолчанию.pan_drift, 0.5);
        assert_eq!(по_умолчанию.fling_distance, 1.0);

        let cfg = load_from_str("set{ anim_speed = 1.6, pan_drift = 0.9, fling_distance = 2.5 }").unwrap();
        assert_eq!(cfg.anim_speed, 1.6);
        assert_eq!(cfg.pan_drift, 0.9);
        assert_eq!(cfg.fling_distance, 2.5);

        // Ноль — законный выбор (инерции окон нет), а не описка: зажимать его
        // к единице нельзя, иначе ручку невозможно выключить.
        let cfg = load_from_str("set{ fling_distance = 0 }").unwrap();
        assert_eq!(cfg.fling_distance, 0.0);
        let cfg = load_from_str("set{ fling_distance = 99 }").unwrap();
        assert_eq!(cfg.fling_distance, 8.0, "дальность вне [0,8] должна зажиматься");

        // Инерция зажата на разборе, темп — уже в anim::set_tempo (там же и
        // проверяется): сюда доходит как есть, лишь бы не потерялось.
        let cfg = load_from_str("set{ pan_drift = 5.0 }").unwrap();
        assert_eq!(cfg.pan_drift, 1.0, "инерция вне [0,1] должна зажиматься");

        // Ничего не задали — остаются значения по умолчанию, а не нули.
        let cfg = load_from_str("set{ cursor_size = 24 }").unwrap();
        assert_eq!(cfg.anim_speed, 1.0);
        assert_eq!(cfg.pan_drift, 0.5);
        assert_eq!(cfg.fling_distance, 1.0);
    }

    #[test]
    fn alt_tab_перебирает_стопку() {
        let cfg = load_from_str(DEFAULT_CONFIG_LUA).unwrap();
        let alt = ModMask { ctrl: false, alt: true, shift: false, logo: false };
        let alt_shift = ModMask { ctrl: false, alt: true, shift: true, logo: false };
        assert_eq!(действие(&cfg, alt, xkb::keysyms::KEY_Tab), "Some(CycleStack(1))");
        assert_eq!(действие(&cfg, alt_shift, xkb::keysyms::KEY_Tab), "Some(CycleStack(-1))");
    }

    /// Снимок экрана и история буфера (12.08.2026). Проверяем ровно то, что на
    /// глаз не видно: имя клавиши `Print` действительно разбирается в keysym, а
    /// пустые `mods = ""` не превращаются в «любой модификатор».
    #[test]
    fn prtscr_снимает_экран_а_super_c_открывает_буфер() {
        let cfg = load_from_str(DEFAULT_CONFIG_LUA).unwrap();
        let без = ModMask { ctrl: false, alt: false, shift: false, logo: false };
        let logo = ModMask { ctrl: false, alt: false, shift: false, logo: true };
        let logo_ctrl = ModMask { ctrl: true, alt: false, shift: false, logo: true };
        assert_eq!(
            действие(&cfg, без, xkb::keysyms::KEY_Print),
            r#"Some(Spawn("grim - | wl-copy"))"#,
        );
        assert!(
            действие(&cfg, logo, xkb::keysyms::KEY_c).contains("cliphist"),
            "Super+C не открывает историю буфера: {}",
            действие(&cfg, logo, xkb::keysyms::KEY_c),
        );
        // Колонка по центру не потерялась, а переехала.
        assert_eq!(действие(&cfg, logo_ctrl, xkb::keysyms::KEY_c), "Some(ColumnCenter)");
    }

    #[test]
    fn super_f_открывает_поиск() {
        let cfg = load_from_str(DEFAULT_CONFIG_LUA).unwrap();
        let logo = ModMask { ctrl: false, alt: false, shift: false, logo: true };
        let logo_shift = ModMask { ctrl: false, alt: false, shift: true, logo: true };
        assert_eq!(действие(&cfg, logo, xkb::keysyms::KEY_f), "Some(WindowSearch)");
        // Максимизация колонки не потерялась, а переехала.
        assert_eq!(действие(&cfg, logo_shift, xkb::keysyms::KEY_f), "Some(ColumnMaximize)");
    }
}
