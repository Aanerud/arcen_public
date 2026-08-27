//! Pure verbosity and bounded log-maintenance policy.

use std::error::Error;
use std::fmt::{Display, Formatter};

/// Minimum supported retention period.
pub const MIN_RETENTION_DAYS: u16 = 7;
/// Maximum supported retention period.
pub const MAX_RETENTION_DAYS: u16 = 100;
/// Default retention period.
pub const DEFAULT_RETENTION_DAYS: u16 = 30;
/// Default active-log rotation threshold.
pub const DEFAULT_ROTATE_BYTES: u64 = 32 * 1024 * 1024;
/// Maximum number of records accepted by one maintenance plan.
pub const MAX_LOG_FILE_RECORDS: usize = 4096;
/// Maximum accepted rotation threshold.
pub const MAX_ROTATE_BYTES: u64 = 4 * 1024 * 1024 * 1024 * 1024;

const SECONDS_PER_DAY: u64 = 24 * 60 * 60;

/// Cumulative operator-selected telemetry profile.
///
/// The numeric representation is a stable external contract. Event severity is
/// independent: a mandatory [`OperationalProfile::Critical`] record may have
/// informational severity.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum OperationalProfile {
    /// Mandatory operational records only.
    #[default]
    Critical = 0,
    /// Critical records plus warnings and errors.
    Error = 1,
    /// Error records plus normal lifecycle and aggregate diagnostics.
    Info = 2,
    /// Info records plus bounded diagnostic detail.
    Debug = 3,
}

impl OperationalProfile {
    /// Returns the stable lowercase profile name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Critical => "critical",
            Self::Error => "error",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    /// Returns whether an effective profile includes a record at `minimum`.
    #[must_use]
    pub const fn includes(self, minimum: Self) -> bool {
        self as u8 >= minimum as u8
    }

    /// Returns the ordinary diagnostic tracing level for this profile.
    #[must_use]
    pub const fn diagnostic_level(self) -> &'static str {
        match self {
            Self::Critical => "error",
            Self::Error => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }
}

impl TryFrom<u8> for OperationalProfile {
    type Error = LogPolicyError;

    fn try_from(value: u8) -> Result<Self, LogPolicyError> {
        match value {
            0 => Ok(Self::Critical),
            1 => Ok(Self::Error),
            2 => Ok(Self::Info),
            3 => Ok(Self::Debug),
            _ => Err(LogPolicyError::InvalidOperationalProfile(value)),
        }
    }
}

impl From<OperationalProfile> for u8 {
    fn from(value: OperationalProfile) -> Self {
        value as Self
    }
}

/// Compatibility vocabulary for the pre-standard verbosity contract.
///
/// New configuration must use [`OperationalProfile`]. This type remains for
/// one migration release so existing process-control and platform code keeps
/// compiling without silently assigning the old numbers new meanings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VerbosityTier {
    /// Warnings and errors only.
    Quiet = 0,
    /// Normal informational diagnostics.
    Normal = 1,
    /// Debug diagnostics.
    Debug = 2,
    /// Full trace diagnostics.
    Trace = 3,
}

impl VerbosityTier {
    /// Returns the stable tracing level name.
    #[must_use]
    pub const fn level_name(self) -> &'static str {
        match self {
            Self::Quiet => "warn",
            Self::Normal => "info",
            Self::Debug => "debug",
            Self::Trace => "trace",
        }
    }
}

impl TryFrom<u8> for VerbosityTier {
    type Error = LogPolicyError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Quiet),
            1 => Ok(Self::Normal),
            2 => Ok(Self::Debug),
            3 => Ok(Self::Trace),
            _ => Err(LogPolicyError::InvalidVerbosityTier(value)),
        }
    }
}

impl From<VerbosityTier> for OperationalProfile {
    fn from(value: VerbosityTier) -> Self {
        match value {
            VerbosityTier::Quiet => Self::Error,
            VerbosityTier::Normal => Self::Info,
            VerbosityTier::Debug | VerbosityTier::Trace => Self::Debug,
        }
    }
}

/// EnvFilter-compatible policy for ordinary diagnostic tracing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelSpec {
    profile: OperationalProfile,
}

impl LevelSpec {
    /// Creates a level specification from a profile or legacy verbosity tier.
    #[must_use]
    pub fn new(profile: impl Into<OperationalProfile>) -> Self {
        Self {
            profile: profile.into(),
        }
    }

    /// Creates a level specification in a const context.
    #[must_use]
    pub const fn from_profile(profile: OperationalProfile) -> Self {
        Self { profile }
    }

    /// Returns the selected operational profile.
    #[must_use]
    pub const fn profile(self) -> OperationalProfile {
        self.profile
    }

    /// Returns the common `EnvFilter` directive.
    #[must_use]
    pub const fn directive(self) -> &'static str {
        match self.profile {
            OperationalProfile::Critical => "warn,arcen=error",
            OperationalProfile::Error => "warn,arcen=warn",
            OperationalProfile::Info => "warn,arcen=info",
            OperationalProfile::Debug => "warn,arcen=debug",
        }
    }
}

