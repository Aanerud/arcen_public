//! The capenc helper's framed-v2 stdout: encoded access units and cursor
//! events on one ordered pipe.
//!
//! framed-v1 is `u32_be length || access unit`. framed-v2, requested with
//! the [`FRAMED_OUTPUT_V2`] argument, tags every record:
//!
//! ```text
//! u32_be length (kind byte + body) || u8 kind || body
//! ```
//!
//! Kind 1 is an Annex-B/OBU access unit, exactly what framed-v1 carried.
//! Kind 2 is a [`CapencCursorEvent`]. A capture process runs as the desktop's
//! user, so a parent treats every record as untrusted: lengths, kinds and
//! cursor geometry are bounded here before anything is allocated.

use crate::cursor::{CursorImage, MAX_SOURCE_CURSOR_SIDE};

/// capenc argument selecting framed-v2 output.
pub const FRAMED_OUTPUT_V2: &str = "framed-v2";
/// Bytes before a record's kind byte.
pub const RECORD_LENGTH_BYTES: usize = 4;
/// Largest access unit, as for framed-v1.
pub const MAX_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;
const CURSOR_EVENT_VERSION: u8 = 1;
const CURSOR_EVENT_HEADER_BYTES: usize = 4;
const CURSOR_IMAGE_HEADER_BYTES: usize = 16;
/// Largest cursor event body.
pub const MAX_CURSOR_EVENT_BYTES: usize = CURSOR_EVENT_HEADER_BYTES
    + CURSOR_IMAGE_HEADER_BYTES
    + (MAX_SOURCE_CURSOR_SIDE as usize) * (MAX_SOURCE_CURSOR_SIDE as usize) * 4;

/// What a framed-v2 record carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapencRecordKind {
    /// An encoded access unit.
    AccessUnit = 1,
    /// A [`CapencCursorEvent`].
    Cursor = 2,
}

impl CapencRecordKind {
    /// The kind for a wire byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::AccessUnit),
            2 => Some(Self::Cursor),
            _ => None,
        }
    }

    /// Largest body of this kind.
    #[must_use]
    pub const fn max_body_bytes(self) -> usize {
        match self {
            Self::AccessUnit => MAX_ACCESS_UNIT_BYTES,
            Self::Cursor => MAX_CURSOR_EVENT_BYTES,
        }
    }
}

/// The cursor sprite a compositor reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapencCursorSprite {
    /// No new sprite since the last event: keep the last known one.
    Unchanged,
    /// The desktop set an empty cursor (it hides the pointer).
    Empty,
    /// A new sprite.
    Image(CursorImage),
}

/// One change of the cursor on the captured output.
///
/// Sent only when something changed: the cursor entered or left this
/// output (Mutter's cursor id turned nonzero or zero), or its sprite
/// changed. Position-only movement is never sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapencCursorEvent {
    /// Whether the compositor reports the cursor on this output now.
    pub on_output: bool,
    /// The sprite.
    pub sprite: CapencCursorSprite,
}

/// Why a framed-v2 record was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapencRecordError {
    /// Zero length or longer than the record kind allows.
    Length(usize),
    /// An unknown record kind.
    Kind(u8),
    /// A malformed cursor event.
    Cursor(&'static str),
}

impl std::fmt::Display for CapencRecordError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Length(length) => {
                write!(formatter, "framed-v2 record length {length} is invalid")
            }
            Self::Kind(kind) => write!(formatter, "framed-v2 record kind {kind} is unknown"),
            Self::Cursor(reason) => write!(formatter, "framed-v2 cursor event: {reason}"),
        }
    }
}

impl std::error::Error for CapencRecordError {}

/// Validates a record's length prefix before its kind byte and body are
/// read. Returns the body length.
///
/// # Errors
///
/// [`CapencRecordError::Length`] for zero or a length beyond any kind.
pub fn record_body_length(
    length_prefix: [u8; RECORD_LENGTH_BYTES],
) -> Result<usize, CapencRecordError> {
    let length = u32::from_be_bytes(length_prefix) as usize;
    if length < 2 || length > 1 + MAX_ACCESS_UNIT_BYTES.max(MAX_CURSOR_EVENT_BYTES) {
        return Err(CapencRecordError::Length(length));
    }
    Ok(length - 1)
}

/// Validates a record's kind byte against its body length.
///
/// # Errors
///
/// [`CapencRecordError`] for an unknown kind or a body too long for it.
pub fn record_kind(kind: u8, body_length: usize) -> Result<CapencRecordKind, CapencRecordError> {
    let kind = CapencRecordKind::from_byte(kind).ok_or(CapencRecordError::Kind(kind))?;
    if body_length == 0 || body_length > kind.max_body_bytes() {
        return Err(CapencRecordError::Length(body_length + 1));
    }
    Ok(kind)
}

