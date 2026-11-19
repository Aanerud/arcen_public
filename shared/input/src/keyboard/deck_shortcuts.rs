// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The keyboard shortcuts a Deck keeps for itself during a session, on a Mac
//! keyboard and on a PC keyboard.
//!
//! Every other chord goes to the host. The two keyboards' chords are the
//! same actions with the same menu titles, so someone who moves between
//! Decks finds each where the other kept it. A PC chord is the Mac chord
//! transliterated, Command → Windows, Option → Alt, Control → Control
//! (`PARITY.md` Q-2), except where Windows itself owns the result or the
//! remote desktop clients PC users know agree on another chord
//! (`REFERENCE-INPUT.md` §1): Show Fullscreen is Ctrl+Alt+Break (Remote
//! Desktop's; Ctrl+Windows+F is Explorer's) and Ctrl+Alt+Enter (other remote
//! desktop clients', for keyboards without Break), Quit is Alt+F4, and a PC keyboard
//! adds Ctrl+Alt+End for Ctrl+Alt+Del, which Windows never lets an
//! application see. Remote Desktop's Ctrl+Alt+Home (Ctrl+Option+Home on a
//! Mac) gives the keyboard to this computer and back ([`super::capture`]).
//! Alt means the left Alt only: the right one is `AltGr`, which types
//! characters.
//!
//! Every chord here is a key a host application can never have, so the
//! list is short: releasing stuck keys, the keyboard modes and Paste as
//! Keystrokes are menu items without a chord, and F9 reaches the host. A
//! test holds every chord against [`super::PRO_APP_CHORDS`].

use arcen_protocol::keys::{KEY_Q, KEY_RETURN};

use super::pc::{KEY_END, KEY_F4, KEY_F12, KEY_HOME, KEY_PAUSE};

/// An action a Deck keeps for itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeckShortcut {
    /// Quit the Deck. In a session a tap belongs to the host and only a
    /// hold quits (Command+Q, Alt+F4).
    Quit,
    /// End the session.
    Disconnect,
    /// Type the clipboard's text as keystrokes. A menu item, no chord.
    PasteAsKeystrokes,
    /// Keyboard mode for this session: physical keys. A menu item, no
    /// chord.
    KeyboardPhysical,
    /// Keyboard mode for this session: characters. A menu item, no chord.
    KeyboardCharacters,
    /// Enter or leave fullscreen.
    ToggleFullscreen,
    /// Send Ctrl+Alt+Del to the host. A PC keyboard's own Ctrl+Alt+Del
    /// never reaches an application, so a PC Deck has a chord for it.
    SendSecureAttention,
    /// Give the keyboard to this computer (every key held on the host is
    /// released, nothing more reaches it, the system shortcuts act locally)
    /// or back to the host ([`super::KeyboardCapture::toggle`]).
    /// Ctrl+Alt+Home, Ctrl+Option+Home on a Mac.
    ReleaseKeyboard,
}

/// A chord on a Mac keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per modifier key of a chord"
)]
pub struct MacChord {
    /// Control.
    pub control: bool,
    /// Option.
    pub option: bool,
    /// Shift.
    pub shift: bool,
    /// Command.
    pub command: bool,
    /// The key identifier.
    pub key: u32,
}

/// A chord on a PC keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per modifier key of a chord"
)]
pub struct PcChord {
    /// Either Control.
    pub control: bool,
    /// The left Alt.
    pub alt: bool,
    /// Either Shift.
    pub shift: bool,
    /// Either Windows key.
    pub windows: bool,
    /// The key identifier.
    pub key: u32,
}

impl PcChord {
    const fn control_alt(key: u32) -> Self {
        Self {
            control: true,
            alt: true,
            shift: false,
            windows: false,
            key,
        }
    }
}

impl MacChord {
    /// Control, Option and Command as given (no Shift) with `key`.
    #[must_use]
    pub const fn new(control: bool, option: bool, command: bool, key: u32) -> Self {
        Self {
            control,
            option,
            shift: false,
            command,
            key,
        }
    }
}

/// The modifier keys held on a PC keyboard.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "one flag per modifier key on a PC keyboard"
)]
pub struct PcModifierState {
    /// Either Control.
    pub control: bool,
    /// Either Shift.
    pub shift: bool,
    /// The left Alt.
    pub alt: bool,
    /// The right Alt (`AltGr`).
    pub altgr: bool,
    /// Either Windows key.
    pub windows: bool,
}

