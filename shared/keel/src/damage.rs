// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use core::ops::Range;

use crate::{BLOCK_SIZE, BgraFrame, BlockGrid, HashKernel, KeelError, KernelPreference};

pub(crate) const WORD_BITS: usize = u64::BITS as usize;

/// Default number of narrowed updates over which the audit stripe sweeps the
/// whole frame once (about one second at 30 fps, half a second at 60).
pub const DEFAULT_AUDIT_PERIOD: usize = 30;

#[derive(Debug)]
pub struct DamageTracker {
    grid: BlockGrid,
    kernel: HashKernel,
    previous_hashes: Vec<u64>,
    dirty_bits: Vec<u64>,
    has_baseline: bool,
    audit_period: usize,
    audit_cursor: usize,
    last_hashed_blocks: usize,
    last_was_baseline: bool,
    rows: Option<RowPlanes>,
}

/// Retained position-independent row-segment hashes, `BLOCK_SIZE` per block
/// (block-major), for the current and the previous frame.
#[derive(Debug)]
pub(crate) struct RowPlanes {
    pub(crate) current: Vec<u64>,
    pub(crate) previous: Vec<u64>,
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
            audit_period: DEFAULT_AUDIT_PERIOD,
            audit_cursor: 0,
            last_hashed_blocks: 0,
            last_was_baseline: false,
            rows: None,
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

    /// Whether a baseline exists, so a narrowed update can be trusted.
    #[must_use]
    pub const fn has_baseline(&self) -> bool {
        self.has_baseline
    }

    /// Blocks hashed by the most recent update: the whole grid for
    /// [`Self::update`], OS damage plus the audit stripe for
    /// [`Self::update_within`]. The work meter's hash cost basis.
    #[must_use]
    pub const fn last_hashed_blocks(&self) -> usize {
        self.last_hashed_blocks
    }

    /// Narrowed updates per full audit sweep.
    #[must_use]
    pub const fn audit_period(&self) -> usize {
        self.audit_period
    }

    /// Sets how many narrowed updates the audit stripe takes to cover the
    /// frame once. The stripe is `ceil(block_rows / period)` block rows tall,
    /// so a block whose change the OS never reported is found within
    /// `period` narrowed updates. Values below one are treated as one, which
    /// audits the whole frame every update.
    pub fn set_audit_period(&mut self, period: usize) {
        self.audit_period = period.max(1);
    }

    pub fn reset(&mut self) {
        self.has_baseline = false;
        self.dirty_bits.fill(0);
    }

    /// Starts retaining every block row's hash for the current and previous
    /// frame, which pixel motion search ([`crate::MotionEstimator`]) reads.
    /// Costs 256 bytes per block (8 MB at 3840x2160) and one copy of each
    /// changed block's rows per update; with the CRC32C kernel the rows are
    /// hashed a second time. Takes effect from a fresh baseline: the next
    /// update hashes the whole frame.
    pub fn retain_row_hashes(&mut self) {
        if self.rows.is_none() {
            let len = self.grid.block_count() * BLOCK_SIZE;
            self.rows = Some(RowPlanes {
                current: vec![0; len],
                previous: vec![0; len],
            });
            self.reset();
        }
    }

    #[must_use]
    pub const fn retains_row_hashes(&self) -> bool {
        self.rows.is_some()
    }

    /// Whether the latest update established a baseline (every block dirty,
    /// no previous frame to compare with).
    #[must_use]
    pub const fn last_was_baseline(&self) -> bool {
        self.last_was_baseline
    }

    pub(crate) const fn row_planes(&self) -> Option<&RowPlanes> {
        self.rows.as_ref()
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

        self.last_was_baseline = !self.has_baseline;
        self.roll_row_hashes();
        self.dirty_bits.fill(0);
        let mut dirty_blocks = 0usize;
        for index in 0..self.grid.block_count() {
            if self.hash_block(frame, index)? {
                dirty_blocks += 1;
            }
        }
        self.has_baseline = true;
        self.last_hashed_blocks = self.grid.block_count();
        Ok(self.summary(dirty_blocks))
    }

