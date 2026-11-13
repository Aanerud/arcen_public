// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Lossless refinement records (ADR 0016) — DRAFT, not negotiated by any
//! product yet.
//!
//! Four binary records: [`RefineFence`] and [`RefineBarrier`] travel on the
//! video lane (ordered with the access units they describe), [`RefineTile`]
//! and [`RefineDigest`] on the lowest-priority refine lane. Headers are
//! big-endian. Every decoder here checks lengths and limits with checked
//! arithmetic **before** it allocates anything sized by the peer.
//!
//! The records are codec-free: a tile's payload is opaque bytes whose
//! expanded size the header fixes; the codec adapters live elsewhere.

use crate::wire::FrameType;
use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::num::NonZeroU32;

/// Record format version carried in byte 1 of every refinement record.
pub const REFINE_RECORD_VERSION: u8 = 1;
/// Bytes of the common prefix (type, version, monitor, topology generation,
/// stream epoch, refinement generation).
pub const REFINE_PREFIX_BYTES: usize = 24;
/// Bytes of a [`RefineTile`] header before its payload.
pub const REFINE_TILE_HEADER_BYTES: usize = 48;
/// Bytes of a [`RefineBarrier`].
pub const REFINE_BARRIER_BYTES: usize = 40;
/// Fixed bytes of a [`RefineFence`] before its runs.
pub const REFINE_FENCE_FIXED_BYTES: usize = 40;
/// Fixed bytes of a [`RefineDigest`] before its runs.
pub const REFINE_DIGEST_FIXED_BYTES: usize = 56;
/// Bytes of one `(start, len)` run.
pub const REFINE_RUN_BYTES: usize = 8;
/// Side of a refinement block (Keel's grid).
pub const REFINE_BLOCK: u16 = 16;
/// Largest tile width in pixels.
pub const MAX_TILE_WIDTH: u16 = 256;
/// Largest tile height in pixels.
pub const MAX_TILE_HEIGHT: u16 = 64;
/// Largest expanded (decoded) tile: three planes of `u16` codes.
pub const MAX_TILE_EXPANDED_BYTES: usize = 98_304;
/// Largest compressed tile payload.
pub const MAX_TILE_PAYLOAD_BYTES: usize = 128 * 1024;
/// Largest number of blocks a monitor grid may have (16384×16384 pixels).
pub const MAX_GRID_BLOCKS: u32 = 1024 * 1024;

/// Codec id of `delta-zstd1` (ADR 0016 §6).
pub const CODEC_DELTA_ZSTD1: u8 = 1;
/// Capability token of `delta-zstd1`.
pub const CODEC_DELTA_ZSTD1_TOKEN: &str = "delta-zstd1";

/// Why a refinement record was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefineRecordError {
    /// Shorter than its fixed header.
    Short,
    /// Wrong frame type byte for the decoder used.
    WrongType(u8),
    /// Unknown record version.
    Version(u8),
    /// A zero refinement generation.
    ZeroGeneration,
    /// Bytes after the declared end of the record.
    TrailingBytes,
    /// Declared payload length differs from the bytes present.
    PayloadLength,
    /// Payload above [`MAX_TILE_PAYLOAD_BYTES`].
    PayloadTooLarge,
    /// Tile geometry breaks the grid alignment or the size limits.
    Geometry,
    /// Expanded size above [`MAX_TILE_EXPANDED_BYTES`].
    ExpandedTooLarge,
    /// Unknown domain, depth, codec or reason value.
    Value,
    /// Runs are empty, overlapping, unordered or outside the grid.
    Runs,
    /// Block count is zero or above [`MAX_GRID_BLOCKS`].
    BlockCount,
}

impl RefineRecordError {
    /// Stable token for telemetry.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Short => "short",
            Self::WrongType(_) => "wrong_type",
            Self::Version(_) => "version",
            Self::ZeroGeneration => "zero_generation",
            Self::TrailingBytes => "trailing_bytes",
            Self::PayloadLength => "payload_length",
            Self::PayloadTooLarge => "payload_too_large",
            Self::Geometry => "geometry",
            Self::ExpandedTooLarge => "expanded_too_large",
            Self::Value => "value",
            Self::Runs => "runs",
            Self::BlockCount => "block_count",
        }
    }
}

impl Display for RefineRecordError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "refinement record refused: {}", self.token())
    }
}

