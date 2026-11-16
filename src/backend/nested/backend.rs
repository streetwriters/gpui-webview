//! The GTK/WebKit main-thread side: windows, webviews and the command
//! channel to the compositor thread.

use super::Command;
use super::Shared;
use crate::spec::ContextMenu;
use crate::spec::Error;
use crate::spec::Placement;
use crate::spec::Presentation;
use crate::spec::Result;
use crate::spec::Source;
use crate::spec::Storage;
use crate::spec::SurfaceFrame;
use crate::spec::SurfaceId;
use crate::spec::SurfaceSpec;
use crate::spec::WebInputEvent;
use crate::spec::WebViewEvent;
use gpui::Window;
use smithay::reexports::calloop::channel::Sender as CommandSender;
use smithay::reexports::calloop::channel::channel;
use smithay::reexports::wayland_server::ListeningSocket;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use super::compositor::compositor_main;
use super::lock;
use super::policy::{MenuState, SurfaceControl, apply_surface_policy, trim_context_menu};
use crate::debug::debug_log;
use crate::host::WebViewBackend;

/// The backend. One per process; a second is refused (`Error::Platform`).
pub struct NestedBackend {
    commands: CommandSender<Command>,
    compositor: Option<std::thread::JoinHandle<()>>,
    shared: Arc<Shared>,
    /// The GTK window + webview per surface (main thread only).
    surfaces: HashMap<SurfaceId, NestedSurface>,
    /// One `WebContext` for every `Storage::Shared` surface: one cookie jar.
    shared_context: Option<wry::WebContext>,
    gtk_ready: bool,
}

/// A live GTK/webview client window on our compositor.
pub(super) struct NestedSurface {
    window: gtk::Window,
    /// The container the webview lives in; its size request must follow a
    /// resize, or GTK ignores a smaller configure (T6).
    fixed: gtk::Fixed,
    webview: wry::WebView,
    control: SurfaceControl,
    storage: Storage,
    /// The size the toplevel is configured to; `place` reconfigures on change.
    size: (u32, u32),
}

/// Iterations the periodic pump drains per call. GTK's main context keeps a
/// source permanently ready in this environment (`main_iteration_do(false)`
/// returns true forever), so an unbounded drain burns a core; the host's 4 ms
/// cadence makes a small cap plenty.
const PUMP_ITERATIONS: usize = 16;

/// Iterations the one-shot pumps (`create`, `destroy`, `Drop`) drain: those
/// calls must finish the map / close work before returning.
const PUMP_DRAIN: usize = 1024;

/// The one nested backend a process may own.
static INSTANCE: std::sync::OnceLock<()> = std::sync::OnceLock::new();

impl NestedBackend {
    /// Binds the private socket and starts the compositor thread;
    pub fn new() -> Result<Self> {
        Self::start()
    }

    /// Like [`Self::new`], with dma-buf capabilities up front (headless E2E).
    /// Only built with `dma-buf`.
    #[cfg(dmabuf)]
    pub fn new_with_dmabuf(capabilities: Option<gpui::DmaBufCapabilities>) -> Result<Self> {
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

        // Set before the compositor thread and GTK/WebKit exist; `ensure_gtk`
        // calls `gdk::set_allowed_backends`, so `GDK_BACKEND` is not needed.
        //
        // SAFETY: `set_var` is only unsound while another thread reads or
        // writes the environment concurrently. This runs once, on the main
        // thread, at backend construction; the app's existing threads (GPUI's
        // event loop) do not read `WAYLAND_DISPLAY`, and GDK/WebKit have not
        // been initialized yet. This is the earliest point the design allows.
        unsafe {
            std::env::set_var("WAYLAND_DISPLAY", &socket_name);
        }

        let (commands, receiver) = channel();
        let (ready_tx, ready_rx) = mpsc::channel::<std::result::Result<(), String>>();
        let shared = Arc::new(Shared::default());
        let compositor = std::thread::Builder::new()
            .name("gpui-webview-compositor".into())
            .spawn({
                let shared = shared.clone();
                move || compositor_main(socket, receiver, shared, ready_tx)
            })
            .map_err(|error| Error::Platform(format!("compositor thread: {error}")))?;

        // Globals must exist before a client connects (T1): a failed start is
        // an error here, not a gtk_init hang on a socket nobody serves.
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => {}
            Ok(Err(message)) => return Err(Error::Platform(message)),
            Err(_) => return Err(Error::Platform("compositor thread did not start".into())),
        }

