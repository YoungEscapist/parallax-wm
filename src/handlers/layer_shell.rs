use smithay::{
    delegate_layer_shell,
    desktop::layer_map_for_output,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    wayland::shell::wlr_layer::{
        KeyboardInteractivity, Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler,
        WlrLayerShellState,
    },
};

use crate::state::Dawn;

impl WlrLayerShellHandler for Dawn {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        _output: Option<WlOutput>,
        _layer: Layer,
        _namespace: String,
    ) {
        // Берём первый (и пока единственный) output из space.
        // Слои кладём на выход-призрак с масштабом 1 (см. Dawn::layer_output).
        let output = self.layer_output.clone().unwrap_or_else(|| {
            self.space.outputs().next()
                .expect("dawn: layer-surface without any output")
                .clone()
        });
        let layer_surface = smithay::desktop::LayerSurface::new(surface, _namespace);
        {
            let mut map = layer_map_for_output(&output);
            if let Err(e) = map.map_layer(&layer_surface) {
                tracing::warn!("dawn/layer: map_layer failed: {:?}", e);
                return;
            }
        } // map дропается здесь, до request_redraw

        // Клавиатуру отдаём слою, если он её просит (лаунчеры, панели с
        // вводом). Без этого fuzzel открывался, но не получал ни единой
        // клавиши: ни набрать имя приложения, ни закрыть по Esc.
        // Exclusive и OnDemand трактуем одинаково: у нас нет «клика по слою»,
        // после которого OnDemand получил бы фокус сам.
        let хочет_клавиатуру = matches!(
            layer_surface.cached_state().keyboard_interactivity,
            KeyboardInteractivity::Exclusive | KeyboardInteractivity::OnDemand,
        );
        if хочет_клавиатуру {
            if let Some(kb) = self.seat.get_keyboard() {
                let serial = smithay::utils::SERIAL_COUNTER.next_serial();
                let цель = crate::focus::KeyboardFocusTarget::Wayland(
                    layer_surface.wl_surface().clone(),
                );
                kb.set_focus(self, Some(цель), serial);
                self.layer_keyboard = Some(layer_surface.wl_surface().clone());
            }
        }
        self.request_redraw();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        // Собираем output-ы ДО заимствований.
        let outputs: Vec<_> = self.layer_output.clone().into_iter()
            .chain(self.space.outputs().cloned())
            .collect();
        for output in &outputs {
            let layer_found = {
                let map = layer_map_for_output(output);
                let all: Vec<_> = map.layers().cloned().collect();
                all.into_iter().find(|l| l.layer_surface() == &surface)
            };
            if let Some(layer) = layer_found {
                let mut map = layer_map_for_output(output);
                map.unmap_layer(&layer);
                drop(map);
                // Слой ушёл — клавиатуру возвращаем окну, которое было в
                // фокусе до него, иначе ввод проваливается в никуда.
                if self.layer_keyboard.as_ref() == Some(surface.wl_surface()) {
                    self.layer_keyboard = None;
                    if let Some(w) = self.focused_window() {
                        crate::xwin::focus(self, &w);
                    }
                }
                self.request_redraw();
                return;
            }
        }
    }

    fn ack_configure(
        &mut self,
        _surface: smithay::reexports::wayland_server::protocol::wl_surface::WlSurface,
        _configure: smithay::wayland::shell::wlr_layer::LayerSurfaceConfigure,
    ) {
    }
}

delegate_layer_shell!(Dawn);
