//! Vendor-neutral pen-tablet classification from HID report descriptors.
//!
//! A drawing tablet is recognised by what its HID report descriptor says it
//! is, never by who made it. A vendor id is only a claim, and a list of vendor
//! ids is a list of the tablets someone happened to own. The descriptor is the
//! device's own statement of its input surface, and the HID Usage Tables give
//! that statement a vendor-independent vocabulary: Digitizers page `0x0D`,
//! Pen `0x02`, Tip Pressure `0x30`, In Range `0x32`, X/Y Tilt `0x3D`/`0x3E`,
//! Twist `0x41`, Barrel Switch `0x44`, Eraser `0x45`.
//!
//! The classifier is deliberately conservative and fails closed. It is the
//! gate in front of a host's kernel-facing virtual-device boundary
//! (`/dev/uhid`, `usb-vhci`), so a keyboard, mouse, gamepad, storage device or
//! vendor-only device must never pass:
//!
//! * A device is a pen tablet when one of its top-level collections is a
//!   standard Digitizers-page pen (application usage Pen or Digitizer, or any
//!   digitizer collection with a nested Stylus) that reports X and Y and at
//!   least one of Tip Switch, Tip Pressure or In Range.
//! * `0xFF0D` is the vendor-defined Digitizers page: its usage ids mirror the
//!   Digitizers page (`0x01` Digitizer, `0x02` Pen, ...), which is how the
//!   Linux HID stack treats it too. A top-level application collection on
//!   `0xFF0D` whose usage is Digitizer or Pen is a pen tablet even when its
//!   reports are opaque vendor bytes, provided the same device publishes no
//!   joystick, gamepad or multi-axis controller collection (game controllers
//!   are not tablet functions). Keyboard and keypad collections are allowed,
//!   as for standard pens: they are how many tablets present express keys.
//!   Its axes are then unknown: nothing is invented.
//! * Any other `0xFF0D` collection counts only together with a standard
//!   Digitizers-page (`0x0D`) collection on the same device. Every other
//!   vendor-defined page (`0xFF00`, ...) never makes a device a tablet.
//! * Touch-only digitizers (touch screens and touch pads without a stylus)
//!   are rejected: Arcen bridges pens, not multi-touch.
//!
//! Parsing is bounded: descriptors above
//! [`MAX_HID_REPORT_DESCRIPTOR_BYTES`], collection nesting deeper than
//! [`MAX_COLLECTION_DEPTH`], more than [`MAX_TOP_LEVEL_COLLECTIONS`] top-level
//! collections, or more than [`MAX_LOCAL_USAGES`] usages on one main item are
//! refused rather than truncated.

use crate::{MAX_INTERFACES, SetupPacket};
use std::fmt::{Display, Formatter};

/// Largest HID report descriptor accepted from any peer or device.
///
/// Matches the raw-HID wire bound (`MAX_HID_DESCRIPTOR_LEN`) and Linux's
/// `HID_MAX_DESCRIPTOR_SIZE`.
pub const MAX_HID_REPORT_DESCRIPTOR_BYTES: usize = 4096;
/// Deepest collection nesting accepted.
pub const MAX_COLLECTION_DEPTH: usize = 32;
/// Most top-level collections accepted in one descriptor.
pub const MAX_TOP_LEVEL_COLLECTIONS: usize = 64;
/// Most local usages accepted on one main item.
pub const MAX_LOCAL_USAGES: usize = 256;
/// Most `Push` levels accepted on the global-state stack.
pub const MAX_GLOBAL_STACK_DEPTH: usize = 16;

/// HID Usage Page: Generic Desktop.
pub const USAGE_PAGE_GENERIC_DESKTOP: u16 = 0x01;
/// HID Usage Page: Button.
pub const USAGE_PAGE_BUTTON: u16 = 0x09;
/// HID Usage Page: Digitizers.
pub const USAGE_PAGE_DIGITIZER: u16 = 0x0D;
/// Vendor-defined page that mirrors the Digitizers page in its low byte.
///
/// Some tablets publish their pen collection on this vendor page and reuse the
/// Digitizers usage numbers inside it. It is a page-numbering convention, not
/// a vendor identity; any manufacturer may use it.
pub const USAGE_PAGE_VENDOR_DIGITIZER: u16 = 0xFF0D;
/// First vendor-defined usage page.
pub const USAGE_PAGE_VENDOR_DEFINED_MIN: u16 = 0xFF00;

const USAGE_X: u16 = 0x30;
const USAGE_Y: u16 = 0x31;
const DIGITIZER_DIGITIZER: u16 = 0x01;
const DIGITIZER_PEN: u16 = 0x02;
const DIGITIZER_TOUCH_SCREEN: u16 = 0x04;
const DIGITIZER_TOUCH_PAD: u16 = 0x05;
const DIGITIZER_STYLUS: u16 = 0x20;
const DIGITIZER_FINGER: u16 = 0x22;
const DIGITIZER_TIP_PRESSURE: u16 = 0x30;
const DIGITIZER_IN_RANGE: u16 = 0x32;
const DIGITIZER_X_TILT: u16 = 0x3D;
const DIGITIZER_Y_TILT: u16 = 0x3E;
const DIGITIZER_TWIST: u16 = 0x41;
const DIGITIZER_TIP_SWITCH: u16 = 0x42;
const DIGITIZER_BARREL_SWITCH: u16 = 0x44;
const DIGITIZER_ERASER: u16 = 0x45;
const DIGITIZER_INVERT: u16 = 0x3C;
const DIGITIZER_SECONDARY_BARREL_SWITCH: u16 = 0x5A;
const DIGITIZER_CONTACT_IDENTIFIER: u16 = 0x51;

