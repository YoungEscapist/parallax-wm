use smithay::{
    desktop::{layer_map_for_output, WindowSurfaceType},
    backend::{
        allocator::dmabuf::Dmabuf,
        renderer::{ImportDma, utils::on_commit_buffer_handler},
    },
    delegate_compositor, delegate_dmabuf, delegate_shm,
    reexports::wayland_server::protocol::{
        wl_buffer::WlBuffer,
        wl_surface::WlSurface,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{CompositorClientState, CompositorHandler, CompositorState, with_states},
        shell::wlr_layer::LayerSurfaceData,
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        shm::{ShmHandler, ShmState},
    },
    utils::{Logical, Size},
    xwayland::XWaylandClientData,
};

use std::cell::RefCell;

use crate::{
    grabs::resize_grab::handle_commit,
    state::{ClientState, Dawn},
};

impl CompositorHandler for Dawn {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        // У клиента XWayland своя структура данных (её создаёт сам smithay при
        // спавне сервера), а не наш ClientState — раньше тут был просто
        // unwrap(), то есть паника на первом же коммите X11-клиента.
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if let Some(window) = self
            .space
            .elements()
            .find(|w| crate::xwin::is_surface(w, surface))
            .cloned()
        {
            window.on_commit();
            // trace!: срабатывает на каждый commit каждого клиента — у
            // анимированного окна это десятки строк в секунду в горячем пути.
            tracing::trace!("dawn: commit for mapped window");
        }

        // Эластичное расталкивание соседей при ресайзе — ТОЛЬКО в режиме
        // коллизии (Super+S). Без него плавающие окна на холсте должны стоять
        // там, где их поставили: ресайз соседа не имеет права их двигать.
        // Обзор ничего не меняет — ресайз там работает по обычной логике
        // (тайловые окна тянут деления раскладки), как будто обзора нет.
        let elastic = self.is_snapping_enabled;
        handle_commit(&mut self.space, surface, elastic);
        self.popups.commit(surface);

        // Первый configure всплывающему окну шлём МЫ.
        //
        // Клиент, создав xdg_popup, коммитит его пустым и ждёт configure —
        // пока он не придёт, буфер не прикрепляется и меню не появляется
        // вообще. Ни smithay, ни PopupManager этот configure сами не шлют:
        // размер и место — дело компоновщика (см. unconstrain_popup).
        // Замер 05.08.2026: тестовый клиент создавал попап и молчал навсегда,
        // «ПОПАП: получен configure» в его выводе не появлялось.
        if let Some(popup) = self.popups.find_popup(surface) {
            match popup {
                smithay::desktop::PopupKind::Xdg(ref xdg) => {
                    if !xdg.is_initial_configure_sent() {
                        // Ошибка тут означает мёртвый ресурс — клиент уже ушёл.
                        let _ = xdg.send_configure();
                    }
                }
                smithay::desktop::PopupKind::InputMethod(_) => {}
            }
        }

        self.ensure_layer_configured(surface);
        self.focus_layer_if_wanted(surface);

        // Без этого новый буфер клиента (например, первый кадр kitty/foot)
        // остаётся закоммиченным только в состоянии — VBlank-цепочка рендера
        // могла уже умереть из-за отсутствия изменений, и без явного пинка
        // сюда экран так и останется на предыдущем кадре.
        self.request_redraw();
    }
}

