//! Shared, OS-free multi-monitor topology placement primitives.
//!
//! Every product that maps a client-requested [`RequestedMonitorTopology`]
//! onto host outputs needs the same handful of pure decisions: how a stream
//! extent relates to its output transform, where each monitor's rectangle
//! lands on the host desktop, whether the resulting layout keeps signed
//! coordinates or is translated to a non-negative origin, and how the
//! committed plan becomes a shared [`RegionSet`]/[`AppliedRegionSet`] pair.
//! Those decisions used to be re-derived (and could silently drift) inside
//! each host and client. They live here instead.
//!
//! # Explicit conventions, never inferred
//!
//! [`TransformConvention`] and [`OriginPolicy`] are always supplied by the
//! caller. Nothing in this module inspects the target OS, a `cfg!` flag, or a
//! product name to guess them: a host that streams a native pre-rotation
//! extent and carries the transform separately
//! ([`TransformConvention::NativeNeedsTransform`]) and a client that receives
//! an already compositor-oriented extent
//! ([`TransformConvention::AlreadyCompositorOriented`]) are both legitimate
//! and must both stay expressible on any platform.
//!
//! # Units
//!
//! ADR 0009 keeps requested arrangement geometry in the client's *logical*
//! desktop space and per-monitor stream sizes in *physical* pixels, and
//! forbids deriving an aggregate pixel extent by combining logical origins
//! with physical stream sizes. [`LayoutSpace`]/[`SpacedLayoutRect`] make that
//! unit pairing explicit and [`checked_layout_bounds`] rejects the forbidden
//! mixture instead of silently returning a plausible-looking rectangle.

use std::collections::VecDeque;
use std::error::Error;
use std::fmt::{Display, Formatter};

use crate::multi_monitor::{
    AppliedMonitor, LayoutBounds, LayoutRect, LayoutTranslation, RequestedMonitor,
};
use crate::region::{
    AppliedRect, AppliedRegionDescriptor, AppliedRegionSet, LogicalPoint, LogicalRect, LogicalSize,
    OutputIdentity, OutputTransform, PhysicalSize, RegionContractError, RegionDescriptor,
    RegionGeneration, RegionId, RegionSet, Scale120,
};
use crate::{MediaContractError, Rotation};

/// How a caller's per-region stream extent relates to its output transform.
///
/// This is a property of the *pipeline*, not of the operating system, and is
/// always passed in explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum TransformConvention {
    /// The stream extent is the native, pre-transform mode the output is
    /// driven at, and the rotation is carried alongside it for the
    /// compositor/RandR/CCD to apply. A 90/270-degree monitor therefore
    /// occupies a desktop footprint with its extents swapped relative to the
    /// stream. Both hosts use this.
    NativeNeedsTransform,
    /// The stream extent has already absorbed the transform, so it is the
    /// on-screen footprint as-is and the region records
    /// [`OutputTransform::Normal`] regardless of any informational panel
    /// rotation metadata. Deck's compositor-oriented multi-monitor-v1 stream
    /// uses this; rotating it a second time would double-apply the transform.
    AlreadyCompositorOriented,
}

impl TransformConvention {
    /// Returns the on-desktop footprint of a stream extent under this
    /// convention.
    #[must_use]
    pub const fn desktop_footprint(
        self,
        width_px: u32,
        height_px: u32,
        rotation: Rotation,
    ) -> (u32, u32) {
        match self {
            Self::NativeNeedsTransform => match rotation {
                Rotation::Degrees0 | Rotation::Degrees180 => (width_px, height_px),
                Rotation::Degrees90 | Rotation::Degrees270 => (height_px, width_px),
            },
            Self::AlreadyCompositorOriented => (width_px, height_px),
        }
    }

    /// Returns the transform a region descriptor records under this
    /// convention.
    #[must_use]
    pub const fn region_transform(self, rotation: Rotation) -> OutputTransform {
        match self {
            Self::NativeNeedsTransform => match rotation {
                Rotation::Degrees0 => OutputTransform::Normal,
                Rotation::Degrees90 => OutputTransform::Rotate90,
                Rotation::Degrees180 => OutputTransform::Rotate180,
                Rotation::Degrees270 => OutputTransform::Rotate270,
            },
            Self::AlreadyCompositorOriented => OutputTransform::Normal,
        }
    }

    /// Returns this monitor's on-desktop footprint under this convention.
    #[must_use]
    pub fn monitor_desktop_footprint(self, monitor: &RequestedMonitor) -> (u32, u32) {
        let monitor = monitor.monitor();
        self.desktop_footprint(monitor.width_px, monitor.height_px, monitor.rotation)
    }
}

/// Whether a placed layout keeps its natural signed coordinates or is
/// translated so its bounding rectangle starts at a non-negative origin.
///
/// Like [`TransformConvention`], this is always supplied by the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum OriginPolicy {
    /// Keep every computed coordinate exactly as placed, including negative
    /// ones. The Windows virtual desktop is natively signed and the OS anchors
    /// the primary display's own origin at `(0, 0)`, so a monitor above or
    /// left of it legitimately carries negative coordinates and translating
    /// the layout would drag the primary off `(0, 0)`.
    PreserveSigned,
    /// Translate the whole layout by the same offset so its bounding
    /// rectangle starts at a non-negative origin, preserving every relative
    /// offset. An Xorg/RandR screen's virtual framebuffer must start at
    /// `(0, 0)`.
    TranslateToNonNegative,
}

/// A placed layout: the final per-monitor footprints, their aggregate bounds,
/// and the translation the [`OriginPolicy`] applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacedLayout {
    rects: Vec<LayoutRect>,
    bounds: LayoutBounds,
    translation: LayoutTranslation,
}

impl PlacedLayout {
    /// Returns the final footprints, in the caller's input order.
    #[must_use]
    pub fn rects(&self) -> &[LayoutRect] {
        &self.rects
    }

    /// Returns the aggregate bounds of [`Self::rects`].
    #[must_use]
    pub const fn bounds(&self) -> LayoutBounds {
        self.bounds
    }

    /// Returns the translation the origin policy applied. Always zero under
    /// [`OriginPolicy::PreserveSigned`].
    #[must_use]
    pub const fn translation(&self) -> LayoutTranslation {
        self.translation
    }
}

