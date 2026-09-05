#!/usr/bin/env bash
# launch_native.sh — запуск parallax напрямую, БЕЗ nix-shell.
#
# Бинарь собирается через build.sh ЧИСТО системным тулчейном (cargo/rustc/
# mold из xbps, без nix-shell) — все зависимости (libinput, libseat, libdrm,
# mesa/gbm, libdisplay-info, libxkbcommon, ...) резолвятся из /usr/lib через
# системный ld.so, RUNPATH пустой. LD_LIBRARY_PATH не нужен.
#
# ВАЖНО: собранный через nix-shell бинарь падает с general protection fault
# в libc.so.6 при каждом запуске (несовместимость системного ld.so с
# Nix-glibc, подтянутой транзитивно через RUNPATH нижних библиотек) — см.
# комментарий в build.sh. Если после пересборки снова начались segfault'ы
# без единой строчки в логе — проверьте, что сборка НЕ шла через nix-shell,
# а через build.sh (машина автора) или build_portable.sh (любая другая).
#
# Использование: ./launch_native.sh [--debug] [--winit]
set -euo pipefail

PLX_DIR="$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)"

# Каталог сборки. На машине автора это /mnt/plx-build/target — туда пишет
# build.sh. У всех остальных бинарь лежит в `target/` самого чекаута, потому
# что README ведёт именно этим путём: `./build_portable.sh`, потом
# `./launch_native.sh`. Пока каталог был захардкожен, вторая команда у
# постороннего падала с «Бинарь не найден» на пути, которого у него нет вовсе.
# Порядок: явный PLX_BUILD_DIR, потом авторский — если там ДЕЙСТВИТЕЛЬНО
# лежит бинарь, — потом свой.
BUILD_DIR="${PLX_BUILD_DIR:-}"
if [[ -z "$BUILD_DIR" ]]; then
    if [[ -x /mnt/plx-build/target/extra/release/plx-extra ]]; then
        BUILD_DIR="/mnt/plx-build/target"
    else
        BUILD_DIR="$PLX_DIR/target"
    fi
fi
# Сеанс идёт на ПОЛНОЙ сборке: шлем, окна в Minecraft и мультиюзер нужны
# именно здесь. Минимальная лежит рядом — plx-standard, тот же композитор без них.
#
# Каталог — ИМЕННО `extra/release`, тот, куда пишет build.sh. Пока здесь стоял
# `release/plx-extra`, Super+R работал вхолостую: build.sh с разделением на две
# сборки кладёт бинари в `extra/` и `standard/`, а запускался остаток от прежней
# общей сборки одним `--workspace` (в `release/` лежат оба бинаря с одним
# временем — по ним это и видно). Пересборка проходила, `свежие_исходники`
# продолжали видеть исходники новее ЗАПУСКАЕМОГО файла, и сессия поднималась на
# старом бинаре с руганью «Сборка отчиталась об успехе, но бинарь не обновился».
# Поймано 03.09.2026 при разборе расхода процессора.
#
# Полная сборка предпочтительнее, но сеанс поднимется и на стандартной: у
# того, кто собрал только `-p plx-standard`, ровно тот же композитор без шлема,
# Minecraft и мультиюзера, и отказываться его запускать не за что.
BINARY="$BUILD_DIR/extra/release/plx-extra"
# Имя переменной ЛАТИНИЦЕЙ: кириллическое bash за идентификатор не считает и
# падает на `not a valid identifier` — та же грабля, что с chown в build.sh.
for cand in \
    "$BUILD_DIR/extra/release/plx-extra" \
    "$BUILD_DIR/release/plx-extra" \
    "$BUILD_DIR/standard/release/plx-standard" \
    "$BUILD_DIR/release/plx-standard"; do
    if [[ -x "$cand" ]]; then BINARY="$cand"; break; fi
done
# Отладочный бинарь лежит там же, где и релизный, только в `debug/`. Пока здесь
# стояло `$BUILD_DIR/debug/parallax`, `--debug` не работал вовсе: бинаря с
# именем `parallax` не существует с тех пор, как сборка разделилась на
# plx-standard и plx-extra.
[[ "${1:-}" == --debug ]] && BINARY="$BUILD_DIR/extra/debug/plx-extra"

DWALL_BIN="$HOME/.local/bin/plx-wall"

