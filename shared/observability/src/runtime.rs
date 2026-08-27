//! Builder, profile reload, tracing bridge, and canonical lifecycle routing.

use std::error::Error;
use std::fmt::{Display, Formatter};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arcen_telemetry::{
    CanonicalRecord, CorrelationId, EventSeverity, FieldValue, HealthState, LevelSpec,
    LifecycleEventKind, OperationalProfile, SchemaValidationError, StructuredFields,
    TelemetryComponent, TelemetryPlatform, TelemetryRole, TelemetryTarget, ValidatedLifecycleEvent,
};
use tracing::{Dispatch, Event, Subscriber};
use tracing_subscriber::filter::{EnvFilter, ParseError};
use tracing_subscriber::layer::{Context, Layer, SubscriberExt};
use tracing_subscriber::registry::{LookupSpan, Registry};
use tracing_subscriber::reload;

use crate::sink::{
    BoundedSink, DeliveryOutcome, Sink, SinkBuildError, SinkLossDelta, SinkStats, WaitError,
    WriterRecordSink, WriterTextSink,
};
use crate::{DEFAULT_CANONICAL_QUEUE_CAPACITY, DEFAULT_CONSOLE_QUEUE_CAPACITY};

const DROP_WAIT: Duration = Duration::from_millis(100);

type FilterReloadHandle = reload::Handle<EnvFilter, Registry>;

/// Explicit top-level context for a validated lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleContext {
    /// Session/correlation identity. It must match the validated event.
    pub sid: CorrelationId,
    /// Schema-approved authenticated user.
    pub user: Option<String>,
    /// Schema-approved local host.
    pub host: Option<String>,
    /// Schema-approved peer address.
    pub peer_addr: Option<String>,
    /// Optional common health state.
    pub health_state: Option<HealthState>,
}

/// One coherent snapshot of the effective diagnostic filter policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveDiagnosticPolicy {
    /// Cumulative operational profile used to build the base filter.
    pub profile: OperationalProfile,
    /// Optional `ARCEN_LOG` refinement applied to that same filter generation.
    pub arcen_log: Option<String>,
}

struct PendingRecordSink {
    name: String,
    capacity: usize,
    canonical_writer: bool,
    sink: Box<dyn Sink<CanonicalRecord>>,
}

struct PendingTextSink {
    name: String,
    capacity: usize,
    sink: Box<dyn Sink<String>>,
}

/// Configures an observability runtime without global tracing side effects.
pub struct ObservabilityBuilder {
    role: TelemetryRole,
    component: TelemetryComponent,
    platform: TelemetryPlatform,
    profile: OperationalProfile,
    canonical_capacity: usize,
    console_capacity: usize,
    record_sinks: Vec<PendingRecordSink>,
    text_sinks: Vec<PendingTextSink>,
    arcen_log: Option<String>,
    read_arcen_log: bool,
}

impl std::fmt::Debug for ObservabilityBuilder {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservabilityBuilder")
            .field("role", &self.role)
            .field("component", &self.component)
            .field("platform", &self.platform)
            .field("profile", &self.profile)
            .field("canonical_capacity", &self.canonical_capacity)
            .field("console_capacity", &self.console_capacity)
            .field("record_sink_count", &self.record_sinks.len())
            .field("text_sink_count", &self.text_sinks.len())
            .field("read_arcen_log", &self.read_arcen_log)
            .finish_non_exhaustive()
    }
}

impl ObservabilityBuilder {
    /// Creates a builder with all process identity values and profile required.
    #[must_use]
    pub fn new(
        role: TelemetryRole,
        component: TelemetryComponent,
        platform: TelemetryPlatform,
        profile: OperationalProfile,
    ) -> Self {
        Self {
            role,
            component,
            platform,
            profile,
            canonical_capacity: DEFAULT_CANONICAL_QUEUE_CAPACITY,
            console_capacity: DEFAULT_CONSOLE_QUEUE_CAPACITY,
            record_sinks: Vec::new(),
            text_sinks: Vec::new(),
            arcen_log: None,
            read_arcen_log: true,
        }
    }

    /// Sets the default capacity used by subsequently registered canonical sinks.
    #[must_use]
    pub const fn canonical_queue_capacity(mut self, capacity: usize) -> Self {
        self.canonical_capacity = capacity;
        self
    }

    /// Sets the default capacity used by subsequently registered console sinks.
    #[must_use]
    pub const fn console_queue_capacity(mut self, capacity: usize) -> Self {
        self.console_capacity = capacity;
        self
    }

    /// Registers the required canonical JSON Lines writer.
    #[must_use]
    pub fn canonical_writer(
        mut self,
        name: impl Into<String>,
        writer: impl Write + Send + 'static,
    ) -> Self {
        self.record_sinks.push(PendingRecordSink {
            name: name.into(),
            capacity: self.canonical_capacity,
            canonical_writer: true,
            sink: Box::new(WriterRecordSink::new(writer)),
        });
        self
    }

    /// Registers an app-owned file/native lifecycle adapter.
    #[must_use]
    pub fn register_sink(
        mut self,
        name: impl Into<String>,
        sink: impl Sink<CanonicalRecord>,
    ) -> Self {
        self.record_sinks.push(PendingRecordSink {
            name: name.into(),
            capacity: self.canonical_capacity,
            canonical_writer: false,
            sink: Box::new(sink),
        });
        self
    }

    /// Registers an optional bounded human console writer.
    #[must_use]
    pub fn human_console_writer(
        mut self,
        name: impl Into<String>,
        writer: impl Write + Send + 'static,
    ) -> Self {
        self.text_sinks.push(PendingTextSink {
            name: name.into(),
            capacity: self.console_capacity,
            sink: Box::new(WriterTextSink::new(writer)),
        });
        self
    }

