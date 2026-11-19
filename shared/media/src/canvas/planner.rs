// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas planner: one monitor's Keel facts in, canvas records out.
//!
//! Every decision comes from Keel (damage, history, traits, verified moves,
//! classes, motion regions) and the mirror; the planner maps those facts to
//! commands (design §4.2), in this order of precedence:
//!
//! 1. A block wholly inside a verified move becomes part of a `Copy`.
//!    Moves are hints from the OS or Keel's estimator; each is checked
//!    exactly against the mirror (what the Deck shows), and its source must
//!    hold no lossy block (ADR 0028 §6), before it is sent.
//! 2. A solid block becomes part of a `Fill` (coalesced into rectangles).
//! 3. A whole 64 × 64 cell the cache already holds becomes a `CachePaint`;
//!    a whole cell sent as tiles is stored (`CacheStore`) for next time.
//! 4. A `Motion` block inside a surface goes to that surface; motion
//!    outside every surface (a caret, a clock) goes as tiles.
//! 5. Other changed blocks go as exact tiles. Many-colour blocks over
//!    [`PlannerConfig::image_tile_limit_px`] in one tick (a photo opening)
//!    go as one lossy picture of a one-shot surface instead, and are
//!    rebuilt exactly once they settle (rule 6).
//! 6. `Settled` blocks that hold lossy pixels are refined to exact tiles
//!    with the budget left over.
//! 7. Sustained full-frame motion (at least
//!    [`PlannerConfig::full_motion_enter_percent`] of the blocks for
//!    [`PlannerConfig::full_motion_hold`]) makes one whole-monitor surface:
//!    the degenerate case is today's full-frame stream, with no separate
//!    path.
//!
//! A tick is two calls: [`CanvasPlanner::observe`] runs Keel and returns
//! the monitor's [`MonitorDemand`]; the governor turns every monitor's
//! demand into grants; [`CanvasPlanner::plan`] spends its grant, encodes
//! surfaces through the [`SurfaceEncoder`] seam, applies the records to the
//! mirror and returns them for the video lane.
//!
//! Work not granted is never lost: its blocks stay pending and are sent,
//! oldest first, when budget allows. Mandatory work (a reset's repaint, a
//! new surface's IDR, damage near the latest input, a keepalive) is never
//! deferred.

use super::cost::CostModel;
use super::governor::{MonitorDemand, MonitorGrant, SurfaceDemand, TickOutcome};
use super::mirror::CanvasMirror;
use super::pixels::{NoSurfacePixels, PackedTileCodes};
use super::state::CanvasStateError;
use super::surface::{
    SurfaceChange, SurfaceEncodeError, SurfaceEncodeRequest, SurfaceEncoder, SurfaceRole,
    SurfaceScale, SurfaceSet, SurfaceSlot,
};
use super::tile::{CanvasTileError, TilePacker, pack_tiles};
use crate::refine::source::RefineSource;
use crate::refine::tile::{BLOCK_U32, RefineGrid, TileCodes, TileError, TileRect, extract_bgra8};
use arcen_keel::{
    BgraFrame, BlockClass, BlockClassMap, BlockGrid, BlockHistory, BlockPaint, BlockTraitsMap,
    ClassConfig, DamageTracker, ExternalDamage, KeelError, KernelPreference, MotionConfig,
    MotionEstimator, MotionRegions, MoveRect, PaintKind, PixelRect, RegionConfig, SlotId, TileKey,
    TileLedger, WorkKind, verify_move,
};
use arcen_protocol::canvas::{
    CanvasCommand, CanvasFrame, CanvasPixelFormat, CanvasRecord, CanvasRecordError, CanvasRect,
    CanvasReset, CanvasResetReason, CanvasScope, CanvasTerms, MAX_CANVAS_COMMANDS,
    MAX_CANVAS_FRAME_BYTES, MAX_CANVAS_SIDE, MAX_FRAME_AREA_FACTOR, SURFACE_CODEC_H264,
    SurfacePaint, canvas_block_count,
};
use arcen_protocol::refine::{BlockRuns, RefineDomain};
use std::fmt::{Display, Formatter};
use std::num::NonZeroU32;
use std::time::{Duration, Instant};

/// Side of a cache cell, in pixels (a whole number of blocks; a cache
/// entry follows the tile rules).
pub const CACHE_CELL: u32 = 64;

/// Most copies in one frame.
pub const MAX_COPIES_PER_FRAME: usize = 8;

const EMPTY_RECT: TileRect = TileRect {
    x: 0,
    y: 0,
    width: 0,
    height: 0,
};
const CELL_BLOCKS: u32 = CACHE_CELL / BLOCK_U32;
/// Room kept below the protocol's record limit for headers.
const FRAME_BYTES_SOFT_LIMIT: usize = MAX_CANVAS_FRAME_BYTES - 64 * 1024;

/// Tunables of a [`CanvasPlanner`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannerConfig {
    /// Keel block classes.
    pub classes: ClassConfig,
    /// Keel motion regions.
    pub regions: RegionConfig,
    /// Keel motion search.
    pub motion: MotionConfig,
    /// Keel hashing kernel.
    pub kernel: KernelPreference,
    /// Rule 5: many-colour damage up to this many pixels in one tick goes
    /// as exact tiles; more goes as one lossy picture.
    pub image_tile_limit_px: u64,
    /// Blocks within this distance of the latest input are mandatory.
    pub input_radius_px: u32,
    /// How long an input event keeps its neighbourhood mandatory.
    pub input_window: Duration,
    /// Rule 7: share of `Motion` blocks (percent) that starts the
    /// whole-monitor surface after [`Self::full_motion_hold`].
    pub full_motion_enter_percent: u64,
    /// Rule 7: share below which the whole-monitor surface ends.
    pub full_motion_exit_percent: u64,
    /// Rule 7: how long the share must hold.
    pub full_motion_hold: Duration,
    /// An empty frame keeps a still canvas alive this often.
    pub keepalive: Duration,
    /// Use the Deck's cache.
    pub cache: bool,
}

impl Default for PlannerConfig {
    fn default() -> Self {
        Self {
            classes: ClassConfig::default(),
            regions: RegionConfig::default(),
            motion: MotionConfig::default(),
            kernel: KernelPreference::Auto,
            image_tile_limit_px: 128 * 128 * 4,
            input_radius_px: 64,
            input_window: Duration::from_millis(250),
            full_motion_enter_percent: 60,
            full_motion_exit_percent: 40,
            full_motion_hold: Duration::from_millis(500),
            keepalive: Duration::from_secs(1),
            cache: true,
        }
    }
}

/// Why planning failed. The planner starts a new generation (reason
/// `recovery`) at its next tick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlannerError {
    /// The frame is not the planner's size: a new size is a new planner.
    Geometry,
    /// The canvas exceeds the Deck's or the protocol's limit.
    CanvasTooLarge,
    /// [`CanvasPlanner::plan`] without [`CanvasPlanner::observe`].
    NotObserved,
    /// Keel refused the frame.
    Keel(KeelError),
    /// A tile did not pack.
    Tile(CanvasTileError),
    /// A surface did not encode.
    Surface(SurfaceEncodeError),
    /// The mirror refused a record the planner built.
    State(CanvasStateError),
    /// A record did not encode.
    Record(CanvasRecordError),
}

impl Display for PlannerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Geometry => formatter.write_str("canvas planner: frame size changed"),
            Self::CanvasTooLarge => formatter.write_str("canvas planner: canvas too large"),
            Self::NotObserved => formatter.write_str("canvas planner: plan without observe"),
            Self::Keel(error) => write!(formatter, "canvas planner: keel: {error}"),
            Self::Tile(error) => write!(formatter, "canvas planner: tile: {error:?}"),
            Self::Surface(error) => write!(formatter, "canvas planner: {error}"),
            Self::State(error) => write!(formatter, "canvas planner: {error}"),
            Self::Record(error) => write!(formatter, "canvas planner: record: {error}"),
        }
    }
}

impl std::error::Error for PlannerError {}

