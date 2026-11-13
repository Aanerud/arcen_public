// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Key identifiers as PC/AT scan code set 1 make codes.
//!
//! A host that injects scan codes lets its own keyboard layout decide the
//! character, exactly as for a keyboard plugged into it: the key next to
//! Enter types `æ` under a Norwegian layout and `'` under a US one. This is
//! what a Windows host injects (`KEYEVENTF_SCANCODE`), as Microsoft's RDP
//! clients send scan codes by default.
//!
//! Set 1 make codes of the main block equal Linux evdev key codes, which
//! were numbered after them; the `E0`-prefixed (extended) keys do not.

use arcen_protocol::keys::{
    KEY_ALT, KEY_ALTGR, KEY_CONTROL, KEY_INTL_BACKSLASH, KEY_META, KEY_SHIFT, MOD_KEYPAD,
};

/// A set 1 make code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Set1Scancode {
    /// The code without its prefix.
    pub code: u8,
    /// The key is `E0`-prefixed (`KEYEVENTF_EXTENDEDKEY`).
    pub extended: bool,
}

impl Set1Scancode {
    const fn plain(code: u8) -> Self {
        Self {
            code,
            extended: false,
        }
    }

    const fn extended(code: u8) -> Self {
        Self {
            code,
            extended: true,
        }
    }
}

/// Left Shift.
pub const LEFT_SHIFT: Set1Scancode = Set1Scancode::plain(0x2A);
/// Left Control.
pub const LEFT_CONTROL: Set1Scancode = Set1Scancode::plain(0x1D);
/// Left Alt.
pub const LEFT_ALT: Set1Scancode = Set1Scancode::plain(0x38);
/// Right Alt, `AltGr` under a layout that has it.
pub const RIGHT_ALT: Set1Scancode = Set1Scancode::extended(0x38);
/// Left Windows key.
pub const LEFT_META: Set1Scancode = Set1Scancode::extended(0x5B);

