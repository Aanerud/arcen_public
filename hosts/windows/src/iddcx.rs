//! Strict, default-off host integration for the first-party Arcen IddCx driver.

#[cfg(windows)]
use std::sync::Arc;

use arcen_iddcx_provider::abi::AdapterLuid as ContractLuid;
#[cfg(windows)]
use arcen_iddcx_provider::abi::{Capabilities, PRODUCT_CODE_BASE};
#[cfg(windows)]
use arcen_iddcx_provider::{resolve_render_adapter, AdapterCandidate, CapabilityGate};

use crate::config::WindowsIddCxConfig;
use crate::multi_monitor_topology::{
    AvailableOutput, OutputModeCapability, PhysicalOutputInventory,
};
use crate::nvapi::AdapterLuid;

pub(crate) const CONTROL_PATH: &str = r"\\.\Global\ArcenIddCx";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedRenderAdapter {
    pub(crate) stable_id: String,
    pub(crate) description: String,
    pub(crate) luid: AdapterLuid,
}

pub(crate) fn planning_inventory(
    config: &WindowsIddCxConfig,
) -> Result<PhysicalOutputInventory, String> {
    let adapter = resolve_configured_render_adapter(config)?;
    let outputs = (0..arcen_iddcx_provider::abi::MAX_MONITORS)
        .map(|index| AvailableOutput {
            adapter_luid: adapter.luid,
            target_id: u32::try_from(index).expect("IddCx connector count is bounded"),
            adapter_output_index: u32::try_from(index).expect("IddCx connector count is bounded"),
            adapter_name: adapter.description.clone(),
            global_index: u32::try_from(index).expect("IddCx connector count is bounded"),
            device_name: format!("ARCEN_IDDCX_CONNECTOR_{index}"),
            mode_capability: OutputModeCapability::CustomTimingCapable {
                min_width: arcen_iddcx_provider::abi::MIN_WIDTH,
                max_width: arcen_iddcx_provider::abi::MAX_WIDTH,
                min_height: arcen_iddcx_provider::abi::MIN_HEIGHT,
                max_height: arcen_iddcx_provider::abi::MAX_HEIGHT,
                min_refresh_hz: arcen_iddcx_provider::abi::MIN_REFRESH_MILLIHZ / 1_000,
                max_refresh_hz: arcen_iddcx_provider::abi::MAX_REFRESH_MILLIHZ / 1_000,
            },
            supported_rotations: vec![
                arcen_media::Rotation::Degrees0,
                arcen_media::Rotation::Degrees90,
                arcen_media::Rotation::Degrees180,
                arcen_media::Rotation::Degrees270,
            ],
            current_x: 0,
            current_y: 0,
            current_width: 1_920,
            current_height: 1_080,
            current_refresh_hz: 60,
            primary: index == 0,
        })
        .collect();
    PhysicalOutputInventory::new(outputs).map_err(|error| error.to_string())
}

#[cfg(windows)]
pub(crate) fn probe_strict_readiness(config: &WindowsIddCxConfig) -> Result<(), String> {
    if !config.enabled {
        return Err("platform.iddcx.enabled is false".to_string());
    }
    let _ = resolve_configured_render_adapter(config)?;
    let control = NativeControl::open()?;
    ensure_capabilities(config.enabled, &control.capabilities()?)
}

#[cfg(not(windows))]
pub(crate) fn probe_strict_readiness(_config: &WindowsIddCxConfig) -> Result<(), String> {
    Err("IddCx is available only on Windows".to_string())
}

#[cfg(windows)]
pub(crate) fn open_inheritable_control_file() -> Result<std::fs::File, String> {
    NativeControl::open_file()
}

#[cfg(not(windows))]
pub(crate) fn open_inheritable_control_file() -> Result<std::fs::File, String> {
    Err("IddCx is available only on Windows".to_string())
}

#[cfg(windows)]
static INHERITED_CONTROL: std::sync::OnceLock<Arc<NativeControl>> = std::sync::OnceLock::new();

#[cfg(windows)]
pub(crate) fn install_inherited_control_handle(raw: isize) -> Result<(), String> {
    use std::os::windows::io::{FromRawHandle, RawHandle};

    if raw == 0 || raw == -1 {
        return Err("session agent inherited an invalid IddCx control handle".to_string());
    }
    // SAFETY: CreateProcess inherited this handle specifically for the child.
    // This function runs once, and the resulting File owns and closes it.
    let file = unsafe { std::fs::File::from_raw_handle(raw as RawHandle) };
    INHERITED_CONTROL
        .set(Arc::new(NativeControl::from_file(file)))
        .map_err(|_| "IddCx control handle was installed more than once".to_string())
}

