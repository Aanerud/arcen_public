//! Concrete QUIC transport adapter (Quinn 0.11 + rustls 0.23, standardized
//! QUIC transport crypto only — no custom UDP encryption).
//!
//! Callers own the actual `quinn::Endpoint`, `quinn::ServerConfig`, and
//! `quinn::ClientConfig` (and therefore every rustls certificate, key, and
//! verifier decision). This module performs, on top of an established
//! `quinn::Connection`:
//!
//! - an authenticated binding handshake (see [`handshake`]) that agrees on a
//!   bound session identifier, remote [`QuicRole`], grant-bound peer identity,
//!   and bounded capability intersection before any application payload flows,
//!   mutually gated by a required [`PeerIdentityAuthorizer`];
//! - a persistent unidirectional reliable stream per direction carrying
//!   framed control/reliable-media/input messages (see [`framing`]);
//! - RFC 9221 encrypted datagrams for `MediaLowLatency` traffic only, capped
//!   by both a conservative configurable payload cap and Quinn's dynamic
//!   `max_datagram_size`;
//! - bounded outbound/inbound/event queues with explicit failures, never
//!   silent drops, for reliable/control traffic;
//! - a lifecycle/feedback event stream (see [`feedback`]) built on Quinn's
//!   connection statistics.
//!
//! No 0-RTT, no adaptive FEC, and no custom encryption are implemented here.
//! Certificate issuance, rotation, and revocation remain caller/product
//! concerns.

mod carrier_bench;
mod config;
mod direct;
mod error;
mod feedback;
mod framing;
mod handshake;
mod identity;
mod monitor;
mod monitor_registry;
mod peer;

pub use carrier_bench::{
    ALLOWED_MONITOR_COUNTS, ActivePattern, AggregateMetrics, BENCH_COMPLETION_DRAIN_ALLOWANCE,
    BENCH_TICK_INTERVAL, BenchConfig, BenchConfigError, BenchFrame, BenchRunError, CarrierKind,
    CarrierRunResult, CarrierSelection, CliArgError, ComparisonResult, FrameCodecError,
    MAX_BENCH_DURATION, MAX_BENCH_FRAMES, MAX_BENCH_PAYLOAD_BYTES, MAX_BENCH_RECEIVER_DELAY_FLOOR,
    MAX_BENCH_TOTAL_BYTES, MAX_RECEIVER_DELAY, MIN_ASSUMED_TRANSFER_BYTES_PER_SEC,
    MIN_BENCH_DURATION, MIN_BENCH_FRAMES, MIN_BENCH_PAYLOAD_BYTES, PerMonitorMetrics,
    PerMonitorValidator, SchedulerError, WeightedRoundRobinScheduler, Workload,
    carrier_bench_live_task_count, deterministic_payload, encoded_frame_bytes,
    jains_fairness_index, max_per_monitor_offered_frame_count, offered_frame_count, parse_cli_args,
    percentile, predicted_completion_deadline, produces_at_tick, resolve_tick_count, run_carrier_a,
    run_carrier_b, scheduler_weight_for, tick_pacing_for,
};
pub use config::{
    apply_direct_server_limits, apply_migration_stub_server_limits,
    monitor_carrier_transport_config, monitor_carrier_transport_config_arc,
    recommended_transport_config, recommended_transport_config_arc,
};
pub use direct::{DirectQuicDialParams, DirectQuicStream, accept_direct, connect_direct};
pub use error::QuicTransportError;
pub use feedback::{DatagramDropReason, FeedbackSnapshot};
pub use framing::{DatagramSequenceGuard, SequenceDecision};
pub use handshake::{
    HANDSHAKE_PROTOCOL_VERSION, HandshakeRejectReason, HandshakeRequest, HandshakeResponse,
    MAX_HANDSHAKE_FRAME_BYTES, MAX_SESSION_ID_BYTES,
};
pub use identity::{
    IdentityAuthorizationError, PeerIdentityAuthorizer, PeerIdentityClaim, QuicRole,
};
pub use monitor::{
    MAX_MONITOR_STREAM_PREFACE_BYTES, MAX_MONITOR_STREAM_SESSION_ID_BYTES,
    MAX_MONITOR_STREAMS_PER_CONNECTION, MEDIA_PLAN_FINGERPRINT_BYTES, MONITOR_STREAM_PREFACE_MAGIC,
    MONITOR_STREAM_PREFACE_VERSION, MonitorStreamIdentity, MonitorStreamPrefaceError,
    accept_monitor_stream, open_monitor_stream,
};
pub use monitor_registry::{ExpectedMonitorStream, MonitorRosterError, MonitorStreamRoster};
pub use peer::{AsyncTransportPeer, BoxFuture, QuicPeer, QuicPeerCounters, QuicRuntimeConfig};