if [[ ! -x "$BINARY" ]]; then
    echo "Бинарь не найден: $BINARY" >&2
    if [[ "$BUILD_DIR" == "$PLX_DIR/target" ]]; then
        echo "Сборка: cd $PLX_DIR && ./build_portable.sh" >&2
    else
        echo "Сборка: cd $PLX_DIR && ./build.sh" >&2
    fi
    exit 1
fi

if ldd "$BINARY" 2>&1 | grep -q "not found"; then
    echo "Не хватает системных библиотек (нужны -dev пакеты, см. build.sh):" >&2
    ldd "$BINARY" | grep "not found" >&2
    exit 1
fi

# ── NixOS: библиотеки, которые грузятся через dlopen ─────────────────────────
# Проверка ldd выше видит только DT_NEEDED. А libwayland-client (winit-режим)
# и libEGL/libGLESv2 (udev/DRM) подтягиваются dlopen'ом ПО SONAME, в ldd их
# нет вовсе, и RUNPATH у бинаря пустой. На Void они лежат в /usr/lib и
# находятся сами — там LD_LIBRARY_PATH действительно не нужен (см. шапку).
# На NixOS каталога /usr/lib нет: без LD_LIBRARY_PATH winit падает сразу с
# EventLoopCreation(... NoWaylandLib), а tty-режим — на инициализации EGL.
#
# Пути берём из shell.nix, а не хардкодом: после nix-collect-garbage хеши
# в /nix/store меняются, и любой зафиксированный путь протухает.
if [[ -e /etc/NIXOS && -z "${LD_LIBRARY_PATH:-}" && -f "$PLX_DIR/shell.nix" ]]; then
    echo "NixOS: беру LD_LIBRARY_PATH из shell.nix (dlopen-библиотеки)…"
    LD_LIBRARY_PATH="$(nix-shell "$PLX_DIR/shell.nix" --run 'printf %s "$LD_LIBRARY_PATH"')"
    export LD_LIBRARY_PATH
fi

# ── Приветствие zsh в терминалах сессии ──────────────────────────────────────
# .zshrc показывает fastfetch один раз на цепочку шеллов и помечает это
# экспортом __FASTFETCH_SHOWN. Но parallax запускают ИЗ логин-шелла tty, который
# приветствие уже показал, — и флаг наследовался дальше: в parallax, в каждый
# ghostty, в каждый zsh внутри него. Итог: приветствия не было НИ В ОДНОМ
# терминале сессии. Сессия компоновщика — новый контекст, флаг предыдущего
# шелла к ней отношения не имеет.
unset __FASTFETCH_SHOWN

LOG_DIR="$PLX_DIR/logs"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/plx_native_$(date +%Y%m%d_%H%M%S).log"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

# ── Сессионная шина D-Bus ────────────────────────────────────────────────────
# Без неё в сессии нет ни портала (демонстрация экрана, выбор файлов), ни
# уведомлений: xdg-desktop-portal живёт ТОЛЬКО на сессионной шине. Замер
# 03.08.2026: у parallax не было DBUS_SESSION_BUS_ADDRESS вовсе, /run/user/1000/bus
# не существовал, а единственный запущенный xdg-desktop-portal висел в чужой
# сессии (XDG_RUNTIME_DIR=/tmp/runtime-1000, шина /tmp/dbus-*) — приложения из
# parallax его не видели.
#
# Перезапускаем сам скрипт под dbus-run-session: она поднимает шину, ставит
# DBUS_SESSION_BUS_ADDRESS и умирает вместе с сессией.
if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && -z "${PLX_NO_DBUS:-}" ]]; then
    if [[ -S "$XDG_RUNTIME_DIR/bus" ]]; then
        export DBUS_SESSION_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/bus"
    elif command -v dbus-run-session >/dev/null; then
        echo "D-Bus: сессионной шины нет — поднимаю через dbus-run-session"
        exec dbus-run-session -- "$0" "$@"
    else
        echo "⚠ dbus-run-session не найден: портал и уведомления работать не будут" >&2
    fi
fi

