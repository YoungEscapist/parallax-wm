#!/usr/bin/env bash
# build_portable.sh — сборка parallax на обычном Linux (НЕ NixOS), без nix-shell.
# Полагается на системные -dev библиотеки через pkg-config + rustup/cargo.
#
# Зависимости (примеры имён пакетов):
#   Debian/Ubuntu:
#     sudo apt install build-essential pkg-config cargo rustc \
#       libwayland-dev libxkbcommon-dev libinput-dev libudev-dev \
#       libseat-dev libdrm-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev \
#       libdisplay-info-dev libpipewire-0.3-dev libspa-0.2-dev libpixman-1-dev
#     # для запуска нужен ещё xwayland
#   Fedora:
#     sudo dnf install gcc pkgconf-pkg-config cargo rust \
#       wayland-devel libxkbcommon-devel libinput-devel systemd-devel \
#       libseat-devel libdrm-devel mesa-libgbm-devel mesa-libEGL-devel \
#       mesa-libGLES-devel libdisplay-info-devel pipewire-devel pixman-devel \
#       xorg-x11-server-Xwayland
#   Arch:
#     sudo pacman -S base-devel pkgconf rust wayland libxkbcommon libinput \
#       systemd-libs seatd libdrm mesa libdisplay-info pipewire pixman \
#       xorg-xwayland clang
#     (на Artix и прочей родне без systemd пакета systemd-libs нет — там libudev)
#
# Рекомендуется свежий стабильный Rust (rustup). mlua собирает Lua 5.4 из
# исходников (feature "vendored") — отдельный Lua ставить не нужно.
set -euo pipefail
cd "$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)"

# Линкер. В .cargo/config.toml теперь ПУСТО (там были mold и target-cpu=native
# — оба верные ровно для машины автора и оба ломавшие чужую сборку), так что
# ускорители подбираются здесь и только если они в системе есть.
plx_flags="${RUSTFLAGS:-}"
if command -v mold >/dev/null 2>&1; then
    plx_flags="$plx_flags -C link-arg=-fuse-ld=mold"
elif command -v ld.lld >/dev/null 2>&1; then
    plx_flags="$plx_flags -C link-arg=-fuse-ld=lld"
else
    echo "build_portable: ни mold, ни lld — собираю системным ld (дольше)."
fi
# Код под ЭТОТ процессор — только по явной просьбе: такой бинарь падает с
# SIGILL на другом железе, и по умолчанию так собираться не должно ничего.
if [[ "${PLX_NATIVE:-0}" != 0 ]]; then
    echo "build_portable: PLX_NATIVE=1 — бинарь будет непереносимым."
    plx_flags="$plx_flags -C target-cpu=native"
fi
export RUSTFLAGS="$plx_flags"

# Профиль сборки. release (fat LTO) по умолчанию; `--profile quick` или
# PLX_PROFILE=quick — тот же оптимизированный код с thin LTO: в 8 раз быстрее
# пересобирается и линкуется в несколько потоков, то есть проходит там, где
# fat LTO ловит OOM на 4-8 ГиБ памяти (см. [profile.quick] в Cargo.toml).
profile="${PLX_PROFILE:-release}"
args=()
for arg in "$@"; do
    case "$arg" in
        --profile) echo "build_portable: пишите --profile=имя одним словом" >&2; exit 2 ;;
        --profile=*) profile="${arg#*=}" ;;
        --quick) profile=quick ;;
        *) args+=("$arg") ;;
    esac
done

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
cargo build --profile "$profile" --target-dir target/standard -p plx-standard ${args[@]+"${args[@]}"}
cargo build --profile "$profile" --target-dir target/extra   -p plx-extra   ${args[@]+"${args[@]}"}

# Складываем оба рядом, туда, где их обещает README.
mkdir -p target/release
cp -f "target/standard/$profile/plx-standard" target/release/plx-standard
cp -f "target/extra/$profile/plx-extra"     target/release/plx-extra

echo ""
echo "Бинари:"
echo "  ./target/release/plx-standard  (без шлема, Minecraft и мультиюзера)"
echo "  ./target/release/plx-extra    (всё)"
