// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas state owner: validates every record against one monitor's
//! canvas and turns it into [`CanvasWork`], the ordered operations an
//! executor performs (ADR 0028 §1, §4).
//!
//! The checks here are the ones the wire decoder cannot make: rectangles
//! inside the canvas, grid alignment at its right and bottom edges, block
//! runs inside a paint's destination, the fence order, the Deck's terms
//! (canvas size, surfaces, cache slots and bytes) and the surface and cache
//! lifecycles. A record that breaks any of them leaves the state broken
//! until the next [`CanvasReset`]: the Deck then asks for one
//! (`canvas_reset_request`).
//!
//! The state also tracks which blocks hold lossy pixels (surface paints and
//! what was copied from them). Exactness, and the digest, cover only the
//! other blocks (ADR 0028 §6).

use crate::refine::tile::{BLOCK_U32, RefineGrid, TileRect};
use arcen_protocol::canvas::{
    CANVAS_BLOCK, CanvasCommand, CanvasFrame, CanvasRecord, CanvasRect, CanvasReset, CanvasScope,
    CanvasTerms, MAX_FRAME_AREA_FACTOR, MAX_SURFACES_PER_MONITOR, SurfacePaint,
};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::num::NonZeroU32;

/// Why a canvas record was refused by the state owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasStateError {
    /// The record names another monitor.
    Monitor,
    /// A frame arrived before any reset.
    NoGeneration,
    /// The state is broken by an earlier refusal; only a reset is accepted.
    Broken,
    /// The frame's scope is not the current generation's.
    Scope,
    /// The fence did not increase.
    FenceOrder,
    /// The frame's block count is not the canvas grid's.
    Grid,
    /// The reset's canvas exceeds the Deck's `max_canvas_px`.
    CanvasTooLarge,
    /// A rectangle leaves the canvas.
    Bounds,
    /// A rectangle breaks the grid rule at the canvas edges, or a block run
    /// leaves its destination.
    Alignment,
    /// A surface id at or above the Deck's surface limit.
    SurfaceLimit,
    /// A surface that is not open was painted or closed.
    SurfaceClosed,
    /// The first paint of an opened surface is not an IDR.
    NeedsIdr,
    /// A cache slot at or above the Deck's slot count.
    SlotLimit,
    /// A cache slot that holds nothing was painted or evicted.
    SlotEmpty,
    /// A store would exceed the Deck's cache bytes.
    CacheBudget,
    /// The frame writes more than [`MAX_FRAME_AREA_FACTOR`] times the
    /// canvas area.
    WorkBudget,
    /// The executor failed (a tile or surface did not decode).
    Execution,
}

impl CanvasStateError {
    /// Stable token for telemetry and `canvas_reset_request` reasons.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Monitor => "monitor",
            Self::NoGeneration => "no_generation",
            Self::Broken => "broken",
            Self::Scope => "scope",
            Self::FenceOrder => "fence_order",
            Self::Grid => "grid",
            Self::CanvasTooLarge => "canvas_too_large",
            Self::Bounds => "bounds",
            Self::Alignment => "alignment",
            Self::SurfaceLimit => "surface_limit",
            Self::SurfaceClosed => "surface_closed",
            Self::NeedsIdr => "needs_idr",
            Self::SlotLimit => "slot_limit",
            Self::SlotEmpty => "slot_empty",
            Self::CacheBudget => "cache_budget",
            Self::WorkBudget => "work_budget",
            Self::Execution => "execution",
        }
    }
}

impl Display for CanvasStateError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "canvas state refused a record: {}", self.token())
    }
}

impl std::error::Error for CanvasStateError {}

