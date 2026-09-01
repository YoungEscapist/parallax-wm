use crate::Dawn;
use crate::canvas::VelocityTracker;
use crate::tiling::Layout;
use smithay::{
    desktop::Window,
    input::pointer::{
        AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent,
        GesturePinchBeginEvent, GesturePinchEndEvent, GesturePinchUpdateEvent,
        GestureSwipeBeginEvent, GestureSwipeEndEvent, GestureSwipeUpdateEvent,
        GrabStartData as PointerGrabStartData, MotionEvent, PointerGrab,
        PointerInnerHandle, RelativeMotionEvent,
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{Logical, Point, Rectangle, Size},
};
use std::time::{Duration, Instant};

/// Минимальная пауза между двумя свапами тайловых окон в одном драге.
/// Без неё свап срабатывал на КАЖДОМ motion-событии, пока курсор висит над
/// соседом (сотни раз в секунду): раскладка пересобиралась быстрее, чем окна
/// успевали доехать, и картинка дрожала.
const SWAP_COOLDOWN: Duration = Duration::from_millis(160);

/// Отступ от края окна-цели (доля размера), внутри которого свап НЕ считается.
/// Курсор должен зайти в «ядро» соседа, а не задеть его границу — иначе на
/// стыке двух окон свапы туда-сюда пингпонгуют от дрожания мыши.
const SWAP_INSET: f64 = 0.18;


/// Прямоугольник, В КОТОРОМ окно ОКАЖЕТСЯ: если оно сейчас летит (свап,
/// толчок), берём цель анимации, а не текущий кадр полёта. Все решения драга
/// (что толкать, с чем свапаться) должны считаться от покоя — иначе они
/// зависят от фазы анимации и сами себя подхлёстывают.
fn resting_rect(data: &Dawn, window: &Window) -> Option<Rectangle<i32, Logical>> {
    let geo = data.space.element_geometry(window)?;
    let loc = data.window_anim_target(window).unwrap_or(geo.loc);
    Some(Rectangle::new(loc, geo.size))
}

/// Тайловое окно текущего тега, в «ядро» которого попал курсор (см. SWAP_INSET).
/// Считается по покоящимся слотам, а не по space.element_under: во время
/// перелёта окон hit-test по живой геометрии ловил случайных соседей.
fn tiled_swap_target(data: &Dawn, dragged: &Window, cursor: Point<f64, Logical>) -> Option<Window> {
    // В монокле у всех окон стопки ОДИН И ТОТ ЖЕ прямоугольник во весь экран:
    // «окно под курсором» там не определено, и свап по позиции просто
    // перемешивал бы стопку каждые SWAP_COOLDOWN. Порядок стопки меняется
    // клавишами, не мышью.
    if data.tile_config.layout == Layout::Monocle {
        return None;
    }
    let tags = data.viewport.current_tags();
    data.tagged_windows.iter()
        .filter(|tw| {
            tw.tags & tags != 0
                && !tw.floating
                && !crate::dwindle::same_window(&tw.window, dragged)
        })
        .find_map(|tw| {
            let r = resting_rect(data, &tw.window)?;
            let ix = (r.size.w as f64 * SWAP_INSET).round() as i32;
            let iy = (r.size.h as f64 * SWAP_INSET).round() as i32;
            let core = Rectangle::new(
                Point::from((r.loc.x + ix, r.loc.y + iy)),
                Size::from((r.size.w - 2 * ix, r.size.h - 2 * iy)),
            );
            (core.size.w > 0 && core.size.h > 0 && core.to_f64().contains(cursor))
                .then(|| tw.window.clone())
        })
}

/// Порог примагничивания (2.1): при отпускании кнопки край окна подравнивается
/// к соседнему, если оказался в пределах этой дистанции.
const SNAP_DISTANCE: i32 = 20;

/// Скорость отпускания (px/сек), выше которой магнитирование НЕ применяется:
/// окно бросили, а не положили, и прилипание к случайно оказавшемуся рядом
/// краю съело бы весь бросок (см. finish()).
const SNAP_MAX_SPEED: f64 = 600.0;

pub struct MoveSurfaceGrab {
    pub start_data: PointerGrabStartData<Dawn>,
    pub window: Window,
    pub initial_window_location: Point<i32, Logical>,
    /// (2.5) уже вырвано из горизонтальной ленты в этом драге — сшивание
    /// соседей делается один раз, при первом пересечении порога по Y
    torn_from_ribbon: bool,
    /// Остальные окна из "созвездия" (см. selection.rs) этого окна и их
    /// позиции на момент начала драга — двигаются той же дельтой, что и
    /// перетаскиваемое окно, чтобы группа перемещалась как единое целое.
    group_initial: Vec<(Window, Point<i32, Logical>)>,
    /// Трекер скорости курсора во время драга — на отпускании даёт инерцию
    /// (окно доезжает по инерции, см. button()).
    velocity: VelocityTracker,
    /// Последняя позиция окна во время драга (для проекции инерции).
    last_loc: Point<i32, Logical>,
    /// Когда в этом драге в последний раз произошёл свап тайловых окон —
    /// см. SWAP_COOLDOWN.
    last_swap: Option<Instant>,
}

impl MoveSurfaceGrab {
    pub fn new(
        start_data: PointerGrabStartData<Dawn>,
        window: Window,
        initial_window_location: Point<i32, Logical>,
        group_initial: Vec<(Window, Point<i32, Logical>)>,
    ) -> Self {
        Self {
            start_data,
            window,
            initial_window_location,
            torn_from_ribbon: false,
            group_initial,
            velocity: VelocityTracker::new(),
            last_loc: initial_window_location,
            last_swap: None,
        }
    }

    /// Сдвинуть остальные окна выделения на ту же дельту, что и
    /// перетаскиваемое. Нужно там, где окно под курсором доводят «руками»
    /// (магнитирование): группа обязана приехать тем же смещением, иначе
    /// строй, который держался весь драг, ломается на последнем шаге.
    fn shift_group(&self, data: &mut Dawn, shift: Point<i32, Logical>) {
        if shift.x == 0 && shift.y == 0 {
            return;
        }
        for (member, _) in &self.group_initial {
            let Some(loc) = data.space.element_geometry(member).map(|g| g.loc) else {
                continue;
            };
            let new_loc = loc + shift;
            data.space.map_element(member.clone(), new_loc, false);
            if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| &tw.window == member) {
                tw.float_position = new_loc;
                tw.position = new_loc;
                tw.float_position_set = true;
            }
        }
    }
}

