// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Protocol-v3 Qt key identifiers to Linux evdev key codes.

use arcen_input::keyboard::scancode::set1_position;
use arcen_input::text::host_layout;
use arcen_protocol::messages::HostKeymapMsg;

pub use arcen_protocol::keys::{MOD_ALT, MOD_CTRL, MOD_KEYPAD, MOD_META, MOD_SHIFT};

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

/// The Deck's view of a session's layout (`HostKeymapMsg`): what the active
/// layout types at each printable Qt key position, unmodified and with
/// Shift, from `chars_at` (by evdev code; `None` for a position the
/// layout does not describe). Built by the shared
/// [`arcen_input::text::host_layout`] contract every Pier uses.
#[must_use]
pub fn host_keymap_msg(
    source: &str,
    generation: u64,
    layouts: Vec<String>,
    active_layout: Option<String>,
    chars_at: impl Fn(u16) -> Option<(Option<String>, Option<String>)>,
) -> HostKeymapMsg {
    let mut notice = host_layout::host_keymap_msg(source, layouts, active_layout, |qt_key| {
        chars_at(qt_key_to_evdev(qt_key, 0)?)
    });
    notice.generation = generation;
    notice
}

/// The evdev code of a Qt key identifier. A main-block position's code is
/// its PC set-1 make code (the shared [`set1_position`] table, which evdev
/// was numbered after); `E0`-prefixed keys, locks, Print and Pause, and the
/// keypad (with [`MOD_KEYPAD`]) have evdev codes of their own.
pub fn qt_key_to_evdev(qt_key: u32, modifiers: u32) -> Option<u16> {
    if modifiers & MOD_KEYPAD != 0 {
        if let Some(code) = keypad_key(qt_key) {
            return Some(code);
        }
    }
    if let Some(code) = evdev_only_key(qt_key) {
        return Some(code);
    }
    set1_position(qt_key)
        .filter(|scan| !scan.extended)
        .map(|scan| u16::from(scan.code))
}

