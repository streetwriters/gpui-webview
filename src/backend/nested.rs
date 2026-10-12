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
//! through a command channel and three shared maps (frames, events, focus).
//!
//! This is not an optimisation, it is required: `gtk_init` connects to our
//! socket and blocks on a `wl_registry` roundtrip, which only a *dispatching*
//! compositor can answer. On one thread that is an immediate deadlock — the
//! headless E2E test (`tests/nested_e2e.rs`) caught exactly this. The socket is
//! bound before the thread starts, so clients may connect at any time.
//!
//! ## Buffers
//!
//! Only `wl_shm` is advertised, at one CPU copy per commit: fine for a clip or
//! a page, marginal for 60 fps video. `linux-dmabuf` needs external-texture
//! import in GPUI's renderer (the `dma-buf` feature, EMBEDS.md §14) plus a
//! renderer patch; when that lands it becomes the fast path (v4 with
//! DRM-device feedback). Frame callbacks are released as soon as a frame is
//! copied, which paces WebKit at our paint rate.
//!
//! ## Current limits (all in TODO.md)
//!
//! - no `wl_data_device` yet (clipboard/drag between the page and the app);
//! - key *releases* are synthesized per keystroke (GPUI's interceptor only
//!   delivers key-down), so holding a modifier across keys is approximate;
//! - popups (`xdg_popup`) arrive but are not placed; IME
//!   (`zwp_text_input_v3`) is not bridged;
//! - the compositor thread polls every few milliseconds; the design's calloop
//!   integration would let it block on the socket instead.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use gpui_kit::Window;
use smithay::backend::input::{Axis, ButtonState, KeyState};
use smithay::delegate_compositor;
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
use smithay::utils::{Point as SPoint, Size, Transform, SERIAL_COUNTER, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState, SurfaceAttributes,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, SurfaceCachedState, ToplevelSurface, XdgShellHandler,
    XdgShellState,
};
use smithay::wayland::shm::{self, ShmHandler, ShmState};

use crate::host::WebViewBackend;
use crate::spec::{
    Error, Placement, Presentation, Result, Source, SurfaceFrame, SurfaceId, SurfaceSpec,
    WebInputEvent, WebViewEvent,
};

/// Committed frames, written by the compositor thread and read by the app each
/// paint (and each `frame_image`).
type SharedFrames = Arc<Mutex<HashMap<SurfaceId, SurfaceFrame>>>;
/// Backend events for the manager.
type SharedEvents = Arc<Mutex<Vec<WebViewEvent>>>;
/// The keyboard focus key. The main thread sets it optimistically (so
/// `is_focused` is true the moment a surface is activated); the compositor is
/// authoritative for the protocol side and clears it on teardown.
type SharedFocus = Arc<Mutex<Option<SurfaceId>>>;

/// Work sent to the compositor thread. GTK is main-thread-only, so this is the
/// only crossing between the two worlds besides the shared maps.
enum Command {
    /// A GTK window exists and should be configured once its toplevel maps.
    Create {
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
    Shutdown,
}

/// A poisoned lock only means the *other* side panicked while holding it; the
/// data is a plain map, so recovering is strictly better than aborting the app.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `GPUI_WEBVIEW_DEBUG=1` traces the protocol side (connections, toplevel
/// mapping, commits, buffer formats) to stderr. It is the first thing to reach
/// for when a surface stays blank on a new machine.
fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("GPUI_WEBVIEW_DEBUG").is_some())
}

macro_rules! debug_log {
    ($($arg:tt)*) => {
        if debug_enabled() {
            eprintln!("gpui-webview: {}", format_args!($($arg)*));
        }
    };
}

/// The backend. One per window.
pub struct NestedBackend {
    /// The private socket's name; GDK is pointed at it (`WAYLAND_DISPLAY`).
    socket_name: std::ffi::OsString,
    commands: Sender<Command>,
    compositor: Option<std::thread::JoinHandle<()>>,
    frames: SharedFrames,
    events: SharedEvents,
    focus: SharedFocus,
    /// The GTK window + webview per surface (main thread only).
    surfaces: HashMap<SurfaceId, NestedSurface>,
    gtk_ready: bool,
}

