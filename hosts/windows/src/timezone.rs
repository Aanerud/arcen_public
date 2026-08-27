//! Broker-owned Windows system time-zone redirection and crash recovery.

use std::fmt::{Display, Formatter};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arcen_session::restore_lease::{
    IanaTimeZone, LeaseOwnerId, RecoveryDirective, RestoreEvent, RestoreLease, RestorePhase,
    RestoreResource, StateFingerprint,
};
use serde::{Deserialize, Serialize};

use crate::logging::SESSION;

const JOURNAL_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 64 * 1024;
const MAX_WINDOWS_ID_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowsSystemTime {
    year: u16,
    month: u16,
    day_of_week: u16,
    day: u16,
    hour: u16,
    minute: u16,
    second: u16,
    milliseconds: u16,
}

impl WindowsSystemTime {
    fn validate(&self, field: &str) -> Result<(), String> {
        if self.month == 0 {
            if [
                self.year,
                self.day_of_week,
                self.day,
                self.hour,
                self.minute,
                self.second,
                self.milliseconds,
            ]
            .iter()
            .any(|value| *value != 0)
            {
                return Err(format!("{field} has a partial disabled transition"));
            }
            return Ok(());
        }
        if self.month > 12
            || self.day_of_week > 6
            || self.hour > 23
            || self.minute > 59
            || self.second > 59
            || self.milliseconds > 999
        {
            return Err(format!("{field} has an out-of-range transition"));
        }
        if self.year == 0 {
            if !(1..=5).contains(&self.day) {
                return Err(format!("{field} recurring week is outside 1..=5"));
            }
        } else if !(1..=31).contains(&self.day) {
            return Err(format!("{field} day is outside 1..=31"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WindowsDynamicTimeZone {
    bias: i32,
    standard_name: String,
    standard_date: WindowsSystemTime,
    standard_bias: i32,
    daylight_name: String,
    daylight_date: WindowsSystemTime,
    daylight_bias: i32,
    time_zone_key_name: String,
    dynamic_daylight_time_disabled: bool,
}

impl WindowsDynamicTimeZone {
    fn validate(&self) -> Result<(), String> {
        validate_bias(self.bias, "bias")?;
        validate_bias(self.standard_bias, "standard_bias")?;
        validate_bias(self.daylight_bias, "daylight_bias")?;
        validate_wide_string(&self.standard_name, 31, "standard_name", true)?;
        validate_wide_string(&self.daylight_name, 31, "daylight_name", true)?;
        validate_wide_string(&self.time_zone_key_name, 127, "time_zone_key_name", false)?;
        self.standard_date.validate("standard_date")?;
        self.daylight_date.validate("daylight_date")?;
        Ok(())
    }

    fn fingerprint(&self) -> Result<StateFingerprint, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self)
            .map_err(|error| format!("serialize Windows time-zone snapshot: {error}"))?;
        StateFingerprint::from_bytes(&bytes)
            .map_err(|error| format!("fingerprint Windows time-zone snapshot: {error}"))
    }
}

fn validate_bias(value: i32, field: &str) -> Result<(), String> {
    if !(-1_440..=1_440).contains(&value) {
        return Err(format!("{field} is outside -1440..=1440 minutes"));
    }
    Ok(())
}

fn validate_wide_string(
    value: &str,
    max_units: usize,
    field: &str,
    empty_allowed: bool,
) -> Result<(), String> {
    if (!empty_allowed && value.is_empty())
        || value.encode_utf16().count() > max_units
        || value.contains('\0')
    {
        return Err(format!("{field} is empty, oversized, or contains NUL"));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TimezoneRecoveryJournal {
    version: u32,
    lease: RestoreLease,
    original: WindowsDynamicTimeZone,
    target_windows_id: String,
    target: WindowsDynamicTimeZone,
}

impl TimezoneRecoveryJournal {
    fn validate(&self) -> Result<(), String> {
        if self.version != JOURNAL_VERSION {
            return Err(format!(
                "timezone recovery journal version {} is unsupported",
                self.version
            ));
        }
        if self.lease.resource() != RestoreResource::Timezone {
            return Err("timezone recovery journal has the wrong resource".to_string());
        }
        validate_windows_id(&self.target_windows_id)?;
        self.original.validate()?;
        self.target.validate()?;
        if self.lease.original() != self.original.fingerprint()?
            || self.lease.target() != self.target.fingerprint()?
        {
            return Err("timezone recovery journal fingerprint mismatch".to_string());
        }
        if !self
            .target
            .time_zone_key_name
            .eq_ignore_ascii_case(&self.target_windows_id)
        {
            return Err("timezone recovery target key does not match target snapshot".to_string());
        }
        Ok(())
    }

    pub(crate) fn support_metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "version": self.version,
            "resource": "timezone",
            "phase": self.lease.phase(),
            "target_windows_id": self.target_windows_id,
            "original_fingerprint": hex_fingerprint(self.lease.original()),
            "target_fingerprint": hex_fingerprint(self.lease.target()),
        })
    }
}

fn hex_fingerprint(value: StateFingerprint) -> String {
    let mut output = String::with_capacity(64);
    for byte in value.as_bytes() {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn validate_windows_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_WINDOWS_ID_BYTES
        || !value.is_ascii()
        || value.contains(['\0', '\r', '\n'])
    {
        return Err("Windows time-zone identifier is invalid".to_string());
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn default_journal_path() -> PathBuf {
    crate::paths::recovery_dir().join("timezone-recovery.json")
}

#[cfg(not(windows))]
pub(crate) fn default_journal_path() -> PathBuf {
    PathBuf::from("timezone-recovery.json")
}

fn ensure_no_reparse_points(path: &Path) -> Result<(), String> {
    if path.as_os_str().is_empty() {
        return Err("timezone recovery journal path is empty".to_string());
    }
    if path
        .components()
        .any(|component| component == std::path::Component::ParentDir)
    {
        return Err(format!(
            "timezone recovery journal path contains a parent traversal: {}",
            path.display()
        ));
    }
    for component in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        if component.as_os_str().is_empty() {
            continue;
        }
        let metadata = match std::fs::symlink_metadata(component) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "inspect timezone recovery path component {}: {error}",
                    component.display()
                ));
            }
        };
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

            if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
                return Err(format!(
                    "refusing reparse point in timezone recovery path: {}",
                    component.display()
                ));
            }
        }
        #[cfg(not(windows))]
        if metadata.file_type().is_symlink() {
            return Err(format!(
                "refusing symlink in timezone recovery path: {}",
                component.display()
            ));
        }
    }
    Ok(())
}

