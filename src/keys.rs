//! Translate characters and key names into evdev keycodes for injection
//! through the seat's virtual keyboard. Built from the same "us" keymap the
//! keyboard advertises to clients, so what we press is what clients decode.

use std::collections::HashMap;

use smithay::input::keyboard::{xkb, Keycode};

// evdev codes for modifier keys (xkb keycode = evdev + 8).
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_LEFTALT: u32 = 56;
const KEY_LEFTMETA: u32 = 125;

#[derive(Debug, Clone, Copy)]
pub struct KeyPress {
    pub keycode: Keycode,
    pub shift: bool,
}

pub struct KeyMapper {
    by_char: HashMap<char, KeyPress>,
    by_keysym: HashMap<u32, KeyPress>,
}

impl KeyMapper {
    pub fn new() -> anyhow::Result<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            "us",
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| anyhow::anyhow!("failed to compile us keymap"))?;

        let mut by_char = HashMap::new();
        let mut by_keysym = HashMap::new();
        for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            let keycode = Keycode::new(raw);
            for level in 0..2u32 {
                for sym in keymap.key_get_syms_by_level(keycode, 0, level) {
                    let press = KeyPress {
                        keycode,
                        shift: level == 1,
                    };
                    by_keysym.entry(sym.raw()).or_insert(press);
                    if let Some(c) = sym.key_char() {
                        by_char.entry(c).or_insert(press);
                    }
                }
            }
        }
        Ok(KeyMapper { by_char, by_keysym })
    }

    pub fn for_char(&self, c: char) -> Option<KeyPress> {
        match c {
            '\n' | '\r' => self.for_name("Return"),
            '\t' => self.for_name("Tab"),
            _ => self.by_char.get(&c).copied(),
        }
    }

    /// Resolve a key name: a single character, an XKB keysym name
    /// (case-insensitive, e.g. "Return", "F5"), or a common alias.
    pub fn for_name(&self, name: &str) -> Option<KeyPress> {
        let mut chars = name.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if let Some(p) = self.for_char(c) {
                return Some(p);
            }
        }
        let lower = name.to_ascii_lowercase();
        let canonical = match lower.as_str() {
            "enter" | "return" => "Return",
            "esc" | "escape" => "Escape",
            "space" => "space",
            "tab" => "Tab",
            "backspace" => "BackSpace",
            "delete" | "del" => "Delete",
            "insert" => "Insert",
            "home" => "Home",
            "end" => "End",
            "pageup" | "page_up" | "prior" => "Prior",
            "pagedown" | "page_down" | "next" => "Next",
            "up" => "Up",
            "down" => "Down",
            "left" => "Left",
            "right" => "Right",
            "menu" => "Menu",
            "printscreen" | "print" => "Print",
            other => other,
        };
        let sym = xkb::keysym_from_name(canonical, xkb::KEYSYM_NO_FLAGS);
        let sym = if sym.raw() == 0 {
            xkb::keysym_from_name(canonical, xkb::KEYSYM_CASE_INSENSITIVE)
        } else {
            sym
        };
        if sym.raw() == 0 {
            return None;
        }
        self.by_keysym.get(&sym.raw()).copied()
    }

    /// Resolve a modifier name to its keycode.
    pub fn modifier(name: &str) -> Option<Keycode> {
        let evdev = match name.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => KEY_LEFTCTRL,
            "shift" => KEY_LEFTSHIFT,
            "alt" => KEY_LEFTALT,
            "super" | "meta" | "win" | "cmd" | "logo" => KEY_LEFTMETA,
            _ => return None,
        };
        Some(Keycode::new(evdev + 8))
    }

    pub fn shift_keycode() -> Keycode {
        Keycode::new(KEY_LEFTSHIFT + 8)
    }

    /// Parse a combo like "ctrl+shift+t" into (modifier keycodes, final key).
    pub fn parse_combo(&self, combo: &str) -> Result<(Vec<Keycode>, KeyPress), String> {
        let parts: Vec<&str> = combo.split('+').map(|s| s.trim()).collect();
        let (last, mods) = parts
            .split_last()
            .ok_or_else(|| format!("empty key combo: {combo:?}"))?;
        let mut mod_codes = Vec::new();
        for m in mods {
            let code =
                Self::modifier(m).ok_or_else(|| format!("unknown modifier {m:?} in {combo:?}"))?;
            mod_codes.push(code);
        }
        let key = self
            .for_name(last)
            .ok_or_else(|| format!("unknown key {last:?} in {combo:?}"))?;
        Ok((mod_codes, key))
    }
}
