//! Созвездия — раскладка «мастер и стопка», взятая из halley.
//!
//! Источник: [saltnpepper97/halley](https://github.com/saltnpepper97/halley),
//! `crates/halley-core/src/cluster_layout.rs::layout_tiling_workspace` и
//! `crates/halley-core/src/cluster.rs`. Там это называется кластером; в dawn у
//! той же вещи давно своё имя — созвездие.
//!
//! **Что именно взято.** Модель и геометрия:
//!
//!  · у грозди есть ПОРЯДОК, и `члены[0]` — мастер (`Cluster::master`);
//!  · мастер занимает 60% ширины слева во всю высоту, остальные делят
//!    оставшуюся колонку сверху вниз поровну (`layout_tiling_workspace`);
//!  · двое — это ровно две колонки, без вертикального деления вовсе;
//!  · последнему в стопке достаётся ВЕСЬ остаток по высоте, а не его
//!    вычисленная доля: иначе накопленная ошибка округления оставляла бы у дна
//!    щель в пару пикселей;
//!  · продвижение окна в мастера — перестановка его в начало списка
//!    (`Cluster::promote_member_to_master`).
//!
//! **Чего НЕ взято и почему.** У halley член сверх лимита уходит в очередь и
//! на экране не появляется вовсе (`overflow_members`, `queue_members`) — до
//! него добираются через полоску кластера в его интерфейсе. У dawn такой
//! полоски нет, и «лишнее» окно просто исчезло бы с холста без единого способа
//! его достать. Поэтому места получают ВСЕ члены; лимита нет.
//!
//! Ещё не взяты режимы `Collapsed`/`Active` с узлом-ядром: в dawn у грозди
//! ровно две операции — собрать и распустить (Win+D), и промежуточного
//! состояния между ними нет.

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle, Size};

/// Доля ширины под мастера. Число из halley (`split_w * 0.6`).
pub const МАСТЕР_ДОЛЯ: f64 = 0.6;

/// Зазор между членами грозди. У halley это `inner_gap` из настроек; в dawn
/// берём внутренний зазор тайлинга — гроздь обязана выглядеть частью того же
/// интерфейса, а не отдельной поделкой.
pub fn зазор() -> i32 {
    crate::tiling::GAP_INNER
}

/// Куда встаёт один член грозди. `глубина` — его номер в порядке наложения,
/// как `ClusterWorkspacePlacement::depth` у halley: 0 у мастера.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Размещение {
    pub индекс: usize,
    pub rect: Rectangle<i32, Logical>,
    pub глубина: usize,
}

/// Раскладка грозди из `членов` окон в прямоугольнике `bounds`.
///
/// Порт `layout_tiling_workspace` из halley один в один, только на целых
/// логических пикселях вместо f32: у dawn размеры окон целые, и считать их в
/// долях пикселя значит копить расхождение между тем, что попросили у клиента,
/// и тем, куда положили рамку.
pub fn раскладка(
    bounds: Rectangle<i32, Logical>,
    зазор: i32,
    членов: usize,
) -> Vec<Размещение> {
    if членов == 0 {
        return Vec::new();
    }
    if членов == 1 {
        return vec![Размещение { индекс: 0, rect: bounds, глубина: 0 }];
    }

    let зазор = зазор.max(0);
    let split_w = (bounds.size.w - зазор).max(0);
    let master_w = ((split_w as f64 * МАСТЕР_ДОЛЯ).round() as i32).clamp(0, split_w);
    let stack_w = (split_w - master_w).max(0);
    let stack_x = bounds.loc.x + master_w + зазор;
    let в_стопке = членов - 1;

    let mut out = Vec::with_capacity(членов);
    out.push(Размещение {
        индекс: 0,
        rect: Rectangle::new(bounds.loc, Size::from((master_w, bounds.size.h))),
        глубина: 0,
    });

    // Двое — это две колонки во всю высоту. Отдельная ветка не оптимизация:
    // без неё пара окон получила бы «стопку из одного», то есть тот же
    // прямоугольник, но пройденный через деление на единицу.
    if в_стопке == 1 {
        out.push(Размещение {
            индекс: 1,
            rect: Rectangle::new(
                Point::from((stack_x, bounds.loc.y)),
                Size::from((stack_w, bounds.size.h)),
            ),
            глубина: 1,
        });
        return out;
    }

    let все_зазоры = зазор * (в_стопке.saturating_sub(1)) as i32;
    let высота = ((bounds.size.h - все_зазоры).max(0) as f64) / в_стопке as f64;
    let mut y = bounds.loc.y;
    let дно = bounds.loc.y + bounds.size.h;

    for i in 0..в_стопке {
        let осталось = в_стопке - i;
        // Последнему — весь остаток: доли по отдельности округляются вниз, и
        // без этого у дна грозди оставалась бы щель тем шире, чем больше окон.
        let h = if осталось == 1 {
            (дно - y).max(0)
        } else {
            высота.round().max(0.0) as i32
        };
        out.push(Размещение {
            индекс: i + 1,
            rect: Rectangle::new(Point::from((stack_x, y)), Size::from((stack_w, h))),
            глубина: i + 1,
        });
        y += h + зазор;
    }

    out
}

