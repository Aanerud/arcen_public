use std::collections::HashMap;

use serde::Serialize;
use windows::core::{Interface, PCSTR, PCWSTR};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_ADAPTER_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ALL_PATHS, QDC_VIRTUAL_MODE_AWARE,
    QUERY_DISPLAY_CONFIG_FLAGS,
};
use windows::Win32::Foundation::{
    FreeLibrary, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, HANDLE, HMODULE, LUID, WIN32_ERROR,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11VideoDevice,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplaySettingsExW, GetMonitorInfoW, DEVMODEW, DISPLAYCONFIG_PATH_ACTIVE,
    ENUM_CURRENT_SETTINGS, ENUM_DISPLAY_SETTINGS_FLAGS, ENUM_DISPLAY_SETTINGS_MODE, MONITORINFO,
};
use windows::Win32::System::LibraryLoader::{
    GetProcAddress, LoadLibraryExW, LOAD_LIBRARY_SEARCH_SYSTEM32,
};
use windows::Win32::UI::WindowsAndMessaging::MONITORINFOF_PRIMARY;

const MAX_ENUMERATED_MODES: u32 = 4096;
const INVENTORY_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize)]
pub struct HostCapabilityReport {
    pub schema_version: u32,
    pub topology_error: Option<String>,
    pub nvenc_runtime_dll: bool,
    pub openh264_compiled: bool,
    pub vmware_resolution_tool: Option<String>,
    pub adapters: Vec<AdapterCapability>,
    pub recommendation: Option<AdapterRecommendation>,
    pub hypervisor_present: Option<bool>,
    pub firmware_sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdapterCapability {
    pub stable_id: String,
    pub device_path: Option<String>,
    pub dxgi_index: u32,
    pub description: String,
    pub kind: AdapterKind,
    pub vendor_id: u32,
    pub device_id: u32,
    pub subsystem_id: u32,
    pub revision: u32,
    pub dedicated_video_memory_bytes: u64,
    pub shared_system_memory_bytes: u64,
    pub session_luid: String,
    pub software: bool,
    pub d3d11_feature_level: Option<String>,
    pub d3d11_video_device: bool,
    pub direct_nvenc_candidate: bool,
    pub outputs: Vec<OutputCapability>,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AdapterKind {
    Hardware,
    MicrosoftBasic,
    Software,
    RemoteOrIndirect,
    Paravirtualized,
}

#[derive(Clone, Debug, Serialize)]
pub struct OutputCapability {
    pub adapter_output_index: u32,
    pub attached_global_index: Option<u32>,
    pub device_name: String,
    pub attached_to_desktop: bool,
    pub primary: bool,
    pub monitor_handle: String,
    pub desktop_rect: RectCapability,
    pub current_mode: Option<ModeCapability>,
    pub supported_modes: Vec<ModeCapability>,
    pub target_available: Option<bool>,
    pub ccd_active: Option<bool>,
    pub target_id: Option<u32>,
    pub monitor_device_path: Option<String>,
    pub monitor_friendly_name: Option<String>,
    pub output_technology: Option<i32>,
    pub edid_manufacture_id: Option<u16>,
    pub edid_product_code_id: Option<u16>,
    pub connector_instance: Option<u32>,
    pub deskside_identity_sha256: Option<String>,
    pub deskside_edid_sha256: Option<String>,
    pub deskside_capture_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct RectCapability {
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct ModeCapability {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct AdapterRecommendation {
    pub stable_id: String,
    pub description: String,
    pub adapter_output_index: u32,
    pub device_name: String,
    pub reason: String,
}

#[derive(Clone)]
struct CcdTarget {
    active: bool,
    target_available: bool,
    target_id: u32,
    monitor_device_path: Option<String>,
    monitor_friendly_name: Option<String>,
    output_technology: i32,
    edid_manufacture_id: u16,
    edid_product_code_id: u16,
    connector_instance: u32,
}

impl HostCapabilityReport {
    pub fn human_summary(&self) -> String {
        let mut output = format!(
            "Arcen Windows host capabilities (schema {})\nNVENC runtime DLL: {}\nOpenH264 software encoder: {}\nVMwareResolutionSet: {}\n",
            self.schema_version,
            availability(self.nvenc_runtime_dll),
            availability(self.openh264_compiled),
            self.vmware_resolution_tool
                .as_deref()
                .unwrap_or("not found")
        );
        if let Some(error) = self.topology_error.as_deref() {
            output.push_str(&format!("CCD topology: unavailable ({error})\n"));
        }
        for adapter in &self.adapters {
            output.push_str(&format!(
                "\n[{}] {} kind={:?} pci={:04x}:{:04x} vram={} MiB d3d11={} video={} nvenc_candidate={}\n  id={}\n",
                adapter.dxgi_index,
                adapter.description,
                adapter.kind,
                adapter.vendor_id,
                adapter.device_id,
                adapter.dedicated_video_memory_bytes / (1024 * 1024),
                adapter.d3d11_feature_level.as_deref().unwrap_or("unavailable"),
                availability(adapter.d3d11_video_device),
                availability(adapter.direct_nvenc_candidate),
                adapter.stable_id,
            ));
            if adapter.outputs.is_empty() {
                output.push_str("  outputs: none\n");
            }
            for display in &adapter.outputs {
                output.push_str(&format!(
                    "  output {} global={:?} device={} attached={} target_available={:?} primary={} rect={}x{}@{},{}\n",
                    display.adapter_output_index,
                    display.attached_global_index,
                    display.device_name,
                    display.attached_to_desktop,
                    display.target_available,
                    display.primary,
                    display.desktop_rect.width,
                    display.desktop_rect.height,
                    display.desktop_rect.left,
                    display.desktop_rect.top,
                ));
            }
        }
        match &self.recommendation {
            Some(recommendation) => output.push_str(&format!(
                "\nRecommended: {} output {} ({}) - {}\n",
                recommendation.description,
                recommendation.adapter_output_index,
                recommendation.device_name,
                recommendation.reason
            )),
            None => output.push_str("\nRecommended: none (no healthy attached output)\n"),
        }
        output
    }
}

pub fn probe() -> Result<HostCapabilityReport, String> {
    let (ccd, topology_error) = match query_ccd_targets() {
        Ok(value) => (value, None),
        Err(error) => (HashMap::new(), Some(error)),
    };
    let nvenc_runtime_dll = nvenc_runtime_dll();
    let mut adapters = enumerate_adapters(&ccd, nvenc_runtime_dll)?;
    assign_stable_ordinals(&mut adapters);
    let recommendation = recommend(&adapters);
    let hypervisor_present = crate::deskside::cpuid_hypervisor_present();
    let firmware_sha256 = crate::deskside::positive_firmware_fingerprint().ok();
    Ok(HostCapabilityReport {
        schema_version: INVENTORY_VERSION,
        topology_error,
        nvenc_runtime_dll,
        openh264_compiled: arcen_capenc::compiled_backend_features().software_h264,
        vmware_resolution_tool: vmware_resolution_tool()
            .map(|path| path.to_string_lossy().into_owned()),
        adapters,
        recommendation,
        hypervisor_present,
        firmware_sha256,
    })
}

fn enumerate_adapters(
    ccd: &HashMap<String, CcdTarget>,
    nvenc_runtime_dll: bool,
) -> Result<Vec<AdapterCapability>, String> {
    // SAFETY: windows-rs owns and releases all returned COM interfaces.
    unsafe {
        let factory: IDXGIFactory1 =
            CreateDXGIFactory1().map_err(|error| format!("CreateDXGIFactory1: {error}"))?;
        let mut adapters = Vec::new();
        let mut attached_global_index = 0u32;
        for adapter_index in 0u32.. {
            let adapter = match factory.EnumAdapters1(adapter_index) {
                Ok(adapter) => adapter,
                Err(_) => break,
            };
            let desc = adapter
                .GetDesc1()
                .map_err(|error| format!("IDXGIAdapter1::GetDesc1: {error}"))?;
            let description = utf16(&desc.Description);
            let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
            let luid = desc.AdapterLuid;
            let device_path = adapter_device_path(luid)
                .ok()
                .filter(|path| !path.is_empty());
            let adapter_stable_id = stable_id(
                device_path.as_deref(),
                desc.VendorId,
                desc.DeviceId,
                desc.SubSysId,
                desc.Revision,
                0,
            );
            let session_luid = format!("{:08x}:{:08x}", luid.HighPart as u32, luid.LowPart);
            let (feature_level, video_device) = d3d11_capability(&adapter);
            let mut outputs = Vec::new();
            for output_index in 0u32.. {
                let output = match adapter.EnumOutputs(output_index) {
                    Ok(output) => output,
                    Err(_) => break,
                };
                let output_desc = match output.GetDesc() {
                    Ok(desc) => desc,
                    Err(_) => continue,
                };
                let device_name = utf16(&output_desc.DeviceName);
                let attached = output_desc.AttachedToDesktop.as_bool();
                let global_index = attached.then(|| {
                    let index = attached_global_index;
                    attached_global_index += 1;
                    index
                });
                let rect = output_desc.DesktopCoordinates;
                let primary = monitor_is_primary(output_desc.Monitor);
                let ccd_target = ccd.get(&device_name.to_ascii_uppercase());
                outputs.push(OutputCapability {
                    adapter_output_index: output_index,
                    attached_global_index: global_index,
                    device_name: device_name.clone(),
                    attached_to_desktop: attached,
                    primary,
                    monitor_handle: format!("0x{:x}", output_desc.Monitor.0 as usize),
                    desktop_rect: RectCapability {
                        left: rect.left,
                        top: rect.top,
                        width: rect.right.saturating_sub(rect.left),
                        height: rect.bottom.saturating_sub(rect.top),
                    },
                    current_mode: if attached {
                        current_mode(&device_name).ok()
                    } else {
                        None
                    },
                    supported_modes: if attached {
                        supported_modes(&device_name).unwrap_or_default()
                    } else {
                        Vec::new()
                    },
                    target_available: ccd_target.map(|target| target.target_available),
                    ccd_active: ccd_target.map(|target| target.active),
                    target_id: ccd_target.map(|target| target.target_id),
                    monitor_device_path: ccd_target
                        .and_then(|target| target.monitor_device_path.clone()),
                    monitor_friendly_name: ccd_target
                        .and_then(|target| target.monitor_friendly_name.clone()),
                    output_technology: ccd_target.map(|target| target.output_technology),
                    edid_manufacture_id: ccd_target.map(|target| target.edid_manufacture_id),
                    edid_product_code_id: ccd_target.map(|target| target.edid_product_code_id),
                    connector_instance: ccd_target.map(|target| target.connector_instance),
                    deskside_identity_sha256: ccd_target
                        .and_then(|target| target.monitor_device_path.as_deref())
                        .map(crate::deskside::normalized_identity_hash),
                    deskside_edid_sha256: ccd_target.and_then(|target| {
                        (target.edid_manufacture_id != 0 && target.edid_product_code_id != 0).then(
                            || {
                                crate::deskside::edid_tuple_hash(
                                    target.edid_manufacture_id,
                                    target.edid_product_code_id,
                                    target.connector_instance,
                                )
                            },
                        )
                    }),
                    deskside_capture_sha256: ccd_target.and_then(|target| {
                        crate::deskside::capture_pin_hash(
                            &adapter_stable_id,
                            output_index,
                            &device_name,
                            target.monitor_device_path.as_deref()?,
                            target.output_technology,
                        )
                        .ok()
                    }),
                });
            }
            let kind = classify_adapter(desc.VendorId, software, &description, &outputs);
            adapters.push(AdapterCapability {
                stable_id: adapter_stable_id,
                device_path,
                dxgi_index: adapter_index,
                description,
                kind,
                vendor_id: desc.VendorId,
                device_id: desc.DeviceId,
                subsystem_id: desc.SubSysId,
                revision: desc.Revision,
                dedicated_video_memory_bytes: desc.DedicatedVideoMemory as u64,
                shared_system_memory_bytes: desc.SharedSystemMemory as u64,
                session_luid,
                software,
                d3d11_feature_level: feature_level,
                d3d11_video_device: video_device,
                direct_nvenc_candidate: desc.VendorId == 0x10de && nvenc_runtime_dll,
                outputs,
            });
        }
        Ok(adapters)
    }
}

pub(crate) fn classify_adapter(
    vendor_id: u32,
    software: bool,
    description: &str,
    outputs: &[OutputCapability],
) -> AdapterKind {
    if software {
        AdapterKind::Software
    } else if vendor_id == 0x1414 {
        AdapterKind::MicrosoftBasic
    } else if matches!(vendor_id, 0x15ad | 0x1af4 | 0x80ee | 0x1234) {
        AdapterKind::Paravirtualized
    } else if vendor_id == 0
        || description.to_ascii_lowercase().contains("remote")
        || (!outputs.is_empty()
            && outputs
                .iter()
                .all(|output| matches!(output.output_technology, Some(15 | 16 | 17))))
    {
        AdapterKind::RemoteOrIndirect
    } else {
        AdapterKind::Hardware
    }
}

fn recommend(adapters: &[AdapterCapability]) -> Option<AdapterRecommendation> {
    let mut candidates = adapters
        .iter()
        .filter(|adapter| {
            matches!(
                adapter.kind,
                AdapterKind::Hardware | AdapterKind::Paravirtualized
            ) && adapter.d3d11_feature_level.is_some()
        })
        .flat_map(|adapter| {
            adapter
                .outputs
                .iter()
                .filter(|output| {
                    output.attached_to_desktop && output.target_available != Some(false)
                })
                .map(move |output| (adapter, output))
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(adapter, output)| {
        (
            !adapter.direct_nvenc_candidate,
            output.primary,
            adapter.dedicated_video_memory_bytes,
            adapter.dxgi_index,
            output.adapter_output_index,
        )
    });
    candidates
        .first()
        .map(|(adapter, output)| AdapterRecommendation {
            stable_id: adapter.stable_id.clone(),
            description: adapter.description.clone(),
            adapter_output_index: output.adapter_output_index,
            device_name: output.device_name.clone(),
            reason: if adapter.direct_nvenc_candidate {
                "same-adapter direct NVENC candidate; non-primary/lower-tier adapters are preferred"
                    .to_string()
            } else {
                "healthy attached output; no usable same-adapter hardware encoder was proven"
                    .to_string()
            },
        })
}

fn assign_stable_ordinals(adapters: &mut [AdapterCapability]) {
    let mut seen = HashMap::<String, u32>::new();
    for adapter in adapters {
        let base = stable_id(
            adapter.device_path.as_deref(),
            adapter.vendor_id,
            adapter.device_id,
            adapter.subsystem_id,
            adapter.revision,
            0,
        );
        let ordinal = seen.entry(base).or_default();
        adapter.stable_id = stable_id(
            adapter.device_path.as_deref(),
            adapter.vendor_id,
            adapter.device_id,
            adapter.subsystem_id,
            adapter.revision,
            *ordinal,
        );
        *ordinal += 1;
    }
}

fn stable_id(
    device_path: Option<&str>,
    vendor: u32,
    device: u32,
    subsystem: u32,
    revision: u32,
    ordinal: u32,
) -> String {
    device_path.map_or_else(
        || {
            format!(
                "pci:ven_{vendor:04x}&dev_{device:04x}&subsys_{subsystem:08x}&rev_{revision:02x}#{ordinal}"
            )
        },
        |path| format!("{}#{ordinal}", path.to_ascii_lowercase()),
    )
}

fn d3d11_capability(
    adapter: &windows::Win32::Graphics::Dxgi::IDXGIAdapter1,
) -> (Option<String>, bool) {
    // SAFETY: the adapter COM interface remains alive for the synchronous call.
    unsafe {
        let Ok(adapter): Result<IDXGIAdapter, _> = adapter.cast() else {
            return (None, false);
        };
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let mut level = D3D_FEATURE_LEVEL(0);
        if D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut level),
            Some(&mut context),
        )
        .is_err()
        {
            return (None, false);
        }
        let video = device
            .as_ref()
            .is_some_and(|device| device.cast::<ID3D11VideoDevice>().is_ok());
        (Some(format!("0x{:04x}", level.0)), video)
    }
}

fn query_ccd_targets() -> Result<HashMap<String, CcdTarget>, String> {
    let flags = QUERY_DISPLAY_CONFIG_FLAGS(QDC_ALL_PATHS.0 | QDC_VIRTUAL_MODE_AWARE.0);
    for _ in 0..4 {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        // SAFETY: valid writable count pointers are supplied.
        let sizes = unsafe { GetDisplayConfigBufferSizes(flags, &mut path_count, &mut mode_count) };
        win32_ok(sizes, "GetDisplayConfigBufferSizes")?;
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![
            windows::Win32::Devices::Display::DISPLAYCONFIG_MODE_INFO::default();
            mode_count as usize
        ];
        // SAFETY: buffers are initialized to the immediately returned sizes.
        let result = unsafe {
            QueryDisplayConfig(
                flags,
                &mut path_count,
                paths.as_mut_ptr(),
                &mut mode_count,
                modes.as_mut_ptr(),
                None,
            )
        };
        if result == ERROR_INSUFFICIENT_BUFFER {
            continue;
        }
        win32_ok(result, "QueryDisplayConfig")?;
        paths.truncate(path_count as usize);
        let mut values = HashMap::new();
        for path in paths {
            let source = source_name(path.sourceInfo.adapterId, path.sourceInfo.id)?;
            let target = target_name(path.targetInfo.adapterId, path.targetInfo.id).ok();
            let key = source.to_ascii_uppercase();
            let candidate = CcdTarget {
                active: path.flags & DISPLAYCONFIG_PATH_ACTIVE != 0,
                target_available: path.targetInfo.targetAvailable.as_bool(),
                target_id: path.targetInfo.id,
                monitor_device_path: target
                    .as_ref()
                    .map(|target| utf16(&target.monitorDevicePath))
                    .filter(|value| !value.is_empty()),
                monitor_friendly_name: target
                    .as_ref()
                    .map(|target| utf16(&target.monitorFriendlyDeviceName))
                    .filter(|value| !value.is_empty()),
                output_technology: path.targetInfo.outputTechnology.0,
                edid_manufacture_id: target.as_ref().map_or(0, |target| target.edidManufactureId),
                edid_product_code_id: target.as_ref().map_or(0, |target| target.edidProductCodeId),
                connector_instance: target.as_ref().map_or(0, |target| target.connectorInstance),
            };
            match values.entry(key) {
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(candidate);
                }

                std::collections::hash_map::Entry::Occupied(mut entry)
                    if candidate.active && !entry.get().active =>
                {
                    entry.insert(candidate);
                }
                std::collections::hash_map::Entry::Occupied(_) => {}
            }
        }
        return Ok(values);
    }
    Err("display topology changed repeatedly while probing".to_string())
}

/// Probes the interactive Windows desktop and converts its attached DXGI/CCD
/// outputs into the stable inventory used by the multi-monitor planner and
/// capenc re-resolution path.
///
/// Adapter eligibility is *capture* capability plus the operator allowlist —
/// deliberately not NVENC candidacy. This host's capture path is DXGI/WGC on a
/// D3D11 device, and its encode path is either direct NVENC or source-built
/// OpenH264 (`capenc::EncoderSelection::resolve_auto`), so requiring an
/// NVENC-capable adapter here would exclude every software
/// host from multi-monitor even when every gate downstream can serve it.
/// Which encoder backend each planned region actually gets is decided later,
/// by `encoder_admission::plan_encoder_sets` and the measured runtime
/// admission, not by this inventory.
pub fn physical_output_inventory(
    allowed_adapters: &[String],
) -> Result<crate::multi_monitor_topology::PhysicalOutputInventory, String> {
    use crate::multi_monitor_topology::{
        AvailableOutput, OutputMode, OutputModeCapability, PhysicalOutputInventory,
    };
    use arcen_media::Rotation;

    let report = probe()?;
    if let Some(error) = report.topology_error {
        return Err(format!("CCD topology probe failed: {error}"));
    }
    let mut outputs = Vec::new();
    for adapter in report.adapters {
        if adapter.d3d11_feature_level.is_none()
            || !allowed_adapters
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(&adapter.description))
        {
            continue;
        }
        let adapter_luid = parse_session_luid(&adapter.session_luid)?;
        for output in adapter
            .outputs
            .into_iter()
            .filter(|output| output.attached_to_desktop)
        {
            let target_id = output
                .target_id
                .ok_or_else(|| format!("{} has no CCD target id", output.device_name))?;
            let global_index = output
                .attached_global_index
                .ok_or_else(|| format!("{} has no attached-output ordinal", output.device_name))?;
            let current = output
                .current_mode
                .ok_or_else(|| format!("{} has no current display mode", output.device_name))?;
            if output.desktop_rect.width <= 0 || output.desktop_rect.height <= 0 {
                return Err(format!(
                    "{} has invalid desktop rectangle {}x{}",
                    output.device_name, output.desktop_rect.width, output.desktop_rect.height
                ));
            }
            let fixed_modes = output
                .supported_modes
                .iter()
                .map(|mode| OutputMode {
                    width: mode.width,
                    height: mode.height,
                    refresh_hz: mode.refresh_hz.max(1),
                })
                .collect::<Vec<_>>();
            let mode_capability = if adapter.vendor_id == 0x10de {
                OutputModeCapability::CustomTimingCapable {
                    min_width: 320,
                    max_width: 8_192,
                    min_height: 240,
                    max_height: 8_192,
                    min_refresh_hz: 24,
                    max_refresh_hz: 240,
                }
            } else if fixed_modes.is_empty() {
                OutputModeCapability::FixedModes(vec![OutputMode {
                    width: current.width,
                    height: current.height,
                    refresh_hz: current.refresh_hz.max(1),
                }])
            } else {
                OutputModeCapability::FixedModes(fixed_modes)
            };
            outputs.push(AvailableOutput {
                adapter_luid,
                target_id,
                adapter_output_index: output.adapter_output_index,
                adapter_name: adapter.description.clone(),
                global_index,
                device_name: output.device_name,
                mode_capability,
                supported_rotations: vec![
                    Rotation::Degrees0,
                    Rotation::Degrees90,
                    Rotation::Degrees180,
                    Rotation::Degrees270,
                ],
                current_x: output.desktop_rect.left,
                current_y: output.desktop_rect.top,
                current_width: u32::try_from(output.desktop_rect.width)
                    .map_err(|_| "desktop width does not fit u32".to_string())?,
                current_height: u32::try_from(output.desktop_rect.height)
                    .map_err(|_| "desktop height does not fit u32".to_string())?,
                current_refresh_hz: current.refresh_hz.max(1),
                primary: output.primary,
            });
        }
    }
    if outputs.is_empty() {
        return Err(format!(
            "no attached capture-capable outputs matched allowed adapters {allowed_adapters:?}"
        ));
    }
    outputs.sort_by_key(|output| (!output.primary, output.global_index));
    PhysicalOutputInventory::new(outputs).map_err(|error| error.to_string())
}

fn parse_session_luid(value: &str) -> Result<crate::nvapi::AdapterLuid, String> {
    let (high, low) = value
        .split_once(':')
        .ok_or_else(|| format!("invalid DXGI adapter LUID {value:?}"))?;
    let high = u32::from_str_radix(high, 16)
        .map_err(|_| format!("invalid DXGI adapter LUID high part {high:?}"))?;
    let low = u32::from_str_radix(low, 16)
        .map_err(|_| format!("invalid DXGI adapter LUID low part {low:?}"))?;
    Ok(crate::nvapi::AdapterLuid {
        low_part: low,
        high_part: high as i32,
    })
}

fn source_name(adapter: LUID, id: u32) -> Result<String, String> {
    let mut value = DISPLAYCONFIG_SOURCE_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
            adapterId: adapter,
            id,
        },
        ..DISPLAYCONFIG_SOURCE_DEVICE_NAME::default()
    };
    // SAFETY: header is the first member of the correctly sized packet.
    win32_ok(
        WIN32_ERROR(unsafe { DisplayConfigGetDeviceInfo(&mut value.header) } as u32),
        "DisplayConfigGetDeviceInfo(source)",
    )?;
    Ok(utf16(&value.viewGdiDeviceName))
}

