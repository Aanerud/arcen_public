// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Who has the keyboard during a session: the host or this computer.
//!
//! One decision for every Deck. While the *host* has the keyboard, every key
//! goes to the host, and so do the system shortcuts (Alt+Tab, Alt+Shift+Tab,
//! the Windows key, Print Screen) whenever the policy says so: by default
//! whenever a session window has focus, windowed or fullscreen. The escape
//! chord (Ctrl+Alt+Home on a PC, Ctrl+Option+Home on a Mac,
//! [`DeckShortcut::ReleaseKeyboard`]) gives the keyboard to *this
//! computer*: every key held on the host is released first, then nothing
//! more reaches the host and the system shortcuts act locally. The same
//! chord, or a click into the picture, gives it back; that click is
//! consumed, so it never paints a stroke or presses a button on the host.
//!
//! A Deck only reports facts (a session, focus, fullscreen, the chord, a
//! click) and acts on the answers: whether keys reach the host, whether the
//! system shortcuts are taken, whether to release held keys, whether to
//! consume a click, and what its indicator says ([`KeyboardStatus`]).
//!
//! [`DeckShortcut::ReleaseKeyboard`]: super::DeckShortcut::ReleaseKeyboard

/// When the session takes the system shortcuts (the Windows key, Alt+Tab,
/// Alt+Shift+Tab, Alt+Esc, Ctrl+Esc, Ctrl+Shift+Esc, Print Screen) from the
/// Deck's computer while the host has the keyboard.
///
/// By default whenever a session window has focus, windowed or fullscreen
/// ([`Self::Host`]): a remote desktop is used as the computer it shows, so
/// its shortcuts act on it. Windows+L and Ctrl+Alt+Del stay the Deck
/// computer's by design of Windows.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KeyboardCapturePolicy {
    /// Whenever a session window has focus (the default).
    #[default]
    Host,
    /// Only while the session is fullscreen.
    Fullscreen,
    /// Never: this computer keeps its system shortcuts; other keys still
    /// reach the host.
    ThisComputer,
}

impl KeyboardCapturePolicy {
    /// Every setting, in menu order.
    pub const ALL: [Self; 3] = [Self::Host, Self::Fullscreen, Self::ThisComputer];

    /// The settings token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Fullscreen => "fullscreen",
            Self::ThisComputer => "this_computer",
        }
    }

    /// The setting a token names, the keyboard grab's earlier tokens
    /// (`always`, `fullscreen`, `never`) included.
    #[must_use]
    pub fn from_token(token: &str) -> Option<Self> {
        match token {
            "always" => Some(Self::Host),
            "never" => Some(Self::ThisComputer),
            _ => Self::ALL.into_iter().find(|policy| policy.token() == token),
        }
    }

    /// Whether the policy takes the system shortcuts while the host has the
    /// keyboard.
    #[must_use]
    pub const fn takes_system_shortcuts(self, fullscreen: bool) -> bool {
        match self {
            Self::Host => true,
            Self::Fullscreen => fullscreen,
            Self::ThisComputer => false,
        }
    }
}

/// Who has the keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardOwner {
    /// The host: keys reach it while a session window has focus.
    Host,
    /// This computer: the escape chord or the menu gave it back; nothing
    /// reaches the host until the chord again or a click into the picture.
    ThisComputer,
}

/// What a Deck's indicator says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyboardStatus {
    /// No session.
    NoSession,
    /// The host has the keyboard; `system_shortcuts`: Alt+Tab and the
    /// Windows key go there too (the policy takes them now).
    Host {
        /// The system shortcuts go to the host as well.
        system_shortcuts: bool,
    },
    /// This computer has the keyboard.
    ThisComputer,
}

/// What a change of owner asks of the Deck.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureChange {
    /// The owner now.
    pub owner: KeyboardOwner,
    /// Release every key held on the host before anything else (the
    /// keyboard left the host).
    pub release_host_keys: bool,
}

/// Where a click into the picture goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClickRoute {
    /// To the host, as always.
    Forward,
    /// Nowhere: it gave the keyboard back to the host. Its release is
    /// consumed too.
    Consume,
}