/// One operation an executor performs, in order. Rectangles are inside the
/// canvas; payloads borrow the record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasOp<'a> {
    /// Start over: a `width × height` canvas of opaque black, no surfaces,
    /// an empty cache.
    Reset {
        /// Canvas width.
        width: u32,
        /// Canvas height.
        height: u32,
    },
    /// Fill a rectangle.
    Fill {
        /// Rectangle.
        rect: TileRect,
        /// `[b, g, r, 255]`.
        bgra: [u8; 4],
    },
    /// Copy within the canvas, as if through a temporary.
    Copy {
        /// Source.
        src: TileRect,
        /// Destination left edge.
        dst_x: u32,
        /// Destination top edge.
        dst_y: u32,
    },
    /// Decode a `delta-zstd1` depth-8 tile into a rectangle.
    Tile {
        /// Rectangle.
        rect: TileRect,
        /// Compressed payload.
        payload: &'a [u8],
    },
    /// Store canvas pixels into a slot.
    CacheStore {
        /// Slot.
        slot: u32,
        /// Canvas rectangle.
        rect: TileRect,
    },
    /// Paint a slot; `rect` is the destination, the entry's size.
    CachePaint {
        /// Slot.
        slot: u32,
        /// Destination.
        rect: TileRect,
    },
    /// Forget a slot.
    CacheEvict {
        /// Slot.
        slot: u32,
    },
    /// Open (or replace) a surface decoder.
    SurfaceOpen {
        /// Surface id.
        surface: u8,
        /// Coded width.
        width: u32,
        /// Coded height.
        height: u32,
    },
    /// Close a surface decoder.
    SurfaceClose {
        /// Surface id.
        surface: u8,
    },
    /// Decode an access unit; paint `blocks` from the picture scaled into
    /// `dest`.
    SurfacePaint {
        /// Surface id.
        surface: u8,
        /// IDR picture.
        idr: bool,
        /// Where the whole picture lands.
        dest: TileRect,
        /// Canvas rectangles that take decoded pixels: whole blocks
        /// (clipped at the canvas edge), one per row span of a run.
        blocks: Vec<TileRect>,
        /// The access unit.
        access_unit: &'a [u8],
    },
}

/// The operations of one record, for one monitor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasWork<'a> {
    /// Monitor.
    pub monitor_id: u16,
    /// Canvas generation.
    pub generation: NonZeroU32,
    /// Fence (0 for a reset).
    pub fence: u64,
    /// Capture time on the wire clock (0 for a reset).
    pub timestamp_ms: u32,
    /// The operations, in order.
    pub ops: Vec<CanvasOp<'a>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Generation {
    scope: CanvasScope,
    grid: RefineGrid,
    fence: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Surface {
    awaiting_idr: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheEntry {
    width: u32,
    height: u32,
    /// Lossy flag of each entry block, row-major.
    lossy: Vec<bool>,
}

impl CacheEntry {
    fn bytes(&self) -> u64 {
        u64::from(self.width) * u64::from(self.height) * 4
    }
}

/// The interpreter of one monitor's canvas, shared by the Pier's mirror and
/// every Deck.
#[derive(Debug, Clone)]
pub struct CanvasState {
    monitor_id: u16,
    terms: CanvasTerms,
    generation: Option<Generation>,
    broken: bool,
    surfaces: [Option<Surface>; MAX_SURFACES_PER_MONITOR as usize],
    cache: BTreeMap<u32, CacheEntry>,
    cache_bytes: u64,
    lossy: Vec<bool>,
}

fn rect(rect: CanvasRect) -> TileRect {
    TileRect {
        x: u32::from(rect.x),
        y: u32::from(rect.y),
        width: u32::from(rect.width),
        height: u32::from(rect.height),
    }
}

fn inside(rect: TileRect, grid: RefineGrid) -> bool {
    rect.x
        .checked_add(rect.width)
        .is_some_and(|right| right <= grid.width)
        && rect
            .y
            .checked_add(rect.height)
            .is_some_and(|bottom| bottom <= grid.height)
}

/// Grid origin, whole blocks unless the rectangle ends at the canvas edge.
fn grid_aligned(rect: TileRect, grid: RefineGrid) -> bool {
    rect.x % BLOCK_U32 == 0
        && rect.y % BLOCK_U32 == 0
        && (rect.width % BLOCK_U32 == 0 || rect.x + rect.width == grid.width)
        && (rect.height % BLOCK_U32 == 0 || rect.y + rect.height == grid.height)
}

pub(crate) fn intersect(a: TileRect, b: TileRect) -> Option<TileRect> {
    let x = a.x.max(b.x);
    let y = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > x && bottom > y).then(|| TileRect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    })
}

