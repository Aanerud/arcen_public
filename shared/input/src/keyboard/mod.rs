// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keyboard policy between a client's keys (a Mac's or a PC's) and a host's keys.
//!
//! A Mac keyboard has Command, Control and two Option keys; a Windows or
//! Linux desktop has Control, Alt, `AltGr` and the Windows (Super) key. Which
//! host key a Mac key becomes is a policy of the destination, decided here
//! once for every client:
//!
//! - [`resolve_keyboard_policy`] turns the host's platform, what its hello
//!   accepts, and a connection's settings into a [`KeyboardPolicy`];
//! - [`ShortcutTranslator`] applies one to a stream of physical key edges,
//!   producing the host key edges and modifier masks to send;
//! - [`scancode`] maps key identifiers to PC scan codes for a host that
//!   injects physical keys (Windows);
//! - [`host_keys`] holds every Pier's code for the PC keys outside the
//!   main block and F1–F12 (Insert, Print Screen, Scroll Lock, Pause, Menu,
//!   F13–F24), what a Pier's hello says of them, and what a Deck reads.
//!
//! A PC keyboard (a Windows Deck's) has the same policy in its PC flavour:
//!
//! - [`pc`] names a PC keyboard's keys by position, the inverse of
//!   [`scancode`] plus the keypad, locks and input method keys;
//! - [`windows`] reads Windows key messages into those keys;
//! - [`resolve_pc_keyboard_policy`] and [`PcKeyboardInput`] turn its key
//!   edges into the same host key edges, so a shortcut lands on the host
//!   alike from either Deck;
//! - [`DeckShortcut`] lists the chords both Decks keep for themselves, and
//!   [`grab_takes`] what a PC Deck's keyboard grab takes from its system;
//! - [`KeyboardCapture`] decides who has the keyboard (the host or this
//!   computer) and [`KeyboardCapturePolicy`] when the system shortcuts go
//!   to the host;
//! - [`SendKey`] is the Send Key menu, [`KeyRemapTable`] a validated remap
//!   table, and [`PRO_APP_CHORDS`] the chords no Deck chord may take.

mod capture;
mod deck_shortcuts;
pub mod host_keys;
pub mod pc;
mod pc_input;
mod pro_app_chords;
pub mod scancode;
mod send_keys;
mod shortcuts;
pub mod windows;

use arcen_protocol::messages::HostPlatform;

pub use capture::{
    CaptureChange, ClickRoute, KeyboardCapture, KeyboardCapturePolicy, KeyboardOwner,
    KeyboardStatus,
};
pub use deck_shortcuts::{DeckShortcut, MacChord, PcChord, PcModifierState};
pub use pc_input::{
    AltGrKey, HookRoute, HookRouter, MacHostCommandKey, PcKeyAction, PcKeyEvent, PcKeyboardInput,
    PcKeyboardSettings, SystemChord, grab_takes, resolve_pc_keyboard_policy,
};
pub use pro_app_chords::PRO_APP_CHORDS;
pub use send_keys::{
    KEY_INSERT, KEY_SYS_REQ, KeyRemap, KeyRemapTable, MAC_PC_KEYS, RemapError, SendKey,
    remap_may_send,
};
pub use shortcuts::{
    DeckKey, DeckModifiers, HostKeyEdge, ShortcutTranslator, WINDOWS_APP_CONTROL_KEYS,
};

/// What a connection asks Command and Control to be.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShortcutPreference {
    /// The host platform's default: Command is Command on a Mac host, the
    /// Windows key with Mac edit shortcuts on a Windows host
    /// ([`ShortcutMode::WindowsApp`]), and Control on a Linux host.
    #[default]
    Automatic,
    /// [`ShortcutMode::WindowsApp`].
    WindowsApp,
    /// [`ShortcutMode::SwapCommandControl`].
    SwapCommandControl,
    /// [`ShortcutMode::CommandIsControl`].
    CommandIsControl,
}

impl ShortcutPreference {
    /// Every preference, in the order a settings menu lists them.
    pub const ALL: [Self; 4] = [
        Self::Automatic,
        Self::WindowsApp,
        Self::SwapCommandControl,
        Self::CommandIsControl,
    ];

    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Automatic => "automatic",
            Self::WindowsApp => "windows_app",
            Self::SwapCommandControl => "swap_command_control",
            Self::CommandIsControl => "command_is_control",
        }
    }

    /// The preference a settings token names.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|preference| preference.token() == token)
    }
}

/// What Command and Control become on the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutMode {
    /// Command is the host's Meta key (a Mac host's Command, the Windows
    /// key, Super); Control is Control.
    CommandIsMeta,
    /// Command and Control are both Control (Arcen's Linux default).
    CommandIsControl,
    /// Command is Control and Control is the Windows (Super) key, for every
    /// key (as Parsec's and `RustDesk`'s swap).
    SwapCommandControl,
    /// Microsoft Windows App: Command is the Windows key, except that
    /// Command with C, X, V, A, Z, F or W is sent as Control with that key.
    WindowsApp,
}

