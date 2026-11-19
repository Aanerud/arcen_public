// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Canvas records (ADR 0028): damage-first software streaming.
//!
//! A Deck that supports it keeps one persistent eight-bit canvas per monitor
//! and applies ordered canvas commands to it; the Pier keeps an exact mirror
//! of the same canvas. Negotiated per session: the Deck sends
//! [`CanvasCapability`] in its `AuthResponse` (`canvas_v1`), the Pier
//! answers with [`CanvasStatusMsg`] in its `ServerHelloMsg` (`canvas`). A
//! Pier sends canvas records only to a Deck that asked, and only for a
//! monitor its hello reports `on`.
//!
//! One binary frame type, [`FrameType::Canvas`] (`0x0E`), on the video lane,
//! with two record kinds: [`CanvasReset`], the ordered barrier that starts a
//! canvas generation, and [`CanvasFrame`], one ordered list of
//! [`CanvasCommand`]s under a fence. Deck → Pier messages
//! ([`CanvasControlMsg`]) are session JSON on the control lane.
//!
//! Headers are big-endian, as in `crate::refine`. Every decoder checks
//! lengths and limits with checked arithmetic **before** it allocates
//! anything sized by the peer. Geometry that needs the canvas size (a rect
//! inside the canvas, a run inside a destination) is checked by the state
//! owner, which knows the size from the generation's reset.

use crate::bytes::{ByteReader, Short};
use crate::refine::{
    check_block_count, parse_runs, put_runs, BlockRuns, RefineRecordError, RefineScope,
    CODEC_DELTA_ZSTD1, CODEC_DELTA_ZSTD1_TOKEN, MAX_TILE_EXPANDED_BYTES, MAX_TILE_HEIGHT,
    MAX_TILE_PAYLOAD_BYTES, MAX_TILE_WIDTH, REFINE_BLOCK, REFINE_RUN_BYTES,
};
use crate::wire::FrameType;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};

/// Record format version carried in byte 1 of every canvas record.
pub const CANVAS_RECORD_VERSION: u8 = 1;
/// Bytes of the common prefix (type, version, kind, reserved, monitor,
/// topology generation, stream epoch, canvas generation).
pub const CANVAS_PREFIX_BYTES: usize = 26;
/// Bytes of a [`CanvasReset`].
pub const CANVAS_RESET_BYTES: usize = CANVAS_PREFIX_BYTES + 12;
/// Fixed bytes of a [`CanvasFrame`] before its commands.
pub const CANVAS_FRAME_FIXED_BYTES: usize = CANVAS_PREFIX_BYTES + 20;
/// Bytes of one command's type-length header.
pub const CANVAS_COMMAND_HEADER_BYTES: usize = 6;
/// Side of a canvas block (Keel's grid, refine's block).
pub const CANVAS_BLOCK: u16 = REFINE_BLOCK;
/// Largest canvas side in pixels (the largest texture both Decks create).
pub const MAX_CANVAS_SIDE: u32 = 16_384;
/// Largest canvas record in bytes. A repaint larger than this spans frames.
pub const MAX_CANVAS_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Largest number of commands in one frame.
pub const MAX_CANVAS_COMMANDS: u32 = 65_536;
/// Protocol bound on surfaces per monitor; a Deck advertises its own lower
/// limit in [`CanvasCapability::max_surfaces_per_monitor`].
pub const MAX_SURFACES_PER_MONITOR: u8 = 4;
/// Largest surface side in pixels.
pub const MAX_SURFACE_SIDE: u16 = 8_192;
/// Largest surface area in pixels (H.264 level 6.2: 139,264 macroblocks).
pub const MAX_SURFACE_PIXELS: u32 = 139_264 * 256;
/// Largest access unit one `SurfacePaint` carries.
pub const MAX_SURFACE_AU_BYTES: usize = 8 * 1024 * 1024;
/// Protocol bound on cache slots; a Deck advertises its own.
pub const MAX_CACHE_SLOTS: u32 = 65_536;
/// Protocol bound on the cache budget a Deck may advertise.
pub const MAX_CACHE_BYTES: u64 = 1024 * 1024 * 1024;
/// Largest number of monitors a status may list.
pub const MAX_CANVAS_MONITORS: usize = 16;
/// Longest reason token in a status or a control message.
pub const MAX_CANVAS_REASON_BYTES: usize = 32;
/// Most canvas area one frame's commands may write, as a multiple of the
/// canvas area: the pixels of every `Fill`, `Copy`, `Tile`, `CacheStore`
/// and `CachePaint` rectangle and every block a `SurfacePaint` lists. The
/// state owner refuses a frame above it, so the work a record causes stays
/// proportional to the canvas, not to its command count.
pub const MAX_FRAME_AREA_FACTOR: u64 = 4;
/// Bit depth of every canvas v1 tile (eight-bit RGB codes).
pub const CANVAS_TILE_DEPTH: u8 = 8;
/// Surface codec id: H.264 Constrained Baseline, 4:2:0, eight bits, Annex-B.
pub const SURFACE_CODEC_H264: u8 = 1;
/// `SurfacePaint` flag: the access unit is an IDR picture.
pub const SURFACE_PAINT_IDR: u8 = 0x01;

/// Why a canvas record or message was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasRecordError {
    /// Shorter than its fixed header.
    Short,
    /// Wrong frame type byte.
    WrongType(u8),
    /// Unknown record version.
    Version(u8),
    /// Unknown record kind.
    Kind(u8),
    /// A reserved field is not zero, or a flag is unknown.
    Reserved,
    /// A zero canvas generation.
    ZeroGeneration,
    /// A zero fence.
    ZeroFence,
    /// Bytes after the declared end of the record or a command.
    TrailingBytes,
    /// The record exceeds [`MAX_CANVAS_FRAME_BYTES`].
    FrameTooLarge,
    /// Command count is above [`MAX_CANVAS_COMMANDS`] or more than the
    /// record can hold.
    CommandCount,
    /// A command's length does not fit its type or the record.
    CommandLength,
    /// Unknown command type.
    UnknownCommand(u8),
    /// A rectangle is empty, unaligned, oversized or outside the canvas
    /// bounds.
    Geometry,
    /// Tile payload empty or above [`MAX_TILE_PAYLOAD_BYTES`].
    PayloadTooLarge,
    /// Expanded tile size above [`MAX_TILE_EXPANDED_BYTES`].
    ExpandedTooLarge,
    /// Unknown reason, format, depth or codec value.
    Value,
    /// Runs are overlapping, unordered or outside the grid.
    Runs,
    /// Block count is zero or above the grid limit.
    BlockCount,
    /// Surface id above the protocol bound.
    Surface,
    /// Cache slot above the protocol bound.
    Slot,
    /// An empty access unit, or one above [`MAX_SURFACE_AU_BYTES`].
    AccessUnit,
    /// A status or control message breaks its bounds.
    Message,
}

impl CanvasRecordError {
    /// Stable token for telemetry.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::WrongType(_) => "wrong_type",
            Self::Version(_) => "version",
            Self::Kind(_) => "kind",
            Self::Reserved => "reserved",
            Self::ZeroGeneration => "zero_generation",
            Self::ZeroFence => "zero_fence",
            Self::TrailingBytes => "trailing_bytes",
            Self::FrameTooLarge => "frame_too_large",
            Self::CommandCount => "command_count",
            Self::CommandLength => "command_length",
            Self::UnknownCommand(_) => "unknown_command",
            Self::Geometry => "geometry",
            Self::PayloadTooLarge => "payload_too_large",
            Self::ExpandedTooLarge => "expanded_too_large",
            Self::Value => "value",
            Self::Runs => "runs",
            Self::BlockCount => "block_count",
            Self::Surface => "surface",
            Self::Slot => "slot",
            Self::AccessUnit => "access_unit",
            Self::Message => "message",
        }
    }
}

impl Display for CanvasRecordError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "canvas record refused: {}", self.token())
    }
}

impl std::error::Error for CanvasRecordError {}

impl From<Short> for CanvasRecordError {
    fn from(_: Short) -> Self {
        Self::Short
    }
}

impl From<RefineRecordError> for CanvasRecordError {
    fn from(error: RefineRecordError) -> Self {
        match error {
            RefineRecordError::Short => Self::Short,
            RefineRecordError::TrailingBytes => Self::TrailingBytes,
            RefineRecordError::BlockCount => Self::BlockCount,
            _ => Self::Runs,
        }
    }
}