impl std::error::Error for RefineRecordError {}

/// Identity every refinement record carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RefineScope {
    /// Session monitor id (as in the region video header).
    pub monitor_id: u16,
    /// Applied topology generation.
    pub topology_generation: u64,
    /// The monitor's encoder stream epoch.
    pub stream_epoch: u64,
    /// Refinement generation (ADR 0016 R3).
    pub generation: NonZeroU32,
}

/// Tile domain (ADR 0016 R1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefineDomain {
    /// Full-range transfer-coded R′G′B′ codes, written directly.
    Rgb,
    /// Y′CbCr 4:4:4 codes, through the presenter's own matrix.
    YCbCr,
}

impl RefineDomain {
    /// Wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Rgb => 1,
            Self::YCbCr => 2,
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Rgb),
            2 => Some(Self::YCbCr),
            _ => None,
        }
    }

    /// Capability token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Rgb => "rgb",
            Self::YCbCr => "ycbcr",
        }
    }
}

/// The colour contract a generation refines in. Values are the wire bytes;
/// `arcen-media` maps them to its colour vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RefineContractBytes {
    /// [`RefineDomain::wire`].
    pub domain: u8,
    /// Bit depth (10 in v1; 12 reserved).
    pub depth: u8,
    /// Transfer characteristic id.
    pub transfer: u8,
    /// Colour primaries id.
    pub primaries: u8,
    /// Matrix id (Y′CbCr only; 0 for RGB).
    pub matrix: u8,
    /// Range id.
    pub range: u8,
    /// Chroma sampling (2 = 4:4:4, the only refinable value in v1).
    pub chroma: u8,
}

/// Why a generation started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BarrierReason {
    /// First generation of a monitor.
    Start,
    /// Decoder recovery or a recovery keyframe.
    Recovery,
    /// Geometry or topology changed.
    Geometry,
    /// Colour contract changed.
    Contract,
    /// The Deck asked for a reset.
    ResetRequest,
    /// The Deck re-enabled refinement for the monitor.
    Reenable,
}

impl BarrierReason {
    /// Wire value.
    #[must_use]
    pub const fn wire(self) -> u8 {
        match self {
            Self::Start => 1,
            Self::Recovery => 2,
            Self::Geometry => 3,
            Self::Contract => 4,
            Self::ResetRequest => 5,
            Self::Reenable => 6,
        }
    }

    /// Parses a wire value.
    #[must_use]
    pub const fn from_wire(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Start),
            2 => Some(Self::Recovery),
            3 => Some(Self::Geometry),
            4 => Some(Self::Contract),
            5 => Some(Self::ResetRequest),
            6 => Some(Self::Reenable),
            _ => None,
        }
    }
}

/// Ascending, non-overlapping, non-empty `(start, len)` block runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockRuns(Vec<(u32, u32)>);

impl BlockRuns {
    /// Validates runs against a grid of `block_count` blocks.
    ///
    /// # Errors
    ///
    /// [`RefineRecordError::Runs`].
    pub fn new(runs: Vec<(u32, u32)>, block_count: u32) -> Result<Self, RefineRecordError> {
        let mut next = 0_u32;
        for (index, &(start, len)) in runs.iter().enumerate() {
            let end = start.checked_add(len).ok_or(RefineRecordError::Runs)?;
            if len == 0 || end > block_count || (index > 0 && start < next) {
                return Err(RefineRecordError::Runs);
            }
            next = end;
        }
        Ok(Self(runs))
    }

    /// Builds runs from ascending block indices (duplicates are merged).
    #[must_use]
    pub fn from_sorted_indices(indices: impl IntoIterator<Item = u32>) -> Self {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for index in indices {
            match runs.last_mut() {
                Some((start, len)) if *start + *len == index => *len += 1,
                Some((start, len)) if index < *start + *len => {}
                _ => runs.push((index, 1)),
            }
        }
        Self(runs)
    }

    /// The runs.
    #[must_use]
    pub fn runs(&self) -> &[(u32, u32)] {
        &self.0
    }

    /// Every covered block index, ascending.
    pub fn indices(&self) -> impl Iterator<Item = u32> + '_ {
        self.0.iter().flat_map(|&(start, len)| start..start + len)
    }

    /// Number of covered blocks.
    #[must_use]
    pub fn count(&self) -> u64 {
        self.0.iter().map(|&(_, len)| u64::from(len)).sum()
    }

    /// Whether no block is covered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// `0x0A`: precedes every access unit of a monitor while refinement is
