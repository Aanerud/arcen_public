#![allow(clippy::expect_used)]

use std::io::{self, Write};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use arcen_observability::{ObservabilityBuilder, WaitError};
use arcen_telemetry::{
    CanonicalRecord, EventSeverity, OperationalProfile, TelemetryComponent, TelemetryPlatform,
    TelemetryRole, TelemetryTarget,
};

#[derive(Debug, Default)]
struct BlockingState {
    entered: bool,
    release: bool,
    writes: usize,
}

#[derive(Debug, Clone, Default)]
struct BlockingWriter {
    state: Arc<(Mutex<BlockingState>, Condvar)>,
}

impl BlockingWriter {
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

impl Write for BlockingWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let (lock, changed) = &*self.state;
        let mut state = lock
            .lock()
            .map_err(|_| io::Error::other("state lock poisoned"))?;
        state.entered = true;
        changed.notify_all();
        while !state.release {
            state = changed
                .wait(state)
                .map_err(|_| io::Error::other("state lock poisoned"))?;
        }
        state.writes += 1;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn record(sequence: u64, timestamp: &str) -> CanonicalRecord {
    CanonicalRecord::new(
        timestamp,
        sequence,
        OperationalProfile::Critical,
        EventSeverity::Info,
        TelemetryRole::Host,
        TelemetryComponent::new("pier").expect("component"),
        TelemetryPlatform::Linux,
        TelemetryTarget::new("arcen::telemetry").expect("target"),
        "bounded global flush",
    )
    .expect("record")
}

#[test]
fn global_flush_is_bounded_and_does_not_close_the_sink() {
    let writer = BlockingWriter::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        TelemetryComponent::new("pier").expect("component"),
        TelemetryPlatform::Linux,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_queue_capacity(1)
    .canonical_writer("blocking-json", writer.clone())
    .build()
    .expect("runtime");
    let installed = runtime.install_global().expect("global install");
    let handle = installed.handle();
    assert_eq!(
        handle
            .emit_record(&record(1, "2026-07-24T16:00:00.000000Z"))
            .expect("emit")
            .enqueued,
        1
    );
    writer.wait_until_entered();

    let started = Instant::now();
    let error = installed
        .flush(Duration::from_millis(5))
        .expect_err("blocked writer must time out");
    assert!(started.elapsed() < Duration::from_millis(50));
    assert_eq!(error.failures().len(), 1);
    assert_eq!(error.failures()[0].sink, "blocking-json");
    assert_eq!(error.failures()[0].error, WaitError::Timeout);

    writer.release();
    installed
        .flush(Duration::from_secs(1))
        .expect("flush after release");
    assert_eq!(
        handle
            .emit_record(&record(2, "2026-07-24T16:00:00.000001Z"))
            .expect("emit after flush")
            .enqueued,
        1
    );
    installed
        .flush(Duration::from_secs(1))
        .expect("repeat flush");
    assert_eq!(handle.sink_stats()[0].delivered, 2);
}
