// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Reading any HID pen digitizer from its own report descriptor (Q-16: any
//! tablet works by default).
//!
//! A host that reads a bridged pen itself (a macOS Pier: the tablet maker's
//! driver does not bind a virtual HID device, ADR 0022 §14) used to know one
//! model's packets ([`crate::IntuosPenReader`]). Most pen tablets do not need
//! a model table at all: their report descriptor states, in the HID Usage
//! Tables' vocabulary, where each pen field sits in the report, its size and
//! its logical and physical range. [`GenericPenLayout`] is that statement,
//! parsed; [`GenericPenReader`] turns reports into the same
//! [`HostPenSample`] the dedicated reader produces.
//!
//! What is read, per report:
//!
//! | Usage | Pages |
//! | --- | --- |
//! | X, Y (absolute) | Generic Desktop `0x30`/`0x31` |
//! | Tip Switch, Barrel Switch, Secondary Barrel Switch, Invert, Eraser, In Range, Tip Pressure, X/Y Tilt, Twist | Digitizers `0x0D` |
//! | the same, in the vendor digitizer convention | `0xFF0D`: the usage id's high byte names the standard page it stands for (`0x00` Digitizers, `0x01` Generic Desktop, `0x09` Button), as the Linux `wacom` driver reads Wacom's HID-generic tablets |
//! | Button 1, 2, 3 | Button page, only where the Digitizers usage is absent (tip, barrel, secondary barrel) |
//!
//! Only fields inside a pen application collection are read: an Application
//! collection whose usage is Pen, Light Pen or Digitizer (on `0x0D` or its
//! vendor mirror `0xFF0D`), or any digitizer collection with a nested Stylus.
//! Touch screens, touch pads, mice and keyboards in the same descriptor are
//! ignored; their reports never become pen samples. A report qualifies when
//! it carries absolute X and Y and at least one of Tip Switch, Tip Pressure
//! or In Range.
//!
//! The descriptor and every report come from the Deck, so both are
//! untrusted: the parser is bounded exactly like
//! [`crate::parse_report_descriptor`] (size, nesting, usages, the global
//! stack), every field is at most 32 bits, every bit read is checked against
//! the report's length, and no value is used without clamping into its
//! declared range. A report too short for its layout yields nothing.

use crate::{
    HostPenSample, MAX_COLLECTION_DEPTH, MAX_GLOBAL_STACK_DEPTH, MAX_HID_REPORT_DESCRIPTOR_BYTES,
    MAX_LOCAL_USAGES, MAX_TRANSFER_BYTES,
};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::time::{Duration, Instant};

/// Most pen reports one layout keeps (one per report id).
pub const MAX_PEN_REPORTS: usize = 16;
/// Widest field read, in bits.
pub const MAX_PEN_FIELD_BITS: u32 = 32;
/// Most bits one report may declare before its later fields are ignored.
pub const MAX_PEN_REPORT_BITS: u32 = 16 * 1024 * 8;
const _: () = assert!(MAX_PEN_REPORT_BITS as usize == MAX_TRANSFER_BYTES * 8);
/// Pressure above which a pen without a tip switch counts as touching:
/// one percent of its range.
pub const GENERIC_TOUCH_THRESHOLD: f32 = 0.01;
/// How long a pen whose reports carry no In Range may stay silent before it
/// counts as gone. Such a pen simply stops reporting when it is lifted away;
/// over a bridge it reports about once per network round trip, so the bound
/// is several of those.
pub const GENERIC_QUIET_EXIT: Duration = Duration::from_millis(250);
/// The tilt a descriptor that states no unit and a range wider than ±90 is
/// assumed to span, in degrees: libinput's assumption for such tablets.
pub const UNITLESS_TILT_DEGREES: f64 = 64.0;

const PAGE_GENERIC_DESKTOP: u16 = 0x01;
const PAGE_BUTTON: u16 = 0x09;
const PAGE_DIGITIZER: u16 = 0x0D;
const PAGE_VENDOR_DIGITIZER: u16 = 0xFF0D;

