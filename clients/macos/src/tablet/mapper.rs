//! Pure AppKit-sample → [`arcen_input::PenEvent`] mapper.
//!
//! This is deliberately free of any AppKit/objc2 dependency so it can be unit
//! tested with synthetic samples — no display, tablet, or `NSApplication`
//! required. [`super::monitor`] (macOS only) is the only caller that touches
//! live `NSEvent`s; it fills in [`super::sample::NativeTabletPoint`] and
//! [`super::sample::NativeTabletProximity`] and hands them to this mapper.
//!
//! Established tablet behavior: pointer type selects pen vs. eraser, tip
//! contact/proximity are tracked independently of pressure alone, and a
//! proximity leave/enter transition is always surfaced even when no point
//! sample accompanies it. Field-level unit conversions (tilt, rotation) are
//! justified in the doc comments below.
#![forbid(unsafe_code)]

use arcen_input::{LowLatencyMetadata, PenEvent, PenTool};

use super::sample::{NativeTabletPoint, NativeTabletProximity, NativeTabletTool};

/// The target area a native AppKit window-space point is normalized into.
///
/// AppKit delivers `locationInWindow` in the window's own native coordinate
/// space: origin bottom-left, Y increasing upward, in points (already
/// resolution-independent of Retina backing scale — a 2x backing scale
/// changes physical pixels per point, not the point value itself, so no
/// separate backing-scale factor is needed as long as every quantity here
/// stays in points). Deck's video is drawn into `image_rect`, a centered
/// aspect-fit **sub-rectangle** of the window content (the same rectangle
/// `ui/app.rs::viewer_input_surface` computes for mouse input, accounting
/// for letterboxing and fullscreen transitions every frame) in egui's
/// top-left-origin point space. [`ViewSize::within_window`] captures exactly
/// the two pieces of information needed to map one into the other: the
/// window's total content height (for the bottom-left → top-left flip) and
/// `image_rect`'s bounds within that same flipped space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewSize {
    /// AppKit window content height in points, used only to flip
    /// `window_y` from bottom-left to top-left origin.
    window_height: f64,
    /// `image_rect.left()` in the window's top-left-origin point space.
    image_left: f64,
    /// `image_rect.top()` in the window's top-left-origin point space.
    image_top: f64,
    image_width: f64,
    image_height: f64,
}

impl ViewSize {
    /// A mapping target that *is* the whole window (no letterboxing): the
    /// simple case used before `image_rect`-aware mapping existed, kept for
    /// callers/tests that only care about a bare width/height area.
    #[must_use]
    pub const fn new(width: f64, height: f64) -> Self {
        Self {
            window_height: height,
            image_left: 0.0,
            image_top: 0.0,
            image_width: width,
            image_height: height,
        }
    }

    /// The real integration mapping: `image_rect` (Deck's current
    /// letterboxed video sub-rectangle, in egui's top-left-origin point
    /// space) positioned within a window of `window_height` points.
    #[must_use]
    pub const fn within_window(
        window_height: f64,
        image_left: f64,
        image_top: f64,
        image_width: f64,
        image_height: f64,
    ) -> Self {
        Self {
            window_height,
            image_left,
            image_top,
            image_width,
            image_height,
        }
    }

    fn is_valid(self) -> bool {
        self.window_height.is_finite()
            && self.window_height > 0.0
            && self.image_left.is_finite()
            && self.image_top.is_finite()
            && self.image_width.is_finite()
            && self.image_height.is_finite()
            && self.image_width > 0.0
            && self.image_height > 0.0
    }
}

/// Stateful, pure AppKit-sample → `PenEvent` mapper.
///
/// Holds only the minimal state a single native sample cannot carry by
/// itself: which tool is currently in proximity (from the last proximity
/// sample) and whether we are in proximity at all. The caller supplies
/// sequencing/timestamp metadata explicitly, so this type has no wall-clock
/// or global-counter dependency and stays fully deterministic in tests.
#[derive(Debug, Clone, Copy)]
pub struct TabletMapper {
    tool: PenTool,
    in_proximity: bool,
}

impl Default for TabletMapper {
    fn default() -> Self {
        Self {
            tool: PenTool::Tip,
            in_proximity: false,
        }
    }
}

impl TabletMapper {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn in_proximity(&self) -> bool {
        self.in_proximity
    }

    #[must_use]
    pub const fn tool(&self) -> PenTool {
        self.tool
    }

