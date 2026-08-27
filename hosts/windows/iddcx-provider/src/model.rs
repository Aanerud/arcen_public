use std::collections::BTreeSet;

use crate::abi::{
    ABI_VERSION, APPLY_REPLACE_TOPOLOGY, APPLY_REQUIRE_RENDER_ADAPTER, AdapterLuid, ApplyRequest,
    Capabilities, DRIVER_VERSION, MAX_HEIGHT, MAX_MODES_PER_MONITOR, MAX_MODES_PER_MONITOR_U32,
    MAX_MONITORS, MAX_MONITORS_U32, MAX_WIDTH, MIN_HEIGHT, MIN_REFRESH_MILLIHZ, MIN_WIDTH,
    MONITOR_PRIMARY, Mode, MonitorDescriptor, PRODUCT_CODE_BASE, REQUIRED_CAPABILITIES,
};
use crate::build_base_edid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MonitorSpec {
    pub connector_index: u32,
    pub desktop_x: i32,
    pub desktop_y: i32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub rotation_degrees: u32,
    pub primary: bool,
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TopologySpec {
    pub generation: u32,
    pub render_adapter: AdapterLuid,
    pub monitors: Vec<MonitorSpec>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TopologyError {
    ZeroGeneration,
    ZeroRenderAdapter,
    MonitorCount {
        count: usize,
    },
    DuplicateConnector {
        connector_index: u32,
    },
    PrimaryCount {
        count: usize,
    },
    InvalidRotation {
        connector_index: u32,
        degrees: u32,
    },
    InvalidMode {
        connector_index: u32,
    },
    InvalidEdid {
        connector_index: u32,
        reason: String,
    },
}

impl core::fmt::Display for TopologyError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ZeroGeneration => {
                formatter.write_str("IddCx topology generation must be nonzero")
            }
            Self::ZeroRenderAdapter => {
                formatter.write_str("IddCx render-adapter affinity must resolve to a nonzero LUID")
            }
            Self::MonitorCount { count } => {
                write!(
                    formatter,
                    "IddCx topology must contain 1..={MAX_MONITORS} monitors, got {count}"
                )
            }
            Self::DuplicateConnector { connector_index } => {
                write!(
                    formatter,
                    "IddCx connector index {connector_index} is duplicated"
                )
            }
            Self::PrimaryCount { count } => {
                write!(
                    formatter,
                    "IddCx topology must contain exactly one primary monitor, got {count}"
                )
            }
            Self::InvalidRotation {
                connector_index,
                degrees,
            } => write!(
                formatter,
                "IddCx connector {connector_index} has unsupported rotation {degrees}"
            ),
            Self::InvalidMode { connector_index } => {
                write!(
                    formatter,
                    "IddCx connector {connector_index} has an unsupported mode"
                )
            }
            Self::InvalidEdid {
                connector_index,
                reason,
            } => write!(
                formatter,
                "build IddCx connector {connector_index} EDID: {reason}"
            ),
        }
    }
}

impl std::error::Error for TopologyError {}

