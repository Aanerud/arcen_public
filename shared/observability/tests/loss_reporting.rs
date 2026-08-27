#![allow(clippy::expect_used)]

use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use arcen_observability::{
    BoundedSink, DeliveryOutcome, ObservabilityBuilder, RuntimeError, Sink, SinkError,
    SinkLossClass, WaitError,
};
use arcen_telemetry::{
    CanonicalRecord, EventSeverity, OperationalProfile, TelemetryComponent, TelemetryPlatform,
    TelemetryRole, TelemetryTarget,
};

#[derive(Debug, Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn lines(&self) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().expect("writer lock").clone();
        String::from_utf8(bytes)
            .expect("UTF-8")
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

#[derive(Debug)]
struct FailingFileWriter;

impl Write for FailingFileWriter {
    fn write(&mut self, _bytes: &[u8]) -> io::Result<usize> {
        Err(io::Error::other("expected file failure"))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default)]
struct FailingNativeSink {
    deliveries: Arc<AtomicU64>,
    fail_flush: Arc<AtomicBool>,
}

impl Sink<CanonicalRecord> for FailingNativeSink {
    fn deliver(&mut self, _item: CanonicalRecord) -> Result<(), SinkError> {
        self.deliveries.fetch_add(1, Ordering::Relaxed);
        Err(SinkError::adapter("expected native failure"))
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        if self.fail_flush.load(Ordering::Acquire) {
            Err(SinkError::adapter("expected native flush failure"))
        } else {
            Ok(())
        }
    }
}

fn component() -> TelemetryComponent {
    TelemetryComponent::new("pier").expect("component")
}

fn record(sequence: u64) -> CanonicalRecord {
    CanonicalRecord::new(
        "2026-07-24T16:00:00.000000Z",
        sequence,
        OperationalProfile::Critical,
        EventSeverity::Info,
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Linux,
        TelemetryTarget::new("arcen::telemetry").expect("target"),
        "source record",
    )
    .expect("record")
}

fn wait_for_delivered(handle: &arcen_observability::ObservabilityHandle, sink: &str, count: u64) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while handle
        .sink_stats()
        .iter()
        .find(|stats| stats.name == sink)
        .is_none_or(|stats| stats.delivered < count)
    {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}

#[test]
fn complete_deltas_cover_file_native_delivery_and_post_flush_failures() {
    let healthy = SharedWriter::default();
    let native = FailingNativeSink::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Linux,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_writer("healthy-json", healthy.clone())
    .canonical_writer("failing-file", FailingFileWriter)
    .register_sink("failing-native", native.clone())
    .build()
    .expect("runtime");
    let handle = runtime.handle();

    assert_eq!(handle.emit_record(&record(1)).expect("emit").enqueued, 3);
    handle.flush(Duration::from_secs(1)).expect("flush");
    let first = handle.drain_loss_deltas();
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].sink(), "failing-file");
    assert_eq!(first[0].class(), SinkLossClass::DeliveryFailure);
    assert_eq!(first[0].count(), 1);
    assert_eq!(first[1].sink(), "failing-native");
    assert_eq!(first[1].class(), SinkLossClass::DeliveryFailure);
    assert_eq!(first[1].count(), 1);
    assert!(handle.drain_loss_deltas().is_empty());

    for (index, delta) in first.iter().enumerate() {
        let report = handle
            .emit_loss_notice(delta, format!("2026-07-24T16:00:00.{:06}Z", index + 1))
            .expect("loss notice");
        assert_eq!(report.enqueued, 1);
        assert_eq!(report.dropped, 0);
    }
    handle.flush(Duration::from_secs(1)).expect("notice flush");
    assert!(handle.drain_loss_deltas().is_empty());
    assert_eq!(native.deliveries.load(Ordering::Relaxed), 1);
    let lines = healthy.lines();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1]["fields"]["sink"], "failing-file:delivery_failure");
    assert_eq!(
        lines[2]["fields"]["sink"],
        "failing-native:delivery_failure"
    );

    assert_eq!(handle.emit_record(&record(2)).expect("emit").enqueued, 3);
    handle.flush(Duration::from_secs(1)).expect("second flush");
    let subsequent = handle.drain_loss_deltas();
    assert_eq!(subsequent.len(), 2);
    assert!(subsequent.iter().all(|delta| delta.count() == 1));
    assert!(handle.drain_loss_deltas().is_empty());

    native.fail_flush.store(true, Ordering::Release);
    assert!(handle.flush(Duration::from_secs(1)).is_err());
    let post_flush = handle.drain_loss_deltas();
    assert_eq!(post_flush.len(), 1);
    assert_eq!(post_flush[0].sink(), "failing-native");
    assert_eq!(post_flush[0].class(), SinkLossClass::FlushFailure);
    assert_eq!(post_flush[0].count(), 1);
    native.fail_flush.store(false, Ordering::Release);
    handle
        .emit_loss_notice(&post_flush[0], "2026-07-24T16:00:00.000003Z")
        .expect("flush-loss notice");
    handle
        .flush(Duration::from_secs(1))
        .expect("recovered flush");
    assert!(handle.drain_loss_deltas().is_empty());
}

