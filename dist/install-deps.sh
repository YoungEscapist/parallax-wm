#!/usr/bin/env bash
# Поставить системные зависимости Parallax.
#
#   sudo ./dist/install-deps.sh          — поставить
#   ./dist/install-deps.sh --print       — только показать команду, ничего не делать
#
# Списки те же, что в шапке build_portable.sh, — там они и остаются
# справочником для дистрибутивов, которых здесь нет.
#
# Чего в списках НЕТ намеренно:
#   * Lua — mlua собирает Lua 5.4 из исходников (feature "vendored");
#   * rustup — ставить его пакетом дистрибутива смысла нет, см. rustup.rs;
#   * mold — необязателен, .cargo/config.toml просит его, а build_portable.sh
#     тихо откатывается на системный линкер, если mold не найден.
#
# ВНИМАНИЕ: имена переменных только латиницей — bash считает «имя=значение»
# с кириллицей КОМАНДОЙ (грабля, уже съедавшая chown в build.sh и migrate.sh).
set -euo pipefail

print_only=0
[ "${1:-}" = "--print" ] && print_only=1

if command -v xbps-install >/dev/null 2>&1; then
    # Void. Отдельных -dev пакетов здесь больше, чем где-либо: у Void они
    # разъехались с основными (libseat/libseat-devel и так далее).
    manager="Void (xbps)"
    cmd=(xbps-install -y
        base-devel pkg-config
        wayland wayland-devel libxkbcommon libxkbcommon-devel
        libinput libinput-devel eudev-libudev-devel
        libseat libseat-devel libdrm libdrm-devel
        MesaLib-devel libglvnd-devel libdisplay-info libdisplay-info-devel
        # Пакета `xwayland` в Void нет вовсе — вложенный X зовётся так:
        xorg-server-xwayland)
elif command -v pacman >/dev/null 2>&1; then
    # Arch. Ни libseat, ни libudev, ни libegl отдельными пакетами тут НЕ
    # существуют: их дают seatd, systemd-libs и mesa. Прежний скрипт звал
    # несуществующие имена и падал на первой же строке.
    manager="Arch (pacman)"
    cmd=(pacman -S --needed --noconfirm
        base-devel pkgconf wayland libxkbcommon libinput
        systemd-libs seatd libdrm mesa libdisplay-info xorg-xwayland)
elif command -v apt >/dev/null 2>&1; then
    manager="Debian/Ubuntu (apt)"
    cmd=(apt install -y
        build-essential pkg-config
        libwayland-dev libxkbcommon-dev libinput-dev libudev-dev
        libseat-dev libdrm-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev
        libdisplay-info-dev xwayland)
elif command -v dnf >/dev/null 2>&1; then
    manager="Fedora (dnf)"
    cmd=(dnf install -y
        gcc pkgconf-pkg-config
        wayland-devel libxkbcommon-devel libinput-devel systemd-devel
        libseat-devel libdrm-devel mesa-libgbm-devel mesa-libEGL-devel
        mesa-libGLES-devel libdisplay-info-devel xorg-x11-server-Xwayland)
else
    echo "Не узнаю пакетный менеджер. Списки для других дистрибутивов —" >&2
    echo "в шапке build_portable.sh; на NixOS есть shell.nix." >&2
    exit 1
fi

echo "# $manager"
printf '%q ' "${cmd[@]}"
echo

if [ "$print_only" = 1 ]; then
    exit 0
fi

if [ "$(id -u)" != 0 ]; then
    echo "Нужны права root: sudo $0" >&2
    exit 1
fi

"${cmd[@]}"

echo
echo "Готово. Дальше:"
echo "  rustup — если Rust ещё не стоит (https://rustup.rs)"
echo "  ./build_portable.sh — собрать plx-standard и plx-extra"