/// Identity every canvas record carries: the monitor, its applied topology
/// generation, its stream epoch and the **canvas** generation (started by a
/// [`CanvasReset`]). The same shape as refinement's scope.
pub type CanvasScope = RefineScope;

/// Blocks in a canvas of `width × height` pixels.
#[must_use]
pub fn canvas_block_count(width: u32, height: u32) -> u32 {
    let block = u32::from(CANVAS_BLOCK);
    width.div_ceil(block).saturating_mul(height.div_ceil(block))
}

/// A rectangle in canvas pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CanvasRect {
    /// Left edge.
    pub x: u16,
    /// Top edge.
    pub y: u16,
    /// Width.
    pub width: u16,
    /// Height.
    pub height: u16,
}

impl CanvasRect {
    /// Right edge (exclusive).
    #[must_use]
    pub const fn right(self) -> u32 {
        self.x as u32 + self.width as u32
    }

    /// Bottom edge (exclusive).
    #[must_use]
    pub const fn bottom(self) -> u32 {
        self.y as u32 + self.height as u32
    }

    /// Whether the rectangle lies inside a canvas of `width × height`.
    #[must_use]
    pub const fn inside(self, width: u32, height: u32) -> bool {
        self.right() <= width && self.bottom() <= height
    }

    /// Whether the rectangle is grid-aligned in a canvas of
    /// `width × height`: its origin on the grid and each size a whole number
    /// of blocks unless it ends at the canvas's right or bottom edge.
    #[must_use]
    pub const fn grid_aligned(self, width: u32, height: u32) -> bool {
        let block = CANVAS_BLOCK;
        self.x % block == 0
            && self.y % block == 0
            && (self.width % block == 0 || self.right() == width)
            && (self.height % block == 0 || self.bottom() == height)
    }

    fn check_nonempty(self) -> Result<(), CanvasRecordError> {
        if self.width == 0
            || self.height == 0
            || self.right() > MAX_CANVAS_SIDE
            || self.bottom() > MAX_CANVAS_SIDE
        {
            Err(CanvasRecordError::Geometry)
        } else {
            Ok(())
        }
    }

    fn check_origin_aligned(self) -> Result<(), CanvasRecordError> {
        self.check_nonempty()?;
        if self.x % CANVAS_BLOCK != 0 || self.y % CANVAS_BLOCK != 0 {
            Err(CanvasRecordError::Geometry)
        } else {
            Ok(())
        }
    }

    /// Tile and cache-entry rules: grid origin and refine's per-tile limits.
    fn check_tile(self) -> Result<(), CanvasRecordError> {
        self.check_origin_aligned()?;
        if self.width > MAX_TILE_WIDTH || self.height > MAX_TILE_HEIGHT {
            return Err(CanvasRecordError::Geometry);
        }
        Ok(())
    }

    fn put(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x.to_be_bytes());
        out.extend_from_slice(&self.y.to_be_bytes());
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
    }

    fn read(reader: &mut ByteReader<'_>) -> Result<Self, Short> {
        Ok(Self {
            x: reader.u16()?,
            y: reader.u16()?,
            width: reader.u16()?,
            height: reader.u16()?,
        })
    }
}

fn check_point(x: u16, y: u16, aligned: bool) -> Result<(), CanvasRecordError> {
    if u32::from(x) >= MAX_CANVAS_SIDE
        || u32::from(y) >= MAX_CANVAS_SIDE
        || (aligned && (x % CANVAS_BLOCK != 0 || y % CANVAS_BLOCK != 0))
    {
        Err(CanvasRecordError::Geometry)
    } else {
        Ok(())
    }
}

/// Why a canvas generation started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasResetReason {
    /// First generation of a monitor.
    Start,
    /// The Pier lost ordering (a relay dropped canvas records, an encoder
    /// restarted).
    Recovery,
    /// Geometry or topology changed.
    Geometry,
    /// The Deck asked for a reset ([`CanvasControlMsg::CanvasResetRequest`]).
    ResetRequest,
}

impl CanvasResetReason {
    /// Wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Start => 1,
            Self::Recovery => 2,
            Self::Geometry => 3,
            Self::ResetRequest => 4,
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Start),
            2 => Some(Self::Recovery),
            3 => Some(Self::Geometry),
            4 => Some(Self::ResetRequest),
            _ => None,
        }
    }
}

/// Pixel format of a canvas.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasPixelFormat {
    /// Opaque eight-bit BGRA, the desktop's own sRGB-coded values; alpha is
    /// always 255.
    Bgra8,
}

impl CanvasPixelFormat {
    /// Wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Bgra8 => 1,
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Bgra8),
            _ => None,
        }
    }
}

/// Kind byte of a [`FrameType::Canvas`] record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CanvasRecordKind {
    /// [`CanvasReset`].
    Reset,
    /// [`CanvasFrame`].
    Frame,
}

impl CanvasRecordKind {
    /// Wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Reset => 1,
            Self::Frame => 2,
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Reset),
            2 => Some(Self::Frame),
            _ => None,
        }
    }
}

/// Kind 1: the ordered barrier that starts a canvas generation. The Deck
/// clears the monitor's canvas to opaque black, closes every surface and
/// empties the cache; the Pier does the same to its mirror.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasReset {
    /// Identity, with the new generation.
    pub scope: CanvasScope,
    /// Why.
    pub reason: CanvasResetReason,
    /// Pixel format.
    pub format: CanvasPixelFormat,
    /// Canvas width in pixels.
    pub width: u32,
    /// Canvas height in pixels.
    pub height: u32,
}

impl CanvasReset {
    /// Blocks in this generation's grid.
    #[must_use]
    pub fn block_count(&self) -> u32 {
        canvas_block_count(self.width, self.height)
    }

    fn check(&self) -> Result<(), CanvasRecordError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_CANVAS_SIDE
            || self.height > MAX_CANVAS_SIDE
        {
            return Err(CanvasRecordError::Geometry);
        }
        Ok(())
    }

    /// Encodes the record.
    ///
    /// # Errors
    ///
    /// [`CanvasRecordError::Geometry`] for a size outside the limits.
    pub fn encode(&self) -> Result<Vec<u8>, CanvasRecordError> {
        self.check()?;
        let mut out = Vec::with_capacity(CANVAS_RESET_BYTES);
        put_prefix(&mut out, CanvasRecordKind::Reset, &self.scope);
        out.extend_from_slice(&[self.reason.wire(), self.format.wire(), 0, 0]);
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        Ok(out)
    }

    fn decode_body(
        scope: CanvasScope,
        reader: &mut ByteReader<'_>,
    ) -> Result<Self, CanvasRecordError> {
        let reason = CanvasResetReason::from_wire(reader.u8()?).ok_or(CanvasRecordError::Value)?;
        let format = CanvasPixelFormat::from_wire(reader.u8()?).ok_or(CanvasRecordError::Value)?;
        if reader.u16()? != 0 {
            return Err(CanvasRecordError::Reserved);
        }
        let reset = Self {
            scope,
            reason,
            format,
            width: reader.u32()?,
            height: reader.u32()?,
        };
        if reader.remaining() != 0 {
            return Err(CanvasRecordError::TrailingBytes);
        }
        reset.check()?;
        Ok(reset)
    }
}

/// A lossless eight-bit RGB tile: refine's `delta-zstd1` at depth 8 (the
/// expanded layout is `width × height × 3 planes × 2 bytes`, high bytes
/// zero).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasTile<'a> {
    /// Destination; grid origin, at most 256 × 64, whole blocks except at
    /// the canvas's right/bottom edge.
    pub rect: CanvasRect,
    /// Codec id ([`CODEC_DELTA_ZSTD1`]).
    pub codec: u8,
    /// Code depth ([`CANVAS_TILE_DEPTH`]).
    pub depth: u8,
    /// Compressed payload.
    pub payload: &'a [u8],
}

impl CanvasTile<'_> {
    /// Expanded (decoded) bytes, checked against the tile limit.
    ///
    /// # Errors
    ///
    /// [`CanvasRecordError::ExpandedTooLarge`].
    pub fn expanded_bytes(&self) -> Result<usize, CanvasRecordError> {
        usize::from(self.rect.width)
            .checked_mul(usize::from(self.rect.height))
            .and_then(|pixels| pixels.checked_mul(6))
            .filter(|bytes| *bytes <= MAX_TILE_EXPANDED_BYTES)
            .ok_or(CanvasRecordError::ExpandedTooLarge)
    }

    fn check(&self) -> Result<(), CanvasRecordError> {
        self.rect.check_tile()?;
        if self.codec != CODEC_DELTA_ZSTD1 || self.depth != CANVAS_TILE_DEPTH {
            return Err(CanvasRecordError::Value);
        }
        if self.payload.is_empty() || self.payload.len() > MAX_TILE_PAYLOAD_BYTES {
            return Err(CanvasRecordError::PayloadTooLarge);
        }
        self.expanded_bytes().map(|_| ())
    }
}

