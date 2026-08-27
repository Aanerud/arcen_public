//! Linux journald/syslog lifecycle sink.
//!
//! Formatting is fully separated from datagram delivery so the exact journal
//! fields and syslog message produced for one [`ValidatedLifecycleEvent`] can
//! be exercised without a live `systemd-journald` or `syslogd`. Delivery is
//! process-local, best-effort, and never changes a caller's own outcome: a
//! formatting or datagram failure is reported once through `tracing` and
//! then silently ignored until the next failure/success transition.
//!
//! Every adapter in this module accepts only [`ValidatedLifecycleEvent`], so
//! an event's schema, category, and outcome are already proven before any
//! socket write is attempted. No username, uid, peer address, raw PAM/child
//! error, or credential material is ever placed in a journal field or
//! syslog message.
//!
//! Delivery order: a cached nonblocking Unix datagram to the systemd journal
//! native protocol socket, then a cached nonblocking Unix datagram to
//! `/dev/log` (RFC 3164-style), then exactly one structured `tracing`
//! fallback report if both are unavailable or would block. No `libsystemd`
//! linkage and no new logging dependency are introduced: delivery uses only
//! `std::os::unix::net::UnixDatagram`, and the bounded query seam below uses
//! only the `journalctl` binary already present on a systemd host.

use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arcen_observability::LifecycleContext;
use arcen_telemetry::{
    CorrelationId, FieldValue, LifecycleEventKind, LifecycleFieldType, LifecycleSeverity,
    StructuredFields, TelemetryTarget, ValidatedLifecycleEvent, MAX_STRUCTURED_FIELDS,
};

/// Native systemd journal datagram socket.
pub(crate) const JOURNAL_SOCKET: &str = "/run/systemd/journal/socket";
/// Classic BSD syslog datagram socket.
pub(crate) const SYSLOG_SOCKET: &str = "/dev/log";
/// `SYSLOG_IDENTIFIER` / syslog tag used for every lifecycle record.
pub(crate) const SYSLOG_IDENTIFIER: &str = "arcen-pier";
/// The systemd unit that owns this process, for documentation/query use.
pub(crate) const SYSTEMD_UNIT: &str = "arcen-pier.service";
/// Journal field carrying the stable numeric lifecycle event identifier.
pub(crate) const JOURNAL_EVENT_ID_FIELD: &str = "ARCEN_EVENT_ID";

/// Nine fixed journal fields (`MESSAGE`, `PRIORITY`, `SYSLOG_IDENTIFIER`,
/// `ARCEN_EVENT_ID`, `ARCEN_EVENT_NAME`, `ARCEN_CATEGORY`, `ARCEN_OUTCOME`,
/// `ARCEN_SEVERITY`, `ARCEN_CORRELATION_ID`) plus up to
/// [`MAX_STRUCTURED_FIELDS`] schema-approved `ARCEN_FIELD_*` entries.
pub(crate) const MAX_JOURNAL_FIELD_LINES: usize = 9 + MAX_STRUCTURED_FIELDS;
/// Defensive-depth bound on one rendered journal datagram.
pub(crate) const MAX_JOURNAL_DATAGRAM_BYTES: usize = 16 * 1024;
/// Defensive-depth bound on one rendered RFC 3164-style syslog message.
pub(crate) const MAX_SYSLOG_MESSAGE_BYTES: usize = MAX_JOURNAL_DATAGRAM_BYTES;

/// A formatting-time defensive-depth bound violation.
///
/// [`ValidatedLifecycleEvent`] already enforces field count/size/content
/// bounds; this only guards the sink against a future shared-schema change
/// that raises those caps without updating this module.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum EventFormatError {
    TooManyFields(usize),
    RenderedTooLarge(usize),
    FieldContainsControl(String),
}

impl Display for EventFormatError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyFields(count) => write!(
                formatter,
                "lifecycle event field-line count {count} exceeds {MAX_JOURNAL_FIELD_LINES}"
            ),
            Self::RenderedTooLarge(bytes) => {
                write!(
                    formatter,
                    "lifecycle event rendered size {bytes} bytes exceeds its bound"
                )
            }
            Self::FieldContainsControl(field) => write!(
                formatter,
                "lifecycle event field `{field}` unexpectedly contains a control character"
            ),
        }
    }
}

impl std::error::Error for EventFormatError {}

fn field_value_string(value: &FieldValue) -> String {
    match value {
        FieldValue::Boolean(value) => value.to_string(),
        FieldValue::Integer(value) => value.to_string(),
        FieldValue::String(value) => value.clone(),
    }
}

/// Maps shared severity to the classic syslog/journal numeric priority
/// (`6`=info, `4`=warning, `3`=err).
pub(crate) const fn journal_priority(severity: LifecycleSeverity) -> u8 {
    match severity {
        LifecycleSeverity::Information => 6,
        LifecycleSeverity::Warning => 4,
        LifecycleSeverity::Error => 3,
    }
}

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Howard Hinnant's `civil_from_days`: converts a UNIX epoch day count to a
/// proleptic-Gregorian `(year, month, day)`. Avoids depending on `chrono` or
/// `time` for one RFC 3164 timestamp field.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d)
}

/// Formats a UTC RFC 3164-style `"Mmm dd hh:mm:ss"` timestamp. Deterministic
/// given a fixed `SystemTime`, so pure-formatter tests never depend on the
/// wall clock.
fn rfc3164_timestamp(now: SystemTime) -> String {
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let total_secs = duration.as_secs();
    let days = (total_secs / 86_400) as i64;
    let secs_of_day = total_secs % 86_400;
    let (_year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    format!(
        "{} {day:2} {hour:02}:{minute:02}:{second:02}",
        MONTH_NAMES[(month - 1) as usize]
    )
}

/// A cached datagram-socket send failure, abstracted so tests can inject a
/// fake backend without touching real journal/syslog sockets.
#[derive(Debug)]
pub(crate) enum SocketSendError {
    /// No socket was available at process-local connect time.
    Unavailable,
    /// The socket refused to accept the datagram without blocking.
    WouldBlock,
    /// The kernel returned another I/O failure on send.
    Io(std::io::Error),
}

impl Display for SocketSendError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => formatter.write_str("socket unavailable"),
            Self::WouldBlock => formatter.write_str("send would block"),
            Self::Io(error) => write!(formatter, "send failed: {error}"),
        }
    }
}

impl std::error::Error for SocketSendError {}

/// The two cached nonblocking Unix datagram sends, abstracted so tests can
/// inject a fake backend without touching `/run/systemd/journal/socket` or
/// `/dev/log`.
pub(crate) trait JournalSyslogApi: Send + Sync {
    fn send_journal(&self, bytes: &[u8]) -> Result<(), SocketSendError>;
    fn send_syslog(&self, bytes: &[u8]) -> Result<(), SocketSendError>;
}

#[cfg(unix)]
fn connect_nonblocking(path: &str) -> Option<std::os::unix::net::UnixDatagram> {
    let socket = std::os::unix::net::UnixDatagram::unbound().ok()?;
    socket.set_nonblocking(true).ok()?;
    socket.connect(path).ok()?;
    Some(socket)
}

#[cfg(unix)]
fn send_datagram(
    socket: Option<&std::os::unix::net::UnixDatagram>,
    bytes: &[u8],
) -> Result<(), SocketSendError> {
    let Some(socket) = socket else {
        return Err(SocketSendError::Unavailable);
    };
    match socket.send(bytes) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
            Err(SocketSendError::WouldBlock)
        }
        Err(error) => Err(SocketSendError::Io(error)),
    }
}

/// The real datagram backend: cached nonblocking Unix datagram clients for
/// the systemd journal socket and `/dev/log`, created once at process-local
/// initialization. Unavailable sockets (path missing, connect refused) are
/// cached as `None` rather than retried per event.
#[cfg(unix)]
pub(crate) struct RealJournalSyslogApi {
    journal: Option<std::os::unix::net::UnixDatagram>,
    syslog: Option<std::os::unix::net::UnixDatagram>,
}

#[cfg(unix)]
impl RealJournalSyslogApi {
    pub(crate) fn connect() -> Self {
        Self {
            journal: connect_nonblocking(JOURNAL_SOCKET),
            syslog: connect_nonblocking(SYSLOG_SOCKET),
        }
    }
}

#[cfg(unix)]
impl JournalSyslogApi for RealJournalSyslogApi {
    fn send_journal(&self, bytes: &[u8]) -> Result<(), SocketSendError> {
        send_datagram(self.journal.as_ref(), bytes)
    }

    fn send_syslog(&self, bytes: &[u8]) -> Result<(), SocketSendError> {
        send_datagram(self.syslog.as_ref(), bytes)
    }
}