/// Frames one record.
///
/// # Errors
///
/// [`CapencRecordError::Length`] for an empty or oversized body.
pub fn encode_record(kind: CapencRecordKind, body: &[u8]) -> Result<Vec<u8>, CapencRecordError> {
    if body.is_empty() || body.len() > kind.max_body_bytes() {
        return Err(CapencRecordError::Length(body.len() + 1));
    }
    let length =
        u32::try_from(body.len() + 1).map_err(|_| CapencRecordError::Length(body.len()))?;
    let mut out = Vec::with_capacity(RECORD_LENGTH_BYTES + 1 + body.len());
    out.extend_from_slice(&length.to_be_bytes());
    out.push(kind as u8);
    out.extend_from_slice(body);
    Ok(out)
}

/// Writes one record to `writer` without copying its body, then flushes
/// it: the parent reads each record as soon as it is written.
///
/// # Errors
///
/// [`std::io::ErrorKind::InvalidData`] for an empty or oversized body, or
/// the writer's error.
pub fn write_record(
    writer: &mut impl std::io::Write,
    kind: CapencRecordKind,
    body: &[u8],
) -> std::io::Result<()> {
    let invalid = || {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            CapencRecordError::Length(body.len() + 1).to_string(),
        )
    };
    if body.is_empty() || body.len() > kind.max_body_bytes() {
        return Err(invalid());
    }
    let length = u32::try_from(body.len() + 1).map_err(|_| invalid())?;
    writer.write_all(&length.to_be_bytes())?;
    writer.write_all(&[kind as u8])?;
    writer.write_all(body)?;
    writer.flush()
}

impl std::fmt::Display for CapencCursorEvent {
    /// `on_output=<bool> sprite=<unchanged|empty|WxH@x,y>`, for logs.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "on_output={} sprite=", self.on_output)?;
        match &self.sprite {
            CapencCursorSprite::Unchanged => formatter.write_str("unchanged"),
            CapencCursorSprite::Empty => formatter.write_str("empty"),
            CapencCursorSprite::Image(image) => write!(
                formatter,
                "{}x{}@{},{}",
                image.width(),
                image.height(),
                image.hotspot().0,
                image.hotspot().1
            ),
        }
    }
}

/// Encodes a cursor event body.
#[must_use]
pub fn encode_cursor_event(event: &CapencCursorEvent) -> Vec<u8> {
    let sprite = match event.sprite {
        CapencCursorSprite::Unchanged => 0,
        CapencCursorSprite::Empty => 1,
        CapencCursorSprite::Image(_) => 2,
    };
    let mut out = vec![CURSOR_EVENT_VERSION, u8::from(event.on_output), sprite, 0];
    if let CapencCursorSprite::Image(image) = &event.sprite {
        for value in [
            image.width(),
            image.height(),
            image.hotspot().0,
            image.hotspot().1,
        ] {
            out.extend_from_slice(&value.to_be_bytes());
        }
        out.extend_from_slice(image.rgba());
    }
    out
}

/// Decodes a cursor event body.
///
/// # Errors
///
/// [`CapencRecordError::Cursor`] for anything outside the contract.
pub fn decode_cursor_event(body: &[u8]) -> Result<CapencCursorEvent, CapencRecordError> {
    let malformed = CapencRecordError::Cursor;
    if body.len() < CURSOR_EVENT_HEADER_BYTES || body.len() > MAX_CURSOR_EVENT_BYTES {
        return Err(malformed("length"));
    }
    if body[0] != CURSOR_EVENT_VERSION {
        return Err(malformed("version"));
    }
    let on_output = match body[1] {
        0 => false,
        1 => true,
        _ => return Err(malformed("flags")),
    };
    if body[3] != 0 {
        return Err(malformed("reserved byte"));
    }
    let rest = &body[CURSOR_EVENT_HEADER_BYTES..];
    let sprite = match body[2] {
        0 | 1 if !rest.is_empty() => return Err(malformed("trailing bytes")),
        0 => CapencCursorSprite::Unchanged,
        1 => CapencCursorSprite::Empty,
        2 => {
            if rest.len() < CURSOR_IMAGE_HEADER_BYTES {
                return Err(malformed("image header"));
            }
            let field = |at: usize| {
                u32::from_be_bytes([rest[at], rest[at + 1], rest[at + 2], rest[at + 3]])
            };
            let (width, height) = (field(0), field(4));
            let hotspot = (i64::from(field(8)), i64::from(field(12)));
            if width == 0
                || height == 0
                || width > MAX_SOURCE_CURSOR_SIDE
                || height > MAX_SOURCE_CURSOR_SIDE
            {
                return Err(malformed("image size"));
            }
            if hotspot.0 >= i64::from(width) || hotspot.1 >= i64::from(height) {
                return Err(malformed("hotspot"));
            }
            let pixels = &rest[CURSOR_IMAGE_HEADER_BYTES..];
            let image = CursorImage::from_straight_rgba(width, height, hotspot, pixels.to_vec())
                .map_err(|_| malformed("pixels"))?;
            CapencCursorSprite::Image(image)
        }
        _ => return Err(malformed("sprite kind")),
    };
    Ok(CapencCursorEvent { on_output, sprite })
}

