// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! A PC keyboard's keys, by position.
//!
//! A PC keyboard names each key by its PC/AT scan code set 1 make code, the
//! code Windows reports with every key message and the one Linux evdev codes
//! were numbered after. This module turns a set 1 code into what a Deck does
//! with the key: the key identifier the wire carries (the same Qt key
//! identifier a Mac Deck sends for that position, so one representation
//! travels for both), a modifier for the keyboard policy, a lock, an input
//! method key the Deck's own input method consumes, or a key that never
//! leaves the Deck.
//!
//! It is the inverse of [`super::scancode::set1_scancode`] for every key
//! that table knows, and covers what that table leaves out: the keypad, the
//! lock keys, Print Screen, Pause, the Menu key, F13–F24 and the Japanese
//! and Korean keys.

use arcen_protocol::keys::{KEY_INTL_BACKSLASH, KEY_RETURN};

use super::scancode::Set1Scancode;

/// `Qt::Key_Enter`: the keypad's Enter (sent with `MOD_KEYPAD`).
pub const KEY_ENTER: u32 = 0x0100_0005;
/// `Qt::Key_Pause` (Pause and Break).
pub const KEY_PAUSE: u32 = 0x0100_0008;
/// `Qt::Key_Print` (Print Screen and `SysRq`).
pub const KEY_PRINT: u32 = 0x0100_0009;
/// `Qt::Key_Clear`: the keypad's 5 with Num Lock off.
pub const KEY_CLEAR: u32 = 0x0100_000B;
/// `Qt::Key_CapsLock`.
pub const KEY_CAPS_LOCK: u32 = 0x0100_0024;
/// `Qt::Key_NumLock`.
pub const KEY_NUM_LOCK: u32 = 0x0100_0025;
/// `Qt::Key_ScrollLock`.
pub const KEY_SCROLL_LOCK: u32 = 0x0100_0026;
/// `Qt::Key_F1`; F2–F35 follow it.
pub const KEY_F1: u32 = 0x0100_0030;
/// `Qt::Key_F4`.
pub const KEY_F4: u32 = 0x0100_0033;
/// `Qt::Key_F9`: the Deck's panic key on every Deck.
pub const KEY_F9: u32 = 0x0100_0038;
/// `Qt::Key_F12`.
pub const KEY_F12: u32 = 0x0100_003B;
/// `Qt::Key_Menu`: the PC's application (context menu) key.
pub const KEY_MENU: u32 = 0x0100_0055;
/// `Qt::Key_Home`.
pub const KEY_HOME: u32 = 0x0100_0010;
/// `Qt::Key_End`.
pub const KEY_END: u32 = 0x0100_0011;
/// `Qt::Key_Delete`.
pub const KEY_DELETE: u32 = 0x0100_0007;

/// A modifier key on a PC keyboard. Left and right are told apart: the
/// right Alt is `AltGr` under a layout that has it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PcModifier {
    /// Left Control.
    LeftControl,
    /// Right Control.
    RightControl,
    /// Left Shift.
    LeftShift,
    /// Right Shift.
    RightShift,
    /// Left Alt.
    LeftAlt,
    /// Right Alt (`AltGr`).
    RightAlt,
    /// Left Windows key.
    LeftWindows,
    /// Right Windows key.
    RightWindows,
}

/// A lock key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LockKey {
    /// Caps Lock: forwarded, the host's own lock follows it.
    Caps,
    /// Num Lock: stays on the Deck, which decides what each keypad key is
    /// (a digit or a navigation key) from its own Num Lock, as a Mac's
    /// keypad always types digits.
    Num,
    /// Scroll Lock: forwarded.
    Scroll,
}

impl LockKey {
    /// The key identifier.
    #[must_use]
    pub const fn qt_key(self) -> u32 {
        match self {
            Self::Caps => KEY_CAPS_LOCK,
            Self::Num => KEY_NUM_LOCK,
            Self::Scroll => KEY_SCROLL_LOCK,
        }
    }
}

