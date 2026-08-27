//! Deterministic localhost integration test for the concrete QUIC transport
//! adapter.
//!
//! Uses real mutual TLS with static test-only certificates, a private
//! `rustls::RootCertStore` on both sides (never a "skip verification"
//! helper), and an explicit [`PeerIdentityAuthorizer`] that checks the exact
//! expected certificate bytes for the peer it authorizes.

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use arcen_identity::{
    ClientIdentity, GrantNonce, GrantReplayConsumer, GrantReplayConsumption, GrantReplayKey,
    GrantSignatureVerifier, GrantValidationContext, HostIdentity, SESSION_GRANT_VERSION_V1,
    SessionGrantClaims, SignedSessionGrant, consume_validated_session_grant,
    validate_session_grant,
};
use arcen_transport::quic::{
    self, DirectQuicDialParams, HandshakeRejectReason, IdentityAuthorizationError,
    PeerIdentityAuthorizer, PeerIdentityClaim, QuicAdmission, QuicDialParams, QuicPeer, QuicRole,
    QuicRuntimeConfig, QuicTransportError,
};
use arcen_transport::{
    AuthenticatedPeer, AuthorizationState, BoundedTransportPolicy, CAPABILITY_ENCRYPTED_DATAGRAM,
    CAPABILITY_RELIABLE_STREAM, CAPABILITY_TRANSPORT_QUIC, CapabilityId, DeliveryMechanism,
    EnvelopeMetadata, InboundEnvelope, MessageClass, NegotiatedCapabilities, OutboundEnvelope,
    PeerRole, ReliabilityClass, TransportEvent,
};
use support::{
    Issued, build_client_rustls_config, build_quinn_client_config, build_quinn_server_config,
    build_server_rustls_config, client_identity, server_identity,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Authorizes a peer only if it presents exactly the expected end-entity
/// certificate. This is a real, explicit check — never a permissive
/// allow-all shim.
struct ExactCertificateAuthorizer {
    expected: Vec<u8>,
    expected_identity: &'static str,
}

impl PeerIdentityAuthorizer for ExactCertificateAuthorizer {
    fn authorize(&self, claim: &PeerIdentityClaim<'_>) -> Result<(), IdentityAuthorizationError> {
        match claim.certificate_chain.first() {
            Some(leaf)
                if leaf.as_ref() == self.expected.as_slice()
                    && claim.expected_peer_identity == self.expected_identity =>
            {
                Ok(())
            }
            Some(_) => Err(IdentityAuthorizationError::Rejected(
                "certificate did not match expected identity".to_owned(),
            )),
            None => Err(IdentityAuthorizationError::NoCertificateChain),
        }
    }
}

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

fn consumed_grant(session_id: &str, nonce_value: &str) -> arcen_identity::ConsumedSessionGrant {
    let client = ClientIdentity::new("client-1").expect("client");
    let host = HostIdentity::new("host-1").expect("host");
    let nonce = GrantNonce::new(nonce_value).expect("nonce");
    let grant = SignedSessionGrant {
        claims: SessionGrantClaims {
            version: SESSION_GRANT_VERSION_V1,
            issuer: "arcen".to_owned(),
            audience: "transport".to_owned(),
            subject: "human-1".to_owned(),
            tenant: Some("tenant-1".to_owned()),
            client_identity: client.clone(),
            host_identity: host.clone(),
            session_id: session_id.to_owned(),
            nonce: nonce.clone(),
            issued_at: 100,
            expires_at: 200,
        },
        key_id: "test-key".to_owned(),
        algorithm: "test".to_owned(),
        signature: vec![1],
    };
    let validated = validate_session_grant(
        &AcceptSignature,
        &grant,
        GrantValidationContext {
            issuer: "arcen",
            audience: "transport",
            expected_subject: "human-1",
            expected_tenant: Some("tenant-1"),
            expected_client_identity: &client,
            expected_host_identity: &host,
            expected_session_id: session_id,
            expected_nonce: &nonce,
            now_epoch_seconds: 150,
        },
    )
    .expect("validated grant");
    consume_validated_session_grant(&ConsumeReplay, &validated, 150).expect("consumed grant")
}

fn capabilities() -> NegotiatedCapabilities {
    NegotiatedCapabilities::new(
        [
            CAPABILITY_TRANSPORT_QUIC,
            CAPABILITY_RELIABLE_STREAM,
            CAPABILITY_ENCRYPTED_DATAGRAM,
        ]
        .map(|name| CapabilityId::new(name).expect("capability")),
    )
    .expect("capabilities")
}

fn admission(role: PeerRole, session_id: &str, nonce: &str) -> QuicAdmission {
    let (local_identity, remote_role, remote_identity) = match role {
        PeerRole::Client => ("client-1", PeerRole::Host, "host-1"),
        PeerRole::Host => ("host-1", PeerRole::Client, "client-1"),
        PeerRole::Gateway => panic!("test only connects client and host"),
    };
    let required = NegotiatedCapabilities::new([]).expect("no extra requirements");
    QuicAdmission::new(
        consumed_grant(session_id, nonce),
        AuthenticatedPeer::new(role, local_identity).expect("local peer"),
        AuthenticatedPeer::new(remote_role, remote_identity).expect("remote peer"),
        capabilities(),
        &required,
        AuthorizationState::Authorized,
        150,
    )
    .expect("QUIC admission")
}

struct Harness {
    client_peer: QuicPeer,
    server_peer: QuicPeer,
    // Endpoints must outlive the peers' connections; never read directly,
    // only kept alive for the harness's lifetime.
    #[allow(dead_code)]
    client_endpoint: quinn::Endpoint,
    #[allow(dead_code)]
    server_endpoint: quinn::Endpoint,
}

async fn establish_pair(feedback_interval: Duration) -> Harness {
    let policy = BoundedTransportPolicy::default();
    let server_identity = server_identity();
    let client_identity = client_identity();

    let server_rustls = build_server_rustls_config(&server_identity, &client_identity.cert_der);
    let client_rustls = build_client_rustls_config(&client_identity, &server_identity.cert_der);

    let server_quinn_config = build_quinn_server_config(server_rustls, &policy);
    let client_quinn_config = build_quinn_client_config(client_rustls, &policy);

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("bind addr");
    let server_endpoint =
        quinn::Endpoint::server(server_quinn_config, bind_addr).expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local addr");

    let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind addr"))
        .expect("client endpoint");

    let server_authorizer = Arc::new(ExactCertificateAuthorizer {
        expected: client_identity.cert_der.as_ref().to_vec(),
        expected_identity: "client-1",
    });
    let client_authorizer = Arc::new(ExactCertificateAuthorizer {
        expected: server_identity.cert_der.as_ref().to_vec(),
        expected_identity: "host-1",
    });

    let mut server_runtime = QuicRuntimeConfig::new(server_authorizer);
    server_runtime.feedback_interval = feedback_interval;
    let mut client_runtime = QuicRuntimeConfig::new(client_authorizer);
    client_runtime.feedback_interval = feedback_interval;

    let server_endpoint_for_task = server_endpoint.clone();
    let accept_task = tokio::spawn(async move {
        let incoming = server_endpoint_for_task
            .accept()
            .await
            .expect("one incoming connection");
        let connection = incoming.await.expect("accepted connection");
        quic::accept(
            connection,
            admission(PeerRole::Host, "session-1", "server-nonce-1"),
            server_runtime,
        )
        .await
        .expect("server-side handshake")
    });

    let client_peer = quic::connect(QuicDialParams {
        endpoint: &client_endpoint,
        client_config: client_quinn_config,
        remote_addr: server_addr,
        server_name: "localhost",
        admission: admission(PeerRole::Client, "session-1", "client-nonce-1"),
        runtime: client_runtime,
    })
    .await
    .expect("client-side handshake");

    let server_peer = accept_task.await.expect("accept task join");

    Harness {
        client_peer,
        server_peer,
        client_endpoint,
        server_endpoint,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_stream_round_trips_without_claiming_application_admission() {
    let policy = BoundedTransportPolicy::default();
    let server_identity = server_identity();
    let client_identity = client_identity();
    let server_config = build_quinn_server_config(
        build_server_rustls_config(&server_identity, &client_identity.cert_der),
        &policy,
    );
    let client_config = build_quinn_client_config(
        build_client_rustls_config(&client_identity, &server_identity.cert_der),
        &policy,
    );
    let server_endpoint = quinn::Endpoint::server(
        server_config,
        "127.0.0.1:0".parse().expect("server bind address"),
    )
    .expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local address");
    let client_endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind address"))
            .expect("client endpoint");

    let server_endpoint_for_task = server_endpoint.clone();
    let accept_task = tokio::spawn(async move {
        let incoming = server_endpoint_for_task
            .accept()
            .await
            .expect("incoming connection");
        let connection = incoming.await.expect("accepted connection");
        let mut stream = quic::accept_direct(connection)
            .await
            .expect("accepted direct stream");
        stream
            .write_all(b"direct-reply")
            .await
            .expect("write direct response");
        let mut request = [0_u8; 12];
        stream
            .read_exact(&mut request)
            .await
            .expect("read direct request");
        assert_eq!(&request, b"direct-hello");
        stream.shutdown().await.expect("finish server stream");
        let mut acknowledgement = [0_u8; 1];
        stream
            .read_exact(&mut acknowledgement)
            .await
            .expect("read client acknowledgement");
        assert_eq!(acknowledgement, [1]);
    });

    let mut stream = quic::connect_direct(DirectQuicDialParams {
        endpoint: client_endpoint,
        client_config,
        remote_addr: server_addr,
        server_name: "localhost",
    })
    .await
    .expect("connect direct stream");
    assert_eq!(stream.remote_address(), server_addr);
    assert_eq!(stream.feedback_snapshot().current_mtu, 1_200);
    let mut response = [0_u8; 12];
    stream
        .read_exact(&mut response)
        .await
        .expect("read direct response");
    assert_eq!(&response, b"direct-reply");
    stream
        .write_all(b"direct-hello")
        .await
        .expect("write direct request");
    stream
        .write_all(&[1])
        .await
        .expect("write client acknowledgement");
    stream.shutdown().await.expect("finish client stream");

    accept_task.await.expect("accept task join");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_stream_rejects_an_invalid_preface() {
    let policy = BoundedTransportPolicy::default();
    let server_identity = server_identity();
    let client_identity = client_identity();
    let server_config = build_quinn_server_config(
        build_server_rustls_config(&server_identity, &client_identity.cert_der),
        &policy,
    );
    let client_config = build_quinn_client_config(
        build_client_rustls_config(&client_identity, &server_identity.cert_der),
        &policy,
    );
    let server_endpoint = quinn::Endpoint::server(
        server_config,
        "127.0.0.1:0".parse().expect("server bind address"),
    )
    .expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local address");
    let client_endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind address"))
            .expect("client endpoint");

    let server_endpoint_for_task = server_endpoint.clone();
    let accept_task = tokio::spawn(async move {
        let incoming = server_endpoint_for_task
            .accept()
            .await
            .expect("incoming connection");
        let connection = incoming.await.expect("accepted connection");
        quic::accept_direct(connection).await
    });

    let connection = client_endpoint
        .connect_with(client_config, server_addr, "localhost")
        .expect("start client connection")
        .await
        .expect("client connection");
    let (mut send, _recv) = connection.open_bi().await.expect("open direct stream");
    send.write_all(b"wrong-preface-v1")
        .await
        .expect("write invalid preface");

    let result = accept_task.await.expect("accept task join");
    assert!(matches!(result, Err(QuicTransportError::DirectPreface)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_stream_delivers_final_server_data_before_implicit_close() {
    let policy = BoundedTransportPolicy::default();
    let server_identity = server_identity();
    let client_identity = client_identity();
    let server_config = build_quinn_server_config(
        build_server_rustls_config(&server_identity, &client_identity.cert_der),
        &policy,
    );
    let client_config = build_quinn_client_config(
        build_client_rustls_config(&client_identity, &server_identity.cert_der),
        &policy,
    );
    let server_endpoint = quinn::Endpoint::server(
        server_config,
        "127.0.0.1:0".parse().expect("server bind address"),
    )
    .expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local address");
    let client_endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind address"))
            .expect("client endpoint");

    let server_endpoint_for_task = server_endpoint.clone();
    let server = tokio::spawn(async move {
        let incoming = server_endpoint_for_task
            .accept()
            .await
            .expect("incoming connection");
        let connection = incoming.await.expect("accepted connection");
        let mut stream = quic::accept_direct(connection)
            .await
            .expect("accepted direct stream");
        stream
            .write_all(b"terminal-control")
            .await
            .expect("write terminal control");
    });

    let mut stream = quic::connect_direct(DirectQuicDialParams {
        endpoint: client_endpoint,
        client_config,
        remote_addr: server_addr,
        server_name: "localhost",
    })
    .await
    .expect("connect direct stream");
    let mut response = [0_u8; 16];
    stream
        .read_exact(&mut response)
        .await
        .expect("read terminal control");
    assert_eq!(&response, b"terminal-control");
    server.await.expect("server task join");
}

fn envelope(
    reliability: ReliabilityClass,
    delivery: DeliveryMechanism,
    payload: &[u8],
    sequence: u64,
    peer_identity: &str,
) -> OutboundEnvelope {
    let message_class = match reliability {
        ReliabilityClass::Control => MessageClass::Control,
        ReliabilityClass::MediaReliable | ReliabilityClass::MediaLowLatency => MessageClass::Media,
        ReliabilityClass::InputLowLatency => MessageClass::Input,
    };
    OutboundEnvelope::new(
        EnvelopeMetadata {
            message_class,
            reliability,
            delivery,
            declared_size: 0,
            sequence,
            session_id: "session-1".to_owned(),
            peer_identity: peer_identity.to_owned(),
        },
        payload.to_vec(),
    )
    .expect("bounded envelope")
}

fn control_envelope(payload: &[u8], sequence: u64, peer_identity: &str) -> OutboundEnvelope {
    envelope(
        ReliabilityClass::Control,
        DeliveryMechanism::ReliableStream,
        payload,
        sequence,
        peer_identity,
    )
}

fn media_reliable_envelope(payload: &[u8], sequence: u64, peer_identity: &str) -> OutboundEnvelope {
    envelope(
        ReliabilityClass::MediaReliable,
        DeliveryMechanism::ReliableStream,
        payload,
        sequence,
        peer_identity,
    )
}

fn datagram_envelope(payload: &[u8], sequence: u64, peer_identity: &str) -> OutboundEnvelope {
    envelope(
        ReliabilityClass::MediaLowLatency,
        DeliveryMechanism::EncryptedDatagram,
        payload,
        sequence,
        peer_identity,
    )
}

async fn expect_message(peer: &QuicPeer) -> InboundEnvelope {
    tokio::time::timeout(Duration::from_secs(5), peer.recv_message())
        .await
        .expect("message arrives before timeout")
        .expect("peer is not closed")
}

async fn expect_event(peer: &QuicPeer) -> TransportEvent {
    tokio::time::timeout(Duration::from_secs(5), peer.recv_event())
        .await
        .expect("event arrives before timeout")
        .expect("peer is not closed")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handshake_binds_session_and_agrees_remote_role() {
    let harness = establish_pair(Duration::from_secs(60)).await;

    assert_eq!(harness.client_peer.binding().session_id(), "session-1");
    assert_eq!(harness.server_peer.binding().session_id(), "session-1");
    assert_eq!(harness.client_peer.remote_role(), QuicRole::Host);
    assert_eq!(harness.server_peer.remote_role(), QuicRole::Client);
    assert_eq!(harness.client_peer.local_role(), QuicRole::Client);
    assert_eq!(harness.server_peer.local_role(), QuicRole::Host);

    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connected_and_session_bound_events_are_observed_on_both_sides() {
    let harness = establish_pair(Duration::from_secs(60)).await;

    let client_first = expect_event(&harness.client_peer).await;
    let client_second = expect_event(&harness.client_peer).await;
    assert!(matches!(client_first, TransportEvent::Connected(_)));
    assert!(matches!(
        client_second,
        TransportEvent::SessionBound(connection_id)
            if connection_id == harness.client_peer.connection_id()
    ));

    let server_first = expect_event(&harness.server_peer).await;
    let server_second = expect_event(&harness.server_peer).await;
    assert!(matches!(server_first, TransportEvent::Connected(_)));
    assert!(matches!(
        server_second,
        TransportEvent::SessionBound(connection_id)
            if connection_id == harness.server_peer.connection_id()
    ));

    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn binding_rejects_a_tls_peer_not_approved_for_the_claimed_session() {
    let policy = BoundedTransportPolicy::default();
    let server_identity = server_identity();
    let client_identity = client_identity();
    let server_rustls = build_server_rustls_config(&server_identity, &client_identity.cert_der);
    let client_rustls = build_client_rustls_config(&client_identity, &server_identity.cert_der);
    let server_config = build_quinn_server_config(server_rustls, &policy);
    let client_config = build_quinn_client_config(client_rustls, &policy);

    let server_endpoint = quinn::Endpoint::server(
        server_config,
        "127.0.0.1:0".parse().expect("server bind address"),
    )
    .expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local address");
    let client_endpoint =
        quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind address"))
            .expect("client endpoint");

    // TLS trusts the real client certificate, but the binding authorizer
    // deliberately expects a different identity. This proves the Arcen
    // role/session authorization layer fails closed after TLS succeeds.
    let rejecting_authorizer = Arc::new(ExactCertificateAuthorizer {
        expected: server_identity.cert_der.as_ref().to_vec(),
        expected_identity: "client-1",
    });
    let server_endpoint_for_task = server_endpoint.clone();
    let accept_task = tokio::spawn(async move {
        let incoming = server_endpoint_for_task
            .accept()
            .await
            .expect("incoming connection");
        let connection = incoming.await.expect("TLS-authenticated connection");
        quic::accept(
            connection,
            admission(PeerRole::Host, "session-1", "server-reject-nonce"),
            QuicRuntimeConfig::new(rejecting_authorizer),
        )
        .await
    });

    let client_authorizer = Arc::new(ExactCertificateAuthorizer {
        expected: server_identity.cert_der.as_ref().to_vec(),
        expected_identity: "host-1",
    });
    let client_result = quic::connect(QuicDialParams {
        endpoint: &client_endpoint,
        client_config,
        remote_addr: server_addr,
        server_name: "localhost",
        admission: admission(PeerRole::Client, "session-1", "client-reject-nonce"),
        runtime: QuicRuntimeConfig::new(client_authorizer),
    })
    .await;
    let server_result = accept_task.await.expect("accept task join");

    assert!(matches!(
        server_result,
        Err(QuicTransportError::Unauthorized(_))
    ));
    assert!(
        matches!(
            &client_result,
            Err(QuicTransportError::Handshake(
                HandshakeRejectReason::Unauthorized
            ))
        ),
        "unexpected client rejection: {client_result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reliable_stream_preserves_order_for_control_and_media() {
    let harness = establish_pair(Duration::from_secs(60)).await;

    harness
        .client_peer
        .send(control_envelope(b"control-1", 0, "client-1"))
        .await
        .expect("send control-1");
    harness
        .client_peer
        .send(media_reliable_envelope(b"media-1", 1, "client-1"))
        .await
        .expect("send media-1");
    harness
        .client_peer
        .send(control_envelope(b"control-2", 2, "client-1"))
        .await
        .expect("send control-2");

    let first = expect_message(&harness.server_peer).await;
    assert_eq!(first.metadata.reliability, ReliabilityClass::Control);
    assert_eq!(first.metadata.delivery, DeliveryMechanism::ReliableStream);
    assert_eq!(first.payload, b"control-1");
    let second = expect_message(&harness.server_peer).await;
    assert_eq!(second.metadata.reliability, ReliabilityClass::MediaReliable);
    assert_eq!(second.metadata.delivery, DeliveryMechanism::ReliableStream);
    assert_eq!(second.payload, b"media-1");
    let third = expect_message(&harness.server_peer).await;
    assert_eq!(third.metadata.reliability, ReliabilityClass::Control);
    assert_eq!(third.metadata.delivery, DeliveryMechanism::ReliableStream);
    assert_eq!(third.payload, b"control-2");

    // And the reverse direction independently preserves order too.
    harness
        .server_peer
        .send(control_envelope(b"reply-1", 0, "host-1"))
        .await
        .expect("send reply-1");
    harness
        .server_peer
        .send(control_envelope(b"reply-2", 1, "host-1"))
        .await
        .expect("send reply-2");
    assert_eq!(
        expect_message(&harness.client_peer).await.payload,
        b"reply-1"
    );
    assert_eq!(
        expect_message(&harness.client_peer).await.payload,
        b"reply-2"
    );

    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encrypted_datagram_carries_low_latency_media() {
    let harness = establish_pair(Duration::from_secs(60)).await;

    harness
        .client_peer
        .send(datagram_envelope(b"low-latency-frame", 0, "client-1"))
        .await
        .expect("send datagram");

    let received = expect_message(&harness.server_peer).await;
    assert_eq!(
        received.metadata.reliability,
        ReliabilityClass::MediaLowLatency
    );
    assert_eq!(
        received.metadata.delivery,
        DeliveryMechanism::EncryptedDatagram
    );
    assert_eq!(received.payload, b"low-latency-frame");
    assert_eq!(harness.client_peer.counters().datagrams_sent, 1);

    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn feedback_snapshot_is_available_synchronously_and_via_events() {
    let harness = establish_pair(Duration::from_millis(50)).await;

    // Synchronous snapshot works immediately without waiting on the ticker.
    let snapshot = harness.client_peer.feedback_snapshot();
    assert!(snapshot.current_mtu > 0);

    // The periodic ticker also emits explicit feedback events.
    let mut saw_feedback = false;
    for _ in 0..10 {
        if let TransportEvent::Feedback(_) = expect_event(&harness.client_peer).await {
            saw_feedback = true;
            break;
        }
    }
    assert!(saw_feedback, "expected at least one Feedback event");

    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_is_observed_as_an_explicit_event_on_the_peer() {
    let harness = establish_pair(Duration::from_secs(60)).await;

    harness.client_peer.close();

    let mut saw_closed = false;
    for _ in 0..10 {
        if let TransportEvent::Closed = expect_event(&harness.server_peer).await {
            saw_closed = true;
            break;
        }
    }
    assert!(saw_closed, "expected a Closed event on the peer");

    drop(harness);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_close_terminates_receivers_after_queued_events_are_drained() {
    let harness = establish_pair(Duration::from_secs(60)).await;
    harness.client_peer.close();

    while let Some(event) = harness.client_peer.recv_event().await {
        if event == TransportEvent::Closed {
            break;
        }
    }
    assert_eq!(harness.client_peer.recv_event().await, None);
    assert_eq!(harness.client_peer.recv_message().await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocked_event_receiver_observes_closed_before_terminal_none() {
    let harness = establish_pair(Duration::from_secs(60)).await;
    let mut saw_connected = false;
    let mut saw_bound = false;
    loop {
        match tokio::time::timeout(Duration::from_millis(25), harness.client_peer.recv_event())
            .await
        {
            Ok(Some(TransportEvent::Connected(_))) => saw_connected = true,
            Ok(Some(TransportEvent::SessionBound(_))) => saw_bound = true,
            Ok(Some(_)) => {}
            Ok(None) => panic!("peer closed before the close test began"),
            Err(_) => break,
        }
    }
    assert!(saw_connected);
    assert!(saw_bound);

    let waiting = harness.client_peer.recv_event();
    tokio::pin!(waiting);
    tokio::select! {
        event = &mut waiting => panic!("event receiver completed before close: {event:?}"),
        () = tokio::time::sleep(Duration::from_millis(10)) => {}
    }

    harness.client_peer.close();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), &mut waiting)
            .await
            .expect("blocked receiver wakes on close"),
        Some(TransportEvent::Closed)
    );
    assert_eq!(harness.client_peer.recv_event().await, None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_identifies_previous_and_replacement_connection() {
    let policy = BoundedTransportPolicy::default();
    let server_identity = server_identity();
    let client_identity = client_identity();
    let server_rustls = build_server_rustls_config(&server_identity, &client_identity.cert_der);
    let server_quinn_config = build_quinn_server_config(server_rustls, &policy);

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("bind addr");
    let server_endpoint =
        quinn::Endpoint::server(server_quinn_config, bind_addr).expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local addr");
    let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind addr"))
        .expect("client endpoint");

    // Accept two connections in sequence: the initial one and the reconnect.
    let server_endpoint_for_task = server_endpoint.clone();
    let client_identity_der = client_identity.cert_der.clone();
    let server_identity_der = server_identity.cert_der.clone();
    let accept_task = tokio::spawn(async move {
        let mut peers = Vec::with_capacity(2);
        for index in 0..2 {
            let authorizer = Arc::new(ExactCertificateAuthorizer {
                expected: client_identity_der.as_ref().to_vec(),
                expected_identity: "client-1",
            });
            let incoming = server_endpoint_for_task
                .accept()
                .await
                .expect("incoming connection");
            let connection = incoming.await.expect("accepted connection");
            let runtime = QuicRuntimeConfig::new(authorizer);
            let nonce = format!("server-reconnect-{index}");
            let peer = quic::accept(
                connection,
                admission(PeerRole::Host, "session-reconnect", &nonce),
                runtime,
            )
            .await
            .expect("server-side handshake");
            peers.push(peer);
        }
        peers
    });

    let make_client_config = |client: &Issued| {
        build_quinn_client_config(
            build_client_rustls_config(client, &server_identity_der),
            &policy,
        )
    };

    let client_authorizer = Arc::new(ExactCertificateAuthorizer {
        expected: server_identity_der.as_ref().to_vec(),
        expected_identity: "host-1",
    });
    let first_peer = quic::connect(QuicDialParams {
        endpoint: &client_endpoint,
        client_config: make_client_config(&client_identity),
        remote_addr: server_addr,
        server_name: "localhost",
        admission: admission(PeerRole::Client, "session-reconnect", "client-reconnect-0"),
        runtime: QuicRuntimeConfig::new(client_authorizer.clone()),
    })
    .await
    .expect("initial client handshake");
    let first_id = first_peer.connection_id();

    let second_peer = quic::reconnect(
        &first_peer,
        QuicDialParams {
            endpoint: &client_endpoint,
            client_config: make_client_config(&client_identity),
            remote_addr: server_addr,
            server_name: "localhost",
            admission: admission(PeerRole::Client, "session-reconnect", "client-reconnect-1"),
            runtime: QuicRuntimeConfig::new(client_authorizer),
        },
    )
    .await
    .expect("reconnect handshake");
    let second_id = second_peer.connection_id();

    assert_ne!(first_id, second_id);

    let mut saw_reconnecting = false;
    for _ in 0..10 {
        if let TransportEvent::Reconnecting = expect_event(&first_peer).await {
            saw_reconnecting = true;
            break;
        }
    }
    assert!(saw_reconnecting, "expected Reconnecting on the old peer");

    let mut saw_reconnected = false;
    for _ in 0..10 {
        if let TransportEvent::Reconnected(id) = expect_event(&second_peer).await {
            assert_eq!(id, second_id);
            saw_reconnected = true;
            break;
        }
    }
    assert!(saw_reconnected, "expected Reconnected on the new peer");

    let _server_peers = accept_task.await.expect("accept task join");
    drop(first_peer);
    drop(second_peer);
    drop(client_endpoint);
    drop(server_endpoint);
}
