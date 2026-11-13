// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Loopback tests for the relay a Pier uses when its session runs behind a
//! local socket (`arcen_transport::quic::PierLaneRelay`, ADR 0014): every
//! message lands on its own lane, a stalled video lane holds neither audio
//! nor input, video drops resynchronise on a keyframe, and a replaced input
//! lane releases held input.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines
)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{LaneEndpoint, LaneRole, LaneRoleSet, SessionRecord};
use arcen_protocol::wire::{
    AudioCodec, AudioHeader, ChromaSubsampling, FrameType, MicrophoneHeader, VIDEO_KEYFRAME_FLAG,
    VideoCodec, VideoHeader, encode_audio_header, encode_microphone_header, encode_video_header,
};
use arcen_transport::BoundedTransportPolicy;
use arcen_transport::quic::{
    DirectQuicDialParams, LaneConnection, LaneRecoveryStep, LaneRelayEvent, LaneRuntimeConfig,
    LocalLaneWriter, PeerLaneEvent, PeerLaneReader, PierLaneRelay, RELAY_INPUT_RELEASE_REASON,
    RelayRoute, SessionLane, accept_attachment, accept_session_lanes, connect_session_lanes,
    follow_lane_exit, grant_attachment, lane_transport_config_arc, next_attachment_generation,
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

struct Session {
    _server: quinn::Endpoint,
    _client: quinn::Endpoint,
    pier: LaneConnection,
    deck: LaneConnection,
    pier_control: SessionLane,
    deck_control: SessionLane,
}

async fn session() -> Session {
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
    let address = server.local_addr().unwrap();
    let client = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let accepting = server.clone();
    let accept = tokio::spawn(async move {
        let connection = accepting.accept().await.unwrap().await.unwrap();
        accept_session_lanes(connection, LaneRuntimeConfig::new(LaneEndpoint::Pier), STEP)
            .await
            .unwrap()
    });
    let (deck, deck_control) = timed(connect_session_lanes(
        DirectQuicDialParams {
            endpoint: client.clone(),
            client_config,
            remote_addr: address,
            server_name: "localhost",
        },
        LaneRuntimeConfig::new(LaneEndpoint::Deck),
    ))
    .await
    .unwrap();
    let (pier, pier_control) = timed(accept).await.unwrap();
    Session {
        _server: server,
        _client: client,
        pier,
        deck,
        pier_control,
        deck_control,
    }
}

/// Grants every role and accepts the grant on the Deck through a real
/// `lane_attachment` record on the control lane.
async fn attach(session: &mut Session) {
    let grant = grant_attachment(
        &session.pier,
        next_attachment_generation(),
        LaneRoleSet::all(),
    )
    .unwrap();
    session
        .pier_control
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            serde_json::to_string(&grant).unwrap(),
        )))
        .await
        .unwrap();
    let record = timed(session.deck_control.reader.next_record())
        .await
        .unwrap();
    let SessionRecord::Text(text) = record else {
        panic!("expected lane_attachment text");
    };
    accept_attachment(&session.deck, &serde_json::from_str(&text).unwrap()).unwrap();
}

fn video_frame(monitor: u16, keyframe: bool, payload: usize) -> Vec<u8> {
    let mut frame = encode_video_header(VideoHeader {
        frame_type: FrameType::VideoH264,
        codec: VideoCodec::H264,
        chroma: ChromaSubsampling::Yuv420,
        flags: if keyframe { VIDEO_KEYFRAME_FLAG } else { 0 },
        timestamp_ms: 0,
        monitor_id: monitor,
        topology_generation: 0,
        stream_epoch: 0,
    });
    frame.resize(frame.len() + payload, 0xAB);
    frame
}

fn audio_frame(timestamp_ms: u32) -> Vec<u8> {
    let mut frame = encode_audio_header(AudioHeader {
        codec: AudioCodec::Opus,
        timestamp_ms,
    })
    .to_vec();
    frame.extend_from_slice(&[0x55; 80]);
    frame
}

/// The Deck's half of a session-message lane it opens.
async fn deck_lane(deck: &LaneConnection, role: LaneRole) -> SessionLane {
    SessionLane::split(timed(deck.open_lane(role)).await.unwrap()).unwrap()
}

