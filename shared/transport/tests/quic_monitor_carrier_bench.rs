//! Real Quinn loopback tests for the QUIC multi-monitor carrier benchmark
//! foundation (`arcen_transport::quic::{run_carrier_a, run_carrier_b}`).
//!
//! These are deterministic *foundation* tests: small, fixed frame counts
//! that prove both carriers complete a loopback run with zero ordering,
//! payload, or completion failures for 2 and 4 monitors in both active
//! patterns. They intentionally do **not** assert on any throughput,
//! latency, or fairness *value* — this module measures, it does not gate a
//! carrier selection (see `docs/adr/0009-multi-monitor-foundation.md`'s
//! "no unverified numeric thresholds" performance gate). A separate,
//! heavier, `#[ignore]`-marked diagnostic test below is for humans running
//! `cargo test -- --ignored` on purpose, never regular CI.
//!
//! Uses the same real mutual-TLS fixtures and connection-pair harness as
//! `quic_monitor_stream.rs` via the shared `support` module (never a
//! "skip verification" helper).

#![allow(clippy::expect_used, clippy::unwrap_used)]
#![cfg(feature = "quic")]

mod support;

use std::net::SocketAddr;
use std::time::Duration;

use arcen_transport::BoundedTransportPolicy;
use arcen_transport::quic::{
    ActivePattern, BenchConfig, CarrierSelection, MIN_BENCH_DURATION, Workload, run_carrier_a,
    run_carrier_b,
};
use quinn::Connection;
use support::{
    build_client_rustls_config, build_quinn_client_config,
    build_quinn_client_config_for_monitor_carrier, build_quinn_server_config,
    build_quinn_server_config_for_monitor_carrier, build_server_rustls_config, client_identity,
    server_identity,
};

/// A connected client/server QUIC connection pair with no admission or
/// binding-handshake layer above it, mirroring `quic_monitor_stream.rs`'s
/// `ConnectedPair` harness.
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

fn assert_clean_run(result: &arcen_transport::quic::CarrierRunResult, expected_sent_frames: u64) {
    assert_eq!(result.aggregate.total_sent_frames, expected_sent_frames);
    assert_eq!(
        result.aggregate.total_delivered_frames,
        expected_sent_frames
    );
    assert_eq!(result.aggregate.total_completion_failures, 0);
    assert_eq!(result.aggregate.total_recovery_failures, 0);
    assert_eq!(result.aggregate.total_monitor_id_mismatches, 0);
    assert!(
        (result.aggregate.fairness_index - 1.0).abs() < 1e-9,
        "a fully clean, complete-delivery run must report fairness_index \
         ~1.0 (delivery-ratio normalized) regardless of the active \
         pattern's own workload volume skew, got {}",
        result.aggregate.fairness_index
    );
    for metric in &result.per_monitor {
        assert_eq!(metric.ordering_failures, 0, "monitor {}", metric.monitor_id);
        assert_eq!(metric.payload_failures, 0, "monitor {}", metric.monitor_id);
        assert_eq!(
            metric.monitor_id_mismatches, 0,
            "monitor {}",
            metric.monitor_id
        );
        assert_eq!(
            metric.completion_failures, 0,
            "monitor {}",
            metric.monitor_id
        );
    }
}

async fn run_carrier_a_loopback(
    monitors: u8,
    pattern: ActivePattern,
    frames: u64,
    payload_bytes: usize,
) {
    let pair = connected_pair_for_carrier_a().await;
    let config = BenchConfig {
        monitors,
        workload: Workload::Frames(frames),
        payload_bytes,
        pattern,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::A),
    };
    let result = run_carrier_a(&pair.client_connection, &pair.server_connection, &config)
        .await
        .expect("carrier A loopback run");
    let expected_ticks_produced: u64 = (0..monitors)
        .map(|monitor_index| {
            (0..frames)
                .filter(|&tick| {
                    arcen_transport::quic::produces_at_tick(
                        pattern,
                        usize::from(monitor_index),
                        tick,
                    )
                })
                .count() as u64
        })
        .sum();
    assert_clean_run(&result, expected_ticks_produced);
}

