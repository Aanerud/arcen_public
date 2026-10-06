//! Shared Windows multi-monitor adapter selection policy.
//!
//! Windows supplies the live DXGI/NVAPI inventory. This module keeps the
//! portable policy deterministic: administrator lists only constrain the
//! choice, and an empty allow-list means any eligible streaming adapter.

use arcen_media::Rotation;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsCcdOutputKind {
    /// A real monitor/panel whose preferred/native timing may be landscape
    /// even when Windows presents it as portrait through CCD target rotation.
    PhysicalPanel,
    /// A Pier-owned virtual/headless timing where Arcen writes the mode list,
    /// so portrait can be represented directly as a native portrait timing.
    PierOwnedTiming,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowsCcdModePlan {
    /// Windows source-surface size in desktop coordinates. For rotated
    /// targets this is the already-rotated desktop footprint.
    pub source_width: u32,
    pub source_height: u32,
    /// Native target timing sent to the monitor before CCD applies
    /// `DISPLAYCONFIG_PATH_TARGET_INFO::rotation`.
    pub target_width: u32,
    pub target_height: u32,
    /// Rotation to place in `DISPLAYCONFIG_PATH_TARGET_INFO::rotation`.
    pub target_rotation: Rotation,
}

/// Plans Windows CCD source and target extents from the host-authoritative
/// desktop source size.
///
/// Windows CCD treats source modes as desktop-coordinate surfaces and target
/// modes as native signal timings. A portrait desktop therefore keeps its
/// portrait source size while the target timing is the corresponding
/// landscape mode plus target rotation.
#[must_use]
pub const fn windows_ccd_mode_plan(
    source_width: u32,
    source_height: u32,
    rotation: Rotation,
    output_kind: WindowsCcdOutputKind,
) -> WindowsCcdModePlan {
    let (target_width, target_height, target_rotation) = match output_kind {
        WindowsCcdOutputKind::PhysicalPanel => match rotation {
            Rotation::Degrees0 | Rotation::Degrees180 => (source_width, source_height, rotation),
            Rotation::Degrees90 | Rotation::Degrees270 => (source_height, source_width, rotation),
        },
        WindowsCcdOutputKind::PierOwnedTiming => (source_width, source_height, Rotation::Degrees0),
    };
    WindowsCcdModePlan {
        source_width,
        source_height,
        target_width,
        target_height,
        target_rotation,
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AdminHeadlessMode {
    #[default]
    Auto,
    Off,
    Force,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsMultiMonitorAdapter {
    pub description: String,
    pub vendor_id: u32,
    pub nvenc_capable: bool,
    pub grid_or_quadro_class: bool,
    pub desktop_owner: bool,
    pub dxgi_index: u32,
}

impl WindowsMultiMonitorAdapter {
    #[must_use]
    pub fn is_nvidia(&self) -> bool {
        self.vendor_id == 0x10de
    }

    #[must_use]
    pub fn eligible_for_streaming(&self) -> bool {
        self.is_nvidia() && self.nvenc_capable
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WindowsMultiMonitorPolicy<'a> {
    pub advertise_enabled: bool,
    pub allowed_adapters: &'a [String],
    pub excluded_adapters: &'a [String],
    pub headless_mode: AdminHeadlessMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsMultiMonitorRefusal {
    AdminDisabled,
    NoAdapters,
    NoNvencNvidiaAdapters,
    NoAdaptersWithinAdminLists,
}

impl WindowsMultiMonitorRefusal {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::AdminDisabled => "multi-monitor advertisement is disabled by configuration",
            Self::NoAdapters => "no DXGI adapters were reported by the host inventory",
            Self::NoNvencNvidiaAdapters => {
                "no NVIDIA adapter with NVENC capability was reported by the host inventory"
            }
            Self::NoAdaptersWithinAdminLists => {
                "no NVENC-capable NVIDIA adapter remained after applying administrator adapter lists"
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowsMultiMonitorSelectionReason {
    DesktopAdapter,
    GridOrQuadroAdapter,
    FirstEligibleDxgiAdapter,
}

impl WindowsMultiMonitorSelectionReason {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::DesktopAdapter => {
                "selected the configured/current desktop adapter because it is eligible"
            }
            Self::GridOrQuadroAdapter => {
                "selected a GRID/Quadro-class adapter eligible for NVIDIA headless provisioning"
            }
            Self::FirstEligibleDxgiAdapter => {
                "selected the first eligible NVIDIA/NVENC adapter by DXGI index"
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WindowsMultiMonitorSelection {
    pub adapter_description: String,
    pub nvidia_headless_enabled: bool,
    pub selection_reason: WindowsMultiMonitorSelectionReason,
}

/// Chooses the effective Windows streaming adapter from live adapter facts and
/// administrator allow/exclude lists.
///
/// # Errors
///
/// Returns a typed refusal when the administrator disabled advertisement, the
/// inventory is empty, no NVIDIA/NVENC adapter exists, or administrator lists
/// remove every otherwise eligible adapter.
pub fn resolve_windows_multi_monitor_adapter(
    adapters: &[WindowsMultiMonitorAdapter],
    policy: &WindowsMultiMonitorPolicy<'_>,
) -> Result<WindowsMultiMonitorSelection, WindowsMultiMonitorRefusal> {
    if !policy.advertise_enabled {
        return Err(WindowsMultiMonitorRefusal::AdminDisabled);
    }
    if adapters.is_empty() {
        return Err(WindowsMultiMonitorRefusal::NoAdapters);
    }

    let eligible_before_lists = adapters
        .iter()
        .any(WindowsMultiMonitorAdapter::eligible_for_streaming);
    if !eligible_before_lists {
        return Err(WindowsMultiMonitorRefusal::NoNvencNvidiaAdapters);
    }

    let mut eligible = adapters
        .iter()
        .filter(|adapter| adapter.eligible_for_streaming())
        .filter(|adapter| adapter_allowed(adapter, policy.allowed_adapters))
        .filter(|adapter| !adapter_list_contains(policy.excluded_adapters, &adapter.description))
        .collect::<Vec<_>>();
    eligible.sort_by_key(|adapter| adapter.dxgi_index);

    let (chosen, selection_reason) = eligible
        .iter()
        .find(|adapter| adapter.desktop_owner)
        .map(|adapter| (*adapter, WindowsMultiMonitorSelectionReason::DesktopAdapter))
        .or_else(|| {
            eligible
                .iter()
                .find(|adapter| adapter.grid_or_quadro_class)
                .map(|adapter| {
                    (
                        *adapter,
                        WindowsMultiMonitorSelectionReason::GridOrQuadroAdapter,
                    )
                })
        })
        .or_else(|| {
            eligible.first().map(|adapter| {
                (
                    *adapter,
                    WindowsMultiMonitorSelectionReason::FirstEligibleDxgiAdapter,
                )
            })
        })
        .ok_or(WindowsMultiMonitorRefusal::NoAdaptersWithinAdminLists)?;

    let nvidia_headless_enabled = match policy.headless_mode {
        AdminHeadlessMode::Auto => chosen.grid_or_quadro_class,
        AdminHeadlessMode::Off => false,
        AdminHeadlessMode::Force => true,
    };
    Ok(WindowsMultiMonitorSelection {
        adapter_description: chosen.description.clone(),
        nvidia_headless_enabled,
        selection_reason,
    })
}

fn adapter_allowed(adapter: &WindowsMultiMonitorAdapter, allowed_adapters: &[String]) -> bool {
    allowed_adapters.is_empty() || adapter_list_contains(allowed_adapters, &adapter.description)
}

fn adapter_list_contains(list: &[String], description: &str) -> bool {
    list.iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(description))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ccd_source_stays_oriented_while_target_mode_rotates() {
        for (rotation, expected_target) in [
            (Rotation::Degrees0, (2_880, 5_120)),
            (Rotation::Degrees90, (5_120, 2_880)),
            (Rotation::Degrees180, (2_880, 5_120)),
            (Rotation::Degrees270, (5_120, 2_880)),
        ] {
            let plan =
                windows_ccd_mode_plan(2_880, 5_120, rotation, WindowsCcdOutputKind::PhysicalPanel);
            assert_eq!(
                (plan.source_width, plan.source_height),
                (2_880, 5_120),
                "{rotation:?} source mode is the rotated desktop surface"
            );
            assert_eq!(
                (plan.target_width, plan.target_height),
                expected_target,
                "{rotation:?} target mode is the native timing"
            );
            assert_eq!(plan.target_rotation, rotation);
        }
    }

    #[test]
    fn ccd_pier_owned_timing_uses_native_portrait_without_rotation() {
        for rotation in [
            Rotation::Degrees0,
            Rotation::Degrees90,
            Rotation::Degrees180,
            Rotation::Degrees270,
        ] {
            let plan = windows_ccd_mode_plan(
                1_440,
                2_560,
                rotation,
                WindowsCcdOutputKind::PierOwnedTiming,
            );
            assert_eq!((plan.source_width, plan.source_height), (1_440, 2_560));
            assert_eq!((plan.target_width, plan.target_height), (1_440, 2_560));
            assert_eq!(plan.target_rotation, Rotation::Degrees0);
        }
    }

    #[test]
    fn ccd_mixed_orientation_negative_origin_layout_is_non_overlapping() {
        let primary = windows_ccd_mode_plan(
            5_120,
            2_880,
            Rotation::Degrees0,
            WindowsCcdOutputKind::PhysicalPanel,
        );
        let portrait = windows_ccd_mode_plan(
            2_880,
            5_120,
            Rotation::Degrees270,
            WindowsCcdOutputKind::PhysicalPanel,
        );

        assert_eq!(
            (primary.source_width, primary.source_height),
            (5_120, 2_880)
        );
        assert_eq!(
            (primary.target_width, primary.target_height),
            (5_120, 2_880)
        );
        assert_eq!(
            (portrait.source_width, portrait.source_height),
            (2_880, 5_120)
        );
        assert_eq!(
            (portrait.target_width, portrait.target_height),
            (5_120, 2_880)
        );

        let primary_rect = (0_i32, 0_i32, primary.source_width, primary.source_height);
        let portrait_rect = (
            -2_880_i32,
            -2_240_i32,
            portrait.source_width,
            portrait.source_height,
        );
        let separated_on_x =
            i64::from(portrait_rect.0) + i64::from(portrait_rect.2) <= i64::from(primary_rect.0);
        let overlaps_y = i64::from(portrait_rect.1)
            < i64::from(primary_rect.1) + i64::from(primary_rect.3)
            && i64::from(primary_rect.1) < i64::from(portrait_rect.1) + i64::from(portrait_rect.3);
        assert!(separated_on_x && overlaps_y);
    }

    fn adapter(
        description: &str,
        dxgi_index: u32,
        nvenc_capable: bool,
        grid_or_quadro_class: bool,
        desktop_owner: bool,
    ) -> WindowsMultiMonitorAdapter {
        WindowsMultiMonitorAdapter {
            description: description.to_string(),
            vendor_id: 0x10de,
            nvenc_capable,
            grid_or_quadro_class,
            desktop_owner,
            dxgi_index,
        }
    }

    fn policy<'a>(
        allowed_adapters: &'a [String],
        excluded_adapters: &'a [String],
    ) -> WindowsMultiMonitorPolicy<'a> {
        WindowsMultiMonitorPolicy {
            advertise_enabled: true,
            allowed_adapters,
            excluded_adapters,
            headless_mode: AdminHeadlessMode::Auto,
        }
    }

    #[test]
    fn v100d_plus_rtx_prefers_desktop_when_eligible() {
        let adapters = [
            adapter("NVIDIA GRID V100D-16Q", 0, true, true, false),
            adapter("NVIDIA RTX 6000 Ada", 1, true, false, true),
        ];

        let selected = resolve_windows_multi_monitor_adapter(&adapters, &policy(&[], &[]))
            .expect("desktop RTX is eligible");

        assert_eq!(selected.adapter_description, "NVIDIA RTX 6000 Ada");
        assert!(!selected.nvidia_headless_enabled);
        assert_eq!(
            selected.selection_reason,
            WindowsMultiMonitorSelectionReason::DesktopAdapter
        );
    }

    #[test]
    fn v100d_plus_rtx_exclusion_moves_to_grid() {
        let adapters = [
            adapter("NVIDIA GRID V100D-16Q", 0, true, true, false),
            adapter("NVIDIA RTX 6000 Ada", 1, true, false, true),
        ];
        let excluded = vec!["nvidia rtx 6000 ada".to_string()];

        let selected = resolve_windows_multi_monitor_adapter(&adapters, &policy(&[], &excluded))
            .expect("V100D remains eligible");

        assert_eq!(selected.adapter_description, "NVIDIA GRID V100D-16Q");
        assert!(selected.nvidia_headless_enabled);
        assert_eq!(
            selected.selection_reason,
            WindowsMultiMonitorSelectionReason::GridOrQuadroAdapter
        );
    }

    #[test]
    fn only_geforce_uses_first_eligible_without_headless() {
        let adapters = [
            adapter("NVIDIA GeForce RTX 4080", 3, true, false, false),
            adapter("NVIDIA GeForce RTX 4090", 2, true, false, false),
        ];

        let selected = resolve_windows_multi_monitor_adapter(&adapters, &policy(&[], &[]))
            .expect("GeForce NVENC adapter remains eligible");

        assert_eq!(selected.adapter_description, "NVIDIA GeForce RTX 4090");
        assert!(!selected.nvidia_headless_enabled);
        assert_eq!(
            selected.selection_reason,
            WindowsMultiMonitorSelectionReason::FirstEligibleDxgiAdapter
        );
    }

    #[test]
    fn no_nvidia_refuses_without_disabling_startup() {
        let adapters = [WindowsMultiMonitorAdapter {
            description: "Intel UHD Graphics".to_string(),
            vendor_id: 0x8086,
            nvenc_capable: false,
            grid_or_quadro_class: false,
            desktop_owner: true,
            dxgi_index: 0,
        }];

        assert_eq!(
            resolve_windows_multi_monitor_adapter(&adapters, &policy(&[], &[])),
            Err(WindowsMultiMonitorRefusal::NoNvencNvidiaAdapters)
        );
    }

    #[test]
    fn excluding_everything_refuses_after_inventory_eligibility() {
        let adapters = [adapter("NVIDIA GRID V100D-16Q", 0, true, true, true)];
        let excluded = vec!["NVIDIA GRID V100D-16Q".to_string()];

        assert_eq!(
            resolve_windows_multi_monitor_adapter(&adapters, &policy(&[], &excluded)),
            Err(WindowsMultiMonitorRefusal::NoAdaptersWithinAdminLists)
        );
    }

    #[test]
    fn allow_list_restricts_to_named_adapter() {
        let adapters = [
            adapter("NVIDIA GRID V100D-16Q", 0, true, true, false),
            adapter("NVIDIA RTX 6000 Ada", 1, true, false, true),
        ];
        let allowed = vec!["NVIDIA GRID V100D-16Q".to_string()];

        let selected = resolve_windows_multi_monitor_adapter(&adapters, &policy(&allowed, &[]))
            .expect("allow-list names V100D");

        assert_eq!(selected.adapter_description, "NVIDIA GRID V100D-16Q");
        assert!(selected.nvidia_headless_enabled);
    }

    #[test]
    fn forced_and_disabled_headless_override_auto() {
        let adapters = [adapter("NVIDIA GeForce RTX 4080", 0, true, false, true)];
        let mut cfg = policy(&[], &[]);
        cfg.headless_mode = AdminHeadlessMode::Force;
        assert!(
            resolve_windows_multi_monitor_adapter(&adapters, &cfg)
                .expect("forced")
                .nvidia_headless_enabled
        );
        cfg.headless_mode = AdminHeadlessMode::Off;
        assert!(
            !resolve_windows_multi_monitor_adapter(&adapters, &cfg)
                .expect("disabled")
                .nvidia_headless_enabled
        );
    }
}
