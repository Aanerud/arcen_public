//! Transport path signal used by live video rate control.
//!
//! This module is pure telemetry data: no transport runtime and no platform
//! calls. Concrete adapters fill it from their own counters.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::Duration;

/// How long a baseline RTT sample stays authoritative before a longer clear
/// path is allowed to become the new baseline.
pub const PATH_BASELINE_WINDOW: Duration = Duration::from_secs(10);

/// Point-in-time transport path truth for the adaptive video rate controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathSignal {
    /// Current smoothed RTT in microseconds.
    pub rtt_micros: u64,
    /// Lowest RTT observed in the current baseline window, in microseconds.
    pub baseline_rtt_micros: u64,
    /// Current congestion window in bytes.
    pub congestion_window_bytes: u64,
    /// Best available estimate of bytes in flight/queued.
    pub bytes_in_flight: Option<u64>,
    /// Congestion events since the previous sample.
    pub congestion_events_delta: u64,
    /// Lost packets since the previous sample.
    pub lost_packets_delta: u64,
    /// Lost bytes since the previous sample.
    pub lost_bytes_delta: u64,
    /// Packets sent since the previous sample.
    pub sent_packets_delta: u64,
}

impl PathSignal {
    /// Current RTT as a [`Duration`].
    #[must_use]
    pub const fn rtt(self) -> Duration {
        Duration::from_micros(self.rtt_micros)
    }

    /// Baseline/minimum RTT as a [`Duration`].
    #[must_use]
    pub const fn baseline_rtt(self) -> Duration {
        Duration::from_micros(self.baseline_rtt_micros)
    }

    /// Queueing delay above the baseline RTT.
    #[must_use]
    pub fn queue_delay(self) -> Duration {
        self.rtt().saturating_sub(self.baseline_rtt())
    }

    /// Lost packet share in this interval.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn loss_rate(self) -> f64 {
        if self.sent_packets_delta == 0 {
            return 0.0;
        }
        self.lost_packets_delta as f64 / self.sent_packets_delta as f64
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BaselineSample {
    observed_at: Duration,
    rtt_micros: u64,
}

/// Tracks windowed baseline RTT and converts cumulative transport counters
/// into interval deltas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathSignalState {
    window: Duration,
    samples: VecDeque<BaselineSample>,
    congestion_events: u64,
    lost_packets: u64,
    lost_bytes: u64,
    sent_packets: u64,
}

impl Default for PathSignalState {
    fn default() -> Self {
        Self::new(PATH_BASELINE_WINDOW)
    }
}

impl PathSignalState {
    /// Creates a tracker with a caller-selected baseline window.
    #[must_use]
    pub const fn new(window: Duration) -> Self {
        Self {
            window,
            samples: VecDeque::new(),
            congestion_events: 0,
            lost_packets: 0,
            lost_bytes: 0,
            sent_packets: 0,
        }
    }

    /// Observes one set of cumulative path counters at a caller-injected time.
    #[must_use]
    pub fn observe(&mut self, observed_at: Duration, counters: PathSignalCounters) -> PathSignal {
        let rtt_micros = micros(counters.rtt);
        self.samples.push_back(BaselineSample {
            observed_at,
            rtt_micros,
        });
        while self
            .samples
            .front()
            .is_some_and(|sample| observed_at.saturating_sub(sample.observed_at) > self.window)
        {
            let _ = self.samples.pop_front();
        }
        let baseline = self
            .samples
            .iter()
            .map(|sample| sample.rtt_micros)
            .min()
            .unwrap_or(rtt_micros);
        let signal = PathSignal {
            rtt_micros,
            baseline_rtt_micros: baseline,
            congestion_window_bytes: counters.congestion_window_bytes,
            bytes_in_flight: counters.bytes_in_flight,
            congestion_events_delta: counters
                .congestion_events
                .saturating_sub(self.congestion_events),
            lost_packets_delta: counters.lost_packets.saturating_sub(self.lost_packets),
            lost_bytes_delta: counters.lost_bytes.saturating_sub(self.lost_bytes),
            sent_packets_delta: counters.sent_packets.saturating_sub(self.sent_packets),
        };
        self.congestion_events = counters.congestion_events;
        self.lost_packets = counters.lost_packets;
        self.lost_bytes = counters.lost_bytes;
        self.sent_packets = counters.sent_packets;
        signal
    }
}

/// Cumulative counters from a concrete transport.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathSignalCounters {
    pub rtt: Duration,
    pub congestion_window_bytes: u64,
    pub bytes_in_flight: Option<u64>,
    pub congestion_events: u64,
    pub lost_packets: u64,
    pub lost_bytes: u64,
    pub sent_packets: u64,
}

#[must_use]
fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(rtt_ms: u64, congestion: u64, lost: u64, sent: u64) -> PathSignalCounters {
        PathSignalCounters {
            rtt: Duration::from_millis(rtt_ms),
            congestion_window_bytes: 12_000,
            bytes_in_flight: None,
            congestion_events: congestion,
            lost_packets: lost,
            lost_bytes: lost * 1_200,
            sent_packets: sent,
        }
    }

    #[test]
    fn baseline_is_windowed_and_counters_are_deltas() {
        let mut state = PathSignalState::new(Duration::from_secs(10));
        let first = state.observe(Duration::ZERO, counters(50, 2, 3, 10));
        assert_eq!(first.baseline_rtt_micros, 50_000);
        assert_eq!(first.lost_bytes_delta, 3_600);
        let second = state.observe(Duration::from_secs(5), counters(40, 5, 4, 25));
        assert_eq!(second.baseline_rtt_micros, 40_000);
        assert_eq!(second.congestion_events_delta, 3);
        assert_eq!(second.lost_packets_delta, 1);
        assert_eq!(second.sent_packets_delta, 15);
        let stepped = state.observe(Duration::from_secs(16), counters(60, 5, 4, 30));
        assert_eq!(stepped.baseline_rtt_micros, 60_000);
        assert_eq!(stepped.queue_delay(), Duration::ZERO);
    }
}
