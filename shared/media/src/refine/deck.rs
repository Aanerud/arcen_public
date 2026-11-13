// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Deck-side refinement state owner (ADR 0016 R2–R5, R7).
//!
//! One [`DeckRefineSession`] per connection is the only place that decides
//! whether a refined block is valid. The video lane reader feeds it fences
//! and barriers (in video order, before the frame queue), the refine lane
//! reader feeds it tiles and digests, the decode worker returns decoded
//! codes through [`DeckRefineSession::commit`] (which revalidates), and the
//! presenter reports presented fences and takes GPU overlay deltas.

use super::BLOCK;
use super::digest::{DigestContext, canonical_digest, tile_block_fingerprints};
use super::limits::{
    MALFORMED_DISABLE_COUNT, MALFORMED_WINDOW, MAX_PENDING_AGE, MAX_PENDING_DECODED_BYTES,
    MAX_PENDING_JOBS, MAX_STAGING_BYTES_PER_FRAME, MAX_TILES_PER_SECOND, SlidingCount,
};
use super::tile::{RefineGrid, TileCodes, TileRect};
use arcen_protocol::refine::{
    BlockRuns, RefineBarrier, RefineContractBytes, RefineControlMsg, RefineDigest, RefineFence,
    RefineRecordError, RefineScope, RefineTileHeader,
};
use arcen_protocol::wire::FrameType;
use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

/// Monitors one session may refine (a peer cannot make the Deck allocate
/// state for every possible monitor id).
pub const MAX_REFINE_MONITORS: usize = 16;
/// Blocks all refined monitors of a session may hold together (one
/// 16384×16384 monitor, or e.g. eight 8K monitors).
pub const MAX_SESSION_REFINE_BLOCKS: u64 = 1 << 20;

/// Bytes one overlay block occupies in the GPU staging buffer (`RGB10A2`).
pub const STAGING_BYTES_PER_BLOCK: usize = BLOCK * BLOCK * 4;
/// How long a digest may wait for its covered blocks before the Deck asks
/// the Pier to start over.
pub const DIGEST_COMPLETION_TIMEOUT: Duration = Duration::from_secs(3);

/// A verification level (ADR 0016 R4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VerificationLevel {
    /// Decompressed source codes equal the Pier's digest.
    SourceCodes = 1,
    /// The GPU overlay texture holds exactly those codes.
    GpuOverlay = 2,
    /// The rendered composite holds exactly those codes.
    RenderedRgb = 3,
}

/// A published verdict.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// Highest level reached.
    pub level: VerificationLevel,
    /// Fence the digest described.
    pub fence: u64,
    /// Covered blocks.
    pub coverage: BlockRuns,
    /// Blocks in the grid.
    pub block_count: u32,
    fingerprints: Vec<(u32, u128)>,
}

impl Verdict {
    /// Coverage in tenths of a percent.
    #[must_use]
    pub fn coverage_permille(&self) -> u32 {
        permille(self.coverage.count(), self.block_count)
    }
}

fn permille(part: u64, whole: u32) -> u32 {
    if whole == 0 {
        0
    } else {
        u32::try_from(part * 1000 / u64::from(whole)).unwrap_or(1000)
    }
}

/// Why a record was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineDrop {
    /// Refinement is disabled for the session or monitor.
    Disabled,
    /// No generation has started for the monitor.
    NoGeneration,
    /// An older generation, topology or epoch.
    StaleGeneration,
    /// A generation whose barrier has not arrived on the video lane.
    FutureGeneration,
    /// Every block changed after the tile was captured.
    Stale,
    /// Above the tile rate.
    Rate,
    /// Pending bytes or jobs at their bound.
    Backpressure,
    /// Malformed (counted towards the disable threshold).
    Malformed(RefineRecordError),
    /// The job outlived [`MAX_PENDING_AGE`].
    Expired,
}

/// Why refinement was disabled for the session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineDisabled {
    /// Too many malformed records.
    MalformedRecords,
}

