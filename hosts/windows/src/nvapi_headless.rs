//! Guarded NVIDIA headless-output activation proof.
//!
//! This module is intentionally narrower than a product output provider. It
//! proves one prerequisite transaction on one explicitly selected spare NVAPI
//! display ID:
//!
//! 1. snapshot its empty EDID and the interactive CCD inventory;
//! 2. arm an out-of-process recovery watchdog;
//! 3. write one deterministic Arcen EDID;
//! 4. prove that NVAPI marks the display connected and Windows marks one more
//!    CCD target available;
//! 5. clear the EDID and prove the exact baseline returns.
//!
//! It never calls `TryCustomDisplay`, `SaveCustomDisplay`, or
//! `SetDisplayConfig`. Product provisioning remains blocked until this proof
//! succeeds and its lifecycle can be composed with the existing physical
//! output-provider recovery journal.

use std::path::{Path, PathBuf};
use std::time::Duration;
#[cfg(windows)]
use std::time::Instant;

use serde::{Deserialize, Serialize};

use crate::nvapi::AdapterLuid;
#[cfg(windows)]
use crate::nvapi::DisplayMapping;
use crate::nvapi_inventory::{DisplayIdEntry, GpuEntry, NvapiInventoryReport};

const JOURNAL_VERSION: u32 = 1;
const MAX_JOURNAL_BYTES: u64 = 16 * 1024;
const MAX_EDID_BYTES: usize = 256;
const SETTLE_TIMEOUT: Duration = Duration::from_secs(30);
const SETTLE_POLL: Duration = Duration::from_millis(100);
const MAX_HOLD_MS: u64 = 30_000;

/// Final monitor contract an NVIDIA headless output must advertise before the
/// session starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HeadlessDisplayContract {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub width_mm: f32,
    pub height_mm: f32,
    pub scale: arcen_media::Scale120,
    pub product_id: u16,
    pub serial: u32,
    pub hdr10: bool,
    pub primary: bool,
    pub preferred_output_index: Option<u32>,
}

