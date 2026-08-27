use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::fs::Metadata;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use arcen_telemetry::{
    plan_log_maintenance, LogFileId, LogFileKind, LogFileRecord, RetentionPolicy,
};
use windows::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;

use crate::session::BrokerAgentLease;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActiveLog {
    Broker,
    Agent,
}

struct DiscoveredLog {
    path: PathBuf,
    kind: LogFileKind,
    active: Option<ActiveLog>,
    size_bytes: u64,
    timestamp: SystemTime,
}

struct InactiveCandidate(DiscoveredLog);

impl PartialEq for InactiveCandidate {
    fn eq(&self, other: &Self) -> bool {
        self.0.timestamp == other.0.timestamp && self.0.path == other.0.path
    }
}

impl Eq for InactiveCandidate {}

impl PartialOrd for InactiveCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for InactiveCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .timestamp
            .cmp(&other.0.timestamp)
            .then_with(|| self.0.path.cmp(&other.0.path))
    }
}

struct Discovery {
    logs: Vec<DiscoveredLog>,
    recognized: usize,
    omitted: usize,
}

struct LogRoots {
    broker: PathBuf,
    broker_archive: PathBuf,
    sessions: PathBuf,
    archived_sessions: PathBuf,
}

#[derive(Debug)]
pub(crate) struct SupportBundleLog {
    pub(crate) path: PathBuf,
    pub(crate) archive_path: String,
    pub(crate) size_bytes: u64,
    pub(crate) modified: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SupportBundleLogError {
    PermissionDenied,
    Unavailable,
}

impl LogRoots {
    fn production() -> Self {
        Self {
            broker: crate::paths::broker_log_path(),
            broker_archive: crate::paths::archive_log_dir(),
            sessions: crate::paths::sessions_log_dir(),
            archived_sessions: crate::paths::archived_sessions_log_dir(),
        }
    }
}

pub(crate) fn run(
    policy: RetentionPolicy,
    agent_lease: &Arc<BrokerAgentLease>,
) -> Result<(), String> {
    let active_agent = agent_lease.active_log()?;
    if active_agent
        .as_ref()
        .is_some_and(|registration| !registration.ready)
    {
        return Ok(());
    }
    let roots = LogRoots::production();
    let now = SystemTime::now();
    let discovery = discover_bounded(
        active_agent.as_ref().map(|active| active.path.as_path()),
        &roots,
        now,
    )?;
    if discovery.omitted != 0 {
        tracing::warn!(
            target: crate::logging::NET,
            recognized = discovery.recognized,
            selected = discovery.logs.len(),
            omitted = discovery.omitted,
            "Windows log maintenance selected the oldest bounded subset; omitted files remain eligible for the next pass"
        );
    }
    let mut acl_reset_failures = 0usize;
    let mut first_acl_reset_error = None;
    for entry in &discovery.logs {
        if entry.active.is_none() && entry.kind == LogFileKind::Session {
            if let Err(error) = crate::windows_session::reset_session_log_access(&entry.path) {
                acl_reset_failures += 1;
                first_acl_reset_error.get_or_insert(error);
            }
        }
    }
    if acl_reset_failures != 0 {
        tracing::warn!(
            target: crate::logging::NET,
            failures = acl_reset_failures,
            first_error = first_acl_reset_error.as_deref().unwrap_or("none"),
            "inactive session-log ACL reconciliation was incomplete; retention continues"
        );
    }
    // A prior broker crash between granting and revoking session-log access
    // (see `grant_session_log_access`) can leave a stray per-user traversal
    // grant on the sessions directory itself. Only reconcile it while no
    // session is being registered or is active, so a concurrently launching
    // agent's own directory grant is never clobbered mid-setup.
    if active_agent.is_none() {
        if let Err(error) = crate::windows_session::reset_session_log_access(&roots.sessions) {
            tracing::warn!(
                target: crate::logging::NET,
                %error,
                "sessions directory ACL reconciliation was incomplete; retention continues"
            );
        }
    }
    let records = records_for(&discovery.logs, now);
    let plan = plan_log_maintenance(&policy, &records).map_err(|error| error.to_string())?;
    let delete_error = delete_planned(&discovery.logs, &plan.delete).err();
    let mut archive_failures = 0usize;
    let mut first_archive_error = None;
    for id in plan.archive {
        let entry = &discovery.logs[id.value() as usize];
        let result = match entry.active {
            Some(ActiveLog::Broker) => rotate_broker(entry, &roots),
            Some(ActiveLog::Agent) => {
                let user_sid = &active_agent
                    .as_ref()
                    .ok_or_else(|| "active agent log registration disappeared".to_string())?
                    .user_sid;
                rotate_agent(entry, user_sid, &roots).map(|()| {
                    agent_lease.request_log_reopen();
                })
            }
            None => Ok(()),
        };
        if let Err(error) = result {
            archive_failures += 1;
            first_archive_error.get_or_insert(error);
        }
    }
    match (delete_error, first_archive_error) {
        (None, None) => Ok(()),
        (delete, archive) => Err(format!(
            "log maintenance completed with failures: delete={}; archive_failures={archive_failures}; first_archive={}",
            delete.as_deref().unwrap_or("none"),
            archive.as_deref().unwrap_or("none")
        )),
    }
}

fn discover_bounded(
    active_agent: Option<&Path>,
    roots: &LogRoots,
    now: SystemTime,
) -> Result<Discovery, String> {
    let mut fixed = Vec::new();
    let mut candidates = BinaryHeap::new();
    let mut recognized = 0usize;
    if let Some(metadata) = regular_non_reparse_metadata(&roots.broker)? {
        recognized += 1;
        fixed.push(DiscoveredLog {
            path: roots.broker.clone(),
            kind: LogFileKind::Active,
            active: Some(ActiveLog::Broker),
            size_bytes: metadata.len(),
            timestamp: log_timestamp(&metadata, now),
        });
    }

    collect_recognized(
        &roots.broker_archive,
        is_broker_archive_name,
        LogFileKind::Archive,
        active_agent,
        now,
        &mut fixed,
        &mut candidates,
        &mut recognized,
    )?;
    collect_recognized(
        &roots.sessions,
        is_session_log_name,
        LogFileKind::Session,
        active_agent,
        now,
        &mut fixed,
        &mut candidates,
        &mut recognized,
    )?;
    collect_recognized(
        &roots.archived_sessions,
        is_session_log_name,
        LogFileKind::Session,
        active_agent,
        now,
        &mut fixed,
        &mut candidates,
        &mut recognized,
    )?;
    let logs = finalize_bounded_logs(fixed, candidates, arcen_telemetry::MAX_LOG_FILE_RECORDS);
    let omitted = recognized.saturating_sub(logs.len());
    Ok(Discovery {
        logs,
        recognized,
        omitted,
    })
}

pub(crate) fn support_bundle_logs() -> Result<(Vec<SupportBundleLog>, bool), SupportBundleLogError>
{
    const MAX_SUPPORT_BUNDLE_LOGS: usize = arcen_telemetry::MAX_BUNDLE_ENTRIES - 32;

    let roots = LogRoots::production();
    let log_root = crate::paths::logs_dir();
    let root_metadata =
        std::fs::symlink_metadata(&log_root).map_err(|error| classify_support_log_error(&error))?;
    if !root_metadata.is_dir()
        || root_metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err(SupportBundleLogError::Unavailable);
    }
    let mut logs = Vec::new();
    let mut truncated = false;
    collect_support_log(
        &roots.broker,
        &log_root,
        is_active_broker_name,
        &mut logs,
        &mut truncated,
    )?;
    let mut inspected = 0usize;
    'directories: for (directory, recognize) in [
        (
            roots.broker_archive.as_path(),
            is_broker_archive_name as fn(&str) -> bool,
        ),
        (roots.sessions.as_path(), is_session_log_name),
        (roots.archived_sessions.as_path(), is_session_log_name),
    ] {
        let entries = match std::fs::read_dir(directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(classify_support_log_error(&error)),
        };
        for entry in entries {
            if inspected >= arcen_telemetry::MAX_LOG_FILE_RECORDS
                || logs.len() >= MAX_SUPPORT_BUNDLE_LOGS
            {
                truncated = true;
                break 'directories;
            }
            inspected += 1;
            let entry = entry.map_err(|error| classify_support_log_error(&error))?;
            collect_support_log(
                &entry.path(),
                &log_root,
                recognize,
                &mut logs,
                &mut truncated,
            )?;
        }
    }
    logs.sort_by(|left, right| {
        right
            .modified
            .cmp(&left.modified)
            .then_with(|| left.archive_path.cmp(&right.archive_path))
    });
    Ok((logs, truncated))
}

