# Notification sounds

The built-in notification tone is not a file: it is two soft mallet notes
(D5 → A5) synthesised in `src/звуки.rs`, next to the power-hold sounds, and
written to `$XDG_RUNTIME_DIR/plx-notify.wav` at startup.

The three files here are alternatives to point `notify_sound` at. They are
short, deliberately quiet tones from [akx/Notifications](https://github.com/akx/Notifications)
— "a small collection of hand-crafted, subtle notification tones". The upstream
collection is dual-licensed **CC0 1.0 (public domain)** / CC-BY 3.0; parallax
uses it under CC0, so nothing here restricts the GPL-3.0 licence of the
compositor itself.

| file                 | upstream name | length | character                                 |
|----------------------|---------------|--------|-------------------------------------------|
| `notify-glass.ogg`   | `Glass.ogg`   | 0.87 s | long glassy chime — the built-in one before 10.2026 |
| `notify-cloud.ogg`   | `Cloud.ogg`   | 1.48 s | two-note, slightly warmer                 |
| `notify-polite.ogg`  | `Polite.ogg`  | 0.48 s | three quiet wooden taps                   |

```lua
set{ notify_sound = "/path/to/parallax/assets/sounds/notify-polite.ogg" }
set{ notify_volume = 0.35 }   -- 0.0 … 1.0
set{ notify_sound = "off" }   -- silence
```
