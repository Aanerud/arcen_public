//! Protocol-v3 Qt key identifiers to Linux evdev key codes.

pub const MOD_SHIFT: u32 = 0x01;
pub const MOD_CTRL: u32 = 0x02;
pub const MOD_ALT: u32 = 0x04;
pub const MOD_META: u32 = 0x08;
pub const MOD_KEYPAD: u32 = 0x10;

pub const MODIFIER_CODES: [u16; 8] = [42, 54, 29, 97, 56, 100, 125, 126];
pub const LOCK_CODES: [u16; 3] = [58, 69, 70];

/// `KEY_SYSRQ`, the PrintScreen/SysRq key.
pub const KEY_SYSRQ: u16 = 99;
/// `KEY_PRINT`, which X and Wayland keymaps also map to `Print`.
pub const KEY_PRINT: u16 = 210;

/// The evdev code a kernel virtual keyboard may emit for `code`.
///
/// A uinput keyboard is an ordinary kernel keyboard: the kernel's SysRq
/// filter reads it on every virtual terminal, whatever X server owns the
/// screen, so a remote Alt+PrintScreen+key would run a SysRq action on the
/// host. PrintScreen is therefore sent as `KEY_PRINT`, which the kernel never
/// treats as SysRq.
pub const fn kernel_safe_key(code: u16) -> u16 {
    if code == KEY_SYSRQ {
        KEY_PRINT
    } else {
        code
    }
}

pub fn qt_key_to_evdev(qt_key: u32, modifiers: u32) -> Option<u16> {
    if modifiers & MOD_KEYPAD != 0 {
        if let Some(code) = keypad_key(qt_key) {
            return Some(code);
        }
    }
    Some(match qt_key {
        // Letters.
        0x41 => 30,
        0x42 => 48,
        0x43 => 46,
        0x44 => 32,
        0x45 => 18,
        0x46 => 33,
        0x47 => 34,
        0x48 => 35,
        0x49 => 23,
        0x4A => 36,
        0x4B => 37,
        0x4C => 38,
        0x4D => 50,
        0x4E => 49,
        0x4F => 24,
        0x50 => 25,
        0x51 => 16,
        0x52 => 19,
        0x53 => 31,
        0x54 => 20,
        0x55 => 22,
        0x56 => 47,
        0x57 => 17,
        0x58 => 45,
        0x59 => 21,
        0x5A => 44,
        // Digits.
        0x30 => 11,
        0x31 => 2,
        0x32 => 3,
        0x33 => 4,
        0x34 => 5,
        0x35 => 6,
        0x36 => 7,
        0x37 => 8,
        0x38 => 9,
        0x39 => 10,
        // F1-F12.
        0x0100_0030 => 59,
        0x0100_0031 => 60,
        0x0100_0032 => 61,
        0x0100_0033 => 62,
        0x0100_0034 => 63,
        0x0100_0035 => 64,
        0x0100_0036 => 65,
        0x0100_0037 => 66,
        0x0100_0038 => 67,
        0x0100_0039 => 68,
        0x0100_003A => 87,
        0x0100_003B => 88,
        // Modifiers.
        0x0100_0020 => 42,
        0x0100_0021 => 29,
        0x0100_0022 => 125,
        0x0100_0023 => 56,
        // Navigation.
        0x0100_0013 => 103,
        0x0100_0015 => 108,
        0x0100_0012 => 105,
        0x0100_0014 => 106,
        0x0100_0010 => 102,
        0x0100_0011 => 107,
        0x0100_0016 => 104,
        0x0100_0017 => 109,
        // Editing.
        0x0100_0003 => 14,
        0x0100_0007 => 111,
        0x0100_0004 | 0x0100_0005 => 28,
        0x0100_0000 => 1,
        0x0100_0001 => 15,
        0x0100_0006 => 110,
        // Symbols.
        0x20 => 57,
        0x2D => 12,
        0x3D => 13,
        0x5B => 26,
        0x5D => 27,
        0x5C => 43,
        0x3B => 39,
        0x27 => 40,
        0x60 => 41,
        0x2C => 51,
        0x2E => 52,
        0x2F => 53,
        // Locks / print / pause.
        0x0100_0024 => 58,
        0x0100_0025 => 69,
        0x0100_0026 => 70,
        0x0100_0009 => 99,
        0x0100_0008 => 119,
        _ => return None,
    })
}

