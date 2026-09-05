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
# запуске (см. dmesg: "parallax[...] general protection fault ... in libc.so.6"),
# без единой строчки в логе — падало до init tracing_subscriber.
#
# Фикс: не использовать nix-shell для сборки вообще. В системе (Void, xbps)
# уже есть все нужные -dev пакеты и .pc-файлы (wayland-client, libinput,
# libseat, libdrm, gbm, egl, glesv2, libdisplay-info, libudev, xkbcommon) —
# pkg-config находит их сам без PKG_CONFIG_PATH. mold ставится штатно:
# `sudo xbps-install mold`.

TARGET_DIR=/mnt/plx-build/target

cd "$(dirname "$0")"

# Собираются ОБА бинаря: plx-standard (без шлема, Minecraft и мультиюзера) и
# plx-extra (со всем). Это не два разных исходника, а один и тот же крейт с
# разным набором фич — см. `[features]` в Cargo.toml и заглушки в src/*_stub/.
#
# ДВА ОТДЕЛЬНЫХ ВЫЗОВА, а не один `--workspace`, и каталоги сборки тоже разные.
# Причина не в аккуратности, а в том, что иначе разделения НЕ ПРОИСХОДИТ вовсе:
# собирая несколько членов workspace разом, cargo ОБЪЕДИНЯЕТ их наборы фич и
# строит общую библиотеку по сумме — то есть с `vr`, `mine` и `share`. Поймано
# замером: после `--workspace` оба бинаря весили 17.8 МиБ байт в байт, и в
# «стандартном» лежали 137 строк openxr. Раздельные каталоги нужны потому, что
# отпечаток сборки включает набор фич: в общем каталоге каждый вызов вытеснял
# бы предыдущий и пересобирал библиотеку целиком (fat LTO — это минуты).
cargo build --release --target-dir "$TARGET_DIR/standard" -p plx-standard "$@"
cargo build --release --target-dir "$TARGET_DIR/extra"   -p plx-extra   "$@"

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
# написан. Поймано 23.08.2026: после сборки от root в /mnt/plx-build/target
# лежало 3160 root'овых файлов.
if [[ $EUID -eq 0 ]]; then
    owner="$(stat -c %U:%G "$(dirname "$0")")"
    chown -R "$owner" "$TARGET_DIR" 2>/dev/null || true
fi

# plx-host — терминальная команда раздачи. Кладём рядом с plx-wall, в
# ~/.local/bin ВЛАДЕЛЬЦА репозитория: при сборке от root $HOME — это /root, и
# команда тихо уехала бы туда, где её никто не ищет.
owner_user="$(stat -c %U "$(dirname "$0")")"
owner_home="$(getent passwd "$owner_user" | cut -d: -f6)"
bindir="${owner_home:-$HOME}/.local/bin"
mkdir -p "$bindir"
cp plx-host "$bindir/.plx-host.new"
mv -f "$bindir/.plx-host.new" "$bindir/plx-host"
chmod +x "$bindir/plx-host"
[[ $EUID -eq 0 ]] && chown "$owner_user" "$bindir/plx-host" 2>/dev/null

echo ""
echo "Бинари:"
echo "  $TARGET_DIR/standard/release/plx-standard"
echo "  $TARGET_DIR/extra/release/plx-extra"
echo "Команда: $bindir/plx-host"
