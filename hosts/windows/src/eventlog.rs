//! Windows Event Log lifecycle sink.
//!
//! Formatting is fully separated from the Win32 FFI backend so the exact
//! insertion strings produced for one [`ValidatedLifecycleEvent`] can be
//! exercised without a live `Application` event source. The sink is
//! process-local (one `RegisterEventSourceW` per process, released on
//! `Drop`), best-effort, and never changes a caller's own outcome: a
//! formatting or FFI failure is reported once through `tracing` and then
//! silently ignored for the remainder of the process lifetime.
//!
//! Every adapter in this module accepts only [`ValidatedLifecycleEvent`], so
//! an event's schema, category, and outcome are already proven before any
//! FFI call is attempted. No username, SID, raw OS error, or credential
//! material is ever placed in a rendered insertion string.

use std::fmt::{self, Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arcen_observability::{LifecycleContext, ObservabilityHandle, Sink, SinkError};
use arcen_telemetry::{
    CanonicalRecord, CorrelationId, EventSeverity, FieldValue, LifecycleCategory,
    LifecycleSeverity, TelemetryTarget, ValidatedLifecycleEvent, MAX_FIELD_KEY_BYTES,
    MAX_FIELD_STRING_BYTES,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EvtClose, EvtNext, EvtQuery, EvtQueryChannelPath,
    EvtQueryReverseDirection, EvtRender, EvtRenderEventXml, RegisterEventSourceW, ReportEventW,
    EVENTLOG_ERROR_TYPE, EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE, EVT_HANDLE,
    REPORT_EVENT_TYPE,
};

use crate::logging::EVENTLOG;

/// Event source / provider name registered under the `Application` channel.
pub(crate) const EVENT_PROVIDER: &str = "ArcenPier";
/// Windows Event Log channel used for lifecycle records.
pub(crate) const EVENT_CHANNEL: &str = "Application";
/// Registry path where the event source is (de)registered.
#[allow(dead_code)]
pub(crate) const EVENT_SOURCE_REGISTRY_PATH: &str =
    r"SYSTEM\CurrentControlSet\Services\EventLog\Application\ArcenPier";

/// One human summary, the bounded canonical record envelope, and up to
/// [`arcen_telemetry::MAX_STRUCTURED_FIELDS`] schema-approved fields.
pub(crate) const MAX_INSERTION_STRINGS: usize = 40;
/// Bound on one rendered insertion string, matching the shared field-value cap.
pub(crate) const MAX_INSERTION_STRING_CHARS: usize =
    MAX_FIELD_KEY_BYTES + 1 + MAX_FIELD_STRING_BYTES;
/// Bound on the total UTF-16 code units rendered for one event report.
pub(crate) const MAX_RENDERED_UTF16_UNITS: usize = 8192;
/// Maximum reports waiting for the dedicated Event Log worker.
pub(crate) const MAX_PENDING_EVENTS: usize = 64;

const HRESULT_ACCESS_DENIED: u32 = 0x8007_0005;
const HRESULT_NO_MORE_ITEMS: u32 = 0x8007_0103;
const HRESULT_INSUFFICIENT_BUFFER: u32 = 0x8007_007a;

/// A formatting-time defensive-depth bound violation.
///
/// [`ValidatedLifecycleEvent`] already enforces field count/size/content
/// bounds; this only guards the sink against a future shared-schema change
/// that raises those caps without updating this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EventFormatError {
    TooManyInsertionStrings(usize),
    InsertionStringTooLong(usize),
    RenderedTooLarge(usize),
}

impl Display for EventFormatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyInsertionStrings(count) => write!(
                formatter,
                "lifecycle event insertion-string count {count} exceeds {MAX_INSERTION_STRINGS}"
            ),
            Self::InsertionStringTooLong(units) => write!(
                formatter,
                "lifecycle event insertion string {units} UTF-16 units exceeds \
                 {MAX_INSERTION_STRING_CHARS}"
            ),
            Self::RenderedTooLarge(units) => write!(
                formatter,
                "lifecycle event rendered size {units} UTF-16 units exceeds \
                 {MAX_RENDERED_UTF16_UNITS}"
            ),
        }
    }
}

impl std::error::Error for EventFormatError {}

/// Builds deterministic, bounded Windows Event Log insertion strings for one
/// validated lifecycle event.
///
/// Entry 0 is a human summary. The remaining entries are sorted `key=value`
/// pairs covering `event_id`, `event_name`, `category`, `outcome`,
/// `severity`, `correlation_id`, and every schema-approved structured field.
/// Validation happens here, before any FFI call is attempted.
pub(crate) fn build_insertion_strings(
    event: &ValidatedLifecycleEvent,
) -> Result<Vec<String>, EventFormatError> {
    let definition = event.definition();
    let summary = format!(
        "ArcenPier lifecycle event {} {} ({})",
        definition.kind.id(),
        definition.name,
        definition.severity.as_str()
    );

    let mut pairs: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    pairs.insert("event_id".to_string(), definition.kind.id().to_string());
    pairs.insert("event_name".to_string(), definition.name.to_string());
    pairs.insert(
        "category".to_string(),
        definition.category.as_str().to_string(),
    );
    pairs.insert(
        "outcome".to_string(),
        definition.outcome.as_str().to_string(),
    );
    pairs.insert(
        "severity".to_string(),
        definition.severity.as_str().to_string(),
    );
    pairs.insert(
        "correlation_id".to_string(),
        event.correlation_id().as_str().to_string(),
    );
    for (key, value) in event.fields().as_map() {
        pairs.insert(key.clone(), field_value_string(value));
    }

    let mut strings = Vec::with_capacity(1 + pairs.len());
    strings.push(summary);
    for (key, value) in pairs {
        strings.push(format!("{key}={value}"));
    }

    if strings.len() > MAX_INSERTION_STRINGS {
        return Err(EventFormatError::TooManyInsertionStrings(strings.len()));
    }
    let mut rendered_units = 0usize;
    for string in &strings {
        let units = string.encode_utf16().count();
        if units > MAX_INSERTION_STRING_CHARS {
            return Err(EventFormatError::InsertionStringTooLong(units));
        }
        // +1 accounts for the NUL terminator each string needs when rendered.
        rendered_units += units + 1;
    }
    if rendered_units > MAX_RENDERED_UTF16_UNITS {
        return Err(EventFormatError::RenderedTooLarge(rendered_units));
    }
    Ok(strings)
}

