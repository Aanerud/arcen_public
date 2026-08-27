//! The provider side of the credential pipe: the single-use decryption gate and
//! the transport-generic serve loop.
//!
//! [`CredentialSession`] owns the per-Advise ephemeral recipient. It publishes
//! the public key + challenge for a `Ready` message, then opens exactly one
//! sealed credential — a second attempt, an expired attempt, or a tampered
//! attempt fails closed and destroys the key. [`provider_serve`] drives the wire
//! exchange over any [`FrameIo`]: it announces the provider, sends `Ready`, and
//! blocks for exactly one `PushCredentials`, acknowledging it and returning the
//! decrypted payload for the platform to arm as a one-shot autologon.
//!
//! Every path here is pure and unit-tested; the Windows pipe thread supplies
//! only the [`FrameIo`] and the surrounding COM plumbing.

use crate::crypto::{CryptoError, EphemeralRecipient, SealedCredential};
use crate::payload::CredentialPayload;
use crate::transport::{FrameIo, TransportError};
use crate::{
    CpMessage, Hello, NonceTracker, PushAck, Ready, Role, UsageScenario, PROTOCOL_VERSION,
};

/// Why a credential session refused a push.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionError {
    /// The session has no recipient: it was already consumed or never armed.
    NotArmed,
    /// The single-use window elapsed before the push arrived.
    Expired,
    /// The account SID on the push did not match the SID the broker
    /// authenticated (transcript binding would have failed anyway; this is an
    /// explicit early check).
    AccountMismatch,
    /// The envelope could not be opened (wrong key/tamper/AAD mismatch) or the
    /// decrypted payload was invalid.
    Crypto(CryptoError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotArmed => f.write_str("credential session is not armed"),
            Self::Expired => f.write_str("credential session expired"),
            Self::AccountMismatch => f.write_str("pushed account SID mismatch"),
            Self::Crypto(error) => write!(f, "credential decrypt failed: {error}"),
        }
    }
}

impl std::error::Error for SessionError {}

/// A single-use, expiring credential-decryption gate for one Advise lifecycle.
pub struct CredentialSession {
    recipient: Option<EphemeralRecipient>,
    protocol_version: u16,
    expires_at_ms: u64,
    /// The account SID the broker is expected to bind, if the provider learned
    /// it out of band. When `None`, any SID that satisfies the AEAD is accepted.
    expected_account_sid: Option<Vec<u8>>,
}

impl CredentialSession {
    /// Arm a session with a fresh recipient. `expires_at_ms` is an absolute
    /// deadline on the same clock passed to [`Self::ingest`].
    pub fn new(recipient: EphemeralRecipient, expires_at_ms: u64) -> Self {
        Self {
            recipient: Some(recipient),
            protocol_version: PROTOCOL_VERSION,
            expires_at_ms,
            expected_account_sid: None,
        }
    }

    /// Generate a brand-new recipient and arm the session.
    pub fn generate(expires_at_ms: u64) -> Result<Self, CryptoError> {
        Ok(Self::new(EphemeralRecipient::generate()?, expires_at_ms))
    }

    /// Optionally pin the account SID the broker must bind. A mismatch is
    /// rejected before any decryption is attempted.
    pub fn expect_account_sid(&mut self, sid: &[u8]) {
        self.expected_account_sid = Some(sid.to_vec());
    }

    pub fn is_armed(&self) -> bool {
        self.recipient.is_some()
    }

    /// The ephemeral public key to publish in `Ready`, or `None` once consumed.
    pub fn public_key(&self) -> Option<[u8; crate::crypto::PUBLIC_KEY_LEN]> {
        self.recipient.as_ref().map(|r| *r.public_key())
    }

    /// The challenge to publish in `Ready`, or `None` once consumed.
    pub fn challenge(&self) -> Option<[u8; crate::crypto::CHALLENGE_LEN]> {
        self.recipient.as_ref().map(|r| *r.challenge())
    }

    /// Build the `Ready` message for this session.
    pub fn ready_message(
        &self,
        clsid: &str,
        usage: UsageScenario,
        pid: u32,
        nonce: u64,
    ) -> Result<Ready, SessionError> {
        let public = self.public_key().ok_or(SessionError::NotArmed)?;
        let challenge = self.challenge().ok_or(SessionError::NotArmed)?;
        Ok(Ready {
            clsid: clsid.to_string(),
            usage,
            pid,
            cp_public: crate::encode_hex(&public),
            challenge: crate::encode_hex(&challenge),
            nonce,
        })
    }