        let backend = Self {
            commands,
            compositor: Some(compositor),
            shared,
            surfaces: HashMap::new(),
            shared_context: None,
            gtk_ready: false,
        };
        let _ = INSTANCE.set(());
        Ok(backend)
    }

    /// Initializes GTK once, on the main thread (the `gtk_init` thread).
    fn ensure_gtk(&mut self) -> Result<()> {
        if self.gtk_ready {
            return Ok(());
        }
        static ALLOWED_BACKENDS: std::sync::Once = std::sync::Once::new();
        ALLOWED_BACKENDS.call_once(|| gtk::gdk::set_allowed_backends("wayland"));
        gtk::init().map_err(|error| Error::Platform(format!("gtk init: {error}")))?;
        self.gtk_ready = true;
        Ok(())
    }

    /// Drains pending GTK work so WebKit's IPC keeps moving between frames.
    /// `max_iterations` bounds the call: the periodic pump passes
    /// [`PUMP_ITERATIONS`] (GTK has a permanently-ready source here, so an
    /// unbounded drain burns a core), the one-shot create/teardown pumps pass
    /// [`PUMP_DRAIN`].
    fn pump_gtk(&mut self, max_iterations: usize) {
        if self.gtk_ready {
            // Drain, don't probe `events_pending`: the frame clock and WebKit's
            // IPC run through idle/timeout sources, not GDK events.
            for _ in 0..max_iterations {
                if !gtk::main_iteration_do(false) {
                    break;
                }
            }
        }
    }
}

