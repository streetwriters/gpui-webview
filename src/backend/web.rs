//! The wasm backend (EMBEDS.md §11): a clip `<div>` + `<iframe>` stacked over
//! GPUI's canvas.
//!
//! GPUI draws into one `<canvas>` appended to `body`; a live surface is a
//! `position:fixed` clip div holding the `<iframe>`. The iframe is created
//! once per surface and **never reparented** (the DOM-overlay rule from the
//! research): scrolling moves it with `transform: translate()`, clipping
//! changes the div's size and the iframe's offset inside it, and teardown
//! removes the nodes and revokes any blob URL.
//!
//! Overlays: by default the iframe sits above the canvas, so it is
//! interactive. When a GPUI overlay intersects it, the backend flips the
//! clip's `z-index` below the canvas and asks GPUI to paint a transparent hole
//! (the app's business); the page stays live and visible, it just is not
//! clickable until the popup closes.

use std::collections::HashMap;

use gpui::Window;
use wasm_bindgen::JsCast;

use crate::host::WebViewBackend;
use crate::spec::{
    Error, Placement, Presentation, Result, Source, SurfaceId, SurfaceSpec, WebViewEvent,
};

/// One live DOM surface.
struct WebSurface {
    /// The clipping container.
    clip: web_sys::HtmlElement,
    /// The iframe inside it.
    frame: web_sys::HtmlIFrameElement,
    /// A blob URL to revoke on teardown (web clips loaded from bytes).
    blob: Option<String>,
    /// Last applied placement, so unchanged frames are skipped.
    last: Option<Placement>,
}

/// The wasm backend. One per window.
pub struct WebBackend {
    surfaces: HashMap<SurfaceId, WebSurface>,
    events: Vec<WebViewEvent>,
    /// The z-index the canvas is assumed to have; surfaces above it are
    /// interactive, lowered ones go below. The app must give its canvas this
    /// z-index when it wants the transparent-hole overlay pattern
    /// (`WebBackend::with_canvas_z`).
    canvas_z: i32,
}

impl Default for WebBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WebBackend {
    pub fn new() -> Self {
        Self {
            surfaces: HashMap::new(),
            events: Vec::new(),
            canvas_z: 0,
        }
    }

    /// The z-index the host gave GPUI's canvas. With the hole pattern the app
    /// sets it explicitly (e.g. 0) and lowered surfaces go just below it.
    pub fn with_canvas_z(mut self, canvas_z: i32) -> Self {
        self.canvas_z = canvas_z;
        self
    }

    fn document() -> Result<web_sys::Document> {
        web_sys::window()
            .and_then(|window| window.document())
            .ok_or_else(|| Error::Platform("no document".into()))
    }

    /// The sandbox attribute for a policy: `allow-scripts`/`allow-same-origin`
    /// only when granted, plus popups so links behave like the product's
    /// (which the navigation policy then reroutes).
    fn sandbox_for(spec: &SurfaceSpec) -> String {
        spec.policy
            .sandbox_features()
            .join(" ")
    }

    fn apply_attributes(frame: &web_sys::HtmlIFrameElement, spec: &SurfaceSpec) {
        let _ = frame.set_attribute("sandbox", &Self::sandbox_for(spec));
        let _ = frame.set_attribute("allow", &spec.policy.allow.join("; "));
        let _ = frame.set_attribute("allowfullscreen", "");
        let _ = frame.set_attribute("referrerpolicy", "origin");
        let _ = frame.set_attribute("title", &spec.title);
        let _ = frame.set_attribute("frameborder", "0");
        // The iframe fills its container; the container clips.
        let style = frame.style();
        let _ = style.set_property("width", "100%");
        let _ = style.set_property("height", "100%");
        let _ = style.set_property("border", "0");
        let _ = style.set_property("display", "block");
        let _ = style.set_property("background", "#ffffff");
        // A referrer is set through the loader URL? Browsers ignore a custom
        // Referer header from `fetch`; the embed provider receives the page's
        // own referrer policy. `corsHost` rewrites the URL before it gets here.
        let _ = frame.set_attribute("loading", "eager");
    }

    /// Builds the source URL for a surface. `Source::Html` is materialised
    /// into a blob URL, which the caller must revoke on navigate/teardown.
    fn source_url(&mut self, source: &Source) -> Result<(String, Option<String>)> {
        match source {
            Source::Url(url) => Ok((url.clone(), None)),
            Source::Html { html, .. } => {
                let (url, blob) = self.blob_url(html.as_bytes(), "text/html")?;
                Ok((url, Some(blob)))
            }
        }
    }

    /// A blob URL for in-memory content. Always revoked on navigate/teardown
    /// (the production web clip never revokes its blob URL; this does).
    fn blob_url(&self, bytes: &[u8], mime: &str) -> Result<(String, String)> {
        let chunk = js_sys::Uint8Array::from(bytes);
        let parts = js_sys::Array::new();
        parts.push(&chunk.buffer());
        let options = web_sys::BlobPropertyBag::new();
        options.set_type(mime);
        let blob =
            web_sys::Blob::new_with_u8_array_sequence_and_options(&parts, &options)
                .map_err(|error| Error::Platform(format!("blob: {error:?}")))?;
        let url = web_sys::Url::create_object_url_with_blob(&blob)
            .map_err(|error| Error::Platform(format!("blob url: {error:?}")))?;
        Ok((url.clone(), url))
    }
}