/// Paints a surface's next access unit into the canvas.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfacePaint<'a> {
    /// Surface id.
    pub surface: u8,
    /// The access unit is an IDR picture ([`SURFACE_PAINT_IDR`]).
    pub idr: bool,
    /// Where the decoded picture lands, scaled from the surface size when
    /// they differ; grid-aligned like a tile but of any size.
    pub dest: CanvasRect,
    /// Canvas blocks inside `dest` that take the decoded pixels; empty means
    /// the access unit is decoded (it stays in the prediction chain) and
    /// paints nothing.
    pub blocks: BlockRuns,
    /// One Annex-B access unit.
    pub access_unit: &'a [u8],
}

/// One canvas command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasCommand<'a> {
    /// Fills a rectangle with one opaque colour.
    Fill {
        /// Rectangle (any pixel position).
        rect: CanvasRect,
        /// `[r, g, b]`.
        rgb: [u8; 3],
    },
    /// Copies a rectangle within the canvas, as if through a temporary
    /// (overlap is allowed: a scroll or a window move).
    Copy {
        /// Source rectangle.
        src: CanvasRect,
        /// Destination left edge.
        dst_x: u16,
        /// Destination top edge.
        dst_y: u16,
    },
    /// Writes a lossless tile.
    Tile(CanvasTile<'a>),
    /// Stores canvas pixels into a cache slot, replacing what it held.
    CacheStore {
        /// Slot.
        slot: u32,
        /// Canvas rectangle (tile rules).
        rect: CanvasRect,
    },
    /// Paints a cache slot at a grid-aligned point.
    CachePaint {
        /// Slot.
        slot: u32,
        /// Destination left edge.
        dst_x: u16,
        /// Destination top edge.
        dst_y: u16,
    },
    /// Forgets a cache slot.
    CacheEvict {
        /// Slot.
        slot: u32,
    },
    /// Opens a surface (or replaces an open one): a fresh decoder whose
    /// first paint is an IDR.
    SurfaceOpen {
        /// Surface id.
        surface: u8,
        /// Codec id ([`SURFACE_CODEC_H264`]).
        codec: u8,
        /// Coded width (even).
        width: u16,
        /// Coded height (even).
        height: u16,
    },
    /// Closes a surface.
    SurfaceClose {
        /// Surface id.
        surface: u8,
    },
    /// Decodes an access unit and paints it.
    SurfacePaint(SurfacePaint<'a>),
}

impl CanvasCommand<'_> {
    /// Wire type byte.
    #[must_use]
    pub const fn wire(&self) -> u8 {
        match self {
            Self::Fill { .. } => 1,
            Self::Copy { .. } => 2,
            Self::Tile(_) => 3,
            Self::CacheStore { .. } => 4,
            Self::CachePaint { .. } => 5,
            Self::CacheEvict { .. } => 6,
            Self::SurfaceOpen { .. } => 7,
            Self::SurfaceClose { .. } => 8,
            Self::SurfacePaint(_) => 9,
        }
    }

    /// Stable token for telemetry.
    #[must_use]
    pub const fn token(&self) -> &'static str {
        match self {
            Self::Fill { .. } => "fill",
            Self::Copy { .. } => "copy",
            Self::Tile(_) => "tile",
            Self::CacheStore { .. } => "cache_store",
            Self::CachePaint { .. } => "cache_paint",
            Self::CacheEvict { .. } => "cache_evict",
            Self::SurfaceOpen { .. } => "surface_open",
            Self::SurfaceClose { .. } => "surface_close",
            Self::SurfacePaint(_) => "surface_paint",
        }
    }

    /// Checks every rule that does not need the canvas size.
    ///
    /// # Errors
    ///
    /// [`CanvasRecordError`].
    pub fn check(&self, block_count: u32) -> Result<(), CanvasRecordError> {
        match self {
            Self::Fill { rect, .. } => rect.check_nonempty(),
            Self::Copy { src, dst_x, dst_y } => {
                src.check_nonempty()?;
                CanvasRect {
                    x: *dst_x,
                    y: *dst_y,
                    width: src.width,
                    height: src.height,
                }
                .check_nonempty()
            }
            Self::Tile(tile) => tile.check(),
            Self::CacheStore { slot, rect } => {
                check_slot(*slot)?;
                rect.check_tile()
            }
            Self::CachePaint { slot, dst_x, dst_y } => {
                check_slot(*slot)?;
                check_point(*dst_x, *dst_y, true)
            }
            Self::CacheEvict { slot } => check_slot(*slot),
            Self::SurfaceOpen {
                surface,
                codec,
                width,
                height,
            } => {
                check_surface(*surface)?;
                if *codec != SURFACE_CODEC_H264 {
                    return Err(CanvasRecordError::Value);
                }
                if *width == 0
                    || *height == 0
                    || width % 2 != 0
                    || height % 2 != 0
                    || *width > MAX_SURFACE_SIDE
                    || *height > MAX_SURFACE_SIDE
                    || u32::from(*width) * u32::from(*height) > MAX_SURFACE_PIXELS
                {
                    return Err(CanvasRecordError::Geometry);
                }
                Ok(())
            }
            Self::SurfaceClose { surface } => check_surface(*surface),
            Self::SurfacePaint(paint) => {
                check_surface(paint.surface)?;
                paint.dest.check_origin_aligned()?;
                if paint.access_unit.is_empty() || paint.access_unit.len() > MAX_SURFACE_AU_BYTES {
                    return Err(CanvasRecordError::AccessUnit);
                }
                if paint.blocks.runs().iter().any(|&(start, len)| {
                    start
                        .checked_add(len)
                        .is_none_or(|end| len == 0 || end > block_count)
                }) {
                    return Err(CanvasRecordError::Runs);
                }
                Ok(())
            }
        }
    }

    fn body_len(&self) -> usize {
        match self {
            Self::Fill { .. } | Self::Copy { .. } | Self::CacheStore { .. } => 12,
            Self::Tile(tile) => 12 + tile.payload.len(),
            Self::CachePaint { .. } | Self::SurfaceOpen { .. } => 8,
            Self::CacheEvict { .. } | Self::SurfaceClose { .. } => 4,
            Self::SurfacePaint(paint) => {
                20 + paint.blocks.runs().len() * REFINE_RUN_BYTES + paint.access_unit.len()
            }
        }
    }

    fn put(&self, out: &mut Vec<u8>) -> Result<(), CanvasRecordError> {
        let len = u32::try_from(self.body_len()).map_err(|_| CanvasRecordError::CommandLength)?;
        out.push(self.wire());
        out.push(0);
        out.extend_from_slice(&len.to_be_bytes());
        match self {
            Self::Fill { rect, rgb } => {
                rect.put(out);
                out.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 0]);
            }
            Self::Copy { src, dst_x, dst_y } => {
                src.put(out);
                out.extend_from_slice(&dst_x.to_be_bytes());
                out.extend_from_slice(&dst_y.to_be_bytes());
            }
            Self::Tile(tile) => {
                tile.rect.put(out);
                out.extend_from_slice(&[tile.codec, tile.depth, 0, 0]);
                out.extend_from_slice(tile.payload);
            }
            Self::CacheStore { slot, rect } => {
                out.extend_from_slice(&slot.to_be_bytes());
                rect.put(out);
            }
            Self::CachePaint { slot, dst_x, dst_y } => {
                out.extend_from_slice(&slot.to_be_bytes());
                out.extend_from_slice(&dst_x.to_be_bytes());
                out.extend_from_slice(&dst_y.to_be_bytes());
            }
            Self::CacheEvict { slot } => out.extend_from_slice(&slot.to_be_bytes()),
            Self::SurfaceOpen {
                surface,
                codec,
                width,
                height,
            } => {
                out.extend_from_slice(&[*surface, *codec, 0, 0]);
                out.extend_from_slice(&width.to_be_bytes());
                out.extend_from_slice(&height.to_be_bytes());
            }
            Self::SurfaceClose { surface } => out.extend_from_slice(&[*surface, 0, 0, 0]),
            Self::SurfacePaint(paint) => {
                let flags = if paint.idr { SURFACE_PAINT_IDR } else { 0 };
                out.extend_from_slice(&[paint.surface, flags, 0, 0]);
                paint.dest.put(out);
                put_runs(out, &paint.blocks);
                let au_len = u32::try_from(paint.access_unit.len())
                    .map_err(|_| CanvasRecordError::AccessUnit)?;
                out.extend_from_slice(&au_len.to_be_bytes());
                out.extend_from_slice(paint.access_unit);
            }
        }
        Ok(())
    }

    /// Reads one command whose body is exactly `body`.
    fn read(
        kind: u8,
        body: &[u8],
        block_count: u32,
    ) -> Result<CanvasCommand<'_>, CanvasRecordError> {
        let fixed = |len: usize| {
            if body.len() == len {
                Ok(())
            } else {
                Err(CanvasRecordError::CommandLength)
            }
        };
        let mut reader = ByteReader::new(body);
        let command = match kind {
            1 => {
                fixed(12)?;
                let rect = CanvasRect::read(&mut reader)?;
                let [r, g, b, reserved] = reader.take::<4>()?;
                if reserved != 0 {
                    return Err(CanvasRecordError::Reserved);
                }
                CanvasCommand::Fill {
                    rect,
                    rgb: [r, g, b],
                }
            }
            2 => {
                fixed(12)?;
                CanvasCommand::Copy {
                    src: CanvasRect::read(&mut reader)?,
                    dst_x: reader.u16()?,
                    dst_y: reader.u16()?,
                }
            }
            3 => {
                if body.len() < 12 {
                    return Err(CanvasRecordError::CommandLength);
                }
                let rect = CanvasRect::read(&mut reader)?;
                let [codec, depth, r0, r1] = reader.take::<4>()?;
                if r0 != 0 || r1 != 0 {
                    return Err(CanvasRecordError::Reserved);
                }
                CanvasCommand::Tile(CanvasTile {
                    rect,
                    codec,
                    depth,
                    payload: reader.rest(),
                })
            }
            4 => {
                fixed(12)?;
                CanvasCommand::CacheStore {
                    slot: reader.u32()?,
                    rect: CanvasRect::read(&mut reader)?,
                }
            }
            5 => {
                fixed(8)?;
                CanvasCommand::CachePaint {
                    slot: reader.u32()?,
                    dst_x: reader.u16()?,
                    dst_y: reader.u16()?,
                }
            }
            6 => {
                fixed(4)?;
                CanvasCommand::CacheEvict {
                    slot: reader.u32()?,
                }
            }
            7 => {
                fixed(8)?;
                let [surface, codec, r0, r1] = reader.take::<4>()?;
                if r0 != 0 || r1 != 0 {
                    return Err(CanvasRecordError::Reserved);
                }
                CanvasCommand::SurfaceOpen {
                    surface,
                    codec,
                    width: reader.u16()?,
                    height: reader.u16()?,
                }
            }
            8 => {
                fixed(4)?;
                let [surface, r0, r1, r2] = reader.take::<4>()?;
                if r0 != 0 || r1 != 0 || r2 != 0 {
                    return Err(CanvasRecordError::Reserved);
                }
                CanvasCommand::SurfaceClose { surface }
            }
            9 => {
                let [surface, flags, r0, r1] = reader.take::<4>()?;
                if flags & !SURFACE_PAINT_IDR != 0 || r0 != 0 || r1 != 0 {
                    return Err(CanvasRecordError::Reserved);
                }
                let dest = CanvasRect::read(&mut reader)?;
                let run_count = reader.u32()?;
                // Bound before allocating: the runs must fit in this
                // command's body, ahead of the access unit's length.
                let run_bytes = usize::try_from(run_count)
                    .ok()
                    .and_then(|count| count.checked_mul(REFINE_RUN_BYTES))
                    .filter(|bytes| {
                        bytes
                            .checked_add(4)
                            .is_some_and(|need| need <= reader.remaining())
                    })
                    .ok_or(CanvasRecordError::CommandLength)?;
                let blocks = parse_runs(reader.slice(run_bytes)?, run_count, block_count)?;
                let au_len =
                    usize::try_from(reader.u32()?).map_err(|_| CanvasRecordError::AccessUnit)?;
                if au_len > MAX_SURFACE_AU_BYTES {
                    return Err(CanvasRecordError::AccessUnit);
                }
                if au_len != reader.remaining() {
                    return Err(CanvasRecordError::CommandLength);
                }
                CanvasCommand::SurfacePaint(SurfacePaint {
                    surface,
                    idr: flags & SURFACE_PAINT_IDR != 0,
                    dest,
                    blocks,
                    access_unit: reader.rest(),
                })
            }
            other => return Err(CanvasRecordError::UnknownCommand(other)),
        };
        command.check(block_count)?;
        Ok(command)
    }
}

