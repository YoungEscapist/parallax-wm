#!/usr/bin/env bash
# build_portable.sh — сборка parallax на обычном Linux (НЕ NixOS), без nix-shell.
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

# ДВА ОТДЕЛЬНЫХ ВЫЗОВА, а не один общий, и каталоги сборки тоже разные — ровно
# как в build.sh, и по тем же двум причинам:
#
#  1. Голый `cargo build --release` здесь не собирал НИЧЕГО исполняемого.
#     Корневой пакет `parallax` — это только `[lib]`, а оба бинаря лежат в
#     отдельных крейтах bins/. По умолчанию cargo строит один корневой пакет,
#     так что в target/release не появлялось ни plx-standard, ни plx-extra.
#  2. Собирать их одной командой всё равно нельзя: cargo ОБЪЕДИНЯЕТ наборы фич
#     членов workspace и строит библиотеку по сумме — «минимальный» бинарь
#     получил бы и vr, и mine, и share. Разные каталоги нужны потому, что
#     отпечаток сборки включает набор фич: в общем каталоге вызовы вытесняли бы
#     друг друга и пересобирали всё заново (fat LTO — это минуты).
cargo build --release --target-dir target/standard -p plx-standard "$@"
cargo build --release --target-dir target/extra   -p plx-extra   "$@"

# Складываем оба рядом, туда, где их обещает README.
mkdir -p target/release
cp -f target/standard/release/plx-standard target/release/plx-standard
cp -f target/extra/release/plx-extra     target/release/plx-extra

echo ""
echo "Бинари:"
echo "  ./target/release/plx-standard  (без шлема, Minecraft и мультиюзера)"
echo "  ./target/release/plx-extra    (всё)"
