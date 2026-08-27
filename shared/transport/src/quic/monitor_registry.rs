//! Pure, bounded registry for the direct-monitor stream foundation's
//! expected roster.
//!
//! [`MonitorStreamRoster`] tracks which of the 1-4 expected monitor streams
//! for one session/attachment/topology generation have registered, so a
//! caller can gate readiness (for example, before allowing initial
//! keyframes to flow) without touching Quinn or Tokio. This module performs
//! no I/O and has no platform dependencies: it is exercised entirely through
//! plain values and [`super::monitor::MonitorStreamIdentity`].

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{Display, Formatter};
use std::num::{NonZeroU16, NonZeroU64};

use super::monitor::{
    MAX_MONITOR_STREAMS_PER_CONNECTION, MEDIA_PLAN_FINGERPRINT_BYTES, MonitorStreamIdentity,
};

/// One expected monitor stream: its session-scoped id and the exact
/// media-plan fingerprint its stream's preface must present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExpectedMonitorStream {
    /// Session-scoped, nonzero monitor id.
    pub session_monitor_id: NonZeroU16,
    /// Exact media-plan fingerprint expected for this monitor.
    pub media_plan_fingerprint: [u8; MEDIA_PLAN_FINGERPRINT_BYTES],
}

/// Explicit, fail-closed roster rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorRosterError {
    /// The roster must contain 1-4 monitors
    /// ([`MAX_MONITOR_STREAMS_PER_CONNECTION`]).
    InvalidRosterSize,
    /// Two expected entries claimed the same monitor id.
    DuplicateExpectedMonitor,
    /// A registering stream claimed a different session identifier.
    SessionMismatch,
    /// A registering stream claimed a stale attachment generation.
    StaleAttachmentGeneration,
    /// A registering stream claimed a stale topology generation.
    StaleTopologyGeneration,
    /// A registering stream claimed a monitor id outside the roster.
    UnknownMonitor,
    /// A monitor id has already registered a stream once.
    DuplicateMonitor,
    /// A registering stream's media-plan fingerprint did not match.
    FingerprintMismatch,
}

impl Display for MonitorRosterError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidRosterSize => "monitor roster must contain 1-4 expected monitors",
            Self::DuplicateExpectedMonitor => {
                "two expected roster entries claimed the same monitor id"
            }
            Self::SessionMismatch => "registering stream claimed a different session identifier",
            Self::StaleAttachmentGeneration => {
                "registering stream claimed a stale attachment generation"
            }
            Self::StaleTopologyGeneration => {
                "registering stream claimed a stale topology generation"
            }
            Self::UnknownMonitor => "registering stream claimed a monitor id outside the roster",
            Self::DuplicateMonitor => "monitor id already registered a stream once",
            Self::FingerprintMismatch => {
                "registering stream's media-plan fingerprint did not match"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for MonitorRosterError {}

/// Bounded, pure registry of one session/attachment/topology generation's
/// expected monitor roster.
///
/// Streams are accepted exactly once: after [`Self::register`] succeeds for
/// a monitor id, a later call for the same id always fails with
/// [`MonitorRosterError::DuplicateMonitor`], regardless of registration
/// order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorStreamRoster {
    session_id: String,
    attachment_generation: NonZeroU64,
    topology_generation: NonZeroU64,
    expected: BTreeMap<NonZeroU16, [u8; MEDIA_PLAN_FINGERPRINT_BYTES]>,
    accepted: BTreeSet<NonZeroU16>,
}

impl MonitorStreamRoster {
    /// Creates a bounded roster for exactly one session/attachment/topology
    /// generation.
    ///
    /// # Errors
    ///
    /// Returns [`MonitorRosterError::InvalidRosterSize`] unless `expected`
    /// yields 1-4 entries, or
    /// [`MonitorRosterError::DuplicateExpectedMonitor`] if two entries
    /// claim the same monitor id.
    pub fn new(
        session_id: impl Into<String>,
        attachment_generation: NonZeroU64,
        topology_generation: NonZeroU64,
        expected: impl IntoIterator<Item = ExpectedMonitorStream>,
    ) -> Result<Self, MonitorRosterError> {
        let mut map = BTreeMap::new();
        for entry in expected {
            if map
                .insert(entry.session_monitor_id, entry.media_plan_fingerprint)
                .is_some()
            {
                return Err(MonitorRosterError::DuplicateExpectedMonitor);
            }
        }
        if map.is_empty() || map.len() > MAX_MONITOR_STREAMS_PER_CONNECTION {
            return Err(MonitorRosterError::InvalidRosterSize);
        }
        Ok(Self {
            session_id: session_id.into(),
            attachment_generation,
            topology_generation,
            expected: map,
            accepted: BTreeSet::new(),
        })
    }

