//! Спокойное закрытие окна: снимок содержимого плюс затухание.
//!
//! **Зачем снимок, а не сама поверхность.** Анимировать закрытие «как есть»
//! нечем: к тому моменту, когда dawn узнаёт о закрытии
//! (`XdgShellHandler::toplevel_destroyed`, `XwmHandler::destroyed_window`),
//! клиента уже нет — рисовать нечего, и окно пропадает одним кадром. Поэтому
//! ровно в этот момент, пока текстура последнего кадра ещё жива в
//! `RendererSurfaceState`, окно рисуется в свой offscreen-буфер, и дальше
//! анимируется УЖЕ КАРТИНКА. Клиент к этому времени может быть мёртв — картинке
//! всё равно.
//!
//! **Движение.** Только затухание и лёгкое сжатие к центру. Не «схлопывание в
//! точку» и не разлёт: закрытие — это не событие, за которым надо следить,
//! окно просто перестаёт быть. Кривая — [`crate::anim::ease_calm`], та же
//! спокойная, что у камеры; резкая `ease_out_cubic` тут ни к чему — гнаться
//! взгляду не за чем.
//!
//! Приём с offscreen-буфером тот же, что в [`crate::blur`]: `create_buffer` →
//! `bind` → `render`. Отказ на любом шаге — мягкий: окно просто исчезнет
//! сразу, как исчезало раньше.

use std::time::{Duration, Instant};

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::Color32F;
use smithay::backend::renderer::{
    Bind, Frame as _, Offscreen, Renderer as _,
    element::{AsRenderElements as _, Id, surface::WaylandSurfaceRenderElement},
    gles::{GlesRenderer, GlesTexture},
    utils::draw_render_elements,
};
use smithay::desktop::Window;
use smithay::utils::{
    Buffer as BufferCoords, Logical, Physical, Point, Rectangle, Scale, Size, Transform,
};

/// Наименьший масштаб в конце ухода. Не ноль: окно должно ПОГАСНУТЬ, а не
/// стянуться в точку — точка притягивает взгляд ровно туда, откуда его пора
/// увести.
const МАСШТАБ_В_КОНЦЕ: f64 = 0.92;

/// Уходящее окно: картинка последнего кадра плюс где и как долго её гасить.
pub struct Уход {
    pub текстура: GlesTexture,
    /// Контекст рендерера, в котором текстура создана. Запоминаем ЗДЕСЬ, а не
    /// спрашиваем при отрисовке: `TextureRenderElement` требует именно тот
    /// контекст, которому текстура принадлежит, а к моменту показа рендерер
    /// вообще может быть другим (второе устройство, пересоздание после
    /// VT-switch) — и тогда картинка была бы не та.
    pub контекст: smithay::backend::renderer::ContextId<GlesTexture>,
    /// Где окно стояло на ХОЛСТЕ (не на экране): камера за время ухода
    /// продолжает ездить, и привязка к экрану уводила бы картинку с места.
    pub rect: Rectangle<i32, Logical>,
    /// Постоянный Id: damage tracker ведёт историю по нему, и новый Id на
    /// каждый кадр означал бы полную перерисовку экрана (та же грабля, что в
    /// decor.rs и build_wallpaper_backdrop).
    pub id: Id,
    start: Instant,
    duration: Duration,
}

impl Уход {
    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// Доля пройденного пути по спокойной кривой.
    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        crate::anim::ease_calm(elapsed / self.duration.as_secs_f64().max(1e-6))
    }

    /// Прозрачность и масштаб на текущий момент.
    pub fn alpha_scale(&self) -> (f32, f64) {
        let t = self.t();
        ((1.0 - t) as f32, 1.0 - (1.0 - МАСШТАБ_В_КОНЦЕ) * t)
    }
}

/// Снять картинку окна в текстуру и завести уход.
///
/// `rect` — геометрия окна на холсте (`Space::element_geometry`), она же
/// задаёт размер буфера. Возврат `None` — снимок не вышел (буфер клиента уже
/// отпущен, не хватило видеопамяти): вызывающий просто убирает окно как
/// раньше, без анимации.
pub fn снять(
    renderer: &mut GlesRenderer,
    window: &Window,
    rect: Rectangle<i32, Logical>,
    duration: Duration,
) -> Option<Уход> {
    if rect.size.w <= 0 || rect.size.h <= 0 {
        return None;
    }
    // Совсем большие окна не снимаем: буфер во весь 4K — это 33 МБ на ровном
    // месте, а закрыть подряд можно и десяток. Предел щедрый (вдвое больше
    // любого разумного экрана) и нужен только против вырождения.
    const ПРЕДЕЛ: i32 = 8192;
    if rect.size.w > ПРЕДЕЛ || rect.size.h > ПРЕДЕЛ {
        return None;
    }

    // Дерево поверхностей кладём так, чтобы ВИДИМАЯ часть окна легла в (0,0):
    // у клиентов с клиентскими рамками (GTK, Electron) начало дерева левее и
    // выше геометрии — ровно та же пара точек, что в render_surface.
    let сдвиг = window.geometry().loc;
    let элементы: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = window.render_elements(
        renderer,
        Point::<i32, Physical>::from((-сдвиг.x, -сдвиг.y)),
        Scale::from(1.0),
        1.0f32,
    );
    if элементы.is_empty() {
        // Клиент уже отпустил буферы — рисовать нечего.
        return None;
    }

    let размер = Size::<i32, Physical>::from((rect.size.w, rect.size.h));
    let mut текстура = Offscreen::<GlesTexture>::create_buffer(
        renderer,
        Fourcc::Abgr8888,
        Size::<i32, BufferCoords>::from((размер.w, размер.h)),
    )
    .map_err(|e| tracing::warn!("dawn/close: буфер снимка {:?}: {:?}", размер, e))
    .ok()?;

    let полный = Rectangle::<i32, Physical>::new((0, 0).into(), размер);
    {
        let mut fb = renderer.bind(&mut текстура).ok()?;
        let mut frame = renderer.render(&mut fb, размер, Transform::Normal).ok()?;
        // Чистим В ПРОЗРАЧНОЕ, а не в чёрное: у окна скруглённые углы и
        // клиентские тени, и чёрная подложка вылезла бы из-под них прямыми
        // углами ровно в момент, когда на окно смотрят.
        frame.clear(Color32F::TRANSPARENT, &[полный]).ok()?;
        draw_render_elements::<GlesRenderer, _, _>(&mut frame, 1.0, &элементы, &[полный])
            .map_err(|e| tracing::warn!("dawn/close: снимок не нарисовался: {:?}", e))
            .ok()?;
        // SyncPoint ждать не нужно и вредно: ждать пришлось бы прямо в
        // обработчике закрытия окна, то есть в главном цикле. Текстуру
        // прочитает следующий кадр того же контекста, а внутри одного контекста
        // GL и так упорядочивает команды.
        let _ = frame.finish().ok()?;
    }

    Some(Уход {
        контекст: renderer.context_id(),
        текстура,
        rect,
        id: Id::new(),
        start: Instant::now(),
        duration,
    })
}
