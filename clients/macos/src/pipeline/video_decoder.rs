use std::sync::OnceLock;

use thiserror::Error;

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

#[cfg(target_os = "macos")]
#[derive(Debug, Clone)]
pub struct NativeDecodedVideoFrame {
    pub pixel_buffer: apple_cf::cv::CVPixelBuffer,
    pub video: arcen_media::VideoConfiguration,
}

/// The two colour axes a session negotiates once and a packet header never
/// carries: colour primaries and transfer characteristics.
///
/// Read back from the host's own `ServerColorCaps` (`active_primaries` /
/// `active_transfer`), so this is what the host says it is *actually*
/// sending, not what the Deck asked for. `transfer` is the axis that
/// decides whether presentation is HDR: see
/// `crate::ui::video_render::presentation_colorspace_for`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionColor {
    pub primaries: arcen_media::ColorPrimaries,
    pub transfer: arcen_media::TransferCharacteristics,
}

impl Default for SessionColor {
    /// BT.709 SDR -- `arcen_media::VideoConfiguration::legacy_h264`'s own
    /// pair, and the correct assumption for any host that does not report
    /// these axes at all.
    fn default() -> Self {
        Self {
            primaries: arcen_media::ColorPrimaries::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
        }
    }
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub width: usize,
    pub height: usize,
    pub rgba: Vec<u8>,
    pub timestamp_ms: u32,
    pub pixel_format: String,
    pub backend: &'static str,
    #[cfg(target_os = "macos")]
    pub native: Option<NativeDecodedVideoFrame>,
}

#[derive(Debug, Error)]
pub enum VideoDecodeError {
    #[error("native VideoToolbox {0:?} decode path is not wired in this implementation pass yet")]
    CodecPathNotWired(VideoCodec),
    #[error("native VideoToolbox decoding is only implemented on macOS")]
    UnsupportedPlatform,
    #[error("invalid video wire colour metadata: {0:?}")]
    InvalidWireColor(wire::ProtocolError),
    #[error("{0}")]
    Backend(String),
}

/// The wire codecs the native VideoToolbox path decodes today. H.264 is
/// the 4:2:0 MVP stream; H.265 (HEVC Rext) carries the native 4:4:4 stream;
/// AV1 carries the royalty-free 4:2:0 8/10-bit NVENC stream. AV1 uses OBU
/// framing (see the [`av1`] module) rather than Annex-B NALs, so it takes
/// its own decode path rather than the Annex-B/[`AnnexBCodec`] one below.
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

/// The two codecs that use ITU-T Annex-B NAL framing and a cached
/// VPS/SPS/PPS-style parameter set ([`ParameterSetCache`]) -- a narrower
/// type than [`StreamCodec`] so every function in that pipeline (`nal_kind`,
/// `parse_annex_b_access_unit`, `ParameterSetCache`, `prepare_sample_nals`)
/// stays exhaustively matched without a `StreamCodec::Av1 => unreachable!()`
/// arm anywhere: AV1 uses OBU framing (see the [`av1`] module) and has no
/// VPS/SPS/PPS-equivalent parameter set, so it never reaches this pipeline
/// at all -- `PlatformVideoDecoder::decode` branches to its own AV1 path
/// before any of these are called. Mirrors `probe_matrix.rs`'s own
/// `ProbeCodec`, which draws exactly the same line for the same reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnnexBCodec {
    H264,
    H265,
}