/// A live GTK/webview client window on our compositor.
struct NestedSurface {
    window: gtk::Window,
    webview: wry::WebView,
}

/// The compositor state: the core globals plus per-surface bookkeeping. Lives
/// on the compositor thread only.
pub struct NestedState {
    compositor_state: CompositorState,
    shm_state: ShmState,
    xdg_shell_state: XdgShellState,
    /// Kept alive so the `zxdg_output_manager_v1` global stays advertised.
    #[allow(dead_code)]
    output_manager_state: OutputManagerState,
    seat_state: SeatState<Self>,
    pointer: PointerHandle<Self>,
    keyboard: KeyboardHandle<Self>,
    /// Kept alive so the `wl_output` global stays advertised.
    #[allow(dead_code)]
    output: Output,
    /// Created but not yet mapped by the client, in creation order: a new
    /// toplevel has no other identity, and only our own GTK windows connect.
    pending: VecDeque<SurfaceId>,
    /// The size to configure each not-yet-mapped toplevel to.
    requested: HashMap<SurfaceId, (u32, u32)>,
    /// Mapped toplevels by surface key.
    mapped: HashMap<SurfaceId, WlSurface>,
    focus: SharedFocus,
    frames: SharedFrames,
    /// A monotonic clock for the millisecond timestamps the protocol wants.
    epoch: std::time::Instant,
}

impl NestedState {
    /// The millisecond timestamp the protocol expects (monotonic).
    fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }
}

/// Per-client compositor state (smithay's protocol bookkeeping).
#[derive(Debug)]
struct WebClientData {
    compositor: CompositorClientState,
}

impl WebClientData {
    fn new() -> Self {
        Self {
            compositor: CompositorClientState::default(),
        }
    }
}

impl smithay::reexports::wayland_server::backend::ClientData for WebClientData {}

impl NestedBackend {
    /// Binds the private socket and starts the compositor thread. GTK
    /// initializes lazily on the first surface, once the socket exists and is
    /// in `WAYLAND_DISPLAY`.
    pub fn new() -> Result<Self> {
        let socket = ListeningSocket::bind_auto("gpui-webview", 0..32)
            .map_err(|error| Error::Platform(format!("wayland socket: {error}")))?;
        let socket_name = socket
            .socket_name()
            .map(|name| name.to_os_string())
            .ok_or_else(|| Error::Platform("wayland socket has no name".into()))?;

        let (commands, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let frames: SharedFrames = Arc::new(Mutex::new(HashMap::new()));
        let events: SharedEvents = Arc::new(Mutex::new(Vec::new()));
        let focus: SharedFocus = Arc::new(Mutex::new(None));
        let compositor = std::thread::Builder::new()
            .name("gpui-webview-compositor".into())
            .spawn({
                let frames = frames.clone();
                let focus = focus.clone();
                move || compositor_main(socket, receiver, frames, focus, ready_tx)
            })
            .map_err(|error| Error::Platform(format!("compositor thread: {error}")))?;

        // Wait for the globals to exist before any client may connect: a
        // failed start must be an error here, not a GTK init that hangs on a
        // socket nobody serves.
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return Err(Error::Platform(message)),
            Err(_) => return Err(Error::Platform("compositor thread did not start".into())),
        }

        Ok(Self {
            socket_name,
            commands,
            compositor: Some(compositor),
            frames,
            events,
            focus,
            surfaces: HashMap::new(),
            gtk_ready: false,
        })
    }

