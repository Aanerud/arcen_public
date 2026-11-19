// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
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
//! Kind 2 is a [`CapencCursorEvent`]. Kind 5, only when the parent passed
//! [`CURSOR_POSITION_ARG`], is a [`CapencCursorPosition`]: where the pointer
//! is on the output, sent whenever it moved. A capture process runs as the desktop's
//! user, so a parent treats every record as untrusted: lengths, kinds and
//! cursor geometry are bounded here before anything is allocated.

use crate::cursor::{CursorImage, MAX_SOURCE_CURSOR_SIDE};

/// capenc output index that captures the whole X screen (every monitor, as
/// the desktop arranges them) instead of one output: a Linux Pier joining
/// the workstation's own session serves its monitors as one picture.
pub const WHOLE_SCREEN_OUTPUT_INDEX: u32 = u32::MAX;
/// The largest side NVENC encodes for `codec` (a capenc codec token):
/// 4096 for H.264, 8192 for H.265 and AV1.
#[must_use]
pub fn whole_screen_max_dimension(codec: &str) -> u32 {
    if codec == "h264" { 4096 } else { 8192 }
}

/// The frame a whole X screen of `width`x`height` is captured at so the
/// encoder takes it: unchanged when it fits `max` on both sides, otherwise
/// scaled down with its aspect ratio kept, to even sides.
#[must_use]
pub fn fit_whole_screen(width: u32, height: u32, max: u32) -> (u32, u32) {
    if width <= max && height <= max {
        return (width, height);
    }
    let longest = u64::from(width.max(height));
    let scale = |side: u32| -> u32 {
        let scaled = u64::from(side) * u64::from(max) / longest;
        u32::try_from(scaled).unwrap_or(max).max(2) & !1
    };
    (scale(width), scale(height))
}

/// capenc argument selecting framed-v2 output.
pub const FRAMED_OUTPUT_V2: &str = "framed-v2";
/// capenc argument asking a framed-v2 cursor stream for pointer positions.
pub const CURSOR_POSITION_ARG: &str = "cursor-position";
const CURSOR_POSITION_VERSION: u8 = 1;
/// Body of a [`CapencRecordKind::CursorPosition`] record.
pub const CURSOR_POSITION_BYTES: usize = 12;
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
    /// A lossless-refinement record for the **video** lane
    /// (`RefineFence`/`RefineBarrier`), written immediately before the
    /// access unit it belongs to (ADR 0016). Only with `refine=on`.
    RefineVideo = 3,
    /// A lossless-refinement record for the **refine** lane
    /// (`RefineTiles`/`RefineDigest`). Only with `refine=on`.
    RefineLane = 4,
    /// A [`CapencCursorPosition`]. Only with [`CURSOR_POSITION_ARG`].
    CursorPosition = 5,
    /// A complete canvas record (`arcen_protocol::canvas`, frame type
    /// `0x0E`, ADR 0028) for the video lane, in order with the others.
    /// Only with `canvas=` (`crate::canvas::serve::CanvasCapencArgs`).
    Canvas = 6,
}

/// Largest refinement record body on either lane.
pub const MAX_REFINE_RECORD_BYTES: usize = 8 * 1024 * 1024;

