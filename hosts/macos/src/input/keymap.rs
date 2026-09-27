//! Protocol-v3 Qt key identifiers to macOS virtual key codes.
//!
//! The Pier speaks one key vocabulary on the wire on every platform. This is
//! the macOS half of that contract, and it is deliberately the same shape as
//! the Linux evdev map so a key that works against one host works against the
//! other.
//!
//! Keys macOS genuinely has no equivalent for return `None` rather than a
//! nearby substitute. A wrong key is worse than a refused one, and the caller
//! counts unmapped keys so the gap is visible instead of silent.

/// Shift modifier.
pub const MOD_SHIFT: u32 = 0x01;
/// Control modifier.
pub const MOD_CTRL: u32 = 0x02;
/// Alt/Option modifier.
pub const MOD_ALT: u32 = 0x04;
/// Meta/Command modifier.
pub const MOD_META: u32 = 0x08;
/// Keypad modifier.
pub const MOD_KEYPAD: u32 = 0x10;

/// Virtual key codes for the modifiers, used to release stuck modifiers.
pub const MODIFIER_CODES: [u16; 5] = [0x38, 0x3B, 0x3A, 0x37, 0x39];

/// Maps a Qt key identifier and modifier mask to a macOS virtual key code.
///
/// Returns `None` for keys macOS does not have, such as Num Lock, Scroll Lock,
/// Print Screen and Pause.
#[must_use]
pub fn qt_key_to_macos(qt_key: u32, modifiers: u32) -> Option<u16> {
    if modifiers & MOD_KEYPAD != 0 {
        if let Some(code) = keypad_key(qt_key) {
            return Some(code);
        }
    }
    Some(match qt_key {
        // Letters. macOS letter codes follow the original ADB layout rather
        // than alphabetical order.
        0x41 => 0x00,
        0x42 => 0x0B,
        0x43 => 0x08,
        0x44 => 0x02,
        0x45 => 0x0E,
        0x46 => 0x03,
        0x47 => 0x05,
        0x48 => 0x04,
        0x49 => 0x22,
        0x4A => 0x26,
        0x4B => 0x28,
        0x4C => 0x25,
        0x4D => 0x2E,
        0x4E => 0x2D,
        0x4F => 0x1F,
        0x50 => 0x23,
        0x51 => 0x0C,
        0x52 => 0x0F,
        0x53 => 0x01,
        0x54 => 0x11,
        0x55 => 0x20,
        0x56 => 0x09,
        0x57 => 0x0D,
        0x58 => 0x07,
        0x59 => 0x10,
        0x5A => 0x06,
        // Digits.
        0x30 => 0x1D,
        0x31 => 0x12,
        0x32 => 0x13,
        0x33 => 0x14,
        0x34 => 0x15,
        0x35 => 0x17,
        0x36 => 0x16,
        0x37 => 0x1A,
        0x38 => 0x1C,
        0x39 => 0x19,
        // F1-F12.
        0x0100_0030 => 0x7A,
        0x0100_0031 => 0x78,
        0x0100_0032 => 0x63,
        0x0100_0033 => 0x76,
        0x0100_0034 => 0x60,
        0x0100_0035 => 0x61,
        0x0100_0036 => 0x62,
        0x0100_0037 => 0x64,
        0x0100_0038 => 0x65,
        0x0100_0039 => 0x6D,
        0x0100_003A => 0x67,
        0x0100_003B => 0x6F,
        // Modifiers.
        0x0100_0020 => 0x38,
        0x0100_0021 => 0x3B,
        0x0100_0022 => 0x37,
        0x0100_0023 => 0x3A,
        // Navigation.
        0x0100_0012 => 0x7B,
        0x0100_0013 => 0x7E,
        0x0100_0014 => 0x7C,
        0x0100_0015 => 0x7D,
        0x0100_0010 => 0x73,
        0x0100_0011 => 0x77,
        0x0100_0016 => 0x74,
        0x0100_0017 => 0x79,
        // Editing.
        0x0100_0000 => 0x35,
        0x0100_0001 => 0x30,
        0x0100_0003 => 0x33,
        0x0100_0004 | 0x0100_0005 => 0x24,
        0x0100_0007 => 0x75,
        // Insert is deliberately absent. A Mac keyboard has no Insert, and the
        // key code that sits in its physical position is Help (0x72) — a
        // different key that applications act on differently. Translating one
        // to the other does not give the person Insert; it silently presses
        // something else. Unmapped is the honest answer, and the caller counts
        // it so the gap is visible rather than disguised as a keystroke.
        // Symbols.
        0x20 => 0x31,
        0x2D => 0x1B,
        0x3D => 0x18,
        0x5B => 0x21,
        0x5D => 0x1E,
        0x5C => 0x2A,
        0x3B => 0x29,
        0x27 => 0x27,
        0x60 => 0x32,
        0x2C => 0x2B,
        0x2E => 0x2F,
        0x2F => 0x2C,
        // Caps Lock is the only lock macOS exposes as a key code. Num Lock,
        // Scroll Lock, Print Screen and Pause are deliberately unmapped.
        0x0100_0024 => 0x39,
        _ => return None,
    })
}

