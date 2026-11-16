// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Per-client outbound frame queue — the async adapter around shared policy.
//!
//! The portable decisions live in [`arcen_media::video::SharedVideoQueue`]:
//! bounded buffering, drop-and-IDR recovery, generation barriers and counters.
//! This module owns only Tokio wakeups and the Linux `capenc` IDR side effect.
//! Lossless-refinement records that precede an access unit ride beside it
//! in the shared [`RefineVideoCarry`], so a dropped access unit never drops
//! its invalidations (ADR 0016 R2).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use arcen_media::refine::carry::RefineVideoCarry;
use arcen_media::video::{
    FrameClassification, RoomState, SharedVideoQueue, VideoQueuePush, VideoQueueWaitStats,
};
use tokio::sync::Notify;

use crate::logging::target;
use crate::media::capenc::IdrRequester;

/// `send_queue` maxsize. Raised from 4 to 8 to match the observed QUIC burst
/// depth (5-7 frames) at ~33ms RTT. IDR-on-drop still bounds recovery; the
/// larger queue prevents spurious IDR storms during CWND expansion.
pub const CAPACITY: usize = 8;

/// How long the frame pump waits for room before it lets an AU be dropped.
///
/// Waiting instead of dropping is backpressure: the pump stops taking AUs,
/// capenc's pipe fills, and capenc encodes fewer frames, each a valid P-frame
/// of the last one sent, so a brief stall costs frames, not the prediction
/// chain. At 60 fps the eight-frame queue is only 133 ms, and the lab link's
/// hiccups overflowed it: each loss meant an IDR, whose size caused the next
/// loss (27 IDRs in 50 s, 800 ms p50 frame age). Past this bound the link is
/// not hiccupping but gone, and drop-and-IDR recovery takes over as before.
pub const ROOM_WAIT: Duration = Duration::from_millis(500);

/// At most one IDR request per second while a drop streak persists
/// (`KEYFRAME_REQUEST_MIN_INTERVAL_S = 1.0`).
pub const KEYFRAME_REQUEST_MIN_INTERVAL: Duration = Duration::from_secs(1);

/// One queued wire frame and the sequence number its refinement records
/// are filed under.
struct SeqFrame {
    seq: u64,
    bytes: Vec<u8>,
}

/// Refinement records waiting for their access unit, and the next sequence.
#[derive(Default)]
struct RefineLedger {
    carry: RefineVideoCarry,
    next_seq: u64,
}

/// A frame to write on the video lane, preceded by the refinement records
/// that belong before it (empty unless the session refines).
pub struct DequeuedFrame {
    pub refine: Vec<Vec<u8>>,
    pub frame: Vec<u8>,
}

/// Bounded, drop-oldest, IDR-on-drop outbound queue for one client.
pub struct FrameQueue {
    inner: Mutex<SharedVideoQueue<SeqFrame>>,
    refine: Mutex<RefineLedger>,
    notify: Notify,
    /// Signalled whenever the writer takes a frame, for [`Self::wait_for_room`].
    room: Notify,
    idr: IdrRequester,
}

impl FrameQueue {
    pub fn new(idr: IdrRequester) -> Self {
        Self {
            inner: Mutex::new(SharedVideoQueue::new(
                CAPACITY,
                arcen_media::video::pipeline_contract(arcen_media::video::PipelineId::Auto)
                    .queue
                    .encoded_overflow
                    .keyframe_request_min_interval,
            )),
            refine: Mutex::new(RefineLedger::default()),
            notify: Notify::new(),
            room: Notify::new(),
            idr,
        }
    }

    /// Files `refine` under a new sequence number and wraps `data` with it.
    fn sequence(
        &self,
        data: Vec<u8>,
        refine: Vec<Vec<u8>>,
    ) -> (SeqFrame, std::sync::MutexGuard<'_, RefineLedger>) {
        let mut ledger = self.refine.lock().unwrap();
        ledger.next_seq += 1;
        let seq = ledger.next_seq;
        if !refine.is_empty() {
            ledger.carry.push(seq, refine);
        }
        (SeqFrame { seq, bytes: data }, ledger)
    }