#[cfg(not(windows))]
pub(crate) fn install_inherited_control_handle(_raw: isize) -> Result<(), String> {
    Err("IddCx is available only on Windows".to_string())
}

#[cfg(windows)]
pub(crate) fn inherited_control(enabled: bool) -> Result<Arc<NativeControl>, String> {
    let control = INHERITED_CONTROL.get().cloned().ok_or_else(|| {
        "IddCx is enabled but no broker-owned control handle was inherited".to_string()
    })?;
    ensure_capabilities(enabled, &control.capabilities()?)?;
    Ok(control)
}

#[cfg(windows)]
pub(crate) fn validate_inherited_control(enabled: bool) -> Result<(), String> {
    inherited_control(enabled).map(|_| ())
}

#[cfg(not(windows))]
pub(crate) fn validate_inherited_control(_enabled: bool) -> Result<(), String> {
    Err("IddCx is available only on Windows".to_string())
}

#[cfg(windows)]
fn ensure_capabilities(enabled: bool, capabilities: &Capabilities) -> Result<(), String> {
    match arcen_iddcx_provider::evaluate_capabilities(enabled, capabilities) {
        CapabilityGate::Ready => Ok(()),
        CapabilityGate::Disabled => Err("IddCx capability gate is disabled".to_string()),
        CapabilityGate::Blocked(reason) => Err(format!(
            "IddCx strict capability gate blocked the provider: {reason}"
        )),
    }
}

#[cfg(windows)]
pub(crate) struct NativeControl {
    file: std::sync::Mutex<std::fs::File>,
}

#[cfg(windows)]
impl NativeControl {
    fn from_file(file: std::fs::File) -> Self {
        Self {
            file: std::sync::Mutex::new(file),
        }
    }

    fn open() -> Result<Self, String> {
        Self::open_file().map(Self::from_file)
    }

    fn open_file() -> Result<std::fs::File, String> {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(CONTROL_PATH)
            .map_err(|error| format!("open {CONTROL_PATH}: {error}"))
    }

    pub(crate) fn capabilities(&self) -> Result<Capabilities, String> {
        let mut response = Capabilities::default();
        self.ioctl(
            arcen_iddcx_provider::abi::IOCTL_GET_CAPABILITIES,
            None::<&u8>,
            &mut response,
        )?;
        Ok(response)
    }

    pub(crate) fn apply(
        &self,
        request: &arcen_iddcx_provider::abi::ApplyRequest,
    ) -> Result<arcen_iddcx_provider::abi::TopologyResponse, String> {
        let mut response = arcen_iddcx_provider::abi::TopologyResponse::default();
        self.ioctl(
            arcen_iddcx_provider::abi::IOCTL_APPLY_TOPOLOGY,
            Some(request),
            &mut response,
        )?;
        validate_operation_response("apply", request.generation, &response)?;
        Ok(response)
    }

    pub(crate) fn status(&self) -> Result<arcen_iddcx_provider::abi::StatusResponse, String> {
        let mut response = arcen_iddcx_provider::abi::StatusResponse::default();
        self.ioctl(
            arcen_iddcx_provider::abi::IOCTL_QUERY_STATUS,
            None::<&u8>,
            &mut response,
        )?;
        validate_response_layout(&response)?;
        Ok(response)
    }

    pub(crate) fn remove(
        &self,
        generation: u32,
    ) -> Result<arcen_iddcx_provider::abi::TopologyResponse, String> {
        let request = arcen_iddcx_provider::abi::RemoveRequest {
            generation,
            ..arcen_iddcx_provider::abi::RemoveRequest::default()
        };
        let mut response = arcen_iddcx_provider::abi::TopologyResponse::default();
        self.ioctl(
            arcen_iddcx_provider::abi::IOCTL_REMOVE_TOPOLOGY,
            Some(&request),
            &mut response,
        )?;
        validate_operation_response("remove", 0, &response)?;
        Ok(response)
    }

    fn ioctl<I, O>(&self, code: u32, input: Option<&I>, output: &mut O) -> Result<(), String> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::System::IO::DeviceIoControl;

