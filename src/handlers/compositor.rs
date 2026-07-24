use smithay::{
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
        compositor::{CompositorClientState, CompositorHandler, CompositorState},
        dmabuf::{DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        shm::{ShmHandler, ShmState},
    },
};

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
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);

        if let Some(window) = self
            .space
            .elements()
            .find(|w| {
                w.toplevel()
                    .map(|t| t.wl_surface() == surface)
                    .unwrap_or(false)
            })
            .cloned()
        {
            window.on_commit();
            tracing::debug!("dawn: commit for mapped window");
        }

        handle_commit(&mut self.space, surface);
        self.popups.commit(surface);

        // Без этого новый буфер клиента (например, первый кадр kitty/foot)
        // остаётся закоммиченным только в состоянии — VBlank-цепочка рендера
        // могла уже умереть из-за отсутствия изменений, и без явного пинка
        // сюда экран так и останется на предыдущем кадре.
        crate::udev::render_all(self);
    }
}

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
