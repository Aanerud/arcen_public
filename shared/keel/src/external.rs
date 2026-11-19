// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use crate::{BLOCK_SIZE, BlockGrid, DamageMap, DamageSummary, KeelError};

const WORD_BITS: usize = u64::BITS as usize;

/// Most move vectors one [`ExternalDamage`] retains for a pending frame.
///
/// A desktop reports a handful of moves per frame (one scroll, one dragged
/// window). Past this bound the list is dropped as a whole and only the
/// conservative damage bits remain, because a partial ordered list could make
/// a later move copy content an omitted earlier move should have placed.
pub const MAX_MOVE_RECTS: usize = 32;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PixelRect {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}

impl PixelRect {
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.width == 0 || self.height == 0
    }

    /// Exclusive right edge, saturating.
    #[must_use]
    pub const fn right(self) -> usize {
        self.x.saturating_add(self.width)
    }

    /// Exclusive bottom edge, saturating.
    #[must_use]
    pub const fn bottom(self) -> usize {
        self.y.saturating_add(self.height)
    }
}

/// Pixels copied from `source` to `destination` within one frame.
///
/// A move is a hint, never a promise: the frame that produced it may have
/// been coalesced with others, and the source may have held content the
/// receiver never saw. A consumer must verify a move against what the
/// receiver shows (an exact comparison of the moved band) before it sends it
/// as a copy. The destination and source blocks are always also marked in
/// the damage bits, so a rejected move falls back to ordinary damage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MoveRect {
    pub source_x: usize,
    pub source_y: usize,
    pub destination: PixelRect,
}

impl MoveRect {
    #[must_use]
    pub const fn source(self) -> PixelRect {
        PixelRect {
            x: self.source_x,
            y: self.source_y,
            width: self.destination.width,
            height: self.destination.height,
        }
    }

    /// Displacement `(dx, dy)` from source to destination in pixels.
    #[must_use]
    #[allow(clippy::cast_possible_wrap)]
    pub const fn offset(self) -> (i64, i64) {
        (
            self.destination.x as i64 - self.source_x as i64,
            self.destination.y as i64 - self.source_y as i64,
        )
    }
}

