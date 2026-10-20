//! One lane's bounded send queue and its dedicated writer task.

use std::collections::VecDeque;
use std::fmt::{Debug, Display, Formatter};
use std::num::NonZeroU32;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{LaneFraming, LaneRole, LaneStreamCode, PeerTransportMismatch};
use bytes::Bytes;
use quinn::{SendStream, WriteError};
use tokio::sync::{Notify, watch};

use crate::lanes::{LaneFailure, LaneRecoveryAction, lane_max_frame_bytes, lane_send_priority};

use super::connection::{Shared, classify_peer_close};
use super::lane::{LaneControl, SendHalf};
use super::stats::{RoleStats, add, bump, micros, raise, sub};
use super::{
    INTERACTIVE_LANE_BURST_BYTES, INTERACTIVE_LANE_BURST_WINDOW, LOW_PRIORITY_WRITE_CHUNK_BYTES,
    LaneQueueBounds, is_low_priority,
};

/// How a lane's writer or reader ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneExit {
    /// The owner finished the lane and every queued record was written.
    Finished,
    /// The lane failed; the failure was reported and `action` is the
    /// registry's lane-scoped recovery. Queued records were discarded and
    /// nothing written is resent.
    Failed {
        /// Why.
        failure: LaneFailure,
        /// Recovery to follow.
        action: LaneRecoveryAction,
    },
    /// Superseded by a newer generation, disabled, failed through the other
    /// half, or the attachment ended. The stream was reset with this code.
    Cancelled(LaneStreamCode),
    /// The QUIC connection was lost.
    ConnectionLost {
        /// The peer closed it because it speaks another transport layout.
        mismatch: Option<PeerTransportMismatch>,
    },
}

/// Why a record was not queued. Every variant except `Oversize` returns the
/// record, so an edge-bearing message is never lost silently: the caller
/// waits ([`LaneSender::send`]), retries, or applies its own policy.
#[derive(Clone, PartialEq, Eq)]
pub enum LaneSendError {
    /// The role's queue is full: back-pressure.
    Full(Bytes),
    /// The lane closed; its exit says why.
    Closed(Bytes),
    /// The record exceeds the role's frame bound.
    Oversize {
        /// Record length.
        len: usize,
        /// Role bound.
        max: usize,
    },
}

impl LaneSendError {
    /// The rejected record, when the error carries it.
    #[must_use]
    pub fn into_record(self) -> Option<Bytes> {
        match self {
            Self::Full(record) | Self::Closed(record) => Some(record),
            Self::Oversize { .. } => None,
        }
    }
}

impl Debug for LaneSendError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(record) => write!(formatter, "Full({} bytes)", record.len()),
            Self::Closed(record) => write!(formatter, "Closed({} bytes)", record.len()),
            Self::Oversize { len, max } => {
                write!(formatter, "Oversize {{ len: {len}, max: {max} }}")
            }
        }
    }
}

impl Display for LaneSendError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Full(_) => formatter.write_str("lane queue full"),
            Self::Closed(_) => formatter.write_str("lane closed"),
            Self::Oversize { len, max } => {
                write!(formatter, "{len}-byte record exceeds the lane bound {max}")
            }
        }
    }
}

impl std::error::Error for LaneSendError {}

struct Queued {
    payload: Bytes,
    enqueued_at: Instant,
}

#[derive(Default)]
struct Queue {
    records: VecDeque<Queued>,
    bytes: usize,
    closed: bool,
    finishing: bool,
}

struct SenderInner {
    shared: Arc<Shared>,
    role: LaneRole,
    lane_generation: NonZeroU32,
    bounds: LaneQueueBounds,
    max_frame: usize,
    control: Arc<LaneControl>,
    queue: Mutex<Queue>,
    item_ready: Notify,
    space_ready: Notify,
    exit: watch::Sender<Option<LaneExit>>,
    delivered: watch::Sender<Option<bool>>,
}

/// Finishes the lane once the last [`LaneSender`] handle is gone.
struct SenderGuard(Arc<SenderInner>);

impl Drop for SenderGuard {
    fn drop(&mut self) {
        self.0.queue().finishing = true;
        self.0.item_ready.notify_one();
    }
}