impl From<KeelError> for PlannerError {
    fn from(error: KeelError) -> Self {
        Self::Keel(error)
    }
}

/// Commands of a tick, by kind.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CommandMix {
    pub fill: u32,
    pub copy: u32,
    pub tile: u32,
    pub cache_store: u32,
    pub cache_paint: u32,
    pub cache_evict: u32,
    pub surface_open: u32,
    pub surface_close: u32,
    pub surface_paint: u32,
}

impl CommandMix {
    fn count(&mut self, command: &CanvasCommand<'_>) {
        let slot = match command {
            CanvasCommand::Fill { .. } => &mut self.fill,
            CanvasCommand::Copy { .. } => &mut self.copy,
            CanvasCommand::Tile(_) => &mut self.tile,
            CanvasCommand::CacheStore { .. } => &mut self.cache_store,
            CanvasCommand::CachePaint { .. } => &mut self.cache_paint,
            CanvasCommand::CacheEvict { .. } => &mut self.cache_evict,
            CanvasCommand::SurfaceOpen { .. } => &mut self.surface_open,
            CanvasCommand::SurfaceClose { .. } => &mut self.surface_close,
            CanvasCommand::SurfacePaint(_) => &mut self.surface_paint,
        };
        *slot += 1;
    }

    /// Adds another mix.
    pub fn add(&mut self, other: Self) {
        self.fill += other.fill;
        self.copy += other.copy;
        self.tile += other.tile;
        self.cache_store += other.cache_store;
        self.cache_paint += other.cache_paint;
        self.cache_evict += other.cache_evict;
        self.surface_open += other.surface_open;
        self.surface_close += other.surface_close;
        self.surface_paint += other.surface_paint;
    }

    /// Every command.
    #[must_use]
    pub const fn total(&self) -> u32 {
        self.fill
            + self.copy
            + self.tile
            + self.cache_store
            + self.cache_paint
            + self.cache_evict
            + self.surface_open
            + self.surface_close
            + self.surface_paint
    }
}

/// Predicted CPU of what a tick sent, by tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TickEstimate {
    /// Keel work (hashing, classification).
    pub sunk: Duration,
    /// Mandatory work.
    pub mandatory: Duration,
    /// Copies, fills, cache paints, tiles, one-shot pictures.
    pub content: Duration,
    /// Surface pictures.
    pub surfaces: Duration,
    /// Refinement tiles.
    pub refine: Duration,
}

impl TickEstimate {
    /// Every tier.
    #[must_use]
    pub fn total(&self) -> Duration {
        self.sunk + self.mandatory + self.content + self.surfaces + self.refine
    }
}

/// What one [`CanvasPlanner::plan`] produced.
#[derive(Debug, Clone, Default)]
pub struct PlannedTick {
    /// Wire records for the video lane, in order (`FrameType::Canvas`
    /// bodies): a reset first when one was due, then frames.
    pub records: Vec<Vec<u8>>,
    /// Generation of the records.
    pub generation: u32,
    /// Newest fence (0 when no frame was sent).
    pub fence: u64,
    /// The frames carried optional work (non-IDR surface pictures,
    /// refinement).
    pub carried_optional: bool,
    /// Commands by kind.
    pub mix: CommandMix,
    /// Predicted CPU of what was sent.
    pub estimate: TickEstimate,
    /// Changed blocks painted exactly this tick and how many ticks each
    /// waited since it changed.
    pub exact_latency: Vec<(u32, u32)>,
    /// Changed blocks still waiting after this tick.
    pub pending_blocks: u32,
    /// Surface pictures sent.
    pub surfaces_painted: u32,
    /// Measured work, for the cost model.
    pub measured: Vec<(WorkKind, u64, Duration)>,
}

impl PlannedTick {
    /// The governor's view of the tick.
    #[must_use]
    pub fn outcome(&self, monitor_id: u16) -> TickOutcome {
        TickOutcome {
            monitor_id,
            generation: self.generation,
            fence: self.fence,
            carried_optional: self.carried_optional,
            measured: self.measured.clone(),
        }
    }
}

/// An eight-bit source over the borrowed frame, for tile packing.
struct FrameSource<'a> {
    frame: BgraFrame<'a>,
    grid: RefineGrid,
}

impl RefineSource for FrameSource<'_> {
    fn grid(&self) -> RefineGrid {
        self.grid
    }

    fn depth(&self) -> u8 {
        8
    }

    fn domain(&self) -> RefineDomain {
        RefineDomain::Rgb
    }

    fn observe(&mut self, _src: &[u8], _stride: usize, _rect: TileRect) -> Vec<usize> {
        Vec::new()
    }

    fn ready(&self) -> bool {
        true
    }

    fn extract(&self, rect: TileRect) -> Result<TileCodes, TileError> {
        extract_bgra8(self.frame.pixels(), self.frame.stride(), self.grid, rect)
    }
}

#[derive(Debug, Clone, Copy)]
enum Lot {
    Tile(u32),
    OneShot,
}

#[derive(Debug, Clone)]
struct OneShot {
    id: u8,
    rect: TileRect,
    blocks: Vec<u32>,
    cost: Duration,
}

impl OneShot {
    fn coded(&self) -> (u32, u32) {
        SurfaceScale::Full.coded(self.rect.width, self.rect.height)
    }

    fn coded_pixels(&self) -> u64 {
        let (width, height) = self.coded();
        u64::from(width) * u64::from(height)
    }
}

#[derive(Debug, Clone)]
struct SurfaceWork {
    id: u8,
    blocks: Vec<u32>,
    idr: bool,
}

/// What [`CanvasPlanner::observe`] decided, waiting for the grant.
#[derive(Debug, Clone, Default)]
struct Staged {
    now: Duration,
    reset: Option<Vec<u8>>,
    copies: Vec<MoveRect>,
    copied: Vec<u32>,
    changes: Vec<SurfaceChange>,
    closes_for_reset: Vec<u8>,
    fills: Vec<u32>,
    cache_paints: Vec<(TileKey, u32)>,
    cache_stores: Vec<(TileKey, u32)>,
    surfaces: Vec<SurfaceWork>,
    mandatory_tiles: Vec<u32>,
    lots: Vec<(Lot, Duration)>,
    one_shot: Option<OneShot>,
    refine: Vec<(u32, Duration)>,
    estimate: TickEstimate,
    surface_costs: Vec<(u8, Duration)>,
    measured: Vec<(WorkKind, u64, Duration)>,
}

/// One owned command before the record borrows it.
#[derive(Debug, Clone)]
enum Cmd {
    Fill {
        rect: TileRect,
        rgb: [u8; 3],
    },
    Copy {
        src: TileRect,
        dst_x: u32,
        dst_y: u32,
    },
    Tile(usize),
    CacheStore {
        slot: u32,
        rect: TileRect,
    },
    CachePaint {
        slot: u32,
        x: u32,
        y: u32,
    },
    CacheEvict {
        slot: u32,
    },
    Open {
        id: u8,
        width: u32,
        height: u32,
    },
    Close {
        id: u8,
    },
    Paint {
        id: u8,
        idr: bool,
        dest: TileRect,
        blocks: Vec<u32>,
        au: usize,
    },
}

