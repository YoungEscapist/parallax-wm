#!/usr/bin/env bash
# launch_portable.sh — запуск dawn на обычном Linux (НЕ NixOS), без nix-хардкодов
# LD_LIBRARY_PATH. Библиотеки берутся из системного ld.so (пакеты из
# build_portable.sh). Запускать с ЧИСТОГО TTY (Ctrl+Alt+F3, логин без DE) —
# dawn берёт DRM master, поэтому графическая сессия на этом VT бежать не должна.
#
# Использование: ./launch_portable.sh [--debug]
set -euo pipefail
DIR="$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)"

BIN="$DIR/target/release/dawn"
[[ "${1:-}" == --debug ]] && BIN="$DIR/target/debug/dawn"

if [[ ! -x "$BIN" ]]; then
    echo "Бинарь не найден: $BIN" >&2
    echo "Сначала собери: ./build_portable.sh" >&2
    exit 1
fi

# XDG_RUNTIME_DIR нужен smithay/libseat.
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export RUST_LOG="${RUST_LOG:-info}"

mkdir -p "$DIR/logs"
LOG="$DIR/logs/dawn_$(date +%Y%m%d_%H%M%S).log"

echo "Бинарь: $BIN"
echo "Лог:    $LOG"
echo "Выход:  Super+Shift+Q"

# Чистим переменные текущей сессии, чтобы dawn не пытался быть вложенным клиентом.
env -u DISPLAY -u WAYLAND_DISPLAY "$BIN" --tty 2>&1 | tee "$LOG"
