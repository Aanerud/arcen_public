// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The health contract every Pier shares. These tests merge the Linux and
//! Windows Piers' suites (which pinned the same behaviour from two sides)
//! and pin the points where the copies had diverged.

use super::*;
use arcen_protocol::messages::{ClientQosSampleMsg, NetworkIdentityMsg};
use arcen_telemetry::{CorrelationId, HealthTrackerError, ValidatedLifecycleEvent};

fn rate(fps: Option<u32>) -> ContentEvidence {
    ContentEvidence::Rate(fps)
}

fn sent(frames_sent: u64) -> HostCounters {
    HostCounters {
        frames_sent,
        ..HostCounters::default()
    }
}

#[allow(clippy::unnecessary_wraps)]
fn client_qos(qos: ClientQosSampleMsg) -> Option<ClientTelemetrySnapshotMsg> {
    Some(ClientTelemetrySnapshotMsg {
        qos: Some(qos),
        network: None,
    })
}

fn field_string(fields: &StructuredFields, key: &str) -> Option<String> {
    match fields.as_map().get(key) {
        Some(FieldValue::String(text)) => Some(text.clone()),
        _ => None,
    }
}

fn field_integer(fields: &StructuredFields, key: &str) -> Option<i64> {
    match fields.as_map().get(key) {
        Some(FieldValue::Integer(number)) => Some(*number),
        _ => None,
    }
}

fn wifi_snapshot() -> ClientNetworkSnapshotMsg {
    ClientNetworkSnapshotMsg::new(
        NetworkInterfaceKind::Wifi,
        NetworkScopeMsg::Wan,
        Some(433),
        Some(-52),
        Some(1_500),
        Some(NetworkIdentityMsg::try_from("home-network".to_string()).expect("valid ssid")),
    )
    .expect("valid client network snapshot")
}

/// The latest retained client network snapshot reaches `snapshot_fields`
/// as normalized fields, never SSID or RSSI.
#[test]
fn snapshot_fields_include_normalized_client_network_facts_never_ssid_or_rssi() {
    let mut health = SessionHealth::new(QosTargets::default());
    health.record_client_at(
        1_000,
        Some(ClientTelemetrySnapshotMsg {
            qos: None,
            network: Some(wifi_snapshot()),
        }),
    );
    let observation = health.observe(2_000, 30, sent(60));
    let client_network = health
        .client()
        .and_then(|telemetry| telemetry.network.as_ref());
    let fields = snapshot_fields(&observation.sample, &observation.assessment, client_network);
    let map = fields.as_map();
    assert_eq!(
        map.get("client_network_kind"),
        Some(&FieldValue::String("wifi".to_owned()))
    );
    assert_eq!(
        map.get("client_network_scope"),
        Some(&FieldValue::String("wan".to_owned()))
    );
    assert_eq!(map.get("client_link_mbps"), Some(&FieldValue::Integer(433)));
    assert_eq!(map.get("client_mtu"), Some(&FieldValue::Integer(1_500)));
    assert!(
        !map.contains_key("client_ssid") && !map.contains_key("client_rssi_dbm"),
        "SSID/RSSI must never reach a persisted lifecycle field"
    );
    let rendered = format!("{fields:?}");
    assert!(!rendered.contains("home-network") && !rendered.contains("-52"));
}

#[test]
fn legacy_client_absence_stays_unavailable() {
    let mut health = SessionHealth::new(QosTargets::default());
    let observation = health.observe(2_000, 30, sent(60));
    assert_eq!(observation.assessment.client_experience.state, None);
    assert_eq!(
        observation.assessment.host_delivery.state,
        Some(HealthState::Ok)
    );
    assert!(health.client().is_none());
}

#[test]
fn client_and_host_health_are_assessed_independently() {
    let mut health = SessionHealth::new(QosTargets::default());
    health.record_client_at(
        1_000,
        client_qos(ClientQosSampleMsg {
            frames_decoded: Some(30),
            frames_presented: Some(10),
            sample_window_secs: SampleWindowSecs::try_from(2).ok(),
            ..ClientQosSampleMsg::default()
        }),
    );
    let observation = health.observe(2_000, 30, sent(60));
    assert_eq!(
        observation.assessment.host_delivery.state,
        Some(HealthState::Ok)
    );
    assert_eq!(
        observation.assessment.client_experience.state,
        Some(HealthState::Critical)
    );
    assert_eq!(observation.assessment.overall, Some(HealthState::Critical));
}

