#!/usr/bin/env bash
# install-cursor.sh — поставить тему курсора Bibata-Modern-Ice.
#
#   ./dist/install-cursor.sh           поставить (в ~/.local/share/icons, без root)
#   ./dist/install-cursor.sh --print   только сказать, что будет сделано
#   ./dist/install-cursor.sh --force   переставить, даже если тема уже есть
#
# ЗАЧЕМ ЭТО ОТДЕЛЬНЫЙ ШАГ, А НЕ СТРОЧКА В install-deps.sh. Курсор в Wayland
# рисует композитор, а картинку он берёт из ТЕМЫ КУРСОРА — набора файлов в
# /usr/share/icons или ~/.local/share/icons. Темы нет — компоновщик пишет в лог
# «the theme has no cursor […] cursor will be invisible» и рисует пустоту:
# указатель при этом жив, двигается и попадает по кнопкам, но его НЕ ВИДНО, и
# со стороны это выглядит как «установка сломала мышь».
#
# Пакета `bibata` нет ни в Void, ни в официальных репозиториях Arch (там она
# живёт в AUR), ни в Debian, ни в Fedora — поэтому пакетным менеджером её взять
# нельзя, и тема ставится готовым архивом из релизов проекта. Это единственное
# место во всей установке, которое лезет в сеть мимо пакетного менеджера и
# cargo, — поэтому оно и вынесено в отдельный скрипт, который видно.
#
# Ставим В ДОМАШНИЙ каталог, а не в /usr/share/icons: root для этого не нужен,
# а компоновщик и клиенты (GTK/Qt/wayland-cursor) читают обе ветки одинаково.
#
# ВНИМАНИЕ: имена переменных только латиницей — bash считает «имя=значение»
# с кириллицей КОМАНДОЙ (грабля, уже съедавшая chown в build.sh).
set -euo pipefail

theme=${PLX_CURSOR_THEME:-Bibata-Modern-Ice}
# Версия закреплена: «latest» однажды переедет, а сломанный курсор — ровно та
# беда, которую этот скрипт и лечит. Обновлять руками, сверяя с релизами.
version=${PLX_CURSOR_VERSION:-v2.0.7}
url=${PLX_CURSOR_URL:-https://github.com/ful1e5/Bibata_Cursor/releases/download/$version/$theme.tar.xz}
dest=${PLX_CURSOR_DEST:-$HOME/.local/share/icons}

print_only=0
force=0
for a in "$@"; do
    case "$a" in
        --print) print_only=1 ;;
        --force) force=1 ;;
        -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
        *) echo "Неизвестный ключ: $a" >&2; exit 2 ;;
    esac
done

# Уже есть? Смотрим во все три ветки, куда её мог положить кто угодно до нас:
# системную, старую домашнюю (~/.icons) и XDG-шную. Признак темы — каталог
# `cursors` внутри: пустой каталог с одним index.theme даёт ровно ту же
# невидимую стрелку, что и полное отсутствие темы.
found=""
for dir in "/usr/share/icons/$theme" "$HOME/.icons/$theme" "$dest/$theme"; do
    [ -d "$dir/cursors" ] && { found="$dir"; break; }
done

if [ -n "$found" ] && [ "$force" = 0 ]; then
    echo "# тема курсора $theme уже стоит: $found"
    exit 0
fi

if [ "$print_only" = 1 ]; then
    echo "# скачать $url"
    echo "# распаковать в $dest"
    exit 0
fi

command -v curl >/dev/null 2>&1 || { echo "нужен curl, чтобы скачать тему курсора" >&2; exit 1; }
command -v tar  >/dev/null 2>&1 || { echo "нужен tar, чтобы распаковать тему курсора" >&2; exit 1; }
# xz-архив: без распаковщика tar молча не справится — скажем это заранее.
command -v xz   >/dev/null 2>&1 || { echo "нужен xz (пакет xz / xz-utils)" >&2; exit 1; }

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "# скачиваю $theme $version"
curl -fL --retry 2 --connect-timeout 20 -o "$tmp/theme.tar.xz" "$url" \
    || { echo "не удалось скачать $url" >&2; exit 1; }

mkdir -p "$dest"
# В архиве лежит один каталог с именем темы — распаковываем как есть.
tar -xJf "$tmp/theme.tar.xz" -C "$dest"

[ -d "$dest/$theme/cursors" ] || { echo "распаковалось не то: в $dest/$theme нет cursors/" >&2; exit 1; }
echo "# готово: $dest/$theme"
