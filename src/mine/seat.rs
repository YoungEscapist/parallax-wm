//! Своё место (seat) для мода: свой курсор, свой фокус, своя клавиатура.
//!
//! **Почему не общий ввод хозяина, как было.** Игра — такое же окно parallax, и
//! пока панелями правил хозяйский `wl_seat`, три вещи ломались разом:
//!
//! * курсор хозяина утаскивался на панель, а Minecraft в этот момент держит
//!   его захваченным (`GLFW_CURSOR_DISABLED` → pointer lock), поэтому
//!   `переместить_курсор_в` честно отказывался двигаться — и клики уходили
//!   туда, где стрелка стояла, а не туда, куда смотрит игрок;
//! * фокус клавиатуры — свойство МЕСТА: отдав его панели, мы отбирали
//!   клавиатуру у игры, и `F7` (отдать управление назад) до неё уже не
//!   долетал — «управление не возвращается в Minecraft»;
//! * хит-тест `surface_under` находил окно игры: оно развёрнуто на весь экран
//!   и лежит поверх панелей, так что всякий синтетический клик попадал в неё.
//!
//! Отдельное место снимает все три: игра остаётся за хозяйским местом целиком
//! (мышь крутит голову, `F7` работает, захват курсора не нарушен), а панели
//! получают ввод от места `plx-mine`. Тот же приём, что у гостей `plx-share`
//! (`share/seat.rs`) — и по той же причине.
//!
//! **Хит-теста здесь нет вовсе.** Куда попал взгляд, уже решила сцена
//! (`vr::scene::навести`): цель известна ОКНОМ, а не точкой на холсте. Это не
//! срезание угла, а единственный верный путь — окно игры перекрывает панели, и
//! любой поиск «кто под точкой» вернул бы её.

use smithay::backend::input::ButtonState;
use smithay::desktop::{Window, WindowSurfaceType};
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::Parallax;

/// Завести место мода. `None` — если не завелась клавиатура: без неё место
/// бессмысленно (в терминал на панели не напечатать), а молчаливая половина
/// хуже честного отказа.
pub fn завести(state: &mut Parallax) -> Option<Seat<Parallax>> {
    let dh = state.display_handle.clone();
    let mut место = state.seat_state.new_wl_seat(&dh, "plx-mine".to_string());
    // Раскладка та же, что у хозяина: клавиши приходят скан-кодами evdev, а
    // раскладку к ним применяет parallax.
    let раскладка = state.lua_config.xkb_config();
    if место.add_keyboard(раскладка, 200, 25).is_err() {
        tracing::warn!("plx/mine: seat without a keyboard — refused");
        return None;
    }
    место.add_pointer();
    tracing::info!("plx/mine: the mod has its own seat");
    Some(место)
}

/// Убрать место: снять фокус и отпустить глобал `wl_seat`.
pub fn убрать(state: &mut Parallax, место: Seat<Parallax>) {
    if let Some(клавиатура) = место.get_keyboard() {
        let serial = SERIAL_COUNTER.next_serial();
        клавиатура.set_focus(state, None, serial);
    }
    drop(место);
}

/// Место мода (клон дешёвый — внутри Rc).
fn место(state: &Parallax) -> Option<Seat<Parallax>> {
    state.mine.as_ref()?.место.clone()
}

/// Поверхность окна под точкой ЕГО СОДЕРЖИМОГО и её начало на холсте.
///
/// Точка приходит в пикселях панели, а панель — это содержимое окна: ровно то,
/// что нарисовано в полотно с вычитанием `geometry().loc` (см.
/// `vr::нарисовать_в_полотно`). Поэтому обратный перевод обязан это смещение
/// ПРИБАВИТЬ — у клиентов с клиентскими рамками (GTK, Firefox) вокруг окна
/// невидимые поля под тень, и без прибавки клик уезжал бы на их ширину.
fn поверхность(
    state: &Parallax,
    окно: &Window,
    пкс_x: f64,
    пкс_y: f64,
) -> Option<(WlSurface, Point<f64, Logical>, Point<f64, Logical>)> {
    let место_окна = state.space.element_geometry(окно)?.loc;
    let своё = окно.geometry().loc;
    // Точка в системе координат самого окна (как её ждёт `Window::surface_under`).
    let внутри = Point::<f64, Logical>::from((пкс_x + своё.x as f64, пкс_y + своё.y as f64));
    // Та же точка на холсте — её увидит клиент в `wl_pointer.motion`.
    let на_холсте = Point::<f64, Logical>::from((
        место_окна.x as f64 + пкс_x,
        место_окна.y as f64 + пкс_y,
    ));
    // Начало отсчёта поверхности на холсте: `render_location` плюс смещение
    // самой поверхности внутри окна (у попапов и сабсюрфейсов оно ненулевое).
    let render = место_окна - своё;
    let (поверхность, смещение) = окно.surface_under(внутри, WindowSurfaceType::ALL)?;
    Some((поверхность, на_холсте, (смещение + render).to_f64()))
}

