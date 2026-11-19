// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Vertical scroll and move detection from retained row-segment hashes.
//!
//! Captures without OS move reports (`XShm`, `PipeWire` SHM, `ScreenCaptureKit`)
//! still scroll. A scroll shifts every row of a band by the same offset, so
//! each changed row's hash reappears in the previous frame `dy` rows away.
//! The estimator votes on `dy` from a sample of distinctive rows, then checks
//! the winner against every row of the band. The output is a list of
//! [`MoveRect`] hints like an OS would report; a consumer verifies each one
//! exactly against what the receiver shows before it copies.
//!
//! Only vertical offsets are found from pixels: row-segment hashes sit on the
//! 16-pixel block columns, so a horizontal shift changes every segment.
//! Horizontal and diagonal moves come from OS move reports.

use crate::damage::{RowPlanes, WORD_BITS, any_bit_in_range};
use crate::{BLOCK_SIZE, BlockGrid, DamageTracker, KeelError, MoveRect, PixelRect};

/// Largest vertical offset searched by default, in pixels per frame.
pub const DEFAULT_MAX_SCROLL: usize = 512;
/// Most moves reported per frame.
pub const MAX_ESTIMATED_MOVES: usize = 8;

const SAMPLE_COLUMNS: usize = 8;
const ANCHORS_PER_COLUMN: usize = 24;
const MIN_ANCHOR_VOTES: u32 = 3;
const MIN_MOVE_ROWS: usize = BLOCK_SIZE;
/// A block column joins a move when at least this share (out of 16) of the
/// band rows a true scroll would match do match.
const COLUMN_MATCH_SIXTEENTHS: usize = 12;

/// Tunables for [`MotionEstimator`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MotionConfig {
    /// Largest `|dy|` searched, in pixels.
    pub max_scroll: usize,
    /// Smallest band, in block rows, worth searching.
    pub min_band_block_rows: usize,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            max_scroll: DEFAULT_MAX_SCROLL,
            min_band_block_rows: 2,
        }
    }
}

/// Finds vertical scrolls and moves in the latest [`DamageTracker`] update.
///
/// Requires [`DamageTracker::retain_row_hashes`]. Cost is proportional to the
/// changed rows: a vote over a bounded sample of rows, then one comparison
/// per row and block column of each band. No allocation after construction.
#[derive(Debug)]
pub struct MotionEstimator {
    config: MotionConfig,
    grid: BlockGrid,
    votes: Vec<u32>,
    columns_ok: Vec<bool>,
    rows_ok: Vec<bool>,
    moves: Vec<MoveRect>,
    previous_offsets: [i32; MAX_ESTIMATED_MOVES],
    previous_count: usize,
}

impl MotionEstimator {
    #[must_use]
    pub fn new(grid: BlockGrid, config: MotionConfig) -> Self {
        let max_scroll = config.max_scroll.min(grid.height());
        Self {
            config: MotionConfig {
                max_scroll,
                ..config
            },
            grid,
            votes: vec![0; 2 * max_scroll + 1],
            columns_ok: vec![false; grid.blocks_wide()],
            rows_ok: vec![false; grid.height()],
            moves: Vec::with_capacity(MAX_ESTIMATED_MOVES),
            previous_offsets: [0; MAX_ESTIMATED_MOVES],
            previous_count: 0,
        }
    }

    #[must_use]
    pub const fn config(&self) -> MotionConfig {
        self.config
    }

    /// Moves found by the latest [`Self::estimate`].
    #[must_use]
    pub fn moves(&self) -> &[MoveRect] {
        &self.moves
    }

    /// Forgets the previous frame's offsets.
    pub fn reset(&mut self) {
        self.moves.clear();
        self.previous_count = 0;
    }

    /// Estimates moves for the tracker's latest update. Empty after a
    /// baseline update or when nothing changed.
    ///
    /// # Errors
    ///
    /// [`KeelError::GeometryChanged`] for a tracker of another geometry;
    /// [`KeelError::RowHashesNotRetained`] when the tracker does not retain
    /// row hashes.
    pub fn estimate(&mut self, tracker: &DamageTracker) -> Result<&[MoveRect], KeelError> {
        if tracker.grid() != self.grid {
            return Err(KeelError::GeometryChanged {
                expected: self.grid,
                actual: tracker.grid(),
            });
        }
        let rows = tracker
            .row_planes()
            .ok_or(KeelError::RowHashesNotRetained)?;
        self.moves.clear();
        if tracker.last_was_baseline() {
            self.previous_count = 0;
            return Ok(&self.moves);
        }
        let bits = tracker.damage_map().bits;
        let mut next_offsets = [0i32; MAX_ESTIMATED_MOVES];
        let mut next_count = 0usize;
        let mut block_row = 0usize;
        while block_row < self.grid.blocks_tall() && self.moves.len() < MAX_ESTIMATED_MOVES {
            if !self.row_dirty(bits, block_row) {
                block_row += 1;
                continue;
            }
            let first = block_row;
            while block_row < self.grid.blocks_tall() && self.row_dirty(bits, block_row) {
                block_row += 1;
            }
            let band = Band::new(self.grid, bits, first, block_row);
            if band.block_rows() < self.config.min_band_block_rows {
                continue;
            }
            if let Some(found) = self.search_band(rows, band) {
                if next_count < MAX_ESTIMATED_MOVES {
                    let (_, dy) = found.offset();
                    next_offsets[next_count] = i32::try_from(-dy).unwrap_or(0);
                    next_count += 1;
                }
                self.moves.push(found);
            }
        }
        self.previous_offsets = next_offsets;
        self.previous_count = next_count;
        Ok(&self.moves)
    }

