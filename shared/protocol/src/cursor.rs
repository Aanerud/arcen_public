// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Host cursor images: the `CursorUpdate` binary record (`FrameType` 0x50).
//!
//! A host whose desktop reports its cursor separately from the video (a
//! Wayland `ScreenCast` stream in `metadata` cursor mode) can show the Deck
//! the exact cursor image rather than one of the [`CursorShapeKind`] names.
//! One record carries the whole current cursor state, so a newer record
//! always supersedes an older one and a sender may keep only the newest.
//!
//! An image travels once: the record carries it inline (PNG) only when the
//! receiver does not hold that `image_id` yet, and afterwards only the id.
//! Both ends keep the same deterministic cache (`arcen_input::cursor`), so
//! the sender knows which ids the receiver holds without acknowledgements;
//! the input/cursor lane is reliable and ordered, and both ends reset the
//! cache when that lane is reopened.
//!
//! Layout, big-endian, [`CURSOR_UPDATE_HEADER_SIZE`] fixed bytes then the
//! PNG:
//!
//! ```text
//! u8  frame_type   0x50
//! u8  version      1
//! u8  flags        bit0 visible, bit1 image inline; other bits zero
//! u8  fallback     CursorShapeKind wire index (older shape vocabulary)
//! u64 sequence     strictly increasing per attachment
//! u32 output       host's output number of the image (0: single output)
//! u32 generation   output generation of the image's pixel space
//! u32 image_id     0 when hidden
//! u16 width, u16 height, u16 hotspot_x, u16 hotspot_y   (stream pixels)
//! u32 png_len      0 unless the image is inline
//! ```
//!
//! A record is legal only to a client that declared
//! [`crate::messages::AuthResponse::cursor_image_v1`] and only while
//! client-drawn (`local`) cursor authority is active.
//!
//! The lane cache itself ([`CursorImageCache`]) is part of this contract,
//! so it lives here: the Deck, a Pier that writes its own input/cursor lane
//! (`arcen_input::cursor::CursorLaneOutbox`) and a Pier that relays an
//! agent's records onto that lane ([`CursorRecordRelay`]) all run the same
//! one.

use std::collections::VecDeque;
use std::num::NonZeroU32;

use crate::messages::CursorShapeKind;
use crate::wire::{FrameType, PNG_MAGIC};

/// Version of the `CursorUpdate` record.
pub const CURSOR_UPDATE_VERSION: u8 = 1;
/// Fixed bytes before the inline PNG.
pub const CURSOR_UPDATE_HEADER_SIZE: usize = 36;
/// Largest width or height of a cursor image on the wire, in pixels.
pub const MAX_CURSOR_SIDE: u16 = 256;
/// Largest inline PNG. Keeps every record inside the input/cursor lane's
/// 64 KiB frame limit with room for the session-record framing.
pub const MAX_CURSOR_PNG_BYTES: usize = 60 * 1024;
/// Largest encoded record.
pub const MAX_CURSOR_UPDATE_BYTES: usize = CURSOR_UPDATE_HEADER_SIZE + MAX_CURSOR_PNG_BYTES;

const FLAG_VISIBLE: u8 = 0x01;
const FLAG_INLINE: u8 = 0x02;
const KNOWN_FLAGS: u8 = FLAG_VISIBLE | FLAG_INLINE;

/// Identifier of one cursor image, unique for the life of the host desktop
/// that assigned it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CursorImageId(NonZeroU32);

impl CursorImageId {
    /// Wraps a nonzero id.
    #[must_use]
    pub const fn new(value: u32) -> Option<Self> {
        match NonZeroU32::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    /// The wire value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0.get()
    }
}

/// A visible cursor: which image, its size and its hotspot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorImageRef {
    /// Image identity.
    pub id: CursorImageId,
    /// Width in stream pixels.
    pub width: u16,
    /// Height in stream pixels.
    pub height: u16,
    /// Hotspot column, inside the image.
    pub hotspot_x: u16,
    /// Hotspot row, inside the image.
    pub hotspot_y: u16,
    /// The PNG, when the receiver does not hold `id` yet.
    pub inline_png: Option<Vec<u8>>,
}