/// Block indices `rect` touches, ascending.
fn touched(grid: RefineGrid, rect: TileRect) -> impl Iterator<Item = u32> {
    let cols = grid.cols();
    let (x0, y0) = (rect.x / BLOCK_U32, rect.y / BLOCK_U32);
    let x1 = (rect.x + rect.width).div_ceil(BLOCK_U32);
    let y1 = (rect.y + rect.height).div_ceil(BLOCK_U32);
    (y0..y1).flat_map(move |by| (x0..x1).map(move |bx| by * cols + bx))
}

/// Row spans of validated runs, as whole-block rectangles clipped to the
/// canvas.
fn run_spans(grid: RefineGrid, runs: &[(u32, u32)]) -> Vec<TileRect> {
    let cols = grid.cols();
    let mut spans = Vec::new();
    for &(start, len) in runs {
        let mut index = start;
        let end = start + len;
        while index < end {
            let (bx, by) = (index % cols, index / cols);
            let span = (cols - bx).min(end - index);
            let x = bx * BLOCK_U32;
            let y = by * BLOCK_U32;
            spans.push(TileRect {
                x,
                y,
                width: (span * BLOCK_U32).min(grid.width - x),
                height: BLOCK_U32.min(grid.height - y),
            });
            index += span;
        }
    }
    spans
}

impl CanvasState {
    /// A state for `monitor_id` under the terms the Deck offered.
    #[must_use]
    pub fn new(monitor_id: u16, terms: CanvasTerms) -> Self {
        Self {
            monitor_id,
            terms,
            generation: None,
            broken: false,
            surfaces: [None; MAX_SURFACES_PER_MONITOR as usize],
            cache: BTreeMap::new(),
            cache_bytes: 0,
            lossy: Vec::new(),
        }
    }

    /// Monitor.
    #[must_use]
    pub const fn monitor_id(&self) -> u16 {
        self.monitor_id
    }

    /// The canvas grid, once a reset arrived.
    #[must_use]
    pub fn grid(&self) -> Option<RefineGrid> {
        self.generation.map(|generation| generation.grid)
    }

    /// The current scope, once a reset arrived.
    #[must_use]
    pub fn scope(&self) -> Option<CanvasScope> {
        self.generation.map(|generation| generation.scope)
    }

    /// The newest applied fence (0 right after a reset).
    #[must_use]
    pub fn fence(&self) -> u64 {
        self.generation.map_or(0, |generation| generation.fence)
    }

    /// Whether only a reset is accepted.
    #[must_use]
    pub const fn needs_reset(&self) -> bool {
        self.broken || self.generation.is_none()
    }

    /// Cache bytes in use.
    #[must_use]
    pub const fn cache_bytes(&self) -> u64 {
        self.cache_bytes
    }

    /// Whether block `index` holds lossy pixels.
    #[must_use]
    pub fn is_lossy(&self, index: u32) -> bool {
        self.lossy.get(index as usize).copied().unwrap_or(false)
    }