fn check_slot(slot: u32) -> Result<(), CanvasRecordError> {
    if slot < MAX_CACHE_SLOTS {
        Ok(())
    } else {
        Err(CanvasRecordError::Slot)
    }
}

fn check_surface(surface: u8) -> Result<(), CanvasRecordError> {
    if surface < MAX_SURFACES_PER_MONITOR {
        Ok(())
    } else {
        Err(CanvasRecordError::Surface)
    }
}

/// Kind 2: one ordered list of commands for one monitor, applied atomically
/// before the canvas is presented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanvasFrame<'a> {
    /// Identity.
    pub scope: CanvasScope,
    /// Fence: nonzero, strictly increasing within a generation.
    pub fence: u64,
    /// Capture time on the wire clock (`crate::wire::now_wire_timestamp_ms`).
    pub timestamp_ms: u32,
    /// Blocks in the canvas grid (must equal the generation's).
    pub block_count: u32,
    /// The commands, in order.
    pub commands: Vec<CanvasCommand<'a>>,
}

impl CanvasFrame<'_> {
    /// Encodes the frame after checking every limit a decoder checks.
    ///
    /// # Errors
    ///
    /// [`CanvasRecordError`].
    pub fn encode(&self) -> Result<Vec<u8>, CanvasRecordError> {
        if self.fence == 0 {
            return Err(CanvasRecordError::ZeroFence);
        }
        check_block_count(self.block_count)?;
        let count = u32::try_from(self.commands.len())
            .ok()
            .filter(|count| *count <= MAX_CANVAS_COMMANDS)
            .ok_or(CanvasRecordError::CommandCount)?;
        let mut total = CANVAS_FRAME_FIXED_BYTES;
        for command in &self.commands {
            command.check(self.block_count)?;
            total = total
                .checked_add(CANVAS_COMMAND_HEADER_BYTES + command.body_len())
                .filter(|total| *total <= MAX_CANVAS_FRAME_BYTES)
                .ok_or(CanvasRecordError::FrameTooLarge)?;
        }
        let mut out = Vec::with_capacity(total);
        put_prefix(&mut out, CanvasRecordKind::Frame, &self.scope);
        out.extend_from_slice(&self.fence.to_be_bytes());
        out.extend_from_slice(&self.timestamp_ms.to_be_bytes());
        out.extend_from_slice(&self.block_count.to_be_bytes());
        out.extend_from_slice(&count.to_be_bytes());
        for command in &self.commands {
            command.put(&mut out)?;
        }
        debug_assert_eq!(out.len(), total);
        Ok(out)
    }
}

fn decode_frame<'a>(
    scope: CanvasScope,
    reader: &mut ByteReader<'a>,
) -> Result<CanvasFrame<'a>, CanvasRecordError> {
    let fence = reader.u64()?;
    if fence == 0 {
        return Err(CanvasRecordError::ZeroFence);
    }
    let timestamp_ms = reader.u32()?;
    let block_count = check_block_count(reader.u32()?)?;
    let count = reader.u32()?;
    // Bound before allocating: within the protocol limit and no more
    // commands than the record could hold at their smallest.
    let smallest = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(CANVAS_COMMAND_HEADER_BYTES + 4));
    if count > MAX_CANVAS_COMMANDS || smallest.is_none_or(|bytes| bytes > reader.remaining()) {
        return Err(CanvasRecordError::CommandCount);
    }
    let mut commands = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let kind = reader.u8()?;
        if reader.u8()? != 0 {
            return Err(CanvasRecordError::Reserved);
        }
        let len = usize::try_from(reader.u32()?).map_err(|_| CanvasRecordError::CommandLength)?;
        let body = reader
            .slice(len)
            .map_err(|_| CanvasRecordError::CommandLength)?;
        commands.push(CanvasCommand::read(kind, body, block_count)?);
    }
    if reader.remaining() != 0 {
        return Err(CanvasRecordError::TrailingBytes);
    }
    Ok(CanvasFrame {
        scope,
        fence,
        timestamp_ms,
        block_count,
        commands,
    })
}

