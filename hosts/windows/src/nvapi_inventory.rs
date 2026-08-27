//! Read-only NVIDIA display-target inventory.
//!
//! This module answers one question and mutates nothing: **does the NVIDIA
//! driver on this host expose display targets that Windows is not currently
//! using?** It exists because Deck sends a complete `1..=4` monitor layout while
//! Windows Pier can only serve display paths Windows already enumerates
//! (`docs/adr/0008-virtual-display-for-windows-hosts.md`, superseding decision).
//!
//! Every call below is a getter. There is deliberately no `SetEDID`, no
//! `TryCustomDisplay`, no `SaveCustomDisplay` and no `SetDisplayConfig` here.
//! `NvAPI_DISP_TryCustomDisplay` in particular only changes *timing on an
//! existing `displayId`*; it never creates a target, so it cannot be used as
//! evidence of spare capacity.
//!
//! ABI declarations and QueryInterface IDs are derived from NVIDIA's public,
//! MIT-licensed NVAPI SDK at commit
//! `cd6918f60b3c9a0476fdfe7e89bb32330602049d`:
//! <https://github.com/NVIDIA/nvapi/tree/cd6918f60b3c9a0476fdfe7e89bb32330602049d>.
//! The Rust bindings are original.

use crate::nvapi::AdapterLuid;
use serde::{Deserialize, Serialize};

pub const SCHEMA_VERSION: u32 = 1;

/// `NV_GPU_DISPLAYIDS` bitfield positions, in declaration order.
const DISPLAY_ID_FLAG_DYNAMIC: u32 = 1 << 0;
const DISPLAY_ID_FLAG_MULTI_STREAM_ROOT_NODE: u32 = 1 << 1;
const DISPLAY_ID_FLAG_ACTIVE: u32 = 1 << 2;
const DISPLAY_ID_FLAG_CLUSTER: u32 = 1 << 3;
const DISPLAY_ID_FLAG_OS_VISIBLE: u32 = 1 << 4;
const DISPLAY_ID_FLAG_WFD: u32 = 1 << 5;
const DISPLAY_ID_FLAG_CONNECTED: u32 = 1 << 6;
const DISPLAY_ID_FLAG_PHYSICALLY_CONNECTED: u32 = 1 << 17;

/// EDID manufacturer ID Arcen writes; see `crate::edid`.
const ARCEN_EDID_MANUFACTURER: &str = "ARN";

/// Result of one NVAPI or Win32 call, kept even when it failed. An unsupported
/// entry point is itself evidence, so nothing is silently dropped.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CallOutcome {
    pub call: String,
    pub status: i32,
    pub detail: String,
}

impl CallOutcome {
    pub fn ok(call: impl Into<String>) -> Self {
        Self {
            call: call.into(),
            status: 0,
            detail: String::new(),
        }
    }

    pub fn failed(call: impl Into<String>, status: i32, detail: impl Into<String>) -> Self {
        Self {
            call: call.into(),
            status,
            detail: detail.into(),
        }
    }

