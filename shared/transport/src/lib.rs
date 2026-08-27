//! Bounded, transport-neutral peer admission and envelope contracts.
//!
//! This crate defines a product-neutral transport contract (payload caps,
//! authenticated admission, reliability classes, bounded queues, metadata-first
//! receive, and lifecycle events). The optional `quic` feature adds the
//! concrete Quinn adapter. Dormant secure-WebSocket compatibility policy is
//! available only through the explicit `wss-compat` feature.
//!
//! No adaptive forward error correction is implemented or claimed anywhere in
//! this crate. See [`FecPolicy`].

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

#[cfg(feature = "wss-compat")]
pub mod fallback;
#[cfg(feature = "quic")]
pub mod quic;
pub mod tls;

use arcen_identity::{
    ActiveHostSessionId, ConsumedSessionGrant, HostIdentity, ValidatedDirectResumeGrant,
    ValidatedSessionGrant,
};
use arcen_protocol::FrameType;
use std::collections::{BTreeSet, VecDeque};
use std::error::Error;
use std::fmt::{Display, Formatter};

/// Maximum negotiated capabilities on one peer.
pub const MAX_NEGOTIATED_CAPABILITIES: usize = 64;
/// Maximum bytes in one capability or peer identity.
pub const MAX_TRANSPORT_IDENTITY_BYTES: usize = 512;
/// Capability identifying the dormant secure-WebSocket compatibility profile.
#[cfg(feature = "wss-compat")]
pub const CAPABILITY_TRANSPORT_WSS: &str = "transport:wss-v1";
/// Capability identifying the concrete QUIC profile.
pub const CAPABILITY_TRANSPORT_QUIC: &str = "transport:quic-v1";
/// Capability identifying ordered reliable-stream delivery.
pub const CAPABILITY_RELIABLE_STREAM: &str = "delivery:reliable-stream-v1";
/// Capability identifying RFC 9221 encrypted-datagram delivery.
#[cfg(feature = "quic")]
pub const CAPABILITY_ENCRYPTED_DATAGRAM: &str = "delivery:encrypted-datagram-v1";

// ---------------------------------------------------------------------------
// Structured-logging field name constants
//
// Use these as field names when emitting `tracing` events from hosts, clients,
// and the shared transport so log collectors can correlate across components.
// ---------------------------------------------------------------------------

/// Structured log target used by all Arcen transport events.
pub const LOG_TARGET_TRANSPORT: &str = "arcen::transport";

/// Field: active transport mode (`"quic_datagrams"` | `"quic_streams"`).
pub const LOG_FIELD_TRANSPORT_MODE: &str = "transport_mode";
/// Field: transport mode before a transition (same values as `LOG_FIELD_TRANSPORT_MODE`).
pub const LOG_FIELD_TRANSPORT_MODE_PREV: &str = "transport_mode_prev";
/// Field: QUIC connection ID (hex string).
pub const LOG_FIELD_QUIC_CONN_ID: &str = "quic_conn_id";
/// Field: remote socket address ("host:port").
pub const LOG_FIELD_REMOTE_ADDR: &str = "remote_addr";
/// Field: round-trip time in milliseconds (u64).
pub const LOG_FIELD_RTT_MS: &str = "rtt_ms";
/// Field: congestion window in bytes (u64).
pub const LOG_FIELD_CWND: &str = "cwnd";
/// Field: cumulative packets lost on this path (u64).
pub const LOG_FIELD_LOST_PACKETS: &str = "lost_packets";
/// Field: cumulative bytes lost on this path (u64).
pub const LOG_FIELD_LOST_BYTES: &str = "lost_bytes";
/// Field: current path MTU in bytes (u16).
pub const LOG_FIELD_CURRENT_MTU: &str = "current_mtu";
/// Field: number of black-hole detections on this path (u64).
pub const LOG_FIELD_BLACK_HOLES: &str = "black_holes";
/// Field: reason a datagram was dropped (string).
pub const LOG_FIELD_DATAGRAM_DROP_REASON: &str = "datagram_drop_reason";
/// Field: `FailureKind` that triggered a circuit-breaker transition (string).
pub const LOG_FIELD_FAILURE_KIND: &str = "failure_kind";
/// Field: bool; whether this connection uses datagrams for media (bool).
pub const LOG_FIELD_DATAGRAMS_ACTIVE: &str = "datagrams_active";

/// Transport profile availability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TransportProfile {
    /// Dormant secure-WebSocket compatibility profile.
    #[cfg(feature = "wss-compat")]
    WebSocketSecure,
    /// Standard QUIC reliable streams plus encrypted datagrams.
    Quic,
}

/// Semantic protocol message class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MessageClass {
    /// Control and lifecycle messages.
    Control,
    /// Encoded media frames.
    Media,
    /// User input events.
    Input,
}

/// Payload reliability and latency class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReliabilityClass {
    /// Ordered, reliable control messages.
    Control,
    /// Ordered, reliable media that cannot be discarded.
    MediaReliable,
    /// Latency-sensitive media eligible for the encrypted datagram path.
    MediaLowLatency,
    /// Latency-sensitive input, kept reliable unless a reviewed profile says otherwise.
    InputLowLatency,
}

/// Delivery mechanism selected by a concrete adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryMechanism {
    /// Ordered reliable stream, used by WSS and QUIC persistent streams.
    ReliableStream,
    /// Encrypted QUIC datagram (RFC 9221), available only for `MediaLowLatency`.
    #[cfg(feature = "quic")]
    EncryptedDatagram,
}

/// Maps a wire-level binary frame type byte to its [`ReliabilityClass`].
///
/// Byte values are interpreted via `arcen_protocol::FrameType` so this mapping
/// stays in lockstep with the wire contract.
///
/// | Byte(s)     | Frame kind              | Class             |
/// |-------------|-------------------------|-------------------|
/// | 0x01–0x05   | Video (full/partial/codec) | `MediaLowLatency` |
/// | 0x10        | Audio (downstream)      | `MediaLowLatency` |
/// | 0x11        | `AudioUpstream` (mic)   | `MediaLowLatency` |
/// | 0x20        | Clipboard               | `Control`         |
/// | 0x30        | HID device added        | `Control`         |
/// | 0x31        | HID device removed      | `Control`         |
/// | 0x32        | HID report              | `InputLowLatency` |
/// | _other_     | Unknown/control JSON    | `Control`         |
///
/// JSON/text control messages are not binary-framed with a type byte; the
/// caller is responsible for routing them to [`ReliabilityClass::Control`]
/// directly.
#[must_use]
pub fn reliability_class_for_frame_byte(frame_type_byte: u8) -> ReliabilityClass {
    match FrameType::try_from(frame_type_byte) {
        Ok(
            FrameType::VideoFull
            | FrameType::VideoPartial
            | FrameType::VideoH264
            | FrameType::VideoH265
            | FrameType::VideoAv1
            | FrameType::RegionVideoFull
            | FrameType::RegionVideoH264
            | FrameType::RegionVideoH265
            | FrameType::RegionVideoAv1
            | FrameType::Audio
            | FrameType::AudioUpstream,
        ) => ReliabilityClass::MediaLowLatency,
        Ok(
            FrameType::Clipboard
            | FrameType::HidDeviceAdded
            | FrameType::HidDeviceRemoved
            | FrameType::UsbBridgeUrbSubmit
            | FrameType::UsbBridgeUrbCancel,
        )
        | Err(_) => ReliabilityClass::Control,
        Ok(FrameType::HidReport | FrameType::UsbBridgeUrbComplete) => {
            ReliabilityClass::InputLowLatency
        }
    }
}