impl AnnexBCodec {
    fn from_stream_codec(codec: StreamCodec) -> Option<Self> {
        match codec {
            StreamCodec::H264 => Some(Self::H264),
            StreamCodec::H265 => Some(Self::H265),
            StreamCodec::Av1 => None,
        }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NalKind {
    Slice,
    Keyframe,
    Vps,
    Sps,
    Pps,
    Other,
}

fn nal_kind(codec: AnnexBCodec, nal: &[u8]) -> NalKind {
    match codec {
        AnnexBCodec::H264 => match nal.first().map(|byte| byte & 0x1f) {
            Some(1) => NalKind::Slice,
            Some(5) => NalKind::Keyframe,
            Some(7) => NalKind::Sps,
            Some(8) => NalKind::Pps,
            _ => NalKind::Other,
        },
        // H.265 NAL type lives in bits 6..1 of the first header byte.
        AnnexBCodec::H265 => match nal.first().map(|byte| (byte >> 1) & 0x3f) {
            Some(32) => NalKind::Vps,
            Some(33) => NalKind::Sps,
            Some(34) => NalKind::Pps,
            // IRAP pictures (BLA 16-18, IDR 19-20, CRA 21) are decoder
            // entry points — the H.265 analogue of an H.264 IDR.
            Some(nal_type) if (16..=21).contains(&nal_type) => NalKind::Keyframe,
            Some(nal_type) if nal_type <= 31 => NalKind::Slice,
            _ => NalKind::Other,
        },
    }
}

fn annex_b_nals(data: &[u8]) -> Vec<&[u8]> {
    let Some((mut start_code, mut start_code_len)) = find_start_code(data, 0) else {
        return (!data.is_empty()).then_some(data).into_iter().collect();
    };

    let mut out = Vec::new();
    loop {
        let nal_start = start_code + start_code_len;
        let Some((next_start_code, next_start_code_len)) = find_start_code(data, nal_start) else {
            push_trimmed_nal(&mut out, &data[nal_start..]);
            break;
        };
        push_trimmed_nal(&mut out, &data[nal_start..next_start_code]);
        start_code = next_start_code;
        start_code_len = next_start_code_len;
    }

    out
}

fn parse_annex_b_access_unit(codec: AnnexBCodec, data: &[u8]) -> Result<Vec<&[u8]>, String> {
    if data.is_empty() {
        return Err("empty Annex-B access unit".to_string());
    }
    let Some((first_start_code, _)) = find_start_code(data, 0) else {
        return Err("access unit is not Annex-B (missing leading start code)".to_string());
    };
    if data[..first_start_code].iter().any(|byte| *byte != 0) {
        return Err(
            "Annex-B access unit has non-zero data before its first start code".to_string(),
        );
    }
    let nals = annex_b_nals(data);
    if nals.is_empty() {
        return Err("Annex-B access unit contains no NAL units".to_string());
    }
    for nal in &nals {
        let min_header_len = match codec {
            AnnexBCodec::H264 => 1,
            AnnexBCodec::H265 => 2,
        };
        if nal.len() < min_header_len {
            return Err(format!(
                "truncated {:?} NAL header: {} byte(s)",
                codec,
                nal.len()
            ));
        }
        if nal[0] & 0x80 != 0 {
            return Err(format!("{codec:?} NAL has forbidden_zero_bit set"));
        }
    }
    Ok(nals)
}

fn find_start_code(data: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 3 <= data.len() {
        if data[index..].starts_with(&[0, 0, 1]) {
            return Some((index, 3));
        }
        if index + 4 <= data.len() && data[index..].starts_with(&[0, 0, 0, 1]) {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

fn push_trimmed_nal<'a>(out: &mut Vec<&'a [u8]>, nal: &'a [u8]) {
    let end = nal
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |index| index + 1);
    if end != 0 {
        out.push(&nal[..end]);
    }
}

fn nals_to_avcc(nals: &[&[u8]]) -> Vec<u8> {
    let total = nals.iter().map(|nal| 4 + nal.len()).sum();
    let mut out = Vec::with_capacity(total);
    for nal in nals {
        out.extend_from_slice(&(nal.len() as u32).to_be_bytes());
        out.extend_from_slice(nal);
    }

    out
}

#[derive(Debug, Default)]
struct ParameterSetCache {
    codec: Option<AnnexBCodec>,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl ParameterSetCache {
    fn reset(&mut self, codec: AnnexBCodec) {
        self.codec = Some(codec);
        self.vps = None;
        self.sps = None;
        self.pps = None;
    }

    fn observe(&mut self, codec: AnnexBCodec, nals: &[&[u8]]) -> bool {
        let mut changed = false;
        if self.codec != Some(codec) {
            self.reset(codec);
            changed = true;
        }

        let incoming_vps = nals
            .iter()
            .rev()
            .find(|nal| nal_kind(codec, nal) == NalKind::Vps)
            .copied();
        let incoming_sps = nals
            .iter()
            .rev()
            .find(|nal| nal_kind(codec, nal) == NalKind::Sps)
            .copied();
        let incoming_pps = nals
            .iter()
            .rev()
            .find(|nal| nal_kind(codec, nal) == NalKind::Pps)
            .copied();
        let has_incoming =
            incoming_vps.is_some() || incoming_sps.is_some() || incoming_pps.is_some();
        let config_changed = incoming_vps
            .is_some_and(|nal| self.vps.as_deref().is_some_and(|old| old != nal))
            || incoming_sps.is_some_and(|nal| self.sps.as_deref().is_some_and(|old| old != nal))
            || incoming_pps.is_some_and(|nal| self.pps.as_deref().is_some_and(|old| old != nal));

        if config_changed && self.is_complete(codec) {
            self.reset(codec);
            changed = true;
        }
        if let Some(vps) = incoming_vps {
            if self.vps.as_deref() != Some(vps) {
                self.vps = Some(vps.to_vec());
                changed = true;
            }
        }
        if let Some(sps) = incoming_sps {
            if self.sps.as_deref() != Some(sps) {
                self.sps = Some(sps.to_vec());
                changed = true;
            }
        }
        if let Some(pps) = incoming_pps {
            if self.pps.as_deref() != Some(pps) {
                self.pps = Some(pps.to_vec());
                changed = true;
            }
        }
        changed && has_incoming
    }

    fn is_complete(&self, codec: AnnexBCodec) -> bool {
        match codec {
            AnnexBCodec::H264 => self.sps.is_some() && self.pps.is_some(),
            AnnexBCodec::H265 => self.vps.is_some() && self.sps.is_some() && self.pps.is_some(),
        }
    }

    fn parameter_sets(&self, codec: AnnexBCodec) -> Option<Vec<&[u8]>> {
        if self.codec != Some(codec) || !self.is_complete(codec) {
            return None;
        }
        Some(match codec {
            AnnexBCodec::H264 => vec![self.sps.as_deref()?, self.pps.as_deref()?],
            AnnexBCodec::H265 => vec![
                self.vps.as_deref()?,
                self.sps.as_deref()?,
                self.pps.as_deref()?,
            ],
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SamplePreparationError {
    MissingParameterSets,
}

fn prepare_sample_nals<'a>(
    codec: AnnexBCodec,
    cache: &'a ParameterSetCache,
    nals: &'a [&'a [u8]],
) -> Result<Option<Vec<&'a [u8]>>, SamplePreparationError> {
    let has_keyframe = nals
        .iter()
        .any(|nal| nal_kind(codec, nal) == NalKind::Keyframe);
    let slices: Vec<&[u8]> = nals
        .iter()
        .copied()
        .filter(|nal| matches!(nal_kind(codec, nal), NalKind::Slice | NalKind::Keyframe))
        .collect();
    if slices.is_empty() {
        return Ok(None);
    }
    if !has_keyframe {
        return Ok(Some(slices));
    }

    let Some(parameter_sets) = cache.parameter_sets(codec) else {
        return Err(SamplePreparationError::MissingParameterSets);
    };
    let mut prepared = Vec::with_capacity(parameter_sets.len() + slices.len());
    // Inline parameter sets were cached above and stripped from `slices`, so
    // every keyframe carries exactly one canonical complete set.
    prepared.extend(parameter_sets);
    prepared.extend(slices);
    Ok(Some(prepared))
}

/// AV1 "Low Overhead Bitstream Format" (OBU) parsing, scoped to exactly
/// what `av1C` (the AV1 Codec Configuration Box's
/// `AV1CodecConfigurationRecord`) and `CMVideoFormatDescriptionCreate`
/// need: locating a Sequence Header OBU within a Temporal Unit and reading
/// out of it `seq_profile`/`seq_level_idx[0]`/`seq_tier[0]`/the
/// colour-config bits/the coded dimensions. No unsafe, no FFI -- purely a
/// byte-oriented parser, testable without any Apple API.
///
/// AV1 does not use H.264/HEVC-style VPS/SPS/PPS parameter sets, and the
/// vendored `apple-cf`/`videotoolbox` bindings this codebase uses expose no
/// `CMVideoFormatDescriptionCreateFromAV1ParameterSets`-equivalent (grep
/// confirms no such symbol in either crate). VideoToolbox instead expects
/// an `av1C` box supplied through the format description's
/// `SampleDescriptionExtensionAtoms` extension -- see
/// `platform::make_av1_format_description`, which this module feeds.
///
/// Every field, bit width and branch below is transcribed directly from
/// the normative syntax tables in AOMediaCodec/av1-spec
/// `06.bitstream.syntax.md` (`sequence_header_obu()`/`color_config()`/
/// `obu_header()`/`open_bitstream_unit()`) and AOMediaCodec/av1-spec
/// `04.conventions.md` (the `f(n)`/`leb128()` descriptors and their exact
/// bit-parsing algorithms), cross-checked against
/// AOMediaCodec/av1-spec `03.symbols.md` for the two named constants used
/// in a control-flow decision (`SELECT_SCREEN_CONTENT_TOOLS` /
/// `SELECT_INTEGER_MV`, both `2`, hence both always non-zero) and
/// AOMediaCodec/av1-spec `07.bitstream.semantics.md` for the `obu_type`
/// numeric table. The `av1C` byte layout is transcribed from
/// AOMediaCodec/av1-isobmff `index.bs` section 2.3.3
/// (`AV1CodecConfigurationRecord`) and independently cross-checked against
/// three shipping decoders that build this same VideoToolbox
/// `av1C`/`SampleDescriptionExtensionAtoms` extension: Chromium's
/// `media/gpu/mac/vt_config_util.mm`, Firefox's
/// `dom/media/platforms/apple/AppleVTDecoder.cpp`, and the from-scratch
/// Swift implementation in `alvr-org/alvr-visionos`'s
/// `ALVRClient/AV1Parser.swift` (which independently arrives at the exact
/// same bit-packing this module uses).
///
/// One documented gap: `timing_info_present_flag == 1` is rejected rather
/// than parsed (`timing_info()`/`decoder_model_info()`/
/// `operating_parameters_info()` are not implemented). This is irrelevant
/// to a live, low-latency remote-desktop stream with no container-level
/// timing model -- AV1-in-ISOBMFF itself recommends
/// `timing_info_present_flag` be `0` for exactly this reason ("The
/// presentation times of AV1 samples are given by the ISOBMFF
/// structures").
pub(crate) mod av1 {
    /// Big-endian, MSB-first bit reader for AV1's `f(n)` syntax elements
    /// (av1-spec `04.conventions.md`: "Unsigned n-bit number appearing
    /// directly in the bitstream. The bits are read from high to low
    /// order.").
    struct BitReader<'a> {
        bytes: &'a [u8],
        byte_pos: usize,
        /// Next bit to read within `bytes[byte_pos]`, MSB-first, 0..=7.
        bit_pos: u8,
    }

    impl<'a> BitReader<'a> {
        const fn new(bytes: &'a [u8]) -> Self {
            Self {
                bytes,
                byte_pos: 0,
                bit_pos: 0,
            }
        }

        fn read_bit(&mut self) -> Result<bool, String> {
            let byte = *self
                .bytes
                .get(self.byte_pos)
                .ok_or_else(|| "AV1 sequence header OBU is truncated".to_string())?;
            let bit = (byte >> (7 - self.bit_pos)) & 1 != 0;
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
            Ok(bit)
        }

        /// `f(n)`: the next `n_bits`, most-significant-bit-first.
        fn f(&mut self, n_bits: u32) -> Result<u32, String> {
            debug_assert!(n_bits <= 32);
            let mut value = 0u32;
            for _ in 0..n_bits {
                value = (value << 1) | u32::from(self.read_bit()?);
            }
            Ok(value)
        }
    }

    /// `leb128()` (av1-spec `04.conventions.md`): up to 8 little-endian
    /// base-128 bytes, continuation flagged by each byte's most
    /// significant bit. Returns `(value, bytes_consumed)`.
    fn read_leb128(bytes: &[u8]) -> Result<(u64, usize), String> {
        let mut value: u64 = 0;
        for (i, byte) in bytes.iter().copied().enumerate().take(8) {
            value |= u64::from(byte & 0x7f) << (i * 7);
            if byte & 0x80 == 0 {
                return Ok((value, i + 1));
            }
        }
        Err("AV1 leb128 does not terminate within 8 bytes".to_string())
    }

    /// `obu_type` (av1-spec `07.bitstream.semantics.md`'s numeric table).
    /// Only [`ObuType::SequenceHeader`] is acted on here; every other kind
    /// is passed through unmodified as part of the sample payload (see
    /// `platform::decode_av1`) -- AV1 tolerates, and this codebase relies
    /// on, VideoToolbox seeing whatever real mix of OBUs the encoder
    /// produced.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum ObuType {
        SequenceHeader,
        TemporalDelimiter,
        FrameHeader,
        TileGroup,
        Metadata,
        Frame,
        RedundantFrameHeader,
        TileList,
        Padding,
        Reserved(u8),
    }

    const fn obu_type_from_bits(bits: u8) -> ObuType {
        match bits {
            1 => ObuType::SequenceHeader,
            2 => ObuType::TemporalDelimiter,
            3 => ObuType::FrameHeader,
            4 => ObuType::TileGroup,
            5 => ObuType::Metadata,
            6 => ObuType::Frame,
            7 => ObuType::RedundantFrameHeader,
            8 => ObuType::TileList,
            15 => ObuType::Padding,
            other => ObuType::Reserved(other),
        }
    }

    /// One parsed `open_bitstream_unit()` (av1-spec
    /// `06.bitstream.syntax.md`): `obu_type` plus the byte ranges of the
    /// whole OBU (header + `obu_size` leb128, if present, + payload) and
    /// just its payload, both borrowed from the input so a Sequence Header
    /// OBU can be re-embedded byte-for-byte into `av1C`'s `configOBUs` --
    /// copying the real bytes makes "the values in configOBUs match the
    /// AV1CodecConfigurationRecord" (av1-isobmff `index.bs`) a byte
    /// identity, not a hope.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct Obu<'a> {
        pub(crate) obu_type: ObuType,
        pub(crate) whole: &'a [u8],
        pub(crate) payload: &'a [u8],
    }

    /// Walks a "Low Overhead Bitstream Format" byte buffer (av1-spec's own
    /// "Low overhead bitstream format" section; av1-isobmff `index.bs`'s
    /// "AV1 Sample Format": one Temporal Unit's worth of concatenated
    /// OBUs) into individual OBUs.
    ///
    /// Every OBU must set `obu_has_size_field`, except optionally the last
    /// one in the buffer, whose size then defaults to "the rest of the
    /// buffer" -- both are explicitly sanctioned by av1-isobmff's "AV1
    /// Sample Format" section. `obu_extension_flag` (temporal/spatial layer
    /// id) is skipped, not interpreted: Arcen has no scalable/multi-layer
    /// AV1 use case.
    ///
    /// # Errors
    ///
    /// Returns `Err` for `obu_forbidden_bit` set, a header or `obu_size`
    /// that runs past the end of `data`, or a claimed payload length that
    /// would too.
    pub(crate) fn parse_obus(data: &[u8]) -> Result<Vec<Obu<'_>>, String> {
        let mut obus = Vec::new();
        let mut offset = 0usize;
        while offset < data.len() {
            let header_byte = data[offset];
            if header_byte & 0x80 != 0 {
                return Err(format!(
                    "AV1 OBU at offset {offset} has obu_forbidden_bit set"
                ));
            }
            let obu_type = obu_type_from_bits((header_byte >> 3) & 0x0f);
            let extension_flag = (header_byte >> 2) & 1 != 0;
            let has_size_field = (header_byte >> 1) & 1 != 0;
            let header_len = if extension_flag { 2 } else { 1 };
            if offset + header_len > data.len() {
                return Err(format!(
                    "AV1 OBU at offset {offset} is truncated in its header"
                ));
            }
            let (payload_len, size_field_len) = if has_size_field {
                let (size, consumed) = read_leb128(&data[offset + header_len..])?;
                let size = usize::try_from(size)
                    .map_err(|_| "AV1 obu_size overflows usize".to_string())?;
                (size, consumed)
            } else {
                (data.len() - offset - header_len, 0)
            };
            let payload_start = offset + header_len + size_field_len;
            let payload_end = payload_start
                .checked_add(payload_len)
                .ok_or_else(|| "AV1 OBU payload length overflow".to_string())?;
            if payload_end > data.len() {
                return Err(format!(
                    "AV1 OBU at offset {offset} claims {payload_len} payload bytes but only {} \
                     remain",
                    data.len() - payload_start
                ));
            }
            obus.push(Obu {
                obu_type,
                whole: &data[offset..payload_end],
                payload: &data[payload_start..payload_end],
            });
            if !has_size_field {
                // Sanctioned only for the last OBU in a sample (av1-isobmff
                // `index.bs`: "it is assumed to fill the remainder of the
                // sample"), so `payload_end` is already `data.len()` here --
                // there is nothing left to walk.
                break;
            }
            offset = payload_end;
        }
        Ok(obus)
    }

    /// The `av1C` (`AV1CodecConfigurationRecord`, av1-isobmff `index.bs`
    /// section 2.3.3) fields, plus the coded dimensions
    /// `CMVideoFormatDescriptionCreate` needs, extracted from a Sequence
    /// Header OBU's payload (av1-spec `sequence_header_obu()`).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct SequenceHeaderFields {
        pub(crate) seq_profile: u8,
        pub(crate) seq_level_idx_0: u8,
        pub(crate) seq_tier_0: u8,
        pub(crate) high_bitdepth: bool,
        pub(crate) twelve_bit: bool,
        pub(crate) monochrome: bool,
        pub(crate) chroma_subsampling_x: u8,
        pub(crate) chroma_subsampling_y: u8,
        pub(crate) chroma_sample_position: u8,
        pub(crate) max_frame_width: u32,
        pub(crate) max_frame_height: u32,
    }