    /// Points GDK at our private socket and initializes GTK (once, on this
    /// thread — the main thread).
    fn ensure_gtk(&mut self) -> Result<()> {
        if self.gtk_ready {
            return Ok(());
        }
        // GDK reads these before its first connection. The backend is created
        // on the main thread before any GTK use in the process, and GPUI's own
        // Wayland connection was opened earlier, so this only redirects GTK.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &self.socket_name);
            std::env::set_var("GDK_BACKEND", "wayland");
        }
        gtk::init().map_err(|error| Error::Platform(format!("gtk init: {error}")))?;
        self.gtk_ready = true;
        Ok(())
    }

    /// Runs pending GTK iterations. Called from every entry point that the
    /// paint loop already hits, so the UI process (and thus WebKit's IPC)
    /// keeps moving between frames.
    fn pump_gtk(&mut self) {
        if self.gtk_ready {
            // Drain everything GLib has ready. `events_pending` alone is not
            // enough: GTK's frame clock and WebKit's IPC run through
            // idle/timeout sources, not just GDK events, so a loop gated on
            // events left the page unpainted. Bounded, in case a source keeps
            // re-arming itself.
            for _ in 0..1024 {
                if !gtk::main_iteration_do(false) {
                    break;
                }
            }
        }
    }
}

impl Drop for NestedBackend {
    fn drop(&mut self) {
        // Close the GTK windows first so their clients disconnect cleanly
        // before the compositor goes away.
        use gtk::prelude::*;
        for (_, surface) in self.surfaces.drain() {
            surface.window.close();
            drop(surface.webview);
        }
        self.pump_gtk();
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.compositor.take() {
            let _ = thread.join();
        }
    }
}

impl WebViewBackend for NestedBackend {
    fn name(&self) -> &'static str {
        "nested"
    }

    fn attach(&mut self, _window: &Window) -> Result<()> {
        // The compositor socket is already up; GTK attaches with the first
        // surface.
        Ok(())
    }

    fn create(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        self.ensure_gtk()?;
        use gtk::prelude::*;
        let (width, height) = (spec.size.0.max(1) as i32, spec.size.1.max(1) as i32);

        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title(&format!("gpui-webview-{id}"));
        window.set_default_size(width, height);
        // No client-side decorations: the block shows the page, never GTK's
        // titlebar or its decoration shadow.
        window.set_decorated(false);
        window.set_resizable(false);
        let fixed = gtk::Fixed::new();
        fixed.set_size_request(width, height);
        window.add(&fixed);

        use wry::WebViewBuilderExtUnix as _;
        let builder = wry::WebViewBuilder::new()
            .with_bounds(wry::Rect {
                position: wry::dpi::LogicalPosition::new(0, 0).into(),
                size: wry::dpi::LogicalSize::new(width, height).into(),
            })
            .with_visible(true)
            .with_focused(false);
        let builder = match &spec.source {
            Source::Url(url) => builder.with_url(url.clone()),
            Source::Html { html, .. } => builder.with_html(html.clone()),
            Source::Local { .. } => {
                return Err(Error::Platform(
                    "local sources need a WebKitGTK URI-scheme handler (phase 4)".into(),
                ));
            }
        };
        let webview = builder
            .build_gtk(&fixed)
            .map_err(|error| Error::Platform(format!("webkitgtk: {error}")))?;
        // WebKitGTK has no builder-level script switch in wry 0.57; apply it
        // through the settings, the counterpart of the macOS
        // `WKWebpagePreferences` path. Fully qualified, because GTK's
        // `WidgetExt::settings` (style settings) would win method resolution.
        if !spec.policy.scripts {
            use wry::WebViewExtUnix as _;
            let view = webview.webview();
            if let Some(settings) = webkit2gtk::WebViewExt::settings(&view) {
                webkit2gtk::SettingsExt::set_enable_javascript(&settings, false);
            }
        }
        window.show_all();

        self.surfaces.insert(id, NestedSurface { window, webview });
        let _ = self.commands.send(Command::Create {
            id,
            width: width as u32,
            height: height as u32,
        });
        self.pump_gtk();
        Ok(())
    }

    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return self.create(id, spec);
        };
        match &spec.source {
            Source::Url(url) => surface
                .webview
                .load_url(url)
                .map_err(|error| Error::Load(error.to_string())),
            Source::Html { html, .. } => surface
                .webview
                .load_html(html)
                .map_err(|error| Error::Load(error.to_string())),
            Source::Local { .. } => Err(Error::Platform(
                "local sources need a WebKitGTK URI-scheme handler (phase 4)".into(),
            )),
        }
    }

    fn place(&mut self, _id: SurfaceId, _at: Placement) {
        // The app paints the committed frame itself; the client keeps the
        // configured size, so scrolling never relayouts the page.
    }

    fn set_focus(&mut self, id: SurfaceId, focused: bool) {
        // Optimistic, so the host's very next keystroke is routed correctly;
        // the compositor applies the protocol side asynchronously.
        *lock(&self.focus) = if focused { Some(id) } else { None };
        let _ = self.commands.send(Command::SetFocus { id, focused });
    }

    fn present(&mut self, _id: SurfaceId, mode: Presentation) -> Result<()> {
        if mode == Presentation::Inline {
            return Ok(());
        }
        Err(Error::Unsupported)
    }

    fn destroy(&mut self, id: SurfaceId) {
        use gtk::prelude::*;
        if let Some(surface) = self.surfaces.remove(&id) {
            surface.window.close();
            drop(surface.webview);
        }
        lock(&self.frames).remove(&id);
        {
            let mut focus = lock(&self.focus);
            if *focus == Some(id) {
                *focus = None;
            }
        }
        let _ = self.commands.send(Command::Destroy { id });
        self.pump_gtk();
    }

    fn frame_image(&mut self, id: SurfaceId) -> Option<SurfaceFrame> {
        self.pump_gtk();
        lock(&self.frames).get(&id).cloned()
    }

    fn input(&mut self, id: SurfaceId, event: WebInputEvent) {
        let _ = self.commands.send(Command::Input { id, event });
    }

    fn is_focused(&self, id: SurfaceId) -> bool {
        *lock(&self.focus) == Some(id)
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        std::mem::take(&mut *lock(&self.events))
    }

    fn supports_local(&self) -> bool {
        false
    }
}