/// Selects the [`DeliveryMechanism`] for a given [`ReliabilityClass`] and
/// active [`TransportProfile`].
///
/// `datagram_available` must reflect a runtime check against
/// `quinn::Connection::max_datagram_size().is_some()` before calling on the
/// QUIC path; pass `false` to force stream delivery.
///
/// Routing table:
///
/// | Class                | Profile::Quic + datagrams | Profile::Quic no datagrams | WSS |
/// |----------------------|---------------------------|----------------------------|-----|
/// | `Control`            | `ReliableStream`          | `ReliableStream`           | `ReliableStream` |
/// | `MediaReliable`      | `ReliableStream`          | `ReliableStream`           | `ReliableStream` |
/// | `MediaLowLatency`    | `EncryptedDatagram`       | `ReliableStream`           | `ReliableStream` |
/// | `InputLowLatency`    | `ReliableStream`          | `ReliableStream`           | `ReliableStream` |
#[must_use]
pub fn delivery_mechanism_for(
    reliability_class: ReliabilityClass,
    profile: TransportProfile,
    datagram_available: bool,
    payload_len_bytes: usize,
    max_datagram_payload_bytes: usize,
    connection_max_datagram_size_bytes: Option<usize>,
    framing_overhead_bytes: usize,
) -> DeliveryMechanism {
    #[cfg(feature = "quic")]
    if profile == TransportProfile::Quic
        && datagram_available
        && reliability_class == ReliabilityClass::MediaLowLatency
        && payload_len_bytes
            .checked_add(framing_overhead_bytes)
            .is_some_and(|total| {
                total <= max_datagram_payload_bytes
                    && connection_max_datagram_size_bytes
                        .is_some_and(|live_limit| total <= live_limit)
            })
    {
        return DeliveryMechanism::EncryptedDatagram;
    }
    // Suppress unused-variable warning when quic feature is off
    let _ = (
        profile,
        datagram_available,
        reliability_class,
        payload_len_bytes,
        max_datagram_payload_bytes,
        connection_max_datagram_size_bytes,
        framing_overhead_bytes,
    );
    DeliveryMechanism::ReliableStream
}

/// Authenticated transport peer role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    /// Interactive client.
    Client,
    /// Target Host.
    Host,
    /// Gateway relay or service peer.
    Gateway,
}

/// Explicit transport admission authorization state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationState {
    /// Required authorization inputs are incomplete.
    Pending,
    /// Policy authorized admission using all retained evidence.
    Authorized,
    /// Policy explicitly denied admission.
    Denied,
}

/// Opaque connection identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(String);

impl ConnectionId {
    /// Creates an identifier from an authoritative transport value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Direct-session replacement-connection binding, distinct from Gateway admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectResumeTransportBinding {
    connection_id: ConnectionId,
    host_identity: HostIdentity,
    active_session_id: ActiveHostSessionId,
}

impl DirectResumeTransportBinding {
    /// Binds a direct session connection to stable Host and active-session evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when any transport-visible identity is malformed.
    pub fn new(
        connection_id: ConnectionId,
        host_identity: HostIdentity,
        active_session_id: ActiveHostSessionId,
    ) -> Result<Self, TransportContractError> {
        validate_transport_identity(connection_id.as_str())?;
        validate_transport_identity(host_identity.as_str())?;
        validate_transport_identity(active_session_id.as_str())?;
        Ok(Self {
            connection_id,
            host_identity,
            active_session_id,
        })
    }

    /// Creates binding evidence from a fully validated direct-resume grant.
    ///
    /// # Errors
    ///
    /// Returns an error when a transport-visible identity is malformed.
    pub fn from_validated_grant(
        connection_id: ConnectionId,
        grant: &ValidatedDirectResumeGrant,
    ) -> Result<Self, TransportContractError> {
        Self::new(
            connection_id,
            grant.claims().host_identity().clone(),
            grant.claims().active_session_id().clone(),
        )
    }

    /// Checks that validated grant evidence belongs to this Host session.
    ///
    /// # Errors
    ///
    /// Returns a precise Host or active-session topology mismatch.
    pub fn validate_grant(
        &self,
        grant: &ValidatedDirectResumeGrant,
    ) -> Result<(), TransportContractError> {
        if self.host_identity != *grant.claims().host_identity() {
            return Err(TransportContractError::HostEvidenceMismatch);
        }
        if self.active_session_id != *grant.claims().active_session_id() {
            return Err(TransportContractError::SessionEvidenceMismatch);
        }
        Ok(())
    }

    /// Returns the replacement connection.
    #[must_use]
    pub const fn connection_id(&self) -> &ConnectionId {
        &self.connection_id
    }

    /// Returns stable target Host evidence.
    #[must_use]
    pub const fn host_identity(&self) -> &HostIdentity {
        &self.host_identity
    }

    /// Returns active host-session evidence.
    #[must_use]
    pub const fn active_session_id(&self) -> &ActiveHostSessionId {
        &self.active_session_id
    }
}

/// Backward-compatible alias for the former WSS-specific binding name.
pub type DirectWssResumeBinding = DirectResumeTransportBinding;

/// One negotiated capability identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CapabilityId(String);

impl CapabilityId {
    /// Creates a bounded capability identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is empty or exceeds the shared bound.
    pub fn new(value: impl Into<String>) -> Result<Self, TransportContractError> {
        let value = value.into();
        validate_transport_identity(&value)?;
        Ok(Self(value))
    }

    /// Returns the capability identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Bounded set of capabilities selected during negotiation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NegotiatedCapabilities(BTreeSet<CapabilityId>);

impl NegotiatedCapabilities {
    /// Creates a bounded capability set.
    ///
    /// # Errors
    ///
    /// Returns an error when the set exceeds the shared capability count.
    pub fn new(
        capabilities: impl IntoIterator<Item = CapabilityId>,
    ) -> Result<Self, TransportContractError> {
        let capabilities = capabilities.into_iter().collect::<BTreeSet<_>>();
        if capabilities.len() > MAX_NEGOTIATED_CAPABILITIES {
            return Err(TransportContractError::CapabilityLimit);
        }
        Ok(Self(capabilities))
    }

    /// Returns whether a capability was negotiated.
    #[must_use]
    pub fn contains(&self, capability: &CapabilityId) -> bool {
        self.0.contains(capability)
    }

    /// Returns whether a capability name was negotiated.
    #[must_use]
    pub fn contains_name(&self, capability: &str) -> bool {
        self.0
            .iter()
            .any(|candidate| candidate.as_str() == capability)
    }

    /// Iterates negotiated capabilities.
    pub fn iter(&self) -> impl Iterator<Item = &CapabilityId> {
        self.0.iter()
    }

    /// Returns the bounded intersection of two advertised capability sets.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).cloned().collect())
    }

    /// Returns whether every required capability is present.
    #[must_use]
    pub fn supports_all(&self, required: &Self) -> bool {
        required.0.is_subset(&self.0)
    }
}

/// Selects a mutually supported concrete profile, preferring QUIC when policy
/// does not require one profile explicitly.
///
/// # Errors
///
/// Returns an error when the required profile is unavailable or the peers have
/// no profile overlap. Callers must not silently downgrade in either case.
pub fn select_transport_profile(
    local: &[TransportProfile],
    remote: &[TransportProfile],
    required: Option<TransportProfile>,
) -> Result<TransportProfile, TransportContractError> {
    let shared = |profile| local.contains(&profile) && remote.contains(&profile);
    if let Some(required) = required {
        return shared(required)
            .then_some(required)
            .ok_or(TransportContractError::TransportProfileMismatch);
    }
    if shared(TransportProfile::Quic) {
        return Ok(TransportProfile::Quic);
    }
    #[cfg(feature = "wss-compat")]
    if shared(TransportProfile::WebSocketSecure) {
        return Ok(TransportProfile::WebSocketSecure);
    }
    Err(TransportContractError::TransportProfileMismatch)
}

/// Validates the negotiated capabilities for a selected profile and policy.
///
/// # Errors
///
/// Returns an error when profile-mandatory or policy-required capabilities are
/// absent. This is a fail-closed downgrade boundary.
pub fn validate_negotiated_capabilities(
    profile: TransportProfile,
    negotiated: &NegotiatedCapabilities,
    required: &NegotiatedCapabilities,
) -> Result<(), TransportContractError> {
    let profile_capability = match profile {
        #[cfg(feature = "wss-compat")]
        TransportProfile::WebSocketSecure => CAPABILITY_TRANSPORT_WSS,
        TransportProfile::Quic => CAPABILITY_TRANSPORT_QUIC,
    };
    if !negotiated.contains_name(profile_capability)
        || !negotiated.contains_name(CAPABILITY_RELIABLE_STREAM)
        || !negotiated.supports_all(required)
    {
        return Err(TransportContractError::CapabilityMismatch);
    }
    Ok(())
}