fn open_journal_read(path: &Path) -> Result<std::fs::File, String> {
    ensure_no_reparse_points(path)?;
    #[cfg(windows)]
    let file = {
        use std::os::windows::fs::OpenOptionsExt as _;
        use windows::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT;

        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)
    };
    #[cfg(not(windows))]
    let file = std::fs::File::open(path);
    let file = file
        .map_err(|error| format!("open timezone recovery journal {}: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("stat timezone recovery journal {}: {error}", path.display()))?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(format!(
                "refusing reparse-point timezone recovery journal {}",
                path.display()
            ));
        }
    }
    if !metadata.is_file() {
        return Err(format!(
            "timezone recovery journal is not a regular file: {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(format!(
            "timezone recovery journal is {} bytes; limit is {MAX_JOURNAL_BYTES}",
            metadata.len()
        ));
    }
    Ok(file)
}

pub(crate) fn read_journal(path: &Path) -> Result<TimezoneRecoveryJournal, String> {
    let mut file = open_journal_read(path)?;
    let mut bytes = Vec::new();
    file.by_ref()
        .take(MAX_JOURNAL_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("read timezone recovery journal {}: {error}", path.display()))?;
    if bytes.len() as u64 > MAX_JOURNAL_BYTES {
        return Err(format!(
            "timezone recovery journal exceeded {MAX_JOURNAL_BYTES} bytes while reading"
        ));
    }
    let journal: TimezoneRecoveryJournal = serde_json::from_slice(&bytes).map_err(|error| {
        format!(
            "parse timezone recovery journal {}: {error}",
            path.display()
        )
    })?;
    journal.validate()?;
    Ok(journal)
}

fn write_journal(path: &Path, journal: &TimezoneRecoveryJournal) -> Result<(), String> {
    ensure_no_reparse_points(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| "timezone recovery journal has no parent directory".to_string())?;
    if !parent.is_dir() {
        return Err(format!(
            "timezone recovery directory does not exist: {}",
            parent.display()
        ));
    }
    journal.validate()?;
    let bytes = serde_json::to_vec_pretty(journal)
        .map_err(|error| format!("serialize timezone recovery journal: {error}"))?;
    crate::recovery::write_atomic_bytes(
        path,
        &bytes,
        MAX_JOURNAL_BYTES,
        "timezone recovery journal",
    )?;
    ensure_no_reparse_points(path)
}

fn remove_journal(path: &Path) -> Result<(), String> {
    ensure_no_reparse_points(path)?;
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "remove timezone recovery journal {}: {error}",
            path.display()
        )),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MutationError {
    PrivilegeDenied(String),
    Failed {
        message: String,
        may_have_mutated: bool,
    },
}

impl Display for MutationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PrivilegeDenied(message) => write!(formatter, "{message}"),
            Self::Failed {
                message,
                may_have_mutated,
            } => write!(
                formatter,
                "{message} (mutation uncertainty: {may_have_mutated})"
            ),
        }
    }
}

pub(crate) trait TimeZoneBackend: Send + Sync {
    fn current(&self) -> Result<WindowsDynamicTimeZone, String>;
    fn resolve(&self, windows_id: &str) -> Result<WindowsDynamicTimeZone, String>;
    fn apply(&self, state: &WindowsDynamicTimeZone) -> Result<(), MutationError>;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RecoveryOutcome {
    Clean,
    RemovedArmed,
    RemovedRestored,
    Restored,
    HoldForOperator,
}

fn reconcile(backend: &dyn TimeZoneBackend, path: &Path) -> Result<RecoveryOutcome, String> {
    ensure_no_reparse_points(path)?;
    match path.try_exists() {
        Ok(false) => return Ok(RecoveryOutcome::Clean),
        Ok(true) => {}
        Err(error) => {
            return Err(format!(
                "probe timezone recovery journal {}: {error}",
                path.display()
            ));
        }
    }
    let mut journal = read_journal(path)?;
    let current = backend.current()?;
    let current_fingerprint = current.fingerprint()?;
    match journal.lease.adjudicate_recovery(current_fingerprint) {
        RecoveryDirective::RemoveUnmutatedJournal => {
            remove_journal(path)?;
            Ok(RecoveryOutcome::RemovedArmed)
        }
        RecoveryDirective::RemoveRestoredJournal => {
            if !matches!(journal.lease.phase(), RestorePhase::Restored) {
                journal
                    .lease
                    .apply(RestoreEvent::BeginRestore)
                    .map_err(|error| error.to_string())?;
                write_journal(path, &journal)?;
                journal
                    .lease
                    .apply(RestoreEvent::RestoreSucceeded)
                    .map_err(|error| error.to_string())?;
                write_journal(path, &journal)?;
            }
            remove_journal(path)?;
            Ok(RecoveryOutcome::RemovedRestored)
        }
        RecoveryDirective::RestoreOriginal => {
            restore_journal(backend, path, journal)?;
            Ok(RecoveryOutcome::Restored)
        }
        RecoveryDirective::HoldForOperator => {
            write_journal(path, &journal)?;
            Ok(RecoveryOutcome::HoldForOperator)
        }
    }
}

fn restore_journal(
    backend: &dyn TimeZoneBackend,
    path: &Path,
    mut journal: TimezoneRecoveryJournal,
) -> Result<(), String> {
    journal
        .lease
        .apply(RestoreEvent::BeginRestore)
        .map_err(|error| error.to_string())?;
    write_journal(path, &journal)?;
    let apply_error = backend.apply(&journal.original).err();
    let current = backend.current()?;
    let current_fingerprint = current.fingerprint()?;
    if current_fingerprint == journal.lease.original() {
        journal
            .lease
            .apply(RestoreEvent::RestoreSucceeded)
            .map_err(|error| error.to_string())?;
        write_journal(path, &journal)?;
        remove_journal(path)?;
        return Ok(());
    }
    if current_fingerprint != journal.lease.target() {
        journal
            .lease
            .apply(RestoreEvent::OwnershipConflict)
            .map_err(|error| error.to_string())?;
        write_journal(path, &journal)?;
        return Err(
            "timezone restore found conflicting system state; journal retained".to_string(),
        );
    }
    Err(apply_error.map_or_else(
        || "timezone restore did not reach the original state; journal retained".to_string(),
        |error| format!("timezone restore failed; journal retained: {error}"),
    ))
}

trait JournalWriter: Send + Sync {
    fn write(&self, path: &Path, journal: &TimezoneRecoveryJournal) -> Result<(), String>;
}

struct DurableJournalWriter;

impl JournalWriter for DurableJournalWriter {
    fn write(&self, path: &Path, journal: &TimezoneRecoveryJournal) -> Result<(), String> {
        write_journal(path, journal)
    }
}

struct ControllerInner {
    backend: Arc<dyn TimeZoneBackend>,
    journal_writer: Arc<dyn JournalWriter>,
    journal_path: PathBuf,
    redirection_safe: AtomicBool,
    watchdog_enabled: bool,
}

#[derive(Clone)]
pub(crate) struct TimezoneController {
    inner: Arc<ControllerInner>,
}

impl TimezoneController {
    pub(crate) fn startup() -> Self {
        Self::with_backend_writer_and_watchdog(
            Arc::new(SystemTimeZoneBackend),
            Arc::new(DurableJournalWriter),
            default_journal_path(),
            true,
        )
    }

