//! One lane's reader, owned by the role's worker.

use std::fmt::{Debug, Formatter};
use std::num::NonZeroU32;
use std::sync::Arc;

use arcen_protocol::lanes::{LaneRole, LaneStreamCode};
use bytes::Bytes;
use quinn::ReadError;

use crate::lanes::{LaneFailure, LaneRecoveryAction, lane_max_frame_bytes};

use super::connection::{Shared, classify_peer_close};
use super::lane::{LaneControl, RecvHalf};
use super::stats::{add, bump};
use super::writer::LaneExit;

/// Reader of one lane. Reads run in the owner's task, so a slow video
/// decoder delays only the video lane.
///
/// [`LaneReceiver::recv_frame`] reads the existing length-prefixed media
/// framing; [`LaneReceiver::read_chunk`] hands raw bytes to an owner that
/// decodes session messages itself. Every failure is reported to the
/// registry once, the stream is stopped, and the lane's writer half (on a
/// bidirectional lane) is reset.
pub struct LaneReceiver {
    shared: Arc<Shared>,
    role: LaneRole,
    lane_generation: NonZeroU32,
    control: Arc<LaneControl>,
    recv: RecvHalf,
    max_frame: usize,
    healthy: bool,
    exit: Option<LaneExit>,
    partial: PartialFrame,
}

/// Bytes of the frame being read, kept across calls so that a cancelled
/// [`LaneReceiver::recv_frame`] loses nothing and the next call resumes.
#[derive(Default)]
struct PartialFrame {
    length: [u8; 4],
    length_filled: usize,
    body: Option<Vec<u8>>,
    body_filled: usize,
}

impl PartialFrame {
    const fn started(&self) -> bool {
        self.length_filled > 0
    }

    /// Reads until a whole frame is buffered. Every await is a single
    /// cancel-safe `read` whose bytes are stored before the next await.
    async fn fill(
        &mut self,
        stream: &mut quinn::RecvStream,
        max_frame: usize,
    ) -> Result<Bytes, ReadFail> {
        while self.length_filled < self.length.len() {
            match stream.read(&mut self.length[self.length_filled..]).await? {
                Some(count) => self.length_filled += count,
                None => {
                    return Err(ReadFail::Finished {
                        mid_frame: self.started(),
                    });
                }
            }
        }
        if self.body.is_none() {
            let length = usize::try_from(u32::from_be_bytes(self.length)).unwrap_or(usize::MAX);
            if length > max_frame {
                return Err(ReadFail::Oversize);
            }
            self.body = Some(vec![0_u8; length]);
            self.body_filled = 0;
        }
        let Some(body) = self.body.as_mut() else {
            return Err(ReadFail::Reset);
        };
        while self.body_filled < body.len() {
            match stream.read(&mut body[self.body_filled..]).await? {
                Some(count) => self.body_filled += count,
                None => return Err(ReadFail::Finished { mid_frame: true }),
            }
        }
        let frame = self.body.take().unwrap_or_default();
        *self = Self::default();
        Ok(Bytes::from(frame))
    }
}

impl Debug for LaneReceiver {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaneReceiver")
            .field("role", &self.role)
            .field("lane_generation", &self.lane_generation)
            .field("exit", &self.exit)
            .finish_non_exhaustive()
    }
}

enum ReadFail {
    Cancelled,
    Finished { mid_frame: bool },
    Reset,
    Lost(quinn::ConnectionError),
    Oversize,
}

impl From<ReadError> for ReadFail {
    fn from(error: ReadError) -> Self {
        match error {
            ReadError::ConnectionLost(error) => Self::Lost(error),
            ReadError::Reset(_)
            | ReadError::ClosedStream
            | ReadError::IllegalOrderedRead
            | ReadError::ZeroRttRejected => Self::Reset,
        }
    }
}

/// Whether a lane cancelled with its attachment may still hand out data:
/// only when the connection itself has closed, since a closed QUIC
/// connection keeps the received bytes readable and never blocks a read.
/// A superseded, disabled or failed lane is never drained.
fn drains_after_close(shared: &Shared, control: &LaneControl) -> bool {
    control.reason() == Some(LaneStreamCode::AttachmentEnded)
        && shared.connection.close_reason().is_some()
}

impl LaneReceiver {
    pub(super) fn new(
        shared: Arc<Shared>,
        role: LaneRole,
        lane_generation: NonZeroU32,
        control: Arc<LaneControl>,
        recv: RecvHalf,
    ) -> Self {
        let max_frame = lane_max_frame_bytes(role, &shared.policy);
        Self {
            shared,
            role,
            lane_generation,
            control,
            recv,
            max_frame,
            healthy: false,
            exit: None,
            partial: PartialFrame::default(),
        }
    }

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

    /// How the reader ended, if it has.
    #[must_use]
    pub const fn exit(&self) -> Option<&LaneExit> {
        self.exit.as_ref()
    }

