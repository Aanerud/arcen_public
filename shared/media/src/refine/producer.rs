// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Pier-side refinement producer, generic over a [`RefineSource`]: the
//! scheduler, tile extraction into immutable snapshots for a compression
//! worker, and the token bucket that paces finished records. Codec-free:
//! the caller compresses. The only product-specific part is the source
//! (`super::source`): for the Linux Xorg depth-30 `XShm` capture it is
//! [`super::source::PackedRgb10Source`].

use super::RefineContract;
use super::budget::{MAX_BURST_BYTES, RefineBudget};
use super::digest::tile_block_fingerprints;
use super::pier::{PierMonitorRefine, TileJob};
use super::source::RefineSource;
use super::tile::{TileCodes, TileRect};
use arcen_protocol::refine::{BarrierReason, CODEC_DELTA_ZSTD1, RefineTileHeader};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub use super::source::SourceMirror;

/// Tiles handed to the worker and not yet finished, plus finished records
/// not yet written, at most.
pub const MAX_OUTSTANDING_TILES: usize = 16;

/// A first refinement generation unique to this process (and to each
/// producer in it), so a restarted producer that reuses the same
/// monitor/topology/epoch scope never announces a generation a Deck may
/// still hold (ADR 0016 R3). Mixed from the wall clock, the process id and
/// a per-process counter; not a secret.
#[must_use]
pub fn process_unique_generation() -> NonZeroU32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let clock = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            since.subsec_nanos()
                ^ u32::try_from(since.as_secs() & 0xFFFF_FFFF)
                    .unwrap_or(0)
                    .rotate_left(7)
        });
    let mixed = clock.wrapping_mul(0x9E37_79B9)
        ^ std::process::id().rotate_left(16)
        ^ COUNTER
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1)
            .wrapping_mul(0x85EB_CA6B);
    NonZeroU32::new(mixed).unwrap_or(NonZeroU32::MIN)
}

/// One tile for the compression worker: an immutable snapshot of its codes.
#[derive(Debug, Clone)]
pub struct TileWork {
    /// The job.
    pub job: TileJob,
    /// Its codes, copied out of the source.
    pub codes: TileCodes,
}

/// The refinement producer of one monitor.
#[derive(Debug)]
pub struct RefineProducer<S: RefineSource> {
    scheduler: PierMonitorRefine,
    source: S,
    depth: u8,
    budget: RefineBudget,
    /// Ceiling the operator configured; no rate ever exceeds it.
    max_rate_bps: u64,
    /// The Pier's link budget set the rate ([`Self::set_link_rate`]); the
    /// video-target floor no longer applies.
    link_rate: bool,
    /// Holds the link budget asked for.
    pub holds: u64,
    /// The Deck asked for no tiles (`refine_control`): fences continue.
    paused: bool,
    in_worker: usize,
    ready: VecDeque<(TileJob, Vec<u8>)>,
    /// Finished records cancelled by motion (their blocks are rescheduled).
    pub dropped: u64,
    /// Records written.
    pub written: u64,
    /// Bytes written.
    pub written_bytes: u64,
}

impl<S: RefineSource> RefineProducer<S> {
    /// A producer over `source` at `rate_bps` (the session's refinement
    /// ceiling). `contract` must describe the source's domain and depth.
    #[must_use]
    pub fn new(
        scope: (u16, u64, u64),
        contract: RefineContract,
        source: S,
        still_ms: u32,
        rate_bps: u64,
        now: Duration,
    ) -> Self {
        let (monitor_id, topology_generation, stream_epoch) = scope;
        let grid = source.grid();
        Self {
            scheduler: PierMonitorRefine::new(
                monitor_id,
                topology_generation,
                stream_epoch,
                contract,
                grid,
                still_ms,
            )
            .with_first_generation(process_unique_generation()),
            depth: source.depth(),
            source,
            budget: RefineBudget::new(rate_bps, MAX_BURST_BYTES, now),
            max_rate_bps: rate_bps,
            link_rate: false,
            holds: 0,
            paused: false,
            in_worker: 0,
            ready: VecDeque::new(),
            dropped: 0,
            written: 0,
            written_bytes: 0,
        }
    }