/// The compositor thread: owns the `Display`, accepts clients, applies the
/// commands and publishes committed frames. Never touches GTK.
fn compositor_main(
    socket: ListeningSocket,
    commands: Receiver<Command>,
    frames: SharedFrames,
    focus: SharedFocus,
    ready: Sender<std::result::Result<(), String>>,
) {
    let mut display = match Display::<NestedState>::new() {
        Ok(display) => display,
        Err(error) => {
            let _ = ready.send(Err(format!("wayland display: {error}")));
            return;
        }
    };
    let handle = display.handle();
    let compositor_state = CompositorState::new::<NestedState>(&handle);
    // Both 32-bit formats: GDK's SHM path allocates XRGB8888 for opaque
    // windows and ARGB8888 for translucent ones, and refuses to paint if its
    // preferred format was not advertised. (Missing XRGB8888 was why GTK
    // committed state without ever attaching a buffer.)
    let shm_state = ShmState::new::<NestedState>(
        &handle,
        vec![wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888],
    );
    let xdg_shell_state = XdgShellState::new::<NestedState>(&handle);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<NestedState>(&handle);
    let output = Output::new(
        "gpui-webview".to_string(),
        PhysicalProperties {
            size: Size::from((1920, 1080)),
            subpixel: smithay::output::Subpixel::Unknown,
            make: "Notesnook".to_string(),
            model: "gpui-webview".to_string(),
        },
    );
    // First (and only) output: without `wl_output` GTK has no monitor at all
    // and floods `gdk_monitor_get_scale_factor` criticals; WebKit wants a
    // screen size and scale too. One mode at 1x, 60 Hz.
    output.change_current_state(
        Some(Mode {
            size: Size::from((1920, 1080)),
            refresh: 60_000,
        }),
        Some(Transform::Normal),
        Some(Scale::Integer(1)),
        None,
    );
    output.set_preferred(Mode {
        size: Size::from((1920, 1080)),
        refresh: 60_000,
    });
    let _output_global = output.create_global::<NestedState>(&handle);

    let mut seat_state = SeatState::<NestedState>::new();
    let mut seat = seat_state.new_wl_seat(&handle, "gpui-webview");
    let pointer = seat.add_pointer();
    let keyboard = match seat.add_keyboard(XkbConfig::default(), 500, 25) {
        Ok(keyboard) => keyboard,
        Err(error) => {
            let _ = ready.send(Err(format!("xkb keymap: {error}")));
            return;
        }
    };

    let mut state = NestedState {
        compositor_state,
        shm_state,
        xdg_shell_state,
        output_manager_state,
        seat_state,
        pointer,
        keyboard,
        output,
        pending: VecDeque::new(),
        requested: HashMap::new(),
        mapped: HashMap::new(),
        focus,
        frames,
        epoch: std::time::Instant::now(),
    };
    let _ = ready.send(Ok(()));

    loop {
        // Commands from the main thread (surface lifecycle and input).
        loop {
            match commands.try_recv() {
                Ok(Command::Create { id, width, height }) => {
                    state.requested.insert(id, (width, height));
                    state.pending.push_back(id);
                }
                Ok(Command::Destroy { id }) => {
                    state.pending.retain(|pending| *pending != id);
                    state.requested.remove(&id);
                    let was_focused = {
                        let mut focused = lock(&state.focus);
                        let was = *focused == Some(id);
                        if was {
                            *focused = None;
                        }
                        was
                    };
                    if was_focused {
                        // Hand the keyboard back before the surface goes away.
                        let keyboard = state.keyboard.clone();
                        keyboard.set_focus(&mut state, None, SERIAL_COUNTER.next_serial());
                    }
                    if let Some(surface) = state.mapped.remove(&id) {
                        state.output.leave(&surface);
                    }
                    lock(&state.frames).remove(&id);
                }
                Ok(Command::SetFocus { id, focused }) => {
                    let serial = SERIAL_COUNTER.next_serial();
                    let keyboard = state.keyboard.clone();
                    if focused {
                        if let Some(surface) = state.mapped.get(&id).cloned() {
                            keyboard.set_focus(&mut state, Some(surface), serial);
                        }
                    } else {
                        let was = *lock(&state.focus) == Some(id);
                        if was {
                            keyboard.set_focus(&mut state, None, serial);
                        }
                    }
                    let mut target = lock(&state.focus);
                    if focused {
                        *target = Some(id);
                    } else if *target == Some(id) {
                        *target = None;
                    }
                }
                Ok(Command::Input { id, event }) => apply_input(&mut state, id, event),
                Ok(Command::Shutdown) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => break,
            }
        }

        // New client connections. The socket was bound before this thread
        // started, so a client may already be waiting.
        while let Ok(Some(stream)) = socket.accept() {
            let _ = stream.set_nonblocking(true);
            let mut display_handle = display.handle();
            match display_handle.insert_client(stream, Arc::new(WebClientData::new())) {
                Ok(client) => debug_log!("client connected: {:?}", client.id()),
                Err(error) => eprintln!("gpui-webview: inserting client failed: {error}"),
            }
        }

        if let Err(error) = display.dispatch_clients(&mut state) {
            eprintln!("gpui-webview: dispatch failed: {error}");
        }
        let _ = display.flush_clients();

        // The design's calloop integration would block on the socket here;
        // until then, a short poll keeps latency low while idle cost is a few
        // cheap syscalls per tick.
        std::thread::sleep(Duration::from_millis(4));
    }
}

