//! Transport-config helpers and connection parameter bundles.
//!
//! Callers remain in control of the actual `quinn::ServerConfig` /
//! `quinn::ClientConfig` (and therefore of the underlying rustls
//! `ServerConfig`/`ClientConfig`, certificate chains, and verifiers). This
//! module only offers a recommended `quinn::TransportConfig` helper that
//! configures the stream/datagram limits this adapter requires; callers
//! apply it to their own configs.

use std::sync::Arc;
use std::time::Duration;

use quinn::{AckFrequencyConfig, VarInt};

use crate::BoundedTransportPolicy;

use super::monitor::MAX_MONITOR_STREAMS_PER_CONNECTION;

// ---------------------------------------------------------------------------
// Tunable constants — all values here are starting points; operators and
// integration tests should validate against their own network profiles and
// adjust via the per-endpoint override hooks before locking in production.
// ---------------------------------------------------------------------------

/// Conservative initial MTU for Arcen QUIC endpoints.
///
/// RFC 9312 §4.10 notes that networks often prefer dropping oversize packets
/// over performing lower-layer fragmentation; fragmentation is a bottleneck
/// when it occurs silently. 1200 bytes is the RFC 9000 minimum MTU and works
/// across virtually all production paths including NAT-heavy enterprise and
/// mobile networks. The compatibility profile keeps this fixed rather than
/// probing above the path's known-safe floor.
///
/// **Do not raise without confirming your production network path.**
pub(super) const ARCEN_QUIC_INITIAL_MTU: u16 = 1200;

/// Minimum MTU floor used by Arcen QUIC endpoints.
///
/// Matches `ARCEN_QUIC_INITIAL_MTU`.
pub(super) const ARCEN_QUIC_MIN_MTU: u16 = 1200;

/// Idle timeout (milliseconds). 30 s is the Quinn default and matches common
/// UDP NAT binding lifetimes (enterprise NAT floor ~30 s). Range: 20–60 s.
pub(super) const ARCEN_QUIC_IDLE_TIMEOUT_MS: u32 = 30_000;

/// Keep-alive interval for attached/active sessions.
///
/// Sent while a session is actively attached to keep NAT mappings alive.
/// Must be shorter than `ARCEN_QUIC_IDLE_TIMEOUT_MS`. Range: 15–30 s.
pub(super) const ARCEN_QUIC_KEEPALIVE_SECS: u64 = 20;

/// Per-stream flow-control receive window (1 MiB).
///
/// Sufficient for a single burst of compressed video frames or a batch of
/// control messages. Range: 512 KiB – 2 MiB.
pub(super) const ARCEN_QUIC_STREAM_RECV_WINDOW: u32 = 1 << 20;

/// Connection-level flow-control receive window (16 MiB).
///
/// Sized for concurrent control + media streams at typical remote-desktop
/// bitrates. Range: 8 – 64 MiB.
pub(super) const ARCEN_QUIC_CONN_RECV_WINDOW: u32 = 16 << 20;

/// Connection-level send window (16 MiB).
///
/// Symmetric with `ARCEN_QUIC_CONN_RECV_WINDOW`. Range: 8 – 64 MiB.
pub(super) const ARCEN_QUIC_CONN_SEND_WINDOW: u64 = (16 << 20) as u64;

/// Datagram send/receive buffer size (2 MiB).
///
/// Floor for H.264/audio datagram burst buffering. The actual value passed to
/// `recommended_transport_config` is `max(policy-computed, this constant)`.
/// Range: 1 – 8 MiB.
pub(super) const ARCEN_QUIC_DATAGRAM_BUFFER_BYTES: usize = 2 * 1024 * 1024;

/// Maximum pending unauthenticated direct QUIC attempts.
pub(super) const ARCEN_QUIC_DIRECT_MAX_INCOMING: usize = 64;
/// Per-attempt pre-accept buffering before product authentication.
pub(super) const ARCEN_QUIC_DIRECT_INCOMING_BUFFER_BYTES: u64 = 16 * 1024;
/// Aggregate pre-accept buffering for all unauthenticated attempts.
pub(super) const ARCEN_QUIC_DIRECT_INCOMING_BUFFER_TOTAL_BYTES: u64 = 512 * 1024;

