// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Deck-side `delta-zstd1` decoder (pure Rust, `ruzstd`).
//!
//! The frame header is checked by [`check_zstd_frame_header`] before the
//! decoder allocates; the decoder's window is capped; output is bounded to
//! the expected size; and bytes after the frame are refused.

use super::limits::{MAX_ZSTD_WINDOW_BYTES, ZstdHeaderError, check_zstd_frame_header};
use super::tile::{TileCodes, TileError, decode_residuals};
use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

/// Why a tile payload could not be decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TileDecodeError {
    /// The frame header broke a limit.
    Header(ZstdHeaderError),
    /// The frame itself is corrupt.
    Corrupt,
    /// The frame produced a different number of bytes than declared.
    Size,
    /// Bytes follow the frame.
    TrailingData,
    /// The residual transform refused the bytes.
    Tile(TileError),
}

/// Decodes one `delta-zstd1` payload into planar codes.
///
/// # Errors
///
/// [`TileDecodeError`].
pub fn decode_tile(
    payload: &[u8],
    width: u32,
    height: u32,
    depth: u8,
) -> Result<TileCodes, TileDecodeError> {
    let expected = width as usize * height as usize * 6;
    check_zstd_frame_header(payload, expected).map_err(TileDecodeError::Header)?;
    let mut source = payload;
    let mut decoder = FrameDecoder::new();
    decoder.set_max_window_size(MAX_ZSTD_WINDOW_BYTES);
    decoder
        .init(&mut source)
        .map_err(|_| TileDecodeError::Corrupt)?;
    decoder
        .decode_blocks(&mut source, BlockDecodingStrategy::UptoBytes(expected))
        .map_err(|_| TileDecodeError::Corrupt)?;
    if !decoder.is_finished() {
        return Err(TileDecodeError::Size);
    }
    let bytes = decoder.collect().unwrap_or_default();
    if bytes.len() != expected {
        return Err(TileDecodeError::Size);
    }
    if !source.is_empty() {
        return Err(TileDecodeError::TrailingData);
    }
    decode_residuals(&bytes, width, height, depth).map_err(TileDecodeError::Tile)
}
