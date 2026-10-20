//! Placement, reconfiguration and verification of a headless Wayland
//! session's virtual outputs.
//!
//! Two compositor behaviours exist, decided from the compositor's own
//! version, never from a distribution name:
//!
//! - Startup monitors ([`VirtualOutputMode::StartupMonitors`]), Mutter's `--virtual-monitor`
//!   outputs. Fixed for the compositor's lifetime: one output, no resize.
//! - Remote monitors ([`VirtualOutputMode::RemoteMonitors`]), `ScreenCast.RecordVirtual`
//!   outputs owned by the session's `ScreenCast` session. Mutter creates
//!   each monitor at the size and refresh its consumer negotiates, keeps it
//!   when the consumer leaves, resizes it in place when a consumer
//!   negotiates another size, and removes it with its session. Several
//!   outputs are placed with `DisplayConfig.ApplyMonitorsConfig`. Proven on
//!   Mutter 49 ([`REMOTE_MONITORS_MIN_MAJOR`]).
//!
//! Everything here is pure policy over facts the Linux adapter gathers:
//! which outputs to change and how, whether a layout is one Mutter accepts
//! (adjacent, non-overlapping, anchored at the origin), and whether the
//! compositor's logical monitors are exactly that layout.

use std::fmt::{Display, Formatter};

use crate::wayland_session::VirtualOutputRequest;

/// Lowest Mutter/GNOME Shell major version whose `RecordVirtual` monitors
/// were qualified to persist, resize in place and be placed (GNOME 49).
pub const REMOTE_MONITORS_MIN_MAJOR: u32 = 49;

/// Most virtual outputs one headless session serves (the shared
/// multi-monitor roster bound).
pub const MAX_WAYLAND_OUTPUTS: usize = 4;

/// How a compositor provides a session's virtual outputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VirtualOutputMode {
    /// `--virtual-monitor` at compositor start: one fixed output.
    StartupMonitors,
    /// `RecordVirtual` monitors: several outputs, resizable, placeable.
    RemoteMonitors,
}

impl VirtualOutputMode {
    /// The mode a compositor of `major` version supports; unknown is the
    /// fixed mode.
    #[must_use]
    pub const fn for_compositor_major(major: Option<u32>) -> Self {
        match major {
            Some(major) if major >= REMOTE_MONITORS_MIN_MAJOR => Self::RemoteMonitors,
            _ => Self::StartupMonitors,
        }
    }

    /// Whether outputs may change size or count during a session.
    #[must_use]
    pub const fn reconfigurable(self) -> bool {
        matches!(self, Self::RemoteMonitors)
    }

    /// Most outputs this mode serves.
    #[must_use]
    pub const fn max_outputs(self) -> usize {
        match self {
            Self::StartupMonitors => 1,
            Self::RemoteMonitors => MAX_WAYLAND_OUTPUTS,
        }
    }

    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::StartupMonitors => "startup-monitors",
            Self::RemoteMonitors => "remote-monitors",
        }
    }
}

/// The major version in `gnome-shell --version` (`GNOME Shell 49.4`) or
/// `mutter --version` (`mutter 49.4`) output: the first token that starts
/// with a digit, up to its first dot.
#[must_use]
pub fn parse_compositor_major(version_output: &str) -> Option<u32> {
    version_output
        .split_whitespace()
        .find(|token| token.as_bytes().first().is_some_and(u8::is_ascii_digit))
        .and_then(|token| token.split('.').next())
        .and_then(|major| major.parse().ok())
}

/// What the client said about the display an output stands for, from which
/// the host decides the output's desktop UI scale (a virtual output has no
/// physical size of its own). Zero means unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OutputDensity {
    /// Physical width in millimetres.
    pub width_mm: u32,
    /// Physical height in millimetres.
    pub height_mm: u32,
    /// The client's logical width of the display, in its points; larger
    /// than the output's width when the client streams backing pixels.
    pub logical_width: u32,
}

impl OutputDensity {
    /// Whether the client said anything.
    #[must_use]
    pub const fn is_known(&self) -> bool {
        (self.width_mm > 0 && self.height_mm > 0) || self.logical_width > 0
    }
}

/// One virtual output and where it sits in the session's desktop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedOutput {
    /// Size and refresh.
    pub request: VirtualOutputRequest,
    /// Left edge in desktop pixels.
    pub x: i32,
    /// Top edge in desktop pixels.
    pub y: i32,
    /// The client display's density, for the output's UI scale.
    pub density: OutputDensity,
}

impl PlacedOutput {
    /// An output at the desktop origin.
    #[must_use]
    pub const fn at_origin(request: VirtualOutputRequest) -> Self {
        Self {
            request,
            x: 0,
            y: 0,
            density: OutputDensity {
                width_mm: 0,
                height_mm: 0,
                logical_width: 0,
            },
        }
    }

