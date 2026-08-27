use core::ops::Range;

use crate::{BgraFrame, BlockGrid, HashKernel, KeelError, KernelPreference};

const WORD_BITS: usize = u64::BITS as usize;

#[derive(Debug)]
pub struct DamageTracker {
    grid: BlockGrid,
    kernel: HashKernel,
    previous_hashes: Vec<u64>,
    dirty_bits: Vec<u64>,
    has_baseline: bool,
}

impl DamageTracker {
    /// Allocates the reusable hash grid and damage bitset.
    ///
    /// # Errors
    ///
    /// Returns the geometry errors documented by [`BlockGrid::new`].
    pub fn new(
        width: usize,
        height: usize,
        preference: KernelPreference,
    ) -> Result<Self, KeelError> {
        let grid = BlockGrid::new(width, height)?;
        let words = grid.block_count().div_ceil(WORD_BITS);
        Ok(Self {
            grid,
            kernel: preference.resolve(),
            previous_hashes: vec![0; grid.block_count()],
            dirty_bits: vec![0; words],
            has_baseline: false,
        })
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.grid
    }

    #[must_use]
    pub const fn kernel(&self) -> HashKernel {
        self.kernel
    }

    pub fn reset(&mut self) {
        self.has_baseline = false;
        self.dirty_bits.fill(0);
    }

    /// Updates the retained hashes and reusable damage map.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without mutating state when the
    /// frame dimensions differ from this tracker.
    pub fn update(&mut self, frame: BgraFrame<'_>) -> Result<DamageSummary, KeelError> {
        if frame.grid() != self.grid {
            return Err(KeelError::GeometryChanged {
                expected: self.grid,
                actual: frame.grid(),
            });
        }

        self.dirty_bits.fill(0);
        let mut dirty_blocks = 0usize;
        for index in 0..self.grid.block_count() {
            let Some(bounds) = self.grid.block_bounds(index) else {
                return Err(KeelError::GeometryOverflow);
            };
            let hash = self.kernel.hash_block(frame, bounds);
            let dirty = !self.has_baseline || self.previous_hashes[index] != hash;
            self.previous_hashes[index] = hash;
            if dirty {
                set_bit(&mut self.dirty_bits, index);
                dirty_blocks += 1;
            }
        }
        self.has_baseline = true;

        let dirty_block_rows = (0..self.grid.blocks_tall())
            .filter(|row| row_is_dirty(self.grid, &self.dirty_bits, *row))
            .count();
        Ok(DamageSummary {
            dirty_blocks,
            total_blocks: self.grid.block_count(),
            dirty_block_rows,
            total_block_rows: self.grid.blocks_tall(),
        })
    }

    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        DamageMap::new(self.grid, &self.dirty_bits)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DamageSummary {
    pub dirty_blocks: usize,
    pub total_blocks: usize,
    pub dirty_block_rows: usize,
    pub total_block_rows: usize,
}

impl DamageSummary {
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn damage_ratio(self) -> f64 {
        self.dirty_blocks as f64 / self.total_blocks as f64
    }

    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn converted_row_ratio(self) -> f64 {
        self.dirty_block_rows as f64 / self.total_block_rows as f64
    }

    #[must_use]
    pub const fn is_clean(self) -> bool {
        self.dirty_blocks == 0
    }

    #[must_use]
    pub const fn is_full_damage(self) -> bool {
        self.dirty_blocks == self.total_blocks
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DamageMap<'a> {
    grid: BlockGrid,
    bits: &'a [u64],
}

impl<'a> DamageMap<'a> {
    pub(crate) const fn new(grid: BlockGrid, bits: &'a [u64]) -> Self {
        Self { grid, bits }
    }

    #[must_use]
    pub const fn grid(self) -> BlockGrid {
        self.grid
    }

    #[must_use]
    pub fn is_dirty(self, block_index: usize) -> bool {
        block_index < self.grid.block_count() && bit_is_set(self.bits, block_index)
    }

    #[must_use]
    pub fn dirty_blocks(self) -> DirtyBlocks<'a> {
        DirtyBlocks {
            map: self,
            next_index: 0,
        }
    }

    #[must_use]
    pub fn dirty_block_rows(self) -> DirtyBlockRows<'a> {
        DirtyBlockRows {
            map: self,
            next_block_row: 0,
        }
    }

    fn row_is_dirty(self, block_row: usize) -> bool {
        row_is_dirty(self.grid, self.bits, block_row)
    }
}