#[derive(Debug)]
pub struct ExternalDamage {
    grid: BlockGrid,
    dirty_bits: Vec<u64>,
    moves: Vec<MoveRect>,
    moves_overflowed: bool,
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
            moves: Vec::with_capacity(MAX_MOVE_RECTS),
            moves_overflowed: false,
        })
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.grid
    }

    pub fn reset(&mut self) {
        self.dirty_bits.fill(0);
        self.moves.clear();
        self.moves_overflowed = false;
    }

    /// Move vectors in report order, clipped so source and destination both
    /// lie inside the frame. Empty when none were reported or when more than
    /// [`MAX_MOVE_RECTS`] arrived (see [`Self::moves_overflowed`]).
    #[must_use]
    pub fn move_rects(&self) -> &[MoveRect] {
        &self.moves
    }

    /// Whether moves were reported but dropped because the bound was
    /// exceeded. The damage bits still cover every source and destination.
    #[must_use]
    pub const fn moves_overflowed(&self) -> bool {
        self.moves_overflowed
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

    /// Records a move vector and conservatively marks its destination and
    /// source.
    ///
    /// Move-rect APIs often describe pixels copied from `source` to
    /// `destination`. The destination is definitely damaged; the source is
    /// marked as well because another OS damage rect may cover the reveal, and
    /// over-marking spends bits while under-marking starves changed content.
    /// The vector itself is kept (clipped to the frame) in
    /// [`Self::move_rects`], so a consumer that can copy may do so; the
    /// marked bits remain the fallback for one that cannot.
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
        if let Some(clipped) = clip_move(
            self.grid,
            source_x,
            source_y,
            destination_left,
            destination_top,
            width,
            height,
        ) {
            self.push_move(clipped);
        }
    }

    fn push_move(&mut self, rect: MoveRect) {
        if self.moves_overflowed {
            return;
        }
        if self.moves.len() == MAX_MOVE_RECTS {
            self.moves.clear();
            self.moves_overflowed = true;
            return;
        }
        self.moves.push(rect);
    }

    /// Merges another accumulator for the same frame geometry: damage bits
    /// are unioned and its move vectors are appended after this one's, in
    /// order, under the same [`MAX_MOVE_RECTS`] bound.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] when `other` was built for a
    /// different frame size.
    pub fn merge_external(&mut self, other: &Self) -> Result<(), KeelError> {
        self.merge_from(other.damage_map())?;
        if other.moves_overflowed {
            self.moves.clear();
            self.moves_overflowed = true;
            return Ok(());
        }
        for rect in &other.moves {
            self.push_move(*rect);
        }
        Ok(())
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

    /// Like [`Self::observe`], and also keeps the observation's move
    /// vectors, appended in capture order.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] when `damage` describes another
    /// frame size.
    pub fn observe_external(&mut self, damage: &ExternalDamage) -> Result<(), KeelError> {
        if damage.grid() != self.damage.grid() {
            return Err(KeelError::GeometryChanged {
                expected: self.damage.grid(),
                actual: damage.grid(),
            });
        }
        if !self.pending {
            self.damage.reset();
        }
        self.damage.merge_external(damage)?;
        self.pending = true;
        Ok(())
    }

    /// Move vectors of the pending frame, in capture order.
    #[must_use]
    pub fn move_rects(&self) -> &[MoveRect] {
        self.damage.move_rects()
    }

    /// Whether the pending frame's moves were dropped for exceeding the
    /// bound.
    #[must_use]
    pub const fn moves_overflowed(&self) -> bool {
        self.damage.moves_overflowed()
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

/// Clips a move so both its source and its destination lie in the frame,
/// trimming the same edges of each. `None` for an empty or zero-length move.
fn clip_move(
    grid: BlockGrid,
    source_x: i64,
    source_y: i64,
    destination_left: i64,
    destination_top: i64,
    width: i64,
    height: i64,
) -> Option<MoveRect> {
    let (dst_x, src_x, width) = clip_axis(destination_left, source_x, width, grid.width())?;
    let (dst_y, src_y, height) = clip_axis(destination_top, source_y, height, grid.height())?;
    if dst_x == src_x && dst_y == src_y {
        return None;
    }
    Some(MoveRect {
        source_x: src_x,
        source_y: src_y,
        destination: PixelRect {
            x: dst_x,
            y: dst_y,
            width,
            height,
        },
    })
}

/// One axis of [`clip_move`]: the span `[start, start + len)` at the
/// destination and its paired span at the source, intersected with
/// `[0, extent)` on both sides.
fn clip_axis(
    destination: i64,
    source: i64,
    length: i64,
    extent: usize,
) -> Option<(usize, usize, usize)> {
    let extent = i64::try_from(extent).ok()?;
    let lead = 0i64
        .saturating_sub(destination)
        .max(0i64.saturating_sub(source))
        .max(0);
    let trail = destination
        .saturating_add(length)
        .saturating_sub(extent)
        .max(source.saturating_add(length).saturating_sub(extent))
        .max(0);
    let clipped = length.saturating_sub(lead).saturating_sub(trail);
    if clipped <= 0 {
        return None;
    }
    Some((
        usize::try_from(destination.saturating_add(lead)).ok()?,
        usize::try_from(source.saturating_add(lead)).ok()?,
        usize::try_from(clipped).ok()?,
    ))
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
    fn move_rect_keeps_its_vector() {
        let mut damage = ExternalDamage::new(64, 32).unwrap();
        damage.mark_move_rect(0, 8, 0, 0, 64, 24);
        assert_eq!(
            damage.move_rects(),
            [MoveRect {
                source_x: 0,
                source_y: 8,
                destination: PixelRect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 24,
                },
            }]
        );
        assert_eq!(damage.move_rects()[0].offset(), (0, -8));
        damage.reset();
        assert!(damage.move_rects().is_empty());
    }

    #[test]
    fn move_rect_clips_source_and_destination_together() {
        let mut damage = ExternalDamage::new(64, 32).unwrap();
        // Destination hangs off the left and bottom; source off the top.
        damage.mark_move_rect(10, -4, -6, 4, 26, 40);
        let [moved] = damage.move_rects() else {
            panic!("one clipped move expected");
        };
        assert_eq!(
            moved.source(),
            PixelRect {
                x: 16,
                y: 0,
                width: 26,
                height: 24
            }
        );
        assert_eq!(
            moved.destination,
            PixelRect {
                x: 0,
                y: 8,
                width: 26,
                height: 24
            }
        );
        assert_eq!(moved.offset(), (-16, 8));
    }

    #[test]
    fn empty_or_zero_moves_keep_no_vector() {
        let mut damage = ExternalDamage::new(64, 32).unwrap();
        damage.mark_move_rect(16, 0, 16, 0, 32, 16);
        damage.mark_move_rect(0, 100, 0, 0, 16, 16);
        assert!(damage.move_rects().is_empty());
        assert!(!damage.moves_overflowed());
        assert_eq!(
            damage.damage_map().dirty_blocks().collect::<Vec<_>>(),
            [0, 1]
        );
    }

    #[test]
    fn move_overflow_drops_the_whole_list_but_keeps_bits() {
        let mut damage = ExternalDamage::new(64, 64).unwrap();
        for _ in 0..=MAX_MOVE_RECTS {
            damage.mark_move_rect(0, 0, 0, 16, 16, 32);
        }
        assert!(damage.moves_overflowed());
        assert!(damage.move_rects().is_empty());
        assert_eq!(
            damage.damage_map().dirty_blocks().collect::<Vec<_>>(),
            [0, 4]
        );
        damage.mark_move_rect(0, 0, 0, 16, 16, 32);
        assert!(damage.move_rects().is_empty(), "overflow holds until reset");
    }

    #[test]
    fn pending_damage_keeps_moves_in_capture_order() {
        let mut first = ExternalDamage::new(64, 64).unwrap();
        let mut second = ExternalDamage::new(64, 64).unwrap();
        first.mark_move_rect(0, 16, 0, 0, 64, 48);
        second.mark_move_rect(0, 32, 0, 0, 64, 32);
        let mut pending = PendingExternalDamage::new(64, 64).unwrap();
        pending.observe_external(&first).unwrap();
        pending.observe_external(&second).unwrap();
        let offsets = pending
            .move_rects()
            .iter()
            .map(|rect| rect.offset())
            .collect::<Vec<_>>();
        assert_eq!(offsets, [(0, -16), (0, -32)]);
        pending.mark_submitted();
        pending.observe(first.damage_map()).unwrap();
        assert!(pending.move_rects().is_empty(), "a new frame starts clean");
        assert!(matches!(
            pending.observe_external(&ExternalDamage::new(32, 32).unwrap()),
            Err(KeelError::GeometryChanged { .. })
        ));
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