async fn next_frame(reader: &mut PeerLaneReader) -> Bytes {
    loop {
        match timed(reader.next()).await.unwrap() {
            PeerLaneEvent::Frame(frame) => return frame,
            PeerLaneEvent::Opened { .. } | PeerLaneEvent::Lost { .. } => {}
        }
    }
}

async fn next_to_session(relay: &mut PierLaneRelay) -> SessionRecord {
    match timed(relay.next_event()).await {
        LaneRelayEvent::ToSession(record) => record,
        other => panic!("expected a record for the session, got {other:?}"),
    }
}

fn is_type(record: &SessionRecord, msg_type: &str) -> bool {
    matches!(record, SessionRecord::Text(text)
        if serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|value| value.get("type")?.as_str().map(str::to_owned))
            .as_deref() == Some(msg_type))
}

#[tokio::test]
async fn every_session_message_lands_on_its_own_lane() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let mut deck_input = deck_lane(&session.deck, LaneRole::InputCursor).await;
    let mut deck_bulk = deck_lane(&session.deck, LaneRole::BulkAux).await;
    // Let the relay's readers install the lanes' write halves.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let hello = SessionRecord::Text("{\"type\":\"server_hello\"}".to_owned());
    assert_eq!(
        relay.route_to_deck(hello.clone()),
        RelayRoute::Control(hello)
    );
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(video_frame(0, true, 1024))),
        RelayRoute::Queued(LaneRole::Video)
    );
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(audio_frame(20))),
        RelayRoute::Queued(LaneRole::AudioOutput)
    );
    assert_eq!(
        relay.route_to_deck(SessionRecord::Text(
            "{\"type\":\"cursor_shape\",\"visible\":true}".to_owned()
        )),
        RelayRoute::Queued(LaneRole::InputCursor)
    );
    assert_eq!(
        relay.route_to_deck(SessionRecord::Text(
            "{\"type\":\"clipboard_data\",\"text\":\"x\"}".to_owned()
        )),
        RelayRoute::Queued(LaneRole::BulkAux)
    );

    let mut video = PeerLaneReader::new(&session.deck, LaneRole::Video);
    let mut audio = PeerLaneReader::new(&session.deck, LaneRole::AudioOutput);
    assert_eq!(next_frame(&mut video).await[0], FrameType::VideoH264 as u8);
    assert_eq!(next_frame(&mut audio).await[0], FrameType::Audio as u8);
    let cursor = timed(deck_input.reader.next_record()).await.unwrap();
    assert!(is_type(&cursor, "cursor_shape"));
    let clipboard = timed(deck_bulk.reader.next_record()).await.unwrap();
    assert!(is_type(&clipboard, "clipboard_data"));

    // And back: input, bulk and microphone from the Deck reach the session.
    deck_input
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"key_event\",\"key\":4,\"down\":true}".to_owned(),
        )))
        .await
        .unwrap();
    assert!(is_type(&next_to_session(&mut relay).await, "key_event"));
    deck_bulk
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"clipboard_data\",\"text\":\"y\"}".to_owned(),
        )))
        .await
        .unwrap();
    assert!(is_type(
        &next_to_session(&mut relay).await,
        "clipboard_data"
    ));
    let mut microphone = LocalLaneWriter::open(&session.deck, LaneRole::Microphone)
        .await
        .unwrap();
    let mut mic_frame = encode_microphone_header(MicrophoneHeader {
        codec: AudioCodec::Opus,
        sequence: 1,
        timestamp_ms: 0,
        generation: 3,
    })
    .unwrap()
    .to_vec();
    mic_frame.extend_from_slice(&[0x11; 40]);
    microphone
        .send(Bytes::from(mic_frame.clone()))
        .await
        .unwrap();
    assert_eq!(
        next_to_session(&mut relay).await,
        SessionRecord::Binary(mic_frame)
    );
    let stats = relay.stats();
    assert_eq!(stats.video_frames, 1);
    assert_eq!(stats.input_records, 1);
    assert_eq!(stats.microphone_frames, 1);
}

