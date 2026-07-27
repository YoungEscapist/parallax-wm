-- dawn compositor config
--
-- This file is Lua. It's evaluated once at startup (and again on
-- Super+Shift+C, "reload_config"). Two functions are available:
--
--   xkb{ layout = "us,ru", variant = "", model = "", options = "" }
--     Keyboard layout(s) for xkbcommon. `layout` is a comma-separated list of
--     layouts to compile into the keymap (the first is active at startup);
--     switch between them at runtime with the layout_next/layout_prev
--     actions below (bound to Ctrl+Space / Ctrl+Shift+Space by default).
--
--   bind{ mods = "super+shift", key = "q", action = "quit", ... }
--     Registers a keybinding. `mods` is a "+"-joined combination of ctrl,
--     alt, shift, super (aliases: control, mod1, logo, mod4, win). `key` is
--     an X11/xkb keysym name (letters/digits, or names like Return, Tab,
--     Left, F1, comma, grave, space). Extra fields depend on the action
--     (see below). A bind requires an EXACT modifier match — e.g. a
--     mods="super" bind will NOT fire while shift is also held.
--
-- Available actions and their extra fields:
--   spawn            cmd = "ghostty"
--   quit             (saves session, stops the compositor)
--   kill             (closes the focused window)
--   set_layout       layout = "tile" | "float" | "monocle"
--   toggle_layout    (toggles between Float and Tile)
--   zoom             (swap focused window with master)
--   toggle_floating  (toggle floating for the focused window)
--   focus_direction  dx = -1|0|1, dy = -1|0|1 (spatial navigation)
--   focus_stack      dir = 1 | -1 (focus next/prev in stack order)
--   inc_nmaster      n = 1 | -1
--   set_mfact        delta = 0.05 | -0.05
--   move_focused     dx = <px>, dy = <px> (floating window move)
--   resize_focused    dw = <px>, dh = <px> (floating window resize)
--   view_tag         tag = 1-9
--   tag_window       tag = 1-9 (assign focused window to tag)
--   toggle_view      tag = 1-9 (toggle tag in current view)
--   toggle_tag       tag = 1-9 (toggle tag on focused window)
--   toggle_minimap
--   toggle_portal
--   toggle_bookmarks_mode  (Super+1-9 jump/save camera bookmarks instead of tags)
--   toggle_snapping
--   toggle_fold_stack
--   vt_switch        vt = 1-12
--   layout_next / layout_prev   (cycle keyboard layout)
--   reload_config
--   group_selected     (group current rubber-band selection into a "constellation" —
--                        moves/resizes as one rigid unit; Float only)
--   ungroup_selected    (disband the constellation the focused window belongs to)
--   pin_bookmark_at_cursor  (Alt+B: pin a camera bookmark at the cursor into the
--                        lowest free slot 1-9; jump to it in bookmarks mode with Super+N)
--   column_width_cycle  (Columns/niri mode: cycle active column width 1/3→1/2→2/3→full)
--
-- niri-подобный режим Columns (Super+N): колонки — вертикальные стопки окон,
-- холст скроллится по горизонтали к активной колонке. В этом режиме те же
-- бинды работают по-niri:
--   Super+←/→                focus колонка влево/вправо
--   Super+↑/↓                focus окно внутри колонки (стопки)
--   Super+Ctrl+←/→           переставить колонку
--   Super+Ctrl+↑/↓           переставить окно в колонке
--   Super+comma / period     consume/expel окно в стопку колонки
--   Super+r                  сменить ширину активной колонки
--   Alt+колесо               листать колонки
-- Новое окно открывается отдельной колонкой справа от активной, камера едет
-- к нему.

xkb{ layout = "us,ru", variant = "", options = "grp:alt_shift_toggle" }

-- ── VT switching ─────────────────────────────────────────────────────────
for i = 1, 12 do
  bind{ mods = "ctrl+alt", key = "F" .. i, action = "vt_switch", vt = i }
end

-- ── Session / windows ────────────────────────────────────────────────────
-- Mouse: in Float mode, LMB-drag on empty canvas rubber-band-selects windows
-- under the rectangle (plain click with no drag just clears the selection).
-- "kill" closes all selected windows at once if there's a selection, else
-- just the focused window.
bind{ mods = "super+shift", key = "q", action = "quit" }
bind{ mods = "super", key = "q", action = "kill" }
bind{ mods = "super", key = "g", action = "group_selected" }
bind{ mods = "super+shift", key = "g", action = "ungroup_selected" }
bind{ mods = "super", key = "Return", action = "spawn", cmd = "ghostty" }
bind{ mods = "super+shift", key = "Return", action = "zoom" }
bind{ mods = "super", key = "w", action = "toggle_floating" }

