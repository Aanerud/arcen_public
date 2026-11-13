// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Where a host-driven pen points: the one Native tablet position path for
//! every Pier (ADR 0023).
//!
//! A USB-bridged tablet (Native tablet) is driven on the host — by the
//! kernel's `wacom` driver on Linux, by the Pier's own pen reader on macOS —
//! so only the host knows where the pen is. Every Pier reports it the same
//! way, and its platform code only produces samples:
//!
//! 1. A platform adapter turns what it reads into a [`PenSample`]: device
//!    units with their ranges, the tool in proximity, the time it was read.
//!    The Linux adapter reads the bridged pen's own evdev node (X11 and
//!    Wayland alike) through [`EvdevPenFrames`]; the macOS adapter converts
//!    the shared Intuos pen reader's packets. A Windows adapter would read
//!    its pen the same way (follow-up: Windows has no Native tablet, ADR
//!    0017).
//! 2. [`PenPointerLead`] is the one policy: while a pen is in proximity and
//!    the desktop maps the whole tablet to the whole desktop
//!    ([`PenMapping::WholeArea`]), the pen leads and its position is the
//!    host pointer's, mapped here ([`map_to_output`]); otherwise the
//!    platform's own pointer source leads (the X pointer, the compositor's
//!    cursor metadata), which is still needed for application warps, a
//!    remapped tablet and every session without a bridged pen.
//! 3. What the lead publishes is a
//!    [`DesktopPointerSample`](crate::host_pointer::DesktopPointerSample),
//!    fed to the shared [`HostPointerFeed`](crate::host_pointer::HostPointerFeed)
//!    and sent as `CursorPosition`; the Deck draws the pen mark there
//!    ([`led_pointer_mark`](crate::host_pointer::led_pointer_mark)).
//!
//! Positions are pushed as each pen report arrives; nothing polls for them.
//! That is what made the pen feel immediate: an X server's pointer, polled
//! every 8 ms after xf86-input-wacom processed the event, lagged visibly.

use std::sync::{Mutex, PoisonError};
use std::time::Instant;

use crate::host_pointer::DesktopPointerSample;

/// `EV_SYN`.
pub const EV_SYN: u16 = 0x00;
/// `EV_KEY`.
pub const EV_KEY: u16 = 0x01;
/// `EV_ABS`.
pub const EV_ABS: u16 = 0x03;
/// `SYN_REPORT`.
pub const SYN_REPORT: u16 = 0;
/// `SYN_DROPPED`.
pub const SYN_DROPPED: u16 = 3;
/// `ABS_X`.
pub const ABS_X: u16 = 0x00;
/// `ABS_Y`.
pub const ABS_Y: u16 = 0x01;
/// `BTN_TOOL_PEN`.
pub const BTN_TOOL_PEN: u16 = 0x140;
/// `BTN_TOOL_RUBBER`.
pub const BTN_TOOL_RUBBER: u16 = 0x141;

/// The `BTN_TOOL_*` codes of tablet tools: pen, rubber, brush, pencil,
/// airbrush, mouse and lens (not finger, which is touch).
pub const TABLET_TOOL_CODES: [u16; 7] = [
    BTN_TOOL_PEN,
    BTN_TOOL_RUBBER,
    0x142,
    0x143,
    0x144,
    0x146,
    0x147,
];

/// One absolute axis's range, inclusive (as `EVIOCGABS` reports it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisRange {
    pub minimum: i32,
    pub maximum: i32,
}

impl AxisRange {
    /// Where `value` lies on the axis, `0.0..1.0`, clamped. This is how
    /// libinput and the X server scale an absolute axis onto a screen:
    /// `(value - minimum) / (maximum - minimum + 1)`.
    #[must_use]
    pub fn fraction(self, value: i32) -> f64 {
        let span = f64::from(self.maximum) - f64::from(self.minimum) + 1.0;
        ((f64::from(value) - f64::from(self.minimum)) / span).clamp(0.0, 1.0)
    }

    /// Whether the range holds more than one value.
    #[must_use]
    pub const fn is_usable(self) -> bool {
        self.maximum > self.minimum
    }
}

/// Which tablet tool is in proximity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenTool {
    /// A pen tip.
    Pen,
    /// A pen's eraser end.
    Eraser,
    /// Any other tablet tool (brush, pencil, airbrush, puck, lens).
    Other,
}