/// Signed logical/physical extent of one reported axis, as the descriptor
/// declared it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AxisRange {
    /// Logical minimum.
    pub logical_min: i64,
    /// Logical maximum.
    pub logical_max: i64,
    /// Physical minimum (zero when undeclared).
    pub physical_min: i64,
    /// Physical maximum (zero when undeclared).
    pub physical_max: i64,
    /// Raw HID unit word.
    pub unit: u32,
    /// HID unit exponent, already sign-decoded (`-8..=7`).
    pub unit_exponent: i8,
}

impl AxisRange {
    /// Number of distinct logical steps, saturating at `u64::MAX`.
    #[must_use]
    pub fn levels(self) -> u64 {
        let span = i128::from(self.logical_max) - i128::from(self.logical_min) + 1;
        u64::try_from(span.max(0)).unwrap_or(u64::MAX)
    }

    /// Whether the logical range is non-empty.
    #[must_use]
    pub const fn is_valid(self) -> bool {
        self.logical_max > self.logical_min
    }

    /// Whether the physical extent is declared in degrees (HID English
    /// Rotation system, unit `0x14`).
    #[must_use]
    pub const fn physical_unit_is_degrees(self) -> bool {
        self.unit == 0x14 && self.physical_max != self.physical_min
    }
}

/// Pen surface a classified descriptor declares.
///
/// `None`/`false`/`0` mean "the descriptor does not declare it", never "the
/// pen cannot do it": a vendor-mirrored collection may carry axes in opaque
/// vendor bytes this parser cannot see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PenCapabilities {
    /// Horizontal position axis.
    pub x: Option<AxisRange>,
    /// Vertical position axis.
    pub y: Option<AxisRange>,
    /// Tip contact switch.
    pub tip_switch: bool,
    /// Hover/proximity indicator.
    pub in_range: bool,
    /// Tip pressure axis.
    pub pressure: Option<AxisRange>,
    /// Horizontal tilt axis.
    pub tilt_x: Option<AxisRange>,
    /// Vertical tilt axis.
    pub tilt_y: Option<AxisRange>,
    /// Barrel rotation (twist) axis.
    pub twist: Option<AxisRange>,
    /// Eraser end or inverted-tool indicator.
    pub eraser: bool,
    /// Number of barrel switches (Barrel Switch, Secondary Barrel Switch).
    pub barrel_buttons: u8,
}

impl PenCapabilities {
    /// Pressure levels the device reports, when a pressure axis is declared.
    #[must_use]
    pub fn pressure_levels(&self) -> Option<u64> {
        self.pressure
            .filter(|range| range.is_valid())
            .map(AxisRange::levels)
    }

    /// Whether both tilt axes are declared.
    #[must_use]
    pub const fn has_tilt(&self) -> bool {
        self.tilt_x.is_some() && self.tilt_y.is_some()
    }

    /// Whether this is enough to drive a pen: position plus a contact or
    /// proximity signal.
    #[must_use]
    pub const fn is_pen_shaped(&self) -> bool {
        self.x.is_some()
            && self.y.is_some()
            && (self.tip_switch || self.in_range || self.pressure.is_some())
    }

    /// Union of two declarations, keeping the first declared range per axis.
    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        Self {
            x: self.x.or(other.x),
            y: self.y.or(other.y),
            tip_switch: self.tip_switch || other.tip_switch,
            in_range: self.in_range || other.in_range,
            pressure: self.pressure.or(other.pressure),
            tilt_x: self.tilt_x.or(other.tilt_x),
            tilt_y: self.tilt_y.or(other.tilt_y),
            twist: self.twist.or(other.twist),
            eraser: self.eraser || other.eraser,
            barrel_buttons: self.barrel_buttons.max(other.barrel_buttons),
        }
    }
}

/// What one top-level collection turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionClass {
    /// A standard Digitizers-page pen with position and contact/proximity.
    Pen,
    /// A collection on the vendor-mirrored Digitizers page (`0xFF0D`).
    VendorDigitizer,
    /// A standard Digitizers-page collection that is touch, not pen.
    Touch,
    /// A standard Digitizers-page collection that is neither pen nor touch.
    OtherDigitizer,
    /// Anything else: keyboard, mouse, consumer control, gamepad, vendor page.
    Other,
}

/// One application collection's identity and declared pen surface.
///
/// Top-level collections and Application collections nested inside them are
/// both listed: a host associates inputs with the nearest enclosing
/// Application collection, so a nested one is a device function in its own
/// right and is classified as one. Inputs belong to the innermost
/// Application collection only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TopLevelCollection {
    /// Usage page of the collection's usage.
    pub usage_page: u16,
    /// Usage id of the collection's usage.
    pub usage: u16,
    /// Classification.
    pub class: CollectionClass,
    /// Pen surface declared inside it (meaningful for pen/vendor digitizer).
    pub capabilities: PenCapabilities,
    /// Whether it was nested inside another Application collection.
    pub nested: bool,
    /// Generic Desktop usage of a joystick, gamepad or multi-axis controller
    /// named by a physical or logical collection inside it.
    pub nested_controller_usage: Option<u16>,
}

/// Bounded summary of one report descriptor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReportDescriptorSummary {
    /// Application collections — top-level ones and any nested inside them —
    /// in the order they close.
    pub collections: Vec<TopLevelCollection>,
}

impl ReportDescriptorSummary {
    fn any(&self, class: CollectionClass) -> bool {
        self.collections
            .iter()
            .any(|collection| collection.class == class)
    }