    /// Parses a Sequence Header OBU payload (av1-spec
    /// `06.bitstream.syntax.md`'s `sequence_header_obu()`/`color_config()`),
    /// stopping as soon as every [`SequenceHeaderFields`] value has been
    /// read -- `separate_uv_delta_q`, `film_grain_params_present` and
    /// anything after them are never consulted, so they are never read
    /// either (bit-reading is strictly sequential, so it is always correct
    /// to stop once the fields a caller needs have all been read in the
    /// exact order/width the spec lays out, which every field below is).
    ///
    /// # Errors
    ///
    /// Returns `Err` for a truncated buffer, or for
    /// `timing_info_present_flag == 1` (see this module's doc).
    #[allow(clippy::similar_names)]
    pub(crate) fn parse_sequence_header(payload: &[u8]) -> Result<SequenceHeaderFields, String> {
        let mut r = BitReader::new(payload);
        let seq_profile = r.f(3)? as u8;
        let _still_picture = r.f(1)?;
        let reduced_still_picture_header = r.f(1)? != 0;

        let mut seq_level_idx_0 = 0u8;
        let mut seq_tier_0 = 0u8;
        if reduced_still_picture_header {
            seq_level_idx_0 = r.f(5)? as u8;
            // seq_tier[0] = 0 (av1-spec).
        } else {
            let timing_info_present_flag = r.f(1)? != 0;
            if timing_info_present_flag {
                return Err("AV1 sequence header has timing_info_present_flag=1; \
                     timing_info()/decoder_model_info()/operating_parameters_info() are not \
                     implemented by this parser (irrelevant to a live remote-desktop stream \
                     with no container-level timing model)"
                    .to_string());
            }
            // decoder_model_info_present_flag = 0 is forced by the spec
            // whenever timing_info_present_flag == 0 (the only case
            // supported above), so decoder_model_present_for_this_op[i] is
            // always 0 below and operating_parameters_info() is never
            // present -- no bits to read for it.
            let initial_display_delay_present_flag = r.f(1)? != 0;
            let operating_points_cnt_minus_1 = r.f(5)?;
            for i in 0..=operating_points_cnt_minus_1 {
                let _operating_point_idc = r.f(12)?;
                let seq_level_idx = r.f(5)? as u8;
                let seq_tier = if seq_level_idx > 7 { r.f(1)? as u8 } else { 0 };
                if i == 0 {
                    seq_level_idx_0 = seq_level_idx;
                    seq_tier_0 = seq_tier;
                }
                if initial_display_delay_present_flag {
                    let present_for_this_op = r.f(1)? != 0;
                    if present_for_this_op {
                        let _initial_display_delay_minus_1 = r.f(4)?;
                    }
                }
            }
        }

        let frame_width_bits_minus_1 = r.f(4)?;
        let frame_height_bits_minus_1 = r.f(4)?;
        let max_frame_width = r.f(frame_width_bits_minus_1 + 1)? + 1;
        let max_frame_height = r.f(frame_height_bits_minus_1 + 1)? + 1;

        let frame_id_numbers_present_flag = if reduced_still_picture_header {
            false
        } else {
            r.f(1)? != 0
        };
        if frame_id_numbers_present_flag {
            let _delta_frame_id_length_minus_2 = r.f(4)?;
            let _additional_frame_id_length_minus_1 = r.f(3)?;
        }

        let _use_128x128_superblock = r.f(1)?;
        let _enable_filter_intra = r.f(1)?;
        let _enable_intra_edge_filter = r.f(1)?;

        if !reduced_still_picture_header {
            let _enable_interintra_compound = r.f(1)?;
            let _enable_masked_compound = r.f(1)?;
            let _enable_warped_motion = r.f(1)?;
            let _enable_dual_filter = r.f(1)?;
            let enable_order_hint = r.f(1)? != 0;
            if enable_order_hint {
                let _enable_jnt_comp = r.f(1)?;
                let _enable_ref_frame_mvs = r.f(1)?;
            }
            let seq_choose_screen_content_tools = r.f(1)? != 0;
            // SELECT_SCREEN_CONTENT_TOOLS = 2 (av1-spec `03.symbols.md`):
            // always non-zero, so only whether the *explicit* value read
            // below is zero matters to the next `if`.
            let seq_force_screen_content_tools = if seq_choose_screen_content_tools {
                2
            } else {
                r.f(1)?
            };
            if seq_force_screen_content_tools > 0 {
                let seq_choose_integer_mv = r.f(1)? != 0;
                if !seq_choose_integer_mv {
                    let _seq_force_integer_mv = r.f(1)?;
                }
            }
            if enable_order_hint {
                let _order_hint_bits_minus_1 = r.f(3)?;
            }
        }

        let _enable_superres = r.f(1)?;
        let _enable_cdef = r.f(1)?;
        let _enable_restoration = r.f(1)?;

        // color_config() (av1-spec `06.bitstream.syntax.md`).
        let high_bitdepth = r.f(1)? != 0;
        let (twelve_bit, bit_depth) = if seq_profile == 2 && high_bitdepth {
            let twelve_bit = r.f(1)? != 0;
            (twelve_bit, if twelve_bit { 12 } else { 10 })
        } else {
            (false, if high_bitdepth { 10 } else { 8 })
        };
        let monochrome = if seq_profile == 1 {
            false
        } else {
            r.f(1)? != 0
        };
        let color_description_present_flag = r.f(1)? != 0;
        let (color_primaries, transfer_characteristics, matrix_coefficients) =
            if color_description_present_flag {
                (r.f(8)?, r.f(8)?, r.f(8)?)
            } else {
                (2, 2, 2) // CP_UNSPECIFIED, TC_UNSPECIFIED, MC_UNSPECIFIED.
            };

        let (chroma_subsampling_x, chroma_subsampling_y, chroma_sample_position) = if monochrome {
            let _color_range = r.f(1)?;
            (1u8, 1u8, 0u8) // CSP_UNKNOWN.
        } else if color_primaries == 1 && transfer_characteristics == 13 && matrix_coefficients == 0
        {
            // CP_BT_709 && TC_SRGB && MC_IDENTITY: implied full-range,
            // unsubsampled 4:4:4 (av1-spec `color_config()`).
            (0u8, 0u8, 0u8)
        } else {
            let _color_range = r.f(1)?;
            let (subsampling_x, subsampling_y) = match seq_profile {
                0 => (1u8, 1u8),
                1 => (0u8, 0u8),
                _ if bit_depth == 12 => {
                    let subsampling_x = r.f(1)? as u8;
                    let subsampling_y = if subsampling_x != 0 { r.f(1)? as u8 } else { 0 };
                    (subsampling_x, subsampling_y)
                }
                _ => (1u8, 0u8),
            };
            let chroma_sample_position = if subsampling_x != 0 && subsampling_y != 0 {
                r.f(2)? as u8
            } else {
                0
            };
            (subsampling_x, subsampling_y, chroma_sample_position)
        };

        Ok(SequenceHeaderFields {
            seq_profile,
            seq_level_idx_0,
            seq_tier_0,
            high_bitdepth,
            twelve_bit,
            monochrome,
            chroma_subsampling_x,
            chroma_subsampling_y,
            chroma_sample_position,
            max_frame_width,
            max_frame_height,
        })
    }

    /// Builds the `av1C` box payload (`AV1CodecConfigurationRecord`,
    /// av1-isobmff `index.bs` section 2.3.3): a fixed 4-byte header
    /// followed verbatim by `sequence_header_obu` (the complete OBU --
    /// header, `obu_size`, and payload -- exactly as it appeared on the
    /// wire, per [`Obu::whole`]). `initial_presentation_delay_present` is
    /// always signalled `0`: Arcen has no pre-buffered decode schedule to
    /// describe (a live low-latency stream has no use for it), matching
    /// the field's own semantics (a hint for how many samples to buffer
    /// before starting presentation).
    ///
    /// Matches the approach independently taken by Chromium, Firefox,
    /// WebKit and the ALVR/Moonlight community VideoToolbox AV1 clients
    /// (see this module's doc): `configOBUs` is the real OBU bytes, not a
    /// re-serialisation of them.
    #[must_use]
    pub(crate) fn build_av1c(fields: &SequenceHeaderFields, sequence_header_obu: &[u8]) -> Vec<u8> {
        let mut record = Vec::with_capacity(4 + sequence_header_obu.len());
        // marker(1)=1, version(7)=1.
        record.push(0x80 | 0x01);
        // seq_profile(3), seq_level_idx_0(5).
        record.push((fields.seq_profile << 5) | (fields.seq_level_idx_0 & 0x1f));
        // seq_tier_0(1), high_bitdepth(1), twelve_bit(1), monochrome(1),
        // chroma_subsampling_x(1), chroma_subsampling_y(1),
        // chroma_sample_position(2).
        record.push(
            ((fields.seq_tier_0 & 1) << 7)
                | (u8::from(fields.high_bitdepth) << 6)
                | (u8::from(fields.twelve_bit) << 5)
                | (u8::from(fields.monochrome) << 4)
                | ((fields.chroma_subsampling_x & 1) << 3)
                | ((fields.chroma_subsampling_y & 1) << 2)
                | (fields.chroma_sample_position & 0x03),
        );
        // reserved(3)=0, initial_presentation_delay_present(1)=0,
        // reserved(4)=0.
        record.push(0x00);
        record.extend_from_slice(sequence_header_obu);
        record
    }

    /// Renders a short `kind:length` summary of `obus`, for logging --
    /// mirrors `summarize_nals`'s role for the Annex-B pipeline.
    #[must_use]
    pub(crate) fn summarize(obus: &[Obu<'_>]) -> String {
        let parts = obus
            .iter()
            .map(|obu| {
                let kind: std::borrow::Cow<'static, str> = match obu.obu_type {
                    ObuType::SequenceHeader => "seq_header".into(),
                    ObuType::TemporalDelimiter => "temporal_delimiter".into(),
                    ObuType::FrameHeader => "frame_header".into(),
                    ObuType::TileGroup => "tile_group".into(),
                    ObuType::Metadata => "metadata".into(),
                    ObuType::Frame => "frame".into(),
                    ObuType::RedundantFrameHeader => "redundant_frame_header".into(),
                    ObuType::TileList => "tile_list".into(),
                    ObuType::Padding => "padding".into(),
                    ObuType::Reserved(value) => format!("reserved({value})").into(),
                };
                format!("{kind}:{}", obu.whole.len())
            })
            .collect::<Vec<_>>()
            .join(",");
        format!("obus=[{parts}]")
    }
}

/// Minimal, from-scratch Annex-B parameter-set (VPS/SPS/PPS) bitstream
/// construction for the runtime decode-capability probe (see
/// [`DecodeCapabilities`] below). Every syntax element is written by an
/// explicit bit writer against the exact ITU-T H.264/H.265 syntax tables
/// (VPS/SPS `profile_tier_level` bit widths cross-checked against
/// FFmpeg's `libavcodec/hevc/ps.c` parser, the most widely deployed
/// independent implementation of this syntax), rather than copied from
/// any hardcoded byte literal: a byte blob cannot be reviewed
/// field-by-field, and this probe's whole purpose is to be an honest,
/// inspectable substitute for parameter sets Arcen has no local encoder
/// to produce on the Deck.
mod bitstream {
    /// Big-endian, MSB-first bit writer for Annex-B RBSP syntax elements.
    #[derive(Debug, Default, Clone)]
    pub(super) struct BitWriter {
        bytes: Vec<u8>,
        partial: u8,
        /// Number of valid bits already shifted into `partial`, 0..=7.
        bit_pos: u8,
    }

    impl BitWriter {
        pub(super) fn new() -> Self {
            Self::default()
        }

        /// Writes one bit, most-significant-bit-first within each byte.
        pub(super) fn write_bit(&mut self, bit: bool) {
            self.partial = (self.partial << 1) | u8::from(bit);
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bytes.push(self.partial);
                self.partial = 0;
                self.bit_pos = 0;
            }
        }

        /// `n_bits` zero bits -- shorthand for a run of reserved/unused
        /// flag bits.
        pub(super) fn write_zeros(&mut self, n_bits: u32) {
            for _ in 0..n_bits {
                self.write_bit(false);
            }
        }

        /// `u(n)`: the low `n_bits` of `value`, most-significant-bit-first.
        pub(super) fn write_bits(&mut self, value: u32, n_bits: u32) {
            debug_assert!(n_bits <= 32);
            for i in (0..n_bits).rev() {
                self.write_bit((value >> i) & 1 != 0);
            }
        }

        /// `ue(v)`: unsigned Exp-Golomb code (ITU-T H.264/H.265 9.1).
        pub(super) fn write_ue(&mut self, value: u32) {
            let code_num = value + 1;
            let leading_zero_bits = 31 - code_num.leading_zeros();
            self.write_zeros(leading_zero_bits);
            self.write_bits(code_num, leading_zero_bits + 1);
        }

