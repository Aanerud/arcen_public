//! Pure per-attachment lane registry: admission, ownership and failure
//! semantics for the QUIC lanes defined in [`arcen_protocol::lanes`].
//!
//! This module performs no I/O and is compiled without the `quic` feature, so
//! the default `arcen-transport` graph stays free of Quinn and Tokio. The
//! `quic` feature's lane runtime (`quic::LaneConnection`) feeds it the shape
//! of each accepted stream and its decoded preface, and hands a stream to a
//! worker only after [`LaneRegistry::admit_peer`] returns an admission. No
//! product uses the runtime yet.
//!
//! Invariants the registry enforces before any payload reaches a worker:
//!
//! - the control lane is first and the only lane admitted before
//!   authorization; TLS establishment is never authorization;
//! - every other lane names the authorized attachment generation and its
//!   Pier-issued binding (compared in constant time);
//! - each role uses its fixed stream shape and opener, so a lane cannot carry
//!   payload in the wrong direction;
//! - a role has at most one open lane; a replacement must present a strictly
//!   newer lane generation, and a newer peer-opened lane supersedes the open
//!   one (QUIC has no cross-stream order, so the reset of the old stream may
//!   arrive after the new stream);
//! - on the Deck, Pier-opened lanes that arrive before the control lane's
//!   `lane_attachment` is processed are held, one per role, unread, and
//!   validated in full at [`LaneRegistry::authorize`]; the Pier never holds
//!   a lane before authorization;
//! - a failed lane is never replaced by another role's stream: sends for that
//!   role fail until its own lane is reopened, disabled, or the attachment
//!   ends;
//! - recovery is lane-scoped: a video failure never changes audio or
//!   microphone state or generation.

use std::fmt::{Display, Formatter};
use std::num::{NonZeroU32, NonZeroU64};

use arcen_protocol::lanes::{
    LANE_ROLE_COUNT, LaneAttachment, LaneEndpoint, LanePreface, LaneRole, LaneRoleSet, LaneShape,
};
use arcen_protocol::{
    AUDIO_PRIORITY_MAX_FRAME_BYTES, MAX_MICROPHONE_OPUS_BYTES, MICROPHONE_HEADER_SIZE,
    MICROPHONE_PCM_BYTES,
};
use subtle::ConstantTimeEq;

use crate::BoundedTransportPolicy;

/// Peer-opened lanes the Deck holds per role while authorization is pending.
pub const MAX_PENDING_LANES_PER_ROLE: usize = 1;

/// Extra concurrent streams per shape a peer may open beyond the role table,
/// so a lane can be replaced while QUIC still accounts the reset stream.
pub const LANE_RECOVERY_HEADROOM: u32 = 1;

/// Number of lanes `opener` opens with `shape`, derived from the role table.
#[must_use]
pub const fn lanes_opened_by(opener: LaneEndpoint, shape: LaneShape) -> u32 {
    let mut count = 0;
    let mut index = 0;
    while index < LANE_ROLE_COUNT {
        let role = LaneRole::ALL[index];
        if endpoint_eq(role.opener(), opener) && shape_eq(role.shape(), shape) {
            count += 1;
        }
        index += 1;
    }
    count
}

const fn endpoint_eq(left: LaneEndpoint, right: LaneEndpoint) -> bool {
    matches!(
        (left, right),
        (LaneEndpoint::Pier, LaneEndpoint::Pier) | (LaneEndpoint::Deck, LaneEndpoint::Deck)
    )
}

const fn shape_eq(left: LaneShape, right: LaneShape) -> bool {
    matches!(
        (left, right),
        (LaneShape::Bidirectional, LaneShape::Bidirectional)
            | (LaneShape::Unidirectional, LaneShape::Unidirectional)
    )
}

/// QUIC concurrent-stream limits an endpoint grants its peer.
///
/// These are the lane carrier's values (`quic::lane_transport_config`). The
/// single-stream `recommended_transport_config` keeps its current limits
/// until a product switches to the lane dispatcher, because raising them
/// alone would only let a peer open streams nothing accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneStreamLimits {
    /// `max_concurrent_bidi_streams` granted to the peer.
    pub max_concurrent_bidi_streams: u32,
    /// `max_concurrent_uni_streams` granted to the peer.
    pub max_concurrent_uni_streams: u32,
}

impl LaneStreamLimits {
    /// Limits `local` grants the peer: the lanes the peer opens plus
    /// [`LANE_RECOVERY_HEADROOM`] for each shape the peer uses at all.
    #[must_use]
    pub const fn granted_by(local: LaneEndpoint) -> Self {
        let peer = local.peer();
        Self {
            max_concurrent_bidi_streams: with_headroom(lanes_opened_by(
                peer,
                LaneShape::Bidirectional,
            )),
            max_concurrent_uni_streams: with_headroom(lanes_opened_by(
                peer,
                LaneShape::Unidirectional,
            )),
        }
    }
}

const fn with_headroom(count: u32) -> u32 {
    if count == 0 {
        0
    } else {
        count + LANE_RECOVERY_HEADROOM
    }
}

/// Stream limits a Pier grants a Deck.
pub const PIER_LANE_STREAM_LIMITS: LaneStreamLimits =
    LaneStreamLimits::granted_by(LaneEndpoint::Pier);
/// Stream limits a Deck grants a Pier.
pub const DECK_LANE_STREAM_LIMITS: LaneStreamLimits =
    LaneStreamLimits::granted_by(LaneEndpoint::Deck);

/// Relative QUIC send priority for a lane (higher is sent first).
///
/// Audio keeps the existing priority-stream value (10, the same as
/// `quic::AUDIO_PRIORITY_STREAM_PRIORITY`). Priority is necessary but not
/// sufficient: lanes still share congestion and flow control, so video
/// queueing stays bounded by the shared pipeline queue policy.
#[must_use]
pub const fn lane_send_priority(role: LaneRole) -> i32 {
    match role {
        LaneRole::InputCursor => 30,
        LaneRole::Control => 20,
        LaneRole::AudioOutput | LaneRole::Microphone => 10,
        LaneRole::Video => 0,
        LaneRole::BulkAux => -10,
    }
}

/// Largest single framed message a lane accepts.
#[must_use]
pub const fn lane_max_frame_bytes(role: LaneRole, policy: &BoundedTransportPolicy) -> usize {
    match role {
        LaneRole::Control | LaneRole::BulkAux => policy.max_control_payload_bytes,
        LaneRole::Video => policy.max_media_payload_bytes,
        LaneRole::AudioOutput => AUDIO_PRIORITY_MAX_FRAME_BYTES,
        LaneRole::InputCursor => policy.max_input_payload_bytes,
        LaneRole::Microphone => {
            MICROPHONE_HEADER_SIZE
                + if MAX_MICROPHONE_OPUS_BYTES > MICROPHONE_PCM_BYTES {
                    MAX_MICROPHONE_OPUS_BYTES
                } else {
                    MICROPHONE_PCM_BYTES
                }
        }
    }
}

/// What happens when a lane exhausts its recovery attempts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneExhaustion {
    /// The feature is disabled visibly for the rest of the attachment.
    DisableVisibly,
    /// The attachment ends; the existing reconnect state machine takes over.
    EndAttachment,
}

