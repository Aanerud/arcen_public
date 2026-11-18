// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The desk kept private while a Deck is attached to the user's own
//! session, on a host whose window server filters input event by event and
//! sets each display's transfer table (the macOS Pier; ADR 0025 §5, GAP-014
//! closed). The Linux Pier grabs evdev devices and zeroes `RandR` ramps
//! ([`super::disables_while_attached`]); the rules here are the same rules
//! for a host that can only see events and displays.
//!
//! - **Input:** an event filter sees who posted each event, not which
//!   device made it. Only the Pier's own process passes. Hardware events,
//!   the events of a virtual HID device (the window server cannot tell one
//!   from a desk keyboard), and other processes' synthetic events (an
//!   on-screen keyboard at the desk is the desk's input) are dropped. So a
//!   session whose Deck input travels through virtual HID devices cannot be
//!   kept private ([`DeckInputRoute`]).
//! - **Displays:** every display but the Pier's own virtual displays is
//!   given an all-zero transfer table, proved by reading it back
//!   ([`transfer_table_is_dark`]); capture reads the composited desktop
//!   before the table, so the Deck's picture is unchanged.
//! - **Hand-back:** the shared [`super::DETACH_STEPS`]: lock, the lock
//!   screen proved, outputs, sound, input. A session that would not lock
//!   stays private, retried, for at most [`super::UNLOCKED_GRACE`]
//!   ([`hand_back_unlocked`]).

use std::time::{Duration, Instant};

/// Who posted an input event, as an event filter can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventPoster {
    /// The Pier's own process (its posted events).
    ThisPier,
    /// The window server's own input: a hardware device or a virtual HID
    /// device, which it cannot tell apart.
    HidSystem,
    /// Another process's synthetic events.
    OtherProcess,
}

impl EventPoster {
    /// Classifies an event by the process id the window server recorded for
    /// its poster (`0` for its own HID input).
    #[must_use]
    pub const fn from_pid(poster_pid: i64, pier_pid: i64) -> Self {
        if poster_pid == 0 {
            Self::HidSystem
        } else if poster_pid == pier_pid {
            Self::ThisPier
        } else {
            Self::OtherProcess
        }
    }
}

/// Whether an input event reaches the desktop while a Deck is attached and
/// the desk is private: only the Pier's own.
#[must_use]
pub const fn event_passes_while_attached(poster: EventPoster) -> bool {
    matches!(poster, EventPoster::ThisPier)
}

/// What the host knows about one display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeskDisplayFact {
    /// One of the Pier's own virtual displays (the Deck's displays).
    pub pier_virtual: bool,
}

/// Whether a display goes dark while a Deck is attached: every display but
/// the Pier's own virtual ones (a mirrored or wireless display shows the
/// desktop to someone too).
#[must_use]
pub const fn blanks_while_attached(display: DeskDisplayFact) -> bool {
    !display.pier_virtual
}

/// The largest transfer-table output that still counts as dark.
pub const DARK_TABLE_LIMIT: f32 = 1.0 / 1024.0;

/// Whether a transfer table read back from a display is dark: it has
/// entries, and every one is at most [`DARK_TABLE_LIMIT`].
#[must_use]
pub fn transfer_table_is_dark(entries: &[f32]) -> bool {
    !entries.is_empty()
        && entries
            .iter()
            .all(|entry| entry.is_finite() && *entry <= DARK_TABLE_LIMIT)
}

/// How the Deck's input reaches the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeckInputRoute {
    /// Events the Pier posts itself.
    PostedEvents,
    /// Keys (or more) through the Pier's virtual HID devices.
    VirtualHidDevices,
}

/// Why the desk cannot be made private.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeskPrivacyRefusal {
    /// The Deck's input goes through virtual HID devices, which an event
    /// filter cannot tell from the desk's own.
    VirtualHidInput,
    /// The input filter could not be installed (the Pier's process lacks
    /// the Accessibility grant, or the window server refused it).
    InputFilterUnavailable,
    /// A display could not be darkened, or did not read back dark.
    DisplaysNotDark,
}

