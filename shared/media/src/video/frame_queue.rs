// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Transport-independent outbound video queue policy.
//!
//! This core owns the decisions that must match on every Pier: bounded video
//! buffering, prediction-chain recovery after loss, generation barriers for
//! topology changes, and keyframe-request throttling. Host adapters own only
//! wakeups, async waiting, and the platform-specific keyframe request side
//! effect.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Classification of one encoded access unit for queue recovery decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameClassification {
    /// This AU is a full keyframe/IDR for the normal prediction chain.
    pub keyframe: bool,
    /// This AU can start decoding after a generation barrier.
    pub recovery_point: bool,
}

impl FrameClassification {
    /// Classifies an AU where every keyframe is also the recovery point.
    #[must_use]
    pub const fn keyframe_is_recovery(keyframe: bool) -> Self {
        Self {
            keyframe,
            recovery_point: keyframe,
        }
    }

    /// Classifies an AU with explicit recovery-point evidence.
    #[must_use]
    pub const fn new(keyframe: bool, recovery_point: bool) -> Self {
        Self {
            keyframe,
            recovery_point,
        }
    }
}

/// What enqueuing one video AU decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoQueuePush<T> {
    /// The AU is now visible to the writer.
    Enqueued {
        /// Buffered AUs made obsolete by this AU but not counted as loss.
        cleared: usize,
    },
    /// The AU was suppressed or a queued prediction chain was discarded.
    Dropped {
        /// Number of AUs discarded by this decision, including the new AU when
        /// it was not enqueued.
        count: usize,
        /// Whether this decision started a new keyframe-recovery interval.
        recovery_started: bool,
        /// Whether the host adapter should request an IDR/keyframe now.
        idr_request: bool,
    },
    /// The queue is closed; the adapter gets the AU back.
    Closed(T),
}

impl<T> VideoQueuePush<T> {
    /// Whether the AU was accepted into the queue.
    #[must_use]
    pub const fn enqueued(&self) -> bool {
        matches!(self, Self::Enqueued { .. })
    }

    /// Whether the adapter should request a keyframe.
    #[must_use]
    pub const fn idr_request(&self) -> bool {
        matches!(
            self,
            Self::Dropped {
                idr_request: true,
                ..
            }
        )
    }
}

/// Result of pinning a generation recovery AU.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PinGenerationRecovery {
    /// Whether the recovery AU was accepted and pinned.
    pub accepted: bool,
    /// Buffered AUs discarded while pinning.
    pub dropped: usize,
    /// Whether the host adapter should request the follow-up IDR now.
    pub idr_request: bool,
    /// The timestamp recorded for that request, for rollback if the adapter's
    /// non-blocking keyframe request fails before reaching the encoder.
    pub requested_at: Option<Instant>,
}

/// Result of deciding whether a queued recovery needs a keyframe request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframeRequestRetry {
    /// Whether the adapter should try to hand the request to the encoder now.
    pub due: bool,
    /// Recovery remains pending until a handoff succeeds.
    pub pending: bool,
    /// When a pending request is throttled, the time at which it becomes due.
    pub retry_at: Option<Instant>,
}

/// Decision returned by [`FullFrameRequestCoalescer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullFrameRequestDecision {
    /// Deliver a keyframe request to every encoder now.
    pub deliver_now: bool,
    /// A client request arrived during the guard and is waiting.
    pub pending: bool,
    /// When the pending request should be delivered.
    pub deliver_at: Option<Instant>,
}

/// Coalesces explicit Deck `request_full_frame` messages without dropping them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FullFrameRequestCoalescer {
    min_interval: Duration,
    last_delivered_at: Option<Instant>,
    pending: bool,
}

impl FullFrameRequestCoalescer {
    #[must_use]
    pub const fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            last_delivered_at: None,
            pending: false,
        }
    }

    #[must_use]
    pub fn request(&mut self, now: Instant) -> FullFrameRequestDecision {
        if self.deliver_due_at(now).is_none() {
            self.last_delivered_at = Some(now);
            self.pending = false;
            FullFrameRequestDecision {
                deliver_now: true,
                pending: false,
                deliver_at: None,
            }
        } else {
            self.pending = true;
            self.decision(now)
        }
    }

    #[must_use]
    pub fn poll(&mut self, now: Instant) -> FullFrameRequestDecision {
        if self.pending && self.deliver_due_at(now).is_none() {
            self.last_delivered_at = Some(now);
            self.pending = false;
            FullFrameRequestDecision {
                deliver_now: true,
                pending: false,
                deliver_at: None,
            }
        } else {
            self.decision(now)
        }
    }

    #[must_use]
    pub const fn pending(&self) -> bool {
        self.pending
    }

    fn decision(&self, now: Instant) -> FullFrameRequestDecision {
        FullFrameRequestDecision {
            deliver_now: false,
            pending: self.pending,
            deliver_at: self.pending.then(|| self.deliver_due_at(now)).flatten(),
        }
    }

    fn deliver_due_at(&self, now: Instant) -> Option<Instant> {
        self.last_delivered_at.and_then(|last| {
            let due = last + self.min_interval;
            (now < due).then_some(due)
        })
    }
}

/// Wait-time statistics for frames handed from the shared queue to a host writer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VideoQueueWaitStats {
    /// Frames popped since the previous sample.
    pub frames: u64,
    /// Mean time a popped frame spent queued before the writer took it.
    pub mean: Duration,
    /// Longest queue wait observed in the sample.
    pub max: Duration,
}

impl VideoQueueWaitStats {
    /// Merges the samples of several per-monitor queues into one, weighting
    /// each mean by the frames it covers, so a multi-monitor session reports
    /// the whole session's wait rather than its primary monitor's alone.
    #[must_use]
    pub fn combine(samples: impl IntoIterator<Item = Self>) -> Self {
        let mut frames = 0u64;
        let mut total_nanos = 0u128;
        let mut max = Duration::ZERO;
        for sample in samples {
            frames = frames.saturating_add(sample.frames);
            total_nanos = total_nanos.saturating_add(
                sample
                    .mean
                    .as_nanos()
                    .saturating_mul(u128::from(sample.frames)),
            );
            max = max.max(sample.max);
        }
        let mean = if frames == 0 {
            Duration::ZERO
        } else {
            Duration::from_nanos(
                u64::try_from(total_nanos / u128::from(frames)).unwrap_or(u64::MAX),
            )
        };
        Self { frames, mean, max }
    }
}

/// A host writer's video writes: how long they waited on the transport, and
/// whether one is waiting now.
///
/// A writer that the path holds up for a whole rate interval sends no frame
/// and leaves its queue empty, so the queue alone cannot say that video
/// waits. [`Self::unsent`] is what a Pier reports as
/// [`crate::rate_control::RateHints::video_backlog`]: encoded video still
/// queued, or a write the transport has not accepted yet.
#[derive(Debug, Default)]
pub struct VideoWriteMeter {
    in_progress: AtomicUsize,
    waited_micros: AtomicU64,
}

impl VideoWriteMeter {
    /// Starts one video write; it is in progress until the guard drops,
    /// which adds its duration to [`Self::take_wait`].
    #[must_use = "the write counts as in progress only while the guard lives"]
    pub fn begin(&self) -> VideoWrite<'_> {
        self.in_progress.fetch_add(1, Ordering::Relaxed);
        VideoWrite {
            meter: self,
            started: Instant::now(),
        }
    }

    /// Whether a video write is waiting on the transport now.
    #[must_use]
    pub fn in_progress(&self) -> bool {
        self.in_progress.load(Ordering::Relaxed) != 0
    }

    /// Whether encoded video waits to be sent: `queued` (the host's own
    /// queues hold a frame) or a write in progress.
    #[must_use]
    pub fn unsent(&self, queued: bool) -> bool {
        queued || self.in_progress()
    }

    /// Time the finished writes spent writing since the previous call.
    pub fn take_wait(&self) -> Duration {
        Duration::from_micros(self.waited_micros.swap(0, Ordering::Relaxed))
    }
}

