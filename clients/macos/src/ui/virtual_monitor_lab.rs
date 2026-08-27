//! Developer-only virtual monitor lab (`dev-tools` feature).
//!
//! `arcen-deck virtual-monitor-lab [2|4] [--timeout-secs N]` tiles 2 or 4
//! real, decorated native macOS windows *inside one attached display* --
//! halves for 2, quadrants for 4 -- so multi-monitor window/paint/input
//! routing can be regressed on a single-display development Mac without any
//! real host, network connection, decoder, or second physical display.
//!
//! It is deliberately, entirely parallel to the production session and
//! fullscreen paths:
//!
//! * it is compiled only under the default-off `dev-tools` Cargo feature, so
//!   ordinary release builds contain none of this code and its CLI
//!   subcommand does not appear in `--help`;
//! * even when compiled in, it refuses to do anything at all unless
//!   [`ENABLE_ENV_VAR`] is set to exactly `1` (see [`lab_admission`]);
//! * it never opens fullscreen windows, never touches
//!   `crate::ui::multi_window` activation state, never constructs a
//!   `MultiWindowPlan` (which deliberately rejects two windows on the same
//!   display), and is never reachable from `crate::ui::run_native_app`.
//!
//! What it does *not* fake: the windows are genuine decorated `NSWindow`s,
//! the per-window pixels come from a real
//! [`MonitorFrameRouter`](crate::pipeline::monitor_router::MonitorFrameRouter)
//! carrying real
//! [`synthetic_frame`](crate::pipeline::synthetic_multi_monitor::synthetic_frame)
//! tags, isolation is proven by the same
//! [`verify_paint_isolation`](crate::ui::multi_window_diagnostic::verify_paint_isolation)
//! the fullscreen diagnostic uses, and every scripted input event is emitted
//! by the shared [`RegionInputEmitter`] and resolved by the shared
//! [`RegionCoordinateTransformer`] -- no lab-local wire encoding, ordered
//! state, or coordinate math exists anywhere in this module.

use std::time::Duration;

use arcen_input::{
    CoordinateTransformError, KeyboardEvent, LowLatencyMetadata, ModifierMask, PenTool,
    RegionCoordinateTransformer, RegionInputEmitError, RegionInputEmitter, RegionInputWireMessage,
    RegionLogicalPosition, RegionPenSample,
};
use arcen_media::{
    AppliedPoint, AppliedRect, AppliedRegionSet, AppliedSize, LogicalPoint, LogicalRect,
    LogicalSize, MediaContractError, OutputIdentity, PhysicalSize, RegionContractError,
    RegionGeneration, RegionId, RegionPlacement, Rotation, Scale120, SessionMonitorId,
    TopologyGeneration, TransformConvention, LOGICAL_UNITS_PER_PIXEL,
};

use crate::pipeline::monitor_router::{MonitorFrameRouter, RouterAdmissionError, RouterBuildError};
use crate::pipeline::synthetic_multi_monitor::synthetic_frame;
use crate::ui::multi_window_runtime::MonitorWindowAssignment;

/// Runtime opt-in the lab additionally requires on top of the default-off
/// `dev-tools` build feature. Only the exact value `1` enables it.
pub const ENABLE_ENV_VAR: &str = "ARCEN_ENABLE_VIRTUAL_MONITOR_LAB";

/// The only accepted [`ENABLE_ENV_VAR`] value -- not "truthy parsing", an
/// exact match, so a stray `0`/`false`/empty value can never enable a
/// developer tool by accident.
const ENABLE_ENV_VALUE: &str = "1";

/// Every virtual monitor is the same host-pixel size, so tiling math and
/// region math stay independent of the real display's actual resolution.
const VIRTUAL_MONITOR_WIDTH_PX: u32 = 1_920;
const VIRTUAL_MONITOR_HEIGHT_PX: u32 = 1_080;

/// The lab's single region/topology generation. Nothing here renegotiates.
const LAB_GENERATION: u64 = 1;

/// Deck's own production convention: the negotiated stream already arrives
/// compositor oriented (see `crate::ui::region_runtime`).
const TRANSFORM_CONVENTION: TransformConvention = TransformConvention::AlreadyCompositorOriented;

/// Smallest usable tile content area; below this the lab refuses to open
/// windows rather than producing unreadable slivers.
const MIN_TILE_INNER_WIDTH_PTS: f32 = 320.0;
const MIN_TILE_INNER_HEIGHT_PTS: f32 = 200.0;

/// Scripted trace positions are exact integer fractions of a region's
/// logical extent (`numerator / FRACTION_DENOMINATOR`), never floating point,
/// so the emitted coordinates are bit-for-bit reproducible across runs and
/// machines.
const FRACTION_DENOMINATOR: i64 = 4;

/// The gap a "device-supplied" (Wacom) pen sequence jumps ahead of the
/// emitter's own counter, proving the shared emitter adopts an external
/// device sequence and keeps allocating above it afterwards.
const WACOM_SEQUENCE_GAP: u64 = 16;

/// Nanoseconds between consecutive scripted steps. Synthetic and monotonic.
const STEP_TIMESTAMP_STRIDE_NS: u64 = 1_000_000;

/// Protocol key id the scripted keyboard step presses/releases.
const SCRIPTED_KEY_ID: u32 = 0x0004;

/// Every way the lab can refuse to run or fail while running.
#[derive(Debug)]
pub enum VirtualMonitorLabError {
    /// The `dev-tools` build is present but [`ENABLE_ENV_VAR`] was not set
    /// to exactly `1`.
    Disabled,
    /// The lab only tiles 2 (halves) or 4 (quadrants) windows.
    InvalidWindowCount(usize),
    /// No display is attached (or the active display list is unreadable).
    NoDisplay,
    /// The attached display cannot hold the requested tiling at a usable
    /// size once menu bar, Dock, gutters, and title bars are accounted for.
    DisplayTooSmall {
        window_count: usize,
        inner_width_pts: f32,
        inner_height_pts: f32,
    },
    /// The lab's own (always valid) virtual region aggregate was rejected.
    Region(RegionContractError),
    /// The lab's own (always valid) monitor identities were rejected.
    Media(MediaContractError),
    /// The lab's own (always valid) router roster was rejected.
    Router(RouterBuildError),
    /// Routing a synthetic frame into the lab's own router was rejected.
    Routing(RouterAdmissionError),
    /// The shared emitter rejected a scripted input step.
    Input(RegionInputEmitError),
    /// The shared coordinate transformer rejected an emitted position.
    Coordinate(CoordinateTransformError),
    /// `eframe` itself failed to start.
    Native(eframe::Error),
    /// Real windows are only ever opened on macOS.
    UnsupportedPlatform,
}

impl std::fmt::Display for VirtualMonitorLabError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled => write!(
                formatter,
                "the virtual monitor lab is a developer tool: set {ENABLE_ENV_VAR}={ENABLE_ENV_VALUE} to enable it"
            ),
            Self::InvalidWindowCount(count) => write!(
                formatter,
                "invalid virtual monitor window count {count}: only 2 (halves) or 4 (quadrants) are supported"
            ),
            Self::NoDisplay => formatter.write_str("no attached display to tile windows inside"),
            Self::DisplayTooSmall {
                window_count,
                inner_width_pts,
                inner_height_pts,
            } => write!(
                formatter,
                "the attached display is too small to tile {window_count} windows: each tile would be only {inner_width_pts}x{inner_height_pts} points"
            ),
            Self::Region(error) => write!(formatter, "invalid lab region aggregate: {error}"),
            Self::Media(error) => write!(formatter, "invalid lab monitor identity: {error}"),
            Self::Router(error) => write!(formatter, "invalid lab router roster: {error}"),
            Self::Routing(error) => write!(formatter, "lab frame routing rejected: {error}"),
            Self::Input(error) => write!(formatter, "scripted lab input rejected: {error}"),
            Self::Coordinate(error) => {
                write!(formatter, "scripted lab coordinate rejected: {error}")
            }
            Self::Native(error) => write!(formatter, "native virtual monitor lab failed: {error}"),
            Self::UnsupportedPlatform => {
                formatter.write_str("the virtual monitor lab only opens real windows on macOS")
            }
        }
    }
}

impl std::error::Error for VirtualMonitorLabError {}

impl From<RegionContractError> for VirtualMonitorLabError {
    fn from(value: RegionContractError) -> Self {
        Self::Region(value)
    }
}

impl From<MediaContractError> for VirtualMonitorLabError {
    fn from(value: MediaContractError) -> Self {
        Self::Media(value)
    }
}

impl From<RegionInputEmitError> for VirtualMonitorLabError {
    fn from(value: RegionInputEmitError) -> Self {
        Self::Input(value)
    }
}

impl From<CoordinateTransformError> for VirtualMonitorLabError {
    fn from(value: CoordinateTransformError) -> Self {
        Self::Coordinate(value)
    }
}

/// The complete admission decision, in the exact order the real entry point
/// applies it: the developer gate first (so an invalid window count never
/// even hints that the tool exists), then the window count.
///
/// `raw` is whatever [`ENABLE_ENV_VAR`] currently holds -- passed in rather
/// than read here so the real gate is unit-testable without ever mutating
/// this process's environment.
///
/// # Errors
///
/// [`VirtualMonitorLabError::Disabled`] unless `raw` is exactly `Some("1")`,
/// then [`VirtualMonitorLabError::InvalidWindowCount`] unless
/// `window_count` is 2 or 4.
pub fn lab_admission(raw: Option<&str>, window_count: usize) -> Result<(), VirtualMonitorLabError> {
    if raw != Some(ENABLE_ENV_VALUE) {
        return Err(VirtualMonitorLabError::Disabled);
    }
    tiling_grid(window_count)?;
    Ok(())
}