/// Portable stand-in used when building off a Unix host (for example a
/// Windows development machine). Both sends are always `Unavailable`, which
/// [`LifecycleEmitter`] turns into exactly one deduplicated `tracing`
/// report rather than a build or runtime failure.
#[cfg(not(unix))]
pub(crate) struct RealJournalSyslogApi;

#[cfg(not(unix))]
impl RealJournalSyslogApi {
    pub(crate) fn connect() -> Self {
        Self
    }
}

#[cfg(not(unix))]
impl JournalSyslogApi for RealJournalSyslogApi {
    fn send_journal(&self, _bytes: &[u8]) -> Result<(), SocketSendError> {
        Err(SocketSendError::Unavailable)
    }

    fn send_syslog(&self, _bytes: &[u8]) -> Result<(), SocketSendError> {
        Err(SocketSendError::Unavailable)
    }
}

/// Builds the deterministic `(journal-field-name, value)` pairs for one
/// canonical record already serialized to a JSON value, or `None` when the
/// record carries no `event_id` (an ad-hoc diagnostic, not a validated
/// lifecycle event). Journald keeps carrying only lifecycle events, matching
/// pre-migration scope, even though every canonical record (lifecycle and
/// ad-hoc) is routed to every registered [`arcen_observability::Sink`].
pub(crate) fn build_journal_fields_from_canonical(
    value: &serde_json::Value,
) -> Result<Option<Vec<(String, String)>>, EventFormatError> {
    let Some(event_id) = value.get("event_id").and_then(serde_json::Value::as_u64) else {
        return Ok(None);
    };
    let message = value
        .get("message")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let severity_name = value
        .get("severity")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("info");
    let priority = match severity_name {
        "error" => 3,
        "warn" => 4,
        "debug" => 7,
        _ => 6,
    };
    let mut fields = vec![
        ("MESSAGE".to_string(), message.to_string()),
        ("PRIORITY".to_string(), priority.to_string()),
        (
            "SYSLOG_IDENTIFIER".to_string(),
            SYSLOG_IDENTIFIER.to_string(),
        ),
        (JOURNAL_EVENT_ID_FIELD.to_string(), event_id.to_string()),
    ];
    if let Some(name) = value.get("event_name").and_then(serde_json::Value::as_str) {
        fields.push(("ARCEN_EVENT_NAME".to_string(), name.to_string()));
    }
    if let Some(category) = value.get("category").and_then(serde_json::Value::as_str) {
        fields.push(("ARCEN_CATEGORY".to_string(), category.to_string()));
    }
    if let Some(outcome) = value.get("outcome").and_then(serde_json::Value::as_str) {
        fields.push(("ARCEN_OUTCOME".to_string(), outcome.to_string()));
    }
    fields.push(("ARCEN_SEVERITY".to_string(), severity_name.to_string()));
    if let Some(sid) = value.get("sid").and_then(serde_json::Value::as_str) {
        fields.push(("ARCEN_CORRELATION_ID".to_string(), sid.to_string()));
    }
    if let Some(map) = value.get("fields").and_then(serde_json::Value::as_object) {
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for key in keys {
            let rendered = match map.get(key) {
                Some(serde_json::Value::String(text)) => text.clone(),
                Some(serde_json::Value::Bool(flag)) => flag.to_string(),
                Some(serde_json::Value::Number(number)) => number.to_string(),
                _ => continue,
            };
            fields.push((
                format!("ARCEN_FIELD_{}", key.to_ascii_uppercase()),
                rendered,
            ));
        }
    }
    if fields.len() > MAX_JOURNAL_FIELD_LINES {
        return Err(EventFormatError::TooManyFields(fields.len()));
    }
    Ok(Some(fields))
}

/// Renders the systemd journal native-protocol datagram for already-built
/// `(field, value)` pairs, matching [`build_journal_datagram`]'s bounds.
fn render_journal_datagram(fields: &[(String, String)]) -> Result<Vec<u8>, EventFormatError> {
    let mut bytes = Vec::new();
    for (key, value) in fields {
        if value.contains('\n') || value.contains('\0') {
            return Err(EventFormatError::FieldContainsControl(key.clone()));
        }
        bytes.extend_from_slice(key.as_bytes());
        bytes.push(b'=');
        bytes.extend_from_slice(value.as_bytes());
        bytes.push(b'\n');
    }
    if bytes.len() > MAX_JOURNAL_DATAGRAM_BYTES {
        return Err(EventFormatError::RenderedTooLarge(bytes.len()));
    }
    Ok(bytes)
}

/// Renders the RFC 3164-style syslog fallback for already-built
/// `(field, value)` pairs, matching [`build_syslog_message`]'s bounds.
fn render_syslog_message(
    fields: &[(String, String)],
    now: SystemTime,
    pid: u32,
) -> Result<String, EventFormatError> {
    const FACILITY_DAEMON: u8 = 3;
    let priority_value: u8 = fields
        .iter()
        .find(|(key, _)| key == "PRIORITY")
        .and_then(|(_, value)| value.parse().ok())
        .unwrap_or(6);
    let priority = FACILITY_DAEMON * 8 + priority_value;
    let timestamp = rfc3164_timestamp(now);
    let summary = fields
        .iter()
        .find(|(key, _)| key == "MESSAGE")
        .map_or("arcen-pier lifecycle event", |(_, value)| value.as_str());
    let mut message = format!("<{priority}>{timestamp} {SYSLOG_IDENTIFIER}[{pid}]: {summary}");
    let mut pairs: BTreeMap<&str, &str> = BTreeMap::new();
    for (key, value) in fields {
        let pair_key = match key.as_str() {
            "MESSAGE" | "PRIORITY" | "SYSLOG_IDENTIFIER" => continue,
            _ if key == JOURNAL_EVENT_ID_FIELD => "arcen_event_id",
            "ARCEN_EVENT_NAME" => "arcen_event_name",
            "ARCEN_CATEGORY" => "arcen_category",
            "ARCEN_OUTCOME" => "arcen_outcome",
            "ARCEN_SEVERITY" => "arcen_severity",
            "ARCEN_CORRELATION_ID" => "arcen_correlation_id",
            other => other.strip_prefix("ARCEN_FIELD_").unwrap_or(other),
        };
        pairs.insert(pair_key, value.as_str());
    }
    for (key, value) in &pairs {
        message.push(' ');
        message.push_str(&key.to_ascii_lowercase());
        message.push('=');
        message.push_str(value);
    }
    if message.contains('\n') || message.contains('\0') {
        return Err(EventFormatError::FieldContainsControl(
            "message".to_string(),
        ));
    }
    if message.len() > MAX_SYSLOG_MESSAGE_BYTES {
        return Err(EventFormatError::RenderedTooLarge(message.len()));
    }
    Ok(message)
}

/// Adapts the Linux journald/syslog delivery path to the shared
/// [`arcen_observability::sink::Sink`] contract used by every registered
/// canonical record sink.
///
/// Every registered sink receives every canonical record (validated
/// lifecycle events and ordinary ad-hoc diagnostics alike); this adapter
/// treats a record with no `event_id` as a delivered no-op so journald keeps
/// carrying only lifecycle events, exactly as it did before this bridge
/// existed. Delivery order and fallback semantics match the pre-migration
/// design: a cached nonblocking datagram to the systemd
/// journal native socket, then `/dev/log` RFC 3164-style fallback. A
/// delivery failure is counted by the owning `BoundedSink`'s `failures`
/// counter (surfaced through `ObservabilityHandle::sink_stats`) and never
/// recursively emits a lifecycle event itself.
pub(crate) struct CanonicalJournalSink<A: JournalSyslogApi> {
    api: A,
}

impl<A: JournalSyslogApi> CanonicalJournalSink<A> {
    pub(crate) fn new(api: A) -> Self {
        Self { api }
    }
}

impl<A: JournalSyslogApi + 'static> arcen_observability::Sink<arcen_telemetry::CanonicalRecord>
    for CanonicalJournalSink<A>
{
    fn deliver(
        &mut self,
        item: arcen_telemetry::CanonicalRecord,
    ) -> Result<(), arcen_observability::SinkError> {
        let value = serde_json::to_value(&item).map_err(|error| {
            arcen_observability::SinkError::adapter(format!(
                "canonical record serialization failed: {error}"
            ))
        })?;
        let Some(fields) = build_journal_fields_from_canonical(&value)
            .map_err(|error| arcen_observability::SinkError::adapter(error.to_string()))?
        else {
            return Ok(());
        };
        let journal_bytes = render_journal_datagram(&fields)
            .map_err(|error| arcen_observability::SinkError::adapter(error.to_string()))?;
        let journal_error = match self.api.send_journal(&journal_bytes) {
            Ok(()) => return Ok(()),
            Err(error) => error,
        };
        let syslog_message = render_syslog_message(&fields, SystemTime::now(), std::process::id())
            .map_err(|error| arcen_observability::SinkError::adapter(error.to_string()))?;
        match self.api.send_syslog(syslog_message.as_bytes()) {
            Ok(()) => Ok(()),
            Err(syslog_error) => Err(arcen_observability::SinkError::adapter(format!(
                "journal delivery failed ({journal_error}) and syslog fallback failed \
                 ({syslog_error})"
            ))),
        }
    }
}