#[derive(Debug, Clone, Default)]
struct BlockingSink {
    state: Arc<(Mutex<(bool, bool)>, Condvar)>,
}

impl BlockingSink {
    fn wait_until_entered(&self) {
        let (lock, changed) = &*self.state;
        let state = lock.lock().expect("state lock");
        let (_state, timeout) = changed
            .wait_timeout_while(state, Duration::from_secs(1), |state| !state.0)
            .expect("wait");
        assert!(!timeout.timed_out());
    }

    fn release(&self) {
        let (lock, changed) = &*self.state;
        lock.lock().expect("state lock").1 = true;
        changed.notify_all();
    }
}

impl Sink<u64> for BlockingSink {
    fn deliver(&mut self, _item: u64) -> Result<(), SinkError> {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().map_err(|_| SinkError::adapter("poisoned"))?;
        state.0 = true;
        changed.notify_all();
        while !state.1 {
            state = changed
                .wait(state)
                .map_err(|_| SinkError::adapter("poisoned"))?;
        }
        Ok(())
    }
}

#[test]
fn queue_full_and_closed_have_independent_first_deltas() {
    let adapter = BlockingSink::default();
    let queue = BoundedSink::new("bounded", 1, adapter.clone()).expect("queue");
    assert_eq!(queue.try_send(1), DeliveryOutcome::Enqueued);
    adapter.wait_until_entered();
    assert_eq!(queue.try_send(2), DeliveryOutcome::Enqueued);
    assert_eq!(queue.try_send(3), DeliveryOutcome::QueueFull);
    let full = queue.take_loss_deltas();
    assert_eq!(full.len(), 1);
    assert_eq!(full[0].class(), SinkLossClass::QueueFull);
    assert_eq!(full[0].count(), 1);
    assert!(queue.take_loss_deltas().is_empty());

    adapter.release();
    queue.flush(Duration::from_secs(1)).expect("flush");
    queue.shutdown(Duration::from_secs(1)).expect("shutdown");
    assert_eq!(queue.try_send(4), DeliveryOutcome::Closed);
    let closed = queue.take_loss_deltas();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].class(), SinkLossClass::QueueClosed);
    assert_eq!(closed[0].count(), 1);
    assert!(queue.take_loss_deltas().is_empty());
}

#[derive(Debug, Default)]
struct BlockingRecordState {
    entered: bool,
    release: bool,
    deliveries: usize,
}

#[derive(Debug, Clone, Default)]
struct BlockingRecordSink {
    state: Arc<(Mutex<BlockingRecordState>, Condvar)>,
}

impl BlockingRecordSink {
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

    fn deliveries(&self) -> usize {
        self.state.0.lock().expect("state lock").deliveries
    }
}

impl Sink<CanonicalRecord> for BlockingRecordSink {
    fn deliver(&mut self, _item: CanonicalRecord) -> Result<(), SinkError> {
        let (lock, changed) = &*self.state;
        let mut state = lock.lock().map_err(|_| SinkError::adapter("poisoned"))?;
        state.entered = true;
        changed.notify_all();
        while !state.release {
            state = changed
                .wait(state)
                .map_err(|_| SinkError::adapter("poisoned"))?;
        }
        state.deliveries += 1;
        Ok(())
    }
}