/// A connection's keyboard settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardSettings {
    /// Command and Control.
    pub shortcuts: ShortcutPreference,
    /// The left Option key is `AltGr` too (it always is Alt otherwise), for
    /// a host whose layout types characters with `AltGr`.
    pub left_option_altgr: bool,
    /// The connection's keyboard mode; `None` follows the application-wide
    /// typing setting.
    pub typing: Option<crate::text::KeyboardMode>,
}

/// The resolved keyboard policy for one host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent per-key facts of one host, not a state machine"
)]
pub struct KeyboardPolicy {
    /// Command and Control.
    pub shortcuts: ShortcutMode,
    /// The right Option key is `AltGr` rather than Alt.
    pub right_option_altgr: bool,
    /// The left Option key is `AltGr` rather than Alt.
    pub left_option_altgr: bool,
    /// The ISO key next to left Shift is sent as its own key
    /// (`KEY_INTL_BACKSLASH`).
    pub intl_backslash: bool,
    /// The host keeps a Meta (Windows key) release after pointer input from
    /// opening Start itself (`meta_pointer_mask`); otherwise the translator
    /// taps Control before such a release.
    pub host_masks_meta_release: bool,
    /// A Mac keyboard's F13, F14 and F15 are Print Screen, Scroll Lock and
    /// Pause on the host ([`mac_key_on_host`]): a PC host.
    pub pc_print_keys: bool,
}

impl KeyboardPolicy {
    /// The policy every client applied before hosts said what they are:
    /// Command is Control and Option is Alt.
    pub const LEGACY: Self = Self {
        shortcuts: ShortcutMode::CommandIsControl,
        right_option_altgr: false,
        left_option_altgr: false,
        intl_backslash: false,
        host_masks_meta_release: false,
        pc_print_keys: false,
    };

    /// This policy, for a host that does (or does not) mask a pointer-only
    /// Meta release itself.
    #[must_use]
    pub const fn with_host_meta_mask(mut self, host_masks_meta_release: bool) -> Self {
        self.host_masks_meta_release = host_masks_meta_release;
        self
    }
}

impl Default for KeyboardPolicy {
    fn default() -> Self {
        Self::LEGACY
    }
}

/// The keyboard policy for a host.
///
/// A Mac host keeps Command as Command and Option as Option whatever the
/// settings say: its shortcuts are Command shortcuts. A Windows host gets
/// the Microsoft Windows App mapping by default, the right Option key as
/// `AltGr` (and the left one when the setting asks) when its hello accepts
/// `extended_keys`. A Linux host keeps Command as Control and Option as Alt
/// by default, as does a host that does not say what it is. The ISO key is
/// its own key wherever the host accepts `extended_keys`.
#[must_use]
pub fn resolve_keyboard_policy(
    platform: HostPlatform,
    extended_keys: bool,
    settings: KeyboardSettings,
) -> KeyboardPolicy {
    let chosen = |default| match settings.shortcuts {
        ShortcutPreference::Automatic => default,
        ShortcutPreference::WindowsApp => ShortcutMode::WindowsApp,
        ShortcutPreference::SwapCommandControl => ShortcutMode::SwapCommandControl,
        ShortcutPreference::CommandIsControl => ShortcutMode::CommandIsControl,
    };
    let (shortcuts, altgr) = match platform {
        HostPlatform::MacOs => (ShortcutMode::CommandIsMeta, false),
        HostPlatform::Windows => (chosen(ShortcutMode::WindowsApp), extended_keys),
        HostPlatform::Linux | HostPlatform::Unknown => {
            (chosen(ShortcutMode::CommandIsControl), false)
        }
    };
    KeyboardPolicy {
        shortcuts,
        right_option_altgr: altgr,
        left_option_altgr: altgr && settings.left_option_altgr,
        intl_backslash: extended_keys,
        host_masks_meta_release: false,
        pc_print_keys: matches!(platform, HostPlatform::Windows | HostPlatform::Linux),
    }
}

/// The key a Mac keyboard's key is on the host. A Mac keyboard has no Print
/// Screen, Scroll Lock or Pause: an Apple extended keyboard has F13, F14 and
/// F15 in their places, and macOS reports a PC keyboard's Print Screen,
/// Scroll Lock and Pause as F13, F14 and F15. On a PC host
/// ([`KeyboardPolicy::pc_print_keys`]) they are those keys again; a Mac host
/// keeps F13–F15.
#[must_use]
pub const fn mac_key_on_host(qt_key: u32, policy: KeyboardPolicy) -> u32 {
    if !policy.pc_print_keys {
        return qt_key;
    }
    match qt_key {
        MAC_F13 => pc::KEY_PRINT,
        MAC_F14 => pc::KEY_SCROLL_LOCK,
        MAC_F15 => pc::KEY_PAUSE,
        other => other,
    }
}