#[test]
fn stale_client_telemetry_returns_to_unavailable() {
    let mut health = SessionHealth::new(QosTargets::default());
    health.record_client_at(
        1_000,
        client_qos(ClientQosSampleMsg {
            frames_presented: Some(60),
            sample_window_secs: SampleWindowSecs::try_from(2).ok(),
            ..ClientQosSampleMsg::default()
        }),
    );
    assert_eq!(health.client_received_ms(), Some(1_000));
    let observation = health.observe(16_001, 30, sent(60));
    assert_eq!(observation.assessment.client_experience.state, None);
    assert_eq!(health.client_received_ms(), None);
}

#[test]
fn a_sample_the_client_already_called_old_is_never_recorded() {
    let mut health = SessionHealth::new(QosTargets::default());
    health.record_client_at(
        1_000,
        client_qos(ClientQosSampleMsg {
            frames_presented: Some(60),
            sample_age_ms: Some(CLIENT_TELEMETRY_STALE_MS + 1),
            ..ClientQosSampleMsg::default()
        }),
    );
    assert!(health.client().is_none());
}

#[test]
fn snapshot_is_exactly_sixty_second_cadenced() {
    let mut health = SessionHealth::new(QosTargets::default());
    let mut due = |at| health.observe(at, 30, HostCounters::default()).snapshot_due;
    assert!(!due(1_000));
    assert!(!due(60_999));
    assert!(due(61_000));
    assert!(!due(120_999));
    assert!(due(121_000));
}

#[test]
fn transitions_require_two_windows() {
    let mut health = SessionHealth::new(QosTargets::default());
    let first = health.observe(2_000, 30, sent(10));
    assert!(first.transition.is_none());
    let second = health.observe(4_000, 30, sent(20));
    assert_eq!(
        second.transition.as_ref().map(|(kind, _)| *kind),
        Some(LifecycleEventKind::HealthCritical)
    );
}

#[test]
fn snapshot_fields_never_show_a_healthy_zero_for_an_absent_client() {
    let mut health = SessionHealth::new(QosTargets::default());
    let observation = health.observe(2_000, 30, sent(60));
    let fields = snapshot_fields(&observation.sample, &observation.assessment, None);
    assert_eq!(field_string(&fields, "host_state").as_deref(), Some("ok"));
    assert!(
        !fields.as_map().contains_key("client_state"),
        "an absent client must omit client_state, never report it as healthy"
    );
}

/// The copies diverged here: the Windows Pier sent cumulative session
/// totals under `sample_window_secs = 2`. `HealthStatsMsg` documents
/// `sample_window_secs` as the window the stats cover, and the Linux and
/// macOS Piers sent window deltas, so every Pier now sends the window.
#[test]
fn health_stats_reports_window_deltas_not_cumulative_totals() {
    let mut health = SessionHealth::new(QosTargets::default());
    let _ = health.observe(
        2_000,
        30,
        HostCounters {
            frames_sent: 1_000,
            frames_dropped: 40,
            input_events: 500,
            ..HostCounters::default()
        },
    );
    let counters = HostCounters {
        frames_sent: 1_060,
        frames_dropped: 42,
        input_events: 560,
        last_input_sequence: 7,
        last_input_type: "mouse_move",
        ..HostCounters::default()
    };
    let observation = health.observe(4_000, 30, counters);
    let stats =
        SessionHealth::health_stats(&observation, counters, 30, "h264", "420", "1x1".to_string());
    assert_eq!(stats.frames_sent, 60, "must be the 1_060 - 1_000 delta");
    assert_eq!(stats.frames_dropped, 2, "must be the 42 - 40 delta");
    assert_eq!(stats.input_events, 60, "must be the 560 - 500 delta");
    // Last-value state (not a count) still reflects the raw counters.
    assert_eq!(stats.last_input_sequence, 7);
    assert_eq!(stats.last_input_type, "mouse_move");
    assert_eq!(stats.sample_window_secs, SampleWindowSecs::try_from(2).ok());
    assert_eq!(stats.clients_connected, 1);
    assert_eq!(
        stats.health_state,
        observation.assessment.overall.map(health_state_message)
    );
    assert!(stats.health_state.is_some());
}

/// The copies diverged here too: the Windows Pier swallowed a backwards
/// clock. It is reported, never discarded, and the tick still completes.
#[test]
fn decreasing_monotonic_clock_is_reported_not_swallowed() {
    let mut health = SessionHealth::new(QosTargets::default());
    let first = health.observe(10_000, 30, HostCounters::default());
    assert!(first.clock_error.is_none());
    let second = health.observe(9_000, 30, HostCounters::default());
    assert_eq!(
        second.clock_error,
        Some(HealthTrackerError::ClockMovedBackwards)
    );
    assert!(second.transition.is_none());
}

