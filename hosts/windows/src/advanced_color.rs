//! Windows HDR engagement after the session's final displays exist.
//!
//! An HDR EDID only makes Windows advertise support. Capture is genuinely wide
//! after Windows 11 reports `activeColorMode = HDR`: the legacy
//! `advancedColorEnabled` bit can also describe WCG and is not a sufficient HDR
//! gate on current Windows. Microsoft documents that HDR mode makes DWM compose
//! in FP16 scRGB. `bitsPerColorChannel` describes the final display link after
//! DWM and the display kernel, downstream of WGC capture, so it remains
//! diagnostic rather than a capture gate. This module runs in the interactive
//! session agent after NVIDIA headless provisioning and topology binding but
//! before capture starts. Callers pass the exact final display identities;
//! unrelated active outputs are never counted or mutated.

#![cfg(windows)]

use windows::Wdk::System::SystemServices::RtlGetVersion;
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, DisplayConfigSetDeviceInfo, GetDisplayConfigBufferSizes,
    QueryDisplayConfig, DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO, DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_DEVICE_INFO_TYPE,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use crate::logging::DISPLAY;
use crate::recovery::AdvancedColorRecoveryEntry;

const WINDOWS_11_FIRST_BUILD: u32 = 22_000;

// windows 0.58 predates these Windows 11 SDK declarations. Their numeric
// packet kinds and ABI below are taken from wingdi.h in SDK 10.0.26100.
const GET_ADVANCED_COLOR_INFO_2: DISPLAYCONFIG_DEVICE_INFO_TYPE =
    DISPLAYCONFIG_DEVICE_INFO_TYPE(15);
const SET_HDR_STATE: DISPLAYCONFIG_DEVICE_INFO_TYPE = DISPLAYCONFIG_DEVICE_INFO_TYPE(16);

const FLAG_ADVANCED_COLOR_SUPPORTED: u32 = 1 << 0;
const FLAG_ADVANCED_COLOR_ACTIVE: u32 = 1 << 1;
const FLAG_ADVANCED_COLOR_LIMITED_BY_POLICY: u32 = 1 << 3;
const FLAG_HDR_SUPPORTED: u32 = 1 << 4;
const FLAG_HDR_USER_ENABLED: u32 = 1 << 5;
const FLAG_WCG_SUPPORTED: u32 = 1 << 6;
const FLAG_WCG_USER_ENABLED: u32 = 1 << 7;

const COLOR_MODE_SDR: i32 = 0;
const COLOR_MODE_WCG: i32 = 1;
const COLOR_MODE_HDR: i32 = 2;

// Hardware measurement on the GRID host showed Windows taking up to roughly
// twenty-five seconds to publish HDR state after an EDID/topology change.
// This runs before capture and must fail closed, so leave measured headroom
// instead of racing the compositor and intermittently streaming SDR as PQ.
const ENGAGE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(45);
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

#[repr(C)]
struct LegacyAdvancedColorInfo {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
    colour_encoding: i32,
    bits_per_colour_channel: u32,
}

#[repr(C)]
struct AdvancedColorInfo2 {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    flags: u32,
    colour_encoding: i32,
    bits_per_colour_channel: u32,
    active_colour_mode: i32,
}

