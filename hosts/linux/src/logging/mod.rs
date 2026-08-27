//! Reloadable Linux logging built on the shared `arcen_observability` runtime.
//!
//! Every subsystem log line and lifecycle event flows through one
//! `arcen_observability::ObservabilityRuntime`: a managed (packaged) or
//! legacy (`--no-config`/dev) canonical JSON Lines file, an optional dev
//! console (legacy mode only), and the Linux journald/syslog bridge
//! (`eventlog::CanonicalJournalSink`, always registered). `ARCEN_LOG` is a
//! layered refinement on top of the resolved `OperationalProfile`, matching
//! the shared runtime's own semantics (not a full override, unlike the
//! pre-migration `EnvFilter`-based design).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use arcen_observability::{ObservabilityBuilder, ObservabilityHandle, ObservabilityRuntime};
use arcen_session::pier_config::LoggingConfig;
use arcen_telemetry::{
    names, plan_log_maintenance, LogFileId, LogFileKind, LogFileRecord, OperationalProfile,
    RetentionPolicy, TelemetryComponent, TelemetryPlatform, TelemetryRole, VerbosityTier,
};
use serde::Deserialize;

use crate::eventlog::{CanonicalJournalSink, RealJournalSyslogApi};

pub use arcen_telemetry::VerbosityTier as Verbosity;

/// Subsystem log targets.
pub mod target {
    pub const NET: &str = "arcen::net";
    pub const TLS: &str = "arcen::tls";
    pub const AUTH: &str = "arcen::auth";
    pub const SESSION: &str = "arcen::session";
    pub const MEDIA: &str = "arcen::media";
    pub const CAPENC: &str = "arcen::capenc";
    pub const DISPLAY: &str = "arcen::display";
    pub const INPUT: &str = "arcen::input";
    pub const AUDIO: &str = "arcen::audio";
    pub const HEALTH: &str = "arcen::health";
}

/// Logging startup settings resolved before the host CLI emits warnings.
#[derive(Debug, Clone)]
pub struct LoggingOptions {
    /// Effective cumulative operational profile (Level0 in production, per
    /// packaged/CI default, unless overridden).
    pub profile: OperationalProfile,
    /// Where `profile` came from: canonical `logging.level`, migrated
    /// one-release `logging.verbosity`, or the built-in production default.
    pub profile_source: arcen_session::pier_config::LoggingProfileSource,
    pub policy: RetentionPolicy,
    pub config_path: Option<PathBuf>,
    pub managed_log: Option<PathBuf>,
    pub retention_was_clamped: bool,
    /// Validated QoS/hysteresis thresholds this process starts with. A
    /// later SIGHUP re-reads and replaces this from the same config source
    /// (see [`LogController::qos_targets`]), so active sessions can pick up
    /// a corrected threshold without a restart.
    pub qos_targets: arcen_telemetry::QosTargets,
    profile_override: Option<OperationalProfile>,
}

impl LoggingOptions {
    /// Resolves logging from the unified Pier config plus CLI overrides.
    pub fn from_config(config: &crate::cli::Config, args: &[String]) -> Result<Self, String> {
        let resolved = config
            .logging
            .resolved_profile()
            .map_err(|error| format!("Pier config logging: {error:?}"))?;
        let profile_override = profile_override_from_args(args)?;
        let profile = profile_override.unwrap_or(resolved.profile);
        let retention_days = config
            .logging
            .retention_days
            .unwrap_or(arcen_telemetry::DEFAULT_RETENTION_DAYS);
        let policy = RetentionPolicy::new(arcen_telemetry::DEFAULT_ROTATE_BYTES, retention_days)
            .map_err(|error| format!("Pier config logging: {error}"))?;
        let managed_log = argument_value(args, "--managed-log")?
            .map(PathBuf::from)
            .or_else(|| config.managed_log.clone());
        Ok(Self {
            profile,
            profile_source: resolved.source,
            policy,
            config_path: config.config_path.clone(),
            managed_log,
            retention_was_clamped: policy.retention_days() != retention_days,
            qos_targets: config.logging.qos_targets,
            profile_override,
        })
    }

