// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Pier-side refinement state for one monitor (ADR 0016 §3, R2, R3).
//!
//! The capture loop reports damage per captured frame; the encoder output
//! loop asks for the records to write **before** each access unit; a
//! refinement worker asks for due tiles and later reports their
//! fingerprints. The scheduler owns the source fence and the generation.

use super::RefineContract;
use super::digest::{DigestContext, canonical_digest};
use super::tile::{RefineGrid, TileRect, group_blocks};
use arcen_protocol::refine::{
    BarrierReason, BlockRuns, RefineBarrier, RefineDigest, RefineFence, RefineScope,
};
use std::collections::{BTreeSet, VecDeque};
use std::num::NonZeroU32;
use std::time::Duration;

/// Share of a monitor's blocks — counted **distinct**, each block once
/// however often it changed — changed within [`MOTION_WINDOW`] that pauses
/// refinement (2 %). Measured on a Linux X11 host at 3024×1900: an 800×700 window
/// dragged at 1200 px/s changes about 3 % of the blocks in 100 ms and 7 %
/// in 250 ms; the ADR's first 5 %/100 ms gate stayed open and refinement
/// chased the trail. A caret, a clock or typing stays far below 2 %.
pub const MOTION_PERCENT: u32 = 2;
/// Window of the motion gate.
pub const MOTION_WINDOW: Duration = Duration::from_millis(250);
/// Settle time before a digest is sent.
pub const DIGEST_SETTLE: Duration = Duration::from_millis(500);
/// A change within this long of the previous one makes a block "busy".
pub const BUSY_WINDOW: Duration = Duration::from_secs(1);
/// Each consecutive busy change doubles a block's stillness, up to this
/// many doublings (300 ms → 4.8 s): a block a moving object keeps crossing
/// is not refined after every pass, which measured as most of the
/// refinement work under localized motion.
pub const MAX_BUSY_DOUBLINGS: u8 = 4;

#[derive(Debug, Clone, Copy, Default)]
struct BlockState {
    /// When the block last changed; `None` before its first change.
    changed_at: Option<Duration>,
    /// Consecutive changes each within [`BUSY_WINDOW`] of the previous.
    busy: u8,
    /// Fence of the tile sent for its current content.
    sent: Option<u64>,
    /// Fingerprint of the sent content, once the worker reported it.
    fingerprint: Option<u128>,
}

/// One tile the worker should extract, compress and send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileJob {
    /// Identity, including the generation the job belongs to.
    pub scope: RefineScope,
    /// Fence whose source content the tile carries.
    pub valid_from_fence: u64,
    /// Rectangle in source pixels.
    pub rect: TileRect,
}

/// Refinement scheduler for one monitor.
///
/// Idle cost is O(1): a keepalive access unit only advances the fence, which
/// is not new content, so it neither schedules tiles nor triggers a digest;
/// a digest is due only when the covered set or its content changed
/// (`coverage_version`). Work per pump is proportional to the blocks that
/// are unsent or changed, never to the whole grid.
#[derive(Debug, Clone)]
pub struct PierMonitorRefine {
    scope: RefineScope,
    contract: RefineContract,
    grid: RefineGrid,
    still: Duration,
    fence: u64,
    blocks: Vec<BlockState>,
    /// Damage since the last emitted access unit (union across captures
    /// that were dropped or coalesced before encoding).
    pending: Vec<bool>,
    pending_list: Vec<u32>,
    /// Blocks without a tile for their current content.
    unsent: BTreeSet<u32>,
    /// Change events `(time, block)` within the motion window, oldest first.
    recent: VecDeque<(Duration, u32)>,
    last_change: Option<Duration>,
    barrier: Option<BarrierReason>,
    last_tile_at: Option<Duration>,
    /// Bumped whenever a block enters or leaves the fingerprinted set.
    coverage_version: u64,
    digest_sent_for: Option<u64>,
}

