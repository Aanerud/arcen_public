// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! The Deck's refinement hub (ADR 0016): one shared handle that every Deck
//! part talks to, around the one state owner
//! ([`super::deck::DeckRefineSession`]) and the tile decode worker.
//!
//! - The **video lane reader** hands every `RefineBarrier`/`RefineFence`
//!   record to [`DeckRefineHub::on_video_record`] *before* the access unit
//!   behind it enters the frame queue, and tags that access unit with the
//!   fence it returns.
//! - The **refine lane reader** hands tiles and digests to
//!   [`DeckRefineHub::on_refine_record`]; tiles are decoded on the hub's own
//!   worker thread (never the video decode thread) and committed with
//!   revalidation.
//! - The **presenter** implements [`RefineCompositor`] and calls
//!   [`DeckRefineHub::compose`] for each frame it draws with a fence, then
//!   [`DeckRefineHub::presented`] once that frame is on screen and
//!   [`DeckRefineHub::complete_readback`] with what it read back.
//! - The **session** sends [`DeckRefineHub::take_control_messages`] to the
//!   Pier and shows [`DeckRefineHub::status`].

use super::deck::{
    DeckRefineSession, DeckRefineStats, MonitorRefineStatus, OverlayDelta, RefineAdmission,
    RefineDrop, TileTicket, VerificationLevel, VideoRecord,
};
use super::digest::block_fingerprint;
use super::tile::{RefineGrid, TileRect};
use arcen_protocol::refine::{LosslessRefineCapability, RefineControlMsg, RefineTileHeader};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Decode jobs queued for the worker at most (the state owner already
/// bounds admitted jobs to the same number).
pub const DECODE_QUEUE: usize = super::limits::MAX_PENDING_JOBS;

/// A readback the compositor should perform after the composite that shows
/// `blocks` for `fence` completed on the GPU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadbackRequest {
    /// Monitor.
    pub monitor_id: u16,
    /// [`VerificationLevel::GpuOverlay`] reads the overlay texture;
    /// [`VerificationLevel::RenderedRgb`] the rendered, unscaled composite.
    pub level: VerificationLevel,
    /// Fence of the verdict being raised.
    pub fence: u64,
    /// Covered blocks, ascending block index.
    pub blocks: Vec<TileRect>,
    /// Code depth (the composite's `code / (2^depth − 1)` mapping).
    pub depth: u8,
}

/// What a compositor read back: for each requested block, its planar codes
/// (`width × height` samples of plane 0, then 1, then 2), in request order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadbackResult {
    /// The request this answers.
    pub request: ReadbackRequest,
    /// Planar codes per block.
    pub blocks: Vec<Vec<u16>>,
}

/// The Deck presenter's side of refinement: paint exact tiles over the
/// presented frame, and read them back for verification levels 2 and 3.
///
/// Implementations order every overlay write before the composite that
/// samples it on the GPU (never a CPU write into a texture a command buffer
/// in flight may read) and composite in source pixels, before any scaling
/// or display transform (ADR 0016 R1, R2, R8).
pub trait RefineCompositor {
    /// Applies `delta` for a source of `grid` so the next composite shows
    /// it.
    fn apply_overlay(&mut self, grid: RefineGrid, delta: &OverlayDelta);
    /// Reads `request.blocks` back after the next composite completed and
    /// delivers them through [`DeckRefineHub::complete_readback`].
    /// A compositor that cannot read back ignores the request: the verdict
    /// then stays at level 1.
    fn request_readback(&mut self, request: ReadbackRequest);
}

#[cfg_attr(not(feature = "refine-zstd-decode"), allow(dead_code))]
struct Job {
    ticket: TileTicket,
    payload: Vec<u8>,
}

/// Frames whose fence is known but not yet presented, at most.
pub const MAX_TAGGED_FRAMES: usize = 128;

/// Called when something the user sees changed (a tile committed, a
/// verdict raised): the Deck schedules a repaint, which re-presents a still
/// frame with its new overlay.
pub type RefineWaker = Arc<dyn Fn() + Send + Sync>;

