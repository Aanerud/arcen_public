//! Process-local direct-session resume key, bindings, and one-slot registry.

use std::fmt;
use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};
use std::task::{Context, Poll};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use arcen_identity::{
    sign_direct_resume_grant, validate_direct_resume_grant_candidate, ActiveHostSessionId,
    DeckHolderNonce, DirectResumeBindingContext, DirectResumeError, DirectResumeGrantClaims,
    DirectResumeGrantSigner, DirectResumeGrantToken, DirectResumeGrantVerifier, DirectResumeNonce,
    DirectResumeValidationError, DisclaimerDigest, DisclaimerVersion, HostIdentity,
    NativePrincipal,
};
#[cfg(test)]
use arcen_identity::{validate_direct_resume_grant, DirectResumeValidationContext};
use arcen_protocol::messages::{AuthResponse, ResumeErrorCode, AUTH_METHOD_RESUME};
use arcen_session::direct_reconnect::{
    DirectResumeSlot, DirectResumeSlotResult, MonotonicMillis, ReconnectPolicy,
};
use arcen_transport::quic::DirectQuicStream;
use arcen_transport::{ConnectionId, DirectResumeTransportBinding};
use futures_util::{Sink, Stream};
use ring::hmac;
use sha2::{Digest, Sha256};
#[cfg(feature = "wss-compat")]
use tokio::net::TcpStream;
use tokio::sync::mpsc;
#[cfg(feature = "wss-compat")]
use tokio_rustls::server::TlsStream;
use tokio_tungstenite::{
    tungstenite::{Error as WebSocketError, Message},
    WebSocketStream,
};
use zeroize::Zeroize;

#[cfg(feature = "wss-compat")]
type DirectWssSocket = WebSocketStream<TlsStream<TcpStream>>;
type DirectQuicSocket = WebSocketStream<DirectQuicStream>;

pub(crate) enum DirectSessionSocket {
    #[cfg(feature = "wss-compat")]
    Wss(DirectWssSocket),
    Quic(DirectQuicSocket),
}

impl DirectSessionSocket {
    #[cfg(feature = "wss-compat")]
    pub(crate) const fn wss(socket: DirectWssSocket) -> Self {
        Self::Wss(socket)
    }

    pub(crate) const fn quic(socket: DirectQuicSocket) -> Self {
        Self::Quic(socket)
    }

    pub(crate) const fn transport_capability(&self) -> &'static str {
        match self {
            #[cfg(feature = "wss-compat")]
            Self::Wss(_) => arcen_protocol::CAPABILITY_TRANSPORT_WSS,
            Self::Quic(_) => arcen_protocol::CAPABILITY_TRANSPORT_QUIC,
        }
    }
}

impl Sink<Message> for DirectSessionSocket {
    type Error = WebSocketError;

    fn poll_ready(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_ready(context),
            Self::Quic(socket) => Pin::new(socket).poll_ready(context),
        }
    }

    fn start_send(self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).start_send(item),
            Self::Quic(socket) => Pin::new(socket).start_send(item),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_flush(context),
            Self::Quic(socket) => Pin::new(socket).poll_flush(context),
        }
    }

    fn poll_close(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_close(context),
            Self::Quic(socket) => Pin::new(socket).poll_close(context),
        }
    }
}

impl Stream for DirectSessionSocket {
    type Item = Result<Message, WebSocketError>;

    fn poll_next(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.get_mut() {
            #[cfg(feature = "wss-compat")]
            Self::Wss(socket) => Pin::new(socket).poll_next(context),
            Self::Quic(socket) => Pin::new(socket).poll_next(context),
        }
    }
}

const NO_DISCLAIMER_SHA256: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
const NO_DISCLAIMER_VERSION: &str = "none-v1";

pub(crate) trait ResumeClock: Send + Sync {
    fn monotonic_millis(&self) -> MonotonicMillis;
    fn epoch_seconds(&self) -> Result<u64, ResumeRegistryError>;
}

pub(crate) struct SystemResumeClock {
    origin: Instant,
}

impl Default for SystemResumeClock {
    fn default() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl ResumeClock for SystemResumeClock {
    fn monotonic_millis(&self) -> MonotonicMillis {
        MonotonicMillis::new(self.origin.elapsed().as_millis())
    }

    fn epoch_seconds(&self) -> Result<u64, ResumeRegistryError> {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs())
            .map_err(|_| ResumeRegistryError::Clock)
    }
}

/// A process-lifetime HMAC key. The raw bytes are never formatted and are
/// scrubbed when the broker-owned registry is dropped.
struct RingResumeKey {
    bytes: [u8; 32],
}

impl RingResumeKey {
    fn generate() -> Result<Self, ResumeRegistryError> {
        let mut bytes = [0_u8; 32];
        getrandom::getrandom(&mut bytes).map_err(|_| ResumeRegistryError::Randomness)?;
        Ok(Self { bytes })
    }
}

impl Drop for RingResumeKey {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

impl DirectResumeGrantSigner for RingResumeKey {
    type Error = ResumeRegistryError;

    fn sign(&self, canonical_signing_bytes: &[u8]) -> Result<[u8; 32], Self::Error> {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.bytes);
        let tag = hmac::sign(&key, canonical_signing_bytes);
        tag.as_ref()
            .try_into()
            .map_err(|_| ResumeRegistryError::Crypto)
    }
}

impl DirectResumeGrantVerifier for RingResumeKey {
    type Error = ResumeRegistryError;

    fn verify(
        &self,
        canonical_signing_bytes: &[u8],
        signature: &[u8; 32],
    ) -> Result<(), Self::Error> {
        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.bytes);
        hmac::verify(&key, canonical_signing_bytes, signature)
            .map_err(|_| ResumeRegistryError::Crypto)
    }
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct TopologyBinding([u8; 32]);

impl fmt::Debug for TopologyBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("TopologyBinding(<redacted>)")
    }
}

impl TopologyBinding {
    pub(crate) fn from_response(response: &AuthResponse) -> Result<Self, ResumeRegistryError> {
        #[derive(serde::Serialize)]
        struct Topology<'a> {
            screen_width: u32,
            screen_height: u32,
            monitors: &'a [arcen_protocol::messages::ClientMonitor],
            displays_mode: &'a str,
            timezone: &'a Option<String>,
            cursor_preference: arcen_protocol::messages::CursorMode,
        }

        let bytes = serde_json::to_vec(&Topology {
            screen_width: response.screen_width,
            screen_height: response.screen_height,
            monitors: &response.monitors,
            displays_mode: &response.displays_mode,
            timezone: &response.timezone,
            cursor_preference: response.cursor_preference,
        })
        .map_err(|_| ResumeRegistryError::Topology)?;
        Ok(Self(Sha256::digest(bytes).into()))
    }
}

#[derive(Clone)]
pub(crate) struct ResumeBindings {
    pub(crate) host_identity: HostIdentity,
    pub(crate) active_session_id: ActiveHostSessionId,
    pub(crate) native_principal: NativePrincipal,
    pub(crate) holder_nonce: DeckHolderNonce,
    pub(crate) disclaimer_digest: DisclaimerDigest,
    pub(crate) disclaimer_version: DisclaimerVersion,
    pub(crate) topology: TopologyBinding,
}

impl fmt::Debug for ResumeBindings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeBindings")
            .field("host_identity", &self.host_identity)
            .field("active_session_id", &self.active_session_id)
            .field("native_principal", &"<redacted>")
            .field("holder_nonce", &"<redacted>")
            .field("disclaimer_digest", &self.disclaimer_digest)
            .field("disclaimer_version", &self.disclaimer_version)
            .field("topology", &self.topology)
            .finish()
    }
}

