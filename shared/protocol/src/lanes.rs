//! QUIC lane roles and the bounded lane preface (lane protocol v1).
//!
//! One authenticated QUIC connection carries several independent streams
//! ("lanes"). QUIC assigns stream IDs, so the role of a stream is declared by
//! a fixed-size preface written before any payload. The preface binds every
//! non-control lane to the attachment the Pier authorized on the control lane;
//! TLS establishment alone never authorizes a lane.
//!
//! This module is pure wire vocabulary: role table, preface codec, attachment
//! grant DTO and peer-version classification. The registry that admits or
//! refuses streams lives in `arcen-transport::lanes`; the product adapter is
//! `arcen-transport::quic::session_lanes`. See
//! `docs/adr/0014-quic-lanes-and-capture-contracts.md`.
//!
//! Preface wire shape, lane protocol v1 (all integers big-endian):
//!
//! | Field | Bytes | Notes |
//! |-------|-------|-------|
//! | magic | 4 | [`LANE_PREFACE_MAGIC`] |
//! | version | 2 | must equal [`LANE_PROTOCOL_VERSION`] |
//! | role | 1 | [`LaneRole::wire`] |
//! | flags | 1 | reserved, must be zero |
//! | `attachment_generation` | 8 | zero for [`LaneRole::Control`], nonzero otherwise |
//! | `lane_generation` | 4 | nonzero |
//! | binding | 16 | all-zero for control, the Pier-issued binding otherwise |
//!
//! A reader reads [`LANE_PREFACE_HEADER_BYTES`] first, so an unknown version
//! or a legacy `arcen-direct-v1` stream is reported before it reads further.

use std::fmt::{Display, Formatter};
use std::num::{NonZeroU32, NonZeroU64};

use serde::{Deserialize, Serialize};

use crate::wire::FrameType;

/// Fixed magic identifying a lane preface.
pub const LANE_PREFACE_MAGIC: [u8; 4] = *b"ARLN";
/// Lane protocol version carried in every preface. Bumping it is a reviewed,
/// coordinated Deck-and-Pier change; there is no negotiation between versions.
pub const LANE_PROTOCOL_VERSION: u16 = 1;
/// Bytes read before the role is known: magic and version.
pub const LANE_PREFACE_HEADER_BYTES: usize = 4 + 2;
/// Length of the Pier-issued attachment binding.
pub const LANE_ATTACHMENT_BINDING_BYTES: usize = 16;
/// Exact size of a v1 lane preface. Fixed, so a reader never allocates from a
/// peer-declared length.
pub const LANE_PREFACE_BYTES: usize =
    LANE_PREFACE_HEADER_BYTES + 1 + 1 + 8 + 4 + LANE_ATTACHMENT_BINDING_BYTES;
/// Number of lane roles in lane protocol v1.
pub const LANE_ROLE_COUNT: usize = 6;
/// The 16-byte preface of the pre-lane single-stream direct carrier
/// (`arcen-transport` `quic::direct`). Recognised only to report a version
/// mismatch; it is never accepted as a lane.
pub const LEGACY_DIRECT_STREAM_PREFACE: &[u8; 16] = b"arcen-direct-v1\0";
/// QUIC application close code a lane-protocol endpoint uses when the peer
/// speaks another transport layout or lane version ("ARL" + 0x01).
pub const LANE_VERSION_MISMATCH_CLOSE_CODE: u32 = 0x4152_4C01;
/// Close reason the pre-lane Pier sends for an unrecognised stream preface.
/// A lane-protocol Deck uses it to recognise an older Pier.
pub const LEGACY_PIER_PREFACE_CLOSE_REASON: &[u8] = b"invalid direct stream preface";
/// QUIC application close code of an orderly session close ("ARL" + 0x03).
/// The closing endpoint finished its control lane and waited, within a
/// bound, for the peer to acknowledge it, so whatever it said last (an
/// `auth_result` refusal, a close reason) was delivered first. A peer that
/// receives it treats the end as deliberate, never as a transport loss.
pub const SESSION_CLOSED_CLOSE_CODE: u32 = 0x4152_4C03;
/// Close reason sent with [`SESSION_CLOSED_CLOSE_CODE`].
pub const SESSION_CLOSED_CLOSE_REASON: &[u8] = b"session closed";
/// WebSocket close status (1008, policy violation) of
/// [`legacy_direct_stream_refusal_frame`].
pub const LEGACY_REFUSAL_WEBSOCKET_CODE: u16 = 1008;
/// Longest WebSocket close reason (a control frame payload is at most 125
/// bytes, two of which are the status code).
const WEBSOCKET_CLOSE_REASON_MAX: usize = 123;
/// Message type of [`LaneAttachmentMsg`] on the control lane.
pub const LANE_ATTACHMENT_MSG_TYPE: &str = "lane_attachment";

/// QUIC application error code a lane endpoint uses when it resets or stops
/// one lane stream (`RESET_STREAM` / `STOP_SENDING`). Stream-scoped: the
/// connection and every other lane stay open. The values share the
/// [`LANE_VERSION_MISMATCH_CLOSE_CODE`] "ARL" prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneStreamCode {
    /// Refused before any payload was read: preface, role, direction,
    /// authorization, attachment or generation check failed.
    Refused,
    /// The preface did not arrive within the reader's bound.
    PrefaceTimeout,
    /// The lane failed (write error, partial frame, oversize or malformed
    /// frame); a replacement uses a newer lane generation.
    LaneFailed,
    /// A newer generation of the same role replaced this lane.
    Superseded,
    /// The attachment ended; every lane of it is reset.
    AttachmentEnded,
    /// The role was disabled for the rest of the attachment.
    Disabled,
}

impl LaneStreamCode {
    /// Every code, in wire order.
    pub const ALL: [Self; 6] = [
        Self::Refused,
        Self::PrefaceTimeout,
        Self::LaneFailed,
        Self::Superseded,
        Self::AttachmentEnded,
        Self::Disabled,
    ];

    /// Wire value.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Refused => 0x4152_4C10,
            Self::PrefaceTimeout => 0x4152_4C11,
            Self::LaneFailed => 0x4152_4C12,
            Self::Superseded => 0x4152_4C13,
            Self::AttachmentEnded => 0x4152_4C14,
            Self::Disabled => 0x4152_4C15,
        }
    }

    /// Parses a received stream error code. Unknown values are `None`.
    #[must_use]
    pub fn from_code(code: u64) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|candidate| u64::from(candidate.code()) == code)
    }

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Refused => "refused",
            Self::PrefaceTimeout => "preface_timeout",
            Self::LaneFailed => "lane_failed",
            Self::Superseded => "superseded",
            Self::AttachmentEnded => "attachment_ended",
            Self::Disabled => "disabled",
        }
    }
}

/// Which endpoint of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneEndpoint {
    /// The host.
    Pier,
    /// The client.
    Deck,
}

impl LaneEndpoint {
    /// The other endpoint.
    #[must_use]
    pub const fn peer(self) -> Self {
        match self {
            Self::Pier => Self::Deck,
            Self::Deck => Self::Pier,
        }
    }

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Pier => "pier",
            Self::Deck => "deck",
        }
    }
}

/// QUIC stream shape a lane uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneShape {
    /// A bidirectional stream.
    Bidirectional,
    /// A unidirectional stream; data flows from the opener only.
    Unidirectional,
}

impl LaneShape {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Bidirectional => "bidi",
            Self::Unidirectional => "uni",
        }
    }
}

/// Direction application payload flows on a lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneFlow {
    /// Pier to Deck only.
    PierToDeck,
    /// Deck to Pier only.
    DeckToPier,
    /// Both directions.
    Both,
}

/// How payload is framed after the preface.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneFraming {
    /// The existing bounded session-message framing that today runs over the
    /// single direct stream (JSON control messages and negotiated binary
    /// chunks). The lane split does not change those messages.
    SessionMessages,
    /// The existing audio-priority-stream framing reused for every media
    /// lane: each binary media frame (the same bytes a `Message::Binary`
    /// would carry) prefixed by its length as a big-endian `u32`.
    LengthPrefixedMedia,
}