#[repr(C)]
struct SetColorState {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    enable: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorStateApi {
    Legacy,
    DistinctHdr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdvancedColorState {
    adapter: LUID,
    target: u32,
    adapter_device_path: String,
    monitor_device_path: String,
    api: ColorStateApi,
    advanced_color_supported: bool,
    advanced_color_active: bool,
    limited_by_policy: bool,
    hdr_supported: bool,
    hdr_user_enabled: bool,
    wcg_supported: bool,
    wcg_user_enabled: bool,
    active_colour_mode: i32,
    bits_per_colour_channel: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdvancedColorTarget {
    adapter_low: u32,
    adapter_high: i32,
    target: u32,
    adapter_device_path: String,
    monitor_device_path: String,
}

impl AdvancedColorTarget {
    fn from_path(path: &DISPLAYCONFIG_PATH_INFO) -> Result<Self, String> {
        Ok(Self {
            adapter_low: path.targetInfo.adapterId.LowPart,
            adapter_high: path.targetInfo.adapterId.HighPart,
            target: path.targetInfo.id,
            adapter_device_path: adapter_device_path(path)?,
            monitor_device_path: target_monitor_device_path(path)?,
        })
    }

    fn matches(&self, state: &AdvancedColorState) -> bool {
        if !self.adapter_device_path.is_empty() && !self.monitor_device_path.is_empty() {
            return self
                .adapter_device_path
                .eq_ignore_ascii_case(&state.adapter_device_path)
                && self
                    .monitor_device_path
                    .eq_ignore_ascii_case(&state.monitor_device_path);
        }
        self.adapter_low == state.adapter.LowPart
            && self.adapter_high == state.adapter.HighPart
            && self.target == state.target
    }
}

impl AdvancedColorState {
    fn recovery_target(&self, original_hdr_enabled: bool) -> AdvancedColorRecoveryEntry {
        AdvancedColorRecoveryEntry {
            adapter_device_path: self.adapter_device_path.clone(),
            monitor_device_path: self.monitor_device_path.clone(),
            adapter_low: self.adapter.LowPart,
            adapter_high: self.adapter.HighPart,
            target_id: self.target,
            original_hdr_enabled,
            arcen_changed: true,
        }
    }
}

fn windows_11_or_later() -> Result<bool, String> {
    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: `version` is a live, writable structure with the required size
    // field initialized for `RtlGetVersion`.
    let status = unsafe { RtlGetVersion(&mut version) };
    if status.0 < 0 {
        return Err(format!(
            "query Windows version before HDR state selection: NTSTATUS {:#x}",
            status.0
        ));
    }
    Ok(version.dwMajorVersion >= 10 && version.dwBuildNumber >= WINDOWS_11_FIRST_BUILD)
}

fn mode_name(mode: i32) -> &'static str {
    match mode {
        COLOR_MODE_SDR => "sdr",
        COLOR_MODE_WCG => "wcg",
        COLOR_MODE_HDR => "hdr",
        _ => "unknown",
    }
}

fn query_distinct_hdr(
    adapter: LUID,
    target: u32,
    adapter_device_path: String,
    monitor_device_path: String,
) -> Result<AdvancedColorState, String> {
    let mut info = AdvancedColorInfo2 {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: GET_ADVANCED_COLOR_INFO_2,
            size: std::mem::size_of::<AdvancedColorInfo2>() as u32,
            adapterId: adapter,
            id: target,
        },
        flags: 0,
        colour_encoding: 0,
        bits_per_colour_channel: 0,
        active_colour_mode: COLOR_MODE_SDR,
    };
    // SAFETY: `AdvancedColorInfo2` exactly mirrors the Windows 11 wingdi.h
    // packet and its header declares the packet kind and byte size.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut info.header) };
    if status != 0 {
        return Err(format!(
            "query distinct HDR state for target {target}: Win32 error {status}"
        ));
    }
    Ok(AdvancedColorState {
        adapter,
        target,
        adapter_device_path,
        monitor_device_path,
        api: ColorStateApi::DistinctHdr,
        advanced_color_supported: info.flags & FLAG_ADVANCED_COLOR_SUPPORTED != 0,
        advanced_color_active: info.flags & FLAG_ADVANCED_COLOR_ACTIVE != 0,
        limited_by_policy: info.flags & FLAG_ADVANCED_COLOR_LIMITED_BY_POLICY != 0,
        hdr_supported: info.flags & FLAG_HDR_SUPPORTED != 0,
        hdr_user_enabled: info.flags & FLAG_HDR_USER_ENABLED != 0,
        wcg_supported: info.flags & FLAG_WCG_SUPPORTED != 0,
        wcg_user_enabled: info.flags & FLAG_WCG_USER_ENABLED != 0,
        active_colour_mode: info.active_colour_mode,
        bits_per_colour_channel: info.bits_per_colour_channel,
    })
}

fn query_legacy(
    adapter: LUID,
    target: u32,
    adapter_device_path: String,
    monitor_device_path: String,
) -> Result<AdvancedColorState, String> {
    let mut info = LegacyAdvancedColorInfo {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
            size: std::mem::size_of::<LegacyAdvancedColorInfo>() as u32,
            adapterId: adapter,
            id: target,
        },
        flags: 0,
        colour_encoding: 0,
        bits_per_colour_channel: 0,
    };
    // SAFETY: `LegacyAdvancedColorInfo` is `repr(C)`, fully initialized, and
    // its header declares the exact packet kind and byte size Windows expects.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut info.header) };
    if status != 0 {
        return Err(format!(
            "query legacy Advanced Color state for target {target}: Win32 error {status}"
        ));
    }
    let supported = info.flags & FLAG_ADVANCED_COLOR_SUPPORTED != 0;
    let enabled = info.flags & FLAG_ADVANCED_COLOR_ACTIVE != 0;
    Ok(AdvancedColorState {
        adapter,
        target,
        adapter_device_path,
        monitor_device_path,
        api: ColorStateApi::Legacy,
        advanced_color_supported: supported,
        advanced_color_active: enabled,
        limited_by_policy: false,
        hdr_supported: supported,
        hdr_user_enabled: enabled,
        wcg_supported: false,
        wcg_user_enabled: false,
        active_colour_mode: if enabled {
            COLOR_MODE_HDR
        } else {
            COLOR_MODE_SDR
        },
        bits_per_colour_channel: info.bits_per_colour_channel,
    })
}

