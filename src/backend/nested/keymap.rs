//! The physical key a keystroke names, read from the keymap the compositor
//! sent its client.
//!
//! [`crate::physical_key`] is a hand-written US table. Here the reverse table
//! comes from smithay's compiled keymap instead, so a character resolves to the
//! key that types it in the client's layout. The client's layout follows the
//! host's ([`NestedState::set_host_layout`]). Only levels 0 and 1 count: Shift reaches level 1 (T9 presses
//! it), nothing presses AltGr, and a key that would type something else is
//! worse than none. Named keys (`enter`, `f5`, `shift`, ...) do not move between
//! layouts, so a name the keymap does not resolve falls back to the US table.

use std::collections::HashMap;
use std::path::PathBuf;

use gpui_kit::Keystroke;
use smithay::input::keyboard::{Keysym, XkbConfig, xkb};

use super::compositor::NestedState;
use crate::debug::debug_log;

impl NestedState {
    /// Rebuilds [`Self::key_table`] from the keymap the keyboard holds now.
    pub(super) fn rebuild_key_table(&mut self) {
        let keyboard = self.keyboard.clone();
        self.key_table = keyboard.with_xkb_state(self, |context| {
            let xkb = context.xkb().lock().unwrap();
            // SAFETY: the table copies names and codes; no keymap ref-count escapes.
            KeyTable::from_keymap(unsafe { xkb.keymap() })
        });
    }

    /// Recompiles the client keymap for the host layout `name` (sent to the
    /// client as a new `wl_keyboard.keymap`) and rebuilds the key table. A name
    /// the registry does not know (`unknown` before a Wayland host sends its
    /// keymap) falls back to xkb's default layout.
    pub(super) fn set_host_layout(&mut self, name: &str) {
        let layout = host_layout(name);
        debug_log!("keyboard: host layout {name:?} -> {layout:?}");
        let config = match &layout {
            Some(layout) => XkbConfig {
                layout: &layout.layout,
                variant: &layout.variant,
                ..XkbConfig::default()
            },
            None => XkbConfig::default(),
        };
        let keyboard = self.keyboard.clone();
        if let Err(error) = keyboard.set_xkb_config(self, config) {
            debug_log!("keyboard: compiling {layout:?} failed: {error:?}");
            return;
        }
        self.rebuild_key_table();
    }
}

/// Key name (GPUI's spelling, characters lowercased) to evdev code.
#[derive(Debug, Default)]
pub(super) struct KeyTable(HashMap<String, u32>);

impl KeyTable {
    /// Indexes levels 0-1 of layout 0 in `keymap`. When several keys produce a
    /// name, main keys beat the keypad, then the lowest keycode wins: US `<` is
    /// Shift+comma (51), not the 102nd key (86), and `1` is the digit row.
    pub(super) fn from_keymap(keymap: &xkb::Keymap) -> Self {
        let mut candidates: Vec<(bool, u32, String)> = Vec::new();
        keymap.key_for_each(|keymap, keycode| {
            // xkb keycodes are evdev + 8 (`input::xkb_keycode`).
            let Some(evdev) = keycode.raw().checked_sub(8) else {
                return;
            };
            let levels = keymap.num_levels_for_key(keycode, 0).min(2);
            for level in 0..levels {
                for &sym in keymap.key_get_syms_by_level(keycode, 0, level) {
                    if let Some(name) = key_name(sym) {
                        candidates.push((sym.is_keypad_key(), evdev, name));
                    }
                }
            }
        });
        candidates.sort_by_key(|&(keypad, evdev, _)| (keypad, evdev));
        let mut table = HashMap::new();
        for (_, evdev, name) in candidates {
            table.entry(name).or_insert(evdev);
        }
        Self(table)
    }

    /// The evdev code for `keystroke`: its name, then its `key_char` (GPUI
    /// names a non-ASCII key by its US-position guess, while `key_char` is
    /// what the layout types), then the US table for multi-character names.
    pub(super) fn physical_key(&self, keystroke: &Keystroke) -> Option<u32> {
        if let Some(&code) = self.0.get(keystroke.key.as_str()) {
            return Some(code);
        }
        if let Some(ch) = single_char(keystroke.key_char.as_deref())
            && let Some(&code) = self.0.get(&lowercase(ch))
        {
            return Some(code);
        }
        if keystroke.key.chars().count() > 1 {
            return crate::keys::code_for_name(&keystroke.key);
        }
        None
    }
}

/// An xkb layout and variant (`fr` + ``, `us` + `dvorak`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Layout {
    pub(super) layout: String,
    pub(super) variant: String,
}

