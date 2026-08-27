//! Safe wrapper around source-built `rav1e`, Arcen's software AV1 backend.
//!
//! `rav1e` is the only encoder in the product with a twelve-bit path and the
//! only one that can encode AV1 4:4:4 at all -- NVENC's AV1 profile is
//! 4:2:0-only and defines no twelve-bit buffer format. The cost is that this
//! tier is CPU-encode (and, since AV1 4:4:4 has no macOS hardware decoder,
//! CPU-decode too); it is not a 4K60 mainline path. See
//! [`SoftwareAv1Encoder::new`] for the geometry this wrapper actually
//! validates, and the throughput benchmark in this module's tests for
//! measured numbers rather than a claim.
//!
//! This module mirrors `software_h264`'s shape -- a safe wrapper, the sole
//! caller of its codec crate, bounded output, typed errors, stats, and buffer
//! reuse -- but AV1 reorders frames internally (a fixed pipeline-fill delay
//! before the first packet, then steady-state one-in/one-out), so encoding is
//! split into a submit step that may return `Ok(None)` while the pipeline
//! fills, and a [`SoftwareAv1Encoder::finish`] step that flushes and drains
//! whatever is left at end of stream.
//!
//! Scope for this pass is 4:4:4 at eight, ten and twelve bits, which is the
//! combination NVENC cannot do at all. 4:2:0 would be a thin follow-up
//! mirroring [`SoftwareAv1Encoder::encode_i444_8bit`] with
//! `crate::video::I420Frame`; 4:2:2 needs a planar frame type `shared/media`
//! does not have yet, so it is out of scope here rather than half-built.

use std::error::Error;
use std::fmt::{Display, Formatter};

use rav1e::color::{
    ChromaSampling as NativeChromaSampling, ColorDescription as NativeColorDescription,
    ColorPrimaries as NativeColorPrimaries, MatrixCoefficients as NativeMatrixCoefficients,
    PixelRange as NativePixelRange, TransferCharacteristics as NativeTransferCharacteristics,
};
use rav1e::data::{FrameType, Rational};
use rav1e::{Config, Context, EncoderConfig, EncoderStatus, Pixel};

use super::{EncoderBackend, I444Frame, I444P16FrameMut};
use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, TransferCharacteristics,
};

/// Hard cap for one AV1 temporal-unit access unit.
///
/// AV1 4:4:4 at twelve bits carries roughly twice the entropy of the 4:2:0
/// H.264 case `software_h264` bounds at 16 MiB, so this cap is wider, but it
/// still exists so a pathological keyframe fails closed instead of growing
/// output storage without limit.
pub const MAX_SOFTWARE_AV1_ACCESS_UNIT_BYTES: usize = 64 * 1024 * 1024;

/// Bounded retries for rav1e's internal "encoded but not yet emitted" status.
///
/// Draining a single ready packet should never take more than a handful of
/// polls; the cap turns a hypothetical native misbehaviour into a typed
/// failure instead of an infinite loop.
const MAX_POLL_ATTEMPTS: u32 = 1024;

/// Validated `rav1e` stream configuration.
///
/// Geometry, bit depth, range and matrix are validated in
/// [`SoftwareAv1Encoder::new`] against [`EncoderBackend::Rav1e`]'s contract,
/// so this wrapper cannot silently claim a capability the backend contract
/// does not advertise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwareAv1Config {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Currently must be [`ChromaSubsampling::Yuv444`]; see the module docs.
    pub chroma: ChromaSubsampling,
    pub bit_depth: BitDepth,
    pub range: ColorRange,
    pub matrix: ColorMatrix,
    pub primaries: ColorPrimaries,
    pub transfer: TransferCharacteristics,
    /// Target bitrate in bits per second. Zero selects rav1e's fixed-quantizer
    /// mode instead of bitrate-targeted rate control.
    pub bitrate_bps: u32,
    /// rav1e speed preset: 0 is best quality, 10 is fastest.
    pub speed: u8,
    /// Disables frame reordering. Interactive use wants this set so a forced
    /// or requested key frame lands where it was asked for.
    pub low_latency: bool,
    /// Requested tile count for intra-frame parallelism. Zero uses rav1e's
    /// single-tile default; rav1e rounds a nonzero request up to a valid
    /// `tile_cols`/`tile_rows` split.
    pub tiles: u16,
    /// Size of rav1e's dedicated thread pool. Must be nonzero.
    pub num_threads: u16,
}