/// The keyboard's owner during a session, from the facts a Deck reports.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent window facts: session, focus, fullscreen, released"
)]
pub struct KeyboardCapture {
    policy: KeyboardCapturePolicy,
    session: bool,
    focused: bool,
    fullscreen: bool,
    released: bool,
}

impl KeyboardCapture {
    /// No session yet, under `policy`.
    #[must_use]
    pub const fn new(policy: KeyboardCapturePolicy) -> Self {
        Self {
            policy,
            session: false,
            focused: false,
            fullscreen: false,
            released: false,
        }
    }

    /// The policy in force.
    #[must_use]
    pub const fn policy(&self) -> KeyboardCapturePolicy {
        self.policy
    }

    /// Applies a policy (Settings changed).
    pub const fn set_policy(&mut self, policy: KeyboardCapturePolicy) {
        self.policy = policy;
    }

    /// A session is on screen, or not. A new session starts with the host
    /// owning the keyboard.
    pub const fn set_session(&mut self, session: bool) {
        self.session = session;
        self.released = false;
    }

    /// The session window has the keyboard focus, or not. Focus alone never
    /// changes the owner: a window that loses focus takes nothing to the
    /// host, and coming back keeps the owner it had.
    pub const fn set_focused(&mut self, focused: bool) {
        self.focused = focused;
    }

    /// The session window is fullscreen, or not.
    pub const fn set_fullscreen(&mut self, fullscreen: bool) {
        self.fullscreen = fullscreen;
    }

    /// Who has the keyboard (the host, outside a session).
    #[must_use]
    pub const fn owner(&self) -> KeyboardOwner {
        if self.session && self.released {
            KeyboardOwner::ThisComputer
        } else {
            KeyboardOwner::Host
        }
    }

    /// Whether keys reach the host now.
    #[must_use]
    pub const fn keys_to_host(&self) -> bool {
        self.session && self.focused && !self.released
    }

    /// Whether the session takes the system shortcuts now.
    #[must_use]
    pub const fn takes_system_shortcuts(&self) -> bool {
        self.keys_to_host() && self.policy.takes_system_shortcuts(self.fullscreen)
    }

    /// The escape chord or the menu item: the keyboard goes to this
    /// computer, or back to the host.
    pub const fn toggle(&mut self) -> CaptureChange {
        match self.owner() {
            KeyboardOwner::Host => self.release(),
            KeyboardOwner::ThisComputer => self.recapture(),
        }
    }

    /// The keyboard goes to this computer (keys held on the host are
    /// released first). Nothing changes outside a session.
    pub const fn release(&mut self) -> CaptureChange {
        let release_host_keys = self.session && !self.released;
        if self.session {
            self.released = true;
        }
        CaptureChange {
            owner: self.owner(),
            release_host_keys,
        }
    }

    /// The keyboard goes back to the host.
    pub const fn recapture(&mut self) -> CaptureChange {
        self.released = false;
        CaptureChange {
            owner: self.owner(),
            release_host_keys: false,
        }
    }

    /// A button press over the picture: while this computer has the
    /// keyboard it gives it back to the host and is consumed.
    pub const fn click_in_picture(&mut self) -> ClickRoute {
        if matches!(self.owner(), KeyboardOwner::ThisComputer) {
            self.released = false;
            ClickRoute::Consume
        } else {
            ClickRoute::Forward
        }
    }

