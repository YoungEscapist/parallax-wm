#!/usr/bin/env bash
# Собрать/проверить проект из Bash tool песочницы Claude Code.
# Хардкод путей в /nix/store хрупкий (ломается после nix-collect-garbage),
# поэтому используем nix-shell shell.nix — сам находит/докачивает нужное.
#
# Использование:
#   ./build_env.sh check   — cargo check --release
#   ./build_env.sh build   — cargo build --release --target-dir /mnt/dawn-build/target

cmd="${1:-check}"
case "$cmd" in
    check)
        nix-shell shell.nix --run "cargo check --release"
        ;;
    build)
        nix-shell shell.nix --run "cargo build --release --target-dir /mnt/dawn-build/target"
        ;;
    *)
        echo "usage: $0 [check|build]" >&2
        exit 1
        ;;
esac