    fn standard_digitizer(&self) -> bool {
        self.collections
            .iter()
            .any(|collection| collection.usage_page == USAGE_PAGE_DIGITIZER)
    }
}

/// Malformed or out-of-bounds report descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidDescriptorError {
    /// Empty descriptor.
    Empty,
    /// Descriptor exceeds [`MAX_HID_REPORT_DESCRIPTOR_BYTES`].
    TooLarge {
        /// Actual size.
        actual: usize,
    },
    /// An item's data runs past the end of the descriptor.
    Truncated {
        /// Offset of the offending item.
        offset: usize,
    },
    /// `End Collection` without an open collection.
    UnbalancedCollection {
        /// Offset of the offending item.
        offset: usize,
    },
    /// Collections still open at the end of the descriptor.
    UnterminatedCollection,
    /// Nesting deeper than [`MAX_COLLECTION_DEPTH`].
    CollectionTooDeep,
    /// More than [`MAX_TOP_LEVEL_COLLECTIONS`] top-level collections.
    TooManyCollections,
    /// More than [`MAX_LOCAL_USAGES`] usages on one main item.
    TooManyUsages,
    /// `Push`/`Pop` misuse or more than [`MAX_GLOBAL_STACK_DEPTH`] levels.
    GlobalStack,
}

impl Display for HidDescriptorError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("HID report descriptor is empty"),
            Self::TooLarge { actual } => write!(
                formatter,
                "HID report descriptor is {actual} bytes; maximum is {MAX_HID_REPORT_DESCRIPTOR_BYTES}"
            ),
            Self::Truncated { offset } => {
                write!(
                    formatter,
                    "HID report descriptor item at {offset} is truncated"
                )
            }
            Self::UnbalancedCollection { offset } => write!(
                formatter,
                "HID report descriptor closes a collection that is not open at {offset}"
            ),
            Self::UnterminatedCollection => {
                formatter.write_str("HID report descriptor leaves a collection open")
            }
            Self::CollectionTooDeep => {
                formatter.write_str("HID report descriptor nests collections too deeply")
            }
            Self::TooManyCollections => {
                formatter.write_str("HID report descriptor declares too many collections")
            }
            Self::TooManyUsages => {
                formatter.write_str("HID report descriptor declares too many usages on one item")
            }
            Self::GlobalStack => formatter.write_str("HID report descriptor misuses Push/Pop"),
        }
    }
}

impl std::error::Error for HidDescriptorError {}

#[derive(Debug, Clone, Copy, Default)]
struct GlobalState {
    usage_page: u16,
    logical_min: i64,
    logical_max_raw: u32,
    logical_max_size: u8,
    physical_min: i64,
    physical_max_raw: u32,
    physical_max_size: u8,
    unit: u32,
    unit_exponent: i8,
}

impl GlobalState {
    fn range(&self) -> AxisRange {
        AxisRange {
            logical_min: self.logical_min,
            logical_max: interpret_maximum(
                self.logical_min,
                self.logical_max_raw,
                self.logical_max_size,
            ),
            physical_min: self.physical_min,
            physical_max: interpret_maximum(
                self.physical_min,
                self.physical_max_raw,
                self.physical_max_size,
            ),
            unit: self.unit,
            unit_exponent: self.unit_exponent,
        }
    }
}

/// A maximum is signed only when its minimum is negative. Descriptors
/// routinely write `Logical Maximum (65535)` as two bytes `FF FF`, which read
/// as `-1` if every value were sign-extended.
fn interpret_maximum(minimum: i64, raw: u32, size: u8) -> i64 {
    if minimum < 0 {
        sign_extend(raw, size)
    } else {
        i64::from(raw)
    }
}

fn sign_extend(raw: u32, size: u8) -> i64 {
    match size {
        1 => i64::from(i8::from_le_bytes([raw.to_le_bytes()[0]])),
        2 => {
            let [low, high, _, _] = raw.to_le_bytes();
            i64::from(i16::from_le_bytes([low, high]))
        }
        4 => i64::from(i32::from_le_bytes(raw.to_le_bytes())),
        _ => 0,
    }
}

#[derive(Debug, Clone, Copy)]
enum LocalUsage {
    Single {
        page: u16,
        usage: u16,
    },
    Range {
        page: u16,
        minimum: u16,
        maximum: u16,
    },
}

#[derive(Debug, Default)]
struct LocalState {
    usages: Vec<LocalUsage>,
    pending_minimum: Option<(u16, u16)>,
}

impl LocalState {
    fn push(&mut self, usage: LocalUsage) -> Result<(), HidDescriptorError> {
        if self.usages.len() >= MAX_LOCAL_USAGES {
            return Err(HidDescriptorError::TooManyUsages);
        }
        self.usages.push(usage);
        Ok(())
    }
}

// Independent observed facts about one collection, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default)]
struct OpenTopLevel {
    usage_page: u16,
    usage: u16,
    /// Collection depth this application collection was opened at.
    opened_at: usize,
    stylus: bool,
    finger: bool,
    contact_identifier: bool,
    /// A nested Application collection that is not a digitizer one.
    nested_foreign_application: bool,
    /// Generic Desktop usage of a nested non-application (physical, logical)
    /// collection that names a joystick, gamepad or multi-axis controller.
    nested_controller_usage: Option<u16>,
    capabilities: PenCapabilities,
}

