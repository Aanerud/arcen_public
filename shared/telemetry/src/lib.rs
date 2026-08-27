//! Redaction-safe structured telemetry contracts.

#![forbid(unsafe_code)]
#![cfg_attr(test, allow(clippy::expect_used, clippy::unwrap_used))]

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use serde::Serialize;

mod health;
mod lifecycle;
mod log_policy;
pub mod names;
mod network;
mod schema;
mod support_bundle;

pub use health::{
    HealthAssessment, HealthCause, HealthSide, HealthState, HealthTracker, HealthTrackerError,
    HealthTransition, QosSample, QosTargetError, QosTargets, assess_health,
};
pub use lifecycle::{
    LIFECYCLE_EVENT_DEFINITIONS, LifecycleEventDefinition, LifecycleEventKind,
    LifecycleFieldRequirement, LifecycleFieldSpec, LifecycleFieldType, LifecycleSeverity,
    LifecycleValidationError, MAX_STRUCTURED_FIELDS, ValidatedLifecycleEvent,
    lifecycle_event_definition,
};
pub use log_policy::{
    DEFAULT_RETENTION_DAYS, DEFAULT_ROTATE_BYTES, LevelSpec, LogFileId, LogFileKind, LogFileRecord,
    LogMaintenancePlan, LogPolicyError, MAX_LOG_FILE_RECORDS, MAX_RETENTION_DAYS, MAX_ROTATE_BYTES,
    MIN_RETENTION_DAYS, OperationalProfile, RetentionPolicy, VerbosityTier, plan_log_maintenance,
};
pub use network::{
    InterfaceKind, MAX_NETWORK_IDENTITY_BYTES, MAX_NETWORK_MTU, MIN_NETWORK_MTU, NetworkScope,
    NetworkSnapshot, NetworkValidationError, classify_ip, classify_ip_literal,
};
pub use schema::{
    CANONICAL_SCHEMA_VERSION, CanonicalRecord, EventSeverity, MAX_IDENTITY_BYTES,
    MAX_MESSAGE_BYTES, SchemaValidationError, TelemetryComponent, TelemetryPlatform, TelemetryRole,
    TelemetryTarget,
};
pub use support_bundle::{
    BundleComponent, BundleEntry, BundleIdentityKind, BundleNotice, BundlePath, BundlePseudonymKey,
    BundlePseudonymizer, BundleSource, BundleTruncation, CanonicalJsonlTransformError,
    CanonicalJsonlTransformLimits, CanonicalJsonlTransformReport, MAX_BUNDLE_ENTRIES,
    MAX_BUNDLE_NOTICES, MAX_BUNDLE_PATH_BYTES, MAX_CANONICAL_JSON_LINE_BYTES,
    MAX_REDACTION_KEY_PATH_BYTES, MAX_REDACTION_RECORDS, NoticeCode, NoticeKind, REDACTED_VALUE,
    RedactionDecision, RedactionReason, RedactionRecord, SUPPORT_BUNDLE_SCHEMA_VERSION,
    Sha256Digest, SupportBundleContractError, SupportBundleManifest, SupportBundleManifestBuilder,
    SupportBundleRedactionPolicy, TruncationReason, redact_json_document, redact_json_document_at,
    transform_canonical_jsonl,
};

/// Maximum correlation identifier length.
pub const MAX_CORRELATION_ID_BYTES: usize = 128;
/// Maximum structured field key length.
pub const MAX_FIELD_KEY_BYTES: usize = 64;
/// Maximum structured string value length.
pub const MAX_FIELD_STRING_BYTES: usize = 512;

/// Validated correlation identifier shared across trust boundaries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct CorrelationId(String);

