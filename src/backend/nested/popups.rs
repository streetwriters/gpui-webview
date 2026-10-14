//! `xdg_popup` bookkeeping: menus, `<select>` dropdowns and submenus — their
//! placement, painting and teardown.

use super::*;
use crate::debug::debug_log;
use super::buffers::{content_geometry, CommittedBuffer, HeldBuffer};
use super::compositor::{surface_role, NestedState, SurfaceRole};

/// The constrained geometry of a popup of `parent`, in the page's coordinates:
/// `(block, parent origin, geometry)`. The positioner is relative to the
/// parent's window geometry, and the page's content rect is what the popup must
/// fit into (a menu near the block's bottom edge slides up instead of hanging
/// below the block). `new_popup` and `reposition_request` both go through it,
/// so the first configure and a re-anchor cannot drift.
pub(super) fn popup_geometry(
    state: &NestedState,
    parent: &WlSurface,
    positioner: &PositionerState,
) -> Option<(SurfaceId, (i32, i32), Rectangle<i32, Logical>)> {
    let (block, origin) = state.popup_owner(parent)?;
    let page = state.content_size(block);
    let target = Rectangle::new(SPoint::from((0, 0)), Size::from(page));
    let geometry = positioner.get_unconstrained_geometry(target);
    Some((block, origin, geometry))
}

/// One popup's frame as the app sees it. Popups are few and their order in the
/// list is paint order (bottom to top), so this is a `Vec`, not a map.
#[derive(Clone)]
pub(super) struct SharedPopup {
    pub(super) id: u64,
    pub(super) parent: SurfaceId,
    pub(super) position: (i32, i32),
    pub(super) frame: SurfaceFrame,
}

/// A live `xdg_popup` above one of our toplevels: a context menu, a `<select>`
/// dropdown, a date picker. The client commits it like any other surface (shm
/// or dma-buf) and the app paints it at `position` over the page's frame.
pub(super) struct Popup {
    pub(super) id: u64,
    /// The block (toplevel) it belongs to — submenus belong to the same block.
    pub(super) parent: SurfaceId,
    pub(super) surface: PopupSurface,
    pub(super) wl_surface: WlSurface,
    /// Top-left in the page's own coordinates, composed through popup-of-popup
    /// chains.
    pub(super) position: (i32, i32),
    /// The geometry we configured: the hit-test and paint rect.
    pub(super) size: (i32, i32),
    /// Set once the client has committed a buffer (before that there are no
    /// pixels to paint and nothing to hit).
    pub(super) mapped: bool,
    /// The previous dma-buf buffer, held back one commit (see `commit`).
    pub(super) held: HeldBuffer,
}


impl NestedState {
    /// The block a popup's *parent* (its `xdg_surface`) belongs to, plus the
    /// parent's origin in the page's coordinates: `(0, 0)` for a toplevel, the
    /// parent popup's own position for a submenu. The role is stored on the
    /// surface, so this is a lookup rather than a scan.
    pub(super) fn popup_owner(&self, parent: &WlSurface) -> Option<(SurfaceId, (i32, i32))> {
        match surface_role(parent)? {
            SurfaceRole::Toplevel(id) => Some((id, (0, 0))),
            SurfaceRole::Popup { block, position, .. } => Some((block, position)),
        }
    }


    /// The popup of `block` under the given page-local point, topmost first,
    /// with its origin (event coordinates are translated by it).
    pub(super) fn popup_at(&self, block: SurfaceId, x: f64, y: f64) -> Option<(WlSurface, (i32, i32))> {
        self.popups
            .iter()
            .rev()
            .find(|popup| {
                popup.parent == block
                    && popup.mapped
                    && x >= f64::from(popup.position.0)
                    && x < f64::from(popup.position.0 + popup.size.0)
                    && y >= f64::from(popup.position.1)
                    && y < f64::from(popup.position.1 + popup.size.1)
            })
            .map(|popup| (popup.wl_surface.clone(), popup.position))
    }


    /// Whether `block` has any popup at all.
    pub(super) fn has_popups(&self, block: SurfaceId) -> bool {
        self.popups.iter().any(|popup| popup.parent == block)
    }


    /// Dismisses every popup of `block` (menus, their submenus, dropdowns):
    /// the client is told with `popup_done`, the frame disappears from what the
    /// app paints, and the buffer it held is released.
    pub(super) fn dismiss_popups(&mut self, block: SurfaceId) {
        let mut kept = Vec::new();
        let mut dismissed = Vec::new();
        for popup in std::mem::take(&mut self.popups) {
            if popup.parent == block {
                dismissed.push(popup);
            } else {
                kept.push(popup);
            }
        }
        self.popups = kept;
        let had_popups = !dismissed.is_empty();
        for popup in dismissed {
            debug_log!("popup {} of block {block} dismissed", popup.id);
            self.drop_popup(popup, true);
        }
        if had_popups {
            self.refresh_pointer_focus();
        }
    }