/// A validated configuration reload reaches an active session.
#[test]
fn set_targets_replaces_thresholds_for_an_active_session() {
    let mut health = SessionHealth::new(QosTargets::default());
    assert_eq!(health.targets(), QosTargets::default());
    let reloaded =
        QosTargets::new(80, 50, 100, 250, 50, 500, 100, 250, 3).expect("valid reloaded targets");
    assert_ne!(reloaded, QosTargets::default());
    health.set_targets(reloaded);
    assert_eq!(health.targets(), reloaded);
}

/// A still desktop sends a few frames a second against a target of sixty.
/// The capture pipeline's proof says that is the content's rate, and the
/// Deck's received count says the client kept up, so neither side may call
/// it a frame-rate fault.
#[test]
fn a_still_desktop_is_judged_against_the_content_that_changed() {
    let mut health = SessionHealth::new(QosTargets::default());
    let client = |received: u64| {
        client_qos(ClientQosSampleMsg {
            frames_received: Some(received),
            frames_decoded: Some(received),
            frames_presented: Some(received),
            sample_window_secs: SampleWindowSecs::try_from(2).ok(),
            ..ClientQosSampleMsg::default()
        })
    };
    let counters = |frames_sent: u64, content_fps| HostCounters {
        frames_sent,
        content: rate(content_fps),
        ..HostCounters::default()
    };
    health.record_client_at(1_000, client(6));
    let first = health.observe(2_000, 60, counters(6, Some(3)));
    health.record_client_at(3_000, client(6));
    let second = health.observe(4_000, 60, counters(12, Some(3)));
    for observation in [&first, &second] {
        assert_eq!(observation.sample.fps_content, Some(3));
        assert_eq!(
            observation.assessment.host_delivery.state,
            Some(HealthState::Ok)
        );
        assert_eq!(
            observation.assessment.client_experience.state,
            Some(HealthState::Ok)
        );
    }
    assert_eq!(
        second.transition.map(|(kind, _)| kind),
        Some(LifecycleEventKind::HealthOk)
    );
    // Without the pipeline's proof the same rate is the shortfall it always was.
    let unproven = health.observe(6_000, 60, counters(18, None));
    assert_eq!(
        unproven.assessment.host_delivery.state,
        Some(HealthState::Critical)
    );
}

/// A damage-driven source (macOS `ScreenCaptureKit`) proves its content
/// rate from offered frames, but only in a window that also showed the
/// screen still; the offered total keeps counting across unproven windows.
#[test]
fn offered_frames_prove_the_content_rate_only_in_a_proven_window() {
    let mut health = SessionHealth::new_with_fps_warmup(QosTargets::default());
    let counters = |frames_sent: u64, total: u64, proven: bool| HostCounters {
        frames_sent,
        content: ContentEvidence::Offered { total, proven },
        ..HostCounters::default()
    };
    let proven = health.observe(2_000, 60, counters(2, 2, true));
    assert_eq!(proven.sample.fps_content, Some(1));
    let unproven = health.observe(4_000, 60, counters(4, 4, false));
    assert_eq!(unproven.sample.fps_content, None);
    // The delta is from the previous total, not from zero.
    let again = health.observe(6_000, 60, counters(10, 10, true));
    assert_eq!(again.sample.fps_content, Some(3));
}

/// Two still monitors send a keepalive a second each; the counts are the
/// session's, so the window is judged against both outputs' target, and the
/// Deck is told each stream's own rate.
#[test]
fn several_still_outputs_are_judged_together_and_reported_per_stream() {
    let mut health = SessionHealth::new(QosTargets::default());
    let counters = |frames_sent: u64, content| HostCounters {
        frames_sent,
        content: rate(content),
        outputs: 2,
        ..HostCounters::default()
    };
    let idle = health.observe(
        2_000,
        60,
        counters(
            4,
            arcen_telemetry::outputs_content([(60, Some(0)), (60, Some(1))]).fps_content,
        ),
    );
    assert_eq!(idle.sample.fps_target, Some(120));
    assert_eq!(idle.assessment.host_delivery.state, Some(HealthState::Ok));
    let stats =
        SessionHealth::health_stats(&idle, counters(4, None), 60, "h265", "yuv420", "x".into());
    assert!((stats.fps_target - 60.0).abs() < f64::EPSILON);
    assert!((stats.fps_actual - 1.0).abs() < f64::EPSILON);
    assert_eq!(stats.frames_sent, 4);
    // One output busy and behind: its whole target is still owed.
    let behind = health.observe(
        4_000,
        60,
        counters(
            64,
            arcen_telemetry::outputs_content([(60, None), (60, Some(0))]).fps_content,
        ),
    );
    assert_eq!(
        behind.assessment.host_delivery.state,
        Some(HealthState::Critical)
    );
}