/// Role of one QUIC stream within an authorized attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LaneRole {
    /// Authentication, capabilities, output transactions, configuration,
    /// health and lifecycle barriers. The only lane admitted before
    /// authorization; small bounded messages only.
    Control,
    /// Encoded video for every monitor, using the existing fair monitor mux.
    Video,
    /// Host audio-v1 frames.
    AudioOutput,
    /// Deck keyboard/pointer/pen/gestures and Pier cursor/input acks.
    InputCursor,
    /// Deck microphone-v1 frames.
    Microphone,
    /// Clipboard payloads and authorized USB/HID transfers.
    BulkAux,
}

impl LaneRole {
    /// Every role, in wire order.
    pub const ALL: [Self; LANE_ROLE_COUNT] = [
        Self::Control,
        Self::Video,
        Self::AudioOutput,
        Self::InputCursor,
        Self::Microphone,
        Self::BulkAux,
    ];

    /// Wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Control => 1,
            Self::Video => 2,
            Self::AudioOutput => 3,
            Self::InputCursor => 4,
            Self::Microphone => 5,
            Self::BulkAux => 6,
        }
    }

    /// Parses a wire value. Unknown values are `None`, never a guess.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Control),
            2 => Some(Self::Video),
            3 => Some(Self::AudioOutput),
            4 => Some(Self::InputCursor),
            5 => Some(Self::Microphone),
            6 => Some(Self::BulkAux),
            _ => None,
        }
    }

    /// Dense index in `0..LANE_ROLE_COUNT`.
    #[must_use]
    pub const fn index(self) -> usize {
        self.wire() as usize - 1
    }

    /// Stable token for logs, telemetry and the attachment grant.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Video => "video",
            Self::AudioOutput => "audio_output",
            Self::InputCursor => "input_cursor",
            Self::Microphone => "microphone",
            Self::BulkAux => "bulk_aux",
        }
    }

    /// Parses a stable token.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|role| role.token() == value)
    }

    /// QUIC stream shape.
    #[must_use]
    pub const fn shape(self) -> LaneShape {
        match self {
            Self::Control | Self::InputCursor | Self::BulkAux => LaneShape::Bidirectional,
            Self::Video | Self::AudioOutput | Self::Microphone => LaneShape::Unidirectional,
        }
    }

    /// Endpoint that opens the stream and writes the preface.
    #[must_use]
    pub const fn opener(self) -> LaneEndpoint {
        match self {
            Self::Control | Self::InputCursor | Self::BulkAux | Self::Microphone => {
                LaneEndpoint::Deck
            }
            Self::Video | Self::AudioOutput => LaneEndpoint::Pier,
        }
    }

    /// Direction application payload flows.
    #[must_use]
    pub const fn flow(self) -> LaneFlow {
        match self {
            Self::Control | Self::InputCursor | Self::BulkAux => LaneFlow::Both,
            Self::Video | Self::AudioOutput => LaneFlow::PierToDeck,
            Self::Microphone => LaneFlow::DeckToPier,
        }
    }

    /// Whether `sender` may write application payload on this lane.
    #[must_use]
    pub const fn may_send(self, sender: LaneEndpoint) -> bool {
        match self.flow() {
            LaneFlow::Both => true,
            LaneFlow::PierToDeck => matches!(sender, LaneEndpoint::Pier),
            LaneFlow::DeckToPier => matches!(sender, LaneEndpoint::Deck),
        }
    }

    /// Whether the preface must bind to an authorized attachment.
    #[must_use]
    pub const fn requires_attachment(self) -> bool {
        !matches!(self, Self::Control)
    }

    /// Payload framing after the preface.
    #[must_use]
    pub const fn framing(self) -> LaneFraming {
        match self {
            Self::Control | Self::InputCursor | Self::BulkAux => LaneFraming::SessionMessages,
            Self::Video | Self::AudioOutput | Self::Microphone => LaneFraming::LengthPrefixedMedia,
        }
    }

    /// The lane that owns a binary media frame type. Every frame type has
    /// exactly one lane; a sender never moves a frame to another role's lane
    /// when its own lane is unavailable.
    #[must_use]
    pub const fn for_frame_type(frame_type: FrameType) -> Self {
        match frame_type {
            FrameType::VideoFull
            | FrameType::VideoPartial
            | FrameType::VideoH264
            | FrameType::VideoH265
            | FrameType::VideoAv1
            | FrameType::RegionVideoFull
            | FrameType::RegionVideoH264
            | FrameType::RegionVideoH265
            | FrameType::RegionVideoAv1 => Self::Video,
            FrameType::Audio => Self::AudioOutput,
            FrameType::AudioUpstream => Self::Microphone,
            FrameType::Clipboard
            | FrameType::HidDeviceAdded
            | FrameType::HidDeviceRemoved
            | FrameType::HidReport
            | FrameType::UsbBridgeUrbSubmit
            | FrameType::UsbBridgeUrbCancel
            | FrameType::UsbBridgeUrbComplete => Self::BulkAux,
            FrameType::CursorUpdate => Self::InputCursor,
        }
    }
}

impl Display for LaneRole {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.token())
    }
}

/// A small set of lane roles.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct LaneRoleSet(u8);

impl LaneRoleSet {
    /// The empty set.
    pub const EMPTY: Self = Self(0);

    /// Builds a set from roles.
    #[must_use]
    pub const fn from_roles(roles: &[LaneRole]) -> Self {
        let mut bits = 0_u8;
        let mut index = 0;
        while index < roles.len() {
            bits |= 1 << roles[index].index();
            index += 1;
        }
        Self(bits)
    }

    /// Every role.
    #[must_use]
    pub const fn all() -> Self {
        Self::from_roles(&LaneRole::ALL)
    }

    /// Whether `role` is in the set.
    #[must_use]
    pub const fn contains(self, role: LaneRole) -> bool {
        self.0 & (1 << role.index()) != 0
    }

    /// The set with `role` added.
    #[must_use]
    pub const fn with(self, role: LaneRole) -> Self {
        Self(self.0 | (1 << role.index()))
    }

    /// Roles in wire order.
    pub fn iter(self) -> impl Iterator<Item = LaneRole> {
        LaneRole::ALL
            .into_iter()
            .filter(move |role| self.contains(*role))
    }
}

/// Opaque per-attachment binding issued by the Pier after authentication.
///
/// It ties a lane to one authorization result rather than to TLS
/// establishment. It is not a credential outside its connection, but it is
/// still never logged: `Debug` is redacted.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct LaneAttachmentBinding([u8; LANE_ATTACHMENT_BINDING_BYTES]);

impl LaneAttachmentBinding {
    /// Wraps binding bytes. All-zero bytes are reserved for the control lane.
    ///
    /// # Errors
    ///
    /// [`LaneContractError::ZeroBinding`] for all-zero bytes.
    pub fn new(bytes: [u8; LANE_ATTACHMENT_BINDING_BYTES]) -> Result<Self, LaneContractError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(LaneContractError::ZeroBinding);
        }
        Ok(Self(bytes))
    }

    /// Generates a fresh binding from the OS CSPRNG.
    ///
    /// # Errors
    ///
    /// [`LaneContractError::RandomUnavailable`] when the CSPRNG fails, or
    /// [`LaneContractError::ZeroBinding`] in the negligible all-zero case.
    pub fn generate() -> Result<Self, LaneContractError> {
        let mut bytes = [0_u8; LANE_ATTACHMENT_BINDING_BYTES];
        getrandom::getrandom(&mut bytes).map_err(|_| LaneContractError::RandomUnavailable)?;
        Self::new(bytes)
    }

    /// Raw bytes, for constant-time comparison by the registry.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; LANE_ATTACHMENT_BINDING_BYTES] {
        &self.0
    }

    /// Lowercase hex for the control-lane grant message.
    #[must_use]
    pub fn to_hex(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(LANE_ATTACHMENT_BINDING_BYTES * 2);
        for byte in self.0 {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    /// Parses exactly 32 lowercase hex digits.
    ///
    /// # Errors
    ///
    /// [`LaneContractError::MalformedBinding`] or
    /// [`LaneContractError::ZeroBinding`].
    pub fn from_hex(value: &str) -> Result<Self, LaneContractError> {
        let digits = value.as_bytes();
        if digits.len() != LANE_ATTACHMENT_BINDING_BYTES * 2 {
            return Err(LaneContractError::MalformedBinding);
        }
        let mut bytes = [0_u8; LANE_ATTACHMENT_BINDING_BYTES];
        for (index, pair) in digits.chunks_exact(2).enumerate() {
            let high = hex_value(pair[0]).ok_or(LaneContractError::MalformedBinding)?;
            let low = hex_value(pair[1]).ok_or(LaneContractError::MalformedBinding)?;
            bytes[index] = (high << 4) | low;
        }
        Self::new(bytes)
    }
}

const fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

impl std::fmt::Debug for LaneAttachmentBinding {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("LaneAttachmentBinding(<redacted>)")
    }
}