/// Builds the fixed Rust/C ABI request after validating the complete topology.
///
/// # Errors
///
/// Returns [`TopologyError`] when the generation, affinity, monitor roster,
/// rotation, mode, or generated EDID violates the provider contract.
pub fn build_apply_request(spec: &TopologySpec) -> Result<ApplyRequest, TopologyError> {
    validate_topology(spec)?;
    let monitor_count =
        u32::try_from(spec.monitors.len()).map_err(|_| TopologyError::MonitorCount {
            count: spec.monitors.len(),
        })?;
    let mut request = ApplyRequest {
        generation: spec.generation,
        monitor_count,
        render_adapter: spec.render_adapter,
        flags: APPLY_REPLACE_TOPOLOGY | APPLY_REQUIRE_RENDER_ADAPTER,
        ..ApplyRequest::default()
    };
    for (target, monitor) in request.monitors.iter_mut().zip(&spec.monitors) {
        let preferred = Mode {
            width: monitor.width,
            height: monitor.height,
            refresh_millihz: monitor.refresh_hz.saturating_mul(1_000),
        };
        let modes = mode_roster(preferred);
        let connector = u16::try_from(monitor.connector_index).map_err(|_| {
            TopologyError::DuplicateConnector {
                connector_index: monitor.connector_index,
            }
        })?;
        let product_code =
            PRODUCT_CODE_BASE
                .checked_add(connector)
                .ok_or(TopologyError::DuplicateConnector {
                    connector_index: monitor.connector_index,
                })?;
        let serial_number = spec
            .generation
            .rotate_left(8)
            .wrapping_add(monitor.connector_index + 1);
        let edid = build_base_edid(
            preferred,
            product_code,
            serial_number,
            monitor.physical_width_mm,
            monitor.physical_height_mm,
        )
        .map_err(|error| TopologyError::InvalidEdid {
            connector_index: monitor.connector_index,
            reason: error.to_string(),
        })?;
        *target = MonitorDescriptor {
            connector_index: monitor.connector_index,
            desktop_x: monitor.desktop_x,
            desktop_y: monitor.desktop_y,
            rotation_degrees: monitor.rotation_degrees,
            flags: if monitor.primary { MONITOR_PRIMARY } else { 0 },
            mode_count: u32::try_from(modes.len()).map_err(|_| TopologyError::InvalidMode {
                connector_index: monitor.connector_index,
            })?,
            preferred_mode_index: 0,
            physical_width_mm: monitor.physical_width_mm,
            physical_height_mm: monitor.physical_height_mm,
            serial_number,
            product_code,
            reserved: 0,
            modes: {
                let mut target_modes = [Mode::default(); MAX_MODES_PER_MONITOR];
                target_modes[..modes.len()].copy_from_slice(&modes);
                target_modes
            },
            edid,
        };
    }
    debug_assert_eq!(request.abi_version, ABI_VERSION);
    Ok(request)
}

fn validate_topology(spec: &TopologySpec) -> Result<(), TopologyError> {
    if spec.generation == 0 {
        return Err(TopologyError::ZeroGeneration);
    }
    if spec.render_adapter.is_zero() {
        return Err(TopologyError::ZeroRenderAdapter);
    }
    if !(1..=MAX_MONITORS).contains(&spec.monitors.len()) {
        return Err(TopologyError::MonitorCount {
            count: spec.monitors.len(),
        });
    }
    let primary_count = spec
        .monitors
        .iter()
        .filter(|monitor| monitor.primary)
        .count();
    if primary_count != 1 {
        return Err(TopologyError::PrimaryCount {
            count: primary_count,
        });
    }
    let mut connectors = BTreeSet::new();
    for monitor in &spec.monitors {
        if monitor.connector_index >= MAX_MONITORS_U32
            || !connectors.insert(monitor.connector_index)
        {
            return Err(TopologyError::DuplicateConnector {
                connector_index: monitor.connector_index,
            });
        }
        if !matches!(monitor.rotation_degrees, 0 | 90 | 180 | 270) {
            return Err(TopologyError::InvalidRotation {
                connector_index: monitor.connector_index,
                degrees: monitor.rotation_degrees,
            });
        }
        let refresh_millihz = monitor.refresh_hz.saturating_mul(1_000);
        if monitor.width < MIN_WIDTH
            || monitor.width > MAX_WIDTH
            || monitor.height < MIN_HEIGHT
            || monitor.height > MAX_HEIGHT
            || !(MIN_REFRESH_MILLIHZ..=crate::abi::MAX_REFRESH_MILLIHZ).contains(&refresh_millihz)
        {
            return Err(TopologyError::InvalidMode {
                connector_index: monitor.connector_index,
            });
        }
    }
    Ok(())
}

