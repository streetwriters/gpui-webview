//! Frames ready to paint: the crate imports dma-buf content (per client buffer,
//! through the window's renderer) and paints it, so an application never
//! touches `ExternalTexture` or `SurfaceContent` itself.

#[cfg(dmabuf)]
use gpui_kit::ContentMask;
use gpui_kit::{Bounds, Corners, Pixels, Window};

use crate::spec::{SurfaceContent, SurfaceFrame};

/// A committed frame and, on the dma-buf build, the GPU texture imported for
/// it. Stored together so a paint pass cannot pair a frame with another's
/// texture.
#[derive(Clone)]
pub struct ReadyFrame {
    pub(crate) frame: SurfaceFrame,
    #[cfg(dmabuf)]
    pub(crate) texture: Option<gpui_kit::ExternalTexture>,
}

impl ReadyFrame {
    /// The committed size in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.frame.size
    }

    /// Whether two ready frames are the same client commit.
    pub fn same_as(&self, other: &Self) -> bool {
        self.frame.same_as(&other.frame)
    }

    /// The raw committed frame (content + size).
    pub fn frame(&self) -> &SurfaceFrame {
        &self.frame
    }
}

impl std::fmt::Debug for ReadyFrame {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.frame.fmt(f)
    }
}

/// A native popup (a context menu, a `<select>` dropdown) ready to paint above
/// the page at `position` (the page's own coordinates).
#[derive(Clone)]
pub struct ReadyPopup {
    /// Stable while the popup lives; keys its import-cache slot.
    pub id: u64,
    /// The popup's top-left in the page's coordinate space.
    pub position: (i32, i32),
    /// The popup's pixels.
    pub frame: ReadyFrame,
}

impl ReadyPopup {
    /// Whether two popup states are the same popup, place and commit.
    pub fn same_as(&self, other: &Self) -> bool {
        self.id == other.id && self.position == other.position && self.frame.same_as(&other.frame)
    }
}

impl std::fmt::Debug for ReadyPopup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReadyPopup")
            .field("id", &self.id)
            .field("position", &self.position)
            .field("size", &self.frame.size())
            .finish()
    }
}

/// Paints a ready frame at `rect`, clipped to `clip`, with `radius` corners.
///
/// An image frame is copied by the renderer; a dma-buf frame's imported texture
/// is sampled at the full rect under a content mask (painting it into a smaller
/// rect would squash it).
pub fn paint_frame(
    window: &mut Window,
    frame: &ReadyFrame,
    rect: Bounds<Pixels>,
    clip: Bounds<Pixels>,
    radius: Pixels,
) {
    match &frame.frame.content {
        SurfaceContent::Image(image) => {
            let _ = window.paint_image(clip, rect, Corners::all(radius), image.clone(), 0, false);
        }
        #[cfg(dmabuf)]
        SurfaceContent::Dmabuf(_) => {
            if let Some(texture) = &frame.texture {
                window.with_content_mask(Some(ContentMask { bounds: clip }), |window| {
                    window.paint_external_texture(rect, texture.clone());
                });
            }
        }
    }
}