    fn row_dirty(&self, bits: &[u64], block_row: usize) -> bool {
        let start = block_row * self.grid.blocks_wide();
        any_bit_in_range(bits, start, start + self.grid.blocks_wide())
    }

    /// The best verified move in one band, if any.
    fn search_band(&mut self, rows: &RowPlanes, band: Band) -> Option<MoveRect> {
        let mut best: Option<(MoveRect, usize)> = None;
        let voted = self.vote(rows, band);
        let previous = self.previous_offsets;
        for dy in voted
            .into_iter()
            .flatten()
            .chain(previous[..self.previous_count].iter().copied())
        {
            if dy == 0 || best.is_some_and(|(found, _)| found.offset().1 == -i64::from(dy)) {
                continue;
            }
            if let Some(candidate) = self.verify(rows, band, dy) {
                let area = candidate.destination.width * candidate.destination.height;
                if best.is_none_or(|(_, best_area)| area > best_area) {
                    best = Some((candidate, area));
                }
            }
        }
        best.map(|(found, _)| found)
    }

    /// Votes for `dy` (current row `y` shows previous row `y + dy`) using
    /// distinctive row pairs from a sample of block columns. Returns the two
    /// strongest offsets.
    fn vote(&mut self, rows: &RowPlanes, band: Band) -> [Option<i32>; 2] {
        self.votes.fill(0);
        let max = self.config.max_scroll;
        if max == 0 {
            return [None, None];
        }
        let span = band.last_column - band.first_column + 1;
        let step = span.div_ceil(SAMPLE_COLUMNS).max(1);
        let (top, bottom) = (band.top_pixel(), band.bottom_pixel(self.grid));
        let height = self.grid.height();
        let mut column = band.first_column;
        while column <= band.last_column {
            let mut anchors = 0usize;
            let row_step = ((bottom - top) / ANCHORS_PER_COLUMN).max(1);
            let mut y = top + 1;
            while y < bottom && anchors < ANCHORS_PER_COLUMN {
                let here = row(&rows.current, self.grid, column, y);
                let above = row(&rows.current, self.grid, column, y - 1);
                if here != above && here != row(&rows.previous, self.grid, column, y) {
                    anchors += 1;
                    let low = y.saturating_sub(max).max(1);
                    let high = (y + max).min(height - 1);
                    for source in low..=high {
                        if source != y
                            && row(&rows.previous, self.grid, column, source) == here
                            && row(&rows.previous, self.grid, column, source - 1) == above
                        {
                            self.votes[source + max - y] += 1;
                        }
                    }
                    y += row_step;
                } else {
                    y += 1;
                }
            }
            column += step;
        }
        let mut first: Option<(usize, u32)> = None;
        let mut second: Option<(usize, u32)> = None;
        for (slot, &count) in self.votes.iter().enumerate() {
            if count < MIN_ANCHOR_VOTES {
                continue;
            }
            if first.is_none_or(|(_, best)| count > best) {
                second = first;
                first = Some((slot, count));
            } else if second.is_none_or(|(_, best)| count > best) {
                second = Some((slot, count));
            }
        }
        let to_dy = |slot: usize| Some(i32::try_from(slot).ok()? - i32::try_from(max).ok()?);
        [
            first.and_then(|(slot, _)| to_dy(slot)),
            second.and_then(|(slot, _)| to_dy(slot)),
        ]
    }

