//! Stable, append-only lifecycle event vocabulary.

use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::{
    CorrelationId, EventOutcome, FieldValue, LifecycleCategory, LifecycleEvent, OperationalProfile,
    StructuredFields,
};

/// Maximum number of structured fields accepted by one event.
pub const MAX_STRUCTURED_FIELDS: usize = 16;

/// Stable lifecycle event identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u32)]
#[non_exhaustive]
pub enum LifecycleEventKind {
    /// Pier process entered its running state.
    ServiceStart = 1000,
    /// Pier process stopped cleanly.
    ServiceStop = 1001,
    /// Pier process failed.
    ServiceFailed = 1002,
    /// Machine authentication and identity binding succeeded.
    SessionAuthOk = 1100,
    /// Machine authentication was refused.
    SessionAuthFail = 1101,
    /// Session media and input streaming became active.
    SessionStreamStart = 1102,
    /// Session ended cleanly.
    SessionEnd = 1103,
    /// Session ended because a component or transport failed.
    SessionInterrupted = 1104,
    /// Which capture backend actually served the session.
    ///
    /// Level 3. Records the path that *ran*, not the one configured: Windows
    /// falls back DDA -> WGC silently, which is why WGC is the production path
    /// on a headless vGPU, and a configured value would have said otherwise.
    CapturePathSelected = 1105,
    /// The encoder surface the host actually configured.
    ///
    /// Level 3. The 8-bit path is fast because it is zero-copy GPU-direct;
    /// every wide-source route trades that away. Recording the surface makes
    /// that trade measurable instead of remembered.
    EncoderConfigured = 1106,
    /// What the client asked for, what the host granted, and why they differ.
    ///
    /// Level 3. The host had no equivalent of the client's `PlanDegradation`,
    /// so a session that was granted exactly what it asked for was
    /// indistinguishable from one that was quietly reduced.
    ColorPlanResolved = 1107,
    /// Display state was mutated under restore protection.
    DisplayArmed = 1200,
    /// Display state was restored and verified.
    DisplayRestored = 1201,
    /// Display restore succeeded with a warning or fallback.
    DisplayRestoreDegraded = 1202,
    /// Display restore failed.
    DisplayRestoreFailed = 1203,
    /// A watchdog restored display state after a crash.
    WatchdogRestore = 1204,
    /// Credential Provider cold logon completed.
    CpLogonOk = 1300,
    /// Credential Provider cold logon failed.
    CpLogonFail = 1301,
    /// A validated TLS server certificate is active.
    TlsCertificateActive = 1400,
    /// The active TLS server certificate entered its warning window.
    TlsCertificateExpiring = 1401,
    /// A TLS server certificate was reloaded successfully.
    TlsCertificateReloaded = 1402,
    /// A TLS server certificate reload was refused.
    TlsCertificateReloadFailed = 1403,
    /// The active TLS server certificate expired.
    TlsCertificateExpired = 1404,
    /// Deck process entered its running state.
    ClientStart = 1500,
    /// Deck process stopped cleanly.
    ClientStop = 1501,
    /// Deck began a connection attempt.
    ClientConnectAttempt = 1502,
    /// Deck established an authenticated connection.
    ClientConnectOk = 1503,
    /// Deck connection attempt failed.
    ClientConnectFail = 1504,
    /// Deck session ended with a bounded summary.
    ClientSessionEnd = 1505,
    /// Deck began a reconnect attempt.
    ClientReconnect = 1506,
    /// HID device became available.
    HidDeviceAttached = 1600,
    /// HID device became unavailable.
    HidDeviceDetached = 1601,
    /// HID passthrough became active.
    HidPassthroughStart = 1602,
    /// HID passthrough ended.
    HidPassthroughEnd = 1603,
    /// HID passthrough encountered a bounded failure.
    HidPassthroughError = 1604,
    /// A network path became active.
    NetworkPathActive = 1700,
    /// The active network path changed.
    NetworkPathChanged = 1701,
    /// The active network path was lost.
    NetworkPathLost = 1702,
    /// A lost network path was restored.
    NetworkPathRestored = 1703,
    /// Health returned to normal.
    HealthOk = 1800,
    /// Health became degraded.
    HealthDegraded = 1801,
    /// Health became critical.
    HealthCritical = 1802,
    /// Expected health heartbeats were lost.
    HeartbeatLost = 1803,
    /// A bounded telemetry sink dropped records.
    TelemetryDropped = 1804,
    /// The effective process profile was selected or changed.
    EffectiveProfile = 1805,
    /// Sixty-second Level-0 proof-of-life health snapshot.
    HealthSnapshot = 1806,
    /// A required OS permission was granted.
    PermissionGranted = 1900,
    /// A required OS permission was denied.
    PermissionDenied = 1901,
    /// A previously granted OS permission was revoked.
    PermissionRevoked = 1902,
    /// An OS permission request is pending.
    PermissionPending = 1903,
}

impl LifecycleEventKind {
    /// Returns the stable numeric event identifier.
    #[must_use]
    pub const fn id(self) -> u32 {
        self as u32
    }

