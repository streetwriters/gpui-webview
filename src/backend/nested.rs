//! Linux: the in-process nested Wayland compositor (EMBEDS.md §10.3).
//!
//! **Status: scaffold.** The design is settled and the library choice is
//! **smithay** (user decision, 2026-10-12); the compositor itself is the next
//! unit of work and needs a Linux box to build and run. Until it lands this
//! backend reports `Error::Unsupported`, so a Linux session degrades to the
//! poster + `openUrl` exactly like every other missing backend.
//!
//! What lands here (from §10.3), so the shape is not lost:
//!
//! * a private Wayland socket served by this process (`wl_compositor`,
//!   `xdg_shell` incl. `xdg_popup`, `wl_shm`, `linux-dmabuf` v4 with DRM-device
//!   feedback, `wl_seat` with an xkb keymap, `wl_output`, `wl_data_device`);
//! * each live surface's GTK window (wry/WebKitGTK) becomes an `xdg_toplevel`
//!   on it (`gdk_display_open("<private socket>")`);
//! * committed buffers (DMA-BUF when the `dma-buf` feature and the renderer
//!   patch are present, otherwise `wl_shm`) become an ordinary GPUI texture the
//!   app can clip and draw over;
//! * frame callbacks are released when GPUI presents, pacing WebKit at vsync.
//!
//! The GPUI-side requirements (`dma-buf` external-texture import, the
//! `wayland-surface` hook) are listed in §14 and are feature-gated here: the
//! backend must still produce a usable surface without them (the `wl_shm`
//! path), and a session where inline hosting fails falls back to a separate
//! window.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gpui_kit::Window;

use crate::host::WebViewBackend;
use crate::spec::{
    Error, Placement, Presentation, Result, SurfaceId, SurfaceSpec, WebViewEvent,
};

/// The nested-compositor backend. One per window.
pub struct NestedBackend {
    events: Arc<Mutex<Vec<WebViewEvent>>>,
    loader: Option<Arc<dyn crate::host::SchemeLoader>>,
    surfaces: HashMap<SurfaceId, ()>,
}

impl NestedBackend {
    /// Probes the session. Every failure is `Unsupported` so the caller falls
    /// back to the poster; a partial compositor must never half-run.
    pub fn new() -> Result<Self> {
        // The smithay event loop, the private socket and the GTK client
        // connection are created here once the compositor module exists.
        Err(Error::Unsupported)
    }

    /// Whether the session is Wayland at all (`WAYLAND_DISPLAY` set). The
    /// nested compositor is the preferred Linux path even under X11, since it
    /// gives clip + overlay parity there too.
    pub fn session_has_wayland() -> bool {
        std::env::var_os("WAYLAND_DISPLAY").is_some()
    }
}

impl WebViewBackend for NestedBackend {
    fn name(&self) -> &'static str {
        "nested"
    }

    fn attach(&mut self, _window: &Window) -> Result<()> {
        Err(Error::Unsupported)
    }

    fn create(&mut self, _id: SurfaceId, _spec: SurfaceSpec) -> Result<()> {
        Err(Error::Unsupported)
    }

    fn place(&mut self, _id: SurfaceId, _at: Placement) {}

    fn set_focus(&mut self, _id: SurfaceId, _focused: bool) {}

    fn present(&mut self, _id: SurfaceId, _mode: Presentation) -> Result<()> {
        Err(Error::Unsupported)
    }

    fn destroy(&mut self, _id: SurfaceId) {}

    fn set_scheme_loader(&mut self, loader: Arc<dyn crate::host::SchemeLoader>) {
        self.loader = Some(loader);
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        self.events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }
}