    /// Resolves legacy CLI-only logging for tests and `--no-config` diagnostics.
    pub fn from_args(args: &[String]) -> Result<Self, String> {
        Self::from_config(&crate::cli::Config::default(), args)
    }
}

#[derive(Debug, Clone, Copy)]
struct RuntimeState {
    profile: OperationalProfile,
    policy: RetentionPolicy,
    source: arcen_session::pier_config::LoggingProfileSource,
    qos_targets: arcen_telemetry::QosTargets,
}

/// Structured outcome of one `SIGHUP` reload attempt.
///
/// [`LogController::handle_sighup`] previously bundled the managed-log
/// reopen, the profile/QoS state reload, and archive cleanup into one
/// aggregate `Result`, so a reopen or cleanup failure made the *whole*
/// reload look failed even when the validated profile/QoS state had
/// already been committed to `LogController::state` and was safe to
/// publish. [`Self::state_committed`] answers exactly the "was the state
/// commit itself successful" question the QoS-targets publish gate needs,
/// independent of any other step's failure captured in
/// [`Self::error_message`].
#[derive(Debug, Clone, Default)]
pub struct SighupOutcome {
    state_committed: bool,
    errors: Vec<String>,
}

impl SighupOutcome {
    /// Whether the validated profile/QoS state was reloaded and committed
    /// to the controller's shared state this reload — the only condition
    /// under which a fresh `qos_targets()`/`profile()` read is safe to
    /// publish to sessions, regardless of any other step's outcome.
    pub fn state_committed(&self) -> bool {
        self.state_committed
    }

    /// Whether every step of the reload (state commit, managed-log
    /// reopen, archive cleanup) completed with no error at all.
    pub fn is_fully_ok(&self) -> bool {
        self.errors.is_empty()
    }

    /// The aggregated error report, if any step failed — for deduplicated
    /// host-side diagnostics only; never gates the QoS-targets publish.
    pub fn error_message(&self) -> Option<String> {
        if self.errors.is_empty() {
            None
        } else {
            Some(self.errors.join("; "))
        }
    }

    /// Test-only constructor for exercising the publish-gating logic
    /// without a real `LogController`/reload.
    #[cfg(test)]
    pub(crate) fn for_test(state_committed: bool, errors: Vec<String>) -> Self {
        Self {
            state_committed,
            errors,
        }
    }
}

/// Runtime controls retained for the process lifetime.
#[derive(Clone)]
pub struct LogController {
    runtime: Arc<ObservabilityRuntime>,
    handle: ObservabilityHandle,
    managed: Option<ManagedFileWriter>,
    config_path: Option<PathBuf>,
    profile_override: Option<OperationalProfile>,
    state: Arc<Mutex<RuntimeState>>,
}

impl LogController {
    /// Reopens, reloads, and cleans up after logrotate sends `SIGHUP`.
    ///
    /// All actions are attempted independently. A reopen or archive-cleanup
    /// failure never rolls back or suppresses a successful profile/QoS
    /// state commit: [`SighupOutcome::state_committed`] reports the state
    /// commit's own success, and [`SighupOutcome::error_message`] carries
    /// every other step's failure for host-side reporting.
    pub fn handle_sighup(&self) -> SighupOutcome {
        let mut errors = Vec::new();
        if let Some(writer) = &self.managed {
            if let Err(error) = writer.reopen() {
                errors.push(format!("reopen managed log: {error}"));
            }
        }

        let mut state_committed = false;
        match self.reloaded_state() {
            Ok(state) => {
                // Re-reads ARCEN_LOG live, matching the pre-migration
                // reload's own live-environment read.
                let arcen_log = std::env::var("ARCEN_LOG").ok();
                if let Err(error) = self.handle.reload_profile_with(state.profile, arcen_log) {
                    errors.push(format!("reload observability profile: {error}"));
                } else if let Ok(mut current) = self.state.lock() {
                    *current = state;
                    state_committed = true;
                } else {
                    errors.push("logging runtime state lock is poisoned".to_string());
                }
            }
            Err(error) => errors.push(error),
        }

        if let Err(error) = self.cleanup_archives() {
            errors.push(error);
        }
        SighupOutcome {
            state_committed,
            errors,
        }
    }