impl Drop for NestedBackend {
    fn drop(&mut self) {
        // Clients disconnect before the compositor goes away.
        use gtk::prelude::*;
        for (_, surface) in self.surfaces.drain() {
            surface.window.close();
            drop(surface.webview);
        }
        self.pump_gtk(PUMP_DRAIN);
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
        // The formats go in before any client connects, so `linux-dmabuf` is
        // in the first registry roundtrip; otherwise `wl_shm` only.
        #[cfg(dmabuf)]
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
        #[cfg(not(dmabuf))]
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
        // The block shows the page, never GTK's titlebar or shadow.
        window.set_decorated(false);
        // T6: `set_resizable(false)` makes GDK clamp to its own hints and
        // ignore a smaller configure. Decorations are off, so this only affects
        // hints.
        window.set_resizable(true);
        let fixed = gtk::Fixed::new();
        fixed.set_size_request(width, height);
        window.add(&fixed);

        // Ephemeral: a fresh incognito context. Shared: one cookie jar. wry
        // ignores a supplied context under `incognito`, so the arms are
        // exclusive.
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
        // The handlers read these cells at event time, so `navigate` changes
        // the next decision without rebuilding the engine.
        let control = SurfaceControl {
            id,
            navigation: Rc::new(RefCell::new(spec.policy.navigation)),
            menu: Rc::new(RefCell::new(MenuState::of(&spec))),
            shared: self.shared.clone(),
        };
        builder = builder.with_navigation_handler(control.navigation_handler());
        let builder = match &spec.source {
            Source::Url(url) => builder.with_url(url.clone()),
            Source::Html { html, .. } => builder.with_html(html.clone()),
        };
        let webview = builder
            .build_gtk(&fixed)
            .map_err(|error| Error::Platform(format!("webkitgtk: {error}")))?;
        // T23: trim in place and return `false` so WebKit shows the result;
        // `true` only for `ContextMenu::Off`. A page's own menu never reaches
        // GTK.
        use wry::WebViewExtUnix as _;
        let widget = webview.webview();
        let menu_state = control.menu.clone();
        webkit2gtk::WebViewExt::connect_context_menu(&widget, move |_, menu, _, _| {
            let state = *menu_state.borrow();
            if state.menu == ContextMenu::Off {
                return true; // handled: show nothing
            }
            trim_context_menu(menu, state.devtools);
            false // unhandled: WebKit shows the menu we just trimmed
        });
        // wry 0.57 exposes none of these; the macOS backend uses WKWebView
        // preferences. Fully qualified so GTK's style `settings` does not win
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
        let _ = self.commands.send(Command::Configure {
            id,
            width: width as u32,
            height: height as u32,
        });
        self.pump_gtk(PUMP_DRAIN);
        Ok(())
    }

    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        // A live engine cannot change its storage context; recreate it.
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
        // Re-apply the whole policy; the settings take effect on the next
        // load, the navigation/menu cells immediately.
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
        }
    }

    fn place(&mut self, id: SurfaceId, at: Placement) {
        // A resize must relayout the page, so configure the toplevel.
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
        // The GTK side first: both size requests stop a shrink (T6); the
        // compositor's configure is authoritative.
        use gtk::prelude::*;
        use wry::WebViewExtUnix as _;
        surface.fixed.set_size_request(width as i32, height as i32);
        // wry's webview size request must follow too, or the minimum holds.
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
        let _ = self.commands.send(Command::Configure { id, width, height });
    }

    fn set_focus(&mut self, id: SurfaceId, focused: bool) {
        // Optimistic, so the next keystroke routes correctly.
        *lock(&self.shared.focus) = if focused { Some(id) } else { None };
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
        lock(&self.shared.frames).remove(&id);
        {
            let mut focus = lock(&self.shared.focus);
            if *focus == Some(id) {
                *focus = None;
            }
        }
        let _ = self.commands.send(Command::Destroy { id });
        self.pump_gtk(PUMP_DRAIN);
    }

    fn frame_image(&mut self, id: SurfaceId) -> Option<SurfaceFrame> {
        lock(&self.shared.frames).get(&id).cloned()
    }

    fn popup_frames(&mut self, id: SurfaceId) -> Vec<crate::spec::PopupFrame> {
        lock(&self.shared.popups)
            .iter()
            .filter(|popup| popup.parent == id)
            .map(|popup| popup.frame.clone())
            .collect()
    }

    fn pump(&mut self) {
        // GTK/WebKit only advance while their main context is iterated; the
        // host drives this at a fixed cadence while a surface is live.
        self.pump_gtk(PUMP_ITERATIONS);
    }

    fn input(&mut self, id: SurfaceId, event: WebInputEvent) {
        let _ = self.commands.send(Command::Input { id, event });
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        std::mem::take(&mut *lock(&self.shared.events))
    }

    fn set_waker(&mut self, waker: Arc<dyn Fn() + Send + Sync>) {
        *lock(&self.shared.waker) = Some(waker);
    }

    fn set_keyboard_layout(&mut self, name: &str) {
        let _ = self.commands.send(Command::KeyboardLayout(name.to_owned()));
    }

    fn set_scale_factor(&mut self, scale: f32) {
        let _ = self.commands.send(Command::OutputScale(scale as f64));
    }

    fn bridges_selection(&self) -> bool {
        // WebKitGTK's clipboard is our private data device (`selection.rs`).
        true
    }

    fn set_host_selection(&mut self, selection: crate::spec::Selection, text: Option<String>) {
        let _ = self
            .commands
            .send(Command::HostSelection { selection, text });
    }

    fn surface_presented(&mut self, id: SurfaceId) {
        // Best-effort: the compositor may already be gone.
        let _ = self.commands.send(Command::Presented { id });
    }
}
