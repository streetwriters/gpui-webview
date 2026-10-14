//! The native backend: wry hosts the platform engine, clipped and stacked by
//! this crate (EMBEDS.md §10).
//!
//! Backends by platform:
//!
//! * **macOS**: a `WKWebView` inside a layer-backed `NSView` container under
//!   GPUI's view. `masksToBounds` clips it to the block; an intersecting GPUI
//!   overlay hides the surface (freeze) until the transparent-hole pattern
//!   lands.
//! * **Windows**: a WebView2 child HWND inside a clip-container HWND under
//!   GPUI's window (§10.2). Windows clips children to the container, and the
//!   webview keeps the block's full size, so scrolling never relayouts it.
//!   GPUI must present through a flip-model swapchain (`prepare_process`,
//!   T44), or its topmost DirectComposition target would hide the surface.
//! * **Linux**: the implemented path is the in-process nested Wayland
//!   compositor (`nested`, §10.3/§10.4); a wry child view reports
//!   `Error::Platform` so the app falls back to the poster + `openUrl`
//!   exactly as with `Error::Unsupported`.
//!
//! # Manual checklist (per §15)
//!
//! Create → place → clip at the viewport → scroll a clipped surface → overlay
//! a menu → teardown → RSS back to baseline → second activation reuses the
//! engine (no extra process) at 1x and 2x scale. Also verify a `javascript:`
//! clip cannot run script and its links leave through `OpenExternal`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use gpui_kit::Window;

use crate::host::WebViewBackend;
use crate::spec::{Error, Placement, Presentation, Result, SurfaceId, SurfaceSpec, WebViewEvent};

#[cfg(target_os = "macos")]
mod macos;

#[cfg(target_os = "windows")]
mod windows;

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[allow(dead_code)] // Windows places through this; the tests cover it everywhere.
mod geometry;

/// One live native surface.
pub(crate) struct NativeSurface {
    /// The wry webview (the platform engine's view).
    pub(crate) webview: wry::WebView,
    /// The clip container the webview lives in: an AppKit view on macOS, an
    /// `HWND` on Windows; `None` elsewhere.
    #[cfg(target_os = "macos")]
    pub(crate) container: Option<objc2::rc::Retained<objc2_app_kit::NSView>>,
    // `::windows` (not `windows`): the child module `mod windows` shadows the
    // crate name in this file.
    #[cfg(target_os = "windows")]
    pub(crate) container: Option<::windows::Win32::Foundation::HWND>,
    /// Whether the surface is currently lowered (an overlay intersects it).
    pub(crate) lowered: bool,
}

/// The native backend. One per window.
pub struct NativeBackend {
    /// The raw parent view/window, captured at `attach`.
    #[cfg(target_os = "macos")]
    parent: Option<macos::ParentView>,
    #[cfg(target_os = "windows")]
    parent: Option<windows::ParentWindow>,
    /// WebView2's shared environment, built eagerly and passed to every
    /// webview: WebView2's *private* mode still needs a writable user-data
    /// folder, and a second folder would start a second browser process tree.
    /// The default `<exe>.WebView2` next to the binary is unwritable under
    /// Program Files, so the folder lives in `%LOCALAPPDATA%` (T44's
    /// `prepare_process` sibling concern; Windows only).
    #[cfg(target_os = "windows")]
    context: wry::WebContext,
    surfaces: HashMap<SurfaceId, NativeSurface>,
    /// Events raised by the platform handlers (navigation, loads), drained by
    /// the manager.
    events: Arc<Mutex<Vec<WebViewEvent>>>,
    attached: bool,
}

impl NativeBackend {
    /// Probes the platform engine. `Unsupported` when the runtime library is
    /// missing or the session cannot host an inline surface, so the app uses
    /// the poster fallback.
    pub fn new() -> Result<Self> {
        #[cfg(target_os = "macos")]
        {
            // WebKit is part of the OS on macOS; there is no runtime to probe.
            Ok(Self::empty())
        }
        #[cfg(target_os = "windows")]
        {
            // T44: with DirectComposition on, GPUI's topmost target draws
            // above every child HWND; the surface would take input and never
            // be visible. Fail loudly so the app keeps its poster instead.
            // The exact value GPUI accepts (gpui-pre-windows/src/platform.rs:
            // 231): "true" or "1".
            if !direct_composition_disabled() {
                return Err(Error::Platform(
                    "DirectComposition is on: child HWNDs would be invisible; \
                     call gpui_webview::prepare_process() before GPUI starts (T44)"
                        .into(),
                ));
            }
            // No WebView2 Runtime: no engine to host.
            wry::webview_version()
                .map(|_| Self::empty())
                .map_err(|_| Error::Unsupported)
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            Err(Error::Platform(
                "native backend: only macOS (WKWebView) and Windows (WebView2) are implemented \
                 (see EMBEDS.md §10)"
                    .into(),
            ))
        }
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn empty() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            parent: None,
            #[cfg(target_os = "windows")]
            parent: None,
            #[cfg(target_os = "windows")]
            context: wry::WebContext::new(data_directory()),
            surfaces: HashMap::new(),
            events: Arc::new(Mutex::new(Vec::new())),
            attached: false,
        }
    }

    /// Test-only/test-support: the events queued by platform handlers so a
    /// headless test can assert the policy wiring without a GUI.
    pub fn queued_events(&self) -> Vec<WebViewEvent> {
        self.events
            .lock()
            .map(|events| events.clone())
            .unwrap_or_default()
    }
}

