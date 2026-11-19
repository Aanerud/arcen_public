// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Per-block content classes from history and moves, with hysteresis.
//!
//! The frame-level [`crate::ActivityClass`] summarises a whole monitor; a
//! planner that sends each region its own way needs the class of every
//! block. Names are codec-free: the planner maps classes to commands.

use core::time::Duration;

use crate::damage::WORD_BITS;
use crate::{
    BLOCK_SIZE, BlockGrid, BlockHistory, DamageMap, HISTORY_WINDOW, KeelError, MoveRect, PaintKind,
};

/// What a block is doing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum BlockClass {
    /// Unchanged, and the receiver shows it exactly.
    #[default]
    Static = 0,
    /// Unchanged for the settle time, but last painted lossily or not at
    /// all: a candidate to build up to lossless.
    Settled = 1,
    /// Changed recently, but not often enough to be motion: text, carets,
    /// menus.
    Sparse = 2,
    /// Changes on most observations: video, animation, a drag in progress.
    Motion = 3,
    /// Changed in the latest observation and lies wholly inside a move
    /// destination: the receiver can copy it.
    Moved = 4,
}

impl BlockClass {
    const fn from_bits(bits: u8) -> Self {
        match bits {
            1 => Self::Settled,
            2 => Self::Sparse,
            3 => Self::Motion,
            4 => Self::Moved,
            _ => Self::Static,
        }
    }
}

/// Thresholds for [`BlockClassMap`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClassConfig {
    /// A block becomes `Motion` after this many changes in the last 16
    /// observations. Five (about 19 changes a second at 60 Hz) is above fast
    /// typing under a blinking caret and below 24 fps video.
    pub motion_enter: u8,
    /// A `Motion` block stays `Motion` while it changed at least this many
    /// times in the last 16 observations.
    pub motion_stay: u8,
    /// Time a block must be still before it is `Settled` or `Static`.
    pub settle: Duration,
}

impl Default for ClassConfig {
    fn default() -> Self {
        Self {
            motion_enter: 5,
            motion_stay: 2,
            settle: Duration::from_millis(300),
        }
    }
}

/// Class counts and Motion boundary crossings of one update.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClassSummary {
    pub settled: usize,
    pub sparse: usize,
    pub motion: usize,
    pub moved: usize,
    /// Blocks that became `Motion` in this update.
    pub motion_entered: usize,
    /// Blocks that stopped being `Motion` in this update.
    pub motion_left: usize,
}

impl ClassSummary {
    /// Entries plus exits of `Motion`: the class flaps that churn surfaces.
    #[must_use]
    pub const fn motion_flaps(self) -> usize {
        self.motion_entered + self.motion_left
    }
}

/// The class of every block of one geometry.
///
/// Only blocks that are not `Static` are revisited, so an update costs
/// O(changed + recently active blocks). No allocation after construction.
#[derive(Debug)]
pub struct BlockClassMap {
    grid: BlockGrid,
    config: ClassConfig,
    classes: Vec<u8>,
    active: Vec<u32>,
    in_active: Vec<u64>,
    moved: Vec<u64>,
    summary: ClassSummary,
}

impl BlockClassMap {
    #[must_use]
    pub fn new(grid: BlockGrid, config: ClassConfig) -> Self {
        let words = grid.block_count().div_ceil(WORD_BITS);
        Self {
            grid,
            config,
            classes: vec![0; grid.block_count()],
            active: Vec::with_capacity(grid.block_count()),
            in_active: vec![0; words],
            moved: vec![0; words],
            summary: ClassSummary::default(),
        }
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.grid
    }

    #[must_use]
    pub const fn config(&self) -> ClassConfig {
        self.config
    }

    #[must_use]
    pub const fn summary(&self) -> ClassSummary {
        self.summary
    }

    /// Every block back to `Static`.
    pub fn reset(&mut self) {
        self.classes.fill(0);
        self.active.clear();
        self.in_active.fill(0);
        self.summary = ClassSummary::default();
    }

    #[must_use]
    pub fn class(&self, index: usize) -> Option<BlockClass> {
        self.classes
            .get(index)
            .map(|bits| BlockClass::from_bits(*bits))
    }

    /// Reclassifies `blocks` at the next update even though they did not
    /// change: call it after recording a paint without damage (a lossy
    /// surface painted over still blocks).
    pub fn revisit(&mut self, blocks: impl IntoIterator<Item = usize>) {
        for index in blocks {
            self.activate(index);
        }
    }