const GD_X: u16 = 0x30;
const GD_Y: u16 = 0x31;
const DIG_DIGITIZER: u16 = 0x01;
const DIG_PEN: u16 = 0x02;
const DIG_LIGHT_PEN: u16 = 0x03;
const DIG_STYLUS: u16 = 0x20;
const DIG_TIP_PRESSURE: u16 = 0x30;
const DIG_IN_RANGE: u16 = 0x32;
const DIG_INVERT: u16 = 0x3C;
const DIG_X_TILT: u16 = 0x3D;
const DIG_Y_TILT: u16 = 0x3E;
const DIG_TWIST: u16 = 0x41;
const DIG_TIP_SWITCH: u16 = 0x42;
const DIG_BARREL_SWITCH: u16 = 0x44;
const DIG_ERASER: u16 = 0x45;
const DIG_SECONDARY_BARREL_SWITCH: u16 = 0x5A;
const COLLECTION_APPLICATION: u32 = 0x01;

/// Where one field sits in a report and how its value reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PenField {
    /// Bit offset from the start of the report's data (after the report id).
    pub bit_offset: u32,
    /// Width, `1..=32` bits.
    pub bits: u32,
    pub logical_min: i64,
    pub logical_max: i64,
    pub physical_min: i64,
    pub physical_max: i64,
    /// Raw HID unit word.
    pub unit: u32,
    /// Sign-decoded unit exponent, `-8..=7`.
    pub unit_exponent: i8,
}

impl PenField {
    /// The field's value in `data`, sign-extended when its logical minimum
    /// is negative; `None` when `data` is too short.
    #[must_use]
    pub fn read(&self, data: &[u8]) -> Option<i64> {
        let end = u64::from(self.bit_offset) + u64::from(self.bits);
        if self.bits == 0 || self.bits > MAX_PEN_FIELD_BITS || end > (data.len() as u64) * 8 {
            return None;
        }
        let mut raw = 0_u64;
        for bit in 0..self.bits {
            let position = self.bit_offset + bit;
            let byte = data.get(usize::try_from(position / 8).ok()?)?;
            if (byte >> (position % 8)) & 1 == 1 {
                raw |= 1 << bit;
            }
        }
        if self.logical_min < 0 && (raw >> (self.bits - 1)) & 1 == 1 {
            Some(i64::try_from(raw).ok()? - (1_i64 << self.bits))
        } else {
            i64::try_from(raw).ok()
        }
    }

    /// The span `logical_max - logical_min`, when positive and small enough
    /// to be a coordinate range.
    fn span(&self) -> Option<u32> {
        let span = self.logical_max.checked_sub(self.logical_min)?;
        u32::try_from(span)
            .ok()
            .filter(|span| *span > 0 && i32::try_from(*span).is_ok())
    }

    /// `value` as a fraction `0.0..=1.0` of the logical range.
    fn fraction(&self, value: i64) -> f64 {
        let Some(span) = self.span() else {
            return 0.0;
        };
        let offset = value.clamp(self.logical_min, self.logical_max) - self.logical_min;
        #[allow(clippy::cast_precision_loss)]
        let fraction = offset as f64 / f64::from(span);
        fraction.clamp(0.0, 1.0)
    }

    /// `value` in degrees: physical units when the descriptor states an
    /// angle (English Rotation degrees, SI Rotation radians), the logical
    /// value itself when it states none and fits ±90, otherwise the logical
    /// range spread over ±[`UNITLESS_TILT_DEGREES`].
    fn degrees(&self, value: i64) -> f64 {
        let value = value.clamp(self.logical_min, self.logical_max);
        let system = self.unit & 0x0F;
        let length = (self.unit >> 4) & 0x0F;
        let angle = matches!((system, length), (2 | 4, 1));
        #[allow(clippy::cast_precision_loss)]
        if angle {
            let (physical_min, physical_max) = if self.physical_min == 0 && self.physical_max == 0 {
                (self.logical_min, self.logical_max)
            } else {
                (self.physical_min, self.physical_max)
            };
            let span = (self.logical_max - self.logical_min) as f64;
            let physical = if span > 0.0 {
                physical_min as f64
                    + (value - self.logical_min) as f64 * (physical_max - physical_min) as f64
                        / span
            } else {
                physical_min as f64
            };
            let scaled = physical * 10_f64.powi(i32::from(self.unit_exponent));
            if system == 2 {
                scaled.to_degrees()
            } else {
                scaled
            }
        } else if self.logical_min >= -90 && self.logical_max <= 90 && self.unit == 0 {
            value as f64
        } else {
            let span = (self.logical_max - self.logical_min) as f64;
            if span <= 0.0 {
                return 0.0;
            }
            let centred = (value - self.logical_min) as f64 / span * 2.0 - 1.0;
            centred * UNITLESS_TILT_DEGREES
        }
    }

