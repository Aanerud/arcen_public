//! The host's Wayland qualification record on disk, and the running stack
//! it is validated against.
//!
//! The record is policy-free JSON of
//! [`arcen_session::wayland_qualification::WaylandQualificationRecord`],
//! written only by `arcen-pier wayland qualify --write` (root) and read at
//! Pier start. It must be a regular root-owned file in a root-owned
//! directory that neither group nor others can write; anything else is
//! refused rather than trusted.

use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use arcen_session::wayland_qualification::{
    check_schema, parse_compositor_version, parse_os_release, parse_pipewire_version, GpuDriver,
    HostIdentity, OsRelease, PierBuild, QualificationError, QualifyOutput, RequestedSpeedOutputs,
    SchemaProbe, StackVersions, WaylandQualificationRecord, REQUESTED_SCHEMA_VERSION,
};
use arcen_session::wayland_session::HeadlessCompositor;
use tokio::process::Command;

use super::compositor_binary;

/// Where the record lives: the Pier's root-owned state directory.
pub const RECORD_PATH: &str = arcen_session::wayland_qualification::RECORD_PATH;

const MAX_RECORD_BYTES: u64 = 256 * 1024;
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const PIPEWIRE: &str = "/usr/bin/pipewire";
const NVIDIA_VERSION: &str = "/sys/module/nvidia/version";
const NVIDIA_DRM: &str = "/sys/module/nvidia_drm";
const NVIDIA_DRM_MODESET: &str = "/sys/module/nvidia_drm/parameters/modeset";
const OS_RELEASE: &str = "/etc/os-release";
const KERNEL_RELEASE: &str = "/proc/sys/kernel/osrelease";

/// The running Pier build.
///
/// # Errors
///
/// When the executable cannot be hashed: without its digest no record can
/// be bound to this build.
pub fn pier_build() -> Result<PierBuild, String> {
    let identity = crate::build_identity();
    let executable_sha256 = identity
        .artifact_sha256
        .ok_or_else(|| "the Pier executable could not be read to hash it".to_owned())?;
    Ok(PierBuild {
        version: identity.version,
        revision: identity.source_revision,
        executable_sha256,
    })
}

async fn version_output(binary: &str) -> Result<String, String> {
    let mut command = Command::new(binary);
    super::unprivileged_probe(&mut command, binary);
    let output = command
        .arg("--version")
        .env_remove("DISPLAY")
        .env_remove("WAYLAND_DISPLAY")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(VERSION_TIMEOUT, output)
        .await
        .map_err(|_| format!("`{binary} --version` timed out"))?
        .map_err(|error| format!("`{binary} --version` failed: {error}"))?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A kernel module boolean parameter (`Y`/`N`, `1`/`0`).
fn parse_kernel_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "Y" | "y" | "1" => Some(true),
        "N" | "n" | "0" => Some(false),
        _ => None,
    }
}

/// The loaded encoder GPU driver: the NVIDIA kernel module's version,
/// whether its DRM module is loaded and that module's `modeset`. `None`
/// when no NVIDIA driver is loaded.
#[must_use]
pub fn gpu_driver() -> Option<GpuDriver> {
    let version = std::fs::read_to_string(NVIDIA_VERSION).ok()?;
    let version = version.trim();
    let drm = Path::new(NVIDIA_DRM).is_dir();
    (!version.is_empty()).then(|| GpuDriver {
        vendor: "nvidia".to_owned(),
        version: version.to_owned(),
        drm,
        modeset: drm
            .then(|| std::fs::read_to_string(NVIDIA_DRM_MODESET).ok())
            .flatten()
            .as_deref()
            .and_then(parse_kernel_bool),
    })
}

/// The installed compositor stack.
///
/// # Errors
///
/// When the compositor or `PipeWire` does not report a version.
pub async fn stack_versions(compositor: HeadlessCompositor) -> Result<StackVersions, String> {
    let compositor_binary = compositor_binary(compositor);
    let compositor_version =
        parse_compositor_version(&version_output(&compositor_binary).await?)
            .ok_or_else(|| format!("`{compositor_binary} --version` names no version"))?;
    let pipewire_version = parse_pipewire_version(&version_output(PIPEWIRE).await?)
        .ok_or_else(|| format!("`{PIPEWIRE} --version` names no library version"))?;
    Ok(StackVersions {
        compositor,
        compositor_version,
        compositor_prefix: super::compositor_prefix().recorded(),
        pipewire_version,
        gpu_driver: gpu_driver(),
    })
}