/// Validates that an envelope's selected delivery was actually negotiated.
///
/// # Errors
///
/// Returns an error when the selected profile or delivery capability is absent.
pub fn validate_envelope_capabilities(
    profile: TransportProfile,
    negotiated: &NegotiatedCapabilities,
    metadata: &EnvelopeMetadata,
) -> Result<(), TransportContractError> {
    let empty = NegotiatedCapabilities(BTreeSet::new());
    validate_negotiated_capabilities(profile, negotiated, &empty)?;
    #[cfg(feature = "quic")]
    {
        if metadata.delivery == DeliveryMechanism::EncryptedDatagram
            && !negotiated.contains_name(CAPABILITY_ENCRYPTED_DATAGRAM)
        {
            return Err(TransportContractError::CapabilityMismatch);
        }
    }
    #[cfg(not(feature = "quic"))]
    let _ = metadata;
    Ok(())
}

/// Authenticated peer evidence retained independently from grant evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticatedPeer {
    role: PeerRole,
    identity: String,
}

impl AuthenticatedPeer {
    /// Creates bounded authenticated peer evidence.
    ///
    /// # Errors
    ///
    /// Returns an error when the identity is empty or exceeds the shared bound.
    pub fn new(
        role: PeerRole,
        identity: impl Into<String>,
    ) -> Result<Self, TransportContractError> {
        let identity = identity.into();
        validate_transport_identity(&identity)?;
        Ok(Self { role, identity })
    }

    /// Returns the authenticated role.
    #[must_use]
    pub const fn role(&self) -> PeerRole {
        self.role
    }

    /// Returns the authenticated peer identity.
    #[must_use]
    pub fn identity(&self) -> &str {
        &self.identity
    }
}

/// Validated session-grant identity and binding retained for admission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantBindingEvidence {
    version: u16,
    subject: String,
    tenant: Option<String>,
    client_identity: String,
    host_identity: String,
    session_id: String,
    nonce: String,
    expires_at: u64,
}

impl From<&ValidatedSessionGrant> for GrantBindingEvidence {
    fn from(grant: &ValidatedSessionGrant) -> Self {
        Self {
            version: grant.version(),
            subject: grant.subject().to_owned(),
            tenant: grant.tenant().map(str::to_owned),
            client_identity: grant.client_identity().as_str().to_owned(),
            host_identity: grant.host_identity().as_str().to_owned(),
            session_id: grant.session_id().to_owned(),
            nonce: grant.nonce().as_str().to_owned(),
            expires_at: grant.expires_at(),
        }
    }
}

impl GrantBindingEvidence {
    /// Returns the claims schema version.
    #[must_use]
    pub const fn version(&self) -> u16 {
        self.version
    }

    /// Returns the human subject.
    #[must_use]
    pub fn subject(&self) -> &str {
        &self.subject
    }

    /// Returns the tenant.
    #[must_use]
    pub fn tenant(&self) -> Option<&str> {
        self.tenant.as_deref()
    }

    /// Returns the client identity.
    #[must_use]
    pub fn client_identity(&self) -> &str {
        &self.client_identity
    }

    /// Returns the target Host identity.
    #[must_use]
    pub fn host_identity(&self) -> &str {
        &self.host_identity
    }

    /// Returns the active-session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the consumed grant nonce.
    #[must_use]
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    /// Returns the grant expiry.
    #[must_use]
    pub const fn expires_at(&self) -> u64 {
        self.expires_at
    }
}

/// Connection-to-session admission evidence.
#[derive(Debug, PartialEq, Eq)]
pub struct SessionBinding {
    connection_id: ConnectionId,
    session_id: String,
    peer: AuthenticatedPeer,
    grant: GrantBindingEvidence,
    capabilities: NegotiatedCapabilities,
    authorization: AuthorizationState,
}

impl SessionBinding {
    /// Creates admission evidence from a validated grant and authenticated peer.
    ///
    /// Construction validates the grant/session and role identity binding, but
    /// callers must still require `Authorized` through `validate_admission`.
    ///
    /// # Errors
    ///
    /// Returns an error for mismatched, expired, or malformed evidence.
    pub fn new(
        connection_id: ConnectionId,
        session_id: impl Into<String>,
        peer: AuthenticatedPeer,
        consumed_grant: ConsumedSessionGrant,
        capabilities: NegotiatedCapabilities,
        authorization: AuthorizationState,
        now_epoch_seconds: u64,
    ) -> Result<Self, TransportContractError> {
        let session_id = session_id.into();
        let grant = consumed_grant.into_validated();
        validate_transport_identity(&session_id)?;
        if session_id != grant.session_id() {
            return Err(TransportContractError::SessionEvidenceMismatch);
        }
        if grant.expires_at() <= now_epoch_seconds {
            return Err(TransportContractError::GrantExpired);
        }
        validate_peer_grant_binding(&peer, &grant)?;
        Ok(Self {
            connection_id,
            session_id,
            peer,
            grant: GrantBindingEvidence::from(&grant),
            capabilities,
            authorization,
        })
    }

    /// Validates that all admission evidence remains authorized and current.
    ///
    /// # Errors
    ///
    /// Returns an error when authorization, expiry, or any identity binding fails.
    pub fn validate_admission(&self, now_epoch_seconds: u64) -> Result<(), TransportContractError> {
        if self.authorization != AuthorizationState::Authorized {
            return Err(TransportContractError::AdmissionNotAuthorized);
        }
        if self.grant.expires_at <= now_epoch_seconds {
            return Err(TransportContractError::GrantExpired);
        }
        if self.session_id != self.grant.session_id {
            return Err(TransportContractError::SessionEvidenceMismatch);
        }
        validate_peer_grant_evidence(&self.peer, &self.grant)
    }

    /// Returns the connection identifier.
    #[must_use]
    pub const fn connection_id(&self) -> &ConnectionId {
        &self.connection_id
    }

    /// Returns the active-session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns authenticated peer evidence.
    #[must_use]
    pub const fn peer(&self) -> &AuthenticatedPeer {
        &self.peer
    }

    /// Returns validated grant evidence.
    #[must_use]
    pub const fn grant(&self) -> &GrantBindingEvidence {
        &self.grant
    }

    /// Returns negotiated capabilities.
    #[must_use]
    pub const fn capabilities(&self) -> &NegotiatedCapabilities {
        &self.capabilities
    }

    /// Returns the authorization state.
    #[must_use]
    pub const fn authorization(&self) -> AuthorizationState {
        self.authorization
    }
}

pub(crate) fn validate_peer_grant_binding(
    peer: &AuthenticatedPeer,
    grant: &ValidatedSessionGrant,
) -> Result<(), TransportContractError> {
    match peer.role {
        PeerRole::Client if peer.identity != grant.client_identity().as_str() => {
            Err(TransportContractError::RoleIdentityMismatch)
        }
        PeerRole::Host if peer.identity != grant.host_identity().as_str() => {
            Err(TransportContractError::RoleIdentityMismatch)
        }
        PeerRole::Client | PeerRole::Host | PeerRole::Gateway => Ok(()),
    }
}

fn validate_peer_grant_evidence(
    peer: &AuthenticatedPeer,
    grant: &GrantBindingEvidence,
) -> Result<(), TransportContractError> {
    match peer.role {
        PeerRole::Client if peer.identity != grant.client_identity => {
            Err(TransportContractError::RoleIdentityMismatch)
        }
        PeerRole::Host if peer.identity != grant.host_identity => {
            Err(TransportContractError::RoleIdentityMismatch)
        }
        PeerRole::Client | PeerRole::Host | PeerRole::Gateway => Ok(()),
    }
}

fn validate_transport_identity(value: &str) -> Result<(), TransportContractError> {
    if value.is_empty() {
        return Err(TransportContractError::EmptyIdentity);
    }
    if value.len() > MAX_TRANSPORT_IDENTITY_BYTES {
        return Err(TransportContractError::IdentityTooLong);
    }
    Ok(())
}