/// Resolves a usage on the Digitizers page or its vendor mirror to the
/// Digitizers usage id it stands for.
const fn digitizer_equivalent(page: u16, usage: u16) -> Option<u16> {
    if page == USAGE_PAGE_DIGITIZER || (page == USAGE_PAGE_VENDOR_DIGITIZER && usage <= 0xFF) {
        Some(usage)
    } else {
        None
    }
}

fn extended_usage(data: u32, size: u8, page: u16) -> (u16, u16) {
    let [low, high, page_low, page_high] = data.to_le_bytes();
    if size == 4 {
        (
            u16::from_le_bytes([page_low, page_high]),
            u16::from_le_bytes([low, high]),
        )
    } else {
        (page, u16::from_le_bytes([low, high]))
    }
}

fn record_input(top: &mut OpenTopLevel, page: u16, usage: u16, range: AxisRange) {
    if page == USAGE_PAGE_GENERIC_DESKTOP {
        match usage {
            USAGE_X => top.capabilities.x = top.capabilities.x.or(Some(range)),
            USAGE_Y => top.capabilities.y = top.capabilities.y.or(Some(range)),
            _ => {}
        }
        return;
    }
    let Some(digitizer) = digitizer_equivalent(page, usage) else {
        return;
    };
    let capabilities = &mut top.capabilities;
    match digitizer {
        DIGITIZER_TIP_PRESSURE => capabilities.pressure = capabilities.pressure.or(Some(range)),
        DIGITIZER_IN_RANGE => capabilities.in_range = true,
        DIGITIZER_X_TILT => capabilities.tilt_x = capabilities.tilt_x.or(Some(range)),
        DIGITIZER_Y_TILT => capabilities.tilt_y = capabilities.tilt_y.or(Some(range)),
        DIGITIZER_TWIST => capabilities.twist = capabilities.twist.or(Some(range)),
        DIGITIZER_TIP_SWITCH => capabilities.tip_switch = true,
        DIGITIZER_ERASER | DIGITIZER_INVERT => capabilities.eraser = true,
        DIGITIZER_BARREL_SWITCH | DIGITIZER_SECONDARY_BARREL_SWITCH => {
            capabilities.barrel_buttons = capabilities.barrel_buttons.saturating_add(1);
        }
        DIGITIZER_CONTACT_IDENTIFIER => top.contact_identifier = true,
        _ => {}
    }
}

fn close_top_level(top: &OpenTopLevel) -> TopLevelCollection {
    let page = top.usage_page;
    let class = if page == USAGE_PAGE_DIGITIZER {
        // A Stylus collection is the pen transducer itself; some descriptors
        // open it as an Application collection nested in the Pen one.
        let pen_usage = matches!(
            top.usage,
            DIGITIZER_PEN | DIGITIZER_DIGITIZER | DIGITIZER_STYLUS
        );
        let touch_usage = matches!(top.usage, DIGITIZER_TOUCH_SCREEN | DIGITIZER_TOUCH_PAD);
        let touch_shaped = (top.finger || top.contact_identifier) && !top.stylus;
        if top.capabilities.is_pen_shaped()
            && (top.stylus || (pen_usage && !touch_shaped))
            && (!touch_usage || top.stylus)
        {
            CollectionClass::Pen
        } else if touch_usage || touch_shaped {
            CollectionClass::Touch
        } else {
            CollectionClass::OtherDigitizer
        }
    } else if page == USAGE_PAGE_VENDOR_DIGITIZER && !top.nested_foreign_application {
        // A vendor-page "pen" that wraps another device's Application
        // collection (say, a keyboard) is a wrapper, not a pen: its opaque
        // reports cannot be trusted as pen data.
        CollectionClass::VendorDigitizer
    } else {
        CollectionClass::Other
    };
    TopLevelCollection {
        usage_page: page,
        usage: top.usage,
        class,
        capabilities: top.capabilities,
        nested: top.opened_at > 0,
        nested_controller_usage: top.nested_controller_usage,
    }
}

const COLLECTION_APPLICATION: u32 = 0x01;
const fn is_controller_usage(page: u16, usage: u16) -> bool {
    page == USAGE_PAGE_GENERIC_DESKTOP && matches!(usage, 0x04 | 0x05 | 0x08)
}
const fn is_digitizer_application(page: u16) -> bool {
    page == USAGE_PAGE_DIGITIZER || page == USAGE_PAGE_VENDOR_DIGITIZER
}

/// Usages this parser can act on, used to expand `Usage Minimum`/`Maximum`
/// ranges without iterating every value a hostile range could name.
const RECOGNISED_USAGES: [u16; 12] = [
    USAGE_X,
    USAGE_Y,
    DIGITIZER_TIP_PRESSURE,
    DIGITIZER_IN_RANGE,
    DIGITIZER_X_TILT,
    DIGITIZER_Y_TILT,
    DIGITIZER_TWIST,
    DIGITIZER_TIP_SWITCH,
    DIGITIZER_BARREL_SWITCH,
    DIGITIZER_ERASER,
    DIGITIZER_INVERT,
    DIGITIZER_SECONDARY_BARREL_SWITCH,
];

/// One decoded short item.
#[derive(Debug, Clone, Copy)]
struct ShortItem {
    offset: usize,
    item_type: u8,
    tag: u8,
    size: u8,
    data: u32,
}

#[derive(Debug, Default)]
struct DescriptorParser {
    summary: ReportDescriptorSummary,
    global: GlobalState,
    global_stack: Vec<GlobalState>,
    local: LocalState,
    depth: usize,
    /// Open Application collections, innermost last. The outermost is the
    /// top-level collection; nested Application collections are pushed on
    /// top of it and closed into the summary as collections of their own.
    applications: Vec<OpenTopLevel>,
}