/// One real attached display's logical (point-space) rectangle, top-left
/// origin, in the same coordinate space winit/`egui` position windows in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayBoundsPts {
    pub cg_display_id: u32,
    pub origin_x: f32,
    pub origin_y: f32,
    pub width: f32,
    pub height: f32,
}

/// Space the tiling must stay out of, plus the gap between tiles and the
/// height a decorated window's own title bar consumes above its content.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilingInsets {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub gutter: f32,
    pub title_bar: f32,
}

impl TilingInsets {
    /// Leaves the menu bar clear at the top and the Dock clear at the
    /// bottom, with a visible gutter so neighbouring tiles are obviously
    /// separate windows rather than one continuous surface.
    pub const MACOS_DEFAULT: Self = Self {
        left: 24.0,
        top: 64.0,
        right: 24.0,
        bottom: 96.0,
        gutter: 24.0,
        title_bar: 28.0,
    };
}

impl Default for TilingInsets {
    fn default() -> Self {
        Self::MACOS_DEFAULT
    }
}

/// The tile arrangement for a window count: 2 -> two columns, one row
/// (halves); 4 -> two columns, two rows (quadrants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileGrid {
    pub columns: usize,
    pub rows: usize,
}

impl TileGrid {
    #[must_use]
    pub const fn window_count(self) -> usize {
        self.columns * self.rows
    }
}

/// One tile's placement: which virtual monitor it presents, where its
/// decorated window's outer frame goes, and how large its content area is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TilePlacement {
    pub session_monitor_id: SessionMonitorId,
    pub column: usize,
    pub row: usize,
    pub outer_x: f32,
    pub outer_y: f32,
    pub inner_width: f32,
    pub inner_height: f32,
}

impl TilePlacement {
    /// The full outer frame height, content plus title bar -- what the tiles
    /// must not overlap in.
    #[must_use]
    pub fn outer_height(self, insets: TilingInsets) -> f32 {
        self.inner_height + insets.title_bar
    }
}

/// The grid for `window_count`.
///
/// # Errors
///
/// [`VirtualMonitorLabError::InvalidWindowCount`] for anything but 2 or 4.
pub const fn tiling_grid(window_count: usize) -> Result<TileGrid, VirtualMonitorLabError> {
    match window_count {
        2 => Ok(TileGrid {
            columns: 2,
            rows: 1,
        }),
        4 => Ok(TileGrid {
            columns: 2,
            rows: 2,
        }),
        other => Err(VirtualMonitorLabError::InvalidWindowCount(other)),
    }
}

/// One axis's per-tile extent for `count` tiles sharing `available` points
/// with `gutter` between them. Deliberately exhaustive over the only counts
/// [`tiling_grid`] can produce (1 or 2) rather than casting a `usize` into a
/// float.
const fn axis_extent(available: f32, count: usize, gutter: f32) -> Option<f32> {
    match count {
        1 => Some(available),
        2 => Some((available - gutter) / 2.0),
        _ => None,
    }
}

/// The offset of tile `index` along an axis whose tiles are `extent` points
/// wide with `gutter` between them.
const fn axis_offset(index: usize, extent: f32, gutter: f32) -> Option<f32> {
    match index {
        0 => Some(0.0),
        1 => Some(extent + gutter),
        _ => None,
    }
}

/// Row-major tile placements (monitor 1 top-left, then across, then down)
/// for `window_count` windows inside one real display.
///
/// # Errors
///
/// [`VirtualMonitorLabError::InvalidWindowCount`] for anything but 2 or 4,
/// [`VirtualMonitorLabError::DisplayTooSmall`] when the resulting content
/// area would be unusably small, and
/// [`VirtualMonitorLabError::Media`] if a session monitor id is rejected
/// (statically impossible for `1..=4`).
pub fn tile_placements(
    window_count: usize,
    display: DisplayBoundsPts,
    insets: TilingInsets,
) -> Result<Vec<TilePlacement>, VirtualMonitorLabError> {
    let grid = tiling_grid(window_count)?;
    let available_width = display.width - insets.left - insets.right;
    let available_height = display.height - insets.top - insets.bottom;
    let (Some(cell_width), Some(cell_height)) = (
        axis_extent(available_width, grid.columns, insets.gutter),
        axis_extent(available_height, grid.rows, insets.gutter),
    ) else {
        return Err(VirtualMonitorLabError::InvalidWindowCount(window_count));
    };
    let inner_width = cell_width;
    let inner_height = cell_height - insets.title_bar;
    if !(inner_width >= MIN_TILE_INNER_WIDTH_PTS && inner_height >= MIN_TILE_INNER_HEIGHT_PTS) {
        return Err(VirtualMonitorLabError::DisplayTooSmall {
            window_count,
            inner_width_pts: inner_width,
            inner_height_pts: inner_height,
        });
    }

    let mut placements = Vec::with_capacity(window_count);
    for index in 0..window_count {
        let column = index % grid.columns;
        let row = index / grid.columns;
        let (Some(column_offset), Some(row_offset)) = (
            axis_offset(column, cell_width, insets.gutter),
            axis_offset(row, cell_height, insets.gutter),
        ) else {
            return Err(VirtualMonitorLabError::InvalidWindowCount(window_count));
        };
        let wire_id = u16::try_from(index + 1).unwrap_or(u16::MAX);
        placements.push(TilePlacement {
            session_monitor_id: SessionMonitorId::new(wire_id)?,
            column,
            row,
            outer_x: display.origin_x + insets.left + column_offset,
            outer_y: display.origin_y + insets.top + row_offset,
            inner_width,
            inner_height,
        });
    }
    Ok(placements)
}

/// The lab's session monitor identities, `1..=window_count`, in the same
/// row-major order as [`tile_placements`].
///
/// # Errors
///
/// [`VirtualMonitorLabError::InvalidWindowCount`] for anything but 2 or 4.
pub fn virtual_monitor_ids(
    window_count: usize,
) -> Result<Vec<SessionMonitorId>, VirtualMonitorLabError> {
    tiling_grid(window_count)?;
    (1..=window_count)
        .map(|index| {
            let wire_id = u16::try_from(index).unwrap_or(u16::MAX);
            SessionMonitorId::new(wire_id).map_err(VirtualMonitorLabError::Media)
        })
        .collect()
}

/// This tile's deterministic native window title -- distinct from both the
/// production/diagnostic titles (`crate::ui::multi_window_runtime::
/// window_title_for`) and the root app title, so resolving a lab window's
/// current display can never accidentally match a production window.
#[must_use]
pub fn lab_window_title(session_monitor_id: SessionMonitorId) -> String {
    format!(
        "Arcen Deck Lab — Virtual Monitor {}",
        session_monitor_id.get()
    )
}

/// The lab's virtual region aggregate: `window_count` identical
/// 1920x1080 host-pixel regions laid out row-major in applied space
/// exactly like the tiles are laid out on screen, so a region id, a tile,
/// and a virtual monitor id always mean the same thing.
///
/// Built with the shared [`arcen_media::build_region_sets`] under Deck's own
/// production [`TransformConvention`] -- the lab never hand-rolls an applied
/// region set.
///
/// # Errors
///
/// [`VirtualMonitorLabError::InvalidWindowCount`] for anything but 2 or 4,
/// or [`VirtualMonitorLabError::Region`] if the shared contract rejects the
/// (always valid) placements.
pub fn virtual_region_set(window_count: usize) -> Result<AppliedRegionSet, VirtualMonitorLabError> {
    let grid = tiling_grid(window_count)?;
    let generation = RegionGeneration::new(LAB_GENERATION)?;
    let mut placements = Vec::with_capacity(window_count);
    for index in 0..window_count {
        let column = index % grid.columns;
        let row = index / grid.columns;
        let origin_x = i64::try_from(column).unwrap_or(0) * i64::from(VIRTUAL_MONITOR_WIDTH_PX);
        let origin_y = i64::try_from(row).unwrap_or(0) * i64::from(VIRTUAL_MONITOR_HEIGHT_PX);
        let region_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
        placements.push(RegionPlacement {
            region_id: RegionId::new(region_number)?,
            output: OutputIdentity::new(&format!("arcen-lab-virtual-{region_number}"))?,
            logical_rect: LogicalRect::new(
                LogicalPoint::from_pixels(origin_x, origin_y)?,
                LogicalSize::from_pixels(
                    u64::from(VIRTUAL_MONITOR_WIDTH_PX),
                    u64::from(VIRTUAL_MONITOR_HEIGHT_PX),
                )?,
            )?,
            stream_size: PhysicalSize::new(VIRTUAL_MONITOR_WIDTH_PX, VIRTUAL_MONITOR_HEIGHT_PX)?,
            scale: Scale120::new(120)?,
            rotation: Rotation::Degrees0,
            primary: index == 0,
            applied_rect: AppliedRect::new(
                AppliedPoint::new(origin_x, origin_y),
                AppliedSize::new(VIRTUAL_MONITOR_WIDTH_PX, VIRTUAL_MONITOR_HEIGHT_PX)?,
            )?,
        });
    }
    let (_requested, applied) =
        arcen_media::build_region_sets(generation, TRANSFORM_CONVENTION, &placements)?;
    Ok(applied)
}

/// The region id presenting virtual monitor `index` (0-based, row-major).
///
/// # Errors
///
/// [`VirtualMonitorLabError::Region`] if the id is rejected (statically
/// impossible for the indices this module produces).
fn region_id_for_index(index: usize) -> Result<RegionId, VirtualMonitorLabError> {
    let region_number = u32::try_from(index + 1).unwrap_or(u32::MAX);
    Ok(RegionId::new(region_number)?)
}