impl WebViewBackend for NativeBackend {
    fn name(&self) -> &'static str {
        "native"
    }

    fn attach(&mut self, window: &Window) -> Result<()> {
        if self.attached {
            return Ok(());
        }
        #[cfg(target_os = "macos")]
        {
            self.parent = Some(macos::ParentView::from_window(window)?);
            self.attached = true;
            Ok(())
        }
        #[cfg(target_os = "windows")]
        {
            self.parent = Some(windows::ParentWindow::from_window(window)?);
            self.attached = true;
            Ok(())
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = window;
            Err(Error::Platform(
                "native backend: only macOS and Windows are implemented".into(),
            ))
        }
    }

    fn create(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        {
            let parent = self
                .parent
                .as_ref()
                .ok_or_else(|| Error::Platform("attach was not called".into()))?
                .clone();
            #[cfg(target_os = "macos")]
            let surface = macos::create_surface(id, &spec, &parent, &self.events)?;
            #[cfg(target_os = "windows")]
            let surface = windows::create_surface(
                id,
                &spec,
                &parent,
                &self.events,
                &mut self.context,
            )?;
            self.surfaces.insert(id, surface);
            return Ok(());
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (id, spec);
            Err(Error::Platform(
                "native backend: only macOS and Windows are implemented".into(),
            ))
        }
    }

    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        // wry has no "reconfigure this view" call, but the engine cost is in
        // the process, not the wrapper: load the new source into the same
        // webview and re-apply the platform policy.
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return self.create(id, spec);
        };
        #[cfg(target_os = "macos")]
        {
            macos::apply_policy(&surface.webview, &spec);
        }
        #[cfg(target_os = "windows")]
        {
            windows::apply_policy(&surface.webview, &spec);
        }
        match &spec.source {
            crate::spec::Source::Url(url) => surface
                .webview
                .load_url(url)
                .map_err(|error| Error::Load(error.to_string()))?,
            crate::spec::Source::Html { html, .. } => surface
                .webview
                .load_html(html)
                .map_err(|error| Error::Load(error.to_string()))?,
        }
        surface.lowered = false;
        Ok(())
    }

    fn place(&mut self, id: SurfaceId, at: Placement) {
        #[cfg(any(target_os = "macos", target_os = "windows"))]
        let parent = self.parent.clone();
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        #[cfg(target_os = "macos")]
        {
            if let Some(parent) = &parent {
                macos::place(surface, at, parent);
            }
            return;
        }
        #[cfg(target_os = "windows")]
        {
            if let Some(parent) = &parent {
                windows::place(surface, id, at, parent);
            }
            return;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = (surface, at);
        }
    }

    fn set_focus(&mut self, id: SurfaceId, focused: bool) {
        let Some(surface) = self.surfaces.get(&id) else {
            return;
        };
        if focused {
            let _ = surface.webview.focus();
            return;
        }
        #[cfg(target_os = "macos")]
        {
            // Hand AppKit's first responder back to GPUI's view (wry's
            // `focus_parent` would focus our own clip container instead).
            let _ = surface;
            if let Some(parent) = self.parent.as_ref() {
                parent.make_first_responder();
            }
        }
        #[cfg(target_os = "windows")]
        {
            // Give Win32 focus back to GPUI's window, but only when this
            // surface plausibly owns it: `SetFocus` would activate a background
            // window (wry's `focus_parent` would focus our own container).
            if let Some(parent) = self.parent.as_ref() {
                windows::return_focus(parent, surface.container, id);
            }
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            let _ = surface.webview.focus_parent();
        }
    }

    fn present(&mut self, _id: SurfaceId, mode: Presentation) -> Result<()> {
        // Inline is the only presentation the child-view backend has today;
        // window/fullscreen are the separate-window path (§13).
        if mode == Presentation::Inline {
            return Ok(());
        }
        Err(Error::Unsupported)
    }

    fn destroy(&mut self, id: SurfaceId) {
        let Some(surface) = self.surfaces.remove(&id) else {
            return;
        };
        #[cfg(target_os = "macos")]
        {
            use objc2::rc::{Retained, autoreleasepool};
            // The webview may still be AppKit's first responder; give the
            // keyboard back to GPUI before the view disappears.
            if let Some(parent) = self.parent.as_ref() {
                parent.make_first_responder();
            }
            if let Some(container) = &surface.container {
                let container: &Retained<objc2_app_kit::NSView> = container;
                autoreleasepool(|_| {
                    container.removeFromSuperview();
                });
            }
            // Dropping the wry WebView tears the engine's view down.
            drop(surface.webview);
            return;
        }
        #[cfg(target_os = "windows")]
        {
            windows::destroy(surface, id, self.parent.as_ref());
            return;
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            drop(surface.webview);
        }
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        self.events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }

    /// See [`WebViewBackend::page_snapshot`]: WebView2 renders its own PNG.
    #[cfg(feature = "test-support")]
    fn page_snapshot(&mut self, id: SurfaceId) -> Option<Vec<u8>> {
        #[cfg(target_os = "windows")]
        {
            let surface = self.surfaces.get(&id)?;
            windows::page_snapshot(&surface.webview)
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = id;
            None
        }
    }
}