fn mode_roster(preferred: Mode) -> Vec<Mode> {
    let mut modes = Vec::with_capacity(5);
    push_unique(&mut modes, preferred);
    if preferred.refresh_millihz != 60_000 {
        push_unique(
            &mut modes,
            Mode {
                refresh_millihz: 60_000,
                ..preferred
            },
        );
    }
    for (width, height) in [(1_920, 1_080), (1_280, 720), (1_024, 768)] {
        push_unique(
            &mut modes,
            Mode {
                width,
                height,
                refresh_millihz: 60_000,
            },
        );
    }
    modes.truncate(MAX_MODES_PER_MONITOR);
    modes
}

fn push_unique(modes: &mut Vec<Mode>, candidate: Mode) {
    if !modes.contains(&candidate) {
        modes.push(candidate);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityGate {
    Disabled,
    Ready,
    Blocked(CapabilityBlocker),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CapabilityBlocker {
    InvalidSize { actual: u32 },
    AbiVersion { actual: u32 },
    DriverVersion { actual: u32 },
    MissingFlags { missing: u32 },
    MonitorCapacity { actual: u32 },
    ModeCapacity { actual: u32 },
    GeometryBounds,
    RefreshBounds,
    AdapterNotReady { state: u32 },
    StaleTopology { generation: u32, monitors: u32 },
}

impl core::fmt::Display for CapabilityBlocker {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidSize { actual } => write!(formatter, "driver capability size is {actual}"),
            Self::AbiVersion { actual } => {
                write!(
                    formatter,
                    "driver ABI {actual} does not match {ABI_VERSION}"
                )
            }
            Self::DriverVersion { actual } => {
                write!(
                    formatter,
                    "driver version 0x{actual:08x} does not match 0x{DRIVER_VERSION:08x}"
                )
            }
            Self::MissingFlags { missing } => {
                write!(
                    formatter,
                    "driver is missing capability flags 0x{missing:08x}"
                )
            }
            Self::MonitorCapacity { actual } => {
                write!(formatter, "driver supports only {actual} monitors")
            }
            Self::ModeCapacity { actual } => {
                write!(formatter, "driver supports only {actual} modes per monitor")
            }
            Self::GeometryBounds => formatter.write_str("driver geometry bounds are insufficient"),
            Self::RefreshBounds => formatter.write_str("driver refresh bounds are insufficient"),
            Self::AdapterNotReady { state } => {
                write!(formatter, "driver adapter state {state} is not ready")
            }
            Self::StaleTopology {
                generation,
                monitors,
            } => write!(
                formatter,
                "driver still owns stale generation {generation} with {monitors} monitors"
            ),
        }
    }
}

#[must_use]
pub fn evaluate_capabilities(enabled: bool, capabilities: &Capabilities) -> CapabilityGate {
    if !enabled {
        return CapabilityGate::Disabled;
    }
    if capabilities.size as usize != core::mem::size_of::<Capabilities>() {
        return CapabilityGate::Blocked(CapabilityBlocker::InvalidSize {
            actual: capabilities.size,
        });
    }
    if capabilities.abi_version != ABI_VERSION {
        return CapabilityGate::Blocked(CapabilityBlocker::AbiVersion {
            actual: capabilities.abi_version,
        });
    }
    if capabilities.driver_version != DRIVER_VERSION {
        return CapabilityGate::Blocked(CapabilityBlocker::DriverVersion {
            actual: capabilities.driver_version,
        });
    }
    let missing = REQUIRED_CAPABILITIES & !capabilities.flags;
    if missing != 0 {
        return CapabilityGate::Blocked(CapabilityBlocker::MissingFlags { missing });
    }
    if capabilities.max_monitors < MAX_MONITORS_U32 {
        return CapabilityGate::Blocked(CapabilityBlocker::MonitorCapacity {
            actual: capabilities.max_monitors,
        });
    }
    if capabilities.max_modes_per_monitor < MAX_MODES_PER_MONITOR_U32 {
        return CapabilityGate::Blocked(CapabilityBlocker::ModeCapacity {
            actual: capabilities.max_modes_per_monitor,
        });
    }
    if capabilities.min_width > MIN_WIDTH
        || capabilities.max_width < MAX_WIDTH
        || capabilities.min_height > MIN_HEIGHT
        || capabilities.max_height < MAX_HEIGHT
    {
        return CapabilityGate::Blocked(CapabilityBlocker::GeometryBounds);
    }
    if capabilities.min_refresh_millihz > MIN_REFRESH_MILLIHZ
        || capabilities.max_refresh_millihz < crate::abi::MAX_REFRESH_MILLIHZ
    {
        return CapabilityGate::Blocked(CapabilityBlocker::RefreshBounds);
    }
    if capabilities.adapter_state != 2 {
        return CapabilityGate::Blocked(CapabilityBlocker::AdapterNotReady {
            state: capabilities.adapter_state,
        });
    }
    if capabilities.active_monitor_count != 0 {
        return CapabilityGate::Blocked(CapabilityBlocker::StaleTopology {
            generation: capabilities.active_generation,
            monitors: capabilities.active_monitor_count,
        });
    }
    CapabilityGate::Ready
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdapterCandidate {
    pub stable_id: String,
    pub description: String,
    pub luid: AdapterLuid,
    pub direct_capture_candidate: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AffinityError {
    EmptySelector,
    NoMatch,
    Ambiguous { matches: usize },
    NotCaptureCapable,
    ZeroLuid,
}

impl core::fmt::Display for AffinityError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptySelector => formatter.write_str("render-adapter selector is empty"),
            Self::NoMatch => formatter.write_str("render-adapter selector matched no DXGI adapter"),
            Self::Ambiguous { matches } => {
                write!(
                    formatter,
                    "render-adapter selector matched {matches} DXGI adapters"
                )
            }
            Self::NotCaptureCapable => {
                formatter.write_str("selected render adapter is not a direct-capture candidate")
            }
            Self::ZeroLuid => formatter.write_str("selected render adapter has a zero LUID"),
        }
    }
}

