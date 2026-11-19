// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Chords professional applications on a host depend on.
//!
//! Every chord a Deck keeps for itself is a chord these applications can
//! never have. The list is what artists and editors reported and what the
//! applications' default keymaps bind; the test below fails when a Deck
//! chord ([`super::DeckShortcut`], either keyboard) lands on one of them.

use arcen_protocol::keys::{MOD_ALT, MOD_CTRL, MOD_KEYPAD, MOD_META, MOD_SHIFT};

use super::pc::{KEY_ENTER, KEY_F1, KEY_F9, KEY_PRINT, KEY_SCROLL_LOCK};

/// `(key identifier, modifiers as MOD_* bits, what it does)`.
pub const PRO_APP_CHORDS: &[(u32, u32, &str)] = &[
    (0x20, MOD_SHIFT, "Shift+Space -- Flame pan"),
    (0x20, MOD_ALT, "Alt+Space -- Flame zoom"),
    (0x5B, 0, "[ -- decrease brush"),
    (0x5D, 0, "] -- increase brush"),
    (0x57, 0, "W -- translate gizmo"),
    (0x45, 0, "E -- rotate gizmo"),
    (0x52, 0, "R -- scale gizmo"),
    (0x0100_0012, 0, "Left -- previous frame"),
    (0x0100_0014, 0, "Right -- next frame"),
    (0x0100_0012, MOD_SHIFT, "Shift+Left -- previous keyframe"),
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
    (0x4B, MOD_CTRL | MOD_META, "Ctrl+Super+K -- a Flame hotkey"),
    (0x55, MOD_CTRL | MOD_META, "Ctrl+Super+U -- a Flame hotkey"),
    (KEY_PRINT, 0, "Print Screen -- a Flame hotkey"),
    (KEY_SCROLL_LOCK, 0, "Scroll Lock -- a Flame hotkey"),
    (KEY_F1, MOD_CTRL, "Ctrl+F1 -- context help"),
    (KEY_F1 + 7, 0, "F8 -- Maya component mode"),
    (KEY_F9, 0, "F9 -- Maya vertex, Resolve insert edit"),
    (KEY_F1 + 9, 0, "F10 -- Maya edge, Resolve overwrite edit"),
    (KEY_F1 + 10, 0, "F11 -- Maya face, Resolve replace edit"),
    (KEY_F1 + 11, 0, "F12 -- Resolve place on top"),
    (KEY_F9, MOD_SHIFT, "Shift+F9 -- Resolve"),
    (
        KEY_ENTER,
        MOD_KEYPAD,
        "Keypad Enter -- Nuke, distinct from Return",
    ),
    (0x5D, MOD_CTRL, "Ctrl+] -- brightness up"),
    (0x5B, MOD_CTRL, "Ctrl+[ -- brightness down"),
    (0x0100_0001, MOD_ALT, "Alt+Tab -- switch host windows"),
    (
        0x0100_0001,
        MOD_ALT | MOD_SHIFT,
        "Alt+Shift+Tab -- switch host windows back",
    ),
];

#[cfg(test)]
mod tests {
    use super::super::DeckShortcut;
    use super::*;

    /// Control, Alt (Option), Shift and Meta (Windows, Command) as MOD bits.
    fn modifiers(held: [bool; 4]) -> u32 {
        held.into_iter()
            .zip([MOD_CTRL, MOD_ALT, MOD_SHIFT, MOD_META])
            .filter(|(held, _)| *held)
            .fold(0, |bits, (_, bit)| bits | bit)
    }

    /// No chord a Deck keeps while the host has the keyboard is a chord a
    /// professional application uses, on a PC keyboard or a Mac one. The
    /// quit chord is a hold: its tap goes to the host.
    #[test]
    fn no_deck_chord_takes_a_pro_app_chord() {
        for action in DeckShortcut::ALL {
            if action == DeckShortcut::Quit {
                continue;
            }
            let mut chords: Vec<(u32, u32)> = action
                .pc_chords()
                .iter()
                .map(|chord| {
                    (
                        chord.key,
                        modifiers([chord.control, chord.alt, chord.shift, chord.windows]),
                    )
                })
                .collect();
            if let Some(chord) = action.mac_chord() {
                chords.push((
                    chord.key,
                    modifiers([chord.control, chord.option, chord.shift, chord.command]),
                ));
            }
            for (key, held) in chords {
                for (pro_key, pro_held, description) in PRO_APP_CHORDS {
                    assert!(
                        !(key == *pro_key && held == *pro_held),
                        "{action:?} takes {description}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_escape_chord_is_not_a_pro_app_chord() {
        let home = super::super::pc::KEY_HOME;
        assert!(
            PRO_APP_CHORDS
                .iter()
                .all(|(key, held, _)| !(*key == home && *held == MOD_CTRL | MOD_ALT))
        );
    }
}
