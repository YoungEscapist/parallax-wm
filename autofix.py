import re, shutil, subprocess, sys
from pathlib import Path

DAWN = Path.home() / "dev/dawn"
INPUT_RS = DAWN / "src/input.rs"
XDG_RS = DAWN / "src/handlers/xdg_shell.rs"

def backup(p):
    b = p.with_suffix(p.suffix + ".bak_autofix")
    shutil.copy2(p, b)
    return b

def patch(path, old, new, label):
    code = path.read_text()
    if old not in code:
        print(f"⚠️  {label}: маркер не найден, пропускаю")
        return code, False
    code = code.replace(old, new, 1)
    path.write_text(code)
    print(f"✅ {label}: применено")
    return code, True

# ── input.rs: логируем PointerMotion / PointerMotionAbsolute ──
backup(INPUT_RS)

_, ok1 = patch(
    INPUT_RS,
    "InputEvent::PointerMotion { event, .. } => {\n               let delta = event.delta();",
    "InputEvent::PointerMotion { event, .. } => {\n               let delta = event.delta();\n               tracing::info!(\"PTR MOTION: delta=({:.2},{:.2})\", delta.x, delta.y);",
    "PointerMotion logging",
)

_, ok2 = patch(
    INPUT_RS,
    "InputEvent::PointerMotionAbsolute { event, .. } => {\n                let output = self.space.outputs().next().unwrap();",
    "InputEvent::PointerMotionAbsolute { event, .. } => {\n                tracing::info!(\"PTR MOTION ABS\");\n                let output = self.space.outputs().next().unwrap();",
    "PointerMotionAbsolute logging",
)

# ── xdg_shell.rs: set_focus сразу в new_toplevel ──
backup(XDG_RS)

_, ok3 = patch(
    XDG_RS,
    "self.space.map_element(window, cursor_i32, false);\n        self.arrange();",
    "self.space.map_element(window.clone(), cursor_i32, false);\n        self.arrange();\n\n        // Автоматически отдаём фокус новому окну (как sway/hyprland)\n        if let Some(kb) = self.seat.get_keyboard() {\n            if let Some(top) = window.toplevel() {\n                let serial = smithay::utils::SERIAL_COUNTER.next_serial();\n                kb.set_focus(self, Some(top.wl_surface().clone()), serial);\n                tracing::info!(\"dawn: auto-focus new toplevel\");\n            }\n        }",
    "set_focus в new_toplevel",
)

if not (ok1 or ok2 or ok3):
    print("\n❌ Ни один патч не применился — файлы разошлись с ожидаемым. Покажи актуальные версии.")
    sys.exit(1)

print("\n── cargo build --release ──")
r = subprocess.run(["cargo", "build", "--release"], cwd=DAWN, capture_output=True, text=True)
if r.returncode != 0:
    print("❌ Ошибки сборки:")
    print(r.stderr[-4000:])
    sys.exit(1)
print("✅ Сборка успешна")