    /// The same output standing for a display of `density`.
    #[must_use]
    pub const fn with_density(mut self, density: OutputDensity) -> Self {
        self.density = density;
        self
    }

    fn right(&self) -> i64 {
        i64::from(self.x) + i64::from(self.request.width())
    }

    fn bottom(&self) -> i64 {
        i64::from(self.y) + i64::from(self.request.height())
    }

    fn overlaps(&self, other: &Self) -> bool {
        i64::from(self.x) < other.right()
            && i64::from(other.x) < self.right()
            && i64::from(self.y) < other.bottom()
            && i64::from(other.y) < self.bottom()
    }

    /// Shares an edge segment of positive length (Mutter's adjacency).
    fn touches(&self, other: &Self) -> bool {
        let vertical_overlap =
            i64::from(self.y).max(i64::from(other.y)) < self.bottom().min(other.bottom());
        let horizontal_overlap =
            i64::from(self.x).max(i64::from(other.x)) < self.right().min(other.right());
        ((self.right() == i64::from(other.x) || other.right() == i64::from(self.x))
            && vertical_overlap)
            || ((self.bottom() == i64::from(other.y) || other.bottom() == i64::from(self.y))
                && horizontal_overlap)
    }
}

/// Why a layout cannot be applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayoutError {
    /// No output, or more than the mode serves.
    Count {
        /// Outputs requested.
        requested: usize,
        /// Most the mode serves.
        max: usize,
    },
    /// Two outputs overlap.
    Overlap(usize, usize),
    /// An output shares no edge with the rest.
    Detached(usize),
    /// The layout does not start at `(0, 0)`.
    NotAnchored {
        /// Smallest x.
        x: i32,
        /// Smallest y.
        y: i32,
    },
}

impl Display for LayoutError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count { requested, max } => {
                write!(formatter, "{requested} outputs requested, 1..={max} served")
            }
            Self::Overlap(first, second) => {
                write!(formatter, "outputs {first} and {second} overlap")
            }
            Self::Detached(index) => write!(formatter, "output {index} touches no other output"),
            Self::NotAnchored { x, y } => {
                write!(formatter, "layout starts at ({x}, {y}), not (0, 0)")
            }
        }
    }
}

impl std::error::Error for LayoutError {}

/// Checks that `outputs` is a layout the mode serves and Mutter accepts:
/// within the count bound, no overlap, every output connected to the rest
/// by shared edges, and the bounding box anchored at the origin.
///
/// # Errors
///
/// [`LayoutError`] naming the first problem.
pub fn validate_layout(
    outputs: &[PlacedOutput],
    mode: VirtualOutputMode,
) -> Result<(), LayoutError> {
    if outputs.is_empty() || outputs.len() > mode.max_outputs() {
        return Err(LayoutError::Count {
            requested: outputs.len(),
            max: mode.max_outputs(),
        });
    }
    for (first, output) in outputs.iter().enumerate() {
        for (offset, other) in outputs[first + 1..].iter().enumerate() {
            if output.overlaps(other) {
                return Err(LayoutError::Overlap(first, first + 1 + offset));
            }
        }
    }
    let mut reached = vec![false; outputs.len()];
    reached[0] = true;
    let mut frontier = vec![0];
    while let Some(index) = frontier.pop() {
        for (other, output) in outputs.iter().enumerate() {
            if !reached[other] && outputs[index].touches(output) {
                reached[other] = true;
                frontier.push(other);
            }
        }
    }
    if let Some(detached) = reached.iter().position(|reached| !reached) {
        return Err(LayoutError::Detached(detached));
    }
    let x = outputs.iter().map(|output| output.x).min().unwrap_or(0);
    let y = outputs.iter().map(|output| output.y).min().unwrap_or(0);
    if (x, y) != (0, 0) {
        return Err(LayoutError::NotAnchored { x, y });
    }
    Ok(())
}

/// Places outputs left to right along the top edge, in order.
#[must_use]
pub fn left_to_right(requests: &[VirtualOutputRequest]) -> Vec<PlacedOutput> {
    let mut x = 0_i32;
    requests
        .iter()
        .map(|request| {
            let placed = PlacedOutput {
                request: *request,
                x,
                y: 0,
                density: OutputDensity::default(),
            };
            x = x.saturating_add(i32::try_from(request.width()).unwrap_or(i32::MAX));
            placed
        })
        .collect()
}

/// What moving from the current outputs to the requested ones takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconfiguration {
    /// Nothing changes.
    Unchanged,
    /// Same count: the listed outputs change size or refresh in place
    /// (a consumer renegotiates); every output is placed again.
    InPlace {
        /// Indices whose size or refresh changes.
        resized: Vec<usize>,
        /// Whether any position changes.
        moved: bool,
    },
    /// Another count: a new set of outputs replaces the old one.
    Recreate,
}