fn active_paths() -> Result<Vec<DISPLAYCONFIG_PATH_INFO>, String> {
    let mut path_count = 0_u32;
    let mut mode_count = 0_u32;
    // SAFETY: both count pointers name initialized writable `u32` values.
    let status = unsafe {
        GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "size active display configuration: Win32 error {}",
            status.0
        ));
    }

    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
    let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
    // SAFETY: the buffers are sized from the immediately preceding query and
    // remain live for the call. Their count pointers match their capacities.
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        )
    };
    if status != ERROR_SUCCESS {
        return Err(format!(
            "query active display configuration: Win32 error {}",
            status.0
        ));
    }
    paths.truncate(path_count as usize);
    Ok(paths)
}

fn source_gdi_name(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, String> {
    let mut request = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: path.sourceInfo.adapterId,
            id: path.sourceInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: the request packet is initialized with the documented type and
    // byte size and remains writable for the synchronous call.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
    if status != 0 {
        return Err(format!(
            "query source name for display source {}: Win32 error {status}",
            path.sourceInfo.id
        ));
    }
    let end = request
        .viewGdiDeviceName
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(request.viewGdiDeviceName.len());
    Ok(String::from_utf16_lossy(&request.viewGdiDeviceName[..end]))
}

fn utf16_nul_terminated(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn target_monitor_device_path(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, String> {
    let mut request = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: path.targetInfo.id,
        },
        ..Default::default()
    };
    // SAFETY: the request packet is initialized with the documented type and
    // byte size and remains writable for the synchronous call.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
    if status != 0 {
        return Err(format!(
            "query target name for display target {}: Win32 error {status}",
            path.targetInfo.id
        ));
    }
    let value = utf16_nul_terminated(&request.monitorDevicePath);
    if value.is_empty() {
        return Err(format!(
            "display target {} has no stable monitor device path",
            path.targetInfo.id
        ));
    }
    Ok(value)
}

fn adapter_device_path(path: &DISPLAYCONFIG_PATH_INFO) -> Result<String, String> {
    let mut request = DISPLAYCONFIG_ADAPTER_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
            adapterId: path.targetInfo.adapterId,
            id: 0,
        },
        ..Default::default()
    };
    // SAFETY: the request packet is initialized with the documented type and
    // byte size and remains writable for the synchronous call.
    let status = unsafe { DisplayConfigGetDeviceInfo(&mut request.header) };
    if status != 0 {
        return Err(format!(
            "query adapter name for display target {}: Win32 error {status}",
            path.targetInfo.id
        ));
    }
    let value = utf16_nul_terminated(&request.adapterDevicePath);
    if value.is_empty() {
        return Err(format!(
            "display target {} has no stable adapter device path",
            path.targetInfo.id
        ));
    }
    Ok(value)
}

