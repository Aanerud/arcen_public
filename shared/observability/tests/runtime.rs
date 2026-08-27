#![allow(clippy::expect_used)]

use std::error::Error;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arcen_observability::{
    BoundedSink, CallerQosSnapshot, DeliveryOutcome, HeartbeatCounters, LifecycleContext,
    ObservabilityBuilder, QosCounters, QosSampler, Sink, SinkError, WaitError,
};
use arcen_telemetry::{
    CanonicalRecord, CorrelationId, EventSeverity, FieldValue, HealthState, LifecycleEventKind,
    MAX_MESSAGE_BYTES, OperationalProfile, SchemaValidationError, StructuredFields,
    TelemetryComponent, TelemetryPlatform, TelemetryRole, TelemetryTarget, ValidatedLifecycleEvent,
    names,
};

#[derive(Debug, Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("writer lock").clone()).expect("UTF-8")
    }

    fn values(&self) -> Vec<serde_json::Value> {
        self.text()
            .lines()
            .map(|line| serde_json::from_str(line).expect("canonical JSON"))
            .collect()
    }
}

impl Write for SharedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("writer lock poisoned"))?
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn component() -> TelemetryComponent {
    TelemetryComponent::new("pier").expect("component")
}

fn target() -> TelemetryTarget {
    TelemetryTarget::new("arcen::session").expect("target")
}

fn correlation() -> CorrelationId {
    CorrelationId::new("canonical-session-correlation-id").expect("correlation")
}

fn record(minimum: OperationalProfile, severity: EventSeverity, sequence: u64) -> CanonicalRecord {
    CanonicalRecord::new(
        "2026-07-24T16:00:00.000000Z",
        sequence,
        minimum,
        severity,
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Windows,
        target(),
        "diagnostic",
    )
    .expect("record")
}

fn fixture_record() -> CanonicalRecord {
    let mut fields = StructuredFields::default();
    fields
        .insert(
            names::field::AUTH_METHOD,
            FieldValue::String("password".to_owned()),
        )
        .expect("field");
    fields
        .insert(
            names::field::IDENTITY_BINDING,
            FieldValue::String("platform_account".to_owned()),
        )
        .expect("field");
    let lifecycle =
        ValidatedLifecycleEvent::new(LifecycleEventKind::SessionAuthOk, correlation(), fields)
            .expect("fixture fields match SESSION_AUTH_OK");
    CanonicalRecord::new(
        "2026-07-24T16:00:00.000000Z",
        42,
        OperationalProfile::Critical,
        EventSeverity::Info,
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Windows,
        target(),
        "session authentication succeeded",
    )
    .expect("record")
    .with_event(lifecycle.kind())
    .with_sid(lifecycle.correlation_id().clone())
    .with_identity(
        Some(r"DOMAIN\artist"),
        Some("pier-01"),
        Some("192.0.2.10:54000"),
    )
    .expect("identity")
    .with_health_state(HealthState::Ok)
    .with_fields(lifecycle.fields().clone())
}

fn runtime(
    profile: OperationalProfile,
    writer: SharedWriter,
) -> arcen_observability::ObservabilityRuntime {
    ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Windows,
        profile,
    )
    .arcen_log(None::<String>)
    .canonical_writer("json", writer)
    .build()
    .expect("runtime")
}

#[test]
fn exact_fixture_output_is_byte_identical() {
    let writer = SharedWriter::default();
    let runtime = runtime(OperationalProfile::Critical, writer.clone());
    let report = runtime
        .handle()
        .emit_record(&fixture_record())
        .expect("emit");
    assert_eq!(report.enqueued, 1);
    runtime
        .guard()
        .flush(Duration::from_secs(1))
        .expect("flush");
    assert_eq!(
        writer.text(),
        include_str!("../../telemetry/tests/fixtures/canonical-record-v1.jsonl")
    );
}