    /// Supplies an explicit `ARCEN_LOG` refinement instead of consulting the environment.
    #[must_use]
    pub fn arcen_log(mut self, directive: Option<impl Into<String>>) -> Self {
        self.arcen_log = directive.map(Into::into);
        self.read_arcen_log = false;
        self
    }

    /// Starts sink workers and creates a local tracing dispatch.
    ///
    /// This method never installs a global subscriber. Call
    /// [`ObservabilityRuntime::install_global`] explicitly when process ownership
    /// makes global installation appropriate.
    ///
    /// # Errors
    ///
    /// Returns missing-sink, duplicate-name, environment, filter, or worker
    /// startup errors.
    pub fn build(self) -> Result<ObservabilityRuntime, BuildError> {
        if !self
            .record_sinks
            .iter()
            .any(|pending| pending.canonical_writer)
        {
            return Err(BuildError::MissingCanonicalSink);
        }
        let arcen_log = if self.read_arcen_log {
            match std::env::var("ARCEN_LOG") {
                Ok(value) => Some(value),
                Err(std::env::VarError::NotPresent) => None,
                Err(error) => return Err(BuildError::Environment(error)),
            }
        } else {
            self.arcen_log
        };
        let filter =
            build_filter(self.profile, arcen_log.as_deref()).map_err(BuildError::Filter)?;

        let mut names = Vec::new();
        let mut record_sinks = Vec::with_capacity(self.record_sinks.len());
        for pending in self.record_sinks {
            if names.contains(&pending.name) {
                return Err(BuildError::DuplicateSinkName(pending.name));
            }
            names.push(pending.name.clone());
            record_sinks.push(
                BoundedSink::new_boxed(&pending.name, pending.capacity, pending.sink)
                    .map_err(BuildError::Sink)?,
            );
        }
        let mut text_sinks = Vec::with_capacity(self.text_sinks.len());
        for pending in self.text_sinks {
            if names.contains(&pending.name) {
                return Err(BuildError::DuplicateSinkName(pending.name));
            }
            names.push(pending.name.clone());
            text_sinks.push(
                BoundedSink::new_boxed(&pending.name, pending.capacity, pending.sink)
                    .map_err(BuildError::Sink)?,
            );
        }

        let inner = Arc::new(RuntimeInner {
            role: self.role,
            component: self.component,
            platform: self.platform,
            sequence: Mutex::new(0),
            format_failures: AtomicU64::new(0),
            record_sinks,
            text_sinks,
            reload_state: Mutex::new(ReloadState {
                filter_reload: None,
                profile: self.profile,
                arcen_log,
            }),
        });
        let handle = ObservabilityHandle {
            inner: Arc::clone(&inner),
        };
        let diagnostic_layer = DiagnosticLayer {
            handle: handle.clone(),
        };
        let (filter_layer, filter_reload) = reload::Layer::new(filter);
        let subscriber = tracing_subscriber::registry()
            .with(filter_layer)
            .with(diagnostic_layer);
        inner
            .reload_state
            .lock()
            .map_err(|_| BuildError::Poisoned)?
            .filter_reload = Some(filter_reload);
        let dispatch = Dispatch::new(subscriber);
        let guard = ShutdownGuard { inner, armed: true };

        Ok(ObservabilityRuntime {
            handle,
            dispatch,
            guard,
        })
    }
}

/// Initialized local runtime, tracing dispatch, handle, and flush guard.
#[derive(Debug)]
pub struct ObservabilityRuntime {
    handle: ObservabilityHandle,
    dispatch: Dispatch,
    guard: ShutdownGuard,
}

impl ObservabilityRuntime {
    /// Returns a cloneable application control/event handle.
    #[must_use]
    pub fn handle(&self) -> ObservabilityHandle {
        self.handle.clone()
    }

    /// Returns the local tracing dispatch.
    #[must_use]
    pub fn dispatch(&self) -> Dispatch {
        self.dispatch.clone()
    }

    /// Runs a closure with this runtime's tracing dispatch as thread default.
    pub fn with_default<T>(&self, operation: impl FnOnce() -> T) -> T {
        tracing::dispatcher::with_default(&self.dispatch, operation)
    }

    /// Installs this runtime for the lifetime of the process.
    ///
    /// This consumes the local owner. After successful installation, dropping
    /// the returned owner or clones of its handle cannot shut down the sinks;
    /// the global dispatch retains the runtime until process exit. Applications
    /// needing controlled shutdown must keep this runtime local instead.
    ///
    /// # Errors
    ///
    /// Returns an error if another global dispatch was already installed.
    pub fn install_global(self) -> Result<InstalledObservability, GlobalInstallError> {
        self.install_process_lifetime(|dispatch| {
            tracing::dispatcher::set_global_default(dispatch).map_err(|error| error.to_string())
        })
    }

    fn install_process_lifetime(
        self,
        install: impl FnOnce(Dispatch) -> Result<(), String>,
    ) -> Result<InstalledObservability, GlobalInstallError> {
        if let Err(message) = install(self.dispatch.clone()) {
            return Err(GlobalInstallError {
                message,
                runtime: self,
            });
        }
        let Self {
            handle,
            dispatch: _,
            mut guard,
        } = self;
        guard.disarm();
        Ok(InstalledObservability { handle })
    }

    /// Returns the bounded flush/shutdown guard.
    #[must_use]
    pub const fn guard(&self) -> &ShutdownGuard {
        &self.guard
    }
}

