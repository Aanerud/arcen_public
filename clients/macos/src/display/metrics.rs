//! One validated per-display metrics value object.
//!
//! Every Deck consumer of a local display's geometry -- the legacy
//! `ClientMonitor` roster ([`crate::display::enumerate`]), the pinned
//! fullscreen stream size
//! ([`crate::display::presentation_size_for_display`]) and the
//! multi-monitor-v1 requested topology
//! ([`crate::display::topology::build_requested_topology`]) -- derives its
//! numbers from a single [`DisplayMetrics`] value. Before this existed the
//! roster/stream path computed an *integer* backing scale while the topology
//! path computed an `f32` ratio from the same display mode, so a scaled or
//! non-integer Retina mode could advertise a `scale` the stream width and
//! height did not agree with.
//!
//! # Pinned coordinate convention
//!
//! *All* arrangement geometry in this module, on the wire
//! (`ClientMonitor.x/y`, `RequestedMonitor`'s logical arrangement rect) and in
//! every consumer is **CoreGraphics global display space**: logical points,
//! origin at the **top-left** of the primary display, x increasing right and
//! **y increasing down**. `CGDisplayBounds` already reports exactly that
//! space, so it needs no conversion.
//!
//! AppKit (`NSScreen.frame`) uses the opposite vertical convention:
//! bottom-left origin, y increasing *up*. [`ScreenSpaceFlip`] is the one
//! sanctioned conversion between the two, applied exactly once at the AppKit
//! boundary (`crate::display`'s single `NSScreen` pass), never re-applied
//! downstream. Safe-area insets are edge-named rather than axis-signed
//! (`top` is the visual top edge in both spaces), so they are carried across
//! unchanged by construction.

use std::fmt;

use arcen_media::Rotation;

/// Largest logical-to-backing scale a real display can plausibly report.
///
/// macOS ships 1x and 2x panels and a handful of non-integer scaled modes; a
/// ratio beyond this is a mode-reading bug, not a display, and must never
/// reach a host as a stream size.
pub const MAX_BACKING_SCALE: f32 = 8.0;

/// How far the horizontal and vertical backing ratios of one display may
/// differ before the reading is rejected as non-uniform.
///
/// Real scaled modes disagree only by the rounding of one point row or
/// column (well under a thousandth); anything larger means the two axes were
/// read from different orientations or different displays.
const SCALE_AXIS_TOLERANCE: f32 = 1.0 / 64.0;

/// Failure building a validated [`DisplayMetrics`], [`LogicalRect`] or
/// [`BackingScale`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMetricsError {
    /// The logical arrangement rectangle was zero-sized or not finite.
    DegenerateArrangement,
    /// The arrangement rectangle overflows the signed logical desktop domain.
    ArrangementOverflow,
    /// The backing pixel extent was zero-sized.
    DegenerateBacking,
    /// The backing scale was `NaN` or infinite.
    NonFiniteScale,
    /// The backing scale was zero or negative.
    NonPositiveScale,
    /// The backing scale exceeded [`MAX_BACKING_SCALE`].
    ImplausibleScale,
    /// The horizontal and vertical backing ratios disagree by more than
    /// [`SCALE_AXIS_TOLERANCE`], so no single scale describes the display.
    NonUniformScale,
}

impl fmt::Display for DisplayMetricsError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DegenerateArrangement => {
                formatter.write_str("the display's logical arrangement rectangle is degenerate")
            }
            Self::ArrangementOverflow => formatter
                .write_str("the display's logical arrangement rectangle overflows the desktop"),
            Self::DegenerateBacking => {
                formatter.write_str("the display's backing pixel extent is degenerate")
            }
            Self::NonFiniteScale => {
                formatter.write_str("the display's backing scale is not finite")
            }
            Self::NonPositiveScale => {
                formatter.write_str("the display's backing scale is not positive")
            }
            Self::ImplausibleScale => write!(
                formatter,
                "the display's backing scale exceeds the {MAX_BACKING_SCALE}x limit"
            ),
            Self::NonUniformScale => {
                formatter.write_str("the display's horizontal and vertical backing scales disagree")
            }
        }
    }
}

impl std::error::Error for DisplayMetricsError {}

/// macOS fullscreen safe-area insets, in logical points.
///
/// On a notched MacBook the system lays a fullscreen window out *below* the
/// notch, so the presentable height is smaller than the display's logical
/// height. Every other display reports zero.
///
/// The fields are named after the *visual* edges they reserve, which is the
/// same in AppKit's bottom-left space and the pinned CG top-left space, so an
/// inset never needs the [`ScreenSpaceFlip`] conversion.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SafeAreaInsets {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

impl SafeAreaInsets {
    /// No reserved area: every display without a notch.
    pub const ZERO: Self = Self {
        top: 0,
        bottom: 0,
        left: 0,
        right: 0,
    };

    #[must_use]
    pub fn is_zero(self) -> bool {
        self == Self::ZERO
    }
}

/// One display's exact logical-to-backing scale: finite, strictly positive
/// and no larger than [`MAX_BACKING_SCALE`].
///
/// This is the *only* backing-scale derivation in the client. It is a true
/// ratio, never an integer floor, so a 1.5x scaled Retina mode advertises
/// 1.5 and streams 1.5x backing pixels rather than advertising one number
/// and streaming another.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
pub struct BackingScale(f32);

impl BackingScale {
    /// A 1:1 (non-Retina) display.
    pub const ONE: Self = Self(1.0);

