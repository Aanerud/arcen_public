use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const JOURNAL_VERSION: u32 = 5;
const MIN_SUPPORTED_JOURNAL_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchdogResource {
    Display,
    NvapiHeadless,
    Timezone,
}

impl WatchdogResource {
    pub(crate) const fn as_arg(self) -> &'static str {
        match self {
            Self::Display => "display",
            Self::NvapiHeadless => "nvapi-headless",
            Self::Timezone => "timezone",
        }
    }

    pub(crate) fn parse(value: &str) -> Result<Self, String> {
        match value {
            "display" => Ok(Self::Display),
            "nvapi-headless" => Ok(Self::NvapiHeadless),
            "timezone" => Ok(Self::Timezone),
            _ => Err("--resource must be display, nvapi-headless, or timezone".to_string()),
        }
    }
}

#[cfg(windows)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WatchdogWait {
    Disarmed,
    ParentExited,
}

#[cfg(windows)]
pub(crate) fn wait_for_watchdog_parent(
    parent_handle_value: isize,
    ready_handle_value: isize,
    path: &Path,
) -> Result<WatchdogWait, String> {
    use windows::Win32::Foundation::{
        CloseHandle, GetHandleInformation, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::System::Threading::{SetEvent, WaitForSingleObject};

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            if !self.0.is_invalid() {
                // SAFETY: this wrapper uniquely owns an inherited valid handle.
                unsafe {
                    let _ = CloseHandle(self.0);
                }
            }
        }
    }

    let parent = OwnedHandle(HANDLE(parent_handle_value as _));
    let ready = OwnedHandle(HANDLE(ready_handle_value as _));
    if parent.0.is_invalid() || ready.0.is_invalid() {
        return Err("watchdog received an invalid inherited handle".to_string());
    }
    let mut parent_flags = 0;
    let mut ready_flags = 0;
    // SAFETY: inherited values are validated before any wait or signal operation.
    unsafe {
        GetHandleInformation(parent.0, &mut parent_flags)
            .map_err(|error| format!("validate inherited parent process handle: {error}"))?;
        GetHandleInformation(ready.0, &mut ready_flags)
            .map_err(|error| format!("validate inherited readiness event handle: {error}"))?;
    }
    // SAFETY: the validated parent handle grants synchronize access.
    if unsafe { WaitForSingleObject(parent.0, 0) } != WAIT_TIMEOUT {
        remove_file(path, "unmutated recovery journal")?;
        return Err(
            "watchdog parent exited before readiness; unmutated journal removed".to_string(),
        );
    }
    // SAFETY: ready is a validated inherited event handle.
    unsafe { SetEvent(ready.0) }
        .map_err(|error| format!("signal recovery watchdog readiness: {error}"))?;
    drop(ready);
    loop {
        if !path.exists() {
            return Ok(WatchdogWait::Disarmed);
        }
        // SAFETY: parent is a validated synchronizable process handle.
        let wait = unsafe { WaitForSingleObject(parent.0, 500) };
        if wait == WAIT_OBJECT_0 {
            return Ok(WatchdogWait::ParentExited);
        }
        if wait == WAIT_FAILED {
            return Err(format!(
                "wait for watchdog parent process: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
}

pub(crate) fn remove_file(path: &Path, description: &str) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove {description} {path:?}: {error}")),
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct DisplayRecoveryJournal {
    pub version: u32,
    #[serde(default)]
    pub mutation_started: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deskside: Option<DesksideRecoveryEntry>,
    pub device_name: String,
    #[serde(default)]
    pub selected_path_index: usize,
    pub original_width: u32,
    pub original_height: u32,
    pub original_refresh_hz: u32,
    pub topology_paths_hex: String,
    pub topology_modes_hex: String,
    pub devmode_hex: String,
    pub nvapi: Option<crate::nvapi::RecoveryData>,
    #[serde(default)]
    pub headless_nvapi_edids: Vec<crate::nvapi_headless::HeadlessEdidRecovery>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stable_topology: Option<StableTopologySnapshot>,
}

impl DisplayRecoveryJournal {
    pub fn new(
        device_name: String,
        original_width: u32,
        original_height: u32,
        original_refresh_hz: u32,
        topology_paths: &[u8],
        topology_modes: &[u8],
        devmode: &[u8],
        nvapi: Option<crate::nvapi::RecoveryData>,
    ) -> Self {
        Self {
            version: JOURNAL_VERSION,
            mutation_started: false,
            deskside: None,
            device_name,
            selected_path_index: 0,
            original_width,
            original_height,
            original_refresh_hz,
            topology_paths_hex: encode_hex(topology_paths),
            topology_modes_hex: encode_hex(topology_modes),
            devmode_hex: encode_hex(devmode),
            nvapi,
            headless_nvapi_edids: Vec::new(),
            stable_topology: None,
        }
    }

    pub fn topology_paths(&self) -> Result<Vec<u8>, String> {
        decode_hex(&self.topology_paths_hex)
    }

    pub fn topology_modes(&self) -> Result<Vec<u8>, String> {
        decode_hex(&self.topology_modes_hex)
    }

    pub fn devmode(&self) -> Result<Vec<u8>, String> {
        decode_hex(&self.devmode_hex)
    }

    pub fn with_deskside(mut self, deskside: Option<DesksideRecoveryEntry>) -> Self {
        self.deskside = deskside;
        self
    }

    pub fn with_stable_topology(mut self, topology: StableTopologySnapshot) -> Self {
        self.stable_topology = Some(topology);
        self
    }

    pub fn with_selected_path_index(mut self, selected_path_index: usize) -> Self {
        self.selected_path_index = selected_path_index;
        self
    }

    pub fn with_headless_nvapi_edids(
        mut self,
        entries: Vec<crate::nvapi_headless::HeadlessEdidRecovery>,
    ) -> Self {
        self.headless_nvapi_edids = entries;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct StableOutputIdentity {
    pub adapter_stable_id: String,
    pub monitor_device_path: String,
    pub adapter_output_index: u32,
    pub output_technology: i32,
    pub connector_instance: u32,
    pub edid_manufacture_id: u16,
    pub edid_product_code_id: u16,
    #[serde(default)]
    pub edid_sha256: Option<String>,
    pub binding: StableOutputBackend,
}

impl StableOutputIdentity {
    pub fn nvapi_display_id(&self) -> Option<u32> {
        match self.binding {
            StableOutputBackend::Nvidia {
                nvapi_display_id, ..
            } => Some(nvapi_display_id),
            StableOutputBackend::WindowsNative => None,
        }
    }

    pub fn nvapi_output_binding(&self) -> Option<(u32, u32)> {
        match self.binding {
            StableOutputBackend::Nvidia {
                nvapi_output_id,
                nvapi_head,
                ..
            } => Some((nvapi_output_id, nvapi_head)),
            StableOutputBackend::WindowsNative => None,
        }
    }

    pub fn immutable_binding_matches(&self, current: &Self) -> bool {
        if !self
            .adapter_stable_id
            .eq_ignore_ascii_case(&current.adapter_stable_id)
            || self.adapter_output_index != current.adapter_output_index
            || self.output_technology != current.output_technology
            || self.connector_instance != current.connector_instance
        {
            return false;
        }
        match (&self.binding, &current.binding) {
            (
                StableOutputBackend::Nvidia {
                    nvapi_display_id: expected,
                    nvapi_output_id: expected_output,
                    nvapi_head: expected_head,
                },
                StableOutputBackend::Nvidia {
                    nvapi_display_id: actual,
                    nvapi_output_id: actual_output,
                    nvapi_head: actual_head,
                },
            ) => {
                expected == actual
                    && expected_output == actual_output
                    && expected_head == actual_head
            }
            (StableOutputBackend::WindowsNative, StableOutputBackend::WindowsNative) => {
                self.monitor_device_path
                    .eq_ignore_ascii_case(&current.monitor_device_path)
                    && self.edid_manufacture_id == current.edid_manufacture_id
                    && self.edid_product_code_id == current.edid_product_code_id
                    && self.edid_sha256 == current.edid_sha256
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StableOutputBackend {
    Nvidia {
        nvapi_display_id: u32,
        nvapi_output_id: u32,
        nvapi_head: u32,
    },
    WindowsNative,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct StableTopologySnapshot {
    pub paths: Vec<StableOutputIdentity>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DesksideRecoveryStage {
    Armed,
    Protected,
    Restoring,
    RestoreFailed,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct DesksideRecoveryEntry {
    pub stage: DesksideRecoveryStage,
    pub input_fingerprint_sha256: [u8; 32],
    pub display_fingerprint_sha256: [u8; 32],
    pub physical_monitor_count: usize,
}

impl DesksideRecoveryEntry {
    pub fn armed(
        input_fingerprint_sha256: [u8; 32],
        display_fingerprint_sha256: [u8; 32],
        physical_monitor_count: usize,
    ) -> Self {
        Self {
            stage: DesksideRecoveryStage::Armed,
            input_fingerprint_sha256,
            display_fingerprint_sha256,
            physical_monitor_count,
        }
    }
}

/// Where the display recovery journal lives.
///
/// This journal is armed by the per-session agent, which runs under the
/// signed-in user's unelevated token, so it must sit in a directory that token
/// can create files in. The Arcen data root is not such a directory: it carries
/// a protected DACL with exactly SYSTEM and Administrators, so arming from the root failed every session with
///     create display recovery journal "...\display-recovery.tmp-<pid>":
///     Access is denied.
/// `recovery/` is no better; only the service writes there. The agent-writable
/// runtime directory is the one place both requirements hold.
pub fn default_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ARCEN_DISPLAY_RECOVERY_JOURNAL") {
        return PathBuf::from(path);
    }
    crate::paths::agent_runtime_dir().join("display-recovery.json")
}

pub fn write_atomic(path: &Path, journal: &DisplayRecoveryJournal) -> Result<(), String> {
    let payload = serde_json::to_vec_pretty(journal)
        .map_err(|error| format!("serialize display recovery journal: {error}"))?;
    write_atomic_bytes(
        path,
        &payload,
        MAX_JOURNAL_BYTES,
        "display recovery journal",
    )
}

pub(crate) fn write_atomic_bytes(
    path: &Path,
    payload: &[u8],
    max_bytes: u64,
    description: &str,
) -> Result<(), String> {
    if payload.len() as u64 > max_bytes {
        return Err(format!("{description} exceeds {max_bytes} bytes"));
    }
    if let Some(parent) = path.parent() {
        reject_reparse_point(parent, description)?;
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {description} directory {parent:?}: {error}"))?;
        reject_reparse_point(parent, description)?;
    }
    reject_reparse_point(path, description)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    reject_reparse_point(&temporary, description)?;
    let write_result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|error| format!("create {description} {temporary:?}: {error}"))?;
        file.write_all(payload)
            .map_err(|error| format!("write {description} {temporary:?}: {error}"))?;
        file.flush()
            .map_err(|error| format!("flush {description} {temporary:?}: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("sync {description} {temporary:?}: {error}"))?;
        std::fs::rename(&temporary, path)
            .map_err(|error| format!("publish {description} {path:?}: {error}"))?;
        sync_parent_directory(path, description)
    })();
    if write_result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    write_result
}

fn sync_parent_directory(path: &Path, description: &str) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    match std::fs::File::open(parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(()),
        Err(error)
            if cfg!(windows)
                && matches!(
                    error.raw_os_error(),
                    // Windows may reject FlushFileBuffers on a directory handle.
                    Some(1 | 5 | 6 | 87)
                ) =>
        {
            Ok(())
        }
        Err(error) => Err(format!(
            "sync parent directory for {description} {parent:?}: {error}"
        )),
    }
}

fn require_object_fields(
    value: &serde_json::Value,
    fields: &[&str],
    description: &str,
) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| format!("{description} is not an object"))?;
    for field in fields {
        if !object.contains_key(*field) {
            return Err(format!("{description} lacks required field `{field}`"));
        }
    }
    Ok(())
}

fn require_v4_authority(value: &serde_json::Value) -> Result<(), String> {
    require_object_fields(
        value,
        &[
            "version",
            "mutation_started",
            "device_name",
            "selected_path_index",
            "original_width",
            "original_height",
            "original_refresh_hz",
            "topology_paths_hex",
            "topology_modes_hex",
            "devmode_hex",
            "nvapi",
            "stable_topology",
        ],
        "display recovery journal v4",
    )?;
    let stable = value
        .get("stable_topology")
        .and_then(serde_json::Value::as_object)
        .ok_or_else(|| "display recovery journal v4 stable topology is absent".to_string())?;
    let paths = stable
        .get("paths")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| "display recovery journal v4 stable topology lacks paths".to_string())?;
    for (index, identity) in paths.iter().enumerate() {
        require_object_fields(
            identity,
            &[
                "adapter_stable_id",
                "monitor_device_path",
                "adapter_output_index",
                "output_technology",
                "connector_instance",
                "edid_manufacture_id",
                "edid_product_code_id",
                "edid_sha256",
                "binding",
            ],
            &format!("display recovery journal v4 stable path {index}"),
        )?;
        let binding = identity.get("binding").ok_or_else(|| {
            format!("display recovery journal v4 stable path {index} lacks binding")
        })?;
        require_object_fields(
            binding,
            &["kind"],
            &format!("display recovery journal v4 stable path {index} binding"),
        )?;
        match binding.get("kind").and_then(serde_json::Value::as_str) {
            Some("nvidia") => require_object_fields(
                binding,
                &["nvapi_display_id", "nvapi_output_id", "nvapi_head"],
                &format!("display recovery journal v4 stable path {index} NVIDIA binding"),
            )?,
            Some("windows_native") => {}
            _ => {
                return Err(format!(
                    "display recovery journal v4 stable path {index} has unknown backend binding"
                ));
            }
        }
    }
    if let Some(nvapi) = value.get("nvapi").filter(|value| !value.is_null()) {
        require_object_fields(
            nvapi,
            &[
                "device_name",
                "adapter_luid",
                "original_edid",
                "original_config",
                "display_id",
                "width",
                "height",
                "refresh_hz",
                "ownership",
                "custom",
                "custom_snapshot_complete",
                "pre_existing_custom",
                "cleanup_stage",
                "edid_write_stage",
                "intended_edid_sha256",
            ],
            "display recovery journal v4 NVAPI authority",
        )?;
    }
    Ok(())
}

fn require_v5_authority(value: &serde_json::Value) -> Result<(), String> {
    require_v4_authority(value)?;
    require_object_fields(
        value,
        &["headless_nvapi_edids"],
        "display recovery journal v5",
    )
}

pub fn read(path: &Path) -> Result<DisplayRecoveryJournal, String> {
    reject_reparse_point(path, "display recovery journal")?;
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("stat display recovery journal {path:?}: {error}"))?;
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(format!(
            "display recovery journal is {} bytes; limit is {MAX_JOURNAL_BYTES}",
            metadata.len()
        ));
    }
    let payload = std::fs::read(path)
        .map_err(|error| format!("read display recovery journal {path:?}: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|error| format!("parse display recovery journal {path:?}: {error}"))?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "display recovery journal lacks a numeric version".to_string())?;
    if version == JOURNAL_VERSION as u64 {
        require_v5_authority(&value)?;
    } else if version == 4 {
        require_v4_authority(&value)?;
    }
    let journal: DisplayRecoveryJournal = serde_json::from_value(value)
        .map_err(|error| format!("parse display recovery journal {path:?}: {error}"))?;
    if !(MIN_SUPPORTED_JOURNAL_VERSION..=JOURNAL_VERSION).contains(&journal.version) {
        return Err(format!(
            "display recovery journal version {} is unsupported",
            journal.version
        ));
    }
    if journal.device_name.is_empty() || journal.device_name.len() > 128 {
        return Err("display recovery journal has an invalid device name".to_string());
    }
    if let Some(deskside) = journal.deskside.as_ref() {
        if !(1..=16).contains(&deskside.physical_monitor_count)
            || deskside.input_fingerprint_sha256 == [0; 32]
            || deskside.display_fingerprint_sha256 == [0; 32]
        {
            return Err("display recovery journal has invalid deskside metadata".to_string());
        }
    }
    if journal.version >= 4 {
        if journal.topology_paths()?.is_empty()
            || journal.topology_modes()?.is_empty()
            || journal.devmode()?.is_empty()
        {
            return Err("display recovery journal v4 has empty topology authority".to_string());
        }
        let stable = journal.stable_topology.as_ref().ok_or_else(|| {
            "display recovery journal v4 lacks stable topology identities".to_string()
        })?;
        if stable.paths.is_empty() || stable.paths.len() > 128 {
            return Err("display recovery journal has invalid stable path count".to_string());
        }
        if journal.selected_path_index >= stable.paths.len() {
            return Err(
                "display recovery journal selected binding is outside stable topology".to_string(),
            );
        }
        let mut identities = std::collections::BTreeSet::new();
        let mut nvapi_ids = std::collections::BTreeSet::new();
        for identity in &stable.paths {
            if identity.adapter_stable_id.is_empty()
                || identity.adapter_stable_id.len() > 1024
                || identity.monitor_device_path.is_empty()
                || identity.monitor_device_path.len() > 1024
                || identity.edid_sha256.as_ref().is_some_and(|hash| {
                    hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
                || !identities.insert((
                    identity.adapter_stable_id.to_ascii_lowercase(),
                    identity.monitor_device_path.to_ascii_lowercase(),
                ))
            {
                return Err(
                    "display recovery journal has invalid or duplicate stable output identity"
                        .to_string(),
                );
            }
            if identity
                .nvapi_display_id()
                .is_some_and(|display_id| !nvapi_ids.insert(display_id))
            {
                return Err(
                    "display recovery journal repeats a stable NVAPI display identity".to_string(),
                );
            }
            if let StableOutputBackend::Nvidia {
                nvapi_output_id,
                nvapi_head,
                ..
            } = identity.binding
            {
                if nvapi_output_id.count_ones() != 1
                    || nvapi_output_id.trailing_zeros() != nvapi_head
                {
                    return Err(
                        "display recovery journal has invalid NVAPI output/head binding"
                            .to_string(),
                    );
                }
            }
        }
        if journal.version >= 5 {
            if journal.headless_nvapi_edids.len() > arcen_media::MAX_MULTI_MONITOR_COUNT {
                return Err(
                    "display recovery journal has too many headless EDID entries".to_string(),
                );
            }
            let mut displays = std::collections::BTreeSet::new();
            let mut outputs = Vec::new();
            for entry in &journal.headless_nvapi_edids {
                entry.validate()?;
                if !displays.insert(entry.display_id)
                    || outputs.contains(&(entry.adapter_luid, entry.output_id))
                {
                    return Err(
                        "display recovery journal repeats a headless EDID output".to_string()
                    );
                }
                outputs.push((entry.adapter_luid, entry.output_id));
            }
        }
        // NVAPI recovery state describes the ONE display this journal mutates,
        // so it must be correlated with the selected path, never with the
        // topology as a whole. Asking whether *any* path is NVIDIA rejected
        // every legitimate journal on a machine that has an NVIDIA adapter
        // attached but mutates a display on a different one. A VM with two
        // NVIDIA GRID adapters and a Microsoft Basic Display Adapter driving
        // the console is exactly that shape: the selected display is
        // Windows-native and correctly has no NVAPI state, yet the GRID
        // adapters made `any` true and refused every session.
        //
        // Standalone NVAPI state must match the selected path. A full stable
        // topology can recover an NVIDIA-selected multi-output journal without
        // standalone NVAPI state because each path already carries its exact
        // immutable NVIDIA binding.
        let selected = &stable.paths[journal.selected_path_index];
        match journal.nvapi.as_ref() {
            Some(nvapi)
                if selected.nvapi_display_id().is_none()
                    || selected.nvapi_display_id() != nvapi.display_id =>
            {
                return Err(
                    "display recovery journal selected binding does not match NVAPI authority"
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    if journal.original_width == 0
        || journal.original_height == 0
        || journal.original_width > 16_384
        || journal.original_height > 8_640
        || journal.original_refresh_hz > 1_000
    {
        return Err("display recovery journal has invalid original geometry".to_string());
    }
    if let Some(nvapi) = journal.nvapi.as_ref() {
        if nvapi.display_id.is_none() {
            return Err(
                "display recovery journal NVAPI state lacks authoritative display id".to_string(),
            );
        }
        if nvapi.device_name != journal.device_name {
            return Err("display recovery journal NVAPI device does not match".to_string());
        }
        if nvapi.width == 0
            || nvapi.height == 0
            || nvapi.width > 16_384
            || nvapi.height > 8_640
            || nvapi.refresh_hz == 0
            || nvapi.refresh_hz > 1_000
        {
            return Err("display recovery journal has invalid NVAPI geometry".to_string());
        }
        if nvapi
            .original_edid
            .as_ref()
            .is_some_and(|edid| edid.len() > 256)
        {
            return Err("display recovery journal NVAPI EDID exceeds 256 bytes".to_string());
        }
        if nvapi.original_config.paths.is_empty() || nvapi.original_config.paths.len() > 128 {
            return Err("display recovery journal has invalid NVAPI path count".to_string());
        }
        crate::nvapi::validate_display_config(&nvapi.original_config)
            .map_err(|error| format!("display recovery journal NVAPI topology: {error}"))?;
        if nvapi
            .original_config
            .paths
            .iter()
            .any(|path| path.targets.len() > 64)
        {
            return Err("display recovery journal NVAPI target count exceeds limit".to_string());
        }
        if nvapi
            .custom
            .as_ref()
            .is_some_and(|custom| !custom.is_valid())
            || nvapi
                .pre_existing_custom
                .iter()
                .any(|custom| !custom.is_valid())
            || nvapi.pre_existing_custom.len() > 64
        {
            return Err("display recovery journal has invalid custom timing state".to_string());
        }
        match (
            nvapi.edid_write_stage,
            nvapi.intended_edid_sha256.as_deref(),
        ) {
            (crate::nvapi::EdidWriteStage::None, None) => {}
            (
                crate::nvapi::EdidWriteStage::Attempted | crate::nvapi::EdidWriteStage::Verified,
                Some(hash),
            ) if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) => {
                let display_id = nvapi.display_id.ok_or_else(|| {
                    "EDID write checkpoint lacks authoritative NVAPI display id".to_string()
                })?;
                let bound = journal.stable_topology.as_ref().is_some_and(|topology| {
                    topology
                        .paths
                        .iter()
                        .any(|identity| identity.nvapi_display_id() == Some(display_id))
                });
                if !bound {
                    return Err(
                        "EDID write checkpoint lacks immutable stable output binding".to_string(),
                    );
                }
            }
            _ => {
                return Err(
                    "display recovery journal has invalid EDID write checkpoint".to_string()
                );
            }
        }
        if !matches!(
            nvapi.ownership,
            crate::nvapi::TimingOwnership::NotTried
                | crate::nvapi::TimingOwnership::CleanupComplete
        ) && nvapi.custom.is_none()
        {
            return Err(
                "display recovery journal claims custom timing ownership without timing data"
                    .to_string(),
            );
        }
    }
    Ok(journal)
}

pub fn remove(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("remove display recovery journal {path:?}: {error}")),
    }
}

pub fn upgrade_legacy_stable_topology(
    path: &Path,
    topology: StableTopologySnapshot,
    selected_path_index: usize,
) -> Result<(), String> {
    let mut journal = legacy_windows_migration_evidence(path)?;
    if journal.version >= JOURNAL_VERSION {
        return Err("display recovery journal is already current".to_string());
    }
    if journal.stable_topology.is_some() {
        return Err("legacy display recovery journal unexpectedly has stable topology".to_string());
    }
    journal.version = JOURNAL_VERSION;
    journal.selected_path_index = selected_path_index;
    journal.stable_topology = Some(topology);
    write_atomic(path, &journal)
}

pub fn legacy_windows_migration_evidence(path: &Path) -> Result<DisplayRecoveryJournal, String> {
    reject_reparse_point(path, "display recovery journal")?;
    let payload = std::fs::read(path)
        .map_err(|error| format!("read legacy display recovery journal {path:?}: {error}"))?;
    let value: serde_json::Value = serde_json::from_slice(&payload)
        .map_err(|error| format!("parse legacy display recovery journal {path:?}: {error}"))?;
    validate_legacy_windows_migration_value(&value)?;
    read(path)
}

fn validate_legacy_windows_migration_value(value: &serde_json::Value) -> Result<(), String> {
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "legacy display recovery journal lacks a numeric version".to_string())?;
    if !(MIN_SUPPORTED_JOURNAL_VERSION as u64..JOURNAL_VERSION as u64).contains(&version) {
        return Err("display recovery journal is not a migratable v1-v3 journal".to_string());
    }
    if value
        .get("mutation_started")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return Err(
            "legacy journal lacks explicit durable mutation-started evidence; preserve it for manual recovery"
                .to_string(),
        );
    }
    match value.as_object().and_then(|object| object.get("nvapi")) {
        Some(value) if value.is_null() => {}
        Some(_) => {
            return Err(
                "legacy NVIDIA journal lacks trustworthy EDID-attempt/original ownership evidence; preserve it for manual recovery"
                    .to_string(),
            )
        }
        None => {
            return Err(
                "legacy journal lacks explicit backend/EDID ownership evidence; preserve it for manual recovery"
                    .to_string(),
            )
        }
    }
    Ok(())
}

pub fn mark_mutation_started(path: &Path) -> Result<(), String> {
    let mut journal = read(path)?;
    if !journal.mutation_started {
        journal.mutation_started = true;
        write_atomic(path, &journal)?;
    }
    Ok(())
}

pub fn mark_deskside_stage(path: &Path, next: DesksideRecoveryStage) -> Result<(), String> {
    let mut journal = read(path)?;
    let deskside = journal
        .deskside
        .as_mut()
        .ok_or_else(|| "display recovery journal has no deskside metadata".to_string())?;
    let valid = deskside.stage == next
        || matches!(
            (deskside.stage, next),
            (
                DesksideRecoveryStage::Armed,
                DesksideRecoveryStage::Protected
            ) | (
                DesksideRecoveryStage::Protected,
                DesksideRecoveryStage::Restoring
            ) | (
                DesksideRecoveryStage::Restoring,
                DesksideRecoveryStage::RestoreFailed
            )
        );
    if !valid {
        return Err(format!(
            "invalid deskside recovery transition {:?} -> {next:?}",
            deskside.stage
        ));
    }
    deskside.stage = next;
    write_atomic(path, &journal)
}

pub(crate) fn reject_reparse_point(path: &Path, description: &str) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(format!("inspect {description} path {path:?}: {error}")),
    };
    if metadata.file_type().is_symlink() {
        return Err(format!("{description} path {path:?} is a symlink"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(format!("{description} path {path:?} is a reparse point"));
        }
    }
    Ok(())
}

pub fn mark_nvapi_ownership(
    path: &Path,
    active: &crate::nvapi::ActiveExactMode,
) -> Result<(), String> {
    let mut journal = read(path)?;
    if active.edid_write_stage != crate::nvapi::EdidWriteStage::None {
        let hash = active
            .intended_edid_sha256
            .as_deref()
            .filter(|hash| hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .ok_or_else(|| "EDID write checkpoint has invalid intended fingerprint".to_string())?;
        let _ = hash;
        let display_id = journal
            .nvapi
            .as_ref()
            .and_then(|nvapi| nvapi.display_id)
            .ok_or_else(|| "EDID write checkpoint lacks stable display id".to_string())?;
        if !journal.stable_topology.as_ref().is_some_and(|topology| {
            topology
                .paths
                .iter()
                .any(|identity| identity.nvapi_display_id() == Some(display_id))
        }) {
            return Err("EDID write checkpoint lacks immutable connector binding".to_string());
        }
    }
    let nvapi = journal
        .nvapi
        .as_mut()
        .ok_or_else(|| "display recovery journal has no NVAPI state".to_string())?;
    let valid_edid_transition = nvapi.edid_write_stage == active.edid_write_stage
        || matches!(
            (nvapi.edid_write_stage, active.edid_write_stage),
            (
                crate::nvapi::EdidWriteStage::None,
                crate::nvapi::EdidWriteStage::Attempted
            ) | (
                crate::nvapi::EdidWriteStage::Attempted,
                crate::nvapi::EdidWriteStage::Verified
            )
        );
    if !valid_edid_transition {
        return Err(format!(
            "invalid NVAPI EDID transition {:?} -> {:?}",
            nvapi.edid_write_stage, active.edid_write_stage
        ));
    }
    let valid_transition = matches!(
        (nvapi.ownership, active.ownership),
        (
            crate::nvapi::TimingOwnership::NotTried,
            crate::nvapi::TimingOwnership::TrialAttemptedByUs
        ) | (
            crate::nvapi::TimingOwnership::TrialAttemptedByUs,
            crate::nvapi::TimingOwnership::TrialAppliedByUs
        ) | (
            crate::nvapi::TimingOwnership::TrialAppliedByUs,
            crate::nvapi::TimingOwnership::SaveAttemptedByUs
        ) | (
            crate::nvapi::TimingOwnership::SaveAttemptedByUs,
            crate::nvapi::TimingOwnership::SavedByUs
                | crate::nvapi::TimingOwnership::TrialAppliedByUs
                | crate::nvapi::TimingOwnership::SaveAttemptedByUs
        )
    ) || nvapi.ownership == active.ownership;
    if !valid_transition {
        return Err(format!(
            "invalid NVAPI ownership transition {:?} -> {:?}",
            nvapi.ownership, active.ownership
        ));
    }
    nvapi.ownership = active.ownership;
    nvapi.custom = active.custom.clone();
    nvapi.custom_snapshot_complete = active.custom_snapshot_complete;
    nvapi.pre_existing_custom = active.pre_existing_custom.clone();
    nvapi.edid_write_stage = active.edid_write_stage;
    nvapi.intended_edid_sha256 = active.intended_edid_sha256.clone();
    write_atomic(path, &journal)
}

pub fn nvapi_cleanup_stage(path: &Path) -> Result<crate::nvapi::CleanupStage, String> {
    read(path)?
        .nvapi
        .map(|nvapi| nvapi.cleanup_stage)
        .ok_or_else(|| "display recovery journal has no NVAPI state".to_string())
}

pub fn mark_nvapi_cleanup_stage(
    path: &Path,
    next: crate::nvapi::CleanupStage,
) -> Result<(), String> {
    let mut journal = read(path)?;
    let nvapi = journal
        .nvapi
        .as_mut()
        .ok_or_else(|| "display recovery journal has no NVAPI state".to_string())?;
    if nvapi.cleanup_stage == next {
        return Ok(());
    }
    if nvapi.cleanup_stage.next() != Some(next) {
        return Err(format!(
            "invalid NVAPI cleanup transition {:?} -> {next:?}",
            nvapi.cleanup_stage
        ));
    }
    nvapi.cleanup_stage = next;
    if next == crate::nvapi::CleanupStage::Complete {
        nvapi.ownership = crate::nvapi::TimingOwnership::CleanupComplete;
        nvapi.custom = None;
    }
    write_atomic(path, &journal)
}

pub fn rearm_nvapi(path: &Path, replacement: crate::nvapi::RecoveryData) -> Result<(), String> {
    let mut journal = read(path)?;
    let current = journal
        .nvapi
        .as_ref()
        .ok_or_else(|| "display recovery journal has no NVAPI state".to_string())?;
    let unowned_pending = current.cleanup_stage == crate::nvapi::CleanupStage::Pending
        && current.ownership == crate::nvapi::TimingOwnership::NotTried
        && current.custom.is_none();
    let cleaned = current.cleanup_stage == crate::nvapi::CleanupStage::Complete
        && current.ownership == crate::nvapi::TimingOwnership::CleanupComplete;
    if !unowned_pending && !cleaned {
        return Err("cannot rearm NVAPI recovery before prior cleanup completes".to_string());
    }
    if replacement.device_name != journal.device_name
        || replacement.adapter_luid != current.adapter_luid
        || replacement.display_id != current.display_id
        || replacement.original_edid != current.original_edid
        || replacement.original_config != current.original_config
    {
        return Err("NVAPI retarget attempted to change original recovery state".to_string());
    }
    if replacement.cleanup_stage != crate::nvapi::CleanupStage::Pending
        || replacement.ownership != crate::nvapi::TimingOwnership::NotTried
        || replacement.custom.is_some()
    {
        return Err("NVAPI retarget recovery state is not freshly pending".to_string());
    }
    journal.nvapi = Some(replacement);
    write_atomic(path, &journal)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    if value.len() % 2 != 0 {
        return Err("display recovery hex field has odd length".to_string());
    }
    let decode = |byte: u8| match byte {
        b'0'..=b'9' => Ok(byte - b'0'),
        b'a'..=b'f' => Ok(byte - b'a' + 10),
        b'A'..=b'F' => Ok(byte - b'A' + 10),
        _ => Err("display recovery hex field contains a non-hex byte".to_string()),
    };
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| Ok((decode(pair[0])? << 4) | decode(pair[1])?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The journal is armed by the per-session agent under the signed-in
    /// user's unelevated token. The Arcen data root carries a protected DACL
    /// with exactly SYSTEM and Administrators, so a default path in the root
    /// makes every display-mutating session fail with
    /// `create display recovery journal ...: Access is denied`. That shipped
    /// once; this pins the directory so it cannot come back.
    #[test]
    fn default_journal_path_is_agent_writable_not_the_protected_root() {
        let previous = std::env::var_os("ARCEN_DISPLAY_RECOVERY_JOURNAL");
        std::env::remove_var("ARCEN_DISPLAY_RECOVERY_JOURNAL");
        let path = default_path();
        if let Some(previous) = previous {
            std::env::set_var("ARCEN_DISPLAY_RECOVERY_JOURNAL", previous);
        }

        assert_eq!(
            path,
            crate::paths::agent_runtime_dir().join("display-recovery.json")
        );
        assert_ne!(
            path,
            crate::paths::arcen_data_root().join("display-recovery.json"),
            "the agent cannot create files in the protected data root"
        );
        assert_ne!(
            path,
            crate::paths::recovery_dir().join("display-recovery.json"),
            "recovery/ is written by the service, not by the agent"
        );
    }

    fn journal() -> DisplayRecoveryJournal {
        DisplayRecoveryJournal::new(
            r"\\.\DISPLAY6".to_string(),
            1680,
            1050,
            59,
            &[1, 2, 3],
            &[4, 5],
            &[6, 7, 8, 9],
            None,
        )
        .with_stable_topology(StableTopologySnapshot {
            paths: vec![StableOutputIdentity {
                adapter_stable_id: "pci:test-adapter".to_string(),
                monitor_device_path: r"\\?\DISPLAY#TEST#UID1".to_string(),
                adapter_output_index: 0,
                output_technology: 4,
                connector_instance: 1,
                edid_manufacture_id: 1,
                edid_product_code_id: 2,
                edid_sha256: Some("0".repeat(64)),
                binding: StableOutputBackend::WindowsNative,
            }],
        })
    }

    #[test]
    fn journal_round_trip_is_atomic_and_lossless() {
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();
        let value = journal();
        write_atomic(&path, &value).unwrap();
        let loaded = read(&path).unwrap();
        assert!(!loaded.mutation_started);
        assert_eq!(loaded.topology_paths().unwrap(), vec![1, 2, 3]);
        assert_eq!(loaded.topology_modes().unwrap(), vec![4, 5]);
        assert_eq!(loaded.devmode().unwrap(), vec![6, 7, 8, 9]);
        assert_eq!(loaded.stable_topology, value.stable_topology);
        mark_mutation_started(&path).unwrap();
        assert!(read(&path).unwrap().mutation_started);
        remove(&path).unwrap();
    }

    #[test]
    fn legacy_journal_without_deskside_metadata_remains_compatible() {
        let mut value = serde_json::to_value(journal()).unwrap();
        value
            .as_object_mut()
            .expect("journal object")
            .remove("deskside");
        let decoded: DisplayRecoveryJournal = serde_json::from_value(value).unwrap();
        assert!(decoded.deskside.is_none());
    }

    #[test]
    fn legacy_journal_without_stable_topology_deserializes_for_fail_closed_recovery() {
        let mut value = serde_json::to_value(journal()).unwrap();
        value["version"] = serde_json::json!(3);
        value
            .as_object_mut()
            .expect("journal object")
            .remove("stable_topology");
        let decoded: DisplayRecoveryJournal = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.version, 3);
        assert!(decoded.stable_topology.is_none());
    }

    #[test]
    fn stable_topology_serialization_preserves_all_output_identity_fields() {
        let value = journal();
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: DisplayRecoveryJournal = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.version, 5);
        assert_eq!(decoded.stable_topology, value.stable_topology);
    }

    #[test]
    fn current_journal_preserves_headless_edid_rollback_authority() {
        let mut value = journal();
        value.headless_nvapi_edids = vec![crate::nvapi_headless::HeadlessEdidRecovery {
            display_id: 0x8206_1081,
            output_id: 0x400,
            adapter_luid: crate::nvapi::AdapterLuid {
                low_part: 47_171,
                high_part: 0,
            },
            original_edid: None,
            intended_edid_sha256: "0".repeat(64),
        }];
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: DisplayRecoveryJournal = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.version, 5);
        assert_eq!(decoded.headless_nvapi_edids.len(), 1);
        assert_eq!(decoded.headless_nvapi_edids[0].display_id, 0x8206_1081);
        assert_eq!(decoded.headless_nvapi_edids[0].output_id, 0x400);
    }

    #[test]
    fn malformed_v4_authority_fields_are_never_defaulted() {
        let base = serde_json::to_value(journal()).unwrap();
        for field in [
            "mutation_started",
            "selected_path_index",
            "topology_paths_hex",
            "topology_modes_hex",
            "devmode_hex",
            "nvapi",
            "stable_topology",
        ] {
            let mut value = base.clone();
            value.as_object_mut().unwrap().remove(field);
            assert!(
                require_v4_authority(&value).unwrap_err().contains(field),
                "omitted top-level field {field} was accepted"
            );
        }

        let identity_fields = [
            "adapter_stable_id",
            "monitor_device_path",
            "adapter_output_index",
            "output_technology",
            "connector_instance",
            "edid_manufacture_id",
            "edid_product_code_id",
            "edid_sha256",
            "binding",
        ];
        for field in identity_fields {
            let mut value = base.clone();
            value["stable_topology"]["paths"][0]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                require_v4_authority(&value).unwrap_err().contains(field),
                "omitted stable identity field {field} was accepted"
            );
        }
    }

    #[test]
    fn malformed_v4_nvapi_authority_fields_are_never_defaulted() {
        let mut value = journal();
        let nvapi = crate::nvapi::test_recovery_data(value.device_name.clone());
        value.stable_topology.as_mut().unwrap().paths[0].binding = StableOutputBackend::Nvidia {
            nvapi_display_id: nvapi.display_id.unwrap(),
            nvapi_output_id: 1,
            nvapi_head: 0,
        };
        value.nvapi = Some(nvapi);
        let base = serde_json::to_value(value).unwrap();
        for field in ["kind", "nvapi_display_id", "nvapi_output_id", "nvapi_head"] {
            let mut malformed = base.clone();
            malformed["stable_topology"]["paths"][0]["binding"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(
                require_v4_authority(&malformed)
                    .unwrap_err()
                    .contains(field),
                "omitted NVIDIA binding field {field} was accepted"
            );
        }
        for field in [
            "original_edid",
            "display_id",
            "ownership",
            "custom",
            "custom_snapshot_complete",
            "pre_existing_custom",
            "cleanup_stage",
            "edid_write_stage",
            "intended_edid_sha256",
        ] {
            let mut malformed = base.clone();
            malformed["nvapi"].as_object_mut().unwrap().remove(field);
            assert!(
                require_v4_authority(&malformed)
                    .unwrap_err()
                    .contains(field),
                "omitted NVAPI authority field {field} was accepted"
            );
        }
    }

    #[test]
    fn legacy_migration_requires_explicit_non_nvidia_ownership_evidence() {
        let mut value = serde_json::to_value(journal()).unwrap();
        value["version"] = serde_json::json!(3);
        value["mutation_started"] = serde_json::json!(true);
        value["nvapi"] = serde_json::Value::Null;
        assert!(validate_legacy_windows_migration_value(&value).is_ok());

        value.as_object_mut().unwrap().remove("mutation_started");
        assert!(validate_legacy_windows_migration_value(&value)
            .unwrap_err()
            .contains("mutation-started"));
        value["mutation_started"] = serde_json::json!(true);
        value["nvapi"] = serde_json::to_value(crate::nvapi::test_recovery_data(
            r"\\.\DISPLAY6".to_string(),
        ))
        .unwrap();
        assert!(validate_legacy_windows_migration_value(&value)
            .unwrap_err()
            .contains("legacy NVIDIA"));
    }

    #[test]
    fn guarded_legacy_upgrade_is_atomic_and_preserves_mutation_state() {
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-upgrade-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();
        let stable = journal().stable_topology.unwrap();
        let mut legacy = journal();
        legacy.version = 3;
        legacy.mutation_started = true;
        legacy.stable_topology = None;
        write_atomic(&path, &legacy).unwrap();

        upgrade_legacy_stable_topology(&path, stable.clone(), 0).unwrap();
        let upgraded = read(&path).unwrap();
        assert_eq!(upgraded.version, 5);
        assert!(upgraded.mutation_started);
        assert_eq!(upgraded.stable_topology, Some(stable));
        remove(&path).unwrap();
    }

    #[test]
    fn injected_edid_changes_mutable_identity_but_not_immutable_output_binding() {
        let mut original = journal()
            .stable_topology
            .unwrap()
            .paths
            .into_iter()
            .next()
            .unwrap();
        original.binding = StableOutputBackend::Nvidia {
            nvapi_display_id: 0x1234,
            nvapi_output_id: 1,
            nvapi_head: 0,
        };
        let mut injected = original.clone();
        injected.monitor_device_path = r"\\?\DISPLAY#TRGA05E#INJECTED".to_string();
        injected.edid_manufacture_id = 0x4752;
        injected.edid_product_code_id = 0xA05E;
        injected.edid_sha256 = Some("f".repeat(64));

        assert!(original.immutable_binding_matches(&injected));
        assert_ne!(original, injected);
        let restored = original.clone();
        assert_eq!(original, restored);
    }

    #[test]
    fn windows_native_binding_requires_complete_windows_identity() {
        let original = journal()
            .stable_topology
            .unwrap()
            .paths
            .into_iter()
            .next()
            .unwrap();
        let mut changed = original.clone();
        changed.monitor_device_path.push_str("-other");
        assert!(!original.immutable_binding_matches(&changed));
    }

    #[test]
    fn stable_nvidia_binding_without_standalone_nvapi_state_reads() {
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-backend-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();
        let software = journal();
        write_atomic(&path, &software).unwrap();
        assert!(read(&path).is_ok());

        let mut stable_nvidia = software;
        stable_nvidia.stable_topology.as_mut().unwrap().paths[0].binding =
            StableOutputBackend::Nvidia {
                nvapi_display_id: 0x1234,
                nvapi_output_id: 1,
                nvapi_head: 0,
            };
        write_atomic(&path, &stable_nvidia).unwrap();
        read(&path).expect("stable topology carries the NVIDIA recovery authority");
        remove(&path).unwrap();
    }

    /// A journal whose SELECTED display is Windows-native is valid even when
    /// another attached adapter is NVIDIA.
    ///
    /// pier-windows.example.internal is exactly this shape: two NVIDIA GRID adapters plus a
    /// Microsoft Basic Display Adapter driving the hypervisor console. The
    /// session mutates the console, so there is correctly no NVAPI recovery
    /// state, but a topology-wide `any` test saw the GRID adapters and refused
    /// every session on the machine.
    #[test]
    fn a_windows_native_selection_is_valid_beside_an_unselected_nvidia_path() {
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-mixed-adapter-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();

        let mut mixed = journal();
        let mut nvidia_path = mixed.stable_topology.as_ref().unwrap().paths[0].clone();
        nvidia_path.adapter_stable_id = "pci:test-nvidia-grid".to_string();
        nvidia_path.monitor_device_path = r"\\?\DISPLAY#TEST#UID2".to_string();
        nvidia_path.adapter_output_index = 1;
        nvidia_path.binding = StableOutputBackend::Nvidia {
            nvapi_display_id: 0x1234,
            nvapi_output_id: 1,
            nvapi_head: 0,
        };
        mixed
            .stable_topology
            .as_mut()
            .unwrap()
            .paths
            .push(nvidia_path);
        // Index 0 stays selected: the Windows-native console display.
        assert_eq!(mixed.selected_path_index, 0);
        assert!(mixed.nvapi.is_none());

        write_atomic(&path, &mixed).unwrap();
        read(&path).expect("an unselected NVIDIA path must not invalidate the journal");
        remove(&path).unwrap();
    }

    #[test]
    fn deskside_stage_updates_are_bounded_and_idempotent() {
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-deskside-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();
        let value =
            journal().with_deskside(Some(DesksideRecoveryEntry::armed([1; 32], [2; 32], 2)));
        write_atomic(&path, &value).unwrap();
        mark_deskside_stage(&path, DesksideRecoveryStage::Protected).unwrap();
        mark_deskside_stage(&path, DesksideRecoveryStage::Protected).unwrap();
        assert_eq!(
            read(&path).unwrap().deskside.unwrap().stage,
            DesksideRecoveryStage::Protected
        );
        remove(&path).unwrap();
    }

    #[test]
    fn journal_rejects_corruption_and_unsupported_versions() {
        let mut value = journal();
        value.topology_paths_hex = "xyz".to_string();
        assert!(value.topology_paths().is_err());
        value.version = 99;
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-version-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();
        write_atomic(&path, &value).unwrap();
        assert!(read(&path).unwrap_err().contains("version"));
        remove(&path).unwrap();
    }

    #[test]
    fn journal_nvapi_state_can_be_durably_updated() {
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-trial-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();
        let mut value = journal();
        let nvapi = crate::nvapi::test_recovery_data(value.device_name.clone());
        value.stable_topology.as_mut().unwrap().paths[0].binding = StableOutputBackend::Nvidia {
            nvapi_display_id: nvapi.display_id.unwrap(),
            nvapi_output_id: 1,
            nvapi_head: 0,
        };
        value.nvapi = Some(nvapi);
        write_atomic(&path, &value).unwrap();

        let mut active = crate::nvapi::ActiveExactMode {
            custom: None,
            ownership: crate::nvapi::TimingOwnership::NotTried,
            save_error: None,
            custom_snapshot_complete: true,
            pre_existing_custom: Vec::new(),
            edid_write_stage: crate::nvapi::EdidWriteStage::Attempted,
            intended_edid_sha256: Some("0".repeat(64)),
        };
        mark_nvapi_ownership(&path, &active).unwrap();
        active.edid_write_stage = crate::nvapi::EdidWriteStage::Verified;
        mark_nvapi_ownership(&path, &active).unwrap();
        active.custom = Some(crate::nvapi::CustomDisplay::test_value(3600, 2338, 60));
        active.ownership = crate::nvapi::TimingOwnership::TrialAttemptedByUs;
        mark_nvapi_ownership(&path, &active).unwrap();
        active.ownership = crate::nvapi::TimingOwnership::TrialAppliedByUs;
        mark_nvapi_ownership(&path, &active).unwrap();
        assert_eq!(
            read(&path).unwrap().nvapi.unwrap().ownership,
            crate::nvapi::TimingOwnership::TrialAppliedByUs
        );
        active.ownership = crate::nvapi::TimingOwnership::SaveAttemptedByUs;
        mark_nvapi_ownership(&path, &active).unwrap();
        active.ownership = crate::nvapi::TimingOwnership::SavedByUs;
        mark_nvapi_ownership(&path, &active).unwrap();
        assert_eq!(
            read(&path).unwrap().nvapi.unwrap().ownership,
            crate::nvapi::TimingOwnership::SavedByUs
        );
        for stage in [
            crate::nvapi::CleanupStage::TopologyRestored,
            crate::nvapi::CleanupStage::TrialReverted,
            crate::nvapi::CleanupStage::SavedTimingDeleted,
            crate::nvapi::CleanupStage::EdidRestored,
            crate::nvapi::CleanupStage::Complete,
        ] {
            mark_nvapi_cleanup_stage(&path, stage).unwrap();
            assert_eq!(nvapi_cleanup_stage(&path).unwrap(), stage);
            mark_nvapi_cleanup_stage(&path, stage).unwrap();
        }
        assert_eq!(
            read(&path).unwrap().nvapi.unwrap().ownership,
            crate::nvapi::TimingOwnership::CleanupComplete
        );
        let mut replacement = crate::nvapi::test_recovery_data(value.device_name.clone());
        replacement.width = 1920;
        replacement.height = 1072;
        rearm_nvapi(&path, replacement).unwrap();
        let rearmed = read(&path).unwrap().nvapi.unwrap();
        assert_eq!(rearmed.cleanup_stage, crate::nvapi::CleanupStage::Pending);
        assert_eq!(rearmed.ownership, crate::nvapi::TimingOwnership::NotTried);
        assert_eq!((rearmed.width, rearmed.height), (1920, 1072));
        remove(&path).unwrap();
        remove(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn isolation_only_pending_nvapi_state_can_rearm_for_media_retarget() {
        let path = std::env::temp_dir().join(format!(
            "arcen-display-recovery-pending-retarget-test-{}.json",
            std::process::id()
        ));
        remove(&path).unwrap();
        let mut value = journal();
        let nvapi = crate::nvapi::test_recovery_data(value.device_name.clone());
        value.stable_topology.as_mut().unwrap().paths[0].binding = StableOutputBackend::Nvidia {
            nvapi_display_id: nvapi.display_id.unwrap(),
            nvapi_output_id: 1,
            nvapi_head: 0,
        };
        value.nvapi = Some(nvapi);
        write_atomic(&path, &value).unwrap();

        let mut replacement = crate::nvapi::test_recovery_data(value.device_name);
        replacement.width = 1920;
        replacement.height = 1072;
        rearm_nvapi(&path, replacement).unwrap();

        let rearmed = read(&path).unwrap().nvapi.unwrap();
        assert_eq!((rearmed.width, rearmed.height), (1920, 1072));
        assert_eq!(rearmed.cleanup_stage, crate::nvapi::CleanupStage::Pending);
        assert_eq!(rearmed.ownership, crate::nvapi::TimingOwnership::NotTried);
        remove(&path).unwrap();
    }
}
