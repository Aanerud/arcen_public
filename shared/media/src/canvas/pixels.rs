// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The CPU executor of [`CanvasWork`]: the Pier's mirror pixels and the
//! oracle every Deck's GPU executor is tested against (ADR 0028 §1).
//!
//! Two seams keep it codec-free: a [`TileDecoder`] turns a tile payload
//! into codes, and a [`SurfaceDecoder`] turns an access unit into a picture
//! (or into nothing: a mirror that never copies from lossy blocks needs no
//! surface pixels, and leaves painted blocks as they were).

use super::state::{CanvasOp, CanvasWork, intersect};
use crate::refine::tile::{TileCodes, TileRect};
use arcen_protocol::canvas::MAX_SURFACES_PER_MONITOR;
use std::collections::{BTreeMap, VecDeque};
use std::fmt::{Display, Formatter};

/// Why the executor could not perform an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasExecError {
    /// An operation before the first reset, or outside the canvas.
    Canvas,
    /// A tile payload did not decode, or decoded to another size.
    Tile,
    /// A surface access unit did not decode.
    Surface,
    /// A cache slot the state owner accepted holds nothing here.
    Cache,
}

impl CanvasExecError {
    /// Stable token for telemetry.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Canvas => "canvas",
            Self::Tile => "tile",
            Self::Surface => "surface",
            Self::Cache => "cache",
        }
    }
}

impl Display for CanvasExecError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "canvas execution failed: {}", self.token())
    }
}

impl std::error::Error for CanvasExecError {}

/// Turns a tile payload into depth-8 planar R′G′B′ codes of `width ×
/// height`.
pub trait TileDecoder {
    /// Decodes one payload.
    ///
    /// # Errors
    ///
    /// [`CanvasExecError::Tile`].
    fn decode(
        &mut self,
        payload: &[u8],
        width: u32,
        height: u32,
    ) -> Result<TileCodes, CanvasExecError>;
}

/// The Pier's own tiles, in the order it packed them: the mirror applies
/// the codes it compressed without decompressing them again.
#[derive(Debug, Default)]
pub struct PackedTileCodes {
    codes: VecDeque<TileCodes>,
}

impl PackedTileCodes {
    /// Appends the codes of the next tile command.
    pub fn push(&mut self, codes: TileCodes) {
        self.codes.push_back(codes);
    }

    /// Tiles not applied yet.
    #[must_use]
    pub fn len(&self) -> usize {
        self.codes.len()
    }

    /// Whether every tile was applied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.codes.is_empty()
    }
}

impl TileDecoder for PackedTileCodes {
    fn decode(
        &mut self,
        _payload: &[u8],
        width: u32,
        height: u32,
    ) -> Result<TileCodes, CanvasExecError> {
        let codes = self.codes.pop_front().ok_or(CanvasExecError::Tile)?;
        if codes.width() == width && codes.height() == height {
            Ok(codes)
        } else {
            Err(CanvasExecError::Tile)
        }
    }
}

/// `delta-zstd1` at depth 8, decoded with the pure-Rust Zstandard decoder.
#[cfg(feature = "refine-zstd-decode")]
#[derive(Debug, Default, Clone, Copy)]
pub struct ZstdTileDecoder;

#[cfg(feature = "refine-zstd-decode")]
impl TileDecoder for ZstdTileDecoder {
    fn decode(
        &mut self,
        payload: &[u8],
        width: u32,
        height: u32,
    ) -> Result<TileCodes, CanvasExecError> {
        crate::refine::zstd_decode::decode_tile(payload, width, height, super::CANVAS_TILE_DEPTH)
            .map_err(|_| CanvasExecError::Tile)
    }
}

/// Residual bytes without compression: only for tests, where the default
/// feature set has no Zstandard.
#[cfg(test)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResidualTileDecoder;

#[cfg(test)]
impl TileDecoder for ResidualTileDecoder {
    fn decode(
        &mut self,
        payload: &[u8],
        width: u32,
        height: u32,
    ) -> Result<TileCodes, CanvasExecError> {
        crate::refine::tile::decode_residuals(payload, width, height, super::CANVAS_TILE_DEPTH)
            .map_err(|_| CanvasExecError::Tile)
    }
}

/// A decoded surface picture, opaque BGRA8, `width × 4` bytes a row.
#[derive(Debug, Clone, Copy)]
pub struct SurfacePicture<'p> {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// Pixels.
    pub bgra: &'p [u8],
}

