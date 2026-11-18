// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

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

/// Ordered hardware codec candidates for an ordinary adaptive session.
#[must_use]
pub fn adaptive_codec_ladder(preferred: VideoCodec) -> &'static [VideoCodec] {
    crate::video::pipeline_codec_ladder(crate::video::PipelineId::Auto, preferred)
}

/// Ordered hardware codec candidates for an adaptive stream whose largest
/// picture is `width`x`height` on `backend`.
///
/// The [`adaptive_codec_ladder`] from `preferred`, without the codecs the
/// backend cannot encode at that size ([`super::codec_dimension_ceiling`]).
/// When none of those can, the next richer codecs follow, HEVC before AV1,
/// each only when the Deck decodes it (`client`): a Deck that asked for
/// H.264 under an adaptive contract authorized the host's choice, and a
/// 5120 px monitor is served with HEVC rather than refused. Empty when no
/// codec the Deck decodes fits; the caller then fits the picture or
/// refuses, as before.
#[must_use]
pub fn adaptive_codec_ladder_for_size(
    preferred: VideoCodec,
    backend: super::EncoderBackend,
    width: u32,
    height: u32,
    client: Option<arcen_protocol::messages::ClientVideoCapabilitiesMsg>,
) -> Vec<VideoCodec> {
    let fits = |codec: VideoCodec| super::codec_fits_dimensions(backend, codec, width, height);
    let ladder: Vec<VideoCodec> = adaptive_codec_ladder(preferred)
        .iter()
        .copied()
        .filter(|codec| fits(*codec))
        .collect();
    if !ladder.is_empty() {
        return ladder;
    }
    let Some(client) = client else {
        return ladder;
    };
    [VideoCodec::H265, VideoCodec::Av1]
        .into_iter()
        .filter(|codec| {
            *codec != preferred
                && backend.contract().supports(*codec)
                && fits(*codec)
                && codec_supported_by_client(*codec, client)
        })
        .collect()
}