impl DescriptorParser {
    fn item(&mut self, item: ShortItem) -> Result<(), HidDescriptorError> {
        match item.item_type {
            0 => {
                let result = self.main_item(item);
                self.local = LocalState::default();
                result
            }
            1 => self.global_item(item),
            2 => self.local_item(item),
            _ => Ok(()),
        }
    }

    fn main_item(&mut self, item: ShortItem) -> Result<(), HidDescriptorError> {
        match item.tag {
            // Input. Constant fields are padding and declare nothing.
            0x8 if item.data & 0x01 == 0 => {
                self.input();
                Ok(())
            }
            0xA => self.open_collection(item.data),
            0xC => self.close_collection(item.offset),
            _ => Ok(()),
        }
    }

    fn input(&mut self) {
        let Some(open) = self.applications.last_mut() else {
            return;
        };
        let range = self.global.range();
        for usage in &self.local.usages {
            match *usage {
                LocalUsage::Single { page, usage } => record_input(open, page, usage, range),
                LocalUsage::Range {
                    page,
                    minimum,
                    maximum,
                } => {
                    for usage in RECOGNISED_USAGES {
                        if (minimum..=maximum).contains(&usage) {
                            record_input(open, page, usage, range);
                        }
                    }
                }
            }
        }
    }

    fn open_collection(&mut self, kind: u32) -> Result<(), HidDescriptorError> {
        if self.depth >= MAX_COLLECTION_DEPTH {
            return Err(HidDescriptorError::CollectionTooDeep);
        }
        let (page, usage) = match self.local.usages.first().copied() {
            Some(LocalUsage::Single { page, usage }) => (page, usage),
            Some(LocalUsage::Range { page, minimum, .. }) => (page, minimum),
            None => (self.global.usage_page, 0),
        };
        let application = self.depth == 0 || kind & 0xFF == COLLECTION_APPLICATION;
        if application {
            if self.summary.collections.len() + self.applications.len() >= MAX_TOP_LEVEL_COLLECTIONS
            {
                return Err(HidDescriptorError::TooManyCollections);
            }
            if let Some(parent) = self.applications.last_mut() {
                if !is_digitizer_application(page) {
                    parent.nested_foreign_application = true;
                }
            }
            self.applications.push(OpenTopLevel {
                usage_page: page,
                usage,
                opened_at: self.depth,
                ..OpenTopLevel::default()
            });
        } else if let Some(open) = self.applications.last_mut() {
            match digitizer_equivalent(page, usage) {
                Some(DIGITIZER_STYLUS) => open.stylus = true,
                Some(DIGITIZER_FINGER) => open.finger = true,
                _ => {}
            }
            if is_controller_usage(page, usage) && open.nested_controller_usage.is_none() {
                open.nested_controller_usage = Some(usage);
            }
        }
        self.depth += 1;
        Ok(())
    }

    fn close_collection(&mut self, offset: usize) -> Result<(), HidDescriptorError> {
        if self.depth == 0 {
            return Err(HidDescriptorError::UnbalancedCollection { offset });
        }
        self.depth -= 1;
        if self
            .applications
            .last()
            .is_some_and(|open| open.opened_at == self.depth)
        {
            if let Some(open) = self.applications.pop() {
                self.summary.collections.push(close_top_level(&open));
            }
        }
        Ok(())
    }

