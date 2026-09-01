//! Narrow NVAPI display seam.
//!
//! ABI declarations and QueryInterface IDs are derived from NVIDIA's public,
//! MIT-licensed NVAPI SDK at commit
//! `cd6918f60b3c9a0476fdfe7e89bb32330602049d`:
//! <https://github.com/NVIDIA/nvapi/tree/cd6918f60b3c9a0476fdfe7e89bb32330602049d>.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
#[cfg(windows)]
use std::ffi::c_char;
use std::ffi::c_void;

const NVAPI_OK: i32 = 0;
const NVAPI_END_ENUMERATION: i32 = -7;
const NVAPI_INSUFFICIENT_BUFFER: i32 = -174;
const NVAPI_DATA_NOT_FOUND: i32 = -121;
const NVAPI_MAX_PHYSICAL_GPUS: usize = 64;
const NV_EDID_DATA_SIZE: usize = 256;
const NVAPI_SHORT_STRING_MAX: usize = 64;
const MAX_DISPLAY_CONFIG_PATHS: usize = 128;
const MAX_TARGETS_PER_PATH: usize = 64;
const MAX_GPU_DISPLAY_IDS: usize = 256;
const MAX_CUSTOM_DISPLAYS: usize = 64;
const NV_GPU_DISPLAY_ID_FLAG_CONNECTED: u32 = 1 << 6;
const NV_FORMAT_A8R8G8B8: i32 = 21;
const NV_TIMING_OVERRIDE_CVT_RB: i32 = 6;
const NV_DISPLAYCONFIG_FORCE_MODE_ENUMERATION: u32 = 0x0000_0008;
const NV_FORCE_COMMIT_VIDPN: u32 = 0x0000_0010;
#[cfg(all(test, windows))]
const NV_DISPLAYCONFIG_VALIDATE_ONLY: u32 = 0x0000_0001;
const NV_SCALING_PUBLIC_VALUES: &[u32] = &[0, 1, 2, 3, 5, 6, 7, 8, 255];

pub const ID_INITIALIZE: u32 = 0x0150_e828;
pub const ID_GET_ERROR_MESSAGE: u32 = 0x6c2d_048c;
pub const ID_ENUM_PHYSICAL_GPUS: u32 = 0xe5ac_921f;
pub const ID_GPU_GET_EDID: u32 = 0x37d3_2e69;
pub const ID_GPU_GET_ALL_DISPLAY_IDS: u32 = 0x7852_10a2;
pub const ID_GPU_SET_EDID: u32 = 0xe83d_6456;
pub const ID_GPU_GET_ADAPTER_ID: u32 = 0x0ff0_7fde;
pub const ID_DISP_GET_TIMING: u32 = 0x1751_67e9;
pub const ID_DISP_ENUM_CUSTOM_DISPLAY: u32 = 0xa207_2d59;
pub const ID_DISP_TRY_CUSTOM_DISPLAY: u32 = 0x1f7d_b630;
pub const ID_DISP_DELETE_CUSTOM_DISPLAY: u32 = 0x552e_5b9b;
pub const ID_DISP_SAVE_CUSTOM_DISPLAY: u32 = 0x4988_2876;
pub const ID_DISP_REVERT_CUSTOM_DISPLAY: u32 = 0xcbbd_40f0;
pub const ID_DISP_GET_DISPLAY_ID_BY_NAME: u32 = 0xae45_7190;
pub const ID_DISP_GET_DISPLAY_CONFIG: u32 = 0x11ab_ccf8;
pub const ID_DISP_SET_DISPLAY_CONFIG: u32 = 0x5d8c_f8de;
pub const ID_SYS_GET_GPU_AND_OUTPUT_ID: u32 = 0x112b_a1a5;