/// granted; the access unit's source fence and the blocks it invalidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefineFence {
    /// Identity.
    pub scope: RefineScope,
    /// Source fence of the following access unit.
    pub fence: u64,
    /// Blocks in the monitor's grid.
    pub block_count: u32,
    /// Blocks this access unit changes (union of every capture since the
    /// previous access unit).
    pub invalidated: BlockRuns,
}

/// `0x0D`: starts a refinement generation, in video order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineBarrier {
    /// Identity, with the new generation.
    pub scope: RefineScope,
    /// Why.
    pub reason: BarrierReason,
    /// The generation's colour contract.
    pub contract: RefineContractBytes,
    /// Source width in pixels.
    pub source_width: u32,
    /// Source height in pixels.
    pub source_height: u32,
}

/// `0x0B` header: one lossless rectangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefineTileHeader {
    /// Identity.
    pub scope: RefineScope,
    /// Fence of the access unit whose source the tile was taken from.
    pub valid_from_fence: u64,
    /// Domain.
    pub domain: RefineDomain,
    /// Bit depth of the codes.
    pub depth: u8,
    /// Codec id ([`CODEC_DELTA_ZSTD1`]).
    pub codec: u8,
    /// Left edge in source pixels (multiple of 16).
    pub x: u16,
    /// Top edge in source pixels (multiple of 16).
    pub y: u16,
    /// Width in pixels.
    pub width: u16,
    /// Height in pixels.
    pub height: u16,
    /// Payload bytes following the header.
    pub payload_len: u32,
}

impl RefineTileHeader {
    /// Expanded (decoded) bytes: `width × height × 3 planes × 2 bytes`,
    /// checked.
    ///
    /// # Errors
    ///
    /// [`RefineRecordError::ExpandedTooLarge`].
    pub fn expanded_bytes(&self) -> Result<usize, RefineRecordError> {
        usize::from(self.width)
            .checked_mul(usize::from(self.height))
            .and_then(|pixels| pixels.checked_mul(6))
            .filter(|bytes| *bytes <= MAX_TILE_EXPANDED_BYTES)
            .ok_or(RefineRecordError::ExpandedTooLarge)
    }

    /// Checks the geometry rules that do not need the source size:
    /// alignment, nonzero size and the per-tile limits. The source bounds
    /// are checked by the state owner, which knows the geometry.
    ///
    /// # Errors
    ///
    /// [`RefineRecordError::Geometry`] or
    /// [`RefineRecordError::ExpandedTooLarge`].
    pub fn check_geometry(&self) -> Result<(), RefineRecordError> {
        if self.x % REFINE_BLOCK != 0
            || self.y % REFINE_BLOCK != 0
            || self.width == 0
            || self.height == 0
            || self.width > MAX_TILE_WIDTH
            || self.height > MAX_TILE_HEIGHT
        {
            return Err(RefineRecordError::Geometry);
        }
        self.expanded_bytes().map(|_| ())
    }
}

/// `0x0C`: canonical digest of the covered blocks at a fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefineDigest {
    /// Identity.
    pub scope: RefineScope,
    /// Fence the digest describes.
    pub fence: u64,
    /// Canonical digest v1 (ADR 0016 R4), XXH3-128 little-endian bytes.
    pub digest: [u8; 16],
    /// Blocks in the monitor's grid.
    pub block_count: u32,
    /// Covered blocks.
    pub coverage: BlockRuns,
}

fn put_prefix(out: &mut Vec<u8>, frame_type: FrameType, scope: &RefineScope) {
    out.push(frame_type as u8);
    out.push(REFINE_RECORD_VERSION);
    out.extend_from_slice(&scope.monitor_id.to_be_bytes());
    out.extend_from_slice(&scope.topology_generation.to_be_bytes());
    out.extend_from_slice(&scope.stream_epoch.to_be_bytes());
    out.extend_from_slice(&scope.generation.get().to_be_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take<const N: usize>(&mut self) -> Result<[u8; N], RefineRecordError> {
        let end = self.at.checked_add(N).ok_or(RefineRecordError::Short)?;
        let slice = self
            .bytes
            .get(self.at..end)
            .ok_or(RefineRecordError::Short)?;
        self.at = end;
        let mut out = [0_u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }
    fn u8(&mut self) -> Result<u8, RefineRecordError> {
        Ok(self.take::<1>()?[0])
    }
    fn u16(&mut self) -> Result<u16, RefineRecordError> {
        Ok(u16::from_be_bytes(self.take()?))
    }
    fn u32(&mut self) -> Result<u32, RefineRecordError> {
        Ok(u32::from_be_bytes(self.take()?))
    }
    fn u64(&mut self) -> Result<u64, RefineRecordError> {
        Ok(u64::from_be_bytes(self.take()?))
    }
    fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.at..).unwrap_or_default()
    }
}