/// What a video-lane record meant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoRecord {
    /// The next access unit of `monitor_id` carries `fence`.
    Fence {
        /// Monitor.
        monitor_id: u16,
        /// Fence.
        fence: u64,
    },
    /// A generation started.
    Barrier {
        /// Monitor.
        monitor_id: u16,
    },
    /// Ignored (stale or for a disabled monitor).
    Ignored,
}

/// An admitted tile awaiting decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileTicket {
    job: u64,
    /// Monitor.
    pub monitor_id: u16,
    /// Identity at admission.
    pub scope: RefineScope,
    /// Fence the tile is valid from.
    pub valid_from_fence: u64,
    /// Rectangle.
    pub rect: TileRect,
    /// Code depth.
    pub depth: u8,
    /// Expanded bytes the decoder must produce.
    pub expanded_bytes: usize,
}

/// Result of [`DeckRefineSession::on_refine_record`].
#[derive(Debug, PartialEq, Eq)]
pub enum RefineAdmission {
    /// Decode this tile's payload off the owner, then call `commit`.
    Tile(TileTicket),
    /// A digest was stored (or discarded as superseded).
    Digest,
    /// Dropped.
    Dropped(RefineDrop),
}

/// Overlay changes for one presented frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OverlayDelta {
    /// Clear every block first (new generation).
    pub clear_all: bool,
    /// Blocks to clear.
    pub clear: Vec<TileRect>,
    /// Blocks to write: rectangle and planar codes.
    pub set: Vec<(TileRect, Vec<u16>)>,
}

impl OverlayDelta {
    /// Whether nothing changes.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        !self.clear_all && self.clear.is_empty() && self.set.is_empty()
    }
}

/// Counters for the debug view and telemetry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DeckRefineStats {
    /// Tiles admitted for decode.
    pub tiles_admitted: u64,
    /// Blocks committed into the overlay.
    pub blocks_committed: u64,
    /// Blocks refused at commit because they changed during decode.
    pub blocks_stale_at_commit: u64,
    /// Records dropped (any reason).
    pub dropped: u64,
    /// Malformed records.
    pub malformed: u64,
    /// Fences applied.
    pub fences: u64,
    /// Blocks invalidated.
    pub blocks_invalidated: u64,
    /// Generations started.
    pub generations: u64,
    /// Digests that matched.
    pub digest_matches: u64,
    /// Digests that did not.
    pub digest_mismatches: u64,
    /// Compressed payload bytes admitted.
    pub payload_bytes: u64,
}

#[derive(Debug, Clone)]
struct OverlayBlock {
    fence: u64,
    fingerprint: u128,
    /// Codes waiting for upload; `None` once on the GPU.
    staged: Option<Vec<u16>>,
}

#[derive(Debug, Clone, Copy)]
struct Generation {
    scope: RefineScope,
    contract: RefineContractBytes,
    grid: RefineGrid,
}

#[derive(Debug, Clone)]
struct PendingDigest {
    record: RefineDigest,
    received_at: Duration,
}

/// Refinement state of one monitor.
#[derive(Debug, Clone, Default)]
struct MonitorState {
    disabled: bool,
    generation: Option<Generation>,
    received_fence: u64,
    presented_fence: u64,
    invalidated_at: Vec<u64>,
    overlay: Vec<Option<OverlayBlock>>,
    on_gpu: Vec<bool>,
    clear_all: bool,
    digest: Option<PendingDigest>,
    verdict: Option<Verdict>,
}

impl MonitorState {
    fn adopt(&mut self, generation: Generation) {
        let count = generation.grid.block_count() as usize;
        self.generation = Some(generation);
        self.received_fence = 0;
        self.presented_fence = 0;
        self.invalidated_at = vec![0; count];
        self.overlay = vec![None; count];
        self.on_gpu = vec![false; count];
        self.clear_all = true;
        self.digest = None;
        self.verdict = None;
    }

    fn clear(&mut self) {
        self.generation = None;
        self.overlay.clear();
        self.invalidated_at.clear();
        self.on_gpu.clear();
        self.clear_all = true;
        self.digest = None;
        self.verdict = None;
    }

    fn visible_blocks(&self) -> u64 {
        self.overlay
            .iter()
            .flatten()
            .filter(|block| block.fence <= self.presented_fence)
            .count() as u64
    }
}

