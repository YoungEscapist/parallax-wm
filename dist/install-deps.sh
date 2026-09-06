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

# Знает ли пакетный менеджер такое имя. Нужно там, где один список обслуживает
# несколько родственных дистрибутивов (Arch и Artix, Manjaro, EndeavourOS): имена
# у них расходятся, и звать несуществующий пакет нельзя — pacman отказывается
# ставить ВСЁ, а не только пропавшее.
pac_known() { pacman -Si "$1" >/dev/null 2>&1 || pacman -Sg "$1" >/dev/null 2>&1; }

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
        pipewire-devel pixman-devel
        # clang — не для сборки нашего кода, а для bindgen: libspa-sys и
        # pipewire-sys генерят привязки по системным заголовкам и ищут
        # libclang.so. Без него сборка падает на «Unable to find libclang».
        clang
        # Пакета `xwayland` в Void нет вовсе — вложенный X зовётся так:
        xorg-server-xwayland)
elif command -v pacman >/dev/null 2>&1; then
    # Arch и родня. Ни libseat, ни libegl отдельными пакетами тут НЕ существуют:
    # их дают seatd и mesa. Прежний скрипт звал несуществующие имена и падал на
    # первой же строке.
    manager="Arch (pacman)"
    pkgs=(base-devel pkgconf wayland libxkbcommon libinput
        seatd libdrm mesa libdisplay-info pipewire pixman
        xorg-xwayland clang)

    # Читается ли база вообще: если pacman не знает даже сам себя, значит
    # `pacman -Sy` ни разу не звали, и спрашивать её про имена бессмысленно —
    # тогда берём имена как для обычного Arch и пусть ругается сам pacman.
    db_ok=0
    pac_known pacman && db_ok=1
    [ "$db_ok" = 1 ] || echo "# база пакетов пуста — сначала: sudo pacman -Sy" >&2

    # libudev даёт systemd-libs — но ТОЛЬКО там, где есть systemd. На Artix и
    # прочих без него пакета systemd-libs не существует вовсе (там `libudev` из
    # репозитория system), а pacman на неизвестное имя отказывает всей команде
    # целиком: «target not found» — и установка падает, не поставив ничего.
    udev_pkg=systemd-libs
    if [ "$db_ok" = 1 ]; then
        udev_pkg=""
        for p in systemd-libs libudev eudev libudev-zero; do
            if pac_known "$p"; then udev_pkg="$p"; break; fi
        done
    fi
    if [ -n "$udev_pkg" ]; then
        pkgs+=("$udev_pkg")
    else
        echo "# внимание: провайдер libudev не найден в базе pacman" >&2
        echo "#   (systemd-libs / libudev / eudev) — поставьте его руками" >&2
    fi

    # То же лекарство от расхождения имён для всего остального: имя, которого в
    # базе нет, выкидываем с предупреждением, вместо того чтобы уронить всё.
    if [ "$db_ok" = 1 ]; then
        kept=()
        for p in "${pkgs[@]}"; do
            if pac_known "$p"; then
                kept+=("$p")
            else
                echo "# пропускаю: в репозиториях нет пакета «$p»" >&2
            fi
        done
        pkgs=("${kept[@]}")
    fi

    cmd=(pacman -S --needed --noconfirm "${pkgs[@]}")
elif command -v apt >/dev/null 2>&1; then
    manager="Debian/Ubuntu (apt)"
    cmd=(apt install -y
        build-essential pkg-config
        libwayland-dev libxkbcommon-dev libinput-dev libudev-dev
        libseat-dev libdrm-dev libgbm-dev libegl1-mesa-dev libgles2-mesa-dev
        libdisplay-info-dev libpipewire-0.3-dev libspa-0.2-dev libpixman-1-dev
        libclang-dev
        xwayland)
elif command -v dnf >/dev/null 2>&1; then
    manager="Fedora (dnf)"
    cmd=(dnf install -y
        gcc pkgconf-pkg-config
        wayland-devel libxkbcommon-devel libinput-devel systemd-devel
        libseat-devel libdrm-devel mesa-libgbm-devel mesa-libEGL-devel
        mesa-libGLES-devel libdisplay-info-devel pipewire-devel pixman-devel
        clang-devel
        xorg-x11-server-Xwayland)
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
