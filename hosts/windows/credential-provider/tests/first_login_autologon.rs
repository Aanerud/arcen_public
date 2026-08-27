//! Pure integration harness for the credential-provider side of first-login.
//!
//! It drives the real broker <-> provider handshake over the in-memory duplex
//! (the same drivers the Windows pipe thread uses) and feeds the decrypted
//! credential into the *actual* [`CredentialFields`] autologon state machine, so
//! the full chain — `ready -> encrypted push -> CredentialsChanged-state (arm)
//! -> single autologon -> serialize -> clear` — is exercised on every dev OS
//! with no LogonUI, no COM, and no registration.

use std::sync::{Arc, Mutex};
use std::thread;

use arcen_cp_ipc::broker::{push_credential, recv_ready};
use arcen_cp_ipc::cp_session::{provider_serve, CredentialSession, ProviderIdentity};
use arcen_cp_ipc::transport::mem_frame_duplex;
use arcen_cp_ipc::{CredentialPayload, NonceTracker, UsageScenario};

use arcen_credential_provider::fields::{CredentialCountReport, CredentialFields};
use arcen_credential_provider::secret::SecretWide;

const CLSID: &str = "{2FBE34F2-9E7A-42FA-BFBF-44897694BE60}";
const ACCOUNT_SID: &str = "S-1-5-21-9-9-9-1001";
const USERNAME: &str = r"STUDIO\artist";
const PASSWORD: &str = "a long enough passphrase";

/// End to end: the provider serves one sealed push, arms a one-shot autologon in
/// the shared fields (mirroring the pipe worker), and LogonUI's re-query then
/// auto-submits exactly once and clears.
#[test]
fn push_arms_a_single_autologon_that_serializes_once_then_clears() {
    let fields: Arc<Mutex<CredentialFields>> = Arc::new(Mutex::new(CredentialFields::new()));
    let (mut broker, mut provider) = mem_frame_duplex();

    let fields_worker = Arc::clone(&fields);
    let provider_thread = thread::spawn(move || {
        let mut session = CredentialSession::generate(60_000).expect("session");
        let identity = ProviderIdentity {
            clsid: CLSID.to_string(),
            usage: UsageScenario::Logon,
            pid: 4321,
        };
        provider_serve(
            &mut provider,
            &mut session,
            &identity,
            &|| 0,
            |request_id, payload| {
                // This is exactly what the Windows pipe worker does before it
                // acknowledges a valid push.
                let mut guard = fields_worker.lock().expect("fields");
                guard.arm_autologon(
                    payload.username().to_string(),
                    SecretWide::from_text(payload.password()),
                    request_id,
                    60_000,
                );
                Ok(())
            },
        )
        .expect("provider serve");
    });

    // Broker side: read readiness, seal, and push the credential.
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
    .expect("push");
    assert!(armed);
    provider_thread.join().expect("join");

    // The credential is armed (the CredentialsChanged-equivalent state). Now walk
    // the exact LogonUI callback sequence against the real fields.
    let mut fields = fields.lock().expect("fields");
    assert!(fields.has_pending_autologon());
    assert_eq!(fields.autologon_request_id(), Some(1));

    // GetCredentialCount: the tile becomes the default and auto-submits once.
    assert_eq!(
        fields.autologon_report(true, 0),
        CredentialCountReport {
            count: 1,
            default_index: Some(0),
            autologon: true,
        }
    );
    // A repeated GetCredentialCount must not auto-submit again.
    assert_eq!(
        fields.autologon_report(true, 0),
        CredentialCountReport {
            count: 1,
            default_index: Some(0),
            autologon: false,
        }
    );

    // GetSerialization consumes the credential exactly once.
    let (username, password) = fields.take_autologon(0).expect("pending autologon");
    assert_eq!(username, USERNAME);
    assert_eq!(
        password.as_utf16(),
        SecretWide::from_text(PASSWORD).as_utf16()
    );
    assert!(!fields.has_pending_autologon());

    // A second GetSerialization / ReportResult finds nothing to submit and clears.
    assert!(fields.take_autologon(0).is_none());
    fields.reset_after_result();
    assert!(!fields.has_pending_autologon());
    assert_eq!(
        fields.autologon_report(true, 0),
        CredentialCountReport {
            count: 1,
            default_index: None,
            autologon: false,
        }
    );
}

/// A wrong-key push is not armed: the fields stay in the manual state, so LogonUI
/// never auto-submits a credential the provider could not decrypt.
#[test]
fn a_push_that_fails_to_decrypt_arms_no_autologon() {
    use arcen_cp_ipc::transport::FrameIo;

    let fields: Arc<Mutex<CredentialFields>> = Arc::new(Mutex::new(CredentialFields::new()));
    let (mut broker, mut provider) = mem_frame_duplex();

    let fields_worker = Arc::clone(&fields);
    let provider_thread = thread::spawn(move || {
        let mut session = CredentialSession::generate(60_000).expect("session");
        let identity = ProviderIdentity {
            clsid: CLSID.to_string(),
            usage: UsageScenario::Logon,
            pid: 4321,
        };
        let _ = provider_serve(
            &mut provider,
            &mut session,
            &identity,
            &|| 0,
            |request_id, payload| {
                fields_worker.lock().expect("fields").arm_autologon(
                    payload.username().to_string(),
                    SecretWide::from_text(payload.password()),
                    request_id,
                    60_000,
                );
                Ok(())
            },
        );
    });

    // Seal against a *different* recipient key so the provider's decrypt fails.
    let mut tracker = NonceTracker::new();
    let _readiness = recv_ready(&mut broker, &mut tracker).expect("readiness");
    let wrong = CredentialSession::generate(60_000).expect("session");
    let sealed = arcen_cp_ipc::crypto::seal_credential(
        &wrong.public_key().expect("public"),
        &wrong.challenge().expect("challenge"),
        ACCOUNT_SID.as_bytes(),
        1,
        arcen_cp_ipc::PROTOCOL_VERSION,
        &CredentialPayload::new(USERNAME, PASSWORD).expect("payload"),
    )
    .expect("seal");
    let push = arcen_cp_ipc::PushCredentials::from_sealed(1, ACCOUNT_SID, &sealed, 10);
    broker
        .write_message(&arcen_cp_ipc::CpMessage::PushCredentials(push))
        .expect("write push");
    let _ = provider_thread.join();

    assert!(!fields.lock().expect("fields").has_pending_autologon());
}
