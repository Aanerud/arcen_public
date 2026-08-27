//! Off-hot-path snapshots over registered atomic counters.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use arcen_telemetry::{HealthAssessment, QosSample, QosTargets, assess_health};

/// Hot-path counters read by [`QosSampler`] only when a caller requests a sample.
#[derive(Debug, Default)]
pub struct QosCounters {
    /// Frames submitted by the host.
    pub frames_sent: AtomicU64,
    /// Frames dropped by the host.
    pub frames_dropped: AtomicU64,
    /// Frames decoded by the client.
    pub frames_decoded: AtomicU64,
    /// Frames presented by the client.
    pub frames_presented: AtomicU64,
    /// Input events in the sampling window.
    pub input_events: AtomicU64,
}

/// Caller-owned instantaneous values combined with atomic counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CallerQosSnapshot {
    /// Observed frames per second.
    pub fps_actual: Option<u32>,
    /// Requested frames per second.
    pub fps_target: Option<u32>,
    /// Estimated path bandwidth.
    pub bandwidth_mbps: Option<u32>,
    /// Host capture time.
    pub capture_time_ms: Option<u32>,
    /// Host encode time.
    pub encode_time_ms: Option<u32>,
    /// Client decode time.
    pub decode_time_ms: Option<u32>,
    /// Client display time.
    pub display_time_ms: Option<u32>,
    /// Application round-trip time.
    pub rtt_ms: Option<u32>,
    /// Input-to-acknowledgement latency.
    pub input_latency_ms: Option<u32>,
    /// Consecutive missed health intervals.
    pub heartbeat_misses: Option<u32>,
}

/// Clockless sampler over caller-registered atomic counters.
#[derive(Debug)]
pub struct QosSampler<'a> {
    counters: &'a QosCounters,
}

impl<'a> QosSampler<'a> {
    /// Registers a counter set.
    #[must_use]
    pub const fn new(counters: &'a QosCounters) -> Self {
        Self { counters }
    }

    /// Reads counters and combines caller-supplied time and instantaneous facts.
    #[must_use]
    pub fn sample(&self, timestamp_ms: u64, caller: CallerQosSnapshot) -> QosSample {
        QosSample {
            timestamp_ms,
            fps_actual: caller.fps_actual,
            fps_target: caller.fps_target,
            bandwidth_mbps: caller.bandwidth_mbps,
            frames_sent: Some(self.counters.frames_sent.load(Ordering::Relaxed)),
            frames_dropped: Some(self.counters.frames_dropped.load(Ordering::Relaxed)),
            frames_decoded: Some(self.counters.frames_decoded.load(Ordering::Relaxed)),
            frames_presented: Some(self.counters.frames_presented.load(Ordering::Relaxed)),
            capture_time_ms: caller.capture_time_ms,
            encode_time_ms: caller.encode_time_ms,
            decode_time_ms: caller.decode_time_ms,
            display_time_ms: caller.display_time_ms,
            rtt_ms: caller.rtt_ms,
            input_latency_ms: caller.input_latency_ms,
            input_events: Some(self.counters.input_events.swap(0, Ordering::Relaxed)),
            heartbeat_misses: caller.heartbeat_misses,
        }
    }

    /// Evaluates one explicit sample using the pure PR1 health contract.
    #[must_use]
    pub fn assess(sample: &QosSample, targets: &QosTargets) -> HealthAssessment {
        assess_health(sample, targets)
    }
}

/// Atomics for application heartbeat truth.
#[derive(Debug, Default)]
pub struct HeartbeatCounters {
    sent_sequence: AtomicU64,
    received_sequence: AtomicU64,
    missed_intervals: AtomicU32,
}

impl HeartbeatCounters {
    /// Records the most recently sent application sequence.
    pub fn record_sent(&self, sequence: u64) {
        self.sent_sequence.store(sequence, Ordering::Relaxed);
    }

    /// Records the most recently echoed application sequence and clears misses.
    pub fn record_received(&self, sequence: u64) {
        self.received_sequence.store(sequence, Ordering::Relaxed);
        self.missed_intervals.store(0, Ordering::Relaxed);
    }

    /// Increments and returns consecutive missed intervals.
    #[must_use]
    pub fn record_miss(&self) -> u32 {
        self.missed_intervals.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Reads one snapshot with the caller's clock and measured RTT.
    #[must_use]
    pub fn snapshot(&self, timestamp_ms: u64, rtt_ms: Option<u32>) -> HeartbeatSnapshot {
        HeartbeatSnapshot {
            timestamp_ms,
            sent_sequence: self.sent_sequence.load(Ordering::Relaxed),
            received_sequence: self.received_sequence.load(Ordering::Relaxed),
            missed_intervals: self.missed_intervals.load(Ordering::Relaxed),
            rtt_ms,
        }
    }
}

/// Caller-clocked heartbeat snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeartbeatSnapshot {
    /// Caller clock in milliseconds.
    pub timestamp_ms: u64,
    /// Most recently sent application sequence.
    pub sent_sequence: u64,
    /// Most recently received echoed sequence.
    pub received_sequence: u64,
    /// Consecutive missed intervals.
    pub missed_intervals: u32,
    /// Caller-measured application RTT.
    pub rtt_ms: Option<u32>,
}
