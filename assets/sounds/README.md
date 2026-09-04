# Notification sounds

Three short, deliberately quiet tones from [akx/Notifications](https://github.com/akx/Notifications)
— "a small collection of hand-crafted, subtle notification tones". The upstream
collection is dual-licensed **CC0 1.0 (public domain)** / CC-BY 3.0; parallax
uses it under CC0, so nothing here restricts the GPL-3.0 licence of the
compositor itself.

| file                 | upstream name | length | character                          |
|----------------------|---------------|--------|------------------------------------|
| `notify-glass.ogg`   | `Glass.ogg`   | 0.87 s | soft glassy chime — **the default** |
| `notify-cloud.ogg`   | `Cloud.ogg`   | 1.48 s | two-note, slightly warmer          |
| `notify-polite.ogg`  | `Polite.ogg`  | 0.48 s | three quiet wooden taps            |

`notify-glass.ogg` is compiled into the binary (`include_bytes!` in
`src/notify.rs`) so that a portable build needs no install step; the other two
are here to be pointed at from `config.lua`:

```lua
set{ notify_sound = "/path/to/parallax/assets/sounds/notify-cloud.ogg" }
set{ notify_volume = 0.35 }   -- 0.0 … 1.0
set{ notify_sound = "off" }   -- silence
```
