//! Per-connection lane state, the accept dispatcher and lane openers.

use std::fmt::{Debug, Display, Formatter};
use std::num::NonZeroU32;
use std::ops::ControlFlow;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, Instant};

use arcen_protocol::lanes::{
    LANE_PREFACE_BYTES, LANE_PREFACE_HEADER_BYTES, LANE_ROLE_COUNT,
    LANE_VERSION_MISMATCH_CLOSE_CODE, LaneAttachment, LaneEndpoint, LanePreface, LanePrefaceError,
    LaneRole, LaneRoleSet, LaneShape, LaneStreamCode, PeerTransportMismatch,
    lane_mismatch_close_reason,
};
use quinn::{Connection, ConnectionError, RecvStream, SendStream, VarInt};
use tokio::sync::{Notify, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::BoundedTransportPolicy;
use crate::lanes::{
    LaneEndReason, LaneFailure, LaneOpenError, LaneRecoveryAction, LaneRegistry, LaneRegistryError,
    LaneState, LaneUnavailable, PeerLaneAdmission, PendingResolution, SupersededLane,
    lane_send_priority,
};

use super::lane::{Lane, LaneControl, LaneParts, RecvHalf, SendHalf, varint};
use super::stats::{LaneStatusSnapshot, RoleStats, bump};
use super::telemetry::LaneTelemetry;
use super::{LaneRuntimeConfig, MAX_CONCURRENT_PREFACE_READS};

/// Classifies a QUIC connection close as a peer running an incompatible
/// transport layout (a pre-lane build or another lane version), so both
/// endpoints can print [`PeerTransportMismatch::message`].
#[must_use]
pub fn classify_peer_close(error: &ConnectionError) -> Option<PeerTransportMismatch> {
    match error {
        ConnectionError::ApplicationClosed(close) => {
            PeerTransportMismatch::from_peer_close(close.error_code.into_inner(), &close.reason)
        }
        _ => None,
    }
}

/// Why a local lane could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneOpenFailure {
    /// The registry refused to prepare the lane.
    Registry(LaneOpenError),
    /// The stream could not be opened or its preface written in time. The
    /// failure was reported; `action` is the lane-scoped recovery.
    Stream {
        /// Role.
        role: LaneRole,
        /// Generation that failed.
        lane_generation: NonZeroU32,
        /// Recovery the registry chose.
        action: LaneRecoveryAction,
        /// The peer closed the connection because it speaks another
        /// transport layout.
        mismatch: Option<PeerTransportMismatch>,
        /// Bounded diagnostic.
        detail: String,
    },
}

impl Display for LaneOpenFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry(error) => Display::fmt(error, formatter),
            Self::Stream {
                mismatch: Some(mismatch),
                ..
            } => Display::fmt(mismatch, formatter),
            Self::Stream { role, detail, .. } => {
                write!(formatter, "{role} lane could not be opened: {detail}")
            }
        }
    }
}

impl std::error::Error for LaneOpenFailure {}

/// Why [`LaneConnection::next_peer_lane`] returned without a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneWaitError {
    /// The local endpoint opens this role; there is nothing to accept.
    LocallyOpened(LaneRole),
    /// The attachment ended.
    Ended(LaneEndReason),
}

impl Display for LaneWaitError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocallyOpened(role) => write!(formatter, "the local endpoint opens {role}"),
            Self::Ended(reason) => write!(formatter, "attachment ended: {}", reason.token()),
        }
    }
}

impl std::error::Error for LaneWaitError {}

struct PeerStreams {
    lane_generation: NonZeroU32,
    /// Steps owed for a held lane this one replaced before authorization.
    superseded: Option<SupersededLane>,
    send: Option<SendStream>,
    recv: RecvStream,
}

struct LiveLane {
    lane_generation: NonZeroU32,
    control: Arc<LaneControl>,
}

struct State {
    registry: LaneRegistry,
    held: [Option<PeerStreams>; LANE_ROLE_COUNT],
    live: [Option<LiveLane>; LANE_ROLE_COUNT],
    delivered: [Option<LaneParts>; LANE_ROLE_COUNT],
}