impl From<OperationalProfile> for LevelSpec {
    fn from(profile: OperationalProfile) -> Self {
        Self::from_profile(profile)
    }
}

impl From<VerbosityTier> for LevelSpec {
    fn from(tier: VerbosityTier) -> Self {
        Self::from_profile(tier.into())
    }
}

/// Validated rotation and retention limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetentionPolicy {
    rotate_bytes: u64,
    retention_days: u16,
}

impl RetentionPolicy {
    /// Creates a policy and normalizes retention to the supported range.
    ///
    /// # Errors
    ///
    /// Returns an error when the rotation threshold is zero or unreasonably large.
    pub fn new(rotate_bytes: u64, retention_days: u16) -> Result<Self, LogPolicyError> {
        if rotate_bytes == 0 || rotate_bytes > MAX_ROTATE_BYTES {
            return Err(LogPolicyError::InvalidRotateBytes(rotate_bytes));
        }
        Ok(Self {
            rotate_bytes,
            retention_days: retention_days.clamp(MIN_RETENTION_DAYS, MAX_RETENTION_DAYS),
        })
    }

    /// Returns the active-log rotation threshold.
    #[must_use]
    pub const fn rotate_bytes(self) -> u64 {
        self.rotate_bytes
    }

    /// Returns the normalized retention period.
    #[must_use]
    pub const fn retention_days(self) -> u16 {
        self.retention_days
    }
}

impl Default for RetentionPolicy {
    fn default() -> Self {
        Self {
            rotate_bytes: DEFAULT_ROTATE_BYTES,
            retention_days: DEFAULT_RETENTION_DAYS,
        }
    }
}

/// Host-owned identifier for a discovered log file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogFileId(u32);

impl LogFileId {
    /// Creates an identifier.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// Returns the host-provided value.
    #[must_use]
    pub const fn value(self) -> u32 {
        self.0
    }
}

/// Lifecycle role of a discovered file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFileKind {
    /// The active file that may be renamed and reopened.
    Active,
    /// A rotated archive.
    Archive,
    /// A closed per-session log.
    Session,
}

/// Clockless metadata supplied by a host adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogFileRecord {
    /// Host-owned identifier.
    pub id: LogFileId,
    /// Lifecycle role.
    pub kind: LogFileKind,
    /// Current file size.
    pub size_bytes: u64,
    /// Whole age from the host's current-time calculation.
    pub age_seconds: u64,
}

/// Deterministic host actions for one maintenance pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogMaintenancePlan {
    /// Active files to archive, sorted by identifier.
    pub archive: Vec<LogFileId>,
    /// Archive or session files to delete, sorted by identifier.
    pub delete: Vec<LogFileId>,
}

/// Plans log maintenance without opening files or consulting a clock.
///
/// # Errors
///
/// Returns an error when `records` exceeds [`MAX_LOG_FILE_RECORDS`].
pub fn plan_log_maintenance(
    policy: &RetentionPolicy,
    records: &[LogFileRecord],
) -> Result<LogMaintenancePlan, LogPolicyError> {
    if records.len() > MAX_LOG_FILE_RECORDS {
        return Err(LogPolicyError::TooManyLogFiles(records.len()));
    }

    let retention_seconds = u64::from(policy.retention_days) * SECONDS_PER_DAY;
    let mut archive = Vec::new();
    let mut delete = Vec::new();

    for record in records {
        match record.kind {
            LogFileKind::Active
                if record.size_bytes != 0
                    && (record.size_bytes >= policy.rotate_bytes
                        || record.age_seconds >= SECONDS_PER_DAY) =>
            {
                archive.push(record.id);
            }
            LogFileKind::Archive | LogFileKind::Session
                if record.age_seconds >= retention_seconds =>
            {
                delete.push(record.id);
            }
            LogFileKind::Active | LogFileKind::Archive | LogFileKind::Session => {}
        }
    }

    archive.sort_unstable();
    archive.dedup();
    delete.sort_unstable();
    delete.dedup();

    Ok(LogMaintenancePlan { archive, delete })
}

/// Invalid shared log policy input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogPolicyError {
    /// Numeric operational profile is outside `0..=3`.
    InvalidOperationalProfile(u8),
    /// Numeric verbosity is outside `0..=3`.
    InvalidVerbosityTier(u8),
    /// Rotation threshold is zero or exceeds the supported cap.
    InvalidRotateBytes(u64),
    /// The host supplied an unbounded metadata set.
    TooManyLogFiles(usize),
}

impl Display for LogPolicyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOperationalProfile(value) => {
                write!(formatter, "operational profile {value} is outside 0..=3")
            }
            Self::InvalidVerbosityTier(value) => {
                write!(formatter, "verbosity tier {value} is outside 0..=3")
            }
            Self::InvalidRotateBytes(value) => {
                write!(formatter, "rotation threshold {value} bytes is unsupported")
            }
            Self::TooManyLogFiles(count) => {
                write!(
                    formatter,
                    "log metadata count {count} exceeds the bounded limit"
                )
            }
        }
    }
}

