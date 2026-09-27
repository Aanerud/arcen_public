use std::fmt::{Display, Formatter};

use arcen_protocol::messages::VideoSelectionIntent;

use crate::{
    BitDepth, ChromaSubsampling, CodecSet, ColorMatrix, ColorRange, EncodeIntent, VideoCodec,
    VideoConfiguration,
};

use super::{ResolvedClientVideoRequest, ResolvedMediaPlan, VideoVariant};

/// Operator policy for negotiated depth, range, and matrix.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ColorPolicy {
    AlwaysOn,
    AlwaysOff,
    DefaultOn,
    #[default]
    DefaultOff,
}

impl ColorPolicy {
    pub const ALL: &'static [Self] = &[
        Self::AlwaysOn,
        Self::AlwaysOff,
        Self::DefaultOn,
        Self::DefaultOff,
    ];

    pub const CONSERVATIVE_BIT_DEPTH: BitDepth = BitDepth::Eight;
    pub const CONSERVATIVE_COLOR_RANGE: ColorRange = ColorRange::Limited;
    pub const CONSERVATIVE_COLOR_MATRIX: ColorMatrix = ColorMatrix::Bt709;

    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::AlwaysOn => "always-on",
            Self::AlwaysOff => "always-off",
            Self::DefaultOn => "default-on",
            Self::DefaultOff => "default-off",
        }
    }

    #[must_use]
    pub fn from_token(value: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .copied()
            .find(|policy| policy.token() == value)
    }

    #[must_use]
    pub fn resolve_bit_depth(
        self,
        ceiling: BitDepth,
        client_request: Option<BitDepth>,
    ) -> BitDepth {
        match self {
            Self::AlwaysOn => ceiling,
            Self::AlwaysOff => Self::CONSERVATIVE_BIT_DEPTH,
            Self::DefaultOn => client_request.map_or(ceiling, |wanted| wanted.min(ceiling)),
            Self::DefaultOff => {
                client_request.map_or(Self::CONSERVATIVE_BIT_DEPTH, |wanted| wanted.min(ceiling))
            }
        }
    }

    #[must_use]
    pub fn resolve_color_range(
        self,
        ceiling: ColorRange,
        client_request: Option<ColorRange>,
    ) -> ColorRange {
        match self {
            Self::AlwaysOn => ceiling,
            Self::AlwaysOff => Self::CONSERVATIVE_COLOR_RANGE,
            Self::DefaultOn => client_request.map_or(ceiling, |wanted| wanted.min(ceiling)),
            Self::DefaultOff => {
                client_request.map_or(Self::CONSERVATIVE_COLOR_RANGE, |wanted| wanted.min(ceiling))
            }
        }
    }

    #[must_use]
    pub fn resolve_color_matrix(
        self,
        ceiling: ColorMatrix,
        client_request: Option<ColorMatrix>,
    ) -> ColorMatrix {
        match self {
            Self::AlwaysOn => ceiling,
            Self::AlwaysOff => Self::CONSERVATIVE_COLOR_MATRIX,
            Self::DefaultOn => client_request.unwrap_or(ceiling),
            Self::DefaultOff => client_request.unwrap_or(Self::CONSERVATIVE_COLOR_MATRIX),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorCeiling {
    pub bit_depth: BitDepth,
    pub color_range: ColorRange,
    pub color_matrix: ColorMatrix,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)]
pub struct ClientColorRequest {
    pub bit_depth: Option<BitDepth>,
    pub color_range: Option<ColorRange>,
    pub color_matrix: Option<ColorMatrix>,
    pub supports_main10: bool,
    pub supports_main12: bool,
    pub supports_full_range: bool,
    pub supports_identity_matrix: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorMatrixCapabilities {
    pub bt601: bool,
    pub bt2020_ncl: bool,
}

impl ColorMatrixCapabilities {
    pub const ALL: Self = Self {
        bt601: true,
        bt2020_ncl: true,
    };

    pub const BT709_ONLY: Self = Self {
        bt601: false,
        bt2020_ncl: false,
    };
}

#[must_use]
pub fn cap_bit_depth_to_client(
    depth: BitDepth,
    supports_main10: bool,
    supports_main12: bool,
) -> BitDepth {
    match depth {
        BitDepth::Twelve if !supports_main12 => {
            cap_bit_depth_to_client(BitDepth::Ten, supports_main10, supports_main12)
        }
        BitDepth::Ten if !supports_main10 => BitDepth::Eight,
        other => other,
    }
}

#[must_use]
pub fn resolve_client_color_request(
    policy: ColorPolicy,
    ceiling: ColorCeiling,
    request: ClientColorRequest,
) -> (BitDepth, ColorRange, ColorMatrix) {
    resolve_client_color_request_with_matrix_caps(
        policy,
        ceiling,
        request,
        ColorMatrixCapabilities::ALL,
    )
}

#[must_use]
pub fn resolve_client_color_request_with_matrix_caps(
    policy: ColorPolicy,
    ceiling: ColorCeiling,
    request: ClientColorRequest,
    matrix_capabilities: ColorMatrixCapabilities,
) -> (BitDepth, ColorRange, ColorMatrix) {
    let bit_depth = cap_bit_depth_to_client(
        policy.resolve_bit_depth(ceiling.bit_depth, request.bit_depth),
        request.supports_main10,
        request.supports_main12,
    );
    let color_range = match policy.resolve_color_range(ceiling.color_range, request.color_range) {
        ColorRange::Full if !request.supports_full_range => ColorRange::Limited,
        range => range,
    };
    let color_matrix = match policy.resolve_color_matrix(ceiling.color_matrix, request.color_matrix)
    {
        ColorMatrix::Identity if !request.supports_identity_matrix => ColorMatrix::Bt709,
        ColorMatrix::Bt601 if !matrix_capabilities.bt601 => ColorMatrix::Bt709,
        ColorMatrix::Bt2020Ncl if !matrix_capabilities.bt2020_ncl => ColorMatrix::Bt709,
        matrix => matrix,
    };
    (bit_depth, color_range, color_matrix)
}

#[must_use]
pub fn color_contract_is_servable(video: VideoConfiguration, plan: &ResolvedMediaPlan) -> bool {
    VideoVariant::new(video).is_coherent()
        && plan.codecs.contains(video.codec)
        && plan.chroma.contains(video.chroma)
        && plan.bit_depths.contains(video.bit_depth)
        && plan.ranges.contains(video.range)
        && (!video.matrix.is_identity() || plan.backend.contract().identity_matrix)
}

const ADAPTIVE_FROM_AV1: [VideoCodec; 3] = [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264];
const ADAPTIVE_FROM_HEVC: [VideoCodec; 2] = [VideoCodec::H265, VideoCodec::H264];
const ADAPTIVE_FROM_H264: [VideoCodec; 1] = [VideoCodec::H264];
const ADAPTIVE_UNSUPPORTED: [VideoCodec; 0] = [];

/// Ordered hardware codec candidates for an ordinary adaptive session.
#[must_use]
pub const fn adaptive_codec_ladder(preferred: VideoCodec) -> &'static [VideoCodec] {
    match preferred {
        VideoCodec::Av1 => &ADAPTIVE_FROM_AV1,
        VideoCodec::H265 => &ADAPTIVE_FROM_HEVC,
        VideoCodec::H264 => &ADAPTIVE_FROM_H264,
        VideoCodec::Jpeg | VideoCodec::Vp9 => &ADAPTIVE_UNSUPPORTED,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostInitialVideoPolicy {
    pub current: VideoConfiguration,
    pub color_policy: ColorPolicy,
    pub codec_pinned: bool,
    pub variant_pinned: bool,
    pub max_fps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedHostInitialVideo {
    pub video: VideoConfiguration,
    pub selection: VideoSelectionIntent,
    pub encode_intent: EncodeIntent,
    pub max_fps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInitialVideoError(String);

impl Display for HostInitialVideoError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HostInitialVideoError {}

fn client_can_decode(
    video: VideoConfiguration,
    capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg,
) -> bool {
    let codec = match video.codec {
        VideoCodec::Av1 => capabilities.av1,
        VideoCodec::H265 => capabilities.h265,
        VideoCodec::H264 => capabilities.h264,
        VideoCodec::Jpeg | VideoCodec::Vp9 => false,
    };
    let chroma = video.chroma != crate::ChromaSubsampling::Yuv444 || capabilities.yuv444;
    let depth = match video.bit_depth {
        BitDepth::Eight => true,
        BitDepth::Ten => capabilities.main10,
        BitDepth::Twelve => capabilities.main10 && capabilities.main12,
    };
    let range = video.range != ColorRange::Full || capabilities.full_range;
    let matrix = match video.matrix {
        ColorMatrix::Bt709 => true,
        ColorMatrix::Bt601 => capabilities.bt601_matrix,
        ColorMatrix::Bt2020Ncl => capabilities.bt2020_ncl_matrix,
        ColorMatrix::Identity => capabilities.identity_matrix,
    };
    codec && chroma && depth && range && matrix
}

fn codec_supported_by_client(
    codec: VideoCodec,
    capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg,
) -> bool {
    match codec {
        VideoCodec::Av1 => capabilities.av1,
        VideoCodec::H265 => capabilities.h265,
        VideoCodec::H264 => capabilities.h264,
        VideoCodec::Jpeg | VideoCodec::Vp9 => false,
    }
}

fn resolve_codec(
    requested: VideoCodec,
    policy: HostInitialVideoPolicy,
    supported_codecs: CodecSet,
    capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg,
) -> Option<VideoCodec> {
    if policy.codec_pinned {
        return (supported_codecs.contains(policy.current.codec)
            && codec_supported_by_client(policy.current.codec, capabilities))
        .then_some(policy.current.codec);
    }
    adaptive_codec_ladder(requested)
        .iter()
        .copied()
        .find(|codec| {
            supported_codecs.contains(*codec) && codec_supported_by_client(*codec, capabilities)
        })
}

/// Apply host colour policy and exact pins to a validated client request,
/// for a host that has not measured its encoders.
///
/// Without a measurement the client's codec is followed, and the host's
/// encoder preflight is what refuses one it cannot run. Only a pinned codec
/// or variant is held to the configured codec: an unpinned configured codec
/// is a default, not a measurement, and treating it as one turned every HEVC
/// Grading request on the Linux and Windows Piers into H.264. A host that has
/// measured its encoders calls
/// [`resolve_host_initial_video_with_supported_codecs`] instead.
///
/// The active product hosts permit 4:4:4 only with HEVC. Probe-only H.264
/// 4:4:4 remains available below the product host layer.
///
/// # Errors
///
/// Returns an error when an exact administrator pin is incoherent,
/// incompatible with the requested 4:4:4 contract, or outside the client's
/// advertised decode capabilities.
pub fn resolve_host_initial_video(
    request: ResolvedClientVideoRequest,
    policy: HostInitialVideoPolicy,
) -> Result<ResolvedHostInitialVideo, HostInitialVideoError> {
    let codecs = if policy.codec_pinned || policy.variant_pinned {
        CodecSet::from_slice(&[policy.current.codec])
    } else {
        CodecSet::from_slice(&[VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1])
    };
    resolve_host_initial_video_with_supported_codecs(request, policy, codecs)
}

/// Apply host policy while intersecting the measured host codecs with the client.
///
/// # Errors
///
/// Returns an error when exact administrator pins are unsupported, or no
/// measured host codec is compatible with the client.
pub fn resolve_host_initial_video_with_supported_codecs(
    request: ResolvedClientVideoRequest,
    policy: HostInitialVideoPolicy,
    supported_codecs: CodecSet,
) -> Result<ResolvedHostInitialVideo, HostInitialVideoError> {
    if policy.variant_pinned {
        if !supported_codecs.contains(policy.current.codec) {
            return Err(HostInitialVideoError(format!(
                "administrator video.variant {} requires an encoder the host probe did not make available",
                VideoVariant::new(policy.current).id()
            )));
        }
        if !client_can_decode(policy.current, request.capabilities) {
            return Err(HostInitialVideoError(format!(
                "administrator video.variant {} is incompatible with the client decode capabilities",
                VideoVariant::new(policy.current).id()
            )));
        }
        return Ok(ResolvedHostInitialVideo {
            video: policy.current,
            selection: VideoSelectionIntent::Exact,
            encode_intent: request.encode_intent,
            max_fps: policy.max_fps.min(request.max_fps),
        });
    }

    let (bit_depth, range, matrix) = resolve_client_color_request_with_matrix_caps(
        policy.color_policy,
        ColorCeiling {
            bit_depth: policy.current.bit_depth,
            color_range: policy.current.range,
            color_matrix: policy.current.matrix,
        },
        ClientColorRequest {
            bit_depth: Some(request.video.bit_depth),
            color_range: Some(request.video.range),
            color_matrix: Some(request.video.matrix),
            supports_main10: request.capabilities.main10,
            supports_main12: request.capabilities.main12,
            supports_full_range: request.capabilities.full_range,
            supports_identity_matrix: request.capabilities.identity_matrix,
        },
        ColorMatrixCapabilities {
            bt601: request.capabilities.bt601_matrix,
            bt2020_ncl: request.capabilities.bt2020_ncl_matrix,
        },
    );
    let codec = resolve_codec(
        request.video.codec,
        policy,
        supported_codecs,
        request.capabilities,
    )
    .ok_or_else(|| {
        HostInitialVideoError(format!(
            "no measured encoder codec can satisfy client preference {}",
            request.video.codec.token()
        ))
    })?;
    // A codec the ladder fell back to serves 4:2:0 and says so; a codec an
    // administrator pinned is refused instead, because the pin is a promise
    // the request cannot be kept within.
    let chroma = if request.video.chroma == ChromaSubsampling::Yuv444 && codec != VideoCodec::H265 {
        if policy.codec_pinned {
            return Err(HostInitialVideoError(format!(
                "administrator codec pin {} cannot serve the requested yuv444 contract",
                codec.token()
            )));
        }
        ChromaSubsampling::Yuv420
    } else {
        request.video.chroma
    };
    let video = VideoConfiguration {
        codec,
        chroma,
        bit_depth,
        range,
        matrix,
        primaries: request.video.primaries,
        transfer: request.video.transfer,
    };
    let variant = VideoVariant::new(video);
    if !variant.is_coherent() {
        return Err(HostInitialVideoError(format!(
            "initial video request resolves to an incoherent contract: {}",
            variant.id()
        )));
    }
    if !client_can_decode(video, request.capabilities) {
        return Err(HostInitialVideoError(format!(
            "administrator video pin {} is incompatible with the client decode capabilities",
            variant.id()
        )));
    }
    Ok(ResolvedHostInitialVideo {
        video,
        selection: if policy.codec_pinned {
            VideoSelectionIntent::Exact
        } else {
            request.selection
        },
        encode_intent: request.encode_intent,
        max_fps: policy.max_fps.min(request.max_fps),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChromaSubsampling, ColorPrimaries, TransferCharacteristics};

    fn video(codec: VideoCodec) -> VideoConfiguration {
        VideoConfiguration {
            codec,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            range: ColorRange::Limited,
            matrix: ColorMatrix::Bt709,
            primaries: ColorPrimaries::Bt709,
            transfer: TransferCharacteristics::Bt709,
        }
    }

    #[test]
    fn adaptive_order_is_typed_and_stable() {
        assert_eq!(
            adaptive_codec_ladder(VideoCodec::Av1),
            [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264]
        );
        assert_eq!(
            adaptive_codec_ladder(VideoCodec::H265),
            [VideoCodec::H265, VideoCodec::H264]
        );
    }

    #[test]
    fn unadvertised_matrix_falls_back_to_bt709() {
        let resolved = resolve_client_color_request_with_matrix_caps(
            ColorPolicy::AlwaysOn,
            ColorCeiling {
                bit_depth: BitDepth::Ten,
                color_range: ColorRange::Full,
                color_matrix: ColorMatrix::Bt2020Ncl,
            },
            ClientColorRequest {
                bit_depth: None,
                color_range: None,
                color_matrix: None,
                supports_main10: true,
                supports_main12: false,
                supports_full_range: true,
                supports_identity_matrix: false,
            },
            ColorMatrixCapabilities::BT709_ONLY,
        );
        assert_eq!(resolved.2, ColorMatrix::Bt709);
    }

    #[test]
    fn fidelity_request_degrades_chroma_after_h264_codec_fallback() {
        let request = ResolvedClientVideoRequest {
            selection: VideoSelectionIntent::ColorFidelity,
            video: VideoConfiguration {
                codec: VideoCodec::H265,
                chroma: ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Ten,
                ..video(VideoCodec::H265)
            },
            encode_intent: EncodeIntent::Quality,
            max_fps: 60,
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                yuv444: true,
                main10: true,
                ..Default::default()
            },
        };
        let resolved = resolve_host_initial_video_with_supported_codecs(
            request,
            HostInitialVideoPolicy {
                current: video(VideoCodec::H264),
                color_policy: ColorPolicy::DefaultOff,
                codec_pinned: false,
                variant_pinned: false,
                max_fps: 60,
            },
            CodecSet::from_slice(&[VideoCodec::H264]),
        )
        .expect("H.264 fallback should degrade before validation");

        assert_eq!(resolved.video.codec, VideoCodec::H264);
        assert_eq!(resolved.video.chroma, ChromaSubsampling::Yuv420);
    }

    #[test]
    fn an_unmeasured_host_follows_the_requested_codec_unless_it_is_pinned() {
        let request = ResolvedClientVideoRequest {
            selection: VideoSelectionIntent::ColorFidelity,
            video: VideoConfiguration {
                chroma: ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Ten,
                ..video(VideoCodec::H265)
            },
            encode_intent: EncodeIntent::Quality,
            max_fps: 30,
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                yuv444: true,
                main10: true,
                ..Default::default()
            },
        };
        let policy = HostInitialVideoPolicy {
            current: VideoConfiguration {
                bit_depth: BitDepth::Ten,
                ..video(VideoCodec::H264)
            },
            color_policy: ColorPolicy::AlwaysOn,
            codec_pinned: false,
            variant_pinned: false,
            max_fps: 60,
        };
        let resolved = resolve_host_initial_video(request, policy).expect("follows the client");
        assert_eq!(
            (
                resolved.video.codec,
                resolved.video.chroma,
                resolved.video.bit_depth
            ),
            (VideoCodec::H265, ChromaSubsampling::Yuv444, BitDepth::Ten),
            "an unpinned H.264 default is not a measurement"
        );

        let pinned = resolve_host_initial_video(
            request,
            HostInitialVideoPolicy {
                codec_pinned: true,
                ..policy
            },
        )
        .expect_err("a pinned H.264 cannot keep a 4:4:4 request");
        assert!(pinned.to_string().contains("yuv444"));
    }

    #[test]
    fn production_resolver_never_selects_a_codec_the_host_probe_rejected() {
        let request = ResolvedClientVideoRequest {
            selection: VideoSelectionIntent::AdaptivePerformance,
            video: video(VideoCodec::H265),
            encode_intent: EncodeIntent::Interactive,
            max_fps: 60,
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                ..Default::default()
            },
        };
        let resolved = resolve_host_initial_video_with_supported_codecs(
            request,
            HostInitialVideoPolicy {
                current: video(VideoCodec::H264),
                color_policy: ColorPolicy::DefaultOff,
                codec_pinned: false,
                variant_pinned: false,
                max_fps: 60,
            },
            CodecSet::from_slice(&[VideoCodec::H264]),
        )
        .expect("H.264 is measured and client-supported");
        assert_eq!(resolved.video.codec, VideoCodec::H264);
    }
}
