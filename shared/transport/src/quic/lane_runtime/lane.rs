//! One admitted or opened lane: its generation, cancellation and the two
//! stream halves, each of which resets (never finishes) on drop unless the
//! lane closed cleanly.

use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::num::{NonZeroU32, NonZeroU64};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::task::{Context, Poll};

use arcen_protocol::lanes::{LaneRole, LaneShape, LaneStreamCode};
use quinn::{RecvStream, SendStream, VarInt};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};

use crate::lanes::{LaneFailure, LaneRecoveryAction, SupersededLane};

use super::LaneQueueBounds;
use super::connection::Shared;
use super::reader::LaneReceiver;
use super::stats::bump;
use super::writer::LaneSender;

/// Cancellation shared by a lane's halves. The first cause wins and picks
/// the code both halves reset or stop with.
pub(super) struct LaneControl {
    token: CancellationToken,
    reason: AtomicU8,
}

impl LaneControl {
    pub(super) fn new(token: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            token,
            reason: AtomicU8::new(0),
        })
    }

    pub(super) fn cancel(&self, code: LaneStreamCode) {
        let encoded = LaneStreamCode::ALL
            .iter()
            .position(|candidate| *candidate == code)
            .and_then(|index| u8::try_from(index + 1).ok())
            .unwrap_or(0);
        let _ = self
            .reason
            .compare_exchange(0, encoded, Ordering::AcqRel, Ordering::Acquire);
        self.token.cancel();
    }

    /// The code the halves use; a lane cancelled through its parent token
    /// (connection shutdown) reports the attachment as ended.
    pub(super) fn reason(&self) -> Option<LaneStreamCode> {
        match self.reason.load(Ordering::Acquire) {
            0 if self.token.is_cancelled() => Some(LaneStreamCode::AttachmentEnded),
            0 => None,
            encoded => LaneStreamCode::ALL.get(usize::from(encoded) - 1).copied(),
        }
    }

    pub(super) fn code_or_failed(&self) -> LaneStreamCode {
        self.reason().unwrap_or(LaneStreamCode::LaneFailed)
    }

    pub(super) const fn token(&self) -> &CancellationToken {
        &self.token
    }
}

pub(super) fn varint(code: LaneStreamCode) -> VarInt {
    VarInt::from_u32(code.code())
}

/// Send half that resets on drop unless finished.
pub(super) struct SendHalf {
    stream: Option<SendStream>,
    control: Arc<LaneControl>,
}

impl SendHalf {
    pub(super) const fn new(stream: SendStream, control: Arc<LaneControl>) -> Self {
        Self {
            stream: Some(stream),
            control,
        }
    }

    pub(super) const fn stream(&mut self) -> Option<&mut SendStream> {
        self.stream.as_mut()
    }

    /// Resets the stream; `true` when this call reset it.
    pub(super) fn reset(&mut self, code: LaneStreamCode) -> bool {
        self.stream
            .take()
            .is_some_and(|mut stream| stream.reset(varint(code)).is_ok())
    }

    /// Finishes the stream and hands it back, so the caller can wait for
    /// the peer to acknowledge it ([`SendStream::stopped`]).
    pub(super) fn finish(&mut self) -> Option<SendStream> {
        let mut stream = self.stream.take()?;
        stream.finish().ok().map(|()| stream)
    }
}

impl Drop for SendHalf {
    fn drop(&mut self) {
        let code = self.control.code_or_failed();
        self.reset(code);
    }
}

/// Receive half that stops the peer's sending on drop.
pub(super) struct RecvHalf {
    stream: Option<RecvStream>,
    control: Arc<LaneControl>,
}

impl RecvHalf {
    pub(super) const fn new(stream: RecvStream, control: Arc<LaneControl>) -> Self {
        Self {
            stream: Some(stream),
            control,
        }
    }

    pub(super) const fn stream(&mut self) -> Option<&mut RecvStream> {
        self.stream.as_mut()
    }

    /// Stops the stream; `true` when this call stopped it.
    pub(super) fn stop(&mut self, code: LaneStreamCode) -> bool {
        self.stream
            .take()
            .is_some_and(|mut stream| stream.stop(varint(code)).is_ok())
    }
}

impl Drop for RecvHalf {
    fn drop(&mut self) {
        let code = self.control.code_or_failed();
        self.stop(code);
    }
}

/// A lane without its runtime handle, as stored while awaiting its owner.
pub(super) struct LaneParts {
    pub(super) role: LaneRole,
    pub(super) lane_generation: NonZeroU32,
    pub(super) attachment_generation: Option<NonZeroU64>,
    pub(super) superseded: Option<SupersededLane>,
    pub(super) control: Arc<LaneControl>,
    pub(super) send: Option<SendHalf>,
    pub(super) recv: Option<RecvHalf>,
}

/// One lane, opened locally or admitted from the peer, before its owner
/// splits it into a writer and reader.
///
/// Dropping a lane resets its streams; it never finishes them, so the peer
/// cannot mistake an abandoned lane for a clean end.
pub struct Lane {
    shared: Arc<Shared>,
    parts: LaneParts,
}