type NvPhysicalGpuHandle = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct AdapterLuid {
    pub low_part: u32,
    pub high_part: i32,
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
struct NvGpuDisplayIds {
    version: u32,
    connector_type: i32,
    display_id: u32,
    flags: u32,
}

impl NvGpuDisplayIds {
    fn initialized() -> Self {
        Self {
            version: nvapi_version::<Self>(3),
            ..Self::default()
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
struct NvViewportF {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct NvTimingExt {
    flag: u32,
    rr: u16,
    rrx1k: u32,
    aspect: u32,
    rep: u16,
    status: u32,
    #[serde(with = "serde_name40")]
    name: [u8; 40],
}

mod serde_name40 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S>(value: &[u8; 40], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bytes(value)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<[u8; 40], D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Vec::<u8>::deserialize(deserializer)?;
        value
            .try_into()
            .map_err(|value: Vec<u8>| serde::de::Error::invalid_length(value.len(), &"40 bytes"))
    }
}

impl Default for NvTimingExt {
    fn default() -> Self {
        Self {
            flag: 0,
            rr: 0,
            rrx1k: 0,
            aspect: 0,
            rep: 0,
            status: 0,
            name: [0; 40],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct NvTiming {
    h_visible: u16,
    h_border: u16,
    h_front_porch: u16,
    h_sync_width: u16,
    h_total: u16,
    h_sync_polarity: u8,
    v_visible: u16,
    v_border: u16,
    v_front_porch: u16,
    v_sync_width: u16,
    v_total: u16,
    v_sync_polarity: u8,
    interlaced: u16,
    pixel_clock_10_khz: u32,
    extra: NvTimingExt,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct NvTimingFlag {
    interlace_and_reserved: u32,
    format: u32,
    scaling: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct NvTimingInput {
    version: u32,
    width: u32,
    height: u32,
    refresh_hz: f32,
    flags: NvTimingFlag,
    timing_type: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Serialize)]
struct NvCustomDisplayRaw {
    version: u32,
    width: u32,
    height: u32,
    depth: u32,
    color_format: i32,
    source_partition: NvViewportF,
    x_ratio: f32,
    y_ratio: f32,
    timing: NvTiming,
    flags: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CustomDisplay {
    raw: NvCustomDisplayRaw,
}

impl CustomDisplay {
    #[cfg(test)]
    pub(crate) fn test_value(width: u32, height: u32, refresh_hz: u32) -> Self {
        Self {
            raw: NvCustomDisplayRaw {
                version: nvapi_version::<NvCustomDisplayRaw>(1),
                width,
                height,
                timing: NvTiming {
                    extra: NvTimingExt {
                        rr: refresh_hz as u16,
                        rrx1k: refresh_hz * 1_000,
                        ..NvTimingExt::default()
                    },
                    ..NvTiming::default()
                },
                ..NvCustomDisplayRaw::default()
            },
        }
    }

    fn matches_request(&self, width: u32, height: u32, refresh_hz: u32) -> bool {
        if self.raw.width != width || self.raw.height != height {
            return false;
        }
        let refresh_millihz = if self.raw.timing.extra.rrx1k != 0 {
            self.raw.timing.extra.rrx1k
        } else {
            u32::from(self.raw.timing.extra.rr) * 1_000
        };
        refresh_millihz == 0 || refresh_millihz.abs_diff(refresh_hz.saturating_mul(1_000)) <= 1_000
    }

    pub(crate) fn is_valid(&self) -> bool {
        self.raw.version == nvapi_version::<NvCustomDisplayRaw>(1)
            && self.raw.width > 0
            && self.raw.width <= 16_384
            && self.raw.height > 0
            && self.raw.height <= 8_640
            && self.raw.source_partition.x.is_finite()
            && self.raw.source_partition.y.is_finite()
            && self.raw.source_partition.width.is_finite()
            && self.raw.source_partition.height.is_finite()
            && self.raw.x_ratio.is_finite()
            && self.raw.y_ratio.is_finite()
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct NvResolution {
    width: u32,
    height: u32,
    color_depth: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct NvPosition {
    x: i32,
    y: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
struct NvSourceModeInfo {
    resolution: NvResolution,
    color_format: i32,
    position: NvPosition,
    spanning_orientation: i32,
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct NvTargetInfoRaw {
    display_id: u32,
    details: *mut c_void,
    target_id: u32,
}

impl Default for NvTargetInfoRaw {
    fn default() -> Self {
        Self {
            display_id: 0,
            details: std::ptr::null_mut(),
            target_id: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct NvPathInfoRaw {
    version: u32,
    source_id: u32,
    target_count: u32,
    targets: *mut NvTargetInfoRaw,
    source: *mut NvSourceModeInfo,
    flags: u32,
    os_adapter_id: *mut c_void,
}

impl Default for NvPathInfoRaw {
    fn default() -> Self {
        Self {
            version: nvapi_version::<Self>(2),
            source_id: 0,
            target_count: 0,
            targets: std::ptr::null_mut(),
            source: std::ptr::null_mut(),
            flags: 0,
            os_adapter_id: std::ptr::null_mut(),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NvAdvancedTargetInfoRaw {
    version: u32,
    rotation: u32,
    scaling: u32,
    refresh_rate_1k: u32,
    flags: u32,
    connector: u32,
    tv_format: u32,
    timing_override: u32,
    timing: NvTiming,
}

impl Default for NvAdvancedTargetInfoRaw {
    fn default() -> Self {
        Self {
            version: nvapi_version::<Self>(1),
            rotation: 0,
            scaling: 0,
            refresh_rate_1k: 0,
            flags: 0,
            connector: 0,
            tv_format: 0,
            timing_override: 0,
            timing: NvTiming::default(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct DisplayTargetAdvancedInfo {
    rotation: u32,
    scaling: u32,
    refresh_rate_1k: u32,
    flags: u32,
    connector: u32,
    tv_format: u32,
    timing_override: u32,
    timing: NvTiming,
}

impl From<NvAdvancedTargetInfoRaw> for DisplayTargetAdvancedInfo {
    fn from(value: NvAdvancedTargetInfoRaw) -> Self {
        Self {
            rotation: value.rotation,
            scaling: value.scaling,
            refresh_rate_1k: value.refresh_rate_1k,
            flags: value.flags,
            connector: value.connector,
            tv_format: value.tv_format,
            timing_override: value.timing_override,
            timing: value.timing,
        }
    }
}

impl From<DisplayTargetAdvancedInfo> for NvAdvancedTargetInfoRaw {
    fn from(value: DisplayTargetAdvancedInfo) -> Self {
        Self {
            version: nvapi_version::<Self>(1),
            rotation: value.rotation,
            scaling: value.scaling,
            refresh_rate_1k: value.refresh_rate_1k,
            flags: value.flags,
            connector: value.connector,
            tv_format: value.tv_format,
            timing_override: value.timing_override,
            timing: value.timing,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DisplayTargetInfo {
    pub display_id: u32,
    pub target_id: u32,
    #[serde(default)]
    pub advanced: Option<DisplayTargetAdvancedInfo>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct DisplayConfigPath {
    pub source_id: u32,
    source: NvSourceModeInfo,
    pub targets: Vec<DisplayTargetInfo>,
    pub non_nvidia_adapter: bool,
    #[serde(default)]
    pub reserved_path_flags: u32,
    #[serde(default)]
    pub os_adapter_luid: Option<AdapterLuid>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct DisplayConfig {
    pub paths: Vec<DisplayConfigPath>,
}

/// Read-only view of the desktop geometry a path drives.
///
/// The backing `NvSourceModeInfo` stays private so no caller outside this module
/// can hand a hand-built NVAPI source mode back to the driver.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub struct SourceGeometry {
    pub width: u32,
    pub height: u32,
    pub color_depth: u32,
    pub position_x: i32,
    pub position_y: i32,
    pub primary: bool,
}

impl DisplayConfigPath {
    pub fn source_geometry(&self) -> SourceGeometry {
        SourceGeometry {
            width: self.source.resolution.width,
            height: self.source.resolution.height,
            color_depth: self.source.resolution.color_depth,
            position_x: self.source.position.x,
            position_y: self.source.position.y,
            primary: self.source.flags & 1 != 0,
        }
    }
}

struct OwnedDisplayConfigRaw {
    paths: Vec<NvPathInfoRaw>,
    sources: Vec<Box<NvSourceModeInfo>>,
    targets: Vec<Vec<NvTargetInfoRaw>>,
    advanced: Vec<Vec<Option<Box<NvAdvancedTargetInfoRaw>>>>,
    os_adapters: Vec<Option<Box<AdapterLuid>>>,
}

impl OwnedDisplayConfigRaw {
    fn from_config(config: &DisplayConfig) -> Result<Self, String> {
        validate_display_config(config)?;
        let mut sources: Vec<Box<NvSourceModeInfo>> = config
            .paths
            .iter()
            .map(|path| Box::new(path.source))
            .collect();
        let mut advanced: Vec<Vec<Option<Box<NvAdvancedTargetInfoRaw>>>> = config
            .paths
            .iter()
            .map(|path| {
                path.targets
                    .iter()
                    .map(|target| {
                        target
                            .advanced
                            .map(NvAdvancedTargetInfoRaw::from)
                            .map(Box::new)
                    })
                    .collect()
            })
            .collect();
        let mut os_adapters: Vec<Option<Box<AdapterLuid>>> = config
            .paths
            .iter()
            .map(|path| path.os_adapter_luid.map(Box::new))
            .collect();
        let mut targets: Vec<Vec<NvTargetInfoRaw>> = config
            .paths
            .iter()
            .enumerate()
            .map(|(path_index, path)| {
                path.targets
                    .iter()
                    .enumerate()
                    .map(|(target_index, target)| NvTargetInfoRaw {
                        display_id: target.display_id,
                        details: advanced[path_index][target_index].as_mut().map_or(
                            std::ptr::null_mut(),
                            |details| {
                                details.as_mut() as *mut NvAdvancedTargetInfoRaw as *mut c_void
                            },
                        ),
                        target_id: target.target_id,
                    })
                    .collect()
            })
            .collect();
        let paths = config
            .paths
            .iter()
            .enumerate()
            .map(|(index, path)| NvPathInfoRaw {
                version: nvapi_version::<NvPathInfoRaw>(2),
                source_id: path.source_id,
                target_count: targets[index].len() as u32,
                targets: if targets[index].is_empty() {
                    std::ptr::null_mut()
                } else {
                    targets[index].as_mut_ptr()
                },
                source: sources[index].as_mut(),
                flags: path.reserved_path_flags | u32::from(path.non_nvidia_adapter),
                os_adapter_id: os_adapters[index]
                    .as_mut()
                    .map_or(std::ptr::null_mut(), |luid| {
                        luid.as_mut() as *mut AdapterLuid as *mut c_void
                    }),
            })
            .collect();
        Ok(Self {
            paths,
            sources,
            targets,
            advanced,
            os_adapters,
        })
    }

    fn config_for_nvapi_application(config: &DisplayConfig) -> Result<DisplayConfig, String> {
        validate_display_config(config)?;
        if config.paths.iter().any(|path| path.non_nvidia_adapter) {
            return Err(
                "NVAPI application config unexpectedly contains a non-NVIDIA path".to_string(),
            );
        }
        let mut prepared = config.clone();
        let anchor = prepared
            .paths
            .iter()
            .position(|path| path.source.flags & 1 != 0)
            .or_else(|| {
                prepared
                    .paths
                    .iter()
                    .enumerate()
                    .min_by_key(|(_, path)| (path.source.position.x, path.source.position.y))
                    .map(|(index, _)| index)
            })
            .ok_or_else(|| "NVAPI application config has no display paths".to_string())?;
        let origin = prepared.paths[anchor].source.position;
        for (index, path) in prepared.paths.iter_mut().enumerate() {
            path.source.position.x =
                path.source
                    .position
                    .x
                    .checked_sub(origin.x)
                    .ok_or_else(|| {
                        "NVAPI X position overflow while normalizing topology".to_string()
                    })?;
            path.source.position.y =
                path.source
                    .position
                    .y
                    .checked_sub(origin.y)
                    .ok_or_else(|| {
                        "NVAPI Y position overflow while normalizing topology".to_string()
                    })?;
            path.source.flags = (path.source.flags & !1) | u32::from(index == anchor);
            path.source_id = 0;
            for target in &mut path.targets {
                target.target_id = 0;
            }
        }
        validate_display_config(&prepared)?;
        Ok(prepared)
    }

    fn to_config(&self) -> Result<DisplayConfig, String> {
        if self.paths.len() != self.sources.len()
            || self.paths.len() != self.targets.len()
            || self.paths.len() != self.advanced.len()
            || self.paths.len() != self.os_adapters.len()
        {
            return Err(
                "NVAPI display config backing arrays have inconsistent lengths".to_string(),
            );
        }
        let paths = self
            .paths
            .iter()
            .enumerate()
            .map(|(path_index, path)| {
                if self.targets[path_index].len() != self.advanced[path_index].len()
                    || self.targets[path_index].len() != path.target_count as usize
                {
                    return Err(format!(
                        "NVAPI source {} target backing count changed during query",
                        path.source_id
                    ));
                }
                let non_nvidia_adapter = path.flags & 1 != 0;
                let targets = self.targets[path_index]
                    .iter()
                    .enumerate()
                    .map(|(target_index, target)| DisplayTargetInfo {
                        display_id: target.display_id,
                        target_id: target.target_id,
                        advanced: self.advanced[path_index][target_index]
                            .as_ref()
                            .map(|details| DisplayTargetAdvancedInfo::from(**details)),
                    })
                    .collect();
                Ok(DisplayConfigPath {
                    source_id: path.source_id,
                    source: *self.sources[path_index],
                    targets,
                    non_nvidia_adapter,
                    reserved_path_flags: path.flags & !1,
                    os_adapter_luid: self.os_adapters[path_index].as_ref().map(|luid| **luid),
                })
            })
            .collect::<Result<Vec<_>, String>>()?;
        let config = DisplayConfig { paths };
        validate_display_config(&config)?;
        Ok(config)
    }
}

pub(crate) fn validate_display_config(config: &DisplayConfig) -> Result<(), String> {
    if config.paths.is_empty() || config.paths.len() > MAX_DISPLAY_CONFIG_PATHS {
        return Err(format!(
            "NVAPI display config path count {} is outside 1..={MAX_DISPLAY_CONFIG_PATHS}",
            config.paths.len()
        ));
    }
    for path in &config.paths {
        if path.targets.len() > MAX_TARGETS_PER_PATH {
            return Err(format!(
                "NVAPI source {} has {} targets; safety limit is {MAX_TARGETS_PER_PATH}",
                path.source_id,
                path.targets.len()
            ));
        }
        if path.non_nvidia_adapter != path.os_adapter_luid.is_some() {
            return Err(format!(
                "NVAPI source {} non-NVIDIA flag disagrees with OS adapter LUID presence",
                path.source_id
            ));
        }
        if path.reserved_path_flags != 0 {
            return Err(format!(
                "NVAPI source {} carries nonzero reserved path flags {:#010x}",
                path.source_id, path.reserved_path_flags
            ));
        }
        for target in &path.targets {
            match (path.non_nvidia_adapter, target.advanced.as_ref()) {
                (true, Some(_)) => {
                    return Err(format!(
                        "NVAPI non-NVIDIA source {} target carries NVIDIA advanced details",
                        path.source_id
                    ));
                }
                (_, Some(advanced)) => {
                    if advanced.rotation > 4 {
                        return Err(format!(
                            "NVAPI source {} has invalid rotation {}",
                            path.source_id, advanced.rotation
                        ));
                    }
                    if !NV_SCALING_PUBLIC_VALUES.contains(&advanced.scaling) {
                        return Err(format!(
                            "NVAPI source {} has invalid scaling {}",
                            path.source_id, advanced.scaling
                        ));
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}

impl DisplayConfig {
    pub fn set_resolution(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        let matching: Vec<&mut DisplayConfigPath> = self
            .paths
            .iter_mut()
            .filter(|path| {
                path.targets
                    .iter()
                    .any(|target| target.display_id == display_id)
            })
            .collect();
        if matching.len() != 1 {
            return Err(format!(
                "NVAPI topology contains {} paths for display id 0x{display_id:08x}",
                matching.len()
            ));
        }
        let matching = matching.into_iter().next().unwrap();
        matching.source.resolution = NvResolution {
            width,
            height,
            color_depth: 32,
        };
        for target in &mut matching.targets {
            if target.display_id == display_id {
                target.advanced = None;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayMapping {
    pub display_id: u32,
    pub output_id: u32,
    pub head: u32,
    pub adapter_luid: AdapterLuid,
    // NVAPI handles are process-local tokens, not pointers to Rust-owned memory.
    // Store the token as an integer so async session ownership can move safely;
    // convert it back only at the synchronous FFI call boundary.
    gpu: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct BootPathBinding {
    pub source_id: u32,
    pub target_id: u32,
}

#[derive(Clone, Debug)]
pub struct ExactModeSnapshot {
    pub mapping: DisplayMapping,
    pub original_edid: Option<Vec<u8>>,
    pub original_config: DisplayConfig,
    application_config: DisplayConfig,
    pub custom_snapshot_complete: bool,
    pub pre_existing_custom: Vec<CustomDisplay>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TimingOwnership {
    #[default]
    NotTried,
    TrialAttemptedByUs,
    TrialAppliedByUs,
    SaveAttemptedByUs,
    SavedByUs,
    CleanupComplete,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EdidWriteStage {
    #[default]
    None,
    Attempted,
    Verified,
}

impl TimingOwnership {
    fn trial_was_attempted(self) -> bool {
        matches!(
            self,
            Self::TrialAttemptedByUs
                | Self::TrialAppliedByUs
                | Self::SaveAttemptedByUs
                | Self::SavedByUs
        )
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CleanupStage {
    #[default]
    Pending,
    TopologyRestored,
    TrialReverted,
    SavedTimingDeleted,
    EdidRestored,
    Complete,
}

impl CleanupStage {
    pub(crate) const fn next(self) -> Option<Self> {
        match self {
            Self::Pending => Some(Self::TopologyRestored),
            Self::TopologyRestored => Some(Self::TrialReverted),
            Self::TrialReverted => Some(Self::SavedTimingDeleted),
            Self::SavedTimingDeleted => Some(Self::EdidRestored),
            Self::EdidRestored => Some(Self::Complete),
            Self::Complete => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ActiveExactMode {
    pub custom: Option<CustomDisplay>,
    pub ownership: TimingOwnership,
    pub save_error: Option<String>,
    pub custom_snapshot_complete: bool,
    pub pre_existing_custom: Vec<CustomDisplay>,
    pub edid_write_stage: EdidWriteStage,
    pub intended_edid_sha256: Option<String>,
}

#[derive(Clone, Debug)]
pub struct ApplyExactError {
    pub message: String,
    pub active: Option<ActiveExactMode>,
    pub topology_commit_failed: bool,
}

impl std::fmt::Display for ApplyExactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecoveryData {
    pub device_name: String,
    pub adapter_luid: AdapterLuid,
    pub original_edid: Option<Vec<u8>>,
    pub original_config: DisplayConfig,
    #[serde(default)]
    pub display_id: Option<u32>,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    #[serde(default)]
    pub ownership: TimingOwnership,
    #[serde(default)]
    pub custom: Option<CustomDisplay>,
    #[serde(default)]
    pub custom_snapshot_complete: bool,
    #[serde(default)]
    pub pre_existing_custom: Vec<CustomDisplay>,
    #[serde(default)]
    pub cleanup_stage: CleanupStage,
    #[serde(default)]
    pub edid_write_stage: EdidWriteStage,
    #[serde(default)]
    pub intended_edid_sha256: Option<String>,
}

pub trait NvapiDriver {
    fn map_display(
        &mut self,
        device_name: &str,
        adapter_luid: AdapterLuid,
    ) -> Result<DisplayMapping, String>;
    fn map_recovery_display(
        &mut self,
        device_name: &str,
        adapter_luid: AdapterLuid,
        _original_config: &DisplayConfig,
        _display_id: Option<u32>,
    ) -> Result<DisplayMapping, String> {
        self.map_display(device_name, adapter_luid)
    }
    fn get_edid(&mut self, mapping: DisplayMapping) -> Result<Option<Vec<u8>>, String>;
    fn set_edid(&mut self, mapping: DisplayMapping, edid: &[u8]) -> Result<(), String>;
    fn get_display_config(&mut self) -> Result<DisplayConfig, String>;
    fn set_display_config(&mut self, config: &DisplayConfig) -> Result<(), String>;
    fn enum_custom_displays(&mut self, display_id: u32) -> Result<Vec<CustomDisplay>, String>;
    fn calculate_custom_display(
        &mut self,
        display_id: u32,
        width: u32,
        height: u32,
        refresh_hz: u32,
    ) -> Result<CustomDisplay, String>;
    fn try_custom_display(&mut self, display_id: u32, custom: &CustomDisplay)
        -> Result<(), String>;
    fn save_custom_display(&mut self, display_id: u32) -> Result<(), String>;
    fn revert_custom_display(&mut self, display_id: u32) -> Result<(), String>;
    fn delete_custom_display(
        &mut self,
        display_id: u32,
        custom: &CustomDisplay,
    ) -> Result<(), String>;
}

pub fn snapshot<D: NvapiDriver>(
    driver: &mut D,
    device_name: &str,
    adapter_luid: AdapterLuid,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<ExactModeSnapshot, String> {
    let mapping = driver.map_display(device_name, adapter_luid)?;
    let original_edid = driver.get_edid(mapping)?;
    let original_config = driver.get_display_config()?;
    let (pre_existing_custom, custom_snapshot_complete) =
        match driver.enum_custom_displays(mapping.display_id) {
            Ok(custom) => (
                custom
                    .into_iter()
                    .filter(|custom| custom.matches_request(width, height, refresh_hz))
                    .collect(),
                true,
            ),
            Err(error) => {
                tracing::warn!(
                    display_id = format_args!("{:#x}", mapping.display_id),
                    %error,
                    "custom timing enumeration unavailable; persistent save is disabled"
                );
                (Vec::new(), false)
            }
        };
    Ok(ExactModeSnapshot {
        mapping,
        original_edid,
        application_config: original_config.clone(),
        original_config,
        custom_snapshot_complete,
        pre_existing_custom,
    })
}

pub fn retarget_snapshot<D: NvapiDriver>(
    driver: &mut D,
    original: &ExactModeSnapshot,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Result<ExactModeSnapshot, String> {
    let (pre_existing_custom, custom_snapshot_complete) =
        match driver.enum_custom_displays(original.mapping.display_id) {
            Ok(custom) => (
                custom
                    .into_iter()
                    .filter(|custom| custom.matches_request(width, height, refresh_hz))
                    .collect(),
                true,
            ),
            Err(error) => {
                tracing::warn!(
                    display_id = format_args!("{:#x}", original.mapping.display_id),
                    %error,
                    "retarget custom timing enumeration unavailable; persistent save disabled"
                );
                (Vec::new(), false)
            }
        };
    let mut retarget = original.clone();
    retarget.application_config = driver.get_display_config()?;
    retarget.pre_existing_custom = pre_existing_custom;
    retarget.custom_snapshot_complete = custom_snapshot_complete;
    Ok(retarget)
}

pub fn apply_exact<D, F>(
    driver: &mut D,
    snapshot: &ExactModeSnapshot,
    edid: &[u8],
    width: u32,
    height: u32,
    refresh_hz: u32,
    mut checkpoint: F,
) -> Result<ActiveExactMode, ApplyExactError>
where
    D: NvapiDriver,
    F: FnMut(&ActiveExactMode) -> Result<(), String>,
{
    use sha2::{Digest, Sha256};
    let intended_edid_sha256 = format!("{:x}", Sha256::digest(edid));
    let mut active = ActiveExactMode {
        custom: None,
        ownership: TimingOwnership::NotTried,
        save_error: None,
        custom_snapshot_complete: snapshot.custom_snapshot_complete,
        pre_existing_custom: snapshot.pre_existing_custom.clone(),
        edid_write_stage: EdidWriteStage::None,
        intended_edid_sha256: Some(intended_edid_sha256),
    };
    // Do not rewrite an EDID that is already exactly right.
    //
    // Measured, repeatedly: writing the EDID during a session drops the
    // display's Advanced Color capability for the rest of that session --
    // `supported=true enabled=true bpc=10` immediately before the write,
    // `supported=false ... =8` immediately after, and it does not come back
    // within the session even when polled. Re-enabling is impossible from
    // there, because enabling HDR requires the very capability the write
    // destroyed.
    //
    // When a display has already been provisioned with the HDR10 EDID for
    // this exact geometry (`nvapi-provision-arcen-edid --hdr10`), the bytes
    // this session is about to write are identical to the bytes already on
    // the display, so the write buys nothing and costs HDR. Skipping it
    // keeps the desktop in HDR for the whole session.
    let already_correct = snapshot.original_edid.as_deref() == Some(edid);
    if already_correct {
        return apply_exact_topology(driver, snapshot, width, height, active);
    }
    if edid.len() > 128 {
        return Err(ApplyExactError {
            message: format!(
                "HDR display EDID is not provisioned before session start on display id \
                 0x{:08x}; refusing to rewrite it after connection",
                snapshot.mapping.display_id
            ),
            active: None,
            topology_commit_failed: false,
        });
    }
    active.edid_write_stage = EdidWriteStage::Attempted;
    checkpoint(&active).map_err(|message| ApplyExactError {
        message,
        active: Some(active.clone()),
        topology_commit_failed: false,
    })?;
    driver
        .set_edid(snapshot.mapping, edid)
        .map_err(|message| ApplyExactError {
            message,
            active: Some(active.clone()),
            topology_commit_failed: false,
        })?;
    verify_effective_edid(driver, snapshot.mapping, Some(edid)).map_err(|message| {
        ApplyExactError {
            message,
            active: Some(active.clone()),
            topology_commit_failed: false,
        }
    })?;
    active.edid_write_stage = EdidWriteStage::Verified;
    checkpoint(&active).map_err(|message| ApplyExactError {
        message,
        active: Some(active.clone()),
        topology_commit_failed: false,
    })?;
    let custom = driver
        .calculate_custom_display(snapshot.mapping.display_id, width, height, refresh_hz)
        .map_err(|message| ApplyExactError {
            message,
            active: Some(active.clone()),
            topology_commit_failed: false,
        })?;
    active.custom = Some(custom);
    active.ownership = TimingOwnership::TrialAttemptedByUs;
    checkpoint(&active).map_err(|message| ApplyExactError {
        message,
        active: Some(active.clone()),
        topology_commit_failed: false,
    })?;
    let custom = active
        .custom
        .as_ref()
        .expect("custom timing was initialized");
    driver
        .try_custom_display(snapshot.mapping.display_id, custom)
        .map_err(|message| ApplyExactError {
            message,
            active: Some(active.clone()),
            topology_commit_failed: false,
        })?;
    active.ownership = TimingOwnership::TrialAppliedByUs;
    checkpoint(&active).map_err(|message| ApplyExactError {
        message,
        active: Some(active.clone()),
        topology_commit_failed: false,
    })?;
    if !active.custom_snapshot_complete {
        active.save_error = Some(
            "pre-existing custom timings could not be enumerated; persistent save disabled"
                .to_string(),
        );
    }
    if active.pre_existing_custom.contains(
        active
            .custom
            .as_ref()
            .expect("custom timing was initialized"),
    ) {
        return apply_exact_topology(driver, snapshot, width, height, active);
    }
    if !active.custom_snapshot_complete {
        checkpoint(&active).map_err(|message| ApplyExactError {
            message,
            active: Some(active.clone()),
            topology_commit_failed: false,
        })?;
        return apply_exact_topology(driver, snapshot, width, height, active);
    }
    active.ownership = TimingOwnership::SaveAttemptedByUs;
    checkpoint(&active).map_err(|message| ApplyExactError {
        message,
        active: Some(active.clone()),
        topology_commit_failed: false,
    })?;
    match driver.save_custom_display(snapshot.mapping.display_id) {
        Ok(()) => {
            let after_save = driver
                .enum_custom_displays(snapshot.mapping.display_id)
                .map_err(|message| ApplyExactError {
                    message,
                    active: Some(active.clone()),
                    topology_commit_failed: false,
                })?;
            if let Some(created) = newly_created_matching_custom(
                &active.pre_existing_custom,
                &after_save,
                width,
                height,
                refresh_hz,
            ) {
                active.custom = Some(created);
                active.ownership = TimingOwnership::SavedByUs;
            } else {
                active.save_error =
                    Some("saved custom timing could not be proven newly created".to_string());
            }
        }
        Err(error) => {
            active.ownership = TimingOwnership::TrialAppliedByUs;
            active.save_error = Some(error);
        }
    }
    checkpoint(&active).map_err(|message| ApplyExactError {
        message,
        active: Some(active.clone()),
        topology_commit_failed: false,
    })?;
    apply_exact_topology(driver, snapshot, width, height, active)
}

fn apply_exact_topology<D: NvapiDriver>(
    driver: &mut D,
    snapshot: &ExactModeSnapshot,
    width: u32,
    height: u32,
    active: ActiveExactMode,
) -> Result<ActiveExactMode, ApplyExactError> {
    let mut config = snapshot.application_config.clone();
    config
        .set_resolution(snapshot.mapping.display_id, width, height)
        .map_err(|message| ApplyExactError {
            message,
            active: Some(active.clone()),
            topology_commit_failed: false,
        })?;
    driver
        .set_display_config(&config)
        .map_err(|message| ApplyExactError {
            message,
            active: Some(active.clone()),
            topology_commit_failed: true,
        })?;
    Ok(active)
}

fn newly_created_matching_custom(
    before: &[CustomDisplay],
    after: &[CustomDisplay],
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> Option<CustomDisplay> {
    after
        .iter()
        .filter(|candidate| candidate.matches_request(width, height, refresh_hz))
        .find(|candidate| {
            let before_count = before.iter().filter(|item| *item == *candidate).count();
            let after_count = after.iter().filter(|item| *item == *candidate).count();
            after_count > before_count
        })
        .cloned()
}

#[allow(dead_code)]
pub fn restore_exact<D: NvapiDriver>(
    driver: &mut D,
    snapshot: &ExactModeSnapshot,
    active: Option<&ActiveExactMode>,
) -> Result<(), String> {
    restore_exact_staged(driver, snapshot, active, CleanupStage::Pending, |_| Ok(()))
}

pub(crate) fn restore_exact_staged<D, F>(
    driver: &mut D,
    snapshot: &ExactModeSnapshot,
    active: Option<&ActiveExactMode>,
    stage: CleanupStage,
    checkpoint: F,
) -> Result<(), String>
where
    D: NvapiDriver,
    F: FnMut(CleanupStage) -> Result<(), String>,
{
    restore_exact_staged_with_topology_fallback(
        driver,
        snapshot,
        active,
        stage,
        checkpoint,
        |error| Err(format!("restore NVAPI topology: {error}")),
    )
}

fn restore_exact_staged_with_topology_fallback<D, F, G>(
    driver: &mut D,
    snapshot: &ExactModeSnapshot,
    active: Option<&ActiveExactMode>,
    mut stage: CleanupStage,
    mut checkpoint: F,
    mut topology_fallback: G,
) -> Result<(), String>
where
    D: NvapiDriver,
    F: FnMut(CleanupStage) -> Result<(), String>,
    G: FnMut(String) -> Result<(), String>,
{
    let edid_write_was_attempted = active.is_some_and(|active| {
        matches!(
            active.edid_write_stage,
            EdidWriteStage::Attempted | EdidWriteStage::Verified
        )
    });
    if stage < CleanupStage::TopologyRestored {
        // The injected EDID may be the only reason the temporary mode is
        // valid. Restore the monitor's effective EDID before asking NVAPI to
        // reinstate the original topology. This preparation is deliberately
        // idempotent and uncheckpointed: a crash repeats it from Pending.
        if edid_write_was_attempted {
            restore_effective_edid(driver, snapshot)?;
        }
        if let Err(error) = driver.set_display_config(&snapshot.original_config) {
            topology_fallback(error)?;
        }
        checkpoint_cleanup_stage(&mut checkpoint, &mut stage, CleanupStage::TopologyRestored)?;
    }

    if stage < CleanupStage::TrialReverted {
        if let Some(active) = active {
            if active.ownership.trial_was_attempted() {
                driver
                    .revert_custom_display(snapshot.mapping.display_id)
                    .map_err(|error| format!("revert custom timing trial: {error}"))?;
            }
        }
        checkpoint_cleanup_stage(&mut checkpoint, &mut stage, CleanupStage::TrialReverted)?;
    }

    if stage < CleanupStage::SavedTimingDeleted {
        if let Some(active) = active {
            if matches!(
                active.ownership,
                TimingOwnership::SaveAttemptedByUs | TimingOwnership::SavedByUs
            ) {
                if let Some(custom) = saved_custom_owned_by_us(driver, snapshot, active)? {
                    driver
                        .delete_custom_display(snapshot.mapping.display_id, &custom)
                        .map_err(|error| format!("delete saved custom timing: {error}"))?;
                }
            }
        }
        checkpoint_cleanup_stage(
            &mut checkpoint,
            &mut stage,
            CleanupStage::SavedTimingDeleted,
        )?;
    }

    if stage < CleanupStage::EdidRestored {
        if edid_write_was_attempted {
            restore_effective_edid(driver, snapshot)?;
        }
        checkpoint_cleanup_stage(&mut checkpoint, &mut stage, CleanupStage::EdidRestored)?;
    }

    if stage < CleanupStage::Complete {
        checkpoint_cleanup_stage(&mut checkpoint, &mut stage, CleanupStage::Complete)?;
    }
    Ok(())
}

fn restore_effective_edid<D: NvapiDriver>(
    driver: &mut D,
    snapshot: &ExactModeSnapshot,
) -> Result<(), String> {
    driver
        .set_edid(snapshot.mapping, &[])
        .map_err(|error| format!("purge injected EDID: {error}"))?;
    if let Some(original) = snapshot.original_edid.as_deref() {
        let current = driver
            .get_edid(snapshot.mapping)
            .map_err(|error| format!("read EDID after purge: {error}"))?;
        if current.as_deref() != Some(original) {
            driver
                .set_edid(snapshot.mapping, original)
                .map_err(|error| format!("restore previous effective EDID: {error}"))?;
        }
    }
    verify_effective_edid(driver, snapshot.mapping, snapshot.original_edid.as_deref())
        .map_err(|error| format!("verify complete EDID after restore: {error}"))
}

fn verify_effective_edid<D: NvapiDriver>(
    driver: &mut D,
    mapping: DisplayMapping,
    expected: Option<&[u8]>,
) -> Result<(), String> {
    const ATTEMPTS: usize = 20;
    const DELAY: std::time::Duration = std::time::Duration::from_millis(50);
    let mut last = None;
    for attempt in 0..ATTEMPTS {
        match driver.get_edid(mapping) {
            Ok(current) if current.as_deref() == expected => return Ok(()),
            Ok(current) => {
                last = Some(format!(
                    "effective EDID length {:?}, expected {:?}",
                    current.as_ref().map(Vec::len),
                    expected.map(<[u8]>::len)
                ))
            }
            Err(error) => last = Some(error),
        }
        if attempt + 1 < ATTEMPTS {
            std::thread::sleep(DELAY);
        }
    }
    Err(last.unwrap_or_else(|| "EDID readback did not settle".to_string()))
}

fn checkpoint_cleanup_stage<F>(
    checkpoint: &mut F,
    stage: &mut CleanupStage,
    next: CleanupStage,
) -> Result<(), String>
where
    F: FnMut(CleanupStage) -> Result<(), String>,
{
    checkpoint(next)
        .map_err(|error| format!("checkpoint NVAPI cleanup stage {next:?}: {error}"))?;
    *stage = next;
    Ok(())
}

fn saved_custom_owned_by_us<D: NvapiDriver>(
    driver: &mut D,
    snapshot: &ExactModeSnapshot,
    active: &ActiveExactMode,
) -> Result<Option<CustomDisplay>, String> {
    let custom = active
        .custom
        .as_ref()
        .ok_or_else(|| "saved timing ownership has no exact custom timing".to_string())?;
    if active.pre_existing_custom.contains(custom) || !active.custom_snapshot_complete {
        return Ok(None);
    }
    let current = driver.enum_custom_displays(snapshot.mapping.display_id)?;
    if active.ownership == TimingOwnership::SavedByUs {
        return Ok(current.contains(custom).then(|| custom.clone()));
    }
    Ok(newly_created_matching_custom(
        &active.pre_existing_custom,
        &current,
        custom.raw.width,
        custom.raw.height,
        custom_refresh_hz(custom),
    ))
}

fn custom_refresh_hz(custom: &CustomDisplay) -> u32 {
    if custom.raw.timing.extra.rrx1k != 0 {
        custom.raw.timing.extra.rrx1k.saturating_add(500) / 1_000
    } else {
        u32::from(custom.raw.timing.extra.rr)
    }
}

pub fn recovery_data(
    device_name: String,
    snapshot: &ExactModeSnapshot,
    width: u32,
    height: u32,
    refresh_hz: u32,
) -> RecoveryData {
    RecoveryData {
        device_name,
        adapter_luid: snapshot.mapping.adapter_luid,
        original_edid: snapshot.original_edid.clone(),
        original_config: snapshot.original_config.clone(),
        display_id: Some(snapshot.mapping.display_id),
        width,
        height,
        refresh_hz,
        ownership: TimingOwnership::NotTried,
        custom: None,
        custom_snapshot_complete: snapshot.custom_snapshot_complete,
        pre_existing_custom: snapshot.pre_existing_custom.clone(),
        cleanup_stage: CleanupStage::Pending,
        edid_write_stage: EdidWriteStage::None,
        intended_edid_sha256: None,
    }
}

#[cfg(test)]
pub(crate) fn test_recovery_data(device_name: String) -> RecoveryData {
    RecoveryData {
        device_name,
        adapter_luid: AdapterLuid::default(),
        original_edid: None,
        original_config: DisplayConfig {
            paths: vec![DisplayConfigPath {
                source_id: 0,
                source: NvSourceModeInfo {
                    resolution: NvResolution {
                        width: 1680,
                        height: 1050,
                        color_depth: 32,
                    },
                    ..NvSourceModeInfo::default()
                },
                targets: vec![DisplayTargetInfo {
                    display_id: 1,
                    target_id: 1,
                    advanced: Some(DisplayTargetAdvancedInfo::default()),
                }],
                non_nvidia_adapter: false,
                reserved_path_flags: 0,
                os_adapter_luid: None,
            }],
        },
        display_id: Some(1),
        width: 3600,
        height: 2338,
        refresh_hz: 60,
        ownership: TimingOwnership::NotTried,
        custom: None,
        custom_snapshot_complete: true,
        pre_existing_custom: Vec::new(),
        cleanup_stage: CleanupStage::Pending,
        edid_write_stage: EdidWriteStage::None,
        intended_edid_sha256: None,
    }
}

#[allow(dead_code)]
pub fn restore_recovery<D: NvapiDriver>(
    driver: &mut D,
    recovery: &RecoveryData,
) -> Result<(), String> {
    restore_recovery_staged(driver, recovery, |_| Ok(()))
}

fn reconstruct_complete_recovery_config(
    current: &DisplayConfig,
    original: &DisplayConfig,
    bindings: &std::collections::BTreeMap<u32, DisplayMapping>,
    boot_paths: &std::collections::BTreeMap<u32, BootPathBinding>,
) -> Result<DisplayConfig, String> {
    validate_display_config(current)?;
    validate_display_config(original)?;

    let mut current_targets = std::collections::BTreeMap::new();
    for (path_index, path) in current.paths.iter().enumerate() {
        if path.non_nvidia_adapter {
            continue;
        }
        for target in &path.targets {
            if current_targets
                .insert(target.display_id, (path_index, target))
                .is_some()
            {
                return Err(format!(
                    "current NVAPI topology repeats stable display id 0x{:08x}",
                    target.display_id
                ));
            }
        }
    }

    let mut persisted_ids = std::collections::BTreeSet::new();
    let mut reconstructed = Vec::with_capacity(original.paths.len());
    for original_path in &original.paths {
        if original_path.non_nvidia_adapter {
            return Err(
                "journal NVAPI topology contains a non-NVIDIA path without a stable NVAPI identity"
                    .to_string(),
            );
        }
        let first_target = original_path
            .targets
            .first()
            .ok_or_else(|| "journal NVAPI path has no stable display targets".to_string())?;
        let first_binding = bindings.get(&first_target.display_id).ok_or_else(|| {
            format!(
                "stable journal display id 0x{:08x} lacks a current NVAPI binding",
                first_target.display_id
            )
        })?;

        let mut path = original_path.clone();
        let mut source_ids = original_path
            .targets
            .iter()
            .map(|target| {
                boot_paths
                    .get(&target.display_id)
                    .map(|binding| binding.source_id)
                    .ok_or_else(|| {
                        format!(
                            "connected stable display id 0x{:08x} lacks a current all-path source id",
                            target.display_id
                        )
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        source_ids.sort_unstable();
        source_ids.dedup();
        let [source_id] = source_ids.as_slice() else {
            return Err("journal clone group maps to multiple current CCD sources".to_string());
        };
        let _current_source_id = *source_id;
        // The public NVAPI contract permits zero for every NVIDIA source so
        // the driver computes per-adapter CCD source IDs. Replaying current
        // global CCD IDs is invalid when paths span multiple adapters.
        path.source_id = 0;
        path.os_adapter_luid = None;
        for target in &mut path.targets {
            if !persisted_ids.insert(target.display_id) {
                return Err(format!(
                    "journal NVAPI topology repeats stable display id 0x{:08x}",
                    target.display_id
                ));
            }
            let target_binding = bindings.get(&target.display_id).ok_or_else(|| {
                format!(
                    "stable journal display id 0x{:08x} lacks a current NVAPI binding",
                    target.display_id
                )
            })?;
            if target_binding.adapter_luid != first_binding.adapter_luid
                || target_binding.gpu != first_binding.gpu
            {
                return Err(format!(
                    "journal clone group spans current adapters at display id 0x{:08x}",
                    target.display_id
                ));
            }
            let _current_target_id = boot_paths
                .get(&target.display_id)
                .ok_or_else(|| {
                    format!(
                        "connected stable display id 0x{:08x} lacks a current all-path target id",
                        target.display_id
                    )
                })?
                .target_id;
            // targetId is a Windows CCD field for non-NVIDIA adapters and is
            // ignored for NVIDIA paths. Keep it zero rather than replaying a
            // boot-local CCD identifier.
            target.target_id = 0;
        }
        reconstructed.push(path);
    }

    let config = DisplayConfig {
        paths: reconstructed,
    };
    validate_display_config(&config)?;
    Ok(config)
}

pub(crate) fn restore_recovery_staged<D, F>(
    driver: &mut D,
    recovery: &RecoveryData,
    checkpoint: F,
) -> Result<(), String>
where
    D: NvapiDriver,
    F: FnMut(CleanupStage) -> Result<(), String>,
{
    restore_recovery_staged_with_topology_fallback(driver, recovery, None, checkpoint, |error| {
        Err(format!("restore NVAPI topology: {error}"))
    })
}

pub(crate) fn restore_recovery_staged_with_topology_fallback<D, F, G>(
    driver: &mut D,
    recovery: &RecoveryData,
    boot_paths: Option<&std::collections::BTreeMap<u32, BootPathBinding>>,
    checkpoint: F,
    topology_fallback: G,
) -> Result<(), String>
where
    D: NvapiDriver,
    F: FnMut(CleanupStage) -> Result<(), String>,
    G: FnMut(String) -> Result<(), String>,
{
    if recovery.ownership == TimingOwnership::CleanupComplete
        || recovery.cleanup_stage == CleanupStage::Complete
    {
        return Ok(());
    }
    let mapping = driver.map_recovery_display(
        &recovery.device_name,
        recovery.adapter_luid,
        &recovery.original_config,
        recovery.display_id,
    )?;
    let mut bindings = std::collections::BTreeMap::new();
    bindings.insert(mapping.display_id, mapping);
    for display_id in recovery
        .original_config
        .paths
        .iter()
        .filter(|path| !path.non_nvidia_adapter)
        .flat_map(|path| path.targets.iter().map(|target| target.display_id))
    {
        if bindings.contains_key(&display_id) {
            continue;
        }
        let binding = driver.map_recovery_display(
            &recovery.device_name,
            recovery.adapter_luid,
            &recovery.original_config,
            Some(display_id),
        )?;
        bindings.insert(display_id, binding);
    }
    let current = driver
        .get_display_config()
        .map_err(|error| format!("read current NVAPI topology for complete recovery: {error}"))?;
    let derived_boot_paths;
    let boot_paths = match boot_paths {
        Some(bindings) => bindings,
        None => {
            derived_boot_paths = current
                .paths
                .iter()
                .flat_map(|path| {
                    path.targets.iter().map(move |target| {
                        (
                            target.display_id,
                            BootPathBinding {
                                source_id: path.source_id,
                                target_id: target.target_id,
                            },
                        )
                    })
                })
                .collect();
            &derived_boot_paths
        }
    };
    let restore_config = reconstruct_complete_recovery_config(
        &current,
        &recovery.original_config,
        &bindings,
        boot_paths,
    )?;
    let snapshot = ExactModeSnapshot {
        mapping,
        original_edid: recovery.original_edid.clone(),
        original_config: restore_config.clone(),
        application_config: restore_config,
        custom_snapshot_complete: recovery.custom_snapshot_complete,
        pre_existing_custom: recovery.pre_existing_custom.clone(),
    };
    let active = (recovery.ownership != TimingOwnership::NotTried
        || recovery.edid_write_stage != EdidWriteStage::None)
        .then(|| ActiveExactMode {
            custom: recovery.custom.clone(),
            ownership: recovery.ownership,
            save_error: None,
            custom_snapshot_complete: recovery.custom_snapshot_complete,
            pre_existing_custom: recovery.pre_existing_custom.clone(),
            edid_write_stage: recovery.edid_write_stage,
            intended_edid_sha256: recovery.intended_edid_sha256.clone(),
        });
    restore_exact_staged_with_topology_fallback(
        driver,
        &snapshot,
        active.as_ref(),
        recovery.cleanup_stage,
        checkpoint,
        topology_fallback,
    )
}

const fn nvapi_version<T>(version: u32) -> u32 {
    std::mem::size_of::<T>() as u32 | (version << 16)
}

#[cfg(windows)]
mod dynamic {
    use super::*;
    use std::ffi::CString;
    use std::sync::OnceLock;
    use windows::core::{PCSTR, PCWSTR};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::LibraryLoader::{
        GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
    };

    type QueryInterface = unsafe extern "C" fn(u32) -> *mut c_void;
    type InitializeFn = unsafe extern "C" fn() -> i32;
    type GetErrorMessageFn = unsafe extern "C" fn(i32, *mut c_char) -> i32;
    type EnumPhysicalGpusFn = unsafe extern "C" fn(*mut NvPhysicalGpuHandle, *mut u32) -> i32;
    type GetDisplayIdByNameFn = unsafe extern "C" fn(*const c_char, *mut u32) -> i32;
    type GetGpuAndOutputIdFn = unsafe extern "C" fn(u32, *mut NvPhysicalGpuHandle, *mut u32) -> i32;
    type GetAdapterIdFn = unsafe extern "C" fn(NvPhysicalGpuHandle, *mut c_void) -> i32;
    type GetAllDisplayIdsFn =
        unsafe extern "C" fn(NvPhysicalGpuHandle, *mut NvGpuDisplayIds, *mut u32) -> i32;
    type GetEdidFn = unsafe extern "C" fn(NvPhysicalGpuHandle, u32, *mut NvEdid) -> i32;
    type SetEdidFn = unsafe extern "C" fn(NvPhysicalGpuHandle, u32, *mut NvEdid) -> i32;
    type GetTimingFn = unsafe extern "C" fn(u32, *mut NvTimingInput, *mut NvTiming) -> i32;
    type EnumCustomDisplayFn = unsafe extern "C" fn(u32, u32, *mut NvCustomDisplayRaw) -> i32;
    type CustomDisplayFn = unsafe extern "C" fn(*mut u32, u32, *mut NvCustomDisplayRaw) -> i32;
    type SaveCustomDisplayFn = unsafe extern "C" fn(*mut u32, u32, u32, u32) -> i32;
    type RevertCustomDisplayFn = unsafe extern "C" fn(*mut u32, u32) -> i32;
    type GetDisplayConfigFn = unsafe extern "C" fn(*mut u32, *mut NvPathInfoRaw) -> i32;
    type SetDisplayConfigFn = unsafe extern "C" fn(u32, *mut NvPathInfoRaw, u32) -> i32;

    #[derive(Clone, Copy)]
    struct Api {
        get_error_message: GetErrorMessageFn,
        enum_physical_gpus: EnumPhysicalGpusFn,
        get_display_id_by_name: GetDisplayIdByNameFn,
        get_gpu_and_output_id: GetGpuAndOutputIdFn,
        get_adapter_id: GetAdapterIdFn,
        get_all_display_ids: GetAllDisplayIdsFn,
        get_edid: GetEdidFn,
        set_edid: SetEdidFn,
        get_timing: GetTimingFn,
        enum_custom_display: EnumCustomDisplayFn,
        try_custom_display: CustomDisplayFn,
        delete_custom_display: CustomDisplayFn,
        save_custom_display: SaveCustomDisplayFn,
        revert_custom_display: RevertCustomDisplayFn,
        get_display_config: GetDisplayConfigFn,
        set_display_config: SetDisplayConfigFn,
    }

    impl Api {
        fn load() -> Result<Self, String> {
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
            // SAFETY: the module was loaded successfully and the symbol name is static,
            // null-terminated ASCII.
            let raw_query =
                unsafe { GetProcAddress(module, PCSTR(c"nvapi_QueryInterface".as_ptr().cast())) }
                    .ok_or_else(|| "nvapi64.dll does not export nvapi_QueryInterface".to_string())?;
            let query_address = raw_query as *const ();
            // SAFETY: NVIDIA documents nvapi_QueryInterface as a cdecl function taking
            // one u32 interface ID and returning the corresponding function address.
            let query: QueryInterface = unsafe { std::mem::transmute(query_address) };

            // Keep the NVIDIA driver module pinned for the process. All resolved
            // function pointers below would be invalid after FreeLibrary.
            let api = Self {
                get_error_message: resolve(query, ID_GET_ERROR_MESSAGE, "NvAPI_GetErrorMessage")?,
                enum_physical_gpus: resolve(
                    query,
                    ID_ENUM_PHYSICAL_GPUS,
                    "NvAPI_EnumPhysicalGPUs",
                )?,
                get_display_id_by_name: resolve(
                    query,
                    ID_DISP_GET_DISPLAY_ID_BY_NAME,
                    "NvAPI_DISP_GetDisplayIdByDisplayName",
                )?,
                get_gpu_and_output_id: resolve(
                    query,
                    ID_SYS_GET_GPU_AND_OUTPUT_ID,
                    "NvAPI_SYS_GetGpuAndOutputIdFromDisplayId",
                )?,
                get_adapter_id: resolve(
                    query,
                    ID_GPU_GET_ADAPTER_ID,
                    "NvAPI_GPU_GetAdapterIdFromPhysicalGpu",
                )?,
                get_all_display_ids: resolve(
                    query,
                    ID_GPU_GET_ALL_DISPLAY_IDS,
                    "NvAPI_GPU_GetAllDisplayIds",
                )?,
                get_edid: resolve(query, ID_GPU_GET_EDID, "NvAPI_GPU_GetEDID")?,
                set_edid: resolve(query, ID_GPU_SET_EDID, "NvAPI_GPU_SetEDID")?,
                get_timing: resolve(query, ID_DISP_GET_TIMING, "NvAPI_DISP_GetTiming")?,
                enum_custom_display: resolve(
                    query,
                    ID_DISP_ENUM_CUSTOM_DISPLAY,
                    "NvAPI_DISP_EnumCustomDisplay",
                )?,
                try_custom_display: resolve(
                    query,
                    ID_DISP_TRY_CUSTOM_DISPLAY,
                    "NvAPI_DISP_TryCustomDisplay",
                )?,
                delete_custom_display: resolve(
                    query,
                    ID_DISP_DELETE_CUSTOM_DISPLAY,
                    "NvAPI_DISP_DeleteCustomDisplay",
                )?,
                save_custom_display: resolve(
                    query,
                    ID_DISP_SAVE_CUSTOM_DISPLAY,
                    "NvAPI_DISP_SaveCustomDisplay",
                )?,
                revert_custom_display: resolve(
                    query,
                    ID_DISP_REVERT_CUSTOM_DISPLAY,
                    "NvAPI_DISP_RevertCustomDisplayTrial",
                )?,
                get_display_config: resolve(
                    query,
                    ID_DISP_GET_DISPLAY_CONFIG,
                    "NvAPI_DISP_GetDisplayConfig",
                )?,
                set_display_config: resolve(
                    query,
                    ID_DISP_SET_DISPLAY_CONFIG,
                    "NvAPI_DISP_SetDisplayConfig",
                )?,
            };
            let initialize: InitializeFn = resolve(query, ID_INITIALIZE, "NvAPI_Initialize")?;
            // SAFETY: initialize was resolved from NVIDIA's public interface ID with
            // the exact documented no-argument signature.
            let status = unsafe { initialize() };
            if status != NVAPI_OK {
                return Err(format!("NvAPI_Initialize returned status {status}"));
            }
            Ok(api)
        }

        fn status(self, operation: &str, status: i32) -> Result<(), String> {
            if status == NVAPI_OK {
                return Ok(());
            }
            let mut message = [0i8; NVAPI_SHORT_STRING_MAX];
            // SAFETY: message points to the documented writable 64-byte short string.
            let message_status = unsafe { (self.get_error_message)(status, message.as_mut_ptr()) };
            let text = if message_status == NVAPI_OK {
                let bytes: Vec<u8> = message
                    .iter()
                    .take_while(|byte| **byte != 0)
                    .map(|byte| *byte as u8)
                    .collect();
                String::from_utf8_lossy(&bytes).into_owned()
            } else {
                format!("NvAPI_GetErrorMessage returned {message_status}")
            };
            Err(format!(
                "{operation} returned NVAPI status {status} ({text})"
            ))
        }
    }

    fn api() -> Result<Api, String> {
        static API: OnceLock<Api> = OnceLock::new();
        if let Some(api) = API.get() {
            return Ok(*api);
        }
        let loaded = Api::load()?;
        let _ = API.set(loaded);
        Ok(*API.get().unwrap_or(&loaded))
    }

    fn resolve<T: Copy>(query: QueryInterface, id: u32, name: &str) -> Result<T, String> {
        // SAFETY: QueryInterface is initialized from the NVIDIA export and accepts
        // every public interface ID as a u32.
        let pointer = unsafe { query(id) };
        if pointer.is_null() {
            return Err(format!(
                "{name} (QueryInterface id 0x{id:08x}) is unavailable"
            ));
        }
        if std::mem::size_of::<T>() != std::mem::size_of::<*mut c_void>() {
            return Err(format!("{name} function pointer has unexpected Rust size"));
        }
        // SAFETY: each call site supplies the exact function-pointer signature from
        // the pinned public header and the size equality is checked above.
        Ok(unsafe { std::mem::transmute_copy(&pointer) })
    }

    pub struct Nvapi {
        api: Api,
    }

    impl Nvapi {
        pub fn load() -> Result<Self, String> {
            Ok(Self { api: api()? })
        }

        fn status(&self, operation: &str, status: i32) -> Result<(), String> {
            self.api.status(operation, status)
        }

        fn raw_display_config(&self) -> Result<OwnedDisplayConfigRaw, String> {
            let mut count = 0u32;
            // SAFETY: count is writable and a null path array requests the required count.
            self.status("NvAPI_DISP_GetDisplayConfig(count)", unsafe {
                (self.api.get_display_config)(&mut count, std::ptr::null_mut())
            })?;
            if count == 0 {
                return Err("NVAPI returned an empty display topology".to_string());
            }
            if count as usize > MAX_DISPLAY_CONFIG_PATHS {
                return Err(format!(
                    "NVAPI returned {count} display paths; safety limit is {MAX_DISPLAY_CONFIG_PATHS}"
                ));
            }

            let mut paths = vec![NvPathInfoRaw::default(); count as usize];
            let mut sources: Vec<Box<NvSourceModeInfo>> = (0..count)
                .map(|_| Box::new(NvSourceModeInfo::default()))
                .collect();
            for (path, source) in paths.iter_mut().zip(sources.iter_mut()) {
                path.source = source.as_mut();
            }
            // SAFETY: paths contains count initialized versioned entries and every
            // source pointer refers to a stable Box allocation.
            self.status("NvAPI_DISP_GetDisplayConfig(paths)", unsafe {
                (self.api.get_display_config)(&mut count, paths.as_mut_ptr())
            })?;
            paths.truncate(count as usize);
            sources.truncate(count as usize);

            let mut targets: Vec<Vec<NvTargetInfoRaw>> = paths
                .iter()
                .map(|path| {
                    if path.target_count as usize > MAX_TARGETS_PER_PATH {
                        Err(format!(
                            "NVAPI path has {} targets; safety limit is {MAX_TARGETS_PER_PATH}",
                            path.target_count
                        ))
                    } else {
                        Ok(vec![NvTargetInfoRaw::default(); path.target_count as usize])
                    }
                })
                .collect::<Result<_, _>>()?;
            let mut advanced: Vec<Vec<Option<Box<NvAdvancedTargetInfoRaw>>>> = paths
                .iter()
                .map(|path| {
                    (0..path.target_count)
                        .map(|_| {
                            if path.flags & 1 != 0 {
                                None
                            } else {
                                Some(Box::new(NvAdvancedTargetInfoRaw::default()))
                            }
                        })
                        .collect()
                })
                .collect();
            let mut os_adapters: Vec<Option<Box<AdapterLuid>>> = paths
                .iter()
                .map(|path| (path.flags & 1 != 0).then(|| Box::new(AdapterLuid::default())))
                .collect();
            for (path_index, (path, target)) in paths.iter_mut().zip(targets.iter_mut()).enumerate()
            {
                for (target_index, target_info) in target.iter_mut().enumerate() {
                    target_info.details = advanced[path_index][target_index]
                        .as_mut()
                        .map_or(std::ptr::null_mut(), |details| {
                            details.as_mut() as *mut NvAdvancedTargetInfoRaw as *mut c_void
                        });
                }
                path.targets = if target.is_empty() {
                    std::ptr::null_mut()
                } else {
                    target.as_mut_ptr()
                };
                path.os_adapter_id = os_adapters[path_index]
                    .as_mut()
                    .map_or(std::ptr::null_mut(), |luid| {
                        luid.as_mut() as *mut AdapterLuid as *mut c_void
                    });
            }
            // SAFETY: path/source/target/advanced/LUID allocations remain stable
            // for this call and every public structure is initialized/versioned.
            self.status("NvAPI_DISP_GetDisplayConfig(details)", unsafe {
                (self.api.get_display_config)(&mut count, paths.as_mut_ptr())
            })?;
            if count as usize != paths.len()
                || paths
                    .iter()
                    .zip(targets.iter())
                    .any(|(path, targets)| path.target_count as usize != targets.len())
            {
                return Err(
                    "NVAPI display topology changed while advanced details were queried"
                        .to_string(),
                );
            }
            let raw = OwnedDisplayConfigRaw {
                paths,
                sources,
                targets,
                advanced,
                os_adapters,
            };
            raw.to_config()?;
            Ok(raw)
        }

        fn mapping_from_display_id(
            &self,
            display_id: u32,
            expected_adapter_luid: Option<AdapterLuid>,
            require_connected: bool,
        ) -> Result<DisplayMapping, String> {
            let mut gpu = std::ptr::null_mut();
            let mut output_id = 0u32;
            // SAFETY: output pointers are valid and display_id came from NVAPI
            // or an immutable NVAPI topology snapshot.
            self.status("NvAPI_SYS_GetGpuAndOutputIdFromDisplayId", unsafe {
                (self.api.get_gpu_and_output_id)(display_id, &mut gpu, &mut output_id)
            })?;
            if gpu.is_null() || output_id.count_ones() != 1 {
                return Err(format!(
                    "NVAPI mapped display 0x{display_id:08x} to invalid GPU/output 0x{output_id:08x}"
                ));
            }

            let mut nv_luid = AdapterLuid::default();
            // SAFETY: nv_luid is an initialized writable Windows LUID and gpu is a
            // physical handle returned by NVAPI.
            self.status("NvAPI_GPU_GetAdapterIdFromPhysicalGpu", unsafe {
                (self.api.get_adapter_id)(gpu, (&mut nv_luid as *mut AdapterLuid).cast())
            })?;
            if let Some(expected) = expected_adapter_luid {
                if nv_luid != expected {
                    return Err(format!(
                        "NVAPI adapter LUID {:08x}:{:08x} does not match selected DXGI LUID {:08x}:{:08x}",
                        nv_luid.high_part as u32,
                        nv_luid.low_part,
                        expected.high_part as u32,
                        expected.low_part
                    ));
                }
            }

            let mut gpu_count = 0u32;
            let mut gpus = [std::ptr::null_mut(); NVAPI_MAX_PHYSICAL_GPUS];
            // SAFETY: the fixed array and count pointer match the public API contract.
            self.status("NvAPI_EnumPhysicalGPUs", unsafe {
                (self.api.enum_physical_gpus)(gpus.as_mut_ptr(), &mut gpu_count)
            })?;
            if gpu_count as usize > gpus.len() {
                return Err(format!(
                    "NVAPI returned {gpu_count} physical GPUs; capacity is {}",
                    gpus.len()
                ));
            }
            if !gpus[..gpu_count as usize].contains(&gpu) {
                return Err("mapped NVAPI physical GPU is absent from enumeration".to_string());
            }

            let mut display_count = 0u32;
            // SAFETY: a null display array requests the count for a valid GPU handle.
            self.status("NvAPI_GPU_GetAllDisplayIds(count)", unsafe {
                (self.api.get_all_display_ids)(gpu, std::ptr::null_mut(), &mut display_count)
            })?;
            if display_count as usize > MAX_GPU_DISPLAY_IDS {
                return Err(format!(
                    "NVAPI returned {display_count} display IDs; safety limit is {MAX_GPU_DISPLAY_IDS}"
                ));
            }
            let mut displays = vec![NvGpuDisplayIds::initialized(); display_count as usize];
            // SAFETY: display storage has one initialized versioned element per count.
            self.status("NvAPI_GPU_GetAllDisplayIds(values)", unsafe {
                (self.api.get_all_display_ids)(gpu, displays.as_mut_ptr(), &mut display_count)
            })?;
            let display = displays[..display_count as usize]
                .iter()
                .find(|display| display.display_id == display_id)
                .ok_or_else(|| {
                    format!(
                        "display id 0x{display_id:08x} is not attached to its mapped physical GPU"
                    )
                })?;
            if require_connected && display.flags & NV_GPU_DISPLAY_ID_FLAG_CONNECTED == 0 {
                return Err(format!(
                    "display id 0x{display_id:08x} is not currently connected"
                ));
            }

            Ok(DisplayMapping {
                display_id,
                output_id,
                head: output_id.trailing_zeros(),
                adapter_luid: nv_luid,
                gpu: gpu as usize,
            })
        }

        pub(crate) fn map_headless_display_id(
            &self,
            display_id: u32,
            expected_adapter_luid: AdapterLuid,
        ) -> Result<DisplayMapping, String> {
            self.mapping_from_display_id(display_id, Some(expected_adapter_luid), false)
        }

        pub(crate) fn activate_extended_displays(&self, display_ids: &[u32]) -> Result<(), String> {
            if display_ids.is_empty() {
                return Err("NVAPI extended topology has no displays".to_string());
            }
            if display_ids.iter().copied().collect::<BTreeSet<_>>().len() != display_ids.len() {
                return Err("NVAPI extended topology repeats a display id".to_string());
            }
            let mut targets = display_ids
                .iter()
                .map(|display_id| {
                    vec![NvTargetInfoRaw {
                        display_id: *display_id,
                        details: std::ptr::null_mut(),
                        target_id: 0,
                    }]
                })
                .collect::<Vec<_>>();
            let mut paths = targets
                .iter_mut()
                .map(|targets| NvPathInfoRaw {
                    version: nvapi_version::<NvPathInfoRaw>(2),
                    source_id: 0,
                    target_count: 1,
                    targets: targets.as_mut_ptr(),
                    source: std::ptr::null_mut(),
                    flags: 0,
                    os_adapter_id: std::ptr::null_mut(),
                })
                .collect::<Vec<_>>();
            // NVIDIA's DisplayConfiguration sample activates extended heads
            // with target-only paths first, allowing the driver to allocate
            // source IDs and modes before a second call positions them.
            self.status("NvAPI_DISP_SetDisplayConfig(extended)", unsafe {
                (self.api.set_display_config)(paths.len() as u32, paths.as_mut_ptr(), 0)
            })
        }

        pub(super) fn set_display_config_with_flags(
            &self,
            config: &DisplayConfig,
            flags: u32,
        ) -> Result<(), String> {
            let prepared = OwnedDisplayConfigRaw::config_for_nvapi_application(config)?;
            let mut raw = OwnedDisplayConfigRaw::from_config(&prepared)?;
            // SAFETY: raw owns stable Box/Vec backing for every path/source/target/
            // advanced/LUID pointer for the duration of this synchronous call.
            self.status("NvAPI_DISP_SetDisplayConfig", unsafe {
                (self.api.set_display_config)(raw.paths.len() as u32, raw.paths.as_mut_ptr(), flags)
            })
        }
    }

    impl NvapiDriver for Nvapi {
        fn map_display(
            &mut self,
            device_name: &str,
            adapter_luid: AdapterLuid,
        ) -> Result<DisplayMapping, String> {
            let mut display_id = 0u32;
            let mut names = vec![device_name.to_string()];
            if let Some(name) = device_name.strip_prefix(r"\\.\") {
                names.push(format!(r"\\{name}"));
            }
            let mut last_error = String::new();
            for name in names {
                let name = CString::new(name)
                    .map_err(|_| "Windows display name contains a NUL byte".to_string())?;
                // SAFETY: name is NUL-terminated and display_id is writable.
                let status =
                    unsafe { (self.api.get_display_id_by_name)(name.as_ptr(), &mut display_id) };
                match self.status("NvAPI_DISP_GetDisplayIdByDisplayName", status) {
                    Ok(()) => {
                        last_error.clear();
                        break;
                    }
                    Err(error) => last_error = error,
                }
            }
            if !last_error.is_empty() {
                return Err(last_error);
            }

            self.mapping_from_display_id(display_id, Some(adapter_luid), true)
        }

        fn map_recovery_display(
            &mut self,
            device_name: &str,
            adapter_luid: AdapterLuid,
            original_config: &DisplayConfig,
            display_id: Option<u32>,
        ) -> Result<DisplayMapping, String> {
            if let Some(display_id) = display_id {
                return self
                    .mapping_from_display_id(display_id, None, true)
                    .map_err(|error| {
                        format!("exact journal display id 0x{display_id:08x} failed: {error}")
                    });
            }
            let name_error = match self.map_display(device_name, adapter_luid) {
                Ok(mapping) => return Ok(mapping),
                Err(error) => error,
            };
            let mut matches = Vec::new();
            let mut fallback_errors = Vec::new();
            for display_id in original_config
                .paths
                .iter()
                .filter(|path| !path.non_nvidia_adapter)
                .flat_map(|path| path.targets.iter().map(|target| target.display_id))
            {
                match self.mapping_from_display_id(display_id, Some(adapter_luid), true) {
                    Ok(mapping) => matches.push(mapping),
                    Err(error) => fallback_errors.push(format!("0x{display_id:08x}: {error}")),
                }
            }
            matches.sort_by_key(|mapping| mapping.display_id);
            matches.dedup_by_key(|mapping| mapping.display_id);
            match matches.as_slice() {
                [mapping] => Ok(*mapping),
                [] => Err(format!(
                    "{name_error}; journal display-id recovery found no exact adapter match ({})",
                    fallback_errors.join("; ")
                )),
                _ => Err(format!(
                    "{name_error}; journal display-id recovery is ambiguous across {} outputs",
                    matches.len()
                )),
            }
        }

        fn get_edid(&mut self, mapping: DisplayMapping) -> Result<Option<Vec<u8>>, String> {
            let mut edid = NvEdid::default();
            // SAFETY: edid is a writable versioned structure and mapping came from NVAPI.
            let status = unsafe {
                (self.api.get_edid)(
                    mapping.gpu as NvPhysicalGpuHandle,
                    mapping.output_id,
                    &mut edid,
                )
            };
            if status == NVAPI_DATA_NOT_FOUND {
                return Ok(None);
            }
            self.status("NvAPI_GPU_GetEDID", status)?;
            if edid.size == 0 {
                return Ok(None);
            }
            if edid.size as usize > edid.data.len() {
                return Err(format!(
                    "NvAPI_GPU_GetEDID returned unsupported {}-byte EDID page",
                    edid.size
                ));
            }
            Ok(Some(edid.data[..edid.size as usize].to_vec()))
        }

        fn set_edid(&mut self, mapping: DisplayMapping, bytes: &[u8]) -> Result<(), String> {
            if bytes.len() > NV_EDID_DATA_SIZE {
                return Err(format!(
                    "NVAPI EDID payload exceeds {NV_EDID_DATA_SIZE} bytes"
                ));
            }
            let mut edid = NvEdid::default();
            edid.size = bytes.len() as u32;
            edid.data[..bytes.len()].copy_from_slice(bytes);
            // SAFETY: edid is initialized and mapping uses the one-bit output ID
            // returned by NvAPI_SYS_GetGpuAndOutputIdFromDisplayId.
            self.status("NvAPI_GPU_SetEDID", unsafe {
                (self.api.set_edid)(
                    mapping.gpu as NvPhysicalGpuHandle,
                    mapping.output_id,
                    &mut edid,
                )
            })
        }

        fn get_display_config(&mut self) -> Result<DisplayConfig, String> {
            self.raw_display_config()?.to_config()
        }

        fn set_display_config(&mut self, config: &DisplayConfig) -> Result<(), String> {
            self.set_display_config_with_flags(
                config,
                NV_DISPLAYCONFIG_FORCE_MODE_ENUMERATION | NV_FORCE_COMMIT_VIDPN,
            )
        }

        fn calculate_custom_display(
            &mut self,
            display_id: u32,
            width: u32,
            height: u32,
            refresh_hz: u32,
        ) -> Result<CustomDisplay, String> {
            let mut input = NvTimingInput {
                version: nvapi_version::<NvTimingInput>(1),
                width,
                height,
                refresh_hz: refresh_hz as f32,
                flags: NvTimingFlag {
                    scaling: 1,
                    ..NvTimingFlag::default()
                },
                timing_type: NV_TIMING_OVERRIDE_CVT_RB,
            };
            let mut timing = NvTiming::default();
            // SAFETY: input/output are initialized structures with versions/layouts
            // from the pinned public header.
            self.status("NvAPI_DISP_GetTiming", unsafe {
                (self.api.get_timing)(display_id, &mut input, &mut timing)
            })?;
            Ok(CustomDisplay {
                raw: NvCustomDisplayRaw {
                    version: nvapi_version::<NvCustomDisplayRaw>(1),
                    width,
                    height,
                    depth: 32,
                    color_format: NV_FORMAT_A8R8G8B8,
                    source_partition: NvViewportF {
                        x: 0.0,
                        y: 0.0,
                        width: 1.0,
                        height: 1.0,
                    },
                    x_ratio: 1.0,
                    y_ratio: 1.0,
                    timing,
                    flags: 0,
                },
            })
        }

        fn enum_custom_displays(&mut self, display_id: u32) -> Result<Vec<CustomDisplay>, String> {
            let mut displays = Vec::new();
            for index in 0..MAX_CUSTOM_DISPLAYS {
                let mut raw = NvCustomDisplayRaw {
                    version: nvapi_version::<NvCustomDisplayRaw>(1),
                    ..NvCustomDisplayRaw::default()
                };
                // SAFETY: raw is an initialized, correctly versioned public
                // NV_CUSTOM_DISPLAY and remains writable for the synchronous call.
                let status =
                    unsafe { (self.api.enum_custom_display)(display_id, index as u32, &mut raw) };
                if status == NVAPI_END_ENUMERATION {
                    return Ok(displays);
                }
                self.status("NvAPI_DISP_EnumCustomDisplay", status)?;
                let display = CustomDisplay { raw };
                if !display.is_valid() {
                    return Err(format!(
                        "NvAPI_DISP_EnumCustomDisplay returned invalid entry at index {index}"
                    ));
                }
                displays.push(display);
            }
            Err(format!(
                "NvAPI_DISP_EnumCustomDisplay exceeded safety limit {MAX_CUSTOM_DISPLAYS}"
            ))
        }

        fn try_custom_display(
            &mut self,
            display_id: u32,
            custom: &CustomDisplay,
        ) -> Result<(), String> {
            for attempt in 0..3 {
                let mut id = display_id;
                let mut raw = custom.raw;
                // SAFETY: the one-element arrays and versioned custom timing remain valid
                // for the synchronous call.
                let status = unsafe { (self.api.try_custom_display)(&mut id, 1, &mut raw) };
                if status != NVAPI_INSUFFICIENT_BUFFER || attempt == 2 {
                    return self.status("NvAPI_DISP_TryCustomDisplay", status);
                }
                tracing::warn!(
                    display_id = format_args!("{display_id:#x}"),
                    attempt = attempt + 1,
                    "NvAPI_DISP_TryCustomDisplay reported a transient insufficient buffer; retrying"
                );
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            unreachable!()
        }

        fn save_custom_display(&mut self, display_id: u32) -> Result<(), String> {
            let mut id = display_id;
            // SAFETY: id points to the one active display being saved.
            self.status("NvAPI_DISP_SaveCustomDisplay", unsafe {
                (self.api.save_custom_display)(&mut id, 1, 1, 1)
            })
        }

        fn revert_custom_display(&mut self, display_id: u32) -> Result<(), String> {
            let mut id = display_id;
            // SAFETY: id points to the one active display whose trial is reverted.
            self.status("NvAPI_DISP_RevertCustomDisplayTrial", unsafe {
                (self.api.revert_custom_display)(&mut id, 1)
            })
        }

        fn delete_custom_display(
            &mut self,
            display_id: u32,
            custom: &CustomDisplay,
        ) -> Result<(), String> {
            let mut id = display_id;
            let mut raw = custom.raw;
            // SAFETY: id/custom point to one initialized saved custom timing.
            self.status("NvAPI_DISP_DeleteCustomDisplay", unsafe {
                (self.api.delete_custom_display)(&mut id, 1, &mut raw)
            })
        }
    }

    pub use Nvapi as DynamicNvapi;
}

#[cfg(windows)]
pub use dynamic::DynamicNvapi as Nvapi;

#[cfg(not(windows))]
pub struct Nvapi;

#[cfg(not(windows))]
impl Nvapi {
    pub fn load() -> Result<Self, String> {
        Err("NVAPI is only available on Windows".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Map,
        GetEdid,
        SetEdid(usize),
        GetConfig,
        Enum,
        Timing,
        Try,
        Save,
        SetConfig(u32, u32),
        Revert,
        Delete,
    }

    struct MockDriver {
        calls: Vec<Call>,
        failures: VecDeque<Call>,
        current_edid: Option<Vec<u8>>,
        mapping: DisplayMapping,
        config: DisplayConfig,
        custom_displays: Vec<CustomDisplay>,
        pending_custom: Option<CustomDisplay>,
        named_mapping_available: bool,
        edid_writes_effective: bool,
    }

    fn custom_display(width: u32, height: u32, refresh_hz: u32) -> CustomDisplay {
        CustomDisplay::test_value(width, height, refresh_hz)
    }

    impl MockDriver {
        fn new() -> Self {
            let mapping = DisplayMapping {
                display_id: 0x1234,
                output_id: 4,
                head: 2,
                adapter_luid: AdapterLuid {
                    low_part: 7,
                    high_part: 9,
                },
                gpu: 1,
            };
            Self {
                calls: Vec::new(),
                failures: VecDeque::new(),
                current_edid: Some(vec![0xaa; 128]),
                mapping,
                config: DisplayConfig {
                    paths: vec![DisplayConfigPath {
                        source_id: 0,
                        source: NvSourceModeInfo {
                            resolution: NvResolution {
                                width: 1680,
                                height: 1050,
                                color_depth: 32,
                            },
                            ..NvSourceModeInfo::default()
                        },
                        targets: vec![DisplayTargetInfo {
                            display_id: mapping.display_id,
                            target_id: 3,
                            advanced: Some(DisplayTargetAdvancedInfo::default()),
                        }],
                        non_nvidia_adapter: false,
                        reserved_path_flags: 0,
                        os_adapter_luid: None,
                    }],
                },
                custom_displays: Vec::new(),
                pending_custom: None,
                named_mapping_available: true,
                edid_writes_effective: true,
            }
        }

        fn record(&mut self, call: Call) -> Result<(), String> {
            self.calls.push(call.clone());
            if self.failures.front() == Some(&call) {
                self.failures.pop_front();
                Err(format!("injected failure at {call:?}"))
            } else {
                Ok(())
            }
        }
    }

    impl NvapiDriver for MockDriver {
        fn map_display(
            &mut self,
            _device_name: &str,
            luid: AdapterLuid,
        ) -> Result<DisplayMapping, String> {
            self.record(Call::Map)?;
            assert_eq!(luid, self.mapping.adapter_luid);
            self.named_mapping_available
                .then_some(self.mapping)
                .ok_or_else(|| "named display disappeared".to_string())
        }

        fn map_recovery_display(
            &mut self,
            device_name: &str,
            luid: AdapterLuid,
            original_config: &DisplayConfig,
            display_id: Option<u32>,
        ) -> Result<DisplayMapping, String> {
            if let Some(display_id) = display_id {
                self.record(Call::Map)?;
                return (display_id == self.mapping.display_id)
                    .then_some(self.mapping)
                    .ok_or_else(|| "exact journal display id did not match".to_string());
            }
            match self.map_display(device_name, luid) {
                Ok(mapping) => Ok(mapping),
                Err(_)
                    if original_config
                        .paths
                        .iter()
                        .flat_map(|path| path.targets.iter())
                        .any(|target| target.display_id == self.mapping.display_id) =>
                {
                    Ok(self.mapping)
                }
                Err(error) => Err(error),
            }
        }

        fn get_edid(&mut self, _mapping: DisplayMapping) -> Result<Option<Vec<u8>>, String> {
            self.record(Call::GetEdid)?;
            Ok(self.current_edid.clone())
        }

        fn set_edid(&mut self, _mapping: DisplayMapping, edid: &[u8]) -> Result<(), String> {
            self.record(Call::SetEdid(edid.len()))?;
            if self.edid_writes_effective {
                self.current_edid = (!edid.is_empty()).then(|| edid.to_vec());
            }
            Ok(())
        }

        fn get_display_config(&mut self) -> Result<DisplayConfig, String> {
            self.record(Call::GetConfig)?;
            Ok(self.config.clone())
        }

        fn set_display_config(&mut self, config: &DisplayConfig) -> Result<(), String> {
            let source = config.paths[0].source.resolution;
            self.record(Call::SetConfig(source.width, source.height))?;
            self.config = config.clone();
            Ok(())
        }

        fn enum_custom_displays(&mut self, _display_id: u32) -> Result<Vec<CustomDisplay>, String> {
            self.record(Call::Enum)?;
            Ok(self.custom_displays.clone())
        }

        fn calculate_custom_display(
            &mut self,
            _display_id: u32,
            width: u32,
            height: u32,
            refresh_hz: u32,
        ) -> Result<CustomDisplay, String> {
            self.record(Call::Timing)?;
            Ok(custom_display(width, height, refresh_hz))
        }

        fn try_custom_display(
            &mut self,
            _display_id: u32,
            custom: &CustomDisplay,
        ) -> Result<(), String> {
            self.record(Call::Try)?;
            self.pending_custom = Some(custom.clone());
            Ok(())
        }

        fn save_custom_display(&mut self, _display_id: u32) -> Result<(), String> {
            self.record(Call::Save)?;
            let custom = self
                .pending_custom
                .clone()
                .ok_or_else(|| "save called without a pending trial".to_string())?;
            if !self.custom_displays.contains(&custom) {
                self.custom_displays.push(custom);
            }
            Ok(())
        }

        fn revert_custom_display(&mut self, _display_id: u32) -> Result<(), String> {
            self.record(Call::Revert)
        }

        fn delete_custom_display(
            &mut self,
            _display_id: u32,
            custom: &CustomDisplay,
        ) -> Result<(), String> {
            self.record(Call::Delete)?;
            let index = self
                .custom_displays
                .iter()
                .position(|item| item == custom)
                .ok_or_else(|| "delete called for unknown custom timing".to_string())?;
            self.custom_displays.remove(index);
            Ok(())
        }
    }

    fn snapshot_for(driver: &mut MockDriver) -> ExactModeSnapshot {
        let luid = driver.mapping.adapter_luid;
        snapshot(driver, r"\\.\DISPLAY6", luid, 3600, 2338, 60).unwrap()
    }

    fn apply_for(
        driver: &mut MockDriver,
        snapshot: &ExactModeSnapshot,
    ) -> Result<ActiveExactMode, ApplyExactError> {
        apply_exact(driver, snapshot, &[0x55; 128], 3600, 2338, 60, |_| Ok(()))
    }

    #[test]
    fn hdr_exact_apply_requires_the_edid_to_exist_before_session_start() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        driver.calls.clear();

        let error = apply_exact(&mut driver, &snapshot, &[0x55; 256], 3600, 2338, 60, |_| {
            Ok(())
        })
        .unwrap_err();

        assert!(error
            .message
            .contains("not provisioned before session start"));
        assert!(error.active.is_none());
        assert!(
            !driver
                .calls
                .iter()
                .any(|call| matches!(call, Call::SetEdid(_))),
            "a connected HDR session must never rewrite the EDID"
        );
    }

    #[test]
    fn preprovisioned_hdr_edid_applies_only_the_requested_topology() {
        let mut driver = MockDriver::new();
        let hdr_edid = vec![0x55; 256];
        driver.current_edid = Some(hdr_edid.clone());
        let snapshot = snapshot_for(&mut driver);
        driver.calls.clear();

        apply_exact(
            &mut driver,
            &snapshot,
            &hdr_edid,
            3600,
            2338,
            60,
            |_| Ok(()),
        )
        .expect("preprovisioned HDR display");

        assert!(
            !driver
                .calls
                .iter()
                .any(|call| matches!(call, Call::SetEdid(_) | Call::Timing)),
            "a preprovisioned HDR display needs no EDID or custom-timing mutation"
        );
        assert!(driver.calls.contains(&Call::SetConfig(3600, 2338)));
    }

    #[test]
    fn public_query_ids_are_pinned_to_nvidia_sdk_commit() {
        assert_eq!(ID_INITIALIZE, 0x0150_e828);
        assert_eq!(ID_GPU_SET_EDID, 0xe83d_6456);
        assert_eq!(ID_DISP_GET_TIMING, 0x1751_67e9);
        assert_eq!(ID_DISP_ENUM_CUSTOM_DISPLAY, 0xa207_2d59);
        assert_eq!(ID_DISP_TRY_CUSTOM_DISPLAY, 0x1f7d_b630);
        assert_eq!(ID_DISP_SAVE_CUSTOM_DISPLAY, 0x4988_2876);
        assert_eq!(ID_DISP_GET_DISPLAY_CONFIG, 0x11ab_ccf8);
        assert_eq!(ID_DISP_SET_DISPLAY_CONFIG, 0x5d8c_f8de);
    }

    #[test]
    fn retargeted_exact_modes_clean_each_owned_timing_once() {
        let mut driver = MockDriver::new();
        let original = snapshot_for(&mut driver);
        let first = apply_for(&mut driver, &original).unwrap();
        restore_exact(&mut driver, &original, Some(&first)).unwrap();

        let fallback = retarget_snapshot(&mut driver, &original, 1920, 1072, 60).unwrap();
        let second = apply_exact(&mut driver, &fallback, &[0x66; 128], 1920, 1072, 60, |_| {
            Ok(())
        })
        .unwrap();
        restore_exact(&mut driver, &fallback, Some(&second)).unwrap();

        assert_eq!(
            driver
                .calls
                .iter()
                .filter(|call| **call == Call::Revert)
                .count(),
            2
        );
        assert_eq!(
            driver
                .calls
                .iter()
                .filter(|call| **call == Call::Delete)
                .count(),
            2
        );
        assert!(driver.custom_displays.is_empty());
        assert_eq!(
            driver.config.paths[0].source.resolution,
            original.original_config.paths[0].source.resolution
        );
    }

    #[test]
    fn retarget_uses_fresh_driver_configuration_for_topology_commit() {
        let mut driver = MockDriver::new();
        let original = snapshot_for(&mut driver);
        driver.config.paths[0].source_id = 47;

        let retarget = retarget_snapshot(&mut driver, &original, 1920, 1072, 60).unwrap();
        apply_exact(&mut driver, &retarget, &[0x66; 128], 1920, 1072, 60, |_| {
            Ok(())
        })
        .unwrap();

        assert_eq!(driver.config.paths[0].source_id, 47);
        assert_eq!(original.original_config.paths[0].source_id, 0);
    }

    #[test]
    fn ffi_struct_sizes_and_versions_match_public_x64_header() {
        assert_eq!(std::mem::size_of::<AdapterLuid>(), 8);
        assert_eq!(std::mem::size_of::<NvEdid>(), 272);
        assert_eq!(std::mem::size_of::<NvGpuDisplayIds>(), 16);
        assert_eq!(std::mem::size_of::<NvTimingExt>(), 64);
        assert_eq!(std::mem::size_of::<NvTiming>(), 96);
        assert_eq!(std::mem::size_of::<NvTimingFlag>(), 12);
        assert_eq!(std::mem::size_of::<NvTimingInput>(), 32);
        assert_eq!(std::mem::size_of::<NvCustomDisplayRaw>(), 144);
        assert_eq!(std::mem::size_of::<NvTargetInfoRaw>(), 24);
        assert_eq!(std::mem::size_of::<NvAdvancedTargetInfoRaw>(), 128);
        assert_eq!(std::mem::size_of::<NvPathInfoRaw>(), 48);
        assert_eq!(nvapi_version::<NvEdid>(3), 0x0003_0110);
        assert_eq!(nvapi_version::<NvGpuDisplayIds>(3), 0x0003_0010);
        assert_eq!(nvapi_version::<NvTimingInput>(1), 0x0001_0020);
        assert_eq!(nvapi_version::<NvCustomDisplayRaw>(1), 0x0001_0090);
        assert_eq!(nvapi_version::<NvAdvancedTargetInfoRaw>(1), 0x0001_0080);
        assert_eq!(nvapi_version::<NvPathInfoRaw>(2), 0x0002_0030);
    }

    #[test]
    fn topology_round_trip_preserves_advanced_targets_and_non_nvidia_luid() {
        let advanced = |rotation, scaling, refresh_rate_1k, marker| {
            Some(DisplayTargetAdvancedInfo {
                rotation,
                scaling,
                refresh_rate_1k,
                flags: marker,
                connector: marker + 10,
                tv_format: marker + 20,
                timing_override: marker + 30,
                timing: NvTiming {
                    h_visible: 3600,
                    v_visible: 2338,
                    pixel_clock_10_khz: 50000 + marker,
                    extra: NvTimingExt {
                        rr: 60,
                        rrx1k: refresh_rate_1k,
                        ..NvTimingExt::default()
                    },
                    ..NvTiming::default()
                },
            })
        };
        let config = DisplayConfig {
            paths: vec![
                DisplayConfigPath {
                    source_id: 1,
                    source: NvSourceModeInfo::default(),
                    targets: vec![
                        DisplayTargetInfo {
                            display_id: 0x10,
                            target_id: 0,
                            advanced: advanced(1, 2, 59_940, 1),
                        },
                        DisplayTargetInfo {
                            display_id: 0x11,
                            target_id: 0,
                            advanced: advanced(3, 5, 60_000, 2),
                        },
                    ],
                    non_nvidia_adapter: false,
                    reserved_path_flags: 0,
                    os_adapter_luid: None,
                },
                DisplayConfigPath {
                    source_id: 2,
                    source: NvSourceModeInfo::default(),
                    targets: vec![DisplayTargetInfo {
                        display_id: 0,
                        target_id: 77,
                        advanced: None,
                    }],
                    non_nvidia_adapter: true,
                    reserved_path_flags: 0,
                    os_adapter_luid: Some(AdapterLuid {
                        low_part: 0x1234_5678,
                        high_part: 0x1020_3040,
                    }),
                },
            ],
        };

        let raw = OwnedDisplayConfigRaw::from_config(&config).unwrap();
        let restored = raw.to_config().unwrap();

        assert_eq!(restored, config);
        assert!(raw.targets[0]
            .iter()
            .all(|target| !target.details.is_null()));
        assert!(raw.targets[1][0].details.is_null());
        assert!(!raw.paths[1].os_adapter_id.is_null());
    }

    #[test]
    fn topology_round_trip_preserves_every_public_nv_scaling_value() {
        assert_eq!(NV_SCALING_PUBLIC_VALUES, &[0, 1, 2, 3, 5, 6, 7, 8, 255]);
        for &scaling in NV_SCALING_PUBLIC_VALUES {
            let mut config = MockDriver::new().config;
            config.paths[0].targets[0]
                .advanced
                .as_mut()
                .unwrap()
                .scaling = scaling;

            validate_display_config(&config).unwrap();
            let restored = OwnedDisplayConfigRaw::from_config(&config)
                .unwrap()
                .to_config()
                .unwrap();

            assert_eq!(restored, config, "NV_SCALING value {scaling} was lossy");
        }
    }

    #[test]
    fn topology_validation_rejects_inconsistent_or_invalid_target_metadata() {
        let mut config = MockDriver::new().config;
        config.paths[0].non_nvidia_adapter = true;
        assert!(validate_display_config(&config)
            .unwrap_err()
            .contains("OS adapter LUID"));

        config.paths[0].non_nvidia_adapter = false;
        config.paths[0].targets[0]
            .advanced
            .as_mut()
            .unwrap()
            .rotation = 5;
        assert!(validate_display_config(&config)
            .unwrap_err()
            .contains("invalid rotation"));

        config.paths[0].targets[0]
            .advanced
            .as_mut()
            .unwrap()
            .rotation = 0;
        config.paths[0].targets[0]
            .advanced
            .as_mut()
            .unwrap()
            .scaling = 4;
        assert!(validate_display_config(&config)
            .unwrap_err()
            .contains("invalid scaling"));

        let mut config = MockDriver::new().config;
        config.paths[0].reserved_path_flags = 0x20;
        assert!(validate_display_config(&config)
            .unwrap_err()
            .contains("reserved path flags"));
    }

    #[test]
    fn exact_apply_and_restore_order_includes_purge() {
        let mut driver = MockDriver::new();
        let original = driver.current_edid.clone().unwrap();
        let snapshot = snapshot_for(&mut driver);
        let active = apply_for(&mut driver, &snapshot).unwrap();
        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();

        assert_eq!(
            driver.calls,
            vec![
                Call::Map,
                Call::GetEdid,
                Call::GetConfig,
                Call::Enum,
                Call::SetEdid(128),
                Call::GetEdid,
                Call::Timing,
                Call::Try,
                Call::Save,
                Call::Enum,
                Call::SetConfig(3600, 2338),
                Call::SetEdid(0),
                Call::GetEdid,
                Call::SetEdid(128),
                Call::GetEdid,
                Call::SetConfig(1680, 1050),
                Call::Revert,
                Call::Enum,
                Call::Delete,
                Call::SetEdid(0),
                Call::GetEdid,
                Call::SetEdid(128),
                Call::GetEdid,
            ]
        );
        assert_eq!(driver.current_edid.as_deref(), Some(original.as_slice()));
    }

    #[test]
    fn recovery_restores_edid_before_original_topology() {
        let mut driver = MockDriver::new();
        let original_edid = driver.current_edid.clone().unwrap();
        let snapshot = snapshot_for(&mut driver);
        let active = apply_for(&mut driver, &snapshot).unwrap();
        driver.calls.clear();

        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();

        let initial_edid_restore = driver
            .calls
            .windows(3)
            .position(|calls| {
                calls
                    == [
                        Call::SetEdid(0),
                        Call::GetEdid,
                        Call::SetEdid(original_edid.len()),
                    ]
            })
            .unwrap();
        let topology_restore = driver
            .calls
            .iter()
            .position(|call| *call == Call::SetConfig(1680, 1050))
            .unwrap();
        assert!(initial_edid_restore < topology_restore);
    }

    #[test]
    fn recovery_rebinds_from_journal_when_windows_name_disappears() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        let mut recovery = recovery_data(r"\\.\DISPLAY2".to_string(), &snapshot, 1120, 760, 120);
        recovery.ownership = TimingOwnership::TrialAppliedByUs;
        recovery.edid_write_stage = EdidWriteStage::Verified;
        recovery.intended_edid_sha256 = Some("0".repeat(64));
        driver.named_mapping_available = false;
        driver.current_edid = Some(vec![0x55; 128]);

        restore_recovery(&mut driver, &recovery).unwrap();

        assert_eq!(
            driver.config.paths[0].source.resolution,
            snapshot.original_config.paths[0].source.resolution
        );
        assert_eq!(
            driver.config.paths[0].targets[0].display_id,
            snapshot.original_config.paths[0].targets[0].display_id
        );
        assert_eq!(
            driver.config.paths[0].targets[0].advanced,
            snapshot.original_config.paths[0].targets[0].advanced
        );
        assert_eq!(driver.current_edid, snapshot.original_edid);
    }

    #[test]
    fn recovery_never_prefers_boot_local_name_over_exact_display_id() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        let mut recovery = recovery_data(r"\\.\DISPLAY2".to_string(), &snapshot, 1120, 760, 120);
        recovery.display_id = Some(driver.mapping.display_id ^ 0x100);
        driver.named_mapping_available = true;

        let error = restore_recovery(&mut driver, &recovery).unwrap_err();
        assert!(error.contains("exact journal display id"));
        assert_eq!(driver.calls.first(), Some(&Call::Map));
    }

    #[test]
    fn complete_recovery_rebinds_every_stable_target_and_preserves_semantics() {
        let driver = MockDriver::new();
        let mut original = driver.config.clone();
        original.paths[0].source_id = 3;
        original.paths[0].source.position = NvPosition { x: 3080, y: 0 };
        original.paths[0].targets[0].target_id = 31;
        original.paths[0].targets.push(DisplayTargetInfo {
            display_id: 2,
            target_id: 32,
            advanced: Some(DisplayTargetAdvancedInfo {
                rotation: 1,
                scaling: 2,
                refresh_rate_1k: 120_000,
                ..DisplayTargetAdvancedInfo::default()
            }),
        });

        let mut current = driver.config.clone();
        current.paths[0].source_id = 91;
        current.paths[0].targets[0].target_id = 101;
        current.paths[0].targets.push(DisplayTargetInfo {
            display_id: 2,
            target_id: 102,
            advanced: None,
        });
        let mut bindings = std::collections::BTreeMap::new();
        bindings.insert(driver.mapping.display_id, driver.mapping);
        bindings.insert(
            2,
            DisplayMapping {
                display_id: 2,
                ..driver.mapping
            },
        );
        let boot_paths = std::collections::BTreeMap::from([
            (
                driver.mapping.display_id,
                BootPathBinding {
                    source_id: 91,
                    target_id: 101,
                },
            ),
            (
                2,
                BootPathBinding {
                    source_id: 91,
                    target_id: 102,
                },
            ),
        ]);

        let reconstructed =
            reconstruct_complete_recovery_config(&current, &original, &bindings, &boot_paths)
                .unwrap();
        assert_eq!(reconstructed.paths.len(), 1);
        assert_eq!(reconstructed.paths[0].source_id, 0);
        assert_eq!(reconstructed.paths[0].os_adapter_luid, None);
        assert_eq!(reconstructed.paths[0].source, original.paths[0].source);
        assert_eq!(
            reconstructed.paths[0].targets[0].display_id,
            driver.mapping.display_id
        );
        assert_eq!(reconstructed.paths[0].targets[0].target_id, 0);
        assert_eq!(reconstructed.paths[0].targets[1].display_id, 2);
        assert_eq!(reconstructed.paths[0].targets[1].target_id, 0);
        assert_eq!(
            reconstructed.paths[0].targets[1].advanced,
            original.paths[0].targets[1].advanced
        );
    }

    #[test]
    fn complete_recovery_fails_closed_for_missing_or_unstable_identity() {
        let driver = MockDriver::new();
        let original = driver.config.clone();
        let bindings = std::collections::BTreeMap::new();
        let boot_paths = std::collections::BTreeMap::new();
        assert!(reconstruct_complete_recovery_config(
            &driver.config,
            &original,
            &bindings,
            &boot_paths,
        )
        .unwrap_err()
        .contains("lacks a current NVAPI binding"));

        let bindings =
            std::collections::BTreeMap::from([(driver.mapping.display_id, driver.mapping)]);
        let boot_paths = std::collections::BTreeMap::from([(
            driver.mapping.display_id,
            BootPathBinding {
                source_id: driver.config.paths[0].source_id,
                target_id: driver.config.paths[0].targets[0].target_id,
            },
        )]);
        let mut insufficient = original.clone();
        insufficient.paths[0].non_nvidia_adapter = true;
        insufficient.paths[0].os_adapter_luid = Some(AdapterLuid {
            low_part: 1,
            high_part: 0,
        });
        insufficient.paths[0].targets[0].advanced = None;
        assert!(reconstruct_complete_recovery_config(
            &original,
            &insufficient,
            &bindings,
            &boot_paths,
        )
        .unwrap_err()
        .contains("without a stable NVAPI identity"));
    }

    #[test]
    fn complete_recovery_recreates_two_inactive_paths_after_isolation_and_reboot() {
        let driver = MockDriver::new();
        let mut original = DisplayConfig { paths: Vec::new() };
        let mut bindings = std::collections::BTreeMap::new();
        for index in 0..3_u32 {
            let mut path = driver.config.paths[0].clone();
            path.source_id = index;
            path.source.position = NvPosition {
                x: (index as i32) * 1800,
                y: 0,
            };
            path.targets[0].display_id = 0x1000 + index;
            path.targets[0].target_id = 0;
            original.paths.push(path);
            bindings.insert(
                0x1000 + index,
                DisplayMapping {
                    display_id: 0x1000 + index,
                    adapter_luid: AdapterLuid {
                        low_part: 0x2000 + index,
                        high_part: 1,
                    },
                    gpu: index as usize + 1,
                    ..driver.mapping
                },
            );
        }

        // Exact isolation left only the session output active, with fresh
        // boot-local source/target ids.
        let mut current = DisplayConfig {
            paths: vec![original.paths[0].clone()],
        };
        current.paths[0].source_id = 77;
        current.paths[0].targets[0].target_id = 88;
        let boot_paths = std::collections::BTreeMap::from([
            (
                0x1000,
                BootPathBinding {
                    source_id: 77,
                    target_id: 88,
                },
            ),
            (
                0x1001,
                BootPathBinding {
                    source_id: 78,
                    target_id: 0,
                },
            ),
            (
                0x1002,
                BootPathBinding {
                    source_id: 79,
                    target_id: 0,
                },
            ),
        ]);

        let recovered =
            reconstruct_complete_recovery_config(&current, &original, &bindings, &boot_paths)
                .unwrap();
        assert_eq!(recovered.paths.len(), 3);
        assert!(recovered.paths.iter().all(|path| path.source_id == 0));
        assert!(recovered
            .paths
            .iter()
            .flat_map(|path| &path.targets)
            .all(|target| target.target_id == 0));
        assert_eq!(
            recovered
                .paths
                .iter()
                .map(|path| path.source.position.x)
                .collect::<Vec<_>>(),
            vec![0, 1800, 3600]
        );
        assert_eq!(
            recovered
                .paths
                .iter()
                .map(|path| path.targets[0].display_id)
                .collect::<Vec<_>>(),
            vec![0x1000, 0x1001, 0x1002]
        );
    }

    #[test]
    fn nvapi_application_normalizes_subset_origin_and_boot_local_ids() {
        let driver = MockDriver::new();
        let mut config = driver.config.clone();
        let mut second = config.paths[0].clone();
        config.paths[0].source_id = 41;
        config.paths[0].source.position = NvPosition { x: 1280, y: 200 };
        config.paths[0].source.flags = 0;
        config.paths[0].targets[0].target_id = 51;
        second.source_id = 42;
        second.source.position = NvPosition { x: 3080, y: 200 };
        second.source.flags = 0;
        second.targets[0].display_id = 2;
        second.targets[0].target_id = 52;
        config.paths.push(second);

        let prepared = OwnedDisplayConfigRaw::config_for_nvapi_application(&config).unwrap();

        assert_eq!(prepared.paths[0].source.position, NvPosition { x: 0, y: 0 });
        assert_eq!(
            prepared.paths[1].source.position,
            NvPosition { x: 1800, y: 0 }
        );
        assert_eq!(prepared.paths[0].source.flags & 1, 1);
        assert_eq!(prepared.paths[1].source.flags & 1, 0);
        assert!(prepared.paths.iter().all(|path| path.source_id == 0));
        assert!(prepared
            .paths
            .iter()
            .flat_map(|path| &path.targets)
            .all(|target| target.target_id == 0));
        assert_eq!(config.paths[0].source.position.x, 1280);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires an interactive NVIDIA lab"]
    fn native_staged_topology_calls_nvapi_set_display_config() {
        let mut driver = Nvapi::load().expect("load native NVAPI");
        let before = driver
            .get_display_config()
            .expect("read native NVAPI topology");
        driver
            .set_display_config_with_flags(&before, NV_DISPLAYCONFIG_VALIDATE_ONLY)
            .expect("validate staged topology through NvAPI_DISP_SetDisplayConfig");
        let after = driver
            .get_display_config()
            .expect("re-read native topology");
        assert_eq!(
            before
                .paths
                .iter()
                .flat_map(|path| path.targets.iter().map(|target| target.display_id))
                .collect::<std::collections::BTreeSet<_>>(),
            after
                .paths
                .iter()
                .flat_map(|path| path.targets.iter().map(|target| target.display_id))
                .collect::<std::collections::BTreeSet<_>>()
        );
    }

    #[test]
    fn verified_platform_fallback_completes_cleanup_after_nvapi_topology_failure() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        let mut recovery = recovery_data(r"\\.\DISPLAY2".to_string(), &snapshot, 1120, 760, 120);
        let custom = custom_display(1120, 760, 120);
        recovery.ownership = TimingOwnership::SavedByUs;
        recovery.custom = Some(custom.clone());
        recovery.edid_write_stage = EdidWriteStage::Verified;
        recovery.intended_edid_sha256 = Some("0".repeat(64));
        driver.custom_displays.push(custom);
        driver.current_edid = Some(vec![0x55; 128]);
        driver.failures.push_back(Call::SetConfig(1680, 1050));
        let mut stage = CleanupStage::Pending;
        let mut fallback_error = None;

        restore_recovery_staged_with_topology_fallback(
            &mut driver,
            &recovery,
            None,
            |next| {
                stage = next;
                Ok(())
            },
            |error| {
                fallback_error = Some(error);
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(stage, CleanupStage::Complete);
        assert!(fallback_error
            .as_deref()
            .unwrap()
            .contains("injected failure"));
        assert_eq!(driver.current_edid, snapshot.original_edid);
        assert!(driver.custom_displays.is_empty());
    }

    #[test]
    fn every_cleanup_failure_stage_is_replayable() {
        for failure in [
            Call::SetEdid(0),
            Call::GetEdid,
            Call::SetEdid(128),
            Call::SetConfig(1680, 1050),
            Call::Revert,
            Call::Enum,
            Call::Delete,
        ] {
            let mut driver = MockDriver::new();
            let snapshot = snapshot_for(&mut driver);
            let custom = custom_display(3600, 2338, 60);
            driver.custom_displays.push(custom.clone());
            driver.current_edid = Some(vec![0x55; 128]);
            driver.failures.push_back(failure.clone());
            let active = ActiveExactMode {
                custom: Some(custom),
                ownership: TimingOwnership::SavedByUs,
                save_error: None,
                custom_snapshot_complete: true,
                pre_existing_custom: Vec::new(),
                edid_write_stage: EdidWriteStage::Verified,
                intended_edid_sha256: Some("0".repeat(64)),
            };
            let mut stage = CleanupStage::Pending;

            assert!(
                restore_exact_staged(&mut driver, &snapshot, Some(&active), stage, |next| {
                    stage = next;
                    Ok(())
                })
                .is_err(),
                "{failure:?} was not exercised"
            );
            restore_exact_staged(&mut driver, &snapshot, Some(&active), stage, |next| {
                stage = next;
                Ok(())
            })
            .unwrap_or_else(|error| panic!("{failure:?} was not replayable: {error}"));

            assert_eq!(stage, CleanupStage::Complete);
            assert_eq!(driver.config, snapshot.original_config);
            assert_eq!(driver.current_edid, snapshot.original_edid);
            assert!(driver.custom_displays.is_empty());
        }
    }

    #[test]
    fn saved_timing_cleanup_survives_later_topology_failure() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        let custom = custom_display(3600, 2338, 60);
        driver.custom_displays.push(custom.clone());
        let mut recovery = recovery_data(r"\\.\DISPLAY6".to_string(), &snapshot, 3600, 2338, 60);
        recovery.ownership = TimingOwnership::SavedByUs;
        recovery.custom = Some(custom);
        let journal_path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!(
                "nvapi-cleanup-replay-{}-{}.json",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        crate::recovery::remove(&journal_path).unwrap();
        let journal = crate::recovery::DisplayRecoveryJournal::new(
            r"\\.\DISPLAY6".to_string(),
            1680,
            1050,
            59,
            &[1],
            &[2],
            &[3],
            Some(recovery),
        )
        .with_stable_topology(crate::recovery::StableTopologySnapshot {
            paths: vec![crate::recovery::StableOutputIdentity {
                adapter_stable_id: "pci:test".to_string(),
                monitor_device_path: "monitor:test".to_string(),
                adapter_output_index: 0,
                output_technology: 4,
                connector_instance: 0,
                edid_manufacture_id: 1,
                edid_product_code_id: 2,
                edid_sha256: Some("0".repeat(64)),
                binding: crate::recovery::StableOutputBackend::Nvidia {
                    nvapi_display_id: driver.mapping.display_id,
                    nvapi_output_id: driver.mapping.output_id,
                    nvapi_head: driver.mapping.head,
                },
            }],
        });
        crate::recovery::write_atomic(&journal_path, &journal).unwrap();
        driver.calls.clear();
        let first = crate::recovery::read(&journal_path).unwrap().nvapi.unwrap();

        restore_recovery_staged(&mut driver, &first, |next| {
            crate::recovery::mark_nvapi_cleanup_stage(&journal_path, next)
        })
        .unwrap();

        assert_eq!(
            crate::recovery::nvapi_cleanup_stage(&journal_path).unwrap(),
            CleanupStage::Complete
        );
        assert_eq!(
            driver
                .calls
                .iter()
                .filter(|call| **call == Call::Delete)
                .count(),
            1
        );
        assert!(driver.custom_displays.is_empty());
        let mut topology_attempts = 0;
        let mut restore_windows_topology = || {
            topology_attempts += 1;
            if topology_attempts == 1 {
                Err("injected later Windows topology restore failure")
            } else {
                Ok(())
            }
        };
        assert!(restore_windows_topology().is_err());

        driver.calls.clear();
        let second = crate::recovery::read(&journal_path).unwrap().nvapi.unwrap();
        restore_recovery_staged(&mut driver, &second, |next| {
            crate::recovery::mark_nvapi_cleanup_stage(&journal_path, next)
        })
        .unwrap();

        let completed = crate::recovery::read(&journal_path).unwrap();
        assert_eq!(
            completed.nvapi.as_ref().unwrap().cleanup_stage,
            CleanupStage::Complete
        );
        assert_eq!(
            completed.nvapi.as_ref().unwrap().ownership,
            TimingOwnership::CleanupComplete
        );
        assert!(
            driver.calls.is_empty(),
            "retry after a later topology failure must not repeat NVAPI cleanup"
        );
        restore_windows_topology().unwrap();
        assert!(journal_path.exists());
        crate::recovery::remove(&journal_path).unwrap();
        crate::recovery::remove(&journal_path).unwrap();
        assert!(!journal_path.exists());
    }

    #[test]
    fn saved_timing_already_absent_is_already_clean() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        let custom = custom_display(3600, 2338, 60);
        let active = ActiveExactMode {
            custom: Some(custom),
            ownership: TimingOwnership::SavedByUs,
            save_error: None,
            custom_snapshot_complete: true,
            pre_existing_custom: Vec::new(),
            edid_write_stage: EdidWriteStage::Verified,
            intended_edid_sha256: Some("0".repeat(64)),
        };
        driver.calls.clear();

        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();

        assert!(driver.calls.contains(&Call::Enum));
        assert!(!driver.calls.contains(&Call::Delete));
    }

    #[test]
    fn every_apply_failure_stage_can_be_rolled_back_and_purged() {
        for failure in [
            Call::SetEdid(128),
            Call::Timing,
            Call::Try,
            Call::SetConfig(3600, 2338),
        ] {
            let mut driver = MockDriver::new();
            let original = driver.current_edid.clone();
            let snapshot = snapshot_for(&mut driver);
            driver.failures.push_back(failure.clone());

            let error = apply_for(&mut driver, &snapshot).unwrap_err();
            restore_exact(&mut driver, &snapshot, error.active.as_ref()).unwrap();

            assert!(
                driver.calls.contains(&Call::SetEdid(0)),
                "{failure:?} did not purge the injected EDID"
            );
            assert_eq!(
                driver.current_edid, original,
                "{failure:?} did not restore the effective EDID"
            );
            assert_eq!(
                driver.config, snapshot.original_config,
                "{failure:?} did not restore the original topology"
            );
        }
    }

    #[test]
    fn save_failure_keeps_a_temporary_trial_that_can_be_reverted() {
        let mut driver = MockDriver::new();
        let original = driver.current_edid.clone();
        let snapshot = snapshot_for(&mut driver);
        driver.failures.push_back(Call::Save);

        let active = apply_for(&mut driver, &snapshot).unwrap();

        assert_eq!(active.ownership, TimingOwnership::TrialAppliedByUs);
        assert!(active.save_error.as_deref().unwrap().contains("Save"));
        assert!(driver.calls.contains(&Call::SetConfig(3600, 2338)));
        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();
        assert!(driver.calls.contains(&Call::Revert));
        assert!(!driver.calls.contains(&Call::Delete));
        assert_eq!(driver.current_edid, original);
    }

    #[test]
    fn unavailable_pre_existing_snapshot_forces_trial_only_cleanup() {
        let mut driver = MockDriver::new();
        driver.failures.push_back(Call::Enum);
        let snapshot = snapshot_for(&mut driver);
        assert!(!snapshot.custom_snapshot_complete);

        let active = apply_for(&mut driver, &snapshot).unwrap();

        assert_eq!(active.ownership, TimingOwnership::TrialAppliedByUs);
        assert!(!active.custom_snapshot_complete);
        assert!(active
            .save_error
            .as_deref()
            .unwrap()
            .contains("persistent save disabled"));
        assert!(!driver.calls.contains(&Call::Save));
        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();
        assert!(driver.calls.contains(&Call::Revert));
        assert!(!driver.calls.contains(&Call::Delete));
    }

    #[test]
    fn pre_existing_identical_custom_timing_is_never_deleted() {
        let mut driver = MockDriver::new();
        let existing = custom_display(3600, 2338, 60);
        driver.custom_displays.push(existing.clone());
        let snapshot = snapshot_for(&mut driver);

        let active = apply_for(&mut driver, &snapshot).unwrap();

        assert_eq!(active.pre_existing_custom, vec![existing.clone()]);
        assert_eq!(active.ownership, TimingOwnership::TrialAppliedByUs);
        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();
        assert!(driver.calls.contains(&Call::Revert));
        assert!(!driver.calls.contains(&Call::Delete));
        assert_eq!(driver.custom_displays, vec![existing]);
    }

    #[test]
    fn saved_by_us_cleanup_deletes_only_the_new_custom_timing() {
        let mut driver = MockDriver::new();
        let unrelated = custom_display(1920, 1080, 60);
        driver.custom_displays.push(unrelated.clone());
        let snapshot = snapshot_for(&mut driver);
        let mut checkpoints = Vec::new();

        let active = apply_exact(
            &mut driver,
            &snapshot,
            &[0x55; 128],
            3600,
            2338,
            60,
            |active| {
                checkpoints.push((active.edid_write_stage, active.ownership));
                Ok(())
            },
        )
        .unwrap();

        assert_eq!(
            checkpoints,
            vec![
                (EdidWriteStage::Attempted, TimingOwnership::NotTried),
                (EdidWriteStage::Verified, TimingOwnership::NotTried),
                (
                    EdidWriteStage::Verified,
                    TimingOwnership::TrialAttemptedByUs
                ),
                (EdidWriteStage::Verified, TimingOwnership::TrialAppliedByUs),
                (EdidWriteStage::Verified, TimingOwnership::SaveAttemptedByUs),
                (EdidWriteStage::Verified, TimingOwnership::SavedByUs)
            ]
        );
        assert_eq!(active.ownership, TimingOwnership::SavedByUs);
        assert_eq!(driver.custom_displays.len(), 2);
        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();
        assert_eq!(driver.custom_displays, vec![unrelated]);
    }

    #[test]
    fn edid_attempt_is_checkpointed_before_write_and_no_original_edid_is_recoverable() {
        let mut driver = MockDriver::new();
        let mut snapshot = snapshot_for(&mut driver);
        snapshot.original_edid = None;
        driver.calls.clear();
        let error = apply_exact(
            &mut driver,
            &snapshot,
            &[0x55; 128],
            3600,
            2338,
            60,
            |active| {
                assert_eq!(active.edid_write_stage, EdidWriteStage::Attempted);
                assert!(active.intended_edid_sha256.is_some());
                Err("crash-before-edid-write".to_string())
            },
        )
        .unwrap_err();
        assert!(error.message.contains("crash-before-edid-write"));
        assert!(!driver
            .calls
            .iter()
            .any(|call| matches!(call, Call::SetEdid(_))));
    }

    #[test]
    fn crash_after_edid_readback_retains_verified_output_bound_stage() {
        let mut driver = MockDriver::new();
        let mut snapshot = snapshot_for(&mut driver);
        snapshot.original_edid = None;
        let error = apply_exact(
            &mut driver,
            &snapshot,
            &[0x66; 128],
            3600,
            2338,
            60,
            |active| {
                if active.edid_write_stage == EdidWriteStage::Verified {
                    Err("crash-after-edid-readback".to_string())
                } else {
                    Ok(())
                }
            },
        )
        .unwrap_err();
        let active = error.active.expect("verified EDID stage retained");
        assert_eq!(active.edid_write_stage, EdidWriteStage::Verified);
        assert_eq!(driver.current_edid, Some(vec![0x66; 128]));
    }

    #[test]
    fn failed_edid_call_retains_attempt_stage_and_is_recoverable_without_original_edid() {
        let mut driver = MockDriver::new();
        let mut snapshot = snapshot_for(&mut driver);
        snapshot.original_edid = None;
        driver.failures.push_back(Call::SetEdid(128));

        let error = apply_exact(&mut driver, &snapshot, &[0x77; 128], 3600, 2338, 60, |_| {
            Ok(())
        })
        .unwrap_err();
        let active = error.active.expect("attempted EDID stage retained");
        assert_eq!(active.edid_write_stage, EdidWriteStage::Attempted);

        restore_exact(&mut driver, &snapshot, Some(&active)).unwrap();
        assert_eq!(driver.current_edid, None);
    }

    #[test]
    fn armed_but_unmutated_recovery_never_purges_edid() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        let original = driver.current_edid.clone();

        restore_exact(&mut driver, &snapshot, None).unwrap();

        assert_eq!(driver.current_edid, original);
        assert!(!driver
            .calls
            .iter()
            .any(|call| matches!(call, Call::SetEdid(_))));
    }

    #[test]
    fn attempted_edid_without_timing_ownership_is_cleaned_during_recovery() {
        let mut driver = MockDriver::new();
        let mut snapshot = snapshot_for(&mut driver);
        snapshot.original_edid = None;
        driver.current_edid = Some(vec![0x88; 128]);
        let mut recovery = recovery_data(r"\\.\DISPLAY6".to_string(), &snapshot, 3600, 2338, 60);
        recovery.edid_write_stage = EdidWriteStage::Attempted;
        recovery.intended_edid_sha256 = Some("0".repeat(64));

        restore_recovery(&mut driver, &recovery).unwrap();

        assert_eq!(driver.current_edid, None);
        assert!(driver.calls.contains(&Call::SetEdid(0)));
    }

    #[test]
    fn full_original_edid_readback_mismatch_is_a_fatal_cleanup_error() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        driver.current_edid = Some(vec![0x55; 128]);
        driver.edid_writes_effective = false;
        let active = ActiveExactMode {
            custom: None,
            ownership: TimingOwnership::NotTried,
            save_error: None,
            custom_snapshot_complete: true,
            pre_existing_custom: Vec::new(),
            edid_write_stage: EdidWriteStage::Attempted,
            intended_edid_sha256: Some("0".repeat(64)),
        };

        let error = restore_exact(&mut driver, &snapshot, Some(&active)).unwrap_err();

        assert!(error.contains("verify complete EDID after restore"));
        assert_eq!(driver.current_edid, Some(vec![0x55; 128]));
    }

    #[test]
    fn persisted_recovery_deletes_the_recorded_saved_timing_without_recalculation() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        let created = custom_display(3600, 2338, 60);
        driver.custom_displays.push(created.clone());
        let mut recovery = recovery_data(r"\\.\DISPLAY6".to_string(), &snapshot, 3600, 2338, 60);
        recovery.ownership = TimingOwnership::SavedByUs;
        recovery.custom = Some(created);
        recovery.custom_snapshot_complete = true;

        restore_recovery(&mut driver, &recovery).unwrap();

        assert!(!driver.calls.contains(&Call::Timing));
        assert!(driver.calls.contains(&Call::Revert));
        assert!(driver.calls.contains(&Call::Delete));
        assert!(driver.custom_displays.is_empty());
    }

    #[test]
    fn recovery_handles_every_persisted_custom_mode_crash_stage() {
        for (stage, expect_revert, expect_delete) in [
            (TimingOwnership::NotTried, false, false),
            (TimingOwnership::TrialAttemptedByUs, true, false),
            (TimingOwnership::TrialAppliedByUs, true, false),
            (TimingOwnership::SaveAttemptedByUs, true, true),
            (TimingOwnership::SavedByUs, true, true),
            (TimingOwnership::CleanupComplete, false, false),
        ] {
            let mut driver = MockDriver::new();
            let snapshot = snapshot_for(&mut driver);
            let custom = custom_display(3600, 2338, 60);
            if matches!(
                stage,
                TimingOwnership::SaveAttemptedByUs | TimingOwnership::SavedByUs
            ) {
                driver.custom_displays.push(custom.clone());
            }
            driver.calls.clear();
            let mut recovery =
                recovery_data(r"\\.\DISPLAY6".to_string(), &snapshot, 3600, 2338, 60);
            recovery.ownership = stage;
            recovery.custom_snapshot_complete = true;
            recovery.custom = (stage != TimingOwnership::NotTried
                && stage != TimingOwnership::CleanupComplete)
                .then_some(custom);

            restore_recovery(&mut driver, &recovery).unwrap();

            assert_eq!(
                driver.calls.contains(&Call::Revert),
                expect_revert,
                "wrong trial cleanup at {stage:?}"
            );
            assert_eq!(
                driver.calls.contains(&Call::Delete),
                expect_delete,
                "wrong saved timing cleanup at {stage:?}"
            );
            if stage == TimingOwnership::CleanupComplete {
                assert!(
                    driver.calls.is_empty(),
                    "completed cleanup must be a recovery no-op"
                );
            }
        }
    }

    #[test]
    fn save_attempt_crash_never_deletes_identical_pre_existing_timing() {
        let mut driver = MockDriver::new();
        let existing = custom_display(3600, 2338, 60);
        driver.custom_displays.push(existing.clone());
        let snapshot = snapshot_for(&mut driver);
        driver.calls.clear();
        let mut recovery = recovery_data(r"\\.\DISPLAY6".to_string(), &snapshot, 3600, 2338, 60);
        recovery.ownership = TimingOwnership::SaveAttemptedByUs;
        recovery.custom = Some(existing.clone());

        restore_recovery(&mut driver, &recovery).unwrap();

        assert!(driver.calls.contains(&Call::Revert));
        assert!(!driver.calls.contains(&Call::Delete));
        assert_eq!(driver.custom_displays, vec![existing]);
    }

    #[test]
    fn recovery_before_try_purges_edid_without_revert_or_delete() {
        let mut driver = MockDriver::new();
        let snapshot = snapshot_for(&mut driver);
        driver.current_edid = Some(vec![0x55; 128]);
        let mut recovery = recovery_data(r"\\.\DISPLAY6".to_string(), &snapshot, 3600, 2338, 60);
        recovery.edid_write_stage = EdidWriteStage::Attempted;
        recovery.intended_edid_sha256 = Some("0".repeat(64));

        restore_recovery(&mut driver, &recovery).unwrap();

        assert!(driver.calls.contains(&Call::SetConfig(1680, 1050)));
        assert!(driver.calls.contains(&Call::SetEdid(0)));
        assert!(!driver.calls.contains(&Call::Revert));
        assert!(!driver.calls.contains(&Call::Delete));
        assert_eq!(driver.current_edid, snapshot.original_edid);
    }

    #[test]
    fn topology_mapping_rejects_missing_or_ambiguous_display_paths() {
        let mut config = MockDriver::new().config;
        let error = config.set_resolution(0xffff, 1920, 1080).unwrap_err();
        assert!(error.contains("0 paths"));
        config.paths.push(config.paths[0].clone());
        let error = config.set_resolution(0x1234, 1920, 1080).unwrap_err();
        assert!(error.contains("2 paths"));
    }
}
