//! Lane lifecycle events (`LANE_REFUSED`, `LANE_FAILED`, `LANE_STARVED`)
//! and the bounded, non-blocking path that delivers them.

use std::fmt::{Debug, Formatter};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use arcen_observability::{LifecycleContext, ObservabilityHandle, canonical_timestamp_now};
use arcen_telemetry::{
    LifecycleEventKind, StructuredFields, TelemetryTarget, ValidatedLifecycleEvent,
    lifecycle_fields, names,
};

use super::{LANE_TELEMETRY_BURST, LANE_TELEMETRY_PER_SECOND};

/// Receives validated-shape lane lifecycle events.
///
/// Called synchronously from lane tasks and the accept dispatcher, so an
/// implementation must not block: hand the event to a bounded queue and
/// return. [`ObservabilityLaneSink`] does exactly that through
/// `arcen-observability`.
pub trait LaneTelemetrySink: Send + Sync + 'static {
    /// Records one lifecycle event of `kind` with schema-shaped `fields`.
    fn record(&self, kind: LifecycleEventKind, fields: StructuredFields);
}

/// Emits lane events through the process's `arcen-observability` runtime
/// with the session's lifecycle context. Delivery uses the runtime's
/// bounded sinks (`try_send`), so a full sink drops and counts the record
/// instead of blocking a lane.
pub struct ObservabilityLaneSink {
    handle: ObservabilityHandle,
    context: std::sync::RwLock<LifecycleContext>,
}

impl Debug for ObservabilityLaneSink {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservabilityLaneSink")
            .field("sid", &self.context().sid)
            .finish_non_exhaustive()
    }
}

impl ObservabilityLaneSink {
    /// A sink bound to one session's lifecycle context.
    #[must_use]
    pub const fn new(handle: ObservabilityHandle, context: LifecycleContext) -> Self {
        Self {
            handle,
            context: std::sync::RwLock::new(context),
        }
    }

    /// Moves later events to `context`: a Pier starts the lane runtime
    /// before authentication, under a connection-scoped context, and binds
    /// the session's own once it is known.
    pub fn bind(&self, context: LifecycleContext) {
        *self
            .context
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = context;
    }

    fn context(&self) -> LifecycleContext {
        self.context
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

impl LaneTelemetrySink for ObservabilityLaneSink {
    fn record(&self, kind: LifecycleEventKind, fields: StructuredFields) {
        let context = self.context();
        let Ok(event) = ValidatedLifecycleEvent::new(kind, context.sid.clone(), fields) else {
            return;
        };
        let Some(timestamp) = canonical_timestamp_now() else {
            return;
        };
        let Ok(target) = TelemetryTarget::new(names::target::TRANSPORT) else {
            return;
        };
        let _report =
            self.handle
                .emit_lifecycle(&event, context, timestamp, target, event.definition().name);
    }
}

/// Token bucket in front of a sink: a hostile peer that opens refused
/// streams in a loop cannot turn the lane runtime into a log flood.
pub(super) struct LaneTelemetry {
    sink: Option<Arc<dyn LaneTelemetrySink>>,
    bucket: Mutex<Bucket>,
    suppressed: AtomicU64,
}

struct Bucket {
    tokens: u32,
    refilled_at: Instant,
}

impl LaneTelemetry {
    pub(super) fn new(sink: Option<Arc<dyn LaneTelemetrySink>>) -> Self {
        Self {
            sink,
            bucket: Mutex::new(Bucket {
                tokens: LANE_TELEMETRY_BURST,
                refilled_at: Instant::now(),
            }),
            suppressed: AtomicU64::new(0),
        }
    }

    pub(super) fn suppressed(&self) -> u64 {
        self.suppressed.load(Ordering::Relaxed)
    }

    fn admit(&self) -> bool {
        let Ok(mut bucket) = self.bucket.lock() else {
            return false;
        };
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(bucket.refilled_at);
        let refill = elapsed.as_millis() * u128::from(LANE_TELEMETRY_PER_SECOND) / 1_000;
        if refill > 0 {
            let refill = u32::try_from(refill).unwrap_or(u32::MAX);
            bucket.tokens = bucket
                .tokens
                .saturating_add(refill)
                .min(LANE_TELEMETRY_BURST);
            bucket.refilled_at = now;
        }
        if bucket.tokens == 0 {
            return false;
        }
        bucket.tokens -= 1;
        true
    }

    fn emit(&self, kind: LifecycleEventKind, fields: Option<StructuredFields>) {
        let Some(sink) = self.sink.as_ref() else {
            return;
        };
        let Some(fields) = fields else {
            return;
        };
        if self.admit() {
            sink.record(kind, fields);
        } else {
            self.suppressed.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(super) fn refused(&self, role: &str, reason: &str, lane_generation: Option<u32>) {
        self.emit(
            LifecycleEventKind::LaneRefused,
            lifecycle_fields::lane_refused(role, reason, lane_generation),
        );
    }

    pub(super) fn failed(
        &self,
        role: &str,
        failure: &str,
        recovery: &str,
        lane_generation: u32,
        reset_stream: bool,
    ) {
        self.emit(
            LifecycleEventKind::LaneFailed,
            lifecycle_fields::lane_failed(
                role,
                failure,
                recovery,
                Some(lane_generation),
                Some(reset_stream),
            ),
        );
    }

    pub(super) fn starved(
        &self,
        role: &str,
        waited: Duration,
        queued_records: u64,
        queued_bytes: u64,
        cause: &str,
    ) {
        self.emit(
            LifecycleEventKind::LaneStarved,
            lifecycle_fields::lane_starved(
                role,
                u64::try_from(waited.as_millis()).unwrap_or(u64::MAX),
                Some(queued_records),
                Some(queued_bytes),
                Some(cause),
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Counting(Mutex<Vec<LifecycleEventKind>>);

    impl LaneTelemetrySink for Counting {
        fn record(&self, kind: LifecycleEventKind, _fields: StructuredFields) {
            if let Ok(mut events) = self.0.lock() {
                events.push(kind);
            }
        }
    }

    #[test]
    fn a_refusal_flood_is_rate_limited_and_counted() {
        let sink = Arc::new(Counting::default());
        let telemetry = LaneTelemetry::new(Some(sink.clone()));
        for _ in 0..1_000 {
            telemetry.refused("unknown", "bad_magic", None);
        }
        let delivered = sink.0.lock().map_or(0, |events| events.len());
        assert!(delivered >= usize::try_from(LANE_TELEMETRY_BURST).unwrap_or(0));
        assert!(delivered < 64, "{delivered}");
        assert_eq!(
            u64::try_from(delivered).unwrap_or(0) + telemetry.suppressed(),
            1_000
        );
    }
}