/// An exact integer fraction (`numerator / FRACTION_DENOMINATOR`) of a
/// region's logical extent, rounded half-up, with no floating point
/// anywhere -- the same coordinate is produced on every machine and run.
const fn fraction_logical(numerator: i64, extent_px: u32) -> i64 {
    let maximum = extent_px as i64 * LOGICAL_UNITS_PER_PIXEL - 1;
    (maximum * numerator + FRACTION_DENOMINATOR / 2) / FRACTION_DENOMINATOR
}

/// One step of the deterministic scripted input trace. Every variant is
/// dispatched through the *shared* [`RegionInputEmitter`]; nothing here
/// encodes wire messages or tracks ordered state on its own.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScriptedInputStep {
    /// Move the pointer to `numerator/4` of the region's extent, producing
    /// whatever leave/enter/motion transitions the shared emitter derives.
    PointerMotion {
        monitor_index: usize,
        x_numerator: i64,
        y_numerator: i64,
    },
    PointerButton {
        monitor_index: usize,
        x_numerator: i64,
        y_numerator: i64,
        button: u8,
        pressed: bool,
    },
    PointerScroll {
        monitor_index: usize,
        x_numerator: i64,
        y_numerator: i64,
        ticks_x: i32,
        ticks_y: i32,
    },
    /// A non-region keyboard edge allocated from the same session-global
    /// sequence counter the region emitter allocates from, proving the two
    /// genuinely interleave (`RegionInputEmitter::advance_sequence_to`).
    Keyboard {
        key_id: u32,
        pressed: bool,
        modifiers: u32,
    },
    /// A pen/stylus sample using the emitter's own next sequence.
    Pen {
        monitor_index: usize,
        x_numerator: i64,
        y_numerator: i64,
        tool: PenTool,
        touching: bool,
    },
    /// A Wacom-style sample carrying a *device-supplied* sequence ahead of
    /// the emitter's counter (`RegionInputEmitter::pen_with_sequence`).
    WacomPen {
        monitor_index: usize,
        x_numerator: i64,
        y_numerator: i64,
    },
}

/// The full scripted trace for `window_count` virtual monitors: every event
/// kind, on every monitor, in a fixed order, ending with a motion back to
/// the first monitor so a cross-monitor leave/enter pair is always proven.
///
/// # Errors
///
/// [`VirtualMonitorLabError::InvalidWindowCount`] for anything but 2 or 4.
pub fn scripted_trace(
    window_count: usize,
) -> Result<Vec<ScriptedInputStep>, VirtualMonitorLabError> {
    tiling_grid(window_count)?;
    let mut steps = Vec::new();
    for monitor_index in 0..window_count {
        steps.push(ScriptedInputStep::PointerMotion {
            monitor_index,
            x_numerator: 1,
            y_numerator: 1,
        });
        steps.push(ScriptedInputStep::PointerMotion {
            monitor_index,
            x_numerator: 2,
            y_numerator: 2,
        });
        steps.push(ScriptedInputStep::PointerButton {
            monitor_index,
            x_numerator: 2,
            y_numerator: 2,
            button: 1,
            pressed: true,
        });
        steps.push(ScriptedInputStep::PointerButton {
            monitor_index,
            x_numerator: 2,
            y_numerator: 2,
            button: 1,
            pressed: false,
        });
        steps.push(ScriptedInputStep::PointerScroll {
            monitor_index,
            x_numerator: 2,
            y_numerator: 2,
            ticks_x: 0,
            ticks_y: -1,
        });
        steps.push(ScriptedInputStep::Keyboard {
            key_id: SCRIPTED_KEY_ID,
            pressed: true,
            modifiers: ModifierMask::SHIFT,
        });
        steps.push(ScriptedInputStep::Keyboard {
            key_id: SCRIPTED_KEY_ID,
            pressed: false,
            modifiers: 0,
        });
        steps.push(ScriptedInputStep::Pen {
            monitor_index,
            x_numerator: 3,
            y_numerator: 1,
            tool: PenTool::Tip,
            touching: true,
        });
        steps.push(ScriptedInputStep::WacomPen {
            monitor_index,
            x_numerator: 3,
            y_numerator: 3,
        });
    }
    steps.push(ScriptedInputStep::PointerMotion {
        monitor_index: 0,
        x_numerator: 3,
        y_numerator: 3,
    });
    Ok(steps)
}

/// What one traced input actually carried, after the shared emitter encoded
/// it and the shared transformer resolved its applied host pixel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TracedInput {
    Region {
        input_type: &'static str,
        region_id: u32,
        logical_x: i64,
        logical_y: i64,
        applied_x: i64,
        applied_y: i64,
        button: Option<(u8, bool)>,
    },
    Keyboard {
        key_id: u32,
        pressed: bool,
        modifiers: u32,
    },
}

/// One recorded trace entry: exactly what the shared emitter produced, plus
/// the applied host coordinate the shared transformer resolved for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputTraceRecord {
    pub step_index: usize,
    pub sequence: u64,
    pub timestamp_ns: u64,
    pub input: TracedInput,
    /// Which region held pointer focus in the shared ordered state right
    /// after this step was applied.
    pub focused_region_id: Option<u32>,
}

impl std::fmt::Display for InputTraceRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.input {
            TracedInput::Region {
                input_type,
                region_id,
                logical_x,
                logical_y,
                applied_x,
                applied_y,
                button,
            } => {
                write!(
                    formatter,
                    "#{step:03} seq={sequence} {input_type} region={region_id} logical=({logical_x},{logical_y}) applied=({applied_x},{applied_y})",
                    step = self.step_index,
                    sequence = self.sequence,
                )?;
                if let Some((button, pressed)) = button {
                    write!(formatter, " button={button}:{pressed}")?;
                }
                Ok(())
            }
            TracedInput::Keyboard {
                key_id,
                pressed,
                modifiers,
            } => write!(
                formatter,
                "#{step:03} seq={sequence} keyboard key={key_id:#06x} pressed={pressed} modifiers={modifiers:#04x}",
                step = self.step_index,
                sequence = self.sequence,
            ),
        }
    }
}

/// The region-local logical position at `numerator/4` of virtual monitor
/// `monitor_index`'s extent.
fn lab_position(
    monitor_index: usize,
    x_numerator: i64,
    y_numerator: i64,
) -> Result<RegionLogicalPosition, VirtualMonitorLabError> {
    Ok(RegionLogicalPosition {
        region_id: region_id_for_index(monitor_index)?,
        point: LogicalPoint::new(
            fraction_logical(x_numerator, VIRTUAL_MONITOR_WIDTH_PX),
            fraction_logical(y_numerator, VIRTUAL_MONITOR_HEIGHT_PX),
        ),
    })
}

/// A fully populated pen sample; the lab always reports proximity so the
/// shared ordered state keeps a live pen rather than immediately clearing it.
const fn lab_pen_sample(
    position: RegionLogicalPosition,
    tool: PenTool,
    touching: bool,
) -> RegionPenSample {
    RegionPenSample {
        position,
        pressure: 0.5,
        tilt_x_degrees: 6.0,
        tilt_y_degrees: -6.0,
        rotation_degrees: 15.0,
        tool,
        in_proximity: true,
        touching,
        buttons: 0,
    }
}

/// Turns one shared-emitter wire message into a trace record, resolving the
/// applied host pixel through the shared [`RegionCoordinateTransformer`] --
/// never through any lab-local coordinate math.
fn record_from_message(
    transformer: RegionCoordinateTransformer<'_>,
    step_index: usize,
    message: &RegionInputWireMessage,
    focused_region_id: Option<u32>,
) -> Result<InputTraceRecord, VirtualMonitorLabError> {
    let position = message.position();
    let metadata = message.metadata();
    let region_id = RegionId::new(position.region_id)?;
    let applied = transformer.logical_to_applied(
        region_id,
        LogicalPoint::new(position.logical_x, position.logical_y),
    )?;
    Ok(InputTraceRecord {
        step_index,
        sequence: metadata.sequence,
        timestamp_ns: metadata.timestamp_ns,
        input: TracedInput::Region {
            input_type: message.input_type(),
            region_id: position.region_id,
            logical_x: position.logical_x,
            logical_y: position.logical_y,
            applied_x: applied.x,
            applied_y: applied.y,
            button: message.button_state(),
        },
        focused_region_id,
    })
}