    /// Returns the authoritative managed path, if configured.
    pub fn managed_log_path(&self) -> Option<&Path> {
        self.managed.as_ref().map(ManagedFileWriter::path)
    }

    /// Returns the process's shared bridge handle, for the production
    /// `LifecycleEmitter`, health snapshots, and network-probe emission.
    pub fn handle(&self) -> ObservabilityHandle {
        self.handle.clone()
    }

    /// Returns the currently effective operational profile.
    pub fn profile(&self) -> Result<OperationalProfile, String> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "logging runtime state lock is poisoned".to_string())?
            .profile)
    }

    /// Returns the currently effective QoS/hysteresis thresholds. A
    /// successful SIGHUP reload replaces these atomically with a freshly
    /// validated read of the same config source; sessions started before
    /// or after the reload observe the same up-to-date value via this
    /// accessor (see `net::server`'s shared `Arc<RwLock<QosTargets>>`).
    pub fn qos_targets(&self) -> Result<arcen_telemetry::QosTargets, String> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "logging runtime state lock is poisoned".to_string())?
            .qos_targets)
    }

    /// Returns a stable name for the currently effective profile's
    /// configuration source, for the `EFFECTIVE_PROFILE` lifecycle event.
    /// A CLI/support-bundle override always wins regardless of the
    /// underlying config source, matching `LoggingOptions::from_config`.
    pub fn profile_source_name(&self) -> Result<&'static str, String> {
        if self.profile_override.is_some() {
            return Ok("cli_override");
        }
        let source = self
            .state
            .lock()
            .map_err(|_| "logging runtime state lock is poisoned".to_string())?
            .source;
        Ok(match source {
            arcen_session::pier_config::LoggingProfileSource::Level => "config_level",
            arcen_session::pier_config::LoggingProfileSource::LegacyVerbosity => {
                "config_legacy_verbosity"
            }
            arcen_session::pier_config::LoggingProfileSource::ProductionDefault => {
                "production_default"
            }
        })
    }

    /// Flushes and joins every registered sink with a bounded per-sink
    /// timeout, for a clean-exit shutdown. Never touches the global tracing
    /// dispatch; only drains and joins the sink worker threads.
    pub fn shutdown(&self, timeout_per_sink: Duration) -> Result<(), String> {
        self.runtime
            .guard()
            .shutdown(timeout_per_sink)
            .map_err(|error| error.to_string())
    }

    fn reloaded_state(&self) -> Result<RuntimeState, String> {
        let current = *self
            .state
            .lock()
            .map_err(|_| "logging runtime state lock is poisoned".to_string())?;
        let Some(path) = self.config_path.as_deref() else {
            return Ok(current);
        };
        let envelope = load_runtime_config(path)?;
        let resolved = envelope
            .logging
            .resolved_profile()
            .map_err(|error| format!("logging config: {error:?}"))?;
        let profile = self.profile_override.unwrap_or(resolved.profile);
        let retention_days = envelope
            .logging
            .retention_days
            .unwrap_or(arcen_telemetry::DEFAULT_RETENTION_DAYS);
        let policy = RetentionPolicy::new(arcen_telemetry::DEFAULT_ROTATE_BYTES, retention_days)
            .map_err(|error| format!("logging config: {error}"))?;
        Ok(RuntimeState {
            profile,
            policy,
            source: resolved.source,
            qos_targets: envelope.logging.qos_targets,
        })
    }

    fn cleanup_archives(&self) -> Result<(), String> {
        let Some(path) = self.managed_log_path() else {
            return Ok(());
        };
        let directory = path
            .parent()
            .ok_or_else(|| format!("managed log has no parent: {}", path.display()))?;
        let active_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("managed log name is not UTF-8: {}", path.display()))?;
        let prefix = format!("{active_name}-");
        let mut paths = Vec::new();
        let entries = std::fs::read_dir(directory)
            .map_err(|error| format!("enumerate {}: {error}", directory.display()))?;
        for entry in entries {
            let entry =
                entry.map_err(|error| format!("enumerate {}: {error}", directory.display()))?;
            let archive = entry.path();
            let metadata = std::fs::symlink_metadata(&archive)
                .map_err(|error| format!("inspect {}: {error}", archive.display()))?;
            let recognized = archive
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix));
            if recognized && metadata.is_file() && !metadata.file_type().is_symlink() {
                paths.push((archive, metadata));
            }
            if paths.len() > arcen_telemetry::MAX_LOG_FILE_RECORDS {
                return Err(format!(
                    "recognized log count exceeds {}",
                    arcen_telemetry::MAX_LOG_FILE_RECORDS
                ));
            }
        }

        let policy = self
            .state
            .lock()
            .map_err(|_| "logging runtime state lock is poisoned".to_string())?
            .policy;
        let now = SystemTime::now();
        let records = paths
            .iter()
            .enumerate()
            .map(|(index, (_, metadata))| LogFileRecord {
                id: LogFileId::new(index as u32),
                kind: LogFileKind::Archive,
                size_bytes: metadata.len(),
                age_seconds: metadata
                    .modified()
                    .ok()
                    .and_then(|modified| now.duration_since(modified).ok())
                    .map_or(0, |age| age.as_secs()),
            })
            .collect::<Vec<_>>();
        let plan = plan_log_maintenance(&policy, &records).map_err(|error| error.to_string())?;
        for id in plan.delete {
            let archive = &paths[id.value() as usize].0;
            std::fs::remove_file(archive)
                .map_err(|error| format!("delete expired log {}: {error}", archive.display()))?;
        }
        Ok(())
    }
}

