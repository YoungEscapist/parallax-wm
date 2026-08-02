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
    xwayland::XWaylandClientData,
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

        // Без этого новый буфер клиента (например, первый кадр kitty/foot)
        // остаётся закоммиченным только в состоянии — VBlank-цепочка рендера
        // могла уже умереть из-за отсутствия изменений, и без явного пинка
        // сюда экран так и останется на предыдущем кадре.
        self.request_redraw();
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