/// Process-lifetime owner returned after successful global installation.
///
/// The global dispatch owns the sink runtime. Dropping this value only releases
/// the caller's handle clone and can never stop globally installed sinks.
#[derive(Debug)]
pub struct InstalledObservability {
    handle: ObservabilityHandle,
}

impl InstalledObservability {
    /// Returns a cloneable application control/event handle.
    #[must_use]
    pub fn handle(&self) -> ObservabilityHandle {
        self.handle.clone()
    }

    /// Flushes every globally retained sink without shutting any worker down.
    ///
    /// The bound is applied independently to each sink. The operation may be
    /// called repeatedly, and later records remain deliverable.
    ///
    /// # Errors
    ///
    /// Returns every sink-specific timeout, closure, or adapter flush failure.
    pub fn flush(&self, timeout_per_sink: Duration) -> Result<(), FlushError> {
        self.handle.flush(timeout_per_sink)
    }

    /// Atomically drains complete per-sink loss deltas for heartbeat reporting.
    ///
    /// This does not emit records. Call [`Self::emit_loss_notice`] once for each
    /// returned delta. Loss caused while routing those notices remains queued
    /// for the next heartbeat, so reporting cannot recurse in one drain. A
    /// short-lived process should flush first to place a barrier after queued
    /// deliveries, then drain before exit.
    #[must_use]
    pub fn drain_loss_deltas(&self) -> Vec<SinkLossDelta> {
        self.handle.drain_loss_deltas()
    }

    /// Routes one canonical `TELEMETRY_DROPPED` record to every sink except its
    /// origin. No session identifier is required.
    ///
    /// The canonical `sink` field is encoded as `<sink>:<loss_class>` because
    /// the PR1 lifecycle schema has one sink field and a count field.
    ///
    /// # Errors
    ///
    /// Returns canonical schema, field validation, or counter conversion errors.
    pub fn emit_loss_notice(
        &self,
        delta: &SinkLossDelta,
        timestamp: impl Into<String>,
    ) -> Result<EmissionReport, RuntimeError> {
        self.handle.emit_loss_notice(delta, timestamp)
    }
}

/// Failed global installation with the still-live local runtime recoverable.
#[derive(Debug)]
pub struct GlobalInstallError {
    message: String,
    runtime: ObservabilityRuntime,
}

impl GlobalInstallError {
    /// Recovers the local runtime so the caller may continue using or shut it down.
    #[must_use]
    pub fn into_runtime(self) -> ObservabilityRuntime {
        self.runtime
    }
}

impl Display for GlobalInstallError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "global tracing installation failed: {}",
            self.message
        )
    }
}

impl Error for GlobalInstallError {}

/// Cloneable profile control and typed record router.
#[derive(Clone)]
pub struct ObservabilityHandle {
    inner: Arc<RuntimeInner>,
}

impl std::fmt::Debug for ObservabilityHandle {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ObservabilityHandle")
            .field("profile", &self.profile())
            .field("sink_stats", &self.sink_stats())
            .finish_non_exhaustive()
    }
}

impl ObservabilityHandle {
    /// Flushes every retained sink without shutting any worker down.
    ///
    /// The bound is applied independently to each sink. This is safe for both
    /// local and process-global handles and does not invalidate the dispatcher.
    ///
    /// # Errors
    ///
    /// Returns every sink-specific timeout, closure, or adapter flush failure.
    pub fn flush(&self, timeout_per_sink: Duration) -> Result<(), FlushError> {
        self.inner.flush_sinks(timeout_per_sink)
    }

    /// Atomically drains queue-full, queue-closed, delivery, and flush failure
    /// deltas from initial zero.
    ///
    /// Repeated calls return an empty vector until new loss occurs. The complete
    /// cursor is independent of legacy [`Self::take_drop_notices`] accounting;
    /// heartbeat code should use this API exclusively. At most four deltas are
    /// returned per registered sink. Short-lived callers should flush first so
    /// all earlier delivery and flush outcomes are visible.
    #[must_use]
    pub fn drain_loss_deltas(&self) -> Vec<SinkLossDelta> {
        self.inner.drain_loss_deltas()
    }

    /// Routes one canonical `TELEMETRY_DROPPED` record to all sinks except the
    /// originating sink, without consulting diagnostic filters or requiring a
    /// session. Worker failures caused by this route are counted for a later
    /// drain and are never emitted recursively here.
    ///
    /// # Errors
    ///
    /// Returns canonical schema, field validation, or counter conversion errors.
    pub fn emit_loss_notice(
        &self,
        delta: &SinkLossDelta,
        timestamp: impl Into<String>,
    ) -> Result<EmissionReport, RuntimeError> {
        let count = i64::try_from(delta.count()).map_err(|_| RuntimeError::CounterOverflow)?;
        let mut fields = StructuredFields::default();
        fields
            .insert(
                "sink",
                FieldValue::String(format!("{}:{}", delta.sink(), delta.class().as_str())),
            )
            .map_err(|error| RuntimeError::Lifecycle(error.to_string()))?;
        fields
            .insert("dropped_count", FieldValue::Integer(count))
            .map_err(|error| RuntimeError::Lifecycle(error.to_string()))?;
        let target = TelemetryTarget::new("arcen::telemetry").map_err(RuntimeError::Schema)?;
        let timestamp = timestamp.into();
        self.inner.with_next_sequence(|sequence| {
            let record = CanonicalRecord::new(
                timestamp,
                sequence,
                OperationalProfile::Critical,
                EventSeverity::Warn,
                self.inner.role,
                self.inner.component.clone(),
                self.inner.platform,
                target,
                "telemetry delivery loss detected",
            )
            .map_err(RuntimeError::Schema)?
            .with_event(LifecycleEventKind::TelemetryDropped)
            .with_fields(fields);
            Ok(self
                .inner
                .route_record_excluding(&record, Some(delta.sink()), true))
        })
    }