/// Monitors served at their own rates (30 and 120) are judged against the
/// sum of their targets, not the primary's rate times the outputs.
#[test]
fn outputs_at_their_own_rates_are_judged_against_their_sum() {
    let mut health = SessionHealth::new(QosTargets::default());
    let content = arcen_telemetry::outputs_content([(30, None), (120, None)]);
    let counters = |frames_sent: u64| HostCounters {
        frames_sent,
        content: rate(content.fps_content),
        outputs: 2,
        outputs_fps_target: content.fps_target,
        ..HostCounters::default()
    };
    // 120 a second in all: the primary's 30 times two would call it
    // healthy; it is four fifths of what the two monitors owe.
    let behind = health.observe(2_000, 30, counters(240));
    assert_eq!(behind.sample.fps_target, Some(150));
    assert_eq!(
        behind.assessment.host_delivery.state,
        Some(HealthState::Degraded)
    );
    let stats =
        SessionHealth::health_stats(&behind, counters(240), 30, "h265", "yuv420", "x".into());
    assert!(
        (stats.fps_target - 30.0).abs() < f64::EPSILON,
        "per stream, as before"
    );
    let delivered = health.observe(4_000, 30, counters(240 + 300));
    assert_eq!(
        delivered.assessment.host_delivery.state,
        Some(HealthState::Ok)
    );
}

/// The window maths is `arcen_telemetry::HostWindowTracker`'s, exactly: the
/// Linux and Windows copies computed the same numbers by hand.
#[test]
fn the_host_window_is_the_shared_window_tracker() {
    let mut health = SessionHealth::new_with_fps_warmup(QosTargets::default());
    let mut tracker = HostWindowTracker::default();
    let ticks = [
        (2_000, 50, 1, 900_000, 4),
        (4_100, 111, 1, 2_100_000, 9),
        (5_999, 170, 3, 3_000_000, 9),
        (5_999, 171, 3, 3_000_100, 9),
        (19_000, 600, 4, 9_000_000, 30),
    ];
    for (at, frames_sent, frames_dropped, bytes_sent, input_events) in ticks {
        let observation = health.observe(
            at,
            60,
            HostCounters {
                frames_sent,
                frames_dropped,
                bytes_sent,
                input_events,
                ..HostCounters::default()
            },
        );
        let window = tracker.observe(
            at,
            60,
            HostCounterTotals {
                frames_sent,
                frames_dropped,
                bytes_sent,
                input_events,
                frames_offered: None,
            },
            2_000,
        );
        assert_eq!(observation.sample, window.sample, "at {at}");
        assert_eq!(
            observation.bandwidth_mbps.to_bits(),
            window.bandwidth_mbps.to_bits()
        );
    }
}

#[test]
fn fps_warmup_applies_only_when_enabled() {
    let mut cold = SessionHealth::new(QosTargets::default());
    let mut warm = SessionHealth::new_with_fps_warmup(QosTargets::default());
    assert!(!cold.observe(2_000, 60, sent(2)).sample.fps_warmup);
    assert!(warm.observe(2_000, 60, sent(2)).sample.fps_warmup);
    assert!(warm.observe(11_999, 60, sent(4)).sample.fps_warmup);
    assert!(!warm.observe(12_000, 60, sent(6)).sample.fps_warmup);
}

/// Regression for the session that logged `dominant_cause=loss value=0`
/// while the Deck correctly logged `dominant_cause=unpresented value=1398`.
#[test]
fn client_presentation_loss_is_reported_with_the_client_value_not_host_zero() {
    let mut health = SessionHealth::new(QosTargets::default());
    let client = |decoded: u64, presented: u64| {
        client_qos(ClientQosSampleMsg {
            frames_decoded: Some(decoded),
            frames_presented: Some(presented),
            sample_window_secs: SampleWindowSecs::try_from(2).ok(),
            ..ClientQosSampleMsg::default()
        })
    };
    health.record_client_at(1_000, client(1_000, 861));
    let first = health.observe(2_000, 30, sent(20));
    assert!(first.transition.is_none());
    health.record_client_at(3_000, client(2_000, 1_722));
    let second = health.observe(4_000, 30, sent(40));

    assert_eq!(
        second.assessment.host_delivery.state,
        Some(HealthState::Critical)
    );
    assert_eq!(
        second.assessment.client_experience.state,
        Some(HealthState::Critical)
    );
    assert_eq!(second.sample.frames_dropped, Some(0));
    assert!(second.sample.frames_decoded > second.sample.frames_presented);

    let (kind, fields) = second.transition.expect("critical transition");
    assert_eq!(kind, LifecycleEventKind::HealthCritical);
    assert_eq!(
        field_string(&fields, "dominant_cause").as_deref(),
        Some("unpresented")
    );
    // 2000 decoded, 1722 presented -> 278 unpresented, two still in flight
    // at the window boundary -> 1380 basis points.
    assert_eq!(field_integer(&fields, "value"), Some(1_380));
    assert_eq!(
        field_integer(&fields, "threshold"),
        Some(i64::from(
            QosTargets::default().drop_critical_basis_points()
        ))
    );
}