/// Metadata available before payload allocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvelopeMetadata {
    /// Semantic protocol message class.
    pub message_class: MessageClass,
    /// Reliability and latency semantics.
    pub reliability: ReliabilityClass,
    /// Concrete delivery selected by negotiation.
    pub delivery: DeliveryMechanism,
    /// Declared payload bytes.
    pub declared_size: u32,
    /// Monotonic per-peer sequence.
    pub sequence: u64,
    /// Active-session evidence.
    pub session_id: String,
    /// Authenticated peer evidence.
    pub peer_identity: String,
}

/// Bounded outbound transport payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundEnvelope {
    /// Pre-allocation envelope metadata.
    pub metadata: EnvelopeMetadata,
    /// Uninterpreted protocol bytes.
    pub payload: Vec<u8>,
}

impl OutboundEnvelope {
    /// Creates an envelope whose declaration matches its payload.
    ///
    /// # Errors
    ///
    /// Returns an error when the payload size cannot be represented on the wire.
    pub fn new(
        mut metadata: EnvelopeMetadata,
        payload: Vec<u8>,
    ) -> Result<Self, TransportContractError> {
        metadata.declared_size = u32::try_from(payload.len())
            .map_err(|_| TransportContractError::DeclaredSizeUnrepresentable)?;
        Ok(Self { metadata, payload })
    }
}

/// Fully received bounded inbound envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundEnvelope {
    /// Validated metadata.
    pub metadata: EnvelopeMetadata,
    /// Payload allocated only after metadata acceptance.
    pub payload: Vec<u8>,
}

/// Metadata acceptance token required before payload reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AcceptedInboundEnvelope {
    metadata: EnvelopeMetadata,
    allocation_size: usize,
}

impl AcceptedInboundEnvelope {
    /// Returns validated metadata.
    #[must_use]
    pub const fn metadata(&self) -> &EnvelopeMetadata {
        &self.metadata
    }

    /// Returns the exact bounded allocation size.
    #[must_use]
    pub const fn allocation_size(&self) -> usize {
        self.allocation_size
    }
}

/// Per-connection payload and queue policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedTransportPolicy {
    /// Maximum control message bytes.
    pub max_control_payload_bytes: usize,
    /// Maximum media frame bytes.
    pub max_media_payload_bytes: usize,
    /// Maximum input event bytes.
    pub max_input_payload_bytes: usize,
    /// Maximum queued messages.
    pub max_queued_messages: usize,
    /// Maximum total queued bytes.
    pub max_queued_bytes: usize,
    /// Conservative maximum encrypted-datagram payload bytes. This is a
    /// contract-level ceiling independent of the dynamic, path-derived
    /// `max_datagram_size` reported by a live QUIC connection; the QUIC
    /// adapter enforces both.
    pub max_datagram_payload_bytes: usize,
}

impl Default for BoundedTransportPolicy {
    fn default() -> Self {
        Self {
            max_control_payload_bytes: 1024 * 1024,
            max_media_payload_bytes: 64 * 1024 * 1024,
            max_input_payload_bytes: 64 * 1024,
            max_queued_messages: 256,
            max_queued_bytes: 128 * 1024 * 1024,
            // Conservative: well under the common 1200-byte safe QUIC
            // datagram size assumption (1500 Ethernet MTU minus IPv6/UDP/QUIC
            // overhead), leaving headroom for the class/sequence frame header.
            max_datagram_payload_bytes: 1024,
        }
    }
}

impl BoundedTransportPolicy {
    /// Validates an outbound envelope.
    ///
    /// # Errors
    ///
    /// Returns an explicit metadata, cap, profile, or delivery failure.
    pub fn validate(
        self,
        profile: TransportProfile,
        envelope: &OutboundEnvelope,
    ) -> Result<(), TransportContractError> {
        let declared = usize::try_from(envelope.metadata.declared_size)
            .map_err(|_| TransportContractError::DeclaredSizeUnrepresentable)?;
        if declared != envelope.payload.len() {
            return Err(TransportContractError::PayloadLengthMismatch {
                declared,
                actual: envelope.payload.len(),
            });
        }
        self.validate_metadata(profile, &envelope.metadata)
    }

    /// Validates metadata without allocating its declared payload.
    ///
    /// # Errors
    ///
    /// Returns an explicit malformed, cap, profile, or delivery failure.
    pub fn validate_metadata(
        self,
        profile: TransportProfile,
        metadata: &EnvelopeMetadata,
    ) -> Result<(), TransportContractError> {
        #[cfg(not(feature = "quic"))]
        let _ = profile;
        let class_matches = matches!(
            (metadata.message_class, metadata.reliability),
            (MessageClass::Control, ReliabilityClass::Control)
                | (
                    MessageClass::Media,
                    ReliabilityClass::MediaReliable | ReliabilityClass::MediaLowLatency
                )
                | (MessageClass::Input, ReliabilityClass::InputLowLatency)
        );
        if !class_matches {
            return Err(TransportContractError::MessageClassMismatch);
        }
        match metadata.delivery {
            DeliveryMechanism::ReliableStream => {}
            #[cfg(feature = "quic")]
            DeliveryMechanism::EncryptedDatagram => {
                if profile != TransportProfile::Quic {
                    return Err(TransportContractError::DeliveryNotAvailableForProfile);
                }
                if metadata.reliability != ReliabilityClass::MediaLowLatency {
                    return Err(TransportContractError::DeliveryClassMismatch);
                }
            }
        }
        validate_transport_identity(&metadata.session_id)?;
        validate_transport_identity(&metadata.peer_identity)?;
        let actual = usize::try_from(metadata.declared_size)
            .map_err(|_| TransportContractError::DeclaredSizeUnrepresentable)?;
        let maximum = match metadata.reliability {
            ReliabilityClass::Control => self.max_control_payload_bytes,
            ReliabilityClass::MediaReliable => self.max_media_payload_bytes,
            ReliabilityClass::MediaLowLatency => {
                if is_encrypted_datagram(metadata.delivery) {
                    self.max_datagram_payload_bytes
                } else {
                    self.max_media_payload_bytes
                }
            }
            ReliabilityClass::InputLowLatency => self.max_input_payload_bytes,
        }
        .min(u32::MAX as usize);
        if actual > maximum {
            return Err(TransportContractError::PayloadTooLarge { actual, maximum });
        }
        Ok(())
    }

    /// Accepts inbound metadata against admission and receive expectations.
    ///
    /// # Errors
    ///
    /// Returns before allocation when any class, size, sequence, session, peer,
    /// grant, capability, or authorization evidence is invalid.
    pub fn accept_inbound(
        self,
        profile: TransportProfile,
        metadata: EnvelopeMetadata,
        binding: &SessionBinding,
        expectation: &ReceiveExpectation<'_>,
    ) -> Result<AcceptedInboundEnvelope, TransportContractError> {
        let admission = InboundAdmission::from_binding(binding, expectation.now_epoch_seconds)?;
        self.accept_inbound_for_admission(profile, metadata, &admission, expectation)
    }

    fn accept_inbound_for_admission(
        self,
        profile: TransportProfile,
        metadata: EnvelopeMetadata,
        admission: &InboundAdmission,
        expectation: &ReceiveExpectation<'_>,
    ) -> Result<AcceptedInboundEnvelope, TransportContractError> {
        self.validate_metadata(profile, &metadata)?;
        validate_envelope_capabilities(profile, &admission.capabilities, &metadata)?;
        if !expectation
            .allowed_message_classes
            .contains(&metadata.message_class)
        {
            return Err(TransportContractError::WrongMessageClass);
        }
        if metadata.sequence != expectation.expected_sequence {
            return Err(TransportContractError::SequenceMismatch);
        }
        if metadata.session_id != admission.session_id {
            return Err(TransportContractError::SessionEvidenceMismatch);
        }
        if metadata.peer_identity != admission.peer_identity {
            return Err(TransportContractError::PeerEvidenceMismatch);
        }
        let allocation_size = usize::try_from(metadata.declared_size)
            .map_err(|_| TransportContractError::DeclaredSizeUnrepresentable)?;
        Ok(AcceptedInboundEnvelope {
            metadata,
            allocation_size,
        })
    }
}

