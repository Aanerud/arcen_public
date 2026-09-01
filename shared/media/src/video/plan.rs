use std::collections::BTreeMap;
use std::error::Error;
use std::fmt::{Display, Formatter};

use arcen_protocol::messages::CursorMode;
use arcen_telemetry::CorrelationId;

use crate::{
    BitDepth, BitDepthSet, ChromaSet, ChromaSubsampling, CodecSet, ColorMatrix, ColorPrimaries,
    ColorRange, ColorRangeSet, TransferCharacteristics, VideoCodec, VideoConfiguration,
};

const READY_PREFIX: &str = "[capenc] READY ";
const UNAVAILABLE_PREFIX: &str = "[capenc] UNAVAILABLE ";

/// READY protocol version.
///
/// Bumped from 1 to 2 when the line gained bit depth, colour range, matrix,
/// primaries and transfer. There is deliberately no v1 compatibility path: a
/// capenc that predates colour negotiation cannot state what colour it is
/// producing, and silently assuming 8-bit limited is exactly the failure this
/// work exists to remove.
const READY_VERSION: &str = "2";

/// User or host-policy encoder request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EncoderRequest {
    #[default]
    Auto,
    NativeNvenc,
    WindowsMediaFoundation,
    SoftwareH264,
    SoftwareAv1,
}

impl EncoderRequest {
    #[must_use]
    pub const fn as_arg(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::NativeNvenc => "nvenc",
            Self::WindowsMediaFoundation => "mf",
            Self::SoftwareH264 => "software-h264",
            Self::SoftwareAv1 => "software-av1",
        }
    }

    #[must_use]
    pub const fn accepts(self, backend: EncoderBackend) -> bool {
        matches!(self, Self::Auto)
            || matches!(
                (self, backend),
                (Self::NativeNvenc, EncoderBackend::NativeNvenc)
                    | (
                        Self::WindowsMediaFoundation,
                        EncoderBackend::WindowsMediaFoundation
                    )
                    | (Self::SoftwareH264, EncoderBackend::OpenH264)
                    | (Self::SoftwareAv1, EncoderBackend::Rav1e)
            )
    }
}

/// Concrete encoder selected before hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    NativeNvenc,
    WindowsMediaFoundation,
    OpenH264,
    /// Pure-Rust AV1 software encoder. The only backend with a 12-bit path.
    Rav1e,
}

/// Whether a backend encodes on dedicated silicon or on the CPU.
///
/// This exists so clients stop inferring it from the backend's *name*. A
/// substring test for "native" happens to classify today's three backends
/// correctly and would silently mislabel a future `amf-h264` or `vaapi-av1` as
/// a software fallback. The class is a property of the backend, so the backend
/// states it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceleratorClass {
    /// Dedicated encode silicon: NVENC, AMF, `QuickSync`, VA-API and successors.
    Hardware,
    /// CPU encode.
    Software,
}

impl AcceleratorClass {
    /// Stable wire token. Kept separate from `Debug` so renaming the variant
    /// cannot silently change the protocol.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Hardware => "hardware",
            Self::Software => "software",
        }
    }

    /// Parse a wire token. Unknown values are `None` so a newer host cannot
    /// have its class silently misread as software by an older client.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        match value {
            "hardware" => Some(Self::Hardware),
            "software" => Some(Self::Software),
            _ => None,
        }
    }
}

/// Which capture path actually delivered the desktop image to the encoder.
///
/// Deliberately separate from [`EncoderBackend`]: that names what *encoded*
/// the frame, and the two were conflated in the READY line, which is why a
/// host log could say `native-nvenc` while saying nothing about whether the
/// pixels arrived via a zero-copy GPU path or a host round trip.
///
/// The distinction is the whole 8-bit speed argument. `NvFBC` and Desktop
/// Duplication hand frames to the encoder without leaving the GPU; `XShm` is a
/// host copy, and every wide-source route measured so far gives the zero-copy
/// path up. A log that cannot name the path cannot price the trade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureBackend {
    /// NVIDIA Frame Buffer Capture into CUDA device memory (Linux).
    NvFbc,
    /// X11 MIT-SHM into host memory (Linux).
    XShm,
    /// DXGI Desktop Duplication (Windows).
    DesktopDuplication,
    /// Windows Graphics Capture (Windows).
    WindowsGraphicsCapture,
}

impl CaptureBackend {
    /// Stable wire token. Kept separate from `Debug` so renaming a variant
    /// cannot silently change the protocol.
    #[must_use]
    pub const fn ready_token(self) -> &'static str {
        match self {
            Self::NvFbc => "nvfbc",
            Self::XShm => "xshm",
            Self::DesktopDuplication => "dxgi-dda",
            Self::WindowsGraphicsCapture => "wgc",
        }
    }

    /// Parse a wire token. Unknown values are `None` rather than a guess: a
    /// newer capenc naming a path this build does not know must read as
    /// "unreported", not as some other path.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        match value {
            "nvfbc" => Some(Self::NvFbc),
            "xshm" => Some(Self::XShm),
            "dxgi-dda" => Some(Self::DesktopDuplication),
            "wgc" => Some(Self::WindowsGraphicsCapture),
            _ => None,
        }
    }

    /// Whether frames reach the encoder without a host round trip.
    ///
    /// `NvFBC` grabs straight into CUDA device memory and Desktop Duplication
    /// yields a D3D11 texture the encoder can read in place. `XShm` copies
    /// through shared host memory, and WGC is treated as a copy because the
    /// production path stages its frames rather than encoding them in place.
    #[must_use]
    pub const fn zero_copy(self) -> bool {
        match self {
            Self::NvFbc | Self::DesktopDuplication => true,
            Self::XShm | Self::WindowsGraphicsCapture => false,
        }
    }
}

impl EncoderBackend {
    /// Parses the exact backend token emitted by READY and media rosters.
    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        match value {
            "native-nvenc" => Some(Self::NativeNvenc),
            "media-foundation-sw-h264" => Some(Self::WindowsMediaFoundation),
            "openh264-sw-h264" => Some(Self::OpenH264),
            "rav1e-sw-av1" => Some(Self::Rav1e),
            _ => None,
        }
    }

    #[must_use]
    pub const fn ready_token(self) -> &'static str {
        match self {
            Self::NativeNvenc => "native-nvenc",
            Self::WindowsMediaFoundation => "media-foundation-sw-h264",
            Self::OpenH264 => "openh264-sw-h264",
            Self::Rav1e => "rav1e-sw-av1",
        }
    }

    /// Whether this backend encodes on dedicated silicon.
    ///
    /// A new vendor backend sets this once here, and every client that reports
    /// the media path is correct for it without changing.
    #[must_use]
    pub const fn accelerator_class(self) -> AcceleratorClass {
        match self {
            Self::NativeNvenc => AcceleratorClass::Hardware,
            Self::WindowsMediaFoundation | Self::OpenH264 | Self::Rav1e => {
                AcceleratorClass::Software
            }
        }
    }

    /// The inherent capability contract of this backend.
    ///
    /// **This is the one row a new backend adds.** It is the widest thing the
    /// backend could ever do, independent of the machine it runs on. A runtime
    /// probe reports what a *particular* host can do and may be narrower; it is
    /// intersected with this via [`BackendLimits::narrowed_to`] and is never
    /// allowed to be wider. Validation of a child's READY line is checked
    /// against this contract, so the rules live in one table rather than in
    /// hand-written conditionals spread across the parser.
    #[must_use]
    pub const fn contract(self) -> BackendLimits {
        match self {
            // NVENC: both Annex-B codecs plus AV1 from Ada Lovelace onward,
            // 4:2:2/4:4:4 via HEVC Rext, and 10-bit from Turing onward. There
            // is deliberately no 12-bit entry -- `NV_ENC_BIT_DEPTH` defines
            // only 8 and 10, and no NVIDIA GPU encodes 12-bit at any
            // subsampling. Geometry and rate bounds are generous; the per-GPU
            // probe narrows all of this.
            //
            // AV1 is narrower than this shared `chroma` set states: NVENC
            // exposes only `NV_ENC_AV1_PROFILE_MAIN_GUID`, which is 4:2:0
            // only (no AV1 High/Professional GUID exists here for 4:4:4).
            // `chroma`/`bit_depths` are one set per *backend*, not per codec,
            // so this contract cannot say "HEVC reaches 4:4:4 but AV1 does
            // not" without either over-claiming for AV1 or under-claiming for
            // HEVC's genuine Rext support -- see `codec_chroma_ceiling`, the
            // narrow, explicit, codec-specific refinement `resolve_available`/
            // `degrade_to_limits`/`parse_ready_v1` all consult so a plan can
            // never resolve to AV1 4:4:4 on this backend (it would fail at
            // `NvEncInitializeEncoder`). Ada's AV1 Main profile genuinely
            // reaches 10-bit like HEVC Main10, so `bit_depths` needs no
            // similar per-codec carve-out; NVENC has no 12-bit path for any
            // codec, so that axis already excludes AV1 12-bit for free.
            Self::NativeNvenc => BackendLimits {
                codecs: CodecSet::from_slice(&[
                    VideoCodec::H264,
                    VideoCodec::H265,
                    VideoCodec::Av1,
                ]),
                // 4:2:2 is deliberately absent, and that is a *build* fact
                // rather than a hardware one. Blackwell silicon encodes HEVC
                // and H.264 4:2:2, but the surface formats it needs
                // (`NV_ENC_BUFFER_FORMAT_NV16`, `P210`), the capability enum
                // (`NV_ENC_CAPS_SUPPORT_YUV422_ENCODE`) and the Ultra High
                // Quality tuning all arrived in **Video Codec SDK 13.0**, and
                // the bindings vendored here are **NVENCAPI 12.1**. There is
                // literally no constant to name a 4:2:2 surface with.
                //
                // Claiming 4:2:2 here anyway would resolve a plan, emit a
                // READY line promising it, and only then fail at encoder
                // init — advertising a capability this build cannot deliver,
                // which is exactly the class of silent over-claim the colour
                // work exists to remove. It is restored by updating the
                // bindings; see `docs/architecture/nvenc-sdk13-blackwell.md`.
                chroma: ChromaSet::from_slice(&[
                    ChromaSubsampling::Yuv420,
                    ChromaSubsampling::Yuv444,
                ]),
                bit_depths: BitDepthSet::from_slice(&[BitDepth::Eight, BitDepth::Ten]),
                ranges: ColorRangeSet::from_slice(&[ColorRange::Limited, ColorRange::Full]),
                // NV_ENC_VUI_MATRIX_COEFFS_RGB exists, and 4:4:4 surfaces can
                // carry G/B/R. Whether the result survives the client is a
                // probe-matrix question, not a contract question. AV1 cannot
                // reach an identity matrix on this backend either way: that
                // requires 4:4:4 (`ColorMatrix::is_identity`), which
                // `codec_chroma_ceiling` already refuses for AV1.
                identity_matrix: true,
                max_width: 8192,
                max_height: 8192,
                max_fps: 240,
                cursor_in_video: true,
            },
            // Inbox Media Foundation software H.264: 4:2:0 8-bit only. It can
            // at least signal range correctly via MF_MT_VIDEO_NOMINAL_RANGE.
            Self::WindowsMediaFoundation => BackendLimits {
                codecs: CodecSet::from_slice(&[VideoCodec::H264]),
                chroma: ChromaSet::from_slice(&[ChromaSubsampling::Yuv420]),
                bit_depths: BitDepthSet::from_slice(&[BitDepth::Eight]),
                ranges: ColorRangeSet::from_slice(&[ColorRange::Limited, ColorRange::Full]),
                identity_matrix: false,
                max_width: 4096,
                max_height: 4096,
                max_fps: 60,
                cursor_in_video: false,
            },
            // The portable OpenH264 contract: H.264 Baseline, 4:2:0, 8-bit,
            // at most 1920x1200 at 30 fps, cursor composited by the client.
            // Baseline cannot carry 4:4:4 or any depth above eight.
            Self::OpenH264 => BackendLimits {
                codecs: CodecSet::from_slice(&[VideoCodec::H264]),
                chroma: ChromaSet::from_slice(&[ChromaSubsampling::Yuv420]),
                bit_depths: BitDepthSet::from_slice(&[BitDepth::Eight]),
                ranges: ColorRangeSet::from_slice(&[ColorRange::Limited, ColorRange::Full]),
                identity_matrix: false,
                max_width: 1920,
                max_height: 1200,
                max_fps: 30,
                cursor_in_video: false,
            },
            // rav1e: pure-Rust AV1. The only backend Arcen has with a genuine
            // 12-bit and 4:4:4 path, and the only route to 12-bit anywhere in
            // the product. CPU-bound, so the geometry ceiling is modest and
            // the real limit is measured, not declared.
            Self::Rav1e => BackendLimits {
                codecs: CodecSet::from_slice(&[VideoCodec::Av1]),
                chroma: ChromaSet::from_slice(&[
                    ChromaSubsampling::Yuv420,
                    ChromaSubsampling::Yuv422,
                    ChromaSubsampling::Yuv444,
                ]),
                bit_depths: BitDepthSet::from_slice(&[
                    BitDepth::Eight,
                    BitDepth::Ten,
                    BitDepth::Twelve,
                ]),
                ranges: ColorRangeSet::from_slice(&[ColorRange::Limited, ColorRange::Full]),
                identity_matrix: true,
                max_width: 3840,
                max_height: 2160,
                max_fps: 60,
                cursor_in_video: false,
            },
        }
    }
}