/// What the cursor looks like now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorAppearance {
    /// The desktop shows no cursor (a client set an empty one).
    Hidden,
    /// The desktop shows this image.
    Image(CursorImageRef),
}

/// The whole current cursor state, as one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorUpdate {
    /// Strictly increasing per attachment; a receiver ignores a record whose
    /// sequence is not greater than the last one it applied.
    pub sequence: u64,
    /// The host's number for the output whose pixel space the image is in,
    /// stable for the desktop (0: single output). Not a session monitor id.
    pub output: u32,
    /// Output generation of that pixel space.
    pub generation: u32,
    /// The named shape for receivers that cannot use the image.
    pub fallback: CursorShapeKind,
    /// The cursor.
    pub appearance: CursorAppearance,
}

/// Why a `CursorUpdate` could not be encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorUpdateError {
    /// Shorter than the fixed header.
    Truncated,
    /// The first byte is not `FrameType::CursorUpdate`.
    WrongFrameType,
    /// A version this build does not know.
    UnsupportedVersion(u8),
    /// Reserved flag bits are set.
    ReservedFlags(u8),
    /// A hidden cursor carried image fields, or a visible one had none.
    InconsistentVisibility,
    /// Zero or oversized dimensions.
    Dimensions,
    /// Hotspot outside the image.
    Hotspot,
    /// Inline flag and PNG length disagree, or the PNG is empty, oversized
    /// or not a PNG.
    InlineImage,
    /// The record length does not match its header.
    Length,
}

impl std::fmt::Display for CursorUpdateError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => formatter.write_str("cursor update is truncated"),
            Self::WrongFrameType => formatter.write_str("not a cursor update frame"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported cursor update version {version}")
            }
            Self::ReservedFlags(flags) => {
                write!(formatter, "cursor update has reserved flags {flags:#04x}")
            }
            Self::InconsistentVisibility => {
                formatter.write_str("cursor update visibility disagrees with its image fields")
            }
            Self::Dimensions => formatter.write_str("cursor image dimensions are out of range"),
            Self::Hotspot => formatter.write_str("cursor hotspot lies outside the image"),
            Self::InlineImage => formatter.write_str("cursor update inline image is invalid"),
            Self::Length => formatter.write_str("cursor update length disagrees with its header"),
        }
    }
}

impl std::error::Error for CursorUpdateError {}

/// Stable one-byte wire index of a [`CursorShapeKind`].
#[must_use]
pub const fn shape_wire_index(kind: CursorShapeKind) -> u8 {
    match kind {
        CursorShapeKind::Default => 0,
        CursorShapeKind::Text => 1,
        CursorShapeKind::ResizeNs => 2,
        CursorShapeKind::ResizeEw => 3,
        CursorShapeKind::ResizeNwse => 4,
        CursorShapeKind::ResizeNesw => 5,
        CursorShapeKind::ResizeAll => 6,
        CursorShapeKind::Pointer => 7,
        CursorShapeKind::Crosshair => 8,
        CursorShapeKind::Grab => 9,
        CursorShapeKind::Grabbing => 10,
        CursorShapeKind::ZoomIn => 11,
        CursorShapeKind::ZoomOut => 12,
        CursorShapeKind::Wait => 13,
        CursorShapeKind::Progress => 14,
        CursorShapeKind::Help => 15,
        CursorShapeKind::NotAllowed => 16,
        CursorShapeKind::Hidden => 17,
    }
}

/// The shape for a wire index; an index from a newer host reads as
/// [`CursorShapeKind::Default`], like an unknown JSON shape name.
#[must_use]
pub const fn shape_from_wire_index(index: u8) -> CursorShapeKind {
    match index {
        1 => CursorShapeKind::Text,
        2 => CursorShapeKind::ResizeNs,
        3 => CursorShapeKind::ResizeEw,
        4 => CursorShapeKind::ResizeNwse,
        5 => CursorShapeKind::ResizeNesw,
        6 => CursorShapeKind::ResizeAll,
        7 => CursorShapeKind::Pointer,
        8 => CursorShapeKind::Crosshair,
        9 => CursorShapeKind::Grab,
        10 => CursorShapeKind::Grabbing,
        11 => CursorShapeKind::ZoomIn,
        12 => CursorShapeKind::ZoomOut,
        13 => CursorShapeKind::Wait,
        14 => CursorShapeKind::Progress,
        15 => CursorShapeKind::Help,
        16 => CursorShapeKind::NotAllowed,
        17 => CursorShapeKind::Hidden,
        _ => CursorShapeKind::Default,
    }
}