/// Producer handle of one lane's bounded queue. The records are written by
/// a dedicated task, so a producer never awaits QUIC and a blocked lane
/// blocks only its own role.
///
/// Media lanes frame each record with the existing big-endian `u32` length
/// prefix; session-message lanes write each record verbatim, as one
/// complete, already-encoded session frame. A record is never split across
/// lane generations.
#[derive(Clone)]
pub struct LaneSender {
    inner: Arc<SenderInner>,
    _guard: Arc<SenderGuard>,
}

impl Debug for LaneSender {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let (records, bytes) = self.queued();
        formatter
            .debug_struct("LaneSender")
            .field("role", &self.inner.role)
            .field("lane_generation", &self.inner.lane_generation)
            .field("queued_records", &records)
            .field("queued_bytes", &bytes)
            .finish_non_exhaustive()
    }
}

impl SenderInner {
    fn queue(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stats(&self) -> &RoleStats {
        self.shared.stats(self.role)
    }

    fn enqueue(&self, record: Bytes, count_full: bool) -> Result<(), LaneSendError> {
        let stats = self.stats();
        let len = record.len();
        if len > self.max_frame {
            bump(&stats.rejected_oversize);
            return Err(LaneSendError::Oversize {
                len,
                max: self.max_frame,
            });
        }
        {
            let mut queue = self.queue();
            if queue.closed || queue.finishing || self.control.token().is_cancelled() {
                bump(&stats.rejected_closed);
                return Err(LaneSendError::Closed(record));
            }
            let over_bytes = !queue.records.is_empty()
                && queue.bytes.saturating_add(len) > self.bounds.max_bytes;
            if queue.records.len() >= self.bounds.max_records || over_bytes {
                if count_full {
                    bump(&stats.rejected_full);
                }
                return Err(LaneSendError::Full(record));
            }
            let now = Instant::now();
            if queue.records.is_empty() {
                stats.oldest_queued_at.store(
                    micros(now.saturating_duration_since(self.shared.epoch)).saturating_add(1),
                    std::sync::atomic::Ordering::Relaxed,
                );
            }
            queue.bytes += len;
            queue.records.push_back(Queued {
                payload: record,
                enqueued_at: now,
            });
            add(&stats.queued_records, 1);
            add(&stats.queued_bytes, len as u64);
        }
        self.item_ready.notify_one();
        Ok(())
    }

    fn update_oldest(&self, queue: &Queue) {
        let value = queue.records.front().map_or(0, |front| {
            micros(
                front
                    .enqueued_at
                    .saturating_duration_since(self.shared.epoch),
            )
            .saturating_add(1)
        });
        self.stats()
            .oldest_queued_at
            .store(value, std::sync::atomic::Ordering::Relaxed);
    }

    async fn next_item(&self) -> Option<Queued> {
        loop {
            let notified = self.item_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut queue = self.queue();
                if let Some(item) = queue.records.pop_front() {
                    queue.bytes -= item.payload.len();
                    sub(&self.stats().queued_records, 1);
                    sub(&self.stats().queued_bytes, item.payload.len() as u64);
                    self.update_oldest(&queue);
                    drop(queue);
                    self.space_ready.notify_waiters();
                    return Some(item);
                }
                if queue.finishing {
                    return None;
                }
            }
            notified.await;
        }
    }

    fn close_queue(&self) {
        let (records, bytes) = {
            let mut queue = self.queue();
            queue.closed = true;
            let records = queue.records.len() as u64;
            let bytes = queue.bytes as u64;
            queue.records.clear();
            queue.bytes = 0;
            self.update_oldest(&queue);
            (records, bytes)
        };
        let stats = self.stats();
        sub(&stats.queued_records, records);
        sub(&stats.queued_bytes, bytes);
        add(&stats.discarded_on_close, records);
        self.space_ready.notify_waiters();
    }

    fn record_sent(&self, len: usize, wait: Duration) {
        let stats = self.stats();
        bump(&stats.records_sent);
        add(&stats.bytes_sent, len as u64);
        stats
            .last_writer_wait_us
            .store(micros(wait), std::sync::atomic::Ordering::Relaxed);
        raise(&stats.max_writer_wait_us, micros(wait));
    }

