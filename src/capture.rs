//! Полный перехват курсора приложением (zwp_pointer_constraints_v1 +
//! zwp_relative_pointer_v1).
//!
//! Игре мало «окна во весь экран». Ей нужны две вещи, которых обычная
//! доставка событий не даёт:
//!
//!  1. **захват курсора** — стрелка не должна ни ездить по экрану, ни
//!     упираться в его край: она либо стоит на месте (lock, обзор мышью в
//!     шутере), либо не выпускается за пределы окна (confine, стратегии с
//!     прокруткой карты у края);
//!  2. **относительное движение** — сырые дельты мыши БЕЗ привязки к точке
//!     экрана. Пока курсор заперт, абсолютная позиция не меняется вовсе, и
//!     единственный источник «куда повёл мышью» — эти дельты.
//!
//! Без обоих протоколов заперть курсор нечем: игра просит захват, получает
//! отказ и либо отключает мышиный обзор, либо (чаще) прыгает камерой, пока
//! стрелка ползёт к краю монитора.
//!
//! Что тут есть:
//!  * [`Dawn::pointer_constraint_now`] — спросить, заперт ли курсор ПРЯМО
//!    сейчас (зовётся из input.rs на каждое движение);
//!  * [`Dawn::activate_pointer_constraint`] — включить захват, когда курсор
//!    сам въехал в область запроса;
//!  * реализация [`PointerConstraintsHandler`].
//!
//! Полноэкранный режим (F11 и запрос клиента) живёт отдельно, в
//! fullscreen.rs: перехват ЭКРАНА и перехват КУРСОРА — независимые вещи, игра
//! может просить любую из них по отдельности.

use smithay::{
    delegate_pointer_constraints, delegate_relative_pointer,
    input::pointer::PointerHandle,
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point},
    wayland::{
        compositor::RegionAttributes,
        pointer_constraints::{
            PointerConstraint, PointerConstraintsHandler, with_pointer_constraint,
        },
    },
};

use crate::state::Dawn;

/// Что протокол просит сделать с курсором прямо сейчас.
#[derive(Default)]
pub struct PointerCapture {
    /// Курсор заперт на месте: абсолютную позицию не менять вовсе, клиенту
    /// уходят только относительные дельты.
    pub locked: bool,
    /// Курсор не выпускать за поверхность (и за `region`, если задан).
    pub confined: bool,
    /// Область удержания в surface-локальных координатах (None — вся
    /// поверхность).
    pub region: Option<RegionAttributes>,
    /// Поверхность, которая держит курсор: выпускать за неё нельзя.
    pub surface: Option<WlSurface>,
    /// Начало координат поверхности на холсте — им переводят canvas-точку в
    /// surface-локальную при проверке `region`.
    pub origin: Point<f64, Logical>,
}

impl PointerCapture {
    /// Точка холста всё ещё внутри удержания? Проверяются обе границы:
    /// поверхность (курсор не должен «перескочить» на соседнее окно) и
    /// заданная клиентом область внутри неё.
    pub fn holds(&self, state: &Dawn, pos: Point<f64, Logical>) -> bool {
        let Some(surface) = self.surface.as_ref() else { return true };
        match state.surface_under(pos) {
            Some((s, _)) if &s == surface => {}
            _ => return false,
        }
        self.region.as_ref()
            .is_none_or(|r| r.contains((pos - self.origin).to_i32_round()))
    }
}

