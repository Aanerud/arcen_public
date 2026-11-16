// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Pier's forwarding rule for video-lane refinement records (ADR 0016
//! R2: "the Pier's video queue never loses a fence"). Wire-level, so every
//! queue that may drop access units uses it: the Linux Pier's send queue,
//! the Windows agent's frame queue and the split Piers' lane relay
//! (`arcen-transport`, which does not depend on `arcen-media`).
//!
//! capenc writes a `RefineBarrier`/`RefineFence` immediately before the
//! access unit it describes. The Pier's send queue may drop or suppress
//! access units under pressure; their records must not be dropped with
//! them, or the Deck would keep showing refined blocks over content that
//! changed. A [`RefineVideoCarry`] keeps every record tagged with the
//! sequence number of its access unit; the video writer takes, before the
//! access unit `s` it actually sends, every record up to `s`, merged:
//!
//! - a barrier supersedes everything before it (the Deck starts over on
//!   it), so only the newest barrier and the fences after it survive;
//! - the fences after it collapse into one, carrying the **union** of their
//!   invalidations and the fence number of the access unit being sent.
//!
//! Clearing a block early is the safe side (the Deck shows the lossy video
//! of the same content for a frame); clearing late never happens.

use super::{BlockRuns, RefineBarrier, RefineFence};
use crate::wire::FrameType;
use std::collections::VecDeque;

/// Records held at most. Beyond it the oldest half is merged into one
/// set filed under the **newest** of those access units, with the newest
/// fence number: the older access units then go out without a fence, and a
/// frame without a fence never shows refined tiles on the Deck, while every
/// invalidation keeps a fence number at least as new as its change (a lower
/// one would let a tile captured before the change pass the Deck's check).
pub const MAX_CARRIED_RECORDS: usize = 512;

/// Video-lane refinement records waiting for their access unit.
#[derive(Debug, Default)]
pub struct RefineVideoCarry {
    entries: VecDeque<(u64, Vec<u8>)>,
    merges: u64,
}

impl RefineVideoCarry {
    /// An empty carry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records that precede access unit `seq`, in capenc's order. Sequence
    /// numbers never decrease.
    pub fn push(&mut self, seq: u64, records: impl IntoIterator<Item = Vec<u8>>) {
        self.entries
            .extend(records.into_iter().map(|record| (seq, record)));
        if self.entries.len() > MAX_CARRIED_RECORDS {
            let half = self.entries.len() / 2;
            let oldest: Vec<(u64, Vec<u8>)> = self.entries.drain(..half).collect();
            let last = oldest.last().map_or(0, |(seq, _)| *seq);
            let merged = merge(oldest.into_iter().map(|(_, record)| record).collect());
            for record in merged.into_iter().rev() {
                self.entries.push_front((last, record));
            }
            self.merges += 1;
        }
    }

    /// The records to write before access unit `seq`: every record up to
    /// it, including those of access units that were dropped, merged.
    pub fn take_through(&mut self, seq: u64) -> Vec<Vec<u8>> {
        let count = self.entries.iter().take_while(|(at, _)| *at <= seq).count();
        if count == 0 {
            return Vec::new();
        }
        let records: Vec<Vec<u8>> = self
            .entries
            .drain(..count)
            .map(|(_, record)| record)
            .collect();
        if records.len() == 1 {
            return records;
        }
        merge(records)
    }

    /// Forgets everything (the queue was cleared for a new capture epoch).
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    /// Records held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Times the bound forced an in-place merge.
    #[must_use]
    pub const fn overflow_merges(&self) -> u64 {
        self.merges
    }
}

struct MergedFence {
    first: RefineFence,
    /// The fences' runs, unexpanded: memory follows the records' own size,
    /// never the grid a (possibly hostile) record claims.
    runs: Vec<(u32, u32)>,
    fence: u64,
}