const fn is_encrypted_datagram(delivery: DeliveryMechanism) -> bool {
    match delivery {
        DeliveryMechanism::ReliableStream => false,
        #[cfg(feature = "quic")]
        DeliveryMechanism::EncryptedDatagram => true,
    }
}

struct InboundAdmission {
    session_id: String,
    peer_identity: String,
    capabilities: NegotiatedCapabilities,
}

impl InboundAdmission {
    fn from_binding(
        binding: &SessionBinding,
        now_epoch_seconds: u64,
    ) -> Result<Self, TransportContractError> {
        binding.validate_admission(now_epoch_seconds)?;
        Ok(Self {
            session_id: binding.session_id.clone(),
            peer_identity: binding.peer.identity.clone(),
            capabilities: binding.capabilities.clone(),
        })
    }
}

/// Expected evidence for one inbound envelope.
#[derive(Debug, Clone, Copy)]
pub struct ReceiveExpectation<'a> {
    /// Message classes admitted at this call site.
    pub allowed_message_classes: &'a BTreeSet<MessageClass>,
    /// Required next per-peer sequence.
    pub expected_sequence: u64,
    /// Current epoch seconds used for grant-expiry enforcement.
    pub now_epoch_seconds: u64,
}

/// Deterministic bounded outbound queue.
#[derive(Debug)]
pub struct BoundedQueue {
    profile: TransportProfile,
    policy: BoundedTransportPolicy,
    queued_bytes: usize,
    messages: VecDeque<OutboundEnvelope>,
}

impl BoundedQueue {
    /// Creates an empty queue.
    #[must_use]
    pub fn new(profile: TransportProfile, policy: BoundedTransportPolicy) -> Self {
        Self {
            profile,
            policy,
            queued_bytes: 0,
            messages: VecDeque::new(),
        }
    }

    /// Appends one validated message.
    ///
    /// # Errors
    ///
    /// Returns a payload or queue-bound failure without mutating the queue.
    pub fn push(&mut self, envelope: OutboundEnvelope) -> Result<(), TransportContractError> {
        self.policy.validate(self.profile, &envelope)?;
        if self.messages.len() >= self.policy.max_queued_messages {
            return Err(TransportContractError::QueueMessageLimit);
        }
        let next_bytes = self
            .queued_bytes
            .checked_add(envelope.payload.len())
            .ok_or(TransportContractError::QueueByteLimit)?;
        if next_bytes > self.policy.max_queued_bytes {
            return Err(TransportContractError::QueueByteLimit);
        }
        self.queued_bytes = next_bytes;
        self.messages.push_back(envelope);
        Ok(())
    }

    /// Removes the oldest message.
    pub fn pop(&mut self) -> Option<OutboundEnvelope> {
        let envelope = self.messages.pop_front()?;
        self.queued_bytes -= envelope.payload.len();
        Some(envelope)
    }

    /// Returns queued bytes.
    #[must_use]
    pub const fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// Returns whether the queue is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Returns queued message count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.messages.len()
    }
}

/// Transport and path lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportEvent {
    /// Connection established.
    Connected(ConnectionId),
    /// Connection gained session admission evidence.
    SessionBound(ConnectionId),
    /// Underlying network path changed (including transport-managed migration).
    PathChanged {
        /// Opaque previous path label.
        previous: String,
        /// Opaque new path label.
        current: String,
    },
    /// Path MTU changed.
    MtuChanged {
        /// Previous MTU in bytes.
        previous: u16,
        /// Current MTU in bytes.
        current: u16,
    },
    /// Connection was lost and reconnect is starting.
    Reconnecting,
    /// A replacement connection was established.
    Reconnected(ConnectionId),
    /// Connection was closed.
    Closed,
    /// A congestion/feedback snapshot became available.
    #[cfg(feature = "quic")]
    Feedback(quic::FeedbackSnapshot),
    /// An encrypted datagram was dropped; reliable/control traffic never
    /// drops silently, but best-effort datagrams may under pressure.
    #[cfg(feature = "quic")]
    DatagramDropped(quic::DatagramDropReason),
    /// The transport circuit breaker changed the active fallback mode.
    ///
    /// Hosts and clients should log this event with [`LOG_FIELD_TRANSPORT_MODE_PREV`]
    /// and [`LOG_FIELD_TRANSPORT_MODE`] for operational visibility.
    #[cfg(feature = "wss-compat")]
    FallbackModeChanged {
        /// Mode before the transition.
        from: fallback::FallbackMode,
        /// Mode after the transition.
        to: fallback::FallbackMode,
    },
}

/// Synchronous adapter boundary for compatibility transports.
///
/// Metadata is read first from an adapter-defined bounded header. Payload bytes
/// can be requested only with an acceptance token produced by shared policy.
///
/// QUIC I/O is inherently asynchronous, so the optional QUIC adapter uses a
/// separate async trait instead.
pub trait TransportPeer {
    /// Adapter-specific failure.
    type Error;

    /// Returns the negotiated concrete profile.
    fn profile(&self) -> TransportProfile;

    /// Returns the current admission binding.
    fn binding(&self) -> Option<&SessionBinding>;

    /// Sends one validated transport envelope.
    ///
    /// # Errors
    ///
    /// Returns adapter or policy failure.
    fn send(&mut self, envelope: OutboundEnvelope) -> Result<(), Self::Error>;

    /// Reads bounded envelope metadata without allocating the payload.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure while parsing the bounded header.
    fn receive_metadata(&mut self) -> Result<Option<EnvelopeMetadata>, Self::Error>;

    /// Reads payload bytes into the exact post-validation allocation.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure or reports a byte count that shared code checks
    /// against the declaration.
    fn receive_payload(
        &mut self,
        accepted: &AcceptedInboundEnvelope,
        destination: &mut [u8],
    ) -> Result<usize, Self::Error>;

    /// Receives the next lifecycle event, if available.
    ///
    /// # Errors
    ///
    /// Returns an adapter failure.
    fn next_event(&mut self) -> Result<Option<TransportEvent>, Self::Error>;
}

/// Receives one envelope with all limits enforced before payload allocation.
///
/// # Errors
///
/// Returns an adapter failure or shared contract rejection.
pub fn receive_bounded<P: TransportPeer>(
    peer: &mut P,
    policy: BoundedTransportPolicy,
    expectation: &ReceiveExpectation<'_>,
) -> Result<Option<InboundEnvelope>, BoundedReceiveError<P::Error>> {
    let admission = peer
        .binding()
        .ok_or(BoundedReceiveError::Contract(
            TransportContractError::MissingAdmissionBinding,
        ))
        .and_then(|binding| {
            InboundAdmission::from_binding(binding, expectation.now_epoch_seconds)
                .map_err(BoundedReceiveError::Contract)
        })?;
    let Some(metadata) = peer
        .receive_metadata()
        .map_err(BoundedReceiveError::Adapter)?
    else {
        return Ok(None);
    };
    let accepted = policy
        .accept_inbound_for_admission(peer.profile(), metadata, &admission, expectation)
        .map_err(BoundedReceiveError::Contract)?;
    let mut payload = vec![0; accepted.allocation_size];
    let actual = peer
        .receive_payload(&accepted, &mut payload)
        .map_err(BoundedReceiveError::Adapter)?;
    if actual != accepted.allocation_size {
        return Err(BoundedReceiveError::Contract(
            TransportContractError::PayloadLengthMismatch {
                declared: accepted.allocation_size,
                actual,
            },
        ));
    }
    Ok(Some(InboundEnvelope {
        metadata: accepted.metadata,
        payload,
    }))
}

/// Maps a dormant compatibility [`fallback::FallbackMode`] to the string value used for the
/// [`LOG_FIELD_TRANSPORT_MODE`] structured-logging field.
///
/// | Mode                     | Label              |
/// |--------------------------|--------------------|
/// | `QuicWithDatagrams`      | `"quic_datagrams"` |
/// | `QuicStreamsOnly`         | `"quic_streams"`   |
/// | `WebSocketSecure`        | `"wss"`            |
#[must_use]
#[cfg(feature = "wss-compat")]
pub fn transport_mode_label(mode: fallback::FallbackMode) -> &'static str {
    match mode {
        fallback::FallbackMode::QuicWithDatagrams => "quic_datagrams",
        fallback::FallbackMode::QuicStreamsOnly => "quic_streams",
        fallback::FallbackMode::WebSocketSecure => "wss",
    }
}