fn target_name(adapter: LUID, id: u32) -> Result<DISPLAYCONFIG_TARGET_DEVICE_NAME, String> {
    let mut value = DISPLAYCONFIG_TARGET_DEVICE_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
            adapterId: adapter,
            id,
        },
        ..DISPLAYCONFIG_TARGET_DEVICE_NAME::default()
    };
    // SAFETY: header is the first member of the correctly sized packet.
    win32_ok(
        WIN32_ERROR(unsafe { DisplayConfigGetDeviceInfo(&mut value.header) } as u32),
        "DisplayConfigGetDeviceInfo(target)",
    )?;
    Ok(value)
}

fn adapter_device_path(adapter: LUID) -> Result<String, String> {
    let mut value = DISPLAYCONFIG_ADAPTER_NAME {
        header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
            r#type: DISPLAYCONFIG_DEVICE_INFO_GET_ADAPTER_NAME,
            size: std::mem::size_of::<DISPLAYCONFIG_ADAPTER_NAME>() as u32,
            adapterId: adapter,
            id: 0,
        },
        ..DISPLAYCONFIG_ADAPTER_NAME::default()
    };
    // SAFETY: header is the first member of the correctly sized packet.
    win32_ok(
        WIN32_ERROR(unsafe { DisplayConfigGetDeviceInfo(&mut value.header) } as u32),
        "DisplayConfigGetDeviceInfo(adapter)",
    )?;
    Ok(utf16(&value.adapterDevicePath))
}