impl Dawn {
    /// Захват курсора для поверхности под указателем.
    ///
    /// Считается по поверхности ПОД КУРСОРОМ, а не по фокусу клавиатуры:
    /// протокол привязывает ограничение к поверхности, и активным оно бывает
    /// только пока указатель на ней. Поверхность передаётся готовой — вызов
    /// стоит на пути КАЖДОГО движения мыши (до тысячи событий в секунду у
    /// игровых мышей), и лишний хит-тест здесь не бесплатный.
    pub fn pointer_constraint_at(
        &self,
        under: Option<&(WlSurface, Point<f64, Logical>)>,
    ) -> PointerCapture {
        let mut итог = PointerCapture::default();
        let Some(pointer) = self.seat.get_pointer() else { return итог };
        let Some((surface, origin)) = under else { return итог };
        let origin = *origin;
        итог.origin = origin;
        let местная = (self.pointer_location - origin).to_i32_round();
        with_pointer_constraint(surface, &pointer, |ограничение| match ограничение {
            Some(c) if c.is_active() => {
                // Вне своей области ограничение не действует — курсор ходит как
                // обычно, пока не вернётся в неё.
                if !c.region().is_none_or(|r| r.contains(местная)) {
                    return;
                }
                match &*c {
                    PointerConstraint::Locked(_) => итог.locked = true,
                    PointerConstraint::Confined(conf) => {
                        итог.confined = true;
                        итог.region = conf.region().cloned();
                        итог.surface = Some(surface.clone());
                    }
                }
            }
            _ => {}
        });
        итог
    }

    /// Курсор въехал в область запрошенного захвата — включить его.
    ///
    /// Клиент создаёт ограничение заранее (обычно на весь свой surface), а
    /// вступает оно в силу только когда указатель действительно там. Зовётся
    /// после каждого движения курсора.
    pub fn activate_pointer_constraint(&mut self) {
        let Some(pointer) = self.seat.get_pointer() else { return };
        let Some((surface, origin)) = self.surface_under(self.pointer_location) else { return };
        let местная = (self.pointer_location - origin).to_i32_round();
        with_pointer_constraint(&surface, &pointer, |ограничение| match ограничение {
            Some(c) if !c.is_active() => {
                if c.region().is_none_or(|r| r.contains(местная)) {
                    c.activate();
                    tracing::debug!("dawn/capture: захват курсора включён");
                }
            }
            _ => {}
        });
    }

    /// Начало координат поверхности на холсте (левый верхний угол окна, к
    /// которому она принадлежит). Нужно, чтобы перевести surface-локальную
    /// точку клиента в canvas-точку курсора.
    fn surface_origin(&self, surface: &WlSurface) -> Option<Point<f64, Logical>> {
        self.space
            .elements()
            .find(|w| crate::xwin::is_surface(w, surface))
            .and_then(|w| self.space.element_geometry(w))
            .map(|geo| geo.loc.to_f64())
    }
}

impl PointerConstraintsHandler for Dawn {
    /// Клиент создал ограничение. Если указатель уже на этой поверхности —
    /// включаем сразу: иначе игра, запросившая захват в ответ на клик по
    /// своему же окну, ждала бы, пока курсор «въедет» туда, где он и так есть.
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        if pointer.current_focus().as_ref() != Some(surface) {
            return;
        }
        with_pointer_constraint(surface, pointer, |ограничение| {
            if let Some(c) = ограничение {
                c.activate();
            }
        });
        tracing::info!("dawn/capture: клиент запросил захват курсора");
    }

    /// Клиент подсказал, где показать курсор, когда захват снимется (обычно —
    /// центр прицела). Переносим стрелку туда, иначе после выхода из игры она
    /// окажется там, где её заперли.
    fn cursor_position_hint(
        &mut self,
        surface: &WlSurface,
        pointer: &PointerHandle<Self>,
        location: Point<f64, Logical>,
    ) {
        let активен = with_pointer_constraint(surface, pointer, |c| {
            c.is_some_and(|c| c.is_active())
        });
        if !активен {
            return;
        }
        let Some(origin) = self.surface_origin(surface) else { return };
        let цель = origin + location;
        self.pointer_location = цель;
        pointer.set_location(цель);
        // Стрелку двигали мы, а не мышь: фиксируем новую экранную точку как
        // эталонную, иначе sync_pointer_to_camera утащит её обратно.
        self.pointer_warped();
        self.request_redraw();
    }
}

delegate_pointer_constraints!(Dawn);
delegate_relative_pointer!(Dawn);
