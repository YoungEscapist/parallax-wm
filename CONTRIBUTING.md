# Contributing

*[Русская версия](CONTRIBUTING.ru.md)*

Parallax is a one-person compositor that grew into something other people can
run. Patches are welcome; so are bug reports that just describe what you saw.
Write in English or Russian — both are read.

## Before you start

Two things about this tree surprise everyone who opens it for the first time:

**The comments and the identifiers are in Russian.** Not the strings you see on
screen (those go through `t!`/`tf!` and exist in both languages), and not the
logs — those are always English, because a log ends up in someone else's bug
report. But `fn найти_окно`, `struct Холст`, and every comment. This is
deliberate and it is not going to change: the author thinks in Russian, and a
compositor is mostly reasoning, not API surface. If you send a patch in English
identifiers it will be taken and translated rather than refused — say so in the
PR and nobody will spend time confused.

**Comments explain *why*, at length.** A block that took a day to get right
carries the story of what was wrong before it, and often the measurement that
proved it. Please keep that habit for anything non-obvious. "Fixed hit test" is
worth less than "X clamps the pointer into the root window, and our canvas is
unbounded — hence the offset".

## Building

Rust via rustup, `pkg-config`, a C compiler, and the development packages listed
in the header of `build_portable.sh`. Lua is compiled from source by `mlua`.

```sh
./build.sh                       # both binaries, into /mnt/plx-build/target
cargo build --release -p plx-standard
cargo build --release -p plx-extra
```

Never `cargo build --workspace`. Cargo unifies features across workspace
members, so a single invocation gives you two binaries that both contain
everything — measured, not feared: after one `--workspace` run both binaries
were 17.8 MiB byte for byte, and the "standard" one had OpenXR inside it.
`build.sh` calls cargo twice, with separate `--target-dir`s, for this reason.

`.cargo/config.toml` in this repository links with `mold` and compiles with
`target-cpu=native` — right for the author's machine, wrong for yours if you do
not have mold or a Zen 3. Override it with the `RUSTFLAGS` environment variable
(it beats the config file) or edit the file locally; `build_portable.sh` already
does the portable thing.

## Checking your change

Do not send a change you have only reasoned about. There are two cheap ways to
actually look at it:

**The headless harness** — a full compositor with its own `HOME`, its own D-Bus
and a control socket, but no screen and no input. It does not touch your live
session, so you can use it while the real one is running:

```sh
./harness.sh                       # one 2560x1080 output
MODE=1920x1280@60,2560x1080@60 ./harness.sh    # two monitors
./ctl.py 'shot /tmp/a.png'         # take a frame
./ctl.py windows                   # what is open and how big
./ctl.py 'key logo+space'          # press something (mods: logo, shift, ctrl, alt)
./ctl.py 'action toggle_overview'  # or run an action by name, no key needed
./ctl.py help                      # everything it understands
```

Run the two-monitor variant for anything that computes canvas coordinates. The
second output's home is at (1 000 000, 0), and a "counts from canvas zero" bug
is completely invisible on one monitor.

**The tests** — roughly two hundred of them, all pure: canvas geometry, the
tiling tree, config parsing, protocol framing. No display needed.

```sh
cargo test --features extra
```

## Traps this tree has already sprung

They cost real hours. In rough order of how often they bite:

- **A Cyrillic variable name in a shell script is not a variable.** `имя=значение`
  is parsed by bash as a *command*, so it fails with "command not found" and
  `set -e` does not catch it (an assignment inside an `if` is not the last
  command of the pipeline). This silently disabled a `chown` in `build.sh` and
  made all of `migrate.sh` do nothing while exiting 0. Function names in
  Cyrillic are fine — bash accepts those. **Variable names: Latin only.**
- **Single-character Cyrillic identifiers in Rust.** `с`/`c`, `р`/`p`, `о`/`o`,
  `г`/`r` are indistinguishable on screen, and rustc's `confusable_idents` is
  right to say so. The lint is nevertheless allowed crate-wide in `src/lib.rs`,
  with the reason written above the attribute: it fires on one pair at a time,
  so fixing one place only moves the warning to the next file, and in Russian
  code homoglyphs are the norm rather than a typo. That is a decision about the
  *lint*, not about naming: prefer a name that reads (`рад`, `глуб`, `статус`)
  over a single letter, and do not add new one-letter Cyrillic locals.
- **The config reference is two files.** `default_config.lua` (English, the one
  embedded in the binary) and `default_config.ru.lua` (Russian) must differ only
  in their comments — a knob added to one belongs in the other on the same line.
  `config::tests::два_справочника_совпадают` compares everything that is not a
  comment and fails if they drift.
- **`cargo fmt` over the whole tree** produces a 28 000-line diff. The tree is
  not rustfmt-formatted. Format the lines you touched, by hand, in the
  surrounding style.
- **`pgrep -f` and `pkill -f` match themselves**, and `pkill -f parallax` from a
  script will happily kill the live session on TTY. The harness kills by
  `XDG_RUNTIME_DIR` in `/proc/*/environ` instead — copy that approach.
- **Solid colours must be premultiplied** before they go to smithay, or they
  come out as white rectangles.
- **A missing key in `config.lua` reads as `false`**, not as nil, through mlua.
  Every `set{}` used to switch off the settings it did not mention. If you add a
  boolean knob, go through `config.rs` and see how the existing ones distinguish
  "absent" from "false".

## Layout of the source

`src/` is flat on purpose — one file per subsystem, ~57 000 lines in 81 files.

| | |
|---|---|
| `lib.rs`, `state.rs` | startup, the event loop, the shared state |
| `canvas.rs`, `anim.rs`, `monitors.rs` | the infinite plane, the camera, animation curves |
| `tiling.rs`, `dwindle.rs`, `columns.rs`, `fullscreen.rs` | layouts |
| `overview.rs`, `switcher.rs`, `constellation.rs` | navigation across the canvas |
| `bar.rs`, `tray.rs`, `sni.rs`, `wifi.rs`, `audio.rs`, `bluetooth.rs` | the built-in shell |
| `blur.rs`, `rounded.rs`, `decor.rs`, `icons.rs`, `text.rs` | drawing |
| `udev.rs`, `winit.rs`, `headless.rs` | backends: TTY/DRM, nested, harness |
| `xwayland.rs`, `xwin.rs` | X11 clients and the fixes games need |
| `input.rs`, `touchpad.rs`, `gestures.rs`, `grabs/` | input |
| `config.rs`, `lang.rs`, `ctl.rs` | Lua config, translations, the control socket |
| `vr/`, `mine/`, `share/` | optional features, each with a `*_stub/` twin |

A `*_stub/` directory is the same module with the same shape, doing nothing.
That is why the calling code never has a `#[cfg]` in it: turning a feature off
swaps the implementation, it does not delete the call.

## Commits

Subject lines in this repository are Russian and describe the area, then the
change: `Оформление: панель, шрифт, скругление, блюр, значки`. Follow whatever
is natural for you; a clear subject matters more than the language.

One logical change per commit. If you found the bug and its cause, put the cause
in the commit message — it is where the next person will look.

## License

By contributing you agree that your work goes out under GPL-3.0-or-later, the
license of the project. The third-party work Parallax stands on is listed in
[THIRD-PARTY.md](THIRD-PARTY.md); if your change ports code from somewhere else,
add the source there too.
