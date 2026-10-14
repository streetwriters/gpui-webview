//! Linux: the in-process nested Wayland compositor (EMBEDS.md §10.3).
//!
//! GPUI stays a native Wayland/X11 client; this backend serves a private
//! Wayland socket (smithay) and every live surface's GTK/WebKitGTK window is
//! an `xdg_toplevel` on it. Committed buffers become frames the app paints at
//! the block's rect, so clipping, scrolling and overlays are ordinary GPUI
//! painting.
//!
//! The compositor runs on its own thread (T1); GTK on the main thread. The two
//! meet through a command channel and one shared-state struct (frames, popups,
//! events, focus). `linux-dmabuf` (feature `dma-buf`) imports frames as GPU
//! textures with a one-commit hold (T14); `wl_shm` copies pixels instead.
//! `Policy` is enforced through WebKitGTK settings; the README lists the
//! limits.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};

use crate::spec::{SurfaceFrame, SurfaceId, WebInputEvent, WebViewEvent};

mod backend;
mod buffers;
mod compositor;
mod input;
mod keymap;
mod policy;
mod popups;

pub use backend::NestedBackend;

/// The maps shared between the main thread and the compositor thread.
#[derive(Default)]
pub(super) struct Shared {
    /// Committed page frames, read per paint.
    pub(super) frames: Mutex<HashMap<SurfaceId, SurfaceFrame>>,
    /// Popup frames, bottom to top, read per paint.
    pub(super) popups: Mutex<Vec<popups::SharedPopup>>,
    /// Backend events awaiting a manager drain.
    pub(super) events: Mutex<Vec<WebViewEvent>>,
    /// Keyboard focus: the main thread sets it optimistically, the
    /// compositor is authoritative for the protocol side.
    pub(super) focus: Mutex<Option<SurfaceId>>,
    /// The commit waker the app installed (`WebViewBackend::set_waker`); the
    /// compositor calls it after publishing a frame.
    pub(super) waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// Work for the compositor thread — the only crossing besides the shared maps.
pub(super) enum Command {
    /// Configure the toplevel to this size: at create (before it maps) and on
    /// a resize (so the page re-lays out instead of being stretched).
    Configure {
        id: SurfaceId,
        width: u32,
        height: u32,
    },
    Destroy {
        id: SurfaceId,
    },
    SetFocus {
        id: SurfaceId,
        focused: bool,
    },
    Input {
        id: SurfaceId,
        event: WebInputEvent,
    },
    /// The app painted the surface: release its parked frame callbacks
    /// (present pacing).
    Presented {
        id: SurfaceId,
    },
    /// The renderer's importable formats, advertised as `linux-dmabuf` feedback.
    #[cfg(dmabuf)]
    DmabufFeedback(gpui_kit::DmaBufCapabilities),
    /// The host's active keyboard layout name; the client keymap follows it.
    KeyboardLayout(String),
    Shutdown,
}

/// T12: GPUI wheel units → libinput units (one tick is 60, one click 15; scale by 15/60).
const GDK_AXIS_UNITS_PER_GPUI_PIXEL: f64 = 0.25;

/// Recover a poisoned lock: the data is a plain map, aborting helps nobody.
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
