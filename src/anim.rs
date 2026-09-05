use std::time::{Duration, Instant};

use smithay::desktop::Window;
use smithay::utils::{Logical, Point, Rectangle};

use crate::canvas::zoom_anchor_camera;
use crate::state::Parallax;
use crate::tiling::Layout;

/// Скорость броска (px/сек), на которой окно улетает на всю дальность.
/// Быстрее — не дальше: выше этой отметки путь упирается в потолок, и очень
/// резкий флик не зашвыривает окно за горизонт холста.
pub const FLING_FULL_SPEED: f64 = 7000.0;
/// Дальность броска по умолчанию (px в координатах холста) — сколько окно
/// проезжает после отпускания на скорости FLING_FULL_SPEED и выше.
///
/// Раньше здесь стояло 420 под именем MAX_GLIDE, и это был не «потолок на
/// всякий случай», а фактический предел ЛЮБОГО броска: сколько ни маши мышью,
/// окно проезжало полэкрана и вставало. На бесконечном холсте это читается как
/// «инерции нет» — бросить окно в дальний угол было нельзя в принципе.
pub const FLING_DISTANCE: f64 = 2000.0;

/// Дальность броска в промилле от FLING_DISTANCE. Атомик — по той же причине,
/// что и ТЕМП: ω спрашивают из мест, куда `&Parallax` не передан.
static ДАЛЬНОСТЬ: AtomicU32 = AtomicU32::new(1000);

/// Применить дальность броска из конфига (`set{ fling_distance = ... }`,
/// где 1.0 — как задумано). 0 гасит инерцию окон совсем — это законный выбор
/// вкуса, поэтому нижняя граница именно ноль, а не «почти ноль».
pub fn set_fling(k: f64) {
    let k = if k.is_finite() { k.clamp(0.0, 8.0) } else { 1.0 };
    ДАЛЬНОСТЬ.store((k * 1000.0).round() as u32, Ordering::Relaxed);
}

/// Дальность броска (px) с учётом ручки конфига.
pub fn fling_distance() -> f64 {
    FLING_DISTANCE * (ДАЛЬНОСТЬ.load(Ordering::Relaxed) as f64 / 1000.0)
}

/// ω для полёта с заданной стартовой скоростью (px/сек).
///
/// Путь доезда = |v|/ω, поэтому ω = max(FLING_FULL_SPEED, |v|) / дальность
/// даёт путь, который РАСТЁТ ЛИНЕЙНО со скоростью броска до самой дальности и
/// только там упирается в потолок. Прошлая формула (пол по ω плюс потолок по
/// пути) вела себя иначе: до 3360 px/сек путь был пропорционален скорости, а
/// дальше намертво вставал на 420 px — то есть весь диапазон «бросить сильно»
/// схлопывался в одно значение.
pub fn glide_omega(speed: f64) -> f64 {
    glide_omega_for(speed, fling_distance())
}

/// Та же формула с явной дальностью. Отдельно от `glide_omega` ради тестов:
/// дальность живёт в process-wide атомике, и тест, который крутил бы её ручкой,
/// ломал бы соседние тесты, идущие параллельно в том же процессе.
pub fn glide_omega_for(speed: f64, distance: f64) -> f64 {
    if distance <= 1.0 {
        // Инерция выключена ручкой: ω заведомо больше любого dt-шага, окно
        // останавливается там же, где его отпустили.
        return 1000.0;
    }
    FLING_FULL_SPEED.max(speed) / distance
}

// ── Длительности: одно место на весь компоновщик ─────────────────────────────
//
// Раньше каждая анимация носила свою константу прямо в вызове —
// `Duration::from_millis(220)` в columns.rs, 300 и 360 в state.rs, 180 и 600 в
// tiling.rs, 260 и 320 в selection.rs. Значения расползлись по шести файлам,
// и одинаковые по смыслу движения (перелёт камеры к столу и к закладке) давно
// шли с разной скоростью просто потому, что их правили в разные дни. Сделать
// после этого «все анимации спокойнее» означало обойти шесть файлов и ничего
// не забыть.
//
// Теперь длительности названы по ДВИЖЕНИЮ, а не по числу, и лежат здесь.
// Вызывающие зовут `anim::дуг::перелёт_к_столу()`, а не помнят миллисекунды.

use std::sync::atomic::{AtomicU32, Ordering};

/// Общий темп анимаций в промилле: 1000 — как задумано, больше — медленнее и
/// спокойнее, меньше — резче. Крутится из config.lua (`set{ anim_speed = ... }`,
/// где 1.0 — норма), поэтому вкус подбирается перечитыванием конфига
/// (Super+Shift+C), а не пересборкой.
///
/// Атомик, а не поле в `Parallax`: длительность спрашивают из мест, которым `&Parallax`
/// не передан (конструкторы анимаций), и протаскивать его туда ради одного
/// числа — шум на десятки сигнатур. Значение process-wide по своей природе.
static ТЕМП: AtomicU32 = AtomicU32::new(1000);

/// Применить темп из конфига. Значения вне [0.2, 4.0] — почти наверняка описка
/// (нуль остановил бы анимации совсем), поэтому зажимаются.
pub fn set_tempo(k: f64) {
    let k = if k.is_finite() { k.clamp(0.2, 4.0) } else { 1.0 };
    ТЕМП.store((k * 1000.0).round() as u32, Ordering::Relaxed);
}

pub fn tempo() -> f64 {
    ТЕМП.load(Ordering::Relaxed) as f64 / 1000.0
}

/// Длительности движений. Числа — «как задумано» при темпе 1.0; наружу выходят
/// уже с поправкой.
pub mod дуг {
    use super::{tempo, Duration};

    fn мс(ms: u64) -> Duration {
        Duration::from_secs_f64(ms as f64 / 1000.0 * tempo())
    }

    /// Камера подъезжает к окну, получившему фокус (Super+стрелки, Alt+Tab).
    pub fn прыжок_к_окну() -> Duration { мс(280) }
    /// Перелёт в кадр другого стола (Super+цифра) — дорога длиннее, чем до
    /// соседнего окна, и торопить её незачем.
    pub fn перелёт_к_столу() -> Duration { мс(400) }
    /// Прыжок к закладке камеры. То же движение, что и к столу: раньше шло
    /// на 320 против 360 без всякой причины.
    pub fn прыжок_к_закладке() -> Duration { мс(400) }
    /// Шаг панорамирования с клавиатуры.
    pub fn шаг_пана() -> Duration { мс(420) }
    /// Камера догоняет активную колонку/строку в ленте (Columns).
    pub fn подводка_ленты() -> Duration { мс(300) }
    /// Вход и выход обзора столов.
    pub fn обзор() -> Duration { мс(360) }

    /// Окно едет в свой слот раскладки: сборка тайлинга, переразметка после
    /// закрытия соседа.
    pub fn сборка_тайлинга() -> Duration { мс(260) }
    /// Разлёт окон во Float — «красивое» движение, ему можно быть длиннее.
    pub fn разлёт_во_флоат() -> Duration { мс(560) }
    /// Окно уступает место: толчок соседа при коллизии, обмен местами.
    pub fn толчок_соседа() -> Duration { мс(200) }
    /// Сбор выделения в созвездие и его разборка.
    pub fn созвездие() -> Duration { мс(320) }
    /// Подсветка фокуса переезжает на новое окно.
    pub fn аура_фокуса() -> Duration { мс(200) }
    /// Окно открывается: всплытие из точки в свой размер.
    pub fn открытие_окна() -> Duration { мс(280) }
    /// Окно закрывается: снимок гаснет и чуть сжимается (см. close.rs).
    /// Заметно длиннее открытия НАМЕРЕННО: открытие обязано успеть за рукой,
    /// а закрытие ничего не ждёт — спокойное угасание читается лучше рывка.
    pub fn закрытие_окна() -> Duration { мс(360) }
    /// Перелистывание столов вбок (Hyprland-style slide): уходящий стол уезжает
    /// за край, приходящий въезжает с другой стороны. Длиннее прыжка к окну —
    /// движение крупное, через весь экран, и на коротком времени оно
    /// превращается в мельтешение вместо перелистывания.
    pub fn слайд_стола() -> Duration { мс(340) }
    /// Смена уровня зума по команде: лупа (Super+Space), взгляд сверху
    /// (Super+пробел удержанием). Раньше шли на 220–250 — теперь одинаково и
    /// заметно спокойнее: зум меняет ВЕСЬ кадр, и торопить его — верный способ
    /// потерять, где ты был.
    pub fn смена_зума() -> Duration { мс(380) }
    /// Вход в композитор: холст выплывает из темноты и доезжает до своего
    /// зума (см. [`super::Вход`]). Единственное движение, которое человек
    /// видит целиком и ровно один раз за сеанс, — поэтому оно длиннее всех
    /// прочих. Короче секунды оно читается как мигание при старте, а не как
    /// появление рабочего места.
    pub fn вход() -> Duration { мс(1100) }
}

#[inline]
pub fn lerp(a: f64, b: f64, t: f64) -> f64 {
    a + (b - a) * t
}

/// Ease-out cubic: мгновенный старт, мягкая посадка.
///
/// Осталась для движений, которые ОТВЕЧАЮТ на действие и обязаны стартовать
/// без задержки. Камере она больше не достаётся — см. [`ease_calm`].
#[inline]
pub fn ease_out_cubic(t: f64) -> f64 {
    let inv = 1.0 - t.clamp(0.0, 1.0);
    1.0 - inv * inv * inv
}

/// Спокойная кривая камеры: 6t⁵−15t⁴+10t³ (smootherstep).
///
/// **Почему не ease-out-cubic, который был здесь раньше.** У него максимум
/// скорости приходится на t=0 и равен ТРЁМ средним: камера стартует рывком, а
/// дальше долго подползает. На неподвижной картинке разницы не видно, а на
/// холсте это ровно то, что читается как «дёрганые прыжки»: глаз ловит рывок
/// на старте и теряет, куда поехали.
///
/// У smootherstep нулевые и скорость, И УСКОРЕНИЕ на обоих концах, а пик
/// скорости — 1.875 среднего. Движение получается ровным по всей длине: камера
/// трогается с места, разгоняется, тормозит и встаёт — без единого рывка.
/// Ровно это и просили словом «спокойнее».
#[inline]
pub fn ease_calm(t: f64) -> f64 {
    let t = t.clamp(0.0, 1.0);
    t * t * t * (t * (t * 6.0 - 15.0) + 10.0)
}

