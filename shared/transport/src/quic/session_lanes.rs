//! Product-facing lane session adapter (ADR 0014, `wlp-workers`).
//!
//! The lane runtime ([`super::LaneConnection`]) moves bytes; this module is
//! the small layer every Pier and Deck uses on top of it, so that each
//! product stays a thin adapter:
//!
//! - **Which message goes where.** Session messages keep their existing
//!   JSON and binary shapes and travel as [`SessionRecord`]s on the control,
//!   input/cursor or bulk lane chosen by
//!   [`arcen_protocol::lanes::lane_for_session_text`] and
//!   [`arcen_protocol::lanes::lane_for_binary_message`]; media frames travel
//!   on the video, audio-output and microphone lanes
//!   ([`LaneRole::for_frame_type`]).
//! - **Establishing the carrier.** [`connect_session_lanes`] (Deck) and
//!   [`accept_session_lanes`] (Pier) start the lane runtime on a QUIC
//!   connection, keep its send window interactive and open or accept the
//!   control lane.
//! - **Authorizing the attachment.** [`grant_attachment`] (Pier, after
//!   authentication) and [`accept_attachment`] (Deck, on `lane_attachment`).
//! - **Reading and writing session records.** [`SessionLaneReader`] and
//!   [`session_record_bytes`].
//! - **Recovering a lane.** [`follow_lane_exit`] turns a lane's exit into
//!   the one step its owner takes under
//!   [`crate::lanes::LaneRecoveryPolicy`]; no step ever moves a role onto
//!   another role's lane.
//! - **Handing the current lane to producers.** [`LaneSenderSlot`] holds the
//!   newest sender of a role across reopenings.

use std::num::{NonZeroU32, NonZeroU64};
use std::time::Duration;

use arcen_protocol::lanes::{
    LaneAttachment, LaneAttachmentBinding, LaneAttachmentMsg, LaneContractError, LaneEndpoint,
    LaneRole, LaneRoleSet, PeerTransportMismatch, SESSION_CLOSED_CLOSE_CODE,
    SESSION_CLOSED_CLOSE_REASON, SessionRecord, SessionRecordError,
};
use bytes::Bytes;
use quinn::{Connection, ConnectionError, VarInt};
use tokio::sync::watch;

use super::{
    DirectQuicDialParams, Lane, LaneConnection, LaneExit, LaneOpenFailure, LaneQueueBounds,
    LaneReceiver, LaneRuntimeConfig, LaneSender, LaneWaitError, QuicTransportError,
    keep_send_window_interactive,
};
use crate::lanes::{
    LaneEndReason, LaneFailure, LaneRecoveryAction, LaneRegistryError, SupersededLane,
};

/// Encodes a session record as one lane record (length prefix included).
#[must_use]
pub fn session_record_bytes(record: &SessionRecord) -> Bytes {
    Bytes::from(record.encode())
}

/// Reader of a session-message lane (control, input/cursor or bulk).
///
/// [`SessionLaneReader::next_record`] is cancel-safe: dropping it inside a
/// `select!` loses no bytes. A record that does not decode fails the lane
/// with [`LaneFailure::MalformedFrame`], so the stream is reset and the
/// role's recovery applies; nothing after a broken record is read.
#[derive(Debug)]
pub struct SessionLaneReader {
    receiver: LaneReceiver,
}

impl SessionLaneReader {
    /// Wraps the receive half of a session-message lane.
    #[must_use]
    pub const fn new(receiver: LaneReceiver) -> Self {
        Self { receiver }
    }

    /// Role.
    #[must_use]
    pub const fn role(&self) -> LaneRole {
        self.receiver.role()
    }

    /// Lane generation.
    #[must_use]
    pub const fn lane_generation(&self) -> NonZeroU32 {
        self.receiver.lane_generation()
    }

    /// Reads the next session record.
    ///
    /// # Errors
    ///
    /// The lane's [`LaneExit`]; a record that fails to decode is reported as
    /// [`LaneFailure::MalformedFrame`] first.
    pub async fn next_record(&mut self) -> Result<SessionRecord, LaneExit> {
        let body = self.receiver.recv_frame().await?;
        SessionRecord::decode_body(&body).map_err(|_: SessionRecordError| {
            let failure = LaneFailure::MalformedFrame;
            let action = self.receiver.fail(failure);
            LaneExit::Failed { failure, action }
        })
    }

    /// Reports a failure the owner found in a decoded record (for example a
    /// message that is illegal on this lane) and returns the recovery.
    pub fn fail(&mut self, failure: LaneFailure) -> LaneRecoveryAction {
        self.receiver.fail(failure)
    }

    /// Resolves when the lane is superseded, fails or its attachment ends.
    pub async fn cancelled(&self) {
        self.receiver.cancelled().await;
    }
}

/// One session-message lane split into its halves, with the supersession
/// steps its owner still owes.
#[derive(Debug)]
pub struct SessionLane {
    /// Role.
    pub role: LaneRole,
    /// Lane generation.
    pub lane_generation: NonZeroU32,
    /// The older lane of this role this one replaced. Apply its steps
    /// (release held input, request a keyframe) before using this lane.
    pub superseded: Option<SupersededLane>,
    /// Bounded writer.
    pub sender: LaneSender,
    /// Reader.
    pub reader: SessionLaneReader,
}