/// Side effects of a state change, applied after the lock is released.
#[derive(Default)]
struct Effects {
    cancel: Vec<(Arc<LaneControl>, LaneStreamCode)>,
    refuse: Vec<(
        Option<SendStream>,
        RecvStream,
        LaneStreamCode,
        Option<LaneRole>,
    )>,
    drop_parts: Vec<LaneParts>,
    notify: Vec<LaneRole>,
    ended: Option<LaneEndReason>,
}

pub(super) struct Shared {
    pub(super) connection: Connection,
    pub(super) local: LaneEndpoint,
    pub(super) policy: BoundedTransportPolicy,
    preface_timeout: Duration,
    pub(super) telemetry: LaneTelemetry,
    pub(super) epoch: Instant,
    state: Mutex<State>,
    deliveries: [Notify; LANE_ROLE_COUNT],
    ended: watch::Sender<Option<LaneEndReason>>,
    shutdown: CancellationToken,
    stats: [RoleStats; LANE_ROLE_COUNT],
    refused_without_role: AtomicU64,
    local_mismatch: Mutex<Option<PeerTransportMismatch>>,
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(super) const fn stats(&self, role: LaneRole) -> &RoleStats {
        &self.stats[role.index()]
    }

    pub(super) fn mark_healthy(&self, role: LaneRole, generation: NonZeroU32) {
        self.state().registry.mark_healthy(role, generation);
    }

    pub(super) fn report_failure(
        &self,
        role: LaneRole,
        lane_generation: NonZeroU32,
        failure: LaneFailure,
    ) -> LaneRecoveryAction {
        let mut effects = Effects::default();
        let action = {
            let mut state = self.state();
            let action = state
                .registry
                .report_failure(role, lane_generation, failure);
            match action {
                LaneRecoveryAction::EndAttachment { reason, .. } => {
                    Self::end_locked(&mut state, reason, &mut effects);
                }
                LaneRecoveryAction::Disable { role, .. } => {
                    if let Some(live) = state.live[role.index()].take() {
                        effects
                            .cancel
                            .push((live.control, LaneStreamCode::Disabled));
                    }
                }
                LaneRecoveryAction::Reopen { role, failed, .. } => {
                    // Whoever reported, every half of the failed generation
                    // stops: nothing more is read from or written to it.
                    let index = role.index();
                    if state.live[index]
                        .as_ref()
                        .is_some_and(|live| live.lane_generation == failed)
                    {
                        if let Some(live) = state.live[index].take() {
                            effects
                                .cancel
                                .push((live.control, LaneStreamCode::LaneFailed));
                        }
                    }
                }
                LaneRecoveryAction::Superseded => {}
            }
            action
        };
        if action != LaneRecoveryAction::Superseded {
            bump(&self.stats(role).failures);
            self.telemetry.failed(
                role.token(),
                failure.token(),
                action.token(),
                lane_generation.get(),
                failure.requires_reset(),
            );
        }
        self.apply(effects);
        action
    }

    fn end_locked(state: &mut State, reason: LaneEndReason, effects: &mut Effects) {
        Self::end_scoped_locked(state, reason, effects, false);
    }

    /// Ends the attachment. `keep_control` leaves an open control lane to
    /// its owner, which closes it gracefully: the control lane belongs to
    /// the connection, and what it still carries (a close reason) must not
    /// be reset with the attachment's lanes.
    fn end_scoped_locked(
        state: &mut State,
        reason: LaneEndReason,
        effects: &mut Effects,
        keep_control: bool,
    ) {
        if state.registry.ended().is_none() {
            state.registry.end(reason);
        }
        let reason = state.registry.ended().unwrap_or(reason);
        for role in LaneRole::ALL {
            let index = role.index();
            if keep_control && role == LaneRole::Control && state.live[index].is_some() {
                effects.notify.push(role);
                continue;
            }
            if let Some(live) = state.live[index].take() {
                effects
                    .cancel
                    .push((live.control, LaneStreamCode::AttachmentEnded));
            }
            if let Some(held) = state.held[index].take() {
                effects.refuse.push((
                    held.send,
                    held.recv,
                    LaneStreamCode::AttachmentEnded,
                    Some(role),
                ));
            }
            if let Some(parts) = state.delivered[index].take() {
                effects
                    .cancel
                    .push((Arc::clone(&parts.control), LaneStreamCode::AttachmentEnded));
                effects.drop_parts.push(parts);
            }
            effects.notify.push(role);
        }
        effects.ended = Some(reason);
    }

