//! Seat input: `WebInputEvent` to `wl_seat` pointer, keyboard and axis
//! events.

use super::*;
use crate::debug::debug_log;
use super::compositor::{surface_role, NestedState, SurfaceRole};

impl NestedState {
    /// Re-enters the surface under the pointer on the next event, by dropping
    /// the pointer focus first.
    ///
    /// Call this whenever a popup appears or disappears. GDK learns *where* the
    /// pointer is only from `enter`/`leave`; a popup the client destroys itself
    /// (a menu choice, a dismissed dropdown) never gets a usable `leave` — the
    /// surface is already gone, so the event arrives with a `nil` surface, which
    /// GDK's handler ignores — and GDK then keeps sending every pointer event to
    /// the dead window *forever*. Smithay only re-sends `enter` when the focus
    /// changes, so forgetting it here is what makes the next event re-enter the
    /// surface that is actually under the pointer.
    pub(super) fn refresh_pointer_focus(&mut self) {
        if self.pointer.current_focus().is_some() {
            self.release_pointer_focus();
        }
    }


    /// Sends a `leave` for the current pointer focus (a `None` focus motion).
    pub(super) fn release_pointer_focus(&mut self) {
        let pointer = self.pointer.clone();
        pointer.motion(
            self,
            None,
            &MotionEvent {
                location: SPoint::from((0.0, 0.0)),
                serial: SERIAL_COUNTER.next_serial(),
                time: self.now_ms(),
            },
        );
        pointer.frame(self);
    }


}