/// One video write in progress ([`VideoWriteMeter::begin`]).
#[derive(Debug)]
pub struct VideoWrite<'a> {
    meter: &'a VideoWriteMeter,
    started: Instant,
}

impl Drop for VideoWrite<'_> {
    fn drop(&mut self) {
        let micros = u64::try_from(self.started.elapsed().as_micros()).unwrap_or(u64::MAX);
        self.meter
            .waited_micros
            .fetch_add(micros, Ordering::Relaxed);
        self.meter.in_progress.fetch_sub(1, Ordering::Relaxed);
    }
}

#[derive(Debug)]
struct QueuedVideo<T> {
    item: T,
    enqueued_at: Instant,
    keyframe: bool,
    bytes: u64,
}

impl<T> QueuedVideo<T> {
    const fn new(item: T, enqueued_at: Instant, keyframe: bool, bytes: u64) -> Self {
        Self {
            item,
            enqueued_at,
            keyframe,
            bytes,
        }
    }
}

/// Frames that may always wait behind the writer, whatever their age: one
/// being written and one ready, so a writer that has not yet woken never
/// throttles the encoder.
pub const MIN_VIDEO_BACKLOG_FRAMES: usize = 2;

/// How long encoded video may wait for the host's writer before the encoder
/// side should stop handing over more pictures: two frame periods of the
/// contract rate, clamped to 20-100 ms.
///
/// When the network is slower than the encoder, every frame admitted behind
/// a full writer waits for all the frames ahead of it; with an eight-frame
/// queue at the 20-odd fps a capped link sustains, that is hundreds of
/// milliseconds of stale pictures. Refusing room once the oldest queued frame
/// is this old turns the backlog into backpressure instead: the producer
/// stops taking encoded frames, the encoder stalls, and the next picture it
/// captures is the current screen (latest frame wins) rather than one more
/// frame of a queue. Encoded frames are never dropped by this rule, so the
/// prediction chain is untouched.
#[must_use]
pub fn video_backlog_budget(contract_fps: u32) -> Duration {
    (Duration::from_secs(2) / contract_fps.max(1))
        .clamp(Duration::from_millis(20), Duration::from_millis(100))
}

/// Environment override read by [`video_backlog_budget_from_env`]:
/// `ARCEN_VIDEO_BACKLOG=legacy` restores the capacity-only queue.
pub const VIDEO_BACKLOG_ENV: &str = "ARCEN_VIDEO_BACKLOG";

/// [`video_backlog_budget`] unless the operator set
/// [`VIDEO_BACKLOG_ENV`] to `legacy`.
#[must_use]
pub fn video_backlog_budget_from_env(contract_fps: u32) -> Option<Duration> {
    (!video_backlog_is_legacy_from_env()).then(|| video_backlog_budget(contract_fps))
}

/// Whether the operator restored the capacity-only queue with
/// `ARCEN_VIDEO_BACKLOG=legacy`.
#[must_use]
pub fn video_backlog_is_legacy_from_env() -> bool {
    std::env::var(VIDEO_BACKLOG_ENV).is_ok_and(|value| value.trim().eq_ignore_ascii_case("legacy"))
}

/// How long a frame producer waits for room in a full send queue before it
/// lets the queue's drop-and-keyframe recovery take over: past this the link
/// is not slow but gone.
pub const VIDEO_ROOM_WAIT: Duration = Duration::from_millis(500);

/// Bytes a send queue holding canvas records (ADR 0028) keeps before it
/// refuses room: one record of the largest size. With the record bound
/// (the queue's capacity) it caps what a slow path makes the Pier hold.
pub const CANVAS_QUEUE_BYTES: u64 = arcen_protocol::canvas::MAX_CANVAS_FRAME_BYTES as u64;

/// What one outbound video record is, for its producer's backpressure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutboundVideoRecord {
    /// An encoded access unit; a keyframe (`recovery_point`) supersedes
    /// what queued before it.
    AccessUnit {
        /// A keyframe or other recovery point.
        recovery_point: bool,
    },
    /// A canvas frame: ordered state the Deck builds on, never superseded.
    CanvasFrame,
    /// A canvas reset: the generation's barrier, which supersedes every
    /// record queued before it.
    CanvasReset,
}

/// How a producer waits for room before it hands a record over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomWait {
    /// Never: the record is a recovery point and supersedes the queue.
    Never,
    /// At most this long; then the queue's own rule (drop and recover) takes
    /// over.
    UpTo(Duration),
    /// Until there is room or the queue closes.
    UntilRoomOrClosed,
}

impl RoomWait {
    /// The wait as an optional bound: `None` waits until room or close.
    /// Only meaningful for a wait other than [`Self::Never`].
    #[must_use]
    pub const fn max_wait(self) -> Option<Duration> {
        match self {
            Self::Never => Some(Duration::ZERO),
            Self::UpTo(wait) => Some(wait),
            Self::UntilRoomOrClosed => None,
        }
    }
}

impl OutboundVideoRecord {
    /// The record a capenc canvas record (`reset` from its prefix) or access
    /// unit is.
    #[must_use]
    pub const fn new(canvas: bool, recovery_point: bool) -> Self {
        match (canvas, recovery_point) {
            (true, true) => Self::CanvasReset,
            (true, false) => Self::CanvasFrame,
            (false, recovery_point) => Self::AccessUnit { recovery_point },
        }
    }

    /// How its producer waits for room. An access unit waits at most
    /// [`VIDEO_ROOM_WAIT`]: past it the link is gone and dropping the
    /// prediction chain for a keyframe is the recovery. A canvas frame
    /// waits until there is room or the queue closes: dropping one costs a
    /// new generation (a full repaint, larger than everything a short stall
    /// held back) and waiting loses nothing, because the encode loop merges
    /// the damage it could not send into its next frame. A recovery point
    /// never waits.
    #[must_use]
    pub const fn room_wait(self) -> RoomWait {
        match self {
            Self::AccessUnit {
                recovery_point: true,
            }
            | Self::CanvasReset => RoomWait::Never,
            Self::AccessUnit {
                recovery_point: false,
            } => RoomWait::UpTo(VIDEO_ROOM_WAIT),
            Self::CanvasFrame => RoomWait::UntilRoomOrClosed,
        }
    }
}

/// Encoded frames a split Pier's desktop agent may have handed to its
/// network relay that the relay has not yet released into the video lane.
///
/// A single-process Pier's writer takes a frame from the shared queue and
/// then waits for room in the video lane, so its backlog budget sees every
/// frame the network has not taken. A split Pier (the Windows broker and
/// agent, ADR 0014) has a local socket and the relay's own backlog between
/// the agent's queue and the lane; without this window those frames wait
/// where no budget sees them. With one frame outstanding the relay always
/// has the next frame ready when the lane takes one, and every older frame
/// waits in the agent's shared queue, under its budget.
pub const RELAYED_VIDEO_FRAMES_IN_FLIGHT: u64 = 1;

/// How long a blocked [`RelayedVideoWindow`] waits without the relay
/// releasing anything before it lets one more frame through. Reports cross a
/// reliable local socket, so a quiet relay means a slow lane (a large
/// keyframe at a low bitrate, a QUIC stall), not lost reports: the window
/// keeps counting and only paces one frame per period past the limit.
pub const RELAYED_VIDEO_RESYNC: Duration = Duration::from_secs(1);

/// The agent-side half of a split Pier's frames-in-flight limit: the video
/// frames this attachment handed to its relay, against the relay's running
/// count of frames it released (sent into the lane, or dropped by its own
/// policy). Both counts start with the attachment and are never reset, so
/// what is in flight is always `sent - released`.
///
/// It stays open until the first release report arrives, so a relay that
/// never reports (an older service, a transport without lanes, or
/// `ARCEN_VIDEO_BACKLOG=legacy` on the service) keeps the previous
/// behaviour. Once a report arrived the limit stays on.
#[derive(Debug, Clone)]
pub struct RelayedVideoWindow {
    limit: u64,
    sent: u64,
    released: u64,
    active: bool,
    blocked_since: Option<Instant>,
    resyncs: u64,
}

