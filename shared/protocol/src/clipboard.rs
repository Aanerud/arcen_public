//! Bounded clipboard offer reassembly, and the framing that produces one.
//!
//! Receiving a transfer was already shared; sending one was not, and was
//! written three times — once on macOS and twice on Linux. [`ClipboardTransfer`]
//! closes that asymmetry, so an offer and the chunks that satisfy it are built
//! by the same code that knows how to take them apart.

use crate::messages::{ClipboardContentKind, ClipboardDataMsg, CLIPBOARD_DATA};
use crate::wire::{
    encode_clipboard_chunk, ClipboardChunkHeader, CHUNK_BYTES, HARD_MAX_CLIPBOARD_BYTES,
};
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::time::{Duration, Instant};
use zeroize::Zeroize;

/// Maximum interval without accepted progress.
pub const CLIPBOARD_REASSEMBLY_TIMEOUT: Duration = Duration::from_secs(5);

/// Completed, validated-by-framing clipboard bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedClipboardData {
    pub sequence: u64,
    pub kind: ClipboardContentKind,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl CompletedClipboardData {
    /// Transfers payload ownership after content validation.
    #[must_use]
    pub fn take_bytes(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.bytes)
    }
}

impl Drop for CompletedClipboardData {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

#[derive(Debug)]
struct InFlight {
    sequence: u64,
    kind: ClipboardContentKind,
    total_size: usize,
    truncated: bool,
    bytes: Vec<u8>,
    last_progress: Instant,
}

impl InFlight {
    fn scrub(&mut self) {
        self.bytes.zeroize();
        self.bytes.clear();
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.bytes.zeroize();
    }
}

/// One-in-flight, contiguous, latest-wins clipboard reassembler.
#[derive(Debug)]
pub struct ClipboardReassembler {
    maximum: usize,
    latest_sequence: u64,
    in_flight: Option<InFlight>,
}

impl ClipboardReassembler {
    /// Creates a reassembler with a maximum in `1..=20 MiB`.
    ///
    /// # Errors
    ///
    /// Rejects zero and values above the protocol hard maximum.
    pub const fn new(maximum: usize) -> Result<Self, ClipboardReassemblyError> {
        if maximum == 0 || maximum > HARD_MAX_CLIPBOARD_BYTES {
            return Err(ClipboardReassemblyError::InvalidMaximum);
        }
        Ok(Self {
            maximum,
            latest_sequence: 0,
            in_flight: None,
        })
    }

    /// Accepts a newer offer and scrubs any older partial item.
    ///
    /// # Errors
    ///
    /// Rejects invalid metadata, stale sequences, and oversize offers.
    pub fn begin(&mut self, offer: ClipboardDataMsg) -> Result<(), ClipboardReassemblyError> {
        self.begin_at(offer, Instant::now())
    }

    /// Deterministic timestamp-injected form of [`Self::begin`].
    pub fn begin_at(
        &mut self,
        offer: ClipboardDataMsg,
        now: Instant,
    ) -> Result<(), ClipboardReassemblyError> {
        if offer.msg_type != CLIPBOARD_DATA
            || offer.sequence == 0
            || offer.size_bytes == 0
            || (offer.truncated && offer.kind != ClipboardContentKind::TextUtf8)
        {
            return Err(ClipboardReassemblyError::InvalidOffer);
        }
        if offer.sequence <= self.latest_sequence {
            return Err(ClipboardReassemblyError::StaleSequence);
        }
        let total_size =
            usize::try_from(offer.size_bytes).map_err(|_| ClipboardReassemblyError::Oversize)?;
        if total_size > self.maximum || total_size > HARD_MAX_CLIPBOARD_BYTES {
            return Err(ClipboardReassemblyError::Oversize);
        }

        self.abort();
        self.latest_sequence = offer.sequence;
        self.in_flight = Some(InFlight {
            sequence: offer.sequence,
            kind: offer.kind,
            total_size,
            truncated: offer.truncated,
            bytes: Vec::new(),
            last_progress: now,
        });
        Ok(())
    }

    /// Appends exactly the next contiguous chunk.
    ///
    /// # Errors
    ///
    /// Rejects missing offers, metadata mismatch, stale/noncontiguous chunks,
    /// oversize growth, and failed bounded allocation.
    pub fn push(
        &mut self,
        header: ClipboardChunkHeader,
        payload: &[u8],
    ) -> Result<Option<CompletedClipboardData>, ClipboardReassemblyError> {
        self.push_at(header, payload, Instant::now())
    }