/// Turns surface access units into pictures.
pub trait SurfaceDecoder {
    /// A surface was opened (or replaced).
    fn open(&mut self, surface: u8, width: u32, height: u32);
    /// A surface was closed, or a reset closed every surface (closing one
    /// that is not open does nothing).
    fn close(&mut self, surface: u8);
    /// Decodes one access unit, in order. `None` paints nothing (the blocks
    /// keep their pixels and stay marked lossy).
    ///
    /// # Errors
    ///
    /// [`CanvasExecError::Surface`].
    fn decode(
        &mut self,
        surface: u8,
        access_unit: &[u8],
        idr: bool,
    ) -> Result<Option<SurfacePicture<'_>>, CanvasExecError>;
}

/// A surface decoder that keeps no pixels: the mirror's under the rule that
/// lossy blocks are never copy or cache sources (ADR 0028 §6).
#[derive(Debug, Default, Clone, Copy)]
pub struct NoSurfacePixels;

impl SurfaceDecoder for NoSurfacePixels {
    fn open(&mut self, _surface: u8, _width: u32, _height: u32) {}

    fn close(&mut self, _surface: u8) {}

    fn decode(
        &mut self,
        _surface: u8,
        _access_unit: &[u8],
        _idr: bool,
    ) -> Result<Option<SurfacePicture<'_>>, CanvasExecError> {
        Ok(None)
    }
}

#[derive(Debug, Clone)]
struct CachedPixels {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

/// One monitor's canvas pixels on the CPU, opaque BGRA8.
#[derive(Debug, Clone, Default)]
pub struct CanvasPixels {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
    cache: BTreeMap<u32, CachedPixels>,
}

const OPAQUE_BLACK: [u8; 4] = [0, 0, 0, 255];

impl CanvasPixels {
    /// An executor with no canvas yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Canvas width.
    #[must_use]
    pub const fn width(&self) -> u32 {
        self.width
    }

    /// Canvas height.
    #[must_use]
    pub const fn height(&self) -> u32 {
        self.height
    }

    /// Bytes per row.
    #[must_use]
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }

    /// The canvas, BGRA8.
    #[must_use]
    pub fn bgra(&self) -> &[u8] {
        &self.bgra
    }

    fn inside(&self, rect: TileRect) -> bool {
        rect.x
            .checked_add(rect.width)
            .is_some_and(|right| right <= self.width)
            && rect
                .y
                .checked_add(rect.height)
                .is_some_and(|bottom| bottom <= self.height)
    }

    fn read(&self, rect: TileRect) -> Vec<u8> {
        let mut out = Vec::with_capacity(rect.pixels() * 4);
        for row in rect.y..rect.y + rect.height {
            let start = row as usize * self.stride() + rect.x as usize * 4;
            out.extend_from_slice(&self.bgra[start..start + rect.width as usize * 4]);
        }
        out
    }

    fn write(&mut self, rect: TileRect, bgra: &[u8]) {
        let row_bytes = rect.width as usize * 4;
        let stride = self.stride();
        for (row, source) in (rect.y..rect.y + rect.height).zip(bgra.chunks_exact(row_bytes)) {
            let start = row as usize * stride + rect.x as usize * 4;
            self.bgra[start..start + row_bytes].copy_from_slice(source);
        }
    }

    /// Performs `work` in order. A failure leaves the canvas partly
    /// updated: the caller breaks the state and asks for a reset.
    ///
    /// # Errors
    ///
    /// [`CanvasExecError`].
    pub fn execute(
        &mut self,
        work: &CanvasWork<'_>,
        tiles: &mut dyn TileDecoder,
        surfaces: &mut dyn SurfaceDecoder,
    ) -> Result<(), CanvasExecError> {
        for op in &work.ops {
            self.op(op, tiles, surfaces)?;
        }
        Ok(())
    }