/// Maps a [`fallback::FailureKind`] to the string value used for the
/// [`LOG_FIELD_FAILURE_KIND`] structured-logging field.
#[must_use]
#[cfg(feature = "wss-compat")]
pub fn failure_kind_label(kind: fallback::FailureKind) -> &'static str {
    match kind {
        fallback::FailureKind::QuicHandshake => "quic_handshake",
        fallback::FailureKind::PacketTimeout => "packet_timeout",
        fallback::FailureKind::SustainedLoss => "sustained_loss",
        fallback::FailureKind::DatagramDrop => "datagram_drop",
        fallback::FailureKind::ConnectionClosed => "connection_closed",
        fallback::FailureKind::PathChange => "path_change",
    }
}

/// Maps a [`quic::DatagramDropReason`] to the string value used for the
/// [`LOG_FIELD_DATAGRAM_DROP_REASON`] structured-logging field.
#[cfg(feature = "quic")]
#[must_use]
pub fn datagram_drop_reason_label(reason: quic::DatagramDropReason) -> &'static str {
    match reason {
        quic::DatagramDropReason::MalformedFrame => "malformed_frame",
        quic::DatagramDropReason::ExceedsConfiguredCap => "exceeds_configured_cap",
        quic::DatagramDropReason::AdmissionRejected => "admission_rejected",
        quic::DatagramDropReason::ExceedsDynamicPathLimit => "exceeds_dynamic_path_limit",
        quic::DatagramDropReason::UnsupportedByPeer => "unsupported_by_peer",
        quic::DatagramDropReason::DisabledLocally => "disabled_locally",
        quic::DatagramDropReason::SendBufferFull => "send_buffer_full",
        quic::DatagramDropReason::InboundQueueFull => "inbound_queue_full",
        quic::DatagramDropReason::Duplicate => "duplicate",
        quic::DatagramDropReason::Late => "late",
        quic::DatagramDropReason::ConnectionRejected => "connection_rejected",
    }
}

/// Bounded receive failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundedReceiveError<E> {
    /// Concrete adapter failure.
    Adapter(E),
    /// Shared metadata, admission, or payload contract rejection.
    Contract(TransportContractError),
}

/// Forward error correction policy. No adaptive FEC scheme is implemented by
/// this crate. Any future adaptive behavior requires a reviewed profile; this
/// type only records that FEC is unsupported or intentionally disabled so
/// callers never infer adaptive behavior that does not exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FecPolicy {
    /// No FEC implementation exists in this crate.
    #[default]
    Unsupported,
    /// The caller explicitly disables any external FEC layer. This does not
    /// imply that this crate contains an FEC implementation.
    Disabled,
}

#[cfg(test)]
mod routing_tests {
    use super::*;

    #[test]
    fn video_bytes_map_to_media_low_latency() {
        for byte in 0x01u8..=0x05 {
            assert_eq!(
                reliability_class_for_frame_byte(byte),
                ReliabilityClass::MediaLowLatency,
                "frame byte 0x{byte:02X}"
            );
        }
    }

    #[test]
    fn audio_bytes_map_to_media_low_latency() {
        assert_eq!(
            reliability_class_for_frame_byte(0x10),
            ReliabilityClass::MediaLowLatency
        );
        assert_eq!(
            reliability_class_for_frame_byte(0x11),
            ReliabilityClass::MediaLowLatency
        );
    }

    #[test]
    fn hid_device_lifecycle_maps_to_control() {
        assert_eq!(
            reliability_class_for_frame_byte(0x20),
            ReliabilityClass::Control
        );
        assert_eq!(
            reliability_class_for_frame_byte(0x30),
            ReliabilityClass::Control
        );
        assert_eq!(
            reliability_class_for_frame_byte(0x31),
            ReliabilityClass::Control
        );
    }

    #[test]
    fn hid_report_maps_to_input_low_latency() {
        assert_eq!(
            reliability_class_for_frame_byte(0x32),
            ReliabilityClass::InputLowLatency
        );
    }

    #[test]
    fn hard_usb_urb_frames_remain_reliable() {
        assert_eq!(
            reliability_class_for_frame_byte(0x40),
            ReliabilityClass::Control
        );
        assert_eq!(
            reliability_class_for_frame_byte(0x41),
            ReliabilityClass::Control
        );
        assert_eq!(
            reliability_class_for_frame_byte(0x42),
            ReliabilityClass::InputLowLatency
        );
        assert_eq!(
            delivery_mechanism_for(
                ReliabilityClass::InputLowLatency,
                TransportProfile::Quic,
                true,
                64,
                1_200,
                Some(1_200),
                16,
            ),
            DeliveryMechanism::ReliableStream
        );
    }

    #[test]
    fn unknown_byte_maps_to_control() {
        assert_eq!(
            reliability_class_for_frame_byte(0xFF),
            ReliabilityClass::Control
        );
    }

    #[test]
    #[cfg(feature = "wss-compat")]
    fn wss_always_delivers_via_stream() {
        for class in [
            ReliabilityClass::Control,
            ReliabilityClass::MediaReliable,
            ReliabilityClass::MediaLowLatency,
            ReliabilityClass::InputLowLatency,
        ] {
            assert_eq!(
                delivery_mechanism_for(
                    class,
                    TransportProfile::WebSocketSecure,
                    true,
                    1,
                    1024,
                    Some(1200),
                    0
                ),
                DeliveryMechanism::ReliableStream,
                "WSS class {class:?} must always use ReliableStream"
            );
        }
    }

    #[cfg(feature = "quic")]
    #[test]
    fn quic_media_low_latency_uses_datagram_when_available() {
        assert_eq!(
            delivery_mechanism_for(
                ReliabilityClass::MediaLowLatency,
                TransportProfile::Quic,
                true,
                900,
                1024,
                Some(1200),
                0
            ),
            DeliveryMechanism::EncryptedDatagram
        );
    }

    #[cfg(feature = "quic")]
    #[test]
    fn quic_media_low_latency_falls_back_when_datagram_unavailable() {
        assert_eq!(
            delivery_mechanism_for(
                ReliabilityClass::MediaLowLatency,
                TransportProfile::Quic,
                false,
                900,
                1024,
                Some(1200),
                0
            ),
            DeliveryMechanism::ReliableStream
        );
    }

    #[cfg(feature = "quic")]
    #[test]
    fn quic_media_low_latency_falls_back_when_payload_exceeds_policy_cap() {
        assert_eq!(
            delivery_mechanism_for(
                ReliabilityClass::MediaLowLatency,
                TransportProfile::Quic,
                true,
                1200,
                1024,
                Some(1400),
                0
            ),
            DeliveryMechanism::ReliableStream
        );
    }

    #[cfg(feature = "quic")]
    #[test]
    fn quic_media_low_latency_falls_back_when_payload_exceeds_live_cap() {
        assert_eq!(
            delivery_mechanism_for(
                ReliabilityClass::MediaLowLatency,
                TransportProfile::Quic,
                true,
                900,
                1200,
                Some(850),
                0
            ),
            DeliveryMechanism::ReliableStream
        );
    }

    #[cfg(feature = "quic")]
    #[test]
    fn quic_non_media_classes_always_use_stream() {
        for class in [
            ReliabilityClass::Control,
            ReliabilityClass::MediaReliable,
            ReliabilityClass::InputLowLatency,
        ] {
            assert_eq!(
                delivery_mechanism_for(
                    class,
                    TransportProfile::Quic,
                    true,
                    100,
                    1024,
                    Some(1200),
                    0
                ),
                DeliveryMechanism::ReliableStream,
                "QUIC class {class:?} must use ReliableStream"
            );
        }
    }

    #[cfg(feature = "quic")]
    #[test]
    fn microphone_pcm_frame_falls_back_to_stream_with_default_cap() {
        assert_eq!(
            delivery_mechanism_for(
                ReliabilityClass::MediaLowLatency,
                TransportProfile::Quic,
                true,
                arcen_protocol::MICROPHONE_HEADER_SIZE + arcen_protocol::MICROPHONE_PCM_BYTES,
                1024,
                Some(1200),
                0
            ),
            DeliveryMechanism::ReliableStream
        );
    }