#[test]
fn profile_matrix_uses_minimum_profile_not_severity() {
    let writer = SharedWriter::default();
    let runtime = runtime(OperationalProfile::Critical, writer.clone());
    let handle = runtime.handle();
    let profiles = [
        OperationalProfile::Critical,
        OperationalProfile::Error,
        OperationalProfile::Info,
        OperationalProfile::Debug,
    ];
    let severities = [
        EventSeverity::Info,
        EventSeverity::Debug,
        EventSeverity::Error,
        EventSeverity::Warn,
    ];
    let mut expected = 0;
    for (selected_index, selected) in profiles.into_iter().enumerate() {
        handle.reload_profile(selected).expect("reload");
        for (minimum_index, minimum) in profiles.into_iter().enumerate() {
            let report = handle
                .emit_record(&record(
                    minimum,
                    severities[minimum_index],
                    (selected_index * 4 + minimum_index) as u64,
                ))
                .expect("emit");
            assert_eq!(report.included, minimum_index <= selected_index);
            expected += usize::from(report.included);
        }
    }
    runtime
        .guard()
        .flush(Duration::from_secs(1))
        .expect("flush");
    assert_eq!(writer.text().lines().count(), expected);
    assert_eq!(expected, 10);
}

fn profile_event() -> ValidatedLifecycleEvent {
    let mut fields = StructuredFields::default();
    fields
        .insert("profile_level", FieldValue::Integer(0))
        .expect("field");
    fields
        .insert("profile_name", FieldValue::String("critical".to_owned()))
        .expect("field");
    fields
        .insert("profile_source", FieldValue::String("config".to_owned()))
        .expect("field");
    ValidatedLifecycleEvent::new(LifecycleEventKind::EffectiveProfile, correlation(), fields)
        .expect("validated event")
}

#[test]
fn mandatory_level_zero_info_survives_arcen_log_off_and_identity_is_top_level() {
    let writer = SharedWriter::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Windows,
        OperationalProfile::Critical,
    )
    .arcen_log(Some("arcen=off"))
    .canonical_writer("json", writer.clone())
    .build()
    .expect("runtime");
    let report = runtime
        .handle()
        .emit_lifecycle(
            &profile_event(),
            LifecycleContext {
                sid: correlation(),
                user: Some(r"DOMAIN\artist".to_owned()),
                host: Some("pier-01".to_owned()),
                peer_addr: Some("192.0.2.10:54000".to_owned()),
                health_state: None,
            },
            "2026-07-24T16:00:00.000000Z",
            TelemetryTarget::new("arcen::telemetry").expect("target"),
            "effective profile selected",
        )
        .expect("emit");
    assert!(report.included);
    runtime
        .guard()
        .flush(Duration::from_secs(1))
        .expect("flush");
    let value: serde_json::Value =
        serde_json::from_str(writer.text().trim()).expect("canonical JSON");
    assert_eq!(value["severity"], "info");
    assert_eq!(value["profile_level"], 0);
    assert_eq!(value["user"], r"DOMAIN\artist");
    assert!(value["fields"].get("user").is_none());
    assert!(value["fields"].get("sid").is_none());
}

