#!/bin/bash
set -e
LOG=/tmp/dawn.log
STRACE_LOG=/tmp/dawn_strace.log
sudo rm -f "$LOG" "$STRACE_LOG"

echo "XDG_RUNTIME_DIR=$XDG_RUNTIME_DIR"

timeout --signal=KILL 15 ~/dev/dawn/target/release/dawn --tty > "$LOG" 2>&1 &
DAWN_PID=$!

sleep 2
if kill -0 "$DAWN_PID" 2>/dev/null; then
    echo "dawn жив (PID $DAWN_PID), снимаю strace на 8 секунд..."
    sudo timeout 8 strace -f -p "$DAWN_PID" -tt \
        -e trace=poll,ppoll,epoll_wait,ioctl,read,write \
        -o "$STRACE_LOG" 2>/dev/null || true
else
    echo "dawn уже упал за первые 2 секунды — смотри $LOG"
fi

wait "$DAWN_PID" 2>/dev/null || true
sudo chown "$USER":"$USER" "$LOG" "$STRACE_LOG" 2>/dev/null || true
echo "── Готово ──"
echo "Логи: $LOG"
echo "Strace: $STRACE_LOG"