    #[cfg(feature = "quic")]
    #[test]
    fn microphone_max_opus_frame_falls_back_to_stream_with_default_cap() {
        assert_eq!(
            delivery_mechanism_for(
                ReliabilityClass::MediaLowLatency,
                TransportProfile::Quic,
                true,
                arcen_protocol::MICROPHONE_HEADER_SIZE + arcen_protocol::MAX_MICROPHONE_OPUS_BYTES,
                1024,
                Some(1200),
                0
            ),
            DeliveryMechanism::ReliableStream
        );
    }
}

/// Transport contract failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportContractError {
    /// Semantic message class did not match reliability semantics.
    MessageClassMismatch,
    /// Message class is not admitted by this receive path.
    WrongMessageClass,
    /// Delivery mechanism is not available for the given profile.
    DeliveryNotAvailableForProfile,
    /// Delivery mechanism is not permitted for the envelope's reliability class.
    DeliveryClassMismatch,
    /// Payload exceeded its message or reliability-class cap.
    PayloadTooLarge {
        /// Actual or declared bytes.
        actual: usize,
        /// Maximum bytes.
        maximum: usize,
    },
    /// Declared and actual payload lengths differ.
    PayloadLengthMismatch {
        /// Declared bytes.
        declared: usize,
        /// Actual bytes.
        actual: usize,
    },
    /// Declared size cannot be represented by the shared wire contract.
    DeclaredSizeUnrepresentable,
    /// Envelope sequence did not match the expected next value.
    SequenceMismatch,
    /// Envelope or admission active-session evidence did not match.
    SessionEvidenceMismatch,
    /// Envelope peer evidence did not match authenticated admission.
    PeerEvidenceMismatch,
    /// Stable direct-session Host evidence did not match.
    HostEvidenceMismatch,
    /// Authenticated role and grant identity did not match.
    RoleIdentityMismatch,
    /// Admission state was not authorized.
    AdmissionNotAuthorized,
    /// Peer had no admission binding.
    MissingAdmissionBinding,
    /// Grant expired before admission or receive.
    GrantExpired,
    /// Identity or capability value was empty.
    EmptyIdentity,
    /// Identity or capability value exceeded its shared bound.
    IdentityTooLong,
    /// Negotiated capability count exceeded its shared bound.
    CapabilityLimit,
    /// Negotiated capabilities do not satisfy profile or policy requirements.
    CapabilityMismatch,
    /// Peers have no policy-permitted concrete transport profile overlap.
    TransportProfileMismatch,
    /// Authenticated handshake role did not match admission expectations.
    PeerRoleMismatch,
    /// Queue message count is full.
    QueueMessageLimit,
    /// Queue byte count is full or overflowed.
    QueueByteLimit,
    /// Adapter payload did not correspond to the accepted metadata.
    ReceiveStateMismatch,
}

impl Display for TransportContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeliveryNotAvailableForProfile => {
                formatter.write_str("delivery mechanism is not available for this profile")
            }
            Self::MessageClassMismatch => {
                formatter.write_str("message and reliability classes do not match")
            }
            Self::WrongMessageClass => formatter.write_str("message class is not admitted"),
            Self::DeliveryClassMismatch => formatter
                .write_str("delivery mechanism is not permitted for this reliability class"),
            Self::PayloadTooLarge { actual, maximum } => {
                write!(formatter, "payload is {actual} bytes; maximum is {maximum}")
            }
            Self::PayloadLengthMismatch { declared, actual } => {
                write!(
                    formatter,
                    "payload declared {declared} bytes but produced {actual}"
                )
            }
            Self::DeclaredSizeUnrepresentable => {
                formatter.write_str("declared payload size is not representable")
            }
            Self::SequenceMismatch => formatter.write_str("envelope sequence does not match"),
            Self::SessionEvidenceMismatch => {
                formatter.write_str("active-session evidence does not match")
            }
            Self::PeerEvidenceMismatch => formatter.write_str("peer evidence does not match"),
            Self::HostEvidenceMismatch => {
                formatter.write_str("direct-session Host evidence does not match")
            }
            Self::RoleIdentityMismatch => {
                formatter.write_str("authenticated role identity does not match the grant")
            }
            Self::AdmissionNotAuthorized => {
                formatter.write_str("transport admission is not authorized")
            }
            Self::MissingAdmissionBinding => {
                formatter.write_str("transport peer has no admission binding")
            }
            Self::GrantExpired => formatter.write_str("session grant is expired"),
            Self::EmptyIdentity => formatter.write_str("transport identity is empty"),
            Self::IdentityTooLong => formatter.write_str("transport identity exceeds its bound"),
            Self::CapabilityLimit => formatter.write_str("negotiated capability limit reached"),
            Self::CapabilityMismatch => {
                formatter.write_str("negotiated capabilities do not satisfy policy")
            }
            Self::TransportProfileMismatch => {
                formatter.write_str("no policy-permitted transport profile overlap")
            }
            Self::PeerRoleMismatch => {
                formatter.write_str("authenticated peer role does not match admission")
            }
            Self::QueueMessageLimit => formatter.write_str("transport queue message limit reached"),
            Self::QueueByteLimit => formatter.write_str("transport queue byte limit reached"),
            Self::ReceiveStateMismatch => {
                formatter.write_str("payload does not match accepted envelope metadata")
            }
        }
    }
}