/// The attachment every non-control lane must name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LaneAttachment {
    /// Monotonic attachment generation, nonzero.
    pub generation: NonZeroU64,
    /// Pier-issued binding for this generation.
    pub binding: LaneAttachmentBinding,
}

/// Validated lane preface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LanePreface {
    role: LaneRole,
    attachment: Option<LaneAttachment>,
    lane_generation: NonZeroU32,
}

impl LanePreface {
    /// Preface for the control lane, which is opened before authorization.
    #[must_use]
    pub const fn control(lane_generation: NonZeroU32) -> Self {
        Self {
            role: LaneRole::Control,
            attachment: None,
            lane_generation,
        }
    }

    /// Preface for a lane bound to an authorized attachment.
    ///
    /// # Errors
    ///
    /// [`LaneContractError::ControlCarriesAttachment`] for the control role.
    pub const fn bound(
        role: LaneRole,
        attachment: LaneAttachment,
        lane_generation: NonZeroU32,
    ) -> Result<Self, LaneContractError> {
        if !role.requires_attachment() {
            return Err(LaneContractError::ControlCarriesAttachment);
        }
        Ok(Self {
            role,
            attachment: Some(attachment),
            lane_generation,
        })
    }

    /// Declared role.
    #[must_use]
    pub const fn role(&self) -> LaneRole {
        self.role
    }

    /// Declared attachment; `None` only for the control lane.
    #[must_use]
    pub const fn attachment(&self) -> Option<&LaneAttachment> {
        self.attachment.as_ref()
    }

    /// Declared lane generation.
    #[must_use]
    pub const fn lane_generation(&self) -> NonZeroU32 {
        self.lane_generation
    }

    /// Encodes the fixed-size v1 preface.
    #[must_use]
    pub fn encode(&self) -> [u8; LANE_PREFACE_BYTES] {
        let mut out = [0_u8; LANE_PREFACE_BYTES];
        out[0..4].copy_from_slice(&LANE_PREFACE_MAGIC);
        out[4..6].copy_from_slice(&LANE_PROTOCOL_VERSION.to_be_bytes());
        out[6] = self.role.wire();
        out[7] = 0;
        let (generation, binding) = match self.attachment {
            Some(attachment) => (attachment.generation.get(), *attachment.binding.as_bytes()),
            None => (0, [0; LANE_ATTACHMENT_BINDING_BYTES]),
        };
        out[8..16].copy_from_slice(&generation.to_be_bytes());
        out[16..20].copy_from_slice(&self.lane_generation.get().to_be_bytes());
        out[20..].copy_from_slice(&binding);
        out
    }

    /// Checks the magic and version before reading the rest of the preface.
    ///
    /// # Errors
    ///
    /// [`LanePrefaceError::LegacyDirectStream`] for a pre-lane peer,
    /// [`LanePrefaceError::BadMagic`] for anything else unknown, and
    /// [`LanePrefaceError::UnsupportedVersion`] for another lane version.
    pub fn check_header(header: &[u8; LANE_PREFACE_HEADER_BYTES]) -> Result<(), LanePrefaceError> {
        if header[..4] != LANE_PREFACE_MAGIC {
            if LEGACY_DIRECT_STREAM_PREFACE.starts_with(header) {
                return Err(LanePrefaceError::LegacyDirectStream);
            }
            return Err(LanePrefaceError::BadMagic);
        }
        let version = u16::from_be_bytes([header[4], header[5]]);
        if version != LANE_PROTOCOL_VERSION {
            return Err(LanePrefaceError::UnsupportedVersion {
                received: version,
                supported: LANE_PROTOCOL_VERSION,
            });
        }
        Ok(())
    }

    /// Decodes and validates a complete v1 preface.
    ///
    /// # Errors
    ///
    /// Any [`LanePrefaceError`]; nothing past the preface is interpreted.
    pub fn decode(bytes: &[u8]) -> Result<Self, LanePrefaceError> {
        let header: &[u8; LANE_PREFACE_HEADER_BYTES] = bytes
            .get(..LANE_PREFACE_HEADER_BYTES)
            .and_then(|slice| slice.try_into().ok())
            .ok_or(LanePrefaceError::Truncated)?;
        Self::check_header(header)?;
        if bytes.len() < LANE_PREFACE_BYTES {
            return Err(LanePrefaceError::Truncated);
        }
        if bytes.len() > LANE_PREFACE_BYTES {
            return Err(LanePrefaceError::TrailingBytes);
        }
        let role = LaneRole::from_wire(bytes[6]).ok_or(LanePrefaceError::UnknownRole(bytes[6]))?;
        if bytes[7] != 0 {
            return Err(LanePrefaceError::ReservedFlags(bytes[7]));
        }
        let generation = u64::from_be_bytes(read_array(&bytes[8..16]));
        let lane_generation = NonZeroU32::new(u32::from_be_bytes(read_array(&bytes[16..20])))
            .ok_or(LanePrefaceError::ZeroLaneGeneration)?;
        let binding: [u8; LANE_ATTACHMENT_BINDING_BYTES] = read_array(&bytes[20..]);
        if role.requires_attachment() {
            let generation =
                NonZeroU64::new(generation).ok_or(LanePrefaceError::MissingAttachment)?;
            let binding = LaneAttachmentBinding::new(binding)
                .map_err(|_| LanePrefaceError::MissingAttachment)?;
            Ok(Self {
                role,
                attachment: Some(LaneAttachment {
                    generation,
                    binding,
                }),
                lane_generation,
            })
        } else {
            if generation != 0 || binding.iter().any(|byte| *byte != 0) {
                return Err(LanePrefaceError::ControlCarriesAttachment);
            }
            Ok(Self::control(lane_generation))
        }
    }
}

fn read_array<const N: usize>(bytes: &[u8]) -> [u8; N] {
    let mut out = [0_u8; N];
    out.copy_from_slice(&bytes[..N]);
    out
}

/// Why a lane preface was refused while decoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanePrefaceError {
    /// Fewer bytes than a complete preface.
    Truncated,
    /// More bytes than a v1 preface where only the preface was expected.
    TrailingBytes,
    /// Not a lane preface.
    BadMagic,
    /// The peer speaks the pre-lane single-stream direct carrier.
    LegacyDirectStream,
    /// The peer speaks another lane protocol version.
    UnsupportedVersion {
        /// Version the peer sent.
        received: u16,
        /// The only version this build speaks.
        supported: u16,
    },
    /// Role byte outside the v1 table.
    UnknownRole(u8),
    /// Reserved flags were set.
    ReservedFlags(u8),
    /// Lane generation zero.
    ZeroLaneGeneration,
    /// A non-control lane without attachment generation or binding.
    MissingAttachment,
    /// A control preface carried attachment fields.
    ControlCarriesAttachment,
}

