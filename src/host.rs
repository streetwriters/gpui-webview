//! The backend contract: one implementation per platform (EMBEDS.md §8/§9); on
//! Linux the backend is process-wide and the manager is per window. `attach`
//! hands over the window once; everything after is state or geometry.

use gpui_kit::{Window, px};

use crate::spec::{
    Error, OverlayBehavior, Placement, Presentation, Result, Source, SurfaceId, SurfaceSpec,
};

/// A seekable byte stream for `Source::Local` content; produced by a [`SchemeLoader`].
pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}
impl<T: std::io::Read + std::io::Seek + Send> ReadSeek for T {}

/// Streams `Source::Local` content to the surface; the app implements it over
/// its attachment store (EMBEDS.md §5). A backend registers the scheme with the
/// platform engine and pulls ranges through these methods.
pub trait SchemeLoader: Send + Sync + 'static {
    /// Opens `scheme://path`. Called once per navigation/range at most.
    fn open(&self, scheme: &str, path: &str) -> Result<Box<dyn ReadSeek>>;

    /// The total length when known without opening; `None` lets a backend buffer.
    fn len(&self, scheme: &str, path: &str) -> Option<u64> {
        let _ = (scheme, path);
        None
    }

    /// The MIME type to serve the content as.
    fn mime(&self, scheme: &str, path: &str) -> Option<String> {
        let _ = (scheme, path);
        None
    }
}

/// The backend contract. Implementations are `'static`; only the platform-refusable methods fail.
pub trait WebViewBackend: 'static {
    /// The backend's name for diagnostics ("mock", "web", "macos", …).
    fn name(&self) -> &'static str {
        "backend"
    }

    /// Binds the backend to a window (once, before the first `create`). A failure
    /// here degrades to `Error::Unsupported` before any engine exists.
    fn attach(&mut self, window: &Window) -> Result<()>;

    /// Creates a surface for `id` (at most `Config::capacity` times per backend).
    fn create(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()>;

    /// Navigates an existing surface to new content (pool reuse; the default
    /// recreates the engine, real backends override it).
    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        self.destroy(id);
        self.create(id, spec)
    }

    /// Applies geometry and visibility; clipping never changes the page's layout.
    fn place(&mut self, id: SurfaceId, at: Placement);

    fn set_focus(&mut self, id: SurfaceId, focused: bool);

    /// Opens the separate-window / fullscreen presentation; `Inline` restores.
    fn present(&mut self, id: SurfaceId, mode: Presentation) -> Result<()>;

    /// Tears the surface down. Must be safe to call twice.
    fn destroy(&mut self, id: SurfaceId);

    /// The latest decoded frame for a self-compositing backend (the nested
    /// compositor); `None` otherwise, and before the first commit.
    fn frame_image(&mut self, _id: SurfaceId) -> Option<crate::spec::SurfaceFrame> {
        None
    }

    /// Native popups above `id`'s surface, bottom to top; only self-compositing
    /// backends produce these.
    fn popup_frames(&mut self, _id: SurfaceId) -> Vec<crate::spec::PopupFrame> {
        Vec::new()
    }

    /// Advances the platform engine's main-context work (GTK/WebKit on Linux);
    /// a backend whose engine owns its own loop leaves this a no-op. The host
    /// calls it on the main thread at a fixed cadence while a surface is live.
    fn pump(&mut self) {}

    /// Forwards one input event; only self-compositing backends consume it.
    fn input(&mut self, _id: SurfaceId, _event: crate::spec::WebInputEvent) {}

    /// What `Config::on_overlay` this backend can honour: a self-compositing one
    /// lowers (overlays paint above), a child view freezes. Default: `Freeze`.
    fn default_overlay_behavior(&self) -> OverlayBehavior {
        OverlayBehavior::Freeze
    }

    /// Registers the commit waker: the backend calls it after it publishes a
    /// new frame for any surface (the compositor thread's "commit"), and the
    /// app wakes its paint loop off it instead of polling. Coalescing is the
    /// waker's job; calls can arrive on any thread. Default: no-op.
    fn set_waker(&mut self, _waker: std::sync::Arc<dyn Fn() + Send + Sync>) {}

    /// The app painted `id`'s latest frame. Self-compositing backends release
    /// the surface's parked frame callbacks here (present pacing); an idle
    /// app that never reports leaves the client waiting, which is the point.
    fn surface_presented(&mut self, _id: SurfaceId) {}

    /// The host's active keyboard layout, by the name the platform reports
    /// (GPUI's `keyboard_layout().name()`, e.g. `French`). A backend that hands
    /// its client a keymap matches it to this layout so keys type what the
    /// user's keyboard shows. Default: no-op.
    fn set_keyboard_layout(&mut self, _name: &str) {}

    /// The host window's scale factor (GPUI's `Window::scale_factor`, e.g.
    /// `2.0` on a HiDPI screen). A backend whose client renders off-screen
    /// renders at this density, so the page is sharp instead of upscaled.
    /// Default: no-op.
    fn set_scale_factor(&mut self, _scale: f32) {}

    /// Sets the streaming source for `Source::Local` (once, before any `create`).
    fn set_scheme_loader(&mut self, _loader: std::sync::Arc<dyn SchemeLoader>) {}

    /// Backend-originated events since the last drain.
    fn events(&mut self) -> Vec<crate::spec::WebViewEvent> {
        Vec::new()
    }

    /// Test-support (`test-support` feature): a PNG snapshot of `id`'s current
    /// page, rendered by the engine itself (WebView2's `CapturePreview`), so it
    /// needs no desktop compositor. `None` when the platform cannot render one
    /// or `id` is not live. The Windows E2E uses it to prove the page paints
    /// even where the desktop readback is blind to GPU composition.
    #[cfg(feature = "test-support")]
    fn page_snapshot(&mut self, _id: SurfaceId) -> Option<Vec<u8>> {
        None
    }

    /// Whether the backend can serve `Source::Local` (a scheme handler or blob URL).
    fn supports_local(&self) -> bool {
        false
    }
}

