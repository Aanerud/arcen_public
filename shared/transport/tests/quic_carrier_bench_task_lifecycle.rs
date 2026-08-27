//! Deterministic, real-Quinn proof that `run_carrier_a`/`run_carrier_b`
//! leave zero live tasks behind — and leave both connections in a state
//! that never blocks a subsequent, unrelated run — on every non-success
//! exit path, not only on the happy path already covered by
//! `quic_monitor_carrier_bench.rs`.
//!
//! Two distinct failure shapes are forced, for both carriers:
//!
//! - An outer [`arcen_transport::quic::BenchRunError::CompletionTimeout`]:
//!   each carrier is run with its sender connected normally but its
//!   receiver connection deliberately mismatched to an unrelated pair, so
//!   the receiver side blocks forever on real, unfulfillable QUIC I/O
//!   while the sender side completes normally — forcing the genuine outer
//!   `tokio::time::timeout` in `run_carrier_a`/`run_carrier_b` to elapse
//!   and drop `completion` mid-flight. Run in the background (not awaited
//!   directly), each scenario first asserts every per-monitor consumer
//!   task (and every reader task) is genuinely alive mid-flight — proving
//!   the timeout forces cleanup of tasks that actually started, not
//!   merely ones that never got the chance to — before letting the real
//!   timeout elapse and asserting zero.
//! - An early, non-timeout `?` error: each carrier is run over one
//!   ordinary, correctly-matched connection pair with a real, multi-tick
//!   paced workload, and this test explicitly closes the receiver
//!   connection a short, fixed delay after the run starts — well before
//!   the paced run would otherwise finish — asserting every producer/
//!   sender, reader, and consumer task is still genuinely alive at that
//!   point, so producer/sender tasks are still genuinely in flight when
//!   the run fails.
//!
//! Every scenario asserts `carrier_bench_live_task_count` is *exactly* its
//! pre-run baseline the instant `run_carrier_a`/`run_carrier_b` returns —
//! no polling/bounded retry — and that an entirely fresh, ordinary run,
//! launched immediately afterward with no intervening delay, completes
//! successfully, proving neither a leaked task nor any other
//! global/connection state blocks future runs. An immediate (not
//! eventual/polled) assertion is the actual contract this module's
//! "Structured task ownership" section documents: every producer/sender,
//! reader, and consumer task handle a failure path still owns — every
//! task spawned by a run is a leaf, never itself the owner of a further
//! nested task — is explicitly `abort()`-ed *and joined* — not merely
//! dropped — before `run_carrier_a`/`run_carrier_b` returns, so
//! `carrier_bench_live_task_count` must already reflect zero live tasks by
//! then, not merely "soon after". This file makes no production carrier
//! "winner" claim — it exists purely to prove cleanup, exactly like
//! `quic_monitor_carrier_bench.rs` proves ordinary completion.
//!
//! Serialized via `SERIAL`: `carrier_bench_live_task_count` reads a single
//! process-wide counter that every concurrently-running `#[tokio::test]`
//! in this binary shares, so each test here acquires `SERIAL` for its
//! entire body — otherwise one test's in-flight tasks could be counted
//! against another test's own baseline/zero assertions.
//!
//! Uses the same real mutual-TLS fixtures and connection-pair harness as
//! `quic_monitor_carrier_bench.rs` via the shared `support` module (never a
//! "skip verification" helper).

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use arcen_transport::BoundedTransportPolicy;
use arcen_transport::quic::{
    ActivePattern, BenchConfig, BenchRunError, CarrierKind, CarrierSelection, MIN_BENCH_FRAMES,
    Workload, carrier_bench_live_task_count, run_carrier_a, run_carrier_b,
};
use quinn::{Connection, VarInt};
use support::{
    build_client_rustls_config, build_quinn_client_config,
    build_quinn_client_config_for_monitor_carrier, build_quinn_server_config,
    build_quinn_server_config_for_monitor_carrier, build_server_rustls_config, client_identity,
    server_identity,
};

/// Serializes every test in this file against the process-wide
/// `carrier_bench_live_task_count` counter — see this module's own doc
/// comment.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A connected client/server QUIC connection pair with no admission or
/// binding-handshake layer above it, mirroring
/// `quic_monitor_carrier_bench.rs`'s own `ConnectedPair` harness.
struct ConnectedPair {
    #[allow(dead_code)]
    client_endpoint: quinn::Endpoint,
    #[allow(dead_code)]
    server_endpoint: quinn::Endpoint,
    client_connection: Connection,
    server_connection: Connection,
}

