#!/bin/bash
# Поднять headless-харнесс dawn: свой HOME, свой XDG_RUNTIME_DIR, своя шина
# D-Bus и управляющий сокет. Экрана и ввода у него нет, кадр уходит в PNG по
# команде — то есть живой сеанс на tty7 он не задевает вовсе и проверять можно
# прямо во время работы (или игры).
#
#   ./harness.sh                 — поднять заново (один монитор 2560x1080)
#   MODE=1920x1280@60,2560x1080@60 ./harness.sh   — два монитора, как у Ярика
#   ./ctl.py 'shot /tmp/к.png'   — снять кадр (при двух мониторах ещё и к-2.png)
#   ./ctl.py windows             — что за окна и какого они размера
#   ./ctl.py 'mouse to 300 23'   — навести курсор (экранные пиксели)
#   ./ctl.py 'vr on'             — надеть шлем (нужен рантайм OpenXR, см. ниже)
#
# ШЛЕМ БЕЗ ШЛЕМА. VR-режим (src/vr/) проверяется прямо здесь, на симуляторе
# Monado — ни Quest, ни живого сеанса не нужно:
#
#   # 1) поднять симулятор в ТОМ ЖЕ XDG_RUNTIME_DIR, что и харнесс:
#   setsid runuser -u yarik -- bash -c 'sleep 100000 | env \
#     HOME=/tmp/dawn-harness/home XDG_RUNTIME_DIR=/tmp/dawn-harness/run \
#     XRT_COMPOSITOR_NULL=1 XRT_DRIVER_SIMULATED=1 monado-service' &
#   # 2) поднять харнесс, указав ему рантайм:
#   XR_RUNTIME_JSON=/usr/local/share/openxr/1/openxr_monado.json ./harness.sh
#   # 3) ./ctl.py 'vr on' ; ./ctl.py 'vr panels'
#
# Две грабли симулятора: ему нужен НАСТОЯЩИЙ stdin (отсюда `sleep | …` —
# на /dev/null epoll не работает), и сокет он кладёт в XDG_RUNTIME_DIR, так
# что у службы и у харнесса он обязан быть один.
#   ./ctl.py help                — весь список команд
#
# ВНИМАНИЕ: имена переменных только латиницей — bash считает «имя=значение»
# с кириллицей КОМАНДОЙ (та же грабля когда-то убила chown в build.sh).
set -u
WLDEBUG=${WLDEBUG:-}
root=/tmp/dawn-harness
bin=/mnt/dawn-build/target/release/dawn
user=${SUDO_USER:-yarik}
# Режимы через запятую = столько мониторов. Двухмониторный прогон обязателен
# для всего, что считает холст: у второго монитора дом (1 000 000, 0), и
# ошибка «от нуля холста» на первом невидима (см. monitors::ШАГ_ДОМА).
mode=${MODE:-2560x1080@60}
# Имена коннекторов для этих режимов (через запятую, тот же порядок) — под
# ними ищутся monitor{} из config.lua (позиция, primary, tag). Без переменной
# выходы называются headless-1/headless-2 и под конфиг не подставляются.
names=${NAMES:-}

# Убивать перебором окружения, а не по имени: запуск идёт через
# `setsid runuser`, записанный pid — обёртка, а `pkill -f dawn` рискует поймать
# живой сеанс на tty7. Свой XDG_RUNTIME_DIR — признак ровно харнесса.
# ГРАБЛЯ: рантайм OpenXR (monado-service/wivrn-server) сидит в ТОМ ЖЕ
# XDG_RUNTIME_DIR — иначе dawn не найдёт его IPC-сокет. Под этот перебор он
# попадал вместе с харнессом, и перезапуск харнесса молча уносил симулятор:
# следующий `vr on` отвечал XR_ERROR_RUNTIME_UNAVAILABLE на ровном месте.
# Поэтому рантайм из отстрела исключён по имени — он переживает перезапуск.
for p in /proc/[0-9]*; do
    if tr '\0' '\n' < "$p/environ" 2>/dev/null | grep -qx "XDG_RUNTIME_DIR=$root/run"; then
        case "$(cat "$p/comm" 2>/dev/null)" in
            monado-service|wivrn-server) continue ;;
        esac
        kill "${p#/proc/}" 2>/dev/null
    fi
done
sleep 1

mkdir -p "$root/run" "$root/shots" "$root/home"
# Свой config.lua: харнесс не обязан повторять живой, но по умолчанию удобно
# смотреть ровно то, что видит Ярик.
if [ ! -f "$root/home/.config/dawn/config.lua" ]; then
    mkdir -p "$root/home/.config"
    cp -r "/home/$user/.config/dawn" "$root/home/.config/" 2>/dev/null
    cp -r "/home/$user/.config/dwall" "$root/home/.config/" 2>/dev/null
    cp -r "/home/$user/.config/ghostty" "$root/home/.config/" 2>/dev/null
fi
# 2>/dev/null: в run/ остаются каталоги от прошлой шины D-Bus, до которых root
# не всегда дотягивается, — на дело это не влияет.
chown -R "$user:$user" "$root/run" "$root/shots" "$root/home" 2>/dev/null
chmod 700 "$root/run"

# dbus-run-session даёт харнессу СВОЮ сессионную шину. Без неё
# DAWN_HEADLESS_SERVICES включать нельзя: полка держит имя
# org.kde.StatusNotifierWatcher и на общей шине отняла бы трей у живого dawn.
# Профили Nix — как в живом сеансе. Значки nix-приложений (AyuGram) лежат в
# `<профиль>/share/icons`, и без этой переменной харнесс не воспроизводил бы
# ровно ту беду, которую чиним: панель рисует букву вместо значка.
nix_profiles=${NIX_PROFILES:-"/nix/var/nix/profiles/default /home/$user/.nix-profile"}
setsid runuser -u "$user" -- env -i \
  HOME="$root/home" USER="$user" PATH=/usr/local/bin:/usr/bin:/usr/sbin:/bin \
  NIX_PROFILES="$nix_profiles" \
  XCURSOR_THEME="${XCURSOR_THEME:-Bibata-Modern-Ice}" \
  XCURSOR_SIZE="${XCURSOR_SIZE:-24}" \
  XDG_RUNTIME_DIR="$root/run" \
  DAWN_HEADLESS_MODE="$mode" \
  ${names:+DAWN_HEADLESS_NAMES="$names"} \
  DAWN_HEADLESS_SERVICES=1 \
  ${WLDEBUG:+WAYLAND_DEBUG=1} \
  TZ=Europe/Riga \
  ${XR_RUNTIME_JSON:+XR_RUNTIME_JSON="$XR_RUNTIME_JSON"} \
  RUST_LOG=dawn=debug,info \
  dbus-run-session -- "$bin" --headless \
  > "$root/dawn.log" 2>&1 &

sleep 4
grep -a "dawn socket\|dawn/ctl" "$root/dawn.log" | tail -2
echo "лог: $root/dawn.log, снимки: $root/shots"
echo "обои: ./ctl.py 'action spawn cmd=\"/home/$user/.local/bin/dwall\"'"