/// `Qt::Key_F13`.
const MAC_F13: u32 = pc::KEY_F1 + 12;
/// `Qt::Key_F14`.
const MAC_F14: u32 = pc::KEY_F1 + 13;
/// `Qt::Key_F15`.
const MAC_F15: u32 = pc::KEY_F1 + 14;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_macs_f13_to_f15_are_print_scroll_lock_and_pause_on_a_pc_host() {
        for platform in [HostPlatform::Windows, HostPlatform::Linux] {
            let policy = resolve_keyboard_policy(platform, true, KeyboardSettings::default());
            assert_eq!(mac_key_on_host(pc::KEY_F1 + 12, policy), pc::KEY_PRINT);
            assert_eq!(
                mac_key_on_host(pc::KEY_F1 + 13, policy),
                pc::KEY_SCROLL_LOCK
            );
            assert_eq!(mac_key_on_host(pc::KEY_F1 + 14, policy), pc::KEY_PAUSE);
            assert_eq!(mac_key_on_host(pc::KEY_F1 + 15, policy), pc::KEY_F1 + 15);
            assert_eq!(mac_key_on_host(0x41, policy), 0x41);
        }
        let windows =
            resolve_keyboard_policy(HostPlatform::Windows, true, KeyboardSettings::default());
        for remap in MAC_PC_KEYS {
            assert_eq!(mac_key_on_host(remap.from, windows), remap.to);
        }
        assert!(KeyRemapTable::new(&MAC_PC_KEYS).is_ok());
        let mac = resolve_keyboard_policy(HostPlatform::MacOs, true, KeyboardSettings::default());
        assert_eq!(mac_key_on_host(pc::KEY_F1 + 12, mac), pc::KEY_F1 + 12);
        assert_eq!(
            mac_key_on_host(pc::KEY_F1 + 12, KeyboardPolicy::LEGACY),
            pc::KEY_F1 + 12
        );
    }

    const fn settings(shortcuts: ShortcutPreference, left_option_altgr: bool) -> KeyboardSettings {
        KeyboardSettings {
            shortcuts,
            left_option_altgr,
            typing: None,
        }
    }

    #[test]
    fn each_platform_has_its_default() {
        let automatic = KeyboardSettings::default();
        let mac = resolve_keyboard_policy(HostPlatform::MacOs, true, automatic);
        assert_eq!(mac.shortcuts, ShortcutMode::CommandIsMeta);
        assert!(!mac.right_option_altgr && mac.intl_backslash);
        let windows = resolve_keyboard_policy(HostPlatform::Windows, true, automatic);
        assert_eq!(windows.shortcuts, ShortcutMode::WindowsApp);
        assert!(windows.right_option_altgr && !windows.left_option_altgr);
        assert_eq!(
            resolve_keyboard_policy(HostPlatform::Unknown, false, automatic),
            KeyboardPolicy::LEGACY
        );
        assert_eq!(
            resolve_keyboard_policy(HostPlatform::Linux, false, automatic),
            KeyboardPolicy {
                pc_print_keys: true,
                ..KeyboardPolicy::LEGACY
            }
        );
    }

    #[test]
    fn settings_apply_except_on_a_mac_host() {
        let swap = settings(ShortcutPreference::SwapCommandControl, true);
        assert_eq!(
            resolve_keyboard_policy(HostPlatform::Windows, true, swap).shortcuts,
            ShortcutMode::SwapCommandControl
        );
        assert!(resolve_keyboard_policy(HostPlatform::Windows, true, swap).left_option_altgr);
        assert_eq!(
            resolve_keyboard_policy(HostPlatform::Linux, true, swap).shortcuts,
            ShortcutMode::SwapCommandControl
        );
        assert!(!resolve_keyboard_policy(HostPlatform::Linux, true, swap).left_option_altgr);
        assert_eq!(
            resolve_keyboard_policy(HostPlatform::MacOs, true, swap).shortcuts,
            ShortcutMode::CommandIsMeta
        );
    }

    #[test]
    fn a_windows_host_without_extended_keys_keeps_option_as_alt() {
        let policy = resolve_keyboard_policy(
            HostPlatform::Windows,
            false,
            settings(ShortcutPreference::Automatic, true),
        );
        assert_eq!(policy.shortcuts, ShortcutMode::WindowsApp);
        assert!(!policy.right_option_altgr && !policy.left_option_altgr);
        assert!(!policy.intl_backslash);
    }

    #[test]
    fn preference_tokens_round_trip() {
        for preference in ShortcutPreference::ALL {
            assert_eq!(
                ShortcutPreference::from_token(preference.token()),
                Some(preference)
            );
        }
        assert_eq!(ShortcutPreference::from_token("bogus"), None);
    }
}
