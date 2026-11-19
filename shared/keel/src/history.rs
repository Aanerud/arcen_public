// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Per-block history: when each block last changed, how often it changed
//! recently, and how its current content was last painted.

use core::time::Duration;

use crate::damage::WORD_BITS;
use crate::{BlockGrid, DamageMap, KeelError};

/// Observations covered by [`BlockHistory::recent_changes`].
pub const HISTORY_WINDOW: u8 = 16;

const NEVER: u64 = u64::MAX;
const LOSSY_BIT: u8 = 0x80;
const KIND_MASK: u8 = 0x07;

/// How a block's current content reached the receiver.
///
/// These are kinds of paint, not codecs or wire records: the planner that
/// sends the content maps its own commands onto them.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u8)]
pub enum PaintKind {
    /// The current content has not been painted since it last changed.
    #[default]
    Unpainted = 0,
    /// Painted as one colour.
    Solid = 1,
    /// Painted bit-exactly from source pixels.
    Exact = 2,
    /// Copied from elsewhere on the receiver.
    Copied = 3,
    /// Painted from a receiver-side cache entry.
    Cached = 4,
    /// Painted by a lossy encoder.
    Lossy = 5,
}

impl PaintKind {
    const fn from_bits(bits: u8) -> Self {
        match bits & KIND_MASK {
            1 => Self::Solid,
            2 => Self::Exact,
            3 => Self::Copied,
            4 => Self::Cached,
            5 => Self::Lossy,
            _ => Self::Unpainted,
        }
    }
}

/// A block's last paint and whether what the receiver shows only
/// approximates the source.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BlockPaint {
    kind: PaintKind,
    lossy: bool,
}

impl BlockPaint {
    pub const UNPAINTED: Self = Self {
        kind: PaintKind::Unpainted,
        lossy: false,
    };

    /// A paint of `kind`. `inherited_lossy` matters only for
    /// [`PaintKind::Copied`] and [`PaintKind::Cached`], whose content is
    /// exactly as lossy as the source they came from; `Lossy` is always
    /// lossy, and `Unpainted`, `Solid` and `Exact` never are.
    #[must_use]
    pub const fn new(kind: PaintKind, inherited_lossy: bool) -> Self {
        let lossy = match kind {
            PaintKind::Lossy => true,
            PaintKind::Copied | PaintKind::Cached => inherited_lossy,
            PaintKind::Unpainted | PaintKind::Solid | PaintKind::Exact => false,
        };
        Self { kind, lossy }
    }

    #[must_use]
    pub const fn kind(self) -> PaintKind {
        self.kind
    }

    #[must_use]
    pub const fn is_lossy(self) -> bool {
        self.lossy
    }

    const fn to_bits(self) -> u8 {
        self.kind as u8 | if self.lossy { LOSSY_BIT } else { 0 }
    }

    const fn from_bits(bits: u8) -> Self {
        Self {
            kind: PaintKind::from_bits(bits),
            lossy: bits & LOSSY_BIT != 0,
        }
    }
}

/// A snapshot of one block's history at the latest observation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BlockRecord {
    /// Time since the block last changed; `None` if it never changed.
    pub age: Option<Duration>,
    /// Observations since the block last changed (0 = changed in the latest).
    pub ticks_since_change: Option<u64>,
    /// Changes within the last [`HISTORY_WINDOW`] observations.
    pub recent_changes: u8,
    pub paint: BlockPaint,
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    changed_at_us: u64,
    changed_tick: u64,
    /// Bit `n` set: the block changed `n` observations before `changed_tick`.
    recent: u16,
    paint: u8,
}

impl Entry {
    const EMPTY: Self = Self {
        changed_at_us: NEVER,
        changed_tick: 0,
        recent: 0,
        paint: 0,
    };
}

/// Change history of every block in one frame geometry.
///
/// One clock for everything that asks "how long has this block been still"
/// or "does it change every frame": refinement, block classes and
/// oldest-first fairness. Updates cost O(changed blocks); reads are O(1).
/// No storage is allocated after construction.
#[derive(Debug)]
pub struct BlockHistory {
    grid: BlockGrid,
    tick: u64,
    now_us: u64,
    entries: Vec<Entry>,
}

impl BlockHistory {
    #[must_use]
    pub fn new(grid: BlockGrid) -> Self {
        Self {
            grid,
            tick: 0,
            now_us: 0,
            entries: vec![Entry::EMPTY; grid.block_count()],
        }
    }

    #[must_use]
    pub const fn grid(&self) -> BlockGrid {
        self.grid
    }