/// One input event on the compositor thread: translate GPUI input into
/// `wl_seat` events for the mapped surface.
fn apply_input(state: &mut NestedState, id: SurfaceId, event: WebInputEvent) {
    let Some(surface) = state.mapped.get(&id).cloned() else {
        return;
    };
    let serial = SERIAL_COUNTER.next_serial();
    let time = state.now_ms();
    // The handles are cloned so they can be borrowed alongside `&mut state`.
    let pointer = state.pointer.clone();
    match event {
        WebInputEvent::PointerMotion { x, y } => {
            pointer.motion(
                state,
                Some((surface, SPoint::from((x, y)))),
                &MotionEvent {
                    location: SPoint::from((x, y)),
                    serial,
                    time,
                },
            );
        }
        WebInputEvent::PointerButton {
            x,
            y,
            button,
            pressed,
        } => {
            pointer.motion(
                state,
                Some((surface, SPoint::from((x, y)))),
                &MotionEvent {
                    location: SPoint::from((x, y)),
                    serial,
                    time,
                },
            );
            pointer.button(
                state,
                &ButtonEvent {
                    serial,
                    time,
                    button,
                    state: if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                },
            );
        }
        WebInputEvent::PointerAxis { x, y, dx, dy } => {
            let mut frame = AxisFrame::new(time);
            if dy != 0. {
                frame = frame.value(Axis::Vertical, dy);
            }
            if dx != 0. {
                frame = frame.value(Axis::Horizontal, dx);
            }
            pointer.motion(
                state,
                Some((surface, SPoint::from((x, y)))),
                &MotionEvent {
                    location: SPoint::from((x, y)),
                    serial,
                    time,
                },
            );
            pointer.axis(state, frame);
            pointer.frame(state);
        }
        WebInputEvent::Key { keystroke, pressed } => {
            // GPUI delivers one keystroke per event, so modifiers are
            // synthesized around the key (press modifiers, press the key,
            // release everything). Holding a modifier across keys needs
            // key-up events; see the module docs and TODO.md.
            let Some(code) = evdev_keycode(&keystroke.key) else {
                return;
            };
            let modifiers = keystroke.modifiers;
            let mut sequence: Vec<(u32, KeyState)> = Vec::new();
            if modifiers.control {
                sequence.push((29, KeyState::Pressed)); // KEY_LEFTCTRL
            }
            if modifiers.alt {
                sequence.push((56, KeyState::Pressed)); // KEY_LEFTALT
            }
            if modifiers.shift {
                sequence.push((42, KeyState::Pressed)); // KEY_LEFTSHIFT
            }
            if modifiers.platform {
                sequence.push((125, KeyState::Pressed)); // KEY_LEFTMETA
            }
            sequence.push((
                code,
                if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
            ));
            if modifiers.platform {
                sequence.push((125, KeyState::Released));
            }
            if modifiers.shift {
                sequence.push((42, KeyState::Released));
            }
            if modifiers.alt {
                sequence.push((56, KeyState::Released));
            }
            if modifiers.control {
                sequence.push((29, KeyState::Released));
            }
            let keyboard = state.keyboard.clone();
            for (code, key_state) in sequence {
                keyboard.input::<(), _>(
                    state,
                    xkb_keycode(code),
                    key_state,
                    serial,
                    time,
                    |_, _, _| smithay::input::keyboard::FilterResult::Forward,
                );
            }
        }
    }
}

