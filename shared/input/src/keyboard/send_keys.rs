// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keys a Deck can send from its menu, and key remaps.
//!
//! **Send Key** (Connection ▸ Send Key on both Decks) sends a key the
//! keyboard in front of someone lacks or cannot send: a Mac has no Print
//! Screen, Scroll Lock, Pause or Insert, a PC's own Ctrl+Alt+Del never
//! reaches an application, and few keyboards have F13–F24. Each is a press
//! and a release through the ordinary key path, after every key held on the
//! host is released.
//!
//! **Remaps** turn one key into another on the way to the host (the Mac's
//! F13–F15 as Print Screen, Scroll Lock and Pause). Every table is checked
//! here, whatever wrote it: a remap can never produce a power, sleep or
//! wake key, `SysRq`, or anything outside the keys a remap may send.

use arcen_protocol::keys::{KEY_ALT, KEY_CONTROL, MOD_ALT, MOD_CTRL};

use super::HostKeyEdge;
use super::pc::{KEY_DELETE, KEY_F1, KEY_HOME, KEY_MENU, KEY_PAUSE, KEY_PRINT, KEY_SCROLL_LOCK};

/// `Qt::Key_Insert`.
pub const KEY_INSERT: u32 = 0x0100_0006;
/// `Qt::Key_SysReq`: never sent by a remap.
pub const KEY_SYS_REQ: u32 = 0x0100_000A;

/// A key the Send Key menu sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SendKey {
    /// Print Screen.
    PrintScreen,
    /// Scroll Lock.
    ScrollLock,
    /// Pause.
    Pause,
    /// Insert.
    Insert,
    /// Ctrl+Alt+Del (a Windows host's secure attention sequence).
    ControlAltDelete,
    /// The application (context menu) key.
    Menu,
    /// F13 to F24: the number.
    Function(u8),
}

/// Titles of F13–F24, in order.
const FUNCTION_TITLES: [&str; 12] = [
    "F13", "F14", "F15", "F16", "F17", "F18", "F19", "F20", "F21", "F22", "F23", "F24",
];

impl SendKey {
    /// Every key, in menu order: the named keys, then F13–F24.
    pub const ALL: [Self; 18] = [
        Self::PrintScreen,
        Self::ScrollLock,
        Self::Pause,
        Self::Insert,
        Self::ControlAltDelete,
        Self::Menu,
        Self::Function(13),
        Self::Function(14),
        Self::Function(15),
        Self::Function(16),
        Self::Function(17),
        Self::Function(18),
        Self::Function(19),
        Self::Function(20),
        Self::Function(21),
        Self::Function(22),
        Self::Function(23),
        Self::Function(24),
    ];

    /// The menu title, the same on both Decks.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::PrintScreen => "Print Screen",
            Self::ScrollLock => "Scroll Lock",
            Self::Pause => "Pause",
            Self::Insert => "Insert",
            Self::ControlAltDelete => "Ctrl+Alt+Del",
            Self::Menu => "Menu",
            Self::Function(number) => {
                if number >= 13 && number <= 24 {
                    FUNCTION_TITLES[(number - 13) as usize]
                } else {
                    ""
                }
            }
        }
    }

    /// The key identifier it presses (`None` for Ctrl+Alt+Del, a chord, and
    /// for a function key outside F13–F24).
    #[must_use]
    pub const fn qt_key(self) -> Option<u32> {
        Some(match self {
            Self::PrintScreen => KEY_PRINT,
            Self::ScrollLock => KEY_SCROLL_LOCK,
            Self::Pause => KEY_PAUSE,
            Self::Insert => KEY_INSERT,
            Self::Menu => KEY_MENU,
            Self::Function(number @ 13..=24) => KEY_F1 + number as u32 - 1,
            Self::Function(_) | Self::ControlAltDelete => return None,
        })
    }

    /// The host key edges after every held key is released: a press and a
    /// release of the key with no modifier, or Ctrl+Alt+Del pressed and
    /// released in order.
    #[must_use]
    pub fn edges(self) -> Vec<HostKeyEdge> {
        let edge = |qt_key, pressed, modifiers| HostKeyEdge {
            qt_key,
            pressed,
            modifiers,
        };
        if self == Self::ControlAltDelete {
            return vec![
                edge(KEY_CONTROL, true, MOD_CTRL),
                edge(KEY_ALT, true, MOD_CTRL | MOD_ALT),
                edge(KEY_DELETE, true, MOD_CTRL | MOD_ALT),
                edge(KEY_DELETE, false, MOD_CTRL | MOD_ALT),
                edge(KEY_ALT, false, MOD_CTRL),
                edge(KEY_CONTROL, false, 0),
            ];
        }
        self.qt_key()
            .map(|qt_key| vec![edge(qt_key, true, 0), edge(qt_key, false, 0)])
            .unwrap_or_default()
    }
}