impl Error for LogPolicyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u32, kind: LogFileKind, size_bytes: u64, age_seconds: u64) -> LogFileRecord {
        LogFileRecord {
            id: LogFileId::new(id),
            kind,
            size_bytes,
            age_seconds,
        }
    }

    #[test]
    fn numeric_profiles_and_directives_are_stable() {
        let expected = [
            (
                0,
                OperationalProfile::Critical,
                "critical",
                "warn,arcen=error",
            ),
            (1, OperationalProfile::Error, "error", "warn,arcen=warn"),
            (2, OperationalProfile::Info, "info", "warn,arcen=info"),
            (3, OperationalProfile::Debug, "debug", "warn,arcen=debug"),
        ];
        for (value, profile, name, directive) in expected {
            assert_eq!(OperationalProfile::try_from(value), Ok(profile));
            assert_eq!(u8::from(profile), value);
            assert_eq!(profile.as_str(), name);
            assert_eq!(LevelSpec::new(profile).directive(), directive);
            assert!(profile.includes(OperationalProfile::Critical));
        }
        assert_eq!(
            OperationalProfile::try_from(4),
            Err(LogPolicyError::InvalidOperationalProfile(4))
        );
    }

    #[test]
    fn legacy_tiers_keep_old_numbers_but_migrate_explicitly() {
        let expected = [
            (0, VerbosityTier::Quiet, OperationalProfile::Error),
            (1, VerbosityTier::Normal, OperationalProfile::Info),
            (2, VerbosityTier::Debug, OperationalProfile::Debug),
            (3, VerbosityTier::Trace, OperationalProfile::Debug),
        ];
        for (old_value, tier, profile) in expected {
            assert_eq!(VerbosityTier::try_from(old_value), Ok(tier));
            assert_eq!(u8::from(tier), old_value);
            assert_eq!(OperationalProfile::from(tier), profile);
            assert_eq!(
                LevelSpec::new(tier).directive(),
                LevelSpec::new(profile).directive()
            );
        }
    }

    #[test]
    fn retention_is_clamped_to_supported_boundaries() {
        let low = RetentionPolicy::new(DEFAULT_ROTATE_BYTES, 1).expect("valid rotation");
        let high = RetentionPolicy::new(DEFAULT_ROTATE_BYTES, u16::MAX).expect("valid rotation");
        assert_eq!(low.retention_days(), MIN_RETENTION_DAYS);
        assert_eq!(high.retention_days(), MAX_RETENTION_DAYS);
        assert_eq!(RetentionPolicy::default().retention_days(), 30);
    }

    #[test]
    fn invalid_rotation_thresholds_are_rejected() {
        assert_eq!(
            RetentionPolicy::new(0, 30),
            Err(LogPolicyError::InvalidRotateBytes(0))
        );
        assert_eq!(
            RetentionPolicy::new(MAX_ROTATE_BYTES + 1, 30),
            Err(LogPolicyError::InvalidRotateBytes(MAX_ROTATE_BYTES + 1))
        );
    }

    #[test]
    fn maintenance_triggers_size_age_and_exact_retention_boundaries() {
        let policy = RetentionPolicy::default();
        let records = [
            record(8, LogFileKind::Active, DEFAULT_ROTATE_BYTES, 0),
            record(2, LogFileKind::Active, 1, SECONDS_PER_DAY),
            record(7, LogFileKind::Active, 0, SECONDS_PER_DAY * 2),
            record(
                4,
                LogFileKind::Archive,
                1,
                u64::from(DEFAULT_RETENTION_DAYS) * SECONDS_PER_DAY,
            ),
            record(
                3,
                LogFileKind::Session,
                1,
                u64::from(DEFAULT_RETENTION_DAYS - 1) * SECONDS_PER_DAY,
            ),
        ];

        let plan = plan_log_maintenance(&policy, &records).expect("bounded records");
        assert_eq!(plan.archive, [LogFileId::new(2), LogFileId::new(8)]);
        assert_eq!(plan.delete, [LogFileId::new(4)]);
    }

    #[test]
    fn output_is_sorted_deduplicated_and_disjoint() {
        let policy = RetentionPolicy::default();
        let records = [
            record(9, LogFileKind::Archive, 1, u64::MAX),
            record(3, LogFileKind::Active, u64::MAX, 0),
            record(9, LogFileKind::Archive, 1, u64::MAX),
            record(3, LogFileKind::Active, u64::MAX, 0),
        ];
        let plan = plan_log_maintenance(&policy, &records).expect("bounded records");
        assert_eq!(plan.archive, [LogFileId::new(3)]);
        assert_eq!(plan.delete, [LogFileId::new(9)]);
        assert!(
            plan.archive
                .iter()
                .all(|id| !plan.delete.iter().any(|deleted| deleted == id))
        );
    }

    #[test]
    fn record_cap_is_rejected_instead_of_truncated() {
        let records = vec![record(1, LogFileKind::Archive, 1, 0); MAX_LOG_FILE_RECORDS + 1];
        assert_eq!(
            plan_log_maintenance(&RetentionPolicy::default(), &records),
            Err(LogPolicyError::TooManyLogFiles(MAX_LOG_FILE_RECORDS + 1))
        );
    }
}