/// A decoded [`FrameType::Canvas`] record, borrowing its payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CanvasRecord<'a> {
    /// Kind 1.
    Reset(CanvasReset),
    /// Kind 2.
    Frame(CanvasFrame<'a>),
}

impl CanvasRecord<'_> {
    /// Identity.
    #[must_use]
    pub const fn scope(&self) -> &CanvasScope {
        match self {
            Self::Reset(reset) => &reset.scope,
            Self::Frame(frame) => &frame.scope,
        }
    }
}

impl<'a> CanvasRecord<'a> {
    /// Decodes and validates a record. The size bound is checked first, and
    /// every count and length before anything sized by it is allocated.
    ///
    /// # Errors
    ///
    /// [`CanvasRecordError`].
    pub fn decode(bytes: &'a [u8]) -> Result<Self, CanvasRecordError> {
        if bytes.len() > MAX_CANVAS_FRAME_BYTES {
            return Err(CanvasRecordError::FrameTooLarge);
        }
        let mut reader = ByteReader::new(bytes);
        let frame_type = reader.u8()?;
        if frame_type != FrameType::Canvas as u8 {
            return Err(CanvasRecordError::WrongType(frame_type));
        }
        let version = reader.u8()?;
        if version != CANVAS_RECORD_VERSION {
            return Err(CanvasRecordError::Version(version));
        }
        let kind_byte = reader.u8()?;
        let kind =
            CanvasRecordKind::from_wire(kind_byte).ok_or(CanvasRecordError::Kind(kind_byte))?;
        if reader.u8()? != 0 {
            return Err(CanvasRecordError::Reserved);
        }
        let monitor_id = reader.u16()?;
        let topology_generation = reader.u64()?;
        let stream_epoch = reader.u64()?;
        let generation =
            std::num::NonZeroU32::new(reader.u32()?).ok_or(CanvasRecordError::ZeroGeneration)?;
        let scope = CanvasScope {
            monitor_id,
            topology_generation,
            stream_epoch,
            generation,
        };
        match kind {
            CanvasRecordKind::Reset => {
                CanvasReset::decode_body(scope, &mut reader).map(Self::Reset)
            }
            CanvasRecordKind::Frame => decode_frame(scope, &mut reader).map(Self::Frame),
        }
    }
}

fn put_prefix(out: &mut Vec<u8>, kind: CanvasRecordKind, scope: &CanvasScope) {
    out.extend_from_slice(&[
        FrameType::Canvas as u8,
        CANVAS_RECORD_VERSION,
        kind.wire(),
        0,
    ]);
    out.extend_from_slice(&scope.monitor_id.to_be_bytes());
    out.extend_from_slice(&scope.topology_generation.to_be_bytes());
    out.extend_from_slice(&scope.stream_epoch.to_be_bytes());
    out.extend_from_slice(&scope.generation.get().to_be_bytes());
}

fn reason_ok(reason: &str) -> bool {
    !reason.is_empty()
        && reason.len() <= MAX_CANVAS_REASON_BYTES
        && reason
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

fn optional_reason_ok(reason: Option<&String>) -> bool {
    reason.is_none_or(|reason| reason_ok(reason))
}

/// Deck capability (`AuthResponse.canvas_v1`): what the Deck can hold. Sent
/// only by a Deck that implements canvas v1 and has it switched on.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasCapability {
    /// Largest canvas side in pixels (the Deck's texture limit).
    #[serde(default)]
    pub max_canvas_px: u32,
    /// Most surfaces the Deck decodes at once per monitor.
    #[serde(default)]
    pub max_surfaces_per_monitor: u8,
    /// Cache budget in bytes (0 = no cache).
    #[serde(default)]
    pub cache_bytes: u64,
    /// Cache slots (0 = no cache).
    #[serde(default)]
    pub cache_slots: u32,
    /// Tile codec tokens ([`CODEC_DELTA_ZSTD1_TOKEN`]).
    #[serde(default)]
    pub tile_codecs: Vec<String>,
}

/// What a Pier may use with one Deck: the capability clamped to the
/// protocol's bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasTerms {
    /// Largest canvas side.
    pub max_canvas_px: u32,
    /// Surfaces per monitor.
    pub max_surfaces_per_monitor: u8,
    /// Cache budget in bytes.
    pub cache_bytes: u64,
    /// Cache slots.
    pub cache_slots: u32,
}

impl CanvasCapability {
    /// The terms a Pier may use, or `None` when the Deck cannot hold a
    /// canvas this protocol defines (no `delta-zstd1`, no surface, a canvas
    /// smaller than one block).
    #[must_use]
    pub fn terms(&self) -> Option<CanvasTerms> {
        let codec = self
            .tile_codecs
            .iter()
            .any(|token| token == CODEC_DELTA_ZSTD1_TOKEN);
        if !codec
            || self.max_surfaces_per_monitor == 0
            || self.max_canvas_px < u32::from(CANVAS_BLOCK)
        {
            return None;
        }
        let (cache_bytes, cache_slots) = if self.cache_bytes == 0 || self.cache_slots == 0 {
            (0, 0)
        } else {
            (
                self.cache_bytes.min(MAX_CACHE_BYTES),
                self.cache_slots.min(MAX_CACHE_SLOTS),
            )
        };
        Some(CanvasTerms {
            max_canvas_px: self.max_canvas_px.min(MAX_CANVAS_SIDE),
            max_surfaces_per_monitor: self.max_surfaces_per_monitor.min(MAX_SURFACES_PER_MONITOR),
            cache_bytes,
            cache_slots,
        })
    }
}

/// Whether canvas runs, as the Pier's hello states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CanvasStateMsg {
    /// Canvas records are sent.
    On,
    /// Canvas records are not sent; `reason` says why.
    Off,
    /// A state this peer does not know (a newer Pier): treated as off.
    #[serde(other)]
    Unknown,
}

/// One monitor's canvas state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasMonitorStatusMsg {
    /// Session monitor id.
    pub monitor_id: u16,
    /// On or off.
    pub state: CanvasStateMsg,
    /// Stable reason token when off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Pier → Deck: the session's canvas plan (`ServerHelloMsg.canvas`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasStatusMsg {
    /// On (for at least one monitor) or off.
    pub state: CanvasStateMsg,
    /// Stable reason token when off (`operator_off`, `not_software`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Tile codec token when on ([`CODEC_DELTA_ZSTD1_TOKEN`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tile_codec: Option<String>,
    /// Cache budget the Pier uses, within the Deck's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_bytes: Option<u64>,
    /// Cache slots the Pier uses, within the Deck's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_slots: Option<u32>,
    /// Surfaces per monitor the Pier uses, within the Deck's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_surfaces_per_monitor: Option<u8>,
    /// Per-monitor state when on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub monitors: Vec<CanvasMonitorStatusMsg>,
}

impl CanvasStatusMsg {
    /// Whether canvas records may arrive for `monitor_id`.
    #[must_use]
    pub fn is_on_for(&self, monitor_id: u16) -> bool {
        self.state == CanvasStateMsg::On
            && self.monitors.iter().any(|monitor| {
                monitor.monitor_id == monitor_id && monitor.state == CanvasStateMsg::On
            })
    }

    /// Checks the bounds a Deck relies on: at most [`MAX_CANVAS_MONITORS`]
    /// distinct monitors, reason tokens of at most
    /// [`MAX_CANVAS_REASON_BYTES`] lowercase ASCII, and terms within what
    /// the Deck offered.
    ///
    /// # Errors
    ///
    /// [`CanvasRecordError::Message`].
    pub fn validate(&self, offered: &CanvasTerms) -> Result<(), CanvasRecordError> {
        let mut seen = std::collections::BTreeSet::new();
        let monitors_ok = self.monitors.len() <= MAX_CANVAS_MONITORS
            && self.monitors.iter().all(|monitor| {
                seen.insert(monitor.monitor_id) && optional_reason_ok(monitor.reason.as_ref())
            });
        let within = self
            .cache_bytes
            .is_none_or(|bytes| bytes <= offered.cache_bytes)
            && self
                .cache_slots
                .is_none_or(|slots| slots <= offered.cache_slots)
            && self
                .max_surfaces_per_monitor
                .is_none_or(|surfaces| surfaces <= offered.max_surfaces_per_monitor);
        let codec_ok = self
            .tile_codec
            .as_deref()
            .map_or(self.state != CanvasStateMsg::On, |codec| {
                codec == CODEC_DELTA_ZSTD1_TOKEN
            });
        if monitors_ok && within && codec_ok && optional_reason_ok(self.reason.as_ref()) {
            Ok(())
        } else {
            Err(CanvasRecordError::Message)
        }
    }
}