    /// Validates an already-computed scale.
    ///
    /// # Errors
    ///
    /// [`DisplayMetricsError::NonFiniteScale`],
    /// [`DisplayMetricsError::NonPositiveScale`] or
    /// [`DisplayMetricsError::ImplausibleScale`].
    pub fn new(value: f32) -> Result<Self, DisplayMetricsError> {
        if !value.is_finite() {
            return Err(DisplayMetricsError::NonFiniteScale);
        }
        if value <= 0.0 {
            return Err(DisplayMetricsError::NonPositiveScale);
        }
        if value > MAX_BACKING_SCALE {
            return Err(DisplayMetricsError::ImplausibleScale);
        }
        Ok(Self(value))
    }

    /// The exact ratio of one axis's `backing` pixels to its `logical`
    /// points.
    ///
    /// # Errors
    ///
    /// [`DisplayMetricsError::DegenerateArrangement`] for a zero logical
    /// extent, [`DisplayMetricsError::DegenerateBacking`] for a zero backing
    /// extent, and whatever [`BackingScale::new`] rejects.
    pub fn from_axis(logical: u32, backing: u32) -> Result<Self, DisplayMetricsError> {
        if logical == 0 {
            return Err(DisplayMetricsError::DegenerateArrangement);
        }
        if backing == 0 {
            return Err(DisplayMetricsError::DegenerateBacking);
        }
        Self::new(backing as f32 / logical as f32)
    }

    /// The exact ratio as advertised on the wire.
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }

    /// Whether this display has more backing pixels than logical points.
    #[must_use]
    pub fn is_hidpi(self) -> bool {
        self.0 > 1.0
    }

    /// The scale in milli-units, for exact equality and log comparisons that
    /// must not depend on float bit patterns.
    #[must_use]
    pub fn millis(self) -> i64 {
        (f64::from(self.0) * 1000.0).round() as i64
    }
}

/// A logical (point-space) rectangle in the pinned CG arrangement space:
/// top-left origin, y down, primary display at (0, 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogicalRect {
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

impl LogicalRect {
    /// Validates a non-empty logical rectangle that stays inside the signed
    /// desktop domain.
    ///
    /// # Errors
    ///
    /// [`DisplayMetricsError::DegenerateArrangement`] for a zero extent and
    /// [`DisplayMetricsError::ArrangementOverflow`] when an extent or the far
    /// edge leaves the signed desktop domain.
    pub fn new(x: i32, y: i32, width: u32, height: u32) -> Result<Self, DisplayMetricsError> {
        if width == 0 || height == 0 {
            return Err(DisplayMetricsError::DegenerateArrangement);
        }
        // Both extents must stay inside `i32` on their own, not just once
        // added to the origin: a display far enough to the left could
        // otherwise carry a width no `i32` edge can express, and
        // `right()`/`bottom()` would wrap.
        if i32::try_from(width).is_err() || i32::try_from(height).is_err() {
            return Err(DisplayMetricsError::ArrangementOverflow);
        }
        let far_x = i64::from(x) + i64::from(width);
        let far_y = i64::from(y) + i64::from(height);
        if far_x > i64::from(i32::MAX) || far_y > i64::from(i32::MAX) {
            return Err(DisplayMetricsError::ArrangementOverflow);
        }
        Ok(Self {
            x,
            y,
            width,
            height,
        })
    }

    /// Rounds a `CGDisplayBounds`-shaped rectangle into the pinned integer
    /// arrangement space. This is the single place CoreGraphics floats become
    /// wire coordinates, so rounding happens exactly once.
    ///
    /// Negative origins are ordinary: a display left of or above the primary
    /// has a negative CG `x`/`y`.
    ///
    /// # Errors
    ///
    /// [`DisplayMetricsError::DegenerateArrangement`] when any component is
    /// not finite or the size rounds to zero, and
    /// [`DisplayMetricsError::ArrangementOverflow`] when a coordinate leaves
    /// the signed desktop domain.
    pub fn from_cg_bounds(
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    ) -> Result<Self, DisplayMetricsError> {
        if !(x.is_finite() && y.is_finite() && width.is_finite() && height.is_finite()) {
            return Err(DisplayMetricsError::DegenerateArrangement);
        }
        if !(width > 0.0 && height > 0.0) {
            return Err(DisplayMetricsError::DegenerateArrangement);
        }
        let origin_x = round_to_i32(x).ok_or(DisplayMetricsError::ArrangementOverflow)?;
        let origin_y = round_to_i32(y).ok_or(DisplayMetricsError::ArrangementOverflow)?;
        let width = round_to_u32(width).ok_or(DisplayMetricsError::ArrangementOverflow)?;
        let height = round_to_u32(height).ok_or(DisplayMetricsError::ArrangementOverflow)?;
        Self::new(origin_x, origin_y, width, height)
    }

    #[must_use]
    pub const fn x(self) -> i32 {
        self.x
    }

