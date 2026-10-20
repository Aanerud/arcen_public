//! Pen samples as frames for an emulated-input stylus.
//!
//! Some hosts cannot create a pen device of their own and have to hand pen
//! input to the desktop's compositor through an emulated-input protocol. On
//! Linux Wayland that protocol is libei's `ei_stylus` interface (libei 1.7):
//! the compositor offers a stylus device and the capabilities it supports,
//! the client binds the ones it will send, and every change is delimited by
//! a frame. The protocol is strict, and a compositor may disconnect a client
//! that breaks it, so the rules live here, once, rather than in each adapter:
//!
//! * proximity-in and proximity-out never share a frame; a tool swap (tip to
//!   eraser) is the old tool leaving in one frame and the new one entering in
//!   the next, with everything the old tool held released first;
//! * the frame that brings a tool into proximity carries its position and a
//!   value for every bound axis;
//! * tip-down and tip-up never share a frame, nor do erase-start and
//!   erase-stop;
//! * with pressure bound, the pressure is above zero from the frame holding
//!   tip-down until tip-up;
//! * barrel buttons travel in the same frame as the stylus change they
//!   coincide with, and only the buttons the device can deliver are sent;
//! * an eraser cannot be sent as a tip: when the stylus cannot erase, an
//!   eraser sample is held out of proximity rather than drawn with.
//!
//! The press and release order itself comes from [`crate::plan_pen_edges`],
//! the same planner every other host uses. Axis values follow the
//! vendor-neutral [`crate::pen_axes`] contract, which is also the protocol's:
//! pressure `0..=1`, tilt in degrees `-90..=90`, rotation in degrees
//! `0..360`. A product adapter only turns each [`StylusOp`] into a protocol
//! request and maps coordinates into the stylus region.

use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

use crate::pen_axes::{TILT_DEGREES_FULL_SCALE, clamp_pressure, wrap_rotation_degrees};
use crate::{CapabilityAvailability, PenEdge, PenTool, PenToolState, plan_pen_edges};

/// Pressure sent while the tip is down when the sample reports none.
///
/// Contact implies pressure in the emulated-input protocol; a pen whose
/// firmware reports zero at the moment of contact still has to send a value
/// above zero. One step of a 16-bit axis is the smallest value every receiver
/// distinguishes from zero.
pub const MIN_CONTACT_PRESSURE: f32 = 1.0 / 65_535.0;

/// Most barrel buttons an emulated stylus can deliver: the three stylus
/// buttons Linux defines.
pub const MAX_STYLUS_BARREL_BUTTONS: u8 = 3;

/// Linux input event code of barrel button `index` (`BTN_STYLUS`,
/// `BTN_STYLUS2`, `BTN_STYLUS3`), the codes the emulated-input button
/// interface carries; `None` beyond [`MAX_STYLUS_BARREL_BUTTONS`].
#[must_use]
pub const fn barrel_button_code(index: u8) -> Option<u32> {
    match index {
        0 => Some(0x14b),
        1 => Some(0x14c),
        2 => Some(0x149),
        _ => None,
    }
}

/// The stylus capabilities a client bound on the device the compositor
/// offered. Only these may be sent.
///
/// One flag per protocol capability bit, named as the protocol names them.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StylusBinding {
    /// Tip pressure.
    #[serde(default)]
    pub pressure: bool,
    /// X/Y tilt.
    #[serde(default)]
    pub tilt: bool,
    /// Barrel rotation.
    #[serde(default)]
    pub rotation: bool,
    /// Eraser.
    #[serde(default)]
    pub erase: bool,
    /// Barrel buttons the device can deliver, `0..=3`: zero when the stylus
    /// device carries no button interface.
    #[serde(default)]
    pub barrel_buttons: u8,
}

impl StylusBinding {
    /// The buttons that can actually be delivered.
    #[must_use]
    pub const fn deliverable_buttons(self) -> u8 {
        if self.barrel_buttons > MAX_STYLUS_BARREL_BUTTONS {
            MAX_STYLUS_BARREL_BUTTONS
        } else {
            self.barrel_buttons
        }
    }
}