/// Privacy-safe software AV1 encoder failure.
#[derive(Debug)]
pub enum SoftwareAv1Error {
    InvalidConfig,
    FrameGeometryMismatch,
    /// The frame passed to an `encode_*` method does not match the chroma or
    /// bit depth this encoder was constructed with.
    UnexpectedFrameKind,
    NativeConfig(rav1e::InvalidConfig),
    Encoder(EncoderStatus),
    OutputTooLarge,
    OutputAllocation,
}

impl Display for SoftwareAv1Error {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConfig => "invalid software AV1 configuration",
            Self::FrameGeometryMismatch => {
                "input frame geometry differs from encoder configuration"
            }
            Self::UnexpectedFrameKind => {
                "input frame chroma or bit depth does not match the configured encoder"
            }
            Self::NativeConfig(_) => "rav1e rejected the derived encoder configuration",
            Self::Encoder(_) => "rav1e reported an encoder failure",
            Self::OutputTooLarge => "software AV1 access unit exceeds the cap",
            Self::OutputAllocation => "software AV1 output allocation failed",
        })
    }
}

impl Error for SoftwareAv1Error {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NativeConfig(error) => Some(error),
            Self::Encoder(error) => Some(error),
            _ => None,
        }
    }
}

/// Encoded frame classification exposed without native types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Av1FrameKind {
    Key,
    Inter,
    IntraOnly,
    Switch,
}

impl From<FrameType> for Av1FrameKind {
    fn from(kind: FrameType) -> Self {
        match kind {
            FrameType::KEY => Self::Key,
            FrameType::INTER => Self::Inter,
            FrameType::INTRA_ONLY => Self::IntraOnly,
            FrameType::SWITCH => Self::Switch,
        }
    }
}

/// Borrowed encoded AV1 temporal unit, valid until the next `encode_*` call.
#[derive(Debug, Clone, Copy)]
pub struct EncodedAv1AccessUnit<'a> {
    pub bytes: &'a [u8],
    pub kind: Av1FrameKind,
    pub is_keyframe: bool,
}

/// Owned encoded AV1 temporal unit, returned when draining at end of stream.
///
/// [`SoftwareAv1Encoder::finish`] may drain several reordered packets at
/// once, so unlike [`EncodedAv1AccessUnit`] these cannot all borrow the same
/// reused buffer.
#[derive(Debug, Clone)]
pub struct FinishedAv1AccessUnit {
    pub bytes: Vec<u8>,
    pub kind: Av1FrameKind,
    pub is_keyframe: bool,
}

/// Bounded Arcen-owned software AV1 encoder counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SoftwareAv1Stats {
    pub encoded_frames: u64,
    /// `encode_*` calls that returned `Ok(None)` because rav1e's reorder
    /// pipeline was still filling and had no packet ready yet.
    pub pending_polls: u64,
    pub emitted_bytes: u64,
    pub output_capacity_growths: u64,
    pub output_capacity: usize,
}

const fn to_native_chroma(chroma: ChromaSubsampling) -> NativeChromaSampling {
    match chroma {
        ChromaSubsampling::Yuv420 => NativeChromaSampling::Cs420,
        ChromaSubsampling::Yuv422 => NativeChromaSampling::Cs422,
        ChromaSubsampling::Yuv444 => NativeChromaSampling::Cs444,
    }
}

const fn to_native_range(range: ColorRange) -> NativePixelRange {
    match range {
        ColorRange::Limited => NativePixelRange::Limited,
        ColorRange::Full => NativePixelRange::Full,
    }
}

/// Maps H.273 primaries to rav1e's enum. `DisplayP3` is H.273 value 12 (SMPTE
/// EG 432-1), which is rav1e's `SMPTE432` -- not `SMPTE431`, which is DCI-P3.
const fn to_native_primaries(primaries: ColorPrimaries) -> NativeColorPrimaries {
    match primaries {
        ColorPrimaries::Bt709 => NativeColorPrimaries::BT709,
        ColorPrimaries::Bt2020 => NativeColorPrimaries::BT2020,
        ColorPrimaries::DisplayP3 => NativeColorPrimaries::SMPTE432,
    }
}

