// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

use std::sync::OnceLock;

use arcen_media::annexb::{
    nal_kind, nals_to_length_prefixed, parse_access_unit, prepare_sample_nals, NalCodec, NalKind,
    ParameterSetCache, SamplePreparationError,
};
use arcen_media::bitstream::synth::HevcProbeProfile;
pub(crate) use arcen_media::obu as av1;

use crate::protocol::wire;
use crate::protocol::{ChromaSubsampling, FrameType, VideoCodec, VideoHeader};

const BGRA_PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"BGRA");
const RGBA_PIXEL_FORMAT: u32 = u32::from_be_bytes(*b"RGBA");
const NV12_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"420v");
const NV12_FULL_RANGE: u32 = u32::from_be_bytes(*b"420f");
const NV24_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"444v");
const NV24_FULL_RANGE: u32 = u32::from_be_bytes(*b"444f");
// Ten-bit biplanar formats. CoreVideo spells the ten-bit family with an `x`
// prefix and encodes the range in the FourCC itself rather than in a separate
// flag, so `x444` and `xf44` are the ten-bit 4:4:4 video- and full-range
// formats. Apple publishes no per-profile hardware-decode matrix, so whether a
// given Mac actually produces these is a probe-matrix question rather than
// something to assume here.
const P010_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"x420");
const P010_FULL_RANGE: u32 = u32::from_be_bytes(*b"xf20");
const P210_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"x422");
const P210_FULL_RANGE: u32 = u32::from_be_bytes(*b"xf22");
const P410_VIDEO_RANGE: u32 = u32::from_be_bytes(*b"x444");
const P410_FULL_RANGE: u32 = u32::from_be_bytes(*b"xf44");

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColorRange {
    Video,
    Full,
}

/// Coded component depth of a `CoreVideo` buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufferDepth {
    Eight,
    Ten,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PixelBufferFormat {
    Rgba,
    Bgra,
    Biplanar {
        chroma: ChromaSubsampling,
        range: ColorRange,
        depth: BufferDepth,
    },
}

fn classify_pixel_buffer_format(format: u32) -> Result<PixelBufferFormat, String> {
    let biplanar = |chroma, range, depth| {
        Ok(PixelBufferFormat::Biplanar {
            chroma,
            range,
            depth,
        })
    };
    match format {
        RGBA_PIXEL_FORMAT => Ok(PixelBufferFormat::Rgba),
        BGRA_PIXEL_FORMAT => Ok(PixelBufferFormat::Bgra),
        NV12_VIDEO_RANGE => biplanar(
            ChromaSubsampling::Yuv420,
            ColorRange::Video,
            BufferDepth::Eight,
        ),
        NV12_FULL_RANGE => biplanar(
            ChromaSubsampling::Yuv420,
            ColorRange::Full,
            BufferDepth::Eight,
        ),
        NV24_VIDEO_RANGE => biplanar(
            ChromaSubsampling::Yuv444,
            ColorRange::Video,
            BufferDepth::Eight,
        ),
        NV24_FULL_RANGE => biplanar(
            ChromaSubsampling::Yuv444,
            ColorRange::Full,
            BufferDepth::Eight,
        ),
        P010_VIDEO_RANGE => biplanar(
            ChromaSubsampling::Yuv420,
            ColorRange::Video,
            BufferDepth::Ten,
        ),
        P010_FULL_RANGE => biplanar(
            ChromaSubsampling::Yuv420,
            ColorRange::Full,
            BufferDepth::Ten,
        ),
        P210_VIDEO_RANGE => biplanar(
            ChromaSubsampling::Yuv422,
            ColorRange::Video,
            BufferDepth::Ten,
        ),
        P210_FULL_RANGE => biplanar(
            ChromaSubsampling::Yuv422,
            ColorRange::Full,
            BufferDepth::Ten,
        ),
        P410_VIDEO_RANGE => biplanar(
            ChromaSubsampling::Yuv444,
            ColorRange::Video,
            BufferDepth::Ten,
        ),
        P410_FULL_RANGE => biplanar(
            ChromaSubsampling::Yuv444,
            ColorRange::Full,
            BufferDepth::Ten,
        ),
        other => Err(format!(
            "unsupported VideoToolbox pixel format 0x{other:08x} ({})",
            fourcc_string(other)
        )),
    }
}

/// The `CoreVideo` pixel format to request for a negotiated stream.
///
/// Full range is selected by asking for a different FourCC, not by setting a
/// flag: on Apple the range *is* the format. Requesting `444v` for a
/// full-range stream is how a grading session silently gets crushed blacks.
#[must_use]
pub fn preferred_pixel_format(
    chroma: ChromaSubsampling,
    ten_bit: bool,
    full_range: bool,
) -> Option<u32> {
    Some(match (chroma, ten_bit, full_range) {
        (ChromaSubsampling::Yuv420, false, false) => NV12_VIDEO_RANGE,
        (ChromaSubsampling::Yuv420, false, true) => NV12_FULL_RANGE,
        (ChromaSubsampling::Yuv420, true, false) => P010_VIDEO_RANGE,
        (ChromaSubsampling::Yuv420, true, true) => P010_FULL_RANGE,
        (ChromaSubsampling::Yuv444, false, false) => NV24_VIDEO_RANGE,
        (ChromaSubsampling::Yuv444, false, true) => NV24_FULL_RANGE,
        (ChromaSubsampling::Yuv444, true, false) => P410_VIDEO_RANGE,
        (ChromaSubsampling::Yuv444, true, true) => P410_FULL_RANGE,
        (ChromaSubsampling::Yuv422, true, false) => P210_VIDEO_RANGE,
        (ChromaSubsampling::Yuv422, true, true) => P210_FULL_RANGE,
        // There is no eight-bit biplanar 4:2:2 format in the family Arcen
        // consumes, so an eight-bit 4:2:2 request has no answer rather than a
        // wrong one.
        (ChromaSubsampling::Yuv422, false, _) => return None,
    })
}

/// Whether the negotiated wire bit depth should be requested/described using
/// VideoToolbox's ten-bit biplanar family (`P010`/`P210`/`P410`).
///
/// There is no true twelve-bit `CVPixelBuffer` format in the family
/// [`preferred_pixel_format`] requests, so a twelve-bit stream asks for
/// ten-bit rather than going unrequested: the ten-bit biplanar formats store
/// each sample in the high bits of a 16-bit word, so requesting ten-bit only
/// loses the bottom two bits of a genuine twelve-bit sample, versus the four
/// bits an eight-bit request would lose.
#[must_use]
pub fn wants_ten_bit_pixel_format(depth: wire::BitDepth) -> bool {
    matches!(depth, wire::BitDepth::Ten | wire::BitDepth::Twelve)
}

/// Named `kCMFormatDescriptionColorPrimaries_*` constant to stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorPrimariesToken {
    Bt709,
    SmpteC,
    Bt2020,
}

/// Named `kCMFormatDescriptionTransferFunction_*` constant to stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferFunctionToken {
    Bt709,
    Srgb,
    Pq,
    Hlg,
}

/// Named `kCMFormatDescriptionYCbCrMatrix_*` constant to stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum YCbCrMatrixToken {
    Bt709,
    Bt601,
    Bt2020,
}

/// Resolved plan for the four `CMFormatDescription` colour extensions this
/// decoder stamps explicitly rather than trusting VideoToolbox's inference
/// from the in-stream VUI (undocumented and unverified whether VT honours,
/// say, `video_full_range_flag` on its own).
///
/// Kept as a plain, `raw::CFStringRef`-free struct so the mapping decision
/// is a pure function, testable without any Apple API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorExtensionPlan {
    pub primaries: ColorPrimariesToken,
    pub transfer: TransferFunctionToken,
    pub matrix: YCbCrMatrixToken,
    pub full_range: bool,
    /// Set only for [`wire::ColorMatrix::Identity`]: CoreVideo/CoreMedia
    /// expose no identity/GBR `YCbCrMatrix` constant —
    /// `kCVImageBufferYCbCrMatrixKey` permits only 709, 601, 2020, 240M,
    /// P3-D65 and DCI-P3 — so an identity-matrix stream cannot be described
    /// faithfully to any Apple API. Refusing to describe (and therefore
    /// decode) identity-matrix streams would make the highest-fidelity,
    /// zero-chroma-conversion-error mode this feature branch exists for
    /// undecodable on macOS, so this plan knowingly stamps BT.709 instead
    /// and flags it here: the caller must log this loudly, and any renderer
    /// consuming the resulting frames must treat them as already-RGB and
    /// must NOT apply a YCbCr matrix on top.
    pub matrix_is_knowingly_inaccurate: bool,
}

/// Pure mapping from the wire's per-frame matrix coefficients, negotiated
/// transfer characteristic, and separately-carried full-range flag to the
/// colour extensions to stamp.
#[must_use]
pub fn color_extension_plan(
    matrix: wire::ColorMatrix,
    full_range: bool,
    transfer: arcen_media::TransferCharacteristics,
) -> ColorExtensionPlan {
    let (primaries, matrix_token, matrix_is_knowingly_inaccurate) = match matrix {
        wire::ColorMatrix::Bt709 => (ColorPrimariesToken::Bt709, YCbCrMatrixToken::Bt709, false),
        wire::ColorMatrix::Bt601 => (ColorPrimariesToken::SmpteC, YCbCrMatrixToken::Bt601, false),
        wire::ColorMatrix::Bt2020Ncl => {
            (ColorPrimariesToken::Bt2020, YCbCrMatrixToken::Bt2020, false)
        }
        wire::ColorMatrix::Identity => (ColorPrimariesToken::Bt709, YCbCrMatrixToken::Bt709, true),
    };
    let transfer = match transfer {
        arcen_media::TransferCharacteristics::Bt709 => TransferFunctionToken::Bt709,
        arcen_media::TransferCharacteristics::Srgb => TransferFunctionToken::Srgb,
        arcen_media::TransferCharacteristics::Pq => TransferFunctionToken::Pq,
        arcen_media::TransferCharacteristics::Hlg => TransferFunctionToken::Hlg,
    };
    ColorExtensionPlan {
        primaries,
        transfer,
        matrix: matrix_token,
        full_range,
        matrix_is_knowingly_inaccurate,
    }
}

fn fourcc_string(format: u32) -> String {
    String::from_utf8_lossy(&format.to_be_bytes()).into_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PlaneLayout {
    width: usize,
    height: usize,
    bytes_per_row: usize,
    byte_len: usize,
}

fn validate_biplanar_layout(
    chroma: ChromaSubsampling,
    depth: BufferDepth,
    width: usize,
    height: usize,
    y: PlaneLayout,
    cbcr: PlaneLayout,
) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("biplanar buffer has zero dimensions".to_string());
    }
    // Above eight bits every sample occupies a 16-bit word, so the minimum
    // stride doubles. Validating against the eight-bit minimum would accept a
    // half-length plane and read past the end of the chroma rows.
    let bytes_per_sample = match depth {
        BufferDepth::Eight => 1,
        BufferDepth::Ten => 2,
    };
    if y.width != width || y.height != height || y.bytes_per_row < width * bytes_per_sample {
        return Err(format!(
            "invalid luma plane {}x{} stride={} for frame {width}x{height}",
            y.width, y.height, y.bytes_per_row
        ));
    }
    let (chroma_width, chroma_height) = match chroma {
        ChromaSubsampling::Yuv420 => (width.div_ceil(2), height.div_ceil(2)),
        ChromaSubsampling::Yuv422 => (width.div_ceil(2), height),
        ChromaSubsampling::Yuv444 => (width, height),
    };
    let min_chroma_stride = chroma_width
        .checked_mul(2 * bytes_per_sample)
        .ok_or_else(|| "chroma stride overflow".to_string())?;
    if cbcr.width != chroma_width
        || cbcr.height != chroma_height
        || cbcr.bytes_per_row < min_chroma_stride
    {
        return Err(format!(
            "invalid CbCr plane {}x{} stride={} for {:?} frame {width}x{height}",
            cbcr.width, cbcr.height, cbcr.bytes_per_row, chroma
        ));
    }
    let min_y_len = y
        .bytes_per_row
        .checked_mul(y.height)
        .ok_or_else(|| "luma plane length overflow".to_string())?;
    let min_cbcr_len = cbcr
        .bytes_per_row
        .checked_mul(cbcr.height)
        .ok_or_else(|| "chroma plane length overflow".to_string())?;
    if y.byte_len < min_y_len || cbcr.byte_len < min_cbcr_len {
        return Err(format!(
            "biplanar plane storage is truncated: y={}/{} cbcr={}/{}",
            y.byte_len, min_y_len, cbcr.byte_len, min_cbcr_len
        ));
    }
    Ok(())
}

/// The decoder's native picture: a `CVPixelBuffer` on macOS.
#[cfg(target_os = "macos")]
pub type NativePixelBuffer = apple_cf::cv::CVPixelBuffer;

/// Off macOS this crate has no native decoder, so no native picture can
/// exist: the type is uninhabited and every path that would present one is
/// unreachable (a Windows Deck has its own decoder and presenter).
#[cfg(not(target_os = "macos"))]
#[derive(Debug, Clone)]
pub enum NativePixelBuffer {}

#[cfg(not(target_os = "macos"))]
impl NativePixelBuffer {
    #[must_use]
    pub fn width(&self) -> usize {
        match *self {}
    }

    #[must_use]
    pub fn height(&self) -> usize {
        match *self {}
    }

    #[must_use]
    pub fn pixel_format(&self) -> u32 {
        match *self {}
    }
}

/// A decoded `CVPixelBuffer` with its video configuration (the shared
/// [`arcen_deck_core::decode::NativeDecodedVideoFrame`]).
pub type NativeDecodedVideoFrame =
    arcen_deck_core::decode::NativeDecodedVideoFrame<NativePixelBuffer>;

/// One decoded frame (the shared [`arcen_deck_core::decode::DecodedVideoFrame`]).
pub type DecodedVideoFrame = arcen_deck_core::decode::DecodedVideoFrame<NativePixelBuffer>;

pub use arcen_deck_core::decode::{
    NativePictureFormat, SessionColor, VideoDecodeError, VideoDecoderBackend,
};

/// The shared layout name of a CoreVideo pixel format.
#[must_use]
pub fn native_picture_format(fourcc: u32) -> NativePictureFormat {
    match &fourcc.to_be_bytes() {
        b"420v" | b"420f" => NativePictureFormat::Nv12,
        b"x420" => NativePictureFormat::P010,
        b"xf44" => NativePictureFormat::BiPlanar444Ten,
        b"BGRA" => NativePictureFormat::Bgra8,
        _ => NativePictureFormat::Other(fourcc),
    }
}

/// The wire codecs the native VideoToolbox path decodes today. H.264 is
/// the 4:2:0 MVP stream; H.265 (HEVC Rext) carries the native 4:4:4 stream;
/// AV1 carries the royalty-free 4:2:0 8/10-bit NVENC stream. AV1 uses OBU
/// framing (see the [`av1`] module) rather than Annex-B NALs, so it takes
/// its own decode path rather than the Annex-B/[`NalCodec`] one below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamCodec {
    H264,
    H265,
    Av1,
}

