//! Единый интерфейс к окну независимо от его природы: нативный Wayland-клиент
//! (xdg_toplevel) или X11-клиент через XWayland (X11Surface).
//!
//! Весь остальной код dawn работает с `smithay::desktop::Window` и НЕ должен
//! знать, какой протокол за ним стоит. Раньше он знал: везде стояло
//! `window.toplevel().unwrap()` / сравнение окон по `wl_surface()` — у X11-окна
//! `toplevel()` возвращает None, поэтому такое окно нельзя было ни
//! сфокусировать, ни отресайзить, а `.unwrap()` просто ронял компоситор.
//!
//! Правила:
//!   * сравнение окон — `a == b` (Window: PartialEq по внутреннему id);
//!   * поиск окна по поверхности — [`is_surface`];
//!   * размер/закрытие/configure — функции этого модуля.
//!
//! Отдельная тонкость X11: у окна поверхность появляется НЕ СРАЗУ. XWayland
//! связывает wl_surface с X11-окном через протокол xwayland_shell уже ПОСЛЕ
//! map_window_request, поэтому [`surface`] у только что появившегося X11-окна
//! честно возвращает None — фокус такое окно всё равно принимает, см. focus.rs.

use smithay::{
    desktop::{Window, WindowSurface},
    reexports::{
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::protocol::wl_surface::WlSurface,
    },
    utils::{Logical, Point, Size},
    wayland::seat::WaylandFocus,
};

use crate::state::{Dawn, TaggedWindow};
use crate::tiling::Layout;

// ── Идентификация ────────────────────────────────────────────────────────────

/// wl_surface окна (у X11 — та, которую XWayland связал с X11-окном).
pub fn surface(window: &Window) -> Option<WlSurface> {
    window.wl_surface().map(|s| s.into_owned())
}

/// Принадлежит ли поверхность этому окну.
pub fn is_surface(window: &Window, surface: &WlSurface) -> bool {
    window.wl_surface().as_deref() == Some(surface)
}

/// Идентификатор приложения: `app_id` у Wayland, `WM_CLASS` у X11 — им
/// оперирует сохранение/восстановление сессии (session.rs).
pub fn app_id(window: &Window) -> Option<String> {
    match window.underlying_surface() {
        WindowSurface::Wayland(t) => {
            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .and_then(|d| d.app_id.clone())
            })
        }
        WindowSurface::X11(s) => {
            let class = s.class();
            (!class.is_empty()).then_some(class)
        }
    }
}

/// Заголовок окна: `xdg_toplevel.title` у Wayland, `WM_NAME` у X11.
///
/// Нужен поиску окон (Super+F): человек помнит не app_id, а «ту вкладку с
/// документацией». Пустую строку не возвращаем — вызывающий подставит app_id.
pub fn title(window: &Window) -> Option<String> {
    let t = match window.underlying_surface() {
        WindowSurface::Wayland(t) => {
            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                states
                    .data_map
                    .get::<smithay::wayland::shell::xdg::XdgToplevelSurfaceData>()
                    .and_then(|d| d.lock().ok())
                    .and_then(|d| d.title.clone())
            })
        }
        WindowSurface::X11(s) => Some(s.title()),
    };
    t.filter(|s| !s.trim().is_empty())
}

// ── Размер и configure ───────────────────────────────────────────────────────

/// Что делать с tiled-состояниями окна при смене размера.
#[derive(Clone, Copy, PartialEq)]
pub enum Tiled {
    /// Выставить (окно уезжает в тайлинг).
    Set,
    /// Снять (окно уходит во float).
    Unset,
    /// Не трогать.
    Keep,
}