#[test]
fn client_presentation_loss_reports_unpresented_rather_than_zero_host_loss() {
    let sample = QosSample {
        frames_decoded: Some(1_000),
        frames_presented: Some(700),
        frames_sent: Some(1_000),
        frames_dropped: Some(0),
        ..QosSample::default()
    };
    let targets = QosTargets::default();
    let (value, threshold) = cause_value_threshold(
        HealthCause::Loss,
        HealthSideKind::ClientExperience,
        HealthState::Degraded,
        &sample,
        &targets,
    );
    assert_eq!(value, 2_980, "expected 30% supersession in basis points");
    assert_eq!(
        threshold,
        Some(u32::from(targets.drop_degraded_basis_points()))
    );
    assert_eq!(
        health_cause_name(HealthCause::Loss, HealthSideKind::ClientExperience),
        "unpresented"
    );
}

#[test]
fn host_delivery_loss_still_reports_transport_counters() {
    let sample = QosSample {
        frames_sent: Some(900),
        frames_dropped: Some(100),
        frames_decoded: Some(1_000),
        frames_presented: Some(1_000),
        ..QosSample::default()
    };
    let (value, _) = cause_value_threshold(
        HealthCause::Loss,
        HealthSideKind::HostDelivery,
        HealthState::Degraded,
        &sample,
        &QosTargets::default(),
    );
    assert_eq!(value, 1_000, "100 dropped of 1000 total is 10%");
    assert_eq!(
        health_cause_name(HealthCause::Loss, HealthSideKind::HostDelivery),
        "loss"
    );
}

/// Real host delivery loss keeps its name and its own counters end to end.
#[test]
fn host_delivery_loss_keeps_its_name_and_host_counters() {
    let mut health = SessionHealth::new(QosTargets::default());
    let counters = |frames_sent: u64, frames_dropped: u64| HostCounters {
        frames_sent,
        frames_dropped,
        ..HostCounters::default()
    };
    assert!(
        health
            .observe(2_000, 30, counters(60, 60))
            .transition
            .is_none()
    );
    let second = health.observe(4_000, 30, counters(120, 120));
    assert_eq!(second.assessment.client_experience.state, None);
    let (_, fields) = second.transition.expect("critical transition");
    assert_eq!(
        field_string(&fields, "dominant_cause").as_deref(),
        Some("loss")
    );
    assert_eq!(field_integer(&fields, "value"), Some(5_000));
}

/// One `HealthCause`, two meanings. If these ever agree, the distinction
/// has been lost again.
#[test]
fn the_two_sides_read_different_counters_for_the_same_cause() {
    let sample = QosSample {
        frames_sent: Some(1_000),
        frames_dropped: Some(0),
        frames_decoded: Some(1_000),
        frames_presented: Some(500),
        ..QosSample::default()
    };
    let targets = QosTargets::default();
    let value = |side| {
        cause_value_threshold(
            HealthCause::Loss,
            side,
            HealthState::Degraded,
            &sample,
            &targets,
        )
        .0
    };
    assert_eq!(value(HealthSideKind::ClientExperience), 4_980);
    assert_eq!(value(HealthSideKind::HostDelivery), 0);
}

#[test]
fn unpresented_is_zero_rather_than_dividing_by_no_decoded_frames() {
    assert_eq!(unpresented_basis_points(&QosSample::default()), 0);
    assert_eq!(
        unpresented_basis_points(&QosSample {
            frames_decoded: Some(0),
            frames_presented: Some(0),
            ..QosSample::default()
        }),
        0
    );
    // Presented may briefly exceed decoded across a window boundary.
    assert_eq!(
        unpresented_basis_points(&QosSample {
            frames_decoded: Some(10),
            frames_presented: Some(20),
            ..QosSample::default()
        }),
        0
    );
}

#[test]
fn snapshots_keep_host_loss_visible_without_undeclared_fields() {
    let sample = QosSample {
        timestamp_ms: 4_000,
        frames_sent: Some(60),
        frames_dropped: Some(0),
        frames_decoded: Some(1_000),
        frames_presented: Some(861),
        ..QosSample::default()
    };
    let fields = snapshot_fields(&sample, &HealthAssessment::default(), None);
    assert_eq!(field_integer(&fields, "drop_basis_points"), Some(0));
    assert_eq!(field_integer(&fields, "unpresented_basis_points"), None);
    assert_eq!(
        field_string(&fields, "overall_state").as_deref(),
        Some("unavailable")
    );
}