-- ── Layouts ───────────────────────────────────────────────────────────────
bind{ mods = "super", key = "d", action = "toggle_layout" }
bind{ mods = "super", key = "t", action = "set_layout", layout = "tile" }
bind{ mods = "super", key = "m", action = "set_layout", layout = "monocle" }
-- niri-подобные колонки (вертикальные стопки, скролл камерой к активной колонке)
bind{ mods = "super", key = "n", action = "toggle_niri_mode" }
-- Ширина активной колонки в Columns (1/3 → 1/2 → 2/3 → full)
bind{ mods = "super", key = "r", action = "column_width_cycle" }
-- niri-воркспейсы: Super+PageUp/Down переключают воркспейс (в Columns остаёмся
-- в Columns); Super+Ctrl+PageUp/Down переносят активную колонку на соседний.
bind{ mods = "super", key = "Next",  action = "workspace_step", dir = 1 }
bind{ mods = "super", key = "Prior", action = "workspace_step", dir = -1 }
bind{ mods = "super+ctrl", key = "Next",  action = "move_column_to_workspace", dir = 1 }
bind{ mods = "super+ctrl", key = "Prior", action = "move_column_to_workspace", dir = -1 }
bind{ mods = "super", key = "comma", action = "inc_nmaster", n = 1 }
bind{ mods = "super", key = "period", action = "inc_nmaster", n = -1 }
bind{ mods = "super+shift", key = "h", action = "set_mfact", delta = -0.05 }
bind{ mods = "super+shift", key = "l", action = "set_mfact", delta = 0.05 }

-- ── Focus / navigation ───────────────────────────────────────────────────
bind{ mods = "super", key = "Left",  action = "focus_direction", dx = -1, dy = 0 }
bind{ mods = "super", key = "Right", action = "focus_direction", dx = 1, dy = 0 }
bind{ mods = "super", key = "Up",    action = "focus_direction", dx = 0, dy = -1 }
bind{ mods = "super", key = "Down",  action = "focus_direction", dx = 0, dy = 1 }
bind{ mods = "super", key = "j",   action = "focus_stack", dir = 1 }
bind{ mods = "super", key = "Tab", action = "focus_stack", dir = 1 }
bind{ mods = "super", key = "k", action = "focus_stack", dir = -1 }
bind{ mods = "super+shift", key = "Tab", action = "focus_stack", dir = -1 }

-- ── Move / resize floating windows ───────────────────────────────────────
bind{ mods = "super+ctrl", key = "h",     action = "move_focused", dx = -20, dy = 0 }
bind{ mods = "super+ctrl", key = "Left",  action = "move_focused", dx = -20, dy = 0 }
bind{ mods = "super+ctrl", key = "l",     action = "move_focused", dx = 20, dy = 0 }
bind{ mods = "super+ctrl", key = "Right", action = "move_focused", dx = 20, dy = 0 }
bind{ mods = "super+ctrl", key = "k",  action = "move_focused", dx = 0, dy = -20 }
bind{ mods = "super+ctrl", key = "Up",  action = "move_focused", dx = 0, dy = -20 }
bind{ mods = "super+ctrl", key = "j",   action = "move_focused", dx = 0, dy = 20 }
bind{ mods = "super+ctrl", key = "Down", action = "move_focused", dx = 0, dy = 20 }

-- Note: Super+Shift+H/L are taken by set_mfact above, so resize only binds
-- j/k/arrows here (matches the historical behavior of dawn's hardcoded
-- keymap, where the mfact bind shadowed the resize bind for h/l).
bind{ mods = "super+shift", key = "k",     action = "resize_focused", dw = 0, dh = -30 }
bind{ mods = "super+shift", key = "Up",    action = "resize_focused", dw = 0, dh = -30 }
bind{ mods = "super+shift", key = "j",     action = "resize_focused", dw = 0, dh = 30 }
bind{ mods = "super+shift", key = "Down",  action = "resize_focused", dw = 0, dh = 30 }
bind{ mods = "super+shift", key = "Left",  action = "resize_focused", dw = -30, dh = 0 }
bind{ mods = "super+shift", key = "Right", action = "resize_focused", dw = 30, dh = 0 }

-- ── Tags / workspaces (Super+1-9 view, Super+Shift+1-9 assign) ───────────
-- When bookmarks_mode is on (Super+B), these instead jump to / save camera
-- bookmarks in the same numbered slots.
for i = 1, 9 do
  bind{ mods = "super", key = tostring(i), action = "view_tag", tag = i }
  bind{ mods = "super+shift", key = tostring(i), action = "tag_window", tag = i }
end

-- ── Toggles ───────────────────────────────────────────────────────────────
bind{ mods = "super", key = "grave", action = "toggle_minimap" }
bind{ mods = "super", key = "p", action = "toggle_portal" }
bind{ mods = "super", key = "b", action = "toggle_bookmarks_mode" }
bind{ mods = "alt", key = "b", action = "pin_bookmark_at_cursor" }
bind{ mods = "super", key = "s", action = "toggle_snapping" }
bind{ mods = "super+shift", key = "s", action = "toggle_fold_stack" }

-- ── Keyboard layout switching ────────────────────────────────────────────
bind{ mods = "ctrl", key = "space", action = "layout_next" }
bind{ mods = "ctrl+shift", key = "space", action = "layout_prev" }

-- ── Config reload ─────────────────────────────────────────────────────────
bind{ mods = "super+shift", key = "c", action = "reload_config" }