/// A key that drives the Deck's own input method. Its result reaches the
/// host as committed text; the key itself never does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ImeKey {
    /// Japanese Katakana/Hiragana (Kana), or the Korean Hangul key.
    KanaHangul,
    /// Japanese Kanji, or the Korean Hanja key.
    KanjiHanja,
    /// Japanese Henkan (convert).
    Convert,
    /// Japanese Muhenkan (do not convert).
    NonConvert,
    /// Japanese Hankaku/Zenkaku, Eisu and the other input mode keys.
    InputMode,
    /// A key the input method took (Windows `VK_PROCESSKEY`).
    Processed,
}

/// What a key on a PC keyboard is to a Deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum PcKey {
    /// A modifier, for the keyboard policy.
    Modifier(PcModifier),
    /// A key position: the key identifier the wire carries, and whether it
    /// is on the keypad (`MOD_KEYPAD`).
    Position {
        /// The key identifier.
        qt_key: u32,
        /// On the keypad.
        keypad: bool,
    },
    /// A lock key.
    Lock(LockKey),
    /// An input method key.
    Ime(ImeKey),
    /// A character key the wire has no position for (the JIS Yen and Ro
    /// keys, the keypad comma): the characters the Deck's layout types with
    /// it go as committed text.
    CharacterOnly,
    /// A key the Deck's operating system keeps: power, sleep and wake (which
    /// must never reach a host), volume, media and browser keys.
    LocalOnly,
    /// Not a key the person pressed: a modifier the keyboard or Windows adds
    /// around another key (the "fake" Shift around keypad navigation, the
    /// Control Windows adds before `AltGr`).
    Synthetic,
    /// A code this table does not know.
    Unmapped,
}

impl PcKey {
    const fn position(qt_key: u32) -> Self {
        Self::Position {
            qt_key,
            keypad: false,
        }
    }

    const fn keypad(qt_key: u32) -> Self {
        Self::Position {
            qt_key,
            keypad: true,
        }
    }
}

/// The main block's letters and punctuation, by set 1 code from `0x10`, by
/// what a US layout prints on each position.
const MAIN_BLOCK: [(u8, u32); 37] = [
    (0x10, 0x51),
    (0x11, 0x57),
    (0x12, 0x45),
    (0x13, 0x52),
    (0x14, 0x54),
    (0x15, 0x59),
    (0x16, 0x55),
    (0x17, 0x49),
    (0x18, 0x4F),
    (0x19, 0x50),
    (0x1A, 0x5B),
    (0x1B, 0x5D),
    (0x1E, 0x41),
    (0x1F, 0x53),
    (0x20, 0x44),
    (0x21, 0x46),
    (0x22, 0x47),
    (0x23, 0x48),
    (0x24, 0x4A),
    (0x25, 0x4B),
    (0x26, 0x4C),
    (0x27, 0x3B),
    (0x28, 0x27),
    (0x29, 0x60),
    (0x2B, 0x5C),
    (0x2C, 0x5A),
    (0x2D, 0x58),
    (0x2E, 0x43),
    (0x2F, 0x56),
    (0x30, 0x42),
    (0x31, 0x4E),
    (0x32, 0x4D),
    (0x33, 0x2C),
    (0x34, 0x2E),
    (0x35, 0x2F),
    (0x0C, 0x2D),
    (0x0D, 0x3D),
];

/// The keypad with Num Lock on, by set 1 code (not `E0`-prefixed).
const KEYPAD: [(u8, u32); 15] = [
    (0x47, 0x37),
    (0x48, 0x38),
    (0x49, 0x39),
    (0x4A, 0x2D),
    (0x4B, 0x34),
    (0x4C, 0x35),
    (0x4D, 0x36),
    (0x4E, 0x2B),
    (0x4F, 0x31),
    (0x50, 0x32),
    (0x51, 0x33),
    (0x52, 0x30),
    (0x53, 0x2E),
    (0x37, 0x2A),
    (0x59, 0x3D),
];

/// What a key is, by its set 1 make code.
///
/// The keypad's digit keys are digits here (Num Lock on); a platform
/// adapter that knows its Num Lock is off maps them to navigation keys with
/// [`keypad_navigation`]. Num Lock is accepted both as `45` (set 1) and as
/// `E0 45` (Windows reports it extended, and Pause as `45`).
#[must_use]
pub fn pc_key(scancode: Set1Scancode) -> PcKey {
    if scancode.extended {
        extended_key(scancode.code)
    } else {
        plain_key(scancode.code)
    }
}