    /// Registers one validated preface identity against the roster,
    /// accepting each monitor id exactly once.
    ///
    /// # Errors
    ///
    /// Returns a precise, fail-closed reason:
    /// [`MonitorRosterError::SessionMismatch`],
    /// [`MonitorRosterError::StaleAttachmentGeneration`],
    /// [`MonitorRosterError::StaleTopologyGeneration`],
    /// [`MonitorRosterError::UnknownMonitor`],
    /// [`MonitorRosterError::DuplicateMonitor`], or
    /// [`MonitorRosterError::FingerprintMismatch`].
    pub fn register(&mut self, identity: &MonitorStreamIdentity) -> Result<(), MonitorRosterError> {
        if identity.session_id() != self.session_id {
            return Err(MonitorRosterError::SessionMismatch);
        }
        if identity.attachment_generation() != self.attachment_generation {
            return Err(MonitorRosterError::StaleAttachmentGeneration);
        }
        if identity.topology_generation() != self.topology_generation {
            return Err(MonitorRosterError::StaleTopologyGeneration);
        }
        let monitor_id = identity.session_monitor_id();
        let Some(expected_fingerprint) = self.expected.get(&monitor_id) else {
            return Err(MonitorRosterError::UnknownMonitor);
        };
        if self.accepted.contains(&monitor_id) {
            return Err(MonitorRosterError::DuplicateMonitor);
        }
        if expected_fingerprint != identity.media_plan_fingerprint() {
            return Err(MonitorRosterError::FingerprintMismatch);
        }
        self.accepted.insert(monitor_id);
        Ok(())
    }

