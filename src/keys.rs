//! GPUI keystrokes to physical keys.
//!
//! GPUI names a keystroke by its *keysym*: punctuation arrives as the shifted
//! name (`@` for the `2` key) with the Shift that needed it stripped, and a
//! release after Shift is released first comes back under the base name. A
//! page needs the physical key in both cases, and the editor's held-key
//! tracking must pair the halves, so both go through [`physical_key`].

use gpui_kit::Keystroke;

/// The physical key a keystroke names, as a Linux input-event code (evdev —
/// the code space smithay's keymap consumes, so it doubles as the portable
/// identity for held keys).
///
/// The key *name* is tried first, then `key_char` (for layouts whose keysym
/// names are not US names). Unknown keys return `None`: a page should receive
/// nothing rather than a mistyped key.
pub fn physical_key(keystroke: &Keystroke) -> Option<u32> {
    if let Some(code) = code_for_name(&keystroke.key) {
        return Some(code);
    }
    let ch = keystroke.key_char.as_deref()?;
    let mut chars = ch.chars();
    let first = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    code_for_name(&first.to_ascii_lowercase().to_string())
}

/// A US-layout table for the keys a page needs. `Fn` is deliberately absent:
/// it is a hardware modifier platforms do not report as a key event, and a
/// code for it would type nothing (the old table reused the Menu key's code).
fn code_for_name(key: &str) -> Option<u32> {
    Some(match key {
        "a" => 30,
        "b" => 48,
        "c" => 46,
        "d" => 32,
        "e" => 18,
        "f" => 33,
        "g" => 34,
        "h" => 35,
        "i" => 23,
        "j" => 36,
        "k" => 37,
        "l" => 38,
        "m" => 50,
        "n" => 49,
        "o" => 24,
        "p" => 25,
        "q" => 16,
        "r" => 19,
        "s" => 31,
        "t" => 20,
        "u" => 22,
        "v" => 47,
        "w" => 17,
        "x" => 45,
        "y" => 21,
        "z" => 44,
        "1" => 2,
        "2" => 3,
        "3" => 4,
        "4" => 5,
        "5" => 6,
        "6" => 7,
        "7" => 8,
        "8" => 9,
        "9" => 10,
        "0" => 11,
        // Shifted keysym names GPUI emits for the digit row.
        "!" => 2,
        "@" => 3,
        "#" => 4,
        "$" => 5,
        "%" => 6,
        "^" => 7,
        "&" => 8,
        "*" => 9,
        "(" => 10,
        ")" => 11,
        "=" | "plus" | "+" => 13,
        "-" | "minus" | "_" => 12,
        "[" | "{" => 26,
        "]" | "}" => 27,
        ";" | ":" => 39,
        "'" | "\"" => 40,
        "`" | "~" => 41,
        "," | "<" => 51,
        "." | ">" => 52,
        "/" | "?" => 53,
        "\\" | "|" => 43,
        "space" => 57,
        "escape" => 1,
        "enter" | "return" => 28,
        "tab" => 15,
        "backspace" => 14,
        "delete" => 111,
        "insert" => 110,
        "up" => 103,
        "down" => 108,
        "left" => 105,
        "right" => 106,
        "home" => 102,
        "end" => 107,
        "pageup" => 104,
        "pagedown" => 109,
        "f1" => 59,
        "f2" => 60,
        "f3" => 61,
        "f4" => 62,
        "f5" => 63,
        "f6" => 64,
        "f7" => 65,
        "f8" => 66,
        "f9" => 67,
        "f10" => 68,
        "f11" => 87,
        "f12" => 88,
        "capslock" => 58,
        "numlock" => 69,
        "scroll_lock" | "scrolllock" => 70,
        "print" | "printscreen" | "sysrq" => 99,
        "pause" => 119,
        // The Menu key: KEY_COMPOSE (127) is what PC keyboards emit for it.
        "menu" | "contextmenu" => 127,
        "back" => 158,
        "forward" => 159,
        "shift" => 42,
        "control" => 29,
        "alt" => 56,
        "platform" | "super" | "meta" | "cmd" => 125,
        _ => return None,
    })
}
