#!/usr/bin/env bash
# install-extra.sh — поставить plx-extra, полную сборку Parallax.
#
#   ./install-extra.sh
#   curl -fsSL https://raw.githubusercontent.com/YoungEscapist/parallax-wm/master/install-extra.sh | bash
#
# Это тонкая обёртка над install.sh: вся работа (пакеты, rustup, сборка,
# конфиг, пункт в менеджере входа) там, здесь только жёстко выбрана сборка.
# Два отдельных файла существуют затем, чтобы человеку не приходилось помнить
# ключ: он видит два имени и выбирает по описанию, а не по --extra.
#
# Что такое plx-extra: всё, что есть в plx-standard, плюс то, чего в ней нет
# нарочно — VR-режим (шлем через WiVRn/OpenXR), окна в Minecraft (dmine),
# гости и демонстрация экрана нескольким (dshare, мультиюзер) и шейдерные
# украшения: бесконечный куб, свет вместо солнца, свечение окон в цвет обоев.
# Тянет больше зависимостей и собирается дольше.
#
# Обычная сборка — install-standard.sh. Обе разом — ./install.sh --both.
#
# Все прочие ключи install.sh работают и здесь и передаются как есть:
#   ./install-extra.sh --quick -y
#   ./install-extra.sh --update
#   ./install-extra.sh --dry-run
#
# ВНИМАНИЕ: имена переменных только латиницей — bash считает «имя=значение»
# с кириллицей КОМАНДОЙ (грабля, уже съедавшая chown в build.sh).
set -euo pipefail

flavour=--extra
flavour_name=plx-extra

repo_url=${PLX_REPO:-https://github.com/YoungEscapist/parallax-wm.git}
raw_url=${PLX_RAW:-https://raw.githubusercontent.com/YoungEscapist/parallax-wm/master/install.sh}

# Выбор сборки принадлежит имени файла, а не командной строке: иначе
# `install-extra.sh --standard` поставил бы не то, что написано на коробке.
args=()
for a in "$@"; do
    case "$a" in
        --standard|--extra|--both)
            echo "install-extra.sh: ключ $a пропущен — этот скрипт ставит только $flavour_name" >&2
            echo "                  (обе сборки разом: ./install.sh --both)" >&2
            ;;
        *) args+=("$a") ;;
    esac
done

# Дерево рядом — запускаем install.sh из него. Нет дерева (запуск через
# curl | bash, тогда $0 это `bash`) — тянем install.sh из репозитория, а он
# уже сам склонирует исходники и перезапустится внутри клона.
self=${BASH_SOURCE[0]:-$0}
here=""
if [ -f "$self" ]; then
    candidate=$(cd -- "$(dirname -- "$(realpath -- "$self")")" && pwd)
    [ -f "$candidate/install.sh" ] && here="$candidate"
fi

if [ -n "$here" ]; then
    exec bash "$here/install.sh" "$flavour" ${args[@]+"${args[@]}"}
fi

command -v curl >/dev/null 2>&1 || {
    echo "install-extra.sh: нужен curl, чтобы скачать install.sh" >&2
    echo "                  (или склонируйте дерево: git clone $repo_url)" >&2
    exit 1
}
tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT
curl -fsSL "$raw_url" -o "$tmp"
exec bash "$tmp" "$flavour" ${args[@]+"${args[@]}"}