    /// Deterministic timestamp-injected form of [`Self::push`].
    pub fn push_at(
        &mut self,
        header: ClipboardChunkHeader,
        payload: &[u8],
        now: Instant,
    ) -> Result<Option<CompletedClipboardData>, ClipboardReassemblyError> {
        if payload.is_empty() || payload.len() > CHUNK_BYTES {
            return Err(ClipboardReassemblyError::ChunkSize);
        }
        if self.in_flight.as_ref().is_some_and(|in_flight| {
            now.saturating_duration_since(in_flight.last_progress) >= CLIPBOARD_REASSEMBLY_TIMEOUT
        }) {
            self.abort();
            return Err(ClipboardReassemblyError::Expired);
        }
        let in_flight = self
            .in_flight
            .as_mut()
            .ok_or(ClipboardReassemblyError::MissingOffer)?;
        let header_total =
            usize::try_from(header.total_size).map_err(|_| ClipboardReassemblyError::Mismatch)?;
        let header_offset =
            usize::try_from(header.offset).map_err(|_| ClipboardReassemblyError::Mismatch)?;
        if header.sequence != in_flight.sequence
            || header.kind != in_flight.kind
            || header_total != in_flight.total_size
        {
            return Err(ClipboardReassemblyError::Mismatch);
        }
        if header_offset != in_flight.bytes.len() {
            return Err(ClipboardReassemblyError::NonContiguous);
        }
        let new_len = in_flight
            .bytes
            .len()
            .checked_add(payload.len())
            .ok_or(ClipboardReassemblyError::Oversize)?;
        if new_len > in_flight.total_size || new_len > self.maximum {
            return Err(ClipboardReassemblyError::Oversize);
        }
        in_flight
            .bytes
            .try_reserve(payload.len())
            .map_err(|_| ClipboardReassemblyError::AllocationFailed)?;
        in_flight.bytes.extend_from_slice(payload);
        in_flight.last_progress = now;

        if new_len != in_flight.total_size {
            return Ok(None);
        }
        let mut completed = self
            .in_flight
            .take()
            .ok_or(ClipboardReassemblyError::MissingOffer)?;
        let bytes = std::mem::take(&mut completed.bytes);
        Ok(Some(CompletedClipboardData {
            sequence: completed.sequence,
            kind: completed.kind,
            bytes,
            truncated: completed.truncated,
        }))
    }

    /// Scrubs and drops a partial item.
    pub fn abort(&mut self) {
        if let Some(mut in_flight) = self.in_flight.take() {
            in_flight.scrub();
        }
    }

    /// Scrubs an item after five seconds without accepted progress.
    #[must_use]
    pub fn expire(&mut self, now: Instant) -> bool {
        let expired = self.in_flight.as_ref().is_some_and(|in_flight| {
            now.saturating_duration_since(in_flight.last_progress) >= CLIPBOARD_REASSEMBLY_TIMEOUT
        });
        if expired {
            self.abort();
        }
        expired
    }

    /// Returns buffered bytes for memory-bound assertions.
    #[must_use]
    pub fn buffered_len(&self) -> usize {
        self.in_flight
            .as_ref()
            .map_or(0, |in_flight| in_flight.bytes.len())
    }

    /// Returns the newest accepted offer sequence.
    #[must_use]
    pub const fn latest_sequence(&self) -> u64 {
        self.latest_sequence
    }
}

impl Drop for ClipboardReassembler {
    fn drop(&mut self) {
        self.abort();
    }
}

/// Clipboard offer or chunk reassembly failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardReassemblyError {
    InvalidMaximum,
    InvalidOffer,
    StaleSequence,
    Oversize,
    MissingOffer,
    Mismatch,
    NonContiguous,
    ChunkSize,
    Expired,
    AllocationFailed,
}

