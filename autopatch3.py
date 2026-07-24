import re, shutil, subprocess, sys
from pathlib import Path

SRC = Path.home() / "dev/dawn/src/udev.rs"
BAK = SRC.with_suffix(".rs.bak3")
shutil.copy2(SRC, BAK)
code = original = SRC.read_text()

# ФИКС: убрать весь блок session_active + change_vt (мёртвый код + spurious VT-switch)
pattern = re.compile(
    r"    // Как niri:.*?session\.change_vt\(current_vt\) \{\n"
    r"        tracing::warn!\(\"dawn/udev: change_vt failed: \{:\?\}\", e\);\n"
    r"    \}\n\n",
    re.DOTALL
)
code, n = pattern.subn("", code)
if n:
    print(f"✅ Убран блок session_active/change_vt ({n} замена)")
else:
    print("⚠️  Паттерн не найден — покажи актуальный файл, поправлю вручную")

if code != original:
    SRC.write_text(code)
    print("✅ udev.rs обновлён, бэкап:", BAK)
else:
    print("⚠️  Файл не изменился")
    sys.exit(1)

print("\n── cargo build --release ──")
r = subprocess.run(["cargo", "build", "--release"], cwd=Path.home()/"dev/dawn",
                    capture_output=True, text=True)
if r.returncode == 0:
    print("✅ Сборка успешна")
else:
    print("❌ Ошибки сборки:")
    print(r.stderr[-3000:])
    shutil.copy2(BAK, SRC)
    print(f"↩️  Откат: {BAK} → {SRC}")
    sys.exit(1)
