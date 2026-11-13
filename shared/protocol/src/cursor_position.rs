// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host pointer position: the `CursorPosition` binary record (`FrameType`
//! 0x51).
//!
//! In client-drawn (`local`) cursor authority the Deck draws the pointer
//! where its own pointing device is. When the host pointer moves for a
//! reason the Deck did not cause — a USB-bridged tablet driven by the
//! host's own driver, an application warping the pointer, a device plugged
//! into the host — the Deck's pointer would stay behind. The host then
//! reports where its pointer is, and the Deck draws the cursor there until
//! its own pointing device moves again. A host never reports positions that
//! merely echo the Deck's own pointer input
//! (`arcen_input::host_pointer::HostPointerReporter`).
//!
//! One record carries the whole current position, so a newer record always
//! supersedes an older one and a sender keeps only the newest.
//!
//! Layout, big-endian, [`CURSOR_POSITION_SIZE`] bytes:
//!
//! ```text
//! u8  frame_type     0x51
//! u8  version        1
//! u8  flags          bit0 on_stream; other bits zero
//! u8  reserved       0
//! u64 sequence       strictly increasing per input/cursor lane
//! u32 output         host's output number (as in `CursorUpdate`; 0: single output)
//! u32 generation     output generation of the pixel space
//! u16 monitor_id     video monitor id of the stream showing it (0: primary/legacy)
//! u16 reserved       0
//! i32 x, i32 y       the pointer's hotspot, in that stream's pixels
//! u16 space_width    width of the pixel space x was measured in
//! u16 space_height   height of the pixel space y was measured in
//! ```
//!
//! `on_stream` is clear when the host pointer is on no streamed output (a
//! host-only screen area); the Deck then draws no host-positioned cursor.
//! The receiver scales by `x / space_width`, so a record measured before a
//! resize lands where it was, never on another pixel grid.
//!
//! A record is legal only to a client that declared
//! [`crate::messages::AuthResponse::cursor_position_v1`], only while
//! client-drawn cursor authority is active, and only on the input/cursor
//! lane. The sequence restarts when that lane is reopened.

use crate::wire::FrameType;

/// Version of the `CursorPosition` record.
pub const CURSOR_POSITION_VERSION: u8 = 1;
/// Encoded size of a record.
pub const CURSOR_POSITION_SIZE: usize = 36;

const FLAG_ON_STREAM: u8 = 0x01;
const KNOWN_FLAGS: u8 = FLAG_ON_STREAM;

/// Where the host pointer is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorPosition {
    /// Strictly increasing per input/cursor lane; a receiver ignores a
    /// record whose sequence is not greater than the last one it applied.
    pub sequence: u64,
    /// Whether the pointer is on a streamed output.
    pub on_stream: bool,
    /// The host's number for the output (as in `CursorUpdate`).
    pub output: u32,
    /// Output generation of the pixel space.
    pub generation: u32,
    /// Video monitor id of the stream that shows this pixel space (0: the
    /// primary or only stream).
    pub monitor_id: u16,
    /// Hotspot column in stream pixels.
    pub x: i32,
    /// Hotspot row in stream pixels.
    pub y: i32,
    /// Width of the pixel space `x` is in.
    pub space_width: u16,
    /// Height of the pixel space `y` is in.
    pub space_height: u16,
}

/// Why a `CursorPosition` could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorPositionError {
    /// Not exactly [`CURSOR_POSITION_SIZE`] bytes.
    Length,
    /// The first byte is not `FrameType::CursorPosition`.
    WrongFrameType,
    /// A version this build does not know.
    UnsupportedVersion(u8),
    /// Reserved flag bits or bytes are set.
    Reserved,
    /// A zero sequence or an empty pixel space.
    Invalid,
}

impl std::fmt::Display for CursorPositionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Length => formatter.write_str("cursor position has the wrong length"),
            Self::WrongFrameType => formatter.write_str("not a cursor position frame"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported cursor position version {version}")
            }
            Self::Reserved => formatter.write_str("cursor position has reserved bits set"),
            Self::Invalid => {
                formatter.write_str("cursor position has a zero sequence or empty pixel space")
            }
        }
    }
}

impl std::error::Error for CursorPositionError {}

impl CursorPosition {
    /// The position as a fraction of its pixel space, `(0, 0)` top-left and
    /// `(1, 1)` bottom-right; `None` when the pointer is off the streams.
    #[must_use]
    pub fn fraction(&self) -> Option<(f64, f64)> {
        if !self.on_stream || self.space_width == 0 || self.space_height == 0 {
            return None;
        }
        Some((
            f64::from(self.x) / f64::from(self.space_width),
            f64::from(self.y) / f64::from(self.space_height),
        ))
    }
}