/// Plans the move from `current` to `requested`, which must be a valid
/// layout for `mode`. A fixed-output mode refuses any change.
///
/// # Errors
///
/// [`ReconfigureError`].
pub fn plan_reconfiguration(
    current: &[PlacedOutput],
    requested: &[PlacedOutput],
    mode: VirtualOutputMode,
) -> Result<Reconfiguration, ReconfigureError> {
    validate_layout(requested, mode).map_err(ReconfigureError::Layout)?;
    if current == requested {
        return Ok(Reconfiguration::Unchanged);
    }
    if !mode.reconfigurable() {
        return Err(ReconfigureError::Fixed);
    }
    if current.len() != requested.len() {
        return Ok(Reconfiguration::Recreate);
    }
    let resized = current
        .iter()
        .zip(requested)
        .enumerate()
        .filter(|(_, (old, new))| old.request != new.request)
        .map(|(index, _)| index)
        .collect();
    let moved = current
        .iter()
        .zip(requested)
        .any(|(old, new)| (old.x, old.y) != (new.x, new.y));
    Ok(Reconfiguration::InPlace { resized, moved })
}

/// Why outputs cannot be reconfigured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReconfigureError {
    /// The requested layout is invalid.
    Layout(LayoutError),
    /// The compositor's outputs are fixed for its lifetime.
    Fixed,
    /// The requested generation is not the next one.
    Generation {
        /// Current generation.
        current: u64,
        /// Requested generation.
        requested: u64,
    },
}

impl Display for ReconfigureError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Layout(error) => error.fmt(formatter),
            Self::Fixed => formatter.write_str(
                "this compositor's virtual outputs are fixed for the session (no resize, one output)",
            ),
            Self::Generation { current, requested } => write!(
                formatter,
                "output generation {requested} requested, the next after {current} is required"
            ),
        }
    }
}

impl std::error::Error for ReconfigureError {}

/// The generation a reconfiguration must name: exactly the next one, so a
/// late or replayed request is refused.
///
/// # Errors
///
/// [`ReconfigureError::Generation`].
pub const fn check_next_generation(current: u64, requested: u64) -> Result<(), ReconfigureError> {
    if current < u64::MAX && requested == current + 1 {
        Ok(())
    } else {
        Err(ReconfigureError::Generation { current, requested })
    }
}

/// What a new attachment does with a desktop whose outputs differ from the
/// ones it asks for (another display size, layout or count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopologyReattach {
    /// The outputs already match: attach as they are.
    Reattach,
    /// Move the desktop to the requested outputs in one output transaction
    /// before the first capture epoch; the desktop and its applications
    /// stay. A refused transaction keeps the previous outputs.
    ReconfigureInPlace,
    /// The outputs are fixed for the compositor's life: only a new desktop
    /// can have them.
    Replace,
}

impl TopologyReattach {
    /// Stable log token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Reattach => "reattach",
            Self::ReconfigureInPlace => "reconfigure_in_place",
            Self::Replace => "replace",
        }
    }
}

/// Decides [`TopologyReattach`] for a desktop in `mode` whose live outputs
/// do (`matches`) or do not match the requested ones.
#[must_use]
pub const fn reattach_topology_action(mode: VirtualOutputMode, matches: bool) -> TopologyReattach {
    if matches {
        TopologyReattach::Reattach
    } else if mode.reconfigurable() {
        TopologyReattach::ReconfigureInPlace
    } else {
        TopologyReattach::Replace
    }
}

/// One logical monitor as the compositor reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedPlacement {
    /// Connector of its (single) monitor.
    pub connector: String,
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
}

/// Why the compositor's layout is not the requested one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlacementMismatch {
    /// A requested output has no logical monitor.
    Missing(String),
    /// A requested output sits elsewhere.
    Moved {
        /// Its connector.
        connector: String,
        /// Requested position.
        expected: (i32, i32),
        /// Observed position.
        observed: (i32, i32),
    },
    /// A requested output runs at another UI scale (in thousandths).
    Scaled {
        /// Its connector.
        connector: String,
        /// Requested scale, thousandths.
        expected: u32,
        /// Observed scale, thousandths.
        observed: u32,
    },
    /// A logical monitor that was not requested is enabled.
    Unexpected(String),
}

impl Display for PlacementMismatch {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(connector) => write!(formatter, "output {connector} is not placed"),
            Self::Moved {
                connector,
                expected,
                observed,
            } => write!(
                formatter,
                "output {connector} is at {observed:?}, {expected:?} was requested"
            ),
            Self::Scaled {
                connector,
                expected,
                observed,
            } => write!(
                formatter,
                "output {connector} runs at UI scale {observed}/1000, {expected}/1000 was requested"
            ),
            Self::Unexpected(connector) => {
                write!(
                    formatter,
                    "output {connector} is enabled but was not requested"
                )
            }
        }
    }
}