    fn apply(&self, effects: Effects) {
        for (control, code) in effects.cancel {
            control.cancel(code);
        }
        for (send, recv, code, role) in effects.refuse {
            self.reset_streams(send, recv, code, role);
        }
        drop(effects.drop_parts);
        if let Some(reason) = effects.ended {
            self.ended.send_if_modified(|current| {
                if current.is_none() {
                    *current = Some(reason);
                    true
                } else {
                    false
                }
            });
        }
        for role in effects.notify {
            self.deliveries[role.index()].notify_waiters();
        }
    }

    fn reset_streams(
        &self,
        send: Option<SendStream>,
        mut recv: RecvStream,
        code: LaneStreamCode,
        role: Option<LaneRole>,
    ) {
        let _ = recv.stop(varint(code));
        if let Some(mut send) = send {
            let _ = send.reset(varint(code));
        }
        if let Some(role) = role {
            bump(&self.stats(role).resets);
        }
    }

    fn refuse(
        &self,
        send: Option<SendStream>,
        recv: RecvStream,
        code: LaneStreamCode,
        role: Option<LaneRole>,
        reason: &str,
        lane_generation: Option<NonZeroU32>,
    ) {
        self.reset_streams(send, recv, code, role);
        match role {
            Some(role) => bump(&self.stats(role).refused),
            None => bump(&self.refused_without_role),
        }
        self.telemetry.refused(
            role.map_or("unknown", LaneRole::token),
            reason,
            lane_generation.map(NonZeroU32::get),
        );
    }

    /// Records an admitted peer lane for its owner, cancelling the lane it
    /// supersedes and any older lane its owner has not taken yet.
    fn install_locked(
        &self,
        state: &mut State,
        admission: crate::lanes::LaneAdmission,
        streams: PeerStreams,
        effects: &mut Effects,
    ) {
        let role = admission.role;
        let index = role.index();
        let control = LaneControl::new(self.shutdown.child_token());
        if let Some(previous) = state.live[index].replace(LiveLane {
            lane_generation: admission.lane_generation,
            control: Arc::clone(&control),
        }) {
            let code = if admission.superseded.is_some() {
                LaneStreamCode::Superseded
            } else {
                LaneStreamCode::LaneFailed
            };
            effects.cancel.push((previous.control, code));
        }
        if admission.superseded.is_some() {
            bump(&self.stats(role).superseded);
        }
        // The owner owes the steps for the oldest lane it actually held: one
        // it never took (`stale`) or a held lane that was never read passes
        // its own steps on.
        let mut superseded = admission.superseded.or(streams.superseded);
        if let Some(stale) = state.delivered[index].take() {
            superseded = stale.superseded.or(superseded);
            effects.drop_parts.push(stale);
        }
        bump(&self.stats(role).lanes_opened);
        let send = streams
            .send
            .map(|send| SendHalf::new(send, Arc::clone(&control)));
        state.delivered[index] = Some(LaneParts {
            role,
            lane_generation: admission.lane_generation,
            attachment_generation: admission.attachment_generation,
            superseded,
            control: Arc::clone(&control),
            send,
            recv: Some(RecvHalf::new(streams.recv, control)),
        });
        effects.notify.push(role);
    }

