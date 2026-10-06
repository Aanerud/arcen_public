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

/// Reusable latest-wins accumulator for OS-provided damage.
///
/// A capture backend may deliver more than one raw frame before the encoder
/// submits one picture. Those observations describe one pending encoded frame,
/// so they must be unioned until the submission consumes them. The next
/// observation after that starts a fresh map.
#[derive(Debug)]
pub struct PendingExternalDamage {
    damage: ExternalDamage,
    pending: bool,
}

/// Metadata verdict for OS damage reports.
///
/// `Unknown` means the adapter must conservatively mark the full frame rather
/// than treat a missing or unreadable rectangle list as "clean".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DamageMetadataStatus {
    Complete,
    Unknown,
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

    pub fn mark_full_frame(&mut self) {
        self.mark_rect(PixelRect {
            x: 0,
            y: 0,
            width: self.grid.width(),
            height: self.grid.height(),
        });
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

    /// Conservatively marks every 16x16 Keel block overlapped by signed pixel
    /// bounds.
    ///
    /// This is the adapter seam for OS dirty-rectangle APIs: Windows RECT,
    /// CoreGraphics `CGRect` and similar sources can name partially out-of-frame
    /// bounds. Clipping stays here so hosts do not each grow their own
    /// saturating-cast policy.
    pub fn mark_rect_bounds(&mut self, left: i64, top: i64, right: i64, bottom: i64) {
        let start_x = clamp_signed_to_extent(left, self.grid.width());
        let start_y = clamp_signed_to_extent(top, self.grid.height());
        let end_x = clamp_signed_to_extent(right, self.grid.width());
        let end_y = clamp_signed_to_extent(bottom, self.grid.height());
        if start_x >= end_x || start_y >= end_y {
            return;
        }
        self.mark_rect(PixelRect {
            x: start_x,
            y: start_y,
            width: end_x - start_x,
            height: end_y - start_y,
        });
    }

    /// Conservatively marks a moved rectangle's destination and source.
    ///
    /// Move-rect APIs often describe pixels copied from `source` to
    /// `destination`. The destination is definitely damaged; the source is
    /// marked as well because another OS damage rect may cover the reveal, and
    /// over-marking spends bits while under-marking starves changed content.
    pub fn mark_move_rect(
        &mut self,
        source_x: i64,
        source_y: i64,
        destination_left: i64,
        destination_top: i64,
        destination_right: i64,
        destination_bottom: i64,
    ) {
        self.mark_rect_bounds(
            destination_left,
            destination_top,
            destination_right,
            destination_bottom,
        );
        let width = destination_right.saturating_sub(destination_left);
        let height = destination_bottom.saturating_sub(destination_top);
        if width <= 0 || height <= 0 {
            return;
        }
        self.mark_rect_bounds(
            source_x,
            source_y,
            source_x.saturating_add(width),
            source_y.saturating_add(height),
        );
    }

    /// Merges another damage map for the same frame geometry.
    ///
    /// Used by latest-wins capture queues: when newer raw frames supersede
    /// older ones before encode, their dirty regions are unioned into the
    /// encoded frame so the QP map still describes everything that changed
    /// since the last submitted picture.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] when the map was built for a
    /// different frame size.
    pub fn merge_from(&mut self, damage: DamageMap<'_>) -> Result<(), KeelError> {
        if damage.grid() != self.grid {
            return Err(KeelError::GeometryChanged {
                expected: self.grid,
                actual: damage.grid(),
            });
        }
        for index in damage.dirty_blocks() {
            set_bit(&mut self.dirty_bits, index);
        }
        Ok(())
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

impl PendingExternalDamage {
    /// Creates a reusable latest-wins damage accumulator.
    ///
    /// # Errors
    ///
    /// Returns the geometry errors documented by [`BlockGrid::new`].
    pub fn new(width: usize, height: usize) -> Result<Self, KeelError> {
        Ok(Self {
            damage: ExternalDamage::new(width, height)?,
            pending: false,
        })
    }

    /// Merges one capture observation into the pending encoded frame.
    ///
    /// If no observation is pending, this starts a new frame by resetting the
    /// accumulator first. If a previous capture has not yet been submitted, the
    /// new damage is unioned into it.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] when `damage` describes another
    /// frame size.
    pub fn observe(&mut self, damage: DamageMap<'_>) -> Result<(), KeelError> {
        if !self.pending {
            self.damage.reset();
        }
        self.damage.merge_from(damage)?;
        self.pending = true;
        Ok(())
    }

    pub fn mark_submitted(&mut self) {
        self.pending = false;
    }

    pub fn discard_pending(&mut self) {
        self.pending = false;
        self.damage.reset();
    }

    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending
    }

    #[must_use]
    pub fn damage_map(&self) -> DamageMap<'_> {
        self.damage.damage_map()
    }
}