impl Display for ClipboardReassemblyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidMaximum => formatter.write_str("invalid clipboard reassembly maximum"),
            Self::InvalidOffer => formatter.write_str("invalid clipboard offer"),
            Self::StaleSequence => formatter.write_str("stale clipboard sequence"),
            Self::Oversize => formatter.write_str("clipboard reassembly exceeds bound"),
            Self::MissingOffer => formatter.write_str("clipboard chunk has no accepted offer"),
            Self::Mismatch => formatter.write_str("clipboard chunk metadata mismatch"),
            Self::NonContiguous => formatter.write_str("clipboard chunk is not contiguous"),
            Self::ChunkSize => formatter.write_str("invalid clipboard chunk size"),
            Self::Expired => formatter.write_str("clipboard reassembly expired"),
            Self::AllocationFailed => formatter.write_str("clipboard reassembly allocation failed"),
        }
    }
}

impl Error for ClipboardReassemblyError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn offer(sequence: u64, size: u32) -> ClipboardDataMsg {
        ClipboardDataMsg::new(sequence, ClipboardContentKind::TextUtf8, size, false)
    }

    fn header(sequence: u64, total_size: u32, offset: u32) -> ClipboardChunkHeader {
        ClipboardChunkHeader {
            kind: ClipboardContentKind::TextUtf8,
            sequence,
            total_size,
            offset,
        }
    }

    #[test]
    fn contiguous_chunks_complete_and_reject_gap_overlap() {
        let now = Instant::now();
        let mut reassembler = ClipboardReassembler::new(32).unwrap();
        reassembler.begin_at(offer(1, 4), now).unwrap();
        assert_eq!(
            reassembler.push_at(header(1, 4, 0), b"ab", now).unwrap(),
            None
        );
        assert_eq!(
            reassembler.push_at(header(1, 4, 3), b"c", now),
            Err(ClipboardReassemblyError::NonContiguous)
        );
        assert_eq!(
            reassembler.push_at(header(1, 4, 1), b"c", now),
            Err(ClipboardReassemblyError::NonContiguous)
        );
        assert_eq!(
            reassembler.push_at(header(1, 4, 2), b"cd", now).unwrap(),
            Some(CompletedClipboardData {
                sequence: 1,
                kind: ClipboardContentKind::TextUtf8,
                bytes: b"abcd".to_vec(),
                truncated: false
            })
        );
    }

    #[test]
    fn newer_offer_replaces_older_and_stale_never_returns() {
        let now = Instant::now();
        let mut reassembler = ClipboardReassembler::new(32).unwrap();
        reassembler.begin_at(offer(3, 4), now).unwrap();
        reassembler.push_at(header(3, 4, 0), b"old", now).unwrap();
        reassembler.begin_at(offer(4, 3), now).unwrap();
        assert_eq!(reassembler.buffered_len(), 0);
        assert_eq!(
            reassembler.begin_at(offer(3, 1), now),
            Err(ClipboardReassemblyError::StaleSequence)
        );
        assert_eq!(
            reassembler
                .push_at(header(4, 3, 0), b"new", now)
                .unwrap()
                .unwrap()
                .bytes,
            b"new"
        );
    }

    #[test]
    fn timeout_aborts_and_twenty_chunks_stay_bounded() {
        let start = Instant::now();
        let mut reassembler = ClipboardReassembler::new(HARD_MAX_CLIPBOARD_BYTES).unwrap();
        reassembler
            .begin_at(
                ClipboardDataMsg::new(
                    1,
                    ClipboardContentKind::ImagePng,
                    HARD_MAX_CLIPBOARD_BYTES as u32,
                    false,
                ),
                start,
            )
            .unwrap();
        let chunk = vec![0x5a; CHUNK_BYTES];
        for index in 0..20 {
            let complete = reassembler
                .push_at(
                    ClipboardChunkHeader {
                        kind: ClipboardContentKind::ImagePng,
                        sequence: 1,
                        total_size: HARD_MAX_CLIPBOARD_BYTES as u32,
                        offset: (index * CHUNK_BYTES) as u32,
                    },
                    &chunk,
                    start,
                )
                .unwrap();
            assert_eq!(complete.is_some(), index == 19);
        }
        reassembler.begin_at(offer(2, 4), start).unwrap();
        assert!(!reassembler.expire(start + Duration::from_millis(4_999)));
        assert_eq!(
            reassembler.push_at(header(2, 4, 0), b"a", start + CLIPBOARD_REASSEMBLY_TIMEOUT),
            Err(ClipboardReassemblyError::Expired)
        );
        assert_eq!(reassembler.buffered_len(), 0);
    }
}