    fn admit(
        &self,
        shape: LaneShape,
        preface: &LanePreface,
        send: Option<SendStream>,
        recv: RecvStream,
    ) {
        let role = preface.role();
        let lane_generation = preface.lane_generation();
        let mut streams = PeerStreams {
            lane_generation,
            superseded: None,
            send,
            recv,
        };
        let mut effects = Effects::default();
        let refused = {
            let mut state = self.state();
            match state.registry.admit_peer(shape, preface) {
                Err(refusal) => Some((streams, refusal)),
                Ok(PeerLaneAdmission::PendingAuthorization {
                    role, superseded, ..
                }) => {
                    let older = state.held[role.index()].take();
                    streams.superseded = older
                        .as_ref()
                        .and_then(|older| older.superseded)
                        .or(superseded);
                    state.held[role.index()] = Some(streams);
                    if let Some(older) = older {
                        bump(&self.stats(role).superseded);
                        effects.refuse.push((
                            older.send,
                            older.recv,
                            LaneStreamCode::Superseded,
                            Some(role),
                        ));
                    }
                    None
                }
                Ok(PeerLaneAdmission::Admitted(admission)) => {
                    self.install_locked(&mut state, admission, streams, &mut effects);
                    None
                }
            }
        };
        self.apply(effects);
        if let Some((streams, refusal)) = refused {
            self.refuse(
                streams.send,
                streams.recv,
                LaneStreamCode::Refused,
                Some(role),
                refusal.token(),
                Some(streams.lane_generation),
            );
        }
    }

    fn handle_preface(&self, read: PrefaceRead) -> ControlFlow<()> {
        let PrefaceRead {
            shape,
            send,
            recv,
            outcome,
        } = read;
        match outcome {
            Ok(preface) => {
                self.admit(shape, &preface, send, recv);
                ControlFlow::Continue(())
            }
            Err(PrefaceFailure::Preface(error)) => {
                if let Some(mismatch) = error.version_mismatch() {
                    self.refuse_mismatch(mismatch, error.token(), send, recv);
                    return ControlFlow::Break(());
                }
                self.refuse(
                    send,
                    recv,
                    LaneStreamCode::Refused,
                    None,
                    error.token(),
                    None,
                );
                ControlFlow::Continue(())
            }
            Err(failure) => {
                let code = if matches!(failure, PrefaceFailure::Timeout) {
                    LaneStreamCode::PrefaceTimeout
                } else {
                    LaneStreamCode::Refused
                };
                self.refuse(send, recv, code, None, failure.token(), None);
                ControlFlow::Continue(())
            }
        }
    }
}

fn close_for_mismatch(connection: &Connection) {
    connection.close(
        VarInt::from_u32(LANE_VERSION_MISMATCH_CLOSE_CODE),
        lane_mismatch_close_reason().as_bytes(),
    );
}

/// How long a pre-lane Deck's refusal may wait for its acknowledgement.
const LEGACY_REFUSAL_LINGER: Duration = Duration::from_secs(1);

/// Answers a pre-lane Deck on its own stream with a WebSocket close frame
/// it prints, waits (bounded) until its QUIC stack acknowledged it, then
/// closes the connection with the mismatch code. A closed connection
/// still lets the Deck read what it acknowledged.
async fn refuse_legacy_direct_stream(connection: Connection, mut send: SendStream) {
    let frame = arcen_protocol::lanes::legacy_direct_stream_refusal_frame();
    let delivered = async {
        send.write_all(&frame).await.ok()?;
        send.finish().ok()?;
        send.stopped().await.ok()
    };
    let _ = tokio::time::timeout(LEGACY_REFUSAL_LINGER, delivered).await;
    close_for_mismatch(&connection);
}

impl Shared {
    /// Refuses a peer on another transport layout and closes the connection
    /// with the version-mismatch code, recording why for the owner.
    fn refuse_mismatch(
        &self,
        mismatch: PeerTransportMismatch,
        token: &str,
        send: Option<SendStream>,
        recv: RecvStream,
    ) {
        *self
            .local_mismatch
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(mismatch);
        match send {
            // A pre-lane Deck reads WebSocket frames on this stream: tell it
            // why in a form it prints, then close.
            Some(send) if mismatch == PeerTransportMismatch::PeerPredatesLanes => {
                drop(recv);
                bump(&self.refused_without_role);
                self.telemetry.refused("unknown", token, None);
                tokio::spawn(refuse_legacy_direct_stream(self.connection.clone(), send));
            }
            send => {
                self.refuse(send, recv, LaneStreamCode::Refused, None, token, None);
                close_for_mismatch(&self.connection);
            }
        }
    }
}