/// Which coordinate space a rectangle's origin or extent is expressed in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayoutSpace {
    /// The client's logical desktop arrangement space: `Monitor::x`/`y` plus
    /// `RequestedMonitor::logical_width`/`logical_height`.
    LogicalArrangement,
    /// Host physical/backing pixels: applied desktop rectangles and
    /// per-monitor stream sizes.
    HostPixel,
}

impl Display for LayoutSpace {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LogicalArrangement => formatter.write_str("logical arrangement"),
            Self::HostPixel => formatter.write_str("host pixel"),
        }
    }
}

/// A layout rectangle carrying the coordinate space of its origin and of its
/// extent separately, so an ADR 0009 mixed-unit combination is visible instead
/// of implicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpacedLayoutRect {
    /// Space the rectangle's `x`/`y` are expressed in.
    pub origin_space: LayoutSpace,
    /// Space the rectangle's `width`/`height` are expressed in.
    pub extent_space: LayoutSpace,
    /// The rectangle itself.
    pub rect: LayoutRect,
}

impl SpacedLayoutRect {
    /// Tags a rectangle whose origin and extent are both logical arrangement
    /// units.
    #[must_use]
    pub const fn logical(rect: LayoutRect) -> Self {
        Self {
            origin_space: LayoutSpace::LogicalArrangement,
            extent_space: LayoutSpace::LogicalArrangement,
            rect,
        }
    }

    /// Tags a rectangle whose origin and extent are both host pixels.
    #[must_use]
    pub const fn host_pixel(rect: LayoutRect) -> Self {
        Self {
            origin_space: LayoutSpace::HostPixel,
            extent_space: LayoutSpace::HostPixel,
            rect,
        }
    }

    /// Tags a rectangle with independent origin and extent spaces.
    #[must_use]
    pub const fn new(
        origin_space: LayoutSpace,
        extent_space: LayoutSpace,
        rect: LayoutRect,
    ) -> Self {
        Self {
            origin_space,
            extent_space,
            rect,
        }
    }

    /// Whether this rectangle mixes coordinate spaces, which ADR 0009 forbids
    /// aggregating.
    #[must_use]
    pub const fn is_mixed_unit(self) -> bool {
        !matches!(
            (self.origin_space, self.extent_space),
            (
                LayoutSpace::LogicalArrangement,
                LayoutSpace::LogicalArrangement
            ) | (LayoutSpace::HostPixel, LayoutSpace::HostPixel)
        )
    }
}

/// Typed rejection from the shared placement primitives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum TopologyPlacementError {
    /// A monitor's presentation scale cannot be represented in [`Scale120`].
    ScaleOutOfRange,
    /// The supplied primary index is not inside the monitor roster.
    PrimaryIndexOutOfRange {
        /// The out-of-range index.
        index: usize,
        /// The roster length.
        len: usize,
    },
    /// ADR 0009: a rectangle combined an origin from one coordinate space with
    /// an extent from another (for example a logical origin with a physical
    /// stream size).
    MixedUnitRect {
        /// Space of the rectangle's origin.
        origin_space: LayoutSpace,
        /// Space of the rectangle's extent.
        extent_space: LayoutSpace,
    },
    /// A rectangle was not expressed in the aggregate's declared space.
    LayoutSpaceMismatch {
        /// Space the aggregate was requested in.
        expected: LayoutSpace,
        /// Space the offending rectangle was tagged with.
        actual: LayoutSpace,
    },
    /// A shared monitor/layout contract invariant failed.
    Contract(MediaContractError),
    /// A shared region contract invariant failed.
    Region(RegionContractError),
}

