//! Синтетический ввод для харнесса: мышь и клавиатура «изнутри» dawn.
//!
//! **Зачем.** Проверить наведение, клик, драг и колесо снаружи было нечем.
//! Виртуальное устройство через `/dev/uinput` работает, но бьёт по ВСЕЙ
//! машине: события идут всем читателям evdev, то есть и живому сеансу Ярика
//! на tty7 (24.08.2026 тестовый экземпляр так поймал чужой Ctrl+Alt+F1 и
//! попытался увести VT). Здесь события рождаются прямо в процессе харнесса и
//! дальше ничьими не становятся.
//!
//! **Почему не звать `pointer.motion` напрямую.** Половина поведения dawn
//! живёт ДО вызова smithay: пан у края экрана, зажим в видимую область, драг
//! миникарты, наведение на чипы панели, sloppy focus. Синтетика поэтому
//! входит через ту же дверь, что и настоящая мышь, — `process_input_event`,
//! отличаясь только источником события.
//!
//! Ускорения libinput здесь нет и не должно быть: дельта доезжает до
//! `pointer_location` как есть (делится только на зум), поэтому «поставить
//! курсор в точку X,Y» — это ровно одно событие с разностью координат, без
//! замкнутого цикла с промахами и пересчётом множителя.

use std::path::PathBuf;

use smithay::backend::input::{
    Axis, AxisRelativeDirection, AxisSource, ButtonState, Device, DeviceCapability, Event,
    InputBackend, KeyState, KeyboardKeyEvent, Keycode, PointerAxisEvent, PointerButtonEvent,
    PointerMotionEvent, UnusedEvent,
};

/// Метка бэкенда: сами события ниже, тип нужен только системе типов smithay.
#[derive(Debug)]
pub struct Синтетика;

#[derive(PartialEq, Eq, Hash, Debug)]
pub struct Устройство;

impl Device for Устройство {
    fn id(&self) -> String {
        "dawn-synth".into()
    }
    fn name(&self) -> String {
        "dawn synthetic input".into()
    }
    fn has_capability(&self, c: DeviceCapability) -> bool {
        matches!(c, DeviceCapability::Keyboard | DeviceCapability::Pointer)
    }
    fn usb_id(&self) -> Option<(u32, u32)> {
        None
    }
    fn syspath(&self) -> Option<PathBuf> {
        None
    }
}

/// Время события. Настоящий ввод его получает от libinput; здесь — часы
/// монотонного времени процесса. Ноль ставить нельзя: по времени считаются
/// двойной клик, инерция и повтор клавиш.
fn сейчас_мкс() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[derive(Debug)]
pub struct Движение {
    pub dx: f64,
    pub dy: f64,
    pub время: u64,
}

impl Движение {
    pub fn new(dx: f64, dy: f64) -> Self {
        Self { dx, dy, время: сейчас_мкс() }
    }
}

impl Event<Синтетика> for Движение {
    fn time(&self) -> u64 {
        self.время
    }
    fn device(&self) -> Устройство {
        Устройство
    }
}

impl PointerMotionEvent<Синтетика> for Движение {
    fn delta_x(&self) -> f64 {
        self.dx
    }
    fn delta_y(&self) -> f64 {
        self.dy
    }
    // Сырая дельта равна ускоренной: ускорения у синтетики нет вовсе, а игры
    // считают по ней свой обзор — пусть видят то же самое движение.
    fn delta_x_unaccel(&self) -> f64 {
        self.dx
    }
    fn delta_y_unaccel(&self) -> f64 {
        self.dy
    }
}

#[derive(Debug)]
pub struct Кнопка {
    pub код: u32,
    pub нажата: bool,
    pub время: u64,
}

impl Кнопка {
    pub fn new(код: u32, нажата: bool) -> Self {
        Self { код, нажата, время: сейчас_мкс() }
    }
}

impl Event<Синтетика> for Кнопка {
    fn time(&self) -> u64 {
        self.время
    }
    fn device(&self) -> Устройство {
        Устройство
    }
}

impl PointerButtonEvent<Синтетика> for Кнопка {
    fn button_code(&self) -> u32 {
        self.код
    }
    fn state(&self) -> ButtonState {
        if self.нажата { ButtonState::Pressed } else { ButtonState::Released }
    }
}

/// Колесо мыши. Источник — именно `Wheel`, а не `Finger`: в dawn от источника
/// зависит ветка (палец = тачпад, там Super+2 пальца двигают окно, а колесо
/// над миникартой крутит её зум).
#[derive(Debug)]
pub struct Колесо {
    /// Вертикаль в единицах v120 (120 = один зубец).
    pub v120: f64,
    /// Горизонталь там же. Своя мышь у Ярика её не даёт, а вот источники
    /// синтетики дают: тачпад в игре и наклон колеса у гостя.
    pub v120_гор: f64,
    pub время: u64,
}