enum PrefaceFailure {
    Timeout,
    Preface(LanePrefaceError),
    Stream,
}

impl PrefaceFailure {
    const fn token(&self) -> &'static str {
        match self {
            Self::Timeout => "preface_timeout",
            Self::Preface(error) => error.token(),
            Self::Stream => "preface_stream_error",
        }
    }
}

struct PrefaceRead {
    shape: LaneShape,
    send: Option<SendStream>,
    recv: RecvStream,
    outcome: Result<LanePreface, PrefaceFailure>,
}

/// Reads exactly one fixed-size preface: the header first, so another
/// version or a legacy peer is reported before anything else is read, and
/// never a byte past the preface.
async fn read_preface(recv: &mut RecvStream) -> Result<LanePreface, PrefaceFailure> {
    let mut bytes = [0_u8; LANE_PREFACE_BYTES];
    let (header, rest) = bytes.split_at_mut(LANE_PREFACE_HEADER_BYTES);
    recv.read_exact(header)
        .await
        .map_err(|_| PrefaceFailure::Stream)?;
    let header: &[u8; LANE_PREFACE_HEADER_BYTES] =
        (&*header).try_into().map_err(|_| PrefaceFailure::Stream)?;
    LanePreface::check_header(header).map_err(PrefaceFailure::Preface)?;
    recv.read_exact(rest)
        .await
        .map_err(|_| PrefaceFailure::Stream)?;
    LanePreface::decode(&bytes).map_err(PrefaceFailure::Preface)
}

fn spawn_preface_read(
    reads: &mut JoinSet<PrefaceRead>,
    weak: &Weak<Shared>,
    shape: LaneShape,
    send: Option<SendStream>,
    mut recv: RecvStream,
    timeout: Duration,
) {
    if reads.len() >= MAX_CONCURRENT_PREFACE_READS {
        if let Some(shared) = weak.upgrade() {
            shared.refuse(
                send,
                recv,
                LaneStreamCode::Refused,
                None,
                "preface_backlog",
                None,
            );
        }
        return;
    }
    reads.spawn(async move {
        let outcome = tokio::time::timeout(timeout, read_preface(&mut recv))
            .await
            .unwrap_or(Err(PrefaceFailure::Timeout));
        PrefaceRead {
            shape,
            send,
            recv,
            outcome,
        }
    });
}

/// The connection's only caller of `accept_bi`/`accept_uni`.
async fn dispatch(
    weak: Weak<Shared>,
    connection: Connection,
    shutdown: CancellationToken,
    timeout: Duration,
) {
    let mut reads = JoinSet::new();
    loop {
        tokio::select! {
            biased;
            () = shutdown.cancelled() => break,
            Some(joined) = reads.join_next(), if !reads.is_empty() => {
                let Ok(read) = joined else { continue };
                let Some(shared) = weak.upgrade() else { break };
                if shared.handle_preface(read).is_break() {
                    break;
                }
            }
            accepted = connection.accept_bi() => match accepted {
                Ok((send, recv)) => spawn_preface_read(
                    &mut reads, &weak, LaneShape::Bidirectional, Some(send), recv, timeout,
                ),
                Err(_) => break,
            },
            accepted = connection.accept_uni() => match accepted {
                Ok(recv) => spawn_preface_read(
                    &mut reads, &weak, LaneShape::Unidirectional, None, recv, timeout,
                ),
                Err(_) => break,
            },
        }
    }
    reads.abort_all();
    if let Some(shared) = weak.upgrade() {
        let mut effects = Effects::default();
        Shared::end_locked(&mut shared.state(), LaneEndReason::Closed, &mut effects);
        shared.apply(effects);
    }
}

/// Lanes of one QUIC connection: the accept dispatcher, the lane registry
/// and per-role handoff to owners. Cloning shares the same state.
#[derive(Clone)]
pub struct LaneConnection {
    shared: Arc<Shared>,
}

impl Debug for LaneConnection {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaneConnection")
            .field("local", &self.shared.local)
            .field("remote", &self.shared.connection.remote_address())
            .finish_non_exhaustive()
    }
}