impl std::error::Error for PlacementMismatch {}

/// Checks the compositor's logical monitors against `requested`
/// (connector, placement) pairs: every requested output at its position and
/// nothing else enabled.
///
/// # Errors
///
/// [`PlacementMismatch`] naming the first difference.
pub fn verify_placement(
    requested: &[(String, PlacedOutput)],
    observed: &[ObservedPlacement],
) -> Result<(), PlacementMismatch> {
    for (connector, placed) in requested {
        let Some(found) = observed
            .iter()
            .find(|placement| &placement.connector == connector)
        else {
            return Err(PlacementMismatch::Missing(connector.clone()));
        };
        if (found.x, found.y) != (placed.x, placed.y) {
            return Err(PlacementMismatch::Moved {
                connector: connector.clone(),
                expected: (placed.x, placed.y),
                observed: (found.x, found.y),
            });
        }
    }
    if let Some(extra) = observed.iter().find(|placement| {
        !requested
            .iter()
            .any(|(connector, _)| *connector == placement.connector)
    }) {
        return Err(PlacementMismatch::Unexpected(extra.connector.clone()));
    }
    Ok(())
}

/// How a compositor lays out its logical monitors (Mutter's `layout-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LayoutMode {
    /// Positions are in logical pixels: a monitor at UI scale `s` spans its
    /// mode's size divided by `s` (`scale-monitor-framebuffer`).
    Logical,
    /// Positions are in physical pixels whatever the scale.
    Physical,
}

impl LayoutMode {
    /// Mutter's `layout-mode` property value (1 logical, 2 physical); a
    /// compositor that does not report one lays out physically.
    #[must_use]
    pub const fn from_mutter(value: Option<u32>) -> Self {
        match value {
            Some(1) => Self::Logical,
            _ => Self::Physical,
        }
    }
}

/// The kind of `ScreenCast` stream an absolute pointer or touch coordinate
/// is given for (`RemoteDesktop.NotifyPointerMotionAbsolute` and the touch
/// calls address a stream, not the desktop).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbsoluteInputStream {
    /// A `RecordMonitor` stream: Mutter adds the coordinate divided by the
    /// monitor's UI scale (in the logical layout mode) to the logical
    /// monitor's position.
    Monitor,
    /// A `RecordVirtual` stream: Mutter adds the coordinate, undivided, to
    /// its stage view's layout, which is in logical pixels in the logical
    /// layout mode.
    Virtual,
}

/// The coordinate to give Mutter for a position `(x, y)` in output pixels
/// on a stream of `kind` whose monitor runs at UI `scale` in a compositor
/// laid out in `layout`.
///
/// Mutter 49 to 51 map a monitor stream's coordinate through the monitor's
/// scale themselves but add a virtual stream's to the view layout as it is,
/// so in the logical layout mode (`scale-monitor-framebuffer`, fractional
/// scaling) a virtual stream needs logical pixels: output pixels over the
/// scale. Every other case takes output pixels. A scale that is not a
/// positive finite number counts as 1.
#[must_use]
pub fn absolute_stream_coordinate(
    kind: AbsoluteInputStream,
    layout: LayoutMode,
    scale: f64,
    (x, y): (f64, f64),
) -> (f64, f64) {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    match (kind, layout) {
        (AbsoluteInputStream::Virtual, LayoutMode::Logical) => (x / scale, y / scale),
        _ => (x, y),
    }
}

/// One output of a layout in desktop pixels, with its UI scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScaledOutput {
    /// Left edge in desktop pixels.
    pub x: i32,
    /// Top edge in desktop pixels.
    pub y: i32,
    /// Width in pixels (the mode's).
    pub width: u32,
    /// Height in pixels (the mode's).
    pub height: u32,
    /// UI scale.
    pub scale: f64,
}

impl ScaledOutput {
    fn logical_size(&self) -> (i64, i64) {
        let scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };
        // Compositors only offer scales that divide the mode evenly.
        #[allow(clippy::cast_possible_truncation)]
        let size = |pixels: u32| (f64::from(pixels) / scale).round() as i64;
        (size(self.width), size(self.height))
    }

    fn scaled(&self, offset: i64) -> i64 {
        let scale = if self.scale.is_finite() && self.scale > 0.0 {
            self.scale
        } else {
            1.0
        };
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let scaled = (offset as f64 / scale).round() as i64;
        scaled
    }
}

/// Why a desktop-pixel layout has no logical-mode equivalent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogicalLayoutError {
    /// Output `0` touches no other output.
    Detached(usize),
    /// The outputs' scales make the layout overlap or tear apart in logical
    /// pixels (output `0` is where it shows).
    Inconsistent(usize),
}

