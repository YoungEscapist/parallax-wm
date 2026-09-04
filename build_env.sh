#!/usr/bin/env bash
# Собрать/проверить проект из Bash tool песочницы Claude Code.
# Хардкод путей в /nix/store хрупкий (ломается после nix-collect-garbage),
# поэтому используем nix-shell shell.nix — сам находит/докачивает нужное.
#
# Использование:
#   ./build_env.sh check   — cargo check --release
#   ./build_env.sh build   — cargo build --release --target-dir /mnt/plx-build/target
#
# ПРЕДУПРЕЖДЕНИЕ (2026-08-03): `build` через nix-shell даёт бинарь, который
# падает с general protection fault в libc.so.6 при каждом запуске —
# системный ld.so (PT_INTERP) + Nix-glibc, подтянутая транзитивно через
# RUNPATH нижних либ, несовместимы. `check` безопасен (компиляция без
# финальной линковки в исполняемый файл). Для реальной сборки используйте
# build.sh (чистый системный тулчейн, в системе уже есть все -dev пакеты).

cmd="${1:-check}"
case "$cmd" in
    check)
        nix-shell shell.nix --run "cargo check --release"
        ;;
    build)
        nix-shell shell.nix --run "cargo build --release --target-dir /mnt/plx-build/target"
        ;;
    *)
        echo "usage: $0 [check|build]" >&2
        exit 1
        ;;
esac