impl SessionLane {
    /// Splits a session-message lane with [`LaneQueueBounds::for_role`].
    ///
    /// # Errors
    ///
    /// Returns the lane unchanged when it is a media lane.
    pub fn split(lane: Lane) -> Result<Self, Lane> {
        if lane.role().framing() != arcen_protocol::lanes::LaneFraming::SessionMessages {
            return Err(lane);
        }
        let role = lane.role();
        let lane_generation = lane.lane_generation();
        let superseded = lane.superseded();
        let (sender, receiver) = lane.split(LaneQueueBounds::for_role(role));
        match (sender, receiver) {
            (Some(sender), Some(receiver)) => Ok(Self {
                role,
                lane_generation,
                superseded,
                sender,
                reader: SessionLaneReader::new(receiver),
            }),
            // Session-message lanes are bidirectional, so both halves exist.
            _ => unreachable!("a session-message lane is bidirectional"),
        }
    }
}

fn split_control(lane: Lane) -> SessionLane {
    match SessionLane::split(lane) {
        Ok(control) => control,
        Err(_) => unreachable!("the control lane carries session messages"),
    }
}

/// Why the lane carrier could not be established.
#[derive(Debug)]
pub enum SessionLaneSetupError {
    /// The QUIC connection could not be made.
    Connect(QuicTransportError),
    /// The peer runs another transport layout; both sides print
    /// [`PeerTransportMismatch::message`].
    Mismatch(PeerTransportMismatch),
    /// The control lane could not be opened.
    Open(LaneOpenFailure),
    /// The peer did not open its control lane in time.
    Timeout,
    /// The attachment ended before the control lane arrived.
    Ended(LaneWaitError),
}

impl std::fmt::Display for SessionLaneSetupError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Connect(error) => write!(formatter, "{error}"),
            Self::Mismatch(mismatch) => write!(formatter, "{mismatch}"),
            Self::Open(error) => write!(formatter, "{error}"),
            Self::Timeout => formatter.write_str("the control lane did not arrive in time"),
            Self::Ended(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for SessionLaneSetupError {}

/// Deck: dials the Pier, starts the lane runtime, keeps the connection's
/// send window interactive (the Deck's equivalent of the Pier's
/// [`keep_send_window_interactive`], so bulk clipboard bytes cannot sit in
/// QUIC ahead of input and microphone frames) and opens the control lane.
///
/// The endpoint in `params` is kept alive by the returned connection's
/// endpoint handle; the caller keeps `params.endpoint` (cloned) for as long
/// as the session runs.
///
/// # Errors
///
/// [`SessionLaneSetupError`].
pub async fn connect_session_lanes(
    params: DirectQuicDialParams<'_>,
    config: LaneRuntimeConfig,
) -> Result<(LaneConnection, SessionLane), SessionLaneSetupError> {
    let connecting = params
        .endpoint
        .connect_with(params.client_config, params.remote_addr, params.server_name)
        .map_err(|error| SessionLaneSetupError::Connect(QuicTransportError::Connect(error)))?;
    let connection = connecting
        .await
        .map_err(|error| SessionLaneSetupError::Connect(QuicTransportError::Connection(error)))?;
    tokio::spawn(keep_send_window_interactive(connection.clone()));
    let lanes = LaneConnection::start(connection, config);
    let control = match lanes.open_lane(LaneRole::Control).await {
        Ok(lane) => lane,
        Err(LaneOpenFailure::Stream {
            mismatch: Some(mismatch),
            ..
        }) => return Err(SessionLaneSetupError::Mismatch(mismatch)),
        Err(error) => return Err(SessionLaneSetupError::Open(error)),
    };
    Ok((lanes, split_control(control)))
}

/// Pier: starts the lane runtime on an accepted connection, keeps its send
/// window interactive and waits up to `control_timeout` for the Deck's
/// control lane. A pre-lane Deck is told why on its own stream and closed
/// with the version-mismatch code; this returns
/// [`SessionLaneSetupError::Mismatch`] for it, which the Pier logs.
///
/// # Errors
///
/// [`SessionLaneSetupError::Timeout`], [`SessionLaneSetupError::Mismatch`]
/// or [`SessionLaneSetupError::Ended`].
pub async fn accept_session_lanes(
    connection: Connection,
    config: LaneRuntimeConfig,
    control_timeout: Duration,
) -> Result<(LaneConnection, SessionLane), SessionLaneSetupError> {
    tokio::spawn(keep_send_window_interactive(connection.clone()));
    let lanes = LaneConnection::start(connection, config);
    let lane = tokio::time::timeout(control_timeout, lanes.next_peer_lane(LaneRole::Control))
        .await
        .map_err(|_| SessionLaneSetupError::Timeout)?
        .map_err(|error| {
            lanes.local_mismatch().map_or(
                SessionLaneSetupError::Ended(error),
                SessionLaneSetupError::Mismatch,
            )
        })?;
    Ok((lanes, split_control(lane)))
}

/// Why an attachment grant could not be issued or accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneGrantError {
    /// The grant itself is invalid (or the OS random source failed).
    Contract(LaneContractError),
    /// The registry refused (control lane not open, already authorized,
    /// attachment ended).
    Registry(LaneRegistryError),
}