impl Display for LogicalLayoutError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Detached(index) => write!(formatter, "output {index} touches no other output"),
            Self::Inconsistent(index) => write!(
                formatter,
                "output {index} cannot keep its neighbours at its UI scale in a logical layout"
            ),
        }
    }
}

impl std::error::Error for LogicalLayoutError {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Right,
    Left,
    Below,
    Above,
}

/// A rectangle as (x, y, width, height).
type Rect = (i64, i64, i64, i64);

/// How `b` touches `a`: along an edge they share for some length (corners
/// alone do not count).
fn side_of(a: Rect, b: Rect) -> Option<Side> {
    let (ax, ay, aw, ah) = a;
    let (bx, by, bw, bh) = b;
    let rows = ay < by + bh && by < ay + ah;
    let columns = ax < bx + bw && bx < ax + aw;
    if rows && ax + aw == bx {
        Some(Side::Right)
    } else if rows && bx + bw == ax {
        Some(Side::Left)
    } else if columns && ay + ah == by {
        Some(Side::Below)
    } else if columns && by + bh == ay {
        Some(Side::Above)
    } else {
        None
    }
}

/// How `b` touches `a` in desktop pixels, if it does along an edge.
fn touching(a: &ScaledOutput, b: &ScaledOutput) -> Option<Side> {
    let rect = |output: &ScaledOutput| {
        (
            i64::from(output.x),
            i64::from(output.y),
            i64::from(output.width),
            i64::from(output.height),
        )
    };
    side_of(rect(a), rect(b))
}

/// Where `b` goes in logical pixels given `a` at `origin`, keeping the edge
/// they share in desktop pixels and their alignment along it.
fn logical_neighbour(
    a: &ScaledOutput,
    origin: (i64, i64),
    b: &ScaledOutput,
    side: Side,
) -> (i64, i64) {
    let (alw, alh) = a.logical_size();
    let (blw, blh) = b.logical_size();
    let along = |a_start: i32,
                 a_extent: u32,
                 b_start: i32,
                 b_extent: u32,
                 a_logical: i64,
                 b_logical: i64,
                 a_origin: i64| {
        let offset = i64::from(b_start) - i64::from(a_start);
        if offset == 0 {
            a_origin
        } else if offset + i64::from(b_extent) == i64::from(a_extent) {
            // Aligned on the far edge.
            a_origin + a_logical - b_logical
        } else {
            a_origin + a.scaled(offset)
        }
    };
    match side {
        Side::Right => (
            origin.0 + alw,
            along(a.y, a.height, b.y, b.height, alh, blh, origin.1),
        ),
        Side::Left => (
            origin.0 - blw,
            along(a.y, a.height, b.y, b.height, alh, blh, origin.1),
        ),
        Side::Below => (
            along(a.x, a.width, b.x, b.width, alw, blw, origin.0),
            origin.1 + alh,
        ),
        Side::Above => (
            along(a.x, a.width, b.x, b.width, alw, blw, origin.0),
            origin.1 - blh,
        ),
    }
}

/// The positions a layout in desktop pixels takes in a compositor's
/// logical layout mode, where an output spans its size divided by its UI
/// scale: every edge two outputs share stays shared, outputs keep their
/// alignment along it, and the layout starts at the origin. With every
/// scale 1 the positions are the desktop ones (moved to the origin).
///
/// # Errors
///
/// [`LogicalLayoutError`] when an output touches no other one, or the
/// scales make shared edges disagree or outputs overlap.
pub fn logical_positions(outputs: &[ScaledOutput]) -> Result<Vec<(i32, i32)>, LogicalLayoutError> {
    if outputs.is_empty() {
        return Ok(Vec::new());
    }
    let mut placed: Vec<Option<(i64, i64)>> = vec![None; outputs.len()];
    placed[0] = Some((0, 0));
    let mut queue = std::collections::VecDeque::from([0_usize]);
    while let Some(a) = queue.pop_front() {
        let origin = placed[a].unwrap_or_default();
        for b in 0..outputs.len() {
            if placed[b].is_some() {
                continue;
            }
            if let Some(side) = touching(&outputs[a], &outputs[b]) {
                placed[b] = Some(logical_neighbour(&outputs[a], origin, &outputs[b], side));
                queue.push_back(b);
            }
        }
    }
    let positions = placed
        .iter()
        .enumerate()
        .map(|(index, position)| position.ok_or(LogicalLayoutError::Detached(index)))
        .collect::<Result<Vec<_>, _>>()?;
    // Every edge two outputs share must still be shared, on the same side
    // and for some length (a compositor refuses outputs that only meet at a
    // corner or not at all), and nothing may overlap.
    for (a, output_a) in outputs.iter().enumerate() {
        let (alw, alh) = output_a.logical_size();
        let logical_a = (positions[a].0, positions[a].1, alw, alh);
        for (b, output_b) in outputs.iter().enumerate().skip(a + 1) {
            let (blw, blh) = output_b.logical_size();
            let logical_b = (positions[b].0, positions[b].1, blw, blh);
            if let Some(side) = touching(output_a, output_b) {
                let holds = side_of(logical_a, logical_b) == Some(side);
                if !holds {
                    return Err(LogicalLayoutError::Inconsistent(b));
                }
            }
            let (pa, pb) = (positions[a], positions[b]);
            let overlap =
                pa.0 < pb.0 + blw && pb.0 < pa.0 + alw && pa.1 < pb.1 + blh && pb.1 < pa.1 + alh;
            if overlap {
                return Err(LogicalLayoutError::Inconsistent(b));
            }
        }
    }
    let min_x = positions
        .iter()
        .map(|position| position.0)
        .min()
        .unwrap_or(0);
    let min_y = positions
        .iter()
        .map(|position| position.1)
        .min()
        .unwrap_or(0);
    positions
        .iter()
        .enumerate()
        .map(|(index, (x, y))| {
            Ok((
                i32::try_from(x - min_x).map_err(|_| LogicalLayoutError::Inconsistent(index))?,
                i32::try_from(y - min_y).map_err(|_| LogicalLayoutError::Inconsistent(index))?,
            ))
        })
        .collect()
}

