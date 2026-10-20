//! Real Quinn loopback tests for the isolated lane runtime (ADR 0014,
//! plan §9): independent per-lane writers, the single accept dispatcher,
//! refusal before any worker, Deck pending authorization, supersession and
//! lane-scoped recovery.
//!
//! Latency bounds are deliberately generous (hundreds of milliseconds on a
//! loopback path whose round trip is well under one); they prove the absence
//! of video- or flood-induced blocking, not a performance target. Measured
//! values are printed for the record (`--nocapture`).

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::too_many_lines,
    clippy::cast_precision_loss
)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{
    LANE_VERSION_MISMATCH_CLOSE_CODE, LEGACY_DIRECT_STREAM_PREFACE, LaneAttachment,
    LaneAttachmentBinding, LaneEndpoint, LanePreface, LaneRole, LaneRoleSet, LaneStreamCode,
    PeerTransportMismatch,
};
use arcen_telemetry::{
    CorrelationId, FieldValue, LifecycleEventKind, StructuredFields, ValidatedLifecycleEvent,
};
use arcen_transport::BoundedTransportPolicy;
use arcen_transport::lanes::{LaneEndReason, LaneFailure, LaneRecoveryAction, LaneState};
use arcen_transport::quic::{
    Lane, LaneConnection, LaneDuplex, LaneExit, LaneOpenFailure, LaneQueueBounds, LaneReceiver,
    LaneRuntimeConfig, LaneSendError, LaneSender, LaneTelemetrySink, classify_peer_close,
    keep_send_window_interactive, lane_transport_config_arc,
};
use bytes::Bytes;
use quinn::{Connection, VarInt};
use support::{
    build_client_rustls_config, build_server_rustls_config, client_identity, server_identity,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const STEP: Duration = Duration::from_secs(5);
const LATENCY_BOUND: Duration = Duration::from_millis(250);

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

fn nz32(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).unwrap()
}

/// Record: tag, sequence, send time (micros since the test epoch), padding.
fn stamped(tag: u8, sequence: u64, size: usize) -> Bytes {
    let mut record = vec![0_u8; size.max(17)];
    record[0] = tag;
    record[1..9].copy_from_slice(&sequence.to_be_bytes());
    let sent = u64::try_from(epoch().elapsed().as_micros()).unwrap();
    record[9..17].copy_from_slice(&sent.to_be_bytes());
    Bytes::from(record)
}

fn read_stamp(record: &[u8]) -> (u8, u64, Duration) {
    let sequence = u64::from_be_bytes(record[1..9].try_into().unwrap());
    let sent = u64::from_be_bytes(record[9..17].try_into().unwrap());
    let now = u64::try_from(epoch().elapsed().as_micros()).unwrap();
    (
        record[0],
        sequence,
        Duration::from_micros(now.saturating_sub(sent)),
    )
}

/// Session-message lanes write each record verbatim; these tests use a
/// length prefix as a stand-in for the product's session framing, so the
/// shared reader's `recv_frame` can parse them.
fn session_record(tag: u8, sequence: u64, size: usize) -> Bytes {
    let inner = stamped(tag, sequence, size);
    let mut framed = Vec::with_capacity(4 + inner.len());
    framed.extend_from_slice(&u32::try_from(inner.len()).unwrap().to_be_bytes());
    framed.extend_from_slice(&inner);
    Bytes::from(framed)
}

fn re_stamp_session(record: &[u8]) -> Bytes {
    let mut framed = Vec::with_capacity(4 + record.len());
    framed.extend_from_slice(&u32::try_from(record.len()).unwrap().to_be_bytes());
    framed.extend_from_slice(record);
    Bytes::from(framed)
}

#[derive(Default)]
struct Latencies(Vec<Duration>);

impl Latencies {
    fn push(&mut self, value: Duration) {
        self.0.push(value);
    }

    fn summary(&mut self, label: &str) -> Duration {
        self.0.sort();
        let max = self.0.last().copied().unwrap_or_default();
        let p50 = self.0.get(self.0.len() / 2).copied().unwrap_or_default();
        let p99 = self
            .0
            .get(self.0.len().saturating_mul(99) / 100)
            .copied()
            .unwrap_or_default();
        println!(
            "{label}: n={} p50={:.2}ms p99={:.2}ms max={:.2}ms",
            self.0.len(),
            p50.as_secs_f64() * 1e3,
            p99.as_secs_f64() * 1e3,
            max.as_secs_f64() * 1e3
        );
        max
    }
}

#[derive(Default)]
struct Collected(Mutex<Vec<(LifecycleEventKind, StructuredFields)>>);

impl LaneTelemetrySink for Collected {
    fn record(&self, kind: LifecycleEventKind, fields: StructuredFields) {
        // Every event the runtime emits satisfies its closed schema.
        ValidatedLifecycleEvent::new(
            kind,
            CorrelationId::from_uuid_v4_bytes([7; 16]),
            fields.clone(),
        )
        .expect("lane event matches its schema");
        self.0.lock().unwrap().push((kind, fields));
    }
}

impl Collected {
    fn has(&self, kind: LifecycleEventKind, field: &str, value: &str) -> bool {
        self.0.lock().unwrap().iter().any(|(event, fields)| {
            *event == kind
                && fields.as_map().get(field) == Some(&FieldValue::String(value.to_owned()))
        })
    }

    fn has_pair(&self, kind: LifecycleEventKind, pairs: &[(&str, &str)]) -> bool {
        self.0.lock().unwrap().iter().any(|(event, fields)| {
            *event == kind
                && pairs.iter().all(|(field, value)| {
                    fields.as_map().get(*field) == Some(&FieldValue::String((*value).to_owned()))
                })
        })
    }
}

struct Pair {
    _server_endpoint: quinn::Endpoint,
    _client_endpoint: quinn::Endpoint,
    pier_connection: Connection,
    deck_connection: Connection,
    pier: LaneConnection,
    deck: LaneConnection,
    pier_events: Arc<Collected>,
    deck_events: Arc<Collected>,
    attachment: LaneAttachment,
}