/// Ширина Y-полосы, в пределах которой окна считаются "одной горизонтальной
/// лентой" (2.5).
const RIBBON_ROW_TOLERANCE: i32 = 20;

/// Когда окно вырывается из своей ленты (вертикальный вылет > высоты окна),
/// все окна ленты правее вытащенного подтягиваются влево на его ширину —
/// лента "сшивается" без образования дыры.
fn stitch_ribbon_gap(
    data: &mut Dawn,
    dragged: &Window,
    dragged_initial_loc: Point<i32, Logical>,
    dragged_width: i32,
) {
    let row_y = dragged_initial_loc.y;
    // По покоящимся прямоугольникам: окно, которое ещё летит от толчка, иначе
    // попадало/не попадало в ленту в зависимости от фазы своей анимации.
    let to_shift: Vec<(Window, Point<i32, Logical>)> = data.space.elements()
        .filter(|w| {
            *w != dragged
        })
        .filter_map(|w| resting_rect(data, w).map(|r| (w.clone(), r)))
        .filter(|(_, r)| {
            (r.loc.y - row_y).abs() <= RIBBON_ROW_TOLERANCE && r.loc.x > dragged_initial_loc.x
        })
        .map(|(w, r)| (w, Point::from((r.loc.x - dragged_width, r.loc.y))))
        .collect();

    if to_shift.is_empty() {
        return;
    }

    for (w, new_loc) in &to_shift {
        // Не телепорт, а тот же пружинный доезд, что и у остальных сдвигов —
        // «сшивание» ленты рывком выбивалось из общего ощущения.
        data.animate_window_to_dur(w, *new_loc, crate::anim::дуг::толчок_соседа());
        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
            &tw.window == w
        }) {
            tw.float_position = *new_loc;
            tw.position = *new_loc;
        }
    }
    data.request_plane_reset();
    tracing::info!("dawn: ribbon stitched ({} windows shifted)", to_shift.len());
}