    /// Returns `true` once every expected monitor has registered.
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.accepted.len() == self.expected.len()
    }

    /// Returns the expected monitor ids that have not yet registered, in
    /// ascending order.
    #[must_use]
    pub fn missing_monitors(&self) -> Vec<NonZeroU16> {
        self.expected
            .keys()
            .filter(|id| !self.accepted.contains(*id))
            .copied()
            .collect()
    }

    /// Returns the session identifier this roster is scoped to.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the attachment generation this roster is scoped to.
    #[must_use]
    pub const fn attachment_generation(&self) -> NonZeroU64 {
        self.attachment_generation
    }

    /// Returns the topology generation this roster is scoped to.
    #[must_use]
    pub const fn topology_generation(&self) -> NonZeroU64 {
        self.topology_generation
    }

    /// Returns the number of expected monitors (1-4).
    #[must_use]
    pub fn expected_len(&self) -> usize {
        self.expected.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz16(value: u16) -> NonZeroU16 {
        NonZeroU16::new(value).expect("nonzero")
    }

    fn nz64(value: u64) -> NonZeroU64 {
        NonZeroU64::new(value).expect("nonzero")
    }

    fn expected(monitor_id: u16, fingerprint_byte: u8) -> ExpectedMonitorStream {
        ExpectedMonitorStream {
            session_monitor_id: nz16(monitor_id),
            media_plan_fingerprint: [fingerprint_byte; MEDIA_PLAN_FINGERPRINT_BYTES],
        }
    }

    fn identity_for(
        session_id: &str,
        attachment_generation: u64,
        topology_generation: u64,
        monitor_id: u16,
        fingerprint_byte: u8,
    ) -> MonitorStreamIdentity {
        MonitorStreamIdentity::new(
            session_id,
            nz64(attachment_generation),
            nz64(topology_generation),
            nz16(monitor_id),
            [fingerprint_byte; MEDIA_PLAN_FINGERPRINT_BYTES],
        )
        .expect("valid identity")
    }

    #[test]
    fn empty_roster_is_rejected() {
        assert_eq!(
            MonitorStreamRoster::new("session-1", nz64(1), nz64(1), []),
            Err(MonitorRosterError::InvalidRosterSize)
        );
    }

    #[test]
    fn oversized_roster_is_rejected() {
        let entries = (1..=5).map(|id| expected(id, 1));
        assert_eq!(
            MonitorStreamRoster::new("session-1", nz64(1), nz64(1), entries),
            Err(MonitorRosterError::InvalidRosterSize)
        );
    }

    #[test]
    fn duplicate_expected_monitor_is_rejected() {
        assert_eq!(
            MonitorStreamRoster::new(
                "session-1",
                nz64(1),
                nz64(1),
                [expected(1, 1), expected(1, 2)],
            ),
            Err(MonitorRosterError::DuplicateExpectedMonitor)
        );
    }

    #[test]
    fn single_monitor_roster_becomes_ready_after_registration() {
        let mut roster = MonitorStreamRoster::new("session-1", nz64(1), nz64(1), [expected(1, 7)])
            .expect("roster");
        assert!(!roster.is_ready());
        assert_eq!(roster.missing_monitors(), vec![nz16(1)]);

        roster
            .register(&identity_for("session-1", 1, 1, 1, 7))
            .expect("registers");
        assert!(roster.is_ready());
        assert!(roster.missing_monitors().is_empty());
    }

    #[test]
    fn four_monitor_roster_accepts_out_of_order_registration() {
        let entries = [
            expected(1, 10),
            expected(2, 20),
            expected(3, 30),
            expected(4, 40),
        ];
        let mut roster =
            MonitorStreamRoster::new("session-1", nz64(5), nz64(9), entries).expect("roster");
        assert_eq!(roster.expected_len(), 4);

        // Registers out of numeric order: 3, 1, 4, 2.
        for (monitor_id, fingerprint_byte) in [(3, 30), (1, 10), (4, 40), (2, 20)] {
            roster
                .register(&identity_for(
                    "session-1",
                    5,
                    9,
                    monitor_id,
                    fingerprint_byte,
                ))
                .unwrap_or_else(|error| panic!("monitor {monitor_id} should register: {error}"));
        }
        assert!(roster.is_ready());
        assert!(roster.missing_monitors().is_empty());
    }

    #[test]
    fn session_mismatch_is_rejected() {
        let mut roster = MonitorStreamRoster::new("session-1", nz64(1), nz64(1), [expected(1, 7)])
            .expect("roster");
        assert_eq!(
            roster.register(&identity_for("session-2", 1, 1, 1, 7)),
            Err(MonitorRosterError::SessionMismatch)
        );
    }

    #[test]
    fn stale_attachment_generation_is_rejected() {
        let mut roster = MonitorStreamRoster::new("session-1", nz64(2), nz64(1), [expected(1, 7)])
            .expect("roster");
        assert_eq!(
            roster.register(&identity_for("session-1", 1, 1, 1, 7)),
            Err(MonitorRosterError::StaleAttachmentGeneration)
        );
    }

    #[test]
    fn stale_topology_generation_is_rejected() {
        let mut roster = MonitorStreamRoster::new("session-1", nz64(1), nz64(2), [expected(1, 7)])
            .expect("roster");
        assert_eq!(
            roster.register(&identity_for("session-1", 1, 1, 1, 7)),
            Err(MonitorRosterError::StaleTopologyGeneration)
        );
    }

    #[test]
    fn unknown_monitor_is_rejected() {
        let mut roster = MonitorStreamRoster::new("session-1", nz64(1), nz64(1), [expected(1, 7)])
            .expect("roster");
        assert_eq!(
            roster.register(&identity_for("session-1", 1, 1, 2, 7)),
            Err(MonitorRosterError::UnknownMonitor)
        );
    }

    #[test]
    fn duplicate_monitor_registration_is_rejected() {
        let mut roster = MonitorStreamRoster::new("session-1", nz64(1), nz64(1), [expected(1, 7)])
            .expect("roster");
        roster
            .register(&identity_for("session-1", 1, 1, 1, 7))
            .expect("first registration succeeds");
        assert_eq!(
            roster.register(&identity_for("session-1", 1, 1, 1, 7)),
            Err(MonitorRosterError::DuplicateMonitor)
        );
    }

    #[test]
    fn wrong_fingerprint_is_rejected_and_slot_remains_open() {
        let mut roster = MonitorStreamRoster::new("session-1", nz64(1), nz64(1), [expected(1, 7)])
            .expect("roster");
        assert_eq!(
            roster.register(&identity_for("session-1", 1, 1, 1, 99)),
            Err(MonitorRosterError::FingerprintMismatch)
        );
        assert!(!roster.is_ready());
        // A subsequent attempt with the correct fingerprint still succeeds:
        // a rejected wrong-fingerprint attempt did not consume the slot.
        roster
            .register(&identity_for("session-1", 1, 1, 1, 7))
            .expect("correct fingerprint registers");
        assert!(roster.is_ready());
    }
}