async fn pair(preface_timeout: Duration) -> Pair {
    epoch();
    let policy = BoundedTransportPolicy::default();
    let server = server_identity();
    let client = client_identity();
    let server_crypto = quinn::crypto::rustls::QuicServerConfig::try_from(
        build_server_rustls_config(&server, &client.cert_der),
    )
    .unwrap();
    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(server_crypto));
    server_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Pier));
    let client_crypto = quinn::crypto::rustls::QuicClientConfig::try_from(
        build_client_rustls_config(&client, &server.cert_der),
    )
    .unwrap();
    let mut client_config = quinn::ClientConfig::new(Arc::new(client_crypto));
    client_config.transport_config(lane_transport_config_arc(&policy, LaneEndpoint::Deck));

    let server_endpoint =
        quinn::Endpoint::server(server_config, "127.0.0.1:0".parse::<SocketAddr>().unwrap())
            .unwrap();
    let address = server_endpoint.local_addr().unwrap();
    let client_endpoint = quinn::Endpoint::client("127.0.0.1:0".parse().unwrap()).unwrap();
    let accept = {
        let endpoint = server_endpoint.clone();
        tokio::spawn(async move { endpoint.accept().await.unwrap().await.unwrap() })
    };
    let deck_connection = client_endpoint
        .connect_with(client_config, address, "localhost")
        .unwrap()
        .await
        .unwrap();
    let pier_connection = accept.await.unwrap();
    tokio::spawn(keep_send_window_interactive(pier_connection.clone()));

    let pier_events = Arc::new(Collected::default());
    let deck_events = Arc::new(Collected::default());
    let pier = LaneConnection::start(
        pier_connection.clone(),
        LaneRuntimeConfig::new(LaneEndpoint::Pier)
            .with_preface_timeout(preface_timeout)
            .with_telemetry(pier_events.clone()),
    );
    let deck = LaneConnection::start(
        deck_connection.clone(),
        LaneRuntimeConfig::new(LaneEndpoint::Deck)
            .with_preface_timeout(preface_timeout)
            .with_telemetry(deck_events.clone()),
    );
    Pair {
        _server_endpoint: server_endpoint,
        _client_endpoint: client_endpoint,
        pier_connection,
        deck_connection,
        pier,
        deck,
        pier_events,
        deck_events,
        attachment: LaneAttachment {
            generation: NonZeroU64::new(7).unwrap(),
            binding: LaneAttachmentBinding::generate().unwrap(),
        },
    }
}

struct Control {
    deck_send: LaneSender,
    deck_recv: LaneReceiver,
    pier_send: LaneSender,
    pier_recv: LaneReceiver,
}

async fn timed<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .expect("step timed out")
}

/// Opens the control lane and authorizes the attachment on both sides with
/// every role, as the products do after authentication.
async fn attach(pair: &Pair) -> Control {
    let deck_lane = timed(pair.deck.open_lane(LaneRole::Control)).await.unwrap();
    let pier_lane = timed(pair.pier.next_peer_lane(LaneRole::Control))
        .await
        .unwrap();
    pair.pier
        .authorize(pair.attachment, LaneRoleSet::all())
        .unwrap();
    pair.deck
        .authorize(pair.attachment, LaneRoleSet::all())
        .unwrap();
    let (deck_send, deck_recv) = deck_lane.split_default();
    let (pier_send, pier_recv) = pier_lane.split_default();
    Control {
        deck_send: deck_send.unwrap(),
        deck_recv: deck_recv.unwrap(),
        pier_send: pier_send.unwrap(),
        pier_recv: pier_recv.unwrap(),
    }
}

async fn local_lane(connection: &LaneConnection, role: LaneRole) -> Lane {
    timed(connection.open_lane(role)).await.unwrap()
}

async fn peer_lane(connection: &LaneConnection, role: LaneRole) -> Lane {
    timed(connection.next_peer_lane(role)).await.unwrap()
}

/// Echoes every session record back on the same lane.
fn spawn_echo(mut receiver: LaneReceiver, sender: LaneSender) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Ok(record) = receiver.recv_frame().await {
            if sender.send(re_stamp_session(&record)).await.is_err() {
                break;
            }
        }
    })
}

/// Sends one session record every `interval` and returns the round trips.
async fn ping(
    sender: &LaneSender,
    receiver: &mut LaneReceiver,
    tag: u8,
    count: u64,
    interval: Duration,
) -> Latencies {
    let mut latencies = Latencies::default();
    for sequence in 0..count {
        sender
            .send(session_record(tag, sequence, 64))
            .await
            .unwrap();
        let echoed = timed(receiver.recv_frame()).await.unwrap();
        let (echo_tag, echo_sequence, rtt) = read_stamp(&echoed);
        assert_eq!((echo_tag, echo_sequence), (tag, sequence));
        latencies.push(rtt);
        tokio::time::sleep(interval).await;
    }
    latencies
}