impl LanePrefaceError {
    /// Whether the refusal means the peer runs an incompatible build, which
    /// the endpoint reports with [`LANE_VERSION_MISMATCH_CLOSE_CODE`] and
    /// [`PeerTransportMismatch::message`].
    #[must_use]
    pub const fn version_mismatch(self) -> Option<PeerTransportMismatch> {
        match self {
            Self::LegacyDirectStream => Some(PeerTransportMismatch::PeerPredatesLanes),
            Self::UnsupportedVersion { received, .. } => {
                Some(PeerTransportMismatch::LaneVersion { received })
            }
            _ => None,
        }
    }

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Truncated => "truncated",
            Self::TrailingBytes => "trailing_bytes",
            Self::BadMagic => "bad_magic",
            Self::LegacyDirectStream => "legacy_direct_stream",
            Self::UnsupportedVersion { .. } => "unsupported_version",
            Self::UnknownRole(_) => "unknown_role",
            Self::ReservedFlags(_) => "reserved_flags",
            Self::ZeroLaneGeneration => "zero_lane_generation",
            Self::MissingAttachment => "missing_attachment",
            Self::ControlCarriesAttachment => "control_carries_attachment",
        }
    }
}

impl Display for LanePrefaceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedVersion {
                received,
                supported,
            } => write!(
                formatter,
                "lane preface version {received} is not supported (this build speaks {supported})"
            ),
            Self::UnknownRole(role) => write!(formatter, "unknown lane role {role}"),
            Self::ReservedFlags(flags) => write!(formatter, "reserved lane flags {flags:#04x}"),
            other => write!(formatter, "lane preface refused: {}", other.token()),
        }
    }
}

impl std::error::Error for LanePrefaceError {}

/// A peer that cannot speak this build's transport layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerTransportMismatch {
    /// The peer uses the pre-lane single direct stream (`arcen-direct-v1`).
    PeerPredatesLanes,
    /// The peer uses another lane protocol version.
    LaneVersion {
        /// Version the peer sent.
        received: u16,
    },
}

impl PeerTransportMismatch {
    /// Operator-facing explanation. Deck and Pier are updated together; no
    /// legacy single-stream multiplexing is retained.
    #[must_use]
    pub fn message(self) -> String {
        match self {
            Self::PeerPredatesLanes => format!(
                "the other side uses the older single-stream transport; this build requires \
                 lane protocol v{LANE_PROTOCOL_VERSION}. Update the Deck and the Pier to the \
                 same release"
            ),
            Self::LaneVersion { received } => format!(
                "the other side uses lane protocol v{received}; this build speaks \
                 v{LANE_PROTOCOL_VERSION}. Update the Deck and the Pier to the same release"
            ),
        }
    }

    /// Classifies a QUIC application close received while opening the
    /// control lane. `None` means the close was not a version mismatch.
    #[must_use]
    pub fn from_peer_close(code: u64, reason: &[u8]) -> Option<Self> {
        if code == u64::from(LANE_VERSION_MISMATCH_CLOSE_CODE) {
            let received = std::str::from_utf8(reason)
                .ok()
                .and_then(|reason| reason.strip_prefix("arcen-lanes v"))
                .and_then(|rest| rest.split_once(':'))
                .and_then(|(version, _)| version.parse().ok())
                .unwrap_or(0);
            return Some(Self::LaneVersion { received });
        }
        if code == 0 && reason == LEGACY_PIER_PREFACE_CLOSE_REASON {
            return Some(Self::PeerPredatesLanes);
        }
        None
    }
}

/// Human-readable, bounded close reason sent with
/// [`LANE_VERSION_MISMATCH_CLOSE_CODE`]. It names the sender's lane version
/// and is readable by a pre-lane peer that only prints the reason.
#[must_use]
pub fn lane_mismatch_close_reason() -> String {
    format!(
        "arcen-lanes v{LANE_PROTOCOL_VERSION}: incompatible transport; update the Deck and the \
         Pier to the same release"
    )
}

impl Display for PeerTransportMismatch {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message())
    }
}

/// The reason a lane Pier gives a pre-lane Deck in
/// [`legacy_direct_stream_refusal_frame`].
#[must_use]
pub fn legacy_deck_refusal_reason() -> String {
    format!(
        "this Pier speaks lane protocol v{LANE_PROTOCOL_VERSION}; update the Deck to the same \
         Arcen release as the Pier"
    )
}

/// The bytes a lane Pier writes back on a pre-lane Deck's
/// `arcen-direct-v1` stream before closing it.
///
/// That stream carried raw WebSocket frames, and the pre-lane Deck reads
/// first, so one unmasked server close frame (RFC 6455 §5.5.1, status
/// [`LEGACY_REFUSAL_WEBSOCKET_CODE`]) is something it already understands:
/// it prints "host closed the connection during setup: " followed by
/// [`legacy_deck_refusal_reason`] instead of "connection lost".
#[must_use]
pub fn legacy_direct_stream_refusal_frame() -> Vec<u8> {
    let reason = legacy_deck_refusal_reason();
    let mut cut = reason.len().min(WEBSOCKET_CLOSE_REASON_MAX);
    while !reason.is_char_boundary(cut) {
        cut -= 1;
    }
    let reason = &reason.as_bytes()[..cut];
    let mut frame = Vec::with_capacity(4 + reason.len());
    frame.push(0x88);
    // At most 125, so the 7-bit length form applies.
    frame.push(u8::try_from(2 + reason.len()).unwrap_or(125));
    frame.extend_from_slice(&LEGACY_REFUSAL_WEBSOCKET_CODE.to_be_bytes());
    frame.extend_from_slice(reason);
    frame
}

/// Lane contract construction failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaneContractError {
    /// All-zero binding bytes.
    ZeroBinding,
    /// Binding text is not 32 lowercase hex digits.
    MalformedBinding,
    /// The OS CSPRNG failed.
    RandomUnavailable,
    /// A control lane cannot carry an attachment.
    ControlCarriesAttachment,
    /// Attachment generation zero.
    ZeroAttachmentGeneration,
    /// The grant names another lane protocol version.
    UnsupportedVersion(u16),
    /// The grant names an unknown role token.
    UnknownRole,
    /// The grant lists a role twice, lists control, or exceeds the table.
    InvalidRoleList,
    /// The message type is not [`LANE_ATTACHMENT_MSG_TYPE`].
    WrongMessageType,
}

impl Display for LaneContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::ZeroBinding => "lane attachment binding is all zero",
            Self::MalformedBinding => "lane attachment binding is not 32 lowercase hex digits",
            Self::RandomUnavailable => "OS random source unavailable",
            Self::ControlCarriesAttachment => "the control lane cannot carry an attachment",
            Self::ZeroAttachmentGeneration => "lane attachment generation is zero",
            Self::UnsupportedVersion(_) => "lane attachment names an unsupported lane version",
            Self::UnknownRole => "lane attachment names an unknown role",
            Self::InvalidRoleList => "lane attachment role list is invalid",
            Self::WrongMessageType => "not a lane_attachment message",
        };
        formatter.write_str(text)
    }
}

impl std::error::Error for LaneContractError {}

/// Control-lane message the Pier sends after successful authentication,
/// before it accepts any non-control lane (and before `server_hello`).
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneAttachmentMsg {
    /// Always [`LANE_ATTACHMENT_MSG_TYPE`].
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Always [`LANE_PROTOCOL_VERSION`].
    pub lane_protocol_version: u16,
    /// Nonzero attachment generation.
    pub attachment_generation: u64,
    /// [`LaneAttachmentBinding::to_hex`].
    pub binding: String,
    /// Non-control roles the Pier will admit for this attachment.
    pub roles: Vec<String>,
}

impl std::fmt::Debug for LaneAttachmentMsg {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LaneAttachmentMsg")
            .field("lane_protocol_version", &self.lane_protocol_version)
            .field("attachment_generation", &self.attachment_generation)
            .field("binding", &"<redacted>")
            .field("roles", &self.roles)
            .finish()
    }
}

impl LaneAttachmentMsg {
    /// Builds the grant for an attachment and its enabled roles. Control is
    /// implicit and never listed.
    #[must_use]
    pub fn new(attachment: LaneAttachment, roles: LaneRoleSet) -> Self {
        Self {
            msg_type: LANE_ATTACHMENT_MSG_TYPE.to_string(),
            lane_protocol_version: LANE_PROTOCOL_VERSION,
            attachment_generation: attachment.generation.get(),
            binding: attachment.binding.to_hex(),
            roles: roles
                .iter()
                .filter(|role| role.requires_attachment())
                .map(|role| role.token().to_string())
                .collect(),
        }
    }

