//! `gpui-webview`: pooled, clipped live web surfaces for GPUI applications.
//!
//! A *surface* is a live web page placed at a rectangle in a GPUI window:
//! an inline embed, a saved web clip, a popup. This crate owns the platform
//! layer — WKWebView / WebView2 / WebKitGTK / a wasm `<iframe>` — behind one
//! small contract, and knows nothing about any application's documents. Keys
//! are opaque `u64`s; content is a URL, HTML bytes, or a host-provided local
//! source.
//!
//! # Why a pool
//!
//! A web engine is expensive (tens of MB, ~100 ms to create) and on every
//! desktop platform the live view sits in the platform's own layer stack, so
//! it cannot be freely clipped or drawn over. The [`WebViews`] manager
//! therefore keeps at most [`Config::capacity`] engines alive, reuses them by
//! *navigation* instead of recreation, clips them to the viewport, yields them
//! under GPUI overlays, and tears them down when they scroll out. Applications
//! render a cheap native poster by default and activate a live surface on
//! demand (the "poster by default, live surface on demand" pattern).
//!
//! # Example
//!
//! ```
//! use gpui_webview::{Config, Policy, Source, SurfaceSpec, WebViews};
//!
//! // In an application, `platform_default` picks the best backend for the
//! // target; in tests it is `Error::Unsupported` unless a backend feature is
//! // built in, which is exactly what the poster fallback expects.
//! let mut webviews = WebViews::new(gpui_webview::mock::MockBackend::new(), Config::default());
//! let mut spec = SurfaceSpec::new(
//!     Source::Url("https://www.youtube.com/embed/dQw4w9WgXcQ".into()),
//!     Policy::media(),
//!     "YouTube",
//! );
//! spec.policy.referrer = None; // set the host's `corsHost` origin here
//! // The page may run scripts and keep storage; the *engine* stays a viewer:
//! // `Policy::media()`'s `engine` field is all-off (no devtools, no engine
//! // context menu, no camera/microphone). Flip one on
//! // for a host that wants it, or set `GPUI_WEBVIEW_DEVTOOLS=1` to inspect a
//! // live embed while debugging the nested Linux backend.
//! spec.policy.engine.devtools = false;
//! // `activate` returns `Err(Error::Unsupported)` when no backend is
//! // available; the application then keeps its poster and opens the URL.
//! # let _ = spec;
//! ```
//!
//! # Guarantees worth testing (the contract)
//!
//! - at most `capacity` engines alive; an idle engine is reused by navigation,
//!   never recreated, for a different surface;
//! - `frame` is a no-op when nothing changed; `place` never relayouts the page
//!   (clipping changes the viewport, not the surface's size);
//! - a surface is destroyed when its block has been fully outside the viewport
//!   for a frame, and lowered or hidden when a GPUI overlay intersects it
//!   (`Config::on_overlay` picks which);
//! - `Error::Unsupported` is returned — never a panic — when the target, the
//!   session or the runtime library cannot host a surface.
//!
//! # What stays out
//!
//! No browser engine of its own, no document/editor/provider concepts, no
//! policy *decisions* (the crate enforces the [`Policy`] it is handed), and no
//! application chrome except the popups the engine cannot draw itself and the
//! separate-window presentation, both of which need the surface's own state
//! (EMBEDS.md §7).

#![deny(unsafe_op_in_unsafe_fn)]

pub mod backend;
/// The `GPUI_WEBVIEW_DEBUG` traces. Linux uses them in the nested compositor
/// and the dma-buf paths; Windows traces the surface lifecycle
/// (create/place/focus/destroy).
#[cfg(any(
    target_os = "windows",
    all(target_os = "linux", any(feature = "nested", dmabuf))
))]
mod debug;
pub mod host;
pub mod keys;
pub mod manager;
#[cfg(feature = "mock")]
pub mod mock;
mod paint;
pub mod spec;

pub use host::{UnsupportedBackend, WebViewBackend};
pub use keys::physical_key;
pub use manager::WebViews;
pub use paint::{ReadyFrame, ReadyPopup, paint_frame};
pub use spec::{
    Config, ContextMenu, Engine, Error, Navigation, OverlayBehavior, Placement, PointerButton,
    Policy, PopupFrame, Presentation, Result, Selection, Source, Storage, SurfaceContent, SurfaceFrame,
    SurfaceId, SurfaceSpec, WebInputEvent, WebViewEvent,
};
#[cfg(dmabuf)]
pub use spec::{DmaBufFrame, DmaBufPlane};

/// The crate's version, for host diagnostics.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Prepares the process for live native surfaces; call this as the first
/// thing in `main`, before any other thread exists.
///
/// On Windows with the `wry` feature, GPUI must not present through
/// DirectComposition: its target is created topmost, and a topmost DComp
/// target draws above every child HWND, so a WebView2 surface would be
/// created, receive input, and never become visible (T44). This sets
/// `GPUI_DISABLE_DIRECT_COMPOSITION=1` when it is unset; GPUI then uses a
/// flip-model swapchain and child HWNDs composite above it. GPUI reads the
/// variable once, at platform init, so setting it later does nothing.
///
/// A no-op on every other target and without the feature.
pub fn prepare_process() {
    #[cfg(all(target_os = "windows", feature = "wry"))]
    {
        if std::env::var_os("GPUI_DISABLE_DIRECT_COMPOSITION").is_none() {
            // SAFETY: this function is documented (and used) as the first
            // call in `main`. GPUI still reads the variable on the main
            // thread before any other thread can touch the environment.
            unsafe {
                std::env::set_var("GPUI_DISABLE_DIRECT_COMPOSITION", "1");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interface the composition roots depend on is callable with default
    /// features and names no editor concept.
    #[test]
    fn public_surface_is_usable_without_a_backend_feature() {
        let mut webviews = WebViews::new(
            UnsupportedBackend("test"),
            Config {
                capacity: 2,
                ..Config::default()
            },
        );
        assert_eq!(webviews.backend_name(), "test");
        assert_eq!(
            webviews.activate(
                1,
                SurfaceSpec::new(
                    Source::Url("https://example.com".into()),
                    Policy::media(),
                    "x"
                )
            ),
            Err(Error::Unsupported)
        );
    }
}