impl MergedFence {
    fn finish(self) -> Vec<u8> {
        RefineFence {
            scope: self.first.scope,
            fence: self.fence,
            block_count: self.first.block_count,
            invalidated: union_runs(self.runs),
        }
        .encode()
    }
}

/// The union of block runs: sorted, overlapping and touching runs joined.
fn union_runs(mut runs: Vec<(u32, u32)>) -> BlockRuns {
    runs.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::with_capacity(runs.len());
    for (start, len) in runs {
        let end = u64::from(start) + u64::from(len);
        match out.last_mut() {
            Some((last_start, last_len))
                if u64::from(start) <= u64::from(*last_start) + u64::from(*last_len) =>
            {
                let last_end = (u64::from(*last_start) + u64::from(*last_len)).max(end);
                *last_len = u32::try_from(last_end - u64::from(*last_start)).unwrap_or(u32::MAX);
            }
            _ => out.push((start, len)),
        }
    }
    BlockRuns::from_runs_unchecked(out)
}

/// Merges records in lane order: only the newest barrier and the fences
/// after it survive, collapsed into one fence with the union of their
/// invalidations and the newest fence number.
fn merge(records: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    // A barrier on the ordered lane makes everything before it moot.
    let start = records
        .iter()
        .rposition(|record| {
            record.first() == Some(&(FrameType::RefineBarrier as u8))
                && RefineBarrier::decode(record).is_ok()
        })
        .unwrap_or(0);
    let mut out = Vec::new();
    let mut current: Option<MergedFence> = None;
    for record in records.into_iter().skip(start) {
        let fence = (record.first() == Some(&(FrameType::RefineFence as u8)))
            .then(|| RefineFence::decode(&record).ok())
            .flatten();
        if let Some(fence) = fence {
            match current.as_mut() {
                Some(merged)
                    if merged.first.scope == fence.scope
                        && merged.first.block_count == fence.block_count =>
                {
                    merged.runs.extend_from_slice(fence.invalidated.runs());
                    merged.fence = merged.fence.max(fence.fence);
                }
                _ => {
                    if let Some(previous) = current.take() {
                        out.push(previous.finish());
                    }
                    current = Some(MergedFence {
                        runs: fence.invalidated.runs().to_vec(),
                        fence: fence.fence,
                        first: fence,
                    });
                }
            }
        } else {
            if let Some(previous) = current.take() {
                out.push(previous.finish());
            }
            out.push(record);
        }
    }
    if let Some(previous) = current.take() {
        out.push(previous.finish());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refine::{BarrierReason, RefineContractBytes, RefineScope};
    use std::num::NonZeroU32;

    fn scope(generation: u32) -> RefineScope {
        RefineScope {
            monitor_id: 1,
            topology_generation: 1,
            stream_epoch: 1,
            generation: NonZeroU32::new(generation).unwrap(),
        }
    }

    fn fence(generation: u32, number: u64, blocks: &[u32]) -> Vec<u8> {
        RefineFence {
            scope: scope(generation),
            fence: number,
            block_count: 100,
            invalidated: BlockRuns::from_sorted_indices(blocks.iter().copied()),
        }
        .encode()
    }

    fn barrier(generation: u32) -> Vec<u8> {
        RefineBarrier {
            scope: scope(generation),
            reason: BarrierReason::Recovery,
            contract: RefineContractBytes {
                domain: 1,
                depth: 10,
                transfer: 1,
                primaries: 1,
                matrix: 0,
                range: 1,
                chroma: 2,
            },
            source_width: 160,
            source_height: 160,
        }
        .encode()
    }

    #[test]
    fn a_sent_access_unit_takes_only_its_own_records() {
        let mut carry = RefineVideoCarry::new();
        carry.push(1, [fence(2, 1, &[3])]);
        carry.push(2, [fence(2, 2, &[4])]);
        assert_eq!(carry.take_through(1), vec![fence(2, 1, &[3])]);
        assert_eq!(carry.take_through(2), vec![fence(2, 2, &[4])]);
        assert!(carry.is_empty());
    }

    #[test]
    fn dropped_access_units_hand_their_invalidations_to_the_next_one_sent() {
        let mut carry = RefineVideoCarry::new();
        carry.push(1, [fence(2, 1, &[3, 4])]);
        carry.push(2, [fence(2, 2, &[10])]);
        carry.push(3, [fence(2, 3, &[4, 50])]);
        // Access units 1 and 2 were dropped by the send queue.
        let records = carry.take_through(3);
        assert_eq!(records.len(), 1);
        let merged = RefineFence::decode(&records[0]).unwrap();
        assert_eq!(merged.fence, 3, "the fence of the access unit sent");
        assert_eq!(
            merged.invalidated.indices().collect::<Vec<_>>(),
            vec![3, 4, 10, 50]
        );
    }

    #[test]
    fn merging_whole_grid_fences_never_expands_the_grid() {
        // A fence of a few bytes may claim the largest grid covered by one
        // run: merging hundreds of them must cost their bytes, not the grid.
        let huge = |number: u64, start: u32, len: u32| {
            RefineFence {
                scope: scope(2),
                fence: number,
                block_count: 1 << 20,
                invalidated: BlockRuns::new(vec![(start, len)], 1 << 20).unwrap(),
            }
            .encode()
        };
        let mut carry = RefineVideoCarry::new();
        for seq in 1..=400_u64 {
            carry.push(seq, [huge(seq, 0, 1 << 20)]);
        }
        carry.push(401, [huge(401, 10, 5), huge(401, 12, 9)]);
        let records = carry.take_through(401);
        assert_eq!(records.len(), 1);
        let merged = RefineFence::decode(&records[0]).unwrap();
        assert_eq!(merged.invalidated.runs(), &[(0, 1 << 20)]);
        assert_eq!(merged.fence, 401);
        // Overlapping and touching runs join; gaps stay.
        let mut carry = RefineVideoCarry::new();
        carry.push(
            1,
            [
                huge(1, 10, 5),
                huge(1, 12, 9),
                huge(1, 21, 2),
                huge(1, 30, 1),
            ],
        );
        let merged = RefineFence::decode(&carry.take_through(1)[0]).unwrap();
        assert_eq!(merged.invalidated.runs(), &[(10, 13), (30, 1)]);
    }

    #[test]
    fn a_dropped_barrier_survives_and_supersedes_older_fences() {
        let mut carry = RefineVideoCarry::new();
        carry.push(1, [fence(2, 7, &[1])]);
        carry.push(2, [barrier(3), fence(3, 1, &[])]);
        carry.push(3, [fence(3, 2, &[9])]);
        let records = carry.take_through(3);
        assert_eq!(records.len(), 2);
        assert_eq!(RefineBarrier::decode(&records[0]).unwrap().scope, scope(3));
        let merged = RefineFence::decode(&records[1]).unwrap();
        assert_eq!((merged.scope, merged.fence), (scope(3), 2));
        assert_eq!(merged.invalidated.indices().collect::<Vec<_>>(), vec![9]);
    }

    #[test]
    fn the_bound_never_files_an_invalidation_under_an_older_fence() {
        let mut carry = RefineVideoCarry::new();
        let total = MAX_CARRIED_RECORDS as u64 + 1;
        for seq in 1..=total {
            carry.push(seq, [fence(2, seq, &[u32::try_from(seq % 100).unwrap()])]);
        }
        assert!(carry.len() <= MAX_CARRIED_RECORDS);
        assert_eq!(carry.overflow_merges(), 1);
        // The oldest access units go out without a fence (no tiles shown
        // over them); the merged invalidations travel under the newest
        // merged fence, never an older one.
        assert!(carry.take_through(1).is_empty());
        let merged_at = total / 2;
        let records = carry.take_through(merged_at);
        let merged = RefineFence::decode(&records[0]).unwrap();
        assert_eq!(merged.fence, merged_at);
        assert!(merged.invalidated.count() > 1);
    }
}