struct Shared {
    session: Mutex<DeckRefineSession>,
    started: Instant,
    /// The readback last requested, so a frame does not ask twice.
    pending_readback: Mutex<Option<(u16, VerificationLevel, u64)>>,
    /// `(timestamp_ms, monitor_id, fence)` of access units that followed a
    /// fence on the video lane, oldest first.
    tagged: Mutex<std::collections::VecDeque<(u32, u16, u64)>>,
    waker: Mutex<Option<RefineWaker>>,
    /// Records are processed only while this connection asked for
    /// refinement (and, after the hello, the Pier offered it).
    accepting: std::sync::atomic::AtomicBool,
}

impl Shared {
    fn wake(&self) {
        let waker = lock(&self.waker).clone();
        if let Some(waker) = waker {
            waker();
        }
    }
}

/// The Deck's refinement hub for one connection. Cheap to clone.
#[derive(Clone)]
pub struct DeckRefineHub {
    shared: Arc<Shared>,
    worker: Option<SyncSender<Job>>,
}

impl std::fmt::Debug for DeckRefineHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeckRefineHub")
            .field("worker", &self.worker.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for DeckRefineHub {
    fn default() -> Self {
        Self::new()
    }
}

impl DeckRefineHub {
    /// A hub with its decode worker (when this build has the decoder).
    #[must_use]
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            session: Mutex::new(Self::fresh_session()),
            started: Instant::now(),
            pending_readback: Mutex::new(None),
            tagged: Mutex::new(std::collections::VecDeque::new()),
            waker: Mutex::new(None),
            accepting: std::sync::atomic::AtomicBool::new(false),
        });
        let worker = spawn_worker(&shared);
        Self { shared, worker }
    }

    /// The capability a Deck advertises when its user switch is on; `None`
    /// when this build cannot decode tiles. Both domains: R′G′B′ tiles are
    /// written as they are, Y′CbCr tiles go through the presenter's own
    /// matrix ([`RefineCompositor`]).
    #[must_use]
    pub fn capability() -> Option<LosslessRefineCapability> {
        use arcen_protocol::refine::RefineDomain;
        cfg!(feature = "refine-zstd-decode").then(|| LosslessRefineCapability {
            domains: vec![
                RefineDomain::Rgb.token().to_owned(),
                RefineDomain::YCbCr.token().to_owned(),
            ],
            codecs: vec![arcen_protocol::refine::CODEC_DELTA_ZSTD1_TOKEN.to_owned()],
        })
    }

    fn now(&self) -> Duration {
        self.shared.started.elapsed()
    }

    fn session(&self) -> MutexGuard<'_, DeckRefineSession> {
        lock(&self.shared.session)
    }

    /// One presenter composites one picture: one monitor is refined.
    fn fresh_session() -> DeckRefineSession {
        DeckRefineSession::new().with_max_monitors(1)
    }

    /// Whether records are processed. Off until the session sets it: a
    /// connection that did not ask for refinement, or whose Pier did not
    /// offer it, ignores every refinement record it is sent.
    pub fn set_accepting(&self, accepting: bool) {
        self.shared
            .accepting
            .store(accepting, std::sync::atomic::Ordering::Release);
        if !accepting {
            *self.session() = Self::fresh_session();
            lock(&self.shared.tagged).clear();
            self.shared.wake();
        }
    }

    fn accepting(&self) -> bool {
        self.shared
            .accepting
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The source grid and code depth of `monitor_id`'s generation.
    #[must_use]
    pub fn grid(&self, monitor_id: u16) -> Option<(RefineGrid, u8)> {
        self.session().grid(monitor_id)
    }

    /// The colour contract of `monitor_id`'s generation (its barrier's).
    #[must_use]
    pub fn contract(&self, monitor_id: u16) -> Option<arcen_protocol::refine::RefineContractBytes> {
        self.session().contract(monitor_id)
    }

    /// Forgets everything: a new connection or a new hello.
    pub fn reset(&self) {
        *self.session() = Self::fresh_session();
        *lock(&self.shared.pending_readback) = None;
        lock(&self.shared.tagged).clear();
    }

    /// Sets the function that schedules a repaint.
    pub fn set_waker(&self, waker: RefineWaker) {
        *lock(&self.shared.waker) = Some(waker);
    }

    /// The access unit stamped `timestamp_ms` followed a fence on the video
    /// lane: remember it, so the presenter can name the frame's fence.
    pub fn note_frame(&self, timestamp_ms: u32, monitor_id: u16, fence: u64) {
        let mut tagged = lock(&self.shared.tagged);
        if tagged.len() >= MAX_TAGGED_FRAMES {
            tagged.pop_front();
        }
        tagged.push_back((timestamp_ms, monitor_id, fence));
    }

    /// The fence of the decoded frame stamped `timestamp_ms`, if one was
    /// noted; entries older than it are forgotten (their frames were
    /// dropped or coalesced). When two access units share a millisecond the
    /// **oldest** is taken: the presented fence can only lag, never lead, so
    /// a tile is never shown over a frame older than its content.
    #[must_use]
    pub fn frame_tag(&self, timestamp_ms: u32) -> Option<(u16, u64)> {
        let mut tagged = lock(&self.shared.tagged);
        let position = tagged.iter().position(|(at, ..)| *at == timestamp_ms)?;
        tagged.drain(..position);
        tagged
            .pop_front()
            .map(|(_, monitor, fence)| (monitor, fence))
    }

    /// A video-lane record, in lane order, before the access unit behind
    /// it enters the frame queue. Returns the fence that access unit
    /// carries, when the record was a fence.
    ///
    /// # Errors
    ///
    /// The drop reason for a malformed or out-of-order record (the state
    /// owner already cleared what it must and queued a `refine_reset`).
    pub fn on_video_record(&self, bytes: &[u8]) -> Result<Option<(u16, u64)>, RefineDrop> {
        if !self.accepting() {
            return Ok(None);
        }
        let now = self.now();
        let result = self.session().on_video_record(bytes, now);
        match result {
            Ok(VideoRecord::Fence { monitor_id, fence }) => Ok(Some((monitor_id, fence))),
            Ok(VideoRecord::Barrier { .. }) => {
                // The overlay is cleared: the picture changes.
                self.shared.wake();
                Ok(None)
            }
            Ok(VideoRecord::Ignored) => Ok(None),
            Err(drop) => {
                // A refused record may have queued a `refine_reset`.
                self.shared.wake();
                Err(drop)
            }
        }
    }

    /// A refine-lane record. A tile is admitted and queued for the worker.
    #[allow(clippy::must_use_candidate, reason = "the admission is informational")]
    pub fn on_refine_record(&self, bytes: &[u8]) -> RefineAdmission {
        if !self.accepting() {
            return RefineAdmission::Dropped(RefineDrop::Disabled);
        }
        let now = self.now();
        let admission = self.session().on_refine_record(bytes, now);
        if admission == RefineAdmission::Digest {
            // A digest may complete a verdict at once.
            self.shared.wake();
        }
        if let RefineAdmission::Tile(ticket) = admission {
            let payload = RefineTileHeader::decode(bytes)
                .map(|(_, payload)| payload.to_vec())
                .unwrap_or_default();
            let Some(worker) = self.worker.as_ref() else {
                self.session().forget(&ticket);
                return RefineAdmission::Dropped(RefineDrop::Backpressure);
            };
            match worker.try_send(Job { ticket, payload }) {
                Ok(()) => {}
                Err(TrySendError::Full(job) | TrySendError::Disconnected(job)) => {
                    self.session().forget(&job.ticket);
                    return RefineAdmission::Dropped(RefineDrop::Backpressure);
                }
            }
        }
        admission
    }

    /// Whether the decode backlog is at its bound; the refine lane reader
    /// waits while it is (see [`DeckRefineSession::backlog_full`]).
    #[must_use]
    pub fn backlog_full(&self) -> bool {
        self.session().backlog_full()
    }

    /// The presenter is about to draw the frame carrying `fence` for
    /// `monitor_id`: applies the overlay changes valid at that fence and
    /// asks for a due readback. Returns whether a generation is active (the
    /// compositor should composite with its overlay).
    pub fn compose(
        &self,
        compositor: &mut impl RefineCompositor,
        monitor_id: u16,
        fence: u64,
    ) -> bool {
        let (delta, grid, readback) = {
            let mut session = self.session();
            let delta = session.take_overlay_delta(monitor_id, fence);
            let grid = session.grid(monitor_id);
            let readback = session.readback_due(monitor_id);
            (delta, grid, readback)
        };
        let Some((grid, depth)) = grid else {
            if delta.clear_all {
                compositor.apply_overlay(
                    RefineGrid {
                        width: 0,
                        height: 0,
                    },
                    &delta,
                );
            }
            return false;
        };
        if !delta.is_empty() {
            compositor.apply_overlay(grid, &delta);
        }
        if let Some((level, verdict_fence, blocks)) = readback {
            let mut pending = lock(&self.shared.pending_readback);
            if *pending != Some((monitor_id, level, verdict_fence)) {
                *pending = Some((monitor_id, level, verdict_fence));
                drop(pending);
                compositor.request_readback(ReadbackRequest {
                    monitor_id,
                    level,
                    fence: verdict_fence,
                    blocks,
                    depth,
                });
            }
        }
        true
    }

    /// The frame carrying `fence` for `monitor_id` is on screen.
    pub fn presented(&self, monitor_id: u16, fence: u64) {
        let now = self.now();
        let before = self
            .session()
            .status(monitor_id)
            .verdict
            .map(|verdict| verdict.level);
        self.session().on_presented(monitor_id, fence, now);
        let after = self
            .session()
            .status(monitor_id)
            .verdict
            .map(|verdict| verdict.level);
        if before != after {
            self.shared.wake();
        }
    }

    /// A compositor's readback: fingerprints the blocks and raises the
    /// verdict if every one matches. Returns whether it did.
    #[allow(clippy::must_use_candidate, reason = "the outcome is informational")]
    pub fn complete_readback(&self, result: &ReadbackResult) -> bool {
        let request = &result.request;
        if result.blocks.len() != request.blocks.len() {
            return false;
        }
        let Some((grid, _)) = self.session().grid(request.monitor_id) else {
            return false;
        };
        let mut fingerprints = Vec::with_capacity(request.blocks.len());
        for (rect, planar) in request.blocks.iter().zip(&result.blocks) {
            let Some(index) = block_index(grid, *rect) else {
                return false;
            };
            if planar.len() != rect.pixels() * 3 {
                return false;
            }
            fingerprints.push((index, block_fingerprint(rect.width, rect.height, planar)));
        }
        let raised = self.session().on_readback(
            request.monitor_id,
            request.level,
            request.fence,
            &fingerprints,
        );
        if raised {
            self.shared.wake();
        } else {
            // Let a later frame try again.
            *lock(&self.shared.pending_readback) = None;
        }
        raised
    }

    /// Whether a still screen should re-present its last frame so overlay
    /// changes become visible without a new video frame.
    #[must_use]
    pub fn overlay_dirty(&self, monitor_id: u16) -> bool {
        self.session().overlay_dirty(monitor_id)
    }

    /// Enables or disables refinement for a monitor (the 10-bit layer fell
    /// back to 8 bits, or came back).
    pub fn set_monitor_enabled(&self, monitor_id: u16, enabled: bool) {
        self.session().set_monitor_enabled(monitor_id, enabled);
        self.shared.wake();
    }

    /// The decoder of `monitor_id` was reset or the connection resumed.
    pub fn decoder_reset(&self, monitor_id: u16, reason: &str) {
        self.session().decoder_reset(monitor_id, reason);
        self.shared.wake();
    }

    /// Control messages to send to the Pier.
    #[must_use]
    pub fn take_control_messages(&self) -> Vec<RefineControlMsg> {
        self.session().take_control_messages()
    }

    /// Status of one monitor.
    #[must_use]
    pub fn status(&self, monitor_id: u16) -> MonitorRefineStatus {
        self.session().status(monitor_id)
    }

    /// Counters.
    #[must_use]
    pub fn stats(&self) -> DeckRefineStats {
        self.session().stats()
    }

    /// Per-block debug state (see [`DeckRefineSession::block_map`]).
    #[must_use]
    pub fn block_map(&self, monitor_id: u16) -> Option<(RefineGrid, Vec<Option<bool>>)> {
        self.session().block_map(monitor_id)
    }

    /// Runs `f` against the state owner (tests and diagnostics).
    pub fn with_session<T>(&self, f: impl FnOnce(&mut DeckRefineSession) -> T) -> T {
        f(&mut self.session())
    }
}