/// Указка переехала на панель: рассказать клиенту и отдать ему клавиатуру.
///
/// Фокус едет ЗА ВЗГЛЯДОМ, а не за кликом (в отличие от гостя `plx-share`): в
/// шлеме это ровно то же правило, и оно единственное, при котором можно
/// смотреть в терминал и печатать, ничего не нажимая.
///
/// `брать_фокус` — «взгляд ТОЛЬКО ЧТО зашёл на эту панель» (решает
/// [`crate::mine::взять_фокус`]). Указка едет каждый кадр, а клавиатура — нет:
/// иначе фокус был бы не «за взглядом», а ПРИБИТ к нему, и всякий бинд,
/// метящий в фокус, откатывался бы следующим же кадром.
pub fn навести(state: &mut Parallax, окно: &Window, пкс_x: f64, пкс_y: f64, брать_фокус: bool) {
    let Some(место) = место(state) else { return };
    let Some((поверхность, точка, начало)) = поверхность(state, окно, пкс_x, пкс_y) else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let время = state.start_time.elapsed().as_millis() as u32;

    if брать_фокус {
        let прежний = место.get_keyboard().and_then(|к| к.current_focus());
        let цель = crate::focus::KeyboardFocusTarget::for_window(окно);
        if let (Some(клавиатура), Some(цель)) = (место.get_keyboard(), цель) {
            if прежний.as_ref() != Some(&цель) {
                клавиатура.set_focus(state, Some(цель), serial);
            }
        }
    }

    if let Some(указатель) = место.get_pointer() {
        указатель.motion(
            state,
            Some((поверхность, начало)),
            &MotionEvent { location: точка, serial, time: время },
        );
        указатель.frame(state);
    }
}

/// Взгляд ушёл с панелей: увести указку с клиента, фокус оставить.
///
/// Фокус НЕ снимаем нарочно: игрок то и дело отводит взгляд в мир, и терять на
/// этом набранную строку в терминале было бы наказанием за поворот головы.
pub fn увести(state: &mut Parallax) {
    let Some(место) = место(state) else { return };
    let Some(указатель) = место.get_pointer() else { return };
    let serial = SERIAL_COUNTER.next_serial();
    let время = state.start_time.elapsed().as_millis() as u32;
    указатель.motion(
        state,
        None,
        &MotionEvent { location: (0.0, 0.0).into(), serial, time: время },
    );
    указатель.frame(state);
}

/// Кнопка мыши от мода. Панель под взглядом ещё и поднимается наверх — как по
/// клику хозяина.
pub fn кнопка(state: &mut Parallax, окно: Option<Window>, код: u32, нажата: bool) {
    let Some(место) = место(state) else { return };
    let serial = SERIAL_COUNTER.next_serial();
    let время = state.start_time.elapsed().as_millis() as u32;
    if нажата {
        if let Some(окно) = окно {
            state.space.raise_element(&окно, false);
            // …и тут же вернуть игру наверх. Панель поднимается ради своих же
            // попапов и порядка среди панелей, но на МОНИТОРЕ она при этом
            // выныривала поверх развёрнутого Minecraft — щелчок по панели в
            // мире выкидывал человека на рабочий стол (жалоба 01.09.2026
            // «если нажимать на них ЛКМ»).
            crate::mine::игру_наверх(state);
        }
    }
    let Some(указатель) = место.get_pointer() else { return };
    указатель.button(
        state,
        &ButtonEvent {
            button: код,
            state: if нажата { ButtonState::Pressed } else { ButtonState::Released },
            serial,
            time: время,
        },
    );
    указатель.frame(state);
}

/// Колесо. Те же 15 логических пикселей на зубец, что и у своей мыши.
pub fn колесо(state: &mut Parallax, dx: f64, dy: f64) {
    use smithay::input::pointer::AxisFrame;
    let Some(место) = место(state) else { return };
    let Some(указатель) = место.get_pointer() else { return };
    let время = state.start_time.elapsed().as_millis() as u32;
    let кадр = AxisFrame::new(время)
        .source(smithay::backend::input::AxisSource::Wheel)
        .value(smithay::backend::input::Axis::Horizontal, dx * 15.0)
        .v120(smithay::backend::input::Axis::Horizontal, (dx * 120.0) as i32)
        .value(smithay::backend::input::Axis::Vertical, dy * 15.0)
        .v120(smithay::backend::input::Axis::Vertical, (dy * 120.0) as i32);
    указатель.axis(state, кадр);
    указатель.frame(state);
}

/// Клавиша от мода скан-кодом evdev.
///
/// Разбор — ОБЩИЙ с хозяйской клавиатурой (`input::разобрать_клавишу`), и это
/// главное здесь. Своя урезанная копия («только `find_action`»), стоявшая тут
/// раньше, молча теряла всё, чего в таблице биндов нет: обзор столов тапом по
/// Super, лупу Super+Space и пан её стрелками, конец перебора Alt+Tab на
/// отпускании Alt, клавиши меню и поиска окон, аварийный Super+Shift+Escape.
/// Из игры они не работали — при том, что в самом parallax работают.
///
/// Модификаторы при этом считаются по СВОЕМУ месту: «игрок держит Super» и
/// «хозяин держит Super» — разные факты, и клавиатура у мода своя.
pub fn клавиша(state: &mut Parallax, код: u32, нажата: bool) {
    let Some(место) = место(state) else { return };
    let Some(клавиатура) = место.get_keyboard() else { return };
    let serial = SERIAL_COUNTER.next_serial();
    let время = state.start_time.elapsed().as_millis() as u32;
    let состояние = if нажата {
        smithay::backend::input::KeyState::Pressed
    } else {
        smithay::backend::input::KeyState::Released
    };
    клавиатура.input::<(), _>(
        state,
        (код + 8).into(),
        состояние,
        serial,
        время,
        |state, modifiers, handle| {
            crate::input::разобрать_клавишу(state, modifiers, handle, нажата, true)
        },
    );
    // Кадр после клавиши — по той же причине, что и у хозяйской клавиатуры:
    // переключение стола или обзор иначе доедут до экрана (и до панелей в
    // игре) только со следующим чужим поводом нарисовать.
    state.request_redraw();
}