impl std::error::Error for AffinityError {}

/// Resolves one exact, capture-capable render adapter.
///
/// # Errors
///
/// Returns [`AffinityError`] when selectors are empty, absent, ambiguous, or
/// resolve to an adapter that cannot support the direct capture path.
pub fn resolve_render_adapter(
    stable_id: Option<&str>,
    description: Option<&str>,
    candidates: &[AdapterCandidate],
) -> Result<AdapterCandidate, AffinityError> {
    let stable_id = stable_id.map(str::trim).filter(|value| !value.is_empty());
    let description = description.map(str::trim).filter(|value| !value.is_empty());
    if stable_id.is_none() && description.is_none() {
        return Err(AffinityError::EmptySelector);
    }
    let matches = candidates
        .iter()
        .filter(|candidate| {
            stable_id.is_none_or(|expected| candidate.stable_id == expected)
                && description
                    .is_none_or(|expected| candidate.description.eq_ignore_ascii_case(expected))
        })
        .cloned()
        .collect::<Vec<_>>();
    let candidate = match matches.as_slice() {
        [] => return Err(AffinityError::NoMatch),
        [candidate] => candidate.clone(),
        _ => {
            return Err(AffinityError::Ambiguous {
                matches: matches.len(),
            });
        }
    };
    if !candidate.direct_capture_candidate {
        return Err(AffinityError::NotCaptureCapable);
    }
    if candidate.luid.is_zero() {
        return Err(AffinityError::ZeroLuid);
    }
    Ok(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abi::{
        CAP_ATOMIC_REPLACE, CAP_CONSOLE_SESSION, CAP_DYNAMIC_MONITORS, CAP_EXACT_MODES,
        CAP_HANDLE_CLEANUP_ROLLBACK, CAP_MONITOR_EDID, CAP_RENDER_ADAPTER_AFFINITY, CAP_ROLLBACK,
        CAP_SWAPCHAIN_DRAIN,
    };

    fn monitor(index: u32, primary: bool) -> MonitorSpec {
        MonitorSpec {
            connector_index: index,
            desktop_x: if index == 0 { 0 } else { 1_920 },
            desktop_y: 0,
            width: 1_920,
            height: 1_080,
            refresh_hz: 60,
            rotation_degrees: 0,
            primary,
            physical_width_mm: 0,
            physical_height_mm: 0,
        }
    }

    #[test]
    fn builds_one_through_four_dynamic_descriptors() {
        for count in 1..=MAX_MONITORS {
            let spec = TopologySpec {
                generation: 7,
                render_adapter: AdapterLuid {
                    low_part: 42,
                    high_part: -1,
                },
                monitors: (0..count)
                    .map(|index| monitor(index as u32, index == 0))
                    .collect(),
            };
            let request = build_apply_request(&spec).expect("request");
            assert_eq!(request.monitor_count as usize, count);
            for descriptor in &request.monitors[..count] {
                assert!((1..=MAX_MODES_PER_MONITOR as u32).contains(&descriptor.mode_count));
                assert_eq!(
                    descriptor
                        .edid
                        .iter()
                        .fold(0u8, |sum, value| sum.wrapping_add(*value)),
                    0
                );
            }
        }
    }

    #[test]
    fn rejects_non_atomic_or_under_capable_driver() {
        let mut capabilities = Capabilities::default();
        capabilities.adapter_state = 2;
        assert_eq!(
            evaluate_capabilities(true, &capabilities),
            CapabilityGate::Ready
        );
        capabilities.flags &= !CAP_ATOMIC_REPLACE;
        assert_eq!(
            evaluate_capabilities(true, &capabilities),
            CapabilityGate::Blocked(CapabilityBlocker::MissingFlags {
                missing: CAP_ATOMIC_REPLACE
            })
        );
        let all = CAP_DYNAMIC_MONITORS
            | CAP_MONITOR_EDID
            | CAP_EXACT_MODES
            | CAP_RENDER_ADAPTER_AFFINITY
            | CAP_ATOMIC_REPLACE
            | CAP_ROLLBACK
            | CAP_HANDLE_CLEANUP_ROLLBACK
            | CAP_SWAPCHAIN_DRAIN
            | CAP_CONSOLE_SESSION;
        assert_eq!(all, REQUIRED_CAPABILITIES);

        let mut wrong_version = Capabilities::default();
        wrong_version.adapter_state = 2;
        wrong_version.driver_version = 0;
        assert_eq!(
            evaluate_capabilities(true, &wrong_version),
            CapabilityGate::Blocked(CapabilityBlocker::DriverVersion { actual: 0 })
        );
    }

    #[test]
    fn disabled_gate_never_inspects_driver_state() {
        assert_eq!(
            evaluate_capabilities(false, &Capabilities::default()),
            CapabilityGate::Disabled
        );
    }

    #[test]
    fn affinity_resolution_is_exact_and_rejects_ambiguous_names() {
        let candidates = vec![
            AdapterCandidate {
                stable_id: "pci-a".to_string(),
                description: "NVIDIA GRID".to_string(),
                luid: AdapterLuid {
                    low_part: 1,
                    high_part: 0,
                },
                direct_capture_candidate: true,
            },
            AdapterCandidate {
                stable_id: "pci-b".to_string(),
                description: "NVIDIA GRID".to_string(),
                luid: AdapterLuid {
                    low_part: 2,
                    high_part: 0,
                },
                direct_capture_candidate: true,
            },
        ];
        assert_eq!(
            resolve_render_adapter(None, Some("nvidia grid"), &candidates),
            Err(AffinityError::Ambiguous { matches: 2 })
        );
        assert_eq!(
            resolve_render_adapter(Some("pci-b"), None, &candidates)
                .expect("stable selector")
                .luid
                .low_part,
            2
        );
    }
}
