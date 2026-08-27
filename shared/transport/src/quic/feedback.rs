//! Congestion/feedback snapshot derived from Quinn connection statistics.

use std::time::Duration;

/// Point-in-time congestion and path feedback snapshot, derived from
/// `quinn::Connection::stats().path`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FeedbackSnapshot {
    /// Current best-estimate round-trip time.
    pub rtt: Duration,
    /// Current congestion window in bytes.
    pub congestion_window: u64,
    /// Total congestion events observed on this path.
    pub congestion_events: u64,
    /// Total packets lost on this path.
    pub lost_packets: u64,
    /// Total bytes lost on this path.
    pub lost_bytes: u64,
    /// Total packets sent on this path.
    pub sent_packets: u64,
    /// Largest UDP payload size the path currently supports.
    pub current_mtu: u16,
    /// Number of times a black hole (silent connectivity loss) was detected.
    pub black_holes_detected: u64,
}

impl FeedbackSnapshot {
    /// Builds a snapshot from a live Quinn connection's current path stats.
    #[must_use]
    pub fn from_connection(connection: &quinn::Connection) -> Self {
        let stats = connection.stats();
        Self {
            rtt: stats.path.rtt,
            congestion_window: stats.path.cwnd,
            congestion_events: stats.path.congestion_events,
            lost_packets: stats.path.lost_packets,
            lost_bytes: stats.path.lost_bytes,
            sent_packets: stats.path.sent_packets,
            current_mtu: stats.path.current_mtu,
            black_holes_detected: stats.path.black_holes_detected,
        }
    }
}

/// Explicit, non-silent reason a best-effort encrypted datagram was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DatagramDropReason {
    /// The datagram did not contain a valid Arcen low-latency media frame.
    MalformedFrame,
    /// The payload exceeded the configured contract-level datagram cap.
    ExceedsConfiguredCap,
    /// Authenticated admission, capability, or envelope metadata was rejected.
    AdmissionRejected,
    /// The payload exceeded the connection's current dynamic
    /// `max_datagram_size` (path MTU derived).
    ExceedsDynamicPathLimit,
    /// The peer does not support receiving datagrams on this connection.
    UnsupportedByPeer,
    /// Datagram support is disabled locally for this connection.
    DisabledLocally,
    /// Quinn's internal datagram send buffer had no space available
    /// (proactive send-side backpressure check before attempting the send).
    SendBufferFull,
    /// The bounded inbound application queue had no remaining message or byte capacity.
    InboundQueueFull,
    /// The datagram repeated a sequence number already accepted.
    Duplicate,
    /// The datagram arrived outside the configured lateness window.
    Late,
    /// The underlying connection rejected the send (for example, closing).
    ConnectionRejected,
}