/// Per-monitor status for the user and the debug view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorRefineStatus {
    /// Whether a generation is active.
    pub active: bool,
    /// Visible refined blocks in tenths of a percent.
    pub refined_permille: u32,
    /// The current verdict, if any.
    pub verdict: Option<Verdict>,
    /// One line for the status bar.
    pub text: String,
}

/// The Deck's refinement state owner for one connection.
#[derive(Debug, Default)]
pub struct DeckRefineSession {
    monitors: BTreeMap<u16, MonitorState>,
    jobs: HashMap<u64, (usize, Duration)>,
    pending_bytes: usize,
    next_job: u64,
    tile_rate: SlidingCount,
    malformed: SlidingCount,
    disabled: Option<RefineDisabled>,
    resets: Vec<RefineControlMsg>,
    stats: DeckRefineStats,
}

impl DeckRefineSession {
    /// Empty state; nothing is refined until a barrier arrives.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Counters.
    #[must_use]
    pub const fn stats(&self) -> DeckRefineStats {
        self.stats
    }

    /// Session-wide disable, if any.
    #[must_use]
    pub const fn disabled(&self) -> Option<RefineDisabled> {
        self.disabled
    }

    /// Control messages to send to the Pier (`refine_reset`,
    /// `refine_control`).
    pub fn take_control_messages(&mut self) -> Vec<RefineControlMsg> {
        std::mem::take(&mut self.resets)
    }

    fn note_malformed(&mut self, error: RefineRecordError, now: Duration) -> RefineDrop {
        self.stats.malformed += 1;
        self.stats.dropped += 1;
        if self.malformed.note(now, MALFORMED_WINDOW) >= MALFORMED_DISABLE_COUNT {
            self.disable_session(RefineDisabled::MalformedRecords);
        }
        RefineDrop::Malformed(error)
    }

    fn drop_with(&mut self, reason: RefineDrop) -> RefineDrop {
        self.stats.dropped += 1;
        reason
    }

    fn disable_session(&mut self, reason: RefineDisabled) {
        self.disabled = Some(reason);
        for state in self.monitors.values_mut() {
            state.clear();
        }
        self.jobs.clear();
        self.pending_bytes = 0;
    }

    /// Enables or disables refinement for one monitor (for example when the
    /// ten-bit layer falls back to eight bits). Disabling clears the
    /// overlay at once; re-enabling asks the Pier for a new generation.
    pub fn set_monitor_enabled(&mut self, monitor_id: u16, enabled: bool) {
        if !self.monitors.contains_key(&monitor_id) && self.monitors.len() >= MAX_REFINE_MONITORS {
            return;
        }
        let state = self.monitors.entry(monitor_id).or_default();
        if state.disabled != enabled {
            return;
        }
        state.disabled = !enabled;
        if !enabled {
            state.clear();
        }
        self.resets.push(RefineControlMsg::RefineControl {
            monitor_id,
            enabled,
        });
    }

    /// The decoder of a monitor was torn down or recovered, or the session
    /// reconnected: drop everything for it and ask for a new generation.
    /// Recovery keyframes never inherit validity (ADR 0016 R3).
    pub fn decoder_reset(&mut self, monitor_id: u16, reason: &str) {
        let Some(state) = self.monitors.get_mut(&monitor_id) else {
            return;
        };
        let epoch = state
            .generation
            .map_or(0, |generation| generation.scope.stream_epoch);
        state.clear();
        self.resets.push(RefineControlMsg::RefineReset {
            monitor_id,
            stream_epoch: epoch,
            reason: reason.to_string(),
        });
    }

