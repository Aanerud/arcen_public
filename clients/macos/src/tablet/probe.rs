//! Deterministic tablet presence/capability probe.
//!
//! Two independent, deterministic signals are combined here, and neither one
//! requires — or claims — that a human moved a physical pen:
//!
//! 1. **USB presence**: whether a Wacom-vendor USB device is enumerated at
//!    all (via the existing cross-platform [`crate::logging::diagnostics`]
//!    inventory). This is available the instant a tablet is plugged in, with
//!    zero pen interaction.
//! 2. **Empirical axis capability**: which axes (pressure, tilt, rotation,
//!    eraser, barrel buttons) have actually been *observed* varying across
//!    delivered [`super::sample::RawTabletSample`]s. This never claims a
//!    capability exists before it is actually seen, and it never claims a
//!    capability is *absent* just because it has not been seen yet — the
//!    default is [`arcen_input::CapabilityAvailability::Unknown`], not
//!    `Unavailable`.
//!
//! Both signals are exposed as pure, unit-testable functions/types so this
//! probe's *logic* is deterministic even though live USB enumeration and live
//! AppKit sample delivery are not.
#![forbid(unsafe_code)]

use arcen_input::CapabilityAvailability;

use super::sample::{NativeTabletProximity, NativeTabletTool, RawTabletSample};

/// Wacom's USB vendor id (`WACOM_USB_VENDOR_ID`), also used by
/// `crate::hid::session::TABLET_VENDOR_IDS` and
/// `crate::logging::diagnostics::tablet_brand`.
pub const WACOM_USB_VENDOR_ID: u16 = 0x056A;

/// Query the existing cross-platform USB inventory for a Wacom-vendor
/// device. This is a presence check only — it says nothing about whether
/// AppKit has delivered any tablet event, and nothing about a human touching
/// the pen. `Unavailable` here is a fully deterministic negative ("no such
/// device is enumerated"), not a guess.
#[must_use]
pub fn wacom_usb_presence() -> CapabilityAvailability {
    match crate::logging::diagnostics::usb_inventory() {
        Ok(devices) => {
            if devices
                .iter()
                .any(|device| device.vendor_id == WACOM_USB_VENDOR_ID)
            {
                CapabilityAvailability::Available
            } else {
                CapabilityAvailability::Unavailable
            }
        }
        // Enumeration failure (subsystem/permission issue) is genuinely
        // unknown, not "no device": conflating the two would misreport a
        // permissions problem as "no tablet attached".
        Err(_) => CapabilityAvailability::Unknown,
    }
}

/// Empirically observed axis/tool capability, accumulated from a stream of
/// [`RawTabletSample`]s. Every field starts `Unknown` and only ever moves to
/// `Available` when a sample actually demonstrates the axis in use — never
/// synthesized, never inferred from silence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabletCapabilityProbe {
    pub pressure: CapabilityAvailability,
    pub tilt: CapabilityAvailability,
    pub rotation: CapabilityAvailability,
    pub eraser: CapabilityAvailability,
    pub barrel_buttons: CapabilityAvailability,
    /// Any proximity or point sample was observed at all — the minimal
    /// "AppKit is actually delivering tablet events" signal.
    pub any_tool_seen: CapabilityAvailability,
    points_observed: u64,
    proximity_observed: u64,
}

impl TabletCapabilityProbe {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn points_observed(&self) -> u64 {
        self.points_observed
    }

    #[must_use]
    pub const fn proximity_observed(&self) -> u64 {
        self.proximity_observed
    }

    /// Fold one delivered sample into the accumulated capability state.
    pub fn observe(&mut self, sample: RawTabletSample) {
        let tool = match sample {
            RawTabletSample::Point(point) => point.tool,
            RawTabletSample::Proximity(proximity) => proximity.tool,
        };
        if !tool.is_pen_or_eraser() {
            return;
        }
        self.any_tool_seen = CapabilityAvailability::Available;
        match sample {
            RawTabletSample::Point(point) => {
                self.points_observed = self.points_observed.saturating_add(1);
                if point.pressure > 0.0 {
                    self.pressure = CapabilityAvailability::Available;
                }
                if point.tilt_x != 0.0 || point.tilt_y != 0.0 {
                    self.tilt = CapabilityAvailability::Available;
                }
                if point.rotation_degrees != 0.0 {
                    self.rotation = CapabilityAvailability::Available;
                }
                if point.buttons.barrel_bits() != 0 {
                    self.barrel_buttons = CapabilityAvailability::Available;
                }
            }
            RawTabletSample::Proximity(proximity) => {
                self.proximity_observed = self.proximity_observed.saturating_add(1);
                self.observe_proximity_tool(proximity);
            }
        }
    }

