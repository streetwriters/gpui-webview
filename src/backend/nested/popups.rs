//! `xdg_popup` bookkeeping: menus and `<select>` dropdowns.

use crate::spec::SurfaceId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point as SPoint;
use smithay::utils::Rectangle;
use smithay::utils::Size;
use smithay::wayland::shell::xdg::PopupSurface;
use smithay::wayland::shell::xdg::PositionerState;
use smithay::wayland::shell::xdg::XdgPopupSurfaceData;

use super::buffers::{CommittedBuffer, HeldBuffer, content_geometry};
use super::compositor::{NestedState, SurfaceRole, surface_role};
use super::lock;
use crate::debug::debug_log;

/// The constrained geometry of a popup of `parent`: `(block, parent origin,
/// geometry)`, fitted into the page's content rect. Shared by map and reposition.
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

/// One popup's app-facing frame plus the block it belongs to; list order is
/// paint order.
#[derive(Clone)]
pub(crate) struct SharedPopup {
    pub(super) parent: SurfaceId,
    pub(super) frame: crate::spec::PopupFrame,
}

/// A live `xdg_popup` above one of our toplevels, committed like any surface.
pub(super) struct Popup {
    pub(super) id: u64,
    /// The block (toplevel) it belongs to; submenus share it.
    pub(super) parent: SurfaceId,
    pub(super) surface: PopupSurface,
    /// Top-left in the page's own coordinates, composed through popup chains.
    pub(super) position: (i32, i32),
    /// The geometry we configured: the hit-test and paint rect.
    pub(super) size: (i32, i32),
    /// Set once the client commits a buffer; before that there is nothing to paint.
    pub(super) mapped: bool,
    /// The previous dma-buf buffer, held back one commit (T14).
    pub(super) held: HeldBuffer,
}

impl NestedState {
    /// The block a popup's parent belongs to plus the parent's origin in page
    /// coordinates.
    pub(super) fn popup_owner(&self, parent: &WlSurface) -> Option<(SurfaceId, (i32, i32))> {
        match surface_role(parent)? {
            SurfaceRole::Toplevel(id) => Some((id, (0, 0))),
            SurfaceRole::Popup { id, block } => {
                let position = self.popups.iter().find(|popup| popup.id == id)?.position;
                Some((block, position))
            }
        }
    }

    /// The popup of `block` under a page-local point, topmost first, with its origin.
    pub(super) fn popup_at(
        &self,
        block: SurfaceId,
        x: f64,
        y: f64,
    ) -> Option<(WlSurface, (i32, i32))> {
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
            .map(|popup| (popup.surface.wl_surface().clone(), popup.position))
    }

    /// Whether `block` has any popup at all.
    pub(super) fn has_popups(&self, block: SurfaceId) -> bool {
        self.popups.iter().any(|popup| popup.parent == block)
    }

    /// Dismisses every popup of `block` with `popup_done`; held buffers and frames go.
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

    /// A popup's commit: same transports as a toplevel, a different sink.
    pub(super) fn commit_popup(&mut self, id: u64, surface: &WlSurface) {
        let Some(index) = self.popups.iter().position(|popup| popup.id == id) else {
            return;
        };
        // T22: GTK3 menus commit a shadow margin that is buffer but not content.
        // Keep the whole buffer, paint it shifted by the geometry origin, hit-test the content rect.
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
                if !popup.mapped {
                    debug_log!(
                        "commit: popup {} mapped {}x{} at ({}, {})",
                        popup.id,
                        frame.size.0,
                        frame.size.1,
                        popup.position.0,
                        popup.position.1
                    );
                }
                popup.mapped = true;
                if let Some(buffer) = hold {
                    popup.held.hold(buffer);
                }
                let shared = SharedPopup {
                    parent: popup.parent,
                    frame: crate::spec::PopupFrame {
                        id: popup.id,
                        // The frame includes the shadow; shift it so content lands at the origin.
                        position: (popup.position.0 - shadow.0, popup.position.1 - shadow.1),
                        frame,
                    },
                };
                let mut list = lock(&self.shared.popups);
                match list
                    .iter_mut()
                    .find(|entry| entry.frame.id == shared.frame.id)
                {
                    Some(slot) => *slot = shared,
                    None => list.push(shared),
                }
            }
            CommittedBuffer::Removed => {
                let popup = &mut self.popups[index];
                debug_log!("commit: popup {} unmapped", popup.id);
                popup.mapped = false;
                // The popup surface itself stays alive; only its frame goes.
                let held = popup.held.take();
                self.release_popup_frame(id, held);
            }
            CommittedBuffer::Unchanged => {}
            CommittedBuffer::Rejected => {}
        }

        self.fire_frame_callbacks(surface);
    }

    /// Forgets a popup the client destroyed: no `popup_done`, just the bookkeeping.
    pub(super) fn forget_popup(&mut self, id: u64) {
        if let Some(index) = self.popups.iter().position(|popup| popup.id == id) {
            let popup = self.popups.remove(index);
            debug_log!("popup {id} destroyed by the client");
            self.refresh_pointer_focus();
            self.drop_popup(popup, false);
        }
    }

    /// Releases a popup's held buffer and drops its frame (an unmapping commit too).
    pub(super) fn release_popup_frame(&mut self, id: u64, held: Option<WlBuffer>) {
        if let Some(buffer) = held {
            buffer.release();
        }
        lock(&self.shared.popups).retain(|entry| entry.frame.id != id);
    }

    /// Forgets a popup entirely: `popup_done` when `dismiss`, pointer leave, buffer,
    /// frame. The one popup teardown path.
    pub(super) fn drop_popup(&mut self, mut popup: Popup, dismiss: bool) {
        if dismiss {
            popup.surface.send_popup_done();
        }
        self.output.leave(popup.surface.wl_surface());
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