    /// Reads one length-prefixed frame.
    ///
    /// Cancel-safe: dropping the future (for example in a `select!`) loses
    /// no bytes; a partially received length or body is kept and the next
    /// call resumes the same frame.
    ///
    /// # Errors
    ///
    /// The [`LaneExit`] that ended the lane: a reported failure with its
    /// recovery (peer reset or finish, partial frame, oversize), a
    /// cancellation (superseded, disabled, attachment ended), or connection
    /// loss. Every later call returns the same exit.
    pub async fn recv_frame(&mut self) -> Result<Bytes, LaneExit> {
        if let Some(exit) = &self.exit {
            return Err(exit.clone());
        }
        let result = {
            let Self {
                recv,
                partial,
                control,
                max_frame,
                ..
            } = self;
            let Some(stream) = recv.stream() else {
                return Err(self.cancel_exit());
            };
            let result = tokio::select! {
                biased;
                () = control.token().cancelled() => Err(ReadFail::Cancelled),
                result = partial.fill(stream, *max_frame) => result,
            };
            match result {
                // The connection closed: QUIC still holds what the peer sent
                // and was acknowledged before it closed, and a closed
                // connection never blocks a read, so hand that out first.
                // An orderly close's last records (a refusal, a close
                // reason) arrive this way.
                Err(ReadFail::Cancelled) if drains_after_close(&self.shared, control) => {
                    partial.fill(stream, *max_frame).await
                }
                other => other,
            }
        };
        self.settle(result)
    }

    /// Reads up to `max_bytes` raw bytes, for an owner decoding session
    /// messages itself. Cancel-safe: a dropped call consumes nothing. Do not
    /// mix with [`LaneReceiver::recv_frame`] inside one frame.
    ///
    /// # Errors
    ///
    /// As [`LaneReceiver::recv_frame`]; a clean finish is
    /// [`LaneFailure::PeerFinished`] (the owner's framing decides whether it
    /// ended inside a message and reports with [`LaneReceiver::fail`]).
    pub async fn read_chunk(&mut self, max_bytes: usize) -> Result<Bytes, LaneExit> {
        if let Some(exit) = &self.exit {
            return Err(exit.clone());
        }
        let result = {
            let Some(stream) = self.recv.stream() else {
                return Err(self.cancel_exit());
            };
            let read = async {
                match stream.read_chunk(max_bytes.max(1), true).await {
                    Ok(Some(chunk)) => Ok(chunk.bytes),
                    Ok(None) => Err(ReadFail::Finished { mid_frame: false }),
                    Err(error) => Err(error.into()),
                }
            };
            tokio::pin!(read);
            let result = tokio::select! {
                biased;
                () = self.control.token().cancelled() => Err(ReadFail::Cancelled),
                result = &mut read => result,
            };
            match result {
                Err(ReadFail::Cancelled) if drains_after_close(&self.shared, &self.control) => {
                    read.await
                }
                other => other,
            }
        };
        self.settle(result)
    }

    /// Reports a failure the owner detected in a received frame (malformed,
    /// out of policy), stops the stream, resets the writer half and returns
    /// the lane-scoped recovery.
    pub fn fail(&mut self, failure: LaneFailure) -> LaneRecoveryAction {
        if let Some(LaneExit::Failed { action, .. }) = &self.exit {
            return *action;
        }
        let action = self.fail_inner(failure);
        self.exit = Some(LaneExit::Failed { failure, action });
        action
    }

    /// Token cancelled when this lane is superseded, disabled, fails or its
    /// attachment ends.
    #[must_use]
    pub fn cancellation_token(&self) -> tokio_util::sync::CancellationToken {
        self.control.token().clone()
    }

    /// Resolves when this lane is superseded, fails or its attachment ends.
    pub async fn cancelled(&self) {
        self.control.token().cancelled().await;
    }

    fn fail_inner(&mut self, failure: LaneFailure) -> LaneRecoveryAction {
        self.control.cancel(LaneStreamCode::LaneFailed);
        if self.recv.stop(LaneStreamCode::LaneFailed) {
            bump(&self.shared.stats(self.role).resets);
        }
        self.shared
            .report_failure(self.role, self.lane_generation, failure)
    }

    fn cancelled_exit(&mut self) -> LaneExit {
        let code = self.control.code_or_failed();
        if self.recv.stop(code) {
            bump(&self.shared.stats(self.role).resets);
        }
        // A lost connection also cancels every lane through the dispatcher;
        // report the loss itself, so a peer speaking another transport
        // layout is named rather than seen as an ended attachment.
        if let Some(error) = self.shared.connection.close_reason() {
            return LaneExit::ConnectionLost {
                mismatch: classify_peer_close(&error),
            };
        }
        LaneExit::Cancelled(code)
    }

    fn cancel_exit(&mut self) -> LaneExit {
        let exit = self.cancelled_exit();
        self.exit = Some(exit.clone());
        exit
    }

    fn settle(&mut self, result: Result<Bytes, ReadFail>) -> Result<Bytes, LaneExit> {
        let exit = match result {
            Ok(frame) => {
                let stats = self.shared.stats(self.role);
                bump(&stats.records_received);
                add(&stats.bytes_received, frame.len() as u64);
                if !self.healthy {
                    self.healthy = true;
                    self.shared.mark_healthy(self.role, self.lane_generation);
                }
                return Ok(frame);
            }
            Err(ReadFail::Cancelled) => self.cancelled_exit(),
            Err(ReadFail::Lost(error)) => LaneExit::ConnectionLost {
                mismatch: classify_peer_close(&error),
            },
            Err(failure) => {
                let failure = match failure {
                    ReadFail::Finished { mid_frame: true } => LaneFailure::PartialFrame,
                    ReadFail::Finished { mid_frame: false } => LaneFailure::PeerFinished,
                    ReadFail::Oversize => LaneFailure::Oversize,
                    _ => LaneFailure::PeerReset,
                };
                let action = self.fail_inner(failure);
                LaneExit::Failed { failure, action }
            }
        };
        self.exit = Some(exit.clone());
        Err(exit)
    }
}