    /// Returns the selected operational profile.
    ///
    /// # Errors
    ///
    /// Returns an error if concurrent reload state was poisoned.
    pub fn profile(&self) -> Result<OperationalProfile, RuntimeError> {
        Ok(self
            .inner
            .reload_state
            .lock()
            .map_err(|_| RuntimeError::Poisoned)?
            .profile)
    }

    /// Returns the coherent effective profile and `ARCEN_LOG` refinement.
    ///
    /// # Errors
    ///
    /// Returns an error if concurrent reload state was poisoned.
    pub fn effective_diagnostic_policy(&self) -> Result<EffectiveDiagnosticPolicy, RuntimeError> {
        let state = self
            .inner
            .reload_state
            .lock()
            .map_err(|_| RuntimeError::Poisoned)?;
        Ok(EffectiveDiagnosticPolicy {
            profile: state.profile,
            arcen_log: state.arcen_log.clone(),
        })
    }

    /// Reloads the selected profile while retaining the current `ARCEN_LOG` refinement.
    ///
    /// # Errors
    ///
    /// Returns filter construction, reload, or synchronization errors.
    pub fn reload_profile(&self, profile: OperationalProfile) -> Result<(), RuntimeError> {
        let mut state = self
            .inner
            .reload_state
            .lock()
            .map_err(|_| RuntimeError::Poisoned)?;
        let arcen_log = state.arcen_log.clone();
        Self::apply_reload(&mut state, profile, arcen_log)
    }

    /// Reloads profile and `ARCEN_LOG` refinement atomically from the caller's perspective.
    ///
    /// # Errors
    ///
    /// Returns filter construction, reload, or synchronization errors.
    pub fn reload_profile_with(
        &self,
        profile: OperationalProfile,
        arcen_log: Option<String>,
    ) -> Result<(), RuntimeError> {
        let mut state = self
            .inner
            .reload_state
            .lock()
            .map_err(|_| RuntimeError::Poisoned)?;
        Self::apply_reload(&mut state, profile, arcen_log)
    }

    fn apply_reload(
        state: &mut ReloadState,
        profile: OperationalProfile,
        arcen_log: Option<String>,
    ) -> Result<(), RuntimeError> {
        let filter = build_filter(profile, arcen_log.as_deref()).map_err(RuntimeError::Filter)?;
        state
            .filter_reload
            .as_ref()
            .ok_or(RuntimeError::NotInitialized)?
            .reload(filter)
            .map_err(|error| RuntimeError::Reload(error.to_string()))?;
        state.arcen_log = arcen_log;
        state.profile = profile;
        Ok(())
    }

    /// Routes a prebuilt canonical record solely by its minimum profile.
    ///
    /// Event severity is intentionally not consulted.
    ///
    /// # Errors
    ///
    /// Returns an error if concurrent reload state was poisoned.
    pub fn emit_record(&self, record: &CanonicalRecord) -> Result<EmissionReport, RuntimeError> {
        if !self.profile()?.includes(record.minimum_profile()) {
            return Ok(EmissionReport::excluded());
        }
        Ok(self.inner.route_record(record))
    }