/// Runs the scripted trace through a real shared [`RegionInputEmitter`] over
/// `regions`, recording every wire message the emitter derived along with
/// the applied host coordinate the shared transformer resolved for it.
///
/// Pure: no windows, no environment, no clock. The same `regions`/`steps`
/// always produce byte-identical records.
///
/// # Errors
///
/// [`VirtualMonitorLabError::Input`] if the shared emitter rejects a step,
/// [`VirtualMonitorLabError::Coordinate`] if the shared transformer rejects
/// an emitted position, or [`VirtualMonitorLabError::Region`] for an invalid
/// region id.
pub fn run_input_trace(
    regions: &AppliedRegionSet,
    steps: &[ScriptedInputStep],
) -> Result<Vec<InputTraceRecord>, VirtualMonitorLabError> {
    let transformer = RegionCoordinateTransformer::new(regions);
    let mut emitter = RegionInputEmitter::new();
    let mut records = Vec::new();
    for (step_index, step) in steps.iter().enumerate() {
        let timestamp_ns = (u64::try_from(step_index).unwrap_or(0) + 1) * STEP_TIMESTAMP_STRIDE_NS;
        let messages = match *step {
            ScriptedInputStep::PointerMotion {
                monitor_index,
                x_numerator,
                y_numerator,
            } => {
                let position = lab_position(monitor_index, x_numerator, y_numerator)?;
                emitter.pointer_motion(regions, position, timestamp_ns)?
            }
            ScriptedInputStep::PointerButton {
                monitor_index,
                x_numerator,
                y_numerator,
                button,
                pressed,
            } => {
                let position = lab_position(monitor_index, x_numerator, y_numerator)?;
                emitter.pointer_button(regions, position, button, pressed, timestamp_ns)?
            }
            ScriptedInputStep::PointerScroll {
                monitor_index,
                x_numerator,
                y_numerator,
                ticks_x,
                ticks_y,
            } => {
                let position = lab_position(monitor_index, x_numerator, y_numerator)?;
                emitter.pointer_scroll(
                    regions,
                    position,
                    i64::from(ticks_x) * LOGICAL_UNITS_PER_PIXEL,
                    i64::from(ticks_y) * LOGICAL_UNITS_PER_PIXEL,
                    timestamp_ns,
                )?
            }
            ScriptedInputStep::Keyboard {
                key_id,
                pressed,
                modifiers,
            } => {
                // Non-region input allocates from the same session-global
                // counter, then hands the floor back to the shared emitter,
                // exactly like a real client that interleaves keyboard and
                // region input on one stream.
                let sequence = emitter.sequence() + 1;
                let event = KeyboardEvent {
                    key_id,
                    pressed,
                    modifiers: ModifierMask(modifiers),
                    caps_lock_on: Some(false),
                    num_lock_on: Some(false),
                    scroll_lock_on: Some(false),
                    metadata: LowLatencyMetadata {
                        sequence,
                        timestamp_ns,
                        coalescable: false,
                    },
                };
                emitter.advance_sequence_to(event.metadata.sequence);
                records.push(InputTraceRecord {
                    step_index,
                    sequence: event.metadata.sequence,
                    timestamp_ns: event.metadata.timestamp_ns,
                    input: TracedInput::Keyboard {
                        key_id: event.key_id,
                        pressed: event.pressed,
                        modifiers: event.modifiers.0,
                    },
                    focused_region_id: emitter.state().active_pointer_region().map(RegionId::get),
                });
                continue;
            }
            ScriptedInputStep::Pen {
                monitor_index,
                x_numerator,
                y_numerator,
                tool,
                touching,
            } => {
                let position = lab_position(monitor_index, x_numerator, y_numerator)?;
                vec![emitter.pen(
                    regions,
                    lab_pen_sample(position, tool, touching),
                    timestamp_ns,
                    false,
                )?]
            }
            ScriptedInputStep::WacomPen {
                monitor_index,
                x_numerator,
                y_numerator,
            } => {
                let position = lab_position(monitor_index, x_numerator, y_numerator)?;
                let device_sequence = emitter.sequence() + WACOM_SEQUENCE_GAP;
                vec![emitter.pen_with_sequence(
                    regions,
                    lab_pen_sample(position, PenTool::Tip, true),
                    device_sequence,
                    timestamp_ns,
                    true,
                )?]
            }
        };
        let focused_region_id = emitter.state().active_pointer_region().map(RegionId::get);
        for message in &messages {
            records.push(record_from_message(
                transformer,
                step_index,
                message,
                focused_region_id,
            )?);
        }
    }
    Ok(records)
}

/// What one tile's own paint callback observed about its decorated window
/// this frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileWindowObservation {
    pub inner_rect_known: bool,
    pub fullscreen: Option<bool>,
    pub close_requested: bool,
    /// The `CGDirectDisplayID` this exact window currently reports, resolved
    /// by title through the shared
    /// [`window_display_id`](crate::ui::multi_window_runtime::window_display_id)
    /// lookup -- `None` when it cannot be resolved (never a guess).
    pub observed_display_id: Option<u32>,
}

/// Whether a tile's decorated window is genuinely placed on the one display
/// the lab targeted.
///
/// Deliberately *not*
/// [`ViewportBindEvaluation`](crate::ui::multi_window_runtime::ViewportBindEvaluation):
/// that production evaluation requires `fullscreen == Some(true)`, which is
/// exactly what a windowed tile must never be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileBindEvaluation {
    /// The operator closed this window; the lab treats that as "end the run".
    Closed,
    /// The window went fullscreen, which the lab never asks for -- a windowed
    /// tile that fullscreens has stopped being a tiling test.
    UnexpectedFullscreen,
    /// Not enough is known yet (no inner rect, or the display id has not
    /// resolved). Keep waiting.
    Waiting,
    /// Genuinely windowed on the expected display.
    Bound,
    /// Windowed, but on a different display than the lab targeted.
    WrongDisplay { expected: u32, observed: u32 },
}

/// Evaluates one tile window observation against the display the lab
/// targeted, in strict precedence order: closed, unexpected fullscreen,
/// still-unknown, wrong display, bound.
#[must_use]
pub const fn evaluate_tile_bind(
    observation: TileWindowObservation,
    expected_display_id: u32,
) -> TileBindEvaluation {
    if observation.close_requested {
        return TileBindEvaluation::Closed;
    }
    if matches!(observation.fullscreen, Some(true)) {
        return TileBindEvaluation::UnexpectedFullscreen;
    }
    if !observation.inner_rect_known {
        return TileBindEvaluation::Waiting;
    }
    match observation.observed_display_id {
        None => TileBindEvaluation::Waiting,
        Some(observed) if observed == expected_display_id => TileBindEvaluation::Bound,
        Some(observed) => TileBindEvaluation::WrongDisplay {
            expected: expected_display_id,
            observed,
        },
    }
}

/// The `(viewport_id, session_monitor_id)` mapping the lab's paint
/// acknowledgements are checked against: the first virtual monitor is the
/// root viewport, every later one gets the same deterministic viewport id
/// production windows use
/// ([`MonitorWindowAssignment::viewport_id_for`]).
#[must_use]
pub fn expected_paint_plan(
    monitor_ids: &[SessionMonitorId],
) -> Vec<(egui::ViewportId, SessionMonitorId)> {
    monitor_ids
        .iter()
        .enumerate()
        .map(|(index, &monitor_id)| {
            let viewport_id = if index == 0 {
                egui::ViewportId::ROOT
            } else {
                MonitorWindowAssignment::viewport_id_for(monitor_id)
            };
            (viewport_id, monitor_id)
        })
        .collect()
}

/// Builds the lab's real [`MonitorFrameRouter`] and seeds every virtual
/// monitor's slot with that monitor's own deterministic synthetic frame, so
/// each tile paints from a genuinely routed frame rather than a color the
/// lab handed it directly.
///
/// # Errors
///
/// [`VirtualMonitorLabError::Media`] for an invalid generation,
/// [`VirtualMonitorLabError::Router`] if the roster is rejected, or
/// [`VirtualMonitorLabError::Routing`] if a frame is rejected.
pub fn build_lab_router(
    monitor_ids: &[SessionMonitorId],
) -> Result<(TopologyGeneration, MonitorFrameRouter), VirtualMonitorLabError> {
    let generation = TopologyGeneration::new(LAB_GENERATION)?;
    let mut router =
        MonitorFrameRouter::new(generation, monitor_ids).map_err(VirtualMonitorLabError::Router)?;
    for &monitor_id in monitor_ids {
        router
            .route_decoded_frame(generation, monitor_id, synthetic_frame(monitor_id, 0))
            .map_err(VirtualMonitorLabError::Routing)?;
    }
    Ok((generation, router))
}

/// Outcome of one `virtual-monitor-lab` run.
#[derive(Debug, Clone, PartialEq)]
pub struct VirtualMonitorLabReport {
    pub window_count: usize,
    /// The single real display every tile was placed inside.
    pub cg_display_id: u32,
    pub tiles: Vec<TilePlacement>,
    /// Virtual monitor ids whose decorated window was observed genuinely
    /// windowed on `cg_display_id` (see [`evaluate_tile_bind`]).
    pub bound_session_monitor_ids: Vec<u16>,
    pub unbound_session_monitor_ids: Vec<u16>,
    /// Every tile's own paint callback claimed exactly its assigned virtual
    /// monitor and painted exactly that monitor's routed tag, with no two
    /// tiles painting the same tag -- proven by the same
    /// [`verify_paint_isolation`](crate::ui::multi_window_diagnostic::verify_paint_isolation)
    /// the fullscreen diagnostic uses.
    pub isolation_verified: bool,
    /// The deterministic scripted input trace, recorded before any window
    /// opened.
    pub trace: Vec<InputTraceRecord>,
}

impl VirtualMonitorLabReport {
    /// Every tile bound on the targeted display and isolation held.
    #[must_use]
    pub fn fully_verified(&self) -> bool {
        self.unbound_session_monitor_ids.is_empty() && self.isolation_verified
    }