/// Resolves a layout's display name to its xkb layout and variant through
/// xkeyboard-config's registry. The name is xkb's `layout_get_name` (what GPUI
/// reports on X11 and Wayland), which the registry lists as the layout's or
/// variant's `<description>`.
fn host_layout(name: &str) -> Option<Layout> {
    let root = std::env::var_os("XKB_CONFIG_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/usr/share/X11/xkb"));
    ["evdev.xml", "evdev.extras.xml"].iter().find_map(|file| {
        let registry = std::fs::read_to_string(root.join("rules").join(file)).ok()?;
        find_layout(&registry, name)
    })
}

/// Scans the registry's `<layout>` blocks for a layout or variant described
/// as `name`. The format is regular (one `configItem` per layout, then its
/// `<variantList>`), so a scan suffices.
fn find_layout(registry: &str, name: &str) -> Option<Layout> {
    for block in registry.split("<layout>").skip(1) {
        let block = block.split("</layout>").next().unwrap_or(block);
        let (head, variants) = block.split_once("<variantList>").unwrap_or((block, ""));
        let Some(layout) = element(head, "name") else {
            continue;
        };
        if element(head, "description").as_deref() == Some(name) {
            return Some(Layout {
                layout,
                variant: String::new(),
            });
        }
        for variant in variants.split("<variant>").skip(1) {
            if element(variant, "description").as_deref() == Some(name)
                && let Some(variant) = element(variant, "name")
            {
                return Some(Layout { layout, variant });
            }
        }
    }
    None
}