impl std::fmt::Display for LaneGrantError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Contract(error) => write!(formatter, "{error}"),
            Self::Registry(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for LaneGrantError {}

/// Attachment generations issued by this process. Monotonic across every
/// connection, so a stale lane of an earlier attachment can never match.
static ATTACHMENT_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Pier: the next attachment generation of this process (never zero).
#[must_use]
pub fn next_attachment_generation() -> NonZeroU64 {
    let next = ATTACHMENT_GENERATION
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        .saturating_add(1);
    NonZeroU64::new(next).unwrap_or(NonZeroU64::MIN)
}

/// Pier: authorizes the attachment after authentication with a fresh random
/// binding and returns the `lane_attachment` message to send on the control
/// lane. Send it before opening the video and audio lanes.
///
/// # Errors
///
/// [`LaneGrantError`].
pub fn grant_attachment(
    lanes: &LaneConnection,
    generation: NonZeroU64,
    roles: LaneRoleSet,
) -> Result<LaneAttachmentMsg, LaneGrantError> {
    debug_assert_eq!(lanes.local(), LaneEndpoint::Pier);
    let attachment = LaneAttachment {
        generation,
        binding: LaneAttachmentBinding::generate().map_err(LaneGrantError::Contract)?,
    };
    lanes
        .authorize(attachment, roles)
        .map_err(LaneGrantError::Registry)?;
    Ok(LaneAttachmentMsg::new(attachment, roles))
}

/// Deck: validates a received `lane_attachment` and authorizes the
/// attachment; lanes the Pier opened early are validated in full and become
/// available through [`LaneConnection::next_peer_lane`]. Returns the granted
/// roles.
///
/// # Errors
///
/// [`LaneGrantError`].
pub fn accept_attachment(
    lanes: &LaneConnection,
    message: &LaneAttachmentMsg,
) -> Result<LaneRoleSet, LaneGrantError> {
    let (attachment, roles) = message.validate().map_err(LaneGrantError::Contract)?;
    lanes
        .authorize(attachment, roles)
        .map_err(LaneGrantError::Registry)?;
    Ok(roles)
}

/// The one step a lane owner takes after its lane ended.
#[derive(Debug)]
pub enum LaneRecoveryStep {
    /// The local endpoint reopened the role; continue on this lane. Apply
    /// `request_keyframe` / `release_held_input` first.
    Reopened {
        /// Replacement lane.
        lane: Lane,
        /// The replacement must start a new prediction chain.
        request_keyframe: bool,
        /// Release held keys, buttons and pen state before input resumes.
        release_held_input: bool,
    },
    /// The peer reopens the role; wait for its next lane with
    /// [`LaneConnection::next_peer_lane`]. Apply the flags first.
    AwaitPeer {
        /// The replacement must start a new prediction chain.
        request_keyframe: bool,
        /// Release held keys, buttons and pen state now.
        release_held_input: bool,
    },
    /// A newer generation replaced this lane; the owner of the newer lane
    /// applies its [`SupersededLane`] steps. A peer-opened role waits for
    /// that lane; a locally opened one is already running it.
    Superseded,
    /// The role is disabled for the rest of the attachment: tell the user.
    Disabled(LaneRole),
    /// The attachment ended; the existing reconnect state machine decides
    /// what happens next. Held input must be released.
    Ended(LaneEndReason),
    /// The QUIC connection was lost.
    ConnectionLost(Option<PeerTransportMismatch>),
    /// The owner finished the lane cleanly.
    Finished,
}

/// Turns a lane exit into its owner's next step, reopening the lane when
/// the local endpoint owns the role. A reopen that itself fails is followed
/// the same way until a lane is open or the role reaches a terminal
/// outcome, so the recovery budget, not this loop, bounds the attempts.
pub async fn follow_lane_exit(lanes: &LaneConnection, exit: &LaneExit) -> LaneRecoveryStep {
    let mut action = match exit {
        LaneExit::Finished => return LaneRecoveryStep::Finished,
        LaneExit::ConnectionLost { mismatch } => {
            return LaneRecoveryStep::ConnectionLost(*mismatch);
        }
        LaneExit::Cancelled(_) => {
            return lanes
                .ended()
                .map_or(LaneRecoveryStep::Superseded, LaneRecoveryStep::Ended);
        }
        LaneExit::Failed { action, .. } => *action,
    };
    loop {
        match action {
            LaneRecoveryAction::Superseded => return LaneRecoveryStep::Superseded,
            LaneRecoveryAction::Disable { role, .. } => return LaneRecoveryStep::Disabled(role),
            LaneRecoveryAction::EndAttachment { reason, .. } => {
                return LaneRecoveryStep::Ended(reason);
            }
            LaneRecoveryAction::Reopen {
                role,
                reopened_by,
                request_keyframe,
                release_held_input,
                ..
            } => {
                if reopened_by != lanes.local() {
                    return LaneRecoveryStep::AwaitPeer {
                        request_keyframe,
                        release_held_input,
                    };
                }
                match lanes.open_lane(role).await {
                    Ok(lane) => {
                        return LaneRecoveryStep::Reopened {
                            lane,
                            request_keyframe,
                            release_held_input,
                        };
                    }
                    Err(LaneOpenFailure::Stream {
                        mismatch: Some(mismatch),
                        ..
                    }) => return LaneRecoveryStep::ConnectionLost(Some(mismatch)),
                    Err(LaneOpenFailure::Stream { action: next, .. }) => action = next,
                    Err(LaneOpenFailure::Registry(_)) => {
                        return lanes
                            .ended()
                            .map_or(LaneRecoveryStep::Disabled(role), LaneRecoveryStep::Ended);
                    }
                }
            }
        }
    }
}

/// The newest sender of one role, shared with every producer of that role.
///
/// The lane's owner installs each generation's sender as it opens or
/// accepts it and clears it when the role is disabled or the attachment
/// ends. A producer takes [`LaneSenderSlot::current`] for each record, or
/// waits for one with [`LaneSenderSlot::wait`]; it never falls back to
/// another role's slot.
#[derive(Debug, Clone)]
pub struct LaneSenderSlot {
    role: LaneRole,
    current: watch::Sender<Option<LaneSender>>,
}

impl LaneSenderSlot {
    /// An empty slot for `role`.
    #[must_use]
    pub fn new(role: LaneRole) -> Self {
        Self {
            role,
            current: watch::Sender::new(None),
        }
    }

    /// Role.
    #[must_use]
    pub const fn role(&self) -> LaneRole {
        self.role
    }

    /// Installs the sender of a newly opened generation.
    pub fn install(&self, sender: LaneSender) {
        debug_assert_eq!(sender.role(), self.role);
        self.current.send_replace(Some(sender));
    }

    /// Removes the sender (role disabled, attachment ended).
    pub fn clear(&self) {
        self.current.send_replace(None);
    }

    /// The current sender, if a lane is open.
    #[must_use]
    pub fn current(&self) -> Option<LaneSender> {
        self.current.borrow().clone()
    }

    /// Waits until a sender is installed.
    pub async fn wait(&self) -> LaneSender {
        let mut receiver = self.current.subscribe();
        loop {
            if let Some(sender) = receiver.borrow_and_update().clone() {
                return sender;
            }
            if receiver.changed().await.is_err() {
                // The slot owns the sender half, so this cannot happen while
                // `self` is borrowed.
                std::future::pending::<()>().await;
            }
        }
    }
}

/// Why a lane owner stops serving its role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneTerminal {
    /// The role is disabled for the rest of the attachment: tell the user.
    Disabled(LaneRole),
    /// The attachment ended; the session's reconnect policy applies.
    Ended(LaneEndReason),
    /// The QUIC connection was lost.
    ConnectionLost(Option<PeerTransportMismatch>),
    /// The lane was finished cleanly.
    Finished,
}