/// Задать окну размер. `size = None` — «решай сам» (только Wayland; у X11
/// такого понятия нет, там размер всегда явный, поэтому None = не трогать).
///
/// Wayland: складывает изменение в pending state, отправляет его [`configure`].
/// X11: применяется сразу — у X11 нечего копить, configure идёт по X-соединению
/// одним пакетом вместе с позицией (позицию потом досылает
/// [`Dawn::sync_x11_geometry`]).
pub fn set_size(window: &Window, size: Option<Size<i32, Logical>>, tiled: Tiled) {
    match window.underlying_surface() {
        WindowSurface::Wayland(t) => {
            t.with_pending_state(|state| {
                state.size = size;
                match tiled {
                    Tiled::Set => {
                        state.states.set(xdg_toplevel::State::TiledLeft);
                        state.states.set(xdg_toplevel::State::TiledRight);
                        state.states.set(xdg_toplevel::State::TiledTop);
                        state.states.set(xdg_toplevel::State::TiledBottom);
                    }
                    Tiled::Unset => {
                        state.states.unset(xdg_toplevel::State::TiledLeft);
                        state.states.unset(xdg_toplevel::State::TiledRight);
                        state.states.unset(xdg_toplevel::State::TiledTop);
                        state.states.unset(xdg_toplevel::State::TiledBottom);
                    }
                    Tiled::Keep => {}
                }
            });
        }
        WindowSurface::X11(s) => {
            let Some(size) = size else { return };
            let mut geo = s.geometry();
            if geo.size == size || s.is_override_redirect() {
                return;
            }
            geo.size = size;
            if let Err(err) = s.configure(geo) {
                tracing::warn!("dawn/xwayland: configure(size) failed: {}", err);
            }
        }
    }
}

/// Развернуть окно на весь экран: размер монитора + состояние Fullscreen.
///
/// Состояние важно не меньше размера: по нему клиент убирает СВОИ рамки,
/// скругления и тени (GTK/Electron рисуют их сами), а видеоплееры включают
/// полноэкранный режим. Tiled-состояния при этом снимаются — они бы заставили
/// клиента и дальше считать себя частью сетки.
pub fn set_fullscreen(window: &Window, size: Option<Size<i32, Logical>>) {
    match window.underlying_surface() {
        WindowSurface::Wayland(t) => {
            t.with_pending_state(|state| {
                state.states.set(xdg_toplevel::State::Fullscreen);
                state.states.unset(xdg_toplevel::State::TiledLeft);
                state.states.unset(xdg_toplevel::State::TiledRight);
                state.states.unset(xdg_toplevel::State::TiledTop);
                state.states.unset(xdg_toplevel::State::TiledBottom);
                state.size = size;
            });
        }
        WindowSurface::X11(s) => {
            if let Err(err) = s.set_fullscreen(true) {
                tracing::warn!("dawn/xwayland: set_fullscreen: {}", err);
            }
            set_size(window, size, Tiled::Keep);
        }
    }
}

/// Снять полноэкранный режим и вернуть окну прежний размер.
pub fn unset_fullscreen(window: &Window, size: Option<Size<i32, Logical>>) {
    match window.underlying_surface() {
        WindowSurface::Wayland(t) => {
            t.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Fullscreen);
                state.size = size;
            });
        }
        WindowSurface::X11(s) => {
            if let Err(err) = s.set_fullscreen(false) {
                tracing::warn!("dawn/xwayland: unset_fullscreen: {}", err);
            }
            set_size(window, size, Tiled::Keep);
        }
    }
}

/// Отметить/снять состояние «идёт интерактивный ресайз» (только Wayland —
/// клиенты по нему отключают дорогую перерисовку).
pub fn set_resizing(window: &Window, resizing: bool) {
    if let Some(t) = window.toplevel() {
        t.with_pending_state(|state| {
            if resizing {
                state.states.set(xdg_toplevel::State::Resizing);
            } else {
                state.states.unset(xdg_toplevel::State::Resizing);
            }
        });
    }
}

/// Отправить накопленный configure. Для X11 — no-op: там ничего не копится.
pub fn configure(window: &Window) {
    if let Some(t) = window.toplevel() {
        t.send_pending_configure();
    }
}

/// Ограничения размера, заданные клиентом: (min, max). Нулевая компонента
/// max = «без ограничения» (так это устроено в xdg_shell; у X11 отсутствующая
/// подсказка WM_NORMAL_HINTS приводится к тому же виду).
pub fn size_constraints(window: &Window) -> (Size<i32, Logical>, Size<i32, Logical>) {
    match window.underlying_surface() {
        WindowSurface::Wayland(t) => {
            smithay::wayland::compositor::with_states(t.wl_surface(), |states| {
                let mut guard = states
                    .cached_state
                    .get::<smithay::wayland::shell::xdg::SurfaceCachedState>();
                let data = guard.current();
                (data.min_size, data.max_size)
            })
        }
        WindowSurface::X11(s) => (
            s.min_size().unwrap_or_default(),
            s.max_size().unwrap_or_default(),
        ),
    }
}

