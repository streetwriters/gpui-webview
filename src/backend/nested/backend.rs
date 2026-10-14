//! The GTK/WebKit main-thread side: windows, webviews and the command
//! channel to the compositor thread.

use super::*;
use crate::debug::debug_log;
use super::compositor::compositor_main;
use super::policy::{apply_surface_policy, trim_context_menu, MenuState, SurfaceControl};

/// The backend. `NestedBackend` is process-wide: GDK has one display
/// connection, `WAYLAND_DISPLAY` is process-global, and its private compositor
/// serves a single Wayland socket, so a second backend must refuse instead of
/// silently producing windows that never map. (The manager is still per
/// window.)
pub struct NestedBackend {
    commands: Sender<Command>,
    compositor: Option<std::thread::JoinHandle<()>>,
    frames: SharedFrames,
    popups: SharedPopups,
    events: SharedEvents,
    focus: SharedFocus,
    /// The GTK window + webview per surface (main thread only).
    surfaces: HashMap<SurfaceId, NestedSurface>,
    /// One `WebContext` shared by every `Storage::Shared` surface, so embeds
    /// keep one cookie jar (and it persists for the backend's lifetime).
    shared_context: Option<wry::WebContext>,
    gtk_ready: bool,
    /// Set by `pump_gtk` when it drained GTK work; `wants_animation_frame`
    /// consumes it so the app schedules the next paint that picks the client's
    /// commit up. Without it, a paint that lands before WebKit has produced
    /// the frame could stall the commit → pump → paint loop.
    pump_did_work: bool,
}


/// A live GTK/webview client window on our compositor.
pub(super) struct NestedSurface {
    window: gtk::Window,
    /// The container the webview lives in; kept so a resize can drop its
    /// minimum-size request (a GTK widget never shrinks below it, which would
    /// make the client ignore the compositor's configure).
    fixed: gtk::Fixed,
    webview: wry::WebView,
    control: SurfaceControl,
    storage: Storage,
    /// The size the toplevel is configured to; `place` reconfigures on change.
    size: (u32, u32),
}


/// The one nested backend a process may own. A second one would point GDK's
/// single display connection at a different socket (its windows would never
/// map, and closing the first would cut GTK's connection), so it is refused
/// with `Error::Platform` and the caller keeps its poster.
static INSTANCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();


impl NestedBackend {
    /// Binds the private socket and starts the compositor thread. GTK
    /// initializes lazily on the first surface, once the socket exists and is
    /// in `WAYLAND_DISPLAY`.
    pub fn new() -> Result<Self> {
        Self::start()
    }

    /// Like [`Self::new`], but with the renderer's dma-buf import capabilities
    /// supplied up front instead of by [`WebViewBackend::attach`]. The headless
    /// E2E uses this (it has a GPU device but no GPUI window); apps take the
    /// `attach` path. Only built with the `dma-buf` feature.
    #[cfg(all(target_os = "linux", feature = "dma-buf"))]
    pub fn new_with_dmabuf(capabilities: Option<gpui_kit::DmaBufCapabilities>) -> Result<Self> {
        let backend = Self::start()?;
        if let Some(capabilities) = capabilities {
            let _ = backend.commands.send(Command::DmabufFeedback(capabilities));
        }
        Ok(backend)
    }

    fn start() -> Result<Self> {
        if INSTANCE.get().is_some() {
            return Err(Error::Platform(
                "a nested webview backend already runs in this process".into(),
            ));
        }
        let socket = ListeningSocket::bind_auto("gpui-webview", 0..32)
            .map_err(|error| Error::Platform(format!("wayland socket: {error}")))?;
        let socket_name = socket
            .socket_name()
            .map(|name| name.to_os_string())
            .ok_or_else(|| Error::Platform("wayland socket has no name".into()))?;

        // GDK reads `WAYLAND_DISPLAY` when it connects (the main thread, at
        // first GTK use). Set it here — before this backend spawns its
        // compositor thread and before GTK/WebKit (the only consumers of this
        // variable) exist — rather than lazily from `ensure_gtk`, when the
        // process is busier. `GDK_BACKEND` is not needed: `ensure_gtk` calls
        // `gdk::set_allowed_backends("wayland")`, which is a function call,
        // not environment state.
        //
        // SAFETY: `set_var` is only unsound while another thread reads or
        // writes the environment concurrently. This runs once, on the main
        // thread, at backend construction; the app's existing threads (GPUI's
        // event loop) do not read `WAYLAND_DISPLAY`, and GDK/WebKit have not
        // been initialized yet. This is the earliest point the design allows.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &socket_name);
        }

