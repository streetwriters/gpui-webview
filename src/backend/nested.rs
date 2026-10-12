//! Linux: the in-process nested Wayland compositor (EMBEDS.md §10.3).
//!
//! GPUI stays a **native Wayland/X11 client**; only the inline web surface
//! needs a different path. This backend serves a **private Wayland socket**
//! from this process (built on **smithay**, the chosen library), and every live
//! surface's GTK window — with its wry/WebKitGTK webview — becomes an
//! `xdg_toplevel` *on our compositor*. Committed `wl_shm` buffers are copied
//! into a GPUI image the app paints at the block's rect, so clipping, scrolling
//! and overlays are ordinary GPUI painting: none of the native-child-view
//! z-order or clipping problems apply.
//!
//! Why not advertise `linux-dmabuf`: zero-copy buffers need external-texture
//! import in GPUI's renderer (the `dma-buf` feature, EMBEDS.md §14) plus a
//! renderer patch. Without it clients fall back to `wl_shm` — one CPU copy per
//! commit, fine for a clip or a page, marginal for 60 fps video. When the
//! renderer feature lands, advertising `linux-dmabuf` v4 with DRM-device
//! feedback is the fast path.
//!
//! Current limits, all tracked in TODO.md:
//! - no `wl_data_device` yet (clipboard/drag between the page and the app);
//! - key *releases* are synthesized per keystroke (GPUI's keystroke interceptor
//!   only delivers key-down), so holding a modifier across keys is approximate;
//! - `linux-dmabuf`, popup placement (`xdg_popup` arrives but is not placed),
//!   and IME (`zwp_text_input_v3`) are future work.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use gpui_kit::Window;
use smithay::delegate_compositor;
use smithay::delegate_output;
use smithay::delegate_seat;
use smithay::delegate_shm;
use smithay::delegate_xdg_shell;
use smithay::backend::input::{Axis, ButtonState, KeyState};
use smithay::input::keyboard::{KeyboardHandle, XkbConfig};
use smithay::input::pointer::{AxisFrame, ButtonEvent, MotionEvent, PointerHandle};
use smithay::input::{SeatHandler, SeatState};
use smithay::output::{Output, PhysicalProperties};
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_shm;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::{Client, Display, ListeningSocket};
use smithay::utils::{Size, Point as SPoint, SERIAL_COUNTER, Serial};
use smithay::wayland::buffer::BufferHandler;
use smithay::wayland::compositor::{
    BufferAssignment, CompositorClientState, CompositorHandler, CompositorState,
    SurfaceAttributes,
};
use smithay::wayland::output::{OutputHandler, OutputManagerState};
use smithay::wayland::shell::xdg::{
    PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
};
use smithay::wayland::shm::{self, ShmHandler, ShmState};

use crate::host::WebViewBackend;
use crate::spec::{
    Error, Placement, Presentation, Result, Source, SurfaceFrame, SurfaceId, SurfaceSpec,
    WebInputEvent, WebViewEvent,
};

/// The backend. One per window.
pub struct NestedBackend {
    display: Display<NestedState>,
    /// The private socket GTK connects to (`WAYLAND_DISPLAY`).
    socket: ListeningSocket,
    state: NestedState,
    /// The GTK window + webview per surface.
    surfaces: HashMap<SurfaceId, NestedSurface>,
    gtk_ready: bool,
}

/// A live GTK/webview client window on our compositor.
struct NestedSurface {
    window: gtk::Window,
    webview: wry::WebView,
}