/// Lane-specific recovery policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneRecoveryPolicy {
    /// Consecutive failures that may be recovered by reopening the lane.
    pub max_consecutive_failures: u8,
    /// Outcome once that bound is exceeded.
    pub exhausted: LaneExhaustion,
    /// Held keys/buttons/pen state must be released on failure.
    pub release_held_input: bool,
    /// The reopened lane must start a new prediction chain.
    pub requires_keyframe: bool,
}

impl LaneRecoveryPolicy {
    /// Policy for `role`.
    #[must_use]
    pub const fn for_role(role: LaneRole) -> Self {
        match role {
            LaneRole::Control => Self {
                max_consecutive_failures: 0,
                exhausted: LaneExhaustion::EndAttachment,
                release_held_input: true,
                requires_keyframe: false,
            },
            LaneRole::Video => Self {
                max_consecutive_failures: 3,
                exhausted: LaneExhaustion::EndAttachment,
                release_held_input: false,
                requires_keyframe: true,
            },
            LaneRole::AudioOutput | LaneRole::Microphone => Self {
                max_consecutive_failures: 3,
                exhausted: LaneExhaustion::DisableVisibly,
                release_held_input: false,
                requires_keyframe: false,
            },
            LaneRole::InputCursor => Self {
                max_consecutive_failures: 3,
                exhausted: LaneExhaustion::EndAttachment,
                release_held_input: true,
                requires_keyframe: false,
            },
            LaneRole::BulkAux => Self {
                max_consecutive_failures: 2,
                exhausted: LaneExhaustion::DisableVisibly,
                release_held_input: false,
                requires_keyframe: false,
            },
        }
    }
}

/// Why one lane failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneFailure {
    /// The peer reset or stopped the stream.
    PeerReset,
    /// The peer finished the stream while the attachment was live.
    PeerFinished,
    /// A local write failed before any byte of the frame was accepted.
    WriteFailed,
    /// A write failed after part of a length-prefixed frame was accepted,
    /// or a read ended inside one. The framing is broken.
    PartialFrame,
    /// A frame exceeded [`lane_max_frame_bytes`].
    Oversize,
    /// A frame failed validation.
    MalformedFrame,
}

impl LaneFailure {
    /// Whether the local endpoint must reset (not finish) the stream, so the
    /// peer never mistakes a truncated frame for a clean end. Payload already
    /// written is never resent on the replacement lane.
    #[must_use]
    pub const fn requires_reset(self) -> bool {
        matches!(
            self,
            Self::WriteFailed | Self::PartialFrame | Self::Oversize | Self::MalformedFrame
        )
    }

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::PeerReset => "peer_reset",
            Self::PeerFinished => "peer_finished",
            Self::WriteFailed => "write_failed",
            Self::PartialFrame => "partial_frame",
            Self::Oversize => "oversize",
            Self::MalformedFrame => "malformed_frame",
        }
    }
}

/// Why a lane was disabled for the rest of the attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneDisableReason {
    /// Its recovery bound was exhausted.
    RecoveryExhausted,
}

/// Why an attachment's lanes ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneEndReason {
    /// The control lane failed.
    ControlFailed,
    /// A lane whose loss ends the attachment exhausted recovery.
    LaneExhausted(LaneRole),
    /// The session closed normally.
    Closed,
}

impl LaneEndReason {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::ControlFailed => "control_failed",
            Self::LaneExhausted(_) => "lane_exhausted",
            Self::Closed => "closed",
        }
    }
}

/// Observable state of one role's lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneState {
    /// Never opened.
    Absent,
    /// Open with this generation.
    Open(NonZeroU32),
    /// Failed; awaiting a replacement with a newer generation.
    Recovering {
        /// Generation that failed.
        failed: NonZeroU32,
        /// Why.
        failure: LaneFailure,
    },
    /// Disabled for the rest of the attachment.
    Disabled(LaneDisableReason),
}

#[derive(Debug, Clone, Copy)]
struct LaneSlot {
    state: LaneState,
    highest: Option<NonZeroU32>,
    consecutive_failures: u8,
    /// At most [`MAX_PENDING_LANES_PER_ROLE`] peer lane held before
    /// authorization (Deck only).
    pending: Option<LanePreface>,
}

impl LaneSlot {
    const NEW: Self = Self {
        state: LaneState::Absent,
        highest: None,
        consecutive_failures: 0,
        pending: None,
    };
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    AwaitingControl,
    Unauthorized,
    Authorized {
        attachment: LaneAttachment,
        roles: LaneRoleSet,
    },
    Ended(LaneEndReason),
}

/// Why a stream was refused before any payload reached a worker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneRefusal {
    /// The attachment has ended.
    AttachmentEnded,
    /// A non-control lane arrived before the control lane.
    ControlNotFirst(LaneRole),
    /// A non-control lane arrived before authorization.
    PreAuthorization(LaneRole),
    /// The stream shape does not match the role.
    WrongShape {
        /// Declared role.
        role: LaneRole,
        /// Shape the role requires.
        expected: LaneShape,
    },
    /// The stream was opened by the endpoint that may not open this role,
    /// i.e. it would carry payload in the wrong direction.
    WrongDirection {
        /// Declared role.
        role: LaneRole,
        /// Endpoint that must open it.
        opener: LaneEndpoint,
    },
    /// The attachment grant does not include this role.
    RoleNotGranted(LaneRole),
    /// The preface names an older attachment generation.
    StaleAttachment {
        /// Generation presented.
        presented: NonZeroU64,
        /// Authorized generation.
        current: NonZeroU64,
    },
    /// The preface names another attachment generation or binding.
    AttachmentMismatch(LaneRole),
    /// The role already has an open lane.
    DuplicateLane(LaneRole),
    /// The lane generation is not newer than the last one seen.
    StaleLaneGeneration {
        /// Declared role.
        role: LaneRole,
        /// Generation presented.
        presented: NonZeroU32,
        /// Highest generation already seen.
        highest: NonZeroU32,
    },
    /// The role was disabled for this attachment.
    LaneDisabled(LaneRole),
}

impl LaneRefusal {
    /// Stable token for telemetry.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::AttachmentEnded => "attachment_ended",
            Self::ControlNotFirst(_) => "control_not_first",
            Self::PreAuthorization(_) => "pre_authorization",
            Self::WrongShape { .. } => "wrong_shape",
            Self::WrongDirection { .. } => "wrong_direction",
            Self::RoleNotGranted(_) => "role_not_granted",
            Self::StaleAttachment { .. } => "stale_attachment",
            Self::AttachmentMismatch(_) => "attachment_mismatch",
            Self::DuplicateLane(_) => "duplicate_lane",
            Self::StaleLaneGeneration { .. } => "stale_lane_generation",
            Self::LaneDisabled(_) => "lane_disabled",
        }
    }
}

impl Display for LaneRefusal {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "lane refused: {}", self.token())
    }
}

impl std::error::Error for LaneRefusal {}

/// A lane the registry admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneAdmission {
    /// Role.
    pub role: LaneRole,
    /// Lane generation now open.
    pub lane_generation: NonZeroU32,
    /// Attachment generation; `None` only for the control lane.
    pub attachment_generation: Option<NonZeroU64>,
    /// An older open lane of this role this lane replaced, with the steps
    /// the owner must apply to it. This is not a failure and does not consume
    /// the role's recovery budget.
    pub superseded: Option<SupersededLane>,
}

