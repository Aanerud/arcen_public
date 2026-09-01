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
    QueryDisplayConfig, DISPLAYCONFIG_DEVICE_INFO_GET_ADVANCED_COLOR_INFO,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_HEADER,
    DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE, DISPLAYCONFIG_DEVICE_INFO_TYPE,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LUID};
use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

use crate::logging::DISPLAY;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdvancedColorState {
    adapter: LUID,
    target: u32,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AdvancedColorTarget {
    adapter_low: u32,
    adapter_high: i32,
    target: u32,
}

impl AdvancedColorTarget {
    fn from_path(path: &DISPLAYCONFIG_PATH_INFO) -> Self {
        Self {
            adapter_low: path.targetInfo.adapterId.LowPart,
            adapter_high: path.targetInfo.adapterId.HighPart,
            target: path.targetInfo.id,
        }
    }

    fn matches(self, state: &AdvancedColorState) -> bool {
        self.adapter_low == state.adapter.LowPart
            && self.adapter_high == state.adapter.HighPart
            && self.target == state.target
    }
}

impl AdvancedColorState {
    pub(crate) const fn bits_per_colour_channel(self) -> u32 {
        self.bits_per_colour_channel
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

fn query_distinct_hdr(adapter: LUID, target: u32) -> Result<AdvancedColorState, String> {
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

fn query_legacy(adapter: LUID, target: u32) -> Result<AdvancedColorState, String> {
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
        let target = AdvancedColorTarget::from_path(matches[0].1);
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
        let state = if distinct_hdr {
            query_distinct_hdr(path.targetInfo.adapterId, path.targetInfo.id)?
        } else {
            query_legacy(path.targetInfo.adapterId, path.targetInfo.id)?
        };
        states.push(state);
    }
    Ok(states)
}

fn set_hdr_enabled(state: AdvancedColorState) -> bool {
    let mut request = SetColorState {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: match state.api {
                ColorStateApi::Legacy => DISPLAYCONFIG_DEVICE_INFO_SET_ADVANCED_COLOR_STATE,
                ColorStateApi::DistinctHdr => SET_HDR_STATE,
            },
            size: std::mem::size_of::<SetColorState>() as u32,
            adapterId: state.adapter,
            id: state.target,
        },
        enable: 1,
    };
    // SAFETY: both legacy Advanced Color and Windows 11 HDR set packets share
    // this header-plus-u32 ABI, and the header selects the matching packet.
    unsafe { DisplayConfigSetDeviceInfo(&mut request.header) == 0 }
}

fn is_genuinely_hdr(state: &AdvancedColorState) -> bool {
    state.hdr_supported && state.advanced_color_active && state.active_colour_mode == COLOR_MODE_HDR
}

fn is_requested_target(
    state: &AdvancedColorState,
    required_targets: &[AdvancedColorTarget],
) -> bool {
    required_targets.iter().any(|target| target.matches(state))
}

fn all_required_targets_engaged(
    required_targets: &[AdvancedColorTarget],
    states: &[AdvancedColorState],
) -> bool {
    required_targets.iter().all(|target| {
        states
            .iter()
            .any(|state| target.matches(state) && is_genuinely_hdr(state))
    })
}

fn engage(required_targets: &[AdvancedColorTarget]) -> Result<Vec<AdvancedColorState>, String> {
    if required_targets.is_empty() {
        return Err("HDR session requires at least one display target".to_string());
    }
    let required_count = required_targets.len();
    let deadline = std::time::Instant::now() + ENGAGE_TIMEOUT;
    let mut last_summary = String::new();
    loop {
        let all_states = query()?;
        let states = all_states
            .into_iter()
            .filter(|state| is_requested_target(state, required_targets))
            .collect::<Vec<_>>();
        let missing_targets = required_targets
            .iter()
            .filter(|target| !states.iter().any(|state| target.matches(state)))
            .count();
        let summary = states
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
            .join(", ");
        if summary != last_summary {
            tracing::debug!(
                target: DISPLAY,
                required_targets = required_count,
                missing_targets,
                state = summary,
                "advanced colour state after final display provisioning"
            );
            last_summary = summary;
        }

        let all_engaged =
            missing_targets == 0 && all_required_targets_engaged(required_targets, &states);
        if all_engaged {
            tracing::info!(
                target: DISPLAY,
                required_targets = required_count,
                engaged_targets = states.len(),
                "HDR mode engaged on every required display"
            );
            return Ok(states);
        }

        for state in states
            .iter()
            .copied()
            .filter(|state| state.hdr_supported && !is_genuinely_hdr(state))
        {
            let accepted = set_hdr_enabled(state);
            tracing::debug!(
                target: DISPLAY,
                target_id = state.target,
                accepted,
                api = ?state.api,
                "HDR enable requested after final display provisioning"
            );
        }

        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "HDR display setup did not enter HDR mode on all {required_count} \
                 requested target(s) within {}ms (missing={missing_targets}; {last_summary})",
                ENGAGE_TIMEOUT.as_millis()
            ));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Enables and verifies HDR mode on every requested HDR display.
///
/// This is a hard pre-capture gate. Returning success-shaped state after a
/// failed `SET` would make the encoder signal PQ over an SDR desktop.
pub(crate) fn engage_required(
    required_targets: &[AdvancedColorTarget],
) -> Result<Vec<AdvancedColorState>, String> {
    let states = engage(required_targets)?;
    let output_link_bpc = states
        .iter()
        .copied()
        .map(AdvancedColorState::bits_per_colour_channel)
        .max()
        .unwrap_or(0);
    tracing::info!(
        target: DISPLAY,
        required_targets = required_targets.len(),
        output_link_bpc,
        "HDR desktop verified before capture: active colour mode is HDR and DWM is composing in FP16 scRGB; output-link bpc is downstream of capture"
    );
    Ok(states)
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
        let states = [requested_sdr, unrelated_hdr];

        assert!(!all_required_targets_engaged(&required, &states));
        assert!(is_requested_target(&requested_sdr, &required));
        assert!(!is_requested_target(&unrelated_hdr, &required));
    }

    #[test]
    fn windows_11_hdr_packets_match_the_sdk_abi() {
        assert_eq!(GET_ADVANCED_COLOR_INFO_2.0, 15);
        assert_eq!(SET_HDR_STATE.0, 16);
        assert_eq!(std::mem::size_of::<AdvancedColorInfo2>(), 36);
        assert_eq!(std::mem::size_of::<SetColorState>(), 24);
    }
}