impl Error for TransportContractError {}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_identity::{
        ClientIdentity, ConsumedSessionGrant, GrantNonce, GrantReplayConsumer,
        GrantReplayConsumption, GrantReplayKey, GrantSignatureVerifier, GrantValidationContext,
        HostIdentity, SESSION_GRANT_VERSION_V1, SessionGrantClaims, SignedSessionGrant,
        consume_validated_session_grant, validate_session_grant,
    };

    struct AcceptSignature;

    impl GrantSignatureVerifier for AcceptSignature {
        type Error = ();

        fn verify(&self, _grant: &SignedSessionGrant) -> Result<(), Self::Error> {
            Ok(())
        }
    }

    struct ConsumeReplay;

    impl GrantReplayConsumer for ConsumeReplay {
        type Error = ();

        fn consume_once(
            &self,
            _key: &GrantReplayKey,
            _retain_until_epoch_seconds: u64,
            _now_epoch_seconds: u64,
        ) -> Result<GrantReplayConsumption, Self::Error> {
            Ok(GrantReplayConsumption::Consumed)
        }
    }

    fn consumed_grant() -> ConsumedSessionGrant {
        let client = ClientIdentity::new("client-1").expect("client");
        let host = HostIdentity::new("host-1").expect("Host");
        let nonce = GrantNonce::new("nonce-1").expect("nonce");
        let grant = SignedSessionGrant {
            claims: SessionGrantClaims {
                version: SESSION_GRANT_VERSION_V1,
                issuer: "arcen".to_owned(),
                audience: "gateway".to_owned(),
                subject: "human-1".to_owned(),
                tenant: Some("tenant-1".to_owned()),
                client_identity: client.clone(),
                host_identity: host.clone(),
                session_id: "session-1".to_owned(),
                nonce: nonce.clone(),
                issued_at: 100,
                expires_at: 200,
            },
            key_id: "key-1".to_owned(),
            algorithm: "adapter".to_owned(),
            signature: vec![1],
        };
        let validated = validate_session_grant(
            &AcceptSignature,
            &grant,
            GrantValidationContext {
                issuer: "arcen",
                audience: "gateway",
                expected_subject: "human-1",
                expected_tenant: Some("tenant-1"),
                expected_client_identity: &client,
                expected_host_identity: &host,
                expected_session_id: "session-1",
                expected_nonce: &nonce,
                now_epoch_seconds: 150,
            },
        )
        .expect("validated grant");
        consume_validated_session_grant(&ConsumeReplay, &validated, 150).expect("consumed grant")
    }

    fn binding(authorization: AuthorizationState) -> SessionBinding {
        let transport_capability = CAPABILITY_TRANSPORT_QUIC;
        SessionBinding::new(
            ConnectionId::new("connection-1"),
            "session-1",
            AuthenticatedPeer::new(PeerRole::Client, "client-1").expect("peer"),
            consumed_grant(),
            NegotiatedCapabilities::new(
                [
                    transport_capability,
                    CAPABILITY_RELIABLE_STREAM,
                    "control-v3",
                ]
                .map(|name| CapabilityId::new(name).expect("capability")),
            )
            .expect("capabilities"),
            authorization,
            150,
        )
        .expect("binding")
    }

    fn test_profile() -> TransportProfile {
        TransportProfile::Quic
    }

    fn metadata(bytes: u32) -> EnvelopeMetadata {
        EnvelopeMetadata {
            message_class: MessageClass::Control,
            reliability: ReliabilityClass::Control,
            delivery: DeliveryMechanism::ReliableStream,
            declared_size: bytes,
            sequence: 1,
            session_id: "session-1".to_owned(),
            peer_identity: "client-1".to_owned(),
        }
    }

    fn envelope(bytes: usize) -> OutboundEnvelope {
        OutboundEnvelope::new(metadata(0), vec![0; bytes]).expect("envelope")
    }

    #[cfg(feature = "quic")]
    fn envelope_for(
        reliability: ReliabilityClass,
        delivery: DeliveryMechanism,
        bytes: usize,
    ) -> OutboundEnvelope {
        let message_class = match reliability {
            ReliabilityClass::Control => MessageClass::Control,
            ReliabilityClass::MediaReliable | ReliabilityClass::MediaLowLatency => {
                MessageClass::Media
            }
            ReliabilityClass::InputLowLatency => MessageClass::Input,
        };
        OutboundEnvelope::new(
            EnvelopeMetadata {
                message_class,
                reliability,
                delivery,
                ..metadata(0)
            },
            vec![0; bytes],
        )
        .expect("envelope")
    }

    #[test]
    fn queue_bounds_are_deterministic_and_non_mutating_on_failure() {
        let policy = BoundedTransportPolicy {
            max_control_payload_bytes: 4,
            max_media_payload_bytes: 8,
            max_input_payload_bytes: 2,
            max_queued_messages: 1,
            max_queued_bytes: 4,
            max_datagram_payload_bytes: 4,
        };
        let mut queue = BoundedQueue::new(test_profile(), policy);
        queue.push(envelope(4)).expect("first message");
        assert_eq!(
            queue.push(envelope(1)),
            Err(TransportContractError::QueueMessageLimit)
        );
        assert_eq!(queue.len(), 1);
        assert_eq!(queue.queued_bytes(), 4);
        assert_eq!(queue.pop().map(|value| value.payload.len()), Some(4));
        assert!(queue.is_empty());
    }

    #[test]
    fn malformed_oversize_and_wrong_class_metadata_fail_before_allocation() {
        let policy = BoundedTransportPolicy {
            max_control_payload_bytes: 4,
            ..BoundedTransportPolicy::default()
        };
        let binding = binding(AuthorizationState::Authorized);
        let allowed = BTreeSet::from([MessageClass::Control]);
        let expectation = ReceiveExpectation {
            allowed_message_classes: &allowed,
            expected_sequence: 1,
            now_epoch_seconds: 151,
        };

        let mut malformed = metadata(1);
        malformed.reliability = ReliabilityClass::MediaReliable;
        assert_eq!(
            policy.accept_inbound(test_profile(), malformed, &binding, &expectation),
            Err(TransportContractError::MessageClassMismatch)
        );
        assert_eq!(
            policy.accept_inbound(test_profile(), metadata(5), &binding, &expectation),
            Err(TransportContractError::PayloadTooLarge {
                actual: 5,
                maximum: 4,
            })
        );
        let media = EnvelopeMetadata {
            message_class: MessageClass::Media,
            reliability: ReliabilityClass::MediaReliable,
            ..metadata(1)
        };
        assert_eq!(
            policy.accept_inbound(test_profile(), media, &binding, &expectation),
            Err(TransportContractError::WrongMessageClass)
        );
    }

    #[test]
    fn matching_session_id_without_authorization_cannot_admit() {
        let pending = binding(AuthorizationState::Pending);
        assert_eq!(
            pending.validate_admission(151),
            Err(TransportContractError::AdmissionNotAuthorized)
        );
    }

    #[test]
    #[cfg(all(feature = "quic", feature = "wss-compat"))]
    fn datagrams_are_rejected_outside_quic_profile() {
        let policy = BoundedTransportPolicy::default();
        let datagram = envelope_for(
            ReliabilityClass::MediaLowLatency,
            DeliveryMechanism::EncryptedDatagram,
            1,
        );
        assert_eq!(
            policy.validate(TransportProfile::WebSocketSecure, &datagram),
            Err(TransportContractError::DeliveryNotAvailableForProfile)
        );
        assert_eq!(policy.validate(TransportProfile::Quic, &datagram), Ok(()));
    }

    #[test]
    #[cfg(feature = "quic")]
    fn datagrams_are_rejected_for_non_low_latency_classes() {
        let policy = BoundedTransportPolicy::default();
        for class in [
            ReliabilityClass::Control,
            ReliabilityClass::MediaReliable,
            ReliabilityClass::InputLowLatency,
        ] {
            let datagram = envelope_for(class, DeliveryMechanism::EncryptedDatagram, 1);
            assert_eq!(
                policy.validate(TransportProfile::Quic, &datagram),
                Err(TransportContractError::DeliveryClassMismatch)
            );
        }
    }

    #[test]
    #[cfg(feature = "quic")]
    fn every_class_may_use_the_reliable_stream() {
        let policy = BoundedTransportPolicy::default();
        for class in [
            ReliabilityClass::Control,
            ReliabilityClass::MediaReliable,
            ReliabilityClass::MediaLowLatency,
            ReliabilityClass::InputLowLatency,
        ] {
            let stream_envelope = envelope_for(class, DeliveryMechanism::ReliableStream, 1);
            #[cfg(feature = "wss-compat")]
            assert_eq!(
                policy.validate(TransportProfile::WebSocketSecure, &stream_envelope),
                Ok(())
            );
            assert_eq!(
                policy.validate(TransportProfile::Quic, &stream_envelope),
                Ok(())
            );
        }
    }

    #[test]
    #[cfg(feature = "quic")]
    fn datagram_payload_cap_is_independent_of_media_cap() {
        let policy = BoundedTransportPolicy {
            max_media_payload_bytes: 8,
            max_datagram_payload_bytes: 2,
            ..BoundedTransportPolicy::default()
        };
        let datagram = envelope_for(
            ReliabilityClass::MediaLowLatency,
            DeliveryMechanism::EncryptedDatagram,
            3,
        );
        assert_eq!(
            policy.validate(TransportProfile::Quic, &datagram),
            Err(TransportContractError::PayloadTooLarge {
                actual: 3,
                maximum: 2,
            })
        );
        let stream = envelope_for(
            ReliabilityClass::MediaLowLatency,
            DeliveryMechanism::ReliableStream,
            3,
        );
        assert_eq!(policy.validate(TransportProfile::Quic, &stream), Ok(()));
    }

    #[test]
    fn fec_policy_defaults_to_unsupported_not_adaptive() {
        assert_eq!(FecPolicy::default(), FecPolicy::Unsupported);
    }

    #[test]
    fn direct_resume_transport_binding_is_bounded_and_distinct() {
        let binding = DirectResumeTransportBinding::new(
            ConnectionId::new("wss-connection-2"),
            HostIdentity::new("stable-host").expect("host"),
            ActiveHostSessionId::new("active-session").expect("session"),
        )
        .expect("binding");
        assert_eq!(binding.connection_id().as_str(), "wss-connection-2");
        assert_eq!(binding.host_identity().as_str(), "stable-host");
        assert_eq!(binding.active_session_id().as_str(), "active-session");
        assert_eq!(
            DirectResumeTransportBinding::new(
                ConnectionId::new(""),
                HostIdentity::new("stable-host").expect("host"),
                ActiveHostSessionId::new("active-session").expect("session"),
            ),
            Err(TransportContractError::EmptyIdentity)
        );
    }
}