/// A lane replaced by a newer generation of its role, and the role's
/// recovery side effects the owner must apply before the new lane carries
/// payload. Supersession can happen before the old stream's reset arrives,
/// and the late reset is then reported as
/// [`LaneRecoveryAction::Superseded`], so these steps are carried here and
/// nowhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SupersededLane {
    /// Generation replaced.
    pub generation: NonZeroU32,
    /// Always true: stop the old stream and reset it, never finish it.
    pub reset_stream: bool,
    /// Release held keys/buttons/pen state from the old lane.
    pub release_held_input: bool,
    /// The new lane must start a new prediction chain.
    pub request_keyframe: bool,
}

impl SupersededLane {
    /// The replacement steps [`LaneRecoveryPolicy::for_role`] prescribes.
    #[must_use]
    pub const fn for_role(role: LaneRole, generation: NonZeroU32) -> Self {
        let policy = LaneRecoveryPolicy::for_role(role);
        Self {
            generation,
            reset_stream: true,
            release_held_input: policy.release_held_input,
            request_keyframe: policy.requires_keyframe,
        }
    }
}

/// Outcome of [`LaneRegistry::admit_peer`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLaneAdmission {
    /// The lane is open; hand it to its worker.
    Admitted(LaneAdmission),
    /// Deck only: the lane arrived before authorization. The owner keeps the
    /// stream unread until [`LaneRegistry::authorize`] resolves it, and
    /// resets it if the attachment ends first.
    PendingAuthorization {
        /// Role.
        role: LaneRole,
        /// Held lane generation.
        lane_generation: NonZeroU32,
        /// An older held lane of this role this one replaced, with the
        /// role's replacement steps. The held stream was never read, so the
        /// steps are conservative; the owner still resets that stream.
        superseded: Option<SupersededLane>,
    },
}

/// How [`LaneRegistry::authorize`] resolved one held lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingResolution {
    /// Role.
    pub role: LaneRole,
    /// Held lane generation.
    pub lane_generation: NonZeroU32,
    /// Admitted, or refused with the reason a fully validated lane gets.
    pub outcome: Result<LaneAdmission, LaneRefusal>,
}

/// Misuse of the registry by its owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneRegistryError {
    /// Authorization requires an open control lane.
    ControlNotOpen,
    /// The attachment was already authorized on this connection.
    AlreadyAuthorized,
    /// The attachment has ended.
    AttachmentEnded,
    /// The local endpoint does not open this role.
    NotLocalOpener(LaneRole),
}

impl Display for LaneRegistryError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::ControlNotOpen => "authorization requires an open control lane",
            Self::AlreadyAuthorized => "attachment already authorized",
            Self::AttachmentEnded => "attachment ended",
            Self::NotLocalOpener(_) => "the local endpoint does not open this lane",
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for LaneRegistryError {}

/// Why a payload for a role cannot be sent now. There is deliberately no
/// suggestion of another lane: the caller drops or holds the payload under
/// that role's own policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneUnavailable {
    /// The local endpoint may not send on this role.
    WrongDirection(LaneRole),
    /// The role's lane is not open; carries its current state.
    NotOpen(LaneRole, LaneState),
    /// The attachment has ended.
    AttachmentEnded,
}

/// What the owner must do after a lane failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneRecoveryAction {
    /// The failure names a generation that is no longer current; nothing
    /// changes.
    Superseded,
    /// Reopen only this role's lane with a newer generation.
    Reopen {
        /// Role to reopen.
        role: LaneRole,
        /// Generation that failed.
        failed: NonZeroU32,
        /// Endpoint that opens the replacement.
        reopened_by: LaneEndpoint,
        /// Reset (not finish) the failed stream.
        reset_stream: bool,
        /// The replacement must start with a keyframe.
        request_keyframe: bool,
        /// Release held keys/buttons/pen before input resumes.
        release_held_input: bool,
    },
    /// Disable this role visibly for the rest of the attachment.
    Disable {
        /// Role disabled.
        role: LaneRole,
        /// Reset (not finish) the failed stream.
        reset_stream: bool,
    },
    /// End the attachment; held input is released and the existing reconnect
    /// state machine decides what happens next.
    EndAttachment {
        /// Role whose failure ended it.
        role: LaneRole,
        /// Reset (not finish) the failed stream.
        reset_stream: bool,
        /// Why.
        reason: LaneEndReason,
    },
}

impl LaneRecoveryAction {
    /// Stable token for telemetry.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Superseded => "superseded",
            Self::Reopen { .. } => "reopen",
            Self::Disable { .. } => "disable",
            Self::EndAttachment { .. } => "end_attachment",
        }
    }
}

/// Lane ownership state for one attachment on one connection.
#[derive(Debug, Clone)]
pub struct LaneRegistry {
    local: LaneEndpoint,
    phase: Phase,
    lanes: [LaneSlot; LANE_ROLE_COUNT],
}

impl LaneRegistry {
    /// A registry for a new connection.
    #[must_use]
    pub const fn new(local: LaneEndpoint) -> Self {
        Self {
            local,
            phase: Phase::AwaitingControl,
            lanes: [LaneSlot::NEW; LANE_ROLE_COUNT],
        }
    }

    /// Local endpoint.
    #[must_use]
    pub const fn local(&self) -> LaneEndpoint {
        self.local
    }

    /// Authorized attachment, once granted.
    #[must_use]
    pub const fn attachment(&self) -> Option<&LaneAttachment> {
        match &self.phase {
            Phase::Authorized { attachment, .. } => Some(attachment),
            _ => None,
        }
    }

    /// Why the attachment ended, if it did.
    #[must_use]
    pub const fn ended(&self) -> Option<LaneEndReason> {
        match self.phase {
            Phase::Ended(reason) => Some(reason),
            _ => None,
        }
    }

    /// Current state of a role's lane.
    #[must_use]
    pub const fn state(&self, role: LaneRole) -> LaneState {
        self.lanes[role.index()].state
    }

    /// Prepares the preface for a lane the local endpoint opens, reserving
    /// the next lane generation.
    ///
    /// # Errors
    ///
    /// [`LaneRegistryError::NotLocalOpener`] when the peer opens this role,
    /// or the [`LaneRefusal`] the peer would apply, mapped to a registry
    /// error, when the lane may not be opened now.
    pub fn open_local(&mut self, role: LaneRole) -> Result<LanePreface, LaneOpenError> {
        if !endpoint_eq(role.opener(), self.local) {
            return Err(LaneOpenError::Registry(LaneRegistryError::NotLocalOpener(
                role,
            )));
        }
        let generation = self.next_generation(role);
        let preface = if role.requires_attachment() {
            let Phase::Authorized { attachment, .. } = self.phase else {
                return Err(LaneOpenError::Refused(self.unauthorized_refusal(role)));
            };
            LanePreface::bound(role, attachment, generation)
                .map_err(|_| LaneOpenError::Refused(LaneRefusal::AttachmentMismatch(role)))?
        } else {
            LanePreface::control(generation)
        };
        self.register(role.shape(), &preface, false)
            .map_err(LaneOpenError::Refused)?;
        Ok(preface)
    }