#[tokio::test]
async fn a_stalled_video_lane_holds_neither_audio_nor_input() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let deck_input = deck_lane(&session.deck, LaneRole::InputCursor).await;
    // The Deck reads audio but never reads video.
    let mut audio = PeerLaneReader::new(&session.deck, LaneRole::AudioOutput);

    let started = Instant::now();
    let mut audio_routed = 0_u32;
    for index in 0..240_u32 {
        let route = relay.route_to_deck(SessionRecord::Binary(video_frame(
            0,
            index == 0,
            256 * 1024,
        )));
        assert!(matches!(
            route,
            RelayRoute::Queued(LaneRole::Video) | RelayRoute::Dropped(LaneRole::Video)
        ));
        if index % 4 == 0 {
            assert_eq!(
                relay.route_to_deck(SessionRecord::Binary(audio_frame(index))),
                RelayRoute::Queued(LaneRole::AudioOutput)
            );
            audio_routed += 1;
            // Audio keeps arriving while the video lane is stuck.
            assert_eq!(next_frame(&mut audio).await[0], FrameType::Audio as u8);
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(audio_routed, 60);
    // Routing never waited for video.
    assert!(started.elapsed() < STEP);

    let stats = relay.stats();
    assert!(
        stats.video_dropped > 0,
        "the backlog must overflow: {stats:?}"
    );
    assert!(stats.keyframe_requests >= 1);
    // The relay asked the session for a keyframe.
    assert!(is_type(
        &next_to_session(&mut relay).await,
        "request_full_frame"
    ));

    // Input from the Deck still reaches the session at once.
    let sent = Instant::now();
    deck_input
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"mouse_button\",\"button\":1,\"down\":false}".to_owned(),
        )))
        .await
        .unwrap();
    loop {
        let record = next_to_session(&mut relay).await;
        if is_type(&record, "mouse_button") {
            break;
        }
        assert!(is_type(&record, "request_full_frame"));
    }
    assert!(sent.elapsed() < Duration::from_secs(1));

    // A delta frame of the broken chain is held; its keyframe passes once
    // the backlog has room again.
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(video_frame(0, false, 16))),
        RelayRoute::Dropped(LaneRole::Video)
    );
    let mut video = PeerLaneReader::new(&session.deck, LaneRole::Video);
    // Drain the stalled lane so the writer catches up.
    let drain = tokio::spawn(async move {
        loop {
            if video.next().await.is_err() {
                break;
            }
        }
    });
    let deadline = Instant::now() + STEP;
    loop {
        if relay.route_to_deck(SessionRecord::Binary(video_frame(0, true, 16)))
            == RelayRoute::Queued(LaneRole::Video)
        {
            break;
        }
        assert!(Instant::now() < deadline, "keyframe never admitted");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Let the drained backlog empty before the next delta.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(video_frame(0, false, 16))),
        RelayRoute::Queued(LaneRole::Video)
    );
    drain.abort();
}

#[tokio::test]
async fn the_relay_reports_every_video_frame_it_is_done_with() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let mut released = relay.video_released();
    assert_eq!(*released.borrow_and_update(), 0);
    let mut video = PeerLaneReader::new(&session.deck, LaneRole::Video);

    // A frame the lane takes is released once it is in the lane's queue.
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(video_frame(0, true, 1024))),
        RelayRoute::Queued(LaneRole::Video)
    );
    timed(released.changed()).await.unwrap();
    assert_eq!(*released.borrow_and_update(), 1);
    assert_eq!(next_frame(&mut video).await[0], FrameType::VideoH264 as u8);

    // A frame the relay drops is released at once.
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(vec![FrameType::VideoH264 as u8])),
        RelayRoute::Dropped(LaneRole::Video)
    );
    assert_eq!(*released.borrow_and_update(), 2);

    // Audio, cursor and control never count.
    let _ = relay.route_to_deck(SessionRecord::Binary(audio_frame(20)));
    let _ = relay.route_to_deck(SessionRecord::Text(
        "{\"type\":\"server_hello\"}".to_owned(),
    ));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(*released.borrow(), 2);
}