/// Ищет ближайший "магнитный" край среди остальных окон для каждой оси
/// независимо (X и Y примагничиваются отдельно). Возвращает Some только по
/// тем осям, где нашлось окно в пределах SNAP_DISTANCE.
fn find_snap_target(
    data: &Dawn,
    window: &Window,
    free_loc: Point<i32, Logical>,
    riding: &[Window],
) -> Option<Point<i32, Logical>> {
    let size = data.space.element_geometry(window)?.size;
    let (w, h) = (size.w, size.h);

    let mut best_x: Option<(i32, i32)> = None;
    let mut best_y: Option<(i32, i32)> = None;

    for other in data.space.elements() {
        let is_self = other == window;
        if is_self { continue; }
        // Окна, которые едут вместе с нами, магнитной целью быть не могут:
        // расстояние до них за весь драг не менялось, и «подравнивание» по ним
        // означало бы просто сдвиг всей группы вбок.
        if riding.iter().any(|w| crate::dwindle::same_window(w, other)) { continue; }
        let og = match data.space.element_geometry(other) { Some(g) => g, None => continue };

        for cand in [
            og.loc.x + og.size.w,       // наш left → их right
            og.loc.x - w,               // наш right → их left
            og.loc.x,                   // наш left → их left
            og.loc.x + og.size.w - w,   // наш right → их right
        ] {
            let dist = (cand - free_loc.x).abs();
            if dist <= SNAP_DISTANCE && best_x.is_none_or(|(_, d)| dist < d) {
                best_x = Some((cand, dist));
            }
        }
        for cand in [
            og.loc.y + og.size.h,
            og.loc.y - h,
            og.loc.y,
            og.loc.y + og.size.h - h,
        ] {
            let dist = (cand - free_loc.y).abs();
            if dist <= SNAP_DISTANCE && best_y.is_none_or(|(_, d)| dist < d) {
                best_y = Some((cand, dist));
            }
        }
    }

    if best_x.is_none() && best_y.is_none() {
        return None;
    }
    let x = best_x.map(|(c, _)| c).unwrap_or(free_loc.x);
    let y = best_y.map(|(c, _)| c).unwrap_or(free_loc.y);
    Some(Point::from((x, y)))
}

/// Толкает окна, в которые "врезалось" перетаскиваемое (коллизия, пока кнопка
/// зажата — магнитирование отдельно, на отпускании). Каждый пересекающийся
/// сосед сдвигается по оси наименьшего перекрытия на величину перекрытия —
/// стандартная эвристика minimum-translation-vector, ощущается как толчок.
///
/// Толчок ЦЕПНОЙ: сдвинутое окно само становится толкателем для своих соседей,
/// те — для своих, и так волной по всей куче (BFS). Без каскада окно, стоящее
/// вплотную за толкнутым, просто оказывалось накрыто им.
///
/// `drag_vel` — текущая скорость перетаскивания (px/сек): окно, которого
/// коснулись первый раз (оно ещё стоит), получает не только сдвиг из-под
/// курсора, но и ИМПУЛЬС — уезжает по инерции и сам расталкивает дальше
/// (см. Dawn::impulse_window и resolve_fling_collisions). Раньше сосед просто
/// доезжал ровно до края и вставал — «пинка» не чувствовалось.
fn push_colliding_windows(
    data: &mut Dawn,
    dragged: &Window,
    dragged_loc: Point<i32, Logical>,
    drag_vel: Point<f64, Logical>,
) {
    let size = match data.space.element_geometry(dragged) { Some(g) => g.size, None => return };
    let dragged_rect = Rectangle::new(dragged_loc, size);

    // Геометрия соседей берётся ПОКОЯЩАЯСЯ (цель анимации), а не текущая:
    // толкнутое окно летит несколько кадров, и если каждый кадр пересчитывать
    // перекрытие по его живой позиции, оно толкается снова и снова (цель
    // убегает вместе с ним) — окно уползало рывками, пока курсор рядом.
    let mut others: Vec<(Window, Rectangle<i32, Logical>)> = data.space.elements()
        .filter(|w| {
            *w != dragged
        })
        .filter_map(|w| {
            let size = data.space.element_geometry(w)?.size;
            let loc = data.window_anim_target(w)
                .or_else(|| data.space.element_geometry(w).map(|g| g.loc))?;
            Some((w.clone(), Rectangle::new(loc, size)))
        })
        .collect();

    // Каждое окно волна двигает не больше одного раза — этим цепочка и
    // завершается (и не возникает пинг-понга «А толкнул Б, Б толкнул А»).
    let mut moved = vec![false; others.len()];
    // Нормаль толчка для каждого сдвинутого окна — по ней даётся импульс.
    let mut push_dir = vec![(0.0f64, 0.0f64); others.len()];
    let mut queue: std::collections::VecDeque<Rectangle<i32, Logical>> =
        std::collections::VecDeque::new();
    queue.push_back(dragged_rect);

    while let Some(mover) = queue.pop_front() {
        let mcx = mover.loc.x + mover.size.w / 2;
        let mcy = mover.loc.y + mover.size.h / 2;
        for i in 0..others.len() {
            if moved[i] { continue; }
            let geo = others[i].1;
            let overlap = match mover.intersection(geo) { Some(o) => o, None => continue };
            // Перекрытие в пару пикселей — шум округления, толкать нечего.
            if overlap.size.w <= 2 || overlap.size.h <= 2 { continue; }

            let ocx = geo.loc.x + geo.size.w / 2;
            let ocy = geo.loc.y + geo.size.h / 2;

            let (push_x, push_y) = if overlap.size.w < overlap.size.h {
                (if ocx >= mcx { overlap.size.w } else { -overlap.size.w }, 0)
            } else {
                (0, if ocy >= mcy { overlap.size.h } else { -overlap.size.h })
            };

            let new_geo = Rectangle::new(
                Point::from((geo.loc.x + push_x, geo.loc.y + push_y)),
                geo.size,
            );
            others[i].1 = new_geo;
            moved[i] = true;
            // signum() у 0.0 равен 1.0, поэтому нулевую ось задаём явно.
            push_dir[i] = if push_x != 0 {
                ((push_x as f64).signum(), 0.0)
            } else {
                (0.0, (push_y as f64).signum())
            };
            // Отъехавшее окно теперь само толкает тех, на кого наехало.
            queue.push_back(new_geo);
        }
    }

    for (i, (other, geo)) in others.into_iter().enumerate() {
        if !moved[i] { continue; }
        // Окно ещё стоит (анимации нет) — это ПЕРВОЕ касание в этой волне:
        // отдаём ему часть скорости драга вдоль оси толчка, и дальше оно едет
        // само, по инерции, толкая своих соседей. Пока оно уже летит, только
        // подправляем цель — доливать импульс каждый кадр значило бы
        // разгонять его без предела, пока курсор стоит рядом.
        let resting = data.window_anim_target(&other).is_none();
        let (nx, ny) = push_dir[i];
        let along = drag_vel.x * nx + drag_vel.y * ny;
        if resting && along > 0.0 {
            let speed = (along * 0.72).clamp(crate::anim::IMPULSE_MIN, crate::anim::IMPULSE_MAX);
            let stop = data.impulse_window(
                &other,
                Point::from((nx * speed, ny * speed)),
                crate::anim::glide_omega(speed),
            );
            // Инерционного доезда может не хватить, чтобы окно вылезло
            // из-под курсора — тогда добираем до расчётной MTV-позиции.
            if (geo.loc.x - stop.x) as f64 * nx + (geo.loc.y - stop.y) as f64 * ny <= 0.0 {
                continue;
            }
        }
        // Плавный LERP вместо телепорта — ощущается как инерция толчка
        // (переанимируется на каждый кадр, пока коллизия продолжается).
        data.animate_window_to_dur(&other, geo.loc, crate::anim::дуг::толчок_соседа());
        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
            tw.window == other
        }) {
            tw.float_position = geo.loc;
            tw.position = geo.loc;
        }
    }
}

