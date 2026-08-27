//! Direct-monitor QUIC stream preface — the additive shared foundation for
//! the per-monitor reliable stream carrier ("Carrier B") described in the
//! multi-monitor architecture plan.
//!
//! This module only defines the bounded preface/identity wire contract plus
//! the minimal open/accept helpers around it. It does not select Carrier B
//! as the product default, does not change `DirectQuicStream`'s existing
//! single bidirectional carrier ("Carrier A"), and does not enable datagram
//! media. Product adapters and the carrier A/B benchmark remain future work.
//!
//! Wire shape (all multi-byte integers big-endian):
//!
//! | Field | Bytes | Notes |
//! |-------|-------|-------|
//! | magic | 4 | fixed [`MONITOR_STREAM_PREFACE_MAGIC`] |
//! | version | 2 | must equal [`MONITOR_STREAM_PREFACE_VERSION`] exactly |
//! | `session_id_len` | 2 | `1..=`[`MAX_MONITOR_STREAM_SESSION_ID_BYTES`] |
//! | `session_id` | `session_id_len` | UTF-8, control-character free |
//! | `attachment_generation` | 8 | nonzero |
//! | `topology_generation` | 8 | nonzero |
//! | `session_monitor_id` | 2 | nonzero |
//! | `media_plan_fingerprint` | 32 | opaque, caller-computed |
//!
//! Everything after the fixed 8-byte header (magic + version + session
//! length) has an exact, bounded length once the declared session length is
//! known, so a reader parses it with two bounded reads and never allocates
//! past [`MAX_MONITOR_STREAM_PREFACE_BYTES`] — even before validating the
//! rest of the frame.

use std::fmt::{Display, Formatter};
use std::num::{NonZeroU16, NonZeroU64};
use std::time::Duration;

use quinn::{Connection, RecvStream, SendStream};

use super::QuicTransportError;

/// Fixed magic identifying a direct-monitor stream preface frame.
pub const MONITOR_STREAM_PREFACE_MAGIC: [u8; 4] = *b"ARMP";

/// Current direct-monitor stream preface wire version. Bumping this is a
/// reviewed, breaking protocol change.
pub const MONITOR_STREAM_PREFACE_VERSION: u16 = 1;

/// Fixed byte length of the caller-computed media-plan fingerprint (sized
/// for a SHA-256 digest). This module never computes or verifies the
/// fingerprint itself; callers own that binding and pass it through opaque.
pub const MEDIA_PLAN_FINGERPRINT_BYTES: usize = 32;

/// Hard cap on the claimed session identifier length within a preface frame.
pub const MAX_MONITOR_STREAM_SESSION_ID_BYTES: usize = 512;

/// Bytes fixed regardless of session id length: `attachment_generation`
/// (8 bytes), `topology_generation` (8 bytes), `session_monitor_id`
/// (2 bytes), and `media_plan_fingerprint`
/// ([`MEDIA_PLAN_FINGERPRINT_BYTES`] bytes).
const FIXED_TAIL_BYTES: usize = 8 + 8 + 2 + MEDIA_PLAN_FINGERPRINT_BYTES;

/// Bytes read before the session id length is known: magic (4) + version (2)
/// + session id length (2).
const HEADER_BYTES: usize = 4 + 2 + 2;

/// Hard cap on a fully encoded preface frame, guarding against unbounded
/// allocation from a malformed or hostile peer before any registry lookup
/// runs.
pub const MAX_MONITOR_STREAM_PREFACE_BYTES: usize =
    HEADER_BYTES + MAX_MONITOR_STREAM_SESSION_ID_BYTES + FIXED_TAIL_BYTES;

/// Maximum simultaneous per-monitor streams on one connection, matching the
/// product's 1-4 monitor topology bound. Also used to size the
/// `max_concurrent_uni_streams` value in the separate, test-only
/// [`super::config::monitor_carrier_transport_config`] — the live
/// [`super::config::recommended_transport_config`] is unaffected and keeps
/// its existing limit of 1.
pub const MAX_MONITOR_STREAMS_PER_CONNECTION: usize = 4;

/// Validated, bounded identity carried by one direct-monitor stream preface.
///
/// Every field is fail-closed by construction: both generations and the
/// monitor id are non-zero (`NonZeroU64`/`NonZeroU16`), and the session
/// identifier is bounded and control-character free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorStreamIdentity {
    session_id: String,
    attachment_generation: NonZeroU64,
    topology_generation: NonZeroU64,
    session_monitor_id: NonZeroU16,
    media_plan_fingerprint: [u8; MEDIA_PLAN_FINGERPRINT_BYTES],
}

