#!/usr/bin/env bash
# build_portable.sh — сборка dawn на обычном Linux (НЕ NixOS), без nix-shell.
# Полагается на системные -dev библиотеки через pkg-config + rustup/cargo.
#
# Зависимости (примеры имён пакетов):
#   Debian/Ubuntu:
#     sudo apt install build-essential pkg-config cargo rustc \
#       libwayland-dev libxkbcommon-dev libinput-dev libudev-dev \
#       libseat-dev libdrm-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev \
#       libdisplay-info-dev
#     # для запуска нужен ещё xwayland
#   Fedora:
#     sudo dnf install gcc pkgconf-pkg-config cargo rust \
#       wayland-devel libxkbcommon-devel libinput-devel systemd-devel \
#       libseat-devel libdrm-devel mesa-libgbm-devel mesa-libEGL-devel \
#       mesa-libGLES-devel libdisplay-info-devel xorg-x11-server-Xwayland
#   Arch:
#     sudo pacman -S base-devel pkgconf rust wayland libxkbcommon libinput \
#       systemd-libs seatd libdrm mesa libdisplay-info xorg-xwayland
#
# Рекомендуется свежий стабильный Rust (rustup). mlua собирает Lua 5.4 из
# исходников (feature "vendored") — отдельный Lua ставить не нужно.
set -euo pipefail
cd "$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)"

# Линкер: по умолчанию .cargo/config.toml просит mold. Если mold нет — тихо
# откатываемся на системный ld (переопределяем rustflags через env RUSTFLAGS,
# которое имеет приоритет над config.toml).
if ! command -v mold >/dev/null 2>&1; then
    echo "build_portable: mold не найден — собираю системным линкером."
    export RUSTFLAGS=""
fi

exec cargo build --release "$@"
# Бинарь: ./target/release/dawn