    /// Builds and routes one ad-hoc canonical record using runtime-owned identity,
    /// timestamp, and sequence values.
    ///
    /// The caller supplies semantic event data and explicit top-level session
    /// context, but cannot supply process identity, timestamp, or sequence.
    /// Minimum profile controls inclusion independently of true severity.
    ///
    /// # Errors
    ///
    /// Returns schema validation, clock, sequence, profile, or synchronization
    /// errors. Invalid messages and identity values are never routed.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_ad_hoc(
        &self,
        minimum_profile: OperationalProfile,
        severity: EventSeverity,
        target: TelemetryTarget,
        message: impl Into<String>,
        context: LifecycleContext,
        fields: StructuredFields,
    ) -> Result<EmissionReport, RuntimeError> {
        let timestamp = canonical_now().map_err(|()| RuntimeError::TimestampUnavailable)?;
        let message = message.into();
        self.inner.with_next_sequence(|sequence| {
            let mut record = CanonicalRecord::new(
                timestamp,
                sequence,
                minimum_profile,
                severity,
                self.inner.role,
                self.inner.component.clone(),
                self.inner.platform,
                target,
                message,
            )
            .map_err(RuntimeError::Schema)?
            .with_sid(context.sid)
            .with_identity(context.user, context.host, context.peer_addr)
            .map_err(RuntimeError::Schema)?
            .with_fields(fields);
            if let Some(health_state) = context.health_state {
                record = record.with_health_state(health_state);
            }
            self.emit_record(&record)
        })
    }

    /// Bridges one validated lifecycle event and explicit top-level context.
    ///
    /// # Errors
    ///
    /// Returns identity mismatch or canonical schema validation errors.
    pub fn emit_lifecycle(
        &self,
        event: &ValidatedLifecycleEvent,
        context: LifecycleContext,
        timestamp: impl Into<String>,
        target: TelemetryTarget,
        message: impl Into<String>,
    ) -> Result<EmissionReport, RuntimeError> {
        if event.correlation_id() != &context.sid {
            return Err(RuntimeError::CorrelationMismatch);
        }
        let timestamp = timestamp.into();
        let message = message.into();
        self.inner.with_next_sequence(|sequence| {
            let mut record = CanonicalRecord::new(
                timestamp,
                sequence,
                event.definition().minimum_profile,
                event.definition().severity.into(),
                self.inner.role,
                self.inner.component.clone(),
                self.inner.platform,
                target,
                message,
            )
            .map_err(RuntimeError::Schema)?
            .with_event(event.kind())
            .with_sid(context.sid)
            .with_identity(context.user, context.host, context.peer_addr)
            .map_err(RuntimeError::Schema)?
            .with_fields(event.fields().clone());
            if let Some(health_state) = context.health_state {
                record = record.with_health_state(health_state);
            }
            self.emit_record(&record)
        })
    }

    /// Returns per-sink monotonic delivery counters.
    #[must_use]
    pub fn sink_stats(&self) -> Vec<SinkStats> {
        self.inner
            .record_sinks
            .iter()
            .map(BoundedSink::stats)
            .chain(self.inner.text_sinks.iter().map(BoundedSink::stats))
            .collect()
    }

    /// Returns tracing events that could not be converted to the canonical schema.
    #[must_use]
    pub fn format_failures(&self) -> u64 {
        self.inner.format_failures.load(Ordering::Relaxed)
    }

    /// Builds, but does not recursively emit, `TELEMETRY_DROPPED` notices.
    ///
    /// The explicit pull model prevents a saturated sink from recursively
    /// generating loss records while routing its own loss record. This legacy
    /// method covers queue rejection only and uses a cursor independent from
    /// [`Self::drain_loss_deltas`]; new heartbeat code should not mix the APIs.
    ///
    /// # Errors
    ///
    /// Returns counter conversion or lifecycle validation failures.
    pub fn take_drop_notices(
        &self,
        correlation_id: &CorrelationId,
    ) -> Result<Vec<ValidatedLifecycleEvent>, RuntimeError> {
        let mut notices = Vec::new();
        for sink in &self.inner.record_sinks {
            let dropped = sink.take_unreported_drops();
            if dropped != 0 {
                notices.push(drop_notice(&sink.stats().name, dropped, correlation_id)?);
            }
        }
        for sink in &self.inner.text_sinks {
            let dropped = sink.take_unreported_drops();
            if dropped != 0 {
                notices.push(drop_notice(&sink.stats().name, dropped, correlation_id)?);
            }
        }
        Ok(notices)
    }

    fn route_diagnostic(&self, event: &Event<'_>) {
        let metadata = event.metadata();
        let severity = match *metadata.level() {
            tracing::Level::ERROR => EventSeverity::Error,
            tracing::Level::WARN => EventSeverity::Warn,
            tracing::Level::INFO => EventSeverity::Info,
            tracing::Level::DEBUG | tracing::Level::TRACE => EventSeverity::Debug,
        };
        let minimum_profile = diagnostic_minimum_profile(severity);
        let target = TelemetryTarget::new(metadata.target())
            .or_else(|_| TelemetryTarget::new("arcen::dependency"));
        let Ok(target) = target else {
            self.inner.format_failures.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let mut visitor = DiagnosticVisitor::default();
        event.record(&mut visitor);
        if visitor.invalid_fields != 0 {
            self.inner
                .format_failures
                .fetch_add(visitor.invalid_fields, Ordering::Relaxed);
        }
        let message = visitor
            .message
            .unwrap_or_else(|| metadata.name().to_owned());
        let Ok(timestamp) = canonical_now() else {
            self.inner.format_failures.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let result = self.inner.with_next_sequence(|sequence| {
            let record = CanonicalRecord::new(
                timestamp,
                sequence,
                minimum_profile,
                severity,
                self.inner.role,
                self.inner.component.clone(),
                self.inner.platform,
                target,
                message,
            )
            .map_err(RuntimeError::Schema)?
            .with_fields(visitor.fields);
            Ok(self.inner.route_record(&record))
        });
        if result.is_err() {
            self.inner.format_failures.fetch_add(1, Ordering::Relaxed);
        }
    }
}

fn drop_notice(
    sink_name: &str,
    dropped: u64,
    correlation_id: &CorrelationId,
) -> Result<ValidatedLifecycleEvent, RuntimeError> {
    let dropped = i64::try_from(dropped).map_err(|_| RuntimeError::CounterOverflow)?;
    let mut fields = StructuredFields::default();
    fields
        .insert("sink", FieldValue::String(sink_name.to_owned()))
        .map_err(|error| RuntimeError::Lifecycle(error.to_string()))?;
    fields
        .insert("dropped_count", FieldValue::Integer(dropped))
        .map_err(|error| RuntimeError::Lifecycle(error.to_string()))?;
    ValidatedLifecycleEvent::new(
        LifecycleEventKind::TelemetryDropped,
        correlation_id.clone(),
        fields,
    )
    .map_err(|error| RuntimeError::Lifecycle(error.to_string()))
}

struct RuntimeInner {
    role: TelemetryRole,
    component: TelemetryComponent,
    platform: TelemetryPlatform,
    sequence: Mutex<u64>,
    format_failures: AtomicU64,
    record_sinks: Vec<BoundedSink<CanonicalRecord>>,
    text_sinks: Vec<BoundedSink<String>>,
    reload_state: Mutex<ReloadState>,
}

struct ReloadState {
    filter_reload: Option<FilterReloadHandle>,
    profile: OperationalProfile,
    arcen_log: Option<String>,
}

impl std::fmt::Debug for RuntimeInner {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RuntimeInner")
            .field("role", &self.role)
            .field("component", &self.component)
            .field("platform", &self.platform)
            .field(
                "profile",
                &self
                    .reload_state
                    .lock()
                    .map(|state| state.profile)
                    .map_err(|_| "poisoned"),
            )
            .field(
                "sequence",
                &self
                    .sequence
                    .lock()
                    .map(|sequence| *sequence)
                    .map_err(|_| "poisoned"),
            )
            .finish_non_exhaustive()
    }
}