/// One pen sample, its position already in the stylus region's coordinates.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StylusSample {
    /// Horizontal position in the stylus region.
    pub x: f64,
    /// Vertical position in the stylus region.
    pub y: f64,
    /// Normalized pressure.
    pub pressure: f32,
    /// X tilt in degrees.
    pub tilt_x_degrees: f32,
    /// Y tilt in degrees.
    pub tilt_y_degrees: f32,
    /// Barrel rotation in degrees.
    pub rotation_degrees: f32,
    /// Tip or eraser.
    pub tool: PenTool,
    /// Within digitizer proximity.
    pub in_proximity: bool,
    /// Tip touching the surface.
    pub touching: bool,
    /// Barrel buttons, as a bitmask.
    pub buttons: u16,
}

/// One request of the emulated-input stylus protocol.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StylusOp {
    /// The tool entered proximity.
    ProximityIn,
    /// The tool left proximity.
    ProximityOut,
    /// Position in the stylus region.
    Motion {
        /// Horizontal.
        x: f32,
        /// Vertical.
        y: f32,
    },
    /// Pressure, `0..=1`.
    Pressure {
        /// Value.
        pressure: f32,
    },
    /// Tilt in degrees, `-90..=90`.
    Tilt {
        /// Along X.
        x: f32,
        /// Along Y.
        y: f32,
    },
    /// Rotation in degrees, `0..360`.
    Rotation {
        /// Value.
        degrees: f32,
    },
    /// The eraser became active.
    EraseStart,
    /// The eraser became inactive.
    EraseStop,
    /// Logical contact began.
    TipDown,
    /// Logical contact ended.
    TipUp,
    /// A barrel button changed.
    Button {
        /// Zero-based barrel button.
        index: u8,
        /// Held.
        pressed: bool,
    },
}

/// The requests of one protocol frame, in order.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StylusFrame {
    /// Requests, sent in order before the frame is closed.
    pub ops: Vec<StylusOp>,
}

impl StylusFrame {
    fn has(&self, wanted: StylusOp) -> bool {
        self.ops.contains(&wanted)
    }
}

/// Why a sample was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StylusSampleError {
    /// A coordinate or axis is not a finite number.
    NotFinite(&'static str),
}

impl Display for StylusSampleError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFinite(field) => write!(formatter, "pen {field} is not a finite number"),
        }
    }
}

impl std::error::Error for StylusSampleError {}

#[derive(Debug, Clone, Copy, PartialEq)]
struct SentAxes {
    x: f32,
    y: f32,
    pressure: f32,
    tilt: (f32, f32),
    rotation: f32,
}

/// Turns pen samples into protocol-valid stylus frames for one bound stylus.
#[derive(Debug, Clone)]
pub struct StylusEmulator {
    binding: StylusBinding,
    tool: PenToolState,
    sent: Option<SentAxes>,
    dropped_buttons: u64,
    withheld_erasers: u64,
}

impl StylusEmulator {
    /// A stylus out of proximity holding nothing.
    #[must_use]
    pub fn new(binding: StylusBinding) -> Self {
        Self {
            binding,
            tool: PenToolState::default(),
            sent: None,
            dropped_buttons: 0,
            withheld_erasers: 0,
        }
    }

    /// The bound capabilities.
    #[must_use]
    pub const fn binding(&self) -> StylusBinding {
        self.binding
    }

    /// Whether the stylus is in proximity, touching or holding a button.
    #[must_use]
    pub const fn holds_anything(&self) -> bool {
        self.tool.in_proximity
    }

    /// Barrel button edges the device could not deliver.
    #[must_use]
    pub const fn dropped_buttons(&self) -> u64 {
        self.dropped_buttons
    }

    /// Eraser samples held out of proximity because the stylus cannot erase.
    #[must_use]
    pub const fn withheld_erasers(&self) -> u64 {
        self.withheld_erasers
    }

    /// Plans the frames that bring the stylus to `sample`.
    ///
    /// # Errors
    ///
    /// [`StylusSampleError::NotFinite`] when a coordinate or axis is NaN or
    /// infinite; nothing is planned and the state is unchanged.
    pub fn plan(&mut self, sample: &StylusSample) -> Result<Vec<StylusFrame>, StylusSampleError> {
        check_finite(sample)?;
        let mut in_proximity = sample.in_proximity;
        if in_proximity && sample.tool == PenTool::Eraser && !self.binding.erase {
            // Drawing with the eraser end would mark what the user meant to
            // remove; the eraser stays away instead.
            self.withheld_erasers = self.withheld_erasers.saturating_add(1);
            in_proximity = false;
        }
        let axes = SentAxes {
            #[allow(clippy::cast_possible_truncation)]
            x: sample.x as f32,
            #[allow(clippy::cast_possible_truncation)]
            y: sample.y as f32,
            pressure: clamp_pressure(f64::from(sample.pressure)),
            tilt: (
                clamp_tilt(sample.tilt_x_degrees),
                clamp_tilt(sample.tilt_y_degrees),
            ),
            rotation: wrap_rotation_degrees(f64::from(sample.rotation_degrees)),
        };
        let (edges, next) = plan_pen_edges(
            self.tool,
            sample.tool,
            in_proximity,
            sample.touching,
            sample.buttons,
        );
        let frames = self.frames(&edges, next, Some(axes));
        self.tool = next;
        Ok(frames)
    }