    /// `value` as a barrel rotation, `0.0..360.0`: physical units when the
    /// descriptor states an angle, otherwise the logical range spread over
    /// one full turn (`(v - min) / (max - min + 1)` of 360°).
    fn turn_degrees(&self, value: i64) -> f64 {
        let system = self.unit & 0x0F;
        let length = (self.unit >> 4) & 0x0F;
        let degrees = if matches!((system, length), (2 | 4, 1)) {
            self.degrees(value)
        } else {
            let value = value.clamp(self.logical_min, self.logical_max);
            #[allow(clippy::cast_precision_loss)]
            let span = (self.logical_max - self.logical_min + 1) as f64;
            #[allow(clippy::cast_precision_loss)]
            let offset = (value - self.logical_min) as f64;
            offset / span * 360.0
        };
        let turned = degrees.rem_euclid(360.0);
        if turned < 360.0 { turned } else { 0.0 }
    }
}

/// The pen fields of one input report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PenReportLayout {
    /// The report id; `0` when the descriptor uses none.
    pub report_id: u8,
    /// Bytes of data (after the report id) every field read needs; a
    /// shorter report is not read at all.
    pub data_bytes: usize,
    pub x: PenField,
    pub y: PenField,
    pub tip: Option<PenField>,
    pub in_range: Option<PenField>,
    pub pressure: Option<PenField>,
    pub barrel: Option<PenField>,
    pub secondary_barrel: Option<PenField>,
    pub invert: Option<PenField>,
    pub eraser: Option<PenField>,
    pub tilt_x: Option<PenField>,
    pub tilt_y: Option<PenField>,
    pub twist: Option<PenField>,
}

/// Every pen report of one HID interface, from its report descriptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GenericPenLayout {
    /// Whether reports start with a report id byte.
    pub report_ids: bool,
    pub reports: Vec<PenReportLayout>,
}

/// Why a descriptor gives no readable pen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GenericPenRefusal {
    /// Empty, oversized, truncated, unbalanced, over-nested or otherwise
    /// beyond the parser's bounds.
    Malformed,
    /// Well-formed, but no input report carries a pen with absolute X and Y
    /// and a tip, pressure or in-range field (a vendor packet format, a
    /// mouse-mode tablet, a touch-only digitizer, not a pen at all).
    NoPenReport,
}

impl Display for GenericPenRefusal {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Malformed => "its HID report descriptor could not be read",
            Self::NoPenReport => {
                "its HID report descriptor declares no standard pen report (its pen speaks a \
                 vendor format)"
            }
        })
    }
}

impl std::error::Error for GenericPenRefusal {}

