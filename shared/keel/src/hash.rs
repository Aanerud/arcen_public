// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use crate::{BgraFrame, BlockBounds};
use xxhash_rust::xxh3::xxh3_64_with_seed;

const ROW_SEED: u64 = 0x9e37_79b1_85eb_ca87;
const CRC_MIX_SEED: u64 = 0x6a09_e667_f3bc_c909;
const COMBINE_PRIME: u64 = 0xc2b2_ae3d_27d4_eb4f;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum KernelPreference {
    #[default]
    Auto,
    Xxh3,
    Crc32c,
}

impl KernelPreference {
    #[must_use]
    pub const fn resolve(self) -> HashKernel {
        match self {
            // CRC remains explicitly benchmarkable. Auto stays on XXH3 until
            // corpus evidence justifies a calibrated CPU-class allow-list.
            Self::Auto | Self::Xxh3 => HashKernel::Xxh3,
            Self::Crc32c => HashKernel::Crc32c,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HashKernel {
    Xxh3,
    Crc32c,
}

impl HashKernel {
    pub(crate) fn hash_block(self, frame: BgraFrame<'_>, bounds: BlockBounds) -> u64 {
        match self {
            // A 16x16 BGRA block is pitch-separated in the mapped capture.
            // XXH3's short-input path is faster here than a streaming state:
            // hash each active row directly, then fold the row hashes in
            // order into one session-local 64-bit fingerprint.
            Self::Xxh3 => {
                let mut combined = ROW_SEED ^ bounds.height as u64;
                for row_offset in 0..bounds.height {
                    let row = frame.block_row_segment(bounds.y + row_offset, bounds);
                    combined = fold_row(combined, row_segment_hash(row), row_offset);
                }
                finish_mix(combined)
            }
            Self::Crc32c => {
                let mut crc = 0u32;
                let mut independent = CRC_MIX_SEED ^ bounds.height as u64;
                for row_offset in 0..bounds.height {
                    let row = frame.block_row_segment(bounds.y + row_offset, bounds);
                    crc = crc32c::crc32c_append(crc, row);
                    independent = mix_row(independent, row, row_offset, CRC_MIX_SEED);
                }
                // Two CRC32Cs with different initial values are affine for a
                // fixed block length and provide only 32 bits of collision
                // strength. Pair CRC32C with an independently seeded XXH3 high
                // word to retain a genuine 64-bit damage fingerprint.
                u64::from(crc) | (finish_mix(independent) & 0xffff_ffff_0000_0000)
            }
        }
    }

    /// Hashes a block and writes each row segment's position-independent
    /// hash ([`row_segment_hash`]) to `rows`, which holds at least
    /// `bounds.height` entries. Returns the same block hash as
    /// [`Self::hash_block`].
    pub(crate) fn hash_block_rows(
        self,
        frame: BgraFrame<'_>,
        bounds: BlockBounds,
        rows: &mut [u64],
    ) -> u64 {
        match self {
            Self::Xxh3 => {
                let mut combined = ROW_SEED ^ bounds.height as u64;
                for (row_offset, slot) in rows.iter_mut().enumerate().take(bounds.height) {
                    let row = frame.block_row_segment(bounds.y + row_offset, bounds);
                    *slot = row_segment_hash(row);
                    combined = fold_row(combined, *slot, row_offset);
                }
                finish_mix(combined)
            }
            Self::Crc32c => {
                for (row_offset, slot) in rows.iter_mut().enumerate().take(bounds.height) {
                    *slot =
                        row_segment_hash(frame.block_row_segment(bounds.y + row_offset, bounds));
                }
                self.hash_block(frame, bounds)
            }
        }
    }
}

/// Position-independent hash of one block-row segment: equal pixels give an
/// equal hash wherever they sit vertically, which is what motion search
/// matches on.
#[inline]
pub(crate) fn row_segment_hash(row: &[u8]) -> u64 {
    xxh3_64_with_seed(row, ROW_SEED)
}

#[inline]
const fn fold_row(combined: u64, row_hash: u64, row_offset: usize) -> u64 {
    (combined.rotate_left(17) ^ row_hash ^ (row_offset as u64).wrapping_mul(CRC_MIX_SEED))
        .wrapping_mul(COMBINE_PRIME)
}

#[inline]
fn mix_row(mut combined: u64, row: &[u8], row_offset: usize, seed: u64) -> u64 {
    let row_hash = xxh3_64_with_seed(row, seed.wrapping_mul(row_offset as u64 + 1));
    combined = combined.rotate_left(17) ^ row_hash;
    combined.wrapping_mul(COMBINE_PRIME)
}

#[inline]
const fn finish_mix(combined: u64) -> u64 {
    combined ^ (combined >> 29)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::BlockGrid;

    #[test]
    fn kernels_are_deterministic() {
        let pixels = (0u8..=255).cycle().take(16 * 16 * 4).collect::<Vec<_>>();
        let frame = BgraFrame::new(&pixels, 16, 16, 64).unwrap();
        let bounds = BlockGrid::new(16, 16).unwrap().block_bounds(0).unwrap();
        for kernel in [HashKernel::Xxh3, HashKernel::Crc32c] {
            assert_eq!(
                kernel.hash_block(frame, bounds),
                kernel.hash_block(frame, bounds)
            );
        }
    }

    #[test]
    fn automatic_kernel_is_xxh3_until_calibrated() {
        assert_eq!(KernelPreference::Auto.resolve(), HashKernel::Xxh3);
    }

    #[test]
    fn crc32c_fingerprint_has_an_independent_high_word() {
        let first_pixels = vec![0u8; 16 * 16 * 4];
        let second_pixels = (0u8..=255).cycle().take(16 * 16 * 4).collect::<Vec<_>>();
        let grid = BlockGrid::new(16, 16).unwrap();
        let bounds = grid.block_bounds(0).unwrap();
        let first = HashKernel::Crc32c
            .hash_block(BgraFrame::new(&first_pixels, 16, 16, 64).unwrap(), bounds);
        let second = HashKernel::Crc32c
            .hash_block(BgraFrame::new(&second_pixels, 16, 16, 64).unwrap(), bounds);

        let fold = |hash: u64| {
            let bytes = hash.to_le_bytes();
            u32::from_le_bytes(bytes[..4].try_into().unwrap())
                ^ u32::from_le_bytes(bytes[4..].try_into().unwrap())
        };
        assert_ne!(fold(first), fold(second));
    }
}