    /// Observations so far.
    #[must_use]
    pub const fn tick(&self) -> u64 {
        self.tick
    }

    /// Time of the latest observation.
    #[must_use]
    pub const fn now(&self) -> Duration {
        Duration::from_micros(self.now_us)
    }

    /// Forgets all history and paint.
    pub fn reset(&mut self) {
        self.tick = 0;
        self.now_us = 0;
        self.entries.fill(Entry::EMPTY);
    }

    /// Marks every block unpainted while keeping its change history: the
    /// receiver lost its picture (a reset or a new generation).
    pub fn invalidate_paint(&mut self) {
        for entry in &mut self.entries {
            entry.paint = 0;
        }
    }

    /// Records one observation at `now`. Every block in `damage` changed:
    /// its clock restarts and its paint becomes [`PaintKind::Unpainted`].
    /// Time never runs backwards; an earlier `now` is treated as the latest.
    /// Every captured frame should be observed, clean ones included, so the
    /// change rate counts observations rather than changes.
    ///
    /// # Errors
    ///
    /// Returns [`KeelError::GeometryChanged`] without mutating state when the
    /// map has another geometry.
    pub fn observe(&mut self, damage: DamageMap<'_>, now: Duration) -> Result<(), KeelError> {
        if damage.grid() != self.grid {
            return Err(KeelError::GeometryChanged {
                expected: self.grid,
                actual: damage.grid(),
            });
        }
        let now_us = u64::try_from(now.as_micros()).unwrap_or(NEVER - 1);
        self.now_us = self.now_us.max(now_us.min(NEVER - 1));
        self.tick = self.tick.saturating_add(1);
        for (word_index, &word) in damage.bits.iter().enumerate() {
            let mut remaining = word;
            while remaining != 0 {
                let index = word_index * WORD_BITS + remaining.trailing_zeros() as usize;
                remaining &= remaining - 1;
                let Some(entry) = self.entries.get_mut(index) else {
                    break;
                };
                entry.recent = shifted(entry.recent, self.tick - entry.changed_tick) | 1;
                entry.changed_tick = self.tick;
                entry.changed_at_us = self.now_us;
                entry.paint = 0;
            }
        }
        Ok(())
    }

    /// Records how a block's current content was painted. Returns `false`
    /// for an index outside the grid.
    pub fn record_paint(&mut self, index: usize, paint: BlockPaint) -> bool {
        self.entries.get_mut(index).is_some_and(|entry| {
            entry.paint = paint.to_bits();
            true
        })
    }

    /// Records the same paint for every block in `blocks`.
    pub fn record_paint_blocks(
        &mut self,
        blocks: impl IntoIterator<Item = usize>,
        paint: BlockPaint,
    ) {
        let bits = paint.to_bits();
        for index in blocks {
            if let Some(entry) = self.entries.get_mut(index) {
                entry.paint = bits;
            }
        }
    }

    #[must_use]
    pub fn paint(&self, index: usize) -> Option<BlockPaint> {
        self.entries
            .get(index)
            .map(|entry| BlockPaint::from_bits(entry.paint))
    }

    /// Whether the receiver shows an approximation of this block.
    #[must_use]
    pub fn is_lossy(&self, index: usize) -> bool {
        self.paint(index).is_some_and(BlockPaint::is_lossy)
    }

    /// Time since the block last changed; `None` if it never changed or the
    /// index is outside the grid.
    #[must_use]
    pub fn age(&self, index: usize) -> Option<Duration> {
        let entry = self.entries.get(index)?;
        (entry.changed_at_us != NEVER)
            .then(|| Duration::from_micros(self.now_us.saturating_sub(entry.changed_at_us)))
    }

    /// Observations since the block last changed (0 when it changed in the
    /// latest one).
    #[must_use]
    pub fn ticks_since_change(&self, index: usize) -> Option<u64> {
        let entry = self.entries.get(index)?;
        (entry.changed_at_us != NEVER).then(|| self.tick - entry.changed_tick)
    }

    /// Whether the block changed in the latest observation.
    #[must_use]
    pub fn changed_now(&self, index: usize) -> bool {
        self.ticks_since_change(index) == Some(0)
    }

    /// Changes within the last `window` observations, the latest included.
    /// `window` is capped at [`HISTORY_WINDOW`].
    #[must_use]
    pub fn recent_changes(&self, index: usize, window: u8) -> u8 {
        let Some(entry) = self.entries.get(index) else {
            return 0;
        };
        let window = window.min(HISTORY_WINDOW);
        if window == 0 || entry.changed_at_us == NEVER {
            return 0;
        }
        let aligned = shifted(entry.recent, self.tick - entry.changed_tick);
        let mask = if window == HISTORY_WINDOW {
            u16::MAX
        } else {
            (1u16 << window) - 1
        };
        // At most 16 set bits.
        u8::try_from((aligned & mask).count_ones()).unwrap_or(HISTORY_WINDOW)
    }

