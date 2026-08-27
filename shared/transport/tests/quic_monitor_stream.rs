//! Real Quinn loopback tests for the additive direct-monitor QUIC stream
//! foundation ("Carrier B"): the bounded preface, the server-open/client-accept
//! helpers, and the pure expected-roster registry. These tests never select
//! Carrier B as a product default; they only exercise the shared,
//! not-yet-product-selected foundation described in
//! `docs/architecture/transport.md`.
//!
//! Uses the same real mutual-TLS fixtures as `quic_localhost.rs` via the
//! shared `support` module (never a "skip verification" helper).

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroU64};
use std::time::Duration;

use arcen_transport::BoundedTransportPolicy;
use arcen_transport::quic::{
    ExpectedMonitorStream, MAX_MONITOR_STREAMS_PER_CONNECTION, MEDIA_PLAN_FINGERPRINT_BYTES,
    MonitorRosterError, MonitorStreamIdentity, MonitorStreamPrefaceError, MonitorStreamRoster,
    QuicTransportError, accept_monitor_stream, open_monitor_stream,
};
use quinn::Connection;
use support::{
    build_client_rustls_config, build_quinn_client_config,
    build_quinn_client_config_for_monitor_carrier, build_quinn_server_config,
    build_quinn_server_config_for_monitor_carrier, build_server_rustls_config, client_identity,
    server_identity,
};

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(5);

fn nz16(value: u16) -> NonZeroU16 {
    NonZeroU16::new(value).expect("nonzero")
}

fn nz64(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("nonzero")
}

fn fingerprint(byte: u8) -> [u8; MEDIA_PLAN_FINGERPRINT_BYTES] {
    [byte; MEDIA_PLAN_FINGERPRINT_BYTES]
}

fn identity(
    session_id: &str,
    attachment_generation: u64,
    topology_generation: u64,
    monitor_id: u16,
    fingerprint_byte: u8,
) -> MonitorStreamIdentity {
    MonitorStreamIdentity::new(
        session_id,
        nz64(attachment_generation),
        nz64(topology_generation),
        nz16(monitor_id),
        fingerprint(fingerprint_byte),
    )
    .expect("valid monitor stream identity")
}

/// A connected client/server QUIC connection pair with no admission or
/// binding-handshake layer above it — deliberately as raw as
/// `connect_direct`/`accept_direct` operate, since the direct-monitor stream
/// foundation sits directly on top of the connection like Carrier A does.
struct ConnectedPair {
    // Endpoints must outlive the connections; never read directly, only
    // kept alive for the pair's lifetime.
    #[allow(dead_code)]
    client_endpoint: quinn::Endpoint,
    #[allow(dead_code)]
    server_endpoint: quinn::Endpoint,
    client_connection: Connection,
    server_connection: Connection,
}

async fn connected_pair() -> ConnectedPair {
    connected_pair_with(build_quinn_server_config, build_quinn_client_config).await
}

/// Same connection setup as [`connected_pair`], except both sides use the
/// test-only `monitor_carrier_transport_config_arc` (concurrent
/// unidirectional stream limit raised to
/// [`MAX_MONITOR_STREAMS_PER_CONNECTION`]). Only tests that genuinely need
/// more than one concurrent monitor stream open at once should call this
/// instead of `connected_pair`; every other test proves the unmodified live
/// `recommended_transport_config` (limit of 1) is enough.
async fn connected_pair_for_monitor_carrier() -> ConnectedPair {
    connected_pair_with(
        build_quinn_server_config_for_monitor_carrier,
        build_quinn_client_config_for_monitor_carrier,
    )
    .await
}