impl DeskPrivacyRefusal {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::VirtualHidInput => "virtual_hid_input",
            Self::InputFilterUnavailable => "input_filter_unavailable",
            Self::DisplaysNotDark => "displays_not_dark",
        }
    }
}

/// Whether the desk can be kept private for a session whose Deck input
/// takes `route`.
///
/// # Errors
///
/// [`DeskPrivacyRefusal::VirtualHidInput`] for virtual HID input.
pub const fn admits_input_route(route: DeckInputRoute) -> Result<(), DeskPrivacyRefusal> {
    match route {
        DeckInputRoute::PostedEvents => Ok(()),
        DeckInputRoute::VirtualHidDevices => Err(DeskPrivacyRefusal::VirtualHidInput),
    }
}

/// How often a private desk's displays are proved dark again: a display
/// reconfiguration (a display plugged in, a mode change, wake) gives every
/// display its own table back, and the desk sees the session until the next
/// check.
pub const DISPLAY_CHECK: Duration = Duration::from_millis(100);

/// How often the console's holder is read while the desk is private.
pub const CONSOLE_CHECK: Duration = Duration::from_secs(1);

/// Consecutive console readings naming someone else before the desk is
/// handed to them: one failed reading is not a switch of account.
pub const CONSOLE_AWAY_READINGS: u32 = 3;

/// How long a desk display may stay undarkened before the Deck's session is
/// ended: privacy that cannot be kept is not kept by streaming on.
pub const NOT_DARK_LIMIT: Duration = Duration::from_secs(2);

/// What the watcher of a private desk does after an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchVerdict {
    /// The desk is private, or may still become so; keep watching.
    Hold,
    /// The console belongs to another account: give the desk to it and end
    /// the Deck's session.
    ConsoleLeft,
    /// A desk display could not be kept dark for [`NOT_DARK_LIMIT`]: end
    /// the Deck's session (it is locked before the desk gets anything back).
    PrivacyLost,
}

/// The watcher's memory between observations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PrivacyWatch {
    away_readings: u32,
    not_dark_since: Option<Instant>,
}

impl PrivacyWatch {
    /// One reading of the console: whether it still belongs to the session's
    /// account.
    pub fn console(&mut self, ours: bool) -> WatchVerdict {
        if ours {
            self.away_readings = 0;
            return WatchVerdict::Hold;
        }
        self.away_readings = self.away_readings.saturating_add(1);
        if self.away_readings >= CONSOLE_AWAY_READINGS {
            WatchVerdict::ConsoleLeft
        } else {
            WatchVerdict::Hold
        }
    }

    /// One proof of the displays at `now`: whether every desk display read
    /// back dark (a failed display list counts as not dark).
    pub fn displays(&mut self, dark: bool, now: Instant) -> WatchVerdict {
        if dark {
            self.not_dark_since = None;
            return WatchVerdict::Hold;
        }
        let since = *self.not_dark_since.get_or_insert(now);
        if now.saturating_duration_since(since) >= NOT_DARK_LIMIT {
            WatchVerdict::PrivacyLost
        } else {
            WatchVerdict::Hold
        }
    }
}

/// How often a session that would not lock is asked to lock again while the
/// desk stays private.
pub const LOCK_RETRY: Duration = Duration::from_secs(5);

/// What happens to the desk when its session did not lock as the Deck left.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnlockedHandBack {
    /// Keep it private and ask the session to lock again after
    /// [`LOCK_RETRY`].
    KeepPrivate,
    /// Give the desk back unlocked: the session has refused to lock for
    /// [`super::UNLOCKED_GRACE`] (its lock screen is off by policy), and
    /// its owner at the desk must get it back.
    GiveBack,
}