        let (commands, receiver) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let frames: SharedFrames = Arc::new(Mutex::new(HashMap::new()));
        let popups: SharedPopups = Arc::new(Mutex::new(Vec::new()));
        let events: SharedEvents = Arc::new(Mutex::new(Vec::new()));
        let focus: SharedFocus = Arc::new(Mutex::new(None));
        let compositor = std::thread::Builder::new()
            .name("gpui-webview-compositor".into())
            .spawn({
                let frames = frames.clone();
                let popups = popups.clone();
                let focus = focus.clone();
                move || compositor_main(socket, receiver, frames, popups, focus, ready_tx)
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

        let backend = Self {
            commands,
            compositor: Some(compositor),
            frames,
            popups,
            events,
            focus,
            surfaces: HashMap::new(),
            shared_context: None,
            gtk_ready: false,
            pump_did_work: false,
        };
        let _ = INSTANCE.set(());
        Ok(backend)
    }

    /// Points GDK at our private socket and initializes GTK (once, on this
    /// thread — the main thread).
    fn ensure_gtk(&mut self) -> Result<()> {
        if self.gtk_ready {
            return Ok(());
        }
        // The display name came from `start` (the environment, set before any
        // of our threads existed). Restricting the backend is a call, not an
        // env var.
        static ALLOWED_BACKENDS: std::sync::Once = std::sync::Once::new();
        ALLOWED_BACKENDS.call_once(|| gtk::gdk::set_allowed_backends("wayland"));
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
            let mut ran = false;
            for _ in 0..1024 {
                if !gtk::main_iteration_do(false) {
                    break;
                }
                ran = true;
            }
            self.pump_did_work |= ran;
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

    fn attach(&mut self, window: &Window) -> Result<()> {
        // The compositor socket is already up; GTK attaches with the first
        // surface. The renderer's dma-buf import capabilities go to the
        // compositor now, before any client connects, so the `linux-dmabuf`
        // global is part of the first registry roundtrip. Without support (or
        // with the feature off) the compositor stays `wl_shm`-only.
        #[cfg(all(target_os = "linux", feature = "dma-buf"))]
        {
            if let Some(capabilities) = window.dmabuf_capabilities() {
                debug_log!(
                    "renderer imports dma-bufs: node {}:{}, {} format(s)",
                    capabilities.device.0,
                    capabilities.device.1,
                    capabilities.formats.len()
                );
                let _ = self.commands.send(Command::DmabufFeedback(capabilities));
            } else {
                debug_log!("renderer has no dma-buf import support; wl_shm only");
            }
        }
        #[cfg(not(all(target_os = "linux", feature = "dma-buf")))]
        {
            let _ = window;
            debug_log!("dma-buf support is not built in (feature `dma-buf`); wl_shm only");
        }
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
        // Resizable only so GDK accepts the compositor's configure: with
        // `set_resizable(false)` GDK clamps its Wayland window to its own
        // min/max hints and silently ignores a smaller configure. Decorations
        // are off, so this only affects hints.
        window.set_resizable(true);
        let fixed = gtk::Fixed::new();
        fixed.set_size_request(width, height);
        window.add(&fixed);

        // Storage: a clip is ephemeral (a fresh incognito context, no cookies
        // survive it), an embed shares one cookie jar with every other shared
        // surface of this backend. wry ignores a supplied context under
        // `incognito`, so the two arms are exclusive.
        let mut builder = match spec.policy.storage {
            Storage::Ephemeral => wry::WebViewBuilder::new().with_incognito(true),
            Storage::Shared => {
                let context = self
                    .shared_context
                    .get_or_insert_with(|| wry::WebContext::new(None));
                wry::WebViewBuilder::new_with_web_context(context)
            }
        };

        use wry::WebViewBuilderExtUnix as _;
        builder = builder
            .with_bounds(wry::Rect {
                position: wry::dpi::LogicalPosition::new(0, 0).into(),
                size: wry::dpi::LogicalSize::new(width, height).into(),
            })
            .with_visible(true)
            .with_focused(false);
        // The policy cells are read by the handlers at event time, so
        // `navigate` changes what the next navigation/menu request sees
        // without rebuilding the engine.
        let control = SurfaceControl {
            id,
            navigation: Rc::new(RefCell::new(spec.policy.navigation)),
            menu: Rc::new(RefCell::new(MenuState::of(&spec))),
            events: self.events.clone(),
        };
        builder = builder.with_navigation_handler(control.navigation_handler());
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
        if crate::debug::debug_enabled() {
            use wry::WebViewExtUnix as _;
            connect_debug_tracing(&webview.webview());
        }
        // The engine's own context menu. `Trimmed` (the default) keeps what a
        // note needs — copy/cut/paste, selection, link and media actions — and
        // drops the browser chrome; "Inspect Element" only survives when
        // `devtools` is on. A *page's* menu is page content (a `contextmenu`
        // handler never reaches GTK), so YouTube's and Vimeo's are untouched.
        // The closure reads the shared cell, so `navigate` changes the trim
        // without a reconnect.
        use wry::WebViewExtUnix as _;
        let widget = webview.webview();
        let menu_state = control.menu.clone();
        webkit2gtk::WebViewExt::connect_context_menu(&widget, move |_, menu, _, _| {
            let state = *menu_state.borrow();
            if state.menu == ContextMenu::Engine {
                return false; // unhandled: WebKit shows its menu untouched
            }
            if state.menu == ContextMenu::Off {
                return true; // handled: show nothing
            }
            let dropped = trim_context_menu(menu, state.devtools, state.fullscreen);
            debug_log!(
                "context menu: kept {} item(s), dropped {dropped}",
                webkit2gtk::ContextMenuExt::items(menu).len()
            );
            false // unhandled: WebKit shows the menu we just trimmed
        });
        // The policy, applied through WebKitGTK's settings: wry 0.57 has no
        // builder-level switch for any of these (the macOS backend uses
        // `WKWebpagePreferences`/`with_devtools` instead). Fully qualified,
        // because GTK's `WidgetExt::settings` (style settings) would win method
        // resolution.
        apply_surface_policy(&widget, &control, &spec);
        // Backend → app events: title, load failure, crash, fullscreen, close.
        control.connect_handlers(&widget);
        window.show_all();

        self.surfaces.insert(
            id,
            NestedSurface {
                window,
                fixed,
                webview,
                control,
                storage: spec.policy.storage,
                size: (width as u32, height as u32),
            },
        );
        let _ = self.commands.send(Command::Create {
            id,
            width: width as u32,
            height: height as u32,
        });
        self.pump_gtk();
        Ok(())
    }

    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        // A live engine cannot change its storage context. Recreate it, the
        // same way the manager would if it had to.
        if self
            .surfaces
            .get(&id)
            .is_some_and(|surface| surface.storage != spec.policy.storage)
        {
            self.destroy(id);
            return self.create(id, spec);
        }
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return self.create(id, spec);
        };
        // Re-apply the whole policy on the reused engine: scripts, devtools,
        // fullscreen, media, popups, the context menu and the navigation
        // rule. (The settings are read at the *next* load; the navigation
        // handler reads the cell at the next decision.)
        use wry::WebViewExtUnix as _;
        let widget = surface.webview.webview();
        apply_surface_policy(&widget, &surface.control, &spec);
        debug_log!(
            "navigate block {id} to {:?} (navigation {:?})",
            spec.source,
            spec.policy.navigation
        );
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

    fn place(&mut self, id: SurfaceId, at: Placement) {
        // Scrolling never relayouts the page, but a *resize* must: configure
        // the toplevel so the client re-lays out instead of being stretched
        // by the app's paint (the macOS backend re-frames its webview for the
        // same reason).
        let (width, height) = (
            at.rect.size.width.as_f32().max(1.0) as u32,
            at.rect.size.height.as_f32().max(1.0) as u32,
        );
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        if surface.size == (width, height) {
            return;
        }
        surface.size = (width, height);
        // The GTK side first: a `gtk::Fixed` minimum size and the webview's
        // bounds are what stop a shrinking window (GTK never goes below the
        // child's size request), so they must follow the configure. The
        // compositor's `Command::Resize` then sends the authoritative
        // `xdg_toplevel.configure`.
        use gtk::prelude::*;
        use wry::WebViewExtUnix as _;
        surface.fixed.set_size_request(width as i32, height as i32);
        // wry set a *size request* on the webview when it built the fixed
        // parent (`set_size_request` in `add_to_container`); that minimum is
        // what keeps the window from shrinking, so it must follow too.
        surface
            .webview
            .webview()
            .set_size_request(width as i32, height as i32);
        surface.window.set_default_size(width as i32, height as i32);
        let _ = surface.webview.set_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0, 0).into(),
            size: wry::dpi::LogicalSize::new(width, height).into(),
        });
        surface.window.resize(width as i32, height as i32);
        let _ = self.commands.send(Command::Resize { id, width, height });
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

    fn popup_frames(&mut self, id: SurfaceId) -> Vec<crate::spec::PopupFrame> {
        // Like `frame_image`: the app reads popups from its frame pass, and GTK
        // must keep running for the client to produce them at all.
        self.pump_gtk();
        lock(&self.popups)
            .iter()
            .filter(|popup| popup.parent == id)
            .map(|popup| crate::spec::PopupFrame {
                id: popup.id,
                position: popup.position,
                frame: popup.frame.clone(),
            })
            .collect()
    }

    fn input(&mut self, id: SurfaceId, event: WebInputEvent) {
        let _ = self.commands.send(Command::Input { id, event });
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        std::mem::take(&mut *lock(&self.events))
    }

    fn wants_animation_frame(&mut self) -> bool {
        let wants = self.pump_did_work;
        self.pump_did_work = false;
        wants
    }

    fn supports_local(&self) -> bool {
        false
    }
}


