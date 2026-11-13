// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Pier-side `delta-zstd1` encoder (native libzstd, level 1).
//!
//! Frames always declare their content size, use no dictionary and no
//! checksum, and keep the window at most 128 KiB, so a Deck's header check
//! accepts them.

use super::tile::{TileCodes, TileError, encode_residuals};
use zstd::zstd_safe::CParameter;

/// Zstandard level of `delta-zstd1`.
pub const LEVEL: i32 = 1;

/// Why a tile could not be encoded.
#[derive(Debug)]
pub enum TileEncodeError {
    /// The residual transform refused the tile.
    Tile(TileError),
    /// libzstd failed.
    Zstd(std::io::Error),
}

/// A reusable level-1 compressor; keep one per refinement worker.
pub struct TileEncoder {
    compressor: zstd::bulk::Compressor<'static>,
}

impl std::fmt::Debug for TileEncoder {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TileEncoder")
            .finish_non_exhaustive()
    }
}

impl TileEncoder {
    /// A compressor configured for `delta-zstd1`.
    ///
    /// # Errors
    ///
    /// [`TileEncodeError::Zstd`].
    pub fn new() -> Result<Self, TileEncodeError> {
        let mut compressor = zstd::bulk::Compressor::new(LEVEL).map_err(TileEncodeError::Zstd)?;
        for parameter in [
            CParameter::ContentSizeFlag(true),
            CParameter::ChecksumFlag(false),
            CParameter::DictIdFlag(false),
            CParameter::WindowLog(17),
        ] {
            compressor
                .set_parameter(parameter)
                .map_err(TileEncodeError::Zstd)?;
        }
        Ok(Self { compressor })
    }

    /// Encodes one tile's codes.
    ///
    /// # Errors
    ///
    /// [`TileEncodeError`].
    pub fn encode(&mut self, codes: &TileCodes, depth: u8) -> Result<Vec<u8>, TileEncodeError> {
        let residuals = encode_residuals(codes, depth).map_err(TileEncodeError::Tile)?;
        self.compressor
            .compress(&residuals)
            .map_err(TileEncodeError::Zstd)
    }
}
