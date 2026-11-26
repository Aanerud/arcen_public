// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Whether a present reached the glass, from a compositor's frame
//! statistics, and how long a present may wait for that word.
//!
//! A presenter keeps each present in flight until the statistics say it was
//! shown, then counts it confirmed (with a time) or unconfirmed (shown, time
//! unknown). Statistics can stop reaching a present: DXGI reports zeroes
//! where it keeps none, and a reading can stay behind the present for as
//! long as the swapchain runs. A present that waited for them without end
//! held every later one behind it: a Windows Deck in the lab presented 60
//! frames a second while it counted none of them, and its health read every
//! frame as lost. A present resolves within [`PRESENT_CONFIRMATION_DEADLINE`].

use std::time::Duration;

/// How long a present whose GPU work finished may wait for the statistics
/// to say it was shown before it counts as unconfirmed: many refreshes at
/// any display rate, far beyond a compositor's queue.
pub const PRESENT_CONFIRMATION_DEADLINE: Duration = Duration::from_secs(1);

/// One reading of a swapchain's frame statistics
/// (`DXGI_FRAME_STATISTICS`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresentStatistics {
    /// The number of the last present shown (`PresentCount`, numbered as
    /// `GetLastPresentCount` numbers presents).
    pub present_count: u32,
    /// The vertical blank it was shown at (`PresentRefreshCount`).
    pub present_refresh_count: u32,
    /// The vertical blank of this reading (`SyncRefreshCount`).
    pub sync_refresh_count: u32,
}

impl PresentStatistics {
    /// A reading that says nothing: DXGI fills every field with zero where
    /// it keeps no statistics for the swapchain.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.present_count == 0 && self.present_refresh_count == 0 && self.sync_refresh_count == 0
    }
}

/// What the statistics say about one present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresentConfirmation {
    /// Shown, by the statistics.
    Displayed,
    /// Not yet: ask again later.
    Pending,
    /// Shown as far as the presenter can tell (the present succeeded and
    /// its GPU work finished), but no statistics say when.
    Unconfirmed,
}

/// What `statistics` (`None` when the swapchain returned none, as DXGI
/// does while they are disjoint) say about present number `present_id`,
/// whose GPU work finished and which has waited `waited` since it was
/// presented.
#[must_use]
pub fn confirm_present(
    statistics: Option<PresentStatistics>,
    present_id: u32,
    waited: Duration,
) -> PresentConfirmation {
    let Some(statistics) = statistics.filter(|reading| !reading.is_empty()) else {
        return PresentConfirmation::Unconfirmed;
    };
    if statistics.present_count.wrapping_sub(present_id) < u32::MAX / 2 {
        PresentConfirmation::Displayed
    } else if waited > PRESENT_CONFIRMATION_DEADLINE {
        PresentConfirmation::Unconfirmed
    } else {
        PresentConfirmation::Pending
    }
}

/// Whether a presenter's frame statistics are live, for
/// [`crate::client_telemetry::ClientTelemetry::record_presenter_frames`]'s
/// `timestamp_ever_reported`.
///
/// For a presenter whose unconfirmed frames were shown with their time
/// unknown ([`PresentConfirmation::Unconfirmed`]), an unconfirmed frame is
/// no evidence that a later picture replaced it. Statistics come and go
/// with the presentation path (a window composed, then flipped
/// independently, a mode change), so the presenter's word on timing holds
/// only while it keeps confirming: a pass that resolves frames and confirms
/// none of them means the statistics stopped, and its frames count as
/// presented, as before the first confirmation. A pass that confirms some
/// and not others keeps the statistics live, and the others count as
/// superseded.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PresentStatisticsLive(bool);

impl PresentStatisticsLive {
    /// One pass's `confirmed` and `unconfirmed` frames; returns whether the
    /// statistics are live for them.
    pub fn observe(&mut self, confirmed: u64, unconfirmed: u64) -> bool {
        if confirmed > 0 {
            self.0 = true;
        } else if unconfirmed > 0 {
            self.0 = false;
        }
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const fn reading(present_count: u32) -> PresentStatistics {
        PresentStatistics {
            present_count,
            present_refresh_count: 1_000,
            sync_refresh_count: 1_002,
        }
    }

    #[test]
    fn a_present_the_statistics_reached_is_displayed() {
        let soon = Duration::from_millis(20);
        assert_eq!(
            confirm_present(Some(reading(41)), 41, soon),
            PresentConfirmation::Displayed
        );
        // A later present shown: this one was shown or replaced before it.
        assert_eq!(
            confirm_present(Some(reading(44)), 41, soon),
            PresentConfirmation::Displayed
        );
        // Present numbers wrap.
        assert_eq!(
            confirm_present(Some(reading(2)), u32::MAX - 1, soon),
            PresentConfirmation::Displayed
        );
        assert_eq!(
            confirm_present(Some(reading(40)), 41, soon),
            PresentConfirmation::Pending
        );
    }

    /// The lab's stall: statistics that never reach a present (an empty
    /// reading, or one that stays behind) no longer hold it, and every
    /// present behind it, in flight for good.
    #[test]
    fn a_present_the_statistics_never_reach_resolves_unconfirmed() {
        let soon = Duration::from_millis(20);
        let late = PRESENT_CONFIRMATION_DEADLINE + Duration::from_millis(1);
        let empty = Some(PresentStatistics::default());
        assert_eq!(
            confirm_present(empty, 41, soon),
            PresentConfirmation::Unconfirmed
        );
        assert_eq!(
            confirm_present(None, 41, soon),
            PresentConfirmation::Unconfirmed
        );
        assert_eq!(
            confirm_present(Some(reading(40)), 41, PRESENT_CONFIRMATION_DEADLINE),
            PresentConfirmation::Pending
        );
        assert_eq!(
            confirm_present(Some(reading(40)), 41, late),
            PresentConfirmation::Unconfirmed
        );
    }

    #[test]
    fn statistics_are_live_only_while_they_confirm() {
        let mut live = PresentStatisticsLive::default();
        assert!(!live.observe(0, 3), "before any confirmation");
        assert!(live.observe(12, 0));
        assert!(live.observe(0, 0), "a pass that resolved nothing");
        assert!(live.observe(10, 2), "some confirmed: the rest superseded");
        assert!(!live.observe(0, 14), "the statistics stopped");
        assert!(live.observe(1, 0), "and came back");
    }
}