    #[must_use]
    pub const fn y(self) -> i32 {
        self.y
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    #[must_use]
    pub const fn size(self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// The exclusive right edge in arrangement points. Exact: [`LogicalRect::new`]
    /// already proved both the extent and the far edge fit `i32`.
    #[must_use]
    pub const fn right(self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    /// The exclusive bottom edge in arrangement points; larger `y` is
    /// further **down** the desktop.
    #[must_use]
    pub const fn bottom(self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }
}

fn round_to_i32(value: f64) -> Option<i32> {
    let rounded = value.round();
    (rounded >= f64::from(i32::MIN) && rounded <= f64::from(i32::MAX)).then_some(rounded as i32)
}

fn round_to_u32(value: f64) -> Option<u32> {
    let rounded = value.round();
    (rounded >= 0.0 && rounded <= f64::from(u32::MAX)).then_some(rounded as u32)
}

/// The one sanctioned conversion between AppKit's bottom-left, y-up screen
/// space and the pinned CG top-left, y-down arrangement space.
///
/// AppKit measures every screen and window frame from the bottom-left corner
/// of the *primary* screen (`NSScreen.screens[0]`, the one owning the menu
/// bar), with y increasing upwards. CoreGraphics measures from that screen's
/// top-left corner with y increasing downwards. The two therefore differ by
/// `primary_height - (y + height)` on the vertical axis only; x and both
/// extents are identical.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScreenSpaceFlip {
    primary_height_pts: f64,
}

impl ScreenSpaceFlip {
    /// The flip for a primary screen `primary_height_pts` points tall.
    ///
    /// Returns `None` for a non-finite or non-positive height, so a bad
    /// AppKit reading can never silently shift every display's origin.
    #[must_use]
    pub fn new(primary_height_pts: f64) -> Option<Self> {
        (primary_height_pts.is_finite() && primary_height_pts > 0.0)
            .then_some(Self { primary_height_pts })
    }

    /// The CG (y-down) top edge of a rectangle `height` points tall whose
    /// AppKit (y-up) bottom edge sits at `appkit_bottom_y`.
    #[must_use]
    pub fn cg_top_y(self, appkit_bottom_y: f64, height: f64) -> f64 {
        self.primary_height_pts - (appkit_bottom_y + height)
    }

    /// The exact inverse of [`ScreenSpaceFlip::cg_top_y`].
    #[must_use]
    pub fn appkit_bottom_y(self, cg_top_y: f64, height: f64) -> f64 {
        self.primary_height_pts - (cg_top_y + height)
    }

    /// The pinned CG arrangement rectangle for an `NSScreen.frame`.
    ///
    /// # Errors
    ///
    /// Whatever [`LogicalRect::from_cg_bounds`] rejects.
    pub fn cg_rect_from_appkit_frame(
        self,
        x: f64,
        appkit_bottom_y: f64,
        width: f64,
        height: f64,
    ) -> Result<LogicalRect, DisplayMetricsError> {
        LogicalRect::from_cg_bounds(x, self.cg_top_y(appkit_bottom_y, height), width, height)
    }
}

/// An encoder-aligned stream extent in pixels: width divisible by four for
/// YUV444 capture, height even.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StreamExtent {
    width: u32,
    height: u32,
}

impl StreamExtent {
    /// Aligns a raw pixel extent for the encoder, never yielding a degenerate
    /// size.
    #[must_use]
    pub const fn aligned(width: u32, height: u32) -> Self {
        Self {
            width: if width < 4 { 4 } else { width & !3 },
            height: if height < 2 { 2 } else { height & !1 },
        }
    }

    #[must_use]
    pub const fn width(self) -> u32 {
        self.width
    }

    #[must_use]
    pub const fn height(self) -> u32 {
        self.height
    }

    /// The `[width, height]` form every stream-size caller passes around.
    #[must_use]
    pub const fn as_array(self) -> [u32; 2] {
        [self.width, self.height]
    }
}

/// The exact backing-pixel extent of `points` logical points on an axis
/// whose display mode reports `logical` points backed by `backing` pixels.
///
/// Integer rounding rather than float multiplication so a 2x panel's
/// presentable height converts exactly (`1130 pt * 2338 px / 1169 pt =
/// 2260 px`), and so a non-integer scaled mode lands on the same pixel row
/// the window server does.
#[must_use]
pub fn points_to_backing(points: u32, logical: u32, backing: u32) -> u32 {
    if logical == 0 {
        return points;
    }
    let scaled =
        (u64::from(points) * u64::from(backing) + u64::from(logical) / 2) / u64::from(logical);
    u32::try_from(scaled).unwrap_or(u32::MAX)
}

/// Maps `CGDisplayRotation`'s degrees to the nearest supported clockwise
/// rotation. Unknown/unavailable (`-1.0`) and any non-canonical angle
/// conservatively report unrotated rather than guessing.
#[must_use]
pub fn rotation_from_degrees(degrees: f64) -> Rotation {
    const EPSILON: f64 = 0.5;
    if !degrees.is_finite() {
        return Rotation::Degrees0;
    }
    if (degrees - 90.0).abs() < EPSILON {
        Rotation::Degrees90
    } else if (degrees - 180.0).abs() < EPSILON {
        Rotation::Degrees180
    } else if (degrees - 270.0).abs() < EPSILON {
        Rotation::Degrees270
    } else {
        Rotation::Degrees0
    }
}

/// The backing pixel extent covering `arrangement`, given the display mode's
/// own point and pixel dimensions.
///
/// `CGDisplayBounds` is always desktop-space (already rotated), while a
/// display mode describes the panel's framebuffer. For a 90/270-degree
/// rotated display the two are transposed, so the mode's pixels are
/// transposed with them; when neither orientation matches (mirroring, or a
/// mode/bounds disagreement) the arrangement stays authoritative and only
/// the mode's *ratio* is carried onto it. A mode that reports fewer pixels
/// than points, or no usable mode at all, is reported 1x -- exactly what the
/// two previous scale helpers did.
#[must_use]
pub fn backing_for_arrangement(
    arrangement: LogicalRect,
    mode_points: (u32, u32),
    mode_pixels: (u32, u32),
) -> (u32, u32) {
    let (point_width, point_height) = mode_points;
    let (pixel_width, pixel_height) = mode_pixels;
    let (arrangement_width, arrangement_height) = arrangement.size();
    if point_width == 0
        || point_height == 0
        || pixel_width < point_width
        || pixel_height < point_height
    {
        return (arrangement_width, arrangement_height);
    }
    if (point_width, point_height) == (arrangement_width, arrangement_height) {
        return (pixel_width, pixel_height);
    }
    if (point_height, point_width) == (arrangement_width, arrangement_height) {
        return (pixel_height, pixel_width);
    }
    (
        points_to_backing(arrangement_width, point_width, pixel_width),
        points_to_backing(arrangement_height, point_height, pixel_height),
    )
}

/// One display's complete, validated metrics: everything Deck needs to
/// advertise, present and stream it, derived once and shared by every
/// consumer.
///
/// Invariants held by construction:
/// - the logical arrangement rectangle is non-empty and inside the signed
///   desktop domain, in the pinned CG top-left/y-down space;
/// - the backing pixel extent is non-empty and covers exactly that
///   rectangle;
/// - [`DisplayMetrics::scale`] is finite, strictly positive, at most
///   [`MAX_BACKING_SCALE`], and describes *both* axes;
/// - every derived extent ([`DisplayMetrics::presentable_size`],
///   [`DisplayMetrics::native_stream_extent`],
///   [`DisplayMetrics::presentation_stream_extent`]) comes from those same
///   numbers, so an advertised scale can never disagree with an advertised
///   stream size.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayMetrics {
    display_id: u32,
    arrangement: LogicalRect,
    backing_width: u32,
    backing_height: u32,
    scale: BackingScale,
    rotation: Rotation,
    insets: SafeAreaInsets,
}