    /// A record from the **video** lane (`RefineFence`/`RefineBarrier`),
    /// in lane order, before the frame queue.
    ///
    /// # Errors
    ///
    /// The drop reason for a malformed or out-of-order record.
    pub fn on_video_record(
        &mut self,
        bytes: &[u8],
        now: Duration,
    ) -> Result<VideoRecord, RefineDrop> {
        if self.disabled.is_some() {
            return Ok(VideoRecord::Ignored);
        }
        match bytes.first().copied().map(FrameType::try_from) {
            Some(Ok(FrameType::RefineBarrier)) => match RefineBarrier::decode(bytes) {
                Ok(barrier) => self.on_barrier(&barrier, now),
                Err(error) => Err(self.note_malformed(error, now)),
            },
            Some(Ok(FrameType::RefineFence)) => match RefineFence::decode(bytes) {
                Ok(fence) => self.on_fence(&fence, now),
                Err(error) => Err(self.note_malformed(error, now)),
            },
            _ => Err(self.note_malformed(
                RefineRecordError::WrongType(bytes.first().copied().unwrap_or(0)),
                now,
            )),
        }
    }

    /// Fail-safe: drop everything refined for a monitor and ask the Pier
    /// for a new generation.
    fn fail_safe_clear(&mut self, monitor_id: u16, reason: &str) {
        if let Some(state) = self.monitors.get_mut(&monitor_id) {
            let epoch = state
                .generation
                .map_or(0, |generation| generation.scope.stream_epoch);
            state.clear();
            self.resets.push(RefineControlMsg::RefineReset {
                monitor_id,
                stream_epoch: epoch,
                reason: reason.to_string(),
            });
        }
    }

    /// A barrier on the ordered video lane is authoritative (ADR 0016 R3):
    /// whatever generation it names — newer, older or the same — starts
    /// over, because nothing reorders that lane and a producer that
    /// restarted may legitimately reuse an identity. Two exceptions, both
    /// refused and fail-safe: the same identity with a different source
    /// size (a generation id is never reused for other geometry), and a
    /// monitor beyond the session's monitor or block budget.
    fn on_barrier(
        &mut self,
        barrier: &RefineBarrier,
        now: Duration,
    ) -> Result<VideoRecord, RefineDrop> {
        let monitor_id = barrier.scope.monitor_id;
        let grid = RefineGrid {
            width: barrier.source_width,
            height: barrier.source_height,
        };
        if !self.monitors.contains_key(&monitor_id) && self.monitors.len() >= MAX_REFINE_MONITORS {
            return Err(self.note_malformed(RefineRecordError::Value, now));
        }
        let others: u64 = self
            .monitors
            .iter()
            .filter(|(id, _)| **id != monitor_id)
            .filter_map(|(_, state)| state.generation)
            .map(|generation| u64::from(generation.grid.block_count()))
            .sum();
        if others + u64::from(grid.block_count()) > MAX_SESSION_REFINE_BLOCKS {
            self.fail_safe_clear(monitor_id, "block_budget");
            return Err(self.note_malformed(RefineRecordError::BlockCount, now));
        }
        let state = self.monitors.entry(monitor_id).or_default();
        if state.disabled {
            return Ok(VideoRecord::Ignored);
        }
        if let Some(current) = state.generation
            && current.scope == barrier.scope
            && current.grid != grid
        {
            self.fail_safe_clear(monitor_id, "generation_geometry_reuse");
            return Err(self.note_malformed(RefineRecordError::Geometry, now));
        }
        state.adopt(Generation {
            scope: barrier.scope,
            contract: barrier.contract,
            grid,
        });
        // Jobs of the old generation are refused at commit; their pending
        // bytes are released there.
        self.stats.generations += 1;
        Ok(VideoRecord::Barrier { monitor_id })
    }