fn current_mode(device_name: &str) -> Result<ModeCapability, String> {
    enumerate_mode(device_name, ENUM_CURRENT_SETTINGS)
}

fn supported_modes(device_name: &str) -> Result<Vec<ModeCapability>, String> {
    let mut values = Vec::new();
    for index in 0..MAX_ENUMERATED_MODES {
        match enumerate_mode(device_name, ENUM_DISPLAY_SETTINGS_MODE(index)) {
            Ok(mode) => values.push(mode),
            Err(_) => break,
        }
    }
    values.sort();
    values.dedup();
    Ok(values)
}

fn enumerate_mode(
    device_name: &str,
    index: ENUM_DISPLAY_SETTINGS_MODE,
) -> Result<ModeCapability, String> {
    let device = wide(device_name);
    let mut mode = DEVMODEW {
        dmSize: std::mem::size_of::<DEVMODEW>() as u16,
        ..DEVMODEW::default()
    };
    // SAFETY: device is null-terminated and mode is writable.
    let found = unsafe {
        EnumDisplaySettingsExW(
            PCWSTR(device.as_ptr()),
            index,
            &mut mode,
            ENUM_DISPLAY_SETTINGS_FLAGS(0),
        )
    };
    if !found.as_bool() {
        return Err(format!("EnumDisplaySettingsExW failed for {device_name}"));
    }
    Ok(ModeCapability {
        width: mode.dmPelsWidth,
        height: mode.dmPelsHeight,
        refresh_hz: mode.dmDisplayFrequency,
    })
}

