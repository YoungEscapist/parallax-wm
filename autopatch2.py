#!/usr/bin/env python3
import re, shutil, subprocess, sys
from pathlib import Path

SRC = Path.home() / "dev/dawn/src/udev.rs"
BAK = SRC.with_suffix(".rs.bak2")
shutil.copy2(SRC, BAK)
print(f"✅ Backup: {BAK}")

code = SRC.read_text()
original = code

# ──────────────────────────────────────────────────────────
# ФИКС 1: DrmDevice::new(fd, false) → true (как в anvil)
# disable_connectors=true — безопасный дефолт, не требует
# ручного управления connector state
# ──────────────────────────────────────────────────────────
old = 'let (drm, notifier) = DrmDevice::new(device_fd.clone(), false)?;'
new = 'let (drm, notifier) = DrmDevice::new(device_fd.clone(), true)?;'
if old in code:
    code = code.replace(old, new)
    print("✅ ФИКС 1: DrmDevice::new(..., false) → (..., true)")
else:
    print("⚠️  ФИКС 1: строка не найдена (проверь вручную)")

# ──────────────────────────────────────────────────────────
# ФИКС 2: рендерить СРАЗУ после add_surface, а не ждать
# ActivateSession/Timer. Master уже выдан при session.open()
# пока сессия активна (как в anvil — device_added не ждёт).
# ──────────────────────────────────────────────────────────
old_marker = 'tracing::info!("dawn/udev: device ready, waiting for ActivateSession");'
new_block = '''let node_render = node;
                event_loop.handle().insert_idle(move |state| {
                    tracing::info!("dawn/udev: initial render (idle)");
                    let mut devices = std::mem::take(&mut state.udev_devices);
                    if let Some(dev) = devices.get_mut(&node_render) {
                        let crtcs: Vec<_> = dev.surfaces.keys().cloned().collect();
                        for crtc in crtcs {
                            if let Some(surface) = dev.surfaces.get_mut(&crtc) {
                                let gles = &mut dev.gles as *mut GlesRenderer;
                                unsafe { render_surface(surface, &mut *gles, state); }
                            }
                        }
                    }
                    state.udev_devices = devices;
                });'''

if old_marker in code:
    code = code.replace(old_marker, new_block)
    print("✅ ФИКС 2: немедленный рендер через insert_idle (без ожидания ActivateSession)")
else:
    print("⚠️  ФИКС 2: маркер не найден — проверь, был ли применён autopatch.py ранее")
    print("    Ищу альтернативный паттерн (Timer ещё не удалён)...")
    # Fallback: если Timer ещё присутствует, заменяем его целиком
    timer_pattern = re.compile(
        r'event_loop\.handle\(\)\.insert_source\(\s*'
        r'Timer::from_duration\(Duration::from_millis\(100\)\),.*?'
        r'\)\.unwrap\(\);',
        re.DOTALL
    )
    code, n = timer_pattern.subn(new_block.strip(), code)
    if n:
        print(f"✅ ФИКС 2 (fallback): Timer заменён на insert_idle ({n} замен)")
    else:
        print("❌ ФИКС 2: не удалось найти ни один паттерн — нужна ручная правка")

# ──────────────────────────────────────────────────────────
if code != original:
    SRC.write_text(code)
    print("\n✅ udev.rs обновлён")
else:
    print("\n⚠️  Файл не изменился")

print("\n── cargo build --release ──")
r = subprocess.run(["cargo", "build", "--release"], cwd=Path.home()/"dev/dawn",
                    capture_output=True, text=True)
if r.returncode == 0:
    print("✅ Сборка успешна!")
else:
    print("❌ Ошибки сборки:")
    for l in r.stderr.split('\n'):
        if 'error' in l:
            print(f"   {l}")
    print("\n── Откат ──")
    shutil.copy2(BAK, SRC)
    print(f"✅ Откат: {BAK} → {SRC}")
    sys.exit(1)

print("\n═══════════════════════════════════")
print(" Патчи применены, сборка OK")
print(" Тест: dawn 8")
print("═══════════════════════════════════")