impl CorrelationId {
    /// Validates a non-empty, bounded identifier.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, oversized, or control-character values.
    pub fn new(value: impl Into<String>) -> Result<Self, TelemetryContractError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > MAX_CORRELATION_ID_BYTES
            || value.chars().any(char::is_control)
        {
            return Err(TelemetryContractError::InvalidCorrelationId);
        }
        Ok(Self(value))
    }

    /// Parses a canonical, lowercase, hyphenated UUID correlation identifier.
    ///
    /// # Errors
    ///
    /// Returns an error unless the value is exactly 36 ASCII bytes with UUID
    /// hyphens and lowercase hexadecimal digits.
    pub fn parse_uuid(value: impl Into<String>) -> Result<Self, TelemetryContractError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.len() != 36
            || bytes.iter().enumerate().any(|(index, byte)| match index {
                8 | 13 | 18 | 23 => *byte != b'-',
                _ => !byte.is_ascii_digit() && !(b'a'..=b'f').contains(byte),
            })
        {
            return Err(TelemetryContractError::InvalidCorrelationId);
        }
        Ok(Self(value))
    }

    /// Formats random bytes as a canonical RFC 4122 UUID v4 identifier.
    #[must_use]
    pub fn from_uuid_v4_bytes(mut bytes: [u8; 16]) -> Self {
        const HEX: &[u8; 16] = b"0123456789abcdef";

        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;

        let mut value = String::with_capacity(36);
        for (index, byte) in bytes.into_iter().enumerate() {
            if matches!(index, 4 | 6 | 8 | 10) {
                value.push('-');
            }
            value.push(char::from(HEX[usize::from(byte >> 4)]));
            value.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
        Self(value)
    }

    /// Returns the identifier value.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for CorrelationId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl AsRef<str> for CorrelationId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// Stable lifecycle event category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum LifecycleCategory {
    /// Transport establishment and binding.
    Connection,
    /// Online OIDC identity validation.
    OnlineIdentity,
    /// Commercial entitlement lease lifecycle.
    Entitlement,
    /// OS machine authentication.
    MachineAuthentication,
    /// Protocol and capability negotiation.
    Negotiation,
    /// Media/input streaming lifecycle.
    Streaming,
    /// Reconnect and path migration lifecycle.
    Reconnect,
    /// Drain, release, and session-state cleanup.
    Cleanup,
    /// Operational health.
    Health,
    /// HID and peripheral state.
    Peripheral,
    /// Network path state.
    Network,
    /// Telemetry runtime state.
    Telemetry,
    /// OS permission state.
    Permission,
}

impl LifecycleCategory {
    /// Returns the stable telemetry category name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::OnlineIdentity => "online_identity",
            Self::Entitlement => "entitlement",
            Self::MachineAuthentication => "machine_authentication",
            Self::Negotiation => "negotiation",
            Self::Streaming => "streaming",
            Self::Reconnect => "reconnect",
            Self::Cleanup => "cleanup",
            Self::Health => "health",
            Self::Peripheral => "peripheral",
            Self::Network => "network",
            Self::Telemetry => "telemetry",
            Self::Permission => "permission",
        }
    }
}

/// Stable event outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    /// Lifecycle action started.
    Started,
    /// Lifecycle action completed.
    Succeeded,
    /// Lifecycle action failed.
    Failed,
}

impl EventOutcome {
    /// Returns the stable telemetry outcome name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Started => "started",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
        }
    }
}

/// Redaction-safe field value types.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum FieldValue {
    /// Boolean.
    Boolean(bool),
    /// Signed integer.
    Integer(i64),
    /// Bounded non-sensitive string.
    String(String),
}

/// Structured fields that reject common secret-bearing key names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct StructuredFields(BTreeMap<String, FieldValue>);

impl StructuredFields {
    /// Inserts a validated field.
    ///
    /// # Errors
    ///
    /// Rejects invalid keys, likely secret-bearing keys, and oversized strings.
    pub fn insert(
        &mut self,
        key: impl Into<String>,
        value: FieldValue,
    ) -> Result<(), TelemetryContractError> {
        let key = key.into();
        if key.is_empty()
            || key.len() > MAX_FIELD_KEY_BYTES
            || !key
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        {
            return Err(TelemetryContractError::InvalidFieldKey);
        }
        let normalized = key.as_str();
        if ["sid", "user", "host", "peer_addr"].contains(&normalized) {
            return Err(TelemetryContractError::ReservedFieldKey);
        }
        if [
            "password",
            "secret",
            "token",
            "credential",
            "authorization",
            "cookie",
            "passphrase",
            "private_key",
            "key_path",
            "session_key",
        ]
        .iter()
        .any(|forbidden| normalized.contains(forbidden))
        {
            return Err(TelemetryContractError::SensitiveFieldKey);
        }
        if let FieldValue::String(text) = &value {
            if text.len() > MAX_FIELD_STRING_BYTES {
                return Err(TelemetryContractError::FieldValueTooLarge);
            }
            if text.chars().any(char::is_control) {
                return Err(TelemetryContractError::FieldValueContainsControl);
            }
        }
        if !self.0.contains_key(&key) && self.0.len() >= MAX_STRUCTURED_FIELDS {
            return Err(TelemetryContractError::TooManyFields);
        }
        self.0.insert(key, value);
        Ok(())
    }

    /// Returns an immutable field map.
    #[must_use]
    pub const fn as_map(&self) -> &BTreeMap<String, FieldValue> {
        &self.0
    }

    /// Returns whether no fields are present.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the number of fields.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }
}

/// Correlated structured lifecycle event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleEvent {
    /// Cross-boundary correlation identifier.
    pub correlation_id: CorrelationId,
    /// Stable category.
    pub category: LifecycleCategory,
    /// Stable outcome.
    pub outcome: EventOutcome,
    /// Redaction-safe structured fields.
    pub fields: StructuredFields,
}