impl WebViewBackend for WebBackend {
    fn name(&self) -> &'static str {
        "web"
    }

    fn attach(&mut self, _window: &Window) -> Result<()> {
        // The canvas is already in the document; nothing to bind.
        Ok(())
    }

    fn create(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        let document = Self::document()?;
        let body = document
            .body()
            .ok_or_else(|| Error::Platform("no body".into()))?;

        let clip = document
            .create_element("div")
            .map_err(|error| Error::Platform(format!("create div: {error:?}")))?
            .dyn_into::<web_sys::HtmlElement>()
            .map_err(|_| Error::Platform("div is not an element".into()))?;
        let style = clip.style();
        let _ = style.set_property("position", "fixed");
        let _ = style.set_property("left", "0");
        let _ = style.set_property("top", "0");
        let _ = style.set_property("overflow", "hidden");
        let _ = style.set_property("z-index", &(self.canvas_z + 1).to_string());
        let _ = style.set_property("pointer-events", "auto");
        let _ = style.set_property("will-change", "transform");
        let _ = style.set_property("display", "none");

        let frame = document
            .create_element("iframe")
            .map_err(|error| Error::Platform(format!("create iframe: {error:?}")))?
            .dyn_into::<web_sys::HtmlIFrameElement>()
            .map_err(|_| Error::Platform("iframe is not an iframe".into()))?;
        Self::apply_attributes(&frame, &spec);

        let (url, blob) = self.source_url(&spec.source)?;
        let _ = clip.append_child(&frame);
        let _ = body.append_child(&clip);
        let _ = frame.set_src(&url);

        self.surfaces.insert(
            id,
            WebSurface {
                clip,
                frame,
                blob,
                last: None,
            },
        );
        Ok(())
    }

    fn navigate(&mut self, id: SurfaceId, spec: SurfaceSpec) -> Result<()> {
        if !self.surfaces.contains_key(&id) {
            return self.create(id, spec);
        }
        let (url, blob) = self.source_url(&spec.source)?;
        let surface = self.surfaces.get_mut(&id).expect("checked above");
        Self::apply_attributes(&surface.frame, &spec);
        let _ = surface.frame.set_src(&url);
        // Revoke the previous blob *after* the new source is set, so the
        // engine has already switched away from it (the production leak is
        // fixed here: the URL is always revoked on navigate/teardown).
        if let Some(previous) = std::mem::replace(&mut surface.blob, blob) {
            let _ = web_sys::Url::revoke_object_url(&previous);
        }
        surface.last = None;
        Ok(())
    }

    fn place(&mut self, id: SurfaceId, at: Placement) {
        let Some(surface) = self.surfaces.get_mut(&id) else {
            return;
        };
        if surface.last.as_ref() == Some(&at) {
            return;
        }
        surface.last = Some(at);
        let style = surface.clip.style();
        if !at.visible {
            let _ = style.set_property("display", "none");
            return;
        }
        // The clip is the intersection of the block and the viewport: the
        // page keeps its full block size and only the visible window of it is
        // shown, so scrolling never relayouts.
        let clip = at.clip;
        let _ = style.set_property("display", "block");
        let _ = style.set_property("left", &format!("{}px", clip.origin.x.as_f32()));
        let _ = style.set_property("top", &format!("{}px", clip.origin.y.as_f32()));
        let _ = style.set_property("width", &format!("{}px", clip.size.width.as_f32()));
        let _ = style.set_property("height", &format!("{}px", clip.size.height.as_f32()));
        // The iframe stays at the block's size and is offset inside the clip,
        // so clipping changes only the viewport, never the page.
        let offset_x = clip.origin.x - at.rect.origin.x;
        let offset_y = clip.origin.y - at.rect.origin.y;
        let frame_style = surface.frame.style();
        let _ = frame_style.set_property("width", &format!("{}px", at.rect.size.width.as_f32()));
        let _ = frame_style.set_property("height", &format!("{}px", at.rect.size.height.as_f32()));
        let _ = frame_style.set_property("transform", &format!(
            "translate({}px, {}px)",
            -offset_x.as_f32(),
            -offset_y.as_f32()
        ));
        // Overlay: drop below the canvas so GPUI draws over the live page.
        // The app paints a transparent hole where the surface shows through.
        let z = if at.overlay {
            self.canvas_z - 1
        } else {
            self.canvas_z + 1
        };
        let _ = style.set_property("z-index", &z.to_string());
        let _ = style.set_property("pointer-events", if at.overlay { "none" } else { "auto" });
    }

    fn set_focus(&mut self, id: SurfaceId, focused: bool) {
        let Some(surface) = self.surfaces.get(&id) else {
            return;
        };
        if focused {
            let _ = surface.frame.focus();
        } else {
            // Blur the iframe so the DOM's focus returns to the page (the host
            // listens for keys there); blurring the container would not move
            // focus away from the frame's document.
            let _ = surface.frame.blur();
        }
    }

    fn present(&mut self, _id: SurfaceId, mode: Presentation) -> Result<()> {
        // The web host's fullscreen is the app's own presentation; a browser
        // iframe cannot be moved between documents. Inline restores.
        if mode == Presentation::Inline {
            return Ok(());
        }
        Err(Error::Unsupported)
    }

    fn destroy(&mut self, id: SurfaceId) {
        let Some(surface) = self.surfaces.remove(&id) else {
            return;
        };
        if let Some(blob) = surface.blob {
            let _ = web_sys::Url::revoke_object_url(&blob);
        }
        if let Some(parent) = surface.clip.parent_node() {
            let _ = parent.remove_child(&surface.clip);
        }
    }

    fn events(&mut self) -> Vec<WebViewEvent> {
        std::mem::take(&mut self.events)
    }
}