/// Everything the record's validity depends on, for the running host.
///
/// # Errors
///
/// See [`pier_build`] and [`stack_versions`].
pub async fn host_identity(compositor: HeadlessCompositor) -> Result<HostIdentity, String> {
    Ok(HostIdentity {
        pier: pier_build()?,
        stack: stack_versions(compositor).await?,
    })
}

/// The operating system, for operators.
#[must_use]
pub fn os_release() -> OsRelease {
    let (id, version_id) =
        parse_os_release(&std::fs::read_to_string(OS_RELEASE).unwrap_or_default());
    OsRelease {
        id,
        version_id,
        kernel: std::fs::read_to_string(KERNEL_RELEASE)
            .unwrap_or_default()
            .trim()
            .to_owned(),
    }
}

fn root_only(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == 0 && metadata.mode() & 0o022 == 0
}

/// Reads the record at `path`, refusing anything root alone could not have
/// written.
///
/// # Errors
///
/// [`QualificationError`]: missing, unreadable or not root-only, malformed,
/// or of another schema.
pub fn load_record(path: &Path) -> Result<WaylandQualificationRecord, QualificationError> {
    let shown = path.display().to_string();
    match read_root_only(path) {
        Ok(Some(text)) => parse_record(&text),
        Ok(None) => Err(QualificationError::Missing { path: shown }),
        Err(reason) => Err(QualificationError::Unreadable {
            path: shown,
            reason,
        }),
    }
}

/// Reads a small root-owned state file (`None` when it does not exist),
/// refusing anything root alone could not have written.
///
/// # Errors
///
/// Why the file cannot be trusted or read.
pub fn read_root_only(path: &Path) -> Result<Option<String>, String> {
    let unreadable = |reason: String| reason;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(error) => return Err(unreadable(error.to_string())),
    };
    if !metadata.file_type().is_file() {
        return Err(unreadable("not a regular file".to_owned()));
    }
    if !root_only(&metadata) {
        return Err(unreadable(
            "it must be owned by root and writable by root alone".to_owned(),
        ));
    }
    let parent = path
        .parent()
        .ok_or_else(|| unreadable("no parent directory".to_owned()))?;
    let parent_metadata =
        std::fs::metadata(parent).map_err(|error| unreadable(error.to_string()))?;
    if !root_only(&parent_metadata) {
        return Err(unreadable(format!(
            "its directory {} must be owned by root and writable by root alone",
            parent.display()
        )));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc_o_nofollow())
        .open(path)
        .map_err(|error| unreadable(error.to_string()))?;
    let mut text = String::new();
    file.take(MAX_RECORD_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| unreadable(error.to_string()))?;
    if text.len() as u64 > MAX_RECORD_BYTES {
        return Err(unreadable("larger than any record".to_owned()));
    }
    Ok(Some(text))
}

/// The displays a previous record (of any schema) measured that are worth
/// measuring again (`arcen_session::wayland_qualification::
/// outputs_worth_remeasuring`): only its sizes and rates are read, never as
/// evidence. Schema-2 records named only the output refresh (a 120 Hz output
/// there was a 60 Hz display's paced one), no codec (H.264) and no route
/// (the record's capture route).
#[must_use]
pub fn previous_outputs(path: &Path) -> Vec<QualifyOutput> {
    let Ok(Some(text)) = read_root_only(path) else {
        return Vec::new();
    };
    previous_outputs_in(&text)
}

/// [`previous_outputs`] of a record's text.
#[must_use]
pub fn previous_outputs_in(text: &str) -> Vec<QualifyOutput> {
    use arcen_session::wayland_qualification::{
        outputs_worth_remeasuring, speed_minimum_fps, CaptureRoute, RateEvidence,
        SourceRateMeasurement,
    };
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let number = |rate: &serde_json::Value, key: &str| {
        rate.get(key)
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u32::try_from(value).ok())
    };
    let route_of = |value: Option<&serde_json::Value>| {
        value
            .cloned()
            .and_then(|route| serde_json::from_value::<CaptureRoute>(route).ok())
    };
    let record_route =
        route_of(value.pointer("/facts/capture_route/route")).unwrap_or(CaptureRoute::HostCopy);
    let rates = value
        .pointer("/facts/source_rates")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|rate| {
            let refresh_hz = number(rate, "refresh_hz")?;
            let display_refresh_hz = number(rate, "display_refresh_hz")
                .unwrap_or(if refresh_hz == 120 { 60 } else { refresh_hz });
            let output = QualifyOutput::new(
                number(rate, "width")?,
                number(rate, "height")?,
                display_refresh_hz,
            )?;
            Some(SourceRateMeasurement {
                width: output.width,
                height: output.height,
                display_refresh_hz,
                refresh_hz,
                codec: rate
                    .get("codec")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("h264")
                    .to_owned(),
                route: route_of(rate.get("route")).unwrap_or(record_route),
                sustained_source_fps: number(rate, "sustained_source_fps"),
                seconds: number(rate, "seconds").unwrap_or(0),
                evidence: RateEvidence::Measured,
            })
        })
        .collect::<Vec<_>>();
    outputs_worth_remeasuring(
        &rates,
        speed_minimum_fps(
            arcen_media::video::pipeline_contract(arcen_media::video::PipelineId::Speed).max_fps,
        ),
    )
}

