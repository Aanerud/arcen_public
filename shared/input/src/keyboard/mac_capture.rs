// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Which Mac system shortcuts a macOS Deck takes for the host.
//!
//! macOS acts on ⌘Tab, ⌘Space, Mission Control and the other system chords
//! before any window sees them. A macOS Deck can take them only with an
//! active event tap (it needs the Accessibility permission), and only while
//! the host has the keyboard ([`super::KeyboardCapture`]) and a session
//! window is key. The tap is a thin adapter: it reads a key event's virtual
//! key code and flags, asks [`MacTapRouter::route`], and either lets the
//! event go on to the system or hands it to the Deck's own keyboard path.
//!
//! What it takes while the remote computer has the keyboard (the keyboard
//! switch, Ctrl+Option+Home by default, decides; [`super::KeyboardSwitchChord`])
//! and the policy says it gets the system shortcuts:
//!
//! | Chord | macOS does |
//! | --- | --- |
//! | ⌘Tab, ⇧⌘Tab | app switcher |
//! | ⌘\`, ⇧⌘\` | next window of the app |
//! | ⌘Space, ⌥⌘Space | Spotlight, Finder search |
//! | ⌃↑, ⌃↓ | Mission Control, application windows |
//! | ⌃←, ⌃→ | the previous or next Space |
//! | ⇧⌘3, ⇧⌘4, ⇧⌘5 (with or without ⌃) | screenshots |
//! | ⌥⌘D | show or hide the Dock |
//!
//! Never taken: ⌥⌘Esc (Force Quit stays the way out), ⌃⌘Q (lock), the
//! Deck's own chords (the keyboard switch, ⌘Q's hold-to-quit), media, brightness and
//! power keys and the Globe key (not key events a tap reads), and anything
//! while Secure Event Input is on (the system gives a tap no key then, and
//! the adapter lets every event go when it sees it on).
//!
//! A key's release goes where its press went, so a chord taken for the
//! host is never half delivered to the Mac.

/// A Mac virtual key code (`kVK_*`, `HIToolbox/Events.h`).
pub type MacKeyCode = u16;

/// `kVK_ANSI_D`.
pub const MAC_KEY_D: MacKeyCode = 0x02;
/// `kVK_ANSI_Q`.
pub const MAC_KEY_Q: MacKeyCode = 0x0C;
/// `kVK_ANSI_3`.
pub const MAC_KEY_3: MacKeyCode = 0x14;
/// `kVK_ANSI_4`.
pub const MAC_KEY_4: MacKeyCode = 0x15;
/// `kVK_ANSI_5`.
pub const MAC_KEY_5: MacKeyCode = 0x17;
/// `kVK_Tab`.
pub const MAC_KEY_TAB: MacKeyCode = 0x30;
/// `kVK_Space`.
pub const MAC_KEY_SPACE: MacKeyCode = 0x31;
/// `kVK_ANSI_Grave`: the key that macOS's "next window" chord names.
pub const MAC_KEY_GRAVE: MacKeyCode = 0x32;
/// `kVK_Escape`.
pub const MAC_KEY_ESCAPE: MacKeyCode = 0x35;
/// `kVK_Home`.
pub const MAC_KEY_HOME: MacKeyCode = 0x73;
/// `kVK_LeftArrow`.
pub const MAC_KEY_LEFT: MacKeyCode = 0x7B;
/// `kVK_RightArrow`.
pub const MAC_KEY_RIGHT: MacKeyCode = 0x7C;
/// `kVK_DownArrow`.
pub const MAC_KEY_DOWN: MacKeyCode = 0x7D;
/// `kVK_UpArrow`.
pub const MAC_KEY_UP: MacKeyCode = 0x7E;

/// The modifier keys held at a key event, from a `CGEventFlags` value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the four independent Mac modifier keys"
)]
pub struct MacTapModifiers {
    /// ⌘.
    pub command: bool,
    /// ⌃.
    pub control: bool,
    /// ⌥.
    pub option: bool,
    /// ⇧.
    pub shift: bool,
}