    fn on_fence(&mut self, fence: &RefineFence, now: Duration) -> Result<VideoRecord, RefineDrop> {
        let Some(state) = self.monitors.get_mut(&fence.scope.monitor_id) else {
            return Ok(VideoRecord::Ignored);
        };
        let Some(generation) = state.generation else {
            return Ok(VideoRecord::Ignored);
        };
        // On the ordered lane every fence follows its own barrier, so a
        // fence for any other identity, an out-of-order fence or another
        // grid means the Deck no longer knows what the overlay shows: clear
        // it (fail safe) and ask for a new generation.
        if fence.scope != generation.scope
            || fence.fence <= state.received_fence
            || fence.block_count != generation.grid.block_count()
        {
            self.fail_safe_clear(fence.scope.monitor_id, "fence_mismatch");
            return Err(self.note_malformed(RefineRecordError::Value, now));
        }
        state.received_fence = fence.fence;
        let mut cleared = Vec::new();
        for index in fence.invalidated.indices() {
            let slot = index as usize;
            state.invalidated_at[slot] = fence.fence;
            // Clear on receipt unless the overlay already holds content
            // captured at or after this fence (a tile that overtook it).
            if state.overlay[slot]
                .as_ref()
                .is_some_and(|block| block.fence < fence.fence)
            {
                state.overlay[slot] = None;
                cleared.push(index);
            }
        }
        self.stats.fences += 1;
        self.stats.blocks_invalidated += fence.invalidated.count();
        if !fence.invalidated.is_empty() {
            let mut hit = vec![false; state.invalidated_at.len()];
            for index in fence.invalidated.indices() {
                hit[index as usize] = true;
            }
            let touches = |runs: &BlockRuns| {
                runs.indices()
                    .any(|index| hit.get(index as usize) == Some(&true))
            };
            if state
                .verdict
                .as_ref()
                .is_some_and(|verdict| touches(&verdict.coverage))
            {
                state.verdict = None;
            }
            if state
                .digest
                .as_ref()
                .is_some_and(|digest| touches(&digest.record.coverage))
            {
                state.digest = None;
            }
        }
        Ok(VideoRecord::Fence {
            monitor_id: fence.scope.monitor_id,
            fence: fence.fence,
        })
    }

