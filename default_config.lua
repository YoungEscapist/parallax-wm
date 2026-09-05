-- parallax compositor config
--
-- The same file in Russian is default_config.ru.lua: the same settings on the
-- same lines, comments translated. This one is what the compositor embeds and
-- what it writes to ~/.config/parallax/config.lua on a first run; if you would
-- rather read it in Russian, copy the other one over your config.lua instead.
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
--   set{ cursor_size = 16, cursor_client_max = -1 }
--     Cursor size. cursor_size is the size of the compositor's own arrow and
--     of every shape an application asks for through wp_cursor_shape_v1
--     (0 = take it from XCURSOR_SIZE). cursor_client_max is the ceiling for
--     applications that draw the cursor as their own image (XWayland, GTK3):
--     -1 = the ceiling equals cursor_size, 0 = no ceiling and the image is
--     shown as sent, N = a ceiling of N px.
--
--   set{ close_anim = true }
--     A quiet fade for a closed window: a snapshot of its last frame dims and
--     shrinks slightly towards its centre over 360 ms. On by default. The knob
--     exists because the snapshot is taken with an offscreen render pass — the
--     same trick as the blur below — and it has never been watched live. If it
--     starts spoiling the frame, turn it off here and press Super+Shift+C.
--
--   set{ blur = false }
--     Blur the background behind the bar's islands (frosted glass, like
--     macOS). OFF BY DEFAULT: the code is written but has never been watched
--     live, and a mistake in a render pass costs a black screen. Turn it on
--     with a tty within reach to back out to. What is blurred is the
--     background (the wallpaper), not the whole scene under the panel: a
--     full-screen offscreen pass every frame is a different price.
--
--   set{ infinite_wallpaper = true }
--     The wallpaper lives ON THE CANVAS and rides with the camera instead of
--     being glued to the screen. There is ONE copy of it: slightly larger than
--     the screen and shifted with a falloff, so no camera position ever shows
--     a seam or a repeat. false is the old behaviour (the image always exactly
--     the size of the output, not moving at all). If the wallpaper texture
--     cannot be obtained, parallax quietly falls back to the old behaviour —
--     without a wallpaper the screen would be black.
--
--   set{ pan_drift = 0.5 }
--     How long the canvas keeps drifting after a fling with fingers or mouse:
--     0 stops it dead, 1 lets it travel longest. The scale is logarithmic in
--     coasting time, so 0.6 is noticeably floatier than 0.5, not marginally.
--     A gentle scroll settles faster than a sharp fling — that is intended.
--
--   set{ fling_distance = 1.0 }
--     How far a THROWN window flies (Win+LMB, release while moving): 1.0 is as
--     designed (around 2000 canvas px on a sharp throw), 0 leaves the window
--     exactly where it was released, 2.0 sends it twice as far. The distance
--     grows with the speed of the throw, so a window can be nudged a hand's
--     width or hurled into a far corner. A flying window shoves the ones it
--     runs into (collision mode, Super+S).
--
--   set{ anim_speed = 1.0 }
--     The tempo of EVERY animation: 1.0 as designed, 1.5 half again slower and
--     calmer, 0.7 sharper. One knob for camera flights, zoom, the tiling
--     settling, the scatter into float and the opening of windows alike; the
--     durations themselves live in anim.rs (`anim::дуг`). Picked up by a
--     config reload (Super+Shift+C), no rebuild needed.
--
--   set{ bird_eye_key = "space" }
--     The key for zoom-nav: Super+<this key> toggles the pulled-back view in
--     which the bare arrow keys pan the canvas. It is intercepted BEFORE the
--     binds, so the combination Super+<bird_eye_key> is not available to
--     bind{}.
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
--   restart          (saves session and restarts the compositor in place:
--                     parallax exits with code 42 and launch_native.sh brings it
--                     back up, rebuilding first if the sources are newer than
--                     the binary. Picks up a fresh build without logging out;
--                     clients do NOT survive — the wayland socket dies with
--                     the compositor)
--   kill             (closes the focused window)
--   set_layout       layout = "tile" | "float" | "monocle" | "columns"
--   toggle_layout    (toggles between Float and Tile; ignored in the Super-tap
--                     desktop overview, which manages the camera itself)
--   zoom             (Tile: raise the focused window to the top of the split
--                     tree — Hyprland's "layoutmsg movetoroot"; other layouts:
--                     swap it with the first window in the stack)
--   toggle_floating  (toggle floating for the focused window)
--   toggle_fullscreen (F11: focused window fills the whole screen — no corner
--                     rounding, no shadow, zoom reset to 1:1; toggles back and
--                     restores the previous size, camera and zoom)
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
--   toggle_snapping    (collision: windows shove each other while being moved)
--   toggle_magnetism   (an edge snaps to a neighbour once, on release)
--   toggle_fold_stack
--   share_start / share_stop / share_toggle   port = 7373
--                      (multi-user: show the desktop to guests, up to five)
--   vt_switch        vt = 1-12
--   layout_next / layout_prev   (cycle keyboard layout)
--   reload_config
--   group_selected     (bind the selection into a constellation: the "master
--                        and stack" layout from halley — the master takes 60%
--                        of the width at full height, the rest share the right
--                        column; Float only. Normally Win+D does this to the
--                        selection, so there is no bind of its own)
--   ungroup_selected    (dissolve the constellation, leaving the windows put)
--   pin_bookmark_at_cursor  (Alt+B: pin a camera bookmark at the cursor into the
--                        lowest free slot 1-9; jump to it in bookmarks mode with Super+N)
--   column_width_cycle  (Columns/niri mode: cycle active column width 1/3→1/2→2/3→full)
--   toggle_niri_mode    (toggles Columns ↔ Tile)
--
-- Column actions (Columns/niri), all named after the niri commands:
--   window_height_cycle     (switch-preset-window-height)
--   window_height_reset     (reset-window-height)
--   column_width_adjust     percent = ±N   (set-column-width "±N%")
--   window_height_adjust    percent = ±N   (set-window-height "±N%")
--   column_maximize         (maximize-column)
--   column_center           (center-column)
--   column_focus_first / column_focus_last     (focus-column-first/last)
--   column_move_to_first / column_move_to_last (move-column-to-first/last)
--   consume_or_expel_left / consume_or_expel_right
--                           (consume-or-expel-window-left/right)
--   column_toggle_tabbed    (toggle-column-tabbed-display)
--   focus_floating_or_tiling (switch-focus-between-floating-and-tiling)
--   center_focused_column   mode = "never" | "always" | "on-overflow"
--   workspace_step          dir = 1 | -1   (previous/next workspace)
--   move_column_to_workspace dir = 1 | -1  (move the column to the neighbour)
--
-- The niri-like Columns mode (Super+N): columns are vertical stacks of windows
-- and the canvas scrolls horizontally to the active one. In this mode the same
-- binds work the niri way:
--   Super+←/→                focus the column to the left/right
--   Super+↑/↓                focus a window inside the column (the stack)
--   Super+Ctrl+←/→           move the column
--   Super+Ctrl+↑/↓           move the window inside the column
--   Super+comma / period     consume/expel a window into the column's stack
--   Super+r                  cycle the active column's width
--   Alt+wheel                scroll through the columns
-- A new window opens as its own column to the right of the active one, and the
-- camera travels to it.