fn read_prefix(
    reader: &mut Reader<'_>,
    expected: FrameType,
) -> Result<RefineScope, RefineRecordError> {
    let frame_type = reader.u8()?;
    if frame_type != expected as u8 {
        return Err(RefineRecordError::WrongType(frame_type));
    }
    let version = reader.u8()?;
    if version != REFINE_RECORD_VERSION {
        return Err(RefineRecordError::Version(version));
    }
    let monitor_id = reader.u16()?;
    let topology_generation = reader.u64()?;
    let stream_epoch = reader.u64()?;
    let generation = NonZeroU32::new(reader.u32()?).ok_or(RefineRecordError::ZeroGeneration)?;
    Ok(RefineScope {
        monitor_id,
        topology_generation,
        stream_epoch,
        generation,
    })
}

fn put_runs(out: &mut Vec<u8>, runs: &BlockRuns) {
    out.extend_from_slice(
        &u32::try_from(runs.0.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes(),
    );
    for &(start, len) in &runs.0 {
        out.extend_from_slice(&start.to_be_bytes());
        out.extend_from_slice(&len.to_be_bytes());
    }
}

fn read_runs(reader: &mut Reader<'_>, block_count: u32) -> Result<BlockRuns, RefineRecordError> {
    let count = reader.u32()?;
    // Bound before allocating: the record must hold every declared run.
    let needed = usize::try_from(count)
        .ok()
        .and_then(|count| count.checked_mul(REFINE_RUN_BYTES))
        .ok_or(RefineRecordError::Runs)?;
    if needed != reader.rest().len() {
        return Err(if needed > reader.rest().len() {
            RefineRecordError::Short
        } else {
            RefineRecordError::TrailingBytes
        });
    }
    if count > block_count {
        return Err(RefineRecordError::Runs);
    }
    let mut runs = Vec::with_capacity(count as usize);
    for _ in 0..count {
        runs.push((reader.u32()?, reader.u32()?));
    }
    BlockRuns::new(runs, block_count)
}

fn check_block_count(block_count: u32) -> Result<u32, RefineRecordError> {
    if block_count == 0 || block_count > MAX_GRID_BLOCKS {
        Err(RefineRecordError::BlockCount)
    } else {
        Ok(block_count)
    }
}

impl RefineFence {
    /// Encodes the record.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            REFINE_FENCE_FIXED_BYTES + self.invalidated.0.len() * REFINE_RUN_BYTES,
        );
        put_prefix(&mut out, FrameType::RefineFence, &self.scope);
        out.extend_from_slice(&self.fence.to_be_bytes());
        out.extend_from_slice(&self.block_count.to_be_bytes());
        put_runs(&mut out, &self.invalidated);
        out
    }

    /// Decodes and validates the record.
    ///
    /// # Errors
    ///
    /// [`RefineRecordError`].
    pub fn decode(bytes: &[u8]) -> Result<Self, RefineRecordError> {
        let mut reader = Reader { bytes, at: 0 };
        let scope = read_prefix(&mut reader, FrameType::RefineFence)?;
        let fence = reader.u64()?;
        let block_count = check_block_count(reader.u32()?)?;
        let invalidated = read_runs(&mut reader, block_count)?;
        Ok(Self {
            scope,
            fence,
            block_count,
            invalidated,
        })
    }
}