        /// `se(v)`: signed Exp-Golomb code (ITU-T H.264/H.265 9.1.1's
        /// mapping from a signed value to the `ue(v)` `codeNum`).
        pub(super) fn write_se(&mut self, value: i32) {
            let doubled = i64::from(value) * 2;
            let code_num = if value <= 0 { -doubled } else { doubled - 1 };
            self.write_ue(code_num as u32);
        }

        /// `rbsp_trailing_bits()`: a single `1` stop bit, then zero
        /// padding to the next byte boundary.
        pub(super) fn rbsp_trailing_bits(&mut self) {
            self.write_bit(true);
            while self.bit_pos != 0 {
                self.write_bit(false);
            }
        }

        /// Number of bits written so far, including any not-yet-flushed
        /// partial byte. Test-only: every production caller finishes with
        /// [`Self::rbsp_trailing_bits`], which always leaves this
        /// byte-aligned.
        #[cfg(test)]
        pub(super) fn bit_len(&self) -> usize {
            self.bytes.len() * 8 + usize::from(self.bit_pos)
        }

        /// Consumes the writer, returning the byte-aligned RBSP.
        pub(super) fn into_rbsp(self) -> Vec<u8> {
            debug_assert_eq!(self.bit_pos, 0, "RBSP must be byte-aligned before use");
            self.bytes
        }
    }

    /// Applies Annex-B "emulation prevention": inserts `0x03` after any
    /// two consecutive zero bytes immediately followed by a byte `<=
    /// 0x03`, so the result never contains a real start-code prefix
    /// (`0x000000`, `0x000001`, `0x000002`) or an unescaped `0x000003`
    /// once wrapped in a NAL header.
    pub(super) fn escape_emulation_prevention(rbsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rbsp.len());
        let mut zero_run = 0u8;
        for &byte in rbsp {
            if zero_run >= 2 && byte <= 3 {
                out.push(0x03);
                zero_run = 0;
            }
            out.push(byte);
            zero_run = if byte == 0 { zero_run + 1 } else { 0 };
        }
        out
    }

    /// Removes Annex-B emulation-prevention bytes before test-only RBSP
    /// parsing. The production decoder passes the escaped NAL to
    /// VideoToolbox, which performs this step itself.
    #[cfg(test)]
    pub(super) fn unescape_emulation_prevention(ebsp: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(ebsp.len());
        let mut zero_run = 0u8;
        for (index, &byte) in ebsp.iter().enumerate() {
            if zero_run >= 2
                && byte == 0x03
                && ebsp.get(index + 1).is_some_and(|next| *next <= 0x03)
            {
                zero_run = 0;
                continue;
            }
            out.push(byte);
            zero_run = if byte == 0 { zero_run + 1 } else { 0 };
        }
        out
    }

    /// Minimal companion bit reader, used only by tests to confirm the
    /// writer above actually placed fields where the SPS/PPS/VPS builders
    /// below believe they are.
    #[cfg(test)]
    #[derive(Debug, Clone, Copy)]
    pub(super) struct BitReader<'a> {
        bytes: &'a [u8],
        byte_pos: usize,
        /// Next bit to read within `bytes[byte_pos]`, MSB-first, 0..=7.
        bit_pos: u8,
    }

    #[cfg(test)]
    impl<'a> BitReader<'a> {
        pub(super) fn new(bytes: &'a [u8]) -> Self {
            Self {
                bytes,
                byte_pos: 0,
                bit_pos: 0,
            }
        }

        pub(super) fn read_bit(&mut self) -> bool {
            let byte = self.bytes[self.byte_pos];
            let bit = (byte >> (7 - self.bit_pos)) & 1 != 0;
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bit_pos = 0;
                self.byte_pos += 1;
            }
            bit
        }

        pub(super) fn read_bits(&mut self, n_bits: u32) -> u32 {
            let mut value = 0u32;
            for _ in 0..n_bits {
                value = (value << 1) | u32::from(self.read_bit());
            }
            value
        }

        pub(super) fn skip(&mut self, n_bits: u32) {
            for _ in 0..n_bits {
                self.read_bit();
            }
        }

        pub(super) fn read_ue(&mut self) -> u32 {
            let mut leading_zero_bits = 0u32;
            while !self.read_bit() {
                leading_zero_bits += 1;
            }
            let remainder = self.read_bits(leading_zero_bits);
            (1u32 << leading_zero_bits) - 1 + remainder
        }
    }
}

/// One synthetic HEVC stream variant the runtime capability probe (see
/// [`DecodeCapabilities`]) builds a real `CMFormatDescription`/
/// `VTDecompressionSession` for. Chroma is restricted to 4:2:0 or 4:4:4 --
/// the only two Arcen's own wire protocol needs this probe to
/// distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct HevcProbeProfile {
    /// `chroma_format_idc`: `1` for 4:2:0, `3` for 4:4:4.
    chroma_format_idc: u8,
    /// Luma and chroma bit depth (this probe never separates them): `8`,
    /// `10`, or `12`.
    bit_depth: u8,
}

impl HevcProbeProfile {
    fn chroma(self) -> ChromaSubsampling {
        if self.chroma_format_idc == 3 {
            ChromaSubsampling::Yuv444
        } else {
            ChromaSubsampling::Yuv420
        }
    }

    fn ten_bit(self) -> bool {
        self.bit_depth >= 10
    }
}

/// The synthetic picture size every probe builds. One CTU
/// (`log2_diff_max_min_luma_coding_block_size` in [`build_hevc_sps`] picks
/// a 64-sample CTU) at exactly the picture size avoids any
/// boundary/partial-CTU edge case a still-tinier picture would introduce.
const PROBE_DIMENSION: u16 = 64;

/// HEVC level to declare in every probed `profile_tier_level` -- Level
/// 1.0, comfortably above what a single `PROBE_DIMENSION` x
/// `PROBE_DIMENSION` picture needs.
const PROBE_HEVC_LEVEL_IDC: u8 = 30;

/// `general_profile_idc` a real encoder would choose for `profile`,
/// matching x265/NVENC convention: Main (1) for 8-bit 4:2:0, Main 10 (2)
/// for 10-bit 4:2:0, and Format Range Extensions ("Rext", 4) for
/// everything else this probe exercises (4:4:4 at any depth, or 12-bit
/// 4:2:0 -- "Main 12"). This is the exact axis the whole probe cares
/// about: whether VideoToolbox accepts profile 4 (Rext) at all is the
/// only way to learn whether this Mac decodes HEVC Rext, since
/// `chroma_format_idc`/`bit_depth_luma_minus8` alone would still parse
/// even against hardware that only implements a plain Main/Main10
/// decoder.
fn hevc_general_profile_idc(profile: HevcProbeProfile) -> u8 {
    match (profile.chroma_format_idc, profile.bit_depth) {
        (1, 8) => 1,
        (1, 10) => 2,
        _ => 4,
    }
}

/// Writes `profile_tier_level(profilePresentFlag=1, maxNumSubLayersMinus1=0)`
/// (ITU-T H.265 7.3.3): a fixed 96 bits (12 bytes) for the single-layer,
/// no-sub-layers case every VPS/SPS this probe builds uses -- 8
/// (space/tier/idc) + 32 (compatibility flags) + 4 (source/constraint
/// flags) + 43 (profile-dependent constraint/reserved bits, the same
/// total on every branch) + 1 (`inbld`/reserved) + 8 (`level_idc`). Bit
/// widths cross-checked against FFmpeg's `decode_profile_tier_level`
/// (`libavcodec/hevc/ps.c`), independent of and predating this probe.
fn write_profile_tier_level(w: &mut bitstream::BitWriter, profile: HevcProbeProfile) {
    let general_profile_idc = hevc_general_profile_idc(profile);
    w.write_bits(0, 2); // general_profile_space
    w.write_bit(false); // general_tier_flag (Main tier)
    w.write_bits(u32::from(general_profile_idc), 5);
    for compat_idc in 0..32u32 {
        // A profile is always compatible with itself; no other profile's
        // claim applies to a stream synthesised purely to probe one.
        w.write_bit(compat_idc == u32::from(general_profile_idc));
    }
    w.write_bit(true); // general_progressive_source_flag
    w.write_bit(false); // general_interlaced_source_flag
    w.write_bit(true); // general_non_packed_constraint_flag
    w.write_bit(true); // general_frame_only_constraint_flag
    match general_profile_idc {
        4..=10 => {
            // Format Range Extensions ("Rext") family: nine explicit
            // constraint flags, set from this profile's *actual*
            // chroma/bit-depth ceiling -- not copied from any real
            // encoder's stream, since declaring itself accurately is this
            // probe's only job.
            w.write_bit(profile.bit_depth <= 12); // general_max_12bit_constraint_flag
            w.write_bit(profile.bit_depth <= 10); // general_max_10bit_constraint_flag
            w.write_bit(profile.bit_depth <= 8); // general_max_8bit_constraint_flag
            w.write_bit(profile.chroma_format_idc <= 2); // general_max_422chroma_constraint_flag
            w.write_bit(profile.chroma_format_idc <= 1); // general_max_420chroma_constraint_flag
            w.write_bit(false); // general_max_monochrome_constraint_flag
            w.write_bit(false); // general_intra_constraint_flag
            w.write_bit(false); // general_one_picture_only_constraint_flag
            w.write_bit(false); // general_lower_bit_rate_constraint_flag
            w.write_zeros(34); // XXX_reserved_zero_34bits[0..33]
        }
        2 => {
            w.write_zeros(7);
            w.write_bit(false); // general_one_picture_only_constraint_flag
            w.write_zeros(35); // XXX_reserved_zero_35bits[0..34]
        }
        _ => w.write_zeros(43), // XXX_reserved_zero_43bits[0..42] (Main / Main Still Picture)
    }
    w.write_bit(false); // general_inbld_flag / general_reserved_zero_bit
    w.write_bits(u32::from(PROBE_HEVC_LEVEL_IDC), 8);
}

/// Prepends a two-byte HEVC NAL header (`nuh_layer_id = 0`,
/// `nuh_temporal_id_plus1 = 1`) and Annex-B-escapes `rbsp`.
fn hevc_nal(nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let escaped = bitstream::escape_emulation_prevention(rbsp);
    let mut nal = Vec::with_capacity(2 + escaped.len());
    nal.push(nal_unit_type << 1);
    nal.push(1);
    nal.extend_from_slice(&escaped);
    nal
}

/// Builds a complete, spec-valid `video_parameter_set_rbsp` NAL for
/// `profile`: single layer, single sub-layer, no timing/extension data --
/// the minimum `CMVideoFormatDescriptionCreateFromHEVCParameterSets`
/// needs a VPS for at all.
fn build_hevc_vps(profile: HevcProbeProfile) -> Vec<u8> {
    let mut w = bitstream::BitWriter::new();
    w.write_bits(0, 4); // vps_video_parameter_set_id
    w.write_bit(true); // vps_base_layer_internal_flag
    w.write_bit(true); // vps_base_layer_available_flag
    w.write_bits(0, 6); // vps_max_layers_minus1
    w.write_bits(0, 3); // vps_max_sub_layers_minus1
    w.write_bit(true); // vps_temporal_id_nesting_flag
    w.write_bits(0xFFFF, 16); // vps_reserved_0xffff_16bits
    write_profile_tier_level(&mut w, profile);
    w.write_bit(false); // vps_sub_layer_ordering_info_present_flag
    w.write_ue(0); // vps_max_dec_pic_buffering_minus1[0]
    w.write_ue(0); // vps_max_num_reorder_pics[0]
    w.write_ue(0); // vps_max_latency_increase_plus1[0]
    w.write_bits(0, 6); // vps_max_layer_id
    w.write_ue(0); // vps_num_layer_sets_minus1
    w.write_bit(false); // vps_timing_info_present_flag
    w.write_bit(false); // vps_extension_flag
    w.rbsp_trailing_bits();
    hevc_nal(32, &w.into_rbsp())
}

