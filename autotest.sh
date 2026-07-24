#!/bin/bash
LOG=/tmp/dawn.log
sudo rm -f "$LOG"

timeout --signal=KILL 15 ~/dev/dawn/target/release/dawn --tty > "$LOG" 2>&1 &
DAWN_PID=$!
echo "dawn запущен (PID $DAWN_PID) — у тебя 15 секунд: подвигай мышкой, понажимай буквы, кликни по окну"
wait "$DAWN_PID" 2>/dev/null

sudo chown "$USER":"$USER" "$LOG" 2>/dev/null

echo ""
echo "═══════ АВТОМАТИЧЕСКИЙ РАЗБОР ═══════"
motion_rel=$(grep -c "PTR MOTION:" "$LOG")
motion_abs=$(grep -c "PTR MOTION ABS" "$LOG")
focus=$(grep -c "auto-focus new toplevel" "$LOG")
panics=$(grep -ci "panic" "$LOG")

echo "Relative motion events: $motion_rel"
echo "Absolute motion events: $motion_abs"
echo "Auto-focus сработал раз: $focus"
echo "Паники в логе: $panics"
echo ""
if [ "$motion_rel" = "0" ] && [ "$motion_abs" = "0" ]; then
    echo "⚠️  ВЫВОД: события мыши НЕ доходят до process_input_event вообще."
    echo "    Проблема в libinput/регистрации источника, а не в фокусе."
else
    echo "✅ ВЫВОД: события мыши доходят ($motion_rel rel / $motion_abs abs)."
fi
if [ "$panics" != "0" ]; then
    echo "⚠️  Есть паника — смотри полный лог:"
    grep -i -A5 "panic" "$LOG"
fi
echo "══════════════════════════════════"
