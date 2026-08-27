//! Bounded clipboard offer reassembly.

use crate::messages::{ClipboardContentKind, ClipboardDataMsg, CLIPBOARD_DATA};
use crate::wire::{ClipboardChunkHeader, CHUNK_BYTES, HARD_MAX_CLIPBOARD_BYTES};
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
