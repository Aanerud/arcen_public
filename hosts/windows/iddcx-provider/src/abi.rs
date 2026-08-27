use core::mem::{align_of, size_of};

pub const ABI_VERSION: u32 = 1;
pub const DRIVER_VERSION: u32 = 0x0001_0000;
pub const MAX_MONITORS: usize = 4;
pub const MAX_MONITORS_U32: u32 = 4;
pub const MAX_MODES_PER_MONITOR: usize = 8;
pub const MAX_MODES_PER_MONITOR_U32: u32 = 8;
pub const EDID_BYTES: usize = 128;
pub const EDID_MANUFACTURER_ID: u16 = (1 << 10) | (18 << 5) | 3;
pub const PRODUCT_CODE_BASE: u16 = 0xa100;
pub const MIN_WIDTH: u32 = 320;
pub const MAX_WIDTH: u32 = 4_095;
pub const MIN_HEIGHT: u32 = 240;
pub const MAX_HEIGHT: u32 = 4_095;
pub const MIN_REFRESH_MILLIHZ: u32 = 24_000;
pub const MAX_REFRESH_MILLIHZ: u32 = 120_000;

pub const CAP_DYNAMIC_MONITORS: u32 = 1 << 0;
pub const CAP_MONITOR_EDID: u32 = 1 << 1;
pub const CAP_EXACT_MODES: u32 = 1 << 2;
pub const CAP_RENDER_ADAPTER_AFFINITY: u32 = 1 << 3;
pub const CAP_ATOMIC_REPLACE: u32 = 1 << 4;
pub const CAP_ROLLBACK: u32 = 1 << 5;
pub const CAP_HANDLE_CLEANUP_ROLLBACK: u32 = 1 << 6;
pub const CAP_SWAPCHAIN_DRAIN: u32 = 1 << 7;
pub const CAP_CONSOLE_SESSION: u32 = 1 << 8;
pub const REQUIRED_CAPABILITIES: u32 = CAP_DYNAMIC_MONITORS
    | CAP_MONITOR_EDID
    | CAP_EXACT_MODES
    | CAP_RENDER_ADAPTER_AFFINITY
    | CAP_ATOMIC_REPLACE
    | CAP_ROLLBACK
    | CAP_HANDLE_CLEANUP_ROLLBACK
    | CAP_SWAPCHAIN_DRAIN
    | CAP_CONSOLE_SESSION;

pub const APPLY_REPLACE_TOPOLOGY: u32 = 1 << 0;
pub const APPLY_REQUIRE_RENDER_ADAPTER: u32 = 1 << 1;
pub const MONITOR_PRIMARY: u32 = 1 << 0;

pub const BINDING_ABSENT: u32 = 0;
pub const BINDING_ARRIVING: u32 = 1;
pub const BINDING_PRESENT: u32 = 2;
pub const BINDING_DEPARTING: u32 = 3;
pub const BINDING_FAILED: u32 = 4;
pub const BINDING_SWAPCHAIN_READY: u32 = 1 << 0;
pub const BINDING_RENDER_ADAPTER_MATCHED: u32 = 1 << 1;

pub const APPLY_REQUEST_SIZE: u32 = 1_104;
pub const TOPOLOGY_RESPONSE_SIZE: u32 = 160;
pub const REMOVE_REQUEST_SIZE: u32 = 24;
pub const CAPABILITIES_SIZE: u32 = 64;

const FILE_DEVICE_UNKNOWN: u32 = 0x22;
const METHOD_BUFFERED: u32 = 0;
const FILE_READ_ACCESS: u32 = 1;
const FILE_WRITE_ACCESS: u32 = 2;

const fn ctl_code(function: u32, access: u32) -> u32 {
    (FILE_DEVICE_UNKNOWN << 16) | (access << 14) | (function << 2) | METHOD_BUFFERED
}