    /// Open a pushed credential exactly once. On any error (expired, already
    /// consumed, tampered, wrong account) the recipient is destroyed so no retry
    /// can reuse this lifecycle's key.
    pub fn ingest(
        &mut self,
        sealed: &SealedCredential,
        account_sid: &[u8],
        request_id: u64,
        now_ms: u64,
    ) -> Result<CredentialPayload, SessionError> {
        // Consume the recipient up front: even a rejected attempt burns the
        // single-use key, defeating replay and repeated-guess attempts.
        let recipient = self.recipient.take().ok_or(SessionError::NotArmed)?;

        if now_ms > self.expires_at_ms {
            return Err(SessionError::Expired);
        }
        if let Some(expected) = &self.expected_account_sid {
            if expected.as_slice() != account_sid {
                return Err(SessionError::AccountMismatch);
            }
        }
        recipient
            .open(sealed, account_sid, request_id, self.protocol_version)
            .map_err(SessionError::Crypto)
    }

    /// Destroy the recipient without opening anything (UnAdvise / shutdown).
    pub fn disarm(&mut self) {
        self.recipient = None;
    }
}

impl std::fmt::Debug for CredentialSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CredentialSession")
            .field("armed", &self.is_armed())
            .field("expires_at_ms", &self.expires_at_ms)
            .finish()
    }
}

/// Static identity a provider announces in `Hello`/`Ready`.
#[derive(Clone, Debug)]
pub struct ProviderIdentity {
    pub clsid: String,
    pub usage: UsageScenario,
    pub pid: u32,
}