    /// What the indicator says.
    #[must_use]
    pub const fn status(&self) -> KeyboardStatus {
        if !self.session {
            return KeyboardStatus::NoSession;
        }
        match self.owner() {
            KeyboardOwner::ThisComputer => KeyboardStatus::ThisComputer,
            KeyboardOwner::Host => KeyboardStatus::Host {
                system_shortcuts: self.policy.takes_system_shortcuts(self.fullscreen),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn focused_session(policy: KeyboardCapturePolicy) -> KeyboardCapture {
        let mut capture = KeyboardCapture::new(policy);
        capture.set_session(true);
        capture.set_focused(true);
        capture
    }

    #[test]
    fn the_host_has_the_keyboard_and_its_shortcuts_by_default_windowed_and_fullscreen() {
        assert_eq!(
            KeyboardCapturePolicy::default(),
            KeyboardCapturePolicy::Host
        );
        let mut capture = focused_session(KeyboardCapturePolicy::default());
        for fullscreen in [false, true] {
            capture.set_fullscreen(fullscreen);
            assert!(capture.keys_to_host());
            assert!(capture.takes_system_shortcuts());
            assert_eq!(
                capture.status(),
                KeyboardStatus::Host {
                    system_shortcuts: true
                }
            );
        }
    }

    #[test]
    fn the_escape_chord_toggles_and_releases_held_keys_only_on_the_way_out() {
        let mut capture = focused_session(KeyboardCapturePolicy::Host);
        assert_eq!(
            capture.toggle(),
            CaptureChange {
                owner: KeyboardOwner::ThisComputer,
                release_host_keys: true,
            }
        );
        assert!(!capture.keys_to_host());
        assert!(!capture.takes_system_shortcuts());
        assert_eq!(capture.status(), KeyboardStatus::ThisComputer);
        // Releasing again releases nothing more.
        assert!(!capture.release().release_host_keys);
        assert_eq!(
            capture.toggle(),
            CaptureChange {
                owner: KeyboardOwner::Host,
                release_host_keys: false,
            }
        );
        assert!(capture.keys_to_host());
    }

    #[test]
    fn a_click_into_the_picture_takes_the_keyboard_back_and_is_consumed_once() {
        let mut capture = focused_session(KeyboardCapturePolicy::Host);
        assert_eq!(capture.click_in_picture(), ClickRoute::Forward);
        let _ = capture.release();
        assert_eq!(capture.click_in_picture(), ClickRoute::Consume);
        assert_eq!(capture.owner(), KeyboardOwner::Host);
        assert_eq!(capture.click_in_picture(), ClickRoute::Forward);
    }

    #[test]
    fn focus_never_changes_the_owner() {
        let mut capture = focused_session(KeyboardCapturePolicy::Host);
        capture.set_focused(false);
        assert!(!capture.keys_to_host());
        assert_eq!(capture.owner(), KeyboardOwner::Host);
        let _ = capture.release();
        capture.set_focused(true);
        assert_eq!(capture.owner(), KeyboardOwner::ThisComputer);
        // Unfocused and released: a click (focusing the window) gives it back.
        capture.set_focused(false);
        assert_eq!(capture.click_in_picture(), ClickRoute::Consume);
    }

    #[test]
    fn a_new_session_starts_with_the_host() {
        let mut capture = focused_session(KeyboardCapturePolicy::Host);
        let _ = capture.release();
        capture.set_session(false);
        assert_eq!(capture.status(), KeyboardStatus::NoSession);
        assert_eq!(capture.owner(), KeyboardOwner::Host);
        assert!(!capture.release().release_host_keys);
        capture.set_session(true);
        assert_eq!(capture.owner(), KeyboardOwner::Host);
        assert!(capture.keys_to_host());
    }

    #[test]
    fn the_policy_decides_the_system_shortcuts_not_the_keys() {
        let mut capture = focused_session(KeyboardCapturePolicy::Fullscreen);
        assert!(capture.keys_to_host());
        assert!(!capture.takes_system_shortcuts());
        capture.set_fullscreen(true);
        assert!(capture.takes_system_shortcuts());
        capture.set_policy(KeyboardCapturePolicy::ThisComputer);
        assert!(capture.keys_to_host());
        assert!(!capture.takes_system_shortcuts());
        assert_eq!(
            capture.status(),
            KeyboardStatus::Host {
                system_shortcuts: false
            }
        );
    }

    #[test]
    fn the_keyboard_grab_tokens_still_read() {
        for (token, policy) in [
            ("always", KeyboardCapturePolicy::Host),
            ("fullscreen", KeyboardCapturePolicy::Fullscreen),
            ("never", KeyboardCapturePolicy::ThisComputer),
            ("host", KeyboardCapturePolicy::Host),
            ("this_computer", KeyboardCapturePolicy::ThisComputer),
        ] {
            assert_eq!(KeyboardCapturePolicy::from_token(token), Some(policy));
        }
        assert_eq!(KeyboardCapturePolicy::from_token("sometimes"), None);
        for policy in KeyboardCapturePolicy::ALL {
            assert_eq!(
                KeyboardCapturePolicy::from_token(policy.token()),
                Some(policy)
            );
        }
    }
}