    fn starved(&self, waited: Duration, cause: &str) {
        let stats = self.stats();
        bump(&stats.starved);
        let (records, bytes) = {
            let queue = self.queue();
            (queue.records.len() as u64, queue.bytes as u64)
        };
        self.shared
            .telemetry
            .starved(self.role.token(), waited, records, bytes, cause);
    }
}

enum WriteFail {
    Cancelled,
    Stream { written: usize, error: WriteError },
}

async fn write_slices(
    stream: &mut SendStream,
    slices: [&[u8]; 2],
    chunk: usize,
    yield_between: bool,
) -> Result<(), WriteFail> {
    let mut written = 0_usize;
    for slice in slices {
        let mut offset = 0;
        while offset < slice.len() {
            let end = slice.len().min(offset.saturating_add(chunk));
            match stream.write(&slice[offset..end]).await {
                Ok(count) => {
                    offset += count;
                    written += count;
                }
                Err(error) => return Err(WriteFail::Stream { written, error }),
            }
            if yield_between && offset < slice.len() {
                tokio::task::yield_now().await;
            }
        }
    }
    Ok(())
}

impl LaneSender {
    pub(super) fn spawn(
        shared: Arc<Shared>,
        role: LaneRole,
        lane_generation: NonZeroU32,
        control: Arc<LaneControl>,
        half: SendHalf,
        bounds: LaneQueueBounds,
    ) -> Self {
        let max_frame = lane_max_frame_bytes(role, &shared.policy);
        let inner = Arc::new(SenderInner {
            shared,
            role,
            lane_generation,
            bounds,
            max_frame,
            control,
            queue: Mutex::new(Queue::default()),
            item_ready: Notify::new(),
            space_ready: Notify::new(),
            exit: watch::Sender::new(None),
            delivered: watch::Sender::new(None),
        });
        tokio::spawn(run_writer(Arc::clone(&inner), half));
        Self {
            _guard: Arc::new(SenderGuard(Arc::clone(&inner))),
            inner,
        }
    }

    /// Role.
    #[must_use]
    pub fn role(&self) -> LaneRole {
        self.inner.role
    }

    /// Lane generation this sender writes.
    #[must_use]
    pub fn lane_generation(&self) -> NonZeroU32 {
        self.inner.lane_generation
    }

    /// Queues one record without waiting. Records are written whole and in
    /// order by the lane's writer task.
    ///
    /// # Errors
    ///
    /// [`LaneSendError::Full`] (back-pressure, record returned),
    /// [`LaneSendError::Closed`] (record returned) or
    /// [`LaneSendError::Oversize`].
    pub fn try_send(&self, record: Bytes) -> Result<(), LaneSendError> {
        self.inner.enqueue(record, true)
    }

    /// Queues one record, waiting for room in this role's queue only.
    ///
    /// Cancel-safe in the queue sense: a dropped call never leaves a
    /// partial record queued, but the record it owned is dropped with it.
    /// An owner that must not lose it (input edges) uses
    /// [`LaneSender::try_send`], which hands it back.
    ///
    /// # Errors
    ///
    /// [`LaneSendError::Closed`] when the lane ends while waiting, or
    /// [`LaneSendError::Oversize`].
    pub async fn send(&self, mut record: Bytes) -> Result<(), LaneSendError> {
        loop {
            let notified = self.inner.space_ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.inner.enqueue(record, false) {
                Err(LaneSendError::Full(returned)) => record = returned,
                other => return other,
            }
            tokio::select! {
                () = &mut notified => {}
                () = self.inner.control.token().cancelled() => {
                    bump(&self.inner.stats().rejected_closed);
                    return Err(LaneSendError::Closed(record));
                }
            }
        }
    }

    /// Records and bytes waiting in this lane's queue.
    #[must_use]
    pub fn queued(&self) -> (usize, usize) {
        let queue = self.inner.queue();
        (queue.records.len(), queue.bytes)
    }

    /// Writes what is queued, then finishes the stream cleanly. Later
    /// records are refused as closed.
    pub fn finish(&self) {
        self.inner.queue().finishing = true;
        self.inner.item_ready.notify_one();
    }