impl std::fmt::Display for LaneTerminal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disabled(role) => write!(formatter, "{role} lane disabled"),
            Self::Ended(reason) => write!(formatter, "lane attachment ended: {}", reason.token()),
            Self::ConnectionLost(Some(mismatch)) => write!(formatter, "{mismatch}"),
            Self::ConnectionLost(None) => formatter.write_str("QUIC connection lost"),
            Self::Finished => formatter.write_str("lane finished"),
        }
    }
}

impl std::error::Error for LaneTerminal {}

/// What a write did besides queueing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneWriteOutcome {
    /// The record was queued on the open lane.
    Queued,
    /// [`LocalLaneWriter::try_send`] found the role's queue full and dropped
    /// the record (media only; the caller counts it).
    DroppedFull,
    /// The lane had failed and was reopened before this record was queued
    /// on the replacement. Records queued on the failed lane were discarded
    /// and are not resent. Apply the steps before the next record.
    Reopened {
        /// Start a new prediction chain (video).
        request_keyframe: bool,
    },
}

/// Writer of a media lane the local endpoint opens (Pier video and audio
/// output; Deck microphone): one bounded queue and writer task per role,
/// reopened with a newer generation under the role's recovery policy. It
/// never writes to another role's lane.
#[derive(Debug)]
pub struct LocalLaneWriter {
    lanes: LaneConnection,
    role: LaneRole,
    bounds: LaneQueueBounds,
    sender: LaneSender,
}

impl LocalLaneWriter {
    /// Opens `role` with [`LaneQueueBounds::for_role`].
    ///
    /// # Errors
    ///
    /// [`LaneTerminal`] when the role cannot be opened within its recovery
    /// budget.
    pub async fn open(lanes: &LaneConnection, role: LaneRole) -> Result<Self, LaneTerminal> {
        Self::open_with(lanes, role, LaneQueueBounds::for_role(role)).await
    }