/// The compositor state: the core globals plus per-surface bookkeeping.
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
    #[allow(dead_code)]
    output: Output,
    /// Created but not yet mapped by the client, in creation order: a new
    /// toplevel has no other identity, and only our own GTK windows connect.
    pending: VecDeque<SurfaceId>,
    /// The size to configure each not-yet-mapped toplevel to.
    requested: HashMap<SurfaceId, (u32, u32)>,
    /// Mapped toplevels by surface key.
    mapped: HashMap<SurfaceId, WlSurface>,
    /// The keyboard focus key, if any.
    focus: Option<SurfaceId>,
    /// Frames committed since the app last read them, keyed by surface key.
    frames: HashMap<SurfaceId, SurfaceFrame>,
    /// Events for the manager (title changes, load failures).
    events: Vec<WebViewEvent>,
    /// A monotonic clock for the millisecond timestamps the protocol wants.
    epoch: std::time::Instant,
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
    /// Starts the compositor and binds the private socket. GTK initializes
    /// lazily on the first surface, once the socket exists and is in
    /// `WAYLAND_DISPLAY`.
    pub fn new() -> Result<Self> {
        let display = Display::<NestedState>::new()
            .map_err(|error| Error::Platform(format!("wayland display: {error}")))?;
        // A private socket per process; `bind_auto` walks `-0`, `-1`, … so two
        // windows (or two app instances) never collide.
        let socket = ListeningSocket::bind_auto("gpui-webview", 0..32)
            .map_err(|error| Error::Platform(format!("wayland socket: {error}")))?;

        let handle = display.handle();
        let compositor_state = CompositorState::new::<NestedState>(&handle);
        let shm_state = ShmState::new::<NestedState>(&handle, vec![wl_shm::Format::Argb8888]);
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
        let mut seat_state = SeatState::<NestedState>::new();
        let mut seat = seat_state.new_wl_seat(&handle, "gpui-webview");
        let pointer = seat.add_pointer();
        let keyboard = seat
            .add_keyboard(XkbConfig::default(), 500, 25)
            .map_err(|error| Error::Platform(format!("xkb keymap: {error}")))?;

        let state = NestedState {
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
            focus: None,
            frames: HashMap::new(),
            events: Vec::new(),
            epoch: std::time::Instant::now(),
        };

        Ok(Self {
            display,
            socket,
            state,
            surfaces: HashMap::new(),
            gtk_ready: false,
        })
    }

    /// Accepts pending client connections, dispatches the compositor and pumps
    /// GTK. Called from every backend entry point, so the socket never stalls
    /// between frames.
    fn pump(&mut self) {
        while let Ok(Some(stream)) = self.socket.accept() {
            let _ = stream.set_nonblocking(true);
            let mut handle = self.display.handle();
            if let Err(error) = handle.insert_client(stream, Arc::new(WebClientData::new())) {
                eprintln!("gpui-webview: inserting client failed: {error}");
            }
        }
        let _ = self.display.dispatch_clients(&mut self.state);
        let _ = self.display.flush_clients();

        // GTK/WebKit need their main context pumped on this thread.
        if self.gtk_ready {
            while gtk::events_pending() {
                gtk::main_iteration_do(false);
            }
        }
    }

    /// Points GDK at our private socket and initializes GTK (once).
    fn ensure_gtk(&mut self) -> Result<()> {
        if self.gtk_ready {
            return Ok(());
        }
        let Some(name) = self.socket.socket_name() else {
            return Err(Error::Platform("wayland socket has no name".into()));
        };
        // GDK reads these before its first connection. The backend is created
        // on the main thread before any GTK use in the process, and GPUI's own
        // Wayland connection was opened earlier, so this only redirects GTK.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", name);
            std::env::set_var("GDK_BACKEND", "wayland");
        }
        gtk::init().map_err(|error| Error::Platform(format!("gtk init: {error}")))?;
        self.gtk_ready = true;
        Ok(())
    }

    /// The millisecond timestamp the protocol expects (monotonic).
    fn now_ms(&self) -> u32 {
        self.state.epoch.elapsed().as_millis() as u32
    }

    fn serial(&self) -> Serial {
        SERIAL_COUNTER.next_serial()
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
        // `WKWebpagePreferences` path.
        if !spec.policy.scripts {
            use wry::WebViewExtUnix as _;
            // Fully qualified: GTK's `WidgetExt::settings` (style settings)
            // would otherwise win the method resolution on `&self`.
            let view = webview.webview();
            if let Some(settings) = webkit2gtk::WebViewExt::settings(&view) {
                webkit2gtk::SettingsExt::set_enable_javascript(&settings, false);
            }
        }
        window.show_all();

        self.surfaces.insert(id, NestedSurface { window, webview });
        self.state.pending.push_back(id);
        self.state.requested.insert(id, (width as u32, height as u32));
        // Run one iteration so the client connects, maps and configures before
        // the caller's next frame.
        self.pump();
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
        let serial = self.serial();
        // The handle is cloned so it can be borrowed alongside `&mut state`.
        let keyboard = self.state.keyboard.clone();
        if focused {
            let surface = self.state.mapped.get(&id).cloned();
            if let Some(surface) = surface {
                keyboard.set_focus(&mut self.state, Some(surface), serial);
                self.state.focus = Some(id);
            }
        } else if self.state.focus == Some(id) {
            keyboard.set_focus(&mut self.state, None, serial);
            self.state.focus = None;
        }
        self.pump();
    }

    fn present(&mut self, _id: SurfaceId, mode: Presentation) -> Result<()> {
        if mode == Presentation::Inline {
            return Ok(());
        }
        Err(Error::Unsupported)
    }

    fn destroy(&mut self, id: SurfaceId) {
        use gtk::prelude::*;
        if self.state.focus == Some(id) {
            let serial = self.serial();
            let keyboard = self.state.keyboard.clone();
            keyboard.set_focus(&mut self.state, None, serial);
            self.state.focus = None;
        }
        self.state.mapped.remove(&id);
        self.state.frames.remove(&id);
        self.state.requested.remove(&id);
        self.state.pending.retain(|surface| *surface != id);
        if let Some(surface) = self.surfaces.remove(&id) {
            surface.window.close();
            drop(surface.webview);
        }
        self.pump();
    }

    fn frame_image(&mut self, id: SurfaceId) -> Option<SurfaceFrame> {
        self.pump();
        self.state.frames.get(&id).cloned()
    }

    fn input(&mut self, id: SurfaceId, event: WebInputEvent) {
        let Some(surface) = self.state.mapped.get(&id).cloned() else {
            return;
        };
        let serial = self.serial();
        let time = self.now_ms();
        // The handles are cloned so they can be borrowed alongside `&mut state`.
        let pointer = self.state.pointer.clone();
        match event {
            WebInputEvent::PointerMotion { x, y } => {
                pointer.motion(
                    &mut self.state,
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
                    &mut self.state,
                    Some((surface, SPoint::from((x, y)))),
                    &MotionEvent {
                        location: SPoint::from((x, y)),
                        serial,
                        time,
                    },
                );
                pointer.button(
                    &mut self.state,
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
                    &mut self.state,
                    Some((surface, SPoint::from((x, y)))),
                    &MotionEvent {
                        location: SPoint::from((x, y)),
                        serial,
                        time,
                    },
                );
                pointer.axis(&mut self.state, frame);
                pointer.frame(&mut self.state);
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
                let keyboard = self.state.keyboard.clone();
                for (code, state) in sequence {
                    keyboard.input::<(), _>(
                        &mut self.state,
                        xkb_keycode(code),
                        state,
                        serial,
                        time,
                        |_, _, _| smithay::input::keyboard::FilterResult::Forward,
                    );
                }
            }
        }
        self.pump();
    }

    fn is_focused(&self, id: SurfaceId) -> bool {
        self.state.focus == Some(id)
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        std::mem::take(&mut self.state.events)
    }

    fn supports_local(&self) -> bool {
        false
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

// -- smithay handlers --------------------------------------------------------

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
            return;
        };

        // The committed buffer: `wl_shm` is the only format we advertise, so
        // anything else is ignored and the previous frame stays.
        let assignment = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<std::cell::RefCell<SurfaceAttributes>>()
                .and_then(|attrs| attrs.borrow_mut().buffer.take())
        });
        match assignment {
            Some(BufferAssignment::NewBuffer(buffer)) => {
                // `wl_shm` is the only format advertised. Once the pixels are
                // copied the buffer is free for the client to reuse, which is
                // what keeps WebKit's shm pool flowing (we never sample it
                // again).
                if let Some(frame) = copy_shm_buffer(&buffer) {
                    self.frames.insert(id, frame);
                }
                buffer.release();
            }
            Some(BufferAssignment::Removed) => {
                self.frames.remove(&id);
            }
            None => {}
        }

        // Release the frame callbacks, which is what paces WebKit at our paint
        // rate: the client draws the next frame only after this.
        let callbacks = smithay::wayland::compositor::with_states(surface, |states| {
            states
                .data_map
                .get::<std::cell::RefCell<SurfaceAttributes>>()
                .map(|attrs| std::mem::take(&mut attrs.borrow_mut().frame_callbacks))
                .unwrap_or_default()
        });
        let time = self.epoch.elapsed().as_millis() as u32;
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
            self.frames.remove(&id);
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
        surface.with_pending_state(|state| {
            state.states.set(
                smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel::State::Activated,
            );
            if let Some((width, height)) = self.requested.get(&id).copied() {
                state.size = Some(Size::from((width as i32, height as i32)));
            }
        });
        surface.send_configure();
        self.mapped.insert(id, surface.wl_surface().clone());
    }

    fn reposition_request(
        &mut self,
        _surface: PopupSurface,
        _positioner: PositionerState,
        _token: u32,
    ) {
    }

    fn new_popup(&mut self, _surface: PopupSurface, _positioner: PositionerState) {
        // A `<select>` menu or a context menu opened by the page. Placing it
        // into the block's GPUI surface is future work (EMBEDS.md §10.3 lists
        // popups among the things we draw ourselves).
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

/// Copies a committed `wl_shm` buffer into a GPUI frame image. `ARGB8888`
/// bytes are BGRA in little-endian memory, which is exactly what
/// `RenderImage` expects, so the rows copy through unchanged.
fn copy_shm_buffer(buffer: &WlBuffer) -> Option<SurfaceFrame> {
    let mut out: Option<SurfaceFrame> = None;
    let _ = shm::with_buffer_contents(buffer, |pointer, length, data| {
        out = copy_pixels(pointer, length, data);
    });
    out
}

/// Copies one buffer's rows into a `RenderImage`. Returns `None` for a
/// malformed size/stride or a truncated memory map.
fn copy_pixels(
    pointer: *const u8,
    length: usize,
    data: smithay::wayland::shm::BufferData,
) -> Option<SurfaceFrame> {
    let (width, height, stride) = (
        data.width.max(1) as usize,
        data.height.max(1) as usize,
        data.stride.max(0) as usize,
    );
    if stride < width * 4 {
        return None;
    }
    let mut pixels = Vec::with_capacity(width * height * 4);
    for row in 0..height {
        let start = data.offset.max(0) as usize + row * stride;
        let end = start + width * 4;
        if end > length {
            return None;
        }
        // SAFETY: the caller (the `with_buffer_contents` callback) guarantees
        // `pointer` is valid for `length` bytes while it runs, and the range
        // was checked above.
        let row_bytes = unsafe { std::slice::from_raw_parts(pointer.add(start), width * 4) };
        pixels.extend_from_slice(row_bytes);
    }
    let image = image::RgbaImage::from_raw(width as u32, height as u32, pixels)?;
    Some(SurfaceFrame {
        image: Arc::new(gpui_kit::RenderImage::new(vec![image::Frame::new(image)])),
        size: (width as u32, height as u32),
    })
}
