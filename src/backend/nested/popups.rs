//! `xdg_popup` bookkeeping: menus and `<select>` dropdowns.

use crate::spec::SurfaceId;
use smithay::reexports::wayland_server::protocol::wl_buffer::WlBuffer;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point as SPoint;
use smithay::utils::Rectangle;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::Size;
use smithay::wayland::shell::xdg::PopupSurface;
use smithay::wayland::shell::xdg::PositionerState;
use smithay::wayland::shell::xdg::XdgPopupSurfaceData;

use super::buffers::{CommittedBuffer, HeldBuffer, content_geometry};
use super::compositor::{NestedState, SurfaceRole, surface_role};
use super::lock;
use crate::debug::debug_log;

/// The constrained geometry of a popup of `parent`: `(block, parent origin,
/// geometry)`, fitted into the page's content rect. Shared by map, reposition
/// and the reactive re-constrain.
pub(super) fn popup_geometry(
    state: &NestedState,
    parent: &WlSurface,
    positioner: &PositionerState,
) -> Option<(SurfaceId, (i32, i32), Rectangle<i32, Logical>)> {
    let (block, origin) = state.popup_owner(parent)?;
    let page = state.content_size(block);
    // The geometry is relative to the parent, so the page is too (a submenu's
    // parent sits at `origin`).
    let target = Rectangle::new(SPoint::from((-origin.0, -origin.1)), Size::from(page));
    let unconstrained = positioner.get_unconstrained_geometry(target);
    let geometry = fit_into(unconstrained, target);
    if geometry != unconstrained {
        debug_log!("popup forced into the page: {unconstrained:?} → {geometry:?}");
    }
    Some((block, origin, geometry))
}