    /// Opens `role` with explicit queue bounds.
    ///
    /// # Errors
    ///
    /// As [`LocalLaneWriter::open`].
    pub async fn open_with(
        lanes: &LaneConnection,
        role: LaneRole,
        bounds: LaneQueueBounds,
    ) -> Result<Self, LaneTerminal> {
        let lane = match lanes.open_lane(role).await {
            Ok(lane) => lane,
            Err(LaneOpenFailure::Stream { action, .. }) => {
                let exit = LaneExit::Failed {
                    failure: LaneFailure::WriteFailed,
                    action,
                };
                match follow_lane_exit(lanes, &exit).await {
                    LaneRecoveryStep::Reopened { lane, .. } => lane,
                    step => return Err(terminal_of(lanes, role, &step)),
                }
            }
            Err(LaneOpenFailure::Registry(_)) => {
                return Err(lanes
                    .ended()
                    .map_or(LaneTerminal::Disabled(role), LaneTerminal::Ended));
            }
        };
        let sender = Self::split_sender(lane, bounds, role)?;
        Ok(Self {
            lanes: lanes.clone(),
            role,
            bounds,
            sender,
        })
    }

    fn split_sender(
        lane: Lane,
        bounds: LaneQueueBounds,
        role: LaneRole,
    ) -> Result<LaneSender, LaneTerminal> {
        lane.split(bounds).0.ok_or(LaneTerminal::Disabled(role))
    }

    /// Role.
    #[must_use]
    pub const fn role(&self) -> LaneRole {
        self.role
    }

    /// The current lane's sender (for status and queue depth).
    #[must_use]
    pub const fn sender(&self) -> &LaneSender {
        &self.sender
    }

    /// Queues `record`, waiting only for room in this role's queue. If the
    /// lane has failed, follows its recovery first and queues the record on
    /// the replacement.
    ///
    /// Not cancel-safe for the record: a dropped call drops it.
    ///
    /// # Errors
    ///
    /// [`LaneTerminal`] once the role cannot continue.
    pub async fn send(&mut self, record: Bytes) -> Result<LaneWriteOutcome, LaneTerminal> {
        let mut outcome = LaneWriteOutcome::Queued;
        loop {
            match self.sender.send(record.clone()).await {
                Ok(()) => return Ok(outcome),
                Err(super::LaneSendError::Oversize { .. }) => {
                    return Err(LaneTerminal::Disabled(self.role));
                }
                Err(_) => outcome = self.recover().await?,
            }
        }
    }

    /// Queues `record` without waiting; a full queue drops it and reports
    /// [`LaneWriteOutcome::DroppedFull`]. For media whose newest frame is
    /// worth more than an old one (audio), never for input edges.
    ///
    /// # Errors
    ///
    /// [`LaneTerminal`] once the role cannot continue.
    pub async fn try_send(&mut self, record: Bytes) -> Result<LaneWriteOutcome, LaneTerminal> {
        let mut outcome = LaneWriteOutcome::Queued;
        loop {
            match self.sender.try_send(record.clone()) {
                Ok(()) => return Ok(outcome),
                Err(super::LaneSendError::Full(_)) => {
                    return Ok(match outcome {
                        LaneWriteOutcome::Reopened { .. } => outcome,
                        _ => LaneWriteOutcome::DroppedFull,
                    });
                }
                Err(super::LaneSendError::Oversize { .. }) => {
                    return Err(LaneTerminal::Disabled(self.role));
                }
                Err(super::LaneSendError::Closed(_)) => outcome = self.recover().await?,
            }
        }
    }

    /// Resolves when the current lane's writer ends; follow it with
    /// [`LocalLaneWriter::recover`] to reopen before the next record.
    pub async fn closed(&self) -> LaneExit {
        self.sender.closed().await
    }

    /// Follows the current lane's exit: reopens it or reports why the role
    /// stopped.
    ///
    /// # Errors
    ///
    /// [`LaneTerminal`].
    pub async fn recover(&mut self) -> Result<LaneWriteOutcome, LaneTerminal> {
        let exit = self.sender.closed().await;
        match follow_role_exit(&self.lanes, self.role, &exit).await {
            LaneRecoveryStep::Reopened {
                lane,
                request_keyframe,
                ..
            } => {
                self.sender = Self::split_sender(lane, self.bounds, self.role)?;
                Ok(LaneWriteOutcome::Reopened { request_keyframe })
            }
            step => Err(terminal_of(&self.lanes, self.role, &step)),
        }
    }

    /// Finishes the lane cleanly once its queue drains.
    pub fn finish(&self) {
        self.sender.finish();
    }
}

/// [`follow_lane_exit`] for an owner that knows its role: a lane cancelled
/// because a failure was reported elsewhere (the other half of the lane, or
/// the owner through [`LaneConnection::report_failure`]) is recovered as that
/// failure, since its recovery action went to the reporter.
async fn follow_role_exit(
    lanes: &LaneConnection,
    role: LaneRole,
    exit: &LaneExit,
) -> LaneRecoveryStep {
    if let LaneExit::Cancelled(arcen_protocol::lanes::LaneStreamCode::LaneFailed) = exit {
        if lanes.ended().is_none() {
            if let crate::lanes::LaneState::Recovering { failed, .. } = lanes.lane_state(role) {
                let policy = crate::lanes::LaneRecoveryPolicy::for_role(role);
                let action = LaneRecoveryAction::Reopen {
                    role,
                    failed,
                    reopened_by: role.opener(),
                    reset_stream: true,
                    request_keyframe: policy.requires_keyframe,
                    release_held_input: policy.release_held_input,
                };
                return follow_lane_exit(
                    lanes,
                    &LaneExit::Failed {
                        failure: LaneFailure::WriteFailed,
                        action,
                    },
                )
                .await;
            }
        }
    }
    follow_lane_exit(lanes, exit).await
}