xkb{ layout = "us,ru", variant = "", options = "grp:alt_shift_toggle" }

-- Interface language / язык интерфейса: "en" (default) or "ru".
--
-- Covers what you read on screen — the bar, the wifi/bluetooth/audio menus,
-- the screenshot and overview hints, notifications — and the replies of the
-- terminal commands (plx-host, the control socket). Re-read on Super+Shift+C,
-- so you can switch without restarting.
--
-- Logs are ALWAYS English and this knob does not touch them: a log ends up in
-- someone else's bug report, and it has to be readable by more than its author.
-- So is what plx-host says on its own behalf (its --help, and its complaint
-- about finding no control socket — printed exactly when there is nobody left
-- to ask which language you chose); what it relays from the compositor follows
-- the knob.
set{ lang = "en" }

-- The zoom-nav key (Super+Space by default).
set{ bird_eye_key = "space" }

-- One cursor size everywhere, over windows and off them alike. Applications
-- that speak wp_cursor_shape_v1 (GTK4, Qt6, Chromium) ask us for a SHAPE and
-- get it in our theme at our size; the rest (XWayland, GTK3) have their own
-- image squeezed down to cursor_client_max. If you need a large cursor drawn
-- by the application itself (a crosshair in a game), set cursor_client_max = 0.
set{ cursor_size = 0, cursor_client_max = -1 }

-- The tempo of the animations. Set it to taste; re-read on Super+Shift+C:
-- above one is longer and calmer, below one is sharper.
set{ anim_speed = 1.0, pan_drift = 0.5, fling_distance = 1.0 }

-- An infinite wallpaper: the image lies on the canvas and rides with the
-- camera (one copy, with a falloff — no seams and no repeats).
set{ infinite_wallpaper = true }

-- The quiet fade of a closed window (described in the header above).
set{ close_anim = true }

-- Blur behind the bar. Off: never watched live, see the description above.
set{ blur = false }

-- Windows rim-lit IN THE WALLPAPER'S COLOUR: a glowing rim runs along the
-- inside of the window edge in a colour taken from the palette of the current
-- wallpaper (plx-wall computes it and writes ~/.cache/plx-wall/palette.json —
-- the same colour that goes to the terminal and to the shell). Change the
-- wallpaper and the glow changes with it, without a restart.
--
--   glow       — strength, 0.0…1.0; 0 turns it off. It fades out on BRIGHT
--                wallpapers by itself: there a coloured rim reads as grime
--                rather than as light.
--   glow_width — the width of the rim in logical pixels (it follows the zoom).
--
-- The same knob lights the window's OUTLINE — a halo outside, reaching twice
-- as far out as the rim reaches in. The outline is not a separate thing: the
-- shadow around the window and the rim inside it are computed by ONE shader
-- from one frame (src/rounded.rs), so they cannot drift apart, and the shadow
-- falls AWAY FROM the light source (sun below) instead of always downwards.
--
-- The window being worked in glows at full strength, the rest at half: that is
-- how you see where the input goes even with a dozen windows on the canvas.
--
-- plx-extra ONLY (the `shaders` feature). The standard build has no such
-- shader at all; the knob is read as usual and does nothing, and a line about
-- that goes to the log when the config is read, so that "I set it and nothing
-- happens" does not look like a fault.
set{ glow = 0.0, glow_width = 12.0 }

-- A LIGHT ON THE CANVAS in the wallpaper's colour. Not a drawn sun: there is
-- no disc on the screen. There is a source standing at a point of the canvas,
-- and from it the scene is lit (a soft pool over the wallpaper) and so are the
-- WINDOWS — the side facing the light is brighter, the far side falls into
-- shade, and the rim (glow above) burns stronger where the light lands. Hence
-- both a sense of "where the light is" on an endless canvas and a sense of
-- volume in the windows.
--
--   sun       — strength, 0.0…1.0; 0 turns it off. Fades on bright wallpapers.
--   sun_size  — how far the light reaches, in screen widths (the falloff).
--   sun_x/y   — where the source stands, in screen fractions from the
--               monitor's home. Outside 0…1 is fine: the canvas is endless and
--               the source may well hang past the edge of the screen.
--   sun_far   — how FAR away it is: 0 is glued to the screen, 1 lies on the
--               canvas alongside the windows. A quarter by default: a source
--               honestly standing on the canvas leaves the frame after the
--               first long flight, and a real sun does not stay behind you.
--
-- plx-extra ONLY (the `shaders` feature), like glow above.
set{ sun = 0.0, sun_size = 1.6, sun_x = 0.78, sun_y = 0.18, sun_far = 0.25 }

