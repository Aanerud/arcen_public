// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! OS-independent observability runtime for Arcen's canonical telemetry contracts.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

mod runtime;
mod sampler;
pub mod session_health;
mod sink;
#[cfg(feature = "support-bundle")]
pub mod support_bundle;

pub use runtime::{
    BuildError, EffectiveDiagnosticPolicy, EmissionReport, FlushError, GlobalInstallError,
    InstalledObservability, LifecycleContext, ObservabilityBuilder, ObservabilityHandle,
    ObservabilityRuntime, RuntimeError, ShutdownGuard, SinkFlushFailure, canonical_timestamp_now,
};
pub use sampler::{
    CallerQosSnapshot, HeartbeatCounters, HeartbeatSnapshot, QosCounters, QosSampler,
};
pub use session_health::monitor_rates::{
    MONITOR_LIMITED_HOST_SHORTFALL, MonitorCounters, MonitorRateChange, MonitorRates,
    log_monitor_rate_change,
};
pub use session_health::{
    ContentEvidence, HealthObservation, HostCounters, SessionHealth, health_state_message,
    health_stats_with_path, log_governed_frame_rate,
};
pub use sink::{
    BoundedSink, DeliveryOutcome, Sink, SinkBuildError, SinkError, SinkLossClass, SinkLossDelta,
    SinkStats, WaitError, WriterRecordSink, WriterTextSink,
};

/// Default capacity for a canonical record sink.
pub const DEFAULT_CANONICAL_QUEUE_CAPACITY: usize = 1024;
/// Default capacity for an interactive human console sink.
pub const DEFAULT_CONSOLE_QUEUE_CAPACITY: usize = 256;