fn terminal_of(lanes: &LaneConnection, role: LaneRole, step: &LaneRecoveryStep) -> LaneTerminal {
    match *step {
        LaneRecoveryStep::Disabled(role) => LaneTerminal::Disabled(role),
        LaneRecoveryStep::Ended(reason) => LaneTerminal::Ended(reason),
        LaneRecoveryStep::ConnectionLost(mismatch) => LaneTerminal::ConnectionLost(mismatch),
        LaneRecoveryStep::Finished => LaneTerminal::Finished,
        LaneRecoveryStep::Reopened { .. }
        | LaneRecoveryStep::AwaitPeer { .. }
        | LaneRecoveryStep::Superseded => lanes
            .ended()
            .map_or(LaneTerminal::Disabled(role), LaneTerminal::Ended),
    }
}

/// What a [`PeerLaneReader`] produced.
#[derive(Debug)]
pub enum PeerLaneEvent {
    /// The peer opened a (new) lane for the role. Apply the steps before
    /// handling its frames: they cover both a superseded lane and a failed
    /// one the peer replaced. `sender` is the write half of a bidirectional
    /// lane (input/cursor and bulk on the Pier).
    Opened {
        /// Lane generation.
        lane_generation: NonZeroU32,
        /// Start a new prediction chain (video).
        request_keyframe: bool,
        /// Release held keys, buttons and pen state.
        release_held_input: bool,
        /// Write half, for a bidirectional lane.
        sender: Option<LaneSender>,
    },
    /// One length-prefixed frame (a media frame, or a session record body
    /// for [`SessionRecord::decode_body`]).
    Frame(Bytes),
    /// The lane failed (or was failed by its owner) and the peer will open
    /// a replacement. Apply the steps now, not when the replacement
    /// arrives: held keys, buttons and pen state must not stay down while
    /// the peer reopens. The next [`PeerLaneEvent::Opened`] carries only
    /// steps owed since (a supersession).
    Lost {
        /// Start a new prediction chain (video).
        request_keyframe: bool,
        /// Release held keys, buttons and pen state.
        release_held_input: bool,
    },
}

/// Reader of a role the peer opens (Deck: video, audio output; Pier:
/// input/cursor, microphone, bulk). It follows the role across lane
/// generations: a failed lane surfaces at once as [`PeerLaneEvent::Lost`]
/// with its steps, supersession and peer reopening as
/// [`PeerLaneEvent::Opened`] with the steps still owed; disabling, the end
/// of the attachment and connection loss end it.
///
/// [`PeerLaneReader::next`] is cancel-safe.
#[derive(Debug)]
pub struct PeerLaneReader {
    lanes: LaneConnection,
    role: LaneRole,
    receiver: Option<LaneReceiver>,
    owed_keyframe: bool,
    owed_release: bool,
}

impl PeerLaneReader {
    /// A reader for `role`, which the peer must open.
    #[must_use]
    pub fn new(lanes: &LaneConnection, role: LaneRole) -> Self {
        debug_assert_ne!(role.opener(), lanes.local());
        Self {
            lanes: lanes.clone(),
            role,
            receiver: None,
            owed_keyframe: false,
            owed_release: false,
        }
    }

    /// Role.
    #[must_use]
    pub const fn role(&self) -> LaneRole {
        self.role
    }

    /// Generation of the lane being read, if one is open.
    #[must_use]
    pub fn lane_generation(&self) -> Option<NonZeroU32> {
        self.receiver.as_ref().map(LaneReceiver::lane_generation)
    }

    /// The next event.
    ///
    /// # Errors
    ///
    /// [`LaneTerminal`] once the role cannot continue.
    pub async fn next(&mut self) -> Result<PeerLaneEvent, LaneTerminal> {
        loop {
            if let Some(receiver) = self.receiver.as_mut() {
                match receiver.recv_frame().await {
                    Ok(frame) => return Ok(PeerLaneEvent::Frame(frame)),
                    Err(exit) => {
                        self.receiver = None;
                        self.follow(&exit).await?;
                        if self.owed_keyframe || self.owed_release {
                            return Ok(PeerLaneEvent::Lost {
                                request_keyframe: std::mem::take(&mut self.owed_keyframe),
                                release_held_input: std::mem::take(&mut self.owed_release),
                            });
                        }
                        continue;
                    }
                }
            }
            let lane = self
                .lanes
                .next_peer_lane(self.role)
                .await
                .map_err(|error| match error {
                    LaneWaitError::Ended(reason) => LaneTerminal::Ended(reason),
                    LaneWaitError::LocallyOpened(role) => LaneTerminal::Disabled(role),
                })?;
            let lane_generation = lane.lane_generation();
            if let Some(superseded) = lane.superseded() {
                self.owed_keyframe |= superseded.request_keyframe;
                self.owed_release |= superseded.release_held_input;
            }
            let (sender, receiver) = lane.split_default();
            let Some(receiver) = receiver else {
                return Err(LaneTerminal::Disabled(self.role));
            };
            self.receiver = Some(receiver);
            let event = PeerLaneEvent::Opened {
                lane_generation,
                request_keyframe: std::mem::take(&mut self.owed_keyframe),
                release_held_input: std::mem::take(&mut self.owed_release),
                sender,
            };
            return Ok(event);
        }
    }