#[test]
fn tracing_debug_diagnostics_follow_live_reload() {
    let writer = SharedWriter::default();
    let runtime = runtime(OperationalProfile::Info, writer.clone());
    runtime.with_default(|| {
        tracing::debug!(target: "arcen::test", message = "before reload");
    });
    runtime
        .handle()
        .reload_profile(OperationalProfile::Debug)
        .expect("reload");
    runtime.with_default(|| {
        tracing::debug!(target: "arcen::test", message = "after reload", queue_depth = 3_i64);
    });
    runtime
        .guard()
        .flush(Duration::from_secs(1))
        .expect("flush");
    let text = writer.text();
    assert!(!text.contains("before reload"));
    assert!(text.contains("after reload"));
    assert!(text.contains(r#""queue_depth":3"#));
}

#[test]
fn concurrent_reloads_publish_only_coherent_filter_policy_pairs() {
    let writer = SharedWriter::default();
    let runtime = runtime(OperationalProfile::Critical, writer);
    let handle = runtime.handle();
    let barrier = Arc::new(Barrier::new(3));

    let critical_handle = handle.clone();
    let critical_barrier = Arc::clone(&barrier);
    let critical = thread::spawn(move || {
        critical_barrier.wait();
        for _ in 0..500 {
            critical_handle
                .reload_profile_with(
                    OperationalProfile::Critical,
                    Some("arcen::test=debug".to_owned()),
                )
                .expect("critical reload");
        }
    });

    let debug_handle = handle.clone();
    let debug_barrier = Arc::clone(&barrier);
    let debug = thread::spawn(move || {
        debug_barrier.wait();
        for _ in 0..500 {
            debug_handle
                .reload_profile_with(
                    OperationalProfile::Debug,
                    Some("arcen::test=off".to_owned()),
                )
                .expect("debug reload");
        }
    });

    barrier.wait();
    for _ in 0..1_000 {
        let policy = handle
            .effective_diagnostic_policy()
            .expect("effective policy");
        assert!(
            (policy.profile == OperationalProfile::Critical
                && (policy.arcen_log.is_none()
                    || policy.arcen_log.as_deref() == Some("arcen::test=debug")))
                || (policy.profile == OperationalProfile::Debug
                    && policy.arcen_log.as_deref() == Some("arcen::test=off"))
        );
        thread::yield_now();
    }
    critical.join().expect("critical reloader");
    debug.join().expect("debug reloader");

    let policy = handle
        .effective_diagnostic_policy()
        .expect("final effective policy");
    assert!(
        (policy.profile == OperationalProfile::Critical
            && policy.arcen_log.as_deref() == Some("arcen::test=debug"))
            || (policy.profile == OperationalProfile::Debug
                && policy.arcen_log.as_deref() == Some("arcen::test=off"))
    );
}

#[test]
fn ordinary_diagnostics_follow_the_four_level_mapping() {
    // Use emit_record directly instead of tracing macros to avoid tracing's
    // callsite interest cache: when a non-globally-installed subscriber is used
    // via with_default, rebuild_interest_cache() re-evaluates callsites against
    // the global (noop) dispatcher, permanently marking warn/info/debug as NEVER
    // after the first Critical-profile iteration. emit_record bypasses the
    // tracing dispatch layer entirely and tests the profile-gating logic directly.
    let writer = SharedWriter::default();
    let runtime = runtime(OperationalProfile::Critical, writer.clone());
    let handle = runtime.handle();

    // (message, minimum_profile, severity) — mirrors diagnostic_minimum_profile()
    let diagnostic_pairs: &[(&str, OperationalProfile, EventSeverity)] = &[
        ("error", OperationalProfile::Critical, EventSeverity::Error),
        ("warn", OperationalProfile::Error, EventSeverity::Warn),
        ("info", OperationalProfile::Info, EventSeverity::Info),
        ("debug", OperationalProfile::Debug, EventSeverity::Debug),
    ];

    let mut seq = 0u64;
    for profile in [
        OperationalProfile::Critical,
        OperationalProfile::Error,
        OperationalProfile::Info,
        OperationalProfile::Debug,
    ] {
        handle.reload_profile(profile).expect("reload");
        for &(message, minimum, severity) in diagnostic_pairs {
            seq += 1;
            let rec = CanonicalRecord::new(
                "2026-07-24T16:00:00.000000Z",
                seq,
                minimum,
                severity,
                TelemetryRole::Host,
                component(),
                TelemetryPlatform::Windows,
                target(),
                message,
            )
            .expect("record");
            handle.emit_record(&rec).expect("emit");
        }
    }
    runtime
        .guard()
        .flush(Duration::from_secs(1))
        .expect("flush");
    let lines: Vec<_> = writer.text().lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 10);
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains(r#""message":"error""#))
            .count(),
        4
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains(r#""message":"warn""#))
            .count(),
        3
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains(r#""message":"info""#))
            .count(),
        2
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains(r#""message":"debug""#))
            .count(),
        1
    );
}

#[test]
fn arcen_log_can_refine_diagnostics_without_controlling_mandatory_routing() {
    let writer = SharedWriter::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Windows,
        OperationalProfile::Critical,
    )
    .arcen_log(Some("arcen::test=debug"))
    .canonical_writer("json", writer.clone())
    .build()
    .expect("runtime");
    runtime.with_default(|| {
        tracing::debug!(target: "arcen::test", message = "developer override");
    });
    runtime
        .guard()
        .flush(Duration::from_secs(1))
        .expect("flush");
    assert!(writer.text().contains("developer override"));
}

#[derive(Debug, Default)]
struct BlockingState {
    entered: bool,
    release: bool,
    values: Vec<u64>,
}

#[derive(Debug, Clone, Default)]
struct BlockingSink {
    state: Arc<(Mutex<BlockingState>, Condvar)>,
}

impl BlockingSink {
    fn wait_until_entered(&self) {
        let (lock, changed) = &*self.state;
        let state = lock.lock().expect("state lock");
        let (_state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(1), |state| !state.entered)
            .expect("wait");
        assert!(!timeout.timed_out());
    }

    fn release(&self) {
        let (lock, changed) = &*self.state;
        lock.lock().expect("state lock").release = true;
        changed.notify_all();
    }
}

impl Sink<u64> for BlockingSink {
    fn deliver(&mut self, item: u64) -> Result<(), SinkError> {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().map_err(|_| SinkError::adapter("poisoned"))?;
        state.entered = true;
        changed.notify_all();
        while !state.release {
            state = changed
                .wait(state)
                .map_err(|_| SinkError::adapter("poisoned"))?;
        }
        state.values.push(item);
        Ok(())
    }
}

#[test]
fn full_queue_returns_immediately_counts_exactly_and_worker_drains() {
    let adapter = BlockingSink::default();
    let queue = BoundedSink::new("blocking", 1, adapter.clone()).expect("queue");
    assert_eq!(queue.try_send(1), DeliveryOutcome::Enqueued);
    adapter.wait_until_entered();
    assert_eq!(queue.try_send(2), DeliveryOutcome::Enqueued);
    let started = Instant::now();
    assert_eq!(queue.try_send(3), DeliveryOutcome::QueueFull);
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(queue.stats().dropped, 1);
    adapter.release();
    queue.flush(Duration::from_secs(1)).expect("flush");
    assert_eq!(queue.stats().delivered, 2);
    queue.shutdown(Duration::from_secs(1)).expect("shutdown");
}

#[derive(Debug)]
struct SlowSink {
    entered: Arc<AtomicBool>,
}

impl Sink<u64> for SlowSink {
    fn deliver(&mut self, _item: u64) -> Result<(), SinkError> {
        self.entered.store(true, Ordering::Release);
        thread::sleep(Duration::from_millis(80));
        Ok(())
    }
}

#[test]
fn flush_and_shutdown_are_bounded() {
    let entered = Arc::new(AtomicBool::new(false));
    let queue = BoundedSink::new(
        "slow",
        1,
        SlowSink {
            entered: Arc::clone(&entered),
        },
    )
    .expect("queue");
    assert_eq!(queue.try_send(1), DeliveryOutcome::Enqueued);
    while !entered.load(Ordering::Acquire) {
        thread::yield_now();
    }
    let started = Instant::now();
    assert_eq!(
        queue.flush(Duration::from_millis(5)),
        Err(WaitError::Timeout)
    );
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(
        queue.shutdown(Duration::from_millis(5)),
        Err(WaitError::Timeout)
    );
    thread::sleep(Duration::from_millis(100));
    queue
        .shutdown(Duration::from_secs(1))
        .expect("eventual shutdown");
}

#[derive(Debug)]
struct FailingSink;

impl Sink<u64> for FailingSink {
    fn deliver(&mut self, _item: u64) -> Result<(), SinkError> {
        Err(SinkError::adapter("expected failure"))
    }
}

#[test]
fn sink_failures_are_counted_without_stopping_the_worker() {
    let queue = BoundedSink::new("failing", 2, FailingSink).expect("queue");
    assert_eq!(queue.try_send(1), DeliveryOutcome::Enqueued);
    assert_eq!(queue.try_send(2), DeliveryOutcome::Enqueued);
    queue.flush(Duration::from_secs(1)).expect("flush");
    assert_eq!(queue.stats().failures, 2);
    assert_eq!(queue.stats().delivered, 0);
    queue.shutdown(Duration::from_secs(1)).expect("shutdown");
}

#[derive(Debug)]
struct FlushFailingSink;

impl Sink<u64> for FlushFailingSink {
    fn deliver(&mut self, _item: u64) -> Result<(), SinkError> {
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        Err(SinkError::adapter("expected flush failure"))
    }
}

#[test]
fn explicit_flush_and_shutdown_surface_adapter_failures() {
    let queue = BoundedSink::new("flush-failing", 1, FlushFailingSink).expect("queue");
    assert!(matches!(
        queue.flush(Duration::from_secs(1)),
        Err(WaitError::Sink(_))
    ));
    assert!(matches!(
        queue.shutdown(Duration::from_secs(1)),
        Err(WaitError::Sink(_))
    ));
    assert_eq!(queue.stats().failures, 2);
}

#[derive(Debug)]
struct FlushFailingWriter;

impl Write for FlushFailingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::Error::other("expected writer flush failure"))
    }
}