/// Connection pair using the live `recommended_transport_config` (uni-stream
/// limit of 1) — what Carrier A's single multiplexed stream needs.
async fn connected_pair_for_carrier_a() -> ConnectedPair {
    connected_pair_with(build_quinn_server_config, build_quinn_client_config).await
}

/// Connection pair using the test-only `monitor_carrier_transport_config`
/// (uni-stream limit of 4) — what Carrier B's one-stream-per-monitor
/// foundation needs.
async fn connected_pair_for_carrier_b() -> ConnectedPair {
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

/// A minimal, always-valid config: 1 frame total, tiny payload, no
/// artificial delay — used for both the mismatched-connection
/// `CompletionTimeout` scenarios (where its own `predicted_completion_deadline`
/// is what this test waits out) and each scenario's own "fresh run still
/// works" proof (where a small, fast config keeps the proof itself quick).
fn minimal_config(monitors: u8, carrier: CarrierKind) -> BenchConfig {
    BenchConfig {
        monitors,
        workload: Workload::Frames(MIN_BENCH_FRAMES),
        payload_bytes: 1,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(carrier),
    }
}

/// A short, multi-tick paced config used only by the mid-run
/// connection-close scenarios: paced production takes a real, deterministic
/// ~300ms, giving this test a wide, unhurried window in which to force a
/// real failure while producer/sender tasks are still genuinely in flight
/// (rather than already finished).
fn paced_config(monitors: u8, carrier: CarrierKind) -> BenchConfig {
    BenchConfig {
        monitors,
        workload: Workload::Duration(Duration::from_millis(300)),
        payload_bytes: 32,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(carrier),
    }
}

/// Asserts `carrier_bench_live_task_count` is *exactly* `baseline` right
/// now — no poll, no bounded retry, no grace period. Every
/// `run_carrier_a`/`run_carrier_b` failure path explicitly `abort()`s
/// *and joins* (`.await`s) every producer/sender, reader, and consumer
/// task handle it still owns — every one of these tasks is a leaf, never
/// itself the owner of a further nested task, so there is no residual
/// task graph any of them could still be mid-way through tearing down —
/// before returning to its own caller, so `carrier_bench_live_task_count`
/// must already be back at `baseline` the instant that call returns. See
/// this module's own doc comment and `arcen_transport::quic`'s
/// "Structured task ownership" section.
fn assert_live_task_count_is(baseline: usize) {
    let current = carrier_bench_live_task_count();
    assert_eq!(
        current, baseline,
        "expected carrier_bench_live_task_count to already be back at baseline \
         {baseline} the instant the run returned, found {current} still live"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_completion_timeout_leaves_zero_live_tasks_and_a_clean_second_run() {
    let _serial = SERIAL.lock().await;
    let baseline = carrier_bench_live_task_count();

    // Two independent, unrelated connected pairs: the sender side (the
    // first pair's client connection) completes entirely normally end to
    // end (a QUIC send does not require its peer to actually read
    // anything, only enough flow-control credit for this run's one tiny
    // frame, which the default window trivially covers) — but the receiver
    // side is paired with the *second* pair's server connection, whose own
    // peer (the second pair's client connection) this test never touches.
    // `carrier_a_receive_all`'s `accept_uni().await` therefore blocks on
    // real, unfulfillable QUIC I/O forever, deterministically forcing
    // `run_carrier_a`'s own outer `CompletionTimeout` once the sender side
    // has already finished.
    let sender_pair = connected_pair_for_carrier_a().await;
    let mismatched_receiver_pair = connected_pair_for_carrier_a().await;
    let config = minimal_config(2, CarrierKind::A);

    // Run in the background (rather than awaited directly) so this test
    // can observe the run's task graph *mid-flight* — specifically, that
    // every per-monitor consumer task has already been spawned and is
    // genuinely alive — before letting the real `CompletionTimeout`
    // elapse, proving the eventual zero-task assertion below is a forced
    // teardown of tasks that were actually running, not merely of tasks
    // that never got the chance to start.
    let sender_connection = sender_pair.client_connection.clone();
    let receiver_connection = mismatched_receiver_pair.server_connection.clone();
    let run_task = tokio::spawn(async move {
        run_carrier_a(&sender_connection, &receiver_connection, &config).await
    });

    // `run_carrier_a` spawns every per-monitor consumer task (2, for this
    // 2-monitor config) and its single demux reader task strictly before
    // it can ever reach a point that would block this sleep from
    // observing them; the reader is permanently stuck on the mismatched
    // connection's `accept_uni()`, so it can never demux a frame or drop
    // its `senders` map, which in turn guarantees every consumer task
    // stays parked on its own empty channel — neither can ever finish on
    // its own before the outer timeout. Producer tasks are not included
    // in this lower bound: this config's single unpaced frame per monitor
    // typically finishes and is joined well within this sleep, so
    // asserting only "at least reader + every consumer" (not producers
    // too) keeps this assertion robust rather than racing producer
    // completion timing.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mid_run_count = carrier_bench_live_task_count();
    assert!(
        mid_run_count >= baseline + 1 + 2,
        "expected at least the reader task (1) and every per-monitor consumer \
         task (2) to still be alive while run_carrier_a is stuck on its \
         mismatched receiver connection, found {mid_run_count} live \
         (baseline {baseline})"
    );

    let result = run_task.await.expect("run_carrier_a task join");
    assert!(
        matches!(result, Err(BenchRunError::CompletionTimeout { .. })),
        "expected a CompletionTimeout, got {result:?}"
    );

    assert_live_task_count_is(baseline);

    // Proof this run's failure left no residual/global corruption: an
    // ordinary, freshly-connected run completes cleanly right afterward.
    let clean_pair = connected_pair_for_carrier_a().await;
    let clean_result = run_carrier_a(
        &clean_pair.client_connection,
        &clean_pair.server_connection,
        &minimal_config(2, CarrierKind::A),
    )
    .await
    .expect("a fresh carrier A run after a prior timeout must still complete cleanly");
    assert_eq!(clean_result.aggregate.total_completion_failures, 0);
    assert_live_task_count_is(baseline);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_completion_timeout_leaves_zero_live_tasks_and_a_clean_second_run() {
    let _serial = SERIAL.lock().await;
    let baseline = carrier_bench_live_task_count();

    // Same mismatched-connection technique as the Carrier A scenario
    // above, but exercising Carrier B's structurally different cleanup
    // path: `Vec<AbortOnDropHandle<_>>` sender *and* reader handles (one
    // reader task per monitor, no single shared stream), each reader task
    // blocking forever in its own `accept_monitor_stream` call on the
    // mismatched connection.
    let sender_pair = connected_pair_for_carrier_b().await;
    let mismatched_receiver_pair = connected_pair_for_carrier_b().await;
    let config = minimal_config(2, CarrierKind::B);

    // Run in the background so this test can observe every per-monitor
    // consumer task genuinely alive mid-flight before the real
    // `CompletionTimeout` elapses — see the matching comment on the
    // Carrier A scenario above for why this is a stronger proof than
    // asserting zero-after-return alone.
    let sender_connection = sender_pair.client_connection.clone();
    let receiver_connection = mismatched_receiver_pair.server_connection.clone();
    let run_task = tokio::spawn(async move {
        run_carrier_b(&sender_connection, &receiver_connection, &config).await
    });

    // Every one of `run_carrier_b`'s 2 reader tasks is permanently stuck
    // on the mismatched connection's own `accept_monitor_stream` call, so
    // none can ever demux a frame or drop its own clone of `senders` —
    // guaranteeing every one of the 2 per-monitor consumer tasks stays
    // parked on its own empty channel. As with the Carrier A scenario,
    // sender tasks are not included in this lower bound since this
    // config's single unpaced frame per monitor typically finishes and is
    // joined well within this sleep.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mid_run_count = carrier_bench_live_task_count();
    assert!(
        mid_run_count >= baseline + 2 + 2,
        "expected at least every reader task (2) and every per-monitor \
         consumer task (2) to still be alive while run_carrier_b is stuck on \
         its mismatched receiver connection, found {mid_run_count} live \
         (baseline {baseline})"
    );

    let result = run_task.await.expect("run_carrier_b task join");
    assert!(
        matches!(result, Err(BenchRunError::CompletionTimeout { .. })),
        "expected a CompletionTimeout, got {result:?}"
    );

    assert_live_task_count_is(baseline);

    let clean_pair = connected_pair_for_carrier_b().await;
    let clean_result = run_carrier_b(
        &clean_pair.client_connection,
        &clean_pair.server_connection,
        &minimal_config(2, CarrierKind::B),
    )
    .await
    .expect("a fresh carrier B run after a prior timeout must still complete cleanly");
    assert_eq!(clean_result.aggregate.total_completion_failures, 0);
    assert_live_task_count_is(baseline);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_mid_run_connection_close_leaves_zero_live_tasks_and_a_clean_second_run() {
    let _serial = SERIAL.lock().await;
    let baseline = carrier_bench_live_task_count();

    let pair = connected_pair_for_carrier_a().await;
    let sender_connection = pair.client_connection.clone();
    let receiver_connection = pair.server_connection.clone();
    let config = paced_config(2, CarrierKind::A);
    let run_task = tokio::spawn(async move {
        run_carrier_a(&sender_connection, &receiver_connection, &config).await
    });

    // This paced run's own production phase alone takes ~300ms end to end
    // (see `paced_config`'s doc comment); 50ms is comfortably early enough
    // that producer/reader/consumer tasks are still genuinely running
    // (not already finished) when the connection they depend on is
    // force-closed out from under them — a real, early, non-timeout `?`
    // error path.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mid_run_count = carrier_bench_live_task_count();
    assert_eq!(
        mid_run_count,
        baseline + 2 + 1 + 2,
        "expected both producer tasks (2), the single reader task (1), and \
         both per-monitor consumer tasks (2) to still be alive 50ms into a \
         ~300ms paced run, found {mid_run_count} live (baseline {baseline})"
    );
    pair.server_connection
        .close(VarInt::from_u32(1), b"forced test failure");

    let result = run_task.await.expect("run_carrier_a task join");
    assert!(
        result.is_err(),
        "expected run_carrier_a to fail after its receiver connection was force-closed \
         mid-run, got {result:?}"
    );
    assert!(
        !matches!(result, Err(BenchRunError::CompletionTimeout { .. })),
        "expected a real transport/task failure, not the unrelated outer CompletionTimeout \
         safety net, got {result:?}"
    );

    assert_live_task_count_is(baseline);

    let clean_pair = connected_pair_for_carrier_a().await;
    let clean_result = run_carrier_a(
        &clean_pair.client_connection,
        &clean_pair.server_connection,
        &minimal_config(2, CarrierKind::A),
    )
    .await
    .expect("a fresh carrier A run after a prior mid-run failure must still complete cleanly");
    assert_eq!(clean_result.aggregate.total_completion_failures, 0);
    assert_live_task_count_is(baseline);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_mid_run_connection_close_leaves_zero_live_tasks_and_a_clean_second_run() {
    let _serial = SERIAL.lock().await;
    let baseline = carrier_bench_live_task_count();

    let pair = connected_pair_for_carrier_b().await;
    let sender_connection = pair.client_connection.clone();
    let receiver_connection = pair.server_connection.clone();
    let config = paced_config(2, CarrierKind::B);
    let run_task = tokio::spawn(async move {
        run_carrier_b(&sender_connection, &receiver_connection, &config).await
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    let mid_run_count = carrier_bench_live_task_count();
    assert_eq!(
        mid_run_count,
        baseline + 2 + 2 + 2,
        "expected both sender tasks (2), both reader tasks (2), and both \
         per-monitor consumer tasks (2) to still be alive 50ms into a \
         ~300ms paced run, found {mid_run_count} live (baseline {baseline})"
    );
    pair.server_connection
        .close(VarInt::from_u32(1), b"forced test failure");

    let result = run_task.await.expect("run_carrier_b task join");
    assert!(
        result.is_err(),
        "expected run_carrier_b to fail after its receiver connection was force-closed \
         mid-run, got {result:?}"
    );
    assert!(
        !matches!(result, Err(BenchRunError::CompletionTimeout { .. })),
        "expected a real transport/task failure, not the unrelated outer CompletionTimeout \
         safety net, got {result:?}"
    );

    assert_live_task_count_is(baseline);

    let clean_pair = connected_pair_for_carrier_b().await;
    let clean_result = run_carrier_b(
        &clean_pair.client_connection,
        &clean_pair.server_connection,
        &minimal_config(2, CarrierKind::B),
    )
    .await
    .expect("a fresh carrier B run after a prior mid-run failure must still complete cleanly");
    assert_eq!(clean_result.aggregate.total_completion_failures, 0);
    assert_live_task_count_is(baseline);
}
