//! Concrete transport adapters for the pure path signal telemetry type.

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