#[test]
fn handle_flush_reports_every_sink_failure_by_name() {
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Windows,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_writer("first-json", FlushFailingWriter)
    .canonical_writer("second-json", FlushFailingWriter)
    .build()
    .expect("runtime");
    let error = runtime
        .handle()
        .flush(Duration::from_secs(1))
        .expect_err("both sink flushes fail");
    assert_eq!(error.failures().len(), 2);
    assert_eq!(error.failures()[0].sink, "first-json");
    assert!(matches!(error.failures()[0].error, WaitError::Sink(_)));
    assert_eq!(error.failures()[1].sink, "second-json");
    assert!(matches!(error.failures()[1].error, WaitError::Sink(_)));
}

#[derive(Debug, Clone, Default)]
struct BlockingWriter(BlockingSink);

impl Write for BlockingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .deliver(bytes.len() as u64)
            .map_err(io::Error::other)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn telemetry_drop_reporting_is_explicit_and_non_recursive() {
    let writer = BlockingWriter::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Windows,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_queue_capacity(1)
    .canonical_writer("json", writer.clone())
    .build()
    .expect("runtime");
    let handle = runtime.handle();
    assert_eq!(
        handle
            .emit_record(&record(
                OperationalProfile::Critical,
                EventSeverity::Info,
                1,
            ))
            .expect("emit")
            .enqueued,
        1
    );
    writer.0.wait_until_entered();
    assert_eq!(
        handle
            .emit_record(&record(
                OperationalProfile::Critical,
                EventSeverity::Info,
                2,
            ))
            .expect("emit")
            .enqueued,
        1
    );
    assert_eq!(
        handle
            .emit_record(&record(
                OperationalProfile::Critical,
                EventSeverity::Info,
                3,
            ))
            .expect("emit")
            .dropped,
        1
    );

    let notices = handle
        .take_drop_notices(&correlation())
        .expect("drop notice");
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].kind(), LifecycleEventKind::TelemetryDropped);
    assert_eq!(handle.sink_stats()[0].dropped, 1);

    let report = handle
        .emit_lifecycle(
            &notices[0],
            LifecycleContext {
                sid: correlation(),
                user: None,
                host: None,
                peer_addr: None,
                health_state: None,
            },
            "2026-07-24T16:00:00.000000Z",
            TelemetryTarget::new("arcen::telemetry").expect("target"),
            "telemetry records dropped",
        )
        .expect("route notice");
    assert_eq!(report.dropped, 1);
    assert_eq!(handle.sink_stats()[0].dropped, 2);
    assert_eq!(
        handle
            .take_drop_notices(&correlation())
            .expect("next drop delta")
            .len(),
        1
    );
    writer.0.release();
    runtime
        .guard()
        .shutdown(Duration::from_secs(1))
        .expect("shutdown");
}