async fn run_carrier_b_loopback(
    monitors: u8,
    pattern: ActivePattern,
    frames: u64,
    payload_bytes: usize,
) {
    let pair = connected_pair_for_carrier_b().await;
    let config = BenchConfig {
        monitors,
        workload: Workload::Frames(frames),
        payload_bytes,
        pattern,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::B),
    };
    let result = run_carrier_b(&pair.client_connection, &pair.server_connection, &config)
        .await
        .expect("carrier B loopback run");
    let expected_ticks_produced: u64 = (0..monitors)
        .map(|monitor_index| {
            (0..frames)
                .filter(|&tick| {
                    arcen_transport::quic::produces_at_tick(
                        pattern,
                        usize::from(monitor_index),
                        tick,
                    )
                })
                .count() as u64
        })
        .sum();
    assert_clean_run(&result, expected_ticks_produced);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_completes_two_monitor_all_active_loopback() {
    run_carrier_a_loopback(2, ActivePattern::AllActive, 40, 256).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_completes_four_monitor_all_active_loopback() {
    run_carrier_a_loopback(4, ActivePattern::AllActive, 40, 256).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_completes_two_monitor_one_active_rest_idle_loopback() {
    run_carrier_a_loopback(2, ActivePattern::OneActiveRestIdle, 50, 256).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_completes_four_monitor_one_active_rest_idle_loopback() {
    run_carrier_a_loopback(4, ActivePattern::OneActiveRestIdle, 50, 256).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_completes_two_monitor_all_active_loopback() {
    run_carrier_b_loopback(2, ActivePattern::AllActive, 40, 256).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_completes_four_monitor_all_active_loopback() {
    run_carrier_b_loopback(4, ActivePattern::AllActive, 40, 256).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_completes_two_monitor_one_active_rest_idle_loopback() {
    run_carrier_b_loopback(2, ActivePattern::OneActiveRestIdle, 50, 256).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_completes_four_monitor_one_active_rest_idle_loopback() {
    run_carrier_b_loopback(4, ActivePattern::OneActiveRestIdle, 50, 256).await;
}

/// A tiny (1-byte, the `MIN_BENCH_PAYLOAD_BYTES` floor) payload and a
/// frame count (5,000/monitor) well above the scheduler's bounded queue
/// capacity (`BENCH_SCHEDULER_QUEUE_CAPACITY == 8`), fully unpaced
/// (`Workload::Frames`, no `receiver_delay`), so producers routinely
/// outrun the drain and repeatedly observe `QueueFull` — exercising
/// `carrier_a_enqueue_with_backpressure`'s wake-on-space wait under real
/// backpressure, over a real Quinn loopback connection, rather than only
/// the isolated unit-level Notify proof in the library's own test module.
/// A completed, zero-failure run here is the practical proof that
/// replacing the old fixed-200us sleep/poll retry with `Notify`-based
/// backpressure neither drops frames nor stalls under genuine saturation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_completes_a_saturated_tiny_payload_run_without_dropping_or_stalling() {
    run_carrier_a_loopback(4, ActivePattern::AllActive, 5_000, 1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_rejects_invalid_monitor_count_before_touching_the_connection() {
    let pair = connected_pair_for_carrier_a().await;
    let config = BenchConfig {
        monitors: 3,
        workload: Workload::Frames(10),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::A),
    };
    let result = run_carrier_a(&pair.client_connection, &pair.server_connection, &config).await;
    assert!(
        result.is_err(),
        "invalid monitor count must fail validation"
    );
}

/// Release CLI regression: every recognized singleton flag must be
/// rejected as a `DuplicateArg` the moment it is supplied a second time,
/// never silently overwritten by the later occurrence. This exercises the
/// exact same `parse_cli_args` entry point the `quic_multi_monitor_carrier_bench`
/// example binary's `main` calls directly, so it is a regression on the
/// real CLI contract, not merely an internal implementation detail.
#[test]
fn cli_parser_rejects_every_repeated_singleton_flag_end_to_end() {
    let base: [&str; 12] = [
        "--monitors",
        "2",
        "--frames",
        "10",
        "--payload-bytes",
        "64",
        "--pattern",
        "all-active",
        "--receiver-delay-ms",
        "0",
        "--carrier",
        "both",
    ];
    let duplicated_flags: [(&str, &str); 7] = [
        ("--monitors", "4"),
        ("--frames", "20"),
        ("--duration", "1s"),
        ("--payload-bytes", "128"),
        ("--pattern", "one-active-rest-idle"),
        ("--receiver-delay-ms", "5"),
        ("--carrier", "a"),
    ];
    for (flag, value) in duplicated_flags {
        let mut invocation: Vec<String> = base.iter().map(|value| (*value).to_owned()).collect();
        // Every other flag already appears once in `base`, so pushing it a
        // second time creates the duplicate. `--duration` does not appear
        // in `base` at all (it uses `--frames` as its workload flag), so
        // it must be pushed twice here to create its own duplicate
        // occurrence.
        invocation.push(flag.to_owned());
        invocation.push(value.to_owned());
        if flag == "--duration" {
            invocation.push(flag.to_owned());
            invocation.push(value.to_owned());
        }
        let result = arcen_transport::quic::parse_cli_args(&invocation);
        assert_eq!(
            result,
            Err(arcen_transport::quic::CliArgError::DuplicateArg(flag)),
            "expected {flag} to be rejected as a duplicate, got {result:?}"
        );
    }
}

/// A generous, CI-safe tolerance window for asserting a paced
/// `Workload::Duration` run's actual wall-clock time is "close to" its
/// requested duration: tight enough that a genuine regression back to
/// blasting all ticks unpaced (which would complete in well under 100ms)
/// is caught, wide enough to absorb ordinary scheduler/OS jitter on a
/// shared or loaded CI runner.
const DURATION_PACING_LOWER_BOUND: Duration = Duration::from_millis(850);
const DURATION_PACING_UPPER_BOUND: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_duration_workload_paces_close_to_the_requested_wall_clock_duration() {
    let pair = connected_pair_for_carrier_a().await;
    let config = BenchConfig {
        monitors: 2,
        workload: Workload::Duration(Duration::from_secs(1)),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::A),
    };
    let wall_clock_started = std::time::Instant::now();
    let result = run_carrier_a(&pair.client_connection, &pair.server_connection, &config)
        .await
        .expect("carrier A 1s duration loopback run");
    let wall_clock_elapsed = wall_clock_started.elapsed();
    assert_eq!(result.aggregate.total_completion_failures, 0);
    for measured in [result.aggregate.elapsed, wall_clock_elapsed] {
        assert!(
            measured >= DURATION_PACING_LOWER_BOUND,
            "a paced 1s Workload::Duration run must take approximately 1s of \
             actual wall-clock time, not blast through unpaced; measured {measured:?}"
        );
        assert!(
            measured <= DURATION_PACING_UPPER_BOUND,
            "a paced 1s Workload::Duration run must not run far past its \
             requested duration; measured {measured:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_duration_workload_paces_close_to_the_requested_wall_clock_duration() {
    let pair = connected_pair_for_carrier_b().await;
    let config = BenchConfig {
        monitors: 2,
        workload: Workload::Duration(Duration::from_secs(1)),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::B),
    };
    let wall_clock_started = std::time::Instant::now();
    let result = run_carrier_b(&pair.client_connection, &pair.server_connection, &config)
        .await
        .expect("carrier B 1s duration loopback run");
    let wall_clock_elapsed = wall_clock_started.elapsed();
    assert_eq!(result.aggregate.total_completion_failures, 0);
    for measured in [result.aggregate.elapsed, wall_clock_elapsed] {
        assert!(
            measured >= DURATION_PACING_LOWER_BOUND,
            "a paced 1s Workload::Duration run must take approximately 1s of \
             actual wall-clock time, not blast through unpaced; measured {measured:?}"
        );
        assert!(
            measured <= DURATION_PACING_UPPER_BOUND,
            "a paced 1s Workload::Duration run must not run far past its \
             requested duration; measured {measured:?}"
        );
    }
}

/// A generous, CI-safe tolerance window for a paced [`MIN_BENCH_DURATION`]
/// (2ms) run — the smallest representable `Workload::Duration`, resolving
/// to a single scheduling tick. This is deliberately much wider (relative
/// to the 2ms target) than [`DURATION_PACING_LOWER_BOUND`]/
/// [`DURATION_PACING_UPPER_BOUND`]'s tolerance around a 1s target: at this
/// tiny scale, ordinary task-spawn/TLS-handshake/stream-open/finish
/// overhead unrelated to pacing itself is a much larger fraction of the
/// measured time. The lower bound alone is still enough to catch a
/// regression back to the one-tick pacing-undershoot bug this test
/// exists for: before the fix, a 2ms run's last (and only) tick's pacing
/// target was `epoch + 0`, i.e. immediate/unpaced, so it would measure
/// near `0ms` instead of `>= 1ms`.
const MIN_DURATION_PACING_LOWER_BOUND: Duration = Duration::from_millis(1);
const MIN_DURATION_PACING_UPPER_BOUND: Duration = Duration::from_secs(2);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_min_duration_workload_paces_close_to_the_requested_wall_clock_duration() {
    let pair = connected_pair_for_carrier_a().await;
    let config = BenchConfig {
        monitors: 2,
        workload: Workload::Duration(MIN_BENCH_DURATION),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::A),
    };
    let wall_clock_started = std::time::Instant::now();
    let result = run_carrier_a(&pair.client_connection, &pair.server_connection, &config)
        .await
        .expect("carrier A MIN_BENCH_DURATION loopback run");
    let wall_clock_elapsed = wall_clock_started.elapsed();
    assert_eq!(result.aggregate.total_completion_failures, 0);
    assert!(
        wall_clock_elapsed >= MIN_DURATION_PACING_LOWER_BOUND,
        "a paced {MIN_BENCH_DURATION:?} Workload::Duration run must actually \
         pace for approximately its full requested span, not finish one \
         whole tick interval short (undershoot bug); measured {wall_clock_elapsed:?}"
    );
    assert!(
        wall_clock_elapsed <= MIN_DURATION_PACING_UPPER_BOUND,
        "a paced {MIN_BENCH_DURATION:?} Workload::Duration run must not run \
         far past its requested duration; measured {wall_clock_elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_min_duration_workload_paces_close_to_the_requested_wall_clock_duration() {
    let pair = connected_pair_for_carrier_b().await;
    let config = BenchConfig {
        monitors: 2,
        workload: Workload::Duration(MIN_BENCH_DURATION),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::ZERO,
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::B),
    };
    let wall_clock_started = std::time::Instant::now();
    let result = run_carrier_b(&pair.client_connection, &pair.server_connection, &config)
        .await
        .expect("carrier B MIN_BENCH_DURATION loopback run");
    let wall_clock_elapsed = wall_clock_started.elapsed();
    assert_eq!(result.aggregate.total_completion_failures, 0);
    assert!(
        wall_clock_elapsed >= MIN_DURATION_PACING_LOWER_BOUND,
        "a paced {MIN_BENCH_DURATION:?} Workload::Duration run must actually \
         pace for approximately its full requested span, not finish one \
         whole tick interval short (undershoot bug); measured {wall_clock_elapsed:?}"
    );
    assert!(
        wall_clock_elapsed <= MIN_DURATION_PACING_UPPER_BOUND,
        "a paced {MIN_BENCH_DURATION:?} Workload::Duration run must not run \
         far past its requested duration; measured {wall_clock_elapsed:?}"
    );
}

/// A generous, CI-safe tolerance window proving `receiver_delay` is
/// carrier-neutral: both Carrier A (via its per-monitor demux consumer,
/// see `carrier_a_receive_all`/`carrier_a_consume_one`) and Carrier B (via
/// its existing per-monitor stream task) apply the same configured
/// `receiver_delay` on an independent, parallel path per monitor, so a
/// 4-monitor run's real completion time is bounded by the single busiest
/// monitor's own delay cost (`frames * receiver_delay`, ~500ms for the
/// config below) for *both* carriers — not the fully-serial cost across
/// every monitor combined an earlier revision of Carrier A's reader used
/// to pay (`monitors * frames * receiver_delay`, ~2s here, 4x more).
///
/// `RECEIVER_DELAY_COMPARABILITY_UPPER_BOUND` sits between those two
/// figures (roughly double the expected ~500ms parallel floor, well under
/// the ~2s the old serial-Carrier-A bug would have taken), so a
/// regression back to the old per-frame-serialized-across-every-monitor
/// behavior would fail this bound, while ordinary task-spawn/TLS-
/// handshake/scheduling overhead does not.
const RECEIVER_DELAY_COMPARABILITY_LOWER_BOUND: Duration = Duration::from_millis(400);
const RECEIVER_DELAY_COMPARABILITY_UPPER_BOUND: Duration = Duration::from_millis(1_500);

/// Expected bounds for the nearest-rank p50 of 10 unpaced frames each
/// incurring the shared per-monitor consumer's serialized 50ms
/// `receiver_delay` (see `average_p50_nanos`'s caller for the derivation
/// of the ~250ms expected median).
const EXPECTED_P50_LOWER_BOUND: Duration = Duration::from_millis(150);
const EXPECTED_P50_UPPER_BOUND: Duration = Duration::from_millis(700);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_monitor_receiver_delay_yields_a_comparable_predicted_floor_for_both_carriers() {
    // 4 monitors, all-active, 10 frames/monitor, 50ms receiver_delay:
    // `max_per_monitor_offered_frame_count` is 10 for either carrier, so
    // the predicted completion floor (and, once demuxed in parallel per
    // monitor, the real completion time) is the same ~500ms figure for
    // both — not 4x more for Carrier A, as it would have been before
    // Carrier A's reader was split into per-monitor consumer paths.
    let config_a = BenchConfig {
        monitors: 4,
        workload: Workload::Frames(10),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::from_millis(50),
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::A),
    };
    let config_b = BenchConfig {
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::B),
        ..config_a
    };

    let pair_a = connected_pair_for_carrier_a().await;
    let started_a = std::time::Instant::now();
    let result_a = run_carrier_a(
        &pair_a.client_connection,
        &pair_a.server_connection,
        &config_a,
    )
    .await
    .expect("carrier A 4-monitor receiver_delay loopback run must not spuriously time out");
    let elapsed_a = started_a.elapsed();

    let pair_b = connected_pair_for_carrier_b().await;
    let started_b = std::time::Instant::now();
    let result_b = run_carrier_b(
        &pair_b.client_connection,
        &pair_b.server_connection,
        &config_b,
    )
    .await
    .expect("carrier B 4-monitor receiver_delay loopback run must not spuriously time out");
    let elapsed_b = started_b.elapsed();

    assert_eq!(result_a.aggregate.total_completion_failures, 0);
    assert_eq!(result_b.aggregate.total_completion_failures, 0);

    for (label, elapsed) in [("carrier A", elapsed_a), ("carrier B", elapsed_b)] {
        assert!(
            elapsed >= RECEIVER_DELAY_COMPARABILITY_LOWER_BOUND,
            "{label} must actually apply the configured receiver_delay per \
             monitor, not skip it; measured {elapsed:?}"
        );
        assert!(
            elapsed <= RECEIVER_DELAY_COMPARABILITY_UPPER_BOUND,
            "{label} must not pay a fully-serial, across-every-monitor \
             receiver_delay cost (the old carrier-A-only bug this test \
             regresses); measured {elapsed:?}"
        );
    }

    // The two carriers' real completion times must stay within a small,
    // bounded ratio of each other — proving `receiver_delay` no longer
    // biases one carrier over the other under an identical config, which
    // is the whole point of making it a carrier-neutral, per-monitor,
    // post-demux consumer delay.
    let ratio = elapsed_a.as_secs_f64() / elapsed_b.as_secs_f64().max(f64::MIN_POSITIVE);
    assert!(
        (0.25..=4.0).contains(&ratio),
        "carrier A ({elapsed_a:?}) and carrier B ({elapsed_b:?}) completion \
         times must be comparable (ratio {ratio:.3}) under an identical \
         receiver_delay config"
    );

    // Beyond wall-clock completion time, the *recorded latency itself*
    // must also reflect the same expected delay accumulation and the same
    // point-of-observation definition for both carriers: both hand every
    // frame to the exact same shared consumer stage
    // (`carrier_receive_consume_one`), which applies `receiver_delay`
    // *before* capturing `receive_elapsed`/calling `PerMonitorValidator::
    // record`. With 10 unpaced frames all arriving at this per-monitor
    // consumer well before it can drain them (each drain step costs the
    // full 50ms delay), the consumer processes them roughly serially, so
    // the nearest-rank p50 (the 5th of 10 ascending latencies) should sit
    // around 5 * 50ms = 250ms for either carrier — not a strict equality
    // (real task scheduling/TLS/channel overhead differs run to run), but
    // squarely comparable between carriers, unlike a pre-fix world where
    // one carrier's consumer never included the delay's cost in latency
    // at all.
    let p50_a = average_p50_nanos(&result_a.per_monitor);
    let p50_b = average_p50_nanos(&result_b.per_monitor);
    for (label, p50) in [("carrier A", p50_a), ("carrier B", p50_b)] {
        assert!(
            p50 >= EXPECTED_P50_LOWER_BOUND && p50 <= EXPECTED_P50_UPPER_BOUND,
            "{label}'s average per-monitor p50 latency must reflect the \
             configured receiver_delay's serialized accumulation \
             (expected roughly 250ms for 10 frames at 50ms/frame); \
             measured {p50:?}"
        );
    }
    let p50_ratio = p50_a.as_secs_f64() / p50_b.as_secs_f64().max(f64::MIN_POSITIVE);
    assert!(
        (0.3..=3.3).contains(&p50_ratio),
        "carrier A's ({p50_a:?}) and carrier B's ({p50_b:?}) recorded p50 \
         latency must be comparable (ratio {p50_ratio:.3}) under an \
         identical receiver_delay config, since both hand frames to the \
         same shared per-monitor consumer stage"
    );
}

/// Averages the `p50_latency_nanos` across every monitor that delivered at
/// least one frame (an all-active-pattern run's every monitor, for the
/// caller above), converted to a `Duration`. A monitor that delivered
/// nothing would report `None`; this test's config never produces one.
fn average_p50_nanos(per_monitor: &[arcen_transport::quic::PerMonitorMetrics]) -> Duration {
    let (sum_nanos, count) = per_monitor.iter().fold((0_u128, 0_u64), |(sum, count), m| {
        match m.p50_latency_nanos {
            Some(nanos) => (sum + u128::from(nanos), count + 1),
            None => (sum, count),
        }
    });
    assert!(
        count > 0,
        "expected at least one monitor with a recorded p50 latency"
    );
    let average_nanos = u64::try_from(sum_nanos / u128::from(count)).unwrap_or(u64::MAX);
    Duration::from_nanos(average_nanos)
}

/// A generous CI-safe upper bound proving the pathological 600s-duration /
/// 500ms-receiver-delay configuration is rejected essentially immediately
/// (by `BenchConfig::validate` inside the run function, before any task is
/// ever spawned) rather than actually executing for the ~6.9 days its raw
/// arithmetic would otherwise imply.
const PATHOLOGICAL_CONFIG_REJECTION_BOUND: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_a_rejects_the_pathological_duration_and_receiver_delay_combination_immediately() {
    let pair = connected_pair_for_carrier_a().await;
    let config = BenchConfig {
        monitors: 2,
        workload: Workload::Duration(Duration::from_secs(600)),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::from_millis(500),
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::A),
    };
    let started = std::time::Instant::now();
    let result = run_carrier_a(&pair.client_connection, &pair.server_connection, &config).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < PATHOLOGICAL_CONFIG_REJECTION_BOUND,
        "a pathological 600s/500ms config must be rejected up front, not \
         actually executed; took {elapsed:?}"
    );
    assert!(
        matches!(
            result,
            Err(arcen_transport::quic::BenchRunError::Config(
                arcen_transport::quic::BenchConfigError::ReceiverDelayFloorExceedsCap(_)
            ))
        ),
        "expected a ReceiverDelayFloorExceedsCap rejection, got {result:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn carrier_b_rejects_the_pathological_duration_and_receiver_delay_combination_immediately() {
    let pair = connected_pair_for_carrier_b().await;
    let config = BenchConfig {
        monitors: 2,
        workload: Workload::Duration(Duration::from_secs(600)),
        payload_bytes: 64,
        pattern: ActivePattern::AllActive,
        receiver_delay: Duration::from_millis(500),
        carriers: CarrierSelection::Only(arcen_transport::quic::CarrierKind::B),
    };
    let started = std::time::Instant::now();
    let result = run_carrier_b(&pair.client_connection, &pair.server_connection, &config).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < PATHOLOGICAL_CONFIG_REJECTION_BOUND,
        "a pathological 600s/500ms config must be rejected up front, not \
         actually executed; took {elapsed:?}"
    );
    assert!(
        matches!(
            result,
            Err(arcen_transport::quic::BenchRunError::Config(
                arcen_transport::quic::BenchConfigError::ReceiverDelayFloorExceedsCap(_)
            ))
        ),
        "expected a ReceiverDelayFloorExceedsCap rejection, got {result:?}"
    );
}

/// A larger, still-deterministic, diagnostic-only comparison run. Marked
/// `#[ignore]` so it never runs in ordinary CI (it is not flaky — it is
/// simply heavier than a fast unit/integration suite should be, and its
/// output is a diagnostic measurement, not a pass/fail gate). Run
/// explicitly with `cargo test --features quic -- --ignored
/// carrier_bench_diagnostic_comparison`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "heavier diagnostic run, not a CI gate; run explicitly with --ignored"]
async fn carrier_bench_diagnostic_comparison_four_monitors_all_active() {
    run_carrier_a_loopback(4, ActivePattern::AllActive, 3_000, 65_536).await;
    run_carrier_b_loopback(4, ActivePattern::AllActive, 3_000, 65_536).await;
}
