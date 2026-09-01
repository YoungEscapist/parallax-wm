# Parallax

*[Русская версия](README.ru.md)*

A Wayland compositor where windows live on one infinite canvas and the screen is
a camera looking at it.

Most compositors give you a screen and ask how to divide it. Parallax gives you a
plane without edges. Workspaces are places on that plane, not separate worlds:
you fly to them, you can zoom out until several of them fit on screen at once,
you can pin a bookmark anywhere and jump back to it. Tiling still works — it just
happens inside a region of the canvas instead of inside your monitor.

Written in Rust on [smithay](https://github.com/Smithay/smithay), for TTY/DRM and
(for development) winit. Russian is the language of the code comments and of the
in-tree docs; this file and the config reference are in English too.

> **Status: pre-release.** It is a daily driver on the author's machine — games
> under Xwayland, Steam, screen sharing, two monitors — but it has had exactly
> one set of eyes on it. Expect rough edges, and run it from a TTY you can
> escape from.

## What it does

**The canvas**
- One unbounded plane for every window; the output is a camera over it, with
  smooth pan, zoom and inertia.
- Zoom-nav (`Super+Space`): pull back for a bird's-eye view and pan with the
  arrow keys.
- Minimap (`Super+~`) with live window thumbnails, its own pan/zoom, and
  click-to-fly.
- Camera bookmarks: drop one anywhere (`Alt+B`), jump to it (`Super+Alt+1..9`),
  or switch to bookmark mode instead of workspaces entirely (`Super+B`).
- Overview shows the workspaces exactly as they are — floating windows stay
  floating, monocle stays monocle — because a workspace in the overview is just
  the same windows, shifted.

**Layouts**
- `tile` — Hyprland-style dwindle (BSP): a new window splits the focused slot.
- `columns` — a niri-style scrollable strip, with tabbed columns.
- `float`, `monocle`, and per-workspace switching between all of them.
- No minimum window size: a client that refuses to shrink gets cropped in the
  renderer, so the tiling stays honest.
- Constellations: bind windows into a group that moves as one.

**The shell, built in**
- A bar with three islands: workspaces, window chips with real application
  icons, and a status side with tray (StatusNotifierItem), Wi-Fi, audio and
  Bluetooth menus.
- Hovering a window chip shows a live preview of where that window is.
- Rounded corners, drop shadows, optional background blur behind the bar,
  shelf, menus and preview cards.
- Wallpapers can live on the canvas and drift with the camera instead of being
  glued to the screen. Live (video) wallpapers come from the companion tool
  [dwall](https://github.com/mifaroslav-dotcom/dwall).

**The rest**
- Xwayland, with the pointer-clamping and hit-test fixes that games need.
- Screen sharing through an xdg-desktop-portal backend of its own, plus
  wlr-screencopy and a built-in region screenshot.
- Multi-monitor: independent workspaces, `monitor{ primary }`, drag a window
  across the edge to send it to the other screen.
- Guest sharing: show your desktop to someone else with an input seat of their
  own (`Super+Shift+S`, client: [dshare](https://github.com/mifaroslav-dotcom/dshare)).
- VR: put your windows on panels inside a headset over OpenXR/WiVRn
  (`Super+Alt+V`) — experimental, verified against a Monado simulator.
- Configuration is Lua, reloaded live with `Super+Shift+C`.

## Building

Rust (via rustup), pkg-config, a C compiler, and the development packages for
wayland, libxkbcommon, libinput, udev, libseat, libdrm, gbm, EGL, GLES and
libdisplay-info. Lua is built from source by `mlua`, so you do not need it
installed. `xwayland` is needed at runtime.

Exact package names per distribution are in the header of `build_portable.sh`.

```sh
./build_portable.sh            # release build into target/release/parallax
```

The first build pulls smithay from git (the revision is pinned in `Cargo.toml`)
and a lot of crates, so it needs a network. `Cargo.lock` is committed.

NixOS users: `shell.nix` plus `build.sh` (note that a binary linked against
Nix's glibc will segfault on a non-Nix system, and vice versa — see the comment
at the top of `build.sh`).

## Running

Only from a **clean TTY**, with no graphical session holding DRM master
(Ctrl+Alt+F3, log in, then run it):

```sh
./launch_native.sh             # release
./launch_native.sh --debug
./launch_native.sh --winit     # nested in an existing session, for development
```

Quit with `Super+Shift+Q`, restart in place with `Super+R`. Logs land in `logs/`.

## Configuration

```sh
mkdir -p ~/.config/parallax
cp default_config.lua ~/.config/parallax/config.lua
```

`default_config.lua` is both the default configuration and the reference manual:
every knob is documented next to the line that sets it. It is Lua, evaluated at
startup and again on `Super+Shift+C`.

A few starting points:

```lua
xkb{ layout = "us,ru" }                       -- keyboard layouts, Ctrl+Space to switch
bind{ mods = "super", key = "Return",         -- your terminal
      action = "spawn", cmd = "ghostty" }
set{ blur = true }                            -- frosted glass behind the bar
set{ anim_speed = 1.0 }                       -- tempo of every animation
set{ infinite_wallpaper = true }              -- wallpaper rides the canvas
monitor{ name = "DP-2", primary = true }
```

### Some default keys

| Key | Action |
| --- | --- |
| `Super+Return` | terminal |
| `Super+Q` | close window |
| `Super+Shift+Q` | quit the compositor |
| `Super+1..9` | go to workspace |
| `Super+Shift+1..9` | send window to workspace |
| `Super+T` / `Super+Shift+D` / `Super+Shift+M` / `Super+Shift+N` | tile / float / monocle / columns |
| `Super+Space` | zoom-nav (bird's eye) |
| `Super+~` | minimap |
| `Super+B`, `Alt+B`, `Super+Alt+1..9` | bookmark mode, drop bookmark, jump |
| `Super+F` | search windows |
| `Super+V` | float the selected window |
| `Super+Shift+C` | reload the config |
| `Super+R` | restart the compositor |

The full list — including mouse gestures, touchpad gestures and VR controller
bindings — is in `default_config.lua`.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).

The bundled Nunito font is under the SIL Open Font License; its text is in
[assets/Nunito-OFL.txt](assets/Nunito-OFL.txt).
