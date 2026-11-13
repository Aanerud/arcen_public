// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Keyboard policy between a Mac client's keys and a host's keys.
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
//!   injects physical keys (Windows).

pub mod scancode;
mod shortcuts;

use arcen_protocol::messages::HostPlatform;

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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        for platform in [HostPlatform::Linux, HostPlatform::Unknown] {
            assert_eq!(
                resolve_keyboard_policy(platform, false, automatic),
                KeyboardPolicy::LEGACY
            );
        }
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