/// Deck → Pier canvas messages on the control lane, sent only for a monitor
/// the Pier's hello reported `on`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CanvasControlMsg {
    /// Flow control: what the Deck has presented and what it still holds.
    /// Sent once per presented canvas frame, and at least every 250 ms
    /// while frames arrive. Used for throttling only,
    /// never for cache or canvas correctness.
    CanvasAck {
        /// Monitor.
        monitor_id: u16,
        /// Canvas generation the fence belongs to.
        generation: u32,
        /// Newest presented fence (0 = none yet in this generation).
        fence: u64,
        /// Canvas record bytes received and not yet applied.
        unprocessed_bytes: u64,
        /// 95th-percentile surface decode time over the last second, in
        /// milliseconds.
        decode_ms_p95: u32,
    },
    /// Asks the Pier for a [`CanvasReset`] (`ResetRequest`): the Deck lost
    /// or refused canvas state.
    CanvasResetRequest {
        /// Monitor.
        monitor_id: u16,
        /// The generation the Deck holds (0 = none).
        generation: u32,
        /// Stable reason token.
        reason: String,
    },
}

impl CanvasControlMsg {
    /// Checks the bounds a Pier relies on.
    ///
    /// # Errors
    ///
    /// [`CanvasRecordError::Message`].
    pub fn validate(&self) -> Result<(), CanvasRecordError> {
        match self {
            Self::CanvasAck { .. } => Ok(()),
            Self::CanvasResetRequest { reason, .. } if reason_ok(reason) => Ok(()),
            Self::CanvasResetRequest { .. } => Err(CanvasRecordError::Message),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn scope() -> CanvasScope {
        CanvasScope {
            monitor_id: 2,
            topology_generation: 7,
            stream_epoch: 3,
            generation: NonZeroU32::new(5).unwrap(),
        }
    }

    const BLOCKS: u32 = 120 * 68;

    fn every_command<'a>(payload: &'a [u8], au: &'a [u8]) -> Vec<CanvasCommand<'a>> {
        let rect = CanvasRect {
            x: 32,
            y: 16,
            width: 256,
            height: 64,
        };
        vec![
            CanvasCommand::Fill {
                rect: CanvasRect {
                    x: 3,
                    y: 5,
                    width: 7,
                    height: 9,
                },
                rgb: [1, 2, 3],
            },
            CanvasCommand::Copy {
                src: CanvasRect {
                    x: 0,
                    y: 24,
                    width: 1920,
                    height: 1000,
                },
                dst_x: 0,
                dst_y: 0,
            },
            CanvasCommand::Tile(CanvasTile {
                rect,
                codec: CODEC_DELTA_ZSTD1,
                depth: CANVAS_TILE_DEPTH,
                payload,
            }),
            CanvasCommand::CacheStore { slot: 9, rect },
            CanvasCommand::CachePaint {
                slot: 9,
                dst_x: 64,
                dst_y: 128,
            },
            CanvasCommand::CacheEvict { slot: 9 },
            CanvasCommand::SurfaceOpen {
                surface: 1,
                codec: SURFACE_CODEC_H264,
                width: 1280,
                height: 720,
            },
            CanvasCommand::SurfacePaint(SurfacePaint {
                surface: 1,
                idr: true,
                dest: CanvasRect {
                    x: 320,
                    y: 160,
                    width: 1280,
                    height: 720,
                },
                blocks: BlockRuns::from_sorted_indices([2420, 2421, 2422, 2540]),
                access_unit: au,
            }),
            CanvasCommand::SurfacePaint(SurfacePaint {
                surface: 1,
                idr: false,
                dest: CanvasRect {
                    x: 320,
                    y: 160,
                    width: 1280,
                    height: 720,
                },
                blocks: BlockRuns::default(),
                access_unit: au,
            }),
            CanvasCommand::SurfaceClose { surface: 1 },
        ]
    }

    fn frame<'a>(commands: Vec<CanvasCommand<'a>>) -> CanvasFrame<'a> {
        CanvasFrame {
            scope: scope(),
            fence: 42,
            timestamp_ms: 0xDEAD_BEEF,
            block_count: BLOCKS,
            commands,
        }
    }

    #[test]
    fn records_round_trip() {
        let reset = CanvasReset {
            scope: scope(),
            reason: CanvasResetReason::Geometry,
            format: CanvasPixelFormat::Bgra8,
            width: 1920,
            height: 1080,
        };
        let bytes = reset.encode().unwrap();
        assert_eq!(bytes.len(), CANVAS_RESET_BYTES);
        assert_eq!(bytes[0], 0x0E);
        assert_eq!(CanvasRecord::decode(&bytes), Ok(CanvasRecord::Reset(reset)));
        assert_eq!(reset.block_count(), BLOCKS);

        let payload = [9_u8; 40];
        let au = [0, 0, 0, 1, 0x65, 0x88];
        let frame = frame(every_command(&payload, &au));
        let bytes = frame.encode().unwrap();
        assert_eq!(CanvasRecord::decode(&bytes), Ok(CanvasRecord::Frame(frame)));
    }

