#![allow(clippy::expect_used)]

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arcen_observability::ObservabilityBuilder;
use arcen_telemetry::{
    CanonicalRecord, EventSeverity, OperationalProfile, TelemetryComponent, TelemetryPlatform,
    TelemetryRole, TelemetryTarget,
};

#[derive(Debug, Clone, Default)]
struct SharedWriter(Arc<Mutex<Vec<u8>>>);

impl SharedWriter {
    fn line_count(&self) -> usize {
        self.0
            .lock()
            .expect("writer lock")
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .count()
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

#[test]
fn public_global_install_keeps_sinks_for_process_lifetime() {
    let writer = SharedWriter::default();
    let runtime = ObservabilityBuilder::new(
        TelemetryRole::Host,
        TelemetryComponent::new("pier").expect("component"),
        TelemetryPlatform::Linux,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_writer("json", writer.clone())
    .build()
    .expect("runtime");
    let installed = runtime.install_global().expect("global install");
    let handle = installed.handle();
    let first = CanonicalRecord::new(
        "2026-07-24T16:00:00.000000Z",
        1,
        OperationalProfile::Critical,
        EventSeverity::Info,
        TelemetryRole::Host,
        TelemetryComponent::new("pier").expect("component"),
        TelemetryPlatform::Linux,
        TelemetryTarget::new("arcen::telemetry").expect("target"),
        "still alive",
    )
    .expect("record");
    assert_eq!(handle.emit_record(&first).expect("emit").enqueued, 1);
    installed
        .flush(Duration::from_secs(1))
        .expect("global flush");
    assert_eq!(writer.line_count(), 1);
    assert_eq!(handle.sink_stats()[0].delivered, 1);

    let second = CanonicalRecord::new(
        "2026-07-24T16:00:00.000001Z",
        2,
        OperationalProfile::Critical,
        EventSeverity::Info,
        TelemetryRole::Host,
        TelemetryComponent::new("pier").expect("component"),
        TelemetryPlatform::Linux,
        TelemetryTarget::new("arcen::telemetry").expect("target"),
        "after flush",
    )
    .expect("record");
    assert_eq!(handle.emit_record(&second).expect("emit").enqueued, 1);
    installed
        .flush(Duration::from_secs(1))
        .expect("repeat global flush");
    assert_eq!(writer.line_count(), 2);

    drop(installed);
    drop(handle.clone());
    let third = CanonicalRecord::new(
        "2026-07-24T16:00:00.000002Z",
        3,
        OperationalProfile::Critical,
        EventSeverity::Info,
        TelemetryRole::Host,
        TelemetryComponent::new("pier").expect("component"),
        TelemetryPlatform::Linux,
        TelemetryTarget::new("arcen::telemetry").expect("target"),
        "after installed owner drop",
    )
    .expect("record");
    assert_eq!(handle.emit_record(&third).expect("emit").enqueued, 1);
    handle
        .flush(Duration::from_secs(1))
        .expect("handle flush after owner drop");
    assert_eq!(writer.line_count(), 3);
    assert_eq!(handle.sink_stats()[0].delivered, 3);

    let rejected_writer = SharedWriter::default();
    let rejected = ObservabilityBuilder::new(
        TelemetryRole::Host,
        TelemetryComponent::new("pier").expect("component"),
        TelemetryPlatform::Linux,
        OperationalProfile::Critical,
    )
    .arcen_log(None::<String>)
    .canonical_writer("rejected-json", rejected_writer.clone())
    .build()
    .expect("second runtime");
    let recovered = rejected
        .install_global()
        .expect_err("second global install must fail")
        .into_runtime();
    assert_eq!(
        recovered
            .handle()
            .emit_record(&first)
            .expect("recovered emit")
            .enqueued,
        1
    );
    recovered
        .guard()
        .flush(Duration::from_secs(1))
        .expect("recovered local flush");
    assert_eq!(rejected_writer.line_count(), 1);
}
