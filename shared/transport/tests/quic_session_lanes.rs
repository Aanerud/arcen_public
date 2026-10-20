//! Loopback tests for the product-facing lane session adapter
//! (`arcen_transport::quic::session_lanes`): carrier setup on both sides,
//! the attachment grant, session records on their lanes, the clear refusal
//! of a pre-lane peer in both directions, and lane recovery steps.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines
)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use arcen_protocol::lanes::{
    LaneEndpoint, LaneRole, LaneRoleSet, PeerTransportMismatch, SessionClose, SessionRecord,
};
use arcen_transport::BoundedTransportPolicy;
use arcen_transport::lanes::{LaneFailure, LaneState};
use arcen_transport::quic::{
    DirectQuicDialParams, LaneConnection, LaneExit, LaneRecoveryStep, LaneRuntimeConfig,
    LaneSenderSlot, SessionLane, SessionLaneSetupError, accept_attachment, accept_direct,
    accept_session_lanes, connect_direct, connect_session_lanes, follow_lane_exit,
    grant_attachment, lane_transport_config_arc, recommended_transport_config_arc,
    session_record_bytes,
};
use bytes::Bytes;
use support::{
    build_client_rustls_config, build_server_rustls_config, client_identity, server_identity,
};

const STEP: Duration = Duration::from_secs(5);

async fn timed<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .expect("step timed out")
}

struct Endpoints {
    server: quinn::Endpoint,
    client: quinn::Endpoint,
    client_config: quinn::ClientConfig,
    address: SocketAddr,
}

fn endpoints(lane_server: bool, lane_client: bool) -> Endpoints {
    let policy = BoundedTransportPolicy::default();
    let server = server_identity();
    let client = client_identity();
    let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(
        build_server_rustls_config(&server, &client.cert_der),
    )
    .unwrap();
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    server_config.transport_config(if lane_server {
        lane_transport_config_arc(&policy, LaneEndpoint::Pier)
    } else {
        recommended_transport_config_arc(&policy)
    });
    let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(
        build_client_rustls_config(&client, &server.cert_der),
    )
    .unwrap();
    let mut client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    client_config.transport_config(if lane_client {
        lane_transport_config_arc(&policy, LaneEndpoint::Deck)
    } else {
        recommended_transport_config_arc(&policy)
    });
    let server =
        quinn::Endpoint::server(server_config, "127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .unwrap();
    let address = server.local_addr().unwrap();
    let client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    Endpoints {
        server,
        client,
        client_config,
        address,
    }
}

struct Session {
    _endpoints: Endpoints,
    pier: LaneConnection,
    deck: LaneConnection,
    pier_control: SessionLane,
    deck_control: SessionLane,
}

async fn session() -> Session {
    let endpoints = endpoints(true, true);
    let server = endpoints.server.clone();
    let accept = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        accept_session_lanes(connection, LaneRuntimeConfig::new(LaneEndpoint::Pier), STEP)
            .await
            .unwrap()
    });
    let (deck, deck_control) = timed(connect_session_lanes(
        DirectQuicDialParams {
            endpoint: endpoints.client.clone(),
            client_config: endpoints.client_config.clone(),
            remote_addr: endpoints.address,
            server_name: "localhost",
        },
        LaneRuntimeConfig::new(LaneEndpoint::Deck),
    ))
    .await
    .unwrap();
    let (pier, pier_control) = timed(accept).await.unwrap();
    Session {
        _endpoints: endpoints,
        pier,
        deck,
        pier_control,
        deck_control,
    }
}

/// Grants every role on the Pier and accepts the grant on the Deck through
/// a real `lane_attachment` record on the control lane.
async fn attach(session: &mut Session) {
    let grant = grant_attachment(
        &session.pier,
        NonZeroU64::new(1).unwrap(),
        LaneRoleSet::all(),
    )
    .unwrap();
    let text = serde_json::to_string(&grant).unwrap();
    session
        .pier_control
        .sender
        .send(session_record_bytes(&SessionRecord::Text(text)))
        .await
        .unwrap();
    let SessionRecord::Text(received) = timed(session.deck_control.reader.next_record())
        .await
        .unwrap()
    else {
        panic!("lane_attachment expected");
    };
    let message = serde_json::from_str(&received).unwrap();
    let roles = accept_attachment(&session.deck, &message).unwrap();
    for role in LaneRole::ALL {
        assert_eq!(roles.contains(role), role != LaneRole::Control, "{role}");
    }
}

