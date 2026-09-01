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

TARGET_DIR=/mnt/dawn-build/target

cd "$(dirname "$0")"
cargo build --release --target-dir "$TARGET_DIR" "$@"

# Сборку зовут ДВА разных пользователя: человек (или Super+R внутри сессии) —
# от yarik, я при проверках — от root. Каталог сборки при этом ОДИН, и после
# прогона от root в нём остаются root'овые .o и .rlib: следующая сборка от
# yarik доходит до них и падает на «Permission denied» — rustc открывает
# готовый файл на запись, а не пересоздаёт его. Ровно так 23.08.2026 Super+R
# три минуты собирал, тихо откатился на прежний бинарь (rebuild_if_stale не
# фатален) и поднял сессию БЕЗ свежих правок — а выглядело это как «панель не
# появилась».
#
# Поэтому root, закончив, возвращает каталог владельцу репозитория. Обратная
# сторона (yarik портит сборку root'у) не важна: у root прав хватает всегда.
# ВНИМАНИЕ: имя переменной ЛАТИНИЦЕЙ. Кириллическое имя bash за имя переменной
# не считает — строка `хозяин="..."` выполняется как КОМАНДА и падает с
# «command not found», а `set -e` до неё не добирается (присваивание внутри
# if — не последняя команда конвейера). Итог: chown молча не выполнялся вовсе,
# и root оставлял за собой ровно тот каталог сборки, ради которого этот блок и
# написан. Поймано 23.08.2026: после сборки от root в /mnt/dawn-build/target
# лежало 3160 root'овых файлов.
if [[ $EUID -eq 0 ]]; then
    owner="$(stat -c %U:%G "$(dirname "$0")")"
    chown -R "$owner" "$TARGET_DIR" 2>/dev/null || true
fi

# dawn-share — терминальная команда раздачи. Кладём рядом с dwall, в
# ~/.local/bin ВЛАДЕЛЬЦА репозитория: при сборке от root $HOME — это /root, и
# команда тихо уехала бы туда, где её никто не ищет.
owner_user="$(stat -c %U "$(dirname "$0")")"
owner_home="$(getent passwd "$owner_user" | cut -d: -f6)"
bindir="${owner_home:-$HOME}/.local/bin"
mkdir -p "$bindir"
cp dawn-share "$bindir/.dawn-share.new"
mv -f "$bindir/.dawn-share.new" "$bindir/dawn-share"
chmod +x "$bindir/dawn-share"
[[ $EUID -eq 0 ]] && chown "$owner_user" "$bindir/dawn-share" 2>/dev/null

echo ""
echo "Binary: $TARGET_DIR/release/dawn"
echo "Команда: $bindir/dawn-share"