/// Builds a `quinn::TransportConfig` compatible with this adapter's stream
/// and datagram usage.
///
/// ## Configured parameters
///
/// | Parameter | Value | Notes |
/// |-----------|-------|-------|
/// | Congestion | CUBIC (Quinn default) | Baseline; tune to BBR after profiling |
/// | Concurrent bidi streams | 1 | Direct product carrier or advanced handshake |
/// | Concurrent uni streams | 1 | Advanced adapter's single persistent stream |
/// | Idle timeout | 30 s | Matches NAT floor; tune 20–60 s |
/// | Keep-alive | 20 s | Must be < idle timeout; tune 15–30 s |
/// | Initial MTU | 1200 | RFC 9000 minimum |
/// | Min MTU | 1200 | Floor; never fragment below |
/// | MTU discovery | disabled | Avoid oversize probes on VPN/IPsec paths |
/// | Datagram buffers | max(policy, 2 MiB) send + recv | Prevent media burst drops |
/// | Stream recv window | 1 MiB | Per-stream flow control |
/// | Conn recv/send window | 16 MiB | Connection-level flow control |
/// | Send fairness | true | Prevent per-stream HOL under mux |
/// | Segmentation offload | true | Reduces send-path CPU on supported NICs |
/// | ACK frequency | threshold=0, `max_delay=5ms` | Immediate per-packet ACK on LAN; gives CUBIC finer RTT samples, reducing burst delivery from CWND expansion |
///
/// Callers may further customize the returned config (e.g. swap in BBR,
/// adjust window sizes for BDP, or enable `ack_frequency_config`) before
/// attaching it to their `quinn::ServerConfig`/`quinn::ClientConfig`.
///
/// This is the **live** config used by both the direct product carrier and
/// the advanced `QuicPeer` adapter today; its concurrent unidirectional
/// stream limit stays at 1, matching the advanced adapter's single
/// persistent stream. It is unaffected by the additive direct-monitor
/// stream foundation ("Carrier B", not yet product-selected; see
/// [`super::monitor`]) — see [`monitor_carrier_transport_config`] for the
/// separate, test-only config that raises this limit.
///
/// ## Runtime policy (caller responsibility)
///
/// - If `Connection::max_datagram_size()` returns `None`: disable datagrams
///   for that connection and route all traffic over reliable streams.
/// - On repeated `SendDatagramError::TooLarge`: clamp payload size.
/// - On repeated `UnsupportedByPeer`/`Disabled`: pin to streams-only.
///
/// ## Multiple connections for `QoS`
///
/// QUIC stream IDs are inside encryption; the network cannot differentiate
/// stream classes (RFC 9308 §4.1). If control/input traffic and media frames
/// need different DSCP or network treatment, place them on **separate QUIC
/// connections** — each with its own `TransportConfig` and `QoS` marking.
#[must_use]
pub fn recommended_transport_config(policy: &BoundedTransportPolicy) -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();

    // Stream concurrency: one bidi for either the direct product carrier or
    // the advanced binding handshake, plus the advanced `QuicPeer` adapter's
    // exactly one persistent uni stream per direction. This is the live
    // config; it intentionally does not raise the uni-stream limit for the
    // additive, not-yet-product-selected direct-monitor stream foundation —
    // see `monitor_carrier_transport_config` for that separate, unwired,
    // test-only config.
    config.max_concurrent_bidi_streams(VarInt::from_u32(1));
    config.max_concurrent_uni_streams(VarInt::from_u32(1));

    // Flow-control windows sized for typical remote-desktop BDP.
    config.stream_receive_window(VarInt::from_u32(ARCEN_QUIC_STREAM_RECV_WINDOW));
    config.receive_window(VarInt::from_u32(ARCEN_QUIC_CONN_RECV_WINDOW));
    config.send_window(ARCEN_QUIC_CONN_SEND_WINDOW);

    // Per-stream fairness prevents a high-rate media stream from starving
    // control or input streams under congestion.
    config.send_fairness(true);

    // NAT survival: keep-alive shorter than idle timeout.
    config.keep_alive_interval(Some(Duration::from_secs(ARCEN_QUIC_KEEPALIVE_SECS)));
    // 30_000 ms always fits VarInt, so this never panics.
    config.max_idle_timeout(Some(quinn::IdleTimeout::from(VarInt::from_u32(
        ARCEN_QUIC_IDLE_TIMEOUT_MS,
    ))));

    // Keep the compatibility baseline at the QUIC minimum. The fleet path
    // includes 1280-byte IPsec interfaces, where Quinn's default 1452-byte
    // discovery probe fails locally before black-hole recovery can help.
    config.initial_mtu(ARCEN_QUIC_INITIAL_MTU);
    config.min_mtu(ARCEN_QUIC_MIN_MTU);
    config.mtu_discovery_config(None);

    // Datagram buffers: floor at 2 MiB to absorb H.264/audio frame bursts.
    let datagram_buffer = policy
        .max_datagram_payload_bytes
        .saturating_mul(64)
        .max(policy.max_datagram_payload_bytes)
        .max(ARCEN_QUIC_DATAGRAM_BUFFER_BYTES);
    config.datagram_receive_buffer_size(Some(datagram_buffer));
    config.datagram_send_buffer_size(datagram_buffer);

    // Reduce send-path CPU on NICs with GSO support; no-op on unsupported.
    config.enable_segmentation_offload(true);

    // ACK frequency: request the peer ACK every packet immediately (threshold
    // = 0) with a 5ms max delay. At ~33ms LAN RTT the default 25ms max_ack_delay
    // means the sender's CUBIC pacer operates with coarse ~58ms feedback, which
    // can produce 5–7 frame bursts when the CWND expands after a pause. Tighter
    // ACK cadence gives CUBIC ~38ms feedback, smoothing CWND growth and reducing
    // burst arrivals. Both endpoints must negotiate this extension; Quinn skips
    // the IMMEDIATE_ACK frame if the peer does not advertise support.
    let mut ack_freq = AckFrequencyConfig::default();
    ack_freq
        .ack_eliciting_threshold(VarInt::from_u32(0))
        .max_ack_delay(Some(Duration::from_millis(5)));
    config.ack_frequency_config(Some(ack_freq));

    config
}