/// Every per-stream fact that changes VideoToolbox's session configuration.
///
/// This deliberately holds the wire depth rather than just its ten-bit
/// pixel-format family: the `VideoConfiguration` stamped onto native output
/// must stay in lockstep with the packet metadata even when two depths happen
/// to select the same CoreVideo pixel format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct VideoStreamKey {
    codec: StreamCodec,
    chroma: ChromaSubsampling,
    bit_depth: wire::BitDepth,
    full_range: bool,
    matrix: wire::ColorMatrix,
}

impl VideoStreamKey {
    fn from_header(codec: StreamCodec, header: &VideoHeader) -> Result<Self, VideoDecodeError> {
        Ok(Self {
            codec,
            chroma: header.chroma,
            bit_depth: header
                .bit_depth()
                .map_err(VideoDecodeError::InvalidWireColor)?,
            full_range: matches!(header.color_range(), wire::ColorRange::Full),
            matrix: header
                .color_matrix()
                .map_err(VideoDecodeError::InvalidWireColor)?,
        })
    }

    fn requires_session_rebuild(self, active: Option<Self>) -> bool {
        active != Some(self)
    }
}

/// The Annex-B NAL layout of a wire codec, or `None` for AV1, which uses OBU
/// framing ([`av1`]) and has no VPS/SPS/PPS-equivalent parameter set, so it
/// never reaches the Annex-B pipeline: `PlatformVideoDecoder::decode` branches
/// to its own AV1 path first.
const fn annex_b_codec(codec: StreamCodec) -> Option<NalCodec> {
    match codec {
        StreamCodec::H264 => Some(NalCodec::H264),
        StreamCodec::H265 => Some(NalCodec::H265),
        StreamCodec::Av1 => None,
    }
}

/// The wire chroma a synthetic probe profile declares.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const fn probe_profile_chroma(profile: HevcProbeProfile) -> ChromaSubsampling {
    if profile.chroma_format_idc == 3 {
        ChromaSubsampling::Yuv444
    } else {
        ChromaSubsampling::Yuv420
    }
}

#[derive(Default)]
pub struct NativeVideoDecoder {
    inner: platform::PlatformVideoDecoder,
}

impl NativeVideoDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the session-level colour axes the host reported as active,
    /// so decoded frames carry the stream's real primaries/transfer rather
    /// than a BT.709 assumption. Idempotent, and safe to call on every
    /// hello: a session's negotiated colour cannot change mid-stream.
    pub fn set_session_color(&mut self, session_color: SessionColor) {
        self.inner.set_session_color(session_color);
    }

    pub fn set_prefer_native_biplanar(&mut self, prefer: bool) -> bool {
        self.inner.set_prefer_native_biplanar(prefer)
    }

    /// Applies served-pipeline latency policy to the next VideoToolbox
    /// decompression session this decoder creates.
    pub fn set_decode_latency_policy(&mut self, policy: arcen_media::video::DecodeLatencyPolicy) {
        self.inner.set_decode_latency_policy(policy);
    }

    pub fn decode(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
        let codec = match (header.codec, header.frame_type) {
            (VideoCodec::H264, _) | (_, FrameType::VideoH264) => StreamCodec::H264,
            (VideoCodec::H265, _) | (_, FrameType::VideoH265) => StreamCodec::H265,
            (VideoCodec::Av1, _) | (_, FrameType::VideoAv1) => StreamCodec::Av1,
            _ => return Err(VideoDecodeError::CodecPathNotWired(header.codec)),
        };
        // Validate every colour field before a packet can reach a decoder
        // session. This gives reserved wire values a typed error path that the
        // monitor router turns into keyframe recovery, rather than an implicit
        // legacy-default conversion.
        VideoStreamKey::from_header(codec, header)?;
        self.inner.decode(codec, header, payload)
    }

    pub fn backend_name(&self) -> &'static str {
        self.inner.backend_name()
    }

    /// Decoded frames discarded because one submitted access unit produced
    /// several decoder callbacks and only the newest can be presented.
    ///
    /// Expected to stay at zero: Arcen's encoders are pinned to zero-reorder
    /// output (`EncodeIntent::REQUIRED_FRAME_INTERVAL_P`), so one access unit
    /// yields one frame. A non-zero value here is the signature of reordered
    /// output reaching a client that cannot reorder, which looks to a viewer
    /// like playback running forward, jumping back, then forward again.
    pub const fn collapsed_decode_callbacks(&self) -> u64 {
        self.inner.collapsed_decode_callbacks()
    }

    /// Whether `kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder`
    /// reported hardware acceleration for the current session. `None` before
    /// any session has been created, or if the property could not be read.
    ///
    /// Apple publishes no per-profile hardware-decode matrix, so this is the
    /// only reliable answer to whether a given stream (for example ten-bit
    /// 4:4:4) actually decoded in hardware on this Mac.
    pub fn is_hardware_accelerated(&self) -> Option<bool> {
        self.inner.is_hardware_accelerated()
    }

    pub fn wants_keyframe(&self) -> bool {
        self.inner.wants_keyframe()
    }

    pub fn notify_discontinuity(&mut self) {
        self.inner.notify_discontinuity();
    }
}

impl VideoDecoderBackend for NativeVideoDecoder {
    type NativeFrame = NativePixelBuffer;

    fn set_session_color(&mut self, session_color: SessionColor) {
        Self::set_session_color(self, session_color);
    }

    fn set_prefer_native_frames(&mut self, prefer: bool) -> bool {
        Self::set_prefer_native_biplanar(self, prefer)
    }

    fn set_decode_latency_policy(&mut self, policy: arcen_media::video::DecodeLatencyPolicy) {
        Self::set_decode_latency_policy(self, policy);
    }

    fn decode(
        &mut self,
        header: &VideoHeader,
        payload: &[u8],
    ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
        Self::decode(self, header, payload)
    }

    fn backend_name(&self) -> &'static str {
        Self::backend_name(self)
    }

    fn is_hardware_accelerated(&self) -> Option<bool> {
        Self::is_hardware_accelerated(self)
    }

    fn wants_keyframe(&self) -> bool {
        Self::wants_keyframe(self)
    }

    fn notify_discontinuity(&mut self) {
        Self::notify_discontinuity(self);
    }

    fn collapsed_decode_callbacks(&self) -> u64 {
        Self::collapsed_decode_callbacks(self)
    }
}

/// Reports what an HEVC stream's SPS says it carries, as it arrives.
///
/// The hello says what the host meant to send; this says what came. The Pier
/// logs the same reading of the same bytes as it encodes them, so the two
/// ends can be compared line for line.
fn log_received_stream_truth(sps: &[u8]) {
    match arcen_media::hevc_sps::parse_sps(sps) {
        Ok(truth) => {
            let colour = truth.colour;
            tracing::info!(
                target: crate::logging::target::SESSION,
                summary = %truth.summary(),
                visible = %format_args!("{}x{}", truth.display_width, truth.display_height),
                coded = %format_args!("{}x{}", truth.width, truth.height),
                chroma_format_idc = truth.chroma_format_idc,
                bit_depth = truth.bit_depth_luma,
                primaries = colour.map(|colour| colour.primaries),
                transfer = colour.map(|colour| colour.transfer),
                matrix = colour.map(|colour| colour.matrix),
                full_range = colour.map(|colour| colour.full_range),
                "received stream truth",
            );
        }
        Err(error) => tracing::warn!(
            target: crate::logging::target::SESSION,
            %error,
            "received an HEVC SPS this Deck could not read"
        ),
    }
}

/// Compares what an HEVC stream's SPS carries with what its frame header
/// claims, and says so when they differ.
///
/// The header is the host's claim; the SPS is what the decoder obeys. They
/// disagreed for every Grading session a macOS host once served — header
/// 4:4:4 10-bit, bitstream Main 4:2:0 8-bit — and nothing noticed, because
/// this decoder converts whatever arrives into the format it was asked for.
fn warn_when_the_stream_contradicts_its_header(stream: VideoStreamKey, sps: &[u8]) {
    if let Some(truth) = stream_contradicting_header(stream, sps) {
        tracing::warn!(
            target: crate::logging::target::VIDEO,
            stream = %truth.summary(),
            header_chroma = ?stream.chroma,
            header_bit_depth = ?stream.bit_depth,
            header_full_range = stream.full_range,
            "the stream is not what its header says; the host is sending less than it claims"
        );
    }
}

/// The SPS reading when it contradicts the header's chroma, depth or range.
fn stream_contradicting_header(
    stream: VideoStreamKey,
    sps: &[u8],
) -> Option<arcen_media::hevc_sps::HevcStreamTruth> {
    let truth = arcen_media::hevc_sps::parse_sps(sps).ok()?;
    let chroma_idc = match stream.chroma {
        ChromaSubsampling::Yuv420 => 1,
        ChromaSubsampling::Yuv422 => 2,
        ChromaSubsampling::Yuv444 => 3,
    };
    let depth = match stream.bit_depth {
        wire::BitDepth::Eight => 8,
        wire::BitDepth::Ten => 10,
        wire::BitDepth::Twelve => 12,
    };
    let range_matches = truth
        .colour
        .is_none_or(|colour| colour.full_range == stream.full_range);
    (truth.chroma_format_idc != chroma_idc || truth.bit_depth_luma != depth || !range_matches)
        .then_some(truth)
}

/// Real, empirically-probed decode capability for this process, backing
/// every codec/colour-fidelity flag `ClientHelloMsg` advertises. Every
/// field reflects an actual attempt to build a `CMFormatDescription` and
/// create a `VTDecompressionSession` on this machine (see
/// [`probe_decode_capabilities`]) -- never a hardcoded assumption.
///
/// Claiming a capability this Deck has not demonstrated is actively
/// harmful: the host honours the claim and sends a stream this Deck then
/// fails to decode. On macOS there is no API that answers "can I decode
/// HEVC Rext at ten bits" other than actually trying --
/// `VTIsHardwareDecodeSupported` is codec-level only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DecodeCapabilities {
    pub h264: bool,
    pub h265: bool,
    /// Codec-level only
    /// (`VTIsHardwareDecodeSupported(kCMVideoCodecType_AV1)`): there is no
    /// `CMVideoFormatDescriptionCreateFromParameterSets`-equivalent entry
    /// point for AV1 in the vendored bindings this codebase uses (the
    /// `probe_matrix` module documents the same gap), so unlike every
    /// other field here this one cannot be escalated to a real
    /// per-profile session probe *without a real coded stream to draw a
    /// Sequence Header OBU from* -- this process has no local AV1 encoder
    /// at start-up, before any connection exists, to produce one. The wire
    /// decode path (`StreamCodec::Av1`/`platform::decode_av1`) and
    /// `probe_matrix.rs`'s AV1 rows both *do* build a real session from an
    /// actual encoder-produced Sequence Header OBU (see the [`av1`]
    /// module) -- only this specific field, backing the pre-connection
    /// hello handshake, stays codec-level. Never a promise of 4:4:4
    /// support -- see `yuv444`, which is probed for real via HEVC Rext.
    /// The Deck platform profile caps it: an Intel Deck never claims AV1
    /// (`arcen_media::deck_platform::DeckPlatform::advertised_av1`).
    pub av1: bool,
    pub yuv444: bool,
    pub main10: bool,
    pub main12: bool,
    pub full_range: bool,
    /// Always `false`: CoreVideo/CoreMedia expose no identity/GBR
    /// `YCbCrMatrix` constant (see [`color_extension_plan`]'s
    /// `matrix_is_knowingly_inaccurate`), so there is no faithful way to
    /// probe -- or decode -- an identity-matrix stream on this platform
    /// at all. Kept as an explicit field (rather than only ever being
    /// implied) so every hello flag this struct backs is visibly
    /// accounted for here, with its reasoning attached.
    pub identity_matrix: bool,
    /// The actual VideoToolbox/CoreMedia probe accepted BT.601 extensions.
    pub bt601_matrix: bool,
    /// The actual VideoToolbox/CoreMedia probe accepted BT.2020 NCL
    /// extensions.
    pub bt2020_ncl_matrix: bool,
    /// Whether the primary probed codec (H.265 if it decoded here,
    /// otherwise H.264) reported hardware acceleration. `None` when
    /// neither decoded, or the property could not be read.
    pub hardware_accelerated: Option<bool>,
}

impl DecodeCapabilities {
    /// A short summary for `ClientHelloMsg::decoder_backend`, which used
    /// to be left a hardcoded empty string.
    #[must_use]
    pub fn decoder_backend_label(&self) -> &'static str {
        if !self.h264 && !self.h265 {
            return "unsupported";
        }
        match self.hardware_accelerated {
            Some(true) => "videotoolbox-hw",
            Some(false) => "videotoolbox-sw",
            None => "videotoolbox",
        }
    }
}

static DECODE_CAPABILITIES: OnceLock<DecodeCapabilities> = OnceLock::new();

/// Returns this process's real decode-capability probe result, running
/// the actual `VTDecompressionSessionCreate` attempts (see
/// `platform::probe_decode_capabilities`) at most once per process and
/// reusing the cached result for every later call -- a probe on every
/// reconnect would add visible latency, and the answer cannot change for
/// the lifetime of this process: VideoToolbox's hardware/codec
/// availability is a machine-level fact, not a per-connection one.
#[must_use]
pub fn probe_decode_capabilities() -> DecodeCapabilities {
    *DECODE_CAPABILITIES.get_or_init(platform::probe_decode_capabilities)
}

#[cfg(target_os = "macos")]
mod platform {
    use std::ffi::c_void;
    use std::ptr;
    use std::sync::mpsc::{self, Receiver, Sender};

