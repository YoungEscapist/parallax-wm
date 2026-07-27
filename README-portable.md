# dawn — сборка на другом ПК (без NixOS / без nix-shell)

Dawn — Wayland-компоситор на Rust (smithay, TTY/DRM backend).

## 1. Зависимости

Нужны Rust (cargo, желательно через rustup), pkg-config, C-компилятор и системные
`-dev` библиотеки. Точные имена пакетов и команды установки — в шапке
`build_portable.sh` (Debian/Ubuntu, Fedora, Arch).

Кратко нужны dev-версии: wayland, libxkbcommon, libinput, udev/systemd, seat
(libseat), libdrm, gbm, EGL, GLES, libdisplay-info. Для запуска — ещё `xwayland`.
Lua отдельно ставить не надо — `mlua` собирает Lua 5.4 из исходников.

## 2. Сборка

```sh
./build_portable.sh            # release
./build_portable.sh --verbose  # доп. аргументы уходят в cargo
```

Бинарь: `target/release/dawn`. Если в системе нет линкера `mold`, скрипт сам
откатится на системный `ld` (правило mold лежит в `.cargo/config.toml`).

Первая сборка тянет smithay с гита (rev зафиксирован в `Cargo.toml`) и много
крейтов — нужен интернет; `Cargo.lock` в комплекте, версии воспроизводимы.

## 3. Запуск

Только с **чистого TTY** (Ctrl+Alt+F3, вход в консоль без графического DE —
dawn забирает DRM master):

```sh
./launch_portable.sh           # release
./launch_portable.sh --debug   # debug-бинарь
```

Выход из компоситора — `Super+Shift+Q`. Логи — в `logs/`.

## 4. Конфиг

Клавиши/поведение настраиваются в Lua. Пример — `default_config.lua` в комплекте;
рабочий конфиг кладётся в `~/.config/dawn/config.lua`:

```sh
mkdir -p ~/.config/dawn
cp default_config.lua ~/.config/dawn/config.lua
```

Терминал по `Super+Return` задаётся строкой `spawn` в конфиге (по умолчанию в
этой сборке — `ghostty`; поменяй `cmd` на свой).

### Режим точек (закладки камеры)
- `Super+B` — вкл/выкл режим точек (вместо воркспейсов). Точки сохраняются.
- `Alt+B` — поставить точку на позиции курсора.
- `Super+1‑9` — прыжок к точке (в режиме точек).
- Точки видны крестиками на минимапе (`Super+` \` — показать минимапу).

## Примечания
- `launch.zsh` / `launch_tty.zsh` / `build.sh` в репозитории — NixOS-специфичные
  (хардкод путей `/nix/store`), на другом ПК используй `*_portable.sh`.