pub(crate) struct ResumeHandoff {
    pub(crate) socket: DirectSessionSocket,
    pub(crate) session_log_id: arcen_telemetry::CorrelationId,
    pub(crate) previous_session_log_id: String,
    pub(crate) successor_grant: DirectResumeGrantToken,
    pub(crate) window_secs: u32,
}

pub(crate) struct GrantRefresh {
    pub(crate) grant: DirectResumeGrantToken,
    pub(crate) window_secs: u32,
}

impl fmt::Debug for GrantRefresh {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GrantRefresh")
            .field("grant", &"<redacted>")
            .field("window_secs", &self.window_secs)
            .finish()
    }
}

pub(crate) struct ResumePermit {
    owner: mpsc::UnboundedSender<OwnerCommand>,
    active_session_id: ActiveHostSessionId,
    previous_session_log_id: String,
    successor_grant: DirectResumeGrantToken,
    window_secs: u32,
}

impl fmt::Debug for ResumePermit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumePermit")
            .field("active_session_id", &self.active_session_id)
            .field("previous_session_log_id", &self.previous_session_log_id)
            .field("successor_grant", &"<redacted>")
            .field("window_secs", &self.window_secs)
            .finish()
    }
}

impl fmt::Debug for ResumeHandoff {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeHandoff")
            .field("socket", &"<direct-wss>")
            .field("session_log_id", &self.session_log_id)
            .field("previous_session_log_id", &self.previous_session_log_id)
            .field("successor_grant", &"<redacted>")
            .field("window_secs", &self.window_secs)
            .finish()
    }
}

#[derive(Debug)]
pub(crate) enum OwnerCommand {
    Resume(Box<ResumeHandoff>),
    Terminal,
    BrokerShutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryState {
    Attached,
    Detached,
    Resuming,
    Draining,
}

struct RegistryEntry {
    bindings: ResumeBindings,
    policy: ReconnectPolicy,
    slot: DirectResumeSlot,
    state: EntryState,
    owner: mpsc::UnboundedSender<OwnerCommand>,
    previous_session_log_id: String,
}

pub(crate) struct ResumeRegistry {
    key: RingResumeKey,
    clock: Arc<dyn ResumeClock>,
    entry: Mutex<Option<RegistryEntry>>,
}

impl fmt::Debug for ResumeRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeRegistry")
            .field("key", &"<redacted>")
            .field("entry", &"<one-slot>")
            .finish()
    }
}

impl ResumeRegistry {
    pub(crate) fn new() -> Result<Arc<Self>, ResumeRegistryError> {
        Self::with_clock(Arc::new(SystemResumeClock::default()))
    }

    fn with_clock(clock: Arc<dyn ResumeClock>) -> Result<Arc<Self>, ResumeRegistryError> {
        Ok(Arc::new(Self {
            key: RingResumeKey::generate()?,
            clock,
            entry: Mutex::new(None),
        }))
    }

    pub(crate) fn monotonic_now(&self) -> MonotonicMillis {
        self.clock.monotonic_millis()
    }

    pub(crate) fn resume_handshake_available(&self) -> Result<bool, ResumeRegistryError> {
        Ok(self
            .lock_entry()?
            .as_ref()
            .map(|entry| entry.state)
            .is_some_and(|state| matches!(state, EntryState::Detached | EntryState::Resuming)))
    }

    pub(crate) fn issue_initial(
        &self,
        bindings: ResumeBindings,
        policy: ReconnectPolicy,
        owner: mpsc::UnboundedSender<OwnerCommand>,
        session_log_id: &arcen_telemetry::CorrelationId,
    ) -> Result<DirectResumeGrantToken, ResumeRegistryError> {
        if policy.is_disabled() {
            return Err(ResumeRegistryError::Disabled);
        }
        let nonce = fresh_nonce()?;
        let now = self.clock.epoch_seconds()?;
        let token = self.sign_claims(&bindings, 1, nonce, policy, now)?;
        let mut entry = self.lock_entry()?;
        if entry.is_some() {
            return Err(ResumeRegistryError::Busy);
        }
        *entry = Some(RegistryEntry {
            bindings,
            policy,
            slot: DirectResumeSlot::new(1, nonce),
            state: EntryState::Attached,
            owner,
            previous_session_log_id: session_log_id.to_string(),
        });
        Ok(token)
    }

    pub(crate) fn mark_detached(
        &self,
        active_session_id: &ActiveHostSessionId,
    ) -> Result<(), ResumeRegistryError> {
        let mut guard = self.lock_entry()?;
        let entry = guard
            .as_mut()
            .ok_or(ResumeRegistryError::SessionUnavailable)?;
        if entry.bindings.active_session_id != *active_session_id {
            return Err(ResumeRegistryError::SessionMismatch);
        }
        if entry.state == EntryState::Draining {
            return Err(ResumeRegistryError::SlotUnavailable);
        }
        entry.state = EntryState::Detached;
        Ok(())
    }

    pub(crate) fn mark_attached(
        &self,
        active_session_id: &ActiveHostSessionId,
    ) -> Result<(), ResumeRegistryError> {
        let mut guard = self.lock_entry()?;
        let entry = guard
            .as_mut()
            .ok_or(ResumeRegistryError::SessionUnavailable)?;
        if entry.bindings.active_session_id != *active_session_id {
            return Err(ResumeRegistryError::SessionMismatch);
        }
        if entry.state == EntryState::Draining {
            return Err(ResumeRegistryError::SlotUnavailable);
        }
        entry.state = EntryState::Attached;
        Ok(())
    }

    pub(crate) fn refresh_grant(
        &self,
        active_session_id: &ActiveHostSessionId,
    ) -> Result<GrantRefresh, ResumeRegistryError> {
        let mut guard = self.lock_entry()?;
        let entry = guard
            .as_mut()
            .ok_or(ResumeRegistryError::SessionUnavailable)?;
        if entry.bindings.active_session_id != *active_session_id {
            return Err(ResumeRegistryError::SessionMismatch);
        }
        if entry.state != EntryState::Attached {
            return Err(ResumeRegistryError::NotAttached);
        }
        let now = match self.clock.epoch_seconds() {
            Ok(now) => now,
            Err(error) => {
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                return Err(error);
            }
        };
        let Some((generation, nonce)) = entry.slot.current() else {
            entry.state = EntryState::Draining;
            return Err(ResumeRegistryError::SlotUnavailable);
        };
        let next_nonce = match fresh_nonce() {
            Ok(nonce) => nonce,
            Err(error) => {
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                return Err(error);
            }
        };
        let (next_generation, next_nonce) = match entry
            .slot
            .compare_and_rotate(generation, &nonce, next_nonce)
        {
            DirectResumeSlotResult::Rotated { generation, nonce } => (generation, nonce),
            DirectResumeSlotResult::Replayed => {
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                return Err(ResumeRegistryError::SlotUnavailable);
            }
            DirectResumeSlotResult::GenerationExhausted => {
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                return Err(ResumeRegistryError::GenerationExhausted);
            }
        };
        let grant = match self.sign_claims(
            &entry.bindings,
            next_generation,
            next_nonce,
            entry.policy,
            now,
        ) {
            Ok(grant) => grant,
            Err(error) => {
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                return Err(error);
            }
        };
        Ok(GrantRefresh {
            grant,
            window_secs: entry.policy.window_secs(),
        })
    }