    #[cfg(test)]
    fn with_backend(backend: Arc<dyn TimeZoneBackend>, journal_path: PathBuf) -> Self {
        Self::with_backend_writer_and_watchdog(
            backend,
            Arc::new(DurableJournalWriter),
            journal_path,
            false,
        )
    }

    #[cfg(test)]
    fn with_backend_and_writer(
        backend: Arc<dyn TimeZoneBackend>,
        journal_writer: Arc<dyn JournalWriter>,
        journal_path: PathBuf,
    ) -> Self {
        Self::with_backend_writer_and_watchdog(backend, journal_writer, journal_path, false)
    }

    fn with_backend_writer_and_watchdog(
        backend: Arc<dyn TimeZoneBackend>,
        journal_writer: Arc<dyn JournalWriter>,
        journal_path: PathBuf,
        watchdog_enabled: bool,
    ) -> Self {
        let controller = Self {
            inner: Arc::new(ControllerInner {
                backend,
                journal_writer,
                journal_path,
                redirection_safe: AtomicBool::new(true),
                watchdog_enabled,
            }),
        };
        match reconcile(
            controller.inner.backend.as_ref(),
            &controller.inner.journal_path,
        ) {
            Ok(RecoveryOutcome::HoldForOperator) => {
                controller.disable();
                tracing::error!(
                    target: SESSION,
                    journal = %controller.inner.journal_path.display(),
                    "timezone recovery is conflicted; redirection held for operator"
                );
            }
            Ok(outcome) if outcome != RecoveryOutcome::Clean => tracing::warn!(
                target: SESSION,
                ?outcome,
                journal = %controller.inner.journal_path.display(),
                "startup timezone recovery reconciled a pending journal"
            ),
            Ok(_) => {}
            Err(error) => {
                controller.disable();
                tracing::error!(
                    target: SESSION,
                    %error,
                    journal = %controller.inner.journal_path.display(),
                    "timezone recovery is ambiguous; redirection disabled while service continues"
                );
            }
        }
        controller
    }

    fn disable(&self) {
        self.inner.redirection_safe.store(false, Ordering::Release);
    }

    fn write_journal(&self, journal: &TimezoneRecoveryJournal) -> Result<(), String> {
        self.inner
            .journal_writer
            .write(&self.inner.journal_path, journal)
    }

    #[cfg(test)]
    fn is_safe(&self) -> bool {
        self.inner.redirection_safe.load(Ordering::Acquire)
    }

    pub(crate) fn begin(
        &self,
        feature_enabled: bool,
        requested: Option<&str>,
        owner: &str,
    ) -> RedirectOutcome {
        if !feature_enabled {
            return RedirectOutcome::Disabled;
        }
        if !self.inner.redirection_safe.load(Ordering::Acquire) {
            return RedirectOutcome::RecoveryHeld;
        }
        let Some(requested) = requested else {
            return RedirectOutcome::Absent;
        };
        let iana = match IanaTimeZone::new(requested.to_string()) {
            Ok(iana) => iana,
            Err(error) => return RedirectOutcome::Invalid(error.to_string()),
        };
        let Some(windows_id) = crate::tz_map::windows_zone(iana.as_str()) else {
            return RedirectOutcome::Unmapped(iana.as_str().to_string());
        };
        match self.begin_mapped(windows_id, owner) {
            Ok(Some(lease)) => RedirectOutcome::Applied(lease),
            Ok(None) => RedirectOutcome::AlreadyCurrent(windows_id.to_string()),
            Err(error) => {
                if error.uncertain {
                    self.disable();
                }
                RedirectOutcome::Warning(error.message)
            }
        }
    }

    fn begin_mapped(
        &self,
        windows_id: &str,
        owner: &str,
    ) -> Result<Option<TimezoneLease>, RedirectFailure> {
        let original = self
            .inner
            .backend
            .current()
            .map_err(RedirectFailure::safe)?;
        let target = self
            .inner
            .backend
            .resolve(windows_id)
            .map_err(RedirectFailure::safe)?;
        if original == target {
            return Ok(None);
        }
        let owner = LeaseOwnerId::new(owner.to_string()).map_err(|error| {
            RedirectFailure::safe(format!("timezone lease owner is invalid: {error}"))
        })?;
        let owner_text = owner.as_str().to_string();
        let mut journal = TimezoneRecoveryJournal {
            version: JOURNAL_VERSION,
            lease: RestoreLease::arm(
                RestoreResource::Timezone,
                owner,
                original.fingerprint().map_err(RedirectFailure::safe)?,
                target.fingerprint().map_err(RedirectFailure::safe)?,
            ),
            original,
            target_windows_id: windows_id.to_string(),
            target,
        };
        self.write_journal(&journal)
            .map_err(RedirectFailure::safe)?;
        if self.inner.watchdog_enabled {
            if let Err(error) = crate::display::spawn_timezone_recovery_watchdog(
                &self.inner.journal_path,
                &owner_text,
            ) {
                let cleanup = remove_journal(&self.inner.journal_path);
                return Err(RedirectFailure::safe(match cleanup {
                    Ok(()) => error,
                    Err(cleanup) => format!("{error}; {cleanup}"),
                }));
            }
        }
        journal
            .lease
            .apply(RestoreEvent::BeginApply)
            .map_err(|error| RedirectFailure::safe(error.to_string()))?;
        self.write_journal(&journal)
            .map_err(RedirectFailure::safe)?;
        if let Err(error) = self.inner.backend.apply(&journal.target) {
            let recovery = reconcile(self.inner.backend.as_ref(), &self.inner.journal_path);
            return Err(match recovery {
                Ok(RecoveryOutcome::HoldForOperator) | Err(_) => RedirectFailure {
                    message: format!(
                        "timezone redirection failed and recovery is uncertain: {error}"
                    ),
                    uncertain: true,
                },
                Ok(_) => RedirectFailure {
                    message: format!("timezone redirection skipped: {error}"),
                    uncertain: false,
                },
            });
        }
        let lease = TimezoneLease {
            controller: self.clone(),
            active: true,
            target_windows_id: windows_id.to_string(),
        };
        let post_apply = (|| {
            let applied = self
                .inner
                .backend
                .current()
                .map_err(|error| format!("verify applied timezone: {error}"))?;
            if applied.fingerprint()? != journal.lease.target() {
                return Err("timezone apply verification failed".to_string());
            }
            journal
                .lease
                .apply(RestoreEvent::ApplySucceeded)
                .map_err(|error| error.to_string())?;
            self.write_journal(&journal)
        })();
        if let Err(error) = post_apply {
            return Err(self.restore_after_post_apply_failure(lease, error));
        }
        Ok(Some(lease))
    }