    /// Reclassifies every block at the next update; pair it with
    /// [`BlockHistory::invalidate_paint`] when the receiver lost its picture.
    pub fn revisit_all(&mut self) {
        for index in 0..self.grid.block_count() {
            self.activate(index);
        }
    }

    fn activate(&mut self, index: usize) {
        if index < self.grid.block_count() && !bit(&self.in_active, index) {
            set(&mut self.in_active, index, true);
            self.active.push(u32::try_from(index).unwrap_or(u32::MAX));
        }
    }

    /// Blocks that are not `Static`, in no particular order.
    pub fn active_blocks(&self) -> impl Iterator<Item = usize> + '_ {
        self.active.iter().map(|index| *index as usize)
    }

    /// Reclassifies after `history` observed `damage`. `moves` are the moves
    /// the consumer verified for this frame; unverified hints must not be
    /// passed, since `Moved` tells the planner it may copy.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without mutating state when the
    /// history or map has another geometry.
    pub fn update(
        &mut self,
        history: &BlockHistory,
        damage: DamageMap<'_>,
        moves: &[MoveRect],
    ) -> Result<ClassSummary, KeelError> {
        for actual in [history.grid(), damage.grid()] {
            if actual != self.grid {
                return Err(KeelError::GeometryChanged {
                    expected: self.grid,
                    actual,
                });
            }
        }
        self.mark_moved(moves);
        for (word_index, &word) in damage.bits.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let index = word_index * WORD_BITS + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                self.activate(index);
            }
        }

        let mut summary = ClassSummary::default();
        let mut slot = 0usize;
        while slot < self.active.len() {
            let index = self.active[slot] as usize;
            let previous = BlockClass::from_bits(self.classes[index]);
            let next = self.classify(history, index, previous);
            self.classes[index] = next as u8;
            match (previous == BlockClass::Motion, next == BlockClass::Motion) {
                (false, true) => summary.motion_entered += 1,
                (true, false) => summary.motion_left += 1,
                _ => {}
            }
            match next {
                BlockClass::Static => {
                    set(&mut self.in_active, index, false);
                    self.active.swap_remove(slot);
                    continue;
                }
                BlockClass::Settled => summary.settled += 1,
                BlockClass::Sparse => summary.sparse += 1,
                BlockClass::Motion => summary.motion += 1,
                BlockClass::Moved => summary.moved += 1,
            }
            slot += 1;
        }
        self.summary = summary;
        Ok(summary)
    }

    fn classify(&self, history: &BlockHistory, index: usize, previous: BlockClass) -> BlockClass {
        let config = self.config;
        if history.changed_now(index) && bit(&self.moved, index) {
            return BlockClass::Moved;
        }
        let changes = history.recent_changes(index, HISTORY_WINDOW);
        if changes >= config.motion_enter
            || (previous == BlockClass::Motion && changes >= config.motion_stay)
        {
            return BlockClass::Motion;
        }
        if history.age(index).is_some_and(|age| age < config.settle) {
            return BlockClass::Sparse;
        }
        let paint = history.paint(index).unwrap_or_default();
        if paint.is_lossy() || paint.kind() == PaintKind::Unpainted {
            BlockClass::Settled
        } else {
            BlockClass::Static
        }
    }

    /// Marks the blocks wholly covered by a move destination.
    fn mark_moved(&mut self, moves: &[MoveRect]) {
        self.moved.fill(0);
        for moved in moves {
            let rect = moved.destination;
            let right = rect.right().min(self.grid.width());
            let bottom = rect.bottom().min(self.grid.height());
            let first_x = rect.x.div_ceil(BLOCK_SIZE);
            let first_y = rect.y.div_ceil(BLOCK_SIZE);
            // A tail block is covered when the rect reaches the frame edge.
            let end_x = if right == self.grid.width() {
                self.grid.blocks_wide()
            } else {
                right / BLOCK_SIZE
            };
            let end_y = if bottom == self.grid.height() {
                self.grid.blocks_tall()
            } else {
                bottom / BLOCK_SIZE
            };
            for block_y in first_y..end_y {
                for block_x in first_x..end_x {
                    if let Some(index) = self.grid.block_index(block_x, block_y) {
                        set(&mut self.moved, index, true);
                    }
                }
            }
        }
    }
}

fn bit(words: &[u64], index: usize) -> bool {
    words
        .get(index / WORD_BITS)
        .is_some_and(|word| word & (1u64 << (index % WORD_BITS)) != 0)
}

