//! Loopback proof of the lane carrier's peer liveness (`arcen_transport::
//! liveness`): a live Deck is heard through keep-alives on an idle
//! connection, a Deck whose packets stop arriving is proven silent and its
//! connection closed for a same-user newcomer, and another user's
//! attachment is never probed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use arcen_protocol::lanes::LaneEndpoint;
use arcen_transport::BoundedTransportPolicy;
use arcen_transport::liveness::{LANE_IDLE_TIMEOUT, PEER_SILENCE_PROOF, PeerLivenessVerdict};
use arcen_transport::quic::{
    IncumbentAttachment, IncumbentOutcome, PEER_SILENT_CLOSE_CODE, lane_transport_config_arc,
    prove_peer_liveness,
};
use support::{
    build_client_rustls_config, build_server_rustls_config, client_identity, server_identity,
};
use tokio::net::UdpSocket;

/// A UDP relay between one client and the server that can stop forwarding
/// in both directions, as a vanished peer would.
struct Relay {
    address: SocketAddr,
    paused: Arc<AtomicBool>,
}

async fn relay(server: SocketAddr) -> Relay {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    upstream.connect(server).await.unwrap();
    let paused = Arc::new(AtomicBool::new(false));
    let client = Arc::new(tokio::sync::Mutex::new(None::<SocketAddr>));
    let address = socket.local_addr().unwrap();
    {
        let (socket, upstream, paused, client) = (
            Arc::clone(&socket),
            Arc::clone(&upstream),
            Arc::clone(&paused),
            Arc::clone(&client),
        );
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; 65_536];
            while let Ok((length, from)) = socket.recv_from(&mut buffer).await {
                *client.lock().await = Some(from);
                if !paused.load(Ordering::SeqCst) {
                    let _ = upstream.send(&buffer[..length]).await;
                }
            }
        });
    }
    {
        let paused = Arc::clone(&paused);
        tokio::spawn(async move {
            let mut buffer = vec![0_u8; 65_536];
            while let Ok(length) = upstream.recv(&mut buffer).await {
                let target = *client.lock().await;
                if let (false, Some(target)) = (paused.load(Ordering::SeqCst), target) {
                    let _ = socket.send_to(&buffer[..length], target).await;
                }
            }
        });
    }
    Relay { address, paused }
}

/// Stops forwarding and lets datagrams already in flight arrive, so a
/// liveness proof that starts now sees a peer that is silent from its
/// first sample (a late in-flight datagram would read as alive).
async fn go_silent(relay: &Relay) {
    relay.paused.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(300)).await;
}

struct Pair {
    _server: quinn::Endpoint,
    _client: quinn::Endpoint,
    pier_side: quinn::Connection,
    _deck_side: quinn::Connection,
    relay: Relay,
}