    /// Bounds how long frames may wait for the writer before
    /// [`Self::wait_for_room`] holds the pump back, from the session's
    /// contract rate (`arcen_media::video::video_backlog_budget`), unless
    /// `ARCEN_VIDEO_BACKLOG=legacy`.
    pub fn apply_backlog_budget(&self, contract_fps: u32) {
        self.inner.lock().unwrap().set_backlog_budget(
            arcen_media::video::video_backlog_budget_from_env(contract_fps),
        );
    }

    /// Enqueue one wire frame (10-byte header + payload). Returns `false` if a
    /// frame had to be dropped to make room (for drop telemetry), `true`
    /// otherwise. Non-blocking.
    pub fn enqueue(&self, data: Vec<u8>, is_keyframe: bool) -> bool {
        self.enqueue_classified_at(data, is_keyframe, is_keyframe, Instant::now())
    }

    pub(crate) fn enqueue_classified(
        &self,
        data: Vec<u8>,
        is_keyframe: bool,
        is_recovery_point: bool,
        refine: Vec<Vec<u8>>,
    ) -> bool {
        self.enqueue_refined_at(data, is_keyframe, is_recovery_point, refine, Instant::now())
    }

    fn enqueue_classified_at(
        &self,
        data: Vec<u8>,
        is_keyframe: bool,
        is_recovery_point: bool,
        now: Instant,
    ) -> bool {
        self.enqueue_refined_at(data, is_keyframe, is_recovery_point, Vec::new(), now)
    }

    fn enqueue_refined_at(
        &self,
        data: Vec<u8>,
        is_keyframe: bool,
        is_recovery_point: bool,
        refine: Vec<Vec<u8>>,
        now: Instant,
    ) -> bool {
        let outcome = {
            // The ledger stays locked until the frame is queued, so a
            // writer never takes records filed for a frame not yet queued.
            let (frame, _ledger) = self.sequence(data, refine);
            let mut inner = self.inner.lock().unwrap();
            inner.push(
                frame,
                FrameClassification::new(is_keyframe, is_recovery_point),
                now,
            )
        };
        match outcome {
            VideoQueuePush::Enqueued { .. } => {
                self.notify.notify_one();
                true
            }
            VideoQueuePush::Dropped {
                idr_request,
                recovery_started,
                ..
            } => {
                if recovery_started {
                    tracing::warn!(
                        target: target::MEDIA,
                        drops = self.drops_since_keyframe(),
                        idr_request,
                        "send queue lost AU — cleared prediction chain, awaiting IDR"
                    );
                } else {
                    tracing::debug!(
                        target: target::MEDIA,
                        idr_request,
                        "send queue suppressed AU while awaiting keyframe"
                    );
                }
                if idr_request {
                    let delivered = self.idr.request();
                    self.note_keyframe_request_handoff(delivered, now);
                }
                self.notify.notify_one();
                false
            }
            VideoQueuePush::Closed(_) => false,
        }
    }

    /// Pin the exact self-contained recovery AU for a paused generation.
    ///
    /// Once pinned, later frames are dropped until activation so queue overflow
    /// cannot replace the SPS/PPS/IDR selected by the generation waiter. A
    /// second recovery point is requested for the post-activation prediction
    /// chain; P-frames remain suppressed until it is queued.
    pub fn pin_generation_recovery(&self, data: Vec<u8>, refine: Vec<Vec<u8>>) -> bool {
        let requested_at = Instant::now();
        let outcome = {
            let (frame, _ledger) = self.sequence(data, refine);
            let mut inner = self.inner.lock().unwrap();
            inner.pin_generation_recovery(frame, requested_at)
        };
        if !outcome.accepted {
            return false;
        }
        if outcome.idr_request && !self.idr.request() {
            self.note_keyframe_request_handoff(false, requested_at);
        } else if outcome.idr_request {
            self.note_keyframe_request_handoff(true, requested_at);
        }
        self.notify.notify_one();
        true
    }