    /// The scheduler (fence, generation, motion state).
    #[must_use]
    pub const fn scheduler(&self) -> &PierMonitorRefine {
        &self.scheduler
    }

    /// A whole captured frame; returns the blocks it changed.
    pub fn observe_whole(&mut self, src: &[u8], stride: usize, now: Duration) -> Vec<usize> {
        let grid = self.source.grid();
        let rect = TileRect {
            x: 0,
            y: 0,
            width: grid.width,
            height: grid.height,
        };
        self.observe_rect(src, stride, rect, now)
    }

    /// A captured rectangle (`src[0]` is its top-left pixel); returns the
    /// blocks it changed.
    pub fn observe_rect(
        &mut self,
        src: &[u8],
        stride: usize,
        rect: TileRect,
        now: Duration,
    ) -> Vec<usize> {
        let dirty = self.source.observe(src, stride, rect);
        self.scheduler.on_capture(dirty.clone(), now);
        dirty
    }

    /// Damage known without the pixels (a capture API's dirty regions, for
    /// a capture whose pixels reach the source later): the blocks are
    /// invalidated with the next access unit like observed damage. Indices
    /// outside the grid are ignored; the list need not be sorted.
    pub fn observe_blocks(&mut self, mut blocks: Vec<usize>, now: Duration) {
        let count = self.source.grid().block_count() as usize;
        blocks.retain(|block| *block < count);
        blocks.sort_unstable();
        blocks.dedup();
        self.scheduler.on_capture(blocks, now);
    }

    /// The source, for a capture loop that updates it itself (a lagging
    /// readback marks it stale, then observes the frame it read back).
    pub fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// The floor before the Pier's link budget has a path sample: the
    /// session's current video bitrate target, never above the configured
    /// ceiling. Ignored once [`Self::set_link_rate`] has set the rate
    /// (ADR 0016, refinement-budget amendment).
    pub fn cap_rate(&mut self, bps: u64, now: Duration) {
        if !self.link_rate {
            self.budget.set_rate(bps.min(self.max_rate_bps), now);
        }
    }

    /// The rate the Pier's link budget allows from the link's residual
    /// (ADR 0016 R6), never above the configured ceiling. `hold` freezes
    /// the bucket and cancels finished records (their blocks are refined
    /// again later) until the next call without it.
    pub fn set_link_rate(&mut self, bps: u64, hold: bool, now: Duration) {
        self.link_rate = true;
        self.budget.set_rate(bps.min(self.max_rate_bps), now);
        if hold {
            if !self.budget.is_suspended(now) {
                self.holds += 1;
            }
            // Until the next budget window at the latest; a later call
            // without `hold` releases it at once.
            self.budget
                .hold_until(now, now.saturating_add(super::budget::RESUME_AFTER * 3));
            for (job, _) in std::mem::take(&mut self.ready) {
                self.scheduler.tile_dropped(&job);
                self.dropped += 1;
            }
        } else {
            self.budget.release(now);
        }
    }

    /// Whether the link budget set the rate (`false`: the video-target
    /// floor).
    #[must_use]
    pub const fn rate_from_link(&self) -> bool {
        self.link_rate
    }

    /// Current refinement rate.
    #[must_use]
    pub const fn rate_bps(&self) -> u64 {
        self.budget.rate_bps()
    }