pub(crate) fn targets_for_device_names(
    device_names: &[String],
) -> Result<Vec<AdvancedColorTarget>, String> {
    if device_names.is_empty() {
        return Err("HDR session requires at least one display target".to_string());
    }
    let paths = active_paths()?;
    let named_paths = paths
        .iter()
        .map(|path| source_gdi_name(path).map(|name| (name, path)))
        .collect::<Result<Vec<_>, _>>()?;
    let mut targets = Vec::with_capacity(device_names.len());
    for device_name in device_names {
        let matches = named_paths
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case(device_name))
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return Err(format!(
                "HDR target {device_name:?} resolved to {} active display paths",
                matches.len()
            ));
        }
        let target = AdvancedColorTarget::from_path(matches[0].1)?;
        if targets.contains(&target) {
            return Err(format!(
                "HDR target {device_name:?} resolves to a duplicate display target"
            ));
        }
        targets.push(target);
    }
    Ok(targets)
}

fn query() -> Result<Vec<AdvancedColorState>, String> {
    let distinct_hdr = windows_11_or_later()?;
    let paths = active_paths()?;

    let mut states = Vec::with_capacity(paths.len());
    for path in &paths {
        let adapter_device_path = adapter_device_path(path)?;
        let monitor_device_path = target_monitor_device_path(path)?;
        let state = if distinct_hdr {
            query_distinct_hdr(
                path.targetInfo.adapterId,
                path.targetInfo.id,
                adapter_device_path,
                monitor_device_path,
            )?
        } else {
            query_legacy(
                path.targetInfo.adapterId,
                path.targetInfo.id,
                adapter_device_path,
                monitor_device_path,
            )?
        };
        states.push(state);
    }
    Ok(states)
}

fn set_hdr_enabled(state: &AdvancedColorState, enabled: bool) -> bool {
    let request = SetColorState {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: match state.api {
                ColorStateApi::Legacy => DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
                ColorStateApi::DistinctHdr => SET_HDR_STATE,
            },
            size: std::mem::size_of::<SetColorState>() as u32,
            adapterId: state.adapter,
            id: state.target,
        },
        enable: u32::from(enabled),
    };
    // SAFETY: both legacy Advanced Color and Windows 11 HDR set packets share
    // this header-plus-u32 ABI, and the header selects the matching packet.
    unsafe { DisplayConfigSetDeviceInfo(&request.header) == 0 }
}

fn is_genuinely_hdr(state: &AdvancedColorState) -> bool {
    state.hdr_supported && state.advanced_color_active && state.active_colour_mode == COLOR_MODE_HDR
}

fn is_sdr(state: &AdvancedColorState) -> bool {
    !state.advanced_color_active && state.active_colour_mode == COLOR_MODE_SDR
}

fn user_hdr_enabled(state: &AdvancedColorState) -> bool {
    state.hdr_user_enabled
}

fn is_requested_target(
    state: &AdvancedColorState,
    required_targets: &[AdvancedColorTarget],
) -> bool {
    required_targets.iter().any(|target| target.matches(state))
}

fn all_required_targets_in_mode(
    required_targets: &[AdvancedColorTarget],
    states: &[AdvancedColorState],
    desired_hdr: bool,
) -> bool {
    required_targets.iter().all(|target| {
        states
            .iter()
            .any(|state| target.matches(state) && target_satisfies_desired(state, desired_hdr))
    })
}