/// The scan code of a key identifier, by key position.
///
/// `None` for keypad keys (their scan codes type navigation keys while the
/// host's Num Lock is off, and a Mac keypad always types digits), for the
/// lock keys, Pause, Print Screen and Clear (whose state or prefixes a host
/// handles by virtual key), and for identifiers this table does not know. A
/// host injects those as it did before.
#[must_use]
pub fn set1_scancode(qt_key: u32, modifiers: u32) -> Option<Set1Scancode> {
    if modifiers & MOD_KEYPAD != 0 {
        return None;
    }
    let plain = Set1Scancode::plain;
    let extended = Set1Scancode::extended;
    Some(match qt_key {
        // Letters, by their US position.
        0x41 => plain(0x1E),
        0x42 => plain(0x30),
        0x43 => plain(0x2E),
        0x44 => plain(0x20),
        0x45 => plain(0x12),
        0x46 => plain(0x21),
        0x47 => plain(0x22),
        0x48 => plain(0x23),
        0x49 => plain(0x17),
        0x4A => plain(0x24),
        0x4B => plain(0x25),
        0x4C => plain(0x26),
        0x4D => plain(0x32),
        0x4E => plain(0x31),
        0x4F => plain(0x18),
        0x50 => plain(0x19),
        0x51 => plain(0x10),
        0x52 => plain(0x13),
        0x53 => plain(0x1F),
        0x54 => plain(0x14),
        0x55 => plain(0x16),
        0x56 => plain(0x2F),
        0x57 => plain(0x11),
        0x58 => plain(0x2D),
        0x59 => plain(0x15),
        0x5A => plain(0x2C),
        // Digit row; the shifted US characters are accepted as aliases, as
        // a Windows host always did.
        0x31 | 0x21 => plain(0x02),
        0x32 | 0x40 => plain(0x03),
        0x33 | 0x23 => plain(0x04),
        0x34 | 0x24 => plain(0x05),
        0x35 | 0x25 => plain(0x06),
        0x36 | 0x5E => plain(0x07),
        0x37 | 0x26 => plain(0x08),
        0x38 | 0x2A => plain(0x09),
        0x39 | 0x28 => plain(0x0A),
        0x30 | 0x29 => plain(0x0B),
        0x2D | 0x5F => plain(0x0C),
        0x3D | 0x2B => plain(0x0D),
        // Punctuation positions.
        0x5B | 0x7B => plain(0x1A),
        0x5D | 0x7D => plain(0x1B),
        0x5C | 0x7C => plain(0x2B),
        0x3B | 0x3A => plain(0x27),
        0x27 | 0x22 => plain(0x28),
        0x60 | 0x7E => plain(0x29),
        0x2C | 0x3C => plain(0x33),
        0x2E | 0x3E => plain(0x34),
        0x2F | 0x3F => plain(0x35),
        KEY_INTL_BACKSLASH => plain(0x56),
        0x20 => plain(0x39),
        // Editing.
        0x0100_0000 => plain(0x01),
        0x0100_0001 | 0x0100_0002 => plain(0x0F),
        0x0100_0003 => plain(0x0E),
        0x0100_0004 => plain(0x1C),
        0x0100_0005 => extended(0x1C),
        0x0100_0006 => extended(0x52),
        0x0100_0007 => extended(0x53),
        // Navigation.
        0x0100_0010 => extended(0x47),
        0x0100_0011 => extended(0x4F),
        0x0100_0012 => extended(0x4B),
        0x0100_0013 => extended(0x48),
        0x0100_0014 => extended(0x4D),
        0x0100_0015 => extended(0x50),
        0x0100_0016 => extended(0x49),
        0x0100_0017 => extended(0x51),
        // Modifiers.
        KEY_SHIFT => LEFT_SHIFT,
        KEY_CONTROL => LEFT_CONTROL,
        KEY_ALT => LEFT_ALT,
        KEY_ALTGR => RIGHT_ALT,
        KEY_META => LEFT_META,
        // F1-F12.
        0x0100_0030..=0x0100_0039 => plain(0x3B + u8::try_from(qt_key - 0x0100_0030).unwrap_or(0)),
        0x0100_003A => plain(0x57),
        0x0100_003B => plain(0x58),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn letters_and_punctuation_are_physical_positions() {
        assert_eq!(set1_scancode(0x51, 0), Some(Set1Scancode::plain(0x10)));
        assert_eq!(set1_scancode(0x5A, 0), Some(Set1Scancode::plain(0x2C)));
        assert_eq!(set1_scancode(0x27, 0), Some(Set1Scancode::plain(0x28)));
        assert_eq!(set1_scancode(0x60, 0), Some(Set1Scancode::plain(0x29)));
        assert_eq!(
            set1_scancode(KEY_INTL_BACKSLASH, 0),
            Some(Set1Scancode::plain(0x56))
        );
        assert_eq!(
            set1_scancode(0x0100_0039, 0),
            Some(Set1Scancode::plain(0x44))
        );
    }

    #[test]
    fn right_alt_and_navigation_are_extended() {
        assert_eq!(set1_scancode(KEY_ALTGR, 0), Some(RIGHT_ALT));
        assert_eq!(set1_scancode(KEY_ALT, 0), Some(LEFT_ALT));
        assert_eq!(set1_scancode(KEY_META, 0), Some(LEFT_META));
        assert_eq!(
            set1_scancode(0x0100_0012, 0),
            Some(Set1Scancode::extended(0x4B))
        );
    }

    #[test]
    fn keypad_and_lock_keys_have_no_scancode() {
        assert_eq!(set1_scancode(0x31, MOD_KEYPAD), None);
        assert_eq!(set1_scancode(0x0100_0024, 0), None);
        assert_eq!(set1_scancode(0x0100_0008, 0), None);
        assert_eq!(set1_scancode(0xDEAD_BEEF, 0), None);
    }

    #[test]
    fn main_block_codes_are_unique_per_position() {
        let mut seen = std::collections::BTreeMap::new();
        for qt_key in (0x20..0x80).chain([KEY_INTL_BACKSLASH]) {
            if let Some(code) = set1_scancode(qt_key, 0) {
                seen.entry(code).or_insert_with(Vec::new).push(qt_key);
            }
        }
        for (code, keys) in seen {
            // Aliases share a position only with a US shifted character.
            assert!(keys.len() <= 2, "{code:?} has {keys:x?}");
        }
    }
}