/// `HEALTH_OK`, `HEALTH_DEGRADED`, `HEALTH_CRITICAL` and `HEALTH_SNAPSHOT`
/// carry closed field schemas: an undeclared key makes
/// `ValidatedLifecycleEvent::new` reject the event and it is dropped. Every
/// field set built here — with and without client network facts — must be
/// accepted.
#[test]
fn every_generated_health_field_set_is_accepted_by_the_closed_schema() {
    const TARGET_FPS: u32 = 100;
    let correlation = CorrelationId::new("0123456789abcdef").expect("valid correlation identifier");
    let mut health = SessionHealth::new(QosTargets::default());
    health.record_client_at(
        0,
        Some(ClientTelemetrySnapshotMsg {
            qos: None,
            network: Some(wifi_snapshot()),
        }),
    );
    let targets = QosTargets::default();
    // Target 100 fps and 2-second windows make `fps_actual = frames / 2` a
    // whole percentage, so the bands derived from `QosTargets` are hit
    // exactly.
    let frames_at_percent = |percent: u32| u64::from(percent) * 2;
    let critical = u32::from(targets.fps_critical_percent());
    let degraded = u32::from(targets.fps_degraded_percent());
    let steps: [u64; 6] = [
        frames_at_percent(critical / 2),
        frames_at_percent(critical / 2),
        frames_at_percent(u32::midpoint(critical, degraded)),
        frames_at_percent(u32::midpoint(critical, degraded)),
        frames_at_percent(degraded + 30),
        frames_at_percent(degraded + 30),
    ];
    let mut kinds = Vec::new();
    let mut timestamp = 2_000_u64;
    let mut total = 0_u64;
    for frames in steps {
        total += frames;
        let observation = health.observe(timestamp, TARGET_FPS, sent(total));
        if let Some((kind, fields)) = observation.transition {
            ValidatedLifecycleEvent::new(kind, correlation.clone(), fields).unwrap_or_else(
                |error| panic!("{kind:?} produced fields the closed schema rejects: {error:?}"),
            );
            kinds.push(kind);
        }
        let network = health
            .client()
            .and_then(|telemetry| telemetry.network.as_ref());
        assert!(network.is_some());
        for client_network in [None, network] {
            let snapshot =
                snapshot_fields(&observation.sample, &observation.assessment, client_network);
            ValidatedLifecycleEvent::new(
                LifecycleEventKind::HealthSnapshot,
                correlation.clone(),
                snapshot,
            )
            .unwrap_or_else(|error| {
                panic!("health snapshot produced fields the closed schema rejects: {error:?}")
            });
        }
        timestamp += 2_000;
    }
    for kind in [
        LifecycleEventKind::HealthCritical,
        LifecycleEventKind::HealthDegraded,
        LifecycleEventKind::HealthOk,
    ] {
        assert!(kinds.contains(&kind), "{kind:?} never fired: {kinds:?}");
    }
}

#[test]
fn window_summaries_name_the_worst_state_or_say_unavailable() {
    let fields = window_summary_fields(5, 300, 2, 40, Some(HealthState::Degraded));
    assert_eq!(field_integer(&fields, "window_ticks"), Some(5));
    assert_eq!(field_integer(&fields, "frames_sent"), Some(300));
    assert_eq!(
        field_string(&fields, "worst_overall_state").as_deref(),
        Some("degraded")
    );
    let empty = window_summary_fields(0, u64::MAX, 0, 0, None);
    assert_eq!(field_integer(&empty, "frames_sent"), Some(i64::MAX));
    assert_eq!(
        field_string(&empty, "worst_overall_state").as_deref(),
        Some("unavailable")
    );
}

#[test]
fn the_wire_and_log_names_of_a_state_agree_with_its_serde_form() {
    for state in [
        HealthState::Ok,
        HealthState::Degraded,
        HealthState::Critical,
    ] {
        let wire = serde_json::to_value(health_state_message(state)).expect("wire state");
        let canonical = serde_json::to_value(state).expect("canonical state");
        assert_eq!(wire, canonical);
        assert_eq!(wire, serde_json::Value::from(state.as_str()));
    }
    assert_eq!(state_name(None), "unavailable");
}