impl LaneConnection {
    /// Takes over `connection`'s stream acceptance and spawns its accept
    /// dispatcher. Nothing else may call `accept_bi`/`accept_uni` on this
    /// connection afterwards. Must be called inside a Tokio runtime.
    ///
    /// The Pier still runs [`super::super::keep_send_window_interactive`]
    /// for the connection; lanes share its congestion and flow control.
    #[must_use]
    pub fn start(connection: Connection, config: LaneRuntimeConfig) -> Self {
        let LaneRuntimeConfig {
            local,
            policy,
            preface_timeout,
            telemetry,
        } = config;
        let shutdown = CancellationToken::new();
        let shared = Arc::new(Shared {
            connection: connection.clone(),
            local,
            policy,
            preface_timeout,
            telemetry: LaneTelemetry::new(telemetry),
            epoch: Instant::now(),
            state: Mutex::new(State {
                registry: LaneRegistry::new(local),
                held: std::array::from_fn(|_| None),
                live: std::array::from_fn(|_| None),
                delivered: std::array::from_fn(|_| None),
            }),
            deliveries: std::array::from_fn(|_| Notify::new()),
            ended: watch::Sender::new(None),
            shutdown: shutdown.clone(),
            stats: std::array::from_fn(|_| RoleStats::default()),
            refused_without_role: AtomicU64::new(0),
            local_mismatch: Mutex::new(None),
        });
        tokio::spawn(dispatch(
            Arc::downgrade(&shared),
            connection,
            shutdown,
            preface_timeout,
        ));
        Self { shared }
    }

    /// Local endpoint.
    #[must_use]
    pub fn local(&self) -> LaneEndpoint {
        self.shared.local
    }

    /// The underlying QUIC connection.
    #[must_use]
    pub fn connection(&self) -> &Connection {
        &self.shared.connection
    }

    /// Opens a lane the local endpoint owns: reserves the next generation,
    /// opens a stream of the role's shape, sets the contract priority and
    /// writes the preface, all within the preface timeout.
    ///
    /// # Errors
    ///
    /// [`LaneOpenFailure::Registry`] when the registry refuses (wrong
    /// opener, not authorized, still open, disabled, ended), or
    /// [`LaneOpenFailure::Stream`] when the stream fails; that failure has
    /// already been reported and carries the recovery.
    pub async fn open_lane(&self, role: LaneRole) -> Result<Lane, LaneOpenFailure> {
        let (preface, control, attachment_generation) = {
            let mut state = self.shared.state();
            let preface = state
                .registry
                .open_local(role)
                .map_err(LaneOpenFailure::Registry)?;
            let control = LaneControl::new(self.shared.shutdown.child_token());
            if let Some(previous) = state.live[role.index()].replace(LiveLane {
                lane_generation: preface.lane_generation(),
                control: Arc::clone(&control),
            }) {
                previous.control.cancel(LaneStreamCode::LaneFailed);
            }
            let attachment_generation =
                preface.attachment().map(|attachment| attachment.generation);
            (preface, control, attachment_generation)
        };
        let lane_generation = preface.lane_generation();
        let opened = tokio::time::timeout(
            self.shared.preface_timeout,
            open_stream(&self.shared.connection, role, &preface),
        )
        .await
        .unwrap_or_else(|_| {
            Err(OpenStreamError {
                detail: "timed out opening the lane".to_owned(),
                connection: None,
            })
        });
        match opened {
            Ok((send, recv)) => {
                bump(&self.shared.stats(role).lanes_opened);
                Ok(Lane::new(
                    Arc::clone(&self.shared),
                    LaneParts {
                        role,
                        lane_generation,
                        attachment_generation,
                        superseded: None,
                        control: Arc::clone(&control),
                        send: Some(SendHalf::new(send, Arc::clone(&control))),
                        recv: recv.map(|recv| RecvHalf::new(recv, control)),
                    },
                ))
            }
            Err(error) => {
                control.cancel(LaneStreamCode::LaneFailed);
                let action =
                    self.shared
                        .report_failure(role, lane_generation, LaneFailure::WriteFailed);
                Err(LaneOpenFailure::Stream {
                    role,
                    lane_generation,
                    action,
                    mismatch: error.connection.as_ref().and_then(classify_peer_close),
                    detail: error.detail,
                })
            }
        }
    }