impl PierMonitorRefine {
    /// Starts refinement for a monitor at generation 1. A producer process
    /// should start from a per-process value instead
    /// ([`Self::with_first_generation`]), so a restarted producer never
    /// reuses a generation a Deck already holds (ADR 0016 R3).
    #[must_use]
    pub fn new(
        monitor_id: u16,
        topology_generation: u64,
        stream_epoch: u64,
        contract: RefineContract,
        grid: RefineGrid,
        still_ms: u32,
    ) -> Self {
        let count = grid.block_count();
        Self {
            scope: RefineScope {
                monitor_id,
                topology_generation,
                stream_epoch,
                generation: NonZeroU32::MIN,
            },
            contract,
            grid,
            still: Duration::from_millis(u64::from(still_ms)),
            fence: 0,
            blocks: vec![BlockState::default(); count as usize],
            pending: vec![false; count as usize],
            pending_list: Vec::new(),
            unsent: (0..count).collect(),
            recent: VecDeque::new(),
            last_change: None,
            barrier: Some(BarrierReason::Start),
            last_tile_at: None,
            coverage_version: 0,
            digest_sent_for: None,
        }
    }

    /// The first generation this scheduler announces.
    #[must_use]
    pub const fn with_first_generation(mut self, generation: NonZeroU32) -> Self {
        self.scope.generation = generation;
        self
    }

    /// Current identity.
    #[must_use]
    pub const fn scope(&self) -> RefineScope {
        self.scope
    }

    /// Last emitted source fence.
    #[must_use]
    pub const fn fence(&self) -> u64 {
        self.fence
    }

    /// Grid.
    #[must_use]
    pub const fn grid(&self) -> RefineGrid {
        self.grid
    }

    /// Starts a new generation: everything sent is forgotten, and the next
    /// access unit is preceded by a barrier. Used for recovery keyframes,
    /// geometry/contract/epoch changes, refine-lane loss, `refine_reset` and
    /// re-enable (ADR 0016 R3).
    pub fn reset(&mut self, reason: BarrierReason) {
        self.scope.generation = self
            .scope
            .generation
            .checked_add(1)
            .unwrap_or(NonZeroU32::MIN);
        for block in &mut self.blocks {
            block.sent = None;
            block.fingerprint = None;
        }
        self.unsent = (0..self.grid.block_count()).collect();
        self.barrier = Some(reason);
        self.coverage_version += 1;
        self.digest_sent_for = None;
    }

    /// A new stream epoch (encoder restart): new generation.
    pub fn new_epoch(&mut self, stream_epoch: u64) {
        self.scope.stream_epoch = stream_epoch;
        self.reset(BarrierReason::Recovery);
    }