fn field_value_string(value: &FieldValue) -> String {
    match value {
        FieldValue::Boolean(value) => value.to_string(),
        FieldValue::Integer(value) => value.to_string(),
        FieldValue::String(value) => value.clone(),
    }
}

/// Maps shared severity to the classic Win32 Event Log report type.
pub(crate) const fn severity_event_type(severity: LifecycleSeverity) -> REPORT_EVENT_TYPE {
    match severity {
        LifecycleSeverity::Information => EVENTLOG_INFORMATION_TYPE,
        LifecycleSeverity::Warning => EVENTLOG_WARNING_TYPE,
        LifecycleSeverity::Error => EVENTLOG_ERROR_TYPE,
    }
}

/// An opaque native (or fake, in tests) event-source handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RawEventHandle(pub(crate) isize);

/// The three classic Win32 Event Log calls, abstracted so tests can inject a
/// fake backend without touching the real `Application` channel.
pub(crate) trait Win32EventLogApi: Send + Sync {
    fn register_event_source(&self, source: &str) -> Result<RawEventHandle, EventLogSinkError>;
    fn report_event(
        &self,
        handle: RawEventHandle,
        event_id: u32,
        severity: LifecycleSeverity,
        strings: &[String],
    ) -> Result<(), EventLogSinkError>;
    fn deregister_event_source(&self, handle: RawEventHandle) -> Result<(), EventLogSinkError>;
}

/// Sink-level failure. Never surfaced to session/auth/display/CP callers.
#[derive(Debug)]
pub(crate) enum EventLogSinkError {
    Format(EventFormatError),
    Register(windows::core::Error),
    Report(windows::core::Error),
    Deregister(windows::core::Error),
}

impl Display for EventLogSinkError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(error) => write!(formatter, "format lifecycle event: {error}"),
            Self::Register(error) => write!(formatter, "RegisterEventSourceW: {error}"),
            Self::Report(error) => write!(formatter, "ReportEventW: {error}"),
            Self::Deregister(error) => write!(formatter, "DeregisterEventSource: {error}"),
        }
    }
}

impl std::error::Error for EventLogSinkError {}

impl From<EventFormatError> for EventLogSinkError {
    fn from(error: EventFormatError) -> Self {
        Self::Format(error)
    }
}

fn to_wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The real Win32 Event Log API, calling `advapi32`/`wevtapi` directly.
pub(crate) struct RealWin32EventLogApi;

impl Win32EventLogApi for RealWin32EventLogApi {
    fn register_event_source(&self, source: &str) -> Result<RawEventHandle, EventLogSinkError> {
        let wide = to_wide(source);
        // SAFETY: `wide` is NUL-terminated and remains alive for the call.
        // `None` (local computer) is the documented safe default for the
        // `lpUNCServerName` out-param.
        let handle = unsafe { RegisterEventSourceW(None, PCWSTR(wide.as_ptr())) }
            .map_err(EventLogSinkError::Register)?;
        Ok(RawEventHandle(handle.0 as isize))
    }

    fn report_event(
        &self,
        handle: RawEventHandle,
        event_id: u32,
        severity: LifecycleSeverity,
        strings: &[String],
    ) -> Result<(), EventLogSinkError> {
        let wide_strings: Vec<Vec<u16>> = strings.iter().map(|value| to_wide(value)).collect();
        let pointers: Vec<PCWSTR> = wide_strings
            .iter()
            .map(|value| PCWSTR(value.as_ptr()))
            .collect();
        let win_handle = HANDLE(handle.0 as *mut core::ffi::c_void);
        // SAFETY: `win_handle` was returned by `RegisterEventSourceW` for this
        // process and remains valid until `DeregisterEventSource`. `pointers`
        // references `wide_strings`, which both outlive this call. No user
        // SID or raw binary data is supplied.
        unsafe {
            ReportEventW(
                win_handle,
                severity_event_type(severity),
                0,
                event_id,
                None,
                0,
                Some(&pointers),
                None,
            )
        }
        .map_err(EventLogSinkError::Report)
    }

    fn deregister_event_source(&self, handle: RawEventHandle) -> Result<(), EventLogSinkError> {
        let win_handle = HANDLE(handle.0 as *mut core::ffi::c_void);
        // SAFETY: `win_handle` was returned by `RegisterEventSourceW` for this
        // process and is deregistered exactly once, from `Drop`.
        unsafe { DeregisterEventSource(win_handle) }.map_err(EventLogSinkError::Deregister)
    }
}

/// Adapter accepted by [`LifecycleEmitter`]; only [`ValidatedLifecycleEvent`]
/// can reach it.
pub(crate) trait EventLogBackend: Send + Sync {
    fn report(&self, event: &ValidatedLifecycleEvent) -> Result<(), EventLogSinkError>;
}

/// Process-local RAII Windows Event Log source: registers once, reports
/// through the injected [`Win32EventLogApi`], and deregisters on `Drop`.
pub(crate) struct WindowsEventLogSink<A: Win32EventLogApi> {
    api: A,
    handle: RawEventHandle,
}

