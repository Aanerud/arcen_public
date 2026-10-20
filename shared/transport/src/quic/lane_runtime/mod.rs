//! Isolated QUIC lane runtime: one accept dispatcher per connection, lane
//! openers, and an independent bounded writer and reader per lane.
//!
//! This is the I/O half of ADR 0014. The pure contracts it drives are
//! [`arcen_protocol::lanes`] (roles, preface, attachment grant) and
//! [`crate::lanes`] (registry, recovery policy, priorities, frame bounds);
//! every admission, refusal and recovery decision is the registry's, and
//! this module only performs the stream operations those decisions require.
//!
//! - [`LaneConnection::start`] spawns the connection's only caller of
//!   `accept_bi`/`accept_uni`. It reads each stream's fixed-size preface
//!   under [`LaneRuntimeConfig::preface_timeout`], never reading past it,
//!   asks the registry, and either hands the stream to its role's owner
//!   ([`LaneConnection::next_peer_lane`]), holds it unread while a Deck's
//!   authorization is pending, or resets and stops it with
//!   [`LaneStreamCode::Refused`] before any payload reaches a worker.
//! - [`LaneConnection::open_lane`] opens a lane the local endpoint owns with
//!   its preface, contract priority and next generation;
//!   [`LaneConnection::reopen`] follows a [`LaneRecoveryAction::Reopen`]. No
//!   call ever routes a role onto another role's stream.
//! - [`Lane::split`] gives each lane its own [`LaneSender`] (bounded queue
//!   and dedicated writer task) and [`LaneReceiver`]. A writer blocked on
//!   stream credit blocks only its own role.
//!
//! Every product runs on this runtime through the product adapter in
//! [`super::session_lanes`]; the Windows and macOS Piers, whose session runs
//! behind a local socket, through [`super::PierLaneRelay`] on top of it. See
//! `docs/architecture/transport.md`.
//!
//! [`LaneStreamCode::Refused`]: arcen_protocol::lanes::LaneStreamCode::Refused
//! [`LaneRecoveryAction::Reopen`]: crate::lanes::LaneRecoveryAction::Reopen

mod connection;
mod lane;
mod reader;
mod stats;
mod telemetry;
mod writer;

use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::time::Duration;

use arcen_protocol::lanes::{LaneEndpoint, LaneRole};

use crate::BoundedTransportPolicy;

pub use connection::{LaneConnection, LaneOpenFailure, LaneWaitError, classify_peer_close};
pub use lane::{Lane, LaneDuplex};
pub use reader::LaneReceiver;
pub use stats::{LaneDropCounts, LaneRoleStatus, LaneStatusSnapshot};
pub use telemetry::{LaneTelemetrySink, ObservabilityLaneSink};
pub use writer::{LaneExit, LaneSendError, LaneSender};

/// Default bound for reading a peer lane's complete preface.
pub const DEFAULT_LANE_PREFACE_TIMEOUT: Duration = Duration::from_secs(5);

/// Peer streams whose prefaces may be read concurrently. QUIC's stream
/// limits already bound this; the explicit cap keeps the dispatcher bounded
/// whatever limits a caller configures. Further streams are refused.
pub const MAX_CONCURRENT_PREFACE_READS: usize = 8;

/// Lifecycle events a connection may emit in a burst before rate limiting.
pub const LANE_TELEMETRY_BURST: u32 = 32;

/// Sustained lifecycle events per second per connection after the burst.
pub const LANE_TELEMETRY_PER_SECOND: u32 = 4;

/// Low-priority lanes (video, bulk) hand QUIC at most this many bytes per
/// write call and yield between calls, so a waiting audio, input or control
/// writer is scheduled whenever connection send-window space frees up.
pub const LOW_PRIORITY_WRITE_CHUNK_BYTES: usize = 64 * 1024;

/// Bytes an interactive lane (input/cursor, control) may write within
/// [`INTERACTIVE_LANE_BURST_WINDOW`] at its contract priority before its
/// stream shares the audio priority. About 5 Mbit/s: far above real input and
/// control traffic, far below a flood.
pub const INTERACTIVE_LANE_BURST_BYTES: usize = 64 * 1024;

/// Window over which [`INTERACTIVE_LANE_BURST_BYTES`] is measured.
pub const INTERACTIVE_LANE_BURST_WINDOW: Duration = Duration::from_millis(100);

/// Runtime configuration for one connection's lanes.
#[derive(Clone)]
pub struct LaneRuntimeConfig {
    /// Which endpoint this process is.
    pub local: LaneEndpoint,
    /// Frame bounds ([`crate::lanes::lane_max_frame_bytes`]).
    pub policy: BoundedTransportPolicy,
    /// Bound for receiving a peer lane's complete preface.
    pub preface_timeout: Duration,
    /// Non-blocking lifecycle sink; `None` keeps only the status counters.
    pub telemetry: Option<Arc<dyn LaneTelemetrySink>>,
}