/// The displays Decks asked Speed for without evidence; empty when the
/// file is missing or cannot be trusted.
#[must_use]
pub fn load_requested_outputs(path: &Path) -> RequestedSpeedOutputs {
    read_root_only(path)
        .ok()
        .flatten()
        .and_then(|text| serde_json::from_str::<RequestedSpeedOutputs>(&text).ok())
        .filter(|list| list.schema_version == REQUESTED_SCHEMA_VERSION)
        .map_or_else(RequestedSpeedOutputs::new, RequestedSpeedOutputs::validated)
}

/// Writes the automatic re-qualification marker like a record.
///
/// # Errors
///
/// Any I/O failure.
pub fn write_requalify_marker(
    path: &Path,
    marker: &arcen_session::wayland_qualification::RequalifyAttempt,
) -> std::io::Result<()> {
    let mut json = serde_json::to_vec_pretty(marker).map_err(std::io::Error::other)?;
    json.push(b'\n');
    write_root_only(path, &json)
}

/// Serialises this process's read-modify-write of the requested-displays
/// list (the Pier notes requests from many sessions; an automatic
/// re-qualification edits it too).
static REQUESTED_UPDATE: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Loads the requested-displays list, lets `change` edit it, and writes it
/// back when `change` says it changed — under one process-wide lock, on the
/// list as it is now, so concurrent edits are not lost.
///
/// # Errors
///
/// The write's I/O failure.
pub fn update_requested_outputs(
    path: &Path,
    change: impl FnOnce(&mut RequestedSpeedOutputs) -> bool,
) -> std::io::Result<RequestedSpeedOutputs> {
    let _guard = REQUESTED_UPDATE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut list = load_requested_outputs(path);
    if change(&mut list) {
        write_requested_outputs(path, &list)?;
    }
    Ok(list)
}

/// Writes the requested-displays list like a record (atomic, root 0644).
///
/// # Errors
///
/// Any I/O failure.
pub fn write_requested_outputs(path: &Path, list: &RequestedSpeedOutputs) -> std::io::Result<()> {
    let mut json = serde_json::to_vec_pretty(list).map_err(std::io::Error::other)?;
    json.push(b'\n');
    write_root_only(path, &json)
}

/// Parses a record, reporting another schema as such.
///
/// # Errors
///
/// [`QualificationError::Malformed`] or
/// [`QualificationError::UnsupportedSchema`].
pub fn parse_record(text: &str) -> Result<WaylandQualificationRecord, QualificationError> {
    let probe: SchemaProbe =
        serde_json::from_str(text).map_err(|error| QualificationError::Malformed {
            reason: error.to_string(),
        })?;
    check_schema(probe)?;
    serde_json::from_str(text).map_err(|error| QualificationError::Malformed {
        reason: error.to_string(),
    })
}

fn libc_o_nofollow() -> i32 {
    nix::fcntl::OFlag::O_NOFOLLOW.bits()
}

/// Writes `record` to `path` atomically: a new 0644 root file beside it,
/// synced, renamed over the old one, and the directory synced. Creates the
/// directory (0755) when it does not exist.
///
/// # Errors
///
/// Any I/O failure; the previous record is left in place.
pub fn write_record(path: &Path, record: &WaylandQualificationRecord) -> std::io::Result<()> {
    let mut json = serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?;
    json.push(b'\n');
    write_root_only(path, &json)
}

