# Installing Parallax

*[Русская версия](INSTALL.ru.md)*

Parallax is a Wayland compositor. It takes over the display: it wants a **clean
TTY** with no other graphical session holding DRM master, and it is **beta
software written by a neural network** (see the note in the [README](README.md)).
Install it on a machine you can still get into if the session refuses to start —
another TTY, SSH, or a display manager entry you can back out of.

There are no binary packages yet. You build it from source; on a modern desktop
that takes a few minutes.

---

## Quick install

From a checkout:

```sh
git clone https://github.com/YoungEscapist/parallax-wm.git
cd parallax-wm
./install.sh
```

Or without cloning first — the script clones into `~/.local/src/parallax-wm` and
continues there:

```sh
curl -fsSL https://raw.githubusercontent.com/YoungEscapist/parallax-wm/master/install.sh | bash
```

Flags go through `bash -s --`, e.g. `… | bash -s -- --extra --quick`.

`install.sh` does five things, shows every command before running it, and asks
before each one that needs root:

1. installs the system development packages (Void, Arch, Debian/Ubuntu, Fedora);
2. checks for Rust, offering [rustup](https://rustup.rs) if there is none;
3. builds the compositor;
4. copies `default_config.lua` to `~/.config/parallax/config.lua` (an existing
   config is never touched);
5. registers a **Parallax** entry with your display manager.

Useful flags:

| | |
|---|---|
| `--extra` / `--both` | build `plx-extra` instead of / along with `plx-standard` — see [Which build](#which-build) |
| `--quick` | build with the `quick` profile: same optimised code, thin LTO, ~8× faster to rebuild, and it fits in less memory |
| `--native` | compile for *this* CPU (`-C target-cpu=native`); faster, but the binary will `SIGILL` on another machine |
| `--jobs N` | limit cargo's parallelism on a small machine |
| `--no-deps`, `--no-rust`, `--no-build`, `--no-config`, `--no-session` | skip a step |
| `--update` | `git pull` and rebuild what you already have |
| `--uninstall` | remove the session entry (`--purge` removes the config too) |
| `--dry-run` | print what would happen and do nothing |
| `-y` | don't ask |

Everything below is the same thing done by hand.

---

## Requirements

* **Linux with DRM/KMS.** Any GPU with a working Mesa or NVIDIA driver.
  Development also works nested in an existing Wayland session (winit backend).
* **Rust**, a recent stable. The tree is developed on 1.98; `install.sh` warns
  below 1.82, since smithay and several crates want a modern compiler.
  Distribution packages are fine, rustup is what upstream uses.
* **A C compiler and pkg-config** — several dependencies build C code.
* **Memory and disk**: the release profile uses fat LTO, which links in a single
  process and wants several GiB to itself — `install.sh` warns below 6 GiB of
  RAM. The `--quick` profile links in parallel and needs much less. The target
  directory grows to a few GiB.
* At runtime: `Xwayland` (for X11 applications), and a session D-Bus for the
  tray, the screencast portal and the notification sound.

### Dependencies

Development packages for: wayland, libxkbcommon, libinput, udev, libseat,
libdrm, gbm, EGL, GLES, libdisplay-info, PipeWire (with libspa — the screencast
portal goes through it) and pixman (smithay's renderer links `-lpixman-1`).

Lua does **not** need to be installed: `mlua` builds Lua 5.4 from source.

```sh
./dist/install-deps.sh --print   # show the exact command for your distribution
sudo ./dist/install-deps.sh      # run it
```

<details>
<summary>The package lists, if you would rather type them yourself</summary>

**Void**

```sh
sudo xbps-install -y base-devel pkg-config \
  wayland wayland-devel libxkbcommon libxkbcommon-devel \
  libinput libinput-devel eudev-libudev-devel libseat libseat-devel \
  libdrm libdrm-devel MesaLib-devel libglvnd-devel \
  libdisplay-info libdisplay-info-devel pipewire-devel pixman-devel \
  xorg-server-xwayland
```

**Arch**

```sh
sudo pacman -S --needed base-devel pkgconf wayland libxkbcommon libinput \
  systemd-libs seatd libdrm mesa libdisplay-info pipewire pixman xorg-xwayland \
  clang
```

On Arch derivatives without systemd (Artix and friends) there is no
`systemd-libs` package at all — libudev comes from `libudev` instead. Since
`pacman` refuses the whole command over one unknown name, put `libudev` in place
of `systemd-libs` there; `install-deps.sh` picks the right one on its own.

**Debian / Ubuntu**

```sh
sudo apt install build-essential pkg-config \
  libwayland-dev libxkbcommon-dev libinput-dev libudev-dev libseat-dev \
  libdrm-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev \
  libdisplay-info-dev libpipewire-0.3-dev libspa-0.2-dev libpixman-1-dev \
  xwayland
```

**Fedora**

```sh
sudo dnf install gcc pkgconf-pkg-config \
  wayland-devel libxkbcommon-devel libinput-devel systemd-devel \
  libseat-devel libdrm-devel mesa-libgbm-devel mesa-libEGL-devel \
  mesa-libGLES-devel libdisplay-info-devel pipewire-devel pixman-devel \
  xorg-x11-server-Xwayland
```

**NixOS** — `nix-shell` in the checkout, then `./build.sh`. Mind the warning at
the top of `build.sh`: a binary linked against Nix's glibc segfaults on a
non-Nix system and vice versa.

</details>

---

## Which build

Parallax ships as two binaries built from one crate with different feature sets.
There is no second source tree: what a feature turns off is replaced by a stub
of the same shape, so the calling code is identical in both.

| | `plx-standard` | `plx-extra` |
|---|---|---|
| compositor, tiling, ribbon, overview, wallpaper | yes | yes |
| bar, tray, bluetooth, wifi, audio, portal, screenshot, X11, gestures | yes | yes |
| VR headset | — | yes |
| windows inside Minecraft | — | yes |
| multi-user desktop sharing | — | yes |
| window glow, canvas light, desktop cube | — | yes |

`plx-standard` is the default and is what most people want: it is the build
where every pixel takes the shortest path to the screen. The optional parts of
`plx-extra` are all off by default anyway — you turn them on in the config.

You can switch later: build the other binary and restart the session.

---

## Building by hand

```sh
./build_portable.sh              # both binaries, release profile
./build_portable.sh --quick      # thin LTO: ~8× faster rebuilds, less memory
PLX_NATIVE=1 ./build_portable.sh # compile for this CPU (non-portable binary)
```

The binaries land in `target/standard/<profile>/plx-standard` and
`target/extra/<profile>/plx-extra`, and are copied to `target/release/` where
`launch_native.sh` looks for them.

One binary on its own:

```sh
cargo build --release -p plx-standard
```

> **Do not build both with a single `--workspace` command.** Cargo unifies
> features across workspace members, so you would get two identical binaries
> that both contain everything — measured: 17.8 MiB each, byte for byte, with
> OpenXR inside the "standard" one. The build scripts invoke cargo twice, with
> separate target directories, for exactly this reason.

The first build fetches smithay from git (the revision is pinned) and a lot of
crates, so it needs a network. `Cargo.lock` is committed.

**Build profiles.** `release` is fat LTO with one codegen unit — the fastest
frame, and about five minutes to rebuild after a one-line change on a 16-thread
machine. `quick` is the same optimised code with thin LTO and 16 codegen units:
36 seconds for the same change, at a few percent of frame time. Use it while
you are editing, or when fat LTO does not fit in your RAM.

`.cargo/config.toml` holds nothing machine-specific on purpose. The build
scripts pick up `mold` or `lld` if either is installed, and `-C
target-cpu=native` is opt-in (`--native` / `PLX_NATIVE=1`) because a binary
built that way crashes with `SIGILL` on a different CPU.

---

## Running it

Parallax needs a **clean TTY** — switch with Ctrl+Alt+F3, log in, then:

```sh
cd ~/.local/src/parallax-wm
./launch_native.sh
```

`launch_native.sh` brings up a session bus if there is none, starts the
wallpaper daemon if you have one, writes logs to `logs/`, and rebuilds first if
the sources are newer than the binary.

Quit with **Super+Shift+Q**, restart in place with **Super+R**.

For a look around without leaving your current desktop, run it nested in a
window:

```sh
./launch_native.sh --winit
```

### From a display manager

```sh
sudo ./dist/install-session.sh          # --uninstall removes it again
```

That installs exactly two files — `/usr/local/bin/parallax-session` and
`/usr/share/wayland-sessions/parallax.desktop` — and nothing else. The binary
stays in the checkout where the build put it, and the wrapper reaches it through
`launch_native.sh`; so a rebuild and `Super+R` keep pointing at the same place
instead of at a stale copy under `/usr/local/bin`.

Works with ly, greetd, SDDM and GDM. Override the checkout path with
`PLX_CHECKOUT`, the install prefixes with `BIN_DIR` and `SESSIONS_DIR`, and the
binary itself with `PLX_BINARY` (that last one is for distribution packages,
where there is no source tree at all).

---

## Configuration

```sh
mkdir -p ~/.config/parallax
cp default_config.lua ~/.config/parallax/config.lua
```

The file is Lua and is re-read on save — no restart. Every key is documented in
place, in the file itself; the README has the tour. `default_config.ru.lua` is
the same file with Russian comments (`install.sh` picks it if your locale is
Russian).

The companion tools are separate and optional: [plx-wall](https://github.com/mifaroslav-dotcom/plx-wall)
for live wallpapers and the colour palette the desktop tints itself with, and
`plx-share` / `plx-host` for showing your desktop to a guest.

---

## Updating

```sh
cd ~/.local/src/parallax-wm
./install.sh --update
```

or by hand: `git pull && ./build_portable.sh`. If the session is running,
**Super+R** restarts it in place — it rebuilds first when the sources are newer
than the binary, and keeps your windows.

## Uninstalling

```sh
./install.sh --uninstall     # session entry
./install.sh --purge         # session entry and ~/.config/parallax
rm -rf ~/.local/src/parallax-wm
```

Nothing else is written outside the checkout, except `~/.local/bin/plx-host` if
you built `plx-extra`.

---

## When it does not work

**The build fails on a missing `.pc` file / `pkg-config` error.** A development
package is missing — `./dist/install-deps.sh --print` lists the full set for
your distribution. The usual suspects are `libspa-0.2-dev`,
`libpipewire-0.3-dev` and `libpixman-1-dev`: nothing else pulls them in, and the
failure happens deep inside a build script where the cause is not obvious.

**The build is killed, or the machine freezes near the end.** That is fat LTO
linking in one process. Build with `--quick`, or `--jobs 4`.

**`cannot find -fuse-ld=mold`.** An old `.cargo/config.toml` from before this
was fixed, or your own `RUSTFLAGS`. The current tree asks for no linker it has
not found in your system.

**The compositor starts and exits immediately, or the screen goes black and
comes back.** Something else holds DRM master — another compositor, X11, or a
display manager on that VT. Use a TTY where nothing graphical is running.

**"Permission denied" opening `/dev/dri/card0` or an input device.** You need a
seat manager: `seatd` (add yourself to the `_seatd`/`seat` group and enable the
service) or elogind/systemd-logind. Parallax uses libseat and does not need to
be setuid.

**Started from a display manager, the session dies immediately.** Look at the
display manager's log: `parallax-session` prints what it tried. The two usual
causes are a checkout path that moved (reinstall the session file) and a missing
`XDG_RUNTIME_DIR` on a system without elogind (the wrapper creates one, but the
directory must be writable).

**No tray, no screen sharing, no notification sound.** There is no session
D-Bus. `launch_native.sh` and `parallax-session` start one with
`dbus-run-session` if they can — install `dbus` if the log says they could not.

**It runs, but everything is slow, or it crashes with `SIGILL`.** A binary built
with `--native`/`PLX_NATIVE=1` on a different machine. Rebuild without it.

Logs live in `logs/` inside the checkout (`launch_native.sh`), or in the display
manager's log when started from there. `RUST_LOG=parallax=debug` makes them
talkative. Bug reports: <https://github.com/YoungEscapist/parallax-wm/issues>.