impl Dawn {
    /// Отдать клавиатуру слою, который её просит (лаунчер, панель с вводом).
    ///
    /// Делаем это на КОММИТЕ, а не при создании поверхности: в момент
    /// new_layer_surface клиент ещё не прислал ни буфера, ни своих желаний, и
    /// фокус на пустую поверхность smithay просто терял — fuzzel открывался
    /// немым, ни набрать, ни закрыть по Esc.
    fn focus_layer_if_wanted(&mut self, surface: &WlSurface) {
        use smithay::wayland::shell::wlr_layer::KeyboardInteractivity;
        // Страховка: слой мог умереть не через layer_destroyed (клиента убили
        // сигналом). Мёртвая ссылка глушила бы sloppy-focus навсегда — по
        // экрану это выглядит как «в окнах перестали работать клики».
        if let Some(s) = self.layer_keyboard.clone() {
            use smithay::reexports::wayland_server::Resource;
            if !s.is_alive() {
                self.layer_keyboard = None;
                if let Some(w) = self.focused_window() {
                    crate::xwin::focus(self, &w);
                }
            }
        }
        if self.layer_keyboard.as_ref() == Some(surface) {
            return; // уже отдали
        }
        let Some(output) = self.layer_output.clone()
            .or_else(|| self.space.outputs().next().cloned()) else { return };
        let слой = {
            let map = layer_map_for_output(&output);
            map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL).cloned()
        };
        let Some(слой) = слой else { return };
        let хочет = matches!(
            слой.cached_state().keyboard_interactivity,
            KeyboardInteractivity::Exclusive | KeyboardInteractivity::OnDemand,
        );
        if !хочет { return; }
        if let Some(kb) = self.seat.get_keyboard() {
            let serial = smithay::utils::SERIAL_COUNTER.next_serial();
            let цель = crate::focus::KeyboardFocusTarget::Wayland(surface.clone());
            kb.set_focus(self, Some(цель), serial);
            self.layer_keyboard = Some(surface.clone());
            tracing::info!("dawn: клавиатура отдана layer-поверхности");
        }
    }

    /// Первый configure для layer-поверхности (wlr-layer-shell).
    ///
    /// Его ОБЯЗАН отправить композитор в ответ на первый commit клиента —
    /// `LayerMap::arrange` этого сознательно не делает (см. комментарий в
    /// smithay: до первого commit клиент ещё не сообщил свой желаемый размер,
    /// и configure нарушил бы протокол). В dawn этого шага не было вообще:
    /// layer-клиент вставал в LayerMap, но configure не получал никогда, а без
    /// него ему запрещено прикреплять буфер. Поэтому обои dwall не появлялись
    /// (его bg_configured навсегда оставался false), и меню выбора обоев тоже:
    /// в WAYLAND_DEBUG видно set_size(740,480) без единого события в ответ.
    fn ensure_layer_configured(&mut self, surface: &WlSurface) {
        // Признак снимаем ДО захвата LayerMap: with_states берёт замок данных
        // поверхности, и держать оба замка разом незачем (тем же вложением
        // замков мы уже ловили мёртвый клинч в build_layer_elements).
        let already = with_states(surface, |states| {
            states.data_map.get::<LayerSurfaceData>()
                .map(|d| d.lock().unwrap().initial_configure_sent)
        });
        // Не layer-поверхность (обычное окно, popup) — выходим.
        let Some(already) = already else { return };

        // Уже сконфигурирована — пересчитываем ТОЛЬКО если клиент сам
        // передумал насчёт размера (см. relayout_if_client_resized).
        //
        // Здесь стоял map.arrange() на каждый commit (как в anvil), и он
        // устраивал пинг-понг: arrange пересчитывал размер поверхности, слал
        // configure, клиент отвечал ack + commit, commit снова звал arrange —
        // и так по кругу. В протокольном логе dwall серийник configure за
        // несколько секунд доходил до 9340, а размер меню приезжал 2482×1047
        // вместо 740×480. Клиент при этом жёг ядро вхолостую и переставал
        // отвечать на сигналы — меню открывалось один раз и больше никогда.
        if already {
            self.relayout_if_client_resized(surface);
            return;
        }

        let Some(output) = self.layer_output.clone()
            .or_else(|| self.space.outputs().next().cloned()) else { return };
        let layer = {
            let mut map = layer_map_for_output(&output);
            // Единственный arrange — перед ПЕРВЫМ configure: до него клиент
            // как раз сообщил желаемый размер своим первым commit.
            map.arrange();
            map.layer_for_surface(surface, WindowSurfaceType::ALL).cloned()
        }; // замок LayerMap отпущен здесь, до отправки configure
        let Some(layer) = layer else { return };
        layer.layer_surface().send_configure();
    }

    /// Повторный configure для слоя, который САМ попросил другой размер.
    ///
    /// Layer-клиент, которому нужно вырасти, шлёт `set_size(w,h)` + commit и
    /// ЖДЁТ configure: до него прикреплять новый буфер ему нельзя. Раз dawn
    /// после первого configure не делал больше ничего, такой клиент застревал
    /// навсегда на первом кадре.
    ///
    /// Так ломался mako (замер 10.08.2026): первое уведомление появлялось,
    /// второе не появлялось никогда, а первое висело на экране даже после
    /// `makoctl dismiss --all` — по dbus mako отвечал, что уведомлений нет,
    /// но кадр не менялся, потому что mako стоял в ожидании configure на
    /// новую высоту стопки.
    ///
    /// Пинг-понга, из-за которого arrange отсюда когда-то убрали, тут нет:
    /// пересчёт запускает только смена ЗАПРОШЕННОГО клиентом размера, а не
    /// каждый commit. Клиент с постоянным set_size (меню dwall) проходит
    /// через эту ветку ровно один раз.
    fn relayout_if_client_resized(&mut self, surface: &WlSurface) {
        let Some(output) = self.layer_output.clone()
            .or_else(|| self.space.outputs().next().cloned()) else { return };

        // Слой достаём под замком LayerMap, а его cached_state читаем уже
        // после — вложение «замок карты + замок данных поверхности» здесь ни
        // к чему (той же вложенностью ловился клинч в build_layer_elements).
        let layer = {
            let map = layer_map_for_output(&output);
            map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL).cloned()
        };
        let Some(layer) = layer else { return };
        let запрошен = layer.cached_state().size;

        let изменился = with_states(surface, |states| {
            states.data_map.insert_if_missing(RefCell::<ПрошлыйЗапросРазмера>::default);
            let ячейка = states.data_map.get::<RefCell<ПрошлыйЗапросРазмера>>().unwrap();
            let прошлый = ячейка.borrow_mut().0.replace(запрошен);
            прошлый != Some(запрошен)
        });
        if !изменился {
            return;
        }

        // arrange сам шлёт configure — и только тем слоям, у которых размер
        // реально поменялся (см. size_changed в LayerMap::arrange).
        layer_map_for_output(&output).arrange();
        self.request_redraw();
    }
}

/// Размер, который layer-клиент просил в прошлый раз (`set_size`).
///
/// Живёт в data_map самой поверхности, поэтому исчезает вместе с ней —
/// отдельной чистки при layer_destroyed не нужно.
#[derive(Default)]
struct ПрошлыйЗапросРазмера(Option<Size<i32, Logical>>);

impl BufferHandler for Dawn {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl ShmHandler for Dawn {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for Dawn {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        for device in self.udev_devices.values_mut() {
            if device.gles.import_dmabuf(&dmabuf, None).is_ok() {
                let _ = notifier.successful::<Dawn>();
                return;
            }
        }
        tracing::warn!("dawn/dmabuf: import failed");
        notifier.failed();
    }
}

delegate_compositor!(Dawn);
delegate_shm!(Dawn);
delegate_dmabuf!(Dawn);
// wp_viewporter: обработчика писать не надо, smithay сам складывает src/dst в
// состояние поверхности, а рендер читает их в SurfaceView::from_states.
// См. Dawn::viewporter_state.
smithay::delegate_viewporter!(Dawn);