-- THE DESKTOP CUBE, the one from Compiz: the workspaces stand on the faces of
-- a prism, and the overview (tap Super) stops being a flat grid. The wheel
-- pushes the cube away and pulls it closer, dragging turns it by hand, and a
-- click on a face takes you to its workspace.
--
-- The cube is ENDLESS: it has cube_faces faces, but the ring may hold any
-- number of workspaces — the slots take them in turn and are reassigned on the
-- back face, which nobody sees. You can spin it forever, and on the tenth
-- workspace the cube is still a cube rather than a twenty-sided wall.
--
--   cube        — strength, 0.0…1.0; 0 turns it off. cube_shade is scaled by
--                 it as well.
--   cube_faces  — how many faces, 3…12.
--   cube_fill   — what fraction of the screen width the front face takes.
--   cube_focal  — the focal length in screen widths: less is a sharper
--                 perspective.
--   cube_shade  — how much the far edge of a face darkens; with no shading the
--                 cube reads as a flat mosaic.
--   cube_switch — turn the cube when stepping to the neighbouring workspace
--                 (Super+PgUp/PgDn) too, not only in the overview. Exactly what
--                 Compiz did.
--
-- plx-extra ONLY (the `shaders` feature). Without it the overview stays a flat
-- grid of workspaces and Super+PgUp/PgDn is a plain workspace switch.
set{ cube = 0.0, cube_faces = 4, cube_fill = 0.62, cube_focal = 2.2 }
set{ cube_shade = 0.35, cube_switch = true }

-- The entrance of the desktop at startup: the canvas swims out of the dark to
-- its own zoom while the bar arrives from above. A second and a bit, exactly
-- once per session. The duration follows the common tempo (anim_speed above).
set{ intro = true }

-- The notification sound: a short tone on every popup — parallax hears them
-- itself, on the session bus, so the notification daemon can be any (mako,
-- dunst, your own).
--   notify_sound  — path to a file; empty = the built-in tone (a soft glassy
--                   one, CC0, see assets/sounds/README.md), "off" = silence.
--                   Two more sit next to the built-in one: notify-cloud.ogg is
--                   warmer, notify-polite.ogg is three quiet wooden clicks.
--   notify_volume — loudness, 0.0…1.0.
-- An application that asked for silence with the suppress-sound hint (music
-- players on a track change) will not sound.
set{ notify_sound = "", notify_volume = 0.35 }

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
-- Constellations are driven ONLY by the selection and Win+D (see
-- constellation.rs): rubber-band some windows, Win+D binds them into a
-- "master and stack" cluster, Win+D on it again dissolves it. There are no
-- separate super+g / super+shift+g any more: two knobs for one action only
-- confused which of them did what. The group_selected / ungroup_selected
-- actions themselves are still available from the config — if you want them on
-- a key of their own, bring the bind back on the line below.
bind{ mods = "super", key = "Return", action = "spawn", cmd = "ghostty" }
bind{ mods = "super+shift", key = "Return", action = "zoom" }

-- Fullscreen: the window fills the whole monitor with no rounding and no
-- shadow, at zoom 1:1 (needed for video, games and screen sharing). Pressing
-- it again restores the window, the camera and the zoom.
bind{ mods = "", key = "F11", action = "toggle_fullscreen" }

-- ── Layouts ───────────────────────────────────────────────────────────────
bind{ mods = "super", key = "d", action = "toggle_layout" }
bind{ mods = "super+shift", key = "d", action = "set_layout", layout = "float" }
bind{ mods = "super", key = "t", action = "set_layout", layout = "tile" }
-- Monocle moved to Super+Shift+M: Super+M went to the magnetism toggle (see
-- the Toggles section), and the Shift variant here is already used by float.
bind{ mods = "super+shift", key = "m", action = "set_layout", layout = "monocle" }
-- niri-like columns (vertical stacks, the camera scrolls to the active column)
bind{ mods = "super", key = "n", action = "toggle_niri_mode" }
-- The same Columns, but without the toggle — switch to it outright.
bind{ mods = "super+shift", key = "n", action = "set_layout", layout = "columns" }
-- ── Columns — the layout and the binds as in niri ────────────────────────
-- Width/height: niri's presets (⅓ → ½ → ⅔), percentages and a reset.
-- Super+R is taken by restarting the compositor (see below), so the width
-- presets moved to Super+Alt+R.
bind{ mods = "super+alt", key = "r", action = "column_width_cycle" }
bind{ mods = "super+shift", key = "r", action = "window_height_cycle" }
bind{ mods = "super+ctrl", key = "r", action = "window_height_reset" }
bind{ mods = "super", key = "minus", action = "column_width_adjust", percent = -10 }
bind{ mods = "super", key = "equal", action = "column_width_adjust", percent = 10 }
bind{ mods = "super+shift", key = "minus", action = "window_height_adjust", percent = -10 }
bind{ mods = "super+shift", key = "equal", action = "window_height_adjust", percent = 10 }
-- A column at full width and back; a column centred on the screen.
-- Super+F is WINDOW SEARCH by name: the letters show on screen and Enter flies
-- the camera to the match (switching the workspace if the window is on another
-- one). See switcher.rs. Full-width column moved from here to Super+Shift+F.
bind{ mods = "super", key = "f", action = "window_search" }
bind{ mods = "super+shift", key = "f", action = "column_maximize" }
-- Super+C is taken by the clipboard history (see below), so centring a column
-- moved to Super+Ctrl+C.
bind{ mods = "super+ctrl", key = "c", action = "column_center" }
-- The first/last column, and moving a column to the start/end of the strip.
bind{ mods = "super", key = "Home", action = "column_focus_first" }
bind{ mods = "super", key = "End",  action = "column_focus_last" }
bind{ mods = "super+ctrl", key = "Home", action = "column_move_to_first" }
bind{ mods = "super+ctrl", key = "End",  action = "column_move_to_last" }
-- Take a window into the column or push it out with one key (niri:
-- consume-or-expel-window-left/right).
bind{ mods = "super", key = "bracketleft",  action = "consume_or_expel_left" }
bind{ mods = "super", key = "bracketright", action = "consume_or_expel_right" }
-- A tabbed column: only the active window is shown, with a strip of tabs on
-- the left (niri: toggle-column-tabbed-display).
bind{ mods = "super+shift", key = "v", action = "column_toggle_tabbed" }