#[test]
fn loss_notice_excludes_healthy_origin_and_does_not_amplify_its_full_queue() {
    let healthy = SharedWriter::default();
    let origin = BlockingRecordSink::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Linux,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_queue_capacity(1)
    .canonical_writer("healthy-json", healthy.clone())
    .register_sink("origin-native", origin.clone())
    .build()
    .expect("runtime");
    let handle = runtime.handle();

    assert_eq!(handle.emit_record(&record(10)).expect("emit").enqueued, 2);
    origin.wait_until_entered();
    wait_for_delivered(&handle, "healthy-json", 1);
    assert_eq!(handle.emit_record(&record(11)).expect("emit").enqueued, 2);
    wait_for_delivered(&handle, "healthy-json", 2);
    let saturated = handle.emit_record(&record(12)).expect("emit");
    wait_for_delivered(&handle, "healthy-json", 3);
    assert_eq!(saturated.enqueued, 1);
    assert_eq!(saturated.dropped, 1);
    let deltas = handle.drain_loss_deltas();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].sink(), "origin-native");
    assert_eq!(deltas[0].class(), SinkLossClass::QueueFull);

    let report = handle
        .emit_loss_notice(&deltas[0], "2026-07-24T16:00:00.000010Z")
        .expect("loss notice");
    assert_eq!(report.enqueued, 1);
    assert_eq!(report.dropped, 0);
    origin.release();
    handle.flush(Duration::from_secs(1)).expect("flush");
    assert_eq!(origin.deliveries(), 2);
    assert!(handle.drain_loss_deltas().is_empty());
    assert_eq!(
        healthy
            .lines()
            .iter()
            .filter(|line| line["event_name"] == "TELEMETRY_DROPPED")
            .count(),
        1
    );
}

#[derive(Debug)]
struct AlwaysFail;

impl Sink<u64> for AlwaysFail {
    fn deliver(&mut self, _item: u64) -> Result<(), SinkError> {
        Err(SinkError::adapter("expected failure"))
    }
}

#[test]
fn concurrent_drains_account_for_every_failure_exactly_once() {
    const PRODUCERS: usize = 8;
    const PER_PRODUCER: usize = 200;
    let queue = BoundedSink::new("concurrent", 4096, AlwaysFail).expect("queue");
    let mut producers = Vec::new();
    for producer in 0..PRODUCERS {
        let queue = queue.clone();
        producers.push(thread::spawn(move || {
            for item in 0..PER_PRODUCER {
                assert_eq!(
                    queue.try_send((producer * PER_PRODUCER + item) as u64),
                    DeliveryOutcome::Enqueued
                );
            }
        }));
    }
    for producer in producers {
        producer.join().expect("producer");
    }
    queue.flush(Duration::from_secs(2)).expect("flush");

    let total = Arc::new(AtomicU64::new(0));
    let mut drainers = Vec::new();
    for _ in 0..8 {
        let queue = queue.clone();
        let total = Arc::clone(&total);
        drainers.push(thread::spawn(move || {
            let count = queue
                .take_loss_deltas()
                .into_iter()
                .filter(|delta| delta.class() == SinkLossClass::DeliveryFailure)
                .map(|delta| delta.count())
                .sum::<u64>();
            total.fetch_add(count, Ordering::Relaxed);
        }));
    }
    for drainer in drainers {
        drainer.join().expect("drainer");
    }
    assert_eq!(
        total.load(Ordering::Relaxed),
        (PRODUCERS * PER_PRODUCER) as u64
    );
    assert!(queue.take_loss_deltas().is_empty());
    queue.shutdown(Duration::from_secs(1)).expect("shutdown");
}

#[derive(Debug, Clone, Default)]
struct PanickingDeliverySink {
    calls: Arc<AtomicU64>,
}

impl Sink<CanonicalRecord> for PanickingDeliverySink {
    fn deliver(&mut self, _item: CanonicalRecord) -> Result<(), SinkError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        panic!("expected delivery panic");
    }
}

fn wait_for_worker_exit(handle: &arcen_observability::ObservabilityHandle, sink: &str) {
    let deadline = Instant::now() + Duration::from_secs(1);
    while handle
        .sink_stats()
        .iter()
        .find(|stats| stats.name == sink)
        .is_none_or(|stats| !stats.worker_finished)
    {
        assert!(Instant::now() < deadline);
        thread::yield_now();
    }
}