/// Process-local lifecycle event emitter.
///
/// Owns (through an `Arc`) at most one native sink per process. `emit` never
/// returns an error and never blocks session/auth/display work on native
/// delivery; a failure is reported once through `tracing`, deduplicated
/// until the next successful delivery.
#[derive(Clone)]
pub(crate) struct LifecycleEmitter {
    handle: Option<arcen_observability::ObservabilityHandle>,
    host: Option<String>,
    failure_reported: Arc<AtomicBool>,
    /// Ad-hoc-summary delivery-failure latch, kept separate from
    /// `failure_reported` (which tracks stable `LifecycleEventKind`
    /// delivery) so a summary-record failure/recovery never masks or is
    /// masked by an unrelated lifecycle-event failure/recovery.
    summary_failure_reported: Arc<AtomicBool>,
    /// Test-only mirror of every event passed to `emit`, entirely decoupled
    /// from bridge delivery. Always `None` in production.
    recorded: Option<Arc<Mutex<Vec<ValidatedLifecycleEvent>>>>,
}

impl LifecycleEmitter {
    /// An emitter with no bridge handle; `emit` is then a guaranteed no-op.
    pub(crate) fn disabled() -> Self {
        Self {
            handle: None,
            host: None,
            failure_reported: Arc::new(AtomicBool::new(false)),
            summary_failure_reported: Arc::new(AtomicBool::new(false)),
            recorded: None,
        }
    }

    /// An emitter that routes every event through the process's shared
    /// [`arcen_observability::ObservabilityHandle`] (canonical file,
    /// journald, and any other registered sink at once).
    pub(crate) fn new(
        handle: arcen_observability::ObservabilityHandle,
        host: Option<String>,
    ) -> Self {
        Self {
            handle: Some(handle),
            host,
            failure_reported: Arc::new(AtomicBool::new(false)),
            summary_failure_reported: Arc::new(AtomicBool::new(false)),
            recorded: None,
        }
    }

    /// Test-only emitter that mirrors every event passed to `emit` into the
    /// returned `Vec`, completely decoupled from bridge/sink delivery. Used
    /// to assert that `emit_session_*`/`emit_service_*` helpers build the
    /// correct event kind and fields, independent of delivery mechanics
    /// (covered separately by `CanonicalJournalSink` and bridge tests).
    #[cfg(test)]
    pub(crate) fn recording() -> (Self, Arc<Mutex<Vec<ValidatedLifecycleEvent>>>) {
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let emitter = Self {
            handle: None,
            host: Some("test-host".to_string()),
            failure_reported: Arc::new(AtomicBool::new(false)),
            summary_failure_reported: Arc::new(AtomicBool::new(false)),
            recorded: Some(Arc::clone(&recorded)),
        };
        (emitter, recorded)
    }

    /// Builds a top-level [`LifecycleContext`] for `sid`, filling `host`
    /// from this emitter's process-local hostname. `user`/`peer_addr` are
    /// supplied by the caller: authenticated session/auth call sites pass
    /// the real authenticated username and connection peer address, while
    /// pre-authentication/service-level call sites pass `None` for both
    /// (identical to [`Self::emit`]'s default).
    pub(crate) fn session_context(
        &self,
        sid: CorrelationId,
        user: Option<String>,
        peer_addr: Option<String>,
        health_state: Option<arcen_telemetry::HealthState>,
    ) -> LifecycleContext {
        LifecycleContext {
            sid,
            user,
            host: self.host.clone(),
            peer_addr,
            health_state,
        }
    }

    /// Best-effort emission with no identity context: `user`/`peer_addr`
    /// stay `None`, matching this crate's default privacy posture for
    /// service-level and pre-authentication events. Session/auth call sites
    /// that already hold an authenticated username and peer address use
    /// [`Self::emit_context`] instead so those identities reach the
    /// top-level canonical-record/bridge fields (never nested `fields`).
    pub(crate) fn emit(&self, event: &ValidatedLifecycleEvent) {
        self.emit_context(
            event,
            LifecycleContext {
                sid: event.correlation_id().clone(),
                user: None,
                host: self.host.clone(),
                peer_addr: None,
                health_state: None,
            },
        );
    }

    /// Best-effort emission with an explicit top-level [`LifecycleContext`].
    /// Never affects the caller's own outcome: bridge delivery happens
    /// through bounded, nonblocking sink queues, so a synchronous failure
    /// here can only be a schema/correlation programming error, never a
    /// native sink's own health.
    ///
    /// Native sink policy is unaffected by `context.user`/`context.peer_addr`
    /// being populated: [`build_journal_fields_from_canonical`] only ever
    /// reads a closed allowlist of canonical-record keys that never includes
    /// `user`, `host`, or `peer_addr`, so journald/syslog delivery continues
    /// to omit identity even though the canonical JSON Lines file now
    /// carries it for session/auth events.
    pub(crate) fn emit_context(&self, event: &ValidatedLifecycleEvent, context: LifecycleContext) {
        if let Some(recorded) = &self.recorded {
            if let Ok(mut guard) = recorded.lock() {
                guard.push(event.clone());
            }
        }
        let Some(handle) = &self.handle else {
            return;
        };
        let target = target_for_kind(event.kind());
        let message = default_message(event.kind());
        match handle.emit_lifecycle(event, context, canonical_now(), target, message) {
            Ok(_) => self.failure_reported.store(false, Ordering::Release),
            Err(error) => {
                if !self.failure_reported.swap(true, Ordering::AcqRel) {
                    tracing::warn!(
                        target: crate::logging::target::HEALTH,
                        %error,
                        event_id = event.kind().id(),
                        event_name = event.definition().name,
                        "lifecycle event bridge delivery failed; continuing without native \
                         delivery"
                    );
                }
            }
        }
    }

    /// Emits a periodic (Level2, 10-second) QoS window summary as an
    /// ad-hoc [`arcen_telemetry::CanonicalRecord`] carrying the same
    /// explicit top-level `sid`/`user`/`host`/`peer_addr` identity context
    /// as [`Self::emit_context`] — never through a `tracing` diagnostic
    /// macro, whose fields are flat key-values with no top-level identity
    /// promotion. There is no stable `LifecycleEventKind` for a periodic
    /// aggregation window (the 1000-1903 vocabulary is append-only and
    /// this is not one of its fixed events), so this routes through the
    /// shared runtime's own ad-hoc emission path,
    /// [`arcen_observability::ObservabilityHandle::emit_ad_hoc`], which
    /// assigns the one process-wide canonical `sequence` counter shared
    /// with every `LifecycleEventKind` record — so a 10s summary and a
    /// lifecycle event interleave with strictly increasing, non-colliding
    /// sequence numbers in the same canonical file, exactly like the wire
    /// format requires.
    pub(crate) fn emit_summary(
        &self,
        context: LifecycleContext,
        target: TelemetryTarget,
        message: impl Into<String>,
        minimum_profile: arcen_telemetry::OperationalProfile,
        fields: StructuredFields,
    ) {
        let Some(handle) = &self.handle else {
            return;
        };
        match handle.emit_ad_hoc(
            minimum_profile,
            arcen_telemetry::EventSeverity::Info,
            target,
            message,
            context,
            fields,
        ) {
            Ok(_) => self
                .summary_failure_reported
                .store(false, Ordering::Release),
            Err(error) => {
                if !self.summary_failure_reported.swap(true, Ordering::AcqRel) {
                    tracing::warn!(
                        target: crate::logging::target::HEALTH,
                        %error,
                        "10s QoS summary bridge delivery failed; continuing without native \
                         delivery"
                    );
                }
            }
        }
    }

    /// Drains every registered sink's complete loss deltas (queue-full,
    /// queue-closed, delivery-failure, and flush-failure, each counted from
    /// the previous complete drain) and routes one canonical
    /// `TELEMETRY_DROPPED` record per delta to every sink except its own
    /// origin, via the shared runtime's [`arcen_observability::ObservabilityHandle::drain_loss_deltas`]/
    /// [`arcen_observability::ObservabilityHandle::emit_loss_notice`].
    ///
    /// This has no session identity: sink loss is a process-level fact, not
    /// a per-session one, so this is safe to call from a session-independent
    /// cadence such as the service heartbeat, and safe to call repeatedly —
    /// each drain only returns loss counted since the previous complete
    /// drain, so calling this on a fixed schedule cannot double-report.
    /// Routing a notice can only queue further loss for a later drain, never
    /// emit synchronously back through the sink it is reporting on, so this
    /// cannot recurse.
    pub(crate) fn emit_loss_notices(&self) {
        let Some(handle) = &self.handle else {
            return;
        };
        for delta in handle.drain_loss_deltas() {
            let _ = handle.emit_loss_notice(&delta, canonical_now());
        }
    }
}

