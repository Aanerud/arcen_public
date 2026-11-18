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

#[cfg(test)]
#[allow(clippy::cast_precision_loss)]
mod bench {
    use super::*;

    /// A photo-like 10-bit tile: smooth light plus sensor noise.
    fn photo_tile(seed: u32) -> TileCodes {
        let (width, height) = (256_u32, 64_u32);
        let mut state = seed.wrapping_mul(2_654_435_761).max(1);
        let mut samples = Vec::with_capacity((width * height * 3) as usize);
        for plane in 0..3_u32 {
            for y in 0..height {
                for x in 0..width {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    let base = 300 + (x * 2 + y * 3 + plane * 50 + seed % 200) % 400;
                    let noise = state % 9;
                    samples.push(u16::try_from(base + noise).unwrap_or(1023));
                }
            }
        }
        TileCodes::new(width, height, samples).expect("tile")
    }

    /// Throughput of one and two encoders over a 4K frame's tiles (510);
    /// run with `--ignored --nocapture` on the target host.
    #[test]
    #[ignore = "benchmark"]
    fn worker_throughput() {
        let tiles: Vec<TileCodes> = (0..510).map(photo_tile).collect();
        let raw = tiles.len() * 256 * 64 * 6;
        let run = |workers: usize| {
            let started = std::time::Instant::now();
            let compressed: usize = std::thread::scope(|scope| {
                let handles: Vec<_> = tiles
                    .chunks(tiles.len().div_ceil(workers))
                    .map(|chunk| {
                        scope.spawn(move || {
                            let mut encoder = TileEncoder::new().expect("encoder");
                            chunk
                                .iter()
                                .map(|tile| encoder.encode(tile, 10).expect("encode").len())
                                .sum::<usize>()
                        })
                    })
                    .collect();
                handles.into_iter().map(|h| h.join().expect("join")).sum()
            });
            let secs = started.elapsed().as_secs_f64();
            println!(
                "workers={workers} frame_ms={:.1} raw_mbyte_s={:.0} out_mbit_s={:.0} ratio={:.2}",
                secs * 1000.0,
                raw as f64 / secs / 1e6,
                compressed as f64 * 8.0 / secs / 1e6,
                raw as f64 / compressed as f64
            );
        };
        for workers in [1, 2, 1, 2] {
            run(workers);
        }
    }
}