impl Default for RelayedVideoWindow {
    fn default() -> Self {
        Self::new(RELAYED_VIDEO_FRAMES_IN_FLIGHT)
    }
}

impl RelayedVideoWindow {
    /// A window allowing `limit` frames (at least one) in flight.
    #[must_use]
    pub const fn new(limit: u64) -> Self {
        Self {
            limit: if limit == 0 { 1 } else { limit },
            sent: 0,
            released: 0,
            active: false,
            blocked_since: None,
            resyncs: 0,
        }
    }

    /// Whether the writer may hand the relay another frame at `now`: below
    /// the limit, or blocked for [`RELAYED_VIDEO_RESYNC`] without the relay
    /// releasing anything (one frame; [`Self::note_sent_at`] restarts the
    /// period).
    pub fn admits(&mut self, now: Instant) -> bool {
        if !self.active || self.outstanding() < self.limit {
            self.blocked_since = None;
            return true;
        }
        let since = *self.blocked_since.get_or_insert(now);
        now.saturating_duration_since(since) >= RELAYED_VIDEO_RESYNC
    }

    /// When a blocked window lets the next frame through if the relay
    /// releases nothing first.
    #[must_use]
    pub fn resync_at(&self) -> Option<Instant> {
        self.blocked_since.map(|since| since + RELAYED_VIDEO_RESYNC)
    }

    /// The writer handed the relay one video frame at `now`. Counted whether
    /// or not the window is active yet.
    pub fn note_sent_at(&mut self, now: Instant) {
        if self.active && self.outstanding() >= self.limit {
            // The stall allowance was used: the next one is a period away.
            self.blocked_since = Some(now);
            self.resyncs = self.resyncs.saturating_add(1);
        }
        self.sent = self.sent.saturating_add(1);
    }

    /// The relay has released `released` video frames of this attachment in
    /// total. The first report activates the window; a count that does not
    /// grow changes nothing.
    pub fn note_released_total(&mut self, released: u64) {
        self.active = true;
        if released > self.released {
            self.released = released;
            self.blocked_since = None;
        }
    }

    /// Whether a release report has been seen.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        self.active
    }

    /// Frames handed to the relay and not yet released by it.
    #[must_use]
    pub const fn outstanding(&self) -> u64 {
        self.sent.saturating_sub(self.released)
    }

    /// Frames let through past the limit because the relay released nothing
    /// for [`RELAYED_VIDEO_RESYNC`].
    #[must_use]
    pub const fn resyncs(&self) -> u64 {
        self.resyncs
    }
}

/// Whether an async adapter should keep waiting for writer room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomState {
    /// A normal P-frame enqueue would not follow the ordinary path, so waiting
    /// for capacity is not useful.
    NotOrdinary,
    /// A normal P-frame can be enqueued immediately.
    HasRoom,
    /// The ordinary queue is full; wait for a pop or timeout.
    Full,
}

#[allow(clippy::struct_excessive_bools)]
#[derive(Debug)]
pub struct SharedVideoQueue<T> {
    capacity: usize,
    keyframe_request_min_interval: Duration,
    deque: VecDeque<QueuedVideo<T>>,
    generation_recovery: Option<QueuedVideo<T>>,
    generation_chain: bool,
    require_generation_recovery: bool,
    protected_front: bool,
    awaiting_keyframe: bool,
    keyframe_request_pending: bool,
    drops_since_keyframe: u64,
    last_keyframe_request_at: Option<Instant>,
    paused: bool,
    closed: bool,
    frames_sent: u64,
    frames_dropped: u64,
    bytes_sent: u64,
    keyframe_bytes_sent: u64,
    wait_total: Duration,
    wait_max: Duration,
    wait_frames: u64,
    backlog_budget: Option<Duration>,
    byte_budget: Option<u64>,
}

