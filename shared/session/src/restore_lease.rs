//! Durable restore-lease state for client-environment redirection.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest, Sha256};
use std::error::Error;
use std::fmt::{Display, Formatter};

/// Maximum UTF-8 byte length of an IANA time-zone identifier.
pub const MAX_IANA_TIMEZONE_BYTES: usize = 128;
/// Maximum UTF-8 byte length of a restore-lease owner identifier.
pub const MAX_LEASE_OWNER_BYTES: usize = 128;
/// Maximum state snapshot accepted for fingerprinting.
pub const MAX_STATE_FINGERPRINT_BYTES: usize = 64 * 1024;

/// A validated IANA time-zone identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct IanaTimeZone(String);

impl IanaTimeZone {
    /// Parses and validates an IANA time-zone identifier.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, oversized, absolute, malformed, or
    /// unsupported identifiers.
    pub fn parse(value: impl Into<String>) -> Result<Self, IanaTimeZoneError> {
        Self::new(value)
    }

    /// Creates a validated IANA time-zone identifier.
    ///
    /// # Errors
    ///
    /// Returns an error for empty, oversized, absolute, malformed, or
    /// unsupported identifiers.
    pub fn new(value: impl Into<String>) -> Result<Self, IanaTimeZoneError> {
        let value = value.into();
        validate_iana_timezone(&value)?;
        Ok(Self(value))
    }

    /// Returns the validated identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for IanaTimeZone {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

fn validate_iana_timezone(value: &str) -> Result<(), IanaTimeZoneError> {
    if value.is_empty() {
        return Err(IanaTimeZoneError::Empty);
    }
    if value.len() > MAX_IANA_TIMEZONE_BYTES {
        return Err(IanaTimeZoneError::TooLong);
    }
    if value.starts_with('/') {
        return Err(IanaTimeZoneError::Absolute);
    }
    for segment in value.split('/') {
        if segment.is_empty() {
            return Err(IanaTimeZoneError::EmptySegment);
        }
        if segment == "." || segment == ".." {
            return Err(IanaTimeZoneError::DotSegment);
        }
        if !segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+'))
        {
            return Err(IanaTimeZoneError::UnsupportedCharacter);
        }
    }
    Ok(())
}

/// IANA time-zone validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IanaTimeZoneError {
    /// The identifier was empty.
    Empty,
    /// The identifier exceeded [`MAX_IANA_TIMEZONE_BYTES`].
    TooLong,
    /// The identifier was an absolute path.
    Absolute,
    /// The identifier contained an empty path segment.
    EmptySegment,
    /// The identifier contained `.` or `..`.
    DotSegment,
    /// The identifier contained a character outside the supported IANA subset.
    UnsupportedCharacter,
}

impl Display for IanaTimeZoneError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "IANA time-zone identifier is empty",
            Self::TooLong => "IANA time-zone identifier exceeds 128 bytes",
            Self::Absolute => "IANA time-zone identifier must not be absolute",
            Self::EmptySegment => "IANA time-zone identifier contains an empty segment",
            Self::DotSegment => "IANA time-zone identifier contains a dot segment",
            Self::UnsupportedCharacter => {
                "IANA time-zone identifier contains an unsupported character"
            }
        })
    }
}

impl Error for IanaTimeZoneError {}

/// A bounded identifier for the process or session owning a restore lease.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct LeaseOwnerId(String);

impl LeaseOwnerId {
    /// Parses a bounded, non-empty owner identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is empty or exceeds
    /// [`MAX_LEASE_OWNER_BYTES`].
    pub fn parse(value: impl Into<String>) -> Result<Self, LeaseOwnerIdError> {
        Self::new(value)
    }

    /// Creates a bounded, non-empty owner identifier.
    ///
    /// # Errors
    ///
    /// Returns an error when the identifier is empty or exceeds
    /// [`MAX_LEASE_OWNER_BYTES`].
    pub fn new(value: impl Into<String>) -> Result<Self, LeaseOwnerIdError> {
        let value = value.into();
        if value.is_empty() {
            return Err(LeaseOwnerIdError::Empty);
        }
        if value.len() > MAX_LEASE_OWNER_BYTES {
            return Err(LeaseOwnerIdError::TooLong);
        }
        Ok(Self(value))
    }

    /// Returns the owner identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for LeaseOwnerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

/// Restore-lease owner validation failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LeaseOwnerIdError {
    /// The owner identifier was empty.
    Empty,
    /// The owner identifier exceeded [`MAX_LEASE_OWNER_BYTES`].
    TooLong,
}

