// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Normative limits (ADR 0016 R5) and the codec-free Zstandard frame-header
//! check that runs before any decoder allocates.

use std::collections::VecDeque;
use std::time::Duration;

pub use arcen_protocol::refine::{
    MAX_TILE_EXPANDED_BYTES, MAX_TILE_HEIGHT, MAX_TILE_PAYLOAD_BYTES, MAX_TILE_WIDTH,
};

/// Largest Zstandard window a tile may declare.
pub const MAX_ZSTD_WINDOW_BYTES: u64 = 128 * 1024;
/// Deck: decoded bytes received but not yet committed, per session.
pub const MAX_PENDING_DECODED_BYTES: usize = 8 * 1024 * 1024;
/// Deck: jobs received but not yet committed, per session.
pub const MAX_PENDING_JOBS: usize = 256;
/// Deck: a job older than this is dropped.
pub const MAX_PENDING_AGE: Duration = Duration::from_secs(2);
/// Deck: overlay bytes staged for the GPU per presented frame.
pub const MAX_STAGING_BYTES_PER_FRAME: usize = 4 * 1024 * 1024;
/// Deck: tiles accepted per second per session.
pub const MAX_TILES_PER_SECOND: usize = 4_000;
/// Malformed records within [`MALFORMED_WINDOW`] that disable refinement.
pub const MALFORMED_DISABLE_COUNT: usize = 8;
/// Window of [`MALFORMED_DISABLE_COUNT`].
pub const MALFORMED_WINDOW: Duration = Duration::from_secs(10);

const ZSTD_MAGIC: u32 = 0xFD2F_B528;
const SKIPPABLE_MAGIC_MASK: u32 = 0xFFFF_FFF0;
const SKIPPABLE_MAGIC: u32 = 0x184D_2A50;

/// Why a compressed payload was refused before decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZstdHeaderError {
    /// Fewer bytes than the header needs.
    Short,
    /// A skippable frame.
    Skippable,
    /// Not a Zstandard frame.
    Magic,
    /// The reserved descriptor bit is set.
    Reserved,
    /// A dictionary id is present.
    Dictionary,
    /// No content size is declared.
    NoContentSize,
    /// The declared content size differs from the expected expanded size.
    ContentSize,
    /// The window exceeds [`MAX_ZSTD_WINDOW_BYTES`].
    Window,
    /// The payload exceeds [`MAX_TILE_PAYLOAD_BYTES`].
    TooLarge,
}