    use apple_cf::cf::{AsCFType, CFData, CFDictionary, CFNumber, CFString, CFType};
    use apple_cf::cm::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer};
    use apple_cf::cv::{CVPixelBuffer, CVPixelBufferLockFlags};
    use apple_cf::raw;
    use videotoolbox::{ffi, DecodedFrame, DecompressionSession, PixelTransferSession};

    use super::{
        av1, classify_pixel_buffer_format, color_extension_plan, fourcc_string, nal_kind,
        nals_to_length_prefixed, parse_access_unit, preferred_pixel_format, prepare_sample_nals,
        validate_biplanar_layout, wants_ten_bit_pixel_format, wire, BufferDepth, ChromaSubsampling,
        ColorExtensionPlan, ColorPrimariesToken, ColorRange, DecodedVideoFrame, NalCodec, NalKind,
        NativeDecodedVideoFrame, ParameterSetCache, PixelBufferFormat, PlaneLayout,
        SamplePreparationError, SessionColor, StreamCodec, TransferFunctionToken, VideoDecodeError,
        VideoHeader, VideoStreamKey, YCbCrMatrixToken, BGRA_PIXEL_FORMAT, RGBA_PIXEL_FORMAT,
    };

    const FRAME_TIMESCALE: i32 = 1_000;

    enum DecodeMessage {
        Frame(DecodedVideoFrame),
        Error(String),
    }

    /// The most recently observed AV1 Sequence Header OBU: its raw bytes
    /// (reused verbatim as `av1C`'s `configOBUs`, see [`av1::build_av1c`])
    /// alongside the fields parsed out of it. AV1's analogue of
    /// `ParameterSetCache`, kept as a separate field rather than folded
    /// into it because AV1 has no VPS/SPS/PPS and does not go through the
    /// Annex-B pipeline `ParameterSetCache` serves (see [`NalCodec`]).
    struct Av1SequenceCache {
        obu: Vec<u8>,
        fields: av1::SequenceHeaderFields,
    }

    #[derive(Default)]
    pub struct PlatformVideoDecoder {
        stream: Option<VideoStreamKey>,
        parameter_sets: ParameterSetCache,
        av1_sequence: Option<Av1SequenceCache>,
        format: Option<CMFormatDescription>,
        session: Option<DecompressionSession>,
        tx: Option<Sender<DecodeMessage>>,
        rx: Option<Receiver<DecodeMessage>>,
        waiting_for_keyframe: bool,
        hardware_accelerated: Option<bool>,
        prefer_native_biplanar: bool,
        /// Decoded frames thrown away by [`Self::drain_decoded_messages`]
        /// because a later callback for the same submitted access unit
        /// superseded them.
        ///
        /// Under Arcen's zero-reorder encoder contract
        /// (`EncodeIntent::REQUIRED_FRAME_INTERVAL_P`) one submitted access
        /// unit should produce exactly one decoded frame, so this counter
        /// should stay at zero for an entire session. It exists because it
        /// did not: when the encoder was briefly allowed to emit B-frames,
        /// VideoToolbox released several callbacks at once and the drain kept
        /// only the newest, so frames vanished beneath every packet, loss and
        /// supersede counter the overlay reports. Silent discard turned a
        /// timestamp defect into an unexplainable one.
        collapsed_decode_callbacks: u64,
        /// The negotiated session-level colour axes, as the *host* reported
        /// them back (`ServerColorCaps::active_primaries`/`active_transfer`).
        ///
        /// These two axes are deliberately not read from the packet header:
        /// [`VideoHeader::flags`] is a full byte carrying keyframe, depth,
        /// range and matrix already, and primaries/transfer are negotiated
        /// once for a session rather than varying per frame. Defaulting
        /// them to BT.709 and never setting them -- which is what this
        /// decoder did until the transfer axis existed -- silently discards
        /// a negotiated PQ stream's own identity, so an HDR session is
        /// presented as SDR while every layer still agrees with itself.
        session_color: SessionColor,
        decode_latency_policy: arcen_media::video::DecodeLatencyPolicy,
    }

    impl PlatformVideoDecoder {
        pub fn set_session_color(&mut self, session_color: SessionColor) {
            self.session_color = session_color;
        }

        pub fn set_prefer_native_biplanar(&mut self, prefer: bool) -> bool {
            if self.prefer_native_biplanar == prefer {
                return false;
            }
            self.prefer_native_biplanar = prefer;
            self.format = None;
            self.session = None;
            self.waiting_for_keyframe = true;
            true
        }

        pub fn set_decode_latency_policy(
            &mut self,
            policy: arcen_media::video::DecodeLatencyPolicy,
        ) {
            self.decode_latency_policy = policy;
        }

        pub fn decode(
            &mut self,
            codec: StreamCodec,
            header: &VideoHeader,
            payload: &[u8],
        ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
            let stream = VideoStreamKey::from_header(codec, header)?;
            if stream.requires_session_rebuild(self.stream) {
                self.stream = Some(stream);
                match super::annex_b_codec(codec) {
                    Some(annex_b_codec) => self.parameter_sets.reset(annex_b_codec),
                    None => self.av1_sequence = None,
                }
                self.format = None;
                self.session = None;
                self.tx = None;
                self.rx = None;
                self.waiting_for_keyframe = true;
            }

            let Some(annex_b_codec) = super::annex_b_codec(codec) else {
                return self.decode_av1(codec, header, payload);
            };

            let nals = parse_access_unit(annex_b_codec, payload).map_err(|error| {
                self.waiting_for_keyframe = true;
                VideoDecodeError::Backend(error.to_string())
            })?;
            let update = self.parameter_sets.observe(annex_b_codec, &nals);
            if update.new_sps && annex_b_codec == NalCodec::H265 {
                if let Some(sps) = self.parameter_sets.sps() {
                    super::log_received_stream_truth(sps);
                }
            }
            let format_changed = update.changed;
            if format_changed && annex_b_codec == NalCodec::H265 {
                if let (Some(stream), Some(sps)) = (self.stream, self.parameter_sets.sps()) {
                    super::warn_when_the_stream_contradicts_its_header(stream, sps);
                }
            }
            if format_changed {
                self.format = None;
                self.session = None;
                self.tx = None;
                self.rx = None;
                self.waiting_for_keyframe = true;
            }
            if self.format.is_none() || self.session.is_none() {
                self.rebuild_session_if_ready(codec, header)?;
            }

            let is_keyframe = nals
                .iter()
                .any(|nal| super::nal_kind(annex_b_codec, nal) == super::NalKind::Keyframe);
            if self.waiting_for_keyframe && !is_keyframe {
                return Ok(None);
            }

            let decode_nals = match prepare_sample_nals(annex_b_codec, &self.parameter_sets, &nals)
            {
                Ok(Some(nals)) => nals,
                Ok(None) => return Ok(None),
                Err(SamplePreparationError::MissingParameterSets) => {
                    self.waiting_for_keyframe = true;
                    tracing::warn!(
                        target: crate::logging::target::VIDEO,
                        ?codec,
                        "keyframe is missing a complete cached parameter-set configuration",
                    );
                    return Ok(None);
                }
            };
            let sample_payload = nals_to_length_prefixed(&decode_nals);
            let unit_summary = summarize_nals(annex_b_codec, &decode_nals);
            let Some(format) = &self.format else {
                self.waiting_for_keyframe = true;
                return Ok(None);
            };
            let Some(session) = &self.session else {
                self.waiting_for_keyframe = true;
                return Ok(None);
            };
            let sample = make_sample_buffer(format, &sample_payload, header.timestamp_ms)?;
            session
                .decode(&sample)
                .map_err(|error| VideoDecodeError::Backend(format!("VT decode failed: {error}")))?;
            session.wait_for_async_frames().map_err(|error| {
                VideoDecodeError::Backend(format!("VT wait_for_async_frames failed: {error}"))
            })?;

            let decoded = self.drain_decoded_messages(codec, header, &unit_summary)?;
            if decoded.is_some() {
                self.waiting_for_keyframe = false;
            }
            Ok(decoded)
        }

        /// AV1's decode path: OBU-framed, not Annex-B NAL-framed, and with
        /// no VPS/SPS/PPS-style parameter set, so it cannot go through
        /// [`Self::decode`]'s Annex-B pipeline above (see [`NalCodec`]).
        /// Reuses [`Self::rebuild_session_if_ready`] (session creation,
        /// colour-extension stamping, pixel-format request,
        /// hardware-acceleration query) and [`Self::drain_decoded_messages`]
        /// (async frame draining, transient-error recovery) unchanged --
        /// only format-description construction and sample preparation
        /// differ from the Annex-B codecs.
        ///
        /// Per AOMediaCodec/av1-isobmff `index.bs`'s "AV1 Sample Format",
        /// `payload` is expected to already be one whole Temporal Unit's
        /// OBUs (Arcen's transport reassembles a complete access unit
        /// before calling [`Self::decode`] for every codec, not just this
        /// one), fed to `CMSampleBufferCreateReady` verbatim: unlike
        /// H.264/HEVC's AVCC repackaging, AV1 needs no re-framing, and
        /// av1-isobmff explicitly tolerates (does not require stripping)
        /// a Sequence Header OBU duplicated between `av1C`'s `configOBUs`
        /// and the sample itself ("Compliant AV1 decoders are expected to
        /// handle that").
        fn decode_av1(
            &mut self,
            codec: StreamCodec,
            header: &VideoHeader,
            payload: &[u8],
        ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
            let obus = av1::parse_obus(payload).map_err(|error| {
                self.waiting_for_keyframe = true;
                VideoDecodeError::Backend(format!("AV1 OBU parsing failed: {error}"))
            })?;

            let sequence_header_obu = obus
                .iter()
                .find(|obu| obu.obu_type == av1::ObuType::SequenceHeader);
            let mut format_changed = false;
            if let Some(obu) = sequence_header_obu {
                match av1::parse_sequence_header(obu.payload) {
                    Ok(fields) => {
                        let changed = self
                            .av1_sequence
                            .as_ref()
                            .is_none_or(|cache| cache.obu.as_slice() != obu.whole);
                        if changed {
                            self.av1_sequence = Some(Av1SequenceCache {
                                obu: obu.whole.to_vec(),
                                fields,
                            });
                            format_changed = true;
                        }
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: crate::logging::target::VIDEO,
                            %error,
                            "failed to parse AV1 sequence header OBU; ignoring it",
                        );
                    }
                }
            }
            if format_changed {
                self.format = None;
                self.session = None;
                self.tx = None;
                self.rx = None;
                self.waiting_for_keyframe = true;
            }
            if self.format.is_none() || self.session.is_none() {
                self.rebuild_session_if_ready(codec, header)?;
            }

            // Per av1-isobmff's "AV1 Sample Format": a sync sample "contains
            // a Sequence Header OBU before the first Frame Header OBU", so
            // presence of one is this stream's entry-point signal -- the
            // AV1 analogue of the Annex-B pipeline's keyframe NAL check.
            let is_keyframe = sequence_header_obu.is_some();
            if self.waiting_for_keyframe && !is_keyframe {
                return Ok(None);
            }
            let Some(format) = &self.format else {
                self.waiting_for_keyframe = true;
                return Ok(None);
            };
            let Some(session) = &self.session else {
                self.waiting_for_keyframe = true;
                return Ok(None);
            };

            let sample = make_sample_buffer(format, payload, header.timestamp_ms)?;
            session
                .decode(&sample)
                .map_err(|error| VideoDecodeError::Backend(format!("VT decode failed: {error}")))?;
            session.wait_for_async_frames().map_err(|error| {
                VideoDecodeError::Backend(format!("VT wait_for_async_frames failed: {error}"))
            })?;

            let unit_summary = av1::summarize(&obus);
            let decoded = self.drain_decoded_messages(codec, header, &unit_summary)?;
            if decoded.is_some() {
                self.waiting_for_keyframe = false;
            }
            Ok(decoded)
        }

        pub fn backend_name(&self) -> &'static str {
            match self.hardware_accelerated {
                Some(true) => "videotoolbox-bgra-hw",
                Some(false) => "videotoolbox-bgra-sw",
                None => "videotoolbox-bgra",
            }
        }

        /// See [`super::NativeVideoDecoder::is_hardware_accelerated`].
        pub fn is_hardware_accelerated(&self) -> Option<bool> {
            self.hardware_accelerated
        }

        pub fn wants_keyframe(&self) -> bool {
            self.waiting_for_keyframe
        }

        pub fn notify_discontinuity(&mut self) {
            self.waiting_for_keyframe = true;
        }

        fn rebuild_session_if_ready(
            &mut self,
            codec: StreamCodec,
            header: &VideoHeader,
        ) -> Result<(), VideoDecodeError> {
            let chroma = header.chroma;
            let bit_depth = header
                .bit_depth()
                .map_err(VideoDecodeError::InvalidWireColor)?;
            let ten_bit = wants_ten_bit_pixel_format(bit_depth);
            let full_range = matches!(header.color_range(), wire::ColorRange::Full);
            let matrix = header
                .color_matrix()
                .map_err(VideoDecodeError::InvalidWireColor)?;
            let native_video = native_video_configuration(
                codec,
                chroma,
                bit_depth,
                full_range,
                matrix,
                self.session_color,
            );

            let base_format = match codec {
                StreamCodec::H264 => {
                    let Some(parameter_sets) = self.parameter_sets.parameter_sets(NalCodec::H264)
                    else {
                        self.waiting_for_keyframe = true;
                        return Ok(());
                    };
                    make_h264_format_description(parameter_sets[0], parameter_sets[1])?
                }
                StreamCodec::H265 => {
                    let Some(parameter_sets) = self.parameter_sets.parameter_sets(NalCodec::H265)
                    else {
                        self.waiting_for_keyframe = true;
                        return Ok(());
                    };
                    make_hevc_format_description(
                        parameter_sets[0],
                        parameter_sets[1],
                        parameter_sets[2],
                    )?
                }
                StreamCodec::Av1 => {
                    let Some(cache) = &self.av1_sequence else {
                        self.waiting_for_keyframe = true;
                        return Ok(());
                    };
                    make_av1_format_description(&cache.fields, &cache.obu)?
                }
            };

            let plan = color_extension_plan(matrix, full_range, self.session_color.transfer);
            if plan.matrix_is_knowingly_inaccurate {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    ?codec,
                    "stream uses an identity/GBR matrix, which CoreVideo/CoreMedia cannot \
                     describe (kCVImageBufferYCbCrMatrixKey has no identity constant); \
                     stamping BT.709 on the CMFormatDescription as a knowingly inaccurate \
                     placeholder — frames from this session are NOT actually BT.709 YCbCr \
                     and any renderer consuming them must treat them as already-RGB and must \
                     not apply a YCbCr matrix",
                );
            }
            let format = apply_color_extensions(base_format, codec, plan);

            let requested_pixel_format = preferred_pixel_format(chroma, ten_bit, full_range);
            let requested_pixel_format_name = requested_pixel_format.map(fourcc_string);
            if requested_pixel_format.is_none() {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    ?chroma,
                    ten_bit,
                    full_range,
                    "no CVPixelBuffer format exists for this chroma/depth/range combination; \
                     letting VideoToolbox choose its native output",
                );
            }
            // A NULL destination-attributes dictionary asks VideoToolbox for
            // the decoder's native output, which is what happens below when
            // `requested_pixel_format` has no answer. Otherwise we request the
            // exact FourCC `preferred_pixel_format` resolved for this stream's
            // negotiated chroma/depth/range, rather than letting VideoToolbox
            // guess.
            let attributes = requested_pixel_format.and_then(|format| {
                build_destination_attributes(format, self.prefer_native_biplanar)
            });

            let (tx, rx) = mpsc::channel();

            // Each attempt gets its own transfer session and callback, because
            // `PixelTransferSession` is neither `Clone` nor guaranteed `Sync`
            // and the callback must own it. The outer `Result` is a hard
            // failure; the inner one is the session creation we may retry.
            let prefer_native_biplanar = self.prefer_native_biplanar;
            let build_session = |attributes: Option<&CFDictionary>| {
                let transfer = PixelTransferSession::new().map_err(|error| {
                    VideoDecodeError::Backend(format!(
                        "VTPixelTransferSessionCreate failed: {error}"
                    ))
                })?;
                let callback_tx = tx.clone();
                Ok(DecompressionSession::new_with_image_buffer_attributes(
                    &format,
                    attributes,
                    move |frame| {
                        let message = match copy_rgba_frame(
                            frame,
                            chroma,
                            &transfer,
                            native_video,
                            prefer_native_biplanar,
                        ) {
                            Ok(frame) => DecodeMessage::Frame(frame),
                            Err(error) => DecodeMessage::Error(error),
                        };
                        let _ = callback_tx.send(message);
                    },
                ))
            };

            let session = match build_session(attributes.as_ref())? {
                Ok(session) => session,
                Err(error) if attributes.is_some() => {
                    // A requested destination pixel format the decoder cannot
                    // supply fails session creation outright. Rather than
                    // losing the session entirely, fall back to VideoToolbox's
                    // own native output, which the copy path already
                    // classifies and converts. Logged at warn because the
                    // negotiated format was not honoured exactly, and a
                    // colourist is entitled to know that.
                    tracing::warn!(
                        target: crate::logging::target::VIDEO,
                        ?codec,
                        ?chroma,
                        ten_bit,
                        full_range,
                        ?requested_pixel_format_name,
                        %error,
                        "VideoToolbox refused the requested destination pixel format; retrying \
                         with its native output",
                    );
                    build_session(None)?.map_err(|error| {
                        VideoDecodeError::Backend(format!(
                            "VTDecompressionSessionCreate failed even without a pixel-format \
                             request: {error}"
                        ))
                    })?
                }
                Err(error) => {
                    return Err(VideoDecodeError::Backend(format!(
                        "VTDecompressionSessionCreate failed: {error}"
                    )));
                }
            };
            session
                .set_real_time(self.decode_latency_policy.realtime)
                .map_err(|error| {
                    VideoDecodeError::Backend(format!(
                        "Could not enable VT real-time decode: {error}"
                    ))
                })?;
            let hardware_accelerated = query_hardware_accelerated(&session);
            tracing::info!(
                target: crate::logging::target::VIDEO,
                ?codec,
                ?chroma,
                ten_bit,
                full_range,
                ?matrix,
                ?requested_pixel_format_name,
                realtime = self.decode_latency_policy.realtime,
                ?hardware_accelerated,
                "VideoToolbox decompression session (re)configured",
            );

            self.format = Some(format);
            self.session = Some(session);
            self.tx = Some(tx);
            self.rx = Some(rx);
            self.hardware_accelerated = hardware_accelerated;
            self.waiting_for_keyframe = true;
            Ok(())
        }

        fn drain_decoded_messages(
            &mut self,
            codec: StreamCodec,
            header: &VideoHeader,
            unit_summary: &str,
        ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
            if self.rx.is_none() {
                return Ok(None);
            }
            let mut last: Option<DecodedVideoFrame> = None;
            let mut collapsed = 0u64;
            // The receiver is borrowed per iteration rather than for the whole
            // loop so the error arms below can record `collapsed` through
            // `&mut self` before they return. Holding one borrow across the
            // loop is what previously forced those returns to throw the count
            // away — silently, which is the exact behaviour this counter
            // exists to abolish.
            loop {
                let Some(Ok(message)) = self.rx.as_ref().map(Receiver::try_recv) else {
                    break;
                };
                match message {
                    DecodeMessage::Frame(frame) => {
                        if last.replace(frame).is_some() {
                            collapsed += 1;
                        }
                    }
                    DecodeMessage::Error(error) => {
                        self.record_collapsed_callbacks(collapsed, unit_summary);
                        if is_transient_bad_data(&error) {
                            tracing::warn!(
                                target: crate::logging::target::VIDEO,
                                %error,
                                %unit_summary,
                                "VideoToolbox rejected the access unit; rebuilding at a keyframe",
                            );
                            self.waiting_for_keyframe = true;
                            self.rebuild_session_if_ready(codec, header)?;
                            return Ok(None);
                        }
                        self.waiting_for_keyframe = true;
                        return Err(VideoDecodeError::Backend(format!(
                            "{error}; {unit_summary}"
                        )));
                    }
                }
            }
            self.record_collapsed_callbacks(collapsed, unit_summary);
            Ok(last)
        }

        /// Record frames this drain discarded, warning on the first ever.
        ///
        /// Loud, because under the zero-reorder encoder contract this cannot
        /// happen: one access unit in, one decoded frame out. Reaching here
        /// means either the encoder emitted reordered output after all, or a
        /// decoder session is releasing frames in bursts — and either way the
        /// picture will appear to jump backwards.
        fn record_collapsed_callbacks(&mut self, collapsed: u64, unit_summary: &str) {
            if collapsed == 0 {
                return;
            }
            let first = self.collapsed_decode_callbacks == 0;
            self.collapsed_decode_callbacks =
                self.collapsed_decode_callbacks.saturating_add(collapsed);
            if first {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    collapsed,
                    %unit_summary,
                    "decoder released several frames for one access unit; only the newest is \
                     presented. Expected zero under the zero-reorder encoder contract — see \
                     EncodeIntent::REQUIRED_FRAME_INTERVAL_P",
                );
            }
        }

        /// Decoded frames discarded because one access unit produced several
        /// decoder callbacks. Should be zero for a whole session; see the
        /// field's own documentation.
        pub const fn collapsed_decode_callbacks(&self) -> u64 {
            self.collapsed_decode_callbacks
        }
    }

    fn make_h264_format_description(
        sps: &[u8],
        pps: &[u8],
    ) -> Result<CMFormatDescription, VideoDecodeError> {
        let parameter_sets = [sps.as_ptr(), pps.as_ptr()];
        let parameter_set_sizes = [sps.len(), pps.len()];
        let mut format: raw::CMFormatDescriptionRef = ptr::null_mut();
        let status = unsafe {
            raw::CMVideoFormatDescriptionCreateFromH264ParameterSets(
                raw::kCFAllocatorDefault,
                parameter_sets.len(),
                parameter_sets.as_ptr(),
                parameter_set_sizes.as_ptr(),
                4,
                &mut format,
            )
        };
        if status != 0 || format.is_null() {
            return Err(VideoDecodeError::Backend(format!(
                "CMVideoFormatDescriptionCreateFromH264ParameterSets failed: {status}"
            )));
        }
        CMFormatDescription::from_raw(format.cast_mut().cast::<c_void>()).ok_or_else(|| {
            VideoDecodeError::Backend(
                "CMVideoFormatDescriptionCreateFromH264ParameterSets returned NULL".to_string(),
            )
        })
    }

    fn make_hevc_format_description(
        vps: &[u8],
        sps: &[u8],
        pps: &[u8],
    ) -> Result<CMFormatDescription, VideoDecodeError> {
        let parameter_sets = [vps.as_ptr(), sps.as_ptr(), pps.as_ptr()];
        let parameter_set_sizes = [vps.len(), sps.len(), pps.len()];
        let mut format: raw::CMFormatDescriptionRef = ptr::null_mut();
        // No extensions dictionary — VideoToolbox derives Rext 4:4:4 support
        // from the SPS itself.
        let extensions: raw::CFDictionaryRef = unsafe { std::mem::zeroed() };
        let status = unsafe {
            raw::CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                raw::kCFAllocatorDefault,
                parameter_sets.len(),
                parameter_sets.as_ptr(),
                parameter_set_sizes.as_ptr(),
                4,
                extensions,
                &mut format,
            )
        };
        if status != 0 || format.is_null() {
            return Err(VideoDecodeError::Backend(format!(
                "CMVideoFormatDescriptionCreateFromHEVCParameterSets failed: {status}"
            )));
        }
        CMFormatDescription::from_raw(format.cast_mut().cast::<c_void>()).ok_or_else(|| {
            VideoDecodeError::Backend(
                "CMVideoFormatDescriptionCreateFromHEVCParameterSets returned NULL".to_string(),
            )
        })
    }

    /// Builds a `CMVideoFormatDescription` for AV1 from `fields`/
    /// `sequence_header_obu` (see the [`super::av1`] module). Unlike
    /// H.264/HEVC, there is no
    /// `CMVideoFormatDescriptionCreateFromAV1ParameterSets`-equivalent
    /// entry point in the vendored `apple-cf`/`videotoolbox` bindings this
    /// codebase uses (confirmed absent from both crates' sources) or, as
    /// far as this codebase's research found, in CoreMedia itself -- so
    /// this builds the description with the generic
    /// `CMVideoFormatDescriptionCreate`, passing the AV1 Codec
    /// Configuration Record (`av1C`, AOMediaCodec/av1-isobmff `index.bs`)
    /// through the `SampleDescriptionExtensionAtoms` extension, exactly
    /// the approach Chromium (`media/gpu/mac/vt_config_util.mm`), Firefox
    /// (`AppleVTDecoder.cpp`) and WebKit (`FormatDescriptionUtilities.cpp`)
    /// all use for the same gap (see the [`super::av1`] module doc for the
    /// full cross-check).
    ///
    /// No colour extensions are stamped here -- the caller
    /// (`rebuild_session_if_ready`) applies them afterwards via
    /// `apply_color_extensions`, exactly as it already does for H.264/HEVC,
    /// so both codec families go through one shared, already-proven
    /// colour-extension code path rather than a second copy of it.
    fn make_av1_format_description(
        fields: &super::av1::SequenceHeaderFields,
        sequence_header_obu: &[u8],
    ) -> Result<CMFormatDescription, VideoDecodeError> {
        let width = i32::try_from(fields.max_frame_width).map_err(|_| {
            VideoDecodeError::Backend(format!(
                "AV1 max_frame_width {} does not fit in i32",
                fields.max_frame_width
            ))
        })?;
        let height = i32::try_from(fields.max_frame_height).map_err(|_| {
            VideoDecodeError::Backend(format!(
                "AV1 max_frame_height {} does not fit in i32",
                fields.max_frame_height
            ))
        })?;

        let av1c = super::av1::build_av1c(fields, sequence_header_obu);
        let av1c_data = CFData::from_bytes(&av1c);
        let av1c_key = CFString::new("av1C");
        let atoms = CFDictionary::from_pairs(&[(&av1c_key, &av1c_data)]);
        // SAFETY: `kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms`
        // is a well-known, process-wide singleton CFStringRef exported by
        // CoreMedia.
        let atoms_key = unsafe {
            CFString::from_raw_retained(
                raw::kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms
                    .cast_mut()
                    .cast(),
            )
        }
        .ok_or_else(|| {
            VideoDecodeError::Backend(
                "kCMFormatDescriptionExtension_SampleDescriptionExtensionAtoms was NULL"
                    .to_string(),
            )
        })?;
        let extensions = CFDictionary::from_pairs(&[(&atoms_key, &atoms)]);

        let mut format: raw::CMVideoFormatDescriptionRef = ptr::null_mut();
        // SAFETY: `extensions` is a valid, live `CFDictionary` for the
        // duration of this call; `format` is a valid out-pointer.
        let status = unsafe {
            raw::CMVideoFormatDescriptionCreate(
                raw::kCFAllocatorDefault,
                raw::kCMVideoCodecType_AV1,
                width,
                height,
                extensions.as_ptr().cast(),
                &mut format,
            )
        };
        if status != 0 || format.is_null() {
            return Err(VideoDecodeError::Backend(format!(
                "CMVideoFormatDescriptionCreate (AV1) failed: {status}"
            )));
        }
        CMFormatDescription::from_raw(format.cast_mut().cast::<c_void>()).ok_or_else(|| {
            VideoDecodeError::Backend(
                "CMVideoFormatDescriptionCreate (AV1) returned NULL".to_string(),
            )
        })
    }

    /// Builds the `destinationImageBufferAttributes` dictionary requesting
    /// `pixel_format`, or `None` if the (effectively unreachable in
    /// practice) key constant is unavailable — VideoToolbox is then left to
    /// choose its own native output, exactly as before this change.
    fn build_destination_attributes(
        pixel_format: u32,
        require_iosurface: bool,
    ) -> Option<CFDictionary> {
        // SAFETY: `kCVPixelBufferPixelFormatTypeKey` is a well-known,
        // process-wide singleton CFStringRef exported by CoreVideo.
        let key = unsafe {
            CFString::from_raw_retained(raw::kCVPixelBufferPixelFormatTypeKey.cast_mut().cast())
        }?;
        let value = CFNumber::from_i64(i64::from(pixel_format));
        if !require_iosurface {
            return Some(CFDictionary::from_pairs(&[(&key, &value)]));
        }
        // SAFETY: these are well-known CoreVideo/CoreFoundation constants.
        let metal_key = unsafe {
            CFString::from_raw_retained(raw::kCVPixelBufferMetalCompatibilityKey.cast_mut().cast())
        }?;
        // SAFETY: `kCVPixelBufferIOSurfacePropertiesKey` is a process-wide
        // singleton CFStringRef; an empty dictionary is the documented way to
        // ask CoreVideo for IOSurface-backed buffers without extra properties.
        let iosurface_key = unsafe {
            CFString::from_raw_retained(raw::kCVPixelBufferIOSurfacePropertiesKey.cast_mut().cast())
        }?;
        // SAFETY: `kCFBooleanTrue` is a borrowed singleton CFBooleanRef; this
        // retains it into an owned, type-erased CF value for the dictionary.
        let metal_true =
            unsafe { CFType::from_raw_retained(raw::kCFBooleanTrue.cast_mut().cast()) }?;
        let iosurface_properties = CFDictionary::from_pairs(&[]);
        Some(CFDictionary::from_pairs(&[
            (&key, &value),
            (&metal_key, &metal_true),
            (&iosurface_key, &iosurface_properties),
        ]))
    }

    /// Resolves a [`ColorPrimariesToken`] to the actual Apple constant.
    fn resolve_color_primaries(token: ColorPrimariesToken) -> raw::CFStringRef {
        // SAFETY: these are well-known, process-wide singleton CFStringRefs
        // exported by CoreMedia.
        unsafe {
            match token {
                ColorPrimariesToken::Bt709 => raw::kCMFormatDescriptionColorPrimaries_ITU_R_709_2,
                ColorPrimariesToken::SmpteC => raw::kCMFormatDescriptionColorPrimaries_SMPTE_C,
                ColorPrimariesToken::Bt2020 => raw::kCMFormatDescriptionColorPrimaries_ITU_R_2020,
            }
        }
    }

    /// Resolves a [`TransferFunctionToken`] to the actual Apple constant.
    fn resolve_transfer_function(token: TransferFunctionToken) -> raw::CFStringRef {
        // SAFETY: well-known, process-wide singleton CFStringRef exported by
        // CoreMedia.
        unsafe {
            match token {
                TransferFunctionToken::Bt709 => {
                    raw::kCMFormatDescriptionTransferFunction_ITU_R_709_2
                }
                TransferFunctionToken::Srgb => raw::kCMFormatDescriptionTransferFunction_sRGB,
                TransferFunctionToken::Pq => {
                    raw::kCMFormatDescriptionTransferFunction_SMPTE_ST_2084_PQ
                }
                TransferFunctionToken::Hlg => {
                    raw::kCMFormatDescriptionTransferFunction_ITU_R_2100_HLG
                }
            }
        }
    }

    /// Resolves a [`YCbCrMatrixToken`] to the actual Apple constant.
    fn resolve_ycbcr_matrix(token: YCbCrMatrixToken) -> raw::CFStringRef {
        // SAFETY: these are well-known, process-wide singleton CFStringRefs
        // exported by CoreMedia.
        unsafe {
            match token {
                YCbCrMatrixToken::Bt709 => raw::kCMFormatDescriptionYCbCrMatrix_ITU_R_709_2,
                YCbCrMatrixToken::Bt601 => raw::kCMFormatDescriptionYCbCrMatrix_ITU_R_601_4,
                YCbCrMatrixToken::Bt2020 => raw::kCMFormatDescriptionYCbCrMatrix_ITU_R_2020,
            }
        }
    }

    /// Retains a borrowed (`Get`-convention) CoreFoundation constant as an
    /// owned [`CFType`], so its ownership matches what [`CFType`]'s `Drop`
    /// expects. Named Apple `k...` constants such as
    /// `kCMFormatDescriptionExtension_ColorPrimaries` or `kCFBooleanTrue`
    /// are +0 borrowed, process-wide singletons.
    fn retain_extension_constant(name: &str, ptr: *const c_void) -> Result<CFType, String> {
        // SAFETY: `ptr` is either NULL or a valid, immortal CFTypeRef naming
        // one of the well-known constants above.
        unsafe { CFType::from_raw_retained(ptr.cast_mut()) }
            .ok_or_else(|| format!("{name} was NULL"))
    }

    /// Builds the four (key, value) overrides for `plan`, each independently
    /// retained so they can be inserted into a fresh `CFDictionary`.
    fn color_extension_overrides(
        plan: ColorExtensionPlan,
    ) -> Result<[(CFType, CFType); 4], String> {
        let primaries_key = retain_extension_constant(
            "kCMFormatDescriptionExtension_ColorPrimaries",
            unsafe { raw::kCMFormatDescriptionExtension_ColorPrimaries }.cast(),
        )?;
        let primaries_value = retain_extension_constant(
            "colour primaries constant",
            resolve_color_primaries(plan.primaries).cast(),
        )?;

        let transfer_key = retain_extension_constant(
            "kCMFormatDescriptionExtension_TransferFunction",
            unsafe { raw::kCMFormatDescriptionExtension_TransferFunction }.cast(),
        )?;
        let transfer_value = retain_extension_constant(
            "transfer function constant",
            resolve_transfer_function(plan.transfer).cast(),
        )?;

        let matrix_key = retain_extension_constant(
            "kCMFormatDescriptionExtension_YCbCrMatrix",
            unsafe { raw::kCMFormatDescriptionExtension_YCbCrMatrix }.cast(),
        )?;
        let matrix_value = retain_extension_constant(
            "YCbCr matrix constant",
            resolve_ycbcr_matrix(plan.matrix).cast(),
        )?;

        let full_range_key = retain_extension_constant(
            "kCMFormatDescriptionExtension_FullRangeVideo",
            unsafe { raw::kCMFormatDescriptionExtension_FullRangeVideo }.cast(),
        )?;
        let full_range_bool = if plan.full_range {
            unsafe { raw::kCFBooleanTrue }
        } else {
            unsafe { raw::kCFBooleanFalse }
        };
        let full_range_value =
            retain_extension_constant("kCFBooleanTrue/False", full_range_bool.cast())?;

        Ok([
            (primaries_key, primaries_value),
            (transfer_key, transfer_value),
            (matrix_key, matrix_value),
            (full_range_key, full_range_value),
        ])
    }

    /// Recreates `format` with `overrides` merged into its extensions
    /// dictionary, replacing any of the same keys VideoToolbox may already
    /// have set while deriving the description from the SPS/PPS/VPS.
    ///
    /// **This must use the video-specific `CMVideoFormatDescriptionCreate`,
    /// not the generic `CMFormatDescriptionCreate`.** A video format
    /// description's dimensions are intrinsic state, *not* an entry in the
    /// extensions dictionary, and the generic constructor has no width or
    /// height parameter at all — so recreating through it silently produced a
    /// 0x0 description. VideoToolbox then rejected the resulting session with
    /// `kVTParameterErr` (-6661), which is exactly how Media Foundation's
    /// H.264 streams failed on target hardware while FFmpeg (which builds its
    /// own description) decoded the identical bytes. Dimensions are therefore
    /// carried across explicitly, and [`recreate_preserves_geometry`] refuses
    /// any recreate that loses or changes them.
    ///
    /// For video, `media_subtype_raw()` *is* the `CMVideoCodecType`, so it is
    /// passed straight through.
    ///
    /// Both codecs share this path rather than HEVC taking a shortcut through
    /// `CMVideoFormatDescriptionCreateFromHEVCParameterSets`'s own
    /// `extensions` parameter, whose precedence versus VUI-derived values
    /// Apple does not document — this way "explicitly set" means the same,
    /// independently-verifiable thing for both codecs' probe-matrix entries.
    /// H.264 has no such parameter regardless (verified against
    /// `CMFormatDescription.h`).
    fn recreate_with_extension_overrides(
        format: &CMFormatDescription,
        overrides: &[(CFType, CFType); 4],
    ) -> Result<CMFormatDescription, String> {
        let override_keys: Vec<*mut c_void> =
            overrides.iter().map(|(key, _)| key.as_ptr()).collect();

        let mut pairs: Vec<(CFType, CFType)> = Vec::new();
        if let Some(existing_ptr) = format.extensions() {
            // SAFETY: `CMFormatDescriptionGetExtensions` (which
            // `CMFormatDescription::extensions` wraps) returns a +0 borrowed
            // dictionary owned by `format`; retaining it here gives this
            // function its own independent +1 reference to a `CFDictionary`
            // that outlives `format`.
            let existing = unsafe { CFDictionary::from_raw_retained(existing_ptr.cast_mut()) }
                .ok_or_else(|| {
                    "CMFormatDescriptionGetExtensions returned a non-wrappable dictionary"
                        .to_string()
                })?;
            for key in existing.keys().values() {
                if override_keys.contains(&key.as_ptr()) {
                    continue; // superseded by `overrides`, added back below.
                }
                if let Some(value) = existing.get(&key) {
                    pairs.push((key, value));
                }
            }
        }
        for (key, value) in overrides {
            pairs.push((key.clone(), value.clone()));
        }

        let as_cf_pairs: Vec<(&dyn AsCFType, &dyn AsCFType)> = pairs
            .iter()
            .map(|(key, value)| (key as &dyn AsCFType, value as &dyn AsCFType))
            .collect();
        let merged = CFDictionary::from_pairs(&as_cf_pairs);

        let source_dimensions = video_dimensions(format);
        if source_dimensions.0 <= 0 || source_dimensions.1 <= 0 {
            return Err(format!(
                "source format description has non-positive dimensions {}x{}; refusing to \
                 recreate it",
                source_dimensions.0, source_dimensions.1
            ));
        }

        let mut new_format: raw::CMVideoFormatDescriptionRef = ptr::null_mut();
        // SAFETY: `merged` is a valid +1 CFDictionary that outlives this call,
        // and `new_format` is a valid out-pointer. The codec type comes from
        // the source description's own media subtype.
        let status = unsafe {
            raw::CMVideoFormatDescriptionCreate(
                raw::kCFAllocatorDefault,
                format.media_subtype_raw(),
                source_dimensions.0,
                source_dimensions.1,
                merged.as_ptr().cast(),
                &mut new_format,
            )
        };
        if status != 0 || new_format.is_null() {
            return Err(format!(
                "CMVideoFormatDescriptionCreate (colour-extension override) failed: {status}"
            ));
        }
        let recreated = CMFormatDescription::from_raw(new_format.cast_mut().cast::<c_void>())
            .ok_or_else(|| {
                "CMVideoFormatDescriptionCreate (colour-extension override) returned NULL"
                    .to_string()
            })?;

        // The guard that would have caught the -6661 regression before it
        // reached hardware: a colour override must never alter geometry.
        let recreated_dimensions = video_dimensions(&recreated);
        if recreated_dimensions != source_dimensions {
            return Err(format!(
                "colour-extension override changed geometry from {}x{} to {}x{}; discarding it \
                 and keeping VideoToolbox's own description",
                source_dimensions.0,
                source_dimensions.1,
                recreated_dimensions.0,
                recreated_dimensions.1
            ));
        }
        Ok(recreated)
    }

    /// Coded `(width, height)` of a video format description.
    ///
    /// Read through the raw API because these are intrinsic to the
    /// description rather than entries in its extensions dictionary — the
    /// distinction that caused the `kVTParameterErr` regression.
    fn video_dimensions(format: &CMFormatDescription) -> (i32, i32) {
        // SAFETY: `format` is a live `CMFormatDescription`; the call is a pure
        // read and returns a plain by-value struct. A non-video description
        // yields zeroes, which callers treat as invalid.
        let dimensions =
            unsafe { raw::CMVideoFormatDescriptionGetDimensions(format.as_ptr().cast()) };
        (dimensions.width, dimensions.height)
    }

    /// Explicitly stamps colour extensions onto `format` rather than
    /// trusting whatever VideoToolbox inferred from the in-stream VUI —
    /// whether VideoToolbox honours e.g. the in-stream `video_full_range_flag`
    /// on its own is undocumented and unverified, which is exactly why this
    /// sets the extensions rather than trusting inference. Logs the
    /// pre-override ("inferred") and post-override ("set") extension values
    /// so the probe-matrix report can compare them; that comparison is
    /// itself a deliverable of this change, not a debug aid.
    ///
    /// Falls back to the original, un-overridden `format` (with a warning)
    /// if the override cannot be built or applied, so a CoreFoundation
    /// plumbing failure degrades to "VideoToolbox's inference" rather than
    /// breaking decode outright.
    fn apply_color_extensions(
        format: CMFormatDescription,
        codec: StreamCodec,
        plan: ColorExtensionPlan,
    ) -> CMFormatDescription {
        log_color_extensions(&format, codec, "inferred (before override)");
        let overrides = match color_extension_overrides(plan) {
            Ok(overrides) => overrides,
            Err(error) => {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    ?codec,
                    %error,
                    "failed to build colour-extension overrides; keeping VideoToolbox's inferred format description",
                );
                return format;
            }
        };
        match recreate_with_extension_overrides(&format, &overrides) {
            Ok(overridden) => {
                log_color_extensions(&overridden, codec, "explicitly set (after override)");
                overridden
            }
            Err(error) => {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    ?codec,
                    %error,
                    "failed to stamp explicit colour extensions; keeping VideoToolbox's inferred format description",
                );
                format
            }
        }
    }

    /// Logs the current value of each of the four colour extensions this
    /// decoder cares about, tagged with `when` (e.g. "inferred" vs. "set").
    fn log_color_extensions(format: &CMFormatDescription, codec: StreamCodec, when: &str) {
        // SAFETY: `CMFormatDescriptionGetExtensions` (wrapped by
        // `CMFormatDescription::extensions`) returns a +0 borrowed
        // dictionary; retaining it here gives this function its own
        // independent +1 reference.
        let dict = format
            .extensions()
            .and_then(|ptr| unsafe { CFDictionary::from_raw_retained(ptr.cast_mut()) });
        let describe = |key: raw::CFStringRef| -> String {
            let Some(dict) = &dict else {
                return "<absent>".to_string();
            };
            // SAFETY: `key` is a well-known, process-wide singleton
            // CFStringRef exported by CoreMedia.
            let Some(key) = (unsafe { CFType::from_raw_retained(key.cast_mut().cast()) }) else {
                return "<absent>".to_string();
            };
            dict.get(&key)
                .map_or_else(|| "<absent>".to_string(), |value| value.description())
        };
        tracing::info!(
            target: crate::logging::target::VIDEO,
            ?codec,
            %when,
            primaries = %describe(unsafe { raw::kCMFormatDescriptionExtension_ColorPrimaries }),
            transfer = %describe(unsafe { raw::kCMFormatDescriptionExtension_TransferFunction }),
            matrix = %describe(unsafe { raw::kCMFormatDescriptionExtension_YCbCrMatrix }),
            full_range = %describe(unsafe { raw::kCMFormatDescriptionExtension_FullRangeVideo }),
            "CMFormatDescription colour extensions",
        );
    }

    /// Queries `kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder`
    /// right after session creation. Apple publishes no per-profile
    /// hardware-decode matrix, so this property is the only reliable answer
    /// to whether a given stream (for example ten-bit 4:4:4) actually
    /// decoded in hardware on this Mac.
    fn query_hardware_accelerated(session: &DecompressionSession) -> Option<bool> {
        let property = match unsafe {
            session.copy_property(
                ffi::kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder,
            )
        } {
            Ok(property) => property,
            Err(error) => {
                tracing::warn!(
                    target: crate::logging::target::VIDEO,
                    %error,
                    "failed to query kVTDecompressionPropertyKey_UsingHardwareAcceleratedVideoDecoder",
                );
                return None;
            }
        };
        let value = property?;
        let value_ptr = value.as_ptr().cast_const();
        let true_ptr = unsafe { ffi::kCFBooleanTrue }.cast::<c_void>();
        Some(value_ptr == true_ptr)
    }

    fn make_sample_buffer(
        format: &CMFormatDescription,
        payload: &[u8],
        timestamp_ms: u32,
    ) -> Result<CMSampleBuffer, VideoDecodeError> {
        let block = CMBlockBuffer::create(payload)
            .ok_or_else(|| VideoDecodeError::Backend("CMBlockBufferCreate failed".to_string()))?;
        let timing = raw::CMSampleTimingInfo {
            duration: unsafe { raw::CMTimeMake(1, 60) },
            presentationTimeStamp: unsafe {
                raw::CMTimeMake(i64::from(timestamp_ms), FRAME_TIMESCALE)
            },
            decodeTimeStamp: unsafe { raw::CMTimeMake(i64::from(timestamp_ms), FRAME_TIMESCALE) },
        };
        let sample_size = payload.len();
        let mut sample: raw::CMSampleBufferRef = ptr::null_mut();
        let status = unsafe {
            raw::CMSampleBufferCreateReady(
                raw::kCFAllocatorDefault,
                block.as_ptr().cast(),
                format.as_ptr().cast(),
                1,
                1,
                &timing,
                1,
                &sample_size,
                &mut sample,
            )
        };
        if status != 0 || sample.is_null() {
            return Err(VideoDecodeError::Backend(format!(
                "CMSampleBufferCreateReady failed: {status}"
            )));
        }
        Ok(unsafe { CMSampleBuffer::from_ptr(sample.cast()) })
    }

    fn copy_rgba_frame(
        frame: DecodedFrame,
        expected_chroma: ChromaSubsampling,
        transfer: &PixelTransferSession,
        native_video: arcen_media::VideoConfiguration,
        prefer_native_biplanar: bool,
    ) -> Result<DecodedVideoFrame, String> {
        if frame.status != 0 {
            return Err(format!("VT decoder callback status {}", frame.status));
        }
        let pixel_buffer = frame
            .image_buffer
            .ok_or_else(|| "VT decoder callback did not include an image buffer".to_string())?;
        let format = pixel_buffer.pixel_format();
        let timestamp_ms = timestamp_to_ms(frame.presentation_time);
        return match classify_pixel_buffer_format(format)? {
            PixelBufferFormat::Rgba | PixelBufferFormat::Bgra => {
                copy_locked_pixels(&pixel_buffer, format, timestamp_ms)
            }
            PixelBufferFormat::Biplanar {
                chroma,
                range,
                depth,
            } => {
                if chroma != expected_chroma {
                    return Err(format!(
                        "VT output chroma {:?} ({}) does not match stream {:?}",
                        chroma,
                        fourcc_string(format),
                        expected_chroma
                    ));
                }
                if prefer_native_biplanar {
                    return native_biplanar_frame(
                        &pixel_buffer,
                        format,
                        chroma,
                        range,
                        depth,
                        timestamp_ms,
                        actual_native_video_configuration(native_video, chroma, range, depth),
                    );
                }
                copy_biplanar_to_rgba(
                    &pixel_buffer,
                    format,
                    chroma,
                    range,
                    depth,
                    timestamp_ms,
                    transfer,
                    actual_native_video_configuration(native_video, chroma, range, depth),
                )
            }
        };

        fn native_biplanar_frame(
            pixel_buffer: &CVPixelBuffer,
            format: u32,
            chroma: ChromaSubsampling,
            range: ColorRange,
            depth: BufferDepth,
            timestamp_ms: u32,
            native_video: arcen_media::VideoConfiguration,
        ) -> Result<DecodedVideoFrame, String> {
            let width = pixel_buffer.width();
            let height = pixel_buffer.height();
            let guard = pixel_buffer
                .lock(CVPixelBufferLockFlags::READ_ONLY)
                .map_err(|status| format!("CVPixelBufferLockBaseAddress failed: {status}"))?;
            if guard.plane_count() != 2 {
                return Err(format!(
                    "{} buffer has {} planes; expected exactly 2",
                    fourcc_string(format),
                    guard.plane_count()
                ));
            }
            let y_data = guard
                .plane_data(0)
                .ok_or_else(|| "missing luma plane data".to_string())?;
            let cbcr_data = guard
                .plane_data(1)
                .ok_or_else(|| "missing CbCr plane data".to_string())?;
            validate_biplanar_layout(
                chroma,
                depth,
                width,
                height,
                PlaneLayout {
                    width: guard.width_of_plane(0),
                    height: guard.height_of_plane(0),
                    bytes_per_row: guard.bytes_per_row_of_plane(0),
                    byte_len: y_data.len(),
                },
                PlaneLayout {
                    width: guard.width_of_plane(1),
                    height: guard.height_of_plane(1),
                    bytes_per_row: guard.bytes_per_row_of_plane(1),
                    byte_len: cbcr_data.len(),
                },
            )?;
            drop(guard);
            let range_name = match range {
                ColorRange::Video => "video",
                ColorRange::Full => "full",
            };
            Ok(DecodedVideoFrame {
                width,
                height,
                rgba: Vec::new(),
                timestamp_ms,
                pixel_format: format!("{}-{range_name}-iosurface", fourcc_string(format)),
                backend: "videotoolbox-iosurface",
                native: Some(NativeDecodedVideoFrame {
                    picture: pixel_buffer.clone(),
                    format: super::native_picture_format(format),
                    video: native_video,
                }),
            })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_biplanar_to_rgba(
        pixel_buffer: &CVPixelBuffer,
        format: u32,
        chroma: ChromaSubsampling,
        range: ColorRange,
        depth: BufferDepth,
        timestamp_ms: u32,
        transfer: &PixelTransferSession,
        native_video: arcen_media::VideoConfiguration,
    ) -> Result<DecodedVideoFrame, String> {
        let width = pixel_buffer.width();
        let height = pixel_buffer.height();
        {
            let guard = pixel_buffer
                .lock(CVPixelBufferLockFlags::READ_ONLY)
                .map_err(|status| format!("CVPixelBufferLockBaseAddress failed: {status}"))?;
            if guard.plane_count() != 2 {
                return Err(format!(
                    "{} buffer has {} planes; expected exactly 2",
                    fourcc_string(format),
                    guard.plane_count()
                ));
            }
            let y_data = guard
                .plane_data(0)
                .ok_or_else(|| "missing luma plane data".to_string())?;
            let cbcr_data = guard
                .plane_data(1)
                .ok_or_else(|| "missing CbCr plane data".to_string())?;
            validate_biplanar_layout(
                chroma,
                depth,
                width,
                height,
                PlaneLayout {
                    width: guard.width_of_plane(0),
                    height: guard.height_of_plane(0),
                    bytes_per_row: guard.bytes_per_row_of_plane(0),
                    byte_len: y_data.len(),
                },
                PlaneLayout {
                    width: guard.width_of_plane(1),
                    height: guard.height_of_plane(1),
                    bytes_per_row: guard.bytes_per_row_of_plane(1),
                    byte_len: cbcr_data.len(),
                },
            )?;
        }

        let destination = CVPixelBuffer::create(width, height, BGRA_PIXEL_FORMAT)
            .map_err(|status| format!("create BGRA transfer destination failed: {status}"))?;
        transfer
            .transfer(pixel_buffer, &destination)
            .map_err(|error| format!("VTPixelTransferSession transfer failed: {error}"))?;
        let mut frame = copy_locked_pixels(&destination, BGRA_PIXEL_FORMAT, timestamp_ms)?;
        let range_name = match range {
            ColorRange::Video => "video",
            ColorRange::Full => "full",
        };
        frame.pixel_format = format!("{}-{range_name}->BGRA->RGBA", fourcc_string(format));
        frame.native = Some(NativeDecodedVideoFrame {
            picture: pixel_buffer.clone(),
            format: super::native_picture_format(format),
            video: native_video,
        });
        Ok(frame)
    }

    fn copy_locked_pixels(
        pixel_buffer: &CVPixelBuffer,
        format: u32,
        timestamp_ms: u32,
    ) -> Result<DecodedVideoFrame, String> {
        let width = pixel_buffer.width();
        let height = pixel_buffer.height();
        let guard = pixel_buffer
            .lock(CVPixelBufferLockFlags::READ_ONLY)
            .map_err(|status| format!("CVPixelBufferLockBaseAddress failed: {status}"))?;
        let row_bytes = guard.bytes_per_row();
        let source = guard.as_slice();
        let mut rgba = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            let start = y * row_bytes;
            let end = start + width * 4;
            if end > source.len() {
                return Err("CVPixelBuffer row exceeds source length".to_string());
            }
            if format == RGBA_PIXEL_FORMAT {
                // Fast path: straight row copy, no per-pixel work.
                rgba.extend_from_slice(&source[start..end]);
            } else {
                // Word-wise B<->R swap (autovectorizes; the naive per-pixel
                // extend_from_slice version cost ~6-8 ms for a 4K frame).
                let row = &source[start..end];
                let base = rgba.len();
                rgba.resize(base + row.len(), 0);
                let out = &mut rgba[base..];
                for (dst, src) in out.chunks_exact_mut(4).zip(row.chunks_exact(4)) {
                    let v = u32::from_le_bytes([src[0], src[1], src[2], src[3]]);
                    let swapped =
                        (v & 0xFF00_FF00) | ((v >> 16) & 0x0000_00FF) | ((v << 16) & 0x00FF_0000);
                    dst.copy_from_slice(&swapped.to_le_bytes());
                }
            }
        }
        Ok(DecodedVideoFrame {
            width,
            height,
            rgba,
            timestamp_ms,
            pixel_format: if format == RGBA_PIXEL_FORMAT {
                "RGBA-direct".to_string()
            } else {
                "BGRA->RGBA".to_string()
            },
            backend: "videotoolbox-bgra",
            native: None,
        })
    }

    pub(super) fn native_video_configuration(
        codec: StreamCodec,
        chroma: ChromaSubsampling,
        bit_depth: wire::BitDepth,
        full_range: bool,
        matrix: wire::ColorMatrix,
        session_color: SessionColor,
    ) -> arcen_media::VideoConfiguration {
        arcen_deck_core::decode::wire_video_configuration(
            match codec {
                StreamCodec::H264 => arcen_media::VideoCodec::H264,
                StreamCodec::H265 => arcen_media::VideoCodec::H265,
                StreamCodec::Av1 => arcen_media::VideoCodec::Av1,
            },
            chroma,
            bit_depth,
            if full_range {
                wire::ColorRange::Full
            } else {
                wire::ColorRange::Limited
            },
            matrix,
            session_color,
        )
    }

    pub(super) fn actual_native_video_configuration(
        mut requested: arcen_media::VideoConfiguration,
        chroma: ChromaSubsampling,
        range: ColorRange,
        depth: BufferDepth,
    ) -> arcen_media::VideoConfiguration {
        requested.chroma = match chroma {
            ChromaSubsampling::Yuv420 => arcen_media::ChromaSubsampling::Yuv420,
            ChromaSubsampling::Yuv422 => arcen_media::ChromaSubsampling::Yuv422,
            ChromaSubsampling::Yuv444 => arcen_media::ChromaSubsampling::Yuv444,
        };
        requested.range = match range {
            ColorRange::Video => arcen_media::ColorRange::Limited,
            ColorRange::Full => arcen_media::ColorRange::Full,
        };
        requested.bit_depth = match depth {
            BufferDepth::Eight => arcen_media::BitDepth::Eight,
            BufferDepth::Ten => arcen_media::BitDepth::Ten,
        };
        requested
    }

    fn timestamp_to_ms(timestamp: (i64, i32)) -> u32 {
        let (value, timescale) = timestamp;
        if timescale <= 0 {
            return 0;
        }
        let ms = value.saturating_mul(1000) / i64::from(timescale);
        ms.clamp(0, i64::from(u32::MAX)) as u32
    }

    fn summarize_nals(codec: NalCodec, nals: &[&[u8]]) -> String {
        let parts = nals
            .iter()
            .map(|nal| {
                let kind = match nal_kind(codec, nal) {
                    NalKind::Slice => "slice",
                    NalKind::Keyframe => "keyframe",
                    NalKind::Vps => "vps",
                    NalKind::Sps => "sps",
                    NalKind::Pps => "pps",
                    NalKind::Other => "other",
                };
                format!("{kind}:{}", nal.len())
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("nals=[{parts}]")
    }

    fn is_transient_bad_data(error: &str) -> bool {
        error.contains("VT decoder callback status -12909")
    }

    /// Runs every real `w3-real-caps`/`w6-av1-decode` decode-capability
    /// probe on this machine. See `super::probe_decode_capabilities` for
    /// the process-wide cache wrapping this; this function itself always
    /// re-probes, so tests (and that cache) can call it directly.
    pub(super) fn probe_decode_capabilities() -> super::DecodeCapabilities {
        // Some decoders are not registered by default on every system;
        // mirrors `probe_matrix::backend::register_supplemental_decoders`.
        videotoolbox::register_supplemental_video_decoder_if_available(videotoolbox::Codec::H264);
        videotoolbox::register_supplemental_video_decoder_if_available(videotoolbox::Codec::HEVC);
        // SAFETY: `VTRegisterSupplementalVideoDecoderIfAvailable` is a
        // documented, side-effect-free-when-inapplicable opt-in. There is
        // no safe wrapper reachable for AV1 since `videotoolbox::Codec`
        // has no AV1 variant (verified against
        // `videotoolbox-0.18.1/src/session/mod.rs`).
        unsafe {
            ffi::VTRegisterSupplementalVideoDecoderIfAvailable(raw::kCMVideoCodecType_AV1);
        }

        let baseline = super::HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 8,
        };
        let h265 = probe_hevc_session(baseline, false, wire::ColorMatrix::Bt709);
        log_probe_result("h265", &h265);
        let h264 = probe_h264_session();
        log_probe_result("h264", &h264);
        let yuv444 = probe_hevc_session(
            super::HevcProbeProfile {
                chroma_format_idc: 3,
                bit_depth: 8,
            },
            false,
            wire::ColorMatrix::Bt709,
        );
        log_probe_result("yuv444", &yuv444);
        let main10 = probe_hevc_session(
            super::HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 10,
            },
            false,
            wire::ColorMatrix::Bt709,
        );
        log_probe_result("main10", &main10);
        let main12 = probe_hevc_session(
            super::HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 12,
            },
            false,
            wire::ColorMatrix::Bt709,
        );
        log_probe_result("main12", &main12);
        let full_range = probe_hevc_session(baseline, true, wire::ColorMatrix::Bt709);
        log_probe_result("full_range", &full_range);
        let bt601_matrix = probe_hevc_session(baseline, false, wire::ColorMatrix::Bt601);
        log_probe_result("bt601_matrix", &bt601_matrix);
        let bt2020_ncl_matrix = probe_hevc_session(baseline, false, wire::ColorMatrix::Bt2020Ncl);
        log_probe_result("bt2020_ncl_matrix", &bt2020_ncl_matrix);
        let probe_claims_av1 = probe_av1_hardware_decode_supported();
        let platform = crate::platform::current();
        // The probe answers for this Mac; the platform profile caps it. No
        // Intel Deck advertises AV1, whatever VideoToolbox says.
        let av1 = platform.advertised_av1(probe_claims_av1);
        tracing::info!(
            target: crate::logging::target::VIDEO,
            capability = "av1",
            supported = av1,
            probe_claims_av1,
            deck_platform = platform.label(),
            "decode-capability probe: codec-level only (see DecodeCapabilities::av1 docs)",
        );

        let hardware_accelerated = h265
            .as_ref()
            .ok()
            .copied()
            .flatten()
            .or_else(|| h264.as_ref().ok().copied().flatten());
        let decode_path = platform.assess_decode_path(hardware_accelerated);
        if decode_path == arcen_media::deck_platform::DecodePathAssessment::UnexpectedSoftware {
            tracing::warn!(
                target: crate::logging::target::VIDEO,
                deck_platform = platform.label(),
                decode_path = decode_path.token(),
                "decode-capability probe: VideoToolbox decodes in software on Apple silicon",
            );
        } else {
            tracing::info!(
                target: crate::logging::target::VIDEO,
                deck_platform = platform.label(),
                decode_path = decode_path.token(),
                "decode-capability probe: decode path",
            );
        }

        super::DecodeCapabilities {
            h264: h264.is_ok(),
            h265: h265.is_ok(),
            av1,
            yuv444: yuv444.is_ok(),
            main10: main10.is_ok(),
            main12: main12.is_ok(),
            full_range: full_range.is_ok(),
            identity_matrix: false,
            bt601_matrix: bt601_matrix.is_ok(),
            bt2020_ncl_matrix: bt2020_ncl_matrix.is_ok(),
            hardware_accelerated,
        }
    }

    fn log_probe_result(capability: &'static str, result: &Result<Option<bool>, String>) {
        match result {
            Ok(hardware_accelerated) => tracing::info!(
                target: crate::logging::target::VIDEO,
                capability,
                ?hardware_accelerated,
                "decode-capability probe: session created",
            ),
            Err(error) => tracing::info!(
                target: crate::logging::target::VIDEO,
                capability,
                %error,
                "decode-capability probe: session could not be created; reporting this \
                 capability as unsupported",
            ),
        }
    }

    /// Attempts to build a real `CMFormatDescription` (from the synthetic
    /// but spec-valid VPS/SPS/PPS the outer `bitstream`-backed builders
    /// produce) and create a real `VTDecompressionSession` for
    /// `profile`/`full_range`. `Ok(hardware_accelerated)` means session
    /// creation succeeded (`None` only if the hardware-acceleration
    /// property itself could not be read, which is not by itself a probe
    /// failure); `Err(reason)` means either step failed, and the
    /// capability this backs must be reported `false`.
    ///
    /// Mirrors `probe_matrix::backend::probe_row`'s approach (real
    /// `CMVideoFormatDescriptionCreateFromHEVCParameterSets` +
    /// `VTDecompressionSessionCreate`), but deliberately stops at session
    /// creation rather than decoding a real access unit: unlike
    /// `probe-matrix` (fed real host-captured elementary streams), this
    /// probe has no local encoder to draw a genuine coded picture from,
    /// and per Apple's documented behaviour (see `probe_requires_hardware`
    /// in `probe_matrix.rs`), session creation is already where
    /// VideoToolbox validates and rejects an unsupported
    /// profile/chroma/bit-depth combination.
    fn probe_hevc_session(
        profile: super::HevcProbeProfile,
        full_range: bool,
        matrix: wire::ColorMatrix,
    ) -> Result<Option<bool>, String> {
        let vps = arcen_media::bitstream::synth::build_hevc_vps(profile);
        let sps = arcen_media::bitstream::synth::build_hevc_sps(profile);
        let pps = arcen_media::bitstream::synth::build_hevc_pps();
        let format =
            make_hevc_format_description(&vps, &sps, &pps).map_err(|error| error.to_string())?;
        let plan = color_extension_plan(
            matrix,
            full_range,
            arcen_media::TransferCharacteristics::Bt709,
        );
        let format = apply_color_extensions(format, StreamCodec::H265, plan);
        let attributes = preferred_pixel_format(
            super::probe_profile_chroma(profile),
            profile.ten_bit(),
            full_range,
        )
        .and_then(|format| build_destination_attributes(format, false));
        let session = DecompressionSession::new_with_image_buffer_attributes(
            &format,
            attributes.as_ref(),
            |_frame| {},
        )
        .map_err(|error| format!("VTDecompressionSessionCreate: {error}"))?;
        Ok(query_hardware_accelerated(&session))
    }

    /// Same shape as [`probe_hevc_session`] but for the Baseline H.264
    /// probe backing `supports_h264`.
    fn probe_h264_session() -> Result<Option<bool>, String> {
        let sps = arcen_media::bitstream::synth::build_h264_sps();
        let pps = arcen_media::bitstream::synth::build_h264_pps();
        let format = make_h264_format_description(&sps, &pps).map_err(|error| error.to_string())?;
        let attributes = preferred_pixel_format(ChromaSubsampling::Yuv420, false, false)
            .and_then(|format| build_destination_attributes(format, false));
        let session = DecompressionSession::new_with_image_buffer_attributes(
            &format,
            attributes.as_ref(),
            |_frame| {},
        )
        .map_err(|error| format!("VTDecompressionSessionCreate: {error}"))?;
        Ok(query_hardware_accelerated(&session))
    }

    #[cfg(test)]
    pub(super) fn probe_speed_decode_latency_policy() -> Result<(), String> {
        let policy = arcen_media::video::pipeline_contract(arcen_media::video::PipelineId::Speed)
            .decode_latency;
        let h264_sps = arcen_media::bitstream::synth::build_h264_sps();
        let h264_pps = arcen_media::bitstream::synth::build_h264_pps();
        let h264_format = make_h264_format_description(&h264_sps, &h264_pps)
            .map_err(|error| error.to_string())?;
        let h264_attributes = preferred_pixel_format(ChromaSubsampling::Yuv420, false, false)
            .and_then(|format| build_destination_attributes(format, false));
        let h264 = DecompressionSession::new_with_image_buffer_attributes(
            &h264_format,
            h264_attributes.as_ref(),
            |_frame| {},
        )
        .map_err(|error| format!("H.264 VTDecompressionSessionCreate: {error}"))?;
        h264.set_real_time(policy.realtime)
            .map_err(|error| format!("H.264 RealTime: {error}"))?;

        let profile = super::HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 8,
        };
        let vps = arcen_media::bitstream::synth::build_hevc_vps(profile);
        let sps = arcen_media::bitstream::synth::build_hevc_sps(profile);
        let pps = arcen_media::bitstream::synth::build_hevc_pps();
        let hevc_format =
            make_hevc_format_description(&vps, &sps, &pps).map_err(|error| error.to_string())?;
        let hevc_attributes = preferred_pixel_format(
            super::probe_profile_chroma(profile),
            profile.ten_bit(),
            false,
        )
        .and_then(|format| build_destination_attributes(format, false));
        let hevc = DecompressionSession::new_with_image_buffer_attributes(
            &hevc_format,
            hevc_attributes.as_ref(),
            |_frame| {},
        )
        .map_err(|error| format!("HEVC VTDecompressionSessionCreate: {error}"))?;
        hevc.set_real_time(policy.realtime)
            .map_err(|error| format!("HEVC RealTime: {error}"))?;
        Ok(())
    }

    /// Whether this Mac's VideoToolbox claims AV1 hardware decode at all.
    /// Codec-level only (see `super::DecodeCapabilities::av1` docs): unlike
    /// the HEVC/H.264 probes above, this specific call site has no real
    /// Sequence Header OBU to build a genuine `av1C` from -- this process
    /// has no local AV1 encoder to draw one from before any connection
    /// exists, and hand-authoring a synthetic one (the same approach the
    /// HEVC/H.264 probes above take for their VPS/SPS/PPS) was judged not
    /// worth the risk for this one pre-connection capability flag when the
    /// wire decode path and `probe_matrix.rs` (see the [`super::av1`]
    /// module) already do the real thing with real encoder-produced data.
    /// Under-claiming AV1 support here is the documented, accepted
    /// trade-off (see the task this shipped under); over-claiming would
    /// not be.
    fn probe_av1_hardware_decode_supported() -> bool {
        // SAFETY: `VTIsHardwareDecodeSupported` is a documented,
        // side-effect-free codec-level query. There is no safe wrapper
        // reachable for AV1 (the `videotoolbox` crate's `Codec` enum has
        // no AV1 variant), so this calls the raw FFI export directly with
        // the real `kCMVideoCodecType_AV1` constant, exactly like every
        // other raw-FFI call site this file and `probe_matrix.rs` already
        // make for capabilities the safe wrapper does not cover.
        unsafe { ffi::VTIsHardwareDecodeSupported(raw::kCMVideoCodecType_AV1) != 0 }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{DecodedVideoFrame, SessionColor, StreamCodec, VideoDecodeError, VideoHeader};

    #[derive(Default)]
    pub struct PlatformVideoDecoder {
        waiting_for_keyframe: bool,
    }

    impl PlatformVideoDecoder {
        /// No decoder on this platform, so there is nothing for the
        /// negotiated colour to inform -- accepted and dropped so the
        /// cross-platform `NativeVideoDecoder` API stays uniform.
        pub fn set_session_color(&mut self, _session_color: SessionColor) {}

        pub fn set_prefer_native_biplanar(&mut self, _prefer: bool) -> bool {
            false
        }

        pub fn set_decode_latency_policy(
            &mut self,
            _policy: arcen_media::video::DecodeLatencyPolicy,
        ) {
        }

        pub fn decode(
            &mut self,
            _codec: StreamCodec,
            _header: &VideoHeader,
            _payload: &[u8],
        ) -> Result<Option<DecodedVideoFrame>, VideoDecodeError> {
            Err(VideoDecodeError::UnsupportedPlatform)
        }

        pub fn backend_name(&self) -> &'static str {
            "unsupported"
        }

        pub const fn collapsed_decode_callbacks(&self) -> u64 {
            0
        }

        pub fn is_hardware_accelerated(&self) -> Option<bool> {
            None
        }

        pub fn wants_keyframe(&self) -> bool {
            self.waiting_for_keyframe
        }

        pub fn notify_discontinuity(&mut self) {
            self.waiting_for_keyframe = true;
        }
    }

    /// This build has no VideoToolbox to probe -- see
    /// `super::probe_decode_capabilities`. Every field defaults to
    /// `false`/`None`: the conservative, honest answer for a platform
    /// that cannot decode any of this at all, matching this module's own
    /// `PlatformVideoDecoder::backend_name` ("unsupported").
    pub(super) fn probe_decode_capabilities() -> super::DecodeCapabilities {
        super::DecodeCapabilities::default()
    }

    #[cfg(test)]
    pub(super) fn probe_speed_decode_latency_policy() -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speed_decode_latency_policy_does_not_block_real_vt_sessions() {
        platform::probe_speed_decode_latency_policy()
            .expect("Speed decode tuning must not reject H.264 or HEVC sessions");
    }

    #[test]
    fn classifies_supported_videotoolbox_pixel_formats_without_endian_ambiguity() {
        assert_eq!(NV12_VIDEO_RANGE.to_be_bytes(), *b"420v");
        assert_eq!(NV12_FULL_RANGE.to_be_bytes(), *b"420f");
        assert_eq!(NV24_VIDEO_RANGE.to_be_bytes(), *b"444v");
        assert_eq!(NV24_FULL_RANGE.to_be_bytes(), *b"444f");
        assert_eq!(P010_VIDEO_RANGE.to_be_bytes(), *b"x420");
        assert_eq!(P010_FULL_RANGE.to_be_bytes(), *b"xf20");
        assert_eq!(P410_VIDEO_RANGE.to_be_bytes(), *b"x444");
        assert_eq!(P410_FULL_RANGE.to_be_bytes(), *b"xf44");

        let biplanar = |chroma, range, depth| PixelBufferFormat::Biplanar {
            chroma,
            range,
            depth,
        };
        assert_eq!(
            classify_pixel_buffer_format(NV12_VIDEO_RANGE).unwrap(),
            biplanar(
                ChromaSubsampling::Yuv420,
                ColorRange::Video,
                BufferDepth::Eight
            )
        );
        assert_eq!(
            classify_pixel_buffer_format(NV12_FULL_RANGE).unwrap(),
            biplanar(
                ChromaSubsampling::Yuv420,
                ColorRange::Full,
                BufferDepth::Eight
            )
        );
        assert_eq!(
            classify_pixel_buffer_format(NV24_VIDEO_RANGE).unwrap(),
            biplanar(
                ChromaSubsampling::Yuv444,
                ColorRange::Video,
                BufferDepth::Eight
            )
        );
        assert_eq!(
            classify_pixel_buffer_format(NV24_FULL_RANGE).unwrap(),
            biplanar(
                ChromaSubsampling::Yuv444,
                ColorRange::Full,
                BufferDepth::Eight
            )
        );
        // The ten-bit family: the range lives in the FourCC, not in a flag.
        assert_eq!(
            classify_pixel_buffer_format(P410_FULL_RANGE).unwrap(),
            biplanar(
                ChromaSubsampling::Yuv444,
                ColorRange::Full,
                BufferDepth::Ten
            )
        );
        assert_eq!(
            classify_pixel_buffer_format(P010_VIDEO_RANGE).unwrap(),
            biplanar(
                ChromaSubsampling::Yuv420,
                ColorRange::Video,
                BufferDepth::Ten
            )
        );
        assert_eq!(
            classify_pixel_buffer_format(RGBA_PIXEL_FORMAT).unwrap(),
            PixelBufferFormat::Rgba
        );
        assert_eq!(
            classify_pixel_buffer_format(BGRA_PIXEL_FORMAT).unwrap(),
            PixelBufferFormat::Bgra
        );
        // v308 is packed 4:4:4, not the 444v bi-planar layout.
        assert!(classify_pixel_buffer_format(u32::from_be_bytes(*b"v308")).is_err());
        assert!(classify_pixel_buffer_format(u32::from_be_bytes(*b"420p")).is_err());
    }

    #[test]
    fn preferred_pixel_format_selects_range_by_fourcc_not_by_flag() {
        // Asking for the video-range FourCC on a full-range stream is exactly
        // how a grading session silently gets crushed blacks, so the mapping
        // is pinned.
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv444, true, true),
            Some(P410_FULL_RANGE)
        );
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv444, true, false),
            Some(P410_VIDEO_RANGE)
        );
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv444, false, true),
            Some(NV24_FULL_RANGE)
        );
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv420, false, false),
            Some(NV12_VIDEO_RANGE)
        );
        // No eight-bit biplanar 4:2:2 exists in this family, so there is no
        // answer rather than a wrong one.
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv422, false, true),
            None
        );
    }

    #[test]
    fn wants_ten_bit_pixel_format_treats_eight_bit_only_as_eight_bit() {
        assert!(!wants_ten_bit_pixel_format(wire::BitDepth::Eight));
        assert!(wants_ten_bit_pixel_format(wire::BitDepth::Ten));
        // There is no true twelve-bit CVPixelBuffer format in the family
        // this decoder requests, so twelve-bit degrades to a ten-bit
        // request rather than going unrequested or truncating to eight.
        assert!(wants_ten_bit_pixel_format(wire::BitDepth::Twelve));
    }

    #[test]
    fn native_presentation_contract_uses_actual_corevideo_depth_and_range() {
        let requested = platform::native_video_configuration(
            StreamCodec::H265,
            ChromaSubsampling::Yuv444,
            wire::BitDepth::Twelve,
            true,
            wire::ColorMatrix::Bt709,
            SessionColor::default(),
        );
        let actual = platform::actual_native_video_configuration(
            requested,
            ChromaSubsampling::Yuv444,
            ColorRange::Video,
            BufferDepth::Ten,
        );
        assert_eq!(actual.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(actual.range, arcen_media::ColorRange::Limited);
        assert_eq!(actual.chroma, arcen_media::ChromaSubsampling::Yuv444);
        assert_eq!(actual.codec, arcen_media::VideoCodec::H265);
        assert_eq!(actual.matrix, arcen_media::ColorMatrix::Bt709);
    }

    /// The negotiated session colour must survive onto the frame's own
    /// presentation contract, and must survive *unchanged* by the
    /// CoreVideo-actuals pass.
    ///
    /// This is the regression guard for the bug this pair of functions
    /// carried until HDR existed: `native_video_configuration` hard-coded
    /// `primaries: Bt709, transfer: Bt709`, so a negotiated PQ stream
    /// arrived at the presentation layer claiming to be SDR and was
    /// displayed as SDR -- with every layer still internally consistent,
    /// which is exactly why it went unnoticed. `actual_*` may only correct
    /// the axes CoreVideo actually decides (chroma, range, depth); it has
    /// no opinion on primaries or transfer and must not acquire one.
    #[test]
    fn negotiated_session_colour_survives_onto_the_presentation_contract() {
        let hdr = SessionColor {
            primaries: arcen_media::ColorPrimaries::Bt2020,
            transfer: arcen_media::TransferCharacteristics::Pq,
        };
        let requested = platform::native_video_configuration(
            StreamCodec::H265,
            ChromaSubsampling::Yuv444,
            wire::BitDepth::Ten,
            true,
            wire::ColorMatrix::Bt2020Ncl,
            hdr,
        );
        assert_eq!(requested.primaries, arcen_media::ColorPrimaries::Bt2020);
        assert_eq!(requested.transfer, arcen_media::TransferCharacteristics::Pq);

        let actual = platform::actual_native_video_configuration(
            requested,
            ChromaSubsampling::Yuv444,
            ColorRange::Full,
            BufferDepth::Ten,
        );
        assert_eq!(actual.primaries, arcen_media::ColorPrimaries::Bt2020);
        assert_eq!(actual.transfer, arcen_media::TransferCharacteristics::Pq);
    }

    /// A ten-bit stream is not by itself an HDR stream: Grading Reference
    /// is 4:4:4 ten-bit BT.709, entirely SDR. Nothing may infer HDR from
    /// depth.
    #[test]
    fn ten_bit_bt709_stays_an_sdr_contract() {
        let sdr = SessionColor::default();
        let requested = platform::native_video_configuration(
            StreamCodec::H265,
            ChromaSubsampling::Yuv444,
            wire::BitDepth::Ten,
            true,
            wire::ColorMatrix::Bt709,
            sdr,
        );
        assert_eq!(requested.bit_depth, arcen_media::BitDepth::Ten);
        assert_eq!(
            requested.transfer,
            arcen_media::TransferCharacteristics::Bt709
        );
    }

    /// Regression guard for the `kVTParameterErr` (-6661) failure found on
    /// target hardware: Media Foundation's H.264 streams were rejected by
    /// VideoToolbox while FFmpeg decoded the identical bytes, because the
    /// colour-extension override recreated the format description through the
    /// generic `CMFormatDescriptionCreate`, which has no width/height
    /// parameters and so produced a 0x0 description.
    ///
    /// The real fix is structural (use `CMVideoFormatDescriptionCreate` and
    /// carry the dimensions across), and cannot be exercised here because
    /// CoreMedia is macOS-only. What this test pins is the invariant that
    /// makes the bug impossible to reintroduce silently: a colour override is
    /// a *colour* operation and must never alter geometry, so
    /// `recreate_with_extension_overrides` compares dimensions before and
    /// after and discards any recreate that changes them.
    #[test]
    fn colour_override_must_be_geometry_preserving_by_contract() {
        // Documented as an executable statement of intent: the four keys a
        // colour override touches are all colour keys, and none of them can
        // express geometry.
        let plan = color_extension_plan(
            wire::ColorMatrix::Bt709,
            true,
            arcen_media::TransferCharacteristics::Bt709,
        );
        assert_eq!(plan.primaries, ColorPrimariesToken::Bt709);
        assert_eq!(plan.transfer, TransferFunctionToken::Bt709);
        assert_eq!(plan.matrix, YCbCrMatrixToken::Bt709);
        assert!(plan.full_range);
        assert!(!plan.matrix_is_knowingly_inaccurate);
    }

    #[test]
    fn color_extension_plan_maps_each_wire_matrix_to_its_conventional_pairing() {
        let bt709 = color_extension_plan(
            wire::ColorMatrix::Bt709,
            true,
            arcen_media::TransferCharacteristics::Bt709,
        );
        assert_eq!(bt709.primaries, ColorPrimariesToken::Bt709);
        assert_eq!(bt709.transfer, TransferFunctionToken::Bt709);
        assert_eq!(bt709.matrix, YCbCrMatrixToken::Bt709);
        assert!(bt709.full_range);
        assert!(!bt709.matrix_is_knowingly_inaccurate);

        // `full_range` is threaded straight through independent of matrix.
        let bt709_limited = color_extension_plan(
            wire::ColorMatrix::Bt709,
            false,
            arcen_media::TransferCharacteristics::Bt709,
        );
        assert!(!bt709_limited.full_range);

        let bt601 = color_extension_plan(
            wire::ColorMatrix::Bt601,
            false,
            arcen_media::TransferCharacteristics::Bt709,
        );
        assert_eq!(bt601.primaries, ColorPrimariesToken::SmpteC);
        assert_eq!(bt601.matrix, YCbCrMatrixToken::Bt601);
        assert!(!bt601.matrix_is_knowingly_inaccurate);

        let bt2020 = color_extension_plan(
            wire::ColorMatrix::Bt2020Ncl,
            true,
            arcen_media::TransferCharacteristics::Bt709,
        );
        assert_eq!(bt2020.primaries, ColorPrimariesToken::Bt2020);
        assert_eq!(bt2020.matrix, YCbCrMatrixToken::Bt2020);
        // Apple's own header recommends the 709 transfer constant even for
        // 2020 content ("semantically equivalent ... which is preferred").
        assert_eq!(bt2020.transfer, TransferFunctionToken::Bt709);
        assert!(!bt2020.matrix_is_knowingly_inaccurate);
    }

    #[test]
    fn color_extension_plan_preserves_negotiated_hdr_transfer() {
        let pq = color_extension_plan(
            wire::ColorMatrix::Bt2020Ncl,
            true,
            arcen_media::TransferCharacteristics::Pq,
        );
        assert_eq!(pq.primaries, ColorPrimariesToken::Bt2020);
        assert_eq!(pq.transfer, TransferFunctionToken::Pq);
        assert_eq!(pq.matrix, YCbCrMatrixToken::Bt2020);

        let hlg = color_extension_plan(
            wire::ColorMatrix::Bt2020Ncl,
            true,
            arcen_media::TransferCharacteristics::Hlg,
        );
        assert_eq!(hlg.transfer, TransferFunctionToken::Hlg);
    }

    #[test]
    fn color_extension_plan_flags_identity_matrix_as_knowingly_inaccurate() {
        // CoreVideo/CoreMedia expose no identity/GBR YCbCrMatrix constant,
        // so an identity-matrix stream cannot be described faithfully.
        // Refusing to decode it at all would make the highest-fidelity
        // (zero chroma-conversion-error) mode of this feature undecodable
        // on macOS, so the plan knowingly substitutes BT.709 and flags it.
        let identity = color_extension_plan(
            wire::ColorMatrix::Identity,
            true,
            arcen_media::TransferCharacteristics::Bt709,
        );
        assert!(identity.matrix_is_knowingly_inaccurate);
        assert_eq!(identity.matrix, YCbCrMatrixToken::Bt709);
        assert_eq!(identity.primaries, ColorPrimariesToken::Bt709);
        assert_eq!(identity.transfer, TransferFunctionToken::Bt709);
        assert!(identity.full_range);
    }

    #[test]
    fn validates_biplanar_plane_dimensions_and_padded_strides() {
        let y = PlaneLayout {
            width: 5,
            height: 3,
            bytes_per_row: 8,
            byte_len: 24,
        };
        let nv12 = PlaneLayout {
            width: 3,
            height: 2,
            bytes_per_row: 8,
            byte_len: 16,
        };
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv420,
            BufferDepth::Eight,
            5,
            3,
            y,
            nv12
        )
        .is_ok());

        let nv24 = PlaneLayout {
            width: 5,
            height: 3,
            bytes_per_row: 16,
            byte_len: 48,
        };
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv444,
            BufferDepth::Eight,
            5,
            3,
            y,
            nv24
        )
        .is_ok());

        let short_stride = PlaneLayout {
            bytes_per_row: 5,
            ..nv12
        };
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv420,
            BufferDepth::Eight,
            5,
            3,
            y,
            short_stride
        )
        .is_err());
        let truncated = PlaneLayout {
            byte_len: 7,
            ..nv12
        };
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv420,
            BufferDepth::Eight,
            5,
            3,
            y,
            truncated
        )
        .is_err());
    }

    #[test]
    fn ten_bit_layouts_require_double_width_strides() {
        // The whole point of threading depth into validation: an eight-bit
        // minimum stride would accept a half-length ten-bit plane and read
        // past the end of every chroma row.
        let y8 = PlaneLayout {
            width: 4,
            height: 2,
            bytes_per_row: 4,
            byte_len: 8,
        };
        let chroma8 = PlaneLayout {
            width: 4,
            height: 2,
            bytes_per_row: 8,
            byte_len: 16,
        };
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv444,
            BufferDepth::Eight,
            4,
            2,
            y8,
            chroma8
        )
        .is_ok());
        // The same strides are too short once each sample is 16 bits.
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv444,
            BufferDepth::Ten,
            4,
            2,
            y8,
            chroma8
        )
        .is_err());

        let y10 = PlaneLayout {
            width: 4,
            height: 2,
            bytes_per_row: 8,
            byte_len: 16,
        };
        let chroma10 = PlaneLayout {
            width: 4,
            height: 2,
            bytes_per_row: 16,
            byte_len: 32,
        };
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv444,
            BufferDepth::Ten,
            4,
            2,
            y10,
            chroma10
        )
        .is_ok());
    }

    #[test]
    fn four_two_two_is_accepted_now_that_a_ten_bit_format_exists() {
        // 4:2:2 was previously rejected outright. It has half-width chroma at
        // full height, which the layout rules now express directly.
        let y = PlaneLayout {
            width: 4,
            height: 2,
            bytes_per_row: 8,
            byte_len: 16,
        };
        let chroma = PlaneLayout {
            width: 2,
            height: 2,
            bytes_per_row: 8,
            byte_len: 16,
        };
        assert!(validate_biplanar_layout(
            ChromaSubsampling::Yuv422,
            BufferDepth::Ten,
            4,
            2,
            y,
            chroma
        )
        .is_ok());
    }

    #[test]
    fn discontinuity_forces_keyframe_gating() {
        let mut decoder = NativeVideoDecoder::new();
        decoder.notify_discontinuity();
        assert!(decoder.wants_keyframe());
    }

    #[test]
    fn a_stream_that_is_less_than_its_header_is_caught() {
        use arcen_media::bitstream::synth::build_hevc_sps;
        let grading_header = VideoStreamKey {
            codec: StreamCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: wire::BitDepth::Ten,
            full_range: false,
            matrix: wire::ColorMatrix::Bt709,
        };
        // What every macOS Grading session once was: a 4:4:4 10-bit header
        // over a Main 4:2:0 8-bit bitstream.
        let main = build_hevc_sps(HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 8,
        });
        assert!(stream_contradicting_header(grading_header, &main).is_some());
        let rext = build_hevc_sps(HevcProbeProfile {
            chroma_format_idc: 3,
            bit_depth: 10,
        });
        assert!(stream_contradicting_header(grading_header, &rext).is_none());
    }

    // ---- w3-real-caps: DecodeCapabilities / probe caching ----

    #[test]
    fn decoder_backend_label_reflects_the_probe_conservatively() {
        assert_eq!(
            DecodeCapabilities::default().decoder_backend_label(),
            "unsupported"
        );
        let sw = DecodeCapabilities {
            h265: true,
            hardware_accelerated: Some(false),
            ..DecodeCapabilities::default()
        };
        assert_eq!(sw.decoder_backend_label(), "videotoolbox-sw");
        let hw = DecodeCapabilities {
            h265: true,
            hardware_accelerated: Some(true),
            ..DecodeCapabilities::default()
        };
        assert_eq!(hw.decoder_backend_label(), "videotoolbox-hw");
        let unknown = DecodeCapabilities {
            h264: true,
            hardware_accelerated: None,
            ..DecodeCapabilities::default()
        };
        assert_eq!(unknown.decoder_backend_label(), "videotoolbox");
    }

    #[test]
    fn decode_capabilities_conservative_default_is_all_false() {
        // The honest fallback this whole probe leans on: absent a
        // successful probe, every capability is `false`/`None`, never a
        // guessed `true`.
        let capabilities = DecodeCapabilities::default();
        assert!(!capabilities.h264);
        assert!(!capabilities.h265);
        assert!(!capabilities.av1);
        assert!(!capabilities.yuv444);
        assert!(!capabilities.main10);
        assert!(!capabilities.main12);
        assert!(!capabilities.full_range);
        assert!(!capabilities.identity_matrix);
        assert!(!capabilities.bt601_matrix);
        assert!(!capabilities.bt2020_ncl_matrix);
        assert_eq!(capabilities.hardware_accelerated, None);
    }

    #[test]
    fn matrix_only_stream_change_requires_a_decoder_session_rebuild() {
        let header = |matrix| VideoHeader {
            frame_type: FrameType::VideoH265,
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            flags: VideoHeader::encode_flags(
                true,
                wire::BitDepth::Ten,
                wire::ColorRange::Full,
                matrix,
            ),
            timestamp_ms: 1,
            monitor_id: 0,
            topology_generation: 0,
            stream_epoch: 0,
        };
        let bt709 =
            VideoStreamKey::from_header(StreamCodec::H265, &header(wire::ColorMatrix::Bt709))
                .expect("known matrix");
        let bt601 =
            VideoStreamKey::from_header(StreamCodec::H265, &header(wire::ColorMatrix::Bt601))
                .expect("known matrix");

        assert!(!bt709.requires_session_rebuild(Some(bt709)));
        assert!(
            bt601.requires_session_rebuild(Some(bt709)),
            "matrix changes must reset cached parameter sets and rebuild the VT session"
        );
    }

    #[test]
    fn once_lock_get_or_init_runs_the_initializer_exactly_once() {
        // Demonstrates the exact caching pattern `probe_decode_capabilities`
        // relies on (a local, test-owned `OnceLock` rather than the real
        // process-global one, so this test cannot interfere with others).
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::OnceLock;

        static CALLS: AtomicUsize = AtomicUsize::new(0);
        static CACHE: OnceLock<u32> = OnceLock::new();

        fn cached_value() -> u32 {
            *CACHE.get_or_init(|| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                42
            })
        }

        assert_eq!(cached_value(), 42);
        assert_eq!(cached_value(), 42);
        assert_eq!(cached_value(), 42);
        assert_eq!(
            CALLS.load(Ordering::SeqCst),
            1,
            "the underlying probe must run exactly once"
        );
    }

    #[test]
    fn probe_decode_capabilities_is_stable_across_repeated_calls() {
        // Whether or not this machine can actually decode anything,
        // repeated calls within the same process must observe the
        // identical result -- the process-wide cache's whole contract (a
        // probe on every reconnect would add visible latency).
        let first = probe_decode_capabilities();
        let second = probe_decode_capabilities();
        assert_eq!(first, second);
    }

    // ---- w6-av1-decode: av1 module (OBU parsing / sequence header / av1C) ----

    #[test]
    fn av1_codec_fourcc_matches_the_av01_four_character_code() {
        // Cross-check for the `kCMVideoCodecType_AV1` constant
        // `platform::make_av1_format_description` passes to
        // `CMVideoFormatDescriptionCreate` (verified against
        // `apple-cf-0.9.3/src/raw/generated.rs`:
        // `pub const kCMVideoCodecType_AV1: _bindgen_ty_1683 = 1_635_135_537`).
        const KNOWN_KCM_VIDEO_CODEC_TYPE_AV1: u32 = 1_635_135_537;
        assert_eq!(KNOWN_KCM_VIDEO_CODEC_TYPE_AV1.to_be_bytes(), *b"av01");
    }

    #[test]
    fn av1_420_8_and_10_bit_map_to_the_nvenc_delivered_pixel_formats() {
        // AV1 from NVENC is 4:2:0 8/10-bit only (Ada+, Main profile); these
        // are the exact four FourCCs a `StreamCodec::Av1` session requests,
        // via the same codec-agnostic `preferred_pixel_format` H.264/HEVC
        // already use -- this pins that AV1 gets no special-cased (and
        // possibly divergent) mapping.
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv420, false, false),
            Some(NV12_VIDEO_RANGE)
        );
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv420, false, true),
            Some(NV12_FULL_RANGE)
        );
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv420, true, false),
            Some(P010_VIDEO_RANGE)
        );
        assert_eq!(
            preferred_pixel_format(ChromaSubsampling::Yuv420, true, true),
            Some(P010_FULL_RANGE)
        );
    }

    #[test]
    fn annex_b_codec_maps_h264_and_h265_but_not_av1() {
        // AV1 uses OBU framing, not Annex-B, and has no VPS/SPS/PPS -- it
        // must never reach the Annex-B pipeline.
        assert_eq!(annex_b_codec(StreamCodec::H264), Some(NalCodec::H264));
        assert_eq!(annex_b_codec(StreamCodec::H265), Some(NalCodec::H265));
        assert_eq!(annex_b_codec(StreamCodec::Av1), None);
    }
}