const fn to_native_transfer(transfer: TransferCharacteristics) -> NativeTransferCharacteristics {
    match transfer {
        TransferCharacteristics::Bt709 => NativeTransferCharacteristics::BT709,
        TransferCharacteristics::Srgb => NativeTransferCharacteristics::SRGB,
        TransferCharacteristics::Pq => NativeTransferCharacteristics::SMPTE2084,
        TransferCharacteristics::Hlg => NativeTransferCharacteristics::HLG,
    }
}

/// Maps Arcen's identity/GBR matrix to AV1's `matrix_coefficients = 0`
/// (Identity), the same H.273 value; every other matrix Arcen negotiates maps
/// 1:1 by H.273 value too.
const fn to_native_matrix(matrix: ColorMatrix) -> NativeMatrixCoefficients {
    match matrix {
        ColorMatrix::Identity => NativeMatrixCoefficients::Identity,
        ColorMatrix::Bt709 => NativeMatrixCoefficients::BT709,
        ColorMatrix::Bt601 => NativeMatrixCoefficients::BT601,
        ColorMatrix::Bt2020Ncl => NativeMatrixCoefficients::BT2020NCL,
    }
}

/// The two pixel widths AV1 supports; picked once at construction from
/// [`SoftwareAv1Config::bit_depth`] and never mixed within one encoder.
enum Inner {
    Eight(Context<u8>),
    Wide(Context<u16>),
}

/// Attachment-scoped safe wrapper around source-built `rav1e`.
///
/// This is the sole Arcen caller of `rav1e`. The wrapper exposes no native
/// pointers. `rav1e` is pure Rust with no vendored C/C++, unlike `openh264`,
/// but it is not unsafe-free -- it still uses `unsafe` internally (e.g. for
/// SIMD and raw-buffer reinterpretation in its `v_frame` dependency), so this
/// wrapper narrows the *language* boundary, not a safety guarantee about
/// every line underneath it.
pub struct SoftwareAv1Encoder {
    config: SoftwareAv1Config,
    inner: Inner,
    output: Vec<u8>,
    stats: SoftwareAv1Stats,
}

impl std::fmt::Debug for SoftwareAv1Encoder {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SoftwareAv1Encoder")
            .field("config", &self.config)
            .field("stats", &self.stats)
            .finish_non_exhaustive()
    }
}

impl SoftwareAv1Encoder {
    /// Construct the 4:4:4 rav1e encoder for one validated configuration.
    ///
    /// # Errors
    ///
    /// Rejects geometry, bit depth, range or matrix combinations outside
    /// [`EncoderBackend::Rav1e`]'s contract, non-4:4:4 chroma (see the module
    /// docs), a speed preset above 10, a zero or excessive thread count, and
    /// native configuration failure.
    pub fn new(config: SoftwareAv1Config) -> Result<Self, SoftwareAv1Error> {
        let limits = EncoderBackend::Rav1e.contract();
        if config.width == 0
            || config.height == 0
            || config.width > limits.max_width
            || config.height > limits.max_height
            || config.fps == 0
            || config.fps > limits.max_fps
            || config.chroma != ChromaSubsampling::Yuv444
            || !limits.supports_bit_depth(config.bit_depth)
            || !limits.supports_range(config.range)
            || !limits.supports_matrix(config.matrix)
            || config.speed > 10
            || config.num_threads == 0
            || config.num_threads > 64
        {
            return Err(SoftwareAv1Error::InvalidConfig);
        }

        let mut enc = EncoderConfig::with_speed_preset(config.speed);
        enc.width = config.width as usize;
        enc.height = config.height as usize;
        enc.time_base = Rational::new(1, u64::from(config.fps));
        enc.bit_depth = usize::from(config.bit_depth.bits());
        enc.chroma_sampling = to_native_chroma(config.chroma);
        enc.pixel_range = to_native_range(config.range);
        enc.color_description = Some(NativeColorDescription {
            color_primaries: to_native_primaries(config.primaries),
            transfer_characteristics: to_native_transfer(config.transfer),
            matrix_coefficients: to_native_matrix(config.matrix),
        });
        enc.low_latency = config.low_latency;
        enc.tiles = usize::from(config.tiles);
        if config.bitrate_bps > 0 {
            enc.bitrate =
                i32::try_from(config.bitrate_bps).map_err(|_| SoftwareAv1Error::InvalidConfig)?;
            // Matches rav1e's own CLI convention: a bitrate target implies the
            // maximum (least constraining) quantizer ceiling.
            enc.quantizer = 255;
        }

        let native = Config::default()
            .with_encoder_config(enc)
            .with_threads(usize::from(config.num_threads));
        let inner = match config.bit_depth {
            BitDepth::Eight => Inner::Eight(
                native
                    .new_context::<u8>()
                    .map_err(SoftwareAv1Error::NativeConfig)?,
            ),
            BitDepth::Ten | BitDepth::Twelve => Inner::Wide(
                native
                    .new_context::<u16>()
                    .map_err(SoftwareAv1Error::NativeConfig)?,
            ),
        };

        Ok(Self {
            config,
            inner,
            output: Vec::new(),
            stats: SoftwareAv1Stats::default(),
        })
    }