impl RuntimeInner {
    fn with_next_sequence<T>(
        &self,
        operation: impl FnOnce(u64) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        let mut sequence = self.sequence.lock().map_err(|_| RuntimeError::Poisoned)?;
        let next = sequence
            .checked_add(1)
            .ok_or(RuntimeError::SequenceExhausted)?;
        *sequence = next;
        operation(next)
    }

    fn flush_sinks(&self, timeout_per_sink: Duration) -> Result<(), FlushError> {
        let mut failures = Vec::new();
        for sink in &self.record_sinks {
            if let Err(error) = sink.flush(timeout_per_sink) {
                failures.push(SinkFlushFailure {
                    sink: sink.stats().name,
                    error,
                });
            }
        }
        for sink in &self.text_sinks {
            if let Err(error) = sink.flush(timeout_per_sink) {
                failures.push(SinkFlushFailure {
                    sink: sink.stats().name,
                    error,
                });
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(FlushError { failures })
        }
    }

    fn drain_loss_deltas(&self) -> Vec<SinkLossDelta> {
        self.record_sinks
            .iter()
            .flat_map(BoundedSink::take_loss_deltas)
            .chain(
                self.text_sinks
                    .iter()
                    .flat_map(BoundedSink::take_loss_deltas),
            )
            .collect()
    }

    fn route_record(&self, record: &CanonicalRecord) -> EmissionReport {
        self.route_record_excluding(record, None, false)
    }

    fn route_record_excluding(
        &self,
        record: &CanonicalRecord,
        excluded_sink: Option<&str>,
        healthy_only: bool,
    ) -> EmissionReport {
        let human = format_human(record);
        let mut report = EmissionReport {
            included: true,
            enqueued: 0,
            dropped: 0,
        };
        for sink in &self.record_sinks {
            if sink.is_alive()
                && (!healthy_only || sink.is_healthy())
                && excluded_sink != Some(sink.name())
            {
                report.add(sink.try_send(record.clone()));
            }
        }
        match human {
            Ok(line) => {
                for sink in &self.text_sinks {
                    if sink.is_alive()
                        && (!healthy_only || sink.is_healthy())
                        && excluded_sink != Some(sink.name())
                    {
                        report.add(sink.try_send(line.clone()));
                    }
                }
            }
            Err(()) => {
                self.format_failures.fetch_add(1, Ordering::Relaxed);
            }
        }
        report
    }
}

fn format_human(record: &CanonicalRecord) -> Result<String, ()> {
    let value = serde_json::to_value(record).map_err(|_| ())?;
    let severity = value
        .get("severity")
        .and_then(serde_json::Value::as_str)
        .ok_or(())?;
    let target = value
        .get("target")
        .and_then(serde_json::Value::as_str)
        .ok_or(())?;
    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .ok_or(())?;
    Ok(format!("{severity:<5} {target}: {message}\n"))
}

fn build_filter(
    profile: OperationalProfile,
    arcen_log: Option<&str>,
) -> Result<EnvFilter, ParseError> {
    let base = LevelSpec::new(profile).directive();
    match arcen_log.filter(|value| !value.is_empty()) {
        Some(refinement) => EnvFilter::try_new(format!("{base},{refinement}")),
        None => EnvFilter::try_new(base),
    }
}

const fn diagnostic_minimum_profile(severity: EventSeverity) -> OperationalProfile {
    match severity {
        EventSeverity::Error => OperationalProfile::Critical,
        EventSeverity::Warn => OperationalProfile::Error,
        EventSeverity::Info => OperationalProfile::Info,
        EventSeverity::Debug => OperationalProfile::Debug,
    }
}

#[derive(Debug, Default)]
struct DiagnosticVisitor {
    message: Option<String>,
    fields: StructuredFields,
    invalid_fields: u64,
}

/// Make a string safe for the telemetry field contract without losing it.
///
/// `StructuredFields::insert` rejects any string containing a control
/// character or exceeding the size bound, and a rejected field is dropped
/// entirely. That is the correct contract at the boundary and the wrong
/// outcome for a diagnostic: an error carrying a child process's multi-line
/// stderr would vanish completely because it contained a newline, leaving an
/// empty `fields` object exactly when something had gone wrong. Escaping and
/// truncating keeps the evidence.
fn sanitize_field_text(value: &str) -> String {
    const LIMIT: usize = arcen_telemetry::MAX_FIELD_STRING_BYTES;
    let mut out = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if control.is_control() => out.push('\u{fffd}'),
            other => out.push(other),
        }
    }
    if out.len() <= LIMIT {
        return out;
    }
    // Keep both ends. A diagnostic's cause is usually its last line, so
    // head-only truncation reliably discards the answer: that is exactly what
    // happened when a capenc failure was cut off before the line naming why.
    let marker = "...[cut]...";
    let budget = LIMIT.saturating_sub(marker.len());
    let head_budget = budget / 2;
    let tail_budget = budget - head_budget;

    let mut head_end = head_budget.min(out.len());
    while head_end > 0 && !out.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = out.len().saturating_sub(tail_budget);
    while tail_start < out.len() && !out.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if tail_start < head_end {
        tail_start = head_end;
    }

    let mut result = String::with_capacity(LIMIT);
    result.push_str(&out[..head_end]);
    result.push_str(marker);
    result.push_str(&out[tail_start..]);
    result
}