/// Размер буфера, прикреплённого к поверхности (в логических пикселях).
/// Нужен для курсоров клиентов: их картинку мы приводим к своему размеру.
pub fn surface_buffer_size(surface: &WlSurface) -> Option<Size<i32, Logical>> {
    smithay::wayland::compositor::with_states(surface, |states| {
        states
            .data_map
            .get::<smithay::backend::renderer::utils::RendererSurfaceStateUserData>()
            .and_then(|d| d.lock().ok())
            .and_then(|d| d.surface_size())
    })
}

/// Размер, который окно занимает сейчас (по последнему буферу/геометрии).
pub fn current_size(window: &Window) -> Size<i32, Logical> {
    window.geometry().size
}

/// Попросить клиента закрыться.
pub fn close(window: &Window) {
    match window.underlying_surface() {
        WindowSurface::Wayland(t) => t.send_close(),
        WindowSurface::X11(s) => {
            if let Err(err) = s.close() {
                tracing::warn!("dawn/xwayland: close failed: {}", err);
            }
        }
    }
}

/// Окно — это override-redirect X11-окно (меню, тултип, иконка драга)?
///
/// Такие окна X-клиент рисует сам и сам же ими распоряжается: место выбирает
/// в корневых координатах, а из списка кандидатов на фокус выпадает по самому
/// смыслу флага (ICCCM: «оконный менеджер, не трогай»). У нативных
/// Wayland-клиентов аналог — xdg_popup, и он тоже не toplevel, поэтому в space
/// как отдельное окно не попадает.
pub fn is_override_redirect(window: &Window) -> bool {
    match window.underlying_surface() {
        WindowSurface::X11(s) => s.is_override_redirect(),
        WindowSurface::Wayland(_) => false,
    }
}

/// Сфокусировать окно клавиатурой: активирует его, гасит остальные и
/// поднимает наверх.
pub fn focus(state: &mut Dawn, window: &Window) {
    // Меню и тултипы X11-клиента (override-redirect) фокус не принимают —
    // и что важнее, не имеют права его ОТНИМАТЬ.
    //
    // Ниже по коду focus() гасит все остальные окна (`set_activated(false)`) и
    // переводит клавиатуру на цель. Для меню Steam это смертельно: наведя на
    // выпавшее меню курсор, пользователь через sloppy focus (input.rs) вызывал
    // focus() на самом меню, главное окно Steam получало «ты больше не
    // активно» и тут же своё меню закрывало. Ровно то же было бы при клике по
    // пункту меню.
    //
    // Наверх такие окна поднимать тоже не нужно: они и так замаплены последними
    // (mapped_override_redirect_window), то есть уже поверх всего.
    if is_override_redirect(window) {
        return;
    }
    let serial = smithay::utils::SERIAL_COUNTER.next_serial();
    state.space.raise_element(window, true);
    // Меню и тултипы обязаны остаться поверх своего окна. raise_element выше
    // кладёт наверх ЕГО, то есть уже открытое меню оказалось бы за родителем —
    // видно это было бы как «меню моргнуло и пропало». Поднимаем их следом.
    let popups: Vec<Window> = state
        .space
        .elements()
        .filter(|w| is_override_redirect(w))
        .cloned()
        .collect();
    for popup in &popups {
        state.space.raise_element(popup, false);
    }
    let others: Vec<Window> = state
        .space
        .elements()
        .filter(|w| *w != window && !is_override_redirect(w))
        .cloned()
        .collect();
    for w in others {
        w.set_activated(false);
        configure(&w);
    }
    window.set_activated(true);
    configure(window);
    state.raise_x11(window);

    // X11-окно можно фокусировать даже до того, как у него появится
    // wl_surface: enter доиграется при связывании (см. focus.rs).
    if let Some(target) = crate::focus::KeyboardFocusTarget::for_window(window) {
        if let Some(kb) = state.seat.get_keyboard() {
            kb.set_focus(state, Some(target), serial);
        }
    }
}