/// Drive the provider side of the exchange over `channel`.
///
/// Announces the provider (`Hello`), publishes `Ready`, then blocks for exactly
/// one `PushCredentials`. On success it opens the credential and hands it to
/// `arm`; only after that callback succeeds does it send
/// `PushAck { armed: true }`. Decryption, platform arming, and notification
/// failures send `PushAck { armed: false }` and fail closed.
///
/// `now_ms` is invoked when a push arrives to evaluate single-use expiry.
pub fn provider_serve<T, F, R>(
    channel: &mut T,
    session: &mut CredentialSession,
    identity: &ProviderIdentity,
    now_ms: &dyn Fn() -> u64,
    arm: F,
) -> Result<R, TransportError>
where
    T: FrameIo,
    F: FnOnce(u64, CredentialPayload) -> Result<R, String>,
{
    let mut tracker = NonceTracker::new();

    channel.write_message(&CpMessage::Hello(Hello {
        protocol_version: PROTOCOL_VERSION,
        role: Role::Provider,
        nonce: 1,
    }))?;
    let ready = session
        .ready_message(&identity.clsid, identity.usage, identity.pid, 2)
        .map_err(TransportError::Session)?;
    channel.write_message(&CpMessage::Ready(ready))?;

    let message = channel.read_message()?;
    tracker
        .accept(message.nonce())
        .map_err(|_| TransportError::Replay(message.nonce()))?;
    match message {
        CpMessage::PushCredentials(push) => {
            let sealed = push.to_sealed().map_err(TransportError::Protocol)?;
            match session.ingest(
                &sealed,
                push.account_sid.as_bytes(),
                push.request_id,
                now_ms(),
            ) {
                Ok(payload) => match arm(push.request_id, payload) {
                    Ok(result) => {
                        channel.write_message(&CpMessage::PushAck(PushAck {
                            request_id: push.request_id,
                            armed: true,
                            nonce: 3,
                        }))?;
                        Ok(result)
                    }
                    Err(detail) => {
                        let _ = channel.write_message(&CpMessage::PushAck(PushAck {
                            request_id: push.request_id,
                            armed: false,
                            nonce: 3,
                        }));
                        Err(TransportError::ArmFailed(detail))
                    }
                },
                Err(error) => {
                    let _ = channel.write_message(&CpMessage::PushAck(PushAck {
                        request_id: push.request_id,
                        armed: false,
                        nonce: 3,
                    }));
                    Err(TransportError::Session(error))
                }
            }
        }
        CpMessage::Hello(_) | CpMessage::Ready(_) | CpMessage::PushAck(_) => Err(
            TransportError::Unexpected("provider awaited a credential push"),
        ),
        CpMessage::SessionComplete(_) | CpMessage::Ack(_) => Err(TransportError::Unexpected(
            "provider awaited a credential push",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::seal_credential;

    fn armed_session(now: u64) -> CredentialSession {
        CredentialSession::generate(now + 30_000).expect("session")
    }

    #[test]
    fn ingest_opens_exactly_once() {
        let now = 1_000;
        let mut session = armed_session(now);
        let public = session.public_key().expect("public");
        let challenge = session.challenge().expect("challenge");
        let payload = CredentialPayload::new(r"CORP\alice", "hunter2").expect("payload");
        let sealed = seal_credential(
            &public,
            &challenge,
            b"S-1-5-21-7",
            42,
            PROTOCOL_VERSION,
            &payload,
        )
        .expect("seal");

        let opened = session
            .ingest(&sealed, b"S-1-5-21-7", 42, now)
            .expect("first ingest");
        assert_eq!(opened.username(), r"CORP\alice");
        assert_eq!(opened.password(), "hunter2");

        // The recipient is consumed: a replay is refused.
        assert_eq!(
            session.ingest(&sealed, b"S-1-5-21-7", 42, now),
            Err(SessionError::NotArmed)
        );
        assert!(!session.is_armed());
    }

    #[test]
    fn expired_push_is_rejected_and_burns_the_key() {
        let now = 1_000;
        let mut session = CredentialSession::generate(now + 10).expect("session");
        let public = session.public_key().expect("public");
        let challenge = session.challenge().expect("challenge");
        let payload = CredentialPayload::new("alice", "pw").expect("payload");
        let sealed = seal_credential(
            &public,
            &challenge,
            b"S-1-5-18",
            1,
            PROTOCOL_VERSION,
            &payload,
        )
        .expect("seal");
        assert_eq!(
            session.ingest(&sealed, b"S-1-5-18", 1, now + 11),
            Err(SessionError::Expired)
        );
        assert!(!session.is_armed());
    }

    #[test]
    fn pinned_account_mismatch_is_rejected() {
        let now = 1_000;
        let mut session = armed_session(now);
        session.expect_account_sid(b"S-1-5-21-7");
        let public = session.public_key().expect("public");
        let challenge = session.challenge().expect("challenge");
        let payload = CredentialPayload::new("alice", "pw").expect("payload");
        let sealed = seal_credential(
            &public,
            &challenge,
            b"S-1-5-21-9",
            1,
            PROTOCOL_VERSION,
            &payload,
        )
        .expect("seal");
        assert_eq!(
            session.ingest(&sealed, b"S-1-5-21-9", 1, now),
            Err(SessionError::AccountMismatch)
        );
    }

    #[test]
    fn tampered_ciphertext_is_rejected_and_burns_the_key() {
        let now = 1_000;
        let mut session = armed_session(now);
        let public = session.public_key().expect("public");
        let challenge = session.challenge().expect("challenge");
        let payload = CredentialPayload::new("alice", "pw").expect("payload");
        let mut sealed = seal_credential(
            &public,
            &challenge,
            b"S-1-5-18",
            1,
            PROTOCOL_VERSION,
            &payload,
        )
        .expect("seal");
        let mut bytes = sealed.ciphertext().to_vec();
        bytes[0] ^= 0x01;
        sealed = SealedCredential::from_parts(*sealed.broker_public(), *sealed.nonce(), bytes)
            .expect("rebuild");
        assert!(matches!(
            session.ingest(&sealed, b"S-1-5-18", 1, now),
            Err(SessionError::Crypto(_))
        ));
        assert!(!session.is_armed());
    }

    #[test]
    fn ready_message_is_well_formed() {
        let session = armed_session(0);
        let ready = session
            .ready_message(
                "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}",
                UsageScenario::Logon,
                7,
                9,
            )
            .expect("ready");
        assert_eq!(
            ready.cp_public_bytes().unwrap(),
            session.public_key().unwrap()
        );
        assert_eq!(
            ready.challenge_bytes().unwrap(),
            session.challenge().unwrap()
        );
        assert!(CpMessage::Ready(ready).validate().is_ok());
    }
}
