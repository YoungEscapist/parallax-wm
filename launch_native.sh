#!/usr/bin/env bash
# launch_native.sh — запуск dawn напрямую, БЕЗ nix-shell.
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
# без единой строчки в логе — проверьте, что сборка НЕ шла через
# build_env.sh build (nix-shell), а через build.sh.
#
# Использование: ./launch_native.sh [--debug] [--winit]
set -euo pipefail

DAWN_DIR="$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)"
BUILD_DIR="/mnt/dawn-build/target"
BINARY="$BUILD_DIR/release/dawn"
[[ "${1:-}" == --debug ]] && BINARY="$BUILD_DIR/debug/dawn"

DWALL_BIN="$HOME/.local/bin/dwall"

if [[ ! -x "$BINARY" ]]; then
    echo "Бинарь не найден: $BINARY" >&2
    echo "Сборка: cd $DAWN_DIR && ./build_env.sh build" >&2
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
if [[ -e /etc/NIXOS && -z "${LD_LIBRARY_PATH:-}" && -f "$DAWN_DIR/shell.nix" ]]; then
    echo "NixOS: беру LD_LIBRARY_PATH из shell.nix (dlopen-библиотеки)…"
    LD_LIBRARY_PATH="$(nix-shell "$DAWN_DIR/shell.nix" --run 'printf %s "$LD_LIBRARY_PATH"')"
    export LD_LIBRARY_PATH
fi

# ── Приветствие zsh в терминалах сессии ──────────────────────────────────────
# .zshrc показывает fastfetch один раз на цепочку шеллов и помечает это
# экспортом __FASTFETCH_SHOWN. Но dawn запускают ИЗ логин-шелла tty, который
# приветствие уже показал, — и флаг наследовался дальше: в dawn, в каждый
# ghostty, в каждый zsh внутри него. Итог: приветствия не было НИ В ОДНОМ
# терминале сессии. Сессия компоновщика — новый контекст, флаг предыдущего
# шелла к ней отношения не имеет.
unset __FASTFETCH_SHOWN

LOG_DIR="$DAWN_DIR/logs"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/dawn_native_$(date +%Y%m%d_%H%M%S).log"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"

# ── Сессионная шина D-Bus ────────────────────────────────────────────────────
# Без неё в сессии нет ни портала (демонстрация экрана, выбор файлов), ни
# уведомлений: xdg-desktop-portal живёт ТОЛЬКО на сессионной шине. Замер
# 03.08.2026: у dawn не было DBUS_SESSION_BUS_ADDRESS вовсе, /run/user/1000/bus
# не существовал, а единственный запущенный xdg-desktop-portal висел в чужой
# сессии (XDG_RUNTIME_DIR=/tmp/runtime-1000, шина /tmp/dbus-*) — приложения из
# dawn его не видели.
#
# Перезапускаем сам скрипт под dbus-run-session: она поднимает шину, ставит
# DBUS_SESSION_BUS_ADDRESS и умирает вместе с сессией.
if [[ -z "${DBUS_SESSION_BUS_ADDRESS:-}" && -z "${DAWN_NO_DBUS:-}" ]]; then
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
# запущен (в сессии он один на пользователя) и глушим вместе с dawn.
#
# Живость проверяем ПО СОКЕТАМ, а не по именам процессов. `pipewire-pulse` —
# это тот же бинарь, запущенный как `pipewire -c pipewire-pulse.conf`, и comm у
# него тоже "pipewire". Осиротевший мост от прошлой сессии (dawn убили, мост
# пережил) ловился прежним `pgrep -x pipewire`, скрипт считал ядро поднятым и
# не запускал ни его, ни wireplumber. В итоге сессия оставалась совсем без
# звука: сокета pipewire-0 нет, устройств нет, `pactl` отвечает Connection
# refused, меню звука dawn пишет «no output device» (разобрано 05.08.2026).
PIPEWIRE_PIDS=()
if [[ -z "${DAWN_NO_PIPEWIRE:-}" ]]; then
    # Ядра нет — значит всё, что осталось от прошлой сессии, мёртвый груз, и
    # к тому же держит сокет pulse, куда иначе не встанет новый мост.
    if [[ ! -S "$XDG_RUNTIME_DIR/pipewire-0" ]]; then
        pkill -x -u "$(id -u)" pipewire    >/dev/null 2>&1 || true
        pkill -x -u "$(id -u)" wireplumber >/dev/null 2>&1 || true
        sleep 0.5
    fi
    # $1 — бинарь, $2 — сокет-признак того, что он уже работает.
    dawn_start_audio() {
        command -v "$1" >/dev/null || return 0
        [[ -S "$2" ]] && return 0
        "$1" >/dev/null 2>&1 &
        PIPEWIRE_PIDS+=("$!")
        sleep 0.5
    }
    dawn_start_audio pipewire "$XDG_RUNTIME_DIR/pipewire-0"
    # wireplumber — сессионный менеджер: без него ядро живо, но НИ ОДНОГО
    # устройства не создаёт. Своего сокета у него нет, зато имя уникальное,
    # так что здесь pgrep как раз уместен.
    if command -v wireplumber >/dev/null \
       && ! pgrep -x -u "$(id -u)" wireplumber >/dev/null; then
        wireplumber >/dev/null 2>&1 &
        PIPEWIRE_PIDS+=("$!")
        sleep 0.5
    fi
    dawn_start_audio pipewire-pulse "$XDG_RUNTIME_DIR/pulse/native"