/// Resolve the directory for the legacy rolling log file.
pub fn log_dir() -> PathBuf {
    if let Some(dir) = legacy_log_dir_override(std::env::var_os("ARCEN_LOG_DIR")) {
        return dir;
    }

    let var_log = PathBuf::from("/var/log/arcen");
    if std::fs::create_dir_all(&var_log).is_ok() && is_writable(&var_log) {
        return var_log;
    }
    if let Ok(state) = std::env::var("XDG_STATE_HOME") {
        if !state.is_empty() {
            return PathBuf::from(state).join("arcen");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return PathBuf::from(home).join(".local/state/arcen");
        }
    }
    PathBuf::from("/tmp/arcen")
}

/// Read-only log-root derivation for offline support collection.
///
/// This preserves `ARCEN_LOG_DIR` legacy mode but never creates or probes a
/// directory. Packaged mode is the fixed `/var/log/arcen` root.
pub fn support_bundle_log_root() -> (PathBuf, bool) {
    match legacy_log_dir_override(std::env::var_os("ARCEN_LOG_DIR")) {
        Some(path) => (path, true),
        None => (PathBuf::from("/var/log/arcen"), false),
    }
}

#[derive(Debug)]
pub struct SupportBundleLog {
    pub path: PathBuf,
    pub archive_path: String,
    pub size_bytes: u64,
    pub modified: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupportBundleLogError {
    PermissionDenied,
    Unavailable,
}

const MAX_SUPPORT_BUNDLE_LOGS: usize = arcen_telemetry::MAX_BUNDLE_ENTRIES - 32;
const SENSITIVE_SESSION_ROOT: &str = "/run/arcen/sessions";

fn classify_support_log_error(error: &std::io::Error) -> SupportBundleLogError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        SupportBundleLogError::PermissionDenied
    } else {
        SupportBundleLogError::Unavailable
    }
}