fn collect_support_log(
    path: &Path,
    log_root: &Path,
    recognize: fn(&str) -> bool,
    logs: &mut Vec<SupportBundleLog>,
    truncated: &mut bool,
) -> Result<(), SupportBundleLogError> {
    if logs.len() >= arcen_telemetry::MAX_BUNDLE_ENTRIES - 32 {
        *truncated = true;
        return Ok(());
    }
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return Ok(());
    };
    if !recognize(name) {
        return Ok(());
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.is_file()
                && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 =>
        {
            metadata
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(classify_support_log_error(&error)),
    };
    let relative = path
        .strip_prefix(log_root)
        .map_err(|_| SupportBundleLogError::Unavailable)?;
    let relative = relative
        .components()
        .map(|component| component.as_os_str().to_str())
        .collect::<Option<Vec<_>>>()
        .ok_or(SupportBundleLogError::Unavailable)?
        .join("/");
    logs.push(SupportBundleLog {
        path: path.to_path_buf(),
        archive_path: format!("logs/{relative}"),
        size_bytes: metadata.len(),
        modified: metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
    });
    Ok(())
}

fn classify_support_log_error(error: &std::io::Error) -> SupportBundleLogError {
    if error.kind() == std::io::ErrorKind::PermissionDenied {
        SupportBundleLogError::PermissionDenied
    } else {
        SupportBundleLogError::Unavailable
    }
}