/// Telemetry contract validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelemetryContractError {
    /// Correlation identifier is invalid.
    InvalidCorrelationId,
    /// Field key is invalid.
    InvalidFieldKey,
    /// Field key belongs to the canonical top-level record context.
    ReservedFieldKey,
    /// Field key appears to carry sensitive data.
    SensitiveFieldKey,
    /// String field exceeds its cap.
    FieldValueTooLarge,
    /// String field contains a control character.
    FieldValueContainsControl,
    /// Structured field count exceeds its cap.
    TooManyFields,
}

impl Display for TelemetryContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidCorrelationId => "correlation identifier is invalid",
            Self::InvalidFieldKey => "structured field key is invalid",
            Self::ReservedFieldKey => {
                "structured field key is reserved for canonical record context"
            }
            Self::SensitiveFieldKey => "structured field key may contain sensitive data",
            Self::FieldValueTooLarge => "structured field value exceeds its cap",
            Self::FieldValueContainsControl => {
                "structured field value contains a control character"
            }
            Self::TooManyFields => "structured field count exceeds its cap",
        })
    }
}

impl Error for TelemetryContractError {}

impl From<VerbosityTier> for u8 {
    fn from(value: VerbosityTier) -> Self {
        value as Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn structured_fields_reject_secret_bearing_keys() {
        let mut fields = StructuredFields::default();
        for key in [
            "access_token",
            "session_cookie",
            "user_passphrase",
            "tls_private_key",
            "certificate_key_path",
            "media_session_key",
        ] {
            assert_eq!(
                fields.insert(key, FieldValue::String("value".to_owned())),
                Err(TelemetryContractError::SensitiveFieldKey)
            );
        }
        for key in ["sid", "user", "host", "peer_addr"] {
            assert_eq!(
                fields.insert(key, FieldValue::String("value".to_owned())),
                Err(TelemetryContractError::ReservedFieldKey)
            );
        }
        assert_eq!(fields.insert("retry_count", FieldValue::Integer(2)), Ok(()));
    }

    #[test]
    fn structured_fields_reject_controls_and_unbounded_counts() {
        let mut fields = StructuredFields::default();
        assert_eq!(
            fields.insert("reason_class", FieldValue::String("bad\nvalue".to_owned())),
            Err(TelemetryContractError::FieldValueContainsControl)
        );
        for index in 0..MAX_STRUCTURED_FIELDS {
            fields
                .insert(
                    format!("field_{index}"),
                    FieldValue::Integer(i64::try_from(index).expect("small bounded index")),
                )
                .expect("bounded field");
        }
        assert_eq!(
            fields.insert("overflow", FieldValue::Boolean(true)),
            Err(TelemetryContractError::TooManyFields)
        );
        assert_eq!(fields.insert("field_0", FieldValue::Boolean(true)), Ok(()));
    }

    #[test]
    fn lifecycle_category_names_are_stable() {
        assert_eq!(
            LifecycleCategory::OnlineIdentity.as_str(),
            "online_identity"
        );
        assert_eq!(
            LifecycleCategory::MachineAuthentication.as_str(),
            "machine_authentication"
        );
    }

    #[test]
    fn canonical_uuid_correlation_ids_round_trip() {
        let value = "01234567-89ab-cdef-8123-456789abcdef";
        let id = CorrelationId::parse_uuid(value).expect("canonical UUID");
        assert_eq!(id.as_str(), value);
        assert_eq!(id.to_string(), value);
        assert_eq!(id.as_ref(), value);
    }

    #[test]
    fn uuid_parser_rejects_noncanonical_or_unsafe_text() {
        for invalid in [
            "",
            "0123456789abcdef8123456789abcdef",
            "01234567-89AB-cdef-8123-456789abcdef",
            "01234567-89ab-cdef-8123-456789abcdeg",
            "01234567-89ab-cdef-8123-456789abcdef\n",
        ] {
            assert_eq!(
                CorrelationId::parse_uuid(invalid),
                Err(TelemetryContractError::InvalidCorrelationId)
            );
        }
    }

    #[test]
    fn uuid_v4_bytes_set_version_and_variant_bits() {
        let id = CorrelationId::from_uuid_v4_bytes([0xff; 16]);
        assert_eq!(id.as_str(), "ffffffff-ffff-4fff-bfff-ffffffffffff");
        assert_eq!(CorrelationId::parse_uuid(id.to_string()), Ok(id));
    }

    #[test]
    fn general_correlation_ids_keep_existing_validation() {
        assert_eq!(
            CorrelationId::new("first-login-42").expect("general correlation id"),
            CorrelationId::new("first-login-42").expect("general correlation id")
        );
        assert_eq!(
            CorrelationId::new("bad\nvalue"),
            Err(TelemetryContractError::InvalidCorrelationId)
        );
    }
}