    /// Updates only the blocks the OS reported as damaged, plus a rotating
    /// audit stripe of whole block rows.
    ///
    /// Blocks outside `os_damage` and the stripe keep their retained hash and
    /// are reported clean, so the cost follows the reported damage instead of
    /// the frame size. When the report is a superset of the real change the
    /// result is identical to [`Self::update`]. A change the OS failed to
    /// report is found when the stripe reaches it, within
    /// [`Self::audit_period`] narrowed updates. Without a baseline (first
    /// frame, after [`Self::reset`]) this performs a full update.
    ///
    /// Use [`Self::update`] instead when the OS damage is
    /// [`crate::DamageMetadataStatus::Unknown`].
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without mutating state when the
    /// frame or the damage map has another geometry.
    pub fn update_within(
        &mut self,
        frame: BgraFrame<'_>,
        os_damage: DamageMap<'_>,
    ) -> Result<DamageSummary, KeelError> {
        for actual in [frame.grid(), os_damage.grid()] {
            if actual != self.grid {
                return Err(KeelError::GeometryChanged {
                    expected: self.grid,
                    actual,
                });
            }
        }
        if !self.has_baseline {
            return self.update(frame);
        }

        self.last_was_baseline = false;
        self.roll_row_hashes();
        self.dirty_bits.fill(0);
        let blocks_wide = self.grid.blocks_wide();
        let audit_rows = self.next_audit_rows();
        let audit_blocks = audit_rows.start * blocks_wide..audit_rows.end * blocks_wide;

        let mut dirty_blocks = 0usize;
        let mut hashed = 0usize;
        for (word_index, &word) in os_damage.bits.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let index = word_index * WORD_BITS + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                if index >= self.grid.block_count() || audit_blocks.contains(&index) {
                    continue;
                }
                hashed += 1;
                if self.hash_block(frame, index)? {
                    dirty_blocks += 1;
                }
            }
        }
        for index in audit_blocks {
            hashed += 1;
            if self.hash_block(frame, index)? {
                dirty_blocks += 1;
            }
        }
        self.last_hashed_blocks = hashed;
        Ok(self.summary(dirty_blocks))
    }

    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        DamageMap::new(self.grid, &self.dirty_bits)
    }

    /// Hashes one block, retains the hash and marks it when changed.
    fn hash_block(&mut self, frame: BgraFrame<'_>, index: usize) -> Result<bool, KeelError> {
        let Some(bounds) = self.grid.block_bounds(index) else {
            return Err(KeelError::GeometryOverflow);
        };
        let hash = match self.rows.as_mut() {
            Some(rows) => {
                let start = index * BLOCK_SIZE;
                self.kernel.hash_block_rows(
                    frame,
                    bounds,
                    &mut rows.current[start..start + BLOCK_SIZE],
                )
            }
            None => self.kernel.hash_block(frame, bounds),
        };
        let dirty = !self.has_baseline || self.previous_hashes[index] != hash;
        self.previous_hashes[index] = hash;
        if dirty {
            set_bit(&mut self.dirty_bits, index);
        }
        Ok(dirty)
    }

    /// Makes the previous frame's row hashes current again for every block
    /// the last update changed, before this update overwrites them.
    fn roll_row_hashes(&mut self) {
        let Some(rows) = self.rows.as_mut() else {
            return;
        };
        for (word_index, &word) in self.dirty_bits.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let index = word_index * WORD_BITS + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                let start = index * BLOCK_SIZE;
                if let (Some(previous), Some(current)) = (
                    rows.previous.get_mut(start..start + BLOCK_SIZE),
                    rows.current.get(start..start + BLOCK_SIZE),
                ) {
                    previous.copy_from_slice(current);
                }
            }
        }
    }

    /// The block rows the next narrowed update audits, advancing the cursor.
    fn next_audit_rows(&mut self) -> Range<usize> {
        let rows = self.grid.blocks_tall();
        let stripe = rows.div_ceil(self.audit_period).max(1);
        if self.audit_cursor >= rows {
            self.audit_cursor = 0;
        }
        let start = self.audit_cursor;
        let end = (start + stripe).min(rows);
        self.audit_cursor = end;
        start..end
    }

    fn summary(&self, dirty_blocks: usize) -> DamageSummary {
        let dirty_block_rows = (0..self.grid.blocks_tall())
            .filter(|row| row_is_dirty(self.grid, &self.dirty_bits, *row))
            .count();
        DamageSummary {
            dirty_blocks,
            total_blocks: self.grid.block_count(),
            dirty_block_rows,
            total_block_rows: self.grid.blocks_tall(),
        }
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
    pub(crate) bits: &'a [u64],
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
    any_bit_in_range(words, start, start + grid.blocks_wide())
}

/// Whether any bit in `[start, end)` is set, a word at a time.
pub(crate) fn any_bit_in_range(words: &[u64], start: usize, end: usize) -> bool {
    let mut index = start;
    while index < end {
        let word_index = index / WORD_BITS;
        let Some(&word) = words.get(word_index) else {
            return false;
        };
        let offset = index % WORD_BITS;
        let span = (end - index).min(WORD_BITS - offset);
        let mask = if span == WORD_BITS {
            u64::MAX
        } else {
            ((1u64 << span) - 1) << offset
        };
        if word & mask != 0 {
            return true;
        }
        index += span;
    }
    false
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