use std::net::SocketAddr;

use arcen_identity::ConsumedSessionGrant;

use crate::{
    AuthenticatedPeer, AuthorizationState, CAPABILITY_RELIABLE_STREAM, CAPABILITY_TRANSPORT_QUIC,
    CapabilityId, NegotiatedCapabilities, TransportContractError, TransportProfile,
    validate_negotiated_capabilities, validate_peer_grant_binding,
};

/// ALPN for the direct Pier/Deck QUIC carrier.
pub const DIRECT_QUIC_ALPN_PROTOCOL: &[u8] = b"arcen-quic-v1";

/// One fresh, non-cloneable QUIC connection admission.
///
/// Construction consumes replay-protected grant evidence and records both
/// local and expected remote authenticated identities. A replacement
/// connection therefore requires a new value backed by a freshly consumed
/// grant nonce.
pub struct QuicAdmission {
    pub(crate) consumed_grant: ConsumedSessionGrant,
    pub(crate) local_peer: AuthenticatedPeer,
    pub(crate) expected_remote_peer: AuthenticatedPeer,
    pub(crate) supported_capabilities: NegotiatedCapabilities,
    pub(crate) required_capabilities: NegotiatedCapabilities,
    pub(crate) authorization: AuthorizationState,
    pub(crate) now_epoch_seconds: u64,
}

impl std::fmt::Debug for QuicAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuicAdmission")
            .field("session_id", &self.session_id())
            .field("local_peer", &self.local_peer)
            .field("expected_remote_peer", &self.expected_remote_peer)
            .field("supported_capabilities", &self.supported_capabilities)
            .field("required_capabilities", &self.required_capabilities)
            .field("authorization", &self.authorization)
            .field("now_epoch_seconds", &self.now_epoch_seconds)
            .finish_non_exhaustive()
    }
}

impl QuicAdmission {
    /// Creates a fail-closed QUIC admission.
    ///
    /// # Errors
    ///
    /// Returns an error unless identities match the consumed grant,
    /// authorization is explicit, and supported capabilities include QUIC,
    /// reliable streams, and every policy requirement.
    pub fn new(
        consumed_grant: ConsumedSessionGrant,
        local_peer: AuthenticatedPeer,
        expected_remote_peer: AuthenticatedPeer,
        supported_capabilities: NegotiatedCapabilities,
        required_capabilities: &NegotiatedCapabilities,
        authorization: AuthorizationState,
        now_epoch_seconds: u64,
    ) -> Result<Self, TransportContractError> {
        if authorization != AuthorizationState::Authorized {
            return Err(TransportContractError::AdmissionNotAuthorized);
        }
        let grant = consumed_grant.validated();
        if grant.expires_at() <= now_epoch_seconds {
            return Err(TransportContractError::GrantExpired);
        }
        validate_peer_grant_binding(&local_peer, grant)?;
        validate_peer_grant_binding(&expected_remote_peer, grant)?;

        let required_capabilities =
            NegotiatedCapabilities::new(required_capabilities.iter().cloned().chain([
                CapabilityId::new(CAPABILITY_TRANSPORT_QUIC)?,
                CapabilityId::new(CAPABILITY_RELIABLE_STREAM)?,
            ]))?;
        validate_negotiated_capabilities(
            TransportProfile::Quic,
            &supported_capabilities,
            &required_capabilities,
        )?;

        Ok(Self {
            consumed_grant,
            local_peer,
            expected_remote_peer,
            supported_capabilities,
            required_capabilities,
            authorization,
            now_epoch_seconds,
        })
    }