/// Ease-out с лёгким перелётом: плашка выскакивает чуть больше себя и
/// садится обратно. Классический `back` с c1 = 1.70158 даёт перелёт ~10% —
/// для карточки под курсором это уже кривляние, поэтому c1 ослаблен вдвое
/// (перелёт ~4%): движение читается как «пружинка», а не как «мигнуло».
#[inline]
pub fn ease_out_back(t: f64) -> f64 {
    const C1: f64 = 0.85;
    const C3: f64 = C1 + 1.0;
    let inv = 1.0 - t.clamp(0.0, 1.0);
    1.0 + C3 * (-inv).powi(3) + C1 * inv * inv
}

/// Шаг доли 0…1 к цели за ФИКСИРОВАННОЕ время (с поправкой на темп).
///
/// Отличие от экспоненты, которой раньше ехали все шторки: у экспоненты
/// бесконечный хвост, доля подползает к цели всё медленнее, и любая кривая
/// сверху (`ease_out_*`) складывается с этим подползанием — движение выходит
/// вялым к концу. Линейная доля + кривая на отрисовке дают ровно ту форму
/// движения, которую задаёт кривая, и предсказуемую длительность.
fn шаг_доли(текущая: f64, цель: f64, dt: Duration, длительность: f64) -> f64 {
    let шаг = dt.as_secs_f64() / (длительность * tempo()).max(1e-3);
    if цель > текущая {
        (текущая + шаг).min(цель)
    } else {
        (текущая - шаг).max(цель)
    }
}

/// Camera-only fly-to (no zoom change). Used by focus snapping, bookmarks, minimap clicks.
pub struct CameraAnim {
    pub from: Point<f64, Logical>,
    pub to: Point<f64, Logical>,
    pub start: Instant,
    pub duration: Duration,
}

impl CameraAnim {
    pub fn new(from: Point<f64, Logical>, to: Point<f64, Logical>, duration: Duration) -> Self {
        Self { from, to, start: Instant::now(), duration }
    }

    /// Eased progress in [0, 1].
    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        ease_calm(elapsed / dur)
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    pub fn current(&self) -> Point<f64, Logical> {
        let t = self.t();
        Point::from((lerp(self.from.x, self.to.x, t), lerp(self.from.y, self.to.y, t)))
    }
}

/// Позиционная анимация окна — критически задемпфированная пружина.
///
/// Почему не ease-out-cubic с фиксированной длительностью (как было): цель у
/// окна меняется НА ЛЕТУ и часто (arrange() на каждый свап при драге, толчок
/// соседей на каждый кадр коллизии). Анимация с фиксированной длительностью
/// умеет только перезапуститься с нуля: скорость мгновенно падает до
/// «начальной для нового ease», прогресс обнуляется — окно ползёт
/// асимптотически, дёргается и до цели фактически не доезжает, пока цель
/// шевелится. Пружина же просто меняет цель, сохраняя текущую скорость:
/// перенацеливание непрерывно по скорости, а значит незаметно глазу — ровно
/// то, что даёт «hyprland-овское» ощущение.
///
/// Интегрируется точным решением (не Эйлером), поэтому стабильна при любом dt
/// и при любой частоте вызова tick (VBlank + 60Гц-таймер зовут её вперемешку).
pub struct PosAnim {
    /// Текущая позиция top-left окна в canvas-координатах.
    pub pos: Point<f64, Logical>,
    /// Текущая скорость, px/сек — то, что переживает смену цели.
    pub vel: Point<f64, Logical>,
    /// Куда едем. Публично: коллизия/свап считают геометрию по ЦЕЛИ, а не по
    /// текущему кадру полёта (иначе решение зависит от фазы анимации).
    pub target: Point<f64, Logical>,
    /// Частота пружины (1/сек): больше — резче. См. omega_for.
    omega: f64,
    /// Окно летит СВОБОДНО по инерции (бросок мышью, полученный толчок), а не
    /// едет в слот раскладки. Только такие окна участвуют в столкновениях на
    /// лету (см. resolve_fling_collisions) — иначе сборка тайлинга начала бы
    /// расталкивать всё вокруг.
    pub fling: bool,
    last: Instant,
    done: bool,
}

/// Порог «приехали» по позиции (px) и по скорости (px/сек).
const POS_EPS: f64 = 0.4;
const POS_VEL_EPS: f64 = 8.0;

/// ω, при котором критическая пружина проходит ~99% пути за `dur`
/// (для критического демпфирования остаток ~ (1+ωt)·e^{-ωt}, при ωt=7 это ~0.7%).
fn omega_for(dur: Duration) -> f64 {
    7.0 / dur.as_secs_f64().max(0.016)
}

/// Один шаг критически задемпфированной пружины, точное решение
/// x'' = -ω²x - 2ωx' : x(t) = (x₀ + (v₀ + ωx₀)t)·e^{-ωt}.
fn spring_step(x: f64, v: f64, omega: f64, dt: f64) -> (f64, f64) {
    let e = (-omega * dt).exp();
    let c = v + omega * x;
    ((x + c * dt) * e, (v - omega * c * dt) * e)
}

impl PosAnim {
    pub fn new(from: Point<f64, Logical>, target: Point<f64, Logical>, dur: Duration) -> Self {
        Self {
            pos: from,
            vel: Point::from((0.0, 0.0)),
            target,
            omega: omega_for(dur),
            fling: false,
            last: Instant::now(),
            done: false,
        }
    }

    /// Пружина с заданной стартовой скоростью и явной ω — для инерционного
    /// доезда после броска окна мышью. Если цель выбрана как `from + vel/ω`,
    /// то v₀ + ω·x₀ = 0 и решение вырождается в чистое e^{-ωt}: скорость на
    /// старте В ТОЧНОСТИ равна скорости курсора (никакого рывка в момент
    /// отпускания) и нет перелёта за цель.
    pub fn with_velocity(
        from: Point<f64, Logical>,
        target: Point<f64, Logical>,
        vel: Point<f64, Logical>,
        omega: f64,
    ) -> Self {
        Self { pos: from, vel, target, omega, fling: true, last: Instant::now(), done: false }
    }

    /// Продолжить свободный полёт с НОВОЙ скоростью из текущей точки: цель
    /// пересчитывается как `pos + vel/ω`, то есть решение снова вырождается в
    /// чистое e^{-ωt} — ни рывка, ни перелёта. Это и есть «удар»: столкновение
    /// меняет скорость, а точка остановки следует из неё, а не наоборот.
    pub fn coast(&mut self, vel: Point<f64, Logical>, omega: f64) {
        self.vel = vel;
        self.omega = omega;
        self.target = Point::from((self.pos.x + vel.x / omega, self.pos.y + vel.y / omega));
        self.fling = true;
        self.done = false;
    }

    /// Сменить цель, сохранив скорость. Та же цель (в пределах полпикселя) —
    /// no-op: пересоздавать анимацию каждый кадр незачем.
    pub fn retarget(&mut self, target: Point<f64, Logical>, dur: Duration) {
        if (self.target.x - target.x).abs() < 0.5 && (self.target.y - target.y).abs() < 0.5 {
            return;
        }
        self.target = target;
        self.omega = omega_for(dur);
        self.done = false;
    }

    /// Продвинуть на реальный прошедший dt. dt зажат сверху: после паузы
    /// (VT-switch, залипший кадр) окно должно просто оказаться у цели, а не
    /// пролететь её из-за одного гигантского шага.
    pub fn advance(&mut self, now: Instant) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64().min(0.1);
        self.last = now;
        if self.done || dt <= 0.0 {
            return;
        }
        let (dx, vx) = spring_step(self.pos.x - self.target.x, self.vel.x, self.omega, dt);
        let (dy, vy) = spring_step(self.pos.y - self.target.y, self.vel.y, self.omega, dt);
        if dx.abs() < POS_EPS && dy.abs() < POS_EPS && vx.abs() < POS_VEL_EPS && vy.abs() < POS_VEL_EPS {
            self.pos = self.target;
            self.vel = Point::from((0.0, 0.0));
            self.done = true;
        } else {
            self.pos = Point::from((self.target.x + dx, self.target.y + dy));
            self.vel = Point::from((vx, vy));
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }
}

/// Плавный зум колесом.
///
/// Щелчок колеса — мультипликативный шаг (×1.1), и раньше он применялся
/// мгновенно: каждый щелчок был скачком на 10%, отсюда рваное ощущение.
/// Теперь щелчки двигают ЦЕЛЬ, а масштаб подтягивается к ней экспоненциально,
/// поэтому поток щелчков сливается в одно непрерывное движение.
///
/// Интерполяция идёт по логарифму масштаба: шаги колеса умножают зум, и
/// линейное приближение шло бы заметно быстрее на дальнем конце диапазона,
/// чем у единицы.
pub struct ZoomGlide {
    pub target: f64,
    /// Точка холста под курсором и её место на экране. Держатся неподвижными
    /// весь доезд — поэтому зум идёт «в курсор», а не в центр экрана.
    pub anchor_canvas: Point<f64, Logical>,
    pub anchor_screen: Point<f64, Logical>,
    last: Instant,
}

/// Скорость подтягивания зума к цели (1/с) при темпе 1.0.
///
/// Было 18 (расхождение съедалось за ~0.2 с). На потоке щелчков колеса это
/// читалось как дрожь: зум успевал почти догнать цель между щелчками и каждый
/// раз трогался заново. При 12 (~0.3 с) соседние щелчки СЛИВАЮТСЯ в одно
/// движение — колесо крутят быстрее, чем зум доезжает, и получается ровное
/// приближение вместо серии рывков.
const ZOOM_GLIDE_OMEGA_BASE: f64 = 12.0;

/// То же с поправкой на общий темп: медленнее темп — меньше ω.
pub fn zoom_glide_omega() -> f64 {
    ZOOM_GLIDE_OMEGA_BASE / tempo()
}
/// Ниже этого расхождения по логарифму (≈0.05% масштаба) доезд закончен.
const ZOOM_GLIDE_EPS: f64 = 5e-4;

impl ZoomGlide {
    pub fn new(
        target: f64,
        anchor_canvas: Point<f64, Logical>,
        anchor_screen: Point<f64, Logical>,
    ) -> Self {
        Self { target, anchor_canvas, anchor_screen, last: Instant::now() }
    }

    /// Новый щелчок колеса: цель и якорь меняются, доезд не перезапускается —
    /// скорость остаётся непрерывной.
    pub fn retarget(
        &mut self,
        target: f64,
        anchor_canvas: Point<f64, Logical>,
        anchor_screen: Point<f64, Logical>,
    ) {
        self.target = target;
        self.anchor_canvas = anchor_canvas;
        self.anchor_screen = anchor_screen;
    }

    /// Кадр, в котором доезд ЗАКОНЧИТСЯ: (zoom, камера).
    ///
    /// Нужен тому, кто обрывает доезд досрочно и обязан оставить вид в
    /// осмысленной точке, а не на полпути (см.
    /// `Parallax::завершить_анимации_камеры`). Тот же смысл, что у
    /// [`ZoomAnim::target`].
    pub fn target_frame(&self) -> (f64, Point<f64, Logical>) {
        (
            self.target,
            crate::canvas::zoom_anchor_camera(
                self.anchor_canvas, self.anchor_screen, self.target,
            ),
        )
    }