        let file = self
            .file
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let input_size = input
            .map(|_| u32::try_from(std::mem::size_of::<I>()).expect("IddCx input size fits u32"))
            .unwrap_or(0);
        let output_size =
            u32::try_from(std::mem::size_of::<O>()).expect("IddCx output size fits u32");
        let mut returned = 0u32;
        // SAFETY: the synchronous call cannot retain either pointer. Both
        // values are repr(C) fixed-size ABI structures and remain alive.
        unsafe {
            DeviceIoControl(
                HANDLE(file.as_raw_handle()),
                code,
                input.map(|value| (value as *const I).cast()),
                input_size,
                Some((output as *mut O).cast()),
                output_size,
                Some(&mut returned),
                None,
            )
        }
        .map_err(|error| format!("IddCx DeviceIoControl 0x{code:08x}: {error}"))?;
        if returned as usize != std::mem::size_of::<O>() {
            return Err(format!(
                "IddCx DeviceIoControl 0x{code:08x} returned {returned} bytes, expected {output_size}"
            ));
        }
        Ok(())
    }
}

#[cfg(windows)]
fn validate_response_layout(
    response: &arcen_iddcx_provider::abi::TopologyResponse,
) -> Result<(), String> {
    if response.size as usize != std::mem::size_of::<arcen_iddcx_provider::abi::TopologyResponse>()
        || response.abi_version != arcen_iddcx_provider::abi::ABI_VERSION
    {
        return Err("IddCx driver returned an incompatible topology response".to_string());
    }
    Ok(())
}

#[cfg(windows)]
fn validate_operation_response(
    operation: &str,
    expected_generation: u32,
    response: &arcen_iddcx_provider::abi::TopologyResponse,
) -> Result<(), String> {
    validate_response_layout(response)?;
    if response.operation_status < 0 {
        return Err(format!(
            "IddCx {operation} failed with NTSTATUS 0x{:08x} (rollback 0x{:08x})",
            response.operation_status as u32, response.rollback_status as u32
        ));
    }
    if expected_generation != 0 && response.generation != expected_generation {
        return Err(format!(
            "IddCx {operation} returned generation {}, expected {expected_generation}",
            response.generation
        ));
    }
    Ok(())
}

pub(crate) fn topology_request(
    plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
) -> Result<arcen_iddcx_provider::abi::ApplyRequest, String> {
    let first = plan
        .monitors
        .first()
        .ok_or_else(|| "IddCx topology contains no monitors".to_string())?;
    if plan
        .monitors
        .iter()
        .any(|monitor| monitor.adapter_luid != first.adapter_luid)
    {
        return Err("IddCx topology spans more than one render adapter".to_string());
    }
    let monitors = plan
        .monitors
        .iter()
        .enumerate()
        .map(|(index, monitor)| arcen_iddcx_provider::MonitorSpec {
            connector_index: u32::try_from(index).expect("IddCx monitor count is bounded"),
            desktop_x: monitor.x,
            desktop_y: monitor.y,
            width: monitor.mode_width,
            height: monitor.mode_height,
            refresh_hz: monitor.refresh_hz,
            rotation_degrees: match monitor.rotation {
                arcen_media::Rotation::Degrees0 => 0,
                arcen_media::Rotation::Degrees90 => 90,
                arcen_media::Rotation::Degrees180 => 180,
                arcen_media::Rotation::Degrees270 => 270,
            },
            primary: monitor.primary,
            physical_width_mm: 0,
            physical_height_mm: 0,
        })
        .collect();
    let generation = u32::try_from(plan.generation.get())
        .map_err(|_| "IddCx topology generation exceeds u32".to_string())?;
    arcen_iddcx_provider::build_apply_request(&arcen_iddcx_provider::TopologySpec {
        generation,
        render_adapter: ContractLuid {
            low_part: first.adapter_luid.low_part,
            high_part: first.adapter_luid.high_part,
        },
        monitors,
    })
    .map_err(|error| error.to_string())
}

#[cfg(windows)]
pub(crate) fn active_inventory(
    config: &WindowsIddCxConfig,
) -> Result<PhysicalOutputInventory, String> {
    active_outputs(config).and_then(|outputs| {
        PhysicalOutputInventory::new(outputs.into_iter().map(|(_, output)| output).collect())
            .map_err(|error| error.to_string())
    })
}

#[cfg(not(windows))]
pub(crate) fn active_inventory(
    _config: &WindowsIddCxConfig,
) -> Result<PhysicalOutputInventory, String> {
    Err("IddCx is available only on Windows".to_string())
}