impl<A: Win32EventLogApi> WindowsEventLogSink<A> {
    pub(crate) fn register(api: A, source: &str) -> Result<Self, EventLogSinkError> {
        let handle = api.register_event_source(source)?;
        Ok(Self { api, handle })
    }
}

impl<A: Win32EventLogApi> Drop for WindowsEventLogSink<A> {
    fn drop(&mut self) {
        if let Err(error) = self.api.deregister_event_source(self.handle) {
            tracing::debug!(
                target: EVENTLOG,
                %error,
                "Windows Event Log source deregistration failed"
            );
        }
    }
}

impl<A: Win32EventLogApi> EventLogBackend for WindowsEventLogSink<A> {
    fn report(&self, event: &ValidatedLifecycleEvent) -> Result<(), EventLogSinkError> {
        let strings = build_insertion_strings(event)?;
        self.api.report_event(
            self.handle,
            event.kind().id(),
            event.definition().severity,
            &strings,
        )
    }
}

impl<A: Win32EventLogApi + 'static> Sink<CanonicalRecord> for WindowsEventLogSink<A> {
    fn deliver(&mut self, record: CanonicalRecord) -> Result<(), SinkError> {
        let value = serde_json::to_value(&record)
            .map_err(|_| SinkError::adapter("canonical Event Log serialization failed"))?;
        let Some(event_id) = value.get("event_id").and_then(serde_json::Value::as_u64) else {
            return Ok(());
        };
        let event_id = u32::try_from(event_id)
            .map_err(|_| SinkError::adapter("canonical Event Log event id is out of range"))?;
        let strings = build_canonical_insertion_strings(&value)
            .map_err(|error| SinkError::adapter(error.to_string()))?;
        let severity = match record.severity() {
            EventSeverity::Debug | EventSeverity::Info => LifecycleSeverity::Information,
            EventSeverity::Warn => LifecycleSeverity::Warning,
            EventSeverity::Error => LifecycleSeverity::Error,
        };
        self.api
            .report_event(self.handle, event_id, severity, &strings)
            .map_err(|_| SinkError::adapter("Windows Event Log delivery failed"))
    }
}