/// Checks one tile payload's Zstandard frame header (RFC 8878 §3.1.1):
/// exactly one standard frame, declared content size equal to `expected`,
/// window at most 128 KiB, no dictionary. Trailing data after the frame is
/// refused by the decoder adapter, which knows where the frame ends.
///
/// # Errors
///
/// [`ZstdHeaderError`].
pub fn check_zstd_frame_header(payload: &[u8], expected: usize) -> Result<(), ZstdHeaderError> {
    if payload.len() > MAX_TILE_PAYLOAD_BYTES {
        return Err(ZstdHeaderError::TooLarge);
    }
    let magic = payload
        .get(..4)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
        .ok_or(ZstdHeaderError::Short)?;
    if magic & SKIPPABLE_MAGIC_MASK == SKIPPABLE_MAGIC {
        return Err(ZstdHeaderError::Skippable);
    }
    if magic != ZSTD_MAGIC {
        return Err(ZstdHeaderError::Magic);
    }
    let descriptor = *payload.get(4).ok_or(ZstdHeaderError::Short)?;
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    if descriptor & 0x08 != 0 {
        return Err(ZstdHeaderError::Reserved);
    }
    if descriptor & 0x03 != 0 {
        return Err(ZstdHeaderError::Dictionary);
    }
    let mut at = 5;
    let window = if single_segment {
        None
    } else {
        let byte = *payload.get(at).ok_or(ZstdHeaderError::Short)?;
        at += 1;
        let log = 10 + u64::from(byte >> 3);
        if log > 30 {
            return Err(ZstdHeaderError::Window);
        }
        let base = 1_u64 << log;
        Some(base + (base / 8) * u64::from(byte & 0x07))
    };
    let fcs_bytes = match (fcs_flag, single_segment) {
        (0, false) => return Err(ZstdHeaderError::NoContentSize),
        (0, true) => 1,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    let field = payload
        .get(at..at + fcs_bytes)
        .ok_or(ZstdHeaderError::Short)?;
    let mut raw = [0_u8; 8];
    raw[..fcs_bytes].copy_from_slice(field);
    let mut content = u64::from_le_bytes(raw);
    if fcs_bytes == 2 {
        content += 256;
    }
    if content != expected as u64 {
        return Err(ZstdHeaderError::ContentSize);
    }
    // A single-segment frame's window is its content size.
    if window.unwrap_or(content) > MAX_ZSTD_WINDOW_BYTES {
        return Err(ZstdHeaderError::Window);
    }
    Ok(())
}

/// Counts events in a sliding window (malformed records, tile rate).
#[derive(Debug, Clone, Default)]
pub struct SlidingCount {
    events: VecDeque<Duration>,
}

impl SlidingCount {
    /// Records one event at `now` and returns the count within `window`.
    pub fn note(&mut self, now: Duration, window: Duration) -> usize {
        while self
            .events
            .front()
            .is_some_and(|first| now.saturating_sub(*first) >= window)
        {
            self.events.pop_front();
        }
        self.events.push_back(now);
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(descriptor: u8, rest: &[u8]) -> Vec<u8> {
        let mut out = ZSTD_MAGIC.to_le_bytes().to_vec();
        out.push(descriptor);
        out.extend_from_slice(rest);
        out
    }

    #[test]
    fn malformed_zstd_headers_are_refused_before_decoding() {
        // Single segment, 4-byte content size 98304.
        let ok = header(0b1010_0000, &98_304_u32.to_le_bytes());
        assert_eq!(check_zstd_frame_header(&ok, 98_304), Ok(()));
        assert_eq!(
            check_zstd_frame_header(&ok, 98_303),
            Err(ZstdHeaderError::ContentSize)
        );
        // Window descriptor 2^17 = 128 KiB, 2-byte content size (+256).
        let windowed = header(0b0100_0000, &[(17 - 10) << 3, 0, 1]);
        assert_eq!(check_zstd_frame_header(&windowed, 512), Ok(()));
        let huge_window = header(0b0100_0000, &[(18 - 10) << 3, 0, 1]);
        assert_eq!(
            check_zstd_frame_header(&huge_window, 512),
            Err(ZstdHeaderError::Window)
        );
        let dictionary = header(0b1010_0001, &[1, 0, 0, 0, 0, 0]);
        assert_eq!(
            check_zstd_frame_header(&dictionary, 0),
            Err(ZstdHeaderError::Dictionary)
        );
        let no_size = header(0b0000_0000, &[(17 - 10) << 3]);
        assert_eq!(
            check_zstd_frame_header(&no_size, 512),
            Err(ZstdHeaderError::NoContentSize)
        );
        let reserved = header(0b1010_1000, &98_304_u32.to_le_bytes());
        assert_eq!(
            check_zstd_frame_header(&reserved, 98_304),
            Err(ZstdHeaderError::Reserved)
        );
        let mut skippable = 0x184D_2A53_u32.to_le_bytes().to_vec();
        skippable.extend_from_slice(&[0; 8]);
        assert_eq!(
            check_zstd_frame_header(&skippable, 0),
            Err(ZstdHeaderError::Skippable)
        );
        assert_eq!(
            check_zstd_frame_header(&[0x28, 0xB5], 0),
            Err(ZstdHeaderError::Short)
        );
        assert_eq!(
            check_zstd_frame_header(&[0; 8], 0),
            Err(ZstdHeaderError::Magic)
        );
        let truncated = header(0b1010_0000, &[0, 0]);
        assert_eq!(
            check_zstd_frame_header(&truncated, 0),
            Err(ZstdHeaderError::Short)
        );
        assert_eq!(
            check_zstd_frame_header(&vec![0; MAX_TILE_PAYLOAD_BYTES + 1], 0),
            Err(ZstdHeaderError::TooLarge)
        );
    }

    #[test]
    fn sliding_count_forgets_old_events() {
        let mut count = SlidingCount::default();
        let window = Duration::from_secs(10);
        for second in 0..8 {
            assert_eq!(
                count.note(Duration::from_secs(second), window),
                second as usize + 1
            );
        }
        assert_eq!(count.note(Duration::from_secs(12), window), 6);
    }
}
