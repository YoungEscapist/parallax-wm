#!/bin/bash
ANVIL="$HOME/.cargo/git/checkouts/smithay-312425d48e59d8c8/5312d5c/anvil"

echo "════════════════════════════════════════"
echo " anvil/src/main.rs — точка входа + event loop"
echo "════════════════════════════════════════"
cat "$ANVIL/src/main.rs" 2>/dev/null || echo "не найден main.rs"

echo ""
echo "════════════════════════════════════════"
echo " anvil/src/bin/tty_udev.rs (если есть отдельный бинарь)"
echo "════════════════════════════════════════"
find "$ANVIL" -name "*.rs" | xargs grep -l "event_loop.run\|EventLoop::try_new" 2>/dev/null

echo ""
echo "════════════════════════════════════════"
echo " Функция run_udev / инициализации в udev.rs"
echo "════════════════════════════════════════"
awk '/pub fn run_udev|pub fn init/,0' "$ANVIL/src/udev.rs" | head -150

echo ""
echo "════════════════════════════════════════"
echo " Финальный event_loop.run(...) в udev.rs"
echo "════════════════════════════════════════"
grep -n "event_loop.run\|\.run(" "$ANVIL/src/udev.rs"

echo ""
echo "════════════════════════════════════════"
echo " Session notifier init в udev.rs (insert_source для session)"
echo "════════════════════════════════════════"
grep -n -B2 -A25 "LibSeatSession::new\|insert_source.*session\|insert_source.*notifier" "$ANVIL/src/udev.rs" | head -80

echo ""
echo "════════════════════════════════════════"
echo " Libinput init + insert_source"
echo "════════════════════════════════════════"
grep -n -B2 -A15 "Libinput::new_with_udev\|LibinputInputBackend::new" "$ANVIL/src/udev.rs" | head -60

echo ""
echo "════════════════════════════════════════"
echo " DRM notifier per device (insert_source для DrmEvent)"
echo "════════════════════════════════════════"
grep -n -B3 -A40 "insert_source.*drm_notifier\|fn device_added\|insert_source.*DrmDevice" "$ANVIL/src/udev.rs" | head -100

echo ""
echo "════════════════════════════════════════"
echo " fn render — как рендерится кадр"
echo "════════════════════════════════════════"
grep -n -B2 -A60 "fn render(" "$ANVIL/src/udev.rs" | head -120

echo ""
echo "════════════════════════════════════════"
echo " Конец run_udev — event_loop.run/dispatch"
echo "════════════════════════════════════════"
tail -60 "$ANVIL/src/udev.rs"

echo ""
echo "════════════════════════════════════════"
echo " main.rs / lib.rs — где вызывается event_loop.run финально"
echo "════════════════════════════════════════"
grep -rn "event_loop.run\|\.dispatch(" "$ANVIL/src/"*.rs