/// Поставить `window` мастером грозди — перестановкой в начало
/// (`Cluster::promote_member_to_master` у halley). `false` — окна в грозди нет.
pub fn в_мастера(члены: &mut Vec<Window>, window: &Window) -> bool {
    let Some(i) = члены.iter().position(|w| w == window) else {
        return false;
    };
    if i == 0 {
        return true;
    }
    let w = члены.remove(i);
    члены.insert(0, w);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn рамка(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(Point::from((x, y)), Size::from((w, h)))
    }

    /// Одинокое окно занимает всю рамку — стопки нет вовсе.
    #[test]
    fn один_член_занимает_всё() {
        let b = рамка(10, 20, 800, 600);
        assert_eq!(раскладка(b, 12, 1), vec![Размещение { индекс: 0, rect: b, глубина: 0 }]);
        assert!(раскладка(b, 12, 0).is_empty());
    }

    /// Двое — две колонки во всю высоту, мастер шире (60% против 40%).
    #[test]
    fn двое_делят_ширину_шестьдесят_на_сорок() {
        let b = рамка(0, 0, 1012, 600);
        let l = раскладка(b, 12, 2);
        assert_eq!(l.len(), 2);
        assert_eq!(l[0].rect.size.h, 600);
        assert_eq!(l[1].rect.size.h, 600);
        assert!(l[0].rect.size.w > l[1].rect.size.w, "мастер обязан быть шире");
        // 1012 − 12 = 1000 под окна; 60/40.
        assert_eq!(l[0].rect.size.w, 600);
        assert_eq!(l[1].rect.size.w, 400);
        assert_eq!(l[1].rect.loc.x, l[0].rect.loc.x + l[0].rect.size.w + 12);
    }

    /// Главное свойство раскладки: члены не налезают друг на друга и не
    /// вылезают из рамки, а стопка занимает её ВСЮ высоту без щели у дна.
    ///
    /// Щель у дна — ровно то, ради чего в halley последнему в стопке отдают
    /// остаток целиком: доли округляются вниз, и на пяти окнах набегает
    /// несколько пикселей.
    #[test]
    fn стопка_укладывается_в_рамку_без_щелей() {
        for членов in 2..=9usize {
            for (w, h, g) in [(1600, 900, 12), (1013, 601, 7), (300, 200, 0)] {
                let b = рамка(-40, 30, w, h);
                let l = раскладка(b, g, членов);
                assert_eq!(l.len(), членов, "{членов} членов при {w}×{h}");
                assert_eq!(
                    l.iter().map(|p| p.индекс).collect::<Vec<_>>(),
                    (0..членов).collect::<Vec<_>>(),
                    "номера обязаны идти подряд — по ним берутся сами окна",
                );
                for p in &l {
                    assert!(p.rect.loc.x >= b.loc.x && p.rect.loc.y >= b.loc.y);
                    assert!(p.rect.loc.x + p.rect.size.w <= b.loc.x + b.size.w);
                    assert!(
                        p.rect.loc.y + p.rect.size.h <= b.loc.y + b.size.h,
                        "член {} вылез вниз: {:?} из {:?}", p.индекс, p.rect, b,
                    );
                }
                // Стопка (всё, кроме мастера) кончается ровно у дна рамки.
                let низ = l.iter().skip(1)
                    .map(|p| p.rect.loc.y + p.rect.size.h)
                    .max()
                    .unwrap();
                assert_eq!(низ, b.loc.y + b.size.h, "щель у дна при {членов} членах");
                // Соседи по стопке не перекрываются.
                for пара in l[1..].windows(2) {
                    assert!(
                        пара[0].rect.loc.y + пара[0].rect.size.h <= пара[1].rect.loc.y,
                        "члены стопки налезли: {:?} и {:?}", пара[0].rect, пара[1].rect,
                    );
                }
            }
        }
    }

    /// Вырожденная рамка (гроздь ужали в ноль) не даёт отрицательных размеров:
    /// нулевая поверхность запрещена протоколом, отрицательная — бессмысленна.
    #[test]
    fn вырожденная_рамка_не_даёт_минусов() {
        for (w, h) in [(0, 0), (5, 5), (2, 400)] {
            for членов in 1..=5usize {
                for p in раскладка(рамка(0, 0, w, h), 12, членов) {
                    assert!(p.rect.size.w >= 0 && p.rect.size.h >= 0, "{:?}", p.rect);
                }
            }
        }
    }
}