/// Explicit, fail-closed preface rejection reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorStreamPrefaceError {
    /// The session identifier is empty, oversized, or contains a control
    /// character.
    InvalidSessionId,
    /// The frame could not be decoded (bad magic, bad version, truncated, or
    /// oversized).
    Malformed,
}

impl Display for MonitorStreamPrefaceError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InvalidSessionId => {
                "monitor stream preface session id was empty, oversized, or control-bearing"
            }
            Self::Malformed => "monitor stream preface frame was malformed",
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for MonitorStreamPrefaceError {}

impl MonitorStreamIdentity {
    /// Creates a validated direct-monitor stream identity.
    ///
    /// # Errors
    ///
    /// Returns [`MonitorStreamPrefaceError::InvalidSessionId`] when the
    /// session identifier is empty, exceeds
    /// [`MAX_MONITOR_STREAM_SESSION_ID_BYTES`], or contains a control
    /// character. Generations and the monitor id can never be zero because
    /// callers must supply `NonZeroU64`/`NonZeroU16`.
    pub fn new(
        session_id: impl Into<String>,
        attachment_generation: NonZeroU64,
        topology_generation: NonZeroU64,
        session_monitor_id: NonZeroU16,
        media_plan_fingerprint: [u8; MEDIA_PLAN_FINGERPRINT_BYTES],
    ) -> Result<Self, MonitorStreamPrefaceError> {
        let session_id = session_id.into();
        validate_session_id(session_id.as_bytes())?;
        Ok(Self {
            session_id,
            attachment_generation,
            topology_generation,
            session_monitor_id,
            media_plan_fingerprint,
        })
    }

    /// Returns the bounded, control-free session identifier.
    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// Returns the nonzero attachment generation.
    #[must_use]
    pub const fn attachment_generation(&self) -> NonZeroU64 {
        self.attachment_generation
    }

    /// Returns the nonzero topology generation.
    #[must_use]
    pub const fn topology_generation(&self) -> NonZeroU64 {
        self.topology_generation
    }

    /// Returns the nonzero session-scoped monitor id.
    #[must_use]
    pub const fn session_monitor_id(&self) -> NonZeroU16 {
        self.session_monitor_id
    }

    /// Returns the opaque, caller-computed media-plan fingerprint.
    #[must_use]
    pub const fn media_plan_fingerprint(&self) -> &[u8; MEDIA_PLAN_FINGERPRINT_BYTES] {
        &self.media_plan_fingerprint
    }

    /// Encodes this identity as a bounded binary preface frame.
    ///
    /// # Errors
    ///
    /// Returns [`MonitorStreamPrefaceError::Malformed`] only if the encoded
    /// frame would exceed [`MAX_MONITOR_STREAM_PREFACE_BYTES`], which cannot
    /// happen for an identity constructed through [`Self::new`].
    pub fn encode(&self) -> Result<Vec<u8>, MonitorStreamPrefaceError> {
        let session_bytes = self.session_id.as_bytes();
        let session_len =
            u16::try_from(session_bytes.len()).map_err(|_| MonitorStreamPrefaceError::Malformed)?;
        let mut buffer = Vec::with_capacity(HEADER_BYTES + session_bytes.len() + FIXED_TAIL_BYTES);
        buffer.extend_from_slice(&MONITOR_STREAM_PREFACE_MAGIC);
        buffer.extend_from_slice(&MONITOR_STREAM_PREFACE_VERSION.to_be_bytes());
        buffer.extend_from_slice(&session_len.to_be_bytes());
        buffer.extend_from_slice(session_bytes);
        buffer.extend_from_slice(&self.attachment_generation.get().to_be_bytes());
        buffer.extend_from_slice(&self.topology_generation.get().to_be_bytes());
        buffer.extend_from_slice(&self.session_monitor_id.get().to_be_bytes());
        buffer.extend_from_slice(&self.media_plan_fingerprint);
        if buffer.len() > MAX_MONITOR_STREAM_PREFACE_BYTES {
            return Err(MonitorStreamPrefaceError::Malformed);
        }
        Ok(buffer)
    }