/// One pen sample, as every platform adapter produces it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PenSample {
    /// The tool in proximity; `None` when none is.
    pub tool: Option<PenTool>,
    /// Column in device units.
    pub x: i32,
    /// Row in device units.
    pub y: i32,
    /// The device's X range.
    pub x_range: AxisRange,
    /// The device's Y range.
    pub y_range: AxisRange,
    /// When the adapter read it.
    pub at: Instant,
}

impl PenSample {
    /// Whether a tool is in proximity.
    #[must_use]
    pub const fn in_proximity(&self) -> bool {
        self.tool.is_some()
    }

    /// Where the tool is, as a fraction of the tablet's area.
    #[must_use]
    pub fn fraction(&self) -> (f64, f64) {
        (self.x_range.fraction(self.x), self.y_range.fraction(self.y))
    }
}

/// Accumulates one evdev tablet node's events into [`PenSample`]s: one per
/// `SYN_REPORT` that changed the tool or its position, and one when the
/// last tool leaves. After the kernel dropped events (`SYN_DROPPED`) the
/// state must be read again from the device ([`Self::needs_resync`],
/// [`Self::resync`]), as libevdev does.
#[derive(Debug, Clone)]
pub struct EvdevPenFrames {
    x_range: AxisRange,
    y_range: AxisRange,
    x: i32,
    y: i32,
    /// Bit `i` set while `TABLET_TOOL_CODES[i]` is down.
    tools: u8,
    /// Events after a `SYN_DROPPED` up to the next `SYN_REPORT` are lost.
    dropped: bool,
    /// The frame that ended a drop arrived: the state must be read again
    /// from the device before it is trusted.
    resync: bool,
    last: Option<(Option<PenTool>, i32, i32)>,
}

impl EvdevPenFrames {
    /// Frames for a node with these axis ranges and current values; `None`
    /// when a range is empty.
    #[must_use]
    pub fn new(x_range: AxisRange, y_range: AxisRange, x: i32, y: i32) -> Option<Self> {
        (x_range.is_usable() && y_range.is_usable()).then_some(Self {
            x_range,
            y_range,
            x,
            y,
            tools: 0,
            dropped: false,
            resync: false,
            last: None,
        })
    }

    /// The device's ranges.
    #[must_use]
    pub const fn ranges(&self) -> (AxisRange, AxisRange) {
        (self.x_range, self.y_range)
    }

    /// Notes a tool already in proximity when the node was opened
    /// (`EVIOCGKEY`).
    pub fn set_tool_down(&mut self, code: u16) {
        if let Some(bit) = tool_bit(code) {
            self.tools |= bit;
        }
    }

    /// Whether the last sample placed a tool in proximity.
    #[must_use]
    pub fn in_proximity(&self) -> bool {
        matches!(self.last, Some((Some(_), _, _)))
    }

    /// Whether the kernel dropped events and the frame that ended the drop
    /// has arrived: read the device's key and axis state and pass it to
    /// [`Self::resync`] before applying more events.
    #[must_use]
    pub const fn needs_resync(&self) -> bool {
        self.resync
    }

    /// Rebuilds the state from the device's current key state (`tools_down`:
    /// every `BTN_TOOL_*` code that is down, `EVIOCGKEY`) and axis values
    /// (`EVIOCGABS`), and returns the sample the rebuilt frame makes.
    pub fn resync(
        &mut self,
        tools_down: impl IntoIterator<Item = u16>,
        x: i32,
        y: i32,
        at: Instant,
    ) -> Option<PenSample> {
        self.resync = false;
        self.dropped = false;
        self.tools = 0;
        for code in tools_down {
            self.set_tool_down(code);
        }
        self.x = x;
        self.y = y;
        self.frame(at)
    }

    /// Applies one event read at `at`; a `SYN_REPORT` that changed the tool
    /// or where it is, or that ended its proximity, returns the sample. The
    /// `SYN_REPORT` that ends a `SYN_DROPPED` returns nothing and sets
    /// [`Self::needs_resync`].
    pub fn apply(&mut self, kind: u16, code: u16, value: i32, at: Instant) -> Option<PenSample> {
        match (kind, code) {
            (EV_SYN, SYN_DROPPED) => {
                self.dropped = true;
                None
            }
            (EV_SYN, SYN_REPORT) => {
                if std::mem::take(&mut self.dropped) {
                    self.resync = true;
                    return None;
                }
                self.frame(at)
            }
            _ if self.dropped => None,
            (EV_ABS, ABS_X) => {
                self.x = value;
                None
            }
            (EV_ABS, ABS_Y) => {
                self.y = value;
                None
            }
            (EV_KEY, code) => {
                if let Some(bit) = tool_bit(code) {
                    if value == 0 {
                        self.tools &= !bit;
                    } else {
                        self.tools |= bit;
                    }
                }
                None
            }
            _ => None,
        }
    }