    /// Admits, holds or refuses a stream the peer opened, from its shape and
    /// its decoded preface. Nothing on a refused or held stream may reach a
    /// worker.
    ///
    /// # Errors
    ///
    /// The first [`LaneRefusal`] that applies.
    pub fn admit_peer(
        &mut self,
        shape: LaneShape,
        preface: &LanePreface,
    ) -> Result<PeerLaneAdmission, LaneRefusal> {
        let role = preface.role();
        if !endpoint_eq(role.opener(), self.local.peer()) {
            return Err(LaneRefusal::WrongDirection {
                role,
                opener: role.opener(),
            });
        }
        if matches!(self.phase, Phase::Unauthorized)
            && endpoint_eq(self.local, LaneEndpoint::Deck)
            && role.requires_attachment()
        {
            return self.hold_pending(shape, preface);
        }
        self.register(shape, preface, true)
            .map(PeerLaneAdmission::Admitted)
    }

    fn hold_pending(
        &mut self,
        shape: LaneShape,
        preface: &LanePreface,
    ) -> Result<PeerLaneAdmission, LaneRefusal> {
        let role = preface.role();
        if !shape_eq(shape, role.shape()) {
            return Err(LaneRefusal::WrongShape {
                role,
                expected: role.shape(),
            });
        }
        let presented = preface.lane_generation();
        let slot = &mut self.lanes[role.index()];
        let superseded = match slot.pending {
            Some(held) if presented <= held.lane_generation() => {
                return Err(LaneRefusal::StaleLaneGeneration {
                    role,
                    presented,
                    highest: held.lane_generation(),
                });
            }
            held => held.map(|held| SupersededLane::for_role(role, held.lane_generation())),
        };
        slot.pending = Some(*preface);
        Ok(PeerLaneAdmission::PendingAuthorization {
            role,
            lane_generation: presented,
            superseded,
        })
    }

    fn register(
        &mut self,
        shape: LaneShape,
        preface: &LanePreface,
        allow_supersede: bool,
    ) -> Result<LaneAdmission, LaneRefusal> {
        let role = preface.role();
        if matches!(self.phase, Phase::Ended(_)) {
            return Err(LaneRefusal::AttachmentEnded);
        }
        if !shape_eq(shape, role.shape()) {
            return Err(LaneRefusal::WrongShape {
                role,
                expected: role.shape(),
            });
        }
        let attachment_generation = if role.requires_attachment() {
            let Phase::Authorized { attachment, roles } = self.phase else {
                return Err(self.unauthorized_refusal(role));
            };
            if !roles.contains(role) {
                return Err(LaneRefusal::RoleNotGranted(role));
            }
            let presented = preface
                .attachment()
                .ok_or(LaneRefusal::AttachmentMismatch(role))?;
            if presented.generation < attachment.generation {
                return Err(LaneRefusal::StaleAttachment {
                    presented: presented.generation,
                    current: attachment.generation,
                });
            }
            let binding_matches: bool = presented
                .binding
                .as_bytes()
                .ct_eq(attachment.binding.as_bytes())
                .into();
            if presented.generation != attachment.generation || !binding_matches {
                return Err(LaneRefusal::AttachmentMismatch(role));
            }
            Some(attachment.generation)
        } else {
            if !matches!(self.phase, Phase::AwaitingControl) {
                return Err(LaneRefusal::DuplicateLane(role));
            }
            None
        };
        let presented = preface.lane_generation();
        let slot = &mut self.lanes[role.index()];
        let superseded = match slot.state {
            LaneState::Disabled(_) => return Err(LaneRefusal::LaneDisabled(role)),
            LaneState::Open(open) if !allow_supersede || presented == open => {
                return Err(LaneRefusal::DuplicateLane(role));
            }
            LaneState::Open(open) => Some(SupersededLane::for_role(role, open)),
            LaneState::Absent | LaneState::Recovering { .. } => None,
        };
        if let Some(highest) = slot.highest.filter(|highest| presented <= *highest) {
            return Err(LaneRefusal::StaleLaneGeneration {
                role,
                presented,
                highest,
            });
        }
        slot.highest = Some(presented);
        slot.state = LaneState::Open(presented);
        if !role.requires_attachment() {
            self.phase = Phase::Unauthorized;
        }
        Ok(LaneAdmission {
            role,
            lane_generation: presented,
            attachment_generation,
            superseded,
        })
    }

    const fn unauthorized_refusal(&self, role: LaneRole) -> LaneRefusal {
        match self.phase {
            Phase::AwaitingControl => LaneRefusal::ControlNotFirst(role),
            Phase::Ended(_) => LaneRefusal::AttachmentEnded,
            Phase::Unauthorized | Phase::Authorized { .. } => LaneRefusal::PreAuthorization(role),
        }
    }

    fn next_generation(&self, role: LaneRole) -> NonZeroU32 {
        self.lanes[role.index()]
            .highest
            .and_then(|highest| highest.checked_add(1))
            .unwrap_or(NonZeroU32::MIN)
    }

    /// Records successful authentication on the control lane. The Pier calls
    /// this before sending the attachment grant; the Deck after validating
    /// it. Control is always granted; `roles` lists the other enabled lanes.
    ///
    /// Lanes the Deck held while authorization was pending are validated in
    /// full against the grant (role, attachment generation, binding, lane
    /// generation) and returned, admitted or refused, in role order.
    ///
    /// # Errors
    ///
    /// [`LaneRegistryError`] when the control lane is not open, the
    /// attachment is already authorized, or it ended.
    pub fn authorize(
        &mut self,
        attachment: LaneAttachment,
        roles: LaneRoleSet,
    ) -> Result<Vec<PendingResolution>, LaneRegistryError> {
        match self.phase {
            Phase::AwaitingControl => return Err(LaneRegistryError::ControlNotOpen),
            Phase::Authorized { .. } => return Err(LaneRegistryError::AlreadyAuthorized),
            Phase::Ended(_) => return Err(LaneRegistryError::AttachmentEnded),
            Phase::Unauthorized => {}
        }
        self.phase = Phase::Authorized {
            attachment,
            roles: roles.with(LaneRole::Control),
        };
        let mut resolutions = Vec::new();
        for role in LaneRole::ALL {
            let Some(held) = self.lanes[role.index()].pending.take() else {
                continue;
            };
            resolutions.push(PendingResolution {
                role,
                lane_generation: held.lane_generation(),
                outcome: self.register(role.shape(), &held, true),
            });
        }
        Ok(resolutions)
    }

    /// Generation of the lane held for `role` pending authorization.
    #[must_use]
    pub fn pending(&self, role: LaneRole) -> Option<NonZeroU32> {
        self.lanes[role.index()]
            .pending
            .map(|held| held.lane_generation())
    }

    fn drop_pending(&mut self) {
        for slot in &mut self.lanes {
            slot.pending = None;
        }
    }

    /// Whether the local endpoint may send a payload for `role` now, and on
    /// which lane generation.
    ///
    /// # Errors
    ///
    /// [`LaneUnavailable`]; the payload is never routed to another role.
    pub fn check_send(&self, role: LaneRole) -> Result<NonZeroU32, LaneUnavailable> {
        if matches!(self.phase, Phase::Ended(_)) {
            return Err(LaneUnavailable::AttachmentEnded);
        }
        if !role.may_send(self.local) {
            return Err(LaneUnavailable::WrongDirection(role));
        }
        match self.state(role) {
            LaneState::Open(generation) => Ok(generation),
            state => Err(LaneUnavailable::NotOpen(role, state)),
        }
    }

    /// Records that `role`'s lane delivered successfully, clearing its
    /// consecutive-failure count.
    pub fn mark_healthy(&mut self, role: LaneRole, generation: NonZeroU32) {
        let slot = &mut self.lanes[role.index()];
        if slot.state == LaneState::Open(generation) {
            slot.consecutive_failures = 0;
        }
    }