/// Sends `count` stamped audio-sized frames every 20 ms.
fn spawn_media_source(
    sender: LaneSender,
    tag: u8,
    count: u64,
) -> tokio::task::JoinHandle<LaneSender> {
    tokio::spawn(async move {
        for sequence in 0..count {
            sender.send(stamped(tag, sequence, 960)).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        sender
    })
}

/// Receives `count` frames and checks order, continuity and latency.
async fn drain_media(receiver: &mut LaneReceiver, tag: u8, count: u64) -> Latencies {
    let mut latencies = Latencies::default();
    for expected in 0..count {
        let frame = timed(receiver.recv_frame()).await.unwrap();
        let (frame_tag, sequence, latency) = read_stamp(&frame);
        assert_eq!(frame_tag, tag, "no other role's payload on this lane");
        assert_eq!(sequence, expected, "audio is continuous and in order");
        latencies.push(latency);
    }
    latencies
}

const VIDEO: u8 = 0x56;
const AUDIO: u8 = 0x41;
const INPUT: u8 = 0x49;
const CONTROL: u8 = 0x43;
const MIC: u8 = 0x4D;
const BULK: u8 = 0x42;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_without_stream_credit_does_not_block_audio_input_or_control() {
    let pair = pair(Duration::from_secs(2)).await;
    let control = attach(&pair).await;
    let (pier_video, _) = local_lane(&pair.pier, LaneRole::Video)
        .await
        .split_default();
    let (pier_audio, _) = local_lane(&pair.pier, LaneRole::AudioOutput)
        .await
        .split_default();
    // The Deck takes the video lane and never reads it: the stream's credit
    // runs out and the Pier's video writer blocks.
    let _stalled_video = peer_lane(&pair.deck, LaneRole::Video).await;
    let (_, deck_audio) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();
    let (deck_input_send, deck_input_recv) = local_lane(&pair.deck, LaneRole::InputCursor)
        .await
        .split_default();
    let (pier_input_send, pier_input_recv) = peer_lane(&pair.pier, LaneRole::InputCursor)
        .await
        .split_default();
    let pier_video = pier_video.unwrap();

    let video_source = {
        let video = pier_video.clone();
        tokio::spawn(async move {
            let mut sequence = 0;
            while video
                .send(stamped(VIDEO, sequence, 256 * 1024))
                .await
                .is_ok()
            {
                sequence += 1;
            }
            sequence
        })
    };
    let blocked_by = Instant::now() + STEP;
    while pier_video.queued().0 < LaneQueueBounds::for_role(LaneRole::Video).max_records {
        assert!(Instant::now() < blocked_by, "video never blocked");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let Control {
        deck_send,
        mut deck_recv,
        pier_send,
        pier_recv,
    } = control;
    let control_echo = spawn_echo(pier_recv, pier_send);
    let input_echo = spawn_echo(pier_input_recv.unwrap(), pier_input_send.unwrap());
    let audio = spawn_media_source(pier_audio.unwrap(), AUDIO, 75);
    let mut deck_audio = deck_audio.unwrap();
    let deck_input_send = deck_input_send.unwrap();
    let mut deck_input_recv = deck_input_recv.unwrap();
    let (mut audio_latency, mut input_rtt, mut control_rtt) = tokio::join!(
        drain_media(&mut deck_audio, AUDIO, 75),
        ping(
            &deck_input_send,
            &mut deck_input_recv,
            INPUT,
            150,
            Duration::from_millis(10)
        ),
        ping(
            &deck_send,
            &mut deck_recv,
            CONTROL,
            30,
            Duration::from_millis(50)
        ),
    );
    let _audio_sender = audio.await.unwrap();

    let video_status = pair.pier.status().role(LaneRole::Video).to_owned();
    println!("video while stalled: {video_status:?}");
    assert!(audio_latency.summary("audio one-way, video stalled") < LATENCY_BOUND);
    assert!(input_rtt.summary("input round trip, video stalled") < LATENCY_BOUND);
    assert!(control_rtt.summary("control round trip, video stalled") < LATENCY_BOUND);
    assert!(
        video_status.queued_records <= 4,
        "video queueing is bounded"
    );
    assert!(
        video_status.bytes_sent <= 2 * 1024 * 1024,
        "the stalled video lane holds no more than its stream credit"
    );
    assert!(video_status.starved >= 1);
    assert!(
        pair.pier_events
            .has(LifecycleEventKind::LaneStarved, "role", "video")
    );
    assert_eq!(pair.pier.check_send(LaneRole::Video), Ok(nz32(1)));

    // Control teardown stays responsive with video still blocked: the Deck
    // finishes its control lane and the Pier ends the attachment, cancelling
    // the blocked video writer.
    let started = Instant::now();
    deck_send.finish();
    let reason = timed(pair.pier.wait_ended()).await;
    let video_exit = timed(pier_video.closed()).await;
    let teardown = started.elapsed();
    println!(
        "control teardown with video stalled: {:.2}ms",
        teardown.as_secs_f64() * 1e3
    );
    assert_eq!(reason, LaneEndReason::ControlFailed);
    assert_eq!(
        video_exit,
        LaneExit::Cancelled(LaneStreamCode::AttachmentEnded)
    );
    assert!(teardown < Duration::from_secs(2));
    timed(video_source).await.unwrap();
    control_echo.abort();
    input_echo.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn input_flood_pushes_back_without_losing_edges_or_starving_audio_and_microphone() {
    const EDGES: u64 = 6_000;
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    let (deck_input, _) = local_lane(&pair.deck, LaneRole::InputCursor)
        .await
        .split_default();
    let (_, pier_input) = peer_lane(&pair.pier, LaneRole::InputCursor)
        .await
        .split_default();
    let (deck_mic, _) = local_lane(&pair.deck, LaneRole::Microphone)
        .await
        .split_default();
    let (_, pier_mic) = peer_lane(&pair.pier, LaneRole::Microphone)
        .await
        .split_default();
    let (pier_audio, _) = local_lane(&pair.pier, LaneRole::AudioOutput)
        .await
        .split_default();
    let (_, deck_audio) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();

    // The Pier's injector is slow; every edge must still arrive once, in
    // order.
    let mut pier_input = pier_input.unwrap();
    let injector = tokio::spawn(async move {
        for expected in 0..EDGES {
            let record = timed(pier_input.recv_frame()).await.unwrap();
            let (tag, sequence, _) = read_stamp(&record);
            assert_eq!(
                (tag, sequence),
                (INPUT, expected),
                "no lost or duplicate edge"
            );
            if expected % 200 == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    });
    let deck_input = deck_input.unwrap();
    let flood = {
        let input = deck_input.clone();
        tokio::spawn(async move {
            let mut pushed_back = 0_u64;
            let mut max_queued = 0;
            for sequence in 0..EDGES {
                let record = session_record(INPUT, sequence, 1_000);
                match input.try_send(record) {
                    Ok(()) => {}
                    Err(LaneSendError::Full(record)) => {
                        pushed_back += 1;
                        input.send(record).await.unwrap();
                    }
                    Err(other) => panic!("unexpected {other}"),
                }
                max_queued = max_queued.max(input.queued().0);
            }
            (pushed_back, max_queued)
        })
    };
    let mic = spawn_media_source(deck_mic.unwrap(), MIC, 50);
    let audio = spawn_media_source(pier_audio.unwrap(), AUDIO, 50);
    let mut pier_mic = pier_mic.unwrap();
    let mut deck_audio = deck_audio.unwrap();
    let (mut mic_latency, mut audio_latency) = tokio::join!(
        drain_media(&mut pier_mic, MIC, 50),
        drain_media(&mut deck_audio, AUDIO, 50)
    );
    let (pushed_back, max_queued) = timed(flood).await.unwrap();
    timed(injector).await.unwrap();
    let _ = (mic.await.unwrap(), audio.await.unwrap());

    let input_status = pair.deck.status().role(LaneRole::InputCursor).to_owned();
    println!(
        "input flood: pushed back {pushed_back} times, max queued {max_queued}, {input_status:?}"
    );
    assert!(pushed_back > 0, "back-pressure is exposed to the producer");
    assert!(
        input_status.priority_demotions > 0,
        "the flooding input lane shared the audio priority"
    );
    assert!(max_queued <= LaneQueueBounds::for_role(LaneRole::InputCursor).max_records);
    assert_eq!(input_status.records_sent, EDGES);
    assert_eq!(input_status.drops.discarded_on_close, 0);
    assert!(mic_latency.summary("microphone one-way, input flood") < LATENCY_BOUND);
    assert!(audio_latency.summary("audio one-way, input flood") < LATENCY_BOUND);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bulk_flood_does_not_starve_control_input_or_audio() {
    let pair = pair(Duration::from_secs(2)).await;
    let control = attach(&pair).await;
    let (deck_bulk, _) = local_lane(&pair.deck, LaneRole::BulkAux)
        .await
        .split_default();
    let (_, pier_bulk) = peer_lane(&pair.pier, LaneRole::BulkAux)
        .await
        .split_default();
    let (deck_input_send, deck_input_recv) = local_lane(&pair.deck, LaneRole::InputCursor)
        .await
        .split_default();
    let (pier_input_send, pier_input_recv) = peer_lane(&pair.pier, LaneRole::InputCursor)
        .await
        .split_default();
    let (pier_audio, _) = local_lane(&pair.pier, LaneRole::AudioOutput)
        .await
        .split_default();
    let (_, deck_audio) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();

    let deck_bulk = deck_bulk.unwrap();
    let flood = {
        let bulk = deck_bulk.clone();
        tokio::spawn(async move {
            let mut sequence = 0;
            while bulk
                .send(session_record(BULK, sequence, 256 * 1024))
                .await
                .is_ok()
            {
                sequence += 1;
            }
        })
    };
    let mut pier_bulk = pier_bulk.unwrap();
    let bulk_sink = tokio::spawn(async move {
        let mut bytes = 0_usize;
        while let Ok(record) = pier_bulk.recv_frame().await {
            bytes += record.len();
        }
        bytes
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let Control {
        deck_send,
        mut deck_recv,
        pier_send,
        pier_recv,
    } = control;
    let control_echo = spawn_echo(pier_recv, pier_send);
    let input_echo = spawn_echo(pier_input_recv.unwrap(), pier_input_send.unwrap());
    let audio = spawn_media_source(pier_audio.unwrap(), AUDIO, 50);
    let mut deck_audio = deck_audio.unwrap();
    let deck_input_send = deck_input_send.unwrap();
    let mut deck_input_recv = deck_input_recv.unwrap();
    let started = Instant::now();
    let (mut audio_latency, mut input_rtt, mut control_rtt) = tokio::join!(
        drain_media(&mut deck_audio, AUDIO, 50),
        ping(
            &deck_input_send,
            &mut deck_input_recv,
            INPUT,
            100,
            Duration::from_millis(10)
        ),
        ping(
            &deck_send,
            &mut deck_recv,
            CONTROL,
            20,
            Duration::from_millis(50)
        ),
    );
    let elapsed = started.elapsed();
    let _ = audio.await.unwrap();
    let bulk_bytes = pair.deck.status().role(LaneRole::BulkAux).bytes_sent;
    println!(
        "bulk flood: {:.1} MiB in {:.2}s alongside interactive lanes",
        bulk_bytes as f64 / (1024.0 * 1024.0),
        elapsed.as_secs_f64()
    );
    assert!(bulk_bytes > 4 * 1024 * 1024, "bulk kept flowing");
    assert!(audio_latency.summary("audio one-way, bulk flood") < LATENCY_BOUND);
    assert!(input_rtt.summary("input round trip, bulk flood") < LATENCY_BOUND);
    assert!(control_rtt.summary("control round trip, bulk flood") < LATENCY_BOUND);
    deck_bulk.finish();
    drop(deck_bulk);
    flood.abort();
    control_echo.abort();
    input_echo.abort();
    bulk_sink.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn video_failures_reopen_only_video_with_new_generations_and_no_duplicates() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    let (pier_video, _) = local_lane(&pair.pier, LaneRole::Video)
        .await
        .split_default();
    let (pier_audio, _) = local_lane(&pair.pier, LaneRole::AudioOutput)
        .await
        .split_default();
    let (_, deck_video) = peer_lane(&pair.deck, LaneRole::Video).await.split_default();
    let (_, deck_audio) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();
    let audio = spawn_media_source(pier_audio.unwrap(), AUDIO, 40);
    let mut deck_audio = deck_audio.unwrap();
    let audio_check = tokio::spawn(async move {
        drain_media(&mut deck_audio, AUDIO, 40).await;
        deck_audio
    });

    // Generation 1: the Deck rejects a frame as malformed.
    let pier_video = pier_video.unwrap();
    let mut deck_video = deck_video.unwrap();
    let mut next = 0_u64;
    let mut received = Vec::new();
    for _ in 0..5 {
        pier_video.send(stamped(VIDEO, next, 4096)).await.unwrap();
        next += 1;
    }
    for _ in 0..3 {
        let frame = timed(deck_video.recv_frame()).await.unwrap();
        received.push(read_stamp(&frame).1);
    }
    let deck_action = deck_video.fail(LaneFailure::MalformedFrame);
    assert_eq!(
        deck_action,
        LaneRecoveryAction::Reopen {
            role: LaneRole::Video,
            failed: nz32(1),
            reopened_by: LaneEndpoint::Pier,
            reset_stream: true,
            request_keyframe: true,
            release_held_input: false,
        }
    );
    // Keep writing until the Pier's writer sees the stop.
    let pier_exit = timed(async {
        loop {
            if let Some(exit) = pier_video.exit() {
                break exit;
            }
            let _ = pier_video.try_send(stamped(VIDEO, next, 4096));
            next += 1;
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let LaneExit::Failed { action, .. } = pier_exit else {
        panic!("unexpected {pier_exit:?}");
    };
    assert!(matches!(
        action,
        LaneRecoveryAction::Reopen {
            role: LaneRole::Video,
            reopened_by: LaneEndpoint::Pier,
            request_keyframe: true,
            ..
        }
    ));
    // No fallback: video is unavailable, audio untouched.
    assert!(pair.pier.check_send(LaneRole::Video).is_err());
    assert!(matches!(
        pier_video.try_send(stamped(VIDEO, next, 64)),
        Err(LaneSendError::Closed(_))
    ));
    assert_eq!(pair.pier.check_send(LaneRole::AudioOutput), Ok(nz32(1)));

    // Generation 2 replaces it; frames continue without a duplicate.
    let (pier_video, _) = timed(pair.pier.reopen(&action))
        .await
        .unwrap()
        .expect("the Pier reopens video")
        .split_default();
    let pier_video = pier_video.unwrap();
    assert_eq!(pier_video.lane_generation(), nz32(2));
    let deck_lane = peer_lane(&pair.deck, LaneRole::Video).await;
    assert_eq!(deck_lane.lane_generation(), nz32(2));
    let (_, deck_video) = deck_lane.split_default();
    let mut deck_video = deck_video.unwrap();
    let resume = next;
    for _ in 0..3 {
        pier_video.send(stamped(VIDEO, next, 4096)).await.unwrap();
        next += 1;
    }
    for _ in 0..3 {
        let frame = timed(deck_video.recv_frame()).await.unwrap();
        received.push(read_stamp(&frame).1);
    }
    let mut unique = received.clone();
    unique.dedup();
    assert_eq!(unique, received, "no duplicate payload: {received:?}");
    assert!(received.windows(2).all(|pair| pair[0] < pair[1]));
    assert_eq!(&received[3..], &[resume, resume + 1, resume + 2]);

    // Generation 2: a partial framed write. The Deck stops reading inside a
    // frame larger than the stream window; the Pier resets, never finishes.
    pier_video
        .send(stamped(VIDEO, next, 4 * 1024 * 1024))
        .await
        .unwrap();
    let first = timed(deck_video.read_chunk(64 * 1024)).await.unwrap();
    assert!(!first.is_empty());
    deck_video.fail(LaneFailure::MalformedFrame);
    let pier_exit = timed(pier_video.closed()).await;
    assert!(
        matches!(
            pier_exit,
            LaneExit::Failed {
                failure: LaneFailure::PartialFrame,
                action: LaneRecoveryAction::Reopen {
                    reset_stream: true,
                    ..
                },
            }
        ),
        "{pier_exit:?}"
    );
    let status = pair.pier.status();
    assert!(status.role(LaneRole::Video).resets >= 1);
    assert_eq!(status.role(LaneRole::AudioOutput).failures, 0);
    assert_eq!(
        pair.pier.lane_state(LaneRole::AudioOutput),
        LaneState::Open(nz32(1))
    );
    assert!(pair.pier_events.has_pair(
        LifecycleEventKind::LaneFailed,
        &[("role", "video"), ("failure", "partial_frame")]
    ));
    let _ = audio.await.unwrap();
    let _ = timed(audio_check).await.unwrap();
}

/// Opens a raw stream on `connection` and writes `bytes`, bypassing the
/// runtime, to play a misbehaving or racing peer.
async fn raw_bi(connection: &Connection, bytes: &[u8]) -> (quinn::SendStream, quinn::RecvStream) {
    let (mut send, recv) = timed(connection.open_bi()).await.unwrap();
    send.write_all(bytes).await.unwrap();
    (send, recv)
}

async fn raw_uni(connection: &Connection, bytes: &[u8]) -> quinn::SendStream {
    let mut send = timed(connection.open_uni()).await.unwrap();
    send.write_all(bytes).await.unwrap();
    send
}

async fn expect_refused_bi(stream: (quinn::SendStream, quinn::RecvStream)) -> u64 {
    let (_send, mut recv) = stream;
    let mut buffer = [0_u8; 16];
    match timed(recv.read(&mut buffer)).await {
        Err(quinn::ReadError::Reset(code)) => code.into_inner(),
        other => panic!("expected a reset, got {other:?}"),
    }
}

async fn expect_stopped_uni(send: quinn::SendStream) -> u64 {
    timed(send.stopped())
        .await
        .unwrap()
        .expect("stopped with a code")
        .into_inner()
}

fn refused_code() -> u64 {
    u64::from(LaneStreamCode::Refused.code())
}

fn bound(pair: &Pair, role: LaneRole, generation: u32) -> [u8; 36] {
    LanePreface::bound(role, pair.attachment, nz32(generation))
        .unwrap()
        .encode()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_pier_refuses_bad_streams_before_any_worker() {
    let pair = pair(Duration::from_millis(300)).await;
    let pier_refused = |role: LaneRole| pair.pier.status().role(role).refused;

    // Before authorization: a Deck lane other than control.
    let early = raw_bi(
        &pair.deck_connection,
        &bound(&pair, LaneRole::InputCursor, 1),
    )
    .await;
    assert_eq!(expect_refused_bi(early).await, refused_code());
    assert!(pair.pier_events.has_pair(
        LifecycleEventKind::LaneRefused,
        &[("role", "input_cursor"), ("reason", "control_not_first")]
    ));

    let control = attach(&pair).await;

    // Wrong shape: control on a unidirectional stream.
    let wrong_shape = raw_uni(
        &pair.deck_connection,
        &LanePreface::control(nz32(9)).encode(),
    )
    .await;
    assert_eq!(expect_stopped_uni(wrong_shape).await, refused_code());
    // Duplicate control lane.
    let duplicate = raw_bi(
        &pair.deck_connection,
        &LanePreface::control(nz32(2)).encode(),
    )
    .await;
    assert_eq!(expect_refused_bi(duplicate).await, refused_code());
    // Wrong direction: the Deck claims the Pier's video role.
    let wrong_direction = raw_uni(&pair.deck_connection, &bound(&pair, LaneRole::Video, 1)).await;
    assert_eq!(expect_stopped_uni(wrong_direction).await, refused_code());
    // Unknown role byte.
    let mut unknown = bound(&pair, LaneRole::BulkAux, 1);
    unknown[6] = 0x7F;
    let unknown = raw_bi(&pair.deck_connection, &unknown).await;
    assert_eq!(expect_refused_bi(unknown).await, refused_code());
    // Stale attachment generation.
    let stale_attachment = LanePreface::bound(
        LaneRole::BulkAux,
        LaneAttachment {
            generation: NonZeroU64::new(6).unwrap(),
            binding: pair.attachment.binding,
        },
        nz32(1),
    )
    .unwrap()
    .encode();
    let stale_attachment = raw_bi(&pair.deck_connection, &stale_attachment).await;
    assert_eq!(expect_refused_bi(stale_attachment).await, refused_code());
    // Foreign binding.
    let foreign = LanePreface::bound(
        LaneRole::BulkAux,
        LaneAttachment {
            generation: pair.attachment.generation,
            binding: LaneAttachmentBinding::generate().unwrap(),
        },
        nz32(1),
    )
    .unwrap()
    .encode();
    let foreign = raw_bi(&pair.deck_connection, &foreign).await;
    assert_eq!(expect_refused_bi(foreign).await, refused_code());
    // Stale lane generation: generation 3 is admitted, then 2 is refused.
    let newer = raw_bi(&pair.deck_connection, &bound(&pair, LaneRole::BulkAux, 3)).await;
    let admitted = peer_lane(&pair.pier, LaneRole::BulkAux).await;
    assert_eq!(admitted.lane_generation(), nz32(3));
    let older = raw_bi(&pair.deck_connection, &bound(&pair, LaneRole::BulkAux, 2)).await;
    assert_eq!(expect_refused_bi(older).await, refused_code());
    // A preface that never completes.
    let mut silent = timed(pair.deck_connection.open_bi()).await.unwrap();
    silent
        .0
        .write_all(&bound(&pair, LaneRole::BulkAux, 4)[..3])
        .await
        .unwrap();
    assert_eq!(
        expect_refused_bi(silent).await,
        u64::from(LaneStreamCode::PrefaceTimeout.code())
    );
    // An oversized blob is refused after its six-byte header.
    let blob = vec![0xEE_u8; 256 * 1024];
    let (mut blob_send, blob_recv) = timed(pair.deck_connection.open_bi()).await.unwrap();
    let _ = blob_send.write(&blob).await;
    assert_eq!(
        expect_refused_bi((blob_send, blob_recv)).await,
        refused_code()
    );

    for (role, reason) in [
        ("control", "wrong_shape"),
        ("control", "duplicate_lane"),
        ("video", "wrong_direction"),
        ("unknown", "unknown_role"),
        ("bulk_aux", "stale_attachment"),
        ("bulk_aux", "attachment_mismatch"),
        ("bulk_aux", "stale_lane_generation"),
        ("unknown", "preface_timeout"),
        ("unknown", "bad_magic"),
    ] {
        assert!(
            pair.pier_events.has_pair(
                LifecycleEventKind::LaneRefused,
                &[("role", role), ("reason", reason)]
            ),
            "{role}/{reason}"
        );
    }
    let status = pair.pier.status();
    assert!(status.refused_without_role >= 3);
    assert!(pier_refused(LaneRole::BulkAux) >= 3);
    assert!(pier_refused(LaneRole::Control) >= 2);
    // Nothing refused reached an owner; the admitted lane is the only one.
    assert!(pair.pier.try_next_peer_lane(LaneRole::BulkAux).is_none());
    assert!(
        pair.pier
            .try_next_peer_lane(LaneRole::InputCursor)
            .is_none()
    );
    assert_eq!(
        pair.pier.lane_state(LaneRole::BulkAux),
        LaneState::Open(nz32(3))
    );
    drop((newer, admitted, control));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_legacy_direct_peer_is_closed_with_the_lane_version_mismatch() {
    let pair = pair(Duration::from_secs(2)).await;
    let _legacy = raw_bi(&pair.deck_connection, LEGACY_DIRECT_STREAM_PREFACE).await;
    let error = timed(pair.deck_connection.closed()).await;
    match &error {
        quinn::ConnectionError::ApplicationClosed(close) => {
            assert_eq!(
                close.error_code,
                VarInt::from_u32(LANE_VERSION_MISMATCH_CLOSE_CODE)
            );
        }
        other => panic!("unexpected {other:?}"),
    }
    assert_eq!(
        classify_peer_close(&error),
        Some(PeerTransportMismatch::LaneVersion { received: 1 })
    );
    assert_eq!(timed(pair.pier.wait_ended()).await, LaneEndReason::Closed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deck_holds_early_pier_lanes_until_authorization() {
    let pair = pair(Duration::from_secs(2)).await;
    let deck_control = timed(pair.deck.open_lane(LaneRole::Control)).await.unwrap();
    let _pier_control = peer_lane(&pair.pier, LaneRole::Control).await;
    pair.pier
        .authorize(pair.attachment, LaneRoleSet::all())
        .unwrap();
    // The Pier opens video and audio before the Deck has read the grant.
    let first_video = local_lane(&pair.pier, LaneRole::Video).await;
    let (pier_audio, _) = local_lane(&pair.pier, LaneRole::AudioOutput)
        .await
        .split_default();
    let pier_audio = pier_audio.unwrap();
    pier_audio.send(stamped(AUDIO, 0, 960)).await.unwrap();
    let held_by = Instant::now() + STEP;
    while pair.deck.pending(LaneRole::Video).is_none()
        || pair.deck.pending(LaneRole::AudioOutput).is_none()
    {
        assert!(Instant::now() < held_by, "lanes were not held");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert!(pair.deck.try_next_peer_lane(LaneRole::Video).is_none());
    assert!(
        pair.deck
            .try_next_peer_lane(LaneRole::AudioOutput)
            .is_none()
    );

    // The video lane is replaced while still held.
    let action = pair.pier.report_failure(
        LaneRole::Video,
        first_video.lane_generation(),
        LaneFailure::WriteFailed,
    );
    drop(first_video);
    let (pier_video, _) = timed(pair.pier.reopen(&action))
        .await
        .unwrap()
        .unwrap()
        .split_default();
    let pier_video = pier_video.unwrap();
    pier_video.send(stamped(VIDEO, 0, 4096)).await.unwrap();
    while pair.deck.pending(LaneRole::Video) != Some(nz32(2)) {
        assert!(Instant::now() < held_by, "replacement was not held");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let resolutions = pair
        .deck
        .authorize(pair.attachment, LaneRoleSet::all())
        .unwrap();
    assert_eq!(resolutions.len(), 2);
    assert!(
        resolutions
            .iter()
            .all(|resolution| resolution.outcome.is_ok())
    );
    let video = peer_lane(&pair.deck, LaneRole::Video).await;
    assert_eq!(video.lane_generation(), nz32(2));
    let superseded = video
        .superseded()
        .expect("held replacement reports its steps");
    assert_eq!(superseded.generation, nz32(1));
    assert!(superseded.reset_stream && superseded.request_keyframe);
    assert!(!superseded.release_held_input);
    let (_, deck_video) = video.split_default();
    let (_, deck_audio) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();
    let frame = timed(deck_video.unwrap().recv_frame()).await.unwrap();
    assert_eq!(read_stamp(&frame).0, VIDEO);
    let frame = timed(deck_audio.unwrap().recv_frame()).await.unwrap();
    assert_eq!(read_stamp(&frame).0, AUDIO);
    assert_eq!(pair.deck.status().role(LaneRole::Video).superseded, 1);
    drop(deck_control);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supersession_cancels_the_old_lane_and_signals_release_and_keyframe() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    // Pier side: a newer input lane arrives before the old one's reset.
    let (deck_input, _) = local_lane(&pair.deck, LaneRole::InputCursor)
        .await
        .split_default();
    let (_, old_input) = peer_lane(&pair.pier, LaneRole::InputCursor)
        .await
        .split_default();
    let mut old_input = old_input.unwrap();
    deck_input
        .unwrap()
        .send(session_record(INPUT, 0, 64))
        .await
        .unwrap();
    timed(old_input.recv_frame()).await.unwrap();
    let _racing = raw_bi(
        &pair.deck_connection,
        &bound(&pair, LaneRole::InputCursor, 2),
    )
    .await;
    let input = peer_lane(&pair.pier, LaneRole::InputCursor).await;
    let steps = input.superseded().expect("supersession steps");
    assert_eq!(steps.generation, nz32(1));
    assert!(steps.release_held_input && steps.reset_stream);
    assert!(!steps.request_keyframe);
    assert_eq!(
        timed(old_input.recv_frame()).await,
        Err(LaneExit::Cancelled(LaneStreamCode::Superseded))
    );

    // Deck side: video superseded by a racing generation; audio untouched.
    let (pier_audio, _) = local_lane(&pair.pier, LaneRole::AudioOutput)
        .await
        .split_default();
    let (_, deck_audio) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();
    let _pier_video = local_lane(&pair.pier, LaneRole::Video).await;
    let (_, old_video) = peer_lane(&pair.deck, LaneRole::Video).await.split_default();
    let mut old_video = old_video.unwrap();
    let mut racing = raw_uni(&pair.pier_connection, &bound(&pair, LaneRole::Video, 2)).await;
    let video = peer_lane(&pair.deck, LaneRole::Video).await;
    let steps = video.superseded().expect("supersession steps");
    assert_eq!(steps.generation, nz32(1));
    assert!(steps.request_keyframe && steps.reset_stream);
    assert!(!steps.release_held_input);
    assert_eq!(
        timed(old_video.recv_frame()).await,
        Err(LaneExit::Cancelled(LaneStreamCode::Superseded))
    );
    racing
        .write_all(&{
            let frame = stamped(VIDEO, 9, 128);
            let mut framed = u32::try_from(frame.len()).unwrap().to_be_bytes().to_vec();
            framed.extend_from_slice(&frame);
            framed
        })
        .await
        .unwrap();
    let (_, new_video) = video.split_default();
    let frame = timed(new_video.unwrap().recv_frame()).await.unwrap();
    assert_eq!(read_stamp(&frame).1, 9);
    pier_audio
        .unwrap()
        .send(stamped(AUDIO, 0, 960))
        .await
        .unwrap();
    let frame = timed(deck_audio.unwrap().recv_frame()).await.unwrap();
    assert_eq!(read_stamp(&frame).0, AUDIO);
    assert_eq!(
        pair.deck.lane_state(LaneRole::AudioOutput),
        LaneState::Open(nz32(1))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_deck_refuses_wrong_direction_and_reports_a_truncated_frame() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    // The Pier claims the Deck's microphone role.
    let wrong = raw_uni(
        &pair.pier_connection,
        &bound(&pair, LaneRole::Microphone, 1),
    )
    .await;
    assert_eq!(expect_stopped_uni(wrong).await, refused_code());
    assert!(pair.deck_events.has_pair(
        LifecycleEventKind::LaneRefused,
        &[("role", "microphone"), ("reason", "wrong_direction")]
    ));
    // A media frame that ends inside its declared length.
    let mut truncated = raw_uni(
        &pair.pier_connection,
        &bound(&pair, LaneRole::AudioOutput, 1),
    )
    .await;
    truncated.write_all(&1_000_u32.to_be_bytes()).await.unwrap();
    truncated.write_all(&[0_u8; 10]).await.unwrap();
    truncated.finish().unwrap();
    let (_, audio) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();
    let exit = timed(audio.unwrap().recv_frame()).await.unwrap_err();
    assert_eq!(
        exit,
        LaneExit::Failed {
            failure: LaneFailure::PartialFrame,
            action: LaneRecoveryAction::Reopen {
                role: LaneRole::AudioOutput,
                failed: nz32(1),
                reopened_by: LaneEndpoint::Pier,
                reset_stream: true,
                request_keyframe: false,
                release_held_input: false,
            },
        }
    );
    assert_eq!(pair.deck.lane_state(LaneRole::Video), LaneState::Absent);
    assert!(pair.deck.check_send(LaneRole::AudioOutput).is_err());
}

async fn read_some(duplex: &mut LaneDuplex) -> std::io::Result<usize> {
    let mut buffer = [0_u8; 64];
    timed(duplex.read(&mut buffer)).await
}

fn assert_aborted(result: std::io::Result<usize>) {
    let error = result.expect_err("a cancelled duplex must not deliver");
    assert_eq!(error.kind(), std::io::ErrorKind::ConnectionAborted);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_duplex_stops_delivering_when_the_attachment_ends() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    let (deck_input, deck_input_recv) = local_lane(&pair.deck, LaneRole::InputCursor)
        .await
        .split_default();
    let mut duplex = peer_lane(&pair.pier, LaneRole::InputCursor)
        .await
        .into_duplex()
        .expect("input is bidirectional");
    let token = duplex.cancellation_token();
    let deck_input = deck_input.unwrap();
    deck_input.send(session_record(INPUT, 0, 32)).await.unwrap();
    assert!(read_some(&mut duplex).await.unwrap() > 0);

    // A read is pending when the attachment ends; it is woken and fails.
    let pending = tokio::spawn(async move {
        let result = read_some(&mut duplex).await;
        (result, duplex)
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    pair.pier.end(LaneEndReason::Closed);
    let (result, mut duplex) = timed(pending).await.unwrap();
    assert_aborted(result);
    assert!(token.is_cancelled());
    assert_eq!(
        duplex.cancel_reason(),
        Some(LaneStreamCode::AttachmentEnded)
    );
    let write = timed(duplex.write(b"late")).await;
    assert_eq!(
        write.expect_err("no write after the end").kind(),
        std::io::ErrorKind::ConnectionAborted
    );
    // The Deck sees the Pier's side reset, not a clean finish.
    let exit = timed(deck_input_recv.unwrap().recv_frame())
        .await
        .unwrap_err();
    assert!(
        matches!(
            exit,
            LaneExit::Failed {
                failure: LaneFailure::PeerReset,
                ..
            }
        ),
        "{exit:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_superseded_duplex_never_delivers_a_stale_key_down() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    let (deck_input, _) = local_lane(&pair.deck, LaneRole::InputCursor)
        .await
        .split_default();
    let deck_input = deck_input.unwrap();
    let mut old = peer_lane(&pair.pier, LaneRole::InputCursor)
        .await
        .into_duplex()
        .unwrap();
    deck_input.send(session_record(INPUT, 0, 32)).await.unwrap();
    assert!(read_some(&mut old).await.unwrap() > 0);
    let pending = tokio::spawn(async move {
        let result = read_some(&mut old).await;
        (result, old)
    });

    let _racing = raw_bi(
        &pair.deck_connection,
        &bound(&pair, LaneRole::InputCursor, 2),
    )
    .await;
    let replacement = peer_lane(&pair.pier, LaneRole::InputCursor).await;
    assert!(replacement.superseded().unwrap().release_held_input);
    let (result, mut old) = timed(pending).await.unwrap();
    assert_aborted(result);
    assert_eq!(old.cancel_reason(), Some(LaneStreamCode::Superseded));

    // A key-down still in flight on the old generation is never delivered
    // after the replacement released held input.
    let _ = deck_input.try_send(session_record(INPUT, 1, 32));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_aborted(read_some(&mut old).await);
    assert_aborted(timed(old.write(b"x")).await);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disabled_lane_closes_its_duplex() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    for generation in 1..=2 {
        let lane = local_lane(&pair.deck, LaneRole::BulkAux).await;
        assert_eq!(lane.lane_generation(), nz32(generation));
        let action = pair.deck.report_failure(
            LaneRole::BulkAux,
            lane.lane_generation(),
            LaneFailure::PeerReset,
        );
        assert!(matches!(action, LaneRecoveryAction::Reopen { .. }));
        assert!(lane.is_cancelled(), "a failed generation is cancelled");
    }
    let mut duplex = local_lane(&pair.deck, LaneRole::BulkAux)
        .await
        .into_duplex()
        .unwrap();
    // No delivery succeeds on this generation, so this third consecutive
    // failure exceeds the bulk lane's budget of two.
    let action = pair.deck.report_failure(
        LaneRole::BulkAux,
        duplex.lane_generation(),
        LaneFailure::PeerReset,
    );
    assert_eq!(
        action,
        LaneRecoveryAction::Disable {
            role: LaneRole::BulkAux,
            reset_stream: false,
        }
    );
    assert_aborted(timed(duplex.write(b"after disable")).await);
    assert_aborted(read_some(&mut duplex).await);
    assert_eq!(duplex.cancel_reason(), Some(LaneStreamCode::Disabled));
    assert!(matches!(
        timed(pair.deck.open_lane(LaneRole::BulkAux)).await,
        Err(LaneOpenFailure::Registry(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_recv_frame_resumes_the_same_frame() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    let mut raw = raw_uni(
        &pair.pier_connection,
        &bound(&pair, LaneRole::AudioOutput, 1),
    )
    .await;
    let (_, receiver) = peer_lane(&pair.deck, LaneRole::AudioOutput)
        .await
        .split_default();
    let mut receiver = receiver.unwrap();
    let body: Vec<u8> = (0..1_000_u32).map(|value| (value % 251) as u8).collect();
    let length = u32::try_from(body.len()).unwrap().to_be_bytes();

    let cancel_after = async |receiver: &mut LaneReceiver| {
        tokio::select! {
            frame = receiver.recv_frame() => panic!("frame completed early: {frame:?}"),
            () = tokio::time::sleep(Duration::from_millis(100)) => {}
        }
    };
    // Cancelled inside the length prefix, then inside the body.
    raw.write_all(&length[..2]).await.unwrap();
    cancel_after(&mut receiver).await;
    raw.write_all(&length[2..]).await.unwrap();
    raw.write_all(&body[..500]).await.unwrap();
    cancel_after(&mut receiver).await;
    raw.write_all(&body[500..]).await.unwrap();
    let next = stamped(AUDIO, 1, 64);
    raw.write_all(&u32::try_from(next.len()).unwrap().to_be_bytes())
        .await
        .unwrap();
    raw.write_all(&next).await.unwrap();

    let frame = timed(receiver.recv_frame()).await.unwrap();
    assert_eq!(&frame[..], &body[..], "the cancelled frame arrives intact");
    let frame = timed(receiver.recv_frame()).await.unwrap();
    assert_eq!(read_stamp(&frame).1, 1, "framing stays in sync");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_split_duplex_wakes_a_pending_reader_on_cancellation() {
    let pair = pair(Duration::from_secs(2)).await;
    let _control = attach(&pair).await;
    let (deck_input, _deck_receiver) = local_lane(&pair.deck, LaneRole::InputCursor)
        .await
        .split_default();
    let deck_input = deck_input.unwrap();
    let duplex = peer_lane(&pair.pier, LaneRole::InputCursor)
        .await
        .into_duplex()
        .unwrap();
    let (mut reader, mut writer) = tokio::io::split(duplex);
    deck_input.send(session_record(INPUT, 0, 32)).await.unwrap();
    let mut buffer = [0_u8; 64];
    assert!(timed(reader.read(&mut buffer)).await.unwrap() > 0);

    // The reader waits in its own task; the writer is polled after it from
    // another task, so a single shared registration would hold the writer's
    // waker only.
    let reading = tokio::spawn(async move {
        let mut buffer = [0_u8; 64];
        let started = Instant::now();
        let result = reader.read(&mut buffer).await;
        (result, started.elapsed())
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let writing = tokio::spawn(async move {
        writer.write_all(b"ack").await.unwrap();
        writer.flush().await.unwrap();
        writer
    });
    let mut writer = timed(writing).await.unwrap();
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cancelled_at = Instant::now();
    pair.pier.end(LaneEndReason::Closed);
    let (result, _) = timed(reading).await.unwrap();
    println!(
        "split duplex reader woken {:.2}ms after cancellation",
        cancelled_at.elapsed().as_secs_f64() * 1e3
    );
    assert_aborted(result);
    assert!(cancelled_at.elapsed() < Duration::from_secs(1));
    assert_aborted(timed(writer.write(b"late")).await);
    let shutdown = timed(writer.shutdown()).await;
    assert_eq!(
        shutdown
            .expect_err("shutdown after cancellation fails")
            .kind(),
        std::io::ErrorKind::ConnectionAborted
    );
}