/// One input event on the compositor thread: translate GPUI input into
/// `wl_seat` events for the surface under the pointer — a popup when one is
/// there, the page otherwise.
pub(super) fn apply_input(state: &mut NestedState, id: SurfaceId, event: WebInputEvent) {
    let Some(toplevel) = state
        .mapped
        .get(&id)
        .map(|surface| surface.wl_surface().clone())
    else {
        debug_log!("input: no mapped surface for block {id}");
        return;
    };
    // A native popup floats above the page and gets the pointer while it is
    // over it, in its own coordinate space. A press outside every popup
    // dismisses them — how a menu closes when the user clicks the page.
    let mut target = toplevel;
    let mut origin = (0.0, 0.0);
    if let Some((x, y)) = event.pointer_position() {
        match state.popup_at(id, x, y) {
            Some((surface, position)) => {
                target = surface;
                origin = (f64::from(position.0), f64::from(position.1));
            }
            None => {
                if matches!(event, WebInputEvent::PointerButton { pressed: true, .. })
                    && state.has_popups(id)
                {
                    state.dismiss_popups(id);
                }
            }
        }
    }
    // Surface-local coordinates: the page's are its own, a popup's are relative
    // to the popup's origin.
    let (lx, ly) = match event.pointer_position() {
        Some((x, y)) => (x - origin.0, y - origin.1),
        None => (0.0, 0.0),
    };

    let serial = SERIAL_COUNTER.next_serial();
    let time = state.now_ms();
    // The handles are cloned so they can be borrowed alongside `&mut state`.
    let pointer = state.pointer.clone();
    // smithay sends `event.location - focus_location` to the client, and the
    // coordinates we forward are already surface-local (each GTK toplevel is the
    // only surface of its own connection, at the origin).
    let focus_origin = SPoint::from((0.0, 0.0));
    match event {
        WebInputEvent::PointerMotion { .. } | WebInputEvent::PointerEnter { .. } => {
            pointer.motion(
                state,
                Some((target.clone(), focus_origin)),
                &MotionEvent {
                    location: SPoint::from((lx, ly)),
                    serial,
                    time,
                },
            );
            // Wayland groups pointer events into frames, and clients (GTK,
            // WebKit) only dispatch a group when `wl_pointer.frame` arrives.
            pointer.frame(state);
            debug_log!(
                "input: motion ({lx:.0},{ly:.0}) focus={:?}",
                state.pointer.current_focus().is_some()
            );
        }
        WebInputEvent::PointerLeave => {
            // The pointer left the block's rectangle: send `wl_pointer.leave`
            // so `mouseleave`/hover/cursor state stops rather than sticking.
            // If the pointer is over another block (capacity > 1), its own
            // events will re-enter the right surface.
            let belongs = state.pointer.current_focus().is_some_and(|focused| {
                match surface_role(&focused) {
                    Some(SurfaceRole::Toplevel(block)) => block == id,
                    Some(SurfaceRole::Popup { block, .. }) => block == id,
                    None => false,
                }
            });
            if belongs {
                state.release_pointer_focus();
                debug_log!("input: pointer left block {id}");
            }
        }
        WebInputEvent::PointerButton { button, pressed, .. } => {
            let button = btn_code(button);
            pointer.motion(
                state,
                Some((target.clone(), focus_origin)),
                &MotionEvent {
                    location: SPoint::from((lx, ly)),
                    serial,
                    time,
                },
            );
            pointer.button(
                state,
                &ButtonEvent {
                    serial,
                    time,
                    button,
                    state: if pressed {
                        ButtonState::Pressed
                    } else {
                        ButtonState::Released
                    },
                },
            );
            pointer.frame(state);
            debug_log!(
                "input: button {button:#x} pressed={pressed} ({lx:.0},{ly:.0}) focus={:?}",
                state.pointer.current_focus().is_some()
            );
        }
        WebInputEvent::PointerAxis { dx, dy, .. } => {
            // GPUI's wheel convention is opposite GDK's: GPUI reports wheel
            // *down* as a negative `dy` (X11 button 5 → `Lines(0, -3)`), while
            // WebKitGTK derives `wheelTicks = -delta` from the smooth GDK
            // delta (GDK positive = down/right).
            //
            // The unit is calibrated too (E2E_WHEEL_CALIBRATE=1): GPUI's
            // `pixel_delta(px(20.))` reports one wheel tick as 60 units, but
            // GDK's Wayland smooth axis is libinput's value where one physical
            // click is 15 units (a measured 51 px on a 320x200 viewport).
            // Forwarding the raw 60 made every tick four clicks' worth of
            // scrolling; scale so one GPUI tick == one real wheel click. A
            // precise (trackpad) pixel delta lands near 1:1 scrolling.
            let mut frame = AxisFrame::new(time);
            if dy != 0. {
                frame = frame.value(
                    Axis::Vertical,
                    -dy * GDK_AXIS_UNITS_PER_GPUI_PIXEL,
                );
            }
            if dx != 0. {
                frame = frame.value(
                    Axis::Horizontal,
                    -dx * GDK_AXIS_UNITS_PER_GPUI_PIXEL,
                );
            }
            // Only move the pointer when the focus has to change (a scroll on a
            // surface the pointer is not on yet). A motion in the *same* frame
            // as the scroll makes GDK drop the scroll: its frame flush delivers
            // the queued motion and skips the accumulated scroll
            // (`gdk_wayland_seat_flush_frame_event`). A wheel that moves the
            // mouse every tick would then scroll nothing — and a *first* tick
            // would be silently lost, so the motion gets its own frame before
            // the axis event.
            if state.pointer.current_focus().as_ref() != Some(&target) {
                pointer.motion(
                    state,
                    Some((target.clone(), focus_origin)),
                    &MotionEvent {
                        location: SPoint::from((lx, ly)),
                        serial,
                        time,
                    },
                );
                pointer.frame(state);
            }
            pointer.axis(state, frame);
            pointer.frame(state);
            debug_log!("input: axis ({lx:.0},{ly:.0}) dy={dy} dx={dx}");
        }
        WebInputEvent::Key { keystroke, pressed } => {
            let Some(code) = crate::keys::physical_key(&keystroke) else {
                debug_log!(
                    "input: no keycode for {:?} (char {:?}); dropped",
                    keystroke.key,
                    keystroke.key_char
                );
                return;
            };
            // The keystroke carries the modifier state at press time. When the
            // dedicated `Modifiers` event has not been processed yet (X11 may
            // deliver the key before `XkbStateNotify`), press what is missing
            // so the client interprets the key with the right shift/ctrl level.
            // Never *release* from here: GPUI strips Shift from punctuation
            // keystrokes (`@` arrives with `shift: false`), so a release would
            // type the unshifted key. Releases come from `Modifiers` only. A
            // modifier key itself must not be reconciled *and* pressed: on
            // Linux GPUI suppresses those before the app.
            if !is_modifier_key_name(&keystroke.key) {
                press_missing_modifiers(state, keystroke.modifiers);
            }
            debug_log!(
                "input: key {:?} code {code} pressed={pressed}",
                keystroke.key
            );
            send_key(
                state,
                code,
                if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                },
            );
        }
        WebInputEvent::Modifiers { modifiers } => {
            debug_log!("input: modifiers {modifiers:?}");
            set_protocol_modifiers(state, modifiers);
        }
    }
}


