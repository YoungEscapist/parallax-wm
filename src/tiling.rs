use smithay::{
    desktop::Window,
    reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::{Logical, Rectangle},
};

use crate::state::Dawn;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Layout {
    Tile,    // dwindle горизонтальный
    Float,
    Monocle,
}

impl Layout {
    pub fn symbol(&self) -> &'static str {
        match self {
            Layout::Tile    => "[H]",
            Layout::Float   => "><>",
            Layout::Monocle => "[M]",
        }
    }
}

pub struct TileConfig {
    pub nmaster:     usize,
    pub mfact:       f32,
    pub layout:      Layout,
    pub prev_layout: Layout,
}

impl Default for TileConfig {
    fn default() -> Self {
        Self { nmaster: 1, mfact: 0.55, layout: Layout::Tile, prev_layout: Layout::Float }
    }
}

// ── Dwindle: рекурсивный горизонтальный split ─────────────────────────────────
// n=1: [  A  ]
// n=2: [ A ][ B ]   ← горизонтальный split (лево/право)
// n=3: [ A ][ B ]   ← B делится вертикально (верх/низ)
//           [ C ]
// n=4: [ A ][ B ]   ← C делится горизонтально
//           [C][D]
// Чередуем горизонталь/вертикаль на каждом уровне

fn dwindle_rects(
    rect: Rectangle<i32, Logical>,
    n: usize,
    split_horizontal: bool, // true = лево/право, false = верх/низ
) -> Vec<Rectangle<i32, Logical>> {
    if n == 0 { return vec![]; }
    if n == 1 { return vec![rect]; }

    let (first, rest) = if split_horizontal {
        // Делим лево/право
        let w_first = (rect.size.w as f32 * 0.5).round() as i32;
        let w_rest  = rect.size.w - w_first;
        let first = Rectangle::new(
            rect.loc,
            (w_first, rect.size.h).into(),
        );
        let rest = Rectangle::new(
            (rect.loc.x + w_first, rect.loc.y).into(),
            (w_rest, rect.size.h).into(),
        );
        (first, rest)
    } else {
        // Делим верх/низ
        let h_first = (rect.size.h as f32 * 0.5).round() as i32;
        let h_rest  = rect.size.h - h_first;
        let first = Rectangle::new(
            rect.loc,
            (rect.size.w, h_first).into(),
        );
        let rest = Rectangle::new(
            (rect.loc.x, rect.loc.y + h_first).into(),
            (rect.size.w, h_rest).into(),
        );
        (first, rest)
    };

    let mut result = vec![first];
    // Следующий уровень — противоположное направление
    result.extend(dwindle_rects(rest, n - 1, !split_horizontal));
    result
}

impl Dawn {
    pub fn arrange(&mut self) {
        match self.tile_config.layout {
            Layout::Tile    => self.apply_tile_layout(),
            Layout::Monocle => self.apply_monocle_layout(),
            Layout::Float   => {}
        }
    }

