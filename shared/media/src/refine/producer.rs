// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Pier-side refinement producer for a CPU-resident packed-RGB10
//! source (Linux Xorg depth-30 `XShm`): an exact source mirror that yields
//! damage by comparison, the scheduler, tile extraction into immutable
//! snapshots for a compression worker, and the token bucket that paces
//! finished records. Codec-free: the caller compresses.

use super::budget::{MAX_BURST_BYTES, RefineBudget};
use super::digest::tile_block_fingerprints;
use super::pier::{PierMonitorRefine, TileJob};
use super::tile::{RefineGrid, TileCodes, TileRect, extract_packed_rgb10};
use super::{BLOCK, RefineContract};
use crate::video::{PackedRgb10Layout, Rgb10Signal};
use arcen_protocol::refine::{BarrierReason, CODEC_DELTA_ZSTD1, RefineTileHeader};
use std::collections::VecDeque;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Tiles handed to the worker and not yet finished, plus finished records
/// not yet written, at most.
pub const MAX_OUTSTANDING_TILES: usize = 16;

/// An exact CPU copy of the source, updated by compare-and-copy so the
/// damage it reports is exact per 16×16 block.
#[derive(Debug, Clone)]
pub struct SourceMirror {
    grid: RefineGrid,
    bytes: Vec<u8>,
    valid: bool,
}

impl SourceMirror {
    /// An empty mirror for `grid`.
    #[must_use]
    pub fn new(grid: RefineGrid) -> Self {
        Self {
            grid,
            bytes: vec![0; grid.width as usize * grid.height as usize * 4],
            valid: false,
        }
    }

    /// Bytes per mirror row.
    #[must_use]
    pub fn stride(&self) -> usize {
        self.grid.width as usize * 4
    }

    /// The mirror.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Copies a rectangle whose top-left pixel is `src[0]` (rows `stride`
    /// bytes apart) and returns the blocks whose bytes changed, ascending.
    /// The first observation marks every block it covers. A rectangle
    /// outside the source, or a source too short for it, observes nothing.
    pub fn observe_rect(&mut self, src: &[u8], stride: usize, rect: TileRect) -> Vec<usize> {
        let (x, y, width, height) = (
            rect.x as usize,
            rect.y as usize,
            rect.width as usize,
            rect.height as usize,
        );
        if width == 0
            || height == 0
            || x + width > self.grid.width as usize
            || y + height > self.grid.height as usize
            || stride < width * 4
            || src.len() < stride * (height - 1) + width * 4
        {
            return Vec::new();
        }
        let first = !self.valid;
        let cols = self.grid.cols() as usize;
        let mut dirty = vec![false; self.grid.block_count() as usize];
        let mirror_stride = self.stride();
        for row in 0..height {
            let source_row = &src[row * stride..row * stride + width * 4];
            let mirror_start = (y + row) * mirror_stride + x * 4;
            let block_row = (y + row) / BLOCK;
            let mut column = x;
            while column < x + width {
                let block_end = ((column / BLOCK) + 1) * BLOCK;
                let end = block_end.min(x + width);
                let from = (column - x) * 4;
                let to = (end - x) * 4;
                let mirror = &mut self.bytes[mirror_start + from..mirror_start + to];
                if first || mirror != &source_row[from..to] {
                    mirror.copy_from_slice(&source_row[from..to]);
                    dirty[block_row * cols + column / BLOCK] = true;
                }
                column = end;
            }
        }
        if first {
            self.valid = x == 0
                && y == 0
                && width == self.grid.width as usize
                && height == self.grid.height as usize;
        }
        dirty
            .iter()
            .enumerate()
            .filter_map(|(index, dirty)| dirty.then_some(index))
            .collect()
    }

    /// Observes a whole frame.
    pub fn observe_whole(&mut self, src: &[u8], stride: usize) -> Vec<usize> {
        let rect = TileRect {
            x: 0,
            y: 0,
            width: self.grid.width,
            height: self.grid.height,
        };
        self.observe_rect(src, stride, rect)
    }