fn block_index(grid: RefineGrid, rect: TileRect) -> Option<u32> {
    let blocks = grid.blocks_of(rect);
    match blocks.as_slice() {
        [index] if grid.block_rect(*index) == Some(rect) => Some(*index),
        _ => None,
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(feature = "refine-zstd-decode")]
fn spawn_worker(shared: &Arc<Shared>) -> Option<SyncSender<Job>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel::<Job>(DECODE_QUEUE);
    let weak = Arc::downgrade(shared);
    std::thread::Builder::new()
        .name("arcen-refine-decode".into())
        .spawn(move || run_worker(&receiver, &weak))
        .ok()?;
    Some(sender)
}

#[cfg(feature = "refine-zstd-decode")]
fn run_worker(receiver: &std::sync::mpsc::Receiver<Job>, shared: &std::sync::Weak<Shared>) {
    use super::zstd_decode::decode_tile;
    use arcen_protocol::refine::RefineRecordError;
    while let Ok(job) = receiver.recv() {
        let Some(shared) = shared.upgrade() else {
            return;
        };
        let ticket = job.ticket;
        let decoded = decode_tile(
            &job.payload,
            ticket.rect.width,
            ticket.rect.height,
            ticket.depth,
        );
        let now = shared.started.elapsed();
        let mut session = lock(&shared.session);
        let committed = if let Ok(codes) = decoded {
            session.commit(&ticket, &codes, now).is_ok()
        } else {
            let _ = session.abandon(&ticket, RefineRecordError::PayloadLength, now);
            false
        };
        drop(session);
        if committed {
            shared.wake();
        }
    }
}