/// GPUI's evdev keycode space to xkb's (evdev + 8).
fn xkb_keycode(evdev: u32) -> smithay::input::keyboard::Keycode {
    smithay::input::keyboard::Keycode::new(evdev + 8)
}

/// The Linux input-event code for a GPUI key name. A US-layout table for the
/// keys a page needs; unknown keys are dropped rather than mistyped.
fn evdev_keycode(key: &str) -> Option<u32> {
    Some(match key {
        "a" => 30,
        "b" => 48,
        "c" => 46,
        "d" => 32,
        "e" => 18,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "i" => 23,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "m" => 50,
        "n" => 49,
        "o" => 24,
        "p" => 25,
        "q" => 16,
        "r" => 19,
        "s" => 31,
        "t" => 20,
        "u" => 22,
        "v" => 47,
        "w" => 17,
        "x" => 45,
        "y" => 21,
        "z" => 44,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        "=" | "plus" => 13,
        "-" | "minus" => 12,
        "[" => 26,
        "]" => 27,
        ";" => 39,
        "'" => 40,
        "`" => 41,
        "," => 51,
        "." => 52,
        "/" => 53,
        "\\" => 43,
        "space" => 57,
        "escape" => 1,
        "enter" | "return" => 28,
        "tab" => 15,
        "backspace" => 14,
        "delete" => 111,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "shift" => 42,
        "control" => 29,
        "alt" => 56,
        "platform" | "super" | "meta" | "cmd" => 125,
        "function" => 127,
        _ => return None,
    })
}

