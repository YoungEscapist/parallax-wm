#!/usr/bin/env bash
# install.sh — поставить Parallax одной командой: зависимости, Rust, сборка,
# конфиг, пункт в менеджере входа.
#
#   ./install.sh                    всё по умолчанию (сборка plx-standard)
#   ./install.sh --extra            собрать полную сборку
#   ./install.sh --both             обе
#   ./install.sh --update           git pull + пересборка того, что уже стоит
#   ./install.sh --uninstall        убрать сессию (дерево и конфиг не трогает)
#   ./install.sh --help             все ключи
#
# Скрипт работает и из клона, и «с нуля»:
#
#   curl -fsSL https://raw.githubusercontent.com/YoungEscapist/parallax-wm/master/install.sh | bash
#
# — во втором случае он сам клонирует дерево в ~/.local/src/parallax-wm и
# перезапускается внутри него.
#
# Что он НЕ делает намеренно:
#   * не копирует бинарь в /usr/bin — бинарь остаётся в дереве, куда его
#     положила сборка, и сессия ходит к нему через launch_native.sh (иначе
#     обновление пришлось бы делать в двух местах, см. dist/install-session.sh);
#   * не трогает существующий ~/.config/parallax/config.lua;
#   * ничего не ставит от root, кроме пакетов и двух файлов сессии, — и каждый
#     раз показывает команду, которую собирается выполнить.
#
# ВНИМАНИЕ: имена переменных только латиницей — bash считает «имя=значение»
# с кириллицей КОМАНДОЙ, и присваивание молча превращается в «command not
# found» (грабля, уже съедавшая chown в build.sh и весь migrate.sh).
set -euo pipefail

repo_url=${PLX_REPO:-https://github.com/YoungEscapist/parallax-wm.git}
src_dir_default=${PLX_SRC:-$HOME/.local/src/parallax-wm}

# ── Ключи ─────────────────────────────────────────────────────────────────────
want_standard=1
want_extra=0
do_deps=1
do_rust=1
do_build=1
do_config=1
do_session=1
do_update=0
do_uninstall=0
assume_yes=0
dry_run=0
native=0
profile=release
jobs=""

usage() {
    cat <<'EOF'
install.sh — установка Parallax из исходников.

Что ставить:
  --standard        только plx-standard (по умолчанию): композитор без шлема,
                    Minecraft, мультиюзера и шейдерных украшений
  --extra           только plx-extra: всё вышеперечисленное включено
  --both            обе сборки

Чего не делать:
  --no-deps         не ставить системные пакеты
  --no-rust         не проверять и не ставить rustup
  --no-build        не собирать (полезно с --session-only)
  --no-config       не класть ~/.config/parallax/config.lua
  --no-session      не регистрировать сессию в менеджере входа

Прочее:
  --native          собрать под ЭТОТ процессор (-C target-cpu=native);
                    быстрее, но бинарь перестаёт быть переносимым
  --quick           профиль quick вместо release: тот же оптимизированный код
                    с thin LTO — собирается в разы быстрее и проходит там, где
                    fat LTO не хватает памяти
  --jobs N          передать cargo -j N (на машине с малой памятью)
  --update          git pull в дереве и пересборка
  --uninstall       убрать пункт сессии (--purge — ещё и конфиг)
  -y, --yes         не задавать вопросов
  --dry-run         только показать, что будет сделано
  -h, --help        эта справка

Переменные:
  PLX_SRC   куда клонировать дерево при запуске через curl (по умолчанию
            ~/.local/src/parallax-wm)
  PLX_REPO  откуда клонировать
EOF
}

purge=0
explicit_choice=0
# Ключи запоминаются ДО разбора: при запуске через curl скрипт перезапускает
# себя внутри свежего клона, а к тому моменту `$@` уже съеден shift'ами — и
# `--extra`, `--quick`, `-y` молча терялись бы по дороге.
orig_args=("$@")
while [ $# -gt 0 ]; do
    case "$1" in
        --standard)   want_standard=1; want_extra=0; explicit_choice=1 ;;
        --extra)      want_standard=0; want_extra=1; explicit_choice=1 ;;
        --both)       want_standard=1; want_extra=1; explicit_choice=1 ;;
        --no-deps)    do_deps=0 ;;
        --no-rust)    do_rust=0 ;;
        --no-build)   do_build=0 ;;
        --no-config)  do_config=0 ;;
        --no-session) do_session=0 ;;
        --session-only) do_deps=0; do_rust=0; do_build=0; do_config=0 ;;
        --native)     native=1 ;;
        --quick)      profile=quick ;;
        --jobs)       jobs="${2:?--jobs требует число}"; shift ;;
        --jobs=*)     jobs="${1#*=}" ;;
        --update)     do_update=1 ;;
        --uninstall)  do_uninstall=1 ;;
        --purge)      do_uninstall=1; purge=1 ;;
        -y|--yes)     assume_yes=1 ;;
        --dry-run)    dry_run=1 ;;
        -h|--help)    usage; exit 0 ;;
        *) echo "Неизвестный ключ: $1 (см. --help)" >&2; exit 2 ;;
    esac
    shift