fn monitor_is_primary(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> bool {
    if monitor.0.is_null() {
        return false;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..MONITORINFO::default()
    };
    // SAFETY: info is correctly sized and writable.
    let found = unsafe { GetMonitorInfoW(monitor, &mut info).as_bool() };
    found && info.dwFlags & MONITORINFOF_PRIMARY != 0
}

pub fn nvenc_runtime_dll() -> bool {
    // SAFETY: the null-terminated fixed name is loaded from SYSTEM32 only and
    // the resulting handle is released below.
    unsafe {
        let name = wide("nvEncodeAPI64.dll");
        let Ok(module) = LoadLibraryExW(
            PCWSTR(name.as_ptr()),
            HANDLE::default(),
            LOAD_LIBRARY_SEARCH_SYSTEM32,
        ) else {
            return false;
        };
        let found =
            GetProcAddress(module, PCSTR(b"NvEncodeAPICreateInstance\0".as_ptr())).is_some();
        let _ = FreeLibrary(module);
        found
    }
}

fn vmware_resolution_tool() -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(std::env::var_os("ProgramFiles")?)
        .join("VMware")
        .join("VMware Tools")
        .join("VMwareResolutionSet.exe");
    path.is_file().then_some(path)
}

fn system_file(name: &str) -> Option<std::path::PathBuf> {
    let path = std::path::PathBuf::from(std::env::var_os("SystemRoot")?)
        .join("System32")
        .join(name);
    path.is_file().then_some(path)
}