impl RefineBarrier {
    /// Encodes the record.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(REFINE_BARRIER_BYTES);
        put_prefix(&mut out, FrameType::RefineBarrier, &self.scope);
        out.push(self.reason.wire());
        let c = self.contract;
        out.extend_from_slice(&[
            c.domain,
            c.depth,
            c.transfer,
            c.primaries,
            c.matrix,
            c.range,
            c.chroma,
        ]);
        out.extend_from_slice(&self.source_width.to_be_bytes());
        out.extend_from_slice(&self.source_height.to_be_bytes());
        out
    }

    /// Decodes and validates the record.
    ///
    /// # Errors
    ///
    /// [`RefineRecordError`].
    pub fn decode(bytes: &[u8]) -> Result<Self, RefineRecordError> {
        match bytes.len().cmp(&REFINE_BARRIER_BYTES) {
            std::cmp::Ordering::Less => return Err(RefineRecordError::Short),
            std::cmp::Ordering::Greater => return Err(RefineRecordError::TrailingBytes),
            std::cmp::Ordering::Equal => {}
        }
        let mut reader = Reader { bytes, at: 0 };
        let scope = read_prefix(&mut reader, FrameType::RefineBarrier)?;
        let reason = BarrierReason::from_wire(reader.u8()?).ok_or(RefineRecordError::Value)?;
        let raw: [u8; 7] = reader.take()?;
        let contract = RefineContractBytes {
            domain: raw[0],
            depth: raw[1],
            transfer: raw[2],
            primaries: raw[3],
            matrix: raw[4],
            range: raw[5],
            chroma: raw[6],
        };
        if RefineDomain::from_wire(contract.domain).is_none()
            || !matches!(contract.depth, 8 | 10 | 12)
        {
            return Err(RefineRecordError::Value);
        }
        let source_width = reader.u32()?;
        let source_height = reader.u32()?;
        if source_width == 0
            || source_height == 0
            || source_width > 16_384
            || source_height > 16_384
        {
            return Err(RefineRecordError::Geometry);
        }
        Ok(Self {
            scope,
            reason,
            contract,
            source_width,
            source_height,
        })
    }
}

impl RefineTileHeader {
    /// Encodes the header followed by `payload`.
    #[must_use]
    pub fn encode_with(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(REFINE_TILE_HEADER_BYTES + payload.len());
        put_prefix(&mut out, FrameType::RefineTiles, &self.scope);
        out.extend_from_slice(&self.valid_from_fence.to_be_bytes());
        out.extend_from_slice(&[self.domain.wire(), self.depth, self.codec, 0]);
        out.extend_from_slice(&self.x.to_be_bytes());
        out.extend_from_slice(&self.y.to_be_bytes());
        out.extend_from_slice(&self.width.to_be_bytes());
        out.extend_from_slice(&self.height.to_be_bytes());
        out.extend_from_slice(
            &u32::try_from(payload.len())
                .unwrap_or(u32::MAX)
                .to_be_bytes(),
        );
        out.extend_from_slice(payload);
        out
    }

    /// Decodes a tile record into its header and a borrowed payload. Every
    /// limit is checked here, before any allocation sized by the peer.
    ///
    /// # Errors
    ///
    /// [`RefineRecordError`].
    pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), RefineRecordError> {
        let mut reader = Reader { bytes, at: 0 };
        let scope = read_prefix(&mut reader, FrameType::RefineTiles)?;
        let valid_from_fence = reader.u64()?;
        let domain = RefineDomain::from_wire(reader.u8()?).ok_or(RefineRecordError::Value)?;
        let depth = reader.u8()?;
        let codec = reader.u8()?;
        let reserved = reader.u8()?;
        if !matches!(depth, 8 | 10 | 12) || codec != CODEC_DELTA_ZSTD1 || reserved != 0 {
            return Err(RefineRecordError::Value);
        }
        let header = Self {
            scope,
            valid_from_fence,
            domain,
            depth,
            codec,
            x: reader.u16()?,
            y: reader.u16()?,
            width: reader.u16()?,
            height: reader.u16()?,
            payload_len: reader.u32()?,
        };
        header.check_geometry()?;
        let payload = reader.rest();
        if usize::try_from(header.payload_len).map_or(true, |len| len > MAX_TILE_PAYLOAD_BYTES) {
            return Err(RefineRecordError::PayloadTooLarge);
        }
        if payload.len() != header.payload_len as usize {
            return Err(RefineRecordError::PayloadLength);
        }
        Ok((header, payload))
    }
}