/// `GPUI_WEBVIEW_DEBUG=1`: prove GTK hands pointer events to the view at all.
/// When input "does not work" this separates GDK/compositor problems from
/// WebKit's. Each surface connects its own, once, before the policy handlers.
pub(super) fn connect_debug_tracing(widget: &webkit2gtk::WebView) {
    use gtk::prelude::*;

    // The window above the view: separates GDK delivery from GTK's widget
    // dispatch when input stops arriving.
    if let Some(window_widget) = widget.toplevel() {
        window_widget.connect_button_press_event(|_, event| {
            debug_log!(
                "gtk-window: button-press {} at ({:.0},{:.0})",
                event.button(),
                event.position().0,
                event.position().1
            );
            gtk::glib::Propagation::Proceed
        });
        window_widget.connect_motion_notify_event(|_, event| {
            debug_log!(
                "gtk-window: motion at ({:.0},{:.0})",
                event.position().0,
                event.position().1
            );
            gtk::glib::Propagation::Proceed
        });
    }
    widget.connect_button_press_event(|_, event| {
        // The source device matters: WebKit hands touchscreen sources to
        // the parent class, which ignores them.
        let source = event
            .device()
            .map(|device| device.source())
            .unwrap_or(gtk::gdk::InputSource::Mouse);
        debug_log!(
            "gtk: button-press {} at ({:.0},{:.0}) state={:?} source={source:?} \
             send_event={} time={}",
            event.button(),
            event.position().0,
            event.position().1,
            event.state(),
            event.is_send_event(),
            event.time()
        );
        gtk::glib::Propagation::Proceed
    });
    widget.connect_scroll_event(|_, event| {
        debug_log!(
            "gtk: scroll dir={:?} delta=({:.1},{:.1}) at ({:.0},{:.0})",
            event.direction(),
            event.delta().0,
            event.delta().1,
            event.position().0,
            event.position().1
        );
        gtk::glib::Propagation::Proceed
    });
    widget.connect_motion_notify_event(|_, event| {
        // A stuck GTK grab (a popup whose window died without an ungrab)
        // swallows every later event; name it when it happens.
        let grab = gtk::grab_get_current().map(|widget| widget.widget_name().to_string());
        debug_log!(
            "gtk: motion on the web view at ({:.0},{:.0}) grab={grab:?}",
            event.position().0,
            event.position().1
        );
        gtk::glib::Propagation::Proceed
    });
    widget.connect_leave_notify_event(|_, event| {
        debug_log!(
            "gtk: leave-notify mode={:?} detail={:?}",
            event.mode(),
            event.detail()
        );
        gtk::glib::Propagation::Proceed
    });
    widget.connect_enter_notify_event(|_, event| {
        debug_log!(
            "gtk: enter-notify mode={:?} detail={:?}",
            event.mode(),
            event.detail()
        );
        gtk::glib::Propagation::Proceed
    });
}
