// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Concrete transport adapters for the pure path signal telemetry type.

use std::fmt::Write as _;

pub use arcen_telemetry::{PathSignal, PathSignalCounters, PathSignalState};

/// Samples a live Quinn connection into the shared path signal tracker.
#[cfg(feature = "quic")]
#[must_use]
pub fn observe_quinn_path_signal(
    state: &mut PathSignalState,
    observed_at: std::time::Duration,
    connection: &quinn::Connection,
) -> PathSignal {
    let path = connection.stats().path;
    state.observe(
        observed_at,
        PathSignalCounters {
            rtt: path.rtt,
            congestion_window_bytes: path.cwnd,
            bytes_in_flight: None,
            congestion_events: path.congestion_events,
            lost_packets: path.lost_packets,
            lost_bytes: path.lost_bytes,
            sent_packets: path.sent_packets,
        },
    )
}

/// Builds the host's [`arcen_protocol::messages::PathQualityMsg`] from one
/// path-signal interval. Pure, so every Pier reports the same facts.
#[must_use]
pub fn path_quality_message(
    signal: PathSignal,
    congestion_control: &str,
    wire_bytes: u64,
    window: std::time::Duration,
) -> arcen_protocol::messages::PathQualityMsg {
    arcen_protocol::messages::PathQualityMsg {
        congestion_control: congestion_control.to_string(),
        rtt_us: signal.rtt_micros,
        min_rtt_us: signal.baseline_rtt_micros,
        cwnd_bytes: signal.congestion_window_bytes,
        sent_packets: signal.sent_packets_delta,
        lost_packets: signal.lost_packets_delta,
        congestion_events: signal.congestion_events_delta,
        wire_bytes,
        window_ms: u64::try_from(window.as_millis()).unwrap_or(u64::MAX),
        mtu_bytes: 0,
        black_holes: 0,
    }
}

/// A Deck's one-word reading of the host's [`PathQualityMsg`].
///
/// [`PathQualityMsg`]: arcen_protocol::messages::PathQualityMsg
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathGrade {
    /// Low loss, little queueing, interactive round trip.
    Good,
    /// Usable, but loss, queueing or distance is noticeable.
    Fair,
    /// Loss, queueing or distance that interactive use will feel.
    Poor,
}

impl PathGrade {
    /// Grades one window. Thresholds are interactive-desktop ones: a round
    /// trip under 80 ms with under 30 ms of queueing and under 0.5% loss is
    /// good; over 150 ms, 100 ms of queueing or 2% loss is poor.
    #[must_use]
    pub fn assess(path: &arcen_protocol::messages::PathQualityMsg) -> Self {
        let rtt_ms = path.rtt_us / 1000;
        let queue_ms = path.rtt_us.saturating_sub(path.min_rtt_us) / 1000;
        let loss = path.loss_permille().unwrap_or(0);
        if rtt_ms >= 150 || queue_ms >= 100 || loss >= 20 {
            Self::Poor
        } else if rtt_ms >= 80 || queue_ms >= 30 || loss >= 5 {
            Self::Fair
        } else {
            Self::Good
        }
    }

    /// Stable display word.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Good => "good",
            Self::Fair => "fair",
            Self::Poor => "poor",
        }
    }
}

/// One-line, measured description of the host's path for a Deck's
/// statistics view, e.g. `good · BBR · RTT 42 ms (min 38) · loss 0.1% ·
/// 9.8 Mbit/s · cwnd 120 KB`, followed by `· MTU 1252` when the host reports
/// its path MTU.
#[must_use]
pub fn path_quality_summary(path: &arcen_protocol::messages::PathQualityMsg) -> String {
    let controller = path
        .congestion_control_token()
        .map_or_else(|| "unknown".to_string(), str::to_ascii_uppercase);
    let loss = path.loss_permille().map_or_else(
        || "-".to_string(),
        |permille| format!("{}.{}%", permille / 10, permille % 10),
    );
    let rate = path.wire_bps().map_or_else(
        || "-".to_string(),
        |bps| format!("{}.{} Mbit/s", bps / 1_000_000, (bps % 1_000_000) / 100_000),
    );
    let mut summary = format!(
        "{} · {controller} · RTT {} ms (min {}) · loss {loss} ({} lost / {} sent) · {rate} · cwnd {} KB",
        PathGrade::assess(path).label(),
        path.rtt_us / 1000,
        path.min_rtt_us / 1000,
        path.lost_packets,
        path.sent_packets,
        path.cwnd_bytes / 1024,
    );
    if path.mtu_bytes > 0 {
        let _ = write!(summary, " · MTU {}", path.mtu_bytes);
    }
    summary
}

