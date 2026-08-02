use smithay::{
    delegate_layer_shell,
    desktop::layer_map_for_output,
    reexports::wayland_server::protocol::wl_output::WlOutput,
    wayland::shell::wlr_layer::{
        Layer, LayerSurface as WlrLayerSurface, WlrLayerShellHandler, WlrLayerShellState,
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
        let output = self
            .space
            .outputs()
            .next()
            .expect("dawn: layer-surface without any output")
            .clone();
        let layer_surface = smithay::desktop::LayerSurface::new(surface, _namespace);
        {
            let mut map = layer_map_for_output(&output);
            if let Err(e) = map.map_layer(&layer_surface) {
                tracing::warn!("dawn/layer: map_layer failed: {:?}", e);
                return;
            }
        } // map дропается здесь, до request_redraw
        self.request_redraw();
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        // Собираем output-ы ДО заимствований.
        let outputs: Vec<_> = self.space.outputs().cloned().collect();
        for output in &outputs {
            let layer_found = {
                let map = layer_map_for_output(output);
                let all: Vec<_> = map.layers().cloned().collect();
                all.into_iter().find(|l| l.layer_surface() == &surface)
            };
            if let Some(layer) = layer_found {
                let mut map = layer_map_for_output(output);
                map.unmap_layer(&layer);
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