fn finalize_bounded_logs(
    fixed: Vec<DiscoveredLog>,
    mut candidates: BinaryHeap<InactiveCandidate>,
    limit: usize,
) -> Vec<DiscoveredLog> {
    let capacity = limit.saturating_sub(fixed.len());
    while candidates.len() > capacity {
        candidates.pop();
    }
    let mut logs = fixed;
    logs.extend(candidates.into_iter().map(|candidate| candidate.0));
    logs.sort_by(|left, right| left.path.cmp(&right.path));
    logs
}

fn collect_recognized(
    directory: &Path,
    recognize: fn(&str) -> bool,
    kind: LogFileKind,
    active_agent: Option<&Path>,
    now: SystemTime,
    fixed: &mut Vec<DiscoveredLog>,
    candidates: &mut BinaryHeap<InactiveCandidate>,
    recognized: &mut usize,
) -> Result<(), String> {
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("enumerate {}: {error}", directory.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("enumerate {}: {error}", directory.display()))?;
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if !recognize(name) {
            continue;
        }
        let Some(metadata) = regular_non_reparse_metadata(&path)? else {
            continue;
        };
        *recognized += 1;
        let active = active_agent
            .filter(|active| *active == path)
            .map(|_| ActiveLog::Agent);
        let timestamp = log_timestamp(&metadata, now);
        let log = DiscoveredLog {
            path,
            kind: if active.is_some() {
                LogFileKind::Active
            } else {
                kind
            },
            active,
            size_bytes: metadata.len(),
            timestamp,
        };
        if log.active.is_some() {
            fixed.push(log);
            continue;
        }
        candidates.push(InactiveCandidate(log));
        if candidates.len() > arcen_telemetry::MAX_LOG_FILE_RECORDS {
            candidates.pop();
        }
    }
    Ok(())
}

