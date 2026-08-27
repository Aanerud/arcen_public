//! Concrete Quinn-backed QUIC peer: authenticated binding handshake, a
//! persistent unidirectional reliable stream per direction, encrypted
//! datagrams for low-latency media, bounded queues, lifecycle events, and a
//! congestion/feedback snapshot surface.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use quinn::Connection;
use tokio::sync::{Mutex as TokioMutex, Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::{
    AcceptedInboundEnvelope, AuthenticatedPeer, BoundedQueue, BoundedTransportPolicy, CapabilityId,
    ConnectionId, DeliveryMechanism, EnvelopeMetadata, InboundEnvelope, MessageClass,
    NegotiatedCapabilities, OutboundEnvelope, ReceiveExpectation, SessionBinding,
    TransportContractError, TransportEvent, TransportProfile, validate_envelope_capabilities,
    validate_negotiated_capabilities,
};

use super::QuicAdmission;
use super::error::QuicTransportError;
use super::feedback::{DatagramDropReason, FeedbackSnapshot};
use super::framing::{
    DatagramSequenceGuard, STREAM_FRAME_HEADER_BYTES, SequenceDecision, StreamFrameHeader,
    decode_datagram_frame, decode_stream_header, encode_datagram_frame, encode_stream_header,
};
use super::handshake::{
    HANDSHAKE_PROTOCOL_VERSION, HandshakeRejectReason, HandshakeRequest, HandshakeResponse,
};
use super::identity::{PeerIdentityAuthorizer, PeerIdentityClaim, QuicRole};

/// A boxed, `Send` future — the object-safe async adapter boundary used so
/// Host, Client, and Gateway crates can consume this transport without this
/// crate depending on any of them and without requiring `async_trait`.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Runtime tuning and required authorization hook for a QUIC peer.
///
/// There is no `Default` implementation: an authorizer must always be
/// supplied explicitly (never permissive by default).
#[derive(Clone)]
pub struct QuicRuntimeConfig {
    /// Payload/queue caps applied to every envelope.
    pub policy: BoundedTransportPolicy,
    /// Required peer identity/session authorization hook.
    pub authorizer: Arc<dyn PeerIdentityAuthorizer>,
    /// Bounded inbound application-message queue capacity.
    pub inbound_message_capacity: usize,
    /// Bounded lifecycle/feedback event queue capacity.
    pub event_capacity: usize,
    /// Interval between congestion/feedback snapshot events.
    pub feedback_interval: Duration,
    /// Maximum time allowed for binding and required stream establishment.
    pub establishment_timeout: Duration,
    /// Recently-accepted sequence numbers retained by the inbound datagram
    /// duplicate/late guard.
    pub datagram_sequence_window: usize,
}

impl std::fmt::Debug for QuicRuntimeConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuicRuntimeConfig")
            .field("policy", &self.policy)
            .field("authorizer", &"<dyn PeerIdentityAuthorizer>")
            .field("inbound_message_capacity", &self.inbound_message_capacity)
            .field("event_capacity", &self.event_capacity)
            .field("feedback_interval", &self.feedback_interval)
            .field("establishment_timeout", &self.establishment_timeout)
            .field("datagram_sequence_window", &self.datagram_sequence_window)
            .finish()
    }
}

impl QuicRuntimeConfig {
    /// Creates a runtime configuration with sane bounded defaults. The
    /// authorizer is required and has no permissive fallback.
    #[must_use]
    pub fn new(authorizer: Arc<dyn PeerIdentityAuthorizer>) -> Self {
        Self {
            policy: BoundedTransportPolicy::default(),
            authorizer,
            inbound_message_capacity: 256,
            event_capacity: 256,
            feedback_interval: Duration::from_secs(2),
            establishment_timeout: Duration::from_secs(10),
            datagram_sequence_window: 64,
        }
    }
}

/// Bounded, atomic operational counters. Datagram drops never fail silently:
/// each drop increments a counter in addition to emitting a
/// [`TransportEvent::DatagramDropped`] event (subject to the event queue's
/// own bound).
#[derive(Debug, Default)]
struct Counters {
    datagrams_sent: AtomicU64,
    datagrams_dropped: AtomicU64,
    datagrams_duplicate: AtomicU64,
    datagrams_late: AtomicU64,
    events_dropped: AtomicU64,
}

struct OutboundState {
    queue: BoundedQueue,
    completions: VecDeque<oneshot::Sender<Result<(), QuicTransportError>>>,
    next_stream_sequence: u64,
}

struct InboundMessage {
    envelope: InboundEnvelope,
    _byte_budget: OwnedSemaphorePermit,
}

/// Point-in-time snapshot of [`Counters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QuicPeerCounters {
    /// Datagrams successfully handed to Quinn for transmission.
    pub datagrams_sent: u64,
    /// Datagrams dropped for any send- or receive-side reason
    /// (see [`TransportEvent::DatagramDropped`]).
    pub datagrams_dropped: u64,
    /// Inbound datagrams rejected as exact duplicates.
    pub datagrams_duplicate: u64,
    /// Inbound datagrams rejected as too late relative to the newest sequence.
    pub datagrams_late: u64,
    /// Lifecycle/feedback events dropped because the bounded event queue was full.
    pub events_dropped: u64,
}

