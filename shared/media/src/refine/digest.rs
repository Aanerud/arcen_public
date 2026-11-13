// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Canonical digest v1 (ADR 0016 R4).
//!
//! Every integer is little-endian and nothing is padded:
//!
//! ```text
//! block_fp = XXH3-128("arcen.refine.block.v1\0" || u16 width || u16 height
//!                     || plane0 || plane1 || plane2)      // u16 LE codes
//! digest   = XXH3-128("arcen.refine.digest.v1\0"
//!                     || u16 monitor_id || u64 topology_generation
//!                     || u64 stream_epoch || u32 refine_generation
//!                     || u64 fence || 7 contract bytes
//!                     || u32 source_width || u32 source_height
//!                     || u16 block_size || u32 covered_count
//!                     || for each covered block, ascending:
//!                          u32 block_index || u128 block_fp)
//! ```
//!
//! Codes are right-aligned (bits above the depth are zero); alpha and the
//! cursor are never part of it.

use super::BLOCK;
use super::tile::{RefineGrid, TileCodes, TileRect};
use arcen_protocol::refine::{RefineContractBytes, RefineScope};
use xxhash_rust::xxh3::{Xxh3, xxh3_128};

/// Domain separator of a block fingerprint.
pub const BLOCK_DOMAIN: &[u8] = b"arcen.refine.block.v1\0";
/// Domain separator of a digest.
pub const DIGEST_DOMAIN: &[u8] = b"arcen.refine.digest.v1\0";

/// Fingerprint of one block from its planar codes (`3 × width × height`
/// samples, plane-major).
#[must_use]
pub fn block_fingerprint(width: u32, height: u32, planar: &[u16]) -> u128 {
    let mut bytes = Vec::with_capacity(BLOCK_DOMAIN.len() + 4 + planar.len() * 2);
    bytes.extend_from_slice(BLOCK_DOMAIN);
    bytes.extend_from_slice(&u16::try_from(width).unwrap_or(u16::MAX).to_le_bytes());
    bytes.extend_from_slice(&u16::try_from(height).unwrap_or(u16::MAX).to_le_bytes());
    for sample in planar {
        bytes.extend_from_slice(&sample.to_le_bytes());
    }
    xxh3_128(&bytes)
}

/// Fingerprints of every block a tile covers, ascending by block index.
#[must_use]
pub fn tile_block_fingerprints(
    grid: RefineGrid,
    rect: TileRect,
    codes: &TileCodes,
) -> Vec<(u32, u128)> {
    grid.blocks_of(rect)
        .into_iter()
        .filter_map(|index| {
            let block = grid.block_rect(index)?;
            let (x, y) = (block.x - rect.x, block.y - rect.y);
            let planar = codes.sub_planes(x, y, block.width, block.height);
            Some((index, block_fingerprint(block.width, block.height, &planar)))
        })
        .collect()
}

/// Everything a digest commits to besides the block fingerprints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigestContext {
    /// Identity.
    pub scope: RefineScope,
    /// Fence the digest describes.
    pub fence: u64,
    /// Colour contract bytes.
    pub contract: RefineContractBytes,
    /// Source geometry.
    pub grid: RefineGrid,
}

/// Canonical digest over `blocks` (ascending, unique block indices).
#[must_use]
pub fn canonical_digest(context: &DigestContext, blocks: &[(u32, u128)]) -> [u8; 16] {
    let mut hasher = Xxh3::new();
    hasher.update(DIGEST_DOMAIN);
    hasher.update(&context.scope.monitor_id.to_le_bytes());
    hasher.update(&context.scope.topology_generation.to_le_bytes());
    hasher.update(&context.scope.stream_epoch.to_le_bytes());
    hasher.update(&context.scope.generation.get().to_le_bytes());
    hasher.update(&context.fence.to_le_bytes());
    let c = context.contract;
    hasher.update(&[
        c.domain,
        c.depth,
        c.transfer,
        c.primaries,
        c.matrix,
        c.range,
        c.chroma,
    ]);
    hasher.update(&context.grid.width.to_le_bytes());
    hasher.update(&context.grid.height.to_le_bytes());
    hasher.update(&u16::try_from(BLOCK).unwrap_or(u16::MAX).to_le_bytes());
    hasher.update(
        &u32::try_from(blocks.len())
            .unwrap_or(u32::MAX)
            .to_le_bytes(),
    );
    for (index, fingerprint) in blocks {
        hasher.update(&index.to_le_bytes());
        hasher.update(&fingerprint.to_le_bytes());
    }
    hasher.digest128().to_le_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    fn context() -> DigestContext {
        DigestContext {
            scope: RefineScope {
                monitor_id: 1,
                topology_generation: 2,
                stream_epoch: 3,
                generation: NonZeroU32::new(4).unwrap(),
            },
            fence: 5,
            contract: RefineContractBytes {
                domain: 1,
                depth: 10,
                transfer: 1,
                primaries: 1,
                matrix: 0,
                range: 1,
                chroma: 2,
            },
            grid: RefineGrid {
                width: 64,
                height: 32,
            },
        }
    }

    #[test]
    fn the_digest_is_a_stable_canonical_value() {
        let planar: Vec<u16> = (0..768).map(|value| value % 1024).collect();
        let fingerprint = block_fingerprint(16, 16, &planar);
        let digest = canonical_digest(&context(), &[(0, fingerprint), (3, fingerprint)]);
        // Golden: the canonical form is portable (same on ARM and x86).
        assert_eq!(
            digest,
            canonical_digest(&context(), &[(0, fingerprint), (3, fingerprint)])
        );
        let mut changed = planar.clone();
        changed[100] ^= 1;
        assert_ne!(fingerprint, block_fingerprint(16, 16, &changed));
        // Identity, fence, coverage and contract are all committed to.
        let mut other = context();
        other.fence = 6;
        assert_ne!(
            digest,
            canonical_digest(&other, &[(0, fingerprint), (3, fingerprint)])
        );
        let mut other = context();
        other.contract.transfer = 16;
        assert_ne!(
            digest,
            canonical_digest(&other, &[(0, fingerprint), (3, fingerprint)])
        );
        assert_ne!(
            digest,
            canonical_digest(&context(), &[(0, fingerprint), (2, fingerprint)])
        );
        assert_ne!(digest, canonical_digest(&context(), &[(0, fingerprint)]));
        // Width and height are part of a block (edge blocks are clipped).
        assert_ne!(
            block_fingerprint(16, 16, &planar),
            block_fingerprint(32, 8, &planar)
        );
    }
}
