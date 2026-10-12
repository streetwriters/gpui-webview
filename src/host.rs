//! The backend contract: one implementation per platform, one instance per
//! window (EMBEDS.md §8/§9).
//!
//! `attach` hands the backend the window's raw handle once. After that every
//! call is state or geometry, so a host can drive the surface from its render
//! and paint paths without touching platform APIs.

use gpui_kit::{Window, px};

use crate::spec::{Error, Placement, Presentation, Result, Source, SurfaceId, SurfaceSpec};

/// A seekable, owned byte stream for `Source::Local` content (a web clip's
/// saved page). Boxed through a blanket impl; produced by an app's
/// [`SchemeLoader`].
pub trait ReadSeek: std::io::Read + std::io::Seek + Send {}
impl<T: std::io::Read + std::io::Seek + Send> ReadSeek for T {}

/// Streams `Source::Local` content to the surface. The app implements this
/// over its attachment store; the bytes never pass through the crate's callers
/// or their documents (EMBEDS.md §5).
///
/// A backend registers the scheme with the platform engine (a URL-scheme
/// handler on `WKWebView`/WebView2/WebKitGTK, a blob URL on the web) and pulls
/// ranges through these methods when the engine asks.
pub trait SchemeLoader: Send + Sync + 'static {
    /// Opens `scheme://path`. Called once per navigation/range at most.
    fn open(&self, scheme: &str, path: &str) -> Result<Box<dyn ReadSeek>>;

    /// The total length in bytes, when known without opening. `None` lets a
    /// backend fall back to buffering.
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

/// The backend contract.
///
/// Implementations must be `'static` (the manager owns them). Every method is
/// fallible only where the platform can refuse; `place` and `destroy` never
/// fail: geometry and teardown must always be attempted.
pub trait WebViewBackend: 'static {
    /// The backend's name for diagnostics ("mock", "web", "macos", …).
    fn name(&self) -> &'static str {
        "backend"
    }

    /// Binds the backend to a window. Called once, before the first `create`.
    /// A backend that cannot attach must say so now, so the manager can
    /// degrade to `Error::Unsupported` before any engine exists.
    fn attach(&mut self, window: &Window) -> Result<()>;

    /// Creates a surface for `id`. Called at most `Config::capacity` times
    /// over the backend's life (a reused surface is *navigated*, not
    /// recreated).
    fn create(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()>;

    /// Navigates an existing surface to new content (pool reuse). The default
    /// recreates the engine, which is exactly what the pool exists to avoid;
    /// real backends override it.
    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        self.destroy(id);
        self.create(id, spec)
    }

    /// Applies geometry and visibility. `Placement::rect` is the full block;
    /// clipping changes only what is shown, never the page's layout.
    fn place(&mut self, id: SurfaceId, at: Placement);

    /// Routes keyboard focus into (or out of) the surface.
    fn set_focus(&mut self, id: SurfaceId, focused: bool);

    /// Opens the separate-window / fullscreen presentation. `Inline` restores
    /// the inline surface.
    fn present(&mut self, id: SurfaceId, mode: Presentation) -> Result<()>;

    /// Tears the surface down. Must be safe to call twice.
    fn destroy(&mut self, id: SurfaceId);

    /// Sets the streaming source for `Source::Local`. Called once at setup,
    /// before any `create`.
    fn set_scheme_loader(&mut self, _loader: std::sync::Arc<dyn SchemeLoader>) {}

    /// Backend-originated events since the last drain. The manager forwards
    /// them with its own; a backend with nothing to say returns empty.
    fn events(&mut self) -> Vec<crate::spec::WebViewEvent> {
        Vec::new()
    }

    /// Whether the backend can serve `Source::Local` (it needs a scheme
    /// handler or a blob URL). The manager refuses a local surface on a
    /// backend that cannot, instead of navigating it to nothing.
    fn supports_local(&self) -> bool {
        false
    }
}

/// The inline rect of a surface from a placement: the block's box, without the
/// clip (the backend clips; the page must not relayout).
#[allow(dead_code)] // Used by the platform backends behind features.
pub(crate) fn surface_rect(at: &Placement) -> gpui_kit::Bounds<gpui_kit::Pixels> {
    at.rect
}

/// Zero bounds, used by backends that hide a surface by moving it out.
#[allow(dead_code)] // Used by the platform backends behind features.
pub(crate) fn empty_bounds() -> gpui_kit::Bounds<gpui_kit::Pixels> {
    use gpui_kit::{Bounds, point, size};
    Bounds::new(point(px(0.), px(0.)), size(px(0.), px(0.)))
}

/// Whether a spec can be served by `backend`, so the caller gets a poster +
/// `openUrl` fallback instead of a blank engine.
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

/// A backend that cannot do anything, used when no platform feature is built.
/// `platform_default` returns `Unsupported` before this is ever created, but
/// tests and hosts can install it explicitly.
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
        let supported = |source| {
            spec_supported(
                &backend,
                &SurfaceSpec::new(source, Policy::strict(), "x"),
            )
        };
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