fn plain_key(code: u8) -> PcKey {
    if let Some((_, qt_key)) = MAIN_BLOCK.iter().find(|(make, _)| *make == code) {
        return PcKey::position(*qt_key);
    }
    if let Some((_, qt_key)) = KEYPAD.iter().find(|(make, _)| *make == code) {
        return PcKey::keypad(*qt_key);
    }
    match code {
        0x01 => PcKey::position(0x0100_0000),
        // 1-9, then 0.
        0x02..=0x0A => PcKey::position(0x31 + u32::from(code - 0x02)),
        0x0B => PcKey::position(0x30),
        0x0E => PcKey::position(0x0100_0003),
        0x0F => PcKey::position(0x0100_0001),
        0x1C => PcKey::position(KEY_RETURN),
        0x1D => PcKey::Modifier(PcModifier::LeftControl),
        0x2A => PcKey::Modifier(PcModifier::LeftShift),
        0x36 => PcKey::Modifier(PcModifier::RightShift),
        0x38 => PcKey::Modifier(PcModifier::LeftAlt),
        0x39 => PcKey::position(0x20),
        0x3A => PcKey::Lock(LockKey::Caps),
        0x3B..=0x44 => PcKey::position(KEY_F1 + u32::from(code - 0x3B)),
        0x45 => PcKey::Lock(LockKey::Num),
        0x46 => PcKey::Lock(LockKey::Scroll),
        // `SysRq`: Print Screen with Alt held.
        0x54 => PcKey::position(KEY_PRINT),
        0x56 => PcKey::position(KEY_INTL_BACKSLASH),
        0x57 => PcKey::position(KEY_F1 + 10),
        0x58 => PcKey::position(KEY_F1 + 11),
        // F13-F23, then F24.
        0x64..=0x6E => PcKey::position(KEY_F1 + 12 + u32::from(code - 0x64)),
        0x76 => PcKey::position(KEY_F1 + 23),
        // Katakana/Hiragana; Korean Hangul and Hanja (USB `LANG1`/`LANG2`,
        // which Windows reports as 72/71 and the set 1 table as F2/F1);
        // `LANG3`/`LANG4`.
        0x70 | 0x72 | 0xF2 => PcKey::Ime(ImeKey::KanaHangul),
        0x71 | 0xF1 => PcKey::Ime(ImeKey::KanjiHanja),
        0x77 | 0x78 => PcKey::Ime(ImeKey::InputMode),
        0x79 => PcKey::Ime(ImeKey::Convert),
        0x7B => PcKey::Ime(ImeKey::NonConvert),
        // JIS Ro and Yen, the keypad comma (Brazilian ABNT2).
        0x73 | 0x7D | 0x7E => PcKey::CharacterOnly,
        _ => PcKey::Unmapped,
    }
}

fn extended_key(code: u8) -> PcKey {
    match code {
        0x1C => PcKey::keypad(KEY_ENTER),
        0x1D => PcKey::Modifier(PcModifier::RightControl),
        // The keyboard's own Shift make/break around keypad navigation and
        // Print Screen.
        0x2A | 0x36 => PcKey::Synthetic,
        0x35 => PcKey::keypad(0x2F),
        0x37 => PcKey::position(KEY_PRINT),
        0x38 => PcKey::Modifier(PcModifier::RightAlt),
        0x45 => PcKey::Lock(LockKey::Num),
        // Break: Pause with Control held.
        0x46 => PcKey::position(KEY_PAUSE),
        0x47 => PcKey::position(KEY_HOME),
        0x48 => PcKey::position(0x0100_0013),
        0x49 => PcKey::position(0x0100_0016),
        0x4B => PcKey::position(0x0100_0012),
        0x4D => PcKey::position(0x0100_0014),
        0x4F => PcKey::position(KEY_END),
        0x50 => PcKey::position(0x0100_0015),
        0x51 => PcKey::position(0x0100_0017),
        0x52 => PcKey::position(0x0100_0006),
        0x53 => PcKey::position(KEY_DELETE),
        0x5B => PcKey::Modifier(PcModifier::LeftWindows),
        0x5C => PcKey::Modifier(PcModifier::RightWindows),
        0x5D => PcKey::position(KEY_MENU),
        // Power, sleep, wake; media, volume, browser and launcher keys.
        0x5E
        | 0x5F
        | 0x63
        | 0x10
        | 0x19
        | 0x20
        | 0x21
        | 0x22
        | 0x24
        | 0x2E
        | 0x30
        | 0x32
        | 0x65..=0x6D => PcKey::LocalOnly,
        _ => PcKey::Unmapped,
    }
}

