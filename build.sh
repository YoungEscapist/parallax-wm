#!/usr/bin/env bash
set -e

# ВАЖНО: сборка идёт ЧИСТО системным тулчейном (cargo/rustc/mold из xbps),
# БЕЗ nix-shell. Раньше сборка через nix-shell (см. build_env.sh) тянула
# библиотеки (libinput/libseat/libdrm/mesa/libxkbcommon/...) из /nix/store,
# и они попадали в RUNPATH бинаря. Но line-loader (PT_INTERP) у бинаря —
# системный (/lib64/ld-linux-x86-64.so.2), т.к. компилятор/линковщик тоже
# системные. Системный ld.so, загружая Nix-собранный libgbm.so.1 и другие,
# по ИХ собственному RUNPATH подтягивал ещё и Nix-glibc (libc.so.6) —
# получался процесс с системным интерпретатором, но чужой (Nix) libc.
# glibc жёстко требует, чтобы ld.so и libc.so.6 были из одной сборки —
# итог: моментальный general protection fault в libc.so.6 при каждом
# запуске (см. dmesg: "dawn[...] general protection fault ... in libc.so.6"),
# без единой строчки в логе — падало до init tracing_subscriber.
#
# Фикс: не использовать nix-shell для сборки вообще. В системе (Void, xbps)
# уже есть все нужные -dev пакеты и .pc-файлы (wayland-client, libinput,
# libseat, libdrm, gbm, egl, glesv2, libdisplay-info, libudev, xkbcommon) —
# pkg-config находит их сам без PKG_CONFIG_PATH. mold ставится штатно:
# `sudo xbps-install mold`.

cd "$(dirname "$0")"
cargo build --release --target-dir /mnt/dawn-build/target "$@"

echo ""
echo "Binary: /mnt/dawn-build/target/release/dawn"