# ── PipeWire ─────────────────────────────────────────────────────────────────
# Демонстрация экрана идёт потоком через PipeWire; без него портал ScreenCast
# соглашается на сессию, но кадры отдать не может. Стартуем только если ещё не
# запущен (в сессии он один на пользователя) и глушим вместе с parallax.
#
# Живость проверяем ПО СОКЕТАМ, а не по именам процессов. `pipewire-pulse` —
# это тот же бинарь, запущенный как `pipewire -c pipewire-pulse.conf`, и comm у
# него тоже "pipewire". Осиротевший мост от прошлой сессии (parallax убили, мост
# пережил) ловился прежним `pgrep -x pipewire`, скрипт считал ядро поднятым и
# не запускал ни его, ни wireplumber. В итоге сессия оставалась совсем без
# звука: сокета pipewire-0 нет, устройств нет, `pactl` отвечает Connection
# refused, меню звука parallax пишет «no output device» (разобрано 05.08.2026).
#
# Где аудио держит systemd (NixOS с services.pipewire), руками не лезем ВООБЩЕ:
# юниты стартуют параллельно с этим скриптом, и проверка «жив ли» неизбежно
# гоняется с ними. У pipewire/pipewire-pulse гонку гасят сокеты (systemd делает
# их заранее, socket activation), а у wireplumber сокета нет — и `pgrep -x` ниже
# в одну и ту же секунду не видел ещё не стартовавший юнит и поднимал ВТОРОЙ
# экземпляр. Два сессионных менеджера на одном ядре дерутся за устройства, граф
# встаёт колом: `pw-dump` и `wpctl status` висят по таймауту, аудио-узлов нет
# ни одного, `pactl` отвечает Timeout, и меню звука parallax снова пустое —
# ровно тот же симптом, что и от осиротевшего моста, но причина обратная
# (разобрано 15.08.2026: PID 974 от скрипта против PID 981 от systemd).
#
# `systemctl --user start` идемпотентен и ЖДЁТ готовности юнитов, поэтому он
# и гонку снимает, и заменяет собой весь ручной путь. Ручной путь остаётся для
# машин без systemd (портативная сборка, Void) — там юнитов просто нет.
PIPEWIRE_PIDS=()
if [[ -z "${PLX_NO_PIPEWIRE:-}" ]] \
   && command -v systemctl >/dev/null \
   && systemctl --user cat wireplumber.service >/dev/null 2>&1; then
    echo "PipeWire: аудио под systemd — поднимаю юнитами, руками не трогаю"
    systemctl --user start pipewire.service wireplumber.service pipewire-pulse.service \
        >/dev/null 2>&1 || true
elif [[ -z "${PLX_NO_PIPEWIRE:-}" ]]; then
    # Ядра нет — значит всё, что осталось от прошлой сессии, мёртвый груз, и
    # к тому же держит сокет pulse, куда иначе не встанет новый мост.
    if [[ ! -S "$XDG_RUNTIME_DIR/pipewire-0" ]]; then
        pkill -x -u "$(id -u)" pipewire    >/dev/null 2>&1 || true
        pkill -x -u "$(id -u)" wireplumber >/dev/null 2>&1 || true
        sleep 0.5
    fi
    # $1 — бинарь, $2 — сокет-признак того, что он уже работает.
    plx_start_audio() {
        command -v "$1" >/dev/null || return 0
        [[ -S "$2" ]] && return 0
        "$1" >/dev/null 2>&1 &
        PIPEWIRE_PIDS+=("$!")
        sleep 0.5
    }
    plx_start_audio pipewire "$XDG_RUNTIME_DIR/pipewire-0"
    # wireplumber — сессионный менеджер: без него ядро живо, но НИ ОДНОГО
    # устройства не создаёт. Своего сокета у него нет, зато имя уникальное,
    # так что здесь pgrep как раз уместен.
    if command -v wireplumber >/dev/null \
       && ! pgrep -x -u "$(id -u)" wireplumber >/dev/null; then
        wireplumber >/dev/null 2>&1 &
        PIPEWIRE_PIDS+=("$!")
        sleep 0.5
    fi
    plx_start_audio pipewire-pulse "$XDG_RUNTIME_DIR/pulse/native"