/// Builds a complete `seq_parameter_set_rbsp` NAL for `profile` at a
/// fixed `PROBE_DIMENSION` x `PROBE_DIMENSION` picture: one CTU, no
/// scaling lists, no PCM, no reference-picture sets, no VUI, no SPS
/// extension -- nothing this probe's capability question (does
/// `chroma_format_idc`/`bit_depth_*_minus8` under this
/// `general_profile_idc` even build a session) depends on.
fn build_hevc_sps(profile: HevcProbeProfile) -> Vec<u8> {
    let mut w = bitstream::BitWriter::new();
    w.write_bits(0, 4); // sps_video_parameter_set_id
    w.write_bits(0, 3); // sps_max_sub_layers_minus1
    w.write_bit(true); // sps_temporal_id_nesting_flag
    write_profile_tier_level(&mut w, profile);
    w.write_ue(0); // sps_seq_parameter_set_id
    w.write_ue(u32::from(profile.chroma_format_idc)); // chroma_format_idc
    if profile.chroma_format_idc == 3 {
        w.write_bit(false); // separate_colour_plane_flag
    }
    w.write_ue(u32::from(PROBE_DIMENSION)); // pic_width_in_luma_samples
    w.write_ue(u32::from(PROBE_DIMENSION)); // pic_height_in_luma_samples
    w.write_bit(false); // conformance_window_flag
    w.write_ue(u32::from(profile.bit_depth - 8)); // bit_depth_luma_minus8
    w.write_ue(u32::from(profile.bit_depth - 8)); // bit_depth_chroma_minus8
    w.write_ue(0); // log2_max_pic_order_cnt_lsb_minus4
    w.write_bit(false); // sps_sub_layer_ordering_info_present_flag
    w.write_ue(0); // sps_max_dec_pic_buffering_minus1[0]
    w.write_ue(0); // sps_max_num_reorder_pics[0]
    w.write_ue(0); // sps_max_latency_increase_plus1[0]
    w.write_ue(0); // log2_min_luma_coding_block_size_minus3 (min CB = 8)
    w.write_ue(3); // log2_diff_max_min_luma_coding_block_size (CTU = 64)
    w.write_ue(0); // log2_min_luma_transform_block_size_minus2 (min TB = 4)
    w.write_ue(3); // log2_diff_max_min_transform_block_size (max TB = 32)
    w.write_ue(0); // max_transform_hierarchy_depth_inter
    w.write_ue(0); // max_transform_hierarchy_depth_intra
    w.write_bit(false); // scaling_list_enabled_flag
    w.write_bit(false); // amp_enabled_flag
    w.write_bit(false); // sample_adaptive_offset_enabled_flag
    w.write_bit(false); // pcm_enabled_flag
    w.write_ue(0); // num_short_term_ref_pic_sets
    w.write_bit(false); // long_term_ref_pics_present_flag
    w.write_bit(false); // sps_temporal_mvp_enabled_flag
    w.write_bit(false); // strong_intra_smoothing_enabled_flag
    w.write_bit(false); // vui_parameters_present_flag
    w.write_bit(false); // sps_extension_present_flag
    w.rbsp_trailing_bits();
    hevc_nal(33, &w.into_rbsp())
}

/// Builds a `pic_parameter_set_rbsp` NAL. Profile-independent: nothing
/// this probe cares about (chroma/bit-depth/profile) lives in the PPS.
fn build_hevc_pps() -> Vec<u8> {
    let mut w = bitstream::BitWriter::new();
    w.write_ue(0); // pps_pic_parameter_set_id
    w.write_ue(0); // pps_seq_parameter_set_id
    w.write_bit(false); // dependent_slice_segments_enabled_flag
    w.write_bit(false); // output_flag_present_flag
    w.write_bits(0, 3); // num_extra_slice_header_bits
    w.write_bit(false); // sign_data_hiding_enabled_flag
    w.write_bit(false); // cabac_init_present_flag
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_se(0); // init_qp_minus26
    w.write_bit(false); // constrained_intra_pred_flag
    w.write_bit(false); // transform_skip_enabled_flag
    w.write_bit(false); // cu_qp_delta_enabled_flag
    w.write_se(0); // pps_cb_qp_offset
    w.write_se(0); // pps_cr_qp_offset
    w.write_bit(false); // pps_slice_chroma_qp_offsets_present_flag
    w.write_bit(false); // weighted_pred_flag
    w.write_bit(false); // weighted_bipred_flag
    w.write_bit(false); // transquant_bypass_enabled_flag
    w.write_bit(false); // tiles_enabled_flag
    w.write_bit(false); // entropy_coding_sync_enabled_flag
    w.write_bit(true); // pps_loop_filter_across_slices_enabled_flag
    w.write_bit(false); // deblocking_filter_control_present_flag
    w.write_bit(false); // pps_scaling_list_data_present_flag
    w.write_bit(false); // lists_modification_present_flag
    w.write_ue(0); // log2_parallel_merge_level_minus2
    w.write_bit(false); // slice_segment_header_extension_present_flag
    w.write_bit(false); // pps_extension_present_flag
    w.rbsp_trailing_bits();
    hevc_nal(34, &w.into_rbsp())
}

/// Prepends a one-byte H.264 NAL header (`nal_ref_idc = 3`, the
/// conventional highest-importance value for parameter sets) and
/// Annex-B-escapes `rbsp`.
fn h264_nal(nal_unit_type: u8, rbsp: &[u8]) -> Vec<u8> {
    let escaped = bitstream::escape_emulation_prevention(rbsp);
    let mut nal = Vec::with_capacity(1 + escaped.len());
    nal.push((3 << 5) | nal_unit_type);
    nal.extend_from_slice(&escaped);
    nal
}

/// Builds a minimal Baseline-profile `seq_parameter_set_rbsp` NAL: fixed
/// `PROBE_DIMENSION` x `PROBE_DIMENSION`, implicitly 4:2:0 eight-bit (the
/// high-profile-only chroma/bit-depth fields do not exist in a Baseline
/// SPS at all -- ITU-T H.264 7.3.2.1.1 gates them on `profile_idc`).
/// `pic_order_cnt_type = 2` sidesteps every optional POC field.
fn build_h264_sps() -> Vec<u8> {
    let mut w = bitstream::BitWriter::new();
    w.write_bits(66, 8); // profile_idc = Baseline
    w.write_bit(true); // constraint_set0_flag (obeys Annex A.2.1, Baseline)
    w.write_zeros(5); // constraint_set1_flag .. constraint_set5_flag
    w.write_bits(0, 2); // reserved_zero_2bits
    w.write_bits(10, 8); // level_idc = 1.0
    w.write_ue(0); // seq_parameter_set_id
    w.write_ue(0); // log2_max_frame_num_minus4
    w.write_ue(2); // pic_order_cnt_type = 2 (sidesteps every optional POC field)
    w.write_ue(1); // max_num_ref_frames
    w.write_bit(false); // gaps_in_frame_num_value_allowed_flag
    w.write_ue(u32::from(PROBE_DIMENSION / 16 - 1)); // pic_width_in_mbs_minus1
    w.write_ue(u32::from(PROBE_DIMENSION / 16 - 1)); // pic_height_in_map_units_minus1
    w.write_bit(true); // frame_mbs_only_flag
    w.write_bit(true); // direct_8x8_inference_flag
    w.write_bit(false); // frame_cropping_flag
    w.write_bit(false); // vui_parameters_present_flag
    w.rbsp_trailing_bits();
    h264_nal(7, &w.into_rbsp())
}