fn keypad_key(qt_key: u32) -> Option<u16> {
    Some(match qt_key {
        0x30 => 82,
        0x31 => 79,
        0x32 => 80,
        0x33 => 81,
        0x34 => 75,
        0x35 => 76,
        0x36 => 77,
        0x37 => 71,
        0x38 => 72,
        0x39 => 73,
        0x2A => 55,
        0x2B => 78,
        0x2D => 74,
        0x2E => 83,
        0x2F => 98,
        0x0100_0005 => 96,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A remote session must never be able to press the host's power,
    /// sleep, suspend or wake keys: a settings daemon in the session (or
    /// logind) would act on the host.
    #[test]
    fn no_deck_key_reaches_a_host_power_key() {
        const HOST_POWER_KEYS: [u16; 5] = [116, 142, 143, 205, 356];
        for qt_key in (0..0x1_0000_u32).chain(0x0100_0000..0x0101_0000) {
            for modifiers in [0, MOD_KEYPAD, MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META] {
                if let Some(code) = qt_key_to_evdev(qt_key, modifiers) {
                    assert!(
                        !HOST_POWER_KEYS.contains(&code),
                        "Qt key {qt_key:#x} maps to host power key {code}"
                    );
                }
            }
        }
    }

    #[test]
    fn maps_established_qt_codes_to_evdev() {
        assert_eq!(qt_key_to_evdev(0x41, 0), Some(30));
        assert_eq!(qt_key_to_evdev(0x0100_0021, MOD_CTRL), Some(29));
        assert_eq!(qt_key_to_evdev(0x0100_0038, 0), Some(67));
        assert_eq!(qt_key_to_evdev(0x0100_0012, MOD_SHIFT), Some(105));
        assert_eq!(qt_key_to_evdev(0xDEAD_BEEF, 0), None);
    }

    #[test]
    fn qt_escape_and_tab_match_protocol_contract() {
        assert_eq!(qt_key_to_evdev(0x0100_0000, 0), Some(1));
        assert_eq!(qt_key_to_evdev(0x0100_0001, 0), Some(15));
    }

    #[test]
    fn keypad_modifier_selects_keypad_codes() {
        assert_eq!(qt_key_to_evdev(0x31, 0), Some(2));
        assert_eq!(qt_key_to_evdev(0x31, MOD_KEYPAD), Some(79));
        assert_eq!(qt_key_to_evdev(0x0100_0005, MOD_KEYPAD), Some(96));
    }

    #[test]
    fn flame_critical_chords_all_have_mapped_keys() {
        let keys = [
            0x20,
            0x5B,
            0x5D,
            0x57,
            0x45,
            0x52,
            0x0100_0012,
            0x0100_0014,
            0x4C,
            0x53,
            0x5A,
            0x50,
            0x4B,
            0x0100_0038,
            0x0100_0030,
        ];
        for key in keys {
            assert!(
                qt_key_to_evdev(key, 0).is_some(),
                "unmapped Qt key {key:#x}"
            );
        }
    }

    #[test]
    fn print_screen_never_reaches_a_kernel_keyboard_as_sysrq() {
        let print = qt_key_to_evdev(0x0100_0009, 0).unwrap();
        assert_eq!(kernel_safe_key(print), KEY_PRINT);
        assert_eq!(kernel_safe_key(print), 210);
        for modifiers in [
            0,
            MOD_ALT,
            MOD_CTRL | MOD_ALT,
            MOD_SHIFT | MOD_ALT,
            MOD_KEYPAD,
        ] {
            for qt_key in (0..0x200).chain(0x0100_0000..0x0100_0200) {
                if let Some(code) = qt_key_to_evdev(qt_key, modifiers) {
                    assert_ne!(kernel_safe_key(code), KEY_SYSRQ, "Qt key {qt_key:#x}");
                }
            }
        }
        assert_eq!(kernel_safe_key(30), 30);
        assert_eq!(kernel_safe_key(119), 119);
    }

    #[test]
    fn compact_modifier_bits_match_protocol_contract() {
        assert_eq!(MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META | MOD_KEYPAD, 0x1F);
    }
}