    #[test]
    fn the_layout_is_pinned() {
        let frame = frame(vec![CanvasCommand::Fill {
            rect: CanvasRect {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            rgb: [0xAA, 0xBB, 0xCC],
        }]);
        let bytes = frame.encode().unwrap();
        let mut expected = vec![0x0E, 1, 2, 0, 0, 2];
        expected.extend_from_slice(&7_u64.to_be_bytes());
        expected.extend_from_slice(&3_u64.to_be_bytes());
        expected.extend_from_slice(&5_u32.to_be_bytes());
        expected.extend_from_slice(&42_u64.to_be_bytes());
        expected.extend_from_slice(&0xDEAD_BEEF_u32.to_be_bytes());
        expected.extend_from_slice(&BLOCKS.to_be_bytes());
        expected.extend_from_slice(&1_u32.to_be_bytes());
        expected.extend_from_slice(&[1, 0, 0, 0, 0, 12, 0, 1, 0, 2, 0, 3, 0, 4]);
        expected.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0]);
        assert_eq!(bytes, expected);
        assert_eq!(CANVAS_FRAME_FIXED_BYTES, 46);
    }

    #[test]
    fn every_truncation_and_every_extension_is_refused() {
        let payload = [9_u8; 40];
        let au = [0, 0, 0, 1, 0x65];
        let bytes = frame(every_command(&payload, &au)).encode().unwrap();
        for len in 0..bytes.len() {
            assert!(
                CanvasRecord::decode(&bytes[..len]).is_err(),
                "a record cut at {len} bytes decoded"
            );
        }
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(
            CanvasRecord::decode(&longer),
            Err(CanvasRecordError::TrailingBytes)
        );
        let reset = CanvasReset {
            scope: scope(),
            reason: CanvasResetReason::Start,
            format: CanvasPixelFormat::Bgra8,
            width: 64,
            height: 64,
        }
        .encode()
        .unwrap();
        let mut longer = reset.clone();
        longer.push(0);
        assert_eq!(
            CanvasRecord::decode(&longer),
            Err(CanvasRecordError::TrailingBytes)
        );
    }

    #[test]
    fn single_byte_corruption_never_panics() {
        let payload = [9_u8; 40];
        let au = [0, 0, 0, 1, 0x65];
        let bytes = frame(every_command(&payload, &au)).encode().unwrap();
        // A deterministic sweep: every byte through a handful of values.
        for at in 0..bytes.len() {
            for value in [0_u8, 1, 0x7F, 0x80, 0xFF] {
                let mut mutated = bytes.clone();
                mutated[at] = value;
                let _ = CanvasRecord::decode(&mutated);
            }
        }
        // And random records of random lengths (xorshift, fixed seed).
        let mut state = 0x9E37_79B9_7F4A_7C15_u64;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for _ in 0..20_000 {
            let len = (next() % 160) as usize;
            let mut record: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if let Some(first) = record.first_mut() {
                *first = 0x0E;
            }
            if record.len() > 3 {
                record[1] = 1;
                record[2] = 1 + (next() % 2) as u8;
                record[3] = 0;
            }
            let _ = CanvasRecord::decode(&record);
        }
    }

    #[test]
    fn prefix_rules_are_enforced() {
        let reset = CanvasReset {
            scope: scope(),
            reason: CanvasResetReason::Start,
            format: CanvasPixelFormat::Bgra8,
            width: 64,
            height: 64,
        }
        .encode()
        .unwrap();
        let refuse = |at: usize, value: u8, error: CanvasRecordError| {
            let mut bytes = reset.clone();
            bytes[at] = value;
            assert_eq!(
                CanvasRecord::decode(&bytes),
                Err(error),
                "byte {at} = {value}"
            );
        };
        refuse(0, 0x0D, CanvasRecordError::WrongType(0x0D));
        refuse(1, 2, CanvasRecordError::Version(2));
        refuse(2, 3, CanvasRecordError::Kind(3));
        refuse(3, 1, CanvasRecordError::Reserved);
        refuse(26, 0, CanvasRecordError::Value);
        refuse(27, 2, CanvasRecordError::Value);
        refuse(28, 1, CanvasRecordError::Reserved);
        let mut zero = reset.clone();
        zero[22..26].copy_from_slice(&0_u32.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&zero),
            Err(CanvasRecordError::ZeroGeneration)
        );
        // The largest canvas is exactly the largest grid every frame may
        // name: 1024 × 1024 blocks.
        let mut bytes = reset.clone();
        bytes[30..34].copy_from_slice(&MAX_CANVAS_SIDE.to_be_bytes());
        bytes[34..38].copy_from_slice(&MAX_CANVAS_SIDE.to_be_bytes());
        let Ok(CanvasRecord::Reset(largest)) = CanvasRecord::decode(&bytes) else {
            panic!("the largest canvas decodes");
        };
        assert_eq!(largest.block_count(), crate::refine::MAX_GRID_BLOCKS);
        for (width, height) in [(0, 64), (64, 0), (MAX_CANVAS_SIDE + 1, 64)] {
            let mut bytes = reset.clone();
            bytes[30..34].copy_from_slice(&width.to_be_bytes());
            bytes[34..38].copy_from_slice(&u32::to_be_bytes(height));
            assert_eq!(
                CanvasRecord::decode(&bytes),
                Err(CanvasRecordError::Geometry)
            );
        }
    }

    #[test]
    fn counts_and_lengths_are_bounded_before_allocation() {
        let empty = frame(Vec::new());
        let bytes = empty.encode().unwrap();
        // A huge command count is refused from the header alone.
        let mut huge = bytes.clone();
        huge[42..46].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&huge),
            Err(CanvasRecordError::CommandCount)
        );
        // More commands than the record could hold, even within the limit.
        let mut many = bytes.clone();
        many[42..46].copy_from_slice(&1_000_u32.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&many),
            Err(CanvasRecordError::CommandCount)
        );
        // A zero fence and a zero or oversize grid.
        let mut zero_fence = bytes.clone();
        zero_fence[26..34].copy_from_slice(&0_u64.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&zero_fence),
            Err(CanvasRecordError::ZeroFence)
        );
        for block_count in [0, crate::refine::MAX_GRID_BLOCKS + 1] {
            let mut grid = bytes.clone();
            grid[38..42].copy_from_slice(&u32::to_be_bytes(block_count));
            assert_eq!(
                CanvasRecord::decode(&grid),
                Err(CanvasRecordError::BlockCount)
            );
        }
        // A record above the frame bound is refused before it is parsed.
        let oversize = vec![0_u8; MAX_CANVAS_FRAME_BYTES + 1];
        assert_eq!(
            CanvasRecord::decode(&oversize),
            Err(CanvasRecordError::FrameTooLarge)
        );

        // A command length past the record, and a run count past the
        // command, are refused without allocating.
        let au = [0, 0, 0, 1, 0x65];
        let paint = frame(vec![CanvasCommand::SurfacePaint(SurfacePaint {
            surface: 0,
            idr: true,
            dest: CanvasRect {
                x: 0,
                y: 0,
                width: 64,
                height: 64,
            },
            blocks: BlockRuns::from_sorted_indices([0]),
            access_unit: &au,
        })]);
        let bytes = paint.encode().unwrap();
        let command = CANVAS_FRAME_FIXED_BYTES;
        let mut long = bytes.clone();
        long[command + 2..command + 6].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&long),
            Err(CanvasRecordError::CommandLength)
        );
        let runs_at = command + CANVAS_COMMAND_HEADER_BYTES + 12;
        let mut runs = bytes.clone();
        runs[runs_at..runs_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&runs),
            Err(CanvasRecordError::CommandLength)
        );
        let au_at = runs_at + 4 + REFINE_RUN_BYTES;
        let mut au_len = bytes.clone();
        au_len[au_at..au_at + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&au_len),
            Err(CanvasRecordError::AccessUnit)
        );
        let mut au_short = bytes;
        au_short[au_at..au_at + 4].copy_from_slice(&4_u32.to_be_bytes());
        assert_eq!(
            CanvasRecord::decode(&au_short),
            Err(CanvasRecordError::CommandLength)
        );
    }

    #[test]
    fn command_limits_are_enforced_on_both_sides() {
        let payload = [1_u8; 8];
        let au = [0, 0, 0, 1, 0x65];
        let refused = |command: CanvasCommand<'_>, error: CanvasRecordError| {
            let frame = frame(vec![command]);
            assert_eq!(frame.encode().map(|_| ()), Err(error), "{frame:?}");
        };
        let tile = |x, y, width, height| {
            CanvasCommand::Tile(CanvasTile {
                rect: CanvasRect {
                    x,
                    y,
                    width,
                    height,
                },
                codec: CODEC_DELTA_ZSTD1,
                depth: CANVAS_TILE_DEPTH,
                payload: &payload,
            })
        };
        refused(tile(8, 0, 16, 16), CanvasRecordError::Geometry);
        refused(tile(0, 0, 272, 16), CanvasRecordError::Geometry);
        refused(tile(0, 0, 16, 80), CanvasRecordError::Geometry);
        refused(tile(0, 0, 0, 16), CanvasRecordError::Geometry);
        refused(tile(16_368, 0, 32, 16), CanvasRecordError::Geometry);
        refused(
            CanvasCommand::Tile(CanvasTile {
                rect: CanvasRect {
                    x: 0,
                    y: 0,
                    width: 16,
                    height: 16,
                },
                codec: CODEC_DELTA_ZSTD1,
                depth: 10,
                payload: &payload,
            }),
            CanvasRecordError::Value,
        );
        let big = vec![0_u8; MAX_TILE_PAYLOAD_BYTES + 1];
        refused(
            CanvasCommand::Tile(CanvasTile {
                rect: CanvasRect {
                    x: 0,
                    y: 0,
                    width: 16,
                    height: 16,
                },
                codec: CODEC_DELTA_ZSTD1,
                depth: CANVAS_TILE_DEPTH,
                payload: &big,
            }),
            CanvasRecordError::PayloadTooLarge,
        );
        refused(
            CanvasCommand::CacheEvict {
                slot: MAX_CACHE_SLOTS,
            },
            CanvasRecordError::Slot,
        );
        refused(
            CanvasCommand::CachePaint {
                slot: 0,
                dst_x: 8,
                dst_y: 0,
            },
            CanvasRecordError::Geometry,
        );
        refused(
            CanvasCommand::SurfaceClose {
                surface: MAX_SURFACES_PER_MONITOR,
            },
            CanvasRecordError::Surface,
        );
        for (width, height) in [(0, 2), (3, 2), (MAX_SURFACE_SIDE + 2, 2), (8192, 8192)] {
            refused(
                CanvasCommand::SurfaceOpen {
                    surface: 0,
                    codec: SURFACE_CODEC_H264,
                    width,
                    height,
                },
                CanvasRecordError::Geometry,
            );
        }
        refused(
            CanvasCommand::SurfaceOpen {
                surface: 0,
                codec: 2,
                width: 16,
                height: 16,
            },
            CanvasRecordError::Value,
        );
        refused(
            CanvasCommand::Copy {
                src: CanvasRect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
                dst_x: 16_352,
                dst_y: 0,
            },
            CanvasRecordError::Geometry,
        );
        let paint = |blocks: BlockRuns, access_unit: &'static [u8]| {
            CanvasCommand::SurfacePaint(SurfacePaint {
                surface: 0,
                idr: false,
                dest: CanvasRect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 64,
                },
                blocks,
                access_unit,
            })
        };
        refused(
            paint(BlockRuns::default(), &[]),
            CanvasRecordError::AccessUnit,
        );
        refused(
            paint(BlockRuns::from_sorted_indices([BLOCKS]), &[1]),
            CanvasRecordError::Runs,
        );
        let _ = au;
    }

    #[test]
    fn unknown_commands_and_reserved_bits_are_refused() {
        let au = [0, 0, 0, 1, 0x65];
        let bytes = frame(vec![CanvasCommand::SurfacePaint(SurfacePaint {
            surface: 0,
            idr: false,
            dest: CanvasRect {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            },
            blocks: BlockRuns::default(),
            access_unit: &au,
        })])
        .encode()
        .unwrap();
        let at = CANVAS_FRAME_FIXED_BYTES;
        let mut unknown = bytes.clone();
        unknown[at] = 10;
        assert_eq!(
            CanvasRecord::decode(&unknown),
            Err(CanvasRecordError::UnknownCommand(10))
        );
        let mut reserved = bytes.clone();
        reserved[at + 1] = 1;
        assert_eq!(
            CanvasRecord::decode(&reserved),
            Err(CanvasRecordError::Reserved)
        );
        let mut flags = bytes;
        flags[at + CANVAS_COMMAND_HEADER_BYTES + 1] = 0x02;
        assert_eq!(
            CanvasRecord::decode(&flags),
            Err(CanvasRecordError::Reserved)
        );
    }

    #[test]
    fn a_frame_above_the_limits_cannot_be_encoded() {
        let commands = (0..=MAX_CANVAS_COMMANDS)
            .map(|slot| CanvasCommand::CacheEvict { slot: slot % 16 })
            .collect();
        assert_eq!(
            frame(commands).encode().map(|_| ()),
            Err(CanvasRecordError::CommandCount)
        );
        let au = vec![0_u8; MAX_SURFACE_AU_BYTES];
        let paint = || {
            CanvasCommand::SurfacePaint(SurfacePaint {
                surface: 0,
                idr: false,
                dest: CanvasRect {
                    x: 0,
                    y: 0,
                    width: 16,
                    height: 16,
                },
                blocks: BlockRuns::default(),
                access_unit: &au,
            })
        };
        assert_eq!(
            frame(vec![paint(), paint()]).encode().map(|_| ()),
            Err(CanvasRecordError::FrameTooLarge)
        );
    }

    #[test]
    fn rect_helpers_follow_the_grid() {
        let rect = CanvasRect {
            x: 1904,
            y: 1072,
            width: 16,
            height: 8,
        };
        assert!(rect.inside(1920, 1080));
        assert!(rect.grid_aligned(1920, 1080));
        assert!(!rect.grid_aligned(1920, 1088));
        assert!(!CanvasRect { x: 8, ..rect }.grid_aligned(1920, 1080));
        assert!(!rect.inside(1919, 1080));
        assert_eq!(canvas_block_count(1920, 1080), BLOCKS);
        assert_eq!(canvas_block_count(1, 1), 1);
    }

    #[test]
    fn capability_terms_clamp_to_the_protocol() {
        let deck = CanvasCapability {
            max_canvas_px: 32_768,
            max_surfaces_per_monitor: 9,
            cache_bytes: u64::MAX,
            cache_slots: u32::MAX,
            tile_codecs: vec![CODEC_DELTA_ZSTD1_TOKEN.into()],
        };
        assert_eq!(
            deck.terms(),
            Some(CanvasTerms {
                max_canvas_px: MAX_CANVAS_SIDE,
                max_surfaces_per_monitor: MAX_SURFACES_PER_MONITOR,
                cache_bytes: MAX_CACHE_BYTES,
                cache_slots: MAX_CACHE_SLOTS,
            })
        );
        let no_cache = CanvasCapability {
            cache_slots: 0,
            ..deck.clone()
        };
        assert_eq!(no_cache.terms().map(|terms| terms.cache_bytes), Some(0));
        assert_eq!(CanvasCapability::default().terms(), None);
        assert_eq!(
            CanvasCapability {
                tile_codecs: vec!["delta-deflate1".into()],
                ..deck.clone()
            }
            .terms(),
            None
        );
        assert_eq!(
            CanvasCapability {
                max_surfaces_per_monitor: 0,
                ..deck.clone()
            }
            .terms(),
            None
        );
        // Additive: an older or newer Deck's extra or missing fields are fine.
        let parsed: CanvasCapability =
            serde_json::from_str(r#"{"max_canvas_px":16384,"future":1}"#).unwrap();
        assert_eq!(parsed.max_canvas_px, 16_384);
        assert!(parsed.tile_codecs.is_empty());
    }

    #[test]
    fn the_status_is_additive_bounded_and_per_monitor() {
        let terms = CanvasTerms {
            max_canvas_px: 16_384,
            max_surfaces_per_monitor: 2,
            cache_bytes: 100 << 20,
            cache_slots: 25_600,
        };
        let on = CanvasStatusMsg {
            state: CanvasStateMsg::On,
            reason: None,
            tile_codec: Some(CODEC_DELTA_ZSTD1_TOKEN.into()),
            cache_bytes: Some(64 << 20),
            cache_slots: Some(4_096),
            max_surfaces_per_monitor: Some(2),
            monitors: vec![
                CanvasMonitorStatusMsg {
                    monitor_id: 1,
                    state: CanvasStateMsg::On,
                    reason: None,
                },
                CanvasMonitorStatusMsg {
                    monitor_id: 2,
                    state: CanvasStateMsg::Off,
                    reason: Some("deck_texture_limit".into()),
                },
            ],
        };
        let json = serde_json::to_string(&on).unwrap();
        let back: CanvasStatusMsg = serde_json::from_str(&json).unwrap();
        assert_eq!(back, on);
        assert_eq!(back.validate(&terms), Ok(()));
        assert!(back.is_on_for(1));
        assert!(!back.is_on_for(2));
        assert!(!back.is_on_for(3));

        let newer: CanvasStatusMsg =
            serde_json::from_str(r#"{"state":"paused","reason":"x"}"#).unwrap();
        assert_eq!(newer.state, CanvasStateMsg::Unknown);
        assert!(!newer.is_on_for(1));

        let refused = |status: CanvasStatusMsg| {
            assert_eq!(status.validate(&terms), Err(CanvasRecordError::Message));
        };
        refused(CanvasStatusMsg {
            cache_bytes: Some(200 << 20),
            ..on.clone()
        });
        refused(CanvasStatusMsg {
            max_surfaces_per_monitor: Some(3),
            ..on.clone()
        });
        refused(CanvasStatusMsg {
            tile_codec: None,
            ..on.clone()
        });
        refused(CanvasStatusMsg {
            reason: Some("Not A Token".into()),
            ..on.clone()
        });
        let mut duplicate = on.clone();
        duplicate.monitors[1].monitor_id = 1;
        refused(duplicate);
        let mut many = on;
        many.monitors = (0..=MAX_CANVAS_MONITORS as u16)
            .map(|monitor_id| CanvasMonitorStatusMsg {
                monitor_id,
                state: CanvasStateMsg::On,
                reason: None,
            })
            .collect();
        refused(many);
    }

    #[test]
    fn control_messages_are_tagged_and_bounded() {
        let ack = CanvasControlMsg::CanvasAck {
            monitor_id: 1,
            generation: 3,
            fence: 99,
            unprocessed_bytes: 4096,
            decode_ms_p95: 6,
        };
        let json = serde_json::to_string(&ack).unwrap();
        assert_eq!(
            json,
            r#"{"type":"canvas_ack","monitor_id":1,"generation":3,"fence":99,"unprocessed_bytes":4096,"decode_ms_p95":6}"#
        );
        assert_eq!(
            serde_json::from_str::<CanvasControlMsg>(&json).unwrap(),
            ack
        );
        assert_eq!(ack.validate(), Ok(()));
        let reset = CanvasControlMsg::CanvasResetRequest {
            monitor_id: 1,
            generation: 0,
            reason: "decoder_error".into(),
        };
        let json = serde_json::to_string(&reset).unwrap();
        assert!(json.starts_with(r#"{"type":"canvas_reset_request""#));
        assert_eq!(reset.validate(), Ok(()));
        let long = CanvasControlMsg::CanvasResetRequest {
            monitor_id: 1,
            generation: 0,
            reason: "x".repeat(MAX_CANVAS_REASON_BYTES + 1),
        };
        assert_eq!(long.validate(), Err(CanvasRecordError::Message));
        assert_eq!(
            crate::lanes::lane_for_session_text(&json),
            crate::lanes::LaneRole::Control
        );
    }
}