fn win32_ok(code: WIN32_ERROR, operation: &str) -> Result<(), String> {
    if code == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("{operation} returned Win32 error {}", code.0))
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn utf16(value: &[u16]) -> String {
    let end = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    String::from_utf16_lossy(&value[..end])
}

fn availability(value: bool) -> &'static str {
    if value {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_id_prefers_adapter_device_path() {
        assert_eq!(
            stable_id(Some(r"\\?\PCI#VEN_10DE&DEV_1234"), 0x10de, 0x1234, 0, 1, 0),
            r"\\?\pci#ven_10de&dev_1234#0"
        );
    }

    #[test]
    fn recommendation_prefers_secondary_lower_vram_nvenc_adapter() {
        let output = |primary: bool, device: &str| OutputCapability {
            adapter_output_index: 0,
            attached_global_index: Some(0),
            device_name: device.to_string(),
            attached_to_desktop: true,
            primary,
            monitor_handle: "0x1".to_string(),
            desktop_rect: RectCapability {
                left: 0,
                top: 0,
                width: 1920,
                height: 1080,
            },
            current_mode: None,
            supported_modes: Vec::new(),
            target_available: Some(true),
            ccd_active: Some(true),
            target_id: Some(0),
            monitor_device_path: None,
            monitor_friendly_name: None,
            output_technology: None,
            edid_manufacture_id: None,
            edid_product_code_id: None,
            connector_instance: None,
            deskside_identity_sha256: None,
            deskside_edid_sha256: None,
            deskside_capture_sha256: None,
        };
        let adapter = |id: &str, primary: bool, vram: u64| AdapterCapability {
            stable_id: id.to_string(),
            device_path: None,
            dxgi_index: if primary { 0 } else { 1 },
            description: id.to_string(),
            kind: AdapterKind::Hardware,
            vendor_id: 0x10de,
            device_id: 1,
            subsystem_id: 1,
            revision: 1,
            dedicated_video_memory_bytes: vram,
            shared_system_memory_bytes: 0,
            session_luid: "0:0".to_string(),
            software: false,
            d3d11_feature_level: Some("0xb000".to_string()),
            d3d11_video_device: true,
            direct_nvenc_candidate: true,
            outputs: vec![output(primary, id)],
        };
        let adapters = [
            adapter("vfx", true, 48 * 1024 * 1024 * 1024),
            adapter("remote", false, 8 * 1024 * 1024 * 1024),
        ];

        assert_eq!(recommend(&adapters).unwrap().stable_id, "remote");
    }
}