    /// Plans the frames that release everything the stylus holds and take it
    /// out of proximity: for a connection that ends or is superseded.
    pub fn release(&mut self) -> Vec<StylusFrame> {
        let (edges, next) = plan_pen_edges(self.tool, self.tool.tool, false, false, 0);
        let frames = self.frames(&edges, next, None);
        self.tool = next;
        frames
    }

    fn frames(
        &mut self,
        edges: &[PenEdge],
        next: PenToolState,
        axes: Option<SentAxes>,
    ) -> Vec<StylusFrame> {
        let mut frames = Vec::new();
        let mut frame = StylusFrame::default();
        let mut entered = false;
        for edge in edges {
            match *edge {
                PenEdge::ToolOut(_) => {
                    // Leaving clears every stylus state on both sides, the
                    // eraser included.
                    frame.ops.push(StylusOp::ProximityOut);
                    self.sent = None;
                    frames.push(std::mem::take(&mut frame));
                }
                PenEdge::ToolIn(tool) => {
                    let Some(axes) = axes else { continue };
                    if frame.has(StylusOp::ProximityOut) {
                        frames.push(std::mem::take(&mut frame));
                    }
                    frame.ops.push(StylusOp::ProximityIn);
                    self.push_all_axes(&mut frame, axes, next.touching);
                    if tool == PenTool::Eraser && self.binding.erase {
                        frame.ops.push(StylusOp::EraseStart);
                    }
                    entered = true;
                }
                PenEdge::Barrel { index, pressed } => {
                    if index < self.binding.deliverable_buttons() {
                        frame.ops.push(StylusOp::Button { index, pressed });
                    } else {
                        self.dropped_buttons = self.dropped_buttons.saturating_add(1);
                    }
                }
                PenEdge::TipDown => {
                    if frame.has(StylusOp::TipUp) {
                        frames.push(std::mem::take(&mut frame));
                    }
                    frame.ops.push(StylusOp::TipDown);
                }
                PenEdge::TipUp => {
                    if frame.has(StylusOp::TipDown) {
                        frames.push(std::mem::take(&mut frame));
                    }
                    frame.ops.push(StylusOp::TipUp);
                }
            }
        }
        if next.in_proximity && !entered {
            if let Some(axes) = axes {
                self.push_changed_axes(&mut frame, axes, next.touching);
            }
        }
        if !frame.ops.is_empty() {
            frames.push(frame);
        }
        frames
    }

    fn push_all_axes(&mut self, frame: &mut StylusFrame, axes: SentAxes, touching: bool) {
        let pressure = contact_pressure(axes.pressure, touching);
        frame.ops.push(StylusOp::Motion {
            x: axes.x,
            y: axes.y,
        });
        if self.binding.pressure {
            frame.ops.push(StylusOp::Pressure { pressure });
        }
        if self.binding.tilt {
            frame.ops.push(StylusOp::Tilt {
                x: axes.tilt.0,
                y: axes.tilt.1,
            });
        }
        if self.binding.rotation {
            frame.ops.push(StylusOp::Rotation {
                degrees: axes.rotation,
            });
        }
        self.sent = Some(SentAxes { pressure, ..axes });
    }

    fn push_changed_axes(&mut self, frame: &mut StylusFrame, axes: SentAxes, touching: bool) {
        let Some(previous) = self.sent else {
            self.push_all_axes(frame, axes, touching);
            return;
        };
        let pressure = contact_pressure(axes.pressure, touching);
        if differs(axes.x, previous.x) || differs(axes.y, previous.y) {
            frame.ops.push(StylusOp::Motion {
                x: axes.x,
                y: axes.y,
            });
        }
        // The frame that makes contact always restates the pressure: the
        // value last sent may have been a hover's zero.
        if self.binding.pressure
            && (differs(pressure, previous.pressure) || frame.has(StylusOp::TipDown))
        {
            frame.ops.push(StylusOp::Pressure { pressure });
        }
        if self.binding.tilt
            && (differs(axes.tilt.0, previous.tilt.0) || differs(axes.tilt.1, previous.tilt.1))
        {
            frame.ops.push(StylusOp::Tilt {
                x: axes.tilt.0,
                y: axes.tilt.1,
            });
        }
        if self.binding.rotation && differs(axes.rotation, previous.rotation) {
            frame.ops.push(StylusOp::Rotation {
                degrees: axes.rotation,
            });
        }
        self.sent = Some(SentAxes { pressure, ..axes });
    }
}