done

# ── Вывод ─────────────────────────────────────────────────────────────────────
if [ -t 1 ]; then
    b=$'\033[1m'; dim=$'\033[2m'; red=$'\033[31m'; yellow=$'\033[33m'; green=$'\033[32m'; off=$'\033[0m'
else
    b=""; dim=""; red=""; yellow=""; green=""; off=""
fi
step_no=0
step()  { step_no=$((step_no + 1)); printf '\n%s[%d/%d] %s%s\n' "$b" "$step_no" "$steps_total" "$1" "$off"; }
info()  { printf '  %s\n' "$1"; }
note()  { printf '  %s%s%s\n' "$dim" "$1" "$off"; }
warn()  { printf '  %s! %s%s\n' "$yellow" "$1" "$off" >&2; }
die()   { printf '\n%sОшибка: %s%s\n' "$red" "$1" "$off" >&2; exit 1; }
ok()    { printf '  %s✓ %s%s\n' "$green" "$1" "$off"; }

# Выполнить команду, показав её. При --dry-run — только показать.
run() {
    printf '  %s$ %s%s\n' "$dim" "$*" "$off"
    [ "$dry_run" = 1 ] && return 0
    "$@"
}

ask() {
    # ask "вопрос" — да по умолчанию; при -y и в неинтерактивном режиме «да».
    [ "$assume_yes" = 1 ] && return 0
    [ -t 0 ] || return 0
    local answer
    read -r -p "  $1 [Y/n] " answer || return 0
    case "$answer" in [nN]*) return 1 ;; *) return 0 ;; esac
}

# ── Привилегии ────────────────────────────────────────────────────────────────
# Пакеты и два файла сессии ставятся от root. Всё остальное — от обычного
# пользователя: собирать от root нельзя, иначе каталог сборки заполнится
# root'овыми .o и следующая сборка от человека упадёт на «Permission denied»
# (эта грабля уже стоила сеанса — см. комментарий в build.sh).
sudo_cmd=""
if [ "$(id -u)" = 0 ]; then
    sudo_cmd=""
elif command -v sudo >/dev/null 2>&1; then
    sudo_cmd="sudo"
elif command -v doas >/dev/null 2>&1; then
    sudo_cmd="doas"
fi
as_root() {
    if [ "$(id -u)" = 0 ]; then
        run "$@"
    elif [ -n "$sudo_cmd" ]; then
        run "$sudo_cmd" "$@"
    else
        warn "нет ни sudo, ни doas — выполните вручную от root:"
        printf '      %s\n' "$*"
        return 1
    fi
}

# ── Где дерево ────────────────────────────────────────────────────────────────
# Скрипт может быть запущен тремя способами: из клона, из скачанного файла
# рядом с клоном и из конвейера curl | bash (тогда $0 — это `bash`, и дерева
# нет вовсе).
self=${BASH_SOURCE[0]:-$0}
src=""
if [ -f "$self" ] && [ "$(basename -- "$self")" = install.sh ]; then
    candidate=$(cd -- "$(dirname -- "$(realpath -- "$self")")" && pwd)
    [ -f "$candidate/Cargo.toml" ] && [ -d "$candidate/src" ] && src="$candidate"
fi