    fn frame(&mut self, at: Instant) -> Option<PenSample> {
        let tool = self.tool();
        if tool.is_none() && !self.in_proximity() {
            return None;
        }
        let state = (tool, self.x, self.y);
        if self.last == Some(state) {
            return None;
        }
        self.last = Some(state);
        Some(PenSample {
            tool,
            x: self.x,
            y: self.y,
            x_range: self.x_range,
            y_range: self.y_range,
            at,
        })
    }

    fn tool(&self) -> Option<PenTool> {
        if self.tools == 0 {
            None
        } else if self.tools & tool_bit(BTN_TOOL_PEN).unwrap_or(0) != 0 {
            Some(PenTool::Pen)
        } else if self.tools & tool_bit(BTN_TOOL_RUBBER).unwrap_or(0) != 0 {
            Some(PenTool::Eraser)
        } else {
            Some(PenTool::Other)
        }
    }
}

fn tool_bit(code: u16) -> Option<u8> {
    TABLET_TOOL_CODES
        .iter()
        .position(|tool| *tool == code)
        .map(|index| 1_u8 << index)
}

/// The pixel of a `width` × `height` desktop a tool at fraction `(x, y)` of
/// the tablet points at, when the whole tablet area maps to the whole
/// desktop: `floor(fraction × size)`, as the X server and Mutter place it.
#[must_use]
pub fn map_to_output(x: f64, y: f64, width: u32, height: u32) -> (i32, i32) {
    let side = |fraction: f64, length: u32| {
        let last = f64::from(length.max(1) - 1);
        // In range by the clamp; `as` cannot truncate a value this small.
        #[allow(clippy::cast_possible_truncation)]
        let pixel = (fraction * f64::from(length)).floor().clamp(0.0, last) as i32;
        pixel
    };
    (side(x, width), side(y, height))
}

/// How the desktop maps the tablet's area onto itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenMapping {
    /// The whole tablet area onto the whole desktop, absolute: the pen's
    /// position is computed here.
    WholeArea,
    /// Not known yet (the desktop has not added the pen): the platform's
    /// pointer leads.
    Unknown,
    /// Remapped by the user or the desktop: the platform's pointer leads,
    /// because only the desktop knows where the pen lands.
    Remapped(PenRemap),
}

/// What makes a mapping other than the whole area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenRemap {
    /// The tablet area is a part of the tablet ("Wacom Tablet Area").
    TabletArea,
    /// A coordinate transformation other than identity (an output mapping,
    /// kept aspect).
    Transform,
    /// The tablet is rotated ("Wacom Rotation").
    Rotation,
    /// The tool moves the pointer relatively (mouse mode).
    Relative,
}

/// What an Xorg desktop's `xf86-input-wacom` reports of one stylus or
/// eraser device of the pen, read by the platform from the device's
/// properties (`None`: the property is missing or unreadable).
#[derive(Debug, Clone, PartialEq)]
pub struct X11WacomTool {
    /// "Wacom Tablet Area": top-left and bottom-right in device units.
    pub tablet_area: Option<[i64; 4]>,
    /// "Wacom Rotation" (an 8-bit property): 0 none.
    pub rotation: Option<i64>,
    /// "Coordinate Transformation Matrix", row-major.
    pub transform: Option<[f32; 9]>,
    /// The X valuator mode is absolute.
    pub absolute: bool,
}

