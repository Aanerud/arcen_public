// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Pier's record of what the receiver's tile cache holds.
//!
//! Content keys are 128-bit XXH3 hashes of exact tile pixels; the Pier
//! assigns receiver slots, so on an ordered lane the receiver needs no
//! acknowledgement for the ledger to be right. Eviction is least recently
//! used within a byte budget and a slot count. Bookkeeping only: the wire
//! records that store, paint and evict belong to `arcen-protocol`.

use core::hash::{BuildHasherDefault, Hasher};
use std::collections::HashMap;

use xxhash_rust::xxh3::Xxh3;

use crate::{BgraFrame, PixelRect};

const NONE: u32 = u32::MAX;

/// 128-bit content key of an exact tile: its width, height and BGRA pixels
/// (alpha included).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TileKey(pub u128);

impl TileKey {
    /// Hashes `rect` of `frame`; `None` when the rect is empty or leaves the
    /// frame.
    #[must_use]
    pub fn of(frame: BgraFrame<'_>, rect: PixelRect) -> Option<Self> {
        let grid = frame.grid();
        if rect.is_empty() || rect.right() > grid.width() || rect.bottom() > grid.height() {
            return None;
        }
        let mut hasher = Xxh3::new();
        hasher.update(&(rect.width as u64).to_le_bytes());
        hasher.update(&(rect.height as u64).to_le_bytes());
        for y in rect.y..rect.bottom() {
            let row = frame.active_row(y)?;
            hasher.update(&row[rect.x * 4..rect.right() * 4]);
        }
        Some(Self(hasher.digest128()))
    }
}

/// A receiver cache slot, assigned by the Pier in `0..max_slots`.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SlotId(pub u32);

/// Why an insert was refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LedgerError {
    /// The tile alone exceeds the byte budget.
    TooLarge { bytes: u64, budget: u64 },
}

impl core::fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooLarge { bytes, budget } => write!(
                formatter,
                "a {bytes}-byte tile exceeds the {budget}-byte cache budget"
            ),
        }
    }
}

impl std::error::Error for LedgerError {}

/// What an insert did.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Inserted {
    /// The key was already held; it is now the most recently used.
    Present(SlotId),
    /// The key now occupies this slot; the caller sends it to the receiver.
    Stored(SlotId),
}

impl Inserted {
    #[must_use]
    pub const fn slot(self) -> SlotId {
        match self {
            Self::Present(slot) | Self::Stored(slot) => slot,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    key: u128,
    bytes: u64,
    newer: u32,
    older: u32,
    live: bool,
}

/// Keys are already uniform hashes; use their low bits directly.
#[derive(Default)]
struct KeyHasher(u64);

impl Hasher for KeyHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.0 = self.0.rotate_left(8) ^ u64::from(*byte);
        }
    }

    fn write_u128(&mut self, value: u128) {
        #[allow(clippy::cast_possible_truncation)]
        let low = value as u64;
        self.0 ^= low;
    }
}

/// Mirror of the receiver's tile cache: key → slot, byte budget, LRU.
/// Storage is sized at construction; no allocation afterwards.
pub struct TileLedger {
    budget_bytes: u64,
    used_bytes: u64,
    slots: Vec<Slot>,
    free: Vec<u32>,
    keys: HashMap<u128, u32, BuildHasherDefault<KeyHasher>>,
    newest: u32,
    oldest: u32,
}

impl core::fmt::Debug for TileLedger {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter
            .debug_struct("TileLedger")
            .field("budget_bytes", &self.budget_bytes)
            .field("used_bytes", &self.used_bytes)
            .field("entries", &self.keys.len())
            .field("max_slots", &self.slots.len())
            .finish_non_exhaustive()
    }
}