/// The block's box from a placement, without the clip (the backend clips).
#[allow(dead_code)] // Used by the platform backends behind features.
pub(crate) fn surface_rect(at: &Placement) -> gpui_kit::Bounds<gpui_kit::Pixels> {
    at.rect
}

/// Whether a URL is content being loaded (`about:`, `data:`, `blob:`) rather
/// than a user navigation. Engine navigation handlers fire for both on some
/// platforms — WebView2 fires for `about:blank` on `NavigateToString` — so the
/// loading schemes must pass while other non-http(s) schemes stay refused.
pub(crate) fn is_loading_geometry(url: &str) -> bool {
    url.starts_with("about:") || url.starts_with("data:") || url.starts_with("blob:")
}

/// Zero bounds, used by backends that hide a surface by moving it out.
#[allow(dead_code)] // Used by the platform backends behind features.
pub(crate) fn empty_bounds() -> gpui_kit::Bounds<gpui_kit::Pixels> {
    use gpui_kit::{Bounds, point, size};
    Bounds::new(point(px(0.), px(0.)), size(px(0.), px(0.)))
}

/// Whether `backend` can serve a spec (otherwise: poster + `openUrl` fallback).
pub fn spec_supported(backend: &dyn WebViewBackend, spec: &SurfaceSpec) -> bool {
    match &spec.source {
        Source::Local { .. } => backend.supports_local(),
        Source::Url(url) => {
            let url = url.trim().to_ascii_lowercase();
            url.starts_with("http://") || url.starts_with("https://")
        }
        Source::Html { .. } => true,
    }
}

/// A backend that cannot do anything; tests and hosts can install it explicitly.
pub struct UnsupportedBackend(pub &'static str);

impl WebViewBackend for UnsupportedBackend {
    fn name(&self) -> &'static str {
        self.0
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::Policy;

    #[test]
    fn only_http_sources_are_remote_surface_material() {
        let backend = UnsupportedBackend("test");
        let supported =
            |source| spec_supported(&backend, &SurfaceSpec::new(source, Policy::strict(), "x"));
        assert!(supported(Source::Url("https://example.com".into())));
        assert!(supported(Source::Url("http://localhost:8080".into())));
        assert!(!supported(Source::Url("javascript:alert(1)".into())));
        assert!(!supported(Source::Url("data:text/html,x".into())));
        assert!(!supported(Source::Url("file:///etc/passwd".into())));
        assert!(supported(Source::Html {
            html: "<p>x</p>".into(),
            base: None
        }));
        // The unsupported backend cannot serve local content.
        assert!(!supported(Source::Local {
            scheme: "nn-clip",
            path: "hash".into()
        }));
    }
}