-- Focus between the floating layer and the strip of columns
-- (niri: switch-focus-between-floating-and-tiling). In niri this is Mod+Space,
-- but in parallax Super+Space is taken by zoom-nav (bird_eye_key, intercepted
-- before the bindings), so it lives on Super+Shift+Space.
bind{ mods = "super+shift", key = "space", action = "focus_floating_or_tiling" }
-- How the view follows the active column: never (the default, as in niri),
-- always, on-overflow. Changed on the fly.
bind{ mods = "super+alt", key = "c", action = "center_focused_column", mode = "always" }
bind{ mods = "super+alt+shift", key = "c", action = "center_focused_column", mode = "never" }
-- niri workspaces: Super+PageUp/Down switch the workspace (in Columns you stay
-- in Columns); Super+Ctrl+PageUp/Down move the active column to the neighbour.
bind{ mods = "super", key = "Next",  action = "workspace_step", dir = 1 }
bind{ mods = "super", key = "Prior", action = "workspace_step", dir = -1 }
bind{ mods = "super+ctrl", key = "Next",  action = "move_column_to_workspace", dir = 1 }
bind{ mods = "super+ctrl", key = "Prior", action = "move_column_to_workspace", dir = -1 }
bind{ mods = "super", key = "comma", action = "inc_nmaster", n = 1 }
bind{ mods = "super", key = "period", action = "inc_nmaster", n = -1 }
bind{ mods = "super+shift", key = "h", action = "set_mfact", delta = -0.05 }
bind{ mods = "super+shift", key = "l", action = "set_mfact", delta = 0.05 }

-- ── The workspace overview (tap Super) ───────────────────────────────────
-- The workspaces lie in a 2D grid around the current one: new ones take their
-- turn to the right, below, to the left and above the cells already taken (and
-- only then the diagonals). The workspaces themselves are not draggable — the
-- overview decides the arrangement and keeps it between visits.