impl TileLedger {
    /// A ledger for a receiver cache of `budget_bytes` and `max_slots`
    /// entries (capped at `u32::MAX - 1`).
    #[must_use]
    pub fn new(budget_bytes: u64, max_slots: u32) -> Self {
        let max_slots = max_slots.min(NONE - 1);
        let count = max_slots as usize;
        // Twice the slots, so tombstones left by evictions are always
        // reclaimed by an in-place rehash rather than a reallocation.
        let keys = HashMap::with_capacity_and_hasher(count * 2, BuildHasherDefault::default());
        Self {
            budget_bytes,
            used_bytes: 0,
            slots: vec![
                Slot {
                    key: 0,
                    bytes: 0,
                    newer: NONE,
                    older: NONE,
                    live: false,
                };
                count
            ],
            free: (0..max_slots).rev().collect(),
            keys,
            newest: NONE,
            oldest: NONE,
        }
    }

    #[must_use]
    pub const fn budget_bytes(&self) -> u64 {
        self.budget_bytes
    }

    #[must_use]
    pub const fn used_bytes(&self) -> u64 {
        self.used_bytes
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// The slot holding `key`, made most recently used.
    pub fn lookup(&mut self, key: TileKey) -> Option<SlotId> {
        let slot = *self.keys.get(&key.0)?;
        self.touch(slot);
        Some(SlotId(slot))
    }

    /// Whether `key` is held, without changing recency.
    #[must_use]
    pub fn contains(&self, key: TileKey) -> bool {
        self.keys.contains_key(&key.0)
    }

    /// Records that the receiver will hold `key` (`bytes` of its budget),
    /// evicting least recently used entries until it fits. Evicted slots are
    /// appended to `evicted`; the caller tells the receiver before it reuses
    /// one.
    ///
    /// # Errors
    ///
    /// [`LedgerError::TooLarge`] when `bytes` exceeds the whole budget; the
    /// ledger is unchanged.
    pub fn insert(
        &mut self,
        key: TileKey,
        bytes: u64,
        evicted: &mut Vec<SlotId>,
    ) -> Result<Inserted, LedgerError> {
        if let Some(slot) = self.lookup(key) {
            return Ok(Inserted::Present(slot));
        }
        if bytes > self.budget_bytes || self.slots.is_empty() {
            return Err(LedgerError::TooLarge {
                bytes,
                budget: if self.slots.is_empty() {
                    0
                } else {
                    self.budget_bytes
                },
            });
        }
        while self.used_bytes + bytes > self.budget_bytes || self.free.is_empty() {
            let oldest = self.oldest;
            if oldest == NONE {
                break;
            }
            self.remove(oldest);
            evicted.push(SlotId(oldest));
        }
        let Some(slot) = self.free.pop() else {
            return Err(LedgerError::TooLarge {
                bytes,
                budget: self.budget_bytes,
            });
        };
        self.slots[slot as usize] = Slot {
            key: key.0,
            bytes,
            newer: NONE,
            older: NONE,
            live: true,
        };
        self.link_newest(slot);
        self.keys.insert(key.0, slot);
        self.used_bytes += bytes;
        Ok(Inserted::Stored(SlotId(slot)))
    }

    /// Forgets one slot (an explicit invalidation). Returns whether it was
    /// held.
    pub fn evict(&mut self, slot: SlotId) -> bool {
        let live = self
            .slots
            .get(slot.0 as usize)
            .is_some_and(|entry| entry.live);
        if live {
            self.remove(slot.0);
        }
        live
    }

    /// Forgets everything: the receiver dropped its cache.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            slot.live = false;
        }
        self.keys.clear();
        self.free.clear();
        self.free
            .extend((0..u32::try_from(self.slots.len()).unwrap_or(0)).rev());
        self.used_bytes = 0;
        self.newest = NONE;
        self.oldest = NONE;
    }

    fn touch(&mut self, slot: u32) {
        if self.newest != slot {
            self.unlink(slot);
            self.link_newest(slot);
        }
    }

    fn remove(&mut self, slot: u32) {
        self.unlink(slot);
        let entry = &mut self.slots[slot as usize];
        entry.live = false;
        self.used_bytes -= entry.bytes;
        self.keys.remove(&entry.key);
        self.free.push(slot);
    }

    fn unlink(&mut self, slot: u32) {
        let Slot { newer, older, .. } = self.slots[slot as usize];
        if newer == NONE {
            self.newest = older;
        } else {
            self.slots[newer as usize].older = older;
        }
        if older == NONE {
            self.oldest = newer;
        } else {
            self.slots[older as usize].newer = newer;
        }
        self.slots[slot as usize].newer = NONE;
        self.slots[slot as usize].older = NONE;
    }

    fn link_newest(&mut self, slot: u32) {
        self.slots[slot as usize].older = self.newest;
        self.slots[slot as usize].newer = NONE;
        if self.newest == NONE {
            self.oldest = slot;
        } else {
            self.slots[self.newest as usize].newer = slot;
        }
        self.newest = slot;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const fn key(value: u128) -> TileKey {
        TileKey(value)
    }

    #[test]
    fn lru_eviction_within_the_byte_budget() {
        let mut ledger = TileLedger::new(300, 16);
        let mut evicted = Vec::new();
        let a = ledger.insert(key(1), 100, &mut evicted).unwrap().slot();
        ledger.insert(key(2), 100, &mut evicted).unwrap();
        ledger.insert(key(3), 100, &mut evicted).unwrap();
        assert_eq!(ledger.lookup(key(1)), Some(a), "1 is now most recent");
        let stored = ledger.insert(key(4), 150, &mut evicted).unwrap();
        assert!(matches!(stored, Inserted::Stored(_)));
        assert!(!ledger.contains(key(2)) && !ledger.contains(key(3)));
        assert!(ledger.contains(key(1)));
        assert_eq!(evicted.len(), 2);
        assert_eq!(ledger.used_bytes(), 250);
        assert_eq!(
            ledger.insert(key(1), 100, &mut evicted).unwrap(),
            Inserted::Present(a)
        );
    }

    #[test]
    fn slot_count_bounds_entries_and_slots_are_reused() {
        let mut ledger = TileLedger::new(u64::MAX, 2);
        let mut evicted = Vec::new();
        let first = ledger.insert(key(1), 1, &mut evicted).unwrap().slot();
        ledger.insert(key(2), 1, &mut evicted).unwrap();
        let third = ledger.insert(key(3), 1, &mut evicted).unwrap().slot();
        assert_eq!(evicted, [first]);
        assert_eq!(third, first, "the evicted slot is reused");
        assert!(third.0 < 2);
    }

    #[test]
    fn too_large_and_explicit_eviction() {
        let mut ledger = TileLedger::new(100, 4);
        let mut evicted = Vec::new();
        assert!(matches!(
            ledger.insert(key(9), 101, &mut evicted),
            Err(LedgerError::TooLarge { .. })
        ));
        let slot = ledger.insert(key(1), 60, &mut evicted).unwrap().slot();
        assert!(ledger.evict(slot));
        assert!(!ledger.evict(slot));
        assert!(ledger.is_empty());
        assert_eq!(ledger.used_bytes(), 0);
        ledger.insert(key(2), 60, &mut evicted).unwrap();
        ledger.clear();
        assert!(ledger.lookup(key(2)).is_none());
        assert!(
            TileLedger::new(10, 0)
                .insert(key(1), 1, &mut evicted)
                .is_err()
        );
    }

    #[test]
    fn tile_keys_cover_shape_and_pixels() {
        let mut pixels = vec![0u8; 32 * 16 * 4];
        let frame = BgraFrame::new(&pixels, 32, 16, 128).unwrap();
        let left = PixelRect {
            x: 0,
            y: 0,
            width: 16,
            height: 16,
        };
        let right = PixelRect { x: 16, ..left };
        let tall = PixelRect {
            width: 8,
            height: 32,
            ..left
        };
        assert_eq!(TileKey::of(frame, left), TileKey::of(frame, right));
        assert!(TileKey::of(frame, tall).is_none(), "leaves the frame");
        let wide = PixelRect {
            width: 32,
            height: 8,
            ..left
        };
        let narrow = PixelRect {
            width: 16,
            height: 16,
            ..left
        };
        assert_ne!(
            TileKey::of(frame, wide),
            TileKey::of(frame, narrow),
            "same bytes, other shape"
        );
        pixels[16 * 4] = 1;
        let changed = BgraFrame::new(&pixels, 32, 16, 128).unwrap();
        assert_ne!(TileKey::of(changed, left), TileKey::of(changed, right));
    }
}