/// Whether a value changed since it was sent: any different value is resent,
/// so the comparison is exact.
fn differs(value: f32, sent: f32) -> bool {
    value.to_bits() != sent.to_bits()
}

fn contact_pressure(pressure: f32, touching: bool) -> f32 {
    if touching && pressure < MIN_CONTACT_PRESSURE {
        MIN_CONTACT_PRESSURE
    } else {
        pressure
    }
}

fn clamp_tilt(degrees: f32) -> f32 {
    degrees.clamp(-TILT_DEGREES_FULL_SCALE, TILT_DEGREES_FULL_SCALE)
}

fn check_finite(sample: &StylusSample) -> Result<(), StylusSampleError> {
    let fields = [
        ("x", sample.x),
        ("y", sample.y),
        ("pressure", f64::from(sample.pressure)),
        ("tilt_x", f64::from(sample.tilt_x_degrees)),
        ("tilt_y", f64::from(sample.tilt_y_degrees)),
        ("rotation", f64::from(sample.rotation_degrees)),
    ];
    match fields.into_iter().find(|(_, value)| !value.is_finite()) {
        Some((field, _)) => Err(StylusSampleError::NotFinite(field)),
        None => Ok(()),
    }
}

/// What a host may truthfully advertise about its pen, axis by axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PenCapabilityTruth {
    /// A pen device exists.
    pub pen: CapabilityAvailability,
    /// Pressure reaches the desktop.
    pub pressure: CapabilityAvailability,
    /// Tilt reaches the desktop.
    pub tilt: CapabilityAvailability,
    /// Barrel rotation reaches the desktop.
    pub rotation: CapabilityAvailability,
    /// The eraser end reaches the desktop.
    pub eraser: CapabilityAvailability,
    /// Hover (proximity) reaches the desktop.
    pub proximity: CapabilityAvailability,
    /// Barrel buttons reach the desktop.
    pub barrel_buttons: CapabilityAvailability,
}

const fn availability(present: bool) -> CapabilityAvailability {
    if present {
        CapabilityAvailability::Available
    } else {
        CapabilityAvailability::Unavailable
    }
}

impl PenCapabilityTruth {
    /// No pen device.
    pub const UNAVAILABLE: Self = Self {
        pen: CapabilityAvailability::Unavailable,
        pressure: CapabilityAvailability::Unavailable,
        tilt: CapabilityAvailability::Unavailable,
        rotation: CapabilityAvailability::Unavailable,
        eraser: CapabilityAvailability::Unavailable,
        proximity: CapabilityAvailability::Unavailable,
        barrel_buttons: CapabilityAvailability::Unavailable,
    };

    /// A Linux uinput tablet-tool device: pressure, tilt, eraser, hover and
    /// two barrel buttons; no rotation axis (no target has proven one is
    /// recognised as tablet rotation).
    pub const LINUX_UINPUT_TABLET: Self = Self {
        pen: CapabilityAvailability::Available,
        pressure: CapabilityAvailability::Available,
        tilt: CapabilityAvailability::Available,
        rotation: CapabilityAvailability::Unavailable,
        eraser: CapabilityAvailability::Available,
        proximity: CapabilityAvailability::Available,
        barrel_buttons: CapabilityAvailability::Available,
    };

    /// `UNAVAILABLE` or `LINUX_UINPUT_TABLET`.
    #[must_use]
    pub const fn linux_uinput(tablet_device: bool) -> Self {
        if tablet_device {
            Self::LINUX_UINPUT_TABLET
        } else {
            Self::UNAVAILABLE
        }
    }

    /// What an emulated stylus with `binding` delivers.
    #[must_use]
    pub const fn emulated_stylus(binding: StylusBinding) -> Self {
        Self {
            pen: CapabilityAvailability::Available,
            pressure: availability(binding.pressure),
            tilt: availability(binding.tilt),
            rotation: availability(binding.rotation),
            eraser: availability(binding.erase),
            proximity: CapabilityAvailability::Available,
            barrel_buttons: availability(binding.deliverable_buttons() > 0),
        }
    }