/// Strict, typed pre-READY backend-unavailability notice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendUnavailableNotice {
    pub backend: EncoderBackend,
    pub reason: BackendUnavailableReason,
}

/// Typed reason a probe did not make a backend available.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendUnavailableReason {
    NotBuilt,
    RuntimeMissing,
    HardwareUnavailable,
    SessionLimit,
    UnsupportedDisplay,
    UnsupportedConfiguration,
}

/// Stable capabilities and limits for one concrete backend.
///
/// Capability is a *set*, not one boolean per codec. Adding a codec therefore
/// changes this type not at all: a backend that gains support for it adds it to
/// its [`EncoderBackend::contract`] row, and everything that merely carries or
/// checks capability keeps working unchanged. The same reasoning extends to
/// chroma, bit depth and colour range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendLimits {
    /// Codecs this backend can encode.
    pub codecs: CodecSet,
    /// Chroma formats this backend can encode.
    pub chroma: ChromaSet,
    /// Bit depths this backend can encode.
    pub bit_depths: BitDepthSet,
    /// Coded sample ranges this backend can emit.
    pub ranges: ColorRangeSet,
    /// Whether this backend can encode with an identity (GBR) matrix.
    pub identity_matrix: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    pub cursor_in_video: bool,
}

impl BackendLimits {
    /// Whether this backend can encode `codec`.
    #[must_use]
    pub const fn supports(self, codec: VideoCodec) -> bool {
        self.codecs.contains(codec)
    }

    /// Whether this backend can encode `chroma`.
    #[must_use]
    pub const fn supports_chroma(self, chroma: ChromaSubsampling) -> bool {
        self.chroma.contains(chroma)
    }

    /// Whether this backend can encode `depth`.
    #[must_use]
    pub const fn supports_bit_depth(self, depth: BitDepth) -> bool {
        self.bit_depths.contains(depth)
    }

    /// Whether this backend can emit `range`.
    #[must_use]
    pub const fn supports_range(self, range: ColorRange) -> bool {
        self.ranges.contains(range)
    }

    /// Whether this backend can encode `matrix`.
    #[must_use]
    pub const fn supports_matrix(self, matrix: ColorMatrix) -> bool {
        !matrix.is_identity() || self.identity_matrix
    }

    /// Narrow these limits to what `other` also allows.
    ///
    /// A runtime probe may report less than the backend contract, never more,
    /// so probe results are intersected with the contract rather than trusted.
    #[must_use]
    pub fn narrowed_to(self, other: Self) -> Self {
        Self {
            codecs: self.codecs.intersection(other.codecs),
            chroma: ChromaSet::from_slice(
                &ChromaSubsampling::ALL
                    .iter()
                    .copied()
                    .filter(|c| self.chroma.contains(*c) && other.chroma.contains(*c))
                    .collect::<Vec<_>>(),
            ),
            bit_depths: self.bit_depths.intersection(other.bit_depths),
            ranges: self.ranges.intersection(other.ranges),
            identity_matrix: self.identity_matrix && other.identity_matrix,
            max_width: self.max_width.min(other.max_width),
            max_height: self.max_height.min(other.max_height),
            max_fps: self.max_fps.min(other.max_fps),
            cursor_in_video: self.cursor_in_video && other.cursor_in_video,
        }
    }
}

/// Side-effect-free availability result supplied by a platform probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendAvailability {
    Available(BackendLimits),
    Unavailable(BackendUnavailableReason),
}

/// One ordered platform candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackendCandidate {
    pub backend: EncoderBackend,
    pub availability: BackendAvailability,
}

/// Requested attachment video contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaRequest {
    pub encoder: EncoderRequest,
    pub video: VideoConfiguration,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub cursor_mode: CursorMode,
}

/// Exact attachment video truth used by hello and every frame.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedMediaPlan {
    pub backend: EncoderBackend,
    pub video: VideoConfiguration,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub cursor_mode: CursorMode,
    pub cursor_in_video: bool,
    /// Codecs the selected backend can encode. Carried so hello and frame
    /// headers report the backend's real capability rather than re-deriving it.
    pub codecs: CodecSet,
    /// Chroma formats the selected backend can encode.
    pub chroma: ChromaSet,
    /// Bit depths the selected backend can encode.
    pub bit_depths: BitDepthSet,
    /// Coded sample ranges the selected backend can emit.
    pub ranges: ColorRangeSet,
}

impl ResolvedMediaPlan {
    /// Whether the selected backend can encode `codec`.
    #[must_use]
    pub const fn supports(self, codec: VideoCodec) -> bool {
        self.codecs.contains(codec)
    }

    /// Wire compatibility accessor. `server_hello` still carries three named
    /// booleans for peers that predate capability sets; they are derived here
    /// rather than stored, so they cannot drift from the set.
    #[must_use]
    pub const fn supports_h264(self) -> bool {
        self.codecs.contains(VideoCodec::H264)
    }

    /// Wire compatibility accessor. See [`ResolvedMediaPlan::supports_h264`].
    #[must_use]
    pub const fn supports_h265(self) -> bool {
        self.codecs.contains(VideoCodec::H265)
    }

    /// Wire compatibility accessor. See [`ResolvedMediaPlan::supports_h264`].
    #[must_use]
    pub const fn supports_av1(self) -> bool {
        self.codecs.contains(VideoCodec::Av1)
    }

    /// Wire compatibility accessor. See [`ResolvedMediaPlan::supports_h264`].
    #[must_use]
    pub const fn supports_yuv444(self) -> bool {
        self.chroma.contains(ChromaSubsampling::Yuv444)
    }

    /// Whether the selected backend can encode ten-bit.
    #[must_use]
    pub const fn supports_main10(self) -> bool {
        self.bit_depths.contains(BitDepth::Ten)
    }

    /// Whether the selected backend can emit full-range samples.
    #[must_use]
    pub const fn supports_full_range(self) -> bool {
        self.ranges.contains(ColorRange::Full)
    }

    /// Stable token for the resolved bit depth.
    #[must_use]
    pub const fn bit_depth_token(self) -> &'static str {
        self.video.bit_depth.token()
    }

    /// Stable token for the resolved colour range.
    #[must_use]
    pub const fn range_token(self) -> &'static str {
        self.video.range.token()
    }

    /// Stable token for the resolved matrix coefficients.
    #[must_use]
    pub const fn matrix_token(self) -> &'static str {
        self.video.matrix.token()
    }

    /// Stable token for the resolved colour primaries.
    #[must_use]
    pub const fn primaries_token(self) -> &'static str {
        self.video.primaries.token()
    }

    /// Stable token for the resolved transfer characteristics.
    #[must_use]
    pub const fn transfer_token(self) -> &'static str {
        self.video.transfer.token()
    }

    #[must_use]
    pub const fn codec_token(self) -> &'static str {
        match self.video.codec {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "h265",
            VideoCodec::Jpeg => "jpeg",
            VideoCodec::Vp9 => "vp9",
            VideoCodec::Av1 => "av1",
        }
    }

    #[must_use]
    pub const fn chroma_token(self) -> &'static str {
        match self.video.chroma {
            ChromaSubsampling::Yuv420 => "yuv420",
            ChromaSubsampling::Yuv422 => "yuv422",
            ChromaSubsampling::Yuv444 => "yuv444",
        }
    }
}