fn check_image(image: &CursorImageRef) -> Result<(), CursorUpdateError> {
    if image.width == 0
        || image.height == 0
        || image.width > MAX_CURSOR_SIDE
        || image.height > MAX_CURSOR_SIDE
    {
        return Err(CursorUpdateError::Dimensions);
    }
    if image.hotspot_x >= image.width || image.hotspot_y >= image.height {
        return Err(CursorUpdateError::Hotspot);
    }
    if let Some(png) = &image.inline_png {
        if png.is_empty() || png.len() > MAX_CURSOR_PNG_BYTES || !png.starts_with(PNG_MAGIC) {
            return Err(CursorUpdateError::InlineImage);
        }
    }
    Ok(())
}

/// Encodes one record.
///
/// # Errors
///
/// The same checks a receiver applies: dimensions, hotspot and inline PNG.
pub fn encode_cursor_update(update: &CursorUpdate) -> Result<Vec<u8>, CursorUpdateError> {
    let (flags, image_id, width, height, hotspot_x, hotspot_y, png): (
        u8,
        u32,
        u16,
        u16,
        u16,
        u16,
        &[u8],
    ) = match &update.appearance {
        CursorAppearance::Hidden => (0, 0, 0, 0, 0, 0, &[]),
        CursorAppearance::Image(image) => {
            check_image(image)?;
            let inline = image.inline_png.as_deref();
            (
                FLAG_VISIBLE | if inline.is_some() { FLAG_INLINE } else { 0 },
                image.id.get(),
                image.width,
                image.height,
                image.hotspot_x,
                image.hotspot_y,
                inline.unwrap_or_default(),
            )
        }
    };
    let png_len = u32::try_from(png.len()).map_err(|_| CursorUpdateError::InlineImage)?;
    let mut out = Vec::with_capacity(CURSOR_UPDATE_HEADER_SIZE + png.len());
    out.push(FrameType::CursorUpdate as u8);
    out.push(CURSOR_UPDATE_VERSION);
    out.push(flags);
    out.push(shape_wire_index(update.fallback));
    out.extend_from_slice(&update.sequence.to_be_bytes());
    out.extend_from_slice(&update.output.to_be_bytes());
    out.extend_from_slice(&update.generation.to_be_bytes());
    out.extend_from_slice(&image_id.to_be_bytes());
    for value in [width, height, hotspot_x, hotspot_y] {
        out.extend_from_slice(&value.to_be_bytes());
    }
    out.extend_from_slice(&png_len.to_be_bytes());
    out.extend_from_slice(png);
    debug_assert!(out.len() <= MAX_CURSOR_UPDATE_BYTES);
    Ok(out)
}

fn be_u16(bytes: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([bytes[at], bytes[at + 1]])
}