/// A clipboard payload on its way out, and the frames that carry it.
///
/// The send side of [`ClipboardReassembler`]. Keeping them in one file is the
/// point: the offer's `total_size` and the chunk offsets have to agree, and a
/// host that builds them separately from the code that checks them is how they
/// stop agreeing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardTransfer {
    sequence: u64,
    kind: ClipboardContentKind,
    bytes: Vec<u8>,
    truncated: bool,
}

impl Drop for ClipboardTransfer {
    fn drop(&mut self) {
        // A clipboard routinely holds a password, a token or a private key,
        // because that is what people copy. Leaving it in freed memory is a
        // longer exposure than the transfer itself. Two hosts scrubbed it and
        // one did not; owning the payload here is what makes that uniform.
        self.bytes.zeroize();
    }
}

/// One message in a clipboard transfer, in the order it must be sent.
///
/// Deliberately not a transport type: the offer is text and the chunks are
/// binary in every transport Arcen speaks, but naming a concrete message type
/// here would drag a websocket dependency into the protocol crate. Each caller
/// maps these two cases onto its own send path, which is the only part of this
/// that is genuinely theirs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardWireMessage {
    /// The offer that announces the transfer. Must be sent first.
    Offer(ClipboardDataMsg),
    /// One framed chunk of the payload.
    Chunk(Vec<u8>),
}

/// Yields a transfer one wire message at a time, offer first.
///
/// The ordering rule — offer, then contiguous chunks, then done — was written
/// out separately on every host, and each copy had its own arithmetic for the
/// chunk boundary and its own conversion for the offsets. The receiver rejects
/// any transfer whose offsets are not contiguous and increasing, so those
/// copies were four chances to disagree with the one reassembler that has to
/// accept all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardCursor {
    transfer: ClipboardTransfer,
    offer_sent: bool,
    offset: usize,
}

impl ClipboardCursor {
    /// Returns whether every message has been yielded.
    #[must_use]
    // Not const: Vec::len only became const in 1.87, above the workspace MSRV.
    pub fn finished(&self) -> bool {
        self.offer_sent && self.offset == self.transfer.bytes.len()
    }

    /// Returns the transfer being sent.
    #[must_use]
    pub const fn transfer(&self) -> &ClipboardTransfer {
        &self.transfer
    }

    /// Returns the next message, or `None` once the transfer is complete.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardFramingError::ChunkEncoding`] when a chunk cannot be
    /// encoded.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "ClipboardTransfer::new rejected any length that does not fit u32"
    )]
    pub fn next_message(&mut self) -> Option<Result<ClipboardWireMessage, ClipboardFramingError>> {
        if !self.offer_sent {
            self.offer_sent = true;
            return Some(Ok(ClipboardWireMessage::Offer(self.transfer.offer())));
        }
        if self.offset == self.transfer.bytes.len() {
            return None;
        }
        let end = self
            .offset
            .saturating_add(CHUNK_BYTES)
            .min(self.transfer.bytes.len());
        let frame = encode_clipboard_chunk(
            ClipboardChunkHeader {
                kind: self.transfer.kind,
                sequence: self.transfer.sequence,
                total_size: self.transfer.bytes.len() as u32,
                offset: self.offset as u32,
            },
            &self.transfer.bytes[self.offset..end],
        );
        match frame {
            Ok(frame) => {
                self.offset = end;
                Some(Ok(ClipboardWireMessage::Chunk(frame)))
            }
            Err(_) => Some(Err(ClipboardFramingError::ChunkEncoding)),
        }
    }
}

/// Why a clipboard payload could not be framed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFramingError {
    /// The payload is empty, which the wire does not carry.
    ///
    /// The chunk validator rejects zero-shaped metadata, so an empty transfer
    /// cannot be encoded at all. Refusing it here, at construction, is the
    /// difference between a caller deciding not to offer an empty clipboard
    /// and a caller discovering at framing time that it cannot — which on at
    /// least one host turned into a stream error that would end the session.
    EmptyPayload,
    /// The payload is larger than the wire's 32-bit size field can describe.
    PayloadTooLarge,
    /// A chunk could not be encoded.
    ChunkEncoding,
}