/// Encodes one record.
///
/// # Errors
///
/// [`CursorPositionError::Invalid`] for a zero sequence or an empty pixel
/// space, which a receiver would refuse.
pub fn encode_cursor_position(
    position: &CursorPosition,
) -> Result<[u8; CURSOR_POSITION_SIZE], CursorPositionError> {
    if position.sequence == 0 || position.space_width == 0 || position.space_height == 0 {
        return Err(CursorPositionError::Invalid);
    }
    let mut out = [0u8; CURSOR_POSITION_SIZE];
    out[0] = FrameType::CursorPosition as u8;
    out[1] = CURSOR_POSITION_VERSION;
    out[2] = if position.on_stream {
        FLAG_ON_STREAM
    } else {
        0
    };
    out[4..12].copy_from_slice(&position.sequence.to_be_bytes());
    out[12..16].copy_from_slice(&position.output.to_be_bytes());
    out[16..20].copy_from_slice(&position.generation.to_be_bytes());
    out[20..22].copy_from_slice(&position.monitor_id.to_be_bytes());
    out[24..28].copy_from_slice(&position.x.to_be_bytes());
    out[28..32].copy_from_slice(&position.y.to_be_bytes());
    out[32..34].copy_from_slice(&position.space_width.to_be_bytes());
    out[34..36].copy_from_slice(&position.space_height.to_be_bytes());
    Ok(out)
}

/// Decodes one record.
///
/// # Errors
///
/// [`CursorPositionError`] for anything outside the contract; a receiver
/// drops such a record and keeps what it shows.
pub fn decode_cursor_position(bytes: &[u8]) -> Result<CursorPosition, CursorPositionError> {
    let bytes: &[u8; CURSOR_POSITION_SIZE] =
        bytes.try_into().map_err(|_| CursorPositionError::Length)?;
    if bytes[0] != FrameType::CursorPosition as u8 {
        return Err(CursorPositionError::WrongFrameType);
    }
    if bytes[1] != CURSOR_POSITION_VERSION {
        return Err(CursorPositionError::UnsupportedVersion(bytes[1]));
    }
    if bytes[2] & !KNOWN_FLAGS != 0 || bytes[3] != 0 || bytes[22] != 0 || bytes[23] != 0 {
        return Err(CursorPositionError::Reserved);
    }
    let u16_at = |at: usize| u16::from_be_bytes([bytes[at], bytes[at + 1]]);
    let u32_at =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    let mut sequence = [0u8; 8];
    sequence.copy_from_slice(&bytes[4..12]);
    let position = CursorPosition {
        sequence: u64::from_be_bytes(sequence),
        on_stream: bytes[2] & FLAG_ON_STREAM != 0,
        output: u32_at(12),
        generation: u32_at(16),
        monitor_id: u16_at(20),
        x: i32::from_be_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]),
        y: i32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]]),
        space_width: u16_at(32),
        space_height: u16_at(34),
    };
    if position.sequence == 0 || position.space_width == 0 || position.space_height == 0 {
        return Err(CursorPositionError::Invalid);
    }
    Ok(position)
}

/// A split Pier's relay of host pointer positions from its agent onto the
/// input/cursor lane: newest wins, and the lane's own sequence (the agent's
/// is that of its local socket, not of the lane) restarts at 1 on every lane
/// the relay writes to, as the receiver expects of a reopened lane.
#[derive(Debug, Default)]
pub struct CursorPositionRelay {
    pending: Option<CursorPosition>,
    lane: Option<u64>,
    sequence: u64,
    refused: u64,
}

/// A record ready for one lane, and what writing it commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreparedCursorPosition {
    /// The record, renumbered for the lane.
    pub bytes: [u8; CURSOR_POSITION_SIZE],
    lane: u64,
    sequence: u64,
}

impl CursorPositionRelay {
    /// Takes one record from the agent; it replaces any still unsent.
    ///
    /// # Errors
    ///
    /// [`CursorPositionError`] for a malformed record, which is dropped.
    pub fn offer(&mut self, record: &[u8]) -> Result<(), CursorPositionError> {
        match decode_cursor_position(record) {
            Ok(position) => {
                self.pending = Some(position);
                Ok(())
            }
            Err(error) => {
                self.refused += 1;
                Err(error)
            }
        }
    }

    /// Whether a record waits for the lane.
    #[must_use]
    pub const fn is_pending(&self) -> bool {
        self.pending.is_some()
    }

    /// The waiting record renumbered for the lane of generation `lane`.
    #[must_use]
    pub fn prepare(&self, lane: u64) -> Option<PreparedCursorPosition> {
        let position = self.pending?;
        let sequence = if self.lane == Some(lane) {
            self.sequence.checked_add(1)?
        } else {
            1
        };
        let bytes = encode_cursor_position(&CursorPosition {
            sequence,
            ..position
        })
        .ok()?;
        Some(PreparedCursorPosition {
            bytes,
            lane,
            sequence,
        })
    }

