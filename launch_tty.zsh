#!/usr/bin/env bash
# launch_tty.zsh — запуск Dawn БЕЗ openvt и без попытки остановить GNOME.
# (Скрипт на bash, несмотря на расширение .zsh — zsh в системе не установлен.)
# Использовать ТОЛЬКО когда вы уже физически залогинены на чистом TTY
# (Ctrl+Alt+F3/F4/..., логин без GDM) — то есть GNOME на этом VT не бежит,
# и logind сам должен корректно приостановить сессию GNOME на tty2 при
# активации этой сессии (без нужды в systemctl stop org.gnome.Shell@user.service,
# который всё равно отклоняется: RefuseManualStop=on в юните).
#
# Использование: ./launch_tty.zsh [--debug]

DAWN_DIR="$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)"
BINARY="$DAWN_DIR/target/release/dawn"
[[ "$1" == --debug ]] && BINARY="$DAWN_DIR/target/debug/dawn"

LOG_DIR="$DAWN_DIR/logs"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/dawn_tty_$(date +%Y%m%d_%H%M%S).log"

if [[ ! -x "$BINARY" ]]; then
    echo "Бинарь не найден: $BINARY" >&2
    exit 1
fi

if [[ -z "$XDG_RUNTIME_DIR" ]]; then
    XDG_RUNTIME_DIR="/run/user/$(id -u)"
fi

NIX_LD_HARDCODE="/nix/store/vcf7irc4an6ffxi1qin2kwv7qdggnfcr-libxkbcommon-1.13.1/lib:/nix/store/indd6wy8j1j62njhdq6m37rkajpvzc3v-wayland-1.24.0/lib:/nix/store/qsyg6xgqnsv4izp725hgx0q1gsmsdnjc-mesa-26.0.4/lib:/nix/store/1fy4004v7q0xi6c5jrr7ld2dinh22vy7-libglvnd-1.7.0/lib:/nix/store/hc43a4spns3ws92041kq53hf1f61zw8l-libdrm-2.4.131/lib:/nix/store/28sadwrjw8vpr7hk2rv52j24fh5m6961-mesa-libgbm-25.1.0/lib:/nix/store/fw5vnl9mlkfp9kdl9za7a3z0y21c552f-libdisplay-info-0.3.0/lib:/nix/store/fw02nk054hybn1swhrfgvln90rfav0iv-seatd-0.9.3/lib:/nix/store/bz0iyqml02szkbk1h2a37wl4nbl6irm1-systemd-minimal-libs-261/lib:/nix/store/ywvmyakqkzflp1mq6z0ly8widqrdgad5-libinput-1.31.3/lib"
export LD_LIBRARY_PATH="${LD_LIBRARY_PATH:-$NIX_LD_HARDCODE}"
export XCURSOR_PATH="${XCURSOR_PATH:-/run/current-system/sw/share/icons}"
export XCURSOR_THEME="${XCURSOR_THEME:-Adwaita}"
# Лог идёт через tee — синхронная запись на диск из единственного потока dawn,
# в котором крутится и рендер. На debug это было ~50 КБ/с прямо в рендер-лупе.
# Кадровые сообщения переведены в trace!, так что debug снова пригоден для
# отладки; при нужде: RUST_LOG=trace ./launch_tty.zsh
export RUST_LOG="${RUST_LOG:-debug}"
# По этому имени xdg-desktop-portal выбирает бэкенд (секция [dawn] в
# portals.conf → luminous, он умеет ext-image-copy-capture-v1, который
# реализует dawn). Без XDG_CURRENT_DESKTOP портал берёт бэкенд по умолчанию —
# в этой системе это kde, а его ScreenCast работает только под kwin, отсюда
# чёрный кадр в демонстрации экрана.
export XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-dawn}"
export XDG_SESSION_TYPE="${XDG_SESSION_TYPE:-wayland}"
# Electron (Vesktop/Discord) идёт в Wayland нативно только с этим флагом. Через
# Xwayland он снимает экран X11-путём — то есть пустое root-окно, тот самый
# чёрный кадр; портал он спрашивает только будучи wayland-клиентом.
export NIXOS_OZONE_WL="${NIXOS_OZONE_WL:-1}"

# Запуск dwall (wallpaper) — ждёт сокет wayland и стартует в фоне.
#
# WAYLAND_DISPLAY экспортируем ЗДЕСЬ, а не полагаемся на dawn: тот зовёт
# set_var уже после подъёма бэкенда, и это меняет окружение только внутри
# своего процесса. Подоболочка ниже — наш собственный ребёнок, порождённый
# раньше dawn, так что переменную она не унаследует ни при каком раскладе, и
# dwall падал с NoCompositor.
DWall="$HOME/.local/bin/dwall"
if [[ -x "$DWall" ]]; then
    (
        for i in $(seq 1 30); do
            [ -S "$XDG_RUNTIME_DIR/wayland-1" ] && break
            sleep 0.2
        done
        [ -S "$XDG_RUNTIME_DIR/wayland-1" ] || exit 0
        # Сокет появился — но dawn мог ещё не начать его обслуживать; connect
        # тогда отобьётся. Пробуем несколько раз, а не падаем с первой попытки.
        export WAYLAND_DISPLAY=wayland-1
        for i in $(seq 1 30); do
            "$DWall" && exit 0
            sleep 0.5
        done
    ) &
fi

# Портал и его бэкенды — не наши дети: их поднимает D-Bus/systemd --user, и
# окружение сессии они запоминают ОДИН раз при старте. Если они уже бегут от
# KDE-сессии (systemd --user у пользователя общий на все сессии), то смотрят в
# сокет kwin и выбирают kde-бэкенд. Гасим — dawn при старте отдаст своё
# окружение в шину (см. export_session_env), и первый же запрос поднимет их
# заново уже под dawn. Параллельной KDE-сессии это стоит лишь перезапуска
# портала при следующем обращении.
systemctl --user stop xdg-desktop-portal.service \
    xdg-desktop-portal-luminous.service \
    xdg-desktop-portal-kde.service \
    xdg-desktop-portal-gtk.service 2>/dev/null

echo "Бинарь:  $BINARY"
echo "Лог:     $LOG"
echo "Выход:   Super+Shift+Q"
echo ""

"$BINARY" --tty 2>&1 | tee "$LOG"

printf '\n═══ Разбор лога: %s ═══\n' "$LOG"
grep -q "DRM master" "$LOG" && echo "✔ DRM master получен" || echo "✗ DRM master НЕ получен"
if grep -q "error while loading shared libraries" "$LOG"; then
    echo "⚠ Бинарь не стартовал — не хватает библиотек:"
    grep "error while loading shared libraries" "$LOG"
fi
grep -ci "DeviceInactive" "$LOG" | xargs -I{} echo "DeviceInactive: {}"
panics=$(grep -ci "panic" "$LOG" 2>/dev/null || echo 0)
echo "Паники: $panics"