impl MoveSurfaceGrab {
    /// Один шаг перетаскивания: окно (и его группа) переезжает под точку
    /// `cursor`, тайловые раскладки при этом свапаются, лента показывает шов
    /// вставки.
    ///
    /// Вынесено из `PointerGrab::motion` целиком, чтобы этим же кодом можно
    /// было двигать окно НЕ мышью. Жест «Super + два пальца» раньше имел
    /// собственную, куда более бедную реализацию прямо в input.rs: она умела
    /// только свободный перенос и потому в Tile и Columns просто отказывалась
    /// работать (см. `плавающее` в старой ветке). Теперь у мыши и у тачпада
    /// один и тот же код, а значит и одно и то же поведение.
    pub fn drag_to(&mut self, data: &mut Dawn, cursor: Point<f64, Logical>, time: u32) {
        let event = MotionEvent {
            location: cursor,
            serial: smithay::utils::SERIAL_COUNTER.next_serial(),
            time,
        };
        let event = &event;

        // Пока идёт драг, окно принадлежит мыши: анимации (в т.ч. толчок от
        // прилетевшего по инерции соседа) его не двигают — см. Dawn::dragged_window.
        data.dragged_window = Some(self.window.clone());

        // В обзоре столов раскладку держит overview.rs (arrange там выходит
        // сразу), поэтому окно там всегда таскается свободно — как в Float,
        // с переносом на стол под курсором на отпускании.
        let is_tiled = data.tile_config.layout != Layout::Float
            && !data.overview_active
            && data.tagged_windows.iter().any(|tw| {
                !tw.floating
                    && tw.window == self.window
            });

        // ── Columns/niri: тащим окно свободно и показываем, куда оно встанет ──
        // В niri перетаскивание не меняет соседей местами: окно «вынимается» из
        // полосы, а подсказка показывает шов между колонками или стопку, куда
        // оно попадёт на отпускании (см. columns_insert_hint_rect и button()).
        // Остальные тайловые раскладки продолжают работать свапами — их не
        // трогаем.
        if is_tiled && data.tile_config.layout == Layout::Columns {
            data.columns_drag_hint = Some(event.location.x);
            let delta = event.location - self.start_data.location;
            let mut loc = (self.initial_window_location.to_f64() + delta).to_i32_round();
            // Выше первого этажа и ниже последнего ленты нет — туда окно не
            // пускаем (см. columns_clamp_to_strip). Между этажами ходить можно:
            // это и есть перенос окна на соседний стол, он применяется на
            // отпускании в finish().
            if let Some(size) = data.space.element_geometry(&self.window).map(|g| g.size) {
                loc.y = data.columns_clamp_to_strip(loc.y, size.h);
            }
            data.space.map_element(self.window.clone(), loc, true);
            data.request_redraw();
            return;
        }

        if is_tiled {
            // Свап только когда курсор зашёл в «ядро» покоящегося слота соседа
            // и с прошлого свапа прошёл SWAP_COOLDOWN — раскладка успевает
            // доехать между перестановками, а на стыке окон нет пинг-понга.
            let ready = self.last_swap.is_none_or(|t| t.elapsed() >= SWAP_COOLDOWN);
            if ready {
                if let Some(target_window) = tiled_swap_target(data, &self.window, event.location) {
                    // Свап в структуре АКТИВНОЙ раскладки (BSP-дерево / колонки /
                    // стопка монокля) — только он и переживает arrange.
                    if data.swap_tiled_windows(&self.window, &target_window) {
                        self.last_swap = Some(Instant::now());
                        data.arrange();
                        data.request_redraw();
                        tracing::debug!("dawn: tiled drag swap");
                    }
                }
            }
            // Окно ЕДЕТ ЗА КУРСОРОМ, как в Hyprland: соседи перестраиваются под
            // него свапами выше, а сама «плитка» всё это время висит на мыши.
            // Раньше тайловое окно во время драга стояло в своём слоте намертво
            // (двигались только соседи), и это читалось как «в тайлинге окна
            // вообще не таскаются» — обратной связи под курсором не было
            // никакой. Строго ПОСЛЕ arrange: тот раскладывает все окна стола,
            // включая наше, и вернул бы его в слот прямо посреди драга.
            // Анимации перетаскиваемое окно не трогают (см. dragged_window),
            // так что map_element держится до отпускания, а на отпускании
            // arrange() в button() вернёт его в слот плавным перелётом.
            let delta = event.location - self.start_data.location;
            let loc = (self.initial_window_location.to_f64() + delta).to_i32_round();
            data.space.map_element(self.window.clone(), loc, true);
            data.request_redraw();
        } else {
            // Floating: свободное перемещение. Окно всегда следует за мышью
            // 1:1 (никакого залипания в процессе) — коллизия (2.1, Super+S)
            // толкает соседей, магнитирование применяется один раз при
            // отпускании кнопки (см. button()), не во время перетаскивания.
            let delta = event.location - self.start_data.location;
            let mut free_loc = (self.initial_window_location.to_f64() + delta).to_i32_round();

            // ── Обзор столов: окно ходит ТОЛЬКО внутри рабочих столов ──
            // Целевой стол — БЛИЖАЙШИЙ к курсору, поэтому окно можно перетащить
            // на другой стол, но не за рамки столов: позиция зажимается в
            // прямоугольник целевого стола. Строгое попадание в прямоугольник
            // тут не годится: в зазоре между столами окно замирало бы, и
            // перенос срабатывал «через раз».
            //
            // Тег окна во время драга НЕ меняем (это делает overview_reassign на
            // отпускании): ему нужно знать, с какого стола окно уехало, чтобы
            // вынуть его из BSP-дерева стола-донора.
            if data.overview_active {
                if let Some(mask) = data.overview_nearest_workspace(event.location) {
                    if let Some(size) = data.space.element_geometry(&self.window).map(|g| g.size) {
                        free_loc = data.overview_clamp_loc(mask, free_loc, size);
                    }
                }
            }

            // Лента: ПЛАВАЮЩЕЕ окно тоже принадлежит этажам, а не пустоте между
            // ними. Тайловое зажимается веткой выше, но окно, сделанное
            // плавающим руками (Super+V), приходило сюда и улетало за ленту
            // ровно так же.
            if data.tile_config.layout == Layout::Columns && !data.overview_active {
                if let Some(size) = data.space.element_geometry(&self.window).map(|g| g.size) {
                    free_loc.y = data.columns_clamp_to_strip(free_loc.y, size.h);
                }
            }

            // Разрыв ленты (2.5): вертикальный вылет за высоту окна — тащим
            // окно из его горизонтальной ленты, соседи справа сшиваются один раз.
            // В обзоре не трогаем: «сшивание» двигает чужие окна, и они уехали
            // бы за рамку своего стола.
            if !self.torn_from_ribbon && !data.overview_active {
                if let Some(win_size) = data.space.element_geometry(&self.window).map(|g| g.size) {
                    let win_size: smithay::utils::Size<i32, Logical> = win_size;
                    let dy: i32 = free_loc.y - self.initial_window_location.y;
                    if dy.abs() > win_size.h {
                        self.torn_from_ribbon = true;
                        stitch_ribbon_gap(data, &self.window, self.initial_window_location, win_size.w);
                    }
                }
            }

            // Трекаем скорость окна (в canvas-пикселях) для инерции на отпускании.
            let step = Point::from((
                (free_loc.x - self.last_loc.x) as f64,
                (free_loc.y - self.last_loc.y) as f64,
            ));
            self.velocity.push(event.time, step);
            self.last_loc = free_loc;

            {
                data.space.map_element(self.window.clone(), free_loc, true);
                // Расталкивание соседей (Super+S) — тоже только вне обзора:
                // толкнутый сосед уехал бы за рамку своего стола.
                if data.is_snapping_enabled && !data.overview_active {
                    push_colliding_windows(data, &self.window, free_loc, self.velocity.launch_velocity());
                }
                // Сохраняем float-позицию и помечаем как вручную размещённое.
                // В обзоре — не сохраняем: там координаты обзорные (ячейка стола
                // в сетке), и записать их во float_position значило бы, что
                // после выхода окно «разлетится» куда-то в сетку обзора.
                if !data.overview_active {
                    if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                        tw.window == self.window
                    }) {
                        tw.float_position = free_loc;
                        tw.float_position_set = true;
                    }
                }

                // "Созвездие" (Super+G): остальные окна группы едут той же дельтой,
                // что и перетаскиваемое — группа двигается как единое целое.
                if !self.group_initial.is_empty() {
                    for (member, init_loc) in &self.group_initial {
                        let mut member_loc = (init_loc.to_f64() + delta).to_i32_round();
                        // В обзоре каждый член группы остаётся в рамке СВОЕГО стола.
                        if data.overview_active {
                            if let (Some(mask), Some(size)) = (
                                data.overview_mask_of_window(member),
                                data.space.element_geometry(member).map(|g| g.size),
                            ) {
                                member_loc = data.overview_clamp_loc(mask, member_loc, size);
                            }
                        }
                        data.space.map_element(member.clone(), member_loc, false);
                        if data.overview_active {
                            continue; // обзорные координаты во float не пишем
                        }
                        if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                            &tw.window == member
                        }) {
                            tw.float_position = member_loc;
                            tw.position = member_loc;
                            tw.float_position_set = true;
                        }
                    }
                    self.обновить_призраков(data);
                }
            }
        }
    }

    /// Пересобрать подсказку «куда встанут остальные».
    ///
    /// Группа (выделение + созвездие) едет по базе и уже стоит на своих местах —
    /// казалось бы, показывать нечего. Но именно этого и не хватало: пока
    /// тащишь одно окно, остальные могут быть за краем экрана, под чужими
    /// окнами или в другом углу холста, и «едут пять» ощущается как рывок
    /// вслепую. Тонкий контур по каждому члену ПЛЮС общая рамка вокруг всей
    /// грозди показывают её целиком — рамка видна даже тогда, когда сами окна
    /// не видны: она пересекает экран.
    fn обновить_призраков(&self, data: &mut Dawn) {
        let mut рамки: Vec<Rectangle<i32, Logical>> = Vec::new();
        for (member, _) in &self.group_initial {
            if let Some(g) = data.space.element_geometry(member) {
                рамки.push(g);
            }
        }
        if рамки.is_empty() {
            data.призраки_группы.clear();
            return;
        }
        // Общая рамка считается и по самому перетаскиваемому окну: гроздь — это
        // всё, что едет, а не только «остальные».
        let mut объединение = data.space.element_geometry(&self.window)
            .into_iter()
            .chain(рамки.iter().copied())
            .reduce(|a, b| a.merge(b));
        if let Some(общая) = объединение.take() {
            рамки.push(общая);
        }
        data.призраки_группы = рамки;
        data.request_redraw();
    }

    /// Посадка окна на отпускании: тот же код и для мыши, и для жеста.
    ///
    /// Здесь живёт всё, что делает драг завершённым, — перенос на стол в
    /// обзоре, вставка в ленту, доводка тайловой раскладки, магнитирование и
    /// инерция. Раньше это было телом `PointerGrab::button`, и тачпадному
    /// жесту не доставалось ничего из перечисленного.
    pub fn finish(&mut self, data: &mut Dawn) {
        // Драг кончился — окно снова принадлежит анимациям (в пути мыши это
        // делает PointerGrab::unset, но жест туда не заходит).
        data.dragged_window = None;
        // Подсказка живёт ровно на время драга. Чистить её надо ЗДЕСЬ, в общей
        // точке завершения: и мышь, и тачпадный жест приходят сюда, а вот
        // PointerGrab::unset жест не зовёт вовсе.
        if !data.призраки_группы.is_empty() {
            data.призраки_группы.clear();
            data.request_redraw();
        }

        // В обзоре столов: отпустили перетаскивание → окно переезжает на
        // воркспейс того бэнда, куда попало (вверх/вниз меняет стол).
        if data.overview_active {
            data.overview_reassign(&self.window);
            return;
        }

        let is_tiled_win = data.tile_config.layout != Layout::Float
            && data.tagged_windows.iter().any(|tw| {
                !tw.floating
                    && tw.window == self.window
            });

        // Columns/niri: вставляем окно туда, куда показывала подсказка.
        //
        // Стол выбираем по ЭТАЖУ, на котором окно отпустили: вертикаль ленты —
        // это и есть переключение столов, и перетаскивание обязано её
        // учитывать. Раньше тут стоял безусловный `columns_drop_window`, то
        // есть окно ВСЕГДА возвращалось на текущий стол, куда бы его ни увели.
        if is_tiled_win && data.tile_config.layout == Layout::Columns {
            let x = data.columns_drag_hint.take().unwrap_or(data.pointer_location.x);
            let y = data.space.element_geometry(&self.window)
                .map(|g| (g.loc.y + g.size.h / 2) as f64)
                .unwrap_or(data.pointer_location.y);
            match data.columns_tag_at_y(y) {
                Some(tag) => data.columns_drop_window_on_ws(&self.window, tag, x),
                None => data.columns_drop_window(&self.window, x),
            }
            data.request_plane_reset();
            return;
        }
        data.columns_drag_hint = None;

        // Тайловое окно: свапы уже применены в motion, осталось только
        // дать раскладке доехать. Магнитирование и инерция — это про
        // свободный холст, тайловое окно они сдвинули бы со слота.
        if is_tiled_win {
            data.arrange();
            data.request_plane_reset();
            data.request_redraw();
            return;
        }

        // Окно созвездия увели руками — гроздь «растащена», и следующий
            // Super+D по ней должен собрать её заново, а не разбирать (см.
            // TaggedWindow::constellation_torn). Тащили всё созвездие целиком
            // (оно попало в выделение) — взаимное расположение не нарушено,
            // метку не ставим.
        let утащили_часть = {
            let ехали: Vec<Window> = self.group_initial.iter()
                .map(|(w, _)| w.clone())
                .chain(std::iter::once(self.window.clone()))
                .collect();
            data.constellation_members_excluding(&self.window)
                .iter()
                .any(|m| !ехали.contains(m))
        };
        if утащили_часть {
            data.mark_constellation_torn(&self.window);
        }

        // Магнитирование (2.1): один раз при отпускании подравниваем край
        // к ближайшему соседу в пределах SNAP_DISTANCE. Со своим тумблером
        // (Super+M) — раньше висело на флаге коллизии, и включить расталкивание
        // без прилипания было нельзя.
        let riding: Vec<Window> = self.group_initial.iter().map(|(w, _)| w.clone()).collect();
        let mut snapped_applied = false;
        // Скорость отпускания решает, ЧТО это было: медленно подведённое к
        // соседу окно кладут (магнитим), брошенное — бросают. Раньше магнит
        // побеждал всегда, и бросок мимо окна, случайно прошедший в 20 px от
        // чужого края, глох на месте: летел-летел и прилип. На холсте, где
        // окон много, под такой край попадаешь постоянно.
        let бросок = self.velocity.launch_velocity();
        let скорость_броска = (бросок.x * бросок.x + бросок.y * бросок.y).sqrt();
        if data.is_magnetism_enabled && скорость_броска < SNAP_MAX_SPEED {
            if let Some(loc) = data.space.element_geometry(&self.window).map(|g| g.loc) {
                if let Some(snapped) = find_snap_target(data, &self.window, loc, &riding) {
                    data.space.map_element(self.window.clone(), snapped, true);
                    if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                        tw.window == self.window
                    }) {
                        tw.float_position = snapped;
                    }
                    // Группа держит строй и на посадке: остальных двигаем
                    // ровно тем же сдвигом, иначе примагниченное окно
                    // уезжало из группы на величину подравнивания.
                    self.shift_group(data, snapped - loc);
                    snapped_applied = true;
                }
            }
        }

        // Инерция перетаскивания: окно доезжает по инерции после отпускания
        // (не применяется, если сработало магнитирование — там нужна
        // точная посадка на край соседа).
        if !snapped_applied {
            let v = бросок; // px/сек в canvas
            let speed = скорость_броска;
            // Ниже — окно просто положили. Порог опущен со 120: на 120 px/сек
            // рука уже отчётливо ведёт окно, и обрубать доезд на этой границе
            // значит терять самые частые, короткие подбросы.
            const MIN_FLING: f64 = 60.0;
            if speed > MIN_FLING {
                // Пружина стартует ровно с той скоростью, с какой курсор
                // отпустил окно, и тормозит экспоненциально. Раньше тут был
                // ease-out на фиксированную дистанцию: его стартовая
                // скорость (3·d/dur) не совпадала со скоростью драга, и в
                // момент отпускания окно заметно «дёргалось» вперёд.
                // ω считается от скорости так, что путь доезда растёт вместе
                // с ней до anim::fling_distance() (см. glide_omega).
                let omega = crate::anim::glide_omega(speed);
                let target = data.fling_window(&self.window, v, omega);
                if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                    tw.window == self.window
                }) {
                    tw.float_position = target;
                    tw.position = target;
                    tw.float_position_set = true;
                }
                // Выделение доезжает тем же броском. Одинаковые скорость и
                // ω дают одинаковый путь (|v|/ω) и одинаковую кривую, так
                // что взаимное расположение сохраняется. Без этого инерцию
                // получало только окно под курсором — и оно улетало из
                // группы, хотя весь драг они ехали вместе.
                for (member, _) in &self.group_initial {
                    let member_target = data.fling_window(member, v, omega);
                    if let Some(tw) = data.tagged_windows.iter_mut().find(|tw| {
                        &tw.window == member
                    }) {
                        tw.float_position = member_target;
                        tw.position = member_target;
                        tw.float_position_set = true;
                    }
                }
            }
        }

        // Сброс plane-кэша на границе драга — сглаживает возможный
        // "хвост"/тень от предыдущей позиции на некоторых DRM-планах.
        data.request_plane_reset();
        data.request_redraw();
    }
}