    /// Decodes a bounded binary preface frame produced by [`Self::encode`].
    ///
    /// `bytes` must contain exactly the preface: any application payload
    /// that follows it on the same stream is the caller's concern and must
    /// not be included here.
    ///
    /// # Errors
    ///
    /// Returns [`MonitorStreamPrefaceError::Malformed`] for a bad magic, bad
    /// version, truncated, or oversized frame, or
    /// [`MonitorStreamPrefaceError::InvalidSessionId`] for an invalid
    /// embedded session id.
    pub fn decode(bytes: &[u8]) -> Result<Self, MonitorStreamPrefaceError> {
        if bytes.len() > MAX_MONITOR_STREAM_PREFACE_BYTES || bytes.len() < HEADER_BYTES {
            return Err(MonitorStreamPrefaceError::Malformed);
        }
        if bytes[0..4] != MONITOR_STREAM_PREFACE_MAGIC {
            return Err(MonitorStreamPrefaceError::Malformed);
        }
        let version = u16::from_be_bytes(
            bytes[4..6]
                .try_into()
                .map_err(|_| MonitorStreamPrefaceError::Malformed)?,
        );
        if version != MONITOR_STREAM_PREFACE_VERSION {
            return Err(MonitorStreamPrefaceError::Malformed);
        }
        let session_len = u16::from_be_bytes(
            bytes[6..8]
                .try_into()
                .map_err(|_| MonitorStreamPrefaceError::Malformed)?,
        ) as usize;
        if session_len == 0 || session_len > MAX_MONITOR_STREAM_SESSION_ID_BYTES {
            return Err(MonitorStreamPrefaceError::Malformed);
        }
        if bytes.len() != HEADER_BYTES + session_len + FIXED_TAIL_BYTES {
            return Err(MonitorStreamPrefaceError::Malformed);
        }

        let session_bytes = &bytes[HEADER_BYTES..HEADER_BYTES + session_len];
        let session_id = validate_session_id(session_bytes)?.to_owned();

        let mut offset = HEADER_BYTES + session_len;
        let attachment_generation = u64::from_be_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .map_err(|_| MonitorStreamPrefaceError::Malformed)?,
        );
        offset += 8;
        let topology_generation = u64::from_be_bytes(
            bytes[offset..offset + 8]
                .try_into()
                .map_err(|_| MonitorStreamPrefaceError::Malformed)?,
        );
        offset += 8;
        let session_monitor_id = u16::from_be_bytes(
            bytes[offset..offset + 2]
                .try_into()
                .map_err(|_| MonitorStreamPrefaceError::Malformed)?,
        );
        offset += 2;
        let mut media_plan_fingerprint = [0_u8; MEDIA_PLAN_FINGERPRINT_BYTES];
        media_plan_fingerprint
            .copy_from_slice(&bytes[offset..offset + MEDIA_PLAN_FINGERPRINT_BYTES]);

        let attachment_generation =
            NonZeroU64::new(attachment_generation).ok_or(MonitorStreamPrefaceError::Malformed)?;
        let topology_generation =
            NonZeroU64::new(topology_generation).ok_or(MonitorStreamPrefaceError::Malformed)?;
        let session_monitor_id =
            NonZeroU16::new(session_monitor_id).ok_or(MonitorStreamPrefaceError::Malformed)?;

        Ok(Self {
            session_id,
            attachment_generation,
            topology_generation,
            session_monitor_id,
            media_plan_fingerprint,
        })
    }
}

fn validate_session_id(session_bytes: &[u8]) -> Result<&str, MonitorStreamPrefaceError> {
    if session_bytes.is_empty() || session_bytes.len() > MAX_MONITOR_STREAM_SESSION_ID_BYTES {
        return Err(MonitorStreamPrefaceError::InvalidSessionId);
    }
    let session_str = std::str::from_utf8(session_bytes)
        .map_err(|_| MonitorStreamPrefaceError::InvalidSessionId)?;
    if session_str.chars().any(char::is_control) {
        return Err(MonitorStreamPrefaceError::InvalidSessionId);
    }
    Ok(session_str)
}