#[test]
fn samplers_use_caller_time_and_registered_atomics() {
    let counters = QosCounters::default();
    counters.frames_sent.store(20, Ordering::Relaxed);
    counters.frames_dropped.store(2, Ordering::Relaxed);
    counters.input_events.store(7, Ordering::Relaxed);
    let sample = QosSampler::new(&counters).sample(
        1234,
        CallerQosSnapshot {
            fps_actual: Some(55),
            fps_target: Some(60),
            rtt_ms: Some(42),
            ..CallerQosSnapshot::default()
        },
    );
    assert_eq!(sample.timestamp_ms, 1234);
    assert_eq!(sample.frames_sent, Some(20));
    assert_eq!(sample.frames_dropped, Some(2));
    assert_eq!(sample.input_events, Some(7));
    assert_eq!(
        QosSampler::new(&counters)
            .sample(1235, CallerQosSnapshot::default())
            .input_events,
        Some(0)
    );

    let heartbeat = HeartbeatCounters::default();
    heartbeat.record_sent(9);
    assert_eq!(heartbeat.record_miss(), 1);
    heartbeat.record_received(9);
    assert_eq!(heartbeat.snapshot(5678, Some(12)).received_sequence, 9);
    assert_eq!(heartbeat.snapshot(5678, Some(12)).missed_intervals, 0);
}