#[derive(Clone, Debug)]
pub struct DirtyBlocks<'a> {
    map: DamageMap<'a>,
    next_index: usize,
}

impl Iterator for DirtyBlocks<'_> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_index < self.map.grid.block_count() {
            let index = self.next_index;
            self.next_index += 1;
            if self.map.is_dirty(index) {
                return Some(index);
            }
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct DirtyBlockRows<'a> {
    map: DamageMap<'a>,
    next_block_row: usize,
}

impl Iterator for DirtyBlockRows<'_> {
    /// Full-width pixel rows, coalesced across adjacent dirty 16-row bands.
    type Item = Range<usize>;

    fn next(&mut self) -> Option<Self::Item> {
        while self.next_block_row < self.map.grid.blocks_tall()
            && !self.map.row_is_dirty(self.next_block_row)
        {
            self.next_block_row += 1;
        }
        if self.next_block_row >= self.map.grid.blocks_tall() {
            return None;
        }

        let first = self.next_block_row;
        self.next_block_row += 1;
        while self.next_block_row < self.map.grid.blocks_tall()
            && self.map.row_is_dirty(self.next_block_row)
        {
            self.next_block_row += 1;
        }

        let start = self.map.grid.block_row_pixels(first)?.start;
        let end = self.map.grid.block_row_pixels(self.next_block_row - 1)?.end;
        Some(start..end)
    }
}

fn set_bit(words: &mut [u64], index: usize) {
    words[index / WORD_BITS] |= 1u64 << (index % WORD_BITS);
}

fn bit_is_set(words: &[u64], index: usize) -> bool {
    words
        .get(index / WORD_BITS)
        .is_some_and(|word| word & (1u64 << (index % WORD_BITS)) != 0)
}

fn row_is_dirty(grid: BlockGrid, words: &[u64], block_row: usize) -> bool {
    if block_row >= grid.blocks_tall() {
        return false;
    }
    let start = block_row * grid.blocks_wide();
    (start..start + grid.blocks_wide()).any(|index| bit_is_set(words, index))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn frame(pixels: &[u8], width: usize, height: usize) -> BgraFrame<'_> {
        BgraFrame::new(pixels, width, height, width * 4).unwrap()
    }

    #[test]
    fn first_frame_is_dirty_then_identical_frame_is_clean() {
        let pixels = vec![7u8; 32 * 32 * 4];
        let mut tracker = DamageTracker::new(32, 32, KernelPreference::Xxh3).unwrap();
        let first = tracker.update(frame(&pixels, 32, 32)).unwrap();
        assert!(first.is_full_damage());
        let second = tracker.update(frame(&pixels, 32, 32)).unwrap();
        assert!(second.is_clean());
        assert_eq!(tracker.damage_map().dirty_blocks().count(), 0);
    }

    #[test]
    fn one_dirty_block_converts_its_full_block_row() {
        let mut pixels = vec![0u8; 32 * 32 * 4];
        let mut tracker = DamageTracker::new(32, 32, KernelPreference::Xxh3).unwrap();
        tracker.update(frame(&pixels, 32, 32)).unwrap();
        pixels[(20 * 32 + 20) * 4] = 1;
        let summary = tracker.update(frame(&pixels, 32, 32)).unwrap();
        assert_eq!(summary.dirty_blocks, 1);
        let rows = tracker.damage_map().dirty_block_rows().collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0], 16..32);
    }

    #[test]
    fn repeated_updates_reuse_hash_and_bitset_storage() {
        let mut pixels = vec![0u8; 64 * 64 * 4];
        let mut tracker = DamageTracker::new(64, 64, KernelPreference::Xxh3).unwrap();
        let hash_ptr = tracker.previous_hashes.as_ptr();
        let bitset_ptr = tracker.dirty_bits.as_ptr();
        let hash_capacity = tracker.previous_hashes.capacity();
        let bitset_capacity = tracker.dirty_bits.capacity();

        for tick in 0..128 {
            pixels[(tick % (64 * 64)) * 4] ^= 1;
            tracker.update(frame(&pixels, 64, 64)).unwrap();
        }

        assert_eq!(tracker.previous_hashes.as_ptr(), hash_ptr);
        assert_eq!(tracker.dirty_bits.as_ptr(), bitset_ptr);
        assert_eq!(tracker.previous_hashes.capacity(), hash_capacity);
        assert_eq!(tracker.dirty_bits.capacity(), bitset_capacity);
    }
}