#[test]
fn delivery_panic_is_counted_once_and_dead_origin_is_never_routed_again() {
    let healthy = SharedWriter::default();
    let panicking = PanickingDeliverySink::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component(),
        TelemetryPlatform::Linux,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_writer("healthy-json", healthy.clone())
    .register_sink("panicking-native", panicking.clone())
    .build()
    .expect("runtime");
    let handle = runtime.handle();

    assert_eq!(handle.emit_record(&record(20)).expect("emit").enqueued, 2);
    wait_for_worker_exit(&handle, "panicking-native");
    let stats = handle
        .sink_stats()
        .into_iter()
        .find(|stats| stats.name == "panicking-native")
        .expect("panic sink stats");
    assert!(!stats.healthy);
    assert!(stats.worker_finished);
    assert_eq!(stats.failures, 1);

    let deltas = handle.drain_loss_deltas();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].sink(), "panicking-native");
    assert_eq!(deltas[0].class(), SinkLossClass::DeliveryFailure);
    assert_eq!(deltas[0].count(), 1);
    assert!(handle.drain_loss_deltas().is_empty());

    let notice = handle
        .emit_loss_notice(&deltas[0], "2026-07-24T16:00:00.000020Z")
        .expect("loss notice");
    assert_eq!(notice.enqueued, 1);
    assert_eq!(notice.dropped, 0);
    let later = handle.emit_record(&record(21)).expect("later emit");
    assert_eq!(later.enqueued, 1);
    assert_eq!(later.dropped, 0);
    assert_eq!(panicking.calls.load(Ordering::Relaxed), 1);

    let flush = handle
        .flush(Duration::from_secs(1))
        .expect_err("dead worker must fail flush");
    assert!(flush.failures().iter().any(|failure| {
        failure.sink == "panicking-native" && failure.error == WaitError::WorkerPanicked
    }));
    assert!(handle.drain_loss_deltas().is_empty());
    assert_eq!(
        healthy
            .lines()
            .iter()
            .filter(|line| line["event_name"] == "TELEMETRY_DROPPED")
            .count(),
        1
    );

    let started = Instant::now();
    assert!(matches!(
        runtime.guard().shutdown(Duration::from_millis(50)),
        Err(RuntimeError::SinkWait {
            error: WaitError::WorkerPanicked,
            ..
        })
    ));
    assert!(started.elapsed() < Duration::from_millis(200));
}

#[derive(Debug, Default)]
struct PanickingFlushSink;

impl Sink<u64> for PanickingFlushSink {
    fn deliver(&mut self, _item: u64) -> Result<(), SinkError> {
        Ok(())
    }

    fn flush(&mut self) -> Result<(), SinkError> {
        panic!("expected flush panic");
    }
}

#[test]
fn flush_panic_finishes_worker_and_reports_bounded_panicked_waits() {
    let queue = BoundedSink::new("panicking-flush", 2, PanickingFlushSink).expect("queue");
    assert_eq!(queue.try_send(1), DeliveryOutcome::Enqueued);
    assert_eq!(
        queue.flush(Duration::from_secs(1)),
        Err(WaitError::WorkerPanicked)
    );

    let stats = queue.stats();
    assert_eq!(stats.delivered, 1);
    assert_eq!(stats.failures, 1);
    assert!(!stats.healthy);

    // Wait for the worker to finish rather than assuming it already has.
    //
    // `flush` reports `WorkerPanicked` as soon as it sees the panic, but
    // `worker_finished` is set by the worker guard's `Drop`, which runs while
    // that thread is still unwinding. The two are not ordered, so asserting the
    // flag immediately passed on an idle machine and failed intermittently on a
    // loaded CI runner. `shutdown` polls the same flag for the same reason.
    let deadline = Instant::now() + Duration::from_secs(5);
    while queue.is_alive() && Instant::now() < deadline {
        std::thread::yield_now();
    }
    assert!(
        queue.stats().worker_finished,
        "worker did not finish within 5s of the flush panic"
    );
    assert!(!queue.is_alive());
    let deltas = queue.take_loss_deltas();
    assert_eq!(deltas.len(), 1);
    assert_eq!(deltas[0].class(), SinkLossClass::FlushFailure);
    assert_eq!(deltas[0].count(), 1);
    assert!(queue.take_loss_deltas().is_empty());
    assert_eq!(
        queue.flush(Duration::from_millis(10)),
        Err(WaitError::WorkerPanicked)
    );
    assert!(queue.take_loss_deltas().is_empty());

    let started = Instant::now();
    assert_eq!(
        queue.shutdown(Duration::from_millis(50)),
        Err(WaitError::WorkerPanicked)
    );
    assert!(started.elapsed() < Duration::from_millis(200));
    assert!(queue.take_loss_deltas().is_empty());
}