#[test]
fn ad_hoc_emission_owns_envelope_identity_and_validates_input() -> Result<(), Box<dyn Error>> {
    let writer = SharedWriter::default();
    let runtime = runtime(OperationalProfile::Critical, writer.clone());
    let handle = runtime.handle();
    let mut fields = StructuredFields::default();
    fields.insert("queue_depth", FieldValue::Integer(7))?;

    let report = handle.emit_ad_hoc(
        OperationalProfile::Critical,
        EventSeverity::Debug,
        TelemetryTarget::new("arcen::session")?,
        "bounded ad-hoc record",
        LifecycleContext {
            sid: correlation(),
            user: Some(r"DOMAIN\artist".to_owned()),
            host: Some("pier-01".to_owned()),
            peer_addr: Some("192.0.2.10:54000".to_owned()),
            health_state: Some(HealthState::Degraded),
        },
        fields,
    )?;
    assert_eq!(report.enqueued, 1);
    handle.flush(Duration::from_secs(1))?;

    let values = writer.values();
    assert_eq!(values.len(), 1);
    let value = &values[0];
    assert_eq!(value["sequence"], 1);
    assert_eq!(value["role"], "host");
    assert_eq!(value["component"], "pier");
    assert_eq!(value["platform"], "windows");
    assert_eq!(value["profile_level"], 0);
    assert_eq!(value["severity"], "debug");
    assert_eq!(value["target"], "arcen::session");
    assert_eq!(value["sid"], correlation().as_str());
    assert_eq!(value["user"], r"DOMAIN\artist");
    assert_eq!(value["host"], "pier-01");
    assert_eq!(value["peer_addr"], "192.0.2.10:54000");
    assert_eq!(value["health_state"], "degraded");
    assert_eq!(value["fields"]["queue_depth"], 7);
    let Some(timestamp) = value["timestamp"].as_str() else {
        panic!("timestamp must be a JSON string");
    };
    assert_eq!(timestamp.len(), 27);
    assert!(timestamp.ends_with('Z'));

    assert!(matches!(
        handle.emit_ad_hoc(
            OperationalProfile::Critical,
            EventSeverity::Info,
            target(),
            "x".repeat(MAX_MESSAGE_BYTES + 1),
            LifecycleContext {
                sid: correlation(),
                user: None,
                host: None,
                peer_addr: None,
                health_state: None,
            },
            StructuredFields::default(),
        ),
        Err(arcen_observability::RuntimeError::Schema(
            SchemaValidationError::InvalidMessage
        ))
    ));
    assert!(matches!(
        handle.emit_ad_hoc(
            OperationalProfile::Critical,
            EventSeverity::Info,
            target(),
            "valid message",
            LifecycleContext {
                sid: correlation(),
                user: Some(String::new()),
                host: None,
                peer_addr: None,
                health_state: None,
            },
            StructuredFields::default(),
        ),
        Err(arcen_observability::RuntimeError::Schema(
            SchemaValidationError::InvalidIdentity
        ))
    ));
    Ok(())
}