#[tokio::test]
async fn session_records_cross_the_control_lane_in_both_directions() {
    let mut session = session().await;
    attach(&mut session).await;
    let records = [
        SessionRecord::Text(r#"{"type":"health_ping","sequence":1}"#.to_owned()),
        SessionRecord::Binary(vec![1, 2, 3]),
        SessionRecord::Ping(vec![7]),
        SessionRecord::Close(Some(SessionClose {
            code: 1000,
            reason: "bye".to_owned(),
        })),
    ];
    for record in &records {
        session
            .deck_control
            .sender
            .send(session_record_bytes(record))
            .await
            .unwrap();
        assert_eq!(
            &timed(session.pier_control.reader.next_record())
                .await
                .unwrap(),
            record
        );
        session
            .pier_control
            .sender
            .send(session_record_bytes(record))
            .await
            .unwrap();
        assert_eq!(
            &timed(session.deck_control.reader.next_record())
                .await
                .unwrap(),
            record
        );
    }
}

#[tokio::test]
async fn input_and_bulk_lanes_carry_their_session_records_after_the_grant() {
    let mut session = session().await;
    attach(&mut session).await;
    let deck_input = SessionLane::split(
        timed(session.deck.open_lane(LaneRole::InputCursor))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut pier_input = SessionLane::split(
        timed(session.pier.next_peer_lane(LaneRole::InputCursor))
            .await
            .unwrap(),
    )
    .unwrap();
    let key = SessionRecord::Text(r#"{"type":"key_event","scan_code":4,"pressed":true}"#.into());
    assert_eq!(key.lane(), LaneRole::InputCursor);
    deck_input
        .sender
        .try_send(session_record_bytes(&key))
        .unwrap();
    assert_eq!(timed(pier_input.reader.next_record()).await.unwrap(), key);

    let deck_bulk = SessionLane::split(
        timed(session.deck.open_lane(LaneRole::BulkAux))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut pier_bulk = SessionLane::split(
        timed(session.pier.next_peer_lane(LaneRole::BulkAux))
            .await
            .unwrap(),
    )
    .unwrap();
    let chunk = SessionRecord::Binary(vec![0x20, 0, 1]);
    assert_eq!(chunk.lane(), LaneRole::BulkAux);
    deck_bulk
        .sender
        .send(session_record_bytes(&chunk))
        .await
        .unwrap();
    assert_eq!(timed(pier_bulk.reader.next_record()).await.unwrap(), chunk);
}

#[tokio::test]
async fn a_malformed_session_record_fails_only_its_lane() {
    let mut session = session().await;
    attach(&mut session).await;
    let deck_input = SessionLane::split(
        timed(session.deck.open_lane(LaneRole::InputCursor))
            .await
            .unwrap(),
    )
    .unwrap();
    let mut pier_input = SessionLane::split(
        timed(session.pier.next_peer_lane(LaneRole::InputCursor))
            .await
            .unwrap(),
    )
    .unwrap();
    // Length 2, kind 9: not a session record.
    deck_input
        .sender
        .send(Bytes::from_static(&[0, 0, 0, 2, 9, 0]))
        .await
        .unwrap();
    let exit = timed(pier_input.reader.next_record()).await.unwrap_err();
    let LaneExit::Failed { failure, .. } = &exit else {
        panic!("failure expected, got {exit:?}");
    };
    assert_eq!(*failure, LaneFailure::MalformedFrame);
    // The Deck reopens input; the Pier waits for it and releases held input.
    let step = follow_lane_exit(&session.pier, &exit).await;
    assert!(
        matches!(
            step,
            LaneRecoveryStep::AwaitPeer {
                release_held_input: true,
                request_keyframe: false
            }
        ),
        "{step:?}"
    );
    // Control is untouched.
    let ping = SessionRecord::Text(r#"{"type":"health_ping"}"#.to_owned());
    session
        .deck_control
        .sender
        .send(session_record_bytes(&ping))
        .await
        .unwrap();
    assert_eq!(
        timed(session.pier_control.reader.next_record())
            .await
            .unwrap(),
        ping
    );
}

#[tokio::test]
async fn video_failure_is_reopened_by_the_pier_with_a_keyframe_and_audio_is_untouched() {
    let mut session = session().await;
    attach(&mut session).await;
    let video = timed(session.pier.open_lane(LaneRole::Video))
        .await
        .unwrap();
    let audio = timed(session.pier.open_lane(LaneRole::AudioOutput))
        .await
        .unwrap();
    let audio_generation = audio.lane_generation();
    let (_audio_sender, _) = audio.split_default();
    let generation = video.lane_generation();
    let (video_sender, _) = video.split_default();
    let action = session
        .pier
        .report_failure(LaneRole::Video, generation, LaneFailure::WriteFailed);
    let exit = LaneExit::Failed {
        failure: LaneFailure::WriteFailed,
        action,
    };
    drop(video_sender);
    let step = timed(follow_lane_exit(&session.pier, &exit)).await;
    let LaneRecoveryStep::Reopened {
        lane,
        request_keyframe,
        release_held_input,
    } = step
    else {
        panic!("reopen expected, got {step:?}");
    };
    assert!(request_keyframe);
    assert!(!release_held_input);
    assert_eq!(lane.role(), LaneRole::Video);
    assert!(lane.lane_generation() > generation);
    assert_eq!(
        session.pier.lane_state(LaneRole::AudioOutput),
        LaneState::Open(audio_generation)
    );
}

#[tokio::test]
async fn audio_is_disabled_visibly_after_its_recovery_budget() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut lane = timed(session.pier.open_lane(LaneRole::AudioOutput))
        .await
        .unwrap();
    for _ in 0..3 {
        let action = session.pier.report_failure(
            LaneRole::AudioOutput,
            lane.lane_generation(),
            LaneFailure::WriteFailed,
        );
        let step = timed(follow_lane_exit(
            &session.pier,
            &LaneExit::Failed {
                failure: LaneFailure::WriteFailed,
                action,
            },
        ))
        .await;
        let LaneRecoveryStep::Reopened { lane: next, .. } = step else {
            panic!("reopen expected, got {step:?}");
        };
        lane = next;
    }
    let action = session.pier.report_failure(
        LaneRole::AudioOutput,
        lane.lane_generation(),
        LaneFailure::WriteFailed,
    );
    let step = follow_lane_exit(
        &session.pier,
        &LaneExit::Failed {
            failure: LaneFailure::WriteFailed,
            action,
        },
    )
    .await;
    assert!(
        matches!(step, LaneRecoveryStep::Disabled(LaneRole::AudioOutput)),
        "{step:?}"
    );
    // Never routed elsewhere: video still opens as video only.
    assert!(session.pier.check_send(LaneRole::AudioOutput).is_err());
}

#[tokio::test]
async fn a_lane_deck_reaching_a_pre_lane_pier_reports_the_mismatch() {
    let endpoints = endpoints(false, true);
    let server = endpoints.server.clone();
    tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        let _ = accept_direct(connection).await;
    });
    let (_deck, mut control) = timed(connect_session_lanes(
        DirectQuicDialParams {
            endpoint: endpoints.client.clone(),
            client_config: endpoints.client_config.clone(),
            remote_addr: endpoints.address,
            server_name: "localhost",
        },
        LaneRuntimeConfig::new(LaneEndpoint::Deck),
    ))
    .await
    .unwrap();
    let exit = timed(control.reader.next_record()).await.unwrap_err();
    assert_eq!(
        exit,
        LaneExit::ConnectionLost {
            mismatch: Some(PeerTransportMismatch::PeerPredatesLanes)
        }
    );
    assert!(
        PeerTransportMismatch::PeerPredatesLanes
            .message()
            .contains("Update the Deck and the Pier to the same release")
    );
}

#[tokio::test]
async fn a_pre_lane_deck_reaching_a_lane_pier_is_closed_with_the_mismatch() {
    let endpoints = endpoints(true, false);
    let server = endpoints.server.clone();
    let pier = tokio::spawn(async move {
        let connection = server.accept().await.unwrap().await.unwrap();
        accept_session_lanes(
            connection,
            LaneRuntimeConfig::new(LaneEndpoint::Pier),
            Duration::from_secs(2),
        )
        .await
    });
    let mut legacy = timed(connect_direct(DirectQuicDialParams {
        endpoint: endpoints.client.clone(),
        client_config: endpoints.client_config.clone(),
        remote_addr: endpoints.address,
        server_name: "localhost",
    }))
    .await
    .unwrap();
    // The pre-lane Deck reads WebSocket frames on its stream: it gets one
    // close frame whose reason it prints, then the end of the stream.
    let mut received = Vec::new();
    let read = timed(tokio::io::AsyncReadExt::read_to_end(
        &mut legacy,
        &mut received,
    ))
    .await;
    assert!(read.is_ok(), "{read:?}");
    assert_eq!(
        received,
        arcen_protocol::lanes::legacy_direct_stream_refusal_frame()
    );
    let close = timed(legacy.connection().closed()).await;
    let quinn::ConnectionError::ApplicationClosed(close) = close else {
        panic!("application close expected, got {close:?}");
    };
    let reason = String::from_utf8_lossy(&close.reason).into_owned();
    assert!(reason.contains("update the Deck and the Pier"), "{reason}");
    // The Pier learns why, so it can log it.
    let setup = timed(pier).await.unwrap();
    assert!(
        matches!(
            setup,
            Err(SessionLaneSetupError::Mismatch(
                PeerTransportMismatch::PeerPredatesLanes
            ))
        ),
        "{setup:?}"
    );
}

#[tokio::test]
async fn a_failed_input_lane_owes_its_release_at_once_not_at_the_replacement() {
    use arcen_transport::quic::{PeerLaneEvent, PeerLaneReader};

    let mut session = session().await;
    attach(&mut session).await;
    let deck_input = SessionLane::split(
        timed(session.deck.open_lane(LaneRole::InputCursor))
            .await
            .unwrap(),
    )
    .unwrap();
    deck_input
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"key_event\"}".to_owned(),
        )))
        .await
        .unwrap();
    let mut pier_input = PeerLaneReader::new(&session.pier, LaneRole::InputCursor);
    assert!(matches!(
        timed(pier_input.next()).await.unwrap(),
        PeerLaneEvent::Opened { .. }
    ));
    assert!(matches!(
        timed(pier_input.next()).await.unwrap(),
        PeerLaneEvent::Frame(_)
    ));
    // The owner finds something illegal and fails the lane; the Deck has
    // not reopened, yet the release is owed now.
    pier_input.fail(LaneFailure::MalformedFrame);
    assert!(matches!(
        timed(pier_input.next()).await.unwrap(),
        PeerLaneEvent::Lost {
            release_held_input: true,
            ..
        }
    ));
}

