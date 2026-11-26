// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The host's operating system shutting down or restarting under a live
//! session: what a Pier records, and how long it may take.
//!
//! The operating system signs its users out before it stops services, so
//! the session a Deck was in is ended by the operating system, never by
//! Arcen (ADR 0025: a Pier locks a session and never ends one). What a
//! Pier owes is the record, written before the operating system terminates
//! its processes:
//!
//! - the session's end (`SESSION_END`), written when the operating system
//!   says it is ending the session ([`SessionEndedBy`]), before anything
//!   else is waited for, and exactly once ([`SessionTally`]); and
//! - the service's stop (`SERVICE_STOP`, reason
//!   [`StopCause::service_stop_reason`]), written after a departure bounded
//!   by [`DEPARTURE_BUDGET`], inside the time the Pier asks the operating
//!   system for ([`SHUTDOWN_GRANT`]).
//!
//! A departure on shutdown waits for nothing that would outlast that: a
//! display journal left behind is replayed when the Pier next starts, and
//! the session is already ending.

use std::time::{Duration, Instant};

const SHUTDOWN_GRANT_SECS: u64 = 10;
const STOP_RECORD_RESERVE_SECS: u64 = 2;

/// How long a Pier asks its operating system to wait for it after a
/// shutdown notice (Windows: the service's preshutdown timeout, which the
/// installer sets).
pub const SHUTDOWN_GRANT: Duration = Duration::from_secs(SHUTDOWN_GRANT_SECS);
/// Kept back from [`SHUTDOWN_GRANT`] to write the stop record and flush the
/// log.
pub const STOP_RECORD_RESERVE: Duration = Duration::from_secs(STOP_RECORD_RESERVE_SECS);
/// The longest the sessions' departures may take once the host is shutting
/// down.
pub const DEPARTURE_BUDGET: Duration =
    Duration::from_secs(SHUTDOWN_GRANT_SECS - STOP_RECORD_RESERVE_SECS);

/// Why the Pier's service is stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopCause {
    /// An administrator, an installer or the service manager asked it to.
    Requested,
    /// The operating system is shutting down or restarting.
    HostShutdown,
}

impl StopCause {
    /// `SERVICE_STOP`'s `reason_class`.
    #[must_use]
    pub const fn service_stop_reason(self) -> &'static str {
        match self {
            Self::Requested => "clean_shutdown",
            Self::HostShutdown => "host_shutdown",
        }
    }

    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::HostShutdown => "host_shutdown",
        }
    }

    /// How long the sessions may take to depart, given the Pier's own limit
    /// for a requested stop.
    #[must_use]
    pub fn drain_limit(self, requested_limit: Duration) -> Duration {
        match self {
            Self::Requested => requested_limit,
            Self::HostShutdown => requested_limit.min(DEPARTURE_BUDGET),
        }
    }

    /// Whether a departing session waits for its displays to be restored
    /// before it is locked. Not on shutdown: the journal is replayed when
    /// the Pier next starts, and the session is ending anyway.
    #[must_use]
    pub const fn awaits_display_restore(self) -> bool {
        matches!(self, Self::Requested)
    }
}

/// When the sessions' departures must be over, from the moment the Pier was
/// asked to stop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DrainDeadline(Instant);

impl DrainDeadline {
    #[must_use]
    pub fn new(stopping_since: Instant, cause: StopCause, requested_limit: Duration) -> Self {
        Self(stopping_since + cause.drain_limit(requested_limit))
    }

    #[must_use]
    pub const fn at(self) -> Instant {
        self.0
    }

    /// The part of a wait of at most `wait`, starting `now`, that fits.
    #[must_use]
    pub fn bound(self, now: Instant, wait: Duration) -> Duration {
        wait.min(self.0.saturating_duration_since(now))
    }
}

/// How the operating system ended a session a Deck was attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionEndedBy {
    /// Its user signed out.
    SignOut,
    /// The host is shutting down or restarting.
    HostShutdown,
}

impl SessionEndedBy {
    /// `SESSION_END`'s `reason_class`.
    #[must_use]
    pub const fn reason_class(self) -> &'static str {
        match self {
            Self::SignOut => "signed_out",
            Self::HostShutdown => "host_shutdown",
        }
    }
}