impl Debug for LaneRuntimeConfig {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaneRuntimeConfig")
            .field("local", &self.local)
            .field("policy", &self.policy)
            .field("preface_timeout", &self.preface_timeout)
            .field("telemetry", &self.telemetry.is_some())
            .finish()
    }
}

impl LaneRuntimeConfig {
    /// Defaults for `local`: default policy and preface timeout, no sink.
    #[must_use]
    pub fn new(local: LaneEndpoint) -> Self {
        Self {
            local,
            policy: BoundedTransportPolicy::default(),
            preface_timeout: DEFAULT_LANE_PREFACE_TIMEOUT,
            telemetry: None,
        }
    }

    /// Replaces the preface timeout.
    #[must_use]
    pub const fn with_preface_timeout(mut self, timeout: Duration) -> Self {
        self.preface_timeout = timeout;
        self
    }

    /// Replaces the frame policy.
    #[must_use]
    pub const fn with_policy(mut self, policy: BoundedTransportPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Installs a lifecycle sink.
    #[must_use]
    pub fn with_telemetry(mut self, sink: Arc<dyn LaneTelemetrySink>) -> Self {
        self.telemetry = Some(sink);
        self
    }
}

/// Bounds of one lane's send queue and the wait that counts as starvation.
///
/// Each role has its own queue, so a full video queue never takes room from
/// audio, input or control. A record larger than `max_bytes` is accepted
/// only into an empty queue, so one maximal frame can always be sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneQueueBounds {
    /// Queued records.
    pub max_records: usize,
    /// Queued payload bytes.
    pub max_bytes: usize,
    /// Queue age or write wait that emits `LANE_STARVED`.
    pub starvation_bound: Duration,
}

impl LaneQueueBounds {
    /// Default bounds for `role`.
    ///
    /// Video holds a few encoded frames, so a stalled path pushes back on
    /// the encoder instead of aging pictures; audio about 320 ms of 20 ms
    /// frames; input enough for a burst of edges without silently dropping
    /// any (a full queue is reported to the caller, never discarded).
    #[must_use]
    pub const fn for_role(role: LaneRole) -> Self {
        match role {
            LaneRole::Control => Self {
                max_records: 64,
                max_bytes: 2 * 1024 * 1024,
                starvation_bound: Duration::from_millis(250),
            },
            LaneRole::Video => Self {
                max_records: 4,
                max_bytes: 32 * 1024 * 1024,
                starvation_bound: Duration::from_secs(1),
            },
            LaneRole::AudioOutput | LaneRole::Microphone => Self {
                max_records: 16,
                max_bytes: 128 * 1024,
                starvation_bound: Duration::from_millis(100),
            },
            LaneRole::InputCursor => Self {
                max_records: 256,
                max_bytes: 512 * 1024,
                starvation_bound: Duration::from_millis(50),
            },
            LaneRole::BulkAux => Self {
                max_records: 8,
                max_bytes: 8 * 1024 * 1024,
                starvation_bound: Duration::from_secs(2),
            },
        }
    }
}

const fn is_low_priority(role: LaneRole) -> bool {
    matches!(role, LaneRole::Video | LaneRole::BulkAux)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lanes::lane_max_frame_bytes;

    #[test]
    fn interactive_queues_hold_at_least_one_maximal_frame() {
        let policy = BoundedTransportPolicy::default();
        for role in [
            LaneRole::AudioOutput,
            LaneRole::Microphone,
            LaneRole::InputCursor,
            LaneRole::Control,
        ] {
            let bounds = LaneQueueBounds::for_role(role);
            assert!(
                bounds.max_bytes >= lane_max_frame_bytes(role, &policy),
                "{role}"
            );
            assert!(bounds.max_records >= 2, "{role}");
        }
    }

    #[test]
    fn interactive_lanes_have_tighter_starvation_bounds_than_video_and_bulk() {
        let video = LaneQueueBounds::for_role(LaneRole::Video).starvation_bound;
        let bulk = LaneQueueBounds::for_role(LaneRole::BulkAux).starvation_bound;
        for role in [
            LaneRole::AudioOutput,
            LaneRole::Microphone,
            LaneRole::InputCursor,
            LaneRole::Control,
        ] {
            let bound = LaneQueueBounds::for_role(role).starvation_bound;
            assert!(bound < video && bound < bulk, "{role}");
        }
    }
}