    /// Whether a whole frame has been observed.
    #[must_use]
    pub const fn is_valid(&self) -> bool {
        self.valid
    }
}

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
    /// Its codes, copied out of the mirror.
    pub codes: TileCodes,
}

/// The refinement producer of one monitor.
#[derive(Debug)]
pub struct RefineProducer {
    scheduler: PierMonitorRefine,
    mirror: SourceMirror,
    layout: PackedRgb10Layout,
    signal: Rgb10Signal,
    depth: u8,
    budget: RefineBudget,
    in_worker: usize,
    ready: VecDeque<(TileJob, Vec<u8>)>,
    /// Finished records cancelled by motion (their blocks are rescheduled).
    pub dropped: u64,
    /// Records written.
    pub written: u64,
    /// Bytes written.
    pub written_bytes: u64,
}

impl RefineProducer {
    /// A producer at `rate_bps` (the session's refinement ceiling).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        scope: (u16, u64, u64),
        contract: RefineContract,
        grid: RefineGrid,
        layout: PackedRgb10Layout,
        signal: Rgb10Signal,
        still_ms: u32,
        rate_bps: u64,
        now: Duration,
    ) -> Self {
        let (monitor_id, topology_generation, stream_epoch) = scope;
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
            mirror: SourceMirror::new(grid),
            layout,
            signal,
            depth: contract.bits(),
            budget: RefineBudget::new(rate_bps, MAX_BURST_BYTES, now),
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

    /// A whole captured frame.
    pub fn observe_whole(&mut self, src: &[u8], stride: usize, now: Duration) {
        let dirty = self.mirror.observe_whole(src, stride);
        self.scheduler.on_capture(dirty, now);
    }

    /// A captured rectangle (`src[0]` is its top-left pixel).
    pub fn observe_rect(&mut self, src: &[u8], stride: usize, rect: TileRect, now: Duration) {
        let dirty = self.mirror.observe_rect(src, stride, rect);
        self.scheduler.on_capture(dirty, now);
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
        if !self.mirror.is_valid() || room == 0 || self.budget.is_suspended(now) {
            return Vec::new();
        }
        let grid = self.scheduler.grid();
        let stride = self.mirror.stride();
        let mut work = Vec::new();
        for job in self.scheduler.due_tiles(now, room) {
            match extract_packed_rgb10(
                self.mirror.bytes(),
                stride,
                grid,
                job.rect,
                self.layout,
                self.signal,
            ) {
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
            domain: arcen_protocol::refine::RefineDomain::Rgb,
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
    /// extraction: the mirror's compare-and-copy damage is exact, so
    /// re-reading the source could only return the same bytes, and the
    /// capture thread never re-fingerprints the screen (a keepalive or an
    /// idle caret costs nothing here).
    pub fn digest(&mut self, now: Duration) -> Option<Vec<u8>> {
        if !self.ready.is_empty() || self.in_worker > 0 {
            return None;
        }
        self.scheduler
            .digest_from_stored(now)
            .map(|digest| digest.encode())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mirror_reports_exact_block_damage() {
        let grid = RefineGrid {
            width: 40,
            height: 20,
        };
        let mut mirror = SourceMirror::new(grid);
        let mut frame = vec![7_u8; 40 * 20 * 4];
        assert_eq!(mirror.observe_whole(&frame, 160).len(), 6);
        assert!(mirror.observe_whole(&frame, 160).is_empty());
        // One pixel in block (2, 1).
        frame[(17 * 40 + 33) * 4] = 9;
        assert_eq!(mirror.observe_whole(&frame, 160), vec![5]);
        // A region read: only its blocks are compared, and a block the
        // region touches but did not change is not damaged.
        frame[(18 * 40 + 31) * 4] = 1;
        let rect = TileRect {
            x: 30,
            y: 16,
            width: 4,
            height: 4,
        };
        let src = frame[(16 * 40 + 30) * 4..].to_vec();
        assert_eq!(mirror.observe_rect(&src, 160, rect), vec![4]);
        assert_eq!(&mirror.bytes()[(18 * 40 + 31) * 4], &1);
    }
}