    /// A record from the **refine** lane (`RefineTiles`/`RefineDigest`).
    pub fn on_refine_record(&mut self, bytes: &[u8], now: Duration) -> RefineAdmission {
        if self.disabled.is_some() {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::Disabled));
        }
        match bytes.first().copied().map(FrameType::try_from) {
            Some(Ok(FrameType::RefineTiles)) => match RefineTileHeader::decode(bytes) {
                Ok((header, _)) => self.admit_tile(&header, now),
                Err(error) => RefineAdmission::Dropped(self.note_malformed(error, now)),
            },
            Some(Ok(FrameType::RefineDigest)) => match RefineDigest::decode(bytes) {
                Ok(digest) => {
                    self.on_digest(digest, now);
                    RefineAdmission::Digest
                }
                Err(error) => RefineAdmission::Dropped(self.note_malformed(error, now)),
            },
            _ => RefineAdmission::Dropped(self.note_malformed(
                RefineRecordError::WrongType(bytes.first().copied().unwrap_or(0)),
                now,
            )),
        }
    }

    fn admit_tile(&mut self, header: &RefineTileHeader, now: Duration) -> RefineAdmission {
        self.expire(now);
        let Some(state) = self.monitors.get(&header.scope.monitor_id) else {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::NoGeneration));
        };
        if state.disabled {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::Disabled));
        }
        let Some(generation) = state.generation else {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::NoGeneration));
        };
        let current = generation.scope;
        if header.scope.topology_generation != current.topology_generation
            || header.scope.stream_epoch != current.stream_epoch
            || header.scope.generation < current.generation
        {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::StaleGeneration));
        }
        if header.scope.generation > current.generation {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::FutureGeneration));
        }
        let rect = TileRect {
            x: u32::from(header.x),
            y: u32::from(header.y),
            width: u32::from(header.width),
            height: u32::from(header.height),
        };
        if header.domain.wire() != generation.contract.domain
            || header.depth != generation.contract.depth
            || generation.grid.check(rect).is_err()
        {
            return RefineAdmission::Dropped(self.note_malformed(RefineRecordError::Geometry, now));
        }
        let blocks = generation.grid.blocks_of(rect);
        if blocks
            .iter()
            .all(|index| state.invalidated_at[*index as usize] > header.valid_from_fence)
        {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::Stale));
        }
        if self.tile_rate.note(now, Duration::from_secs(1)) > MAX_TILES_PER_SECOND {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::Rate));
        }
        let expanded = rect.expanded_bytes();
        if self.jobs.len() >= MAX_PENDING_JOBS
            || self.pending_bytes + expanded > MAX_PENDING_DECODED_BYTES
        {
            return RefineAdmission::Dropped(self.drop_with(RefineDrop::Backpressure));
        }
        self.next_job += 1;
        self.jobs.insert(self.next_job, (expanded, now));
        self.pending_bytes += expanded;
        self.stats.tiles_admitted += 1;
        self.stats.payload_bytes += u64::from(header.payload_len);
        RefineAdmission::Tile(TileTicket {
            job: self.next_job,
            monitor_id: header.scope.monitor_id,
            scope: header.scope,
            valid_from_fence: header.valid_from_fence,
            rect,
            depth: header.depth,
            expanded_bytes: expanded,
        })
    }

    fn expire(&mut self, now: Duration) {
        let before = self.jobs.len();
        let mut freed = 0;
        self.jobs.retain(|_, (bytes, at)| {
            let keep = now.saturating_sub(*at) < MAX_PENDING_AGE;
            if !keep {
                freed += *bytes;
            }
            keep
        });
        self.pending_bytes -= freed;
        self.stats.dropped += (before - self.jobs.len()) as u64;
    }

    fn release(&mut self, ticket: &TileTicket) -> bool {
        match self.jobs.remove(&ticket.job) {
            Some((bytes, _)) => {
                self.pending_bytes -= bytes;
                true
            }
            None => false,
        }
    }

    /// The worker could not decode a tile (malformed payload).
    pub fn abandon(
        &mut self,
        ticket: &TileTicket,
        error: RefineRecordError,
        now: Duration,
    ) -> RefineDrop {
        self.release(ticket);
        self.note_malformed(error, now)
    }

    /// Commits a decoded tile, **revalidating** it against the current
    /// state first: a generation change or an invalidation that arrived
    /// while it was being decompressed wins. Returns the number of blocks
    /// committed.
    ///
    /// # Errors
    ///
    /// The reason nothing was committed.
    pub fn commit(
        &mut self,
        ticket: &TileTicket,
        codes: &TileCodes,
        now: Duration,
    ) -> Result<usize, RefineDrop> {
        if !self.release(ticket) {
            return Err(self.drop_with(RefineDrop::Expired));
        }
        if self.disabled.is_some() {
            return Err(self.drop_with(RefineDrop::Disabled));
        }
        if codes.width() != ticket.rect.width || codes.height() != ticket.rect.height {
            return Err(self.note_malformed(RefineRecordError::Geometry, now));
        }
        let Some(state) = self.monitors.get_mut(&ticket.monitor_id) else {
            return Err(self.drop_with(RefineDrop::NoGeneration));
        };
        let Some(generation) = state
            .generation
            .filter(|generation| generation.scope == ticket.scope)
        else {
            return Err(self.drop_with(RefineDrop::StaleGeneration));
        };
        let mut committed = 0;
        let mut refused = 0;
        for (index, fingerprint) in tile_block_fingerprints(generation.grid, ticket.rect, codes) {
            let slot = index as usize;
            if state.invalidated_at[slot] > ticket.valid_from_fence
                || state.overlay[slot]
                    .as_ref()
                    .is_some_and(|block| block.fence >= ticket.valid_from_fence)
            {
                refused += 1;
                continue;
            }
            let Some(block) = generation.grid.block_rect(index) else {
                continue;
            };
            let planar = codes.sub_planes(
                block.x - ticket.rect.x,
                block.y - ticket.rect.y,
                block.width,
                block.height,
            );
            state.overlay[slot] = Some(OverlayBlock {
                fence: ticket.valid_from_fence,
                fingerprint,
                staged: Some(planar),
            });
            committed += 1;
        }
        self.stats.blocks_committed += committed as u64;
        self.stats.blocks_stale_at_commit += refused;
        self.evaluate(ticket.monitor_id, now);
        if committed == 0 {
            return Err(self.drop_with(RefineDrop::Stale));
        }
        Ok(committed)
    }

    fn on_digest(&mut self, digest: RefineDigest, now: Duration) {
        let monitor_id = digest.scope.monitor_id;
        let Some(state) = self.monitors.get_mut(&monitor_id) else {
            return;
        };
        let Some(generation) = state.generation else {
            return;
        };
        if digest.scope != generation.scope
            || digest.block_count != generation.grid.block_count()
            || state
                .digest
                .as_ref()
                .is_some_and(|pending| pending.record.fence > digest.fence)
        {
            return;
        }
        // A newer digest supersedes the pending one and any verdict.
        state.digest = Some(PendingDigest {
            record: digest,
            received_at: now,
        });
        self.evaluate(monitor_id, now);
    }

    /// The presenter showed the frame carrying `fence` for `monitor_id`.
    pub fn on_presented(&mut self, monitor_id: u16, fence: u64, now: Duration) {
        if let Some(state) = self.monitors.get_mut(&monitor_id)
            && fence > state.presented_fence
            && fence <= state.received_fence
        {
            state.presented_fence = fence;
        }
        self.evaluate(monitor_id, now);
    }

    /// Level 1: publish when the digest's fence has been presented and
    /// every covered block is committed and still valid.
    fn evaluate(&mut self, monitor_id: u16, now: Duration) {
        let Some(state) = self.monitors.get_mut(&monitor_id) else {
            return;
        };
        let (Some(generation), Some(pending)) = (state.generation, state.digest.clone()) else {
            return;
        };
        let record = &pending.record;
        if state.presented_fence < record.fence {
            return;
        }
        let mut fingerprints =
            Vec::with_capacity(usize::try_from(record.coverage.count()).unwrap_or(0));
        for index in record.coverage.indices() {
            match state.overlay.get(index as usize).and_then(Option::as_ref) {
                Some(block) if block.fence <= record.fence => {
                    fingerprints.push((index, block.fingerprint));
                }
                _ => {
                    if now.saturating_sub(pending.received_at) > DIGEST_COMPLETION_TIMEOUT {
                        state.digest = None;
                        self.resets.push(RefineControlMsg::RefineReset {
                            monitor_id,
                            stream_epoch: generation.scope.stream_epoch,
                            reason: "digest_incomplete".into(),
                        });
                    }
                    return;
                }
            }
        }
        let context = DigestContext {
            scope: generation.scope,
            fence: record.fence,
            contract: generation.contract,
            grid: generation.grid,
        };
        let matched = canonical_digest(&context, &fingerprints) == record.digest;
        let coverage = record.coverage.clone();
        let fence = record.fence;
        state.digest = None;
        if matched {
            self.stats.digest_matches += 1;
            state.verdict = Some(Verdict {
                level: VerificationLevel::SourceCodes,
                fence,
                coverage,
                block_count: generation.grid.block_count(),
                fingerprints,
            });
        } else {
            // A mismatch is a defect, not a state: drop the overlay and
            // start over.
            self.stats.digest_mismatches += 1;
            let epoch = generation.scope.stream_epoch;
            state.clear();
            self.resets.push(RefineControlMsg::RefineReset {
                monitor_id,
                stream_epoch: epoch,
                reason: "digest_mismatch".into(),
            });
        }
    }

    /// Overlay changes the presenter must encode, in its own command
    /// buffer, before the conversion pass of the frame carrying
    /// `presenting_fence`. Only blocks valid at that fence are set; at most
    /// [`MAX_STAGING_BYTES_PER_FRAME`] are staged per frame.
    pub fn take_overlay_delta(&mut self, monitor_id: u16, presenting_fence: u64) -> OverlayDelta {
        let Some(state) = self.monitors.get_mut(&monitor_id) else {
            return OverlayDelta::default();
        };
        let mut delta = OverlayDelta {
            clear_all: std::mem::take(&mut state.clear_all),
            ..OverlayDelta::default()
        };
        if delta.clear_all {
            state.on_gpu.iter_mut().for_each(|on| *on = false);
        }
        let Some(generation) = state.generation else {
            return delta;
        };
        let mut budget = MAX_STAGING_BYTES_PER_FRAME / STAGING_BYTES_PER_BLOCK;
        for index in 0..state.overlay.len() {
            let rect = generation
                .grid
                .block_rect(u32::try_from(index).unwrap_or(u32::MAX));
            let Some(rect) = rect else { continue };
            match state.overlay[index].as_mut() {
                None if state.on_gpu[index] => {
                    state.on_gpu[index] = false;
                    delta.clear.push(rect);
                }
                Some(block) if block.fence <= presenting_fence && budget > 0 => {
                    if let Some(planar) = block.staged.take() {
                        delta.set.push((rect, planar));
                        state.on_gpu[index] = true;
                        budget -= 1;
                    }
                }
                _ => {}
            }
        }
        delta
    }

    /// Level 2 / level 3: fingerprints of the covered blocks read back
    /// from the GPU overlay texture (level 2) or the rendered unscaled
    /// composite (level 3), after the command buffer that wrote them
    /// completed. Raises the verdict only if every covered block matches.
    pub fn on_readback(
        &mut self,
        monitor_id: u16,
        level: VerificationLevel,
        fence: u64,
        readback: &[(u32, u128)],
    ) -> bool {
        let Some(verdict) = self
            .monitors
            .get_mut(&monitor_id)
            .and_then(|state| state.verdict.as_mut())
        else {
            return false;
        };
        let needed = match level {
            VerificationLevel::SourceCodes => return false,
            VerificationLevel::GpuOverlay => VerificationLevel::SourceCodes,
            VerificationLevel::RenderedRgb => VerificationLevel::GpuOverlay,
        };
        if verdict.fence != fence
            || verdict.level < needed
            || readback != verdict.fingerprints.as_slice()
        {
            return false;
        }
        verdict.level = verdict.level.max(level);
        true
    }

    /// Covered blocks of the current verdict, for a readback.
    #[must_use]
    pub fn verdict_blocks(&self, monitor_id: u16) -> Option<(u64, Vec<TileRect>)> {
        let state = self.monitors.get(&monitor_id)?;
        let grid = state.generation?.grid;
        let verdict = state.verdict.as_ref()?;
        Some((
            verdict.fence,
            verdict
                .coverage
                .indices()
                .filter_map(|index| grid.block_rect(index))
                .collect(),
        ))
    }

    /// Status of one monitor, worded per ADR 0016 R4.
    #[must_use]
    pub fn status(&self, monitor_id: u16) -> MonitorRefineStatus {
        let off = |text: &str| MonitorRefineStatus {
            active: false,
            refined_permille: 0,
            verdict: None,
            text: text.to_string(),
        };
        if self.disabled.is_some() {
            return off("Refinement disabled (malformed records)");
        }
        let Some(state) = self.monitors.get(&monitor_id) else {
            return off("Refinement off");
        };
        if state.disabled {
            return off("Refinement off for this display");
        }
        let Some(generation) = state.generation else {
            return off("Refinement off");
        };
        let refined = permille(state.visible_blocks(), generation.grid.block_count());
        let text = match &state.verdict {
            Some(verdict) => format!(
                "Source codes verified (pre-display transforms); {}.{}% coverage; level {}",
                verdict.coverage_permille() / 10,
                verdict.coverage_permille() % 10,
                verdict.level as u8
            ),
            None => format!("Refining {}.{}%", refined / 10, refined % 10),
        };
        MonitorRefineStatus {
            active: true,
            refined_permille: refined,
            verdict: state.verdict.clone(),
            text,
        }
    }

    /// Per-block debug state for the lossless-tile view: for each block,
    /// `Some(true)` verified, `Some(false)` refined but unverified, `None`
    /// live (lossy video).
    #[must_use]
    pub fn block_map(&self, monitor_id: u16) -> Option<(RefineGrid, Vec<Option<bool>>)> {
        let state = self.monitors.get(&monitor_id)?;
        let grid = state.generation?.grid;
        let mut map: Vec<Option<bool>> = state
            .overlay
            .iter()
            .map(|block| {
                block
                    .as_ref()
                    .filter(|block| block.fence <= state.presented_fence)
                    .map(|_| false)
            })
            .collect();
        if let Some(verdict) = &state.verdict {
            for index in verdict.coverage.indices() {
                if let Some(slot) = map.get_mut(index as usize) {
                    *slot = Some(true);
                }
            }
        }
        Some((grid, map))
    }
}
