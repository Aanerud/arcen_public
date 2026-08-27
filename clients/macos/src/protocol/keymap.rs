pub const QT_KEY_META: u32 = 0x0100_0022;
pub const QT_KEY_CTRL: u32 = 0x0100_0021;
pub const QT_KEY_SHIFT: u32 = 0x0100_0020;
pub const QT_KEY_ALT: u32 = 0x0100_0023;
pub const QT_KEY_F9: u32 = 0x0100_0038;

pub const MOD_SHIFT: u32 = 0x0200_0000;
pub const MOD_CTRL: u32 = 0x0400_0000;
pub const MOD_ALT: u32 = 0x0800_0000;
pub const MOD_META: u32 = 0x1000_0000;
pub const MOD_KEYPAD: u32 = 0x2000_0000;

pub const MODIFIER_BIT_SHIFT: u8 = 0x01;
pub const MODIFIER_BIT_CTRL: u8 = 0x02;
pub const MODIFIER_BIT_ALT: u8 = 0x04;
pub const MODIFIER_BIT_META: u8 = 0x08;
pub const MODIFIER_BIT_KEYPAD: u8 = 0x10;

pub fn qt_key_to_linux_scancode(qt_key: u32) -> u16 {
    match qt_key {
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
        0x0100_0020 => 42,
        QT_KEY_CTRL => 29,
        0x0100_0023 => 56,
        QT_KEY_META => 125,
        0x0100_0013 => 103,
        0x0100_0015 => 108,
        0x0100_0012 => 105,
        0x0100_0014 => 106,
        0x0100_0010 => 102,
        0x0100_0011 => 107,
        0x0100_0016 => 104,
        0x0100_0017 => 109,
        0x0100_0003 => 14,
        0x0100_0007 => 111,
        0x0100_0004 => 28,
        0x0100_0005 => 28,
        0x0100_0001 => 1,
        0x0100_0000 => 15,
        0x0100_0006 => 110,
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
        0x0100_0024 => 58,
        0x0100_0025 => 69,
        0x0100_0026 => 70,
        0x0100_0009 => 99,
        0x0100_0008 => 119,
        _ => 0,
    }
}

pub fn swap_cmd_ctrl_for_linux_dest(qt_key: u32, qt_modifiers: u32) -> (u32, u32) {
    let out_key = if qt_key == QT_KEY_META {
        QT_KEY_CTRL
    } else {
        qt_key
    };
    let out_modifiers = if qt_modifiers & MOD_META != 0 {
        (qt_modifiers & !MOD_META) | MOD_CTRL
    } else {
        qt_modifiers
    };
    (out_key, out_modifiers)
}

pub const FLAME_CRITICAL_CHORDS: &[(u32, u32, &str)] = &[
    (0x20, MOD_SHIFT, "Shift+Space -- Flame pan"),
    (0x20, MOD_ALT, "Alt+Space -- Flame zoom"),
    (0x5B, 0, "[ -- decrease brush"),
    (0x5D, 0, "] -- increase brush"),
    (0x57, 0, "W -- translate gizmo"),
    (0x45, 0, "E -- rotate gizmo"),
    (0x52, 0, "R -- scale gizmo"),
    (0x0100_0012, 0, "Left -- prev frame"),
    (0x0100_0014, 0, "Right -- next frame"),
    (0x0100_0012, MOD_SHIFT, "Shift+Left -- prev keyframe"),
    (0x0100_0014, MOD_SHIFT, "Shift+Right -- next keyframe"),
    (0x20, 0, "Space -- play/pause"),
    (0x4C, 0, "L -- loop"),
    (0x53, MOD_CTRL, "Ctrl+S -- save"),
    (0x5A, MOD_CTRL, "Ctrl+Z -- undo"),
    (0x5A, MOD_CTRL | MOD_SHIFT, "Ctrl+Shift+Z -- redo"),
    (
        0x50,
        MOD_CTRL | MOD_SHIFT | MOD_ALT,
        "Ctrl+Shift+Alt+P -- Flame paint brush",
    ),
    (
        0x4B,
        MOD_CTRL | MOD_SHIFT | MOD_ALT,
        "Ctrl+Shift+Alt+K -- Flame keyer toggle",
    ),
    (0x0100_0038, 0, "F9 -- release all modifiers (client panic)"),
    (0x0100_0030, MOD_CTRL, "Ctrl+F1 -- context-help"),
    (0x5D, MOD_CTRL, "Ctrl+] -- brightness up"),
    (0x5B, MOD_CTRL, "Ctrl+[ -- brightness down"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_flame_critical_keys_to_linux_scancodes() {
        for (qt_key, _, description) in FLAME_CRITICAL_CHORDS {
            assert_ne!(
                qt_key_to_linux_scancode(*qt_key),
                0,
                "{description} must have a Linux scancode mapping"
            );
        }
    }

    #[test]
    fn swaps_cmd_to_ctrl_for_linux_destinations() {
        let (key, modifiers) = swap_cmd_ctrl_for_linux_dest(QT_KEY_META, MOD_META);
        assert_eq!(key, QT_KEY_CTRL);
        assert_eq!(modifiers, MOD_CTRL);
    }

    #[test]
    fn leaves_ctrl_chords_unchanged() {
        let (key, modifiers) = swap_cmd_ctrl_for_linux_dest(0x53, MOD_CTRL);
        assert_eq!(key, 0x53);
        assert_eq!(modifiers, MOD_CTRL);
    }
}