impl Display for LeaseOwnerIdError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Empty => "restore-lease owner identifier is empty",
            Self::TooLong => "restore-lease owner identifier exceeds 128 bytes",
        })
    }
}

impl Error for LeaseOwnerIdError {}

/// A stable SHA-256 fingerprint of a bounded resource state snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateFingerprint([u8; 32]);

impl StateFingerprint {
    /// Hashes a bounded state snapshot using SHA-256.
    ///
    /// # Errors
    ///
    /// Returns an error when `state` exceeds [`MAX_STATE_FINGERPRINT_BYTES`].
    pub fn new(state: &[u8]) -> Result<Self, StateFingerprintError> {
        Self::from_bytes(state)
    }

    /// Hashes a bounded state snapshot using SHA-256.
    ///
    /// # Errors
    ///
    /// Returns an error when `state` exceeds [`MAX_STATE_FINGERPRINT_BYTES`].
    pub fn from_bytes(state: &[u8]) -> Result<Self, StateFingerprintError> {
        if state.len() > MAX_STATE_FINGERPRINT_BYTES {
            return Err(StateFingerprintError);
        }
        Ok(Self(Sha256::digest(state).into()))
    }

    /// Returns the raw SHA-256 digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

/// State snapshot exceeded the fingerprint input bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateFingerprintError;

impl Display for StateFingerprintError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "state snapshot exceeds {MAX_STATE_FINGERPRINT_BYTES} bytes"
        )
    }
}

impl Error for StateFingerprintError {}

/// Client or host resource protected by a restore lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreResource {
    /// Process time zone.
    Timezone,
    /// Display configuration.
    Display,
    /// Default audio device.
    AudioDefaultDevice,
    /// Deskside input ownership.
    DesksideInput,
    /// Deskside display ownership.
    DesksideDisplay,
}

/// Durable restore-lease phase.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestorePhase {
    /// Journal persisted before mutation.
    Armed,
    /// Target mutation may be in progress.
    Applying,
    /// Target mutation completed.
    Applied,
    /// Original state restoration may be in progress.
    Restoring,
    /// Original state restoration completed.
    Restored,
    /// Ownership or state diverged and requires operator adjudication.
    Conflicted,
}

/// Event applied to a restore lease.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RestoreEvent {
    /// Begin applying the target state.
    BeginApply,
    /// Record successful target-state application.
    ApplySucceeded,
    /// Begin restoring the original state.
    BeginRestore,
    /// Record successful original-state restoration.
    RestoreSucceeded,
    /// Record that another owner or unknown state was observed.
    OwnershipConflict,
}

/// Durable lease for restoring a mutated resource after retries or crashes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreLease {
    resource: RestoreResource,
    owner: LeaseOwnerId,
    original: StateFingerprint,
    target: StateFingerprint,
    phase: RestorePhase,
}

impl RestoreLease {
    /// Arms a lease before any resource mutation.
    #[must_use]
    pub const fn arm(
        resource: RestoreResource,
        owner: LeaseOwnerId,
        original: StateFingerprint,
        target: StateFingerprint,
    ) -> Self {
        Self {
            resource,
            owner,
            original,
            target,
            phase: RestorePhase::Armed,
        }
    }

    /// Returns the protected resource.
    #[must_use]
    pub const fn resource(&self) -> RestoreResource {
        self.resource
    }

    /// Returns the lease owner.
    #[must_use]
    pub const fn owner(&self) -> &LeaseOwnerId {
        &self.owner
    }

    /// Returns the original-state fingerprint.
    #[must_use]
    pub const fn original(&self) -> StateFingerprint {
        self.original
    }

    /// Returns the target-state fingerprint.
    #[must_use]
    pub const fn target(&self) -> StateFingerprint {
        self.target
    }

    /// Returns the durable phase.
    #[must_use]
    pub const fn phase(&self) -> RestorePhase {
        self.phase
    }

