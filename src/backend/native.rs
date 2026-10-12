//! The native backend: wry hosts the platform engine, clipped and stacked by
//! this crate (EMBEDS.md §10).
//!
//! Backends by platform:
//!
//! * **macOS**: a `WKWebView` inside a layer-backed `NSView` container under
//!   GPUI's view. `masksToBounds` clips it to the block; an intersecting GPUI
//!   overlay hides the surface (freeze) until the transparent-hole pattern
//!   lands.
//! * **Windows / Linux**: not implemented yet. Windows needs a child `HWND`
//!   clip container with client-relative bounds (§10.2); Linux needs the
//!   in-process nested Wayland compositor for Wayland and the X11 tiers for
//!   X11 (§10.3/§10.4). Both report `Error::Platform` so the app falls back to
//!   the poster + `openUrl` exactly as with `Error::Unsupported`.
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
use crate::spec::{
    Error, Placement, Presentation, Result, SurfaceId, SurfaceSpec, WebViewEvent,
};

#[cfg(target_os = "macos")]
mod macos;

/// One live native surface.
pub(crate) struct NativeSurface {
    /// The wry webview (the platform engine's view).
    pub(crate) webview: wry::WebView,
    /// The macOS layer container the webview lives in; `None` elsewhere.
    #[cfg(target_os = "macos")]
    pub(crate) container: Option<objc2::rc::Retained<objc2_app_kit::NSView>>,
    /// Whether the surface is currently lowered (an overlay intersects it).
    pub(crate) lowered: bool,
}

/// The native backend. One per window.
pub struct NativeBackend {
    /// The raw parent view, captured at `attach`.
    #[cfg(target_os = "macos")]
    parent: Option<macos::ParentView>,
    surfaces: HashMap<SurfaceId, NativeSurface>,
    loader: Option<Arc<dyn crate::host::SchemeLoader>>,
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
        #[cfg(not(target_os = "macos"))]
        {
            Err(Error::Platform(
                "native backend: only macOS is implemented (see EMBEDS.md §10)".into(),
            ))
        }
    }

    fn empty() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            parent: None,
            surfaces: HashMap::new(),
            loader: None,
            events: Arc::new(Mutex::new(Vec::new())),
            attached: false,
        }
    }

    /// Test-only/test-support: the events queued by platform handlers so a
    /// headless test can assert the policy wiring without a GUI.
    pub fn queued_events(&self) -> Vec<WebViewEvent> {
        self.events.lock().map(|events| events.clone()).unwrap_or_default()
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
        #[cfg(not(target_os = "macos"))]
        {
            let _ = window;
            Err(Error::Platform(
                "native backend: only macOS is implemented".into(),
            ))
        }
    }

    fn create(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        #[cfg(target_os = "macos")]
        {
            let parent = self
                .parent
                .as_ref()
                .ok_or_else(|| Error::Platform("attach was not called".into()))?;
            let surface = macos::create_surface(id, &spec, parent, &self.events)?;
            self.surfaces.insert(id, surface);
            return Ok(());
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (id, spec);
            Err(Error::Platform(
                "native backend: only macOS is implemented".into(),
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
        match &spec.source {
            crate::spec::Source::Url(url) => surface
                .webview
                .load_url(url)
                .map_err(|error| Error::Load(error.to_string()))?,
            crate::spec::Source::Html { html, .. } => surface
                .webview
                .load_html(html)
                .map_err(|error| Error::Load(error.to_string()))?,
            crate::spec::Source::Local { .. } => {
                return Err(Error::Platform(
                    "local sources need the platform scheme handler".into(),
                ));
            }
        }
        surface.lowered = false;
        Ok(())
    }

    fn place(&mut self, id: SurfaceId, at: Placement) {
        #[cfg(target_os = "macos")]
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
        #[cfg(not(target_os = "macos"))]
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
        #[cfg(not(target_os = "macos"))]
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
        }
        // Dropping the wry WebView tears the engine's view down.
        drop(surface.webview);
    }

    fn set_scheme_loader(&mut self, loader: Arc<dyn crate::host::SchemeLoader>) {
        self.loader = Some(loader);
    }

    fn supports_local(&self) -> bool {
        // Requires a registered URL scheme; implemented with `webclip` in
        // phase 4 (§5). Until then clips fall back to their poster.
        false
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        self.events
            .lock()
            .map(|mut events| std::mem::take(&mut *events))
            .unwrap_or_default()
    }
}