fi
# XCURSOR_PATH нарочно НЕ задаём: если он установлен, крейт xcursor
# полностью игнорирует стандартные XDG-пути (~/.icons, /usr/share/icons)
# и ищет только там — а /run/current-system (унаследовано из NixOS-версии
# этого скрипта) на этой системе не существует, из-за чего курсор был
# невидим вне окон. Без XCURSOR_PATH crate сам находит /usr/share/icons.
# Курсор: минималистичная белая тема Vanilla-DMZ-AA (пакет xcursor-vanilla-dmz-aa)
# и небольшой размер. Обе переменные читают и клиенты (GTK/Qt/wayland-cursor),
# и сам компоновщик (см. load_default_cursor), так что стрелка везде одна.
export XCURSOR_THEME="${XCURSOR_THEME:-Vanilla-DMZ-AA}"
export XCURSOR_SIZE="${XCURSOR_SIZE:-16}"
# debug по умолчанию: диагностические строки (PTR HIT, PTR КАДР, PTR ЛОКАЛЬ,
# КУРСОР КЛИЕНТА) пишутся в файл лога, а не на экран, зато после жалобы
# «кликаю не туда» данные уже собраны — переспрашивать и повторять не надо.
export RUST_LOG="${RUST_LOG:-dawn=debug,info}"
# По этому имени xdg-desktop-portal выбирает бэкенд (см. launch_tty.zsh).
export XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-dawn}"
# Именно "=", а НЕ ":-": elogind заводит сессию с tty и выставляет
# XDG_SESSION_TYPE=tty ещё до нас, поэтому значение по умолчанию не
# срабатывало никогда. Цена ошибки — вся демонстрация экрана: libwebrtc
# внутри Chromium/Electron считает сессию вэйландовой ТОЛЬКО по этой
# переменной (IsRunningUnderWayland: XDG_SESSION_TYPE=wayland И
# WAYLAND_DISPLAY). Со значением tty он берёт X11-захват через DISPLAY=:0,
# то есть корневое окно XWayland — а оно у rootless-сервера пустое:
# Discord показывал ЧЁРНЫЙ экран с одним курсором, и портал dawn при этом
# не получал ни одного вызова.
export XDG_SESSION_TYPE=wayland
export NIXOS_OZONE_WL="${NIXOS_OZONE_WL:-1}"

# dwall (обои) запускается ПОСЛЕ dawn — см. ниже, после старта компоновщика.
DWALL_PID=""

MODE_ARGS=()
[[ "${1:-}" == --winit || "${2:-}" == --winit ]] && MODE_ARGS+=(--winit)

echo "Бинарь:  $BINARY"
echo "dwall:   $DWALL_BIN"
echo "Лог:     $LOG"
echo ""

"$BINARY" "${MODE_ARGS[@]}" > >(tee "$LOG") 2>&1 &
DAWN_PID=$!