/// Samples a live Quinn connection into one health window's
/// [`arcen_protocol::messages::PathQualityMsg`].
#[cfg(feature = "quic")]
#[derive(Debug, Default)]
pub struct PathQualityTracker {
    signal: PathSignalState,
    udp_tx_bytes: u64,
    black_holes: u64,
    observed_at: Option<std::time::Duration>,
}

#[cfg(feature = "quic")]
impl PathQualityTracker {
    /// Observes `connection` at the caller's monotonic `observed_at`; deltas
    /// cover the time since the previous call (or the connection's start).
    #[must_use]
    pub fn sample(
        &mut self,
        observed_at: std::time::Duration,
        connection: &quinn::Connection,
    ) -> arcen_protocol::messages::PathQualityMsg {
        let stats = connection.stats();
        let udp_tx_bytes = stats.udp_tx.bytes;
        let signal = observe_quinn_path_signal(&mut self.signal, observed_at, connection);
        let window = observed_at.saturating_sub(self.observed_at.unwrap_or_default());
        let wire_bytes = udp_tx_bytes.saturating_sub(self.udp_tx_bytes);
        let black_holes = stats
            .path
            .black_holes_detected
            .saturating_sub(self.black_holes);
        self.udp_tx_bytes = udp_tx_bytes;
        self.black_holes = stats.path.black_holes_detected;
        self.observed_at = Some(observed_at);
        let mut message = path_quality_message(
            signal,
            crate::quic::CongestionControl::from_env().token(),
            wire_bytes,
            window,
        );
        message.mtu_bytes = u64::from(stats.path.current_mtu);
        message.black_holes = black_holes;
        message
    }
}

#[cfg(test)]
mod path_quality_tests {
    use super::{
        PathGrade, PathSignalCounters, PathSignalState, path_quality_message, path_quality_summary,
    };
    use arcen_protocol::messages::PathQualityMsg;
    use std::time::Duration;

    #[test]
    fn path_quality_carries_interval_deltas() {
        let mut state = PathSignalState::default();
        let counters = |rtt_ms, sent, lost| PathSignalCounters {
            rtt: Duration::from_millis(rtt_ms),
            congestion_window_bytes: 64_000,
            bytes_in_flight: None,
            congestion_events: 1,
            lost_packets: lost,
            lost_bytes: lost * 1200,
            sent_packets: sent,
        };
        let _ = state.observe(Duration::from_secs(1), counters(30, 1_000, 2));
        let signal = state.observe(Duration::from_secs(3), counters(45, 3_000, 6));
        let message = path_quality_message(signal, "bbr", 1_000_000, Duration::from_secs(2));
        assert_eq!(message.rtt_us, 45_000);
        assert_eq!(message.min_rtt_us, 30_000);
        assert_eq!(message.sent_packets, 2_000);
        assert_eq!(message.lost_packets, 4);
        assert_eq!(message.congestion_events, 0);
        assert_eq!(message.loss_permille(), Some(2));
        assert_eq!(message.wire_bps(), Some(4_000_000));
        assert_eq!(message.congestion_control_token(), Some("bbr"));
    }

    #[test]
    fn path_grade_and_summary_describe_the_measured_path() {
        let calm = PathQualityMsg {
            congestion_control: "bbr".to_string(),
            rtt_us: 42_400,
            min_rtt_us: 38_100,
            cwnd_bytes: 122_880,
            sent_packets: 2_000,
            lost_packets: 2,
            congestion_events: 0,
            wire_bytes: 2_450_000,
            window_ms: 2_000,
            mtu_bytes: 0,
            black_holes: 0,
        };
        assert_eq!(PathGrade::assess(&calm), PathGrade::Good);
        assert_eq!(
            path_quality_summary(&calm),
            "good · BBR · RTT 42 ms (min 38) · loss 0.1% (2 lost / 2000 sent) · 9.8 Mbit/s · cwnd 120 KB"
        );
        let queued = PathQualityMsg {
            rtt_us: 75_000,
            min_rtt_us: 38_000,
            ..calm.clone()
        };
        assert_eq!(PathGrade::assess(&queued), PathGrade::Fair);
        let lossy = PathQualityMsg {
            lost_packets: 60,
            ..calm.clone()
        };
        assert_eq!(PathGrade::assess(&lossy), PathGrade::Poor);
        let unknown = PathQualityMsg {
            congestion_control: String::new(),
            sent_packets: 0,
            window_ms: 0,
            ..calm
        };
        assert!(path_quality_summary(&unknown).starts_with("good · unknown · "));
        assert!(path_quality_summary(&unknown).contains("loss - (2 lost / 0 sent) · - ·"));
        let probed = PathQualityMsg {
            mtu_bytes: 1_252,
            ..unknown
        };
        assert!(path_quality_summary(&probed).ends_with(" · MTU 1252"));
    }
}