    fn global_item(&mut self, item: ShortItem) -> Result<(), HidDescriptorError> {
        let global = &mut self.global;
        match item.tag {
            0x0 => global.usage_page = u16::try_from(item.data & 0xFFFF).unwrap_or(0),
            0x1 => global.logical_min = sign_extend(item.data, item.size),
            0x2 => {
                global.logical_max_raw = item.data;
                global.logical_max_size = item.size;
            }
            0x3 => global.physical_min = sign_extend(item.data, item.size),
            0x4 => {
                global.physical_max_raw = item.data;
                global.physical_max_size = item.size;
            }
            0x5 => {
                // Unit exponent is a four-bit two's-complement nibble.
                let value = i8::from_le_bytes([item.data.to_le_bytes()[0] & 0x0F]);
                global.unit_exponent = if value >= 8 { value - 16 } else { value };
            }
            0x6 => global.unit = item.data,
            0xA => {
                if self.global_stack.len() >= MAX_GLOBAL_STACK_DEPTH {
                    return Err(HidDescriptorError::GlobalStack);
                }
                self.global_stack.push(self.global);
            }
            0xB => {
                self.global = self
                    .global_stack
                    .pop()
                    .ok_or(HidDescriptorError::GlobalStack)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn local_item(&mut self, item: ShortItem) -> Result<(), HidDescriptorError> {
        let usage = extended_usage(item.data, item.size, self.global.usage_page);
        match item.tag {
            0x0 => self.local.push(LocalUsage::Single {
                page: usage.0,
                usage: usage.1,
            }),
            0x1 => {
                self.local.pending_minimum = Some(usage);
                Ok(())
            }
            0x2 => match self.local.pending_minimum.take() {
                Some((page, minimum)) if page == usage.0 && minimum <= usage.1 => {
                    self.local.push(LocalUsage::Range {
                        page,
                        minimum,
                        maximum: usage.1,
                    })
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        }
    }
}

/// Parses one HID report descriptor into a bounded collection summary.
///
/// # Errors
///
/// Returns [`HidDescriptorError`] for an empty, oversized, truncated,
/// unbalanced or over-nested descriptor.
pub fn parse_report_descriptor(
    bytes: &[u8],
) -> Result<ReportDescriptorSummary, HidDescriptorError> {
    if bytes.is_empty() {
        return Err(HidDescriptorError::Empty);
    }
    if bytes.len() > MAX_HID_REPORT_DESCRIPTOR_BYTES {
        return Err(HidDescriptorError::TooLarge {
            actual: bytes.len(),
        });
    }

    let mut parser = DescriptorParser::default();
    let mut offset = 0_usize;
    while offset < bytes.len() {
        let prefix = bytes[offset];
        if prefix == 0xFE {
            // Long item: size byte, tag byte, data. Carries nothing this
            // classifier reads, so it is skipped by its declared length.
            let Some(&size) = bytes.get(offset + 1) else {
                return Err(HidDescriptorError::Truncated { offset });
            };
            let end = offset + 3 + usize::from(size);
            if end > bytes.len() {
                return Err(HidDescriptorError::Truncated { offset });
            }
            offset = end;
            continue;
        }
        let size: u8 = match prefix & 0x03 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => 4,
        };
        let data_start = offset + 1;
        let data_end = data_start + usize::from(size);
        if data_end > bytes.len() {
            return Err(HidDescriptorError::Truncated { offset });
        }
        let mut data = [0_u8; 4];
        data[..usize::from(size)].copy_from_slice(&bytes[data_start..data_end]);
        parser.item(ShortItem {
            offset,
            item_type: (prefix >> 2) & 0x03,
            tag: prefix >> 4,
            size,
            data: u32::from_le_bytes(data),
        })?;
        offset = data_end;
    }

    if parser.depth != 0 {
        return Err(HidDescriptorError::UnterminatedCollection);
    }
    Ok(parser.summary)
}

/// Why a device is not a pen tablet. Every variant fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotPenTablet {
    /// No report descriptor was supplied.
    NoDescriptors,
    /// More descriptors than a bridged device may have interfaces.
    TooManyDescriptors,
    /// A descriptor failed to parse.
    Malformed(HidDescriptorError),
    /// The device has digitizer collections, but only touch ones.
    TouchOnly,
    /// The device has a standard digitizer collection that is not a usable
    /// pen (for example, no position or no contact/proximity signal).
    NoPenCollection,
    /// The device publishes only a vendor-mirrored digitizer page that names
    /// no Digitizer or Pen, with no standard digitizer to corroborate it.
    VendorDigitizerOnly,
    /// A vendor-page pen collection sits beside a joystick, gamepad or
    /// multi-axis controller collection, which is not a tablet function, so
    /// its opaque reports cannot be trusted to be pen data.
    DisqualifyingCollection,
    /// No digitizer collection at all: keyboard, mouse, gamepad, vendor-only.
    NoDigitizer,
}

impl Display for NotPenTablet {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoDescriptors => formatter.write_str("no HID report descriptor was supplied"),
            Self::TooManyDescriptors => {
                formatter.write_str("more HID report descriptors than a bridged device may have")
            }
            Self::Malformed(error) => Display::fmt(error, formatter),
            Self::TouchOnly => {
                formatter.write_str("the device is a touch digitizer without a pen collection")
            }
            Self::NoPenCollection => formatter.write_str(
                "the device's digitizer collection does not report a usable pen",
            ),
            Self::VendorDigitizerOnly => formatter.write_str(
                "the device publishes only a vendor-defined digitizer page and no standard digitizer",
            ),
            Self::DisqualifyingCollection => formatter.write_str(
                "the device's vendor-page pen sits beside a joystick, gamepad or multi-axis controller",
            ),
            Self::NoDigitizer => formatter.write_str("the device declares no digitizer collection"),
        }
    }
}

impl std::error::Error for NotPenTablet {}

/// Which descriptor evidence made a device a pen tablet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PenTabletEvidence {
    /// A standard Digitizers-page pen collection.
    DigitizerPen,
    /// A vendor-mirrored digitizer collection corroborated by a standard
    /// Digitizers-page collection on the same device.
    VendorDigitizerWithStandardDigitizer,
    /// A Digitizer or Pen application collection on the vendor-defined
    /// Digitizers page (`0xFF0D`), on a device with no keyboard, keypad or
    /// game-controller collection. Its reports may be opaque, so its
    /// capabilities are whatever it declares and nothing more.
    VendorDigitizerPen,
}

/// A device classified as a pen tablet, with its declared pen surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenTabletProfile {
    /// Why it qualified.
    pub evidence: PenTabletEvidence,
    /// Union of the pen surface declared across its pen collections.
    pub capabilities: PenCapabilities,
}

/// Generic Desktop collections that are not tablet functions and are never
/// forwarded with a pen tablet: Joystick (`0x04`), Gamepad (`0x05`) and
/// Multi-axis Controller (`0x08`) — as an Application collection at any
/// depth, or named by a physical/logical collection inside one.
///
/// Keyboard (`0x06`) and Keypad (`0x07`) are deliberately *not* here, and
/// neither is a mouse (`0x02`): tablets present express keys and buttons as
/// keyboards or keypads and their mouse mode as a mouse, and supporting the
/// whole tablet means forwarding those too. Forwarding a tablet's keyboard
/// grants no capability the authenticated Deck lacks — it can already send
/// any keystroke through the ordinary input channel. What stays enforced is
/// that the device is a pen tablet ([`classify_pen_tablet`]): a device
/// without a pen digitizer, such as a plain keyboard, is never forwarded.
fn is_disqualifying(collection: &TopLevelCollection) -> bool {
    disqualifying_usage(collection).is_some()
}