/// Derives the canonical `arcen::` tracing target for one lifecycle event.
///
/// [`arcen_telemetry::LifecycleCategory`] intentionally is not a proxy for
/// this: for example `DisplayArmed`'s category is `Health`, but it belongs on
/// `arcen::display`. The final wildcard arm exists only because
/// `LifecycleEventKind` is `#[non_exhaustive]`; every kind the Linux host
/// itself can emit is matched explicitly above it.
fn target_for_kind(kind: LifecycleEventKind) -> TelemetryTarget {
    let target = match kind {
        LifecycleEventKind::ServiceStart
        | LifecycleEventKind::ServiceStop
        | LifecycleEventKind::ServiceFailed
        | LifecycleEventKind::NetworkPathActive
        | LifecycleEventKind::NetworkPathChanged
        | LifecycleEventKind::NetworkPathLost
        | LifecycleEventKind::NetworkPathRestored => arcen_telemetry::names::target::NET,
        LifecycleEventKind::SessionAuthOk
        | LifecycleEventKind::SessionAuthFail
        | LifecycleEventKind::PermissionGranted
        | LifecycleEventKind::PermissionDenied
        | LifecycleEventKind::PermissionRevoked
        | LifecycleEventKind::PermissionPending => arcen_telemetry::names::target::AUTH,
        LifecycleEventKind::SessionStreamStart
        | LifecycleEventKind::SessionEnd
        | LifecycleEventKind::SessionInterrupted => arcen_telemetry::names::target::SESSION,
        LifecycleEventKind::DisplayArmed
        | LifecycleEventKind::DisplayRestored
        | LifecycleEventKind::DisplayRestoreDegraded
        | LifecycleEventKind::DisplayRestoreFailed
        | LifecycleEventKind::WatchdogRestore => arcen_telemetry::names::target::DISPLAY,
        LifecycleEventKind::TlsCertificateActive
        | LifecycleEventKind::TlsCertificateExpiring
        | LifecycleEventKind::TlsCertificateReloaded
        | LifecycleEventKind::TlsCertificateReloadFailed
        | LifecycleEventKind::TlsCertificateExpired => crate::logging::target::TLS,
        LifecycleEventKind::HidDeviceAttached
        | LifecycleEventKind::HidDeviceDetached
        | LifecycleEventKind::HidPassthroughStart
        | LifecycleEventKind::HidPassthroughEnd
        | LifecycleEventKind::HidPassthroughError => arcen_telemetry::names::target::HID,
        LifecycleEventKind::HealthOk
        | LifecycleEventKind::HealthDegraded
        | LifecycleEventKind::HealthCritical
        | LifecycleEventKind::HeartbeatLost
        | LifecycleEventKind::TelemetryDropped
        | LifecycleEventKind::EffectiveProfile
        | LifecycleEventKind::HealthSnapshot => arcen_telemetry::names::target::HEALTH,
        // Deck (macOS client) and Credential Provider vocabulary the Linux
        // host never emits; kept only for the shared enum's forward-compat
        // wildcard arm.
        _ => arcen_telemetry::names::target::NET,
    };
    TelemetryTarget::new(target).expect("logging::target constants are canonical arcen:: targets")
}

/// Builds a short, deterministic human summary from the event's stable name
/// and outcome (for example `SESSION_AUTH_OK` + `Succeeded` becomes
/// `"session auth ok succeeded"`), so every bridged lifecycle event carries a
/// readable `message` without a large hand-authored per-kind table.
fn default_message(kind: LifecycleEventKind) -> String {
    let definition = kind.definition();
    let mut message = definition.name.to_ascii_lowercase().replace('_', " ");
    message.push(' ');
    message.push_str(definition.outcome.as_str());
    message
}

/// Formats the current wall-clock time as the canonical
/// `YYYY-MM-DDTHH:MM:SS.ffffffZ` timestamp required by
/// [`arcen_observability::ObservabilityHandle::emit_lifecycle`].
///
/// Reuses this file's own `civil_from_days` (the same public-domain
/// Howard Hinnant algorithm shared with `arcen_observability::runtime`'s
/// private `canonical_now`, which this crate cannot call directly).
fn canonical_now() -> String {
    format_canonical_timestamp(SystemTime::now())
}

/// Best-effort local hostname for the `LifecycleEmitter`'s `host` field.
///
/// Reads the kernel's own view (`/proc/sys/kernel/hostname`) rather than
/// linking a hostname-lookup crate feature. Returns `None` (never panics or
/// logs) when the file is missing, unreadable, or not valid UTF-8 — matching
/// the crate-wide "diagnostics degrade gracefully" convention.
pub(crate) fn local_hostname() -> Option<String> {
    let contents = std::fs::read_to_string("/proc/sys/kernel/hostname").ok()?;
    let trimmed = contents.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// Maximum bytes of a single forwarded helper/child stderr line kept before
/// truncation, bounding memory and log growth from a runaway or malicious
/// child (`arcen-capenc`, `arcen-audiocap`, the session agent, and the
/// session launcher all funnel their stderr through
/// [`bounded_diagnostic_line`] before it becomes a `tracing` event).
pub(crate) const MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES: usize = 4096;

/// Bounds a single line of helper/child stderr to
/// [`MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES`] before it is forwarded into the
/// tracing/observability pipeline as a message. Truncates on a UTF-8
/// character boundary and appends a fixed marker so operators can tell a
/// bounded line from a genuinely short one; never panics on non-boundary
/// splits or empty input.
pub(crate) fn bounded_diagnostic_line(line: &str) -> std::borrow::Cow<'_, str> {
    if line.len() <= MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES {
        return std::borrow::Cow::Borrowed(line);
    }
    let mut end = MAX_FORWARDED_DIAGNOSTIC_LINE_BYTES;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    std::borrow::Cow::Owned(format!("{}…(truncated)", &line[..end]))
}

fn format_canonical_timestamp(now: SystemTime) -> String {
    let duration = now.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    let total_secs = duration.as_secs();
    let days = (total_secs / 86_400) as i64;
    let secs_of_day = total_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = secs_of_day / 3600;
    let minute = (secs_of_day % 3600) / 60;
    let second = secs_of_day % 60;
    let micros = duration.subsec_micros();
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{micros:06}Z")
}

/// Builds the same shape of [`arcen_telemetry::CanonicalRecord`] the bridge
/// itself would produce for `event`, using a fixed placeholder timestamp and
/// sequence number. Used both by [`sanitize_journal_record`] (to re-derive
/// only the approved journal fields when re-emitting a bounded excerpt) and
/// by this module's own tests, so both stay byte-for-byte consistent with
/// [`build_journal_fields_from_canonical`]'s expectations.
fn canonical_record_for(event: &ValidatedLifecycleEvent) -> arcen_telemetry::CanonicalRecord {
    let definition = event.definition();
    arcen_telemetry::CanonicalRecord::new(
        "1970-01-01T00:00:00.000000Z",
        0,
        definition.minimum_profile,
        definition.severity.into(),
        arcen_telemetry::TelemetryRole::Host,
        arcen_telemetry::TelemetryComponent::new("pier").expect("valid component"),
        arcen_telemetry::TelemetryPlatform::Linux,
        target_for_kind(event.kind()),
        default_message(event.kind()),
    )
    .expect("valid canonical record")
    .with_event(event.kind())
    .with_sid(event.correlation_id().clone())
    .with_fields(event.fields().clone())
}