    fn restore_after_post_apply_failure(
        &self,
        lease: TimezoneLease,
        error: String,
    ) -> RedirectFailure {
        match lease.finish() {
            Ok(()) => RedirectFailure::safe(format!(
                "timezone redirection skipped after post-apply failure; original restored: {error}"
            )),
            Err(restore_error) => RedirectFailure {
                message: format!(
                    "timezone post-apply failure and synchronous restore could not be verified; \
                     journal retained: {error}; {restore_error}"
                ),
                uncertain: true,
            },
        }
    }

    pub(crate) fn restore_pending(&self) -> Result<RecoveryOutcome, String> {
        let outcome = reconcile(self.inner.backend.as_ref(), &self.inner.journal_path)?;
        if outcome == RecoveryOutcome::HoldForOperator {
            self.disable();
        }
        Ok(outcome)
    }
}

pub(crate) fn reconcile_service_entry() -> Result<RecoveryOutcome, String> {
    reconcile(&SystemTimeZoneBackend, &default_journal_path())
}

struct RedirectFailure {
    message: String,
    uncertain: bool,
}

impl RedirectFailure {
    fn safe(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            uncertain: false,
        }
    }
}

pub(crate) enum RedirectOutcome {
    Disabled,
    RecoveryHeld,
    Absent,
    Invalid(String),
    Unmapped(String),
    AlreadyCurrent(String),
    Applied(TimezoneLease),
    Warning(String),
}

pub(crate) struct TimezoneLease {
    controller: TimezoneController,
    active: bool,
    target_windows_id: String,
}

impl TimezoneLease {
    pub(crate) fn target_windows_id(&self) -> &str {
        &self.target_windows_id
    }

    pub(crate) fn finish(mut self) -> Result<(), String> {
        self.restore()
    }

    fn restore(&mut self) -> Result<(), String> {
        if !self.active {
            return Ok(());
        }
        let result = self.controller.restore_pending().and_then(|outcome| {
            if matches!(
                outcome,
                RecoveryOutcome::Restored | RecoveryOutcome::RemovedRestored
            ) {
                Ok(())
            } else {
                Err(format!("timezone lease restore ended in {outcome:?}"))
            }
        });
        if result.is_ok() {
            self.active = false;
        } else {
            self.controller.disable();
        }
        result
    }
}

impl Drop for TimezoneLease {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::error!(
                target: SESSION,
                %error,
                target_windows_id = %self.target_windows_id,
                "timezone lease drop could not verify restoration; journal retained"
            );
        }
    }
}

pub(crate) fn restore_from_journal(path: Option<PathBuf>) -> Result<(), String> {
    let path = path.unwrap_or_else(default_journal_path);
    ensure_no_reparse_points(&path)?;
    if path.try_exists().map_err(|error| {
        format!(
            "probe timezone recovery journal {}: {error}",
            path.display()
        )
    })? {
        let mut journal = read_journal(&path)?;
        if journal.lease.phase() == RestorePhase::Conflicted {
            tracing::warn!(
                target: SESSION,
                journal = %path.display(),
                "operator explicitly requested restore of a conflicted timezone journal"
            );
            rearm_conflicted_journal(&mut journal, |journal| write_journal(&path, journal))?;
            restore_journal(&SystemTimeZoneBackend, &path, journal)?;
            return Ok(());
        }
    }

    match reconcile(&SystemTimeZoneBackend, &path)? {
        RecoveryOutcome::HoldForOperator => Err(format!(
            "timezone recovery is conflicted; journal retained at {}",
            path.display()
        )),
        outcome => {
            tracing::info!(
                target: SESSION,
                ?outcome,
                journal = %path.display(),
                "operator timezone recovery completed"
            );
            Ok(())
        }
    }
}

fn rearm_conflicted_journal(
    journal: &mut TimezoneRecoveryJournal,
    persist: impl FnOnce(&TimezoneRecoveryJournal) -> Result<(), String>,
) -> Result<(), String> {
    journal.lease = RestoreLease::arm(
        RestoreResource::Timezone,
        journal.lease.owner().clone(),
        journal.lease.original(),
        journal.lease.target(),
    );
    journal
        .lease
        .apply(RestoreEvent::BeginApply)
        .map_err(|error| error.to_string())?;
    // Persist the re-armed operator override directly as Applying. An
    // intermediate Armed record would be interpreted as unmutated even though
    // the machine still holds the conflicted state.
    persist(journal)
}

