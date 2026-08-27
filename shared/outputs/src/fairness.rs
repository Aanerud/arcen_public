//! Validated region roster, deterministic round-robin service order, and the
//! close-one-clear-all teardown policy.
//!
//! Both hosts hand-wrote the same multiplexer shape: a `Vec` of
//! `(region id, queue)` pairs plus an [`AtomicUsize`] cursor, a rotating scan
//! that starts one past whichever entry won last time so a bursty region can
//! never starve a sparse one, and a teardown that closes *every* entry the
//! instant any one region's pipeline ends.
//!
//! This module owns the roster validation, the rotation order, and the
//! teardown fan-out. It deliberately does **not** own the queues: a queue is a
//! host type with a host wakeup model — a `Notify`, a `select_all` over
//! per-queue futures, a channel — and this crate embeds no executor and may
//! not depend on one. The payload is therefore a caller-chosen type the roster
//! only ever hands back by reference, and awaiting stays with the host.
//!
//! # The ordering hazard the teardown policy exists to close
//!
//! A rotating scan races every entry starting at the cursor, so an entry that
//! was closed on its own can lose that race to a different, still-open sibling
//! that happens to have a buffered item ready. A caller relying on "the next
//! dequeue will surely notice" could keep emitting a sibling's buffered items
//! after another region's pipeline has already ended, which violates the
//! whole-session atomic teardown policy. [`FairRoster::close_and_clear_all`]
//! closes and clears every entry up front, so there is nothing left to race.

use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use arcen_media::MAX_MULTI_MONITOR_COUNT;

/// Rejection building a [`FairRoster`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RosterError<K> {
    /// A roster with no regions cannot serve a session.
    Empty,
    /// The same region key appeared more than once.
    Duplicate(K),
    /// More regions than the shared multi-region maximum.
    TooMany { count: usize, limit: usize },
}

impl<K: fmt::Debug> fmt::Display for RosterError<K> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("a fair roster requires at least one region"),
            Self::Duplicate(key) => {
                write!(formatter, "duplicate region key in fair roster: {key:?}")
            }
            Self::TooMany { count, limit } => write!(
                formatter,
                "fair roster holds {count} regions, but at most {limit} are supported"
            ),
        }
    }
}

impl<K: fmt::Debug> std::error::Error for RosterError<K> {}

/// The indices to try, in service order, starting at the roster's cursor.
///
/// Allocation-free: this is what a host iterates on every delivery.
///
/// Deliberately not `Copy`, so a partially consumed service order can never be
/// duplicated by accident.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceOrder {
    start: usize,
    len: usize,
    offset: usize,
}

impl Iterator for ServiceOrder {
    type Item = usize;

    fn next(&mut self) -> Option<usize> {
        if self.offset >= self.len {
            return None;
        }
        let index = (self.start + self.offset) % self.len;
        self.offset += 1;
        Some(index)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.len - self.offset;
        (remaining, Some(remaining))
    }
}

impl ExactSizeIterator for ServiceOrder {}

/// A validated, non-empty roster of regions with a deterministic round-robin
/// service order.
///
/// The cursor and the closed flag are interior-mutable, so a host can hold the
/// roster behind a shared reference — the shape both hosts already use.
pub struct FairRoster<K, T> {
    entries: Vec<(K, T)>,
    cursor: AtomicUsize,
    closed: AtomicBool,
}

impl<K: Copy + Eq + fmt::Debug, T> fmt::Debug for FairRoster<K, T> {
    // The payload is a host queue holding encoded media; only the roster's own
    // observable facts are rendered.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FairRoster")
            .field(
                "regions",
                &self.entries.iter().map(|(key, _)| key).collect::<Vec<_>>(),
            )
            .field("cursor", &self.cursor())
            .field("closed", &self.is_closed())
            .finish_non_exhaustive()
    }
}

impl<K: Copy + Eq + fmt::Debug, T> FairRoster<K, T> {
    /// Builds a roster over one entry per applied region, in any order:
    /// delivery fairness does not depend on roster order.
    ///
    /// # Errors
    ///
    /// - [`RosterError::Empty`] when there are no entries.
    /// - [`RosterError::Duplicate`] when a region key repeats.
    /// - [`RosterError::TooMany`] when there are more entries than
    ///   [`MAX_MULTI_MONITOR_COUNT`].
    pub fn new(entries: impl IntoIterator<Item = (K, T)>) -> Result<Self, RosterError<K>> {
        let entries: Vec<(K, T)> = entries.into_iter().collect();
        if entries.is_empty() {
            return Err(RosterError::Empty);
        }
        if entries.len() > MAX_MULTI_MONITOR_COUNT {
            return Err(RosterError::TooMany {
                count: entries.len(),
                limit: MAX_MULTI_MONITOR_COUNT,
            });
        }
        // A roster is bounded by `MAX_MULTI_MONITOR_COUNT`, so the pairwise
        // scan is cheaper than hashing and needs no `Hash` bound on `K`.
        for (index, (key, _)) in entries.iter().enumerate() {
            if entries[..index].iter().any(|(seen, _)| seen == key) {
                return Err(RosterError::Duplicate(*key));
            }
        }
        Ok(Self {
            entries,
            cursor: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
        })
    }