#[must_use]
pub const fn coalesced_rect_metadata_status(
    first_observation: bool,
    source_frame_count: u32,
    named_regions: usize,
) -> DamageMetadataStatus {
    if first_observation || (source_frame_count > 1 && named_regions == 0) {
        DamageMetadataStatus::Unknown
    } else {
        DamageMetadataStatus::Complete
    }
}

#[must_use]
pub const fn report_only_dirty_regions_status(
    region_count: usize,
    entry_read_failed: bool,
) -> DamageMetadataStatus {
    if region_count == 0 || entry_read_failed {
        DamageMetadataStatus::Unknown
    } else {
        DamageMetadataStatus::Complete
    }
}

fn clamp_signed_to_extent(value: i64, extent: usize) -> usize {
    if value <= 0 {
        return 0;
    }
    usize::try_from(value).map_or(extent, |value| value.min(extent))
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

    #[test]
    fn signed_bounds_clip_before_marking_blocks() {
        let mut damage = ExternalDamage::new(33, 17).unwrap();
        damage.mark_rect_bounds(-8, -4, 8, 8);
        assert_eq!(damage.damage_map().dirty_blocks().collect::<Vec<_>>(), [0]);

        damage.reset();
        damage.mark_rect_bounds(-8, -4, 20, 8);
        assert_eq!(
            damage.damage_map().dirty_blocks().collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn move_rect_marks_destination_and_source() {
        let mut damage = ExternalDamage::new(64, 16).unwrap();
        damage.mark_move_rect(0, 0, 32, 0, 48, 16);
        assert_eq!(
            damage.damage_map().dirty_blocks().collect::<Vec<_>>(),
            [0, 2]
        );
    }

    #[test]
    fn merge_unions_same_geometry_damage_maps() {
        let mut first = ExternalDamage::new(64, 16).unwrap();
        let mut second = ExternalDamage::new(64, 16).unwrap();
        first.mark_rect(PixelRect {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        });
        second.mark_rect(PixelRect {
            x: 48,
            y: 0,
            width: 16,
            height: 16,
        });

        first.merge_from(second.damage_map()).unwrap();
        assert_eq!(
            first.damage_map().dirty_blocks().collect::<Vec<_>>(),
            [0, 3]
        );
    }

    #[test]
    fn pending_external_damage_unions_acquisitions_until_submission() {
        let mut first = ExternalDamage::new(64, 16).unwrap();
        let mut second = ExternalDamage::new(64, 16).unwrap();
        first.mark_rect(PixelRect {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        });
        second.mark_rect(PixelRect {
            x: 48,
            y: 0,
            width: 16,
            height: 16,
        });

        let mut pending = PendingExternalDamage::new(64, 16).unwrap();
        pending.observe(first.damage_map()).unwrap();
        pending.observe(second.damage_map()).unwrap();
        assert_eq!(
            pending.damage_map().dirty_blocks().collect::<Vec<_>>(),
            [0, 3],
            "two acquisitions before one encode must be unioned"
        );

        pending.mark_submitted();
        pending.observe(second.damage_map()).unwrap();
        assert_eq!(
            pending.damage_map().dirty_blocks().collect::<Vec<_>>(),
            [3],
            "the next observation after submission starts a fresh map"
        );
    }

    #[test]
    fn coalesced_rect_metadata_marks_first_and_ambiguous_empty_batches_unknown() {
        assert_eq!(
            coalesced_rect_metadata_status(true, 1, 2),
            DamageMetadataStatus::Unknown,
            "the first OS-damage frame has no previous submitted picture"
        );
        assert_eq!(
            coalesced_rect_metadata_status(false, 2, 0),
            DamageMetadataStatus::Unknown,
            "coalesced frames with no named rects are ambiguous"
        );
        assert_eq!(
            coalesced_rect_metadata_status(false, 1, 0),
            DamageMetadataStatus::Complete,
            "a known single-frame duplicate may carry no damage"
        );
        assert_eq!(
            coalesced_rect_metadata_status(false, 3, 1),
            DamageMetadataStatus::Complete,
            "named rects make a coalesced batch actionable"
        );
    }

    #[test]
    fn report_only_dirty_regions_mark_empty_or_failed_reads_unknown() {
        assert_eq!(
            report_only_dirty_regions_status(0, false),
            DamageMetadataStatus::Unknown,
            "WGC report-only empty lists are not positive clean evidence"
        );
        assert_eq!(
            report_only_dirty_regions_status(2, true),
            DamageMetadataStatus::Unknown,
            "a failed region read invalidates the list"
        );
        assert_eq!(
            report_only_dirty_regions_status(2, false),
            DamageMetadataStatus::Complete
        );
    }
}