    /// Handle a proximity transition (`NSEventTypeTabletProximity`).
    ///
    /// Always attempts to produce a `PenEvent` (even on leave-proximity) so a
    /// proximity edge is never silently dropped: the later bounded input
    /// dispatcher (todo 4) may coalesce superseded hover/move samples, but
    /// proximity/tool edges must survive. Returns `None` only when the
    /// sample cannot be placed (degenerate view size) or fails contract
    /// validation (non-finite/out-of-range input) — an explicit rejection,
    /// never a synthesized fallback value.
    pub fn on_proximity(
        &mut self,
        sample: NativeTabletProximity,
        view: ViewSize,
        metadata: LowLatencyMetadata,
    ) -> Option<PenEvent> {
        let (x, y) = normalize(sample.window_x, sample.window_y, view)?;

        let tool = pen_tool(sample.tool)?;
        self.in_proximity = sample.entering;
        self.tool = tool;

        // A proximity transition carries no pressure/tilt/rotation/button
        // payload of its own (those only arrive on `NSEventTypeTabletPoint`).
        // Report a clean zeroed sample on both enter and leave rather than
        // carrying over stale point-sample state from before the transition.
        let event = PenEvent {
            x,
            y,
            pressure: 0.0,
            tilt_x_degrees: 0.0,
            tilt_y_degrees: 0.0,
            rotation_degrees: 0.0,
            tool: self.tool,
            in_proximity: self.in_proximity,
            touching: false,
            buttons: 0,
            metadata,
        };
        event.validate().ok()?;
        Some(event)
    }

    /// Handle a point/motion sample (`NSEventTypeTabletPoint`).
    ///
    /// `NSEventTypeTabletPoint` is only ever delivered by AppKit while a tool
    /// is already in proximity, so receiving one is treated as authoritative
    /// proof of proximity even if an entering-proximity sample was somehow
    /// missed (self-healing state, not a synthesized value: the very
    /// existence of the point event is the proof).
    pub fn on_point(
        &mut self,
        sample: NativeTabletPoint,
        view: ViewSize,
        metadata: LowLatencyMetadata,
    ) -> Option<PenEvent> {
        let (x, y) = normalize(sample.window_x, sample.window_y, view)?;

        let tool = pen_tool(sample.tool)?;
        self.in_proximity = true;
        self.tool = tool;

        let pressure = sample.pressure.clamp(0.0, 1.0);
        let tilt_x_degrees = tilt_to_degrees(sample.tilt_x);
        let tilt_y_degrees = tilt_to_degrees(sample.tilt_y);
        let rotation_degrees = normalize_rotation(sample.rotation_degrees);
        // The tip button bit is contact, not a barrel button; pressure > 0 is
        // an additional signal some drivers only expose through pressure.
        let touching = sample.buttons.tip_down() || pressure > 0.0;
        let buttons = sample.buttons.barrel_bits();

        let event = PenEvent {
            x,
            y,
            pressure,
            tilt_x_degrees,
            tilt_y_degrees,
            rotation_degrees,
            tool: self.tool,
            in_proximity: true,
            touching,
            buttons,
            metadata,
        };
        event.validate().ok()?;
        Some(event)
    }
}

/// `arcen_input::PenEvent` only models tip vs. eraser. Cursor/puck and unknown
/// sources (including macOS trackpad tablet-shaped events) fail closed rather
/// than being mislabeled as a pen and taking input authority.
fn pen_tool(native: NativeTabletTool) -> Option<PenTool> {
    match native {
        NativeTabletTool::Pen => Some(PenTool::Tip),
        NativeTabletTool::Eraser => Some(PenTool::Eraser),
        NativeTabletTool::Cursor | NativeTabletTool::Unknown => None,
    }
}

/// Normalize an AppKit window-space point (origin bottom-left, Y increasing
/// upward) into Deck's `0.0..=1.0` coordinate space (origin top-left, Y
/// increasing downward — the same convention `ui/app.rs::normalized_pointer`
/// already uses for mouse input), mapped through `view`'s `image_rect`
/// sub-rectangle rather than the whole window. Returns `None` for a
/// degenerate view size or a non-finite result rather than guessing a value.
///
/// Two steps, in order:
/// 1. Flip AppKit's bottom-left-origin `window_y` into the same
///    top-left-origin space `image_rect` is expressed in
///    (`view.window_height - window_y`).
/// 2. Normalize against `image_rect`'s bounds exactly like
///    `ui/app.rs::normalized_pointer` normalizes a mouse position against
///    the same rectangle, so a pen sample and a mouse sample at the same
///    physical spot always resolve to the same normalized coordinate —
///    including while letterboxed and across a fullscreen transition, since
///    `image_rect` is recomputed every frame from the current window/video
///    geometry.
fn normalize(window_x: f64, window_y: f64, view: ViewSize) -> Option<(f64, f64)> {
    if !view.is_valid() {
        return None;
    }
    let top_left_y = view.window_height - window_y;
    let x = ((window_x - view.image_left) / view.image_width).clamp(0.0, 1.0);
    let y = ((top_left_y - view.image_top) / view.image_height).clamp(0.0, 1.0);
    if !x.is_finite() || !y.is_finite() {
        return None;
    }
    Some((x, y))
}