    /// Returns the stable event name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        self.definition().name
    }

    /// Returns the canonical event definition.
    #[must_use]
    pub const fn definition(self) -> &'static LifecycleEventDefinition {
        match self {
            Self::ServiceStart => &LIFECYCLE_EVENT_DEFINITIONS[0],
            Self::ServiceStop => &LIFECYCLE_EVENT_DEFINITIONS[1],
            Self::ServiceFailed => &LIFECYCLE_EVENT_DEFINITIONS[2],
            Self::SessionAuthOk => &LIFECYCLE_EVENT_DEFINITIONS[3],
            Self::SessionAuthFail => &LIFECYCLE_EVENT_DEFINITIONS[4],
            Self::SessionStreamStart => &LIFECYCLE_EVENT_DEFINITIONS[5],
            Self::SessionEnd => &LIFECYCLE_EVENT_DEFINITIONS[6],
            Self::SessionInterrupted => &LIFECYCLE_EVENT_DEFINITIONS[7],
            // Appended to the array rather than inserted in id order: every
            // arm here indexes by position, so inserting would silently
            // renumber every event after it.
            Self::CapturePathSelected => &LIFECYCLE_EVENT_DEFINITIONS[8],
            Self::EncoderConfigured => &LIFECYCLE_EVENT_DEFINITIONS[9],
            Self::ColorPlanResolved => &LIFECYCLE_EVENT_DEFINITIONS[10],
            Self::DisplayArmed => &LIFECYCLE_EVENT_DEFINITIONS[11],
            Self::DisplayRestored => &LIFECYCLE_EVENT_DEFINITIONS[12],
            Self::DisplayRestoreDegraded => &LIFECYCLE_EVENT_DEFINITIONS[13],
            Self::DisplayRestoreFailed => &LIFECYCLE_EVENT_DEFINITIONS[14],
            Self::WatchdogRestore => &LIFECYCLE_EVENT_DEFINITIONS[15],
            Self::CpLogonOk => &LIFECYCLE_EVENT_DEFINITIONS[16],
            Self::CpLogonFail => &LIFECYCLE_EVENT_DEFINITIONS[17],
            Self::TlsCertificateActive => &LIFECYCLE_EVENT_DEFINITIONS[18],
            Self::TlsCertificateExpiring => &LIFECYCLE_EVENT_DEFINITIONS[19],
            Self::TlsCertificateReloaded => &LIFECYCLE_EVENT_DEFINITIONS[20],
            Self::TlsCertificateReloadFailed => &LIFECYCLE_EVENT_DEFINITIONS[21],
            Self::TlsCertificateExpired => &LIFECYCLE_EVENT_DEFINITIONS[22],
            Self::ClientStart => &LIFECYCLE_EVENT_DEFINITIONS[23],
            Self::ClientStop => &LIFECYCLE_EVENT_DEFINITIONS[24],
            Self::ClientConnectAttempt => &LIFECYCLE_EVENT_DEFINITIONS[25],
            Self::ClientConnectOk => &LIFECYCLE_EVENT_DEFINITIONS[26],
            Self::ClientConnectFail => &LIFECYCLE_EVENT_DEFINITIONS[27],
            Self::ClientSessionEnd => &LIFECYCLE_EVENT_DEFINITIONS[28],
            Self::ClientReconnect => &LIFECYCLE_EVENT_DEFINITIONS[29],
            Self::HidDeviceAttached => &LIFECYCLE_EVENT_DEFINITIONS[30],
            Self::HidDeviceDetached => &LIFECYCLE_EVENT_DEFINITIONS[31],
            Self::HidPassthroughStart => &LIFECYCLE_EVENT_DEFINITIONS[32],
            Self::HidPassthroughEnd => &LIFECYCLE_EVENT_DEFINITIONS[33],
            Self::HidPassthroughError => &LIFECYCLE_EVENT_DEFINITIONS[34],
            Self::NetworkPathActive => &LIFECYCLE_EVENT_DEFINITIONS[35],
            Self::NetworkPathChanged => &LIFECYCLE_EVENT_DEFINITIONS[36],
            Self::NetworkPathLost => &LIFECYCLE_EVENT_DEFINITIONS[37],
            Self::NetworkPathRestored => &LIFECYCLE_EVENT_DEFINITIONS[38],
            Self::HealthOk => &LIFECYCLE_EVENT_DEFINITIONS[39],
            Self::HealthDegraded => &LIFECYCLE_EVENT_DEFINITIONS[40],
            Self::HealthCritical => &LIFECYCLE_EVENT_DEFINITIONS[41],
            Self::HeartbeatLost => &LIFECYCLE_EVENT_DEFINITIONS[42],
            Self::TelemetryDropped => &LIFECYCLE_EVENT_DEFINITIONS[43],
            Self::EffectiveProfile => &LIFECYCLE_EVENT_DEFINITIONS[44],
            Self::HealthSnapshot => &LIFECYCLE_EVENT_DEFINITIONS[45],
            Self::PermissionGranted => &LIFECYCLE_EVENT_DEFINITIONS[46],
            Self::PermissionDenied => &LIFECYCLE_EVENT_DEFINITIONS[47],
            Self::PermissionRevoked => &LIFECYCLE_EVENT_DEFINITIONS[48],
            Self::PermissionPending => &LIFECYCLE_EVENT_DEFINITIONS[49],
        }
    }
}

/// Cross-platform lifecycle severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleSeverity {
    /// Routine lifecycle information.
    Information,
    /// Degraded or interrupted operation.
    Warning,
    /// Operator-actionable failure.
    Error,
}

impl LifecycleSeverity {
    /// Returns the stable severity name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Information => "information",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// Required value type for a lifecycle field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleFieldType {
    /// Boolean field.
    Boolean,
    /// Signed integer field.
    Integer,
    /// Bounded string field.
    String,
}

/// Whether a lifecycle field must be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleFieldRequirement {
    /// The field must be present.
    Required,
    /// The field may be omitted.
    Optional,
}

/// One entry in a closed lifecycle field schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleFieldSpec {
    /// Stable lowercase field name.
    pub name: &'static str,
    /// Required value type.
    pub field_type: LifecycleFieldType,
    /// Presence requirement.
    pub requirement: LifecycleFieldRequirement,
}

/// Canonical definition of one lifecycle event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LifecycleEventDefinition {
    /// Stable event identifier.
    pub kind: LifecycleEventKind,
    /// Stable uppercase event name.
    pub name: &'static str,
    /// Canonical category.
    pub category: LifecycleCategory,
    /// Canonical outcome.
    pub outcome: EventOutcome,
    /// Canonical severity.
    pub severity: LifecycleSeverity,
    /// Minimum cumulative operational profile that includes this event.
    pub minimum_profile: OperationalProfile,
    /// Closed required/optional field schema.
    pub fields: &'static [LifecycleFieldSpec],
}

const fn required(name: &'static str, field_type: LifecycleFieldType) -> LifecycleFieldSpec {
    LifecycleFieldSpec {
        name,
        field_type,
        requirement: LifecycleFieldRequirement::Required,
    }
}

const fn optional(name: &'static str, field_type: LifecycleFieldType) -> LifecycleFieldSpec {
    LifecycleFieldSpec {
        name,
        field_type,
        requirement: LifecycleFieldRequirement::Optional,
    }
}