    /// Strictly validates a received grant.
    ///
    /// # Errors
    ///
    /// Any [`LaneContractError`] describing the first invalid field.
    pub fn validate(&self) -> Result<(LaneAttachment, LaneRoleSet), LaneContractError> {
        if self.msg_type != LANE_ATTACHMENT_MSG_TYPE {
            return Err(LaneContractError::WrongMessageType);
        }
        if self.lane_protocol_version != LANE_PROTOCOL_VERSION {
            return Err(LaneContractError::UnsupportedVersion(
                self.lane_protocol_version,
            ));
        }
        let generation = NonZeroU64::new(self.attachment_generation)
            .ok_or(LaneContractError::ZeroAttachmentGeneration)?;
        let binding = LaneAttachmentBinding::from_hex(&self.binding)?;
        if self.roles.len() >= LANE_ROLE_COUNT {
            return Err(LaneContractError::InvalidRoleList);
        }
        let mut roles = LaneRoleSet::EMPTY;
        for token in &self.roles {
            let role = LaneRole::from_token(token).ok_or(LaneContractError::UnknownRole)?;
            if !role.requires_attachment() || roles.contains(role) {
                return Err(LaneContractError::InvalidRoleList);
            }
            roles = roles.with(role);
        }
        Ok((
            LaneAttachment {
                generation,
                binding,
            },
            roles,
        ))
    }
}

// ---------------------------------------------------------------------------
// Session records: the framing of the session-message lanes.
// ---------------------------------------------------------------------------

/// Bytes of a session record's big-endian `u32` length prefix. The length
/// covers the kind byte and the payload, so a session record is read with the
/// same length-prefixed reader as a media frame.
pub const SESSION_RECORD_LENGTH_BYTES: usize = 4;

/// Bytes of a session record's kind byte, which follows the length prefix.
pub const SESSION_RECORD_KIND_BYTES: usize = 1;

/// Largest close reason a session record carries, in bytes (the WebSocket
/// close-frame bound the session protocol already used).
pub const MAX_SESSION_CLOSE_REASON_BYTES: usize = 123;

/// Kind byte of a session record ([`LaneFraming::SessionMessages`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SessionRecordKind {
    /// UTF-8 JSON session message.
    Text,
    /// Binary session chunk (clipboard, HID, USB).
    Binary,
    /// Orderly close with an optional code and reason.
    Close,
    /// Liveness probe.
    Ping,
    /// Liveness reply.
    Pong,
}

impl SessionRecordKind {
    /// Wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Text => 1,
            Self::Binary => 2,
            Self::Close => 3,
            Self::Ping => 4,
            Self::Pong => 5,
        }
    }

    /// Parses a wire value. Unknown values are `None`.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Text),
            2 => Some(Self::Binary),
            3 => Some(Self::Close),
            4 => Some(Self::Ping),
            5 => Some(Self::Pong),
            _ => None,
        }
    }
}

/// Code and reason of an orderly session close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionClose {
    /// Close code (the session protocol's existing WebSocket-style codes).
    pub code: u16,
    /// Human-readable reason, at most [`MAX_SESSION_CLOSE_REASON_BYTES`].
    pub reason: String,
}

/// One session message as carried on a control, input/cursor or bulk lane.
///
/// Wire shape: `u32` BE length of everything after it, then the kind byte,
/// then the payload. A close payload is a `u16` BE code followed by the UTF-8
/// reason, or empty for a close without a code. The messages themselves are
/// the existing session messages; only their envelope differs from the
/// pre-lane single stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionRecord {
    /// UTF-8 JSON session message.
    Text(String),
    /// Binary session chunk.
    Binary(Vec<u8>),
    /// Orderly close.
    Close(Option<SessionClose>),
    /// Liveness probe.
    Ping(Vec<u8>),
    /// Liveness reply.
    Pong(Vec<u8>),
}

/// Why a received session record was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRecordError {
    /// The record has no kind byte.
    Empty,
    /// Unknown kind byte.
    UnknownKind(u8),
    /// A text record or close reason is not UTF-8.
    InvalidUtf8,
    /// A close payload is one byte, or its reason is too long.
    MalformedClose,
}

impl Display for SessionRecordError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => formatter.write_str("session record has no kind byte"),
            Self::UnknownKind(kind) => write!(formatter, "unknown session record kind {kind}"),
            Self::InvalidUtf8 => formatter.write_str("session record text is not UTF-8"),
            Self::MalformedClose => formatter.write_str("session close record is malformed"),
        }
    }
}

impl std::error::Error for SessionRecordError {}

impl SessionRecord {
    /// Kind of this record.
    #[must_use]
    pub const fn kind(&self) -> SessionRecordKind {
        match self {
            Self::Text(_) => SessionRecordKind::Text,
            Self::Binary(_) => SessionRecordKind::Binary,
            Self::Close(_) => SessionRecordKind::Close,
            Self::Ping(_) => SessionRecordKind::Ping,
            Self::Pong(_) => SessionRecordKind::Pong,
        }
    }

    /// The lane this record belongs on, by the session message it carries.
    /// Close, ping and pong records belong to the lane they are written on;
    /// this returns [`LaneRole::Control`] for them.
    #[must_use]
    pub fn lane(&self) -> LaneRole {
        match self {
            Self::Text(text) => lane_for_session_text(text),
            Self::Binary(bytes) => lane_for_binary_message(bytes).unwrap_or(LaneRole::Control),
            Self::Close(_) | Self::Ping(_) | Self::Pong(_) => LaneRole::Control,
        }
    }

    /// Encodes the complete record, length prefix included, ready to be
    /// queued as one lane record. A close reason longer than
    /// [`MAX_SESSION_CLOSE_REASON_BYTES`] is cut at a character boundary.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let close_payload;
        let payload: &[u8] = match self {
            Self::Text(text) => text.as_bytes(),
            Self::Binary(bytes) | Self::Ping(bytes) | Self::Pong(bytes) => bytes,
            Self::Close(None) => &[],
            Self::Close(Some(close)) => {
                let mut reason = close.reason.as_str();
                while reason.len() > MAX_SESSION_CLOSE_REASON_BYTES {
                    let mut end = MAX_SESSION_CLOSE_REASON_BYTES;
                    while !reason.is_char_boundary(end) {
                        end -= 1;
                    }
                    reason = &reason[..end];
                }
                let mut bytes = Vec::with_capacity(2 + reason.len());
                bytes.extend_from_slice(&close.code.to_be_bytes());
                bytes.extend_from_slice(reason.as_bytes());
                close_payload = bytes;
                &close_payload
            }
        };
        let body_len = u32::try_from(payload.len().saturating_add(1)).unwrap_or(u32::MAX);
        let mut out = Vec::with_capacity(SESSION_RECORD_LENGTH_BYTES + 1 + payload.len());
        out.extend_from_slice(&body_len.to_be_bytes());
        out.push(self.kind().wire());
        out.extend_from_slice(payload);
        out
    }

    /// Decodes one record body: everything after the length prefix.
    ///
    /// # Errors
    ///
    /// [`SessionRecordError`] for an empty body, an unknown kind, invalid
    /// UTF-8 or a malformed close.
    pub fn decode_body(body: &[u8]) -> Result<Self, SessionRecordError> {
        let (&kind, payload) = body.split_first().ok_or(SessionRecordError::Empty)?;
        let kind =
            SessionRecordKind::from_wire(kind).ok_or(SessionRecordError::UnknownKind(kind))?;
        Ok(match kind {
            SessionRecordKind::Text => Self::Text(
                std::str::from_utf8(payload)
                    .map_err(|_| SessionRecordError::InvalidUtf8)?
                    .to_owned(),
            ),
            SessionRecordKind::Binary => Self::Binary(payload.to_vec()),
            SessionRecordKind::Ping => Self::Ping(payload.to_vec()),
            SessionRecordKind::Pong => Self::Pong(payload.to_vec()),
            SessionRecordKind::Close => match payload {
                [] => Self::Close(None),
                [_] => return Err(SessionRecordError::MalformedClose),
                [high, low, reason @ ..] => {
                    if reason.len() > MAX_SESSION_CLOSE_REASON_BYTES {
                        return Err(SessionRecordError::MalformedClose);
                    }
                    Self::Close(Some(SessionClose {
                        code: u16::from_be_bytes([*high, *low]),
                        reason: std::str::from_utf8(reason)
                            .map_err(|_| SessionRecordError::InvalidUtf8)?
                            .to_owned(),
                    }))
                }
            },
        })
    }
}