/// Enumerates only managed/legacy Arcen log names without mutating the root.
pub fn support_bundle_logs() -> Result<(Vec<SupportBundleLog>, bool, bool), SupportBundleLogError> {
    let (root, legacy) = support_bundle_log_root();
    if is_sensitive_session_root(&root) {
        return Err(SupportBundleLogError::Unavailable);
    }
    let root_metadata = match std::fs::symlink_metadata(&root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SupportBundleLogError::Unavailable);
        }
        Err(error) => return Err(classify_support_log_error(&error)),
    };
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return Err(SupportBundleLogError::Unavailable);
    }
    let resolved =
        std::fs::canonicalize(&root).map_err(|error| classify_support_log_error(&error))?;
    if is_sensitive_session_root(&resolved) {
        return Err(SupportBundleLogError::Unavailable);
    }
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SupportBundleLogError::Unavailable);
        }
        Err(error) => return Err(classify_support_log_error(&error)),
    };
    let mut logs = Vec::new();
    let mut truncated = false;
    let mut inspected = 0usize;
    for entry in entries {
        if inspected >= arcen_telemetry::MAX_LOG_FILE_RECORDS
            || logs.len() >= MAX_SUPPORT_BUNDLE_LOGS
        {
            truncated = true;
            break;
        }
        inspected += 1;
        let entry = entry.map_err(|error| classify_support_log_error(&error))?;
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|name| name.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if !is_owned_log_name(&name) {
            continue;
        }
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|error| classify_support_log_error(&error))?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            continue;
        }

        logs.push(SupportBundleLog {
            path,
            archive_path: format!("logs/{name}"),
            size_bytes: metadata.len(),
            modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        });
    }
    logs.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.archive_path.cmp(&right.archive_path))
    });
    Ok((logs, truncated, legacy))
}

fn is_owned_log_name(name: &str) -> bool {
    name == "arcen-pier.log" || is_managed_archive_name(name) || is_legacy_daily_name(name)
}