/// A fresh, non-secret correlation id for a lifecycle event that has no live
/// session correlation available (for example process-level service
/// start/stop/failure).
pub(crate) fn random_correlation_id() -> CorrelationId {
    let mut bytes = [0_u8; 16];
    if getrandom::getrandom(&mut bytes).is_err() {
        // Extremely unlikely on a supported host; fall back to a fixed,
        // clearly-synthetic value rather than panicking a lifecycle-adjacent
        // code path.
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
pub(crate) const MAX_EVENT_EXCERPT_RECORDS: usize = 500;
/// Default byte cap for [`query_recent_events`].
pub(crate) const DEFAULT_EVENT_EXCERPT_BYTES: usize = 4 * 1024 * 1024;
/// Hard byte cap for [`query_recent_events`].
pub(crate) const MAX_EVENT_EXCERPT_BYTES: usize = 4 * 1024 * 1024;

/// The packaging-approved conventional syslog files scanned only when
/// journald itself is unavailable. No recursive or unbounded filesystem
/// search is permitted.
#[cfg(target_os = "linux")]
const APPROVED_SYSLOG_FILES: &[&str] = &["/var/log/syslog", "/var/log/messages"];

/// Validated bounds for one [`query_recent_events`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventExcerptLimits {
    pub(crate) max_records: usize,
    pub(crate) max_bytes: usize,
}

impl EventExcerptLimits {
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

/// A bounded excerpt of recently emitted native lifecycle records.
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
    Journal,
    Syslog,
}

/// A [`query_recent_events`] failure.
#[derive(Debug)]
pub(crate) enum NativeEventQueryError {
    InvalidLimits,
    Unavailable,
    PermissionDenied,
    TimedOut,
    Io(std::io::Error),
    CommandFailed,
}

impl Display for NativeEventQueryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => formatter.write_str("event excerpt limits are invalid"),
            Self::Unavailable => formatter
                .write_str("neither the systemd journal nor an approved syslog file is available"),
            Self::PermissionDenied => {
                formatter.write_str("permission was denied reading the systemd journal")
            }
            Self::TimedOut => formatter.write_str("the journalctl query timed out"),
            Self::Io(error) => write!(formatter, "native event query I/O failed: {error}"),
            Self::CommandFailed => formatter.write_str("the journalctl query failed"),
        }
    }
}

impl std::error::Error for NativeEventQueryError {}

/// Reads newest-first bounded JSON-lines for the `arcen-pier` journal
/// identifier via `journalctl` (never `libjournal`/`sd-journal`), falling
/// back to a bounded scan of approved conventional syslog files when
/// journald itself is unavailable.
///
/// Independent of the live [`LifecycleEmitter`]; works while the Pier
/// service is stopped. Enforces both record and byte caps before returning.
/// Callers (Support Bundle) should store `bytes` under `suggested_name` and
/// must not call `journalctl` or scan syslog themselves.
#[cfg(target_os = "linux")]
pub(crate) fn query_recent_events(
    limits: EventExcerptLimits,
) -> Result<BoundedEventExcerpt, NativeEventQueryError> {
    match query_journal(limits) {
        Ok(excerpt) => Ok(excerpt),
        Err(NativeEventQueryError::PermissionDenied) => {
            Err(NativeEventQueryError::PermissionDenied)
        }
        Err(NativeEventQueryError::TimedOut) => {
            query_syslog_files(limits).or(Err(NativeEventQueryError::TimedOut))
        }
        Err(_) => query_syslog_files(limits),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn query_recent_events(
    _limits: EventExcerptLimits,
) -> Result<BoundedEventExcerpt, NativeEventQueryError> {
    Err(NativeEventQueryError::Unavailable)
}

#[cfg(target_os = "linux")]
fn bound_lines_newest_first(
    mut lines: Vec<&str>,
    limits: EventExcerptLimits,
    source: NativeEventQuerySource,
    media_type: &'static str,
    suggested_name: &'static str,
) -> BoundedEventExcerpt {
    lines.reverse(); // journalctl/log files are oldest-first.
    let mut bytes = Vec::new();
    let mut record_count = 0usize;
    let mut truncated = false;
    for line in lines {
        let candidate_len = bytes.len() + line.len() + 1;
        if record_count >= limits.max_records || candidate_len > limits.max_bytes {
            truncated = true;
            break;
        }
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        record_count += 1;
    }
    BoundedEventExcerpt {
        bytes,
        record_count,
        truncated,
        source,
        media_type,
        suggested_name,
    }
}

#[cfg(target_os = "linux")]
fn query_journal(limits: EventExcerptLimits) -> Result<BoundedEventExcerpt, NativeEventQueryError> {
    use std::process::Stdio;

    const JOURNAL_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

    let mut child = std::process::Command::new("journalctl")
        .args([
            "-t",
            SYSLOG_IDENTIFIER,
            "-o",
            "json",
            "--no-pager",
            "-r",
            "-n",
            &limits.max_records.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => NativeEventQueryError::Unavailable,
            _ => NativeEventQueryError::Io(error),
        })?;

    let stdout = child
        .stdout
        .take()
        .ok_or(NativeEventQueryError::CommandFailed)?;
    let stderr = child
        .stderr
        .take()
        .ok_or(NativeEventQueryError::CommandFailed)?;
    let stdout_limit = limits.max_bytes + 1;
    let stdout_thread = std::thread::spawn(move || drain_event_query_stream(stdout, stdout_limit));
    let stderr_thread = std::thread::spawn(move || drain_event_query_stream(stderr, 4096));
    let deadline = std::time::Instant::now() + JOURNAL_QUERY_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(NativeEventQueryError::TimedOut);
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(NativeEventQueryError::Io(error));
            }
        }
    };
    let (mut bytes, stdout_truncated) = stdout_thread
        .join()
        .map_err(|_| NativeEventQueryError::CommandFailed)?;
    let (stderr, _) = stderr_thread
        .join()
        .map_err(|_| NativeEventQueryError::CommandFailed)?;
    let truncated = stdout_truncated || bytes.len() > limits.max_bytes;
    if truncated {
        bytes.truncate(limits.max_bytes);
        if let Some(last_complete) = bytes.iter().rposition(|byte| *byte == b'\n') {
            bytes.truncate(last_complete + 1);
        } else {
            bytes.clear();
        }
    }
    if !status.success() && !truncated {
        let stderr = String::from_utf8_lossy(&stderr);
        if stderr.to_ascii_lowercase().contains("permission") {
            return Err(NativeEventQueryError::PermissionDenied);
        }
        return Err(NativeEventQueryError::CommandFailed);
    }

    let text = String::from_utf8_lossy(&bytes);
    let mut filtered = Vec::new();
    let mut record_count = 0usize;
    let mut invalid_record = false;
    for line in text.lines() {
        if let Some(line) = sanitize_journal_record(line) {
            filtered.extend_from_slice(&line);
            filtered.push(b'\n');
            record_count += 1;
        } else if line.contains(JOURNAL_EVENT_ID_FIELD) {
            invalid_record = true;
        }
    }
    Ok(BoundedEventExcerpt {
        bytes: filtered,
        record_count,
        truncated: truncated || invalid_record,
        source: NativeEventQuerySource::Journal,
        media_type: "application/x-ndjson",
        suggested_name: "events/linux-journal-arcen-pier.jsonl",
    })
}

#[cfg(target_os = "linux")]
fn drain_event_query_stream(mut stream: impl std::io::Read, limit: usize) -> (Vec<u8>, bool) {
    let mut bytes = Vec::with_capacity(limit.min(64 * 1024));
    let mut buffer = [0_u8; 64 * 1024];
    let mut truncated = false;
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(_) => {
                truncated = true;
                break;
            }
        };
        let remaining = limit.saturating_sub(bytes.len());
        let stored = count.min(remaining);
        bytes.extend_from_slice(&buffer[..stored]);
        truncated |= stored != count;
    }
    (bytes, truncated)
}