/// Pure plan-resolution failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaPlanError {
    InvalidGeometry,
    UnsupportedFormat,
    BackendUnavailable(BackendUnavailableReason),
    RequestedBackendMissing,
    NoBackendAvailable,
}

impl Display for MediaPlanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidGeometry => formatter.write_str("invalid requested video geometry"),
            Self::UnsupportedFormat => {
                formatter.write_str("backend does not support the requested video format")
            }
            Self::BackendUnavailable(reason) => {
                write!(formatter, "requested backend is unavailable: {reason:?}")
            }
            Self::RequestedBackendMissing => {
                formatter.write_str("requested backend is absent from platform candidates")
            }
            Self::NoBackendAvailable => formatter.write_str("no encoder backend is available"),
        }
    }
}

impl Error for MediaPlanError {}

/// The chroma ceiling for `codec` specifically on `backend`, when narrower
/// than `backend`'s shared [`BackendLimits::chroma`].
///
/// [`BackendLimits`] states one chroma set per *backend*, not per codec,
/// because every backend before NVENC's AV1 addition only ever needed one:
/// H.264 and HEVC both reach 4:4:4 on NVENC, Media Foundation/OpenH264 only
/// ever offer H.264, and rav1e only ever offers AV1. NVENC is the first
/// backend where two codecs it supports disagree -- HEVC genuinely reaches
/// 4:4:4 via Rext, but the SDK exposes only `NV_ENC_AV1_PROFILE_MAIN_GUID`
/// for AV1, which is 4:2:0-only. Reshaping `BackendLimits` into a per-codec
/// map would ripple into every `.contract().chroma`/`.chroma` reader outside
/// this module (`hosts/linux`, `hosts/windows`), so this is instead the one
/// narrow, explicit refinement consulted ahead of the shared set, independent
/// of whatever a runtime probe narrowed `limits` to.
/// [`ChromaSubsampling::ALL`] everywhere else is a deliberate no-op: no other
/// backend or codec is narrowed by it.
const fn codec_chroma_ceiling(backend: EncoderBackend, codec: VideoCodec) -> ChromaSet {
    match (backend, codec) {
        (EncoderBackend::NativeNvenc, VideoCodec::Av1) => {
            ChromaSet::from_slice(&[ChromaSubsampling::Yuv420])
        }
        _ => ChromaSet::from_slice(ChromaSubsampling::ALL),
    }
}

fn validate_request(request: MediaRequest) -> Result<(), MediaPlanError> {
    if request.width == 0 || request.height == 0 || request.fps == 0 {
        return Err(MediaPlanError::InvalidGeometry);
    }
    if matches!(request.video.chroma, ChromaSubsampling::Yuv420)
        && (request.width | request.height) & 1 != 0
    {
        return Err(MediaPlanError::InvalidGeometry);
    }
    Ok(())
}

fn resolve_available(
    request: MediaRequest,
    backend: EncoderBackend,
    limits: BackendLimits,
) -> Result<ResolvedMediaPlan, MediaPlanError> {
    // Table-driven: the backend says which codecs, chroma, depths and ranges
    // it can encode, and the codec says which of those Arcen offers for it.
    // Adding a codec or a depth needs no edit here. `codec_chroma_ceiling` is
    // the one exception -- see its doc -- for the one backend/codec pair
    // whose real chroma ceiling is narrower than the backend's shared set.
    let format_supported = limits.supports(request.video.codec)
        && limits.supports_chroma(request.video.chroma)
        && limits.supports_bit_depth(request.video.bit_depth)
        && limits.supports_range(request.video.range)
        && limits.supports_matrix(request.video.matrix)
        && request
            .video
            .codec
            .offered_chroma()
            .contains(request.video.chroma)
        && request
            .video
            .codec
            .offered_bit_depths()
            .contains(request.video.bit_depth)
        && codec_chroma_ceiling(backend, request.video.codec).contains(request.video.chroma);
    if !format_supported
        || request.width > limits.max_width
        || request.height > limits.max_height
        || request.fps > limits.max_fps
        || (matches!(request.cursor_mode, CursorMode::Host) && !limits.cursor_in_video)
    {
        return Err(MediaPlanError::UnsupportedFormat);
    }
    Ok(ResolvedMediaPlan {
        backend,
        video: request.video,
        width: request.width,
        height: request.height,
        fps: request.fps,
        cursor_mode: request.cursor_mode,
        cursor_in_video: limits.cursor_in_video,
        codecs: limits.codecs,
        chroma: limits.chroma,
        bit_depths: limits.bit_depths,
        ranges: limits.ranges,
    })
}

/// What a degrading resolution had to change to fit the chosen backend.
///
/// Empty means the backend served the request exactly. Anything non-empty is
/// operator-visible truth: a session that looks worse than asked for must be
/// able to say why, so this is reported rather than applied silently.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PlanDegradation {
    pub codec_changed: bool,
    pub chroma_changed: bool,
    pub bit_depth_reduced: bool,
    pub range_changed: bool,
    pub matrix_changed: bool,
    pub primaries_changed: bool,
    pub transfer_changed: bool,
    pub fps_clamped: bool,
    pub geometry_clamped: bool,
    pub cursor_moved_to_local: bool,
}

impl PlanDegradation {
    /// True when the backend served the request exactly as asked.
    #[must_use]
    pub const fn is_exact(self) -> bool {
        !self.codec_changed
            && !self.chroma_changed
            && !self.bit_depth_reduced
            && !self.range_changed
            && !self.matrix_changed
            && !self.primaries_changed
            && !self.transfer_changed
            && !self.fps_clamped
            && !self.geometry_clamped
            && !self.cursor_moved_to_local
    }

    /// True when anything that affects colour fidelity was changed.
    ///
    /// Separate from [`PlanDegradation::is_exact`] because a colourist cares
    /// about a chroma or depth downgrade in a way they do not care about an
    /// fps clamp, and the Deck surfaces the two differently.
    #[must_use]
    pub const fn colour_degraded(self) -> bool {
        self.codec_changed
            || self.chroma_changed
            || self.bit_depth_reduced
            || self.range_changed
            || self.matrix_changed
            || self.primaries_changed
            || self.transfer_changed
    }
}

/// Codecs to prefer when the requested one cannot be served, richest first.
///
/// A new codec joins this list at the position it should be preferred; nothing
/// else in the resolver changes.
const CODEC_PREFERENCE: &[VideoCodec] = &[VideoCodec::H265, VideoCodec::H264];

/// Richest codec this backend can encode, preferring the requested one.
fn best_codec(requested: VideoCodec, limits: BackendLimits) -> Option<VideoCodec> {
    // Honour the request when the backend can serve it, rather than silently
    // upgrading to something richer.
    if limits.supports(requested) {
        return Some(requested);
    }
    CODEC_PREFERENCE
        .iter()
        .copied()
        .find(|codec| limits.supports(*codec))
}

/// Scale `width`x`height` down to fit the backend maxima, preserving aspect
/// ratio and returning even dimensions.
///
/// Dimensions are clamped rather than the image being scaled at encode time:
/// the resolved plan drives the single display mutation, so the display is set
/// to a geometry the backend can actually encode.
fn fit_geometry(width: u32, height: u32, limits: BackendLimits) -> Option<(u32, u32)> {
    if limits.max_width == 0 || limits.max_height == 0 {
        return None;
    }
    let (mut fitted_width, mut fitted_height) = (width, height);
    if fitted_width > limits.max_width || fitted_height > limits.max_height {
        // Integer math only: pick the tighter of the two ratios, in 1/1024ths,
        // so a 64-bit intermediate cannot overflow for any realistic display.
        let width_scale = u64::from(limits.max_width) * 1024 / u64::from(width.max(1));
        let height_scale = u64::from(limits.max_height) * 1024 / u64::from(height.max(1));
        let scale = width_scale.min(height_scale);
        fitted_width = u32::try_from(u64::from(width) * scale / 1024).ok()?;
        fitted_height = u32::try_from(u64::from(height) * scale / 1024).ok()?;
    }
    // 4:2:0 requires even dimensions; rounding down keeps us inside the maxima.
    fitted_width &= !1;
    fitted_height &= !1;
    if fitted_width == 0 || fitted_height == 0 {
        return None;
    }
    Some((fitted_width, fitted_height))
}