fn disqualifying_usage(collection: &TopLevelCollection) -> Option<u16> {
    if is_controller_usage(collection.usage_page, collection.usage) {
        Some(collection.usage)
    } else {
        collection.nested_controller_usage
    }
}

/// Returns the Generic Desktop usage of the first collection in `descriptor`
/// that is not a tablet function and must never be forwarded with a pen
/// tablet (joystick, gamepad, multi-axis controller), or `None` when it has
/// none. Keyboards and keypads (express keys) are allowed; see
/// `is_disqualifying` for why.
///
/// Bridging forwards every collection of every interface it admits, so this
/// is checked on everything that will actually reach a host, independently
/// of [`classify_pen_tablet`] deciding that the device *is* a tablet.
///
/// # Errors
///
/// Returns [`HidDescriptorError`] for a malformed descriptor.
pub fn disqualifying_collection(descriptor: &[u8]) -> Result<Option<u16>, HidDescriptorError> {
    Ok(parse_report_descriptor(descriptor)?
        .collections
        .iter()
        .find_map(disqualifying_usage))
}

/// Whether a USB interface is a boot keyboard (`bInterfaceSubClass` 1,
/// `bInterfaceProtocol` 1). A pen tablet may have one (express keys) and it
/// is bridged with the tablet; the macOS helper only refuses to *send a USB
/// request* to such a device when probing, so examining a keyboard never
/// touches it.
#[must_use]
pub const fn is_boot_keyboard_interface(subclass: u8, protocol: u8) -> bool {
    subclass == 1 && protocol == 1
}

/// Classifies one device from the report descriptors of its HID interfaces.
///
/// # Errors
///
/// Returns [`NotPenTablet`] when the device is not a pen tablet or any
/// descriptor is malformed. A single malformed descriptor rejects the whole
/// device: a peer that sends one unparseable interface is not trusted on the
/// others.
pub fn classify_pen_tablet<'a, I>(descriptors: I) -> Result<PenTabletProfile, NotPenTablet>
where
    I: IntoIterator<Item = &'a [u8]>,
{
    let mut summaries = Vec::new();
    for descriptor in descriptors {
        if summaries.len() >= MAX_INTERFACES {
            return Err(NotPenTablet::TooManyDescriptors);
        }
        summaries.push(parse_report_descriptor(descriptor).map_err(NotPenTablet::Malformed)?);
    }
    if summaries.is_empty() {
        return Err(NotPenTablet::NoDescriptors);
    }

    let pen = summaries
        .iter()
        .flat_map(|summary| summary.collections.iter())
        .filter(|collection| collection.class == CollectionClass::Pen)
        .fold(None::<PenCapabilities>, |merged, collection| {
            Some(merged.map_or(collection.capabilities, |merged| {
                merged.merged(collection.capabilities)
            }))
        });
    let vendor = summaries
        .iter()
        .flat_map(|summary| summary.collections.iter())
        .filter(|collection| collection.class == CollectionClass::VendorDigitizer)
        .fold(PenCapabilities::default(), |merged, collection| {
            merged.merged(collection.capabilities)
        });
    if let Some(capabilities) = pen {
        return Ok(PenTabletProfile {
            evidence: PenTabletEvidence::DigitizerPen,
            capabilities: capabilities.merged(vendor),
        });
    }

    let has_vendor = summaries
        .iter()
        .any(|summary| summary.any(CollectionClass::VendorDigitizer));
    let has_standard = summaries
        .iter()
        .any(ReportDescriptorSummary::standard_digitizer);
    if has_vendor && has_standard {
        return Ok(PenTabletProfile {
            evidence: PenTabletEvidence::VendorDigitizerWithStandardDigitizer,
            capabilities: vendor,
        });
    }
    let collections = || {
        summaries
            .iter()
            .flat_map(|summary| summary.collections.iter())
    };
    let vendor_pen = collections().any(|collection| {
        collection.class == CollectionClass::VendorDigitizer
            && matches!(collection.usage, DIGITIZER_DIGITIZER | DIGITIZER_PEN)
    });
    if vendor_pen {
        if collections().any(is_disqualifying) {
            return Err(NotPenTablet::DisqualifyingCollection);
        }
        return Ok(PenTabletProfile {
            evidence: PenTabletEvidence::VendorDigitizerPen,
            capabilities: vendor,
        });
    }
    if has_vendor {
        return Err(NotPenTablet::VendorDigitizerOnly);
    }
    if summaries
        .iter()
        .any(|summary| summary.any(CollectionClass::OtherDigitizer))
    {
        return Err(NotPenTablet::NoPenCollection);
    }
    if summaries
        .iter()
        .any(|summary| summary.any(CollectionClass::Touch))
    {
        return Err(NotPenTablet::TouchOnly);
    }
    Err(NotPenTablet::NoDigitizer)
}

/// Returns whether a descriptor declares any digitizer surface at all: a
/// standard Digitizers-page collection or a vendor-mirrored one.
///
/// This is a *presence* hint for a client deciding whether to offer typed pen
/// input, where the operating system's own tablet events carry the pen and no
/// device is created from the descriptor. It is never an authorization: use
/// [`classify_pen_tablet`] before exposing a device to a host kernel.
#[must_use]
pub fn declares_pen_or_vendor_digitizer(descriptor: &[u8]) -> bool {
    parse_report_descriptor(descriptor).is_ok_and(|summary| {
        summary.collections.iter().any(|collection| {
            matches!(
                collection.class,
                CollectionClass::Pen | CollectionClass::VendorDigitizer
            )
        })
    })
}