impl DisplayMetrics {
    /// Validates one display's metrics.
    ///
    /// `arrangement` is the CG top-left/y-down logical rectangle
    /// (`CGDisplayBounds`); `backing_width`/`backing_height` are the pixels
    /// covering it in the same orientation (see
    /// [`backing_for_arrangement`]).
    ///
    /// # Errors
    ///
    /// [`DisplayMetricsError::DegenerateBacking`] for a zero backing extent,
    /// [`DisplayMetricsError::NonUniformScale`] when the two axes disagree by
    /// more than a rounding step, and whatever [`BackingScale::from_axis`]
    /// rejects.
    pub fn new(
        display_id: u32,
        arrangement: LogicalRect,
        backing_width: u32,
        backing_height: u32,
        rotation: Rotation,
        insets: SafeAreaInsets,
    ) -> Result<Self, DisplayMetricsError> {
        let horizontal = BackingScale::from_axis(arrangement.width(), backing_width)?;
        let vertical = BackingScale::from_axis(arrangement.height(), backing_height)?;
        if (horizontal.get() - vertical.get()).abs() > SCALE_AXIS_TOLERANCE {
            return Err(DisplayMetricsError::NonUniformScale);
        }
        Ok(Self {
            display_id,
            arrangement,
            backing_width,
            backing_height,
            // The horizontal ratio is the advertised scale, matching what
            // every previous derivation in this client used.
            scale: horizontal,
            rotation,
            insets,
        })
    }

    /// A 1:1 display: one backing pixel per logical point.
    ///
    /// # Errors
    ///
    /// Whatever [`DisplayMetrics::new`] rejects (statically impossible for a
    /// valid `arrangement`).
    pub fn unscaled(
        display_id: u32,
        arrangement: LogicalRect,
        rotation: Rotation,
        insets: SafeAreaInsets,
    ) -> Result<Self, DisplayMetricsError> {
        Self::new(
            display_id,
            arrangement,
            arrangement.width(),
            arrangement.height(),
            rotation,
            insets,
        )
    }

    /// The same metrics with different safe-area insets, for the AppKit read
    /// that can only happen on the main thread.
    #[must_use]
    pub fn with_safe_area_insets(self, insets: SafeAreaInsets) -> Self {
        Self { insets, ..self }
    }

    /// The `CGDirectDisplayID` these metrics describe.
    #[must_use]
    pub const fn display_id(self) -> u32 {
        self.display_id
    }

    /// The display's logical arrangement rectangle, CG top-left/y-down.
    #[must_use]
    pub const fn arrangement(self) -> LogicalRect {
        self.arrangement
    }

    /// The backing pixels covering the whole arrangement rectangle.
    #[must_use]
    pub const fn backing_size(self) -> (u32, u32) {
        (self.backing_width, self.backing_height)
    }

    /// The display's exact logical-to-backing scale.
    #[must_use]
    pub const fn scale(self) -> BackingScale {
        self.scale
    }

    /// The display's clockwise desktop rotation.
    #[must_use]
    pub const fn rotation(self) -> Rotation {
        self.rotation
    }

    /// The fullscreen safe-area insets, in logical points.
    #[must_use]
    pub const fn safe_area_insets(self) -> SafeAreaInsets {
        self.insets
    }

    /// The insets that actually apply: none when the caller intends to cover
    /// the notch area itself (Settings -> Displays).
    #[must_use]
    pub const fn effective_insets(self, use_notch_area: bool) -> SafeAreaInsets {
        if use_notch_area {
            SafeAreaInsets::ZERO
        } else {
            self.insets
        }
    }

    /// The area a fullscreen window can actually present, in logical points.
    ///
    /// On a notched MacBook this is shorter than the arrangement rectangle:
    /// macOS hands a fullscreen window only the area below the notch, so a
    /// stream sized to the whole panel is aspect-fit into a shorter viewport.
    #[must_use]
    pub fn presentable_size(self, use_notch_area: bool) -> (u32, u32) {
        let insets = self.effective_insets(use_notch_area);
        (
            self.arrangement
                .width()
                .saturating_sub(insets.left)
                .saturating_sub(insets.right),
            self.arrangement
                .height()
                .saturating_sub(insets.top)
                .saturating_sub(insets.bottom),
        )
    }