fn degrade_to_limits(
    request: MediaRequest,
    backend: EncoderBackend,
    limits: BackendLimits,
) -> Result<(ResolvedMediaPlan, PlanDegradation), MediaPlanError> {
    let mut degradation = PlanDegradation::default();

    let codec = best_codec(request.video.codec, limits).ok_or(MediaPlanError::UnsupportedFormat)?;
    degradation.codec_changed = codec != request.video.codec;

    // Keep the requested chroma only if the backend can encode it *and* Arcen
    // offers it for the resolved codec *and* this codec-on-this-backend pair
    // doesn't narrow it further (see `codec_chroma_ceiling`: NVENC's AV1 is
    // 4:2:0-only even though NVENC's HEVC reaches 4:4:4). All three are table
    // lookups, so a codec that later gains 4:4:4 needs no change here.
    let chroma = if limits.supports_chroma(request.video.chroma)
        && codec.offered_chroma().contains(request.video.chroma)
        && codec_chroma_ceiling(backend, codec).contains(request.video.chroma)
    {
        request.video.chroma
    } else {
        ChromaSubsampling::Yuv420
    };
    degradation.chroma_changed = chroma != request.video.chroma;

    // Depth degrades to the deepest the backend can serve that is no deeper
    // than requested, rather than collapsing straight to eight bits: a client
    // asking for 12-bit on an NVENC host should land on 10, not on 8.
    let offered_depths = limits.bit_depths.intersection(codec.offered_bit_depths());
    let bit_depth = offered_depths
        .deepest_up_to(request.video.bit_depth)
        .or_else(|| offered_depths.deepest())
        .ok_or(MediaPlanError::UnsupportedFormat)?;
    degradation.bit_depth_reduced = bit_depth < request.video.bit_depth;

    let range = if limits.supports_range(request.video.range) {
        request.video.range
    } else {
        // Limited range is the universally supported floor.
        ColorRange::Limited
    };
    degradation.range_changed = range != request.video.range;

    let matrix = if limits.supports_matrix(request.video.matrix) {
        request.video.matrix
    } else {
        ColorMatrix::Bt709
    };
    degradation.matrix_changed = matrix != request.video.matrix;

    let fps = request.fps.min(limits.max_fps);
    degradation.fps_clamped = fps != request.fps;
    if fps == 0 {
        return Err(MediaPlanError::InvalidGeometry);
    }

    let (width, height) = fit_geometry(request.width, request.height, limits)
        .ok_or(MediaPlanError::InvalidGeometry)?;
    degradation.geometry_clamped = width != request.width || height != request.height;

    let cursor_mode = if matches!(request.cursor_mode, CursorMode::Host) && !limits.cursor_in_video
    {
        degradation.cursor_moved_to_local = true;
        CursorMode::Local
    } else {
        request.cursor_mode
    };

    let mut video = request.video;
    video.codec = codec;
    video.chroma = chroma;
    video.bit_depth = bit_depth;
    video.range = range;
    video.matrix = matrix;

    Ok((
        ResolvedMediaPlan {
            backend,
            video,
            width,
            height,
            fps,
            cursor_mode,
            cursor_in_video: limits.cursor_in_video,
            codecs: limits.codecs,
            chroma: limits.chroma,
            bit_depths: limits.bit_depths,
            ranges: limits.ranges,
        },
        degradation,
    ))
}

/// Resolve a plan, degrading the request to what the backend can actually serve.
///
/// [`resolve_media_plan`] fails closed when a present backend cannot meet the
/// request exactly. That is the right contract when the caller needs the exact
/// format it asked for, and the wrong one for `auto` fallback: refusing to
/// serve H.264 because the operator configured H.265 turns a degraded session
/// into no session at all.
///
/// This entry point instead picks the best plan the backend can encode and
/// reports what it had to change, so the host can advertise the resolved truth
/// in `server_hello` and log the difference. Geometry is scaled with the aspect
/// ratio preserved rather than each axis being clipped independently.
///
/// Backend *availability* still fails closed exactly as before: this degrades
/// the format, never the requirement that a backend be present and usable.
///
/// # Errors
///
/// Returns typed invalid, unavailable, missing, or unsupported outcomes. A
/// backend that can encode neither H.264 nor H.265, or cannot represent the
/// geometry at all, is still [`MediaPlanError::UnsupportedFormat`] or
/// [`MediaPlanError::InvalidGeometry`].
pub fn resolve_media_plan_degrading(
    request: MediaRequest,
    candidates: &[BackendCandidate],
) -> Result<(ResolvedMediaPlan, PlanDegradation), MediaPlanError> {
    validate_request(request)?;
    let mut matched_explicit = false;
    for candidate in candidates {
        if !request.encoder.accepts(candidate.backend) {
            continue;
        }
        matched_explicit = true;
        match candidate.availability {
            BackendAvailability::Available(limits) => {
                return degrade_to_limits(request, candidate.backend, limits);
            }
            BackendAvailability::Unavailable(reason) => {
                if !matches!(request.encoder, EncoderRequest::Auto) {
                    return Err(MediaPlanError::BackendUnavailable(reason));
                }
            }
        }
    }
    if !matches!(request.encoder, EncoderRequest::Auto) && !matched_explicit {
        Err(MediaPlanError::RequestedBackendMissing)
    } else {
        Err(MediaPlanError::NoBackendAvailable)
    }
}

/// Resolve an ordered, already-probed platform candidate list without I/O.
///
/// Auto advances only over typed unavailable candidates. A present backend with
/// incompatible limits fails closed rather than silently changing the request.
///
/// # Errors
///
/// Returns typed invalid, unavailable, missing, or unsupported outcomes.
pub fn resolve_media_plan(
    request: MediaRequest,
    candidates: &[BackendCandidate],
) -> Result<ResolvedMediaPlan, MediaPlanError> {
    validate_request(request)?;
    let mut matched_explicit = false;
    for candidate in candidates {
        if !request.encoder.accepts(candidate.backend) {
            continue;
        }
        matched_explicit = true;
        match candidate.availability {
            BackendAvailability::Available(limits) => {
                return resolve_available(request, candidate.backend, limits);
            }
            BackendAvailability::Unavailable(reason) => {
                if !matches!(request.encoder, EncoderRequest::Auto) {
                    return Err(MediaPlanError::BackendUnavailable(reason));
                }
            }
        }
    }
    if !matches!(request.encoder, EncoderRequest::Auto) && !matched_explicit {
        Err(MediaPlanError::RequestedBackendMissing)
    } else {
        Err(MediaPlanError::NoBackendAvailable)
    }
}

/// Expected request facts for strict READY-v1 validation.
#[derive(Debug, Clone, Copy)]
pub struct ReadyExpectation<'a> {
    pub request: MediaRequest,
    pub allowed_backends: &'a [EncoderBackend],
    pub session_log_id: Option<&'a str>,
}

/// Strict READY-v1 parse or request-matching failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadyProtocolError {
    MissingPrefix,
    MalformedToken(String),
    DuplicateField(String),
    MissingField(&'static str),
    UnsupportedVersion,
    UnsupportedBackend,
    UnsupportedCodec,
    UnsupportedChroma,
    UnsupportedBitDepth,
    UnsupportedRange,
    UnsupportedMatrix,
    UnsupportedPrimaries,
    UnsupportedTransfer,
    InvalidNumber(&'static str),
    InvalidBoolean(&'static str),
    UnsupportedCursor,
    RequestConflict,
    InvalidSessionId,
    UnknownFields(String),
    CapabilityConflict,
    BackendNotAllowed,
}

impl Display for ReadyProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for ReadyProtocolError {}

/// Strict UNAVAILABLE-v1 parse failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnavailableProtocolError {
    MissingPrefix,
    MalformedToken(String),
    DuplicateField(String),
    MissingField(&'static str),
    UnsupportedVersion,
    UnsupportedBackend,
    UnsupportedReason,
    UnknownFields(String),
}

impl Display for UnavailableProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for UnavailableProtocolError {}

fn take<'a>(
    fields: &mut BTreeMap<&'a str, &'a str>,
    name: &'static str,
) -> Result<&'a str, ReadyProtocolError> {
    fields
        .remove(name)
        .ok_or(ReadyProtocolError::MissingField(name))
}

fn parse_bool(value: &str, name: &'static str) -> Result<bool, ReadyProtocolError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ReadyProtocolError::InvalidBoolean(name)),
    }
}

