//! Non-blocking canonical telemetry for the Wayland capture path.
//!
//! Records are built from shared vocabulary ([`super::evidence`]) and handed
//! to a bounded `arcen-observability` sink whose worker writes canonical JSON
//! lines to stderr. Emission happens on the import worker after negotiation,
//! never from a `PipeWire` or GPU callback, and a full sink drops rather than
//! blocks.

use std::time::Duration;

use arcen_observability::{
    canonical_timestamp_now, LifecycleContext, ObservabilityBuilder, ObservabilityRuntime,
};
use arcen_telemetry::{
    names, CorrelationId, OperationalProfile, TelemetryComponent, TelemetryPlatform, TelemetryRole,
    TelemetryTarget, ValidatedLifecycleEvent,
};

/// The capture path's telemetry runtime.
pub struct CaptureTelemetry {
    runtime: ObservabilityRuntime,
    sid: CorrelationId,
}

impl CaptureTelemetry {
    /// A runtime writing canonical records to stderr at `profile`.
    ///
    /// # Errors
    ///
    /// A description when the runtime cannot be built.
    pub fn to_stderr(sid: CorrelationId, profile: OperationalProfile) -> Result<Self, String> {
        let component = TelemetryComponent::new("capenc").map_err(|error| error.to_string())?;
        let runtime = ObservabilityBuilder::new(
            TelemetryRole::Host,
            component,
            TelemetryPlatform::Linux,
            profile,
        )
        .canonical_writer("capenc-stderr", std::io::stderr())
        .arcen_log(None::<String>)
        .build()
        .map_err(|error| error.to_string())?;
        Ok(Self { runtime, sid })
    }

    /// The correlation id records carry.
    #[must_use]
    pub fn sid(&self) -> &CorrelationId {
        &self.sid
    }

    /// Emits one validated lifecycle event without blocking.
    ///
    /// # Errors
    ///
    /// A description when the record is refused or the clock is unusable.
    pub fn emit(&self, event: &ValidatedLifecycleEvent, message: &str) -> Result<(), String> {
        let timestamp =
            canonical_timestamp_now().ok_or_else(|| "canonical clock unavailable".to_string())?;
        let target =
            TelemetryTarget::new(names::target::MEDIA).map_err(|error| error.to_string())?;
        self.runtime
            .handle()
            .emit_lifecycle(
                event,
                LifecycleContext {
                    sid: self.sid.clone(),
                    user: None,
                    host: None,
                    peer_addr: None,
                    health_state: None,
                },
                timestamp,
                target,
                message,
            )
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    /// Waits briefly for queued records to be written.
    pub fn flush(&self) {
        let _ = self.runtime.handle().flush(Duration::from_millis(500));
    }
}
