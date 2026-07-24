#!/usr/bin/env python3
import re, shutil, subprocess, sys
from pathlib import Path

SRC = Path.home() / "dev/dawn/src/udev.rs"
BAK = SRC.with_suffix(".rs.bak")

shutil.copy2(SRC, BAK)
print(f"✅ Backup: {BAK}")

code = SRC.read_text()
original = code

# ──────────────────────────────────────────────────────────────
# ФИКС 1: Удалить session_active блок + change_vt блок
# ──────────────────────────────────────────────────────────────
pattern_junk = re.compile(
    r'\n\s*// Как niri.*?'
    r'if let Err\(e\) = session\.change_vt\(current_vt\) \{[^}]*\}\n',
    re.DOTALL
)
code_new, n = pattern_junk.subn('\n', code)
if n:
    print(f"✅ ФИКС 1: удалён блок session_active + change_vt ({n} замен)")
    code = code_new
else:
    print("⚠️  ФИКС 1: блок не найден (возможно уже удалён)")

# ──────────────────────────────────────────────────────────────
# ФИКС 2: ActivateSession — вынести std::mem::take из цикла
# ──────────────────────────────────────────────────────────────
old_activate = re.compile(
    r'(SessionEvent::ActivateSession\s*=>\s*\{.*?'
    r'tracing::info!\("dawn/udev: session activated.*?"\);.*?'
    r'let _ = libinput_for_notifier\.resume\(\);)'
    r'(\s*// Берём DRM master.*?)'
    r'let nodes: Vec<_> = state\.udev_devices\.keys\(\)\.cloned\(\)\.collect\(\);'
    r'\s*for node in nodes \{'
    r'\s*let mut devices = std::mem::take\(&mut state\.udev_devices\);'
    r'\s*if let Some\(device\) = devices\.get_mut\(&node\) \{'
    r'(.*?)'          # тело if: activate + reset + render
    r'\s*\}'          # закрытие if
    r'\s*state\.udev_devices = devices;'
    r'\s*\}'          # закрытие for
    r'(\s*\})',       # закрытие ActivateSession =>
    re.DOTALL
)

def replace_activate(m):
    header   = m.group(1)   # до resume()
    comment  = m.group(2)   # комментарий
    body     = m.group(3)   # тело старого if-блока
    closing  = m.group(4)   # } закрытие ActivateSession

    # Убираем лишние отступы из body (оно было внутри if)
    body_clean = re.sub(r'^\s{24}', '                ', body, flags=re.MULTILINE)

    new = (
        f"{header}\n"
        f"                {comment.strip()}\n"
        f"                let mut devices = std::mem::take(&mut state.udev_devices);\n"
        f"                for device in devices.values_mut() {{\n"
        f"{body_clean}\n"
        f"                }}\n"
        f"                state.udev_devices = devices;\n"
        f"{closing}"
    )
    return new

code_new, n = old_activate.subn(replace_activate, code)
if n:
    print(f"✅ ФИКС 2: ActivateSession исправлен (take вынесен из цикла)")
    code = code_new