    /// Шаг доезда. Возвращает новый масштаб, положение камеры, удерживающее
    /// якорь, и признак завершения.
    pub fn advance(&mut self, now: Instant, zoom: f64) -> (f64, Point<f64, Logical>, bool) {
        let dt = now.saturating_duration_since(self.last).as_secs_f64().min(0.1);
        self.last = now;
        let camera_for = |z: f64| crate::canvas::zoom_anchor_camera(
            self.anchor_canvas,
            self.anchor_screen,
            z,
        );
        if dt <= 0.0 {
            return (zoom, camera_for(zoom), false);
        }
        let diff = self.target.ln() - zoom.ln();
        if diff.abs() < ZOOM_GLIDE_EPS {
            return (self.target, camera_for(self.target), true);
        }
        let t = 1.0 - (-zoom_glide_omega() * dt).exp();
        let next = (zoom.ln() + diff * t).exp();
        (next, camera_for(next), false)
    }
}

/// Zoom fly-to that keeps a canvas anchor point pinned to a screen point
/// throughout the animation (e.g. hold-to-zoom keeps the screen center fixed).
pub struct ZoomAnim {
    pub anchor_canvas: Point<f64, Logical>,
    pub anchor_screen: Point<f64, Logical>,
    pub from_zoom: f64,
    pub to_zoom: f64,
    pub start: Instant,
    pub duration: Duration,
    /// Если Some — камера едет ЛИНЕЙНО из первой точки во вторую, а якорь не
    /// используется. Нужно, когда конечный кадр задан целиком (niri-обзор
    /// вписывает всю ленту): с якорным пересчётом камера прыгала бы на первом
    /// же кадре, чтобы прижать якорь к точке экрана.
    cam: Option<(Point<f64, Logical>, Point<f64, Logical>)>,
}

impl ZoomAnim {
    pub fn new(
        anchor_canvas: Point<f64, Logical>,
        anchor_screen: Point<f64, Logical>,
        from_zoom: f64,
        to_zoom: f64,
        duration: Duration,
    ) -> Self {
        Self {
            anchor_canvas, anchor_screen, from_zoom, to_zoom,
            start: Instant::now(), duration, cam: None,
        }
    }

    /// Зум + перелёт камеры одной анимацией: и зум, и камера линейно едут к
    /// заданному кадру. В отличие от `new` конечная камера задана явно, поэтому
    /// кадр в конце ровно тот, что просили (см. обзор ленты в overview.rs).
    pub fn new_pan(
        from_cam: Point<f64, Logical>,
        to_cam: Point<f64, Logical>,
        from_zoom: f64,
        to_zoom: f64,
        duration: Duration,
    ) -> Self {
        Self {
            anchor_canvas: from_cam,
            anchor_screen: Point::from((0.0, 0.0)),
            from_zoom,
            to_zoom,
            start: Instant::now(),
            duration,
            cam: Some((from_cam, to_cam)),
        }
    }

    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        // Той же спокойной кривой, что и камера: зум и перелёт часто идут ОДНОЙ
        // анимацией (new_pan), и разные кривые у них означали бы, что кадр
        // приезжает не тем движением, каким уезжал.
        ease_calm(elapsed / dur)
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// Кадр, в котором анимация ЗАКОНЧИТСЯ: (zoom, камера).
    ///
    /// Нужен тем, кто снимает состояние вида, пока анимация ещё летит
    /// (уход с воркспейса — см. Parallax::view_frame_target): запоминать
    /// промежуточный кадр нельзя, стол потом открылся бы на полпути.
    pub fn target(&self) -> (f64, Point<f64, Logical>) {
        let cam = match self.cam {
            Some((_, to)) => to,
            None => zoom_anchor_camera(self.anchor_canvas, self.anchor_screen, self.to_zoom),
        };
        (self.to_zoom, cam)
    }

    /// Returns (zoom, camera) for the current instant.
    pub fn current(&self) -> (f64, Point<f64, Logical>) {
        let t = self.t();
        let zoom = lerp(self.from_zoom, self.to_zoom, t);
        let cam = match self.cam {
            Some((from, to)) => Point::from((lerp(from.x, to.x, t), lerp(from.y, to.y, t))),
            None => zoom_anchor_camera(self.anchor_canvas, self.anchor_screen, zoom),
        };
        (zoom, cam)
    }
}

/// A rectangle glow (Focus Aura, minimap highlight, ...) fly-to between two
/// canvas-space rectangles (position + size interpolated together).
pub struct RectAnim {
    pub from_pos: Point<f64, Logical>,
    pub from_size: (f64, f64),
    pub to_pos: Point<f64, Logical>,
    pub to_size: (f64, f64),
    pub start: Instant,
    pub duration: Duration,
}

impl RectAnim {
    pub fn new(
        from_pos: Point<f64, Logical>,
        from_size: (f64, f64),
        to_pos: Point<f64, Logical>,
        to_size: (f64, f64),
        duration: Duration,
    ) -> Self {
        Self { from_pos, from_size, to_pos, to_size, start: Instant::now(), duration }
    }

    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        // Здесь, в отличие от камеры, нужен НЕМЕДЛЕННЫЙ старт: это движение —
        // прямой ответ на действие (окно открылось, фокус переехал), и пауза
        // на разгоне читалась бы как подвисание. Спокойная кривая с её
        // нулевой скоростью в нуле хороша ровно там, где движение ведёт взгляд
        // (кадр), а не догоняет его.
        ease_out_cubic(elapsed / dur)
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    pub fn current(&self) -> (Point<f64, Logical>, (f64, f64)) {
        let t = self.t();
        let pos = Point::from((lerp(self.from_pos.x, self.to_pos.x, t), lerp(self.from_pos.y, self.to_pos.y, t)));
        let size = (lerp(self.from_size.0, self.to_size.0, t), lerp(self.from_size.1, self.to_size.1, t));
        (pos, size)
    }
}

/// Открытие окна "с ростом" (Hyprland-style): размер растёт от 40% до 100%
/// целевого, центр остаётся фиксированным. Растим размер настоящими xdg
/// configure (не шейдер) — безопасно для рендер-пайплайна, чуть более
/// "ступенчато" на медленных клиентах, зато не трогает отрисовку контента.
pub struct OpenAnim {
    pub center: Point<f64, Logical>,
    pub target_size: (f64, f64),
    pub start: Instant,
    pub duration: Duration,
}

const OPEN_ANIM_MIN_SCALE: f64 = 0.4;

impl OpenAnim {
    pub fn new(center: Point<f64, Logical>, target_size: (f64, f64), duration: Duration) -> Self {
        Self { center, target_size, start: Instant::now(), duration }
    }

    fn t(&self) -> f64 {
        let elapsed = self.start.elapsed().as_secs_f64();
        let dur = self.duration.as_secs_f64().max(1e-6);
        // Здесь, в отличие от камеры, нужен НЕМЕДЛЕННЫЙ старт: это движение —
        // прямой ответ на действие (окно открылось, фокус переехал), и пауза
        // на разгоне читалась бы как подвисание. Спокойная кривая с её
        // нулевой скоростью в нуле хороша ровно там, где движение ведёт взгляд
        // (кадр), а не догоняет его.
        ease_out_cubic(elapsed / dur)
    }

    pub fn is_done(&self) -> bool {
        self.start.elapsed() >= self.duration
    }

    /// (позиция top-left, (w, h)) для текущего момента.
    pub fn current(&self) -> (Point<i32, Logical>, (i32, i32)) {
        let scale = OPEN_ANIM_MIN_SCALE + (1.0 - OPEN_ANIM_MIN_SCALE) * self.t();
        let w = (self.target_size.0 * scale).round().max(1.0) as i32;
        let h = (self.target_size.1 * scale).round().max(1.0) as i32;
        let loc = Point::from((
            (self.center.x - w as f64 / 2.0).round() as i32,
            (self.center.y - h as f64 / 2.0).round() as i32,
        ));
        (loc, (w, h))
    }
}

// ── Вход в композитор ────────────────────────────────────────────────────────

/// С какого зума холст начинает въезжать. Больше единицы — то есть в первый
/// момент видно ЧУТЬ МЕНЬШЕ холста, чем потом, и кадр «отъезжает», открывая
/// рабочее место. Ровно то движение, в честь которого назван композитор:
/// картинка встаёт на место не рывком, а слоями, каждый со своей скоростью
/// (обои едут вместе с зумом, панель приезжает сверху позже всех).
///
/// 1.06, а не 1.2: на большом отъезде текст первые полсекунды нечитаемо
/// подрагивает, и вместо «появилось» получается «дёрнулось».
pub(crate) const ВХОД_ЗУМ: f64 = 1.06;

/// Доля времени входа, за которую пелена сходит на нет.
///
/// Меньше единицы намеренно: темнота обязана уйти РАНЬШЕ, чем кончится
/// движение, иначе последняя треть отъезда пройдёт под чёрным стеклом и её
/// просто не будет видно.
const ВХОД_ПЕЛЕНА: f64 = 0.62;

/// Появление рабочего места при запуске композитора.
///
/// Живёт одним полем `Parallax::вход` на весь компоновщик (а не по монитору):
/// экраны обязаны появиться ОДНОВРЕМЕННО и одинаково, иначе вместо одного
/// движения выходит два несогласованных.
pub struct Вход {
    начат: Instant,
    длительность: Duration,
}

impl Вход {
    pub fn новый() -> Self {
        Self { начат: Instant::now(), длительность: дуг::вход() }
    }

    fn t(&self) -> f64 {
        let прошло = self.начат.elapsed().as_secs_f64();
        (прошло / self.длительность.as_secs_f64().max(1e-6)).clamp(0.0, 1.0)
    }

    pub fn готов(&self) -> bool {
        self.начат.elapsed() >= self.длительность
    }

    /// Непрозрачность пелены поверх всего кадра: 1 — чернота, 0 — её нет.
    ///
    /// Кривая обратная спокойной: гаснет быстро в начале и мягко подходит к
    /// нулю, поэтому «проявление» видно, а последние проценты не мажут кадр
    /// еле заметной серой плёнкой.
    pub fn пелена(&self) -> f32 {
        let t = (self.t() / ВХОД_ПЕЛЕНА).clamp(0.0, 1.0);
        (1.0 - ease_calm(t)) as f32
    }

    /// Зум холста в этот момент.
    pub fn зум(&self) -> f64 {
        lerp(ВХОД_ЗУМ, 1.0, ease_calm(self.t()))
    }
}

// ── Столкновения на лету (режим коллизии, Super+S) ───────────────────────────

