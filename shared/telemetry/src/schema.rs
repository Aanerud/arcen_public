//! Canonical, deterministic JSON Lines schema-v1 records.

use std::error::Error;
use std::fmt::{Display, Formatter};

use serde::Serialize;

use crate::{
    CorrelationId, HealthState, LifecycleEventKind, LifecycleSeverity, OperationalProfile,
    StructuredFields,
};

/// Frozen canonical record schema version.
pub const CANONICAL_SCHEMA_VERSION: u16 = 1;
/// Maximum canonical message length.
pub const MAX_MESSAGE_BYTES: usize = 512;
/// Maximum schema-approved identity value length.
pub const MAX_IDENTITY_BYTES: usize = 256;

/// True diagnostic severity, independent of the operational profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSeverity {
    /// Detailed diagnostics.
    Debug,
    /// Normal operation.
    Info,
    /// Degraded operation.
    Warn,
    /// Failed operation.
    Error,
}

impl EventSeverity {
    /// Returns the canonical severity name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Debug => "debug",
            Self::Info => "info",
            Self::Warn => "warn",
            Self::Error => "error",
        }
    }
}

impl From<LifecycleSeverity> for EventSeverity {
    fn from(value: LifecycleSeverity) -> Self {
        match value {
            LifecycleSeverity::Information => Self::Info,
            LifecycleSeverity::Warning => Self::Warn,
            LifecycleSeverity::Error => Self::Error,
        }
    }
}

/// Process role in the common monitoring schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryRole {
    /// A session-hosting Pier.
    Host,
    /// A user-facing Deck.
    Client,
    /// A future relay or aggregation service.
    Gateway,
}

/// Operating-system platform in the common monitoring schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TelemetryPlatform {
    /// Linux.
    Linux,
    /// macOS.
    Macos,
    /// Windows.
    Windows,
}

/// Validated lowercase component value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TelemetryComponent(String);

impl TelemetryComponent {
    /// Creates a bounded lowercase snake-case component.
    ///
    /// # Errors
    ///
    /// Returns an error when the value is empty, oversized, or non-canonical.
    pub fn new(value: impl Into<String>) -> Result<Self, SchemaValidationError> {
        let value = value.into();
        if !is_snake_name(&value, 32) {
            return Err(SchemaValidationError::InvalidComponent);
        }
        Ok(Self(value))
    }

    /// Returns the canonical value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Validated canonical `arcen::` tracing target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct TelemetryTarget(String);

impl TelemetryTarget {
    /// Creates a bounded canonical target.
    ///
    /// # Errors
    ///
    /// Returns an error unless every `::`-separated segment is lowercase
    /// snake case and the root segment is `arcen`.
    pub fn new(value: impl Into<String>) -> Result<Self, SchemaValidationError> {
        let value = value.into();
        if value.len() > 64
            || !value.starts_with("arcen::")
            || value.split("::").any(|part| !is_snake_name(part, 32))
        {
            return Err(SchemaValidationError::InvalidTarget);
        }
        Ok(Self(value))
    }

    /// Returns the canonical value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Canonical schema-v1 record.
///
/// Field declaration order is the frozen JSON key order. Optional values are
/// encoded as JSON `null`, preserving one common shape across platforms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CanonicalRecord {
    schema_version: u16,
    timestamp: String,
    sequence: u64,
    profile_level: u8,
    profile_name: &'static str,
    #[serde(skip)]
    minimum_profile: OperationalProfile,
    severity: EventSeverity,
    role: TelemetryRole,
    component: TelemetryComponent,
    platform: TelemetryPlatform,
    target: TelemetryTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    event_name: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    sid: Option<CorrelationId>,
    user: Option<String>,
    host: Option<String>,
    peer_addr: Option<String>,
    health_state: Option<HealthState>,
    message: String,
    fields: StructuredFields,
}

impl CanonicalRecord {
    /// Creates a validated ad-hoc diagnostic record.
    ///
    /// # Errors
    ///
    /// Returns an error for a non-canonical timestamp or unbounded message.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        timestamp: impl Into<String>,
        sequence: u64,
        minimum_profile: OperationalProfile,
        severity: EventSeverity,
        role: TelemetryRole,
        component: TelemetryComponent,
        platform: TelemetryPlatform,
        target: TelemetryTarget,
        message: impl Into<String>,
    ) -> Result<Self, SchemaValidationError> {
        let timestamp = timestamp.into();
        if !is_canonical_timestamp(&timestamp) {
            return Err(SchemaValidationError::InvalidTimestamp);
        }
        let message = message.into();
        validate_text(&message, MAX_MESSAGE_BYTES)
            .map_err(|()| SchemaValidationError::InvalidMessage)?;
        Ok(Self {
            schema_version: CANONICAL_SCHEMA_VERSION,
            timestamp,
            sequence,
            profile_level: minimum_profile.into(),
            profile_name: minimum_profile.as_str(),
            minimum_profile,
            severity,
            role,
            component,
            platform,
            target,
            event_id: None,
            event_name: None,
            category: None,
            outcome: None,
            sid: None,
            user: None,
            host: None,
            peer_addr: None,
            health_state: None,
            message,
            fields: StructuredFields::default(),
        })
    }