    /// Records a failure of `role`'s lane `generation` and returns the
    /// lane-scoped recovery. Other roles' state and generations never change,
    /// except that ending the attachment ends every lane.
    pub fn report_failure(
        &mut self,
        role: LaneRole,
        generation: NonZeroU32,
        failure: LaneFailure,
    ) -> LaneRecoveryAction {
        if matches!(self.phase, Phase::Ended(_)) {
            return LaneRecoveryAction::Superseded;
        }
        let policy = LaneRecoveryPolicy::for_role(role);
        let reset_stream = failure.requires_reset();
        let slot = &mut self.lanes[role.index()];
        if slot.state != LaneState::Open(generation) {
            return LaneRecoveryAction::Superseded;
        }
        slot.consecutive_failures = slot.consecutive_failures.saturating_add(1);
        if role == LaneRole::Control {
            slot.state = LaneState::Recovering {
                failed: generation,
                failure,
            };
            self.phase = Phase::Ended(LaneEndReason::ControlFailed);
            self.drop_pending();
            return LaneRecoveryAction::EndAttachment {
                role,
                reset_stream,
                reason: LaneEndReason::ControlFailed,
            };
        }
        if slot.consecutive_failures > policy.max_consecutive_failures {
            return match policy.exhausted {
                LaneExhaustion::DisableVisibly => {
                    slot.state = LaneState::Disabled(LaneDisableReason::RecoveryExhausted);
                    LaneRecoveryAction::Disable { role, reset_stream }
                }
                LaneExhaustion::EndAttachment => {
                    slot.state = LaneState::Recovering {
                        failed: generation,
                        failure,
                    };
                    let reason = LaneEndReason::LaneExhausted(role);
                    self.phase = Phase::Ended(reason);
                    self.drop_pending();
                    LaneRecoveryAction::EndAttachment {
                        role,
                        reset_stream,
                        reason,
                    }
                }
            };
        }
        slot.state = LaneState::Recovering {
            failed: generation,
            failure,
        };
        LaneRecoveryAction::Reopen {
            role,
            failed: generation,
            reopened_by: role.opener(),
            reset_stream,
            request_keyframe: policy.requires_keyframe,
            release_held_input: policy.release_held_input,
        }
    }

    /// Ends the attachment, for example on normal session close. Held
    /// lanes are dropped; the owner resets every stream it still holds.
    pub fn end(&mut self, reason: LaneEndReason) {
        self.phase = Phase::Ended(reason);
        self.drop_pending();
    }
}

/// Why a locally opened lane could not be prepared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneOpenError {
    /// The registry was asked to do something its endpoint cannot.
    Registry(LaneRegistryError),
    /// The lane may not be opened now.
    Refused(LaneRefusal),
}

impl Display for LaneOpenError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registry(error) => Display::fmt(error, formatter),
            Self::Refused(refusal) => Display::fmt(refusal, formatter),
        }
    }
}

impl std::error::Error for LaneOpenError {}

#[cfg(test)]
mod tests {
    use super::*;
    use arcen_protocol::lanes::{LANE_ATTACHMENT_BINDING_BYTES, LaneAttachmentBinding};