fn is_managed_archive_name(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("arcen-pier.log-") else {
        return false;
    };
    let suffix = suffix.strip_suffix(".gz").unwrap_or(suffix);
    let Some((date, epoch)) = suffix.split_once('-') else {
        return false;
    };
    date.len() == 8
        && date.bytes().all(|byte| byte.is_ascii_digit())
        && !epoch.is_empty()
        && epoch.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_legacy_daily_name(name: &str) -> bool {
    let Some(date) = name.strip_prefix("arcen-host.log.") else {
        return false;
    };
    date.len() == 10
        && date.as_bytes()[4] == b'-'
        && date.as_bytes()[7] == b'-'
        && date
            .bytes()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

fn is_sensitive_session_root(path: &Path) -> bool {
    path.starts_with(SENSITIVE_SESSION_ROOT)
        || path.starts_with(SENSITIVE_SESSION_ROOT.trim_start_matches('/'))
}

fn legacy_log_dir_override(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|dir| !dir.is_empty()).map(PathBuf::from)
}

fn is_writable(dir: &Path) -> bool {
    let probe = dir.join(".arcen-write-probe");
    match File::create(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Initializes the global logging backbone: one `ObservabilityRuntime` with
/// a managed-file or legacy-rolling-file canonical JSON Lines writer, an
/// optional dev console (legacy mode only), and the always-registered
/// Linux journald/syslog bridge.
pub fn init(options: &LoggingOptions) -> Result<LogController, String> {
    let (controller, dispatch) = build_controller(options)?;
    tracing::dispatcher::set_global_default(dispatch)
        .map_err(|error| format!("install global tracing dispatch: {error}"))?;
    Ok(controller)
}

/// Test-only construction sharing `init`'s exact runtime/writer/state setup,
/// but never installing the process-global tracing dispatcher (which can
/// only ever succeed once per test binary): `LogController::handle_sighup`
/// and every other method under test here operate on the controller's own
/// `ObservabilityHandle`/state, not the global dispatch, so this is a
/// behavior-preserving substitute for `init` in tests that construct more
/// than one `LogController`.
#[cfg(test)]
pub(crate) fn init_for_test(options: &LoggingOptions) -> Result<LogController, String> {
    build_controller(options).map(|(controller, _dispatch)| controller)
}

fn build_controller(
    options: &LoggingOptions,
) -> Result<(LogController, tracing::Dispatch), String> {
    let managed = match options.managed_log.as_ref() {
        Some(path) => Some(ManagedFileWriter::open(path.clone())?),
        None => None,
    };

    let component = TelemetryComponent::new(names::component::PIER)
        .map_err(|error| format!("pier component: {error:?}"))?;
    let mut builder = ObservabilityBuilder::new(
        TelemetryRole::Host,
        component,
        TelemetryPlatform::Linux,
        options.profile,
    );
    builder = if let Some(writer) = managed.clone() {
        builder.canonical_writer("managed-file", writer)
    } else {
        let directory = log_dir();
        std::fs::create_dir_all(&directory).map_err(|error| {
            format!(
                "create legacy log directory {}: {error}",
                directory.display()
            )
        })?;
        let rolling = tracing_appender::rolling::daily(&directory, "arcen-host.log");
        builder
            .canonical_writer("legacy-file", rolling)
            .human_console_writer("dev-console", std::io::stderr())
    };
    let runtime = builder
        .register_sink(
            "journald",
            CanonicalJournalSink::new(RealJournalSyslogApi::connect()),
        )
        .build()
        .map_err(|error| format!("build observability runtime: {error}"))?;
    let handle = runtime.handle();
    let dispatch = runtime.dispatch();

    Ok((
        LogController {
            runtime: Arc::new(runtime),
            handle,
            managed,
            config_path: options.config_path.clone(),
            profile_override: options.profile_override,
            state: Arc::new(Mutex::new(RuntimeState {
                profile: options.profile,
                policy: options.policy,
                source: options.profile_source,
                qos_targets: options.qos_targets,
            })),
        },
        dispatch,
    ))
}

/// Strict packaged logging configuration envelope: only the `logging`
/// section of the Pier config file is required for a reload, so this stays
/// a minimal, independently-parseable subset of the full `PierConfig`.
#[derive(Debug, Default, Deserialize)]
struct LoggingEnvelope {
    #[serde(default)]
    logging: LoggingConfig,
}

fn load_runtime_config(path: &Path) -> Result<LoggingEnvelope, String> {
    let bytes = std::fs::read(path).map_err(|error| format!("read {}: {error}", path.display()))?;
    serde_json::from_slice(&bytes).map_err(|error| format!("parse {}: {error}", path.display()))
}

fn argument_value(args: &[String], flag: &str) -> Result<Option<String>, String> {
    let mut values = args
        .iter()
        .enumerate()
        .filter_map(|(index, value)| (value == flag).then_some(index));
    let Some(index) = values.next() else {
        return Ok(None);
    };
    if values.next().is_some() {
        return Err(format!("{flag} may be supplied only once"));
    }
    args.get(index + 1)
        .cloned()
        .map(Some)
        .ok_or_else(|| format!("{flag} requires a value"))
}

/// Resolves the legacy `--verbosity`/`-v`/`-vv`/`-q` CLI overrides through
/// the shared `LoggingConfig::resolved_profile` legacy-verbosity mapping, so
/// the CLI and packaged-config legacy paths stay on exactly one migration
/// rule (`0`→Error, `1`→Info, `2|3`→Debug).
fn profile_override_from_args(args: &[String]) -> Result<Option<OperationalProfile>, String> {
    let verbosity = if let Some(value) = argument_value(args, "--verbosity")? {
        Some(
            value
                .parse::<u8>()
                .map_err(|_| "--verbosity must be an integer from 0 through 3".to_string())?,
        )
    } else if args
        .iter()
        .any(|argument| argument == "-vv" || argument == "--trace")
    {
        Some(u8::from(VerbosityTier::Trace))
    } else if args
        .iter()
        .any(|argument| argument == "-v" || argument == "--verbose")
    {
        Some(u8::from(VerbosityTier::Debug))
    } else if args
        .iter()
        .any(|argument| argument == "-q" || argument == "--quiet")
    {
        Some(u8::from(VerbosityTier::Quiet))
    } else {
        None
    };
    let Some(verbosity) = verbosity else {
        return Ok(None);
    };
    let legacy = LoggingConfig {
        verbosity: Some(verbosity),
        ..LoggingConfig::default()
    };
    legacy
        .resolved_profile()
        .map(|resolved| Some(resolved.profile))
        .map_err(|error| format!("--verbosity/-v/-vv/-q: {error:?}"))
}

/// Lock-backed writer that reopens after logrotate renames the active file.
#[derive(Clone, Debug)]
pub struct ManagedFileWriter {
    path: Arc<PathBuf>,
    file: Arc<Mutex<File>>,
}

impl ManagedFileWriter {
    pub fn open(path: PathBuf) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("create {}: {error}", parent.display()))?;
        }
        let file = open_append(&path)?;
        Ok(Self {
            path: Arc::new(path),
            file: Arc::new(Mutex::new(file)),
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }

    pub fn reopen(&self) -> Result<(), String> {
        let replacement = open_append(self.path())?;
        let mut file = self
            .file
            .lock()
            .map_err(|_| "managed log writer lock is poisoned".to_string())?;
        file.flush()
            .map_err(|error| format!("flush {}: {error}", self.path.display()))?;
        *file = replacement;
        Ok(())
    }
}

impl Write for ManagedFileWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.write(buffer)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.flush()
    }
}