/// Wraps a `quinn::TransportConfig` for callers that want the recommended
/// defaults as an `Arc` ready for `ServerConfig::transport_config`/
/// `ClientConfig::transport_config`.
#[must_use]
pub fn recommended_transport_config_arc(
    policy: &BoundedTransportPolicy,
) -> Arc<quinn::TransportConfig> {
    Arc::new(recommended_transport_config(policy))
}

/// Returns [`recommended_transport_config`] with its concurrent
/// unidirectional stream limit raised to the exact
/// [`MAX_MONITOR_STREAMS_PER_CONNECTION`] (4) the additive direct-monitor
/// stream foundation ("Carrier B", not yet product-selected; see
/// [`super::monitor`]) needs for its 1-4 monitor topology bound.
///
/// **This is not part of any live product transport config.** No product
/// server/client construction path calls this function; it exists solely so
/// the `tests/quic_monitor_stream.rs` loopback suite can open up to four
/// concurrent server-to-Deck monitor streams end to end. The direct product
/// carrier and the advanced `QuicPeer` adapter must keep using
/// [`recommended_transport_config`] unchanged, at its existing
/// `max_concurrent_uni_streams = 1`, until a reviewed product decision wires
/// Carrier B into an actual server/client config.
#[must_use]
pub fn monitor_carrier_transport_config(policy: &BoundedTransportPolicy) -> quinn::TransportConfig {
    let mut config = recommended_transport_config(policy);
    let max_uni_streams = u32::try_from(MAX_MONITOR_STREAMS_PER_CONNECTION).unwrap_or(u32::MAX);
    config.max_concurrent_uni_streams(VarInt::from_u32(max_uni_streams));
    config
}

/// `Arc`-wraps [`monitor_carrier_transport_config`] for the same test-only
/// `ServerConfig`/`ClientConfig::transport_config` convenience
/// [`recommended_transport_config_arc`] offers for the live config. See
/// [`monitor_carrier_transport_config`]'s doc for why this must never be
/// wired into product code.
#[must_use]
pub fn monitor_carrier_transport_config_arc(
    policy: &BoundedTransportPolicy,
) -> Arc<quinn::TransportConfig> {
    Arc::new(monitor_carrier_transport_config(policy))
}

/// Applies strict limits before product authentication accepts a direct
/// session.
pub fn apply_direct_server_limits(config: &mut quinn::ServerConfig) {
    config.max_incoming(ARCEN_QUIC_DIRECT_MAX_INCOMING);
    config.incoming_buffer_size(ARCEN_QUIC_DIRECT_INCOMING_BUFFER_BYTES);
    config.incoming_buffer_size_total(ARCEN_QUIC_DIRECT_INCOMING_BUFFER_TOTAL_BYTES);
}

/// Compatibility alias for the refusal-only migration scaffold.
pub fn apply_migration_stub_server_limits(config: &mut quinn::ServerConfig) {
    apply_direct_server_limits(config);
}
