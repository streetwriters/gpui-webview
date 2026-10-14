//! macOS: a `WKWebView` inside a layer-backed clip container under GPUI's
//! `NSView` (EMBEDS.md §10.1).
//!
//! Why a container: GPUI cannot clip a live platform view to the scroll
//! viewport. The container (`wantsLayer + masksToBounds`) provides clipping,
//! and the webview is positioned inside it so only the visible part of the
//! block shows. The webview itself always keeps the block's full size, so
//! scrolling never relayouts the page; only the container's frame and the
//! webview's offset change.
//!
//! Overlays: full parity needs the transparent-hole pattern (GPUI paints a
//! hole and the webview shows through). Until that lands, an intersecting
//! overlay hides the surface (freeze) so GPUI menus and carets are never
//! occluded.

use std::sync::{Arc, Mutex};

use gpui_kit::Window;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{MainThreadMarker, MainThreadOnly, msg_send, rc::autoreleasepool};
use objc2_app_kit::NSView;
use objc2_foundation::{NSPoint, NSRect, NSSize};
use objc2_web_kit::WKWebViewConfiguration;
use wry::raw_window_handle::{
    AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle,
};

use super::NativeSurface;
use crate::spec::{Error, Placement, Result, SurfaceSpec, WebViewEvent};

/// GPUI's view that hosts the surfaces, plus the coordinates needed to map
/// window-space rects into its space.
#[derive(Clone)]
pub(super) struct ParentView {
    view: Retained<NSView>,
    /// The parent's bounds height at attach time (kept for diagnostics; the
    /// coordinate conversion reads the live bounds instead).
    height: f64,
    /// Whether the parent view uses top-left origin coordinates at attach time
    /// (the conversion re-reads it live).
    flipped: bool,
}

impl ParentView {
    pub(super) fn from_window(window: &Window) -> Result<Self> {
        // `Window` has an inherent `window_handle` returning GPUI's own
        // handle type, so the raw-window-handle trait is called explicitly.
        let raw = HasWindowHandle::window_handle(window)
            .map_err(|error| Error::Platform(format!("window handle: {error}")))?;
        let RawWindowHandle::AppKit(handle) = raw.as_raw() else {
            return Err(Error::Platform("not an AppKit window".into()));
        };
        // SAFETY: GPUI's window handle is a live NSView for the window's
        // lifetime; the view is retained here, and the surface is torn down
        // before the window closes.
        let view = unsafe { Retained::retain(handle.ns_view.as_ptr() as *mut NSView) }
            .ok_or_else(|| Error::Platform("null NSView".into()))?;
        let bounds: NSRect = unsafe { msg_send![&*view, bounds] };
        let flipped: bool = unsafe { msg_send![&*view, isFlipped] };
        Ok(Self {
            view,
            height: bounds.size.height,
            flipped,
        })
    }

    /// Makes GPUI's view the window's first responder again.
    ///
    /// GPUI's `FocusHandle::focus` only updates GPUI's own focus state; it
    /// never calls `makeFirstResponder`. Once a native child view (the
    /// WKWebView) has taken first responder, keyboard events therefore stop
    /// reaching GPUI entirely — typing into the editor would do nothing. Every
    /// path that hands keyboard focus back to the editor must call this.
    pub(super) fn make_first_responder(&self) {
        let window: *mut AnyObject = unsafe { msg_send![&*self.view, window] };
        if window.is_null() {
            return;
        }
        let _: bool = unsafe { msg_send![window, makeFirstResponder: &*self.view] };
    }

    /// A window-space (top-left origin) rect in the parent's coordinates.
    ///
    /// The parent's bounds are read live: a window resize between frames would
    /// otherwise leave the bottom-left-origin conversion using a stale height
    /// and draw every surface at the wrong y.
    fn map_rect(&self, x: f64, y: f64, width: f64, height: f64) -> NSRect {
        let bounds: NSRect = unsafe { msg_send![&*self.view, bounds] };
        let flipped: bool = unsafe { msg_send![&*self.view, isFlipped] };
        let _ = self.flipped;
        let _ = self.height;
        let y = if flipped {
            y
        } else {
            bounds.size.height - (y + height)
        };
        NSRect::new(NSPoint::new(x, y), NSSize::new(width, height))
    }
}

/// A handle wry can build a child into: the container view we create.
struct ContainerHandle(*mut std::ffi::c_void);

impl HasWindowHandle for ContainerHandle {
    fn window_handle(&self) -> std::result::Result<WindowHandle<'_>, HandleError> {
        let ns_view = std::ptr::NonNull::new(self.0).ok_or(HandleError::Unavailable)?;
        let handle = AppKitWindowHandle::new(ns_view);
        Ok(unsafe { WindowHandle::borrow_raw(RawWindowHandle::AppKit(handle)) })
    }
}