    pub(crate) fn begin_drain(
        &self,
        active_session_id: &ActiveHostSessionId,
    ) -> Result<(), ResumeRegistryError> {
        let mut guard = self.lock_entry()?;
        if let Some(entry) = guard.as_mut() {
            if entry.bindings.active_session_id != *active_session_id {
                return Err(ResumeRegistryError::SessionMismatch);
            }
            entry.slot.revoke();
            entry.state = EntryState::Draining;
        }
        Ok(())
    }

    pub(crate) fn complete_drain(
        &self,
        active_session_id: &ActiveHostSessionId,
    ) -> Result<(), ResumeRegistryError> {
        let mut entry = self.lock_entry()?;
        if entry
            .as_ref()
            .is_some_and(|entry| entry.bindings.active_session_id != *active_session_id)
        {
            return Err(ResumeRegistryError::SessionMismatch);
        }
        *entry = None;
        Ok(())
    }

    pub(crate) fn shutdown(&self) -> Result<(), ResumeRegistryError> {
        let owner = {
            let mut guard = self.lock_entry()?;
            guard.as_mut().map(|entry| {
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                entry.owner.clone()
            })
        };
        if let Some(owner) = owner {
            let _ = owner.send(OwnerCommand::BrokerShutdown);
        }
        Ok(())
    }

    pub(crate) fn prepare_resume(
        &self,
        response: &AuthResponse,
        current_host_identity: &HostIdentity,
        session_log_id: &arcen_telemetry::CorrelationId,
    ) -> Result<ResumePermit, ResumeRejection> {
        if response.method != AUTH_METHOD_RESUME
            || !response.username.is_empty()
            || !response.credential.is_empty()
            || response.resume_requested
        {
            return Err(ResumeRejection::new(
                ResumeErrorCode::Unsupported,
                "resume authentication shape is invalid",
                false,
            ));
        }
        let holder_nonce = response
            .resume_holder_nonce
            .as_deref()
            .and_then(decode_nonce)
            .map(DeckHolderNonce::new)
            .ok_or_else(|| {
                ResumeRejection::new(
                    ResumeErrorCode::Replayed,
                    "resume grant was rejected",
                    false,
                )
            })?;
        let token =
            DirectResumeGrantToken::parse(response.resume_grant.clone().unwrap_or_default())
                .map_err(|_| {
                    ResumeRejection::new(
                        ResumeErrorCode::Replayed,
                        "resume grant was rejected",
                        false,
                    )
                })?;
        let topology = TopologyBinding::from_response(response).map_err(|_| {
            ResumeRejection::new(
                ResumeErrorCode::TopologyChanged,
                "resume topology changed",
                true,
            )
        })?;
        let now = self.clock.epoch_seconds().map_err(|_| {
            ResumeRejection::new(
                ResumeErrorCode::InternalFailure,
                "resume validation failed",
                false,
            )
        })?;

        {
            let mut guard = self.lock_entry().map_err(|_| {
                ResumeRejection::new(
                    ResumeErrorCode::InternalFailure,
                    "resume validation failed",
                    false,
                )
            })?;
            let entry = guard.as_mut().ok_or_else(|| {
                ResumeRejection::new(
                    ResumeErrorCode::SessionGone,
                    "resumable session is unavailable",
                    false,
                )
            })?;
            let Some((generation, nonce)) = entry.slot.current() else {
                return Err(ResumeRejection::new(
                    ResumeErrorCode::Replayed,
                    "resume grant was already used",
                    false,
                ));
            };
            if holder_nonce != entry.bindings.holder_nonce {
                return Err(ResumeRejection::new(
                    ResumeErrorCode::Replayed,
                    "resume grant was already used",
                    false,
                ));
            }
            let context = DirectResumeBindingContext {
                expected_host_identity: &entry.bindings.host_identity,
                expected_active_session_id: &entry.bindings.active_session_id,
                expected_native_principal: &entry.bindings.native_principal,
                expected_holder_nonce: entry.bindings.holder_nonce,
                expected_disclaimer_digest: entry.bindings.disclaimer_digest,
                expected_disclaimer_version: &entry.bindings.disclaimer_version,
                now_epoch_seconds: now,
            };
            let validated = validate_direct_resume_grant_candidate(&self.key, &token, context)
                .map_err(map_validation_rejection)?;
            DirectResumeTransportBinding::from_validated_grant(
                ConnectionId::new(session_log_id.to_string()),
                &validated,
            )
            .and_then(|binding| binding.validate_grant(&validated))
            .map_err(|_| {
                ResumeRejection::new(
                    ResumeErrorCode::TopologyChanged,
                    "resume topology changed",
                    true,
                )
            })?;
            let candidate = validated.claims();
            let exact_predecessor = candidate.generation().checked_add(1) == Some(generation);
            if !exact_predecessor && candidate.generation() != generation {
                return Err(map_validation_rejection(
                    DirectResumeValidationError::Claims(DirectResumeError::GenerationMismatch),
                ));
            }
            if !exact_predecessor && candidate.nonce() != DirectResumeNonce::new(nonce) {
                return Err(map_validation_rejection(
                    DirectResumeValidationError::Claims(DirectResumeError::NonceMismatch),
                ));
            }
            if entry.state != EntryState::Detached {
                return Err(ResumeRejection::new(
                    ResumeErrorCode::Replayed,
                    "resume grant was already used",
                    false,
                ));
            }
            if exact_predecessor {
                if entry.bindings.host_identity != *current_host_identity
                    || entry.bindings.topology != topology
                {
                    return Err(ResumeRejection::new(
                        ResumeErrorCode::TopologyChanged,
                        "resume topology changed",
                        false,
                    ));
                }
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                let owner = entry.owner.clone();
                return Err(ResumeRejection::with_owner(
                    ResumeErrorCode::Replayed,
                    "resume grant was already used",
                    owner,
                ));
            }
            if entry.bindings.host_identity != *current_host_identity
                || entry.bindings.topology != topology
            {
                entry.slot.revoke();
                entry.state = EntryState::Draining;
                let owner = entry.owner.clone();
                return Err(ResumeRejection::with_owner(
                    ResumeErrorCode::TopologyChanged,
                    "resume topology changed",
                    owner,
                ));
            }

            let next_nonce = fresh_nonce().map_err(|_| {
                ResumeRejection::new(
                    ResumeErrorCode::InternalFailure,
                    "resume validation failed",
                    false,
                )
            })?;
            let (next_generation, next_nonce) = match entry
                .slot
                .compare_and_rotate(generation, &nonce, next_nonce)
            {
                DirectResumeSlotResult::Rotated { generation, nonce } => (generation, nonce),
                DirectResumeSlotResult::Replayed => {
                    return Err(ResumeRejection::new(
                        ResumeErrorCode::Replayed,
                        "resume grant was already used",
                        false,
                    ));
                }
                DirectResumeSlotResult::GenerationExhausted => {
                    entry.state = EntryState::Draining;
                    return Err(ResumeRejection::new(
                        ResumeErrorCode::InternalFailure,
                        "resume validation failed",
                        true,
                    ));
                }
            };
            let successor = match self.sign_claims(
                &entry.bindings,
                next_generation,
                next_nonce,
                entry.policy,
                now,
            ) {
                Ok(successor) => successor,
                Err(_) => {
                    entry.slot.revoke();
                    entry.state = EntryState::Draining;
                    let owner = entry.owner.clone();
                    return Err(ResumeRejection::with_owner(
                        ResumeErrorCode::InternalFailure,
                        "resume validation failed",
                        owner,
                    ));
                }
            };
            entry.state = EntryState::Resuming;
            let previous_session_log_id = std::mem::replace(
                &mut entry.previous_session_log_id,
                session_log_id.to_string(),
            );
            let owner = entry.owner.clone();
            Ok(ResumePermit {
                owner,
                active_session_id: entry.bindings.active_session_id.clone(),
                previous_session_log_id,
                successor_grant: successor,
                window_secs: entry.policy.window_secs(),
            })
        }
    }