    /// A popup's commit: same transports as a toplevel, but the frame goes into
    /// the shared popup list the app paints from.
    pub(super) fn commit_popup(&mut self, id: u64, surface: &WlSurface) {
        let Some(index) = self.popups.iter().position(|popup| popup.id == id) else {
            return;
        };
        // A popup can set a window geometry too: GTK3 menus carry shadow
        // margins that are part of the committed buffer but are not content.
        // Cropping a tiled dma-buf needs a blit, so the whole buffer is kept
        // and painted shifted by the geometry origin; the hit-test rect stays
        // the configured content rect (`position`/`size`).
        let geometry = content_geometry(surface);
        let shadow = geometry
            .map(|rect| (rect.loc.x, rect.loc.y))
            .unwrap_or((0, 0));
        self.commit_sequence += 1;
        let sequence = self.commit_sequence;
        let committed = self.take_committed(surface, None, sequence);
        match committed {
            CommittedBuffer::New { frame, hold } => {
                let popup = &mut self.popups[index];
                debug_log!(
                    "commit: popup {} {}x{} at ({}, {}) shadow ({}, {})",
                    popup.id,
                    frame.size.0,
                    frame.size.1,
                    popup.position.0,
                    popup.position.1,
                    shadow.0,
                    shadow.1
                );
                popup.mapped = true;
                if let Some(buffer) = hold {
                    popup.held.hold(buffer);
                }
                let shared = SharedPopup {
                    id: popup.id,
                    parent: popup.parent,
                    // The frame includes the shadow margin; position it so the
                    // content lands at the configured content origin.
                    position: (
                        popup.position.0 - shadow.0,
                        popup.position.1 - shadow.1,
                    ),
                    frame,
                };
                let mut list = lock(&self.popups_shared);
                match list.iter_mut().find(|entry| entry.id == shared.id) {
                    Some(slot) => *slot = shared,
                    None => list.push(shared),
                }
            }
            CommittedBuffer::Removed => {
                let popup = &mut self.popups[index];
                debug_log!("commit: popup {} unmapped", popup.id);
                popup.mapped = false;
                // The popup surface itself stays alive (a later commit can
                // remap it); only its frame goes away.
                let held = popup.held.take();
                self.release_popup_frame(id, held);
            }
            CommittedBuffer::Unchanged => {}
            CommittedBuffer::Rejected => {
                debug_log!("commit: unusable popup buffer; keeping the last frame")
            }
        }

        self.fire_frame_callbacks(surface);
    }


    /// Forgets one popup whose surface the client destroyed: no `popup_done`
    /// (the surface is gone), just the bookkeeping.
    pub(super) fn forget_popup(&mut self, id: u64) {
        if let Some(index) = self.popups.iter().position(|popup| popup.id == id) {
            let popup = self.popups.remove(index);
            debug_log!("popup {id} destroyed by the client");
            self.refresh_pointer_focus();
            self.drop_popup(popup, false);
        }
    }


    /// Releases a popup's held buffer and drops its app-facing frame. Shared by
    /// the full teardown (`drop_popup`) and an unmapping null-buffer commit,
    /// which keeps the popup surface alive.
    pub(super) fn release_popup_frame(&mut self, id: u64, held: Option<WlBuffer>) {
        if let Some(buffer) = held {
            buffer.release();
        }
        lock(&self.popups_shared).retain(|entry| entry.id != id);
    }


    /// Forgets a popup entirely: `popup_done` when the client is still there
    /// (`dismiss`), the pointer leave, the held buffer and the shared frame.
    /// The one popup teardown path (`dismiss_popups`, `forget_popup`).
    pub(super) fn drop_popup(&mut self, mut popup: Popup, dismiss: bool) {
        if dismiss {
            popup.surface.send_popup_done();
        }
        self.output.leave(&popup.wl_surface);
        let held = popup.held.take();
        self.release_popup_frame(popup.id, held);
    }

}

/// The surface an `xdg_popup` hangs off (set from `xdg_surface.get_popup`).
pub(super) fn popup_parent(surface: &PopupSurface) -> Option<WlSurface> {
    smithay::wayland::compositor::with_states(surface.wl_surface(), |states| {
        states
            .data_map
            .get::<XdgPopupSurfaceData>()
            .and_then(|data| data.lock().ok().and_then(|data| data.parent.clone()))
    })
}