#[tokio::test]
async fn a_replaced_input_lane_releases_held_input_and_keeps_forwarding() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let mut deck_input = deck_lane(&session.deck, LaneRole::InputCursor).await;
    deck_input
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"key_event\",\"key\":4,\"down\":true}".to_owned(),
        )))
        .await
        .unwrap();
    assert!(is_type(&next_to_session(&mut relay).await, "key_event"));

    // A control message on the input lane fails it.
    deck_input
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"quality_settings\"}".to_owned(),
        )))
        .await
        .unwrap();
    // Held input is released at once, before the Deck has reopened.
    let release = next_to_session(&mut relay).await;
    assert!(is_type(&release, "key_reset_modifiers"), "{release:?}");
    assert!(
        matches!(&release, SessionRecord::Text(text) if text.contains(RELAY_INPUT_RELEASE_REASON))
    );

    // The Pier reset the lane; the Deck sees it on its read half and reopens.
    let exit = timed(deck_input.reader.next_record()).await.unwrap_err();
    let LaneRecoveryStep::Reopened {
        lane,
        release_held_input,
        ..
    } = timed(follow_lane_exit(&session.deck, &exit)).await
    else {
        panic!("the Deck reopens its input lane");
    };
    assert!(release_held_input);
    let replacement = SessionLane::split(lane).unwrap();

    // A key pressed on the replacement lane at once is never cancelled by a
    // release that arrives after it.
    replacement
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"key_event\",\"key\":5,\"down\":true}".to_owned(),
        )))
        .await
        .unwrap();
    loop {
        let record = next_to_session(&mut relay).await;
        if is_type(&record, "key_event") {
            assert!(matches!(&record, SessionRecord::Text(text) if text.contains("\"key\":5")));
            break;
        }
        assert!(is_type(&record, "key_reset_modifiers"), "{record:?}");
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(200), relay.next_event())
            .await
            .is_err(),
        "nothing follows the new key press"
    );
    // The quality message never reached the session.
    assert_eq!(relay.stats().input_records, 2);
}

#[tokio::test]
async fn an_overflowing_clipboard_transfer_is_abandoned_visibly() {
    use arcen_protocol::messages::{ClipboardContentKind, ClipboardDataMsg};
    use arcen_protocol::wire::{ClipboardChunkHeader, encode_clipboard_chunk};

    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    // The Deck has not opened its bulk lane, so nothing drains the backlog.
    let offer = ClipboardDataMsg::new(5, ClipboardContentKind::TextUtf8, 400_000, false);
    assert_eq!(
        relay.route_to_deck(SessionRecord::Text(serde_json::to_string(&offer).unwrap())),
        RelayRoute::Queued(LaneRole::BulkAux)
    );
    let mut dropped = 0;
    for chunk in 0..100_u32 {
        let frame = encode_clipboard_chunk(
            ClipboardChunkHeader {
                kind: ClipboardContentKind::TextUtf8,
                sequence: 5,
                total_size: 400_000,
                offset: chunk * 4_000,
            },
            &[b'a'; 4_000],
        )
        .unwrap();
        if relay.route_to_deck(SessionRecord::Binary(frame))
            == RelayRoute::Dropped(LaneRole::BulkAux)
        {
            dropped += 1;
        }
    }
    assert!(dropped > 0);
    // The Deck is told on the control lane, once.
    let LaneRelayEvent::ToDeckControl(SessionRecord::Text(notice)) =
        timed(relay.next_event()).await
    else {
        panic!("expected a control notice");
    };
    let notice: serde_json::Value = serde_json::from_str(&notice).unwrap();
    assert_eq!(notice["type"], "clipboard_transfer_failed");
    assert_eq!(notice["sequence"], 5);
    assert_eq!(notice["reason"], "bulk_lane_full");
    assert_eq!(relay.stats().clipboard_failed, 1);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), relay.next_event())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn usb_bridge_records_are_never_crowded_out_by_a_clipboard_transfer() {
    use arcen_protocol::messages::{ClipboardContentKind, ClipboardDataMsg};
    use arcen_protocol::wire::{ClipboardChunkHeader, encode_clipboard_chunk};

    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    // No Deck bulk lane yet: nothing drains, so the clipboard fills its
    // backlog and starts dropping.
    let offer = ClipboardDataMsg::new(9, ClipboardContentKind::TextUtf8, 400_000, false);
    relay.route_to_deck(SessionRecord::Text(serde_json::to_string(&offer).unwrap()));
    for chunk in 0..100_u32 {
        let frame = encode_clipboard_chunk(
            ClipboardChunkHeader {
                kind: ClipboardContentKind::TextUtf8,
                sequence: 9,
                total_size: 400_000,
                offset: chunk * 4_000,
            },
            &[b'a'; 4_000],
        )
        .unwrap();
        relay.route_to_deck(SessionRecord::Binary(frame));
    }
    assert!(relay.stats().bulk_dropped > 0, "the clipboard overflowed");
    // A USB host's submits and cancels still get through, all of them. The
    // relay routes by the leading frame type alone, so a frame type and a
    // sequence number stand in for the URB headers.
    let mut sent = Vec::new();
    for id in 1..=40_u32 {
        let kind = if id % 2 == 0 {
            arcen_protocol::wire::FrameType::UsbBridgeUrbCancel
        } else {
            arcen_protocol::wire::FrameType::UsbBridgeUrbSubmit
        };
        let mut frame = vec![kind as u8];
        frame.extend_from_slice(&id.to_le_bytes());
        assert_eq!(
            relay.route_to_deck(SessionRecord::Binary(frame.clone())),
            RelayRoute::Queued(LaneRole::BulkAux),
            "USB record {id}"
        );
        sent.push(frame);
    }
    assert_eq!(relay.stats().usb_records, 40);
    // Once the Deck's bulk lane opens, every USB record arrives, in order.
    let mut deck_bulk = deck_lane(&session.deck, LaneRole::BulkAux).await;
    let mut received = Vec::new();
    while received.len() < sent.len() {
        if let SessionRecord::Binary(bytes) = timed(deck_bulk.reader.next_record()).await.unwrap() {
            if bytes
                .first()
                .is_some_and(|kind| (0x40..=0x42).contains(kind))
            {
                received.push(bytes);
            }
        }
    }
    assert_eq!(received, sent);
    assert_eq!(relay.stats().usb_dropped, 0);
}