#[cfg(windows)]
pub(crate) fn run_restore_watchdog(
    parent_handle: isize,
    ready_handle: isize,
    path: PathBuf,
    correlation_id: arcen_telemetry::CorrelationId,
) -> Result<(), String> {
    ensure_no_reparse_points(&path)?;
    match crate::recovery::wait_for_watchdog_parent(parent_handle, ready_handle, &path)? {
        crate::recovery::WatchdogWait::Disarmed => return Ok(()),
        crate::recovery::WatchdogWait::ParentExited => {}
    }
    tracing::warn!(
        target: SESSION,
        sid = %correlation_id,
        journal = %path.display(),
        "broker exited with active timezone redirection; watchdog restoring"
    );
    let mut last_error = String::new();
    for attempt in 1..=3 {
        match reconcile(&SystemTimeZoneBackend, &path) {
            Ok(RecoveryOutcome::HoldForOperator) => {
                return Err(
                    "timezone watchdog found conflicting system state; journal retained"
                        .to_string(),
                );
            }
            Ok(outcome) => {
                tracing::warn!(
                    target: SESSION,
                    sid = %correlation_id,
                    ?outcome,
                    "timezone watchdog recovery completed"
                );
                return Ok(());
            }
            Err(error) => {
                last_error = error;
                tracing::warn!(
                    target: SESSION,
                    sid = %correlation_id,
                    attempt,
                    error = %last_error,
                    "timezone watchdog recovery attempt failed"
                );
                if attempt < 3 {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    }
    Err(format!(
        "timezone watchdog recovery exhausted 3 attempts: {last_error}"
    ))
}

#[cfg(not(windows))]
pub(crate) fn run_restore_watchdog(
    _parent_handle: isize,
    _ready_handle: isize,
    _path: PathBuf,
    _correlation_id: arcen_telemetry::CorrelationId,
) -> Result<(), String> {
    Err("timezone recovery watchdog is only available on Windows".to_string())
}

#[cfg(windows)]
struct SystemTimeZoneBackend;

#[cfg(not(windows))]
struct SystemTimeZoneBackend;

#[cfg(not(windows))]
impl TimeZoneBackend for SystemTimeZoneBackend {
    fn current(&self) -> Result<WindowsDynamicTimeZone, String> {
        Err("Windows time-zone APIs are unavailable on this platform".to_string())
    }

    fn resolve(&self, _windows_id: &str) -> Result<WindowsDynamicTimeZone, String> {
        Err("Windows time-zone APIs are unavailable on this platform".to_string())
    }

    fn apply(&self, _state: &WindowsDynamicTimeZone) -> Result<(), MutationError> {
        Err(MutationError::PrivilegeDenied(
            "Windows time-zone APIs are unavailable on this platform".to_string(),
        ))
    }
}

#[cfg(windows)]
mod windows_adapter {
    use super::{
        MutationError, SystemTimeZoneBackend, TimeZoneBackend, WindowsDynamicTimeZone,
        WindowsSystemTime,
    };
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{
        CloseHandle, GetLastError, SetLastError, BOOLEAN, ERROR_NOT_ALL_ASSIGNED,
        ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, HANDLE, SYSTEMTIME,
    };
    use windows::Win32::Security::{
        AdjustTokenPrivileges, GetTokenInformation, IsWellKnownSid, LookupPrivilegeValueW,
        TokenUser, WinLocalSystemSid, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY, TOKEN_USER,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    use windows::Win32::System::Time::{
        EnumDynamicTimeZoneInformation, GetDynamicTimeZoneInformation,
        SetDynamicTimeZoneInformation, DYNAMIC_TIME_ZONE_INFORMATION, TIME_ZONE_ID_INVALID,
    };

    const MAX_ENUMERATED_TIME_ZONES: u32 = 4_096;

    impl TimeZoneBackend for SystemTimeZoneBackend {
        fn current(&self) -> Result<WindowsDynamicTimeZone, String> {
            let mut native = DYNAMIC_TIME_ZONE_INFORMATION::default();
            // SAFETY: native points to initialized writable storage of the documented type.
            let result = unsafe { GetDynamicTimeZoneInformation(&mut native) };
            if result == TIME_ZONE_ID_INVALID {
                return Err(format!(
                    "GetDynamicTimeZoneInformation: {}",
                    std::io::Error::last_os_error()
                ));
            }
            WindowsDynamicTimeZone::from_native(&native)
        }

        fn resolve(&self, windows_id: &str) -> Result<WindowsDynamicTimeZone, String> {
            super::validate_windows_id(windows_id)?;
            for index in 0..MAX_ENUMERATED_TIME_ZONES {
                let mut native = DYNAMIC_TIME_ZONE_INFORMATION::default();
                // SAFETY: native points to initialized writable storage; index is bounded.
                let result = unsafe { EnumDynamicTimeZoneInformation(index, &mut native) };
                if result == ERROR_NO_MORE_ITEMS.0 {
                    break;
                }
                if result != ERROR_SUCCESS.0 {
                    return Err(format!(
                        "EnumDynamicTimeZoneInformation({index}) failed with {result}"
                    ));
                }
                let candidate = WindowsDynamicTimeZone::from_native(&native)?;
                if candidate
                    .time_zone_key_name
                    .eq_ignore_ascii_case(windows_id)
                {
                    return Ok(candidate);
                }
            }
            Err(format!(
                "Windows time-zone key {windows_id:?} was not found"
            ))
        }

        fn apply(&self, state: &WindowsDynamicTimeZone) -> Result<(), MutationError> {
            let native = state.to_native().map_err(|message| MutationError::Failed {
                message,
                may_have_mutated: false,
            })?;
            let _privilege = TimeZonePrivilege::enable()?;
            // SAFETY: native is a fully initialized validated snapshot and remains alive.
            unsafe { SetDynamicTimeZoneInformation(&native) }.map_err(|error| {
                MutationError::Failed {
                    message: format!("SetDynamicTimeZoneInformation: {error}"),
                    may_have_mutated: true,
                }
            })
        }
    }

    impl WindowsDynamicTimeZone {
        fn from_native(native: &DYNAMIC_TIME_ZONE_INFORMATION) -> Result<Self, String> {
            let value = Self {
                bias: native.Bias,
                standard_name: from_wide(&native.StandardName, "StandardName")?,
                standard_date: WindowsSystemTime::from_native(native.StandardDate),
                standard_bias: native.StandardBias,
                daylight_name: from_wide(&native.DaylightName, "DaylightName")?,
                daylight_date: WindowsSystemTime::from_native(native.DaylightDate),
                daylight_bias: native.DaylightBias,
                time_zone_key_name: from_wide(&native.TimeZoneKeyName, "TimeZoneKeyName")?,
                dynamic_daylight_time_disabled: native.DynamicDaylightTimeDisabled.0 != 0,
            };
            value.validate()?;
            Ok(value)
        }

        fn to_native(&self) -> Result<DYNAMIC_TIME_ZONE_INFORMATION, String> {
            self.validate()?;
            Ok(DYNAMIC_TIME_ZONE_INFORMATION {
                Bias: self.bias,
                StandardName: to_wide(&self.standard_name)?,
                StandardDate: self.standard_date.to_native(),
                StandardBias: self.standard_bias,
                DaylightName: to_wide(&self.daylight_name)?,
                DaylightDate: self.daylight_date.to_native(),
                DaylightBias: self.daylight_bias,
                TimeZoneKeyName: to_wide(&self.time_zone_key_name)?,
                DynamicDaylightTimeDisabled: BOOLEAN(u8::from(self.dynamic_daylight_time_disabled)),
            })
        }
    }

    impl WindowsSystemTime {
        fn from_native(native: SYSTEMTIME) -> Self {
            Self {
                year: native.wYear,
                month: native.wMonth,
                day_of_week: native.wDayOfWeek,
                day: native.wDay,
                hour: native.wHour,
                minute: native.wMinute,
                second: native.wSecond,
                milliseconds: native.wMilliseconds,
            }
        }

        fn to_native(&self) -> SYSTEMTIME {
            SYSTEMTIME {
                wYear: self.year,
                wMonth: self.month,
                wDayOfWeek: self.day_of_week,
                wDay: self.day,
                wHour: self.hour,
                wMinute: self.minute,
                wSecond: self.second,
                wMilliseconds: self.milliseconds,
            }
        }
    }

    fn from_wide<const N: usize>(value: &[u16; N], field: &str) -> Result<String, String> {
        let end = value.iter().position(|unit| *unit == 0).unwrap_or(N);
        String::from_utf16(&value[..end]).map_err(|_| format!("{field} contains invalid UTF-16"))
    }

    fn to_wide<const N: usize>(value: &str) -> Result<[u16; N], String> {
        let encoded = value.encode_utf16().collect::<Vec<_>>();
        if encoded.len() >= N {
            return Err(format!("Windows string exceeds {} UTF-16 units", N - 1));
        }
        let mut result = [0; N];
        result[..encoded.len()].copy_from_slice(&encoded);
        Ok(result)
    }

    fn token_is_local_system(token: HANDLE) -> Result<bool, String> {
        let mut bytes_needed = 0;
        // SAFETY: this documented sizing call writes only bytes_needed.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut bytes_needed) };
        if bytes_needed == 0 || bytes_needed > 4_096 {
            return Err(format!(
                "GetTokenInformation(TokenUser) returned invalid size {bytes_needed}"
            ));
        }
        let word_size = std::mem::size_of::<usize>();
        let mut storage = vec![0usize; (bytes_needed as usize).div_ceil(word_size)];
        // SAFETY: storage is aligned and has at least bytes_needed writable bytes.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(storage.as_mut_ptr().cast()),
                bytes_needed,
                &mut bytes_needed,
            )
        }
        .map_err(|error| format!("GetTokenInformation(TokenUser): {error}"))?;
        // SAFETY: GetTokenInformation initialized a TOKEN_USER at the aligned buffer start.
        let user = unsafe { &*storage.as_ptr().cast::<TOKEN_USER>() };
        // SAFETY: the SID pointer belongs to storage, remains live, and was returned by Windows.
        Ok(unsafe { IsWellKnownSid(user.User.Sid, WinLocalSystemSid) }.as_bool())
    }

    struct TimeZonePrivilege {
        token: HANDLE,
        previous: TOKEN_PRIVILEGES,
    }

    impl TimeZonePrivilege {
        fn enable() -> Result<Self, MutationError> {
            let mut token = HANDLE::default();
            // SAFETY: GetCurrentProcess is a pseudo-handle and token is valid writable storage.
            unsafe {
                OpenProcessToken(
                    GetCurrentProcess(),
                    TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                    &mut token,
                )
            }
            .map_err(|error| {
                MutationError::PrivilegeDenied(format!("OpenProcessToken: {error}"))
            })?;
            match token_is_local_system(token) {
                Ok(true) => {}
                Ok(false) => {
                    // SAFETY: token is uniquely owned and valid after OpenProcessToken.
                    unsafe {
                        let _ = CloseHandle(token);
                    }
                    return Err(MutationError::PrivilegeDenied(
                        "timezone mutation is restricted to the LocalSystem broker".to_string(),
                    ));
                }
                Err(error) => {
                    // SAFETY: token is uniquely owned and valid after OpenProcessToken.
                    unsafe {
                        let _ = CloseHandle(token);
                    }
                    return Err(MutationError::PrivilegeDenied(error));
                }
            }
            let mut luid = Default::default();
            let name: Vec<u16> = "SeTimeZonePrivilege".encode_utf16().chain([0]).collect();
            // SAFETY: name is NUL-terminated, luid is valid writable storage, and token is owned.
            if let Err(error) =
                unsafe { LookupPrivilegeValueW(None, PCWSTR(name.as_ptr()), &mut luid) }
            {
                // SAFETY: token is uniquely owned and valid after OpenProcessToken.
                unsafe {
                    let _ = CloseHandle(token);
                }

                return Err(MutationError::PrivilegeDenied(format!(
                    "LookupPrivilegeValueW(SeTimeZonePrivilege): {error}"
                )));
            }
            let requested = TOKEN_PRIVILEGES {
                PrivilegeCount: 1,
                Privileges: [LUID_AND_ATTRIBUTES {
                    Luid: luid,
                    Attributes: SE_PRIVILEGE_ENABLED,
                }],
            };
            let mut previous = TOKEN_PRIVILEGES::default();
            let mut previous_length = 0;
            // SAFETY: all token privilege buffers are initialized and correctly sized.
            let adjusted = unsafe {
                SetLastError(ERROR_SUCCESS);
                AdjustTokenPrivileges(
                    token,
                    false,
                    Some(&requested),
                    std::mem::size_of::<TOKEN_PRIVILEGES>() as u32,
                    Some(&mut previous),
                    Some(&mut previous_length),
                )
            };
            // AdjustTokenPrivileges can return success while reporting that the token lacks it.
            // SAFETY: GetLastError reads thread-local error state immediately after the call.
            let last_error = unsafe { GetLastError() };
            if let Err(error) = adjusted {
                // SAFETY: token is uniquely owned and valid.
                unsafe {
                    let _ = CloseHandle(token);
                }
                return Err(MutationError::PrivilegeDenied(format!(
                    "AdjustTokenPrivileges: {error}"
                )));
            }
            if last_error == ERROR_NOT_ALL_ASSIGNED {
                // SAFETY: token is uniquely owned and valid.
                unsafe {
                    let _ = CloseHandle(token);
                }
                return Err(MutationError::PrivilegeDenied(
                    "SeTimeZonePrivilege is not assigned to the LocalSystem broker token"
                        .to_string(),
                ));
            }
            Ok(Self { token, previous })
        }
    }

    impl Drop for TimeZonePrivilege {
        fn drop(&mut self) {
            // SAFETY: token remains valid and previous is the exact state returned by enable.
            unsafe {
                let _ =
                    AdjustTokenPrivileges(self.token, false, Some(&self.previous), 0, None, None);
                let _ = CloseHandle(self.token);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    fn system_time(month: u16) -> WindowsSystemTime {
        WindowsSystemTime {
            year: 0,
            month,
            day_of_week: if month == 0 { 0 } else { 1 },
            day: if month == 0 { 0 } else { 2 },
            hour: if month == 0 { 0 } else { 2 },
            minute: 0,
            second: 0,
            milliseconds: 0,
        }
    }

    fn zone(key: &str, bias: i32) -> WindowsDynamicTimeZone {
        WindowsDynamicTimeZone {
            bias,
            standard_name: "Standard".to_string(),
            standard_date: system_time(11),
            standard_bias: 0,
            daylight_name: "Daylight".to_string(),
            daylight_date: system_time(3),
            daylight_bias: -60,
            time_zone_key_name: key.to_string(),
            dynamic_daylight_time_disabled: false,
        }
    }

    struct FakeState {
        current: WindowsDynamicTimeZone,
        zones: BTreeMap<String, WindowsDynamicTimeZone>,
        apply_error: Option<MutationError>,
        current_failures_after_apply: usize,
        journal_path: PathBuf,
        observed_phases: Vec<RestorePhase>,
        apply_count: usize,
    }

    struct FakeBackend {
        state: Mutex<FakeState>,
    }

    struct FailAppliedJournalWrite {
        failed: AtomicBool,
    }

    impl JournalWriter for FailAppliedJournalWrite {
        fn write(&self, path: &Path, journal: &TimezoneRecoveryJournal) -> Result<(), String> {
            if journal.lease.phase() == RestorePhase::Applied
                && !self.failed.swap(true, Ordering::AcqRel)
            {
                return Err("injected applied journal write failure".to_string());
            }
            write_journal(path, journal)
        }
    }

    impl TimeZoneBackend for FakeBackend {
        fn current(&self) -> Result<WindowsDynamicTimeZone, String> {
            let mut state = self.state.lock().expect("fake state");
            if state.current_failures_after_apply != 0 && state.apply_count != 0 {
                state.current_failures_after_apply -= 1;
                return Err("injected post-apply current failure".to_string());
            }
            Ok(state.current.clone())
        }

        fn resolve(&self, windows_id: &str) -> Result<WindowsDynamicTimeZone, String> {
            self.state
                .lock()
                .expect("fake state")
                .zones
                .get(windows_id)
                .cloned()
                .ok_or_else(|| "unavailable Windows zone".to_string())
        }

        fn apply(&self, value: &WindowsDynamicTimeZone) -> Result<(), MutationError> {
            let mut state = self.state.lock().expect("fake state");
            if state.journal_path.exists() {
                let phase = read_journal(&state.journal_path)
                    .expect("valid journal")
                    .lease
                    .phase();
                state.observed_phases.push(phase);
            }
            state.apply_count += 1;
            if let Some(error) = state.apply_error.take() {
                if matches!(
                    error,
                    MutationError::Failed {
                        may_have_mutated: true,
                        ..
                    }
                ) {
                    state.current = value.clone();
                }
                return Err(error);
            }
            state.current = value.clone();
            Ok(())
        }
    }

    fn test_path(name: &str) -> PathBuf {
        let root = PathBuf::from("target").join("arcen-timezone-tests");
        std::fs::create_dir_all(&root).expect("test journal directory");
        let path = root.join(format!("{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&path);
        path
    }

    fn controller(name: &str) -> (TimezoneController, Arc<FakeBackend>, PathBuf) {
        let path = test_path(name);
        let original = zone("W. Europe Standard Time", -60);
        let target = zone("Pacific Standard Time", 480);
        let backend = Arc::new(FakeBackend {
            state: Mutex::new(FakeState {
                current: original,
                zones: BTreeMap::from([("Pacific Standard Time".to_string(), target)]),
                apply_error: None,
                current_failures_after_apply: 0,
                journal_path: path.clone(),
                observed_phases: Vec::new(),
                apply_count: 0,
            }),
        });
        (
            TimezoneController::with_backend(backend.clone(), path.clone()),
            backend,
            path,
        )
    }

    #[test]
    fn feature_decisions_do_not_mutate() {
        let (controller, backend, _) = controller("decisions");
        assert!(matches!(
            controller.begin(false, Some("America/Los_Angeles"), "sid"),
            RedirectOutcome::Disabled
        ));
        assert!(matches!(
            controller.begin(true, None, "sid"),
            RedirectOutcome::Absent
        ));
        assert!(matches!(
            controller.begin(true, Some("../bad"), "sid"),
            RedirectOutcome::Invalid(_)
        ));
        assert!(matches!(
            controller.begin(true, Some("Arcen/Unknown"), "sid"),
            RedirectOutcome::Unmapped(_)
        ));
        assert_eq!(backend.state.lock().expect("fake state").apply_count, 0);
    }

    #[test]
    fn already_current_is_a_noop() {
        let (controller, backend, path) = controller("already-current");
        backend.state.lock().expect("fake state").current = zone("Pacific Standard Time", 480);
        assert!(matches!(
            controller.begin(true, Some("America/Los_Angeles"), "current-owner"),
            RedirectOutcome::AlreadyCurrent(_)
        ));
        assert_eq!(backend.state.lock().expect("fake state").apply_count, 0);
        assert!(!path.exists());
    }

    #[test]
    fn apply_and_explicit_restore_observe_durable_phase_order() {
        let (controller, backend, path) = controller("phase-order");
        let lease = match controller.begin(
            true,
            Some("America/Los_Angeles"),
            "00000000-0000-4000-8000-000000000001",
        ) {
            RedirectOutcome::Applied(lease) => lease,
            _ => panic!("expected applied lease"),
        };
        assert_eq!(
            read_journal(&path).expect("journal").lease.phase(),
            RestorePhase::Applied
        );
        lease.finish().expect("restore");
        assert!(!path.exists());
        assert_eq!(
            backend.state.lock().expect("fake state").observed_phases,
            [RestorePhase::Applying, RestorePhase::Restoring]
        );
    }

    #[test]
    fn restart_and_repeated_recovery_are_idempotent() {
        let (controller, backend, path) = controller("restart");
        let lease = match controller.begin(true, Some("America/Los_Angeles"), "restart-owner") {
            RedirectOutcome::Applied(lease) => lease,
            _ => panic!("expected applied lease"),
        };
        std::mem::forget(lease);
        let restarted = TimezoneController::with_backend(backend, path.clone());
        assert!(restarted.is_safe());
        assert!(!path.exists());
        assert_eq!(restarted.restore_pending(), Ok(RecoveryOutcome::Clean));
    }

    #[test]
    fn conflict_is_retained_and_disables_redirection() {
        let (controller, backend, path) = controller("conflict");
        let lease = match controller.begin(true, Some("America/Los_Angeles"), "conflict-owner") {
            RedirectOutcome::Applied(lease) => lease,
            _ => panic!("expected applied lease"),
        };
        std::mem::forget(lease);
        backend.state.lock().expect("fake state").current = zone("Third Party Time", 0);
        let restarted = TimezoneController::with_backend(backend, path.clone());
        assert!(!restarted.is_safe());
        assert_eq!(
            read_journal(&path).expect("retained journal").lease.phase(),
            RestorePhase::Conflicted
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn malformed_size_and_version_disable_only_redirection() {
        for (name, bytes) in [
            ("corrupt", b"{not json".to_vec()),
            ("oversize", vec![b'x'; MAX_JOURNAL_BYTES as usize + 1]),
            (
                "version",
                serde_json::to_vec(&serde_json::json!({"version":999})).expect("json"),
            ),
        ] {
            let path = test_path(name);
            std::fs::write(&path, bytes).expect("bad journal");
            let backend = Arc::new(FakeBackend {
                state: Mutex::new(FakeState {
                    current: zone("Original", 0),
                    zones: BTreeMap::new(),
                    apply_error: None,
                    current_failures_after_apply: 0,
                    journal_path: path.clone(),
                    observed_phases: Vec::new(),
                    apply_count: 0,
                }),
            });
            let controller = TimezoneController::with_backend(backend, path.clone());
            assert!(!controller.is_safe());
            assert!(path.exists());
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn privilege_denied_rolls_back_without_blocking_session() {
        let (controller, backend, path) = controller("privilege");
        backend.state.lock().expect("fake state").apply_error =
            Some(MutationError::PrivilegeDenied("denied".to_string()));
        assert!(matches!(
            controller.begin(true, Some("America/Los_Angeles"), "privilege-owner"),
            RedirectOutcome::Warning(_)
        ));
        assert!(!path.exists());
        assert!(controller.is_safe());
        assert_eq!(
            backend
                .state
                .lock()
                .expect("fake state")
                .current
                .time_zone_key_name,
            "W. Europe Standard Time"
        );
    }

    #[test]
    fn uncertain_apply_failure_restores_and_removes_journal() {
        let (controller, backend, path) = controller("apply-failure");
        backend.state.lock().expect("fake state").apply_error = Some(MutationError::Failed {
            message: "reported failure after mutation".to_string(),
            may_have_mutated: true,
        });
        assert!(matches!(
            controller.begin(true, Some("America/Los_Angeles"), "failure-owner"),
            RedirectOutcome::Warning(_)
        ));
        assert!(!path.exists());
        assert!(controller.is_safe());
        assert_eq!(
            backend
                .state
                .lock()
                .expect("fake state")
                .current
                .time_zone_key_name,
            "W. Europe Standard Time"
        );
    }

    #[test]
    fn post_apply_current_failure_restores_synchronously() {
        let (controller, backend, path) = controller("post-apply-current");
        backend
            .state
            .lock()
            .expect("fake state")
            .current_failures_after_apply = 1;

        assert!(matches!(
            controller.begin(true, Some("America/Los_Angeles"), "current-failure-owner"),
            RedirectOutcome::Warning(_)
        ));
        let state = backend.state.lock().expect("fake state");
        assert_eq!(state.current.time_zone_key_name, "W. Europe Standard Time");
        assert_eq!(state.apply_count, 2);
        assert!(controller.is_safe());
        assert!(!path.exists());
    }

    #[test]
    fn post_apply_journal_write_failure_restores_synchronously() {
        let (_, backend, path) = controller("post-apply-write");
        let controller = TimezoneController::with_backend_and_writer(
            backend.clone(),
            Arc::new(FailAppliedJournalWrite {
                failed: AtomicBool::new(false),
            }),
            path.clone(),
        );

        assert!(matches!(
            controller.begin(true, Some("America/Los_Angeles"), "write-failure-owner"),
            RedirectOutcome::Warning(_)
        ));
        let state = backend.state.lock().expect("fake state");
        assert_eq!(state.current.time_zone_key_name, "W. Europe Standard Time");
        assert_eq!(state.apply_count, 2);
        assert!(controller.is_safe());
        assert!(!path.exists());
    }

    #[test]
    fn failed_post_apply_reconciliation_retains_journal_and_disables_controller() {
        let (controller, backend, path) = controller("post-apply-uncertain");
        backend
            .state
            .lock()
            .expect("fake state")
            .current_failures_after_apply = 3;

        assert!(matches!(
            controller.begin(true, Some("America/Los_Angeles"), "uncertain-owner"),
            RedirectOutcome::Warning(_)
        ));
        assert_eq!(
            backend
                .state
                .lock()
                .expect("fake state")
                .current
                .time_zone_key_name,
            "Pacific Standard Time"
        );
        assert!(!controller.is_safe());
        assert!(path.exists());
        assert_eq!(
            read_journal(&path).expect("retained journal").lease.phase(),
            RestorePhase::Applying
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn conflicted_operator_restore_persists_only_applying() {
        let original = zone("Original Time", 0);
        let target = zone("Target Time", 60);
        let mut journal = TimezoneRecoveryJournal {
            version: JOURNAL_VERSION,
            lease: RestoreLease::arm(
                RestoreResource::Timezone,
                LeaseOwnerId::new("operator-owner").expect("owner"),
                original.fingerprint().expect("original fingerprint"),
                target.fingerprint().expect("target fingerprint"),
            ),
            original,
            target_windows_id: "Target Time".to_string(),
            target,
        };
        journal
            .lease
            .apply(RestoreEvent::BeginApply)
            .expect("begin apply");
        journal
            .lease
            .apply(RestoreEvent::OwnershipConflict)
            .expect("conflict");
        let observed = Mutex::new(Vec::new());

        rearm_conflicted_journal(&mut journal, |journal| {
            observed
                .lock()
                .expect("observed phases")
                .push(journal.lease.phase());
            Ok(())
        })
        .expect("rearm");

        assert_eq!(
            *observed.lock().expect("observed phases"),
            [RestorePhase::Applying]
        );
    }

    #[cfg(windows)]
    #[test]
    fn default_journal_uses_protected_recovery_directory() {
        assert_eq!(
            default_journal_path(),
            crate::paths::recovery_dir().join("timezone-recovery.json")
        );
        assert_ne!(
            default_journal_path(),
            crate::paths::arcen_data_root()
                .join("runtime")
                .join("timezone-recovery.json")
        );
    }

    #[cfg(windows)]
    #[test]
    fn recovery_path_rejects_existing_reparse_component() {
        use std::os::windows::fs::symlink_dir;

        let root = PathBuf::from("target").join(format!(
            "arcen-timezone-reparse-test-{}",
            std::process::id()
        ));
        let real = root.join("real");
        let link = root.join("link");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&real).expect("real recovery directory");
        if symlink_dir(&real, &link).is_err() {
            let _ = std::fs::remove_dir_all(root);
            return;
        }

        let error = ensure_no_reparse_points(&link.join("timezone-recovery.json"))
            .expect_err("reparse point must be rejected");
        assert!(error.contains("reparse point"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn strict_snapshot_validation_rejects_bad_ranges() {
        let mut invalid = zone("Invalid", 0);
        invalid.standard_date.month = 13;
        assert!(invalid.validate().is_err());
        invalid.standard_date.month = 11;
        invalid.time_zone_key_name = "x".repeat(128);
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn support_metadata_excludes_full_snapshots_and_owner() {
        let original = zone("Original Time", 0);
        let target = zone("Target Time", 60);
        let journal = TimezoneRecoveryJournal {
            version: JOURNAL_VERSION,
            lease: RestoreLease::arm(
                RestoreResource::Timezone,
                LeaseOwnerId::new("private-owner").expect("owner"),
                original.fingerprint().expect("original fingerprint"),
                target.fingerprint().expect("target fingerprint"),
            ),
            original,
            target_windows_id: "Target Time".to_string(),
            target,
        };
        let metadata = serde_json::to_string(&journal.support_metadata()).expect("metadata");
        assert!(!metadata.contains("private-owner"));
        assert!(!metadata.contains("standard_name"));
        assert!(metadata.contains("target_windows_id"));
        assert!(metadata.len() < 1_024);
    }
}