#[cfg(windows)]
pub(crate) fn rebind_applied_plan(
    config: &WindowsIddCxConfig,
    plan: &crate::multi_monitor_topology::WindowsTopologyPlan,
) -> Result<crate::multi_monitor_topology::WindowsTopologyPlan, String> {
    let outputs = active_outputs(config)?;
    if outputs.len() != plan.monitors.len() {
        return Err(format!(
            "IddCx exposed {} Arcen monitors, expected {}",
            outputs.len(),
            plan.monitors.len()
        ));
    }
    let mut rebound = plan.clone();
    for (index, monitor) in rebound.monitors.iter_mut().enumerate() {
        let (_, output) = outputs
            .iter()
            .find(|(connector, _)| *connector == index as u32)
            .ok_or_else(|| format!("IddCx connector {index} was not enumerated"))?;
        if output.current_width != monitor.width
            || output.current_height != monitor.height
            || output.current_refresh_hz != monitor.refresh_hz
            || output.current_x != monitor.x
            || output.current_y != monitor.y
            || output.primary != monitor.primary
        {
            return Err(format!(
                "IddCx connector {index} enumerated as {}x{}@{} at {},{}, expected {}x{}@{} at {},{}",
                output.current_width,
                output.current_height,
                output.current_refresh_hz,
                output.current_x,
                output.current_y,
                monitor.width,
                monitor.height,
                monitor.refresh_hz,
                monitor.x,
                monitor.y
            ));
        }
        monitor.adapter_luid = output.adapter_luid;
        monitor.target_id = output.target_id;
        monitor.adapter_output_index = output.adapter_output_index;
        monitor.adapter_name.clone_from(&output.adapter_name);
        monitor.global_index = output.global_index;
        monitor.device_name.clone_from(&output.device_name);
    }
    Ok(rebound)
}

#[cfg(windows)]
fn active_outputs(config: &WindowsIddCxConfig) -> Result<Vec<(u32, AvailableOutput)>, String> {
    use crate::multi_monitor_topology::{OutputMode, OutputModeCapability};

    let _selected = resolve_configured_render_adapter(config)?;
    let report = crate::gpu_probe::probe()?;
    let mut outputs = report
        .adapters
        .into_iter()
        .filter_map(|adapter| {
            let luid = parse_luid(&adapter.session_luid)?;
            Some((adapter, luid))
        })
        .flat_map(|(adapter, luid)| {
            let adapter_name = adapter.description;
            adapter.outputs.into_iter().filter_map(move |output| {
                let product = output.edid_product_code_id?;
                let connector = product.checked_sub(PRODUCT_CODE_BASE)?;
                if connector >= arcen_iddcx_provider::abi::MAX_MONITORS as u16
                    || !manufacturer_matches(output.edid_manufacture_id?)
                    || !output
                        .monitor_friendly_name
                        .as_deref()
                        .is_some_and(|name| name.trim().eq_ignore_ascii_case("Arcen IDD"))
                {
                    return None;
                }
                let current = output.current_mode?;
                let target_id = output.target_id?;
                let global_index = output.attached_global_index?;
                let (current_width, current_height, current_refresh_hz) =
                    checked_attached_desktop_state(
                        output.desktop_rect.width,
                        output.desktop_rect.height,
                        current.refresh_hz,
                    )?;
                let modes = output
                    .supported_modes
                    .iter()
                    .map(|mode| OutputMode {
                        width: mode.width,
                        height: mode.height,
                        refresh_hz: mode.refresh_hz,
                    })
                    .collect::<Vec<_>>();
                Some((
                    u32::from(connector),
                    AvailableOutput {
                        adapter_luid: AdapterLuid {
                            low_part: luid.low_part,
                            high_part: luid.high_part,
                        },
                        target_id,
                        adapter_output_index: output.adapter_output_index,
                        adapter_name: adapter_name.clone(),
                        global_index,
                        device_name: output.device_name,
                        mode_capability: OutputModeCapability::FixedModes(modes),
                        supported_rotations: vec![
                            arcen_media::Rotation::Degrees0,
                            arcen_media::Rotation::Degrees90,
                            arcen_media::Rotation::Degrees180,
                            arcen_media::Rotation::Degrees270,
                        ],
                        current_x: output.desktop_rect.left,
                        current_y: output.desktop_rect.top,
                        current_width,
                        current_height,
                        current_refresh_hz,
                        primary: output.primary,
                    },
                ))
            })
        })
        .collect::<Vec<_>>();
    outputs.sort_by_key(|(connector, _)| *connector);
    Ok(outputs)
}

#[cfg(any(windows, test))]
fn checked_attached_desktop_state(
    width: i32,
    height: i32,
    refresh_hz: u32,
) -> Option<(u32, u32, u32)> {
    Some((
        u32::try_from(width).ok()?,
        u32::try_from(height).ok()?,
        refresh_hz,
    ))
}

#[cfg(windows)]
fn manufacturer_matches(actual: u16) -> bool {
    actual == arcen_iddcx_provider::abi::EDID_MANUFACTURER_ID
        || actual.swap_bytes() == arcen_iddcx_provider::abi::EDID_MANUFACTURER_ID
}