/// A session message serialized as a text record. `None` only when the
/// message cannot be serialized, which the protocol's own types never do.
#[must_use]
pub fn session_text_record<T: Serialize>(message: &T) -> Option<SessionRecord> {
    serde_json::to_string(message).ok().map(SessionRecord::Text)
}

/// The clipboard transfer a bulk record belongs to: the sequence of a
/// `clipboard_data` offer or of a binary clipboard chunk. `None` for any
/// other record (HID, USB, malformed).
#[must_use]
pub fn clipboard_transfer_sequence(record: &SessionRecord) -> Option<u64> {
    match record {
        SessionRecord::Text(text) => {
            serde_json::from_str::<crate::messages::ClipboardDataMsg>(text)
                .ok()
                .filter(|offer| offer.msg_type == crate::messages::CLIPBOARD_DATA)
                .map(|offer| offer.sequence)
        }
        SessionRecord::Binary(bytes) => crate::wire::decode_clipboard_chunk(bytes)
            .ok()
            .map(|(header, _)| header.sequence),
        _ => None,
    }
}

/// Session message types carried on the input/cursor lane: every Deck input
/// event (keyboard, pointer, pen, gestures, text commit, held-state reset)
/// and the Pier's cursor and keyboard-text notices (text commit results, the
/// session's keyboard layout), so they stay ordered with the input they
/// answer. Everything else that is JSON goes on the
/// control lane, except [`BULK_AUX_MESSAGE_TYPES`].
pub const INPUT_CURSOR_MESSAGE_TYPES: &[&str] = &[
    "key_event",
    crate::messages::KEY_RESET_MODIFIERS,
    crate::messages::TEXT_COMMIT,
    "mouse_move",
    crate::messages::MOUSE_MOVE_RELATIVE,
    "mouse_button",
    crate::messages::MOUSE_SCROLL,
    crate::messages::PEN_EVENT,
    crate::messages::GESTURE_MAGNIFY,
    crate::messages::GESTURE_ROTATE,
    crate::messages::GESTURE_SMART_ZOOM,
    crate::messages::GESTURE_SWIPE,
    crate::region_input::REGION_POINTER_MOTION,
    crate::region_input::REGION_POINTER_BUTTON,
    crate::region_input::REGION_POINTER_SCROLL,
    crate::region_input::REGION_POINTER_ENTER,
    crate::region_input::REGION_POINTER_LEAVE,
    crate::region_input::REGION_PEN_EVENT,
    crate::messages::CURSOR_SHAPE,
    crate::messages::TEXT_COMMIT_RESULT,
    crate::messages::HOST_KEYMAP,
];

/// Session message types carried on the bulk auxiliary lane, besides the
/// binary clipboard, HID and USB frames.
pub const BULK_AUX_MESSAGE_TYPES: &[&str] = &[crate::messages::CLIPBOARD_DATA];

/// The lane that carries a session message of `msg_type`. Unknown types are
/// control messages: the control lane is the only one whose contract admits
/// every session message.
#[must_use]
pub fn lane_for_message_type(msg_type: &str) -> LaneRole {
    if INPUT_CURSOR_MESSAGE_TYPES.contains(&msg_type) {
        LaneRole::InputCursor
    } else if BULK_AUX_MESSAGE_TYPES.contains(&msg_type) {
        LaneRole::BulkAux
    } else {
        LaneRole::Control
    }
}

/// The lane that carries a JSON session message, read from its `"type"`
/// field. Text that is not a JSON object with a string `"type"` is a control
/// message.
#[must_use]
pub fn lane_for_session_text(text: &str) -> LaneRole {
    #[derive(Deserialize)]
    struct Typed<'a> {
        #[serde(rename = "type", borrow)]
        msg_type: Option<std::borrow::Cow<'a, str>>,
    }
    serde_json::from_str::<Typed<'_>>(text)
        .ok()
        .and_then(|typed| typed.msg_type)
        .map_or(LaneRole::Control, |msg_type| {
            lane_for_message_type(&msg_type)
        })
}