-- ── Focus / navigation ───────────────────────────────────────────────────
bind{ mods = "super", key = "Left",  action = "focus_direction", dx = -1, dy = 0 }
bind{ mods = "super", key = "Right", action = "focus_direction", dx = 1, dy = 0 }
bind{ mods = "super", key = "Up",    action = "focus_direction", dx = 0, dy = -1 }
bind{ mods = "super", key = "Down",  action = "focus_direction", dx = 0, dy = 1 }
bind{ mods = "super", key = "j",   action = "focus_stack", dir = 1 }
bind{ mods = "super", key = "Tab", action = "focus_stack", dir = 1 }
bind{ mods = "super", key = "k", action = "focus_stack", dir = -1 }
bind{ mods = "super+shift", key = "Tab", action = "focus_stack", dir = -1 }
-- Alt+Tab cycles THE STACK: the windows lying on top of one another at the
-- same point of the canvas (the topmost hides the rest, and Super+arrows do
-- not reach them: those look for a neighbour to the side). The order is fixed
-- on the first Tab and held until Alt is released. With nothing under the
-- window, it cycles every window of the workspace.
bind{ mods = "alt", key = "Tab", action = "cycle_stack", dir = 1 }
bind{ mods = "alt+shift", key = "Tab", action = "cycle_stack", dir = -1 }

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
-- j/k/arrows here (matches the historical behavior of parallax's hardcoded
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
-- Super+Ctrl+N adds a tag to / removes it from the CURRENT VIEW (several
-- workspaces on screen at once), Super+Ctrl+Shift+N does the same to the tag
-- set of the focused window (one window visible on several workspaces). Like
-- toggleview/toggletag in dwm.
for i = 1, 9 do
  bind{ mods = "super", key = tostring(i), action = "view_tag", tag = i }
  bind{ mods = "super+shift", key = tostring(i), action = "tag_window", tag = i }
  bind{ mods = "super+ctrl", key = tostring(i), action = "toggle_view", tag = i }
  bind{ mods = "super+ctrl+shift", key = tostring(i), action = "toggle_tag", tag = i }
end

-- ── Toggles ───────────────────────────────────────────────────────────────
bind{ mods = "super", key = "grave", action = "toggle_minimap" }
bind{ mods = "super", key = "p", action = "toggle_portal" }
bind{ mods = "super", key = "b", action = "toggle_bookmarks_mode" }
bind{ mods = "alt", key = "b", action = "pin_bookmark_at_cursor" }
-- Super+S went to the application launcher (see below), so collision mode
-- moved here:
bind{ mods = "super", key = "a", action = "toggle_snapping" }
-- Magnetism is a SEPARATE toggle: collision shoves windows apart all the while
-- they are being moved, while magnetism fires once on release and lines an
-- edge up with a neighbour. The two used to sit on one flag (Super+A), and
-- there was no way to have shoving without sticking.
bind{ mods = "super", key = "m", action = "toggle_magnetism" }
-- Folding into a stack is off Super+Shift+S: the combination squeezed windows
-- into one point, which is not what anyone expected of it. If you want it,
-- bring the line below back.
-- bind{ mods = "super+shift", key = "s", action = "toggle_fold_stack" }

-- ── Multi-user: showing the desktop to guests ────────────────────────────
-- A toggle: it starts sharing and shows a six-digit code on the bar (on the
-- right, "code 123456 · N"); pressing it again stops. The guest connects with
-- plx-share to this machine's address and the code; the default port is 7373
-- (set your own with `port = 1234`). While sharing is on, tiling and
-- workspaces are off: every participant has their own camera over the shared
-- endless canvas.
bind{ mods = "super+shift", key = "s", action = "share_toggle" }

-- ── Keyboard layout switching ────────────────────────────────────────────
bind{ mods = "ctrl", key = "space", action = "layout_next" }
bind{ mods = "ctrl+shift", key = "space", action = "layout_prev" }

-- ── Config reload ─────────────────────────────────────────────────────────
bind{ mods = "super+shift", key = "c", action = "reload_config" }

-- ── Refreshing the compositor (Super+R) ──────────────────────────────────
-- A restart in place: the window session is saved to session.json, parallax
-- exits with code 42, and launch_native.sh brings it back up — rebuilding
-- first if the sources are newer than the binary. That is how a fresh build is
-- picked up without logging out of ly. The windows do close: the wayland
-- socket dies with the compositor. Re-reading config.lua alone is cheaper —
-- Super+Shift+C.
bind{ mods = "super", key = "r", action = "restart" }

-- ── Wallpapers (plx-wall) ──────────────────────────────────────────────────
-- Win+W opens the picker: wallpaper cards plus a "+" tile to add one. Hover a
-- card and a delete cross appears in its corner; a right click on the card
-- does the same. Win+Shift+W cycles wallpapers without the menu.
bind{ mods = "super", key = "w", action = "spawn", cmd = "pkill -USR2 -x plx-wall" }
bind{ mods = "super+shift", key = "w", action = "spawn", cmd = "pkill -USR1 -x plx-wall" }

-- ── The floating layer ─────────────────────────────────────────────────────
-- Win+V: the selected windows (or the focused one) go to the floating layer
-- and back. Works in tiling and in the niri strip alike; the window stays
-- within the bounds of its own workspace.
bind{ mods = "super", key = "v", action = "float_selected" }

-- ── Camera bookmarks ───────────────────────────────────────────────────────
-- Alt+B pins a bookmark under the cursor into the first free slot 1-9,
-- Alt+Win+B removes the one nearest the cursor. The slot numbers show on the
-- minimap (Super+`) next to the crosses. Jumping between bookmarks is Super+N
-- in bookmarks mode (Super+B).
bind{ mods = "alt+super", key = "b", action = "delete_nearest_bookmark" }

-- Jump to a camera bookmark: Win+Alt+digit. Super+digit is taken by the
-- workspaces and Super+N by the niri strip, so the bookmarks got Alt.
for i = 1, 9 do
  bind{ mods = "super+alt", key = tostring(i), action = "jump_bookmark", slot = i }
end

-- ── Bluetooth ───────────────────────────────────────────────────────────────
-- Win+Shift+B (or the XF86Bluetooth key, if the keyboard has one) is a device
-- menu inside the compositor, with no tray and no bluetoothctl.
-- Inside the menu: ↑/↓ (j/k) select, Enter connects (pairing first if the
-- device is new), D disconnects, F forgets, S scans, P powers the adapter,
-- Esc or a click outside closes. A click on a row is the same as Enter on it.
-- The pairing confirmation code is shown at the bottom of the same menu.
bind{ mods = "super+shift", key = "b", action = "bluetooth_menu" }
bind{ mods = "", key = "XF86Bluetooth", action = "bluetooth_menu" }
-- Adapter power without opening the menu.
bind{ mods = "super+ctrl", key = "b", action = "bluetooth_power" }

-- At session start, power the adapter up and connect the device used last (the
-- address is remembered in ~/.local/state/parallax/bluetooth on every connect
-- from the menu). set{ bluetooth_autoconnect = false } turns this off.
set{ bluetooth_autoconnect = true }

-- ── The headset: VR and augmented reality ──────────────────────────────────
-- The windows hang as panels around the room: the same compositor, the same
-- workspaces and binds, only the frame goes to a headset (a Quest 3 over Wi-Fi
-- through WiVRn, and in general any OpenXR runtime with XR_MNDX_egl_enable,
-- see src/vr/).
--
--   Win+Alt+V — put the headset on and take it off (the monitors keep working);
--   Win+Alt+A — passthrough: windows over the real room;
--   Win+Alt+G — the next layout: arc → wall → dome → free;
--   Win+Alt+H — gather the panels again around where you are looking.
--
-- These are on Win+Alt rather than Win+Shift: that row already holds column
-- tabs (V), the sound menu (A) and the config reload (C), and moving them
-- would break a trained hand for the sake of a mode you turn on once a day.
--
-- In the headset: the controller trigger is the left mouse button, the grip
-- drags a panel, the stick forward/back pushes it away and pulls it closer,
-- sideways resizes it. The keyboard and mouse work as usual; with no
-- controllers, your gaze is the pointer.
-- Win+Alt+V is THE WHOLE way in: parallax starts wivrn-server itself, finds
-- the OpenXR runtime itself, and waits two minutes while you put the Quest on
-- and start WiVRn on it — entering VR the second the headset appears. Press it
-- again: while waiting it cancels the wait, in the headset it takes the
-- headset off. There is also a raw "vr_toggle" action (no server, no waiting)
-- — that one is for the Monado simulator in the harness and for debugging.
bind{ mods = "super+alt", key = "v", action = "vr_mode" }
bind{ mods = "super+alt", key = "a", action = "vr_ar" }
bind{ mods = "super+alt", key = "g", action = "vr_layout" }
bind{ mods = "super+alt", key = "h", action = "vr_recenter" }

-- Minecraft: parallax windows as panels inside the game world (see mine/). The
-- bind turns the mode on — after that you need a running Minecraft with the
-- plx-mine mod, which connects to the socket by itself. Leave with the same
-- combination: the mode deliberately cannot be switched off from inside the
-- game, or the panels would vanish while the keyboard belongs to Minecraft.
bind{ mods = "super+alt", key = "m", action = "mine_mode" }

-- layout — the panel layout: "arc" (a semicircle around you, the default),
--          "wall" (flat in front of you), "dome" (in tiers), "free". The
--          Russian names ("дуга", "стена", "купол", "свободно") are accepted
--          just as well — the parser takes both, see config.rs;
-- scale  — metres per window pixel: 0.0008 gives a 1920-wide window about a
--          metre and a half, roughly a monitor on a desk;
-- radius — how far away the panels stand, in metres (0 = compute it from the
--          headset's guardian boundary, which the runtime reports);
-- ar     — enter passthrough straight away;
-- auto   — put the headset on when parallax starts.
vr{ layout = "arc", scale = 0.0008, radius = 0, ar = false, auto = false }

-- Controller gestures and buttons are ordinary parallax actions, the same ones
-- as in bind{}. The keys (the full list is `plxctl vr gestures`):
--   controller: menu_button (☰ and a click on the stick), button_a, button_b,
--               stick_left / stick_right / stick_up / stick_down;
--   hand:       fist, two_fists, thumb_up, palm_up,
--               pinch_middle / pinch_ring / pinch_little,
--               swipe_left / swipe_right / swipe_up / swipe_down.
-- A pinch of thumb and INDEX finger is not on the list: it is the left mouse
-- button — picking a window and pressing buttons — and there is nothing to
-- rebind it to.
-- The value is the name of an action as a string, or a table with an action
-- and its arguments. Whatever is not listed keeps its default: a fist and ☰
-- open the "start" panel, A/X the keyboard, B/Y a terminal, sideways is the
-- neighbouring workspace, up is the workspace overview, down centres the
-- camera on a window, thumb up makes a window fullscreen, the little finger is
-- passthrough, two fists rebuild the scene around your gaze.
vr{ gestures = {
      fist         = "vr_launcher",
      thumb_up     = "toggle_fullscreen",
      swipe_left   = { action = "workspace_step", dir = -1 },
      swipe_right  = { action = "workspace_step", dir = 1 },
      swipe_up     = "toggle_overview",
      swipe_down   = "center_window",
      pinch_little = "vr_ar",
      two_fists    = "vr_recenter",
} }

-- ── The status shelf ───────────────────────────────────────────────────────
-- Win+Shift+P (or a click on the little vertical strip to the right of the
-- workspace bar) is a row of icons: bluetooth, wi-fi, sound with a slider,
-- battery (if there is one), suspend, reboot, power off. A click on the
-- bluetooth icon opens its menu, on wi-fi toggles the radio, on the sound icon
-- mutes, on the slider sets the volume where you pressed. The power buttons
-- need a SECOND click: the first arms them (the icon reddens), the second
-- acts. Esc or a click outside closes.
bind{ mods = "super+shift", key = "p", action = "tray_menu" }

-- ── Wi-Fi and sound ─────────────────────────────────────────────────────────
-- Win+Shift+I — network picker: Enter connect (asks for the password on a new
-- protected network), D disconnect, F forget, S rescan, P radio, Esc close.
-- Win+Shift+A — sound devices: Enter makes the device default AND moves the
-- streams already playing, M mute, -/+ volume, Esc close.
-- Both also open from the tray: left click on the icon opens the menu, right
-- click does the quick thing (wi-fi radio, mute).
bind{ mods = "super+shift", key = "i", action = "wifi_menu" }
bind{ mods = "super+shift", key = "a", action = "audio_menu" }


-- ── The application launcher ────────────────────────────────────────────────
-- fuzzel in Void's colours (theme: ~/.config/fuzzel/fuzzel.ini).
-- A toggle: pressing it again closes the launcher instead of opening a second.
bind{ mods = "super", key = "s", action = "spawn", cmd = "pkill -x fuzzel || fuzzel" }

-- ── Screenshots and the clipboard ───────────────────────────────────────────
-- PrtScr: a shot of the WHOLE screen straight into the clipboard, no file is
-- written. grim gives the png on stdout, wl-copy picks it up from there.
bind{ mods = "", key = "Print", action = "spawn", cmd = "grim - | wl-copy" }
-- Super+C: the clipboard history as a list in fuzzel — images as well as text.
-- What you pick goes back into the clipboard (cliphist decode | wl-copy), so
-- it pastes with a plain Ctrl+V wherever you need it.
--
-- The history itself is filled by two wl-paste watchers, started by
-- launch_native.sh along with the session: without them the list is always
-- empty. A toggle, like the launcher: pressing it again closes the open list.
bind{ mods = "super", key = "c", action = "spawn",
      cmd = "pkill -x fuzzel || cliphist list | fuzzel --dmenu | cliphist decode | wl-copy" }

-- ── Monitors ────────────────────────────────────────────────────────────────
-- monitor{ name = "DP-2", width = 2560, height = 1080, refresh = 200 }
--   name    — the connector name from /sys/class/drm (DP-1, DP-2, HDMI-A-1) or
--             the model from the EDID ("Redmi 30 HFCW"). The connector is the
--             more reliable of the two.
--   width/height/refresh — the mode. An exact match on size, and the closest
--             frequency to refresh among those the connector actually offers.
--             If the connector has NO such size, parallax builds the mode
--             itself from CVT and hands it to the kernel: the hardware
--             stretches the smaller mode over the whole panel with its own
--             scaler. That is how a 4K panel gives you a real FullHD — four
--             times fewer pixels to draw, not merely a larger interface. If
--             even the synthesised mode is refused, we fall back to PREFERRED
--             (with a warn in the log). Asking for more than the physical
--             panel is pointless — such a request is rejected outright.
--             refresh without width/height means "the same size as PREFERRED,
--             but at this frequency". Nothing set at all = as it was.
--   x/y     — where the monitor stands RELATIVE TO ITS NEIGHBOURS (by default
--             to the left of the leftmost, left to right in connection order).
--             This is not a place on the canvas (every monitor has a canvas
--             rectangle of its own, see monitors::ШАГ_ДОМА) — the arrangement
--             is needed exactly for moving the cursor across the edge of the
--             screen (Super carries the mouse to the monitor below/above/
--             beside) and does not move windows or workspaces.
--   scale   — 1.0 by default. It divides the LOGICAL size of the workspace but
--             NOT the mode: the interface gets larger while the scanout and
--             the compositing stay as they were. This is about legibility
--             (DPI), not about load — if load is what you are after, set
--             width/height.
--   transform — normal | 90 | 180 | 270 | flipped | flipped-90/180/270.
--   primary — true makes the monitor the active one at startup even if the
--             kernel did not hand its connector over first (the DRM scan order
--             is not stable — without this flag the "main" monitor changed
--             from session to session). There must be exactly one primary in
--             the whole layout.
-- Applied when a connector appears, that is at startup and on hot-plugging a
-- cable; reload_config (Super+Shift+C) no longer changes the mode.
--
-- Redmi 30 HFCW (DP-2): 2560x1080, whose EDID reports 60 Hz as PREFERRED and
-- 200 Hz as the second detailed timing — without this line the compositor came
-- up at 60. It is also the main monitor, and sits on top in the layout.
monitor{ name = "DP-2", width = 2560, height = 1080, refresh = 200, x = 0, y = 0, primary = true }
-- BOE105HDR (HDMI-A-1): sits below, centred against the ultrawide —
-- (2560 - 1920) / 2 = 320.
monitor{ name = "HDMI-A-1", x = 320, y = 1080 }

-- ═══════════════════════════════════════════════════════════════════════════
-- TOUCHPAD GESTURES (gesture{}) — driftwm's model, brought over 30.08.2026
-- ═══════════════════════════════════════════════════════════════════════════
--
-- A gesture is a bind like any other, only its trigger is fingers:
--
--   gesture{ mods = "alt", fingers = 3, kind = "swipe",
--            where = "window", action = "resize-window" }
--
--   mods    — as in bind{}: "alt", "super", "shift", "ctrl" and combinations
--   fingers — how many fingers (2–5; two arrive as scroll, parallax hides that)
--   kind    — swipe, swipe-up/down/left/right, doubletap-swipe,
--             pinch, pinch-in, pinch-out, hold
--   where   — window (a window under the cursor), canvas (empty), anywhere
--             (the default)
--   action  — continuous or threshold, see below
--
-- CONTINUOUS actions follow the fingers every frame and can only be bound to
-- swipe/pinch (not to the directional ones and not to pinch-in/out, which fire
-- once):
--   pan-viewport  — pan the view (swipe only)
--   zoom          — camera zoom (pinch only)
--   move-window, move-snapped-windows      — move a window
--   resize-window, resize-window-snapped   — resize a window
-- A THRESHOLD action can be any action from the bind{} list, plus
-- center-nearest.
--
-- Recognition thresholds (named as in driftwm, so a line copies across):
--   set{ swipe_threshold = 12.0, pinch_in_threshold = 0.85, pinch_out_threshold = 1.15 }
--
-- ───────────────────────────────────────────────────────────────────────────
-- EVERYTHING BELOW IS COMMENTED OUT ON PURPOSE, and it is not laziness.
--
-- While the table is empty, gestures are handled by the older branches in
-- input.rs — that is, parallax behaves exactly as it did before gesture{}
-- existed. Uncomment a line and its gesture moves to the table ENTIRELY,
-- cancelling the built-in one. Where that cancels something that exists, the
-- line is marked "OVERRIDES" — read it before turning it on.
-- ───────────────────────────────────────────────────────────────────────────

-- NOTE (01.09.2026): everything below now WORKS WITHOUT YOU — this list is
-- built into parallax as the default (`ЖЕСТЫ_ПО_УМОЛЧАНИЮ` in config.rs) and is
-- applied after your config. The lines are kept here as a reference and as a
-- starting point for edits: your own `gesture{}` with the same trigger,
-- modifiers and context REPLACES the default rather than adding to it. To have
-- no gesture at all, give it `action = "none"`.

-- ── Over a window ──────────────────────────────────────────────────────────
-- gesture{ mods = "alt", fingers = 3, kind = "swipe", where = "window", action = "resize-window" }
-- gesture{ mods = "alt+shift", fingers = 3, kind = "swipe", where = "window", action = "resize-window-snapped" }
-- gesture{ mods = "alt", fingers = 3, kind = "pinch-in", where = "window", action = "toggle_fullscreen" }
-- gesture{ mods = "alt", fingers = 3, kind = "pinch-out", where = "window", action = "toggle_fullscreen" }

-- ── Over the canvas ────────────────────────────────────────────────────────
-- A bare two-finger pinch currently does NOTHING (the built-in zoom asks for
-- Alt), so this line cancels nothing — it is a pure addition.
-- gesture{ fingers = 2, kind = "pinch", where = "canvas", action = "zoom" }

-- ── Anywhere ───────────────────────────────────────────────────────────────
-- OVERRIDES: in Columns a bare three-finger swipe scrolls the strip and the
-- workspaces.
-- gesture{ fingers = 3, kind = "swipe", action = "pan-viewport" }
-- OVERRIDES: in Columns four fingers currently scroll the strip just as three do.
-- gesture{ fingers = 4, kind = "swipe", action = "center-nearest" }
-- gesture{ mods = "super", fingers = 3, kind = "swipe", action = "center-nearest" }
-- gesture{ mods = "super", fingers = 2, kind = "pinch", action = "zoom" }
-- gesture{ fingers = 3, kind = "pinch", action = "zoom" }
-- gesture{ fingers = 4, kind = "pinch-out", action = "toggle_overview" }
-- gesture{ mods = "super", fingers = 3, kind = "pinch-out", action = "toggle_overview" }
-- gesture{ fingers = 4, kind = "hold", action = "center_window" }
-- gesture{ mods = "super", fingers = 3, kind = "hold", action = "center_window" }

-- ── What there is NOTHING to bring over from driftwm ────────────────────────
-- These actions of its own do not exist in parallax, and inventing a meaning
-- for them would be worse than saying so plainly:
--   fit-window, fit-window-snapped   — "grow into the free space"
--   zoom-to-fit, zoom-to-fit-snapped — "fit everything on screen"
-- The triggers for them are there (pinch-in/out on 2 and 4 fingers) — it is
-- the actions that are missing.
--
-- Separately: 3-finger-doubletap-swipe (tap with three, then swipe) needs the
-- touchpad's middle click delayed, and the middle click in parallax is already
-- taken — it stops sharing from the bar, for one. The trigger is parsed but
-- does not fire yet.

-- ═══════════════════════════════════════════════════════════════════════════
-- CURSOR EDGE MOTION ON THE TOUCHPAD
-- ═══════════════════════════════════════════════════════════════════════════
--
-- The pad runs out before the screen does: dragging a window across the whole
-- canvas, your finger hits the edge and the movement simply stops. Edge motion
-- notices the finger IN THE EDGE ZONE of the pad and keeps carrying the cursor
-- itself — the faster the deeper the finger has gone.
--
-- It works everywhere and by itself: the movement goes through the ordinary
-- pointer path, so a window drag, a rubber-band selection and any grab all see
-- it alike. This is about the edge of THE PAD, not of the screen.
--
-- It needs the touchpad read directly (libinput does not report raw finger
-- coordinates); the descriptor comes from the same session as libinput's. No
-- touchpad means no edge motion, and nothing will be said about it.
--
--   touchpad_edge_motion — on or off (on by default)
--   touchpad_edge_zone   — the fraction of the pad counted as its edge
--                          (0.08 = 8 %, about a finger's width; capped at 0.4)
--   touchpad_edge_speed  — pixels per second at the very edge
--
--   touchpad_edge_only_drag — carry the cursor ONLY while something is being
--                          dragged (off by default: "make it work everywhere"
--                          was the request). Turn it on if a finger resting on
--                          the rim starts walking the cursor away on its own.
--
-- set{ touchpad_edge_motion = true, touchpad_edge_zone = 0.08,
--      touchpad_edge_speed = 900.0, touchpad_edge_only_drag = false }

-- ─────────────────────────────────────────────────────────────────────────────
-- MOUSE CHORDS (mouse{}) — hevel's model, brought over 05.09.2026
--
-- hevel (git.sr.ht/~dlm/hevel) is a floating, scrolling WM in which absolutely
-- everything is done with the mouse: a command is given not by a button but by
-- a PAIR of buttons pressed in sequence. The buttons are numbered as there:
-- 1 left, 2 wheel, 3 right.
--
--   mouse{ chord = "1-3", action = "spawn_rect", cmd = "ghostty" }
--
-- The first digit is the button held, the second the one pressed after it.
--
-- The chord actions:
--   spawn_rect     — draw a rectangle and open a window in it (needs cmd)
--   close_under    — close the window RELEASED OVER (the victim is chosen on
--                    the way, not at the start)
--   pan            — drag the camera
--   move_window    — drag a window
--   resize_window  — resize a window
-- Plus ANY action from the common table (the same ones that go on keys) — that
-- fires immediately on the second button.
--
-- WHAT IT COSTS. A chord is recognised by its SECOND button, so the first one
-- cannot be handed to the application at once — otherwise "3-1" would have
-- opened a context menu on the way. The first press is HELD BACK for
-- mouse_chord_timeout (250 ms by default, the name and the value from hevel).
-- If the second button never comes, or the same one is released, the
-- application gets an ordinary click, only a little later.
--
-- The delay is SELECTIVE: only a button that some chord STARTS with is held
-- back. That is why the lines below are commented out, and why not one of them
-- is a "1-…" chord: turn one on and you delay every ordinary left click. If
-- you want hevel's full set, uncomment everything; if you would rather leave
-- the left button alone, keep only the "2-…" and "3-…" ones.
--
-- An empty table (nothing uncommented) = the mouse works exactly as it did
-- before this section existed. That is the same promise gesture{} makes.
--
-- mouse{ chord = "1-3", action = "spawn_rect", cmd = "ghostty" }
-- mouse{ chord = "3-1", action = "close_under" }
-- mouse{ chord = "3-2", action = "pan" }
-- mouse{ chord = "2-1", action = "move_window" }
-- mouse{ chord = "2-3", action = "resize_window" }
-- mouse{ chord = "1-2", action = "toggle_fullscreen" }
-- set{ mouse_chord_timeout = 250 }