    /// Attaches a stable lifecycle definition.
    #[must_use]
    pub fn with_event(mut self, kind: LifecycleEventKind) -> Self {
        let definition = kind.definition();
        self.profile_level = definition.minimum_profile.into();
        self.profile_name = definition.minimum_profile.as_str();
        self.minimum_profile = definition.minimum_profile;
        self.severity = definition.severity.into();
        self.event_id = Some(kind.id());
        self.event_name = Some(definition.name);
        self.category = Some(definition.category.as_str());
        self.outcome = Some(definition.outcome.as_str());
        self
    }

    /// Attaches an optional session correlation identifier.
    #[must_use]
    pub fn with_sid(mut self, sid: CorrelationId) -> Self {
        self.sid = Some(sid);
        self
    }

    /// Attaches schema-approved local identity fields.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, oversized, or control-bearing values.
    pub fn with_identity(
        mut self,
        user: Option<impl Into<String>>,
        host: Option<impl Into<String>>,
        peer_addr: Option<impl Into<String>>,
    ) -> Result<Self, SchemaValidationError> {
        self.user = validate_optional_identity(user)?;
        self.host = validate_optional_identity(host)?;
        self.peer_addr = validate_optional_identity(peer_addr)?;
        Ok(self)
    }

    /// Attaches a health state.
    #[must_use]
    pub fn with_health_state(mut self, health_state: HealthState) -> Self {
        self.health_state = Some(health_state);
        self
    }

    /// Replaces the sorted, validated structured fields.
    #[must_use]
    pub fn with_fields(mut self, fields: StructuredFields) -> Self {
        self.fields = fields;
        self
    }

    /// Returns the record's minimum operational profile.
    #[must_use]
    pub const fn minimum_profile(&self) -> OperationalProfile {
        self.minimum_profile
    }

    /// Returns the true event severity.
    #[must_use]
    pub const fn severity(&self) -> EventSeverity {
        self.severity
    }

    /// Serializes one canonical JSON record and trailing newline.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the in-memory record cannot be encoded.
    pub fn to_json_line(&self) -> Result<String, serde_json::Error> {
        let mut line = serde_json::to_string(self)?;
        line.push('\n');
        Ok(line)
    }
}

/// Canonical record validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaValidationError {
    /// Timestamp is not `YYYY-MM-DDTHH:MM:SS.ffffffZ`.
    InvalidTimestamp,
    /// Component is not bounded lowercase snake case.
    InvalidComponent,
    /// Target is not a bounded canonical `arcen::` target.
    InvalidTarget,
    /// Message is empty, oversized, or contains controls.
    InvalidMessage,
    /// Identity value is empty, oversized, or contains controls.
    InvalidIdentity,
}

impl Display for SchemaValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidTimestamp => "timestamp is not canonical UTC microsecond form",
            Self::InvalidComponent => "component is not canonical lowercase snake case",
            Self::InvalidTarget => "target is not a canonical arcen target",
            Self::InvalidMessage => "message is empty, oversized, or contains controls",
            Self::InvalidIdentity => "identity is empty, oversized, or contains controls",
        })
    }
}

impl Error for SchemaValidationError {}

fn validate_optional_identity(
    value: Option<impl Into<String>>,
) -> Result<Option<String>, SchemaValidationError> {
    value
        .map(Into::into)
        .map(|value| {
            validate_text(&value, MAX_IDENTITY_BYTES)
                .map(|()| value)
                .map_err(|()| SchemaValidationError::InvalidIdentity)
        })
        .transpose()
}

fn validate_text(value: &str, max_bytes: usize) -> Result<(), ()> {
    if value.is_empty() || value.len() > max_bytes || value.chars().any(char::is_control) {
        Err(())
    } else {
        Ok(())
    }
}

fn is_snake_name(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && !value.starts_with('_')
        && !value.ends_with('_')
}