/// Доля скорости вдоль оси удара, которая уходит тому, кого ударили.
const IMPULSE_TRANSFER: f64 = 0.72;
/// Сколько скорости вдоль оси удара остаётся у ударившего (остальное гасится
/// «на удар»). Сумма с TRANSFER меньше 1 — столкновение неупругое, куча окон
/// не разгоняет сама себя.
const IMPULSE_KEEP: f64 = 0.22;
/// Минимальный и максимальный толчок (px/сек): совсем вялый контакт всё равно
/// должен заметно отодвинуть соседа, а очень резкий — не улететь за горизонт.
pub(crate) const IMPULSE_MIN: f64 = 240.0;
/// Потолок поднят вместе с дальностью броска: окно, влетевшее в кучу, обязано
/// эту кучу РАЗМЕТАТЬ, а не подвинуть. На 2200 px/сек сосед проезжал заметно
/// меньше самого броска, и удар выглядел вялым рядом с летящим окном.
pub(crate) const IMPULSE_MAX: f64 = 5200.0;
/// Перекрытие меньше этого (px) — шум округления, столкновения нет.
const COLLIDE_EPS: i32 = 2;
/// Скорость сближения (px/сек), ниже которой удар не считается: без этого
/// порога уже разъезжающиеся, но ещё перекрытые окна били бы друг друга каждый
/// кадр и дрожали на месте.
const APPROACH_EPS: f64 = 30.0;

/// Позиция и скорость окна ПРЯМО СЕЙЧАС (кадр полёта, если оно летит).
/// В отличие от `window_anim_target` здесь нужна именно живая геометрия:
/// столкновение происходит там, где окна находятся, а не там, где они
/// когда-нибудь остановятся.
fn live_motion(state: &Parallax, window: &Window) -> Option<(Point<f64, Logical>, Point<f64, Logical>)> {
    if let Some((_, anim)) = state.window_pos_anims.iter()
        .find(|(w, _)| crate::dwindle::same_window(w, window))
    {
        return Some((anim.pos, anim.vel));
    }
    let loc = state.space.element_geometry(window)?.loc;
    Some((loc.to_f64(), Point::from((0.0, 0.0))))
}

/// Окна, летящие по инерции, не проходят сквозь соседей: на контакте окно
/// отдаёт часть импульса тому, в кого врезалось (тот сам уезжает по инерции и
/// толкает дальше — цепочка получается сама собой, каждый толкнутый становится
/// летящим), гасит свою скорость вдоль оси удара и выходит из соседа, чтобы за
/// кадр полёта не «залететь» внутрь него.
///
/// Зовётся каждый кадр из tick() ПОСЛЕ интегрирования пружин.
fn resolve_fling_collisions(state: &mut Parallax) {
    // Коллизия — режим свободного холста: в тайлинге позиции держит раскладка,
    // а в обзоре столов окно не должно вылетать за рамку своего стола.
    if !state.is_snapping_enabled || state.overview_active
        || state.tile_config.layout != Layout::Float
    {
        return;
    }

    let fliers: Vec<Window> = state.window_pos_anims.iter()
        .filter(|(_, anim)| anim.fling && !anim.is_done())
        .map(|(w, _)| w.clone())
        .collect();
    if fliers.is_empty() {
        return;
    }

    let tags = state.viewport.current_tags();
    // Схлопнутая стопка (2.4) перекрывается сама с собой намеренно — её не
    // расталкиваем. Окно под курсором в драге тоже не трогаем: его позицию
    // каждый motion задаёт мышь, и анимация дралась бы с ней.
    let candidates: Vec<Window> = state.tagged_windows.iter()
        .filter(|tw| tw.tags & tags != 0 && !tw.folded)
        .map(|tw| tw.window.clone())
        .filter(|w| state.dragged_window.as_ref().is_none_or(|d| !crate::dwindle::same_window(d, w)))
        .collect();

    for flier in &fliers {
        let (fpos, fvel) = match state.window_pos_anims.iter()
            .find(|(w, _)| crate::dwindle::same_window(w, flier))
        {
            Some((_, a)) if a.fling && !a.is_done() => (a.pos, a.vel),
            _ => continue,
        };
        let fsize = match state.space.element_geometry(flier) { Some(g) => g.size, None => continue };
        let frect = Rectangle::new(fpos.to_i32_round(), fsize);

        for other in &candidates {
            if crate::dwindle::same_window(other, flier) { continue; }
            let osize = match state.space.element_geometry(other) { Some(g) => g.size, None => continue };
            let (opos, ovel) = match live_motion(state, other) { Some(m) => m, None => continue };
            let orect = Rectangle::new(opos.to_i32_round(), osize);
            let inter = match frect.intersection(orect) { Some(i) => i, None => continue };
            if inter.size.w <= COLLIDE_EPS || inter.size.h <= COLLIDE_EPS { continue; }

            // Нормаль удара — ось наименьшего перекрытия, направление «от
            // бьющего к битому» (minimum translation vector).
            let (nx, ny, depth) = if inter.size.w < inter.size.h {
                let dir = if opos.x + osize.w as f64 / 2.0 >= fpos.x + fsize.w as f64 / 2.0 { 1.0 } else { -1.0 };
                (dir, 0.0, inter.size.w as f64)
            } else {
                let dir = if opos.y + osize.h as f64 / 2.0 >= fpos.y + fsize.h as f64 / 2.0 { 1.0 } else { -1.0 };
                (0.0, dir, inter.size.h as f64)
            };

            // Уже разъезжаются (толчок применён кадр назад, битый едет быстрее
            // бьющего) — второй раз бить не за что.
            let approach = (fvel.x - ovel.x) * nx + (fvel.y - ovel.y) * ny;
            if approach <= APPROACH_EPS { continue; }

            // Толчок битому: он летит той же инерционной пружиной, значит сам
            // становится «бьющим» на следующем кадре — волна идёт дальше.
            // Пола у толчка здесь НЕТ (в отличие от драга, где энергию
            // подводит рука): иначе вялый контакт в 31 px/сек порождал бы
            // толчок в 240, куча окон разгоняла бы сама себя и дрожала бы без
            // остановки. Слабый удар — слабый толчок; из соседа бьющий всё
            // равно выталкивается позиционно, ниже.
            let transfer = (approach * IMPULSE_TRANSFER).min(IMPULSE_MAX);
            state.impulse_window(other, Point::from((nx * transfer, ny * transfer)), glide_omega(transfer));

            // Бьющий: гасим скорость вдоль оси удара и ВЫТАЛКИВАЕМ его из
            // соседа на глубину проникновения. Без этой поправки окно за один
            // кадр полёта (v·dt — десятки пикселей) въезжает внутрь соседа и
            // так и остаётся там висеть — это и есть «залетают друг в друга».
            let lost = approach * (1.0 - IMPULSE_KEEP);
            let new_vel = Point::from((fvel.x - nx * lost, fvel.y - ny * lost));
            let new_pos = Point::from((fpos.x - nx * depth, fpos.y - ny * depth));
            let speed = (new_vel.x * new_vel.x + new_vel.y * new_vel.y).sqrt();
            if let Some((_, anim)) = state.window_pos_anims.iter_mut()
                .find(|(w, _)| crate::dwindle::same_window(w, flier))
            {
                anim.pos = new_pos;
                anim.coast(new_vel, glide_omega(speed));
                let target = anim.target.to_i32_round();
                state.space.map_element(flier.clone(), new_pos.to_i32_round(), false);
                if let Some(tw) = state.tagged_windows.iter_mut()
                    .find(|tw| crate::dwindle::same_window(&tw.window, flier))
                {
                    tw.float_position = target;
                    tw.position = target;
                    tw.float_position_set = true;
                }
            }
            // Один удар на кадр: окно, зажатое между двумя соседями, иначе
            // получало бы две поправки позиции подряд и дёргалось.
            break;
        }
    }
}

/// Запасной шаг, если тик зовут первый раз (реального dt ещё нет).
const FRAME_DT: Duration = Duration::from_millis(16);
/// Потолок шага: после паузы (VT-switch, зависший кадр) инерция не должна
/// «телепортировать» камеру одним гигантским dt.
const MAX_DT: Duration = Duration::from_millis(100);

/// Как часто тикать, пока что-то движется, и как редко — пока всё стоит.
///
/// Раньше интервал был один: 16 мс всегда. Плюс главный цикл в main.rs ждал
/// события ровно столько же. На неподвижном экране это 120 пробуждений в
/// секунду ради работы, которой нет: ни анимаций, ни инерции, ни кадров. Для
/// настольной машины это незаметно, а для ноутбука — процессор, которому не
/// дают уйти в глубокий сон, и он не уходит туда НИКОГДА, даже когда экран
/// статичен часами.
///
/// Медленный интервал не добавляет задержек. Любое событие — ввод, коммит
/// клиента, VBlank, D-Bus — будит цикл немедленно, и тик считается сразу за
/// ним (см. main.rs). А непрерывную анимацию везёт цепочка VBlank, которая
/// тикает ровно в такт монитору. Таймер здесь — только страховка на случай,
/// если цепочка оборвалась.
pub const TICK_ACTIVE: Duration = Duration::from_millis(16);
pub const TICK_IDLE: Duration = Duration::from_millis(250);

/// Длительность раскрытия карты окон при темпе 1.0, секунд.
///
/// **26.08.2026: время вместо постоянной экспоненты.** Раньше доля ехала
/// экспонентой с τ = 0.13 (полный ход ≈ 3τ ≈ 0.39 с, но с бесконечным хвостом).
/// Теперь доля идёт РОВНО за это время, а форму движения задаёт кривая на
/// отрисовке (`canvas::minimap_reveal` — ease-out-cubic по размеру,
/// `canvas::minimap_fade` — проявление быстрее размера). 0.30 с: карта крупная,
/// быстрее читается как «мигнуло», медленнее — как «тормозит».
const MINIMAP_SLIDE_DUR: f64 = 0.30;
/// Ближе этого к цели доводку считаем законченной и дожимаем в точное
/// значение (см. `tick`): хвост экспоненты бесконечен, а `anim_busy` обязан
/// когда-то погаснуть.
const MINIMAP_SLIDE_EPS: f64 = 0.001;

/// Постоянная времени ухода панели вверх (см. `Parallax::bar_hide`). Заметно
/// длиннее выезда миникарты: панель — самая крупная плашка на экране, и то,
/// что для маленькой читается как «щёлк», для неё читается как рывок.
const BAR_HIDE_TAU_BASE: f64 = 0.16;
fn bar_hide_tau() -> f64 { BAR_HIDE_TAU_BASE * tempo() }