fn side(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

fn canvas_rect(rect: TileRect) -> CanvasRect {
    CanvasRect {
        x: side(rect.x),
        y: side(rect.y),
        width: side(rect.width),
        height: side(rect.height),
    }
}

fn tile_rect(rect: PixelRect) -> TileRect {
    let value = |v: usize| u32::try_from(v).unwrap_or(u32::MAX);
    TileRect {
        x: value(rect.x),
        y: value(rect.y),
        width: value(rect.width),
        height: value(rect.height),
    }
}

fn area(rect: TileRect) -> u64 {
    u64::from(rect.width) * u64::from(rect.height)
}

fn within(outer: TileRect, inner: TileRect) -> bool {
    outer.x <= inner.x
        && outer.y <= inner.y
        && outer.x + outer.width >= inner.x + inner.width
        && outer.y + outer.height >= inner.y + inner.height
}

fn overlaps(a: PixelRect, b: PixelRect) -> bool {
    a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
}

impl Cmd {
    /// Encoded size, an upper bound.
    fn bytes(&self, payloads: &[Vec<u8>], aus: &[Vec<u8>]) -> usize {
        6 + match self {
            Self::Fill { .. } | Self::Copy { .. } | Self::CacheStore { .. } => 12,
            Self::Tile(index) => 12 + payloads[*index].len(),
            Self::CachePaint { .. } | Self::Open { .. } => 8,
            Self::CacheEvict { .. } => 4,
            Self::Close { .. } => 1,
            Self::Paint { blocks, au, .. } => 16 + blocks.len() * 8 + aus[*au].len(),
        }
    }

    /// Area written, as the state counts it.
    fn area(&self, rects: &[TileRect]) -> u64 {
        match self {
            Self::Fill { rect, .. } | Self::CacheStore { rect, .. } => area(*rect),
            Self::Copy { src, .. } => area(*src),
            Self::Tile(index) => area(rects[*index]),
            Self::CachePaint { .. } => u64::from(CACHE_CELL * CACHE_CELL),
            Self::Paint { blocks, .. } => blocks.len() as u64 * u64::from(BLOCK_U32 * BLOCK_U32),
            Self::CacheEvict { .. } | Self::Open { .. } | Self::Close { .. } => 0,
        }
    }

    fn command<'a>(
        &self,
        payloads: &'a [Vec<u8>],
        rects: &[TileRect],
        aus: &'a [Vec<u8>],
    ) -> CanvasCommand<'a> {
        match self {
            Self::Fill { rect, rgb } => CanvasCommand::Fill {
                rect: canvas_rect(*rect),
                rgb: *rgb,
            },
            Self::Copy { src, dst_x, dst_y } => CanvasCommand::Copy {
                src: canvas_rect(*src),
                dst_x: side(*dst_x),
                dst_y: side(*dst_y),
            },
            Self::Tile(index) => CanvasCommand::Tile(arcen_protocol::canvas::CanvasTile {
                rect: canvas_rect(rects[*index]),
                codec: arcen_protocol::refine::CODEC_DELTA_ZSTD1,
                depth: super::CANVAS_TILE_DEPTH,
                payload: &payloads[*index],
            }),
            Self::CacheStore { slot, rect } => CanvasCommand::CacheStore {
                slot: *slot,
                rect: canvas_rect(*rect),
            },
            Self::CachePaint { slot, x, y } => CanvasCommand::CachePaint {
                slot: *slot,
                dst_x: side(*x),
                dst_y: side(*y),
            },
            Self::CacheEvict { slot } => CanvasCommand::CacheEvict { slot: *slot },
            Self::Open { id, width, height } => CanvasCommand::SurfaceOpen {
                surface: *id,
                codec: SURFACE_CODEC_H264,
                width: side(*width),
                height: side(*height),
            },
            Self::Close { id } => CanvasCommand::SurfaceClose { surface: *id },
            Self::Paint {
                id,
                idr,
                dest,
                blocks,
                au,
            } => CanvasCommand::SurfacePaint(SurfacePaint {
                surface: *id,
                idr: *idr,
                dest: canvas_rect(*dest),
                blocks: BlockRuns::from_sorted_indices(blocks.iter().copied()),
                access_unit: &aus[*au],
            }),
        }
    }
}

/// The planner of one monitor's canvas.
#[derive(Debug)]
pub struct CanvasPlanner {
    config: PlannerConfig,
    keel_grid: BlockGrid,
    grid: RefineGrid,
    tracker: DamageTracker,
    history: BlockHistory,
    traits: BlockTraitsMap,
    estimator: MotionEstimator,
    classes: BlockClassMap,
    regions: MotionRegions,
    ledger: TileLedger,
    mirror: CanvasMirror,
    terms: CanvasTerms,
    monitor_id: u16,
    topology_generation: u64,
    stream_epoch: u64,
    generation: u32,
    surfaces: SurfaceSet,
    pending: Vec<bool>,
    pending_since: Vec<u64>,
    fence: u64,
    tick: u64,
    focus: bool,
    input: Option<(u32, u32, Duration)>,
    full_motion: bool,
    full_motion_since: Option<Duration>,
    last_sent: Option<Duration>,
    reset_reason: Option<CanvasResetReason>,
    staged: Option<Staged>,
}

impl CanvasPlanner {
    /// A planner for a `width × height` monitor under the Deck's terms. Its
    /// first tick starts generation 1 (reason `start`).
    ///
    /// # Errors
    ///
    /// [`PlannerError::CanvasTooLarge`] or [`PlannerError::Keel`].
    pub fn new(
        monitor_id: u16,
        topology_generation: u64,
        stream_epoch: u64,
        width: u32,
        height: u32,
        terms: CanvasTerms,
        config: PlannerConfig,
    ) -> Result<Self, PlannerError> {
        if width > terms.max_canvas_px.min(MAX_CANVAS_SIDE)
            || height > terms.max_canvas_px.min(MAX_CANVAS_SIDE)
        {
            return Err(PlannerError::CanvasTooLarge);
        }
        let (w, h) = (width as usize, height as usize);
        let mut tracker = DamageTracker::new(w, h, config.kernel)?;
        tracker.retain_row_hashes();
        let keel_grid = tracker.grid();
        let grid = RefineGrid { width, height };
        let blocks = keel_grid.block_count();
        Ok(Self {
            config,
            keel_grid,
            grid,
            tracker,
            history: BlockHistory::new(keel_grid),
            traits: BlockTraitsMap::new(keel_grid),
            estimator: MotionEstimator::new(keel_grid, config.motion),
            classes: BlockClassMap::new(keel_grid, config.classes),
            regions: MotionRegions::new(keel_grid, config.regions),
            ledger: TileLedger::new(terms.cache_bytes, terms.cache_slots),
            mirror: CanvasMirror::new(monitor_id, terms),
            terms,
            monitor_id,
            topology_generation,
            stream_epoch,
            generation: 0,
            surfaces: SurfaceSet::new(terms.max_surfaces_per_monitor),
            pending: vec![false; blocks],
            pending_since: vec![0; blocks],
            fence: 0,
            tick: 0,
            focus: false,
            input: None,
            full_motion: false,
            full_motion_since: None,
            last_sent: None,
            reset_reason: Some(CanvasResetReason::Start),
            staged: None,
        })
    }

    /// The monitor.
    #[must_use]
    pub const fn monitor_id(&self) -> u16 {
        self.monitor_id
    }

    /// The mirror: what the Deck shows.
    #[must_use]
    pub const fn mirror(&self) -> &CanvasMirror {
        &self.mirror
    }

    /// The open surfaces.
    #[must_use]
    pub const fn surfaces(&self) -> &SurfaceSet {
        &self.surfaces
    }

    /// Keel's block classes.
    #[must_use]
    pub const fn classes(&self) -> &BlockClassMap {
        &self.classes
    }

    /// The current generation (0 before the first tick).
    #[must_use]
    pub const fn generation(&self) -> u32 {
        self.generation
    }

    /// Whether the whole-monitor surface (rule 7) is on.
    #[must_use]
    pub const fn full_motion(&self) -> bool {
        self.full_motion
    }

    /// Changed blocks not painted yet.
    #[must_use]
    pub fn pending_blocks(&self) -> u32 {
        u32::try_from(self.pending.iter().filter(|pending| **pending).count()).unwrap_or(u32::MAX)
    }

    /// Whether block `index` changed and is not painted yet.
    #[must_use]
    pub fn is_pending(&self, index: u32) -> bool {
        self.pending.get(index as usize).copied().unwrap_or(false)
    }

    /// Whether the monitor has input focus (it is served first).
    pub fn set_focus(&mut self, focus: bool) {
        self.focus = focus;
    }

    /// The pointer or a key acted at `(x, y)`: blocks around it become
    /// mandatory for [`PlannerConfig::input_window`].
    pub fn note_input(&mut self, x: u32, y: u32, now: Duration) {
        self.input = Some((x, y, now));
    }