/// A relay fills the connection's path into an agent's `health_stats` and
/// leaves every other message, and stats an agent measured, alone.
#[test]
fn a_relay_fills_the_path_into_an_agent_s_health_stats_only() {
    let path = || arcen_protocol::messages::PathQualityMsg {
        congestion_control: "bbr".to_owned(),
        rtt_us: 12_000,
        sent_packets: 40,
        window_ms: 2_000,
        ..Default::default()
    };
    let agent = serde_json::to_string(&HealthStatsMsg {
        frames_sent: 120,
        ..Default::default()
    })
    .unwrap();
    let filled: HealthStatsMsg =
        serde_json::from_str(&health_stats_with_path(&agent, path).expect("filled")).unwrap();
    assert_eq!(filled.path, Some(path()));
    assert_eq!(filled.frames_sent, 120);
    let measured = serde_json::to_string(&filled).unwrap();
    assert!(health_stats_with_path(&measured, || unreachable!()).is_none());
    assert!(health_stats_with_path(r#"{"type":"cursor_shape"}"#, || unreachable!()).is_none());
    assert!(
        health_stats_with_path(
            r#"{"type":"text_commit_result","note":"health_stats"}"#,
            || { unreachable!() }
        )
        .is_none()
    );
}

/// A Pier judges a Deck's still-desktop window as the Deck does: the live
/// macOS Deck's keepalives came back without presentation times
/// (`fps 0 < 42` on a Linux Pier) and a Windows host's caret window left
/// a few frames untimestamped (`unpresented` on the Windows Pier).
#[test]
fn a_deck_on_a_still_desktop_is_healthy_and_a_real_stall_is_not() {
    let client_state = |qos: ClientQosSampleMsg| {
        let mut health = SessionHealth::new(QosTargets::default());
        health.record_client_at(1_000, client_qos(qos));
        health
            .observe(2_000, 60, sent(2))
            .assessment
            .client_experience
            .state
    };
    // `untimed` of the superseded frames reached the compositor.
    let window = |received: u64, presented: u64, superseded: Option<u64>, untimed: Option<u64>| {
        ClientQosSampleMsg {
            frames_received: Some(received),
            frames_decoded: Some(received),
            frames_presented: Some(presented),
            frames_superseded: superseded,
            frames_submitted_untimed: untimed,
            sample_window_secs: SampleWindowSecs::try_from(5).ok(),
            ..ClientQosSampleMsg::default()
        }
    };
    // Keepalives the compositor returned without a timestamp.
    assert_eq!(
        client_state(window(5, 0, Some(5), Some(5))),
        Some(HealthState::Ok)
    );
    assert_eq!(
        client_state(window(11, 6, Some(0), Some(0))),
        Some(HealthState::Ok)
    );
    assert_eq!(
        client_state(window(11, 6, None, None)),
        Some(HealthState::Ok)
    );
    // A Deck that predates `frames_superseded`: its untimestamped keepalives.
    assert_eq!(
        client_state(window(5, 0, None, None)),
        Some(HealthState::Ok)
    );
    // Motion at sixty a second, all on the glass.
    assert_eq!(
        client_state(window(300, 298, Some(0), Some(0))),
        Some(HealthState::Ok)
    );
    // Bunched arrivals, a few frames replaced by newer ones.
    assert_eq!(
        client_state(window(300, 292, Some(8), Some(0))),
        Some(HealthState::Ok)
    );
    // Motion the Deck decoded but lost: a third never shown.
    assert_eq!(
        client_state(window(300, 200, Some(0), Some(0))),
        Some(HealthState::Critical)
    );
    // A newest-wins pacer shedding a steady 9 % of moving content.
    assert_eq!(
        client_state(window(300, 273, Some(27), Some(0))),
        Some(HealthState::Critical)
    );
    // A presenter that handled none of the keepalives is a stall.
    assert_eq!(
        client_state(window(5, 0, Some(0), Some(0))),
        Some(HealthState::Critical)
    );
    // Nor did one whose worker kept replacing them: nothing reached the
    // compositor.
    assert_eq!(
        client_state(window(5, 0, Some(5), Some(0))),
        Some(HealthState::Critical)
    );
}

/// One Deck window is judged once, however many Pier ticks see it: a single
/// window outside its targets (here a motion burst's first window, seen
/// live as `unpresented 757` committed on a Linux Pier) does not pass the
/// two-window rule by itself; two consecutive client windows do.
#[test]
fn one_client_window_is_one_window_for_the_hysteresis() {
    let lossy = || {
        client_qos(ClientQosSampleMsg {
            frames_received: Some(300),
            frames_decoded: Some(300),
            frames_presented: Some(260),
            frames_superseded: Some(0),
            sample_window_secs: SampleWindowSecs::try_from(5).ok(),
            ..ClientQosSampleMsg::default()
        })
    };
    let fine = || {
        client_qos(ClientQosSampleMsg {
            frames_received: Some(300),
            frames_decoded: Some(300),
            frames_presented: Some(300),
            frames_superseded: Some(0),
            sample_window_secs: SampleWindowSecs::try_from(5).ok(),
            ..ClientQosSampleMsg::default()
        })
    };
    let mut health = SessionHealth::new(QosTargets::default());
    health.record_client_at(1_000, fine());
    let mut kinds = Vec::new();
    let tick = |health: &mut SessionHealth, kinds: &mut Vec<_>, at: u64| {
        let observation = health.observe(at, 60, sent(at / 1_000 * 60));
        assert_eq!(observation.clock_error, None);
        if let Some((kind, _)) = observation.transition {
            kinds.push((at, kind));
        }
    };
    tick(&mut health, &mut kinds, 2_000);
    tick(&mut health, &mut kinds, 4_000);
    // One lossy window, seen on three ticks.
    health.record_client_at(5_000, lossy());
    tick(&mut health, &mut kinds, 6_000);
    tick(&mut health, &mut kinds, 8_000);
    tick(&mut health, &mut kinds, 10_000);
    health.record_client_at(10_500, fine());
    tick(&mut health, &mut kinds, 12_000);
    tick(&mut health, &mut kinds, 14_000);
    assert_eq!(kinds, [(4_000, LifecycleEventKind::HealthOk)]);
    // Two lossy windows in a row commit.
    health.record_client_at(15_000, lossy());
    tick(&mut health, &mut kinds, 16_000);
    health.record_client_at(20_000, lossy());
    tick(&mut health, &mut kinds, 20_000);
    assert_eq!(
        kinds.last(),
        Some(&(20_000, LifecycleEventKind::HealthCritical))
    );
}

fn deck_window(presented: u64) -> Option<ClientTelemetrySnapshotMsg> {
    client_qos(ClientQosSampleMsg {
        frames_received: Some(300),
        frames_decoded: Some(300),
        frames_presented: Some(presented),
        frames_superseded: Some(0),
        sample_window_secs: SampleWindowSecs::try_from(5).ok(),
        ..ClientQosSampleMsg::default()
    })
}

/// Client telemetry that goes missing for one tick is a change like any
/// other: the client side's committed state holds until two ticks agree.
#[test]
fn one_missing_client_sample_does_not_clear_the_client_state() {
    let mut health = SessionHealth::new(QosTargets::default());
    let kind = |observation: HealthObservation| observation.transition.map(|(kind, _)| kind);
    health.record_client_at(1_000, deck_window(260));
    assert_eq!(kind(health.observe(2_000, 60, sent(120))), None);
    health.record_client_at(5_000, deck_window(260));
    assert_eq!(
        kind(health.observe(6_000, 60, sent(360))),
        Some(LifecycleEventKind::HealthCritical)
    );
    health.record_client_at(7_000, None);
    assert_eq!(kind(health.observe(8_000, 60, sent(480))), None);
    assert_eq!(
        kind(health.observe(10_000, 60, sent(600))),
        Some(LifecycleEventKind::HealthOk)
    );
}

/// A state the session falls back to reports the window that committed it,
/// not the side's latest window.
#[test]
fn a_transition_reports_the_window_that_committed_its_state() {
    let mut health = SessionHealth::new(QosTargets::default());
    let lossy_host = |at: u64, dropped: u64| HostCounters {
        frames_sent: at / 1_000 * 60,
        frames_dropped: dropped,
        ..HostCounters::default()
    };
    health.record_client_at(1_000, deck_window(290));
    let _ = health.observe(2_000, 60, lossy_host(2_000, 120));
    let critical = health.observe(4_000, 60, lossy_host(4_000, 240));
    assert_eq!(
        critical.transition.map(|(kind, _)| kind),
        Some(LifecycleEventKind::HealthCritical)
    );
    health.record_client_at(5_000, deck_window(290));
    assert!(
        health
            .observe(6_000, 60, lossy_host(6_000, 360))
            .transition
            .is_none()
    );
    // The client's next window is fine (one of two); the host recovers.
    health.record_client_at(7_000, deck_window(300));
    assert!(
        health
            .observe(8_000, 60, lossy_host(8_000, 360))
            .transition
            .is_none()
    );
    let (kind, fields) = health
        .observe(10_000, 60, lossy_host(10_000, 360))
        .transition
        .expect("falls back to the client's degraded state");
    assert_eq!(kind, LifecycleEventKind::HealthDegraded);
    assert_eq!(
        field_string(&fields, "dominant_cause").as_deref(),
        Some("unpresented")
    );
    // 300 decoded, 290 shown, two in flight: 8 of 300.
    assert_eq!(field_integer(&fields, "value"), Some(266));
}