impl DiagnosticVisitor {
    fn insert(&mut self, field: &tracing::field::Field, value: FieldValue) {
        let value = match value {
            FieldValue::String(text) => FieldValue::String(sanitize_field_text(&text)),
            other => other,
        };
        if field.name() == "message" {
            if let FieldValue::String(message) = value {
                self.message = Some(message);
            } else {
                self.invalid_fields += 1;
            }
        } else if self.fields.insert(field.name(), value).is_err() {
            self.invalid_fields += 1;
        }
    }
}

impl tracing::field::Visit for DiagnosticVisitor {
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.insert(field, FieldValue::Boolean(value));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.insert(field, FieldValue::Integer(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        match i64::try_from(value) {
            Ok(value) => self.insert(field, FieldValue::Integer(value)),
            Err(_) => self.invalid_fields += 1,
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.insert(field, FieldValue::String(value.to_owned()));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.insert(field, FieldValue::String(format!("{value:?}")));
    }
}

#[derive(Debug)]
struct DiagnosticLayer {
    handle: ObservabilityHandle,
}

impl<S> Layer<S> for DiagnosticLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        self.handle.route_diagnostic(event);
    }
}

fn canonical_now() -> Result<String, ()> {
    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ())?;
    let seconds = duration.as_secs();
    let micros = duration.subsec_micros();
    let days = i64::try_from(seconds / 86_400).map_err(|_| ())?;
    let seconds_of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    if !(0..=9999).contains(&year) {
        return Err(());
    }
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    Ok(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z"
    ))
}

const fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

/// Result of routing one record to all registered sinks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmissionReport {
    /// Whether the selected profile included the record.
    pub included: bool,
    /// Sink queues that accepted it.
    pub enqueued: usize,
    /// Sink queues that rejected it without blocking.
    pub dropped: usize,
}

/// One sink-specific bounded flush failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkFlushFailure {
    /// Configured sink name.
    pub sink: String,
    /// Timeout, closure, or adapter failure.
    pub error: WaitError,
}

/// Complete set of failures from one multi-sink flush attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlushError {
    failures: Vec<SinkFlushFailure>,
}

impl FlushError {
    /// Returns all failures in sink registration order.
    #[must_use]
    pub fn failures(&self) -> &[SinkFlushFailure] {
        &self.failures
    }

    /// Consumes the error and returns all failures.
    #[must_use]
    pub fn into_failures(self) -> Vec<SinkFlushFailure> {
        self.failures
    }
}

impl Display for FlushError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} observability sink(s) failed to flush",
            self.failures.len()
        )
    }
}

impl Error for FlushError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.failures
            .first()
            .map(|failure| &failure.error as &(dyn Error + 'static))
    }
}

impl EmissionReport {
    const fn excluded() -> Self {
        Self {
            included: false,
            enqueued: 0,
            dropped: 0,
        }
    }

    fn add(&mut self, outcome: DeliveryOutcome) {
        match outcome {
            DeliveryOutcome::Enqueued => self.enqueued += 1,
            DeliveryOutcome::QueueFull | DeliveryOutcome::Closed => self.dropped += 1,
        }
    }
}

/// Clone-safe RAII owner for bounded flush and shutdown.
#[derive(Debug)]
pub struct ShutdownGuard {
    inner: Arc<RuntimeInner>,
    armed: bool,
}

impl ShutdownGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Flushes every registered sink with the bound applied to each sink.
    ///
    /// # Errors
    ///
    /// Attempts every sink and returns the first observed failure.
    pub fn flush(&self, timeout_per_sink: Duration) -> Result<(), RuntimeError> {
        let mut first_error = None;
        for sink in &self.inner.record_sinks {
            if let Err(error) = sink.flush(timeout_per_sink) {
                first_error.get_or_insert_with(|| RuntimeError::SinkWait {
                    sink: sink.stats().name,
                    error,
                });
            }
        }
        for sink in &self.inner.text_sinks {
            if let Err(error) = sink.flush(timeout_per_sink) {
                first_error.get_or_insert_with(|| RuntimeError::SinkWait {
                    sink: sink.stats().name,
                    error,
                });
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    /// Drains, flushes, and joins every sink with a per-sink wait bound.
    ///
    /// # Errors
    ///
    /// Attempts every sink and returns the first observed failure.
    pub fn shutdown(&self, timeout_per_sink: Duration) -> Result<(), RuntimeError> {
        let mut first_error = None;
        for sink in &self.inner.record_sinks {
            if let Err(error) = sink.shutdown(timeout_per_sink) {
                first_error.get_or_insert_with(|| RuntimeError::SinkWait {
                    sink: sink.stats().name,
                    error,
                });
            }
        }
        for sink in &self.inner.text_sinks {
            if let Err(error) = sink.shutdown(timeout_per_sink) {
                first_error.get_or_insert_with(|| RuntimeError::SinkWait {
                    sink: sink.stats().name,
                    error,
                });
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

impl Drop for ShutdownGuard {
    fn drop(&mut self) {
        if self.armed {
            let _best_effort = self.shutdown(DROP_WAIT);
        }
    }
}

/// Runtime startup failure.
#[derive(Debug)]
pub enum BuildError {
    /// No canonical writer or native/file canonical sink was supplied.
    MissingCanonicalSink,
    /// Sink names must be unique for unambiguous loss accounting.
    DuplicateSinkName(String),
    /// `ARCEN_LOG` was not valid Unicode.
    Environment(std::env::VarError),
    /// Diagnostic filter was invalid.
    Filter(ParseError),
    /// Sink worker failed to initialize.
    Sink(SinkBuildError),
    /// Internal initialization state was poisoned.
    Poisoned,
}

impl Display for BuildError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingCanonicalSink => formatter.write_str("a canonical sink is required"),
            Self::DuplicateSinkName(name) => write!(formatter, "sink name `{name}` is duplicated"),
            Self::Environment(error) => write!(formatter, "ARCEN_LOG could not be read: {error}"),
            Self::Filter(error) => write!(formatter, "diagnostic filter is invalid: {error}"),
            Self::Sink(error) => write!(formatter, "sink initialization failed: {error}"),
            Self::Poisoned => formatter.write_str("observability initialization state is poisoned"),
        }
    }
}

impl Error for BuildError {}

/// Event routing, reload, or controlled-shutdown failure.
#[derive(Debug)]
pub enum RuntimeError {
    /// The system clock could not produce a canonical UTC timestamp.
    TimestampUnavailable,
    /// The process-wide canonical sequence exhausted `u64`.
    SequenceExhausted,
    /// Lifecycle correlation and explicit top-level SID differed.
    CorrelationMismatch,
    /// Canonical schema construction failed.
    Schema(SchemaValidationError),
    /// Drop count exceeded the lifecycle integer representation.
    CounterOverflow,
    /// Lifecycle drop-notice construction failed.
    Lifecycle(String),
    /// Diagnostic filter was invalid.
    Filter(ParseError),
    /// Filter reload failed.
    Reload(String),
    /// Runtime subscriber was not initialized.
    NotInitialized,
    /// Shared runtime state was poisoned.
    Poisoned,
    /// One bounded sink wait failed.
    SinkWait {
        /// Sink name.
        sink: String,
        /// Flush/shutdown failure.
        error: WaitError,
    },
}

impl Display for RuntimeError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TimestampUnavailable => {
                formatter.write_str("system clock cannot produce a canonical timestamp")
            }
            Self::SequenceExhausted => formatter.write_str("canonical sequence exhausted u64"),
            Self::CorrelationMismatch => {
                formatter.write_str("lifecycle correlation does not match top-level sid")
            }
            Self::Schema(error) => write!(formatter, "canonical record is invalid: {error}"),
            Self::CounterOverflow => formatter.write_str("sink drop count exceeds i64"),
            Self::Lifecycle(error) => write!(formatter, "lifecycle event is invalid: {error}"),
            Self::Filter(error) => write!(formatter, "diagnostic filter is invalid: {error}"),
            Self::Reload(error) => write!(formatter, "diagnostic filter reload failed: {error}"),
            Self::NotInitialized => formatter.write_str("observability runtime is not initialized"),
            Self::Poisoned => formatter.write_str("observability runtime state is poisoned"),
            Self::SinkWait { sink, error } => {
                write!(formatter, "sink `{sink}` wait failed: {error}")
            }
        }
    }
}