/// Returns whether a descriptor declares any digitizer collection: pen,
/// touch, other standard digitizer, or vendor-mirrored digitizer.
///
/// A capture filter for a client deciding which HID interfaces to offer: a
/// touch interface carries no pen, but it is the standard digitizer that
/// corroborates a vendor-page pen on the same device.
#[must_use]
pub fn declares_any_digitizer(descriptor: &[u8]) -> bool {
    parse_report_descriptor(descriptor).is_ok_and(|summary| {
        summary
            .collections
            .iter()
            .any(|collection| collection.class != CollectionClass::Other)
    })
}

/// HID class descriptor type.
pub const DESCRIPTOR_HID: u8 = 0x21;
/// HID report descriptor type.
pub const DESCRIPTOR_HID_REPORT: u8 = 0x22;
const REQUEST_GET_DESCRIPTOR: u8 = 0x06;
const REQUEST_TYPE_STANDARD_INTERFACE_IN: u8 = 0x81;

/// Most subordinate descriptor entries accepted in one HID class descriptor.
pub const MAX_HID_SUBORDINATE_DESCRIPTORS: usize = 16;

/// A malformed HID class descriptor (type `0x21`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HidClassDescriptorError {
    /// Not a HID class descriptor, or `bLength` disagrees with the bytes.
    Malformed,
    /// `bNumDescriptors` is zero, above [`MAX_HID_SUBORDINATE_DESCRIPTORS`],
    /// or `bLength` is not exactly `6 + 3 * bNumDescriptors`.
    InconsistentCount,
    /// The list has no Report (`0x22`) entry, or more than one.
    ReportEntries(usize),
    /// The Report entry's length is zero or above
    /// [`MAX_HID_REPORT_DESCRIPTOR_BYTES`].
    ReportLength(u16),
}

/// Validates one complete HID class descriptor and returns its single
/// report-descriptor length.
///
/// The whole subordinate list is checked, not just the first match: hosts
/// disagree on which Report entry wins when there are several (Linux uses the
/// last), so a list with more than one Report entry is refused outright, as
/// is a list whose `bLength` and `bNumDescriptors` disagree. Other entry
/// types (Physical `0x23`, ...) are allowed.
///
/// # Errors
///
/// Returns [`HidClassDescriptorError`] for any malformed or ambiguous list.
pub fn parse_hid_class_descriptor(descriptor: &[u8]) -> Result<u16, HidClassDescriptorError> {
    if descriptor.len() < 6
        || usize::from(descriptor[0]) != descriptor.len()
        || descriptor[1] != DESCRIPTOR_HID
    {
        return Err(HidClassDescriptorError::Malformed);
    }
    let count = usize::from(descriptor[5]);
    if count == 0 || count > MAX_HID_SUBORDINATE_DESCRIPTORS || descriptor.len() != 6 + 3 * count {
        return Err(HidClassDescriptorError::InconsistentCount);
    }
    let reports: Vec<u16> = descriptor[6..]
        .chunks_exact(3)
        .filter(|entry| entry[0] == DESCRIPTOR_HID_REPORT)
        .map(|entry| u16::from_le_bytes([entry[1], entry[2]]))
        .collect();
    let [length] = reports.as_slice() else {
        return Err(HidClassDescriptorError::ReportEntries(reports.len()));
    };
    if *length == 0 || usize::from(*length) > MAX_HID_REPORT_DESCRIPTOR_BYTES {
        return Err(HidClassDescriptorError::ReportLength(*length));
    }
    Ok(*length)
}

/// Reads the report-descriptor length from an interface's class-specific
/// descriptor bytes: the first HID class descriptor (type `0x21`), validated
/// whole by [`parse_hid_class_descriptor`].
///
/// Returns `None` when no HID descriptor is present, when it is malformed or
/// ambiguous (several Report entries), or when the declared length is zero or
/// above [`MAX_HID_REPORT_DESCRIPTOR_BYTES`].
#[must_use]
pub fn hid_report_descriptor_length(class_descriptors: &[u8]) -> Option<u16> {
    let mut offset = 0_usize;
    while offset + 2 <= class_descriptors.len() {
        let length = usize::from(class_descriptors[offset]);
        if length < 2 || offset + length > class_descriptors.len() {
            return None;
        }
        let descriptor = &class_descriptors[offset..offset + length];
        if descriptor[1] == DESCRIPTOR_HID {
            return parse_hid_class_descriptor(descriptor).ok();
        }
        offset += length;
    }
    None
}

/// The standard `GET_DESCRIPTOR(Report)` request for one HID interface.
#[must_use]
pub const fn hid_report_descriptor_request(interface: u8, length: u16) -> SetupPacket {
    SetupPacket {
        request_type: REQUEST_TYPE_STANDARD_INTERFACE_IN,
        request: REQUEST_GET_DESCRIPTOR,
        value: (DESCRIPTOR_HID_REPORT as u16) << 8,
        index: interface as u16,
        length,
    }
}

/// Returns the interface a setup packet asks for the report descriptor of, if
/// it is a `GET_DESCRIPTOR(Report)` request.
#[must_use]
pub const fn requested_report_descriptor_interface(setup: SetupPacket) -> Option<u8> {
    if setup.request_type == REQUEST_TYPE_STANDARD_INTERFACE_IN
        && setup.request == REQUEST_GET_DESCRIPTOR
        && setup.descriptor_type() == DESCRIPTOR_HID_REPORT
        && setup.index <= 0xFF
    {
        Some(setup.index.to_le_bytes()[0])
    } else {
        None
    }
}

#[cfg(test)]
#[path = "hid_digitizer_tests.rs"]
mod tests;