impl MacTapModifiers {
    /// `kCGEventFlagMaskShift`.
    pub const CG_SHIFT: u64 = 0x0002_0000;
    /// `kCGEventFlagMaskControl`.
    pub const CG_CONTROL: u64 = 0x0004_0000;
    /// `kCGEventFlagMaskAlternate`.
    pub const CG_OPTION: u64 = 0x0008_0000;
    /// `kCGEventFlagMaskCommand`.
    pub const CG_COMMAND: u64 = 0x0010_0000;

    /// The modifiers in a `CGEventFlags` value. Caps Lock, the Fn flag the
    /// arrow and navigation keys always carry, and the device-specific bits
    /// are not modifiers here.
    #[must_use]
    pub const fn from_cg_flags(flags: u64) -> Self {
        Self {
            command: flags & Self::CG_COMMAND != 0,
            control: flags & Self::CG_CONTROL != 0,
            option: flags & Self::CG_OPTION != 0,
            shift: flags & Self::CG_SHIFT != 0,
        }
    }

    const fn is(self, command: bool, control: bool, option: bool) -> bool {
        self.command == command && self.control == control && self.option == option
    }
}

/// A Mac system shortcut a Deck can take for the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacSystemChord {
    /// ⌘Tab and ⇧⌘Tab: the host's app switcher (Alt+Tab on Windows and
    /// Linux, ⌘Tab on a Mac, [`super::ShortcutTranslator`]).
    AppSwitcher,
    /// ⌘\` and ⇧⌘\`.
    NextWindow,
    /// ⌘Space and ⌥⌘Space.
    Spotlight,
    /// ⌃↑.
    MissionControl,
    /// ⌃↓.
    ApplicationWindows,
    /// ⌃← and ⌃→: the Mac's Spaces.
    Spaces,
    /// ⇧⌘3, ⇧⌘4, ⇧⌘5, with or without ⌃.
    Screenshot,
    /// ⌥⌘D.
    Dock,
}

impl MacSystemChord {
    /// The system chord a key press is, if any. Exact modifier sets only:
    /// ⌃⌥← is not ⌃← and stays an ordinary key.
    #[must_use]
    pub const fn for_key(key_code: MacKeyCode, held: MacTapModifiers) -> Option<Self> {
        Some(match key_code {
            MAC_KEY_TAB if held.is(true, false, false) => Self::AppSwitcher,
            MAC_KEY_GRAVE if held.is(true, false, false) => Self::NextWindow,
            MAC_KEY_SPACE if !held.shift && !held.control && held.command => Self::Spotlight,
            MAC_KEY_UP if held.is(false, true, false) && !held.shift => Self::MissionControl,
            MAC_KEY_DOWN if held.is(false, true, false) && !held.shift => Self::ApplicationWindows,
            MAC_KEY_LEFT | MAC_KEY_RIGHT if held.is(false, true, false) && !held.shift => {
                Self::Spaces
            }
            MAC_KEY_3 | MAC_KEY_4 | MAC_KEY_5 if held.command && held.shift && !held.option => {
                Self::Screenshot
            }
            MAC_KEY_D if held.is(true, false, true) && !held.shift => Self::Dock,
            _ => return None,
        })
    }
}

/// One key event a tap sees (`kCGEventKeyDown`/`kCGEventKeyUp`; modifier
/// changes always go on to the system and reach the window as usual).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MacTapEvent {
    /// `kCGKeyboardEventKeycode`.
    pub key_code: MacKeyCode,
    /// Down or up.
    pub pressed: bool,
    /// `kCGKeyboardEventAutorepeat`.
    pub repeat: bool,
    /// The modifiers held.
    pub modifiers: MacTapModifiers,
}

/// Where a tapped key event goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapRoute {
    /// On to the system, untouched (the default, on any doubt).
    PassThrough,
    /// To the host, through the Deck's own keyboard path; the system never
    /// sees it.
    Host,
}