/// [`UnlockedHandBack`] for a session that has refused to lock for
/// `unlocked_for`.
#[must_use]
pub fn hand_back_unlocked(unlocked_for: Duration) -> UnlockedHandBack {
    if unlocked_for >= super::UNLOCKED_GRACE {
        UnlockedHandBack::GiveBack
    } else {
        UnlockedHandBack::KeepPrivate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_pier_s_own_events_reach_a_private_desktop() {
        let pier = 4242;
        assert!(event_passes_while_attached(EventPoster::from_pid(
            pier, pier
        )));
        // The desk's keyboard, a virtual HID keyboard, an on-screen
        // keyboard at the desk: all dropped.
        assert!(!event_passes_while_attached(EventPoster::from_pid(0, pier)));
        assert!(!event_passes_while_attached(EventPoster::from_pid(
            517, pier
        )));
        assert_eq!(EventPoster::from_pid(0, pier), EventPoster::HidSystem);
        assert_eq!(EventPoster::from_pid(517, pier), EventPoster::OtherProcess);
    }

    #[test]
    fn every_display_but_the_pier_s_own_goes_dark() {
        assert!(blanks_while_attached(DeskDisplayFact {
            pier_virtual: false
        }));
        assert!(!blanks_while_attached(DeskDisplayFact {
            pier_virtual: true
        }));
    }

    #[test]
    fn a_table_is_dark_only_when_every_entry_is() {
        assert!(transfer_table_is_dark(&[0.0; 256]));
        assert!(transfer_table_is_dark(&[0.0, DARK_TABLE_LIMIT]));
        assert!(!transfer_table_is_dark(&[]));
        assert!(!transfer_table_is_dark(&[0.0, 0.0, 0.01]));
        assert!(!transfer_table_is_dark(&[0.0, f32::NAN]));
        let identity: Vec<f32> = (0..256_u16).map(|i| f32::from(i) / 255.0).collect();
        assert!(!transfer_table_is_dark(&identity));
    }

    #[test]
    fn virtual_hid_input_cannot_be_kept_private() {
        assert_eq!(admits_input_route(DeckInputRoute::PostedEvents), Ok(()));
        assert_eq!(
            admits_input_route(DeckInputRoute::VirtualHidDevices),
            Err(DeskPrivacyRefusal::VirtualHidInput)
        );
    }

    #[test]
    fn the_console_leaves_only_after_consecutive_readings_name_someone_else() {
        let mut watch = PrivacyWatch::default();
        assert_eq!(watch.console(false), WatchVerdict::Hold);
        assert_eq!(watch.console(false), WatchVerdict::Hold);
        assert_eq!(
            watch.console(true),
            WatchVerdict::Hold,
            "a reading of ours resets"
        );
        for _ in 1..CONSOLE_AWAY_READINGS {
            assert_eq!(watch.console(false), WatchVerdict::Hold);
        }
        assert_eq!(watch.console(false), WatchVerdict::ConsoleLeft);
    }

    #[test]
    fn a_display_that_stays_lit_ends_the_session_after_the_limit() {
        let start = Instant::now();
        let mut watch = PrivacyWatch::default();
        assert_eq!(watch.displays(false, start), WatchVerdict::Hold);
        assert_eq!(
            watch.displays(false, start + NOT_DARK_LIMIT / 2),
            WatchVerdict::Hold
        );
        assert_eq!(
            watch.displays(true, start + NOT_DARK_LIMIT / 2),
            WatchVerdict::Hold,
            "dark again resets"
        );
        assert_eq!(
            watch.displays(false, start + NOT_DARK_LIMIT),
            WatchVerdict::Hold
        );
        assert_eq!(
            watch.displays(false, start + NOT_DARK_LIMIT * 2),
            WatchVerdict::PrivacyLost
        );
    }

    #[test]
    fn a_session_that_will_not_lock_stays_private_for_the_grace_only() {
        assert_eq!(
            hand_back_unlocked(Duration::ZERO),
            UnlockedHandBack::KeepPrivate
        );
        assert_eq!(
            hand_back_unlocked(super::super::UNLOCKED_GRACE.saturating_sub(Duration::from_secs(1))),
            UnlockedHandBack::KeepPrivate
        );
        assert_eq!(
            hand_back_unlocked(super::super::UNLOCKED_GRACE),
            UnlockedHandBack::GiveBack
        );
    }
}