    fn nz(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn attachment(generation: u64, fill: u8) -> LaneAttachment {
        LaneAttachment {
            generation: NonZeroU64::new(generation).unwrap(),
            binding: LaneAttachmentBinding::new([fill; LANE_ATTACHMENT_BINDING_BYTES]).unwrap(),
        }
    }

    fn bound(role: LaneRole, generation: u32) -> LanePreface {
        LanePreface::bound(role, attachment(4, 0x5A), nz(generation)).unwrap()
    }

    #[test]
    fn the_bulk_lane_carries_a_maximal_clipboard_chunk() {
        use arcen_protocol::lanes::{SESSION_RECORD_LENGTH_BYTES, SessionRecord};
        use arcen_protocol::wire::{CLIPBOARD_CHUNK_PAYLOAD_BYTES, CLIPBOARD_HEADER_SIZE};

        let max = lane_max_frame_bytes(LaneRole::BulkAux, &BoundedTransportPolicy::default());
        let record = SessionRecord::Binary(vec![
            0;
            CLIPBOARD_HEADER_SIZE
                + CLIPBOARD_CHUNK_PAYLOAD_BYTES
        ])
        .encode();
        // The writer bounds the whole record, the reader the body after the
        // length prefix.
        assert!(record.len() <= max, "{} > {max}", record.len());
        assert!(record.len() - SESSION_RECORD_LENGTH_BYTES <= max);
    }

    fn admitted(result: Result<PeerLaneAdmission, LaneRefusal>) -> LaneAdmission {
        match result {
            Ok(PeerLaneAdmission::Admitted(admission)) => admission,
            other => panic!("expected admission, got {other:?}"),
        }
    }

    /// A Pier registry with control open and every role granted.
    fn authorized_pier() -> LaneRegistry {
        let mut pier = LaneRegistry::new(LaneEndpoint::Pier);
        pier.admit_peer(LaneShape::Bidirectional, &LanePreface::control(nz(1)))
            .unwrap();
        pier.authorize(attachment(4, 0x5A), LaneRoleSet::all())
            .unwrap();
        pier
    }

    #[test]
    fn stream_limits_follow_the_role_table() {
        assert_eq!(
            lanes_opened_by(LaneEndpoint::Deck, LaneShape::Bidirectional),
            3
        );
        assert_eq!(
            lanes_opened_by(LaneEndpoint::Deck, LaneShape::Unidirectional),
            1
        );
        assert_eq!(
            lanes_opened_by(LaneEndpoint::Pier, LaneShape::Bidirectional),
            0
        );
        assert_eq!(
            lanes_opened_by(LaneEndpoint::Pier, LaneShape::Unidirectional),
            2
        );
        assert_eq!(
            PIER_LANE_STREAM_LIMITS,
            LaneStreamLimits {
                max_concurrent_bidi_streams: 4,
                max_concurrent_uni_streams: 2,
            }
        );
        assert_eq!(
            DECK_LANE_STREAM_LIMITS,
            LaneStreamLimits {
                max_concurrent_bidi_streams: 0,
                max_concurrent_uni_streams: 3,
            }
        );
    }

    #[test]
    fn frame_bounds_reuse_existing_limits() {
        let policy = BoundedTransportPolicy::default();
        assert_eq!(
            lane_max_frame_bytes(LaneRole::AudioOutput, &policy),
            AUDIO_PRIORITY_MAX_FRAME_BYTES
        );
        assert_eq!(
            lane_max_frame_bytes(LaneRole::Video, &policy),
            policy.max_media_payload_bytes
        );
        assert_eq!(
            lane_max_frame_bytes(LaneRole::Microphone, &policy),
            MICROPHONE_HEADER_SIZE + MICROPHONE_PCM_BYTES
        );
        assert!(lane_send_priority(LaneRole::InputCursor) > lane_send_priority(LaneRole::Video));
        assert!(lane_send_priority(LaneRole::AudioOutput) > lane_send_priority(LaneRole::Video));
        assert!(lane_send_priority(LaneRole::Video) > lane_send_priority(LaneRole::BulkAux));
    }

    #[test]
    fn pre_auth_streams_are_refused_before_any_payload() {
        let mut pier = LaneRegistry::new(LaneEndpoint::Pier);
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 1)),
            Err(LaneRefusal::ControlNotFirst(LaneRole::InputCursor))
        );
        let control =
            admitted(pier.admit_peer(LaneShape::Bidirectional, &LanePreface::control(nz(1))));
        assert_eq!(control.attachment_generation, None);
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 1)),
            Err(LaneRefusal::PreAuthorization(LaneRole::InputCursor))
        );
        assert_eq!(
            pier.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Microphone, 1)),
            Err(LaneRefusal::PreAuthorization(LaneRole::Microphone))
        );
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &LanePreface::control(nz(2))),
            Err(LaneRefusal::DuplicateLane(LaneRole::Control))
        );
        assert_eq!(pier.state(LaneRole::InputCursor), LaneState::Absent);
    }

    #[test]
    fn authorization_requires_control_and_happens_once() {
        let mut pier = LaneRegistry::new(LaneEndpoint::Pier);
        assert_eq!(
            pier.authorize(attachment(1, 1), LaneRoleSet::all()),
            Err(LaneRegistryError::ControlNotOpen)
        );
        let mut pier = authorized_pier();
        assert_eq!(
            pier.authorize(attachment(5, 1), LaneRoleSet::all()),
            Err(LaneRegistryError::AlreadyAuthorized)
        );
    }

    #[test]
    fn wrong_role_shape_and_direction_are_refused() {
        let mut pier = authorized_pier();
        // A Deck claiming Video (Pier-opened) on its own uni stream.
        assert_eq!(
            pier.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 1)),
            Err(LaneRefusal::WrongDirection {
                role: LaneRole::Video,
                opener: LaneEndpoint::Pier
            })
        );
        // Microphone must be unidirectional.
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::Microphone, 1)),
            Err(LaneRefusal::WrongShape {
                role: LaneRole::Microphone,
                expected: LaneShape::Unidirectional
            })
        );
        let mut deck = LaneRegistry::new(LaneEndpoint::Deck);
        deck.open_local(LaneRole::Control).unwrap();
        deck.authorize(attachment(4, 0x5A), LaneRoleSet::all())
            .unwrap();
        // A Pier claiming Microphone (Deck-opened).
        assert_eq!(
            deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Microphone, 1)),
            Err(LaneRefusal::WrongDirection {
                role: LaneRole::Microphone,
                opener: LaneEndpoint::Deck
            })
        );
        assert_eq!(
            deck.open_local(LaneRole::Video),
            Err(LaneOpenError::Registry(LaneRegistryError::NotLocalOpener(
                LaneRole::Video
            )))
        );
        // A Deck may not send on Video, and a Pier may not send microphone.
        assert_eq!(
            deck.check_send(LaneRole::Video),
            Err(LaneUnavailable::WrongDirection(LaneRole::Video))
        );
    }

    #[test]
    fn roles_outside_the_grant_are_refused() {
        let mut pier = LaneRegistry::new(LaneEndpoint::Pier);
        pier.admit_peer(LaneShape::Bidirectional, &LanePreface::control(nz(1)))
            .unwrap();
        pier.authorize(
            attachment(4, 0x5A),
            LaneRoleSet::from_roles(&[LaneRole::Video, LaneRole::InputCursor]),
        )
        .unwrap();
        assert_eq!(
            pier.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Microphone, 1)),
            Err(LaneRefusal::RoleNotGranted(LaneRole::Microphone))
        );
        assert!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 1))
                .is_ok()
        );
    }

    #[test]
    fn stale_or_foreign_attachments_are_refused() {
        let mut pier = authorized_pier();
        let stale = LanePreface::bound(LaneRole::InputCursor, attachment(3, 0x5A), nz(1)).unwrap();
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &stale),
            Err(LaneRefusal::StaleAttachment {
                presented: NonZeroU64::new(3).unwrap(),
                current: NonZeroU64::new(4).unwrap()
            })
        );
        let future = LanePreface::bound(LaneRole::InputCursor, attachment(5, 0x5A), nz(1)).unwrap();
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &future),
            Err(LaneRefusal::AttachmentMismatch(LaneRole::InputCursor))
        );
        let forged = LanePreface::bound(LaneRole::InputCursor, attachment(4, 0x11), nz(1)).unwrap();
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &forged),
            Err(LaneRefusal::AttachmentMismatch(LaneRole::InputCursor))
        );
        assert_eq!(pier.state(LaneRole::InputCursor), LaneState::Absent);
    }

    #[test]
    fn duplicate_lanes_and_stale_generations_are_refused() {
        let mut pier = authorized_pier();
        admitted(pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 2)));
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 2)),
            Err(LaneRefusal::DuplicateLane(LaneRole::InputCursor))
        );
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 1)),
            Err(LaneRefusal::StaleLaneGeneration {
                role: LaneRole::InputCursor,
                presented: nz(1),
                highest: nz(2)
            })
        );
        let action = pier.report_failure(LaneRole::InputCursor, nz(2), LaneFailure::PeerReset);
        assert!(matches!(action, LaneRecoveryAction::Reopen { .. }));
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 2)),
            Err(LaneRefusal::StaleLaneGeneration {
                role: LaneRole::InputCursor,
                presented: nz(2),
                highest: nz(2)
            })
        );
        let reopened =
            admitted(pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 3)));
        assert_eq!(reopened.lane_generation, nz(3));
        assert_eq!(reopened.superseded, None);
    }

    #[test]
    fn a_newer_peer_lane_supersedes_the_open_one_without_failing() {
        let mut pier = authorized_pier();
        admitted(pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 1)));
        // The opener reset g1 and opened g2; QUIC delivered g2 first.
        let replacement =
            admitted(pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::InputCursor, 2)));
        assert_eq!(replacement.lane_generation, nz(2));
        assert_eq!(
            replacement.superseded,
            Some(SupersededLane {
                generation: nz(1),
                reset_stream: true,
                release_held_input: true,
                request_keyframe: false,
            }),
            "superseding an input lane must release held input"
        );
        assert_eq!(pier.state(LaneRole::InputCursor), LaneState::Open(nz(2)));
        // The late reset of g1 is not a failure of the live lane.
        assert_eq!(
            pier.report_failure(LaneRole::InputCursor, nz(1), LaneFailure::PeerReset),
            LaneRecoveryAction::Superseded
        );
        // Supersession consumed no recovery budget: three failures still reopen.
        for generation in 2..5 {
            assert!(matches!(
                pier.report_failure(
                    LaneRole::InputCursor,
                    nz(generation),
                    LaneFailure::PeerReset
                ),
                LaneRecoveryAction::Reopen { .. }
            ));
            admitted(pier.admit_peer(
                LaneShape::Bidirectional,
                &bound(LaneRole::InputCursor, generation + 1),
            ));
        }
        // A locally opened lane never silently replaces its own open lane.
        let video = pier.open_local(LaneRole::Video).unwrap();
        assert_eq!(
            pier.open_local(LaneRole::Video),
            Err(LaneOpenError::Refused(LaneRefusal::DuplicateLane(
                LaneRole::Video
            )))
        );
        assert_eq!(
            pier.check_send(LaneRole::Video),
            Ok(video.lane_generation())
        );
    }

    #[test]
    fn video_supersession_requests_video_recovery_only() {
        let mut deck = LaneRegistry::new(LaneEndpoint::Deck);
        deck.open_local(LaneRole::Control).unwrap();
        deck.authorize(attachment(4, 0x5A), LaneRoleSet::all())
            .unwrap();
        let mic = deck.open_local(LaneRole::Microphone).unwrap();
        admitted(deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 1)));
        admitted(deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::AudioOutput, 1)));
        let audio_before = deck.state(LaneRole::AudioOutput);
        let mic_before = deck.state(LaneRole::Microphone);

        let replacement =
            admitted(deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 2)));
        let policy = LaneRecoveryPolicy::for_role(LaneRole::Video);
        assert_eq!(
            replacement.superseded,
            Some(SupersededLane {
                generation: nz(1),
                reset_stream: true,
                release_held_input: policy.release_held_input,
                request_keyframe: policy.requires_keyframe,
            })
        );
        assert!(replacement.superseded.unwrap().request_keyframe);
        assert!(!replacement.superseded.unwrap().release_held_input);
        assert_eq!(deck.state(LaneRole::AudioOutput), audio_before);
        assert_eq!(deck.state(LaneRole::Microphone), mic_before);
        assert_eq!(
            deck.check_send(LaneRole::Microphone),
            Ok(mic.lane_generation())
        );
        // The late reset of the replaced video lane changes nothing.
        assert_eq!(
            deck.report_failure(LaneRole::Video, nz(1), LaneFailure::PeerReset),
            LaneRecoveryAction::Superseded
        );
        assert_eq!(deck.state(LaneRole::Video), LaneState::Open(nz(2)));
    }

    #[test]
    fn supersession_carries_each_roles_recovery_steps() {
        for role in LaneRole::ALL {
            let policy = LaneRecoveryPolicy::for_role(role);
            let superseded = SupersededLane::for_role(role, nz(7));
            assert!(superseded.reset_stream);
            assert_eq!(superseded.release_held_input, policy.release_held_input);
            assert_eq!(superseded.request_keyframe, policy.requires_keyframe);
        }
        assert!(SupersededLane::for_role(LaneRole::InputCursor, nz(1)).release_held_input);
    }

    #[test]
    fn deck_holds_early_pier_lanes_until_authorization() {
        let mut deck = LaneRegistry::new(LaneEndpoint::Deck);
        deck.open_local(LaneRole::Control).unwrap();
        // Video and audio overtake the lane_attachment message.
        assert_eq!(
            deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 1)),
            Ok(PeerLaneAdmission::PendingAuthorization {
                role: LaneRole::Video,
                lane_generation: nz(1),
                superseded: None,
            })
        );
        assert!(matches!(
            deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::AudioOutput, 1)),
            Ok(PeerLaneAdmission::PendingAuthorization { .. })
        ));
        // One held lane per role: a newer one replaces it, an older is stale.
        assert_eq!(
            deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 2)),
            Ok(PeerLaneAdmission::PendingAuthorization {
                role: LaneRole::Video,
                lane_generation: nz(2),
                superseded: Some(SupersededLane::for_role(LaneRole::Video, nz(1))),
            })
        );
        assert!(matches!(
            deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 2)),
            Err(LaneRefusal::StaleLaneGeneration { .. })
        ));
        assert!(matches!(
            deck.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::Video, 3)),
            Err(LaneRefusal::WrongShape { .. })
        ));
        assert_eq!(deck.pending(LaneRole::Video), Some(nz(2)));
        assert_eq!(deck.state(LaneRole::Video), LaneState::Absent);
        assert!(deck.check_send(LaneRole::Microphone).is_err());

        let resolutions = deck
            .authorize(attachment(4, 0x5A), LaneRoleSet::all())
            .unwrap();
        assert_eq!(resolutions.len(), 2);
        assert_eq!(resolutions[0].role, LaneRole::Video);
        assert_eq!(resolutions[0].outcome.unwrap().lane_generation, nz(2));
        assert_eq!(resolutions[1].role, LaneRole::AudioOutput);
        assert!(resolutions[1].outcome.is_ok());
        assert_eq!(deck.state(LaneRole::Video), LaneState::Open(nz(2)));
        assert_eq!(deck.pending(LaneRole::Video), None);
    }

    #[test]
    fn held_lanes_are_revalidated_against_the_grant() {
        let mut deck = LaneRegistry::new(LaneEndpoint::Deck);
        deck.open_local(LaneRole::Control).unwrap();
        let forged = LanePreface::bound(LaneRole::Video, attachment(4, 0x11), nz(1)).unwrap();
        deck.admit_peer(LaneShape::Unidirectional, &forged).unwrap();
        deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::AudioOutput, 1))
            .unwrap();
        let resolutions = deck
            .authorize(
                attachment(4, 0x5A),
                LaneRoleSet::from_roles(&[LaneRole::Video]),
            )
            .unwrap();
        assert_eq!(
            resolutions[0].outcome,
            Err(LaneRefusal::AttachmentMismatch(LaneRole::Video))
        );
        assert_eq!(
            resolutions[1].outcome,
            Err(LaneRefusal::RoleNotGranted(LaneRole::AudioOutput))
        );
        assert_eq!(deck.state(LaneRole::Video), LaneState::Absent);
        assert_eq!(deck.state(LaneRole::AudioOutput), LaneState::Absent);
    }

    #[test]
    fn held_lanes_are_dropped_when_the_attachment_ends() {
        let mut deck = LaneRegistry::new(LaneEndpoint::Deck);
        let control = deck.open_local(LaneRole::Control).unwrap();
        deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 1))
            .unwrap();
        deck.report_failure(
            LaneRole::Control,
            control.lane_generation(),
            LaneFailure::PeerReset,
        );
        assert_eq!(deck.pending(LaneRole::Video), None);
        assert_eq!(
            deck.authorize(attachment(4, 0x5A), LaneRoleSet::all()),
            Err(LaneRegistryError::AttachmentEnded)
        );

        let mut deck = LaneRegistry::new(LaneEndpoint::Deck);
        deck.open_local(LaneRole::Control).unwrap();
        deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::AudioOutput, 1))
            .unwrap();
        deck.end(LaneEndReason::Closed);
        assert_eq!(deck.pending(LaneRole::AudioOutput), None);
        assert_eq!(
            deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::AudioOutput, 2)),
            Err(LaneRefusal::AttachmentEnded)
        );
    }

    #[test]
    fn the_pier_never_holds_a_lane_before_authorization() {
        let mut pier = LaneRegistry::new(LaneEndpoint::Pier);
        pier.admit_peer(LaneShape::Bidirectional, &LanePreface::control(nz(1)))
            .unwrap();
        assert_eq!(
            pier.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Microphone, 1)),
            Err(LaneRefusal::PreAuthorization(LaneRole::Microphone))
        );
        assert_eq!(pier.pending(LaneRole::Microphone), None);
    }

    #[test]
    fn video_recovery_never_touches_audio_or_microphone() {
        let mut deck = LaneRegistry::new(LaneEndpoint::Deck);
        deck.open_local(LaneRole::Control).unwrap();
        deck.authorize(attachment(4, 0x5A), LaneRoleSet::all())
            .unwrap();
        let mic = deck.open_local(LaneRole::Microphone).unwrap();
        deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 1))
            .unwrap();
        deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::AudioOutput, 1))
            .unwrap();
        let audio_before = deck.state(LaneRole::AudioOutput);
        let mic_before = deck.state(LaneRole::Microphone);

        let action = deck.report_failure(LaneRole::Video, nz(1), LaneFailure::PartialFrame);
        assert_eq!(
            action,
            LaneRecoveryAction::Reopen {
                role: LaneRole::Video,
                failed: nz(1),
                reopened_by: LaneEndpoint::Pier,
                reset_stream: true,
                request_keyframe: true,
                release_held_input: false,
            }
        );
        assert_eq!(deck.state(LaneRole::AudioOutput), audio_before);
        assert_eq!(deck.state(LaneRole::Microphone), mic_before);
        assert_eq!(
            deck.check_send(LaneRole::Microphone),
            Ok(mic.lane_generation())
        );
        assert_eq!(
            admitted(deck.admit_peer(LaneShape::Unidirectional, &bound(LaneRole::Video, 2)))
                .lane_generation,
            nz(2)
        );
        assert_eq!(deck.state(LaneRole::AudioOutput), audio_before);
    }

    #[test]
    fn failed_lane_never_falls_back_to_another_role() {
        let mut pier = authorized_pier();
        let audio = pier.open_local(LaneRole::AudioOutput).unwrap();
        let video = pier.open_local(LaneRole::Video).unwrap();
        pier.report_failure(
            LaneRole::AudioOutput,
            audio.lane_generation(),
            LaneFailure::WriteFailed,
        );
        assert!(matches!(
            pier.check_send(LaneRole::AudioOutput),
            Err(LaneUnavailable::NotOpen(
                LaneRole::AudioOutput,
                LaneState::Recovering { .. }
            ))
        ));
        // Video stays usable, and is still only the video lane.
        assert_eq!(
            pier.check_send(LaneRole::Video),
            Ok(video.lane_generation())
        );
        // The reopened audio lane is strictly newer.
        let reopened = pier.open_local(LaneRole::AudioOutput).unwrap();
        assert!(reopened.lane_generation() > audio.lane_generation());
    }

    #[test]
    fn audio_failure_is_bounded_then_disabled_visibly() {
        let mut pier = authorized_pier();
        let mut generation = pier
            .open_local(LaneRole::AudioOutput)
            .unwrap()
            .lane_generation();
        for _ in 0..3 {
            let action =
                pier.report_failure(LaneRole::AudioOutput, generation, LaneFailure::PeerReset);
            assert!(matches!(
                action,
                LaneRecoveryAction::Reopen {
                    reset_stream: false,
                    ..
                }
            ));
            generation = pier
                .open_local(LaneRole::AudioOutput)
                .unwrap()
                .lane_generation();
        }
        let action = pier.report_failure(LaneRole::AudioOutput, generation, LaneFailure::PeerReset);
        assert_eq!(
            action,
            LaneRecoveryAction::Disable {
                role: LaneRole::AudioOutput,
                reset_stream: false
            }
        );
        assert_eq!(
            pier.state(LaneRole::AudioOutput),
            LaneState::Disabled(LaneDisableReason::RecoveryExhausted)
        );
        assert!(matches!(
            pier.open_local(LaneRole::AudioOutput),
            Err(LaneOpenError::Refused(LaneRefusal::LaneDisabled(
                LaneRole::AudioOutput
            )))
        ));
        assert!(
            pier.ended().is_none(),
            "audio loss does not end the attachment"
        );
    }

    #[test]
    fn healthy_delivery_resets_the_recovery_budget() {
        let mut pier = authorized_pier();
        for _ in 0..10 {
            let generation = pier
                .open_local(LaneRole::AudioOutput)
                .unwrap()
                .lane_generation();
            pier.mark_healthy(LaneRole::AudioOutput, generation);
            let action =
                pier.report_failure(LaneRole::AudioOutput, generation, LaneFailure::PeerReset);
            assert!(matches!(action, LaneRecoveryAction::Reopen { .. }));
        }
    }

    #[test]
    fn input_failure_releases_held_state_and_eventually_ends() {
        let mut pier = authorized_pier();
        let mut generation = 1;
        for _ in 0..3 {
            pier.admit_peer(
                LaneShape::Bidirectional,
                &bound(LaneRole::InputCursor, generation),
            )
            .unwrap();
            let action = pier.report_failure(
                LaneRole::InputCursor,
                nz(generation),
                LaneFailure::PeerReset,
            );
            assert!(matches!(
                action,
                LaneRecoveryAction::Reopen {
                    release_held_input: true,
                    reopened_by: LaneEndpoint::Deck,
                    ..
                }
            ));
            generation += 1;
        }
        pier.admit_peer(
            LaneShape::Bidirectional,
            &bound(LaneRole::InputCursor, generation),
        )
        .unwrap();
        let action = pier.report_failure(
            LaneRole::InputCursor,
            nz(generation),
            LaneFailure::MalformedFrame,
        );
        assert_eq!(
            action,
            LaneRecoveryAction::EndAttachment {
                role: LaneRole::InputCursor,
                reset_stream: true,
                reason: LaneEndReason::LaneExhausted(LaneRole::InputCursor),
            }
        );
        assert_eq!(
            pier.admit_peer(LaneShape::Bidirectional, &bound(LaneRole::BulkAux, 1)),
            Err(LaneRefusal::AttachmentEnded)
        );
    }

    #[test]
    fn control_failure_ends_the_attachment() {
        let mut pier = authorized_pier();
        let action = pier.report_failure(LaneRole::Control, nz(1), LaneFailure::PeerFinished);
        assert_eq!(
            action,
            LaneRecoveryAction::EndAttachment {
                role: LaneRole::Control,
                reset_stream: false,
                reason: LaneEndReason::ControlFailed,
            }
        );
        assert_eq!(
            pier.check_send(LaneRole::Video),
            Err(LaneUnavailable::AttachmentEnded)
        );
    }

    #[test]
    fn late_failure_reports_for_old_generations_are_superseded() {
        let mut pier = authorized_pier();
        let first = pier.open_local(LaneRole::Video).unwrap().lane_generation();
        pier.report_failure(LaneRole::Video, first, LaneFailure::PeerReset);
        let second = pier.open_local(LaneRole::Video).unwrap().lane_generation();
        assert_eq!(
            pier.report_failure(LaneRole::Video, first, LaneFailure::PeerReset),
            LaneRecoveryAction::Superseded
        );
        assert_eq!(pier.check_send(LaneRole::Video), Ok(second));
    }

    #[cfg(feature = "quic")]
    #[test]
    fn audio_lane_keeps_the_existing_priority_stream_priority() {
        assert_eq!(
            lane_send_priority(LaneRole::AudioOutput),
            crate::quic::AUDIO_PRIORITY_STREAM_PRIORITY
        );
    }

    #[test]
    fn telemetry_target_matches_the_transport_log_target() {
        assert_eq!(
            arcen_telemetry::names::target::TRANSPORT,
            crate::LOG_TARGET_TRANSPORT
        );
    }

    #[test]
    fn partial_writes_require_a_reset_not_a_finish() {
        for failure in [
            LaneFailure::PartialFrame,
            LaneFailure::WriteFailed,
            LaneFailure::Oversize,
            LaneFailure::MalformedFrame,
        ] {
            assert!(failure.requires_reset(), "{}", failure.token());
        }
        assert!(!LaneFailure::PeerReset.requires_reset());
        assert!(!LaneFailure::PeerFinished.requires_reset());
    }
}