#[derive(Debug, Clone, Copy, Default)]
struct Global {
    usage_page: u16,
    logical_min: i64,
    logical_max_raw: u32,
    logical_max_size: u8,
    physical_min: i64,
    physical_max_raw: u32,
    physical_max_size: u8,
    unit: u32,
    unit_exponent: i8,
    report_size: u32,
    report_count: u32,
    report_id: u8,
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

/// A maximum is signed only when its minimum is negative.
fn maximum(minimum: i64, raw: u32, size: u8) -> i64 {
    if minimum < 0 {
        sign_extend(raw, size)
    } else {
        i64::from(raw)
    }
}

/// A usage as the standard page it stands for: the vendor digitizer
/// convention's high byte names the page, low byte the usage.
fn equivalent(page: u16, usage: u16) -> Option<(u16, u16)> {
    match page {
        PAGE_VENDOR_DIGITIZER => {
            let [low, high] = usage.to_le_bytes();
            let low = u16::from(low);
            match high {
                0x00 => Some((PAGE_DIGITIZER, low)),
                0x01 => Some((PAGE_GENERIC_DESKTOP, low)),
                0x09 => Some((PAGE_BUTTON, low)),
                _ => None,
            }
        }
        PAGE_GENERIC_DESKTOP | PAGE_BUTTON | PAGE_DIGITIZER => Some((page, usage)),
        _ => None,
    }
}

/// Whether an application collection's usage names a pen.
fn pen_application(page: u16, usage: u16) -> bool {
    matches!(
        equivalent(page, usage),
        Some((PAGE_DIGITIZER, DIG_PEN | DIG_LIGHT_PEN | DIG_DIGITIZER))
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    X,
    Y,
    Tip,
    InRange,
    Pressure,
    Barrel,
    SecondaryBarrel,
    Invert,
    Eraser,
    TiltX,
    TiltY,
    Twist,
    ButtonTip,
    ButtonBarrel,
    ButtonSecondary,
}

fn role(page: u16, usage: u16) -> Option<Role> {
    Some(match equivalent(page, usage)? {
        (PAGE_GENERIC_DESKTOP, GD_X) => Role::X,
        (PAGE_GENERIC_DESKTOP, GD_Y) => Role::Y,
        (PAGE_DIGITIZER, DIG_TIP_SWITCH) => Role::Tip,
        (PAGE_DIGITIZER, DIG_IN_RANGE) => Role::InRange,
        (PAGE_DIGITIZER, DIG_TIP_PRESSURE) => Role::Pressure,
        (PAGE_DIGITIZER, DIG_BARREL_SWITCH) => Role::Barrel,
        (PAGE_DIGITIZER, DIG_SECONDARY_BARREL_SWITCH) => Role::SecondaryBarrel,
        (PAGE_DIGITIZER, DIG_INVERT) => Role::Invert,
        (PAGE_DIGITIZER, DIG_ERASER) => Role::Eraser,
        (PAGE_DIGITIZER, DIG_X_TILT) => Role::TiltX,
        (PAGE_DIGITIZER, DIG_Y_TILT) => Role::TiltY,
        (PAGE_DIGITIZER, DIG_TWIST) => Role::Twist,
        (PAGE_BUTTON, 1) => Role::ButtonTip,
        (PAGE_BUTTON, 2) => Role::ButtonBarrel,
        (PAGE_BUTTON, 3) => Role::ButtonSecondary,
        _ => return None,
    })
}

/// One report's fields while the descriptor is walked.
#[derive(Debug, Default)]
struct Partial {
    fields: Vec<(Role, PenField)>,
}

impl Partial {
    fn first(&self, role: Role) -> Option<PenField> {
        self.fields
            .iter()
            .find(|(candidate, _)| *candidate == role)
            .map(|(_, field)| *field)
    }

    fn finish(&self, report_id: u8) -> Option<PenReportLayout> {
        let positional = |field: PenField| field.span().map(|_| field);
        let x = self.first(Role::X).and_then(positional)?;
        let y = self.first(Role::Y).and_then(positional)?;
        let tip = self
            .first(Role::Tip)
            .or_else(|| self.first(Role::ButtonTip));
        let in_range = self.first(Role::InRange);
        let pressure = self
            .first(Role::Pressure)
            .filter(|field| field.span().is_some());
        if tip.is_none() && in_range.is_none() && pressure.is_none() {
            return None;
        }
        let ranged = |field: PenField| (field.logical_max > field.logical_min).then_some(field);
        let mut layout = PenReportLayout {
            report_id,
            data_bytes: 0,
            x,
            y,
            tip,
            in_range,
            pressure,
            barrel: self
                .first(Role::Barrel)
                .or_else(|| self.first(Role::ButtonBarrel)),
            secondary_barrel: self
                .first(Role::SecondaryBarrel)
                .or_else(|| self.first(Role::ButtonSecondary)),
            invert: self.first(Role::Invert),
            eraser: self.first(Role::Eraser),
            tilt_x: self.first(Role::TiltX).and_then(ranged),
            tilt_y: self.first(Role::TiltY).and_then(ranged),
            twist: self.first(Role::Twist).and_then(ranged),
        };
        let end = [
            Some(layout.x),
            Some(layout.y),
            layout.tip,
            layout.in_range,
            layout.pressure,
            layout.barrel,
            layout.secondary_barrel,
            layout.invert,
            layout.eraser,
            layout.tilt_x,
            layout.tilt_y,
            layout.twist,
        ]
        .into_iter()
        .flatten()
        .map(|field| field.bit_offset + field.bits)
        .max()
        .unwrap_or(0);
        layout.data_bytes = usize::try_from(end.div_ceil(8)).ok()?;
        Some(layout)
    }
}

#[derive(Debug, Clone, Copy)]
enum Usage {
    Single(u16, u16),
    Range(u16, u16, u16),
}

#[derive(Debug, Clone, Copy)]
struct Open {
    application: bool,
    page: u16,
    usage: u16,
}

#[derive(Debug, Default)]
struct Walker {
    global: Global,
    stack: Vec<Global>,
    usages: Vec<Usage>,
    pending_minimum: Option<(u16, u16)>,
    usage_items: usize,
    collections: Vec<Open>,
    report_ids: bool,
    offsets: BTreeMap<u8, u32>,
    partial: BTreeMap<u8, Partial>,
}

impl Walker {
    /// Whether fields at this point belong to a pen: the innermost
    /// application collection names a pen, or a digitizer collection holds
    /// a Stylus.
    fn in_pen(&self) -> bool {
        let Some(application) = self.collections.iter().rposition(|open| open.application) else {
            return false;
        };
        let top = self.collections[application];
        if pen_application(top.page, top.usage) {
            return true;
        }
        matches!(equivalent(top.page, top.usage), Some((PAGE_DIGITIZER, _)))
            && self.collections[application..].iter().any(|open| {
                matches!(
                    equivalent(open.page, open.usage),
                    Some((PAGE_DIGITIZER, DIG_STYLUS))
                )
            })
    }

    fn usage_at(&self, index: u32) -> Option<(u16, u16)> {
        let mut remaining = index;
        let mut last = None;
        for usage in &self.usages {
            match *usage {
                Usage::Single(page, id) => {
                    if remaining == 0 {
                        return Some((page, id));
                    }
                    remaining -= 1;
                    last = Some((page, id));
                }
                Usage::Range(page, minimum, maximum) => {
                    let count = u32::from(maximum - minimum) + 1;
                    if remaining < count {
                        let id = u16::try_from(u32::from(minimum) + remaining).ok()?;
                        return Some((page, id));
                    }
                    remaining -= count;
                    last = Some((page, maximum));
                }
            }
        }
        // Fewer usages than fields: the last one repeats (HID 1.11 §6.2.2.8).
        last
    }

    fn input(&mut self, flags: u32) {
        let global = self.global;
        let id = global.report_id;
        let size = global.report_size;
        let count = global.report_count;
        let offset = self.offsets.entry(id).or_insert(0);
        let start = *offset;
        let total = size.saturating_mul(count);
        *offset = offset.saturating_add(total);
        let constant = flags & 0x01 != 0;
        let variable = flags & 0x02 != 0;
        let relative = flags & 0x04 != 0;
        if constant || !variable || relative || size == 0 || size > MAX_PEN_FIELD_BITS {
            return;
        }
        if !self.in_pen() {
            return;
        }
        let logical_max = maximum(
            global.logical_min,
            global.logical_max_raw,
            global.logical_max_size,
        );
        let physical_max = maximum(
            global.physical_min,
            global.physical_max_raw,
            global.physical_max_size,
        );
        let fields = count.min(u32::try_from(MAX_LOCAL_USAGES).unwrap_or(u32::MAX));
        for index in 0..fields {
            let Some(bit_offset) = start.checked_add(index * size) else {
                return;
            };
            if bit_offset.saturating_add(size) > MAX_PEN_REPORT_BITS {
                return;
            }
            let Some((page, usage)) = self.usage_at(index) else {
                return;
            };
            let Some(role) = role(page, usage) else {
                continue;
            };
            let partial = self.partial.entry(id).or_default();
            if partial.fields.len() >= MAX_LOCAL_USAGES {
                return;
            }
            partial.fields.push((
                role,
                PenField {
                    bit_offset,
                    bits: size,
                    logical_min: global.logical_min,
                    logical_max,
                    physical_min: global.physical_min,
                    physical_max,
                    unit: global.unit,
                    unit_exponent: global.unit_exponent,
                },
            ));
        }
    }

    fn item(
        &mut self,
        item_type: u8,
        tag: u8,
        size: u8,
        data: u32,
    ) -> Result<(), GenericPenRefusal> {
        match item_type {
            0 => {
                let result = self.main(tag, data);
                self.usages.clear();
                self.usage_items = 0;
                self.pending_minimum = None;
                result
            }
            1 => self.global_item(tag, size, data),
            2 => self.local_item(tag, size, data),
            _ => Ok(()),
        }
    }

    fn main(&mut self, tag: u8, data: u32) -> Result<(), GenericPenRefusal> {
        match tag {
            0x8 => {
                self.input(data);
                Ok(())
            }
            0xA => {
                if self.collections.len() >= MAX_COLLECTION_DEPTH {
                    return Err(GenericPenRefusal::Malformed);
                }
                let (page, usage) = match self.usages.first() {
                    Some(Usage::Single(page, usage) | Usage::Range(page, usage, _)) => {
                        (*page, *usage)
                    }
                    None => (self.global.usage_page, 0),
                };
                self.collections.push(Open {
                    application: self.collections.is_empty()
                        || data & 0xFF == COLLECTION_APPLICATION,
                    page,
                    usage,
                });
                Ok(())
            }
            0xC => self
                .collections
                .pop()
                .map(|_| ())
                .ok_or(GenericPenRefusal::Malformed),
            _ => Ok(()),
        }
    }

    fn global_item(&mut self, tag: u8, size: u8, data: u32) -> Result<(), GenericPenRefusal> {
        let global = &mut self.global;
        match tag {
            0x0 => global.usage_page = u16::try_from(data & 0xFFFF).unwrap_or(0),
            0x1 => global.logical_min = sign_extend(data, size),
            0x2 => {
                global.logical_max_raw = data;
                global.logical_max_size = size;
            }
            0x3 => global.physical_min = sign_extend(data, size),
            0x4 => {
                global.physical_max_raw = data;
                global.physical_max_size = size;
            }
            0x5 => {
                let nibble = i8::from_le_bytes([data.to_le_bytes()[0] & 0x0F]);
                global.unit_exponent = if nibble >= 8 { nibble - 16 } else { nibble };
            }
            0x6 => global.unit = data,
            0x7 => global.report_size = data,
            0x8 => {
                let id = u8::try_from(data).map_err(|_| GenericPenRefusal::Malformed)?;
                if id == 0 {
                    return Err(GenericPenRefusal::Malformed);
                }
                global.report_id = id;
                self.report_ids = true;
            }
            0x9 => global.report_count = data,
            0xA => {
                if self.stack.len() >= MAX_GLOBAL_STACK_DEPTH {
                    return Err(GenericPenRefusal::Malformed);
                }
                self.stack.push(self.global);
            }
            0xB => self.global = self.stack.pop().ok_or(GenericPenRefusal::Malformed)?,
            _ => {}
        }
        Ok(())
    }

    fn local_item(&mut self, tag: u8, size: u8, data: u32) -> Result<(), GenericPenRefusal> {
        let [low, high, page_low, page_high] = data.to_le_bytes();
        let usage = if size == 4 {
            (
                u16::from_le_bytes([page_low, page_high]),
                u16::from_le_bytes([low, high]),
            )
        } else {
            (self.global.usage_page, u16::from_le_bytes([low, high]))
        };
        if matches!(tag, 0x0..=0x2) {
            self.usage_items += 1;
            if self.usage_items > MAX_LOCAL_USAGES {
                return Err(GenericPenRefusal::Malformed);
            }
        }
        match tag {
            0x0 => self.usages.push(Usage::Single(usage.0, usage.1)),
            0x1 => self.pending_minimum = Some(usage),
            0x2 => {
                if let Some((page, minimum)) = self.pending_minimum.take()
                    && page == usage.0
                    && minimum <= usage.1
                {
                    self.usages.push(Usage::Range(page, minimum, usage.1));
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// Parses one interface's report descriptor into its pen reports.
///
/// # Errors
///
/// [`GenericPenRefusal::Malformed`] for a descriptor beyond the parser's
/// bounds; [`GenericPenRefusal::NoPenReport`] when it declares no readable
/// pen report.
pub fn generic_pen_layout(descriptor: &[u8]) -> Result<GenericPenLayout, GenericPenRefusal> {
    if descriptor.is_empty() || descriptor.len() > MAX_HID_REPORT_DESCRIPTOR_BYTES {
        return Err(GenericPenRefusal::Malformed);
    }
    let mut walker = Walker::default();
    let mut offset = 0_usize;
    while offset < descriptor.len() {
        let prefix = descriptor[offset];
        if prefix == 0xFE {
            let size = descriptor
                .get(offset + 1)
                .copied()
                .ok_or(GenericPenRefusal::Malformed)?;
            let end = offset + 3 + usize::from(size);
            if end > descriptor.len() {
                return Err(GenericPenRefusal::Malformed);
            }
            offset = end;
            continue;
        }
        let size: u8 = match prefix & 0x03 {
            3 => 4,
            other => other,
        };
        let start = offset + 1;
        let end = start + usize::from(size);
        let bytes = descriptor
            .get(start..end)
            .ok_or(GenericPenRefusal::Malformed)?;
        let mut data = [0_u8; 4];
        data[..bytes.len()].copy_from_slice(bytes);
        walker.item(
            (prefix >> 2) & 0x03,
            prefix >> 4,
            size,
            u32::from_le_bytes(data),
        )?;
        offset = end;
    }
    if !walker.collections.is_empty() {
        return Err(GenericPenRefusal::Malformed);
    }
    let report_ids = walker.report_ids;
    let reports: Vec<PenReportLayout> = walker
        .partial
        .iter()
        // With report ids in use, a field declared before the first one is
        // not part of any report a device sends.
        .filter(|(id, _)| !report_ids || **id != 0)
        .filter_map(|(id, partial)| partial.finish(*id))
        .take(MAX_PEN_REPORTS)
        .collect();
    if reports.is_empty() {
        return Err(GenericPenRefusal::NoPenReport);
    }
    Ok(GenericPenLayout {
        report_ids,
        reports,
    })
}

/// Turns one interface's reports into pen samples, by its
/// [`GenericPenLayout`]. Sans-I/O, like [`crate::IntuosPenReader`].
#[derive(Debug, Clone)]
pub struct GenericPenReader {
    layout: GenericPenLayout,
    eraser: bool,
    last: Option<HostPenSample>,
    /// When the last in-proximity sample was read, if its report had no In
    /// Range (only then does silence end proximity).
    quiet_since: Option<Instant>,
    packets: u64,
    ignored: u64,
}

impl GenericPenReader {
    #[must_use]
    pub const fn new(layout: GenericPenLayout) -> Self {
        Self {
            layout,
            eraser: false,
            last: None,
            quiet_since: None,
            packets: 0,
            ignored: 0,
        }
    }

    /// The layout this reader reads by.
    #[must_use]
    pub const fn layout(&self) -> &GenericPenLayout {
        &self.layout
    }

    /// Pen reports read so far, and reports that produced no sample
    /// (another report id, a short report, a pen out of range).
    #[must_use]
    pub const fn counts(&self) -> (u64, u64) {
        (self.packets, self.ignored)
    }

    /// One input report of the interface, verbatim. A sample when the pen
    /// is in range or just left it; `None` otherwise.
    pub fn read(&mut self, report: &[u8]) -> Option<HostPenSample> {
        self.read_at(report, Instant::now())
    }

    /// [`Self::read`], with the time the report arrived.
    pub fn read_at(&mut self, report: &[u8], at: Instant) -> Option<HostPenSample> {
        let Some((layout, data)) = self.select(report) else {
            self.ignored += 1;
            return None;
        };
        let layout = layout.clone();
        let Some(sample) = self.sample(&layout, data) else {
            self.ignored += 1;
            return None;
        };
        self.packets += 1;
        if sample.in_proximity {
            self.last = Some(sample);
            // A pen held still on the surface may send nothing (SET_IDLE 0
            // reports only changes); the tip's release is a report, so only
            // a hovering pen leaves by silence.
            self.quiet_since = (layout.in_range.is_none() && !sample.touching).then_some(at);
            return Some(sample);
        }
        self.exit()
    }

    /// A pen whose reports carry no In Range leaves proximity by going
    /// quiet: once [`GENERIC_QUIET_EXIT`] has passed since its last report
    /// while hovering (never while touching), the exit sample. Call it
    /// periodically; `None` otherwise.
    pub fn expire(&mut self, now: Instant) -> Option<HostPenSample> {
        let since = self.quiet_since?;
        if now.saturating_duration_since(since) < GENERIC_QUIET_EXIT {
            return None;
        }
        self.exit()
    }

    /// One exit at the last position, then nothing.
    fn exit(&mut self) -> Option<HostPenSample> {
        self.quiet_since = None;
        self.eraser = false;
        let last = self.last.take()?;
        Some(HostPenSample {
            pressure: 0.0,
            in_proximity: false,
            touching: false,
            barrel_buttons: 0,
            ..last
        })
    }

    fn select<'r>(&self, report: &'r [u8]) -> Option<(&PenReportLayout, &'r [u8])> {
        if self.layout.report_ids {
            let (&id, data) = report.split_first()?;
            self.layout
                .reports
                .iter()
                .find(|layout| layout.report_id == id)
                .map(|layout| (layout, data))
        } else {
            self.layout.reports.first().map(|layout| (layout, report))
        }
    }

    fn sample(&mut self, layout: &PenReportLayout, data: &[u8]) -> Option<HostPenSample> {
        if data.len() < layout.data_bytes {
            return None;
        }
        let switch = |field: Option<PenField>| -> Option<bool> {
            field.map(|field| {
                field
                    .read(data)
                    .is_some_and(|value| value > field.logical_min.max(0))
            })
        };
        let raw_x = layout.x.read(data)?;
        let raw_y = layout.y.read(data)?;
        let max_x = layout.x.span()?;
        let max_y = layout.y.span()?;
        let device = |field: &PenField, raw: i64, max: u32| -> u32 {
            u32::try_from(raw.clamp(field.logical_min, field.logical_max) - field.logical_min)
                .unwrap_or(0)
                .min(max)
        };
        let device_x = device(&layout.x, raw_x, max_x);
        let device_y = device(&layout.y, raw_y, max_y);
        let pressure_fraction = match layout.pressure {
            Some(field) => Some(field.fraction(field.read(data)?)),
            None => None,
        };
        let tip = switch(layout.tip);
        let eraser_touch = switch(layout.eraser).unwrap_or(false);
        let invert = switch(layout.invert).unwrap_or(false);
        #[allow(clippy::cast_possible_truncation)]
        let touching = match tip {
            Some(tip) => tip || eraser_touch,
            None => {
                eraser_touch
                    || pressure_fraction.is_some_and(|p| p as f32 > GENERIC_TOUCH_THRESHOLD)
            }
        };
        let in_proximity = match switch(layout.in_range) {
            Some(in_range) => in_range || touching,
            // A pen without In Range reports only while it is near.
            None => true,
        };
        if in_proximity {
            if invert || eraser_touch {
                self.eraser = true;
            } else if self.last.is_none() {
                self.eraser = false;
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        let pressure = if touching {
            pressure_fraction.map_or(1.0, |p| p as f32)
        } else {
            0.0
        };
        let unit = |value: u32, max: u32| f64::from(value) / (f64::from(max) + 1.0);
        #[allow(clippy::cast_possible_truncation)]
        let angle = |field: Option<PenField>, limit: f64| -> f32 {
            field
                .and_then(|field| field.read(data).map(|value| field.degrees(value)))
                .map_or(0.0, |degrees| degrees.clamp(-limit, limit) as f32)
        };
        #[allow(clippy::cast_possible_truncation)]
        let rotation = layout
            .twist
            .and_then(|field| field.read(data).map(|value| field.turn_degrees(value)))
            .map_or(0.0, |degrees| (degrees as f32).min(359.999));
        let barrel = switch(layout.barrel).unwrap_or(false);
        let secondary = switch(layout.secondary_barrel).unwrap_or(false);
        Some(HostPenSample {
            x: unit(device_x, max_x),
            y: unit(device_y, max_y),
            device_x,
            device_y,
            device_max_x: max_x,
            device_max_y: max_y,
            pressure: pressure.clamp(0.0, 1.0),
            tilt_x_degrees: angle(layout.tilt_x, 90.0),
            tilt_y_degrees: angle(layout.tilt_y, 90.0),
            rotation_degrees: rotation,
            eraser: self.eraser,
            in_proximity,
            touching,
            barrel_buttons: u16::from(barrel) | (u16::from(secondary) << 1),
        })
    }
}

#[cfg(test)]
#[path = "generic_pen_tests.rs"]
mod tests;