    pub(crate) fn handoff(
        &self,
        permit: ResumePermit,
        socket: DirectSessionSocket,
        session_log_id: arcen_telemetry::CorrelationId,
    ) -> Result<(), (Box<DirectSessionSocket>, ResumeRejection)> {
        let command = OwnerCommand::Resume(Box::new(ResumeHandoff {
            socket,
            session_log_id,
            previous_session_log_id: permit.previous_session_log_id,
            successor_grant: permit.successor_grant,
            window_secs: permit.window_secs,
        }));
        let active_session_id = permit.active_session_id.clone();
        permit.owner.send(command).map_err(|error| {
            if let Err(drain_error) = self.begin_drain(&active_session_id) {
                tracing::error!(
                    target: crate::logging::SESSION,
                    ?drain_error,
                    "resume handoff owner vanished and registry drain failed"
                );
            }
            let OwnerCommand::Resume(handoff) = error.0 else {
                unreachable!("resume handoff returned a different owner command")
            };
            (
                Box::new(handoff.socket),
                ResumeRejection::new(
                    ResumeErrorCode::SessionGone,
                    "resumable session is unavailable",
                    true,
                ),
            )
        })
    }

    fn sign_claims(
        &self,
        bindings: &ResumeBindings,
        generation: u64,
        nonce: [u8; 32],
        policy: ReconnectPolicy,
        now: u64,
    ) -> Result<DirectResumeGrantToken, ResumeRegistryError> {
        let lifetime = u64::from(policy.window_secs())
            .checked_mul(2)
            .ok_or(ResumeRegistryError::Clock)?;
        let expires_at = now
            .checked_add(lifetime)
            .ok_or(ResumeRegistryError::Clock)?;
        let claims = DirectResumeGrantClaims::new(
            bindings.host_identity.clone(),
            bindings.active_session_id.clone(),
            bindings.native_principal.clone(),
            bindings.holder_nonce,
            generation,
            DirectResumeNonce::new(nonce),
            bindings.disclaimer_digest,
            bindings.disclaimer_version.clone(),
            now,
            expires_at,
        )
        .map_err(|_| ResumeRegistryError::Claims)?;
        sign_direct_resume_grant(&self.key, claims, now).map_err(|_| ResumeRegistryError::Crypto)
    }

    fn lock_entry(&self) -> Result<MutexGuard<'_, Option<RegistryEntry>>, ResumeRegistryError> {
        self.entry.lock().map_err(|_| ResumeRegistryError::Lock)
    }
}

fn map_validation_rejection(
    error: DirectResumeValidationError<ResumeRegistryError>,
) -> ResumeRejection {
    match error {
        DirectResumeValidationError::Claims(DirectResumeError::Expired) => {
            ResumeRejection::new(ResumeErrorCode::Expired, "resume grant expired", false)
        }
        DirectResumeValidationError::Claims(
            DirectResumeError::GenerationMismatch
            | DirectResumeError::NonceMismatch
            | DirectResumeError::HolderNonceMismatch,
        ) => ResumeRejection::new(
            ResumeErrorCode::Replayed,
            "resume grant was already used",
            false,
        ),
        DirectResumeValidationError::Claims(DirectResumeError::HostIdentityMismatch) => {
            ResumeRejection::new(
                ResumeErrorCode::TopologyChanged,
                "resume topology changed",
                true,
            )
        }
        DirectResumeValidationError::Claims(DirectResumeError::ActiveSessionMismatch) => {
            ResumeRejection::new(
                ResumeErrorCode::SessionGone,
                "resumable session is unavailable",
                true,
            )
        }
        DirectResumeValidationError::Claims(DirectResumeError::NativePrincipalMismatch) => {
            ResumeRejection::new(
                ResumeErrorCode::NativeIdentityChanged,
                "native session identity changed",
                true,
            )
        }
        DirectResumeValidationError::Claims(
            DirectResumeError::DisclaimerDigestMismatch
            | DirectResumeError::DisclaimerVersionMismatch,
        ) => ResumeRejection::new(
            ResumeErrorCode::InternalFailure,
            "resume policy binding changed",
            true,
        ),
        DirectResumeValidationError::Claims(_) | DirectResumeValidationError::Signature(_) => {
            ResumeRejection::new(
                ResumeErrorCode::Replayed,
                "resume grant was rejected",
                false,
            )
        }
    }
}

pub(crate) struct ResumeRejection {
    pub(crate) code: ResumeErrorCode,
    pub(crate) message: &'static str,
    pub(crate) terminal: bool,
    owner: Option<mpsc::UnboundedSender<OwnerCommand>>,
}

impl fmt::Debug for ResumeRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResumeRejection")
            .field("code", &self.code)
            .field("message", &self.message)
            .field("terminal", &self.terminal)
            .finish()
    }
}

impl ResumeRejection {
    fn new(code: ResumeErrorCode, message: &'static str, terminal: bool) -> Self {
        Self {
            code,
            message,
            terminal,
            owner: None,
        }
    }

    fn with_owner(
        code: ResumeErrorCode,
        message: &'static str,
        owner: mpsc::UnboundedSender<OwnerCommand>,
    ) -> Self {
        Self {
            code,
            message,
            terminal: true,
            owner: Some(owner),
        }
    }