/// Parse and strictly match a capenc READY-v1 line.
///
/// # Errors
///
/// Rejects malformed, duplicate, missing, unknown, contradictory, or
/// request-conflicting fields.
#[allow(clippy::too_many_lines)]
pub fn parse_ready_v1(
    line: &str,
    expectation: ReadyExpectation<'_>,
) -> Result<ResolvedMediaPlan, ReadyProtocolError> {
    let payload = line
        .strip_prefix(READY_PREFIX)
        .ok_or(ReadyProtocolError::MissingPrefix)?;
    let mut fields = BTreeMap::new();
    for token in payload.split_ascii_whitespace() {
        let (name, value) = token
            .split_once('=')
            .ok_or_else(|| ReadyProtocolError::MalformedToken(token.to_owned()))?;
        if name.is_empty() || value.is_empty() {
            return Err(ReadyProtocolError::MalformedToken(token.to_owned()));
        }
        if fields.insert(name, value).is_some() {
            return Err(ReadyProtocolError::DuplicateField(name.to_owned()));
        }
    }
    if take(&mut fields, "version")? != READY_VERSION {
        return Err(ReadyProtocolError::UnsupportedVersion);
    }
    let backend = match take(&mut fields, "backend")? {
        "native-nvenc" => EncoderBackend::NativeNvenc,
        "media-foundation-sw-h264" => EncoderBackend::WindowsMediaFoundation,
        "openh264-sw-h264" => EncoderBackend::OpenH264,
        "rav1e-sw-av1" => EncoderBackend::Rav1e,
        _ => return Err(ReadyProtocolError::UnsupportedBackend),
    };
    if !expectation.allowed_backends.contains(&backend) {
        return Err(ReadyProtocolError::BackendNotAllowed);
    }
    if !expectation.request.encoder.accepts(backend) {
        return Err(ReadyProtocolError::RequestConflict);
    }
    let codec = VideoCodec::from_token(take(&mut fields, "codec")?)
        .ok_or(ReadyProtocolError::UnsupportedCodec)?;
    let chroma = ChromaSubsampling::from_token(take(&mut fields, "chroma")?)
        .ok_or(ReadyProtocolError::UnsupportedChroma)?;
    let bit_depth = BitDepth::from_token(take(&mut fields, "bit_depth")?)
        .ok_or(ReadyProtocolError::UnsupportedBitDepth)?;
    let range = ColorRange::from_token(take(&mut fields, "range")?)
        .ok_or(ReadyProtocolError::UnsupportedRange)?;
    let matrix = ColorMatrix::from_token(take(&mut fields, "matrix")?)
        .ok_or(ReadyProtocolError::UnsupportedMatrix)?;
    let primaries = ColorPrimaries::from_token(take(&mut fields, "primaries")?)
        .ok_or(ReadyProtocolError::UnsupportedPrimaries)?;
    let transfer = TransferCharacteristics::from_token(take(&mut fields, "transfer")?)
        .ok_or(ReadyProtocolError::UnsupportedTransfer)?;
    let width = take(&mut fields, "width")?
        .parse()
        .map_err(|_| ReadyProtocolError::InvalidNumber("width"))?;
    let height = take(&mut fields, "height")?
        .parse()
        .map_err(|_| ReadyProtocolError::InvalidNumber("height"))?;
    let fps = take(&mut fields, "fps")?
        .parse()
        .map_err(|_| ReadyProtocolError::InvalidNumber("fps"))?;
    let supports_h264 = parse_bool(take(&mut fields, "supports_h264")?, "supports_h264")?;
    let supports_h265 = parse_bool(take(&mut fields, "supports_h265")?, "supports_h265")?;
    let supports_yuv444 = parse_bool(take(&mut fields, "supports_yuv444")?, "supports_yuv444")?;
    let supports_main10 = parse_bool(take(&mut fields, "supports_main10")?, "supports_main10")?;
    let supports_full_range = parse_bool(
        take(&mut fields, "supports_full_range")?,
        "supports_full_range",
    )?;
    if (codec == VideoCodec::H264 && !supports_h264)
        || (codec == VideoCodec::H265 && !supports_h265)
        || (chroma == ChromaSubsampling::Yuv444 && !supports_yuv444)
        || (bit_depth == BitDepth::Ten && !supports_main10)
        || (range == ColorRange::Full && !supports_full_range)
    {
        return Err(ReadyProtocolError::CapabilityConflict);
    }
    let cursor_mode = match take(&mut fields, "cursor")? {
        "local" => CursorMode::Local,
        "host" => CursorMode::Host,
        _ => return Err(ReadyProtocolError::UnsupportedCursor),
    };
    if let Some(actual) = fields.remove("sid") {
        CorrelationId::parse_uuid(actual).map_err(|_| ReadyProtocolError::InvalidSessionId)?;
        if expectation
            .session_log_id
            .is_some_and(|expected| expected != actual)
        {
            return Err(ReadyProtocolError::RequestConflict);
        }
    } else if expectation.session_log_id.is_some() {
        return Err(ReadyProtocolError::MissingField("sid"));
    }
    // Consumed so a capenc that names its capture path does not trip the
    // unknown-field check below. The value is deliberately not folded into
    // `ResolvedMediaPlan`: that struct is the video *contract*, and how the
    // pixels were captured is not part of it. Callers that want the path read
    // it with `parse_ready_capture`.
    let _ = fields.remove("capture");
    let _ = fields.remove("capture_zero_copy");
    if !fields.is_empty() {
        return Err(ReadyProtocolError::UnknownFields(
            fields.keys().copied().collect::<Vec<_>>().join(","),
        ));
    }
    let video = VideoConfiguration {
        codec,
        chroma,
        bit_depth,
        range,
        matrix,
        primaries,
        transfer,
    };
    let plan = ResolvedMediaPlan {
        backend,
        video,
        width,
        height,
        fps,
        cursor_mode,
        cursor_in_video: matches!(cursor_mode, CursorMode::Host),
        // The READY line carries named booleans on the wire; they are folded
        // back into capability sets here so nothing downstream has to know the
        // wire shape.
        codecs: {
            let mut set = CodecSet::empty();
            if supports_h264 {
                set = set.with(VideoCodec::H264);
            }
            if supports_h265 {
                set = set.with(VideoCodec::H265);
            }
            // A backend that reports neither named codec but resolved to one
            // anyway (rav1e reporting AV1) must still claim what it resolved,
            // or the capability set would contradict the plan.
            set.with(codec)
        },
        chroma: {
            let mut set = ChromaSet::from_slice(&[ChromaSubsampling::Yuv420]);
            if supports_yuv444 {
                set = set.with(ChromaSubsampling::Yuv444);
            }
            set.with(chroma)
        },
        bit_depths: {
            let mut set = BitDepthSet::from_slice(&[BitDepth::Eight]);
            if supports_main10 {
                set = set.with(BitDepth::Ten);
            }
            set.with(bit_depth)
        },
        ranges: {
            let mut set = ColorRangeSet::from_slice(&[ColorRange::Limited]);
            if supports_full_range {
                set = set.with(ColorRange::Full);
            }
            set.with(range)
        },
    };
    if width == 0
        || height == 0
        || fps == 0
        || ((expectation.request.width == 0) != (expectation.request.height == 0))
        || (expectation.request.width != 0 && width != expectation.request.width)
        || (expectation.request.height != 0 && height != expectation.request.height)
        || fps != expectation.request.fps
        || video != expectation.request.video
        || cursor_mode != expectation.request.cursor_mode
    {
        return Err(ReadyProtocolError::RequestConflict);
    }
    // Every rule below is a table lookup against the backend's declared
    // contract and the codec's offered formats. Adding a codec or a backend
    // therefore needs no edit here; it needs a row in
    // `EncoderBackend::contract`, `VideoCodec::offered_chroma` or
    // `VideoCodec::offered_bit_depths`. `codec_chroma_ceiling` is the one
    // exception, for the one backend/codec pair whose real chroma ceiling is
    // narrower than the backend's shared contract (see its doc) -- without
    // it a child could claim `backend=native-nvenc codec=av1 chroma=444` and
    // have this accept it, even though NVENC only exposes AV1 Main (4:2:0).
    let contract = backend.contract();
    if !contract.supports(codec)
        || !contract.supports_chroma(chroma)
        || !contract.supports_bit_depth(bit_depth)
        || !contract.supports_range(range)
        || !contract.supports_matrix(matrix)
        || !codec.offered_chroma().contains(chroma)
        || !codec.offered_bit_depths().contains(bit_depth)
        || !codec_chroma_ceiling(backend, codec).contains(chroma)
        // A child may not claim capability its backend does not have.
        || !contract.codecs.contains_all(plan.codecs)
        || !plan.chroma.iter().all(|c| contract.supports_chroma(c))
        || !plan.bit_depths.iter().all(|d| contract.supports_bit_depth(d))
        || !plan.ranges.iter().all(|r| contract.supports_range(r))
        // The claimed set must also cover what it actually produced.
        || !plan.codecs.contains(codec)
        || !plan.chroma.contains(chroma)
        || !plan.bit_depths.contains(bit_depth)
        || !plan.ranges.contains(range)
        || width > contract.max_width
        || height > contract.max_height
        || fps > contract.max_fps
        || (chroma == ChromaSubsampling::Yuv420 && (width % 2 != 0 || height % 2 != 0))
    {
        return Err(ReadyProtocolError::CapabilityConflict);
    }
    Ok(plan)
}

/// Format the canonical READY line for one resolved plan.
///
/// Emits no `capture=` field, so a line built this way stays byte-identical to
/// what older builds produced. Use [`format_ready_v1_with_capture`] to name
/// the capture path.
#[must_use]
pub fn format_ready_v1(plan: ResolvedMediaPlan, session_log_id: Option<&str>) -> String {
    format_ready_v1_with_capture(plan, None, session_log_id)
}

/// Format the canonical READY line, optionally naming the capture path.
///
/// `capture` is appended rather than inserted, and omitted entirely when
/// `None`, because [`parse_ready_v1`] rejects unknown fields: a line is only
/// safe to extend in a build whose parser already tolerates the addition.
#[must_use]
pub fn format_ready_v1_with_capture(
    plan: ResolvedMediaPlan,
    capture: Option<CaptureBackend>,
    session_log_id: Option<&str>,
) -> String {
    let sid = session_log_id.map_or_else(String::new, |value| format!(" sid={value}"));
    let capture = capture.map_or_else(String::new, |backend| {
        format!(
            " capture={} capture_zero_copy={}",
            backend.ready_token(),
            backend.zero_copy()
        )
    });
    format!(
        "{READY_PREFIX}version={READY_VERSION} backend={} codec={} chroma={} bit_depth={} \
range={} matrix={} primaries={} transfer={} width={} height={} fps={} \
supports_h264={} supports_h265={} supports_yuv444={} supports_main10={} \
supports_full_range={} cursor={}{}{}",
        plan.backend.ready_token(),
        plan.codec_token(),
        plan.chroma_token(),
        plan.bit_depth_token(),
        plan.range_token(),
        plan.matrix_token(),
        plan.primaries_token(),
        plan.transfer_token(),
        plan.width,
        plan.height,
        plan.fps,
        plan.supports_h264(),
        plan.supports_h265(),
        plan.supports_yuv444(),
        plan.supports_main10(),
        plan.supports_full_range(),
        match plan.cursor_mode {
            CursorMode::Local => "local",
            CursorMode::Host => "host",
        },
        capture,
        sid
    )
}

/// Read the capture path named by a READY line, when it names one.
///
/// Separate from [`parse_ready_v1`] so that adding it changed no signature and
/// no caller. `None` means the line came from a capenc that predates the field
/// or named a path this build does not know — both read as "unreported", never
/// as a guess.
///
/// Does no validation beyond the token: the line's structure is
/// [`parse_ready_v1`]'s job, and a caller that has not accepted the line
/// should not be acting on its capture field either.
#[must_use]
pub fn parse_ready_capture(line: &str) -> Option<CaptureBackend> {
    line.strip_prefix(READY_PREFIX)?
        .split_ascii_whitespace()
        .find_map(|token| token.strip_prefix("capture="))
        .and_then(CaptureBackend::from_token)
}

/// Format a canonical typed pre-READY unavailability notice.
#[must_use]
pub fn format_unavailable_v1(notice: BackendUnavailableNotice) -> String {
    let code = match notice.reason {
        BackendUnavailableReason::NotBuilt => "not_built",
        BackendUnavailableReason::RuntimeMissing => "runtime_missing",
        BackendUnavailableReason::HardwareUnavailable => "hardware_unavailable",
        BackendUnavailableReason::SessionLimit => "session_limit",
        BackendUnavailableReason::UnsupportedDisplay => "unsupported_display",
        BackendUnavailableReason::UnsupportedConfiguration => "unsupported_configuration",
    };
    format!(
        "{UNAVAILABLE_PREFIX}version=1 backend={} code={code}",
        notice.backend.ready_token()
    )
}

fn take_unavailable_field<'a>(
    fields: &mut BTreeMap<&'a str, &'a str>,
    name: &'static str,
) -> Result<&'a str, UnavailableProtocolError> {
    fields
        .remove(name)
        .ok_or(UnavailableProtocolError::MissingField(name))
}

