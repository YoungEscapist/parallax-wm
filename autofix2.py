import re, shutil, subprocess, sys
from pathlib import Path

DAWN = Path.home() / "dev/dawn"
UDEV_RS = DAWN / "src/udev.rs"

b = UDEV_RS.with_suffix(".rs.bak_autofix2")
shutil.copy2(UDEV_RS, b)
print(f"backup: {b}")

code = UDEV_RS.read_text()

old = "                state.udev_devices.insert(node, device);\n                // linux-dmabuf global"
new = """                state.udev_devices.insert(node, device);

                // Явно пробуем стать master сразу — не ждём ActivateSession,
                // который может не прийти, если сессия уже была активна при старте
                if let Some(dev) = state.udev_devices.get_mut(&node) {
                    match dev.drm.activate(false) {
                        Ok(()) => tracing::info!("dawn/udev: DRM master acquired at startup"),
                        Err(e) => tracing::warn!("dawn/udev: initial activate failed: {:?}", e),
                    }
                }

                // linux-dmabuf global"""

if old not in code:
    print("⚠️  Маркер не найден — файл разошёлся с ожидаемым, покажи актуальный кусок вокруг строки 166")
    sys.exit(1)

code = code.replace(old, new, 1)
UDEV_RS.write_text(code)
print("✅ Патч применён")

print("\n── cargo build --release ──")
r = subprocess.run(["cargo", "build", "--release"], cwd=DAWN, capture_output=True, text=True)
if r.returncode != 0:
    print("❌ Ошибки сборки:")
    print(r.stderr[-4000:])
    sys.exit(1)
print("✅ Сборка успешна")