impl core::fmt::Display for ClipboardFramingError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::EmptyPayload => {
                write!(
                    formatter,
                    "the clipboard wire format does not carry an empty transfer"
                )
            }
            Self::PayloadTooLarge => {
                write!(formatter, "clipboard payload exceeds the 32-bit wire size")
            }
            Self::ChunkEncoding => write!(formatter, "clipboard chunk could not be encoded"),
        }
    }
}

impl core::error::Error for ClipboardFramingError {}

impl ClipboardTransfer {
    /// Creates a transfer for one payload.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardFramingError::EmptyPayload`] for an empty payload,
    /// which the wire does not carry, and
    /// [`ClipboardFramingError::PayloadTooLarge`] for one the 32-bit size
    /// field cannot describe. Both are rejected here rather than at framing
    /// time so a caller cannot announce a transfer it will then fail to send.
    pub fn new(
        sequence: u64,
        kind: ClipboardContentKind,
        bytes: Vec<u8>,
    ) -> Result<Self, ClipboardFramingError> {
        if bytes.is_empty() {
            return Err(ClipboardFramingError::EmptyPayload);
        }
        u32::try_from(bytes.len()).map_err(|_| ClipboardFramingError::PayloadTooLarge)?;
        Ok(Self {
            sequence,
            kind,
            bytes,
            truncated: false,
        })
    }

    /// Marks this payload as one a size policy cut short.
    ///
    /// The flag travels in the offer so the peer can tell a deliberate partial
    /// clipboard from a transfer that failed part way.
    #[must_use]
    pub const fn with_truncated(mut self, truncated: bool) -> Self {
        self.truncated = truncated;
        self
    }

    /// Returns whether a size policy cut this payload short.
    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// Returns a cursor that yields this transfer one wire message at a time.
    ///
    /// Preferred over [`Self::chunks`] on a streaming send path: a large paste
    /// never has to exist as a vector of frames at once, and the caller can
    /// interleave it with the rest of the session instead of blocking on it.
    #[must_use]
    pub const fn into_cursor(self) -> ClipboardCursor {
        ClipboardCursor {
            transfer: self,
            offer_sent: false,
            offset: 0,
        }
    }

    /// Returns the sequence this transfer belongs to.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Returns what kind of content this transfer carries.
    #[must_use]
    pub const fn kind(&self) -> ClipboardContentKind {
        self.kind
    }

    /// Returns the whole payload, before chunking.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Returns the offer that announces this transfer.
    ///
    /// This cannot fail: [`Self::new`] already established that the payload is
    /// neither empty nor larger than the wire's size field, so a transfer that
    /// exists is a transfer that can be announced.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        reason = "new() rejected any length that does not fit u32"
    )]
    pub fn offer(&self) -> ClipboardDataMsg {
        let size = self.bytes.len() as u32;
        ClipboardDataMsg::new(self.sequence, self.kind, size, self.truncated)
    }

    /// Returns the framed chunks that carry the payload.
    ///
    /// Each is a complete binary message beginning with the clipboard frame
    /// type, which is what a peer matches on before it will look at a payload.
    ///
    /// # Errors
    ///
    /// Returns [`ClipboardFramingError`] when the payload cannot be framed.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "new() rejected any length that does not fit u32"
    )]
    pub fn chunks(&self) -> Result<Vec<Vec<u8>>, ClipboardFramingError> {
        let total_size = self.bytes.len() as u32;
        let mut frames = Vec::new();
        let mut offset = 0_usize;
        loop {
            let end = offset.saturating_add(CHUNK_BYTES).min(self.bytes.len());
            let chunk_offset =
                u32::try_from(offset).map_err(|_| ClipboardFramingError::PayloadTooLarge)?;
            let frame = encode_clipboard_chunk(
                ClipboardChunkHeader {
                    kind: self.kind,
                    sequence: self.sequence,
                    total_size,
                    offset: chunk_offset,
                },
                &self.bytes[offset..end],
            )
            .map_err(|_| ClipboardFramingError::ChunkEncoding)?;
            frames.push(frame);
            offset = end;
            if offset >= self.bytes.len() {
                break;
            }
        }
        Ok(frames)
    }
}

#[cfg(test)]
mod transfer_tests {
    use super::*;
    use crate::wire::decode_clipboard_chunk;