impl CapencRecordKind {
    /// The kind for a wire byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::AccessUnit),
            2 => Some(Self::Cursor),
            3 => Some(Self::RefineVideo),
            4 => Some(Self::RefineLane),
            5 => Some(Self::CursorPosition),
            6 => Some(Self::Canvas),
            _ => None,
        }
    }

    /// Largest body of this kind.
    #[must_use]
    pub const fn max_body_bytes(self) -> usize {
        match self {
            Self::AccessUnit => MAX_ACCESS_UNIT_BYTES,
            Self::Cursor => MAX_CURSOR_EVENT_BYTES,
            Self::RefineVideo | Self::RefineLane => MAX_REFINE_RECORD_BYTES,
            Self::CursorPosition => CURSOR_POSITION_BYTES,
            Self::Canvas => arcen_protocol::canvas::MAX_CANVAS_FRAME_BYTES,
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

/// Where the pointer is on the captured output.
///
/// Layout: `u8 version 1, u8 flags (bit0 on_output), u16 reserved 0,
/// i32_be x, i32_be y`: the hotspot in the output's stream pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CapencCursorPosition {
    /// Whether the compositor reports the cursor on this output.
    pub on_output: bool,
    /// Hotspot column.
    pub x: i32,
    /// Hotspot row.
    pub y: i32,
}

/// Encodes a cursor position body.
#[must_use]
pub fn encode_cursor_position(position: &CapencCursorPosition) -> [u8; CURSOR_POSITION_BYTES] {
    let mut out = [0u8; CURSOR_POSITION_BYTES];
    out[0] = CURSOR_POSITION_VERSION;
    out[1] = u8::from(position.on_output);
    out[4..8].copy_from_slice(&position.x.to_be_bytes());
    out[8..12].copy_from_slice(&position.y.to_be_bytes());
    out
}

/// Decodes a cursor position body.
///
/// # Errors
///
/// [`CapencRecordError::Cursor`] for anything outside the contract.
pub fn decode_cursor_position(body: &[u8]) -> Result<CapencCursorPosition, CapencRecordError> {
    let malformed = CapencRecordError::Cursor;
    let body: &[u8; CURSOR_POSITION_BYTES] =
        body.try_into().map_err(|_| malformed("position length"))?;
    if body[0] != CURSOR_POSITION_VERSION {
        return Err(malformed("position version"));
    }
    let on_output = match body[1] {
        0 => false,
        1 => true,
        _ => return Err(malformed("position flags")),
    };
    if body[2] != 0 || body[3] != 0 {
        return Err(malformed("position reserved bytes"));
    }
    Ok(CapencCursorPosition {
        on_output,
        x: i32::from_be_bytes([body[4], body[5], body[6], body[7]]),
        y: i32::from_be_bytes([body[8], body[9], body[10], body[11]]),
    })
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

/// The capenc output path a process was dispatched to, for
/// [`check_output_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapencRoute {
    /// Linux Xorg depth-30 `XShm` NVENC streaming: framed-v2 and lossless
    /// refinement.
    XorgWide,
    /// Any other Linux Xorg streaming path (`NvFBC`, software, RGB10 pipe,
    /// self-test): framed-v1 or raw only.
    XorgOther,
    /// Linux Wayland: framed-v2 from every composition; lossless
    /// refinement is accepted, and only the wide Grading/HDR composition
    /// produces it (the others state why it is off).
    Wayland,
    /// Windows: framed-v1 or raw only.
    Windows,
    /// Windows FP16 scRGB NVENC streaming (Grading and HDR): framed-v2 and
    /// lossless refinement.
    WindowsWide,
    /// The shared software loop on Xorg and Windows
    /// (`crate::canvas::CanvasEncodeLoop`): framed-v2 and the canvas.
    Software,
}

impl CapencRoute {
    /// Whether the route writes framed-v2 records.
    #[must_use]
    pub const fn writes_framed_v2(self) -> bool {
        matches!(
            self,
            Self::XorgWide | Self::Wayland | Self::WindowsWide | Self::Software
        )
    }

    /// Whether the route serves a canvas (`canvas=`, ADR 0028): the
    /// software loop wherever it runs.
    #[must_use]
    pub const fn honours_canvas(self) -> bool {
        matches!(self, Self::Wayland | Self::Software)
    }

    /// Whether the route accepts `refine=on` (ADR 0016). A route that
    /// accepts it still refines only a contract the shared plan grants.
    #[must_use]
    pub const fn honours_refinement(self) -> bool {
        matches!(self, Self::XorgWide | Self::Wayland | Self::WindowsWide)
    }
}

/// Refuses output arguments the dispatched path cannot honour, before any
/// capture resource exists: `framed-v2` where the path writes framed-v1 or
/// raw Annex-B (the parent would misparse every byte), and `refine=on`
/// where no composition of the path refines (ADR 0016).
///
/// # Errors
///
/// A readable message naming the refused argument.
pub fn check_output_request(args: &[String], route: CapencRoute) -> Result<(), String> {
    let framed_v2 = args.iter().any(|arg| arg == FRAMED_OUTPUT_V2);
    let refine = args
        .iter()
        .any(|arg| arg == crate::refine::CAPENC_REFINE_ON);
    if refine && !route.honours_refinement() {
        return Err(format!(
            "{} is not honoured by the {route:?} path",
            crate::refine::CAPENC_REFINE_ON
        ));
    }
    let canvas = args
        .iter()
        .any(|arg| arg.starts_with(crate::canvas::serve::CAPENC_CANVAS_ARG_PREFIX));
    if canvas && !route.honours_canvas() {
        return Err(format!(
            "{} is not honoured by the {route:?} path",
            crate::canvas::serve::CAPENC_CANVAS_ARG_PREFIX
        ));
    }
    if canvas && !framed_v2 {
        return Err(format!(
            "{} needs {FRAMED_OUTPUT_V2}",
            crate::canvas::serve::CAPENC_CANVAS_ARG_PREFIX
        ));
    }
    if framed_v2 && !route.writes_framed_v2() {
        return Err(format!(
            "{FRAMED_OUTPUT_V2} is not written by the {route:?} path"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod route_tests {
    use super::*;

    #[test]
    fn a_whole_screen_wider_than_the_codec_is_scaled_to_fit() {
        // Two monitors side by side, 2560x1600 and 1920x1200: too wide for
        // H.264, not for H.265.
        assert_eq!(whole_screen_max_dimension("h264"), 4096);
        assert_eq!(whole_screen_max_dimension("h265"), 8192);
        assert_eq!(fit_whole_screen(4480, 1600, 4096), (4096, 1462));
        assert_eq!(fit_whole_screen(4480, 1600, 8192), (4480, 1600));
        assert_eq!(fit_whole_screen(1600, 5120, 4096), (1280, 4096));
    }

    #[test]
    fn output_arguments_a_path_cannot_honour_are_refused() {
        let args = |list: &[&str]| {
            list.iter()
                .map(|arg| (*arg).to_string())
                .collect::<Vec<_>>()
        };
        let v2_refine = args(&["framed-v2", "refine=on"]);
        for route in [
            CapencRoute::XorgWide,
            CapencRoute::Wayland,
            CapencRoute::WindowsWide,
        ] {
            assert_eq!(check_output_request(&v2_refine, route), Ok(()), "{route:?}");
        }
        for route in [CapencRoute::XorgOther, CapencRoute::Windows] {
            assert!(
                check_output_request(&v2_refine, route).is_err(),
                "{route:?}"
            );
        }
        let v2 = args(&["framed-v2"]);
        assert_eq!(check_output_request(&v2, CapencRoute::Wayland), Ok(()));
        let position = CapencCursorPosition {
            on_output: true,
            x: -3,
            y: 2159,
        };
        let body = encode_cursor_position(&position);
        assert_eq!(decode_cursor_position(&body), Ok(position));
        let framed = encode_record(CapencRecordKind::CursorPosition, &body).unwrap();
        assert_eq!(
            record_kind(framed[4], framed.len() - RECORD_LENGTH_BYTES - 1),
            Ok(CapencRecordKind::CursorPosition)
        );
        assert!(record_kind(5, CURSOR_POSITION_BYTES + 1).is_err());
        for (at, value) in [(0usize, 2u8), (1, 2), (2, 1), (3, 1)] {
            let mut bad = body;
            bad[at] = value;
            assert!(decode_cursor_position(&bad).is_err(), "byte {at}");
        }
        assert!(decode_cursor_position(&body[..11]).is_err());
        assert!(check_output_request(&v2, CapencRoute::XorgOther).is_err());
        assert!(check_output_request(&v2, CapencRoute::Windows).is_err());
        let v1 = args(&["framed-v1"]);
        for route in [
            CapencRoute::XorgWide,
            CapencRoute::XorgOther,
            CapencRoute::Wayland,
            CapencRoute::Windows,
        ] {
            assert_eq!(check_output_request(&v1, route), Ok(()));
        }
    }

    #[test]
    fn only_the_software_loop_serves_a_canvas_and_only_framed_v2() {
        let canvas = vec![
            FRAMED_OUTPUT_V2.to_owned(),
            format!(
                "{}v1:16384:2:104857600:25600:0:0:0:50",
                crate::canvas::serve::CAPENC_CANVAS_ARG_PREFIX
            ),
        ];
        for route in [CapencRoute::Software, CapencRoute::Wayland] {
            assert_eq!(check_output_request(&canvas, route), Ok(()), "{route:?}");
            assert!(
                check_output_request(&canvas[1..], route).is_err(),
                "{route:?}"
            );
        }
        for route in [
            CapencRoute::XorgWide,
            CapencRoute::XorgOther,
            CapencRoute::Windows,
            CapencRoute::WindowsWide,
        ] {
            assert!(check_output_request(&canvas, route).is_err(), "{route:?}");
        }
        let record = encode_record(CapencRecordKind::Canvas, &[0x0E, 1, 2]).unwrap();
        assert_eq!(record_kind(record[4], 3), Ok(CapencRecordKind::Canvas));
        assert!(record_kind(6, arcen_protocol::canvas::MAX_CANVAS_FRAME_BYTES + 1).is_err());
    }
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