impl Display for TopologyPlacementError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ScaleOutOfRange => {
                formatter.write_str("monitor scale cannot be represented in Scale120 units")
            }
            Self::PrimaryIndexOutOfRange { index, len } => write!(
                formatter,
                "primary index {index} is outside a roster of {len} monitors"
            ),
            Self::MixedUnitRect {
                origin_space,
                extent_space,
            } => write!(
                formatter,
                "ADR 0009 forbids aggregating a {origin_space} origin with a {extent_space} extent"
            ),
            Self::LayoutSpaceMismatch { expected, actual } => write!(
                formatter,
                "expected every rectangle in {expected} space, found {actual}"
            ),
            Self::Contract(error) => write!(formatter, "{error}"),
            Self::Region(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for TopologyPlacementError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Contract(error) => Some(error),
            Self::Region(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MediaContractError> for TopologyPlacementError {
    fn from(value: MediaContractError) -> Self {
        Self::Contract(value)
    }
}

impl From<RegionContractError> for TopologyPlacementError {
    fn from(value: RegionContractError) -> Self {
        Self::Region(value)
    }
}

/// Which edge of an already-placed anchor a not-yet-placed candidate touches
/// in the client's logical arrangement space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TouchingEdge {
    Right,
    Left,
    Below,
    Above,
}

/// Returns the edge `candidate` touches on `anchor`, if any: a shared,
/// zero-gap, zero-overlap boundary on one axis with a strict overlap
/// requirement on the perpendicular axis. This matches how a desktop
/// compositor only ever tiles output rectangles edge-to-edge (never
/// diagonally, and never on a boundary that merely brushes a single point).
fn touching_edge(anchor: LayoutRect, candidate: LayoutRect) -> Option<TouchingEdge> {
    let anchor_right = anchor.right_exclusive();
    let anchor_bottom = anchor.bottom_exclusive();
    let candidate_right = candidate.right_exclusive();
    let candidate_bottom = candidate.bottom_exclusive();
    let vertical_overlap =
        i64::from(candidate.y) < anchor_bottom && candidate_bottom > i64::from(anchor.y);
    let horizontal_overlap =
        i64::from(candidate.x) < anchor_right && candidate_right > i64::from(anchor.x);

    if vertical_overlap && i64::from(candidate.x) == anchor_right {
        Some(TouchingEdge::Right)
    } else if vertical_overlap && candidate_right == i64::from(anchor.x) {
        Some(TouchingEdge::Left)
    } else if horizontal_overlap && i64::from(candidate.y) == anchor_bottom {
        Some(TouchingEdge::Below)
    } else if horizontal_overlap && candidate_bottom == i64::from(anchor.y) {
        Some(TouchingEdge::Above)
    } else {
        None
    }
}

fn checked_round_offset(logical_delta: i64, scale: f64) -> Result<i32, TopologyPlacementError> {
    #[allow(clippy::cast_precision_loss)]
    let scaled = (logical_delta as f64 * scale).round();
    if !scaled.is_finite() || scaled < f64::from(i32::MIN) || scaled > f64::from(i32::MAX) {
        return Err(TopologyPlacementError::Contract(
            MediaContractError::CoordinateOverflow,
        ));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(scaled as i32)
}

fn checked_i64_position(value: i64) -> Result<i32, TopologyPlacementError> {
    i32::try_from(value)
        .map_err(|_| TopologyPlacementError::Contract(MediaContractError::CoordinateOverflow))
}

/// Derives every requested monitor's host-pixel desktop origin from the
/// client's logical arrangement, anchored at the primary.
///
/// A single global "primary scale" applied to every monitor's logical offset
/// only preserves exact edge adjacency when every monitor shares the primary's
/// own scale; a chain of differently-scaled monitors drifts apart (gap) or
/// collides (overlap) once the accumulated offset error compounds past the
/// first hop. This instead walks the touching-edge graph breadth-first from
/// the primary: a monitor that touches an already-placed neighbor's logical
/// edge (left, right, above, or below, with perpendicular-axis overlap) is
/// placed flush against that neighbor's own already-computed host-pixel
/// footprint, using the *neighbor's* own scale only for the shared edge's
/// cross-axis offset — so gap-/overlap-free adjacency holds regardless of how
/// many differently scaled hops separate a monitor from the primary.
///
/// A monitor with no touching path back to the primary (a genuine
/// logical-layout gap, or a disconnected cluster) falls back to converting its
/// absolute logical offset from the primary using the primary's own scale,
/// preserving intentionally disconnected layouts.
///
/// Returned offsets are the top-left origin of each monitor's *footprint*
/// under `convention`, in the caller's input order, before any
/// [`OriginPolicy`] is applied. The primary is always at `(0, 0)`.
///
/// # Errors
///
/// Returns an error when `primary_index` is out of range, a requested
/// monitor's logical rectangle is invalid, or a coordinate conversion
/// overflows the signed desktop domain.
pub fn plan_edge_aware_offsets(
    monitors: &[RequestedMonitor],
    primary_index: usize,
    convention: TransformConvention,
) -> Result<Vec<(i32, i32)>, TopologyPlacementError> {
    if primary_index >= monitors.len() {
        return Err(TopologyPlacementError::PrimaryIndexOutOfRange {
            index: primary_index,
            len: monitors.len(),
        });
    }
    let logical_rects = monitors
        .iter()
        .map(RequestedMonitor::logical_arrangement_rect)
        .collect::<Result<Vec<_>, _>>()?;
    let footprints: Vec<(u32, u32)> = monitors
        .iter()
        .map(|monitor| convention.monitor_desktop_footprint(monitor))
        .collect();
    let scales: Vec<(f64, f64)> = monitors
        .iter()
        .zip(&footprints)
        .map(|(monitor, (footprint_width, footprint_height))| {
            (
                f64::from(*footprint_width) / f64::from(monitor.logical_width),
                f64::from(*footprint_height) / f64::from(monitor.logical_height),
            )
        })
        .collect();

    let mut placed: Vec<Option<(i64, i64)>> = vec![None; monitors.len()];
    placed[primary_index] = Some((0, 0));
    let mut queue = VecDeque::new();
    queue.push_back(primary_index);
    while let Some(anchor_index) = queue.pop_front() {
        let anchor_logical = logical_rects[anchor_index];
        let Some((anchor_x, anchor_y)) = placed[anchor_index] else {
            continue;
        };
        let (anchor_footprint_width, anchor_footprint_height) = footprints[anchor_index];
        let (anchor_scale_x, anchor_scale_y) = scales[anchor_index];
        for candidate_index in 0..monitors.len() {
            if placed[candidate_index].is_some() {
                continue;
            }
            let candidate_logical = logical_rects[candidate_index];
            let Some(edge) = touching_edge(anchor_logical, candidate_logical) else {
                continue;
            };
            let (candidate_footprint_width, candidate_footprint_height) =
                footprints[candidate_index];
            let (x, y) = match edge {
                TouchingEdge::Right | TouchingEdge::Left => {
                    let cross_delta = i64::from(candidate_logical.y) - i64::from(anchor_logical.y);
                    let cross_offset = checked_round_offset(cross_delta, anchor_scale_y)?;
                    let x = if edge == TouchingEdge::Right {
                        anchor_x + i64::from(anchor_footprint_width)
                    } else {
                        anchor_x - i64::from(candidate_footprint_width)
                    };
                    (x, anchor_y + i64::from(cross_offset))
                }
                TouchingEdge::Below | TouchingEdge::Above => {
                    let cross_delta = i64::from(candidate_logical.x) - i64::from(anchor_logical.x);
                    let cross_offset = checked_round_offset(cross_delta, anchor_scale_x)?;
                    let y = if edge == TouchingEdge::Below {
                        anchor_y + i64::from(anchor_footprint_height)
                    } else {
                        anchor_y - i64::from(candidate_footprint_height)
                    };
                    (anchor_x + i64::from(cross_offset), y)
                }
            };
            placed[candidate_index] = Some((x, y));
            queue.push_back(candidate_index);
        }
    }

    let primary_logical = logical_rects[primary_index];
    let (primary_scale_x, primary_scale_y) = scales[primary_index];
    for (index, slot) in placed.iter_mut().enumerate() {
        if slot.is_some() {
            continue;
        }
        let logical = logical_rects[index];
        let horizontal_delta = i64::from(logical.x) - i64::from(primary_logical.x);
        let vertical_delta = i64::from(logical.y) - i64::from(primary_logical.y);
        let x = checked_round_offset(horizontal_delta, primary_scale_x)?;
        let y = checked_round_offset(vertical_delta, primary_scale_y)?;
        *slot = Some((i64::from(x), i64::from(y)));
    }

    placed
        .into_iter()
        .map(|entry| {
            let (x, y) = entry.ok_or(TopologyPlacementError::Contract(
                MediaContractError::CoordinateOverflow,
            ))?;
            Ok((checked_i64_position(x)?, checked_i64_position(y)?))
        })
        .collect()
}

/// Applies `policy` to already-placed host-pixel footprints and returns the
/// final rectangles, their aggregate bounds, and the applied translation.
///
/// # Errors
///
/// Returns an error when `rects` is empty or a checked bounding/translation
/// calculation overflows the signed desktop domain.
pub fn apply_origin_policy(
    rects: Vec<LayoutRect>,
    policy: OriginPolicy,
) -> Result<PlacedLayout, TopologyPlacementError> {
    let bounds = checked_layout_bounds(
        LayoutSpace::HostPixel,
        &rects
            .iter()
            .copied()
            .map(SpacedLayoutRect::host_pixel)
            .collect::<Vec<_>>(),
    )?;
    let translation = match policy {
        OriginPolicy::PreserveSigned => LayoutTranslation::default(),
        OriginPolicy::TranslateToNonNegative => bounds.translation_to_origin(),
    };
    if translation.dx == 0 && translation.dy == 0 {
        return Ok(PlacedLayout {
            rects,
            bounds,
            translation,
        });
    }
    let translated = rects
        .into_iter()
        .map(|rect| rect.translated(translation))
        .collect::<Result<Vec<_>, MediaContractError>>()?;
    let bounds = checked_layout_bounds(
        LayoutSpace::HostPixel,
        &translated
            .iter()
            .copied()
            .map(SpacedLayoutRect::host_pixel)
            .collect::<Vec<_>>(),
    )?;
    Ok(PlacedLayout {
        rects: translated,
        bounds,
        translation,
    })
}

/// Places a requested roster end to end: edge-aware placement under
/// `convention` followed by `policy`.
///
/// # Errors
///
/// Returns an error when placement or the origin policy rejects the layout.
pub fn place_monitors(
    monitors: &[RequestedMonitor],
    primary_index: usize,
    convention: TransformConvention,
    policy: OriginPolicy,
) -> Result<PlacedLayout, TopologyPlacementError> {
    let offsets = plan_edge_aware_offsets(monitors, primary_index, convention)?;
    let rects = monitors
        .iter()
        .zip(&offsets)
        .map(|(monitor, (x, y))| {
            let (width, height) = convention.monitor_desktop_footprint(monitor);
            LayoutRect::new(*x, *y, width, height)
        })
        .collect::<Result<Vec<_>, MediaContractError>>()?;
    apply_origin_policy(rects, policy)
}

/// Computes checked aggregate bounds over rectangles that must all be
/// expressed in `space`.
///
/// # Errors
///
/// Returns [`TopologyPlacementError::MixedUnitRect`] for a rectangle whose
/// origin and extent are in different coordinate spaces (the ADR 0009
/// prohibition), [`TopologyPlacementError::LayoutSpaceMismatch`] for a
/// rectangle in a different space than `space`, or a contract error when the
/// input is empty or the aggregate overflows.
pub fn checked_layout_bounds(
    space: LayoutSpace,
    rects: &[SpacedLayoutRect],
) -> Result<LayoutBounds, TopologyPlacementError> {
    let mut plain = Vec::with_capacity(rects.len());
    for spaced in rects {
        if spaced.is_mixed_unit() {
            return Err(TopologyPlacementError::MixedUnitRect {
                origin_space: spaced.origin_space,
                extent_space: spaced.extent_space,
            });
        }
        if spaced.origin_space != space {
            return Err(TopologyPlacementError::LayoutSpaceMismatch {
                expected: space,
                actual: spaced.origin_space,
            });
        }
        plain.push(spaced.rect);
    }
    Ok(LayoutBounds::from_rects(&plain)?)
}

/// Returns one requested monitor's logical arrangement rectangle, tagged as
/// wholly logical.
///
/// # Errors
///
/// Returns an error when the logical rectangle is zero-sized or overflows.
pub fn logical_arrangement_rect(
    monitor: &RequestedMonitor,
) -> Result<SpacedLayoutRect, TopologyPlacementError> {
    Ok(SpacedLayoutRect::logical(
        monitor.logical_arrangement_rect()?,
    ))
}

/// Returns one applied monitor's host-pixel desktop rectangle, tagged as
/// wholly host-pixel.
///
/// # Errors
///
/// Returns an error when the applied rectangle is zero-sized or overflows.
pub fn applied_desktop_rect(
    monitor: &AppliedMonitor,
) -> Result<SpacedLayoutRect, TopologyPlacementError> {
    Ok(SpacedLayoutRect::host_pixel(monitor.desktop_rect_px()?))
}

/// Returns the ADR-0009-forbidden combination of a requested monitor's
/// *logical* arrangement origin with its *physical* stream extent, tagged so
/// [`checked_layout_bounds`] rejects it.
///
/// This exists so the prohibited derivation is a named, checked, rejected
/// value rather than an easy-to-write `LayoutRect::new(monitor.x, monitor.y,
/// monitor.width_px, monitor.height_px)` that silently produces a
/// plausible-looking aggregate.
///
/// # Errors
///
/// Returns an error when the rectangle is zero-sized or overflows.
pub fn logical_origin_with_stream_extent_rect(
    monitor: &RequestedMonitor,
) -> Result<SpacedLayoutRect, TopologyPlacementError> {
    let raw = monitor.monitor();
    Ok(SpacedLayoutRect::new(
        LayoutSpace::LogicalArrangement,
        LayoutSpace::HostPixel,
        LayoutRect::new(raw.x, raw.y, raw.width_px, raw.height_px)?,
    ))
}

/// Converts a floating-point presentation scale into the shared 1/120
/// fixed-point representation.
///
/// # Errors
///
/// Returns [`TopologyPlacementError::ScaleOutOfRange`] when the scale is
/// non-finite, non-positive, or rounds outside the representable range.
pub fn scale120_from_scale(scale: f32) -> Result<Scale120, TopologyPlacementError> {
    let units = (f64::from(scale) * f64::from(Scale120::denominator())).round();
    if !units.is_finite() || units < 1.0 || units > f64::from(u32::MAX) {
        return Err(TopologyPlacementError::ScaleOutOfRange);
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(Scale120::new(units as u32)?)
}

/// Converts a signed layout rectangle into the shared fixed-point logical
/// region rectangle.
///
/// # Errors
///
/// Returns an error when the rectangle cannot be represented in the shared
/// fixed-point logical domain.
pub fn logical_rect_from_layout(rect: LayoutRect) -> Result<LogicalRect, TopologyPlacementError> {
    Ok(LogicalRect::new(
        LogicalPoint::from_pixels(i64::from(rect.x), i64::from(rect.y))?,
        LogicalSize::from_pixels(u64::from(rect.width), u64::from(rect.height))?,
    )?)
}

/// One region's committed placement facts, in the caller's explicit
/// conventions.
///
/// `stream_size` is interpreted by the [`TransformConvention`] passed to
/// [`build_region_sets`]: it is the native pre-transform extent under
/// [`TransformConvention::NativeNeedsTransform`] and the already-oriented
/// extent under [`TransformConvention::AlreadyCompositorOriented`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegionPlacement {
    /// Stable region identity.
    pub region_id: RegionId,
    /// Opaque host output identity.
    pub output: OutputIdentity,
    /// Requested logical arrangement rectangle in shared fixed-point units.
    pub logical_rect: LogicalRect,
    /// Per-region stream extent, interpreted by the convention.
    pub stream_size: PhysicalSize,
    /// Presentation scale in 1/120 units.
    pub scale: Scale120,
    /// Panel/output rotation. Ignored under
    /// [`TransformConvention::AlreadyCompositorOriented`], which records
    /// [`OutputTransform::Normal`].
    pub rotation: Rotation,
    /// Whether this is the primary region.
    pub primary: bool,
    /// Committed applied rectangle in host pixels.
    pub applied_rect: AppliedRect,
}

/// Builds the shared requested and applied region aggregates for one committed
/// plan under an explicit [`TransformConvention`].
///
/// Region order is preserved exactly, so the returned sets zip 1:1 with
/// `placements`.
///
/// # Errors
///
/// Returns a shared region-contract error when identities, roster size,
/// primary count, or an applied extent disagrees with the declared stream size
/// after the convention's transform.
pub fn build_region_sets(
    generation: RegionGeneration,
    convention: TransformConvention,
    placements: &[RegionPlacement],
) -> Result<(RegionSet, AppliedRegionSet), RegionContractError> {
    let descriptors = placements
        .iter()
        .map(|placement| {
            RegionDescriptor::new(
                placement.region_id,
                placement.output.clone(),
                placement.logical_rect,
                placement.stream_size,
                placement.scale,
                convention.region_transform(placement.rotation),
                placement.primary,
            )
        })
        .collect::<Vec<_>>();
    let requested = RegionSet::new(generation, descriptors)?;
    let applied = requested
        .regions()
        .iter()
        .zip(placements)
        .map(|(descriptor, placement)| {
            AppliedRegionDescriptor::new(descriptor.clone(), placement.applied_rect)
        })
        .collect::<Result<Vec<_>, RegionContractError>>()?;
    Ok((requested, AppliedRegionSet::new(generation, applied)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::multi_monitor::{RequestedMonitorTopology, SessionMonitorId};
    use crate::region::{AppliedPoint, AppliedSize};
    use crate::{Monitor, MonitorIdentity};

    #[allow(clippy::too_many_arguments)]
    fn monitor(
        id: &str,
        x: i32,
        y: i32,
        width_px: u32,
        height_px: u32,
        logical_width: u32,
        logical_height: u32,
        primary: bool,
        rotation: Rotation,
    ) -> RequestedMonitor {
        #[allow(clippy::cast_possible_truncation)]
        let scale = (f64::from(width_px) / f64::from(logical_width)) as f32;
        RequestedMonitor::new(
            Monitor {
                identity: MonitorIdentity {
                    id: id.to_owned(),
                    name: format!("Display {id}"),
                    ..MonitorIdentity::default()
                },
                x,
                y,
                width_px,
                height_px,
                scale,
                refresh_hz: 60,
                rotation,
                primary,
                width_mm: 0.0,
                height_mm: 0.0,
            },
            logical_width,
            logical_height,
        )
        .expect("requested monitor")
    }

    fn primary_index(monitors: &[RequestedMonitor]) -> usize {
        monitors
            .iter()
            .position(|monitor| monitor.monitor().primary)
            .expect("primary monitor")
    }

    fn placed_x_widths(placed: &PlacedLayout) -> Vec<(i32, u32)> {
        placed
            .rects()
            .iter()
            .map(|rect| (rect.x, rect.width))
            .collect()
    }

    fn assert_chain_is_flush(placed: &PlacedLayout) {
        let mut sorted = placed.rects().to_vec();
        sorted.sort_by_key(|rect| rect.x);
        for adjacent in sorted.windows(2) {
            assert_eq!(
                adjacent[0].right_exclusive(),
                i64::from(adjacent[1].x),
                "chain must stay gap-free and overlap-free"
            );
        }
    }

    #[test]
    fn three_monitor_mixed_scale_touching_chain_is_flush() {
        let monitors = vec![
            monitor("a", 0, 0, 1920, 1080, 960, 540, true, Rotation::Degrees0),
            monitor("b", 960, 0, 1280, 720, 1280, 720, false, Rotation::Degrees0),
            monitor("c", 2240, 0, 1200, 900, 800, 600, false, Rotation::Degrees0),
        ];
        let placed = place_monitors(
            &monitors,
            primary_index(&monitors),
            TransformConvention::NativeNeedsTransform,
            OriginPolicy::TranslateToNonNegative,
        )
        .expect("placed");
        assert_eq!(
            placed_x_widths(&placed),
            vec![(0, 1920), (1920, 1280), (3200, 1200)]
        );
        assert_chain_is_flush(&placed);
        assert_eq!(placed.bounds().width, 4400);
        assert_eq!(placed.bounds().height, 1080);
    }

    #[test]
    fn four_monitor_mixed_scale_touching_chain_is_flush() {
        let monitors = vec![
            monitor("a", 0, 0, 1280, 720, 1280, 720, true, Rotation::Degrees0),
            monitor(
                "b",
                1280,
                0,
                1920,
                1080,
                960,
                540,
                false,
                Rotation::Degrees0,
            ),
            monitor(
                "c",
                2240,
                0,
                1280,
                960,
                1024,
                768,
                false,
                Rotation::Degrees0,
            ),
            monitor("d", 3264, 0, 1200, 900, 800, 600, false, Rotation::Degrees0),
        ];
        let placed = place_monitors(
            &monitors,
            primary_index(&monitors),
            TransformConvention::NativeNeedsTransform,
            OriginPolicy::TranslateToNonNegative,
        )
        .expect("placed");
        assert_eq!(
            placed_x_widths(&placed),
            vec![(0, 1280), (1280, 1920), (3200, 1280), (4480, 1200)]
        );
        assert_chain_is_flush(&placed);
        assert_eq!(placed.bounds().width, 5680);
    }

    #[test]
    fn signed_origins_are_preserved_or_translated_by_explicit_policy() {
        let monitors = vec![
            monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                960,
                540,
                true,
                Rotation::Degrees0,
            ),
            monitor(
                "left",
                -1280,
                0,
                1280,
                720,
                1280,
                720,
                false,
                Rotation::Degrees0,
            ),
            monitor(
                "far-left",
                -2080,
                0,
                1200,
                900,
                800,
                600,
                false,
                Rotation::Degrees0,
            ),
        ];
        let index = primary_index(&monitors);
        let offsets =
            plan_edge_aware_offsets(&monitors, index, TransformConvention::NativeNeedsTransform)
                .expect("offsets");
        assert_eq!(offsets, vec![(0, 0), (-1280, 0), (-2480, 0)]);

        let signed = place_monitors(
            &monitors,
            index,
            TransformConvention::NativeNeedsTransform,
            OriginPolicy::PreserveSigned,
        )
        .expect("signed");
        assert_eq!(signed.translation(), LayoutTranslation::default());
        assert_eq!(
            placed_x_widths(&signed),
            vec![(0, 1920), (-1280, 1280), (-2480, 1200)]
        );
        assert_eq!(signed.bounds().x, -2480);
        assert_chain_is_flush(&signed);

        let translated = place_monitors(
            &monitors,
            index,
            TransformConvention::NativeNeedsTransform,
            OriginPolicy::TranslateToNonNegative,
        )
        .expect("translated");
        assert_eq!(translated.translation(), LayoutTranslation::new(2480, 0));
        assert_eq!(
            placed_x_widths(&translated),
            vec![(2480, 1920), (1200, 1280), (0, 1200)]
        );
        assert_eq!(translated.bounds().x, 0);
        assert_eq!(translated.bounds().width, 4400);
        assert_chain_is_flush(&translated);
    }

    #[test]
    fn every_rotation_is_footprint_and_transform_aware_per_convention() {
        for (rotation, swapped, transform) in [
            (Rotation::Degrees0, false, OutputTransform::Normal),
            (Rotation::Degrees90, true, OutputTransform::Rotate90),
            (Rotation::Degrees180, false, OutputTransform::Rotate180),
            (Rotation::Degrees270, true, OutputTransform::Rotate270),
        ] {
            let native = TransformConvention::NativeNeedsTransform;
            let oriented = TransformConvention::AlreadyCompositorOriented;
            let expected_native = if swapped { (1080, 1920) } else { (1920, 1080) };
            assert_eq!(
                native.desktop_footprint(1920, 1080, rotation),
                expected_native,
                "{rotation:?} native footprint"
            );
            assert_eq!(native.region_transform(rotation), transform);
            assert_eq!(
                oriented.desktop_footprint(1920, 1080, rotation),
                (1920, 1080),
                "{rotation:?} compositor-oriented footprint"
            );
            assert_eq!(
                oriented.region_transform(rotation),
                OutputTransform::Normal,
                "{rotation:?} must never transform a compositor-oriented stream"
            );
        }
    }

    #[test]
    fn rotated_chain_placement_differs_between_conventions() {
        // A 90-degree secondary declared with an already-rotated logical
        // arrangement: under the host convention it occupies a 1080x1920
        // footprint, under the client convention it stays 1920x1080.
        let monitors = vec![
            monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                1920,
                1080,
                true,
                Rotation::Degrees0,
            ),
            monitor(
                "portrait",
                1920,
                0,
                1920,
                1080,
                1080,
                1920,
                false,
                Rotation::Degrees90,
            ),
        ];
        let index = primary_index(&monitors);
        let native = place_monitors(
            &monitors,
            index,
            TransformConvention::NativeNeedsTransform,
            OriginPolicy::TranslateToNonNegative,
        )
        .expect("native");
        assert_eq!(placed_x_widths(&native), vec![(0, 1920), (1920, 1080)]);
        assert_eq!(native.bounds().height, 1920);

        let oriented = place_monitors(
            &monitors,
            index,
            TransformConvention::AlreadyCompositorOriented,
            OriginPolicy::TranslateToNonNegative,
        )
        .expect("oriented");
        assert_eq!(placed_x_widths(&oriented), vec![(0, 1920), (1920, 1920)]);
        assert_eq!(oriented.bounds().height, 1080);
    }

    #[test]
    fn disconnected_monitor_falls_back_to_the_primary_scale() {
        let monitors = vec![
            monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                960,
                540,
                true,
                Rotation::Degrees0,
            ),
            monitor(
                "island",
                4000,
                0,
                1280,
                720,
                1280,
                720,
                false,
                Rotation::Degrees0,
            ),
        ];
        let offsets = plan_edge_aware_offsets(
            &monitors,
            primary_index(&monitors),
            TransformConvention::NativeNeedsTransform,
        )
        .expect("offsets");
        // No touching edge: 4000 logical units at the primary's own 2.0 scale.
        assert_eq!(offsets, vec![(0, 0), (8000, 0)]);
    }

    #[test]
    fn a_cyclic_touching_graph_places_every_monitor_exactly_once() {
        // A closed 2x2 ring: every monitor touches two neighbors, so a naive
        // walk could revisit and re-place one. Breadth-first placement claims
        // each monitor on first reach and never re-places it.
        let monitors = vec![
            monitor(
                "top-left",
                0,
                0,
                1920,
                1080,
                960,
                540,
                true,
                Rotation::Degrees0,
            ),
            monitor(
                "top-right",
                960,
                0,
                1280,
                720,
                1280,
                720,
                false,
                Rotation::Degrees0,
            ),
            monitor(
                "bottom-left",
                0,
                540,
                1920,
                1080,
                960,
                540,
                false,
                Rotation::Degrees0,
            ),
            monitor(
                "bottom-right",
                960,
                540,
                1280,
                720,
                1280,
                720,
                false,
                Rotation::Degrees0,
            ),
        ];
        let offsets = plan_edge_aware_offsets(
            &monitors,
            primary_index(&monitors),
            TransformConvention::NativeNeedsTransform,
        )
        .expect("offsets");
        assert_eq!(
            offsets,
            vec![(0, 0), (1920, 0), (0, 1080), (1920, 1080)],
            "each ring member is placed once, flush to its first-reached anchor"
        );
    }

    #[test]
    fn an_ambiguous_corner_touch_is_not_an_edge() {
        // Diagonal corner contact shares exactly one point and no
        // perpendicular-axis overlap, so it is a genuine layout gap, not an
        // edge: the candidate falls back to the primary's own scale.
        let monitors = vec![
            monitor(
                "primary",
                0,
                0,
                1920,
                1080,
                960,
                540,
                true,
                Rotation::Degrees0,
            ),
            monitor(
                "diagonal",
                960,
                540,
                1280,
                720,
                1280,
                720,
                false,
                Rotation::Degrees0,
            ),
        ];
        let offsets = plan_edge_aware_offsets(
            &monitors,
            primary_index(&monitors),
            TransformConvention::NativeNeedsTransform,
        )
        .expect("offsets");
        assert_eq!(offsets, vec![(0, 0), (1920, 1080)]);
    }

    #[test]
    fn placement_rejects_an_out_of_range_primary_index() {
        let monitors = vec![monitor(
            "only",
            0,
            0,
            1920,
            1080,
            1920,
            1080,
            true,
            Rotation::Degrees0,
        )];
        assert_eq!(
            plan_edge_aware_offsets(&monitors, 1, TransformConvention::NativeNeedsTransform),
            Err(TopologyPlacementError::PrimaryIndexOutOfRange { index: 1, len: 1 })
        );
    }

    #[test]
    fn placement_rejects_an_offset_that_overflows_the_signed_desktop() {
        let monitors = vec![
            monitor("primary", 0, 0, 3840, 2160, 2, 2, true, Rotation::Degrees0),
            monitor(
                "far",
                2_000_000_000,
                0,
                1280,
                720,
                1280,
                720,
                false,
                Rotation::Degrees0,
            ),
        ];
        assert_eq!(
            plan_edge_aware_offsets(
                &monitors,
                primary_index(&monitors),
                TransformConvention::NativeNeedsTransform
            ),
            Err(TopologyPlacementError::Contract(
                MediaContractError::CoordinateOverflow
            ))
        );
    }

    #[test]
    fn origin_policy_rejects_a_translation_that_overflows() {
        let rects = vec![
            LayoutRect::new(i32::MIN, 0, 4, 4).expect("min-edge rect"),
            LayoutRect::new(i32::MAX - 4, 0, 4, 4).expect("max-edge rect"),
        ];
        assert_eq!(
            apply_origin_policy(rects, OriginPolicy::TranslateToNonNegative),
            Err(TopologyPlacementError::Contract(
                MediaContractError::CoordinateOverflow
            ))
        );
    }

    #[test]
    fn scale120_round_trips_and_rejects_unrepresentable_scales() {
        assert_eq!(
            scale120_from_scale(1.0).expect("1x"),
            Scale120::new(120).expect("120")
        );
        assert_eq!(
            scale120_from_scale(1.5).expect("1.5x"),
            Scale120::new(180).expect("180")
        );
        assert_eq!(
            scale120_from_scale(2.0).expect("2x"),
            Scale120::new(240).expect("240")
        );
        assert_eq!(
            scale120_from_scale(1.25).expect("1.25x"),
            Scale120::new(150).expect("150")
        );
        for invalid in [0.0_f32, -1.0, f32::NAN, f32::INFINITY, 0.001] {
            assert_eq!(
                scale120_from_scale(invalid),
                Err(TopologyPlacementError::ScaleOutOfRange),
                "{invalid} must be rejected"
            );
        }
    }

    #[test]
    fn adr_0009_rejects_pixel_bounds_built_from_logical_origins_and_stream_sizes() {
        let primary = monitor(
            "primary",
            0,
            0,
            1920,
            1080,
            960,
            540,
            true,
            Rotation::Degrees0,
        );
        let secondary = monitor(
            "secondary",
            960,
            0,
            1280,
            720,
            1280,
            720,
            false,
            Rotation::Degrees0,
        );

        let logical = [
            logical_arrangement_rect(&primary).expect("primary logical"),
            logical_arrangement_rect(&secondary).expect("secondary logical"),
        ];
        assert_eq!(
            checked_layout_bounds(LayoutSpace::LogicalArrangement, &logical).expect("logical"),
            LayoutBounds {
                x: 0,
                y: 0,
                width: 2240,
                height: 720,
            }
        );

        let mixed = [
            logical_origin_with_stream_extent_rect(&primary).expect("mixed primary"),
            logical_origin_with_stream_extent_rect(&secondary).expect("mixed secondary"),
        ];
        assert_eq!(
            checked_layout_bounds(LayoutSpace::HostPixel, &mixed),
            Err(TopologyPlacementError::MixedUnitRect {
                origin_space: LayoutSpace::LogicalArrangement,
                extent_space: LayoutSpace::HostPixel,
            })
        );
        assert_eq!(
            checked_layout_bounds(LayoutSpace::LogicalArrangement, &mixed),
            Err(TopologyPlacementError::MixedUnitRect {
                origin_space: LayoutSpace::LogicalArrangement,
                extent_space: LayoutSpace::HostPixel,
            })
        );

        // The honest host-pixel aggregate comes from applied rectangles.
        let topology = RequestedMonitorTopology::new(vec![primary.clone(), secondary.clone()])
            .expect("requested");
        let index = primary_index(topology.monitors());
        let placed = place_monitors(
            topology.monitors(),
            index,
            TransformConvention::NativeNeedsTransform,
            OriginPolicy::TranslateToNonNegative,
        )
        .expect("placed");
        assert_eq!(placed.bounds().width, 3200);
        assert_eq!(placed.bounds().height, 1080);

        let applied = AppliedMonitor::new(
            SessionMonitorId::new(1).expect("session monitor id"),
            primary,
            0,
            0,
        )
        .expect("applied");
        assert_eq!(
            applied_desktop_rect(&applied).expect("applied rect"),
            SpacedLayoutRect::host_pixel(LayoutRect::new(0, 0, 1920, 1080).expect("rect"))
        );
    }

    #[test]
    fn checked_layout_bounds_rejects_a_space_mismatch() {
        let rect = LayoutRect::new(0, 0, 100, 100).expect("rect");
        assert_eq!(
            checked_layout_bounds(LayoutSpace::HostPixel, &[SpacedLayoutRect::logical(rect)]),
            Err(TopologyPlacementError::LayoutSpaceMismatch {
                expected: LayoutSpace::HostPixel,
                actual: LayoutSpace::LogicalArrangement,
            })
        );
    }

    fn placement(
        id: u32,
        output: &str,
        stream: (u32, u32),
        applied: (i64, i64, u32, u32),
        rotation: Rotation,
        primary: bool,
    ) -> RegionPlacement {
        RegionPlacement {
            region_id: RegionId::new(id).expect("region id"),
            output: OutputIdentity::new(output).expect("output identity"),
            logical_rect: logical_rect_from_layout(
                LayoutRect::new(0, 0, stream.0, stream.1).expect("layout rect"),
            )
            .expect("logical rect"),
            stream_size: PhysicalSize::new(stream.0, stream.1).expect("stream size"),
            scale: Scale120::new(120).expect("scale"),
            rotation,
            primary,
            applied_rect: AppliedRect::new(
                AppliedPoint::new(applied.0, applied.1),
                AppliedSize::new(applied.2, applied.3).expect("applied size"),
            )
            .expect("applied rect"),
        }
    }

    #[test]
    fn region_sets_apply_the_declared_transform_convention() {
        let generation = RegionGeneration::new(7).expect("generation");
        let native = placement(
            1,
            "DFP-0",
            (1920, 1080),
            (0, 0, 1080, 1920),
            Rotation::Degrees90,
            true,
        );
        let (requested, applied) = build_region_sets(
            generation,
            TransformConvention::NativeNeedsTransform,
            std::slice::from_ref(&native),
        )
        .expect("native region sets");
        assert_eq!(
            requested.primary().transform(),
            OutputTransform::Rotate90,
            "a host stream keeps its native extent and carries the transform"
        );
        assert_eq!(
            requested.primary().physical_size(),
            PhysicalSize::new(1920, 1080).expect("native size")
        );
        assert_eq!(
            applied.primary().applied_rect().size(),
            AppliedSize::new(1080, 1920).expect("rotated footprint")
        );

        let oriented = placement(
            1,
            "33",
            (1080, 1920),
            (0, 0, 1080, 1920),
            Rotation::Degrees90,
            true,
        );
        let (requested, applied) = build_region_sets(
            generation,
            TransformConvention::AlreadyCompositorOriented,
            std::slice::from_ref(&oriented),
        )
        .expect("oriented region sets");
        assert_eq!(
            requested.primary().transform(),
            OutputTransform::Normal,
            "a compositor-oriented stream must never be rotated a second time"
        );
        assert_eq!(
            requested.primary().physical_size(),
            PhysicalSize::new(1080, 1920).expect("oriented size")
        );
        assert_eq!(
            applied.primary().applied_rect().size(),
            AppliedSize::new(1080, 1920).expect("oriented footprint")
        );
    }

    #[test]
    fn region_sets_reject_an_applied_extent_that_contradicts_the_convention() {
        let generation = RegionGeneration::new(3).expect("generation");
        // Native 1920x1080 rotated 90 degrees must land as 1080x1920.
        let contradictory = placement(
            1,
            "DFP-0",
            (1920, 1080),
            (0, 0, 1920, 1080),
            Rotation::Degrees90,
            true,
        );
        assert_eq!(
            build_region_sets(
                generation,
                TransformConvention::NativeNeedsTransform,
                std::slice::from_ref(&contradictory)
            ),
            Err(RegionContractError::AppliedSizeMismatch {
                expected: AppliedSize::new(1080, 1920).expect("expected"),
                actual: AppliedSize::new(1920, 1080).expect("actual"),
            })
        );
    }

    #[test]
    fn region_sets_preserve_input_order_and_roster_invariants() {
        let generation = RegionGeneration::new(2).expect("generation");
        let placements = vec![
            placement(
                1,
                "DFP-0",
                (1920, 1080),
                (0, 0, 1920, 1080),
                Rotation::Degrees0,
                true,
            ),
            placement(
                2,
                "DFP-1",
                (1280, 720),
                (1920, 0, 1280, 720),
                Rotation::Degrees0,
                false,
            ),
        ];
        let (requested, applied) = build_region_sets(
            generation,
            TransformConvention::NativeNeedsTransform,
            &placements,
        )
        .expect("region sets");
        assert_eq!(
            requested
                .regions()
                .iter()
                .map(|region| region.output_identity().as_str().to_owned())
                .collect::<Vec<_>>(),
            vec!["DFP-0".to_owned(), "DFP-1".to_owned()]
        );
        assert_eq!(applied.regions().len(), 2);
        assert_eq!(
            applied.regions()[1].applied_rect().origin(),
            AppliedPoint::new(1920, 0)
        );

        let mut duplicated = placements.clone();
        duplicated[1].output = OutputIdentity::new("DFP-0").expect("duplicate output");
        assert!(matches!(
            build_region_sets(
                generation,
                TransformConvention::NativeNeedsTransform,
                &duplicated
            ),
            Err(RegionContractError::DuplicateOutputIdentity(_))
        ));
    }
}