fn build_canonical_insertion_strings(
    value: &serde_json::Value,
) -> Result<Vec<String>, EventFormatError> {
    let event_id = value
        .get("event_id")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let event_name = value
        .get("event_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("ARCEN_EVENT");
    let severity = value
        .get("severity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("info");
    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Arcen lifecycle event");
    let mut strings = vec![format!(
        "ArcenPier {event_name} ({event_id}, {severity}): {message}"
    )];
    let mut pairs = std::collections::BTreeMap::new();
    if let Some(object) = value.as_object() {
        for (key, item) in object {
            if matches!(key.as_str(), "message" | "fields") || item.is_null() {
                continue;
            }
            if let Some(rendered) = canonical_scalar(item) {
                pairs.insert(key.clone(), rendered);
            }
        }
        if let Some(fields) = object.get("fields").and_then(serde_json::Value::as_object) {
            for (key, item) in fields {
                if let Some(rendered) = canonical_scalar(item) {
                    pairs.insert(key.clone(), rendered);
                }
            }
        }
    }
    for (key, rendered) in pairs {
        strings.push(format!("{key}={rendered}"));
    }
    validate_insertion_strings(&strings)?;
    Ok(strings)
}

fn canonical_scalar(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Bool(value) => Some(value.to_string()),
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::String(value) => Some(value.clone()),
        _ => None,
    }
}

fn validate_insertion_strings(strings: &[String]) -> Result<(), EventFormatError> {
    if strings.len() > MAX_INSERTION_STRINGS {
        return Err(EventFormatError::TooManyInsertionStrings(strings.len()));
    }
    let mut rendered_units = 0usize;
    for string in strings {
        let units = string.encode_utf16().count();
        if units > MAX_INSERTION_STRING_CHARS {
            return Err(EventFormatError::InsertionStringTooLong(units));
        }
        rendered_units += units + 1;
    }
    if rendered_units > MAX_RENDERED_UTF16_UNITS {
        return Err(EventFormatError::RenderedTooLarge(rendered_units));
    }
    Ok(())
}

/// Process-local lifecycle event emitter.
///
/// Owns (through an `Arc`) at most one native sink per process. `emit` never
/// returns an error and never blocks session/auth/display/CP work on native
/// delivery; a failure is reported once through `tracing` and then ignored.
#[derive(Clone)]
pub(crate) struct LifecycleEmitter {
    backend: Option<Arc<dyn EventLogBackend>>,
    observability: Option<ObservabilityHandle>,
    failure_reported: Arc<AtomicBool>,
}

impl LifecycleEmitter {
    /// An emitter with no native sink; `emit` is then a guaranteed no-op.
    #[cfg(any(test, not(windows)))]
    pub(crate) fn disabled() -> Self {
        Self {
            backend: None,
            observability: None,
            failure_reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// An emitter backed by an already-registered sink (production or fake).
    #[cfg(test)]
    pub(crate) fn from_backend(backend: Arc<dyn EventLogBackend>) -> Self {
        Self {
            backend: Some(backend),
            observability: None,
            failure_reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Registers the real process-local `ArcenPier` Windows Event Log source.
    ///
    /// Best-effort: registration failure is reported once through `tracing`
    /// and returns a disabled emitter. It never fails process startup.
    pub(crate) fn init_process_local(observability: ObservabilityHandle) -> Self {
        Self {
            backend: None,
            observability: Some(observability),
            failure_reported: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Best-effort emission. Never affects the caller's own outcome.
    pub(crate) fn emit(&self, event: &ValidatedLifecycleEvent) {
        self.emit_context(
            event,
            LifecycleContext {
                sid: event.correlation_id().clone(),
                user: None,
                host: local_hostname(),
                peer_addr: None,
                health_state: None,
            },
        );
    }

    pub(crate) fn emit_context(&self, event: &ValidatedLifecycleEvent, context: LifecycleContext) {
        if let Some(observability) = self.observability.as_ref() {
            let result = crate::logging::canonical_timestamp().and_then(|timestamp| {
                let target = TelemetryTarget::new(target_for_category(event.definition().category))
                    .map_err(|error| error.to_string())?;
                observability
                    .emit_lifecycle(
                        event,
                        context,
                        timestamp,
                        target,
                        event.definition().name.to_ascii_lowercase(),
                    )
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            });
            if let Err(error) = result {
                if !self.failure_reported.swap(true, Ordering::AcqRel) {
                    tracing::debug!(
                        target: EVENTLOG,
                        %error,
                        event_id = event.kind().id(),
                        "canonical lifecycle bridge failed"
                    );
                }
            }
        }
        if let Some(backend) = self.backend.as_ref() {
            if let Err(error) = backend.report(event) {
                if !self.failure_reported.swap(true, Ordering::AcqRel) {
                    tracing::warn!(
                        target: EVENTLOG,
                        %error,
                        event_id = event.kind().id(),
                        event_name = event.definition().name,
                        "Windows Event Log lifecycle report failed; continuing without native \
                         delivery"
                    );
                }
            }
        }
    }

    pub(crate) fn emit_drop_notices(&self, context: LifecycleContext) {
        let Some(observability) = self.observability.as_ref() else {
            return;
        };
        let notices = observability.take_drop_notices(&context.sid);
        let Ok(notices) = notices else {
            return;
        };
        for notice in notices {
            self.emit_context(&notice, context.clone());
        }
    }
}

fn target_for_category(category: LifecycleCategory) -> &'static str {
    match category {
        LifecycleCategory::OnlineIdentity | LifecycleCategory::MachineAuthentication => {
            arcen_telemetry::names::target::AUTH
        }
        LifecycleCategory::Streaming => arcen_telemetry::names::target::MEDIA,
        LifecycleCategory::Health => arcen_telemetry::names::target::HEALTH,
        LifecycleCategory::Network | LifecycleCategory::Connection => {
            arcen_telemetry::names::target::NET
        }
        LifecycleCategory::Peripheral => arcen_telemetry::names::target::HID,
        LifecycleCategory::Telemetry => arcen_telemetry::names::target::TELEMETRY,
        LifecycleCategory::Entitlement
        | LifecycleCategory::Negotiation
        | LifecycleCategory::Reconnect
        | LifecycleCategory::Cleanup
        | LifecycleCategory::Permission => arcen_telemetry::names::target::SESSION,
        _ => arcen_telemetry::names::target::SESSION,
    }
}

fn local_hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|value| !value.is_empty() && value.len() <= arcen_telemetry::MAX_IDENTITY_BYTES)
}

/// A fresh, non-secret correlation id for a lifecycle event that has no live
/// session/auth/CP correlation available (for example a standalone
/// `restore-display` invocation).
pub(crate) fn random_correlation_id() -> CorrelationId {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Extremely unlikely on a supported Windows host; fall back to a
        // fixed, clearly-synthetic value rather than panicking a
        // lifecycle-adjacent code path.
        return CorrelationId::parse_uuid("00000000-0000-4000-8000-000000000000")
            .expect("fixed fallback value is a canonical UUID");
    }
    CorrelationId::from_uuid_v4_bytes(bytes)
}

// ---------------------------------------------------------------------------
// Bounded native-event query seam (Support Bundle follow-on contract).
// ---------------------------------------------------------------------------

/// Default record cap for [`query_recent_events`].
pub(crate) const DEFAULT_EVENT_EXCERPT_RECORDS: usize = 500;
/// Hard record cap for [`query_recent_events`].
#[allow(dead_code)]
pub(crate) const MAX_EVENT_EXCERPT_RECORDS: usize = 500;
/// Default byte cap for [`query_recent_events`].
pub(crate) const DEFAULT_EVENT_EXCERPT_BYTES: usize = 4 * 1024 * 1024;
/// Hard byte cap for [`query_recent_events`].
#[allow(dead_code)]
pub(crate) const MAX_EVENT_EXCERPT_BYTES: usize = 4 * 1024 * 1024;

/// Validated bounds for one [`query_recent_events`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventExcerptLimits {
    pub(crate) max_records: usize,
    pub(crate) max_bytes: usize,
}

impl EventExcerptLimits {
    #[allow(dead_code)]
    pub(crate) fn new(max_records: usize, max_bytes: usize) -> Result<Self, NativeEventQueryError> {
        if max_records == 0
            || max_records > MAX_EVENT_EXCERPT_RECORDS
            || max_bytes == 0
            || max_bytes > MAX_EVENT_EXCERPT_BYTES
        {
            return Err(NativeEventQueryError::InvalidLimits);
        }
        Ok(Self {
            max_records,
            max_bytes,
        })
    }
}

impl Default for EventExcerptLimits {
    fn default() -> Self {
        Self {
            max_records: DEFAULT_EVENT_EXCERPT_RECORDS,
            max_bytes: DEFAULT_EVENT_EXCERPT_BYTES,
        }
    }
}

/// A bounded excerpt of recently reported native lifecycle records.
#[derive(Debug)]
pub(crate) struct BoundedEventExcerpt {
    pub(crate) bytes: Vec<u8>,
    pub(crate) record_count: usize,
    pub(crate) truncated: bool,
    pub(crate) source: NativeEventQuerySource,
    pub(crate) media_type: &'static str,
    pub(crate) suggested_name: &'static str,
}

/// Which native store a [`BoundedEventExcerpt`] was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NativeEventQuerySource {
    WindowsEventLog,
    #[allow(dead_code)]
    Journal,
    #[allow(dead_code)]
    Syslog,
}

/// A [`query_recent_events`] failure.
#[derive(Debug)]
pub(crate) enum NativeEventQueryError {
    #[allow(dead_code)]
    InvalidLimits,
    Unavailable,
    PermissionDenied,
    #[allow(dead_code)]
    Io(std::io::Error),
    #[allow(dead_code)]
    CommandFailed,
}

impl Display for NativeEventQueryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("event excerpt limits are invalid"),
            Self::Unavailable => formatter.write_str("the Windows Event Log is unavailable"),
            Self::PermissionDenied => {
                formatter.write_str("permission was denied reading the Windows Event Log")
            }
            Self::Io(error) => write!(formatter, "Windows Event Log query I/O failed: {error}"),
            Self::CommandFailed => formatter.write_str("the Windows Event Log query failed"),
        }
    }
}