async fn pair() -> Pair {
    let policy = BoundedTransportPolicy::default();
    let server_id = server_identity();
    let client_id = client_identity();
    let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(
        build_server_rustls_config(&server_id, &client_id.cert_der),
    )
    .unwrap();
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    server_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Pier));
    let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(
        build_client_rustls_config(&client_id, &server_id.cert_der),
    )
    .unwrap();
    let mut client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    client_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Deck));
    let server =
        quinn::Endpoint::server(server_config, "127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .unwrap();
    let relay = relay(server.local_addr().unwrap()).await;
    let client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let accepting = server.clone();
    let accept = tokio::spawn(async move { accepting.accept().await.unwrap().await.unwrap() });
    let deck_side = client
        .connect_with(client_config, relay.address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let pier_side = accept.await.unwrap();
    Pair {
        _server: server,
        _client: client,
        pier_side,
        _deck_side: deck_side,
        relay,
    }
}

#[tokio::test]
async fn an_idle_live_peer_is_heard_through_keepalives() {
    let pair = pair().await;
    // No application traffic at all: only the carrier's keep-alive and its
    // acknowledgement can prove the peer.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let started = tokio::time::Instant::now();
    let verdict = prove_peer_liveness(&pair.pier_side, PEER_SILENCE_PROOF).await;
    assert_eq!(verdict, PeerLivenessVerdict::Alive);
    assert!(started.elapsed() < PEER_SILENCE_PROOF);
}

#[tokio::test]
async fn a_vanished_peer_is_proven_silent_and_displaced_for_the_same_user() {
    let pair = pair().await;
    let record = Arc::new(IncumbentAttachment::new());
    let registration = record.register("alice", pair.pier_side.clone());

    go_silent(&pair.relay).await;
    assert_eq!(
        record
            .displace_if_silent("mallory", PEER_SILENCE_PROOF)
            .await,
        IncumbentOutcome::OtherOwner,
        "another user's attachment is never probed or closed"
    );
    assert!(pair.pier_side.close_reason().is_none());

    let started = tokio::time::Instant::now();
    let outcome = record.displace_if_silent("alice", PEER_SILENCE_PROOF).await;
    let IncumbentOutcome::Displaced { silent_for } = outcome else {
        panic!("expected displacement, got {outcome:?}");
    };
    assert!(silent_for >= PEER_SILENCE_PROOF);
    assert!(started.elapsed() < PEER_SILENCE_PROOF + Duration::from_secs(1));
    assert!(
        registration.displaced(),
        "the owner learns it was displaced"
    );
    match pair.pier_side.close_reason() {
        Some(quinn::ConnectionError::LocallyClosed) => {}
        other => panic!("expected a local close, got {other:?}"),
    }
    assert_eq!(
        record.displace_if_silent("alice", PEER_SILENCE_PROOF).await,
        IncumbentOutcome::AlreadyClosed
    );
    drop(registration);
    assert_eq!(
        record.displace_if_silent("alice", PEER_SILENCE_PROOF).await,
        IncumbentOutcome::NoIncumbent
    );
    let _ = PEER_SILENT_CLOSE_CODE;
}

#[tokio::test]
async fn a_live_peer_keeps_its_attachment() {
    let pair = pair().await;
    let record = Arc::new(IncumbentAttachment::new());
    let registration = record.register("alice", pair.pier_side.clone());
    assert_eq!(
        record.displace_if_silent("alice", PEER_SILENCE_PROOF).await,
        IncumbentOutcome::Alive
    );
    assert!(!registration.displaced());
    assert!(pair.pier_side.close_reason().is_none());
}

#[tokio::test]
async fn the_carrier_closes_a_silent_connection_within_its_idle_timeout() {
    let pair = pair().await;
    go_silent(&pair.relay).await;
    let started = tokio::time::Instant::now();
    let error = tokio::time::timeout(
        LANE_IDLE_TIMEOUT + Duration::from_secs(3),
        pair.pier_side.closed(),
    )
    .await
    .expect("the idle timeout closes the connection");
    assert!(
        matches!(error, quinn::ConnectionError::TimedOut),
        "{error:?}"
    );
    assert!(started.elapsed() + Duration::from_secs(1) >= LANE_IDLE_TIMEOUT);
}

#[tokio::test]
async fn an_incumbent_that_timed_out_before_the_check_is_handed_over() {
    let pair = pair().await;
    let record = Arc::new(IncumbentAttachment::new());
    let registration = record.register("alice", pair.pier_side.clone());
    // The old attachment ended on its own (its idle timeout) first.
    pair.pier_side.close(0_u32.into(), b"idle");
    assert_eq!(
        record.displace_if_silent("alice", PEER_SILENCE_PROOF).await,
        IncumbentOutcome::AlreadyClosed
    );
    assert!(
        registration.displaced(),
        "an owner waiting for a resume is told to hand over"
    );
    tokio::time::timeout(Duration::from_secs(1), registration.wait_displaced())
        .await
        .expect("the owner's wait resolves");
}

#[tokio::test]
async fn an_incumbent_that_timed_out_during_the_check_is_handed_over() {
    let pair = pair().await;
    let record = Arc::new(IncumbentAttachment::new());
    let registration = record.register("alice", pair.pier_side.clone());
    go_silent(&pair.relay).await;
    let waiter = {
        let watched = tokio::spawn({
            let record = Arc::clone(&record);
            async move { record.displace_if_silent("alice", PEER_SILENCE_PROOF).await }
        });
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(!registration.displaced());
        // The old attachment's own timeout fires mid-proof.
        pair.pier_side.close(0_u32.into(), b"idle");
        watched
    };
    let outcome = waiter.await.unwrap();
    assert!(
        matches!(
            outcome,
            IncumbentOutcome::AlreadyClosed | IncumbentOutcome::Displaced { .. }
        ),
        "{outcome:?}"
    );
    tokio::time::timeout(Duration::from_secs(1), registration.wait_displaced())
        .await
        .expect("the owner learns it was displaced");
}

#[tokio::test]
async fn a_registration_resumed_during_the_check_is_never_marked() {
    let old = pair().await;
    let resumed = pair().await;
    let record = Arc::new(IncumbentAttachment::new());
    let registration = Arc::new(record.register("alice", old.pier_side.clone()));
    go_silent(&old.relay).await;
    let check = tokio::spawn({
        let record = Arc::clone(&record);
        async move { record.displace_if_silent("alice", PEER_SILENCE_PROOF).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    // The Deck resumed on a new connection while the old one was proven.
    registration.replace_connection(resumed.pier_side.clone());
    assert_eq!(check.await.unwrap(), IncumbentOutcome::Alive);
    assert!(!registration.displaced());
    assert!(resumed.pier_side.close_reason().is_none());
}

#[tokio::test]
async fn a_mark_set_while_a_resume_was_validated_is_cleared_by_the_resumed_connection() {
    let old = pair().await;
    let resumed = pair().await;
    let record = Arc::new(IncumbentAttachment::new());
    let registration = record.register("1001#alice", old.pier_side.clone());
    // The old connection is gone; the owner is validating a resume and has
    // not moved the registration yet when a same-user check marks it.
    old.pier_side.close(0_u32.into(), b"lost");
    assert_eq!(
        record
            .displace_if_silent("1001#alice", PEER_SILENCE_PROOF)
            .await,
        IncumbentOutcome::AlreadyClosed
    );
    assert!(registration.displaced());
    registration.replace_connection(resumed.pier_side.clone());
    assert!(
        !registration.displaced(),
        "the resumed attachment is live; its next loss is not a hand-over"
    );
}