/// Opens a new outbound direct-monitor stream and immediately writes the
/// entire bounded preface before returning the owned send stream. Callers
/// write monitor media payload to the returned stream afterward; this
/// helper never sends anything beyond the preface itself.
///
/// `write_all` already hands every preface byte to the QUIC connection's
/// send queue before resolving: unlike a buffered TCP socket, a QUIC
/// `SendStream` has no separate application-level flush step (Quinn's own
/// `AsyncWrite::poll_flush` for `SendStream` is an unconditional, immediate
/// success), so there is nothing further to flush once this returns.
///
/// # Errors
///
/// Returns a connection failure if the unidirectional stream cannot be
/// opened, or a stream write failure if the preface cannot be fully
/// written.
pub async fn open_monitor_stream(
    connection: &Connection,
    identity: &MonitorStreamIdentity,
) -> Result<SendStream, QuicTransportError> {
    let frame = identity
        .encode()
        .map_err(QuicTransportError::MonitorPreface)?;
    let mut send = connection
        .open_uni()
        .await
        .map_err(QuicTransportError::Connection)?;
    send.write_all(&frame)
        .await
        .map_err(QuicTransportError::StreamWrite)?;
    Ok(send)
}

/// Accepts the next inbound unidirectional stream and parses/validates its
/// bounded direct-monitor preface before returning the owned receive stream
/// together with the validated identity. `timeout` is one overall deadline
/// covering both accepting the stream and reading the complete preface, so
/// a peer that trickles bytes just under any single read cannot extend the
/// wait indefinitely.
///
/// The exact preface length is derived from an 8-byte header before the
/// (still-bounded) remainder is read, so a hostile or malformed peer cannot
/// force an unbounded allocation or read: an oversized claimed session
/// length is rejected immediately, before any further bytes are requested.
///
/// # Errors
///
/// Returns [`QuicTransportError::MonitorStreamTimedOut`] if no stream or
/// complete preface arrives in time, a connection failure if the connection
/// closes first, a stream read failure on truncation, or
/// [`QuicTransportError::MonitorPreface`] for a malformed or oversized
/// preface.
pub async fn accept_monitor_stream(
    connection: &Connection,
    timeout: Duration,
) -> Result<(RecvStream, MonitorStreamIdentity), QuicTransportError> {
    // One overall deadline covers accepting the stream and reading the full
    // bounded preface, so a peer that dribbles bytes just under a per-read
    // timeout can never keep this call pending indefinitely.
    tokio::time::timeout(timeout, accept_monitor_stream_inner(connection))
        .await
        .map_err(|_| QuicTransportError::MonitorStreamTimedOut)?
}

