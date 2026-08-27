//! OS-independent observability runtime for Arcen's canonical telemetry contracts.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod runtime;
mod sampler;
mod sink;

pub use runtime::{
    BuildError, EffectiveDiagnosticPolicy, EmissionReport, FlushError, GlobalInstallError,
    InstalledObservability, LifecycleContext, ObservabilityBuilder, ObservabilityHandle,
    ObservabilityRuntime, RuntimeError, ShutdownGuard, SinkFlushFailure,
};
pub use sampler::{
    CallerQosSnapshot, HeartbeatCounters, HeartbeatSnapshot, QosCounters, QosSampler,
};
pub use sink::{
    BoundedSink, DeliveryOutcome, Sink, SinkBuildError, SinkError, SinkLossClass, SinkLossDelta,
    SinkStats, WaitError, WriterRecordSink, WriterTextSink,
};

/// Default capacity for a canonical record sink.
pub const DEFAULT_CANONICAL_QUEUE_CAPACITY: usize = 1024;
/// Default capacity for an interactive human console sink.
pub const DEFAULT_CONSOLE_QUEUE_CAPACITY: usize = 256;