/// Every wire message of one clipboard transfer as the shared sender frames
/// it (every Pier and the Deck), offer first.
fn clipboard_records(sequence: u64, bytes: &[u8]) -> Vec<SessionRecord> {
    use arcen_protocol::clipboard::{ClipboardTransfer, ClipboardWireMessage};
    use arcen_protocol::messages::ClipboardContentKind;

    let mut cursor =
        ClipboardTransfer::new(sequence, ClipboardContentKind::TextUtf8, bytes.to_vec())
            .unwrap()
            .into_cursor();
    let mut records = Vec::new();
    while let Some(message) = cursor.next_message() {
        records.push(match message.unwrap() {
            ClipboardWireMessage::Offer(offer) => {
                SessionRecord::Text(serde_json::to_string(&offer).unwrap())
            }
            ClipboardWireMessage::Chunk(frame) => SessionRecord::Binary(frame),
        });
    }
    records
}

/// Feeds one received record to `reassembler`; returns the completed bytes.
fn reassemble(
    reassembler: &mut arcen_protocol::clipboard::ClipboardReassembler,
    record: SessionRecord,
) -> Option<Vec<u8>> {
    match record {
        SessionRecord::Text(text) => {
            reassembler
                .begin(serde_json::from_str(&text).unwrap())
                .unwrap();
            None
        }
        SessionRecord::Binary(frame) => {
            let (header, payload) = arcen_protocol::wire::decode_clipboard_chunk(&frame).unwrap();
            reassembler
                .push(header, payload)
                .unwrap()
                .map(|mut completed| completed.take_bytes())
        }
        other => panic!("unexpected bulk record {other:?}"),
    }
}

/// The default clipboard policy limit, so several chunks of the maximal size.
const LARGE_CLIPBOARD_BYTES: usize = 8 * 1024 * 1024;

fn large_clipboard() -> Vec<u8> {
    (0..LARGE_CLIPBOARD_BYTES)
        .map(|index| u8::try_from(index % 251).unwrap())
        .collect()
}