/// The tap's routing: which events it takes, and a key's release where its
/// press went.
#[derive(Debug, Clone, Default)]
pub struct MacTapRouter {
    /// Key codes whose press went to the host and whose release has not
    /// come yet (a virtual key code is 0..=127).
    taken: u128,
}

impl MacTapRouter {
    /// Nothing taken.
    #[must_use]
    pub const fn new() -> Self {
        Self { taken: 0 }
    }

    /// Routes one key event. `capturing`: the host has the keyboard, a
    /// session window is key, the policy takes the system shortcuts and
    /// Secure Event Input is off. A release of a key whose press was taken
    /// is taken too, whatever `capturing` says now (the Deck releases the
    /// key on the host).
    pub fn route(&mut self, capturing: bool, event: MacTapEvent) -> TapRoute {
        if event.key_code > 127 {
            return TapRoute::PassThrough;
        }
        let bit = 1u128 << event.key_code;
        let held = self.taken & bit != 0;
        if !event.pressed {
            self.taken &= !bit;
            return if held {
                TapRoute::Host
            } else {
                TapRoute::PassThrough
            };
        }
        if held {
            // A repeat (or a press with no release seen) of a taken key.
            return TapRoute::Host;
        }
        if !capturing || event.repeat {
            return TapRoute::PassThrough;
        }
        if MacSystemChord::for_key(event.key_code, event.modifiers).is_some() {
            self.taken |= bit;
            TapRoute::Host
        } else {
            TapRoute::PassThrough
        }
    }

    /// Forgets every taken key (the tap was removed; the Deck released the
    /// keys on the host).
    pub const fn reset(&mut self) {
        self.taken = 0;
    }

    /// Whether any taken key is still down.
    #[must_use]
    pub const fn holds_any(&self) -> bool {
        self.taken != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(
        clippy::fn_params_excessive_bools,
        reason = "a test shorthand for the four modifier keys"
    )]
    const fn held(command: bool, control: bool, option: bool, shift: bool) -> MacTapModifiers {
        MacTapModifiers {
            command,
            control,
            option,
            shift,
        }
    }
    const CMD: MacTapModifiers = held(true, false, false, false);
    const CTRL: MacTapModifiers = held(false, true, false, false);

    const fn press(key_code: MacKeyCode, modifiers: MacTapModifiers) -> MacTapEvent {
        MacTapEvent {
            key_code,
            pressed: true,
            repeat: false,
            modifiers,
        }
    }
    const fn release(key_code: MacKeyCode, modifiers: MacTapModifiers) -> MacTapEvent {
        MacTapEvent {
            key_code,
            pressed: false,
            repeat: false,
            modifiers,
        }
    }

    #[test]
    fn flags_read_the_four_modifiers_and_ignore_fn_and_caps_lock() {
        let flags =
            MacTapModifiers::CG_COMMAND | MacTapModifiers::CG_SHIFT | 0x0080_0000 | 0x1_0000;
        assert_eq!(
            MacTapModifiers::from_cg_flags(flags),
            held(true, false, false, true)
        );
    }

    #[test]
    fn the_system_chords_are_recognised_exactly() {
        use MacSystemChord as C;
        for (key, modifiers, chord) in [
            (MAC_KEY_TAB, CMD, Some(C::AppSwitcher)),
            (
                MAC_KEY_TAB,
                held(true, false, false, true),
                Some(C::AppSwitcher),
            ),
            (MAC_KEY_GRAVE, CMD, Some(C::NextWindow)),
            (MAC_KEY_SPACE, CMD, Some(C::Spotlight)),
            (
                MAC_KEY_SPACE,
                held(true, false, true, false),
                Some(C::Spotlight),
            ),
            (MAC_KEY_UP, CTRL, Some(C::MissionControl)),
            (MAC_KEY_DOWN, CTRL, Some(C::ApplicationWindows)),
            (MAC_KEY_LEFT, CTRL, Some(C::Spaces)),
            (MAC_KEY_RIGHT, CTRL, Some(C::Spaces)),
            (
                MAC_KEY_3,
                held(true, false, false, true),
                Some(C::Screenshot),
            ),
            (
                MAC_KEY_4,
                held(true, true, false, true),
                Some(C::Screenshot),
            ),
            (MAC_KEY_D, held(true, false, true, false), Some(C::Dock)),
            // Not system chords: ordinary keys for the window.
            (MAC_KEY_TAB, held(false, false, true, false), None),
            (MAC_KEY_TAB, CTRL, None),
            (MAC_KEY_SPACE, CTRL, None),
            (MAC_KEY_LEFT, held(false, true, true, false), None),
            (MAC_KEY_LEFT, CMD, None),
            (MAC_KEY_3, CMD, None),
            (MAC_KEY_D, CMD, None),
        ] {
            assert_eq!(
                MacSystemChord::for_key(key, modifiers),
                chord,
                "{key:#x} {modifiers:?}"
            );
        }
    }

