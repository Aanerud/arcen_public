//! Pure integration harness for the broker <-> Credential Provider credential
//! pipe.
//!
//! This runs the exact transport-generic drivers the Windows named-pipe threads
//! use ([`arcen_cp_ipc::broker`] and [`arcen_cp_ipc::cp_session`]) over
//! the in-memory byte duplex, so the whole handshake — provider `Ready`, sealed
//! encrypted push, single-use decryption, and acknowledgement — is exercised on
//! every dev OS with no Windows, no LogonUI, and no registration.
//!
//! It proves the happy path and the four mandated rejections: tampered
//! ciphertext, replayed/cross-lifecycle envelope, oversized frame, and a failed
//! peer check.

use std::io::Write;
use std::thread;

use arcen_cp_ipc::broker::{push_credential, recv_ready};
use arcen_cp_ipc::cp_session::{provider_serve, CredentialSession, ProviderIdentity, SessionError};
use arcen_cp_ipc::crypto::seal_credential;
use arcen_cp_ipc::transport::{
    mem_duplex, mem_frame_duplex, FrameIo, StreamFrames, TransportError,
};
use arcen_cp_ipc::{
    CpMessage, CredentialPayload, NonceTracker, ProtocolError, PushCredentials, UsageScenario,
    MAX_FRAME_LEN, PROTOCOL_VERSION,
};

const CLSID: &str = "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}";
const ACCOUNT_SID: &str = "S-1-5-21-1111-2222-3333-1001";
const USERNAME: &str = r"STUDIO\artist";
const PASSWORD: &str = "correct horse battery staple";

fn provider_identity() -> ProviderIdentity {
    ProviderIdentity {
        clsid: CLSID.to_string(),
        usage: UsageScenario::Logon,
        pid: 4321,
    }
}