fn records_for(logs: &[DiscoveredLog], now: SystemTime) -> Vec<LogFileRecord> {
    logs.iter()
        .enumerate()
        .map(|(index, entry)| LogFileRecord {
            id: LogFileId::new(index as u32),
            kind: entry.kind,
            size_bytes: entry.size_bytes,
            age_seconds: now
                .duration_since(entry.timestamp)
                .unwrap_or_default()
                .as_secs(),
        })
        .collect()
}

fn delete_planned(logs: &[DiscoveredLog], delete: &[LogFileId]) -> Result<(), String> {
    let mut deleted = 0usize;
    let mut failures = 0usize;
    let mut first_error = None;
    for id in delete {
        let path = &logs[id.value() as usize].path;
        match std::fs::remove_file(path) {
            Ok(()) => deleted += 1,
            Err(error) => {
                failures += 1;
                first_error.get_or_insert_with(|| {
                    format!("delete expired log {}: {error}", path.display())
                });
            }
        }
    }
    if failures != 0 {
        return Err(format!(
            "{failures} delete failures after {deleted} successful deletions; first: {}",
            first_error.as_deref().unwrap_or("unknown")
        ));
    }
    Ok(())
}

fn regular_non_reparse_metadata(path: &Path) -> Result<Option<Metadata>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("inspect {}: {error}", path.display())),
    };
    if metadata.is_file() && metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 == 0 {
        Ok(Some(metadata))
    } else {
        Ok(None)
    }
}

fn log_timestamp(metadata: &Metadata, now: SystemTime) -> SystemTime {
    metadata
        .created()
        .or_else(|_| metadata.modified())
        .unwrap_or(now)
}

fn rotate_broker(entry: &DiscoveredLog, roots: &LogRoots) -> Result<(), String> {
    let Some(log_file) = crate::service::service_log() else {
        return Ok(());
    };
    let archive = unique_archive_path(
        &roots.broker_archive,
        "arcen-pier",
        current_epoch_seconds()?,
    )?;
    std::fs::create_dir_all(&roots.broker_archive)
        .map_err(|error| format!("create broker archive directory: {error}"))?;
    std::fs::rename(&entry.path, &archive).map_err(|error| {
        format!(
            "archive active broker log {} to {}: {error}",
            entry.path.display(),
            archive.display()
        )
    })?;
    if let Err(error) = log_file.reopen() {
        let _ = std::fs::remove_file(&entry.path);
        let rollback = std::fs::rename(&archive, &entry.path);
        return match rollback {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!(
                "{error}; restore broker log name also failed: {rollback_error}"
            )),
        };
    }
    tracing::info!(
        target: crate::logging::NET,
        archive = %archive.display(),
        "broker log archived and reopened"
    );
    Ok(())
}

fn rotate_agent(entry: &DiscoveredLog, user_sid: &str, roots: &LogRoots) -> Result<(), String> {
    let stem = entry
        .path
        .file_stem()
        .and_then(|value| value.to_str())
        .ok_or_else(|| format!("invalid agent log name {}", entry.path.display()))?;
    let directory = roots.archived_sessions.clone();
    std::fs::create_dir_all(&directory)
        .map_err(|error| format!("create {}: {error}", directory.display()))?;
    let archive = unique_archive_path(&directory, stem, current_epoch_seconds()?)?;
    std::fs::rename(&entry.path, &archive).map_err(|error| {
        format!(
            "archive active agent log {} to {}: {error}",
            entry.path.display(),
            archive.display()
        )
    })?;
    let replacement = match crate::service::open_shared_append(&entry.path) {
        Ok(replacement) => replacement,
        Err(error) => return Err(rollback_agent_rotation(&entry.path, &archive, error)),
    };
    drop(replacement);
    if let Err(error) = crate::windows_session::grant_session_log_access(&entry.path, user_sid) {
        return Err(rollback_agent_rotation(&entry.path, &archive, error));
    }
    if let Err(error) = crate::windows_session::revoke_session_log_access(&archive, user_sid) {
        let _ = crate::windows_session::revoke_session_log_access(&entry.path, user_sid);
        return Err(rollback_agent_rotation(&entry.path, &archive, error));
    }
    tracing::info!(
        target: crate::logging::SESSION,
        archive = %archive.display(),
        "active agent log archived; reopen requested"
    );
    Ok(())
}