/// Постоянная времени, с которой миникарта ДОГОНЯЕТ камеру (см.
/// `Parallax::minimap_follow`). Заметно быстрее выезда панели: миникарта обязана
/// поспевать за глазом, просто без синхронного дёргания на каждый пиксель
/// инерции холста.
const MINIMAP_FOLLOW_TAU_BASE: f64 = 0.09;

/// Постоянная времени выезда полки состояния из-под панели и раскрытия
/// карточки предпросмотра. Обе — мелкие плашки прямо под курсором, и мерять их
/// надо по руке, а не по глазу: полка открывается по щелчку, карточка идёт за
/// наведением, и всё, что длиннее ~0.2 с, читается как «тормозит».
const SHELF_TAU_BASE: f64 = 0.07;
fn shelf_tau() -> f64 { SHELF_TAU_BASE * tempo() }

/// Длительность раскрытия карточки предпросмотра, секунд. Мельче карты и прямо
/// под курсором — значит заметно короче (0.17 с): карточка идёт ЗА наведением,
/// и всё, что длиннее ~0.2 с, читается как «тормозит».
const PREVIEW_DUR: f64 = 0.17;

/// Доводка собственного вида карточки предпросмотра (пан/зум колесом и драгом)
/// к цели: те же числа, что у карты, — карточка теперь такая же мини-копия
/// мира, и вести себя обязана так же.
const PREVIEW_VIEW_TAU_BASE: f64 = 0.07;
fn preview_view_tau() -> f64 { PREVIEW_VIEW_TAU_BASE * tempo() }

/// Есть ли у этой ячейки панели что показывать в карточке предпросмотра.
/// Один ответ на два места: цель анимации здесь и отрисовка в udev.rs.
pub fn предпросмотр_возможен(cell: crate::bar::Cell) -> bool {
    matches!(cell, crate::bar::Cell::Tag(_) | crate::bar::Cell::Window(_))
}
fn minimap_follow_tau() -> f64 { MINIMAP_FOLLOW_TAU_BASE * tempo() }
const MINIMAP_FOLLOW_EPS: f64 = 0.05;

/// Скорость доводки зума миникарты к цели (1/с) — тот же приём и то же число,
/// что у `zoom_glide_omega`: соседние щелчки колеса должны сливаться в одно
/// движение, а не дёргаться поштучно.
fn minimap_zoom_omega() -> f64 { ZOOM_GLIDE_OMEGA_BASE / tempo() }
const MINIMAP_ZOOM_EPS: f64 = 5e-4;

impl Parallax {
    /// Движется ли на экране хоть что-то, чему нужен тик 60 Гц.
    ///
    /// Список ровно по тому, что двигает [`tick`] ниже, плюс доводка курсора у
    /// края тачпада: она — единственное, что обязано идти при ПОЛНОМ отсутствии
    /// событий (пальцы прижаты и стоят, libinput молчит, см. input.rs), и
    /// проспать её нельзя.
    pub fn anim_busy(&self) -> bool {
        // Вход идёт первым: пока он не кончился, кадр обязан собираться каждый
        // такт — пелена гаснет по времени, а не по событиям, и на редком тике
        // (TICK_IDLE) появление рабочего места шло бы четырьмя кадрами.
        self.вход.is_some()
            || self.camera_anim.is_some()
            || self.zoom_anim.is_some()
            || self.zoom_glide.is_some()
            || self.momentum.coasting
            || !self.window_open_anims.is_empty()
            || !self.закрытия.is_empty()
            || !self.window_pos_anims.is_empty()
            || !self.слайд_уходят.is_empty()
            || self.focus_aura_anim.is_some()
            || self.edge_drift.is_some()
            || self.автодовод_идёт()
            || self.needs_redraw
            // Идёт раздача и кто-то подключён: тик обязан оставаться частым,
            // иначе гости получали бы кадры с частотой опроса «когда всё
            // стоит» (см. TICK_IDLE) — то есть слайд-шоу.
            || self.раздача_есть_впущенные()
            // Живые обои: тик обязан оставаться частым, пока plx-wall крутит
            // видео, — на редком (TICK_IDLE) сторож кадровых callback'ов
            // отвечал бы обоям четыре раза в секунду, и ролик шёл бы
            // слайд-шоу, пока экран неподвижен. Статичная картинка отметку
            // не обновляет и тик не разгоняет (см. `фон_живой`).
            || self.фон_живой()
            // Панель миникарты в движении: доля ещё не дошла до своей цели.
            || self.minimap_slide != if self.is_minimap_visible { 1.0 } else { 0.0 }
            // Панель уезжает вверх (обзор столов, полный экран) или приезжает.
            || self.bar_hide != if self.overview_active || self.fullscreen_here() { 1.0 } else { 0.0 }
            // Миникарта видна и её зум/слежение ещё не доехали до цели.
            // Цели — поля, а не пересчёт автоподгонки: `anim_busy` зовут на
            // каждой итерации главного цикла, а обход окон там ни к чему.
            || (self.minimap_slide > 0.0 && (
                self.minimap_zoom != self.minimap_zoom_target
                || self.minimap_follow != self.minimap_center_target
            ))
            || self.portal_cast.is_some()
            || self.tray.as_ref().is_some_and(|t| t.armed.is_some())
            // Полка выезжает из-под панели или уезжает обратно.
            || self.shelf_anim != if self.tray.as_ref().is_some_and(|t| t.open) { 1.0 } else { 0.0 }
            // Карточка предпросмотра раскрывается или сворачивается. Держит её
            // не только наведение на ЯЧЕЙКУ панели: курсор на самой карточке
            // (`preview_hover`) тоже — по ней панят и зумят, как по карте.
            || self.preview_anim != if self.bar_hover.is_some_and(предпросмотр_возможен)
                || self.preview_hover || self.preview_drag { 1.0 } else { 0.0 }
            // Собственный вид карточки (пан/зум) ещё доезжает до цели.
            || (self.preview_anim > 0.0 && {
                let (центр, зум) = self.preview_view_target();
                self.preview_zoom != зум || self.preview_center != центр
            })
    }

    /// Пауза до следующего тика: такт кадра, пока что-то движется, и заметно
    /// более редкий опрос, пока всё стоит.
    pub fn tick_interval(&self) -> Duration {
        if self.anim_busy() { TICK_ACTIVE } else { TICK_IDLE }
    }
}