    /// Whether the next tick starts a new generation.
    #[must_use]
    pub const fn reset_pending(&self) -> bool {
        self.reset_reason.is_some()
    }

    /// Starts a new generation at the next tick (a Deck's
    /// `canvas_reset_request`, or recovery after records were lost).
    pub fn request_reset(&mut self, reason: CanvasResetReason) {
        self.reset_reason = Some(reason);
    }

    fn scope(&self) -> Option<CanvasScope> {
        Some(CanvasScope {
            monitor_id: self.monitor_id,
            topology_generation: self.topology_generation,
            stream_epoch: self.stream_epoch,
            generation: NonZeroU32::new(self.generation)?,
        })
    }

    fn block_rect(&self, index: u32) -> TileRect {
        self.grid.block_rect(index).unwrap_or(EMPTY_RECT)
    }

    fn block_pixels(&self, blocks: &[u32]) -> u64 {
        blocks
            .iter()
            .map(|index| area(self.block_rect(*index)))
            .sum()
    }

    fn near_input(&self, rect: TileRect, now: Duration) -> bool {
        let Some((x, y, at)) = self.input else {
            return false;
        };
        if now.saturating_sub(at) > self.config.input_window {
            return false;
        }
        let dx = rect
            .x
            .saturating_sub(x)
            .max(x.saturating_sub(rect.x + rect.width));
        let dy = rect
            .y
            .saturating_sub(y)
            .max(y.saturating_sub(rect.y + rect.height));
        u64::from(dx).pow(2) + u64::from(dy).pow(2) <= u64::from(self.config.input_radius_px).pow(2)
    }

    /// Starts the next generation: the mirror and every receiver-side
    /// state start over, and every block is mandatory.
    fn start_generation(
        &mut self,
        reason: CanvasResetReason,
        staged: &mut Staged,
    ) -> Result<(), PlannerError> {
        self.generation = self.generation.wrapping_add(1).max(1);
        let scope = self.scope().ok_or(PlannerError::Geometry)?;
        let reset = CanvasReset {
            scope,
            reason,
            format: CanvasPixelFormat::Bgra8,
            width: self.grid.width,
            height: self.grid.height,
        };
        let bytes = reset.encode().map_err(PlannerError::Record)?;
        self.mirror
            .apply(
                &CanvasRecord::Reset(reset),
                &mut PackedTileCodes::default(),
                &mut NoSurfacePixels,
            )
            .map_err(PlannerError::State)?;
        staged.closes_for_reset = self.surfaces.slots().map(|slot| slot.id).collect();
        staged.reset = Some(bytes);
        self.surfaces.clear();
        self.ledger.clear();
        self.history.invalidate_paint();
        self.classes.revisit_all();
        self.fence = 0;
        for (pending, since) in self.pending.iter_mut().zip(&mut self.pending_since) {
            if !*pending {
                *pending = true;
                *since = self.tick;
            }
        }
        Ok(())
    }

    /// Runs Keel over the monitor's current picture and decides what the
    /// tick would send. `os_damage`, when the capture reports it, narrows
    /// hashing and supplies move hints. Call [`Self::plan`] next with the
    /// same frame.
    ///
    /// # Errors
    ///
    /// [`PlannerError`]. The planner starts a new generation (reason
    /// `recovery`) at its next tick; after [`PlannerError::Geometry`] the
    /// caller builds a planner for the new size.
    pub fn observe(
        &mut self,
        frame: BgraFrame<'_>,
        os_damage: Option<&ExternalDamage>,
        now: Duration,
        costs: &CostModel,
        scale: SurfaceScale,
    ) -> Result<MonitorDemand, PlannerError> {
        let result = self.observe_tick(frame, os_damage, now, costs, scale);
        if result.is_err() {
            self.recover();
        }
        result
    }

    /// After an error the next tick starts a new generation: the mirror,
    /// Keel's paint records and the Deck may disagree.
    fn recover(&mut self) {
        self.staged = None;
        self.reset_reason = Some(CanvasResetReason::Recovery);
    }

    fn observe_tick(
        &mut self,
        frame: BgraFrame<'_>,
        os_damage: Option<&ExternalDamage>,
        now: Duration,
        costs: &CostModel,
        scale: SurfaceScale,
    ) -> Result<MonitorDemand, PlannerError> {
        if frame.grid() != self.keel_grid {
            return Err(PlannerError::Geometry);
        }
        self.staged = None;
        self.tick += 1;
        let mut staged = Staged {
            now,
            ..Staged::default()
        };
        if let Some(reason) = self.reset_reason.take() {
            self.start_generation(reason, &mut staged)?;
        }
        let resetting = staged.reset.is_some();

        let keel_started = Instant::now();
        match os_damage {
            Some(os) if !resetting => self.tracker.update_within(frame, os.damage_map())?,
            _ => self.tracker.update(frame)?,
        };
        let hashed = self.tracker.last_hashed_blocks() as u64 * u64::from(BLOCK_U32 * BLOCK_U32);
        let damage = self.tracker.damage_map();
        self.history.observe(damage, now)?;
        let mut candidates: Vec<MoveRect> = os_damage
            .map(|os| os.move_rects().to_vec())
            .unwrap_or_default();
        candidates.extend_from_slice(self.estimator.estimate(&self.tracker)?);
        let verified = if resetting {
            Vec::new()
        } else {
            self.verify_moves(frame, &candidates)
        };
        // Traits only where a copy does not reproduce the block: measuring
        // every scrolled block cost most of a 4K scroll tick.
        let copied = self.copied_blocks(&verified);
        let dirty = self.traits.update_where(frame, damage, |index| {
            !copied.get(index).copied().unwrap_or(false)
        })? as u64
            * u64::from(BLOCK_U32 * BLOCK_U32);
        self.classes.update(&self.history, damage, &verified)?;
        self.regions.update(&self.classes)?;
        for index in damage.dirty_blocks() {
            if let (Some(pending), Some(since)) = (
                self.pending.get_mut(index),
                self.pending_since.get_mut(index),
            ) {
                if !*pending {
                    *pending = true;
                    *since = self.tick;
                }
            }
        }
        let keel_elapsed = keel_started.elapsed();
        staged
            .measured
            .push((WorkKind::Hash, hashed, keel_elapsed / 2));
        staged
            .measured
            .push((WorkKind::Classify, dirty, keel_elapsed / 2));
        staged.estimate.sunk =
            costs.predict(WorkKind::Hash, hashed) + costs.predict(WorkKind::Classify, dirty);

        self.update_full_motion(now);
        let targets: Vec<(SurfaceRole, TileRect)> = if self.full_motion {
            vec![(
                SurfaceRole::FullMonitor,
                TileRect {
                    x: 0,
                    y: 0,
                    width: self.grid.width,
                    height: self.grid.height,
                },
            )]
        } else {
            self.regions
                .regions()
                .iter()
                .map(|region| (SurfaceRole::Region(region.id), tile_rect(region.rect)))
                .collect()
        };
        let bounds = TileRect {
            x: 0,
            y: 0,
            width: self.grid.width,
            height: self.grid.height,
        };
        staged.changes = self.surfaces.plan(&targets, scale, bounds);
        let mut projected = self.surfaces.clone();
        projected.apply(&staged.changes);

        self.route(frame, &verified, &projected, resetting, costs, &mut staged);

        let demand = self.demand(&staged, &projected);
        staged.copies = verified;
        self.staged = Some(staged);
        Ok(demand)
    }

