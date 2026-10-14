//! Linux: the in-process nested Wayland compositor (EMBEDS.md §10.3).
//!
//! GPUI stays a **native Wayland/X11 client**; only the inline web surface
//! needs a different path. This backend serves a **private Wayland socket**
//! from this process (built on **smithay**), and every live surface's GTK
//! window — with its wry/WebKitGTK webview — becomes an `xdg_toplevel` *on our
//! compositor*. Committed `wl_shm` buffers are copied into a GPUI image the app
//! paints at the block's rect, so clipping, scrolling and overlays are ordinary
//! GPUI painting: none of the native-child-view z-order or clipping problems
//! apply.
//!
//! ## Threads
//!
//! The compositor runs on **its own thread**; GTK runs on the main thread (GTK3
//! requires the thread that called `gtk_init` for every GTK call). The two meet
//! through a command channel and four shared maps (frames, popups, events,
//! focus).
//!
//! This is not an optimisation, it is required: `gtk_init` connects to our
//! socket and blocks on a `wl_registry` roundtrip, which only a *dispatching*
//! compositor can answer. On one thread that is an immediate deadlock — the
//! headless E2E test (`tests/nested_e2e.rs`) caught exactly this. The socket is
//! bound before the thread starts, so clients may connect at any time.
//!
//! ## Buffers
//!
//! Two transports, chosen by what the renderer can do:
//!
//! - `linux-dmabuf`: when `attach()` reports importable formats (the renderer's
//!   Vulkan driver verified them), the compositor advertises exactly that set as
//!   feedback and a committed dma-buf becomes a [`SurfaceContent::Dmabuf`]
//!   frame. The app imports it into a GPU texture (no CPU copy, smooth video);
//!   the plane fds are duplicated, and the client buffer is held back one
//!   commit so its pixels cannot be rewritten under the import.
//! - `wl_shm`: the universal fallback (and what every GPU-less machine uses).
//!   Committed buffers are copied into a GPUI image at one CPU copy per commit:
//!   fine for a clip or a page, marginal for 60 fps video.
//!
//! Frame callbacks are released as soon as a commit is handled. That is *not*
//! a paint-rate brake: the client is self-paced (it keeps animating while the
//! app is idle or has lowered the surface), and the app simply paints the
//! latest committed frame on its next pass, requesting a repaint when a new
//! commit arrives.
//!
//! ## Policy
//!
//! `Policy` is enforced through WebKitGTK's settings: scripts, developer extras
//! (the "Inspect Element" item), engine fullscreen, camera/microphone + WebRTC,
//! clipboard access from the page, and page-opened windows. The engine's own
//! context menu (Back, Reload, Save Page As, Inspect…) is trimmed by handling
//! `context-menu`; a *page's* menu is page content and never reaches GTK.
//! Navigation is decided per request: `Allow` loads, `OpenExternal` refuses and
//! reports the URL, `Block` refuses silently. `Storage::Ephemeral` uses a fresh
//! incognito context per surface; `Storage::Shared` uses one `WebContext` for
//! the whole backend, so embeds share a cookie jar. Everything in
//! `Policy::engine` is off for a note; `GPUI_WEBVIEW_DEVTOOLS=1` flips devtools
//! on for local debugging.
//!
//! ## Current limits (all in TODO.md)
//!
//! - no `wl_data_device` selection bridge yet (clipboard/drag between the page
//!   and the app);
//! - IME (`zwp_text_input_v3`) is not bridged, and the client runs the default
//!   XKB keymap, so a non-US host layout cannot type its extra characters;
//! - cursor images (`wl_pointer.set_cursor`) are ignored: showing a CSS
//!   `cursor` needs a GPUI custom-cursor API the app does not have;
//! - keyboard focus stays on the toplevel while a native popup is open, so a
//!   menu sees Esc only through the client's own grab handling;
//! - touch/gesture forwarding (`wl_touch`) is not implemented;
//! - frame callbacks are fired when a commit is handled, not when the app
//!   paints: the client is self-paced (WebKit keeps animating while lowered),
//!   and the app consumes whatever the latest commit is on its next paint;
//! - the compositor thread polls every few milliseconds; the design's calloop
//!   integration would let it block on the socket instead.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use gpui_kit::Window;
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
use smithay::backend::allocator::{Buffer as _, Format, Fourcc, Modifier};
use smithay::backend::input::{Axis, ButtonState, KeyState};
use smithay::delegate_compositor;
use smithay::delegate_data_device;
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
use smithay::delegate_dmabuf;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::input::keyboard::{KeyboardHandle, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle};
use smithay::input::{SeatHandler, SeatState};
use smithay::output::{Mode, Output, PhysicalProperties, Scale};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, ListeningSocket};
use smithay::utils::{Logical, Point as SPoint, Rectangle, Size, Transform, SERIAL_COUNTER, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
};
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
use smithay::wayland::dmabuf::{
    DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier, get_dmabuf,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::data_device::{
    ClientDndGrabHandler, DataDeviceHandler, DataDeviceState, ServerDndGrabHandler,
};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgPopupSurfaceData,
    XdgShellHandler, XdgShellState, XdgToplevelSurfaceData,
};
use smithay::wayland::shm::{self, ShmHandler, ShmState};