impl RefineDigest {
    /// Encodes the record.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            REFINE_DIGEST_FIXED_BYTES + self.coverage.0.len() * REFINE_RUN_BYTES,
        );
        put_prefix(&mut out, FrameType::RefineDigest, &self.scope);
        out.extend_from_slice(&self.fence.to_be_bytes());
        out.extend_from_slice(&self.digest);
        out.extend_from_slice(&self.block_count.to_be_bytes());
        put_runs(&mut out, &self.coverage);
        out
    }

    /// Decodes and validates the record.
    ///
    /// # Errors
    ///
    /// [`RefineRecordError`].
    pub fn decode(bytes: &[u8]) -> Result<Self, RefineRecordError> {
        let mut reader = Reader { bytes, at: 0 };
        let scope = read_prefix(&mut reader, FrameType::RefineDigest)?;
        let fence = reader.u64()?;
        let digest = reader.take::<16>()?;
        let block_count = check_block_count(reader.u32()?)?;
        let coverage = read_runs(&mut reader, block_count)?;
        Ok(Self {
            scope,
            fence,
            digest,
            block_count,
            coverage,
        })
    }
}

/// Deck capability (draft; carried in `ClientHelloMsg` when products adopt
/// it): the domains and codecs the Deck can apply.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LosslessRefineCapability {
    /// Domain tokens ([`RefineDomain::token`]).
    #[serde(default)]
    pub domains: Vec<String>,
    /// Codec tokens ([`CODEC_DELTA_ZSTD1_TOKEN`]).
    #[serde(default)]
    pub codecs: Vec<String>,
}

impl LosslessRefineCapability {
    /// Whether a Pier that refines in `domain` with `delta-zstd1` may grant
    /// the refine lane to this Deck.
    #[must_use]
    pub fn accepts(&self, domain: RefineDomain) -> bool {
        self.domains.iter().any(|token| token == domain.token())
            && self
                .codecs
                .iter()
                .any(|token| token == CODEC_DELTA_ZSTD1_TOKEN)
    }
}