    /// The monitor's demand for what [`Self::route`] staged.
    fn demand(&self, staged: &Staged, projected: &SurfaceSet) -> MonitorDemand {
        MonitorDemand {
            monitor_id: self.monitor_id,
            focus: self.focus,
            sunk: staged.estimate.sunk,
            mandatory: staged.estimate.mandatory,
            structural: staged.estimate.content,
            content: staged.lots.iter().map(|(_, cost)| *cost).sum(),
            surfaces: staged
                .surface_costs
                .iter()
                .map(|&(surface, cost)| SurfaceDemand {
                    surface,
                    cost,
                    coded_pixels: projected.get(surface).map_or(0, SurfaceSlot::coded_pixels),
                })
                .collect(),
            refine: staged.refine.iter().map(|(_, cost)| *cost).sum(),
            motion_blocks: u32::try_from(self.classes.summary().motion).unwrap_or(u32::MAX),
            picture_pixels: staged
                .surfaces
                .iter()
                .filter(|work| work.idr)
                .filter_map(|work| projected.get(work.id))
                .map(SurfaceSlot::coded_pixels)
                .chain(staged.one_shot.as_ref().map(OneShot::coded_pixels))
                .sum(),
            canvas_pixels: u64::from(self.grid.width) * u64::from(self.grid.height),
        }
    }

    fn update_full_motion(&mut self, now: Duration) {
        let blocks = self.keel_grid.block_count().max(1) as u64;
        let share = self.classes.summary().motion as u64 * 100 / blocks;
        if self.full_motion {
            if share < self.config.full_motion_exit_percent {
                self.full_motion = false;
                self.full_motion_since = None;
            }
        } else if share >= self.config.full_motion_enter_percent {
            let since = *self.full_motion_since.get_or_insert(now);
            if now.saturating_sub(since) >= self.config.full_motion_hold {
                self.full_motion = true;
            }
        } else {
            self.full_motion_since = None;
        }
    }

    /// The blocks wholly inside a verified move's destination: the copy
    /// reproduces them, so nothing needs their pixels measured.
    fn copied_blocks(&self, moves: &[MoveRect]) -> Vec<bool> {
        let mut copied = vec![false; self.keel_grid.block_count()];
        if moves.is_empty() {
            return copied;
        }
        for (index, copied) in copied.iter_mut().enumerate() {
            let Some(bounds) = self.keel_grid.block_bounds(index) else {
                continue;
            };
            *copied = moves.iter().any(|moved| {
                let rect = moved.destination;
                bounds.x >= rect.x
                    && bounds.y >= rect.y
                    && bounds.x + bounds.width <= rect.x + rect.width
                    && bounds.y + bounds.height <= rect.y + rect.height
            });
        }
        copied
    }

    /// Moves that hold exactly against the mirror, in order, whose sources
    /// hold no lossy block and are not overwritten by an earlier copy.
    fn verify_moves(&self, frame: BgraFrame<'_>, candidates: &[MoveRect]) -> Vec<MoveRect> {
        let pixels = self.mirror.pixels();
        let Ok(reference) = BgraFrame::new(
            pixels.bgra(),
            self.grid.width as usize,
            self.grid.height as usize,
            pixels.stride(),
        ) else {
            return Vec::new();
        };
        let canvas_area = area(TileRect {
            x: 0,
            y: 0,
            width: self.grid.width,
            height: self.grid.height,
        });
        let mut accepted: Vec<MoveRect> = Vec::new();
        let mut copied_area = 0_u64;
        for candidate in candidates {
            if accepted.len() >= MAX_COPIES_PER_FRAME {
                break;
            }
            let source = candidate.source();
            let destination = tile_rect(candidate.destination);
            if candidate.offset() == (0, 0)
                || accepted.contains(candidate)
                || accepted
                    .iter()
                    .any(|earlier| overlaps(earlier.destination, source))
                || copied_area + area(destination) > canvas_area
            {
                continue;
            }
            let Some(candidate) = self.trim_lossy(*candidate) else {
                continue;
            };
            if !verify_move(reference, frame, candidate) {
                continue;
            }
            copied_area += area(tile_rect(candidate.destination));
            accepted.push(candidate);
        }
        accepted
    }

    /// `moved` without the source rows that hold lossy blocks: the longest
    /// band of whole rows whose source is exact (ADR 0028 §6), or `None`
    /// when less than a block row is left.
    fn trim_lossy(&self, moved: MoveRect) -> Option<MoveRect> {
        let source = tile_rect(moved.source());
        let state = self.mirror.state();
        let cols = self.grid.cols();
        let (left, right) = (
            source.x / BLOCK_U32,
            (source.x + source.width).div_ceil(BLOCK_U32),
        );
        let bottom = source.y + source.height;
        let mut best = (source.y, 0_u32);
        let mut start = source.y;
        for row in source.y / BLOCK_U32..bottom.div_ceil(BLOCK_U32) {
            if (left..right).any(|column| state.is_lossy(row * cols + column)) {
                let end = (row * BLOCK_U32).max(source.y);
                if end > start && end - start > best.1 {
                    best = (start, end - start);
                }
                start = ((row + 1) * BLOCK_U32).min(bottom);
            }
        }
        if bottom > start && bottom - start > best.1 {
            best = (start, bottom - start);
        }
        if best.1 == source.height {
            return Some(moved);
        }
        if best.1 < BLOCK_U32 {
            return None;
        }
        let shift = (best.0 - source.y) as usize;
        Some(MoveRect {
            source_x: moved.source_x,
            source_y: best.0 as usize,
            destination: PixelRect {
                x: moved.destination.x,
                y: moved.destination.y + shift,
                width: moved.destination.width,
                height: best.1 as usize,
            },
        })
    }