    /// Returns the grant-bound active-session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        self.consumed_grant.validated().session_id()
    }

    /// Returns the local authenticated identity claim.
    #[must_use]
    pub const fn local_peer(&self) -> &AuthenticatedPeer {
        &self.local_peer
    }

    /// Returns the expected remote authenticated identity.
    #[must_use]
    pub const fn expected_remote_peer(&self) -> &AuthenticatedPeer {
        &self.expected_remote_peer
    }
}

/// Everything needed to dial a peer as the QUIC-level connection initiator.
/// Bundled into one struct so [`connect`] and [`reconnect`] stay within
/// Clippy's argument-count guidance.
pub struct QuicDialParams<'a> {
    /// Caller-owned, already-bound QUIC endpoint.
    pub endpoint: &'a quinn::Endpoint,
    /// Caller-supplied client configuration (rustls verifier, ALPN, etc.).
    pub client_config: quinn::ClientConfig,
    /// Remote socket address to dial.
    pub remote_addr: SocketAddr,
    /// TLS server name to validate against the remote certificate.
    pub server_name: &'a str,
    /// Fresh replay-consumed trust evidence for this connection.
    pub admission: QuicAdmission,
    /// Runtime tuning and required authorization hook.
    pub runtime: QuicRuntimeConfig,
}

impl std::fmt::Debug for QuicDialParams<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuicDialParams")
            .field("remote_addr", &self.remote_addr)
            .field("server_name", &self.server_name)
            .field("local_role", &self.admission.local_peer.role())
            .field("remote_role", &self.admission.expected_remote_peer.role())
            .field("session_id", &self.admission.session_id())
            .field("runtime", &self.runtime)
            .finish_non_exhaustive()
    }
}

/// Dials `remote_addr` as the QUIC-level connection initiator using the
/// caller-supplied `quinn::Endpoint`/`quinn::ClientConfig`, then performs the
/// authenticated admission and capability handshake.
///
/// # Errors
///
/// Returns a connect, handshake, or authorization failure.
pub async fn connect(params: QuicDialParams<'_>) -> Result<QuicPeer, QuicTransportError> {
    let connecting = params
        .endpoint
        .connect_with(params.client_config, params.remote_addr, params.server_name)
        .map_err(QuicTransportError::Connect)?;
    let connection = connecting.await.map_err(QuicTransportError::Connection)?;
    QuicPeer::establish_initiator(connection, params.admission, params.runtime).await
}

/// Completes the QUIC-level accept side of an already-accepted connection
/// (typically produced by driving the caller's `quinn::Endpoint::accept()`
/// loop), then performs the authenticated binding handshake.
///
/// # Errors
///
/// Returns a handshake or authorization failure.
pub async fn accept(
    connection: quinn::Connection,
    admission: QuicAdmission,
    runtime: QuicRuntimeConfig,
) -> Result<QuicPeer, QuicTransportError> {
    QuicPeer::establish_acceptor(connection, admission, runtime).await
}

/// Reconnects by dialing a fresh QUIC connection, explicitly identifying
/// both the previous and the new connection: the previous peer is told a
/// reconnect is starting ([`crate::TransportEvent::Reconnecting`]), and the
/// new peer records that it is the replacement
/// ([`crate::TransportEvent::Reconnected`]). Quinn manages any in-connection
/// path migration on its own; this function only handles the case where a
/// caller must establish a brand new connection (for example, after total
/// connection loss).
///
/// # Errors
///
/// Returns a connect, handshake, or authorization failure. The previous peer
/// is left untouched on failure.
pub async fn reconnect(
    previous: &QuicPeer,
    params: QuicDialParams<'_>,
) -> Result<QuicPeer, QuicTransportError> {
    previous.note_reconnecting();
    let peer = connect(params).await?;
    peer.note_reconnected();
    Ok(peer)
}