    /// The presentable area expressed in this display's own backing pixels,
    /// using the exact per-axis ratio rather than an integer scale factor.
    #[must_use]
    pub fn presentable_backing_size(self, use_notch_area: bool) -> (u32, u32) {
        let (width, height) = self.presentable_size(use_notch_area);
        (
            points_to_backing(width, self.arrangement.width(), self.backing_width),
            points_to_backing(height, self.arrangement.height(), self.backing_height),
        )
    }

    /// The whole display's stream extent for the legacy `ClientMonitor`
    /// roster: the logical arrangement size, encoder-aligned.
    ///
    /// A scaled or Retina display advertises its logical framebuffer (the
    /// host synthesises that desktop rather than wasting bandwidth on backing
    /// pixels); a 1x display's logical size *is* its native pixel size, so
    /// the two cases coincide.
    #[must_use]
    pub fn native_stream_extent(self) -> StreamExtent {
        let (width, height) = self.arrangement.size();
        StreamExtent::aligned(width, height)
    }

    /// The stream extent a pinned session should request: the presentable
    /// area, in backing pixels when `hidpi` and in points otherwise,
    /// encoder-aligned.
    #[must_use]
    pub fn presentation_stream_extent(self, use_notch_area: bool, hidpi: bool) -> StreamExtent {
        let (width, height) = if hidpi {
            self.presentable_backing_size(use_notch_area)
        } else {
            self.presentable_size(use_notch_area)
        };
        StreamExtent::aligned(width, height)
    }