if [ -z "$src" ]; then
    # Запуск через curl. Клонируем и передаём работу дереву — так человек
    # получает не только бинарь, но и исходники, из которых он собран:
    # обновление, конфиг по умолчанию и launch_native.sh живут там же.
    command -v git >/dev/null 2>&1 || die "нужен git (или запускайте install.sh из клона)"
    src="$src_dir_default"
    printf '%sParallax: дерева исходников рядом нет — клонирую.%s\n' "$b" "$off"
    if [ -d "$src/.git" ]; then
        info "уже есть: $src — обновляю"
        run git -C "$src" pull --ff-only
    else
        run mkdir -p "$(dirname -- "$src")"
        run git clone "$repo_url" "$src"
    fi
    [ "$dry_run" = 1 ] && { info "дальше запустился бы $src/install.sh"; exit 0; }
    exec bash "$src/install.sh" ${orig_args[@]+"${orig_args[@]}"}
fi

cd "$src"

# ── Снятие ────────────────────────────────────────────────────────────────────
if [ "$do_uninstall" = 1 ]; then
    steps_total=$((1 + purge))
    step "Убираю пункт сессии"
    as_root ./dist/install-session.sh --uninstall || true
    if [ "$purge" = 1 ]; then
        step "Убираю конфиг"
        run rm -rf "$HOME/.config/parallax"
    fi
    printf '\n%sГотово.%s Дерево исходников (%s) и собранные бинари остались — удалите их руками, если нужно.\n' \
        "$b" "$off" "$src"
    exit 0
fi

# ── Что именно ставим ─────────────────────────────────────────────────────────
# При --update без явного выбора сборки повторяем то, что уже собрано: человек
# просит «обнови», а не «передумай, что у меня стоит».
if [ "$do_update" = 1 ] && [ "$explicit_choice" = 0 ]; then
    want_standard=0; want_extra=0
    [ -x target/standard/release/plx-standard ] && want_standard=1
    [ -x target/extra/release/plx-extra ] && want_extra=1
    [ -x /mnt/plx-build/target/extra/release/plx-extra ] && want_extra=1
    if [ "$want_standard" = 0 ] && [ "$want_extra" = 0 ]; then want_standard=1; fi
fi

steps_total=0
[ "$do_update" = 1 ]  && steps_total=$((steps_total + 1))
[ "$do_deps" = 1 ]    && steps_total=$((steps_total + 1))
[ "$do_rust" = 1 ]    && steps_total=$((steps_total + 1))
[ "$do_build" = 1 ]   && steps_total=$((steps_total + 1))
[ "$do_config" = 1 ]  && steps_total=$((steps_total + 1))
[ "$do_session" = 1 ] && steps_total=$((steps_total + 1))
steps_total=$((steps_total + 1))   # проверка окружения

what=""
[ "$want_standard" = 1 ] && what="plx-standard"
[ "$want_extra" = 1 ] && what="${what:+$what и }plx-extra"

printf '%sParallax — установка из исходников%s\n' "$b" "$off"
note "дерево:  $src"
note "сборка:  ${what:-ничего (--no-build)}"
[ "$dry_run" = 1 ] && printf '  %s(--dry-run: ничего выполнено не будет)%s\n' "$yellow" "$off"

# ── 0. Окружение ──────────────────────────────────────────────────────────────
step "Проверяю окружение"
[ "$(uname -s)" = Linux ] || die "Parallax собирается только под Linux (нужны DRM/KMS, libseat, libinput)"

mem_kb=$(awk '/MemTotal/ {print $2}' /proc/meminfo 2>/dev/null || echo 0)
if [ "$mem_kb" -gt 0 ] && [ "$mem_kb" -lt 6000000 ]; then
    warn "памяти меньше 6 ГиБ: релизная сборка идёт с fat LTO и одним потоком линковки —"
    warn "если сборку убьёт OOM, повторите с --quick (thin LTO, линковка в несколько потоков)"
fi

if [ "$(id -u)" = 0 ] && [ -z "${PLX_ALLOW_ROOT:-}" ]; then
    warn "скрипт запущен от root: собранное дерево окажется root'овым, и следующая"
    warn "сборка от обычного пользователя упадёт на Permission denied."
    ask "Всё равно продолжить?" || die "запустите от своего пользователя — sudo он спросит сам"