    /// How the writer ended, if it has.
    #[must_use]
    pub fn exit(&self) -> Option<LaneExit> {
        self.inner.exit.borrow().clone()
    }

    /// Resolves once the peer has acknowledged every byte of a finished
    /// lane: `true` when the writer finished cleanly and QUIC confirmed
    /// receipt of all of it (the peer's QUIC stack has the bytes, though its
    /// owner may not have read them yet), `false` when the lane ended any
    /// other way (failed, reset, cancelled, connection lost, peer stopped
    /// reading).
    ///
    /// Waits for the writer to end first, so call [`LaneSender::finish`]
    /// (or drop every sender) and bound the wait: a peer that never
    /// acknowledges holds it until the connection closes.
    pub async fn delivered(&self) -> bool {
        let mut receiver = self.inner.delivered.subscribe();
        loop {
            if let Some(delivered) = *receiver.borrow_and_update() {
                return delivered;
            }
            if receiver.changed().await.is_err() {
                return false;
            }
        }
    }

    /// Resolves when the writer ends.
    pub async fn closed(&self) -> LaneExit {
        let mut receiver = self.inner.exit.subscribe();
        loop {
            if let Some(exit) = receiver.borrow_and_update().clone() {
                return exit;
            }
            if receiver.changed().await.is_err() {
                return LaneExit::Cancelled(LaneStreamCode::AttachmentEnded);
            }
        }
    }
}

/// Keeps a flooding interactive lane from starving audio and microphone.
///
/// QUIC priority is strict: while the input or control stream has bytes
/// buffered, nothing of lower priority is sent. Interactive traffic is small,
/// so that is the right order — until a lane writes more than
/// [`INTERACTIVE_LANE_BURST_BYTES`] within [`INTERACTIVE_LANE_BURST_WINDOW`].
/// Then its stream drops to the audio priority, where QUIC's per-stream
/// fairness shares the path round-robin, and it returns to its contract
/// priority after a window within budget. Its queue stays bounded and its
/// order is unchanged; nothing is dropped.
struct BurstGuard {
    base: i32,
    demoted: Option<i32>,
    window_start: Instant,
    window_bytes: usize,
}

impl BurstGuard {
    fn new(role: LaneRole) -> Self {
        let base = lane_send_priority(role);
        let shared = lane_send_priority(LaneRole::AudioOutput);
        Self {
            base,
            demoted: (base > shared).then_some(shared),
            window_start: Instant::now(),
            window_bytes: 0,
        }
    }

    fn after_write(&mut self, inner: &SenderInner, stream: &SendStream, len: usize) {
        let Some(shared) = self.demoted else {
            return;
        };
        let now = Instant::now();
        if now.saturating_duration_since(self.window_start) >= INTERACTIVE_LANE_BURST_WINDOW {
            if self.window_bytes <= INTERACTIVE_LANE_BURST_BYTES
                && stream
                    .priority()
                    .is_ok_and(|priority| priority != self.base)
            {
                let _ = stream.set_priority(self.base);
            }
            self.window_start = now;
            self.window_bytes = 0;
        }
        self.window_bytes = self.window_bytes.saturating_add(len);
        if self.window_bytes > INTERACTIVE_LANE_BURST_BYTES
            && stream
                .priority()
                .is_ok_and(|priority| priority == self.base)
            && stream.set_priority(shared).is_ok()
        {
            bump(&inner.stats().priority_demotions);
        }
    }
}

async fn run_writer(inner: Arc<SenderInner>, mut half: SendHalf) {
    let mut exit = write_loop(&inner, &mut half).await;
    let finished = if exit == LaneExit::Finished {
        half.finish()
    } else {
        None
    };
    if let LaneExit::Cancelled(code) = exit {
        if half.reset(code) {
            bump(&inner.stats().resets);
        }
        // As for the reader: a lost connection is reported as such.
        if let Some(error) = inner.shared.connection.close_reason() {
            exit = LaneExit::ConnectionLost {
                mismatch: super::connection::classify_peer_close(&error),
            };
        }
    }
    inner.close_queue();
    inner.exit.send_replace(Some(exit));
    // Finished is reported as soon as every record is handed to QUIC;
    // delivery (the peer acknowledged all of it) follows separately, so a
    // graceful close can wait for it before closing the connection, which
    // would otherwise abandon whatever was still unacknowledged.
    let delivered = match finished {
        Some(stream) => {
            let stopped = stream.stopped();
            drop(stream);
            tokio::select! {
                biased;
                () = inner.control.token().cancelled() => false,
                stopped = stopped => matches!(stopped, Ok(None)),
            }
        }
        None => false,
    };
    inner.delivered.send_replace(Some(delivered));
}