    /// Blocks that hold exact pixels, ascending.
    pub fn lossless_blocks(&self) -> impl Iterator<Item = u32> + '_ {
        self.lossy
            .iter()
            .enumerate()
            .filter(|(_, lossy)| !**lossy)
            .filter_map(|(index, _)| u32::try_from(index).ok())
    }

    /// Breaks the state (an executor failed); only a reset is accepted next.
    pub fn mark_broken(&mut self) {
        self.broken = true;
    }

    /// Validates `record` and returns its operations. On a refusal the state
    /// is broken until the next reset, except for a record of another
    /// monitor, which never touches this state.
    ///
    /// # Errors
    ///
    /// [`CanvasStateError`].
    pub fn apply<'a>(
        &mut self,
        record: &CanvasRecord<'a>,
    ) -> Result<CanvasWork<'a>, CanvasStateError> {
        if record.scope().monitor_id != self.monitor_id {
            return Err(CanvasStateError::Monitor);
        }
        let result = match record {
            CanvasRecord::Reset(reset) => self.reset(reset),
            CanvasRecord::Frame(frame) => self.frame(frame),
        };
        if result.is_err() {
            self.broken = true;
        }
        result
    }

    fn reset<'a>(&mut self, reset: &CanvasReset) -> Result<CanvasWork<'a>, CanvasStateError> {
        if reset.width > self.terms.max_canvas_px || reset.height > self.terms.max_canvas_px {
            return Err(CanvasStateError::CanvasTooLarge);
        }
        let grid = RefineGrid {
            width: reset.width,
            height: reset.height,
        };
        self.generation = Some(Generation {
            scope: reset.scope,
            grid,
            fence: 0,
        });
        self.broken = false;
        self.surfaces = [None; MAX_SURFACES_PER_MONITOR as usize];
        self.cache.clear();
        self.cache_bytes = 0;
        self.lossy = vec![false; grid.block_count() as usize];
        Ok(CanvasWork {
            monitor_id: self.monitor_id,
            generation: reset.scope.generation,
            fence: 0,
            timestamp_ms: 0,
            ops: vec![CanvasOp::Reset {
                width: reset.width,
                height: reset.height,
            }],
        })
    }

    fn frame<'a>(&mut self, frame: &CanvasFrame<'a>) -> Result<CanvasWork<'a>, CanvasStateError> {
        if self.broken {
            return Err(CanvasStateError::Broken);
        }
        let generation = self.generation.ok_or(CanvasStateError::NoGeneration)?;
        if frame.scope != generation.scope {
            return Err(CanvasStateError::Scope);
        }
        if frame.fence <= generation.fence {
            return Err(CanvasStateError::FenceOrder);
        }
        if frame.block_count != generation.grid.block_count() {
            return Err(CanvasStateError::Grid);
        }
        let grid = generation.grid;
        // Counted before each command's work, so a refused frame did at most
        // the budget's worth.
        let budget = MAX_FRAME_AREA_FACTOR * u64::from(grid.width) * u64::from(grid.height);
        let mut area = 0_u64;
        let mut ops = Vec::with_capacity(frame.commands.len());
        for command in &frame.commands {
            area = area.saturating_add(self.area(command));
            if area > budget {
                return Err(CanvasStateError::WorkBudget);
            }
            ops.push(self.command(grid, command)?);
        }
        if let Some(generation) = self.generation.as_mut() {
            generation.fence = frame.fence;
        }
        Ok(CanvasWork {
            monitor_id: self.monitor_id,
            generation: frame.scope.generation,
            fence: frame.fence,
            timestamp_ms: frame.timestamp_ms,
            ops,
        })
    }

    /// Canvas pixels `command` writes.
    fn area(&self, command: &CanvasCommand<'_>) -> u64 {
        let pixels = |rect: CanvasRect| u64::from(rect.width) * u64::from(rect.height);
        match command {
            CanvasCommand::Fill { rect, .. } | CanvasCommand::CacheStore { rect, .. } => {
                pixels(*rect)
            }
            CanvasCommand::Copy { src, .. } => pixels(*src),
            CanvasCommand::Tile(tile) => pixels(tile.rect),
            CanvasCommand::CachePaint { slot, .. } => self
                .cache
                .get(slot)
                .map_or(0, |entry| u64::from(entry.width) * u64::from(entry.height)),
            CanvasCommand::SurfacePaint(paint) => {
                paint.blocks.count() * u64::from(CANVAS_BLOCK) * u64::from(CANVAS_BLOCK)
            }
            CanvasCommand::CacheEvict { .. }
            | CanvasCommand::SurfaceOpen { .. }
            | CanvasCommand::SurfaceClose { .. } => 0,
        }
    }

    fn command<'a>(
        &mut self,
        grid: RefineGrid,
        command: &CanvasCommand<'a>,
    ) -> Result<CanvasOp<'a>, CanvasStateError> {
        let within = |area: TileRect| {
            if inside(area, grid) {
                Ok(area)
            } else {
                Err(CanvasStateError::Bounds)
            }
        };
        let aligned = |area: TileRect| {
            within(area).and_then(|area| {
                if grid_aligned(area, grid) {
                    Ok(area)
                } else {
                    Err(CanvasStateError::Alignment)
                }
            })
        };
        Ok(match command {
            CanvasCommand::Fill { rect: area, rgb } => {
                let area = within(rect(*area))?;
                self.mark(grid, area, false);
                CanvasOp::Fill {
                    rect: area,
                    bgra: [rgb[2], rgb[1], rgb[0], 255],
                }
            }
            CanvasCommand::Copy { src, dst_x, dst_y } => {
                let src = within(rect(*src))?;
                let dst = within(TileRect {
                    x: u32::from(*dst_x),
                    y: u32::from(*dst_y),
                    ..src
                })?;
                self.mark_copy(grid, src, dst);
                CanvasOp::Copy {
                    src,
                    dst_x: dst.x,
                    dst_y: dst.y,
                }
            }
            CanvasCommand::Tile(tile) => {
                let area = aligned(rect(tile.rect))?;
                self.mark(grid, area, false);
                CanvasOp::Tile {
                    rect: area,
                    payload: tile.payload,
                }
            }
            CanvasCommand::CacheStore { slot, rect: area } => {
                let area = aligned(rect(*area))?;
                self.store(grid, *slot, area)?;
                CanvasOp::CacheStore {
                    slot: *slot,
                    rect: area,
                }
            }
            CanvasCommand::CachePaint { slot, dst_x, dst_y } => {
                let entry = self.slot(*slot)?;
                let area = within(TileRect {
                    x: u32::from(*dst_x),
                    y: u32::from(*dst_y),
                    width: entry.width,
                    height: entry.height,
                })?;
                self.paint_entry(grid, *slot, area);
                CanvasOp::CachePaint {
                    slot: *slot,
                    rect: area,
                }
            }
            CanvasCommand::CacheEvict { slot } => {
                let bytes = self.slot(*slot)?.bytes();
                self.cache_bytes -= bytes;
                self.cache.remove(slot);
                CanvasOp::CacheEvict { slot: *slot }
            }
            CanvasCommand::SurfaceOpen {
                surface,
                width,
                height,
                ..
            } => {
                *self.surface_slot(*surface)? = Some(Surface { awaiting_idr: true });
                CanvasOp::SurfaceOpen {
                    surface: *surface,
                    width: u32::from(*width),
                    height: u32::from(*height),
                }
            }
            CanvasCommand::SurfaceClose { surface } => {
                if self.surface_slot(*surface)?.take().is_none() {
                    return Err(CanvasStateError::SurfaceClosed);
                }
                CanvasOp::SurfaceClose { surface: *surface }
            }
            CanvasCommand::SurfacePaint(paint) => self.surface_paint(grid, paint)?,
        })
    }

    fn surface_slot(&mut self, surface: u8) -> Result<&mut Option<Surface>, CanvasStateError> {
        if surface >= self.terms.max_surfaces_per_monitor {
            return Err(CanvasStateError::SurfaceLimit);
        }
        self.surfaces
            .get_mut(usize::from(surface))
            .ok_or(CanvasStateError::SurfaceLimit)
    }

    fn surface_paint<'a>(
        &mut self,
        grid: RefineGrid,
        paint: &SurfacePaint<'a>,
    ) -> Result<CanvasOp<'a>, CanvasStateError> {
        let dest = rect(paint.dest);
        if !inside(dest, grid) {
            return Err(CanvasStateError::Bounds);
        }
        if !grid_aligned(dest, grid) {
            return Err(CanvasStateError::Alignment);
        }
        let blocks = run_spans(grid, paint.blocks.runs());
        if blocks
            .iter()
            .any(|span| intersect(*span, dest) != Some(*span))
        {
            return Err(CanvasStateError::Alignment);
        }
        let surface = self
            .surface_slot(paint.surface)?
            .as_mut()
            .ok_or(CanvasStateError::SurfaceClosed)?;
        if surface.awaiting_idr && !paint.idr {
            return Err(CanvasStateError::NeedsIdr);
        }
        surface.awaiting_idr = false;
        for span in &blocks {
            self.mark(grid, *span, true);
        }
        Ok(CanvasOp::SurfacePaint {
            surface: paint.surface,
            idr: paint.idr,
            dest,
            blocks,
            access_unit: paint.access_unit,
        })
    }

    fn slot(&self, slot: u32) -> Result<&CacheEntry, CanvasStateError> {
        if slot >= self.terms.cache_slots {
            return Err(CanvasStateError::SlotLimit);
        }
        self.cache.get(&slot).ok_or(CanvasStateError::SlotEmpty)
    }

    fn store(
        &mut self,
        grid: RefineGrid,
        slot: u32,
        area: TileRect,
    ) -> Result<(), CanvasStateError> {
        if slot >= self.terms.cache_slots {
            return Err(CanvasStateError::SlotLimit);
        }
        let entry_grid = RefineGrid {
            width: area.width,
            height: area.height,
        };
        let cols = grid.cols();
        let (bx, by) = (area.x / BLOCK_U32, area.y / BLOCK_U32);
        let lossy = (0..entry_grid.rows())
            .flat_map(|row| (0..entry_grid.cols()).map(move |col| (by + row) * cols + bx + col))
            .map(|index| self.is_lossy(index))
            .collect();
        let entry = CacheEntry {
            width: area.width,
            height: area.height,
            lossy,
        };
        let freed = self.cache.get(&slot).map_or(0, CacheEntry::bytes);
        let total = self.cache_bytes - freed + entry.bytes();
        if total > self.terms.cache_bytes {
            return Err(CanvasStateError::CacheBudget);
        }
        self.cache_bytes = total;
        self.cache.insert(slot, entry);
        Ok(())
    }

    fn paint_entry(&mut self, grid: RefineGrid, slot: u32, area: TileRect) {
        let Some(entry) = self.cache.get(&slot) else {
            return;
        };
        let entry_grid = RefineGrid {
            width: entry.width,
            height: entry.height,
        };
        let marks: Vec<(TileRect, bool)> = (0..entry_grid.block_count())
            .filter_map(|index| {
                let block = entry_grid.block_rect(index)?;
                let lossy = entry.lossy.get(index as usize).copied().unwrap_or(true);
                Some((
                    TileRect {
                        x: area.x + block.x,
                        y: area.y + block.y,
                        ..block
                    },
                    lossy,
                ))
            })
            .collect();
        for (target, lossy) in marks {
            self.mark(grid, target, lossy);
        }
    }

    /// Sets the lossy flag of every block `area` covers whole, and adds it
    /// to every block it covers in part (the rest of that block keeps what
    /// it held).
    fn mark(&mut self, grid: RefineGrid, area: TileRect, lossy: bool) {
        for index in touched(grid, area) {
            let Some(block) = grid.block_rect(index) else {
                continue;
            };
            let whole = intersect(block, area) == Some(block);
            if let Some(flag) = self.lossy.get_mut(index as usize) {
                *flag = if whole { lossy } else { *flag || lossy };
            }
        }
    }

    /// A copy moves lossiness with the pixels: each destination block takes
    /// the flags of the source blocks its pre-image touches (read before the
    /// copy, as the pixels are).
    fn mark_copy(&mut self, grid: RefineGrid, src: TileRect, dst: TileRect) {
        let marks: Vec<(TileRect, bool)> = touched(grid, dst)
            .filter_map(|index| {
                let part = intersect(grid.block_rect(index)?, dst)?;
                let pre_image = TileRect {
                    x: part.x - dst.x + src.x,
                    y: part.y - dst.y + src.y,
                    ..part
                };
                let lossy = touched(grid, pre_image).any(|source| self.is_lossy(source));
                Some((part, lossy))
            })
            .collect();
        for (part, lossy) in marks {
            self.mark(grid, part, lossy);
        }
    }
}
