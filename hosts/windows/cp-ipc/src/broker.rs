//! The broker side of the credential pipe.
//!
//! The LocalSystem broker is the pipe *server*. When a CP connects it reads the
//! provider's `Hello`/`Ready`, records the ephemeral public key + challenge, and
//! (only after the platform peer check passes) may later seal a credential and
//! push it. This module holds the transport-generic building blocks; the Windows
//! host supplies the verified pipe handle and the WTS orchestration around it.
//!
//! Nothing here creates a session or trusts the provider. A successful
//! [`push_credential`] means only that a correctly-keyed envelope was delivered
//! and acknowledged; the broker still re-binds the resulting WTS session by SID.

use crate::crypto::seal_credential;
use crate::payload::CredentialPayload;
use crate::transport::{FrameIo, TransportError};
use crate::{CpMessage, NonceTracker, PushCredentials, Ready, Role, PROTOCOL_VERSION};

/// Provider readiness recorded by the broker after a successful handshake.
#[derive(Clone, Debug)]
pub struct ProviderReadiness {
    pub clsid: String,
    pub usage: crate::UsageScenario,
    /// The LogonUI process id the provider reported. The platform transport must
    /// have already confirmed this is the *actual* connected peer PID.
    pub pid: u32,
    cp_public: [u8; crate::crypto::PUBLIC_KEY_LEN],
    challenge: [u8; crate::crypto::CHALLENGE_LEN],
}

impl ProviderReadiness {
    pub fn cp_public(&self) -> &[u8; crate::crypto::PUBLIC_KEY_LEN] {
        &self.cp_public
    }

    pub fn challenge(&self) -> &[u8; crate::crypto::CHALLENGE_LEN] {
        &self.challenge
    }
}

/// Read and validate the provider's opening `Hello` then `Ready`.
///
/// `tracker` guards transport replay across the connection's lifetime and must
/// be reused for subsequent reads (e.g. the `PushAck`).
pub fn recv_ready<T: FrameIo>(
    channel: &mut T,
    tracker: &mut NonceTracker,
) -> Result<ProviderReadiness, TransportError> {
    let hello = channel.read_message()?;
    tracker
        .accept(hello.nonce())
        .map_err(|_| TransportError::Replay(hello.nonce()))?;
    match hello {
        CpMessage::Hello(hello) if hello.role == Role::Provider => {}
        CpMessage::Hello(_) => {
            return Err(TransportError::Unexpected("hello role is not provider"))
        }
        _ => return Err(TransportError::Unexpected("expected provider hello")),
    }

    let ready = channel.read_message()?;
    tracker
        .accept(ready.nonce())
        .map_err(|_| TransportError::Replay(ready.nonce()))?;
    let ready: Ready = match ready {
        CpMessage::Ready(ready) => ready,
        _ => return Err(TransportError::Unexpected("expected provider ready")),
    };
    let cp_public = ready.cp_public_bytes().map_err(TransportError::Protocol)?;
    let challenge = ready.challenge_bytes().map_err(TransportError::Protocol)?;
    Ok(ProviderReadiness {
        clsid: ready.clsid,
        usage: ready.usage,
        pid: ready.pid,
        cp_public,
        challenge,
    })
}

/// Seal `payload` for `readiness` and push it, returning the provider's `armed`
/// flag from the `PushAck`.
///
/// `peer_ok` is the platform peer-identity verdict (LogonUI.exe + SYSTEM).
/// When it is false, nothing is sealed or sent — the broker fails closed.
/// `account_sid` and `request_id` are bound into the AEAD associated data.
#[allow(clippy::too_many_arguments)] // The transcript inputs stay explicit at this trust boundary.
pub fn push_credential<T: FrameIo>(
    channel: &mut T,
    tracker: &mut NonceTracker,
    readiness: &ProviderReadiness,
    account_sid: &str,
    request_id: u64,
    push_nonce: u64,
    payload: &CredentialPayload,
    peer_ok: bool,
) -> Result<bool, TransportError> {
    if !peer_ok {
        return Err(TransportError::PeerRejected);
    }
    let sealed = seal_credential(
        readiness.cp_public(),
        readiness.challenge(),
        account_sid.as_bytes(),
        request_id,
        PROTOCOL_VERSION,
        payload,
    )
    .map_err(|error| TransportError::Session(crate::cp_session::SessionError::Crypto(error)))?;

    let push = PushCredentials::from_sealed(request_id, account_sid, &sealed, push_nonce);
    channel.write_message(&CpMessage::PushCredentials(push))?;

    let ack = channel.read_message()?;
    tracker
        .accept(ack.nonce())
        .map_err(|_| TransportError::Replay(ack.nonce()))?;
    match ack {
        CpMessage::PushAck(ack) if ack.request_id == request_id => Ok(ack.armed),
        CpMessage::PushAck(_) => Err(TransportError::Unexpected("push ack request-id mismatch")),
        _ => Err(TransportError::Unexpected("expected push ack")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cp_session::{provider_serve, CredentialSession, ProviderIdentity};
    use crate::transport::mem_frame_duplex;
    use crate::UsageScenario;

    const CLSID: &str = "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}";

    #[test]
    fn broker_rejects_push_when_peer_check_fails() {
        let (mut broker, mut provider) = mem_frame_duplex();
        let identity = ProviderIdentity {
            clsid: CLSID.to_string(),
            usage: UsageScenario::Logon,
            pid: 4321,
        };
        let provider_thread = std::thread::spawn(move || {
            let mut session = CredentialSession::generate(60_000).expect("session");
            provider_serve(
                &mut provider,
                &mut session,
                &identity,
                &|| 0,
                |_, payload| Ok(payload),
            )
        });

        let mut tracker = NonceTracker::new();
        let readiness = recv_ready(&mut broker, &mut tracker).expect("ready");
        let payload = CredentialPayload::new("alice", "pw").expect("payload");
        let result = push_credential(
            &mut broker,
            &mut tracker,
            &readiness,
            "S-1-5-21-7",
            1,
            10,
            &payload,
            false,
        );
        assert!(matches!(result, Err(TransportError::PeerRejected)));
        drop(broker); // provider serve loop ends on EOF
        let _ = provider_thread.join();
    }
}