    /// The lane accepted `prepared`.
    pub fn commit(&mut self, prepared: &PreparedCursorPosition) {
        self.lane = Some(prepared.lane);
        self.sequence = prepared.sequence;
        self.pending = None;
    }

    /// Records refused as malformed.
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CursorPosition {
        CursorPosition {
            sequence: 7,
            on_stream: true,
            output: 2,
            generation: 3,
            monitor_id: 4,
            x: -5,
            y: 1079,
            space_width: 1920,
            space_height: 1080,
        }
    }

    #[test]
    fn records_round_trip() {
        let bytes = encode_cursor_position(&sample()).unwrap();
        assert_eq!(bytes[0], 0x51);
        assert_eq!(decode_cursor_position(&bytes).unwrap(), sample());
        let off = CursorPosition {
            on_stream: false,
            ..sample()
        };
        let decoded = decode_cursor_position(&encode_cursor_position(&off).unwrap()).unwrap();
        assert_eq!(decoded, off);
        assert_eq!(decoded.fraction(), None);
        let (fx, fy) = sample().fraction().unwrap();
        assert!(fx < 0.0 && (fy - 1079.0 / 1080.0).abs() < 1e-12);
    }

    #[test]
    fn malformed_records_are_refused() {
        let good = encode_cursor_position(&sample()).unwrap();
        assert_eq!(
            decode_cursor_position(&good[..35]),
            Err(CursorPositionError::Length)
        );
        let mut longer = good.to_vec();
        longer.push(0);
        assert_eq!(
            decode_cursor_position(&longer),
            Err(CursorPositionError::Length)
        );
        let mut bad = good;
        bad[0] = FrameType::CursorUpdate as u8;
        assert_eq!(
            decode_cursor_position(&bad),
            Err(CursorPositionError::WrongFrameType)
        );
        let mut bad = good;
        bad[1] = 2;
        assert_eq!(
            decode_cursor_position(&bad),
            Err(CursorPositionError::UnsupportedVersion(2))
        );
        for at in [2usize, 3, 22, 23] {
            let mut bad = good;
            bad[at] |= 0x02;
            assert_eq!(
                decode_cursor_position(&bad),
                Err(CursorPositionError::Reserved),
                "byte {at}"
            );
        }
        let mut bad = good;
        bad[4..12].fill(0);
        assert_eq!(
            decode_cursor_position(&bad),
            Err(CursorPositionError::Invalid)
        );
        let mut bad = good;
        bad[32..34].fill(0);
        assert_eq!(
            decode_cursor_position(&bad),
            Err(CursorPositionError::Invalid)
        );
        assert_eq!(
            encode_cursor_position(&CursorPosition {
                sequence: 0,
                ..sample()
            }),
            Err(CursorPositionError::Invalid)
        );
    }

    #[test]
    fn decoding_never_panics_on_arbitrary_bytes() {
        let good = encode_cursor_position(&sample()).unwrap();
        for at in 0..CURSOR_POSITION_SIZE {
            for value in [0u8, 1, 0x51, 0x7f, 0x80, 0xff] {
                let mut bytes = good;
                bytes[at] = value;
                let _ = decode_cursor_position(&bytes);
            }
        }
        for length in 0..64 {
            let _ = decode_cursor_position(&vec![0x51; length]);
        }
    }

    #[test]
    fn a_relay_renumbers_positions_for_each_lane_and_keeps_only_the_newest() {
        let mut relay = CursorPositionRelay::default();
        assert!(relay.prepare(7).is_none());
        let record = |sequence, x| {
            encode_cursor_position(&CursorPosition {
                sequence,
                x,
                ..sample()
            })
            .unwrap()
        };
        relay.offer(&record(40, 10)).unwrap();
        relay.offer(&record(41, 20)).unwrap();
        let first = relay.prepare(7).unwrap();
        let decoded = decode_cursor_position(&first.bytes).unwrap();
        assert_eq!((decoded.sequence, decoded.x), (1, 20));
        // Not committed (the lane was full): the same record again.
        assert_eq!(relay.prepare(7), Some(first));
        relay.commit(&first);
        assert!(!relay.is_pending());
        relay.offer(&record(42, 30)).unwrap();
        let second = relay.prepare(7).unwrap();
        assert_eq!(decode_cursor_position(&second.bytes).unwrap().sequence, 2);
        // A reopened lane starts again at 1.
        let reopened = relay.prepare(8).unwrap();
        assert_eq!(decode_cursor_position(&reopened.bytes).unwrap().sequence, 1);
        relay.commit(&reopened);
        assert_eq!(relay.offer(&[0x51; 3]), Err(CursorPositionError::Length));
        assert_eq!(relay.refused(), 1);
        assert!(!relay.is_pending());
    }
}