#[cfg(test)]
mod logical_layout_tests {
    use super::*;

    fn output(x: i32, y: i32, width: u32, height: u32, scale: f64) -> ScaledOutput {
        ScaledOutput {
            x,
            y,
            width,
            height,
            scale,
        }
    }

    #[test]
    fn the_mode_property_selects_the_layout() {
        assert_eq!(LayoutMode::from_mutter(Some(1)), LayoutMode::Logical);
        assert_eq!(LayoutMode::from_mutter(Some(2)), LayoutMode::Physical);
        assert_eq!(LayoutMode::from_mutter(None), LayoutMode::Physical);
    }

    #[test]
    fn a_virtual_stream_in_the_logical_layout_takes_logical_pixels() {
        let point = (2000.0, 1000.0);
        assert_eq!(
            absolute_stream_coordinate(
                AbsoluteInputStream::Virtual,
                LayoutMode::Logical,
                2.0,
                point
            ),
            (1000.0, 500.0)
        );
        assert_eq!(
            absolute_stream_coordinate(
                AbsoluteInputStream::Virtual,
                LayoutMode::Logical,
                1.5,
                (1440.0, 900.0)
            ),
            (960.0, 600.0)
        );
    }

    #[test]
    fn monitor_streams_and_the_physical_layout_take_output_pixels() {
        let point = (2000.0, 1000.0);
        for (kind, layout) in [
            (AbsoluteInputStream::Monitor, LayoutMode::Logical),
            (AbsoluteInputStream::Monitor, LayoutMode::Physical),
            (AbsoluteInputStream::Virtual, LayoutMode::Physical),
        ] {
            assert_eq!(absolute_stream_coordinate(kind, layout, 2.0, point), point);
        }
    }

    #[test]
    fn an_unusable_scale_counts_as_one() {
        for scale in [0.0, -2.0, f64::NAN, f64::INFINITY] {
            assert_eq!(
                absolute_stream_coordinate(
                    AbsoluteInputStream::Virtual,
                    LayoutMode::Logical,
                    scale,
                    (10.0, 20.0)
                ),
                (10.0, 20.0)
            );
        }
    }

    #[test]
    fn unscaled_layouts_keep_their_desktop_positions() {
        let outputs = [
            output(0, 0, 1920, 1080, 1.0),
            output(1920, 0, 1920, 1080, 1.0),
            output(0, 1080, 1920, 1080, 1.0),
            output(1920, 1080, 1920, 1080, 1.0),
        ];
        assert_eq!(
            logical_positions(&outputs).unwrap(),
            vec![(0, 0), (1920, 0), (0, 1080), (1920, 1080)]
        );
    }

    #[test]
    fn a_hidpi_output_left_of_another_shrinks_to_its_logical_size() {
        // Retina 3024x1964 at 2 with a 1080p neighbour on its right.
        let outputs = [
            output(0, 0, 3024, 1964, 2.0),
            output(3024, 0, 1920, 1080, 1.0),
        ];
        assert_eq!(
            logical_positions(&outputs).unwrap(),
            vec![(0, 0), (1512, 0)]
        );
        // Primary on the right, HiDPI neighbour on its left.
        let outputs = [
            output(3840, 0, 1920, 1080, 1.0),
            output(0, 0, 3840, 2160, 2.0),
        ];
        assert_eq!(
            logical_positions(&outputs).unwrap(),
            vec![(1920, 0), (0, 0)]
        );
    }

