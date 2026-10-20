//! Whether the screen of a remote session may lock.
//!
//! GNOME Shell unlocks through GDM's reauthentication channel; without a
//! running GDM a locked shell cannot be unlocked from inside the session.
//! The shell already hides its own Lock action then (`LoginManager.canLock`
//! asks GDM for its version), but the idle lock and the keyboard shortcut
//! do not ask. An Arcen session therefore applies the same test to every
//! lock path: a lock without an unlock path is a lockout, so it is refused.

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

#[cfg(test)]
mod tests {
    use super::*;

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
