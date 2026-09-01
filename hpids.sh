#!/bin/bash
# Pid'ы процессов харнесса (свой XDG_RUNTIME_DIR — единственный надёжный
# признак: запуск идёт через `setsid runuser`, а `pgrep -f dawn` цепляет живой
# сеанс на tty7). Аргумент — подстрока командной строки, без него все.
#
#   ./hpids.sh            — все процессы харнесса
#   ./hpids.sh dwall      — только dwall
#
# 2>/dev/null обязателен: /proc обходится с гонкой, и мёртвые pid'ы иначе
# засыпают вывод сообщениями «No such process».
#
# Имена переменных — ТОЛЬКО латиницей: `имя=значение` с кириллицей bash
# выполняет как КОМАНДУ (та же грабля когда-то убила chown в build.sh, а здесь
# из-за неё скрипт молча ничего не убивал).
set -u
want=${1:-}
for p in /proc/[0-9]*; do
    tr '\0' '\n' < "$p/environ" 2>/dev/null \
        | grep -qx "XDG_RUNTIME_DIR=/tmp/dawn-harness/run" || continue
    cmd=$(tr '\0' ' ' < "$p/cmdline" 2>/dev/null)
    case "$cmd" in
        *"$want"*) echo "${p#/proc/} $cmd";;
    esac
done