    /// The scripted trace, one record per line.
    #[must_use]
    pub fn format_trace(&self) -> String {
        self.trace
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The tiling, one line per window, in the same row-major order as
    /// [`tile_placements`].
    #[must_use]
    pub fn format_tiles(&self) -> String {
        self.tiles
            .iter()
            .map(|tile| {
                format!(
                    "monitor={id} cell=({column},{row}) outer=({x},{y}) inner={width}x{height}",
                    id = tile.session_monitor_id.get(),
                    column = tile.column,
                    row = tile.row,
                    x = tile.outer_x,
                    y = tile.outer_y,
                    width = tile.inner_width,
                    height = tile.inner_height,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl std::fmt::Display for VirtualMonitorLabReport {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "windows={} display={} bound={:?} unbound={:?} isolation_verified={} traced_events={}",
            self.window_count,
            self.cg_display_id,
            self.bound_session_monitor_ids,
            self.unbound_session_monitor_ids,
            self.isolation_verified,
            self.trace.len()
        )
    }
}

/// Runs the developer-only virtual monitor lab: tiles `window_count` (2 or
/// 4) decorated native windows inside the first attached display, paints
/// each one from a real routed synthetic frame, and reports the scripted
/// shared-emitter input trace recorded before any window opened.
///
/// Refuses to do anything -- including reading the display list -- unless
/// [`ENABLE_ENV_VAR`] is exactly `1`.
///
/// # Errors
///
/// [`VirtualMonitorLabError::Disabled`] without the runtime opt-in,
/// [`VirtualMonitorLabError::InvalidWindowCount`] for anything but 2 or 4,
/// [`VirtualMonitorLabError::NoDisplay`] with no attached display,
/// [`VirtualMonitorLabError::DisplayTooSmall`] when the tiles would be
/// unusable, the region/router/input variants if the lab's own (always
/// valid) fixtures are rejected, [`VirtualMonitorLabError::Native`] if
/// `eframe` fails to start, and
/// [`VirtualMonitorLabError::UnsupportedPlatform`] outside macOS.
pub fn run_virtual_monitor_lab(
    window_count: usize,
    timeout: Duration,
) -> Result<VirtualMonitorLabReport, VirtualMonitorLabError> {
    let raw = std::env::var(ENABLE_ENV_VAR).ok();
    lab_admission(raw.as_deref(), window_count)?;

    #[cfg(target_os = "macos")]
    {
        macos::run(window_count, timeout)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = timeout;
        Err(VirtualMonitorLabError::UnsupportedPlatform)
    }
}

#[cfg(target_os = "macos")]
mod macos {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use arcen_media::SessionMonitorId;

    use crate::pipeline::monitor_router::MonitorFrameRouter;
    use crate::pipeline::video_decoder::DecodedVideoFrame;
    use crate::ui::multi_window_diagnostic::{verify_paint_isolation, ViewportPaintAck};
    use crate::ui::multi_window_runtime::{
        live_active_displays, window_display_id, MonitorWindowAssignment,
    };

    use super::{
        build_lab_router, evaluate_tile_bind, expected_paint_plan, lab_window_title,
        run_input_trace, scripted_trace, tile_placements, virtual_monitor_ids, virtual_region_set,
        DisplayBoundsPts, InputTraceRecord, TileBindEvaluation, TilePlacement,
        TileWindowObservation, TilingInsets, TracedInput, VirtualMonitorLabError,
        VirtualMonitorLabReport,
    };

    /// macOS may place a freshly opened window itself before honouring the
    /// requested position, so every tile re-asserts its exact outer position
    /// and inner size on this many leading frames. Bounded, then it stops --
    /// the operator stays free to drag or resize a tile afterwards.
    const POSITION_REASSERT_FRAMES: u64 = 5;

    /// At most this many trace lines are drawn inside a tile; the full trace
    /// always goes to stdout through the report.
    const TILE_TRACE_LINES: usize = 12;

    fn color_for_frame(frame: Option<&DecodedVideoFrame>) -> egui::Color32 {
        match frame {
            Some(frame) if frame.rgba.len() >= 4 => {
                egui::Color32::from_rgb(frame.rgba[0], frame.rgba[1], frame.rgba[2])
            }
            // Unrouted is a bug in the lab's own setup (every monitor is
            // seeded before the app runs); paint it a color no
            // `monitor_pixel_tag` output can produce so it is obvious.
            _ => egui::Color32::from_rgb(0, 255, 0),
        }
    }

    fn tile_observation(
        info: &egui::ViewportInfo,
        observed_display_id: Option<u32>,
    ) -> TileWindowObservation {
        TileWindowObservation {
            inner_rect_known: info.inner_rect.is_some(),
            fullscreen: info.fullscreen,
            close_requested: info
                .events
                .iter()
                .any(|event| matches!(event, egui::ViewportEvent::Close)),
            observed_display_id,
        }
    }

    /// The first attached display's identity, origin, and logical size.
    /// Reuses the shared active-display helper for identity and size, and
    /// only reads the origin (which that helper does not expose) directly.
    fn lab_display_bounds() -> Option<DisplayBoundsPts> {
        let info = live_active_displays().into_iter().next()?;
        let bounds = core_graphics::display::CGDisplay::new(info.cg_display_id).bounds();
        Some(DisplayBoundsPts {
            cg_display_id: info.cg_display_id,
            origin_x: bounds.origin.x as f32,
            origin_y: bounds.origin.y as f32,
            width: info.width_pts,
            height: info.height_pts,
        })
    }

    fn tile_viewport_builder(tile: &TilePlacement) -> egui::ViewportBuilder {
        // Never `with_monitor`: that takes precedence over an explicit
        // position, and the whole point of the lab is placing several
        // windows at explicit positions *inside one* display.
        egui::ViewportBuilder::default()
            .with_title(lab_window_title(tile.session_monitor_id))
            .with_decorations(true)
            .with_resizable(true)
            .with_inner_size([tile.inner_width, tile.inner_height])
            .with_position([tile.outer_x, tile.outer_y])
    }

    fn reassert_position(ctx: &egui::Context, tile: &TilePlacement, viewport_id: egui::ViewportId) {
        let position = egui::pos2(tile.outer_x, tile.outer_y);
        let size = egui::vec2(tile.inner_width, tile.inner_height);
        if viewport_id == egui::ViewportId::ROOT {
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(position));
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
        } else {
            ctx.send_viewport_cmd_to(viewport_id, egui::ViewportCommand::OuterPosition(position));
            ctx.send_viewport_cmd_to(viewport_id, egui::ViewportCommand::InnerSize(size));
        }
    }

    fn paint_tile_body(ui: &mut egui::Ui, color: egui::Color32, heading: &str, lines: &[String]) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, color);
        let mut cursor = rect.left_top() + egui::vec2(16.0, 14.0);
        ui.painter().text(
            cursor,
            egui::Align2::LEFT_TOP,
            heading,
            egui::FontId::proportional(20.0),
            egui::Color32::WHITE,
        );
        cursor.y += 30.0;
        for line in lines {
            ui.painter().text(
                cursor,
                egui::Align2::LEFT_TOP,
                line,
                egui::FontId::monospace(11.0),
                egui::Color32::WHITE,
            );
            cursor.y += 15.0;
        }
    }

    /// The scripted trace lines that belong to one virtual monitor's own
    /// region, so each tile shows the coordinates that were routed to *it*.
    fn tile_trace_lines(trace: &[InputTraceRecord], region_id: u32) -> Vec<String> {
        trace
            .iter()
            .filter(|record| match &record.input {
                TracedInput::Region { region_id: id, .. } => *id == region_id,
                TracedInput::Keyboard { .. } => record.focused_region_id == Some(region_id),
            })
            .take(TILE_TRACE_LINES)
            .map(ToString::to_string)
            .collect()
    }

    struct LabApp {
        monitor_ids: Vec<SessionMonitorId>,
        tiles: Vec<TilePlacement>,
        router: MonitorFrameRouter,
        trace: Vec<InputTraceRecord>,
        expected_display_id: u32,
        started_at: Instant,
        timeout: Duration,
        frame_index: u64,
        paint_acks: Vec<ViewportPaintAck>,
        bind_states: Vec<TileBindEvaluation>,
        finished: bool,
        report: Arc<Mutex<Option<VirtualMonitorLabReport>>>,
    }

    impl LabApp {
        fn record_paint_ack(&mut self, ack: ViewportPaintAck) {
            self.paint_acks
                .retain(|existing| existing.viewport_id != ack.viewport_id);
            self.paint_acks.push(ack);
        }

        fn heading_for(&self, index: usize, tag: u8) -> String {
            format!(
                "Virtual Monitor {id}  ·  tile {column},{row}  ·  region {id}  ·  tag {tag:#04x}",
                id = self.monitor_ids[index].get(),
                column = self.tiles[index].column,
                row = self.tiles[index].row,
            )
        }

        fn finish(&mut self, ctx: &egui::Context) {
            self.finished = true;
            let expected = expected_paint_plan(&self.monitor_ids);
            let isolation_verified = verify_paint_isolation(&expected, &self.paint_acks).is_ok();
            let mut bound = Vec::new();
            let mut unbound = Vec::new();
            for (index, &monitor_id) in self.monitor_ids.iter().enumerate() {
                if matches!(self.bind_states[index], TileBindEvaluation::Bound) {
                    bound.push(monitor_id.get());
                } else {
                    unbound.push(monitor_id.get());
                }
            }
            let report = VirtualMonitorLabReport {
                window_count: self.monitor_ids.len(),
                cg_display_id: self.expected_display_id,
                tiles: self.tiles.clone(),
                bound_session_monitor_ids: bound,
                unbound_session_monitor_ids: unbound,
                isolation_verified,
                trace: self.trace.clone(),
            };
            match self.report.lock() {
                Ok(mut slot) => *slot = Some(report),
                Err(poisoned) => *poisoned.into_inner() = Some(report),
            }
            for index in 1..self.monitor_ids.len() {
                let viewport_id = MonitorWindowAssignment::viewport_id_for(self.monitor_ids[index]);
                ctx.send_viewport_cmd_to(viewport_id, egui::ViewportCommand::Close);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    impl eframe::App for LabApp {
        fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
            if self.finished {
                return;
            }
            self.frame_index += 1;
            if self.frame_index <= POSITION_REASSERT_FRAMES {
                reassert_position(ctx, &self.tiles[0], egui::ViewportId::ROOT);
            }

            for index in 1..self.monitor_ids.len() {
                let monitor_id = self.monitor_ids[index];
                let tile = self.tiles[index];
                let viewport_id = MonitorWindowAssignment::viewport_id_for(monitor_id);
                // Snapshot exactly the frame this monitor id currently routes
                // to *before* opening the viewport and move it into the
                // closure, so the closure's paint and its acknowledgement both
                // come from the identical routed lookup.
                let frame = self.router.latest_frame(monitor_id).cloned();
                let color = color_for_frame(frame.as_ref());
                let tag = frame
                    .as_ref()
                    .and_then(|frame| frame.rgba.first().copied())
                    .unwrap_or(0xFF);
                let heading = self.heading_for(index, tag);
                let lines = tile_trace_lines(&self.trace, u32::from(monitor_id.get()));
                let title = lab_window_title(monitor_id);
                let builder = tile_viewport_builder(&tile);
                let (observation, ack) =
                    ctx.show_viewport_immediate(viewport_id, builder, move |ui, _class| {
                        paint_tile_body(ui, color, &heading, &lines);
                        let observed_display_id = window_display_id(&title);
                        let observation = tile_observation(
                            &ui.ctx().input(|state| state.viewport().clone()),
                            observed_display_id,
                        );
                        // Acknowledged from *inside* this exact callback: the
                        // monitor it believes it presents plus the tag byte it
                        // actually painted from.
                        let ack = ViewportPaintAck {
                            viewport_id,
                            claimed_monitor_id: monitor_id,
                            painted_tag: tag,
                        };
                        (observation, ack)
                    });
                if self.frame_index <= POSITION_REASSERT_FRAMES {
                    reassert_position(ctx, &tile, viewport_id);
                }
                self.bind_states[index] = evaluate_tile_bind(observation, self.expected_display_id);
                self.record_paint_ack(ack);
            }
        }

        fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
            let root_monitor_id = self.monitor_ids[0];
            let root_frame = self.router.latest_frame(root_monitor_id).cloned();
            let root_color = color_for_frame(root_frame.as_ref());
            let root_tag = root_frame
                .as_ref()
                .and_then(|frame| frame.rgba.first().copied())
                .unwrap_or(0xFF);
            let heading = self.heading_for(0, root_tag);
            let lines = tile_trace_lines(&self.trace, u32::from(root_monitor_id.get()));
            egui::CentralPanel::default().show(ui, |panel_ui| {
                paint_tile_body(panel_ui, root_color, &heading, &lines);
            });

            if self.finished {
                return;
            }

            let observed_display_id = window_display_id(&lab_window_title(root_monitor_id));
            let observation = tile_observation(
                &ui.ctx().input(|state| state.viewport().clone()),
                observed_display_id,
            );
            self.bind_states[0] = evaluate_tile_bind(observation, self.expected_display_id);
            self.record_paint_ack(ViewportPaintAck {
                viewport_id: egui::ViewportId::ROOT,
                claimed_monitor_id: root_monitor_id,
                painted_tag: root_tag,
            });

            let closed = self
                .bind_states
                .iter()
                .any(|state| matches!(state, TileBindEvaluation::Closed));
            if closed || self.started_at.elapsed() >= self.timeout {
                let ctx = ui.ctx().clone();
                self.finish(&ctx);
            } else {
                ui.ctx().request_repaint();
            }
        }
    }

    pub(super) fn run(
        window_count: usize,
        timeout: Duration,
    ) -> Result<VirtualMonitorLabReport, VirtualMonitorLabError> {
        let display = lab_display_bounds().ok_or(VirtualMonitorLabError::NoDisplay)?;
        let tiles = tile_placements(window_count, display, TilingInsets::MACOS_DEFAULT)?;
        let monitor_ids = virtual_monitor_ids(window_count)?;
        let regions = virtual_region_set(window_count)?;
        let trace = run_input_trace(&regions, &scripted_trace(window_count)?)?;
        let (_generation, router) = build_lab_router(&monitor_ids)?;

        let report_slot: Arc<Mutex<Option<VirtualMonitorLabReport>>> = Arc::new(Mutex::new(None));
        let report_for_app = Arc::clone(&report_slot);
        let root_tile = tiles[0];
        let bind_states = vec![TileBindEvaluation::Waiting; window_count];
        let fallback_trace = trace.clone();
        let fallback_tiles = tiles.clone();

        let options = eframe::NativeOptions {
            viewport: tile_viewport_builder(&root_tile),
            persist_window: false,
            ..Default::default()
        };
        eframe::run_native(
            "Arcen Deck Virtual Monitor Lab",
            options,
            Box::new(move |_cc| {
                Ok(Box::new(LabApp {
                    monitor_ids,
                    tiles,
                    router,
                    trace,
                    expected_display_id: display.cg_display_id,
                    started_at: Instant::now(),
                    timeout,
                    frame_index: 0,
                    paint_acks: Vec::new(),
                    bind_states,
                    finished: false,
                    report: report_for_app,
                }))
            }),
        )
        .map_err(VirtualMonitorLabError::Native)?;

        let outcome = match report_slot.lock() {
            Ok(mut slot) => slot.take(),
            Err(poisoned) => poisoned.into_inner().take(),
        };
        Ok(outcome.unwrap_or(VirtualMonitorLabReport {
            window_count,
            cg_display_id: display.cg_display_id,
            tiles: fallback_tiles,
            bound_session_monitor_ids: Vec::new(),
            unbound_session_monitor_ids: (1..=u16::try_from(window_count).unwrap_or(u16::MAX))
                .collect(),
            isolation_verified: false,
            trace: fallback_trace,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::protocol::messages::{
        REGION_PEN_EVENT, REGION_POINTER_BUTTON, REGION_POINTER_ENTER, REGION_POINTER_LEAVE,
        REGION_POINTER_MOTION, REGION_POINTER_SCROLL,
    };

    /// A 16" MacBook Pro-class built-in display: one display, plenty of room
    /// for both tilings.
    fn laptop_display() -> DisplayBoundsPts {
        DisplayBoundsPts {
            cg_display_id: 7,
            origin_x: 0.0,
            origin_y: 0.0,
            width: 1_728.0,
            height: 1_117.0,
        }
    }

    fn display_of(width: f32, height: f32) -> DisplayBoundsPts {
        DisplayBoundsPts {
            cg_display_id: 7,
            origin_x: 0.0,
            origin_y: 0.0,
            width,
            height,
        }
    }

    fn outer_rect(tile: TilePlacement, insets: TilingInsets) -> (f32, f32, f32, f32) {
        (
            tile.outer_x,
            tile.outer_y,
            tile.outer_x + tile.inner_width,
            tile.outer_y + tile.outer_height(insets),
        )
    }

    fn overlaps(left: (f32, f32, f32, f32), right: (f32, f32, f32, f32)) -> bool {
        left.0 < right.2 && right.0 < left.2 && left.1 < right.3 && right.1 < left.3
    }

    /// Independently recomputes the shared transformer's forward mapping
    /// (`origin + rounded_ratio(local * (physical - 1), logical_extent - 1)`)
    /// so the trace's applied coordinates are checked against the contract's
    /// own arithmetic rather than against whatever the transformer returned.
    fn expected_applied(origin_px: i64, logical: i64, extent_px: u32) -> i64 {
        let logical_extent = i64::from(extent_px) * LOGICAL_UNITS_PER_PIXEL;
        let denominator = logical_extent - 1;
        let numerator = logical * i64::from(extent_px - 1);
        origin_px + (numerator + denominator / 2) / denominator
    }

    fn expected_origin(index: usize, window_count: usize) -> (i64, i64) {
        let grid = tiling_grid(window_count).expect("valid window count");
        let column = i64::try_from(index % grid.columns).expect("small index");
        let row = i64::try_from(index / grid.columns).expect("small index");
        (
            column * i64::from(VIRTUAL_MONITOR_WIDTH_PX),
            row * i64::from(VIRTUAL_MONITOR_HEIGHT_PX),
        )
    }

    fn trace_for(window_count: usize) -> Vec<InputTraceRecord> {
        let regions = virtual_region_set(window_count).expect("lab regions");
        let steps = scripted_trace(window_count).expect("lab trace script");
        run_input_trace(&regions, &steps).expect("lab trace runs")
    }

    fn region_events(trace: &[InputTraceRecord]) -> Vec<(&'static str, u32)> {
        trace
            .iter()
            .filter_map(|record| match &record.input {
                TracedInput::Region {
                    input_type,
                    region_id,
                    ..
                } => Some((*input_type, *region_id)),
                TracedInput::Keyboard { .. } => None,
            })
            .collect()
    }

    fn acks_for(window_count: usize) -> Vec<crate::ui::multi_window_diagnostic::ViewportPaintAck> {
        let monitor_ids = virtual_monitor_ids(window_count).expect("lab monitor ids");
        expected_paint_plan(&monitor_ids)
            .into_iter()
            .map(
                |(viewport_id, monitor_id)| crate::ui::multi_window_diagnostic::ViewportPaintAck {
                    viewport_id,
                    claimed_monitor_id: monitor_id,
                    painted_tag: synthetic_frame(monitor_id, 0).rgba[0],
                },
            )
            .collect()
    }

    // ---- safety gate ----

    #[test]
    fn lab_admission_requires_the_exact_runtime_opt_in() {
        for raw in [
            None,
            Some(""),
            Some("0"),
            Some("true"),
            Some("yes"),
            Some(" 1"),
        ] {
            assert!(
                matches!(lab_admission(raw, 2), Err(VirtualMonitorLabError::Disabled)),
                "{raw:?} must not enable the lab"
            );
        }
        assert!(lab_admission(Some("1"), 2).is_ok());
        assert!(lab_admission(Some("1"), 4).is_ok());
    }

    #[test]
    fn lab_admission_checks_the_gate_before_the_window_count() {
        // A disabled lab never reveals anything about its arguments.
        assert!(matches!(
            lab_admission(None, 3),
            Err(VirtualMonitorLabError::Disabled)
        ));
        assert!(matches!(
            lab_admission(Some("1"), 3),
            Err(VirtualMonitorLabError::InvalidWindowCount(3))
        ));
    }

    #[test]
    fn lab_admission_rejects_every_unsupported_window_count() {
        for count in [0_usize, 1, 3, 5, 8] {
            assert!(
                matches!(
                    lab_admission(Some("1"), count),
                    Err(VirtualMonitorLabError::InvalidWindowCount(rejected)) if rejected == count
                ),
                "{count} windows must be rejected"
            );
        }
    }

    // ---- tiling math ----

    #[test]
    fn tiling_grid_is_halves_for_two_and_quadrants_for_four() {
        let halves = tiling_grid(2).expect("two windows");
        assert_eq!(
            (halves.columns, halves.rows, halves.window_count()),
            (2, 1, 2)
        );
        let quadrants = tiling_grid(4).expect("four windows");
        assert_eq!(
            (quadrants.columns, quadrants.rows, quadrants.window_count()),
            (2, 2, 4)
        );
    }

    #[test]
    fn two_tiles_are_side_by_side_halves_of_one_row() {
        let insets = TilingInsets::MACOS_DEFAULT;
        let tiles = tile_placements(2, laptop_display(), insets).expect("halves fit");
        assert_eq!(tiles.len(), 2);
        assert_eq!(
            tiles
                .iter()
                .map(|tile| (tile.column, tile.row))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 0)]
        );
        assert_eq!(tiles[0].outer_y, tiles[1].outer_y);
        assert!(tiles[1].outer_x > tiles[0].outer_x);
        assert!((tiles[0].inner_width - tiles[1].inner_width).abs() < f32::EPSILON);
        assert_eq!(
            tiles[1].outer_x - (tiles[0].outer_x + tiles[0].inner_width),
            insets.gutter
        );
    }

    #[test]
    fn four_tiles_are_distinct_quadrants() {
        let tiles =
            tile_placements(4, laptop_display(), TilingInsets::MACOS_DEFAULT).expect("quads fit");
        assert_eq!(
            tiles
                .iter()
                .map(|tile| (tile.column, tile.row))
                .collect::<Vec<_>>(),
            vec![(0, 0), (1, 0), (0, 1), (1, 1)]
        );
        assert_eq!(
            tiles
                .iter()
                .map(|tile| tile.session_monitor_id.get())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn tiles_never_overlap_once_their_title_bars_are_counted() {
        let insets = TilingInsets::MACOS_DEFAULT;
        for window_count in [2_usize, 4] {
            let tiles =
                tile_placements(window_count, laptop_display(), insets).expect("tiling fits");
            for first in 0..tiles.len() {
                for second in (first + 1)..tiles.len() {
                    assert!(
                        !overlaps(
                            outer_rect(tiles[first], insets),
                            outer_rect(tiles[second], insets)
                        ),
                        "tiles {first} and {second} of {window_count} overlap"
                    );
                }
            }
        }
    }

    #[test]
    fn tiles_stay_inside_the_displays_work_area() {
        let insets = TilingInsets::MACOS_DEFAULT;
        let display = laptop_display();
        for window_count in [2_usize, 4] {
            for tile in tile_placements(window_count, display, insets).expect("tiling fits") {
                let (left, top, right, bottom) = outer_rect(tile, insets);
                assert!(left >= display.origin_x + insets.left);
                assert!(top >= display.origin_y + insets.top);
                assert!(right <= display.origin_x + display.width - insets.right);
                assert!(bottom <= display.origin_y + display.height - insets.bottom);
            }
        }
    }

    #[test]
    fn tiling_honours_a_non_zero_display_origin() {
        let insets = TilingInsets::MACOS_DEFAULT;
        let display = DisplayBoundsPts {
            cg_display_id: 9,
            origin_x: -1_920.0,
            origin_y: 240.0,
            width: 1_728.0,
            height: 1_117.0,
        };
        let tiles = tile_placements(4, display, insets).expect("tiling fits");
        assert_eq!(tiles[0].outer_x, display.origin_x + insets.left);
        assert_eq!(tiles[0].outer_y, display.origin_y + insets.top);
    }

    #[test]
    fn tiling_is_deterministic() {
        let insets = TilingInsets::MACOS_DEFAULT;
        assert_eq!(
            tile_placements(4, laptop_display(), insets).expect("first"),
            tile_placements(4, laptop_display(), insets).expect("second")
        );
    }

    #[test]
    fn tiling_rejects_unsupported_counts_and_tiny_displays() {
        let insets = TilingInsets::MACOS_DEFAULT;
        for count in [0_usize, 1, 3, 5] {
            assert!(matches!(
                tile_placements(count, laptop_display(), insets),
                Err(VirtualMonitorLabError::InvalidWindowCount(_))
            ));
        }
        assert!(matches!(
            tile_placements(4, display_of(700.0, 640.0), insets),
            Err(VirtualMonitorLabError::DisplayTooSmall { .. })
        ));
    }

    #[test]
    fn every_tile_has_a_unique_developer_only_window_title() {
        let titles = virtual_monitor_ids(4)
            .expect("lab monitor ids")
            .into_iter()
            .map(lab_window_title)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(titles.len(), 4);
        for title in &titles {
            assert!(title.starts_with("Arcen Deck Lab — Virtual Monitor "));
            // Never collides with a production/diagnostic window title.
            assert!(!crate::ui::multi_window_runtime::window_title_for(
                SessionMonitorId::new(1).expect("nonzero")
            )
            .eq(title));
        }
    }

    // ---- virtual regions ----

    #[test]
    fn virtual_regions_tile_applied_space_without_overlapping() {
        for window_count in [2_usize, 4] {
            let regions = virtual_region_set(window_count).expect("lab regions");
            assert_eq!(regions.regions().len(), window_count);
            for (index, region) in regions.regions().iter().enumerate() {
                let (origin_x, origin_y) = expected_origin(index, window_count);
                assert_eq!(region.applied_rect().origin().x, origin_x);
                assert_eq!(region.applied_rect().origin().y, origin_y);
                assert_eq!(
                    region.descriptor().id().get(),
                    u32::try_from(index + 1).expect("small index")
                );
                assert_eq!(region.descriptor().is_primary(), index == 0);
            }
        }
    }

    // ---- paint isolation ----

    #[test]
    fn the_paint_plan_starts_at_the_root_viewport_and_stays_unique() {
        for window_count in [2_usize, 4] {
            let monitor_ids = virtual_monitor_ids(window_count).expect("lab monitor ids");
            let plan = expected_paint_plan(&monitor_ids);
            assert_eq!(plan.len(), window_count);
            assert_eq!(plan[0].0, egui::ViewportId::ROOT);
            assert_eq!(plan[0].1, monitor_ids[0]);
            let unique = plan
                .iter()
                .map(|(viewport_id, _)| *viewport_id)
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(unique.len(), window_count);
            for (index, (viewport_id, monitor_id)) in plan.iter().enumerate().skip(1) {
                assert_eq!(
                    *viewport_id,
                    MonitorWindowAssignment::viewport_id_for(monitor_ids[index])
                );
                assert_eq!(*monitor_id, monitor_ids[index]);
            }
        }
    }

    #[test]
    fn honest_per_tile_acknowledgements_verify_isolation() {
        for window_count in [2_usize, 4] {
            let monitor_ids = virtual_monitor_ids(window_count).expect("lab monitor ids");
            let plan = expected_paint_plan(&monitor_ids);
            assert!(
                crate::ui::multi_window_diagnostic::verify_paint_isolation(
                    &plan,
                    &acks_for(window_count)
                )
                .is_ok(),
                "{window_count} honest acks must verify"
            );
        }
    }

    #[test]
    fn swapped_tile_acknowledgements_fail_isolation() {
        for window_count in [2_usize, 4] {
            let monitor_ids = virtual_monitor_ids(window_count).expect("lab monitor ids");
            let plan = expected_paint_plan(&monitor_ids);
            let mut acks = acks_for(window_count);
            // Two tiles consume each other's routed frame.
            let first_tag = acks[0].painted_tag;
            acks[0].painted_tag = acks[1].painted_tag;
            acks[1].painted_tag = first_tag;
            assert!(
                crate::ui::multi_window_diagnostic::verify_paint_isolation(&plan, &acks).is_err(),
                "{window_count} swapped acks must fail"
            );
        }
    }

    #[test]
    fn every_tile_paints_a_distinct_routed_tag() {
        for window_count in [2_usize, 4] {
            let monitor_ids = virtual_monitor_ids(window_count).expect("lab monitor ids");
            let (generation, router) = build_lab_router(&monitor_ids).expect("lab router");
            assert_eq!(generation.get(), LAB_GENERATION);
            let tags = monitor_ids
                .iter()
                .map(|&monitor_id| {
                    router
                        .latest_frame(monitor_id)
                        .expect("every lab monitor is seeded")
                        .rgba[0]
                })
                .collect::<std::collections::BTreeSet<_>>();
            assert_eq!(tags.len(), window_count);
        }
    }

    // ---- tile bind evaluation ----

    #[test]
    fn tile_bind_evaluation_covers_every_outcome_in_precedence_order() {
        let base = TileWindowObservation {
            inner_rect_known: true,
            fullscreen: Some(false),
            close_requested: false,
            observed_display_id: Some(7),
        };
        assert_eq!(evaluate_tile_bind(base, 7), TileBindEvaluation::Bound);
        assert_eq!(
            evaluate_tile_bind(
                TileWindowObservation {
                    close_requested: true,
                    fullscreen: Some(true),
                    inner_rect_known: false,
                    observed_display_id: None,
                },
                7
            ),
            TileBindEvaluation::Closed
        );
        assert_eq!(
            evaluate_tile_bind(
                TileWindowObservation {
                    fullscreen: Some(true),
                    ..base
                },
                7
            ),
            TileBindEvaluation::UnexpectedFullscreen
        );
        assert_eq!(
            evaluate_tile_bind(
                TileWindowObservation {
                    inner_rect_known: false,
                    ..base
                },
                7
            ),
            TileBindEvaluation::Waiting
        );
        assert_eq!(
            evaluate_tile_bind(
                TileWindowObservation {
                    observed_display_id: None,
                    ..base
                },
                7
            ),
            TileBindEvaluation::Waiting
        );
        assert_eq!(
            evaluate_tile_bind(
                TileWindowObservation {
                    observed_display_id: Some(9),
                    ..base
                },
                7
            ),
            TileBindEvaluation::WrongDisplay {
                expected: 7,
                observed: 9,
            }
        );
    }

    // ---- scripted input parity ----

    #[test]
    fn the_trace_visits_every_virtual_monitor_and_returns_to_the_first() {
        for window_count in [2_usize, 4] {
            let trace = trace_for(window_count);
            let enters = region_events(&trace)
                .into_iter()
                .filter(|(input_type, _)| *input_type == REGION_POINTER_ENTER)
                .map(|(_, region_id)| region_id)
                .collect::<Vec<_>>();
            let mut expected =
                (1..=u32::try_from(window_count).expect("small count")).collect::<Vec<_>>();
            expected.push(1);
            assert_eq!(enters, expected);
        }
    }

    #[test]
    fn every_region_change_is_a_leave_then_an_enter() {
        for window_count in [2_usize, 4] {
            let events = region_events(&trace_for(window_count));
            let mut focused: Option<u32> = None;
            for (input_type, region_id) in events {
                match input_type {
                    REGION_POINTER_ENTER => {
                        assert_eq!(focused, None, "entered {region_id} while still focused");
                        focused = Some(region_id);
                    }
                    REGION_POINTER_LEAVE => {
                        assert_eq!(focused, Some(region_id));
                        focused = None;
                    }
                    REGION_PEN_EVENT => {}
                    _ => assert_eq!(
                        focused,
                        Some(region_id),
                        "{input_type} outside the focused region"
                    ),
                }
            }
            assert_eq!(focused, Some(1));
        }
    }

    #[test]
    fn the_trace_carries_every_scripted_event_kind() {
        let kinds = region_events(&trace_for(4))
            .into_iter()
            .map(|(input_type, _)| input_type)
            .collect::<std::collections::BTreeSet<_>>();
        for expected in [
            REGION_POINTER_ENTER,
            REGION_POINTER_LEAVE,
            REGION_POINTER_MOTION,
            REGION_POINTER_BUTTON,
            REGION_POINTER_SCROLL,
            REGION_PEN_EVENT,
        ] {
            assert!(
                kinds.contains(expected),
                "{expected} missing from the trace"
            );
        }
        assert!(trace_for(4)
            .iter()
            .any(|record| matches!(record.input, TracedInput::Keyboard { .. })));
    }

    #[test]
    fn button_edges_are_paired_press_then_release_per_monitor() {
        for window_count in [2_usize, 4] {
            let buttons = trace_for(window_count)
                .into_iter()
                .filter_map(|record| match record.input {
                    TracedInput::Region {
                        input_type,
                        region_id,
                        button: Some((button, pressed)),
                        ..
                    } if input_type == REGION_POINTER_BUTTON => Some((region_id, button, pressed)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(buttons.len(), window_count * 2);
            for monitor_index in 0..window_count {
                let region_id = u32::try_from(monitor_index + 1).expect("small index");
                assert_eq!(buttons[monitor_index * 2], (region_id, 1, true));
                assert_eq!(buttons[monitor_index * 2 + 1], (region_id, 1, false));
            }
        }
    }

    #[test]
    fn sequences_are_nonzero_and_strictly_increasing_across_every_event_kind() {
        for window_count in [2_usize, 4] {
            let mut last = 0_u64;
            for record in trace_for(window_count) {
                assert!(record.sequence > last, "{record} did not advance {last}");
                last = record.sequence;
            }
            assert!(last > 0);
        }
    }

    #[test]
    fn the_keyboard_step_shares_one_session_global_sequence_with_region_input() {
        let trace = trace_for(2);
        let keyboard_index = trace
            .iter()
            .position(|record| matches!(record.input, TracedInput::Keyboard { .. }))
            .expect("the script presses a key");
        assert!(keyboard_index > 0);
        assert_eq!(
            trace[keyboard_index].sequence,
            trace[keyboard_index - 1].sequence + 1
        );
        // The next region event must continue above the keyboard's own
        // sequence rather than reissuing it.
        let next_region = trace[keyboard_index..]
            .iter()
            .find(|record| matches!(record.input, TracedInput::Region { .. }))
            .expect("region input resumes after the keyboard");
        assert!(next_region.sequence > trace[keyboard_index].sequence);
    }

    #[test]
    fn the_wacom_step_adopts_its_device_supplied_sequence() {
        let trace = trace_for(2);
        let pen_sequences = trace
            .iter()
            .filter(|record| {
                matches!(
                    &record.input,
                    TracedInput::Region { input_type, .. } if *input_type == REGION_PEN_EVENT
                )
            })
            .map(|record| record.sequence)
            .collect::<Vec<_>>();
        // Two pen samples per monitor: the emitter-sequenced one, then the
        // device-sequenced (Wacom) one exactly `WACOM_SEQUENCE_GAP` later.
        assert_eq!(pen_sequences.len(), 4);
        assert_eq!(pen_sequences[1], pen_sequences[0] + WACOM_SEQUENCE_GAP);
        assert_eq!(pen_sequences[3], pen_sequences[2] + WACOM_SEQUENCE_GAP);
    }

    #[test]
    fn applied_coordinates_match_an_independent_recomputation_of_the_shared_contract() {
        for window_count in [2_usize, 4] {
            let mut checked = 0_usize;
            for record in trace_for(window_count) {
                let TracedInput::Region {
                    region_id,
                    logical_x,
                    logical_y,
                    applied_x,
                    applied_y,
                    ..
                } = record.input
                else {
                    continue;
                };
                let index = usize::try_from(region_id - 1).expect("region ids start at one");
                let (origin_x, origin_y) = expected_origin(index, window_count);
                assert_eq!(
                    applied_x,
                    expected_applied(origin_x, logical_x, VIRTUAL_MONITOR_WIDTH_PX)
                );
                assert_eq!(
                    applied_y,
                    expected_applied(origin_y, logical_y, VIRTUAL_MONITOR_HEIGHT_PX)
                );
                checked += 1;
            }
            assert!(checked > 0);
        }
    }

    #[test]
    fn applied_coordinates_stay_inside_their_own_virtual_monitor() {
        for window_count in [2_usize, 4] {
            for record in trace_for(window_count) {
                let TracedInput::Region {
                    region_id,
                    applied_x,
                    applied_y,
                    ..
                } = record.input
                else {
                    continue;
                };
                let index = usize::try_from(region_id - 1).expect("region ids start at one");
                let (origin_x, origin_y) = expected_origin(index, window_count);
                assert!(
                    (origin_x..origin_x + i64::from(VIRTUAL_MONITOR_WIDTH_PX)).contains(&applied_x)
                );
                assert!((origin_y..origin_y + i64::from(VIRTUAL_MONITOR_HEIGHT_PX))
                    .contains(&applied_y));
            }
        }
    }

    #[test]
    fn scroll_deltas_use_the_shared_logical_unit_scale() {
        let steps = scripted_trace(2).expect("lab trace script");
        assert!(steps.iter().any(|step| matches!(
            step,
            ScriptedInputStep::PointerScroll { ticks_y, .. } if *ticks_y == -1
        )));
        let regions = virtual_region_set(2).expect("lab regions");
        let scrolls = run_input_trace(&regions, &steps)
            .expect("lab trace runs")
            .into_iter()
            .filter(|record| {
                matches!(
                    &record.input,
                    TracedInput::Region { input_type, .. } if *input_type == REGION_POINTER_SCROLL
                )
            })
            .count();
        assert_eq!(scrolls, 2);
        assert_eq!(LOGICAL_UNITS_PER_PIXEL, 120);
    }

    #[test]
    fn the_trace_is_deterministic() {
        assert_eq!(trace_for(4), trace_for(4));
        assert_ne!(trace_for(2), trace_for(4));
    }

    #[test]
    fn fraction_logical_stays_inside_the_regions_logical_extent() {
        let width_extent = i64::from(VIRTUAL_MONITOR_WIDTH_PX) * LOGICAL_UNITS_PER_PIXEL;
        for numerator in 0..=FRACTION_DENOMINATOR {
            let logical = fraction_logical(numerator, VIRTUAL_MONITOR_WIDTH_PX);
            assert!(
                (0..width_extent).contains(&logical),
                "{numerator}/4 escaped"
            );
        }
        assert_eq!(fraction_logical(0, VIRTUAL_MONITOR_WIDTH_PX), 0);
        assert_eq!(
            fraction_logical(FRACTION_DENOMINATOR, VIRTUAL_MONITOR_WIDTH_PX),
            width_extent - 1
        );
    }
}