#[tokio::test]
async fn sender_slots_hand_producers_the_newest_lane_only() {
    let mut session = session().await;
    attach(&mut session).await;
    let slot = LaneSenderSlot::new(LaneRole::BulkAux);
    assert!(slot.current().is_none());
    let waiter = {
        let slot = slot.clone();
        tokio::spawn(async move { slot.wait().await.lane_generation() })
    };
    let lane = SessionLane::split(
        timed(session.deck.open_lane(LaneRole::BulkAux))
            .await
            .unwrap(),
    )
    .unwrap();
    let generation = lane.lane_generation;
    slot.install(lane.sender.clone());
    assert_eq!(timed(waiter).await.unwrap(), generation);
    slot.clear();
    assert!(slot.current().is_none());
}

#[tokio::test]
async fn video_writer_reopens_with_a_keyframe_while_the_audio_reader_keeps_its_lane() {
    use arcen_transport::quic::{LaneWriteOutcome, LocalLaneWriter, PeerLaneEvent, PeerLaneReader};

    let mut session = session().await;
    attach(&mut session).await;
    let mut video = timed(LocalLaneWriter::open(&session.pier, LaneRole::Video))
        .await
        .unwrap();
    let mut audio = timed(LocalLaneWriter::open(&session.pier, LaneRole::AudioOutput))
        .await
        .unwrap();
    let mut deck_video = PeerLaneReader::new(&session.deck, LaneRole::Video);
    let mut deck_audio = PeerLaneReader::new(&session.deck, LaneRole::AudioOutput);

    assert_eq!(
        video.send(Bytes::from_static(b"idr")).await.unwrap(),
        LaneWriteOutcome::Queued
    );
    audio.try_send(Bytes::from_static(b"a0")).await.unwrap();
    assert!(matches!(
        timed(deck_video.next()).await.unwrap(),
        PeerLaneEvent::Opened {
            request_keyframe: false,
            ..
        }
    ));
    assert!(matches!(
        timed(deck_video.next()).await.unwrap(),
        PeerLaneEvent::Frame(frame) if frame.as_ref() == b"idr"
    ));
    assert!(matches!(
        timed(deck_audio.next()).await.unwrap(),
        PeerLaneEvent::Opened { .. }
    ));
    let audio_generation = deck_audio.lane_generation();
    assert!(matches!(
        timed(deck_audio.next()).await.unwrap(),
        PeerLaneEvent::Frame(frame) if frame.as_ref() == b"a0"
    ));

    // The owner detects a video failure outside the writer.
    let failed = video.sender().lane_generation();
    let _ = session
        .pier
        .report_failure(LaneRole::Video, failed, LaneFailure::WriteFailed);
    assert_eq!(
        timed(video.send(Bytes::from_static(b"idr2")))
            .await
            .unwrap(),
        LaneWriteOutcome::Reopened {
            request_keyframe: true
        }
    );
    assert!(video.sender().lane_generation() > failed);
    // The keyframe step arrives first, with the loss (the Deck saw its
    // lane fail) or with the replacement (it saw the supersession).
    let mut keyframe_requested = false;
    loop {
        match timed(deck_video.next()).await.unwrap() {
            PeerLaneEvent::Lost {
                request_keyframe, ..
            }
            | PeerLaneEvent::Opened {
                request_keyframe, ..
            } => keyframe_requested |= request_keyframe,
            PeerLaneEvent::Frame(frame) => {
                assert_eq!(frame.as_ref(), b"idr2", "stale frame");
                break;
            }
        }
    }
    assert!(keyframe_requested);

    // Audio never noticed.
    audio.try_send(Bytes::from_static(b"a1")).await.unwrap();
    assert!(matches!(
        timed(deck_audio.next()).await.unwrap(),
        PeerLaneEvent::Frame(frame) if frame.as_ref() == b"a1"
    ));
    assert_eq!(deck_audio.lane_generation(), audio_generation);
}