    pub fn apply_tile_layout(&mut self) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };
        let geo = match self.space.output_geometry(&output) {
            Some(g) => g,
            None => return,
        };

        let current_tags = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows
            .iter()
            .filter(|tw| tw.tags & current_tags != 0 && !tw.floating)
            .map(|tw| tw.window.clone())
            .collect();

        let n = visible.len();
        if n == 0 { return; }

        // Первый split — горизонтальный (лево/право)
        let rects = dwindle_rects(geo, n, true);

        for (window, rect) in visible.iter().zip(rects.iter()) {
            self.resize_window(window, *rect);
        }
    }

    pub fn apply_monocle_layout(&mut self) {
        let output = match self.space.outputs().next() {
            Some(o) => o.clone(),
            None => return,
        };
        let geo = match self.space.output_geometry(&output) {
            Some(g) => g,
            None => return,
        };
        let current_tags = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows
            .iter()
            .filter(|tw| tw.tags & current_tags != 0 && !tw.floating)
            .map(|tw| tw.window.clone())
            .collect();
        for window in &visible {
            self.resize_window(window, geo);
        }
    }

    pub fn resize_window(&mut self, window: &Window, rect: Rectangle<i32, Logical>) {
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.size = Some(rect.size);
                state.states.set(xdg_toplevel::State::TiledLeft);
                state.states.set(xdg_toplevel::State::TiledRight);
                state.states.set(xdg_toplevel::State::TiledTop);
                state.states.set(xdg_toplevel::State::TiledBottom);
            });
            toplevel.send_pending_configure();
        }
        self.space.map_element(window.clone(), rect.loc, false);
        if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
            tw.window.toplevel().zip(window.toplevel())
                .map(|(a, b)| a.wl_surface() == b.wl_surface())
                .unwrap_or(false)
        }) {
            tw.position = rect.loc;
        }
    }

    pub fn set_layout(&mut self, layout: Layout) {
        self.tile_config.prev_layout = self.tile_config.layout;
        self.tile_config.layout = layout;
        self.arrange();
        tracing::info!("dawn: layout → {}", layout.symbol());
    }

    pub fn toggle_layout(&mut self) {
        let prev = self.tile_config.prev_layout;
        self.set_layout(prev);
    }

    pub fn inc_nmaster(&mut self, delta: i32) {
        let n = self.tile_config.nmaster as i32 + delta;
        self.tile_config.nmaster = n.max(0) as usize;
        self.arrange();
    }

    pub fn set_mfact(&mut self, delta: f32) {
        let new = (self.tile_config.mfact + delta).clamp(0.1, 0.9);
        self.tile_config.mfact = new;
        self.arrange();
    }

    pub fn toggle_floating(&mut self) {
        if let Some(focused) = self.seat.get_keyboard().and_then(|kb| kb.current_focus()) {
            if let Some(tw) = self.tagged_windows.iter_mut().find(|tw| {
                tw.window.toplevel()
                    .map(|t| t.wl_surface() == &focused)
                    .unwrap_or(false)
            }) {
                tw.floating = !tw.floating;
            }
        }
        self.arrange();
    }

    pub fn focus_stack(&mut self, direction: i32) {
        let current_tags = self.viewport.current_tags();
        let visible: Vec<Window> = self.tagged_windows
            .iter()
            .filter(|tw| tw.tags & current_tags != 0)
            .map(|tw| tw.window.clone())
            .collect();
        if visible.is_empty() { return; }

        let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        let current_idx = focused.as_ref().and_then(|fs| {
            visible.iter().position(|w| {
                w.toplevel().map(|t| t.wl_surface() == fs).unwrap_or(false)
            })
        });
        let next_idx = match current_idx {
            Some(idx) => if direction > 0 { (idx + 1) % visible.len() }
                         else { (idx + visible.len() - 1) % visible.len() },
            None => 0,
        };
        let next = &visible[next_idx];
        let serial = smithay::utils::SERIAL_COUNTER.next_serial();
        self.space.raise_element(next, true);
        next.set_activated(true);
        for w in self.space.elements() {
            if w.toplevel().zip(next.toplevel())
                .map(|(a, b)| a.wl_surface() != b.wl_surface())
                .unwrap_or(true)
            {
                w.set_activated(false);
                if let Some(t) = w.toplevel() { t.send_pending_configure(); }
            }
        }
        if let Some(t) = next.toplevel() {
            self.seat.get_keyboard().unwrap()
                .set_focus(self, Some(t.wl_surface().clone()), serial);
            t.send_pending_configure();
        }
    }

    pub fn zoom(&mut self) {
        let current_tags = self.viewport.current_tags();
        let focused = self.seat.get_keyboard().and_then(|kb| kb.current_focus());
        if let Some(fs) = focused {
            let idx = self.tagged_windows.iter().position(|tw| {
                tw.tags & current_tags != 0 && !tw.floating
                    && tw.window.toplevel().map(|t| t.wl_surface() == &fs).unwrap_or(false)
            });
            if let Some(idx) = idx {
                if idx != 0 {
                    self.tagged_windows.swap(0, idx);
                    self.arrange();
                }
            }
        }
    }
}