    /// Applies an idempotent restore transition.
    ///
    /// # Errors
    ///
    /// Returns an explicit error when the event is invalid in the current phase.
    pub fn apply(&mut self, event: RestoreEvent) -> Result<RestorePhase, RestoreTransitionError> {
        let next = match (self.phase, event) {
            (RestorePhase::Armed | RestorePhase::Applying, RestoreEvent::BeginApply) => {
                RestorePhase::Applying
            }
            (RestorePhase::Applying | RestorePhase::Applied, RestoreEvent::ApplySucceeded) => {
                RestorePhase::Applied
            }
            (
                RestorePhase::Applying | RestorePhase::Applied | RestorePhase::Restoring,
                RestoreEvent::BeginRestore,
            ) => RestorePhase::Restoring,
            (RestorePhase::Restoring | RestorePhase::Restored, RestoreEvent::RestoreSucceeded) => {
                RestorePhase::Restored
            }
            (
                RestorePhase::Applying
                | RestorePhase::Applied
                | RestorePhase::Restoring
                | RestorePhase::Conflicted,
                RestoreEvent::OwnershipConflict,
            ) => RestorePhase::Conflicted,
            (phase, event) => return Err(RestoreTransitionError { phase, event }),
        };
        self.phase = next;
        Ok(next)
    }

    /// Determines crash-recovery work from the journal phase and current state.
    #[must_use]
    pub fn recovery_directive(&self, current: StateFingerprint) -> RecoveryDirective {
        match self.phase {
            RestorePhase::Armed => RecoveryDirective::RemoveUnmutatedJournal,
            RestorePhase::Restored => RecoveryDirective::RemoveRestoredJournal,
            RestorePhase::Conflicted => RecoveryDirective::HoldForOperator,
            RestorePhase::Applying | RestorePhase::Applied | RestorePhase::Restoring => {
                if current == self.original {
                    RecoveryDirective::RemoveRestoredJournal
                } else if current == self.target {
                    RecoveryDirective::RestoreOriginal
                } else {
                    RecoveryDirective::HoldForOperator
                }
            }
        }
    }

    /// Adjudicates recovery and durably marks unknown active state as conflicted.
    pub fn adjudicate_recovery(&mut self, current: StateFingerprint) -> RecoveryDirective {
        let directive = self.recovery_directive(current);
        if directive == RecoveryDirective::HoldForOperator
            && matches!(
                self.phase,
                RestorePhase::Applying | RestorePhase::Applied | RestorePhase::Restoring
            )
        {
            self.phase = RestorePhase::Conflicted;
        }
        directive
    }
}

/// Invalid restore-lease transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoreTransitionError {
    /// Phase that rejected the event.
    pub phase: RestorePhase,
    /// Rejected event.
    pub event: RestoreEvent,
}

impl Display for RestoreTransitionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "restore event {:?} rejected in phase {:?}",
            self.event, self.phase
        )
    }
}

impl Error for RestoreTransitionError {}

