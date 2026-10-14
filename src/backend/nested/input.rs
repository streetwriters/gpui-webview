//! Seat input: `WebInputEvent` to `wl_seat` pointer, keyboard and axis events.

use super::GDK_AXIS_UNITS_PER_GPUI_PIXEL;
use crate::spec::SurfaceId;
use crate::spec::WebInputEvent;
use smithay::backend::input::Axis;
use smithay::backend::input::ButtonState;
use smithay::backend::input::KeyState;
use smithay::input::pointer::AxisFrame;
use smithay::input::pointer::ButtonEvent;
use smithay::input::pointer::MotionEvent;
use smithay::utils::Point as SPoint;
use smithay::utils::SERIAL_COUNTER;

use super::compositor::{NestedState, SurfaceRole, surface_role};
use crate::debug::{debug_log, debug_log2};

impl NestedState {
    /// Re-enters the surface under the pointer on the next event. T8: a popup the
    /// client destroys never gets a usable `leave`, so GDK keeps routing to the dead window.
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

/// One input event: translate GPUI input into `wl_seat` events for the surface
/// under the pointer (a popup when there is one, the page otherwise).
pub(super) fn apply_input(state: &mut NestedState, id: SurfaceId, event: WebInputEvent) {
    let Some(toplevel) = state
        .mapped
        .get(&id)
        .map(|surface| surface.wl_surface().clone())
    else {
        debug_log!("input: no mapped surface for block {id}");
        return;
    };
    // A popup takes the pointer while it is over it; an outside press dismisses it.
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
    // Surface-local coordinates: a popup's are relative to its origin.
    let (lx, ly) = match event.pointer_position() {
        Some((x, y)) => (x - origin.0, y - origin.1),
        None => (0.0, 0.0),
    };

    let serial = SERIAL_COUNTER.next_serial();
    let time = state.now_ms();
    // The handles are cloned so they can be borrowed alongside `&mut state`.
    let pointer = state.pointer.clone();
    // T15: smithay subtracts the focus location; the event is surface-local, so pass the origin.
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
            // T7: clients dispatch a group only when the frame arrives.
            pointer.frame(state);
            debug_log2!("input: motion ({lx:.0},{ly:.0})");
        }
        WebInputEvent::PointerLeave => {
            // The pointer left the block: `wl_pointer.leave` stops stuck hover/cursor state.
            let belongs =
                state
                    .pointer
                    .current_focus()
                    .is_some_and(|focused| match surface_role(&focused) {
                        Some(SurfaceRole::Toplevel(block)) => block == id,
                        Some(SurfaceRole::Popup { block, .. }) => block == id,
                        None => false,
                    });
            if belongs {
                state.release_pointer_focus();
                debug_log2!("input: pointer left block {id}");
            }
        }
        WebInputEvent::PointerButton {
            button, pressed, ..
        } => {
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
            debug_log2!("input: button {button:#x} pressed={pressed}");
        }
        WebInputEvent::PointerAxis { dx, dy, .. } => {
            // T12: negate (GPUI's down is negative, GDK's positive) and scale by
            // 15/60, so one tick moves what one native click moves.
            let mut frame = AxisFrame::new(time);
            if dy != 0. {
                frame = frame.value(Axis::Vertical, -dy * GDK_AXIS_UNITS_PER_GPUI_PIXEL);
            }
            if dx != 0. {
                frame = frame.value(Axis::Horizontal, -dx * GDK_AXIS_UNITS_PER_GPUI_PIXEL);
            }
            // T7: a motion sharing the scroll's frame makes GDK's flush drop the
            // scroll, so move the pointer only when the focus changes, in its own frame.
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
            debug_log2!("input: axis ({lx:.0},{ly:.0}) dy={dy} dx={dx}");
        }
        WebInputEvent::Key { keystroke, pressed } => {
            let Some(code) = state.key_table.physical_key(&keystroke) else {
                debug_log!(
                    "input: no keycode for {:?} (char {:?}); dropped",
                    keystroke.key,
                    keystroke.key_char
                );
                return;
            };
            // T9: press what the keystroke's modifier state is missing (X11 can
            // reorder); never release here, or `@` would type `2`. Releases come
            // from `Modifiers`.
            if !is_modifier_key_name(&keystroke.key) {
                press_missing_modifiers(state, keystroke.modifiers);
            }
            debug_log2!(
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
            debug_log2!("input: modifiers {modifiers:?}");
            set_protocol_modifiers(state, modifiers);
        }
    }
}

/// The four modifier flags, in the same order as [`MODIFIER_CODES`].
fn modifier_keys(modifiers: gpui_kit::Modifiers) -> [bool; 4] {
    [
        modifiers.shift,
        modifiers.control,
        modifiers.alt,
        modifiers.platform,
    ]
}

/// KEY_LEFTSHIFT / KEY_LEFTCTRL / KEY_LEFTALT / KEY_LEFTMETA.
const MODIFIER_CODES: [u32; 4] = [42, 29, 56, 125];

/// Sends one key event with a fresh serial, timestamp and a forward filter.
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

/// Presses any missing modifier; never releases (T9).
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

/// Presses/releases the protocol modifier keys to match `modifiers` (T9).
pub(super) fn set_protocol_modifiers(state: &mut NestedState, modifiers: gpui_kit::Modifiers) {
    if state.modifiers == modifiers {
        return;
    }
    let was = modifier_keys(state.modifiers);
    state.modifiers = modifiers;
    let now = modifier_keys(modifiers);
    // Releases first, then presses, so a swap never leaves both down between events.
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

/// Whether GPUI's key name is a modifier key (delivered as `Modifiers`).
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

/// The Linux input-event code a spec [`crate::spec::PointerButton`] names.
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
