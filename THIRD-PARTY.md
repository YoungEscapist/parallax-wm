# Third-party notices

Parallax is licensed under GPL-3.0-or-later (see [LICENSE](LICENSE)). This file
lists the third-party work that is either distributed with it or that parts of
it are derived from, together with the notices those licenses require. It is not
a formality: two of the licenses below ask for exactly this and nothing more.

Every dependency pulled from crates.io is permissive (MIT, Apache-2.0, BSD-2/3,
ISC, Zlib, Unicode-3.0) and compatible with GPL-3.0; the full list with versions
is `Cargo.lock`, and `cargo metadata` will print their licenses. Only the items
that need a notice of their own are named here.

## Code

### Smithay (MIT)

Parallax is built on [Smithay](https://github.com/Smithay/smithay) and its
startup and event-loop structure follows Smithay's `anvil` example compositor.

> MIT License
>
> Copyright (c) 2017 Victor Berger and Victoria Brekenfeld
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

### Hyprland (BSD-3-Clause)

`src/dwindle.rs` is a port of the dwindle (BSP) tiling algorithm from
[Hyprland](https://github.com/hyprwm/Hyprland),
`src/layout/algorithm/tiled/dwindle/DwindleAlgorithm.cpp` — `addTarget`,
`removeTarget`, `recalcSizePosRecursive`, `resizeTarget`,
`moveTargetInDirection`, `moveToRoot`. It is a reimplementation in Rust on a
different data structure, not copied source, but the behaviour is deliberately
Hyprland's, down to the names of the `dwindle:*` settings. Several other files
follow Hyprland's model where the comments say so (monitor and workspace
ownership in `src/monitors.rs`, resize grabs in `src/grabs/resize_grab.rs`).

> BSD 3-Clause License
>
> Copyright (c) 2022-2026, vaxerski
>
> Redistribution and use in source and binary forms, with or without
> modification, are permitted provided that the following conditions are met:
>
> 1. Redistributions of source code must retain the above copyright notice, this
>    list of conditions and the following disclaimer.
> 2. Redistributions in binary form must reproduce the above copyright notice,
>    this list of conditions and the following disclaimer in the documentation
>    and/or other materials provided with the distribution.
> 3. Neither the name of the copyright holder nor the names of its contributors
>    may be used to endorse or promote products derived from this software
>    without specific prior written permission.
>
> THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS"
> AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
> IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
> DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE
> FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
> DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR
> SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER
> CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY,
> OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE
> OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

### driftwm (GPL-3.0-or-later)

The gesture model in `src/gestures.rs` — touchpad gestures as a binding table,
`"<mods>+<N>-finger-<kind>" = "<action>"` — is taken from
[driftwm](https://github.com/malbiruk/driftwm), and the trigger names are kept
identical on purpose so that a configuration line can be moved from one
compositor to the other unchanged. driftwm is also where the idea of doing zoom
in the renderer rather than in output scale came from.

> driftwm — a trackpad-first infinite canvas Wayland compositor
> Copyright (C) 2026 Klim Kostiuk
>
> Licensed under the GNU General Public License, version 3 or later — the same
> license as Parallax.

### hevel (ISC)

The mouse-chord model in `src/аккорды.rs` — a command bound to a *pair* of mouse
buttons pressed in sequence rather than to one button — is taken from
[hevel](https://git.sr.ht/~dlm/hevel), a Plan 9-style floating Wayland
compositor in which the mouse does everything. What is borrowed is the model and
its vocabulary: the button numbering (1 left, 2 wheel, 3 right), the chords
themselves (`1→3` draw a rectangle and open a window in it, `3→1` close the
window released over, `3→2` pan, `2→1` move, `2→3` resize, `1→2` left to the
user), the rule that the first press is held back until the second button
decides, and the name and the 250 ms of `chord_click_timeout_ms`, which is
`mouse_chord_timeout` here. No source was copied — hevel is C on neuswc,
Parallax is Rust on smithay — and the chords are off by default (see the
`mouse{}` section of `default_config.lua` for why).

hevel is under the ISC license. Its `LICENSE` file carries the template's
unfilled copyright line, and is quoted here as it stands:

> `Copyright (c) YYYY YOUR-NAME-HERE <user@your.dom.ain>`
>
> Permission to use, copy, modify, and distribute this software for any
> purpose with or without fee is hereby granted, provided that the above
> copyright notice and this permission notice appear in all copies.
>
> THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
> WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
> MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
> ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
> WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
> ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
> OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

### Compiz (idea, no code)

The desktop cube (`src/куб.rs`, `plx-extra` only) is Compiz's, and the settings
are named after what it did: faces, shading of the far edge, turning the cube on
a workspace switch. The geometry here is written from scratch against this
renderer, and the ring of faces is unbounded — Compiz gave the idea and the
expectations, not a line of code.

### niri, sway (ideas, no code)

The scrollable-strip layout (`src/columns.rs`) and parts of the overview follow
the model of [niri](https://github.com/YaLTeR/niri); a few input decisions
follow [sway](https://github.com/swaywm/sway). Nothing was ported from either —
the comments name them where the behaviour is deliberately theirs.

## Assets shipped with the compositor

### Nunito (SIL Open Font License 1.1)

`assets/Nunito-Regular.ttf` and `assets/Nunito-SemiBold.ttf`, compiled into the
binary by `src/text.rs`. Copyright 2014 The Nunito Project Authors
(https://github.com/googlefonts/nunito). Full license text:
[assets/Nunito-OFL.txt](assets/Nunito-OFL.txt).

### Notification tones (CC0 1.0)

`assets/sounds/notify-*.ogg`, from
[akx/Notifications](https://github.com/akx/Notifications), used under CC0 1.0
(public domain dedication). `notify-glass.ogg` is compiled into the binary by
`src/notify.rs`. Details in [assets/sounds/README.md](assets/sounds/README.md).

### Cover art

`assets/cover.jpg` and `assets/social-preview.png` were generated with Google
Gemini for this project; they contain no third-party material.

## Protocols

Wayland protocol definitions used by Parallax (`wlr-screencopy`,
`ext-image-copy-capture`, `xdg-shell`, `wlr-layer-shell` and the rest) come from
`wayland-protocols` and `wlr-protocols` through the `smithay` and
`wayland-protocols*` crates, under MIT.