// -- smithay handlers (all on the compositor thread) -------------------------

impl SeatHandler for NestedState {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.seat_state
    }
}

impl CompositorHandler for NestedState {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        &client
            .get_data::<WebClientData>()
            .expect("clients are created with WebClientData")
            .compositor
    }

    fn commit(&mut self, surface: &WlSurface) {
        let Some(id) = self
            .mapped
            .iter()
            .find(|(_, mapped)| *mapped == surface)
            .map(|(id, _)| *id)
        else {
            debug_log!("commit for an unmapped surface; ignoring");
            return;
        };

        // The committed buffer: `wl_shm` is the only format we advertise, so
        // anything else is ignored and the previous frame stays. (smithay 0.7
        // keeps `SurfaceAttributes` in the surface's `cached_state`; an old
        // `data_map` lookup compiles but silently returns `None`.)
        let assignment = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceAttributes>()
                .current()
                .buffer
                .take()
        });
        // GTK draws client-side decorations (and their shadows) *around* the
        // window's content; `set_window_geometry` says where the content is,
        // and only that part belongs in the block.
        let geometry = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .cached_state
                .get::<SurfaceCachedState>()
                .current()
                .geometry
        });
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                // Once the pixels are copied the buffer is free for the client
                // to reuse, which is what keeps WebKit's shm pool flowing (we
                // never sample it again).
                match copy_shm_buffer(&buffer, geometry) {
                    Some(frame) => {
                        debug_log!(
                            "commit: {}x{} frame for block {id}",
                            frame.size.0,
                            frame.size.1
                        );
                        lock(&self.frames).insert(id, frame);
                    }
                    None => debug_log!("commit: non-shm or malformed buffer; ignored"),
                }
                buffer.release();
            }
            Some(BufferAssignment::Removed) => {
                lock(&self.frames).remove(&id);
            }
            None => debug_log!("commit: without a new buffer (state only)"),
        }

        // Release the frame callbacks, which is what paces WebKit at our paint
        // rate: the client draws the next frame only after this.
        let callbacks: Vec<smithay::reexports::wayland_server::protocol::wl_callback::WlCallback> =
            smithay::wayland::compositor::with_states(surface, |states| {
                states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .frame_callbacks
                    .drain(..)
                    .collect()
            });
        let time = self.now_ms();
        if !callbacks.is_empty() {
            debug_log!("commit: fired {} frame callbacks", callbacks.len());
        }
        for callback in callbacks {
            callback.done(time);
        }
    }

    fn destroyed(&mut self, surface: &WlSurface) {
        let stale: Vec<SurfaceId> = self
            .mapped
            .iter()
            .filter(|(_, mapped)| *mapped == surface)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            self.mapped.remove(&id);
            lock(&self.frames).remove(&id);
            let mut focused = lock(&self.focus);
            if *focused == Some(id) {
                *focused = None;
            }
        }
    }
}

