//! Заглушка режима Minecraft: то, что видит остальной код, когда фича `mine`
//! выключена.
//!
//! Подставляется вместо `src/mine/` целиком (см. `#[path]` в lib.rs), поэтому
//! снаружи модуль по-прежнему зовётся `crate::mine`.
//!
//! Функции здесь делятся на две группы, и разница между ними важнее, чем
//! кажется. Те, что ОТВЕЧАЮТ НА ВОПРОС о текущем окне (`панель_а_не_игра`,
//! `кнопки_игре`, `в_переборе`, `прячем_курсор`), обязаны возвращать `false` —
//! это ровно тот же ответ, что даёт полная сборка с выключенным режимом, и
//! ввод с тайлингом ведут себя как обычно. Те, что что-то ДЕЛАЮТ, пустые.

use crate::state::Parallax;
use crate::vr::scene::{Ключ, Раскладка};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::desktop::Window;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;

/// Живой режим `plx-mine`. В минимальной сборке значения не существует —
/// `state.mine` всегда `None`.
pub enum Шахта {}

/// Единственный текст про режим в минимальной сборке — одинаковый во всех
/// точках входа, чтобы человек узнавал не «не сработало», а что ставить.
pub fn нет_фичи() -> &'static str {
    crate::т!(
        "Режим Minecraft в этой сборке не собран — нужен plx-extra",
        "Minecraft mode is not built into this binary — use plx-extra",
    )
}

// ── Вопросы про окно: ответ тот же, что при выключенном режиме ───────────────

/// Фокус клавиатуры на панели мода. Панелей нет — фокуса нет.
pub fn фокус_панели_мода(_state: &Parallax) -> Option<WlSurface> {
    None
}

/// Это окно самой игры? Игры нет — значит нет.
pub fn это_окно_игры(_state: &Parallax, _s: &WlSurface) -> bool {
    false
}

pub fn прячем_курсор(_state: &Parallax) -> bool {
    false
}

pub fn панель_а_не_игра(_state: &Parallax, _окно: &Window) -> bool {
    false
}

pub fn кнопки_игре(_state: &Parallax, _окно: &Window) -> bool {
    false
}

pub fn в_переборе(_state: &Parallax, _окно: &Window) -> bool {
    false
}

// ── Действия ────────────────────────────────────────────────────────────────

pub fn игру_наверх(_state: &mut Parallax) {}

pub fn фокус_панели(_state: &mut Parallax, _окно: &Window) {}

pub fn режим(state: &mut Parallax) {
    state.уведомить(нет_фичи());
}

pub fn включить(_state: &mut Parallax) -> Result<(), String> {
    Err(нет_фичи().into())
}

pub fn выключить(_state: &mut Parallax) {}

/// Зовётся каждой итерацией главного цикла — пустое тело, оптимизатор его
/// выкидывает целиком.
pub fn тик(_state: &mut Parallax) {}

pub fn тик_с(_state: &mut Parallax, _renderer: &mut GlesRenderer) {}

pub fn хват_тумблер(_state: &mut Parallax) {}

/// Раскладка панелей общая со шлемом (`state.vr_раскладка`), поэтому и без
/// режима переключать её осмысленно — меняется то, как лягут панели, когда
/// режим появится.
pub fn сменить_раскладку(state: &mut Parallax) -> Раскладка {
    let новая = state.vr_раскладка.следующая();
    state.vr_раскладка = новая;
    новая
}

pub fn назначить_игру(_state: &mut Parallax, _номер: Option<usize>) -> String {
    нет_фичи().into()
}

pub fn луч_снаружи(_state: &mut Parallax, _начало: [f32; 3], _направление: [f32; 3]) -> String {
    нет_фичи().into()
}

pub fn закрепить_снаружи(_state: &mut Parallax, _ключ: Ключ, _x: f32, _y: f32) -> String {
    нет_фичи().into()
}

pub fn состояние(_state: &Parallax) -> String {
    нет_фичи().into()
}

pub fn панели_строкой(_state: &Parallax) -> String {
    нет_фичи().into()
}