impl PointerGrab<Dawn> for MoveSurfaceGrab {
    fn motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        _focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        self.drag_to(data, event.location, event.time);
    }

    fn relative_motion(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        focus: Option<(WlSurface, Point<f64, Logical>)>,
        event: &RelativeMotionEvent,
    ) {
        handle.relative_motion(data, focus, event);
    }

    fn button(
        &mut self,
        data: &mut Dawn,
        handle: &mut PointerInnerHandle<'_, Dawn>,
        event: &ButtonEvent,
    ) {
        handle.button(data, event);
        const BTN_LEFT: u32 = 0x110;
        if !handle.current_pressed().contains(&BTN_LEFT) {
            handle.unset_grab(self, data, event.serial, event.time, true);
            self.finish(data);
            handle.frame(data);
        }
    }

    fn axis(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, details: AxisFrame) {
        handle.axis(data, details)
    }
    fn frame(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>) {
        handle.frame(data);
    }
    fn gesture_swipe_begin(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureSwipeBeginEvent) {
        handle.gesture_swipe_begin(data, event)
    }
    fn gesture_swipe_update(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureSwipeUpdateEvent) {
        handle.gesture_swipe_update(data, event)
    }
    fn gesture_swipe_end(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureSwipeEndEvent) {
        handle.gesture_swipe_end(data, event)
    }
    fn gesture_pinch_begin(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GesturePinchBeginEvent) {
        handle.gesture_pinch_begin(data, event)
    }
    fn gesture_pinch_update(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GesturePinchUpdateEvent) {
        handle.gesture_pinch_update(data, event)
    }
    fn gesture_pinch_end(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GesturePinchEndEvent) {
        handle.gesture_pinch_end(data, event)
    }
    fn gesture_hold_begin(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureHoldBeginEvent) {
        handle.gesture_hold_begin(data, event)
    }
    fn gesture_hold_end(&mut self, data: &mut Dawn, handle: &mut PointerInnerHandle<'_, Dawn>, event: &GestureHoldEndEvent) {
        handle.gesture_hold_end(data, event)
    }
    fn start_data(&self) -> &PointerGrabStartData<Dawn> {
        &self.start_data
    }
    fn unset(&mut self, data: &mut Dawn) {
        data.dragged_window = None;
    }
}