impl Error for RuntimeError {}

#[cfg(test)]
mod tests {
    #[test]
    fn diagnostic_text_is_escaped_rather_than_dropped() {
        // A child process error carrying multi-line stderr must survive. Before
        // this, the newline made StructuredFields::insert reject the whole
        // field, so the log recorded an error with an empty fields object.
        let sanitized = super::sanitize_field_text("capenc failed:\nline two\ttabbed");
        assert!(!sanitized.chars().any(char::is_control), "no control chars");
        assert!(sanitized.contains("\\n"), "newline is escaped, not lost");
        assert!(sanitized.contains("line two"), "content survives");

        let mut fields = arcen_telemetry::StructuredFields::default();
        fields
            .insert("error", arcen_telemetry::FieldValue::String(sanitized))
            .expect("sanitized text must satisfy the field contract");
    }

    #[test]
    fn truncation_keeps_the_tail_because_that_is_where_the_cause_is() {
        let long = format!(
            "{}NVENC init failed: the actual reason",
            "noise ".repeat(400)
        );
        let sanitized = super::sanitize_field_text(&long);
        assert!(sanitized.len() <= arcen_telemetry::MAX_FIELD_STRING_BYTES);
        assert!(
            sanitized.ends_with("NVENC init failed: the actual reason"),
            "the last line must survive truncation: {sanitized}"
        );
        assert!(sanitized.starts_with("noise"), "the head is kept too");
    }

    #[test]
    fn oversized_diagnostic_text_is_truncated_and_still_accepted() {
        let sanitized = super::sanitize_field_text(&"x".repeat(4096));
        assert!(sanitized.contains("...[cut]..."));
        assert!(sanitized.len() <= arcen_telemetry::MAX_FIELD_STRING_BYTES);
        let mut fields = arcen_telemetry::StructuredFields::default();
        fields
            .insert("error", arcen_telemetry::FieldValue::String(sanitized))
            .expect("truncated text must satisfy the field contract");
    }

    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    #[derive(Debug, Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn is_empty(&self) -> bool {
            self.0.lock().expect("writer lock").is_empty()
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
    fn process_lifetime_install_keeps_sinks_alive_after_owner_drop() {
        let writer = SharedWriter::default();
        let retained_dispatch = Arc::new(Mutex::new(None));
        let dispatch_slot = Arc::clone(&retained_dispatch);
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
        let installed = runtime
            .install_process_lifetime(|dispatch| {
                *dispatch_slot.lock().map_err(|_| "dispatch lock poisoned")? = Some(dispatch);
                Ok(())
            })
            .expect("process-lifetime install");
        let handle = installed.handle();
        drop(installed);
        drop(handle.clone());

        let record = CanonicalRecord::new(
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
        assert_eq!(handle.emit_record(&record).expect("emit").enqueued, 1);

        let deadline = Instant::now() + Duration::from_secs(1);
        while writer.is_empty() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(1));
        }
        assert!(!writer.is_empty());
        assert_eq!(handle.sink_stats()[0].delivered, 1);
        assert!(retained_dispatch.lock().expect("dispatch lock").is_some());
    }
}