impl Колесо {
    /// `щелчки` — сколько зубцов колеса, знак = направление (вниз > 0).
    pub fn new(щелчки: f64) -> Self {
        Self::по_осям(0.0, щелчки)
    }

    /// Обе оси сразу: горизонталь ходит с тачпада и с наклона колеса, и
    /// терять её нельзя — в браузере это прокрутка вбок, в редакторе строка
    /// уезжает за край экрана.
    pub fn по_осям(гор: f64, верт: f64) -> Self {
        Self { v120: верт * 120.0, v120_гор: гор * 120.0, время: сейчас_мкс() }
    }
}

impl Event<Синтетика> for Колесо {
    fn time(&self) -> u64 {
        self.время
    }
    fn device(&self) -> Устройство {
        Устройство
    }
}

impl PointerAxisEvent<Синтетика> for Колесо {
    // 15 логических пикселей на зубец — то же соотношение, которым dawn сам
    // добирает величину, когда её нет (см. input.rs, ветка PointerAxis).
    fn amount(&self, axis: Axis) -> Option<f64> {
        match axis {
            Axis::Vertical => Some(self.v120 * 15.0 / 120.0),
            Axis::Horizontal => Some(self.v120_гор * 15.0 / 120.0),
        }
    }
    fn amount_v120(&self, axis: Axis) -> Option<f64> {
        match axis {
            Axis::Vertical => Some(self.v120),
            Axis::Horizontal => Some(self.v120_гор),
        }
    }
    fn source(&self) -> AxisSource {
        AxisSource::Wheel
    }
    fn relative_direction(&self, _axis: Axis) -> AxisRelativeDirection {
        AxisRelativeDirection::Identical
    }
}

#[derive(Debug)]
pub struct Клавиша {
    /// Скан-код evdev (KEY_A = 30), как его отдаёт настоящая клавиатура.
    pub код: u32,
    pub нажата: bool,
    pub время: u64,
}

impl Клавиша {
    pub fn new(код: u32, нажата: bool) -> Self {
        Self { код, нажата, время: сейчас_мкс() }
    }
}

impl Event<Синтетика> for Клавиша {
    fn time(&self) -> u64 {
        self.время
    }
    fn device(&self) -> Устройство {
        Устройство
    }
}

impl KeyboardKeyEvent<Синтетика> for Клавиша {
    // +8: xkb нумерует клавиши на восемь больше, чем evdev, — то же смещение
    // стоит во всех бэкендах smithay.
    fn key_code(&self) -> Keycode {
        (self.код + 8).into()
    }
    fn state(&self) -> KeyState {
        if self.нажата { KeyState::Pressed } else { KeyState::Released }
    }
    fn count(&self) -> u32 {
        1
    }
}

impl InputBackend for Синтетика {
    type Device = Устройство;
    type KeyboardKeyEvent = Клавиша;
    type PointerAxisEvent = Колесо;
    type PointerButtonEvent = Кнопка;
    type PointerMotionEvent = Движение;
    type PointerMotionAbsoluteEvent = UnusedEvent;

    type GestureSwipeBeginEvent = UnusedEvent;
    type GestureSwipeUpdateEvent = UnusedEvent;
    type GestureSwipeEndEvent = UnusedEvent;
    type GesturePinchBeginEvent = UnusedEvent;
    type GesturePinchUpdateEvent = UnusedEvent;
    type GesturePinchEndEvent = UnusedEvent;
    type GestureHoldBeginEvent = UnusedEvent;
    type GestureHoldEndEvent = UnusedEvent;

    type TouchDownEvent = UnusedEvent;
    type TouchUpEvent = UnusedEvent;
    type TouchMotionEvent = UnusedEvent;
    type TouchCancelEvent = UnusedEvent;
    type TouchFrameEvent = UnusedEvent;

    type TabletToolAxisEvent = UnusedEvent;
    type TabletToolProximityEvent = UnusedEvent;
    type TabletToolTipEvent = UnusedEvent;
    type TabletToolButtonEvent = UnusedEvent;

    type SwitchToggleEvent = UnusedEvent;
    type SpecialEvent = UnusedEvent;
}

/// Коды кнопок мыши в нумерации evdev — те же числа, что приходят с железа.
pub const КНОПКА_ЛКМ: u32 = 0x110;
pub const КНОПКА_ПКМ: u32 = 0x111;
pub const КНОПКА_СКМ: u32 = 0x112;

pub fn кнопка_по_имени(имя: &str) -> Option<u32> {
    match имя {
        "left" | "лкм" | "1" => Some(КНОПКА_ЛКМ),
        "right" | "пкм" | "3" => Some(КНОПКА_ПКМ),
        "middle" | "скм" | "2" => Some(КНОПКА_СКМ),
        _ => None,
    }
}