    /// The logical-to-stream ratio a `hidpi` stream of this display carries:
    /// its own exact backing scale, or 1:1 for a point-size stream.
    ///
    /// This is what keeps an advertised `scale` and an advertised stream size
    /// describing the same display: a HiDPI stream is exactly `scale` times
    /// the presentable point size on both axes.
    #[must_use]
    pub const fn stream_scale(self, hidpi: bool) -> BackingScale {
        if hidpi {
            self.scale
        } else {
            BackingScale::ONE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 14" MacBook Pro at "More Space": a 1800x1169 pt mode on a 3600x2338 px
    /// panel with a 38 pt notch inset, measured on 2026-08-03.
    fn retina_builtin() -> DisplayMetrics {
        DisplayMetrics::new(
            1,
            LogicalRect::new(0, 0, 1800, 1169).expect("valid arrangement"),
            3600,
            2338,
            Rotation::Degrees0,
            SafeAreaInsets {
                top: 38,
                ..SafeAreaInsets::ZERO
            },
        )
        .expect("valid retina metrics")
    }

    /// An ordinary 1x 2560x1440 external panel placed to the right of the
    /// built-in display.
    fn external_1x() -> DisplayMetrics {
        DisplayMetrics::unscaled(
            2,
            LogicalRect::new(1800, 0, 2560, 1440).expect("valid arrangement"),
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
        )
        .expect("valid external metrics")
    }

    #[test]
    fn backing_scale_is_the_exact_ratio_not_an_integer_floor() {
        assert_eq!(retina_builtin().scale().get(), 2.0);
        assert_eq!(external_1x().scale().get(), 1.0);
        // The regression this type exists to prevent: a 1.5x scaled mode.
        let scaled = DisplayMetrics::new(
            3,
            LogicalRect::new(0, 0, 2000, 1333).expect("valid arrangement"),
            3000,
            2000,
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
        )
        .expect("valid scaled metrics");
        assert_eq!(scaled.scale().get(), 1.5);
        assert_eq!(scaled.scale().millis(), 1_500);
        assert!(scaled.scale().is_hidpi());
    }

    #[test]
    fn invalid_scales_are_refused_rather_than_clamped() {
        assert_eq!(
            BackingScale::new(f32::NAN),
            Err(DisplayMetricsError::NonFiniteScale)
        );
        assert_eq!(
            BackingScale::new(f32::INFINITY),
            Err(DisplayMetricsError::NonFiniteScale)
        );
        assert_eq!(
            BackingScale::new(0.0),
            Err(DisplayMetricsError::NonPositiveScale)
        );
        assert_eq!(
            BackingScale::new(-2.0),
            Err(DisplayMetricsError::NonPositiveScale)
        );
        assert_eq!(
            BackingScale::new(MAX_BACKING_SCALE + 0.5),
            Err(DisplayMetricsError::ImplausibleScale)
        );
        assert_eq!(
            BackingScale::new(MAX_BACKING_SCALE).map(BackingScale::get),
            Ok(MAX_BACKING_SCALE)
        );
        assert_eq!(
            BackingScale::from_axis(0, 100),
            Err(DisplayMetricsError::DegenerateArrangement)
        );
        assert_eq!(
            BackingScale::from_axis(100, 0),
            Err(DisplayMetricsError::DegenerateBacking)
        );
    }

    #[test]
    fn a_display_whose_axes_disagree_is_refused() {
        // A 1.5x wide, 0.66x tall "display" is a transposed reading, not a
        // panel: no single scale describes it.
        assert_eq!(
            DisplayMetrics::new(
                4,
                LogicalRect::new(0, 0, 2000, 1333).expect("valid arrangement"),
                3000,
                1333,
                Rotation::Degrees0,
                SafeAreaInsets::ZERO,
            ),
            Err(DisplayMetricsError::NonUniformScale)
        );
        // One row of rounding on a real scaled mode still agrees.
        assert!(DisplayMetrics::new(
            5,
            LogicalRect::new(0, 0, 1707, 1067).expect("valid arrangement"),
            2560,
            1600,
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
        )
        .is_ok());
        assert_eq!(
            DisplayMetrics::new(
                6,
                LogicalRect::new(0, 0, 1800, 1169).expect("valid arrangement"),
                0,
                2338,
                Rotation::Degrees0,
                SafeAreaInsets::ZERO,
            ),
            Err(DisplayMetricsError::DegenerateBacking)
        );
    }

    #[test]
    fn degenerate_or_overflowing_arrangements_are_refused() {
        assert_eq!(
            LogicalRect::new(0, 0, 0, 1080),
            Err(DisplayMetricsError::DegenerateArrangement)
        );
        assert_eq!(
            LogicalRect::new(0, 0, 1920, 0),
            Err(DisplayMetricsError::DegenerateArrangement)
        );
        assert_eq!(
            LogicalRect::new(i32::MAX - 10, 0, 1920, 1080),
            Err(DisplayMetricsError::ArrangementOverflow)
        );
        // An extent that only fits because the origin is far enough left is
        // still refused: `right()`/`bottom()` must stay exact.
        assert_eq!(
            LogicalRect::new(-2_000_000_000, 0, 4_000_000_000, 1080),
            Err(DisplayMetricsError::ArrangementOverflow)
        );
        assert_eq!(
            LogicalRect::new(0, -2_000_000_000, 1920, 4_000_000_000),
            Err(DisplayMetricsError::ArrangementOverflow)
        );
        let widest = LogicalRect::new(0, 0, i32::MAX as u32, 1080).expect("the widest valid rect");
        assert_eq!(widest.right(), i32::MAX);
        assert_eq!(
            LogicalRect::from_cg_bounds(0.0, 0.0, f64::NAN, 1080.0),
            Err(DisplayMetricsError::DegenerateArrangement)
        );
        assert_eq!(
            LogicalRect::from_cg_bounds(0.0, 0.0, 0.4, 1080.0),
            Err(DisplayMetricsError::DegenerateArrangement)
        );
    }

    /// A mixed-DPI desktop: a 2x Retina built-in beside a 1x external panel.
    /// Every number for each display comes from that display's own metrics --
    /// never a scale borrowed from the primary.
    #[test]
    fn mixed_dpi_topology_derives_each_display_from_its_own_metrics() {
        let builtin = retina_builtin();
        let external = external_1x();

        assert_eq!(builtin.scale().get(), 2.0);
        assert_eq!(external.scale().get(), 1.0);
        assert_eq!(builtin.backing_size(), (3600, 2338));
        assert_eq!(external.backing_size(), (2560, 1440));

        // Legacy roster stream: each display's own logical framebuffer.
        assert_eq!(builtin.native_stream_extent().as_array(), [1800, 1168]);
        assert_eq!(external.native_stream_extent().as_array(), [2560, 1440]);

        // Pinned point-size streams: the notched built-in loses its menu-bar
        // strip, the external one loses nothing.
        assert_eq!(
            builtin.presentation_stream_extent(false, false).as_array(),
            [1800, 1130]
        );
        assert_eq!(
            external.presentation_stream_extent(false, false).as_array(),
            [2560, 1440]
        );

        // Pinned HiDPI streams: the Retina panel doubles its own presentable
        // area (1131 pt below the 38 pt notch inset), the 1x external panel
        // is untouched by its neighbour's scale.
        assert_eq!(
            builtin.presentation_stream_extent(false, true).as_array(),
            [3600, 2262]
        );
        assert_eq!(
            external.presentation_stream_extent(false, true).as_array(),
            [2560, 1440]
        );

        // The arrangement is one continuous CG top-left desktop.
        assert_eq!(builtin.arrangement().right(), external.arrangement().x());
    }

    /// The exact bug this value object exists to fix: an integer
    /// pixel/logical ratio floors a 1.5x mode to 1x, so the topology
    /// advertised `scale = 1.5` while the stream carried point-size pixels.
    #[test]
    fn non_integer_scale_streams_the_backing_pixels_it_advertises() {
        let scaled = DisplayMetrics::new(
            7,
            LogicalRect::new(0, 0, 2000, 1333).expect("valid arrangement"),
            3000,
            2000,
            Rotation::Degrees0,
            SafeAreaInsets::ZERO,
        )
        .expect("valid scaled metrics");

        assert_eq!(scaled.scale().get(), 1.5);
        // The old integer scale (3000 / 2000 = 1) would have produced
        // [2000, 1332] here -- a 1.0x stream advertised as 1.5x.
        assert_eq!(
            scaled.presentation_stream_extent(false, true).as_array(),
            [3000, 2000]
        );
        assert_eq!(
            scaled.presentation_stream_extent(false, false).as_array(),
            [2000, 1332]
        );
        // The legacy roster keeps advertising the logical framebuffer.
        assert_eq!(scaled.native_stream_extent().as_array(), [2000, 1332]);
    }

    /// Every advertised stream must equal the presentable point size times
    /// the advertised scale, on both axes, for every mode Deck can meet.
    #[test]
    fn advertised_scale_and_stream_extent_always_agree() {
        let cases = [
            // (logical, backing, insets top)
            ((1800, 1169), (1800, 1169), 0),
            ((1800, 1169), (3600, 2338), 38),
            ((2000, 1333), (3000, 2000), 0),
            ((2560, 1440), (2560, 1440), 0),
            ((1512, 982), (3024, 1964), 37),
            ((1707, 1067), (2560, 1600), 0),
        ];
        for ((logical_width, logical_height), (backing_width, backing_height), top) in cases {
            let metrics = DisplayMetrics::new(
                8,
                LogicalRect::new(0, 0, logical_width, logical_height).expect("valid arrangement"),
                backing_width,
                backing_height,
                Rotation::Degrees0,
                SafeAreaInsets {
                    top,
                    ..SafeAreaInsets::ZERO
                },
            )
            .expect("valid metrics");

            for use_notch_area in [false, true] {
                let (presentable_width, presentable_height) =
                    metrics.presentable_size(use_notch_area);
                for hidpi in [false, true] {
                    let stream = metrics.presentation_stream_extent(use_notch_area, hidpi);
                    let stream_scale = metrics.stream_scale(hidpi).get();
                    let expected_width = f64::from(presentable_width) * f64::from(stream_scale);
                    let expected_height = f64::from(presentable_height) * f64::from(stream_scale);
                    // Only encoder alignment (< 4 px wide, < 2 px tall) may
                    // separate the advertised stream from scale x points.
                    assert!(
                        (f64::from(stream.width()) - expected_width).abs() < 4.0,
                        "{logical_width}x{logical_height}@{backing_width}x{backing_height} \
                         hidpi={hidpi} notch={use_notch_area}: stream width {} is not \
                         {stream_scale}x {presentable_width}",
                        stream.width(),
                    );
                    assert!(
                        (f64::from(stream.height()) - expected_height).abs() < 2.0,
                        "{logical_width}x{logical_height}@{backing_width}x{backing_height} \
                         hidpi={hidpi} notch={use_notch_area}: stream height {} is not \
                         {stream_scale}x {presentable_height}",
                        stream.height(),
                    );
                    assert_eq!(stream.width() % 4, 0, "width stays encoder-aligned");
                    assert_eq!(stream.height() % 2, 0, "height stays even");
                }
            }
        }
    }

    /// A 90-degree rotated secondary: `CGDisplayBounds` reports the rotated
    /// (portrait) rectangle while the display mode still describes the
    /// panel's landscape framebuffer, so the backing pixels must be
    /// transposed with it.
    #[test]
    fn rotated_secondary_transposes_the_modes_backing_pixels() {
        let portrait = LogicalRect::new(2560, -200, 1080, 1920).expect("valid arrangement");
        assert_eq!(
            backing_for_arrangement(portrait, (1920, 1080), (3840, 2160)),
            (2160, 3840)
        );
        let metrics = DisplayMetrics::new(
            9,
            portrait,
            2160,
            3840,
            rotation_from_degrees(90.0),
            SafeAreaInsets::ZERO,
        )
        .expect("valid rotated metrics");
        assert_eq!(metrics.rotation(), Rotation::Degrees90);
        assert_eq!(metrics.scale().get(), 2.0);
        assert_eq!(metrics.native_stream_extent().as_array(), [1080, 1920]);
        assert_eq!(
            metrics.presentation_stream_extent(false, true).as_array(),
            [2160, 3840]
        );

        // An unrotated 1x panel keeps the mode's own pixels, and a 180-degree
        // rotation leaves the rectangle's orientation alone.
        let landscape = LogicalRect::new(0, 0, 1920, 1080).expect("valid arrangement");
        assert_eq!(
            backing_for_arrangement(landscape, (1920, 1080), (1920, 1080)),
            (1920, 1080)
        );
        assert_eq!(
            backing_for_arrangement(landscape, (1920, 1080), (3840, 2160)),
            (3840, 2160)
        );
        assert_eq!(rotation_from_degrees(180.0), Rotation::Degrees180);
        assert_eq!(rotation_from_degrees(270.0), Rotation::Degrees270);
        assert_eq!(rotation_from_degrees(0.0), Rotation::Degrees0);
        // CGDisplayRotation reports -1.0 when it cannot answer.
        assert_eq!(rotation_from_degrees(-1.0), Rotation::Degrees0);
        assert_eq!(rotation_from_degrees(f64::NAN), Rotation::Degrees0);
        assert_eq!(rotation_from_degrees(37.0), Rotation::Degrees0);
    }

    #[test]
    fn unreadable_or_downscaled_modes_report_one_to_one() {
        let arrangement = LogicalRect::new(0, 0, 1920, 1080).expect("valid arrangement");
        // No usable mode at all.
        assert_eq!(
            backing_for_arrangement(arrangement, (0, 0), (0, 0)),
            (1920, 1080)
        );
        // A framebuffer smaller than the point size is not a 0.5x display.
        assert_eq!(
            backing_for_arrangement(arrangement, (1920, 1080), (960, 540)),
            (1920, 1080)
        );
        // A mirrored display whose bounds disagree with its own mode keeps
        // the arrangement authoritative and carries the mode's ratio.
        assert_eq!(
            backing_for_arrangement(arrangement, (1280, 720), (2560, 1440)),
            (3840, 2160)
        );
    }

    /// Displays left of or above the primary have negative CG origins; they
    /// must survive rounding, validation and every derived extent.
    #[test]
    fn negative_arrangement_origins_round_trip() {
        let rect = LogicalRect::from_cg_bounds(-1800.0, -832.0, 1800.0, 1169.0)
            .expect("negative origins are ordinary");
        assert_eq!((rect.x(), rect.y()), (-1800, -832));
        assert_eq!(rect.size(), (1800, 1169));
        assert_eq!(rect.right(), 0);
        assert_eq!(rect.bottom(), 337);

        let metrics = DisplayMetrics::new(
            10,
            rect,
            3600,
            2338,
            Rotation::Degrees0,
            SafeAreaInsets {
                top: 39,
                ..SafeAreaInsets::ZERO
            },
        )
        .expect("valid metrics");
        assert_eq!(metrics.arrangement().x(), -1800);
        assert_eq!(metrics.arrangement().y(), -832);
        assert_eq!(
            metrics.presentation_stream_extent(false, true).as_array(),
            [3600, 2260]
        );

        // CG bounds are floats; a half-point origin rounds once, here.
        let rounded = LogicalRect::from_cg_bounds(-1800.5, -832.4, 1800.0, 1169.0)
            .expect("valid arrangement");
        assert_eq!((rounded.x(), rounded.y()), (-1801, -832));
    }

    /// The CG (top-left, y-down) and AppKit (bottom-left, y-up) spaces differ
    /// only on the vertical axis, and the conversion must be exactly
    /// invertible so no consumer can apply it twice.
    #[test]
    fn cg_and_appkit_vertical_conversion_round_trips() {
        let flip = ScreenSpaceFlip::new(1169.0).expect("a real primary screen height");
        // The primary display itself: AppKit (0, 0) is CG (0, 0).
        assert_eq!(flip.cg_top_y(0.0, 1169.0), 0.0);
        assert_eq!(flip.appkit_bottom_y(0.0, 1169.0), 0.0);
        // A 1080p display stacked above the primary sits at CG y = -1080 and
        // AppKit y = +1169.
        assert_eq!(flip.cg_top_y(1169.0, 1080.0), -1080.0);
        assert_eq!(flip.appkit_bottom_y(-1080.0, 1080.0), 1169.0);

        for (appkit_bottom_y, height) in [
            (0.0, 1169.0),
            (1169.0, 1080.0),
            (-1440.0, 1440.0),
            (-732.5, 1920.0),
        ] {
            let cg_top_y = flip.cg_top_y(appkit_bottom_y, height);
            assert_eq!(
                flip.appkit_bottom_y(cg_top_y, height),
                appkit_bottom_y,
                "AppKit -> CG -> AppKit must be exactly the identity",
            );
            assert_eq!(
                flip.cg_top_y(flip.appkit_bottom_y(cg_top_y, height), height),
                cg_top_y,
                "CG -> AppKit -> CG must be exactly the identity",
            );
        }

        // A whole NSScreen.frame converts in one step, including the negative
        // CG origin of a display placed above and left of the primary.
        let rect = flip
            .cg_rect_from_appkit_frame(-1920.0, 1169.0, 1920.0, 1080.0)
            .expect("valid frame");
        assert_eq!((rect.x(), rect.y()), (-1920, -1080));
        assert_eq!(rect.size(), (1920, 1080));

        assert_eq!(ScreenSpaceFlip::new(0.0), None);
        assert_eq!(ScreenSpaceFlip::new(-1169.0), None);
        assert_eq!(ScreenSpaceFlip::new(f64::NAN), None);
    }

    #[test]
    fn safe_area_rules_are_preserved_exactly() {
        let builtin = retina_builtin();
        // Measured on the 14" MacBook Pro: the 38 pt safe-area inset leaves
        // 1131 pt, and encoder alignment lands the stream on the 1800x1130
        // fullscreen viewport AppKit actually hands out.
        assert_eq!(builtin.presentable_size(false), (1800, 1131));
        assert_eq!(
            builtin.presentation_stream_extent(false, false).as_array(),
            [1800, 1130]
        );
        // Covering the notch area asks for the whole panel back.
        assert_eq!(builtin.presentable_size(true), (1800, 1169));
        assert_eq!(
            builtin.presentation_stream_extent(true, false).as_array(),
            [1800, 1168]
        );
        assert_eq!(
            builtin.presentation_stream_extent(true, true).as_array(),
            [3600, 2338]
        );
        // The 39 pt inset AppKit reported for the same panel's fullscreen
        // window origin lands on the same viewport, at both scales.
        let thirty_nine = builtin.with_safe_area_insets(SafeAreaInsets {
            top: 39,
            ..SafeAreaInsets::ZERO
        });
        assert_eq!(
            thirty_nine
                .presentation_stream_extent(false, false)
                .as_array(),
            [1800, 1130]
        );
        assert_eq!(
            thirty_nine
                .presentation_stream_extent(false, true)
                .as_array(),
            [3600, 2260]
        );
        assert!(!builtin.safe_area_insets().is_zero());
        assert!(external_1x().safe_area_insets().is_zero());
    }

    #[test]
    fn absurd_insets_clamp_instead_of_underflowing() {
        let metrics = retina_builtin().with_safe_area_insets(SafeAreaInsets {
            top: 5_000,
            bottom: 5_000,
            left: 5_000,
            right: 5_000,
        });
        assert_eq!(metrics.presentable_size(false), (0, 0));
        assert_eq!(
            metrics.presentation_stream_extent(false, false).as_array(),
            [4, 2]
        );
        assert_eq!(
            metrics.presentation_stream_extent(false, true).as_array(),
            [4, 2]
        );
    }

    #[test]
    fn point_to_backing_conversion_is_exact() {
        assert_eq!(points_to_backing(1130, 1169, 2338), 2260);
        assert_eq!(points_to_backing(1800, 1800, 3600), 3600);
        assert_eq!(points_to_backing(1333, 1333, 2000), 2000);
        assert_eq!(points_to_backing(1067, 1067, 1600), 1600);
        // Half a pixel rounds to the nearest whole one.
        assert_eq!(points_to_backing(3, 2, 3), 5);
        // A zero logical extent cannot scale anything.
        assert_eq!(points_to_backing(1920, 0, 3840), 1920);
    }

    #[test]
    fn stream_extents_stay_encoder_aligned() {
        assert_eq!(StreamExtent::aligned(1366, 769).as_array(), [1364, 768]);
        assert_eq!(StreamExtent::aligned(0, 0).as_array(), [4, 2]);
        assert_eq!(StreamExtent::aligned(3, 1).as_array(), [4, 2]);
        assert_eq!(StreamExtent::aligned(3600, 2338).as_array(), [3600, 2338]);
    }
}
