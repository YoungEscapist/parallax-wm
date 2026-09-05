//! Заглушка куба рабочих столов для сборки без фичи `shaders` (plx-standard).
//!
//! `Шейдер::new` всегда `None` — и это выключает куб в единственной точке:
//! `build_cube_elements` без шейдера выходит первой строкой, обзор остаётся
//! плоской сеткой столов, а переключение стола — обычной сменой тегов.
//! Геометрия ([`Куб`]) настоящая, тем же кодом, что и в полной сборке: её
//! спрашивает ввод (какой стол под курсором на грани), и два разных ответа в
//! двух сборках были бы хуже, чем лишние сто строк арифметики.

use smithay::backend::renderer::{
    element::{Element, Id, Kind, RenderElement, UnderlyingStorage},
    gles::{GlesError, GlesFrame, GlesRenderer},
    utils::{CommitCounter, DamageSet, OpaqueRegions},
};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer as BufferCoords, Physical, Point, Rectangle, Scale, Transform};

// Настоящая геометрия куба — тот же файл, что и в полной сборке.
#[path = "../куб_math.rs"]
mod math;
pub use math::{Куб, оборот, стол_грани, шагов_до};

/// Программы нет и компилировать нечего.
#[derive(Debug, Clone)]
pub struct Шейдер;

impl Шейдер {
    pub fn new(_renderer: &mut GlesRenderer) -> Option<Self> {
        None
    }
}

/// Обёртка-пустышка: сконструировать её в этой сборке невозможно (для `new`
/// нужен `&Шейдер`, а взять его неоткуда), она есть ради типа в перечислении
/// элементов кадра.
#[derive(Debug)]
pub struct Грань<E> {
    inner: E,
}

impl<E> Грань<E> {
    pub fn new(
        inner: E,
        _шейдер: &Шейдер,
        _куб: &Куб,
        _угол: f32,
        _rect: (f32, f32, f32, f32),
    ) -> Self {
        Self { inner }
    }
}

impl<E: Element> Element for Грань<E> {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, BufferCoords> {
        self.inner.src()
    }

    fn damage_since(&self, scale: Scale<f64>, commit: Option<CommitCounter>) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl<E: RenderElement<GlesRenderer>> RenderElement<GlesRenderer> for Грань<E> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        src: Rectangle<f64, BufferCoords>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        self.inner.draw(frame, src, dst, damage, opaque_regions, cache)
    }

    fn underlying_storage(&self, renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}