/// Advances momentum coasting and any active camera/zoom LERP by one frame.
/// Called from the ~60Hz timer in main.rs. No-ops (cheaply) when nothing is
/// animating, so it's safe to run unconditionally forever.
pub fn tick(state: &mut Parallax) {
    let mut dirty = false;

    // Мультиюзер: рассылка списка участников, уборка отвалившихся и запрос
    // кадра, когда гостю пора его отдавать. Место именно здесь — этот тик
    // единственный, кто идёт на НЕПОДВИЖНОМ экране: без него гость, водящий
    // своей камерой по холсту, не получил бы ни одного кадра, пока хозяин не
    // пошевелит мышью.
    state.раздача_тик();

    // Автодовод курсора по краям накладки тачпада. Место то же и по той же
    // причине: палец, ЛЕЖАЩИЙ у края, не порождает событий вообще, и другого
    // повода пошевелить курсор во всём композиторе просто нет.
    state.автодовод_шаг();

    // Живые обои на НЕПОДВИЖНОМ экране. Тот же довод, что и строкой выше:
    // этот тик — единственный, кто идёт, когда на экране ничего не меняется,
    // а обоям нужен кадровый callback, иначе plx-wall засыпает навсегда (см.
    // Parallax::будить_фоновые_слои).
    state.будить_фоновые_слои();

    // Палитра обоев для свечения окон. Здесь же и по той же причине, что
    // соседи выше: смена обоев не порождает в композиторе никаких событий —
    // plx-wall просто рисует другую картинку в свой слой, — а свечение обязано
    // переехать в новый цвет вслед за ней. Стоит это одного `stat` на тик, и
    // только когда свечение включено.
    if state.lua_config.glow > 0.0 || state.lua_config.sun > 0.0 {
        if let Some(п) = crate::обои::Палитра::перечесть(&mut state.палитра_правка) {
            if state.палитра_обоев != Some(п) {
                state.палитра_обоев = Some(п);
                dirty = true;
            }
        }
    }

    // Взвод кнопки питания в полке протухает по времени, а не по событию, —
    // снять его больше некому: на статичном экране других тиков нет. Кадр
    // просим напрямую: камеры это не касается, а `dirty` тянет за собой
    // apply_camera.
    if state.tray_expire_armed() {
        state.request_redraw();
    }

    // Реальный dt, а не фиксированные 16мс: тик зовут и 60Гц-таймер из main.rs,
    // и VBlank-хендлер перед кадром (чтобы анимация сэмплировалась ровно в
    // момент отрисовки). С захардкоженным шагом инерция от этого поехала бы
    // вдвое быстрее — она единственная здесь считается по шагу, а не по Instant.
    let now = Instant::now();
    let dt = state.anim_last_tick
        .map(|prev| now.saturating_duration_since(prev).min(MAX_DT))
        .unwrap_or(FRAME_DT);
    state.anim_last_tick = Some(now);

    // Пальцы упёрлись в край тачпада, а жест не закончен — ведём курсор дальше
    // сами (см. EdgeDrift в input.rs). Место именно здесь: пока пальцы стоят,
    // событий ввода нет вовсе, и продолжить движение больше неоткуда — этот
    // тик единственный, кто вообще идёт на неподвижном экране.
    state.edge_drift_tick(dt);

    // Жест двумя пальцами, у которого не пришёл конец, тут и добивается
    // (см. `жест_сторож`): без этого следующий такой же свайп молча не делал
    // бы ничего. Место то же и по той же причине, что у сноса выше — на
    // неподвижном экране других тиков нет.
    state.жест_сторож();

    // Задержанная первая кнопка мышиного аккорда: вторая так и не пришла —
    // значит это удержание, а не аккорд, и клиент обязан наконец увидеть
    // нажатие (см. `аккорд_сторож`).
    state.аккорд_сторож();

    // Палец, лежащий неподвижно, дозревает до правой кнопки тоже здесь
    // (см. `сенсор_сторож`). Место то же и по той же причине: неподвижный
    // палец не вызывает ни одного события, и других тиков на неподвижном
    // экране нет.
    state.сенсор_сторож();

    // Куб рабочих столов доворачивается здесь же: это единственный тик,
    // который идёт и когда на экране больше ничего не меняется. Кончился
    // поворот — кончился и переход, и кадр возвращается к обычному холсту
    // (в обзоре куб живёт до выхода из обзора, поэтому там переход не при чём).
    if state.куб_активен() {
        let едет = state.куб_тик(dt.as_secs_f64());
        if !едет {
            if state.куб_выход.is_some() {
                // Наезд на грань доигран: вот теперь обзор гаснет (или
                // кончается проворот) — и кадр возвращается к обычному холсту
                // ровно с той картинкой, что стояла на передней грани.
                state.куб_закрытие_доиграло();
                dirty = true;
            } else if state.куб_переход {
                // Проворот довернулся, но куб ещё отодвинут: сам по себе он
                // исчезал в этом кадре, и переключение стола кончалось
                // прыжком «маленький куб → полный экран». Заводим обратный
                // наезд, и переход кончится только вместе с ним.
                state.куб_выход_начать(None, false);
            }
        }
    }


    // Инерция панорамирования применяется ТОЛЬКО когда нет активной анимации
    // камеры/зума. Иначе после конца camera_anim (например перелёта в (0,0) при
    // Win+D→tiling) остаточный momentum снова утаскивал камеру вбок — камера
    // "не доезжала"/уплывала. Пока идёт camera_anim/zoom_anim, momentum молчит.
    // Инерция ТОЛЬКО во Float: pan существует только там, а в tiling/columns
    // поздний momentum.launch() (напр. финальный кадр тачпад-жеста, пришедший
    // уже ПОСЛЕ Win+D) иначе "доезжал" и утаскивал разложенные окна за кадр
    // ("окна уезжают после Win+D").
    // Экранная точка курсора, которую обязана удержать инерция (см. ниже).
    let mut pin_after_camera = None;

    if state.tile_config.layout == Layout::Float
        && state.camera_anim.is_none() && state.zoom_anim.is_none()
    {
        if let Some(delta) = state.momentum.tick(dt) {
            // Инерция — продолжение пана, и курсор в ней обязан стоять на месте
            // экрана ровно так же, как в самом пане (Parallax::pan_camera_by).
            // Полагаться тут на отложенную sync_pointer_to_camera нельзя: она
            // ловит уже уехавшую стрелку и дёргает её назад — в логе
            // 20260729_204021 это `СИНХ КУРСОР ... снос=(17.6,30.0)` сразу
            // после отпускания ЛКМ. Точку берём ДО сдвига камеры, ставим —
            // ПОСЛЕ apply_camera (порядок обязателен, см. pin_pointer_after_camera).
            pin_after_camera = Some(state.pointer_screen_physical());
            state.viewport.cam_x += delta.x;
            state.viewport.cam_y += delta.y;
            dirty = true;
        }
    }

    if let Some(anim) = &state.camera_anim {
        let pos = anim.current();
        state.viewport.cam_x = pos.x;
        state.viewport.cam_y = pos.y;
        dirty = true;
        if anim.is_done() {
            state.camera_anim = None;
        }
    }

    if let Some(anim) = &state.zoom_anim {
        let (zoom, cam) = anim.current();
        state.viewport.zoom = zoom;
        state.viewport.cam_x = cam.x;
        state.viewport.cam_y = cam.y;
        dirty = true;
        if anim.is_done() {
            state.zoom_anim = None;
        }
    }

    // Доезд зума колесом. Читаем текущий масштаб заранее: внутри as_mut()
    // состояние уже занято изменяемой ссылкой на сам доезд.
    let zoom_now = state.viewport.zoom;
    let glide = state.zoom_glide.as_mut().map(|g| g.advance(now, zoom_now));
    if let Some((zoom, cam, done)) = glide {
        state.viewport.zoom = zoom;
        state.viewport.cam_x = cam.x;
        state.viewport.cam_y = cam.y;
        // Экранная точка курсора по построению не изменилась (камера считается
        // ОТ якоря под ним). Помечаем как намеренную, иначе
        // sync_pointer_to_camera увидит уехавшую камеру и будет слать клиенту
        // лишний motion каждый кадр доезда.
        state.pointer_warped();
        dirty = true;
        if done {
            state.zoom_glide = None;
        }
    }

    // Появление окна "с ростом" (Hyprland-style, только Float — см. new_toplevel).
    if !state.window_open_anims.is_empty() {
        let mut finished = Vec::new();
        for (i, (window, anim)) in state.window_open_anims.iter().enumerate() {
            let (loc, (w, h)) = anim.current();
            crate::xwin::set_size(window, Some((w, h).into()), crate::xwin::Tiled::Keep);
            crate::xwin::configure(window);
            state.space.map_element(window.clone(), loc, false);
            if let Some(tw) = state.tagged_windows.iter_mut().find(|tw| {
                &tw.window == window
            }) {
                tw.position = loc;
                tw.float_position = loc;
            }
            if anim.is_done() {
                finished.push(i);
            }
        }
        for i in finished.into_iter().rev() {
            state.window_open_anims.remove(i);
        }
        dirty = true;
    }

    // Гаснущие снимки закрытых окон (см. close.rs). Двигать тут нечего —
    // прозрачность и масштаб считает сам `Уход` по своим часам; здесь только
    // уборка догоревших и признак «кадр обновить надо».
    if !state.закрытия.is_empty() {
        state.закрытия.retain(|уход| !уход.is_done());
        dirty = true;
    }

    // Разлёт/сборка окон при переходах tiling/floating: плавный LERP позиции
    // вместо мгновенного map_element (см. tiling.rs::scatter_to_float/resize_window).
    if !state.window_pos_anims.is_empty() {
        // Общий `now` на весь кадр: иначе окна одной раскладки интегрируются
        // по чуть разным dt и «расползаются» на доли пикселя.
        for (window, anim) in state.window_pos_anims.iter_mut() {
            anim.advance(now);
            state.space.map_element(window.clone(), anim.pos.to_i32_round(), false);
        }
        // Столкновения считаем ПОСЛЕ шага пружин и по живым позициям: удар
        // происходит там, где окна оказались в этом кадре.
        resolve_fling_collisions(state);
        state.window_pos_anims.retain(|(_, anim)| !anim.is_done());
        // Уехавший стол снимаем с холста ровно тогда, когда он доехал: раньше
        // — он исчезнет на полпути, позже — останется висеть за краем и будет
        // попадать под курсор и в damage tracking.
        if !state.слайд_уходят.is_empty() && state.слайд_доехал() {
            state.слайд_прибрать();
            state.request_plane_reset();
        }
        // ВАЖНО: не сбрасывать needs_plane_reset здесь каждый тик — это
        // форсирует полный редрав экрана (reset_buffer_ages) на КАЖДЫЙ кадр
        // анимации вместо инкрементального damage tracking и жёстко лагает
        // (60 полных редравов/сек). Damage tracking сам корректно перерисует
        // только сдвинувшуюся область; plane reset нужен один раз при
        // структурных изменениях (см. arrange()/toggle_fold_stack), не за кадр.
        dirty = true;
    }

    // Focus Aura (5.3): следим за текущим фокусом, при смене — плавный LERP
    // от текущего положения ауры к прямоугольнику нового окна.
    {
        let target = state.focused_surface()
            .and_then(|fs| state.tagged_windows.iter().find(|tw| {
                crate::xwin::is_surface(&tw.window, &fs)
            }))
            .and_then(|tw| state.space.element_geometry(&tw.window))
            .map(|g| (
                Point::from((g.loc.x as f64, g.loc.y as f64)),
                (g.size.w as f64, g.size.h as f64),
            ));

        if let Some((pos, size)) = target {
            let changed = match state.focus_aura_target {
                Some((p, s)) => {
                    (p.x - pos.x).abs() > 0.5 || (p.y - pos.y).abs() > 0.5
                        || (s.0 - size.0).abs() > 0.5 || (s.1 - size.1).abs() > 0.5
                }
                None => true,
            };
            if changed {
                let (from_pos, from_size) = state.focus_aura_current.unwrap_or((pos, size));
                state.focus_aura_anim = Some(RectAnim::new(from_pos, from_size, pos, size, дуг::аура_фокуса()));
                state.focus_aura_target = Some((pos, size));
            }
        } else {
            // Фокус ушёл на невидимое окно (например, unmapped после переключения
            // тега — см. state.rs::refresh_tags) — без этого focus_aura_current
            // навсегда оставался на старой позиции и рисовался как "тень"
            // предыдущего выделения поверх нового тега (см. udev.rs::render_surface).
            if state.focus_aura_current.is_some() {
                dirty = true;
            }
            state.focus_aura_target = None;
            state.focus_aura_current = None;
            state.focus_aura_anim = None;
        }

        if let Some(anim) = &state.focus_aura_anim {
            let (p, s) = anim.current();
            state.focus_aura_current = Some((p, s));
            dirty = true;
            if anim.is_done() {
                state.focus_aura_anim = None;
            }
        }
    }

    // Вход в композитор: холст доезжает со своего начального зума до 1.0, пелена
    // сходит сама по времени (её читает кадр, см. udev::собрать_элементы).
    //
    // Зум ведём СДВИГОМ ОТ ТЕКУЩЕГО, а не пересчётом от дома монитора: за эту
    // секунду камеру успевают тронуть и подводка ленты, и прыжок к первому
    // окну, и восстановление сессии, — а пересчёт от дома молча отменял бы их
    // все. Здесь же центр экрана остаётся на месте, чей бы он ни был.
    if let Some(вход) = &state.вход {
        let новый = вход.зум();
        let готов = вход.готов();
        let пришпилить = |вид: &mut crate::state::Viewport, размер: (f64, f64)| {
            let старый = вид.zoom;
            if (старый - новый).abs() < 1e-9 {
                return;
            }
            // Ведём только тот вид, который во ВХОДЕ и есть, — начатый с
            // `ВХОД_ЗУМ` и потому больше единицы. Иначе движение подхватил бы
            // и монитор, поднявшийся посреди входа (его вид стоит на 1.0):
            // зум сперва скакнул бы ВВЕРХ, к текущей точке анимации, и только
            // потом поехал вниз. Сюда же попадает случай, когда активный вид
            // подменила заявка `monitor{ primary }`.
            if старый <= 1.0 {
                return;
            }
            вид.cam_x += размер.0 / 2.0 * (1.0 / старый - 1.0 / новый);
            вид.cam_y += размер.1 / 2.0 * (1.0 / старый - 1.0 / новый);
            вид.zoom = новый;
        };
        let активный = state.активный;
        let размер_активного = {
            let s = state.screen_size();
            (s.w as f64, s.h as f64)
        };
        пришпилить(&mut state.viewport, размер_активного);
        for (i, m) in state.мониторы.iter_mut().enumerate() {
            if i == активный {
                // Живая копия вида активного монитора лежит в state.viewport —
                // здесь она обновится сама при уходе фокуса (`сохранить_вид`).
                continue;
            }
            let размер = (m.размер.w as f64, m.размер.h as f64);
            пришпилить(&mut m.viewport, размер);
        }
        dirty = true;
        if готов {
            state.вход = None;
        }
    }

    // Уход панели вверх. Цель ставят обзор столов и полный экран — оба раньше
    // просто переставали её рисовать, и панель пропадала одним кадром. Та же
    // схема, что у миникарты, и намеренно ЧУТЬ МЕДЛЕННЕЕ: панель уезжает, пока
    // внизу собирается предпросмотр столов, и рывок здесь сбивал бы всё
    // движение целиком.
    {
        let цель = if state.overview_active || state.fullscreen_here() { 1.0 } else { 0.0 };
        let разница = цель - state.bar_hide;
        if разница.abs() > MINIMAP_SLIDE_EPS {
            let t = 1.0 - (-dt.as_secs_f64() / bar_hide_tau()).exp();
            state.bar_hide += разница * t.clamp(0.0, 1.0);
            dirty = true;
        } else if state.bar_hide != цель {
            state.bar_hide = цель;
            dirty = true;
        }
    }

    // Выезд полки состояния и раскрытие карточки предпросмотра — та же
    // экспонента, что у миникарты и панели, только короче: обе плашки мелкие и
    // висят прямо под курсором.
    //
    // У карточки к доле прилагается ЯЧЕЙКА (`preview_cell`): наведение ушло —
    // `bar_hover` уже None, а карточке ещё ехать, и рисовать в эти кадры было
    // бы нечего. Ячейка держится, пока доля не дойдёт до нуля.
    {
        let цель = if state.tray.as_ref().is_some_and(|t| t.open) { 1.0 } else { 0.0 };
        let разница = цель - state.shelf_anim;
        if разница.abs() > MINIMAP_SLIDE_EPS {
            let t = 1.0 - (-dt.as_secs_f64() / shelf_tau()).exp();
            state.shelf_anim += разница * t.clamp(0.0, 1.0);
            dirty = true;
        } else if state.shelf_anim != цель {
            state.shelf_anim = цель;
            dirty = true;
        }
    }
    {
        let наведено = state.bar_hover.filter(|c| предпросмотр_возможен(*c));
        if let Some(c) = наведено {
            // Перевели курсор на соседнюю ячейку — карточка не уезжает и не
            // приезжает заново, а просто меняет содержимое: так это и читается
            // глазом (одна карточка ходит вдоль панели, а не мигает).
            if state.preview_cell != Some(c) {
                // Другая ячейка — другой стол: собственный вид карточки к нему
                // отношения не имеет и обязан сброситься, иначе новый стол
                // открылся бы отпанённым куда-то в сторону.
                //
                // Ячейку ставим ПЕРВОЙ, и это не перестановка ради красоты:
                // `preview_reset_view` подтягивает показываемый центр к кадру
                // ТЕКУЩЕЙ ячейки, а до присваивания текущей была старая (или
                // никакой). Со старым порядком карточка на первом открытии
                // въезжала паном от начала холста к нужному месту.
                // Уходя со СТАРОЙ ячейки — запомнить её место, и только потом
                // менять ячейку: после присваивания запоминать было бы уже
                // некуда (вид уехал бы в память соседа).
                state.preview_запомнить_вид();
                state.preview_cell = Some(c);
                state.preview_reset_view();
            }
        }
        // Курсор УШЁЛ С ПАНЕЛИ НА САМУ КАРТОЧКУ — она обязана остаться: по ней
        // теперь панят и зумят, как по карте (26.08.2026, прямая просьба). Без
        // этого карточка гасла ровно в тот момент, когда к ней тянутся мышью.
        let держим = наведено.is_some() || state.preview_hover || state.preview_drag;
        let цель = if держим { 1.0 } else { 0.0 };
        if state.preview_anim != цель {
            state.preview_anim = шаг_доли(state.preview_anim, цель, dt, PREVIEW_DUR);
            dirty = true;
        }
        if state.preview_anim <= 0.0 && !держим {
            state.preview_запомнить_вид();
            state.preview_cell = None;
            state.preview_reset_view();
        }

        // Собственные пан/зум карточки доезжают до цели так же мягко, как у
        // карты: щелчок колеса не имеет права быть скачком.
        if state.preview_anim > 0.0 {
            let (центр, зум) = state.preview_view_target();
            let diff = зум.ln() - state.preview_zoom.ln();
            if diff.abs() > MINIMAP_ZOOM_EPS {
                let t = 1.0 - (-minimap_zoom_omega() * dt.as_secs_f64()).exp();
                state.preview_zoom = (state.preview_zoom.ln() + diff * t).exp();
                dirty = true;
            } else if state.preview_zoom != зум {
                state.preview_zoom = зум;
                dirty = true;
            }
            let cur = state.preview_center;
            let dist = ((центр.x - cur.x).powi(2) + (центр.y - cur.y).powi(2)).sqrt();
            if dist > MINIMAP_FOLLOW_EPS {
                let t = 1.0 - (-dt.as_secs_f64() / preview_view_tau()).exp();
                state.preview_center = Point::from((
                    cur.x + (центр.x - cur.x) * t,
                    cur.y + (центр.y - cur.y) * t,
                ));
                dirty = true;
            } else if state.preview_center != центр {
                state.preview_center = центр;
                dirty = true;
            }
        }
    }

    // Выезд панели миникарты. Тумблер (Super+`) задаёт только ЦЕЛЬ, а долю
    // ведём здесь: панель уезжает за правый край так же плавно, как приезжает,
    // — иначе выключение срезало бы её мгновенно.
    //
    // Доводка по экспоненте, привязанная к реальному dt, а не к номеру кадра:
    // тик зовут и таймер 60 Гц, и VBlank-хендлер, и шаг между ними разный.
    {
        let цель = if state.is_minimap_visible { 1.0 } else { 0.0 };
        if state.minimap_slide != цель {
            // Доля идёт ЛИНЕЙНО за фиксированное время, форму движения задаёт
            // кривая на отрисовке (см. `шаг_доли` и `canvas::minimap_reveal`).
            state.minimap_slide = шаг_доли(state.minimap_slide, цель, dt, MINIMAP_SLIDE_DUR);
            dirty = true;
        }
    }

    // Миникарта: собственный зум подтягивается к цели колеса (та же схема,
    // что у ZoomGlide холста), а точка слежения — к текущей камере. Без этого
    // щелчок колеса был бы мгновенным скачком, а пан холста — синхронным
    // дёрганьем миникарты (см. поля `minimap_zoom_target`/`minimap_follow`).
    if state.minimap_slide > 0.0 {
        // Цель: пока панель под РУЧНЫМ управлением (колесо/драг) — её
        // собственные зум/центр, иначе автоподгонка (все окна стола плюс
        // текущий экран). Считаем ЗДЕСЬ, а не в minimap_view: вид зовут и
        // отрисовка, и хит-тест, и цель обязана быть одна на кадр.
        let (центр, зум) = if state.minimap_manual {
            (state.minimap_manual_center, state.minimap_manual_zoom)
        } else {
            state.minimap_auto_target()
        };
        state.minimap_zoom_target = зум;
        state.minimap_center_target = центр;

        let diff = state.minimap_zoom_target.ln() - state.minimap_zoom.ln();
        if diff.abs() > MINIMAP_ZOOM_EPS {
            let t = 1.0 - (-minimap_zoom_omega() * dt.as_secs_f64()).exp();
            state.minimap_zoom = (state.minimap_zoom.ln() + diff * t).exp();
            dirty = true;
        } else if state.minimap_zoom != state.minimap_zoom_target {
            state.minimap_zoom = state.minimap_zoom_target;
            dirty = true;
        }

        let target = state.minimap_center_target;
        let cur = state.minimap_follow;
        let dist = ((target.x - cur.x).powi(2) + (target.y - cur.y).powi(2)).sqrt();
        if dist > MINIMAP_FOLLOW_EPS {
            let t = 1.0 - (-dt.as_secs_f64() / minimap_follow_tau()).exp();
            state.minimap_follow = Point::from((
                cur.x + (target.x - cur.x) * t,
                cur.y + (target.y - cur.y) * t,
            ));
            dirty = true;
        } else if state.minimap_follow != target {
            state.minimap_follow = target;
            dirty = true;
        }
    } else {
        // Панель убрана — доводку не гоняем впустую, а на следующем открытии
        // карта встаёт сразу на своё место: плавно приезжать ей неоткуда,
        // прошлый кадр всё равно устарел. Ручной вид (если он был) при этом
        // сохраняется как есть — панель открывается ровно там, где её оставили.
        let (центр, зум) = if state.minimap_manual {
            (state.minimap_manual_center, state.minimap_manual_zoom)
        } else {
            state.minimap_auto_target()
        };
        if state.minimap_follow != центр || state.minimap_zoom != зум {
            state.minimap_center_target = центр;
            state.minimap_zoom_target = зум;
            state.minimap_follow = центр;
            state.minimap_zoom = зум;
        }
    }

    // Часы в панели. Тик — единственное, что зовут и на неподвижном экране (см.
    // TICK_IDLE), поэтому минуту ловим здесь. Сравниваем СТРОКИ, а не время:
    // перерисовка нужна ровно тогда, когда на панели появится другая надпись,
    // а не каждый тик и не раз в секунду.
    {
        let clock = crate::bar::clock_text();
        if clock != state.bar_clock {
            state.bar_clock = clock;
            let date = crate::bar::date_text();
            if date != state.bar_date {
                state.bar_date = date;
            }
            state.request_redraw();
        }
    }

    if dirty {
        state.apply_camera();
        if let Some(screen) = pin_after_camera {
            state.pin_pointer_after_camera(screen);
        }
        state.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Темп — process-wide атомик, а тесты идут в потоках параллельно: тест,
    /// который его крутит, и тесты, которые от него зависят (доезд зума), обязаны
    /// не пересекаться. Иначе падение будет плавающим и на ровном месте.
    static ЗАМОК_ТЕМПА: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Спокойная кривая обязана быть именно спокойной: трогаться с места и
    /// вставать на месте, без рывка на старте — ровно тем она и отличается от
    /// ease-out-cubic, который здесь был раньше.
    #[test]
    fn спокойная_кривая_трогается_с_места() {
        assert!((ease_calm(0.0) - 0.0).abs() < 1e-12);
        assert!((ease_calm(1.0) - 1.0).abs() < 1e-12);
        assert!((ease_calm(0.5) - 0.5).abs() < 1e-12, "середина должна быть ровно посередине");

        // Скорость на концах — ноль, в середине — максимум.
        let ск = |t: f64| (ease_calm(t + 1e-6) - ease_calm(t - 1e-6)) / 2e-6;
        assert!(ск(0.001) < 0.01, "рывок на старте: {}", ск(0.001));
        assert!(ск(0.999) < 0.01, "рывок на финише: {}", ск(0.999));
        // Пик — 1.875 средней скорости (у ease-out-cubic он был 3.0 и в НУЛЕ).
        assert!((ск(0.5) - 1.875).abs() < 0.01, "пик скорости {}", ск(0.5));

        // Монотонность: камера не должна нигде отъезжать назад.
        let mut prev = -1.0;
        for i in 0..=100 {
            let v = ease_calm(i as f64 / 100.0);
            assert!(v >= prev - 1e-12, "кривая пошла назад на t={}", i);
            prev = v;
        }

        // А ease-out-cubic — наоборот, для того и оставлена: мгновенный старт.
        let скc = |t: f64| (ease_out_cubic(t + 1e-6) - ease_out_cubic(t - 1e-6)) / 2e-6;
        assert!(скc(0.001) > 2.9, "ease_out_cubic перестала стартовать резко");
    }

    /// Темп — одна ручка на все длительности сразу, и он их действительно
    /// тянет. Проверка заодно про зажим: нуль остановил бы анимации совсем.
    #[test]
    fn темп_тянет_все_длительности() {
        let _г = ЗАМОК_ТЕМПА.lock().unwrap_or_else(|e| e.into_inner());
        set_tempo(1.0);
        let норма = дуг::перелёт_к_столу();
        set_tempo(2.0);
        assert_eq!(дуг::перелёт_к_столу(), норма * 2);
        assert_eq!(дуг::сборка_тайлинга().as_millis(), 520);
        assert!(zoom_glide_omega() < ZOOM_GLIDE_OMEGA_BASE, "медленный темп — медленнее и зум");

        set_tempo(0.0);
        assert!(tempo() >= 0.2, "нулевой темп зажат: {}", tempo());
        set_tempo(f64::NAN);
        assert!((tempo() - 1.0).abs() < 1e-9, "NaN не должен ломать темп");
        set_tempo(1.0);
    }

    /// Бросок обязан быть УПРАВЛЯЕМЫМ: чем резче отпустил, тем дальше окно
    /// уехало — вплоть до полной дальности. Старая формула держала потолок в
    /// 420 px и упиралась в него уже на среднем движении руки, из-за чего
    /// «подтолкнуть» и «зашвырнуть» давали почти одно и то же.
    #[test]
    fn путь_броска_растёт_со_скоростью() {
        let d = FLING_DISTANCE;
        let путь = |v: f64| v / glide_omega_for(v, d);

        // Ниже FLING_FULL_SPEED путь пропорционален скорости.
        assert!((путь(700.0) - d / 10.0).abs() < 1.0, "путь на 700 px/с: {}", путь(700.0));
        assert!((путь(3500.0) - d / 2.0).abs() < 1.0, "путь на 3500 px/с: {}", путь(3500.0));

        // Резкий бросок улетает на всю дальность и не дальше неё.
        assert!((путь(FLING_FULL_SPEED) - d).abs() < 1.0);
        assert!(путь(40_000.0) <= d + 1.0, "потолок пробит: {}", путь(40_000.0));

        // Ручка масштабирует ВЕСЬ диапазон, а не только верх.
        let вдвое = |v: f64| v / glide_omega_for(v, d * 2.0);
        assert!((вдвое(700.0) - путь(700.0) * 2.0).abs() < 1.0);
        assert!((вдвое(FLING_FULL_SPEED) - d * 2.0).abs() < 1.0);

        // Нулевая дальность — инерции нет вовсе (fling_distance = 0 в конфиге).
        assert!(путь(0.0) < 1.0 && 5000.0 / glide_omega_for(5000.0, 0.0) < 6.0);
    }

    /// Бросок выделения: окна, отпущенные с одной скоростью и одной ω, обязаны
    /// сохранять взаимное расположение на ВСЁМ доезде, а не только в конечной
    /// точке. Иначе группа, которая весь драг ехала как целое, разваливается в
    /// момент отпускания — окно под курсором улетает по инерции одно
    /// (см. grabs/move_grab.rs, button()).
    #[test]
    fn бросок_сохраняет_строй_группы() {
        let v = Point::<f64, Logical>::from((900.0, -400.0));
        let speed = (v.x * v.x + v.y * v.y).sqrt();
        let omega = glide_omega(speed);

        let a_from = Point::<f64, Logical>::from((100.0, 100.0));
        let b_from = Point::<f64, Logical>::from((520.0, 260.0));
        let offset = (b_from.x - a_from.x, b_from.y - a_from.y);

        // Цель считается так же, как в Parallax::fling_window: from + v/ω.
        let target = |f: Point<f64, Logical>| {
            Point::<f64, Logical>::from((f.x + v.x / omega, f.y + v.y / omega))
        };
        let mut a = PosAnim::with_velocity(a_from, target(a_from), v, omega);
        let mut b = PosAnim::with_velocity(b_from, target(b_from), v, omega);

        let start = Instant::now();
        // Кадров больше, чем было (150): доезд стал длиннее вместе с
        // дальностью броска, и за 1.2 с пружина ещё не считает себя приехавшей.
        for step in 1..=400u64 {
            // Общий `now` на кадр — ровно так их продвигает цикл анимации.
            let now = start + Duration::from_millis(step * 8);
            a.advance(now);
            b.advance(now);
            let dx = (b.pos.x - a.pos.x) - offset.0;
            let dy = (b.pos.y - a.pos.y) - offset.1;
            // Порог — доли пикселя, а не ноль: каждая пружина запоминает свой
            // Instant в момент создания, и первый кадр они интегрируют по dt,
            // различающимся на микросекунды между двумя вызовами fling_window.
            // Это даёт расхождение ~5e-5 px — на порядки ниже целочисленной
            // сетки, в которой окна и раскладываются.
            assert!(
                dx.abs() < 0.01 && dy.abs() < 0.01,
                "строй распался на шаге {step}: расхождение ({dx}, {dy})"
            );
            // То, что реально видит глаз: округлённые позиции держат строй точно.
            let ai: Point<i32, Logical> = a.pos.to_i32_round();
            let bi: Point<i32, Logical> = b.pos.to_i32_round();
            assert_eq!(
                (bi.x - ai.x, bi.y - ai.y),
                (offset.0 as i32, offset.1 as i32),
                "на шаге {step} разъехались уже в пикселях"
            );
        }

        // Проверка не должна быть пустой: окна обязаны реально доехать.
        let path = ((a.pos.x - a_from.x).powi(2) + (a.pos.y - a_from.y).powi(2)).sqrt();
        assert!(path > 100.0, "окно почти не сдвинулось: путь {path}");
        let потолок = fling_distance();
        assert!(path <= потолок + 1.0, "доезд длиннее потолка: {path} > {потолок}");
        assert!(a.is_done() && b.is_done(), "пружины не остановились за 3.2 с");
    }

    /// Зум колесом должен быть непрерывным. Меряем самый резкий шаг за кадр
    /// по логарифму масштаба: старый код применял щелчок мгновенно, и один
    /// кадр из пяти менял зум на ln(1.1)=0.095 — это и есть дёрганость.
    /// Доезд обязан размазать ту же величину по кадрам.
    #[test]
    fn зум_колесом_идёт_без_рывков() {
        // Доезд зума считается от общего темпа — держим его на норме, пока
        // соседний тест его крутит (см. ЗАМОК_ТЕМПА).
        let _г = ЗАМОК_ТЕМПА.lock().unwrap_or_else(|e| e.into_inner());
        set_tempo(1.0);
        let anchor_canvas = Point::<f64, Logical>::from((800.0, 400.0));
        let anchor_screen = Point::<f64, Logical>::from((1280.0, 540.0));
        let шаг = 1.1_f64;
        let щелчков = 5;

        let mut zoom = 1.0_f64;
        let mut glide = ZoomGlide::new(zoom, anchor_canvas, anchor_screen);
        let start = Instant::now();

        let mut самый_резкий: f64 = 0.0;
        // Кадры по 8 мс; щелчок колеса — каждые 40 мс (быстрый прокрут).
        for кадр in 1..=150u64 {
            if кадр <= щелчков * 5 && кадр % 5 == 1 {
                let цель = (glide.target * шаг).clamp(0.05, 5.0);
                glide.retarget(цель, anchor_canvas, anchor_screen);
            }
            let now = start + Duration::from_millis(кадр * 8);
            let (новый, cam, _done) = glide.advance(now, zoom);
            самый_резкий = самый_резкий.max((новый.ln() - zoom.ln()).abs());
            zoom = новый;

            // Якорь обязан стоять на месте весь доезд, иначе зум «уползает»
            // из-под курсора.
            let screen_x = (anchor_canvas.x - cam.x) * zoom;
            let screen_y = (anchor_canvas.y - cam.y) * zoom;
            assert!(
                (screen_x - anchor_screen.x).abs() < 1e-6
                    && (screen_y - anchor_screen.y).abs() < 1e-6,
                "якорь уехал на кадре {кадр}: ({screen_x}, {screen_y})"
            );
        }

        let рывок_старого = шаг.ln(); // 0.0953 — весь щелчок за один кадр
        assert!(
            самый_резкий < рывок_старого / 3.0,
            "шаг за кадр {самый_резкий:.4} — не лучше мгновенного {рывок_старого:.4}"
        );
        // И при этом доезжаем ровно туда, куда накрутили колесом.
        let цель = шаг.powi(щелчков as i32);
        assert!(
            (zoom / цель - 1.0).abs() < 0.005,
            "не доехали до цели: {zoom:.4} против {цель:.4}"
        );
    }

    /// Та же постановка, но по-старому: инерцию получает только окно под
    /// курсором, а второе окно выделения остаётся стоять. Тест закрепляет
    /// саму поломку — он же доказывает, что проверка выше не пустая: если бы
    /// расхождение не возникало, чинить было бы нечего.
    #[test]
    fn без_броска_группы_окно_улетает_одно() {
        let v = Point::<f64, Logical>::from((900.0, -400.0));
        let speed = (v.x * v.x + v.y * v.y).sqrt();
        let omega = glide_omega(speed);

        let a_from = Point::<f64, Logical>::from((100.0, 100.0));
        let b_from = Point::<f64, Logical>::from((520.0, 260.0));
        let offset = (b_from.x - a_from.x, b_from.y - a_from.y);

        let mut a = PosAnim::with_velocity(
            a_from,
            Point::<f64, Logical>::from((a_from.x + v.x / omega, a_from.y + v.y / omega)),
            v,
            omega,
        );
        // b никуда не летит — ровно то, что делал старый код.
        let b_pos = b_from;

        let start = Instant::now();
        for step in 1..=150u64 {
            a.advance(start + Duration::from_millis(step * 8));
        }

        let dx = (b_pos.x - a.pos.x) - offset.0;
        let dy = (b_pos.y - a.pos.y) - offset.1;
        let разъезд = (dx * dx + dy * dy).sqrt();
        assert!(
            разъезд > 100.0,
            "старое поведение обязано разваливать строй, а разъезд всего {разъезд}"
        );
    }
}
