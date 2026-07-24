#!/bin/bash
# Реальный тест на отдельном VT (с DRM master), безопасный через SSH:
# если dawn заморозит физический ввод, эта SSH-сессия останется живой отдельно.
set -e
DAWN_DIR="$HOME/dev/dawn"
LOG_DIR="$DAWN_DIR/logs"
mkdir -p "$LOG_DIR"
LOG="$LOG_DIR/vt_test_$(date +%Y%m%d_%H%M%S).log"

echo "Лог: $LOG"
echo "Выделяю новый VT и запускаю dawn --tty (без тайм-аута, до ручного выхода)..."

# Обязательно обновить тикет sudo ДО фонового запуска: если пароль устарел,
# фоновый sudo попытается прочитать его с tty и получит SIGTTIN — процесс
# молча зависнет (T-state) без единой строки в логе, будто dawn не стартовал.
sudo -v

# openvt сам переподключает stdin/stdout/stderr дочернего процесса к НОВОМУ VT
# (man openvt: "standard input, output and error are directed to that terminal"),
# так что внешний "> $LOG" ничего не поймает — вывод dawn улетит на консоль,
# а не в файл. Поэтому редирект делаем внутри — через bash -c на той стороне.
#
# sudo по умолчанию сбрасывает окружение (env_reset), XDG_RUNTIME_DIR теряется,
# а smithay::ListeningSocketSource::new_auto() без неё паникует с RuntimeDirNotSet.
# Пробрасываем значение явно, как литерал, снятое ДО sudo.
sudo openvt -s -- bash -c "XDG_RUNTIME_DIR='$XDG_RUNTIME_DIR' '$DAWN_DIR/target/release/dawn' --tty > '$LOG' 2>&1" &
OPENVT_PID=$!

echo "openvt PID: $OPENVT_PID"
echo "Тайм-аута нет — сиди сколько нужно. Alt+Q закрывает окно, Alt+Enter открывает kitty."
echo "Чтобы завершить dawn и вернуться сюда: с другого VT/SSH — pkill -f target/release/dawn"
wait "$OPENVT_PID" 2>/dev/null || true

sudo chown "$USER":"$USER" "$LOG" 2>/dev/null || true

echo ""
echo "═══════ РАЗБОР ЛОГА ($LOG) ═══════"
grep -q "DRM master acquired" "$LOG" && echo "✅ DRM master получен" || echo "❌ DRM master НЕ получен (см. лог)"
grep -i "permission denied" "$LOG" | head -5
echo ""
motion=$(grep -c "PTR MOTION" "$LOG" || echo 0)
panics=$(grep -ci "panic" "$LOG" || echo 0)
echo "Motion events: $motion"
echo "Паники: $panics"
[ "$panics" != "0" ] && grep -i -A5 "panic" "$LOG"
echo "════════════════════════════════════"
echo ""
echo "Если экран не вернулся сам — с этой SSH-сессии:"
echo "  sudo chvt \$(fgconsole 2>/dev/null || echo 2)   # вернуться на исходный VT"
echo "  pkill -9 -f target/release/dawn                 # убить если завис"