/// One key turned into another on the way to the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyRemap {
    /// The key the keyboard reports.
    pub from: u32,
    /// The key the host gets.
    pub to: u32,
}

/// Why a remap table was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemapError {
    /// A remap sends a key no remap may send (power, sleep, wake,
    /// `SysRq`, or any key outside [`remap_may_send`]).
    ForbiddenTarget(u32),
    /// A remap takes the escape chord's key (Home), which must stay the
    /// way back to this computer's keyboard.
    EscapeKey,
    /// Two remaps for one key.
    Duplicate(u32),
}

/// Whether a remap may send `qt_key`: printable keys, F1–F24, the editing,
/// navigation and lock keys, Print Screen, Pause, Menu and the modifiers.
/// Everything else (power, sleep, wake, `SysRq`, media and launcher keys)
/// is refused.
#[must_use]
pub const fn remap_may_send(qt_key: u32) -> bool {
    matches!(
        qt_key,
        0x20..=0x7E
            // Escape, Tab, Backtab, Backspace, Return, Enter, Insert, Delete,
            // Pause, Print.
            | 0x0100_0000..=0x0100_0009
            // Home, End, the arrows, Page Up and Page Down.
            | 0x0100_0010..=0x0100_0017
            // Shift, Control, Meta, Alt, AltGr, Caps Lock, Num Lock,
            // Scroll Lock.
            | 0x0100_0020..=0x0100_0023
            | 0x0100_1103
            | 0x0100_0024..=0x0100_0026
            // F1-F24.
            | 0x0100_0030..=0x0100_0047
            | KEY_MENU
    )
}

/// A validated remap table.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeyRemapTable {
    remaps: Vec<KeyRemap>,
}

impl KeyRemapTable {
    /// The table, when every remap is allowed.
    ///
    /// # Errors
    ///
    /// The first remap refused, and why.
    pub fn new(remaps: &[KeyRemap]) -> Result<Self, RemapError> {
        for (index, remap) in remaps.iter().enumerate() {
            if remap.to == KEY_SYS_REQ || !remap_may_send(remap.to) {
                return Err(RemapError::ForbiddenTarget(remap.to));
            }
            if remap.from == KEY_HOME || remap.to == KEY_HOME {
                return Err(RemapError::EscapeKey);
            }
            if remaps[..index].iter().any(|seen| seen.from == remap.from) {
                return Err(RemapError::Duplicate(remap.from));
            }
        }
        Ok(Self {
            remaps: remaps.to_vec(),
        })
    }

    /// The key the host gets for `qt_key`.
    #[must_use]
    pub fn apply(&self, qt_key: u32) -> u32 {
        self.remaps
            .iter()
            .find(|remap| remap.from == qt_key)
            .map_or(qt_key, |remap| remap.to)
    }
}