fn open_append(path: &Path) -> Result<File, String> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("open {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_verbosity_migrates_to_the_documented_profile() {
        let legacy = LoggingConfig {
            verbosity: Some(2),
            ..LoggingConfig::default()
        };
        let resolved = legacy.resolved_profile().expect("legacy verbosity 2");
        assert_eq!(resolved.profile, OperationalProfile::Debug);
    }

    #[test]
    fn production_default_is_level0() {
        let production = LoggingConfig::default();
        let resolved = production.resolved_profile().expect("production default");
        assert_eq!(resolved.profile, OperationalProfile::Critical);
    }

    #[test]
    fn cli_overrides_parse_into_the_same_legacy_mapping() {
        assert_eq!(
            profile_override_from_args(&["--verbosity".to_string(), "2".to_string()])
                .expect("verbosity 2"),
            Some(OperationalProfile::Debug)
        );
        assert_eq!(
            profile_override_from_args(&["-vv".to_string()]).expect("-vv"),
            Some(OperationalProfile::Debug)
        );
        assert_eq!(
            profile_override_from_args(&["-q".to_string()]).expect("-q"),
            Some(OperationalProfile::Error)
        );
        assert_eq!(profile_override_from_args(&[]).expect("no override"), None);
    }

    #[test]
    fn logging_envelope_parses_the_shared_pier_config_schema() {
        // `level` is the canonical `OperationalProfile` discriminant
        // (Critical=0, Error=1, Info=2, Debug=3), not the legacy verbosity
        // scale, so `2` resolves directly to `Info`.
        let envelope: LoggingEnvelope =
            serde_json::from_str(r#"{"logging":{"level":2,"retention_days":2}}"#)
                .expect("logging envelope");
        let resolved = envelope
            .logging
            .resolved_profile()
            .expect("resolved profile");
        assert_eq!(resolved.profile, OperationalProfile::Info);
        let policy = RetentionPolicy::new(
            arcen_telemetry::DEFAULT_ROTATE_BYTES,
            envelope
                .logging
                .retention_days
                .expect("retention configured"),
        )
        .expect("policy");
        assert_eq!(policy.retention_days(), 7);
    }

    #[test]
    fn managed_writer_reopens_at_the_authoritative_path() {
        let root =
            std::env::temp_dir().join(format!("arcen-managed-log-test-{}", std::process::id()));
        let path = root.join("arcen-pier.log");
        let mut writer = ManagedFileWriter::open(path.clone()).expect("managed writer");
        writer.write_all(b"before\n").expect("first write");
        let archive = root.join("arcen-pier.log-1");
        std::fs::rename(&path, &archive).expect("rename active");
        writer.reopen().expect("reopen active");
        writer.write_all(b"after\n").expect("second write");
        assert_eq!(
            std::fs::read_to_string(archive).expect("archive"),
            "before\n"
        );
        assert_eq!(std::fs::read_to_string(path).expect("active"), "after\n");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn legacy_log_directory_override_is_preserved() {
        let explicit = PathBuf::from("/tmp/arcen-test-logs");
        assert_eq!(
            legacy_log_dir_override(Some(explicit.clone().into_os_string())),
            Some(explicit)
        );
        assert_eq!(
            legacy_log_dir_override(Some(std::ffi::OsString::new())),
            None
        );
        assert_eq!(legacy_log_dir_override(None), None);
    }

    #[test]
    fn support_inventory_excludes_xorg_and_unowned_logs() {
        assert!(is_owned_log_name("arcen-pier.log"));
        assert!(is_owned_log_name("arcen-pier.log-20260720-1753000000"));
        assert!(is_owned_log_name("arcen-pier.log-20260720-1753000000.gz"));
        assert!(is_owned_log_name("arcen-host.log.2026-07-20"));
        assert!(!is_owned_log_name("arcen-host.log.customer-secrets"));
        assert!(!is_owned_log_name("arcen-pier.log-20260720"));
        assert!(!is_owned_log_name("Xorg.log"));
        assert!(!is_owned_log_name("customer.log"));
    }

    #[test]
    fn support_inventory_never_enters_session_runtime() {
        assert!(is_sensitive_session_root(Path::new("/run/arcen/sessions")));
        assert!(is_sensitive_session_root(Path::new(
            "/run/arcen/sessions/user"
        )));
        assert!(is_sensitive_session_root(Path::new("run/arcen/sessions")));
        assert!(!is_sensitive_session_root(Path::new("/var/log/arcen")));
    }

    /// Re-review finding #1: a managed-log reopen failure must never
    /// suppress a successful profile/QoS state commit — `state_committed`
    /// must stay `true`, and only the reopen failure should surface in
    /// `error_message`, so the QoS-targets publish gate (driven by
    /// `state_committed` alone) is never starved by an unrelated I/O error.
    #[test]
    fn sighup_partial_success_still_reports_state_committed() {
        let root = std::env::temp_dir().join(format!(
            "arcen-sighup-partial-success-{}",
            std::process::id()
        ));
        let sub_dir = root.join("sub");
        let managed_path = sub_dir.join("arcen-pier.log");
        let options = LoggingOptions::from_args(&[
            "--managed-log".to_string(),
            managed_path.to_string_lossy().into_owned(),
        ])
        .expect("logging options with a managed-log override");
        // `init_for_test` shares every runtime/writer/state construction
        // step with `init` but skips the process-global tracing-dispatch
        // install, which can only ever succeed once per test binary and
        // would otherwise make this test order-dependent against any other
        // test that also constructs a real `LogController`.
        let controller = init_for_test(&options).expect("log controller opens the managed log");

        // Break the managed log's parent directory (replace it with a
        // plain file) so the next reopen fails with ENOTDIR, while leaving
        // the state-reload path (no `config_path`, so `reloaded_state`
        // trivially succeeds) completely unaffected.
        std::fs::remove_dir_all(&sub_dir).expect("remove sub directory");
        std::fs::write(&sub_dir, b"not a directory any more").expect("replace with a file");

        let outcome = controller.handle_sighup();
        assert!(
            outcome.state_committed(),
            "the profile/QoS state commit must succeed independent of the broken reopen"
        );
        assert!(
            !outcome.is_fully_ok(),
            "the broken reopen must still be reported"
        );
        assert!(
            outcome
                .error_message()
                .expect("reopen failure reported")
                .contains("reopen managed log"),
            "the error must name the reopen step, not the (unrelated, successful) state commit"
        );

        let _ = std::fs::remove_dir_all(&root);
    }

    /// The inverse of the above: when the state reload itself fails (an
    /// unparsable packaged config), `state_committed` must be `false` even
    /// though the managed-log reopen (no override configured here, so it is
    /// trivially skipped) reports no error of its own.
    #[test]
    fn sighup_state_reload_failure_reports_state_not_committed() {
        let root = std::env::temp_dir().join(format!(
            "arcen-sighup-reload-failure-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create config directory");
        let config_path = root.join("pier.json");
        std::fs::write(&config_path, b"not valid json").expect("write invalid config");

        let mut options = LoggingOptions::from_args(&[]).expect("logging options");
        options.config_path = Some(config_path);
        let controller = init_for_test(&options).expect("log controller");

        let outcome = controller.handle_sighup();
        assert!(
            !outcome.state_committed(),
            "an unparsable config must never commit a fabricated/stale state"
        );
        assert!(!outcome.is_fully_ok());

        let _ = std::fs::remove_dir_all(&root);
    }
}