    /// Rules 1–6 over the pending blocks.
    #[allow(clippy::too_many_lines)]
    fn route(
        &self,
        frame: BgraFrame<'_>,
        moves: &[MoveRect],
        surfaces: &SurfaceSet,
        resetting: bool,
        costs: &CostModel,
        staged: &mut Staged,
    ) {
        let count = self.grid.block_count();
        let now = staged.now;
        let mut done = vec![false; count as usize];

        // Rule 1: blocks wholly inside a verified copy. The band a copy
        // scrolls through (its source and destination, and the block around
        // it that a viewport's unaligned edge leaves) stays exact: motion
        // there is the revealed strip, and painting it lossily would stop
        // the next copy (lossy blocks are never copy sources).
        let bands: Vec<TileRect> = moves
            .iter()
            .map(|moved| {
                let (a, b) = (tile_rect(moved.source()), tile_rect(moved.destination));
                let x = a.x.min(b.x).saturating_sub(BLOCK_U32);
                let y = a.y.min(b.y).saturating_sub(BLOCK_U32);
                TileRect {
                    x,
                    y,
                    width: (a.x + a.width).max(b.x + b.width) + BLOCK_U32 - x,
                    height: (a.y + a.height).max(b.y + b.height) + BLOCK_U32 - y,
                }
            })
            .collect();
        for moved in moves {
            let destination = tile_rect(moved.destination);
            for index in self.grid.blocks_of(destination) {
                if within(destination, self.block_rect(index)) && !done[index as usize] {
                    done[index as usize] = true;
                    if self.pending[index as usize] {
                        staged.copied.push(index);
                    }
                }
            }
        }

        // Rule 3: whole cells, all pending, none moving.
        let cell_bytes = u64::from(CACHE_CELL * CACHE_CELL) * 4;
        let caching =
            self.config.cache && self.terms.cache_slots > 0 && self.terms.cache_bytes >= cell_bytes;
        let mut key_pixels = 0_u64;
        if caching {
            let cols = self.grid.cols();
            for cell_y in 0..self.grid.height / CACHE_CELL {
                for cell_x in 0..self.grid.width / CACHE_CELL {
                    let anchor = cell_y * CELL_BLOCKS * cols + cell_x * CELL_BLOCKS;
                    if !self.pending[anchor as usize] {
                        continue;
                    }
                    let blocks = || {
                        (0..CELL_BLOCKS * CELL_BLOCKS)
                            .map(move |at| anchor + (at / CELL_BLOCKS) * cols + at % CELL_BLOCKS)
                    };
                    let eligible = blocks().all(|index| {
                        self.pending[index as usize]
                            && !done[index as usize]
                            && self.classes.class(index as usize) != Some(BlockClass::Motion)
                    }) && !blocks().all(|index| {
                        self.traits
                            .get(index as usize)
                            .is_some_and(arcen_keel::BlockTraits::is_solid)
                    });
                    if !eligible {
                        continue;
                    }
                    let rect = PixelRect {
                        x: (cell_x * CACHE_CELL) as usize,
                        y: (cell_y * CACHE_CELL) as usize,
                        width: CACHE_CELL as usize,
                        height: CACHE_CELL as usize,
                    };
                    key_pixels += u64::from(CACHE_CELL * CACHE_CELL);
                    let Some(key) = TileKey::of(frame, rect) else {
                        continue;
                    };
                    if self.ledger.contains(key) && !resetting {
                        for index in blocks() {
                            done[index as usize] = true;
                        }
                        staged.cache_paints.push((key, anchor));
                    } else {
                        staged.cache_stores.push((key, anchor));
                    }
                }
            }
        }
        staged.estimate.sunk += costs.predict(WorkKind::Classify, key_pixels);

        // Rules 2, 4, 5 over every other pending block.
        let slots: Vec<SurfaceSlot> = surfaces.slots().copied().collect();
        let mut surface_blocks: Vec<Vec<u32>> = vec![Vec::new(); slots.len()];
        let mut images = Vec::new();
        let mut tiles = Vec::new();
        for index in 0..count {
            if !self.pending[index as usize] || done[index as usize] {
                continue;
            }
            let rect = self.block_rect(index);
            let class = self.classes.class(index as usize);
            let traits = self.traits.get(index as usize).unwrap_or_default();
            let mandatory = resetting || self.near_input(rect, now);
            let scrolled = bands
                .iter()
                .any(|band| super::state::intersect(*band, rect).is_some());
            if class == Some(BlockClass::Motion) && !scrolled {
                if let Some(slot) = slots.iter().position(|slot| within(slot.dest, rect)) {
                    surface_blocks[slot].push(index);
                    continue;
                }
            }
            if traits.is_solid() {
                staged.fills.push(index);
            } else if mandatory {
                staged.mandatory_tiles.push(index);
            } else if traits.is_measured()
                && !traits.is_low_colour()
                && class != Some(BlockClass::Motion)
                && !scrolled
            {
                images.push(index);
            } else {
                tiles.push(index);
            }
        }

        // Rule 5: a large many-colour band is one lossy picture.
        let image_pixels = self.block_pixels(&images);
        let spare = surfaces.spare_id(&[]).filter(|id| {
            staged
                .changes
                .iter()
                .all(|change| !matches!(change, SurfaceChange::Close(closed) if closed == id))
        });
        let bounds = images
            .iter()
            .map(|index| self.block_rect(*index))
            .reduce(|a, b| {
                let x = a.x.min(b.x);
                let y = a.y.min(b.y);
                TileRect {
                    x,
                    y,
                    width: (a.x + a.width).max(b.x + b.width) - x,
                    height: (a.y + a.height).max(b.y + b.height) - y,
                }
            });
        // One picture only for a compact band: scattered blocks stay tiles.
        let compact = bounds.is_some_and(|rect| area(rect) <= image_pixels * 2);
        if image_pixels > self.config.image_tile_limit_px
            && compact
            && let Some(id) = spare
            && let Some(rect) = bounds
        {
            let (width, height) = SurfaceScale::Full.coded(rect.width, rect.height);
            let coded = u64::from(width) * u64::from(height);
            staged.one_shot = Some(OneShot {
                id,
                rect,
                blocks: images,
                cost: costs.predict(WorkKind::Surface, coded),
            });
        } else {
            tiles.extend(images);
        }

        // Surfaces: a new one paints its IDR now (mandatory), the others
        // when granted. A surface opens only with something to paint: a
        // region of scrolled content (painted exactly above) opens none.
        let mut unpainted = Vec::new();
        for (slot, blocks) in slots.iter().zip(surface_blocks) {
            if blocks.is_empty() {
                if slot.awaiting_idr {
                    unpainted.push(slot.id);
                }
                continue;
            }
            let coded = slot.coded_pixels();
            let cost = costs.predict(WorkKind::Surface, coded);
            if slot.awaiting_idr {
                staged.estimate.mandatory += cost;
            } else {
                staged.surface_costs.push((slot.id, cost));
            }
            staged.surfaces.push(SurfaceWork {
                id: slot.id,
                blocks,
                idr: slot.awaiting_idr,
            });
        }
        for id in unpainted {
            let opened = staged
                .changes
                .iter()
                .position(|change| matches!(change, SurfaceChange::Open(slot) if slot.id == id));
            if let Some(at) = opened {
                let SurfaceChange::Open(slot) = staged.changes[at] else {
                    continue;
                };
                match self.surfaces.get(id) {
                    // A reopen with nothing to paint keeps the surface as
                    // it is (no IDR now; the next tick plans again).
                    Some(open) if open.role == slot.role => {
                        staged.changes.remove(at);
                    }
                    // It replaced another surface: that one still closes.
                    Some(_) => staged.changes[at] = SurfaceChange::Close(id),
                    None => {
                        staged.changes.remove(at);
                    }
                }
            }
        }

        // Costs and the content queue, oldest first.
        let tile_cost = |blocks: &[u32]| costs.predict(WorkKind::Tile, self.block_pixels(blocks));
        staged.estimate.mandatory += tile_cost(&staged.mandatory_tiles);
        let structural_pixels = self.block_pixels(&staged.fills)
            + self.block_pixels(&staged.copied)
            + staged.cache_paints.len() as u64 * u64::from(CACHE_CELL * CACHE_CELL);
        staged.estimate.content = costs.predict(WorkKind::Compose, structural_pixels);
        let mut lots: Vec<(u64, u32, Lot, Duration)> = tiles
            .iter()
            .map(|&index| {
                (
                    self.pending_since[index as usize],
                    index,
                    Lot::Tile(index),
                    tile_cost(&[index]),
                )
            })
            .collect();
        if let Some(one_shot) = &staged.one_shot {
            let since = one_shot
                .blocks
                .iter()
                .map(|index| self.pending_since[*index as usize])
                .min()
                .unwrap_or(self.tick);
            lots.push((since, one_shot.blocks[0], Lot::OneShot, one_shot.cost));
        }
        lots.sort_by_key(|&(since, index, _, _)| (since, index));
        staged.lots = lots
            .into_iter()
            .map(|(_, _, lot, cost)| (lot, cost))
            .collect();

        // Rule 6: settled lossy blocks, oldest first.
        let mut refine: Vec<(Duration, u32)> = self
            .classes
            .active_blocks()
            .filter_map(|index| {
                let block = u32::try_from(index).ok()?;
                (self.classes.class(index) == Some(BlockClass::Settled)
                    && !self.pending[index]
                    && self.mirror.state().is_lossy(block))
                .then(|| (self.history.age(index).unwrap_or_default(), block))
            })
            .collect();
        refine.sort_by_key(|&(age, index)| (std::cmp::Reverse(age), index));
        staged.refine = refine
            .into_iter()
            .map(|(_, index)| (index, tile_cost(&[index])))
            .collect();
    }

    /// Spends `grant` on what [`Self::observe`] decided: encodes the
    /// granted surfaces, packs tiles, applies the records to the mirror
    /// and returns them.
    ///
    /// # Errors
    ///
    /// [`PlannerError`]. The planner starts a new generation (reason
    /// `recovery`) at its next tick; after [`PlannerError::Geometry`] the
    /// caller builds a planner for the new size.
    pub fn plan(
        &mut self,
        frame: BgraFrame<'_>,
        grant: &MonitorGrant,
        timestamp_ms: u32,
        encoder: &mut dyn SurfaceEncoder,
        packer: &mut dyn TilePacker,
    ) -> Result<PlannedTick, PlannerError> {
        let result = self.plan_tick(frame, grant, timestamp_ms, encoder, packer);
        if let Err(error) = &result
            && *error != PlannerError::NotObserved
        {
            self.recover();
        }
        result
    }