    pub fn succeeded(&self) -> bool {
        self.status == 0
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct DisplayIdFlags {
    pub raw: u32,
    pub dynamic: bool,
    pub multi_stream_root_node: bool,
    pub active: bool,
    pub cluster: bool,
    pub os_visible: bool,
    pub wfd: bool,
    pub connected: bool,
    pub physically_connected: bool,
}

pub fn decode_display_id_flags(raw: u32) -> DisplayIdFlags {
    DisplayIdFlags {
        raw,
        dynamic: raw & DISPLAY_ID_FLAG_DYNAMIC != 0,
        multi_stream_root_node: raw & DISPLAY_ID_FLAG_MULTI_STREAM_ROOT_NODE != 0,
        active: raw & DISPLAY_ID_FLAG_ACTIVE != 0,
        cluster: raw & DISPLAY_ID_FLAG_CLUSTER != 0,
        os_visible: raw & DISPLAY_ID_FLAG_OS_VISIBLE != 0,
        wfd: raw & DISPLAY_ID_FLAG_WFD != 0,
        connected: raw & DISPLAY_ID_FLAG_CONNECTED != 0,
        physically_connected: raw & DISPLAY_ID_FLAG_PHYSICALLY_CONNECTED != 0,
    }
}

/// NVAPI output IDs are single-bit masks. Expand a mask into its member IDs.
pub fn decode_output_mask(mask: u32) -> Vec<u32> {
    (0..u32::BITS)
        .map(|bit| 1u32 << bit)
        .filter(|output_id| mask & output_id != 0)
        .collect()
}

/// `NV_MONITOR_CONN_TYPE`, as reported in `NV_GPU_DISPLAYIDS.connectorType`.
pub fn connector_type_name(value: i32) -> &'static str {
    match value {
        0 => "uninitialized",
        1 => "vga",
        2 => "component",
        3 => "svideo",
        4 => "hdmi",
        5 => "dvi",
        6 => "lvds",
        7 => "displayport",
        8 => "composite",
        _ => "unknown",
    }
}

/// `NV_GPU_OUTPUT_TYPE`, as reported by `NvAPI_GPU_GetOutputType`.
pub fn output_type_name(value: i32) -> &'static str {
    match value {
        0 => "unknown",
        1 => "crt",
        2 => "dfp",
        3 => "tv",
        _ => "unrecognized",
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct EdidProbe {
    /// Raw NVAPI status from `NvAPI_GPU_GetEDID`, or `None` when the display had
    /// no resolvable output ID to query with.
    pub queried: bool,
    pub status: i32,
    pub detail: String,
    pub byte_length: usize,
    pub sha256: Option<String>,
    pub manufacturer: Option<String>,
    pub product_code: Option<u16>,
    pub preferred_width: Option<u32>,
    pub preferred_height: Option<u32>,
    /// True when the EDID currently attached to this display carries Arcen's
    /// manufacturer ID, which proves `NvAPI_GPU_SetEDID` previously succeeded
    /// on this board without needing to call it again.
    pub written_by_arcen: bool,
}

/// Which enumeration reported a display ID. A display ID that only `GetAllDisplayIds`
/// reports is a candidate spare target; one that `GetConnectedDisplayIds` also
/// reports is backed by something the driver considers attached.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayIdSource {
    AllDisplayIds,
    ConnectedCached,
    ConnectedUncached,
    ConnectedFake,
    OutputMask,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DisplayIdEntry {
    pub display_id: u32,
    pub connector_type: i32,
    pub flags: DisplayIdFlags,
    pub sources: Vec<DisplayIdSource>,
    pub output_id: Option<u32>,
    pub edid: EdidProbe,
    /// True when this display ID appears as a target in the live NVAPI display
    /// configuration (`NvAPI_DISP_GetDisplayConfig`).
    pub in_nvapi_display_config: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct OutputEntry {
    pub output_id: u32,
    pub bit_index: u32,
    pub in_all_mask: bool,
    pub connected: bool,
    pub active: bool,
    pub output_type: Option<i32>,
    pub display_id: Option<u32>,
    pub display_id_lookup: Option<CallOutcome>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct PciIdentifiers {
    pub device_id: u32,
    pub subsystem_id: u32,
    pub revision_id: u32,
    pub external_device_id: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct GpuEntry {
    pub index: u32,
    pub full_name: Option<String>,
    pub gpu_type: Option<u32>,
    pub system_type: Option<u32>,
    pub quadro: Option<bool>,
    pub virtualization_mode: Option<u32>,
    pub virtualization_mode_name: Option<String>,
    pub board_number: Option<String>,
    pub vbios_version: Option<String>,
    pub pci: Option<PciIdentifiers>,
    pub physical_framebuffer_kib: Option<u32>,
    pub adapter_luid: Option<AdapterLuid>,
    pub all_outputs_mask: Option<u32>,
    pub connected_outputs_mask: Option<u32>,
    pub active_outputs_mask: Option<u32>,
    pub outputs: Vec<OutputEntry>,
    pub displays: Vec<DisplayIdEntry>,
    pub calls: Vec<CallOutcome>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct NvapiConfigTarget {
    pub display_id: u32,
    /// NVAPI path-local target field. NVIDIA paths currently report zero and
    /// do not use this as the Windows CCD target identifier.
    pub path_target_id: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct NvapiConfigPathEntry {
    pub source_id: u32,
    pub non_nvidia_adapter: bool,
    pub os_adapter_luid: Option<AdapterLuid>,
    pub width: u32,
    pub height: u32,
    pub color_depth: u32,
    pub position_x: i32,
    pub position_y: i32,
    pub primary: bool,
    pub targets: Vec<NvapiConfigTarget>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct CcdPathEntry {
    pub source_adapter_luid: AdapterLuid,
    pub source_id: u32,
    pub target_adapter_luid: AdapterLuid,
    pub target_id: u32,
    pub active: bool,
    pub target_available: bool,
    pub status_flags: u32,
    pub output_technology: i32,
    pub gdi_device_name: Option<String>,
    pub monitor_device_path: Option<String>,
    pub monitor_friendly_name: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct UnattachedDisplayEntry {
    pub index: u32,
    pub name: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SpareTargetVerdict {
    /// NVAPI did not load or reported no physical GPU.
    NoNvidiaGpu,
    /// Every NVIDIA display ID the driver knows about is already an active path.
    NoSpareTargets,
    /// The driver reports output IDs with no display ID behind them. An output
    /// ID alone is a connector slot, not an addressable target.
    SpareOutputIdsWithoutDisplayIds,
    /// At least one NVIDIA display ID exists that is not active. This is the only
    /// state in which activating an additional target is even arguable, and it
    /// still requires proving activation and rollback separately.
    SpareDisplayIdsPresent,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SpareDisplayId {
    pub gpu_index: u32,
    pub display_id: u32,
    pub output_id: Option<u32>,
    pub flags: DisplayIdFlags,
    pub in_nvapi_display_config: bool,
    /// `NvAPI_GPU_GetEDID` status for this display id. `NVAPI_DATA_NOT_FOUND`
    /// (-121) is the expected value for a connector with nothing plugged in.
    pub edid_status: i32,
    pub edid_present: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SpareOutputId {
    pub gpu_index: u32,
    pub output_id: u32,
    pub output_type: Option<i32>,
}

/// A distinct Windows CCD target on an NVIDIA adapter with no active path.
///
/// `target_available` is the discriminator that matters: `false` means Windows
/// has a target object but no monitor behind it, so no display configuration
/// can light it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SpareCcdTarget {
    pub adapter_luid: AdapterLuid,
    pub target_id: u32,
    pub target_available: bool,
    pub output_technology: i32,
    pub monitor_friendly_name: Option<String>,
    /// Inactive CCD paths (source/target pairs) Windows offers for this target.
    pub inactive_paths: usize,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct SpareTargetFindings {
    pub nvidia_display_ids_total: usize,
    pub nvidia_display_ids_active: usize,
    pub nvidia_display_ids_connected: usize,
    /// CCD *paths* (source/target pairs) whose target sits on an NVIDIA adapter.
    /// Windows reports one path per (source, target) combination, so this is far
    /// larger than the number of targets.
    pub nvidia_ccd_paths_total: usize,
    /// Distinct `(adapter luid, target id)` pairs behind those paths.
    pub nvidia_ccd_targets_total: usize,
    pub nvidia_ccd_targets_active: usize,
    pub nvidia_ccd_targets_inactive: usize,
    /// Distinct inactive targets Windows reports as having a monitor.
    pub nvidia_ccd_targets_inactive_available: usize,
    pub spare_display_ids: Vec<SpareDisplayId>,
    pub spare_output_ids: Vec<SpareOutputId>,
    pub spare_ccd_targets: Vec<SpareCcdTarget>,
    pub verdict: SpareTargetVerdict,
    pub rationale: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct NvapiInventoryReport {
    pub schema_version: u32,
    /// Always true. This probe has no mutating code path.
    pub read_only: bool,
    pub nvapi_loaded: bool,
    pub driver_version: Option<u32>,
    pub driver_branch: Option<String>,
    pub interface_version: Option<String>,
    pub gdi_primary_display_id: Option<u32>,
    pub gpus: Vec<GpuEntry>,
    pub nvapi_display_config: Vec<NvapiConfigPathEntry>,
    pub ccd_paths: Vec<CcdPathEntry>,
    pub unattached_displays: Vec<UnattachedDisplayEntry>,
    pub findings: SpareTargetFindings,
    pub calls: Vec<CallOutcome>,
}

/// Decode the manufacturer ID, product code and preferred timing from an EDID
/// base block without trusting its length or checksum. Diagnostics only.
pub fn summarize_edid(bytes: &[u8]) -> EdidProbe {
    let mut probe = EdidProbe {
        queried: true,
        byte_length: bytes.len(),
        ..EdidProbe::default()
    };
    if bytes.len() < 128 {
        return probe;
    }
    let packed = u16::from_be_bytes([bytes[8], bytes[9]]);
    let letter = |shift: u32| -> u8 { (((packed >> shift) & 0x1f) as u8) + b'A' - 1 };
    let manufacturer: String = [letter(10), letter(5), letter(0)]
        .iter()
        .map(|byte| char::from(*byte))
        .collect();
    if manufacturer.bytes().all(|byte| byte.is_ascii_uppercase()) {
        probe.written_by_arcen = manufacturer == ARCEN_EDID_MANUFACTURER;
        probe.manufacturer = Some(manufacturer);
    }
    probe.product_code = Some(u16::from_le_bytes([bytes[10], bytes[11]]));
    let detailed = &bytes[54..72];
    if detailed[0] != 0 || detailed[1] != 0 {
        let width = u32::from(detailed[2]) | ((u32::from(detailed[4]) & 0xf0) << 4);
        let height = u32::from(detailed[5]) | ((u32::from(detailed[7]) & 0xf0) << 4);
        probe.preferred_width = Some(width);
        probe.preferred_height = Some(height);
    }
    probe
}

/// Classify the inventory. Pure so it is unit-testable off Windows and so the
/// verdict cannot quietly depend on a live driver.
pub fn evaluate_spare_targets(
    gpus: &[GpuEntry],
    nvapi_config: &[NvapiConfigPathEntry],
    ccd_paths: &[CcdPathEntry],
) -> SpareTargetFindings {
    let nvidia_luids: Vec<AdapterLuid> = gpus.iter().filter_map(|gpu| gpu.adapter_luid).collect();
    let configured_display_ids: Vec<u32> = nvapi_config
        .iter()
        .filter(|path| !path.non_nvidia_adapter)
        .flat_map(|path| path.targets.iter().map(|target| target.display_id))
        .collect();

    let nvidia_ccd: Vec<&CcdPathEntry> = ccd_paths
        .iter()
        .filter(|path| nvidia_luids.contains(&path.target_adapter_luid))
        .collect();
    let nvidia_ccd_paths_total = nvidia_ccd.len();
    // Windows reports one CCD path per (source, target) pair, so the same target
    // appears many times. Only a target with no active path anywhere is spare.
    let mut distinct_targets: Vec<(AdapterLuid, u32)> = Vec::new();
    for path in &nvidia_ccd {
        let key = (path.target_adapter_luid, path.target_id);
        if !distinct_targets.contains(&key) {
            distinct_targets.push(key);
        }
    }
    let mut nvidia_ccd_targets_active = 0usize;
    let mut nvidia_ccd_targets_inactive_available = 0usize;
    let mut spare_ccd_targets: Vec<SpareCcdTarget> = Vec::new();
    for (adapter_luid, target_id) in distinct_targets {
        let paths: Vec<&&CcdPathEntry> = nvidia_ccd
            .iter()
            .filter(|path| path.target_adapter_luid == adapter_luid && path.target_id == target_id)
            .collect();
        if paths.iter().any(|path| path.active) {
            nvidia_ccd_targets_active += 1;
            continue;
        }
        let target_available = paths.iter().any(|path| path.target_available);
        if target_available {
            nvidia_ccd_targets_inactive_available += 1;
        }
        spare_ccd_targets.push(SpareCcdTarget {
            adapter_luid,
            target_id,
            target_available,
            output_technology: paths[0].output_technology,
            monitor_friendly_name: paths
                .iter()
                .find_map(|path| path.monitor_friendly_name.clone())
                .filter(|name| !name.is_empty()),
            inactive_paths: paths.len(),
        });
    }
    let nvidia_ccd_targets_total = nvidia_ccd_targets_active + spare_ccd_targets.len();
    let nvidia_ccd_targets_inactive = spare_ccd_targets.len();

    let mut total = 0usize;
    let mut active = 0usize;
    let mut connected = 0usize;
    let mut spare_display_ids = Vec::new();
    let mut spare_output_ids = Vec::new();

    for gpu in gpus {
        for display in &gpu.displays {
            total += 1;
            if display.flags.active {
                active += 1;
            }
            if display.flags.connected {
                connected += 1;
            }
            let configured = display.in_nvapi_display_config
                || configured_display_ids.contains(&display.display_id);
            if display.flags.active || configured {
                continue;
            }
            spare_display_ids.push(SpareDisplayId {
                gpu_index: gpu.index,
                display_id: display.display_id,
                output_id: display.output_id,
                flags: display.flags,
                in_nvapi_display_config: configured,
                edid_status: display.edid.status,
                edid_present: display.edid.byte_length > 0,
            });
        }
        let display_output_ids: Vec<u32> = gpu
            .displays
            .iter()
            .filter_map(|entry| entry.output_id)
            .collect();
        for output in &gpu.outputs {
            if display_output_ids.contains(&output.output_id) || output.display_id.is_some() {
                continue;
            }
            spare_output_ids.push(SpareOutputId {
                gpu_index: gpu.index,
                output_id: output.output_id,
                output_type: output.output_type,
            });
        }
    }

    let mut rationale = Vec::new();
    let verdict = if gpus.is_empty() {
        rationale.push("NVAPI reported no physical NVIDIA GPU on this host.".to_string());
        SpareTargetVerdict::NoNvidiaGpu
    } else if !spare_display_ids.is_empty() {
        rationale.push(format!(
            "{} NVIDIA display id(s) are neither active nor present in the NVAPI display configuration.",
            spare_display_ids.len()
        ));
        let with_edid = spare_display_ids
            .iter()
            .filter(|spare| spare.edid_present)
            .count();
        if with_edid == 0 {
            rationale.push(
                "No spare display id carries an EDID; the driver reports an empty connector for each, so nothing yet makes Windows treat one as a monitor."
                    .to_string(),
            );
        } else {
            rationale.push(format!(
                "{with_edid} spare display id(s) already carry an EDID while remaining inactive."
            ));
        }
        SpareTargetVerdict::SpareDisplayIdsPresent
    } else if !spare_output_ids.is_empty() {
        rationale.push(format!(
            "{} NVIDIA output id(s) exist with no display id behind them; an output id is a connector slot, not an addressable target.",
            spare_output_ids.len()
        ));
        SpareTargetVerdict::SpareOutputIdsWithoutDisplayIds
    } else {
        rationale.push(
            "Every NVIDIA display id the driver enumerates is already an active display path."
                .to_string(),
        );
        SpareTargetVerdict::NoSpareTargets
    };

    if nvidia_ccd_targets_inactive > 0 {
        rationale.push(format!(
            "Windows CCD reports {nvidia_ccd_targets_inactive} of {nvidia_ccd_targets_total} distinct NVIDIA target(s) with no active path, across {nvidia_ccd_paths_total} enumerated path(s)."
        ));
    }
    if nvidia_ccd_targets_inactive > 0 && nvidia_ccd_targets_inactive_available == 0 {
        rationale.push(
            "Every inactive NVIDIA CCD target reports targetAvailable=false: Windows has a target object but no monitor behind it, so no SetDisplayConfig can light one."
                .to_string(),
        );
    } else if nvidia_ccd_targets_inactive_available > 0 {
        rationale.push(format!(
            "{nvidia_ccd_targets_inactive_available} inactive NVIDIA CCD target(s) report targetAvailable=true, so Windows already has a monitor for them."
        ));
    }
    rationale.push(
        "NvAPI_DISP_TryCustomDisplay only retimes an existing display id and is not evidence of target creation."
            .to_string(),
    );

    SpareTargetFindings {
        nvidia_display_ids_total: total,
        nvidia_display_ids_active: active,
        nvidia_display_ids_connected: connected,
        nvidia_ccd_paths_total,
        nvidia_ccd_targets_total,
        nvidia_ccd_targets_active,
        nvidia_ccd_targets_inactive,
        nvidia_ccd_targets_inactive_available,
        spare_display_ids,
        spare_output_ids,
        spare_ccd_targets,
        verdict,
        rationale,
    }
}

/// Human-readable one-page summary for operators, derived from the same report
/// the JSON form carries.
pub fn render_summary(report: &NvapiInventoryReport) -> String {
    let mut out = String::new();
    out.push_str("Arcen NVAPI display-target inventory (read-only)\n");
    out.push_str(&format!(
        "  nvapi loaded: {}  driver: {}  branch: {}\n",
        report.nvapi_loaded,
        report
            .driver_version
            .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
        report.driver_branch.as_deref().unwrap_or("unknown"),
    ));
    for gpu in &report.gpus {
        out.push_str(&format!(
            "  gpu {} {}\n    quadro={} virtualization={} luid={}\n",
            gpu.index,
            gpu.full_name.as_deref().unwrap_or("<unnamed>"),
            gpu.quadro
                .map_or_else(|| "unknown".to_string(), |value| value.to_string()),
            gpu.virtualization_mode_name.as_deref().unwrap_or("unknown"),
            gpu.adapter_luid.map_or_else(
                || "unknown".to_string(),
                |luid| format!("{:08x}:{:08x}", luid.high_part, luid.low_part)
            ),
        ));
        out.push_str(&format!(
            "    outputs all={} connected={} active={}\n",
            mask_text(gpu.all_outputs_mask),
            mask_text(gpu.connected_outputs_mask),
            mask_text(gpu.active_outputs_mask),
        ));
        for display in &gpu.displays {
            out.push_str(&format!(
                "    display 0x{:08x} output={} connector={} active={} connected={} os_visible={} edid={} manufacturer={}\n",
                display.display_id,
                display
                    .output_id
                    .map_or_else(|| "none".to_string(), |value| format!("0x{value:08x}")),
                connector_type_name(display.connector_type),
                display.flags.active,
                display.flags.connected,
                display.flags.os_visible,
                if display.edid.byte_length > 0 {
                    format!("{} bytes", display.edid.byte_length)
                } else {
                    format!("status {}", display.edid.status)
                },
                display.edid.manufacturer.as_deref().unwrap_or("-"),
            ));
        }
        for call in gpu.calls.iter().filter(|call| !call.succeeded()) {
            out.push_str(&format!(
                "    unavailable: {} (status {})\n",
                call.call, call.status
            ));
        }
    }
    out.push_str(&format!(
        "  ccd: {} path(s) total, {} on NVIDIA adapters, {} distinct NVIDIA target(s), {} active, {} inactive ({} with a monitor)\n",
        report.ccd_paths.len(),
        report.findings.nvidia_ccd_paths_total,
        report.findings.nvidia_ccd_targets_total,
        report.findings.nvidia_ccd_targets_active,
        report.findings.nvidia_ccd_targets_inactive,
        report.findings.nvidia_ccd_targets_inactive_available,
    ));
    for path in &report.nvapi_display_config {
        out.push_str(&format!(
            "  nvapi path source={} {}x{} at {},{} primary={} nvidia={} targets={}\n",
            path.source_id,
            path.width,
            path.height,
            path.position_x,
            path.position_y,
            path.primary,
            !path.non_nvidia_adapter,
            path.targets
                .iter()
                .map(|target| {
                    format!(
                        "0x{:08x}/path-target:{}",
                        target.display_id, target.path_target_id
                    )
                })
                .collect::<Vec<String>>()
                .join(","),
        ));
    }
    for spare in &report.findings.spare_display_ids {
        out.push_str(&format!(
            "  spare display id 0x{:08x} on gpu {} output={} connected={} os_visible={} edid_status={}\n",
            spare.display_id,
            spare.gpu_index,
            spare
                .output_id
                .map_or_else(|| "none".to_string(), |value| format!("0x{value:08x}")),
            spare.flags.connected,
            spare.flags.os_visible,
            spare.edid_status,
        ));
    }
    for spare in &report.findings.spare_output_ids {
        out.push_str(&format!(
            "  spare output id 0x{:08x} on gpu {} type={}\n",
            spare.output_id,
            spare.gpu_index,
            spare
                .output_type
                .map_or("unknown", |value| output_type_name(value)),
        ));
    }
    for spare in &report.findings.spare_ccd_targets {
        out.push_str(&format!(
            "  spare ccd target {} on adapter {:08x}:{:08x} available={} technology={} inactive_paths={}\n",
            spare.target_id,
            spare.adapter_luid.high_part,
            spare.adapter_luid.low_part,
            spare.target_available,
            spare.output_technology,
            spare.inactive_paths,
        ));
    }
    out.push_str(&format!(
        "  unattached NVIDIA displays: {}\n",
        report.unattached_displays.len()
    ));
    for call in report.calls.iter().filter(|call| !call.succeeded()) {
        out.push_str(&format!(
            "  unavailable: {} (status {})\n",
            call.call, call.status
        ));
    }
    out.push_str(&format!("  verdict: {:?}\n", report.findings.verdict));
    for line in &report.findings.rationale {
        out.push_str(&format!("    - {line}\n"));
    }
    out
}

fn mask_text(mask: Option<u32>) -> String {
    mask.map_or_else(
        || "unavailable".to_string(),
        |value| format!("0x{value:08x}({})", decode_output_mask(value).len()),
    )
}

#[cfg(not(windows))]
pub fn inventory() -> Result<NvapiInventoryReport, String> {
    Err("the NVAPI display-target inventory is available only on Windows".to_string())
}

#[cfg(windows)]
pub fn inventory() -> Result<NvapiInventoryReport, String> {
    probe::inventory()
}

#[cfg(windows)]
mod probe {
    use super::*;
    use std::ffi::{c_char, c_void};
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Devices::Display::{
        DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
        DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
        DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
        DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
        DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ALL_PATHS,
    };
    use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, LUID};
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    const NVAPI_OK: i32 = 0;
    const NVAPI_END_ENUMERATION: i32 = -7;
    const NVAPI_DATA_NOT_FOUND: i32 = -121;
    const NVAPI_SHORT_STRING_MAX: usize = 64;
    const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;
    const MAX_GPU_DISPLAY_IDS: usize = 256;
    const MAX_UNATTACHED_DISPLAYS: u32 = 64;
    const NV_EDID_DATA_SIZE: usize = 256;

    /// `NvAPI_GPU_GetConnectedDisplayIds` flags.
    const CONNECTED_IDS_FLAG_UNCACHED: u32 = 0x1;
    const CONNECTED_IDS_FLAG_FAKE: u32 = 0x8;

    /// Public QueryInterface IDs used only by this read-only inventory. Mutating
    /// IDs deliberately do not appear in this module.
    const ID_INITIALIZE: u32 = 0x0150_e828;
    const ID_GET_ERROR_MESSAGE: u32 = 0x6c2d_048c;
    const ID_GET_INTERFACE_VERSION_STRING: u32 = 0x0105_3fa5;
    const ID_ENUM_PHYSICAL_GPUS: u32 = 0xe5ac_921f;
    const ID_SYS_GET_DRIVER_AND_BRANCH_VERSION: u32 = 0x2926_aaad;
    const ID_GPU_GET_FULL_NAME: u32 = 0xceee_8e9f;
    const ID_GPU_GET_PCI_IDENTIFIERS: u32 = 0x2ddf_b66e;
    const ID_GPU_GET_GPU_TYPE: u32 = 0xc33b_aeb1;
    const ID_GPU_GET_SYSTEM_TYPE: u32 = 0xbaaa_bfcc;
    const ID_GPU_GET_QUADRO_STATUS: u32 = 0xe332_fa47;
    const ID_GPU_GET_VIRTUALIZATION_INFO: u32 = 0x44e0_22a9;
    const ID_GPU_GET_BOARD_INFO: u32 = 0x22d5_4523;
    const ID_GPU_GET_VBIOS_VERSION_STRING: u32 = 0xa561_fd7d;
    const ID_GPU_GET_PHYSICAL_FRAME_BUFFER_SIZE: u32 = 0x46fb_eb03;
    const ID_GPU_GET_ADAPTER_ID: u32 = 0x0ff0_7fde;
    const ID_GPU_GET_ALL_OUTPUTS: u32 = 0x7d55_4f8e;
    const ID_GPU_GET_CONNECTED_OUTPUTS: u32 = 0x1730_bfc9;
    const ID_GPU_GET_ACTIVE_OUTPUTS: u32 = 0xe3e8_9b6f;
    const ID_GPU_GET_OUTPUT_TYPE: u32 = 0x40a5_05e4;
    const ID_GPU_GET_ALL_DISPLAY_IDS: u32 = 0x7852_10a2;
    const ID_GPU_GET_CONNECTED_DISPLAY_IDS: u32 = 0x0078_dba2;
    const ID_GPU_GET_EDID: u32 = 0x37d3_2e69;
    const ID_SYS_GET_DISPLAY_ID_FROM_GPU_AND_OUTPUT_ID: u32 = 0x08f2_bab4;
    const ID_SYS_GET_GPU_AND_OUTPUT_ID_FROM_DISPLAY_ID: u32 = 0x112b_a1a5;
    const ID_DISP_GET_GDI_PRIMARY_DISPLAY_ID: u32 = 0x1e9d_8a31;
    const ID_ENUM_UNATTACHED_DISPLAY_HANDLE: u32 = 0x20de_9260;
    const ID_GET_UNATTACHED_ASSOCIATED_DISPLAY_NAME: u32 = 0x4888_d790;

    type NvPhysicalGpuHandle = *mut c_void;
    type NvUnAttachedDisplayHandle = *mut c_void;
    type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;

    type InitializeFn = unsafe extern "C" fn() -> i32;
    type GetErrorMessageFn = unsafe extern "C" fn(i32, *mut c_char) -> i32;
    type ShortStringOutFn = unsafe extern "C" fn(*mut c_char) -> i32;
    type EnumPhysicalGpusFn = unsafe extern "C" fn(*mut NvPhysicalGpuHandle, *mut u32) -> i32;
    type DriverAndBranchFn = unsafe extern "C" fn(*mut u32, *mut c_char) -> i32;
    type GpuShortStringFn = unsafe extern "C" fn(NvPhysicalGpuHandle, *mut c_char) -> i32;
    type GpuU32OutFn = unsafe extern "C" fn(NvPhysicalGpuHandle, *mut u32) -> i32;
    type GpuI32OutFn = unsafe extern "C" fn(NvPhysicalGpuHandle, *mut i32) -> i32;
    type GpuStructOutFn = unsafe extern "C" fn(NvPhysicalGpuHandle, *mut c_void) -> i32;
    type GetPciIdentifiersFn =
        unsafe extern "C" fn(NvPhysicalGpuHandle, *mut u32, *mut u32, *mut u32, *mut u32) -> i32;
    type GetOutputTypeFn = unsafe extern "C" fn(NvPhysicalGpuHandle, u32, *mut i32) -> i32;
    type GetAllDisplayIdsFn =
        unsafe extern "C" fn(NvPhysicalGpuHandle, *mut NvGpuDisplayIds, *mut u32) -> i32;
    type GetConnectedDisplayIdsFn =
        unsafe extern "C" fn(NvPhysicalGpuHandle, *mut NvGpuDisplayIds, *mut u32, u32) -> i32;
    type GetEdidFn = unsafe extern "C" fn(NvPhysicalGpuHandle, u32, *mut NvEdid) -> i32;
    type DisplayIdFromGpuAndOutputFn =
        unsafe extern "C" fn(NvPhysicalGpuHandle, u32, *mut u32) -> i32;
    type GpuAndOutputFromDisplayIdFn =
        unsafe extern "C" fn(u32, *mut NvPhysicalGpuHandle, *mut u32) -> i32;
    type U32OutFn = unsafe extern "C" fn(*mut u32) -> i32;
    type EnumUnattachedFn = unsafe extern "C" fn(u32, *mut NvUnAttachedDisplayHandle) -> i32;
    type UnattachedNameFn = unsafe extern "C" fn(NvUnAttachedDisplayHandle, *mut c_char) -> i32;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    struct NvGpuDisplayIds {
        version: u32,
        connector_type: i32,
        display_id: u32,
        flags: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct NvEdid {
        version: u32,
        data: [u8; NV_EDID_DATA_SIZE],
        size: u32,
        edid_id: u32,
        offset: u32,
    }

    impl Default for NvEdid {
        fn default() -> Self {
            Self {
                version: nvapi_version::<Self>(3),
                data: [0; NV_EDID_DATA_SIZE],
                size: 0,
                edid_id: 0,
                offset: 0,
            }
        }
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    struct NvVirtualizationInfo {
        version: u32,
        virtualization_mode: u32,
        reserved: u32,
    }

    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default)]
    struct NvBoardInfo {
        version: u32,
        board_number: [u8; 16],
    }

    const _: () = {
        assert!(std::mem::size_of::<NvGpuDisplayIds>() == 16);
        assert!(std::mem::align_of::<NvGpuDisplayIds>() == 4);
        assert!(std::mem::size_of::<NvEdid>() == 272);
        assert!(std::mem::align_of::<NvEdid>() == 4);
        assert!(std::mem::size_of::<NvVirtualizationInfo>() == 12);
        assert!(std::mem::align_of::<NvVirtualizationInfo>() == 4);
        assert!(std::mem::size_of::<NvBoardInfo>() == 20);
        assert!(std::mem::align_of::<NvBoardInfo>() == 4);
    };

    const fn nvapi_version<T>(version: u32) -> u32 {
        std::mem::size_of::<T>() as u32 | (version << 16)
    }

    fn virtualization_mode_name(mode: u32) -> &'static str {
        match mode {
            0 => "none",
            1 => "nmos",
            2 => "vgx",
            3 => "host_vgpu",
            4 => "host_vsga",
            _ => "unrecognized",
        }
    }

    struct Loader {
        query: QueryInterface,
        get_error_message: Option<GetErrorMessageFn>,
    }

    impl Loader {
        fn new() -> Result<Self, String> {
            let name: Vec<u16> = "nvapi64.dll"
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            // SAFETY: the DLL name is null-terminated and SYSTEM32-only loading prevents
            // a working-directory DLL from being selected.
            let module = unsafe {
                LoadLibraryExW(
                    PCWSTR(name.as_ptr()),
                    HANDLE::default(),
                    LOAD_LIBRARY_SEARCH_SYSTEM32,
                )
            }
            .map_err(|error| format!("load system32 nvapi64.dll: {error}"))?;
            // SAFETY: the module loaded and the symbol name is static, null-terminated ASCII.
            let raw_query =
                unsafe { GetProcAddress(module, PCSTR(c"nvapi_QueryInterface".as_ptr().cast())) }
                    .ok_or_else(|| "nvapi64.dll does not export nvapi_QueryInterface".to_string())?;
            // SAFETY: NVIDIA documents nvapi_QueryInterface as a cdecl function taking one
            // u32 interface ID and returning the corresponding function address.
            let query: QueryInterface = unsafe { std::mem::transmute(raw_query as *const ()) };
            let mut loader = Self {
                query,
                get_error_message: None,
            };
            loader.get_error_message = loader.resolve::<GetErrorMessageFn>(ID_GET_ERROR_MESSAGE);
            let initialize: InitializeFn = loader
                .resolve(ID_INITIALIZE)
                .ok_or_else(|| "NvAPI_Initialize is unavailable".to_string())?;
            // SAFETY: initialize was resolved from NVIDIA's public interface ID with the
            // exact documented no-argument signature.
            let status = unsafe { initialize() };
            if status != NVAPI_OK {
                return Err(format!("NvAPI_Initialize returned status {status}"));
            }
            Ok(loader)
        }

        fn resolve<T: Copy>(&self, id: u32) -> Option<T> {
            if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
                return None;
            }
            // SAFETY: QueryInterface accepts any public interface ID as a u32 and
            // returns null for entry points this driver does not implement.
            let pointer = unsafe { (self.query)(id) };
            if pointer.is_null() {
                return None;
            }
            // SAFETY: each call site supplies the exact function-pointer signature from
            // the pinned public header, and pointer size equality is checked above.
            Some(unsafe { std::mem::transmute_copy(&pointer) })
        }

        fn message(&self, status: i32) -> String {
            let Some(get_error_message) = self.get_error_message else {
                return String::new();
            };
            let mut buffer = [0i8; NVAPI_SHORT_STRING_MAX];
            // SAFETY: buffer is the documented writable 64-byte short string.
            if unsafe { get_error_message(status, buffer.as_mut_ptr()) } != NVAPI_OK {
                return String::new();
            }
            short_string(&buffer)
        }

        fn outcome(&self, call: &str, status: i32) -> CallOutcome {
            if status == NVAPI_OK {
                CallOutcome::ok(call)
            } else {
                CallOutcome::failed(call, status, self.message(status))
            }
        }

        fn missing(&self, call: &str) -> CallOutcome {
            CallOutcome::failed(call, i32::MIN, "entry point not exported by this driver")
        }
    }

    fn short_string(buffer: &[i8; NVAPI_SHORT_STRING_MAX]) -> String {
        let bytes: Vec<u8> = buffer
            .iter()
            .take_while(|byte| **byte != 0)
            .map(|byte| *byte as u8)
            .collect();
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// Ask NVAPI for a display-id list using the documented two-call pattern:
    /// a null buffer returns the count, then a sized buffer returns the entries.
    fn display_id_list(
        loader: &Loader,
        call: &str,
        mut fetch: impl FnMut(*mut NvGpuDisplayIds, *mut u32) -> i32,
    ) -> (Vec<NvGpuDisplayIds>, CallOutcome) {
        let mut count = 0u32;
        let status = fetch(std::ptr::null_mut(), &mut count);
        if status != NVAPI_OK {
            return (Vec::new(), loader.outcome(call, status));
        }
        if count == 0 {
            return (Vec::new(), CallOutcome::ok(call));
        }
        let count = count.min(MAX_GPU_DISPLAY_IDS as u32);
        let mut entries = vec![
            NvGpuDisplayIds {
                version: nvapi_version::<NvGpuDisplayIds>(3),
                ..NvGpuDisplayIds::default()
            };
            count as usize
        ];
        let mut requested = count;
        let status = fetch(entries.as_mut_ptr(), &mut requested);
        if status != NVAPI_OK {
            return (Vec::new(), loader.outcome(call, status));
        }
        entries.truncate(requested.min(count) as usize);
        (entries, CallOutcome::ok(call))
    }

    pub(super) fn inventory() -> Result<NvapiInventoryReport, String> {
        let mut calls = Vec::new();
        let loader = match Loader::new() {
            Ok(loader) => loader,
            Err(error) => {
                let findings = evaluate_spare_targets(&[], &[], &[]);
                return Ok(NvapiInventoryReport {
                    schema_version: SCHEMA_VERSION,
                    read_only: true,
                    nvapi_loaded: false,
                    driver_version: None,
                    driver_branch: None,
                    interface_version: None,
                    gdi_primary_display_id: None,
                    gpus: Vec::new(),
                    nvapi_display_config: Vec::new(),
                    ccd_paths: ccd_paths(&mut calls),
                    unattached_displays: Vec::new(),
                    findings,
                    calls: {
                        calls.push(CallOutcome::failed("nvapi64.dll", i32::MIN, error));
                        calls
                    },
                });
            }
        };

        let (driver_version, driver_branch) = driver_version(&loader, &mut calls);
        let interface_version = interface_version(&loader, &mut calls);
        let gdi_primary_display_id = gdi_primary_display_id(&loader, &mut calls);
        let nvapi_display_config = nvapi_display_config(&mut calls);
        let gpus = gpus(&loader, &nvapi_display_config, &mut calls);
        let unattached_displays = unattached_displays(&loader, &mut calls);
        let ccd_paths = ccd_paths(&mut calls);
        let findings = evaluate_spare_targets(&gpus, &nvapi_display_config, &ccd_paths);

        Ok(NvapiInventoryReport {
            schema_version: SCHEMA_VERSION,
            read_only: true,
            nvapi_loaded: true,
            driver_version,
            driver_branch,
            interface_version,
            gdi_primary_display_id,
            gpus,
            nvapi_display_config,
            ccd_paths,
            unattached_displays,
            findings,
            calls,
        })
    }

    fn driver_version(
        loader: &Loader,
        calls: &mut Vec<CallOutcome>,
    ) -> (Option<u32>, Option<String>) {
        let call = "NvAPI_SYS_GetDriverAndBranchVersion";
        let Some(entry) = loader.resolve::<DriverAndBranchFn>(ID_SYS_GET_DRIVER_AND_BRANCH_VERSION)
        else {
            calls.push(loader.missing(call));
            return (None, None);
        };
        let mut version = 0u32;
        let mut branch = [0i8; NVAPI_SHORT_STRING_MAX];
        // SAFETY: both out parameters are writable and branch is the documented
        // 64-byte short string.
        let status = unsafe { entry(&mut version, branch.as_mut_ptr()) };
        calls.push(loader.outcome(call, status));
        if status != NVAPI_OK {
            return (None, None);
        }
        (Some(version), Some(short_string(&branch)))
    }

    fn interface_version(loader: &Loader, calls: &mut Vec<CallOutcome>) -> Option<String> {
        let call = "NvAPI_GetInterfaceVersionString";
        let Some(entry) = loader.resolve::<ShortStringOutFn>(ID_GET_INTERFACE_VERSION_STRING)
        else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut buffer = [0i8; NVAPI_SHORT_STRING_MAX];
        // SAFETY: buffer is the documented writable 64-byte short string.
        let status = unsafe { entry(buffer.as_mut_ptr()) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then(|| short_string(&buffer))
    }

    fn gdi_primary_display_id(loader: &Loader, calls: &mut Vec<CallOutcome>) -> Option<u32> {
        let call = "NvAPI_DISP_GetGDIPrimaryDisplayId";
        let Some(entry) = loader.resolve::<U32OutFn>(ID_DISP_GET_GDI_PRIMARY_DISPLAY_ID) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut display_id = 0u32;
        // SAFETY: display_id is a writable u32 out parameter.
        let status = unsafe { entry(&mut display_id) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then_some(display_id)
    }

    fn nvapi_display_config(calls: &mut Vec<CallOutcome>) -> Vec<NvapiConfigPathEntry> {
        use crate::nvapi::NvapiDriver;
        let call = "NvAPI_DISP_GetDisplayConfig";
        let mut driver = match crate::nvapi::Nvapi::load() {
            Ok(driver) => driver,
            Err(error) => {
                calls.push(CallOutcome::failed(call, i32::MIN, error));
                return Vec::new();
            }
        };
        match driver.get_display_config() {
            Ok(config) => {
                calls.push(CallOutcome::ok(call));
                config
                    .paths
                    .iter()
                    .map(|path| {
                        let geometry = path.source_geometry();
                        NvapiConfigPathEntry {
                            source_id: path.source_id,
                            non_nvidia_adapter: path.non_nvidia_adapter,
                            os_adapter_luid: path.os_adapter_luid,
                            width: geometry.width,
                            height: geometry.height,
                            color_depth: geometry.color_depth,
                            position_x: geometry.position_x,
                            position_y: geometry.position_y,
                            primary: geometry.primary,
                            targets: path
                                .targets
                                .iter()
                                .map(|target| NvapiConfigTarget {
                                    display_id: target.display_id,
                                    path_target_id: target.target_id,
                                })
                                .collect(),
                        }
                    })
                    .collect()
            }
            Err(error) => {
                calls.push(CallOutcome::failed(call, i32::MIN, error));
                Vec::new()
            }
        }
    }

    fn gpus(
        loader: &Loader,
        config: &[NvapiConfigPathEntry],
        calls: &mut Vec<CallOutcome>,
    ) -> Vec<GpuEntry> {
        let call = "NvAPI_EnumPhysicalGPUs";
        let Some(entry) = loader.resolve::<EnumPhysicalGpusFn>(ID_ENUM_PHYSICAL_GPUS) else {
            calls.push(loader.missing(call));
            return Vec::new();
        };
        let mut handles: [NvPhysicalGpuHandle; NVAPI_MAX_PHYSICAL_GPUS] =
            [std::ptr::null_mut(); NVAPI_MAX_PHYSICAL_GPUS];
        let mut count = 0u32;
        // SAFETY: the array holds the documented maximum and count is writable.
        let status = unsafe { entry(handles.as_mut_ptr(), &mut count) };
        calls.push(loader.outcome(call, status));
        if status != NVAPI_OK {
            return Vec::new();
        }
        let configured: Vec<u32> = config
            .iter()
            .filter(|path| !path.non_nvidia_adapter)
            .flat_map(|path| path.targets.iter().map(|target| target.display_id))
            .collect();
        handles
            .iter()
            .take((count as usize).min(NVAPI_MAX_PHYSICAL_GPUS))
            .enumerate()
            .map(|(index, handle)| gpu_entry(loader, index as u32, *handle, &configured))
            .collect()
    }

    fn gpu_entry(
        loader: &Loader,
        index: u32,
        handle: NvPhysicalGpuHandle,
        configured: &[u32],
    ) -> GpuEntry {
        let mut calls = Vec::new();
        let full_name = gpu_short_string(
            loader,
            handle,
            ID_GPU_GET_FULL_NAME,
            "NvAPI_GPU_GetFullName",
            &mut calls,
        );
        let vbios_version = gpu_short_string(
            loader,
            handle,
            ID_GPU_GET_VBIOS_VERSION_STRING,
            "NvAPI_GPU_GetVbiosVersionString",
            &mut calls,
        );
        let gpu_type = gpu_i32(
            loader,
            handle,
            ID_GPU_GET_GPU_TYPE,
            "NvAPI_GPU_GetGPUType",
            &mut calls,
        )
        .map(|value| value as u32);
        let system_type = gpu_i32(
            loader,
            handle,
            ID_GPU_GET_SYSTEM_TYPE,
            "NvAPI_GPU_GetSystemType",
            &mut calls,
        )
        .map(|value| value as u32);
        let quadro = gpu_u32(
            loader,
            handle,
            ID_GPU_GET_QUADRO_STATUS,
            "NvAPI_GPU_GetQuadroStatus",
            &mut calls,
        )
        .map(|value| value != 0);
        let physical_framebuffer_kib = gpu_u32(
            loader,
            handle,
            ID_GPU_GET_PHYSICAL_FRAME_BUFFER_SIZE,
            "NvAPI_GPU_GetPhysicalFrameBufferSize",
            &mut calls,
        );
        let virtualization_mode = virtualization_mode(loader, handle, &mut calls);
        let board_number = board_number(loader, handle, &mut calls);
        let pci = pci_identifiers(loader, handle, &mut calls);
        let adapter_luid = adapter_luid(loader, handle, &mut calls);

        let all_outputs_mask = gpu_u32(
            loader,
            handle,
            ID_GPU_GET_ALL_OUTPUTS,
            "NvAPI_GPU_GetAllOutputs",
            &mut calls,
        );
        let connected_outputs_mask = gpu_u32(
            loader,
            handle,
            ID_GPU_GET_CONNECTED_OUTPUTS,
            "NvAPI_GPU_GetConnectedOutputs",
            &mut calls,
        );
        let active_outputs_mask = gpu_u32(
            loader,
            handle,
            ID_GPU_GET_ACTIVE_OUTPUTS,
            "NvAPI_GPU_GetActiveOutputs",
            &mut calls,
        );

        let outputs = outputs(
            loader,
            handle,
            all_outputs_mask,
            connected_outputs_mask,
            active_outputs_mask,
            &mut calls,
        );
        let displays = displays(loader, handle, &outputs, configured, &mut calls);

        GpuEntry {
            index,
            full_name,
            gpu_type,
            system_type,
            quadro,
            virtualization_mode,
            virtualization_mode_name: virtualization_mode
                .map(|mode| virtualization_mode_name(mode).to_string()),
            board_number,
            vbios_version,
            pci,
            physical_framebuffer_kib,
            adapter_luid,
            all_outputs_mask,
            connected_outputs_mask,
            active_outputs_mask,
            outputs,
            displays,
            calls,
        }
    }

    fn gpu_short_string(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        id: u32,
        call: &str,
        calls: &mut Vec<CallOutcome>,
    ) -> Option<String> {
        let Some(entry) = loader.resolve::<GpuShortStringFn>(id) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut buffer = [0i8; NVAPI_SHORT_STRING_MAX];
        // SAFETY: handle came from NvAPI_EnumPhysicalGPUs and buffer is the
        // documented writable 64-byte short string.
        let status = unsafe { entry(handle, buffer.as_mut_ptr()) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then(|| short_string(&buffer))
    }

    fn gpu_u32(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        id: u32,
        call: &str,
        calls: &mut Vec<CallOutcome>,
    ) -> Option<u32> {
        let Some(entry) = loader.resolve::<GpuU32OutFn>(id) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut value = 0u32;
        // SAFETY: handle came from NvAPI_EnumPhysicalGPUs and value is writable.
        let status = unsafe { entry(handle, &mut value) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then_some(value)
    }

    fn gpu_i32(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        id: u32,
        call: &str,
        calls: &mut Vec<CallOutcome>,
    ) -> Option<i32> {
        let Some(entry) = loader.resolve::<GpuI32OutFn>(id) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut value = 0i32;
        // SAFETY: handle came from NvAPI_EnumPhysicalGPUs and value is writable.
        let status = unsafe { entry(handle, &mut value) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then_some(value)
    }

    fn virtualization_mode(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        calls: &mut Vec<CallOutcome>,
    ) -> Option<u32> {
        let call = "NvAPI_GPU_GetVirtualizationInfo";
        let Some(entry) = loader.resolve::<GpuStructOutFn>(ID_GPU_GET_VIRTUALIZATION_INFO) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut info = NvVirtualizationInfo {
            version: nvapi_version::<NvVirtualizationInfo>(1),
            ..NvVirtualizationInfo::default()
        };
        // SAFETY: info is a writable versioned structure and handle came from NVAPI.
        let status = unsafe { entry(handle, (&mut info as *mut NvVirtualizationInfo).cast()) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then_some(info.virtualization_mode)
    }

    fn board_number(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        calls: &mut Vec<CallOutcome>,
    ) -> Option<String> {
        let call = "NvAPI_GPU_GetBoardInfo";
        let Some(entry) = loader.resolve::<GpuStructOutFn>(ID_GPU_GET_BOARD_INFO) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut info = NvBoardInfo {
            version: nvapi_version::<NvBoardInfo>(1),
            ..NvBoardInfo::default()
        };
        // SAFETY: info is a writable versioned structure and handle came from NVAPI.
        let status = unsafe { entry(handle, (&mut info as *mut NvBoardInfo).cast()) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then(|| {
            String::from_utf8_lossy(
                &info
                    .board_number
                    .iter()
                    .copied()
                    .take_while(|byte| *byte != 0)
                    .collect::<Vec<u8>>(),
            )
            .into_owned()
        })
    }

    fn pci_identifiers(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        calls: &mut Vec<CallOutcome>,
    ) -> Option<PciIdentifiers> {
        let call = "NvAPI_GPU_GetPCIIdentifiers";
        let Some(entry) = loader.resolve::<GetPciIdentifiersFn>(ID_GPU_GET_PCI_IDENTIFIERS) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut device_id = 0u32;
        let mut subsystem_id = 0u32;
        let mut revision_id = 0u32;
        let mut external_device_id = 0u32;
        // SAFETY: all four out parameters are writable and handle came from NVAPI.
        let status = unsafe {
            entry(
                handle,
                &mut device_id,
                &mut subsystem_id,
                &mut revision_id,
                &mut external_device_id,
            )
        };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then_some(PciIdentifiers {
            device_id,
            subsystem_id,
            revision_id,
            external_device_id,
        })
    }

    fn adapter_luid(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        calls: &mut Vec<CallOutcome>,
    ) -> Option<AdapterLuid> {
        let call = "NvAPI_GPU_GetAdapterIdFromPhysicalGpu";
        let Some(entry) = loader.resolve::<GpuStructOutFn>(ID_GPU_GET_ADAPTER_ID) else {
            calls.push(loader.missing(call));
            return None;
        };
        let mut luid = AdapterLuid::default();
        // SAFETY: luid is a writable LUID-shaped structure and handle came from NVAPI.
        let status = unsafe { entry(handle, (&mut luid as *mut AdapterLuid).cast()) };
        calls.push(loader.outcome(call, status));
        (status == NVAPI_OK).then_some(luid)
    }

    fn outputs(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        all: Option<u32>,
        connected: Option<u32>,
        active: Option<u32>,
        calls: &mut Vec<CallOutcome>,
    ) -> Vec<OutputEntry> {
        let union_mask = all.unwrap_or(0) | connected.unwrap_or(0) | active.unwrap_or(0);
        if union_mask == 0 {
            return Vec::new();
        }
        let output_type = loader.resolve::<GetOutputTypeFn>(ID_GPU_GET_OUTPUT_TYPE);
        if output_type.is_none() {
            calls.push(loader.missing("NvAPI_GPU_GetOutputType"));
        }
        let display_id_from_output = loader
            .resolve::<DisplayIdFromGpuAndOutputFn>(ID_SYS_GET_DISPLAY_ID_FROM_GPU_AND_OUTPUT_ID);
        if display_id_from_output.is_none() {
            calls.push(loader.missing("NvAPI_SYS_GetDisplayIdFromGpuAndOutputId"));
        }
        let mut entries = Vec::new();
        for output_id in decode_output_mask(union_mask) {
            let mut entry = OutputEntry {
                output_id,
                bit_index: output_id.trailing_zeros(),
                in_all_mask: all.is_some_and(|mask| mask & output_id != 0),
                connected: connected.is_some_and(|mask| mask & output_id != 0),
                active: active.is_some_and(|mask| mask & output_id != 0),
                output_type: None,
                display_id: None,
                display_id_lookup: None,
            };
            if let Some(get_output_type) = output_type {
                let mut value = 0i32;
                // SAFETY: handle came from NVAPI, output_id is a single-bit mask from the
                // driver's own masks, and value is writable.
                let status = unsafe { get_output_type(handle, output_id, &mut value) };
                if status == NVAPI_OK {
                    entry.output_type = Some(value);
                }
            }
            if let Some(to_display_id) = display_id_from_output {
                let mut display_id = 0u32;
                // SAFETY: handle came from NVAPI, output_id is a single-bit mask, and
                // display_id is writable.
                let status = unsafe { to_display_id(handle, output_id, &mut display_id) };
                entry.display_id_lookup =
                    Some(loader.outcome("NvAPI_SYS_GetDisplayIdFromGpuAndOutputId", status));
                if status == NVAPI_OK {
                    entry.display_id = Some(display_id);
                }
            }
            entries.push(entry);
        }
        entries
    }

    fn displays(
        loader: &Loader,
        handle: NvPhysicalGpuHandle,
        outputs: &[OutputEntry],
        configured: &[u32],
        calls: &mut Vec<CallOutcome>,
    ) -> Vec<DisplayIdEntry> {
        let mut collected: Vec<DisplayIdEntry> = Vec::new();

        let mut record = |entry: NvGpuDisplayIds, source: DisplayIdSource| {
            if let Some(existing) = collected
                .iter_mut()
                .find(|candidate| candidate.display_id == entry.display_id)
            {
                if !existing.sources.contains(&source) {
                    existing.sources.push(source);
                }
                existing.flags.raw |= entry.flags;
                let merged = decode_display_id_flags(existing.flags.raw);
                existing.flags = merged;
                return;
            }
            collected.push(DisplayIdEntry {
                display_id: entry.display_id,
                connector_type: entry.connector_type,
                flags: decode_display_id_flags(entry.flags),
                sources: vec![source],
                output_id: None,
                edid: EdidProbe::default(),
                in_nvapi_display_config: configured.contains(&entry.display_id),
            });
        };

        if let Some(get_all) = loader.resolve::<GetAllDisplayIdsFn>(ID_GPU_GET_ALL_DISPLAY_IDS) {
            let (entries, outcome) =
                display_id_list(loader, "NvAPI_GPU_GetAllDisplayIds", |ptr, count| {
                    // SAFETY: handle came from NVAPI; ptr is either null (count query) or a
                    // versioned array of `*count` entries.
                    unsafe { get_all(handle, ptr, count) }
                });
            calls.push(outcome);
            for entry in entries {
                record(entry, DisplayIdSource::AllDisplayIds);
            }
        } else {
            calls.push(loader.missing("NvAPI_GPU_GetAllDisplayIds"));
        }

        if let Some(get_connected) =
            loader.resolve::<GetConnectedDisplayIdsFn>(ID_GPU_GET_CONNECTED_DISPLAY_IDS)
        {
            for (flags, source, label) in [
                (0u32, DisplayIdSource::ConnectedCached, "cached"),
                (
                    CONNECTED_IDS_FLAG_UNCACHED,
                    DisplayIdSource::ConnectedUncached,
                    "uncached",
                ),
                (
                    CONNECTED_IDS_FLAG_FAKE,
                    DisplayIdSource::ConnectedFake,
                    "fake",
                ),
            ] {
                let call = format!("NvAPI_GPU_GetConnectedDisplayIds({label})");
                let (entries, outcome) = display_id_list(loader, &call, |ptr, count| {
                    // SAFETY: handle came from NVAPI; ptr is either null (count query) or a
                    // versioned array of `*count` entries.
                    unsafe { get_connected(handle, ptr, count, flags) }
                });
                calls.push(outcome);
                for entry in entries {
                    record(entry, source);
                }
            }
        } else {
            calls.push(loader.missing("NvAPI_GPU_GetConnectedDisplayIds"));
        }

        for output in outputs {
            if let Some(display_id) = output.display_id {
                record(
                    NvGpuDisplayIds {
                        version: 0,
                        connector_type: -1,
                        display_id,
                        flags: 0,
                    },
                    DisplayIdSource::OutputMask,
                );
            }
        }

        let gpu_and_output = loader
            .resolve::<GpuAndOutputFromDisplayIdFn>(ID_SYS_GET_GPU_AND_OUTPUT_ID_FROM_DISPLAY_ID);
        if gpu_and_output.is_none() {
            calls.push(loader.missing("NvAPI_SYS_GetGpuAndOutputIdFromDisplayId"));
        }
        let get_edid = loader.resolve::<GetEdidFn>(ID_GPU_GET_EDID);
        if get_edid.is_none() {
            calls.push(loader.missing("NvAPI_GPU_GetEDID"));
        }

        for display in &mut collected {
            display.output_id = outputs
                .iter()
                .find(|output| output.display_id == Some(display.display_id))
                .map(|output| output.output_id);
            if display.output_id.is_none() {
                if let Some(resolve) = gpu_and_output {
                    let mut gpu = std::ptr::null_mut();
                    let mut output_id = 0u32;
                    // SAFETY: display_id came from the driver's own enumeration and both
                    // out parameters are writable.
                    let status = unsafe { resolve(display.display_id, &mut gpu, &mut output_id) };
                    if status == NVAPI_OK {
                        display.output_id = Some(output_id);
                    }
                }
            }
            let Some(get_edid) = get_edid else {
                continue;
            };
            let Some(output_id) = display.output_id else {
                display.edid = EdidProbe {
                    queried: false,
                    status: NVAPI_DATA_NOT_FOUND,
                    detail: "no output id resolved for this display id".to_string(),
                    ..EdidProbe::default()
                };
                continue;
            };
            let mut edid = NvEdid::default();
            // SAFETY: handle came from NVAPI, output_id is the driver's own single-bit
            // mask, and edid is a writable versioned structure.
            let status = unsafe { get_edid(handle, output_id, &mut edid) };
            if status != NVAPI_OK {
                display.edid = EdidProbe {
                    queried: true,
                    status,
                    detail: loader.message(status),
                    ..EdidProbe::default()
                };
                continue;
            }
            let length = (edid.size as usize).min(edid.data.len());
            let bytes = &edid.data[..length];
            let mut probe = summarize_edid(bytes);
            probe.status = NVAPI_OK;
            probe.sha256 = (!bytes.is_empty()).then(|| sha256_hex(bytes));
            display.edid = probe;
        }

        collected.sort_by_key(|display| display.display_id);
        collected
    }

    fn unattached_displays(
        loader: &Loader,
        calls: &mut Vec<CallOutcome>,
    ) -> Vec<UnattachedDisplayEntry> {
        let call = "NvAPI_EnumNvidiaUnAttachedDisplayHandle";
        let Some(enumerate) = loader.resolve::<EnumUnattachedFn>(ID_ENUM_UNATTACHED_DISPLAY_HANDLE)
        else {
            calls.push(loader.missing(call));
            return Vec::new();
        };
        let name_of = loader.resolve::<UnattachedNameFn>(ID_GET_UNATTACHED_ASSOCIATED_DISPLAY_NAME);
        let mut entries = Vec::new();
        for index in 0..MAX_UNATTACHED_DISPLAYS {
            let mut handle: NvUnAttachedDisplayHandle = std::ptr::null_mut();
            // SAFETY: handle is a writable out parameter; the API reports the end of the
            // enumeration with NVAPI_END_ENUMERATION.
            let status = unsafe { enumerate(index, &mut handle) };
            if status == NVAPI_END_ENUMERATION {
                calls.push(CallOutcome::ok(call));
                return entries;
            }
            if status != NVAPI_OK {
                calls.push(loader.outcome(call, status));
                return entries;
            }
            let mut name = None;
            if let Some(name_of) = name_of {
                let mut buffer = [0i8; NVAPI_SHORT_STRING_MAX];
                // SAFETY: handle came from the enumeration above and buffer is the
                // documented writable 64-byte short string.
                if unsafe { name_of(handle, buffer.as_mut_ptr()) } == NVAPI_OK {
                    name = Some(short_string(&buffer));
                }
            }
            entries.push(UnattachedDisplayEntry { index, name });
        }
        calls.push(CallOutcome::failed(
            call,
            i32::MIN,
            format!("stopped after {MAX_UNATTACHED_DISPLAYS} handles"),
        ));
        entries
    }

    fn ccd_paths(calls: &mut Vec<CallOutcome>) -> Vec<CcdPathEntry> {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        // SAFETY: both counters are writable u32 out parameters.
        let sizes =
            unsafe { GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut path_count, &mut mode_count) };
        if sizes != ERROR_SUCCESS {
            calls.push(CallOutcome::failed(
                "GetDisplayConfigBufferSizes",
                sizes.0 as i32,
                "buffer size query failed",
            ));
            return Vec::new();
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        // SAFETY: both vectors are initialized and sized to the counts returned above.
        let query = unsafe {
            QueryDisplayConfig(
                QDC_ALL_PATHS,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if query != ERROR_SUCCESS {
            let detail = if query == ERROR_INSUFFICIENT_BUFFER {
                "topology changed during the query"
            } else {
                "query failed"
            };
            calls.push(CallOutcome::failed(
                "QueryDisplayConfig",
                query.0 as i32,
                detail,
            ));
            return Vec::new();
        }
        calls.push(CallOutcome::ok("QueryDisplayConfig(QDC_ALL_PATHS)"));
        paths.truncate(path_count as usize);
        paths.iter().map(ccd_path_entry).collect()
    }

    fn ccd_path_entry(path: &DISPLAYCONFIG_PATH_INFO) -> CcdPathEntry {
        const DISPLAYCONFIG_PATH_ACTIVE: u32 = 0x0000_0001;
        CcdPathEntry {
            source_adapter_luid: luid(path.sourceInfo.adapterId),
            source_id: path.sourceInfo.id,
            target_adapter_luid: luid(path.targetInfo.adapterId),
            target_id: path.targetInfo.id,
            active: path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0,
            target_available: path.targetInfo.targetAvailable.as_bool(),
            status_flags: path.targetInfo.statusFlags,
            output_technology: path.targetInfo.outputTechnology.0,
            gdi_device_name: source_device_name(path),
            monitor_device_path: target_device_path(path).map(|value| value.0),
            monitor_friendly_name: target_device_path(path).map(|value| value.1),
        }
    }

    fn luid(value: LUID) -> AdapterLuid {
        AdapterLuid {
            low_part: value.LowPart,
            high_part: value.HighPart,
        }
    }

    fn source_device_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
        let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the packet carries the correct type/size header and stays valid
        // for this synchronous call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut request.header) } != 0 {
            return None;
        }
        Some(utf16(&request.viewGdiDeviceName))
    }

    fn target_device_path(path: &DISPLAYCONFIG_PATH_INFO) -> Option<(String, String)> {
        let mut request = DISPLAYCONFIG_TARGET_DEVICE_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            },
            ..Default::default()
        };
        // SAFETY: the packet carries the correct type/size header and stays valid
        // for this synchronous call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut request.header) } != 0 {
            return None;
        }
        Some((
            utf16(&request.monitorDevicePath),
            utf16(&request.monitorFriendlyDeviceName),
        ))
    }

    #[allow(dead_code)]
    fn adapter_device_path(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
        let mut request = DISPLAYCONFIG_ADAPTER_NAME {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: 0,
            },
            ..Default::default()
        };
        // SAFETY: the packet carries the correct type/size header and stays valid
        // for this synchronous call.
        if unsafe { DisplayConfigGetDeviceInfo(&mut request.header) } != 0 {
            return None;
        }
        Some(utf16(&request.adapterDevicePath))
    }

    fn utf16(value: &[u16]) -> String {
        let length = value
            .iter()
            .position(|unit| *unit == 0)
            .unwrap_or(value.len());
        String::from_utf16_lossy(&value[..length])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gpu(index: u32, displays: Vec<DisplayIdEntry>, outputs: Vec<OutputEntry>) -> GpuEntry {
        GpuEntry {
            index,
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
                low_part: 0x1234,
                high_part: 0,
            }),
            all_outputs_mask: Some(0x1),
            connected_outputs_mask: Some(0x1),
            active_outputs_mask: Some(0x1),
            outputs,
            displays,
            calls: Vec::new(),
        }
    }

    fn display(display_id: u32, flags: u32, output_id: Option<u32>) -> DisplayIdEntry {
        DisplayIdEntry {
            display_id,
            connector_type: 0,
            flags: decode_display_id_flags(flags),
            sources: vec![DisplayIdSource::AllDisplayIds],
            output_id,
            edid: EdidProbe::default(),
            in_nvapi_display_config: false,
        }
    }

    fn ccd_path(
        adapter: AdapterLuid,
        source_id: u32,
        target_id: u32,
        active: bool,
    ) -> CcdPathEntry {
        CcdPathEntry {
            source_adapter_luid: adapter,
            source_id,
            target_adapter_luid: adapter,
            target_id,
            active,
            target_available: true,
            status_flags: 0,
            output_technology: 5,
            gdi_device_name: None,
            monitor_device_path: None,
            monitor_friendly_name: None,
        }
    }

    fn config_path(display_id: u32, target_id: u32) -> NvapiConfigPathEntry {
        NvapiConfigPathEntry {
            source_id: 0,
            non_nvidia_adapter: false,
            os_adapter_luid: None,
            width: 1920,
            height: 1080,
            color_depth: 32,
            position_x: 0,
            position_y: 0,
            primary: true,
            targets: vec![NvapiConfigTarget {
                display_id,
                path_target_id: target_id,
            }],
        }
    }

    #[test]
    fn decodes_every_documented_display_id_flag() {
        let flags = decode_display_id_flags(0x0002_007f);
        assert!(flags.dynamic);
        assert!(flags.multi_stream_root_node);
        assert!(flags.active);
        assert!(flags.cluster);
        assert!(flags.os_visible);
        assert!(flags.wfd);
        assert!(flags.connected);
        assert!(flags.physically_connected);
        assert_eq!(flags.raw, 0x0002_007f);
    }

    #[test]
    fn decodes_no_flags_for_an_inactive_display_id() {
        let flags = decode_display_id_flags(0);
        assert!(!flags.active);
        assert!(!flags.connected);
        assert!(!flags.physically_connected);
    }

    #[test]
    fn expands_an_output_mask_into_single_bit_ids() {
        assert_eq!(decode_output_mask(0x0000_0005), vec![0x1, 0x4]);
        assert_eq!(decode_output_mask(0), Vec::<u32>::new());
        assert_eq!(
            decode_output_mask(0x8000_0000),
            vec![0x8000_0000],
            "the top bit must not be lost to shift overflow"
        );
    }

    #[test]
    fn names_the_connector_and_output_types_the_lab_reports() {
        assert_eq!(connector_type_name(5), "dvi");
        assert_eq!(connector_type_name(7), "displayport");
        assert_eq!(connector_type_name(-1), "unknown");
        assert_eq!(connector_type_name(99), "unknown");
        assert_eq!(output_type_name(2), "dfp");
        assert_eq!(output_type_name(0), "unknown");
        assert_eq!(output_type_name(42), "unrecognized");
    }

    #[test]
    fn reports_no_nvidia_gpu_when_enumeration_is_empty() {
        let findings = evaluate_spare_targets(&[], &[], &[]);
        assert_eq!(findings.verdict, SpareTargetVerdict::NoNvidiaGpu);
    }

    #[test]
    fn reports_no_spare_targets_when_every_display_id_is_active() {
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x0002_0044 | 0x4, Some(0x1))],
            Vec::new(),
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &[]);
        assert_eq!(findings.verdict, SpareTargetVerdict::NoSpareTargets);
        assert_eq!(findings.nvidia_display_ids_total, 1);
        assert_eq!(findings.nvidia_display_ids_active, 1);
        assert!(findings.spare_display_ids.is_empty());
    }

    #[test]
    fn reports_spare_display_ids_when_one_is_inactive_and_unconfigured() {
        let mut inactive = display(0x8006_2f81, 0x0, Some(0x2));
        inactive.edid = EdidProbe {
            queried: true,
            status: -121,
            detail: "NVAPI_DATA_NOT_FOUND".to_string(),
            ..EdidProbe::default()
        };
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x4, Some(0x1)), inactive],
            Vec::new(),
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &[]);
        assert_eq!(findings.verdict, SpareTargetVerdict::SpareDisplayIdsPresent);
        assert_eq!(findings.spare_display_ids.len(), 1);
        assert_eq!(findings.spare_display_ids[0].display_id, 0x8006_2f81);
        assert_eq!(findings.spare_display_ids[0].edid_status, -121);
        assert!(!findings.spare_display_ids[0].edid_present);
        assert!(findings
            .rationale
            .iter()
            .any(|line| line.contains("No spare display id carries an EDID")));
    }

    #[test]
    fn a_spare_display_id_that_already_carries_an_edid_is_reported_as_such() {
        let mut inactive = display(0x8006_2f81, 0x0, Some(0x2));
        inactive.edid = EdidProbe {
            queried: true,
            status: 0,
            byte_length: 128,
            ..EdidProbe::default()
        };
        let gpus = vec![gpu(0, vec![inactive], Vec::new())];
        let findings = evaluate_spare_targets(&gpus, &[], &[]);
        assert!(findings.spare_display_ids[0].edid_present);
        assert!(findings
            .rationale
            .iter()
            .any(|line| line.contains("already carry an EDID")));
    }

    #[test]
    fn an_inactive_display_id_already_in_the_nvapi_config_is_not_spare() {
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f81, 0x0, Some(0x2))],
            Vec::new(),
        )];
        let config = vec![config_path(0x8006_2f81, 0)];
        let findings = evaluate_spare_targets(&gpus, &config, &[]);
        assert_eq!(findings.verdict, SpareTargetVerdict::NoSpareTargets);
        assert!(findings.spare_display_ids.is_empty());
    }

    #[test]
    fn an_output_id_without_a_display_id_is_reported_separately() {
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x4, Some(0x1))],
            vec![
                OutputEntry {
                    output_id: 0x1,
                    bit_index: 0,
                    in_all_mask: true,
                    connected: true,
                    active: true,
                    output_type: Some(2),
                    display_id: Some(0x8006_2f80),
                    display_id_lookup: None,
                },
                OutputEntry {
                    output_id: 0x2,
                    bit_index: 1,
                    in_all_mask: true,
                    connected: false,
                    active: false,
                    output_type: Some(2),
                    display_id: None,
                    display_id_lookup: None,
                },
            ],
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &[]);
        assert_eq!(
            findings.verdict,
            SpareTargetVerdict::SpareOutputIdsWithoutDisplayIds
        );
        assert_eq!(findings.spare_output_ids.len(), 1);
        assert_eq!(findings.spare_output_ids[0].output_id, 0x2);
    }