    /// Checks `dy` on every row of every block column in the band and
    /// returns the largest rectangle that matches throughout.
    fn verify(&mut self, rows: &RowPlanes, band: Band, dy: i32) -> Option<MoveRect> {
        let (top, bottom) = (band.top_pixel(), band.bottom_pixel(self.grid));
        let height = self.grid.height();
        let shift = isize::try_from(dy).ok()?;
        let source_of = |y: usize| {
            y.checked_add_signed(shift)
                .filter(|source| *source < height)
        };
        // A true scroll matches every band row except the revealed strip.
        let expected = (bottom - top).saturating_sub(dy.unsigned_abs() as usize);
        let needed = (expected * COLUMN_MATCH_SIXTEENTHS / 16).max(MIN_MOVE_ROWS);
        for column in band.first_column..=band.last_column {
            let matched = (top..bottom)
                .filter(|&y| {
                    source_of(y).is_some_and(|source| {
                        row(&rows.current, self.grid, column, y)
                            == row(&rows.previous, self.grid, column, source)
                    })
                })
                .count();
            self.columns_ok[column] = matched >= needed;
        }
        let (first_column, last_column) =
            longest_run(&self.columns_ok[band.first_column..=band.last_column])
                .map(|(start, end)| (band.first_column + start, band.first_column + end))?;
        for y in top..bottom {
            self.rows_ok[y] = source_of(y).is_some_and(|source| {
                (first_column..=last_column).all(|column| {
                    row(&rows.current, self.grid, column, y)
                        == row(&rows.previous, self.grid, column, source)
                })
            });
        }
        let (first_row, last_row) =
            longest_run(&self.rows_ok[top..bottom]).map(|(start, end)| (top + start, top + end))?;
        let move_rows = last_row - first_row + 1;
        if move_rows < MIN_MOVE_ROWS {
            return None;
        }
        let x = first_column * BLOCK_SIZE;
        let right = ((last_column + 1) * BLOCK_SIZE).min(self.grid.width());
        let source_y = source_of(first_row)?;
        Some(MoveRect {
            source_x: x,
            source_y,
            destination: PixelRect {
                x,
                y: first_row,
                width: right - x,
                height: move_rows,
            },
        })
    }
}

/// A run of block rows that contain damage, and its damaged column span.
#[derive(Clone, Copy, Debug)]
struct Band {
    first_block_row: usize,
    end_block_row: usize,
    first_column: usize,
    last_column: usize,
}

impl Band {
    fn new(grid: BlockGrid, bits: &[u64], first_block_row: usize, end_block_row: usize) -> Self {
        let mut first_column = grid.blocks_wide();
        let mut last_column = 0;
        let start = first_block_row * grid.blocks_wide();
        let end = end_block_row * grid.blocks_wide();
        for index in start..end {
            if bits
                .get(index / WORD_BITS)
                .is_some_and(|word| word & (1u64 << (index % WORD_BITS)) != 0)
            {
                let column = index % grid.blocks_wide();
                first_column = first_column.min(column);
                last_column = last_column.max(column);
            }
        }
        Self {
            first_block_row,
            end_block_row,
            first_column: first_column.min(last_column),
            last_column,
        }
    }

    const fn block_rows(self) -> usize {
        self.end_block_row - self.first_block_row
    }

    const fn top_pixel(self) -> usize {
        self.first_block_row * BLOCK_SIZE
    }

    fn bottom_pixel(self, grid: BlockGrid) -> usize {
        (self.end_block_row * BLOCK_SIZE).min(grid.height())
    }
}

/// The hash of pixel row `y` in block column `column`.
fn row(plane: &[u64], grid: BlockGrid, column: usize, y: usize) -> u64 {
    let index = (y / BLOCK_SIZE) * grid.blocks_wide() + column;
    plane
        .get(index * BLOCK_SIZE + y % BLOCK_SIZE)
        .copied()
        .unwrap_or_default()
}

/// Inclusive bounds of the longest run of `true`, earliest on ties.
fn longest_run(flags: &[bool]) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    let mut start = None;
    for (index, &flag) in flags.iter().chain(core::iter::once(&false)).enumerate() {
        match (flag, start) {
            (true, None) => start = Some(index),
            (false, Some(begin)) => {
                if best.is_none_or(|(low, high)| index - begin > high - low + 1) {
                    best = Some((begin, index - 1));
                }
                start = None;
            }
            _ => {}
        }
    }
    best
}

/// Whether `moved` holds exactly: every pixel of its destination in
/// `current` has the colour of the pixel at its source in `reference`. This
/// is the check a consumer runs against what the receiver shows before it
/// copies. The fourth byte is not colour (an X server's capture leaves it
/// 0, a receiver's opaque canvas 255), so it is not compared.
#[must_use]
pub fn verify_move(
    reference: crate::BgraFrame<'_>,
    current: crate::BgraFrame<'_>,
    moved: MoveRect,
) -> bool {
    let source = moved.source();
    let destination = moved.destination;
    if reference.grid() != current.grid()
        || destination.is_empty()
        || source.right() > current.grid().width()
        || source.bottom() > current.grid().height()
        || destination.right() > current.grid().width()
        || destination.bottom() > current.grid().height()
    {
        return false;
    }
    (0..destination.height).all(|offset| {
        let (Some(from), Some(to)) = (
            reference.active_row(source.y + offset),
            current.active_row(destination.y + offset),
        ) else {
            return false;
        };
        same_colour(
            &from[source.x * 4..source.right() * 4],
            &to[destination.x * 4..destination.right() * 4],
        )
    })
}