    fn observe_proximity_tool(&mut self, proximity: NativeTabletProximity) {
        if proximity.tool == NativeTabletTool::Eraser {
            self.eraser = CapabilityAvailability::Available;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tablet::sample::{NativeTabletPoint, TabletButtonMask};

    fn point_sample(pressure: f32, tilt_x: f32, rotation: f32, buttons: u16) -> RawTabletSample {
        RawTabletSample::Point(NativeTabletPoint {
            window_x: 0.0,
            window_y: 0.0,
            pressure,
            tilt_x,
            tilt_y: 0.0,
            rotation_degrees: rotation,
            buttons: TabletButtonMask::new(buttons),
            device_id: 1,
            tool: NativeTabletTool::Pen,
            window_number: 1,
        })
    }

    fn proximity_sample(tool: NativeTabletTool) -> RawTabletSample {
        RawTabletSample::Proximity(NativeTabletProximity {
            window_x: 0.0,
            window_y: 0.0,
            entering: true,
            tool,
            vendor_id: WACOM_USB_VENDOR_ID as u64,
            tablet_id: 1,
            pointing_device_id: 1,
            system_tablet_id: 1,
            vendor_pointing_device_type: 0,
            unique_id: 1,
            capability_mask: 0,
            device_id: 1,
            window_number: 1,
        })
    }

    #[test]
    fn fresh_probe_claims_nothing() {
        let probe = TabletCapabilityProbe::new();
        assert_eq!(probe.pressure, CapabilityAvailability::Unknown);
        assert_eq!(probe.tilt, CapabilityAvailability::Unknown);
        assert_eq!(probe.rotation, CapabilityAvailability::Unknown);
        assert_eq!(probe.eraser, CapabilityAvailability::Unknown);
        assert_eq!(probe.barrel_buttons, CapabilityAvailability::Unknown);
        assert_eq!(probe.any_tool_seen, CapabilityAvailability::Unknown);
        assert_eq!(probe.points_observed(), 0);
    }

    #[test]
    fn unknown_trackpad_shaped_sample_claims_no_capability() {
        let mut probe = TabletCapabilityProbe::new();
        let RawTabletSample::Point(mut point) = point_sample(0.8, 0.5, 45.0, 0) else {
            unreachable!();
        };
        point.tool = NativeTabletTool::Unknown;
        probe.observe(RawTabletSample::Point(point));
        assert_eq!(probe.any_tool_seen, CapabilityAvailability::Unknown);
        assert_eq!(probe.pressure, CapabilityAvailability::Unknown);
        assert_eq!(probe.tilt, CapabilityAvailability::Unknown);
        assert_eq!(probe.rotation, CapabilityAvailability::Unknown);
        assert_eq!(probe.points_observed(), 0);
    }

    #[test]
    fn zero_pressure_point_does_not_claim_pressure_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(point_sample(0.0, 0.0, 0.0, 0));
        assert_eq!(probe.pressure, CapabilityAvailability::Unknown);
        // But the tool being seen at all is now established.
        assert_eq!(probe.any_tool_seen, CapabilityAvailability::Available);
        assert_eq!(probe.points_observed(), 1);
    }

    #[test]
    fn nonzero_pressure_establishes_pressure_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(point_sample(0.6, 0.0, 0.0, 0));
        assert_eq!(probe.pressure, CapabilityAvailability::Available);
    }

    #[test]
    fn nonzero_tilt_establishes_tilt_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(point_sample(0.0, 0.4, 0.0, 0));
        assert_eq!(probe.tilt, CapabilityAvailability::Available);
    }

    #[test]
    fn nonzero_rotation_establishes_rotation_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(point_sample(0.0, 0.0, 45.0, 0));
        assert_eq!(probe.rotation, CapabilityAvailability::Available);
    }

    #[test]
    fn barrel_bit_establishes_barrel_button_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(point_sample(0.0, 0.0, 0.0, TabletButtonMask::LOWER_SIDE));
        assert_eq!(probe.barrel_buttons, CapabilityAvailability::Available);
    }

    #[test]
    fn tip_bit_alone_does_not_establish_barrel_button_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(point_sample(0.0, 0.0, 0.0, TabletButtonMask::TIP));
        assert_eq!(probe.barrel_buttons, CapabilityAvailability::Unknown);
    }

    #[test]
    fn eraser_proximity_establishes_eraser_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(proximity_sample(NativeTabletTool::Eraser));
        assert_eq!(probe.eraser, CapabilityAvailability::Available);
        assert_eq!(probe.proximity_observed(), 1);
    }

    #[test]
    fn pen_proximity_alone_does_not_establish_eraser_capability() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(proximity_sample(NativeTabletTool::Pen));
        assert_eq!(probe.eraser, CapabilityAvailability::Unknown);
    }

    #[test]
    fn capabilities_accumulate_monotonically_across_many_samples() {
        let mut probe = TabletCapabilityProbe::new();
        probe.observe(point_sample(0.0, 0.0, 0.0, 0));
        probe.observe(point_sample(0.5, 0.0, 0.0, 0));
        probe.observe(point_sample(0.0, 0.0, 0.0, 0));
        assert_eq!(probe.pressure, CapabilityAvailability::Available);
        assert_eq!(probe.points_observed(), 3);
    }
}