use crate::host::WebViewBackend;
#[cfg(all(target_os = "linux", feature = "dma-buf"))]
use crate::spec::{DmaBufFrame, DmaBufPlane, SurfaceContent};
use crate::spec::{
    ContextMenu, Error, Navigation, Placement, Presentation, Result, Source, Storage,
    SurfaceFrame, SurfaceId, SurfaceSpec, WebInputEvent, WebViewEvent,
};


mod backend;
mod buffers;
mod compositor;
mod input;
mod policy;
mod popups;

pub use backend::NestedBackend;

/// Committed frames, written by the compositor thread and read by the app each
/// paint (and each `frame_image`).
type SharedFrames = Arc<Mutex<HashMap<SurfaceId, SurfaceFrame>>>;

/// The popups the app paints above the page's frame, written by the compositor
/// thread and read per paint (and each `popup_frames`).
type SharedPopups = Arc<Mutex<Vec<popups::SharedPopup>>>;

/// Backend events for the manager.
type SharedEvents = Arc<Mutex<Vec<WebViewEvent>>>;

/// The keyboard focus key. The main thread sets it optimistically (so
/// `is_focused` is true the moment a surface is activated); the compositor is
/// authoritative for the protocol side and clears it on teardown.
type SharedFocus = Arc<Mutex<Option<SurfaceId>>>;


/// Work sent to the compositor thread. GTK is main-thread-only, so this is the
/// only crossing between the two worlds besides the shared maps.
pub(super) enum Command {
    /// A GTK window exists and should be configured once its toplevel maps.
    Create {
        id: SurfaceId,
        width: u32,
        height: u32,
    },
    /// The block was resized (atom handles, a window resize): configure the
    /// live toplevel so the page re-lays out instead of being stretched.
    Resize {
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
    /// What this process's renderer can import from dma-bufs; the compositor
    /// advertises exactly this as `linux-dmabuf` feedback. Sent from
    /// `attach()`, before any client connects. Only built with `dma-buf`.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    DmabufFeedback(gpui_kit::DmaBufCapabilities),
    Shutdown,
}


/// GPUI's pixel-delta wheel units to the GDK/Wayland smooth axis units the
/// nested compositor sends. GPUI reports one X11 wheel tick as `Lines(0, -3)`
/// and `pixel_delta(px(20.))` turns that into 60 units; GDK's Wayland axis
/// carries libinput's value, where one physical click is 15 units. Measured
/// with `E2E_WHEEL_CALIBRATE=1` (a 320x200 viewport): 15 units → 51 px, 30 →
/// 102 px, 60 → 204 px (linear, 3.4 px/unit). Scaling by 15/60 makes one GPUI
/// tick move exactly what one real wheel click moves; before this, each tick
/// was four clicks.
const GDK_AXIS_UNITS_PER_GPUI_PIXEL: f64 = 0.25;


/// A poisoned lock only means the *other* side panicked while holding it; the
/// data is a plain map, so recovering is strictly better than aborting the app.
pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}