#[tokio::test]
async fn the_session_socket_carries_control_records_as_a_sink_and_stream() {
    use std::future::poll_fn;
    use std::pin::Pin;

    use arcen_transport::quic::SessionLaneSocket;
    use futures_core::Stream;
    use futures_sink::Sink;

    let session = session().await;
    let Session {
        _endpoints,
        pier,
        deck,
        pier_control,
        deck_control,
    } = session;
    let mut pier_socket = SessionLaneSocket::new(pier, pier_control);
    let mut deck_socket = SessionLaneSocket::new(deck.clone(), deck_control);
    let hello = SessionRecord::Text(r#"{"type":"server_hello"}"#.to_owned());
    poll_fn(|context| Pin::new(&mut pier_socket).poll_ready(context))
        .await
        .unwrap();
    Pin::new(&mut pier_socket)
        .start_send(hello.clone())
        .unwrap();
    poll_fn(|context| Pin::new(&mut pier_socket).poll_flush(context))
        .await
        .unwrap();
    let received = timed(poll_fn(|context| {
        Pin::new(&mut deck_socket).poll_next(context)
    }))
    .await;
    assert_eq!(received, Some(Ok(hello)));

    // Closing the Pier's side sends a close record and finishes its control
    // lane: the Deck's stream ends cleanly rather than failing.
    poll_fn(|context| Pin::new(&mut pier_socket).poll_close(context))
        .await
        .unwrap();
    let close = timed(poll_fn(|context| {
        Pin::new(&mut deck_socket).poll_next(context)
    }))
    .await;
    assert_eq!(close, Some(Ok(SessionRecord::Close(None))));
    let end = timed(poll_fn(|context| {
        Pin::new(&mut deck_socket).poll_next(context)
    }))
    .await;
    assert!(end.is_none(), "{end:?}");
    drop(pier_socket);
    assert!(
        timed(deck.wait_ended()).await == arcen_transport::lanes::LaneEndReason::Closed
            || deck.ended().is_some()
    );
}

mod graceful_close {
    //! A session's last words survive its close (0.17.0 qualification F1):
    //! closing a QUIC connection abandons unacknowledged stream data, so a
    //! refusal written just before the socket is dropped used to reach the
    //! Deck as `ConnectionLost`, and a Deck's clean close reached the Pier
    //! as a transport loss.

    use std::future::poll_fn;
    use std::pin::Pin;

    use arcen_protocol::lanes::{SESSION_CLOSED_CLOSE_CODE, SESSION_CLOSED_CLOSE_REASON};
    use arcen_transport::quic::{SESSION_CONTROL_LINGER, SessionLaneSocket, closed_in_order};
    use futures_core::Stream;
    use futures_sink::Sink;

    use super::*;

    async fn send(socket: &mut SessionLaneSocket, record: SessionRecord) {
        poll_fn(|context| Pin::new(&mut *socket).poll_ready(context))
            .await
            .unwrap();
        Pin::new(&mut *socket).start_send(record).unwrap();
        poll_fn(|context| Pin::new(&mut *socket).poll_flush(context))
            .await
            .unwrap();
    }

    async fn next(socket: &mut SessionLaneSocket) -> Option<Result<SessionRecord, LaneExit>> {
        timed(poll_fn(|context| Pin::new(&mut *socket).poll_next(context))).await
    }

    async fn close(socket: &mut SessionLaneSocket) {
        timed(poll_fn(|context| {
            Pin::new(&mut *socket).poll_close(context)
        }))
        .await
        .unwrap();
    }

    fn auth_refusal() -> SessionRecord {
        SessionRecord::Text(
            r#"{"type":"auth_result","success":false,"message":"Authentication failed"}"#
                .to_owned(),
        )
    }

    fn busy_close() -> SessionRecord {
        SessionRecord::Close(Some(SessionClose {
            code: 1011,
            reason: "another session owns this desktop; retry after it disconnects".to_owned(),
        }))
    }

    async fn sockets() -> (SessionLaneSocket, SessionLaneSocket, Endpoints) {
        let Session {
            _endpoints: endpoints,
            pier,
            deck,
            pier_control,
            deck_control,
        } = session().await;
        (
            SessionLaneSocket::new(pier, pier_control),
            SessionLaneSocket::new(deck, deck_control),
            endpoints,
        )
    }

    fn assert_closed_in_order(close: &quinn::ConnectionError) {
        let quinn::ConnectionError::ApplicationClosed(close) = close else {
            panic!("an orderly application close expected, got {close:?}");
        };
        assert_eq!(
            close.error_code,
            quinn::VarInt::from_u32(SESSION_CLOSED_CLOSE_CODE)
        );
        assert_eq!(close.reason.as_ref(), SESSION_CLOSED_CLOSE_REASON);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refusal_written_just_before_the_pier_drops_its_socket_reaches_the_deck() {
        for _ in 0..20 {
            let (mut pier, mut deck, _endpoints) = sockets().await;
            let deck_connection = deck.lanes().connection().clone();
            send(&mut pier, auth_refusal()).await;
            send(&mut pier, busy_close()).await;
            drop(pier);

            assert_eq!(next(&mut deck).await, Some(Ok(auth_refusal())));
            assert_eq!(next(&mut deck).await, Some(Ok(busy_close())));
            assert_eq!(next(&mut deck).await, None, "then a clean end");
            // The Deck answers as a WebSocket client would and goes away;
            // whichever side closes first, it is an orderly close, never an
            // idle timeout.
            close(&mut deck).await;
            drop(deck);
            let started = std::time::Instant::now();
            let close = timed(deck_connection.closed()).await;
            if !matches!(close, quinn::ConnectionError::LocallyClosed) {
                assert_closed_in_order(&close);
            }
            assert!(started.elapsed() < SESSION_CONTROL_LINGER);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_deck_that_reads_after_the_close_still_gets_every_record() {
        let (mut pier, mut deck, _endpoints) = sockets().await;
        let deck_connection = deck.lanes().connection().clone();
        send(&mut pier, auth_refusal()).await;
        drop(pier);
        // The Deck neither reads nor answers: the Pier stops waiting after
        // its bounded linger and closes anyway.
        let started = std::time::Instant::now();
        assert_closed_in_order(&timed(deck_connection.closed()).await);
        assert!(started.elapsed() <= SESSION_CONTROL_LINGER + Duration::from_millis(500));
        // What the Deck's QUIC stack acknowledged is still read, in order,
        // and the orderly close ends the stream cleanly.
        assert_eq!(next(&mut deck).await, Some(Ok(auth_refusal())));
        assert_eq!(next(&mut deck).await, None);
        assert!(closed_in_order(&deck_connection));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_deck_that_closes_cleanly_is_seen_by_the_pier_as_closing() {
        for _ in 0..20 {
            let (mut pier, mut deck, _endpoints) = sockets().await;
            let pier_connection = pier.lanes().connection().clone();
            send(
                &mut deck,
                SessionRecord::Text(r#"{"type":"health_ping"}"#.to_owned()),
            )
            .await;
            close(&mut deck).await;
            drop(deck);

            assert!(matches!(
                next(&mut pier).await,
                Some(Ok(SessionRecord::Text(_)))
            ));
            // The close record is what a Pier maps to `client_closed`.
            assert_eq!(next(&mut pier).await, Some(Ok(SessionRecord::Close(None))));
            assert_eq!(next(&mut pier).await, None);
            drop(pier);
            let close = timed(pier_connection.closed()).await;
            assert!(
                matches!(close, quinn::ConnectionError::LocallyClosed)
                    || matches!(close, quinn::ConnectionError::ApplicationClosed(_)),
                "{close:?}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_deck_dropped_mid_session_without_a_close_is_not_a_clean_close() {
        let (mut pier, deck, _endpoints) = sockets().await;
        drop(deck);
        // No close record: the control lane just ends (the Pier treats that
        // as a transport loss and may hold the desktop for a resume).
        assert_eq!(next(&mut pier).await, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ending_the_attachment_keeps_a_close_reason_already_on_the_control_lane() {
        let Session {
            _endpoints,
            pier,
            deck,
            pier_control,
            deck_control,
        } = session().await;
        let pier_lanes = pier.clone();
        let mut pier = SessionLaneSocket::new(pier, pier_control);
        let mut deck = SessionLaneSocket::new(deck, deck_control);
        send(&mut pier, busy_close()).await;
        // A Pier ends the attachment (resetting media and input lanes)
        // before it drops the control socket.
        pier_lanes.end(arcen_transport::lanes::LaneEndReason::Closed);
        drop(pier);
        assert_eq!(next(&mut deck).await, Some(Ok(busy_close())));
        assert_eq!(next(&mut deck).await, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn both_sides_dropping_at_once_close_promptly() {
        let (pier, deck, _endpoints) = sockets().await;
        let connection = pier.lanes().connection().clone();
        let started = std::time::Instant::now();
        drop(pier);
        drop(deck);
        let _ = timed(connection.closed()).await;
        assert!(
            started.elapsed() < SESSION_CONTROL_LINGER,
            "no side waits out the linger when both finish: {:?}",
            started.elapsed()
        );
    }
}
