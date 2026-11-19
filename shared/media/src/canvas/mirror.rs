// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The canvas mirror: [`CanvasState`] and [`CanvasPixels`] together, as the
//! Pier keeps each monitor's canvas (ADR 0028 §1), and the canvas digest.
//!
//! The digest is refine's canonical digest v1 (ADR 0016 R4) under the
//! canvas contract ([`super::canvas_contract`]: eight-bit R′G′B′), the
//! canvas scope and fence, over every block that holds exact pixels. A
//! Deck computes the same digest from its own canvas (read back) to prove
//! it equals the mirror.

use super::canvas_contract_bytes;
use super::pixels::{CanvasPixels, SurfaceDecoder, TileDecoder};
use super::state::{CanvasState, CanvasStateError};
use crate::refine::digest::{DigestContext, block_fingerprint, canonical_digest};
use crate::refine::tile::TileRect;
use arcen_protocol::canvas::{CanvasRecord, CanvasTerms};

/// A canvas digest at a fence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CanvasDigest {
    /// Fence the digest describes (0 right after a reset).
    pub fence: u64,
    /// Canonical digest v1.
    pub digest: [u8; 16],
    /// Exact blocks it covers.
    pub covered: u32,
}

/// The digest of a canvas whose pixels are `bgra` (opaque BGRA8, `stride`
/// bytes a row) under `state`; `None` before the first reset or when the
/// pixels are smaller than the canvas.
#[must_use]
pub fn canvas_digest(state: &CanvasState, bgra: &[u8], stride: usize) -> Option<CanvasDigest> {
    let grid = state.grid()?;
    let scope = state.scope()?;
    let needed = stride.checked_mul(grid.height as usize)?;
    if stride < grid.width as usize * 4 || bgra.len() < needed {
        return None;
    }
    let blocks: Vec<(u32, u128)> = state
        .lossless_blocks()
        .filter_map(|index| {
            let block = grid.block_rect(index)?;
            Some((
                index,
                block_fingerprint(block.width, block.height, &planar(bgra, stride, block)),
            ))
        })
        .collect();
    let context = DigestContext {
        scope,
        fence: state.fence(),
        contract: canvas_contract_bytes(),
        grid,
    };
    Some(CanvasDigest {
        fence: state.fence(),
        digest: canonical_digest(&context, &blocks),
        covered: u32::try_from(blocks.len()).unwrap_or(u32::MAX),
    })
}

/// Planar R′G′B′ codes of `rect` of a BGRA8 picture.
fn planar(bgra: &[u8], stride: usize, rect: TileRect) -> Vec<u16> {
    let pixels = rect.pixels();
    let mut out = vec![0_u16; pixels * 3];
    let mut at = 0;
    for row in rect.y..rect.y + rect.height {
        let start = row as usize * stride + rect.x as usize * 4;
        for pixel in bgra[start..start + rect.width as usize * 4].chunks_exact(4) {
            out[at] = u16::from(pixel[2]);
            out[pixels + at] = u16::from(pixel[1]);
            out[2 * pixels + at] = u16::from(pixel[0]);
            at += 1;
        }
    }
    out
}

/// One monitor's canvas as the Pier mirrors it (and as a CPU Deck holds
/// it).
#[derive(Debug, Clone)]
pub struct CanvasMirror {
    state: CanvasState,
    pixels: CanvasPixels,
}

impl CanvasMirror {
    /// A mirror for `monitor_id` under the Deck's terms.
    #[must_use]
    pub fn new(monitor_id: u16, terms: CanvasTerms) -> Self {
        Self {
            state: CanvasState::new(monitor_id, terms),
            pixels: CanvasPixels::new(),
        }
    }

    /// The state owner.
    #[must_use]
    pub const fn state(&self) -> &CanvasState {
        &self.state
    }

    /// The pixels.
    #[must_use]
    pub const fn pixels(&self) -> &CanvasPixels {
        &self.pixels
    }

    /// Validates and performs one record. Any refusal or execution failure
    /// breaks the state until the next reset.
    ///
    /// # Errors
    ///
    /// [`CanvasStateError`] ([`CanvasStateError::Execution`] when a tile or
    /// surface did not decode).
    pub fn apply(
        &mut self,
        record: &CanvasRecord<'_>,
        tiles: &mut dyn TileDecoder,
        surfaces: &mut dyn SurfaceDecoder,
    ) -> Result<(), CanvasStateError> {
        let work = self.state.apply(record)?;
        self.pixels.execute(&work, tiles, surfaces).map_err(|_| {
            self.state.mark_broken();
            CanvasStateError::Execution
        })
    }

    /// The digest of the canvas at its newest fence.
    #[must_use]
    pub fn digest(&self) -> Option<CanvasDigest> {
        canvas_digest(&self.state, self.pixels.bgra(), self.pixels.stride())
    }
}