/// Writes `json` to `path` atomically as a root 0644 file in a root-only
/// directory (created 0755 when missing).
fn write_root_only(path: &Path, json: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("record path has no directory"))?;
    if !parent.exists() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o755)
            .create(parent)?;
    }
    let parent_metadata = std::fs::metadata(parent)?;
    if !root_only(&parent_metadata) {
        return Err(std::io::Error::other(format!(
            "{} must be owned by root and writable by root alone",
            parent.display()
        )));
    }
    // Unique per write, so two writers in one process never share a
    // temporary file.
    static WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let temporary = parent.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id(),
        WRITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc_o_nofollow())
            .open(&temporary)?;
        file.write_all(json)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn another_schema_is_reported_as_such_not_as_malformed() {
        assert!(matches!(
            parse_record(r#"{"schema_version":7,"facts":"from the future"}"#),
            Err(QualificationError::UnsupportedSchema { found: 7 })
        ));
        assert!(matches!(
            parse_record(r#"{"schema_version":3}"#),
            Err(QualificationError::Malformed { .. })
        ));
        assert!(matches!(
            parse_record("not json"),
            Err(QualificationError::Malformed { .. })
        ));
    }

    #[test]
    fn a_re_qualification_keeps_the_displays_of_any_earlier_record_worth_measuring() {
        // The 0.17.0 record (schema 2) named only the 120 Hz paced output of
        // a 60 Hz display, and no codec; a size that never held Speed is not
        // carried.
        let schema_2 = r#"{"schema_version":2,"facts":{"capture_route":{"route":"gpu-convert"},"source_rates":[
            {"width":1920,"height":1080,"refresh_hz":120,"sustained_source_fps":60,"seconds":8},
            {"width":1800,"height":1130,"refresh_hz":120,"sustained_source_fps":null,"seconds":8},
            {"width":2560,"height":1440,"refresh_hz":144,"sustained_source_fps":60,"seconds":8}]}}"#;
        let o = |w, h, r| QualifyOutput::new(w, h, r).unwrap();
        assert_eq!(
            previous_outputs_in(schema_2),
            vec![o(1920, 1080, 60), o(2560, 1440, 144)]
        );
        // Schema 3: a display covered by a larger qualifying one of the same
        // refresh, codec and route is served by inference, not re-measured.
        let schema_3 = r#"{"schema_version":3,"facts":{"source_rates":[
            {"width":3024,"height":1964,"display_refresh_hz":120,"refresh_hz":120,"codec":"h265","route":"gpu-convert","sustained_source_fps":60},
            {"width":1800,"height":1130,"display_refresh_hz":120,"refresh_hz":120,"codec":"h265","route":"gpu-convert","sustained_source_fps":60},
            {"width":0,"height":1964,"display_refresh_hz":120,"refresh_hz":120,"codec":"h264","sustained_source_fps":60}]}}"#;
        assert_eq!(previous_outputs_in(schema_3), vec![o(3024, 1964, 120)]);
        assert!(previous_outputs_in("not json").is_empty());
    }

    #[test]
    fn concurrent_requested_list_updates_are_not_lost() {
        // The list must live in a root-owned directory; only root can run it.
        if !nix::unistd::Uid::effective().is_root() {
            return;
        }
        use std::os::unix::fs::DirBuilderExt;
        let directory =
            std::env::temp_dir().join(format!("arcen-requested-{}", std::process::id()));
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&directory)
            .unwrap();
        let path = directory.join("q-speed-requests.json");
        let threads = (0..2_u32)
            .map(|thread| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for index in 0..7_u32 {
                        let output =
                            QualifyOutput::new(1000 + thread * 100 + index * 2, 700, 60).unwrap();
                        update_requested_outputs(&path, |list| list.note(output, "h265", 1))
                            .unwrap();
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().unwrap();
        }
        assert_eq!(load_requested_outputs(&path).entries.len(), 14);
        std::fs::remove_dir_all(&directory).unwrap();
    }

    #[test]
    fn kernel_booleans_are_read_strictly() {
        assert_eq!(parse_kernel_bool("Y\n"), Some(true));
        assert_eq!(parse_kernel_bool("1"), Some(true));
        assert_eq!(parse_kernel_bool("N\n"), Some(false));
        assert_eq!(parse_kernel_bool("0"), Some(false));
        assert_eq!(parse_kernel_bool(""), None);
        assert_eq!(parse_kernel_bool("maybe"), None);
    }

    #[test]
    fn a_missing_record_is_missing() {
        let path = Path::new("/nonexistent/arcen/wayland-qualification.json");
        assert!(matches!(
            load_record(path),
            Err(QualificationError::Missing { .. })
        ));
    }
}