    /// Whether a pen exists at all.
    #[must_use]
    pub const fn pen_available(self) -> bool {
        matches!(self.pen, CapabilityAvailability::Available)
    }
}

/// Why a session cannot receive an emulated stylus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StylusUnavailable {
    /// The compositor offers no emulated-input (EIS) connection at all.
    NoEmulatedInput,
    /// The compositor's emulated-input seat offers no stylus device.
    StylusNotOffered,
    /// The seat offers a stylus, but this host's emulated-input client
    /// cannot drive it yet.
    ClientLacksStylus,
    /// The emulated-input connection could not be established or was lost.
    ConnectionFailed,
    /// The session did not report a stylus probe: it was started by a host
    /// that predates emulated pen input.
    NotProbed,
    /// The session serves several displays, and region-scoped pen samples
    /// are not routed to an emulated stylus yet.
    MultiMonitor,
}

impl StylusUnavailable {
    /// Explanation shown to the user and recorded as evidence; fits the
    /// protocol's tablet-mode reason.
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::NoEmulatedInput => {
                "Tablet support is unavailable in this Wayland session: the compositor has \
                 no emulated-input (EIS) connection, which pen input needs (GNOME 45 or \
                 newer). Mouse compatibility is used."
            }
            Self::StylusNotOffered => {
                "Tablet support is unavailable in this Wayland session: the compositor's \
                 emulated-input (EIS) seat offers no stylus (libei 1.7 ei_stylus, not yet in \
                 GNOME). Mouse compatibility is used."
            }
            Self::ClientLacksStylus => {
                "Tablet support is unavailable in this Wayland session: the compositor \
                 offers a stylus, but this Pier cannot drive it yet. Mouse compatibility is \
                 used."
            }
            Self::ConnectionFailed => {
                "Tablet support is unavailable in this Wayland session: the compositor's \
                 emulated-input (EIS) connection failed. Mouse compatibility is used."
            }
            Self::NotProbed => {
                "Tablet support is unavailable in this Wayland session: it was started by an \
                 older Pier that did not probe for a stylus; sign out to restart it. Mouse \
                 compatibility is used."
            }
            Self::MultiMonitor => {
                "Tablet support is unavailable in this multi-display Wayland session: pen \
                 input is not yet routed to individual displays. Mouse compatibility is used."
            }
        }
    }
}