#[cfg(test)]
mod write_tests {
    use super::*;

    #[test]
    fn written_records_are_the_encoded_records() {
        let event = CapencCursorEvent {
            on_output: true,
            sprite: CapencCursorSprite::Empty,
        };
        let body = encode_cursor_event(&event);
        let mut written = Vec::new();
        write_record(&mut written, CapencRecordKind::AccessUnit, &[0, 0, 1, 0x65]).unwrap();
        write_record(&mut written, CapencRecordKind::Cursor, &body).unwrap();
        let mut expected = encode_record(CapencRecordKind::AccessUnit, &[0, 0, 1, 0x65]).unwrap();
        expected.extend(encode_record(CapencRecordKind::Cursor, &body).unwrap());
        assert_eq!(written, expected);
        assert!(write_record(&mut written, CapencRecordKind::Cursor, &[]).is_err());
        assert_eq!(event.to_string(), "on_output=true sprite=empty");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> CursorImage {
        CursorImage::from_straight_rgba(3, 2, (2, 1), (0..24).collect()).unwrap()
    }

    #[test]
    fn cursor_events_round_trip() {
        for event in [
            CapencCursorEvent {
                on_output: true,
                sprite: CapencCursorSprite::Image(image()),
            },
            CapencCursorEvent {
                on_output: true,
                sprite: CapencCursorSprite::Empty,
            },
            CapencCursorEvent {
                on_output: false,
                sprite: CapencCursorSprite::Unchanged,
            },
        ] {
            let body = encode_cursor_event(&event);
            assert_eq!(decode_cursor_event(&body).unwrap(), event);
            let record = encode_record(CapencRecordKind::Cursor, &body).unwrap();
            let body_length = record_body_length(record[..4].try_into().unwrap()).unwrap();
            assert_eq!(body_length, body.len());
            assert_eq!(
                record_kind(record[4], body_length).unwrap(),
                CapencRecordKind::Cursor
            );
        }
    }

    #[test]
    fn malformed_records_are_refused_before_allocation() {
        assert!(record_body_length(0u32.to_be_bytes()).is_err());
        assert!(record_body_length(1u32.to_be_bytes()).is_err());
        assert!(record_body_length(u32::MAX.to_be_bytes()).is_err());
        assert_eq!(record_kind(9, 4), Err(CapencRecordError::Kind(9)));
        assert!(record_kind(2, MAX_CURSOR_EVENT_BYTES + 1).is_err());
        assert!(record_kind(1, MAX_CURSOR_EVENT_BYTES + 1).is_ok());
        assert!(encode_record(CapencRecordKind::AccessUnit, &[]).is_err());
    }

    #[test]
    fn malformed_cursor_events_are_refused() {
        let good = encode_cursor_event(&CapencCursorEvent {
            on_output: true,
            sprite: CapencCursorSprite::Image(image()),
        });
        let mut bad = good.clone();
        bad[0] = 2;
        assert!(decode_cursor_event(&bad).is_err());
        bad = good.clone();
        bad[1] = 7;
        assert!(decode_cursor_event(&bad).is_err());
        bad = good.clone();
        bad.pop();
        assert!(decode_cursor_event(&bad).is_err());
        bad = good.clone();
        bad[4 + 8..4 + 12].copy_from_slice(&3u32.to_be_bytes());
        assert!(decode_cursor_event(&bad).is_err(), "hotspot outside");
        bad = good.clone();
        bad[4..8].copy_from_slice(&(MAX_SOURCE_CURSOR_SIDE + 1).to_be_bytes());
        assert!(decode_cursor_event(&bad).is_err());
        let mut empty = encode_cursor_event(&CapencCursorEvent {
            on_output: true,
            sprite: CapencCursorSprite::Empty,
        });
        empty.push(0);
        assert!(decode_cursor_event(&empty).is_err());
    }
}