#[tokio::test]
async fn a_multi_megabyte_clipboard_transfer_reaches_the_deck() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let mut deck_bulk = deck_lane(&session.deck, LaneRole::BulkAux).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let bytes = large_clipboard();
    let records = clipboard_records(3, &bytes);
    assert!(records.len() > 8, "several maximal chunks");
    for record in records {
        assert_eq!(
            relay.route_to_deck(record),
            RelayRoute::Queued(LaneRole::BulkAux)
        );
    }
    let mut reassembler =
        arcen_protocol::clipboard::ClipboardReassembler::new(LARGE_CLIPBOARD_BYTES).unwrap();
    let received = loop {
        let record = timed(deck_bulk.reader.next_record()).await.unwrap();
        if let Some(completed) = reassemble(&mut reassembler, record) {
            break completed;
        }
    };
    assert!(received == bytes, "the Deck received the copied bytes");
    assert_eq!(relay.stats().bulk_dropped, 0);
    assert_eq!(relay.stats().clipboard_failed, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(200), relay.next_event())
            .await
            .is_err(),
        "no clipboard_transfer_failed notice"
    );
}

#[tokio::test]
async fn a_multi_megabyte_clipboard_transfer_reaches_the_session_while_input_flows() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let deck_input = deck_lane(&session.deck, LaneRole::InputCursor).await;
    let deck_bulk = deck_lane(&session.deck, LaneRole::BulkAux).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    let bytes = large_clipboard();
    let records = clipboard_records(4, &bytes);
    let sender = deck_bulk.sender.clone();
    let upload = tokio::spawn(async move {
        for record in records {
            sender.send(session_record_bytes(&record)).await.unwrap();
        }
    });
    // Input is not held behind the bulk transfer.
    deck_input
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"key_event\",\"key\":4,\"down\":true}".to_owned(),
        )))
        .await
        .unwrap();
    let mut reassembler =
        arcen_protocol::clipboard::ClipboardReassembler::new(LARGE_CLIPBOARD_BYTES).unwrap();
    let mut key_seen = false;
    let received = loop {
        let record = next_to_session(&mut relay).await;
        if is_type(&record, "key_event") {
            key_seen = true;
            continue;
        }
        if let Some(completed) = reassemble(&mut reassembler, record) {
            break completed;
        }
    };
    timed(upload).await.unwrap();
    assert!(key_seen);
    assert!(received == bytes, "the session received the copied bytes");
}