// ── Появление нового окна ────────────────────────────────────────────────────

impl Dawn {
    /// Размер, который получит новое окно в текущей раскладке. Считается ДО
    /// создания окна, чтобы клиенту ушёл ровно один configure, а не два
    /// (наш «предварительный» и потом ещё один от arrange()).
    pub fn predict_new_window_size(&self) -> Size<i32, Logical> {
        use crate::tiling::{GAP_INNER, GAP_OUTER};
        let output_size: Size<i32, Logical> = self
            .space
            .outputs()
            .next()
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| g.size)
            .unwrap_or_else(|| (1920, 1080).into());

        let (w, h) = match self.tile_config.layout {
            // Columns: колонка в полэкрана, полная высота (см. apply_columns_layout).
            Layout::Columns => (
                (output_size.w / 2 - GAP_INNER).max(1),
                (output_size.h - GAP_OUTER * 2).max(1),
            ),
            Layout::Float => (output_size.w / 2, output_size.h / 2),
            Layout::Monocle => (
                (output_size.w - GAP_OUTER * 2).max(1),
                (output_size.h - GAP_OUTER * 2).max(1),
            ),
            // Tile: половина слота сфокусированного окна — именно его дерево и
            // разделит при вставке (см. tiling::predict_tile_size).
            Layout::Tile => self
                .predict_tile_size()
                .map(|s| (s.w, s.h))
                .unwrap_or((output_size.w, output_size.h)),
        };
        Size::from((w, h))
    }

    /// Общая часть появления окна для обоих протоколов: позиция, теги, место в
    /// списке/дереве, раскладка, фокус. Вызывается из
    /// `XdgShellHandler::new_toplevel` и `XwmHandler::map_window_request`.
    ///
    /// `floating` — окно не участвует в тайлинге (X11-диалоги с фиксированным
    /// размером; у Wayland-окон всегда false).
    pub fn insert_new_window(&mut self, window: Window, size: Size<i32, Logical>, floating: bool) {
        let output_size: Size<i32, Logical> = self
            .space
            .outputs()
            .next()
            .and_then(|o| self.space.output_geometry(o))
            .map(|g| g.size)
            .unwrap_or_else(|| (1920, 1080).into());
        let is_tile = self.tile_config.layout != Layout::Float && !floating;
        let current_tags = self.viewport.current_tags();

        // Новое окно появляется по центру текущего viewport (позиция камеры),
        // а не под курсором — курсор может быть где угодно на бесконечном холсте.
        let zoom = self.viewport.zoom;
        let camera_center_x = self.viewport.cam_x + output_size.w as f64 / (2.0 * zoom);
        let camera_center_y = self.viewport.cam_y + output_size.h as f64 / (2.0 * zoom);
        let spawn: Point<i32, Logical> = (
            camera_center_x as i32 - size.w / 2,
            camera_center_y as i32 - size.h / 2,
        )
            .into();

        // Порядок в tagged_windows задаёт только обход Super+Tab (focus_stack):
        // геометрию в Tile определяет BSP-дерево (dwindle.rs), а не позиция в
        // списке. Всё равно вставляем сразу за сфокусированным — так Tab идёт
        // по соседям, а не прыгает в конец.
        let insert_idx = if is_tile && self.tile_config.layout != Layout::Columns {
            let focused = self.focused_surface();
            focused
                .as_ref()
                .and_then(|fs| {
                    self.tagged_windows
                        .iter()
                        .position(|tw| is_surface(&tw.window, fs))
                })
                .map(|i| i + 1)
                .unwrap_or(self.tagged_windows.len())
        } else {
            self.tagged_windows.len()
        };

        self.tagged_windows.insert(
            insert_idx,
            TaggedWindow {
                window: window.clone(),
                tags: current_tags,
                position: spawn,
                float_position: spawn,
                float_size: floating.then_some(size),
                float_position_set: false,
                floating,
                // Диалоги плавают по своей природе — сборка в тайлинг их не
                // трогает (см. TaggedWindow::float_pinned).
                float_pinned: floating,
                folded: false,
                pre_constellation: None,
            },
        );

        self.space.map_element(window.clone(), spawn, false);
        // Columns (niri): новое окно — отдельная колонка сразу справа от
        // активной, камера едет к нему (см. columns.rs). Делаем ДО arrange(),
        // чтобы reconcile не дописал его в конец.
        if is_tile && self.tile_config.layout == Layout::Columns {
            self.columns_insert_new(&window);
        }
        self.arrange();
        if self.tile_config.layout == Layout::Columns {
            self.columns_scroll_to_active();
        }

        // Появление "с ростом" (Hyprland-style) — только Float: в тайлинге
        // размер и так уже точный (см. predict_new_window_size), а позицию уже
        // анимирует arrange().
        //
        // X11 из этого исключён: каждый кадр анимации — это отдельный
        // configure по X-соединению, а перерисоваться в такт им X11-клиент всё
        // равно не успевает, так что «рост» выйдет рванным и недешёвым.
        if !is_tile && !window.is_x11() {
            let center = Point::from((
                (spawn.x + size.w / 2) as f64,
                (spawn.y + size.h / 2) as f64,
            ));
            self.window_open_anims.push((
                window.clone(),
                crate::anim::OpenAnim::new(
                    center,
                    (size.w as f64, size.h as f64),
                    std::time::Duration::from_millis(260),
                ),
            ));
        }

        // Автоматически отдаём фокус новому окну (как sway/hyprland)
        focus(self, &window);

        tracing::info!(
            "dawn: new window ({}) idx={} tags={:#b} spawn=({},{}) size={}x{}",
            if window.is_x11() { "x11" } else { "wayland" },
            insert_idx,
            current_tags,
            spawn.x,
            spawn.y,
            size.w,
            size.h,
        );

        // map_element сам по себе экран не обновляет — если VBlank-цепочка
        // рендера к этому моменту уже остановилась (нет изменений с прошлого
        // кадра), новое окно так и останется невидимым до явного рендера.
        self.request_redraw();
    }

    /// Убрать окно из всех структур компоситора: список тегов, BSP-деревья,
    /// выделение, созвездия, анимации. Общее для xdg (`toplevel_destroyed`) и
    /// X11 (`destroyed_window`/`unmapped_window`).
    pub fn forget_window(&mut self, window: &Window) {
        // niri: соседа и признак «закрыли именно активное окно» считаем ДО
        // всех удалений — после них окна уже нет ни в полосе, ни в фокусе.
        let (сосед, был_в_фокусе) = if self.tile_config.layout == Layout::Columns {
            let f = self.focused_surface()
                .is_some_and(|fs| is_surface(window, &fs));
            (self.columns_neighbour_before(window), f)
        } else {
            (None, false)
        };
        // Закрылось развёрнутое окно — снимаем режим и возвращаем камеру с
        // зумом, иначе холст остался бы «залипшим» в кадре мёртвого окна.
        self.forget_fullscreen(window);
        self.tagged_windows.retain(|tw| &tw.window != window);
        // Иначе анимации продолжат слать configure/map_element мёртвому окну.
        self.window_pos_anims.retain(|(w, _)| w != window);
        self.window_open_anims.retain(|(w, _)| w != window);
        // Из BSP-деревьев окно убираем ЯВНО: дерево неактивного тега само
        // синхронизируется только когда на этот тег переключатся, а до тех пор
        // держало бы мёртвый Window живым (см. dwindle.rs, sync_dwindle_tree).
        for tree in self.dwindle_trees.values_mut() {
            tree.remove(window);
        }
        // Убираем мёртвые ссылки из выделения/созвездий (см. selection.rs).
        self.selected_windows.retain(|w| w != window);
        for group in &mut self.constellations {
            group.retain(|w| w != window);
        }
        self.constellations.retain(|g| g.len() > 1);
        self.space.unmap_elem(window);
        // Закрытие окна могло опустошить стол в середине ленты — в niri-режиме
        // лента столов плотная, дыр в ней не бывает (в остальных раскладках
        // теги независимы и трогать их нельзя, см. columns_compact_workspaces).
        self.columns_compact_workspaces();
        self.arrange();
        // Фокус на предыдущее окно ленты + камера следом за ним.
        self.columns_after_close(сосед, был_в_фокусе);
        self.request_plane_reset();
        self.request_redraw();
    }
}