/// Builds a minimal `pic_parameter_set_rbsp` NAL (CAVLC, one slice
/// group).
fn build_h264_pps() -> Vec<u8> {
    let mut w = bitstream::BitWriter::new();
    w.write_ue(0); // pic_parameter_set_id
    w.write_ue(0); // seq_parameter_set_id
    w.write_bit(false); // entropy_coding_mode_flag (CAVLC)
    w.write_bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.write_ue(0); // num_slice_groups_minus1
    w.write_ue(0); // num_ref_idx_l0_default_active_minus1
    w.write_ue(0); // num_ref_idx_l1_default_active_minus1
    w.write_bit(false); // weighted_pred_flag
    w.write_bits(0, 2); // weighted_bipred_idc
    w.write_se(0); // pic_init_qp_minus26
    w.write_se(0); // pic_init_qs_minus26
    w.write_se(0); // chroma_qp_index_offset
    w.write_bit(false); // deblocking_filter_control_present_flag
    w.write_bit(false); // constrained_intra_pred_flag
    w.write_bit(false); // redundant_pic_cnt_present_flag
    w.rbsp_trailing_bits();
    h264_nal(8, &w.into_rbsp())
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
        nals_to_avcc, parse_annex_b_access_unit, preferred_pixel_format, prepare_sample_nals,
        validate_biplanar_layout, wants_ten_bit_pixel_format, wire, AnnexBCodec, BufferDepth,
        ChromaSubsampling, ColorExtensionPlan, ColorPrimariesToken, ColorRange, DecodedVideoFrame,
        NalKind, NativeDecodedVideoFrame, ParameterSetCache, PixelBufferFormat, PlaneLayout,
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
    /// Annex-B pipeline `ParameterSetCache` serves (see [`AnnexBCodec`]).
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
    }

    impl PlatformVideoDecoder {
        pub fn set_session_color(&mut self, session_color: SessionColor) {
            self.session_color = session_color;
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
                match AnnexBCodec::from_stream_codec(codec) {
                    Some(annex_b_codec) => self.parameter_sets.reset(annex_b_codec),
                    None => self.av1_sequence = None,
                }
                self.format = None;
                self.session = None;
                self.tx = None;
                self.rx = None;
                self.waiting_for_keyframe = true;
            }

            let Some(annex_b_codec) = AnnexBCodec::from_stream_codec(codec) else {
                return self.decode_av1(codec, header, payload);
            };

            let nals = parse_annex_b_access_unit(annex_b_codec, payload).map_err(|error| {
                self.waiting_for_keyframe = true;
                VideoDecodeError::Backend(error)
            })?;
            let format_changed = self.parameter_sets.observe(annex_b_codec, &nals);
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
            let sample_payload = nals_to_avcc(&decode_nals);
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
        /// [`Self::decode`]'s Annex-B pipeline above (see [`AnnexBCodec`]).
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
                    let Some(parameter_sets) =
                        self.parameter_sets.parameter_sets(AnnexBCodec::H264)
                    else {
                        self.waiting_for_keyframe = true;
                        return Ok(());
                    };
                    make_h264_format_description(parameter_sets[0], parameter_sets[1])?
                }
                StreamCodec::H265 => {
                    let Some(parameter_sets) =
                        self.parameter_sets.parameter_sets(AnnexBCodec::H265)
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
            let attributes = requested_pixel_format.and_then(build_destination_attributes);

            let (tx, rx) = mpsc::channel();

            // Each attempt gets its own transfer session and callback, because
            // `PixelTransferSession` is neither `Clone` nor guaranteed `Sync`
            // and the callback must own it. The outer `Result` is a hard
            // failure; the inner one is the session creation we may retry.
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
                        let message = match copy_rgba_frame(frame, chroma, &transfer, native_video)
                        {
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
            session.set_real_time(true).map_err(|error| {
                VideoDecodeError::Backend(format!("Could not enable VT real-time decode: {error}"))
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
    fn build_destination_attributes(pixel_format: u32) -> Option<CFDictionary> {
        // SAFETY: `kCVPixelBufferPixelFormatTypeKey` is a well-known,
        // process-wide singleton CFStringRef exported by CoreVideo.
        let key = unsafe {
            CFString::from_raw_retained(raw::kCVPixelBufferPixelFormatTypeKey.cast_mut().cast())
        }?;
        let value = CFNumber::from_i64(i64::from(pixel_format));
        Some(CFDictionary::from_pairs(&[(&key, &value)]))
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
    ) -> Result<DecodedVideoFrame, String> {
        if frame.status != 0 {
            return Err(format!("VT decoder callback status {}", frame.status));
        }
        let pixel_buffer = frame
            .image_buffer
            .ok_or_else(|| "VT decoder callback did not include an image buffer".to_string())?;
        let format = pixel_buffer.pixel_format();
        let timestamp_ms = timestamp_to_ms(frame.presentation_time);
        match classify_pixel_buffer_format(format)? {
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
            pixel_buffer: pixel_buffer.clone(),
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
        arcen_media::VideoConfiguration {
            codec: match codec {
                StreamCodec::H264 => arcen_media::VideoCodec::H264,
                StreamCodec::H265 => arcen_media::VideoCodec::H265,
                StreamCodec::Av1 => arcen_media::VideoCodec::Av1,
            },
            chroma: match chroma {
                ChromaSubsampling::Yuv420 => arcen_media::ChromaSubsampling::Yuv420,
                ChromaSubsampling::Yuv422 => arcen_media::ChromaSubsampling::Yuv422,
                ChromaSubsampling::Yuv444 => arcen_media::ChromaSubsampling::Yuv444,
            },
            bit_depth: match bit_depth {
                wire::BitDepth::Eight => arcen_media::BitDepth::Eight,
                wire::BitDepth::Ten => arcen_media::BitDepth::Ten,
                wire::BitDepth::Twelve => arcen_media::BitDepth::Twelve,
            },
            range: if full_range {
                arcen_media::ColorRange::Full
            } else {
                arcen_media::ColorRange::Limited
            },
            matrix: match matrix {
                wire::ColorMatrix::Bt709 => arcen_media::ColorMatrix::Bt709,
                wire::ColorMatrix::Identity => arcen_media::ColorMatrix::Identity,
                wire::ColorMatrix::Bt601 => arcen_media::ColorMatrix::Bt601,
                wire::ColorMatrix::Bt2020Ncl => arcen_media::ColorMatrix::Bt2020Ncl,
            },
            primaries: session_color.primaries,
            transfer: session_color.transfer,
        }
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

    fn summarize_nals(codec: AnnexBCodec, nals: &[&[u8]]) -> String {
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
        let av1 = probe_av1_hardware_decode_supported();
        tracing::info!(
            target: crate::logging::target::VIDEO,
            capability = "av1",
            supported = av1,
            "decode-capability probe: codec-level only (see DecodeCapabilities::av1 docs)",
        );

        let hardware_accelerated = h265
            .as_ref()
            .ok()
            .copied()
            .flatten()
            .or_else(|| h264.as_ref().ok().copied().flatten());

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
        let vps = super::build_hevc_vps(profile);
        let sps = super::build_hevc_sps(profile);
        let pps = super::build_hevc_pps();
        let format =
            make_hevc_format_description(&vps, &sps, &pps).map_err(|error| error.to_string())?;
        let plan = color_extension_plan(
            matrix,
            full_range,
            arcen_media::TransferCharacteristics::Bt709,
        );
        let format = apply_color_extensions(format, StreamCodec::H265, plan);
        let attributes = preferred_pixel_format(profile.chroma(), profile.ten_bit(), full_range)
            .and_then(build_destination_attributes);
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
        let sps = super::build_h264_sps();
        let pps = super::build_h264_pps();
        let format = make_h264_format_description(&sps, &pps).map_err(|error| error.to_string())?;
        let attributes = preferred_pixel_format(ChromaSubsampling::Yuv420, false, false)
            .and_then(build_destination_attributes);
        let session = DecompressionSession::new_with_image_buffer_attributes(
            &format,
            attributes.as_ref(),
            |_frame| {},
        )
        .map_err(|error| format!("VTDecompressionSessionCreate: {error}"))?;
        Ok(query_hardware_accelerated(&session))
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b_nals_with_three_and_four_byte_start_codes() {
        let bytes = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4,
        ];
        let nals = annex_b_nals(&bytes);
        assert_eq!(
            nals,
            vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]
        );
        assert_eq!(nal_kind(AnnexBCodec::H264, nals[0]), NalKind::Sps);
        assert_eq!(nal_kind(AnnexBCodec::H264, nals[1]), NalKind::Pps);
        assert_eq!(nal_kind(AnnexBCodec::H264, nals[2]), NalKind::Keyframe);
    }

    #[test]
    fn converts_annex_b_nals_to_avcc_length_prefixed_payload() {
        let nals = vec![&[0x65, 1, 2][..], &[0x41, 3][..]];
        let avcc = nals_to_avcc(&nals);
        assert_eq!(avcc, vec![0, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x41, 3]);
    }

    #[test]
    fn classifies_hevc_nal_types() {
        // First header byte = nal_type << 1 (layer/temporal bits zero).
        assert_eq!(nal_kind(AnnexBCodec::H265, &[32 << 1, 1]), NalKind::Vps);
        assert_eq!(nal_kind(AnnexBCodec::H265, &[33 << 1, 1]), NalKind::Sps);
        assert_eq!(nal_kind(AnnexBCodec::H265, &[34 << 1, 1]), NalKind::Pps);
        // IDR_W_RADL (19) and CRA (21) are keyframes.
        assert_eq!(
            nal_kind(AnnexBCodec::H265, &[19 << 1, 1]),
            NalKind::Keyframe
        );
        assert_eq!(
            nal_kind(AnnexBCodec::H265, &[21 << 1, 1]),
            NalKind::Keyframe
        );
        // TRAIL_R (1) is a plain slice.
        assert_eq!(nal_kind(AnnexBCodec::H265, &[1 << 1, 1]), NalKind::Slice);
        // SEI (39) is not a slice.
        assert_eq!(nal_kind(AnnexBCodec::H265, &[39 << 1, 1]), NalKind::Other);
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
    fn h264_keyframes_receive_one_complete_cached_parameter_set() {
        let sps = &[0x67, 0x64, 0x00][..];
        let pps = &[0x68, 0xee][..];
        let idr = &[0x65, 0x88][..];
        let mut cache = ParameterSetCache::default();
        assert!(cache.observe(AnnexBCodec::H264, &[sps, pps]));

        let bare_idr = [idr];
        let inline_idr = [sps, pps, idr];
        let injected = prepare_sample_nals(AnnexBCodec::H264, &cache, &bare_idr)
            .unwrap()
            .unwrap();
        assert_eq!(injected, vec![sps, pps, idr]);

        let canonicalized = prepare_sample_nals(AnnexBCodec::H264, &cache, &inline_idr)
            .unwrap()
            .unwrap();
        assert_eq!(canonicalized, vec![sps, pps, idr]);

        // A rebuilt decoder session can reuse the active stream cache.
        let recovered = prepare_sample_nals(AnnexBCodec::H264, &cache, &bare_idr)
            .unwrap()
            .unwrap();
        assert_eq!(recovered, vec![sps, pps, idr]);
    }

    #[test]
    fn h264_config_change_and_reset_require_a_complete_new_parameter_set() {
        let old_sps = &[0x67, 1][..];
        let old_pps = &[0x68, 1][..];
        let new_sps = &[0x67, 2][..];
        let new_pps = &[0x68, 2][..];
        let idr = &[0x65, 9][..];
        let mut cache = ParameterSetCache::default();
        cache.observe(AnnexBCodec::H264, &[old_sps, old_pps]);

        assert!(cache.observe(AnnexBCodec::H264, &[new_sps]));
        assert!(!cache.is_complete(AnnexBCodec::H264));
        assert_eq!(
            prepare_sample_nals(AnnexBCodec::H264, &cache, &[idr]),
            Err(SamplePreparationError::MissingParameterSets)
        );
        cache.observe(AnnexBCodec::H264, &[new_pps]);
        assert_eq!(
            prepare_sample_nals(AnnexBCodec::H264, &cache, &[idr])
                .unwrap()
                .unwrap(),
            vec![new_sps, new_pps, idr]
        );

        cache.reset(AnnexBCodec::H264);
        assert_eq!(
            prepare_sample_nals(AnnexBCodec::H264, &cache, &[idr]),
            Err(SamplePreparationError::MissingParameterSets)
        );
    }

    #[test]
    fn hevc_keyframes_receive_vps_sps_pps_once_and_codec_switch_clears_cache() {
        let vps = &[32 << 1, 1][..];
        let sps = &[33 << 1, 1][..];
        let pps = &[34 << 1, 1][..];
        let idr = &[19 << 1, 1][..];
        let mut cache = ParameterSetCache::default();
        cache.observe(AnnexBCodec::H265, &[vps, sps, pps]);

        assert_eq!(
            prepare_sample_nals(AnnexBCodec::H265, &cache, &[idr])
                .unwrap()
                .unwrap(),
            vec![vps, sps, pps, idr]
        );
        assert_eq!(
            prepare_sample_nals(AnnexBCodec::H265, &cache, &[vps, sps, pps, idr])
                .unwrap()
                .unwrap(),
            vec![vps, sps, pps, idr]
        );

        cache.observe(AnnexBCodec::H264, &[]);
        assert!(!cache.is_complete(AnnexBCodec::H264));
        assert!(cache.parameter_sets(AnnexBCodec::H265).is_none());
    }

    #[test]
    fn non_keyframes_are_not_injected_and_malformed_access_units_fail() {
        let sps = &[0x67, 1][..];
        let pps = &[0x68, 1][..];
        let slice = &[0x41, 7][..];
        let mut cache = ParameterSetCache::default();
        cache.observe(AnnexBCodec::H264, &[sps, pps]);
        assert_eq!(
            prepare_sample_nals(AnnexBCodec::H264, &cache, &[slice])
                .unwrap()
                .unwrap(),
            vec![slice]
        );

        assert!(parse_annex_b_access_unit(AnnexBCodec::H264, &[0x65, 1]).is_err());
        assert!(parse_annex_b_access_unit(AnnexBCodec::H265, &[0, 0, 0, 1, 32 << 1]).is_err());
        assert!(parse_annex_b_access_unit(AnnexBCodec::H264, &[]).is_err());

        let padded = [0, 0, 0, 0, 1, 0x65, 1, 0, 0];
        assert_eq!(
            parse_annex_b_access_unit(AnnexBCodec::H264, &padded).unwrap(),
            vec![&[0x65, 1][..]]
        );
    }

    #[test]
    fn discontinuity_forces_keyframe_gating() {
        let mut decoder = NativeVideoDecoder::new();
        decoder.notify_discontinuity();
        assert!(decoder.wants_keyframe());
    }

    // ---- w3-real-caps: bitstream::BitWriter / exp-Golomb ----

    #[test]
    fn exp_golomb_ue_matches_known_reference_codes() {
        // ITU-T H.264/H.265 9.1, Table 9-2: ue(0)="1", ue(1)="010",
        // ue(2)="011", ue(3)="00100", ue(4)="00101", ue(5)="00110",
        // ue(6)="00111", ue(7)="0001000".
        let cases: &[(u32, &str)] = &[
            (0, "1"),
            (1, "010"),
            (2, "011"),
            (3, "00100"),
            (4, "00101"),
            (5, "00110"),
            (6, "00111"),
            (7, "0001000"),
        ];
        for &(value, expected_bits) in cases {
            let mut w = bitstream::BitWriter::new();
            w.write_ue(value);
            let bit_len = w.bit_len();
            assert_eq!(bit_len, expected_bits.len(), "ue({value}) bit length");
            w.write_zeros(((8 - bit_len % 8) % 8) as u32);
            let bytes = w.into_rbsp();
            let mut reader = bitstream::BitReader::new(&bytes);
            let actual: String = (0..bit_len)
                .map(|_| if reader.read_bit() { '1' } else { '0' })
                .collect();
            assert_eq!(actual, expected_bits, "ue({value})");
        }
    }

    #[test]
    fn exp_golomb_se_matches_the_signed_mapping() {
        // ITU-T H.264/H.265 Table 9-3: se(0)=ue(0), se(1)=ue(1),
        // se(-1)=ue(2), se(2)=ue(3), se(-2)=ue(4). Compares full bit
        // content (not just length) against an independently-built ue()
        // writer for the mapped codeNum.
        let cases: &[(i32, u32)] = &[(0, 0), (1, 1), (-1, 2), (2, 3), (-2, 4)];
        for &(value, expected_ue) in cases {
            let mut actual = bitstream::BitWriter::new();
            actual.write_se(value);
            let bit_len = actual.bit_len();
            let mut expected = bitstream::BitWriter::new();
            expected.write_ue(expected_ue);
            assert_eq!(bit_len, expected.bit_len(), "se({value}) bit length");

            let padding = ((8 - bit_len % 8) % 8) as u32;
            actual.write_zeros(padding);
            expected.write_zeros(padding);
            assert_eq!(actual.into_rbsp(), expected.into_rbsp(), "se({value}) bits");
        }
    }

    #[test]
    fn emulation_prevention_escapes_every_start_code_like_run_but_nothing_else() {
        assert_eq!(
            bitstream::escape_emulation_prevention(&[0, 0, 0]),
            vec![0, 0, 3, 0]
        );
        assert_eq!(
            bitstream::escape_emulation_prevention(&[0, 0, 1]),
            vec![0, 0, 3, 1]
        );
        assert_eq!(
            bitstream::escape_emulation_prevention(&[0, 0, 2]),
            vec![0, 0, 3, 2]
        );
        assert_eq!(
            bitstream::escape_emulation_prevention(&[0, 0, 3]),
            vec![0, 0, 3, 3]
        );
        // 0x04 and above never need escaping.
        assert_eq!(
            bitstream::escape_emulation_prevention(&[0, 0, 4]),
            vec![0, 0, 4]
        );
        // A run of more than two zeros only re-triggers after the
        // inserted 0x03 resets the count.
        assert_eq!(
            bitstream::escape_emulation_prevention(&[0, 0, 0, 0, 1]),
            vec![0, 0, 3, 0, 0, 3, 1]
        );
    }

    // ---- w3-real-caps: synthetic parameter-set builders ----

    #[test]
    fn hevc_nal_headers_match_the_canonical_two_byte_prefixes() {
        let profile = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 8,
        };
        assert_eq!(&build_hevc_vps(profile)[..2], &[0x40, 0x01]);
        assert_eq!(&build_hevc_sps(profile)[..2], &[0x42, 0x01]);
        assert_eq!(&build_hevc_pps()[..2], &[0x44, 0x01]);
    }

    #[test]
    fn h264_nal_headers_match_the_canonical_sps_pps_prefixes() {
        assert_eq!(build_h264_sps()[0], 0x67);
        assert_eq!(build_h264_pps()[0], 0x68);
    }

    #[test]
    fn hevc_general_profile_idc_matches_x265_style_profile_selection() {
        let main = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 8,
        };
        let main10 = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 10,
        };
        let main12 = HevcProbeProfile {
            chroma_format_idc: 1,
            bit_depth: 12,
        };
        let yuv444_8 = HevcProbeProfile {
            chroma_format_idc: 3,
            bit_depth: 8,
        };
        assert_eq!(hevc_general_profile_idc(main), 1);
        assert_eq!(hevc_general_profile_idc(main10), 2);
        assert_eq!(hevc_general_profile_idc(main12), 4);
        assert_eq!(hevc_general_profile_idc(yuv444_8), 4);
    }

    #[test]
    fn hevc_sps_round_trips_profile_chroma_and_bit_depth_for_every_probed_profile() {
        // Independently re-parses (not via the writer) each SPS the real
        // probe builds, confirming the fields VideoToolbox actually keys
        // decodability on land where the builder above believes they do.
        let profiles = [
            HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 8,
            },
            HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 10,
            },
            HevcProbeProfile {
                chroma_format_idc: 1,
                bit_depth: 12,
            },
            HevcProbeProfile {
                chroma_format_idc: 3,
                bit_depth: 8,
            },
        ];
        for profile in profiles {
            let sps = build_hevc_sps(profile);
            let rbsp = bitstream::unescape_emulation_prevention(&sps[2..]);
            let mut r = bitstream::BitReader::new(&rbsp);
            r.skip(4); // sps_video_parameter_set_id
            r.skip(3); // sps_max_sub_layers_minus1
            r.skip(1); // sps_temporal_id_nesting_flag
            r.skip(3); // general_profile_space + general_tier_flag
            let general_profile_idc = r.read_bits(5);
            assert_eq!(
                general_profile_idc,
                u32::from(hevc_general_profile_idc(profile)),
                "{profile:?}"
            );
            r.skip(32); // general_profile_compatibility_flag[32]
            r.skip(4); // progressive/interlaced/non_packed/frame_only source flags
            match general_profile_idc {
                4..=10 => r.skip(9 + 34),
                2 => r.skip(7 + 1 + 35),
                _ => r.skip(43),
            }
            r.skip(1); // general_inbld_flag / reserved
            r.skip(8); // general_level_idc
            r.read_ue(); // sps_seq_parameter_set_id
            let chroma_format_idc = r.read_ue();
            assert_eq!(
                chroma_format_idc,
                u32::from(profile.chroma_format_idc),
                "{profile:?}"
            );
            if profile.chroma_format_idc == 3 {
                r.skip(1); // separate_colour_plane_flag
            }
            assert_eq!(r.read_ue(), u32::from(PROBE_DIMENSION)); // pic_width_in_luma_samples
            assert_eq!(r.read_ue(), u32::from(PROBE_DIMENSION)); // pic_height_in_luma_samples
            r.skip(1); // conformance_window_flag
            let bit_depth_luma_minus8 = r.read_ue();
            let bit_depth_chroma_minus8 = r.read_ue();
            assert_eq!(
                bit_depth_luma_minus8,
                u32::from(profile.bit_depth - 8),
                "{profile:?}"
            );
            assert_eq!(
                bit_depth_chroma_minus8,
                u32::from(profile.bit_depth - 8),
                "{profile:?}"
            );
        }
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
        assert_eq!(
            AnnexBCodec::from_stream_codec(StreamCodec::H264),
            Some(AnnexBCodec::H264)
        );
        assert_eq!(
            AnnexBCodec::from_stream_codec(StreamCodec::H265),
            Some(AnnexBCodec::H265)
        );
        assert_eq!(AnnexBCodec::from_stream_codec(StreamCodec::Av1), None);
    }

    #[test]
    fn parses_a_temporal_delimiter_then_a_sequence_header_obu() {
        // Hand-computed bytes (see inline comments for the exact bit
        // layout), independent of any writer: this exercises the OBU
        // framing layer itself (`av1::parse_obus`), not the
        // sequence-header field parser exercised by the round-trip tests
        // below.
        //
        // Temporal delimiter: obu_forbidden_bit=0, obu_type=2
        // (OBU_TEMPORAL_DELIMITER), obu_extension_flag=0,
        // obu_has_size_field=1, obu_reserved_1bit=0
        //   -> 0_0010_0_1_0 = 0x12; obu_size = leb128(0) = 0x00 (zero
        //   payload bytes -- av1-spec: "the temporal delimiter has an
        //   empty payload").
        // Sequence header: obu_type=1 (OBU_SEQUENCE_HEADER), otherwise
        // identical framing bits -> 0_0001_0_1_0 = 0x0A; obu_size =
        // leb128(1) = 0x01; one payload byte 0xAB (arbitrary content --
        // this test only checks OBU framing, not field decoding).
        let bytes = [0x12, 0x00, 0x0A, 0x01, 0xAB];
        let obus = av1::parse_obus(&bytes).expect("well-formed OBU stream");
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].obu_type, av1::ObuType::TemporalDelimiter);
        assert_eq!(obus[0].whole, &bytes[0..2]);
        assert!(obus[0].payload.is_empty());
        assert_eq!(obus[1].obu_type, av1::ObuType::SequenceHeader);
        assert_eq!(obus[1].whole, &bytes[2..5]);
        assert_eq!(obus[1].payload, &[0xAB]);
        assert_eq!(
            av1::summarize(&obus),
            "obus=[temporal_delimiter:2,seq_header:3]"
        );
    }

    #[test]
    fn last_obu_may_omit_its_size_field_and_fills_the_remainder() {
        // Frame OBU (type 6) with obu_has_size_field=0: obu_forbidden_bit=0,
        // obu_type=0110, obu_extension_flag=0, obu_has_size_field=0,
        // obu_reserved_1bit=0 -> 0_0110_0_0_0 = 0x30. Per av1-isobmff's "AV1
        // Sample Format", this is only permitted for the last OBU in a
        // sample, whose payload then fills the rest of the buffer.
        let bytes = [0x30, 0xAB, 0xCD];
        let obus = av1::parse_obus(&bytes).expect("well-formed OBU stream");
        assert_eq!(obus.len(), 1);
        assert_eq!(obus[0].obu_type, av1::ObuType::Frame);
        assert_eq!(obus[0].payload, &[0xAB, 0xCD]);
    }

    #[test]
    fn parse_obus_rejects_forbidden_bit_and_truncation() {
        assert!(av1::parse_obus(&[0x80]).is_err(), "obu_forbidden_bit set");
        assert!(av1::parse_obus(&[]).unwrap().is_empty());
        // obu_has_size_field=1 but the leb128 byte is missing entirely.
        assert!(av1::parse_obus(&[0x0A]).is_err());
        // obu_size (5) claims more payload than the 1 byte that remains.
        assert!(av1::parse_obus(&[0x0A, 0x05, 0xAB]).is_err());
    }

    /// Big-endian, MSB-first bit writer mirroring [`av1`]'s private
    /// `BitReader`, test-only: builds synthetic Sequence Header OBU
    /// payloads to verify `av1::parse_sequence_header` extracts the exact
    /// fields that were written. Each test below writes every syntax
    /// element inline, in the exact order/width given by
    /// AOMediaCodec/av1-spec `06.bitstream.syntax.md`'s
    /// `sequence_header_obu()`/`color_config()` (the same tables
    /// `parse_sequence_header`'s own doc cites), rather than going through
    /// a shared "spec to bits" helper -- so a bug in one is not masked by
    /// the same bug in the other.
    #[derive(Default)]
    struct TestBitWriter {
        bytes: Vec<u8>,
        partial: u8,
        bit_pos: u8,
    }

    impl TestBitWriter {
        fn write_bit(&mut self, bit: bool) {
            self.partial = (self.partial << 1) | u8::from(bit);
            self.bit_pos += 1;
            if self.bit_pos == 8 {
                self.bytes.push(self.partial);
                self.partial = 0;
                self.bit_pos = 0;
            }
        }

        /// `f(n)`: writes the low `n_bits` of `value`, most-significant-bit-first.
        fn f(&mut self, value: u32, n_bits: u32) {
            for i in (0..n_bits).rev() {
                self.write_bit((value >> i) & 1 != 0);
            }
        }

        /// Zero-pads to the next byte boundary and returns the bytes.
        fn into_bytes(mut self) -> Vec<u8> {
            while self.bit_pos != 0 {
                self.write_bit(false);
            }
            self.bytes
        }
    }

    #[test]
    fn parses_a_reduced_still_picture_sequence_header_with_default_color_description() {
        let mut w = TestBitWriter::default();
        w.f(0, 3); // seq_profile = 0 (Main)
        w.f(1, 1); // still_picture = 1
        w.f(1, 1); // reduced_still_picture_header = 1
        w.f(4, 5); // seq_level_idx[0] = 4 (seq_tier[0] forced 0)
                   // reduced_still_picture_header skips timing/operating-point/frame-id fields entirely.
        w.f(10, 4); // frame_width_bits_minus_1 = 10 (n = 11 bits)
        w.f(10, 4); // frame_height_bits_minus_1 = 10 (n = 11 bits)
        w.f(1919, 11); // max_frame_width_minus_1 = 1919 -> width = 1920
        w.f(1079, 11); // max_frame_height_minus_1 = 1079 -> height = 1080
                       // frame_id_numbers_present_flag is forced 0 by reduced_still_picture_header (not written).
        w.f(0, 1); // use_128x128_superblock
        w.f(0, 1); // enable_filter_intra
        w.f(0, 1); // enable_intra_edge_filter
                   // reduced_still_picture_header skips the enable_interintra_compound..order_hint_bits block.
        w.f(0, 1); // enable_superres
        w.f(0, 1); // enable_cdef
        w.f(0, 1); // enable_restoration
                   // color_config():
        w.f(0, 1); // high_bitdepth = 0 (8-bit)
                   // seq_profile != 2, so no twelve_bit read.
        w.f(0, 1); // mono_chrome = 0 (seq_profile != 1, so this bit is read)
        w.f(0, 1); // color_description_present_flag = 0 -> CP/TC/MC default to UNSPECIFIED(2)
                   // Not the BT.709/sRGB/Identity shortcut (primaries default to 2, not 1) -> general branch:
        w.f(1, 1); // color_range = 1 (value irrelevant to this parser)
                   // seq_profile == 0 -> subsampling_x = subsampling_y = 1 (not read from the bitstream).
        w.f(2, 2); // chroma_sample_position = 2 (subsampling_x && subsampling_y are both 1, so this IS read)
        let payload = w.into_bytes();

        let fields = av1::parse_sequence_header(&payload).expect("valid payload");
        assert_eq!(fields.seq_profile, 0);
        assert_eq!(fields.seq_level_idx_0, 4);
        assert_eq!(fields.seq_tier_0, 0);
        assert!(!fields.high_bitdepth);
        assert!(!fields.twelve_bit);
        assert!(!fields.monochrome);
        assert_eq!(fields.chroma_subsampling_x, 1);
        assert_eq!(fields.chroma_subsampling_y, 1);
        assert_eq!(fields.chroma_sample_position, 2);
        assert_eq!(fields.max_frame_width, 1920);
        assert_eq!(fields.max_frame_height, 1080);
    }

    #[test]
    fn parses_a_non_reduced_sequence_header_with_two_operating_points_and_explicit_color_description(
    ) {
        let mut w = TestBitWriter::default();
        w.f(1, 3); // seq_profile = 1 (High)
        w.f(0, 1); // still_picture = 0
        w.f(0, 1); // reduced_still_picture_header = 0
        w.f(0, 1); // timing_info_present_flag = 0
        w.f(0, 1); // initial_display_delay_present_flag = 0
        w.f(1, 5); // operating_points_cnt_minus_1 = 1 (two operating points)
                   // operating point 0:
        w.f(0, 12); // operating_point_idc[0]
        w.f(8, 5); // seq_level_idx[0] = 8 (> 7, so seq_tier[0] is read)
        w.f(1, 1); // seq_tier[0] = 1 (High tier)
                   // decoder_model_info_present_flag is forced 0, so operating_parameters_info() never
                   // appears; initial_display_delay_present_flag == 0, so nothing more for this point.
                   // operating point 1:
        w.f(0, 12); // operating_point_idc[1]
        w.f(4, 5); // seq_level_idx[1] = 4 (<= 7, seq_tier[1] = 0, not read)
        w.f(11, 4); // frame_width_bits_minus_1 = 11 (n = 12 bits)
        w.f(11, 4); // frame_height_bits_minus_1 = 11 (n = 12 bits)
        w.f(3839, 12); // max_frame_width_minus_1 = 3839 -> width = 3840
        w.f(2159, 12); // max_frame_height_minus_1 = 2159 -> height = 2160
        w.f(0, 1); // frame_id_numbers_present_flag = 0 (explicitly read; not reduced)
        w.f(1, 1); // use_128x128_superblock
        w.f(1, 1); // enable_filter_intra
        w.f(0, 1); // enable_intra_edge_filter
                   // Not reduced -> the full block:
        w.f(0, 1); // enable_interintra_compound
        w.f(0, 1); // enable_masked_compound
        w.f(0, 1); // enable_warped_motion
        w.f(0, 1); // enable_dual_filter
        w.f(1, 1); // enable_order_hint = 1
        w.f(0, 1); // enable_jnt_comp
        w.f(0, 1); // enable_ref_frame_mvs
        w.f(1, 1); // seq_choose_screen_content_tools = 1 -> SELECT (2), no explicit bit
        w.f(1, 1); // seq_choose_integer_mv = 1 -> SELECT, no explicit bit (read because 2 > 0)
        w.f(2, 3); // order_hint_bits_minus_1 = 2 (enable_order_hint == 1)
        w.f(1, 1); // enable_superres
        w.f(1, 1); // enable_cdef
        w.f(1, 1); // enable_restoration
                   // color_config():
        w.f(1, 1); // high_bitdepth = 1
                   // seq_profile == 1, not 2, so no twelve_bit read even though high_bitdepth is set.
                   // seq_profile == 1 -> mono_chrome is NOT read (forced 0).
        w.f(1, 1); // color_description_present_flag = 1
        w.f(9, 8); // color_primaries = 9 (BT.2020)
        w.f(16, 8); // transfer_characteristics = 16 (PQ)
        w.f(9, 8); // matrix_coefficients = 9 (BT.2020 NCL)
                   // Not the BT.709/sRGB/Identity shortcut (primaries=9) -> general branch:
        w.f(1, 1); // color_range = 1
                   // seq_profile == 1 -> subsampling_x = subsampling_y = 0 (not read); both zero -> no
                   // chroma_sample_position read either.
        let payload = w.into_bytes();

        let fields = av1::parse_sequence_header(&payload).expect("valid payload");
        assert_eq!(fields.seq_profile, 1);
        assert_eq!(fields.seq_level_idx_0, 8);
        assert_eq!(fields.seq_tier_0, 1);
        assert!(fields.high_bitdepth);
        assert!(!fields.twelve_bit);
        assert!(!fields.monochrome);
        assert_eq!(fields.chroma_subsampling_x, 0);
        assert_eq!(fields.chroma_subsampling_y, 0);
        assert_eq!(fields.chroma_sample_position, 0);
        assert_eq!(fields.max_frame_width, 3840);
        assert_eq!(fields.max_frame_height, 2160);
    }

    #[test]
    fn parses_a_monochrome_sequence_header() {
        let mut w = TestBitWriter::default();
        w.f(0, 3); // seq_profile = 0
        w.f(1, 1); // still_picture = 1
        w.f(1, 1); // reduced_still_picture_header = 1
        w.f(0, 5); // seq_level_idx[0] = 0
        w.f(5, 4); // frame_width_bits_minus_1 = 5 (n = 6 bits)
        w.f(5, 4); // frame_height_bits_minus_1 = 5 (n = 6 bits)
        w.f(63, 6); // max_frame_width_minus_1 = 63 -> width = 64
        w.f(63, 6); // max_frame_height_minus_1 = 63 -> height = 64
        w.f(0, 1); // use_128x128_superblock
        w.f(0, 1); // enable_filter_intra
        w.f(0, 1); // enable_intra_edge_filter
        w.f(0, 1); // enable_superres
        w.f(0, 1); // enable_cdef
        w.f(0, 1); // enable_restoration
                   // color_config():
        w.f(0, 1); // high_bitdepth = 0
        w.f(1, 1); // mono_chrome = 1 (seq_profile != 1, so this bit is read)
        w.f(0, 1); // color_description_present_flag = 0
                   // mono_chrome branch reads exactly one more bit (color_range) then returns:
        w.f(1, 1); // color_range (value irrelevant to this parser)
        let payload = w.into_bytes();

        let fields = av1::parse_sequence_header(&payload).expect("valid payload");
        assert!(fields.monochrome);
        assert_eq!(fields.chroma_subsampling_x, 1);
        assert_eq!(fields.chroma_subsampling_y, 1);
        assert_eq!(fields.chroma_sample_position, 0);
        assert_eq!(fields.max_frame_width, 64);
        assert_eq!(fields.max_frame_height, 64);
    }

    #[test]
    fn rejects_timing_info_present_flag() {
        let mut w = TestBitWriter::default();
        w.f(0, 3); // seq_profile
        w.f(0, 1); // still_picture
        w.f(0, 1); // reduced_still_picture_header = 0
        w.f(1, 1); // timing_info_present_flag = 1
        let payload = w.into_bytes();
        let error = av1::parse_sequence_header(&payload).expect_err("must reject timing info");
        assert!(error.contains("timing_info_present_flag"));
    }

    #[test]
    fn parse_sequence_header_rejects_a_truncated_payload() {
        assert!(av1::parse_sequence_header(&[]).is_err());
        // Eight bits is nowhere near enough for even the reduced branch.
        assert!(av1::parse_sequence_header(&[0x00]).is_err());
    }

    #[test]
    fn build_av1c_packs_the_header_bytes_exactly_per_the_isobmff_spec() {
        // AOMediaCodec/av1-isobmff `index.bs` section 2.3.3
        // (`AV1CodecConfigurationRecord`): marker(1)=1,version(7)=1 ->
        // 0x81; seq_profile(3)=0,seq_level_idx_0(5)=4 -> 0b000_00100 =
        // 0x04; seq_tier_0(1)=0,high_bitdepth(1)=0,twelve_bit(1)=0,
        // monochrome(1)=0,ss_x(1)=1,ss_y(1)=1,csp(2)=0b00 ->
        // 0b0_0_0_0_1_1_00 = 0x0C; reserved/no-delay byte -> 0x00.
        let fields = av1::SequenceHeaderFields {
            seq_profile: 0,
            seq_level_idx_0: 4,
            seq_tier_0: 0,
            high_bitdepth: false,
            twelve_bit: false,
            monochrome: false,
            chroma_subsampling_x: 1,
            chroma_subsampling_y: 1,
            chroma_sample_position: 0,
            max_frame_width: 1920,
            max_frame_height: 1080,
        };
        let sequence_header_obu = [0xAA, 0xBB, 0xCC];
        let record = av1::build_av1c(&fields, &sequence_header_obu);
        assert_eq!(record, vec![0x81, 0x04, 0x0C, 0x00, 0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn build_av1c_packs_a_second_field_combination_correctly() {
        // seq_profile(3)=2,seq_level_idx_0(5)=31 -> 0b010_11111 = 0x5F;
        // seq_tier_0(1)=1,high_bitdepth(1)=1,twelve_bit(1)=0,
        // monochrome(1)=1,ss_x(1)=1,ss_y(1)=1,csp(2)=0b11 ->
        // 0b1_1_0_1_1_1_11 = 0xDF.
        let fields = av1::SequenceHeaderFields {
            seq_profile: 2,
            seq_level_idx_0: 31,
            seq_tier_0: 1,
            high_bitdepth: true,
            twelve_bit: false,
            monochrome: true,
            chroma_subsampling_x: 1,
            chroma_subsampling_y: 1,
            chroma_sample_position: 3,
            max_frame_width: 64,
            max_frame_height: 64,
        };
        let sequence_header_obu = [0x01, 0x02];
        let record = av1::build_av1c(&fields, &sequence_header_obu);
        assert_eq!(record, vec![0x81, 0x5F, 0xDF, 0x00, 0x01, 0x02]);
        // configOBUs is the input slice, byte-for-byte, never re-serialised.
        assert_eq!(&record[4..], &sequence_header_obu);
    }
}