/// The uniform codec candidates of an adaptive multi-monitor roster on
/// `backend`: [`adaptive_codec_ladder_for_size`] for its largest monitor
/// (ceilings are square, so the longer side is held against the width), or
/// the plain [`adaptive_codec_ladder`] when no codec the Deck decodes fits,
/// so the refusal stays the encoder's. Every Pier's roster admission uses it.
#[must_use]
pub fn adaptive_roster_codec_ladder(
    preferred: VideoCodec,
    backend: super::EncoderBackend,
    monitors: impl IntoIterator<Item = (u32, u32)>,
    client: Option<arcen_protocol::messages::ClientVideoCapabilitiesMsg>,
) -> Vec<VideoCodec> {
    let (longer, shorter) =
        monitors
            .into_iter()
            .fold((0, 0), |(longer, shorter), (width, height)| {
                (
                    longer.max(width.max(height)),
                    shorter.max(width.min(height)),
                )
            });
    let sized = adaptive_codec_ladder_for_size(preferred, backend, longer, shorter, client);
    if sized.is_empty() {
        adaptive_codec_ladder(preferred).to_vec()
    } else {
        sized
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostInitialVideoPolicy {
    pub current: VideoConfiguration,
    pub color_policy: ColorPolicy,
    pub codec_pinned: bool,
    pub variant_pinned: bool,
    /// The administrator's frame-rate ceiling (`video.fps`); `None` when
    /// none is configured, so Auto and Speed follow the Deck display.
    pub administrator_max_fps: Option<u32>,
    /// The served Deck display's refresh
    /// (`AuthResponse::display_refresh_hz`).
    pub display_refresh_hz: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedHostInitialVideo {
    pub video: VideoConfiguration,
    pub selection: VideoSelectionIntent,
    pub pipeline: Option<crate::video::PipelineId>,
    pub encode_intent: EncodeIntent,
    /// `frame_rate.fps`, the rate the session starts at.
    pub max_fps: u32,
    /// The rate with its reason, before the host narrows it to its encoder
    /// and capture ([`crate::video::SessionFrameRate::narrow`]).
    pub frame_rate: crate::video::SessionFrameRate,
}

impl ResolvedHostInitialVideo {
    /// The same plan at most `cap` frames a second, for `basis`.
    #[must_use]
    pub fn narrow_fps(self, cap: u32, basis: crate::video::FrameRateBasis) -> Self {
        let frame_rate = self.frame_rate.narrow(cap, basis);
        Self {
            max_fps: frame_rate.fps,
            frame_rate,
            ..self
        }
    }
}

/// The resolved plan for `video`, at the rate `rate_pipeline`'s contract
/// gives the request on this host.
fn resolved_video(
    video: VideoConfiguration,
    selection: VideoSelectionIntent,
    request: &ResolvedClientVideoRequest,
    rate_pipeline: Option<crate::video::PipelineId>,
    policy: HostInitialVideoPolicy,
) -> ResolvedHostInitialVideo {
    let frame_rate = crate::video::resolve_session_frame_rate(crate::video::FrameRateRequest {
        pipeline: rate_pipeline,
        requested_fps: request.max_fps,
        display_refresh_hz: policy.display_refresh_hz,
        administrator_max_fps: policy.administrator_max_fps,
    });
    ResolvedHostInitialVideo {
        video,
        selection,
        pipeline: request.pipeline,
        encode_intent: request.encode_intent,
        max_fps: frame_rate.fps,
        frame_rate,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostInitialVideoError(String);

impl Display for HostInitialVideoError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for HostInitialVideoError {}

/// Whether a client that advertised `capabilities` can decode `video`: its
/// codec, chroma, depth, range and matrix. The one rule a Pier applies to a
/// request and a Deck applies before it makes one, so a Deck never asks for
/// what its own decoder would refuse.
#[must_use]
pub fn client_can_decode(
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
    requested_pipeline: Option<crate::video::PipelineId>,
    policy: HostInitialVideoPolicy,
    supported_codecs: CodecSet,
    capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg,
) -> Option<VideoCodec> {
    if policy.codec_pinned {
        return (supported_codecs.contains(policy.current.codec)
            && codec_supported_by_client(policy.current.codec, capabilities))
        .then_some(policy.current.codec);
    }
    crate::video::pipeline_codec_ladder(
        requested_pipeline.unwrap_or(crate::video::PipelineId::Auto),
        requested,
    )
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
        // An exact administrator variant is served as Custom: the Deck's
        // own rate, under the administrator's ceiling.
        return Ok(resolved_video(
            policy.current,
            VideoSelectionIntent::Exact,
            &request,
            None,
            policy,
        ));
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
        request.pipeline,
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
    // A codec pin makes the stream exact (served as Custom), and an exact
    // stream keeps the Deck's own rate: a capture child admits an exact
    // request only at exactly that rate.
    let (selection, rate_pipeline) = if policy.codec_pinned {
        (VideoSelectionIntent::Exact, None)
    } else {
        (request.selection, request.pipeline)
    };
    Ok(resolved_video(
        video,
        selection,
        &request,
        rate_pipeline,
        policy,
    ))
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
            motion_priority: crate::video::MotionPriority::Detail,
            pipeline: Some(crate::video::PipelineId::Grading),
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
                administrator_max_fps: None,
                display_refresh_hz: Some(60),
            },
            CodecSet::from_slice(&[VideoCodec::H264]),
        )
        .expect("H.264 fallback should degrade before validation");

        assert_eq!(resolved.video.codec, VideoCodec::H264);
        assert_eq!(resolved.video.chroma, ChromaSubsampling::Yuv420);
    }

    #[test]
    fn host_codec_resolution_consumes_the_pipeline_ladder() {
        let request = ResolvedClientVideoRequest {
            selection: VideoSelectionIntent::ColorFidelity,
            video: VideoConfiguration {
                codec: VideoCodec::Av1,
                chroma: ChromaSubsampling::Yuv444,
                bit_depth: BitDepth::Ten,
                ..video(VideoCodec::Av1)
            },
            encode_intent: EncodeIntent::Quality,
            motion_priority: crate::video::MotionPriority::Detail,
            pipeline: Some(crate::video::PipelineId::Grading),
            max_fps: 30,
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                av1: true,
                yuv444: true,
                main10: true,
                ..Default::default()
            },
        };
        let resolved = resolve_host_initial_video_with_supported_codecs(
            request,
            HostInitialVideoPolicy {
                current: video(VideoCodec::H264),
                color_policy: ColorPolicy::AlwaysOn,
                codec_pinned: false,
                variant_pinned: false,
                administrator_max_fps: None,
                display_refresh_hz: Some(60),
            },
            CodecSet::from_slice(&[VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264]),
        )
        .expect("grading ladder resolves");

        assert_eq!(
            resolved.video.codec,
            VideoCodec::H265,
            "Grading consumes its HEVC-first ladder instead of Auto's AV1-first ladder"
        );
        assert_eq!(resolved.video.chroma, ChromaSubsampling::Yuv444);
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
            motion_priority: crate::video::MotionPriority::Detail,
            pipeline: Some(crate::video::PipelineId::Grading),
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
            administrator_max_fps: None,
            display_refresh_hz: Some(60),
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
            motion_priority: crate::video::MotionPriority::Detail,
            pipeline: Some(crate::video::PipelineId::Auto),
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
                administrator_max_fps: None,
                display_refresh_hz: Some(60),
            },
            CodecSet::from_slice(&[VideoCodec::H264]),
        )
        .expect("H.264 is measured and client-supported");
        assert_eq!(resolved.video.codec, VideoCodec::H264);
    }

    #[test]
    fn auto_follows_the_deck_display_and_an_administrator_ceiling_holds() {
        let request = ResolvedClientVideoRequest {
            selection: VideoSelectionIntent::AdaptivePerformance,
            video: video(VideoCodec::H265),
            encode_intent: EncodeIntent::Interactive,
            motion_priority: crate::video::MotionPriority::Detail,
            pipeline: Some(crate::video::PipelineId::Auto),
            max_fps: 30,
            capabilities: arcen_protocol::messages::ClientVideoCapabilitiesMsg {
                h264: true,
                h265: true,
                ..Default::default()
            },
        };
        let policy = HostInitialVideoPolicy {
            current: video(VideoCodec::H264),
            color_policy: ColorPolicy::DefaultOff,
            codec_pinned: false,
            variant_pinned: false,
            administrator_max_fps: None,
            display_refresh_hz: Some(120),
        };
        let resolved = resolve_host_initial_video(request, policy).expect("resolves");
        assert_eq!(resolved.max_fps, 120);
        assert_eq!(
            resolved.frame_rate.basis,
            crate::video::FrameRateBasis::Display
        );
        let capped = resolve_host_initial_video(
            request,
            HostInitialVideoPolicy {
                administrator_max_fps: Some(60),
                ..policy
            },
        )
        .expect("resolves");
        assert_eq!(capped.max_fps, 60);
        let narrowed = capped.narrow_fps(30, crate::video::FrameRateBasis::Encoder);
        assert_eq!(
            (narrowed.max_fps, narrowed.frame_rate.basis),
            (30, crate::video::FrameRateBasis::Encoder)
        );
        // A codec pin makes the stream exact: the Deck's own rate.
        let pinned = resolve_host_initial_video(
            request,
            HostInitialVideoPolicy {
                codec_pinned: true,
                ..policy
            },
        )
        .expect("resolves");
        assert_eq!(
            (pinned.max_fps, pinned.frame_rate.basis),
            (30, crate::video::FrameRateBasis::Request)
        );
    }

    #[test]
    fn an_adaptive_stream_past_h264s_ceiling_is_served_with_a_codec_that_fits() {
        use super::super::EncoderBackend;
        let deck = |h265: bool, av1: bool| arcen_protocol::messages::ClientVideoCapabilitiesMsg {
            h264: true,
            h265,
            av1,
            ..arcen_protocol::messages::ClientVideoCapabilitiesMsg::default()
        };
        let nvenc = EncoderBackend::NativeNvenc;
        // A 2560x1440 monitor keeps the Deck's H.264.
        assert_eq!(
            adaptive_codec_ladder_for_size(
                VideoCodec::H264,
                nvenc,
                2560,
                1440,
                Some(deck(true, true))
            ),
            [VideoCodec::H264]
        );
        // A 5120x2880 monitor: NVENC's H.264 stops at 4096, so HEVC, then AV1.
        assert_eq!(
            adaptive_codec_ladder_for_size(
                VideoCodec::H264,
                nvenc,
                5120,
                2880,
                Some(deck(true, true))
            ),
            [VideoCodec::H265, VideoCodec::Av1]
        );
        // Only what the Deck decodes, and nothing for a Deck that said nothing.
        assert_eq!(
            adaptive_codec_ladder_for_size(
                VideoCodec::H264,
                nvenc,
                5120,
                2880,
                Some(deck(false, true))
            ),
            [VideoCodec::Av1]
        );
        assert!(
            adaptive_codec_ladder_for_size(
                VideoCodec::H264,
                nvenc,
                5120,
                2880,
                Some(deck(false, false))
            )
            .is_empty()
        );
        assert!(
            adaptive_codec_ladder_for_size(VideoCodec::H264, nvenc, 5120, 2880, None).is_empty()
        );
        // From AV1, the codecs below that still fit.
        assert_eq!(
            adaptive_codec_ladder_for_size(
                VideoCodec::Av1,
                nvenc,
                5120,
                2880,
                Some(deck(true, true))
            ),
            [VideoCodec::Av1, VideoCodec::H265]
        );
        // Past every ceiling: nothing; the caller fits or refuses.
        assert!(
            adaptive_codec_ladder_for_size(
                VideoCodec::H264,
                nvenc,
                10240,
                2880,
                Some(deck(true, true))
            )
            .is_empty()
        );
    }
}