/// The Mac preset for a Windows or Linux host: an Apple extended keyboard's
/// F13, F14 and F15 sit where a PC's Print Screen, Scroll Lock and Pause
/// are, and macOS reports a PC keyboard's three as F13–F15.
pub const MAC_PC_KEYS: [KeyRemap; 3] = [
    KeyRemap {
        from: KEY_F1 + 12,
        to: KEY_PRINT,
    },
    KeyRemap {
        from: KEY_F1 + 13,
        to: KEY_SCROLL_LOCK,
    },
    KeyRemap {
        from: KEY_F1 + 14,
        to: KEY_PAUSE,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_menu_lists_the_missing_keys_then_f13_to_f24() {
        let titles: Vec<_> = SendKey::ALL.iter().map(|key| key.title()).collect();
        assert_eq!(
            titles[..6],
            [
                "Print Screen",
                "Scroll Lock",
                "Pause",
                "Insert",
                "Ctrl+Alt+Del",
                "Menu"
            ]
        );
        assert_eq!(titles[6], "F13");
        assert_eq!(titles[17], "F24");
        assert_eq!(SendKey::Function(13).qt_key(), Some(KEY_F1 + 12));
        assert_eq!(SendKey::Function(24).qt_key(), Some(KEY_F1 + 23));
        assert_eq!(SendKey::Function(25).qt_key(), None);
        assert!(SendKey::Function(12).edges().is_empty());
    }

    #[test]
    fn a_sent_key_is_one_press_and_release_with_nothing_held() {
        for key in SendKey::ALL {
            let edges = key.edges();
            if key == SendKey::ControlAltDelete {
                let keys: Vec<_> = edges
                    .iter()
                    .map(|edge| (edge.qt_key, edge.pressed))
                    .collect();
                assert_eq!(
                    keys,
                    [
                        (KEY_CONTROL, true),
                        (KEY_ALT, true),
                        (KEY_DELETE, true),
                        (KEY_DELETE, false),
                        (KEY_ALT, false),
                        (KEY_CONTROL, false),
                    ]
                );
                continue;
            }
            assert_eq!(edges.len(), 2, "{key:?}");
            assert!(edges[0].pressed && !edges[1].pressed);
            assert_eq!(edges[0].qt_key, edges[1].qt_key);
            assert!(edges.iter().all(|edge| edge.modifiers == 0));
            assert!(remap_may_send(edges[0].qt_key), "{key:?}");
            assert_ne!(edges[0].qt_key, KEY_SYS_REQ);
        }
    }

    #[test]
    fn remaps_never_send_power_sleep_wake_or_sysrq() {
        // Qt's PowerOff, WakeUp, Sleep, PowerDown, Suspend, Hibernate,
        // Standby, LogOff, SysReq, a volume key.
        for target in [
            0x0100_00B7,
            0x0100_00B8,
            0x0102_0005,
            0x0100_010B,
            0x0100_010C,
            0x0100_0108,
            0x0100_0093,
            0x0100_0079,
            KEY_SYS_REQ,
            0x0100_0070,
        ] {
            assert_eq!(
                KeyRemapTable::new(&[KeyRemap {
                    from: KEY_F1 + 12,
                    to: target
                }]),
                Err(RemapError::ForbiddenTarget(target))
            );
        }
        assert_eq!(
            KeyRemapTable::new(&[KeyRemap {
                from: KEY_HOME,
                to: KEY_PRINT
            }]),
            Err(RemapError::EscapeKey)
        );
        let twice = [MAC_PC_KEYS[0], MAC_PC_KEYS[0]];
        assert_eq!(
            KeyRemapTable::new(&twice),
            Err(RemapError::Duplicate(KEY_F1 + 12))
        );
    }

    #[test]
    fn the_mac_preset_is_valid() {
        let table = KeyRemapTable::new(&MAC_PC_KEYS).expect("valid preset");
        assert_eq!(table.apply(KEY_F1 + 12), KEY_PRINT);
        assert_eq!(table.apply(KEY_F1 + 13), KEY_SCROLL_LOCK);
        assert_eq!(table.apply(KEY_F1 + 14), KEY_PAUSE);
        assert_eq!(table.apply(KEY_F1 + 15), KEY_F1 + 15);
    }
}