/// Deck → Pier control requests (draft).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RefineControlMsg {
    /// Forget what was sent for the monitor; start a new generation.
    RefineReset {
        /// Monitor.
        monitor_id: u16,
        /// The epoch the Deck holds.
        stream_epoch: u64,
        /// Stable reason token.
        reason: String,
    },
    /// Enable or disable refinement for one monitor.
    RefineControl {
        /// Monitor.
        monitor_id: u16,
        /// Whether the Deck wants tiles for it.
        enabled: bool,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> RefineScope {
        RefineScope {
            monitor_id: 1,
            topology_generation: 7,
            stream_epoch: 3,
            generation: NonZeroU32::new(2).unwrap(),
        }
    }

    fn tile(width: u16, height: u16) -> RefineTileHeader {
        RefineTileHeader {
            scope: scope(),
            valid_from_fence: 9,
            domain: RefineDomain::Rgb,
            depth: 10,
            codec: CODEC_DELTA_ZSTD1,
            x: 32,
            y: 16,
            width,
            height,
            payload_len: 3,
        }
    }

    #[test]
    fn records_round_trip() {
        let fence = RefineFence {
            scope: scope(),
            fence: 42,
            block_count: 8160,
            invalidated: BlockRuns::from_sorted_indices([3, 4, 5, 9, 100]),
        };
        assert_eq!(RefineFence::decode(&fence.encode()), Ok(fence.clone()));
        let barrier = RefineBarrier {
            scope: scope(),
            reason: BarrierReason::Recovery,
            contract: RefineContractBytes {
                domain: 1,
                depth: 10,
                transfer: 1,
                primaries: 1,
                matrix: 0,
                range: 1,
                chroma: 2,
            },
            source_width: 1920,
            source_height: 1080,
        };
        let bytes = barrier.encode();
        assert_eq!(bytes.len(), REFINE_BARRIER_BYTES);
        assert_eq!(RefineBarrier::decode(&bytes), Ok(barrier));
        let header = tile(256, 64);
        let bytes = header.encode_with(&[1, 2, 3]);
        assert_eq!(&bytes[..1], &[0x0B]);
        let (decoded, payload) = RefineTileHeader::decode(&bytes).unwrap();
        assert_eq!((decoded, payload), (header, &[1_u8, 2, 3][..]));
        let digest = RefineDigest {
            scope: scope(),
            fence: 42,
            digest: [7; 16],
            block_count: 8160,
            coverage: BlockRuns::from_sorted_indices(0..8000),
        };
        assert_eq!(RefineDigest::decode(&digest.encode()), Ok(digest));
    }

    #[test]
    fn tile_limits_are_enforced_before_allocation() {
        let mut header = tile(256, 64);
        assert_eq!(header.expanded_bytes(), Ok(MAX_TILE_EXPANDED_BYTES));
        header.width = 272;
        assert_eq!(header.check_geometry(), Err(RefineRecordError::Geometry));
        header.width = 256;
        header.x = 8;
        assert_eq!(header.check_geometry(), Err(RefineRecordError::Geometry));
        header.x = 0;
        header.height = 0;
        assert_eq!(header.check_geometry(), Err(RefineRecordError::Geometry));
        // Edge tiles may be narrower than a block.
        header.height = 8;
        header.width = 7;
        assert_eq!(header.check_geometry(), Ok(()));

        let header = tile(16, 16);
        let mut bytes = header.encode_with(&[1, 2, 3]);
        bytes.push(0);
        assert_eq!(
            RefineTileHeader::decode(&bytes).map(|_| ()),
            Err(RefineRecordError::PayloadLength)
        );
        // A declared payload above the bound is refused even if present.
        let mut big = tile(16, 16);
        big.payload_len = (MAX_TILE_PAYLOAD_BYTES + 1) as u32;
        let bytes = big.encode_with(&vec![0; MAX_TILE_PAYLOAD_BYTES + 1]);
        assert_eq!(
            RefineTileHeader::decode(&bytes).map(|_| ()),
            Err(RefineRecordError::PayloadTooLarge)
        );
        let mut wrong = tile(16, 16).encode_with(&[1, 2, 3]);
        wrong[1] = 2;
        assert_eq!(
            RefineTileHeader::decode(&wrong).map(|_| ()),
            Err(RefineRecordError::Version(2))
        );
        wrong[1] = 1;
        wrong[20..24].copy_from_slice(&0_u32.to_be_bytes());
        assert_eq!(
            RefineTileHeader::decode(&wrong).map(|_| ()),
            Err(RefineRecordError::ZeroGeneration)
        );
    }

    #[test]
    fn runs_are_strict_and_bounded_by_the_record() {
        assert!(BlockRuns::new(vec![(0, 2), (2, 1)], 10).is_ok());
        assert_eq!(
            BlockRuns::new(vec![(0, 0)], 10),
            Err(RefineRecordError::Runs)
        );
        assert_eq!(
            BlockRuns::new(vec![(4, 2), (5, 1)], 10),
            Err(RefineRecordError::Runs)
        );
        assert_eq!(
            BlockRuns::new(vec![(4, 2), (1, 1)], 10),
            Err(RefineRecordError::Runs)
        );
        assert_eq!(
            BlockRuns::new(vec![(9, 2)], 10),
            Err(RefineRecordError::Runs)
        );
        assert_eq!(
            BlockRuns::new(vec![(u32::MAX, 2)], 10),
            Err(RefineRecordError::Runs)
        );
        // A run count that the record cannot hold is refused before any
        // allocation.
        let fence = RefineFence {
            scope: scope(),
            fence: 1,
            block_count: 10,
            invalidated: BlockRuns::default(),
        };
        let mut bytes = fence.encode();
        let at = bytes.len() - 4;
        bytes[at..].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(RefineFence::decode(&bytes), Err(RefineRecordError::Short));
        let mut bytes = fence.encode();
        bytes[32..36].copy_from_slice(&0_u32.to_be_bytes());
        assert_eq!(
            RefineFence::decode(&bytes),
            Err(RefineRecordError::BlockCount)
        );
    }

    #[test]
    fn capability_needs_domain_and_codec() {
        let capability = LosslessRefineCapability {
            domains: vec!["rgb".into()],
            codecs: vec![CODEC_DELTA_ZSTD1_TOKEN.into()],
        };
        assert!(capability.accepts(RefineDomain::Rgb));
        assert!(!capability.accepts(RefineDomain::YCbCr));
        assert!(!LosslessRefineCapability::default().accepts(RefineDomain::Rgb));
        let json = serde_json::to_string(&RefineControlMsg::RefineControl {
            monitor_id: 1,
            enabled: false,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"type":"refine_control","monitor_id":1,"enabled":false}"#
        );
    }
}