/// The unescaped text of the first `<tag>…</tag>` in `xml`.
fn element(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = start + xml[start..].find(&format!("</{tag}>"))?;
    Some(
        xml[start..end]
            .trim()
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// The name GPUI's Linux backend gives `sym` (`keystroke_from_xkb`), with
/// characters lowercased so both levels of a letter share it.
fn key_name(sym: Keysym) -> Option<String> {
    let named = match sym {
        Keysym::Return => "enter",
        Keysym::Prior => "pageup",
        Keysym::Next => "pagedown",
        Keysym::ISO_Left_Tab | Keysym::Tab => "tab",
        Keysym::XF86_Back => "back",
        Keysym::XF86_Forward => "forward",
        Keysym::space => "space",
        Keysym::BackSpace => "backspace",
        Keysym::Delete => "delete",
        Keysym::Escape => "escape",
        Keysym::Left => "left",
        Keysym::Right => "right",
        Keysym::Up => "up",
        Keysym::Down => "down",
        Keysym::Home => "home",
        Keysym::End => "end",
        Keysym::Insert => "insert",
        _ => "",
    };
    if !named.is_empty() {
        return Some(named.to_owned());
    }
    let name = xkb::keysym_get_name(sym).to_lowercase();
    if sym.is_keypad_key() {
        return Some(name.replace("kp_", ""));
    }
    let utf8 = xkb::keysym_to_utf8(sym);
    match single_char(Some(&utf8)) {
        Some(ch) if !ch.is_control() => Some(lowercase(ch)),
        _ if sym == Keysym::NoSymbol => None,
        _ => Some(name),
    }
}

fn single_char(text: Option<&str>) -> Option<char> {
    let mut chars = text?.chars();
    let first = chars.next()?;
    chars.next().is_none().then_some(first)
}

fn lowercase(ch: char) -> String {
    ch.to_lowercase().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn us_table() -> KeyTable {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compiling the us keymap");
        KeyTable::from_keymap(&keymap)
    }

    fn keystroke(key: &str, key_char: Option<&str>) -> Keystroke {
        Keystroke {
            modifiers: Default::default(),
            key: key.into(),
            key_char: key_char.map(Into::into),
        }
    }

    /// The oracle: every character the hand-written US table knows resolves to
    /// the same key from the compiled `us` keymap itself (not the fallback).
    #[test]
    fn us_keymap_matches_the_hand_table_for_characters() {
        let table = us_table();
        let mut checked = 0;
        for ch in (0x21u8..0x7f).map(char::from) {
            let name = ch.to_string();
            let Some(expected) = crate::keys::code_for_name(&name) else {
                continue;
            };
            assert_eq!(table.0.get(&name), Some(&expected), "key {name:?}");
            checked += 1;
        }
        // Upper-case letters are not in the hand table (GPUI lowercases them).
        assert_eq!(checked, 94 - 26);
    }

    /// The same oracle for the named keys the keymap spells the way GPUI does;
    /// the rest (`shift`, `capslock`, `return`, ...) come from the fallback, and
    /// every name in the hand table must still resolve to its code.
    #[test]
    fn us_keymap_matches_the_hand_table_for_named_keys() {
        let table = us_table();
        let from_keymap = [
            "space",
            "escape",
            "enter",
            "tab",
            "backspace",
            "delete",
            "insert",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "f1",
            "f2",
            "f3",
            "f4",
            "f5",
            "f6",
            "f7",
            "f8",
            "f9",
            "f10",
            "f11",
            "f12",
            "pause",
            "menu",
            "back",
            "forward",
        ];
        for name in from_keymap {
            assert_eq!(
                table.0.get(name),
                crate::keys::code_for_name(name).as_ref(),
                "key {name:?}"
            );
        }
        let fallback = [
            "return",
            "plus",
            "minus",
            "capslock",
            "numlock",
            "scroll_lock",
            "scrolllock",
            "print",
            "printscreen",
            "sysrq",
            "contextmenu",
            "shift",
            "control",
            "alt",
            "platform",
            "super",
            "meta",
            "cmd",
        ];
        for name in fallback {
            assert_eq!(
                table.physical_key(&keystroke(name, None)),
                crate::keys::code_for_name(name),
                "key {name:?}"
            );
        }
    }

    #[test]
    fn shifted_names_and_key_chars_resolve() {
        let table = us_table();
        // T9: GPUI names Shift+2 `@`; a release may come back as `2`.
        assert_eq!(table.physical_key(&keystroke("@", Some("@"))), Some(3));
        assert_eq!(table.physical_key(&keystroke("2", None)), Some(3));
        // Shift+A arrives as `a` with `key_char` `A`.
        assert_eq!(table.physical_key(&keystroke("a", Some("A"))), Some(30));
        // An unknown name resolves through `key_char`.
        assert_eq!(
            table.physical_key(&keystroke("unknown", Some("q"))),
            Some(16)
        );
    }

    #[test]
    fn unreachable_characters_are_dropped() {
        let table = us_table();
        // Not on a US key at levels 0-1: no key, rather than a mistyped one.
        assert_eq!(table.physical_key(&keystroke("é", Some("é"))), None);
        assert_eq!(table.physical_key(&keystroke("unknown", None)), None);
    }

    const REGISTRY: &str = r#"<xkbConfigRegistry>
      <layoutList>
        <layout>
          <configItem>
            <name>us</name>
            <shortDescription>en</shortDescription>
            <description>English (US)</description>
          </configItem>
          <variantList>
            <variant>
              <configItem>
                <name>dvorak</name>
                <description>English (Dvorak)</description>
              </configItem>
            </variant>
          </variantList>
        </layout>
        <layout>
          <configItem popularity="exotic">
            <name>ba</name>
            <description>Bosnian</description>
          </configItem>
          <variantList>
            <variant>
              <configItem>
                <name>us</name>
                <description>Bosnian (US, with Bosnian letters &amp; digraphs)</description>
              </configItem>
            </variant>
          </variantList>
        </layout>
        <layout>
          <configItem>
            <name>fr</name>
            <description>French</description>
          </configItem>
        </layout>
      </layoutList>
    </xkbConfigRegistry>"#;

    fn layout(layout: &str, variant: &str) -> Option<Layout> {
        Some(Layout {
            layout: layout.into(),
            variant: variant.into(),
        })
    }

    #[test]
    fn registry_names_resolve_to_layouts_and_variants() {
        assert_eq!(find_layout(REGISTRY, "English (US)"), layout("us", ""));
        assert_eq!(
            find_layout(REGISTRY, "English (Dvorak)"),
            layout("us", "dvorak")
        );
        assert_eq!(find_layout(REGISTRY, "French"), layout("fr", ""));
        assert_eq!(
            find_layout(REGISTRY, "Bosnian (US, with Bosnian letters & digraphs)"),
            layout("ba", "us")
        );
        assert_eq!(find_layout(REGISTRY, "unknown"), None);
        // `<shortDescription>` is not the description.
        assert_eq!(find_layout(REGISTRY, "en"), None);
    }

    /// The installed registry agrees with the names xkb itself reports: each
    /// compiled layout's `layout_get_name` resolves back to that layout.
    #[test]
    fn installed_registry_round_trips_layout_names() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        for (code, variant) in [
            ("us", ""),
            ("fr", ""),
            ("de", ""),
            ("ru", ""),
            ("us", "dvorak"),
        ] {
            let keymap = xkb::Keymap::new_from_names(
                &context,
                "evdev",
                "pc105",
                code,
                variant,
                None,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .expect("compiling the keymap");
            let name = keymap.layout_get_name(0);
            assert_eq!(host_layout(name), layout(code, variant), "{name:?}");
        }
    }

    /// A layout whose characters sit on other keys: AZERTY types `a` with the
    /// US `q` key and `2` with Shift+`é` (the US `2` key).
    #[test]
    fn french_keymap_moves_characters() {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "evdev",
            "pc105",
            "fr",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .expect("compiling the fr keymap");
        let table = KeyTable::from_keymap(&keymap);
        assert_eq!(table.physical_key(&keystroke("a", Some("a"))), Some(16));
        assert_eq!(table.physical_key(&keystroke("2", Some("2"))), Some(3));
        assert_eq!(table.physical_key(&keystroke("é", Some("é"))), Some(3));
    }
}