/// Parse an exact typed pre-READY unavailability notice.
///
/// # Errors
///
/// Rejects malformed, duplicate, missing, unknown, or unsupported fields.
pub fn parse_unavailable_v1(
    line: &str,
) -> Result<BackendUnavailableNotice, UnavailableProtocolError> {
    let payload = line
        .strip_prefix(UNAVAILABLE_PREFIX)
        .ok_or(UnavailableProtocolError::MissingPrefix)?;
    let mut fields = BTreeMap::new();
    for token in payload.split_ascii_whitespace() {
        let (name, value) = token
            .split_once('=')
            .ok_or_else(|| UnavailableProtocolError::MalformedToken(token.to_owned()))?;
        if name.is_empty() || value.is_empty() {
            return Err(UnavailableProtocolError::MalformedToken(token.to_owned()));
        }
        if fields.insert(name, value).is_some() {
            return Err(UnavailableProtocolError::DuplicateField(name.to_owned()));
        }
    }
    if take_unavailable_field(&mut fields, "version")? != "1" {
        return Err(UnavailableProtocolError::UnsupportedVersion);
    }
    let backend = match take_unavailable_field(&mut fields, "backend")? {
        "native-nvenc" => EncoderBackend::NativeNvenc,
        "media-foundation-sw-h264" => EncoderBackend::WindowsMediaFoundation,
        "openh264-sw-h264" => EncoderBackend::OpenH264,
        _ => return Err(UnavailableProtocolError::UnsupportedBackend),
    };
    let reason = match take_unavailable_field(&mut fields, "code")? {
        "not_built" => BackendUnavailableReason::NotBuilt,
        "runtime_missing" => BackendUnavailableReason::RuntimeMissing,
        "hardware_unavailable" => BackendUnavailableReason::HardwareUnavailable,
        "session_limit" => BackendUnavailableReason::SessionLimit,
        "unsupported_display" => BackendUnavailableReason::UnsupportedDisplay,
        "unsupported_configuration" => BackendUnavailableReason::UnsupportedConfiguration,
        _ => return Err(UnavailableProtocolError::UnsupportedReason),
    };
    if !fields.is_empty() {
        return Err(UnavailableProtocolError::UnknownFields(
            fields.keys().copied().collect::<Vec<_>>().join(","),
        ));
    }
    Ok(BackendUnavailableNotice { backend, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The NVENC contract must not advertise 4:2:2 while the vendored bindings
    /// are NVENCAPI 12.1, which has no 4:2:2 surface format to name. Claiming
    /// it would resolve a plan and emit a READY line promising chroma the
    /// encoder then rejects at init.
    ///
    /// If you have just updated the bindings to Video Codec SDK 13.0, this
    /// test failing is the intended nudge: add `NV_ENC_BUFFER_FORMAT_NV16` and
    /// `P210` handling plus the `SUPPORT_YUV422_ENCODE` cap query in
    /// `hosts/capenc/src/nvenc.rs`, then restore `Yuv422` to the contract and
    /// invert this assertion.
    #[test]
    fn nvenc_contract_withholds_422_until_sdk13_bindings_land() {
        let chroma = EncoderBackend::NativeNvenc.contract().chroma;
        assert!(
            !chroma.contains(ChromaSubsampling::Yuv422),
            "NVENC contract advertises 4:2:2, but the vendored bindings are \
             NVENCAPI 12.1 and cannot express a 4:2:2 surface. See \
             docs/architecture/nvenc-sdk13-blackwell.md",
        );
        assert!(
            chroma.contains(ChromaSubsampling::Yuv444),
            "4:4:4 is hardware-proven and must stay in the contract",
        );
    }

    const H264_420: VideoConfiguration = VideoConfiguration::legacy_h264();
    const REQUEST: MediaRequest = MediaRequest {
        encoder: EncoderRequest::Auto,
        video: H264_420,
        width: 1920,
        height: 1080,
        fps: 30,
        cursor_mode: CursorMode::Local,
    };
    const SOFTWARE_LIMITS: BackendLimits = BackendLimits {
        codecs: CodecSet::from_slice(&[VideoCodec::H264]),
        chroma: ChromaSet::from_slice(&[ChromaSubsampling::Yuv420]),
        bit_depths: BitDepthSet::from_slice(&[BitDepth::Eight]),
        ranges: ColorRangeSet::from_slice(&[ColorRange::Limited, ColorRange::Full]),
        identity_matrix: false,
        max_width: 1920,
        max_height: 1080,
        max_fps: 30,
        cursor_in_video: false,
    };

    #[test]
    fn accelerator_class_is_declared_not_inferred_from_the_backend_name() {
        // The substring heuristic this replaces tested for "native". It agrees
        // with the declared class for today's backends, which is exactly why
        // the bug was invisible.
        for backend in [
            EncoderBackend::NativeNvenc,
            EncoderBackend::WindowsMediaFoundation,
            EncoderBackend::OpenH264,
        ] {
            let guessed = if backend.ready_token().contains("native") {
                AcceleratorClass::Hardware
            } else {
                AcceleratorClass::Software
            };
            assert_eq!(
                backend.accelerator_class(),
                guessed,
                "{} regressed against the legacy heuristic",
                backend.ready_token()
            );
        }
        assert_eq!(
            EncoderBackend::NativeNvenc.accelerator_class(),
            AcceleratorClass::Hardware
        );
        assert_eq!(
            EncoderBackend::OpenH264.accelerator_class(),
            AcceleratorClass::Software
        );
    }

    #[test]
    fn accelerator_class_tokens_round_trip_and_unknown_values_are_rejected() {
        for class in [AcceleratorClass::Hardware, AcceleratorClass::Software] {
            assert_eq!(AcceleratorClass::from_token(class.token()), Some(class));
        }
        // A newer host's class must not be silently read as software.
        assert_eq!(AcceleratorClass::from_token("quantum"), None);
        assert_eq!(AcceleratorClass::from_token(""), None);
        assert_eq!(AcceleratorClass::from_token("Hardware"), None);
    }

    // ---- degrading resolution -------------------------------------------
    //
    // The case that motivated this: a Linux Pier configured for H.265 4:4:4 at
    // 60 fps on a 2560x1600 display, whose NVENC is unavailable. The strict
    // resolver refuses and the session dies; the degrading resolver must serve
    // H.264 4:2:0 within the OpenH264 contract and say what it changed.

    const RICH_REQUEST: MediaRequest = MediaRequest {
        encoder: EncoderRequest::Auto,
        video: VideoConfiguration {
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            ..VideoConfiguration::legacy_h264()
        },
        width: 2560,
        height: 1600,
        fps: 60,
        cursor_mode: CursorMode::Local,
    };

    fn software_only() -> [BackendCandidate; 2] {
        [
            BackendCandidate {
                backend: EncoderBackend::NativeNvenc,
                availability: BackendAvailability::Unavailable(
                    BackendUnavailableReason::HardwareUnavailable,
                ),
            },
            BackendCandidate {
                backend: EncoderBackend::OpenH264,
                availability: BackendAvailability::Available(SOFTWARE_LIMITS),
            },
        ]
    }

    #[test]
    fn strict_resolution_refuses_the_rich_request_that_degrading_resolution_serves() {
        // Documents the exact defect: identical inputs, one fails, one works.
        assert_eq!(
            resolve_media_plan(RICH_REQUEST, &software_only()),
            Err(MediaPlanError::UnsupportedFormat)
        );
        assert!(resolve_media_plan_degrading(RICH_REQUEST, &software_only()).is_ok());
    }

    #[test]
    fn degrading_resolution_clamps_every_axis_and_reports_each_one() {
        let (plan, degradation) =
            resolve_media_plan_degrading(RICH_REQUEST, &software_only()).expect("degraded plan");
        assert_eq!(plan.backend, EncoderBackend::OpenH264);
        assert_eq!(plan.video.codec, VideoCodec::H264);
        assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv420);
        assert_eq!(plan.fps, 30);
        // 2560x1600 is 16:10; fitted into 1920x1080 the height binds first, so
        // the result is 1726x1078 rather than a distorted 1920x1080. Asserted
        // as invariants as well as an exact value, because the exact value is
        // a consequence of integer rounding and the invariants are the contract.
        assert_eq!((plan.width, plan.height), (1726, 1078));
        assert!(
            plan.width <= 1920 && plan.height <= 1080,
            "inside backend maxima"
        );
        assert_eq!((plan.width & 1, plan.height & 1), (0, 0), "even for 4:2:0");
        assert!(degradation.codec_changed);
        assert!(degradation.chroma_changed);
        assert!(degradation.fps_clamped);
        assert!(degradation.geometry_clamped);
        assert!(!degradation.is_exact());
    }

    #[test]
    fn degrading_resolution_is_a_no_op_when_the_backend_can_serve_the_request() {
        let candidates = [BackendCandidate {
            backend: EncoderBackend::OpenH264,
            availability: BackendAvailability::Available(SOFTWARE_LIMITS),
        }];
        let (plan, degradation) =
            resolve_media_plan_degrading(REQUEST, &candidates).expect("exact plan");
        let strict = resolve_media_plan(REQUEST, &candidates).expect("exact plan");
        assert!(degradation.is_exact());
        // A request the backend can serve must resolve identically either way,
        // so adopting the degrading resolver cannot change a working session.
        assert_eq!(plan, strict);
    }

    #[test]
    fn degrading_resolution_keeps_h264_444_when_the_backend_offers_it() {
        // H.264 High 4:4:4 Predictive is now an offered contract, so a
        // 4:4:4-capable H.264 backend must keep 4:4:4 rather than silently
        // dropping to 4:2:0. Whether a given *client* can decode it is a
        // separate, probed question.
        let limits = BackendLimits {
            codecs: CodecSet::from_slice(&[VideoCodec::H264]),
            chroma: ChromaSet::from_slice(&[ChromaSubsampling::Yuv420, ChromaSubsampling::Yuv444]),
            ..SOFTWARE_LIMITS
        };
        let candidates = [BackendCandidate {
            backend: EncoderBackend::OpenH264,
            availability: BackendAvailability::Available(limits),
        }];
        let (plan, degradation) =
            resolve_media_plan_degrading(RICH_REQUEST, &candidates).expect("degraded plan");
        assert_eq!(plan.video.codec, VideoCodec::H264);
        assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv444);
        // The codec still changed, so the session is still reported as degraded.
        assert!(degradation.codec_changed);
        assert!(!degradation.chroma_changed);
    }

    #[test]
    fn degrading_resolution_drops_chroma_when_the_backend_cannot_serve_it() {
        // The real 4:2:0-only floor: OpenH264 Baseline.
        let candidates = [BackendCandidate {
            backend: EncoderBackend::OpenH264,
            availability: BackendAvailability::Available(SOFTWARE_LIMITS),
        }];
        let (plan, degradation) =
            resolve_media_plan_degrading(RICH_REQUEST, &candidates).expect("degraded plan");
        assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv420);
        assert!(degradation.chroma_changed);
        assert!(degradation.colour_degraded());
    }

    #[test]
    fn degrading_resolution_preserves_aspect_ratio_and_returns_even_geometry() {
        // 1366x768 into a 640x480 backend: width binds, and the odd result is
        // rounded down to even so 4:2:0 stays representable.
        let limits = BackendLimits {
            max_width: 640,
            max_height: 480,
            ..SOFTWARE_LIMITS
        };
        let request = MediaRequest {
            width: 1366,
            height: 768,
            ..REQUEST
        };
        let candidates = [BackendCandidate {
            backend: EncoderBackend::OpenH264,
            availability: BackendAvailability::Available(limits),
        }];
        let (plan, degradation) =
            resolve_media_plan_degrading(request, &candidates).expect("degraded plan");
        assert!(plan.width <= 640 && plan.height <= 480);
        assert_eq!(plan.width & 1, 0, "width must be even for 4:2:0");
        assert_eq!(plan.height & 1, 0, "height must be even for 4:2:0");
        assert!(degradation.geometry_clamped);
        // Aspect ratio preserved to within a pixel of rounding.
        let source = f64::from(1366) / f64::from(768);
        let fitted = f64::from(plan.width) / f64::from(plan.height);
        assert!((source - fitted).abs() < 0.02, "aspect drifted: {fitted}");
    }

    #[test]
    fn degrading_resolution_moves_host_cursor_to_local_when_unsupported() {
        let request = MediaRequest {
            cursor_mode: CursorMode::Host,
            ..REQUEST
        };
        let candidates = [BackendCandidate {
            backend: EncoderBackend::OpenH264,
            availability: BackendAvailability::Available(SOFTWARE_LIMITS),
        }];
        let (plan, degradation) =
            resolve_media_plan_degrading(request, &candidates).expect("degraded plan");
        assert_eq!(plan.cursor_mode, CursorMode::Local);
        assert!(degradation.cursor_moved_to_local);
    }

    #[test]
    fn degrading_resolution_still_fails_closed_on_availability() {
        // Degradation applies to format, never to whether a backend exists.
        let none_available = [BackendCandidate {
            backend: EncoderBackend::NativeNvenc,
            availability: BackendAvailability::Unavailable(BackendUnavailableReason::SessionLimit),
        }];
        assert_eq!(
            resolve_media_plan_degrading(REQUEST, &none_available),
            Err(MediaPlanError::NoBackendAvailable)
        );
        // An explicit request for an unavailable backend keeps its typed reason
        // rather than silently degrading onto a different backend.
        let explicit = MediaRequest {
            encoder: EncoderRequest::NativeNvenc,
            ..REQUEST
        };
        assert_eq!(
            resolve_media_plan_degrading(explicit, &none_available),
            Err(MediaPlanError::BackendUnavailable(
                BackendUnavailableReason::SessionLimit
            ))
        );
    }

    #[test]
    fn degrading_resolution_rejects_a_backend_that_can_encode_nothing() {
        let limits = BackendLimits {
            codecs: CodecSet::empty(),
            ..SOFTWARE_LIMITS
        };
        let candidates = [BackendCandidate {
            backend: EncoderBackend::OpenH264,
            availability: BackendAvailability::Available(limits),
        }];
        assert_eq!(
            resolve_media_plan_degrading(REQUEST, &candidates),
            Err(MediaPlanError::UnsupportedFormat)
        );
    }

    #[test]
    fn platform_orders_are_deterministic_and_explicit_openh264_is_not_windows_auto() {
        let unavailable =
            BackendAvailability::Unavailable(BackendUnavailableReason::HardwareUnavailable);
        let windows = [
            BackendCandidate {
                backend: EncoderBackend::NativeNvenc,
                availability: unavailable,
            },
            BackendCandidate {
                backend: EncoderBackend::WindowsMediaFoundation,
                availability: BackendAvailability::Available(SOFTWARE_LIMITS),
            },
        ];
        assert_eq!(
            resolve_media_plan(REQUEST, &windows)
                .expect("Windows fallback")
                .backend,
            EncoderBackend::WindowsMediaFoundation
        );

        let linux = [
            BackendCandidate {
                backend: EncoderBackend::NativeNvenc,
                availability: unavailable,
            },
            BackendCandidate {
                backend: EncoderBackend::OpenH264,
                availability: BackendAvailability::Available(SOFTWARE_LIMITS),
            },
        ];
        assert_eq!(
            resolve_media_plan(REQUEST, &linux)
                .expect("Linux fallback")
                .backend,
            EncoderBackend::OpenH264
        );
        let explicit = MediaRequest {
            encoder: EncoderRequest::SoftwareH264,
            ..REQUEST
        };
        assert_eq!(
            resolve_media_plan(
                explicit,
                &[BackendCandidate {
                    backend: EncoderBackend::OpenH264,
                    availability: BackendAvailability::Available(SOFTWARE_LIMITS),
                }]
            )
            .expect("explicit OpenH264")
            .backend,
            EncoderBackend::OpenH264
        );
    }

    #[test]
    fn available_but_incompatible_candidate_fails_closed() {
        let too_large = MediaRequest {
            width: 1922,
            ..REQUEST
        };
        assert_eq!(
            resolve_media_plan(
                too_large,
                &[
                    BackendCandidate {
                        backend: EncoderBackend::OpenH264,
                        availability: BackendAvailability::Available(SOFTWARE_LIMITS),
                    },
                    BackendCandidate {
                        backend: EncoderBackend::WindowsMediaFoundation,
                        availability: BackendAvailability::Available(SOFTWARE_LIMITS),
                    },
                ]
            ),
            Err(MediaPlanError::UnsupportedFormat)
        );

        // A 10-bit request against a backend whose probe reports 8-bit only
        // is the modern "available but incompatible" case, and must fail
        // closed rather than quietly resolving to 8-bit.
        let ten_bit = MediaRequest {
            video: VideoConfiguration {
                bit_depth: BitDepth::Ten,
                ..VideoConfiguration::legacy_h264()
            },
            ..REQUEST
        };
        assert_eq!(
            resolve_media_plan(
                ten_bit,
                &[BackendCandidate {
                    backend: EncoderBackend::NativeNvenc,
                    availability: BackendAvailability::Available(SOFTWARE_LIMITS),
                }]
            ),
            Err(MediaPlanError::UnsupportedFormat)
        );

        let h264_yuv444 = MediaRequest {
            video: VideoConfiguration {
                codec: VideoCodec::H264,
                chroma: ChromaSubsampling::Yuv444,
                ..VideoConfiguration::legacy_h264()
            },
            ..REQUEST
        };
        // H.264 High 4:4:4 Predictive is offered now, so a backend that can
        // encode it resolves exactly rather than failing closed.
        assert_eq!(
            resolve_media_plan(
                h264_yuv444,
                &[BackendCandidate {
                    backend: EncoderBackend::NativeNvenc,
                    availability: BackendAvailability::Available(BackendLimits {
                        chroma: ChromaSet::from_slice(&[
                            ChromaSubsampling::Yuv420,
                            ChromaSubsampling::Yuv444,
                        ]),
                        ..SOFTWARE_LIMITS
                    }),
                }]
            )
            .expect("H.264 4:4:4 is an offered contract")
            .video
            .chroma,
            ChromaSubsampling::Yuv444
        );

        // ...but a backend that cannot encode 4:4:4 must still fail closed.
        assert_eq!(
            resolve_media_plan(
                h264_yuv444,
                &[BackendCandidate {
                    backend: EncoderBackend::OpenH264,
                    availability: BackendAvailability::Available(SOFTWARE_LIMITS),
                }]
            ),
            Err(MediaPlanError::UnsupportedFormat)
        );
    }

    const NVENC_LIMITS: BackendLimits = EncoderBackend::NativeNvenc.contract();

    const AV1_420_8: VideoConfiguration = VideoConfiguration {
        codec: VideoCodec::Av1,
        ..VideoConfiguration::legacy_h264()
    };

    const AV1_420_10: VideoConfiguration = VideoConfiguration {
        codec: VideoCodec::Av1,
        bit_depth: BitDepth::Ten,
        ..VideoConfiguration::legacy_h264()
    };

    const AV1_444_8: VideoConfiguration = VideoConfiguration {
        codec: VideoCodec::Av1,
        chroma: ChromaSubsampling::Yuv444,
        ..VideoConfiguration::legacy_h264()
    };

    fn nvenc_candidate() -> [BackendCandidate; 1] {
        [BackendCandidate {
            backend: EncoderBackend::NativeNvenc,
            availability: BackendAvailability::Available(NVENC_LIMITS),
        }]
    }

    fn nvenc_request(video: VideoConfiguration) -> MediaRequest {
        MediaRequest {
            encoder: EncoderRequest::Auto,
            video,
            width: 1920,
            height: 1080,
            fps: 60,
            cursor_mode: CursorMode::Local,
        }
    }

    #[test]
    fn nvenc_contract_now_includes_av1_additively() {
        // Additive, not a swap: AV1 joins the set, H.264/HEVC keep working.
        assert!(NVENC_LIMITS.supports(VideoCodec::Av1));
        assert!(NVENC_LIMITS.supports(VideoCodec::H264));
        assert!(NVENC_LIMITS.supports(VideoCodec::H265));
    }

    #[test]
    fn nvenc_resolves_av1_yuv420_at_both_eight_and_ten_bit() {
        for video in [AV1_420_8, AV1_420_10] {
            let plan = resolve_media_plan(nvenc_request(video), &nvenc_candidate())
                .unwrap_or_else(|error| panic!("{video:?} must resolve on NVENC: {error}"));
            assert_eq!(plan.backend, EncoderBackend::NativeNvenc);
            assert_eq!(plan.video.codec, VideoCodec::Av1);
            assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv420);
            assert_eq!(plan.video.bit_depth, video.bit_depth);
        }
    }

    #[test]
    fn nvenc_refuses_av1_yuv444_even_though_it_still_offers_hevc_yuv444() {
        // The regression this test exists to catch: NVENC's HEVC genuinely
        // reaches 4:4:4 (Rext) and must keep doing so (hardware-golden), but
        // its AV1 profile is Main-only (4:2:0). Both requests hit the exact
        // same `NVENC_LIMITS` contract; only the codec differs.
        assert_eq!(
            resolve_media_plan(nvenc_request(AV1_444_8), &nvenc_candidate()),
            Err(MediaPlanError::UnsupportedFormat),
            "a plan must never resolve to AV1 4:4:4 on NVENC -- it would fail at \
             NvEncInitializeEncoder"
        );
        let hevc_plan = resolve_media_plan(
            nvenc_request(VideoConfiguration::grading_reference()),
            &nvenc_candidate(),
        )
        .expect("HEVC 4:4:4 10-bit full range (hardware-golden) must still resolve on NVENC");
        assert_eq!(hevc_plan.video.chroma, ChromaSubsampling::Yuv444);
        assert_eq!(hevc_plan.video.codec, VideoCodec::H265);
    }

    #[test]
    fn nvenc_refuses_av1_twelve_bit() {
        let video = VideoConfiguration {
            codec: VideoCodec::Av1,
            bit_depth: BitDepth::Twelve,
            ..VideoConfiguration::legacy_h264()
        };
        assert_eq!(
            resolve_media_plan(nvenc_request(video), &nvenc_candidate()),
            Err(MediaPlanError::UnsupportedFormat),
            "NVENC has no 12-bit path at any subsampling for any codec"
        );
    }

    #[test]
    fn degrading_resolution_drops_av1_yuv444_to_yuv420_without_changing_codec() {
        let (plan, degradation) =
            resolve_media_plan_degrading(nvenc_request(AV1_444_8), &nvenc_candidate())
                .expect("AV1 4:4:4 must degrade to 4:2:0, not fail closed");
        assert_eq!(plan.video.codec, VideoCodec::Av1);
        assert_eq!(plan.video.chroma, ChromaSubsampling::Yuv420);
        assert!(
            !degradation.codec_changed,
            "AV1 stays AV1 on NVENC -- only chroma degrades"
        );
        assert!(degradation.chroma_changed);
    }

    #[test]
    fn ready_v1_rejects_a_claimed_av1_yuv444_plan_on_nvenc() {
        // A genuinely-resolved AV1 4:2:0 plan on NVENC, its READY line then
        // hand-edited to claim 4:4:4 -- the same technique
        // `ready_round_trip_is_strict_and_canonical` uses to simulate a
        // lying or buggy child. This must be rejected even though NVENC's
        // overall `supports_yuv444` claim is legitimately true (via HEVC).
        let plan = resolve_media_plan(nvenc_request(AV1_420_8), &nvenc_candidate())
            .expect("AV1 4:2:0 plan");
        let line = format_ready_v1(plan, None).replace("chroma=yuv420", "chroma=yuv444");
        assert_eq!(
            parse_ready_v1(
                &line,
                ReadyExpectation {
                    request: nvenc_request(AV1_444_8),
                    allowed_backends: &[EncoderBackend::NativeNvenc],
                    session_log_id: None,
                }
            ),
            Err(ReadyProtocolError::CapabilityConflict),
            "a child claiming AV1 4:4:4 on NVENC must be rejected, not accepted as truth"
        );
    }

    #[test]
    fn ready_round_trip_is_strict_and_canonical() {
        let plan = resolve_media_plan(
            REQUEST,
            &[BackendCandidate {
                backend: EncoderBackend::OpenH264,
                availability: BackendAvailability::Available(SOFTWARE_LIMITS),
            }],
        )
        .expect("plan");
        let sid = "26e9393f-a45b-4567-b634-6f4d34c58cb9";
        let line = format_ready_v1(plan, Some(sid));
        assert_eq!(
            parse_ready_v1(
                &line,
                ReadyExpectation {
                    request: REQUEST,
                    allowed_backends: &[EncoderBackend::OpenH264],
                    session_log_id: Some(sid),
                }
            )
            .expect("READY"),
            plan
        );
        assert!(matches!(
            parse_ready_v1(
                &format!("{line} fps=30"),
                ReadyExpectation {
                    request: REQUEST,
                    allowed_backends: &[EncoderBackend::OpenH264],
                    session_log_id: Some(sid),
                }
            ),
            Err(ReadyProtocolError::DuplicateField(_))
        ));
        assert!(matches!(
            parse_ready_v1(
                &format!("{line} extra=true"),
                ReadyExpectation {
                    request: REQUEST,
                    allowed_backends: &[EncoderBackend::OpenH264],
                    session_log_id: Some(sid),
                }
            ),
            Err(ReadyProtocolError::UnknownFields(_))
        ));

        let wildcard = ReadyExpectation {
            request: MediaRequest {
                width: 0,
                height: 0,
                ..REQUEST
            },
            allowed_backends: &[EncoderBackend::OpenH264],
            session_log_id: Some(sid),
        };
        assert_eq!(
            parse_ready_v1(&line, wildcard).expect("wildcard geometry accepts READY"),
            plan
        );
        assert!(
            parse_ready_v1(
                &line,
                ReadyExpectation {
                    request: MediaRequest {
                        width: 0,
                        ..REQUEST
                    },
                    allowed_backends: &[EncoderBackend::OpenH264],
                    session_log_id: Some(sid),
                }
            )
            .is_err()
        );
        assert!(parse_ready_v1(&line.replace("width=1920", "width=1919"), wildcard).is_err());
        assert!(parse_ready_v1(&line.replace("width=1920", "width=1922"), wildcard).is_err());

        let windows_auto = ReadyExpectation {
            request: REQUEST,
            allowed_backends: &[
                EncoderBackend::NativeNvenc,
                EncoderBackend::WindowsMediaFoundation,
            ],
            session_log_id: Some(sid),
        };
        assert_eq!(
            parse_ready_v1(&line, windows_auto),
            Err(ReadyProtocolError::BackendNotAllowed)
        );
        let mf_line = line.replace(
            "backend=openh264-sw-h264",
            "backend=media-foundation-sw-h264",
        );
        assert!(parse_ready_v1(&mf_line, windows_auto).is_ok());

        let linux_auto = ReadyExpectation {
            request: REQUEST,
            allowed_backends: &[EncoderBackend::NativeNvenc, EncoderBackend::OpenH264],
            session_log_id: Some(sid),
        };
        assert_eq!(
            parse_ready_v1(&mf_line, linux_auto),
            Err(ReadyProtocolError::BackendNotAllowed)
        );
        assert!(parse_ready_v1(&line, linux_auto).is_ok());
    }

    #[test]
    fn unavailable_round_trip_is_strict_and_typed() {
        let notice = BackendUnavailableNotice {
            backend: EncoderBackend::NativeNvenc,
            reason: BackendUnavailableReason::HardwareUnavailable,
        };
        let line = format_unavailable_v1(notice);
        assert_eq!(parse_unavailable_v1(&line).expect("UNAVAILABLE"), notice);
        assert!(parse_unavailable_v1(&format!("{line} detail=driver")).is_err());
        assert!(
            parse_unavailable_v1(
                "[capenc] UNAVAILABLE version=1 backend=native-nvenc code=cuda_init"
            )
            .is_err()
        );
    }

    /// The compatibility contract, in the direction that can break a running
    /// deployment: a capenc that names its capture path must still be
    /// understood, because `parse_ready_v1` rejects unknown fields outright.
    #[test]
    fn a_ready_line_naming_its_capture_path_still_parses() {
        let plan = resolve_media_plan(
            REQUEST,
            &[BackendCandidate {
                backend: EncoderBackend::OpenH264,
                availability: BackendAvailability::Available(SOFTWARE_LIMITS),
            }],
        )
        .expect("plan");
        let sid = "26e9393f-a45b-4567-b634-6f4d34c58cb9";
        let line = format_ready_v1_with_capture(plan, Some(CaptureBackend::XShm), Some(sid));

        assert_eq!(
            parse_ready_v1(
                &line,
                ReadyExpectation {
                    request: REQUEST,
                    allowed_backends: &[EncoderBackend::OpenH264],
                    session_log_id: Some(sid),
                }
            )
            .expect("a capture-naming READY line must parse"),
            plan,
            "naming the capture path must not change the video contract"
        );
        assert_eq!(parse_ready_capture(&line), Some(CaptureBackend::XShm));
    }

    /// The other direction: an older capenc names nothing, and that must read
    /// as unreported rather than as a guessed path.
    #[test]
    fn a_ready_line_without_a_capture_path_reports_none() {
        let plan = resolve_media_plan(
            REQUEST,
            &[BackendCandidate {
                backend: EncoderBackend::OpenH264,
                availability: BackendAvailability::Available(SOFTWARE_LIMITS),
            }],
        )
        .expect("plan");
        let line = format_ready_v1(plan, None);
        assert!(
            !line.contains("capture="),
            "the no-capture line must stay byte-identical to what older builds emit"
        );
        assert_eq!(parse_ready_capture(&line), None);
        assert_eq!(
            format_ready_v1_with_capture(plan, None, None),
            line,
            "None must produce exactly the legacy line"
        );
    }

    #[test]
    fn an_unknown_capture_token_reads_as_unreported_not_as_another_path() {
        let line = "[capenc] READY version=1 capture=holodeck";
        assert_eq!(parse_ready_capture(line), None);
        assert_eq!(CaptureBackend::from_token("holodeck"), None);
    }

    #[test]
    fn capture_tokens_round_trip_and_price_the_zero_copy_trade() {
        for backend in [
            CaptureBackend::NvFbc,
            CaptureBackend::XShm,
            CaptureBackend::DesktopDuplication,
            CaptureBackend::WindowsGraphicsCapture,
        ] {
            assert_eq!(
                CaptureBackend::from_token(backend.ready_token()),
                Some(backend)
            );
        }
        // The distinction the field exists to record.
        assert!(CaptureBackend::NvFbc.zero_copy());
        assert!(CaptureBackend::DesktopDuplication.zero_copy());
        assert!(!CaptureBackend::XShm.zero_copy());
        assert!(!CaptureBackend::WindowsGraphicsCapture.zero_copy());
    }
}