fn is_canonical_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() == 27
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes[10] == b'T'
        && bytes[13] == b':'
        && bytes[16] == b':'
        && bytes[19] == b'.'
        && bytes[26] == b'Z'
        && bytes.iter().enumerate().all(|(index, byte)| {
            matches!(index, 4 | 7 | 10 | 13 | 16 | 19 | 26) || byte.is_ascii_digit()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FieldValue, ValidatedLifecycleEvent, names};

    fn fixture_record() -> CanonicalRecord {
        let mut fields = StructuredFields::default();
        fields
            .insert(
                names::field::AUTH_METHOD,
                FieldValue::String("password".to_owned()),
            )
            .expect("valid field");
        fields
            .insert(
                names::field::IDENTITY_BINDING,
                FieldValue::String("platform_account".to_owned()),
            )
            .expect("valid field");
        let lifecycle = ValidatedLifecycleEvent::new(
            LifecycleEventKind::SessionAuthOk,
            CorrelationId::new("canonical-session-correlation-id").expect("valid correlation ID"),
            fields,
        )
        .expect("fixture fields match SESSION_AUTH_OK");

        CanonicalRecord::new(
            "2026-07-24T16:00:00.000000Z",
            42,
            OperationalProfile::Critical,
            EventSeverity::Info,
            TelemetryRole::Host,
            TelemetryComponent::new(names::component::PIER).expect("valid component"),
            TelemetryPlatform::Windows,
            TelemetryTarget::new(names::target::SESSION).expect("valid target"),
            "session authentication succeeded",
        )
        .expect("valid record")
        .with_event(lifecycle.kind())
        .with_sid(lifecycle.correlation_id().clone())
        .with_identity(
            Some(r"DOMAIN\artist"),
            Some("pier-01"),
            Some("192.0.2.10:54000"),
        )
        .expect("valid identities")
        .with_health_state(HealthState::Ok)
        .with_fields(lifecycle.fields().clone())
    }

    #[test]
    fn canonical_json_matches_frozen_exact_bytes() {
        assert_eq!(
            fixture_record().to_json_line().expect("serializable"),
            include_str!("../tests/fixtures/canonical-record-v1.jsonl")
        );
    }

    #[test]
    fn schema_rejects_noncanonical_names_and_bounds() {
        assert_eq!(
            TelemetryComponent::new("Pier Service"),
            Err(SchemaValidationError::InvalidComponent)
        );
        assert_eq!(
            TelemetryTarget::new("other::session"),
            Err(SchemaValidationError::InvalidTarget)
        );
        assert_eq!(
            CanonicalRecord::new(
                "2026-07-24T16:00:00Z",
                1,
                OperationalProfile::Debug,
                EventSeverity::Debug,
                TelemetryRole::Client,
                TelemetryComponent::new("deck").expect("valid component"),
                TelemetryPlatform::Macos,
                TelemetryTarget::new("arcen::session").expect("valid target"),
                "diagnostic",
            ),
            Err(SchemaValidationError::InvalidTimestamp)
        );
        assert!(
            fixture_record()
                .with_identity(
                    Some("x".repeat(MAX_IDENTITY_BYTES + 1)),
                    None::<String>,
                    None::<String>,
                )
                .is_err()
        );
    }

    #[test]
    fn ad_hoc_diagnostics_omit_monitoring_identity() {
        let record = CanonicalRecord::new(
            "2026-07-24T16:00:00.000000Z",
            7,
            OperationalProfile::Debug,
            EventSeverity::Debug,
            TelemetryRole::Client,
            TelemetryComponent::new("deck").expect("valid component"),
            TelemetryPlatform::Macos,
            TelemetryTarget::new("arcen::media").expect("valid target"),
            "bounded diagnostic",
        )
        .expect("valid record");
        let line = record.to_json_line().expect("serializable");
        assert!(!line.contains(r#""event_id""#));
        assert!(!line.contains(r#""event_name""#));
    }

    #[test]
    fn stable_event_definition_controls_profile_and_severity() {
        let record = CanonicalRecord::new(
            "2026-07-24T16:00:00.000000Z",
            8,
            OperationalProfile::Debug,
            EventSeverity::Error,
            TelemetryRole::Host,
            TelemetryComponent::new("pier").expect("valid component"),
            TelemetryPlatform::Linux,
            TelemetryTarget::new("arcen::session").expect("valid target"),
            "session authentication succeeded",
        )
        .expect("valid record")
        .with_event(LifecycleEventKind::SessionAuthOk);
        assert_eq!(record.minimum_profile(), OperationalProfile::Critical);
        assert_eq!(record.severity(), EventSeverity::Info);
    }
}
