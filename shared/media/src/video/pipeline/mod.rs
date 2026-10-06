//! Shared stream-pipeline contracts.
//!
//! Each user-visible pipeline owns a complete data contract here. Platform
//! adapters consume these values instead of re-deriving bitrate, codec order,
//! colour targets, and cadence policy locally.

use core::time::Duration;

use arcen_protocol::messages::{ServedStreamPipeline, StreamPipeline, VideoSelectionIntent};

use crate::rate_control::{ProbeStep, RateControlPolicy};
use crate::{
    BitDepth, ChromaSubsampling, ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent,
    TransferCharacteristics, VideoCodec, VideoConfiguration,
};

use super::{
    AcceleratorClass, MotionPriority, average_bitrate_bps, encoder_buffer_frames,
    link_capped_average_bitrate_bps,
};

mod auto;
mod grading;
mod hdr;
mod software;
mod speed;

/// Stable pipeline identity known by this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineId {
    Auto,
    Speed,
    Grading,
    Hdr,
    Software,
}

impl PipelineId {
    pub const ALL: &'static [Self] = &[
        Self::Auto,
        Self::Speed,
        Self::Grading,
        Self::Hdr,
        Self::Software,
    ];

    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Speed => "speed",
            Self::Grading => "grading",
            Self::Hdr => "hdr",
            Self::Software => "software",
        }
    }

    #[must_use]
    pub const fn request_wire(self) -> Option<StreamPipeline> {
        match self {
            Self::Auto => Some(StreamPipeline::Auto),
            Self::Speed => Some(StreamPipeline::Speed),
            Self::Grading => Some(StreamPipeline::Grading),
            Self::Hdr => Some(StreamPipeline::Hdr),
            Self::Software => None,
        }
    }

    #[must_use]
    pub const fn served_wire(self) -> ServedStreamPipeline {
        match self {
            Self::Auto => ServedStreamPipeline::Auto,
            Self::Speed => ServedStreamPipeline::Speed,
            Self::Grading => ServedStreamPipeline::Grading,
            Self::Hdr => ServedStreamPipeline::Hdr,
            Self::Software => ServedStreamPipeline::Software,
        }
    }

    #[must_use]
    pub fn from_served_wire(pipeline: &ServedStreamPipeline) -> Option<Self> {
        match pipeline {
            ServedStreamPipeline::Auto => Some(Self::Auto),
            ServedStreamPipeline::Speed => Some(Self::Speed),
            ServedStreamPipeline::Grading => Some(Self::Grading),
            ServedStreamPipeline::Hdr => Some(Self::Hdr),
            ServedStreamPipeline::Software => Some(Self::Software),
            ServedStreamPipeline::Custom | ServedStreamPipeline::Unknown(_) => None,
        }
    }

    #[must_use]
    pub fn from_wire(pipeline: &StreamPipeline) -> Option<Self> {
        match pipeline {
            StreamPipeline::Auto => Some(Self::Auto),
            StreamPipeline::Speed => Some(Self::Speed),
            StreamPipeline::Grading => Some(Self::Grading),
            StreamPipeline::Hdr => Some(Self::Hdr),
            StreamPipeline::Unknown(_) => None,
        }
    }

    /// Whether serving this product pipeline requires the operating system's
    /// display compositor to be in HDR mode.
    #[must_use]
    pub const fn wants_display_hdr(self) -> bool {
        matches!(self, Self::Hdr)
    }
}

/// Whether a served pipeline wants the host display compositor in HDR mode.
///
/// Auto and Speed are eight-bit SDR. Grading is a ten-bit SDR contract whose
/// Windows path captures FP16 scRGB and converts to SDR. Software and custom
/// streams likewise must not leave Windows Advanced Color enabled.
#[must_use]
pub fn served_pipeline_wants_display_hdr(served: Option<&ServedStreamPipeline>) -> bool {
    served
        .and_then(PipelineId::from_served_wire)
        .is_some_and(PipelineId::wants_display_hdr)
}

/// Whether the host display compositor must be in HDR mode for this session.
///
/// `request_hdr10` preserves the legacy Windows behavior for exact/custom or
/// older clients that asked for PQ before named pipelines existed.
#[must_use]
pub fn session_wants_display_hdr(
    served: Option<&ServedStreamPipeline>,
    request_hdr10: bool,
) -> bool {
    request_hdr10 || served_pipeline_wants_display_hdr(served)
}

/// Codec selection policy for a pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecPolicy {
    /// Try codecs in this order, filtered by client and host capability.
    Ladder(&'static [VideoCodec]),
    /// Preserve the codec in the request.
    AsRequested,
}

/// Facts outside the final colour shape that affect served-pipeline truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServedPipelineContext {
    /// `None` means the host could not establish whether the initialized
    /// encoder was hardware or software; classify conservatively as custom.
    pub backend: Option<AcceleratorClass>,
    pub exact_or_admin_override: bool,
}

/// Contract used for host-side operation after served truth is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationalPipeline {
    /// A named product/software contract supplies bitrate, priority and intent.
    Contract(PipelineId),
    /// Exact/custom/admin streams keep the legacy shape-derived bitrate bounds
    /// and the caller's existing priority/intent.
    Legacy,
}

impl CodecPolicy {
    #[must_use]
    pub const fn ladder(self) -> &'static [VideoCodec] {
        match self {
            Self::Ladder(ladder) => ladder,
            Self::AsRequested => &[],
        }
    }
}

/// Starting and ceiling bitrate policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BitratePolicy {
    /// Today's hardware-host behavior: start capped by link-proven budget,
    /// ceiling from the uncapped shape formula.
    LinkCappedAverage,
    /// Start at today's link-proven budget but let the shared rate controller
    /// climb to a fixed fidelity ceiling.
    LinkCappedWithCeiling { ceiling_bps: u32 },
    /// Speed keeps the proven safe start, but its clean-path ceiling is the
    /// true requested 60 fps shape budget rather than another 30 fps cap.
    MotionCappedStartShapeCeiling,
    /// CPU software H.264: start conservatively and climb only when the
    /// transport proves the VM can spend more encode work usefully.
    SoftwareCpuH264 {
        start_1080p30_bps: u32,
        ceiling_1080p30_bps: u32,
        floor_bps: u32,
    },
    /// Fixed compatibility policy. Prefer a pipeline-owned policy for new use.
    Fixed { start_bps: u32, ceiling_bps: u32 },
    /// Windows software fallback inherits the same session rate-control bounds
    /// as the resolved Auto request today.
    SessionBounds,
}

impl BitratePolicy {
    #[must_use]
    pub fn bounds(
        self,
        width: u32,
        height: u32,
        fps: u32,
        chroma: ChromaSubsampling,
        depth: BitDepth,
    ) -> (u32, u32) {
        match self {
            Self::LinkCappedAverage | Self::MotionCappedStartShapeCeiling | Self::SessionBounds => {
                (
                    link_capped_average_bitrate_bps(width, height, fps, chroma, depth),
                    average_bitrate_bps(width, height, fps, chroma, depth),
                )
            }
            Self::SoftwareCpuH264 {
                start_1080p30_bps,
                ceiling_1080p30_bps,
                floor_bps,
            } => {
                let shape = average_bitrate_bps(width, height, fps, chroma, depth);
                let reference =
                    average_bitrate_bps(1920, 1080, 30, ChromaSubsampling::Yuv420, BitDepth::Eight)
                        .max(1);
                let scaled = |bps: u32| -> u32 {
                    let value = u64::from(bps)
                        .saturating_mul(u64::from(shape))
                        .div_ceil(u64::from(reference));
                    u32::try_from(value).unwrap_or(u32::MAX)
                };
                let start = shape.min(scaled(start_1080p30_bps)).max(floor_bps);
                let ceiling = scaled(ceiling_1080p30_bps).max(start).max(floor_bps);
                (start, ceiling)
            }
            Self::LinkCappedWithCeiling { ceiling_bps } => (
                link_capped_average_bitrate_bps(width, height, fps, chroma, depth),
                ceiling_bps.max(1),
            ),
            Self::Fixed {
                start_bps,
                ceiling_bps,
            } => (start_bps, ceiling_bps),
        }
    }

