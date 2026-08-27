//! Pure, allocation-light native tablet sample types.
//!
//! Everything in this file is `#![forbid(unsafe_code)]`-safe and has no
//! AppKit/objc2 dependency: it exists so the AppKit → [`arcen_input::PenEvent`]
//! mapping in [`super::mapper`] can be unit tested without a display, a real
//! tablet, or a running `NSApplication`. The AppKit-facing code that fills
//! these structs in from live `NSEvent`s lives in [`super::monitor`] (macOS
//! only) and contains the `unsafe` FFI reads.
#![forbid(unsafe_code)]

use std::collections::VecDeque;

/// Pointing-device kind reported by `-[NSEvent pointingDeviceType]`.
///
/// Mirrors `NSPointingDeviceType` (`NSPointingDeviceTypeUnknown` = 0,
/// `...Pen` = 1, `...Cursor` = 2, `...Eraser` = 3) without depending on
/// `objc2-app-kit` so this type stays usable from pure unit tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NativeTabletTool {
    #[default]
    Unknown,
    Pen,
    /// A tethered/absolute cursor puck rather than a stylus.
    Cursor,
    Eraser,
}

impl NativeTabletTool {
    /// Decode the raw `NSPointingDeviceType` integer value.
    #[must_use]
    pub const fn from_ns_pointing_device_type(raw: u64) -> Self {
        match raw {
            1 => Self::Pen,
            2 => Self::Cursor,
            3 => Self::Eraser,
            _ => Self::Unknown,
        }
    }

    /// Only real stylus ends participate in typed pen local termination.
    ///
    /// macOS trackpads can emit tablet-shaped AppKit events with an unknown
    /// pointing-device type. Cursor/puck tools are likewise outside Arcen's
    /// current pen/eraser contract. Rejecting both prevents either source from
    /// taking pen authority and suppressing ordinary mouse/trackpad input.
    #[must_use]
    pub const fn is_pen_or_eraser(self) -> bool {
        matches!(self, Self::Pen | Self::Eraser)
    }
}

/// Bit positions of `-[NSEvent buttonMask]` (`NSEventButtonMask`).
///
/// AppKit defines exactly three named bits for tablet events:
/// `NSEventButtonMaskPenTip` (1), `...PenLowerSide` (2), `...PenUpperSide` (4).
/// The lower/upper side bits are the two documented barrel buttons; the tip
/// bit indicates the tip switch (contact), not a barrel button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TabletButtonMask(pub u16);

impl TabletButtonMask {
    pub const TIP: u16 = 1 << 0;
    pub const LOWER_SIDE: u16 = 1 << 1;
    pub const UPPER_SIDE: u16 = 1 << 2;
    const BARREL_MASK: u16 = Self::LOWER_SIDE | Self::UPPER_SIDE;

    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    #[must_use]
    pub const fn tip_down(self) -> bool {
        self.0 & Self::TIP != 0
    }

    /// The two documented barrel-button bits, with the tip bit masked off so
    /// contact state is never mistaken for a barrel button.
    #[must_use]
    pub const fn barrel_bits(self) -> u16 {
        self.0 & Self::BARREL_MASK
    }
}

/// A window-space point sample, in AppKit's native point coordinate space
/// (origin at the bottom-left of the window, Y increasing upward) before
/// normalization.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeTabletPoint {
    /// Window-relative X in points, AppKit's native (bottom-left) origin.
    pub window_x: f64,
    /// Window-relative Y in points, AppKit's native (bottom-left) origin.
    pub window_y: f64,
    /// `-[NSEvent pressure]`, documented range 0.0..=1.0.
    pub pressure: f32,
    /// `-[NSEvent tilt].x`, documented range -1.0..=1.0 (perpendicular = 0).
    pub tilt_x: f32,
    /// `-[NSEvent tilt].y`, documented range -1.0..=1.0 (perpendicular = 0).
    pub tilt_y: f32,
    /// `-[NSEvent rotation]` in degrees. AppKit does not guarantee a 0..360
    /// vs. -180..180 convention across drivers, so the mapper normalizes this
    /// into 0..360 rather than trusting the raw sign/range.
    pub rotation_degrees: f32,
    pub buttons: TabletButtonMask,
    /// `-[NSEvent deviceID]`, used only to detect the tool identity changing
    /// underneath us; never logged.
    pub device_id: u64,
    /// `-[NSEvent pointingDeviceType]`. Unknown identifies tablet-shaped
    /// events such as macOS trackpad pressure and must never be treated as pen.
    pub tool: NativeTabletTool,
    /// `-[NSEvent windowNumber]`: the exact native window this sample was
    /// delivered against. In multi-monitor-v1, root and every secondary
    /// fullscreen viewport are each backed by a distinct `NSWindow`, so this
    /// is the only reliable way to know which viewport's own rect a sample
    /// must be normalized through -- `window_x`/`window_y` alone are
    /// meaningless without knowing which window's coordinate space they are
    /// in. A single-window legacy session still populates this field, but
    /// every caller has exactly one window to match it against.
    pub window_number: isize,
}

