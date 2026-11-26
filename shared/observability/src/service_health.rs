// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The service-level `HEALTH_SNAPSHOT` (1806) every Pier writes each minute.
//!
//! One rule for every Pier: the snapshot names the worst state any session
//! **committed** in the minute, the state its `HEALTH_OK`/`HEALTH_DEGRADED`/
//! `HEALTH_CRITICAL` transitions report after the two-window hysteresis
//! ([`crate::SessionHealth`]), never a single tick's assessment. One critical
//! two-second window at a stream's start or end is not a critical minute.
//! `overall_state` is `unavailable` only when no session reported in the
//! minute: none was streaming, or the one that was had not settled on a state
//! yet (its first two health windows).

use std::sync::atomic::{AtomicU8, Ordering};

use arcen_telemetry::{FieldValue, HealthState, StructuredFields};

use crate::session_health::state_name;

const NO_SESSION: u8 = 0;

/// The worst committed session state since the last snapshot.
///
/// Lock-free, so every session's health tick can report into one shared
/// value; [`ServiceHealth::take_snapshot_fields`] reads and resets it, so a
/// session that has ended cannot hold the next minute at its severity.
#[derive(Debug, Default)]
pub struct ServiceHealth {
    worst: AtomicU8,
}

impl ServiceHealth {
    /// No session reported yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            worst: AtomicU8::new(NO_SESSION),
        }
    }

    /// Records one session's committed state at a health tick
    /// ([`crate::HealthObservation::committed`]). `None`, a session that has
    /// not settled yet, leaves the minute as it is.
    pub fn record(&self, committed: Option<HealthState>) {
        if let Some(state) = committed {
            self.worst.fetch_max(rank(state), Ordering::Relaxed);
        }
    }

    /// The worst committed state since the last call, `None` when no
    /// session reported one, and starts the next minute.
    #[must_use]
    pub fn take(&self) -> Option<HealthState> {
        match self.worst.swap(NO_SESSION, Ordering::Relaxed) {
            1 => Some(HealthState::Ok),
            2 => Some(HealthState::Degraded),
            3 => Some(HealthState::Critical),
            _ => None,
        }
    }

    /// The service `HEALTH_SNAPSHOT`'s fields for the minute just ended
    /// ([`ServiceHealth::take`]): `overall_state` is `ok`, `degraded`,
    /// `critical` or `unavailable`.
    #[must_use]
    pub fn take_snapshot_fields(&self) -> StructuredFields {
        let mut fields = StructuredFields::default();
        let _ = fields.insert(
            "overall_state",
            FieldValue::String(state_name(self.take()).to_owned()),
        );
        fields
    }
}

const fn rank(state: HealthState) -> u8 {
    match state {
        HealthState::Ok => 1,
        HealthState::Degraded => 2,
        HealthState::Critical => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HostCounters, SessionHealth};
    use arcen_telemetry::QosTargets;

    fn overall_state(service: &ServiceHealth) -> Option<String> {
        match service.take_snapshot_fields().as_map().get("overall_state") {
            Some(FieldValue::String(state)) => Some(state.clone()),
            _ => None,
        }
    }

    #[test]
    fn no_session_is_unavailable() {
        let service = ServiceHealth::new();
        assert_eq!(overall_state(&service).as_deref(), Some("unavailable"));
        service.record(None);
        assert_eq!(overall_state(&service).as_deref(), Some("unavailable"));
    }

    #[test]
    fn the_worst_committed_session_wins_and_the_minute_resets() {
        let service = ServiceHealth::new();
        service.record(Some(HealthState::Ok));
        service.record(Some(HealthState::Critical));
        service.record(Some(HealthState::Degraded));
        assert_eq!(overall_state(&service).as_deref(), Some("critical"));
        assert_eq!(overall_state(&service).as_deref(), Some("unavailable"));
    }

    /// Seen live: a service snapshot read `critical` for a minute in which
    /// no session committed critical, because one two-second tick did.
    #[test]
    fn one_critical_tick_in_a_degraded_session_reads_degraded() {
        let service = ServiceHealth::new();
        let mut health = SessionHealth::new(QosTargets::default());
        // 30 fps target; sent per two-second tick: 50 (degraded), 50, then
        // 20 (one critical window), then 50 again.
        let mut sent = 0;
        let mut saw_critical_tick = false;
        for (tick, frames) in [50_u64, 50, 20, 50, 50].into_iter().enumerate() {
            sent += frames;
            let observation = health.observe(
                2_000 * (u64::try_from(tick).unwrap_or(0) + 1),
                30,
                HostCounters {
                    frames_sent: sent,
                    ..HostCounters::default()
                },
            );
            saw_critical_tick |= observation.assessment.overall == Some(HealthState::Critical);
            service.record(observation.committed);
        }
        assert!(saw_critical_tick, "the fixture has one critical tick");
        assert_eq!(overall_state(&service).as_deref(), Some("degraded"));
    }
}