    #[test]
    fn counts_distinct_nvidia_ccd_targets_and_paths_separately() {
        let nvidia = AdapterLuid {
            low_part: 0x1234,
            high_part: 0,
        };
        let other = AdapterLuid {
            low_part: 0x9999,
            high_part: 0,
        };
        let ccd = vec![
            ccd_path(nvidia, 0, 100, true),
            ccd_path(nvidia, 1, 100, false),
            ccd_path(nvidia, 0, 101, false),
            ccd_path(nvidia, 1, 101, false),
            ccd_path(other, 0, 200, false),
        ];
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x4, Some(0x1))],
            Vec::new(),
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &ccd);
        assert_eq!(findings.nvidia_ccd_paths_total, 4);
        assert_eq!(findings.nvidia_ccd_targets_total, 2);
        assert_eq!(findings.nvidia_ccd_targets_active, 1);
        assert_eq!(findings.nvidia_ccd_targets_inactive, 1);
    }

    #[test]
    fn a_target_active_on_any_path_is_never_reported_as_spare() {
        let nvidia = AdapterLuid {
            low_part: 0x1234,
            high_part: 0,
        };
        // pier-windows.example.internal reports exactly this shape: the one live target repeats as
        // an inactive path against every other source id on the same adapter.
        let ccd = vec![
            ccd_path(nvidia, 0, 24832, true),
            ccd_path(nvidia, 1, 24832, false),
            ccd_path(nvidia, 2, 24832, false),
            ccd_path(nvidia, 3, 24832, false),
        ];
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x4, Some(0x1))],
            Vec::new(),
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &ccd);
        assert_eq!(findings.nvidia_ccd_paths_total, 4);
        assert_eq!(findings.nvidia_ccd_targets_total, 1);
        assert_eq!(findings.nvidia_ccd_targets_active, 1);
        assert!(findings.spare_ccd_targets.is_empty());
    }

    #[test]
    fn an_unavailable_spare_ccd_target_is_reported_as_having_no_monitor() {
        let nvidia = AdapterLuid {
            low_part: 0x1234,
            high_part: 0,
        };
        let mut unavailable = ccd_path(nvidia, 0, 24833, false);
        unavailable.target_available = false;
        let mut also_unavailable = ccd_path(nvidia, 1, 24833, false);
        also_unavailable.target_available = false;
        let ccd = vec![
            ccd_path(nvidia, 0, 24832, true),
            unavailable,
            also_unavailable,
        ];
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x4, Some(0x1))],
            Vec::new(),
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &ccd);
        assert_eq!(findings.spare_ccd_targets.len(), 1);
        assert_eq!(findings.spare_ccd_targets[0].target_id, 24833);
        assert!(!findings.spare_ccd_targets[0].target_available);
        assert_eq!(findings.spare_ccd_targets[0].inactive_paths, 2);
        assert_eq!(findings.nvidia_ccd_targets_inactive_available, 0);
        assert!(findings
            .rationale
            .iter()
            .any(|line| line.contains("targetAvailable=false")
                && line.contains("no monitor behind it")));
    }

    #[test]
    fn an_available_but_inactive_spare_ccd_target_is_counted_as_monitor_backed() {
        let nvidia = AdapterLuid {
            low_part: 0x1234,
            high_part: 0,
        };
        let mut backed = ccd_path(nvidia, 1, 24833, false);
        backed.monitor_friendly_name = Some("Arcen".to_string());
        let ccd = vec![ccd_path(nvidia, 0, 24832, true), backed];
        let gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x4, Some(0x1))],
            Vec::new(),
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &ccd);
        assert_eq!(findings.nvidia_ccd_targets_inactive_available, 1);
        assert_eq!(
            findings.spare_ccd_targets[0]
                .monitor_friendly_name
                .as_deref(),
            Some("Arcen")
        );
        assert!(findings
            .rationale
            .iter()
            .any(|line| line.contains("targetAvailable=true")));
    }

    #[test]
    fn a_ccd_target_on_a_gpu_without_a_readable_luid_is_never_treated_as_nvidia() {
        let mut gpus = vec![gpu(
            0,
            vec![display(0x8006_2f80, 0x4, Some(0x1))],
            Vec::new(),
        )];
        gpus[0].adapter_luid = None;
        let ccd = vec![ccd_path(
            AdapterLuid {
                low_part: 0x1234,
                high_part: 0,
            },
            1,
            101,
            false,
        )];
        let findings = evaluate_spare_targets(&gpus, &[], &ccd);
        assert_eq!(findings.nvidia_ccd_targets_total, 0);
        assert!(findings.spare_ccd_targets.is_empty());
    }

    #[test]
    fn always_records_that_try_custom_display_is_not_creation_evidence() {
        let findings = evaluate_spare_targets(&[], &[], &[]);
        assert!(findings
            .rationale
            .iter()
            .any(|line| line.contains("TryCustomDisplay")));
    }

    #[test]
    fn summarizes_an_arcen_generated_edid() {
        let edid = crate::edid::generate(crate::edid::EdidRequest {
            width: 2560,
            height: 1440,
            refresh_hz: 60,
            width_mm: 600.0,
            height_mm: 340.0,
            scale: 1.0,
            product_id: 0x0000,
            serial: 1,
        })
        .expect("generate an Arcen EDID");
        let probe = summarize_edid(&edid);
        assert_eq!(probe.manufacturer.as_deref(), Some("ARN"));
        assert!(probe.written_by_arcen);
        assert_eq!(probe.preferred_width, Some(2560));
        assert_eq!(probe.preferred_height, Some(1440));
        assert_eq!(probe.byte_length, 128);
    }

    #[test]
    fn summarizes_a_truncated_edid_without_panicking() {
        let probe = summarize_edid(&[0u8; 16]);
        assert_eq!(probe.byte_length, 16);
        assert!(probe.manufacturer.is_none());
        assert!(!probe.written_by_arcen);
    }

    #[test]
    fn renders_a_summary_for_a_report_without_a_driver() {
        let report = NvapiInventoryReport {
            schema_version: SCHEMA_VERSION,
            read_only: true,
            nvapi_loaded: false,
            driver_version: None,
            driver_branch: None,
            interface_version: None,
            gdi_primary_display_id: None,
            gpus: Vec::new(),
            nvapi_display_config: Vec::new(),
            ccd_paths: Vec::new(),
            unattached_displays: Vec::new(),
            findings: evaluate_spare_targets(&[], &[], &[]),
            calls: Vec::new(),
        };
        let text = render_summary(&report);
        assert!(text.contains("read-only"));
        assert!(text.contains("NoNvidiaGpu"));
    }

    fn report_with_one_edid() -> NvapiInventoryReport {
        let edid = crate::edid::generate(crate::edid::EdidRequest {
            width: 1920,
            height: 1080,
            refresh_hz: 60,
            width_mm: 520.0,
            height_mm: 290.0,
            scale: 1.0,
            product_id: 0x0001,
            serial: 0x0dad_beef,
        })
        .expect("generate an Arcen EDID");
        let mut probe = summarize_edid(&edid);
        probe.sha256 = Some("00ff".repeat(16));
        let mut entry = display(0x8006_2f80, 0x4, Some(0x1));
        entry.edid = probe;
        let nvidia = AdapterLuid {
            low_part: 0x1234,
            high_part: 0,
        };
        let gpus = vec![gpu(0, vec![entry], Vec::new())];
        let config = vec![config_path(0x8006_2f80, 100)];
        let ccd = vec![
            ccd_path(nvidia, 0, 100, true),
            ccd_path(nvidia, 1, 101, false),
        ];
        let findings = evaluate_spare_targets(&gpus, &config, &ccd);
        NvapiInventoryReport {
            schema_version: SCHEMA_VERSION,
            read_only: true,
            nvapi_loaded: true,
            driver_version: Some(57604),
            driver_branch: Some("r576_00".to_string()),
            interface_version: Some("NVidia Complete Version 1.0".to_string()),
            gdi_primary_display_id: Some(0x8006_2f80),
            gpus,
            nvapi_display_config: config,
            ccd_paths: ccd,
            unattached_displays: Vec::new(),
            findings,
            calls: vec![CallOutcome::ok("NvAPI_Initialize")],
        }
    }

    #[test]
    fn the_json_report_carries_edid_metadata_but_never_raw_edid_bytes() {
        let report = report_with_one_edid();
        let json = serde_json::to_string(&report).expect("serialize the inventory report");
        let edid = &report.gpus[0].displays[0].edid;
        assert_eq!(edid.byte_length, 128);
        assert!(json.contains("\"sha256\":\"00ff"));
        assert!(json.contains("\"manufacturer\":\"ARN\""));
        assert!(
            !json.contains("\"data\""),
            "the report must never carry an EDID byte array"
        );
        let serialized_edid = serde_json::to_value(edid).expect("serialize the EDID probe");
        let fields: Vec<String> = serialized_edid
            .as_object()
            .expect("the EDID probe is a JSON object")
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            fields,
            vec![
                "byte_length",
                "detail",
                "manufacturer",
                "preferred_height",
                "preferred_width",
                "product_code",
                "queried",
                "sha256",
                "status",
                "written_by_arcen",
            ],
            "adding an EDID field requires re-checking that no raw bytes escape"
        );
    }

    #[test]
    fn the_json_report_round_trips_through_its_schema() {
        let report = report_with_one_edid();
        let json = serde_json::to_string(&report).expect("serialize the inventory report");
        let parsed: NvapiInventoryReport =
            serde_json::from_str(&json).expect("parse the inventory report back");
        assert_eq!(parsed, report);
        assert_eq!(parsed.schema_version, SCHEMA_VERSION);
        assert!(parsed.read_only);
    }

    #[test]
    fn the_summary_reports_spare_ccd_targets_and_failed_calls() {
        let mut report = report_with_one_edid();
        report
            .calls
            .push(CallOutcome::failed("NvAPI_GPU_GetEDID", -104, "not found"));
        let text = render_summary(&report);
        assert!(text.contains("spare ccd target 101"));
        assert!(text.contains("unavailable: NvAPI_GPU_GetEDID (status -104)"));
        assert!(text.contains("nvapi path source=0"));
        assert!(text.contains("0x80062f80/path-target:100"));
    }

    #[test]
    fn the_summary_lists_each_spare_display_once() {
        let mut report = report_with_one_edid();
        report.gpus[0].displays[0].flags.active = false;
        report.gpus[0].displays[0].in_nvapi_display_config = false;
        report.nvapi_display_config.clear();
        report.findings = evaluate_spare_targets(&report.gpus, &[], &report.ccd_paths);
        let needle = "spare display id 0x80062f80";
        assert_eq!(render_summary(&report).matches(needle).count(), 1);
    }
}