    fn note_keyframe_request_handoff(&self, delivered: bool, now: Instant) {
        let mut inner = self.inner.lock().unwrap();
        if delivered {
            inner.note_keyframe_request_handoff(true, now);
        } else {
            tracing::warn!(
                target: target::MEDIA,
                "send queue recovery IDR could not be handed to capenc; stopping retries"
            );
            inner.abandon_keyframe_request();
        }
    }

    /// Await the next frame to send. Returns `None` once the queue is closed and
    /// drained (writer task should then exit).
    /// Refinement records are not taken: only paths that never refine
    /// (multi-monitor muxes, tests) use it.
    pub async fn dequeue(&self) -> Option<Vec<u8>> {
        self.dequeue_refined().await.map(|dequeued| dequeued.frame)
    }

    /// Await the next frame to send with the refinement records to write
    /// before it: its own and those of every frame dropped before it,
    /// merged (`RefineVideoCarry::take_through`).
    pub async fn dequeue_refined(&self) -> Option<DequeuedFrame> {
        loop {
            let notified = self.notify.notified();
            let retry_at = {
                let mut g = self.inner.lock().unwrap();
                if !g.is_paused() {
                    if let Some(item) = g.pop_front_with_bytes(|frame| frame.bytes.len()) {
                        drop(g);
                        self.room.notify_waiters();
                        let refine = self.refine.lock().unwrap().carry.take_through(item.seq);
                        return Some(DequeuedFrame {
                            refine,
                            frame: item.bytes,
                        });
                    }
                }
                if g.is_closed() {
                    return None;
                }
                let retry = g.keyframe_request_retry(Instant::now());
                if retry.due {
                    drop(g);
                    let delivered = self.idr.request();
                    self.note_keyframe_request_handoff(delivered, Instant::now());
                    continue;
                } else {
                    retry.retry_at
                }
            };
            match retry_at {
                Some(deadline) => {
                    tokio::select! {
                        () = notified => {}
                        () = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {}
                    }
                }
                None => notified.await,
            }
        }
    }