    /// The Deck turned refinement off for this monitor (`false`) or on
    /// again (`true`). Pausing drops queued records; resuming starts a new
    /// generation, so nothing refined before the pause inherits validity.
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled != self.paused {
            return;
        }
        self.paused = !enabled;
        if enabled {
            self.reset(BarrierReason::Reenable);
        } else {
            for (job, _) in std::mem::take(&mut self.ready) {
                self.scheduler.tile_dropped(&job);
            }
        }
    }

    /// Whether the Deck paused refinement.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.paused
    }

    /// Records to write on the video lane immediately before the next
    /// access unit.
    pub fn before_access_unit(&mut self) -> Vec<Vec<u8>> {
        self.scheduler.before_access_unit()
    }

    /// Starts a new generation (recovery keyframe, reset request, …).
    pub fn reset(&mut self, reason: BarrierReason) {
        self.scheduler.reset(reason);
        self.ready.clear();
    }

    /// Tiles due now, as snapshots for the worker; nothing until a whole
    /// frame has been observed, and never more than
    /// [`MAX_OUTSTANDING_TILES`] outstanding.
    pub fn take_work(&mut self, now: Duration) -> Vec<TileWork> {
        let room = MAX_OUTSTANDING_TILES.saturating_sub(self.in_worker + self.ready.len());
        if self.paused || !self.source.ready() || room == 0 || self.budget.is_suspended(now) {
            return Vec::new();
        }
        let grid = self.scheduler.grid();
        let mut work = Vec::new();
        for job in self.scheduler.due_tiles(now, room) {
            match self.source.extract(job.rect) {
                Ok(codes) => {
                    self.scheduler
                        .tile_fingerprints(&job, &tile_block_fingerprints(grid, job.rect, &codes));
                    work.push(TileWork { job, codes });
                }
                Err(_) => self.scheduler.tile_dropped(&job),
            }
        }
        self.in_worker += work.len();
        work
    }

    /// The worker finished a tile: frame it for the refine lane. A tile of
    /// an older generation is discarded.
    pub fn finish(&mut self, job: &TileJob, payload: Option<&[u8]>) {
        self.in_worker = self.in_worker.saturating_sub(1);
        let Some(payload) = payload else {
            self.scheduler.tile_dropped(job);
            return;
        };
        if job.scope != self.scheduler.scope() {
            return;
        }
        let narrow = |value: u32| u16::try_from(value).unwrap_or(u16::MAX);
        let header = RefineTileHeader {
            scope: job.scope,
            valid_from_fence: job.valid_from_fence,
            domain: self.source.domain(),
            depth: self.depth,
            codec: CODEC_DELTA_ZSTD1,
            x: narrow(job.rect.x),
            y: narrow(job.rect.y),
            width: narrow(job.rect.width),
            height: narrow(job.rect.height),
            payload_len: u32::try_from(payload.len()).unwrap_or(u32::MAX),
        };
        self.ready.push_back((*job, header.encode_with(payload)));
    }

    /// Records the token bucket admits now, in order. Never blocks: what
    /// the bucket refuses waits for a later call. Motion cancels what is
    /// still queued: those blocks are scheduled again once still.
    pub fn drain_ready(&mut self, now: Duration) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        if self.scheduler.in_motion(now) {
            for (job, _) in std::mem::take(&mut self.ready) {
                self.scheduler.tile_dropped(&job);
                self.dropped += 1;
            }
            return out;
        }
        while let Some((_, record)) = self.ready.front() {
            let bytes = record.len() as u64;
            if !self.budget.try_take(bytes, now) {
                break;
            }
            // capenc's pipe has no acknowledgement; the Pier's lane queue
            // bounds what is in flight on the wire.
            self.budget.acknowledged(bytes);
            self.written += 1;
            self.written_bytes += bytes;
            if let Some((_, record)) = self.ready.pop_front() {
                out.push(record);
            }
        }
        out
    }

    /// A digest record, once the monitor has settled and the covered set
    /// changed since the last digest. Built from the fingerprints taken at
    /// extraction: a [`RefineSource`] reports exact damage, so
    /// re-reading the source could only return the same bytes, and the
    /// capture thread never re-fingerprints the screen (a keepalive or an
    /// idle caret costs nothing here).
    pub fn digest(&mut self, now: Duration) -> Option<Vec<u8>> {
        if self.paused || !self.ready.is_empty() || self.in_worker > 0 {
            return None;
        }
        self.scheduler
            .digest_from_stored(now)
            .map(|digest| digest.encode())
    }
}