/// Creates a live macOS surface: container + webview, policy applied.
pub(super) fn create_surface(
    id: crate::SurfaceId,
    spec: &SurfaceSpec,
    parent: &ParentView,
    events: &Arc<Mutex<Vec<WebViewEvent>>>,
) -> Result<NativeSurface> {
    // The clip container.
    let mtm = MainThreadMarker::new().ok_or_else(|| Error::Platform("not the main thread".into()))?;
    let frame = NSRect::new(NSPoint::new(0., 0.), NSSize::new(1., 1.));
    let container = NSView::initWithFrame(NSView::alloc(mtm), frame);
    container.setWantsLayer(true);
    // `masksToBounds` on the backing layer is the clip. Sent directly so the
    // crate needs no QuartzCore bindings.
    let layer: *mut AnyObject = unsafe { msg_send![&*container, layer] };
    if !layer.is_null() {
        let _: () = unsafe { msg_send![layer, setMasksToBounds: true] };
    }
    parent.view.addSubview(&container);

    let builder = build_builder(id, spec, events)?;
    let container_handle = ContainerHandle(Retained::as_ptr(&container) as *mut std::ffi::c_void);
    let webview = autoreleasepool(|_| {
        builder
            .build_as_child(&container_handle)
            .map_err(|error| Error::Platform(format!("wry: {error}")))
    })?;
    let _ = &webview;
    Ok(NativeSurface {
        webview,
        container: Some(container),
        lowered: false,
    })
}

/// Applies geometry/visibility through the container + webview frames.
///
/// The container is the visible clip and lives in the parent's coordinates;
/// the webview keeps the block's full size and is offset inside the container,
/// so the page never relayouts while scrolling.
pub(super) fn place(
    surface: &mut NativeSurface,
    at: Placement,
    parent: &ParentView,
) {
    // A lowered surface needs the transparent hole; until that lands, hide it
    // so GPUI overlays are never occluded.
    let visible = at.visible && !at.overlay;
    let Some(container) = &surface.container else {
        let _ = surface.webview.set_visible(visible);
        return;
    };
    if !visible {
        let _ = surface.webview.set_visible(false);
        unsafe {
            let _: () = msg_send![&**container, setHidden: true];
        }
        return;
    }
    let frame = parent.map_rect(
        at.clip.origin.x.as_f32() as f64,
        at.clip.origin.y.as_f32() as f64,
        at.clip.size.width.as_f32() as f64,
        at.clip.size.height.as_f32() as f64,
    );
    unsafe {
        let _: () = msg_send![&**container, setFrame: frame];
        let _: () = msg_send![&**container, setHidden: false];
    }
    let offset_x = at.clip.origin.x - at.rect.origin.x;
    let offset_y = at.clip.origin.y - at.rect.origin.y;
    let _ = surface.webview.set_bounds(wry::Rect {
        position: wry::dpi::LogicalPosition::new(
            -offset_x.as_f32() as f64,
            -offset_y.as_f32() as f64,
        )
        .into(),
        size: wry::dpi::LogicalSize::new(
            at.rect.size.width.as_f32() as f64,
            at.rect.size.height.as_f32() as f64,
        )
        .into(),
    });
    let _ = surface.webview.set_visible(true);
    surface.lowered = false;
}

/// Re-applies the parts of a policy that can change on navigation. The script
/// switch is read by WebKit at navigation time, so toggling it here affects
/// the *next* load.
pub(super) fn apply_policy(webview: &wry::WebView, spec: &SurfaceSpec) {
    use wry::WebViewExtMacOS;
    let view = webview.webview();
    let configuration = unsafe { view.configuration() };
    let preferences = unsafe { configuration.defaultWebpagePreferences() };
    unsafe {
        preferences.setAllowsContentJavaScript(spec.policy.scripts);
    }
}

/// Builds the wry builder with every policy the macOS backend can enforce.
fn build_builder(
    id: crate::SurfaceId,
    spec: &SurfaceSpec,
    events: &Arc<Mutex<Vec<WebViewEvent>>>,
) -> Result<wry::WebViewBuilder<'static>> {
    let navigation_events = events.clone();
    let policy = spec.policy.clone();
    use wry::{WebViewBuilderExtDarwin as _, WebViewBuilderExtMacos as _};
    let mtm = MainThreadMarker::new();
    let mut builder = wry::WebViewBuilder::new()
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
                }
                false
            }
            crate::spec::Navigation::Block => false,
        })
        .with_on_web_content_process_terminate_handler({
            let events = events.clone();
            move || {
                if let Ok(mut events) = events.lock() {
                    events.push(WebViewEvent::Crashed { id });
                }
            }
        });
    if spec.policy.storage == crate::spec::Storage::Ephemeral {
        builder = builder.with_incognito(true);
    }
    // JavaScript off for clips: `WKWebpagePreferences` on a configuration we
    // hand to wry (it attaches its own handlers to it).
    if !spec.policy.scripts
        && let Some(mtm) = mtm
    {
        let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
        let preferences = unsafe { configuration.defaultWebpagePreferences() };
        unsafe {
            preferences.setAllowsContentJavaScript(false);
        }
        builder = builder.with_webview_configuration(configuration);
    }
    builder = match &spec.source {
        crate::spec::Source::Url(url) => builder.with_url(url.clone()),
        crate::spec::Source::Html { html, .. } => builder.with_html(html.clone()),
        crate::spec::Source::Local { .. } => {
            return Err(Error::Platform(
                "local sources need a WKURLSchemeHandler (phase 4)".into(),
            ));
        }
    };
    Ok(builder)
}