fn target_satisfies_desired(state: &AdvancedColorState, desired_hdr: bool) -> bool {
    user_hdr_enabled(state) == desired_hdr
        && if desired_hdr {
            is_genuinely_hdr(state)
        } else {
            is_sdr(state)
        }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HdrChangeDecision {
    Change { from_hdr: bool, to_hdr: bool },
    NoChange,
}

fn change_decision(state: &AdvancedColorState, desired_hdr: bool) -> HdrChangeDecision {
    if !state.hdr_supported {
        return HdrChangeDecision::NoChange;
    }
    let current_hdr = user_hdr_enabled(state);
    if current_hdr == desired_hdr {
        HdrChangeDecision::NoChange
    } else {
        HdrChangeDecision::Change {
            from_hdr: current_hdr,
            to_hdr: desired_hdr,
        }
    }
}

fn state_summary(states: &[AdvancedColorState]) -> String {
    states
        .iter()
        .map(|state| {
            format!(
                "target={} api={:?} advanced_supported={} advanced_active={} \
                 policy_limited={} hdr_supported={} hdr_user_enabled={} wcg_supported={} \
                 wcg_user_enabled={} mode={} bpc={}",
                state.target,
                state.api,
                state.advanced_color_supported,
                state.advanced_color_active,
                state.limited_by_policy,
                state.hdr_supported,
                state.hdr_user_enabled,
                state.wcg_supported,
                state.wcg_user_enabled,
                mode_name(state.active_colour_mode),
                state.bits_per_colour_channel
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone)]
pub(crate) struct AdvancedColorSessionGuard {
    entries: Vec<AdvancedColorRecoveryEntry>,
}

impl AdvancedColorSessionGuard {
    pub(crate) fn empty() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(crate) fn restore(&mut self) -> Result<(), String> {
        restore_entries(&self.entries, "session_end")?;
        self.entries.clear();
        clear_recovery_entries()
    }
}

impl Drop for AdvancedColorSessionGuard {
    fn drop(&mut self) {
        if self.entries.is_empty() {
            return;
        }
        if let Err(error) = self.restore() {
            tracing::error!(
                target: DISPLAY,
                %error,
                "Advanced Color session guard Drop cleanup failed; recovery journal retained"
            );
        }
    }
}

pub(crate) fn apply_for_session(
    required_targets: &[AdvancedColorTarget],
    desired_hdr: bool,
    session_log_id: &arcen_telemetry::CorrelationId,
) -> Result<AdvancedColorSessionGuard, String> {
    let original = query_targets(required_targets)?;
    let mut entries = recovery_entries_for_states(&original, desired_hdr);
    persist_recovery_entries(&entries, session_log_id)?;
    let applied = set_required_targets(
        required_targets,
        desired_hdr,
        desired_hdr,
        &mut entries,
        session_log_id,
    );
    if let Err(error) = applied {
        let restore = restore_entries(&entries, "session_end");
        return Err(match rollback_restored(&restore) {
            true => match clear_recovery_entries() {
                Ok(()) => error,
                Err(clear) => {
                    format!("{error}; Advanced Color journal clear failed after rollback: {clear}")
                }
            },
            false => {
                let restore = restore.expect_err("checked failed restore");
                format!("{error}; Advanced Color rollback failed: {restore}")
            }
        });
    }
    Ok(AdvancedColorSessionGuard { entries })
}

fn query_targets(
    required_targets: &[AdvancedColorTarget],
) -> Result<Vec<AdvancedColorState>, String> {
    let states = query()?
        .into_iter()
        .filter(|state| is_requested_target(state, required_targets))
        .collect::<Vec<_>>();
    Ok(states)
}

fn recovery_entries_for_states(
    states: &[AdvancedColorState],
    desired_hdr: bool,
) -> Vec<AdvancedColorRecoveryEntry> {
    states
        .iter()
        .filter_map(|state| match change_decision(state, desired_hdr) {
            HdrChangeDecision::Change { from_hdr, .. } => Some(state.recovery_target(from_hdr)),
            HdrChangeDecision::NoChange => None,
        })
        .collect()
}

fn ensure_recovery_entry(
    entries: &mut Vec<AdvancedColorRecoveryEntry>,
    state: &AdvancedColorState,
    original_hdr_enabled: bool,
    session_log_id: &arcen_telemetry::CorrelationId,
) -> Result<(), String> {
    let already_recorded = entries.iter().any(|entry| {
        entry
            .adapter_device_path
            .eq_ignore_ascii_case(&state.adapter_device_path)
            && entry
                .monitor_device_path
                .eq_ignore_ascii_case(&state.monitor_device_path)
    });
    if !already_recorded {
        entries.push(state.clone().recovery_target(original_hdr_enabled));
        persist_recovery_entries(entries, session_log_id)?;
    }
    Ok(())
}

fn set_required_targets(
    required_targets: &[AdvancedColorTarget],
    desired_hdr: bool,
    fail_on_verify: bool,
    entries: &mut Vec<AdvancedColorRecoveryEntry>,
    session_log_id: &arcen_telemetry::CorrelationId,
) -> Result<(), String> {
    if required_targets.is_empty() {
        return Err("Advanced Color session requires at least one display target".to_string());
    }
    let deadline = std::time::Instant::now() + ENGAGE_TIMEOUT;
    let mut last_summary = String::new();
    let mut changed_targets = 0_usize;
    loop {
        let states = match query_targets(required_targets) {
            Ok(states) => states,
            Err(error) if !desired_hdr => {
                tracing::warn!(
                    target: DISPLAY,
                    %error,
                    "continuing SDR session after failing to query Windows HDR state"
                );
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let missing_targets = required_targets
            .iter()
            .filter(|target| !states.iter().any(|state| target.matches(state)))
            .count();
        if missing_targets != 0 && desired_hdr {
            return Err(format!(
                "Advanced Color setup could not find {missing_targets} required display target(s)"
            ));
        }
        if missing_targets != 0 {
            tracing::warn!(
                target: DISPLAY,
                missing_targets,
                "continuing SDR session after failing to resolve every Windows HDR target"
            );
            return Ok(());
        }
        let summary = state_summary(&states);
        if summary != last_summary {
            tracing::debug!(
                target: DISPLAY,
                required_targets = required_targets.len(),
                desired = if desired_hdr { "hdr" } else { "sdr" },
                state = summary,
                "advanced colour state verification"
            );
            last_summary = summary;
        }
        if all_required_targets_in_mode(required_targets, &states, desired_hdr) {
            if !desired_hdr && changed_targets != 0 {
                tracing::info!(
                    target: DISPLAY,
                    changed_targets,
                    "Windows HDR disabled on required display(s) before SDR capture"
                );
            }
            return Ok(());
        }
        for state in &states {
            let HdrChangeDecision::Change { from_hdr, to_hdr } =
                change_decision(state, desired_hdr)
            else {
                continue;
            };
            ensure_recovery_entry(entries, state, from_hdr, session_log_id)?;
            let accepted = set_hdr_enabled(state, to_hdr);
            changed_targets += usize::from(accepted);
            tracing::info!(
                target: DISPLAY,
                target_id = state.target,
                from = if from_hdr { "hdr" } else { "sdr" },
                to = if to_hdr { "hdr" } else { "sdr" },
                reason = "session_start",
                accepted,
                "Windows display HDR state changed"
            );
        }
        if std::time::Instant::now() >= deadline {
            let message = format!(
                "Advanced Color setup did not reach {} mode on all {} requested target(s) within {}ms ({last_summary})",
                if desired_hdr { "HDR" } else { "SDR" },
                required_targets.len(),
                ENGAGE_TIMEOUT.as_millis()
            );
            if fail_on_verify {
                return Err(message);
            }
            tracing::warn!(
                target: DISPLAY,
                %message,
                "continuing session after failing to disable Windows HDR"
            );
            return Ok(());
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn persist_recovery_entries(
    entries: &[AdvancedColorRecoveryEntry],
    session_log_id: &arcen_telemetry::CorrelationId,
) -> Result<(), String> {
    if entries.is_empty() {
        return Ok(());
    }
    let path = crate::recovery::default_path();
    let created = !path.exists();
    let journal = if path.exists() {
        crate::recovery::read(&path)?.with_advanced_color(entries.to_vec())
    } else {
        crate::recovery::DisplayRecoveryJournal::advanced_color_only(entries.to_vec())
    };
    crate::recovery::write_atomic(&path, &journal)?;
    if created {
        if let Err(error) = crate::display::spawn_display_recovery_watchdog(&path, session_log_id) {
            let remove = crate::recovery::remove(&path);
            return Err(match remove {
                Ok(()) => error,
                Err(remove_error) => format!(
                    "{error}; failed to remove unarmed Advanced Color journal: {remove_error}"
                ),
            });
        }
    }
    Ok(())
}

pub(crate) fn restore_entries(
    entries: &[AdvancedColorRecoveryEntry],
    reason: &'static str,
) -> Result<(), String> {
    let mut errors = Vec::new();
    for entry in entries.iter().filter(|entry| entry.arcen_changed) {
        let state = match query() {
            Ok(states) => resolve_recovery_state(entry, states),
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let state = match state {
            Ok(state) => state,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let before = user_hdr_enabled(&state);
        if before == entry.original_hdr_enabled {
            continue;
        }
        let accepted = set_hdr_enabled(&state, entry.original_hdr_enabled);
        tracing::info!(
            target: DISPLAY,
            target_id = entry.target_id,
            from = if before { "hdr" } else { "sdr" },
            to = if entry.original_hdr_enabled { "hdr" } else { "sdr" },
            reason,
            accepted,
            "Windows display HDR state changed"
        );
        if !accepted {
            errors.push(format!(
                "restore Advanced Color target {} to {} was rejected",
                entry.target_id,
                if entry.original_hdr_enabled {
                    "HDR"
                } else {
                    "SDR"
                }
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

fn resolve_recovery_state(
    entry: &AdvancedColorRecoveryEntry,
    states: Vec<AdvancedColorState>,
) -> Result<AdvancedColorState, String> {
    if entry.adapter_device_path.is_empty() || entry.monitor_device_path.is_empty() {
        return Err(format!(
            "Advanced Color recovery target {} lacks boot-stable identity",
            entry.target_id
        ));
    }

    let matches = states
        .into_iter()
        .filter(|state| {
            entry
                .adapter_device_path
                .eq_ignore_ascii_case(&state.adapter_device_path)
                && entry
                    .monitor_device_path
                    .eq_ignore_ascii_case(&state.monitor_device_path)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [state] => Ok(state.clone()),
        [] => Err(format!(
            "Advanced Color recovery target {} with monitor path {:?} is no longer present",
            entry.target_id, entry.monitor_device_path
        )),
        _ => Err(format!(
            "Advanced Color recovery target {} with monitor path {:?} is ambiguous",
            entry.target_id, entry.monitor_device_path
        )),
    }
}

fn rollback_restored(restore: &Result<(), String>) -> bool {
    restore.is_ok()
}

fn clear_recovery_entries() -> Result<(), String> {
    let path = crate::recovery::default_path();
    if !path.exists() {
        return Ok(());
    }
    crate::recovery::clear_advanced_color_entries(&path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(
        api: ColorStateApi,
        hdr_supported: bool,
        advanced_active: bool,
        mode: i32,
        bits: u32,
    ) -> AdvancedColorState {
        state_on_target(1, api, hdr_supported, advanced_active, mode, bits)
    }

    fn state_on_target(
        target: u32,
        api: ColorStateApi,
        hdr_supported: bool,
        advanced_active: bool,
        mode: i32,
        bits: u32,
    ) -> AdvancedColorState {
        AdvancedColorState {
            adapter: LUID::default(),
            target,
            adapter_device_path: "adapter:test".to_string(),
            monitor_device_path: format!("monitor:{target}"),
            api,
            advanced_color_supported: hdr_supported,
            advanced_color_active: advanced_active,
            limited_by_policy: false,
            hdr_supported,
            hdr_user_enabled: advanced_active,
            wcg_supported: false,
            wcg_user_enabled: false,
            active_colour_mode: mode,
            bits_per_colour_channel: bits,
        }
    }

    #[test]
    fn windows_11_hdr_requires_hdr_mode_not_merely_advanced_colour() {
        assert!(is_genuinely_hdr(&state(
            ColorStateApi::DistinctHdr,
            true,
            true,
            COLOR_MODE_HDR,
            8
        )));
        assert!(!is_genuinely_hdr(&state(
            ColorStateApi::DistinctHdr,
            true,
            true,
            COLOR_MODE_WCG,
            10
        )));
        assert!(!is_genuinely_hdr(&state(
            ColorStateApi::DistinctHdr,
            true,
            false,
            COLOR_MODE_HDR,
            10
        )));
        assert!(!is_genuinely_hdr(&state(
            ColorStateApi::DistinctHdr,
            false,
            true,
            COLOR_MODE_HDR,
            10
        )));
    }

    #[test]
    fn legacy_windows_keeps_advanced_colour_as_the_hdr_signal() {
        assert!(is_genuinely_hdr(&state(
            ColorStateApi::Legacy,
            true,
            true,
            COLOR_MODE_HDR,
            8
        )));
    }

    #[test]
    fn unrelated_hdr_output_cannot_satisfy_or_receive_a_session_target_request() {
        let required = [AdvancedColorTarget {
            adapter_low: 0,
            adapter_high: 0,
            target: 1,
            adapter_device_path: "adapter:test".to_string(),
            monitor_device_path: "monitor:1".to_string(),
        }];
        let requested_sdr = state_on_target(
            1,
            ColorStateApi::DistinctHdr,
            true,
            false,
            COLOR_MODE_SDR,
            10,
        );
        let unrelated_hdr = state_on_target(
            2,
            ColorStateApi::DistinctHdr,
            true,
            true,
            COLOR_MODE_HDR,
            10,
        );
        let states = [requested_sdr.clone(), unrelated_hdr.clone()];

        assert!(!all_required_targets_in_mode(&required, &states, true));
        assert!(is_requested_target(&requested_sdr, &required));
        assert!(!is_requested_target(&unrelated_hdr, &required));
    }

    #[test]
    fn hdr_session_from_sdr_records_restore_to_sdr() {
        let sdr = state(ColorStateApi::DistinctHdr, true, false, COLOR_MODE_SDR, 10);
        assert_eq!(
            change_decision(&sdr, true),
            HdrChangeDecision::Change {
                from_hdr: false,
                to_hdr: true
            }
        );
        let entries = recovery_entries_for_states(&[sdr], true);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].original_hdr_enabled);
    }

    #[test]
    fn sdr_session_from_hdr_records_restore_to_hdr() {
        let hdr = state(ColorStateApi::DistinctHdr, true, true, COLOR_MODE_HDR, 10);
        assert_eq!(
            change_decision(&hdr, false),
            HdrChangeDecision::Change {
                from_hdr: true,
                to_hdr: false
            }
        );
        let entries = recovery_entries_for_states(&[hdr], false);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].original_hdr_enabled);
    }

    #[test]
    fn sdr_session_on_sdr_display_does_not_change_hdr_state() {
        let sdr = state(ColorStateApi::DistinctHdr, true, false, COLOR_MODE_SDR, 10);
        assert_eq!(change_decision(&sdr, false), HdrChangeDecision::NoChange);
        assert!(recovery_entries_for_states(&[sdr], false).is_empty());
    }

    #[test]
    fn hdr_session_on_hdr_display_does_not_record_restore() {
        let hdr = state(ColorStateApi::DistinctHdr, true, true, COLOR_MODE_HDR, 10);
        assert_eq!(change_decision(&hdr, true), HdrChangeDecision::NoChange);
        assert!(recovery_entries_for_states(&[hdr], true).is_empty());
    }

    #[test]
    fn recovery_uses_user_toggle_not_active_composition() {
        let mut inactive_but_enabled =
            state(ColorStateApi::DistinctHdr, true, false, COLOR_MODE_SDR, 10);
        inactive_but_enabled.hdr_user_enabled = true;
        assert_eq!(
            change_decision(&inactive_but_enabled, false),
            HdrChangeDecision::Change {
                from_hdr: true,
                to_hdr: false
            }
        );
        let entries = recovery_entries_for_states(&[inactive_but_enabled.clone()], false);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].original_hdr_enabled);
        assert!(
            !target_satisfies_desired(&inactive_but_enabled, false),
            "SDR capture readiness must not ignore an enabled user HDR toggle"
        );
    }

    #[test]
    fn failed_rollback_keeps_advanced_color_recovery_entries() {
        assert!(rollback_restored(&Ok(())));
        assert!(!rollback_restored(&Err("restore failed".to_string())));
    }

    #[test]
    fn recovery_identity_requires_a_unique_stable_path_match() {
        let first = state_on_target(
            1,
            ColorStateApi::DistinctHdr,
            true,
            true,
            COLOR_MODE_HDR,
            10,
        );
        let mut second = state_on_target(
            2,
            ColorStateApi::DistinctHdr,
            true,
            true,
            COLOR_MODE_HDR,
            10,
        );
        second.monitor_device_path = first.monitor_device_path.clone();
        let entry = first.recovery_target(true);

        assert!(resolve_recovery_state(&entry, vec![first.clone()]).is_ok());
        assert!(resolve_recovery_state(&entry, Vec::new())
            .unwrap_err()
            .contains("no longer present"));
        assert!(resolve_recovery_state(&entry, vec![first, second])
            .unwrap_err()
            .contains("ambiguous"));
    }

    #[test]
    fn windows_11_hdr_packets_match_the_sdk_abi() {
        assert_eq!(GET_ADVANCED_COLOR_INFO_2.0, 15);
        assert_eq!(SET_HDR_STATE.0, 16);
        assert_eq!(std::mem::size_of::<AdvancedColorInfo2>(), 36);
        assert_eq!(std::mem::size_of::<SetColorState>(), 24);
    }
}