fn rollback_agent_rotation(active: &Path, archive: &Path, cause: String) -> String {
    let remove_error = match std::fs::remove_file(active) {
        Ok(()) => None,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => Some(error),
    };
    let restore_error = std::fs::rename(archive, active).err();
    match (remove_error, restore_error) {
        (None, None) => cause,
        (remove, restore) => format!(
            "{cause}; rollback remove error: {}; rollback restore error: {}",
            remove.map_or_else(|| "none".to_string(), |error| error.to_string()),
            restore.map_or_else(|| "none".to_string(), |error| error.to_string())
        ),
    }
}

fn unique_archive_path(directory: &Path, stem: &str, epoch: u64) -> Result<PathBuf, String> {
    for suffix in 0_u16..=999 {
        let suffix = if suffix == 0 {
            String::new()
        } else {
            format!("-{suffix}")
        };
        let candidate = directory.join(format!("{stem}-{epoch}{suffix}.log"));
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!(
        "archive collision limit reached for {stem}-{epoch} in {}",
        directory.display()
    ))
}

fn current_epoch_seconds() -> Result<u64, String> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|error| format!("system clock precedes Unix epoch: {error}"))
}

fn is_broker_archive_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("arcen-pier-")
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    is_epoch_with_optional_suffix(stem)
}

fn is_active_broker_name(name: &str) -> bool {
    name == "arcen-pier.log"
}

fn is_session_log_name(name: &str) -> bool {
    let Some(stem) = name
        .strip_prefix("arcen-session-agent-")
        .and_then(|name| name.strip_suffix(".log"))
    else {
        return false;
    };
    if stem.len() < 36
        || arcen_telemetry::CorrelationId::parse_uuid(stem[..36].to_string()).is_err()
    {
        return false;
    }
    let remainder = &stem[36..];
    remainder.is_empty()
        || remainder
            .strip_prefix('-')
            .is_some_and(is_epoch_with_optional_suffix)
}