    #[test]
    fn a_hidpi_output_above_keeps_bottom_and_centre_alignment() {
        // 4K at 2 above a 1080p display, bottom of one on top of the other.
        let outputs = [
            output(0, 2160, 1920, 1080, 1.0),
            output(0, 0, 3840, 2160, 2.0),
        ];
        assert_eq!(
            logical_positions(&outputs).unwrap(),
            vec![(0, 1080), (0, 0)]
        );
        // Bottom-aligned side by side: the smaller one's offset follows.
        let outputs = [
            output(0, 0, 2880, 1800, 1.5),
            output(2880, 720, 1920, 1080, 1.0),
        ];
        assert_eq!(
            logical_positions(&outputs).unwrap(),
            vec![(0, 0), (1920, 120)]
        );
    }

    #[test]
    fn layouts_without_a_logical_equivalent_are_refused() {
        // A 2x2 grid whose top-left is at 2: the right column cannot touch
        // both the shrunken top-left and the unscaled bottom-left.
        let outputs = [
            output(0, 0, 1920, 1080, 2.0),
            output(1920, 0, 1920, 1080, 1.0),
            output(0, 1080, 1920, 1080, 1.0),
            output(1920, 1080, 1920, 1080, 1.0),
        ];
        assert!(matches!(
            logical_positions(&outputs),
            Err(LogicalLayoutError::Inconsistent(_))
        ));
        // Touching only along a short stretch in pixels: the scale moves the
        // HiDPI output's logical rows clear of its neighbour's.
        let apart = [
            output(0, 1900, 1920, 1080, 1.0),
            output(1920, 0, 3840, 2160, 2.0),
        ];
        assert_eq!(
            logical_positions(&apart),
            Err(LogicalLayoutError::Inconsistent(1))
        );
        let detached = [
            output(0, 0, 1920, 1080, 1.0),
            output(4000, 0, 1920, 1080, 1.0),
        ];
        assert_eq!(
            logical_positions(&detached),
            Err(LogicalLayoutError::Detached(1))
        );
        assert_eq!(logical_positions(&[]).unwrap(), Vec::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(width: u32, height: u32, x: i32, y: i32) -> PlacedOutput {
        PlacedOutput {
            request: VirtualOutputRequest::new(width, height, 60).unwrap(),
            x,
            y,
            density: OutputDensity::default(),
        }
    }

    #[test]
    fn a_reattach_reconfigures_remote_monitors_and_replaces_fixed_ones() {
        use TopologyReattach::{Reattach, ReconfigureInPlace, Replace};
        for mode in [
            VirtualOutputMode::StartupMonitors,
            VirtualOutputMode::RemoteMonitors,
        ] {
            assert_eq!(reattach_topology_action(mode, true), Reattach);
        }
        assert_eq!(
            reattach_topology_action(VirtualOutputMode::RemoteMonitors, false),
            ReconfigureInPlace
        );
        assert_eq!(
            reattach_topology_action(VirtualOutputMode::StartupMonitors, false),
            Replace
        );
    }

    #[test]
    fn mode_follows_the_compositor_version() {
        assert_eq!(parse_compositor_major("GNOME Shell 49.4\n"), Some(49));
        assert_eq!(parse_compositor_major("mutter 40.9"), Some(40));
        assert_eq!(parse_compositor_major("mutter 50"), Some(50));
        assert_eq!(parse_compositor_major("unknown"), None);
        assert_eq!(
            VirtualOutputMode::for_compositor_major(Some(49)),
            VirtualOutputMode::RemoteMonitors
        );
        assert_eq!(
            VirtualOutputMode::for_compositor_major(Some(48)),
            VirtualOutputMode::StartupMonitors
        );
        assert_eq!(
            VirtualOutputMode::for_compositor_major(None),
            VirtualOutputMode::StartupMonitors
        );
        assert!(!VirtualOutputMode::StartupMonitors.reconfigurable());
        assert_eq!(VirtualOutputMode::StartupMonitors.max_outputs(), 1);
    }

    #[test]
    fn adjacent_anchored_layouts_are_valid() {
        let mode = VirtualOutputMode::RemoteMonitors;
        assert!(validate_layout(&[output(1920, 1080, 0, 0)], mode).is_ok());
        assert!(
            validate_layout(
                &[output(1920, 1080, 0, 0), output(1280, 800, 1920, 100)],
                mode
            )
            .is_ok()
        );
        assert!(
            validate_layout(
                &[output(1920, 1080, 0, 1000), output(1920, 1000, 0, 0)],
                mode
            )
            .is_ok()
        );
        let requests = [
            VirtualOutputRequest::new(1920, 1080, 60).unwrap(),
            VirtualOutputRequest::new(1024, 768, 60).unwrap(),
        ];
        let placed = left_to_right(&requests);
        assert_eq!((placed[1].x, placed[1].y), (1920, 0));
        assert!(validate_layout(&placed, mode).is_ok());
    }

    #[test]
    fn invalid_layouts_name_their_problem() {
        let mode = VirtualOutputMode::RemoteMonitors;
        assert_eq!(
            validate_layout(&[], mode),
            Err(LayoutError::Count {
                requested: 0,
                max: 4
            })
        );
        assert_eq!(
            validate_layout(
                &[output(800, 600, 0, 0), output(800, 600, 0, 0)],
                VirtualOutputMode::StartupMonitors
            ),
            Err(LayoutError::Count {
                requested: 2,
                max: 1
            })
        );
        assert_eq!(
            validate_layout(&[output(1920, 1080, 0, 0), output(800, 600, 1000, 0)], mode),
            Err(LayoutError::Overlap(0, 1))
        );
        assert_eq!(
            validate_layout(&[output(1920, 1080, 0, 0), output(800, 600, 2000, 0)], mode),
            Err(LayoutError::Detached(1))
        );
        // Corner contact is not adjacency.
        assert_eq!(
            validate_layout(&[output(100, 100, 0, 0), output(100, 100, 100, 100)], mode),
            Err(LayoutError::Detached(1))
        );
        assert_eq!(
            validate_layout(&[output(800, 600, 10, 0)], mode),
            Err(LayoutError::NotAnchored { x: 10, y: 0 })
        );
    }

    #[test]
    fn reconfiguration_is_in_place_for_the_same_count() {
        let mode = VirtualOutputMode::RemoteMonitors;
        let current = [output(1920, 1080, 0, 0)];
        assert_eq!(
            plan_reconfiguration(&current, &current, mode),
            Ok(Reconfiguration::Unchanged)
        );
        assert_eq!(
            plan_reconfiguration(&current, &[output(1600, 900, 0, 0)], mode),
            Ok(Reconfiguration::InPlace {
                resized: vec![0],
                moved: false
            })
        );
        let two = [output(1920, 1080, 0, 0), output(1280, 800, 1920, 0)];
        assert_eq!(
            plan_reconfiguration(
                &two,
                &[output(1920, 1080, 0, 0), output(1280, 800, 1920, 280)],
                mode
            ),
            Ok(Reconfiguration::InPlace {
                resized: vec![],
                moved: true
            })
        );
        assert_eq!(
            plan_reconfiguration(&current, &two, mode),
            Ok(Reconfiguration::Recreate)
        );
        assert_eq!(
            plan_reconfiguration(
                &current,
                &[output(1600, 900, 0, 0)],
                VirtualOutputMode::StartupMonitors
            ),
            Err(ReconfigureError::Fixed)
        );
        assert!(matches!(
            plan_reconfiguration(&current, &[output(1600, 900, 5, 0)], mode),
            Err(ReconfigureError::Layout(LayoutError::NotAnchored { .. }))
        ));
    }

    #[test]
    fn only_the_next_generation_is_accepted() {
        assert!(check_next_generation(1, 2).is_ok());
        assert!(check_next_generation(1, 1).is_err());
        assert!(check_next_generation(2, 1).is_err());
        assert!(check_next_generation(1, 3).is_err());
        assert!(check_next_generation(u64::MAX, 0).is_err());
    }

    #[test]
    fn placement_must_match_exactly() {
        let requested = vec![
            ("Meta-1".to_owned(), output(1920, 1080, 0, 0)),
            ("Meta-2".to_owned(), output(1024, 768, 1920, 0)),
        ];
        let observed = |second: (i32, i32)| {
            vec![
                ObservedPlacement {
                    connector: "Meta-1".into(),
                    x: 0,
                    y: 0,
                },
                ObservedPlacement {
                    connector: "Meta-2".into(),
                    x: second.0,
                    y: second.1,
                },
            ]
        };
        assert!(verify_placement(&requested, &observed((1920, 0))).is_ok());
        assert_eq!(
            verify_placement(&requested, &observed((1920, 10))),
            Err(PlacementMismatch::Moved {
                connector: "Meta-2".into(),
                expected: (1920, 0),
                observed: (1920, 10)
            })
        );
        let mut extra = observed((1920, 0));
        extra.push(ObservedPlacement {
            connector: "Meta-0".into(),
            x: 2944,
            y: 0,
        });
        assert_eq!(
            verify_placement(&requested, &extra),
            Err(PlacementMismatch::Unexpected("Meta-0".into()))
        );
        assert_eq!(
            verify_placement(&requested, &observed((1920, 0))[..1]),
            Err(PlacementMismatch::Missing("Meta-2".into()))
        );
    }
}