/// A proximity transition sample, from `NSEventTypeTabletProximity`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NativeTabletProximity {
    /// Window-relative X in points, AppKit's native (bottom-left) origin.
    /// `-[NSEvent locationInWindow]` is valid on every `NSEvent`, including
    /// proximity events, so a proximity transition can still be placed.
    pub window_x: f64,
    /// Window-relative Y in points, AppKit's native (bottom-left) origin.
    pub window_y: f64,
    /// `-[NSEvent isEnteringProximity]`: true when the tool comes into range,
    /// false when it leaves.
    pub entering: bool,
    /// `-[NSEvent pointingDeviceType]`.
    pub tool: NativeTabletTool,
    /// `-[NSEvent vendorID]`. Wacom reports `0x056a` (`WACOM_USB_VENDOR_ID`).
    pub vendor_id: u64,
    pub tablet_id: u64,
    pub pointing_device_id: u64,
    pub system_tablet_id: u64,
    pub vendor_pointing_device_type: u64,
    pub unique_id: u64,
    /// `-[NSEvent capabilityMask]`. Intentionally left as an opaque raw value:
    /// modern Apple documentation does not publish stable per-axis bit
    /// semantics for this field, so this mapper never decodes it into an
    /// axis-capability claim. Axis capability is instead established
    /// empirically from observed samples — see [`super::probe`].
    pub capability_mask: u64,
    pub device_id: u64,
    /// `-[NSEvent windowNumber]`. See
    /// [`NativeTabletPoint::window_number`]'s doc for why this is required
    /// to correctly route a sample in multi-monitor-v1.
    pub window_number: isize,
}

/// A single delivered native sample, as queued by the AppKit monitor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RawTabletSample {
    Point(NativeTabletPoint),
    Proximity(NativeTabletProximity),
}

impl RawTabletSample {
    /// The native window this sample was delivered against, regardless of
    /// which variant it is. See [`NativeTabletPoint::window_number`]'s doc
    /// for why callers must match this against a known viewport rather than
    /// assuming every sample belongs to root.
    #[must_use]
    pub const fn window_number(self) -> isize {
        match self {
            Self::Point(point) => point.window_number,
            Self::Proximity(proximity) => proximity.window_number,
        }
    }
}

/// A fixed-capacity FIFO queue that never blocks its producer.
///
/// When full, the oldest queued sample is dropped to make room for the
/// newest one and the drop counter is incremented. This bounds memory/latency
/// for a producer that must never block (an AppKit event-monitor callback
/// running on the main thread) while still surfacing loss instead of hiding
/// it silently.
#[derive(Debug)]
pub struct BoundedSampleQueue<T> {
    capacity: usize,
    items: VecDeque<T>,
    dropped: u64,
}