    #[test]
    fn the_cursor_sends_the_offer_before_any_chunk() {
        // The receiver allocates from the offer's total size. A chunk that
        // arrives first has nothing to be reassembled into and is dropped, so
        // this ordering is the whole transfer.
        let bytes = vec![b'z'; CHUNK_BYTES + 1];
        let mut cursor = ClipboardTransfer::new(4, ClipboardContentKind::TextUtf8, bytes)
            .expect("new")
            .into_cursor();
        let first = cursor.next_message().expect("first").expect("ok");
        assert!(matches!(first, ClipboardWireMessage::Offer(_)));
        let second = cursor.next_message().expect("second").expect("ok");
        assert!(matches!(second, ClipboardWireMessage::Chunk(_)));
    }

    #[test]
    fn the_cursor_ends_and_stays_ended() {
        // A send loop asks until it is told to stop. A cursor that yields
        // anything after the last chunk would repeat a payload the receiver
        // has already reassembled.
        let mut cursor = ClipboardTransfer::new(5, ClipboardContentKind::TextUtf8, vec![b'a'; 8])
            .expect("new")
            .into_cursor();
        while cursor.next_message().is_some() {}
        assert!(cursor.finished());
        assert!(cursor.next_message().is_none());
        assert!(cursor.next_message().is_none());
    }

    #[test]
    fn the_cursor_and_chunks_produce_the_same_frames() {
        // Two ways to send one transfer. If they ever disagree, whichever host
        // uses the other one is off the wire contract, which is exactly the
        // drift that having two of them invites.
        let bytes = vec![b'q'; CHUNK_BYTES * 2 + 17];
        let transfer =
            ClipboardTransfer::new(6, ClipboardContentKind::ImagePng, bytes).expect("new");
        let expected = transfer.chunks().expect("chunks");
        let mut cursor = transfer.into_cursor();
        let mut actual = Vec::new();
        while let Some(message) = cursor.next_message() {
            if let ClipboardWireMessage::Chunk(frame) = message.expect("ok") {
                actual.push(frame);
            }
        }
        assert_eq!(actual, expected);
    }

    #[test]
    fn a_truncated_payload_says_so_in_its_offer() {
        // A peer that cannot tell a policy truncation from a failed transfer
        // will either warn about nothing or silently accept half a clipboard.
        let transfer = ClipboardTransfer::new(7, ClipboardContentKind::TextUtf8, vec![b'k'; 4])
            .expect("new")
            .with_truncated(true);
        assert!(transfer.offer().truncated);
    }

    #[test]
    fn an_empty_payload_is_refused_at_construction_not_at_framing() {
        // The chunk validator rejects zero-shaped metadata, so an empty
        // transfer cannot be encoded. A host that only found out while framing
        // turned that into a stream error, which ends a session over an empty
        // clipboard. The caller has to decide not to offer it.
        assert_eq!(
            ClipboardTransfer::new(1, ClipboardContentKind::TextUtf8, Vec::new()),
            Err(ClipboardFramingError::EmptyPayload),
        );
    }

    #[test]
    fn a_payload_larger_than_one_chunk_is_split_and_the_offsets_are_contiguous() {
        let bytes = vec![7_u8; CHUNK_BYTES * 2 + 5];
        let transfer =
            ClipboardTransfer::new(9, ClipboardContentKind::TextUtf8, bytes.clone()).expect("new");
        let frames = transfer.chunks().expect("frames");
        assert_eq!(frames.len(), 3, "two full chunks and a remainder");

        // What the reassembler enforces is what this must produce, so feed the
        // frames straight into it rather than asserting on the shape.
        let mut reassembler = ClipboardReassembler::new(bytes.len()).expect("reassembler");
        reassembler.begin(transfer.offer()).expect("begin");
        let mut completed = None;
        for frame in &frames {
            let (header, payload) = decode_clipboard_chunk(frame).expect("decode");
            if let Some(done) = reassembler.push(header, payload).expect("push") {
                completed = Some(done);
            }
        }
        let mut completed = completed.expect("the transfer completes");
        assert_eq!(completed.take_bytes(), bytes);
    }

    #[test]
    fn a_payload_exactly_one_chunk_long_is_not_split() {
        let bytes = vec![3_u8; CHUNK_BYTES];
        let transfer =
            ClipboardTransfer::new(2, ClipboardContentKind::TextUtf8, bytes).expect("new");
        assert_eq!(transfer.chunks().expect("frames").len(), 1);
    }
}