async fn accept_monitor_stream_inner(
    connection: &Connection,
) -> Result<(RecvStream, MonitorStreamIdentity), QuicTransportError> {
    let mut recv = connection
        .accept_uni()
        .await
        .map_err(QuicTransportError::Connection)?;

    let mut header = [0_u8; HEADER_BYTES];
    recv.read_exact(&mut header)
        .await
        .map_err(QuicTransportError::StreamRead)?;

    let session_len = u16::from_be_bytes([header[6], header[7]]) as usize;
    if header[0..4] != MONITOR_STREAM_PREFACE_MAGIC
        || u16::from_be_bytes([header[4], header[5]]) != MONITOR_STREAM_PREFACE_VERSION
        || session_len == 0
        || session_len > MAX_MONITOR_STREAM_SESSION_ID_BYTES
    {
        return Err(QuicTransportError::MonitorPreface(
            MonitorStreamPrefaceError::Malformed,
        ));
    }

    let total_len = HEADER_BYTES + session_len + FIXED_TAIL_BYTES;
    let mut frame = vec![0_u8; total_len];
    frame[..HEADER_BYTES].copy_from_slice(&header);
    recv.read_exact(&mut frame[HEADER_BYTES..])
        .await
        .map_err(QuicTransportError::StreamRead)?;

    let identity =
        MonitorStreamIdentity::decode(&frame).map_err(QuicTransportError::MonitorPreface)?;
    Ok((recv, identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_identity() -> MonitorStreamIdentity {
        MonitorStreamIdentity::new(
            "session-abc",
            NonZeroU64::new(7).expect("nonzero"),
            NonZeroU64::new(3).expect("nonzero"),
            NonZeroU16::new(2).expect("nonzero"),
            [9_u8; MEDIA_PLAN_FINGERPRINT_BYTES],
        )
        .expect("valid identity")
    }

    #[test]
    fn identity_round_trips() {
        let identity = sample_identity();
        let encoded = identity.encode().expect("encode");
        assert_eq!(MonitorStreamIdentity::decode(&encoded), Ok(identity));
    }

    #[test]
    fn accessors_expose_validated_fields() {
        let identity = sample_identity();
        assert_eq!(identity.session_id(), "session-abc");
        assert_eq!(identity.attachment_generation().get(), 7);
        assert_eq!(identity.topology_generation().get(), 3);
        assert_eq!(identity.session_monitor_id().get(), 2);
        assert_eq!(
            identity.media_plan_fingerprint(),
            &[9_u8; MEDIA_PLAN_FINGERPRINT_BYTES]
        );
    }

    #[test]
    fn empty_and_oversized_session_ids_are_rejected() {
        for session_id in [
            String::new(),
            "x".repeat(MAX_MONITOR_STREAM_SESSION_ID_BYTES + 1),
        ] {
            assert_eq!(
                MonitorStreamIdentity::new(
                    session_id,
                    NonZeroU64::new(1).expect("nonzero"),
                    NonZeroU64::new(1).expect("nonzero"),
                    NonZeroU16::new(1).expect("nonzero"),
                    [0_u8; MEDIA_PLAN_FINGERPRINT_BYTES],
                ),
                Err(MonitorStreamPrefaceError::InvalidSessionId)
            );
        }
    }

    #[test]
    fn control_bearing_session_id_is_rejected() {
        assert_eq!(
            MonitorStreamIdentity::new(
                "session\u{0007}bell",
                NonZeroU64::new(1).expect("nonzero"),
                NonZeroU64::new(1).expect("nonzero"),
                NonZeroU16::new(1).expect("nonzero"),
                [0_u8; MEDIA_PLAN_FINGERPRINT_BYTES],
            ),
            Err(MonitorStreamPrefaceError::InvalidSessionId)
        );
    }

    #[test]
    fn malformed_magic_is_rejected() {
        let mut encoded = sample_identity().encode().expect("encode");
        encoded[0] = b'X';
        assert_eq!(
            MonitorStreamIdentity::decode(&encoded),
            Err(MonitorStreamPrefaceError::Malformed)
        );
    }

    #[test]
    fn wrong_version_is_rejected() {
        let mut encoded = sample_identity().encode().expect("encode");
        encoded[4..6].copy_from_slice(&99_u16.to_be_bytes());
        assert_eq!(
            MonitorStreamIdentity::decode(&encoded),
            Err(MonitorStreamPrefaceError::Malformed)
        );
    }

    #[test]
    fn truncated_frame_is_rejected() {
        let encoded = sample_identity().encode().expect("encode");
        assert_eq!(
            MonitorStreamIdentity::decode(&encoded[..encoded.len() - 1]),
            Err(MonitorStreamPrefaceError::Malformed)
        );
        assert_eq!(
            MonitorStreamIdentity::decode(&[]),
            Err(MonitorStreamPrefaceError::Malformed)
        );
        assert_eq!(
            MonitorStreamIdentity::decode(b"short"),
            Err(MonitorStreamPrefaceError::Malformed)
        );
    }

    #[test]
    fn oversized_frame_is_rejected() {
        let oversized = vec![0_u8; MAX_MONITOR_STREAM_PREFACE_BYTES + 1];
        assert_eq!(
            MonitorStreamIdentity::decode(&oversized),
            Err(MonitorStreamPrefaceError::Malformed)
        );
    }

    #[test]
    fn oversized_claimed_session_length_is_rejected_without_reading_further() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MONITOR_STREAM_PREFACE_MAGIC);
        bytes.extend_from_slice(&MONITOR_STREAM_PREFACE_VERSION.to_be_bytes());
        // Claims a session id far larger than the bound, with no further
        // bytes supplied at all.
        bytes.extend_from_slice(&u16::MAX.to_be_bytes());
        assert_eq!(
            MonitorStreamIdentity::decode(&bytes),
            Err(MonitorStreamPrefaceError::Malformed)
        );
    }

    #[test]
    fn trailing_bytes_after_the_exact_frame_are_rejected() {
        let mut encoded = sample_identity().encode().expect("encode");
        encoded.push(0);
        assert_eq!(
            MonitorStreamIdentity::decode(&encoded),
            Err(MonitorStreamPrefaceError::Malformed)
        );
    }

    #[test]
    fn max_monitor_streams_matches_the_product_topology_bound() {
        assert_eq!(MAX_MONITOR_STREAMS_PER_CONNECTION, 4);
    }
}