    /// How many regions this roster routes. Always at least one.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Always `false`: a roster is validated non-empty at construction.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        false
    }

    /// Every region key, in construction order.
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.entries.iter().map(|(key, _)| *key)
    }

    /// This region's payload, or `None` when `key` is not part of this roster,
    /// for example a stale key from a prior or rejected topology.
    #[must_use]
    pub fn get(&self, key: K) -> Option<&T> {
        self.entries
            .iter()
            .find(|(entry, _)| *entry == key)
            .map(|(_, payload)| payload)
    }

    /// Whether `key` is part of this roster.
    #[must_use]
    pub fn contains(&self, key: K) -> bool {
        self.get(key).is_some()
    }

    /// The entry at `index` in construction order.
    #[must_use]
    pub fn entry(&self, index: usize) -> Option<(K, &T)> {
        self.entries
            .get(index)
            .map(|(key, payload)| (*key, payload))
    }

    /// Where the round-robin cursor currently points.
    #[must_use]
    pub fn cursor(&self) -> usize {
        self.cursor.load(Ordering::Relaxed) % self.entries.len()
    }

    /// The indices to try, starting at the cursor and wrapping once.
    ///
    /// The host races or polls the payloads in this order and reports the
    /// winner through [`Self::record_served`].
    #[must_use]
    pub fn service_order(&self) -> ServiceOrder {
        ServiceOrder {
            start: self.cursor(),
            len: self.entries.len(),
            offset: 0,
        }
    }

    /// Every entry in service order, with its index.
    pub fn entries_in_service_order(&self) -> impl Iterator<Item = (usize, K, &T)> + '_ {
        self.service_order().map(|index| {
            let (key, payload) = &self.entries[index];
            (index, *key, payload)
        })
    }

    /// Advances the cursor past the entry that was just served, so the next
    /// service order starts with that entry's successor.
    ///
    /// An out-of-range index is ignored, so a stale index can never move the
    /// cursor somewhere it could starve a region.
    pub fn record_served(&self, index: usize) {
        if index < self.entries.len() {
            self.cursor
                .store((index + 1) % self.entries.len(), Ordering::Relaxed);
        }
    }

    /// Whether this roster has been torn down.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    /// Atomically tears the whole roster down: marks it closed and applies
    /// `close_one` to **every** payload in construction order, not only the
    /// one whose pipeline actually ended.
    ///
    /// `close_one` must both close and discard whatever the payload has
    /// buffered, so every entry reports "closed and empty" at once and no
    /// already-buffered item of a surviving region can still be delivered.
    ///
    /// Idempotent: closing an already closed roster runs `close_one` again,
    /// which is safe because closing and clearing are themselves idempotent,
    /// and reports through [`Self::is_closed`] either way.
    pub fn close_and_clear_all(&self, mut close_one: impl FnMut(&T)) {
        self.closed.store(true, Ordering::Relaxed);
        for (_, payload) in &self.entries {
            close_one(payload);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use arcen_media::{MAX_MULTI_MONITOR_COUNT, SessionMonitorId};

    use super::{FairRoster, RosterError};

    fn region(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("nonzero region id")
    }

    #[derive(Debug, Default)]
    struct Queue {
        items: RefCell<Vec<u8>>,
        closed: RefCell<bool>,
    }

    impl Queue {
        fn with(items: &[u8]) -> Self {
            Self {
                items: RefCell::new(items.to_vec()),
                closed: RefCell::new(false),
            }
        }

        fn take(&self) -> Option<u8> {
            let mut items = self.items.borrow_mut();
            if items.is_empty() {
                None
            } else {
                Some(items.remove(0))
            }
        }

        fn close_and_clear(&self) {
            *self.closed.borrow_mut() = true;
            self.items.borrow_mut().clear();
        }
    }

    fn roster(count: u16) -> FairRoster<SessionMonitorId, Queue> {
        FairRoster::new((1..=count).map(|value| (region(value), Queue::default())))
            .expect("valid roster")
    }

    /// Serves one item using the roster's service order, exactly the way a
    /// host multiplexer would, and reports the winner back.
    fn serve(roster: &FairRoster<SessionMonitorId, Queue>) -> Option<(SessionMonitorId, u8)> {
        for (index, key, queue) in roster.entries_in_service_order() {
            if let Some(item) = queue.take() {
                roster.record_served(index);
                return Some((key, item));
            }
        }
        None
    }

    #[test]
    fn new_rejects_an_empty_roster() {
        let error = FairRoster::<SessionMonitorId, Queue>::new(Vec::new()).unwrap_err();
        assert_eq!(error, RosterError::Empty);
        assert!(error.to_string().contains("at least one region"));
    }

    #[test]
    fn new_rejects_a_duplicate_region_key() {
        let error = FairRoster::new(vec![
            (region(1), Queue::default()),
            (region(2), Queue::default()),
            (region(1), Queue::default()),
        ])
        .unwrap_err();
        assert_eq!(error, RosterError::Duplicate(region(1)));
    }

    #[test]
    fn new_rejects_more_regions_than_the_shared_maximum() {
        let count = u16::try_from(MAX_MULTI_MONITOR_COUNT + 1).expect("bounded count");
        let error = FairRoster::new((1..=count).map(|value| (region(value), Queue::default())))
            .unwrap_err();
        assert_eq!(
            error,
            RosterError::TooMany {
                count: MAX_MULTI_MONITOR_COUNT + 1,
                limit: MAX_MULTI_MONITOR_COUNT,
            }
        );
    }

    #[test]
    fn new_accepts_one_through_the_shared_maximum() {
        for count in 1..=MAX_MULTI_MONITOR_COUNT {
            let roster = roster(u16::try_from(count).expect("bounded count"));
            assert_eq!(roster.len(), count);
            assert!(!roster.is_empty());
            assert!(!roster.is_closed());
        }
    }

    #[test]
    fn get_routes_known_keys_and_refuses_a_stale_one() {
        let roster = roster(2);
        assert!(roster.get(region(1)).is_some());
        assert!(roster.contains(region(2)));
        assert!(roster.get(region(3)).is_none());
        assert!(!roster.contains(region(3)));
        assert_eq!(
            roster.keys().collect::<Vec<_>>(),
            [region(1), region(2)],
            "keys are reported in construction order"
        );
        assert_eq!(roster.entry(0).expect("first entry").0, region(1));
        assert!(roster.entry(2).is_none());
    }

    #[test]
    fn service_order_starts_at_the_cursor_and_wraps_exactly_once() {
        let roster = roster(4);
        assert_eq!(roster.service_order().collect::<Vec<_>>(), [0, 1, 2, 3]);
        roster.record_served(1);
        assert_eq!(roster.cursor(), 2);
        assert_eq!(roster.service_order().collect::<Vec<_>>(), [2, 3, 0, 1]);
        roster.record_served(3);
        assert_eq!(roster.service_order().collect::<Vec<_>>(), [0, 1, 2, 3]);
        assert_eq!(roster.service_order().len(), 4);
    }

    #[test]
    fn a_stale_index_never_moves_the_cursor() {
        let roster = roster(3);
        roster.record_served(1);
        roster.record_served(9);
        assert_eq!(roster.cursor(), 2);
    }

    #[test]
    fn round_robin_rotates_so_a_bursty_region_cannot_starve_a_sparse_one() {
        let roster = FairRoster::new(vec![
            (region(1), Queue::with(&[10, 11, 12, 13])),
            (region(2), Queue::with(&[20])),
            (region(3), Queue::with(&[30, 31])),
        ])
        .expect("valid roster");

        let mut served = Vec::new();
        while let Some(item) = serve(&roster) {
            served.push(item);
        }

        assert_eq!(
            served,
            [
                (region(1), 10),
                (region(2), 20),
                (region(3), 30),
                (region(1), 11),
                (region(3), 31),
                (region(1), 12),
                (region(1), 13),
            ],
            "every region is offered its turn before a bursty region repeats"
        );
    }

    #[test]
    fn a_single_region_roster_always_serves_that_region() {
        let roster = FairRoster::new(vec![(region(7), Queue::with(&[1, 2, 3]))]).expect("roster");
        assert_eq!(roster.service_order().collect::<Vec<_>>(), [0]);
        for expected in 1..=3 {
            assert_eq!(serve(&roster), Some((region(7), expected)));
        }
        assert_eq!(serve(&roster), None);
        assert_eq!(roster.cursor(), 0);
    }

    #[test]
    fn close_and_clear_all_tears_down_every_region_not_just_the_one_that_ended() {
        let roster = FairRoster::new(vec![
            (region(1), Queue::with(&[1, 2, 3])),
            (region(2), Queue::with(&[])),
            (region(3), Queue::with(&[9])),
        ])
        .expect("valid roster");

        roster.close_and_clear_all(Queue::close_and_clear);

        assert!(roster.is_closed());
        for key in [region(1), region(2), region(3)] {
            let queue = roster.get(key).expect("known region");
            assert!(*queue.closed.borrow(), "{key:?} must be closed");
            assert!(
                queue.items.borrow().is_empty(),
                "{key:?} must have discarded its buffered items"
            );
        }
        assert_eq!(
            serve(&roster),
            None,
            "no surviving region may deliver one more buffered item"
        );
    }

    #[test]
    fn close_and_clear_all_is_idempotent() {
        let roster = roster(2);
        let mut closed = 0_usize;
        roster.close_and_clear_all(|_| closed += 1);
        roster.close_and_clear_all(|_| closed += 1);
        assert_eq!(closed, 4);
        assert!(roster.is_closed());
    }

    #[test]
    fn debug_renders_the_roster_without_the_payloads() {
        let roster = roster(2);
        roster.record_served(0);
        let rendered = format!("{roster:?}");
        assert!(rendered.contains("FairRoster"), "{rendered}");
        assert!(rendered.contains("cursor: 1"), "{rendered}");
        assert!(rendered.contains("closed: false"), "{rendered}");
    }
}