impl Debug for Lane {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Lane")
            .field("role", &self.parts.role)
            .field("lane_generation", &self.parts.lane_generation)
            .field("attachment_generation", &self.parts.attachment_generation)
            .field("superseded", &self.parts.superseded)
            .finish_non_exhaustive()
    }
}

impl Lane {
    pub(super) const fn new(shared: Arc<Shared>, parts: LaneParts) -> Self {
        Self { shared, parts }
    }

    /// Role.
    #[must_use]
    pub const fn role(&self) -> LaneRole {
        self.parts.role
    }

    /// Lane generation.
    #[must_use]
    pub const fn lane_generation(&self) -> NonZeroU32 {
        self.parts.lane_generation
    }

    /// Attachment generation; `None` only for the control lane.
    #[must_use]
    pub const fn attachment_generation(&self) -> Option<NonZeroU64> {
        self.parts.attachment_generation
    }

    /// The older lane of this role this one replaced, if any. The owner
    /// must apply its steps — release held input, request a keyframe —
    /// before this lane carries payload. The old lane is already cancelled:
    /// its sender, receiver or duplex resets and stops its streams and
    /// delivers nothing further.
    #[must_use]
    pub const fn superseded(&self) -> Option<SupersededLane> {
        self.parts.superseded
    }

    /// Whether a newer generation, a failure or the attachment's end has
    /// already cancelled this lane.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.parts.control.token().is_cancelled()
    }

    /// Splits the lane into its own bounded writer (a dedicated task with a
    /// `bounds`-sized queue) and its reader. A half is `None` when the local
    /// endpoint does not send, or does not receive, on this role.
    ///
    /// Must be called inside a Tokio runtime.
    #[must_use]
    pub fn split(mut self, bounds: LaneQueueBounds) -> (Option<LaneSender>, Option<LaneReceiver>) {
        let shared = Arc::clone(&self.shared);
        let parts = &mut self.parts;
        let sender = parts.send.take().map(|half| {
            LaneSender::spawn(
                Arc::clone(&shared),
                parts.role,
                parts.lane_generation,
                Arc::clone(&parts.control),
                half,
                bounds,
            )
        });
        let receiver = parts.recv.take().map(|half| {
            LaneReceiver::new(
                Arc::clone(&shared),
                parts.role,
                parts.lane_generation,
                Arc::clone(&parts.control),
                half,
            )
        });
        (sender, receiver)
    }

    /// Splits with [`LaneQueueBounds::for_role`].
    #[must_use]
    pub fn split_default(self) -> (Option<LaneSender>, Option<LaneReceiver>) {
        let bounds = LaneQueueBounds::for_role(self.parts.role);
        self.split(bounds)
    }

    /// A bidirectional lane as one `AsyncRead`/`AsyncWrite` byte stream, for
    /// an owner that runs its existing session-message framing (for example
    /// a WebSocket codec) directly on the lane in its own task.
    ///
    /// # Errors
    ///
    /// Returns the lane unchanged when it is unidirectional.
    pub fn into_duplex(mut self) -> Result<LaneDuplex, Self> {
        if self.parts.role.shape() != LaneShape::Bidirectional
            || self.parts.send.is_none()
            || self.parts.recv.is_none()
        {
            return Err(self);
        }
        let (Some(send), Some(recv)) = (self.parts.send.take(), self.parts.recv.take()) else {
            return Err(self);
        };
        let token = self.parts.control.token();
        let read_cancelled = Box::pin(token.clone().cancelled_owned());
        let write_cancelled = Box::pin(token.clone().cancelled_owned());
        Ok(LaneDuplex {
            shared: Arc::clone(&self.shared),
            role: self.parts.role,
            lane_generation: self.parts.lane_generation,
            control: Arc::clone(&self.parts.control),
            read_cancelled,
            write_cancelled,
            closed: None,
            healthy: AtomicBool::new(false),
            send,
            recv,
        })
    }

    /// Token cancelled when this lane is superseded, disabled, fails or its
    /// attachment ends, for an owner that `select!`s on it.
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.parts.control.token().clone()
    }
}

/// A bidirectional lane as a byte stream. See [`Lane::into_duplex`].
///
/// The duplex honours lane cancellation: once the lane is superseded,
/// disabled, fails or its attachment ends, the next read, write, flush or
/// shutdown resets and stops both directions with the cancellation's
/// [`LaneStreamCode`] and fails with [`std::io::ErrorKind::ConnectionAborted`];
/// a pending read or write is woken for it, even when the two directions are
/// polled from different tasks. A superseded input duplex can
/// therefore never deliver a stale key-down after its replacement released
/// held input. Failures the owner detects in its framing are reported with
/// [`LaneDuplex::fail`]; dropping the duplex resets both directions.
pub struct LaneDuplex {
    shared: Arc<Shared>,
    role: LaneRole,
    lane_generation: NonZeroU32,
    control: Arc<LaneControl>,
    /// One cancellation registration per direction: a registration keeps a
    /// single waker, and the halves may be polled from different tasks
    /// (`tokio::io::split`), so each direction wakes its own task.
    read_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    write_cancelled: Pin<Box<WaitForCancellationFutureOwned>>,
    closed: Option<LaneStreamCode>,
    healthy: AtomicBool,
    send: SendHalf,
    recv: RecvHalf,
}