fn keypad_key(qt_key: u32) -> Option<u16> {
    Some(match qt_key {
        0x30 => 0x52,
        0x31 => 0x53,
        0x32 => 0x54,
        0x33 => 0x55,
        0x34 => 0x56,
        0x35 => 0x57,
        0x36 => 0x58,
        0x37 => 0x59,
        0x38 => 0x5B,
        0x39 => 0x5C,
        0x2A => 0x43,
        0x2B => 0x45,
        0x2D => 0x4E,
        0x2E => 0x41,
        0x2F => 0x4B,
        0x3D => 0x51,
        0x0100_0005 => 0x4C,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_letters_digits_and_modifiers() {
        assert_eq!(qt_key_to_macos(0x41, 0), Some(0x00));
        assert_eq!(qt_key_to_macos(0x5A, 0), Some(0x06));
        assert_eq!(qt_key_to_macos(0x31, 0), Some(0x12));
        assert_eq!(qt_key_to_macos(0x0100_0021, MOD_CTRL), Some(0x3B));
        assert_eq!(qt_key_to_macos(0xDEAD_BEEF, 0), None);
    }

    #[test]
    fn escape_tab_return_and_backspace_match_the_protocol_contract() {
        assert_eq!(qt_key_to_macos(0x0100_0000, 0), Some(0x35));
        assert_eq!(qt_key_to_macos(0x0100_0001, 0), Some(0x30));
        assert_eq!(qt_key_to_macos(0x0100_0004, 0), Some(0x24));
        assert_eq!(qt_key_to_macos(0x0100_0005, 0), Some(0x24));
        assert_eq!(qt_key_to_macos(0x0100_0003, 0), Some(0x33));
    }

    #[test]
    fn keypad_modifier_selects_keypad_codes() {
        assert_eq!(qt_key_to_macos(0x31, 0), Some(0x12));
        assert_eq!(qt_key_to_macos(0x31, MOD_KEYPAD), Some(0x53));
        assert_eq!(qt_key_to_macos(0x0100_0005, MOD_KEYPAD), Some(0x4C));
    }

    #[test]
    fn keys_macos_lacks_are_refused_rather_than_substituted() {
        // Num Lock, Scroll Lock, Print Screen and Pause. Mapping these to a
        // nearby key would silently press the wrong thing on the host.
        for absent in [0x0100_0025_u32, 0x0100_0026, 0x0100_0009, 0x0100_0008] {
            assert_eq!(qt_key_to_macos(absent, 0), None, "key {absent:#x}");
        }
    }

    #[test]
    fn every_key_the_linux_host_maps_is_either_mapped_or_knowingly_absent() {
        // The wire vocabulary is shared, so a key Linux accepts must not
        // silently disappear on macOS without being listed above.
        let known_absent = [0x0100_0025_u32, 0x0100_0026, 0x0100_0009, 0x0100_0008];
        let linux_keys: [u32; 26] = [
            0x41,
            0x5A,
            0x30,
            0x39,
            0x20,
            0x2D,
            0x3D,
            0x5B,
            0x5D,
            0x5C,
            0x3B,
            0x27,
            0x60,
            0x2C,
            0x2E,
            0x2F,
            0x0100_0000,
            0x0100_0001,
            0x0100_0003,
            0x0100_0004,
            0x0100_0007,
            0x0100_0010,
            0x0100_0012,
            0x0100_0020,
            0x0100_0030,
            0x0100_0024,
        ];
        for key in linux_keys {
            assert!(
                qt_key_to_macos(key, 0).is_some() || known_absent.contains(&key),
                "Qt key {key:#x} is neither mapped nor listed as absent"
            );
        }
    }

    #[test]
    fn modifier_release_list_covers_every_modifier_code_we_emit() {
        for qt_modifier in [
            0x0100_0020_u32,
            0x0100_0021,
            0x0100_0022,
            0x0100_0023,
            0x0100_0024,
        ] {
            let code = qt_key_to_macos(qt_modifier, 0).expect("modifier maps");
            assert!(
                MODIFIER_CODES.contains(&code),
                "modifier {code:#x} is not in the release list"
            );
        }
    }

    #[test]
    fn compact_modifier_bits_match_protocol_contract() {
        assert_eq!(MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META | MOD_KEYPAD, 0x1F);
    }
}