impl ShmHandler for NestedState {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl BufferHandler for NestedState {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl OutputHandler for NestedState {
    fn output_bound(
        &mut self,
        _output: Output,
        _wl_output: smithay::reexports::wayland_server::protocol::wl_output::WlOutput,
    ) {
        debug_log!("client bound wl_output");
    }
}

impl XdgShellHandler for NestedState {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        let Some(id) = self.pending.pop_front() else {
            // Not one of ours (nothing else can connect to the socket, but
            // refuse politely rather than mis-mapping).
            return;
        };
        let requested = self.requested.get(&id).copied();
        surface.with_pending_state(|state| {
            state.states.set(
                smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
            );
            if let Some((width, height)) = requested {
                state.size = Some(Size::from((width as i32, height as i32)));
            }
        });
        let configured = surface.send_configure();
        // GDK's frame clock follows the output a surface is on; without
        // `wl_surface.enter` GTK kept the window mapped but never drew a frame
        // (the client-side trace showed no attach after the configure).
        self.output.enter(surface.wl_surface());
        debug_log!("new_toplevel for block {id}, requested={requested:?}, configured={configured:?}");
        self.mapped.insert(id, surface.wl_surface().clone());
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // A `<select>` menu or a context menu opened by the page. Placing it
        // into the block's GPUI surface is future work (EMBEDS.md §10.3 lists
        // popups among the things we draw ourselves).
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn grab(
        &mut self,
        _surface: PopupSurface,
        _seat: smithay::reexports::wayland_server::protocol::wl_seat::WlSeat,
        _serial: Serial,
    ) {
    }
}

delegate_compositor!(NestedState);
delegate_shm!(NestedState);
delegate_xdg_shell!(NestedState);
delegate_output!(NestedState);
delegate_seat!(NestedState);

// -- buffer copy -------------------------------------------------------------

/// Copies a committed `wl_shm` buffer into a GPUI frame image, cropped to the
/// client's window geometry (the content, without decoration shadows).
/// `ARGB8888`/`XRGB8888` bytes are BGRA in little-endian memory, which is
/// exactly what `RenderImage` expects, so the rows copy through unchanged.
fn copy_shm_buffer(
    buffer: &WlBuffer,
    geometry: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
) -> Option<SurfaceFrame> {
    let mut out: Option<SurfaceFrame> = None;
    let _ = shm::with_buffer_contents(buffer, |pointer, length, data| {
        out = copy_pixels(pointer, length, data, geometry);
    });
    out
}

/// Copies the content rectangle of one buffer into a `RenderImage`. Returns
/// `None` for a malformed size/stride or a truncated memory map.
fn copy_pixels(
    pointer: *const u8,
    length: usize,
    data: smithay::wayland::shm::BufferData,
    geometry: Option<smithay::utils::Rectangle<i32, smithay::utils::Logical>>,
) -> Option<SurfaceFrame> {
    let (buffer_width, buffer_height, stride) = (
        data.width.max(1) as usize,
        data.height.max(1) as usize,
        data.stride.max(0) as usize,
    );
    if stride < buffer_width * 4 {
        return None;
    }

    // The content rect, clamped to the buffer. Without a window geometry the
    // whole buffer is the content.
    let (x0, y0, width, height) = match geometry {
        Some(rect) if rect.size.w > 0 && rect.size.h > 0 => {
            let x0 = rect.loc.x.clamp(0, buffer_width as i32) as usize;
            let y0 = rect.loc.y.clamp(0, buffer_height as i32) as usize;
            let width = (rect.size.w as usize).min(buffer_width - x0);
            let height = (rect.size.h as usize).min(buffer_height - y0);
            (x0, y0, width, height)
        }
        _ => (0, 0, buffer_width, buffer_height),
    };
    if width == 0 || height == 0 {
        return None;
    }

    let mut pixels = Vec::with_capacity(width * height * 4);
    for row in y0..y0 + height {
        let start = data.offset.max(0) as usize + row * stride + x0 * 4;
        let end = start + width * 4;
        if end > length {
            return None;
        }
        // SAFETY: the caller (`with_buffer_contents`) guarantees `pointer` is
        // valid for `length` bytes while the callback runs, and the range was
        // checked above.
        let row_bytes = unsafe { std::slice::from_raw_parts(pointer.add(start), width * 4) };
        pixels.extend_from_slice(row_bytes);
    }
    // XRGB8888 leaves the alpha byte undefined (zero in practice); GPUI blends
    // with it, so an opaque buffer must become opaque here.
    if data.format == wl_shm::Format::Xrgb8888 {
        for alpha in pixels.iter_mut().skip(3).step_by(4) {
            *alpha = 0xff;
        }
    }
    let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
    Some(SurfaceFrame {
        image: Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(image)])),
        size: (width as u32, height as u32),
    })
}