fn be_u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// Decodes one record.
///
/// # Errors
///
/// [`CursorUpdateError`] for anything outside the contract; a receiver
/// drops such a record and keeps its current cursor.
pub fn decode_cursor_update(bytes: &[u8]) -> Result<CursorUpdate, CursorUpdateError> {
    if bytes.len() < CURSOR_UPDATE_HEADER_SIZE {
        return Err(CursorUpdateError::Truncated);
    }
    if bytes[0] != FrameType::CursorUpdate as u8 {
        return Err(CursorUpdateError::WrongFrameType);
    }
    if bytes[1] != CURSOR_UPDATE_VERSION {
        return Err(CursorUpdateError::UnsupportedVersion(bytes[1]));
    }
    let flags = bytes[2];
    if flags & !KNOWN_FLAGS != 0 {
        return Err(CursorUpdateError::ReservedFlags(flags));
    }
    let fallback = shape_from_wire_index(bytes[3]);
    let sequence = u64::from_be_bytes(
        bytes[4..12]
            .try_into()
            .map_err(|_| CursorUpdateError::Truncated)?,
    );
    let output = be_u32(bytes, 12);
    let generation = be_u32(bytes, 16);
    let image_id = be_u32(bytes, 20);
    let width = be_u16(bytes, 24);
    let height = be_u16(bytes, 26);
    let hotspot_x = be_u16(bytes, 28);
    let hotspot_y = be_u16(bytes, 30);
    let png_len = be_u32(bytes, 32) as usize;
    if png_len > MAX_CURSOR_PNG_BYTES {
        return Err(CursorUpdateError::InlineImage);
    }
    if bytes.len() != CURSOR_UPDATE_HEADER_SIZE + png_len {
        return Err(CursorUpdateError::Length);
    }
    let appearance = if flags & FLAG_VISIBLE == 0 {
        if flags & FLAG_INLINE != 0 {
            return Err(CursorUpdateError::InconsistentVisibility);
        }
        if image_id != 0 || width != 0 || height != 0 || hotspot_x != 0 || hotspot_y != 0 {
            return Err(CursorUpdateError::InconsistentVisibility);
        }
        if png_len != 0 {
            return Err(CursorUpdateError::InlineImage);
        }
        CursorAppearance::Hidden
    } else {
        let id = CursorImageId::new(image_id).ok_or(CursorUpdateError::InconsistentVisibility)?;
        let inline = flags & FLAG_INLINE != 0;
        if inline != (png_len != 0) {
            return Err(CursorUpdateError::InlineImage);
        }
        let image = CursorImageRef {
            id,
            width,
            height,
            hotspot_x,
            hotspot_y,
            inline_png: inline.then(|| bytes[CURSOR_UPDATE_HEADER_SIZE..].to_vec()),
        };
        check_image(&image)?;
        CursorAppearance::Image(image)
    };
    Ok(CursorUpdate {
        sequence,
        output,
        generation,
        fallback,
        appearance,
    })
}

/// Images either end holds for one lane.
pub const CURSOR_CACHE_ENTRIES: usize = 32;
/// Decoded bytes either end holds for one lane.
pub const CURSOR_CACHE_BYTES: usize = 2 * 1024 * 1024;

/// A deterministic least-recently-used cache of cursor image ids.
///
/// Each entry weighs `width * height * 4` bytes of the wire image. Inserting
/// evicts the least recently used entries until at most
/// [`CURSOR_CACHE_ENTRIES`] remain and their weight is within
/// [`CURSOR_CACHE_BYTES`]; the newest entry always stays.
#[derive(Debug, Clone)]
pub struct CursorImageCache<V> {
    entries: VecDeque<(CursorImageId, usize, V)>,
    bytes: usize,
}

impl<V> Default for CursorImageCache<V> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }
}

impl<V> CursorImageCache<V> {
    /// The weight of an image of this size.
    #[must_use]
    pub fn weight(width: u16, height: u16) -> usize {
        usize::from(width) * usize::from(height) * 4
    }

    /// Whether `id` is held.
    #[must_use]
    pub fn contains(&self, id: CursorImageId) -> bool {
        self.entries.iter().any(|(entry, _, _)| *entry == id)
    }

    /// The value held for `id`, without marking it used.
    #[must_use]
    pub fn peek(&self, id: CursorImageId) -> Option<&V> {
        self.entries
            .iter()
            .find(|(entry, _, _)| *entry == id)
            .map(|(_, _, value)| value)
    }

    /// Marks `id` most recently used and returns its value.
    pub fn touch(&mut self, id: CursorImageId) -> Option<&V> {
        let index = self.entries.iter().position(|(entry, _, _)| *entry == id)?;
        let entry = self.entries.remove(index)?;
        self.entries.push_back(entry);
        self.entries.back().map(|(_, _, value)| value)
    }

    /// Inserts `id` as most recently used, replacing an existing entry.
    pub fn insert(&mut self, id: CursorImageId, weight: usize, value: V) {
        if let Some(index) = self.entries.iter().position(|(entry, _, _)| *entry == id) {
            if let Some((_, old, _)) = self.entries.remove(index) {
                self.bytes -= old;
            }
        }
        self.entries.push_back((id, weight, value));
        self.bytes += weight;
        while self.entries.len() > 1
            && (self.entries.len() > CURSOR_CACHE_ENTRIES || self.bytes > CURSOR_CACHE_BYTES)
        {
            if let Some((_, weight, _)) = self.entries.pop_front() {
                self.bytes -= weight;
            }
        }
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }

    /// Ids from least to most recently used.
    pub fn ids(&self) -> impl Iterator<Item = CursorImageId> + '_ {
        self.entries.iter().map(|(id, _, _)| *id)
    }

    /// Number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// 64-bit FNV-1a of a PNG: tells two images under one id apart.
fn png_fingerprint(png: &[u8]) -> u64 {
    png.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A relayed record ready for the lane, and what sending it commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedRelayRecord {
    /// The encoded record for the lane.
    pub bytes: Vec<u8>,
    /// Whether the image travels inline.
    pub inline_image: bool,
    sequence: u64,
    image: Option<(CursorImageId, usize, u64)>,
}

/// Puts a session agent's cursor records on the Deck's input/cursor lane.
///
/// A Pier whose session runs in a separate agent process (Windows, macOS)
/// relays that agent's messages onto the lanes; the agent never sees the
/// lane, so it cannot know which images the Deck holds. The agent therefore
/// sends every visible record with its image inline, and the relay keeps
/// the lane's side of the cache contract: it keeps only the newest state,
/// strips an image the Deck already holds, numbers records for the lane,
/// commits only what the lane accepted, and sends the current state again
/// (images included) on a reopened lane.
///
/// The relay also remembers a fingerprint of every image it mirrors, so an
/// agent that reuses an id for a different image (a restarted agent behind
/// the same relay) gets the new image inlined rather than the Deck showing
/// the old one.
#[derive(Debug, Default)]
pub struct CursorRecordRelay {
    mirror: CursorImageCache<u64>,
    latest: Option<CursorUpdate>,
    latest_sent: bool,
    lane: Option<u64>,
    sequence: u64,
    last_agent_sequence: Option<u64>,
    refused: u64,
}

impl CursorRecordRelay {
    /// Takes one record from the agent. A record older than the newest one
    /// taken is ignored.
    ///
    /// # Errors
    ///
    /// [`CursorUpdateError`] for a malformed record, or
    /// [`CursorUpdateError::InlineImage`] for a visible one without its
    /// image; it is dropped and the current state kept.
    pub fn offer(&mut self, record: &[u8]) -> Result<(), CursorUpdateError> {
        let update = match decode_cursor_update(record) {
            Ok(update) => update,
            Err(error) => {
                self.refused += 1;
                return Err(error);
            }
        };
        if let CursorAppearance::Image(image) = &update.appearance {
            if image.inline_png.is_none() {
                self.refused += 1;
                return Err(CursorUpdateError::InlineImage);
            }
        }
        if self
            .last_agent_sequence
            .is_some_and(|last| update.sequence <= last)
        {
            return Ok(());
        }
        self.last_agent_sequence = Some(update.sequence);
        self.latest = Some(update);
        self.latest_sent = false;
        Ok(())
    }

    /// Whether a state waits for the lane.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.latest.is_some() && !self.latest_sent
    }

    /// Whether the relay ever took a record (it then follows lane reopens).
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.latest.is_some()
    }

    /// Records the agent sent that were dropped as malformed.
    #[must_use]
    pub const fn refused(&self) -> u64 {
        self.refused
    }

    /// Builds the record for the lane of generation `lane_generation`, or
    /// `None` when the Deck already shows the newest state on that lane. A
    /// new lane generation empties the mirror: the Deck reset its cache.
    pub fn prepare(&mut self, lane_generation: u64) -> Option<PreparedRelayRecord> {
        if self.lane != Some(lane_generation) {
            self.lane = Some(lane_generation);
            self.mirror.clear();
            self.latest_sent = false;
        }
        if self.latest_sent {
            return None;
        }
        let mut update = self.latest.clone()?;
        let sequence = self.sequence + 1;
        update.sequence = sequence;
        let mut image = None;
        let mut inline_image = false;
        if let CursorAppearance::Image(reference) = &mut update.appearance {
            let weight = CursorImageCache::<u64>::weight(reference.width, reference.height);
            let fingerprint = reference.inline_png.as_deref().map_or(0, png_fingerprint);
            if self.mirror.peek(reference.id) == Some(&fingerprint) {
                reference.inline_png = None;
            } else {
                inline_image = true;
            }
            image = Some((reference.id, weight, fingerprint));
        }
        let bytes = encode_cursor_update(&update).ok()?;
        Some(PreparedRelayRecord {
            bytes,
            inline_image,
            sequence,
            image,
        })
    }

    /// Commits a record the lane accepted.
    pub fn commit(&mut self, prepared: PreparedRelayRecord) {
        self.sequence = prepared.sequence;
        if let Some((id, weight, fingerprint)) = prepared.image {
            if prepared.inline_image || self.mirror.touch(id).is_none() {
                self.mirror.insert(id, weight, fingerprint);
            }
        }
        self.latest_sent = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(len: usize) -> Vec<u8> {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.resize(len.max(PNG_MAGIC.len()), 7);
        bytes
    }

    fn visible(inline: Option<Vec<u8>>) -> CursorUpdate {
        CursorUpdate {
            sequence: 9,
            output: 1,
            generation: 3,
            fallback: CursorShapeKind::Text,
            appearance: CursorAppearance::Image(CursorImageRef {
                id: CursorImageId::new(42).unwrap(),
                width: 24,
                height: 32,
                hotspot_x: 4,
                hotspot_y: 31,
                inline_png: inline,
            }),
        }
    }

    #[test]
    fn records_round_trip() {
        for update in [
            visible(None),
            visible(Some(png(100))),
            CursorUpdate {
                sequence: u64::MAX,
                output: 0,
                generation: 0,
                fallback: CursorShapeKind::Hidden,
                appearance: CursorAppearance::Hidden,
            },
        ] {
            let bytes = encode_cursor_update(&update).unwrap();
            assert_eq!(bytes[0], 0x50);
            assert_eq!(decode_cursor_update(&bytes).unwrap(), update);
        }
        let header_only = encode_cursor_update(&visible(None)).unwrap();
        assert_eq!(header_only.len(), CURSOR_UPDATE_HEADER_SIZE);
    }

    #[test]
    fn the_largest_record_fits_the_input_lane() {
        let bytes = encode_cursor_update(&visible(Some(png(MAX_CURSOR_PNG_BYTES)))).unwrap();
        assert_eq!(bytes.len(), MAX_CURSOR_UPDATE_BYTES);
        const { assert!(MAX_CURSOR_UPDATE_BYTES + 8 < 64 * 1024) };
    }

    #[test]
    fn invalid_images_are_refused_by_both_ends() {
        let mut update = visible(Some(png(MAX_CURSOR_PNG_BYTES + 1)));
        assert_eq!(
            encode_cursor_update(&update),
            Err(CursorUpdateError::InlineImage)
        );
        update = visible(Some(vec![1, 2, 3]));
        assert_eq!(
            encode_cursor_update(&update),
            Err(CursorUpdateError::InlineImage)
        );
        for (width, height, hx, hy, error) in [
            (0, 4, 0, 0, CursorUpdateError::Dimensions),
            (MAX_CURSOR_SIDE + 1, 4, 0, 0, CursorUpdateError::Dimensions),
            (8, 8, 8, 0, CursorUpdateError::Hotspot),
            (8, 8, 0, 8, CursorUpdateError::Hotspot),
        ] {
            let mut update = visible(None);
            if let CursorAppearance::Image(image) = &mut update.appearance {
                image.width = width;
                image.height = height;
                image.hotspot_x = hx;
                image.hotspot_y = hy;
            }
            assert_eq!(encode_cursor_update(&update), Err(error));
        }
    }

    #[test]
    fn malformed_records_are_refused() {
        let good = encode_cursor_update(&visible(Some(png(64)))).unwrap();
        assert_eq!(
            decode_cursor_update(&good[..CURSOR_UPDATE_HEADER_SIZE - 1]),
            Err(CursorUpdateError::Truncated)
        );
        let mut bad = good.clone();
        bad[0] = FrameType::Clipboard as u8;
        assert_eq!(
            decode_cursor_update(&bad),
            Err(CursorUpdateError::WrongFrameType)
        );
        bad = good.clone();
        bad[1] = 2;
        assert_eq!(
            decode_cursor_update(&bad),
            Err(CursorUpdateError::UnsupportedVersion(2))
        );
        bad = good.clone();
        bad[2] |= 0x80;
        assert_eq!(
            decode_cursor_update(&bad),
            Err(CursorUpdateError::ReservedFlags(0x83))
        );
        bad = good.clone();
        bad.push(0);
        assert_eq!(decode_cursor_update(&bad), Err(CursorUpdateError::Length));
        // Visible without an id.
        bad = good.clone();
        bad[20..24].copy_from_slice(&0u32.to_be_bytes());
        assert_eq!(
            decode_cursor_update(&bad),
            Err(CursorUpdateError::InconsistentVisibility)
        );
        // Inline flag without bytes.
        let mut reference = encode_cursor_update(&visible(None)).unwrap();
        reference[2] |= FLAG_INLINE;
        assert_eq!(
            decode_cursor_update(&reference),
            Err(CursorUpdateError::InlineImage)
        );
        // Hidden carrying image fields.
        let mut hidden = encode_cursor_update(&CursorUpdate {
            sequence: 1,
            output: 0,
            generation: 0,
            fallback: CursorShapeKind::Hidden,
            appearance: CursorAppearance::Hidden,
        })
        .unwrap();
        hidden[24] = 1;
        assert_eq!(
            decode_cursor_update(&hidden),
            Err(CursorUpdateError::InconsistentVisibility)
        );
    }

    #[test]
    fn shape_indices_are_stable_and_forward_compatible() {
        for index in 0..=17u8 {
            assert_eq!(shape_wire_index(shape_from_wire_index(index)), index);
        }
        assert_eq!(shape_from_wire_index(200), CursorShapeKind::Default);
    }

    fn agent_record(sequence: u64, id: u32, png_fill: u8) -> Vec<u8> {
        let mut png = png(64);
        png[40] = png_fill;
        let mut update = visible(Some(png));
        update.sequence = sequence;
        if let CursorAppearance::Image(image) = &mut update.appearance {
            image.id = CursorImageId::new(id).unwrap();
        }
        encode_cursor_update(&update).unwrap()
    }

    /// What a Deck holds: applies relayed records like `DeckCursorState`.
    #[derive(Default)]
    struct Deck {
        cache: CursorImageCache<Vec<u8>>,
        shown: Option<Vec<u8>>,
        unknown: u32,
    }

    impl Deck {
        fn apply(&mut self, bytes: &[u8]) {
            let update = decode_cursor_update(bytes).unwrap();
            self.shown = match update.appearance {
                CursorAppearance::Hidden => None,
                CursorAppearance::Image(image) => {
                    let weight = CursorImageCache::<()>::weight(image.width, image.height);
                    if let Some(png) = image.inline_png {
                        self.cache.insert(image.id, weight, png.clone());
                        Some(png)
                    } else if let Some(png) = self.cache.touch(image.id) {
                        Some(png.clone())
                    } else {
                        self.unknown += 1;
                        None
                    }
                }
            };
        }
    }

    fn relay_once(relay: &mut CursorRecordRelay, lane: u64, deck: &mut Deck) -> Option<bool> {
        let prepared = relay.prepare(lane)?;
        deck.apply(&prepared.bytes);
        let inline = prepared.inline_image;
        relay.commit(prepared);
        Some(inline)
    }

    #[test]
    fn relay_sends_each_image_once_per_lane_and_renumbers() {
        let mut relay = CursorRecordRelay::default();
        let mut deck = Deck::default();
        assert!(relay.prepare(1).is_none());
        relay.offer(&agent_record(100, 5, 1)).unwrap();
        assert_eq!(relay_once(&mut relay, 1, &mut deck), Some(true));
        assert_eq!(relay_once(&mut relay, 1, &mut deck), None);
        relay.offer(&agent_record(101, 6, 2)).unwrap();
        assert_eq!(relay_once(&mut relay, 1, &mut deck), Some(true));
        relay.offer(&agent_record(102, 5, 1)).unwrap();
        let prepared = relay.prepare(1).unwrap();
        assert!(!prepared.inline_image);
        assert_eq!(prepared.bytes.len(), CURSOR_UPDATE_HEADER_SIZE);
        assert_eq!(decode_cursor_update(&prepared.bytes).unwrap().sequence, 3);
        deck.apply(&prepared.bytes);
        relay.commit(prepared);
        assert_eq!(deck.unknown, 0);
        assert_eq!(deck.shown.as_ref().map(|png| png[40]), Some(1));
        // A reopened lane: the Deck reset its cache; the state goes again,
        // with its image.
        deck.cache.clear();
        assert_eq!(relay_once(&mut relay, 2, &mut deck), Some(true));
        assert_eq!(deck.unknown, 0);
    }

    #[test]
    fn relay_keeps_the_newest_state_until_the_lane_accepts_it() {
        let mut relay = CursorRecordRelay::default();
        let mut deck = Deck::default();
        relay.offer(&agent_record(1, 1, 1)).unwrap();
        // The lane refused it: nothing is committed.
        assert!(relay.prepare(7).unwrap().inline_image);
        relay.offer(&agent_record(2, 2, 2)).unwrap();
        // A stale agent record is ignored.
        relay.offer(&agent_record(1, 1, 1)).unwrap();
        assert_eq!(relay_once(&mut relay, 7, &mut deck), Some(true));
        assert_eq!(deck.shown.as_ref().map(|png| png[40]), Some(2));
        assert!(!relay.is_pending());
    }

    #[test]
    fn relay_inlines_an_image_whose_id_was_reused() {
        let mut relay = CursorRecordRelay::default();
        let mut deck = Deck::default();
        relay.offer(&agent_record(1, 3, 1)).unwrap();
        relay_once(&mut relay, 1, &mut deck);
        relay.offer(&agent_record(2, 3, 9)).unwrap();
        assert_eq!(relay_once(&mut relay, 1, &mut deck), Some(true));
        assert_eq!(deck.shown.as_ref().map(|png| png[40]), Some(9));
    }

    #[test]
    fn relay_refuses_records_it_cannot_complete() {
        let mut relay = CursorRecordRelay::default();
        // A visible reference without its image.
        let reference = encode_cursor_update(&visible(None)).unwrap();
        assert_eq!(relay.offer(&reference), Err(CursorUpdateError::InlineImage));
        assert_eq!(relay.offer(&[0x50]), Err(CursorUpdateError::Truncated));
        let mut oversized = agent_record(1, 1, 1);
        oversized.extend(std::iter::repeat_n(0, MAX_CURSOR_PNG_BYTES));
        assert!(relay.offer(&oversized).is_err());
        assert_eq!(relay.refused(), 3);
        assert!(!relay.is_active());
        let hidden = encode_cursor_update(&CursorUpdate {
            sequence: 4,
            output: 0,
            generation: 0,
            fallback: CursorShapeKind::Hidden,
            appearance: CursorAppearance::Hidden,
        })
        .unwrap();
        relay.offer(&hidden).unwrap();
        let prepared = relay.prepare(1).unwrap();
        assert!(!prepared.inline_image);
        assert_eq!(
            decode_cursor_update(&prepared.bytes).unwrap().appearance,
            CursorAppearance::Hidden
        );
    }

    #[test]
    fn decoding_never_panics_on_arbitrary_bytes() {
        let good = encode_cursor_update(&visible(Some(png(80)))).unwrap();
        let mut state = 0x9e37_79b9_u32;
        for round in 0..20_000u32 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let mut bytes = good.clone();
            let len = (state as usize) % (good.len() + 8);
            bytes.resize(len, (state >> 8) as u8);
            if !bytes.is_empty() {
                let at = (state as usize / 7) % bytes.len();
                bytes[at] ^= (round as u8) | 1;
            }
            if let Ok(update) = decode_cursor_update(&bytes) {
                if let CursorAppearance::Image(image) = update.appearance {
                    assert!(image.width >= 1 && image.width <= MAX_CURSOR_SIDE);
                    assert!(image.hotspot_x < image.width && image.hotspot_y < image.height);
                }
            }
        }
    }
}
