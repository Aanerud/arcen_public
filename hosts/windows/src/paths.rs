use std::path::PathBuf;

pub(crate) fn arcen_data_root() -> PathBuf {
    std::env::var_os("ProgramData")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"))
        .join("Arcen")
}

pub(crate) fn config_path() -> PathBuf {
    arcen_data_root().join("pier.json")
}

pub(crate) fn logs_dir() -> PathBuf {
    arcen_data_root().join("logs")
}

pub(crate) fn broker_log_path() -> PathBuf {
    logs_dir().join("arcen-pier.log")
}

pub(crate) fn archive_log_dir() -> PathBuf {
    logs_dir().join("archive")
}

pub(crate) fn sessions_log_dir() -> PathBuf {
    logs_dir().join("sessions")
}

pub(crate) fn archived_sessions_log_dir() -> PathBuf {
    archive_log_dir().join("sessions")
}

pub(crate) fn support_dir() -> PathBuf {
    arcen_data_root().join("Support")
}

pub(crate) fn recovery_dir() -> PathBuf {
    arcen_data_root().join("recovery")
}

/// State written by the per-session agent under the signed-in user's
/// unelevated token.
///
/// The Arcen data root carries a protected DACL with exactly SYSTEM and
/// Administrators. Anything the agent must create therefore cannot live in the
/// root, or in `recovery/`, which only the service writes.
pub(crate) fn agent_runtime_dir() -> PathBuf {
    arcen_data_root().join("runtime")
}