    #[must_use]
    pub const fn config(&self) -> SoftwareAv1Config {
        self.config
    }

    #[must_use]
    pub const fn stats(&self) -> SoftwareAv1Stats {
        self.stats
    }

    /// Encode one checked 8-bit planar 4:4:4 frame.
    ///
    /// Returns `Ok(None)` while rav1e's reorder pipeline is still filling;
    /// call [`SoftwareAv1Encoder::finish`] at end of stream to drain what is
    /// left.
    ///
    /// # Errors
    ///
    /// Returns [`SoftwareAv1Error::UnexpectedFrameKind`] unless this encoder
    /// was configured for eight-bit 4:4:4, [`SoftwareAv1Error::FrameGeometryMismatch`]
    /// on a size mismatch, and typed native or output-bound failures
    /// otherwise.
    pub fn encode_i444_8bit(
        &mut self,
        frame: I444Frame<'_>,
    ) -> Result<Option<EncodedAv1AccessUnit<'_>>, SoftwareAv1Error> {
        if self.config.bit_depth != BitDepth::Eight {
            return Err(SoftwareAv1Error::UnexpectedFrameKind);
        }
        if frame.width() != self.config.width as usize
            || frame.height() != self.config.height as usize
        {
            return Err(SoftwareAv1Error::FrameGeometryMismatch);
        }
        let Inner::Eight(ctx) = &mut self.inner else {
            return Err(SoftwareAv1Error::UnexpectedFrameKind);
        };

        let mut native = ctx.new_frame();
        let planes = frame.planes();
        let strides = frame.strides();
        for index in 0..3 {
            native.planes[index].copy_from_raw_u8(planes[index], strides[index], 1);
        }