    pub(crate) fn notify_terminal_owner(&self) {
        if let Some(owner) = &self.owner {
            let _ = owner.send(OwnerCommand::Terminal);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumeRegistryError {
    Randomness,
    Clock,
    Crypto,
    Claims,
    Topology,
    Lock,
    Busy,
    Disabled,
    SessionUnavailable,
    SessionMismatch,
    NotAttached,
    SlotUnavailable,
    GenerationExhausted,
    SuccessorDelivery,
}

fn fresh_nonce() -> Result<[u8; 32], ResumeRegistryError> {
    let mut nonce = [0_u8; 32];
    getrandom::getrandom(&mut nonce).map_err(|_| ResumeRegistryError::Randomness)?;
    Ok(nonce)
}

pub(crate) fn decode_holder_nonce(value: &str) -> Option<DeckHolderNonce> {
    decode_nonce(value).map(DeckHolderNonce::new)
}

fn decode_nonce(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64 {
        return None;
    }
    let mut bytes = [0_u8; 32];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        bytes[index] = (hex_nibble(pair[0])? << 4) | hex_nibble(pair[1])?;
    }
    Some(bytes)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

pub(crate) fn no_disclaimer_binding(
) -> Result<(DisclaimerDigest, DisclaimerVersion), ResumeRegistryError> {
    Ok((
        DisclaimerDigest::parse_lower_hex(NO_DISCLAIMER_SHA256)
            .map_err(|_| ResumeRegistryError::Claims)?,
        DisclaimerVersion::new(NO_DISCLAIMER_VERSION).map_err(|_| ResumeRegistryError::Claims)?,
    ))
}

pub(crate) fn disclaimer_binding(
    disclaimer: Option<&arcen_identity::PreparedDisclaimer>,
) -> Result<(DisclaimerDigest, DisclaimerVersion), ResumeRegistryError> {
    match disclaimer {
        Some(disclaimer) => Ok((
            disclaimer.digest(),
            DisclaimerVersion::new(disclaimer.digest().to_lower_hex())
                .map_err(|_| ResumeRegistryError::Claims)?,
        )),
        None => no_disclaimer_binding(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct FakeClock {
        epoch: AtomicU64,
    }

    impl FakeClock {
        fn new(epoch: u64) -> Arc<Self> {
            Arc::new(Self {
                epoch: AtomicU64::new(epoch),
            })
        }

        fn set(&self, epoch: u64) {
            self.epoch.store(epoch, Ordering::Release);
        }
    }

    impl ResumeClock for FakeClock {
        fn monotonic_millis(&self) -> MonotonicMillis {
            MonotonicMillis::new(u128::from(self.epoch.load(Ordering::Acquire)) * 1_000)
        }

        fn epoch_seconds(&self) -> Result<u64, ResumeRegistryError> {
            Ok(self.epoch.load(Ordering::Acquire))
        }
    }

    fn bindings(suffix: &str) -> ResumeBindings {
        let (disclaimer_digest, disclaimer_version) = no_disclaimer_binding().unwrap();
        ResumeBindings {
            host_identity: HostIdentity::new(format!("spki-sha256:{suffix}")).unwrap(),
            active_session_id: ActiveHostSessionId::new("windows-wts:7").unwrap(),
            native_principal: NativePrincipal::Windows {
                sid: arcen_identity::WindowsSid::new("S-1-5-21-1-2-3-1001").unwrap(),
                wts_session_id: 7,
            },
            holder_nonce: DeckHolderNonce::new([3; 32]),
            disclaimer_digest,
            disclaimer_version,
            topology: TopologyBinding([9; 32]),
        }
    }

    #[test]
    fn ring_hmac_rejects_tamper_and_debug_never_contains_key() {
        let key = RingResumeKey { bytes: [0x5a; 32] };
        let signature = key.sign(b"canonical").unwrap();
        assert!(key.verify(b"canonical", &signature).is_ok());
        assert!(key.verify(b"tampered", &signature).is_err());
        let registry = ResumeRegistry {
            key,
            clock: FakeClock::new(1_000),
            entry: Mutex::new(None),
        };
        let debug = format!("{registry:?}");
        assert!(!debug.contains("5a"));
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn token_validation_binds_host_sid_wts_session_holder_disclaimer_and_time() {
        let clock = FakeClock::new(1_000);
        let registry = ResumeRegistry::with_clock(clock.clone()).unwrap();
        let expected = bindings("host-a");
        let policy = ReconnectPolicy::new(60).unwrap();
        let nonce = [4; 32];
        let token = registry
            .sign_claims(&expected, 8, nonce, policy, 1_000)
            .unwrap();

        let validate = |bindings: &ResumeBindings, holder: DeckHolderNonce, now| {
            validate_direct_resume_grant(
                &registry.key,
                &token,
                DirectResumeValidationContext {
                    expected_host_identity: &bindings.host_identity,
                    expected_active_session_id: &bindings.active_session_id,
                    expected_native_principal: &bindings.native_principal,
                    expected_holder_nonce: holder,
                    expected_generation: 8,
                    expected_nonce: DirectResumeNonce::new(nonce),
                    expected_disclaimer_digest: bindings.disclaimer_digest,
                    expected_disclaimer_version: &bindings.disclaimer_version,
                    now_epoch_seconds: now,
                },
            )
        };
        assert!(validate(&expected, expected.holder_nonce, 1_000).is_ok());
        let mut tampered = token.expose_for_transport().as_bytes().to_vec();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let tampered = DirectResumeGrantToken::parse(String::from_utf8(tampered).unwrap()).unwrap();
        assert!(matches!(
            validate_direct_resume_grant(
                &registry.key,
                &tampered,
                DirectResumeValidationContext {
                    expected_host_identity: &expected.host_identity,
                    expected_active_session_id: &expected.active_session_id,
                    expected_native_principal: &expected.native_principal,
                    expected_holder_nonce: expected.holder_nonce,
                    expected_generation: 8,
                    expected_nonce: DirectResumeNonce::new(nonce),
                    expected_disclaimer_digest: expected.disclaimer_digest,
                    expected_disclaimer_version: &expected.disclaimer_version,
                    now_epoch_seconds: 1_000,
                },
            ),
            Err(DirectResumeValidationError::Signature(_))
                | Err(DirectResumeValidationError::Claims(_))
        ));

        let changed_host = bindings("host-b");
        assert!(matches!(
            validate(&changed_host, expected.holder_nonce, 1_000),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::HostIdentityMismatch
            ))
        ));
        let mut changed_principal = expected.clone();
        changed_principal.native_principal = NativePrincipal::Windows {
            sid: arcen_identity::WindowsSid::new("S-1-5-21-1-2-3-1002").unwrap(),
            wts_session_id: 7,
        };
        assert!(matches!(
            validate(&changed_principal, expected.holder_nonce, 1_000),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::NativePrincipalMismatch
            ))
        ));
        changed_principal.native_principal = NativePrincipal::Windows {
            sid: arcen_identity::WindowsSid::new("S-1-5-21-1-2-3-1001").unwrap(),
            wts_session_id: 8,
        };
        assert!(matches!(
            validate(&changed_principal, expected.holder_nonce, 1_000),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::NativePrincipalMismatch
            ))
        ));
        let mut changed_session = expected.clone();
        changed_session.active_session_id = ActiveHostSessionId::new("windows-wts:8").unwrap();
        assert!(matches!(
            validate(&changed_session, expected.holder_nonce, 1_000),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::ActiveSessionMismatch
            ))
        ));
        assert!(matches!(
            validate(&expected, DeckHolderNonce::new([5; 32]), 1_000),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::HolderNonceMismatch
            ))
        ));
        let mut changed_disclaimer = expected.clone();
        changed_disclaimer.disclaimer_version = DisclaimerVersion::new("changed").unwrap();
        assert!(matches!(
            validate(&changed_disclaimer, expected.holder_nonce, 1_000),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::DisclaimerVersionMismatch
            ))
        ));
        changed_disclaimer = expected.clone();
        changed_disclaimer.disclaimer_digest =
            DisclaimerDigest::parse_lower_hex(&"01".repeat(32)).unwrap();
        assert!(matches!(
            validate(&changed_disclaimer, expected.holder_nonce, 1_000),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::DisclaimerDigestMismatch
            ))
        ));
        assert!(validate(&expected, expected.holder_nonce, 1_060).is_ok());
        clock.set(1_120);
        assert!(matches!(
            validate(&expected, expected.holder_nonce, 1_120),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::Expired
            ))
        ));
    }

    #[test]
    fn direct_slot_has_exactly_one_concurrent_winner() {
        let slot = Arc::new(Mutex::new(DirectResumeSlot::new(9, [7; 32])));
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut threads = Vec::new();
        for index in 0..8_u8 {
            let slot = Arc::clone(&slot);
            let barrier = Arc::clone(&barrier);
            let winners = Arc::clone(&winners);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                if matches!(
                    slot.lock()
                        .unwrap()
                        .compare_and_rotate(9, &[7; 32], [index; 32]),
                    DirectResumeSlotResult::Rotated { .. }
                ) {
                    winners.fetch_add(1, Ordering::AcqRel);
                }
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(winners.load(Ordering::Acquire), 1);
    }

    #[test]
    fn lost_successor_predecessor_is_terminal_then_cleanup_reopens_registration() {
        let clock = FakeClock::new(2_000);
        let registry = ResumeRegistry::with_clock(clock).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, mut commands) = mpsc::unbounded_channel();
        let grant = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::parse_uuid(
                    "00000000-0000-4000-8000-000000000001".to_string(),
                )
                .unwrap(),
            )
            .unwrap();
        response.resume_grant = Some(grant.expose_for_transport().to_string());
        registry.mark_detached(&active_session_id).unwrap();
        assert!(registry.resume_handshake_available().unwrap());
        let attempt_id = arcen_telemetry::CorrelationId::parse_uuid(
            "00000000-0000-4000-8000-000000000002".to_string(),
        )
        .unwrap();
        let permit = registry
            .prepare_resume(&response, &expected.host_identity, &attempt_id)
            .unwrap();
        assert!(!permit.successor_grant.expose_for_transport().is_empty());
        assert_eq!(permit.window_secs, 60);
        registry.mark_detached(&active_session_id).unwrap();

        let replay = registry
            .prepare_resume(&response, &expected.host_identity, &attempt_id)
            .unwrap_err();
        assert_eq!(replay.code, ResumeErrorCode::Replayed);
        assert!(replay.terminal);
        replay.notify_terminal_owner();
        assert!(matches!(commands.try_recv(), Ok(OwnerCommand::Terminal)));
        {
            let guard = registry.entry.lock().unwrap();
            let entry = guard.as_ref().unwrap();
            assert_eq!(entry.state, EntryState::Draining);
            assert!(entry.slot.current().is_none());
        }

        assert!(!registry.resume_handshake_available().unwrap());

        registry.complete_drain(&active_session_id).unwrap();
        let (new_owner, _new_commands) = mpsc::unbounded_channel();
        assert!(registry
            .issue_initial(
                expected,
                ReconnectPolicy::new(60).unwrap(),
                new_owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([3; 16]),
            )
            .is_ok());
    }

    #[test]
    fn exact_predecessor_cannot_drain_an_attached_session() {
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, mut commands) = mpsc::unbounded_channel();
        let predecessor = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        let _successor = registry.refresh_grant(&active_session_id).unwrap();
        response.resume_grant = Some(predecessor.expose_for_transport().to_string());

        let rejection = registry
            .prepare_resume(
                &response,
                &expected.host_identity,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .unwrap_err();
        assert_eq!(rejection.code, ResumeErrorCode::Replayed);
        assert!(!rejection.terminal);
        rejection.notify_terminal_owner();
        assert!(commands.try_recv().is_err());
        let entry = registry.entry.lock().unwrap();
        let entry = entry.as_ref().unwrap();
        assert_eq!(entry.state, EntryState::Attached);
        assert!(entry.slot.current().is_some());
    }

    #[test]
    fn token_older_than_exact_predecessor_cannot_drain() {
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, mut commands) = mpsc::unbounded_channel();
        let generation_one = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        let _generation_two = registry.refresh_grant(&active_session_id).unwrap();
        let _generation_three = registry.refresh_grant(&active_session_id).unwrap();
        response.resume_grant = Some(generation_one.expose_for_transport().to_string());
        registry.mark_detached(&active_session_id).unwrap();

        let rejection = registry
            .prepare_resume(
                &response,
                &expected.host_identity,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .unwrap_err();
        assert_eq!(rejection.code, ResumeErrorCode::Replayed);
        assert!(!rejection.terminal);
        rejection.notify_terminal_owner();
        assert!(commands.try_recv().is_err());
        let guard = registry.entry.lock().unwrap();
        let entry = guard.as_ref().unwrap();
        assert_eq!(entry.state, EntryState::Detached);
        assert_eq!(entry.slot.current().unwrap().0, 3);
    }

    #[test]
    fn concurrent_authenticated_predecessor_has_one_terminal_winner() {
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, mut commands) = mpsc::unbounded_channel();
        let predecessor = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        registry.refresh_grant(&active_session_id).unwrap();
        response.resume_grant = Some(predecessor.expose_for_transport().to_string());
        registry.mark_detached(&active_session_id).unwrap();

        let response = Arc::new(response);
        let host_identity = Arc::new(expected.host_identity);
        let barrier = Arc::new(std::sync::Barrier::new(9));
        let terminal_winners = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut threads = Vec::new();
        for index in 0..8_u8 {
            let registry = Arc::clone(&registry);
            let response = Arc::clone(&response);
            let host_identity = Arc::clone(&host_identity);
            let barrier = Arc::clone(&barrier);
            let terminal_winners = Arc::clone(&terminal_winners);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let rejection = registry
                    .prepare_resume(
                        &response,
                        &host_identity,
                        &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([index; 16]),
                    )
                    .unwrap_err();
                if rejection.terminal {
                    terminal_winners.fetch_add(1, Ordering::AcqRel);
                }
                rejection.notify_terminal_owner();
            }));
        }
        barrier.wait();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(terminal_winners.load(Ordering::Acquire), 1);
        assert!(matches!(commands.try_recv(), Ok(OwnerCommand::Terminal)));
        assert!(commands.try_recv().is_err());
    }

    #[test]
    fn detached_slot_accepts_retry_before_owner_polls_cleanup_channel() {
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, mut commands) = mpsc::unbounded_channel();
        let grant = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        response.resume_grant = Some(grant.expose_for_transport().to_string());

        registry.mark_detached(&active_session_id).unwrap();
        assert!(commands.try_recv().is_err());
        let permit = registry
            .prepare_resume(
                &response,
                &expected.host_identity,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .expect("valid retry must not be rejected as replay during detach cleanup");
        assert_eq!(permit.active_session_id, active_session_id);
    }

    #[test]
    fn display_update_then_transport_loss_retains_resume_authority_and_admission() {
        struct AdmissionLeaseProbe(Arc<AtomicBool>);

        impl Drop for AdmissionLeaseProbe {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        let admission_dropped = Arc::new(AtomicBool::new(false));
        let admission = AdmissionLeaseProbe(Arc::clone(&admission_dropped));
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, _commands) = mpsc::unbounded_channel();
        let grant = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        response.resume_grant = Some(grant.expose_for_transport().to_string());

        // A display_update is attachment-local: accepting it must not rotate or
        // revoke the broker-owned logical session or its admission.
        assert!(!admission_dropped.load(Ordering::Acquire));
        registry.mark_detached(&active_session_id).unwrap();
        let permit = registry
            .prepare_resume(
                &response,
                &expected.host_identity,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .expect("transport loss after display_update must remain resumable");

        assert_eq!(permit.active_session_id, active_session_id);
        assert!(!admission_dropped.load(Ordering::Acquire));
        drop(admission);
        assert!(admission_dropped.load(Ordering::Acquire));
    }

    #[test]
    fn attached_refresh_after_many_windows_rotates_once_with_two_window_lifetime() {
        let clock = FakeClock::new(2_000);
        let registry = ResumeRegistry::with_clock(clock.clone()).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, _commands) = mpsc::unbounded_channel();
        let old = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        response.resume_grant = Some(old.expose_for_transport().to_string());

        clock.set(10_000);
        let refresh = registry.refresh_grant(&active_session_id).unwrap();
        assert_eq!(refresh.window_secs, 60);
        let debug = format!("{refresh:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains(refresh.grant.expose_for_transport()));
        let (generation, nonce) = registry
            .entry
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .slot
            .current()
            .unwrap();
        let context = |now_epoch_seconds| DirectResumeValidationContext {
            expected_host_identity: &expected.host_identity,
            expected_active_session_id: &expected.active_session_id,
            expected_native_principal: &expected.native_principal,
            expected_holder_nonce: expected.holder_nonce,
            expected_generation: generation,
            expected_nonce: DirectResumeNonce::new(nonce),
            expected_disclaimer_digest: expected.disclaimer_digest,
            expected_disclaimer_version: &expected.disclaimer_version,
            now_epoch_seconds,
        };
        assert!(
            validate_direct_resume_grant(&registry.key, &refresh.grant, context(10_119)).is_ok()
        );
        assert!(matches!(
            validate_direct_resume_grant(&registry.key, &refresh.grant, context(10_120)),
            Err(DirectResumeValidationError::Claims(
                DirectResumeError::Expired
            ))
        ));

        registry.mark_detached(&active_session_id).unwrap();
        assert_eq!(
            registry.refresh_grant(&active_session_id).unwrap_err(),
            ResumeRegistryError::NotAttached
        );
        let rejection = registry
            .prepare_resume(
                &response,
                &expected.host_identity,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([2; 16]),
            )
            .unwrap_err();
        assert_eq!(rejection.code, ResumeErrorCode::Expired);
    }

    #[test]
    fn refresh_generation_exhaustion_revokes_and_drains() {
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let expected = bindings("host-a");
        let active_session_id = expected.active_session_id.clone();
        let (owner, _commands) = mpsc::unbounded_channel();
        registry
            .issue_initial(
                expected,
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        registry.entry.lock().unwrap().as_mut().unwrap().slot =
            DirectResumeSlot::new(u64::MAX, [9; 32]);

        assert_eq!(
            registry.refresh_grant(&active_session_id).unwrap_err(),
            ResumeRegistryError::GenerationExhausted
        );
        let guard = registry.entry.lock().unwrap();
        let entry = guard.as_ref().unwrap();
        assert_eq!(entry.state, EntryState::Draining);
        assert!(entry.slot.current().is_none());
    }

    #[test]
    fn tls_or_display_topology_mismatch_is_terminal() {
        for mismatch in ["tls", "display"] {
            let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
            let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
            let mut expected = bindings("host-a");
            expected.topology = TopologyBinding::from_response(&response).unwrap();
            let active_session_id = expected.active_session_id.clone();
            let (owner, mut commands) = mpsc::unbounded_channel();
            let grant = registry
                .issue_initial(
                    expected.clone(),
                    ReconnectPolicy::new(60).unwrap(),
                    owner,
                    &arcen_telemetry::CorrelationId::parse_uuid(
                        "00000000-0000-4000-8000-000000000001".to_string(),
                    )
                    .unwrap(),
                )
                .unwrap();
            response.resume_grant = Some(grant.expose_for_transport().to_string());
            if mismatch == "display" {
                response.screen_width = 1_280;
            }

            registry.mark_detached(&active_session_id).unwrap();
            let host = if mismatch == "tls" {
                HostIdentity::new("spki-sha256:host-b").unwrap()
            } else {
                expected.host_identity.clone()
            };
            let rejection = registry
                .prepare_resume(
                    &response,
                    &host,
                    &arcen_telemetry::CorrelationId::parse_uuid(
                        "00000000-0000-4000-8000-000000000002".to_string(),
                    )
                    .unwrap(),
                )
                .unwrap_err();
            assert_eq!(rejection.code, ResumeErrorCode::TopologyChanged);
            assert!(rejection.terminal);
            rejection.notify_terminal_owner();
            assert!(matches!(commands.try_recv(), Ok(OwnerCommand::Terminal)));
        }
    }

    #[test]
    fn exact_predecessor_with_topology_mismatch_cannot_force_drain() {
        for mismatch in ["tls", "display"] {
            let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
            let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
            let mut expected = bindings("host-a");
            expected.topology = TopologyBinding::from_response(&response).unwrap();
            let active_session_id = expected.active_session_id.clone();
            let (owner, mut commands) = mpsc::unbounded_channel();
            let predecessor = registry
                .issue_initial(
                    expected.clone(),
                    ReconnectPolicy::new(60).unwrap(),
                    owner,
                    &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
                )
                .unwrap();
            registry.refresh_grant(&active_session_id).unwrap();
            response.resume_grant = Some(predecessor.expose_for_transport().to_string());
            if mismatch == "display" {
                response.screen_width = 1_280;
            }
            registry.mark_detached(&active_session_id).unwrap();
            let host = if mismatch == "tls" {
                HostIdentity::new("spki-sha256:host-b").unwrap()
            } else {
                expected.host_identity.clone()
            };

            let rejection = registry
                .prepare_resume(
                    &response,
                    &host,
                    &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([2; 16]),
                )
                .unwrap_err();
            assert_eq!(rejection.code, ResumeErrorCode::TopologyChanged);
            assert!(!rejection.terminal);
            rejection.notify_terminal_owner();
            assert!(commands.try_recv().is_err());
            let guard = registry.entry.lock().unwrap();
            let entry = guard.as_ref().unwrap();
            assert_eq!(entry.state, EntryState::Detached);
            assert_eq!(entry.slot.current().unwrap().0, 2);
        }
    }

    #[test]
    fn unauthenticated_topology_mismatch_cannot_drain_the_session() {
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let (owner, mut commands) = mpsc::unbounded_channel();
        let grant = registry
            .issue_initial(
                expected.clone(),
                ReconnectPolicy::new(60).unwrap(),
                owner,
                &arcen_telemetry::CorrelationId::parse_uuid(
                    "00000000-0000-4000-8000-000000000001".to_string(),
                )
                .unwrap(),
            )
            .unwrap();
        let mut tampered = grant.expose_for_transport().as_bytes().to_vec();
        *tampered.last_mut().unwrap() ^= 1;
        response.resume_grant = Some(String::from_utf8(tampered).unwrap());
        response.screen_width = 1_280;
        registry.mark_detached(&active_session_id).unwrap();

        let rejection = registry
            .prepare_resume(
                &response,
                &expected.host_identity,
                &arcen_telemetry::CorrelationId::parse_uuid(
                    "00000000-0000-4000-8000-000000000002".to_string(),
                )
                .unwrap(),
            )
            .unwrap_err();
        assert_eq!(rejection.code, ResumeErrorCode::Replayed);
        assert!(commands.try_recv().is_err());
        assert!(registry.resume_handshake_available().unwrap());
    }

    #[test]
    fn invalid_candidates_cannot_notify_owner_or_change_detached_slot() {
        let registry = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut response = AuthResponse::resume("03".repeat(32), "placeholder");
        let mut expected = bindings("host-a");
        expected.topology = TopologyBinding::from_response(&response).unwrap();
        let active_session_id = expected.active_session_id.clone();
        let policy = ReconnectPolicy::new(60).unwrap();
        let (owner, mut commands) = mpsc::unbounded_channel();
        let current = registry
            .issue_initial(
                expected.clone(),
                policy,
                owner,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([1; 16]),
            )
            .unwrap();
        let (_, current_nonce) = registry
            .entry
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .slot
            .current()
            .unwrap();

        let mut changed_host = expected.clone();
        changed_host.host_identity = HostIdentity::new("spki-sha256:other-host").unwrap();
        let mut changed_session = expected.clone();
        changed_session.active_session_id = ActiveHostSessionId::new("windows-wts:8").unwrap();
        let mut changed_principal = expected.clone();
        changed_principal.native_principal = NativePrincipal::Windows {
            sid: arcen_identity::WindowsSid::new("S-1-5-21-1-2-3-1002").unwrap(),
            wts_session_id: 7,
        };
        let mut changed_holder = expected.clone();
        changed_holder.holder_nonce = DeckHolderNonce::new([4; 32]);
        let mut changed_disclaimer = expected.clone();
        changed_disclaimer.disclaimer_digest =
            DisclaimerDigest::parse_lower_hex(&"01".repeat(32)).unwrap();
        let mut changed_disclaimer_version = expected.clone();
        changed_disclaimer_version.disclaimer_version = DisclaimerVersion::new("changed").unwrap();

        let foreign = ResumeRegistry::with_clock(FakeClock::new(2_000)).unwrap();
        let mut tampered = current.expose_for_transport().as_bytes().to_vec();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'0' { b'1' } else { b'0' };
        let candidates = vec![
            "v1.00".to_string(),
            String::from_utf8(tampered).unwrap(),
            foreign
                .sign_claims(&expected, 1, current_nonce, policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&changed_host, 1, current_nonce, policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&changed_session, 1, current_nonce, policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&changed_principal, 1, current_nonce, policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&changed_holder, 1, current_nonce, policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&changed_disclaimer, 1, current_nonce, policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&changed_disclaimer_version, 1, current_nonce, policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&expected, 1, [8; 32], policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&expected, 2, [8; 32], policy, 2_000)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&expected, 1, current_nonce, policy, 1_880)
                .unwrap()
                .expose_for_transport()
                .to_string(),
            registry
                .sign_claims(&expected, 1, current_nonce, policy, 2_001)
                .unwrap()
                .expose_for_transport()
                .to_string(),
        ];
        registry.mark_detached(&active_session_id).unwrap();

        response.resume_grant = Some(current.expose_for_transport().to_string());
        response.resume_holder_nonce = Some("04".repeat(32));
        let wrong_holder = registry
            .prepare_resume(
                &response,
                &expected.host_identity,
                &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([15; 16]),
            )
            .unwrap_err();
        wrong_holder.notify_terminal_owner();
        response.resume_holder_nonce = Some("03".repeat(32));

        for (index, candidate) in candidates.into_iter().enumerate() {
            response.resume_grant = Some(candidate);
            let rejection = registry
                .prepare_resume(
                    &response,
                    &expected.host_identity,
                    &arcen_telemetry::CorrelationId::from_uuid_v4_bytes([index as u8; 16]),
                )
                .unwrap_err();
            rejection.notify_terminal_owner();
            let guard = registry.entry.lock().unwrap();
            let entry = guard.as_ref().unwrap();
            assert_eq!(entry.state, EntryState::Detached);
            assert_eq!(entry.slot.current(), Some((1, current_nonce)));
        }
        assert!(commands.try_recv().is_err());
        assert!(registry.resume_handshake_available().unwrap());
    }

    #[test]
    fn holder_nonce_is_exact_lower_hex_and_topology_is_stable() {
        assert_eq!(
            decode_holder_nonce(&"01".repeat(32)).unwrap().as_bytes(),
            &[1; 32]
        );
        assert!(decode_holder_nonce(&"01".repeat(31)).is_none());
        assert!(decode_holder_nonce(&"GG".repeat(32)).is_none());

        let mut response = AuthResponse::pam("user", "password");
        response.screen_width = 1920;
        response.screen_height = 1080;
        response.displays_mode = "single_primary".to_string();
        let first = TopologyBinding::from_response(&response).unwrap();
        response.cursor_preference = arcen_protocol::messages::CursorMode::Host;
        assert_ne!(first, TopologyBinding::from_response(&response).unwrap());
        response.cursor_preference = arcen_protocol::messages::CursorMode::Local;
        response.screen_width = 1280;
        assert_ne!(first, TopologyBinding::from_response(&response).unwrap());
    }

    #[test]
    fn detach_resume_expiry_and_stale_timer_keep_cleanup_boundaries_exact() {
        use arcen_session::direct_reconnect::{
            DirectReconnect, ReconnectEvent, ReconnectState, ReconnectTimerAction,
        };

        #[derive(Default)]
        struct Probe {
            attachment_stops: usize,
            attachment_starts: usize,
            restores: usize,
            revocations: usize,
            finalized: bool,
        }

        impl Probe {
            fn apply(&mut self, actions: arcen_session::direct_reconnect::ReconnectActions) {
                self.attachment_stops += usize::from(actions.stop_media);
                self.attachment_starts += usize::from(actions.start_media);
                if actions.restore_leases && !self.finalized {
                    self.restores += 1;
                    self.finalized = true;
                }
                if actions.revoke_grant && self.revocations == 0 {
                    self.revocations = 1;
                }
            }
        }

        let mut reconnect = DirectReconnect::new(ReconnectPolicy::new(10).unwrap());
        let mut probe = Probe::default();
        let detached = reconnect.apply(ReconnectEvent::UnexpectedLoss, MonotonicMillis::new(1_000));
        assert!(detached.hold_restore_leases);
        assert!(!detached.restore_leases);
        let ReconnectTimerAction::Arm {
            timer_generation: first_timer,
            ..
        } = detached.timer
        else {
            panic!("detach must arm one timer");
        };
        probe.apply(detached);
        assert_eq!(probe.restores, 0);
        assert_eq!(probe.attachment_stops, 1);

        probe.apply(reconnect.apply(ReconnectEvent::BeginResume, MonotonicMillis::new(2_000)));
        let resumed = reconnect.apply(ReconnectEvent::ResumeAccepted, MonotonicMillis::new(2_001));
        assert!(resumed.start_media && resumed.rotate_grant);
        probe.apply(resumed);

        let second_detach =
            reconnect.apply(ReconnectEvent::UnexpectedLoss, MonotonicMillis::new(3_000));
        let ReconnectTimerAction::Arm {
            timer_generation: second_timer,
            ..
        } = second_detach.timer
        else {
            panic!("second detach must arm one timer");
        };
        probe.apply(second_detach);
        assert_ne!(first_timer, second_timer);

        probe.apply(reconnect.apply(
            ReconnectEvent::DeadlineReached {
                timer_generation: first_timer,
            },
            MonotonicMillis::new(20_000),
        ));
        assert!(matches!(reconnect.state(), ReconnectState::Detached { .. }));
        assert_eq!(probe.restores, 0);

        probe.apply(reconnect.apply(
            ReconnectEvent::DeadlineReached {
                timer_generation: second_timer,
            },
            MonotonicMillis::new(20_000),
        ));
        probe.apply(reconnect.apply(ReconnectEvent::OwnerCrashed, MonotonicMillis::new(20_001)));
        assert_eq!(probe.attachment_starts, 1);
        assert_eq!(probe.restores, 1);
        assert_eq!(probe.revocations, 1);
    }

    #[test]
    fn agent_crash_and_broker_shutdown_each_finalize_once() {
        use arcen_session::direct_reconnect::{DirectReconnect, ReconnectEvent};

        for event in [
            ReconnectEvent::OwnerCrashed,
            ReconnectEvent::ExplicitDisconnect,
        ] {
            let mut reconnect = DirectReconnect::new(ReconnectPolicy::new(30).unwrap());
            let first = reconnect.apply(event, MonotonicMillis::new(1));
            assert!(first.restore_leases);
            assert!(first.stop_media);
            assert!(first.reset_input);
            assert!(first.revoke_grant);
            let repeated = reconnect.apply(event, MonotonicMillis::new(2));
            assert!(!repeated.restore_leases);
            assert!(!repeated.revoke_grant);
        }
    }
}