/// Whether two equally long BGRA runs have the same colours.
fn same_colour(left: &[u8], right: &[u8]) -> bool {
    left == right
        || (left.len() == right.len()
            && left
                .chunks_exact(4)
                .zip(right.chunks_exact(4))
                .all(|(a, b)| a[..3] == b[..3]))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{BgraFrame, KernelPreference};

    fn page(width: usize, height: usize, scroll: usize) -> Vec<u8> {
        let mut pixels = vec![0u8; width * height * 4];
        for y in 0..height {
            let line = (y + scroll) as u64;
            for x in 0..width {
                let glyph = (line.wrapping_mul(0x9e37_79b9) ^ (x as u64 / 3)).count_ones() % 3;
                let value = [250u8, 30, 128][glyph as usize];
                pixels[(y * width + x) * 4..][..4].copy_from_slice(&[value, value, value, 0xff]);
            }
        }
        pixels
    }

    #[test]
    fn a_move_verifies_against_a_reference_with_another_fourth_byte() {
        let (width, height) = (32, 32);
        let opaque = page(width, height, 0);
        let mut captured = page(width, height, 4);
        for pixel in captured.chunks_exact_mut(4) {
            pixel[3] = 0;
        }
        let reference = BgraFrame::new(&opaque, width, height, width * 4).unwrap();
        let current = BgraFrame::new(&captured, width, height, width * 4).unwrap();
        let scroll = MoveRect {
            destination: PixelRect {
                x: 0,
                y: 0,
                width,
                height: height - 4,
            },
            source_x: 0,
            source_y: 4,
        };
        assert!(verify_move(reference, current, scroll));
        captured[5 * width * 4] ^= 1;
        let current = BgraFrame::new(&captured, width, height, width * 4).unwrap();
        assert!(!verify_move(reference, current, scroll));
    }

    #[test]
    fn finds_a_whole_frame_scroll_and_it_verifies() {
        let (width, height) = (96, 128);
        let first = page(width, height, 0);
        let second = page(width, height, 21);
        let mut tracker = DamageTracker::new(width, height, KernelPreference::Xxh3).unwrap();
        tracker.retain_row_hashes();
        let mut estimator = MotionEstimator::new(tracker.grid(), MotionConfig::default());
        tracker
            .update(BgraFrame::new(&first, width, height, width * 4).unwrap())
            .unwrap();
        assert!(estimator.estimate(&tracker).unwrap().is_empty(), "baseline");
        tracker
            .update(BgraFrame::new(&second, width, height, width * 4).unwrap())
            .unwrap();
        let moves = estimator.estimate(&tracker).unwrap().to_vec();
        let [found] = moves.as_slice() else {
            panic!("one move expected, got {moves:?}");
        };
        assert_eq!(found.offset(), (0, -21));
        assert_eq!(found.destination.height, height - 21);
        assert!(verify_move(
            BgraFrame::new(&first, width, height, width * 4).unwrap(),
            BgraFrame::new(&second, width, height, width * 4).unwrap(),
            *found,
        ));
    }

    #[test]
    fn requires_retained_rows() {
        let tracker = DamageTracker::new(32, 32, KernelPreference::Xxh3).unwrap();
        let mut estimator = MotionEstimator::new(tracker.grid(), MotionConfig::default());
        assert_eq!(
            estimator.estimate(&tracker).err(),
            Some(KeelError::RowHashesNotRetained)
        );
    }

    #[test]
    fn longest_run_prefers_the_longest_then_the_earliest() {
        assert_eq!(longest_run(&[true, false, true, true]), Some((2, 3)));
        assert_eq!(longest_run(&[true, true, false, true, true]), Some((0, 1)));
        assert_eq!(longest_run(&[false]), None);
    }

    #[test]
    fn verify_move_rejects_out_of_frame_and_mismatch() {
        let pixels = page(32, 32, 0);
        let frame = BgraFrame::new(&pixels, 32, 32, 128).unwrap();
        let inside = MoveRect {
            source_x: 0,
            source_y: 0,
            destination: PixelRect {
                x: 0,
                y: 0,
                width: 32,
                height: 32,
            },
        };
        assert!(verify_move(frame, frame, inside));
        let outside = MoveRect {
            source_y: 8,
            ..inside
        };
        assert!(!verify_move(frame, frame, outside));
    }
}