/// A session's frames, as its `SESSION_END` reports them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionTotals {
    pub frames_sent: u64,
    pub frames_dropped: u64,
}

impl SessionTotals {
    fn plus(self, other: Self) -> Self {
        Self {
            frames_sent: self.frames_sent.saturating_add(other.frames_sent),
            frames_dropped: self.frames_dropped.saturating_add(other.frames_dropped),
        }
    }
}

/// A session's frame totals across its attachments, and the one end record
/// it gets: whichever of the ordinary end and the operating system's end
/// comes first writes it, the other writes nothing.
#[derive(Debug, Default)]
pub struct SessionTally {
    closed: SessionTotals,
    current: SessionTotals,
    ended: bool,
}

impl SessionTally {
    /// What the current attachment has sent and dropped so far.
    pub fn observe_attachment(&mut self, frames_sent: u64, frames_dropped: u64) {
        self.current = SessionTotals {
            frames_sent,
            frames_dropped,
        };
    }

    /// The current attachment ended having sent and dropped these.
    pub fn close_attachment(&mut self, frames_sent: u64, frames_dropped: u64) {
        self.closed = self.closed.plus(SessionTotals {
            frames_sent,
            frames_dropped,
        });
        self.current = SessionTotals::default();
    }

    #[must_use]
    pub fn totals(&self) -> SessionTotals {
        self.closed.plus(self.current)
    }

    /// The totals to record, once: `None` when the end is already recorded.
    pub fn end(&mut self) -> Option<SessionTotals> {
        if self.ended {
            return None;
        }
        self.ended = true;
        Some(self.totals())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_departure_and_the_stop_record_fit_the_grant() {
        assert_eq!(DEPARTURE_BUDGET + STOP_RECORD_RESERVE, SHUTDOWN_GRANT);
        assert!(STOP_RECORD_RESERVE >= Duration::from_secs(1));
    }

    #[test]
    fn a_shutdown_drains_within_the_budget_and_a_requested_stop_keeps_its_limit() {
        let thirty = Duration::from_secs(30);
        assert_eq!(StopCause::Requested.drain_limit(thirty), thirty);
        assert_eq!(
            StopCause::HostShutdown.drain_limit(thirty),
            DEPARTURE_BUDGET
        );
        let short = Duration::from_secs(3);
        assert_eq!(StopCause::HostShutdown.drain_limit(short), short);
    }

    #[test]
    fn only_a_requested_stop_waits_for_the_displays() {
        assert!(StopCause::Requested.awaits_display_restore());
        assert!(!StopCause::HostShutdown.awaits_display_restore());
    }

    #[test]
    fn the_stop_records_say_whether_the_host_shut_down() {
        assert_eq!(StopCause::Requested.service_stop_reason(), "clean_shutdown");
        assert_eq!(
            StopCause::HostShutdown.service_stop_reason(),
            "host_shutdown"
        );
        assert_eq!(SessionEndedBy::SignOut.reason_class(), "signed_out");
        assert_eq!(SessionEndedBy::HostShutdown.reason_class(), "host_shutdown");
    }

    #[test]
    fn a_wait_is_cut_to_what_is_left_of_the_drain() {
        let start = Instant::now();
        let deadline = DrainDeadline::new(start, StopCause::HostShutdown, Duration::from_secs(30));
        assert_eq!(deadline.at(), start + DEPARTURE_BUDGET);
        let fifteen = Duration::from_secs(15);
        assert_eq!(deadline.bound(start, fifteen), DEPARTURE_BUDGET);
        assert_eq!(
            deadline.bound(start + Duration::from_secs(5), Duration::from_secs(1)),
            Duration::from_secs(1)
        );
        assert_eq!(
            deadline.bound(start + Duration::from_secs(60), fifteen),
            Duration::ZERO
        );
    }

    #[test]
    fn the_tally_counts_every_attachment_and_ends_once() {
        let mut tally = SessionTally::default();
        tally.observe_attachment(100, 2);
        tally.close_attachment(120, 3);
        tally.observe_attachment(40, 1);
        assert_eq!(
            tally.totals(),
            SessionTotals {
                frames_sent: 160,
                frames_dropped: 4
            }
        );
        assert_eq!(
            tally.end(),
            Some(SessionTotals {
                frames_sent: 160,
                frames_dropped: 4
            })
        );
        assert_eq!(tally.end(), None);
    }
}