    #[must_use]
    pub fn encoder_start_and_max(
        self,
        width: u32,
        height: u32,
        fps: u32,
        chroma: ChromaSubsampling,
        depth: BitDepth,
    ) -> (u32, u32) {
        let (start, ceiling) = self.bounds(width, height, fps, chroma, depth);
        match self {
            Self::LinkCappedWithCeiling { .. } => (start, ceiling),
            Self::LinkCappedAverage
            | Self::MotionCappedStartShapeCeiling
            | Self::SessionBounds
            | Self::SoftwareCpuH264 { .. }
            | Self::Fixed { .. } => (start, start),
        }
    }

    #[must_use]
    pub const fn encoder_ceiling_bps(self) -> Option<u32> {
        match self {
            Self::LinkCappedWithCeiling { ceiling_bps } => Some(ceiling_bps),
            Self::LinkCappedAverage
            | Self::MotionCappedStartShapeCeiling
            | Self::Fixed { .. }
            | Self::SoftwareCpuH264 { .. }
            | Self::SessionBounds => None,
        }
    }
}

/// Shared keel policy surfaced above capenc-local QP-map enums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeelPolicy {
    pub idle_cadence_required: bool,
    pub qp_map_default: PipelineQpMapDefault,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PipelineQpMapDefault {
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QueuePolicy {
    pub host_video_frames: usize,
    pub host_writer_messages: usize,
    pub deck_video_packets: usize,
    pub deck_video_bytes: usize,
    pub raw_overflow: RawFrameOverflowPolicy,
    pub encoded_overflow: EncodedFrameOverflowPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawFrameOverflowPolicy {
    pub disposition: RawFrameOverflowDisposition,
    pub requires_idr: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawFrameOverflowDisposition {
    LatestWins,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncodedFrameOverflowPolicy {
    pub disposition: EncodedFrameOverflowDisposition,
    pub requires_idr: bool,
    pub keyframe_request_min_interval: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncodedFrameOverflowDisposition {
    ClearPredictionChain,
}

/// Software-only disposition for a requested product pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoftwareFallbackDecision {
    /// The request is already the software/Auto shape and may be served.
    Serve { reason: &'static str },
    /// The request may run, but the host must report the served Software pipeline.
    Degrade { reason: &'static str },
    /// The request requires a fidelity pipeline the software backend cannot truthfully serve.
    Refuse { reason: &'static str },
}

impl SoftwareFallbackDecision {
    #[must_use]
    pub const fn reason(self) -> &'static str {
        match self {
            Self::Serve { reason } | Self::Degrade { reason } | Self::Refuse { reason } => reason,
        }
    }

    #[must_use]
    pub const fn is_refusal(self) -> bool {
        matches!(self, Self::Refuse { .. })
    }
}

/// Decide how a software-only host handles a product pipeline request.
#[must_use]
pub fn software_fallback_decision(
    requested: Option<PipelineId>,
    selection: VideoSelectionIntent,
) -> SoftwareFallbackDecision {
    software::fallback_decision(requested, selection)
}

/// Encoder keyframe cadence policy for a served pipeline.
///
/// Recovery is always explicit: a Deck requests `request_full_frame` after
/// loss or decode failure, and host adapters translate that to a forced IDR.
/// `safety_refresh_interval` is therefore not a recovery mechanism for normal
/// lossy transport; it is only a bounded complete-frame refresh for paths that
/// intentionally suppress static regions and need a finite baseline deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyframePolicy {
    /// Whether this pipeline relies on host-forced IDRs for client recovery.
    pub on_demand: bool,
    /// Optional periodic safety refresh. `None` means no encoder-scheduled
    /// keyframes after startup.
    pub safety_refresh_interval: Option<Duration>,
}

impl KeyframePolicy {
    pub const ON_DEMAND_ONLY: Self = Self {
        on_demand: true,
        safety_refresh_interval: None,
    };

    pub const SOFTWARE_FALLBACK: Self = Self {
        on_demand: true,
        safety_refresh_interval: Some(Duration::from_secs(10)),
    };

    /// Period in frames for encoders that accept a fixed GOP length. `0` means
    /// the backend should disable scheduled keyframes or use its infinite-GOP
    /// sentinel when one exists.
    #[must_use]
    pub fn scheduled_period_frames(self, fps: u32) -> u32 {
        self.safety_refresh_interval.map_or(0, |interval| {
            u32::try_from(interval.as_secs().saturating_mul(u64::from(fps.max(1))))
                .unwrap_or(u32::MAX)
        })
    }

    #[must_use]
    pub const fn safety_refresh_interval(self) -> Option<Duration> {
        self.safety_refresh_interval
    }
}

/// Decoder-side latency policy a Deck can derive from the served pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodeLatencyPolicy {
    pub realtime: bool,
}

impl DecodeLatencyPolicy {
    pub const DEFAULT: Self = Self { realtime: true };

    pub const SPEED: Self = Self { realtime: true };
}

impl Default for DecodeLatencyPolicy {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Complete portable contract for one stream pipeline.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PipelineContract {
    pub id: PipelineId,
    pub max_fps: u32,
    pub priority: MotionPriority,
    pub intent: EncodeIntent,
    pub encoder_buffer_frames: f64,
    pub selection: VideoSelectionIntent,
    pub colour: VideoConfiguration,
    pub codec_policy: CodecPolicy,
    pub bitrate: BitratePolicy,
    pub probe_step: ProbeStep,
    pub keel: KeelPolicy,
    pub queue: QueuePolicy,
    pub decode_latency: DecodeLatencyPolicy,
    pub keyframe: KeyframePolicy,
    pub favours: &'static str,
    pub gives_up: &'static str,
    pub summary: &'static str,
}

impl PipelineContract {
    #[must_use]
    pub fn bitrate_bounds(
        self,
        width: u32,
        height: u32,
        fps: u32,
        chroma: ChromaSubsampling,
        depth: BitDepth,
    ) -> (u32, u32) {
        self.bitrate.bounds(width, height, fps, chroma, depth)
    }

    #[must_use]
    pub fn encoder_bitrate_bounds(
        self,
        width: u32,
        height: u32,
        fps: u32,
        chroma: ChromaSubsampling,
        depth: BitDepth,
    ) -> (u32, u32) {
        self.bitrate
            .encoder_start_and_max(width, height, fps, chroma, depth)
    }

    #[must_use]
    pub const fn encoder_ceiling_bps(self) -> Option<u32> {
        self.bitrate.encoder_ceiling_bps()
    }

    #[must_use]
    pub fn rate_control_policy(
        self,
        width: u32,
        height: u32,
        fps: u32,
        chroma: ChromaSubsampling,
        depth: BitDepth,
    ) -> RateControlPolicy {
        let (start, ceiling) = self.bitrate_bounds(width, height, fps, chroma, depth);
        RateControlPolicy::for_bounds_priority_probe_step(
            u64::from(start),
            u64::from(ceiling),
            self.priority,
            self.probe_step,
        )
    }
}

pub(crate) const AUTO_SPEED_LADDER: [VideoCodec; 3] =
    [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264];
const AUTO_SPEED_FROM_HEVC: [VideoCodec; 2] = [VideoCodec::H265, VideoCodec::H264];
const AUTO_SPEED_FROM_H264: [VideoCodec; 1] = [VideoCodec::H264];
const REQUESTED_JPEG: [VideoCodec; 1] = [VideoCodec::Jpeg];
const REQUESTED_VP9: [VideoCodec; 1] = [VideoCodec::Vp9];
pub(crate) const HEVC_FIDELITY_LADDER: [VideoCodec; 2] = [VideoCodec::H265, VideoCodec::H264];
pub(crate) const SOFTWARE_LADDER: [VideoCodec; 1] = [VideoCodec::H264];
pub(crate) const KEEL_IDLE_QP_OFF: KeelPolicy = KeelPolicy {
    idle_cadence_required: true,
    qp_map_default: PipelineQpMapDefault::Off,
};
/// Deck inbox depth for encoded frames, sized by time rather than by the
/// congestion-window bursts it was first tuned for. A Wi-Fi or VPN stall of a
/// few hundred milliseconds releases every frame produced meanwhile in one
/// burst; a decoder drains that burst in tens of milliseconds and the pacer
/// shows the newest picture. An inbox smaller than the burst instead discards
/// the whole prediction chain and asks for a keyframe, and the large keyframe
/// makes the next burst bigger: eight packets (133 ms at 60 fps) turned
/// 250-300 ms stalls into a five-second keyframe loop on a real Deck.
pub const DECK_BURST_ABSORB: Duration = Duration::from_millis(500);
pub(crate) const DECK_VIDEO_PACKETS: usize = 30;

pub(crate) const DEFAULT_QUEUE_POLICY: QueuePolicy = QueuePolicy {
    host_video_frames: 4,
    host_writer_messages: 2,
    deck_video_packets: DECK_VIDEO_PACKETS,
    deck_video_bytes: 64 * 1024 * 1024,
    raw_overflow: RawFrameOverflowPolicy {
        disposition: RawFrameOverflowDisposition::LatestWins,
        requires_idr: false,
    },
    encoded_overflow: EncodedFrameOverflowPolicy {
        disposition: EncodedFrameOverflowDisposition::ClearPredictionChain,
        requires_idr: true,
        keyframe_request_min_interval: Duration::from_secs(1),
    },
};
pub(crate) const HDR_QUEUE_POLICY: QueuePolicy = QueuePolicy {
    host_video_frames: 8,
    host_writer_messages: 4,
    deck_video_packets: DECK_VIDEO_PACKETS,
    deck_video_bytes: 128 * 1024 * 1024,
    raw_overflow: RawFrameOverflowPolicy {
        disposition: RawFrameOverflowDisposition::LatestWins,
        requires_idr: false,
    },
    encoded_overflow: EncodedFrameOverflowPolicy {
        disposition: EncodedFrameOverflowDisposition::ClearPredictionChain,
        requires_idr: true,
        keyframe_request_min_interval: Duration::from_secs(1),
    },
};

pub(crate) const fn sdr_420(codec: VideoCodec) -> VideoConfiguration {
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

pub(crate) const fn grading_reference() -> VideoConfiguration {
    VideoConfiguration {
        codec: VideoCodec::H265,
        chroma: ChromaSubsampling::Yuv444,
        bit_depth: BitDepth::Ten,
        range: ColorRange::Full,
        matrix: ColorMatrix::Bt709,
        primaries: ColorPrimaries::Bt709,
        transfer: TransferCharacteristics::Bt709,
    }
}

pub(crate) const fn hdr10() -> VideoConfiguration {
    VideoConfiguration {
        codec: VideoCodec::H265,
        chroma: ChromaSubsampling::Yuv444,
        bit_depth: BitDepth::Ten,
        range: ColorRange::Full,
        matrix: ColorMatrix::Bt2020Ncl,
        primaries: ColorPrimaries::Bt2020,
        transfer: TransferCharacteristics::Pq,
    }
}

pub(crate) const fn buffer(priority: MotionPriority, intent: EncodeIntent) -> f64 {
    encoder_buffer_frames(priority, intent)
}

/// Returns the contract for a known pipeline.
#[must_use]
pub const fn pipeline_contract(id: PipelineId) -> PipelineContract {
    match id {
        PipelineId::Auto => auto::CONTRACT,
        PipelineId::Speed => speed::CONTRACT,
        PipelineId::Grading => grading::CONTRACT,
        PipelineId::Hdr => hdr::CONTRACT,
        PipelineId::Software => software::CONTRACT,
    }
}

/// Infer the four named pipelines for legacy requests that lack `pipeline`.
///
/// Exact requests are developer/custom axes and intentionally stay outside the
/// named product pipelines.
#[must_use]
pub fn pipeline_for_request(
    selection: VideoSelectionIntent,
    video: VideoConfiguration,
    max_fps: u32,
    priority: MotionPriority,
) -> Option<PipelineId> {
    match selection {
        VideoSelectionIntent::AdaptivePerformance => {
            if max_fps > 30 || priority == MotionPriority::Motion {
                Some(PipelineId::Speed)
            } else {
                Some(PipelineId::Auto)
            }
        }
        VideoSelectionIntent::ColorFidelity => {
            if matches!(
                video.transfer,
                TransferCharacteristics::Pq | TransferCharacteristics::Hlg
            ) {
                Some(PipelineId::Hdr)
            } else {
                Some(PipelineId::Grading)
            }
        }
        VideoSelectionIntent::Exact => None,
    }
}

/// Resolve an optional wire pipeline, falling back to legacy inference.
#[must_use]
pub fn resolve_request_pipeline(
    wire: Option<&StreamPipeline>,
    selection: VideoSelectionIntent,
    video: VideoConfiguration,
    max_fps: u32,
    priority: MotionPriority,
) -> Option<PipelineId> {
    wire.and_then(PipelineId::from_wire)
        .or_else(|| pipeline_for_request(selection, video, max_fps, priority))
}

/// Derive the pipeline the host actually served from the final video contract.
///
/// The input must be after administrator pins, host codec fallback, desktop
/// encoding rules, and HDR proof have been applied. Exact/custom requests that
/// do not land on one of the product fidelity contracts return `None`.
#[must_use]
pub fn served_pipeline(
    requested: Option<PipelineId>,
    final_video: VideoConfiguration,
    max_fps: u32,
    priority: MotionPriority,
    context: ServedPipelineContext,
) -> ServedStreamPipeline {
    let Some(backend) = context.backend else {
        return ServedStreamPipeline::Custom;
    };
    if backend == AcceleratorClass::Software {
        return ServedStreamPipeline::Software;
    }
    if let Some(requested) = requested
        && !context.exact_or_admin_override
        && product_contract_matches(requested, final_video, max_fps, priority)
    {
        return requested.served_wire();
    }
    if context.exact_or_admin_override {
        return ServedStreamPipeline::Custom;
    }
    for pipeline in [
        PipelineId::Auto,
        PipelineId::Speed,
        PipelineId::Grading,
        PipelineId::Hdr,
    ] {
        if product_contract_matches(pipeline, final_video, max_fps, priority) {
            return pipeline.served_wire();
        }
    }
    ServedStreamPipeline::Custom
}

/// Select the operational contract from the host's served truth.
#[must_use]
pub fn operational_pipeline(served: Option<&ServedStreamPipeline>) -> OperationalPipeline {
    match served {
        Some(ServedStreamPipeline::Auto) => OperationalPipeline::Contract(PipelineId::Auto),
        Some(ServedStreamPipeline::Speed) => OperationalPipeline::Contract(PipelineId::Speed),
        Some(ServedStreamPipeline::Grading) => OperationalPipeline::Contract(PipelineId::Grading),
        Some(ServedStreamPipeline::Hdr) => OperationalPipeline::Contract(PipelineId::Hdr),
        Some(ServedStreamPipeline::Software) => OperationalPipeline::Contract(PipelineId::Software),
        Some(ServedStreamPipeline::Custom | ServedStreamPipeline::Unknown(_)) | None => {
            OperationalPipeline::Legacy
        }
    }
}

/// Bitrate bounds for the contract the host is actually serving.
#[must_use]
pub fn operational_bitrate_bounds(
    served: Option<&ServedStreamPipeline>,
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaSubsampling,
    depth: BitDepth,
) -> (u32, u32) {
    match operational_pipeline(served) {
        OperationalPipeline::Contract(pipeline) => {
            pipeline_contract(pipeline).bitrate_bounds(width, height, fps, chroma, depth)
        }
        OperationalPipeline::Legacy => (
            link_capped_average_bitrate_bps(width, height, fps, chroma, depth),
            average_bitrate_bps(width, height, fps, chroma, depth),
        ),
    }
}

/// Native encoder average/max bounds for the served operational contract.
#[must_use]
pub fn operational_encoder_bitrate_bounds(
    served: Option<&ServedStreamPipeline>,
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaSubsampling,
    depth: BitDepth,
) -> (u32, u32) {
    match operational_pipeline(served) {
        OperationalPipeline::Contract(pipeline) => {
            pipeline_contract(pipeline).encoder_bitrate_bounds(width, height, fps, chroma, depth)
        }
        OperationalPipeline::Legacy => {
            let start = link_capped_average_bitrate_bps(width, height, fps, chroma, depth);
            (start, start)
        }
    }
}

/// Clamp a live bitrate target to one encoder region's operational ceiling.
///
/// Session rate controllers operate on the served stream contract. A
/// multi-region adapter pushes one session target into each region, but each
/// region's capture geometry may have a different contract ceiling.
#[must_use]
pub const fn clamp_live_bitrate_target(target_bps: u64, live_max_bps: u64) -> u64 {
    if target_bps > live_max_bps {
        live_max_bps
    } else {
        target_bps
    }
}

/// Complete rate-control policy for the served operational contract.
#[must_use]
pub fn operational_rate_control_policy(
    served: Option<&ServedStreamPipeline>,
    width: u32,
    height: u32,
    fps: u32,
    chroma: ChromaSubsampling,
    depth: BitDepth,
    fallback_priority: MotionPriority,
) -> RateControlPolicy {
    match operational_pipeline(served) {
        OperationalPipeline::Contract(pipeline) => {
            pipeline_contract(pipeline).rate_control_policy(width, height, fps, chroma, depth)
        }
        OperationalPipeline::Legacy => {
            let (start, ceiling) =
                operational_bitrate_bounds(served, width, height, fps, chroma, depth);
            RateControlPolicy::for_bounds_and_priority(
                u64::from(start),
                u64::from(ceiling),
                fallback_priority,
            )
        }
    }
}

/// Motion priority for the contract the host is actually serving.
#[must_use]
pub fn operational_motion_priority(
    served: Option<&ServedStreamPipeline>,
    fallback: MotionPriority,
) -> MotionPriority {
    match operational_pipeline(served) {
        OperationalPipeline::Contract(pipeline) => pipeline_contract(pipeline).priority,
        OperationalPipeline::Legacy => fallback,
    }
}

/// Encode intent for the contract the host is actually serving.
#[must_use]
pub fn operational_encode_intent(
    served: Option<&ServedStreamPipeline>,
    fallback: EncodeIntent,
) -> EncodeIntent {
    match operational_pipeline(served) {
        OperationalPipeline::Contract(pipeline) => pipeline_contract(pipeline).intent,
        OperationalPipeline::Legacy => fallback,
    }
}

/// Keyframe policy for the contract the host is actually serving.
#[must_use]
pub fn operational_keyframe_policy(served: Option<&ServedStreamPipeline>) -> KeyframePolicy {
    match operational_pipeline(served) {
        OperationalPipeline::Contract(pipeline) => pipeline_contract(pipeline).keyframe,
        OperationalPipeline::Legacy => KeyframePolicy::ON_DEMAND_ONLY,
    }
}

/// Classify a multi-encoder session from the acceleration read-back of every
/// initialized region encoder.
///
/// The aggregate is deliberately conservative: all hardware lets the normal
/// served-pipeline resolver name the product contract, any software encoder
/// makes the whole session a software fallback, and any unknown answer (with no
/// software encoder present) keeps the session custom because the host cannot
/// prove what it served.
#[must_use]
pub fn aggregate_encoder_backend(
    backends: impl IntoIterator<Item = Option<AcceleratorClass>>,
) -> Option<AcceleratorClass> {
    let mut saw_encoder = false;
    let mut saw_unknown = false;
    for backend in backends {
        saw_encoder = true;
        match backend {
            Some(AcceleratorClass::Hardware) => {}
            Some(AcceleratorClass::Software) => return Some(AcceleratorClass::Software),
            None => saw_unknown = true,
        }
    }
    if saw_encoder && !saw_unknown {
        Some(AcceleratorClass::Hardware)
    } else {
        None
    }
}

/// Classify a multi-encoder session from each resolved plan's encoder
/// acceleration.
///
/// This is the plan-level adapter for hosts whose native READY objects already
/// carry [`AcceleratorClass`]. It keeps the conservative aggregation rule in
/// one shared place instead of making every host copy the same iterator shape.
#[must_use]
pub fn aggregate_plan_encoder_backend<'a>(
    plans: impl IntoIterator<Item = &'a super::ResolvedMediaPlan>,
) -> Option<AcceleratorClass> {
    aggregate_encoder_backend(
        plans
            .into_iter()
            .map(|plan| Some(plan.backend.accelerator_class())),
    )
}

#[must_use]
fn product_contract_matches(
    pipeline: PipelineId,
    final_video: VideoConfiguration,
    max_fps: u32,
    priority: MotionPriority,
) -> bool {
    let contract = pipeline_contract(pipeline);
    let fps_matches = match pipeline {
        PipelineId::Speed => max_fps > 30 || priority == MotionPriority::Motion,
        PipelineId::Auto => max_fps <= 30 && priority == MotionPriority::Detail,
        PipelineId::Grading | PipelineId::Hdr => {
            max_fps <= contract.max_fps && priority == contract.priority
        }
        PipelineId::Software => false,
    };
    fps_matches
        && final_video.chroma == contract.colour.chroma
        && final_video.bit_depth == contract.colour.bit_depth
        && final_video.range == contract.colour.range
        && final_video.matrix == contract.colour.matrix
        && final_video.primaries == contract.colour.primaries
        && final_video.transfer == contract.colour.transfer
        && codec_policy_allows(contract.codec_policy, final_video.codec)
}

#[must_use]
fn codec_policy_allows(policy: CodecPolicy, codec: VideoCodec) -> bool {
    match policy {
        CodecPolicy::Ladder(ladder) => ladder.contains(&codec),
        CodecPolicy::AsRequested => true,
    }
}

/// Ordered adaptive codec ladder for the pipeline contract.
#[must_use]
pub fn pipeline_codec_ladder(id: PipelineId, requested: VideoCodec) -> &'static [VideoCodec] {
    let policy = pipeline_contract(id).codec_policy;
    match policy {
        CodecPolicy::Ladder(ladder) => {
            if ladder.len() == AUTO_SPEED_LADDER.len()
                && ladder[0] == VideoCodec::Av1
                && ladder[1] == VideoCodec::H265
                && ladder[2] == VideoCodec::H264
            {
                match requested {
                    VideoCodec::Av1 => &AUTO_SPEED_LADDER,
                    VideoCodec::H265 => &AUTO_SPEED_FROM_HEVC,
                    VideoCodec::H264 => &AUTO_SPEED_FROM_H264,
                    VideoCodec::Jpeg | VideoCodec::Vp9 => &[],
                }
            } else {
                ladder
            }
        }
        CodecPolicy::AsRequested => match requested {
            VideoCodec::Av1 => &[VideoCodec::Av1],
            VideoCodec::H265 => &[VideoCodec::H265],
            VideoCodec::H264 => &[VideoCodec::H264],
            VideoCodec::Jpeg => &REQUESTED_JPEG,
            VideoCodec::Vp9 => &REQUESTED_VP9,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(id: PipelineId, width: u32, height: u32, fps: u32) -> (u32, u32) {
        let c = pipeline_contract(id);
        c.bitrate_bounds(width, height, fps, c.colour.chroma, c.colour.bit_depth)
    }

    #[test]
    fn legacy_requests_infer_product_pipelines() {
        let auto = sdr_420(VideoCodec::H264);
        assert_eq!(
            pipeline_for_request(
                VideoSelectionIntent::AdaptivePerformance,
                auto,
                30,
                MotionPriority::Detail
            ),
            Some(PipelineId::Auto)
        );
        assert_eq!(
            pipeline_for_request(
                VideoSelectionIntent::AdaptivePerformance,
                auto,
                60,
                MotionPriority::Detail
            ),
            Some(PipelineId::Speed)
        );
        assert_eq!(
            pipeline_for_request(
                VideoSelectionIntent::ColorFidelity,
                grading_reference(),
                30,
                MotionPriority::Detail
            ),
            Some(PipelineId::Grading)
        );
        assert_eq!(
            pipeline_for_request(
                VideoSelectionIntent::ColorFidelity,
                hdr10(),
                30,
                MotionPriority::Detail
            ),
            Some(PipelineId::Hdr)
        );
        assert_eq!(
            pipeline_for_request(
                VideoSelectionIntent::Exact,
                auto,
                60,
                MotionPriority::Motion
            ),
            None
        );
    }

    #[test]
    fn pipeline_keyframe_policies_are_explicit_recovery_contracts() {
        for id in [
            PipelineId::Auto,
            PipelineId::Speed,
            PipelineId::Grading,
            PipelineId::Hdr,
        ] {
            let policy = pipeline_contract(id).keyframe;
            assert!(
                policy.on_demand,
                "{id:?} must keep client-requested recovery"
            );
            assert_eq!(policy.safety_refresh_interval(), None, "{id:?}");
            assert_eq!(policy.scheduled_period_frames(60), 0, "{id:?}");
        }

        let software = pipeline_contract(PipelineId::Software).keyframe;
        assert!(software.on_demand);
        assert_eq!(
            software.safety_refresh_interval(),
            Some(Duration::from_secs(10))
        );
        assert_eq!(software.scheduled_period_frames(30), 300);
        assert_eq!(software.scheduled_period_frames(0), 10);
    }

    #[test]
    fn pipeline_contracts_pin_today_values() {
        let auto = pipeline_contract(PipelineId::Auto);
        assert_eq!(auto.max_fps, 30);
        assert_eq!(auto.priority, MotionPriority::Detail);
        assert_eq!(auto.intent, EncodeIntent::Interactive);
        assert_eq!(auto.codec_policy.ladder(), AUTO_SPEED_LADDER);
        assert_eq!(auto.encoder_buffer_frames, 2.0);

        let speed = pipeline_contract(PipelineId::Speed);
        assert_eq!(speed.max_fps, 60);
        assert_eq!(speed.priority, MotionPriority::Motion);
        assert_eq!(speed.encoder_buffer_frames, 1.0);

        for id in [PipelineId::Grading, PipelineId::Hdr] {
            let contract = pipeline_contract(id);
            assert_eq!(contract.max_fps, 30);
            assert_eq!(contract.intent, EncodeIntent::Quality);
            assert_eq!(contract.encoder_buffer_frames, 8.0);
            assert_eq!(contract.codec_policy.ladder(), HEVC_FIDELITY_LADDER);
        }
    }

    #[test]
    fn served_pipeline_uses_final_video_truth() {
        assert_eq!(
            served_pipeline(
                Some(PipelineId::Hdr),
                grading_reference(),
                30,
                MotionPriority::Detail,
                hardware_context()
            ),
            ServedStreamPipeline::Grading,
            "an HDR request served as BT.709 SDR is Grading"
        );
        assert_eq!(
            served_pipeline(
                None,
                grading_reference(),
                30,
                MotionPriority::Detail,
                hardware_context()
            ),
            ServedStreamPipeline::Grading,
            "a final stream that exactly matches a product contract names it"
        );
        assert_eq!(
            served_pipeline(
                Some(PipelineId::Speed),
                sdr_420(VideoCodec::H264),
                60,
                MotionPriority::Motion,
                hardware_context()
            ),
            ServedStreamPipeline::Speed
        );
        assert_eq!(
            served_pipeline(
                Some(PipelineId::Speed),
                sdr_420(VideoCodec::H264),
                30,
                MotionPriority::Detail,
                hardware_context()
            ),
            ServedStreamPipeline::Auto
        );
        assert_eq!(
            served_pipeline(
                Some(PipelineId::Grading),
                sdr_420(VideoCodec::H265),
                30,
                MotionPriority::Detail,
                hardware_context()
            ),
            ServedStreamPipeline::Auto,
            "a fidelity request served as an ordinary 8-bit HEVC stream says Auto"
        );
        for codec in [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264] {
            assert_eq!(
                served_pipeline(
                    Some(PipelineId::Auto),
                    sdr_420(codec),
                    30,
                    MotionPriority::Detail,
                    hardware_context()
                ),
                ServedStreamPipeline::Auto,
                "Auto must allow {codec:?}"
            );
            assert_eq!(
                served_pipeline(
                    Some(PipelineId::Speed),
                    sdr_420(codec),
                    60,
                    MotionPriority::Motion,
                    hardware_context()
                ),
                ServedStreamPipeline::Speed,
                "Speed must allow {codec:?}"
            );
        }
        assert_eq!(
            served_pipeline(
                Some(PipelineId::Speed),
                sdr_420(VideoCodec::H264),
                30,
                MotionPriority::Motion,
                software_context()
            ),
            ServedStreamPipeline::Software,
            "software backend truth wins over Speed's retained motion priority"
        );
        let identity_grading = VideoConfiguration {
            matrix: ColorMatrix::Identity,
            ..grading_reference()
        };
        assert_eq!(
            served_pipeline(
                Some(PipelineId::Grading),
                identity_grading,
                30,
                MotionPriority::Detail,
                custom_context()
            ),
            ServedStreamPipeline::Custom,
            "administrator exact/variant pins stay outside product contracts"
        );
        assert_eq!(
            served_pipeline(
                Some(PipelineId::Auto),
                sdr_420(VideoCodec::H264),
                30,
                MotionPriority::Detail,
                ServedPipelineContext {
                    backend: None,
                    exact_or_admin_override: false,
                },
            ),
            ServedStreamPipeline::Custom,
            "unknown encoder class is conservative"
        );
    }

    #[test]
    fn multi_encoder_backend_aggregation_is_conservative() {
        assert_eq!(
            aggregate_encoder_backend([
                Some(AcceleratorClass::Hardware),
                Some(AcceleratorClass::Hardware),
            ]),
            Some(AcceleratorClass::Hardware),
            "all hardware preserves the resolver's product result"
        );
        assert_eq!(
            aggregate_encoder_backend([
                Some(AcceleratorClass::Hardware),
                Some(AcceleratorClass::Software),
                None,
            ]),
            Some(AcceleratorClass::Software),
            "any software region makes the session software"
        );
        assert_eq!(
            aggregate_encoder_backend([Some(AcceleratorClass::Hardware), None]),
            None,
            "unknown acceleration without software is custom"
        );
        assert_eq!(
            aggregate_encoder_backend([None, None]),
            None,
            "all unknown acceleration is custom"
        );
    }

    #[test]
    fn live_bitrate_target_clamps_to_operational_region_ceiling() {
        assert_eq!(clamp_live_bitrate_target(5_000_000, 8_000_000), 5_000_000);
        assert_eq!(clamp_live_bitrate_target(12_000_000, 8_000_000), 8_000_000);
    }

    #[test]
    fn multi_encoder_unknown_and_software_results_drive_served_truth() {
        let final_video = sdr_420(VideoCodec::H264);
        let requested = Some(PipelineId::Speed);
        assert_eq!(
            served_pipeline(
                requested,
                final_video,
                60,
                MotionPriority::Motion,
                ServedPipelineContext {
                    backend: aggregate_encoder_backend([
                        Some(AcceleratorClass::Hardware),
                        Some(AcceleratorClass::Hardware),
                    ]),
                    exact_or_admin_override: false,
                },
            ),
            ServedStreamPipeline::Speed
        );
        assert_eq!(
            served_pipeline(
                requested,
                final_video,
                60,
                MotionPriority::Motion,
                ServedPipelineContext {
                    backend: aggregate_encoder_backend([
                        Some(AcceleratorClass::Hardware),
                        Some(AcceleratorClass::Software),
                    ]),
                    exact_or_admin_override: false,
                },
            ),
            ServedStreamPipeline::Software
        );
        assert_eq!(
            served_pipeline(
                requested,
                final_video,
                60,
                MotionPriority::Motion,
                ServedPipelineContext {
                    backend: aggregate_encoder_backend([Some(AcceleratorClass::Hardware), None]),
                    exact_or_admin_override: false,
                },
            ),
            ServedStreamPipeline::Custom
        );
    }

    #[test]
    fn operational_contract_uses_served_pipeline_not_requested_pipeline() {
        let shape = (3840, 2160, 30, ChromaSubsampling::Yuv444, BitDepth::Ten);
        assert_eq!(
            operational_bitrate_bounds(
                Some(&ServedStreamPipeline::Grading),
                shape.0,
                shape.1,
                shape.2,
                shape.3,
                shape.4,
            ),
            pipeline_contract(PipelineId::Grading)
                .bitrate_bounds(shape.0, shape.1, shape.2, shape.3, shape.4),
            "an HDR request served as Grading must operate as Grading"
        );
        assert_eq!(
            operational_motion_priority(
                Some(&ServedStreamPipeline::Grading),
                MotionPriority::Motion
            ),
            pipeline_contract(PipelineId::Grading).priority
        );
        assert_eq!(
            operational_encode_intent(
                Some(&ServedStreamPipeline::Grading),
                EncodeIntent::Interactive
            ),
            pipeline_contract(PipelineId::Grading).intent
        );
    }

    #[test]
    fn operational_contract_keeps_legacy_bounds_for_custom_exact_streams() {
        let shape = (1920, 1080, 60, ChromaSubsampling::Yuv420, BitDepth::Eight);
        assert_eq!(
            operational_bitrate_bounds(
                Some(&ServedStreamPipeline::Custom),
                shape.0,
                shape.1,
                shape.2,
                shape.3,
                shape.4,
            ),
            (
                link_capped_average_bitrate_bps(shape.0, shape.1, shape.2, shape.3, shape.4),
                average_bitrate_bps(shape.0, shape.1, shape.2, shape.3, shape.4),
            ),
            "custom/exact sessions keep today's legacy shape-derived bounds"
        );
        assert_eq!(
            operational_motion_priority(
                Some(&ServedStreamPipeline::Custom),
                MotionPriority::Motion
            ),
            MotionPriority::Motion
        );
        assert_eq!(
            operational_encode_intent(
                Some(&ServedStreamPipeline::Custom),
                EncodeIntent::Interactive
            ),
            EncodeIntent::Interactive
        );
    }

    #[test]
    fn operational_contract_uses_software_contract_for_software_truth() {
        let shape = (1920, 1080, 30, ChromaSubsampling::Yuv420, BitDepth::Eight);
        assert_eq!(
            operational_bitrate_bounds(
                Some(&ServedStreamPipeline::Software),
                shape.0,
                shape.1,
                shape.2,
                shape.3,
                shape.4,
            ),
            pipeline_contract(PipelineId::Software)
                .bitrate_bounds(shape.0, shape.1, shape.2, shape.3, shape.4)
        );
        assert_eq!(
            operational_motion_priority(
                Some(&ServedStreamPipeline::Software),
                MotionPriority::Motion
            ),
            pipeline_contract(PipelineId::Software).priority
        );
        assert_eq!(
            operational_encode_intent(Some(&ServedStreamPipeline::Software), EncodeIntent::Quality),
            pipeline_contract(PipelineId::Software).intent
        );
    }

    #[test]
    fn bitrate_bounds_match_today_for_representative_shapes_except_fidelity_ceilings() {
        for id in [PipelineId::Auto, PipelineId::Speed] {
            let c = pipeline_contract(id);
            for (width, height, fps) in [(1800, 1168, 30), (1800, 1168, 60), (3840, 2160, 30)] {
                assert_eq!(
                    c.bitrate_bounds(width, height, fps, c.colour.chroma, c.colour.bit_depth),
                    (
                        link_capped_average_bitrate_bps(
                            width,
                            height,
                            fps,
                            c.colour.chroma,
                            c.colour.bit_depth
                        ),
                        average_bitrate_bps(
                            width,
                            height,
                            fps,
                            c.colour.chroma,
                            c.colour.bit_depth
                        )
                    ),
                    "{id:?} {width}x{height}@{fps}"
                );
            }
        }
        assert_eq!(
            bounds(PipelineId::Software, 1920, 1080, 30),
            (4_000_000, 8_000_000)
        );
        assert_eq!(
            bounds(PipelineId::Software, 1800, 1168, 30),
            (4_055_556, 8_111_112)
        );
        assert_eq!(
            bounds(PipelineId::Software, 1280, 720, 30),
            (1_777_778, 3_555_556)
        );
    }

    #[test]
    fn software_fallback_decision_serves_auto_degrades_speed_and_refuses_fidelity() {
        assert!(matches!(
            software_fallback_decision(
                Some(PipelineId::Auto),
                VideoSelectionIntent::AdaptivePerformance
            ),
            SoftwareFallbackDecision::Serve { .. }
        ));
        assert!(matches!(
            software_fallback_decision(
                Some(PipelineId::Speed),
                VideoSelectionIntent::AdaptivePerformance
            ),
            SoftwareFallbackDecision::Degrade { .. }
        ));
        assert!(matches!(
            software_fallback_decision(
                Some(PipelineId::Grading),
                VideoSelectionIntent::ColorFidelity
            ),
            SoftwareFallbackDecision::Refuse { .. }
        ));
        assert!(matches!(
            software_fallback_decision(Some(PipelineId::Hdr), VideoSelectionIntent::ColorFidelity),
            SoftwareFallbackDecision::Refuse { .. }
        ));
        assert!(matches!(
            software_fallback_decision(None, VideoSelectionIntent::ColorFidelity),
            SoftwareFallbackDecision::Refuse { .. }
        ));
    }

    #[test]
    fn software_fallback_refusal_reasons_fit_close_frames() {
        for decision in [
            software_fallback_decision(
                Some(PipelineId::Grading),
                VideoSelectionIntent::ColorFidelity,
            ),
            software_fallback_decision(Some(PipelineId::Hdr), VideoSelectionIntent::ColorFidelity),
            software_fallback_decision(None, VideoSelectionIntent::ColorFidelity),
        ] {
            assert!(matches!(decision, SoftwareFallbackDecision::Refuse { .. }));
            assert!(
                decision.reason().len() <= 120,
                "close_with_reason preserves software refusal: {}",
                decision.reason()
            );
        }
    }

    #[test]
    fn grading_keeps_today_start_and_uses_fixed_fidelity_ceiling() {
        let c = pipeline_contract(PipelineId::Grading);
        for (width, height, fps) in [(1800, 1168, 30), (1800, 1168, 60), (3840, 2160, 30)] {
            assert_eq!(
                c.bitrate_bounds(width, height, fps, c.colour.chroma, c.colour.bit_depth),
                (
                    link_capped_average_bitrate_bps(
                        width,
                        height,
                        fps,
                        c.colour.chroma,
                        c.colour.bit_depth
                    ),
                    250_000_000
                ),
                "Grading {width}x{height}@{fps}"
            );
        }
        assert_eq!(
            c.bitrate_bounds(1800, 1168, 30, c.colour.chroma, c.colour.bit_depth),
            (4_665_600, 250_000_000)
        );
    }

    #[test]
    fn speed_keeps_the_safe_start_but_ceilings_at_the_sixty_fps_shape() {
        let c = pipeline_contract(PipelineId::Speed);
        let (start, ceiling) =
            c.bitrate_bounds(1920, 1080, 60, c.colour.chroma, c.colour.bit_depth);
        assert_eq!(
            start,
            link_capped_average_bitrate_bps(1920, 1080, 60, c.colour.chroma, c.colour.bit_depth),
            "Speed still starts at the proven safe cap"
        );
        assert_eq!(
            start,
            average_bitrate_bps(1920, 1080, 30, c.colour.chroma, c.colour.bit_depth),
            "the safe start is still the 1080p30 bill"
        );
        assert_eq!(
            ceiling,
            average_bitrate_bps(1920, 1080, 60, c.colour.chroma, c.colour.bit_depth),
            "Speed's clean-path ceiling is the true 60 fps shape"
        );
        assert_eq!(ceiling, start * 2);
    }

    #[test]
    fn speed_contract_requests_decode_low_latency() {
        let speed = pipeline_contract(PipelineId::Speed);
        assert!(speed.decode_latency.realtime);
        assert_eq!(
            PipelineId::from_served_wire(&ServedStreamPipeline::Speed),
            Some(PipelineId::Speed)
        );
        assert_eq!(
            PipelineId::from_served_wire(&ServedStreamPipeline::Custom),
            None
        );
    }

    #[test]
    fn only_the_hdr_served_pipeline_wants_display_hdr() {
        for served in [
            ServedStreamPipeline::Auto,
            ServedStreamPipeline::Speed,
            ServedStreamPipeline::Grading,
            ServedStreamPipeline::Software,
            ServedStreamPipeline::Custom,
        ] {
            assert!(!served_pipeline_wants_display_hdr(Some(&served)));
        }
        assert!(served_pipeline_wants_display_hdr(Some(
            &ServedStreamPipeline::Hdr
        )));
        assert!(!served_pipeline_wants_display_hdr(None));
    }

    #[test]
    fn pq_requests_keep_display_hdr_even_for_custom_or_legacy_sessions() {
        assert!(session_wants_display_hdr(
            Some(&ServedStreamPipeline::Hdr),
            false
        ));
        assert!(session_wants_display_hdr(
            Some(&ServedStreamPipeline::Custom),
            true
        ));
        assert!(!session_wants_display_hdr(
            Some(&ServedStreamPipeline::Custom),
            false
        ));
        assert!(session_wants_display_hdr(None, true));
        for served in [
            ServedStreamPipeline::Auto,
            ServedStreamPipeline::Speed,
            ServedStreamPipeline::Grading,
            ServedStreamPipeline::Software,
        ] {
            assert!(!session_wants_display_hdr(Some(&served), false));
        }
    }

    #[test]
    fn encoder_max_bitrate_only_widens_for_fixed_fidelity_ceilings() {
        let auto = pipeline_contract(PipelineId::Auto);
        assert_eq!(auto.encoder_ceiling_bps(), None);
        let auto_start = link_capped_average_bitrate_bps(
            3840,
            2160,
            30,
            auto.colour.chroma,
            auto.colour.bit_depth,
        );
        assert_eq!(
            auto.encoder_bitrate_bounds(3840, 2160, 30, auto.colour.chroma, auto.colour.bit_depth),
            (auto_start, auto_start),
            "Auto encoder maxBitRate stays byte-for-byte compatible"
        );

        let hdr = pipeline_contract(PipelineId::Hdr);
        assert_eq!(hdr.encoder_ceiling_bps(), Some(500_000_000));
        let hdr_start = link_capped_average_bitrate_bps(
            3840,
            2160,
            30,
            hdr.colour.chroma,
            hdr.colour.bit_depth,
        );
        assert_eq!(
            hdr.encoder_bitrate_bounds(3840, 2160, 30, hdr.colour.chroma, hdr.colour.bit_depth),
            (hdr_start, 500_000_000),
            "HDR uses the same generic encoder ceiling path as Grading"
        );

        let grading = pipeline_contract(PipelineId::Grading);
        assert_eq!(grading.encoder_ceiling_bps(), Some(250_000_000));
        assert_eq!(
            PipelineId::from_served_wire(&ServedStreamPipeline::Grading)
                .map(pipeline_contract)
                .and_then(PipelineContract::encoder_ceiling_bps),
            Some(250_000_000),
            "hosts should carry ceilings from served pipeline truth, not colour inference"
        );
        assert_eq!(
            PipelineId::from_served_wire(&ServedStreamPipeline::Custom)
                .map(pipeline_contract)
                .and_then(PipelineContract::encoder_ceiling_bps),
            None,
            "Custom/exact streams never inherit a product ceiling by colour match"
        );
        assert_eq!(
            grading.encoder_bitrate_bounds(
                1800,
                1168,
                30,
                grading.colour.chroma,
                grading.colour.bit_depth,
            ),
            (4_665_600, 250_000_000)
        );
    }

    #[test]
    fn default_pipeline_probe_steps_match_origin_arithmetic_at_real_bounds() {
        for (id, width, height, fps) in [
            (PipelineId::Auto, 1800, 1168, 30),
            (PipelineId::Auto, 1800, 1168, 60),
            (PipelineId::Auto, 3840, 2160, 30),
            (PipelineId::Auto, 3840, 2160, 60),
            (PipelineId::Speed, 1800, 1168, 30),
            (PipelineId::Speed, 1800, 1168, 60),
            (PipelineId::Speed, 3840, 2160, 30),
            (PipelineId::Speed, 3840, 2160, 60),
        ] {
            assert_default_probe_sequence_matches_origin(id, width, height, fps);
        }
    }

    #[test]
    fn grading_fixed_fidelity_ceiling_uses_target_relative_probe_steps() {
        let contract = pipeline_contract(PipelineId::Grading);
        let mut controller =
            crate::rate_control::RateController::new(contract.rate_control_policy(
                1800,
                1168,
                30,
                contract.colour.chroma,
                contract.colour.bit_depth,
            ));
        let start = controller.target_bps();
        let change = controller
            .observe(clear_sample(controller.target_bps()))
            .expect("first clear grading interval probes");
        assert_eq!(start, 4_665_600);
        assert_eq!(change.target_bps, (start as f64 * 1.08).round() as u64);
        assert!(
            change.target_bps < 10_900_000,
            "Grading must not use 2.5% of the 250 Mbit/s ceiling for its first probe"
        );
    }

    #[test]
    fn hdr_to_grading_fallback_uses_grading_probe_policy() {
        let policy = operational_rate_control_policy(
            Some(&ServedStreamPipeline::Grading),
            1800,
            1168,
            30,
            ChromaSubsampling::Yuv444,
            BitDepth::Ten,
            MotionPriority::Motion,
        );
        let mut controller = crate::rate_control::RateController::new(policy);
        assert_eq!(controller.target_bps(), 4_665_600);
        let change = controller
            .observe(clear_sample(controller.target_bps()))
            .expect("fallback Grading first clear interval probes");
        assert_eq!(change.target_bps, 5_038_848);
        assert_eq!(
            policy.probe_step,
            crate::rate_control::ProbeStep::TargetRelative
        );
    }

    #[test]
    fn hdr_uses_evidence_gated_five_hundred_mbit_contract() {
        let contract = pipeline_contract(PipelineId::Hdr);
        assert_eq!(
            contract.bitrate_bounds(
                3840,
                2160,
                30,
                contract.colour.chroma,
                contract.colour.bit_depth
            ),
            (
                link_capped_average_bitrate_bps(
                    3840,
                    2160,
                    30,
                    contract.colour.chroma,
                    contract.colour.bit_depth
                ),
                500_000_000
            )
        );
        let policy = contract.rate_control_policy(
            3840,
            2160,
            30,
            contract.colour.chroma,
            contract.colour.bit_depth,
        );
        assert_eq!(
            policy.probe_step,
            crate::rate_control::ProbeStep::EvidenceGatedTargetRelative
        );
        let mut controller = crate::rate_control::RateController::new(policy);
        let start = controller.target_bps();
        assert!(
            controller.observe(sample(start / 2, 1, 1, 1)).is_none(),
            "HDR should not probe without delivered-capacity evidence"
        );
        let change = controller
            .observe(clear_sample(start))
            .expect("delivered-capacity evidence permits HDR probe");
        assert_eq!(change.target_bps, (start as f64 * 1.25).round() as u64);
    }

    #[test]
    fn deck_inbox_absorbs_a_wifi_stall_burst_at_sixty_fps() {
        let absorbed_ms = u128::try_from(DECK_VIDEO_PACKETS).expect("small") * 1000 / 60;
        assert!(
            absorbed_ms >= DECK_BURST_ABSORB.as_millis(),
            "{absorbed_ms} ms"
        );
        for id in PipelineId::ALL.iter().copied() {
            assert!(
                pipeline_contract(id).queue.deck_video_packets >= DECK_VIDEO_PACKETS,
                "{id:?}"
            );
        }
    }

    #[test]
    fn non_hdr_queue_policy_stays_at_existing_bounds() {
        for id in [
            PipelineId::Auto,
            PipelineId::Speed,
            PipelineId::Grading,
            PipelineId::Software,
        ] {
            assert_eq!(pipeline_contract(id).queue, DEFAULT_QUEUE_POLICY, "{id:?}");
        }
        assert_eq!(pipeline_contract(PipelineId::Hdr).queue, HDR_QUEUE_POLICY);
    }

    #[test]
    fn raw_overflow_is_latest_wins_without_idr_but_encoded_loss_recovers() {
        for id in PipelineId::ALL.iter().copied() {
            let queue = pipeline_contract(id).queue;
            assert_eq!(
                queue.raw_overflow.disposition,
                RawFrameOverflowDisposition::LatestWins,
                "{id:?}"
            );
            assert!(
                !queue.raw_overflow.requires_idr,
                "raw frame shedding happens before encode and must not force IDR for {id:?}"
            );
            assert_eq!(
                queue.encoded_overflow.disposition,
                EncodedFrameOverflowDisposition::ClearPredictionChain,
                "{id:?}"
            );
            assert!(
                queue.encoded_overflow.requires_idr,
                "encoded AU loss breaks prediction chain for {id:?}"
            );
            assert_eq!(
                queue.encoded_overflow.keyframe_request_min_interval,
                Duration::from_secs(1),
                "{id:?}"
            );
        }
    }

    #[test]
    fn keel_policy_is_idle_cadence_and_qp_off_today() {
        for id in PipelineId::ALL {
            let keel = pipeline_contract(*id).keel;
            assert!(keel.idle_cadence_required);
            assert_eq!(keel.qp_map_default, PipelineQpMapDefault::Off);
        }
    }

    fn hardware_context() -> ServedPipelineContext {
        ServedPipelineContext {
            backend: Some(AcceleratorClass::Hardware),
            exact_or_admin_override: false,
        }
    }

    fn software_context() -> ServedPipelineContext {
        ServedPipelineContext {
            backend: Some(AcceleratorClass::Software),
            exact_or_admin_override: false,
        }
    }

    fn custom_context() -> ServedPipelineContext {
        ServedPipelineContext {
            backend: Some(AcceleratorClass::Hardware),
            exact_or_admin_override: true,
        }
    }

    fn assert_default_probe_sequence_matches_origin(
        id: PipelineId,
        width: u32,
        height: u32,
        fps: u32,
    ) {
        let contract = pipeline_contract(id);
        let (_, ceiling) = contract.bitrate_bounds(
            width,
            height,
            fps,
            contract.colour.chroma,
            contract.colour.bit_depth,
        );
        let mut controller =
            crate::rate_control::RateController::new(contract.rate_control_policy(
                width,
                height,
                fps,
                contract.colour.chroma,
                contract.colour.bit_depth,
            ));
        let mut expected = controller.target_bps();
        for _ in 0..32 {
            let previous = expected;
            let _ = controller.observe(clear_sample(controller.target_bps()));
            expected = origin_probe(previous, u64::from(ceiling)).min(u64::from(ceiling));
            assert_eq!(
                controller.target_bps(),
                expected,
                "{id:?} {width}x{height}@{fps} drifted from origin clear-path probing"
            );
        }

        let _ = controller.observe(congested_sample(controller.target_bps()));
        let _ = controller.observe(congested_sample(controller.target_bps()));
        let mut previous = controller.target_bps();
        for _ in 0..16 {
            let _ = controller.observe(clear_sample(controller.target_bps()));
            if controller.target_bps() != previous {
                assert_eq!(
                    controller.target_bps(),
                    origin_probe(previous, u64::from(ceiling)).min(u64::from(ceiling)),
                    "{id:?} {width}x{height}@{fps} drifted from origin post-congestion recovery"
                );
                return;
            }
            previous = controller.target_bps();
        }
        panic!("{id:?} {width}x{height}@{fps} did not resume probing after congestion");
    }

    fn origin_probe(previous: u64, ceiling: u64) -> u64 {
        let additive = (ceiling as f64 * 0.025).max(250_000.0);
        let multiplicative = previous as f64 * 1.08;
        multiplicative.max(previous as f64 + additive).round() as u64
    }

    fn clear_sample(target_bps: u64) -> crate::rate_control::RateSample {
        sample(target_bps, 1, 1, 1)
    }

    fn congested_sample(target_bps: u64) -> crate::rate_control::RateSample {
        sample(target_bps.min(1_000_000), 100, 1, 80)
    }

    fn sample(
        target_bps: u64,
        wait_ms: u64,
        baseline_ms: u64,
        rtt_ms: u64,
    ) -> crate::rate_control::RateSample {
        crate::rate_control::RateSample {
            delivered_bytes: target_bps / 8,
            peak_pipeline_delivered_bytes: target_bps / 8,
            elapsed: std::time::Duration::from_secs(1),
            mean_frame_wait: std::time::Duration::from_millis(wait_ms),
            frames: 30,
            pipeline_count: 1,
            path: Some(crate::rate_control::PathSignal {
                rtt_micros: rtt_ms * 1_000,
                baseline_rtt_micros: baseline_ms * 1_000,
                congestion_window_bytes: 1_000_000,
                bytes_in_flight: None,
                congestion_events_delta: 0,
                lost_packets_delta: 0,
                lost_bytes_delta: 0,
                sent_packets_delta: 1_000,
            }),
        }
    }
}