async fn connected_pair_with(
    build_server_config: fn(rustls::ServerConfig, &BoundedTransportPolicy) -> quinn::ServerConfig,
    build_client_config: fn(rustls::ClientConfig, &BoundedTransportPolicy) -> quinn::ClientConfig,
) -> ConnectedPair {
    let policy = BoundedTransportPolicy::default();
    let server_identity = server_identity();
    let client_identity = client_identity();

    let server_config = build_server_config(
        build_server_rustls_config(&server_identity, &client_identity.cert_der),
        &policy,
    );
    let client_config = build_client_config(
        build_client_rustls_config(&client_identity, &server_identity.cert_der),
        &policy,
    );

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().expect("bind addr");
    let server_endpoint =
        quinn::Endpoint::server(server_config, bind_addr).expect("server endpoint");
    let server_addr = server_endpoint.local_addr().expect("server local addr");
    let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().expect("client bind addr"))
        .expect("client endpoint");

    let server_endpoint_for_task = server_endpoint.clone();
    let accept_task = tokio::spawn(async move {
        let incoming = server_endpoint_for_task
            .accept()
            .await
            .expect("incoming connection");
        incoming.await.expect("accepted connection")
    });

    let client_connection = client_endpoint
        .connect_with(client_config, server_addr, "localhost")
        .expect("start client connection")
        .await
        .expect("client connection");
    let server_connection = accept_task.await.expect("accept task join");

    ConnectedPair {
        client_endpoint,
        server_endpoint,
        client_connection,
        server_connection,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn single_monitor_stream_preface_is_visible_before_payload() {
    let pair = connected_pair().await;
    let expected_identity = identity("session-1", 1, 1, 1, 7);

    // `open_monitor_stream` returns as soon as it has opened the stream and
    // written/handed off the entire preface — no payload and no `finish()`
    // yet. `send` is still owned right here, unfinished, proving whatever
    // the client observes next can only be the preface itself.
    let mut send = open_monitor_stream(&pair.server_connection, &expected_identity)
        .await
        .expect("open monitor stream");

    // The client fully accepts and parses the preface while the sender
    // above is still holding the open, unfinished stream (no payload sent,
    // no `finish()` called): this is the direct proof that
    // `open_monitor_stream` writes/flushes the preface immediately, rather
    // than only becoming visible once the caller sends more data or closes
    // the stream.
    let (mut recv, parsed_identity) =
        accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
            .await
            .expect("accept monitor stream");
    assert_eq!(parsed_identity, expected_identity);

    // Only now does the sender continue: payload written after the preface
    // was already fully visible and parsed by the peer.
    send.write_all(b"monitor-frame-bytes")
        .await
        .expect("write payload");
    send.finish().expect("finish send stream");

    let payload = recv.read_to_end(64).await.expect("read monitor payload");
    assert_eq!(payload, b"monitor-frame-bytes");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_monitor_streams_round_trip_and_registry_becomes_ready() {
    let pair = connected_pair_for_monitor_carrier().await;
    let attachment_generation = 3;
    let topology_generation = 9;
    let monitors: Vec<(u16, u8)> = vec![(1, 10), (2, 20), (3, 30), (4, 40)];

    let server_monitors = monitors.clone();
    let server = tokio::spawn(async move {
        for (monitor_id, fingerprint_byte) in &server_monitors {
            let preface_identity = identity(
                "session-4",
                attachment_generation,
                topology_generation,
                *monitor_id,
                *fingerprint_byte,
            );
            let mut send = open_monitor_stream(&pair.server_connection, &preface_identity)
                .await
                .unwrap_or_else(|error| panic!("open monitor {monitor_id}: {error}"));
            send.finish().expect("finish send stream");
        }
        pair
    });

    let mut roster = MonitorStreamRoster::new(
        "session-4",
        nz64(attachment_generation),
        nz64(topology_generation),
        monitors
            .iter()
            .map(|(monitor_id, fingerprint_byte)| ExpectedMonitorStream {
                session_monitor_id: nz16(*monitor_id),
                media_plan_fingerprint: fingerprint(*fingerprint_byte),
            }),
    )
    .expect("bounded roster");
    assert_eq!(roster.expected_len(), 4);

    let pair = server.await.expect("server task join");

    for _ in 0..monitors.len() {
        let (_recv, parsed_identity) =
            accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
                .await
                .expect("accept monitor stream");
        roster
            .register(&parsed_identity)
            .expect("registers against roster");
    }

    assert!(roster.is_ready());
    assert!(roster.missing_monitors().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn out_of_order_monitor_registration_still_reaches_ready() {
    let pair = connected_pair_for_monitor_carrier().await;
    let attachment_generation = 1;
    let topology_generation = 1;
    // Server deliberately opens streams in a scrambled monitor-id order.
    let scrambled_order: Vec<(u16, u8)> = vec![(3, 30), (1, 10), (4, 40), (2, 20)];

    let server_order = scrambled_order.clone();
    let server = tokio::spawn(async move {
        for (monitor_id, fingerprint_byte) in &server_order {
            let preface_identity = identity(
                "session-scrambled",
                attachment_generation,
                topology_generation,
                *monitor_id,
                *fingerprint_byte,
            );
            let mut send = open_monitor_stream(&pair.server_connection, &preface_identity)
                .await
                .unwrap_or_else(|error| panic!("open monitor {monitor_id}: {error}"));
            send.finish().expect("finish send stream");
        }
        pair
    });

    let mut roster = MonitorStreamRoster::new(
        "session-scrambled",
        nz64(attachment_generation),
        nz64(topology_generation),
        [1_u16, 2, 3, 4].into_iter().zip([10_u8, 20, 30, 40]).map(
            |(monitor_id, fingerprint_byte)| ExpectedMonitorStream {
                session_monitor_id: nz16(monitor_id),
                media_plan_fingerprint: fingerprint(fingerprint_byte),
            },
        ),
    )
    .expect("bounded roster");

    let pair = server.await.expect("server task join");
    let mut registration_order = Vec::new();
    for _ in 0..scrambled_order.len() {
        let (_recv, parsed_identity) =
            accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
                .await
                .expect("accept monitor stream");
        registration_order.push(parsed_identity.session_monitor_id().get());
        roster
            .register(&parsed_identity)
            .expect("registers against roster");
    }

    // The registration order observed matches the server's scrambled send
    // order (never numeric 1..=4), yet the roster still reaches readiness.
    assert_eq!(registration_order, vec![3, 1, 4, 2]);
    assert!(roster.is_ready());
    assert!(roster.missing_monitors().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_monitor_stream_is_rejected_by_registry() {
    let pair = connected_pair_for_monitor_carrier().await;
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let preface_identity = identity("session-dup", 1, 1, 1, 7);
            let mut send = open_monitor_stream(&pair.server_connection, &preface_identity)
                .await
                .expect("open monitor stream");
            send.finish().expect("finish send stream");
        }
        pair
    });

    let mut roster = MonitorStreamRoster::new(
        "session-dup",
        nz64(1),
        nz64(1),
        [ExpectedMonitorStream {
            session_monitor_id: nz16(1),
            media_plan_fingerprint: fingerprint(7),
        }],
    )
    .expect("bounded roster");

    let pair = server.await.expect("server task join");
    let (_recv_first, first_identity) =
        accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
            .await
            .expect("accept first monitor stream");
    roster.register(&first_identity).expect("first registers");

    let (_recv_second, second_identity) =
        accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
            .await
            .expect("accept second monitor stream");
    assert_eq!(
        roster.register(&second_identity),
        Err(MonitorRosterError::DuplicateMonitor)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_monitor_stream_is_rejected_by_registry() {
    let pair = connected_pair().await;
    let server = tokio::spawn(async move {
        let preface_identity = identity("session-unknown", 1, 1, 2, 7);
        let mut send = open_monitor_stream(&pair.server_connection, &preface_identity)
            .await
            .expect("open monitor stream");
        send.finish().expect("finish send stream");
        pair
    });

    let mut roster = MonitorStreamRoster::new(
        "session-unknown",
        nz64(1),
        nz64(1),
        [ExpectedMonitorStream {
            session_monitor_id: nz16(1),
            media_plan_fingerprint: fingerprint(7),
        }],
    )
    .expect("bounded roster");

    let pair = server.await.expect("server task join");
    let (_recv, parsed_identity) = accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
        .await
        .expect("accept monitor stream");
    assert_eq!(
        roster.register(&parsed_identity),
        Err(MonitorRosterError::UnknownMonitor)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_generation_stream_is_rejected_by_registry() {
    let pair = connected_pair().await;
    let server = tokio::spawn(async move {
        // Claims the previous attachment generation (1), while the roster
        // below is scoped to generation 2.
        let preface_identity = identity("session-stale", 1, 1, 1, 7);
        let mut send = open_monitor_stream(&pair.server_connection, &preface_identity)
            .await
            .expect("open monitor stream");
        send.finish().expect("finish send stream");
        pair
    });

    let mut roster = MonitorStreamRoster::new(
        "session-stale",
        nz64(2),
        nz64(1),
        [ExpectedMonitorStream {
            session_monitor_id: nz16(1),
            media_plan_fingerprint: fingerprint(7),
        }],
    )
    .expect("bounded roster");

    let pair = server.await.expect("server task join");
    let (_recv, parsed_identity) = accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
        .await
        .expect("accept monitor stream");
    assert_eq!(
        roster.register(&parsed_identity),
        Err(MonitorRosterError::StaleAttachmentGeneration)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn wrong_fingerprint_stream_is_rejected_by_registry() {
    let pair = connected_pair().await;
    let server = tokio::spawn(async move {
        let preface_identity = identity("session-fp", 1, 1, 1, 99);
        let mut send = open_monitor_stream(&pair.server_connection, &preface_identity)
            .await
            .expect("open monitor stream");
        send.finish().expect("finish send stream");
        pair
    });

    let mut roster = MonitorStreamRoster::new(
        "session-fp",
        nz64(1),
        nz64(1),
        [ExpectedMonitorStream {
            session_monitor_id: nz16(1),
            media_plan_fingerprint: fingerprint(7),
        }],
    )
    .expect("bounded roster");

    let pair = server.await.expect("server task join");
    let (_recv, parsed_identity) = accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
        .await
        .expect("accept monitor stream");
    assert_eq!(
        roster.register(&parsed_identity),
        Err(MonitorRosterError::FingerprintMismatch)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_magic_preface_is_rejected() {
    let pair = connected_pair().await;
    let server = tokio::spawn(async move {
        let mut send = pair
            .server_connection
            .open_uni()
            .await
            .expect("open raw uni stream");
        // Eight bytes is enough for the fixed header; a bad magic is
        // rejected before the (irrelevant) trailing bytes are requested.
        send.write_all(b"XXXX\x00\x01\x00\x05")
            .await
            .expect("write malformed header");
        send.finish().expect("finish send stream");
        pair
    });

    let pair = server.await.expect("server task join");
    let result = accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT).await;
    assert!(matches!(
        result,
        Err(QuicTransportError::MonitorPreface(
            MonitorStreamPrefaceError::Malformed
        ))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn truncated_preface_is_rejected_without_a_full_timeout_wait() {
    let pair = connected_pair().await;
    let server = tokio::spawn(async move {
        let full_frame = identity("session-trunc", 1, 1, 1, 7)
            .encode()
            .expect("encode preface");
        let mut send = pair
            .server_connection
            .open_uni()
            .await
            .expect("open raw uni stream");
        // Writes fewer bytes than the encoded preface requires, then closes
        // the stream: the receiver must see a truncation failure, not hang
        // for the full accept timeout.
        send.write_all(&full_frame[..full_frame.len() - 4])
            .await
            .expect("write truncated preface");
        send.finish().expect("finish send stream");
        pair
    });

    let pair = server.await.expect("server task join");
    let started = tokio::time::Instant::now();
    let result = accept_monitor_stream(&pair.client_connection, Duration::from_secs(10)).await;
    assert!(matches!(result, Err(QuicTransportError::StreamRead(_))));
    // The stream FIN arrives promptly over loopback; truncation must be
    // observed well before the generous 10-second bound.
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_claimed_session_length_is_rejected_without_hanging() {
    let pair = connected_pair().await;
    let server = tokio::spawn(async move {
        let mut send = pair
            .server_connection
            .open_uni()
            .await
            .expect("open raw uni stream");
        let mut header = Vec::new();
        header.extend_from_slice(b"ARMP");
        header.extend_from_slice(&1_u16.to_be_bytes());
        // Claims a session id far larger than the bound; no further bytes
        // are ever written on this stream.
        header.extend_from_slice(&u16::MAX.to_be_bytes());
        send.write_all(&header)
            .await
            .expect("write oversized-claim header");
        // Deliberately left open (no finish()): a correct acceptor must
        // reject this from the header alone, never blocking on more reads.
        std::mem::forget(send);
        pair
    });

    let pair = server.await.expect("server task join");
    let started = tokio::time::Instant::now();
    let result = accept_monitor_stream(&pair.client_connection, Duration::from_secs(10)).await;
    assert!(matches!(
        result,
        Err(QuicTransportError::MonitorPreface(
            MonitorStreamPrefaceError::Malformed
        ))
    ));
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_times_out_when_no_stream_arrives() {
    let pair = connected_pair().await;
    // The server never opens a monitor stream.
    let result = accept_monitor_stream(&pair.client_connection, Duration::from_millis(100)).await;
    assert!(matches!(
        result,
        Err(QuicTransportError::MonitorStreamTimedOut)
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accept_fails_when_connection_closes_before_any_stream() {
    let pair = connected_pair().await;
    pair.server_connection
        .close(0_u32.into(), b"closing before any monitor stream");
    let result = accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT).await;
    assert!(matches!(result, Err(QuicTransportError::Connection(_))));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_bidi_stream_coexists_with_four_monitor_uni_streams() {
    let pair = connected_pair_for_monitor_carrier().await;
    let monitor_ids: Vec<u16> = vec![1, 2, 3, 4];
    assert_eq!(monitor_ids.len(), MAX_MONITOR_STREAMS_PER_CONNECTION);

    // `Connection` is a cheap, clonable handle to the same underlying QUIC
    // connection, so the server-side work below and the client-side work
    // further down can run concurrently against the one connection pair
    // (whose endpoints stay alive in `pair` for the whole test).
    let server_connection = pair.server_connection.clone();
    let server_monitor_ids = monitor_ids.clone();
    let server = tokio::spawn(async move {
        // Carrier A: the existing direct bidirectional stream, unaffected
        // by the additive Carrier B foundation below.
        let (mut bidi_send, mut bidi_recv) = server_connection
            .open_bi()
            .await
            .expect("open direct bidi stream");
        bidi_send
            .write_all(b"carrier-a-hello")
            .await
            .expect("write bidi hello");

        // Carrier B foundation: up to four server->client monitor streams,
        // concurrently with the still-open bidi stream above, proving the
        // test-only `monitor_carrier_transport_config` stream limits
        // support both shapes on one connection at once (bidi + up to
        // `MAX_MONITOR_STREAMS_PER_CONNECTION` uni streams).
        for monitor_id in &server_monitor_ids {
            let preface_identity = identity("session-coexist", 1, 1, *monitor_id, 5);
            let mut send = open_monitor_stream(&server_connection, &preface_identity)
                .await
                .unwrap_or_else(|error| panic!("open monitor {monitor_id}: {error}"));
            send.finish().expect("finish monitor send stream");
        }

        let mut echoed = [0_u8; 4];
        bidi_recv
            .read_exact(&mut echoed)
            .await
            .expect("read bidi echo");
        assert_eq!(&echoed, b"ack!");
    });

    // Client accepts Carrier A's bidi stream and the four Carrier B monitor
    // streams concurrently, then echoes back on the bidi stream.
    let (mut bidi_send, mut bidi_recv) = pair
        .client_connection
        .accept_bi()
        .await
        .expect("accept direct bidi stream");
    let mut hello = [0_u8; 15];
    bidi_recv
        .read_exact(&mut hello)
        .await
        .expect("read bidi hello");
    assert_eq!(&hello, b"carrier-a-hello");

    let mut accepted_monitor_ids = Vec::new();
    for _ in 0..monitor_ids.len() {
        let (_recv, parsed_identity) =
            accept_monitor_stream(&pair.client_connection, ACCEPT_TIMEOUT)
                .await
                .expect("accept monitor stream");
        accepted_monitor_ids.push(parsed_identity.session_monitor_id().get());
    }
    accepted_monitor_ids.sort_unstable();
    assert_eq!(accepted_monitor_ids, monitor_ids);

    bidi_send.write_all(b"ack!").await.expect("write bidi ack");
    bidi_send.finish().expect("finish bidi send stream");

    server.await.expect("server task join");
}