    #[must_use]
    pub fn record(&self, index: usize) -> Option<BlockRecord> {
        Some(BlockRecord {
            age: self.age(index),
            ticks_since_change: self.ticks_since_change(index),
            recent_changes: self.recent_changes(index, HISTORY_WINDOW),
            paint: self.paint(index)?,
        })
    }
}

/// Ages a change mask by `ticks` observations.
const fn shifted(recent: u16, ticks: u64) -> u16 {
    if ticks >= HISTORY_WINDOW as u64 {
        0
    } else {
        recent << ticks
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::ExternalDamage;
    use crate::PixelRect;

    fn damage_at(blocks: &[usize]) -> ExternalDamage {
        let mut damage = ExternalDamage::new(64, 32).unwrap();
        for &index in blocks {
            damage.mark_rect(PixelRect {
                x: (index % 4) * 16,
                y: (index / 4) * 16,
                width: 16,
                height: 16,
            });
        }
        damage
    }

    #[test]
    fn age_and_rate_follow_observations() {
        let grid = BlockGrid::new(64, 32).unwrap();
        let mut history = BlockHistory::new(grid);
        assert_eq!(history.age(0), None);
        assert_eq!(history.recent_changes(0, HISTORY_WINDOW), 0);

        let changed = damage_at(&[0]);
        let clean = damage_at(&[]);
        for tick in 0..4u64 {
            history
                .observe(changed.damage_map(), Duration::from_millis(tick * 16))
                .unwrap();
        }
        assert_eq!(history.recent_changes(0, 4), 4);
        assert!(history.changed_now(0));
        history
            .observe(clean.damage_map(), Duration::from_millis(100))
            .unwrap();
        assert_eq!(history.age(0), Some(Duration::from_millis(52)));
        assert_eq!(history.ticks_since_change(0), Some(1));
        assert_eq!(history.recent_changes(0, 4), 3);
        assert_eq!(history.recent_changes(0, 1), 0);
        for tick in 0..HISTORY_WINDOW {
            history
                .observe(
                    clean.damage_map(),
                    Duration::from_millis(200 + u64::from(tick)),
                )
                .unwrap();
        }
        assert_eq!(history.recent_changes(0, HISTORY_WINDOW), 0);
        assert_eq!(history.age(1), None, "untouched blocks never changed");
    }

    #[test]
    fn a_change_clears_paint_and_paint_keeps_lossiness() {
        let grid = BlockGrid::new(64, 32).unwrap();
        let mut history = BlockHistory::new(grid);
        assert!(history.record_paint(2, BlockPaint::new(PaintKind::Lossy, false)));
        assert!(history.is_lossy(2));
        history.record_paint(3, BlockPaint::new(PaintKind::Copied, true));
        assert!(history.is_lossy(3), "a copy of lossy content is lossy");
        history.record_paint(4, BlockPaint::new(PaintKind::Exact, true));
        assert!(!history.is_lossy(4), "exact paint is never lossy");

        history
            .observe(damage_at(&[2]).damage_map(), Duration::ZERO)
            .unwrap();
        assert_eq!(history.paint(2), Some(BlockPaint::UNPAINTED));
        assert_eq!(history.paint(3).unwrap().kind(), PaintKind::Copied);
        history.invalidate_paint();
        assert_eq!(history.paint(3), Some(BlockPaint::UNPAINTED));
        assert!(!history.record_paint(99, BlockPaint::UNPAINTED));
    }

    #[test]
    fn time_never_runs_backwards_and_geometry_is_checked() {
        let grid = BlockGrid::new(64, 32).unwrap();
        let mut history = BlockHistory::new(grid);
        let changed = damage_at(&[0]);
        history
            .observe(changed.damage_map(), Duration::from_secs(5))
            .unwrap();
        history
            .observe(damage_at(&[]).damage_map(), Duration::from_secs(1))
            .unwrap();
        assert_eq!(history.now(), Duration::from_secs(5));
        assert_eq!(history.age(0), Some(Duration::ZERO));
        let other = ExternalDamage::new(32, 32).unwrap();
        assert!(matches!(
            history.observe(other.damage_map(), Duration::ZERO),
            Err(KeelError::GeometryChanged { .. })
        ));
        assert_eq!(history.tick(), 2);
    }
}