/// WebView2's persistent data directory: `%LOCALAPPDATA%\<exe stem>\gpui-webview`.
///
/// WebView2's default puts `<exe>.WebView2` next to the binary, which is
/// unwritable under Program Files. `None` leaves WebView2's own default in
/// place (a developer run, or a missing `%LOCALAPPDATA%`).
#[cfg(target_os = "windows")]
fn data_directory() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("LOCALAPPDATA")?;
    let stem = std::env::current_exe().ok()?.file_stem()?.to_owned();
    Some(std::path::Path::new(&base).join(stem).join("gpui-webview"))
}

/// Whether GPUI will start without DirectComposition, by the exact rule its
/// Windows platform uses (`gpui-pre-windows/src/platform.rs:231`): the value
/// `"true"` or `"1"`. Anything else (unset, `"0"`, `"yes"`) leaves
/// DirectComposition on, and child HWNDs would be invisible (T44).
#[cfg(target_os = "windows")]
fn direct_composition_disabled() -> bool {
    std::env::var("GPUI_DISABLE_DIRECT_COMPOSITION")
        .is_ok_and(|value| value == "true" || value == "1")
}

/// The wry builder pieces every child-view platform shares: initial bounds,
/// visibility, focus, devtools, the navigation policy, storage mode and the
/// initial source. Platform modules add their engine specifics on top (macOS'
/// `WKWebViewConfiguration`, WebView2's settings and handlers).
///
/// `allow_geometry` lets non-http(s) URLs through under
/// [`crate::spec::Navigation::OpenExternal`]: Windows must allow them, because
/// WebView2 fires the navigation handler for `NavigateToString`'s
/// `about:blank` — which is how [`crate::spec::Source::Html`] loads (WebKit
/// does not fire it for `loadHTMLString`). The nested Linux backend has always
/// allowed them for the same reason.
pub(super) fn base_builder<'a>(
    mut builder: wry::WebViewBuilder<'a>,
    id: SurfaceId,
    spec: &SurfaceSpec,
    events: &Arc<Mutex<Vec<WebViewEvent>>>,
    allow_geometry: bool,
) -> Result<wry::WebViewBuilder<'a>> {
    let navigation_events = events.clone();
    let policy = spec.policy.clone();
    builder = builder
        .with_bounds(wry::Rect {
            position: wry::dpi::LogicalPosition::new(0, 0).into(),
            size: wry::dpi::LogicalSize::new(1, 1).into(),
        })
        .with_visible(false)
        .with_focused(false)
        .with_devtools(spec.policy.engine.devtools)
        .with_navigation_handler(move |url| match policy.navigation {
            crate::spec::Navigation::Allow => true,
            crate::spec::Navigation::OpenExternal => {
                if url.starts_with("http://") || url.starts_with("https://") {
                    if let Ok(mut events) = navigation_events.lock() {
                        events.push(WebViewEvent::OpenExternal { id, url });
                    }
                    false
                } else if crate::host::is_loading_geometry(&url) {
                    // `about:`, `data:` and `blob:` load content; they are not
                    // user navigations. Windows fires the handler for
                    // `NavigateToString`'s `about:blank`, so it must allow them
                    // (`allow_geometry`); WebKit never consults the handler for
                    // `loadHTMLString`.
                    allow_geometry
                } else {
                    // Every other scheme (`mailto:`, `ms-settings:`, `file:`,
                    // `javascript:`, …) is refused: it must not reach the
                    // host's URL opener from an untrusted clip.
                    false
                }
            }
            crate::spec::Navigation::Block => false,
        });
    if spec.policy.storage == crate::spec::Storage::Ephemeral {
        builder = builder.with_incognito(true);
    }
    builder = match &spec.source {
        crate::spec::Source::Url(url) => builder.with_url(url.clone()),
        crate::spec::Source::Html { html, .. } => builder.with_html(html.clone()),
    };
    Ok(builder)
}