#[cfg(windows)]
fn resolve_configured_render_adapter(
    config: &WindowsIddCxConfig,
) -> Result<ResolvedRenderAdapter, String> {
    let report = crate::gpu_probe::probe()?;
    let candidates = report
        .adapters
        .into_iter()
        .filter_map(|adapter| {
            parse_luid(&adapter.session_luid).map(|luid| AdapterCandidate {
                stable_id: adapter.stable_id,
                description: adapter.description,
                luid,
                direct_capture_candidate: adapter.direct_nvenc_candidate,
            })
        })
        .collect::<Vec<_>>();
    let selected = resolve_render_adapter(
        config.render_adapter.stable_id.as_deref(),
        config.render_adapter.description.as_deref(),
        &candidates,
    )
    .map_err(|error| format!("resolve IddCx render adapter: {error}"))?;
    Ok(ResolvedRenderAdapter {
        stable_id: selected.stable_id,
        description: selected.description,
        luid: AdapterLuid {
            low_part: selected.luid.low_part,
            high_part: selected.luid.high_part,
        },
    })
}

#[cfg(not(windows))]
fn resolve_configured_render_adapter(
    _config: &WindowsIddCxConfig,
) -> Result<ResolvedRenderAdapter, String> {
    Err("IddCx render-adapter probing is available only on Windows".to_string())
}

#[cfg(windows)]
fn parse_luid(value: &str) -> Option<ContractLuid> {
    let (high, low) = value.split_once(':')?;
    Some(ContractLuid {
        low_part: u32::from_str_radix(low, 16).ok()?,
        high_part: u32::from_str_radix(high, 16).ok()? as i32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_media::{
        AppliedSize, LogicalPoint, LogicalRect, LogicalSize, Rotation, Scale120, SessionMonitorId,
        TopologyGeneration,
    };

    #[test]
    fn planning_inventory_reserves_four_dynamic_connectors() {
        if cfg!(windows) {
            return;
        }
        let error = planning_inventory(&WindowsIddCxConfig::default()).expect_err("no probe");
        assert!(error.to_string().contains("Windows"));
    }

    #[test]
    fn topology_request_preserves_affinity_geometry_and_rotation() {
        let plan = crate::multi_monitor_topology::WindowsTopologyPlan {
            generation: TopologyGeneration::new(9).expect("generation"),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 1_920,
            desktop_height: 1_080,
            monitors: vec![crate::multi_monitor_topology::WindowsMonitorPlan {
                session_monitor_id: SessionMonitorId::new(1).expect("monitor"),
                client_display_id: "main".to_string(),
                adapter_luid: AdapterLuid {
                    low_part: 7,
                    high_part: -1,
                },
                target_id: 0,
                adapter_output_index: 0,
                adapter_name: "GPU".to_string(),
                global_index: 0,
                device_name: "virtual".to_string(),
                x: 0,
                y: 0,
                width: 1_920,
                height: 1_080,
                mode_width: 1_080,
                mode_height: 1_920,
                logical_rect: LogicalRect::new(
                    LogicalPoint::new(0, 0),
                    LogicalSize::from_pixels(1_920, 1_080).expect("size"),
                )
                .expect("rect"),
                scale: Scale120::new(120).expect("scale"),
                refresh_hz: 60,
                rotation: Rotation::Degrees90,
                primary: true,
            }],
            requires_custom_timing: false,
        };
        let request = topology_request(&plan).expect("request");
        assert_eq!(request.generation, 9);
        assert_eq!(request.render_adapter.low_part, 7);
        assert_eq!(request.render_adapter.high_part, -1);
        assert_eq!(request.monitors[0].rotation_degrees, 90);
        assert_eq!(request.monitors[0].modes[0].width, 1_080);
        assert_eq!(request.monitors[0].modes[0].height, 1_920);
        let (requested, applied) = plan.region_sets().expect("region sets");
        assert_eq!(
            requested.primary().logical_rect().size(),
            LogicalSize::from_pixels(1_920, 1_080).expect("logical size")
        );
        assert_eq!(
            applied.primary().applied_rect().size(),
            AppliedSize::new(1_920, 1_080).expect("applied size")
        );
    }

    #[test]
    fn attached_desktop_state_uses_rotated_footprint_and_current_refresh() {
        let native_mode = (1_080, 1_920);
        let state = checked_attached_desktop_state(1_920, 1_080, 60);
        assert_eq!(state, Some((1_920, 1_080, 60)));
        assert_eq!(native_mode, (1_080, 1_920));
        assert_eq!(checked_attached_desktop_state(-1, 1_080, 60), None);
    }
}