else:
    # Попробуем более простую замену — просто заменяем ключевые строки
    old_lines = [
        'let nodes: Vec<_> = state.udev_devices.keys().cloned().collect();',
        'for node in nodes {',
    ]
    if all(l in code for l in old_lines):
        # Патчим построчно
        lines = code.split('\n')
        out = []
        i = 0
        skip_until_brace = 0
        inserted_take = False
        while i < len(lines):
            line = lines[i]
            stripped = line.strip()

            # Найти "let nodes: Vec<_> = ..."
            if 'let nodes: Vec<_> = state.udev_devices.keys().cloned().collect();' in line:
                indent = len(line) - len(line.lstrip())
                ind = ' ' * indent
                out.append(f"{ind}let mut devices = std::mem::take(&mut state.udev_devices);")
                i += 1
                continue

            # Следующая строка "for node in nodes {"
            if 'for node in nodes {' in line:
                indent = len(line) - len(line.lstrip())
                ind = ' ' * indent
                out.append(f"{ind}for device in devices.values_mut() {{")
                i += 1
                continue

            # "let mut devices = std::mem::take(&mut state.udev_devices);" внутри for
            if re.match(r'\s*let mut devices = std::mem::take\(&mut state\.udev_devices\);', line):
                # пропускаем — уже добавили снаружи
                i += 1
                continue

            # "if let Some(device) = devices.get_mut(&node) {"
            if re.match(r'\s*if let Some\(device\) = devices\.get_mut\(&node\) \{', line):
                # пропускаем открывающий if
                i += 1
                continue

            # "state.udev_devices = devices;" внутри for
            if re.match(r'\s*state\.udev_devices = devices;', line):
                # оставляем первое вхождение (после for), удаляем второе
                out.append(line)
                i += 1
                continue

            out.append(line)
            i += 1

        code = '\n'.join(out)
        print("✅ ФИКС 2: применён построчный патч")
    else:
        print("⚠️  ФИКС 2: паттерн не найден — возможно уже исправлен")

# ──────────────────────────────────────────────────────────────
# ФИКС 3: Удалить Timer блок (100ms первый рендер)
# ──────────────────────────────────────────────────────────────
pattern_timer = re.compile(
    r'\n\s*// ── Первый рендер через Timer.*?'
    r'event_loop\.handle\(\)\.insert_source\(\s*'
    r'Timer::from_duration\(Duration::from_millis\(100\)\),'
    r'.*?'
    r'TimeoutAction::Drop\s*\}\s*\)\.unwrap\(\);',
    re.DOTALL
)
code_new, n = pattern_timer.subn(
    '\n                tracing::info!("dawn/udev: device ready, waiting for ActivateSession");',
    code
)
if n:
    print(f"✅ ФИКС 3: Timer(100ms) удалён ({n} замен)")
    code = code_new
else:
    print("⚠️  ФИКС 3: Timer блок не найден (возможно уже удалён)")

# ──────────────────────────────────────────────────────────────
# Запись результата
# ──────────────────────────────────────────────────────────────
if code != original:
    SRC.write_text(code)
    print(f"\n✅ udev.rs обновлён")
else:
    print(f"\n⚠️  Файл не изменился — все паттерны уже применены или не найдены")

# ──────────────────────────────────────────────────────────────
# Проверка синтаксиса через rustfmt --check
# ──────────────────────────────────────────────────────────────
print("\n── rustfmt check ──")
r = subprocess.run(
    ["rustfmt", "--edition", "2021", "--check", str(SRC)],
    capture_output=True, text=True
)
if r.returncode == 0:
    print("✅ rustfmt: синтаксис OK")
else:
    print("⚠️  rustfmt нашёл diff (не критично, просто форматирование):")
    print(r.stdout[:500])

# ──────────────────────────────────────────────────────────────
# Сборка
# ──────────────────────────────────────────────────────────────
print("\n── cargo build --release ──")
r = subprocess.run(
    ["cargo", "build", "--release"],
    cwd=Path.home() / "dev/dawn",
    capture_output=True, text=True
)
errors   = [l for l in r.stderr.split('\n') if l.strip().startswith('error')]
warnings = [l for l in r.stderr.split('\n') if 'warning' in l and 'unused' in l]

if r.returncode == 0:
    print("✅ Сборка успешна!")
    if warnings:
        print(f"   {len(warnings)} unused warnings (не критично)")
else:
    print("❌ Ошибки сборки:")
    for e in errors[:20]:
        print(f"   {e}")
    print("\n── Полный stderr ──")
    print(r.stderr[-3000:])
    print("\n── Откат к backup ──")
    shutil.copy2(BAK, SRC)
    print(f"✅ Откат выполнен: {BAK} → {SRC}")
    sys.exit(1)

print("\n═══════════════════════════════════")
print(" Все патчи применены, сборка OK")
print(" Запуск: ~/dev/dawn/target/release/dawn --tty")
print("═══════════════════════════════════")
