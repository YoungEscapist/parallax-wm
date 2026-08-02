#!/usr/bin/env bash
# launch.zsh — запуск Dawn compositor.
# (Скрипт на bash, несмотря на расширение .zsh — zsh в системе не установлен.)
# ВАЖНО: запускать через SSH (не из графической сессии).
# Использование: ./launch.zsh [--debug]

DAWN_DIR="$(cd -- "$(dirname -- "$(realpath -- "$0")")" && pwd)"
BINARY="$DAWN_DIR/target/release/dawn"
[[ "$1" == --debug ]] && BINARY="$DAWN_DIR/target/debug/dawn"

LOG_DIR="$DAWN_DIR/logs"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/dawn_$(date +%Y%m%d_%H%M%S).log"

if [[ ! -x "$BINARY" ]]; then
    echo "Бинарь не найден: $BINARY" >&2
    echo "Сборка: cd $DAWN_DIR && nix-shell --run 'cargo build --release'" >&2
    exit 1
fi

# XDG_RUNTIME_DIR нужен smithay; sudo сбрасывает окружение, поэтому берём заранее.
# ВАЖНО: сам скрипт запускать БЕЗ ведущего sudo (./launch.zsh, не sudo ./launch.zsh) —
# он сам поднимает привилегии внутри через sudo -v/sudo openvt. Если всё же
# запущен через sudo целиком, id -u вернёт 0 и XRT уедет в несуществующий
# /run/user/0 — поэтому здесь подстраховка через SUDO_UID.
REAL_UID="${SUDO_UID:-$(id -u)}"
XRT="${XDG_RUNTIME_DIR:-/run/user/$REAL_UID}"

# LD_LIBRARY_PATH: берём из nix-shell если активен, иначе хардкод из shell.nix
NIX_LD_HARDCODE="/nix/store/vcf7irc4an6ffxi1qin2kwv7qdggnfcr-libxkbcommon-1.13.1/lib:/nix/store/indd6wy8j1j62njhdq6m37rkajpvzc3v-wayland-1.24.0/lib:/nix/store/qsyg6xgqnsv4izp725hgx0q1gsmsdnjc-mesa-26.0.4/lib:/nix/store/1fy4004v7q0xi6c5jrr7ld2dinh22vy7-libglvnd-1.7.0/lib:/nix/store/hc43a4spns3ws92041kq53hf1f61zw8l-libdrm-2.4.131/lib:/nix/store/28sadwrjw8vpr7hk2rv52j24fh5m6961-mesa-libgbm-25.1.0/lib:/nix/store/fw5vnl9mlkfp9kdl9za7a3z0y21c552f-libdisplay-info-0.3.0/lib:/nix/store/fw02nk054hybn1swhrfgvln90rfav0iv-seatd-0.9.3/lib:/nix/store/bz0iyqml02szkbk1h2a37wl4nbl6irm1-systemd-minimal-libs-261/lib:/nix/store/ywvmyakqkzflp1mq6z0ly8widqrdgad5-libinput-1.31.3/lib"
NIX_LD="${LD_LIBRARY_PATH:-$NIX_LD_HARDCODE}"

# Обновить sudo-тикет сейчас, пока есть tty
sudo -v

# ── gnome-shell (не GDM целиком!) ────────────────────────────────────────────
# GDM/logind/сессию НЕ гасим — только сам композитор gnome-shell, который
# держит DRM master на тех же CRTC и периодически отвоёвывает его обратно у
# dawn (подтверждено 2026-07-26: рендер dawn чист по логам — commit+VBlank
# на обоих выходах без единой ошибки, — но экран всё равно перехватывался
# GDM при 2 мониторах). Раньше пробовали systemctl isolate multi-user.target
# — это гасило вообще всё и создавало гонку с logind, зависавшую намертво
# (см. logs/dawn_20260726_143932.log). Просто "не трогать GDM" (без остановки
# gnome-shell) тоже недостаточно при 2+ мониторах — сам gnome-shell продолжает
# бороться за master. Останавливаем только его, GDM/сессия остаются живы.
GNOME_UNIT="org.gnome.Shell@user.service"
# systemctl --user должен смотреть на СЕССИЮ yarik, а не на того, от чьего
# имени сейчас реально выполняется этот скрипт (если запущено как
# `sudo ./launch.zsh` целиком — а не как задумано, `./launch.zsh` без sudo —
# script выполняется от root, и голый `systemctl --user` бьёт в пустую
# root-сессию, молча ничего не делая). Поэтому всегда явно uid/пользователь.
REAL_USER="${SUDO_USER:-$USER}"
sysctl_user() { sudo -u "$REAL_USER" XDG_RUNTIME_DIR="/run/user/$REAL_UID" systemctl --user "$@"; }

GNOME_WAS_ACTIVE=false
if sysctl_user is-active --quiet "$GNOME_UNIT" 2>/dev/null; then
    GNOME_WAS_ACTIVE=true
    echo "Останавливаю $GNOME_UNIT (чтобы не боролся за DRM master)..."
    sysctl_user stop "$GNOME_UNIT"
fi

restore_gnome() {
    if [[ "$GNOME_WAS_ACTIVE" == true ]]; then
        echo "Возвращаю $GNOME_UNIT..."
        sysctl_user start "$GNOME_UNIT"
    fi
}
trap restore_gnome EXIT INT TERM

# ── Запуск Dawn ───────────────────────────────────────────────────────────────
echo "Бинарь:  $BINARY"
echo "Лог:     $LOG"
echo "Выход:   Super+Shift+Q   (или: sudo pkill -f target/release/dawn)"
echo ""

# openvt -s: открыть новый VT, переключить на него, запустить Dawn там
XCURSOR_P="${XCURSOR_PATH:-/run/current-system/sw/share/icons}"
XCURSOR_T="${XCURSOR_THEME:-Adwaita}"

sudo openvt -s -w -- bash -c \
    "XDG_RUNTIME_DIR='$XRT' LD_LIBRARY_PATH='$NIX_LD' XCURSOR_PATH='$XCURSOR_P' XCURSOR_THEME='$XCURSOR_T' RUST_LOG=debug '$BINARY' --tty >'$LOG' 2>&1"

sudo chown "$USER:$USER" "$LOG" 2>/dev/null || true

# openvt сам переключает VT обратно на исходный после выхода; restore_gnome
# (trap EXIT) поднимает gnome-shell обратно, даже если dawn упал/прибит kill'ом.

# ── Диагностика ───────────────────────────────────────────────────────────────
printf '\n═══ Разбор лога: %s ═══\n' "$LOG"

grep -q "DRM master" "$LOG" \
    && echo "✔ DRM master получен" \
    || echo "✗ DRM master НЕ получен"
if grep -q "error while loading shared libraries" "$LOG"; then
    echo "⚠ Бинарь не стартовал — не хватает библиотек:"
    grep "error while loading shared libraries" "$LOG"
fi

motion=$(grep -c "PTR MOTION" "$LOG" 2>/dev/null || echo 0)
panics=$(grep -ci "panic" "$LOG" 2>/dev/null || echo 0)
echo "Motion events: $motion  |  Паники: $panics"

if (( panics > 0 )); then
    printf '\n── Паника ──\n'
    grep -i -A 8 "panic" "$LOG" | head -40
fi

printf '\n── Последние 30 строк ──\n'
tail -30 "$LOG"