    fn prune_recent(&mut self, now: Duration) {
        while self
            .recent
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) >= MOTION_WINDOW)
        {
            self.recent.pop_front();
        }
    }

    /// Reports one captured frame's damaged blocks (Keel's dirty blocks).
    pub fn on_capture(&mut self, dirty: impl IntoIterator<Item = usize>, now: Duration) {
        self.prune_recent(now);
        for index in dirty {
            let Some(block) = self.blocks.get_mut(index) else {
                continue;
            };
            block.busy = match block.changed_at {
                Some(previous) if now.saturating_sub(previous) < BUSY_WINDOW => {
                    block.busy.saturating_add(1).min(MAX_BUSY_DOUBLINGS)
                }
                _ => 0,
            };
            block.changed_at = Some(now);
            block.sent = None;
            if block.fingerprint.take().is_some() {
                self.coverage_version += 1;
            }
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            self.unsent.insert(index);
            self.recent.push_back((now, index));
            self.last_change = Some(now);
            if !self.pending[index as usize] {
                self.pending[index as usize] = true;
                self.pending_list.push(index);
            }
        }
    }

    /// Whether the motion gate is closed at `now`: more than
    /// [`MOTION_PERCENT`] of the blocks changed — each counted once — within
    /// [`MOTION_WINDOW`].
    #[must_use]
    pub fn in_motion(&self, now: Duration) -> bool {
        if self
            .last_change
            .is_none_or(|at| now.saturating_sub(at) >= MOTION_WINDOW)
        {
            return false;
        }
        // An event counts only if it is its block's latest change, so a
        // block that changed every frame is one block, not sixty.
        let distinct = self
            .recent
            .iter()
            .filter(|(at, index)| {
                now.saturating_sub(*at) < MOTION_WINDOW
                    && self.blocks[*index as usize].changed_at == Some(*at)
            })
            .count() as u64;
        distinct * 100 > u64::from(self.grid.block_count()) * u64::from(MOTION_PERCENT)
    }

    /// Records to write on the video lane immediately before the next
    /// access unit: a barrier when a generation starts, then the fence with
    /// every block invalidated since the previous access unit.
    pub fn before_access_unit(&mut self) -> Vec<Vec<u8>> {
        let mut out = Vec::with_capacity(2);
        if let Some(reason) = self.barrier.take() {
            out.push(
                RefineBarrier {
                    scope: self.scope,
                    reason,
                    contract: self.contract.to_bytes(),
                    source_width: self.grid.width,
                    source_height: self.grid.height,
                }
                .encode(),
            );
        }
        self.fence += 1;
        let mut changed = std::mem::take(&mut self.pending_list);
        changed.sort_unstable();
        for &index in &changed {
            self.pending[index as usize] = false;
        }
        out.push(
            RefineFence {
                scope: self.scope,
                fence: self.fence,
                block_count: self.grid.block_count(),
                invalidated: BlockRuns::from_sorted_indices(changed),
            }
            .encode(),
        );
        out
    }

    /// Tiles due at `now`: blocks still for `still_ms` (longer when busy)
    /// whose change has been emitted and that have no tile for their
    /// content, most recently settled first, grouped into ≤256×64
    /// rectangles. Nothing while a barrier is unsent, while in motion, or
    /// when `max_jobs` is zero.
    pub fn due_tiles(&mut self, now: Duration, max_jobs: usize) -> Vec<TileJob> {
        self.prune_recent(now);
        if self.barrier.is_some()
            || self.fence == 0
            || max_jobs == 0
            || self.unsent.is_empty()
            || self.in_motion(now)
        {
            return Vec::new();
        }
        let mut eligible: Vec<(Duration, u32)> = self
            .unsent
            .iter()
            .filter_map(|&index| {
                let block = &self.blocks[index as usize];
                let changed = block.changed_at.unwrap_or(Duration::ZERO);
                // A block whose change is not yet emitted has no fence to be
                // valid from: it waits for the next access unit.
                (!self.pending[index as usize]
                    && now.saturating_sub(changed) >= self.still * (1_u32 << block.busy))
                    .then_some((changed, index))
            })
            .collect();
        if eligible.is_empty() {
            return Vec::new();
        }
        eligible.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        let order: Vec<u32> = eligible.into_iter().map(|(_, index)| index).collect();
        let mut jobs = Vec::new();
        for rect in group_blocks(self.grid, &order).into_iter().take(max_jobs) {
            for index in self.grid.blocks_of(rect) {
                self.blocks[index as usize].sent = Some(self.fence);
                self.unsent.remove(&index);
            }
            jobs.push(TileJob {
                scope: self.scope,
                valid_from_fence: self.fence,
                rect,
            });
        }
        self.last_tile_at = Some(now);
        jobs
    }

    /// The worker extracted a job and fingerprinted its blocks. Ignored for
    /// another generation or for blocks that changed meanwhile.
    pub fn tile_fingerprints(&mut self, job: &TileJob, fingerprints: &[(u32, u128)]) {
        if job.scope != self.scope {
            return;
        }
        for &(index, fingerprint) in fingerprints {
            if let Some(block) = self.blocks.get_mut(index as usize)
                && block.sent == Some(job.valid_from_fence)
            {
                block.fingerprint = Some(fingerprint);
                self.coverage_version += 1;
            }
        }
    }

    /// A tile was not written (queue full, lane reset, motion): its blocks
    /// become due again. They stay invalidated on change regardless.
    pub fn tile_dropped(&mut self, job: &TileJob) {
        if job.scope != self.scope {
            return;
        }
        for index in self.grid.blocks_of(job.rect) {
            let block = &mut self.blocks[index as usize];
            if block.sent == Some(job.valid_from_fence) {
                block.sent = None;
                if block.fingerprint.take().is_some() {
                    self.coverage_version += 1;
                }
                self.unsent.insert(index);
            }
        }
    }

    fn digest_due(&self, now: Duration) -> bool {
        // O(1) when nothing covered changed: keepalive fences are not news.
        self.digest_sent_for != Some(self.coverage_version)
            && self.barrier.is_none()
            && self
                .last_tile_at
                .is_some_and(|at| now.saturating_sub(at) >= DIGEST_SETTLE)
            && !self.in_motion(now)
    }

    /// Blocks a digest would cover now (sent and fingerprinted), if the
    /// monitor has settled and the covered set changed since the last
    /// digest. For a damage source that can miss changes (a hash or OS
    /// rectangles), the caller re-fingerprints these blocks on the source
    /// and passes the result to [`Self::digest`].
    #[must_use]
    pub fn digest_candidates(&self, now: Duration) -> Option<Vec<u32>> {
        if !self.digest_due(now) {
            return None;
        }
        let covered: Vec<u32> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.sent.is_some() && block.fingerprint.is_some())
            .map(|(index, _)| u32::try_from(index).unwrap_or(u32::MAX))
            .collect();
        (!covered.is_empty()).then_some(covered)
    }

    /// Builds the digest from fingerprints recomputed on the current source.
    /// A block whose source no longer matches what was sent (a change the
    /// damage source missed) is treated as changed and leaves the coverage.
    pub fn digest(&mut self, now: Duration, recomputed: &[(u32, u128)]) -> Option<RefineDigest> {
        let mut covered = Vec::with_capacity(recomputed.len());
        for &(index, fingerprint) in recomputed {
            let Some(block) = self.blocks.get(index as usize) else {
                continue;
            };
            if block.sent.is_some() && block.fingerprint == Some(fingerprint) {
                covered.push((index, fingerprint));
            } else if block.sent.is_some() {
                self.on_capture([index as usize], now);
            }
        }
        self.finish_digest(covered)
    }

    /// A digest from the fingerprints taken when the tiles were extracted,
    /// without touching the source: correct when damage is exact (the
    /// producer's compare-and-copy mirror), where a re-read could only
    /// return the same bytes. `None` unless a digest is due.
    pub fn digest_from_stored(&mut self, now: Duration) -> Option<RefineDigest> {
        if !self.digest_due(now) {
            return None;
        }
        let covered: Vec<(u32, u128)> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, block)| block.sent.is_some())
            .filter_map(|(index, block)| Some((u32::try_from(index).ok()?, block.fingerprint?)))
            .collect();
        self.finish_digest(covered)
    }

    fn finish_digest(&mut self, mut covered: Vec<(u32, u128)>) -> Option<RefineDigest> {
        if covered.is_empty() {
            return None;
        }
        covered.sort_unstable_by_key(|(index, _)| *index);
        let context = DigestContext {
            scope: self.scope,
            fence: self.fence,
            contract: self.contract.to_bytes(),
            grid: self.grid,
        };
        let digest = canonical_digest(&context, &covered);
        self.digest_sent_for = Some(self.coverage_version);
        Some(RefineDigest {
            scope: self.scope,
            fence: self.fence,
            digest,
            block_count: self.grid.block_count(),
            coverage: BlockRuns::from_sorted_indices(covered.iter().map(|(index, _)| *index)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BitDepth, ChromaSubsampling, ColorPrimaries, ColorRange, TransferCharacteristics};
    use arcen_protocol::refine::RefineDomain;

    const MS: Duration = Duration::from_millis(1);

    #[test]
    fn a_block_that_keeps_changing_waits_longer_before_refinement() {
        let contract = RefineContract {
            domain: RefineDomain::Rgb,
            depth: BitDepth::Ten,
            transfer: TransferCharacteristics::Bt709,
            primaries: ColorPrimaries::Bt709,
            range: ColorRange::Full,
            chroma: ChromaSubsampling::Yuv444,
        };
        let grid = RefineGrid {
            width: 1024,
            height: 1024,
        };
        let mut refine = PierMonitorRefine::new(1, 1, 1, contract, grid, 300);
        refine.on_capture(0..grid.block_count() as usize, MS * 10);
        refine.before_access_unit();
        // Block 0 is crossed three times within a second each.
        for at in [200, 500, 800] {
            refine.on_capture([0], MS * at);
            refine.before_access_unit();
        }
        let due = |refine: &mut PierMonitorRefine, at: u32| {
            refine
                .due_tiles(MS * at, usize::MAX)
                .iter()
                .any(|job| job.rect.x == 0 && job.rect.y == 0)
        };
        // An ordinary block refines 300 ms after its change; block 0 is busy
        // (three doublings: 2.4 s).
        assert!(!due(&mut refine, 1_200));
        assert!(!due(&mut refine, 3_100));
        assert!(due(&mut refine, 3_300));
    }
}