fi
# XCURSOR_PATH нарочно НЕ задаём: если он установлен, крейт xcursor
# полностью игнорирует стандартные XDG-пути (~/.icons, /usr/share/icons)
# и ищет только там — а /run/current-system (унаследовано из NixOS-версии
# этого скрипта) на этой системе не существует, из-за чего курсор был
# невидим вне окон. Без XCURSOR_PATH crate сам находит /usr/share/icons.
# Курсор: Bibata-Modern-Ice (белая, /usr/share/icons/Bibata-Modern-Ice).
# Обе переменные читают и клиенты (GTK/Qt/wayland-cursor), и сам компоновщик
# (см. load_default_cursor), так что стрелка везде одна. Размер дублируется
# в config.lua через set{ cursor_size }: env читают клиенты, set — компоновщик.
export XCURSOR_THEME="${XCURSOR_THEME:-Bibata-Modern-Ice}"
export XCURSOR_SIZE="${XCURSOR_SIZE:-24}"
# debug по умолчанию: диагностические строки (PTR HIT, PTR КАДР, PTR ЛОКАЛЬ,
# КУРСОР КЛИЕНТА) пишутся в файл лога, а не на экран, зато после жалобы
# «кликаю не туда» данные уже собраны — переспрашивать и повторять не надо.
export RUST_LOG="${RUST_LOG:-parallax=debug,info}"
# Протокольный лог Xwayland по метке-файлу: `touch ~/.plx_wldebug` + перезапуск
# (Super+R). WAYLAND_DEBUG наследует ИМЕННО Xwayland — сам parallax сервер, его
# libwayland-client не касается. Нужен, когда картинка курсора у X-сервера одна,
# а на экране другая: в логе видно, шлёт ли Xwayland set_cursor вообще и с каким
# серийником (см. dawn-x11-cursor-stuck в памяти). Лог растёт быстро — метку
# снимать сразу после замера.
if [ -f "$HOME/.plx_wldebug" ]; then
    export WAYLAND_DEBUG=1
fi
# По этому имени xdg-desktop-portal выбирает бэкенд (см. launch_tty.zsh).
#
# Именно "=", а НЕ ":-", и ровно по той же причине, что у XDG_SESSION_TYPE
# ниже: значение сюда приходит ИЗВНЕ и приходит неверное. Менеджер входа берёт
# его из `DesktopNames=` в .desktop-файле сессии, а у поставленного на машину
# файла там осталось `dawn` — имя до переименования проекта. Дальше всё
# сходится одно к одному и молча:
#   · фронтенд читает `dawn-portals.conf`, а тот зовёт бэкенд `dawn`;
#   · `dawn.portal` объявляет имя `org.freedesktop.impl.portal.desktop.dawn`;
#   · композитор при этом занимает на шине `…desktop.parallax` (portal.rs,
#     BUS_NAME) и пишет `parallax.portal` с `UseIn=parallax`.
# Имя, которого никто не занял, не активируется, ScreenCast отвечает отказом —
# и снаружи это ровно «OBS не видит экран», без единой строки в логе.
export XDG_CURRENT_DESKTOP=parallax
# Именно "=", а НЕ ":-": elogind заводит сессию с tty и выставляет
# XDG_SESSION_TYPE=tty ещё до нас, поэтому значение по умолчанию не
# срабатывало никогда. Цена ошибки — вся демонстрация экрана: libwebrtc
# внутри Chromium/Electron считает сессию вэйландовой ТОЛЬКО по этой
# переменной (IsRunningUnderWayland: XDG_SESSION_TYPE=wayland И
# WAYLAND_DISPLAY). Со значением tty он берёт X11-захват через DISPLAY=:0,
# то есть корневое окно XWayland — а оно у rootless-сервера пустое:
# Discord показывал ЧЁРНЫЙ экран с одним курсором, и портал parallax при этом
# не получал ни одного вызова.
export XDG_SESSION_TYPE=wayland
export NIXOS_OZONE_WL="${NIXOS_OZONE_WL:-1}"

# Занести окружение в ШИНУ. Она поднята выше (dbus-run-session) и успела снять
# значения, которые были ДО двух правок над этой строкой. xdg-desktop-portal
# запускается активацией по имени и наследует окружение шины, а не наше, —
# без этого вызова обе правки чинили бы окружение всему, кроме того
# единственного, ради кого они сделаны.
#
# Переменные перечислены поимённо и только те, что к этой строке ТОЧНО
# выставлены: `dbus-update-activation-environment` на незаданном имени ругается
# и возвращает ошибку, а WAYLAND_DISPLAY здесь ещё неоткуда взяться —
# компоновщик не запущен. Без `--systemd`: на Void его нет, и флаг превратил бы
# вызов в отказ целиком.
if command -v dbus-update-activation-environment >/dev/null; then
    dbus-update-activation-environment \
        XDG_CURRENT_DESKTOP XDG_SESSION_TYPE XDG_RUNTIME_DIR >/dev/null 2>&1 || true
