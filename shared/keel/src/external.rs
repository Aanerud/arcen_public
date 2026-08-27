use crate::{BLOCK_SIZE, BlockGrid, DamageMap, DamageSummary, KeelError};

const WORD_BITS: usize = u64::BITS as usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

#[derive(Debug)]
pub struct ExternalDamage {
    grid: BlockGrid,
    dirty_bits: Vec<u64>,
}

impl ExternalDamage {
    /// Creates a reusable accumulator for externally supplied damage.
    ///
    /// # Errors
    ///
    /// Returns the geometry errors documented by [`BlockGrid::new`].
    pub fn new(width: usize, height: usize) -> Result<Self, KeelError> {
        let grid = BlockGrid::new(width, height)?;
        Ok(Self {
            grid,
            dirty_bits: vec![0; grid.block_count().div_ceil(WORD_BITS)],
        })
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.grid
    }

    pub fn reset(&mut self) {
        self.dirty_bits.fill(0);
    }

    /// Conservatively marks every 16x16 Keel block overlapped by `rect`.
    ///
    /// Empty or fully out-of-frame rectangles are ignored. Partially
    /// out-of-frame rectangles are clipped to the frame, including tail blocks.
    pub fn mark_rect(&mut self, rect: PixelRect) {
        let start_x = rect.x.min(self.grid.width());
        let start_y = rect.y.min(self.grid.height());
        let end_x = rect.x.saturating_add(rect.width).min(self.grid.width());
        let end_y = rect.y.saturating_add(rect.height).min(self.grid.height());
        if start_x >= end_x || start_y >= end_y {
            return;
        }

        let first_block_x = start_x / BLOCK_SIZE;
        let first_block_y = start_y / BLOCK_SIZE;
        let end_block_x = end_x.div_ceil(BLOCK_SIZE);
        let end_block_y = end_y.div_ceil(BLOCK_SIZE);
        for block_y in first_block_y..end_block_y {
            for block_x in first_block_x..end_block_x {
                if let Some(index) = self.grid.block_index(block_x, block_y) {
                    set_bit(&mut self.dirty_bits, index);
                }
            }
        }
    }

    /// Marks non-zero one-byte source blocks onto the Keel grid.
    ///
    /// `source_block_size` is the source block width and height in pixels.
    /// `blocks_wide` and `blocks_tall` must exactly cover this frame using
    /// ceiling division; this prevents adapters from guessing driver geometry.
    ///
    /// # Errors
    ///
    /// Returns an error for a zero source block size, mismatched source-map
    /// geometry, overflow, or storage shorter than one byte per source block.
    pub fn mark_block_map(
        &mut self,
        blocks: &[u8],
        blocks_wide: usize,
        blocks_tall: usize,
        source_block_size: usize,
    ) -> Result<(), KeelError> {
        if source_block_size == 0 {
            return Err(KeelError::ExternalBlockSizeZero);
        }
        let expected_wide = self.grid.width().div_ceil(source_block_size);
        let expected_tall = self.grid.height().div_ceil(source_block_size);
        if blocks_wide != expected_wide || blocks_tall != expected_tall {
            return Err(KeelError::ExternalMapGeometry {
                expected_wide,
                expected_tall,
                actual_wide: blocks_wide,
                actual_tall: blocks_tall,
            });
        }
        let required = blocks_wide
            .checked_mul(blocks_tall)
            .ok_or(KeelError::GeometryOverflow)?;
        if blocks.len() < required {
            return Err(KeelError::ExternalMapTooSmall {
                actual: blocks.len(),
                required,
            });
        }

        for (index, dirty) in blocks[..required].iter().copied().enumerate() {
            if dirty == 0 {
                continue;
            }
            let block_x = index % blocks_wide;
            let block_y = index / blocks_wide;
            let x = block_x
                .checked_mul(source_block_size)
                .ok_or(KeelError::GeometryOverflow)?;
            let y = block_y
                .checked_mul(source_block_size)
                .ok_or(KeelError::GeometryOverflow)?;
            self.mark_rect(PixelRect {
                x,
                y,
                width: source_block_size,
                height: source_block_size,
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        DamageMap::new(self.grid, &self.dirty_bits)
    }

    #[must_use]
    pub fn summary(&self) -> DamageSummary {
        let map = self.damage_map();
        let dirty_blocks = map.dirty_blocks().count();
        let dirty_block_rows = (0..self.grid.blocks_tall())
            .filter(|block_y| {
                (0..self.grid.blocks_wide()).any(|block_x| {
                    self.grid
                        .block_index(block_x, *block_y)
                        .is_some_and(|index| map.is_dirty(index))
                })
            })
            .count();
        DamageSummary {
            dirty_blocks,
            total_blocks: self.grid.block_count(),
            dirty_block_rows,
            total_block_rows: self.grid.blocks_tall(),
        }
    }
}

fn set_bit(words: &mut [u64], index: usize) {
    words[index / WORD_BITS] |= 1u64 << (index % WORD_BITS);
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn block_map_requires_exact_coverage_and_storage() {
        let mut damage = ExternalDamage::new(33, 17).unwrap();
        assert!(matches!(
            damage.mark_block_map(&[0; 6], 3, 2, 0),
            Err(KeelError::ExternalBlockSizeZero)
        ));
        assert!(matches!(
            damage.mark_block_map(&[0; 6], 2, 2, 16),
            Err(KeelError::ExternalMapGeometry { .. })
        ));
        assert!(matches!(
            damage.mark_block_map(&[0; 5], 3, 2, 16),
            Err(KeelError::ExternalMapTooSmall { .. })
        ));
    }

    #[test]
    fn rectangles_clip_to_tail_blocks() {
        let mut damage = ExternalDamage::new(33, 17).unwrap();
        damage.mark_rect(PixelRect {
            x: 32,
            y: 16,
            width: usize::MAX,
            height: usize::MAX,
        });
        assert_eq!(damage.damage_map().dirty_blocks().collect::<Vec<_>>(), [5]);
    }
}