impl DeckShortcut {
    /// Every action, in menu order.
    pub const ALL: [Self; 8] = [
        Self::Quit,
        Self::Disconnect,
        Self::PasteAsKeystrokes,
        Self::KeyboardPhysical,
        Self::KeyboardCharacters,
        Self::ToggleFullscreen,
        Self::SendSecureAttention,
        Self::ReleaseKeyboard,
    ];

    /// The menu title, the same on both Decks.
    #[must_use]
    pub const fn title(self) -> &'static str {
        match self {
            Self::Quit => "Quit Arcen Deck",
            Self::Disconnect => "Disconnect",
            Self::PasteAsKeystrokes => "Paste as Keystrokes",
            Self::KeyboardPhysical => "Keyboard: Physical Keys",
            Self::KeyboardCharacters => "Keyboard: Characters",
            Self::ToggleFullscreen => "Show Fullscreen",
            Self::SendSecureAttention => "Send Ctrl+Alt+Del",
            Self::ReleaseKeyboard => "Keyboard: Host or This Computer",
        }
    }

    /// The chord on a Mac keyboard, if it has one.
    #[must_use]
    pub const fn mac_chord(self) -> Option<MacChord> {
        Some(match self {
            Self::Quit => MacChord::new(false, false, true, KEY_Q),
            Self::Disconnect => MacChord::new(true, true, false, KEY_F12),
            Self::ToggleFullscreen => MacChord::new(true, false, true, 0x46),
            Self::ReleaseKeyboard => MacChord::new(true, true, false, KEY_HOME),
            Self::PasteAsKeystrokes
            | Self::KeyboardPhysical
            | Self::KeyboardCharacters
            | Self::SendSecureAttention => {
                return None;
            }
        })
    }

    /// The chords on a PC keyboard, the one menus show first.
    #[must_use]
    pub const fn pc_chords(self) -> &'static [PcChord] {
        const ALT_F4: PcChord = PcChord {
            control: false,
            alt: true,
            shift: false,
            windows: false,
            key: KEY_F4,
        };
        const DISCONNECT: [PcChord; 1] = [PcChord::control_alt(KEY_F12)];
        // The main Enter (Return) only: the keypad's Enter is another key.
        const FULLSCREEN: [PcChord; 2] = [
            PcChord::control_alt(KEY_PAUSE),
            PcChord::control_alt(KEY_RETURN),
        ];
        const SECURE_ATTENTION: [PcChord; 1] = [PcChord::control_alt(KEY_END)];
        const RELEASE_KEYBOARD: [PcChord; 1] = [PcChord::control_alt(KEY_HOME)];
        match self {
            Self::Quit => &[ALT_F4],
            Self::Disconnect => &DISCONNECT,
            Self::PasteAsKeystrokes | Self::KeyboardPhysical | Self::KeyboardCharacters => &[],
            Self::ToggleFullscreen => &FULLSCREEN,
            Self::SendSecureAttention => &SECURE_ATTENTION,
            Self::ReleaseKeyboard => &RELEASE_KEYBOARD,
        }
    }

    /// Whether the action has a chord on a PC keyboard or a Mac keyboard.
    #[must_use]
    pub const fn has_chord(self) -> bool {
        !self.pc_chords().is_empty() || self.mac_chord().is_some()
    }

    /// The action a chord pressed on a Mac keyboard is.
    #[must_use]
    pub fn for_mac_chord(pressed: MacChord) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.mac_chord() == Some(pressed))
    }

    /// The action a key press on a PC keyboard is, with `held` the modifiers
    /// held at it. `AltGr` held is never a Deck chord: it types characters.
    #[must_use]
    pub fn for_pc_key(qt_key: u32, held: PcModifierState) -> Option<Self> {
        if held.altgr {
            return None;
        }
        Self::ALL.into_iter().find(|action| {
            action.pc_chords().iter().any(|chord| {
                chord.key == qt_key
                    && chord.control == held.control
                    && chord.alt == held.alt
                    && chord.shift == held.shift
                    && chord.windows == held.windows
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn held(control: bool, alt: bool, shift: bool) -> PcModifierState {
        PcModifierState {
            control,
            shift,
            alt,
            altgr: false,
            windows: false,
        }
    }

    #[test]
    fn pc_chords_find_their_action() {
        for action in DeckShortcut::ALL {
            for chord in action.pc_chords() {
                let state = PcModifierState {
                    control: chord.control,
                    shift: chord.shift,
                    alt: chord.alt,
                    altgr: false,
                    windows: chord.windows,
                };
                assert_eq!(
                    DeckShortcut::for_pc_key(chord.key, state),
                    Some(action),
                    "{action:?}"
                );
            }
        }
    }

    #[test]
    fn host_chords_are_not_deck_chords() {
        // Ctrl+V, Ctrl+Alt+V, Alt+F, Ctrl+Alt+Del, Ctrl+F12.
        assert_eq!(
            DeckShortcut::for_pc_key(0x56, held(true, false, false)),
            None
        );
        assert_eq!(
            DeckShortcut::for_pc_key(0x56, held(true, true, false)),
            None
        );
        assert_eq!(
            DeckShortcut::for_pc_key(0x46, held(false, true, false)),
            None
        );
        assert_eq!(
            DeckShortcut::for_pc_key(0x0100_0007, held(true, true, false)),
            None
        );
        assert_eq!(
            DeckShortcut::for_pc_key(KEY_F12, held(true, false, false)),
            None
        );
    }

    #[test]
    fn altgr_types_rather_than_matching_a_chord() {
        let altgr = PcModifierState {
            control: true,
            alt: true,
            altgr: true,
            ..PcModifierState::default()
        };
        assert_eq!(DeckShortcut::for_pc_key(KEY_HOME, altgr), None);
    }

    #[test]
    fn f9_and_the_keyboard_mode_and_paste_chords_reach_the_host() {
        let f9 = super::super::pc::KEY_F9;
        for state in [held(false, false, false), held(true, true, true)] {
            assert_eq!(DeckShortcut::for_pc_key(f9, state), None);
        }
        let control_windows = PcModifierState {
            control: true,
            windows: true,
            ..PcModifierState::default()
        };
        for key in [0x4B, 0x55] {
            assert_eq!(DeckShortcut::for_pc_key(key, control_windows), None);
            assert_eq!(
                DeckShortcut::for_mac_chord(MacChord::new(true, false, true, key)),
                None
            );
        }
        let all = PcModifierState {
            alt: true,
            ..control_windows
        };
        assert_eq!(DeckShortcut::for_pc_key(0x56, all), None);
        assert_eq!(
            DeckShortcut::for_mac_chord(MacChord::new(true, true, true, 0x56)),
            None
        );
        assert_eq!(
            DeckShortcut::for_mac_chord(MacChord::new(false, false, false, f9)),
            None
        );
        for action in [
            DeckShortcut::PasteAsKeystrokes,
            DeckShortcut::KeyboardPhysical,
            DeckShortcut::KeyboardCharacters,
        ] {
            assert!(!action.has_chord(), "{action:?}");
        }
    }

    #[test]
    fn a_pc_chord_is_the_mac_chord_transliterated() {
        for action in DeckShortcut::ALL {
            let Some(mac) = action.mac_chord() else {
                continue;
            };
            // Windows' own keys: Alt+F4 quits, Ctrl+Alt+Break is fullscreen.
            if matches!(action, DeckShortcut::Quit | DeckShortcut::ToggleFullscreen) {
                continue;
            }
            let pc = action.pc_chords()[0];
            assert_eq!(
                (pc.control, pc.alt, pc.shift, pc.windows, pc.key),
                (mac.control, mac.option, mac.shift, mac.command, mac.key),
                "{action:?}"
            );
        }
        assert_eq!(
            DeckShortcut::for_mac_chord(MacChord::new(true, true, false, KEY_HOME)),
            Some(DeckShortcut::ReleaseKeyboard)
        );
    }

    #[test]
    fn the_reference_chords_of_a_pc_deck() {
        let control_alt = held(true, true, false);
        // Fullscreen: Remote Desktop's Ctrl+Alt+Break and the common
        // Ctrl+Alt+Enter; the keypad's Enter is not the chord.
        assert_eq!(
            DeckShortcut::for_pc_key(KEY_PAUSE, control_alt),
            Some(DeckShortcut::ToggleFullscreen)
        );
        assert_eq!(
            DeckShortcut::for_pc_key(KEY_RETURN, control_alt),
            Some(DeckShortcut::ToggleFullscreen)
        );
        assert_eq!(
            DeckShortcut::for_pc_key(super::super::pc::KEY_ENTER, control_alt),
            None
        );
        assert_eq!(DeckShortcut::ToggleFullscreen.pc_chords()[0].key, KEY_PAUSE);
        // Remote Desktop's Ctrl+Alt+Home gives the keyboard to this
        // computer and back.
        assert_eq!(
            DeckShortcut::for_pc_key(KEY_HOME, control_alt),
            Some(DeckShortcut::ReleaseKeyboard)
        );
        // Windows+Escape stays the host's.
        let windows = PcModifierState {
            windows: true,
            ..PcModifierState::default()
        };
        assert_eq!(DeckShortcut::for_pc_key(0x0100_0000, windows), None);
    }
}