        ctx.send_frame(native).map_err(SoftwareAv1Error::Encoder)?;
        let kind = poll_packet(ctx, &mut self.output, &mut self.stats)?;
        Ok(kind.map(|kind| EncodedAv1AccessUnit {
            bytes: &self.output,
            kind,
            is_keyframe: matches!(kind, Av1FrameKind::Key),
        }))
    }

    /// Encode one checked ten- or twelve-bit planar 4:4:4 frame.
    ///
    /// `frame`'s samples are MSB-aligned per [`I444P16FrameMut`] (a ten-bit
    /// code `v` is stored as `v << 6`); this unpacks them to the raw,
    /// low-bit-aligned codes AV1 expects, the same convention y4m and every
    /// other raw-plane AV1 tool use.
    ///
    /// # Errors
    ///
    /// Returns [`SoftwareAv1Error::UnexpectedFrameKind`] unless this encoder
    /// was configured for ten- or twelve-bit 4:4:4, [`SoftwareAv1Error::FrameGeometryMismatch`]
    /// on a size mismatch, and typed native or output-bound failures
    /// otherwise.
    pub fn encode_i444_high_bit_depth(
        &mut self,
        frame: &I444P16FrameMut<'_>,
    ) -> Result<Option<EncodedAv1AccessUnit<'_>>, SoftwareAv1Error> {
        if !matches!(self.config.bit_depth, BitDepth::Ten | BitDepth::Twelve) {
            return Err(SoftwareAv1Error::UnexpectedFrameKind);
        }
        if frame.width() != self.config.width as usize
            || frame.height() != self.config.height as usize
        {
            return Err(SoftwareAv1Error::FrameGeometryMismatch);
        }
        let Inner::Wide(ctx) = &mut self.inner else {
            return Err(SoftwareAv1Error::UnexpectedFrameKind);
        };

        let width = frame.width();
        let height = frame.height();
        let planes = frame.planes();
        let strides = frame.strides();
        let unpack_shift = 16 - u32::from(self.config.bit_depth.bits());

        let mut native = ctx.new_frame();
        for plane_index in 0..3 {
            let source = planes[plane_index];
            let source_stride = strides[plane_index];
            let native_plane = &mut native.planes[plane_index];
            let dest_stride = native_plane.cfg.stride;
            let destination = native_plane.data_origin_mut();
            for row in 0..height {
                let source_row = &source[row * source_stride..row * source_stride + width];
                let dest_row = &mut destination[row * dest_stride..row * dest_stride + width];
                for (dst, &word) in dest_row.iter_mut().zip(source_row.iter()) {
                    *dst = word >> unpack_shift;
                }
            }
        }

        ctx.send_frame(native).map_err(SoftwareAv1Error::Encoder)?;
        let kind = poll_packet(ctx, &mut self.output, &mut self.stats)?;
        Ok(kind.map(|kind| EncodedAv1AccessUnit {
            bytes: &self.output,
            kind,
            is_keyframe: matches!(kind, Av1FrameKind::Key),
        }))
    }

    /// Signal end of stream and drain every remaining reordered packet.
    ///
    /// # Errors
    ///
    /// Returns a typed error if rav1e reports an unrecoverable native failure
    /// or a drained access unit exceeds [`MAX_SOFTWARE_AV1_ACCESS_UNIT_BYTES`].
    pub fn finish(&mut self) -> Result<Vec<FinishedAv1AccessUnit>, SoftwareAv1Error> {
        match &mut self.inner {
            Inner::Eight(ctx) => finish_generic(ctx, &mut self.stats),
            Inner::Wide(ctx) => finish_generic(ctx, &mut self.stats),
        }
    }
}

/// Submit-side packet drain shared by both `encode_*` methods.
///
/// `Ok(None)` means rav1e's reorder pipeline had nothing ready yet (it
/// returned `NeedMoreData`); the caller should submit the next frame.
/// `LimitReached` is treated as a hard error here because it should never
/// occur before [`SoftwareAv1Encoder::finish`] has flushed the encoder --
/// [`Context::send_frame`] itself rejects further frames once flushed.
fn poll_packet<T: Pixel>(
    ctx: &mut Context<T>,
    output: &mut Vec<u8>,
    stats: &mut SoftwareAv1Stats,
) -> Result<Option<Av1FrameKind>, SoftwareAv1Error> {
    for _ in 0..MAX_POLL_ATTEMPTS {
        return match ctx.receive_packet() {
            Ok(packet) => {
                let kind = Av1FrameKind::from(packet.frame_type);
                store_packet(output, stats, &packet.data)?;
                Ok(Some(kind))
            }
            Err(EncoderStatus::Encoded) => continue,
            Err(EncoderStatus::NeedMoreData) => {
                stats.pending_polls = stats.pending_polls.saturating_add(1);
                Ok(None)
            }
            Err(other) => Err(SoftwareAv1Error::Encoder(other)),
        };
    }
    Err(SoftwareAv1Error::Encoder(EncoderStatus::Failure))
}