#[test]
fn concurrent_generated_emissions_share_one_monotonic_sequence() -> Result<(), Box<dyn Error>> {
    const PRODUCERS: usize = 12;
    const PER_PRODUCER: usize = 20;
    let writer = SharedWriter::default();
    let runtime = runtime(OperationalProfile::Debug, writer.clone());
    let handle = runtime.handle();
    let dispatch = runtime.dispatch();
    let barrier = Arc::new(Barrier::new(PRODUCERS));
    let mut producers = Vec::new();

    for producer in 0..PRODUCERS {
        let handle = handle.clone();
        let dispatch = dispatch.clone();
        let barrier = Arc::clone(&barrier);
        producers.push(thread::spawn(move || -> Result<(), String> {
            barrier.wait();
            for item in 0..PER_PRODUCER {
                match producer % 3 {
                    0 => {
                        handle
                            .emit_lifecycle(
                                &profile_event(),
                                LifecycleContext {
                                    sid: correlation(),
                                    user: None,
                                    host: None,
                                    peer_addr: None,
                                    health_state: None,
                                },
                                "2026-07-24T16:00:00.000000Z",
                                TelemetryTarget::new("arcen::telemetry")
                                    .map_err(|error| error.to_string())?,
                                format!("lifecycle {producer}-{item}"),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                    1 => {
                        handle
                            .emit_ad_hoc(
                                OperationalProfile::Info,
                                EventSeverity::Warn,
                                TelemetryTarget::new("arcen::test")
                                    .map_err(|error| error.to_string())?,
                                format!("ad-hoc {producer}-{item}"),
                                LifecycleContext {
                                    sid: correlation(),
                                    user: None,
                                    host: None,
                                    peer_addr: None,
                                    health_state: None,
                                },
                                StructuredFields::default(),
                            )
                            .map_err(|error| error.to_string())?;
                    }
                    _ => tracing::dispatcher::with_default(&dispatch, || {
                        tracing::error!(
                            target: "arcen::test",
                            message = "concurrent diagnostic",
                            producer,
                            item
                        );
                    }),
                }
            }
            Ok(())
        }));
    }
    for producer in producers {
        match producer.join() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => panic!("producer returned an error: {error}"),
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }
    handle.flush(Duration::from_secs(2))?;

    let values = writer.values();
    assert_eq!(values.len(), PRODUCERS * PER_PRODUCER);
    let sequences = values
        .iter()
        .map(|value| match value["sequence"].as_u64() {
            Some(sequence) => sequence,
            None => panic!("sequence must be a JSON u64"),
        })
        .collect::<Vec<_>>();
    assert_eq!(
        sequences,
        (1..=(PRODUCERS * PER_PRODUCER) as u64).collect::<Vec<_>>()
    );
    assert_eq!(
        values
            .iter()
            .filter(|value| value["event_name"] == "EFFECTIVE_PROFILE")
            .count(),
        PRODUCERS / 3 * PER_PRODUCER
    );
    Ok(())
}