impl Debug for LaneDuplex {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaneDuplex")
            .field("role", &self.role)
            .field("lane_generation", &self.lane_generation)
            .field("closed", &self.closed)
            .finish_non_exhaustive()
    }
}

impl LaneDuplex {
    /// Role.
    #[must_use]
    pub const fn role(&self) -> LaneRole {
        self.role
    }

    /// Lane generation.
    #[must_use]
    pub const fn lane_generation(&self) -> NonZeroU32 {
        self.lane_generation
    }

    /// Token cancelled when the lane is superseded, disabled, fails or its
    /// attachment ends. Cloneable, so a codec owner can `select!` on it
    /// while the duplex is borrowed by its framing.
    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.control.token().clone()
    }

    /// Resolves when the lane is superseded, fails or its attachment ends.
    pub async fn cancelled(&self) {
        self.control.token().cancelled().await;
    }

    /// Why the lane was cancelled, if it was.
    #[must_use]
    pub fn cancel_reason(&self) -> Option<LaneStreamCode> {
        self.control.reason()
    }

    /// Reports a failure the owner detected (malformed or oversize session
    /// frame, write failure), resets both directions and returns the
    /// registry's lane-scoped recovery.
    pub fn fail(&mut self, failure: LaneFailure) -> LaneRecoveryAction {
        self.control.cancel(LaneStreamCode::LaneFailed);
        self.close_halves(LaneStreamCode::LaneFailed);
        self.shared
            .report_failure(self.role, self.lane_generation, failure)
    }

    /// Finishes the send direction cleanly, for a normal session close.
    pub fn finish(&mut self) {
        let _ = self.send.finish();
    }

    fn close_halves(&mut self, code: LaneStreamCode) {
        if self.closed.is_some() {
            return;
        }
        self.closed = Some(code);
        let stats = self.shared.stats(self.role);
        if self.send.reset(code) {
            bump(&stats.resets);
        }
        if self.recv.stop(code) {
            bump(&stats.resets);
        }
    }

    /// Closes both halves once the lane is cancelled; `Err` then, and for
    /// every later call. Registers `context`'s waker with the cancellation
    /// of the polled direction only.
    fn check_cancelled(
        &mut self,
        direction: Direction,
        context: &mut Context<'_>,
    ) -> std::io::Result<()> {
        if self.closed.is_none() {
            let registration = match direction {
                Direction::Read => self.read_cancelled.as_mut(),
                Direction::Write => self.write_cancelled.as_mut(),
            };
            if registration.poll(context).is_ready() {
                let code = self.control.code_or_failed();
                self.close_halves(code);
            }
        }
        match self.closed {
            Some(code) => Err(cancelled_lane(code)),
            None => Ok(()),
        }
    }

    fn mark_healthy_once(&self) {
        if !self.healthy.swap(true, Ordering::Relaxed) {
            self.shared.mark_healthy(self.role, self.lane_generation);
        }
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Read,
    Write,
}

fn cancelled_lane(code: LaneStreamCode) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::ConnectionAborted,
        format!("lane closed: {}", code.token()),
    )
}

impl AsyncRead for LaneDuplex {
    fn poll_read(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(Direction::Read, context) {
            return Poll::Ready(Err(error));
        }
        let before = buffer.filled().len();
        let Some(stream) = this.recv.stream() else {
            return Poll::Ready(Err(cancelled_lane(LaneStreamCode::LaneFailed)));
        };
        let result = Pin::new(stream).poll_read(context, buffer);
        if matches!(result, Poll::Ready(Ok(()))) && buffer.filled().len() > before {
            this.mark_healthy_once();
        }
        result
    }
}

impl AsyncWrite for LaneDuplex {
    fn poll_write(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, std::io::Error>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(Direction::Write, context) {
            return Poll::Ready(Err(error));
        }
        let Some(stream) = this.send.stream() else {
            return Poll::Ready(Err(cancelled_lane(LaneStreamCode::LaneFailed)));
        };
        let result = AsyncWrite::poll_write(Pin::new(stream), context, buffer);
        if matches!(result, Poll::Ready(Ok(written)) if written > 0) {
            this.mark_healthy_once();
        }
        result
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(Direction::Write, context) {
            return Poll::Ready(Err(error));
        }
        let Some(stream) = this.send.stream() else {
            return Poll::Ready(Err(cancelled_lane(LaneStreamCode::LaneFailed)));
        };
        AsyncWrite::poll_flush(Pin::new(stream), context)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), std::io::Error>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(Direction::Write, context) {
            return Poll::Ready(Err(error));
        }
        let Some(stream) = this.send.stream() else {
            return Poll::Ready(Ok(()));
        };
        AsyncWrite::poll_shutdown(Pin::new(stream), context)
    }
}