/// The four modifier flags in the protocol's fixed order, paired with the
/// evdev code at the same index in [`MODIFIER_CODES`].
fn modifier_keys(modifiers: gpui_kit::Modifiers) -> [bool; 4] {
    [
        modifiers.shift,
        modifiers.control,
        modifiers.alt,
        modifiers.platform,
    ]
}

/// KEY_LEFTSHIFT / KEY_LEFTCTRL / KEY_LEFTALT / KEY_LEFTMETA, in the order of
/// [`modifier_keys`].
const MODIFIER_CODES: [u32; 4] = [42, 29, 56, 125];

/// Sends one key event to the client with a fresh serial, timestamp and the
/// forward filter the seat applies.
pub(super) fn send_key(state: &mut NestedState, code: u32, key_state: KeyState) {
    let serial = SERIAL_COUNTER.next_serial();
    let time = state.now_ms();
    let keyboard = state.keyboard.clone();
    keyboard.input::<(), _>(
        state,
        xkb_keycode(code),
        key_state,
        serial,
        time,
        |_, _, _| smithay::input::keyboard::FilterResult::Forward,
    );
}

/// Presses any modifier `modifiers` reports that the protocol does not already
/// hold; never releases. GPUI's keystrokes strip Shift for punctuation (`@` is
/// `shift: false`), so the only release source is the dedicated `Modifiers`
/// event — see [`set_protocol_modifiers`].
pub(super) fn press_missing_modifiers(state: &mut NestedState, modifiers: gpui_kit::Modifiers) {
    let current = state.modifiers;
    let mut next = current;
    next.shift |= modifiers.shift;
    next.control |= modifiers.control;
    next.alt |= modifiers.alt;
    next.platform |= modifiers.platform;
    if next == current {
        return;
    }
    state.modifiers = next;
    let was = modifier_keys(current);
    let now = modifier_keys(next);
    for (index, code) in MODIFIER_CODES.iter().enumerate() {
        if !was[index] && now[index] {
            send_key(state, *code, KeyState::Pressed);
        }
    }
}

/// Presses/releases the protocol modifier keys so the client's XKB state
/// matches `modifiers`. GPUI suppresses modifier-only key events on Linux
/// (`is_modifier_key` in its X11/Wayland clients), so the `Modifiers` event is
/// the only signal that Shift/Ctrl/Alt/Super went down or up; without it a
/// held modifier would never be held on the client.
pub(super) fn set_protocol_modifiers(state: &mut NestedState, modifiers: gpui_kit::Modifiers) {
    if state.modifiers == modifiers {
        return;
    }
    let was = modifier_keys(state.modifiers);
    state.modifiers = modifiers;
    let now = modifier_keys(modifiers);
    // Releases first, then presses: a modifier swap mid-chord (Ctrl released
    // as Alt goes down) never leaves both down between the events.
    for (index, code) in MODIFIER_CODES.iter().enumerate() {
        if was[index] && !now[index] {
            send_key(state, *code, KeyState::Released);
        }
    }
    for (index, code) in MODIFIER_CODES.iter().enumerate() {
        if !was[index] && now[index] {
            send_key(state, *code, KeyState::Pressed);
        }
    }
}


/// Whether GPUI's key name is a modifier key. Linux clients never deliver
/// these as `Key` events (they arrive as `Modifiers`), but a platform that
/// does must not have the modifier synthesized *and* pressed.
pub(super) fn is_modifier_key_name(key: &str) -> bool {
    matches!(
        key,
        "shift" | "control" | "alt" | "platform" | "super" | "meta" | "cmd" | "function"
    )
}


/// GPUI's evdev keycode space to xkb's (evdev + 8).
pub(super) fn xkb_keycode(evdev: u32) -> smithay::input::keyboard::Keycode {
    smithay::input::keyboard::Keycode::new(evdev + 8)
}


/// The Linux input-event code a spec [`crate::spec::PointerButton`] names (the
/// `BTN_*` codes the seat sends).
pub(super) fn btn_code(button: crate::spec::PointerButton) -> u32 {
    use crate::spec::PointerButton;
    match button {
        PointerButton::Left => 0x110,
        PointerButton::Right => 0x111,
        PointerButton::Middle => 0x112,
        PointerButton::Back => 0x113,
        PointerButton::Forward => 0x114,
    }
}

// -- smithay handlers (all on the compositor thread) -------------------------
