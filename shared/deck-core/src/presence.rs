// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host presence: whether a saved machine is ready, already signed in,
//! in use by someone else or off, as Home shows it under its name.
//!
//! Scaffolding only. No Pier reports presence today, so the Deck's source
//! is [`NoPresence`] and every machine is [`HostPresence::Unknown`]: Home
//! then shows the address and no status. A future presence service can supply a
//! [`PresenceSource`]; the record is `docs/architecture/host-presence.md`.
//! A developer build (`dev-tools`) also has `DemoPresence`, which invents
//! statuses for screenshots when `ARCEN_DECK_PRESENCE_DEMO` is `1`; a
//! release build always uses [`NoPresence`].

use std::time::SystemTime;

/// What is known about a machine's sessions.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum HostPresence {
    /// Nothing is known: Home shows the address and no status.
    #[default]
    Unknown,
    /// Nobody is signed in: Connect signs in.
    Ready,
    /// A session is signed in. `who` only when the source's policy lets the
    /// Deck show it.
    SignedIn {
        by_you: bool,
        who: Option<String>,
        since: Option<SystemTime>,
    },
    /// Somebody else is using the machine.
    InUse { who: Option<String> },
    /// The machine does not answer.
    Off,
}

impl HostPresence {
    /// The status as the artist reads it; `None` while nothing is known.
    #[must_use]
    pub fn phrase(&self) -> Option<String> {
        match self {
            Self::Unknown => None,
            Self::Ready => Some("Ready".to_owned()),
            Self::SignedIn { by_you: true, .. } => {
                Some("You\u{2019}re signed in \u{b7} resume".to_owned())
            }
            Self::SignedIn {
                by_you: false,
                who: Some(who),
                ..
            }
            | Self::InUse { who: Some(who) } => Some(format!("In use by {who}")),
            Self::SignedIn { by_you: false, .. } | Self::InUse { who: None } => {
                Some("In use by someone else".to_owned())
            }
            Self::Off => Some("Off".to_owned()),
        }
    }

    /// Whether the primary resumes the user's own session.
    #[must_use]
    pub const fn resumes(&self) -> bool {
        matches!(self, Self::SignedIn { by_you: true, .. })
    }

    /// The primary's label.
    #[must_use]
    pub const fn verb(&self) -> &'static str {
        if self.resumes() { "Resume" } else { "Connect" }
    }
}

/// Where Home learns a machine's presence. A source answers at once from
/// what it already knows; it never blocks the UI thread.
pub trait PresenceSource: Send + Sync {
    /// The presence of the saved connection `id` named `name`.
    fn presence(&self, id: &str, name: &str) -> HostPresence;
}

/// The Deck's source today: nothing is known.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPresence;

impl PresenceSource for NoPresence {
    fn presence(&self, _id: &str, _name: &str) -> HostPresence {
        HostPresence::Unknown
    }
}

/// The environment variable that turns on [`DemoPresence`].
#[cfg(feature = "dev-tools")]
pub const DEMO_ENV: &str = "ARCEN_DECK_PRESENCE_DEMO";

/// Invented statuses, the same for the same name, for screenshots of what
/// Home looks like once a source exists. Developer builds only, never on by
/// default.
#[cfg(feature = "dev-tools")]
#[derive(Debug, Clone, Copy, Default)]
pub struct DemoPresence;

#[cfg(feature = "dev-tools")]
impl PresenceSource for DemoPresence {
    fn presence(&self, _id: &str, name: &str) -> HostPresence {
        // FNV-1a: stable across runs and platforms.
        let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
        match hash % 4 {
            0 => HostPresence::Ready,
            1 => HostPresence::SignedIn {
                by_you: true,
                who: None,
                since: None,
            },
            2 => HostPresence::InUse { who: None },
            _ => HostPresence::Off,
        }
    }
}

/// Whether [`DEMO_ENV`] asks for [`DemoPresence`].
#[cfg(feature = "dev-tools")]
#[must_use]
pub fn demo_requested(value: Option<&str>) -> bool {
    value.is_some_and(|value| value.trim() == "1")
}

/// The source the Deck uses: [`NoPresence`]; in a developer build,
/// `DemoPresence` when `ARCEN_DECK_PRESENCE_DEMO` is `1` (read once).
#[must_use]
pub fn current() -> &'static dyn PresenceSource {
    #[cfg(feature = "dev-tools")]
    {
        static DEMO: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        if *DEMO.get_or_init(|| demo_requested(std::env::var(DEMO_ENV).ok().as_deref())) {
            return &DemoPresence;
        }
    }
    &NoPresence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_is_known_by_default() {
        assert_eq!(NoPresence.presence("a", "edit-bay"), HostPresence::Unknown);
        assert_eq!(HostPresence::Unknown.phrase(), None);
        #[cfg(not(feature = "dev-tools"))]
        assert_eq!(current().presence("a", "edit-bay"), HostPresence::Unknown);
    }

    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_demo_needs_its_variable() {
        assert!(!demo_requested(None));
        assert!(!demo_requested(Some("0")));
        assert!(demo_requested(Some("1")));
    }

    #[test]
    fn who_shows_only_when_the_source_gives_it() {
        assert_eq!(
            HostPresence::InUse { who: None }.phrase().as_deref(),
            Some("In use by someone else")
        );
        assert_eq!(
            HostPresence::InUse {
                who: Some("alice".to_owned())
            }
            .phrase()
            .as_deref(),
            Some("In use by alice")
        );
        let mine = HostPresence::SignedIn {
            by_you: true,
            who: None,
            since: None,
        };
        assert_eq!(mine.verb(), "Resume");
        assert_eq!(HostPresence::Ready.verb(), "Connect");
    }
}