fi

# plx-wall (обои) запускается ПОСЛЕ parallax — см. ниже, после старта компоновщика.
DWALL_PID=""

MODE_ARGS=()
[[ "${1:-}" == --winit || "${2:-}" == --winit ]] && MODE_ARGS+=(--winit)

# ── Перезапуск по Super+R ────────────────────────────────────────────────────
# parallax с этим кодом выходит по действию "restart" (см. state::RESTART_EXIT_CODE):
# сессия сохранена, компоновщик ушёл — а логин в ly остался, и мы поднимаем его
# заново прямо здесь. Так свежая сборка забирается без перелогина.
#
# Клиенты перезапуск НЕ переживают: вэйландовый сокет умирает вместе с
# компоновщиком, окна закрываются. Экономится именно вход в систему.
RESTART_CODE=42

# Уборка живёт до цикла: её зовёт и каждая итерация, и trap на выходе.
# plx-wall переживал смерть parallax и потом крутил ядро на 80% в цикле
# переподключения к мёртвому сокету — добиваем обоих вместе с компоновщиком.
cleanup() {
    [[ -n "${WATCHDOG_PID:-}" ]] && kill "$WATCHDOG_PID" 2>/dev/null || true
    [[ -n "${DWALL_PID:-}" ]] && kill -- -"$DWALL_PID" 2>/dev/null || kill "$DWALL_PID" 2>/dev/null || true
    pkill -f "$DWALL_BIN" 2>/dev/null || true
    # Сторожа буфера тоже уводим с собой: без композитора им не за чем следить,
    # а переживший сессию wl-paste цеплялся бы к мёртвому сокету.
    [[ -n "${CLIPHIST_PID:-}" ]] && kill "$CLIPHIST_PID" 2>/dev/null || true
    pkill -f "wl-paste --type text --watch cliphist" 2>/dev/null || true
    pkill -f "wl-paste --type image --watch cliphist" 2>/dev/null || true
    # PipeWire гасим только тот, что подняли САМИ: если он уже работал до нас,
    # он общий для всей пользовательской сессии и не наш, чтобы его убивать.
    # Переживает перезапуск: между итерациями цикла его гасить незачем — новый
    # parallax найдёт готовый звук (см. plx_start_audio выше).
    if [[ "${1:-}" != --keep-audio ]]; then
        for pid in "${PIPEWIRE_PIDS[@]:-}"; do
            [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
        done
    fi
}
trap cleanup EXIT INT TERM

# Пересборка перед перезапуском — только если исходники новее бинаря: когда
# менять нечего, Super+R это просто мгновенный перезапуск.
свежие_исходники() {
    find "$PLX_DIR/src" "$PLX_DIR/Cargo.toml" "$PLX_DIR/default_config.lua" \
         -newer "$BINARY" -print -quit 2>/dev/null || true
}

# Непустой — путь к логу упавшей пересборки; о нём сообщаем уже в сессии.
REBUILD_FAILED=""

rebuild_if_stale() {
    [[ -n "$(свежие_исходники)" ]] || return 0
    echo ""
    echo "── Исходники новее бинаря — пересобираю (Super+R) ──"
    local ok=0
    local build_log="$LOG_DIR/rebuild_$(date +%Y%m%d_%H%M%S).log"
    set -o pipefail
    if [[ "$BINARY" == */debug/plx-extra ]]; then
        # `-p plx-extra` обязателен: корневой пакет — это только [lib], и
        # голый `cargo build` не собрал бы ни одного исполняемого файла.
        (cd "$PLX_DIR" && cargo build -p plx-extra --target-dir "$BUILD_DIR/extra") \
            2>&1 | tee "$build_log" || ok=1
    elif [[ "$BUILD_DIR" == "$PLX_DIR/target" ]]; then
        # Собираем тем же скриптом, каким собран запускаемый бинарь: build.sh
        # пишет в /mnt/plx-build и на чужой машине просто не сможет.
        "$PLX_DIR/build_portable.sh" 2>&1 | tee "$build_log" || ok=1
    else
        "$PLX_DIR/build.sh" 2>&1 | tee "$build_log" || ok=1
    fi
    set +o pipefail
    # Мало нулевого кода возврата: сборка может пройти и НЕ обновить бинарь —
    # так было 23.08.2026, когда cargo от yarik уткнулся в root'овые артефакты
    # в общем каталоге сборки. Проверяем не код, а факт: бинарь обязан стать
    # новее исходников.
    if ! (( ok )) && [[ -n "$(свежие_исходники)" ]]; then
        ok=1
        echo "⚠ Сборка отчиталась об успехе, но бинарь не обновился." | tee -a "$build_log" >&2
    fi
    if (( ok )); then
        # Не фатально: поднимаем прежний бинарь, иначе нажатие Super+R на
        # сломанном коде оставило бы человека без сессии вообще.
        REBUILD_FAILED="$build_log"
        echo "⚠ Сборка не прошла — поднимаю ПРЕЖНИЙ бинарь. Лог: $build_log" >&2
        sleep 5
    else
        rm -f "$build_log"
    fi
}

while :; do
    LOG="$LOG_DIR/plx_native_$(date +%Y%m%d_%H%M%S).log"
    DWALL_PID=""
    CLIPHIST_PID=""
    WATCHDOG_PID=""

    echo "Бинарь:  $BINARY"
    echo "plx-wall:   $DWALL_BIN"
    echo "Лог:     $LOG"
    echo ""

    "$BINARY" "${MODE_ARGS[@]}" > >(tee "$LOG") 2>&1 &
    PLX_PID=$!

    # ── Сторожевой таймер старта ─────────────────────────────────────────────────
    # parallax умеет намертво залипать на ПЕРВОМ кадре (futex-дедлок в одном потоке,
    # 0% CPU, ни строчки в лог — см. логи 115942/121144). К этому моменту он уже
    # забрал у seat'а клавиатуру с мышью и держит DRM master, так что машина
    # становится недоступна целиком: ни ввода, ни переключения VT, только жёсткий
    # ресет или вход по сети. Сторож ждёт START_TIMEOUT секунд признака жизни
    # (любая строка после «initial render») и, не дождавшись, убивает parallax — VT
    # возвращается в шелл сам. Порог задаётся PLX_START_TIMEOUT.
    # ── plx-wall (обои и меню выбора) ───────────────────────────────────────────────
    # Ждём не САМ ФАЙЛ сокета, а строчку о нём в логе ЭТОГО запуска. Разница
    # принципиальная: файл сокета остаётся на диске от прошлой сессии (parallax его не
    # всегда прибирает, если его убили), проверка на -S проходила мгновенно, и
    # plx-wall стартовал в пустоту — до того, как новый компоновщик вообще начал
    # слушать. Он падал, перезапускался по кругу, и нажатие Win+W в этот момент
    # уходило в никуда: pkill не находил процесса.
    #
    # Дальше держим plx-wall под присмотром, пока жив parallax: если он умрёт (или его
    # уронит смена обоев), поднимаем заново, а не оставляем сессию без обоев.
    if [[ -x "$DWALL_BIN" ]]; then
        (
            for _ in $(seq 1 100); do
                grep -q "parallax socket:" "$LOG" 2>/dev/null && break
                sleep 0.1
            done
            grep -q "parallax socket:" "$LOG" 2>/dev/null || exit 0
            export WAYLAND_DISPLAY=wayland-1
            # DISPLAY, унаследованный от логин-шелла (например от параллельной
            # X-сессии на другом VT), уводил GTK-диалоги plx-wall в ЧУЖОЙ X-сервер:
            # проводник по плюсику открывался там и на экране parallax не появлялся
            # вовсе. В wayland-сессии его быть не должно.
            unset DISPLAY
            while kill -0 "$PLX_PID" 2>/dev/null; do
                # || true обязателен: скрипт под set -e, а упавший (или убитый)
                # plx-wall возвращает ненулевой код — без этого умирал сам сторож,
                # и обои больше не поднимались до конца сессии.
                "$DWALL_BIN" >/dev/null 2>&1 || true
                sleep 0.5
            done
        ) &
        DWALL_PID=$!
    else
        echo "plx-wall не найден по пути $DWALL_BIN — обои не запустятся" >&2
    fi

    # ── История буфера обмена (Super+C) ─────────────────────────────────────────
    # cliphist сам ничего не слушает: за буфером следит wl-paste и складывает
    # каждое копирование в базу (~/.cache/cliphist/db). Два сторожа, а не один:
    # `wl-paste --watch` без --type берёт только ПРЕДПОЧТИТЕЛЬНЫЙ тип, и снимок
    # экрана (image/png) мимо текстового сторожа прошёл бы незамеченным — а он тут
    # главный жилец (PrtScr кладёт скрин ровно сюда).
    #
    # Ждём ту же строчку сокета, что и plx-wall: до неё композитора ещё нет, и
    # wl-paste просто вышел бы с ошибкой.
    if command -v cliphist >/dev/null 2>&1 && command -v wl-paste >/dev/null 2>&1; then
        (
            for _ in $(seq 1 100); do
                grep -q "parallax socket:" "$LOG" 2>/dev/null && break
                sleep 0.1
            done
            grep -q "parallax socket:" "$LOG" 2>/dev/null || exit 0
            export WAYLAND_DISPLAY=wayland-1
            unset DISPLAY
            wl-paste --type text --watch cliphist store >/dev/null 2>&1 &
            wl-paste --type image --watch cliphist store >/dev/null 2>&1 &
            wait
        ) &
        CLIPHIST_PID=$!
    fi

    # ── Провал пересборки не должен быть безмолвным ─────────────────────────────
    # Сообщение rebuild_if_stale уходит на консоль VT, а её через секунду
    # закрывает собой сам компоновщик. Снаружи это выглядит как «Super+R ничего
    # не изменил»: 23.08.2026 сессия так три часа проработала на бинаре, снятом
    # ДО правок панели, и промах нашёлся только по времени файла. Поэтому след
    # остаётся там, где его точно увидят: в логе сессии и всплывашкой.
    if [[ -n "$REBUILD_FAILED" ]]; then
        (
            for _ in $(seq 1 100); do
                grep -q "parallax socket:" "$LOG" 2>/dev/null && break
                sleep 0.1
            done
            echo "⚠ ПЕРЕСБОРКА НЕ ПРОШЛА: сессия идёт на ПРЕЖНЕМ бинаре. Лог сборки: $REBUILD_FAILED" >> "$LOG"
            unset DISPLAY
            # Всплывашка — по возможности: демона уведомлений может и не быть.
            notify-send -u critical "plx: пересборка не прошла" \
                "Сессия на прежнем бинаре. Лог: $REBUILD_FAILED" >/dev/null 2>&1 || true
        ) &
        REBUILD_FAILED=""
    fi

    START_TIMEOUT="${PLX_START_TIMEOUT:-25}"
    (
        for _ in $(seq 1 "$START_TIMEOUT"); do
            sleep 1
            kill -0 "$PLX_PID" 2>/dev/null || exit 0   # сам завершился — сторож не нужен
            last=$(grep -v '^[[:space:]]*$' "$LOG" 2>/dev/null | tail -1 || true)
            case "$last" in
                *"initial render"*) ;;                   # всё ещё на первом кадре
                "") ;;                                   # лог пуст — рано судить
                *) exit 0 ;;                             # жизнь есть, сторож не нужен
            esac
        done
        kill -0 "$PLX_PID" 2>/dev/null || exit 0
        echo "⚠ СТОРОЖ: за ${START_TIMEOUT}с parallax не прошёл первый кадр — убиваю (SIGKILL)." | tee -a "$LOG"
        kill -KILL "$PLX_PID" 2>/dev/null || true
    ) &
    WATCHDOG_PID=$!

    # Код возврата нужен целым: он и решает, перезапускаться ли (Super+R).
    # `|| PLX_RC=$?` обязателен — скрипт под set -e.
    PLX_RC=0
    wait "$PLX_PID" || PLX_RC=$?
    if [[ "$PLX_RC" == "$RESTART_CODE" ]]; then
        cleanup --keep-audio
        echo ""
        echo "══ Super+R: перезапуск parallax ══"
        rebuild_if_stale
        continue
    fi
    cleanup
    break
done

printf '\n═══ Разбор лога: %s ═══\n' "$LOG"
grep -q "DRM master" "$LOG" && echo "✔ DRM master получен" || echo "✗ DRM master НЕ получен"
if grep -q "error while loading shared libraries" "$LOG"; then
    echo "⚠ Бинарь не стартовал — не хватает библиотек:"
    grep "error while loading shared libraries" "$LOG"
fi
panics=$(grep -ci "panic" "$LOG" 2>/dev/null || echo 0)
echo "Паники: $panics"