impl<T> SharedVideoQueue<T> {
    /// Builds an empty queue.
    ///
    /// # Panics
    ///
    /// Panics when `capacity` is zero.
    #[must_use]
    pub fn new(capacity: usize, keyframe_request_min_interval: Duration) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            keyframe_request_min_interval,
            deque: VecDeque::with_capacity(capacity),
            generation_recovery: None,
            generation_chain: false,
            require_generation_recovery: false,
            protected_front: false,
            awaiting_keyframe: false,
            keyframe_request_pending: false,
            drops_since_keyframe: 0,
            last_keyframe_request_at: None,
            paused: false,
            closed: false,
            frames_sent: 0,
            frames_dropped: 0,
            bytes_sent: 0,
            keyframe_bytes_sent: 0,
            wait_total: Duration::ZERO,
            wait_max: Duration::ZERO,
            wait_frames: 0,
            backlog_budget: None,
            byte_budget: None,
        }
    }

    /// Bounds how long ordinary frames may wait for the writer before
    /// [`Self::room_state_at`] reports [`RoomState::Full`]; see
    /// [`video_backlog_budget`]. `None` keeps the capacity-only rule.
    pub fn set_backlog_budget(&mut self, budget: Option<Duration>) {
        self.backlog_budget = budget;
    }

    /// Bounds the bytes queued records may hold before
    /// [`Self::room_state_at`] reports [`RoomState::Full`] (see
    /// [`CANVAS_QUEUE_BYTES`]); one record of any size is always taken.
    /// `None` keeps the record bound alone. Only sizes given to
    /// [`Self::push_sized`] count.
    pub fn set_byte_budget(&mut self, budget: Option<u64>) {
        self.byte_budget = budget;
    }

    /// Enqueues one AU or suppresses it according to the shared policy.
    pub fn push(
        &mut self,
        item: T,
        classification: FrameClassification,
        now: Instant,
    ) -> VideoQueuePush<T> {
        self.push_sized(item, classification, 0, now)
    }

    /// [`Self::push`] for a record of `bytes` bytes, counted against the
    /// byte budget while it waits.
    #[allow(clippy::too_many_lines)]
    pub fn push_sized(
        &mut self,
        item: T,
        classification: FrameClassification,
        bytes: u64,
        now: Instant,
    ) -> VideoQueuePush<T> {
        if self.closed {
            return VideoQueuePush::Closed(item);
        }
        let keyframe = classification.keyframe || classification.recovery_point;
        if self.generation_chain && self.require_generation_recovery {
            if classification.recovery_point && self.deque.len() < self.capacity {
                self.deque
                    .push_back(QueuedVideo::new(item, now, keyframe, bytes));
                self.require_generation_recovery = false;
                self.awaiting_keyframe = false;
                self.keyframe_request_pending = false;
                self.drops_since_keyframe = 0;
                self.last_keyframe_request_at = None;
                if !self.paused {
                    self.generation_chain = false;
                }
                VideoQueuePush::Enqueued { cleared: 0 }
            } else {
                self.record_drop(1);
                let idr_request = self.keyframe_request_due(now);
                VideoQueuePush::Dropped {
                    count: 1,
                    recovery_started: false,
                    idr_request,
                }
            }
        } else if self.paused && self.generation_chain {
            let generation_capacity = self
                .capacity
                .saturating_sub(usize::from(self.generation_recovery.is_some()));
            if self.deque.len() < generation_capacity {
                self.deque
                    .push_back(QueuedVideo::new(item, now, keyframe, bytes));
                VideoQueuePush::Enqueued { cleared: 0 }
            } else {
                let count = self.deque.len() + 1;
                self.deque.clear();
                self.require_generation_recovery = true;
                self.awaiting_keyframe = true;
                self.keyframe_request_pending = true;
                self.record_drop(count);
                let idr_request = self.keyframe_request_due(now);
                VideoQueuePush::Dropped {
                    count,
                    recovery_started: true,
                    idr_request,
                }
            }
        } else if self.paused && self.generation_recovery.is_some() {
            self.record_drop(1);
            VideoQueuePush::Dropped {
                count: 1,
                recovery_started: false,
                idr_request: false,
            }
        } else if self.protected_front {
            if self.deque.len() < self.capacity {
                self.deque
                    .push_back(QueuedVideo::new(item, now, keyframe, bytes));
                VideoQueuePush::Enqueued { cleared: 0 }
            } else {
                let pinned = self.deque.pop_front();
                let count = self.deque.len() + 1;
                self.deque.clear();
                if let Some(pinned) = pinned {
                    self.deque.push_back(pinned);
                }
                self.generation_chain = true;
                self.require_generation_recovery = !classification.recovery_point;
                self.awaiting_keyframe = !classification.recovery_point;
                self.keyframe_request_pending = !classification.recovery_point;
                if classification.recovery_point {
                    self.deque
                        .push_back(QueuedVideo::new(item, now, keyframe, bytes));
                    self.drops_since_keyframe = 0;
                    self.keyframe_request_pending = false;
                    self.last_keyframe_request_at = None;
                    self.frames_dropped = self.frames_dropped.saturating_add(count as u64);
                    VideoQueuePush::Enqueued { cleared: count }
                } else {
                    self.record_drop(count);
                    let idr_request = self.keyframe_request_due(now);
                    VideoQueuePush::Dropped {
                        count,
                        recovery_started: true,
                        idr_request,
                    }
                }
            }
        } else if classification.keyframe {
            let cleared = self.deque.len();
            self.deque.clear();
            self.deque
                .push_back(QueuedVideo::new(item, now, keyframe, bytes));
            self.awaiting_keyframe = false;
            self.keyframe_request_pending = false;
            self.drops_since_keyframe = 0;
            self.last_keyframe_request_at = None;
            VideoQueuePush::Enqueued { cleared }
        } else if self.awaiting_keyframe {
            self.keyframe_request_pending = true;
            self.record_drop(1);
            let idr_request = self.keyframe_request_due(now);
            VideoQueuePush::Dropped {
                count: 1,
                recovery_started: false,
                idr_request,
            }
        } else if self.deque.len() < self.capacity {
            self.deque
                .push_back(QueuedVideo::new(item, now, keyframe, bytes));
            VideoQueuePush::Enqueued { cleared: 0 }
        } else {
            let count = self.deque.len() + 1;
            self.deque.clear();
            self.awaiting_keyframe = true;
            self.keyframe_request_pending = true;
            self.record_drop(count);
            let idr_request = self.keyframe_request_due(now);
            VideoQueuePush::Dropped {
                count,
                recovery_started: true,
                idr_request,
            }
        }
    }

    /// Pins the exact self-contained AU that must be sent after a generation
    /// control message is delivered.
    pub fn pin_generation_recovery(&mut self, item: T, now: Instant) -> PinGenerationRecovery {
        if self.closed || !self.paused || self.generation_recovery.is_some() {
            return PinGenerationRecovery {
                accepted: false,
                dropped: 0,
                idr_request: false,
                requested_at: None,
            };
        }
        let dropped = self.deque.len();
        self.deque.clear();
        self.generation_recovery = Some(QueuedVideo::new(item, now, true, 0));
        self.generation_chain = true;
        self.require_generation_recovery = true;
        self.awaiting_keyframe = true;
        self.keyframe_request_pending = true;
        self.drops_since_keyframe = 0;
        self.frames_dropped = self.frames_dropped.saturating_add(dropped as u64);
        PinGenerationRecovery {
            accepted: true,
            dropped,
            idr_request: true,
            requested_at: None,
        }
    }

    /// Rolls back a pinned-generation keyframe request timestamp when the host
    /// adapter failed to hand the request to the encoder at all.
    pub fn clear_keyframe_request_if_at(&mut self, requested_at: Instant) {
        if self.last_keyframe_request_at == Some(requested_at) {
            self.last_keyframe_request_at = None;
            self.keyframe_request_pending = true;
        }
    }

    /// Records whether a host adapter durably handed a pending recovery
    /// keyframe request to the encoder.
    pub fn note_keyframe_request_handoff(&mut self, success: bool, now: Instant) {
        if success {
            self.keyframe_request_pending = false;
            self.last_keyframe_request_at = Some(now);
        }
    }

    /// Stops retrying a pending recovery keyframe request when the adapter can
    /// prove the encoder request path is permanently gone.
    pub fn abandon_keyframe_request(&mut self) {
        self.keyframe_request_pending = false;
    }

    /// Returns whether a pending recovery keyframe request should be retried.
    #[must_use]
    pub fn keyframe_request_retry(&self, now: Instant) -> KeyframeRequestRetry {
        KeyframeRequestRetry {
            due: self.keyframe_request_due_at(now),
            pending: self.keyframe_request_pending,
            retry_at: self.keyframe_request_retry_at(now),
        }
    }

    /// Pops the next item and accounts for its byte size.
    pub fn pop_front_with_bytes(&mut self, bytes: impl FnOnce(&T) -> usize) -> Option<T> {
        if self.paused {
            return None;
        }
        let queued = self.deque.pop_front()?;
        if self.protected_front {
            self.protected_front = false;
        }
        let wait = Instant::now().saturating_duration_since(queued.enqueued_at);
        self.wait_total = self.wait_total.saturating_add(wait);
        self.wait_max = self.wait_max.max(wait);
        self.wait_frames = self.wait_frames.saturating_add(1);
        self.frames_sent = self.frames_sent.saturating_add(1);
        let size = bytes(&queued.item) as u64;
        self.bytes_sent = self.bytes_sent.saturating_add(size);
        if queued.keyframe {
            self.keyframe_bytes_sent = self.keyframe_bytes_sent.saturating_add(size);
        }
        Some(queued.item)
    }

    /// Pops the next item without adding byte accounting.
    pub fn pop_front(&mut self) -> Option<T> {
        self.pop_front_with_bytes(|_| 0)
    }

    /// Returns whether the writer should wait for ordinary queue capacity.
    #[must_use]
    pub fn room_state(&self) -> RoomState {
        self.room_state_at(Instant::now())
    }

    /// [`Self::room_state`] at a caller-injected time: full at capacity,
    /// with a byte budget once the queued records hold it, or, with a
    /// backlog budget, once at least [`MIN_VIDEO_BACKLOG_FRAMES`] wait and
    /// the oldest has waited the whole budget.
    #[must_use]
    pub fn room_state_at(&self, now: Instant) -> RoomState {
        let ordinary = !self.closed
            && !self.paused
            && !self.awaiting_keyframe
            && !self.generation_chain
            && !self.protected_front;
        if !ordinary {
            return RoomState::NotOrdinary;
        }
        if self.deque.len() >= self.capacity {
            return RoomState::Full;
        }
        if self
            .byte_budget
            .is_some_and(|budget| !self.deque.is_empty() && self.queued_bytes() >= budget)
        {
            return RoomState::Full;
        }
        let over_budget = self.backlog_budget.is_some_and(|budget| {
            self.deque.len() >= MIN_VIDEO_BACKLOG_FRAMES
                && self.deque.front().is_some_and(|oldest| {
                    now.saturating_duration_since(oldest.enqueued_at) >= budget
                })
        });
        if over_budget {
            RoomState::Full
        } else {
            RoomState::HasRoom
        }
    }

    /// Close and let already-buffered items drain.
    pub fn close(&mut self) {
        self.closed = true;
    }

    /// Close and discard all buffered state immediately.
    pub fn close_and_clear(&mut self) {
        self.closed = true;
        self.deque.clear();
        self.generation_recovery = None;
        self.awaiting_keyframe = false;
    }

    /// Discards currently visible items, without changing recovery state.
    pub fn clear(&mut self) -> usize {
        let len = self.deque.len();
        self.deque.clear();
        len
    }

    /// Begin a paused generation barrier.
    pub fn begin_generation(&mut self) {
        let dropped = self.deque.len() + usize::from(self.generation_recovery.take().is_some());
        self.deque.clear();
        self.generation_chain = false;
        self.require_generation_recovery = false;
        self.protected_front = false;
        self.awaiting_keyframe = true;
        self.drops_since_keyframe = 0;
        self.last_keyframe_request_at = None;
        self.paused = true;
        self.keyframe_request_pending = true;
        self.frames_dropped = self.frames_dropped.saturating_add(dropped as u64);
    }

    /// Activates the pinned generation recovery AU.
    pub fn activate_generation(&mut self) -> bool {
        let Some(recovery) = self.generation_recovery.take() else {
            return false;
        };
        self.deque.push_front(recovery);
        self.paused = false;
        self.protected_front = true;
        if !self.require_generation_recovery {
            self.generation_chain = false;
        }
        true
    }

    /// Whether the queue is closed.
    #[must_use]
    pub const fn is_closed(&self) -> bool {
        self.closed
    }

    /// Number of visible queued AUs.
    #[must_use]
    pub fn len(&self) -> usize {
        self.deque.len()
    }

    /// Whether there are no visible queued AUs.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.deque.is_empty()
    }

    /// Bytes the visible queued records hold (as given to
    /// [`Self::push_sized`]).
    #[must_use]
    pub fn queued_bytes(&self) -> u64 {
        self.deque.iter().map(|queued| queued.bytes).sum()
    }

    /// Whether non-recovery P-frames are currently suppressed.
    #[must_use]
    pub const fn awaiting_keyframe(&self) -> bool {
        self.awaiting_keyframe
    }

    /// Whether the queue is paused at a generation barrier.
    #[must_use]
    pub const fn is_paused(&self) -> bool {
        self.paused
    }

    /// Whether a post-activation generation still needs recovery.
    #[must_use]
    pub const fn requires_generation_recovery(&self) -> bool {
        self.require_generation_recovery
    }

    /// Frames popped for writing.
    #[must_use]
    pub const fn frames_sent(&self) -> u64 {
        self.frames_sent
    }

    /// Frames discarded because of recovery, barrier or suppression decisions.
    #[must_use]
    pub const fn frames_dropped(&self) -> u64 {
        self.frames_dropped
    }

    /// Drops accumulated in the current recovery interval.
    #[must_use]
    pub const fn drops_since_keyframe(&self) -> u64 {
        self.drops_since_keyframe
    }

    /// Aggregate bytes popped for writing.
    #[must_use]
    pub const fn bytes_sent(&self) -> u64 {
        self.bytes_sent
    }

    /// The part of [`Self::bytes_sent`] that carried keyframes or recovery
    /// points, so a rate controller can tell a keyframe burst from a path
    /// that is filling.
    #[must_use]
    pub const fn keyframe_bytes_sent(&self) -> u64 {
        self.keyframe_bytes_sent
    }

    /// Returns and clears queue-wait statistics for frames popped by the writer.
    pub fn take_wait_stats(&mut self) -> VideoQueueWaitStats {
        let frames = self.wait_frames;
        let mean = if frames == 0 {
            Duration::ZERO
        } else {
            self.wait_total / u32::try_from(frames).unwrap_or(u32::MAX)
        };
        let stats = VideoQueueWaitStats {
            frames,
            mean,
            max: self.wait_max,
        };
        self.wait_total = Duration::ZERO;
        self.wait_max = Duration::ZERO;
        self.wait_frames = 0;
        stats
    }

    fn record_drop(&mut self, count: usize) {
        self.drops_since_keyframe = self.drops_since_keyframe.saturating_add(count as u64);
        self.frames_dropped = self.frames_dropped.saturating_add(count as u64);
    }

    fn keyframe_request_due(&mut self, now: Instant) -> bool {
        self.keyframe_request_pending = true;
        self.keyframe_request_due_at(now)
    }

    fn keyframe_request_due_at(&self, now: Instant) -> bool {
        self.keyframe_request_pending
            && self
                .last_keyframe_request_at
                .is_none_or(|last| now.duration_since(last) >= self.keyframe_request_min_interval)
    }

    fn keyframe_request_retry_at(&self, now: Instant) -> Option<Instant> {
        if !self.keyframe_request_pending {
            return None;
        }
        self.last_keyframe_request_at.and_then(|last| {
            let due = last + self.keyframe_request_min_interval;
            (now < due).then_some(due)
        })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_write_the_transport_holds_is_unsent_video_until_it_ends() {
        let meter = VideoWriteMeter::default();
        assert!(!meter.unsent(false));
        assert!(meter.unsent(true));
        let write = meter.begin();
        std::thread::sleep(Duration::from_millis(2));
        assert!(meter.unsent(false), "an empty queue behind a held write");
        drop(write);
        assert!(!meter.unsent(false));
        assert!(meter.take_wait() >= Duration::from_millis(2));
        assert_eq!(meter.take_wait(), Duration::ZERO);
    }

    #[test]
    fn combined_wait_is_weighted_by_frames() {
        let combined = VideoQueueWaitStats::combine([
            VideoQueueWaitStats {
                frames: 3,
                mean: Duration::from_millis(10),
                max: Duration::from_millis(20),
            },
            VideoQueueWaitStats {
                frames: 1,
                mean: Duration::from_millis(50),
                max: Duration::from_millis(60),
            },
            VideoQueueWaitStats::default(),
        ]);
        assert_eq!(combined.frames, 4);
        assert_eq!(combined.mean, Duration::from_millis(20));
        assert_eq!(combined.max, Duration::from_millis(60));
        assert_eq!(
            VideoQueueWaitStats::combine([]),
            VideoQueueWaitStats::default()
        );
    }

    use super::*;

    const CAPACITY: usize = 8;
    const THROTTLE: Duration = Duration::from_secs(1);

    fn queue() -> SharedVideoQueue<Vec<u8>> {
        SharedVideoQueue::new(CAPACITY, THROTTLE)
    }

    fn push_p(
        q: &mut SharedVideoQueue<Vec<u8>>,
        value: u8,
        now: Instant,
    ) -> VideoQueuePush<Vec<u8>> {
        q.push(
            vec![value],
            FrameClassification::keyframe_is_recovery(false),
            now,
        )
    }

    fn push_key(
        q: &mut SharedVideoQueue<Vec<u8>>,
        value: u8,
        now: Instant,
    ) -> VideoQueuePush<Vec<u8>> {
        q.push(
            vec![value],
            FrameClassification::keyframe_is_recovery(true),
            now,
        )
    }

    fn push_classified(
        q: &mut SharedVideoQueue<Vec<u8>>,
        value: u8,
        keyframe: bool,
        recovery_point: bool,
        now: Instant,
    ) -> VideoQueuePush<Vec<u8>> {
        q.push(
            vec![value],
            FrameClassification::new(keyframe, recovery_point),
            now,
        )
    }

    fn byte(value: usize) -> u8 {
        u8::try_from(value).expect("test value fits u8")
    }

    #[test]
    fn backlog_budget_scales_with_the_contract_rate_within_bounds() {
        assert_eq!(video_backlog_budget(60), Duration::from_nanos(33_333_333));
        assert_eq!(video_backlog_budget(30), Duration::from_nanos(66_666_666));
        assert_eq!(video_backlog_budget(240), Duration::from_millis(20));
        assert_eq!(video_backlog_budget(0), Duration::from_millis(100));
        assert_eq!(video_backlog_budget(10), Duration::from_millis(100));
    }

    #[test]
    fn a_stale_backlog_refuses_room_until_the_writer_catches_up() {
        let mut q = queue();
        let budget = Duration::from_millis(33);
        q.set_backlog_budget(Some(budget));
        let start = Instant::now();
        assert!(push_p(&mut q, 1, start).enqueued());
        // One old frame never throttles: the writer may just not have woken.
        assert_eq!(q.room_state_at(start + budget * 4), RoomState::HasRoom);
        assert!(push_p(&mut q, 2, start + Duration::from_millis(16)).enqueued());
        assert_eq!(
            q.room_state_at(start + Duration::from_millis(20)),
            RoomState::HasRoom
        );
        assert_eq!(q.room_state_at(start + budget), RoomState::Full);
        // The frames are kept, not dropped: the chain stays intact.
        assert_eq!(q.len(), 2);
        assert_eq!(q.frames_dropped(), 0);
        assert_eq!(q.pop_front(), Some(vec![1]));
        assert_eq!(q.room_state_at(start + budget), RoomState::HasRoom);
        // Without a budget only capacity counts.
        q.set_backlog_budget(None);
        assert!(push_p(&mut q, 3, start).enqueued());
        assert_eq!(q.room_state_at(start + budget * 10), RoomState::HasRoom);
    }

    #[test]
    fn dropped_p_frame_clears_descendants_and_awaits_idr() {
        let mut q = queue();
        let now = Instant::now();
        for i in 0..CAPACITY {
            assert!(push_p(&mut q, byte(i), now).enqueued());
        }
        assert_eq!(
            push_p(&mut q, 0xFF, now),
            VideoQueuePush::Dropped {
                count: CAPACITY + 1,
                recovery_started: true,
                idr_request: true,
            }
        );
        assert!(q.awaiting_keyframe());
        assert_eq!(q.frames_dropped(), (CAPACITY + 1) as u64);
        q.close();
        assert!(q.pop_front().is_none());
    }

    #[test]
    fn keyframe_clears_pending_and_fits() {
        let mut q = queue();
        let now = Instant::now();
        for i in 0..CAPACITY {
            assert!(push_p(&mut q, byte(i), now).enqueued());
        }
        assert_eq!(
            push_key(&mut q, 0xAA, now),
            VideoQueuePush::Enqueued { cleared: CAPACITY }
        );
        assert!(!q.awaiting_keyframe());
        assert_eq!(q.pop_front(), Some(vec![0xAA]));
    }

    #[test]
    fn future_p_frames_are_suppressed_until_keyframe_resumes() {
        let mut q = queue();
        let now = Instant::now();
        for i in 0..CAPACITY {
            assert!(push_p(&mut q, byte(i), now).enqueued());
        }
        assert!(push_p(&mut q, 10, now).idr_request());
        q.note_keyframe_request_handoff(true, now);
        assert_eq!(
            push_p(&mut q, 11, now),
            VideoQueuePush::Dropped {
                count: 1,
                recovery_started: false,
                idr_request: false,
            }
        );
        assert_eq!(q.frames_dropped(), (CAPACITY + 2) as u64);

        assert!(push_key(&mut q, 0xAA, now).enqueued());
        assert!(push_p(&mut q, 0xBB, now).enqueued());
        assert_eq!(q.pop_front(), Some(vec![0xAA]));
        assert_eq!(q.pop_front(), Some(vec![0xBB]));
    }

    #[test]
    fn each_recovered_chain_can_request_a_fresh_idr_immediately() {
        let mut q = queue();
        let now = Instant::now();
        for _ in 0..CAPACITY {
            assert!(push_p(&mut q, 0, now).enqueued());
        }
        assert!(push_p(&mut q, 1, now).idr_request());
        q.note_keyframe_request_handoff(true, now);
        assert!(!push_p(&mut q, 2, now).idr_request());
        assert!(push_key(&mut q, 3, now).enqueued());
        let mut requested = false;
        for _ in 0..CAPACITY {
            requested |= push_p(&mut q, 4, now).idr_request();
        }
        requested |= push_p(&mut q, 5, now).idr_request();
        assert!(
            requested,
            "a loss after recovery needs a new IDR without waiting on the old guard"
        );
    }

    #[test]
    fn awaiting_chain_retries_idr_only_after_throttle_interval() {
        let mut q = queue();
        let start = Instant::now();
        for value in 0..CAPACITY {
            assert!(push_p(&mut q, byte(value), start).enqueued());
        }
        assert!(push_p(&mut q, 10, start).idr_request());
        q.note_keyframe_request_handoff(true, start);
        assert!(!push_p(&mut q, 11, start + THROTTLE / 2).idr_request());
        assert!(push_p(&mut q, 12, start + THROTTLE).idr_request());
    }

    #[test]
    fn failed_keyframe_handoff_stays_pending_until_retry_succeeds() {
        let mut q = queue();
        let start = Instant::now();
        for value in 0..CAPACITY {
            assert!(push_p(&mut q, byte(value), start).enqueued());
        }
        assert!(push_p(&mut q, 10, start).idr_request());
        q.note_keyframe_request_handoff(false, start);
        assert_eq!(
            q.keyframe_request_retry(start + Duration::from_millis(1)),
            KeyframeRequestRetry {
                due: true,
                pending: true,
                retry_at: None
            }
        );
        q.note_keyframe_request_handoff(true, start + Duration::from_millis(1));
        assert_eq!(
            q.keyframe_request_retry(start + THROTTLE / 2),
            KeyframeRequestRetry {
                due: false,
                pending: false,
                retry_at: None
            }
        );
    }

    #[test]
    fn successful_handoff_throttles_retry_until_its_deadline() {
        let mut q = queue();
        let start = Instant::now();
        for value in 0..CAPACITY {
            assert!(push_p(&mut q, byte(value), start).enqueued());
        }
        assert!(push_p(&mut q, 10, start).idr_request());
        q.note_keyframe_request_handoff(true, start);
        assert!(!push_p(&mut q, 11, start + Duration::from_millis(1)).idr_request());
        assert_eq!(
            q.keyframe_request_retry(start + THROTTLE / 2),
            KeyframeRequestRetry {
                due: false,
                pending: true,
                retry_at: Some(start + THROTTLE)
            }
        );
    }

    #[test]
    fn full_frame_requests_coalesce_instead_of_discarding() {
        let start = Instant::now();
        let mut coalescer = FullFrameRequestCoalescer::new(THROTTLE);
        assert_eq!(
            coalescer.request(start),
            FullFrameRequestDecision {
                deliver_now: true,
                pending: false,
                deliver_at: None
            }
        );
        assert_eq!(
            coalescer.request(start + THROTTLE / 2),
            FullFrameRequestDecision {
                deliver_now: false,
                pending: true,
                deliver_at: Some(start + THROTTLE)
            }
        );
        assert_eq!(
            coalescer.poll(start + THROTTLE),
            FullFrameRequestDecision {
                deliver_now: true,
                pending: false,
                deliver_at: None
            }
        );
    }

    #[test]
    fn failed_keyframe_enqueue_keeps_awaiting_state() {
        let mut q = queue();
        let now = Instant::now();
        for value in 0..CAPACITY {
            assert!(push_p(&mut q, byte(value), now).enqueued());
        }
        assert!(push_p(&mut q, 10, now).idr_request());
        q.close();
        assert_eq!(
            push_key(&mut q, 0xAA, now),
            VideoQueuePush::Closed(vec![0xAA])
        );
        assert!(q.awaiting_keyframe());
        assert!(q.pop_front().is_none());
    }

    #[test]
    fn generation_barrier_hides_old_and_new_frames_until_activation() {
        let mut q = queue();
        let now = Instant::now();
        assert!(push_p(&mut q, 1, now).enqueued());

        q.begin_generation();
        assert!(q.is_paused());
        assert!(q.awaiting_keyframe());
        assert!(q.pop_front().is_none());
        assert!(push_p(&mut q, 2, now).idr_request());
        q.note_keyframe_request_handoff(true, now);
        let pinned = q.pin_generation_recovery(vec![3], now);
        assert!(pinned.accepted);
        assert!(pinned.idr_request);
        assert_eq!(
            push_classified(&mut q, 4, false, false, now),
            VideoQueuePush::Dropped {
                count: 1,
                recovery_started: false,
                idr_request: false,
            }
        );
        assert!(push_classified(&mut q, 5, true, true, now).enqueued());
        assert!(push_classified(&mut q, 6, false, false, now).enqueued());
        assert!(q.pop_front().is_none());

        assert!(q.activate_generation());
        assert_eq!(q.pop_front(), Some(vec![3]));
        assert_eq!(q.pop_front(), Some(vec![5]));
        assert_eq!(q.pop_front(), Some(vec![6]));
    }

    #[test]
    fn generation_activation_requires_a_pinned_recovery_au() {
        let mut q = queue();
        q.begin_generation();
        assert!(!q.activate_generation());
        assert!(q.is_paused());
    }

    #[test]
    fn pinned_recovery_survives_delayed_barrier_and_overflow() {
        let mut q = queue();
        let now = Instant::now();
        q.begin_generation();
        assert!(push_key(&mut q, 0x10, now).enqueued());
        assert!(push_p(&mut q, 0x11, now).enqueued());
        assert!(q.pin_generation_recovery(vec![0xAA], now).accepted);

        assert!(!push_classified(&mut q, 0x20, false, false, now).enqueued());
        assert!(push_classified(&mut q, 0xBB, true, true, now).enqueued());
        for value in 0..CAPACITY * 3 {
            let accepted = push_classified(&mut q, byte(value), false, false, now).enqueued();
            if value < CAPACITY - 2 {
                assert!(accepted);
            }
        }
        assert!(q.requires_generation_recovery());
        assert!(push_classified(&mut q, 0xCC, true, true, now).enqueued());
        assert!(push_classified(&mut q, 0xDD, false, false, now).enqueued());
        assert!(q.pop_front().is_none());

        assert!(q.activate_generation());
        assert_eq!(q.pop_front(), Some(vec![0xAA]));
        assert_eq!(q.pop_front(), Some(vec![0xCC]));
        assert_eq!(q.pop_front(), Some(vec![0xDD]));
    }

    #[test]
    fn activation_before_followup_idr_suppresses_p_frames() {
        let mut q = queue();
        let now = Instant::now();
        q.begin_generation();
        assert!(q.pin_generation_recovery(vec![0xAA], now).accepted);
        assert!(q.activate_generation());

        assert!(!push_classified(&mut q, 0x10, false, false, now).enqueued());
        assert_eq!(q.pop_front(), Some(vec![0xAA]));
        assert!(push_classified(&mut q, 0xBB, true, true, now).enqueued());
        assert!(push_classified(&mut q, 0x11, false, false, now).enqueued());
        assert_eq!(q.pop_front(), Some(vec![0xBB]));
        assert_eq!(q.pop_front(), Some(vec![0x11]));
    }

    #[test]
    fn activated_recovery_cannot_be_evicted_before_delivery() {
        let mut q = queue();
        let now = Instant::now();
        q.begin_generation();
        assert!(q.pin_generation_recovery(vec![0xAA], now).accepted);
        assert!(push_classified(&mut q, 0xBB, true, true, now).enqueued());
        assert!(q.activate_generation());

        for value in 0..CAPACITY * 2 {
            let _ = push_classified(&mut q, byte(value), false, false, now);
        }
        assert_eq!(q.pop_front(), Some(vec![0xAA]));
        assert!(q.awaiting_keyframe());
        assert!(push_classified(&mut q, 0xCC, true, true, now).enqueued());
        assert_eq!(q.pop_front(), Some(vec![0xCC]));
    }

    #[test]
    fn close_drains_then_ends() {
        let mut q = queue();
        let now = Instant::now();
        assert!(push_p(&mut q, 7, now).enqueued());
        q.close();
        assert_eq!(q.pop_front(), Some(vec![7]));
        assert!(q.pop_front().is_none());
    }

    #[test]
    fn close_and_clear_discards_buffered_frames_instead_of_draining() {
        let mut q = queue();
        let now = Instant::now();
        assert!(push_key(&mut q, 1, now).enqueued());
        assert!(push_p(&mut q, 2, now).enqueued());
        assert!(push_p(&mut q, 3, now).enqueued());
        q.close_and_clear();
        assert!(q.pop_front().is_none());
    }

    #[test]
    fn close_and_clear_also_discards_pinned_generation_recovery_state() {
        let mut q = queue();
        let now = Instant::now();
        q.begin_generation();
        assert!(q.pin_generation_recovery(vec![0xAA], now).accepted);
        assert!(q.activate_generation());
        q.close_and_clear();
        assert!(q.pop_front().is_none());
    }

    #[test]
    fn pop_accounts_sent_frames_and_bytes() {
        let mut q = queue();
        let now = Instant::now();
        assert!(
            q.push(
                vec![1, 2, 3],
                FrameClassification::keyframe_is_recovery(true),
                now
            )
            .enqueued()
        );
        assert!(
            q.push(
                vec![4, 5],
                FrameClassification::keyframe_is_recovery(false),
                now
            )
            .enqueued()
        );
        assert_eq!(q.pop_front_with_bytes(Vec::len), Some(vec![1, 2, 3]));
        assert_eq!(q.pop_front_with_bytes(Vec::len), Some(vec![4, 5]));
        assert_eq!(q.frames_sent(), 2);
        assert_eq!(q.bytes_sent(), 5);
        assert_eq!(q.keyframe_bytes_sent(), 3);
    }

    #[test]
    fn a_relayed_window_stays_open_until_the_relay_reports_and_counts_from_the_start() {
        let now = Instant::now();
        let mut window = RelayedVideoWindow::default();
        for _ in 0..5 {
            assert!(window.admits(now));
            window.note_sent_at(now);
        }
        assert!(!window.is_active());
        // The relay has released four of the five frames sent so far.
        window.note_released_total(4);
        assert!(window.is_active());
        assert_eq!(window.outstanding(), 1);
        assert!(!window.admits(now), "one frame already waits in the relay");
        window.note_released_total(5);
        assert!(window.admits(now));
    }

    #[test]
    fn a_relayed_window_holds_one_frame_in_flight() {
        let now = Instant::now();
        let mut window = RelayedVideoWindow::default();
        window.note_released_total(0);
        assert!(window.admits(now));
        window.note_sent_at(now);
        assert!(!window.admits(now));
        assert_eq!(window.resync_at(), Some(now + RELAYED_VIDEO_RESYNC));
        // A report that does not grow frees nothing.
        window.note_released_total(0);
        assert!(!window.admits(now));
        window.note_released_total(1);
        assert!(window.admits(now + Duration::from_millis(5)));
        assert_eq!(window.resync_at(), None);
    }

    /// The reviewer's case: a slow lane stays quiet for 1.5 s while the agent
    /// has frames queued. The window lets one frame per period through,
    /// keeps counting, and once the relay catches up holds one frame in
    /// flight again instead of forgetting the frames it sent meanwhile.
    #[test]
    fn a_quiet_relay_paces_one_extra_frame_per_period_and_the_backlog_returns_to_one() {
        let start = Instant::now();
        let mut window = RelayedVideoWindow::default();
        let mut released = 0;
        window.note_released_total(released);
        // Steady state: one in flight.
        assert!(window.admits(start));
        window.note_sent_at(start);
        // The lane stalls for 1.5 s; the writer polls every 10 ms.
        let mut let_through = 0;
        for tick in 1..=150 {
            let now = start + Duration::from_millis(10 * tick);
            if window.admits(now) {
                window.note_sent_at(now);
                let_through += 1;
            }
        }
        assert_eq!(let_through, 1, "one frame per quiet second");
        assert_eq!(window.outstanding(), 2);
        assert!(window.is_active(), "the limit never turns off");
        // The lane drains what it holds.
        released += 2;
        let later = start + Duration::from_millis(1600);
        window.note_released_total(released);
        assert_eq!(window.outstanding(), 0);
        assert!(window.admits(later));
        window.note_sent_at(later);
        assert!(!window.admits(later), "back to one frame in flight");
        assert_eq!(window.outstanding(), 1);
    }

    /// A producer that follows [`OutboundVideoRecord::room_wait`] against a
    /// consumer that stalls for `stall` and then takes one record a tick:
    /// what reached the writer, what the queue dropped, the keyframe (new
    /// generation) requests, and the most the queue ever held.
    struct SlowPath {
        sent: Vec<u32>,
        dropped: u64,
        idr_requests: u32,
        most_records: usize,
        most_bytes: u64,
    }

    fn slow_path(record: OutboundVideoRecord, records: u32, stall: Duration) -> SlowPath {
        const TICK: Duration = Duration::from_millis(16);
        const BYTES: u64 = 4 * 1024 * 1024;
        let start = Instant::now();
        let mut queue = SharedVideoQueue::new(8, Duration::from_secs(1));
        queue.set_backlog_budget(Some(video_backlog_budget(60)));
        queue.set_byte_budget(Some(CANVAS_QUEUE_BYTES));
        let mut path = SlowPath {
            sent: Vec::new(),
            dropped: 0,
            idr_requests: 0,
            most_records: 0,
            most_bytes: 0,
        };
        let mut next = 0_u32;
        let mut waiting_since: Option<Instant> = None;
        for step in 0..2_000_u32 {
            let now = start + TICK * step;
            if next < records {
                let full = queue.room_state_at(now) == RoomState::Full;
                let push = match record.room_wait() {
                    RoomWait::Never => true,
                    _ if !full => true,
                    RoomWait::UpTo(wait) => {
                        now.saturating_duration_since(*waiting_since.get_or_insert(now)) >= wait
                    }
                    RoomWait::UntilRoomOrClosed => false,
                };
                if push {
                    waiting_since = None;
                    let classification = match record {
                        OutboundVideoRecord::AccessUnit { recovery_point } => {
                            FrameClassification::keyframe_is_recovery(recovery_point)
                        }
                        OutboundVideoRecord::CanvasFrame => FrameClassification::new(false, false),
                        OutboundVideoRecord::CanvasReset => FrameClassification::new(true, true),
                    };
                    let pushed = queue.push_sized(next, classification, BYTES, now);
                    path.idr_requests += u32::from(pushed.idr_request());
                    next += 1;
                }
            }
            path.most_records = path.most_records.max(queue.len());
            path.most_bytes = path.most_bytes.max(queue.queued_bytes());
            if now.saturating_duration_since(start) >= stall {
                if let Some(item) = queue.pop_front() {
                    path.sent.push(item);
                }
            }
            if next == records && queue.is_empty() {
                break;
            }
        }
        path.dropped = queue.frames_dropped();
        path
    }

    /// A bounded slow consumer: a canvas path that stalls for six seconds
    /// holds its producer back instead of dropping a record, so no new
    /// canvas generation (a full repaint) is started, every record arrives
    /// in order, and the queue never holds more than its record and byte
    /// bounds. An access unit keeps its own rule: past the room wait the
    /// link is gone and the prediction chain restarts at a keyframe.
    #[test]
    fn a_slow_consumer_holds_canvas_frames_back_without_a_drop_or_a_new_generation() {
        let canvas = slow_path(OutboundVideoRecord::CanvasFrame, 60, Duration::from_secs(6));
        assert_eq!(canvas.dropped, 0);
        assert_eq!(canvas.idr_requests, 0, "no new generation");
        assert_eq!(
            canvas.sent,
            (0..60).collect::<Vec<_>>(),
            "every record, in order"
        );
        assert!(canvas.most_records <= 8);
        assert!(
            canvas.most_bytes <= CANVAS_QUEUE_BYTES + 4 * 1024 * 1024,
            "one record past the byte budget at most: {}",
            canvas.most_bytes
        );

        let video = slow_path(
            OutboundVideoRecord::AccessUnit {
                recovery_point: false,
            },
            60,
            Duration::from_secs(6),
        );
        assert!(video.dropped > 0 && video.idr_requests > 0);
    }

    /// A mandatory reset under an optional backlog: the barrier never waits
    /// for room, supersedes the old generation's queued frames (which are
    /// not loss and ask for nothing), and the queue takes the new
    /// generation's frames behind it.
    #[test]
    fn a_canvas_reset_never_waits_and_supersedes_a_full_queue() {
        let now = Instant::now();
        let mut queue = SharedVideoQueue::new(4, Duration::from_secs(1));
        queue.set_byte_budget(Some(CANVAS_QUEUE_BYTES));
        for record in 0..4 {
            assert!(
                queue
                    .push_sized(record, FrameClassification::new(false, false), 1024, now)
                    .enqueued()
            );
        }
        assert_eq!(queue.room_state_at(now), RoomState::Full);
        assert_eq!(
            OutboundVideoRecord::new(true, true).room_wait(),
            RoomWait::Never
        );
        assert_eq!(
            queue.push_sized(9, FrameClassification::new(true, true), 64, now),
            VideoQueuePush::Enqueued { cleared: 4 }
        );
        assert_eq!(queue.frames_dropped(), 0);
        assert!(!queue.awaiting_keyframe());
        assert!(
            queue
                .push_sized(10, FrameClassification::new(false, false), 64, now)
                .enqueued()
        );
        assert_eq!(queue.pop_front(), Some(9));
        assert_eq!(queue.pop_front(), Some(10));
    }

    /// A record that is lost (the queue overflowed past every wait, as a
    /// legacy queue does) holds the rest of its generation back, asks once
    /// for recovery, and the next reset starts a new generation that flows.
    #[test]
    fn a_lost_canvas_record_holds_its_generation_until_the_next_reset() {
        let now = Instant::now();
        let mut queue = SharedVideoQueue::new(2, Duration::from_secs(1));
        let frame = FrameClassification::new(false, false);
        assert!(queue.push(1, frame, now).enqueued());
        assert!(queue.push(2, frame, now).enqueued());
        let lost = queue.push(3, frame, now);
        assert!(matches!(
            lost,
            VideoQueuePush::Dropped {
                recovery_started: true,
                idr_request: true,
                ..
            }
        ));
        assert!(queue.is_empty(), "the broken generation is never sent");
        queue.note_keyframe_request_handoff(true, now);
        let held = queue.push(4, frame, now + Duration::from_millis(100));
        assert!(!held.enqueued() && !held.idr_request(), "asked once");
        assert_eq!(
            queue.push(5, FrameClassification::new(true, true), now),
            VideoQueuePush::Enqueued { cleared: 0 }
        );
        assert!(queue.push(6, frame, now).enqueued());
        assert_eq!(queue.pop_front(), Some(5));
        assert_eq!(queue.pop_front(), Some(6));
    }

    /// The byte budget refuses room once queued records hold it, but a
    /// single record of any size is always taken.
    #[test]
    fn the_byte_budget_refuses_room_but_always_takes_one_record() {
        let now = Instant::now();
        let mut queue = SharedVideoQueue::new(8, Duration::from_secs(1));
        queue.set_byte_budget(Some(100));
        assert_eq!(queue.room_state_at(now), RoomState::HasRoom);
        assert!(
            queue
                .push_sized(1, FrameClassification::new(false, false), 500, now)
                .enqueued()
        );
        assert_eq!(queue.queued_bytes(), 500);
        assert_eq!(queue.room_state_at(now), RoomState::Full);
        assert_eq!(queue.pop_front(), Some(1));
        assert_eq!(queue.queued_bytes(), 0);
        assert_eq!(queue.room_state_at(now), RoomState::HasRoom);
    }

    /// Teardown while a canvas producer waits: closing the queue ends the
    /// wait (the room is no longer ordinary) and hands the record back.
    #[test]
    fn closing_the_queue_releases_a_waiting_canvas_producer() {
        let now = Instant::now();
        let mut queue = SharedVideoQueue::new(1, Duration::from_secs(1));
        let frame = FrameClassification::new(false, false);
        assert!(queue.push(1, frame, now).enqueued());
        assert_eq!(queue.room_state_at(now), RoomState::Full);
        queue.close_and_clear();
        assert_eq!(queue.room_state_at(now), RoomState::NotOrdinary);
        assert_eq!(queue.push(2, frame, now), VideoQueuePush::Closed(2));
    }
}