    /// Follows a recovery action: opens the replacement when the local
    /// endpoint reopens the failed role, and returns `None` for every other
    /// action (the peer reopens, the role is disabled, the attachment ended
    /// or the report was superseded). Never opens another role.
    ///
    /// # Errors
    ///
    /// As [`LaneConnection::open_lane`].
    pub async fn reopen(
        &self,
        action: &LaneRecoveryAction,
    ) -> Result<Option<Lane>, LaneOpenFailure> {
        match *action {
            LaneRecoveryAction::Reopen {
                role, reopened_by, ..
            } if reopened_by == self.shared.local => self.open_lane(role).await.map(Some),
            _ => Ok(None),
        }
    }

    /// Waits for the next lane the peer opened for `role`. If several
    /// arrive before the owner takes one, only the newest is kept and it
    /// carries the supersession steps the owner still owes.
    ///
    /// # Errors
    ///
    /// [`LaneWaitError`] when the local endpoint opens `role` or the
    /// attachment ended.
    pub async fn next_peer_lane(&self, role: LaneRole) -> Result<Lane, LaneWaitError> {
        if role.opener() == self.shared.local {
            return Err(LaneWaitError::LocallyOpened(role));
        }
        loop {
            let notified = self.shared.deliveries[role.index()].notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let mut state = self.shared.state();
                if let Some(parts) = state.delivered[role.index()].take() {
                    return Ok(Lane::new(Arc::clone(&self.shared), parts));
                }
                if let Some(reason) = state.registry.ended() {
                    return Err(LaneWaitError::Ended(reason));
                }
            }
            notified.await;
        }
    }

    /// Takes a waiting peer lane for `role` without waiting.
    #[must_use]
    pub fn try_next_peer_lane(&self, role: LaneRole) -> Option<Lane> {
        let parts = self.shared.state().delivered[role.index()].take()?;
        Some(Lane::new(Arc::clone(&self.shared), parts))
    }

    /// Records the attachment grant: the Pier before sending
    /// `lane_attachment`, the Deck after validating it. Lanes the Deck held
    /// are validated in full; admitted ones become available through
    /// [`LaneConnection::next_peer_lane`], refused ones are reset.
    ///
    /// # Errors
    ///
    /// [`LaneRegistryError`] when the control lane is not open, the
    /// attachment was already authorized, or it ended.
    pub fn authorize(
        &self,
        attachment: LaneAttachment,
        roles: LaneRoleSet,
    ) -> Result<Vec<PendingResolution>, LaneRegistryError> {
        let mut effects = Effects::default();
        let mut refused = Vec::new();
        let resolutions = {
            let mut state = self.shared.state();
            let resolutions = state.registry.authorize(attachment, roles)?;
            for resolution in &resolutions {
                let Some(streams) = state.held[resolution.role.index()].take() else {
                    continue;
                };
                match resolution.outcome {
                    Ok(admission) => {
                        self.shared
                            .install_locked(&mut state, admission, streams, &mut effects);
                    }
                    Err(refusal) => refused.push((resolution.role, refusal, streams)),
                }
            }
            resolutions
        };
        self.shared.apply(effects);
        for (role, refusal, streams) in refused {
            self.shared.refuse(
                streams.send,
                streams.recv,
                LaneStreamCode::Refused,
                Some(role),
                refusal.token(),
                Some(streams.lane_generation),
            );
        }
        Ok(resolutions)
    }

    /// Whether a payload for `role` may be sent now, and on which lane
    /// generation. Never suggests another role.
    ///
    /// # Errors
    ///
    /// [`LaneUnavailable`].
    pub fn check_send(&self, role: LaneRole) -> Result<NonZeroU32, LaneUnavailable> {
        self.shared.state().registry.check_send(role)
    }

    /// Registry state of `role`.
    #[must_use]
    pub fn lane_state(&self, role: LaneRole) -> LaneState {
        self.shared.state().registry.state(role)
    }

    /// Generation of the lane held for `role` pending authorization.
    #[must_use]
    pub fn pending(&self, role: LaneRole) -> Option<NonZeroU32> {
        self.shared.state().registry.pending(role)
    }

    /// Reports a failure the owner detected outside a [`super::LaneSender`]
    /// or [`super::LaneReceiver`] and returns the lane-scoped recovery. A
    /// failed, disabled or ended lane is cancelled, so its sender, receiver
    /// or duplex resets its streams and delivers nothing further.
    #[must_use]
    pub fn report_failure(
        &self,
        role: LaneRole,
        lane_generation: NonZeroU32,
        failure: LaneFailure,
    ) -> LaneRecoveryAction {
        self.shared.report_failure(role, lane_generation, failure)
    }

    /// Ends the attachment: every lane, held stream and undelivered lane is
    /// reset with [`LaneStreamCode::AttachmentEnded`] and later streams are
    /// refused. An open control lane is left to its owner
    /// ([`super::super::SessionLaneSocket`] closes it gracefully, so a close
    /// reason already queued on it is still delivered); it ends with the
    /// connection. The QUIC connection itself stays open for the caller.
    pub fn end(&self, reason: LaneEndReason) {
        let mut effects = Effects::default();
        Shared::end_scoped_locked(&mut self.shared.state(), reason, &mut effects, true);
        self.shared.apply(effects);
    }

    /// The transport mismatch this endpoint closed the connection for (a
    /// pre-lane or other-version peer), if it did.
    #[must_use]
    pub fn local_mismatch(&self) -> Option<PeerTransportMismatch> {
        *self
            .shared
            .local_mismatch
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Why the attachment ended, if it has.
    #[must_use]
    pub fn ended(&self) -> Option<LaneEndReason> {
        *self.shared.ended.borrow()
    }

    /// Resolves when the attachment ends, with the reason.
    pub async fn wait_ended(&self) -> LaneEndReason {
        let mut receiver = self.shared.ended.subscribe();
        loop {
            if let Some(reason) = *receiver.borrow_and_update() {
                return reason;
            }
            if receiver.changed().await.is_err() {
                return LaneEndReason::Closed;
            }
        }
    }

    /// Bounded health snapshot of every role's lane.
    #[must_use]
    pub fn status(&self) -> LaneStatusSnapshot {
        let (states, ended) = {
            let state = self.shared.state();
            (
                LaneRole::ALL.map(|role| state.registry.state(role)),
                state.registry.ended(),
            )
        };
        LaneStatusSnapshot {
            local: self.shared.local,
            ended,
            roles: LaneRole::ALL.map(|role| {
                self.shared
                    .stats(role)
                    .snapshot(role, states[role.index()], self.shared.epoch)
            }),
            refused_without_role: self
                .shared
                .refused_without_role
                .load(std::sync::atomic::Ordering::Relaxed),
            telemetry_suppressed: self.shared.telemetry.suppressed(),
        }
    }
}

struct OpenStreamError {
    detail: String,
    connection: Option<ConnectionError>,
}

impl From<ConnectionError> for OpenStreamError {
    fn from(error: ConnectionError) -> Self {
        Self {
            detail: error.to_string(),
            connection: Some(error),
        }
    }
}

impl From<quinn::WriteError> for OpenStreamError {
    fn from(error: quinn::WriteError) -> Self {
        let detail = error.to_string();
        let connection = match error {
            quinn::WriteError::ConnectionLost(error) => Some(error),
            _ => None,
        };
        Self { detail, connection }
    }
}

async fn open_stream(
    connection: &Connection,
    role: LaneRole,
    preface: &LanePreface,
) -> Result<(SendStream, Option<RecvStream>), OpenStreamError> {
    let (mut send, recv) = match role.shape() {
        LaneShape::Unidirectional => (connection.open_uni().await?, None),
        LaneShape::Bidirectional => {
            let (send, recv) = connection.open_bi().await?;
            (send, Some(recv))
        }
    };
    send.set_priority(lane_send_priority(role))
        .map_err(|error| OpenStreamError {
            detail: error.to_string(),
            connection: None,
        })?;
    send.write_all(&preface.encode()).await?;
    Ok((send, recv))
}
