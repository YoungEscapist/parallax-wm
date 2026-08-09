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
    Kill,
    SetLayout(Layout),
    ToggleLayoutFloatTile,
    Zoom,
    ToggleFloatingFocused,
    /// F11: окно на весь экран (без скруглений и теней) и обратно.
    ToggleFullscreen,
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
    TogglePortal,
    ToggleBookmarksMode,
    ToggleSnapping,
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
    /// Alt+Tab: перебор окон, лежащих друг под другом (см. switcher.rs).
    /// Аргумент — направление: +1 вглубь стопки, −1 назад.
    CycleStack(i32),
    /// Super+F: поиск окна по имени с перелётом к нему (см. switcher.rs).
    WindowSearch,
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
    pub scale: f64,
    pub transform: String,
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
        "toggle_floating" => ToggleFloatingFocused,
        "toggle_fullscreen" => ToggleFullscreen,
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
        "toggle_portal" => TogglePortal,
        "toggle_bookmarks_mode" => ToggleBookmarksMode,
        "toggle_snapping" => ToggleSnapping,
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

    match load_from_str(&source) {
        Ok(cfg) => {
            tracing::info!("dawn/config: loaded {} keybinding(s)", cfg.bindings.len());
            cfg
        }
        Err(e) => {
            tracing::error!("dawn/config: error evaluating config.lua: {} — falling back to built-in default", e);
            load_from_str(DEFAULT_CONFIG_LUA).unwrap_or_default()
        }
    }
}

/// Evaluates a Lua config source string, exposing `bind{}`, `xkb{}` and
/// `set{}` as globals that accumulate into the returned [`Config`].
pub fn load_from_str(source: &str) -> mlua::Result<Config> {
    let lua = Lua::new();
    let bindings: Rc<RefCell<Vec<KeyBinding>>> = Rc::new(RefCell::new(Vec::new()));
    let xkb_settings: Rc<RefCell<XkbSettings>> = Rc::new(RefCell::new(XkbSettings::default()));
    let bird_eye_key: Rc<RefCell<u32>> = Rc::new(RefCell::new(xkb::keysyms::KEY_space));
    let dwindle_cfg: Rc<RefCell<crate::dwindle::DwindleConfig>> =
        Rc::new(RefCell::new(crate::dwindle::DwindleConfig::default()));
    let bt_autoconnect: Rc<RefCell<bool>> = Rc::new(RefCell::new(true));
    let monitors: Rc<RefCell<Vec<MonitorConfig>>> = Rc::new(RefCell::new(Vec::new()));

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
        let set_fn = lua.create_function(move |_, tbl: Table| {
            if let Ok(v) = tbl.get::<bool>("bluetooth_autoconnect") {
                *bt_autoconnect.borrow_mut() = v;
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
            if let Ok(v) = tbl.get::<bool>("preserve_split") {
                d.preserve_split = v;
            }
            if let Ok(v) = tbl.get::<i32>("force_split") {
                d.force_split = v.clamp(0, 2) as u8;
            }
            if let Ok(v) = tbl.get::<f32>("default_split_ratio") {
                d.default_split_ratio = v.clamp(0.1, 1.9);
            }
            Ok(())
        })?;
        lua.globals().set("dwindle", dwindle_fn)?;
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
            monitors.borrow_mut().push(MonitorConfig {
                name,
                width: tbl.get::<i32>("width").unwrap_or(0),
                height: tbl.get::<i32>("height").unwrap_or(0),
                refresh: tbl.get::<i32>("refresh").unwrap_or(0),
                x: tbl.get::<i32>("x").unwrap_or(0),
                y: tbl.get::<i32>("y").unwrap_or(0),
                scale: tbl.get::<f64>("scale").unwrap_or(1.0).clamp(0.25, 8.0),
                transform: tbl.get::<String>("transform")
                    .unwrap_or_else(|_| "normal".into()),
            });
            Ok(())
        })?;
        lua.globals().set("monitor", monitor_fn)?;
    }

    lua.load(source).exec()?;

    let result = Config {
        bindings: bindings.borrow().clone(),
        xkb: xkb_settings.borrow().clone(),
        bird_eye_key: *bird_eye_key.borrow(),
        dwindle: *dwindle_cfg.borrow(),
        bluetooth_autoconnect: *bt_autoconnect.borrow(),
        monitors: monitors.borrow().clone(),
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
                } else if self.selection_is_packed() && self.selection_is_constellation() {
                    // Решает РАССТОЯНИЕ, а не факт группировки: окна уже лежат
                    // вплотную — значит нажатие означает «разобрать». Раньше
                    // условием было одно лишь «это созвездие», и растащенное
                    // руками созвездие нельзя было собрать обратно тем же
                    // нажатием — оно сразу разбиралось.
                    self.scatter_selected_constellation();
                } else {
                    self.gather_selected_into_constellation();
                }
            }
            Zoom => self.zoom(),
            ToggleFloatingFocused => self.toggle_floating(),
            ToggleFullscreen => self.toggle_fullscreen(),
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
            TogglePortal => self.toggle_portal(),
            ToggleBookmarksMode => {
                self.bookmarks_mode = !self.bookmarks_mode;
                tracing::info!("dawn: bookmarks_mode={}", self.bookmarks_mode);
            }
            ToggleSnapping => {
                self.is_snapping_enabled = !self.is_snapping_enabled;
                tracing::info!("dawn: is_snapping_enabled={}", self.is_snapping_enabled);
            }
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

    fn spawn(&self, cmd: &str) {
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
        self.lua_config = new_cfg;
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

    #[test]
    fn alt_tab_перебирает_стопку() {
        let cfg = load_from_str(DEFAULT_CONFIG_LUA).unwrap();
        let alt = ModMask { ctrl: false, alt: true, shift: false, logo: false };
        let alt_shift = ModMask { ctrl: false, alt: true, shift: true, logo: false };
        assert_eq!(действие(&cfg, alt, xkb::keysyms::KEY_Tab), "Some(CycleStack(1))");
        assert_eq!(действие(&cfg, alt_shift, xkb::keysyms::KEY_Tab), "Some(CycleStack(-1))");
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