fi
ok "Linux, $( (nproc 2>/dev/null || echo '?') ) ядер, $((mem_kb / 1024)) МиБ памяти"

# ── 1. Обновление дерева ──────────────────────────────────────────────────────
if [ "$do_update" = 1 ]; then
    step "Обновляю дерево"
    if [ -d .git ]; then
        if [ -n "$(git status --porcelain 2>/dev/null)" ]; then
            warn "в дереве есть незакоммиченные правки — pull может не пройти"
        fi
        run git pull --ff-only || warn "git pull не прошёл — собираю то, что есть"
    else
        warn "это не git-клон: обновлять нечего"
    fi
fi

# ── 2. Системные зависимости ──────────────────────────────────────────────────
if [ "$do_deps" = 1 ]; then
    step "Системные зависимости"
    if ./dist/install-deps.sh --print >/dev/null 2>&1; then
        ./dist/install-deps.sh --print | sed 's/^/  /'
        if ask "Поставить эти пакеты?"; then
            as_root ./dist/install-deps.sh || die "не удалось поставить пакеты"
            ok "пакеты на месте"
        else
            note "пропущено — сборка упадёт, если чего-то не хватает"
        fi
    else
        warn "пакетный менеджер не опознан (не Void, Arch, Debian/Ubuntu или Fedora)."
        warn "Список нужных библиотек — в INSTALL.md, раздел «Зависимости»."
        warn "На NixOS: nix-shell, дальше ./build.sh."
        ask "Продолжить без установки пакетов?" || exit 1
    fi
fi

# ── 3. Rust ───────────────────────────────────────────────────────────────────
if [ "$do_rust" = 1 ]; then
    step "Rust"
    # rustup кладёт cargo в ~/.cargo/bin, а PATH обновляется только в новом
    # шелле — поэтому ищем его и там тоже, иначе скрипт, только что поставивший
    # rustup, тут же сообщит «cargo не найден».
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    if command -v cargo >/dev/null 2>&1; then
        rust_ver=$(rustc --version 2>/dev/null | awk '{print $2}')
        ok "cargo есть, rustc $rust_ver"
        rust_major=${rust_ver%%.*}
        rust_minor=$(printf '%s' "$rust_ver" | cut -d. -f2)
        if [ "${rust_major:-0}" = 1 ] && [ "${rust_minor:-0}" -lt 82 ] 2>/dev/null; then
            warn "rustc $rust_ver старее 1.82 — smithay и часть крейтов могут не собраться."
            warn "Свежий: rustup update stable (или пакет rust вашего дистрибутива)."
        fi
    else
        warn "cargo не найден"
        if ask "Поставить Rust через rustup (rustup.rs, ставится в ~/.rustup и ~/.cargo)?"; then
            command -v curl >/dev/null 2>&1 || die "нужен curl, чтобы скачать rustup"
            if [ "$dry_run" = 1 ]; then
                note '$ curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
            else
                curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
                PATH="$HOME/.cargo/bin:$PATH"
            fi
            command -v cargo >/dev/null 2>&1 || [ "$dry_run" = 1 ] || die "rustup поставился, а cargo не нашёлся"
        else
            die "без cargo собрать нечего"
        fi
    fi
fi