#[cfg(not(feature = "refine-zstd-decode"))]
const fn spawn_worker(_shared: &Arc<Shared>) -> Option<SyncSender<Job>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_readback_must_name_whole_blocks_of_the_grid() {
        let grid = RefineGrid {
            width: 40,
            height: 20,
        };
        let block = grid.block_rect(5).unwrap();
        assert_eq!(block_index(grid, block), Some(5));
        let two = TileRect {
            x: 0,
            y: 0,
            width: 32,
            height: 16,
        };
        assert_eq!(block_index(grid, two), None);
    }

    #[test]
    fn a_frame_tag_takes_the_oldest_match_and_forgets_dropped_frames() {
        let hub = DeckRefineHub::new();
        hub.note_frame(100, 1, 7);
        hub.note_frame(116, 1, 8);
        hub.note_frame(116, 1, 9);
        hub.note_frame(133, 1, 10);
        // The 100 ms frame was coalesced away; 116 maps to the older fence.
        assert_eq!(hub.frame_tag(116), Some((1, 8)));
        assert_eq!(hub.frame_tag(116), Some((1, 9)));
        assert_eq!(hub.frame_tag(100), None);
        assert_eq!(hub.frame_tag(133), Some((1, 10)));
    }

    #[test]
    fn the_capability_follows_the_decoder() {
        assert_eq!(
            DeckRefineHub::capability().is_some(),
            cfg!(feature = "refine-zstd-decode")
        );
    }
}