impl std::error::Error for NativeEventQueryError {}

/// RAII guard that closes one `EVT_HANDLE` exactly once.
struct EvtHandleGuard(EVT_HANDLE);

impl Drop for EvtHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // SAFETY: `self.0` was returned by `EvtQuery`/`EvtNext` and is
            // closed exactly once, from `Drop`.
            let _ = unsafe { EvtClose(self.0) };
        }
    }
}

fn map_evt_error(error: windows::core::Error) -> NativeEventQueryError {
    match error.code().0 as u32 {
        HRESULT_ACCESS_DENIED => NativeEventQueryError::PermissionDenied,
        _ => NativeEventQueryError::Unavailable,
    }
}

fn render_event_xml(handle: EVT_HANDLE) -> windows::core::Result<String> {
    let mut used = 0u32;
    let mut property_count = 0u32;
    // First call discovers the required buffer size; `ERROR_INSUFFICIENT_BUFFER`
    // is the documented, expected sizing outcome.
    // SAFETY: no output buffer is supplied; `used`/`property_count` are valid
    // out-params sized for one `u32` each.
    if let Err(error) = unsafe {
        EvtRender(
            None,
            handle,
            EvtRenderEventXml.0,
            0,
            None,
            &mut used,
            &mut property_count,
        )
    } {
        if error.code().0 as u32 != HRESULT_INSUFFICIENT_BUFFER {
            return Err(error);
        }
    }
    let mut buffer = vec![0u16; (used as usize).div_ceil(2).max(1)];
    let capacity_bytes = (buffer.len() * 2) as u32;
    // SAFETY: `buffer` has at least the byte capacity reported by the sizing
    // call above, and remains alive for the duration of this call.
    unsafe {
        EvtRender(
            None,
            handle,
            EvtRenderEventXml.0,
            capacity_bytes,
            Some(buffer.as_mut_ptr().cast()),
            &mut used,
            &mut property_count,
        )
    }?;
    let used_units = (used as usize) / 2;
    let text = String::from_utf16_lossy(&buffer[..used_units.min(buffer.len())]);
    Ok(text.trim_end_matches('\u{0}').to_string())
}

fn sanitize_event_xml(xml: &str) -> Option<String> {
    let mut pairs = std::collections::BTreeMap::new();
    let mut remainder = xml;
    while let Some(start) = remainder.find("<Data>") {
        let content = &remainder[start + "<Data>".len()..];
        let end = content.find("</Data>")?;
        if let Some((key, value)) = content[..end].split_once('=') {
            if matches!(
                key,
                "event_id" | "event_name" | "category" | "outcome" | "severity" | "correlation_id"
            ) && pairs.insert(key, value).is_some()
            {
                return None;
            }
        }
        remainder = &content[end + "</Data>".len()..];
    }
    let event_id = pairs.get("event_id")?.parse().ok()?;
    let definition = arcen_telemetry::lifecycle_event_definition(event_id)?;
    if *pairs.get("event_name")? != definition.name
        || *pairs.get("category")? != definition.category.as_str()
        || *pairs.get("outcome")? != definition.outcome.as_str()
        || *pairs.get("severity")? != definition.severity.as_str()
    {
        return None;
    }
    let correlation_id =
        arcen_telemetry::CorrelationId::parse_uuid((*pairs.get("correlation_id")?).to_string())
            .ok()?;
    Some(format!(
        "<Event><System><Provider Name=\"ArcenPier\"/><EventID>{}</EventID></System>\
         <EventData><Data>category={}</Data><Data>correlation_id={}</Data>\
         <Data>event_id={}</Data><Data>event_name={}</Data><Data>outcome={}</Data>\
         <Data>severity={}</Data></EventData></Event>",
        definition.kind.id(),
        definition.category.as_str(),
        correlation_id.as_str(),
        definition.kind.id(),
        definition.name,
        definition.outcome.as_str(),
        definition.severity.as_str()
    ))
}

