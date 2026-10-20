//! Small, bounded per-lane health counters and their snapshot.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{LANE_ROLE_COUNT, LaneEndpoint, LaneRole};

use crate::lanes::{LaneEndReason, LaneState};

/// Records a lane refused or discarded instead of delivering, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LaneDropCounts {
    /// `try_send` refused because the role's queue was full. The record
    /// went back to the caller; nothing was lost silently.
    pub rejected_full: u64,
    /// Refused because it exceeded the role's frame bound.
    pub rejected_oversize: u64,
    /// Refused because the lane had closed.
    pub rejected_closed: u64,
    /// Queued records discarded when their lane failed, was superseded or
    /// the attachment ended. A failed lane's payload is never resent.
    pub discarded_on_close: u64,
}

/// Health of one role's lane, across its generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneRoleStatus {
    /// Role.
    pub role: LaneRole,
    /// Registry state.
    pub state: LaneState,
    /// Records queued for the current writer.
    pub queued_records: u64,
    /// Payload bytes queued for the current writer.
    pub queued_bytes: u64,
    /// Age of the oldest queued record.
    pub oldest_queued_age: Duration,
    /// Largest queue age observed at dequeue.
    pub max_queue_age: Duration,
    /// Time the last completed record spent inside the QUIC write.
    pub last_writer_wait: Duration,
    /// Largest write time observed.
    pub max_writer_wait: Duration,
    /// Records written.
    pub records_sent: u64,
    /// Payload bytes written.
    pub bytes_sent: u64,
    /// Records read.
    pub records_received: u64,
    /// Payload bytes read.
    pub bytes_received: u64,
    /// Lanes opened or admitted.
    pub lanes_opened: u64,
    /// Lane failures reported to the registry.
    pub failures: u64,
    /// Streams this endpoint reset or stopped for the role.
    pub resets: u64,
    /// Lanes replaced by a newer generation.
    pub superseded: u64,
    /// Peer streams declaring this role that were refused.
    pub refused: u64,
    /// `LANE_STARVED` episodes.
    pub starved: u64,
    /// Times a flooding interactive lane was moved to the audio priority.
    pub priority_demotions: u64,
    /// Drops and rejections, by reason.
    pub drops: LaneDropCounts,
}

/// Point-in-time status of one connection's lanes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneStatusSnapshot {
    /// Local endpoint.
    pub local: LaneEndpoint,
    /// Why the attachment ended, if it did.
    pub ended: Option<LaneEndReason>,
    /// Per role, in [`LaneRole::ALL`] order.
    pub roles: [LaneRoleStatus; LANE_ROLE_COUNT],
    /// Peer streams refused before their preface named a role.
    pub refused_without_role: u64,
    /// Lifecycle events withheld by the rate limit.
    pub telemetry_suppressed: u64,
}

impl LaneStatusSnapshot {
    /// Status of `role`.
    #[must_use]
    pub const fn role(&self, role: LaneRole) -> &LaneRoleStatus {
        &self.roles[role.index()]
    }
}

#[derive(Default)]
pub(super) struct RoleStats {
    pub(super) queued_records: AtomicU64,
    pub(super) queued_bytes: AtomicU64,
    /// Micros since the connection epoch of the oldest queued record, plus
    /// one; zero when the queue is empty.
    pub(super) oldest_queued_at: AtomicU64,
    pub(super) max_queue_age_us: AtomicU64,
    pub(super) last_writer_wait_us: AtomicU64,
    pub(super) max_writer_wait_us: AtomicU64,
    pub(super) records_sent: AtomicU64,
    pub(super) bytes_sent: AtomicU64,
    pub(super) records_received: AtomicU64,
    pub(super) bytes_received: AtomicU64,
    pub(super) lanes_opened: AtomicU64,
    pub(super) failures: AtomicU64,
    pub(super) resets: AtomicU64,
    pub(super) superseded: AtomicU64,
    pub(super) refused: AtomicU64,
    pub(super) starved: AtomicU64,
    pub(super) priority_demotions: AtomicU64,
    pub(super) rejected_full: AtomicU64,
    pub(super) rejected_oversize: AtomicU64,
    pub(super) rejected_closed: AtomicU64,
    pub(super) discarded_on_close: AtomicU64,
}

pub(super) fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

pub(super) fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub(super) fn add(counter: &AtomicU64, value: u64) {
    counter.fetch_add(value, Ordering::Relaxed);
}

pub(super) fn sub(counter: &AtomicU64, value: u64) {
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
        Some(current.saturating_sub(value))
    });
}

pub(super) fn raise(counter: &AtomicU64, value: u64) {
    counter.fetch_max(value, Ordering::Relaxed);
}

impl RoleStats {
    pub(super) fn snapshot(
        &self,
        role: LaneRole,
        state: LaneState,
        epoch: Instant,
    ) -> LaneRoleStatus {
        let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        let oldest = load(&self.oldest_queued_at);
        let oldest_queued_age = if oldest == 0 {
            Duration::ZERO
        } else {
            let now = micros(epoch.elapsed());
            Duration::from_micros(now.saturating_sub(oldest - 1))
        };
        LaneRoleStatus {
            role,
            state,
            queued_records: load(&self.queued_records),
            queued_bytes: load(&self.queued_bytes),
            oldest_queued_age,
            max_queue_age: Duration::from_micros(load(&self.max_queue_age_us)),
            last_writer_wait: Duration::from_micros(load(&self.last_writer_wait_us)),
            max_writer_wait: Duration::from_micros(load(&self.max_writer_wait_us)),
            records_sent: load(&self.records_sent),
            bytes_sent: load(&self.bytes_sent),
            records_received: load(&self.records_received),
            bytes_received: load(&self.bytes_received),
            lanes_opened: load(&self.lanes_opened),
            failures: load(&self.failures),
            resets: load(&self.resets),
            superseded: load(&self.superseded),
            refused: load(&self.refused),
            starved: load(&self.starved),
            priority_demotions: load(&self.priority_demotions),
            drops: LaneDropCounts {
                rejected_full: load(&self.rejected_full),
                rejected_oversize: load(&self.rejected_oversize),
                rejected_closed: load(&self.rejected_closed),
                discarded_on_close: load(&self.discarded_on_close),
            },
        }
    }
}