/// The mapping an Xorg desktop applies to a pen whose evdev ranges are
/// `x_range` and `y_range`, from every stylus and eraser device the desktop
/// made of it. `xf86-input-wacom`'s defaults are the whole device range as
/// the tablet area, no rotation, an identity transform and absolute mode;
/// anything else is a remap, and the X pointer leads. A property that could
/// not be read proves nothing: [`PenMapping::Unknown`].
#[must_use]
pub fn x11_wacom_mapping(
    tools: &[X11WacomTool],
    x_range: AxisRange,
    y_range: AxisRange,
) -> PenMapping {
    const IDENTITY: [f32; 9] = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0];
    if tools.is_empty() {
        return PenMapping::Unknown;
    }
    let whole = [
        i64::from(x_range.minimum),
        i64::from(y_range.minimum),
        i64::from(x_range.maximum),
        i64::from(y_range.maximum),
    ];
    for tool in tools {
        let (Some(area), Some(rotation), Some(transform)) =
            (tool.tablet_area, tool.rotation, tool.transform)
        else {
            return PenMapping::Unknown;
        };
        if !tool.absolute {
            return PenMapping::Remapped(PenRemap::Relative);
        }
        if area != whole {
            return PenMapping::Remapped(PenRemap::TabletArea);
        }
        if rotation != 0 {
            return PenMapping::Remapped(PenRemap::Rotation);
        }
        if transform
            .iter()
            .zip(IDENTITY)
            .any(|(value, identity)| (value - identity).abs() > 1e-6)
        {
            return PenMapping::Remapped(PenRemap::Transform);
        }
    }
    PenMapping::WholeArea
}

/// Which source leads the host pointer: the pen, or the platform's own
/// pointer (one per desktop pointer, shared by its adapters).
///
/// - A pen or eraser in proximity leads when the mapping is
///   [`PenMapping::WholeArea`] and the desktop's size is known; it is
///   published at once. Any other tool (a puck or lens, relative by default
///   in `xf86-input-wacom`) never leads.
/// - While the pen leads, the platform's samples are only remembered.
/// - When the pen leaves (or its reader ends), nothing is published: the
///   cursor stays where the pen left it, and the platform's next change is
///   published again.
/// - A mapping that stops being the whole area hands the pointer back to
///   the platform at once (its last sample is published).
/// - Nothing is published twice in a row.
#[derive(Debug, Clone)]
pub struct PenPointerLead {
    mapping: PenMapping,
    desktop: Option<(u32, u32)>,
    pen: Option<PenSample>,
    ranges: Option<(AxisRange, AxisRange)>,
    leads: bool,
    platform: Option<DesktopPointerSample>,
    published: Option<(i32, i32, u32, u32)>,
}

impl PenPointerLead {
    /// A lead for a desktop whose mapping is `mapping`.
    #[must_use]
    pub const fn new(mapping: PenMapping) -> Self {
        Self {
            mapping,
            desktop: None,
            pen: None,
            ranges: None,
            leads: false,
            platform: None,
            published: None,
        }
    }

    /// Whether the pen leads.
    #[must_use]
    pub const fn pen_leads(&self) -> bool {
        self.leads
    }

    /// The mapping in force.
    #[must_use]
    pub const fn mapping(&self) -> PenMapping {
        self.mapping
    }

    /// The ranges of the last pen sample.
    #[must_use]
    pub const fn pen_ranges(&self) -> Option<(AxisRange, AxisRange)> {
        self.ranges
    }

    /// The desktop is `width` × `height` pixels (a resize).
    pub fn set_desktop_size(&mut self, width: u32, height: u32) -> Option<DesktopPointerSample> {
        if width == 0 || height == 0 || self.desktop == Some((width, height)) {
            return None;
        }
        self.desktop = Some((width, height));
        self.place_pen()
    }

    /// The desktop's mapping of the tablet is now `mapping`.
    pub fn set_mapping(&mut self, mapping: PenMapping) -> Option<DesktopPointerSample> {
        if self.mapping == mapping {
            return None;
        }
        self.mapping = mapping;
        if mapping == PenMapping::WholeArea {
            return self.place_pen();
        }
        if std::mem::take(&mut self.leads) {
            return self.platform.and_then(|sample| self.publish(sample));
        }
        None
    }

    /// A sample from the pen adapter.
    pub fn pen(&mut self, sample: PenSample) -> Option<DesktopPointerSample> {
        self.ranges = Some((sample.x_range, sample.y_range));
        if !matches!(sample.tool, Some(PenTool::Pen | PenTool::Eraser)) {
            self.pen = None;
            self.leads = false;
            return None;
        }
        self.pen = Some(sample);
        self.place_pen()
    }

    /// The pen adapter ended (the attachment or the device went away).
    pub fn pen_ended(&mut self) {
        self.pen = None;
        self.leads = false;
        self.ranges = None;
    }

    /// A sample from the platform's own pointer source.
    pub fn platform(&mut self, sample: DesktopPointerSample) -> Option<DesktopPointerSample> {
        if sample.desktop_width > 0 && sample.desktop_height > 0 {
            self.desktop = Some((sample.desktop_width, sample.desktop_height));
        }
        self.platform = Some(sample);
        if self.leads {
            return None;
        }
        self.publish(sample)
    }