    /// Waits until an ordinary enqueue would not overflow, for at most
    /// `max_wait` (see [`ROOM_WAIT`]). Returns immediately when the queue is
    /// paused, recovering or closed: those states have their own rules.
    /// Returns whether there is room.
    pub async fn wait_for_room(&self, max_wait: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            let room = self.room.notified();
            match self.inner.lock().unwrap().room_state() {
                RoomState::NotOrdinary => return false,
                RoomState::HasRoom => return true,
                RoomState::Full => {}
            }
            if tokio::time::timeout_at(deadline, room).await.is_err() {
                return false;
            }
        }
    }

    /// Close the queue and wake the writer so it can exit.
    ///
    /// Any already-buffered frames are still handed to the writer first —
    /// [`dequeue`](Self::dequeue) drains before it observes `closed`.
    pub fn close(&self) {
        self.inner.lock().unwrap().close();
        self.notify.notify_one();
        self.room.notify_waiters();
    }

    /// Close the queue **and discard any buffered frames immediately**, so
    /// [`dequeue`](Self::dequeue) returns `None` right away rather than
    /// draining first.
    pub(crate) fn close_and_clear(&self) {
        self.inner.lock().unwrap().close_and_clear();
        self.refine.lock().unwrap().carry.clear();
        self.notify.notify_one();
        self.room.notify_waiters();
    }

    /// A frame is queued that the video lane's writer has not taken: motion
    /// is waiting, so refinement waits too (ADR 0016 R6).
    pub fn has_backlog(&self) -> bool {
        !self.inner.lock().unwrap().is_empty()
    }

    pub fn frames_sent(&self) -> u64 {
        self.inner.lock().unwrap().frames_sent()
    }

    pub fn frames_dropped(&self) -> u64 {
        self.inner.lock().unwrap().frames_dropped()
    }

    /// Aggregate wire bytes dequeued so far, for bandwidth-derived health
    /// telemetry.
    pub fn bytes_sent(&self) -> u64 {
        self.inner.lock().unwrap().bytes_sent()
    }

    /// The part of [`Self::bytes_sent`] that carried keyframes.
    pub fn keyframe_bytes_sent(&self) -> u64 {
        self.inner.lock().unwrap().keyframe_bytes_sent()
    }

    pub fn take_wait_stats(&self) -> VideoQueueWaitStats {
        self.inner.lock().unwrap().take_wait_stats()
    }

    fn drops_since_keyframe(&self) -> u64 {
        self.inner.lock().unwrap().drops_since_keyframe()
    }

    /// Begin a new media-plan generation without exposing queued frames from
    /// the prior geometry. The sender remains paused until
    /// [`activate_generation`](Self::activate_generation).
    pub fn begin_generation(&self) {
        self.inner.lock().unwrap().begin_generation();
    }

    pub fn activate_generation(&self) -> bool {
        let activated = self.inner.lock().unwrap().activate_generation();
        if activated {
            self.notify.notify_one();
        }
        activated
    }

    #[cfg(test)]
    pub(crate) fn awaiting_keyframe(&self) -> bool {
        self.inner.lock().unwrap().awaiting_keyframe()
    }

    pub(crate) fn requires_generation_recovery(&self) -> bool {
        self.inner.lock().unwrap().requires_generation_recovery()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::capenc::test_support::fake_idr;

    /// ADR 0016 R2: whatever the queue drops or suppresses, every
    /// invalidation reaches the Deck before (or with) the next access unit
    /// it does send, under increasing fences.
    #[tokio::test]
    async fn dropped_access_units_never_lose_their_refinement_invalidations() {
        use arcen_protocol::refine::{BlockRuns, RefineFence, RefineScope};
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        let scope = RefineScope {
            monitor_id: 1,
            topology_generation: 1,
            stream_epoch: 1,
            generation: std::num::NonZeroU32::new(5).unwrap(),
        };
        let fence = |number: u64, block: u32| {
            RefineFence {
                scope,
                fence: number,
                block_count: 1000,
                invalidated: BlockRuns::from_sorted_indices([block]),
            }
            .encode()
        };
        let frames = CAPACITY as u64 + 4;
        for index in 1..=frames {
            q.enqueue_classified(
                vec![index as u8],
                false,
                false,
                vec![fence(index, index as u32)],
            );
        }
        // A keyframe ends any suppression the drops started.
        q.enqueue_classified(vec![0xEE], true, true, vec![fence(frames + 1, 999)]);
        let mut seen = std::collections::BTreeSet::new();
        let mut last_fence = 0;
        loop {
            let dequeued = q.dequeue_refined().await.expect("frame");
            for record in &dequeued.refine {
                let fence = RefineFence::decode(record).expect("fence");
                assert!(fence.fence > last_fence, "fences increase");
                last_fence = fence.fence;
                seen.extend(fence.invalidated.indices());
            }
            if dequeued.frame == vec![0xEE] {
                break;
            }
        }
        let expected: std::collections::BTreeSet<u32> =
            (1..=frames as u32).chain(std::iter::once(999)).collect();
        assert_eq!(seen, expected);
        assert!(q.frames_dropped() > 0, "the test must have dropped frames");
    }

    #[tokio::test]
    async fn a_full_queue_makes_the_pump_wait_for_the_writer_instead_of_dropping() {
        let (idr, mut rx) = fake_idr();
        let q = std::sync::Arc::new(FrameQueue::new(idr));
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false));
        }
        let writer = {
            let q = std::sync::Arc::clone(&q);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                q.dequeue().await
            })
        };
        assert!(
            q.wait_for_room(Duration::from_secs(2)).await,
            "the writer made room"
        );
        assert!(
            q.enqueue(vec![0xAA], false),
            "the next P-frame fits: no loss"
        );
        assert_eq!(writer.await.unwrap(), Some(vec![0]));
        assert!(!q.awaiting_keyframe());
        assert_eq!(q.frames_dropped(), 0);
        assert!(
            rx.try_recv().is_err(),
            "no IDR for a stall the queue absorbed"
        );
    }

    #[tokio::test]
    async fn a_writer_that_never_drains_bounds_the_wait() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false));
        }
        let started = tokio::time::Instant::now();
        assert!(!q.wait_for_room(Duration::from_millis(60)).await);
        assert!(started.elapsed() >= Duration::from_millis(60));
        q.close();
        assert!(
            !q.wait_for_room(Duration::from_secs(5)).await,
            "closed returns at once"
        );
    }

    #[tokio::test]
    async fn close_and_clear_wakes_room_waiters_immediately() {
        let (idr, _rx) = fake_idr();
        let q = std::sync::Arc::new(FrameQueue::new(idr));
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false));
        }
        let waiter = {
            let q = std::sync::Arc::clone(&q);
            tokio::spawn(async move {
                let started = tokio::time::Instant::now();
                let has_room = q.wait_for_room(Duration::from_secs(5)).await;
                (has_room, started.elapsed())
            })
        };
        tokio::time::sleep(Duration::from_millis(25)).await;
        q.close_and_clear();
        let (has_room, elapsed) = waiter.await.unwrap();
        assert!(!has_room);
        assert!(
            elapsed < Duration::from_secs(1),
            "close_and_clear must not wait for the full room timeout: {elapsed:?}"
        );
    }

    #[tokio::test]
    async fn close_drains_then_ends() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.enqueue(vec![7], false);
        q.close();
        assert_eq!(q.dequeue().await.unwrap(), vec![7], "buffered frame drains");
        assert!(q.dequeue().await.is_none(), "then closed → None");
    }

    #[tokio::test]
    async fn throttled_recovery_retries_without_later_enqueue() {
        let (idr, mut rx) = fake_idr();
        let q = std::sync::Arc::new(FrameQueue::new(idr));
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false));
        }
        assert!(!q.enqueue(vec![0xAA], false));
        assert!(rx.try_recv().is_ok());
        assert!(!q.enqueue(vec![0xBB], false));

        let writer = {
            let q = std::sync::Arc::clone(&q);
            tokio::spawn(async move { q.dequeue().await })
        };
        tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("pending recovery should retry at its deadline")
            .expect("IDR request should be delivered");
        q.close_and_clear();
        assert!(writer.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn close_stops_pending_recovery_retry() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false));
        }
        assert!(!q.enqueue(vec![0xAA], false));
        assert!(rx.try_recv().is_ok());
        assert!(!q.enqueue(vec![0xBB], false));
        q.close_and_clear();
        assert!(q.dequeue().await.is_none());
    }

    #[tokio::test]
    async fn closed_idr_path_does_not_spin_and_close_still_finishes() {
        let (idr, rx) = fake_idr();
        drop(rx);
        let q = std::sync::Arc::new(FrameQueue::new(idr));
        for i in 0..CAPACITY {
            assert!(q.enqueue(vec![i as u8], false));
        }
        assert!(!q.enqueue(vec![0xAA], false));

        let dequeue = {
            let q = std::sync::Arc::clone(&q);
            tokio::spawn(async move { q.dequeue().await })
        };
        tokio::time::sleep(Duration::from_millis(75)).await;
        assert!(
            !dequeue.is_finished(),
            "closed IDR path must not busy-spin the dequeue future to completion"
        );
        q.close_and_clear();
        assert!(dequeue.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn close_and_clear_discards_buffered_frames_instead_of_draining() {
        let (idr, _rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.enqueue(vec![1], true);
        q.enqueue(vec![2], false);
        q.enqueue(vec![3], false);
        q.close_and_clear();
        assert!(
            q.dequeue().await.is_none(),
            "close_and_clear must discard buffered frames, not drain them"
        );
    }

    #[tokio::test]
    async fn close_and_clear_also_discards_pinned_generation_recovery_state() {
        let (idr, mut rx) = fake_idr();
        let q = FrameQueue::new(idr);
        q.begin_generation();
        assert!(q.pin_generation_recovery(vec![0xAA], Vec::new()));
        assert!(rx.try_recv().is_ok());
        assert!(
            q.activate_generation(),
            "unpauses; AA now sits in the deque"
        );
        q.close_and_clear();
        assert!(
            q.dequeue().await.is_none(),
            "an activated recovery frame must not survive close_and_clear either"
        );
    }
}
