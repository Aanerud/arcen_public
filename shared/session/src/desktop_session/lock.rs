// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether the screen of a remote session may lock.
//!
//! GNOME Shell unlocks through GDM's reauthentication channel; without a
//! running GDM a locked shell cannot be unlocked from inside the session.
//! The shell already hides its own Lock action then (`LoginManager.canLock`
//! asks GDM for its version), but the idle lock and the keyboard shortcut
//! do not ask. An Arcen session therefore applies the same test to every
//! lock path: a lock without an unlock path is a lockout, so it is refused.
//!
//! A Deck's departure from a desktop the Pier created locks it whenever the
//! desktop stays ([`DetachLock::on_detach`]), as it locks a joined console.

use super::version;

/// The oldest GDM whose version GNOME Shell accepts as a lock provider.
pub const MINIMUM_GDM_VERSION: &str = "3.5.91";

/// What the platform knows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LockFacts {
    /// GDM's `org.gnome.DisplayManager.Manager.Version`, when GDM answers
    /// on the system bus.
    pub gdm_version: Option<String>,
}

/// The lock policy of one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockPolicy {
    /// The user's own settings decide (idle lock, lock delay, shortcut).
    HonourUserSettings,
    /// Locking is refused for the whole session.
    Disabled(LockDisabledReason),
}

/// Why locking is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockDisabledReason {
    /// No GDM to unlock through: a lock could only be lifted by a new
    /// password-authenticated attachment.
    NoInSessionUnlockPath,
}

impl LockPolicy {
    /// The policy for `facts`.
    #[must_use]
    pub fn resolve(facts: &LockFacts) -> Self {
        match &facts.gdm_version {
            Some(gdm) if version::at_least(gdm, MINIMUM_GDM_VERSION) => Self::HonourUserSettings,
            _ => Self::Disabled(LockDisabledReason::NoInSessionUnlockPath),
        }
    }

    /// Whether locking is allowed.
    #[must_use]
    pub const fn allows_lock(self) -> bool {
        matches!(self, Self::HonourUserSettings)
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::HonourUserSettings => "honour_user_settings",
            Self::Disabled(LockDisabledReason::NoInSessionUnlockPath) => {
                "disabled:no_in_session_unlock_path"
            }
        }
    }
}

/// How a Deck's attachment left a desktop the Pier created for the account
/// (ADR 0025 §4: a departure locks the session and never ends it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreatedDetach {
    /// The Deck left and the desktop is kept, disconnected, for the
    /// account's next sign-in: a Deck that did not ask for resume, or an
    /// attachment that ended on a host-side failure.
    KeptForSignIn,
    /// The Deck's transport was lost; the desktop is held for its resume.
    HeldForResume,
    /// A same-account newcomer displaced the attachment and takes the
    /// desktop over. It is still locked: the newcomer may yet be refused
    /// before its authenticated unlock, and nobody would then be attached.
    HandedOver,
    /// The desktop ends with the attachment.
    Ends,
}

/// What a created desktop's departure does about its screen lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachLock {
    /// Lock it through logind and confirm its lock screen. A lock that is
    /// not confirmed is a failure, recorded at error level; the desktop is
    /// never ended for it.
    Lock,
    /// Leave its lock as it is.
    Leave(DetachLockSkip),
}

/// Why a departure leaves a created desktop's lock as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetachLockSkip {
    /// The desktop ends; there is nothing left to lock.
    Ends,
    /// The session cannot lock: its shell refuses every lock (no
    /// in-session unlock path, [`LockPolicy::Disabled`]). Only an
    /// authenticated Arcen sign-in reaches it.
    CannotLock,
}

impl DetachLock {
    /// The lock of a created desktop the Deck left by `detach`, under the
    /// session's lock `policy`. Every departure that leaves the desktop
    /// running locks it: a second lock layer behind the Pier's own
    /// authentication, so no desktop is left open between attachments.
    #[must_use]
    pub const fn on_detach(detach: CreatedDetach, policy: LockPolicy) -> Self {
        match detach {
            CreatedDetach::Ends => Self::Leave(DetachLockSkip::Ends),
            CreatedDetach::KeptForSignIn
            | CreatedDetach::HeldForResume
            | CreatedDetach::HandedOver => {
                if policy.allows_lock() {
                    Self::Lock
                } else {
                    Self::Leave(DetachLockSkip::CannotLock)
                }
            }
        }
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Lock => "lock",
            Self::Leave(DetachLockSkip::Ends) => "leave:ends",
            Self::Leave(DetachLockSkip::CannotLock) => "leave:cannot_lock",
        }
    }
}

impl CreatedDetach {
    /// Whether the desktop keeps running after this departure.
    #[must_use]
    pub const fn keeps_desktop(self) -> bool {
        !matches!(self, Self::Ends)
    }

    /// Stable token for logs.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::KeptForSignIn => "kept_for_sign_in",
            Self::HeldForResume => "held_for_resume",
            Self::HandedOver => "handed_over",
            Self::Ends => "ends",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_departure_that_keeps_a_created_desktop_locks_it() {
        let honour = LockPolicy::HonourUserSettings;
        assert_eq!(
            DetachLock::on_detach(CreatedDetach::KeptForSignIn, honour),
            DetachLock::Lock
        );
        assert_eq!(
            DetachLock::on_detach(CreatedDetach::HeldForResume, honour),
            DetachLock::Lock
        );
        // The newcomer may still be refused before its own unlock.
        assert_eq!(
            DetachLock::on_detach(CreatedDetach::HandedOver, honour),
            DetachLock::Lock
        );
        assert_eq!(
            DetachLock::on_detach(CreatedDetach::Ends, honour),
            DetachLock::Leave(DetachLockSkip::Ends)
        );
    }

    #[test]
    fn a_desktop_without_an_unlock_path_is_not_locked_out() {
        let disabled = LockPolicy::Disabled(LockDisabledReason::NoInSessionUnlockPath);
        for detach in [
            CreatedDetach::KeptForSignIn,
            CreatedDetach::HeldForResume,
            CreatedDetach::HandedOver,
        ] {
            assert_eq!(
                DetachLock::on_detach(detach, disabled),
                DetachLock::Leave(DetachLockSkip::CannotLock)
            );
        }
    }

    #[test]
    fn locking_needs_a_gdm_to_unlock_through() {
        let with = |version: Option<&str>| {
            LockPolicy::resolve(&LockFacts {
                gdm_version: version.map(str::to_owned),
            })
        };
        assert_eq!(with(Some("47.0")), LockPolicy::HonourUserSettings);
        assert_eq!(with(Some("40.1")), LockPolicy::HonourUserSettings);
        assert_eq!(
            with(Some("3.4")),
            LockPolicy::Disabled(LockDisabledReason::NoInSessionUnlockPath)
        );
        assert_eq!(
            with(None),
            LockPolicy::Disabled(LockDisabledReason::NoInSessionUnlockPath)
        );
        assert!(with(Some("47.0")).allows_lock());
        assert!(!with(None).allows_lock());
    }
}