impl Display for StylusUnavailable {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.reason())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: StylusBinding = StylusBinding {
        pressure: true,
        tilt: true,
        rotation: true,
        erase: true,
        barrel_buttons: 3,
    };

    fn hover(x: f64) -> StylusSample {
        StylusSample {
            x,
            y: 20.0,
            pressure: 0.0,
            tilt_x_degrees: 10.0,
            tilt_y_degrees: -5.0,
            rotation_degrees: 45.0,
            tool: PenTool::Tip,
            in_proximity: true,
            touching: false,
            buttons: 0,
        }
    }

    fn away() -> StylusSample {
        StylusSample {
            in_proximity: false,
            ..hover(0.0)
        }
    }

    /// Every protocol rule the module promises, checked over one frame list.
    fn assert_protocol_valid(frames: &[StylusFrame], binding: StylusBinding) {
        for frame in frames {
            assert!(!frame.ops.is_empty(), "empty frame");
            let count = |op: StylusOp| frame.ops.iter().filter(|o| **o == op).count();
            assert!(
                !(frame.has(StylusOp::ProximityIn) && frame.has(StylusOp::ProximityOut)),
                "proximity in and out share {frame:?}"
            );
            assert!(
                !(frame.has(StylusOp::TipDown) && frame.has(StylusOp::TipUp)),
                "tip down and up share {frame:?}"
            );
            assert!(
                !(frame.has(StylusOp::EraseStart) && frame.has(StylusOp::EraseStop)),
                "erase start and stop share {frame:?}"
            );
            for op in [
                StylusOp::ProximityIn,
                StylusOp::ProximityOut,
                StylusOp::TipDown,
                StylusOp::TipUp,
            ] {
                assert!(count(op) <= 1, "{op:?} twice in {frame:?}");
            }
            if frame.has(StylusOp::ProximityIn) {
                assert!(
                    frame
                        .ops
                        .iter()
                        .any(|op| matches!(op, StylusOp::Motion { .. }))
                );
                assert_eq!(
                    binding.pressure,
                    frame
                        .ops
                        .iter()
                        .any(|op| matches!(op, StylusOp::Pressure { .. }))
                );
                assert_eq!(
                    binding.tilt,
                    frame
                        .ops
                        .iter()
                        .any(|op| matches!(op, StylusOp::Tilt { .. }))
                );
                assert_eq!(
                    binding.rotation,
                    frame
                        .ops
                        .iter()
                        .any(|op| matches!(op, StylusOp::Rotation { .. }))
                );
            }
            for op in &frame.ops {
                match op {
                    StylusOp::Pressure { .. } => assert!(binding.pressure),
                    StylusOp::Tilt { x, y } => {
                        assert!(binding.tilt);
                        assert!((-90.0..=90.0).contains(x) && (-90.0..=90.0).contains(y));
                    }
                    StylusOp::Rotation { degrees } => {
                        assert!(binding.rotation);
                        assert!((0.0..360.0).contains(degrees));
                    }
                    StylusOp::EraseStart | StylusOp::EraseStop => assert!(binding.erase),
                    StylusOp::Button { index, .. } => {
                        assert!(*index < binding.deliverable_buttons());
                    }
                    _ => {}
                }
            }
        }
    }

    /// Tracks the receiver's view: proximity, contact and the last pressure,
    /// and checks contact always carries pressure above zero.
    fn replay(frames: &[StylusFrame], binding: StylusBinding, state: &mut (bool, bool, f32)) {
        for frame in frames {
            for op in &frame.ops {
                match op {
                    StylusOp::ProximityIn => {
                        assert!(!state.0, "proximity in while in proximity");
                        state.0 = true;
                    }
                    StylusOp::ProximityOut => {
                        assert!(state.0, "proximity out while away");
                        *state = (false, false, 0.0);
                    }
                    StylusOp::TipDown => {
                        assert!(state.0 && !state.1);
                        state.1 = true;
                    }
                    StylusOp::TipUp => {
                        assert!(state.1);
                        state.1 = false;
                    }
                    StylusOp::Pressure { pressure } => state.2 = *pressure,
                    other => {
                        if !matches!(other, StylusOp::ProximityOut) {
                            assert!(state.0, "{other:?} while away");
                        }
                    }
                }
            }
            if binding.pressure && state.1 {
                assert!(state.2 > 0.0, "contact with zero pressure after {frame:?}");
            }
        }
    }

    #[test]
    fn entering_proximity_carries_position_and_every_bound_axis() {
        let mut stylus = StylusEmulator::new(ALL);
        let frames = stylus.plan(&hover(10.0)).unwrap();
        assert_eq!(
            frames,
            vec![StylusFrame {
                ops: vec![
                    StylusOp::ProximityIn,
                    StylusOp::Motion { x: 10.0, y: 20.0 },
                    StylusOp::Pressure { pressure: 0.0 },
                    StylusOp::Tilt { x: 10.0, y: -5.0 },
                    StylusOp::Rotation { degrees: 45.0 },
                ],
            }]
        );
    }

    #[test]
    fn unbound_axes_are_never_sent() {
        let binding = StylusBinding::default();
        let mut stylus = StylusEmulator::new(binding);
        let mut frames = stylus.plan(&hover(10.0)).unwrap();
        frames.extend(
            stylus
                .plan(&StylusSample {
                    touching: true,
                    pressure: 0.5,
                    tilt_x_degrees: 30.0,
                    ..hover(11.0)
                })
                .unwrap(),
        );
        assert_protocol_valid(&frames, binding);
        assert_eq!(
            frames[0].ops,
            vec![StylusOp::ProximityIn, StylusOp::Motion { x: 10.0, y: 20.0 }]
        );
    }

    #[test]
    fn contact_restates_pressure_and_never_sends_zero_while_down() {
        let mut stylus = StylusEmulator::new(ALL);
        let mut state = (false, false, 0.0);
        let frames = stylus.plan(&hover(10.0)).unwrap();
        replay(&frames, ALL, &mut state);
        // Contact reported with zero pressure, as some firmware does.
        let frames = stylus
            .plan(&StylusSample {
                touching: true,
                ..hover(10.0)
            })
            .unwrap();
        assert_eq!(
            frames,
            vec![StylusFrame {
                ops: vec![
                    StylusOp::TipDown,
                    StylusOp::Pressure {
                        pressure: MIN_CONTACT_PRESSURE
                    },
                ],
            }]
        );
        replay(&frames, ALL, &mut state);
        let frames = stylus
            .plan(&StylusSample {
                touching: true,
                pressure: 0.6,
                ..hover(12.0)
            })
            .unwrap();
        assert_eq!(
            frames[0].ops,
            vec![
                StylusOp::Motion { x: 12.0, y: 20.0 },
                StylusOp::Pressure { pressure: 0.6 },
            ]
        );
        replay(&frames, ALL, &mut state);
    }

    #[test]
    fn an_unchanged_sample_sends_nothing() {
        let mut stylus = StylusEmulator::new(ALL);
        stylus.plan(&hover(10.0)).unwrap();
        assert!(stylus.plan(&hover(10.0)).unwrap().is_empty());
    }

    #[test]
    fn leaving_releases_buttons_and_tip_then_leaves_in_one_frame() {
        let mut stylus = StylusEmulator::new(ALL);
        stylus
            .plan(&StylusSample {
                touching: true,
                pressure: 0.4,
                buttons: 0b01,
                ..hover(10.0)
            })
            .unwrap();
        let frames = stylus.plan(&away()).unwrap();
        assert_eq!(
            frames,
            vec![StylusFrame {
                ops: vec![
                    StylusOp::Button {
                        index: 0,
                        pressed: false
                    },
                    StylusOp::TipUp,
                    StylusOp::ProximityOut,
                ],
            }]
        );
        assert!(!stylus.holds_anything());
        assert!(stylus.plan(&away()).unwrap().is_empty());
    }

    #[test]
    fn a_tool_swap_is_two_frames_and_the_eraser_starts_on_entry() {
        let mut stylus = StylusEmulator::new(ALL);
        stylus.plan(&hover(10.0)).unwrap();
        let frames = stylus
            .plan(&StylusSample {
                tool: PenTool::Eraser,
                ..hover(10.0)
            })
            .unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].ops, vec![StylusOp::ProximityOut]);
        assert_eq!(frames[1].ops[0], StylusOp::ProximityIn);
        assert_eq!(frames[1].ops.last(), Some(&StylusOp::EraseStart));
        assert_protocol_valid(&frames, ALL);
    }

    #[test]
    fn an_eraser_is_withheld_when_the_stylus_cannot_erase() {
        let binding = StylusBinding {
            erase: false,
            ..ALL
        };
        let mut stylus = StylusEmulator::new(binding);
        let frames = stylus
            .plan(&StylusSample {
                tool: PenTool::Eraser,
                touching: true,
                pressure: 0.5,
                ..hover(10.0)
            })
            .unwrap();
        assert!(frames.is_empty());
        assert_eq!(stylus.withheld_erasers(), 1);
        assert!(!stylus.holds_anything());
    }

    #[test]
    fn undeliverable_buttons_are_dropped_and_counted() {
        let binding = StylusBinding {
            barrel_buttons: 0,
            ..ALL
        };
        let mut stylus = StylusEmulator::new(binding);
        let mut frames = stylus
            .plan(&StylusSample {
                buttons: 0b11,
                ..hover(10.0)
            })
            .unwrap();
        frames.extend(stylus.plan(&away()).unwrap());
        assert_protocol_valid(&frames, binding);
        assert_eq!(stylus.dropped_buttons(), 4);
    }

    #[test]
    fn non_finite_samples_are_refused_without_changing_state() {
        let mut stylus = StylusEmulator::new(ALL);
        let error = stylus
            .plan(&StylusSample {
                pressure: f32::NAN,
                ..hover(10.0)
            })
            .unwrap_err();
        assert_eq!(error, StylusSampleError::NotFinite("pressure"));
        assert!(!stylus.holds_anything());
        assert!(
            stylus
                .plan(&StylusSample {
                    x: f64::INFINITY,
                    ..hover(10.0)
                })
                .is_err()
        );
    }

    #[test]
    fn out_of_range_axes_are_clamped_and_rotation_wraps() {
        let mut stylus = StylusEmulator::new(ALL);
        let frames = stylus
            .plan(&StylusSample {
                pressure: 3.0,
                tilt_x_degrees: 120.0,
                tilt_y_degrees: -120.0,
                rotation_degrees: 360.0,
                ..hover(10.0)
            })
            .unwrap();
        assert!(frames[0].has(StylusOp::Pressure { pressure: 1.0 }));
        assert!(frames[0].has(StylusOp::Tilt { x: 90.0, y: -90.0 }));
        assert!(frames[0].has(StylusOp::Rotation { degrees: 0.0 }));
    }

    #[test]
    fn release_takes_a_held_stylus_away_and_is_then_empty() {
        let mut stylus = StylusEmulator::new(ALL);
        stylus
            .plan(&StylusSample {
                tool: PenTool::Eraser,
                touching: true,
                pressure: 0.3,
                buttons: 0b10,
                ..hover(10.0)
            })
            .unwrap();
        let frames = stylus.release();
        assert_eq!(
            frames,
            vec![StylusFrame {
                ops: vec![
                    StylusOp::Button {
                        index: 1,
                        pressed: false
                    },
                    StylusOp::TipUp,
                    StylusOp::ProximityOut,
                ],
            }]
        );
        assert!(stylus.release().is_empty());
    }

    /// Every transition between a set of representative samples, under every
    /// binding shape, yields frames the protocol accepts and a receiver state
    /// that matches the sample.
    #[test]
    fn every_transition_is_protocol_valid() {
        let bindings = [
            ALL,
            StylusBinding::default(),
            StylusBinding {
                erase: false,
                barrel_buttons: 1,
                ..ALL
            },
            StylusBinding {
                pressure: true,
                ..StylusBinding::default()
            },
        ];
        let mut samples = vec![away()];
        for tool in [PenTool::Tip, PenTool::Eraser] {
            for touching in [false, true] {
                for buttons in [0, 0b01, 0b110] {
                    for pressure in [0.0, 0.7] {
                        samples.push(StylusSample {
                            pressure,
                            tool,
                            touching,
                            buttons,
                            ..hover(5.0 + f64::from(buttons))
                        });
                    }
                }
            }
        }
        for binding in bindings {
            for first in &samples {
                for second in &samples {
                    let mut stylus = StylusEmulator::new(binding);
                    let mut state = (false, false, 0.0);
                    let mut all = Vec::new();
                    for sample in [first, second, first] {
                        let frames = stylus.plan(sample).unwrap();
                        replay(&frames, binding, &mut state);
                        all.extend(frames);
                        let visible =
                            sample.in_proximity && (sample.tool == PenTool::Tip || binding.erase);
                        assert_eq!(state.0, visible, "{sample:?} under {binding:?}");
                        assert_eq!(state.1, visible && sample.touching);
                    }
                    let frames = stylus.release();
                    replay(&frames, binding, &mut state);
                    all.extend(frames);
                    assert!(!state.0 && !state.1);
                    assert_protocol_valid(&all, binding);
                }
            }
        }
    }

    #[test]
    fn capability_truth_follows_what_was_bound() {
        assert!(!PenCapabilityTruth::UNAVAILABLE.pen_available());
        assert_eq!(
            PenCapabilityTruth::linux_uinput(true).rotation,
            CapabilityAvailability::Unavailable
        );
        let truth = PenCapabilityTruth::emulated_stylus(StylusBinding {
            pressure: true,
            tilt: false,
            rotation: true,
            erase: false,
            barrel_buttons: 0,
        });
        assert!(truth.pen_available());
        assert_eq!(truth.pressure, CapabilityAvailability::Available);
        assert_eq!(truth.tilt, CapabilityAvailability::Unavailable);
        assert_eq!(truth.rotation, CapabilityAvailability::Available);
        assert_eq!(truth.eraser, CapabilityAvailability::Unavailable);
        assert_eq!(truth.proximity, CapabilityAvailability::Available);
        assert_eq!(truth.barrel_buttons, CapabilityAvailability::Unavailable);
    }

    #[test]
    fn every_unavailable_reason_fits_the_tablet_mode_reason() {
        for reason in [
            StylusUnavailable::NoEmulatedInput,
            StylusUnavailable::StylusNotOffered,
            StylusUnavailable::ClientLacksStylus,
            StylusUnavailable::ConnectionFailed,
            StylusUnavailable::NotProbed,
            StylusUnavailable::MultiMonitor,
        ] {
            assert!(reason.reason().len() <= 240, "{reason:?}");
            assert!(reason.reason().contains("Mouse compatibility"));
        }
    }

    #[test]
    fn barrel_buttons_map_to_the_stylus_button_codes() {
        assert_eq!(barrel_button_code(0), Some(0x14b));
        assert_eq!(barrel_button_code(1), Some(0x14c));
        assert_eq!(barrel_button_code(2), Some(0x149));
        assert_eq!(barrel_button_code(3), None);
    }
}