struct Shared {
    connection: Connection,
    policy: BoundedTransportPolicy,
    local_role: QuicRole,
    remote_role: QuicRole,
    local_peer: AuthenticatedPeer,
    binding: SessionBinding,
    admission_epoch_seconds: u64,
    outbound: StdMutex<OutboundState>,
    outbound_notify: Notify,
    inbound_tx: mpsc::Sender<InboundMessage>,
    inbound_rx: TokioMutex<mpsc::Receiver<InboundMessage>>,
    inbound_byte_budget: Arc<Semaphore>,
    events_tx: mpsc::Sender<TransportEvent>,
    events_rx: TokioMutex<mpsc::Receiver<TransportEvent>>,
    datagram_seq_out: AtomicU64,
    stream_seq_in: AtomicU64,
    datagram_guard: StdMutex<DatagramSequenceGuard>,
    cancel: CancellationToken,
    event_gate: StdMutex<()>,
    closed_event_emitted: AtomicBool,
    counters: Counters,
    feedback_interval: Duration,
}

impl Shared {
    fn try_emit_event(&self, event: TransportEvent) {
        let _gate = self
            .event_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if self.closed_event_emitted.load(Ordering::Acquire) && event != TransportEvent::Closed {
            return;
        }
        if self.events_tx.try_send(event).is_err() {
            self.counters.events_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn record_datagram_drop(&self, reason: DatagramDropReason) {
        self.counters
            .datagrams_dropped
            .fetch_add(1, Ordering::Relaxed);
        self.try_emit_event(TransportEvent::DatagramDropped(reason));
    }

    fn emit_closed(&self) {
        let _gate = self
            .event_gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !self.closed_event_emitted.swap(true, Ordering::AcqRel)
            && self.events_tx.try_send(TransportEvent::Closed).is_err()
        {
            self.counters.events_dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn authorize_peer_identity(
    connection: &Connection,
    authorizer: &dyn PeerIdentityAuthorizer,
    claimed_role: QuicRole,
    claimed_session_id: &str,
    expected_peer_identity: &str,
) -> Result<(), QuicTransportError> {
    let certificate_chain = connection
        .peer_identity()
        .and_then(|identity| {
            identity
                .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
                .ok()
        })
        .map(|boxed| *boxed)
        .ok_or(QuicTransportError::MissingPeerIdentity)?;
    let claim = PeerIdentityClaim {
        certificate_chain: &certificate_chain,
        claimed_role,
        claimed_session_id,
        expected_peer_identity,
        remote_address: connection.remote_address(),
    };
    authorizer
        .authorize(&claim)
        .map_err(QuicTransportError::Unauthorized)
}

/// A live, authenticated, bound QUIC peer connection.
///
/// Implements no synchronous [`crate::TransportPeer`] (QUIC I/O is
/// inherently asynchronous); see [`AsyncTransportPeer`] instead.
pub struct QuicPeer {
    shared: Arc<Shared>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for QuicPeer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("QuicPeer")
            .field("connection_id", self.shared.binding.connection_id())
            .field("session_id", &self.shared.binding.session_id())
            .field("local_role", &self.shared.local_role)
            .field("remote_role", &self.shared.remote_role)
            .finish_non_exhaustive()
    }
}

async fn perform_initiator_handshake(
    connection: &Connection,
    admission: &QuicAdmission,
    authorizer: &dyn PeerIdentityAuthorizer,
) -> Result<NegotiatedCapabilities, QuicTransportError> {
    let local_role = QuicRole::from(admission.local_peer.role());
    let session_id = admission.session_id();
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(QuicTransportError::Connection)?;
    let request = HandshakeRequest {
        protocol_version: HANDSHAKE_PROTOCOL_VERSION,
        role: local_role,
        session_id: session_id.to_owned(),
        supported_capabilities: capability_strings(&admission.supported_capabilities),
        required_capabilities: capability_strings(&admission.required_capabilities),
    };
    let request_bytes = request.encode().map_err(QuicTransportError::Handshake)?;
    send.write_all(&request_bytes)
        .await
        .map_err(QuicTransportError::StreamWrite)?;
    send.finish().map_err(|_| QuicTransportError::Closed)?;
    let response_bytes = recv
        .read_to_end(super::handshake::MAX_HANDSHAKE_FRAME_BYTES)
        .await
        .map_err(|_| QuicTransportError::Handshake(HandshakeRejectReason::Malformed))?;
    match HandshakeResponse::decode(&response_bytes).map_err(QuicTransportError::Handshake)? {
        HandshakeResponse::Ack {
            protocol_version,
            role,
            session_id: bound_session,
            negotiated_capabilities,
        } => {
            if protocol_version != HANDSHAKE_PROTOCOL_VERSION {
                return Err(QuicTransportError::Handshake(
                    HandshakeRejectReason::ProtocolVersionMismatch,
                ));
            }
            if bound_session != session_id {
                return Err(QuicTransportError::Handshake(
                    HandshakeRejectReason::SessionMismatch,
                ));
            }
            if role != QuicRole::from(admission.expected_remote_peer.role()) {
                return Err(QuicTransportError::Contract(
                    TransportContractError::PeerRoleMismatch,
                ));
            }
            let negotiated = capabilities_from_strings(negotiated_capabilities)
                .map_err(QuicTransportError::Handshake)?;
            if !admission.supported_capabilities.supports_all(&negotiated)
                || validate_negotiated_capabilities(
                    TransportProfile::Quic,
                    &negotiated,
                    &admission.required_capabilities,
                )
                .is_err()
            {
                return Err(QuicTransportError::Handshake(
                    HandshakeRejectReason::CapabilityMismatch,
                ));
            }
            // Authorization is mutual: the initiator also authorizes the
            // acceptor's TLS identity against the role/session it just
            // acknowledged, so the hook is required on both sides rather
            // than only for the party receiving a claim.
            authorize_peer_identity(
                connection,
                authorizer,
                role,
                &bound_session,
                admission.expected_remote_peer.identity(),
            )?;
            Ok(negotiated)
        }
        HandshakeResponse::Reject(reason) => Err(QuicTransportError::Handshake(reason)),
    }
}

async fn perform_acceptor_handshake(
    connection: &Connection,
    admission: &QuicAdmission,
    authorizer: &dyn PeerIdentityAuthorizer,
) -> Result<NegotiatedCapabilities, QuicTransportError> {
    let local_role = QuicRole::from(admission.local_peer.role());
    let expected_remote_role = QuicRole::from(admission.expected_remote_peer.role());
    let expected_session_id = admission.session_id();
    let (mut send, mut recv) = connection
        .accept_bi()
        .await
        .map_err(QuicTransportError::Connection)?;
    let request = read_handshake_request(&mut send, &mut recv).await?;
    if request.protocol_version != HANDSHAKE_PROTOCOL_VERSION {
        send_handshake_response(
            &mut send,
            &HandshakeResponse::Reject(HandshakeRejectReason::ProtocolVersionMismatch),
        )
        .await?;
        return Err(QuicTransportError::Handshake(
            HandshakeRejectReason::ProtocolVersionMismatch,
        ));
    }
    if expected_session_id != request.session_id {
        send_handshake_response(
            &mut send,
            &HandshakeResponse::Reject(HandshakeRejectReason::SessionMismatch),
        )
        .await?;
        return Err(QuicTransportError::Handshake(
            HandshakeRejectReason::SessionMismatch,
        ));
    }
    if request.role != expected_remote_role {
        send_handshake_response(
            &mut send,
            &HandshakeResponse::Reject(HandshakeRejectReason::Unauthorized),
        )
        .await?;
        return Err(QuicTransportError::Contract(
            TransportContractError::PeerRoleMismatch,
        ));
    }
    if let Err(error) = authorize_peer_identity(
        connection,
        authorizer,
        request.role,
        &request.session_id,
        admission.expected_remote_peer.identity(),
    ) {
        send_handshake_response(
            &mut send,
            &HandshakeResponse::Reject(HandshakeRejectReason::Unauthorized),
        )
        .await?;
        return Err(error);
    }
    let remote_supported = match capabilities_from_strings(request.supported_capabilities) {
        Ok(capabilities) => capabilities,
        Err(reason) => {
            send_handshake_response(&mut send, &HandshakeResponse::Reject(reason)).await?;
            return Err(QuicTransportError::Handshake(reason));
        }
    };
    let remote_required = match capabilities_from_strings(request.required_capabilities) {
        Ok(capabilities) => capabilities,
        Err(reason) => {
            send_handshake_response(&mut send, &HandshakeResponse::Reject(reason)).await?;
            return Err(QuicTransportError::Handshake(reason));
        }
    };
    let negotiated = match negotiate_capabilities(
        &admission.supported_capabilities,
        &admission.required_capabilities,
        &remote_supported,
        &remote_required,
    ) {
        Ok(negotiated) => negotiated,
        Err(reason) => {
            send_handshake_response(&mut send, &HandshakeResponse::Reject(reason)).await?;
            return Err(QuicTransportError::Handshake(reason));
        }
    };
    let ack = HandshakeResponse::Ack {
        protocol_version: HANDSHAKE_PROTOCOL_VERSION,
        role: local_role,
        session_id: request.session_id.clone(),
        negotiated_capabilities: capability_strings(&negotiated),
    };
    send_handshake_response(&mut send, &ack).await?;
    Ok(negotiated)
}

async fn read_handshake_request(
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
) -> Result<HandshakeRequest, QuicTransportError> {
    let Ok(request_bytes) = recv
        .read_to_end(super::handshake::MAX_HANDSHAKE_FRAME_BYTES)
        .await
    else {
        send_handshake_response(
            send,
            &HandshakeResponse::Reject(HandshakeRejectReason::Malformed),
        )
        .await?;
        return Err(QuicTransportError::Handshake(
            HandshakeRejectReason::Malformed,
        ));
    };
    match HandshakeRequest::decode(&request_bytes) {
        Ok(request) => Ok(request),
        Err(reason) => {
            send_handshake_response(send, &HandshakeResponse::Reject(reason)).await?;
            Err(QuicTransportError::Handshake(reason))
        }
    }
}

fn capability_strings(capabilities: &NegotiatedCapabilities) -> Vec<String> {
    capabilities
        .iter()
        .map(|capability| capability.as_str().to_owned())
        .collect()
}

fn capabilities_from_strings(
    capabilities: Vec<String>,
) -> Result<NegotiatedCapabilities, HandshakeRejectReason> {
    let capabilities = capabilities
        .into_iter()
        .map(CapabilityId::new)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| HandshakeRejectReason::Malformed)?;
    NegotiatedCapabilities::new(capabilities).map_err(|_| HandshakeRejectReason::Malformed)
}

fn negotiate_capabilities(
    local_supported: &NegotiatedCapabilities,
    local_required: &NegotiatedCapabilities,
    remote_supported: &NegotiatedCapabilities,
    remote_required: &NegotiatedCapabilities,
) -> Result<NegotiatedCapabilities, HandshakeRejectReason> {
    let negotiated = local_supported.intersection(remote_supported);
    if !negotiated.supports_all(remote_required)
        || validate_negotiated_capabilities(TransportProfile::Quic, &negotiated, local_required)
            .is_err()
    {
        return Err(HandshakeRejectReason::CapabilityMismatch);
    }
    Ok(negotiated)
}

async fn send_handshake_response(
    send: &mut quinn::SendStream,
    response: &HandshakeResponse,
) -> Result<(), QuicTransportError> {
    let bytes = response.encode().map_err(QuicTransportError::Handshake)?;
    send.write_all(&bytes)
        .await
        .map_err(QuicTransportError::StreamWrite)?;
    let stopped = send.stopped();
    send.finish().map_err(|_| QuicTransportError::Closed)?;
    match stopped.await {
        Ok(None) => Ok(()),
        Ok(Some(_)) | Err(_) => Err(QuicTransportError::Closed),
    }
}

impl QuicPeer {
    /// Establishes a peer as the QUIC-level connection initiator. Consumes the
    /// supplied fresh admission and performs authenticated identity, session,
    /// role, and capability negotiation before returning.
    ///
    /// # Errors
    ///
    /// Returns a connection, handshake, or authorization failure.
    pub async fn establish_initiator(
        connection: Connection,
        admission: QuicAdmission,
        runtime: QuicRuntimeConfig,
    ) -> Result<Self, QuicTransportError> {
        let negotiated = tokio::time::timeout(
            runtime.establishment_timeout.max(Duration::from_millis(1)),
            perform_initiator_handshake(&connection, &admission, runtime.authorizer.as_ref()),
        )
        .await
        .map_err(|_| QuicTransportError::EstablishmentTimedOut)??;
        Self::from_established(connection, admission, negotiated, runtime).await
    }

    /// Establishes a peer as the QUIC-level connection acceptor. The claimed
    /// session, role, identity, and required capabilities must match the fresh
    /// admission exactly or the handshake is rejected.
    ///
    /// # Errors
    ///
    /// Returns a connection, handshake, or authorization failure.
    pub async fn establish_acceptor(
        connection: Connection,
        admission: QuicAdmission,
        runtime: QuicRuntimeConfig,
    ) -> Result<Self, QuicTransportError> {
        let negotiated = tokio::time::timeout(
            runtime.establishment_timeout.max(Duration::from_millis(1)),
            perform_acceptor_handshake(&connection, &admission, runtime.authorizer.as_ref()),
        )
        .await
        .map_err(|_| QuicTransportError::EstablishmentTimedOut)??;
        Self::from_established(connection, admission, negotiated, runtime).await
    }

    async fn from_established(
        connection: Connection,
        admission: QuicAdmission,
        negotiated_capabilities: NegotiatedCapabilities,
        runtime: QuicRuntimeConfig,
    ) -> Result<Self, QuicTransportError> {
        let connection_id = ConnectionId::new(connection.stable_id().to_string());
        let session_id = admission.session_id().to_owned();
        let local_role = QuicRole::from(admission.local_peer.role());
        let remote_role = QuicRole::from(admission.expected_remote_peer.role());
        validate_negotiated_capabilities(
            TransportProfile::Quic,
            &negotiated_capabilities,
            &admission.required_capabilities,
        )
        .map_err(QuicTransportError::Contract)?;
        let binding = SessionBinding::new(
            connection_id.clone(),
            session_id,
            admission.expected_remote_peer,
            admission.consumed_grant,
            negotiated_capabilities,
            admission.authorization,
            admission.now_epoch_seconds,
        )
        .map_err(QuicTransportError::Contract)?;
        binding
            .validate_admission(admission.now_epoch_seconds)
            .map_err(QuicTransportError::Contract)?;

        // Opening a uni stream is purely local (it only consumes remote
        // stream-count budget already known from the transport parameter
        // exchange); it does not require any peer action. Accepting the
        // peer's uni stream, however, only resolves once the peer actually
        // writes to it — so it must not be awaited here, or both sides would
        // deadlock waiting for each other to go first. The reader task
        // performs its own `accept_uni` lazily instead.
        let send_stream = tokio::time::timeout(
            runtime.establishment_timeout.max(Duration::from_millis(1)),
            connection.open_uni(),
        )
        .await
        .map_err(|_| QuicTransportError::EstablishmentTimedOut)?
        .map_err(QuicTransportError::Connection)?;

        let inbound_capacity = runtime
            .inbound_message_capacity
            .clamp(1, Semaphore::MAX_PERMITS);
        let (inbound_tx, inbound_rx) = mpsc::channel(inbound_capacity);
        // Connected and SessionBound must always fit before the caller can
        // begin draining events.
        let event_capacity = runtime.event_capacity.clamp(4, Semaphore::MAX_PERMITS);
        let (events_tx, events_rx) = mpsc::channel(event_capacity);
        let feedback_interval = runtime.feedback_interval.max(Duration::from_millis(1));
        let inbound_byte_budget = Arc::new(Semaphore::new(
            runtime
                .policy
                .max_queued_bytes
                .clamp(1, Semaphore::MAX_PERMITS),
        ));

        let shared = Arc::new(Shared {
            connection: connection.clone(),
            policy: runtime.policy,
            local_role,
            remote_role,
            local_peer: admission.local_peer,
            binding,
            admission_epoch_seconds: admission.now_epoch_seconds,
            outbound: StdMutex::new(OutboundState {
                queue: BoundedQueue::new(TransportProfile::Quic, runtime.policy),
                completions: VecDeque::new(),
                next_stream_sequence: 0,
            }),
            outbound_notify: Notify::new(),
            inbound_tx,
            inbound_rx: TokioMutex::new(inbound_rx),
            inbound_byte_budget,
            events_tx,
            events_rx: TokioMutex::new(events_rx),
            datagram_seq_out: AtomicU64::new(0),
            stream_seq_in: AtomicU64::new(0),
            datagram_guard: StdMutex::new(DatagramSequenceGuard::new(
                runtime.datagram_sequence_window,
            )),
            cancel: CancellationToken::new(),
            event_gate: StdMutex::new(()),
            closed_event_emitted: AtomicBool::new(false),
            counters: Counters::default(),
            feedback_interval,
        });

        shared.try_emit_event(TransportEvent::Connected(connection_id));
        shared.try_emit_event(TransportEvent::SessionBound(
            shared.binding.connection_id().clone(),
        ));

        let tasks = vec![
            tokio::spawn(stream_writer_task(shared.clone(), send_stream)),
            tokio::spawn(stream_reader_task(shared.clone())),
            tokio::spawn(datagram_reader_task(shared.clone())),
            tokio::spawn(lifecycle_task(shared.clone())),
        ];

        Ok(Self { shared, tasks })
    }

    /// Returns this connection's stable identifier.
    #[must_use]
    pub fn connection_id(&self) -> ConnectionId {
        self.shared.binding.connection_id.clone()
    }

    /// Returns the current session binding.
    #[must_use]
    pub fn binding(&self) -> &SessionBinding {
        &self.shared.binding
    }

    /// Returns the negotiated remote role.
    #[must_use]
    pub fn remote_role(&self) -> QuicRole {
        self.shared.remote_role
    }

    /// Returns the local role.
    #[must_use]
    pub fn local_role(&self) -> QuicRole {
        self.shared.local_role
    }

    /// Sends one validated envelope. Reliable-stream envelopes are enqueued
    /// on the bounded outbound queue and rejected explicitly (never silently
    /// dropped) when the queue is full; encrypted datagrams are sent
    /// immediately and may be dropped under pressure, always with an
    /// explicit event and counter increment.
    ///
    /// # Errors
    ///
    /// Returns a contract, queue, or datagram-send failure.
    pub async fn send(&self, envelope: OutboundEnvelope) -> Result<(), QuicTransportError> {
        if self.shared.cancel.is_cancelled() {
            return Err(QuicTransportError::Closed);
        }
        self.shared
            .policy
            .validate(TransportProfile::Quic, &envelope)
            .map_err(QuicTransportError::Contract)?;
        validate_envelope_capabilities(
            TransportProfile::Quic,
            self.shared.binding.capabilities(),
            &envelope.metadata,
        )
        .map_err(QuicTransportError::Contract)?;
        if envelope.metadata.session_id != self.shared.binding.session_id() {
            return Err(QuicTransportError::Contract(
                TransportContractError::SessionEvidenceMismatch,
            ));
        }
        if envelope.metadata.peer_identity != self.shared.local_peer.identity() {
            return Err(QuicTransportError::Contract(
                TransportContractError::PeerEvidenceMismatch,
            ));
        }
        match envelope.metadata.delivery {
            DeliveryMechanism::ReliableStream => {
                let (completion_tx, completion_rx) = oneshot::channel();
                {
                    let mut guard = self
                        .shared
                        .outbound
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    if envelope.metadata.sequence != guard.next_stream_sequence {
                        return Err(QuicTransportError::Contract(
                            TransportContractError::SequenceMismatch,
                        ));
                    }
                    let next_stream_sequence = guard.next_stream_sequence.checked_add(1).ok_or(
                        QuicTransportError::Contract(TransportContractError::SequenceMismatch),
                    )?;
                    guard.queue.push(envelope).map_err(|error| match error {
                        TransportContractError::QueueMessageLimit
                        | TransportContractError::QueueByteLimit => {
                            QuicTransportError::OutboundQueueFull
                        }
                        other => QuicTransportError::Contract(other),
                    })?;
                    guard.next_stream_sequence = next_stream_sequence;
                    guard.completions.push_back(completion_tx);
                }
                self.shared.outbound_notify.notify_one();
                tokio::select! {
                    result = completion_rx => result.unwrap_or(Err(QuicTransportError::Closed)),
                    () = self.shared.cancel.cancelled() => Err(QuicTransportError::Closed),
                }
            }
            DeliveryMechanism::EncryptedDatagram => self.send_datagram_now(&envelope),
        }
    }

    fn send_datagram_now(&self, envelope: &OutboundEnvelope) -> Result<(), QuicTransportError> {
        if self.shared.cancel.is_cancelled() {
            return Err(QuicTransportError::Closed);
        }
        let sequence = self.shared.datagram_seq_out.load(Ordering::Acquire);
        if envelope.metadata.sequence != sequence {
            return Err(QuicTransportError::Contract(
                TransportContractError::SequenceMismatch,
            ));
        }
        let next_sequence = sequence.checked_add(1).ok_or(QuicTransportError::Contract(
            TransportContractError::SequenceMismatch,
        ))?;
        self.shared
            .datagram_seq_out
            .compare_exchange(sequence, next_sequence, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| QuicTransportError::Contract(TransportContractError::SequenceMismatch))?;
        let frame = encode_datagram_frame(&envelope.metadata, &envelope.payload)
            .map_err(QuicTransportError::Contract)?;

        let Some(dynamic_limit) = self.shared.connection.max_datagram_size() else {
            self.record_datagram_drop(DatagramDropReason::UnsupportedByPeer);
            return Err(QuicTransportError::DatagramSend(
                quinn::SendDatagramError::UnsupportedByPeer,
            ));
        };
        if frame.len() > dynamic_limit {
            self.record_datagram_drop(DatagramDropReason::ExceedsDynamicPathLimit);
            return Err(QuicTransportError::DatagramSend(
                quinn::SendDatagramError::TooLarge,
            ));
        }
        if self.shared.connection.datagram_send_buffer_space() < frame.len() {
            self.record_datagram_drop(DatagramDropReason::SendBufferFull);
            return Err(QuicTransportError::OutboundQueueFull);
        }
        match self.shared.connection.send_datagram(frame.into()) {
            Ok(()) => {
                self.shared
                    .counters
                    .datagrams_sent
                    .fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(error) => {
                let reason = match &error {
                    quinn::SendDatagramError::UnsupportedByPeer => {
                        DatagramDropReason::UnsupportedByPeer
                    }
                    quinn::SendDatagramError::Disabled => DatagramDropReason::DisabledLocally,
                    quinn::SendDatagramError::TooLarge => {
                        DatagramDropReason::ExceedsDynamicPathLimit
                    }
                    quinn::SendDatagramError::ConnectionLost(_) => {
                        DatagramDropReason::ConnectionRejected
                    }
                };
                self.record_datagram_drop(reason);
                Err(QuicTransportError::DatagramSend(error))
            }
        }
    }

    fn record_datagram_drop(&self, reason: DatagramDropReason) {
        self.shared.record_datagram_drop(reason);
    }

    /// Receives the next application envelope, from either the reliable
    /// stream or an accepted datagram, waiting if none is queued yet.
    /// Returns `None` once the peer is closed and drained.
    pub async fn recv_message(&self) -> Option<InboundEnvelope> {
        let mut receiver = self.shared.inbound_rx.lock().await;
        if let Ok(message) = receiver.try_recv() {
            return Some(message.envelope);
        }
        tokio::select! {
            biased;
            message = receiver.recv() => message.map(|message| message.envelope),
            () = self.shared.cancel.cancelled() => None,
        }
    }

    /// Receives the next lifecycle/feedback event, waiting if none is queued
    /// yet. Returns `None` once the peer is closed and drained.
    pub async fn recv_event(&self) -> Option<TransportEvent> {
        let mut receiver = self.shared.events_rx.lock().await;
        if let Ok(event) = receiver.try_recv() {
            return Some(event);
        }
        tokio::select! {
            biased;
            event = receiver.recv() => event,
            () = self.shared.cancel.cancelled() => None,
        }
    }

    /// Returns a live congestion/feedback snapshot.
    #[must_use]
    pub fn feedback_snapshot(&self) -> FeedbackSnapshot {
        FeedbackSnapshot::from_connection(&self.shared.connection)
    }

    /// Returns a point-in-time counters snapshot.
    #[must_use]
    pub fn counters(&self) -> QuicPeerCounters {
        QuicPeerCounters {
            datagrams_sent: self.shared.counters.datagrams_sent.load(Ordering::Relaxed),
            datagrams_dropped: self
                .shared
                .counters
                .datagrams_dropped
                .load(Ordering::Relaxed),
            datagrams_duplicate: self
                .shared
                .counters
                .datagrams_duplicate
                .load(Ordering::Relaxed),
            datagrams_late: self.shared.counters.datagrams_late.load(Ordering::Relaxed),
            events_dropped: self.shared.counters.events_dropped.load(Ordering::Relaxed),
        }
    }

    /// Emits an explicit reconnect-starting event on this (typically about to
    /// be replaced) peer.
    pub fn note_reconnecting(&self) {
        self.shared.try_emit_event(TransportEvent::Reconnecting);
    }

    /// Emits an explicit reconnected event carrying this peer's own
    /// (new) connection identifier.
    pub fn note_reconnected(&self) {
        self.shared
            .try_emit_event(TransportEvent::Reconnected(self.connection_id()));
    }

    /// Closes the connection and cancels all background tasks. Safe to call
    /// more than once.
    pub fn close(&self) {
        if !self.shared.cancel.is_cancelled() {
            self.shared
                .connection
                .close(quinn::VarInt::from_u32(0), b"closed");
            fail_pending_reliable(&self.shared);
            self.shared.emit_closed();
            self.shared.cancel.cancel();
        }
    }
}

impl Drop for QuicPeer {
    fn drop(&mut self) {
        self.close();
        for task in &self.tasks {
            task.abort();
        }
    }
}

async fn stream_writer_task(shared: Arc<Shared>, mut send_stream: quinn::SendStream) {
    loop {
        loop {
            let popped = {
                let mut guard = shared
                    .outbound
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                guard
                    .queue
                    .pop()
                    .map(|envelope| (envelope, guard.completions.pop_front()))
            };
            let Some((envelope, Some(completion))) = popped else {
                break;
            };
            let payload_len = envelope.metadata.declared_size;
            let header = encode_stream_header(
                envelope.metadata.message_class,
                envelope.metadata.reliability,
                envelope.metadata.delivery,
                payload_len,
                envelope.metadata.sequence,
            );
            if let Err(error) = send_stream.write_all(&header).await {
                let _ = completion.send(Err(QuicTransportError::StreamWrite(error)));
                fail_pending_reliable(&shared);
                shared.emit_closed();
                shared.cancel.cancel();
                return;
            }
            if let Err(error) = send_stream.write_all(&envelope.payload).await {
                let _ = completion.send(Err(QuicTransportError::StreamWrite(error)));
                fail_pending_reliable(&shared);
                shared.emit_closed();
                shared.cancel.cancel();
                return;
            }
            let _ = completion.send(Ok(()));
        }
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            () = shared.outbound_notify.notified() => {}
        }
    }
}

fn fail_pending_reliable(shared: &Shared) {
    let mut guard = shared
        .outbound
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    while guard.queue.pop().is_some() {
        if let Some(completion) = guard.completions.pop_front() {
            let _ = completion.send(Err(QuicTransportError::Closed));
        }
    }
}

async fn stream_reader_task(shared: Arc<Shared>) {
    let mut recv_stream = tokio::select! {
        () = shared.cancel.cancelled() => return,
        result = shared.connection.accept_uni() => {
            let Ok(stream) = result else {
                shared.emit_closed();
                shared.cancel.cancel();
                return;
            };
            stream
        }
    };
    loop {
        let mut header = [0_u8; STREAM_FRAME_HEADER_BYTES];
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            result = recv_stream.read_exact(&mut header) => {
                if result.is_err() {
                    shared.emit_closed();
                    shared.cancel.cancel();
                    return;
                }
            }
        }
        let Some(decoded) = decode_stream_header(header) else {
            shared.emit_closed();
            shared.cancel.cancel();
            return;
        };
        if decoded.delivery != DeliveryMechanism::ReliableStream {
            shared.emit_closed();
            shared.cancel.cancel();
            return;
        }
        let Ok(accepted) = accept_stream_header(&shared, decoded) else {
            shared.emit_closed();
            shared.cancel.cancel();
            return;
        };
        if accepted.allocation_size() > shared.policy.max_queued_bytes {
            shared.emit_closed();
            shared.cancel.cancel();
            return;
        }
        if accepted.allocation_size() > Semaphore::MAX_PERMITS {
            shared.emit_closed();
            shared.cancel.cancel();
            return;
        }
        let Ok(permit_count) = u32::try_from(accepted.allocation_size().max(1)) else {
            shared.emit_closed();
            shared.cancel.cancel();
            return;
        };
        let byte_budget = tokio::select! {
            () = shared.cancel.cancelled() => return,
            result = shared.inbound_byte_budget.clone().acquire_many_owned(permit_count) => {
                let Ok(permit) = result else { return };
                permit
            }
        };
        let mut payload = vec![0_u8; accepted.allocation_size()];
        if recv_stream.read_exact(&mut payload).await.is_err() {
            shared.emit_closed();
            shared.cancel.cancel();
            return;
        }
        shared.stream_seq_in.fetch_add(1, Ordering::Release);
        // Blocking send applies real backpressure for reliable/control data
        // instead of ever silently dropping it.
        let message = InboundMessage {
            envelope: InboundEnvelope {
                metadata: accepted.metadata().clone(),
                payload,
            },
            _byte_budget: byte_budget,
        };
        if shared.inbound_tx.send(message).await.is_err() {
            return;
        }
    }
}

fn all_message_classes() -> std::collections::BTreeSet<MessageClass> {
    [
        MessageClass::Control,
        MessageClass::Media,
        MessageClass::Input,
    ]
    .into_iter()
    .collect()
}

fn accept_stream_header(
    shared: &Shared,
    decoded: StreamFrameHeader,
) -> Result<AcceptedInboundEnvelope, TransportContractError> {
    let metadata = EnvelopeMetadata {
        message_class: decoded.message_class,
        reliability: decoded.reliability,
        delivery: decoded.delivery,
        declared_size: decoded.payload_len,
        sequence: decoded.sequence,
        session_id: shared.binding.session_id().to_owned(),
        peer_identity: shared.binding.peer().identity().to_owned(),
    };
    let allowed = all_message_classes();
    shared.policy.accept_inbound(
        TransportProfile::Quic,
        metadata,
        &shared.binding,
        &ReceiveExpectation {
            allowed_message_classes: &allowed,
            expected_sequence: shared.stream_seq_in.load(Ordering::Acquire),
            now_epoch_seconds: shared.admission_epoch_seconds,
        },
    )
}

async fn datagram_reader_task(shared: Arc<Shared>) {
    loop {
        let bytes = tokio::select! {
            () = shared.cancel.cancelled() => return,
            result = shared.connection.read_datagram() => {
                let Ok(bytes) = result else {
                    shared.emit_closed();
                    shared.cancel.cancel();
                    return;
                };
                bytes
            }
        };
        process_datagram(&shared, &bytes);
    }
}

fn process_datagram(shared: &Shared, bytes: &[u8]) {
    let Some(frame) = decode_datagram_frame(bytes) else {
        shared.record_datagram_drop(DatagramDropReason::MalformedFrame);
        return;
    };
    if frame.delivery != DeliveryMechanism::EncryptedDatagram {
        shared.record_datagram_drop(DatagramDropReason::AdmissionRejected);
        return;
    }
    let metadata = EnvelopeMetadata {
        message_class: frame.message_class,
        reliability: frame.reliability,
        delivery: frame.delivery,
        declared_size: frame.payload_len,
        sequence: frame.sequence,
        session_id: shared.binding.session_id().to_owned(),
        peer_identity: shared.binding.peer().identity().to_owned(),
    };
    let allowed = std::collections::BTreeSet::from([MessageClass::Media]);
    let expectation = ReceiveExpectation {
        allowed_message_classes: &allowed,
        expected_sequence: frame.sequence,
        now_epoch_seconds: shared.admission_epoch_seconds,
    };
    let accepted = match shared.policy.accept_inbound(
        TransportProfile::Quic,
        metadata,
        &shared.binding,
        &expectation,
    ) {
        Ok(accepted) => accepted,
        Err(TransportContractError::PayloadTooLarge { .. }) => {
            shared.record_datagram_drop(DatagramDropReason::ExceedsConfiguredCap);
            return;
        }
        Err(_) => {
            shared.record_datagram_drop(DatagramDropReason::AdmissionRejected);
            return;
        }
    };
    let decision = {
        let mut guard = shared
            .datagram_guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.observe(frame.sequence)
    };
    match decision {
        SequenceDecision::Accept => enqueue_datagram(shared, &accepted, frame.payload),
        SequenceDecision::Duplicate => {
            shared
                .counters
                .datagrams_duplicate
                .fetch_add(1, Ordering::Relaxed);
            shared.record_datagram_drop(DatagramDropReason::Duplicate);
        }
        SequenceDecision::Late => {
            shared
                .counters
                .datagrams_late
                .fetch_add(1, Ordering::Relaxed);
            shared.record_datagram_drop(DatagramDropReason::Late);
        }
    }
}

fn enqueue_datagram(shared: &Shared, accepted: &AcceptedInboundEnvelope, payload: &[u8]) {
    let Ok(permit_count) = u32::try_from(accepted.allocation_size().max(1)) else {
        shared.record_datagram_drop(DatagramDropReason::InboundQueueFull);
        return;
    };
    let Ok(byte_budget) = shared
        .inbound_byte_budget
        .clone()
        .try_acquire_many_owned(permit_count)
    else {
        shared.record_datagram_drop(DatagramDropReason::InboundQueueFull);
        return;
    };
    let message = InboundMessage {
        envelope: InboundEnvelope {
            metadata: accepted.metadata().clone(),
            payload: payload.to_vec(),
        },
        _byte_budget: byte_budget,
    };
    if shared.inbound_tx.try_send(message).is_err() {
        shared.record_datagram_drop(DatagramDropReason::InboundQueueFull);
    }
}

async fn lifecycle_task(shared: Arc<Shared>) {
    let mut ticker = tokio::time::interval(shared.feedback_interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut previous_remote = shared.connection.remote_address();
    let mut previous_mtu = shared.connection.stats().path.current_mtu;

    loop {
        tokio::select! {
            () = shared.cancel.cancelled() => return,
            reason = shared.connection.closed() => {
                let _ = reason;
                shared.emit_closed();
                shared.cancel.cancel();
                return;
            }
            _ = ticker.tick() => {
                let snapshot = FeedbackSnapshot::from_connection(&shared.connection);
                shared.try_emit_event(TransportEvent::Feedback(snapshot));

                let current_remote = shared.connection.remote_address();
                if current_remote != previous_remote {
                    shared.try_emit_event(TransportEvent::PathChanged {
                        previous: previous_remote.to_string(),
                        current: current_remote.to_string(),
                    });
                    previous_remote = current_remote;
                }
                if snapshot.current_mtu != previous_mtu {
                    shared.try_emit_event(TransportEvent::MtuChanged {
                        previous: previous_mtu,
                        current: snapshot.current_mtu,
                    });
                    previous_mtu = snapshot.current_mtu;
                }
            }
        }
    }
}

/// Object-safe async adapter boundary for Host, Client, and Gateway
/// consumers. Uses boxed `Send` futures instead of `async_trait` so it stays
/// object-safe (`Box<dyn AsyncTransportPeer>` / `Arc<dyn AsyncTransportPeer>`)
/// without an extra proc-macro dependency.
pub trait AsyncTransportPeer: Send + Sync {
    /// Returns the negotiated concrete profile (always [`TransportProfile::Quic`]).
    fn profile(&self) -> TransportProfile;

    /// Returns the current session binding.
    fn binding(&self) -> &SessionBinding;

    /// Returns this connection's stable identifier.
    fn connection_id(&self) -> ConnectionId;

    /// Sends one validated transport envelope.
    fn send(&self, envelope: OutboundEnvelope) -> BoxFuture<'_, Result<(), QuicTransportError>>;

    /// Receives the next application payload, if any remain before closure.
    fn recv_message(&self) -> BoxFuture<'_, Option<InboundEnvelope>>;

    /// Receives the next lifecycle/feedback event, if any remain before closure.
    fn recv_event(&self) -> BoxFuture<'_, Option<TransportEvent>>;

    /// Returns a live congestion/feedback snapshot.
    fn feedback_snapshot(&self) -> FeedbackSnapshot;

    /// Closes the connection and cancels background tasks.
    fn close(&self);
}

impl AsyncTransportPeer for QuicPeer {
    fn profile(&self) -> TransportProfile {
        TransportProfile::Quic
    }

    fn binding(&self) -> &SessionBinding {
        Self::binding(self)
    }

    fn connection_id(&self) -> ConnectionId {
        Self::connection_id(self)
    }

    fn send(&self, envelope: OutboundEnvelope) -> BoxFuture<'_, Result<(), QuicTransportError>> {
        Box::pin(Self::send(self, envelope))
    }

    fn recv_message(&self) -> BoxFuture<'_, Option<InboundEnvelope>> {
        Box::pin(Self::recv_message(self))
    }

    fn recv_event(&self) -> BoxFuture<'_, Option<TransportEvent>> {
        Box::pin(Self::recv_event(self))
    }