    #[allow(clippy::too_many_lines)]
    fn plan_tick(
        &mut self,
        frame: BgraFrame<'_>,
        grant: &MonitorGrant,
        timestamp_ms: u32,
        encoder: &mut dyn SurfaceEncoder,
        packer: &mut dyn TilePacker,
    ) -> Result<PlannedTick, PlannerError> {
        if frame.grid() != self.keel_grid {
            return Err(PlannerError::Geometry);
        }
        let staged = self.staged.take().ok_or(PlannerError::NotObserved)?;
        let mut tick = PlannedTick {
            generation: self.generation,
            measured: staged.measured.clone(),
            ..PlannedTick::default()
        };
        tick.estimate.sunk = staged.estimate.sunk;
        tick.estimate.mandatory = staged.estimate.mandatory;
        tick.estimate.content = staged.estimate.content;
        if let Some(reset) = &staged.reset {
            tick.records.push(reset.clone());
            for id in &staged.closes_for_reset {
                encoder.close(*id);
            }
        }

        // Spend the content grant oldest first; at least one lot a tick
        // always goes, so nothing waits for ever.
        let mut tiles = staged.mandatory_tiles.clone();
        let mut one_shot = None;
        let mut left = grant.content;
        for (taken, (lot, cost)) in staged.lots.iter().enumerate() {
            if taken > 0 && *cost > left {
                break;
            }
            left = left.saturating_sub(*cost);
            tick.estimate.content += *cost;
            match lot {
                Lot::Tile(index) => tiles.push(*index),
                Lot::OneShot => one_shot.clone_from(&staged.one_shot),
            }
        }
        let mut refine = Vec::new();
        let mut left = grant.refine;
        for &(index, cost) in &staged.refine {
            if cost > left {
                break;
            }
            left -= cost;
            tick.estimate.refine += cost;
            refine.push(index);
        }

        let mut commands: Vec<Cmd> = Vec::new();
        for moved in &staged.copies {
            commands.push(Cmd::Copy {
                src: tile_rect(moved.source()),
                dst_x: u32::try_from(moved.destination.x).unwrap_or(u32::MAX),
                dst_y: u32::try_from(moved.destination.y).unwrap_or(u32::MAX),
            });
        }
        for change in &staged.changes {
            match *change {
                SurfaceChange::Close(id) => {
                    encoder.close(id);
                    commands.push(Cmd::Close { id });
                }
                SurfaceChange::Open(slot) => {
                    encoder
                        .open(slot.id, slot.coded_width, slot.coded_height)
                        .map_err(PlannerError::Surface)?;
                    commands.push(Cmd::Open {
                        id: slot.id,
                        width: slot.coded_width,
                        height: slot.coded_height,
                    });
                }
                SurfaceChange::Move { .. } => {}
            }
        }
        self.surfaces.apply(&staged.changes);
        for (rect, rgb) in self.coalesce_fills(frame, &staged.fills) {
            commands.push(Cmd::Fill { rect, rgb });
        }
        let mut cached = Vec::new();
        for &(key, anchor) in &staged.cache_paints {
            let rect = self.block_rect(anchor);
            let blocks = self.cell_blocks(anchor);
            if let Some(SlotId(slot)) = self.ledger.lookup(key) {
                commands.push(Cmd::CachePaint {
                    slot,
                    x: rect.x,
                    y: rect.y,
                });
                cached.extend(blocks);
            } else {
                tiles.extend(blocks);
            }
        }

        // Tiles: changed blocks first, then refinement.
        tiles.sort_unstable();
        tiles.dedup();
        let mut exact = tiles.clone();
        exact.extend(&refine);
        exact.sort_unstable();
        let source = FrameSource {
            frame,
            grid: self.grid,
        };
        let packed_at = Instant::now();
        let tiles_out = pack_tiles(&source, &exact, packer).map_err(PlannerError::Tile)?;
        tick.measured.push((
            WorkKind::Tile,
            self.block_pixels(&exact),
            packed_at.elapsed(),
        ));
        let mut rects = Vec::with_capacity(tiles_out.len());
        let mut payloads = Vec::with_capacity(tiles_out.len());
        let mut codes = Vec::with_capacity(tiles_out.len());
        for tile in tiles_out {
            commands.push(Cmd::Tile(rects.len()));
            rects.push(tile.rect);
            payloads.push(tile.payload);
            codes.push(Some(tile.codes));
        }

        // Store whole cells that went as tiles this tick.
        let mut evicted = Vec::new();
        for &(key, anchor) in &staged.cache_stores {
            let blocks = self.cell_blocks(anchor);
            let exact = |index: &u32| {
                tiles.binary_search(index).is_ok() || staged.fills.binary_search(index).is_ok()
            };
            if !blocks.iter().all(exact) {
                continue;
            }
            evicted.clear();
            let bytes = u64::from(CACHE_CELL * CACHE_CELL) * 4;
            if let Ok(arcen_keel::Inserted::Stored(SlotId(slot))) =
                self.ledger.insert(key, bytes, &mut evicted)
            {
                for SlotId(gone) in &evicted {
                    commands.push(Cmd::CacheEvict { slot: *gone });
                }
                let origin = self.block_rect(anchor);
                commands.push(Cmd::CacheStore {
                    slot,
                    rect: TileRect {
                        x: origin.x,
                        y: origin.y,
                        width: CACHE_CELL,
                        height: CACHE_CELL,
                    },
                });
            }
        }

        // Pictures: the one-shot image, then the surfaces.
        let mut aus: Vec<Vec<u8>> = Vec::new();
        let mut lossy = Vec::new();
        let encoded_at = Instant::now();
        let mut coded_pixels = 0_u64;
        // The grant's bitrate is this monitor's share of the link; every
        // picture of the tick takes its part by coded pixels.
        let painting: Vec<&SurfaceWork> = staged
            .surfaces
            .iter()
            .filter(|work| work.idr || grant.surfaces.contains(&work.id))
            .collect();
        let total_coded: u64 = painting
            .iter()
            .filter_map(|work| self.surfaces.get(work.id))
            .map(SurfaceSlot::coded_pixels)
            .chain(one_shot.as_ref().map(OneShot::coded_pixels))
            .sum();
        let share = |coded: u64| {
            let mut quality = grant.quality;
            quality.bitrate_bps = u64::try_from(
                u128::from(quality.bitrate_bps) * u128::from(coded)
                    / u128::from(total_coded.max(1)),
            )
            .unwrap_or(u64::MAX)
            .max(1);
            quality
        };
        if let Some(image) = &one_shot {
            let (width, height) = image.coded();
            encoder
                .open(image.id, width, height)
                .map_err(PlannerError::Surface)?;
            let au = encoder
                .encode(&SurfaceEncodeRequest {
                    surface: image.id,
                    source: frame,
                    rect: image.rect,
                    coded_width: width,
                    coded_height: height,
                    idr: true,
                    quality: share(image.coded_pixels()),
                })
                .map_err(PlannerError::Surface)?;
            encoder.close(image.id);
            coded_pixels += image.coded_pixels();
            commands.push(Cmd::Open {
                id: image.id,
                width,
                height,
            });
            commands.push(Cmd::Paint {
                id: image.id,
                idr: true,
                dest: image.rect,
                blocks: image.blocks.clone(),
                au: aus.len(),
            });
            commands.push(Cmd::Close { id: image.id });
            aus.push(au);
            lossy.extend(&image.blocks);
        }
        for work in painting {
            let Some(slot) = self.surfaces.get(work.id).copied() else {
                continue;
            };
            let quality = share(slot.coded_pixels());
            let au = encoder
                .encode(&SurfaceEncodeRequest {
                    surface: slot.id,
                    source: frame,
                    rect: slot.dest,
                    coded_width: slot.coded_width,
                    coded_height: slot.coded_height,
                    idr: work.idr,
                    quality,
                })
                .map_err(PlannerError::Surface)?;
            coded_pixels += slot.coded_pixels();
            if !work.idr {
                tick.carried_optional = true;
                tick.estimate.surfaces += staged
                    .surface_costs
                    .iter()
                    .find(|(id, _)| *id == work.id)
                    .map_or(Duration::ZERO, |(_, cost)| *cost);
            }
            commands.push(Cmd::Paint {
                id: slot.id,
                idr: work.idr,
                dest: slot.dest,
                blocks: work.blocks.clone(),
                au: aus.len(),
            });
            aus.push(au);
            lossy.extend(&work.blocks);
            self.surfaces.painted(slot.id);
            tick.surfaces_painted += 1;
        }
        if coded_pixels > 0 {
            tick.measured
                .push((WorkKind::Surface, coded_pixels, encoded_at.elapsed()));
        }
        if !refine.is_empty() {
            tick.carried_optional = true;
        }

        let keepalive_due = self
            .last_sent
            .is_none_or(|sent| staged.now.saturating_sub(sent) >= self.config.keepalive);
        if commands.is_empty() && (keepalive_due || staged.reset.is_some()) {
            // An empty frame: a keepalive, or a reset with nothing to paint.
            self.emit(
                Vec::new(),
                &[],
                &[],
                &[],
                &mut codes,
                timestamp_ms,
                &mut tick,
            )?;
        } else if !commands.is_empty() {
            self.emit(
                commands,
                &payloads,
                &rects,
                &aus,
                &mut codes,
                timestamp_ms,
                &mut tick,
            )?;
        }
        if !tick.records.is_empty() {
            self.last_sent = Some(staged.now);
        }

        // Bookkeeping: what the Deck shows now.
        let mut painted = Vec::new();
        let mut paint = |blocks: &[u32], kind: PaintKind, history: &mut BlockHistory| {
            history.record_paint_blocks(
                blocks.iter().map(|index| *index as usize),
                BlockPaint::new(kind, false),
            );
            painted.extend_from_slice(blocks);
        };
        paint(&staged.copied, PaintKind::Copied, &mut self.history);
        paint(&staged.fills, PaintKind::Solid, &mut self.history);
        paint(&cached, PaintKind::Cached, &mut self.history);
        paint(&exact, PaintKind::Exact, &mut self.history);
        paint(&lossy, PaintKind::Lossy, &mut self.history);
        let lossless_changed = staged
            .copied
            .iter()
            .chain(&staged.fills)
            .chain(&cached)
            .chain(&tiles);
        for &index in lossless_changed {
            if self.pending[index as usize] {
                let waited = self.tick - self.pending_since[index as usize];
                tick.exact_latency
                    .push((index, u32::try_from(waited).unwrap_or(u32::MAX)));
            }
        }
        for &index in &painted {
            self.pending[index as usize] = false;
        }
        self.classes
            .revisit(painted.iter().map(|index| *index as usize));
        tick.pending_blocks = self.pending_blocks();
        tick.fence = self.fence;
        Ok(tick)
    }