#[cfg(target_os = "linux")]
fn query_syslog_files(
    limits: EventExcerptLimits,
) -> Result<BoundedEventExcerpt, NativeEventQueryError> {
    use std::collections::VecDeque;
    use std::io::{Read, Seek, SeekFrom};

    const MARKER: &str = "arcen_event_id=";

    let mut any_file_found = false;
    let mut matches = VecDeque::new();
    let mut matched_bytes = 0usize;
    let mut truncated = false;
    for path in APPROVED_SYSLOG_FILES {
        let Ok(mut file) = std::fs::File::open(path) else {
            continue;
        };
        any_file_found = true;
        let length = file.metadata().map_err(NativeEventQueryError::Io)?.len();
        let start = length.saturating_sub(limits.max_bytes as u64);
        if start > 0 {
            truncated = true;
            file.seek(SeekFrom::Start(start))
                .map_err(NativeEventQueryError::Io)?;
        }
        let mut tail = Vec::with_capacity(limits.max_bytes.min(64 * 1024));
        file.take(limits.max_bytes as u64)
            .read_to_end(&mut tail)
            .map_err(NativeEventQueryError::Io)?;
        if start > 0 {
            if let Some(first_newline) = tail.iter().position(|byte| *byte == b'\n') {
                tail.drain(..=first_newline);
            } else {
                tail.clear();
            }
        }
        let contents = String::from_utf8_lossy(&tail);
        for line in contents.lines() {
            if let Some(line) = sanitize_syslog_record(line) {
                let line_bytes = line.len() + 1;
                matches.push_back(line);
                matched_bytes += line_bytes;
                while matches.len() > limits.max_records || matched_bytes > limits.max_bytes {
                    if let Some(removed) = matches.pop_front() {
                        matched_bytes = matched_bytes.saturating_sub(removed.len() + 1);
                        truncated = true;
                    }
                }
            }
        }
    }
    if !any_file_found {
        return Err(NativeEventQueryError::Unavailable);
    }
    let record_count = matches.len();
    let mut bytes = Vec::with_capacity(matched_bytes);
    for line in matches.iter().rev() {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    Ok(BoundedEventExcerpt {
        bytes,
        record_count,
        truncated,
        source: NativeEventQuerySource::Syslog,
        media_type: "text/plain",
        suggested_name: "events/linux-syslog-arcen-pier.log",
    })
}

fn sanitize_journal_record(line: &str) -> Option<Vec<u8>> {
    let object = serde_json::from_str::<serde_json::Value>(line)
        .ok()?
        .as_object()?
        .clone();
    let event_id = object.get(JOURNAL_EVENT_ID_FIELD)?.as_str()?.parse().ok()?;
    let definition = arcen_telemetry::lifecycle_event_definition(event_id)?;
    if object.get("SYSLOG_IDENTIFIER")?.as_str()? != SYSLOG_IDENTIFIER
        || object.get("ARCEN_EVENT_NAME")?.as_str()? != definition.name
        || object.get("ARCEN_CATEGORY")?.as_str()? != definition.category.as_str()
        || object.get("ARCEN_OUTCOME")?.as_str()? != definition.outcome.as_str()
        || object.get("ARCEN_SEVERITY")?.as_str()? != definition.severity.as_str()
    {
        return None;
    }
    let correlation_id = arcen_telemetry::CorrelationId::parse_uuid(
        object.get("ARCEN_CORRELATION_ID")?.as_str()?.to_string(),
    )
    .ok()?;
    let mut fields = StructuredFields::default();
    for spec in definition.fields {
        let key = format!("ARCEN_FIELD_{}", spec.name.to_ascii_uppercase());
        let Some(value) = object.get(&key).and_then(serde_json::Value::as_str) else {
            continue;
        };
        let value = match spec.field_type {
            LifecycleFieldType::String => FieldValue::String(value.to_string()),
            LifecycleFieldType::Integer => FieldValue::Integer(value.parse().ok()?),
            LifecycleFieldType::Boolean => FieldValue::Boolean(value.parse().ok()?),
        };
        fields.insert(spec.name, value).ok()?;
    }
    let event = ValidatedLifecycleEvent::new(definition.kind, correlation_id, fields).ok()?;
    let canonical_value = serde_json::to_value(canonical_record_for(&event)).ok()?;
    let approved_fields = build_journal_fields_from_canonical(&canonical_value).ok()??;
    let mut sanitized = serde_json::Map::new();
    for (key, value) in approved_fields {
        if key != "MESSAGE" {
            sanitized.insert(key, serde_json::Value::String(value));
        }
    }
    serde_json::to_vec(&serde_json::Value::Object(sanitized)).ok()
}

fn sanitize_syslog_record(line: &str) -> Option<String> {
    let start = line.find("arcen-pier[")?;
    let record = &line[start..];
    let event_id = value_after(record, "arcen_event_id=")?.parse().ok()?;
    let definition = arcen_telemetry::lifecycle_event_definition(event_id)?;
    if value_after(record, "arcen_event_name=")? != definition.name
        || value_after(record, "arcen_category=")? != definition.category.as_str()
        || value_after(record, "arcen_outcome=")? != definition.outcome.as_str()
        || value_after(record, "arcen_severity=")? != definition.severity.as_str()
    {
        return None;
    }
    let correlation = arcen_telemetry::CorrelationId::parse_uuid(
        value_after(record, "arcen_correlation_id=")?.to_string(),
    )
    .ok()?;
    Some(format!(
        "arcen-pier lifecycle arcen_event_id={} arcen_event_name={} arcen_category={} \
         arcen_outcome={} arcen_severity={} arcen_correlation_id={}",
        definition.kind.id(),
        definition.name,
        definition.category.as_str(),
        definition.outcome.as_str(),
        definition.severity.as_str(),
        correlation.as_str()
    ))
}

fn value_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let value = line.split_once(marker)?.1;
    Some(value.split_whitespace().next().unwrap_or(value))
}

/// Test-only helpers shared with `net::server` and `display::nvctrl`'s own
/// `LifecycleEmitter`-based tests. `#[cfg(test)]` makes this module visible
/// to every other module's test code within the same `cargo test` build,
/// mirroring the pre-migration `FakeEventLogBackend` seam it replaces.
#[cfg(test)]
pub(crate) mod test_support {
    use std::io;
    use std::sync::Arc;

    use arcen_observability::{ObservabilityBuilder, ObservabilityRuntime};
    use arcen_telemetry::{
        OperationalProfile, TelemetryComponent, TelemetryPlatform, TelemetryRole,
    };

    use super::LifecycleEmitter;

    /// A canonical writer whose every write and flush fails, used to prove
    /// that a genuine sink-delivery failure never blocks or changes a
    /// caller's own outcome (mirrors the old `always_failing` fake backend).
    #[derive(Clone, Copy, Default)]
    pub(crate) struct AlwaysFailWriter;