# ── Сторожевой таймер старта ─────────────────────────────────────────────────
# dawn умеет намертво залипать на ПЕРВОМ кадре (futex-дедлок в одном потоке,
# 0% CPU, ни строчки в лог — см. логи 115942/121144). К этому моменту он уже
# забрал у seat'а клавиатуру с мышью и держит DRM master, так что машина
# становится недоступна целиком: ни ввода, ни переключения VT, только жёсткий
# ресет или вход по сети. Сторож ждёт START_TIMEOUT секунд признака жизни
# (любая строка после «initial render») и, не дождавшись, убивает dawn — VT
# возвращается в шелл сам. Порог задаётся DAWN_START_TIMEOUT.
# ── dwall (обои и меню выбора) ───────────────────────────────────────────────
# Ждём не САМ ФАЙЛ сокета, а строчку о нём в логе ЭТОГО запуска. Разница
# принципиальная: файл сокета остаётся на диске от прошлой сессии (dawn его не
# всегда прибирает, если его убили), проверка на -S проходила мгновенно, и
# dwall стартовал в пустоту — до того, как новый компоновщик вообще начал
# слушать. Он падал, перезапускался по кругу, и нажатие Win+W в этот момент
# уходило в никуда: pkill не находил процесса.
#
# Дальше держим dwall под присмотром, пока жив dawn: если он умрёт (или его
# уронит смена обоев), поднимаем заново, а не оставляем сессию без обоев.
if [[ -x "$DWALL_BIN" ]]; then
    (
        for _ in $(seq 1 100); do
            grep -q "dawn socket:" "$LOG" 2>/dev/null && break
            sleep 0.1
        done
        grep -q "dawn socket:" "$LOG" 2>/dev/null || exit 0
        export WAYLAND_DISPLAY=wayland-1
        # DISPLAY, унаследованный от логин-шелла (например от параллельной
        # X-сессии на другом VT), уводил GTK-диалоги dwall в ЧУЖОЙ X-сервер:
        # проводник по плюсику открывался там и на экране dawn не появлялся
        # вовсе. В wayland-сессии его быть не должно.
        unset DISPLAY
        while kill -0 "$DAWN_PID" 2>/dev/null; do
            # || true обязателен: скрипт под set -e, а упавший (или убитый)
            # dwall возвращает ненулевой код — без этого умирал сам сторож,
            # и обои больше не поднимались до конца сессии.
            "$DWALL_BIN" >/dev/null 2>&1 || true
            sleep 0.5
        done
    ) &
    DWALL_PID=$!
else
    echo "dwall не найден по пути $DWALL_BIN — обои не запустятся" >&2
fi

# ── История буфера обмена (Super+C) ─────────────────────────────────────────
# cliphist сам ничего не слушает: за буфером следит wl-paste и складывает
# каждое копирование в базу (~/.cache/cliphist/db). Два сторожа, а не один:
# `wl-paste --watch` без --type берёт только ПРЕДПОЧТИТЕЛЬНЫЙ тип, и снимок
# экрана (image/png) мимо текстового сторожа прошёл бы незамеченным — а он тут
# главный жилец (PrtScr кладёт скрин ровно сюда).
#
# Ждём ту же строчку сокета, что и dwall: до неё композитора ещё нет, и
# wl-paste просто вышел бы с ошибкой.
if command -v cliphist >/dev/null 2>&1 && command -v wl-paste >/dev/null 2>&1; then
    (
        for _ in $(seq 1 100); do
            grep -q "dawn socket:" "$LOG" 2>/dev/null && break
            sleep 0.1
        done
        grep -q "dawn socket:" "$LOG" 2>/dev/null || exit 0
        export WAYLAND_DISPLAY=wayland-1
        unset DISPLAY
        wl-paste --type text --watch cliphist store >/dev/null 2>&1 &
        wl-paste --type image --watch cliphist store >/dev/null 2>&1 &
        wait
    ) &
    CLIPHIST_PID=$!
fi

START_TIMEOUT="${DAWN_START_TIMEOUT:-25}"
(
    for _ in $(seq 1 "$START_TIMEOUT"); do
        sleep 1
        kill -0 "$DAWN_PID" 2>/dev/null || exit 0   # сам завершился — сторож не нужен
        last=$(grep -v '^[[:space:]]*$' "$LOG" 2>/dev/null | tail -1 || true)
        case "$last" in
            *"initial render"*) ;;                   # всё ещё на первом кадре
            "") ;;                                   # лог пуст — рано судить
            *) exit 0 ;;                             # жизнь есть, сторож не нужен
        esac
    done
    kill -0 "$DAWN_PID" 2>/dev/null || exit 0
    echo "⚠ СТОРОЖ: за ${START_TIMEOUT}с dawn не прошёл первый кадр — убиваю (SIGKILL)." | tee -a "$LOG"
    kill -KILL "$DAWN_PID" 2>/dev/null || true
) &
WATCHDOG_PID=$!

# dwall переживал смерть dawn и потом крутил ядро на 80% в цикле
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
    for pid in "${PIPEWIRE_PIDS[@]:-}"; do
        [[ -n "$pid" ]] && kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

wait "$DAWN_PID" || true
cleanup

printf '\n═══ Разбор лога: %s ═══\n' "$LOG"
grep -q "DRM master" "$LOG" && echo "✔ DRM master получен" || echo "✗ DRM master НЕ получен"
if grep -q "error while loading shared libraries" "$LOG"; then
    echo "⚠ Бинарь не стартовал — не хватает библиотек:"
    grep "error while loading shared libraries" "$LOG"
fi
panics=$(grep -ci "panic" "$LOG" 2>/dev/null || echo 0)
echo "Паники: $panics"