    async fn follow(&mut self, exit: &LaneExit) -> Result<(), LaneTerminal> {
        match follow_role_exit(&self.lanes, self.role, exit).await {
            LaneRecoveryStep::AwaitPeer {
                request_keyframe,
                release_held_input,
            } => {
                self.owed_keyframe |= request_keyframe;
                self.owed_release |= release_held_input;
                Ok(())
            }
            LaneRecoveryStep::Superseded => Ok(()),
            step => Err(terminal_of(&self.lanes, self.role, &step)),
        }
    }

    /// Reports a failure the owner found in a frame (malformed, illegal on
    /// this lane). The next call to [`PeerLaneReader::next`] reports
    /// [`PeerLaneEvent::Lost`] with the steps the failure owes, then waits
    /// for the peer's replacement or reports the terminal outcome.
    pub fn fail(&mut self, failure: LaneFailure) {
        if let Some(receiver) = self.receiver.as_mut() {
            let action = receiver.fail(failure);
            if let LaneRecoveryAction::Reopen {
                request_keyframe,
                release_held_input,
                ..
            } = action
            {
                self.owed_keyframe |= request_keyframe;
                self.owed_release |= release_held_input;
            }
        }
    }
}

type SocketRead = std::pin::Pin<
    Box<
        dyn std::future::Future<Output = (SessionLaneReader, Result<SessionRecord, LaneExit>)>
            + Send,
    >,
>;
type SocketWrite =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), super::LaneSendError>> + Send>>;

enum SocketReadState {
    Idle(SessionLaneReader),
    Reading(SocketRead),
    Done,
}

/// Bound on a session's graceful close: how long a dropped
/// [`SessionLaneSocket`] waits for the peer to acknowledge its control lane
/// and finish its own before the QUIC connection is closed anyway. A peer
/// that never answers holds nothing longer than this.
pub const SESSION_CONTROL_LINGER: Duration = Duration::from_secs(2);

/// Whether the peer closed `connection` in order
/// ([`SESSION_CLOSED_CLOSE_CODE`]): it ended the session deliberately after
/// its control lane was delivered, so the end is not a transport loss.
#[must_use]
pub fn closed_in_order(connection: &Connection) -> bool {
    matches!(
        connection.close_reason(),
        Some(ConnectionError::ApplicationClosed(close))
            if close.error_code == VarInt::from_u32(SESSION_CLOSED_CLOSE_CODE)
    )
}

/// The control lane of one connection as a `Stream` of received
/// [`SessionRecord`]s and a `Sink` of records to send, so a product's
/// existing session code (authentication, hello, attachment dispatch) runs
/// on it unchanged. Every other lane belongs to its own worker.
///
/// Reading is cancel-safe. The stream ends on a clean finish or when the
/// peer closed the connection in order ([`closed_in_order`]), after every
/// record it sent; a failed lane yields its [`LaneExit`] (a
/// [`LaneExit::ConnectionLost`] names a peer that speaks another transport
/// layout). Sending waits only for room in the control lane's own queue.
/// Closing the sink sends a close record (unless one was sent) and finishes
/// the control lane, as a WebSocket close does.
///
/// Dropping the socket closes the session gracefully in the background,
/// bounded by [`SESSION_CONTROL_LINGER`]: the control lane is finished, the
/// peer's QUIC stack must acknowledge all of it (so a refusal or close
/// reason written just before the drop is delivered, not abandoned), the
/// peer is given the chance to read to the end and finish its own side, and
/// only then is the attachment ended and the connection closed with
/// [`SESSION_CLOSED_CLOSE_CODE`]. Other lanes are not held up by it.
pub struct SessionLaneSocket {
    lanes: LaneConnection,
    sender: LaneSender,
    read: SocketReadState,
    write: Option<SocketWrite>,
    close_sent: bool,
}

impl std::fmt::Debug for SessionLaneSocket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionLaneSocket")
            .field("remote", &self.lanes.connection().remote_address())
            .field("control_generation", &self.sender.lane_generation())
            .finish_non_exhaustive()
    }
}

impl SessionLaneSocket {
    /// Wraps an established carrier and its control lane.
    #[must_use]
    pub fn new(lanes: LaneConnection, control: SessionLane) -> Self {
        Self {
            lanes,
            sender: control.sender,
            read: SocketReadState::Idle(control.reader),
            write: None,
            close_sent: false,
        }
    }

    /// The connection's lanes, for the attachment's workers.
    #[must_use]
    pub const fn lanes(&self) -> &LaneConnection {
        &self.lanes
    }

    /// A clone of the control lane's sender.
    #[must_use]
    pub fn control_sender(&self) -> LaneSender {
        self.sender.clone()
    }
}