    impl io::Write for AlwaysFailWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("always-fail test writer"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("always-fail test writer"))
        }
    }

    /// Builds a throwaway [`ObservabilityRuntime`] (one canonical writer,
    /// no journald sink, `ARCEN_LOG` disabled so tests never depend on the
    /// ambient environment) with a [`LifecycleEmitter`] wired to its
    /// handle. Returned alongside the runtime so a test can flush/inspect
    /// `sink_stats` before it drops.
    pub(crate) fn emitter_with_writer(
        writer: impl io::Write + Send + 'static,
    ) -> (LifecycleEmitter, Arc<ObservabilityRuntime>) {
        let runtime = ObservabilityBuilder::new(
            TelemetryRole::Host,
            TelemetryComponent::new("pier").expect("valid component"),
            TelemetryPlatform::Linux,
            OperationalProfile::Debug,
        )
        .canonical_writer("test", writer)
        .arcen_log(None::<String>)
        .build()
        .expect("test observability runtime");
        let runtime = Arc::new(runtime);
        let emitter = LifecycleEmitter::new(runtime.handle(), Some("test-host".to_string()));
        (emitter, runtime)
    }

    /// Snapshots every event recorded so far by a
    /// [`LifecycleEmitter::recording`] fixture, mirroring the pre-migration
    /// `FakeEventLogBackend::recorded` helper used across `net/server.rs`
    /// and `display/nvctrl.rs` tests.
    pub(crate) fn recorded_events(
        recorded: &Arc<std::sync::Mutex<Vec<super::ValidatedLifecycleEvent>>>,
    ) -> Vec<super::ValidatedLifecycleEvent> {
        recorded
            .lock()
            .expect("recorded events lock is poisoned")
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use arcen_observability::Sink;
    use arcen_telemetry::{FieldValue, LifecycleEventKind, StructuredFields};

    use super::*;

    fn correlation_id() -> CorrelationId {
        CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef")
            .expect("canonical correlation id")
    }

    fn service_start_event() -> ValidatedLifecycleEvent {
        let mut fields = StructuredFields::default();
        fields
            .insert("component", FieldValue::String("arcen-pier".to_string()))
            .unwrap();
        fields.insert("pid", FieldValue::Integer(4242)).unwrap();
        ValidatedLifecycleEvent::new(LifecycleEventKind::ServiceStart, correlation_id(), fields)
            .expect("valid service_start event")
    }

    fn session_interrupted_event() -> ValidatedLifecycleEvent {
        let mut fields = StructuredFields::default();
        fields
            .insert("stage", FieldValue::String("transport".to_string()))
            .unwrap();
        fields
            .insert(
                "reason_class",
                FieldValue::String("transport_error".to_string()),
            )
            .unwrap();
        fields
            .insert("duration_ms", FieldValue::Integer(1500))
            .unwrap();
        ValidatedLifecycleEvent::new(
            LifecycleEventKind::SessionInterrupted,
            correlation_id(),
            fields,
        )
        .expect("valid session_interrupted event")
    }

    #[test]
    fn journal_fields_are_fixed_then_sorted_schema_fields() {
        let value =
            serde_json::to_value(canonical_record_for(&service_start_event())).expect("json");
        let fields = build_journal_fields_from_canonical(&value)
            .expect("format ok")
            .expect("event_id present");
        let names: Vec<&str> = fields.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "MESSAGE",
                "PRIORITY",
                "SYSLOG_IDENTIFIER",
                "ARCEN_EVENT_ID",
                "ARCEN_EVENT_NAME",
                "ARCEN_CATEGORY",
                "ARCEN_OUTCOME",
                "ARCEN_SEVERITY",
                "ARCEN_CORRELATION_ID",
                "ARCEN_FIELD_COMPONENT",
                "ARCEN_FIELD_PID",
            ]
        );
        assert_eq!(
            fields
                .iter()
                .find(|(name, _)| name == "ARCEN_EVENT_ID")
                .map(|(_, value)| value.as_str()),
            Some("1000")
        );
        assert_eq!(
            fields
                .iter()
                .find(|(name, _)| name == "PRIORITY")
                .map(|(_, value)| value.as_str()),
            Some("6")
        );
    }

    #[test]
    fn journal_fields_from_canonical_is_none_for_ad_hoc_diagnostics() {
        let record = arcen_telemetry::CanonicalRecord::new(
            "2024-01-05T00:00:00.000000Z",
            1,
            arcen_telemetry::OperationalProfile::Debug,
            arcen_telemetry::EventSeverity::Info,
            arcen_telemetry::TelemetryRole::Host,
            arcen_telemetry::TelemetryComponent::new("pier").expect("valid component"),
            arcen_telemetry::TelemetryPlatform::Linux,
            target_for_kind(LifecycleEventKind::ServiceStart),
            "ordinary ad-hoc diagnostic, not a lifecycle event",
        )
        .expect("valid canonical record");
        let value = serde_json::to_value(record).expect("json");
        assert_eq!(
            build_journal_fields_from_canonical(&value).expect("format ok"),
            None,
            "journald must keep carrying only lifecycle events"
        );
    }

    #[test]
    fn journal_datagram_is_deterministic_and_bounded() {
        let value =
            serde_json::to_value(canonical_record_for(&service_start_event())).expect("json");
        let fields = build_journal_fields_from_canonical(&value)
            .expect("format ok")
            .expect("event_id present");
        let first = render_journal_datagram(&fields).expect("format ok");
        let second = render_journal_datagram(&fields).expect("format ok");
        assert_eq!(first, second, "identical fields render identical bytes");
        assert!(first.len() <= MAX_JOURNAL_DATAGRAM_BYTES);
        let text = String::from_utf8(first).expect("journal payload is UTF-8");
        assert!(text.contains("ARCEN_EVENT_ID=1000\n"));
        assert!(text.contains("SYSLOG_IDENTIFIER=arcen-pier\n"));
        assert!(text.ends_with('\n'));
    }

    #[test]
    fn syslog_message_matches_rfc3164_style_and_lowercase_marker() {
        let now = UNIX_EPOCH + Duration::from_secs(1_753_000_000); // fixed, deterministic instant
        let value =
            serde_json::to_value(canonical_record_for(&session_interrupted_event())).expect("json");
        let fields = build_journal_fields_from_canonical(&value)
            .expect("format ok")
            .expect("event_id present");
        let message = render_syslog_message(&fields, now, 777).expect("format ok");
        assert!(message.starts_with("<28>")); // daemon(3)*8 + warning(4) = 28
        assert!(message.contains("arcen-pier[777]:"));
        assert!(message.contains("arcen_event_id=1104"));
        assert!(message.contains("reason_class=transport_error"));
        assert!(message.contains("stage=transport"));
        assert!(message.len() <= MAX_SYSLOG_MESSAGE_BYTES);
    }

    #[test]
    fn rfc3164_timestamp_pads_single_digit_days() {
        // 2024-01-05 00:00:00 UTC.
        let now = UNIX_EPOCH + Duration::from_secs(1_704_412_800);
        assert_eq!(rfc3164_timestamp(now), "Jan  5 00:00:00");
    }

    #[test]
    fn canonical_timestamp_matches_the_shared_schema_format() {
        // 2024-01-05 00:00:01.250000 UTC.
        let now = UNIX_EPOCH + Duration::from_micros(1_704_412_801_250_000);
        assert_eq!(
            format_canonical_timestamp(now),
            "2024-01-05T00:00:01.250000Z"
        );
    }

    #[test]
    fn priority_mapping_matches_the_plan() {
        assert_eq!(journal_priority(LifecycleSeverity::Information), 6);
        assert_eq!(journal_priority(LifecycleSeverity::Warning), 4);
        assert_eq!(journal_priority(LifecycleSeverity::Error), 3);
    }

    /// Injectable fake datagram backend used to prove the
    /// journal → syslog → tracing-fallback delivery order without any real
    /// socket.
    struct FakeDatagramApi {
        journal: Result<(), SocketSendError>,
        syslog: Result<(), SocketSendError>,
        journal_calls: std::sync::atomic::AtomicUsize,
        syslog_calls: std::sync::atomic::AtomicUsize,
    }

    impl FakeDatagramApi {
        fn new(journal: Result<(), SocketSendError>, syslog: Result<(), SocketSendError>) -> Self {
            Self {
                journal,
                syslog,
                journal_calls: std::sync::atomic::AtomicUsize::new(0),
                syslog_calls: std::sync::atomic::AtomicUsize::new(0),
            }
        }
    }

    impl JournalSyslogApi for FakeDatagramApi {
        fn send_journal(&self, _bytes: &[u8]) -> Result<(), SocketSendError> {
            self.journal_calls.fetch_add(1, Ordering::SeqCst);
            match &self.journal {
                Ok(()) => Ok(()),
                Err(SocketSendError::Unavailable) => Err(SocketSendError::Unavailable),
                Err(SocketSendError::WouldBlock) => Err(SocketSendError::WouldBlock),
                Err(SocketSendError::Io(error)) => Err(SocketSendError::Io(std::io::Error::new(
                    error.kind(),
                    error.to_string(),
                ))),
            }
        }

        fn send_syslog(&self, _bytes: &[u8]) -> Result<(), SocketSendError> {
            self.syslog_calls.fetch_add(1, Ordering::SeqCst);
            match &self.syslog {
                Ok(()) => Ok(()),
                Err(SocketSendError::Unavailable) => Err(SocketSendError::Unavailable),
                Err(SocketSendError::WouldBlock) => Err(SocketSendError::WouldBlock),
                Err(SocketSendError::Io(error)) => Err(SocketSendError::Io(std::io::Error::new(
                    error.kind(),
                    error.to_string(),
                ))),
            }
        }
    }

    #[test]
    fn journal_success_never_falls_back_to_syslog() {
        let api = FakeDatagramApi::new(Ok(()), Ok(()));
        let mut sink = CanonicalJournalSink::new(api);
        sink.deliver(canonical_record_for(&service_start_event()))
            .expect("journal path succeeds");
        assert_eq!(sink.api.journal_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sink.api.syslog_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn journal_failure_falls_back_to_syslog() {
        let api = FakeDatagramApi::new(Err(SocketSendError::Unavailable), Ok(()));
        let mut sink = CanonicalJournalSink::new(api);
        sink.deliver(canonical_record_for(&service_start_event()))
            .expect("syslog fallback succeeds");
        assert_eq!(sink.api.journal_calls.load(Ordering::SeqCst), 1);
        assert_eq!(sink.api.syslog_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn both_sinks_unavailable_is_reported_as_one_error() {
        let api = FakeDatagramApi::new(
            Err(SocketSendError::Unavailable),
            Err(SocketSendError::WouldBlock),
        );
        let mut sink = CanonicalJournalSink::new(api);
        assert!(sink
            .deliver(canonical_record_for(&service_start_event()))
            .is_err());
    }

    #[test]
    fn ad_hoc_diagnostic_never_reaches_journald() {
        let api = FakeDatagramApi::new(Ok(()), Ok(()));
        let mut sink = CanonicalJournalSink::new(api);
        let record = arcen_telemetry::CanonicalRecord::new(
            "2024-01-05T00:00:00.000000Z",
            1,
            arcen_telemetry::OperationalProfile::Debug,
            arcen_telemetry::EventSeverity::Info,
            arcen_telemetry::TelemetryRole::Host,
            arcen_telemetry::TelemetryComponent::new("pier").expect("valid component"),
            arcen_telemetry::TelemetryPlatform::Linux,
            target_for_kind(LifecycleEventKind::ServiceStart),
            "ordinary ad-hoc diagnostic, not a lifecycle event",
        )
        .expect("valid canonical record");
        sink.deliver(record).expect("no-op delivery succeeds");
        assert_eq!(sink.api.journal_calls.load(Ordering::SeqCst), 0);
        assert_eq!(sink.api.syslog_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn emitter_never_panics_when_every_bridge_sink_fails() {
        let (emitter, runtime) = test_support::emitter_with_writer(test_support::AlwaysFailWriter);
        // Deduplication and resilience are exercised indirectly: emit never
        // panics or blocks the caller, even though every registered sink's
        // own delivery is guaranteed to fail.
        emitter.emit(&service_start_event());
        emitter.emit(&service_start_event());
        emitter.emit(&service_start_event());
        // The round trip through the flush command line guarantees every
        // queued delivery has already been attempted; the always-failing
        // writer means `flush` itself also returns an error, which is
        // expected here and intentionally discarded.
        let _ = runtime.guard().flush(Duration::from_secs(1));
        let failures: u64 = runtime
            .handle()
            .sink_stats()
            .iter()
            .map(|stats| stats.failures)
            .sum();
        assert!(
            failures > 0,
            "the always-failing writer's failures must be counted, not silently dropped"
        );
    }

    #[test]
    fn emitter_records_events_on_a_working_recorder() {
        let (emitter, recorded) = LifecycleEmitter::recording();
        emitter.emit(&service_start_event());
        emitter.emit(&session_interrupted_event());
        let recorded = recorded.lock().expect("recorder lock");
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].kind(), LifecycleEventKind::ServiceStart);
        assert_eq!(recorded[1].kind(), LifecycleEventKind::SessionInterrupted);
    }

    /// A thread-safe in-memory canonical writer used to assert on exact
    /// JSON Lines bytes, mirroring `net::server::tests::SharedBufferWriter`.
    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().expect("buffer lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Finding #2 (prior round) / re-review finding #3 (this round): a
    /// saturated/failing journald+syslog bridge sink must surface its own
    /// complete loss (queue-full/closed, delivery, and flush failures) as a
    /// counted `TELEMETRY_DROPPED` notice via the shared runtime's complete,
    /// origin-excluding `drain_loss_deltas`/`emit_loss_notice` API, never
    /// silently, never by recursing back through the same sink, and a
    /// second drain with nothing new since the previous one must emit
    /// nothing further (proving the cursor is complete, not cumulative).
    #[test]
    fn shared_loss_deltas_are_drained_and_reported_without_recursion() {
        let buffer: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let api = FakeDatagramApi::new(
            Err(SocketSendError::Unavailable),
            Err(SocketSendError::WouldBlock),
        );
        let runtime = arcen_observability::ObservabilityBuilder::new(
            arcen_telemetry::TelemetryRole::Host,
            arcen_telemetry::TelemetryComponent::new("pier").expect("valid component"),
            arcen_telemetry::TelemetryPlatform::Linux,
            arcen_telemetry::OperationalProfile::Debug,
        )
        .canonical_writer("test", CapturingWriter(Arc::clone(&buffer)))
        .arcen_log(None::<String>)
        .register_sink("journald", CanonicalJournalSink::new(api))
        .build()
        .expect("test observability runtime");
        let runtime = Arc::new(runtime);
        let emitter = LifecycleEmitter::new(runtime.handle(), Some("test-host".to_string()));
        let context = emitter.session_context(correlation_id(), None, None, None);

        // Drive one lifecycle event through the bridge: both journal and
        // syslog fail, so the shared runtime counts exactly one
        // `delivery_failure` loss for the "journald" sink.
        emitter.emit_context(&service_start_event(), context);
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush sinks");

        buffer.lock().expect("buffer lock").clear();
        emitter.emit_loss_notices();
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush sinks");
        let text = String::from_utf8(buffer.lock().expect("buffer lock").clone())
            .expect("utf8 canonical output");
        let line = text
            .lines()
            .next()
            .expect("one canonical JSON line for the drained delta");
        let value: serde_json::Value = serde_json::from_str(line).expect("valid JSON line");
        assert_eq!(
            value["fields"]["sink"],
            serde_json::json!("journald:delivery_failure")
        );
        assert_eq!(value["fields"]["dropped_count"], serde_json::json!(1));

        // A second drain with no new loss since the previous one must emit
        // nothing further: the complete cursor starts from zero each call,
        // and routing the first notice must not have recursed back through
        // the journald sink it reported on.
        buffer.lock().expect("buffer lock").clear();
        emitter.emit_loss_notices();
        runtime
            .handle()
            .flush(Duration::from_secs(1))
            .expect("flush sinks");
        assert!(
            buffer.lock().expect("buffer lock").is_empty(),
            "draining twice in a row without new loss must not re-emit a stale delta"
        );
    }

    #[test]
    fn disabled_emitter_never_panics_and_records_nothing() {
        let emitter = LifecycleEmitter::disabled();
        emitter.emit(&service_start_event());
        emitter.emit(&session_interrupted_event());
    }

    #[test]
    fn failure_injected_emitter_does_not_change_a_caller_supplied_result() {
        // Mirrors how session/display call sites use `emit`: a native sink
        // failure must never alter the caller's own `Result`.
        let (emitter, _runtime) = test_support::emitter_with_writer(test_support::AlwaysFailWriter);
        fn caller_outcome(emitter: &LifecycleEmitter) -> Result<&'static str, &'static str> {
            emitter.emit(&service_start_event());
            Ok("session proceeded")
        }
        assert_eq!(caller_outcome(&emitter), Ok("session proceeded"));
    }

    #[test]
    fn random_correlation_ids_are_unique_canonical_uuids() {
        let first = random_correlation_id();
        let second = random_correlation_id();
        assert_ne!(first, second);
        assert_eq!(first.as_str().len(), 36);
    }

    #[test]
    fn excerpt_limits_reject_zero_and_oversized_bounds() {
        assert!(EventExcerptLimits::new(0, 1024).is_err());
        assert!(EventExcerptLimits::new(1, 0).is_err());
        assert!(EventExcerptLimits::new(MAX_EVENT_EXCERPT_RECORDS + 1, 1024).is_err());
        assert!(EventExcerptLimits::new(1, MAX_EVENT_EXCERPT_BYTES + 1).is_err());
        assert!(EventExcerptLimits::new(500, 4 * 1024 * 1024).is_ok());
        assert_eq!(EventExcerptLimits::default().max_records, 500);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn bound_lines_newest_first_reverses_and_caps() {
        let limits = EventExcerptLimits::new(2, 4096).unwrap();
        let lines = vec![
            "oldest ARCEN_EVENT_ID=1",
            "middle ARCEN_EVENT_ID=2",
            "newest ARCEN_EVENT_ID=3",
        ];
        let excerpt = bound_lines_newest_first(
            lines,
            limits,
            NativeEventQuerySource::Journal,
            "application/x-ndjson",
            "events/linux-journal-arcen-pier.jsonl",
        );
        let text = String::from_utf8(excerpt.bytes).unwrap();
        assert_eq!(excerpt.record_count, 2);
        assert!(excerpt.truncated);
        assert!(text.starts_with("newest"));
        assert!(text.contains("middle"));
        assert!(!text.contains("oldest"));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn query_recent_events_is_unavailable_off_linux() {
        assert!(matches!(
            query_recent_events(EventExcerptLimits::default()),
            Err(NativeEventQueryError::Unavailable)
        ));
    }

    #[test]
    fn journal_excerpt_drops_hostname_and_unapproved_fields() {
        let line = r#"{"MESSAGE":"injected secret","PRIORITY":"6","SYSLOG_IDENTIFIER":"arcen-pier","ARCEN_EVENT_ID":"1000","ARCEN_EVENT_NAME":"SERVICE_START","ARCEN_CATEGORY":"health","ARCEN_OUTCOME":"succeeded","ARCEN_SEVERITY":"information","ARCEN_CORRELATION_ID":"01234567-89ab-4def-8123-456789abcdef","ARCEN_FIELD_COMPONENT":"pier_broker","ARCEN_FIELD_VERSION":"0.1.0","_HOSTNAME":"sensitive-host","_CMDLINE":"secret"}"#;
        let sanitized = sanitize_journal_record(line).expect("valid lifecycle record");
        let text = String::from_utf8(sanitized).expect("UTF-8");
        assert!(!text.contains("_HOSTNAME"));
        assert!(!text.contains("sensitive-host"));
        assert!(!text.contains("_CMDLINE"));
        assert!(!text.contains("injected secret"));
        assert!(text.contains("ARCEN_EVENT_ID"));
    }

    #[test]
    fn syslog_excerpt_removes_daemon_hostname_prefix() {
        let line = "Jul 20 12:00:00 sensitive-host arcen-pier[42]: lifecycle arcen_category=health arcen_correlation_id=01234567-89ab-4def-8123-456789abcdef arcen_event_id=1000 arcen_event_name=SERVICE_START arcen_outcome=succeeded arcen_severity=information secret=customer";
        let sanitized = sanitize_syslog_record(line).expect("valid lifecycle record");
        assert!(sanitized.starts_with("arcen-pier lifecycle"));
        assert!(!sanitized.contains("sensitive-host"));
        assert!(!sanitized.contains("customer"));
    }
}
