//! Своё место (seat) на каждого гостя: свой курсор, свой фокус, своя клавиатура.
//!
//! **Почему именно seat, а не «подмешать события в общий ввод».** Wayland
//! устроен так, что фокус — свойство МЕСТА, а не композитора: `wl_seat`
//! отдаёт клиенту `wl_pointer` и `wl_keyboard`, и клиент знает, какое место
//! куда смотрит. Пять человек за одним `wl_seat` — это пять человек с одним
//! курсором и одним фокусом: пока гость печатает в терминал, хозяин не может
//! печатать в браузер, а любое движение чужой мыши уводит его собственную
//! стрелку. Отдельные места решают это на уровне протокола, и клиенты
//! (GTK, Qt, Xwayland через свой seat) умеют их разбирать — потому что это
//! ровно тот механизм, которым пользуются мультисит-стойки.
//!
//! **Не всякий клиент слушает ВТОРОЕ место, и это его дело, не наше.**
//! Замер 26.08.2026 в харнессе: гость кликает в окно и печатает «echo» —
//! `alacritty` показывает набранное, `ghostty` не реагирует ничем, хотя
//! композитор в обоих случаях делает ровно одно и то же (в логе
//! `поверхность=true окно=true цель=true` на клике и `фокус=true` на каждой
//! клавише, то есть `wl_keyboard.enter` и `key` клиенту ушли). Ghostty (GTK4)
//! берёт клавиатуру только у того `wl_seat`, который увидел первым. Чинить это
//! в parallax нельзя: единственный способ — посадить всех на одно место, то есть
//! отобрать у хозяина машины его собственный фокус. Признак у пользователя:
//! «гость двигает окна мышью, но не может печатать ровно в этой программе».
//!
//! **Что здесь НЕ делается.** Захваты композитора (Super+перетаскивание окна,
//! тяга за край, выделение рамкой) пока остаются за хозяином машины: они
//! живут в `input.rs` и завязаны на единственную камеру и единственный
//! `pointer_location`. Гость двигает окна так же, как в любом другом
//! композиторе — за клиентскую рамку CSD. Это осознанный рубеж первой версии:
//! лучше честный и предсказуемый ввод в приложения, чем половина оконных
//! жестов с чужой камерой.

use smithay::backend::input::ButtonState;
use smithay::input::keyboard::FilterResult;
use smithay::input::pointer::{ButtonEvent, MotionEvent};
use smithay::input::Seat;
use smithay::utils::{Logical, Point, SERIAL_COUNTER};

use crate::Parallax;

/// Завести место для гостя. `None` — если клавиатура не завелась (без неё
/// место бессмысленно: печатать гость не сможет, а молчаливая половина
/// функциональности хуже честного отказа).
pub fn завести(state: &mut Parallax, id: u8, имя: &str) -> Option<Seat<Parallax>> {
    let dh = state.display_handle.clone();
    let mut место = state.seat_state.new_wl_seat(&dh, format!("plx-share-{id}-{имя}"));
    // Раскладка гостя — та же, что у хозяина машины: клавиши приходят
    // скан-кодами evdev, и раскладку к ним применяет ХОСТ. Своя раскладка на
    // гостя — отдельная задача (нужно передавать её по протоколу).
    let раскладка = state.lua_config.xkb_config();
    if место.add_keyboard(раскладка, 200, 25).is_err() {
        tracing::warn!("plx/share: keyboard failed to start for guest {id}");
        return None;
    }
    место.add_pointer();
    tracing::info!("plx/share: guest {id} has its own seat");
    Some(место)
}

/// Курсор гостя поехал: рассказать клиенту под курсором.
///
/// Позиция уже в координатах ХОЛСТА (перевод из экрана гостя сделан в
/// `Parallax::раздача_курсор` — единственном месте, где известна камера гостя).
pub fn движение(state: &mut Parallax, id: u8, точка: Point<f64, Logical>) {
    let Some(место) = место_гостя(state, id) else { return };
    let Some(указатель) = место.get_pointer() else { return };
    let под = state.surface_under(точка);
    let serial = SERIAL_COUNTER.next_serial();
    let время = state.start_time.elapsed().as_millis() as u32;
    указатель.motion(
        state,
        под,
        &MotionEvent { location: точка, serial, time: время },
    );
    указатель.frame(state);
}