fn set(words: &mut [u64], index: usize, value: bool) {
    if let Some(word) = words.get_mut(index / WORD_BITS) {
        let mask = 1u64 << (index % WORD_BITS);
        if value {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::{BlockPaint, ExternalDamage, PixelRect};

    struct Rig {
        history: BlockHistory,
        classes: BlockClassMap,
        changed: ExternalDamage,
        clean: ExternalDamage,
        now: Duration,
    }

    impl Rig {
        fn new() -> Self {
            let grid = BlockGrid::new(64, 32).unwrap();
            let mut changed = ExternalDamage::new(64, 32).unwrap();
            changed.mark_rect(PixelRect {
                x: 0,
                y: 0,
                width: 16,
                height: 16,
            });
            Self {
                history: BlockHistory::new(grid),
                classes: BlockClassMap::new(grid, ClassConfig::default()),
                changed,
                clean: ExternalDamage::new(64, 32).unwrap(),
                now: Duration::ZERO,
            }
        }

        fn step(&mut self, change: bool, moves: &[MoveRect]) -> ClassSummary {
            self.now += Duration::from_millis(16);
            let damage = if change { &self.changed } else { &self.clean };
            self.history.observe(damage.damage_map(), self.now).unwrap();
            self.classes
                .update(&self.history, damage.damage_map(), moves)
                .unwrap()
        }
    }

    #[test]
    fn one_change_is_sparse_then_settles() {
        let mut rig = Rig::new();
        rig.step(true, &[]);
        assert_eq!(rig.classes.class(0), Some(BlockClass::Sparse));
        for _ in 0..20 {
            rig.step(false, &[]);
        }
        assert_eq!(
            rig.classes.class(0),
            Some(BlockClass::Settled),
            "never painted"
        );
        rig.history
            .record_paint(0, BlockPaint::new(PaintKind::Exact, false));
        rig.step(false, &[]);
        assert_eq!(rig.classes.class(0), Some(BlockClass::Static));
        assert_eq!(rig.classes.active_blocks().count(), 0);
    }

    #[test]
    fn a_paint_without_damage_is_reclassified_when_revisited() {
        let mut rig = Rig::new();
        rig.step(true, &[]);
        rig.history
            .record_paint(0, BlockPaint::new(PaintKind::Exact, false));
        for _ in 0..20 {
            rig.step(false, &[]);
        }
        assert_eq!(rig.classes.class(0), Some(BlockClass::Static));
        rig.history
            .record_paint(0, BlockPaint::new(PaintKind::Lossy, false));
        rig.classes.revisit([0]);
        rig.step(false, &[]);
        assert_eq!(rig.classes.class(0), Some(BlockClass::Settled));
        rig.history.invalidate_paint();
        rig.classes.revisit_all();
        rig.step(false, &[]);
        assert_eq!(
            rig.classes.summary().settled,
            8,
            "the receiver lost its picture"
        );
    }

    #[test]
    fn motion_needs_repeated_change_and_lingers() {
        let mut rig = Rig::new();
        for _ in 0..4 {
            rig.step(true, &[]);
            rig.step(false, &[]);
        }
        assert_eq!(rig.classes.class(0), Some(BlockClass::Sparse), "4 in 16");
        let summary = rig.step(true, &[]);
        assert_eq!(rig.classes.class(0), Some(BlockClass::Motion));
        assert_eq!(summary.motion_entered, 1);
        for _ in 0..12 {
            rig.step(false, &[]);
        }
        assert_eq!(rig.classes.class(0), Some(BlockClass::Motion), "hysteresis");
        let mut left = 0;
        for _ in 0..8 {
            left += rig.step(false, &[]).motion_left;
        }
        assert_eq!(left, 1);
        assert_ne!(rig.classes.class(0), Some(BlockClass::Motion));
    }

    #[test]
    fn changed_blocks_inside_a_move_are_moved() {
        let mut rig = Rig::new();
        let moved = MoveRect {
            source_x: 0,
            source_y: 16,
            destination: PixelRect {
                x: 0,
                y: 0,
                width: 20,
                height: 16,
            },
        };
        rig.step(true, &[moved]);
        assert_eq!(rig.classes.class(0), Some(BlockClass::Moved));
        let partial = MoveRect {
            destination: PixelRect {
                x: 4,
                ..moved.destination
            },
            ..moved
        };
        rig.step(true, &[partial]);
        assert_eq!(
            rig.classes.class(0),
            Some(BlockClass::Sparse),
            "a block only partly covered must be painted"
        );
    }
}