/// Keys whose evdev code is not their set-1 make code.
fn evdev_only_key(qt_key: u32) -> Option<u16> {
    Some(match qt_key {
        // Meta (Super), and `AltGr` (right Alt): the layout's third level.
        0x0100_0022 => 125,
        arcen_protocol::keys::KEY_ALTGR => 100,
        // Navigation.
        0x0100_0013 => 103,
        0x0100_0015 => 108,
        0x0100_0012 => 105,
        0x0100_0014 => 106,
        0x0100_0010 => 102,
        0x0100_0011 => 107,
        0x0100_0016 => 104,
        0x0100_0017 => 109,
        // Editing: Insert, Delete, and Enter as Return.
        0x0100_0006 => 110,
        0x0100_0007 => 111,
        0x0100_0005 => 28,
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
    fn extended_keys_are_right_alt_and_the_iso_key() {
        assert_eq!(
            qt_key_to_evdev(arcen_protocol::keys::KEY_ALTGR, 0),
            Some(100)
        );
        assert!(MODIFIER_CODES.contains(&100));
        assert_eq!(
            qt_key_to_evdev(arcen_protocol::keys::KEY_INTL_BACKSLASH, 0),
            Some(86)
        );
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

    /// Every identifier's evdev code, with and without the keypad flag, as
    /// the Linux Pier's own table gave them before the main block moved to
    /// the shared set-1 table: the move changed no key.
    #[test]
    fn evdev_codes_are_unchanged_by_the_shared_set1_table() {
        const PINNED: &[(u32, u32, u16)] = &[
            (0x20, 0, 57),
            (0x20, 16, 57),
            (0x27, 0, 40),
            (0x27, 16, 40),
            (0x2A, 16, 55),
            (0x2B, 16, 78),
            (0x2C, 0, 51),
            (0x2C, 16, 51),
            (0x2D, 0, 12),
            (0x2D, 16, 74),
            (0x2E, 0, 52),
            (0x2E, 16, 83),
            (0x2F, 0, 53),
            (0x2F, 16, 98),
            (0x30, 0, 11),
            (0x30, 16, 82),
            (0x31, 0, 2),
            (0x31, 16, 79),
            (0x32, 0, 3),
            (0x32, 16, 80),
            (0x33, 0, 4),
            (0x33, 16, 81),
            (0x34, 0, 5),
            (0x34, 16, 75),
            (0x35, 0, 6),
            (0x35, 16, 76),
            (0x36, 0, 7),
            (0x36, 16, 77),
            (0x37, 0, 8),
            (0x37, 16, 71),
            (0x38, 0, 9),
            (0x38, 16, 72),
            (0x39, 0, 10),
            (0x39, 16, 73),
            (0x3B, 0, 39),
            (0x3B, 16, 39),
            (0x3D, 0, 13),
            (0x3D, 16, 13),
            (0x41, 0, 30),
            (0x41, 16, 30),
            (0x42, 0, 48),
            (0x42, 16, 48),
            (0x43, 0, 46),
            (0x43, 16, 46),
            (0x44, 0, 32),
            (0x44, 16, 32),
            (0x45, 0, 18),
            (0x45, 16, 18),
            (0x46, 0, 33),
            (0x46, 16, 33),
            (0x47, 0, 34),
            (0x47, 16, 34),
            (0x48, 0, 35),
            (0x48, 16, 35),
            (0x49, 0, 23),
            (0x49, 16, 23),
            (0x4A, 0, 36),
            (0x4A, 16, 36),
            (0x4B, 0, 37),
            (0x4B, 16, 37),
            (0x4C, 0, 38),
            (0x4C, 16, 38),
            (0x4D, 0, 50),
            (0x4D, 16, 50),
            (0x4E, 0, 49),
            (0x4E, 16, 49),
            (0x4F, 0, 24),
            (0x4F, 16, 24),
            (0x50, 0, 25),
            (0x50, 16, 25),
            (0x51, 0, 16),
            (0x51, 16, 16),
            (0x52, 0, 19),
            (0x52, 16, 19),
            (0x53, 0, 31),
            (0x53, 16, 31),
            (0x54, 0, 20),
            (0x54, 16, 20),
            (0x55, 0, 22),
            (0x55, 16, 22),
            (0x56, 0, 47),
            (0x56, 16, 47),
            (0x57, 0, 17),
            (0x57, 16, 17),
            (0x58, 0, 45),
            (0x58, 16, 45),
            (0x59, 0, 21),
            (0x59, 16, 21),
            (0x5A, 0, 44),
            (0x5A, 16, 44),
            (0x5B, 0, 26),
            (0x5B, 16, 26),
            (0x5C, 0, 43),
            (0x5C, 16, 43),
            (0x5D, 0, 27),
            (0x5D, 16, 27),
            (0x60, 0, 41),
            (0x60, 16, 41),
            (0x1000000, 0, 1),
            (0x1000000, 16, 1),
            (0x1000001, 0, 15),
            (0x1000001, 16, 15),
            (0x1000003, 0, 14),
            (0x1000003, 16, 14),
            (0x1000004, 0, 28),
            (0x1000004, 16, 28),
            (0x1000005, 0, 28),
            (0x1000005, 16, 96),
            (0x1000006, 0, 110),
            (0x1000006, 16, 110),
            (0x1000007, 0, 111),
            (0x1000007, 16, 111),
            (0x1000008, 0, 119),
            (0x1000008, 16, 119),
            (0x1000009, 0, 99),
            (0x1000009, 16, 99),
            (0x1000010, 0, 102),
            (0x1000010, 16, 102),
            (0x1000011, 0, 107),
            (0x1000011, 16, 107),
            (0x1000012, 0, 105),
            (0x1000012, 16, 105),
            (0x1000013, 0, 103),
            (0x1000013, 16, 103),
            (0x1000014, 0, 106),
            (0x1000014, 16, 106),
            (0x1000015, 0, 108),
            (0x1000015, 16, 108),
            (0x1000016, 0, 104),
            (0x1000016, 16, 104),
            (0x1000017, 0, 109),
            (0x1000017, 16, 109),
            (0x1000020, 0, 42),
            (0x1000020, 16, 42),
            (0x1000021, 0, 29),
            (0x1000021, 16, 29),
            (0x1000022, 0, 125),
            (0x1000022, 16, 125),
            (0x1000023, 0, 56),
            (0x1000023, 16, 56),
            (0x1000024, 0, 58),
            (0x1000024, 16, 58),
            (0x1000025, 0, 69),
            (0x1000025, 16, 69),
            (0x1000026, 0, 70),
            (0x1000026, 16, 70),
            (0x1000030, 0, 59),
            (0x1000030, 16, 59),
            (0x1000031, 0, 60),
            (0x1000031, 16, 60),
            (0x1000032, 0, 61),
            (0x1000032, 16, 61),
            (0x1000033, 0, 62),
            (0x1000033, 16, 62),
            (0x1000034, 0, 63),
            (0x1000034, 16, 63),
            (0x1000035, 0, 64),
            (0x1000035, 16, 64),
            (0x1000036, 0, 65),
            (0x1000036, 16, 65),
            (0x1000037, 0, 66),
            (0x1000037, 16, 66),
            (0x1000038, 0, 67),
            (0x1000038, 16, 67),
            (0x1000039, 0, 68),
            (0x1000039, 16, 68),
            (0x100003A, 0, 87),
            (0x100003A, 16, 87),
            (0x100003B, 0, 88),
            (0x100003B, 16, 88),
            (0x1001103, 0, 100),
            (0x1001103, 16, 100),
            (0x1001104, 0, 86),
            (0x1001104, 16, 86),
        ];
        let mut mapped = Vec::new();
        for modifiers in [0, MOD_KEYPAD] {
            for qt_key in (0..0x1_0000_u32).chain(0x0100_0000..0x0101_0000) {
                if let Some(code) = qt_key_to_evdev(qt_key, modifiers) {
                    mapped.push((qt_key, modifiers, code));
                }
            }
        }
        mapped.sort_unstable();
        assert_eq!(mapped, PINNED);
    }

    #[test]
    fn compact_modifier_bits_match_protocol_contract() {
        assert_eq!(MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META | MOD_KEYPAD, 0x1F);
    }
}