/// Happy path: provider publishes readiness, broker seals and pushes, provider
/// decrypts exactly one credential and both ends agree it armed.
#[test]
fn ready_then_encrypted_push_decrypts_and_arms() {
    let (mut broker, mut provider) = mem_frame_duplex();
    let identity = provider_identity();
    let provider_thread = thread::spawn(move || {
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
    let readiness = recv_ready(&mut broker, &mut tracker).expect("readiness");
    assert_eq!(readiness.clsid, CLSID);
    assert_eq!(readiness.pid, 4321);

    let payload = CredentialPayload::new(USERNAME, PASSWORD).expect("payload");
    let armed = push_credential(
        &mut broker,
        &mut tracker,
        &readiness,
        ACCOUNT_SID,
        1,
        10,
        &payload,
        true,
    )
    .expect("push");
    assert!(armed, "provider reported the credential as armed");

    let opened = provider_thread
        .join()
        .expect("join")
        .expect("provider serve");
    assert_eq!(opened.username(), USERNAME);
    assert_eq!(opened.password(), PASSWORD);
}

#[test]
fn platform_arm_failure_is_acknowledged_as_not_armed() {
    let (mut broker, mut provider) = mem_frame_duplex();
    let identity = provider_identity();
    let provider_thread = thread::spawn(move || {
        let mut session = CredentialSession::generate(60_000).expect("session");
        provider_serve(&mut provider, &mut session, &identity, &|| 0, |_, _| {
            Err::<(), _>("native notification failed".to_string())
        })
    });

    let mut tracker = NonceTracker::new();
    let readiness = recv_ready(&mut broker, &mut tracker).expect("readiness");
    let payload = CredentialPayload::new(USERNAME, PASSWORD).expect("payload");
    let armed = push_credential(
        &mut broker,
        &mut tracker,
        &readiness,
        ACCOUNT_SID,
        1,
        10,
        &payload,
        true,
    )
    .expect("push acknowledgement");
    assert!(!armed);
    assert!(matches!(
        provider_thread.join().expect("join"),
        Err(TransportError::ArmFailed(_))
    ));
}

/// A failed platform peer check refuses to seal or send anything.
#[test]
fn failed_peer_check_pushes_nothing() {
    let (mut broker, mut provider) = mem_frame_duplex();
    let identity = provider_identity();
    let provider_thread = thread::spawn(move || {
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
    let readiness = recv_ready(&mut broker, &mut tracker).expect("readiness");
    let payload = CredentialPayload::new(USERNAME, PASSWORD).expect("payload");
    let result = push_credential(
        &mut broker,
        &mut tracker,
        &readiness,
        ACCOUNT_SID,
        1,
        10,
        &payload,
        false, // peer check failed
    );
    assert!(matches!(result, Err(TransportError::PeerRejected)));

    // The provider is still waiting; closing the pipe ends its loop.
    drop(broker);
    let outcome = provider_thread.join().expect("join");
    assert!(matches!(outcome, Err(TransportError::Closed)));
}

/// A tampered ciphertext (one flipped hex nibble) fails the AEAD tag check; the
/// provider acknowledges "not armed" and returns a crypto error.
#[test]
fn tampered_ciphertext_is_rejected() {
    let (mut broker, mut provider) = mem_frame_duplex();
    let identity = provider_identity();
    let provider_thread = thread::spawn(move || {
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
    let readiness = recv_ready(&mut broker, &mut tracker).expect("readiness");
    let payload = CredentialPayload::new(USERNAME, PASSWORD).expect("payload");
    let sealed = seal_credential(
        readiness.cp_public(),
        readiness.challenge(),
        ACCOUNT_SID.as_bytes(),
        7,
        PROTOCOL_VERSION,
        &payload,
    )
    .expect("seal");
    let mut push = PushCredentials::from_sealed(7, ACCOUNT_SID, &sealed, 10);
    // Flip the first ciphertext nibble; still valid-length lowercase hex.
    let mut chars: Vec<char> = push.ciphertext.chars().collect();
    chars[0] = if chars[0] == 'a' { 'b' } else { 'a' };
    push.ciphertext = chars.into_iter().collect();
    broker
        .write_message(&CpMessage::PushCredentials(push))
        .expect("write tampered push");

    let outcome = provider_thread.join().expect("join");
    assert!(matches!(
        outcome,
        Err(TransportError::Session(SessionError::Crypto(_)))
    ));
}

/// An envelope sealed for one Advise lifecycle cannot be replayed into a
/// different provider instance: the new recipient's ephemeral key was never
/// party to the agreement.
#[test]
fn replayed_envelope_from_another_lifecycle_is_rejected() {
    // Lifecycle 1: capture a legitimately sealed push for provider #1.
    let (mut broker1, mut provider1) = mem_frame_duplex();
    let identity = provider_identity();
    let provider1_thread = thread::spawn(move || {
        let mut session = CredentialSession::generate(60_000).expect("session");
        provider_serve(
            &mut provider1,
            &mut session,
            &identity,
            &|| 0,
            |_, payload| Ok(payload),
        )
    });
    let mut tracker1 = NonceTracker::new();
    let readiness1 = recv_ready(&mut broker1, &mut tracker1).expect("readiness1");
    let payload = CredentialPayload::new(USERNAME, PASSWORD).expect("payload");
    let sealed = seal_credential(
        readiness1.cp_public(),
        readiness1.challenge(),
        ACCOUNT_SID.as_bytes(),
        7,
        PROTOCOL_VERSION,
        &payload,
    )
    .expect("seal");
    let captured = PushCredentials::from_sealed(7, ACCOUNT_SID, &sealed, 10);
    broker1
        .write_message(&CpMessage::PushCredentials(captured.clone()))
        .expect("deliver to provider1");
    assert!(provider1_thread.join().expect("join").is_ok());

    // Lifecycle 2: a fresh provider publishes new keys; replaying the captured
    // envelope must fail because provider #2's ephemeral key differs.
    let (mut broker2, mut provider2) = mem_frame_duplex();
    let identity = provider_identity();
    let provider2_thread = thread::spawn(move || {
        let mut session = CredentialSession::generate(60_000).expect("session");
        provider_serve(
            &mut provider2,
            &mut session,
            &identity,
            &|| 0,
            |_, payload| Ok(payload),
        )
    });
    let mut tracker2 = NonceTracker::new();
    let _readiness2 = recv_ready(&mut broker2, &mut tracker2).expect("readiness2");
    broker2
        .write_message(&CpMessage::PushCredentials(captured))
        .expect("replay to provider2");
    let outcome = provider2_thread.join().expect("join");
    assert!(matches!(
        outcome,
        Err(TransportError::Session(SessionError::Crypto(_)))
    ));
}

/// The same live provider instance refuses a second push: the single-use
/// recipient is consumed on the first ingest (the serve loop also returns after
/// one credential, so this asserts at the session level).
#[test]
fn credential_session_is_single_use() {
    let mut session = CredentialSession::generate(60_000).expect("session");
    let public = session.public_key().expect("public");
    let challenge = session.challenge().expect("challenge");
    let payload = CredentialPayload::new(USERNAME, PASSWORD).expect("payload");
    let sealed = seal_credential(
        &public,
        &challenge,
        ACCOUNT_SID.as_bytes(),
        1,
        PROTOCOL_VERSION,
        &payload,
    )
    .expect("seal");
    assert!(session
        .ingest(&sealed, ACCOUNT_SID.as_bytes(), 1, 0)
        .is_ok());
    // Replay against the same session: recipient already consumed.
    assert_eq!(
        session
            .ingest(&sealed, ACCOUNT_SID.as_bytes(), 1, 0)
            .unwrap_err(),
        SessionError::NotArmed
    );
}

/// An oversized length prefix is refused before any body allocation.
#[test]
fn oversized_frame_is_rejected() {
    let (broker_raw, provider_raw) = mem_duplex();
    let mut provider = StreamFrames::new(provider_raw);
    let mut broker = broker_raw;
    let identity = provider_identity();
    let provider_thread = thread::spawn(move || {
        let mut session = CredentialSession::generate(60_000).expect("session");
        provider_serve(
            &mut provider,
            &mut session,
            &identity,
            &|| 0,
            |_, payload| Ok(payload),
        )
    });

    // The provider writes Hello + Ready (buffered, unread here) then blocks on a
    // read; feed it a prefix that lies about a huge body.
    let prefix = ((MAX_FRAME_LEN + 1) as u32).to_be_bytes();
    broker.write_all(&prefix).expect("write prefix");
    broker.flush().expect("flush");

    let outcome = provider_thread.join().expect("join");
    assert!(matches!(
        outcome,
        Err(TransportError::Protocol(
            ProtocolError::FrameTooLarge { .. }
        ))
    ));
}