    fn place_pen(&mut self) -> Option<DesktopPointerSample> {
        let pen = self.pen?;
        let (width, height) = self.desktop?;
        if self.mapping != PenMapping::WholeArea {
            self.leads = false;
            return None;
        }
        self.leads = true;
        let (fx, fy) = pen.fraction();
        let (x, y) = map_to_output(fx, fy, width, height);
        self.publish(DesktopPointerSample {
            x,
            y,
            desktop_width: width,
            desktop_height: height,
            at: pen.at,
        })
    }

    fn publish(&mut self, sample: DesktopPointerSample) -> Option<DesktopPointerSample> {
        let key = (
            sample.x,
            sample.y,
            sample.desktop_width,
            sample.desktop_height,
        );
        (self.published.replace(key) != Some(key)).then_some(sample)
    }
}

/// The bridged pen device a desktop's mapping is checked against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenDevice {
    /// The platform's device number (Linux: the evdev node's major, minor).
    pub device_number: (u64, u64),
    pub x_range: AxisRange,
    pub y_range: AxisRange,
}

/// One desktop's pointer, shared by the threads that feed it: the pen
/// adapter and the platform's own pointer source. Every decision is the
/// [`PenPointerLead`]'s, taken under one lock and published through `sink`
/// under that lock, so samples are published in the order they were decided.
pub struct PenPointerHub {
    lead: Mutex<PenPointerLead>,
    pen_device: Mutex<Option<PenDevice>>,
    sink: Box<dyn Fn(DesktopPointerSample) + Send + Sync>,
}

impl std::fmt::Debug for PenPointerHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PenPointerHub")
            .field("lead", &self.lead)
            .field("pen_device", &self.pen_device)
            .finish_non_exhaustive()
    }
}

impl PenPointerHub {
    /// A hub for a desktop that maps a tablet as `mapping`, publishing to
    /// `sink`.
    #[must_use]
    pub fn new(
        mapping: PenMapping,
        sink: impl Fn(DesktopPointerSample) + Send + Sync + 'static,
    ) -> Self {
        Self {
            lead: Mutex::new(PenPointerLead::new(mapping)),
            pen_device: Mutex::new(None),
            sink: Box::new(sink),
        }
    }

    fn decide(
        &self,
        step: impl FnOnce(&mut PenPointerLead) -> Option<DesktopPointerSample>,
    ) -> Option<DesktopPointerSample> {
        let mut lead = self.lead.lock().unwrap_or_else(PoisonError::into_inner);
        let published = step(&mut lead);
        if let Some(sample) = published {
            (self.sink)(sample);
        }
        published
    }

    /// A sample of the platform's own pointer.
    pub fn platform(&self, sample: DesktopPointerSample) {
        self.decide(|lead| lead.platform(sample));
    }

    /// A sample of the bridged pen; what was published.
    pub fn pen(&self, sample: PenSample) -> Option<DesktopPointerSample> {
        self.decide(|lead| lead.pen(sample))
    }

    /// The reader of the pen `device_number` ended. A later reader of
    /// another device (a newer attachment of the same desktop) is left
    /// alone.
    pub fn pen_ended(&self, device_number: (u64, u64)) {
        let mut device = self
            .pen_device
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if device.is_some_and(|device| device.device_number != device_number) {
            return;
        }
        *device = None;
        self.decide(|lead| {
            lead.pen_ended();
            None
        });
    }

    /// The desktop's size in pixels.
    pub fn set_desktop_size(&self, width: u32, height: u32) {
        self.decide(|lead| lead.set_desktop_size(width, height));
    }

    /// The desktop's mapping of the tablet.
    pub fn set_mapping(&self, mapping: PenMapping) {
        self.decide(|lead| lead.set_mapping(mapping));
    }

    /// Whether the pen leads (the platform's pointer need not be read).
    #[must_use]
    pub fn pen_leads(&self) -> bool {
        self.lead
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pen_leads()
    }

    /// The mapping in force.
    #[must_use]
    pub fn mapping(&self) -> PenMapping {
        self.lead
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .mapping()
    }

    /// Notes the bridged pen device being read.
    pub fn note_pen_device(&self, device: PenDevice) {
        *self
            .pen_device
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(device);
    }

    /// The bridged pen device being read, if any.
    #[must_use]
    pub fn pen_device(&self) -> Option<PenDevice> {
        *self
            .pen_device
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
#[path = "tablet_pointer_tests.rs"]
mod tests;