/// Copy one native packet into the reused, bounded output buffer.
fn store_packet(
    output: &mut Vec<u8>,
    stats: &mut SoftwareAv1Stats,
    data: &[u8],
) -> Result<(), SoftwareAv1Error> {
    if data.is_empty() || data.len() > MAX_SOFTWARE_AV1_ACCESS_UNIT_BYTES {
        return Err(SoftwareAv1Error::OutputTooLarge);
    }
    output.clear();
    let old_capacity = output.capacity();
    output
        .try_reserve(data.len())
        .map_err(|_| SoftwareAv1Error::OutputAllocation)?;
    output.extend_from_slice(data);
    if output.capacity() > old_capacity {
        stats.output_capacity_growths = stats.output_capacity_growths.saturating_add(1);
    }
    stats.encoded_frames = stats.encoded_frames.saturating_add(1);
    stats.emitted_bytes = stats.emitted_bytes.saturating_add(output.len() as u64);
    stats.output_capacity = output.capacity();
    Ok(())
}

/// Flush and drain every packet rav1e has left, owned rather than borrowed
/// since several reordered packets may come back in one call.
fn finish_generic<T: Pixel>(
    ctx: &mut Context<T>,
    stats: &mut SoftwareAv1Stats,
) -> Result<Vec<FinishedAv1AccessUnit>, SoftwareAv1Error> {
    ctx.flush();
    let mut units = Vec::new();
    loop {
        match ctx.receive_packet() {
            Ok(packet) => {
                if packet.data.is_empty() || packet.data.len() > MAX_SOFTWARE_AV1_ACCESS_UNIT_BYTES
                {
                    return Err(SoftwareAv1Error::OutputTooLarge);
                }
                let kind = Av1FrameKind::from(packet.frame_type);
                stats.encoded_frames = stats.encoded_frames.saturating_add(1);
                stats.emitted_bytes = stats.emitted_bytes.saturating_add(packet.data.len() as u64);
                units.push(FinishedAv1AccessUnit {
                    is_keyframe: matches!(kind, Av1FrameKind::Key),
                    bytes: packet.data,
                    kind,
                });
            }
            Err(EncoderStatus::Encoded) => {}
            Err(EncoderStatus::LimitReached) => break,
            Err(other) => return Err(SoftwareAv1Error::Encoder(other)),
        }
    }
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_pattern::TestPattern;
    use crate::video::{ColorTransform, convert_bgra_to_i444_p16};
    use arcen_keel::BgraFrame;

    fn base_config(bit_depth: BitDepth, width: u32, height: u32) -> SoftwareAv1Config {
        SoftwareAv1Config {
            width,
            height,
            fps: 30,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth,
            range: ColorRange::Full,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
            bitrate_bps: 0,
            speed: 10,
            low_latency: true,
            tiles: 0,
            num_threads: 1,
        }
    }

    fn solid_i444_8bit(width: usize, height: usize, value: u8) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        (
            vec![value; width * height],
            vec![value; width * height],
            vec![value; width * height],
        )
    }

    /// Feed enough frames to guarantee at least one packet is emitted,
    /// draining the fixed reorder-pipeline delay rather than assuming its
    /// exact length.
    const SMOKE_FRAME_COUNT: usize = 24;

    #[test]
    fn invalid_config_fails_closed() {
        assert!(matches!(
            SoftwareAv1Encoder::new(SoftwareAv1Config {
                width: 0,
                ..base_config(BitDepth::Eight, 64, 64)
            }),
            Err(SoftwareAv1Error::InvalidConfig)
        ));
        assert!(matches!(
            SoftwareAv1Encoder::new(SoftwareAv1Config {
                chroma: ChromaSubsampling::Yuv420,
                ..base_config(BitDepth::Eight, 64, 64)
            }),
            Err(SoftwareAv1Error::InvalidConfig)
        ));
        assert!(matches!(
            SoftwareAv1Encoder::new(SoftwareAv1Config {
                num_threads: 0,
                ..base_config(BitDepth::Eight, 64, 64)
            }),
            Err(SoftwareAv1Error::InvalidConfig)
        ));
        assert!(matches!(
            SoftwareAv1Encoder::new(SoftwareAv1Config {
                speed: 11,
                ..base_config(BitDepth::Eight, 64, 64)
            }),
            Err(SoftwareAv1Error::InvalidConfig)
        ));
        assert!(matches!(
            SoftwareAv1Encoder::new(SoftwareAv1Config {
                width: 7680,
                ..base_config(BitDepth::Eight, 64, 64)
            }),
            Err(SoftwareAv1Error::InvalidConfig)
        ));
    }

    #[test]
    fn eight_bit_444_encodes_a_keyframe_first_and_stays_bounded() {
        let mut encoder =
            SoftwareAv1Encoder::new(base_config(BitDepth::Eight, 64, 64)).expect("encoder");
        let (g, b, r) = solid_i444_8bit(64, 64, 128);
        let mut produced = Vec::new();
        for _ in 0..SMOKE_FRAME_COUNT {
            let frame = I444Frame::new(64, 64, [&g, &b, &r], [64, 64, 64]).expect("frame");
            if let Some(unit) = encoder.encode_i444_8bit(frame).expect("encode") {
                assert!(!unit.bytes.is_empty());
                assert!(unit.bytes.len() <= MAX_SOFTWARE_AV1_ACCESS_UNIT_BYTES);
                produced.push(unit.is_keyframe);
            }
        }
        let drained = encoder.finish().expect("finish");
        for unit in &drained {
            assert!(!unit.bytes.is_empty());
            assert!(unit.bytes.len() <= MAX_SOFTWARE_AV1_ACCESS_UNIT_BYTES);
        }
        produced.extend(drained.iter().map(|unit| unit.is_keyframe));
        assert_eq!(produced.len(), SMOKE_FRAME_COUNT);
        assert!(produced[0], "AV1 must start with a key frame");
        assert_eq!(encoder.stats().encoded_frames as usize, SMOKE_FRAME_COUNT);
    }

    #[test]
    fn identity_matrix_encodes_gbr_at_eight_bit() {
        let config = SoftwareAv1Config {
            matrix: ColorMatrix::Identity,
            ..base_config(BitDepth::Eight, 64, 64)
        };
        let mut encoder = SoftwareAv1Encoder::new(config).expect("identity encoder");
        let (g, b, r) = solid_i444_8bit(64, 64, 200);
        let mut total = 0usize;
        for _ in 0..SMOKE_FRAME_COUNT {
            let frame = I444Frame::new(64, 64, [&g, &b, &r], [64, 64, 64]).expect("frame");
            if encoder.encode_i444_8bit(frame).expect("encode").is_some() {
                total += 1;
            }
        }
        total += encoder.finish().expect("finish").len();
        assert_eq!(total, SMOKE_FRAME_COUNT);
    }

    fn encode_10_or_12_bit_smoke(bit_depth: BitDepth) {
        let width = 64;
        let height = 64;
        let config = base_config(bit_depth, width as u32, height as u32);
        let mut encoder = SoftwareAv1Encoder::new(config).expect("encoder");
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, bit_depth);
        let bgra = TestPattern::ChromaDetail.render_bgra(width, height);

        let mut total = 0usize;
        for _ in 0..SMOKE_FRAME_COUNT {
            let mut y = vec![0u16; width * height];
            let mut cb = vec![0u16; width * height];
            let mut cr = vec![0u16; width * height];
            let source = BgraFrame::new(&bgra, width, height, width * 4).expect("bgra");
            let mut destination = I444P16FrameMut::new(
                width as u32,
                height as u32,
                [&mut y, &mut cb, &mut cr],
                [width, width, width],
            )
            .expect("destination");
            convert_bgra_to_i444_p16(source, &mut destination, transform).expect("convert");
            if encoder
                .encode_i444_high_bit_depth(&destination)
                .expect("encode")
                .is_some()
            {
                total += 1;
            }
        }
        total += encoder.finish().expect("finish").len();
        assert_eq!(total, SMOKE_FRAME_COUNT);
    }

    #[test]
    fn ten_bit_444_round_trips_through_the_encoder() {
        encode_10_or_12_bit_smoke(BitDepth::Ten);
    }

    #[test]
    fn twelve_bit_444_round_trips_through_the_encoder() {
        encode_10_or_12_bit_smoke(BitDepth::Twelve);
    }

    #[test]
    fn frame_kind_mismatch_fails_closed() {
        let mut encoder =
            SoftwareAv1Encoder::new(base_config(BitDepth::Ten, 64, 64)).expect("ten-bit encoder");
        let (g, b, r) = solid_i444_8bit(64, 64, 128);
        let frame = I444Frame::new(64, 64, [&g, &b, &r], [64, 64, 64]).expect("frame");
        assert!(matches!(
            encoder.encode_i444_8bit(frame),
            Err(SoftwareAv1Error::UnexpectedFrameKind)
        ));
    }

    #[test]
    fn frame_geometry_mismatch_fails_closed() {
        let mut encoder =
            SoftwareAv1Encoder::new(base_config(BitDepth::Eight, 64, 64)).expect("encoder");
        let (g, b, r) = solid_i444_8bit(32, 32, 128);
        let frame = I444Frame::new(32, 32, [&g, &b, &r], [32, 32, 32]).expect("frame");
        assert!(matches!(
            encoder.encode_i444_8bit(frame),
            Err(SoftwareAv1Error::FrameGeometryMismatch)
        ));
    }

    #[test]
    fn output_capacity_never_shrinks_across_calls() {
        let mut encoder =
            SoftwareAv1Encoder::new(base_config(BitDepth::Eight, 64, 64)).expect("encoder");
        let (g, b, r) = solid_i444_8bit(64, 64, 60);
        let mut last_capacity = 0usize;
        for _ in 0..SMOKE_FRAME_COUNT {
            let frame = I444Frame::new(64, 64, [&g, &b, &r], [64, 64, 64]).expect("frame");
            if encoder.encode_i444_8bit(frame).expect("encode").is_some() {
                assert!(encoder.stats().output_capacity >= last_capacity);
                last_capacity = encoder.stats().output_capacity;
            }
        }
    }

    /// Real throughput, not a claim. Encodes synthetic 4:4:4 10-bit frames at
    /// 1080p and 4K and prints measured fps.
    ///
    /// Ignored by default because it is a multi-second CPU-bound measurement,
    /// not a correctness test. Run explicitly, in release mode, to get a
    /// number that means anything:
    /// `cargo test -p arcen-media --features software-av1-source --release -- --ignored --nocapture throughput_1080p_and_4k`
    #[test]
    #[ignore = "throughput measurement, not a correctness check; run with --ignored --nocapture --release"]
    fn throughput_1080p_and_4k_i444_10bit() {
        let threads = std::thread::available_parallelism()
            .map(std::num::NonZero::get)
            .unwrap_or(4)
            .min(64) as u16;
        let tiles = threads.next_power_of_two().min(16);

        for &(width, height, label, frame_count) in &[
            (1920u32, 1080u32, "1080p", 60usize),
            (3840u32, 2160u32, "4K", 24usize),
        ] {
            let fps = measure_fps(width, height, frame_count, threads, tiles);
            println!(
                "[software-av1 bench] {label} ({width}x{height}) 4:4:4 10-bit speed=10 \
                 low_latency threads={threads} tiles={tiles}: {fps:.2} fps over {frame_count} frames"
            );
        }
    }

    fn measure_fps(width: u32, height: u32, frame_count: usize, threads: u16, tiles: u16) -> f64 {
        let config = SoftwareAv1Config {
            tiles,
            num_threads: threads,
            ..base_config(BitDepth::Ten, width, height)
        };
        let mut encoder = SoftwareAv1Encoder::new(config).expect("bench encoder");
        let transform = ColorTransform::new(ColorMatrix::Bt709, ColorRange::Full, BitDepth::Ten);
        let bgra = TestPattern::FullGamutNoise.render_bgra(width as usize, height as usize);
        let (width, height) = (width as usize, height as usize);

        let mut y = vec![0u16; width * height];
        let mut cb = vec![0u16; width * height];
        let mut cr = vec![0u16; width * height];

        let start = std::time::Instant::now();
        for _ in 0..frame_count {
            let source = BgraFrame::new(&bgra, width, height, width * 4).expect("bgra");
            let mut destination = I444P16FrameMut::new(
                width as u32,
                height as u32,
                [&mut y, &mut cb, &mut cr],
                [width, width, width],
            )
            .expect("destination");
            convert_bgra_to_i444_p16(source, &mut destination, transform).expect("convert");
            encoder
                .encode_i444_high_bit_depth(&destination)
                .expect("encode");
        }
        encoder.finish().expect("finish");
        let elapsed = start.elapsed();
        frame_count as f64 / elapsed.as_secs_f64()
    }
}
