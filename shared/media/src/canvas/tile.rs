// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lossless canvas tiles at depth 8 (ADR 0028 §8): refine's grouping, tile
//! limits and `delta-zstd1` residual transform, over an eight-bit source.

use super::CANVAS_TILE_DEPTH;
use crate::refine::source::RefineSource;
use crate::refine::tile::{TileCodes, TileError, TileRect, group_blocks};
use arcen_protocol::canvas::{CanvasCommand, CanvasRect, CanvasTile};
use arcen_protocol::refine::{CODEC_DELTA_ZSTD1, MAX_TILE_PAYLOAD_BYTES, RefineDomain};

/// Why a tile could not be packed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasTileError {
    /// The source is not eight-bit R′G′B′.
    Source,
    /// Extraction or the residual transform refused the tile.
    Tile(TileError),
    /// The compressor failed, or its output exceeds the payload limit.
    Pack,
}

/// Compresses a depth-8 tile's codes into a `delta-zstd1` payload.
pub trait TilePacker {
    /// Packs one tile.
    ///
    /// # Errors
    ///
    /// [`CanvasTileError`].
    fn pack(&mut self, codes: &TileCodes) -> Result<Vec<u8>, CanvasTileError>;
}

/// `delta-zstd1` at depth 8 with refine's native level-1 compressor.
#[cfg(feature = "refine-zstd-encode")]
#[derive(Debug)]
pub struct ZstdTilePacker {
    encoder: crate::refine::zstd_encode::TileEncoder,
}

#[cfg(feature = "refine-zstd-encode")]
impl ZstdTilePacker {
    /// A packer; keep one per worker.
    ///
    /// # Errors
    ///
    /// [`CanvasTileError::Pack`].
    pub fn new() -> Result<Self, CanvasTileError> {
        crate::refine::zstd_encode::TileEncoder::new()
            .map(|encoder| Self { encoder })
            .map_err(|_| CanvasTileError::Pack)
    }
}

#[cfg(feature = "refine-zstd-encode")]
impl TilePacker for ZstdTilePacker {
    fn pack(&mut self, codes: &TileCodes) -> Result<Vec<u8>, CanvasTileError> {
        self.encoder
            .encode(codes, CANVAS_TILE_DEPTH)
            .map_err(|_| CanvasTileError::Pack)
    }
}

/// Residual bytes without compression: only for tests, where the default
/// feature set has no Zstandard (the payload limit still holds: a whole
/// 256 × 64 tile expands to 98,304 bytes).
#[cfg(any(test, feature = "canvas-corpus"))]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ResidualTilePacker;

#[cfg(any(test, feature = "canvas-corpus"))]
impl TilePacker for ResidualTilePacker {
    fn pack(&mut self, codes: &TileCodes) -> Result<Vec<u8>, CanvasTileError> {
        crate::refine::tile::encode_residuals(codes, CANVAS_TILE_DEPTH)
            .map_err(CanvasTileError::Tile)
    }
}

/// One packed tile: where, the exact codes (for the Pier's mirror) and the
/// payload (for the wire).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedTile {
    /// Canvas rectangle.
    pub rect: TileRect,
    /// The codes the payload carries.
    pub codes: TileCodes,
    /// `delta-zstd1` payload.
    pub payload: Vec<u8>,
}

impl PackedTile {
    /// The tile command that carries it.
    #[must_use]
    pub fn command(&self) -> CanvasCommand<'_> {
        let side = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
        CanvasCommand::Tile(CanvasTile {
            rect: CanvasRect {
                x: side(self.rect.x),
                y: side(self.rect.y),
                width: side(self.rect.width),
                height: side(self.rect.height),
            },
            codec: CODEC_DELTA_ZSTD1,
            depth: CANVAS_TILE_DEPTH,
            payload: &self.payload,
        })
    }
}

/// Groups `blocks` (ascending block indices of the source's grid) into
/// tiles of at most 256 × 64 and packs each from `source`.
///
/// # Errors
///
/// [`CanvasTileError`].
pub fn pack_tiles(
    source: &dyn RefineSource,
    blocks: &[u32],
    packer: &mut dyn TilePacker,
) -> Result<Vec<PackedTile>, CanvasTileError> {
    if source.depth() != CANVAS_TILE_DEPTH || source.domain() != RefineDomain::Rgb {
        return Err(CanvasTileError::Source);
    }
    group_blocks(source.grid(), blocks)
        .into_iter()
        .map(|rect| {
            let codes = source.extract(rect).map_err(CanvasTileError::Tile)?;
            let payload = packer.pack(&codes)?;
            if payload.is_empty() || payload.len() > MAX_TILE_PAYLOAD_BYTES {
                return Err(CanvasTileError::Pack);
            }
            Ok(PackedTile {
                rect,
                codes,
                payload,
            })
        })
        .collect()
}