/// Кнопка мыши гостя. Нажатие ещё и переводит клавиатурный фокус ЭТОГО места
/// — иначе гость кликает в окно, а печатает в пустоту.
pub fn кнопка(state: &mut Parallax, id: u8, код: u32, нажата: bool) {
    let Some(место) = место_гостя(state, id) else { return };
    let точка = точка_гостя(state, id);
    let serial = SERIAL_COUNTER.next_serial();
    let время = state.start_time.elapsed().as_millis() as u32;

    if нажата {
        let под = state.surface_under(точка).map(|(s, _)| s);
        // Окно под чужим курсором ищем ДО фокуса: цель клавиатуры считается по
        // ОКНУ (`KeyboardFocusTarget::for_window`), а не по голой поверхности —
        // у X11-окна цель это `X11Surface`, и `From<WlSurface>` дала бы ветку
        // Wayland, то есть ввод ушёл бы мимо. Заодно снимается двойное
        // заимствование `state`: поиск по `space` держит его на чтение, а
        // `raise_element` просит на запись.
        let окно = под.as_ref().and_then(|поверхность| {
            state.space.elements()
                .find(|w| crate::xwin::is_surface(w, поверхность))
                .cloned()
        });
        if let Some(клавиатура) = место.get_keyboard() {
            let цель = окно.as_ref()
                .and_then(crate::focus::KeyboardFocusTarget::for_window)
                .or_else(|| под.clone().map(Into::into));
            tracing::debug!(
                "plx/share: guest {id} clicks at ({:.0},{:.0}): surface={} window={} target={}",
                точка.x, точка.y, под.is_some(), окно.is_some(), цель.is_some(),
            );
            клавиатура.set_focus(state, цель, serial);
        }
        // Поднимаем наверх — как и по клику хозяина. Это единственная оконная
        // операция, которую гость получает сразу: без неё нельзя добраться до
        // окна, лежащего под другим.
        if let Some(окно) = окно {
            state.space.raise_element(&окно, false);
        }
    }

    if let Some(указатель) = место.get_pointer() {
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
}

/// Колесо гостя.
pub fn колесо(state: &mut Parallax, id: u8, dx: f64, dy: f64) {
    use smithay::input::pointer::AxisFrame;
    let Some(место) = место_гостя(state, id) else { return };
    let Some(указатель) = место.get_pointer() else { return };
    let время = state.start_time.elapsed().as_millis() as u32;
    // Те же 15 логических пикселей на зубец, что и у своей мыши (см. input.rs)
    // — иначе прокрутка у гостя ощущалась бы иначе, чем у хозяина.
    let кадр = AxisFrame::new(время)
        .source(smithay::backend::input::AxisSource::Wheel)
        .value(smithay::backend::input::Axis::Horizontal, dx * 15.0)
        .v120(smithay::backend::input::Axis::Horizontal, (dx * 120.0) as i32)
        .value(smithay::backend::input::Axis::Vertical, dy * 15.0)
        .v120(smithay::backend::input::Axis::Vertical, (dy * 120.0) as i32);
    указатель.axis(state, кадр);
    указатель.frame(state);
}

/// Клавиша гостя. Скан-код приходит в нумерации evdev — ту же поправку +8
/// делает и синтетический ввод харнесса (см. synth.rs).
pub fn клавиша(state: &mut Parallax, id: u8, код: u32, нажата: bool) {
    let Some(место) = место_гостя(state, id) else { return };
    let Some(клавиатура) = место.get_keyboard() else { return };
    let serial = SERIAL_COUNTER.next_serial();
    let время = state.start_time.elapsed().as_millis() as u32;
    let состояние = if нажата {
        smithay::backend::input::KeyState::Pressed
    } else {
        smithay::backend::input::KeyState::Released
    };
    // trace!, а не debug!: строка на КАЖДОЕ нажатие и отпускание, а автоповтор
    // у зажатой клавиши идёт 25 раз в секунду — и это на каждого из пятерых.
    tracing::trace!(
        "plx/share: guest {id} key {код} pressed={нажата}, focus={}",
        клавиатура.current_focus().is_some(),
    );
    // Бинды композитора гостю ДАЮТСЯ — «сделай полный доступ», требование
    // Ярика 30.08.2026. До этого дня здесь стоял безусловный
    // `FilterResult::Forward`: гость мог печатать в приложения, но ни одна
    // команда самого parallax ему не подчинялась.
    //
    // Модификаторы берём У ЕГО МЕСТА, а не у хозяйского. Это и есть главная
    // ценность отдельного seat: xkb-состояние своё, поэтому «гость держит
    // Super» и «хозяин держит Super» — разные факты, и Super+1 у одного не
    // становится Super+1 у другого. Считай мы модификаторы по общему месту,
    // бинды гостя срабатывали бы от хозяйских нажатий и наоборот.
    клавиатура.input::<(), _>(
        state,
        (код + 8).into(),
        состояние,
        serial,
        время,
        |state, modifiers, handle| {
            // Бинды — на нажатие. Отпускание уходит клиенту всегда, иначе у
            // приложения останется «зажатая» клавиша, которую никто не отпустил.
            if !нажата {
                return FilterResult::Forward;
            }
            // Латинский символ, как и у своей клавиатуры: иначе в русской
            // раскладке не сработал бы ни один бинд (см. input.rs).
            let raw_latin = handle
                .raw_latin_sym_or_raw_current_sym()
                .map(|s| s.raw())
                .unwrap_or_else(|| handle.modified_sym().raw());
            let mods = crate::config::ModMask {
                ctrl: modifiers.ctrl,
                alt: modifiers.alt,
                shift: modifiers.shift,
                logo: modifiers.logo,
            };
            let Some(действие) = state.lua_config.find_action(mods, raw_latin) else {
                return FilterResult::Forward;
            };
            if !гостю_можно(state, &действие) {
                tracing::info!("plx/share: action {действие:?} not granted to guest {id}");
                // Перехватываем, а не пропускаем: клиенту под курсором эта
                // комбинация тоже не нужна, а «ничего не произошло» — честный
                // ответ на запрещённое.
                return FilterResult::Intercept(());
            }
            state.dispatch_action(действие);
            FilterResult::Intercept(())
        },
    );
}

/// Можно ли отдать гостю это действие.
///
/// Полный доступ — не пустая формула: гость двигает окна, переключает столы,
/// закрывает программы, лезет в обзор, делает всё то же, что хозяин машины.
/// Здесь перечислено ровно то, что вырубает саму раздачу, а с ней и панель
/// управления, из которой гостя выгоняют:
///
/// * `Quit`, `Restart` — кладут сеанс хозяина целиком, вместе со всеми гостями;
/// * `VtSwitch` — уводит экран на другой терминал, у гостей остаётся замерший кадр;
/// * `Share*` — выключает раздачу изнутри неё же.
///
/// Снимается одной строкой: `set{ share_guest_all = true }` в `config.lua`.
fn гостю_можно(state: &Parallax, действие: &crate::config::Action) -> bool {
    use crate::config::Action as Д;
    if state.lua_config.share_guest_all {
        return true;
    }
    !matches!(
        действие,
        Д::Quit
            | Д::Restart
            | Д::VtSwitch(_)
            | Д::ShareStart(_)
            | Д::ShareStop
            | Д::ShareToggle(_)
    )
}

/// Место гостя по номеру (клон дешёвый — внутри Rc).
fn место_гостя(state: &Parallax, id: u8) -> Option<Seat<Parallax>> {
    state
        .раздача
        .as_ref()?
        .гости
        .iter()
        .find(|г| г.id == id)?
        .место
        .clone()
}

fn точка_гостя(state: &Parallax, id: u8) -> Point<f64, Logical> {
    state
        .раздача
        .as_ref()
        .and_then(|р| р.гости.iter().find(|г| г.id == id))
        .map(|г| Point::from((г.курсор.0, г.курсор.1)))
        .unwrap_or_default()
}

/// Убрать место ушедшего гостя: без этого клиенты продолжали бы видеть
/// `wl_seat`, за которым никого нет, и держали бы под него ресурсы.
pub fn убрать(state: &mut Parallax, место: Seat<Parallax>) {
    // Фокус снимаем явно: клиент, у которого он остался, ждал бы клавиш от
    // места, которого больше нет.
    if let Some(клавиатура) = место.get_keyboard() {
        let serial = SERIAL_COUNTER.next_serial();
        клавиатура.set_focus(state, None, serial);
    }
    // Сам `Seat` уничтожается сбросом последней ссылки — глобал `wl_seat`
    // уедет вместе с ним.
    drop(место);
}

/// Заглушки, чтобы обработчики Parallax могли звать это единообразно.
impl Parallax {
    pub fn гостевое_движение(&mut self, id: u8) {
        let точка = точка_гостя(self, id);
        движение(self, id, точка);
    }
    pub fn гостевая_кнопка(&mut self, id: u8, код: u32, нажата: bool) {
        кнопка(self, id, код, нажата);
    }
    pub fn гостевое_колесо(&mut self, id: u8, dx: f64, dy: f64) {
        колесо(self, id, dx, dy);
    }
    pub fn гостевая_клавиша(&mut self, id: u8, код: u32, нажата: bool) {
        клавиша(self, id, код, нажата);
    }
}