    fn cell_blocks(&self, anchor: u32) -> Vec<u32> {
        let cols = self.grid.cols();
        (0..CELL_BLOCKS)
            .flat_map(|dy| (0..CELL_BLOCKS).map(move |dx| anchor + dy * cols + dx))
            .collect()
    }

    /// Solid blocks as rectangles: runs of one colour along each block
    /// row, merged down while a run repeats exactly.
    fn coalesce_fills(&self, frame: BgraFrame<'_>, blocks: &[u32]) -> Vec<(TileRect, [u8; 3])> {
        let cols = self.grid.cols();
        let colour = |index: u32| {
            self.traits
                .get(index as usize)
                .and_then(arcen_keel::BlockTraits::solid_bgr)
                .or_else(|| {
                    let rect = self.block_rect(index);
                    let row = frame.active_row(rect.y as usize)?;
                    let at = rect.x as usize * 4;
                    Some([row[at], row[at + 1], row[at + 2]])
                })
                .map(|[b, g, r]| [r, g, b])
        };
        // (first col, last col, row, colour) runs.
        let mut runs: Vec<(u32, u32, u32, [u8; 3])> = Vec::new();
        for &index in blocks {
            let Some(rgb) = colour(index) else { continue };
            let (bx, by) = (index % cols, index / cols);
            match runs.last_mut() {
                Some(run) if run.2 == by && run.1 + 1 == bx && run.3 == rgb => run.1 = bx,
                _ => runs.push((bx, bx, by, rgb)),
            }
        }
        // (first col, last col, first row, last row, colour).
        let mut rects: Vec<(u32, u32, u32, u32, [u8; 3])> = Vec::new();
        for (x0, x1, row, rgb) in runs {
            if let Some(open) = rects
                .iter_mut()
                .rev()
                .take_while(|open| open.3 + 1 >= row)
                .find(|open| open.0 == x0 && open.1 == x1 && open.3 + 1 == row && open.4 == rgb)
            {
                open.3 = row;
            } else {
                rects.push((x0, x1, row, row, rgb));
            }
        }
        rects
            .into_iter()
            .map(|(x0, x1, y0, y1, rgb)| {
                let x = x0 * BLOCK_U32;
                let y = y0 * BLOCK_U32;
                let right = ((x1 + 1) * BLOCK_U32).min(self.grid.width);
                let bottom = ((y1 + 1) * BLOCK_U32).min(self.grid.height);
                (
                    TileRect {
                        x,
                        y,
                        width: right - x,
                        height: bottom - y,
                    },
                    rgb,
                )
            })
            .collect()
    }

    /// Splits `commands` into records within the protocol's limits, encodes
    /// them and applies them to the mirror.
    #[allow(clippy::too_many_arguments)]
    fn emit(
        &mut self,
        commands: Vec<Cmd>,
        payloads: &[Vec<u8>],
        rects: &[TileRect],
        aus: &[Vec<u8>],
        codes: &mut [Option<TileCodes>],
        timestamp_ms: u32,
        tick: &mut PlannedTick,
    ) -> Result<(), PlannerError> {
        let scope = self.scope().ok_or(PlannerError::Geometry)?;
        let block_count = canvas_block_count(self.grid.width, self.grid.height);
        let area_limit = area(TileRect {
            x: 0,
            y: 0,
            width: self.grid.width,
            height: self.grid.height,
        }) * MAX_FRAME_AREA_FACTOR;
        let mut chunks: Vec<Vec<Cmd>> = vec![Vec::new()];
        let (mut bytes, mut written) = (0_usize, 0_u64);
        for command in commands {
            let (size, covered) = (command.bytes(payloads, aus), command.area(rects));
            let last = chunks.last().map_or(0, Vec::len);
            if last > 0
                && (bytes + size > FRAME_BYTES_SOFT_LIMIT
                    || written + covered > area_limit
                    || last >= MAX_CANVAS_COMMANDS as usize)
            {
                chunks.push(Vec::new());
                (bytes, written) = (0, 0);
            }
            bytes += size;
            written += covered;
            if let Some(chunk) = chunks.last_mut() {
                chunk.push(command);
            }
        }
        let compose_at = Instant::now();
        let mut compose_pixels = 0_u64;
        for chunk in chunks {
            let mut tile_codes = PackedTileCodes::default();
            for command in &chunk {
                if let Cmd::Tile(index) = command
                    && let Some(tile) = codes.get_mut(*index).and_then(Option::take)
                {
                    tile_codes.push(tile);
                }
                compose_pixels += command.area(rects);
            }
            self.fence += 1;
            let frame = CanvasFrame {
                scope,
                fence: self.fence,
                timestamp_ms,
                block_count,
                commands: chunk
                    .iter()
                    .map(|command| command.command(payloads, rects, aus))
                    .collect(),
            };
            for command in &frame.commands {
                tick.mix.count(command);
            }
            tick.records
                .push(frame.encode().map_err(PlannerError::Record)?);
            self.mirror
                .apply(
                    &CanvasRecord::Frame(frame),
                    &mut tile_codes,
                    &mut NoSurfacePixels,
                )
                .map_err(PlannerError::State)?;
        }
        tick.measured
            .push((WorkKind::Compose, compose_pixels, compose_at.elapsed()));
        Ok(())
    }
}