const SERVICE_START_FIELDS: &[LifecycleFieldSpec] = &[
    required("component", LifecycleFieldType::String),
    optional("version", LifecycleFieldType::String),
    optional("pid", LifecycleFieldType::Integer),
];
const SERVICE_STOP_FIELDS: &[LifecycleFieldSpec] = &[
    required("component", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
    optional("uptime_ms", LifecycleFieldType::Integer),
];
const SERVICE_FAILED_FIELDS: &[LifecycleFieldSpec] = &[
    required("component", LifecycleFieldType::String),
    required("stage", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
    optional("os_code", LifecycleFieldType::Integer),
];
const SESSION_AUTH_OK_FIELDS: &[LifecycleFieldSpec] = &[
    required("auth_method", LifecycleFieldType::String),
    required("identity_binding", LifecycleFieldType::String),
    optional("os_session_id", LifecycleFieldType::Integer),
    optional("uid", LifecycleFieldType::Integer),
];
const SESSION_AUTH_FAIL_FIELDS: &[LifecycleFieldSpec] = &[
    required("auth_method", LifecycleFieldType::String),
    required("stage", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
];
const SESSION_STREAM_START_FIELDS: &[LifecycleFieldSpec] = &[
    required("encoder", LifecycleFieldType::String),
    required("codec", LifecycleFieldType::String),
    required("chroma", LifecycleFieldType::String),
    required("width", LifecycleFieldType::Integer),
    required("height", LifecycleFieldType::Integer),
    optional("fps", LifecycleFieldType::Integer),
    optional("display_backend", LifecycleFieldType::String),
    // Colour identity. Optional in the schema so an older emitter's events are
    // still accepted rather than rejected wholesale, but both Piers always
    // populate them and have a test saying so.
    //
    // Their absence is what made a session unreadable: `chroma` and `codec`
    // alone describe a stream that could be 8- or 10-bit, BT.709 or PQ, and
    // the answer lived only in the client's log. A host record that cannot
    // state the depth it encoded cannot be used to check any claim about it.
    optional("bit_depth", LifecycleFieldType::String),
    optional("color_range", LifecycleFieldType::String),
    optional("color_matrix", LifecycleFieldType::String),
    optional("color_primaries", LifecycleFieldType::String),
    optional("transfer", LifecycleFieldType::String),
];
const SESSION_END_FIELDS: &[LifecycleFieldSpec] = &[
    required("reason_class", LifecycleFieldType::String),
    required("duration_ms", LifecycleFieldType::Integer),
    optional("frames_sent", LifecycleFieldType::Integer),
    optional("frames_dropped", LifecycleFieldType::Integer),
];
/// Which capture backend served the session, and at what cost.
///
/// `zero_copy` is the field that carries the 8-bit speed argument: `NvFBC` and
/// DDA hand frames to the encoder without a host round trip, and every wide
/// source route measured so far gives that up.
const CAPTURE_PATH_SELECTED_FIELDS: &[LifecycleFieldSpec] = &[
    required("backend", LifecycleFieldType::String),
    required("zero_copy", LifecycleFieldType::Boolean),
    optional("pixel_format", LifecycleFieldType::String),
    optional("bytes_per_frame", LifecycleFieldType::Integer),
    // What was attempted before this backend won, when anything was.
    optional("fallback_from", LifecycleFieldType::String),
    optional("reason", LifecycleFieldType::String),
];
const ENCODER_CONFIGURED_FIELDS: &[LifecycleFieldSpec] = &[
    required("encoder", LifecycleFieldType::String),
    required("pixel_format", LifecycleFieldType::String),
    required("bit_depth", LifecycleFieldType::String),
    required("chroma", LifecycleFieldType::String),
    optional("codec", LifecycleFieldType::String),
    optional("profile", LifecycleFieldType::String),
];
/// Requested versus granted, so a reduction cannot look like a grant.
const COLOR_PLAN_RESOLVED_FIELDS: &[LifecycleFieldSpec] = &[
    required("requested_bit_depth", LifecycleFieldType::String),
    required("granted_bit_depth", LifecycleFieldType::String),
    required("degraded", LifecycleFieldType::Boolean),
    optional("requested_chroma", LifecycleFieldType::String),
    optional("granted_chroma", LifecycleFieldType::String),
    optional("requested_codec", LifecycleFieldType::String),
    optional("granted_codec", LifecycleFieldType::String),
    optional("reason", LifecycleFieldType::String),
];
const SESSION_INTERRUPTED_FIELDS: &[LifecycleFieldSpec] = &[
    required("stage", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
    optional("duration_ms", LifecycleFieldType::Integer),
    optional("frames_sent", LifecycleFieldType::Integer),
    optional("frames_dropped", LifecycleFieldType::Integer),
];
const DISPLAY_ARMED_FIELDS: &[LifecycleFieldSpec] = &[
    required("display_backend", LifecycleFieldType::String),
    required("policy", LifecycleFieldType::String),
    required("changed", LifecycleFieldType::Boolean),
    optional("width", LifecycleFieldType::Integer),
    optional("height", LifecycleFieldType::Integer),
    optional("os_display_id", LifecycleFieldType::String),
];
const DISPLAY_RESTORED_FIELDS: &[LifecycleFieldSpec] = &[
    required("restore_backend", LifecycleFieldType::String),
    required("changed", LifecycleFieldType::Boolean),
    optional("width", LifecycleFieldType::Integer),
    optional("height", LifecycleFieldType::Integer),
];
const DISPLAY_RESTORE_DEGRADED_FIELDS: &[LifecycleFieldSpec] = &[
    required("restore_backend", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
    optional("journal_pending", LifecycleFieldType::Boolean),
];
const DISPLAY_RESTORE_FAILED_FIELDS: &[LifecycleFieldSpec] = &[
    required("restore_backend", LifecycleFieldType::String),
    required("stage", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
    required("journal_pending", LifecycleFieldType::Boolean),
];
const WATCHDOG_RESTORE_FIELDS: &[LifecycleFieldSpec] = &[
    required("restore_backend", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
    optional("journal_pending", LifecycleFieldType::Boolean),
];
const CP_LOGON_OK_FIELDS: &[LifecycleFieldSpec] = &[
    required("stage", LifecycleFieldType::String),
    optional("os_session_id", LifecycleFieldType::Integer),
];
const CP_LOGON_FAIL_FIELDS: &[LifecycleFieldSpec] = &[
    required("stage", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
];
const TLS_CERTIFICATE_FIELDS: &[LifecycleFieldSpec] = &[
    required("component", LifecycleFieldType::String),
    required("source", LifecycleFieldType::String),
    required("cert_sha256", LifecycleFieldType::String),
    required("spki_sha256", LifecycleFieldType::String),
    required("key_algorithm", LifecycleFieldType::String),
    required("key_bits", LifecycleFieldType::Integer),
    required("not_after_epoch_secs", LifecycleFieldType::Integer),
];
const TLS_CERTIFICATE_EXPIRING_FIELDS: &[LifecycleFieldSpec] = &[
    required("component", LifecycleFieldType::String),
    required("source", LifecycleFieldType::String),
    required("cert_sha256", LifecycleFieldType::String),
    required("spki_sha256", LifecycleFieldType::String),
    required("key_algorithm", LifecycleFieldType::String),
    required("key_bits", LifecycleFieldType::Integer),
    required("not_after_epoch_secs", LifecycleFieldType::Integer),
    required("days_remaining", LifecycleFieldType::Integer),
];
const TLS_CERTIFICATE_RELOAD_FAILED_FIELDS: &[LifecycleFieldSpec] = &[
    required("component", LifecycleFieldType::String),
    required("source", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
];
const CLIENT_START_FIELDS: &[LifecycleFieldSpec] = &[
    required("version", LifecycleFieldType::String),
    required("os", LifecycleFieldType::String),
    required("arch", LifecycleFieldType::String),
];
const CLIENT_STOP_FIELDS: &[LifecycleFieldSpec] = &[
    required("uptime_ms", LifecycleFieldType::Integer),
    optional("reason_class", LifecycleFieldType::String),
];
const CLIENT_CONNECT_ATTEMPT_FIELDS: &[LifecycleFieldSpec] =
    &[required("transport", LifecycleFieldType::String)];
const CLIENT_CONNECT_OK_FIELDS: &[LifecycleFieldSpec] = &[
    required("tls_version", LifecycleFieldType::String),
    optional("rtt_ms", LifecycleFieldType::Integer),
];
const CLIENT_CONNECT_FAIL_FIELDS: &[LifecycleFieldSpec] = &[
    required("reason_class", LifecycleFieldType::String),
    optional("stage", LifecycleFieldType::String),
];
const CLIENT_SESSION_END_FIELDS: &[LifecycleFieldSpec] = &[
    required("duration_ms", LifecycleFieldType::Integer),
    required("reason_class", LifecycleFieldType::String),
    required("frames_decoded", LifecycleFieldType::Integer),
    required("frames_dropped", LifecycleFieldType::Integer),
    optional("avg_fps", LifecycleFieldType::Integer),
    optional("avg_rtt_ms", LifecycleFieldType::Integer),
    optional("worst_health", LifecycleFieldType::String),
    optional("reconnects", LifecycleFieldType::Integer),
];
const CLIENT_RECONNECT_FIELDS: &[LifecycleFieldSpec] = &[
    required("attempt", LifecycleFieldType::Integer),
    required("gap_ms", LifecycleFieldType::Integer),
    optional("reason_class", LifecycleFieldType::String),
];
const HID_DEVICE_FIELDS: &[LifecycleFieldSpec] = &[
    required("vendor_id", LifecycleFieldType::Integer),
    required("product_id", LifecycleFieldType::Integer),
    optional("brand", LifecycleFieldType::String),
    optional("firmware", LifecycleFieldType::String),
    optional("transport", LifecycleFieldType::String),
];
const HID_PASSTHROUGH_START_FIELDS: &[LifecycleFieldSpec] = &[
    required("device_id", LifecycleFieldType::String),
    required("vendor_id", LifecycleFieldType::Integer),
    required("product_id", LifecycleFieldType::Integer),
];
const HID_PASSTHROUGH_END_FIELDS: &[LifecycleFieldSpec] = &[
    required("device_id", LifecycleFieldType::String),
    required("reports_forwarded", LifecycleFieldType::Integer),
    optional("errors", LifecycleFieldType::Integer),
];
const HID_PASSTHROUGH_ERROR_FIELDS: &[LifecycleFieldSpec] = &[
    required("device_id", LifecycleFieldType::String),
    required("reason_class", LifecycleFieldType::String),
];
const NETWORK_PATH_ACTIVE_FIELDS: &[LifecycleFieldSpec] = &[
    required("interface_kind", LifecycleFieldType::String),
    required("scope", LifecycleFieldType::String),
    optional("link_mbps", LifecycleFieldType::Integer),
    optional("ssid", LifecycleFieldType::String),
    optional("rssi_dbm", LifecycleFieldType::Integer),
    optional("mtu", LifecycleFieldType::Integer),
];
const NETWORK_PATH_CHANGED_FIELDS: &[LifecycleFieldSpec] = &[
    required("old_kind", LifecycleFieldType::String),
    required("new_kind", LifecycleFieldType::String),
    optional("old_mbps", LifecycleFieldType::Integer),
    optional("new_mbps", LifecycleFieldType::Integer),
    optional("reason_class", LifecycleFieldType::String),
];
const NETWORK_PATH_LOST_FIELDS: &[LifecycleFieldSpec] =
    &[required("interface_kind", LifecycleFieldType::String)];
const NETWORK_PATH_RESTORED_FIELDS: &[LifecycleFieldSpec] = &[
    required("interface_kind", LifecycleFieldType::String),
    required("gap_ms", LifecycleFieldType::Integer),
];
const HEALTH_OK_FIELDS: &[LifecycleFieldSpec] = &[
    required("previous_state", LifecycleFieldType::String),
    required("degraded_duration_ms", LifecycleFieldType::Integer),
];
const HEALTH_STATE_FIELDS: &[LifecycleFieldSpec] = &[
    required("dominant_cause", LifecycleFieldType::String),
    required("value", LifecycleFieldType::Integer),
    optional("threshold", LifecycleFieldType::Integer),
];
const HEARTBEAT_LOST_FIELDS: &[LifecycleFieldSpec] =
    &[required("missed_intervals", LifecycleFieldType::Integer)];
const TELEMETRY_DROPPED_FIELDS: &[LifecycleFieldSpec] = &[
    required("sink", LifecycleFieldType::String),
    required("dropped_count", LifecycleFieldType::Integer),
];
const EFFECTIVE_PROFILE_FIELDS: &[LifecycleFieldSpec] = &[
    required("profile_level", LifecycleFieldType::Integer),
    required("profile_name", LifecycleFieldType::String),
    required("profile_source", LifecycleFieldType::String),
];
const HEALTH_SNAPSHOT_FIELDS: &[LifecycleFieldSpec] = &[
    required("overall_state", LifecycleFieldType::String),
    optional("host_state", LifecycleFieldType::String),
    optional("client_state", LifecycleFieldType::String),
    optional("fps_actual", LifecycleFieldType::Integer),
    optional("fps_target", LifecycleFieldType::Integer),
    optional("rtt_ms", LifecycleFieldType::Integer),
    optional("drop_basis_points", LifecycleFieldType::Integer),
    optional("heartbeat_misses", LifecycleFieldType::Integer),
];
const PERMISSION_FIELDS: &[LifecycleFieldSpec] = &[
    required("permission", LifecycleFieldType::String),
    required("platform", LifecycleFieldType::String),
];

/// Append-only v1 lifecycle event definitions, sorted by numeric identifier.
pub static LIFECYCLE_EVENT_DEFINITIONS: &[LifecycleEventDefinition; 50] = &[
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ServiceStart,
        name: "SERVICE_START",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: SERVICE_START_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ServiceStop,
        name: "SERVICE_STOP",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: SERVICE_STOP_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ServiceFailed,
        name: "SERVICE_FAILED",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Critical,
        fields: SERVICE_FAILED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::SessionAuthOk,
        name: "SESSION_AUTH_OK",
        category: LifecycleCategory::MachineAuthentication,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: SESSION_AUTH_OK_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::SessionAuthFail,
        name: "SESSION_AUTH_FAIL",
        category: LifecycleCategory::MachineAuthentication,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Critical,
        fields: SESSION_AUTH_FAIL_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::SessionStreamStart,
        name: "SESSION_STREAM_START",
        category: LifecycleCategory::Streaming,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: SESSION_STREAM_START_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::SessionEnd,
        name: "SESSION_END",
        category: LifecycleCategory::Cleanup,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: SESSION_END_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::SessionInterrupted,
        name: "SESSION_INTERRUPTED",
        category: LifecycleCategory::Reconnect,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Critical,
        fields: SESSION_INTERRUPTED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::CapturePathSelected,
        name: "CAPTURE_PATH_SELECTED",
        category: LifecycleCategory::Streaming,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Debug,
        fields: CAPTURE_PATH_SELECTED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::EncoderConfigured,
        name: "ENCODER_CONFIGURED",
        category: LifecycleCategory::Streaming,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Debug,
        fields: ENCODER_CONFIGURED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ColorPlanResolved,
        name: "COLOR_PLAN_RESOLVED",
        category: LifecycleCategory::Streaming,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Debug,
        fields: COLOR_PLAN_RESOLVED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::DisplayArmed,
        name: "DISPLAY_ARMED",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Started,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: DISPLAY_ARMED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::DisplayRestored,
        name: "DISPLAY_RESTORED",
        category: LifecycleCategory::Cleanup,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: DISPLAY_RESTORED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::DisplayRestoreDegraded,
        name: "DISPLAY_RESTORE_DEGRADED",
        category: LifecycleCategory::Cleanup,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Error,
        fields: DISPLAY_RESTORE_DEGRADED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::DisplayRestoreFailed,
        name: "DISPLAY_RESTORE_FAILED",
        category: LifecycleCategory::Cleanup,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Critical,
        fields: DISPLAY_RESTORE_FAILED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::WatchdogRestore,
        name: "WATCHDOG_RESTORE",
        category: LifecycleCategory::Cleanup,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Critical,
        fields: WATCHDOG_RESTORE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::CpLogonOk,
        name: "CP_LOGON_OK",
        category: LifecycleCategory::MachineAuthentication,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: CP_LOGON_OK_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::CpLogonFail,
        name: "CP_LOGON_FAIL",
        category: LifecycleCategory::MachineAuthentication,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Critical,
        fields: CP_LOGON_FAIL_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::TlsCertificateActive,
        name: "TLS_CERTIFICATE_ACTIVE",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: TLS_CERTIFICATE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::TlsCertificateExpiring,
        name: "TLS_CERTIFICATE_EXPIRING",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Started,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Error,
        fields: TLS_CERTIFICATE_EXPIRING_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::TlsCertificateReloaded,
        name: "TLS_CERTIFICATE_RELOADED",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: TLS_CERTIFICATE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::TlsCertificateReloadFailed,
        name: "TLS_CERTIFICATE_RELOAD_FAILED",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Critical,
        fields: TLS_CERTIFICATE_RELOAD_FAILED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::TlsCertificateExpired,
        name: "TLS_CERTIFICATE_EXPIRED",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Critical,
        fields: TLS_CERTIFICATE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ClientStart,
        name: "CLIENT_START",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: CLIENT_START_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ClientStop,
        name: "CLIENT_STOP",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: CLIENT_STOP_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ClientConnectAttempt,
        name: "CLIENT_CONNECT_ATTEMPT",
        category: LifecycleCategory::Connection,
        outcome: EventOutcome::Started,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: CLIENT_CONNECT_ATTEMPT_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ClientConnectOk,
        name: "CLIENT_CONNECT_OK",
        category: LifecycleCategory::Connection,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: CLIENT_CONNECT_OK_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ClientConnectFail,
        name: "CLIENT_CONNECT_FAIL",
        category: LifecycleCategory::Connection,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Critical,
        fields: CLIENT_CONNECT_FAIL_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ClientSessionEnd,
        name: "CLIENT_SESSION_END",
        category: LifecycleCategory::Cleanup,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: CLIENT_SESSION_END_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::ClientReconnect,
        name: "CLIENT_RECONNECT",
        category: LifecycleCategory::Reconnect,
        outcome: EventOutcome::Started,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Error,
        fields: CLIENT_RECONNECT_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HidDeviceAttached,
        name: "HID_DEVICE_ATTACHED",
        category: LifecycleCategory::Peripheral,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: HID_DEVICE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HidDeviceDetached,
        name: "HID_DEVICE_DETACHED",
        category: LifecycleCategory::Peripheral,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: HID_DEVICE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HidPassthroughStart,
        name: "HID_PASSTHROUGH_START",
        category: LifecycleCategory::Peripheral,
        outcome: EventOutcome::Started,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: HID_PASSTHROUGH_START_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HidPassthroughEnd,
        name: "HID_PASSTHROUGH_END",
        category: LifecycleCategory::Peripheral,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: HID_PASSTHROUGH_END_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HidPassthroughError,
        name: "HID_PASSTHROUGH_ERROR",
        category: LifecycleCategory::Peripheral,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Error,
        fields: HID_PASSTHROUGH_ERROR_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::NetworkPathActive,
        name: "NETWORK_PATH_ACTIVE",
        category: LifecycleCategory::Network,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: NETWORK_PATH_ACTIVE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::NetworkPathChanged,
        name: "NETWORK_PATH_CHANGED",
        category: LifecycleCategory::Network,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: NETWORK_PATH_CHANGED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::NetworkPathLost,
        name: "NETWORK_PATH_LOST",
        category: LifecycleCategory::Network,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Error,
        fields: NETWORK_PATH_LOST_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::NetworkPathRestored,
        name: "NETWORK_PATH_RESTORED",
        category: LifecycleCategory::Network,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Error,
        fields: NETWORK_PATH_RESTORED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HealthOk,
        name: "HEALTH_OK",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: HEALTH_OK_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HealthDegraded,
        name: "HEALTH_DEGRADED",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Started,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Critical,
        fields: HEALTH_STATE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HealthCritical,
        name: "HEALTH_CRITICAL",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Critical,
        fields: HEALTH_STATE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HeartbeatLost,
        name: "HEARTBEAT_LOST",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Error,
        minimum_profile: OperationalProfile::Critical,
        fields: HEARTBEAT_LOST_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::TelemetryDropped,
        name: "TELEMETRY_DROPPED",
        category: LifecycleCategory::Telemetry,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Critical,
        fields: TELEMETRY_DROPPED_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::EffectiveProfile,
        name: "EFFECTIVE_PROFILE",
        category: LifecycleCategory::Telemetry,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: EFFECTIVE_PROFILE_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::HealthSnapshot,
        name: "HEALTH_SNAPSHOT",
        category: LifecycleCategory::Health,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Critical,
        fields: HEALTH_SNAPSHOT_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::PermissionGranted,
        name: "PERMISSION_GRANTED",
        category: LifecycleCategory::Permission,
        outcome: EventOutcome::Succeeded,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: PERMISSION_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::PermissionDenied,
        name: "PERMISSION_DENIED",
        category: LifecycleCategory::Permission,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Error,
        fields: PERMISSION_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::PermissionRevoked,
        name: "PERMISSION_REVOKED",
        category: LifecycleCategory::Permission,
        outcome: EventOutcome::Failed,
        severity: LifecycleSeverity::Warning,
        minimum_profile: OperationalProfile::Error,
        fields: PERMISSION_FIELDS,
    },
    LifecycleEventDefinition {
        kind: LifecycleEventKind::PermissionPending,
        name: "PERMISSION_PENDING",
        category: LifecycleCategory::Permission,
        outcome: EventOutcome::Started,
        severity: LifecycleSeverity::Information,
        minimum_profile: OperationalProfile::Info,
        fields: PERMISSION_FIELDS,
    },
];

/// Returns the definition for a stable numeric identifier.
#[must_use]
pub const fn lifecycle_event_definition(id: u32) -> Option<&'static LifecycleEventDefinition> {
    match id {
        1000 => Some(&LIFECYCLE_EVENT_DEFINITIONS[0]),
        1001 => Some(&LIFECYCLE_EVENT_DEFINITIONS[1]),
        1002 => Some(&LIFECYCLE_EVENT_DEFINITIONS[2]),
        1100 => Some(&LIFECYCLE_EVENT_DEFINITIONS[3]),
        1101 => Some(&LIFECYCLE_EVENT_DEFINITIONS[4]),
        1102 => Some(&LIFECYCLE_EVENT_DEFINITIONS[5]),
        1103 => Some(&LIFECYCLE_EVENT_DEFINITIONS[6]),
        1104 => Some(&LIFECYCLE_EVENT_DEFINITIONS[7]),
        1105 => Some(&LIFECYCLE_EVENT_DEFINITIONS[8]),
        1106 => Some(&LIFECYCLE_EVENT_DEFINITIONS[9]),
        1107 => Some(&LIFECYCLE_EVENT_DEFINITIONS[10]),
        1200 => Some(&LIFECYCLE_EVENT_DEFINITIONS[11]),
        1201 => Some(&LIFECYCLE_EVENT_DEFINITIONS[12]),
        1202 => Some(&LIFECYCLE_EVENT_DEFINITIONS[13]),
        1203 => Some(&LIFECYCLE_EVENT_DEFINITIONS[14]),
        1204 => Some(&LIFECYCLE_EVENT_DEFINITIONS[15]),
        1300 => Some(&LIFECYCLE_EVENT_DEFINITIONS[16]),
        1301 => Some(&LIFECYCLE_EVENT_DEFINITIONS[17]),
        1400 => Some(&LIFECYCLE_EVENT_DEFINITIONS[18]),
        1401 => Some(&LIFECYCLE_EVENT_DEFINITIONS[19]),
        1402 => Some(&LIFECYCLE_EVENT_DEFINITIONS[20]),
        1403 => Some(&LIFECYCLE_EVENT_DEFINITIONS[21]),
        1404 => Some(&LIFECYCLE_EVENT_DEFINITIONS[22]),
        1500 => Some(&LIFECYCLE_EVENT_DEFINITIONS[23]),
        1501 => Some(&LIFECYCLE_EVENT_DEFINITIONS[24]),
        1502 => Some(&LIFECYCLE_EVENT_DEFINITIONS[25]),
        1503 => Some(&LIFECYCLE_EVENT_DEFINITIONS[26]),
        1504 => Some(&LIFECYCLE_EVENT_DEFINITIONS[27]),
        1505 => Some(&LIFECYCLE_EVENT_DEFINITIONS[28]),
        1506 => Some(&LIFECYCLE_EVENT_DEFINITIONS[29]),
        1600 => Some(&LIFECYCLE_EVENT_DEFINITIONS[30]),
        1601 => Some(&LIFECYCLE_EVENT_DEFINITIONS[31]),
        1602 => Some(&LIFECYCLE_EVENT_DEFINITIONS[32]),
        1603 => Some(&LIFECYCLE_EVENT_DEFINITIONS[33]),
        1604 => Some(&LIFECYCLE_EVENT_DEFINITIONS[34]),
        1700 => Some(&LIFECYCLE_EVENT_DEFINITIONS[35]),
        1701 => Some(&LIFECYCLE_EVENT_DEFINITIONS[36]),
        1702 => Some(&LIFECYCLE_EVENT_DEFINITIONS[37]),
        1703 => Some(&LIFECYCLE_EVENT_DEFINITIONS[38]),
        1800 => Some(&LIFECYCLE_EVENT_DEFINITIONS[39]),
        1801 => Some(&LIFECYCLE_EVENT_DEFINITIONS[40]),
        1802 => Some(&LIFECYCLE_EVENT_DEFINITIONS[41]),
        1803 => Some(&LIFECYCLE_EVENT_DEFINITIONS[42]),
        1804 => Some(&LIFECYCLE_EVENT_DEFINITIONS[43]),
        1805 => Some(&LIFECYCLE_EVENT_DEFINITIONS[44]),
        1806 => Some(&LIFECYCLE_EVENT_DEFINITIONS[45]),
        1900 => Some(&LIFECYCLE_EVENT_DEFINITIONS[46]),
        1901 => Some(&LIFECYCLE_EVENT_DEFINITIONS[47]),
        1902 => Some(&LIFECYCLE_EVENT_DEFINITIONS[48]),
        1903 => Some(&LIFECYCLE_EVENT_DEFINITIONS[49]),
        _ => None,
    }
}

/// Strict lifecycle-schema validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LifecycleValidationError {
    /// A required field is absent.
    MissingRequiredField(&'static str),
    /// A field is not declared by the event schema.
    UndeclaredField(String),
    /// A field has a value type different from its schema.
    WrongFieldType {
        /// Field name.
        field: &'static str,
        /// Required type.
        expected: LifecycleFieldType,
    },
    /// The raw field container exceeds the lifecycle bound.
    TooManyFields(usize),
}

impl Display for LifecycleValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingRequiredField(field) => {
                write!(formatter, "required lifecycle field `{field}` is missing")
            }
            Self::UndeclaredField(field) => {
                write!(formatter, "lifecycle field `{field}` is not declared")
            }
            Self::WrongFieldType { field, expected } => {
                write!(
                    formatter,
                    "lifecycle field `{field}` does not have required type {expected:?}"
                )
            }
            Self::TooManyFields(count) => {
                write!(formatter, "lifecycle field count {count} exceeds its cap")
            }
        }
    }
}

impl Error for LifecycleValidationError {}

/// Lifecycle event proven to match its stable definition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedLifecycleEvent {
    kind: LifecycleEventKind,
    event: LifecycleEvent,
}

impl ValidatedLifecycleEvent {
    /// Builds an event from its canonical definition and validates its closed schema.
    ///
    /// # Errors
    ///
    /// Returns an error for a missing, undeclared, mistyped, or unbounded field set.
    pub fn new(
        kind: LifecycleEventKind,
        correlation_id: CorrelationId,
        fields: StructuredFields,
    ) -> Result<Self, LifecycleValidationError> {
        let definition = kind.definition();
        if fields.len() > MAX_STRUCTURED_FIELDS {
            return Err(LifecycleValidationError::TooManyFields(fields.len()));
        }
        for spec in definition.fields {
            match fields.as_map().get(spec.name) {
                None if spec.requirement == LifecycleFieldRequirement::Required => {
                    return Err(LifecycleValidationError::MissingRequiredField(spec.name));
                }
                Some(value) if !field_type_matches(spec.field_type, value) => {
                    return Err(LifecycleValidationError::WrongFieldType {
                        field: spec.name,
                        expected: spec.field_type,
                    });
                }
                None | Some(_) => {}
            }
        }
        for name in fields.as_map().keys() {
            if !definition.fields.iter().any(|spec| spec.name == name) {
                return Err(LifecycleValidationError::UndeclaredField(name.clone()));
            }
        }
        Ok(Self {
            kind,
            event: LifecycleEvent {
                correlation_id,
                category: definition.category,
                outcome: definition.outcome,
                fields,
            },
        })
    }

    /// Returns the stable event kind.
    #[must_use]
    pub const fn kind(&self) -> LifecycleEventKind {
        self.kind
    }

    /// Returns the canonical event definition.
    #[must_use]
    pub const fn definition(&self) -> &'static LifecycleEventDefinition {
        self.kind.definition()
    }

    /// Returns the source-compatible raw event.
    #[must_use]
    pub const fn event(&self) -> &LifecycleEvent {
        &self.event
    }

    /// Returns the correlation identifier.
    #[must_use]
    pub const fn correlation_id(&self) -> &CorrelationId {
        &self.event.correlation_id
    }

    /// Returns the validated fields.
    #[must_use]
    pub const fn fields(&self) -> &StructuredFields {
        &self.event.fields
    }
}

const fn field_type_matches(field_type: LifecycleFieldType, value: &FieldValue) -> bool {
    matches!(
        (field_type, value),
        (LifecycleFieldType::Boolean, FieldValue::Boolean(_))
            | (LifecycleFieldType::Integer, FieldValue::Integer(_))
            | (LifecycleFieldType::String, FieldValue::String(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXISTING_GOLDEN: &[(u32, &str)] = &[
        (1000, "SERVICE_START"),
        (1001, "SERVICE_STOP"),
        (1002, "SERVICE_FAILED"),
        (1100, "SESSION_AUTH_OK"),
        (1101, "SESSION_AUTH_FAIL"),
        (1102, "SESSION_STREAM_START"),
        (1103, "SESSION_END"),
        (1104, "SESSION_INTERRUPTED"),
        (1105, "CAPTURE_PATH_SELECTED"),
        (1106, "ENCODER_CONFIGURED"),
        (1107, "COLOR_PLAN_RESOLVED"),
        (1200, "DISPLAY_ARMED"),
        (1201, "DISPLAY_RESTORED"),
        (1202, "DISPLAY_RESTORE_DEGRADED"),
        (1203, "DISPLAY_RESTORE_FAILED"),
        (1204, "WATCHDOG_RESTORE"),
        (1300, "CP_LOGON_OK"),
        (1301, "CP_LOGON_FAIL"),
        (1400, "TLS_CERTIFICATE_ACTIVE"),
        (1401, "TLS_CERTIFICATE_EXPIRING"),
        (1402, "TLS_CERTIFICATE_RELOADED"),
        (1403, "TLS_CERTIFICATE_RELOAD_FAILED"),
        (1404, "TLS_CERTIFICATE_EXPIRED"),
    ];
    const NEW_GOLDEN: &[(u32, &str)] = &[
        (1500, "CLIENT_START"),
        (1501, "CLIENT_STOP"),
        (1502, "CLIENT_CONNECT_ATTEMPT"),
        (1503, "CLIENT_CONNECT_OK"),
        (1504, "CLIENT_CONNECT_FAIL"),
        (1505, "CLIENT_SESSION_END"),
        (1506, "CLIENT_RECONNECT"),
        (1600, "HID_DEVICE_ATTACHED"),
        (1601, "HID_DEVICE_DETACHED"),
        (1602, "HID_PASSTHROUGH_START"),
        (1603, "HID_PASSTHROUGH_END"),
        (1604, "HID_PASSTHROUGH_ERROR"),
        (1700, "NETWORK_PATH_ACTIVE"),
        (1701, "NETWORK_PATH_CHANGED"),
        (1702, "NETWORK_PATH_LOST"),
        (1703, "NETWORK_PATH_RESTORED"),
        (1800, "HEALTH_OK"),
        (1801, "HEALTH_DEGRADED"),
        (1802, "HEALTH_CRITICAL"),
        (1803, "HEARTBEAT_LOST"),
        (1804, "TELEMETRY_DROPPED"),
        (1805, "EFFECTIVE_PROFILE"),
        (1806, "HEALTH_SNAPSHOT"),
        (1900, "PERMISSION_GRANTED"),
        (1901, "PERMISSION_DENIED"),
        (1902, "PERMISSION_REVOKED"),
        (1903, "PERMISSION_PENDING"),
    ];

    fn correlation_id() -> CorrelationId {
        CorrelationId::parse_uuid("01234567-89ab-4def-8123-456789abcdef")
            .expect("canonical correlation ID")
    }

    fn valid_fields(definition: &LifecycleEventDefinition) -> StructuredFields {
        let mut fields = StructuredFields::default();
        for spec in definition.fields {
            if spec.requirement == LifecycleFieldRequirement::Required {
                let value = match spec.field_type {
                    LifecycleFieldType::Boolean => FieldValue::Boolean(false),
                    LifecycleFieldType::Integer => FieldValue::Integer(0),
                    LifecycleFieldType::String => FieldValue::String("bounded".to_owned()),
                };
                fields.insert(spec.name, value).expect("valid schema field");
            }
        }
        fields
    }

    #[test]
    fn definitions_match_append_only_golden_table() {
        let actual: Vec<_> = LIFECYCLE_EVENT_DEFINITIONS
            .iter()
            .map(|definition| (definition.kind.id(), definition.name))
            .collect();
        let expected: Vec<_> = EXISTING_GOLDEN.iter().chain(NEW_GOLDEN).copied().collect();
        assert_eq!(actual, expected);
        assert!(actual.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(actual.iter().all(|(id, name)| {
            u16::try_from(*id).is_ok()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        }));
    }

    #[test]
    fn lookup_and_kind_accessors_are_stable() {
        for definition in LIFECYCLE_EVENT_DEFINITIONS {
            assert_eq!(
                lifecycle_event_definition(definition.kind.id()),
                Some(definition)
            );
            assert_eq!(definition.kind.name(), definition.name);
            assert_eq!(definition.kind.definition(), definition);
        }
        assert_eq!(lifecycle_event_definition(999), None);
        assert_eq!(lifecycle_event_definition(1405), None);
        assert_eq!(lifecycle_event_definition(1904), None);
        assert_eq!(lifecycle_event_definition(2010), None);
    }

    #[test]
    fn every_definition_constructs_its_canonical_raw_event() {
        for definition in LIFECYCLE_EVENT_DEFINITIONS {
            let fields = valid_fields(definition);
            let event =
                ValidatedLifecycleEvent::new(definition.kind, correlation_id(), fields.clone())
                    .expect("definition should accept canonical fields");
            assert_eq!(event.definition(), definition);
            assert_eq!(event.event().category, definition.category);
            assert_eq!(event.event().outcome, definition.outcome);
            assert_eq!(event.fields(), &fields);
        }
    }

    #[test]
    fn validation_rejects_missing_wrong_and_undeclared_fields() {
        let kind = LifecycleEventKind::ServiceStart;
        assert_eq!(
            ValidatedLifecycleEvent::new(kind, correlation_id(), StructuredFields::default()),
            Err(LifecycleValidationError::MissingRequiredField("component"))
        );

        let mut wrong = StructuredFields::default();
        wrong
            .insert("component", FieldValue::Integer(1))
            .expect("valid base field");
        assert_eq!(
            ValidatedLifecycleEvent::new(kind, correlation_id(), wrong),
            Err(LifecycleValidationError::WrongFieldType {
                field: "component",
                expected: LifecycleFieldType::String,
            })
        );

        let mut undeclared = valid_fields(kind.definition());
        undeclared
            .insert("unexpected", FieldValue::Boolean(true))
            .expect("valid base field");
        assert_eq!(
            ValidatedLifecycleEvent::new(kind, correlation_id(), undeclared),
            Err(LifecycleValidationError::UndeclaredField(
                "unexpected".to_owned()
            ))
        );
    }

    #[test]
    fn severity_names_and_definition_mappings_are_stable() {
        assert_eq!(LifecycleSeverity::Information.as_str(), "information");
        assert_eq!(LifecycleSeverity::Warning.as_str(), "warning");
        assert_eq!(LifecycleSeverity::Error.as_str(), "error");
        assert_eq!(
            LifecycleEventKind::ServiceFailed.definition().severity,
            LifecycleSeverity::Error
        );
        assert_eq!(
            LifecycleEventKind::SessionInterrupted.definition().severity,
            LifecycleSeverity::Warning
        );
        assert_eq!(
            LifecycleEventKind::DisplayArmed.definition().outcome,
            EventOutcome::Started
        );
    }

    #[test]
    fn minimum_profiles_are_explicit_and_cumulative() {
        assert_eq!(
            LifecycleEventKind::ServiceStart
                .definition()
                .minimum_profile,
            OperationalProfile::Critical
        );
        assert_eq!(
            LifecycleEventKind::PermissionDenied
                .definition()
                .minimum_profile,
            OperationalProfile::Error
        );
        assert_eq!(
            LifecycleEventKind::NetworkPathActive
                .definition()
                .minimum_profile,
            OperationalProfile::Info
        );
        for definition in LIFECYCLE_EVENT_DEFINITIONS {
            assert!(
                OperationalProfile::Debug.includes(definition.minimum_profile),
                "{} must be included at Debug",
                definition.name
            );
        }
    }

    #[test]
    fn tls_certificate_schemas_are_privacy_bounded_and_exact() {
        const CERTIFICATE_FIELDS: &[&str] = &[
            "component",
            "source",
            "cert_sha256",
            "spki_sha256",
            "key_algorithm",
            "key_bits",
            "not_after_epoch_secs",
        ];
        const EXPIRING_FIELDS: &[&str] = &[
            "component",
            "source",
            "cert_sha256",
            "spki_sha256",
            "key_algorithm",
            "key_bits",
            "not_after_epoch_secs",
            "days_remaining",
        ];
        const RELOAD_FAILED_FIELDS: &[&str] = &["component", "source", "reason_class"];

        for kind in [
            LifecycleEventKind::TlsCertificateActive,
            LifecycleEventKind::TlsCertificateReloaded,
            LifecycleEventKind::TlsCertificateExpired,
        ] {
            assert_eq!(
                kind.definition()
                    .fields
                    .iter()
                    .map(|field| field.name)
                    .collect::<Vec<_>>(),
                CERTIFICATE_FIELDS
            );
        }
        assert_eq!(
            LifecycleEventKind::TlsCertificateExpiring
                .definition()
                .fields
                .iter()
                .map(|field| field.name)
                .collect::<Vec<_>>(),
            EXPIRING_FIELDS
        );
        assert_eq!(
            LifecycleEventKind::TlsCertificateReloadFailed
                .definition()
                .fields
                .iter()
                .map(|field| field.name)
                .collect::<Vec<_>>(),
            RELOAD_FAILED_FIELDS
        );
    }
}