/// The lane that carries a binary message, by its leading [`FrameType`]
/// byte. `None` when the byte names no frame type.
#[must_use]
pub fn lane_for_binary_message(bytes: &[u8]) -> Option<LaneRole> {
    bytes
        .first()
        .and_then(|first| FrameType::try_from(*first).ok())
        .map(LaneRole::for_frame_type)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nz32(value: u32) -> NonZeroU32 {
        NonZeroU32::new(value).unwrap()
    }

    fn attachment() -> LaneAttachment {
        LaneAttachment {
            generation: NonZeroU64::new(7).unwrap(),
            binding: LaneAttachmentBinding::new([0xA5; LANE_ATTACHMENT_BINDING_BYTES]).unwrap(),
        }
    }

    #[test]
    fn role_table_is_exact() {
        use LaneEndpoint::{Deck, Pier};
        use LaneShape::{Bidirectional, Unidirectional};
        let table = [
            (LaneRole::Control, Bidirectional, Deck, LaneFlow::Both),
            (LaneRole::Video, Unidirectional, Pier, LaneFlow::PierToDeck),
            (
                LaneRole::AudioOutput,
                Unidirectional,
                Pier,
                LaneFlow::PierToDeck,
            ),
            (LaneRole::InputCursor, Bidirectional, Deck, LaneFlow::Both),
            (
                LaneRole::Microphone,
                Unidirectional,
                Deck,
                LaneFlow::DeckToPier,
            ),
            (LaneRole::BulkAux, Bidirectional, Deck, LaneFlow::Both),
        ];
        for (role, shape, opener, flow) in table {
            assert_eq!(role.shape(), shape, "{role}");
            assert_eq!(role.opener(), opener, "{role}");
            assert_eq!(role.flow(), flow, "{role}");
            assert_eq!(LaneRole::from_wire(role.wire()), Some(role));
            assert_eq!(LaneRole::from_token(role.token()), Some(role));
            assert_eq!(LaneRole::ALL[role.index()], role);
        }
        assert!(LaneRole::from_wire(0).is_none());
        assert!(LaneRole::from_wire(7).is_none());
        // A unidirectional lane only carries payload from its opener.
        for role in LaneRole::ALL {
            if role.shape() == Unidirectional {
                assert!(role.may_send(role.opener()));
                assert!(!role.may_send(role.opener().peer()));
            }
        }
    }

    #[test]
    fn media_lanes_reuse_priority_audio_framing() {
        assert_eq!(
            LaneRole::AudioOutput.framing(),
            LaneFraming::LengthPrefixedMedia
        );
        assert_eq!(LaneRole::Video.framing(), LaneFraming::LengthPrefixedMedia);
        assert_eq!(
            LaneRole::Microphone.framing(),
            LaneFraming::LengthPrefixedMedia
        );
        assert_eq!(LaneRole::Control.framing(), LaneFraming::SessionMessages);
    }

    #[test]
    fn every_frame_type_has_exactly_one_lane() {
        assert_eq!(
            LaneRole::for_frame_type(FrameType::Audio),
            LaneRole::AudioOutput
        );
        assert_eq!(
            LaneRole::for_frame_type(FrameType::AudioUpstream),
            LaneRole::Microphone
        );
        assert_eq!(
            LaneRole::for_frame_type(FrameType::VideoH265),
            LaneRole::Video
        );
        assert_eq!(
            LaneRole::for_frame_type(FrameType::RegionVideoAv1),
            LaneRole::Video
        );
        assert_eq!(
            LaneRole::for_frame_type(FrameType::Clipboard),
            LaneRole::BulkAux
        );
        assert_eq!(
            LaneRole::for_frame_type(FrameType::UsbBridgeUrbComplete),
            LaneRole::BulkAux
        );
        assert_eq!(
            LaneRole::for_frame_type(FrameType::CursorUpdate),
            LaneRole::InputCursor
        );
        assert_eq!(
            lane_for_binary_message(&[0x50]),
            Some(LaneRole::InputCursor)
        );
    }

    #[test]
    fn preface_round_trips_for_every_role() {
        let control = LanePreface::control(nz32(1));
        let bytes = control.encode();
        assert_eq!(bytes.len(), LANE_PREFACE_BYTES);
        assert_eq!(LANE_PREFACE_BYTES, 36);
        assert_eq!(LanePreface::decode(&bytes), Ok(control));
        for role in LaneRole::ALL
            .into_iter()
            .filter(|role| role.requires_attachment())
        {
            let preface = LanePreface::bound(role, attachment(), nz32(3)).unwrap();
            assert_eq!(LanePreface::decode(&preface.encode()), Ok(preface));
        }
    }

    #[test]
    fn preface_refuses_malformed_input() {
        let good = LanePreface::bound(LaneRole::Video, attachment(), nz32(2))
            .unwrap()
            .encode();
        assert_eq!(
            LanePreface::decode(&good[..LANE_PREFACE_BYTES - 1]),
            Err(LanePrefaceError::Truncated)
        );
        assert_eq!(
            LanePreface::decode(&good[..3]),
            Err(LanePrefaceError::Truncated)
        );
        let mut long = good.to_vec();
        long.push(0);
        assert_eq!(
            LanePreface::decode(&long),
            Err(LanePrefaceError::TrailingBytes)
        );

        let mut bytes = good;
        bytes[6] = 9;
        assert_eq!(
            LanePreface::decode(&bytes),
            Err(LanePrefaceError::UnknownRole(9))
        );

        let mut bytes = good;
        bytes[7] = 1;
        assert_eq!(
            LanePreface::decode(&bytes),
            Err(LanePrefaceError::ReservedFlags(1))
        );

        let mut bytes = good;
        bytes[16..20].copy_from_slice(&0_u32.to_be_bytes());
        assert_eq!(
            LanePreface::decode(&bytes),
            Err(LanePrefaceError::ZeroLaneGeneration)
        );

        let mut bytes = good;
        bytes[8..16].copy_from_slice(&0_u64.to_be_bytes());
        assert_eq!(
            LanePreface::decode(&bytes),
            Err(LanePrefaceError::MissingAttachment)
        );

        let mut bytes = good;
        bytes[20..].copy_from_slice(&[0; LANE_ATTACHMENT_BINDING_BYTES]);
        assert_eq!(
            LanePreface::decode(&bytes),
            Err(LanePrefaceError::MissingAttachment)
        );

        let mut control = LanePreface::control(nz32(1)).encode();
        control[15] = 1;
        assert_eq!(
            LanePreface::decode(&control),
            Err(LanePrefaceError::ControlCarriesAttachment)
        );
        assert_eq!(
            LanePreface::bound(LaneRole::Control, attachment(), nz32(1)),
            Err(LaneContractError::ControlCarriesAttachment)
        );
    }

    #[test]
    fn incompatible_peers_are_reported_clearly() {
        let mut header = [0_u8; LANE_PREFACE_HEADER_BYTES];
        header.copy_from_slice(&LEGACY_DIRECT_STREAM_PREFACE[..LANE_PREFACE_HEADER_BYTES]);
        let error = LanePreface::check_header(&header).unwrap_err();
        assert_eq!(error, LanePrefaceError::LegacyDirectStream);
        assert_eq!(
            error.version_mismatch(),
            Some(PeerTransportMismatch::PeerPredatesLanes)
        );
        assert!(error
            .version_mismatch()
            .unwrap()
            .message()
            .contains("Update the Deck and the Pier"));

        let mut future = LanePreface::control(nz32(1)).encode();
        future[4..6].copy_from_slice(&2_u16.to_be_bytes());
        let error = LanePreface::decode(&future).unwrap_err();
        assert_eq!(
            error,
            LanePrefaceError::UnsupportedVersion {
                received: 2,
                supported: 1
            }
        );
        assert_eq!(
            error.version_mismatch(),
            Some(PeerTransportMismatch::LaneVersion { received: 2 })
        );

        assert_eq!(
            LanePreface::check_header(b"GET / "),
            Err(LanePrefaceError::BadMagic)
        );
        assert_eq!(LanePrefaceError::BadMagic.version_mismatch(), None);
    }

    #[test]
    fn deck_classifies_pier_close_reasons() {
        assert_eq!(
            PeerTransportMismatch::from_peer_close(0, LEGACY_PIER_PREFACE_CLOSE_REASON),
            Some(PeerTransportMismatch::PeerPredatesLanes)
        );
        let reason = lane_mismatch_close_reason();
        assert!(reason.len() < 128);
        assert_eq!(
            PeerTransportMismatch::from_peer_close(
                u64::from(LANE_VERSION_MISMATCH_CLOSE_CODE),
                reason.as_bytes()
            ),
            Some(PeerTransportMismatch::LaneVersion {
                received: LANE_PROTOCOL_VERSION
            })
        );
        assert_eq!(
            PeerTransportMismatch::from_peer_close(
                u64::from(LANE_VERSION_MISMATCH_CLOSE_CODE),
                b"arcen-lanes v3: newer"
            ),
            Some(PeerTransportMismatch::LaneVersion { received: 3 })
        );
        assert_eq!(
            PeerTransportMismatch::from_peer_close(
                u64::from(LANE_VERSION_MISMATCH_CLOSE_CODE),
                b"garbage"
            ),
            Some(PeerTransportMismatch::LaneVersion { received: 0 })
        );
        assert_eq!(PeerTransportMismatch::from_peer_close(0, b"bye"), None);
        assert_eq!(PeerTransportMismatch::from_peer_close(7, b""), None);
    }

    #[test]
    fn binding_is_validated_and_redacted() {
        assert_eq!(
            LaneAttachmentBinding::new([0; LANE_ATTACHMENT_BINDING_BYTES]),
            Err(LaneContractError::ZeroBinding)
        );
        let binding = LaneAttachmentBinding::generate().unwrap();
        assert_eq!(
            LaneAttachmentBinding::from_hex(&binding.to_hex()),
            Ok(binding)
        );
        assert!(!format!("{binding:?}").contains(&binding.to_hex()));
        assert_eq!(
            LaneAttachmentBinding::from_hex("AB"),
            Err(LaneContractError::MalformedBinding)
        );
        assert_eq!(
            LaneAttachmentBinding::from_hex(&"G".repeat(32)),
            Err(LaneContractError::MalformedBinding)
        );
        assert_eq!(
            LaneAttachmentBinding::from_hex(&"A5".repeat(16)),
            Err(LaneContractError::MalformedBinding),
            "uppercase hex is not canonical"
        );
    }

    #[test]
    fn attachment_grant_round_trips_and_is_strict() {
        let roles = LaneRoleSet::from_roles(&[
            LaneRole::Control,
            LaneRole::Video,
            LaneRole::AudioOutput,
            LaneRole::InputCursor,
        ]);
        let message = LaneAttachmentMsg::new(attachment(), roles);
        assert!(!message.roles.contains(&"control".to_string()));
        let json = serde_json::to_string(&message).unwrap();
        let parsed: LaneAttachmentMsg = serde_json::from_str(&json).unwrap();
        let (granted, granted_roles) = parsed.validate().unwrap();
        assert_eq!(granted, attachment());
        assert!(granted_roles.contains(LaneRole::Video));
        assert!(!granted_roles.contains(LaneRole::Microphone));
        assert!(!format!("{parsed:?}").contains(&attachment().binding.to_hex()));

        let mut bad = message.clone();
        bad.roles.push("video".into());
        assert_eq!(bad.validate(), Err(LaneContractError::InvalidRoleList));
        let mut bad = message.clone();
        bad.roles = vec!["control".into()];
        assert_eq!(bad.validate(), Err(LaneContractError::InvalidRoleList));
        let mut bad = message.clone();
        bad.roles = vec!["teleport".into()];
        assert_eq!(bad.validate(), Err(LaneContractError::UnknownRole));
        let mut bad = message.clone();
        bad.lane_protocol_version = 2;
        assert_eq!(
            bad.validate(),
            Err(LaneContractError::UnsupportedVersion(2))
        );
        let mut bad = message.clone();
        bad.attachment_generation = 0;
        assert_eq!(
            bad.validate(),
            Err(LaneContractError::ZeroAttachmentGeneration)
        );
        let mut bad = message;
        bad.msg_type = "auth_result".into();
        assert_eq!(bad.validate(), Err(LaneContractError::WrongMessageType));
        assert!(serde_json::from_str::<LaneAttachmentMsg>(
            r#"{"type":"lane_attachment","lane_protocol_version":1,"attachment_generation":1,"binding":"","roles":[],"extra":1}"#
        )
        .is_err());
    }

    #[test]
    fn the_session_close_code_is_distinct_and_shares_the_prefix() {
        assert_eq!(
            SESSION_CLOSED_CLOSE_CODE >> 8,
            LANE_VERSION_MISMATCH_CLOSE_CODE >> 8
        );
        assert_ne!(SESSION_CLOSED_CLOSE_CODE, LANE_VERSION_MISMATCH_CLOSE_CODE);
        assert_ne!(
            SESSION_CLOSED_CLOSE_CODE,
            LANE_VERSION_MISMATCH_CLOSE_CODE + 1
        );
        for code in LaneStreamCode::ALL {
            assert_ne!(code.code(), SESSION_CLOSED_CLOSE_CODE);
        }
        assert_eq!(
            PeerTransportMismatch::from_peer_close(
                u64::from(SESSION_CLOSED_CLOSE_CODE),
                SESSION_CLOSED_CLOSE_REASON
            ),
            None
        );
    }

    #[test]
    fn the_legacy_refusal_is_one_unmasked_websocket_close_frame() {
        let frame = legacy_direct_stream_refusal_frame();
        assert_eq!(frame[0], 0x88, "FIN + close opcode");
        assert_eq!(frame[1] & 0x80, 0, "server frames are unmasked");
        let len = usize::from(frame[1]);
        assert!(len <= 125);
        assert_eq!(frame.len(), 2 + len);
        assert_eq!(
            u16::from_be_bytes([frame[2], frame[3]]),
            LEGACY_REFUSAL_WEBSOCKET_CODE
        );
        let reason = std::str::from_utf8(&frame[4..]).unwrap();
        assert_eq!(reason, legacy_deck_refusal_reason());
        assert!(reason.contains("update the Deck"));
    }

    #[test]
    fn lane_stream_codes_are_distinct_and_round_trip() {
        let mut seen = std::collections::BTreeSet::new();
        for code in LaneStreamCode::ALL {
            assert!(seen.insert(code.code()), "{}", code.token());
            assert_ne!(code.code(), LANE_VERSION_MISMATCH_CLOSE_CODE);
            assert_eq!(code.code() >> 8, LANE_VERSION_MISMATCH_CLOSE_CODE >> 8);
            assert_eq!(
                LaneStreamCode::from_code(u64::from(code.code())),
                Some(code)
            );
        }
        assert_eq!(LaneStreamCode::from_code(0), None);
        assert_eq!(
            LaneStreamCode::from_code(u64::from(LANE_VERSION_MISMATCH_CLOSE_CODE)),
            None
        );
    }

    #[test]
    fn session_records_round_trip_with_their_length_prefix() {
        let records = [
            SessionRecord::Text(r#"{"type":"health_ping"}"#.to_owned()),
            SessionRecord::Binary(vec![0x20, 1, 2, 3]),
            SessionRecord::Close(None),
            SessionRecord::Close(Some(SessionClose {
                code: 1008,
                reason: "policy".to_owned(),
            })),
            SessionRecord::Ping(vec![9]),
            SessionRecord::Pong(Vec::new()),
        ];
        for record in records {
            let encoded = record.encode();
            let length = u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize;
            assert_eq!(length, encoded.len() - SESSION_RECORD_LENGTH_BYTES);
            assert_eq!(
                SessionRecord::decode_body(&encoded[SESSION_RECORD_LENGTH_BYTES..]).unwrap(),
                record
            );
        }
    }

    #[test]
    fn malformed_session_records_are_refused() {
        assert_eq!(
            SessionRecord::decode_body(&[]),
            Err(SessionRecordError::Empty)
        );
        assert_eq!(
            SessionRecord::decode_body(&[9]),
            Err(SessionRecordError::UnknownKind(9))
        );
        assert_eq!(
            SessionRecord::decode_body(&[1, 0xff]),
            Err(SessionRecordError::InvalidUtf8)
        );
        assert_eq!(
            SessionRecord::decode_body(&[3, 0]),
            Err(SessionRecordError::MalformedClose)
        );
        let mut long_close = vec![3, 3, 232];
        long_close.extend(std::iter::repeat_n(
            b'x',
            MAX_SESSION_CLOSE_REASON_BYTES + 1,
        ));
        assert_eq!(
            SessionRecord::decode_body(&long_close),
            Err(SessionRecordError::MalformedClose)
        );
    }

    #[test]
    fn an_overlong_close_reason_is_cut_at_a_character_boundary() {
        let record = SessionRecord::Close(Some(SessionClose {
            code: 1000,
            reason: "é".repeat(100),
        }));
        let encoded = record.encode();
        let SessionRecord::Close(Some(close)) =
            SessionRecord::decode_body(&encoded[SESSION_RECORD_LENGTH_BYTES..]).unwrap()
        else {
            panic!("close expected");
        };
        assert!(close.reason.len() <= MAX_SESSION_CLOSE_REASON_BYTES);
        assert_eq!(close.code, 1000);
    }

    #[test]
    fn session_messages_route_to_their_lane() {
        for msg_type in INPUT_CURSOR_MESSAGE_TYPES {
            assert_eq!(lane_for_message_type(msg_type), LaneRole::InputCursor);
        }
        assert_eq!(
            lane_for_session_text(r#"{"type":"key_event","scan_code":65,"pressed":true}"#),
            LaneRole::InputCursor
        );
        assert_eq!(
            lane_for_session_text(r#"{"seq":1,"type":"clipboard_data"}"#),
            LaneRole::BulkAux
        );
        for control in [
            "auth_request",
            "auth_response",
            "auth_result",
            "client_hello",
            "server_hello",
            "quality_settings",
            "display_update",
            "health_ping",
            "health_pong",
            "health_stats",
            "request_full_frame",
            "microphone_stream_result",
            "microphone_stream_stop",
            "audio_stream_result",
            "cursor_mode_result",
            LANE_ATTACHMENT_MSG_TYPE,
            "never_heard_of_it",
        ] {
            assert_eq!(
                lane_for_message_type(control),
                LaneRole::Control,
                "{control}"
            );
        }
        assert_eq!(lane_for_session_text("not json"), LaneRole::Control);
        assert_eq!(lane_for_session_text(r#"{"type":7}"#), LaneRole::Control);
    }

    #[test]
    fn binary_messages_route_by_frame_type_and_audio_never_shares_video() {
        assert_eq!(
            lane_for_binary_message(&[FrameType::Clipboard as u8]),
            Some(LaneRole::BulkAux)
        );
        assert_eq!(
            lane_for_binary_message(&[FrameType::UsbBridgeUrbComplete as u8]),
            Some(LaneRole::BulkAux)
        );
        assert_eq!(
            lane_for_binary_message(&[FrameType::Audio as u8]),
            Some(LaneRole::AudioOutput)
        );
        assert_eq!(
            lane_for_binary_message(&[FrameType::AudioUpstream as u8]),
            Some(LaneRole::Microphone)
        );
        assert_eq!(
            lane_for_binary_message(&[FrameType::VideoH265 as u8]),
            Some(LaneRole::Video)
        );
        assert_eq!(lane_for_binary_message(&[0xEE]), None);
        assert_eq!(lane_for_binary_message(&[]), None);
    }
}