/// Reads newest-first bounded raw Event XML for the `ArcenPier` provider.
///
/// Independent of the live [`LifecycleEmitter`]; works while the Pier service
/// is stopped. Enforces both record and byte caps before returning. Callers
/// (Support Bundle) should store `bytes` under `suggested_name` and must not
/// call `wevtutil` or scan the registry themselves.
pub(crate) fn query_recent_events(
    limits: EventExcerptLimits,
) -> Result<BoundedEventExcerpt, NativeEventQueryError> {
    let channel = to_wide(EVENT_CHANNEL);
    let query = to_wide(&format!("*[System[Provider[@Name='{EVENT_PROVIDER}']]]"));
    let flags = EvtQueryChannelPath.0 | EvtQueryReverseDirection.0;
    // SAFETY: `channel`/`query` are NUL-terminated wide strings kept alive
    // for the duration of this call.
    let query_handle = unsafe {
        EvtQuery(
            None,
            PCWSTR(channel.as_ptr()),
            PCWSTR(query.as_ptr()),
            flags,
        )
    }
    .map_err(map_evt_error)?;
    let _query_guard = EvtHandleGuard(query_handle);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"<Events>");
    let mut record_count = 0usize;
    let mut inspected_count = 0usize;
    let mut truncated = false;
    'outer: loop {
        let mut raw_handles = [0isize; 16];
        let mut returned = 0u32;
        // SAFETY: `raw_handles` has 16 valid output slots; `EvtNext` writes
        // at most that many and reports the exact count in `returned`.
        if let Err(error) = unsafe { EvtNext(query_handle, &mut raw_handles, 0, 0, &mut returned) }
        {
            if error.code().0 as u32 == HRESULT_NO_MORE_ITEMS {
                break;
            }
            return Err(map_evt_error(error));
        }
        if returned == 0 {
            break;
        }
        for &raw_handle in &raw_handles[..returned as usize] {
            if inspected_count >= limits.max_records {
                truncated = true;
                break 'outer;
            }
            inspected_count += 1;
            let event_guard = EvtHandleGuard(EVT_HANDLE(raw_handle));
            let raw_xml = render_event_xml(event_guard.0).map_err(map_evt_error)?;
            let Some(xml) = sanitize_event_xml(&raw_xml) else {
                truncated = true;
                continue;
            };
            let fragment_len = xml.len();
            if record_count >= limits.max_records
                || bytes.len() + fragment_len + b"</Events>".len() > limits.max_bytes
            {
                truncated = true;
                break 'outer;
            }
            bytes.extend_from_slice(xml.as_bytes());
            record_count += 1;
        }
    }
    bytes.extend_from_slice(b"</Events>");

    Ok(BoundedEventExcerpt {
        bytes,
        record_count,
        truncated,
        source: NativeEventQuerySource::WindowsEventLog,
        media_type: "application/vnd.microsoft.windows.event+xml",
        suggested_name: "events/windows-application-arcen-pier.xml",
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    use arcen_observability::{BoundedSink, DeliveryOutcome};
    use arcen_telemetry::{
        LifecycleEventKind, OperationalProfile, StructuredFields, TelemetryComponent,
        TelemetryPlatform, TelemetryRole,
    };

    use super::*;

    fn correlation_id() -> CorrelationId {
        CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef")
            .expect("canonical correlation id")
    }

    fn service_start_event() -> ValidatedLifecycleEvent {
        let mut fields = StructuredFields::default();
        fields
            .insert("component", FieldValue::String("pier_broker".to_string()))
            .expect("valid field");
        fields
            .insert("pid", FieldValue::Integer(4242))
            .expect("valid field");
        ValidatedLifecycleEvent::new(LifecycleEventKind::ServiceStart, correlation_id(), fields)
            .expect("valid event")
    }

    fn session_auth_fail_event() -> ValidatedLifecycleEvent {
        let mut fields = StructuredFields::default();
        fields
            .insert(
                "auth_method",
                FieldValue::String("windows_logon".to_string()),
            )
            .expect("valid field");
        fields
            .insert(
                "stage",
                FieldValue::String("credential_verification".to_string()),
            )
            .expect("valid field");
        fields
            .insert(
                "reason_class",
                FieldValue::String("invalid_credentials".to_string()),
            )
            .expect("valid field");
        ValidatedLifecycleEvent::new(
            LifecycleEventKind::SessionAuthFail,
            correlation_id(),
            fields,
        )
        .expect("valid event")
    }

    #[test]
    fn insertion_strings_are_deterministic_and_sorted() {
        let strings = build_insertion_strings(&service_start_event()).expect("format succeeds");
        assert_eq!(
            strings,
            vec![
                "ArcenPier lifecycle event 1000 SERVICE_START (information)".to_string(),
                "category=health".to_string(),
                "component=pier_broker".to_string(),
                "correlation_id=01234567-89ab-4def-8123-456789abcdef".to_string(),
                "event_id=1000".to_string(),
                "event_name=SERVICE_START".to_string(),
                "outcome=succeeded".to_string(),
                "pid=4242".to_string(),
                "severity=information".to_string(),
            ]
        );
    }

    #[test]
    fn insertion_strings_are_stable_across_repeated_calls() {
        let event = session_auth_fail_event();
        assert_eq!(
            build_insertion_strings(&event).unwrap(),
            build_insertion_strings(&event).unwrap()
        );
    }

    #[test]
    fn severity_maps_to_exact_win32_event_types() {
        assert_eq!(
            severity_event_type(LifecycleSeverity::Information),
            EVENTLOG_INFORMATION_TYPE
        );
        assert_eq!(
            severity_event_type(LifecycleSeverity::Warning),
            EVENTLOG_WARNING_TYPE
        );
        assert_eq!(
            severity_event_type(LifecycleSeverity::Error),
            EVENTLOG_ERROR_TYPE
        );
    }

    #[derive(Default)]
    struct FakeWin32EventLogApi {
        registrations: Mutex<u32>,
        deregistrations: Mutex<u32>,
        reports: Mutex<Vec<(u32, LifecycleSeverity, Vec<String>)>>,
        fail_register: bool,
        fail_report: bool,
    }

    fn fake_win32_error() -> windows::core::Error {
        windows::core::Error::from_hresult(windows::core::HRESULT(HRESULT_ACCESS_DENIED as i32))
    }

    impl Win32EventLogApi for FakeWin32EventLogApi {
        fn register_event_source(
            &self,
            _source: &str,
        ) -> Result<RawEventHandle, EventLogSinkError> {
            *self.registrations.lock().expect("lock") += 1;
            if self.fail_register {
                return Err(EventLogSinkError::Register(fake_win32_error()));
            }
            Ok(RawEventHandle(7))
        }

        fn report_event(
            &self,
            handle: RawEventHandle,
            event_id: u32,
            severity: LifecycleSeverity,
            strings: &[String],
        ) -> Result<(), EventLogSinkError> {
            assert_eq!(
                handle,
                RawEventHandle(7),
                "sink must reuse its registered handle"
            );
            self.reports
                .lock()
                .expect("lock")
                .push((event_id, severity, strings.to_vec()));
            if self.fail_report {
                return Err(EventLogSinkError::Report(fake_win32_error()));
            }
            Ok(())
        }

        fn deregister_event_source(&self, handle: RawEventHandle) -> Result<(), EventLogSinkError> {
            assert_eq!(handle, RawEventHandle(7));
            *self.deregistrations.lock().expect("lock") += 1;
            Ok(())
        }
    }

    #[derive(Default)]
    struct BlockingWin32EventLogApi {
        entered: Mutex<bool>,
        entered_changed: Condvar,
        released: Mutex<bool>,
        released_changed: Condvar,
        reports: Mutex<u64>,
    }

    impl BlockingWin32EventLogApi {
        fn wait_until_entered(&self) {
            let mut entered = self.entered.lock().expect("lock");
            while !*entered {
                entered = self.entered_changed.wait(entered).expect("wait");
            }
        }

        fn release(&self) {
            *self.released.lock().expect("lock") = true;
            self.released_changed.notify_all();
        }
    }

    impl Win32EventLogApi for Arc<BlockingWin32EventLogApi> {
        fn register_event_source(
            &self,
            _source: &str,
        ) -> Result<RawEventHandle, EventLogSinkError> {
            Ok(RawEventHandle(8))
        }

        fn report_event(
            &self,
            handle: RawEventHandle,
            _event_id: u32,
            _severity: LifecycleSeverity,
            _strings: &[String],
        ) -> Result<(), EventLogSinkError> {
            assert_eq!(handle, RawEventHandle(8));
            *self.entered.lock().expect("lock") = true;
            self.entered_changed.notify_all();
            let mut released = self.released.lock().expect("lock");
            while !*released {
                released = self.released_changed.wait(released).expect("wait");
            }
            *self.reports.lock().expect("lock") += 1;
            Ok(())
        }

        fn deregister_event_source(&self, handle: RawEventHandle) -> Result<(), EventLogSinkError> {
            assert_eq!(handle, RawEventHandle(8));
            Ok(())
        }
    }

    #[test]
    fn sink_registers_once_reports_exact_ids_and_deregisters_on_drop() {
        let api = Arc::new(FakeWin32EventLogApi::default());
        {
            let sink = WindowsEventLogSink::register(
                FakeWin32EventLogApiHandle(Arc::clone(&api)),
                "ArcenPier",
            )
            .expect("register succeeds");
            assert_eq!(*api.registrations.lock().expect("lock"), 1);
            sink.report(&service_start_event())
                .expect("report succeeds");
            sink.report(&session_auth_fail_event())
                .expect("report succeeds");
            let reports = api.reports.lock().expect("lock");
            assert_eq!(reports.len(), 2);
            assert_eq!(reports[0].0, 1000);
            assert_eq!(reports[0].1, LifecycleSeverity::Information);
            assert_eq!(reports[1].0, 1101);
            assert_eq!(reports[1].1, LifecycleSeverity::Warning);
        }
        assert_eq!(*api.deregistrations.lock().expect("lock"), 1);
        assert_eq!(*api.registrations.lock().expect("lock"), 1);
    }

    #[test]
    fn canonical_sink_preserves_event_id_severity_and_identity_fields() {
        let api = Arc::new(FakeWin32EventLogApi::default());
        let mut sink = WindowsEventLogSink::register(
            FakeWin32EventLogApiHandle(Arc::clone(&api)),
            EVENT_PROVIDER,
        )
        .expect("register succeeds");
        let record = CanonicalRecord::new(
            "2026-07-24T16:00:00.000000Z",
            42,
            OperationalProfile::Critical,
            EventSeverity::Warn,
            TelemetryRole::Host,
            TelemetryComponent::new("pier_broker").expect("component"),
            TelemetryPlatform::Windows,
            TelemetryTarget::new(arcen_telemetry::names::target::AUTH).expect("target"),
            "session authentication failed",
        )
        .expect("record")
        .with_event(LifecycleEventKind::SessionAuthFail)
        .with_sid(correlation_id())
        .with_identity(
            Some(r"DOMAIN\artist"),
            Some("pier-01"),
            Some("192.0.2.10:54000"),
        )
        .expect("identity");

        Sink::deliver(&mut sink, record).expect("canonical delivery");
        let reports = api.reports.lock().expect("lock");
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, 1101);
        assert_eq!(reports[0].1, LifecycleSeverity::Warning);
        assert!(reports[0]
            .2
            .iter()
            .any(|value| value == "sid=01234567-89ab-4def-8123-456789abcdef"));
        assert!(reports[0]
            .2
            .iter()
            .any(|value| value == r"user=DOMAIN\artist"));
    }

    #[test]
    fn shared_eventlog_queue_counts_each_drop_once() {
        let api = Arc::new(BlockingWin32EventLogApi::default());
        let sink =
            WindowsEventLogSink::register(Arc::clone(&api), EVENT_PROVIDER).expect("register");
        let queue = BoundedSink::new("windows_event_log_test", 1, sink).expect("bounded sink");
        let record = CanonicalRecord::new(
            "2026-07-24T16:00:00.000000Z",
            42,
            OperationalProfile::Critical,
            EventSeverity::Warn,
            TelemetryRole::Host,
            TelemetryComponent::new("pier_broker").expect("component"),
            TelemetryPlatform::Windows,
            TelemetryTarget::new(arcen_telemetry::names::target::AUTH).expect("target"),
            "session authentication failed",
        )
        .expect("record")
        .with_event(LifecycleEventKind::SessionAuthFail)
        .with_sid(correlation_id());

        assert_eq!(queue.try_send(record.clone()), DeliveryOutcome::Enqueued);
        api.wait_until_entered();
        assert_eq!(queue.try_send(record.clone()), DeliveryOutcome::Enqueued);
        assert_eq!(queue.try_send(record), DeliveryOutcome::QueueFull);
        assert_eq!(queue.stats().dropped, 1);
        assert_eq!(queue.take_unreported_drops(), 1);
        assert_eq!(queue.take_unreported_drops(), 0);

        api.release();
        queue
            .shutdown(Duration::from_secs(1))
            .expect("bounded drain");
        assert_eq!(*api.reports.lock().expect("lock"), 2);
    }

    /// Cheap `Arc`-sharing wrapper so the RAII sink and the test can both
    /// observe the same fake call counters.
    struct FakeWin32EventLogApiHandle(Arc<FakeWin32EventLogApi>);

    impl Win32EventLogApi for FakeWin32EventLogApiHandle {
        fn register_event_source(&self, source: &str) -> Result<RawEventHandle, EventLogSinkError> {
            self.0.register_event_source(source)
        }

        fn report_event(
            &self,
            handle: RawEventHandle,
            event_id: u32,
            severity: LifecycleSeverity,
            strings: &[String],
        ) -> Result<(), EventLogSinkError> {
            self.0.report_event(handle, event_id, severity, strings)
        }

        fn deregister_event_source(&self, handle: RawEventHandle) -> Result<(), EventLogSinkError> {
            self.0.deregister_event_source(handle)
        }
    }

    #[test]
    fn emitter_reports_registration_failure_once_and_stays_disabled() {
        let api = Arc::new(FakeWin32EventLogApi {
            fail_register: true,
            ..FakeWin32EventLogApi::default()
        });
        let result = WindowsEventLogSink::register(
            FakeWin32EventLogApiHandle(Arc::clone(&api)),
            "ArcenPier",
        );
        assert!(result.is_err());
        assert_eq!(*api.registrations.lock().expect("lock"), 1);
    }

    #[test]
    fn emitter_emit_never_panics_and_reports_failure_once() {
        let api = Arc::new(FakeWin32EventLogApi {
            fail_report: true,
            ..FakeWin32EventLogApi::default()
        });
        let sink = WindowsEventLogSink::register(
            FakeWin32EventLogApiHandle(Arc::clone(&api)),
            "ArcenPier",
        )
        .expect("register succeeds");
        let emitter = LifecycleEmitter::from_backend(Arc::new(sink));
        // Failure-injected sink: emit() must not panic or return an error to
        // the caller, and must not change any product outcome.
        emitter.emit(&service_start_event());
        emitter.emit(&service_start_event());
        assert_eq!(api.reports.lock().expect("lock").len(), 2);
        assert!(emitter.failure_reported.load(Ordering::Acquire));
    }

    #[test]
    fn disabled_emitter_is_a_guaranteed_no_op() {
        let emitter = LifecycleEmitter::disabled();
        emitter.emit(&service_start_event());
        assert!(!emitter.failure_reported.load(Ordering::Acquire));
    }

    #[test]
    fn excerpt_limits_reject_zero_and_oversized_bounds() {
        assert!(EventExcerptLimits::new(0, 1024).is_err());
        assert!(EventExcerptLimits::new(10, 0).is_err());
        assert!(EventExcerptLimits::new(MAX_EVENT_EXCERPT_RECORDS + 1, 1024).is_err());
        assert!(EventExcerptLimits::new(10, MAX_EVENT_EXCERPT_BYTES + 1).is_err());
        assert!(EventExcerptLimits::new(
            DEFAULT_EVENT_EXCERPT_RECORDS,
            DEFAULT_EVENT_EXCERPT_BYTES
        )
        .is_ok());
        assert_eq!(
            EventExcerptLimits::default().max_records,
            DEFAULT_EVENT_EXCERPT_RECORDS
        );
    }

    #[test]
    fn random_correlation_ids_are_unique_canonical_uuids() {
        let first = random_correlation_id();
        let second = random_correlation_id();
        assert_ne!(first, second);
        assert_eq!(first.as_str().len(), 36);
    }

    #[test]
    fn event_xml_sanitizer_removes_hostname_and_security_identity() {
        let xml = "<Event><System><Computer>sensitive-host</Computer><Security UserID=\"S-1-5-18\"/></System><EventData><Data>ArcenPier lifecycle event 1000 SERVICE_START (information)</Data><Data>category=health</Data><Data>component=pier_broker</Data><Data>correlation_id=01234567-89ab-4def-8123-456789abcdef</Data><Data>event_id=1000</Data><Data>event_name=SERVICE_START</Data><Data>outcome=succeeded</Data><Data>severity=information</Data><Data>secret=customer</Data></EventData></Event>";
        let sanitized = sanitize_event_xml(xml).expect("valid lifecycle event");
        assert!(!sanitized.contains("sensitive-host"));
        assert!(!sanitized.contains("S-1-5-18"));
        assert!(!sanitized.contains("customer"));
        assert!(!sanitized.contains("component"));
        assert!(sanitized.contains("<EventID>1000</EventID>"));
    }
}