impl<T> BoundedSampleQueue<T> {
    /// # Panics
    /// Panics if `capacity` is zero; a zero-capacity bounded queue can never
    /// deliver anything, which is never the intended configuration.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "bounded sample queue capacity must be > 0");
        Self {
            capacity,
            items: VecDeque::with_capacity(capacity),
            dropped: 0,
        }
    }

    /// Push a sample, dropping the oldest queued sample if already at
    /// capacity. Never blocks and never grows past `capacity`.
    pub fn push(&mut self, item: T) {
        if self.items.len() == self.capacity {
            self.items.pop_front();
            self.dropped = self.dropped.saturating_add(1);
        }
        self.items.push_back(item);
    }

    /// Drain every currently queued sample, oldest first.
    pub fn drain(&mut self) -> Vec<T> {
        self.items.drain(..).collect()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.items.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped
    }

    #[must_use]
    pub const fn capacity(&self) -> usize {
        self.capacity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_tablet_sample_window_number_reads_through_either_variant() {
        let point = NativeTabletPoint {
            window_x: 0.0,
            window_y: 0.0,
            pressure: 0.0,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation_degrees: 0.0,
            buttons: TabletButtonMask::new(0),
            device_id: 1,
            tool: NativeTabletTool::Pen,
            window_number: 7,
        };
        let proximity = NativeTabletProximity {
            window_x: 0.0,
            window_y: 0.0,
            entering: true,
            tool: NativeTabletTool::Pen,
            vendor_id: 0,
            tablet_id: 0,
            pointing_device_id: 0,
            system_tablet_id: 0,
            vendor_pointing_device_type: 0,
            unique_id: 0,
            capability_mask: 0,
            device_id: 1,
            window_number: 9,
        };

        assert_eq!(RawTabletSample::Point(point).window_number(), 7);
        assert_eq!(RawTabletSample::Proximity(proximity).window_number(), 9);
    }

    #[test]
    fn pointing_device_type_decodes_known_values() {
        assert_eq!(
            NativeTabletTool::from_ns_pointing_device_type(0),
            NativeTabletTool::Unknown
        );
        assert_eq!(
            NativeTabletTool::from_ns_pointing_device_type(1),
            NativeTabletTool::Pen
        );
        assert_eq!(
            NativeTabletTool::from_ns_pointing_device_type(2),
            NativeTabletTool::Cursor
        );
        assert_eq!(
            NativeTabletTool::from_ns_pointing_device_type(3),
            NativeTabletTool::Eraser
        );
    }

    #[test]
    fn pointing_device_type_treats_unrecognized_values_as_unknown() {
        assert_eq!(
            NativeTabletTool::from_ns_pointing_device_type(99),
            NativeTabletTool::Unknown
        );
    }

    #[test]
    fn only_pen_and_eraser_are_typed_pen_sources() {
        assert!(NativeTabletTool::Pen.is_pen_or_eraser());
        assert!(NativeTabletTool::Eraser.is_pen_or_eraser());
        assert!(!NativeTabletTool::Unknown.is_pen_or_eraser());
        assert!(!NativeTabletTool::Cursor.is_pen_or_eraser());
    }

    #[test]
    fn button_mask_separates_tip_from_barrel_bits() {
        let tip_only = TabletButtonMask::new(TabletButtonMask::TIP);
        assert!(tip_only.tip_down());
        assert_eq!(tip_only.barrel_bits(), 0);

        let both_barrel =
            TabletButtonMask::new(TabletButtonMask::LOWER_SIDE | TabletButtonMask::UPPER_SIDE);
        assert!(!both_barrel.tip_down());
        assert_eq!(
            both_barrel.barrel_bits(),
            TabletButtonMask::LOWER_SIDE | TabletButtonMask::UPPER_SIDE
        );

        let everything = TabletButtonMask::new(0xFFFF);
        assert!(everything.tip_down());
        assert_eq!(
            everything.barrel_bits(),
            TabletButtonMask::LOWER_SIDE | TabletButtonMask::UPPER_SIDE
        );
    }

    #[test]
    fn bounded_queue_drops_oldest_when_full_and_counts_drops() {
        let mut queue: BoundedSampleQueue<u32> = BoundedSampleQueue::new(2);
        queue.push(1);
        queue.push(2);
        assert_eq!(queue.dropped_count(), 0);
        queue.push(3);
        assert_eq!(queue.dropped_count(), 1);
        assert_eq!(queue.drain(), vec![2, 3]);
        assert!(queue.is_empty());
    }

    #[test]
    fn bounded_queue_never_exceeds_capacity() {
        let mut queue: BoundedSampleQueue<u32> = BoundedSampleQueue::new(4);
        for value in 0..100u32 {
            queue.push(value);
            assert!(queue.len() <= queue.capacity());
        }
        assert_eq!(queue.dropped_count(), 96);
    }

    #[test]
    #[should_panic(expected = "capacity must be > 0")]
    fn bounded_queue_rejects_zero_capacity() {
        let _: BoundedSampleQueue<u32> = BoundedSampleQueue::new(0);
    }
}
