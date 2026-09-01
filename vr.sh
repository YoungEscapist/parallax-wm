#!/bin/bash
# Поднять VR-сессию dawn: сервер WiVRn на ПК + шлем Quest 3 по Wi-Fi.
#
#   ./vr.sh            — поднять сервер и ждать шлем
#   ./vr.sh --status   — что сейчас: сервер, рантайм, подключён ли шлем
#   ./vr.sh --stop     — погасить сервер
#   ./vr.sh --client   — поставить клиент на Quest по USB (нужен adb и режим
#                        разработчика на шлеме)
#
# Как это устроено целиком:
#
#   dawn (OpenXR-клиент)  ──►  libopenxr_wivrn.so  ──►  wivrn-server (Monado)
#                                                            │ Wi-Fi, H.264/265
#                                                            ▼
#                                                    WiVRn на Quest 3
#
# То есть dawn не знает про Wi-Fi и кодеки вовсе: он отдаёт кадр стандартному
# рантайму OpenXR, а всё остальное — забота WiVRn. Ровно поэтому VR-режим
# работает и с проводным шлемом, и с симулятором Monado (см. harness.sh).
#
# ВНИМАНИЕ: имена переменных только латиницей (кириллическое «имя=значение»
# bash выполняет как команду — та же грабля, что когда-то убила chown в
# build.sh).
set -u

runtime_dir=/usr/local/share/openxr/1
wivrn_json="$runtime_dir/openxr_wivrn.json"
server=/usr/local/bin/wivrn-server
apk=/home/yarik/WiVRn-client-v26.6.2.apk
user=${SUDO_USER:-yarik}

состояние() {
    echo "── VR ──"
    if [ -x "$server" ]; then echo "сервер:   $server"; else echo "сервер:   НЕ УСТАНОВЛЕН"; fi
    if [ -f "$wivrn_json" ]; then echo "рантайм:  $wivrn_json"; else echo "рантайм:  НЕТ"; fi
    local active=/etc/xdg/openxr/1/active_runtime.json
    if [ -e "$active" ]; then
        echo "активный: $(readlink -f "$active")"
    else
        echo "активный: не задан (dawn возьмёт XR_RUNTIME_JSON)"
    fi
    if pgrep -x wivrn-server >/dev/null; then
        echo "процесс:  идёт (pid $(pgrep -x wivrn-server | tr '\n' ' '))"
    else
        echo "процесс:  не запущен"
    fi
    # Шлем виден как подключённый клиент — спрашиваем у самого сервера.
    if command -v wivrnctl >/dev/null 2>&1; then
        wivrnctl status 2>/dev/null | sed 's/^/шлем:     /'
    fi
}

case "${1:-}" in
    --status) состояние; exit 0 ;;
    --stop)
        pkill -x wivrn-server && echo "сервер остановлен" || echo "сервер и так не идёт"
        exit 0
        ;;
    --client)
        if ! command -v adb >/dev/null 2>&1; then
            echo "нет adb: xbps-install android-tools"; exit 1
        fi
        if [ ! -f "$apk" ]; then
            echo "нет $apk — скачать:"
            echo "  curl -L -o $apk https://github.com/WiVRn/WiVRn/releases/download/v26.6.2/WiVRn-release.apk"
            exit 1
        fi
        echo "Шлем должен быть в режиме разработчика и подключён по USB."
        adb devices
        adb install -r "$apk"
        exit $?
        ;;
esac

if [ ! -x "$server" ]; then
    echo "wivrn-server не установлен. Собрать: см. ~/Projects/wivrn и docs/building.md"
    exit 1
fi

# Активный рантайм OpenXR. Без него каждый клиент обязан сам знать
# XR_RUNTIME_JSON; с ним dawn (и любая игра) находит WiVRn сам.
if [ ! -e /etc/xdg/openxr/1/active_runtime.json ]; then
    echo "делаю WiVRn активным рантаймом OpenXR"
    mkdir -p /etc/xdg/openxr/1
    ln -sf "$wivrn_json" /etc/xdg/openxr/1/active_runtime.json
fi

if pgrep -x wivrn-server >/dev/null; then
    echo "сервер уже идёт"
else
    # Своё окружение сессии: сервер должен видеть тот же XDG_RUNTIME_DIR, что и
    # dawn, иначе клиент не найдёт его IPC-сокет.
    runtime=${XDG_RUNTIME_DIR:-/tmp/runtime-1000}
    echo "поднимаю wivrn-server (XDG_RUNTIME_DIR=$runtime)"
    # stdin обязателен НАСТОЯЩИЙ: Monado следит за ним через epoll, а на
    # /dev/null epoll_ctl отвечает отказом и служба падает при старте.
    setsid runuser -u "$user" -- bash -c \
        "sleep infinity | env XDG_RUNTIME_DIR='$runtime' '$server'" \
        > /tmp/wivrn-server.log 2>&1 &
    sleep 3
    if pgrep -x wivrn-server >/dev/null; then
        echo "сервер поднят, лог: /tmp/wivrn-server.log"
    else
        echo "сервер не поднялся, смотри /tmp/wivrn-server.log"; exit 1
    fi
fi

cat <<'КОНЕЦ'

Дальше:
  1. надень Quest 3, запусти на нём приложение WiVRn;
  2. в списке выбери этот компьютер (он объявляет себя по сети сам);
  3. в dawn нажми Win+Alt+V — окна разъедутся панелями по комнате.

Внутри шлема:
  Win+Alt+A — passthrough (окна поверх настоящей комнаты);
  Win+Alt+G — раскладка: дуга → стена → купол → свободно;
  Win+Alt+H — собрать панели заново вокруг взгляда;
  курок контроллера — левая кнопка мыши, хват — тащить панель,
  стик вперёд/назад — ближе/дальше, вбок — размер.
  Клавиатура и мышь работают как обычно.
КОНЕЦ