async fn write_loop(inner: &SenderInner, half: &mut SendHalf) -> LaneExit {
    let framing = inner.role.framing();
    let low_priority = is_low_priority(inner.role);
    let chunk = if low_priority {
        LOW_PRIORITY_WRITE_CHUNK_BYTES
    } else {
        usize::MAX
    };
    let mut healthy = false;
    let mut burst = BurstGuard::new(inner.role);
    loop {
        let item = tokio::select! {
            biased;
            () = inner.control.token().cancelled() => {
                return LaneExit::Cancelled(inner.control.code_or_failed());
            }
            item = inner.next_item() => item,
        };
        let Some(item) = item else {
            return LaneExit::Finished;
        };
        let queue_age = item.enqueued_at.elapsed();
        raise(&inner.stats().max_queue_age_us, micros(queue_age));
        let started = Instant::now();
        let mut armed = true;
        let result = {
            let Some(stream) = half.stream() else {
                return LaneExit::Cancelled(inner.control.code_or_failed());
            };
            let prefix = u32::try_from(item.payload.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes();
            let prefix: &[u8] = match framing {
                LaneFraming::LengthPrefixedMedia => &prefix,
                LaneFraming::SessionMessages => &[],
            };
            let write = write_slices(stream, [prefix, &item.payload], chunk, low_priority);
            tokio::pin!(write);
            let watchdog = tokio::time::sleep(inner.bounds.starvation_bound);
            tokio::pin!(watchdog);
            loop {
                tokio::select! {
                    biased;
                    () = inner.control.token().cancelled() => break Err(WriteFail::Cancelled),
                    result = &mut write => break result,
                    () = &mut watchdog, if armed => {
                        armed = false;
                        inner.starved(queue_age + inner.bounds.starvation_bound, "writer_blocked");
                    }
                }
            }
        };
        let wait = started.elapsed();
        match result {
            Ok(()) => {
                inner.record_sent(item.payload.len(), wait);
                if let Some(stream) = half.stream() {
                    burst.after_write(inner, stream, item.payload.len());
                }
                if !healthy {
                    healthy = true;
                    inner.shared.mark_healthy(inner.role, inner.lane_generation);
                }
                if armed && queue_age + wait > inner.bounds.starvation_bound {
                    inner.starved(queue_age + wait, "queue_age");
                }
            }
            Err(WriteFail::Cancelled) => {
                return LaneExit::Cancelled(inner.control.code_or_failed());
            }
            Err(WriteFail::Stream { written, error }) => {
                return fail_write(inner, half, written, error);
            }
        }
    }
}

fn fail_write(
    inner: &SenderInner,
    half: &mut SendHalf,
    written: usize,
    error: WriteError,
) -> LaneExit {
    let failure = match error {
        WriteError::ConnectionLost(error) => {
            return LaneExit::ConnectionLost {
                mismatch: classify_peer_close(&error),
            };
        }
        WriteError::Stopped(_) if written > 0 => LaneFailure::PartialFrame,
        WriteError::Stopped(_) => LaneFailure::PeerReset,
        WriteError::ClosedStream | WriteError::ZeroRttRejected if written > 0 => {
            LaneFailure::PartialFrame
        }
        WriteError::ClosedStream | WriteError::ZeroRttRejected => LaneFailure::WriteFailed,
    };
    inner.control.cancel(LaneStreamCode::LaneFailed);
    if failure.requires_reset() {
        // After STOP_SENDING QUIC may already have reset the stream; either
        // way this endpoint never finishes a broken frame.
        half.reset(LaneStreamCode::LaneFailed);
        bump(&inner.stats().resets);
    }
    let action = inner
        .shared
        .report_failure(inner.role, inner.lane_generation, failure);
    LaneExit::Failed { failure, action }
}