/// Deterministic work selected during at-least-once journal recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecoveryDirective {
    /// No mutation began; remove the journal.
    RemoveUnmutatedJournal,
    /// Restore the original state and retain the journal until confirmed.
    RestoreOriginal,
    /// Restoration is already complete; remove the journal.
    RemoveRestoredJournal,
    /// Preserve state and journal for operator adjudication.
    HoldForOperator,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(value: &str) -> StateFingerprint {
        StateFingerprint::from_bytes(value.as_bytes()).expect("bounded test input")
    }

    fn lease() -> RestoreLease {
        RestoreLease::arm(
            RestoreResource::Timezone,
            LeaseOwnerId::new("session-42").expect("owner"),
            fingerprint("Europe/Oslo"),
            fingerprint("America/Los_Angeles"),
        )
    }

    #[test]
    fn accepts_supported_iana_identifiers() {
        for identifier in [
            "Europe/Oslo",
            "America/Los_Angeles",
            "Asia/Kolkata",
            "Asia/Kathmandu",
            "UTC",
            "GMT",
            "CET",
            "Etc/GMT+5",
        ] {
            assert_eq!(
                IanaTimeZone::parse(identifier).expect("valid").as_str(),
                identifier
            );
        }
    }

    #[test]
    fn rejects_malformed_iana_identifiers() {
        let too_long = "a".repeat(MAX_IANA_TIMEZONE_BYTES + 1);
        for identifier in [
            "",
            "\0",
            "Europe Oslo",
            "Europe\\Oslo",
            "Europe:Oslo",
            "/Europe/Oslo",
            "Europe//Oslo",
            ".",
            "..",
            "Europe/.",
            "Europe/..",
            "Europe/Oslo?",
            too_long.as_str(),
        ] {
            assert!(
                IanaTimeZone::new(identifier).is_err(),
                "{identifier:?} must be rejected"
            );
        }
    }

    #[test]
    fn serde_preserves_bounded_string_invariants() {
        let timezone = IanaTimeZone::new("Europe/Oslo").expect("timezone");
        assert_eq!(
            serde_json::from_str::<IanaTimeZone>(
                &serde_json::to_string(&timezone).expect("serialize")
            )
            .expect("deserialize"),
            timezone
        );
        assert!(serde_json::from_str::<IanaTimeZone>(r#""Europe Oslo""#).is_err());
        assert!(serde_json::from_str::<LeaseOwnerId>(r#""""#).is_err());
    }

    #[test]
    fn lease_owner_is_nonempty_and_bounded() {
        assert!(LeaseOwnerId::new("").is_err());
        assert!(LeaseOwnerId::new("o".repeat(MAX_LEASE_OWNER_BYTES + 1)).is_err());
        assert_eq!(
            LeaseOwnerId::parse("deck-session").expect("owner").as_str(),
            "deck-session"
        );
    }

    #[test]
    fn fingerprints_are_stable_and_bounded() {
        let digest = StateFingerprint::from_bytes(b"abc").expect("fingerprint");
        assert_eq!(
            digest.as_bytes(),
            &[
                0xba, 0x78, 0x16, 0xbf, 0x8f, 0x01, 0xcf, 0xea, 0x41, 0x41, 0x40, 0xde, 0x5d, 0xae,
                0x22, 0x23, 0xb0, 0x03, 0x61, 0xa3, 0x96, 0x17, 0x7a, 0x9c, 0xb4, 0x10, 0xff, 0x61,
                0xf2, 0x00, 0x15, 0xad,
            ]
        );
        assert!(StateFingerprint::from_bytes(&vec![0; MAX_STATE_FINGERPRINT_BYTES + 1]).is_err());
    }

    #[test]
    fn full_transition_supports_at_least_once_retries() {
        let mut lease = lease();
        for (event, expected) in [
            (RestoreEvent::BeginApply, RestorePhase::Applying),
            (RestoreEvent::BeginApply, RestorePhase::Applying),
            (RestoreEvent::ApplySucceeded, RestorePhase::Applied),
            (RestoreEvent::ApplySucceeded, RestorePhase::Applied),
            (RestoreEvent::BeginRestore, RestorePhase::Restoring),
            (RestoreEvent::BeginRestore, RestorePhase::Restoring),
            (RestoreEvent::RestoreSucceeded, RestorePhase::Restored),
            (RestoreEvent::RestoreSucceeded, RestorePhase::Restored),
        ] {
            assert_eq!(lease.apply(event), Ok(expected));
        }
    }

    #[test]
    fn invalid_transitions_are_explicit_and_leave_phase_unchanged() {
        let mut lease = lease();
        assert_eq!(
            lease.apply(RestoreEvent::ApplySucceeded),
            Err(RestoreTransitionError {
                phase: RestorePhase::Armed,
                event: RestoreEvent::ApplySucceeded,
            })
        );
        assert_eq!(lease.phase(), RestorePhase::Armed);
        assert!(lease.apply(RestoreEvent::OwnershipConflict).is_err());
    }

    #[test]
    fn recovery_directives_cover_unmutated_target_original_and_restored() {
        let mut lease = lease();
        assert_eq!(
            lease.recovery_directive(fingerprint("unknown")),
            RecoveryDirective::RemoveUnmutatedJournal
        );
        lease.apply(RestoreEvent::BeginApply).expect("begin apply");
        assert_eq!(
            lease.recovery_directive(lease.target()),
            RecoveryDirective::RestoreOriginal
        );
        assert_eq!(
            lease.recovery_directive(lease.original()),
            RecoveryDirective::RemoveRestoredJournal
        );
        lease
            .apply(RestoreEvent::BeginRestore)
            .expect("begin restore");
        lease
            .apply(RestoreEvent::RestoreSucceeded)
            .expect("restore succeeds");
        assert_eq!(
            lease.recovery_directive(fingerprint("unknown")),
            RecoveryDirective::RemoveRestoredJournal
        );
    }

    #[test]
    fn unknown_active_state_is_deterministically_conflicted() {
        let mut lease = lease();
        lease.apply(RestoreEvent::BeginApply).expect("begin apply");
        assert_eq!(
            lease.adjudicate_recovery(fingerprint("third-party-state")),
            RecoveryDirective::HoldForOperator
        );
        assert_eq!(lease.phase(), RestorePhase::Conflicted);
        assert_eq!(
            lease.apply(RestoreEvent::OwnershipConflict),
            Ok(RestorePhase::Conflicted)
        );
        assert_eq!(
            lease.recovery_directive(lease.original()),
            RecoveryDirective::HoldForOperator
        );
    }
}