/// The navigation key a keypad key is with Num Lock off, by its key
/// identifier with Num Lock on (`Clear` for 5). `None` for the keypad keys
/// Num Lock does not change.
#[must_use]
pub const fn keypad_navigation(keypad_qt_key: u32) -> Option<u32> {
    Some(match keypad_qt_key {
        0x37 => KEY_HOME,
        0x38 => 0x0100_0013,
        0x39 => 0x0100_0016,
        0x34 => 0x0100_0012,
        0x35 => KEY_CLEAR,
        0x36 => 0x0100_0014,
        0x31 => KEY_END,
        0x32 => 0x0100_0015,
        0x33 => 0x0100_0017,
        0x30 => 0x0100_0006,
        0x2E => KEY_DELETE,
        _ => return None,
    })
}

/// The identifier the ISO key between left Shift and Z is sent as: its own
/// ([`KEY_INTL_BACKSLASH`]) to a host that accepts extended keys, otherwise
/// the grave key's, as a Mac Deck sends it.
#[must_use]
pub const fn iso_key_identifier(intl_backslash: bool) -> u32 {
    if intl_backslash {
        KEY_INTL_BACKSLASH
    } else {
        0x60
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyboard::scancode::set1_scancode;
    use arcen_protocol::keys::{KEY_ALT, KEY_ALTGR, KEY_CONTROL, KEY_META, KEY_SHIFT};

    fn plain(code: u8) -> Set1Scancode {
        Set1Scancode {
            code,
            extended: false,
        }
    }

    fn extended(code: u8) -> Set1Scancode {
        Set1Scancode {
            code,
            extended: true,
        }
    }

    /// Every key identifier `set1_scancode` knows, by its canonical
    /// (unshifted) identifier.
    fn canonical_identifiers() -> Vec<u32> {
        let mut keys: Vec<u32> = (0x20..0x80)
            .filter(|key| !matches!(key, 0x21..=0x26 | 0x28..=0x2B | 0x3A | 0x3C | 0x3E..=0x40))
            .filter(|key| !matches!(key, 0x5E | 0x5F | 0x7B..=0x7E))
            .collect();
        keys.extend([KEY_INTL_BACKSLASH, 0x0100_0000, 0x0100_0001, 0x0100_0003]);
        keys.extend([KEY_RETURN, KEY_ENTER, 0x0100_0006, KEY_DELETE]);
        keys.extend(0x0100_0010..=0x0100_0017);
        keys.extend(KEY_F1..=KEY_F12);
        keys
    }

    #[test]
    fn every_position_round_trips_through_set1_scancode() {
        for qt_key in canonical_identifiers() {
            let Some(code) = set1_scancode(qt_key, 0) else {
                continue;
            };
            let expected = if qt_key == KEY_ENTER {
                PcKey::keypad(KEY_ENTER)
            } else {
                PcKey::position(qt_key)
            };
            assert_eq!(pc_key(code), expected, "{qt_key:#x} via {code:?}");
        }
    }

    #[test]
    fn modifiers_round_trip_to_their_left_side() {
        for (qt_key, modifier) in [
            (KEY_SHIFT, PcModifier::LeftShift),
            (KEY_CONTROL, PcModifier::LeftControl),
            (KEY_ALT, PcModifier::LeftAlt),
            (KEY_ALTGR, PcModifier::RightAlt),
            (KEY_META, PcModifier::LeftWindows),
        ] {
            let code = set1_scancode(qt_key, 0).expect("modifier has a scan code");
            assert_eq!(pc_key(code), PcKey::Modifier(modifier));
        }
        assert_eq!(
            pc_key(extended(0x1D)),
            PcKey::Modifier(PcModifier::RightControl)
        );
        assert_eq!(pc_key(plain(0x36)), PcKey::Modifier(PcModifier::RightShift));
        assert_eq!(
            pc_key(extended(0x5C)),
            PcKey::Modifier(PcModifier::RightWindows)
        );
    }

    #[test]
    fn the_keypad_is_digits_with_the_keypad_flag() {
        assert_eq!(pc_key(plain(0x47)), PcKey::keypad(0x37));
        assert_eq!(pc_key(plain(0x52)), PcKey::keypad(0x30));
        assert_eq!(pc_key(plain(0x53)), PcKey::keypad(0x2E));
        assert_eq!(pc_key(plain(0x37)), PcKey::keypad(0x2A));
        assert_eq!(pc_key(plain(0x4A)), PcKey::keypad(0x2D));
        assert_eq!(pc_key(plain(0x4E)), PcKey::keypad(0x2B));
        assert_eq!(pc_key(extended(0x35)), PcKey::keypad(0x2F));
        assert_eq!(pc_key(extended(0x1C)), PcKey::keypad(KEY_ENTER));
        // The navigation cluster is the same code, extended.
        assert_eq!(pc_key(extended(0x47)), PcKey::position(KEY_HOME));
        assert_eq!(keypad_navigation(0x37), Some(KEY_HOME));
        assert_eq!(keypad_navigation(0x35), Some(KEY_CLEAR));
        assert_eq!(keypad_navigation(0x2A), None);
    }

    #[test]
    fn locks_print_pause_menu_and_function_keys() {
        assert_eq!(pc_key(plain(0x3A)), PcKey::Lock(LockKey::Caps));
        assert_eq!(pc_key(plain(0x45)), PcKey::Lock(LockKey::Num));
        assert_eq!(pc_key(extended(0x45)), PcKey::Lock(LockKey::Num));
        assert_eq!(pc_key(plain(0x46)), PcKey::Lock(LockKey::Scroll));
        assert_eq!(pc_key(extended(0x37)), PcKey::position(KEY_PRINT));
        assert_eq!(pc_key(plain(0x54)), PcKey::position(KEY_PRINT));
        assert_eq!(pc_key(extended(0x46)), PcKey::position(KEY_PAUSE));
        assert_eq!(pc_key(extended(0x5D)), PcKey::position(KEY_MENU));
        assert_eq!(pc_key(plain(0x64)), PcKey::position(0x0100_003C));
        assert_eq!(pc_key(plain(0x6E)), PcKey::position(0x0100_0046));
        assert_eq!(pc_key(plain(0x76)), PcKey::position(0x0100_0047));
    }

    #[test]
    fn input_method_and_layout_only_keys_stay_on_the_deck() {
        for code in [0x70, 0x72, 0xF2] {
            assert_eq!(pc_key(plain(code)), PcKey::Ime(ImeKey::KanaHangul));
        }
        assert_eq!(pc_key(plain(0x71)), PcKey::Ime(ImeKey::KanjiHanja));
        assert_eq!(pc_key(plain(0x79)), PcKey::Ime(ImeKey::Convert));
        assert_eq!(pc_key(plain(0x7B)), PcKey::Ime(ImeKey::NonConvert));
        for code in [0x73, 0x7D, 0x7E] {
            assert_eq!(pc_key(plain(code)), PcKey::CharacterOnly);
        }
    }

    #[test]
    fn power_keys_never_leave_the_deck() {
        for code in [0x5E, 0x5F, 0x63] {
            assert_eq!(pc_key(extended(code)), PcKey::LocalOnly);
        }
        assert_eq!(pc_key(extended(0x2A)), PcKey::Synthetic);
        assert_eq!(pc_key(extended(0x7F)), PcKey::Unmapped);
    }

    #[test]
    fn no_two_codes_share_a_position() {
        let mut seen = std::collections::BTreeMap::new();
        for code in 0..=0xFF_u8 {
            for scancode in [plain(code), extended(code)] {
                if let PcKey::Position { qt_key, keypad } = pc_key(scancode) {
                    // Print Screen is also `SysRq` (Alt+Print Screen).
                    if qt_key == KEY_PRINT && scancode == plain(0x54) {
                        continue;
                    }
                    if let Some(previous) = seen.insert((qt_key, keypad), scancode) {
                        panic!("{qt_key:#x} from {previous:?} and {scancode:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_iso_key_follows_the_hosts_extended_keys() {
        assert_eq!(iso_key_identifier(true), KEY_INTL_BACKSLASH);
        assert_eq!(iso_key_identifier(false), 0x60);
    }
}