pub const IOCTL_GET_CAPABILITIES: u32 = ctl_code(0x800, FILE_READ_ACCESS);
pub const IOCTL_APPLY_TOPOLOGY: u32 = ctl_code(0x801, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const IOCTL_REMOVE_TOPOLOGY: u32 = ctl_code(0x802, FILE_READ_ACCESS | FILE_WRITE_ACCESS);
pub const IOCTL_QUERY_STATUS: u32 = ctl_code(0x803, FILE_READ_ACCESS);

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AdapterLuid {
    pub low_part: u32,
    pub high_part: i32,
}

impl AdapterLuid {
    #[must_use]
    pub const fn is_zero(self) -> bool {
        self.low_part == 0 && self.high_part == 0
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub refresh_millihz: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MonitorDescriptor {
    pub connector_index: u32,
    pub desktop_x: i32,
    pub desktop_y: i32,
    pub rotation_degrees: u32,
    pub flags: u32,
    pub mode_count: u32,
    pub preferred_mode_index: u32,
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
    pub serial_number: u32,
    pub product_code: u16,
    pub reserved: u16,
    pub modes: [Mode; MAX_MODES_PER_MONITOR],
    pub edid: [u8; EDID_BYTES],
}

impl Default for MonitorDescriptor {
    fn default() -> Self {
        Self {
            connector_index: 0,
            desktop_x: 0,
            desktop_y: 0,
            rotation_degrees: 0,
            flags: 0,
            mode_count: 0,
            preferred_mode_index: 0,
            physical_width_mm: 0,
            physical_height_mm: 0,
            serial_number: 0,
            product_code: 0,
            reserved: 0,
            modes: [Mode::default(); MAX_MODES_PER_MONITOR],
            edid: [0; EDID_BYTES],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApplyRequest {
    pub size: u32,
    pub abi_version: u32,
    pub generation: u32,
    pub monitor_count: u32,
    pub render_adapter: AdapterLuid,
    pub flags: u32,
    pub reserved: u32,
    pub monitors: [MonitorDescriptor; MAX_MONITORS],
}

impl Default for ApplyRequest {
    fn default() -> Self {
        Self {
            size: APPLY_REQUEST_SIZE,
            abi_version: ABI_VERSION,
            generation: 0,
            monitor_count: 0,
            render_adapter: AdapterLuid::default(),
            flags: APPLY_REPLACE_TOPOLOGY | APPLY_REQUIRE_RENDER_ADAPTER,
            reserved: 0,
            monitors: core::array::from_fn(|_| MonitorDescriptor::default()),
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MonitorBinding {
    pub connector_index: u32,
    pub state: u32,
    pub os_adapter: AdapterLuid,
    pub os_target_id: u32,
    pub actual_render_adapter: AdapterLuid,
    pub flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TopologyResponse {
    pub size: u32,
    pub abi_version: u32,
    pub generation: u32,
    pub operation_status: i32,
    pub monitor_count: u32,
    pub rollback_status: i32,
    pub reserved: [u32; 2],
    pub bindings: [MonitorBinding; MAX_MONITORS],
}

impl Default for TopologyResponse {
    fn default() -> Self {
        Self {
            size: TOPOLOGY_RESPONSE_SIZE,
            abi_version: ABI_VERSION,
            generation: 0,
            operation_status: 0,
            monitor_count: 0,
            rollback_status: 0,
            reserved: [0; 2],
            bindings: [MonitorBinding::default(); MAX_MONITORS],
        }
    }
}

pub type StatusResponse = TopologyResponse;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoveRequest {
    pub size: u32,
    pub abi_version: u32,
    pub generation: u32,
    pub flags: u32,
    pub reserved: [u32; 2],
}

impl Default for RemoveRequest {
    fn default() -> Self {
        Self {
            size: REMOVE_REQUEST_SIZE,
            abi_version: ABI_VERSION,
            generation: 0,
            flags: 0,
            reserved: [0; 2],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    pub size: u32,
    pub abi_version: u32,
    pub driver_version: u32,
    pub flags: u32,
    pub max_monitors: u32,
    pub max_modes_per_monitor: u32,
    pub min_width: u32,
    pub max_width: u32,
    pub min_height: u32,
    pub max_height: u32,
    pub min_refresh_millihz: u32,
    pub max_refresh_millihz: u32,
    pub adapter_state: u32,
    pub active_generation: u32,
    pub active_monitor_count: u32,
    pub reserved: u32,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            size: CAPABILITIES_SIZE,
            abi_version: ABI_VERSION,
            driver_version: DRIVER_VERSION,
            flags: REQUIRED_CAPABILITIES,
            max_monitors: MAX_MONITORS_U32,
            max_modes_per_monitor: MAX_MODES_PER_MONITOR_U32,
            min_width: MIN_WIDTH,
            max_width: MAX_WIDTH,
            min_height: MIN_HEIGHT,
            max_height: MAX_HEIGHT,
            min_refresh_millihz: MIN_REFRESH_MILLIHZ,
            max_refresh_millihz: MAX_REFRESH_MILLIHZ,
            adapter_state: 0,
            active_generation: 0,
            active_monitor_count: 0,
            reserved: 0,
        }
    }
}

const _: () = {
    assert!(size_of::<AdapterLuid>() == 8);
    assert!(align_of::<AdapterLuid>() == 4);
    assert!(size_of::<Mode>() == 12);
    assert!(size_of::<MonitorDescriptor>() == 268);
    assert!(size_of::<ApplyRequest>() == 1_104);
    assert!(size_of::<MonitorBinding>() == 32);
    assert!(size_of::<TopologyResponse>() == 160);
    assert!(size_of::<RemoveRequest>() == 24);
    assert!(size_of::<Capabilities>() == 64);
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_values_are_stable() {
        assert_eq!(IOCTL_GET_CAPABILITIES, 0x0022_6000);
        assert_eq!(IOCTL_APPLY_TOPOLOGY, 0x0022_e004);
        assert_eq!(IOCTL_REMOVE_TOPOLOGY, 0x0022_e008);
        assert_eq!(IOCTL_QUERY_STATUS, 0x0022_600c);
    }

    #[test]
    fn defaults_carry_exact_abi_sizes() {
        assert_eq!(
            ApplyRequest::default().size as usize,
            size_of::<ApplyRequest>()
        );
        assert_eq!(
            TopologyResponse::default().size as usize,
            size_of::<TopologyResponse>()
        );
        assert_eq!(
            Capabilities::default().size as usize,
            size_of::<Capabilities>()
        );
    }
}