fn is_epoch_with_optional_suffix(value: &str) -> bool {
    let mut pieces = value.split('-');
    let Some(epoch) = pieces.next() else {
        return false;
    };
    if epoch.is_empty() || !epoch.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match (pieces.next(), pieces.next()) {
        (None, None) => true,
        (Some(suffix), None) => {
            !suffix.is_empty()
                && suffix.bytes().all(|byte| byte.is_ascii_digit())
                && suffix.parse::<u16>().is_ok_and(|suffix| suffix <= 999)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn only_owned_log_names_are_recognized() {
        assert!(is_broker_archive_name("arcen-pier-1720000000.log"));
        assert!(!is_broker_archive_name("arcen-pier.log"));
        assert!(is_session_log_name(
            "arcen-session-agent-01234567-89ab-cdef-8123-456789abcdef.log"
        ));
        assert!(is_session_log_name(
            "arcen-session-agent-01234567-89ab-cdef-8123-456789abcdef-1720000000-1.log"
        ));
        assert!(!is_session_log_name("arcen-session-agent-private.log"));
        assert!(!is_broker_archive_name("arcen-pier-private.log"));
        assert!(!is_session_log_name("Xorg.log"));
        assert!(!is_session_log_name("customer.log"));
    }

    #[test]
    fn archive_names_have_a_bounded_collision_suffix() {
        let directory =
            std::env::temp_dir().join(format!("arcen-log-archive-test-{}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("archive test directory");
        std::fs::write(directory.join("arcen-pier-42.log"), b"first").expect("collision fixture");
        assert_eq!(
            unique_archive_path(&directory, "arcen-pier", 42)
                .expect("unique path")
                .file_name()
                .and_then(|value| value.to_str()),
            Some("arcen-pier-42-1.log")
        );
        let _ = std::fs::remove_dir_all(directory);
    }

    #[test]
    fn overflow_maintenance_pass_deletes_expired_logs_and_recovers() {
        let root =
            std::env::temp_dir().join(format!("arcen-log-overflow-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let roots = LogRoots {
            broker: root.join("arcen-pier.log"),
            broker_archive: root.join("archive"),
            sessions: root.join("sessions"),
            archived_sessions: root.join("archive/sessions"),
        };
        std::fs::create_dir_all(&roots.sessions).expect("session log directory");
        let active = roots
            .sessions
            .join("arcen-session-agent-01234567-89ab-cdef-8123-456789abcdef.log");
        std::fs::write(&active, b"active").expect("active log");
        for index in 0..=arcen_telemetry::MAX_LOG_FILE_RECORDS {
            std::fs::write(
                roots.sessions.join(format!(
                    "arcen-session-agent-00000000-0000-4000-8000-{index:012x}.log"
                )),
                b"expired",
            )
            .expect("expired session log");
        }
        let foreign = roots.sessions.join("Xorg.log");
        std::fs::write(&foreign, b"foreign").expect("foreign log");
        let non_regular = roots.sessions.join("arcen-session-agent-directory.log");
        std::fs::create_dir(&non_regular).expect("recognized-name directory");

        let policy = RetentionPolicy::default();
        let future = SystemTime::now()
            + Duration::from_secs((u64::from(policy.retention_days()) + 1) * 24 * 60 * 60);
        let discovery =
            discover_bounded(Some(&active), &roots, future).expect("overflow maintenance pass");

        assert_eq!(
            discovery.recognized,
            arcen_telemetry::MAX_LOG_FILE_RECORDS + 2
        );
        assert_eq!(discovery.logs.len(), arcen_telemetry::MAX_LOG_FILE_RECORDS);
        assert_eq!(discovery.omitted, 2);
        assert!(discovery.logs.iter().any(|entry| entry.path == active));
        let plan = plan_log_maintenance(&policy, &records_for(&discovery.logs, future))
            .expect("bounded overflow plan");
        assert_eq!(plan.delete.len(), arcen_telemetry::MAX_LOG_FILE_RECORDS - 1);
        delete_planned(&discovery.logs, &plan.delete).expect("first bounded deletion pass");
        assert!(active.exists());
        assert!(foreign.exists());
        assert!(non_regular.is_dir());

        let remainder =
            discover_bounded(Some(&active), &roots, future).expect("remainder maintenance pass");
        assert_eq!(remainder.recognized, 3);
        assert_eq!(remainder.omitted, 0);
        let plan = plan_log_maintenance(&policy, &records_for(&remainder.logs, future))
            .expect("remainder plan");
        assert_eq!(plan.delete.len(), 2);
        delete_planned(&remainder.logs, &plan.delete).expect("remainder deletion pass");

        let recovered =
            discover_bounded(Some(&active), &roots, future).expect("recovered maintenance pass");
        assert_eq!(recovered.recognized, 1);
        assert_eq!(recovered.logs.len(), 1);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bounded_selection_keeps_oldest_candidates_deterministically() {
        let mut candidates = BinaryHeap::new();
        for (name, timestamp) in [("d", 4), ("b", 2), ("e", 5), ("a", 1), ("c", 3)] {
            candidates.push(InactiveCandidate(DiscoveredLog {
                path: PathBuf::from(name),
                kind: LogFileKind::Session,
                active: None,
                size_bytes: 1,
                timestamp: UNIX_EPOCH + Duration::from_secs(timestamp),
            }));
        }
        let selected = finalize_bounded_logs(Vec::new(), candidates, 3);
        assert_eq!(
            selected
                .iter()
                .map(|entry| entry.path.as_path())
                .collect::<Vec<_>>(),
            [Path::new("a"), Path::new("b"), Path::new("c")]
        );
    }
}
