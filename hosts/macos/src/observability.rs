//! Structured host telemetry for the macOS Pier.
//!
//! The Linux Pier emits canonical lifecycle records — a schema-validated event
//! vocabulary with a correlation id, a severity, a role, and typed fields —
//! and that is what makes a claim like "latency is fine" checkable instead of
//! asserted. This module gives the macOS Pier the same vocabulary, built from
//! the same shared crates, so a session on either host produces records that
//! can be read the same way.
//!
//! Nothing here invents an event. Every kind emitted is one of the append-only
//! definitions in [`arcen_telemetry::LifecycleEventKind`], and every field is
//! one that kind declares; an event with a field the schema does not declare
//! is refused by the shared validator rather than written. That refusal is the
//! point: a log whose shape drifts per host cannot be compared across hosts,
//! and comparing hosts is why this exists.
//!
//! Delivery is best effort by design. A video thread must never block on a
//! sink, and a host that stops streaming because a log file is slow has made
//! observability into an outage. Emission failures are counted and reported
//! once rather than repeatedly, so a broken sink is visible without becoming a
//! second flood.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use arcen_observability::{
    LifecycleContext, ObservabilityHandle, QosCounters, canonical_timestamp_now,
};
use arcen_telemetry::{
    CorrelationId, LifecycleEventKind, StructuredFields, TelemetryTarget, ValidatedLifecycleEvent,
};

/// What the host has done, as counters a sampler can difference.
///
/// Shared rather than local: [`QosCounters`] is the same type the other hosts
/// feed, so the numbers behind `fps_actual` are computed the same way and a
/// difference between hosts is a real difference rather than a different
/// definition of "frame".
#[derive(Debug, Default)]
pub struct SessionCounters {
    /// Frames and input, in the shared shape.
    pub qos: QosCounters,
    /// Pen samples injected, which the shared counters have no slot for.
    pub pen_samples: AtomicU64,
}

/// Returns random bytes for a session correlation id.
///
/// A correlation id has to be unique per session and must not be guessable
/// from another session's, so it comes from the system generator rather than a
/// counter: a predictable id in a shared log lets one session's records be
/// mistaken for another's.
#[must_use]
pub fn random_correlation_bytes() -> [u8; 16] {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // A failed system generator must not produce a fixed id that silently
        // collides across sessions. Deriving from the clock keeps records
        // separable even in that case, and is the documented weaker path.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |value| value.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes());
    }
    bytes
}

/// Who a record is about.
///
/// Grouped rather than passed as loose arguments so that a call site cannot
/// silently swap the user and the peer address, which are both `Option<String>`
/// and would compile either way round.
#[derive(Debug, Clone)]
pub struct SessionScope {
    /// Correlation id tying one session's records together.
    pub sid: CorrelationId,
    /// Authenticated user, when there is one.
    pub user: Option<String>,
    /// Peer address, when there is one.
    pub peer: Option<String>,
}

impl SessionScope {
    /// Returns a scope with no identity, for service-level records.
    #[must_use]
    pub const fn service(sid: CorrelationId) -> Self {
        Self {
            sid,
            user: None,
            peer: None,
        }
    }
}

/// Emits canonical lifecycle records for one host process.
#[derive(Clone)]
pub struct HostTelemetry {
    handle: Option<ObservabilityHandle>,
    host: Option<String>,
    failure_reported: Arc<AtomicBool>,
}

impl std::fmt::Debug for HostTelemetry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HostTelemetry")
            .field("enabled", &self.handle.is_some())
            .field("host", &self.host)
            .finish_non_exhaustive()
    }
}

impl HostTelemetry {
    /// Returns an emitter that writes through `handle`.
    #[must_use]
    pub fn new(handle: ObservabilityHandle, host: Option<String>) -> Self {
        Self {
            handle: Some(handle),
            host,
            failure_reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns an emitter that writes nothing.
    ///
    /// Used before the runtime is installed and by probes, so call sites never
    /// have to ask whether telemetry exists.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            handle: None,
            host: None,
            failure_reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Returns whether records are actually delivered.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.handle.is_some()
    }

    /// Emits a session-scoped event, ignoring delivery failure after the first.
    pub fn emit(
        &self,
        kind: LifecycleEventKind,
        scope: &SessionScope,
        fields: StructuredFields,
        target: &str,
        message: &str,
    ) {
        let sid = &scope.sid;
        let user = scope.user.clone();
        let peer = scope.peer.clone();
        let Some(handle) = self.handle.as_ref() else {
            return;
        };
        let Ok(event) = ValidatedLifecycleEvent::new(kind, sid.clone(), fields) else {
            // A field the schema does not declare is a programming error here,
            // not a runtime condition, and dropping the record is better than
            // writing one whose shape no reader can rely on.
            self.report_failure_once();
            return;
        };
        let context = LifecycleContext {
            sid: sid.clone(),
            user,
            host: self.host.clone(),
            peer_addr: peer,
            health_state: None,
        };
        // A record with an invented time is worse than a missing one, so a
        // clock the canonical format cannot express means no record.
        let Ok(target) = TelemetryTarget::new(target) else {
            self.report_failure_once();
            return;
        };
        let Some(timestamp) = canonical_timestamp_now() else {
            self.report_failure_once();
            return;
        };
        if handle
            .emit_lifecycle(&event, context, timestamp, target, message.to_owned())
            .is_err()
        {
            self.report_failure_once();
        }
    }

    fn report_failure_once(&self) {
        if !self.failure_reported.swap(true, Ordering::Relaxed) {
            tracing::warn!(
                target: arcen_telemetry::names::target::TELEMETRY,
                "structured telemetry delivery failed; further failures are not reported",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_disabled_emitter_accepts_events_and_writes_nothing() {
        let telemetry = HostTelemetry::disabled();
        assert!(!telemetry.is_enabled());
        telemetry.emit(
            LifecycleEventKind::HealthSnapshot,
            &SessionScope::service(CorrelationId::from_uuid_v4_bytes([7; 16])),
            StructuredFields::default(),
            arcen_telemetry::names::target::HEALTH,
            "health snapshot",
        );
    }
}