#[derive(Clone, Debug)]
pub(crate) struct ProbeRequest {
    pub display_id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub hold_ms: u64,
    pub journal: Option<PathBuf>,
    /// Advertise HDR10 in the probed EDID.
    ///
    /// Diagnostic only. Windows will not offer Advanced Color on an output
    /// whose EDID does not claim it, and without Advanced Color the desktop is
    /// never composited wider than 8-bit — which is what every capture backend
    /// then faithfully reports. This makes that first link testable.
    /// See `docs/internal/ten-bit-source-capture.md`.
    pub hdr10: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ProbeState {
    pub connected: bool,
    pub active: bool,
    pub os_visible: bool,
    pub edid_status: i32,
    pub edid_sha256: Option<String>,
    pub edid_manufacturer: Option<String>,
    pub edid_preferred_width: Option<u32>,
    pub edid_preferred_height: Option<u32>,
    pub ccd_available_target_ids: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct ProbeReport {
    pub schema_version: u32,
    pub display_id: u32,
    pub output_id: u32,
    pub adapter_luid: AdapterLuid,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub intended_edid_sha256: String,
    pub activation_elapsed_ms: u128,
    pub restore_elapsed_ms: u128,
    pub baseline: ProbeState,
    pub activated: ProbeState,
    pub restored: ProbeState,
    pub rollback_verified: bool,
}

#[derive(Clone, Debug)]
struct EdidProvisionChange {
    recovery: HeadlessEdidRecovery,
    desired_edid: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct ExpectedHeadlessDisplay {
    display_id: u32,
    edid_sha256: String,
    width: u32,
    height: u32,
    hdr10: bool,
}

#[derive(Clone, Debug)]
pub(crate) struct PreparedEdidProvision {
    adapter_name: String,
    adapter_luid: AdapterLuid,
    target_count: usize,
    changes: Vec<EdidProvisionChange>,
    expected: Vec<ExpectedHeadlessDisplay>,
    recovery_entries: Vec<HeadlessEdidRecovery>,
}

impl PreparedEdidProvision {
    pub(crate) fn recovery_entries(&self) -> Vec<HeadlessEdidRecovery> {
        self.recovery_entries.clone()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.recovery_entries.is_empty()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HeadlessEdidRecovery {
    pub display_id: u32,
    pub output_id: u32,
    pub adapter_luid: AdapterLuid,
    pub original_edid: Option<Vec<u8>>,
    pub intended_edid_sha256: String,
}

impl HeadlessEdidRecovery {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.display_id == 0 || self.output_id.count_ones() != 1 {
            return Err("NVAPI headless recovery entry has an invalid display/output ID".into());
        }
        if self
            .original_edid
            .as_ref()
            .is_some_and(|edid| edid.len() > MAX_EDID_BYTES)
        {
            return Err("NVAPI headless recovery entry EDID is oversized".into());
        }
        if self.intended_edid_sha256.len() != 64
            || !self
                .intended_edid_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("NVAPI headless recovery entry has an invalid EDID hash".into());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RecoveryJournal {
    version: u32,
    mutation_started: bool,
    entries: Vec<HeadlessEdidRecovery>,
}

impl RecoveryJournal {
    fn validate(&self) -> Result<(), String> {
        if self.version != JOURNAL_VERSION {
            return Err(format!(
                "NVAPI headless recovery journal version {} is unsupported",
                self.version
            ));
        }
        if self.entries.is_empty() || self.entries.len() > arcen_media::MAX_MULTI_MONITOR_COUNT {
            return Err(format!(
                "NVAPI headless recovery journal must contain 1..={} entries",
                arcen_media::MAX_MULTI_MONITOR_COUNT
            ));
        }
        let mut display_ids = std::collections::BTreeSet::new();
        let mut outputs = Vec::new();
        for entry in &self.entries {
            entry.validate()?;
            if !display_ids.insert(entry.display_id)
                || outputs.contains(&(entry.adapter_luid, entry.output_id))
            {
                return Err("NVAPI headless recovery journal repeats an output".into());
            }
            outputs.push((entry.adapter_luid, entry.output_id));
        }
        Ok(())
    }
}
fn default_journal_path() -> PathBuf {
    if let Some(path) = std::env::var_os("ARCEN_NVAPI_HEADLESS_RECOVERY_JOURNAL") {
        return PathBuf::from(path);
    }
    crate::paths::agent_runtime_dir().join("nvapi-headless-recovery.json")
}

pub(crate) fn restore(journal: Option<PathBuf>) -> Result<(), String> {
    let path = journal.unwrap_or_else(default_journal_path);
    if !path.exists() {
        return Ok(());
    }
    restore_from_path(&path)
}

fn write_journal(path: &Path, journal: &RecoveryJournal) -> Result<(), String> {
    journal.validate()?;
    let payload = serde_json::to_vec_pretty(journal)
        .map_err(|error| format!("serialize NVAPI headless recovery journal: {error}"))?;
    crate::recovery::write_atomic_bytes(
        path,
        &payload,
        MAX_JOURNAL_BYTES,
        "NVAPI headless recovery journal",
    )
}

fn read_journal(path: &Path) -> Result<RecoveryJournal, String> {
    crate::recovery::reject_reparse_point(path, "NVAPI headless recovery journal")?;
    let metadata = std::fs::metadata(path)
        .map_err(|error| format!("stat NVAPI headless recovery journal {path:?}: {error}"))?;
    if metadata.len() > MAX_JOURNAL_BYTES {
        return Err(format!(
            "NVAPI headless recovery journal exceeds {MAX_JOURNAL_BYTES} bytes"
        ));
    }
    let payload = std::fs::read(path)
        .map_err(|error| format!("read NVAPI headless recovery journal {path:?}: {error}"))?;
    let journal: RecoveryJournal = serde_json::from_slice(&payload)
        .map_err(|error| format!("parse NVAPI headless recovery journal {path:?}: {error}"))?;
    journal.validate()?;
    Ok(journal)
}

fn mark_mutation_started(path: &Path) -> Result<(), String> {
    let mut journal = read_journal(path)?;
    if !journal.mutation_started {
        journal.mutation_started = true;
        write_journal(path, &journal)?;
    }
    Ok(())
}

fn selected_spare(
    report: &NvapiInventoryReport,
    display_id: u32,
) -> Result<(&GpuEntry, &DisplayIdEntry, AdapterLuid, u32), String> {
    let mut matches = report.gpus.iter().flat_map(|gpu| {
        gpu.displays
            .iter()
            .filter(move |display| display.display_id == display_id)
            .map(move |display| (gpu, display))
    });
    let Some((gpu, display)) = matches.next() else {
        return Err(format!(
            "NVAPI inventory does not contain display id 0x{display_id:08x}"
        ));
    };
    if matches.next().is_some() {
        return Err(format!(
            "NVAPI inventory repeats display id 0x{display_id:08x}"
        ));
    }
    if display.flags.active || display.flags.connected || display.edid.byte_length != 0 {
        return Err(format!(
            "display id 0x{display_id:08x} is not an empty inactive spare target"
        ));
    }
    if display.in_nvapi_display_config {
        return Err(format!(
            "display id 0x{display_id:08x} is already in the NVAPI display configuration"
        ));
    }
    let adapter_luid = gpu
        .adapter_luid
        .ok_or_else(|| format!("display id 0x{display_id:08x} has no NVIDIA adapter LUID"))?;
    let output_id = display.output_id.ok_or_else(|| {
        format!("display id 0x{display_id:08x} has no addressable NVIDIA output ID")
    })?;
    if output_id.count_ones() != 1 {
        return Err(format!(
            "display id 0x{display_id:08x} has invalid output ID 0x{output_id:08x}"
        ));
    }
    Ok((gpu, display, adapter_luid, output_id))
}

fn state_for(
    report: &NvapiInventoryReport,
    display_id: u32,
    adapter_luid: AdapterLuid,
    output_id: u32,
) -> Result<ProbeState, String> {
    let (_, display, current_adapter_luid, current_output_id) =
        selected_display(report, display_id)?;
    if current_adapter_luid != adapter_luid || current_output_id != output_id {
        return Err(format!(
            "display id 0x{display_id:08x} changed adapter/output identity during the probe"
        ));
    }
    let mut ccd_available_target_ids = report
        .ccd_paths
        .iter()
        .filter(|path| path.target_adapter_luid == adapter_luid && path.target_available)
        .map(|path| path.target_id)
        .collect::<Vec<_>>();
    ccd_available_target_ids.sort_unstable();
    ccd_available_target_ids.dedup();
    Ok(ProbeState {
        connected: display.flags.connected,
        active: display.flags.active,
        os_visible: display.flags.os_visible,
        edid_status: display.edid.status,
        edid_sha256: display.edid.sha256.clone(),
        edid_manufacturer: display.edid.manufacturer.clone(),
        edid_preferred_width: display.edid.preferred_width,
        edid_preferred_height: display.edid.preferred_height,
        ccd_available_target_ids,
    })
}

fn selected_display(
    report: &NvapiInventoryReport,
    display_id: u32,
) -> Result<(&GpuEntry, &DisplayIdEntry, AdapterLuid, u32), String> {
    let mut matches = report.gpus.iter().flat_map(|gpu| {
        gpu.displays
            .iter()
            .filter(move |display| display.display_id == display_id)
            .map(move |display| (gpu, display))
    });
    let Some((gpu, display)) = matches.next() else {
        return Err(format!(
            "NVAPI inventory lost display id 0x{display_id:08x}"
        ));
    };
    if matches.next().is_some() {
        return Err(format!(
            "NVAPI inventory repeats display id 0x{display_id:08x}"
        ));
    }
    let adapter_luid = gpu
        .adapter_luid
        .ok_or_else(|| format!("display id 0x{display_id:08x} has no NVIDIA adapter LUID"))?;
    let output_id = display
        .output_id
        .ok_or_else(|| format!("display id 0x{display_id:08x} lost its NVIDIA output ID"))?;
    Ok((gpu, display, adapter_luid, output_id))
}

fn activation_matches(
    baseline: &ProbeState,
    current: &ProbeState,
    intended_sha256: &str,
    width: u32,
    height: u32,
) -> bool {
    current.connected
        && current.edid_sha256.as_deref() == Some(intended_sha256)
        && current.edid_manufacturer.as_deref() == Some("ARN")
        && current.edid_preferred_width == Some(width)
        && current.edid_preferred_height == Some(height)
        && current.ccd_available_target_ids.len() > baseline.ccd_available_target_ids.len()
}

fn matching_gpu<'a>(
    report: &'a NvapiInventoryReport,
    adapter_name: &str,
) -> Result<&'a GpuEntry, String> {
    fn without_nvidia_prefix(value: &str) -> &str {
        value
            .get(..7)
            .filter(|prefix| prefix.eq_ignore_ascii_case("NVIDIA "))
            .map_or(value, |_| &value[7..])
    }

    let mut matches = report.gpus.iter().filter(|gpu| {
        gpu.full_name.as_deref().is_some_and(|name| {
            name.eq_ignore_ascii_case(adapter_name)
                || without_nvidia_prefix(name)
                    .eq_ignore_ascii_case(without_nvidia_prefix(adapter_name))
        })
    });
    let Some(gpu) = matches.next() else {
        return Err(format!("NVAPI inventory has no GPU named {adapter_name:?}"));
    };
    if matches.next().is_some() {
        return Err(format!("NVAPI GPU name {adapter_name:?} is ambiguous"));
    }
    Ok(gpu)
}

fn desired_display_ids(
    report: &NvapiInventoryReport,
    gpu: &GpuEntry,
    target_count: usize,
    preferred_display_id: Option<u32>,
) -> Result<(Vec<u32>, Vec<u32>), String> {
    if !(1..=arcen_media::MAX_MULTI_MONITOR_COUNT).contains(&target_count) {
        return Err(format!(
            "NVIDIA headless target count must be 1..={}",
            arcen_media::MAX_MULTI_MONITOR_COUNT
        ));
    }
    let mut connected = gpu
        .displays
        .iter()
        .filter(|display| display.flags.connected)
        .collect::<Vec<_>>();
    connected.sort_by_key(|display| {
        (
            report.gdi_primary_display_id != Some(display.display_id),
            std::cmp::Reverse(display.output_id.unwrap_or(0)),
        )
    });
    let mandatory = connected
        .iter()
        .copied()
        .filter(|display| !display.edid.written_by_arcen && display.edid.byte_length != 0)
        .collect::<Vec<_>>();
    if mandatory.len() > target_count {
        return Err(format!(
            "GPU {} has {} connected non-Arcen monitors, exceeding requested count {target_count}",
            gpu.full_name.as_deref().unwrap_or("<unnamed>"),
            mandatory.len()
        ));
    }

    let mut keep = mandatory
        .iter()
        .map(|display| display.display_id)
        .collect::<Vec<_>>();
    if let Some(display_id) = preferred_display_id {
        let preferred = gpu
            .displays
            .iter()
            .find(|display| display.display_id == display_id)
            .ok_or_else(|| {
                format!(
                    "configured NVIDIA display id 0x{display_id:08x} is not on the selected GPU"
                )
            })?;
        if preferred.output_id.is_none() {
            return Err(format!(
                "configured NVIDIA display id 0x{display_id:08x} has no addressable output"
            ));
        }
        if keep.len() < target_count && !keep.contains(&display_id) {
            keep.push(display_id);
        }
    }
    for display in &connected {
        if keep.len() == target_count {
            break;
        }
        if display.edid.byte_length != 0 && !keep.contains(&display.display_id) {
            keep.push(display.display_id);
        }
    }
    if keep.len() < target_count {
        let mut spares = gpu
            .displays
            .iter()
            .filter(|display| {
                !keep.contains(&display.display_id)
                    && display.output_id.is_some()
                    && (display.edid.byte_length == 0 || display.edid.written_by_arcen)
            })
            .collect::<Vec<_>>();
        spares.sort_by_key(|display| std::cmp::Reverse(display.output_id.unwrap_or(0)));
        for display in spares {
            if keep.len() == target_count {
                break;
            }
            keep.push(display.display_id);
        }
    }
    if keep.len() != target_count {
        return Err(format!(
            "GPU {} exposes {} usable display IDs, fewer than requested {target_count}",
            gpu.full_name.as_deref().unwrap_or("<unnamed>"),
            keep.len()
        ));
    }
    let remove = gpu
        .displays
        .iter()
        .filter(|display| !keep.contains(&display.display_id))
        .filter(|display| {
            display.edid.written_by_arcen
                || (display.flags.connected && display.edid.byte_length == 0)
        })
        .map(|display| display.display_id)
        .collect::<Vec<_>>();
    Ok((keep, remove))
}

#[cfg(windows)]
pub(crate) fn prepare_provisioning(
    adapter_name: &str,
    contracts: &[HeadlessDisplayContract],
) -> Result<PreparedEdidProvision, String> {
    use crate::nvapi::NvapiDriver as _;
    use sha2::{Digest as _, Sha256};

    if contracts.is_empty() || contracts.len() > arcen_media::MAX_MULTI_MONITOR_COUNT {
        return Err(format!(
            "NVIDIA headless display contract count must be 1..={}",
            arcen_media::MAX_MULTI_MONITOR_COUNT
        ));
    }
    let mut contracts = contracts.to_vec();
    contracts.sort_by_key(|contract| !contract.primary);
    if contracts.iter().filter(|contract| contract.primary).count() != 1 {
        return Err("NVIDIA headless display contracts require exactly one primary".to_string());
    }
    for contract in &contracts {
        crate::display::DisplaySize::validate(contract.width, contract.height)?;
        if contract.refresh_hz == 0 {
            return Err("NVIDIA headless display refresh must be nonzero".to_string());
        }
    }

    let report = crate::nvapi_inventory::inventory()?;
    let gpu = matching_gpu(&report, adapter_name)?;
    let adapter_luid = gpu
        .adapter_luid
        .ok_or_else(|| format!("GPU {adapter_name:?} has no adapter LUID"))?;
    let target_count = contracts.len();
    let preferred_display_id = contracts
        .iter()
        .find(|contract| contract.primary)
        .and_then(|contract| contract.preferred_output_index)
        .and_then(|index| {
            let mut connected = gpu
                .displays
                .iter()
                .filter(|display| display.flags.connected && display.edid.byte_length != 0)
                .collect::<Vec<_>>();
            connected.sort_by_key(|display| std::cmp::Reverse(display.output_id.unwrap_or(0)));
            connected
                .get(index as usize)
                .map(|display| display.display_id)
        });
    let (keep, remove) = desired_display_ids(&report, gpu, target_count, preferred_display_id)?;

    let mut driver = crate::nvapi::Nvapi::load()?;
    let mut changes = Vec::with_capacity(keep.len() + remove.len());
    let mut expected = Vec::with_capacity(keep.len());
    let mut recovery_entries = Vec::with_capacity(keep.len() + remove.len());

    for (display_id, contract) in keep.iter().copied().zip(&contracts) {
        let display = gpu
            .displays
            .iter()
            .find(|display| display.display_id == display_id)
            .expect("selected display came from this GPU");
        if display.flags.connected && !display.edid.written_by_arcen {
            continue;
        }
        let output_id = display
            .output_id
            .ok_or_else(|| format!("display id 0x{display_id:08x} has no output ID"))?;
        let mapping = driver.map_headless_display_id(display_id, adapter_luid)?;
        if mapping.output_id != output_id {
            return Err(format!(
                "display id 0x{display_id:08x} changed output identity during preparation"
            ));
        }
        let original_edid = driver.get_edid(mapping)?;
        let request = crate::edid::EdidRequest {
            width: contract.width,
            height: contract.height,
            refresh_hz: contract.refresh_hz,
            width_mm: contract.width_mm,
            height_mm: contract.height_mm,
            scale: crate::display::edid_scale_ratio(contract.scale).unwrap_or(1.0),
            product_id: contract.product_id,
            serial: contract.serial,
        };
        let desired_edid = if contract.hdr10 {
            crate::edid::generate_hdr10(request)?.to_vec()
        } else {
            crate::edid::generate(request)?.to_vec()
        };
        let intended_edid_sha256 = format!("{:x}", Sha256::digest(&desired_edid));
        let recovery = HeadlessEdidRecovery {
            display_id,
            output_id,
            adapter_luid,
            original_edid: original_edid.clone(),
            intended_edid_sha256: intended_edid_sha256.clone(),
        };
        expected.push(ExpectedHeadlessDisplay {
            display_id,
            edid_sha256: intended_edid_sha256,
            width: contract.width,
            height: contract.height,
            hdr10: contract.hdr10,
        });
        recovery_entries.push(recovery.clone());
        if original_edid.as_deref() != Some(desired_edid.as_slice())
            || !display.flags.connected
            || !display.flags.active
        {
            changes.push(EdidProvisionChange {
                recovery,
                desired_edid: Some(desired_edid),
            });
        }
    }

    for display_id in remove {
        let display = gpu
            .displays
            .iter()
            .find(|display| display.display_id == display_id)
            .expect("removed display came from this GPU");
        let output_id = display
            .output_id
            .ok_or_else(|| format!("display id 0x{display_id:08x} has no output ID"))?;
        let mapping = driver.map_headless_display_id(display_id, adapter_luid)?;
        if mapping.output_id != output_id {
            return Err(format!(
                "display id 0x{display_id:08x} changed output identity during removal preparation"
            ));
        }
        let original_edid = driver.get_edid(mapping)?;
        let recovery = HeadlessEdidRecovery {
            display_id,
            output_id,
            adapter_luid,
            original_edid,
            intended_edid_sha256: format!("{:x}", Sha256::digest(b"")),
        };
        recovery_entries.push(recovery.clone());
        changes.push(EdidProvisionChange {
            recovery,
            desired_edid: None,
        });
    }

    Ok(PreparedEdidProvision {
        adapter_name: adapter_name.to_string(),
        adapter_luid,
        target_count,
        changes,
        expected,
        recovery_entries,
    })
}

#[cfg(not(windows))]
pub(crate) fn prepare_provisioning(
    _adapter_name: &str,
    _contracts: &[HeadlessDisplayContract],
) -> Result<PreparedEdidProvision, String> {
    Err("NVIDIA headless provisioning is available only on Windows".into())
}

#[cfg(windows)]
pub(crate) fn apply_provisioning(prepared: &PreparedEdidProvision) -> Result<(), String> {
    use crate::nvapi::NvapiDriver as _;

    let mut driver = crate::nvapi::Nvapi::load()?;
    for change in &prepared.changes {
        let mapping = driver
            .map_headless_display_id(change.recovery.display_id, change.recovery.adapter_luid)?;
        if mapping.output_id != change.recovery.output_id {
            return Err(format!(
                "display id 0x{:08x} changed output identity before EDID apply",
                change.recovery.display_id
            ));
        }
        if change.desired_edid.is_some()
            && change.recovery.original_edid.as_deref() == change.desired_edid.as_deref()
        {
            driver.set_edid(mapping, &[])?;
        }
        driver.set_edid(mapping, change.desired_edid.as_deref().unwrap_or_default())?;
        let current = driver.get_edid(mapping)?;
        if current.as_deref() != change.desired_edid.as_deref() {
            return Err(format!(
                "display id 0x{:08x} EDID did not read back",
                change.recovery.display_id
            ));
        }
        tracing::debug!(
            target: crate::logging::DISPLAY,
            display_id = format_args!("0x{:08x}", change.recovery.display_id),
            output_id = format_args!("0x{:08x}", change.recovery.output_id),
            edid_bytes = change.desired_edid.as_ref().map_or(0, Vec::len),
            "NVIDIA headless EDID change read back before enumeration settle"
        );
    }
    let requested_display_ids = prepared
        .expected
        .iter()
        .map(|display| display.display_id)
        .collect::<Vec<_>>();
    driver
        .activate_extended_displays(&requested_display_ids)
        .map_err(|error| format!("activate exact NVIDIA headless display set: {error}"))?;
    tracing::info!(
        target: crate::logging::DISPLAY,
        requested_display_ids = ?requested_display_ids,
        "exact NVIDIA headless display set activated before Windows topology planning"
    );
    // The GRID driver can keep an emptied head marked connected until the
    // topology no longer references it. Clear removed heads once more after
    // the exact topology commit so the connector state itself converges, not
    // only its active bit and EDID byte count.
    for change in prepared
        .changes
        .iter()
        .filter(|change| change.desired_edid.is_none())
    {
        let mapping = driver
            .map_headless_display_id(change.recovery.display_id, change.recovery.adapter_luid)?;
        driver.set_edid(mapping, &[])?;
        tracing::debug!(
            target: crate::logging::DISPLAY,
            display_id = format_args!("0x{:08x}", change.recovery.display_id),
            "removed NVIDIA head cleared again after exact topology activation"
        );
    }
    wait_for_provisioned_state(prepared)?;

    let hdr_display_ids = prepared
        .expected
        .iter()
        .filter(|display| display.hdr10)
        .map(|display| display.display_id)
        .collect::<Vec<_>>();
    if !hdr_display_ids.is_empty() {
        tracing::info!(
            target: crate::logging::DISPLAY,
            display_ids = ?hdr_display_ids,
            "HDR EDIDs are active; exact-target HDR engagement is deferred until the final \
             session display lease is bound"
        );
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn apply_provisioning(_prepared: &PreparedEdidProvision) -> Result<(), String> {
    Err("NVIDIA headless provisioning is available only on Windows".into())
}

/// One display carrying an Arcen-written EDID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ArcenEdidDisplay {
    pub adapter: String,
    pub display_id: u32,
    pub output_id: u32,
    pub in_display_config: bool,
    pub product_code: Option<u16>,
}

/// What a clear run did, or would do.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub(crate) struct ClearOutcome {
    pub dry_run: bool,
    pub cleared: Vec<ArcenEdidDisplay>,
    /// Left in place to satisfy the non-headless invariant, or because the
    /// caller named a different display.
    pub kept: Vec<ArcenEdidDisplay>,
}

/// Which Arcen-written EDIDs to remove.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ClearRequest {
    /// Remove only this display id. `None` removes every one it may.
    pub display_id: Option<u32>,
    /// Restrict to displays on the adapter whose name contains this, when the
    /// caller passes one through [`ClearRequest::adapter_filter`].
    pub dry_run: bool,
}

impl ClearRequest {
    pub(crate) const fn adapter_filter(self) -> Option<&'static str> {
        None
    }
}

/// Selects the Arcen-written EDIDs a clear run may remove.
///
/// **Enforces ADR 0009's non-headless invariant.** A Windows host with no
/// active display cannot present LogonUI, so the credential provider has
/// nothing to draw on and the machine is unreachable without a hypervisor
/// console. At least one display carrying an Arcen EDID and present in the
/// display configuration is therefore always kept, even when the caller asked
/// for everything.
///
/// Pure so the invariant is testable without a GPU.
pub(crate) fn plan_clear(
    displays: &[ArcenEdidDisplay],
    request: ClearRequest,
) -> (Vec<ArcenEdidDisplay>, Vec<ArcenEdidDisplay>) {
    let mut cleared = Vec::new();
    let mut kept = Vec::new();

    // The display that must survive: prefer one Windows is actually using, so
    // clearing never trades a live desktop for a spare.
    let survivor = displays
        .iter()
        .position(|display| display.in_display_config)
        .or(if displays.is_empty() { None } else { Some(0) });

    for (index, display) in displays.iter().enumerate() {
        let named = request
            .display_id
            .is_none_or(|wanted| wanted == display.display_id);
        if !named || survivor == Some(index) {
            kept.push(display.clone());
        } else {
            cleared.push(display.clone());
        }
    }
    (cleared, kept)
}

#[cfg(windows)]
pub(crate) fn arcen_edid_displays() -> Result<Vec<ArcenEdidDisplay>, String> {
    let report = crate::nvapi_inventory::inventory()?;
    let mut displays = Vec::new();
    for gpu in &report.gpus {
        for display in &gpu.displays {
            if !display.edid.written_by_arcen {
                continue;
            }
            let Some(output_id) = display.output_id else {
                continue;
            };
            displays.push(ArcenEdidDisplay {
                adapter: gpu.full_name.clone().unwrap_or_else(|| "unknown".into()),
                display_id: display.display_id,
                output_id,
                in_display_config: display.in_nvapi_display_config,
                product_code: display.edid.product_code,
            });
        }
    }
    Ok(displays)
}

/// Removes Arcen-written EDIDs, keeping at least one so the host stays
/// reachable. Verifies each removal by reading the EDID back.
#[cfg(windows)]
pub(crate) fn clear_arcen_edids(request: ClearRequest) -> Result<ClearOutcome, String> {
    use crate::nvapi::NvapiDriver as _;

    // A pending journal describes a topology captured before this call. Any
    // EDID removed underneath it changes the bindings that journal replays
    // against, and the restore then fails with an ambiguous-path error that
    // cannot be resolved without knowing the old topology. Refuse, exactly as
    // the headless probe does, rather than make a recoverable state
    // unrecoverable. A dry run reads nothing and is always safe.
    if !request.dry_run {
        let display_journal = crate::recovery::default_path();
        if display_journal.exists() {
            return Err(format!(
                "refusing to clear EDIDs while display recovery journal {display_journal:?} \
                 exists; run `arcen-pier restore-display` in the console session first"
            ));
        }
    }

    let displays = arcen_edid_displays()?;
    let (cleared, kept) = plan_clear(&displays, request);
    if request.dry_run {
        return Ok(ClearOutcome {
            dry_run: true,
            cleared,
            kept,
        });
    }

    let report = crate::nvapi_inventory::inventory()?;
    let mut driver = crate::nvapi::Nvapi::load()?;
    let mut done = Vec::new();
    for display in &cleared {
        let adapter_luid = report
            .gpus
            .iter()
            .find(|gpu| {
                gpu.displays
                    .iter()
                    .any(|entry| entry.display_id == display.display_id)
            })
            .and_then(|gpu| gpu.adapter_luid)
            .ok_or_else(|| {
                format!(
                    "display id 0x{:08x} has no adapter LUID",
                    display.display_id
                )
            })?;
        let mapping = driver.map_headless_display_id(display.display_id, adapter_luid)?;
        if mapping.output_id != display.output_id {
            return Err(format!(
                "display id 0x{:08x} changed output identity before clear",
                display.display_id
            ));
        }
        driver.set_edid(mapping, &[])?;
        // Read back rather than trust the call: a silent no-op here would
        // leave the operator believing the host was cleaned.
        if let Some(remaining) = driver.get_edid(mapping)? {
            let probe = crate::nvapi_inventory::summarize_edid(&remaining);
            if probe.written_by_arcen {
                return Err(format!(
                    "display id 0x{:08x} still carries an Arcen EDID after clear",
                    display.display_id
                ));
            }
        }
        done.push(display.clone());
    }
    Ok(ClearOutcome {
        dry_run: false,
        cleared: done,
        kept,
    })
}

/// Writes a persistent Arcen EDID to an inactive NVIDIA display id.
///
/// The inverse of [`clear_arcen_edids`], and the command that was missing when
/// that one shipped. Removing a display changes enumeration, so a host pinned
/// to `platform.desktop.output` can end up resolving nothing -- and with no
/// way to put a display back, the only recovery was a reboot that fixed the
/// symptom and not the topology.
///
/// # Errors
///
/// Returns a message when the display id is unknown, already carries an EDID
/// this host wrote, or the write does not read back.
#[cfg(windows)]
pub(crate) fn provision_arcen_edid(
    display_id: u32,
    width: u32,
    height: u32,
    refresh_hz: u32,
    hdr10: bool,
) -> Result<(), String> {
    use crate::nvapi::NvapiDriver as _;

    let journal = crate::recovery::default_path();
    if journal.exists() {
        return Err(format!(
            "refusing to provision while display recovery journal {journal:?} exists; \
             run `arcen-pier restore-display` in the console session first"
        ));
    }
    let report = crate::nvapi_inventory::inventory()?;
    let (adapter_luid, output_id) = report
        .gpus
        .iter()
        .find_map(|gpu| {
            gpu.displays
                .iter()
                .find(|entry| entry.display_id == display_id)
                .and_then(|entry| Some((gpu.adapter_luid?, entry.output_id?)))
        })
        .ok_or_else(|| format!("display id 0x{display_id:08x} is not in the NVAPI inventory"))?;

    let edid_request = crate::edid::EdidRequest {
        width,
        height,
        refresh_hz: refresh_hz.max(1),
        width_mm: 0.0,
        height_mm: 0.0,
        scale: 1.0,
        product_id: 0x0001,
        serial: 0,
    };
    // The persistent counterpart to the session-time EDID choice. A session
    // applies HDR10 only while a Deck asks for PQ and takes it away again
    // afterwards, which means an operator logging in at the console sees an
    // SDR display and no "Use HDR" toggle in Windows Display Settings --
    // Windows only offers HDR where the sink's EDID claims it. Provisioning
    // the HDR10 EDID makes that claim permanent, so HDR is visible and
    // selectable outside a session and can be verified independently of
    // Arcen.
    let edid = if hdr10 {
        crate::edid::generate_hdr10(edid_request)?.to_vec()
    } else {
        crate::edid::generate(edid_request)?.to_vec()
    };

    let mut driver = crate::nvapi::Nvapi::load()?;
    let mapping = driver.map_headless_display_id(display_id, adapter_luid)?;
    if mapping.output_id != output_id {
        return Err(format!(
            "display id 0x{display_id:08x} changed output identity before provisioning"
        ));
    }
    driver.set_edid(mapping, &edid)?;
    // Read back rather than trust the call, for the same reason the clear does.
    let written = driver
        .get_edid(mapping)?
        .ok_or_else(|| format!("display id 0x{display_id:08x} reports no EDID after write"))?;
    if !crate::nvapi_inventory::summarize_edid(&written).written_by_arcen {
        return Err(format!(
            "display id 0x{display_id:08x} did not accept the Arcen EDID"
        ));
    }
    Ok(())
}

#[cfg(not(windows))]
pub(crate) fn provision_arcen_edid(
    _display_id: u32,
    _width: u32,
    _height: u32,
    _refresh_hz: u32,
    _hdr10: bool,
) -> Result<(), String> {
    Err("provisioning an Arcen EDID is available only on Windows".into())
}

#[cfg(not(windows))]
pub(crate) fn clear_arcen_edids(_request: ClearRequest) -> Result<ClearOutcome, String> {
    Err("clearing Arcen EDIDs is available only on Windows".into())
}

#[cfg(windows)]
fn wait_for_provisioned_state(prepared: &PreparedEdidProvision) -> Result<(), String> {
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    let last = loop {
        let last = match crate::nvapi_inventory::inventory() {
            Ok(report) => {
                let gpu = matching_gpu(&report, &prepared.adapter_name)?;
                if gpu.adapter_luid != Some(prepared.adapter_luid) {
                    return Err(
                        "NVIDIA streaming adapter identity changed during provisioning".into(),
                    );
                }
                let connected = gpu
                    .displays
                    .iter()
                    .filter(|display| display.flags.connected)
                    .count();
                let active = gpu
                    .displays
                    .iter()
                    .filter(|display| display.flags.active)
                    .count();
                let expected_match = prepared.expected.iter().all(|expected| {
                    gpu.displays
                        .iter()
                        .find(|display| display.display_id == expected.display_id)
                        .is_some_and(|display| {
                            display.flags.connected
                                && display.flags.active
                                && display.edid.sha256.as_deref()
                                    == Some(expected.edid_sha256.as_str())
                                && display.edid.preferred_width == Some(expected.width)
                                && display.edid.preferred_height == Some(expected.height)
                        })
                });
                let removals_match = prepared
                    .changes
                    .iter()
                    .filter(|change| change.desired_edid.is_none())
                    .all(|change| {
                        gpu.displays
                            .iter()
                            .find(|display| display.display_id == change.recovery.display_id)
                            .is_some_and(|display| {
                                !display.flags.active && display.edid.byte_length == 0
                            })
                    });
                let connected_ids = gpu
                    .displays
                    .iter()
                    .filter(|display| display.flags.connected)
                    .map(|display| format!("0x{:08x}", display.display_id))
                    .collect::<Vec<_>>()
                    .join(",");
                let pending_removals = prepared
                    .changes
                    .iter()
                    .filter(|change| change.desired_edid.is_none())
                    .filter_map(|change| {
                        gpu.displays
                            .iter()
                            .find(|display| display.display_id == change.recovery.display_id)
                            .filter(|display| {
                                display.flags.connected || display.edid.byte_length != 0
                            })
                            .map(|display| {
                                format!(
                                    "0x{:08x}(connected={},active={},bytes={})",
                                    display.display_id,
                                    display.flags.connected,
                                    display.flags.active,
                                    display.edid.byte_length
                                )
                            })
                    })
                    .collect::<Vec<_>>()
                    .join(",");
                if active == prepared.target_count && expected_match && removals_match {
                    if connected != prepared.target_count {
                        tracing::debug!(
                            target: crate::logging::DISPLAY,
                            driver_connected = connected,
                            active,
                            requested = prepared.target_count,
                            "NVIDIA retains connector presence on removed heads; inactive zero-EDID heads are not part of the session display set"
                        );
                    }
                    return Ok(());
                }
                format!(
                    "connected={connected}, active={active}, target={}, expected_match={expected_match}, removals_match={removals_match}, connected_ids=[{connected_ids}], pending_removals=[{pending_removals}]",
                    prepared.target_count,
                )
            }
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            break last;
        }
        std::thread::sleep(SETTLE_POLL);
    };
    Err(format!(
        "NVIDIA headless outputs did not settle within {}ms ({last})",
        SETTLE_TIMEOUT.as_millis()
    ))
}

/// The rollback-direction mirror of [`wait_for_provisioned_state`].
///
/// [`restore_edid`] only waits for NVAPI's own EDID store to read the original
/// bytes back, which settles almost immediately because it never leaves the
/// driver. The consequences that actually decide whether rollback finished --
/// NVIDIA marking the display disconnected, and Windows CCD dropping the
/// target from its available set -- are asynchronous. ADR 0008 measured a
/// single target taking ~1s to return to its empty baseline.
///
/// Without this wait the caller re-applies the original topology and verifies
/// it while the headless targets are still enumerated. That produces both
/// failure modes ADR 0008 records: the topology verification rejects a
/// still-converging desktop, and because the verification failed the journal
/// is never removed, leaving an Arcen-authored EDID connected but inactive
/// behind a stranded recovery journal.
///
/// The per-entry predicate is ADR 0008's own acceptance criterion: every
/// display ID Arcen added is disconnected with no EDID, and every display ID
/// Arcen overwrote carries its original EDID again.
#[cfg(windows)]
fn wait_for_restored_state(entries: &[HeadlessEdidRecovery]) -> Result<(), String> {
    let started = Instant::now();
    let deadline = started + SETTLE_TIMEOUT;
    let last = loop {
        let last = match crate::nvapi_inventory::inventory() {
            Ok(report) => {
                let mut pending = Vec::new();
                for entry in entries {
                    match restored_entry_matches(&report, entry) {
                        Ok(true) => {}
                        Ok(false) => {
                            pending.push(format!("0x{:08x} not settled", entry.display_id));
                        }
                        Err(error) => pending.push(format!("0x{:08x}: {error}", entry.display_id)),
                    }
                }
                if pending.is_empty() {
                    tracing::info!(
                        target: crate::logging::DISPLAY,
                        entries = entries.len(),
                        elapsed_ms = u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                        "NVIDIA headless EDID rollback settled to the pre-provision baseline"
                    );
                    return Ok(());
                }
                pending.join(", ")
            }
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            break last;
        }
        std::thread::sleep(SETTLE_POLL);
    };
    Err(format!(
        "NVIDIA headless EDID rollback did not settle within {}ms ({last})",
        SETTLE_TIMEOUT.as_millis()
    ))
}

/// Whether one rolled-back display ID has returned to its pre-provision state.
///
/// Pure over an already-captured inventory report, so the rollback acceptance
/// criterion itself is covered by ordinary CI rather than only by the
/// elevated-console lab test.
fn restored_entry_matches(
    report: &NvapiInventoryReport,
    entry: &HeadlessEdidRecovery,
) -> Result<bool, String> {
    use sha2::{Digest as _, Sha256};

    let (_, display, adapter_luid, output_id) = selected_display(report, entry.display_id)?;
    if adapter_luid != entry.adapter_luid || output_id != entry.output_id {
        return Err(format!(
            "adapter/output identity changed to output 0x{output_id:08x}"
        ));
    }
    Ok(match entry.original_edid.as_deref() {
        // A spare target Arcen provisioned from nothing: NVAPI must report it
        // disconnected and inactive with no EDID again.
        None => !display.flags.connected && !display.flags.active && display.edid.byte_length == 0,
        // A target that already carried an EDID: the exact original must be back.
        Some(original) => {
            let expected = format!("{:x}", Sha256::digest(original));
            display.flags.connected
                && display.edid.byte_length == original.len()
                && display.edid.sha256.as_deref() == Some(expected.as_str())
        }
    })
}

fn render_summary(report: &ProbeReport) -> String {
    format!(
        "Arcen NVAPI headless activation proof\n\
         display=0x{:08x} output=0x{:08x} adapter={:08x}:{:08x}\n\
         mode={}x{}@{} edid_sha256={}\n\
         activation={}ms available_targets={:?}\n\
         restore={}ms rollback_verified={}\n",
        report.display_id,
        report.output_id,
        report.adapter_luid.high_part,
        report.adapter_luid.low_part,
        report.width,
        report.height,
        report.refresh_hz,
        report.intended_edid_sha256,
        report.activation_elapsed_ms,
        report.activated.ccd_available_target_ids,
        report.restore_elapsed_ms,
        report.rollback_verified,
    )
}

#[cfg(windows)]
struct ArmedEdid {
    driver: crate::nvapi::Nvapi,
    mapping: DisplayMapping,
    original_edid: Option<Vec<u8>>,
    journal: PathBuf,
    armed: bool,
}

#[cfg(windows)]
impl ArmedEdid {
    fn restore_edid(&mut self) -> Result<(), String> {
        restore_edid(
            &mut self.driver,
            self.mapping,
            self.original_edid.as_deref(),
        )?;
        self.armed = false;
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for ArmedEdid {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        match self.restore_edid() {
            Ok(()) => {
                let _ =
                    crate::recovery::remove_file(&self.journal, "NVAPI headless recovery journal");
            }
            Err(error) => tracing::error!(
                target: crate::logging::DISPLAY,
                display_id = format_args!("0x{:08x}", self.mapping.display_id),
                %error,
                journal = %self.journal.display(),
                "scoped NVAPI headless EDID rollback failed; watchdog journal remains armed"
            ),
        }
    }
}

#[cfg(windows)]
fn restore_edid(
    driver: &mut impl crate::nvapi::NvapiDriver,
    mapping: DisplayMapping,
    original_edid: Option<&[u8]>,
) -> Result<(), String> {
    driver
        .set_edid(mapping, &[])
        .map_err(|error| format!("purge temporary headless EDID: {error}"))?;
    if let Some(original) = original_edid {
        let current = driver
            .get_edid(mapping)
            .map_err(|error| format!("read EDID after purge: {error}"))?;
        if current.as_deref() != Some(original) {
            driver
                .set_edid(mapping, original)
                .map_err(|error| format!("restore original EDID: {error}"))?;
        }
    }
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    loop {
        match driver.get_edid(mapping) {
            Ok(current) if current.as_deref() == original_edid => return Ok(()),
            Ok(_) | Err(_) if Instant::now() < deadline => std::thread::sleep(SETTLE_POLL),
            Ok(current) => {
                return Err(format!(
                    "EDID rollback settled at {:?} bytes instead of {:?}",
                    current.as_ref().map(Vec::len),
                    original_edid.map(<[u8]>::len)
                ));
            }
            Err(error) => return Err(format!("verify EDID rollback: {error}")),
        }
    }
}

#[cfg(windows)]
fn wait_for_state(
    display_id: u32,
    adapter_luid: AdapterLuid,
    output_id: u32,
    mut matches: impl FnMut(&ProbeState) -> bool,
) -> Result<(ProbeState, u128), String> {
    let started = Instant::now();
    let deadline = started + SETTLE_TIMEOUT;
    let last = loop {
        let last = match crate::nvapi_inventory::inventory()
            .and_then(|report| state_for(&report, display_id, adapter_luid, output_id))
        {
            Ok(state) if matches(&state) => {
                return Ok((state, started.elapsed().as_millis()));
            }
            Ok(state) => format!("{state:?}"),
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            break last;
        }
        std::thread::sleep(SETTLE_POLL);
    };
    Err(format!(
        "NVAPI headless state did not settle within {}ms ({last})",
        SETTLE_TIMEOUT.as_millis()
    ))
}

#[cfg(windows)]
pub(crate) fn probe(
    request: ProbeRequest,
    owner: &arcen_telemetry::CorrelationId,
) -> Result<ProbeReport, String> {
    use crate::nvapi::NvapiDriver as _;
    use sha2::{Digest as _, Sha256};

    if request.display_id == 0
        || request.width == 0
        || request.height == 0
        || request.refresh_hz == 0
        || request.hold_ms > MAX_HOLD_MS
    {
        return Err(format!(
            "invalid NVAPI headless probe request; hold-ms must be 0..={MAX_HOLD_MS}"
        ));
    }
    crate::display::require_nvapi_headless_probe_context()?;
    let display_journal = crate::recovery::default_path();
    if display_journal.exists() {
        return Err(format!(
            "refusing NVAPI headless probe while display recovery journal {display_journal:?} exists"
        ));
    }
    let journal_path = request.journal.unwrap_or_else(default_journal_path);
    if journal_path.exists() {
        restore_from_path(&journal_path)?;
    }

    let baseline_report = crate::nvapi_inventory::inventory()?;
    let (_, _, adapter_luid, output_id) = selected_spare(&baseline_report, request.display_id)?;
    let baseline = state_for(
        &baseline_report,
        request.display_id,
        adapter_luid,
        output_id,
    )?;
    let edid_request = crate::edid::EdidRequest {
        width: request.width,
        height: request.height,
        refresh_hz: request.refresh_hz,
        width_mm: 0.0,
        height_mm: 0.0,
        scale: 1.0,
        product_id: (request.display_id & 0xffff) as u16,
        serial: request.display_id,
    };
    // A 256-byte EDID is exactly MAX_EDID_BYTES, so the HDR10 variant is the
    // largest this path can carry and still journal a recovery entry.
    let edid: Vec<u8> = if request.hdr10 {
        crate::edid::generate_hdr10(edid_request)?.to_vec()
    } else {
        crate::edid::generate(edid_request)?.to_vec()
    };
    let intended_edid_sha256 = format!("{:x}", Sha256::digest(&edid));

    let mut driver = crate::nvapi::Nvapi::load()?;
    let mapping = driver.map_headless_display_id(request.display_id, adapter_luid)?;
    if mapping.output_id != output_id {
        return Err(format!(
            "display id 0x{:08x} remapped from output 0x{output_id:08x} to 0x{:08x}",
            request.display_id, mapping.output_id
        ));
    }
    let original_edid = driver.get_edid(mapping)?;
    if original_edid.is_some() {
        return Err(format!(
            "display id 0x{:08x} gained an EDID before the guarded mutation",
            request.display_id
        ));
    }
    let journal = RecoveryJournal {
        version: JOURNAL_VERSION,
        mutation_started: false,
        entries: vec![HeadlessEdidRecovery {
            display_id: request.display_id,
            output_id,
            adapter_luid,
            original_edid: original_edid.clone(),
            intended_edid_sha256: intended_edid_sha256.clone(),
        }],
    };
    write_journal(&journal_path, &journal)?;
    if let Err(error) =
        crate::display::spawn_nvapi_headless_recovery_watchdog(&journal_path, owner.as_str())
    {
        let remove =
            crate::recovery::remove_file(&journal_path, "unarmed NVAPI headless recovery journal");
        return Err(match remove {
            Ok(()) => error,
            Err(remove_error) => format!("{error}; {remove_error}"),
        });
    }
    mark_mutation_started(&journal_path)?;

    let mut armed = ArmedEdid {
        driver,
        mapping,
        original_edid,
        journal: journal_path.clone(),
        armed: true,
    };
    armed
        .driver
        .set_edid(mapping, &edid)
        .map_err(|error| format!("write temporary headless EDID: {error}"))?;
    let readback = armed
        .driver
        .get_edid(mapping)?
        .ok_or_else(|| "temporary headless EDID did not read back".to_string())?;
    if readback != edid {
        return Err("temporary headless EDID readback differs from the intended payload".into());
    }
    let (activated, activation_elapsed_ms) =
        wait_for_state(request.display_id, adapter_luid, output_id, |state| {
            activation_matches(
                &baseline,
                state,
                &intended_edid_sha256,
                request.width,
                request.height,
            )
        })?;
    if request.hold_ms > 0 {
        std::thread::sleep(Duration::from_millis(request.hold_ms));
    }

    armed.restore_edid()?;
    let (restored, restore_elapsed_ms) =
        wait_for_state(request.display_id, adapter_luid, output_id, |state| {
            state == &baseline
        })?;
    crate::recovery::remove_file(&journal_path, "NVAPI headless recovery journal")?;

    Ok(ProbeReport {
        schema_version: 1,
        display_id: request.display_id,
        output_id,
        adapter_luid,
        width: request.width,
        height: request.height,
        refresh_hz: request.refresh_hz,
        intended_edid_sha256,
        activation_elapsed_ms,
        restore_elapsed_ms,
        baseline,
        activated,
        restored,
        rollback_verified: true,
    })
}

#[cfg(not(windows))]
pub(crate) fn probe(
    _request: ProbeRequest,
    _owner: &arcen_telemetry::CorrelationId,
) -> Result<ProbeReport, String> {
    Err("NVAPI headless activation is available only on Windows".into())
}

#[cfg(windows)]
pub(crate) fn restore_from_path(path: &Path) -> Result<(), String> {
    let journal = read_journal(path)?;
    if !journal.mutation_started {
        crate::recovery::remove_file(path, "unmutated NVAPI headless recovery journal")?;
        return Ok(());
    }
    restore_recovery_entries(&journal.entries)?;
    crate::recovery::remove_file(path, "NVAPI headless recovery journal")
}

#[cfg(not(windows))]
pub(crate) fn restore_from_path(_path: &Path) -> Result<(), String> {
    Err("NVAPI headless recovery is available only on Windows".into())
}

#[cfg(windows)]
pub(crate) fn restore_recovery_entries(entries: &[HeadlessEdidRecovery]) -> Result<(), String> {
    let mut driver = crate::nvapi::Nvapi::load()?;
    let mut errors = Vec::new();
    for entry in entries.iter().rev() {
        let result = (|| {
            entry.validate()?;
            let mapping = driver.map_headless_display_id(entry.display_id, entry.adapter_luid)?;
            if mapping.output_id != entry.output_id {
                return Err(format!(
                    "NVAPI headless recovery output changed from 0x{:08x} to 0x{:08x}",
                    entry.output_id, mapping.output_id
                ));
            }
            restore_edid(&mut driver, mapping, entry.original_edid.as_deref())
        })();
        if let Err(error) = result {
            errors.push(format!("display 0x{:08x}: {error}", entry.display_id));
        }
    }
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    // Purging the EDID is not the end of the rollback -- NVAPI and Windows CCD
    // converge afterwards, asynchronously. The caller re-applies and verifies
    // the original topology immediately after this returns, so it must not
    // return until that convergence is observable.
    wait_for_restored_state(entries)
}

#[cfg(not(windows))]
pub(crate) fn restore_recovery_entries(_entries: &[HeadlessEdidRecovery]) -> Result<(), String> {
    Err("NVAPI headless recovery is available only on Windows".into())
}

#[cfg(windows)]
pub(crate) fn run_restore_watchdog(
    parent_handle: isize,
    ready_handle: isize,
    path: PathBuf,
    correlation_id: arcen_telemetry::CorrelationId,
) -> Result<(), String> {
    match crate::recovery::wait_for_watchdog_parent(parent_handle, ready_handle, &path)? {
        crate::recovery::WatchdogWait::Disarmed => Ok(()),
        crate::recovery::WatchdogWait::ParentExited => {
            restore_from_path(&path)?;
            tracing::info!(
                target: crate::logging::DISPLAY,
                sid = %correlation_id,
                journal = %path.display(),
                "NVAPI headless recovery watchdog restored the original EDID state"
            );
            Ok(())
        }
    }
}

#[cfg(not(windows))]
pub(crate) fn run_restore_watchdog(
    _parent_handle: isize,
    _ready_handle: isize,
    _path: PathBuf,
    _correlation_id: arcen_telemetry::CorrelationId,
) -> Result<(), String> {
    Err("NVAPI headless recovery watchdog is available only on Windows".into())
}

pub(crate) fn summary(report: &ProbeReport) -> String {
    render_summary(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nvapi_inventory::{
        CallOutcome, DisplayIdFlags, DisplayIdSource, EdidProbe, OutputEntry,
    };

    fn state(connected: bool, available: &[u32]) -> ProbeState {
        ProbeState {
            connected,
            active: false,
            os_visible: true,
            edid_status: 0,
            edid_sha256: connected.then(|| "ab".repeat(32)),
            edid_manufacturer: connected.then(|| "ARN".to_string()),
            edid_preferred_width: connected.then_some(1920),
            edid_preferred_height: connected.then_some(1080),
            ccd_available_target_ids: available.to_vec(),
        }
    }

    fn display(display_id: u32, output_id: u32, connected: bool) -> DisplayIdEntry {
        DisplayIdEntry {
            display_id,
            connector_type: 5,
            flags: DisplayIdFlags {
                raw: 0,
                active: connected,
                connected,
                os_visible: true,
                ..DisplayIdFlags::default()
            },
            sources: vec![DisplayIdSource::AllDisplayIds],
            output_id: Some(output_id),
            edid: EdidProbe {
                queried: true,
                status: if connected { 0 } else { -121 },
                byte_length: if connected { 128 } else { 0 },
                manufacturer: connected.then(|| "ARN".to_string()),
                written_by_arcen: connected,
                ..EdidProbe::default()
            },
            in_nvapi_display_config: connected,
        }
    }

    fn gpu(displays: Vec<DisplayIdEntry>) -> GpuEntry {
        GpuEntry {
            index: 0,
            full_name: Some("GRID RTX6000-8Q".to_string()),
            gpu_type: Some(2),
            system_type: Some(2),
            quadro: Some(true),
            virtualization_mode: Some(2),
            virtualization_mode_name: Some("vgx".to_string()),
            board_number: None,
            vbios_version: None,
            pci: None,
            physical_framebuffer_kib: None,
            adapter_luid: Some(AdapterLuid {
                low_part: 1,
                high_part: 0,
            }),
            all_outputs_mask: Some(0xF00),
            connected_outputs_mask: None,
            active_outputs_mask: None,
            outputs: Vec::<OutputEntry>::new(),
            displays,
            calls: Vec::<CallOutcome>::new(),
        }
    }

    fn inventory(gpu: GpuEntry, primary: u32) -> NvapiInventoryReport {
        NvapiInventoryReport {
            schema_version: 1,
            read_only: true,
            nvapi_loaded: true,
            driver_version: Some(1),
            driver_branch: None,
            interface_version: None,
            gdi_primary_display_id: Some(primary),
            gpus: vec![gpu],
            nvapi_display_config: Vec::new(),
            ccd_paths: Vec::new(),
            unattached_displays: Vec::new(),
            findings: crate::nvapi_inventory::evaluate_spare_targets(&[], &[], &[]),
            calls: Vec::new(),
        }
    }

    #[test]
    fn activation_requires_edid_connection_and_one_new_available_target() {
        let baseline = state(false, &[10]);
        let activated = state(true, &[10, 11]);
        assert!(activation_matches(
            &baseline,
            &activated,
            &"ab".repeat(32),
            1920,
            1080
        ));
        let mut automatically_active = activated.clone();
        automatically_active.active = true;
        assert!(activation_matches(
            &baseline,
            &automatically_active,
            &"ab".repeat(32),
            1920,
            1080
        ));
        let no_new_target = state(true, &[10]);
        assert!(!activation_matches(
            &baseline,
            &no_new_target,
            &"ab".repeat(32),
            1920,
            1080
        ));
    }

    #[test]
    fn journal_rejects_non_single_bit_output_ids_and_oversized_edids() {
        let mut journal = RecoveryJournal {
            version: JOURNAL_VERSION,
            mutation_started: false,
            entries: vec![HeadlessEdidRecovery {
                display_id: 1,
                output_id: 3,
                adapter_luid: AdapterLuid::default(),
                original_edid: None,
                intended_edid_sha256: "0".repeat(64),
            }],
        };
        assert!(journal.validate().is_err());
        journal.entries[0].output_id = 4;
        journal.entries[0].original_edid = Some(vec![0; MAX_EDID_BYTES + 1]);
        assert!(journal.validate().is_err());
    }

    #[test]
    fn summary_states_that_rollback_was_verified() {
        let baseline = state(false, &[10]);
        let activated = state(true, &[10, 11]);
        let report = ProbeReport {
            schema_version: 1,
            display_id: 1,
            output_id: 4,
            adapter_luid: AdapterLuid::default(),
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            intended_edid_sha256: "ab".repeat(32),
            activation_elapsed_ms: 10,
            restore_elapsed_ms: 20,
            baseline: baseline.clone(),
            activated,
            restored: baseline,
            rollback_verified: true,
        };
        assert!(summary(&report).contains("rollback_verified=true"));
    }

    #[test]
    fn provisioning_expands_one_streaming_gpu_in_descending_output_order() {
        let report = inventory(
            gpu(vec![
                display(1, 0x800, true),
                display(2, 0x400, false),
                display(3, 0x200, false),
                display(4, 0x100, false),
            ]),
            1,
        );
        let gpu = matching_gpu(&report, "NVIDIA GRID RTX6000-8Q").expect("GPU");
        let (keep, remove) = desired_display_ids(&report, gpu, 3, None).expect("plan");
        assert_eq!(keep, vec![1, 2, 3]);
        assert!(remove.is_empty());
    }

    #[test]
    fn provisioning_shrinks_only_arcen_managed_outputs() {
        let report = inventory(
            gpu(vec![
                display(1, 0x800, true),
                display(2, 0x400, true),
                display(3, 0x200, true),
                display(4, 0x100, true),
            ]),
            1,
        );
        let gpu = matching_gpu(&report, "GRID RTX6000-8Q").expect("GPU");
        let (keep, remove) = desired_display_ids(&report, gpu, 2, None).expect("plan");
        assert_eq!(keep, vec![1, 2]);
        assert_eq!(remove, vec![3, 4]);
    }

    #[test]
    fn provisioning_clears_inactive_stale_arcen_edids_too() {
        let mut stale = display(3, 0x200, false);
        stale.edid.status = 0;
        stale.edid.byte_length = 128;
        stale.edid.manufacturer = Some("ARN".to_string());
        stale.edid.written_by_arcen = true;
        let report = inventory(
            gpu(vec![
                display(1, 0x800, true),
                display(2, 0x400, false),
                stale,
                display(4, 0x100, false),
            ]),
            1,
        );
        let gpu = matching_gpu(&report, "GRID RTX6000-8Q").expect("GPU");
        let (keep, remove) = desired_display_ids(&report, gpu, 1, None).expect("plan");
        assert_eq!(keep, vec![1]);
        assert_eq!(remove, vec![3]);
    }

    #[test]
    fn zero_edid_connector_is_not_a_mandatory_monitor() {
        let mut sticky = display(3, 0x200, true);
        sticky.edid.status = -121;
        sticky.edid.byte_length = 0;
        sticky.edid.manufacturer = None;
        sticky.edid.written_by_arcen = false;
        let report = inventory(
            gpu(vec![
                display(1, 0x800, true),
                display(2, 0x400, false),
                sticky,
                display(4, 0x100, false),
            ]),
            1,
        );
        let gpu = matching_gpu(&report, "GRID RTX6000-8Q").expect("GPU");
        let (keep, remove) = desired_display_ids(&report, gpu, 1, None).expect("plan");
        assert_eq!(keep, vec![1]);
        assert_eq!(remove, vec![3]);
    }

    #[test]
    fn configured_display_id_wins_over_inventory_order() {
        let report = inventory(
            gpu(vec![
                display(1, 0x800, true),
                display(2, 0x400, true),
                display(3, 0x200, true),
                display(4, 0x100, false),
            ]),
            1,
        );
        let gpu = matching_gpu(&report, "GRID RTX6000-8Q").expect("GPU");
        let (keep, remove) = desired_display_ids(&report, gpu, 1, Some(2)).expect("plan");
        assert_eq!(keep, vec![2]);
        assert_eq!(remove, vec![1, 3]);
    }

    #[test]
    fn zero_edid_gdi_primary_does_not_outrank_a_real_arcen_display() {
        let mut sticky = display(3, 0x200, true);
        sticky.edid.status = -121;
        sticky.edid.byte_length = 0;
        sticky.edid.manufacturer = None;
        sticky.edid.written_by_arcen = false;
        let report = inventory(
            gpu(vec![
                display(1, 0x800, true),
                display(2, 0x400, false),
                sticky,
                display(4, 0x100, false),
            ]),
            3,
        );
        let gpu = matching_gpu(&report, "GRID RTX6000-8Q").expect("GPU");
        let (keep, remove) = desired_display_ids(&report, gpu, 1, None).expect("plan");
        assert_eq!(keep, vec![1]);
        assert_eq!(remove, vec![3]);
    }

    fn recovery(
        display_id: u32,
        output_id: u32,
        original_edid: Option<Vec<u8>>,
    ) -> HeadlessEdidRecovery {
        HeadlessEdidRecovery {
            display_id,
            output_id,
            adapter_luid: AdapterLuid {
                low_part: 1,
                high_part: 0,
            },
            original_edid,
            intended_edid_sha256: "ab".repeat(32),
        }
    }

    /// ADR 0008's rollback acceptance criterion, which no product code path
    /// asserted before: every display ID Arcen added is disconnected with no
    /// EDID, and every display ID Arcen overwrote carries its original again.
    #[test]
    fn rollback_settles_only_when_added_ids_are_gone_and_overwritten_ids_are_original() {
        use sha2::{Digest as _, Sha256};

        let original_edid = vec![0xAA_u8; 128];
        let original_sha = format!("{:x}", Sha256::digest(&original_edid));

        let mut overwritten_display = display(1, 0x800, true);
        overwritten_display.edid.byte_length = original_edid.len();
        overwritten_display.edid.sha256 = Some(original_sha);
        let settled = inventory(gpu(vec![overwritten_display, display(3, 0x200, false)]), 1);

        let spare = recovery(3, 0x200, None);
        let overwritten = recovery(1, 0x800, Some(original_edid.clone()));
        assert!(restored_entry_matches(&settled, &spare).expect("spare settled"));
        assert!(restored_entry_matches(&settled, &overwritten).expect("original EDID back"));

        // A provisioned spare still reporting connected is exactly the
        // "Arcen EDID connected but inactive" leak ADR 0008 records.
        let leaked = inventory(
            gpu(vec![display(1, 0x800, true), display(3, 0x200, true)]),
            1,
        );
        assert!(!restored_entry_matches(&leaked, &spare).expect("leaked spare is not settled"));

        // An overwritten target whose EDID came back with different bytes is
        // not restored either, even though it is connected again.
        let wrong_edid = inventory(
            gpu(vec![display(1, 0x800, true), display(3, 0x200, false)]),
            1,
        );
        assert!(!restored_entry_matches(&wrong_edid, &overwritten).expect("wrong EDID"));

        // A rolled-back id whose output identity moved is a hard error, not a
        // "keep waiting" result: the binding the journal recorded is gone.
        assert!(restored_entry_matches(&settled, &recovery(3, 0x100, None)).is_err());
    }

    fn arcen_display(id: u32, in_config: bool) -> ArcenEdidDisplay {
        ArcenEdidDisplay {
            adapter: "NVIDIA GRID V100D-16Q".to_string(),
            display_id: id,
            output_id: id,
            in_display_config: in_config,
            product_code: Some(0x6100),
        }
    }

    fn clear_all() -> ClearRequest {
        ClearRequest {
            display_id: None,
            dry_run: true,
        }
    }

    /// ADR 0009's non-headless invariant, as a postcondition on planning: a
    /// Windows host with no active display cannot present LogonUI, so the
    /// credential provider has nothing to draw on and the machine is
    /// unreachable without a hypervisor console.
    #[test]
    fn clearing_everything_still_keeps_one_display() {
        let displays = vec![
            arcen_display(1, false),
            arcen_display(2, true),
            arcen_display(3, false),
        ];
        let (cleared, kept) = plan_clear(&displays, clear_all());
        assert_eq!(kept.len(), 1, "a host must never land on zero displays");
        assert_eq!(cleared.len(), 2);
    }

    /// The survivor must be one Windows is actually using, or clearing trades a
    /// live desktop for a spare and the console goes dark anyway.
    #[test]
    fn the_kept_display_is_the_one_in_the_display_config() {
        let displays = vec![
            arcen_display(1, false),
            arcen_display(2, true),
            arcen_display(3, false),
        ];
        let (_, kept) = plan_clear(&displays, clear_all());
        assert_eq!(kept[0].display_id, 2);
        assert!(kept[0].in_display_config);
    }

    #[test]
    fn a_single_display_is_never_cleared() {
        let (cleared, kept) = plan_clear(&[arcen_display(7, true)], clear_all());
        assert!(cleared.is_empty(), "the last display must survive");
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn naming_a_display_clears_only_that_one() {
        let displays = vec![
            arcen_display(1, true),
            arcen_display(2, false),
            arcen_display(3, false),
        ];
        let (cleared, kept) = plan_clear(
            &displays,
            ClearRequest {
                display_id: Some(3),
                dry_run: true,
            },
        );
        assert_eq!(cleared.len(), 1);
        assert_eq!(cleared[0].display_id, 3);
        assert_eq!(kept.len(), 2);
    }

    /// Naming the only in-config display must still refuse: the invariant is
    /// not something an explicit id can opt out of.
    #[test]
    fn naming_the_last_display_does_not_override_the_invariant() {
        let displays = vec![arcen_display(1, true), arcen_display(2, false)];
        let (cleared, _) = plan_clear(
            &displays,
            ClearRequest {
                display_id: Some(1),
                dry_run: true,
            },
        );
        assert!(
            cleared.is_empty(),
            "the surviving display must not be clearable by naming it"
        );
    }

    #[test]
    fn no_arcen_displays_is_not_an_error_and_clears_nothing() {
        let (cleared, kept) = plan_clear(&[], clear_all());
        assert!(cleared.is_empty());
        assert!(kept.is_empty());
    }
}