    fn op(
        &mut self,
        op: &CanvasOp<'_>,
        tiles: &mut dyn TileDecoder,
        surfaces: &mut dyn SurfaceDecoder,
    ) -> Result<(), CanvasExecError> {
        let checked = |pixels: &Self, rect: TileRect| {
            if pixels.inside(rect) {
                Ok(())
            } else {
                Err(CanvasExecError::Canvas)
            }
        };
        match op {
            CanvasOp::Reset { width, height } => {
                self.width = *width;
                self.height = *height;
                let pixels = *width as usize * *height as usize;
                self.bgra = OPAQUE_BLACK.repeat(pixels);
                self.cache.clear();
                for surface in 0..MAX_SURFACES_PER_MONITOR {
                    surfaces.close(surface);
                }
            }
            CanvasOp::Fill { rect, bgra } => {
                checked(self, *rect)?;
                let stride = self.stride();
                for row in rect.y..rect.y + rect.height {
                    let start = row as usize * stride + rect.x as usize * 4;
                    for pixel in
                        self.bgra[start..start + rect.width as usize * 4].chunks_exact_mut(4)
                    {
                        pixel.copy_from_slice(bgra);
                    }
                }
            }
            CanvasOp::Copy { src, dst_x, dst_y } => {
                checked(self, *src)?;
                let dst = TileRect {
                    x: *dst_x,
                    y: *dst_y,
                    ..*src
                };
                checked(self, dst)?;
                self.copy_within(*src, dst);
            }
            CanvasOp::Tile { rect, payload } => {
                checked(self, *rect)?;
                let codes = tiles.decode(payload, rect.width, rect.height)?;
                if codes.width() != rect.width || codes.height() != rect.height {
                    return Err(CanvasExecError::Tile);
                }
                let mut bgra = Vec::with_capacity(rect.pixels() * 4);
                let (r, g, b) = (codes.plane(0), codes.plane(1), codes.plane(2));
                for at in 0..rect.pixels() {
                    let code =
                        |plane: &[u16]| u8::try_from(plane[at]).map_err(|_| CanvasExecError::Tile);
                    bgra.extend_from_slice(&[code(b)?, code(g)?, code(r)?, 255]);
                }
                self.write(*rect, &bgra);
            }
            CanvasOp::CacheStore { slot, rect } => {
                checked(self, *rect)?;
                let bgra = self.read(*rect);
                self.cache.insert(
                    *slot,
                    CachedPixels {
                        width: rect.width,
                        height: rect.height,
                        bgra,
                    },
                );
            }
            CanvasOp::CachePaint { slot, rect } => {
                checked(self, *rect)?;
                let entry = self.cache.get(slot).ok_or(CanvasExecError::Cache)?;
                if entry.width != rect.width || entry.height != rect.height {
                    return Err(CanvasExecError::Cache);
                }
                let bgra = entry.bgra.clone();
                self.write(*rect, &bgra);
            }
            CanvasOp::CacheEvict { slot } => {
                self.cache.remove(slot).ok_or(CanvasExecError::Cache)?;
            }
            CanvasOp::SurfaceOpen {
                surface,
                width,
                height,
            } => surfaces.open(*surface, *width, *height),
            CanvasOp::SurfaceClose { surface } => surfaces.close(*surface),
            CanvasOp::SurfacePaint {
                surface,
                idr,
                dest,
                blocks,
                access_unit,
            } => {
                checked(self, *dest)?;
                if let Some(picture) = surfaces.decode(*surface, access_unit, *idr)? {
                    self.paint_picture(picture, *dest, blocks)?;
                }
            }
        }
        Ok(())
    }

    /// Moves `src` to `dst` (same size, both inside) as if through a
    /// temporary: rows in the order that never reads a row already
    /// overwritten, each row with `copy_within`.
    fn copy_within(&mut self, src: TileRect, dst: TileRect) {
        let stride = self.stride();
        let row_bytes = src.width as usize * 4;
        let row = |offset: u32| {
            let from = (src.y + offset) as usize * stride + src.x as usize * 4;
            let to = (dst.y + offset) as usize * stride + dst.x as usize * 4;
            (from, to)
        };
        if dst.y > src.y {
            for offset in (0..src.height).rev() {
                let (from, to) = row(offset);
                self.bgra.copy_within(from..from + row_bytes, to);
            }
        } else {
            for offset in 0..src.height {
                let (from, to) = row(offset);
                self.bgra.copy_within(from..from + row_bytes, to);
            }
        }
    }

    /// Writes the listed blocks from `picture`, scaled into `dest` by
    /// nearest sample.
    fn paint_picture(
        &mut self,
        picture: SurfacePicture<'_>,
        dest: TileRect,
        blocks: &[TileRect],
    ) -> Result<(), CanvasExecError> {
        let expected = (picture.width as usize)
            .checked_mul(picture.height as usize)
            .and_then(|pixels| pixels.checked_mul(4));
        if picture.width == 0 || picture.height == 0 || expected != Some(picture.bgra.len()) {
            return Err(CanvasExecError::Surface);
        }
        let stride = self.stride();
        for span in blocks {
            let Some(span) = intersect(*span, dest) else {
                continue;
            };
            for y in span.y..span.y + span.height {
                let sy = u64::from(y - dest.y) * u64::from(picture.height) / u64::from(dest.height);
                for x in span.x..span.x + span.width {
                    let sx =
                        u64::from(x - dest.x) * u64::from(picture.width) / u64::from(dest.width);
                    let from = usize::try_from(sy * u64::from(picture.width) + sx)
                        .map_err(|_| CanvasExecError::Surface)?
                        * 4;
                    let to = y as usize * stride + x as usize * 4;
                    self.bgra[to..to + 3].copy_from_slice(&picture.bgra[from..from + 3]);
                    self.bgra[to + 3] = 255;
                }
            }
        }
        Ok(())
    }
}