/// Convert AppKit's normalized `-1.0..=1.0` tilt axis into the `-90.0..=90.0`
/// degree range `arcen_input::PenEvent` validates.
///
/// AppKit's `tilt` property is explicitly documented as a normalized `-1..1`
/// value, not degrees. The `* 90` scaling matches the W3C Pointer Events
/// `tiltX`/`tiltY` convention (`-90..=90`, 0 = perpendicular), which several
/// browser engines apply for exactly this native-to-web conversion — it is
/// an established convention, not an invented one.
fn tilt_to_degrees(raw: f32) -> f32 {
    (raw.clamp(-1.0, 1.0) * 90.0).clamp(-90.0, 90.0)
}

/// Normalize `-[NSEvent rotation]` into `0.0..360.0`.
///
/// Apple's documentation for the degrees this reports does not commit to a
/// `0..360` vs. `-180..180` sign convention across tablet drivers. Rather
/// than assume one, this wraps any finite value into `0..360` (a full
/// rotation reads the same regardless of which sign convention the driver
/// used). Non-finite input is passed through unchanged so
/// `PenEvent::validate` rejects it explicitly instead of this function
/// silently inventing `0.0`.
fn normalize_rotation(raw: f32) -> f32 {
    if !raw.is_finite() {
        return raw;
    }
    let wrapped = raw % 360.0;
    if wrapped < 0.0 {
        wrapped + 360.0
    } else {
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tablet::sample::TabletButtonMask;

    fn metadata(sequence: u64) -> LowLatencyMetadata {
        LowLatencyMetadata {
            sequence,
            timestamp_ns: sequence,
            coalescable: false,
        }
    }

    fn view() -> ViewSize {
        ViewSize::new(1000.0, 500.0)
    }

    fn point(window_x: f64, window_y: f64, pressure: f32, buttons: u16) -> NativeTabletPoint {
        NativeTabletPoint {
            window_x,
            window_y,
            pressure,
            tilt_x: 0.0,
            tilt_y: 0.0,
            rotation_degrees: 0.0,
            buttons: TabletButtonMask::new(buttons),
            device_id: 1,
            tool: NativeTabletTool::Pen,
            window_number: 1,
        }
    }

    fn proximity(entering: bool, tool: NativeTabletTool) -> NativeTabletProximity {
        NativeTabletProximity {
            window_x: 500.0,
            window_y: 250.0,
            entering,
            tool,
            vendor_id: 0x056a,
            tablet_id: 1,
            pointing_device_id: 1,
            system_tablet_id: 1,
            vendor_pointing_device_type: 0,
            unique_id: 1,
            capability_mask: 0,
            device_id: 1,
            window_number: 1,
        }
    }

    #[test]
    fn entering_proximity_with_pen_selects_tip_tool() {
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_proximity(proximity(true, NativeTabletTool::Pen), view(), metadata(1))
            .expect("valid proximity sample maps");
        assert_eq!(event.tool, PenTool::Tip);
        assert!(event.in_proximity);
        assert!(!event.touching);
        assert!(mapper.in_proximity());
        assert_eq!(mapper.tool(), PenTool::Tip);
    }

    #[test]
    fn entering_proximity_with_eraser_selects_eraser_tool() {
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_proximity(
                proximity(true, NativeTabletTool::Eraser),
                view(),
                metadata(1),
            )
            .expect("valid proximity sample maps");
        assert_eq!(event.tool, PenTool::Eraser);
    }

    #[test]
    fn leaving_proximity_clears_touching_and_proximity() {
        let mut mapper = TabletMapper::new();
        mapper
            .on_proximity(proximity(true, NativeTabletTool::Pen), view(), metadata(1))
            .unwrap();
        let event = mapper
            .on_proximity(proximity(false, NativeTabletTool::Pen), view(), metadata(2))
            .expect("valid leave-proximity sample maps");
        assert!(!event.in_proximity);
        assert!(!event.touching);
        assert_eq!(event.pressure, 0.0);
        assert!(!mapper.in_proximity());
    }

    #[test]
    fn point_sample_self_heals_proximity_state() {
        // No prior proximity sample was delivered — the point sample alone
        // is authoritative proof the tool is in range.
        let mut mapper = TabletMapper::new();
        assert!(!mapper.in_proximity());
        let event = mapper
            .on_point(point(100.0, 100.0, 0.5, 0), view(), metadata(1))
            .expect("valid point sample maps");
        assert!(event.in_proximity);
        assert!(mapper.in_proximity());
    }

    #[test]
    fn unknown_trackpad_shaped_point_never_takes_pen_authority() {
        let mut mapper = TabletMapper::new();
        let mut sample = point(100.0, 100.0, 0.5, 0);
        sample.tool = NativeTabletTool::Unknown;
        assert!(mapper.on_point(sample, view(), metadata(1)).is_none());
        assert!(!mapper.in_proximity());
    }

    #[test]
    fn cursor_and_unknown_proximity_never_take_pen_authority() {
        for tool in [NativeTabletTool::Cursor, NativeTabletTool::Unknown] {
            let mut mapper = TabletMapper::new();
            assert!(mapper
                .on_proximity(proximity(true, tool), view(), metadata(1))
                .is_none());
            assert!(!mapper.in_proximity());
        }
    }

    #[test]
    fn tip_button_bit_indicates_contact_even_with_zero_pressure() {
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_point(
                point(0.0, 0.0, 0.0, TabletButtonMask::TIP),
                view(),
                metadata(1),
            )
            .expect("valid point sample maps");
        assert!(event.touching);
    }

    #[test]
    fn positive_pressure_indicates_contact_even_without_tip_bit() {
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_point(point(0.0, 0.0, 0.2, 0), view(), metadata(1))
            .expect("valid point sample maps");
        assert!(event.touching);
    }

    #[test]
    fn hover_with_no_tip_bit_and_zero_pressure_is_not_touching() {
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_point(point(0.0, 0.0, 0.0, 0), view(), metadata(1))
            .expect("valid point sample maps");
        assert!(!event.touching);
    }

    #[test]
    fn barrel_buttons_are_forwarded_without_the_tip_bit() {
        let mut mapper = TabletMapper::new();
        let raw =
            TabletButtonMask::TIP | TabletButtonMask::LOWER_SIDE | TabletButtonMask::UPPER_SIDE;
        let event = mapper
            .on_point(point(0.0, 0.0, 0.3, raw), view(), metadata(1))
            .expect("valid point sample maps");
        assert_eq!(
            event.buttons,
            TabletButtonMask::LOWER_SIDE | TabletButtonMask::UPPER_SIDE
        );
    }

    #[test]
    fn appkit_window_origin_maps_to_normalized_bottom_left() {
        // AppKit (0,0) is the bottom-left corner of the window; Deck's
        // normalized space keeps mouse's top-left, Y-down convention, so
        // AppKit's bottom-left corner is normalized (0, 1) — max Y.
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_point(point(0.0, 0.0, 0.1, 0), view(), metadata(1))
            .unwrap();
        assert_eq!(event.x, 0.0);
        assert_eq!(event.y, 1.0);
    }

    #[test]
    fn window_top_right_maps_to_normalized_top_right() {
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_point(point(1000.0, 500.0, 0.1, 0), view(), metadata(1))
            .unwrap();
        assert_eq!(event.x, 1.0);
        assert_eq!(event.y, 0.0);
    }

    #[test]
    fn out_of_bounds_window_coordinates_are_clamped_not_rejected() {
        let mut mapper = TabletMapper::new();
        let event = mapper
            .on_point(point(-50.0, 9000.0, 0.1, 0), view(), metadata(1))
            .expect("out-of-bounds but finite coordinates still clamp into range");
        assert_eq!(event.x, 0.0);
        assert_eq!(event.y, 0.0);
    }

    #[test]
    fn degenerate_view_size_rejects_the_sample() {
        let mut mapper = TabletMapper::new();
        let degenerate = ViewSize::new(0.0, 0.0);
        assert!(mapper
            .on_point(point(1.0, 1.0, 0.1, 0), degenerate, metadata(1))
            .is_none());
    }

    #[test]
    fn within_window_matches_whole_window_mapping_when_image_rect_fills_it() {
        // `within_window` with a zero-offset image_rect covering the whole
        // window must reproduce exactly the same result as `ViewSize::new`
        // (the simple whole-window case), proving the generalization is
        // behavior-preserving for every existing whole-window test above.
        let whole = ViewSize::new(1000.0, 500.0);
        let letterboxed = ViewSize::within_window(500.0, 0.0, 0.0, 1000.0, 500.0);
        let mut a = TabletMapper::new();
        let mut b = TabletMapper::new();
        let event_a = a.on_point(point(240.0, 360.0, 0.4, 0), whole, metadata(1));
        let event_b = b.on_point(point(240.0, 360.0, 0.4, 0), letterboxed, metadata(1));
        assert_eq!(event_a.map(|e| (e.x, e.y)), event_b.map(|e| (e.x, e.y)));
    }

    #[test]
    fn within_window_maps_through_a_letterboxed_sub_rect() {
        // A 1000x500-point window with a pillarboxed 500x500 video area
        // centered horizontally (image_rect left=250, top=0, 500x500).
        // AppKit (500, 0) is the window's bottom-center — inside the video
        // area at its bottom edge, horizontally centered.
        let mut mapper = TabletMapper::new();
        let view = ViewSize::within_window(500.0, 250.0, 0.0, 500.0, 500.0);
        let event = mapper
            .on_point(point(500.0, 0.0, 0.2, 0), view, metadata(1))
            .expect("point inside the letterboxed image_rect maps");
        assert_eq!(event.x, 0.5);
        assert_eq!(event.y, 1.0);
    }

    #[test]
    fn within_window_clamps_points_outside_the_letterboxed_sub_rect() {
        // AppKit (0, 0) — the window's true bottom-left corner — falls in
        // the left pillarbox bar, outside `image_rect` entirely. It must
        // still clamp into range rather than being rejected: a stylus can
        // physically hover over the letterbox bars.
        let mut mapper = TabletMapper::new();
        let view = ViewSize::within_window(500.0, 250.0, 0.0, 500.0, 500.0);
        let event = mapper
            .on_point(point(0.0, 0.0, 0.2, 0), view, metadata(1))
            .expect("out-of-image_rect but finite coordinates still clamp");
        assert_eq!(event.x, 0.0);
        assert_eq!(event.y, 1.0);
    }

    #[test]
    fn within_window_rejects_non_positive_window_height() {
        let mut mapper = TabletMapper::new();
        let degenerate = ViewSize::within_window(0.0, 0.0, 0.0, 500.0, 500.0);
        assert!(mapper
            .on_point(point(1.0, 1.0, 0.1, 0), degenerate, metadata(1))
            .is_none());
    }

    #[test]
    fn tilt_extremes_map_to_plus_minus_ninety_degrees() {
        assert_eq!(tilt_to_degrees(-1.0), -90.0);
        assert_eq!(tilt_to_degrees(1.0), 90.0);
        assert_eq!(tilt_to_degrees(0.0), 0.0);
    }

    #[test]
    fn tilt_beyond_documented_range_is_clamped() {
        assert_eq!(tilt_to_degrees(-2.0), -90.0);
        assert_eq!(tilt_to_degrees(2.0), 90.0);
    }

    #[test]
    fn rotation_wraps_negative_values_into_0_360() {
        assert_eq!(normalize_rotation(-90.0), 270.0);
        assert_eq!(normalize_rotation(-1.0), 359.0);
        assert_eq!(normalize_rotation(0.0), 0.0);
        assert_eq!(normalize_rotation(359.0), 359.0);
    }

    #[test]
    fn rotation_wraps_a_full_360_back_to_zero() {
        assert_eq!(normalize_rotation(360.0), 0.0);
    }

    #[test]
    fn non_finite_pressure_is_rejected_not_synthesized() {
        let mut mapper = TabletMapper::new();
        assert!(mapper
            .on_point(point(0.0, 0.0, f32::NAN, 0), view(), metadata(1))
            .is_none());
    }

    #[test]
    fn non_finite_rotation_is_rejected_not_synthesized() {
        let mut mapper = TabletMapper::new();
        let mut sample = point(0.0, 0.0, 0.1, 0);
        sample.rotation_degrees = f32::INFINITY;
        assert!(mapper.on_point(sample, view(), metadata(1)).is_none());
    }

    #[test]
    fn sequential_samples_preserve_caller_supplied_metadata() {
        let mut mapper = TabletMapper::new();
        let first = mapper
            .on_point(point(0.0, 0.0, 0.1, 0), view(), metadata(7))
            .unwrap();
        let second = mapper
            .on_point(point(0.0, 0.0, 0.2, 0), view(), metadata(8))
            .unwrap();
        assert_eq!(first.metadata.sequence, 7);
        assert_eq!(second.metadata.sequence, 8);
    }
}