# ── 4. Сборка ─────────────────────────────────────────────────────────────────
if [ "$do_build" = 1 ]; then
    step "Сборка"
    [ -d "$HOME/.cargo/bin" ] && PATH="$HOME/.cargo/bin:$PATH"
    command -v cargo >/dev/null 2>&1 || die "cargo не найден (--no-rust пропустил проверку?)"

    # Линкер и набор инструкций. В .cargo/config.toml лежат ПЕРЕНОСИМЫЕ
    # настройки, а всё, что зависит от машины, добавляется здесь — иначе
    # посторонний клон не собирается без mold, а бинарь, собранный под этот
    # процессор, падает с SIGILL на соседнем.
    build_flags=""
    if command -v mold >/dev/null 2>&1; then
        build_flags="-C link-arg=-fuse-ld=mold"
        note "линкер: mold"
    elif command -v ld.lld >/dev/null 2>&1; then
        build_flags="-C link-arg=-fuse-ld=lld"
        note "линкер: lld"
    else
        note "линкер: системный ld (mold ускорил бы сборку в разы)"
    fi
    if [ "$native" = 1 ]; then
        build_flags="$build_flags -C target-cpu=native"
        note "код под ЭТОТ процессор: бинарь не переносим на другое железо"
    fi

    # Экспортом, а не присваиванием перед вызовом: перед ФУНКЦИЕЙ такое
    # присваивание в bash остаётся в шелле и после возврата, то есть выглядит
    # как временное, а работает как постоянное. И человек должен видеть флаги,
    # с которыми собирают его машину.
    export RUSTFLAGS="$build_flags"
    [ -n "$build_flags" ] && note "RUSTFLAGS=$build_flags"

    cargo_args=()
    [ -n "$jobs" ] && cargo_args+=(-j "$jobs")

    build_one() {
        # $1 — имя крейта, $2 — каталог сборки. Каталоги РАЗНЫЕ и вызовы
        # РАЗДЕЛЬНЫЕ: одной командой cargo объединил бы наборы фич членов
        # workspace и положил бы vr/mine/share в обе сборки (поймано замером —
        # два бинаря по 17.8 МиБ байт в байт), а общий каталог заставлял бы
        # каждый вызов пересобирать всё заново, потому что отпечаток включает
        # набор фич. Подробности — в build.sh и README.
        info "$1 → target/$2/$profile/$1"
        run cargo build --profile "$profile" \
            --target-dir "target/$2" -p "$1" ${cargo_args[@]+"${cargo_args[@]}"}
    }

    build_started=$(date +%s)
    [ "$want_standard" = 1 ] && build_one plx-standard standard
    [ "$want_extra" = 1 ] && build_one plx-extra extra

    # Оба бинаря складываются ещё и в target/release — туда за ними ходят
    # launch_native.sh и README.
    if [ "$dry_run" != 1 ]; then
        mkdir -p target/release
        [ "$want_standard" = 1 ] && cp -f "target/standard/$profile/plx-standard" target/release/plx-standard
        [ "$want_extra" = 1 ] && cp -f "target/extra/$profile/plx-extra" target/release/plx-extra
        ok "собрано за $(( $(date +%s) - build_started )) с"
    fi
fi

# ── 5. Конфиг ─────────────────────────────────────────────────────────────────
if [ "$do_config" = 1 ]; then
    step "Конфиг"
    config_dir="$HOME/.config/parallax"
    config="$config_dir/config.lua"
    if [ -e "$config" ]; then
        note "уже есть, не трогаю: $config"
    else
        # Русский конфиг — тому, у кого русская локаль: комментарии в нём и
        # есть документация, и читать её на чужом языке незачем.
        source_config=default_config.lua
        case "${LANG:-}" in ru_*|ru) source_config=default_config.ru.lua ;; esac
        run mkdir -p "$config_dir"
        run cp "$source_config" "$config"
        ok "положил $config (из $source_config)"
    fi
    note "правки применяются на лету — сохранили файл, конфиг перечитался"
fi

# ── 6. Сессия ─────────────────────────────────────────────────────────────────
if [ "$do_session" = 1 ]; then
    step "Пункт в менеджере входа"
    info "ставятся ровно два файла: /usr/local/bin/parallax-session и"
    info "/usr/share/wayland-sessions/parallax.desktop"
    if ask "Поставить?"; then
        as_root ./dist/install-session.sh || warn "не удалось — можно позже: sudo ./dist/install-session.sh"
    else
        note "пропущено — запускать можно и руками: ./launch_native.sh с чистого TTY"
    fi
fi

# ── Итог ──────────────────────────────────────────────────────────────────────
printf '\n%sГотово.%s\n' "$green$b" "$off"
cat <<EOF

  Запуск с чистого TTY (Ctrl+Alt+F3, войти, и там):
      cd $src && ./launch_native.sh

  Или выбрать «Parallax» в менеджере входа.
  Вложенным окном внутри текущего сеанса (для проб):
      cd $src && ./launch_native.sh --winit

  Выход — Super+Shift+Q, перезапуск на месте — Super+R, логи — $src/logs/.
  Конфиг — ~/.config/parallax/config.lua; все ключи разобраны комментариями
  в нём же и в разделе «Configuration» README.md.
  Обновиться потом: cd $src && ./install.sh --update

EOF
