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
    /// Columns (niri): сменить пресет ширины активной колонки (Super+R).
    ColumnWidthCycle,
    /// niri-воркспейсы: перейти на пред/след воркспейс (dir=-1/+1).
    WorkspaceStep(i32),
    /// niri-воркспейсы: перенести активную колонку на соседний воркспейс (Columns).
    MoveColumnToWorkspace(i32),
    /// Тумблер niri-режим (Columns) ↔ Tile (Win+N): в niri — выход в обычный
    /// tile, не в niri — вход в niri.
    ToggleNiriMode,
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

// ── Config ───────────────────────────────────────────────────────────────────

pub struct Config {
    pub bindings: Vec<KeyBinding>,
    pub xkb: XkbSettings,
    /// Trigger key for the Super-held bird's-eye zoom gesture (default: space).
    /// This is a hold-gesture, not a plain keybind, so it's handled separately.
    pub bird_eye_key: u32,
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
        "toggle_floating" => ToggleFloatingFocused,
        "focus_direction" => FocusDirection(get_i32("dx", 0), get_i32("dy", 0)),
        "focus_stack" => FocusStack(get_i32("dir", 1)),
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
        "column_width_cycle" => ColumnWidthCycle,
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
        let set_fn = lua.create_function(move |_, tbl: Table| {
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

    lua.load(source).exec()?;

    let result = Config {
        bindings: bindings.borrow().clone(),
        xkb: xkb_settings.borrow().clone(),
        bird_eye_key: *bird_eye_key.borrow(),
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
                } else if self.selection_is_constellation() {
                    self.scatter_selected_constellation();
                } else {
                    self.gather_selected_into_constellation();
                }
            }
            Zoom => self.zoom(),
            ToggleFloatingFocused => self.toggle_floating(),
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
                // Super+1-9 в обзоре столов → выйти из обзора и перейти на воркспейс.
                if self.overview_active {
                    self.exit_overview_immediate(Some(mask));
                }
                if self.bookmarks_mode {
                    self.jump_to_camera_bookmark(mask.trailing_zeros() + 1);
                } else {
                    self.view_tag(mask);
                }
            }
            TagWindow(mask) => {
                if self.bookmarks_mode {
                    self.save_camera_bookmark(mask.trailing_zeros() + 1);
                } else {
                    self.tag_window(mask);
                }
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
            ColumnWidthCycle => self.columns_cycle_width(),
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
            Ok(_) => {}
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