impl Drop for SessionLaneSocket {
    fn drop(&mut self) {
        let read = std::mem::replace(&mut self.read, SocketReadState::Done);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            close_in_order(&self.lanes);
            return;
        };
        runtime.spawn(close_gracefully(
            self.lanes.clone(),
            self.sender.clone(),
            read,
        ));
    }
}

fn close_in_order(lanes: &LaneConnection) {
    lanes.end(LaneEndReason::Closed);
    lanes.connection().close(
        VarInt::from_u32(SESSION_CLOSED_CLOSE_CODE),
        SESSION_CLOSED_CLOSE_REASON,
    );
}

/// The graceful close of a dropped [`SessionLaneSocket`].
///
/// Closing a QUIC connection abandons stream data the peer has not
/// acknowledged, so the last records (an `auth_result` refusal, a close
/// reason) would be lost if the connection closed as soon as they were
/// written. Instead: finish the control lane, wait until the peer
/// acknowledged every byte, then wait until the peer finished its side
/// (it read to the end and closed its session) or closed the connection;
/// all within [`SESSION_CONTROL_LINGER`]. Only then end the attachment and
/// close in order. Both endpoints may run this at once without waiting on
/// each other: each side's wait ends on the other's finish, which comes
/// first.
async fn close_gracefully(lanes: LaneConnection, sender: LaneSender, read: SocketReadState) {
    sender.finish();
    let connection = lanes.connection().clone();
    let _ = tokio::time::timeout(SESSION_CONTROL_LINGER, async {
        let _ = sender.delivered().await;
        tokio::select! {
            () = drain_control(read) => {}
            _ = connection.closed() => {}
        }
    })
    .await;
    close_in_order(&lanes);
}

/// Reads the peer's side of the control lane to its end, discarding what
/// remains: the socket's owner is gone.
async fn drain_control(read: SocketReadState) {
    let mut reader = match read {
        SocketReadState::Idle(reader) => reader,
        SocketReadState::Reading(future) => match future.await {
            (reader, Ok(_)) => reader,
            (_, Err(_)) => return,
        },
        SocketReadState::Done => return,
    };
    while reader.next_record().await.is_ok() {}
}

impl futures_core::Stream for SessionLaneSocket {
    type Item = Result<SessionRecord, LaneExit>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            match std::mem::replace(&mut this.read, SocketReadState::Done) {
                SocketReadState::Done => return Poll::Ready(None),
                SocketReadState::Idle(mut reader) => {
                    this.read = SocketReadState::Reading(Box::pin(async move {
                        let result = reader.next_record().await;
                        (reader, result)
                    }));
                }
                SocketReadState::Reading(mut future) => match future.as_mut().poll(context) {
                    Poll::Pending => {
                        this.read = SocketReadState::Reading(future);
                        return Poll::Pending;
                    }
                    Poll::Ready((reader, Ok(record))) => {
                        this.read = SocketReadState::Idle(reader);
                        return Poll::Ready(Some(Ok(record)));
                    }
                    Poll::Ready((
                        _,
                        Err(
                            LaneExit::Finished
                            | LaneExit::Failed {
                                failure: LaneFailure::PeerFinished,
                                ..
                            },
                        ),
                    )) => return Poll::Ready(None),
                    // Every record the peer sent was read first (a closed
                    // connection still hands out what it received).
                    Poll::Ready((_, Err(LaneExit::ConnectionLost { mismatch: None })))
                        if closed_in_order(this.lanes.connection()) =>
                    {
                        return Poll::Ready(None);
                    }
                    Poll::Ready((_, Err(exit))) => return Poll::Ready(Some(Err(exit))),
                },
            }
        }
    }
}

impl futures_sink::Sink<SessionRecord> for SessionLaneSocket {
    type Error = super::LaneSendError;

    fn poll_ready(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.poll_flush(context)
    }

    fn start_send(self: std::pin::Pin<&mut Self>, item: SessionRecord) -> Result<(), Self::Error> {
        let this = self.get_mut();
        this.close_sent |= matches!(item, SessionRecord::Close(_));
        let record = session_record_bytes(&item);
        let sender = this.sender.clone();
        this.write = Some(Box::pin(async move { sender.send(record).await }));
        Ok(())
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let this = self.get_mut();
        let Some(write) = this.write.as_mut() else {
            return std::task::Poll::Ready(Ok(()));
        };
        let result = std::task::ready!(write.as_mut().poll(context));
        this.write = None;
        std::task::Poll::Ready(result)
    }

    /// Sends a close record unless one was sent (the peer reads the end as
    /// deliberate, as with a WebSocket close), then finishes the control
    /// lane once its queue drains.
    fn poll_close(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::ready!(self.as_mut().poll_flush(context))?;
        if !self.close_sent {
            self.close_sent = true;
            let sender = self.sender.clone();
            let record = session_record_bytes(&SessionRecord::Close(None));
            self.write = Some(Box::pin(async move { sender.send(record).await }));
        }
        // A lane that already closed has nothing more to say; that is not a
        // failure to close.
        let _ = std::task::ready!(self.as_mut().poll_flush(context));
        self.sender.finish();
        std::task::Poll::Ready(Ok(()))
    }
}
