#!/usr/bin/env bash
# Поставить Parallax в список сессий менеджера входа (ly, greetd, SDDM, GDM).
#
#   sudo ./dist/install-session.sh          — поставить
#   sudo ./dist/install-session.sh --uninstall
#
# Ставится ДВА файла и ничего больше:
#   /usr/local/bin/parallax-session          — обёртка, зовущая launch_native.sh
#   /usr/share/wayland-sessions/parallax.desktop
#
# Бинарь остаётся в дереве исходников, куда его положил build.sh: обёртка идёт
# к нему через launch_native.sh, а тот умеет и пересобрать, и подобрать
# отладочный запуск. Копировать бинарь в /usr/local/bin незачем — обновление
# тогда пришлось бы делать в двух местах, и «Super+R поднял старый бинарь»
# случалось бы каждый раз.
#
# ВНИМАНИЕ: имена переменных только латиницей — bash считает «имя=значение»
# с кириллицей КОМАНДОЙ, и присваивание молча превращается в «command not
# found» (эта грабля уже съедала chown в build.sh и весь migrate.sh).
set -euo pipefail

checkout="$(cd -- "$(dirname -- "$(realpath -- "$0")")/.." && pwd)"
bin_dir=${BIN_DIR:-/usr/local/bin}
sessions_dir=${SESSIONS_DIR:-/usr/share/wayland-sessions}
wrapper="$bin_dir/parallax-session"
entry="$sessions_dir/parallax.desktop"

if [ "${1:-}" = "--uninstall" ]; then
    rm -f "$wrapper" "$entry"
    echo "Убрано: $wrapper, $entry"
    exit 0
fi

if [ "$(id -u)" != 0 ]; then
    echo "Нужны права root: sudo $0" >&2
    exit 1
fi

if [ ! -x "$checkout/launch_native.sh" ]; then
    echo "Не вижу $checkout/launch_native.sh — запускайте скрипт из дерева исходников" >&2
    exit 1
fi

# Предупреждение, а не отказ: сессию удобно поставить заранее, а собрать потом.
if [ ! -x /mnt/plx-build/target/extra/release/plx-extra ] \
   && [ ! -x "$checkout/target/release/plx-extra" ] \
   && [ ! -x "$checkout/target/release/plx-standard" ]; then
    echo "ВНИМАНИЕ: собранного бинаря не видно — сначала ./build_portable.sh" >&2
fi

install -d "$bin_dir" "$sessions_dir"
# Путь к дереву подставляется здесь, а не читается обёрткой из окружения:
# у менеджера входа своего окружения почти нет, и $HOME в момент Exec= ещё
# не тот, что будет у сессии.
sed "s|@CHECKOUT@|$checkout|" "$checkout/dist/parallax-session" > "$wrapper"
chmod 755 "$wrapper"
install -m 644 "$checkout/dist/parallax.desktop" "$entry"

# Exec= в .desktop зашит на /usr/local/bin — при своём BIN_DIR его надо
# поправить, иначе менеджер входа позовёт несуществующий путь.
if [ "$bin_dir" != /usr/local/bin ]; then
    sed -i "s|/usr/local/bin/parallax-session|$wrapper|g" "$entry"
fi

echo "Поставлено:"
echo "  $wrapper  → $checkout/launch_native.sh"
echo "  $entry"
echo
echo "Сессия «Parallax» появится в списке менеджера входа после его перезапуска."