    fn feedback_snapshot(&self) -> FeedbackSnapshot {
        Self::feedback_snapshot(self)
    }

    fn close(&self) {
        Self::close(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CAPABILITY_ENCRYPTED_DATAGRAM, CAPABILITY_RELIABLE_STREAM, CAPABILITY_TRANSPORT_QUIC,
    };

    fn capabilities(names: &[&str]) -> NegotiatedCapabilities {
        NegotiatedCapabilities::new(
            names
                .iter()
                .map(|name| CapabilityId::new(*name).expect("capability")),
        )
        .expect("capability set")
    }

    #[test]
    fn capability_negotiation_fails_when_remote_policy_requires_datagrams() {
        let base = capabilities(&[CAPABILITY_TRANSPORT_QUIC, CAPABILITY_RELIABLE_STREAM]);
        let remote_supported = base.clone();
        let remote_required = capabilities(&[
            CAPABILITY_TRANSPORT_QUIC,
            CAPABILITY_RELIABLE_STREAM,
            CAPABILITY_ENCRYPTED_DATAGRAM,
        ]);
        assert_eq!(
            negotiate_capabilities(&base, &base, &remote_supported, &remote_required),
            Err(HandshakeRejectReason::CapabilityMismatch)
        );
    }

    #[test]
    fn capability_negotiation_selects_the_exact_bounded_intersection() {
        let local_supported = capabilities(&[
            CAPABILITY_TRANSPORT_QUIC,
            CAPABILITY_RELIABLE_STREAM,
            CAPABILITY_ENCRYPTED_DATAGRAM,
        ]);
        let remote_supported =
            capabilities(&[CAPABILITY_TRANSPORT_QUIC, CAPABILITY_RELIABLE_STREAM]);
        let required = remote_supported.clone();
        assert_eq!(
            negotiate_capabilities(&local_supported, &required, &remote_supported, &required,),
            Ok(required)
        );
    }
}