/// What the client's `constraint_adjustment` left outside `target`, forced in:
/// shrunk to fit, then slid inside. The app paints a popup clipped to its
/// block like the page, so overflow would only be cut off; a GTK menu
/// configured smaller than it asked scrolls instead.
fn fit_into(
    geometry: Rectangle<i32, Logical>,
    target: Rectangle<i32, Logical>,
) -> Rectangle<i32, Logical> {
    if target.size.w <= 0 || target.size.h <= 0 {
        return geometry;
    }
    let size = Size::from((
        geometry.size.w.min(target.size.w),
        geometry.size.h.min(target.size.h),
    ));
    let loc = SPoint::from((
        geometry
            .loc
            .x
            .clamp(target.loc.x, target.loc.x + target.size.w - size.w),
        geometry
            .loc
            .y
            .clamp(target.loc.y, target.loc.y + target.size.h - size.h),
    ));
    Rectangle::new(loc, size)
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
    /// The client asked for an explicit grab (`xdg_popup.grab`): a menu, which
    /// takes the keyboard while it is the topmost one.
    pub(super) grabbing: bool,
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

    /// The block whose surface (page or popup) holds the keyboard.
    pub(super) fn keyboard_block(&self) -> Option<SurfaceId> {
        match surface_role(&self.keyboard.current_focus()?)? {
            SurfaceRole::Toplevel(block) | SurfaceRole::Popup { block, .. } => Some(block),
        }
    }

    /// Hands `block`'s keyboard to its topmost mapped grabbing popup, or back
    /// to the page once none is left. Only while the block holds the
    /// keyboard: a popup never steals focus the app has not given the block.
    pub(super) fn refresh_keyboard_grab(&mut self, block: SurfaceId) {
        if self.keyboard_block() != Some(block) {
            return;
        }
        let target = self
            .popups
            .iter()
            .rev()
            .find(|popup| popup.parent == block && popup.grabbing && popup.mapped)
            .map(|popup| popup.surface.wl_surface().clone())
            .or_else(|| {
                self.mapped
                    .get(&block)
                    .map(|toplevel| toplevel.wl_surface().clone())
            });
        if self.keyboard.current_focus() == target {
            return;
        }
        debug_log!(
            "keyboard: block {block} → {}",
            match target.as_ref().and_then(surface_role) {
                Some(SurfaceRole::Popup { id, .. }) => format!("popup {id}"),
                _ => "page".to_owned(),
            }
        );
        let keyboard = self.keyboard.clone();
        keyboard.set_focus(self, target, SERIAL_COUNTER.next_serial());
    }

    /// `xdg_popup.grab`: the popup takes the keyboard once it is mapped.
    pub(super) fn grab_popup(&mut self, surface: &PopupSurface) {
        let Some(popup) = self
            .popups
            .iter_mut()
            .find(|popup| popup.surface.wl_surface() == surface.wl_surface())
        else {
            return;
        };
        popup.grabbing = true;
        let block = popup.parent;
        self.refresh_keyboard_grab(block);
    }

    /// Moves a popup to a newly constrained `geometry` under a parent at `origin`.
    pub(super) fn place_popup(
        &mut self,
        surface: &PopupSurface,
        origin: (i32, i32),
        geometry: Rectangle<i32, Logical>,
    ) {
        // T40: submenus resolve through the popup's *current* position.
        if let Some(popup) = self
            .popups
            .iter_mut()
            .find(|popup| popup.surface.wl_surface() == surface.wl_surface())
        {
            popup.position = (origin.0 + geometry.loc.x, origin.1 + geometry.loc.y);
            popup.size = (geometry.size.w, geometry.size.h);
        }
    }

    /// The page's content rect changed: re-fit `block`'s reactive popups (the
    /// ones whose positioner asked for it), parents before children.
    pub(super) fn reconstrain_popups(&mut self, block: SurfaceId) {
        let surfaces: Vec<PopupSurface> = self
            .popups
            .iter()
            .filter(|popup| popup.parent == block)
            .map(|popup| popup.surface.clone())
            .collect();
        for surface in surfaces {
            let positioner = surface.with_pending_state(|state| state.positioner);
            if !positioner.reactive {
                continue;
            }
            let Some(parent) = popup_parent(&surface) else {
                continue;
            };
            let Some((_block, origin, geometry)) = popup_geometry(self, &parent, &positioner)
            else {
                continue;
            };
            if surface.with_pending_state(|state| state.geometry) == geometry {
                continue;
            }
            surface.with_pending_state(|state| state.geometry = geometry);
            if let Err(error) = surface.send_configure() {
                debug_log!("popup re-constrain refused: {error:?}");
                continue;
            }
            debug_log!("popup of block {block} re-constrained to {geometry:?}");
            self.place_popup(&surface, origin, geometry);
        }
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
            self.refresh_keyboard_grab(block);
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
                let newly_mapped = !popup.mapped;
                popup.mapped = true;
                let block = popup.parent;
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
                drop(list);
                if newly_mapped {
                    self.refresh_keyboard_grab(block);
                }
                self.wake();
            }
            CommittedBuffer::Removed => {
                let popup = &mut self.popups[index];
                debug_log!("commit: popup {} unmapped", popup.id);
                popup.mapped = false;
                let block = popup.parent;
                // The popup surface itself stays alive; only its frame goes.
                let held = popup.held.take();
                self.release_popup_frame(id, held);
                self.refresh_keyboard_grab(block);
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
            let block = popup.parent;
            self.drop_popup(popup, false);
            self.refresh_keyboard_grab(block);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rectangle<i32, Logical> {
        Rectangle::new(SPoint::from((x, y)), Size::from((w, h)))
    }

    #[test]
    fn a_popup_inside_the_page_is_untouched() {
        let page = rect(0, 0, 320, 200);
        assert_eq!(fit_into(rect(20, 40, 100, 80), page), rect(20, 40, 100, 80));
    }

    #[test]
    fn a_popup_taller_than_the_page_is_shrunk_to_it() {
        let page = rect(0, 0, 320, 200);
        assert_eq!(fit_into(rect(20, 30, 120, 900), page), rect(20, 0, 120, 200));
    }

    #[test]
    fn a_popup_past_an_edge_slides_back_in() {
        let page = rect(0, 0, 320, 200);
        assert_eq!(fit_into(rect(280, 150, 100, 80), page), rect(220, 120, 100, 80));
        assert_eq!(fit_into(rect(-30, -10, 100, 80), page), rect(0, 0, 100, 80));
    }

    #[test]
    fn a_submenu_is_fitted_in_its_parents_coordinates() {
        // The parent menu sits at (200, 50): the page is (-200, -50) from it.
        let page = rect(-200, -50, 320, 200);
        assert_eq!(fit_into(rect(100, 0, 100, 80), page), rect(20, 0, 100, 80));
    }

    #[test]
    fn an_empty_page_leaves_the_geometry_alone() {
        assert_eq!(fit_into(rect(5, 5, 50, 50), rect(0, 0, 0, 0)), rect(5, 5, 50, 50));
    }
}
