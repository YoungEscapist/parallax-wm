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
--   dwindle{ preserve_split = true, force_split = 2 }
--     Tiling (Super+T) is Hyprland's dwindle: a new window splits the FOCUSED
--     window's slot in half, so the layout is a BSP tree rather than a fixed
--     chain — closing a window gives its space to its sibling, not to everyone.
--     Same knobs as the dwindle:* section of hyprland.conf, same defaults:
--       split_width_multiplier (1.0) — how much wider than tall a slot must be
--                                      to split left/right instead of top/bottom
--       preserve_split (false)       — false: the split axis is recomputed from
--                                      the slot's proportions on every relayout
--                                      (windows "flip" as neighbours close);
--                                      true: a split stays where it was made
--       force_split (0)              — which half the new window takes:
--                                      0 = the half the cursor is over,
--                                      1 = always left/top, 2 = always right/bottom
--       default_split_ratio (1.0)    — 1.0 = new splits are 50/50
--
-- Available actions and their extra fields:
--   spawn            cmd = "ghostty"
--   quit             (saves session, stops the compositor)
--   kill             (closes the focused window)
--   set_layout       layout = "tile" | "float" | "monocle"
--   toggle_layout    (toggles between Float and Tile; ignored in the Super-tap
--                     desktop overview, which manages the camera itself)
--   zoom             (Tile: raise the focused window to the top of the split
--                     tree — Hyprland's "layoutmsg movetoroot"; other layouts:
--                     swap it with the first window in the stack)
--   toggle_floating  (toggle floating for the focused window)
--   focus_direction  dx = -1|0|1, dy = -1|0|1 (spatial navigation)
--   focus_stack      dir = 1 | -1 (focus next/prev in stack order)
--   inc_nmaster      n = 1 | -1 (Tile: n>0 flips the nearest split's axis
--                     [togglesplit], n<0 swaps its two halves [swapsplit])
--   set_mfact        delta = 0.05 | -0.05 (Tile: moves the nearest split,
--                     growing the focused window — "layoutmsg splitratio")
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
-- ── Колонки (Columns) — раскладка и биндинги как в niri ──────────────────
-- Ширина/высота: пресеты niri (⅓ → ½ → ⅔), проценты и сброс.
bind{ mods = "super", key = "r", action = "column_width_cycle" }
bind{ mods = "super+shift", key = "r", action = "window_height_cycle" }
bind{ mods = "super+ctrl", key = "r", action = "window_height_reset" }
bind{ mods = "super", key = "minus", action = "column_width_adjust", percent = -10 }
bind{ mods = "super", key = "equal", action = "column_width_adjust", percent = 10 }
bind{ mods = "super+shift", key = "minus", action = "window_height_adjust", percent = -10 }
bind{ mods = "super+shift", key = "equal", action = "window_height_adjust", percent = 10 }
-- Колонка на всю ширину и обратно; колонка по центру экрана.
bind{ mods = "super", key = "f", action = "column_maximize" }
bind{ mods = "super", key = "c", action = "column_center" }
-- Первая/последняя колонка и перенос колонки в начало/конец полосы.
bind{ mods = "super", key = "Home", action = "column_focus_first" }
bind{ mods = "super", key = "End",  action = "column_focus_last" }
bind{ mods = "super+ctrl", key = "Home", action = "column_move_to_first" }
bind{ mods = "super+ctrl", key = "End",  action = "column_move_to_last" }
-- Забрать окно в колонку / вытолкнуть из неё одной клавишей (niri:
-- consume-or-expel-window-left/right).
bind{ mods = "super", key = "bracketleft",  action = "consume_or_expel_left" }
bind{ mods = "super", key = "bracketright", action = "consume_or_expel_right" }
-- Колонка вкладками: видно только активное окно, слева полоска вкладок
-- (niri: toggle-column-tabbed-display).
bind{ mods = "super", key = "v", action = "column_toggle_tabbed" }
-- Фокус между плавающим слоем и полосой колонок
-- (niri: switch-focus-between-floating-and-tiling). У niri это Mod+Space, но
-- в dawn Super+Space занят режимом лупы (bird_eye_key, перехватывается раньше
-- биндингов), поэтому здесь Super+Shift+Space.
bind{ mods = "super+shift", key = "space", action = "focus_floating_or_tiling" }
-- Как вести вид за активной колонкой: never (по умолчанию, как в niri),
-- always, on-overflow. Меняется на лету.
bind{ mods = "super+alt", key = "c", action = "center_focused_column", mode = "always" }
bind{ mods = "super+alt+shift", key = "c", action = "center_focused_column", mode = "never" }
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

-- ── Обзор столов (тап Super) ─────────────────────────────────────────────
-- Столы лежат 2D-сеткой вокруг текущего: новые встают по очереди справа,
-- снизу, слева, сверху от уже занятых ячеек (и только потом по диагоналям).
--
-- Перетащить ВЕСЬ стол по сетке во все четыре стороны можно:
--   · мышью — Win+Alt+ЛКМ (стол едет за курсором, встаёт в ближайшую ячейку);
--   · с клавиатуры — Win+Alt+стрелки (биндинги ниже).
-- Ячейка занята другим столом → столы меняются местами. Расстановка
-- сохраняется между заходами в обзор.
bind{ mods = "super+alt", key = "Left",  action = "move_workspace_slot", dx = -1, dy = 0 }
bind{ mods = "super+alt", key = "Right", action = "move_workspace_slot", dx = 1,  dy = 0 }
bind{ mods = "super+alt", key = "Up",    action = "move_workspace_slot", dx = 0,  dy = -1 }
bind{ mods = "super+alt", key = "Down",  action = "move_workspace_slot", dx = 0,  dy = 1 }

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