#[tokio::test]
async fn ending_the_attachment_ends_the_relay_with_input_released() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let _deck_input = deck_lane(&session.deck, LaneRole::InputCursor).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    session
        .deck
        .connection()
        .close(0_u32.into(), b"deck went away");
    let mut released = false;
    loop {
        match timed(relay.next_event()).await {
            LaneRelayEvent::ToSession(record) if is_type(&record, "key_reset_modifiers") => {
                released = true;
            }
            LaneRelayEvent::Ended(_) => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert!(released);
}

fn agent_cursor_record(sequence: u64, id: u32) -> Vec<u8> {
    use arcen_protocol::cursor::{
        CursorAppearance, CursorImageId, CursorImageRef, CursorUpdate, encode_cursor_update,
    };
    let mut png = arcen_protocol::wire::PNG_MAGIC.to_vec();
    png.resize(200, 3);
    encode_cursor_update(&CursorUpdate {
        sequence,
        output: 0,
        generation: 0,
        fallback: arcen_protocol::messages::CursorShapeKind::Crosshair,
        appearance: CursorAppearance::Image(CursorImageRef {
            id: CursorImageId::new(id).unwrap(),
            width: 32,
            height: 32,
            hotspot_x: 15,
            hotspot_y: 15,
            inline_png: Some(png),
        }),
    })
    .unwrap()
}

async fn next_cursor_update(lane: &mut SessionLane) -> arcen_protocol::cursor::CursorUpdate {
    match timed(lane.reader.next_record()).await.unwrap() {
        SessionRecord::Binary(bytes) => {
            arcen_protocol::cursor::decode_cursor_update(&bytes).unwrap()
        }
        other => panic!("expected a cursor record, got {other:?}"),
    }
}

fn inline(update: &arcen_protocol::cursor::CursorUpdate) -> bool {
    matches!(
        &update.appearance,
        arcen_protocol::cursor::CursorAppearance::Image(image) if image.inline_png.is_some()
    )
}

#[tokio::test]
async fn cursor_images_cross_the_input_lane_once_per_lane() {
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let mut deck_input = deck_lane(&session.deck, LaneRole::InputCursor).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(agent_cursor_record(10, 4))),
        RelayRoute::Queued(LaneRole::InputCursor)
    );
    let first = next_cursor_update(&mut deck_input).await;
    assert!(inline(&first));
    assert_eq!(first.sequence, 1);
    relay.route_to_deck(SessionRecord::Binary(agent_cursor_record(11, 4)));
    let second = next_cursor_update(&mut deck_input).await;
    assert!(!inline(&second), "the Deck already holds image 4");
    assert_eq!(second.sequence, 2);

    // A visible record without its image cannot be completed.
    let mut reference = agent_cursor_record(12, 9);
    reference.truncate(arcen_protocol::cursor::CURSOR_UPDATE_HEADER_SIZE);
    reference[2] &= !0x02;
    reference[32..36].copy_from_slice(&0u32.to_be_bytes());
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(reference)),
        RelayRoute::Dropped(LaneRole::InputCursor)
    );
    assert_eq!(relay.stats().cursor_records_refused, 1);

    // A replacement lane (the Pier fails this one; the Deck reopens): the
    // Deck starts with an empty cache and gets the current cursor again,
    // image included.
    deck_input
        .sender
        .send(session_record_bytes(&SessionRecord::Text(
            "{\"type\":\"quality_settings\"}".to_owned(),
        )))
        .await
        .unwrap();
    let exit = timed(async {
        loop {
            tokio::select! {
                record = deck_input.reader.next_record() => {
                    if let Err(exit) = record {
                        break exit;
                    }
                }
                _ = relay.next_event() => {}
            }
        }
    })
    .await;
    let LaneRecoveryStep::Reopened { lane, .. } =
        timed(follow_lane_exit(&session.deck, &exit)).await
    else {
        panic!("the Deck reopens its input lane");
    };
    let mut replacement = SessionLane::split(lane).unwrap();
    let resent = timed(async {
        loop {
            tokio::select! {
                record = replacement.reader.next_record() => break record.unwrap(),
                _ = relay.next_event() => {}
            }
        }
    })
    .await;
    let SessionRecord::Binary(bytes) = resent else {
        panic!("expected a cursor record");
    };
    let resent = arcen_protocol::cursor::decode_cursor_update(&bytes).unwrap();
    assert!(inline(&resent));
    assert_eq!(resent.sequence, 3);
    assert_eq!(relay.stats().cursor_records, 3);
}

#[tokio::test]
async fn host_pointer_positions_cross_the_input_lane_renumbered_newest_first() {
    use arcen_protocol::cursor_position::{
        CursorPosition, decode_cursor_position, encode_cursor_position,
    };
    let mut session = session().await;
    attach(&mut session).await;
    let mut relay = PierLaneRelay::start(&session.pier);
    let mut deck_input = deck_lane(&session.deck, LaneRole::InputCursor).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let agent = |sequence, x| {
        encode_cursor_position(&CursorPosition {
            sequence,
            on_stream: true,
            output: 0,
            generation: 0,
            monitor_id: 0,
            x,
            y: 540,
            space_width: 1920,
            space_height: 1080,
        })
        .unwrap()
        .to_vec()
    };
    for (sequence, x) in [(500, 100), (501, 200)] {
        assert_eq!(
            relay.route_to_deck(SessionRecord::Binary(agent(sequence, x))),
            RelayRoute::Queued(LaneRole::InputCursor)
        );
    }
    for (sequence, x) in [(1, 100), (2, 200)] {
        let SessionRecord::Binary(bytes) = timed(deck_input.reader.next_record()).await.unwrap()
        else {
            panic!("a position record");
        };
        let position = decode_cursor_position(&bytes).unwrap();
        assert_eq!((position.sequence, position.x), (sequence, x));
    }
    assert_eq!(
        relay.route_to_deck(SessionRecord::Binary(vec![0x51, 1, 0])),
        RelayRoute::Dropped(LaneRole::InputCursor)
    );
}