    #[test]
    fn the_way_out_and_the_decks_own_chords_are_never_taken() {
        let mut router = MacTapRouter::new();
        for event in [
            press(MAC_KEY_ESCAPE, held(true, false, true, false)),
            press(MAC_KEY_Q, held(true, true, false, false)),
            press(MAC_KEY_Q, CMD),
            press(MAC_KEY_HOME, held(false, true, true, false)),
        ] {
            assert_eq!(router.route(true, event), TapRoute::PassThrough);
        }
        assert!(!router.holds_any());
    }

    #[test]
    fn spaces_follow_the_keyboard_switch_like_every_other_chord() {
        let mut router = MacTapRouter::new();
        let left = press(MAC_KEY_LEFT, CTRL);
        assert_eq!(router.route(true, left), TapRoute::Host);
        assert_eq!(
            router.route(true, release(MAC_KEY_LEFT, CTRL)),
            TapRoute::Host
        );
        assert_eq!(router.route(false, left), TapRoute::PassThrough);
    }

    #[test]
    fn nothing_is_taken_while_not_capturing() {
        let mut router = MacTapRouter::new();
        assert_eq!(
            router.route(false, press(MAC_KEY_TAB, CMD)),
            TapRoute::PassThrough
        );
        assert_eq!(
            router.route(false, release(MAC_KEY_TAB, CMD)),
            TapRoute::PassThrough
        );
    }

    #[test]
    fn a_release_goes_where_its_press_went() {
        let mut router = MacTapRouter::new();
        assert_eq!(router.route(true, press(MAC_KEY_TAB, CMD)), TapRoute::Host);
        // A repeat of the held key, then capture ends: the release is still
        // the host's.
        let repeat = MacTapEvent {
            repeat: true,
            ..press(MAC_KEY_TAB, CMD)
        };
        assert_eq!(router.route(true, repeat), TapRoute::Host);
        assert_eq!(
            router.route(false, release(MAC_KEY_TAB, MacTapModifiers::default())),
            TapRoute::Host
        );
        assert!(!router.holds_any());
        // A press that went to the Mac keeps its release there, even if the
        // chord's modifiers came down meanwhile.
        assert_eq!(
            router.route(true, press(MAC_KEY_TAB, MacTapModifiers::default())),
            TapRoute::PassThrough
        );
        assert_eq!(
            router.route(true, release(MAC_KEY_TAB, CMD)),
            TapRoute::PassThrough
        );
    }

    #[test]
    fn an_autorepeat_alone_never_starts_a_chord_and_reset_forgets() {
        let mut router = MacTapRouter::new();
        let repeat = MacTapEvent {
            repeat: true,
            ..press(MAC_KEY_TAB, CMD)
        };
        assert_eq!(router.route(true, repeat), TapRoute::PassThrough);
        assert_eq!(
            router.route(true, press(MAC_KEY_SPACE, CMD)),
            TapRoute::Host
        );
        router.reset();
        assert!(!router.holds_any());
        assert_eq!(
            router.route(
                true,
                MacTapEvent {
                    key_code: 200,
                    ..press(MAC_KEY_TAB, CMD)
                }
            ),
            TapRoute::PassThrough
        );
    }
}
