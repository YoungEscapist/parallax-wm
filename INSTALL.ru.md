# Установка Parallax

*[English version](INSTALL.md)*

Parallax — композитор Wayland. Он забирает себе экран: ему нужен **чистый TTY**,
на котором не сидит другой графический сеанс, и он **бета, написанная
нейросетью** (см. предупреждение в [README.ru.md](README.ru.md)). Ставьте его на
машину, куда вы сможете попасть, если сеанс не поднимется, — другой TTY, SSH или
пункт входа, из которого можно выйти обратно.

Готовых пакетов пока нет: собирается из исходников, на обычном настольном
компьютере это несколько минут.

---

## Быстрая установка

Из клона:

```sh
git clone https://github.com/YoungEscapist/parallax-wm.git
cd parallax-wm
./install.sh
```

Или сразу, без клонирования, — скрипт сам склонирует дерево в
`~/.local/src/parallax-wm` и продолжит уже в нём:

```sh
curl -fsSL https://raw.githubusercontent.com/YoungEscapist/parallax-wm/master/install.sh | bash
```

Ключи в такой форме передаются через `bash -s --`, например
`… | bash -s -- --extra --quick`.

`install.sh` делает пять вещей, показывает каждую команду до того, как её
выполнить, и спрашивает перед всем, что требует root:

1. ставит системные -dev пакеты (Void, Arch, Debian/Ubuntu, Fedora);
2. проверяет Rust и предлагает [rustup](https://rustup.rs), если его нет;
3. собирает композитор;
4. кладёт `default_config.lua` в `~/.config/parallax/config.lua` (уже
   существующий конфиг не трогает);
5. регистрирует пункт **Parallax** в менеджере входа.

Ключи:

| | |
|---|---|
| `--extra` / `--both` | собрать `plx-extra` вместо / вместе с `plx-standard` — см. [Какая сборка](#какая-сборка) |
| `--quick` | профиль `quick`: тот же оптимизированный код с thin LTO, пересобирается в 8 раз быстрее и требует меньше памяти |
| `--native` | компилировать под ЭТОТ процессор (`-C target-cpu=native`); быстрее, но на другой машине такой бинарь падает с `SIGILL` |
| `--jobs N` | ограничить параллельность cargo на слабой машине |
| `--no-deps`, `--no-rust`, `--no-build`, `--no-config`, `--no-session` | пропустить шаг |
| `--update` | `git pull` и пересборка того, что уже стоит |
| `--uninstall` | убрать пункт сессии (`--purge` — ещё и конфиг) |
| `--dry-run` | показать, что было бы сделано, и ничего не делать |
| `-y` | не задавать вопросов |

Всё, что ниже, — то же самое руками.

---

## Что нужно

* **Linux с DRM/KMS.** Любая видеокарта с рабочим Mesa или проприетарной
  NVIDIA. Для разработки годится и вложенный запуск внутри чужого сеанса
  Wayland (бэкенд winit).
* **Rust**, свежий стабильный. Дерево разрабатывается на 1.98; `install.sh`
  ругается на всё старее 1.82 — smithay и часть крейтов хотят современный
  компилятор. Пакет дистрибутива тоже подходит.
* **Компилятор C и pkg-config** — часть зависимостей собирает код на C.
* **Память и диск.** Релизный профиль идёт с fat LTO: линковка в один процесс,
  которой нужно несколько гигабайт (`install.sh` предупреждает, если памяти
  меньше 6 ГиБ). Профиль `--quick` линкует в несколько потоков и требует
  заметно меньше. Каталог сборки разрастается до нескольких гигабайт.
* Во время работы: `Xwayland` (для приложений X11) и сессионная шина D-Bus —
  через неё идут трей, портал демонстрации экрана и звук уведомлений.

### Зависимости

-dev пакеты для: wayland, libxkbcommon, libinput, udev, libseat, libdrm, gbm,
EGL, GLES, libdisplay-info, PipeWire (вместе с libspa — через него работает
портал демонстрации экрана) и pixman (рендерер smithay линкуется с
`-lpixman-1`).

Lua ставить **не нужно**: `mlua` собирает Lua 5.4 из исходников.

```sh
./dist/install-deps.sh --print   # показать команду для вашего дистрибутива
sudo ./dist/install-deps.sh      # выполнить её
```

<details>
<summary>Списки пакетов, если хочется набрать руками</summary>

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
  systemd-libs seatd libdrm mesa libdisplay-info pipewire pixman xorg-xwayland
```

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

**NixOS** — `nix-shell` в дереве, дальше `./build.sh`. Помните про
предупреждение в шапке `build.sh`: бинарь, слинкованный с glibc из Nix, падает
на обычной системе, и наоборот.

</details>

---

## Какая сборка

Parallax — это два бинаря из одного крейта с разным набором фич. Второго дерева
исходников нет: то, что фича выключает, подменяется заглушкой той же формы, и
вызывающий код в обеих сборках одинаков.

| | `plx-standard` | `plx-extra` |
|---|---|---|
| композитор, тайлинг, лента, обзор, обои | да | да |
| панель, трей, блютуз, вайфай, звук, портал, снимок, X11, жесты | да | да |
| шлем (VR) | — | да |
| окна внутри Minecraft | — | да |
| показ рабочего стола гостям | — | да |
| свечение окон, свет на холсте, куб столов | — | да |

`plx-standard` стоит по умолчанию и подходит большинству: это сборка, где
каждый пиксель идёт к экрану самым коротким путём. Всё необязательное в
`plx-extra` и так выключено по умолчанию — включается в конфиге.

Передумать можно потом: собрать второй бинарь и перезапустить сеанс.

---

## Сборка руками

```sh
./build_portable.sh              # оба бинаря, релизный профиль
./build_portable.sh --quick      # thin LTO: пересборка в 8 раз быстрее, памяти меньше
PLX_NATIVE=1 ./build_portable.sh # код под этот процессор (непереносимый бинарь)
```

Бинари ложатся в `target/standard/<профиль>/plx-standard` и
`target/extra/<профиль>/plx-extra`, а копии — в `target/release/`, где их ищет
`launch_native.sh`.

Один бинарь отдельно:

```sh
cargo build --release -p plx-standard
```

> **Не собирайте оба одной командой `--workspace`.** Cargo объединяет наборы
> фич членов workspace, и получаются два одинаковых бинаря, в каждом из которых
> лежит всё. Замерено: по 17.8 МиБ байт в байт, с openxr внутри «стандартного».
> Ровно поэтому сборочные скрипты зовут cargo дважды и в разные каталоги.

Первая сборка тянет smithay из git (ревизия закреплена) и много крейтов — нужна
сеть. `Cargo.lock` в репозитории есть.

**Профили.** `release` — fat LTO и одна кодогенерирующая единица: самый быстрый
кадр и около пяти минут на пересборку после правки одной строки (16 потоков).
`quick` — тот же оптимизированный код с thin LTO и 16 единицами: те же правки
пересобираются за 36 секунд ценой единиц процентов на кадре. Им удобно
пользоваться, пока правишь, и на машине, где fat LTO не влезает в память.

В `.cargo/config.toml` намеренно нет ничего машинного. Сборочные скрипты сами
подхватывают `mold` или `lld`, если они есть, а `-C target-cpu=native`
включается только явно (`--native` / `PLX_NATIVE=1`) — собранный так бинарь
падает с `SIGILL` на другом процессоре.

---

## Запуск

Нужен **чистый TTY** — Ctrl+Alt+F3, войти, и там:

```sh
cd ~/.local/src/parallax-wm
./launch_native.sh
```

`launch_native.sh` поднимает сессионную шину, если её нет, запускает демон
обоев, пишет логи в `logs/` и пересобирает бинарь, если исходники новее.

Выход — **Super+Shift+Q**, перезапуск на месте — **Super+R**.

Посмотреть, не уходя из своего рабочего стола, — вложенным окном:

```sh
./launch_native.sh --winit
```

### Из менеджера входа

```sh
sudo ./dist/install-session.sh          # --uninstall убирает обратно
```

Ставятся ровно два файла — `/usr/local/bin/parallax-session` и
`/usr/share/wayland-sessions/parallax.desktop`, — и больше ничего. Бинарь
остаётся в дереве, куда его положила сборка, а обёртка идёт к нему через
`launch_native.sh`: поэтому пересборка и `Super+R` всегда попадают в то же
место, а не в устаревшую копию под `/usr/local/bin`.

Работает с ly, greetd, SDDM и GDM. Путь к дереву перебивается `PLX_CHECKOUT`,
каталоги установки — `BIN_DIR` и `SESSIONS_DIR`, сам бинарь — `PLX_BINARY`
(последнее нужно пакетам дистрибутивов, где дерева исходников нет вовсе).

---

## Конфиг

```sh
mkdir -p ~/.config/parallax
cp default_config.ru.lua ~/.config/parallax/config.lua
```

Файл на Lua и перечитывается при сохранении — перезапуск не нужен. Каждый ключ
описан прямо в нём; обзор возможностей — в README. `install.sh` при русской
локали кладёт русский вариант сам.

Спутники ставятся отдельно и необязательны:
[plx-wall](https://github.com/mifaroslav-dotcom/plx-wall) — живые обои и
палитра, в цвет которой красится рабочий стол; `plx-share` / `plx-host` — показ
своего стола гостю.

---

## Обновление

```sh
cd ~/.local/src/parallax-wm
./install.sh --update
```

или руками: `git pull && ./build_portable.sh`. Если сеанс идёт, **Super+R**
перезапускает его на месте — сначала пересобирает, если исходники новее бинаря,
и сохраняет окна.

## Удаление

```sh
./install.sh --uninstall     # пункт сессии
./install.sh --purge         # пункт сессии и ~/.config/parallax
rm -rf ~/.local/src/parallax-wm
```

Ничего другого за пределами дерева не пишется, кроме `~/.local/bin/plx-host`,
если собиралась `plx-extra`.

---

## Если не работает

**Сборка падает на отсутствующем `.pc` / ошибке pkg-config.** Не хватает -dev
пакета: полный список для вашего дистрибутива печатает
`./dist/install-deps.sh --print`. Чаще всего это `libspa-0.2-dev`,
`libpipewire-0.3-dev` и `libpixman-1-dev` — их не тянет за собой ничто другое, а
падение происходит внутри чужого build.rs, где причина не видна.

**Сборку убивает OOM или машина замирает под конец.** Это fat LTO, линкующий в
один процесс. Соберите с `--quick` или `--jobs 4`.

**`cannot find -fuse-ld=mold`.** Старый `.cargo/config.toml` (до этой правки)
или свой `RUSTFLAGS`. Нынешнее дерево не просит линкера, которого не нашло в
системе.

**Композитор стартует и сразу выходит, или экран моргает и возвращается.**
DRM master держит кто-то другой — второй композитор, X11 или менеджер входа на
этом VT. Нужен TTY, на котором не запущено ничего графического.

**«Permission denied» на `/dev/dri/card0` или на устройстве ввода.** Нужен
менеджер мест: `seatd` (добавьте себя в группу `_seatd`/`seat` и включите
службу) либо elogind/systemd-logind. Parallax ходит через libseat и setuid ему
не нужен.

**Запуск из менеджера входа сразу завершается.** Смотрите лог менеджера входа:
`parallax-session` пишет, что он пробовал. Две обычные причины — переехавшее
дерево исходников (переустановите файл сессии) и отсутствующий
`XDG_RUNTIME_DIR` на системе без elogind (обёртка создаёт его сама, но каталог
должен быть доступен на запись).

**Нет трея, демонстрации экрана и звука уведомлений.** Нет сессионной шины.
`launch_native.sh` и `parallax-session` поднимают её через `dbus-run-session`,
если могут, — поставьте `dbus`, если в логе написано, что не смогли.

**Работает, но всё медленно, или падает с `SIGILL`.** Это бинарь, собранный с
`--native`/`PLX_NATIVE=1` на другой машине. Пересоберите без них.

Логи лежат в `logs/` внутри дерева (`launch_native.sh`) или в логе менеджера
входа, если запуск шёл оттуда. `RUST_LOG=parallax=debug` делает их подробными.
Об ошибках: <https://github.com/YoungEscapist/parallax-wm/issues>.
