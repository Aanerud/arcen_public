//! Preset admission for a capture source.
//!
//! Auto, Speed, Grading and HDR are complete contracts
//! ([`super::pipeline`]). This module decides, from source evidence and an
//! encoder's capability, whether a capture source can serve a requested
//! preset exactly, only through an explicit visible downgrade, or not at all.
//! Each preset has its own admission function; none is a flag applied to
//! another.
//!
//! Rules owned here, once:
//!
//! - Auto and Speed admit only the eight-bit SDR source family. A wide
//!   capture is never routed through the fast path.
//! - Grading admits only genuine wide SDR-convertible samples with
//!   authoritative colour. Eight-bit or up-converted content is refused, never
//!   labelled Grading.
//! - HDR admits only a proven active HDR output whose samples are PQ/BT.2020
//!   (ten-bit codes or half-float signal) composed with SDR white at
//!   BT.2408's 203 nits, or documented linear light with a known SDR white. Otherwise it falls
//!   back to Grading only when Grading is genuinely admissible, else it is
//!   unavailable. PQ samples are never put through the scRGB-to-PQ transform.
//! - Software encoders follow [`super::software_fallback_decision`].
//! - Copy cost is labelled. A copying Speed path must be rate-qualified.
//!
//! Pure policy: adapters gather facts, call this, and act on the result.

use std::fmt::{Display, Formatter};

use arcen_protocol::messages::CursorMode;

use super::capture_surface::{
    CapturePixelFormat, CopyPath, CursorEvidence, GpuMatch, SamplePrecision, SourceColour,
    SourceFact, SourcePrecision, SourceTransfer,
};
use super::pipeline::{KeyframePolicy, PipelineContract, PipelineId, SoftwareFallbackDecision};
use super::{
    AcceleratorClass, BackendAvailability, BackendCandidate, BackendLimits, CaptureBackend,
    EncoderBackend, EncoderRequest, MediaRequest, MotionPriority, PlanDegradation, Rgb10Signal,
    StreamingPreset, pipeline_codec_ladder, pipeline_contract, resolve_media_plan,
    software_fallback_decision,
};
use crate::{
    ColorMatrix, ColorPrimaries, ColorRange, EncodeIntent, VideoCodec, VideoConfiguration,
};

/// Epoch-level facts about a capture source, gathered by the native adapter
/// before the first frame of a video epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureSourceEvidence {
    /// Capture API family.
    pub backend: CaptureBackend,
    /// Negotiated pixel format.
    pub format: CapturePixelFormat,
    /// Colour of the samples.
    pub colour: SourceColour,
    /// Whether wide samples carry wide content.
    pub sample_precision: SourceFact<SamplePrecision>,
    /// Whether the captured output is in an active HDR colour state.
    pub output_hdr_active: SourceFact<bool>,
    /// Path pixels take to the encoder.
    pub copy_path: CopyPath,
    /// Source GPU relative to the encoder GPU.
    pub gpu: GpuMatch,
    /// Measured sustained capture-to-encoder-input rate on this path.
    pub sustained_fps: SourceFact<u32>,
    /// Cursor representation.
    pub cursor: CursorEvidence,
}

/// The encoder that would serve the epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderCapability {
    /// Concrete backend.
    pub backend: EncoderBackend,
    /// Limits, already narrowed by the runtime probe.
    pub limits: BackendLimits,
}

/// One admission question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmissionRequest {
    /// Preset the Deck selected.
    pub preset: StreamingPreset,
    /// Codec the Deck requested; the pipeline ladder starts from it.
    pub requested_codec: VideoCodec,
    /// Coded width.
    pub width: u32,
    /// Coded height.
    pub height: u32,
    /// Frame-rate ceiling the parent asked for. The admitted ceiling never
    /// exceeds it, and asking for less than the contract's ceiling is not a
    /// degradation.
    pub max_fps: u32,
}

/// Which shared conversion turns source samples into encoder input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceTransform {
    /// Eight-bit RGB to the contract's 4:2:0 BT.709 limited YCbCr.
    Sdr8Rgb,
    /// Eight-bit limited BT.709 YCbCr passed to the encoder unchanged.
    Sdr8YuvPassthrough,
    /// Packed RGB10 through the shared [`Rgb10Signal`] conversion.
    PackedRgb10(Rgb10Signal),
    /// Half-float PQ / BT.2020 signal (a colour-managed compositor's
    /// `RGBA_F16` in an HDR colour state) through
    /// [`Rgb10Signal::map_pq_half`]: kept as PQ, or converted to BT.709 SDR
    /// from the half-float signal rather than from ten-bit PQ codes.
    HalfFloatPq(Rgb10Signal),
    /// Linear half-float to BT.709 SDR (`ScrgbSdrTransform`).
    LinearToSdr,
    /// Linear half-float to PQ (`ScrgbPqTransform`), SDR white placed at
    /// BT.2408's 203 nits.
    LinearToPq {
        /// Source SDR white, in nits.
        sdr_white_nits: u16,
    },
}

impl SourceTransform {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::Sdr8Rgb => "sdr8-rgb",
            Self::Sdr8YuvPassthrough => "sdr8-yuv",
            Self::PackedRgb10(Rgb10Signal::Direct) => "rgb10-direct",
            Self::PackedRgb10(Rgb10Signal::PqBt2020ToSdrBt709) => "rgb10-pq-to-sdr",
            Self::HalfFloatPq(Rgb10Signal::Direct) => "f16-pq-direct",
            Self::HalfFloatPq(Rgb10Signal::PqBt2020ToSdrBt709) => "f16-pq-to-sdr",
            Self::LinearToSdr => "linear-to-sdr",
            Self::LinearToPq { .. } => "linear-to-pq",
        }
    }
}

/// A plan a source can serve.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmittedCapture {
    /// Served pipeline.
    pub pipeline: PipelineId,
    /// First codec of the pipeline ladder the encoder can serve completely.
    pub codec: VideoCodec,
    /// Frame-rate ceiling actually admitted.
    pub fps_ceiling: u32,
    /// Why the ceiling is below the contract's, if it is.
    pub fps_limited_by: Option<AdmissionReason>,
    /// Shared conversion from source to encoder input.
    pub transform: SourceTransform,
    /// Copy path, reported as-is.
    pub copy_path: CopyPath,
    /// The source's SDR colour was assumed, not reported.
    pub colour_assumed: bool,
    /// Cursor truth for READY/hello; [`CursorMode::Host`] when the cursor is
    /// composited into video, [`CursorMode::Local`] when metadata allows
    /// local authority, `None` when the provider does not say.
    pub cursor_mode: Option<CursorMode>,
}

/// The served contract an admitted epoch runs: a downgraded request runs
/// the contract it was downgraded to, never the one it asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdmittedEpoch {
    /// Served contract.
    pub pipeline: PipelineId,
    /// Coded format: the contract's colour with the admitted codec.
    pub video: VideoConfiguration,
    /// Admitted frame-rate ceiling.
    pub fps: u32,
    /// The served contract's priority.
    pub priority: MotionPriority,
    /// The served contract's encoder intent.
    pub intent: EncodeIntent,
    /// The served contract's keyframe policy.
    pub keyframe: KeyframePolicy,
    /// Cursor authority.
    pub cursor_mode: CursorMode,
}

impl AdmittedEpoch {
    /// A live frame-rate request, held to the admitted ceiling.
    #[must_use]
    pub fn live_fps(self, requested: u32) -> u32 {
        requested.clamp(1, self.fps)
    }
}

impl AdmittedCapture {
    /// The epoch to run, or `None` when the admission states no cursor
    /// truth (a source with no cursor is never served silently).
    #[must_use]
    pub fn epoch(&self) -> Option<AdmittedEpoch> {
        let contract = pipeline_contract(self.pipeline);
        Some(AdmittedEpoch {
            pipeline: self.pipeline,
            video: VideoConfiguration {
                codec: self.codec,
                ..contract.colour
            },
            fps: self.fps_ceiling,
            priority: contract.priority,
            intent: contract.intent,
            keyframe: contract.keyframe,
            cursor_mode: self.cursor_mode?,
        })
    }
}

/// Why a preset was downgraded or is unavailable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionReason {
    /// An eight-bit source cannot serve a wide contract.
    EightBitSource,
    /// A wide capture was offered to an eight-bit SDR contract.
    WideSourceOnEightBitPath,
    /// Whether wide samples carry wide content is not proven.
    SourcePrecisionUnproven,
    /// Eight-bit content in a wider container.
    UpconvertedEightBitContent,
    /// Transfer not reported authoritatively.
    TransferUnknown,
    /// Primaries not reported authoritatively.
    PrimariesUnknown,
    /// YCbCr range or matrix not reported authoritatively.
    YuvSignalUnknown,
    /// An eight-bit source carries HDR or linear samples.
    HdrOnEightBitSource,
    /// No shared conversion exists from this source to this contract.
    NoSharedConversion,
    /// The output is not proven to be in an active HDR state.
    HdrOutputNotProven,
    /// The source samples are SDR.
    SdrSource,
    /// Linear or PQ samples without an authoritative SDR white.
    ReferenceWhiteUnknown,
    /// PQ samples whose SDR white is not BT.2408's 203-nit graphics white,
    /// which the shared PQ conversions assume.
    ReferenceWhiteUnsupported,
    /// The encoder cannot serve the complete contract.
    EncoderCannotServeContract,
    /// A same-GPU path is not proven to be one device.
    GpuIdentityUnproven,
    /// A same-GPU path across two devices.
    GpuMismatch,
    /// A copying Speed path has no rate measurement.
    CopyPathNotRateQualified,
    /// The measured capture/copy rate is below the contract ceiling.
    CaptureRateBelowContract,
    /// The encoder's frame-rate limit is below the contract ceiling.
    EncoderRateBelowContract,
    /// The stream is larger than the encoder can encode (`OpenH264` stops at
    /// 1920x1200); the source's size is fixed, so nothing smaller is served.
    EncoderGeometryExceeded,
    /// An admitted Speed epoch encoded below Speed's floor while its source
    /// delivered at Speed's rate (the host's runtime guard,
    /// [`super::SpeedRuntimeGuard`]); the attachment serves Auto.
    SpeedRuntimeBelowFloor,
    /// The software-fallback rule applied.
    SoftwareFallback(&'static str),
}

impl AdmissionReason {
    /// Every reason that carries no payload, for token parsing.
    pub const TOKENED: &'static [Self] = &[
        Self::EightBitSource,
        Self::WideSourceOnEightBitPath,
        Self::SourcePrecisionUnproven,
        Self::UpconvertedEightBitContent,
        Self::TransferUnknown,
        Self::PrimariesUnknown,
        Self::YuvSignalUnknown,
        Self::HdrOnEightBitSource,
        Self::NoSharedConversion,
        Self::HdrOutputNotProven,
        Self::SdrSource,
        Self::ReferenceWhiteUnknown,
        Self::ReferenceWhiteUnsupported,
        Self::EncoderCannotServeContract,
        Self::GpuIdentityUnproven,
        Self::GpuMismatch,
        Self::CopyPathNotRateQualified,
        Self::CaptureRateBelowContract,
        Self::EncoderRateBelowContract,
        Self::EncoderGeometryExceeded,
        Self::SpeedRuntimeBelowFloor,
    ];

    /// Parses a stable token. `software_fallback` carries the shared
    /// software rule's reason, which the token does not; it is restored from
    /// [`software_fallback_decision`] for `requested`.
    #[must_use]
    pub fn from_token(value: &str, requested: PipelineId) -> Option<Self> {
        if value == "software_fallback" {
            let selection = pipeline_contract(requested).selection;
            return Some(Self::SoftwareFallback(
                software_fallback_decision(Some(requested), selection).reason(),
            ));
        }
        Self::TOKENED
            .iter()
            .copied()
            .find(|reason| reason.token() == value)
    }

    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::EightBitSource => "eight_bit_source",
            Self::WideSourceOnEightBitPath => "wide_source_on_eight_bit_path",
            Self::SourcePrecisionUnproven => "source_precision_unproven",
            Self::UpconvertedEightBitContent => "upconverted_eight_bit_content",
            Self::TransferUnknown => "transfer_unknown",
            Self::PrimariesUnknown => "primaries_unknown",
            Self::YuvSignalUnknown => "yuv_signal_unknown",
            Self::HdrOnEightBitSource => "hdr_on_eight_bit_source",
            Self::NoSharedConversion => "no_shared_conversion",
            Self::HdrOutputNotProven => "hdr_output_not_proven",
            Self::SdrSource => "sdr_source",
            Self::ReferenceWhiteUnknown => "reference_white_unknown",
            Self::ReferenceWhiteUnsupported => "reference_white_unsupported",
            Self::EncoderCannotServeContract => "encoder_cannot_serve_contract",
            Self::GpuIdentityUnproven => "gpu_identity_unproven",
            Self::GpuMismatch => "gpu_mismatch",
            Self::CopyPathNotRateQualified => "copy_path_not_rate_qualified",
            Self::CaptureRateBelowContract => "capture_rate_below_contract",
            Self::EncoderRateBelowContract => "encoder_rate_below_contract",
            Self::EncoderGeometryExceeded => "encoder_geometry_exceeded",
            Self::SpeedRuntimeBelowFloor => "speed_runtime_below_floor",
            Self::SoftwareFallback(_) => "software_fallback",
        }
    }
}

impl Display for AdmissionReason {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SoftwareFallback(reason) => formatter.write_str(reason),
            other => formatter.write_str(other.token()),
        }
    }
}

/// Admission outcome for one preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresetAdmission {
    /// The requested preset is served.
    Admit(AdmittedCapture),
    /// Another pipeline is served, and must be reported as served.
    Downgrade {
        /// Preset the Deck asked for.
        requested: StreamingPreset,
        /// What will be served.
        served: AdmittedCapture,
        /// Why the requested preset could not be served.
        reason: AdmissionReason,
    },
    /// Nothing truthful can be served for this preset from this source.
    Unavailable {
        /// Preset the Deck asked for.
        requested: StreamingPreset,
        /// Why.
        reason: AdmissionReason,
        /// For HDR, why the Grading fallback was not admissible either.
        fallback: Option<AdmissionReason>,
    },
}

impl PresetAdmission {
    /// Stable decision token for telemetry.
    #[must_use]
    pub const fn decision_token(&self) -> &'static str {
        match self {
            Self::Admit(_) => "admit",
            Self::Downgrade { .. } => "downgrade",
            Self::Unavailable { .. } => "unavailable",
        }
    }

    /// The plan to serve, when any.
    #[must_use]
    pub const fn served(&self) -> Option<&AdmittedCapture> {
        match self {
            Self::Admit(served) | Self::Downgrade { served, .. } => Some(served),
            Self::Unavailable { .. } => None,
        }
    }

    /// Operator-visible difference between what was requested (the preset's
    /// contract, at no more than `requested_fps`) and what is served, in the
    /// existing degradation vocabulary.
    #[must_use]
    pub fn degradation(
        &self,
        requested: StreamingPreset,
        requested_fps: u32,
    ) -> Option<PlanDegradation> {
        let served = self.served()?;
        let wanted = requested.pipeline_contract();
        let got = pipeline_contract(served.pipeline);
        Some(PlanDegradation {
            codec_changed: false,
            chroma_changed: wanted.colour.chroma != got.colour.chroma,
            bit_depth_reduced: got.colour.bit_depth < wanted.colour.bit_depth,
            range_changed: wanted.colour.range != got.colour.range,
            matrix_changed: wanted.colour.matrix != got.colour.matrix,
            primaries_changed: wanted.colour.primaries != got.colour.primaries,
            transfer_changed: wanted.colour.transfer != got.colour.transfer,
            fps_clamped: served.fps_ceiling < wanted.max_fps.min(requested_fps.max(1)),
            geometry_clamped: false,
            cursor_moved_to_local: false,
        })
    }
}

/// Decides how `source` serves `request.preset` on `encoder`.
#[must_use]
pub fn admit_capture_source(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> PresetAdmission {
    match request.preset {
        StreamingPreset::Auto => admit_auto(request, source, encoder),
        StreamingPreset::Speed => admit_speed(request, source, encoder),
        StreamingPreset::Grading => admit_grading(request, source, encoder),
        StreamingPreset::Hdr => admit_hdr(request, source, encoder),
    }
}

fn unavailable(request: AdmissionRequest, reason: AdmissionReason) -> PresetAdmission {
    PresetAdmission::Unavailable {
        requested: request.preset,
        reason,
        fallback: None,
    }
}

/// Auto: eight-bit SDR, up to 30 fps, detail first.
#[must_use]
pub fn admit_auto(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> PresetAdmission {
    match sdr8_plan(request, PipelineId::Auto, source, encoder) {
        Ok(admission) => admission,
        Err(reason) => unavailable(request, reason),
    }
}

/// Speed: the same eight-bit SDR source family, admitted separately at the
/// 60 fps motion cadence. A copying path must be rate-qualified; without a
/// measurement it is visibly served as Auto instead.
#[must_use]
pub fn admit_speed(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> PresetAdmission {
    let software = encoder.backend.accelerator_class() == AcceleratorClass::Software;
    if !software && !source.copy_path.zero_copy() && source.sustained_fps.authoritative().is_none()
    {
        let auto = AdmissionRequest {
            preset: StreamingPreset::Auto,
            ..request
        };
        return match sdr8_plan(auto, PipelineId::Auto, source, encoder) {
            Ok(PresetAdmission::Admit(served)) => PresetAdmission::Downgrade {
                requested: request.preset,
                served,
                reason: AdmissionReason::CopyPathNotRateQualified,
            },
            Ok(other) => other,
            Err(reason) => unavailable(request, reason),
        };
    }
    match sdr8_plan(request, PipelineId::Speed, source, encoder) {
        Ok(admission) => admission,
        Err(reason) => unavailable(request, reason),
    }
}

/// Speed after the host's runtime guard found an admitted Speed epoch of
/// this attachment encoding below Speed's floor: the same eight-bit source
/// served as Auto, visibly (`speed_runtime_below_floor`).
#[must_use]
pub fn admit_speed_after_runtime_shortfall(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> PresetAdmission {
    let auto = AdmissionRequest {
        preset: StreamingPreset::Auto,
        ..request
    };
    match sdr8_plan(auto, PipelineId::Auto, source, encoder) {
        Ok(PresetAdmission::Admit(served)) => PresetAdmission::Downgrade {
            requested: request.preset,
            served,
            reason: AdmissionReason::SpeedRuntimeBelowFloor,
        },
        Ok(other) => other,
        Err(reason) => unavailable(request, reason),
    }
}

fn sdr8_plan(
    request: AdmissionRequest,
    pipeline: PipelineId,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> Result<PresetAdmission, AdmissionReason> {
    if source.format.precision().is_wide() {
        return Err(AdmissionReason::WideSourceOnEightBitPath);
    }
    let (transform, colour_assumed) = sdr8_transform(source)?;
    check_gpu(source)?;
    if encoder.backend.accelerator_class() == AcceleratorClass::Software {
        return software_plan(request, source, encoder, transform, colour_assumed);
    }
    let contract = pipeline_contract(pipeline);
    let plan = complete_plan(
        request,
        &contract,
        source,
        encoder,
        transform,
        colour_assumed,
    )?;
    Ok(PresetAdmission::Admit(plan))
}

fn sdr8_transform(
    source: &CaptureSourceEvidence,
) -> Result<(SourceTransform, bool), AdmissionReason> {
    match source.colour.transfer.value() {
        Some(SourceTransfer::Pq | SourceTransfer::Hlg | SourceTransfer::LinearExtended) => {
            return Err(AdmissionReason::HdrOnEightBitSource);
        }
        Some(SourceTransfer::Srgb | SourceTransfer::Bt709) | None => {}
    }
    if source
        .colour
        .primaries
        .value()
        .is_some_and(|primaries| primaries != ColorPrimaries::Bt709)
    {
        return Err(AdmissionReason::NoSharedConversion);
    }
    let assumed = source.colour.transfer.authoritative().is_none()
        || source.colour.primaries.authoritative().is_none();
    if source.format.is_yuv() {
        let (Some(range), Some(matrix)) = (
            source.colour.range.authoritative(),
            source.colour.matrix.authoritative(),
        ) else {
            return Err(AdmissionReason::YuvSignalUnknown);
        };
        if range != ColorRange::Limited || matrix != ColorMatrix::Bt709 {
            return Err(AdmissionReason::NoSharedConversion);
        }
        return Ok((SourceTransform::Sdr8YuvPassthrough, assumed));
    }
    Ok((SourceTransform::Sdr8Rgb, assumed))
}

fn software_plan(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
    transform: SourceTransform,
    colour_assumed: bool,
) -> Result<PresetAdmission, AdmissionReason> {
    let contract = pipeline_contract(PipelineId::Software);
    match software_fallback_decision(
        Some(request.preset.pipeline()),
        request.preset.pipeline_contract().selection,
    ) {
        SoftwareFallbackDecision::Refuse { reason } => {
            Err(AdmissionReason::SoftwareFallback(reason))
        }
        SoftwareFallbackDecision::Serve { .. } => Ok(PresetAdmission::Admit(complete_plan(
            request,
            &contract,
            source,
            encoder,
            transform,
            colour_assumed,
        )?)),
        SoftwareFallbackDecision::Degrade { reason } => Ok(PresetAdmission::Downgrade {
            requested: request.preset,
            served: complete_plan(
                request,
                &contract,
                source,
                encoder,
                transform,
                colour_assumed,
            )?,
            reason: AdmissionReason::SoftwareFallback(reason),
        }),
    }
}

/// Grading: genuine wide SDR, 4:4:4 ten-bit full-range BT.709, up to 30 fps.
#[must_use]
pub fn admit_grading(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> PresetAdmission {
    match grading_plan(request, source, encoder) {
        Ok(plan) => PresetAdmission::Admit(plan),
        Err(reason) => unavailable(request, reason),
    }
}

fn grading_plan(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> Result<AdmittedCapture, AdmissionReason> {
    fidelity_preconditions(StreamingPreset::Grading, source, encoder)?;
    let (transfer, primaries) = authoritative_colour(source)?;
    let transform = match (source.format.precision(), transfer, primaries) {
        (
            SourcePrecision::Ten,
            SourceTransfer::Srgb | SourceTransfer::Bt709,
            ColorPrimaries::Bt709,
        ) => SourceTransform::PackedRgb10(Rgb10Signal::Direct),
        (SourcePrecision::Ten, SourceTransfer::Pq, ColorPrimaries::Bt2020) => {
            pq_graphics_white(source)?;
            SourceTransform::PackedRgb10(Rgb10Signal::PqBt2020ToSdrBt709)
        }
        (SourcePrecision::HalfFloat, SourceTransfer::Pq, ColorPrimaries::Bt2020) => {
            pq_graphics_white(source)?;
            SourceTransform::HalfFloatPq(Rgb10Signal::PqBt2020ToSdrBt709)
        }
        (SourcePrecision::HalfFloat, SourceTransfer::LinearExtended, ColorPrimaries::Bt709) => {
            SourceTransform::LinearToSdr
        }
        _ => return Err(AdmissionReason::NoSharedConversion),
    };
    complete_plan(
        request,
        &pipeline_contract(PipelineId::Grading),
        source,
        encoder,
        transform,
        false,
    )
}

/// HDR: a proven active HDR output with PQ/BT.2020 or documented linear
/// samples, 4:4:4 ten-bit full-range BT.2020/PQ, up to 30 fps. Falls back to
/// Grading only when Grading is genuinely admissible.
#[must_use]
pub fn admit_hdr(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> PresetAdmission {
    let reason = match hdr_plan(request, source, encoder) {
        Ok(plan) => return PresetAdmission::Admit(plan),
        Err(reason) => reason,
    };
    if matches!(reason, AdmissionReason::SoftwareFallback(_)) {
        return unavailable(request, reason);
    }
    let grading = AdmissionRequest {
        preset: StreamingPreset::Grading,
        ..request
    };
    match grading_plan(grading, source, encoder) {
        Ok(served) => PresetAdmission::Downgrade {
            requested: request.preset,
            served,
            reason,
        },
        Err(fallback) => PresetAdmission::Unavailable {
            requested: request.preset,
            reason,
            fallback: Some(fallback),
        },
    }
}

fn hdr_plan(
    request: AdmissionRequest,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> Result<AdmittedCapture, AdmissionReason> {
    fidelity_preconditions(StreamingPreset::Hdr, source, encoder)?;
    if source.output_hdr_active.authoritative() != Some(true) {
        return Err(AdmissionReason::HdrOutputNotProven);
    }
    let (transfer, primaries) = authoritative_colour(source)?;
    let transform = match (source.format.precision(), transfer, primaries) {
        (_, SourceTransfer::Srgb | SourceTransfer::Bt709, _) => {
            return Err(AdmissionReason::SdrSource);
        }
        // PQ code values are already the contract's transfer: pass them
        // through, never through the scRGB-to-PQ transform. They are passed
        // unscaled, so the source must compose SDR at the contract's white.
        (SourcePrecision::Ten, SourceTransfer::Pq, ColorPrimaries::Bt2020) => {
            pq_graphics_white(source)?;
            SourceTransform::PackedRgb10(Rgb10Signal::Direct)
        }
        (SourcePrecision::HalfFloat, SourceTransfer::Pq, ColorPrimaries::Bt2020) => {
            pq_graphics_white(source)?;
            SourceTransform::HalfFloatPq(Rgb10Signal::Direct)
        }
        (SourcePrecision::HalfFloat, SourceTransfer::LinearExtended, ColorPrimaries::Bt709) => {
            let white = source
                .colour
                .sdr_white_nits
                .authoritative()
                .filter(|nits| *nits > 0)
                .ok_or(AdmissionReason::ReferenceWhiteUnknown)?;
            SourceTransform::LinearToPq {
                sdr_white_nits: white,
            }
        }
        _ => return Err(AdmissionReason::NoSharedConversion),
    };
    complete_plan(
        request,
        &pipeline_contract(PipelineId::Hdr),
        source,
        encoder,
        transform,
        false,
    )
}

fn fidelity_preconditions(
    preset: StreamingPreset,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
) -> Result<(), AdmissionReason> {
    if encoder.backend.accelerator_class() == AcceleratorClass::Software {
        let decision = software_fallback_decision(
            Some(preset.pipeline()),
            preset.pipeline_contract().selection,
        );
        return Err(AdmissionReason::SoftwareFallback(decision.reason()));
    }
    if !source.format.precision().is_wide() {
        return Err(AdmissionReason::EightBitSource);
    }
    match source.sample_precision {
        SourceFact::Authoritative(SamplePrecision::NativeWide) => {}
        SourceFact::Authoritative(SamplePrecision::UpconvertedEightBit)
        | SourceFact::Inferred(SamplePrecision::UpconvertedEightBit) => {
            return Err(AdmissionReason::UpconvertedEightBitContent);
        }
        SourceFact::Inferred(SamplePrecision::NativeWide) | SourceFact::Unknown => {
            return Err(AdmissionReason::SourcePrecisionUnproven);
        }
    }
    check_gpu(source)
}

fn authoritative_colour(
    source: &CaptureSourceEvidence,
) -> Result<(SourceTransfer, ColorPrimaries), AdmissionReason> {
    let transfer = source
        .colour
        .transfer
        .authoritative()
        .ok_or(AdmissionReason::TransferUnknown)?;
    let primaries = source
        .colour
        .primaries
        .authoritative()
        .ok_or(AdmissionReason::PrimariesUnknown)?;
    Ok((transfer, primaries))
}

/// A PQ source's SDR white must be reported and sit at BT.2408's graphics
/// white: the PQ pass-through and the PQ-to-SDR decode both assume it.
fn pq_graphics_white(source: &CaptureSourceEvidence) -> Result<(), AdmissionReason> {
    let white = source
        .colour
        .sdr_white_nits
        .authoritative()
        .ok_or(AdmissionReason::ReferenceWhiteUnknown)?;
    if super::pq_white::is_graphics_white(f64::from(white)) {
        Ok(())
    } else {
        Err(AdmissionReason::ReferenceWhiteUnsupported)
    }
}

fn check_gpu(source: &CaptureSourceEvidence) -> Result<(), AdmissionReason> {
    if source.copy_path.requires_same_gpu() {
        match source.gpu {
            GpuMatch::Same => {}
            GpuMatch::Different => return Err(AdmissionReason::GpuMismatch),
            GpuMatch::Unknown => return Err(AdmissionReason::GpuIdentityUnproven),
        }
    }
    Ok(())
}

fn complete_plan(
    request: AdmissionRequest,
    contract: &PipelineContract,
    source: &CaptureSourceEvidence,
    encoder: &EncoderCapability,
    transform: SourceTransform,
    colour_assumed: bool,
) -> Result<AdmittedCapture, AdmissionReason> {
    // A capture source's size is fixed (a Wayland output is created at its
    // size before any encoder exists), so a stream the encoder cannot hold
    // is refused by name rather than as a codec failure.
    if request.width > encoder.limits.max_width || request.height > encoder.limits.max_height {
        return Err(AdmissionReason::EncoderGeometryExceeded);
    }
    let mut fps_ceiling = contract.max_fps.min(request.max_fps.max(1));
    let mut fps_limited_by = None;
    if encoder.limits.max_fps < fps_ceiling {
        fps_ceiling = encoder.limits.max_fps;
        fps_limited_by = Some(AdmissionReason::EncoderRateBelowContract);
    }
    if let Some(measured) = source
        .sustained_fps
        .authoritative()
        .filter(|measured| *measured < fps_ceiling)
    {
        fps_ceiling = measured;
        fps_limited_by = Some(AdmissionReason::CaptureRateBelowContract);
    }
    if fps_ceiling == 0 {
        return Err(AdmissionReason::CaptureRateBelowContract);
    }
    let codec = first_servable_codec(request, contract, encoder, fps_ceiling)
        .ok_or(AdmissionReason::EncoderCannotServeContract)?;
    Ok(AdmittedCapture {
        pipeline: contract.id,
        codec,
        fps_ceiling,
        fps_limited_by,
        transform,
        copy_path: source.copy_path,
        colour_assumed,
        cursor_mode: source.cursor.authority(),
    })
}

fn first_servable_codec(
    request: AdmissionRequest,
    contract: &PipelineContract,
    encoder: &EncoderCapability,
    fps: u32,
) -> Option<VideoCodec> {
    let candidate = [BackendCandidate {
        backend: encoder.backend,
        availability: BackendAvailability::Available(encoder.limits),
    }];
    pipeline_codec_ladder(contract.id, request.requested_codec)
        .iter()
        .copied()
        .find(|codec| {
            let video = VideoConfiguration {
                codec: *codec,
                ..contract.colour
            };
            resolve_media_plan(
                MediaRequest {
                    encoder: EncoderRequest::Auto,
                    video,
                    width: request.width,
                    height: request.height,
                    fps,
                    cursor_mode: CursorMode::Local,
                },
                &candidate,
            )
            .is_ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TransferCharacteristics;

    fn nvenc() -> EncoderCapability {
        EncoderCapability {
            backend: EncoderBackend::NativeNvenc,
            limits: EncoderBackend::NativeNvenc.contract(),
        }
    }

    fn openh264() -> EncoderCapability {
        EncoderCapability {
            backend: EncoderBackend::OpenH264,
            limits: EncoderBackend::OpenH264.contract(),
        }
    }

    fn request(preset: StreamingPreset) -> AdmissionRequest {
        AdmissionRequest {
            preset,
            requested_codec: VideoCodec::H265,
            width: 1920,
            height: 1080,
            max_fps: preset.pipeline_contract().max_fps,
        }
    }

    fn sdr8() -> CaptureSourceEvidence {
        CaptureSourceEvidence {
            backend: CaptureBackend::PipeWire,
            format: CapturePixelFormat::Bgrx8,
            colour: SourceColour {
                primaries: SourceFact::Authoritative(ColorPrimaries::Bt709),
                transfer: SourceFact::Authoritative(SourceTransfer::Srgb),
                ..SourceColour::UNKNOWN
            },
            sample_precision: SourceFact::Unknown,
            output_hdr_active: SourceFact::Authoritative(false),
            copy_path: CopyPath::ZeroCopyImport,
            gpu: GpuMatch::Same,
            sustained_fps: SourceFact::Unknown,
            cursor: CursorEvidence::SeparateMetadata,
        }
    }

    fn rgb10_sdr() -> CaptureSourceEvidence {
        CaptureSourceEvidence {
            format: CapturePixelFormat::Xrgb2101010,
            sample_precision: SourceFact::Authoritative(SamplePrecision::NativeWide),
            ..sdr8()
        }
    }

    fn with_colour(
        mut source: CaptureSourceEvidence,
        edit: impl FnOnce(&mut SourceColour),
    ) -> CaptureSourceEvidence {
        edit(&mut source.colour);
        source
    }

    fn rgb10_pq_hdr() -> CaptureSourceEvidence {
        with_colour(
            CaptureSourceEvidence {
                output_hdr_active: SourceFact::Authoritative(true),
                ..rgb10_sdr()
            },
            |colour| {
                colour.primaries = SourceFact::Authoritative(ColorPrimaries::Bt2020);
                colour.transfer = SourceFact::Authoritative(SourceTransfer::Pq);
                colour.sdr_white_nits = SourceFact::Authoritative(203);
            },
        )
    }

    fn f16_pq_hdr() -> CaptureSourceEvidence {
        CaptureSourceEvidence {
            format: CapturePixelFormat::RgbaF16,
            ..rgb10_pq_hdr()
        }
    }

    fn f16_linear_hdr() -> CaptureSourceEvidence {
        with_colour(
            CaptureSourceEvidence {
                format: CapturePixelFormat::RgbaF16,
                output_hdr_active: SourceFact::Authoritative(true),
                ..rgb10_sdr()
            },
            |colour| {
                colour.transfer = SourceFact::Authoritative(SourceTransfer::LinearExtended);
                colour.sdr_white_nits = SourceFact::Authoritative(240);
            },
        )
    }

    fn admitted(admission: PresetAdmission) -> AdmittedCapture {
        match admission {
            PresetAdmission::Admit(plan) => plan,
            other => panic!("expected admission, got {other:?}"),
        }
    }

    // ---- Auto ----

    #[test]
    fn auto_admits_zero_copy_sdr8_at_30fps() {
        let plan = admitted(admit_auto(
            request(StreamingPreset::Auto),
            &sdr8(),
            &nvenc(),
        ));
        assert_eq!(plan.pipeline, PipelineId::Auto);
        assert_eq!(plan.fps_ceiling, 30);
        assert_eq!(plan.transform, SourceTransform::Sdr8Rgb);
        assert_eq!(plan.copy_path, CopyPath::ZeroCopyImport);
        assert_eq!(plan.codec, VideoCodec::H265);
        assert!(!plan.colour_assumed);
        assert_eq!(plan.cursor_mode, Some(CursorMode::Local));
    }

    #[test]
    fn auto_labels_a_host_copy_and_assumed_colour() {
        let source = with_colour(
            CaptureSourceEvidence {
                copy_path: CopyPath::HostCopy,
                gpu: GpuMatch::Unknown,
                cursor: CursorEvidence::Composited,
                ..sdr8()
            },
            |colour| colour.transfer = SourceFact::Unknown,
        );
        let plan = admitted(admit_auto(
            request(StreamingPreset::Auto),
            &source,
            &nvenc(),
        ));
        assert_eq!(plan.copy_path, CopyPath::HostCopy);
        assert!(plan.colour_assumed);
        assert_eq!(plan.cursor_mode, Some(CursorMode::Host));
    }

    #[test]
    fn auto_refuses_wide_and_hdr_eight_bit_sources() {
        assert_eq!(
            admit_auto(request(StreamingPreset::Auto), &rgb10_sdr(), &nvenc()),
            unavailable(
                request(StreamingPreset::Auto),
                AdmissionReason::WideSourceOnEightBitPath
            )
        );
        let pq8 = with_colour(sdr8(), |colour| {
            colour.transfer = SourceFact::Authoritative(SourceTransfer::Pq);
        });
        assert_eq!(
            admit_auto(request(StreamingPreset::Auto), &pq8, &nvenc()),
            unavailable(
                request(StreamingPreset::Auto),
                AdmissionReason::HdrOnEightBitSource
            )
        );
    }

    #[test]
    fn auto_requires_known_yuv_signal_for_nv12() {
        let nv12 = CaptureSourceEvidence {
            format: CapturePixelFormat::Nv12,
            ..sdr8()
        };
        assert_eq!(
            admit_auto(request(StreamingPreset::Auto), &nv12, &nvenc()),
            unavailable(
                request(StreamingPreset::Auto),
                AdmissionReason::YuvSignalUnknown
            )
        );
        let known = with_colour(nv12, |colour| {
            colour.range = SourceFact::Authoritative(ColorRange::Limited);
            colour.matrix = SourceFact::Authoritative(ColorMatrix::Bt709);
        });
        assert_eq!(
            admitted(admit_auto(request(StreamingPreset::Auto), &known, &nvenc())).transform,
            SourceTransform::Sdr8YuvPassthrough
        );
    }

    #[test]
    fn auto_on_software_follows_the_software_contract() {
        let source = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            ..sdr8()
        };
        let plan = admitted(admit_auto(
            request(StreamingPreset::Auto),
            &source,
            &openh264(),
        ));
        assert_eq!(plan.pipeline, PipelineId::Software);
        assert_eq!(plan.codec, VideoCodec::H264);
    }

    #[test]
    fn a_stream_larger_than_the_encoder_is_refused_by_name() {
        let source = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            ..sdr8()
        };
        let limit = AdmissionRequest {
            width: 1920,
            height: 1200,
            ..request(StreamingPreset::Auto)
        };
        assert_eq!(
            admitted(admit_auto(limit, &source, &openh264())).pipeline,
            PipelineId::Software
        );
        for (width, height) in [(1922, 1080), (1920, 1202), (2560, 1600)] {
            let oversize = AdmissionRequest {
                width,
                height,
                ..request(StreamingPreset::Auto)
            };
            for admission in [
                admit_auto(oversize, &source, &openh264()),
                admit_speed(
                    AdmissionRequest {
                        preset: StreamingPreset::Speed,
                        ..oversize
                    },
                    &source,
                    &openh264(),
                ),
            ] {
                assert!(
                    matches!(
                        admission,
                        PresetAdmission::Unavailable {
                            reason: AdmissionReason::EncoderGeometryExceeded,
                            ..
                        }
                    ),
                    "{width}x{height}: {admission:?}"
                );
            }
        }
        // NVENC holds the same size.
        let large = AdmissionRequest {
            width: 2560,
            height: 1600,
            ..request(StreamingPreset::Auto)
        };
        assert_eq!(
            admitted(admit_auto(large, &source, &nvenc())).pipeline,
            PipelineId::Auto
        );
        assert_eq!(
            AdmissionReason::from_token("encoder_geometry_exceeded", PipelineId::Auto),
            Some(AdmissionReason::EncoderGeometryExceeded)
        );
    }

    // ---- Speed ----

    #[test]
    fn speed_admits_zero_copy_at_60fps() {
        let plan = admitted(admit_speed(
            request(StreamingPreset::Speed),
            &sdr8(),
            &nvenc(),
        ));
        assert_eq!(plan.pipeline, PipelineId::Speed);
        assert_eq!(plan.fps_ceiling, 60);
        assert_eq!(plan.fps_limited_by, None);
    }

    #[test]
    fn speed_on_an_unmeasured_copy_path_is_visibly_served_as_auto() {
        let source = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            ..sdr8()
        };
        let admission = admit_speed(request(StreamingPreset::Speed), &source, &nvenc());
        let PresetAdmission::Downgrade { served, reason, .. } = admission else {
            panic!("expected downgrade, got {admission:?}");
        };
        assert_eq!(served.pipeline, PipelineId::Auto);
        assert_eq!(reason, AdmissionReason::CopyPathNotRateQualified);
        assert!(
            admission
                .degradation(StreamingPreset::Speed, 60)
                .unwrap()
                .fps_clamped
        );
    }

    #[test]
    fn speed_on_a_measured_copy_path_is_rate_qualified() {
        let fast = CaptureSourceEvidence {
            copy_path: CopyPath::GpuConvert,
            sustained_fps: SourceFact::Authoritative(75),
            ..sdr8()
        };
        assert_eq!(
            admitted(admit_speed(
                request(StreamingPreset::Speed),
                &fast,
                &nvenc()
            ))
            .fps_ceiling,
            60
        );
        let slow = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            sustained_fps: SourceFact::Authoritative(42),
            ..sdr8()
        };
        let plan = admitted(admit_speed(
            request(StreamingPreset::Speed),
            &slow,
            &nvenc(),
        ));
        assert_eq!(plan.pipeline, PipelineId::Speed);
        assert_eq!(plan.fps_ceiling, 42);
        assert_eq!(
            plan.fps_limited_by,
            Some(AdmissionReason::CaptureRateBelowContract)
        );
        let inferred = CaptureSourceEvidence {
            sustained_fps: SourceFact::Inferred(120),
            ..slow
        };
        assert!(matches!(
            admit_speed(request(StreamingPreset::Speed), &inferred, &nvenc()),
            PresetAdmission::Downgrade {
                reason: AdmissionReason::CopyPathNotRateQualified,
                ..
            }
        ));
    }

    #[test]
    fn speed_on_software_degrades_to_the_software_pipeline() {
        let source = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            ..sdr8()
        };
        let admission = admit_speed(request(StreamingPreset::Speed), &source, &openh264());
        let PresetAdmission::Downgrade { served, reason, .. } = admission else {
            panic!("expected downgrade, got {admission:?}");
        };
        assert_eq!(served.pipeline, PipelineId::Software);
        assert_eq!(served.fps_ceiling, 30);
        assert!(matches!(reason, AdmissionReason::SoftwareFallback(_)));
    }

    #[test]
    fn speed_never_takes_a_wide_capture() {
        assert_eq!(
            admit_speed(request(StreamingPreset::Speed), &f16_linear_hdr(), &nvenc()),
            unavailable(
                request(StreamingPreset::Speed),
                AdmissionReason::WideSourceOnEightBitPath
            )
        );
    }

    #[test]
    fn same_gpu_claims_need_proof_for_every_preset() {
        for preset in [StreamingPreset::Auto, StreamingPreset::Speed] {
            let source = CaptureSourceEvidence {
                gpu: GpuMatch::Unknown,
                ..sdr8()
            };
            assert_eq!(
                admit_capture_source(request(preset), &source, &nvenc()),
                unavailable(request(preset), AdmissionReason::GpuIdentityUnproven)
            );
        }
        let source = CaptureSourceEvidence {
            gpu: GpuMatch::Different,
            ..rgb10_sdr()
        };
        assert_eq!(
            admit_grading(request(StreamingPreset::Grading), &source, &nvenc()),
            unavailable(
                request(StreamingPreset::Grading),
                AdmissionReason::GpuMismatch
            )
        );
    }

    // ---- Grading ----

    #[test]
    fn grading_admits_genuine_wide_sdr() {
        let plan = admitted(admit_grading(
            request(StreamingPreset::Grading),
            &rgb10_sdr(),
            &nvenc(),
        ));
        assert_eq!(plan.pipeline, PipelineId::Grading);
        assert_eq!(plan.codec, VideoCodec::H265);
        assert_eq!(plan.fps_ceiling, 30);
        assert_eq!(
            plan.transform,
            SourceTransform::PackedRgb10(Rgb10Signal::Direct)
        );
        let linear = CaptureSourceEvidence {
            output_hdr_active: SourceFact::Authoritative(false),
            ..f16_linear_hdr()
        };
        assert_eq!(
            admitted(admit_grading(
                request(StreamingPreset::Grading),
                &linear,
                &nvenc()
            ))
            .transform,
            SourceTransform::LinearToSdr
        );
    }

    #[test]
    fn grading_converts_pq_samples_rather_than_relabelling_them() {
        assert_eq!(
            admitted(admit_grading(
                request(StreamingPreset::Grading),
                &rgb10_pq_hdr(),
                &nvenc()
            ))
            .transform,
            SourceTransform::PackedRgb10(Rgb10Signal::PqBt2020ToSdrBt709)
        );
    }

    #[test]
    fn grading_refuses_eight_bit_and_upconverted_content() {
        assert_eq!(
            admit_grading(request(StreamingPreset::Grading), &sdr8(), &nvenc()),
            unavailable(
                request(StreamingPreset::Grading),
                AdmissionReason::EightBitSource
            )
        );
        for (precision, reason) in [
            (
                SourceFact::Authoritative(SamplePrecision::UpconvertedEightBit),
                AdmissionReason::UpconvertedEightBitContent,
            ),
            (
                SourceFact::Inferred(SamplePrecision::NativeWide),
                AdmissionReason::SourcePrecisionUnproven,
            ),
            (
                SourceFact::Unknown,
                AdmissionReason::SourcePrecisionUnproven,
            ),
        ] {
            let source = CaptureSourceEvidence {
                sample_precision: precision,
                ..rgb10_sdr()
            };
            assert_eq!(
                admit_grading(request(StreamingPreset::Grading), &source, &nvenc()),
                unavailable(request(StreamingPreset::Grading), reason)
            );
        }
    }

    #[test]
    fn grading_requires_authoritative_colour() {
        let inferred = with_colour(rgb10_sdr(), |colour| {
            colour.transfer = SourceFact::Inferred(SourceTransfer::Srgb);
        });
        assert_eq!(
            admit_grading(request(StreamingPreset::Grading), &inferred, &nvenc()),
            unavailable(
                request(StreamingPreset::Grading),
                AdmissionReason::TransferUnknown
            )
        );
        let primaries = with_colour(rgb10_sdr(), |colour| {
            colour.primaries = SourceFact::Unknown;
        });
        assert_eq!(
            admit_grading(request(StreamingPreset::Grading), &primaries, &nvenc()),
            unavailable(
                request(StreamingPreset::Grading),
                AdmissionReason::PrimariesUnknown
            )
        );
    }

    #[test]
    fn grading_requires_an_encoder_for_the_exact_contract() {
        let mut limits = EncoderBackend::NativeNvenc.contract();
        limits.chroma = crate::ChromaSet::from_slice(&[crate::ChromaSubsampling::Yuv420]);
        let encoder = EncoderCapability {
            backend: EncoderBackend::NativeNvenc,
            limits,
        };
        assert_eq!(
            admit_grading(request(StreamingPreset::Grading), &rgb10_sdr(), &encoder),
            unavailable(
                request(StreamingPreset::Grading),
                AdmissionReason::EncoderCannotServeContract
            )
        );
        assert!(matches!(
            admit_grading(request(StreamingPreset::Grading), &rgb10_sdr(), &openh264()),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::SoftwareFallback(_),
                ..
            }
        ));
    }

    // ---- HDR ----

    #[test]
    fn hdr_passes_pq_samples_through_without_the_scrgb_transform() {
        let plan = admitted(admit_hdr(
            request(StreamingPreset::Hdr),
            &rgb10_pq_hdr(),
            &nvenc(),
        ));
        assert_eq!(plan.pipeline, PipelineId::Hdr);
        assert_eq!(
            plan.transform,
            SourceTransform::PackedRgb10(Rgb10Signal::Direct)
        );
        assert_eq!(
            pipeline_contract(plan.pipeline).colour.transfer,
            TransferCharacteristics::Pq
        );
    }

    #[test]
    fn hdr_converts_documented_linear_light_with_known_white() {
        let plan = admitted(admit_hdr(
            request(StreamingPreset::Hdr),
            &f16_linear_hdr(),
            &nvenc(),
        ));
        assert_eq!(
            plan.transform,
            SourceTransform::LinearToPq {
                sdr_white_nits: 240
            }
        );
        let unknown_white = with_colour(f16_linear_hdr(), |colour| {
            colour.sdr_white_nits = SourceFact::Inferred(80);
        });
        let admission = admit_hdr(request(StreamingPreset::Hdr), &unknown_white, &nvenc());
        let PresetAdmission::Downgrade { served, reason, .. } = admission else {
            panic!("expected downgrade, got {admission:?}");
        };
        assert_eq!(reason, AdmissionReason::ReferenceWhiteUnknown);
        assert_eq!(served.pipeline, PipelineId::Grading);
        assert_eq!(served.transform, SourceTransform::LinearToSdr);
    }

    #[test]
    fn hdr_never_applies_the_linear_transform_to_pq_half_floats() {
        let plan = admitted(admit_hdr(
            request(StreamingPreset::Hdr),
            &f16_pq_hdr(),
            &nvenc(),
        ));
        assert_eq!(
            plan.transform,
            SourceTransform::HalfFloatPq(Rgb10Signal::Direct)
        );
        // Half floats tagged BT.709 PQ have no shared conversion at all.
        let bt709 = with_colour(f16_pq_hdr(), |colour| {
            colour.primaries = SourceFact::Authoritative(ColorPrimaries::Bt709);
        });
        assert_eq!(
            admit_hdr(request(StreamingPreset::Hdr), &bt709, &nvenc()),
            PresetAdmission::Unavailable {
                requested: StreamingPreset::Hdr,
                reason: AdmissionReason::NoSharedConversion,
                fallback: Some(AdmissionReason::NoSharedConversion),
            }
        );
    }

    #[test]
    fn grading_converts_pq_half_floats_from_the_half_float_signal() {
        assert_eq!(
            admitted(admit_grading(
                request(StreamingPreset::Grading),
                &f16_pq_hdr(),
                &nvenc()
            ))
            .transform,
            SourceTransform::HalfFloatPq(Rgb10Signal::PqBt2020ToSdrBt709)
        );
    }

    #[test]
    fn pq_sources_need_a_reported_graphics_white() {
        for source in [rgb10_pq_hdr(), f16_pq_hdr()] {
            for (white, reason) in [
                (SourceFact::Unknown, AdmissionReason::ReferenceWhiteUnknown),
                (
                    SourceFact::Inferred(203),
                    AdmissionReason::ReferenceWhiteUnknown,
                ),
                (
                    SourceFact::Authoritative(80),
                    AdmissionReason::ReferenceWhiteUnsupported,
                ),
            ] {
                let source = with_colour(source, |colour| colour.sdr_white_nits = white);
                assert_eq!(
                    admit_grading(request(StreamingPreset::Grading), &source, &nvenc()),
                    unavailable(request(StreamingPreset::Grading), reason)
                );
                assert_eq!(
                    admit_hdr(request(StreamingPreset::Hdr), &source, &nvenc()),
                    PresetAdmission::Unavailable {
                        requested: StreamingPreset::Hdr,
                        reason,
                        fallback: Some(reason),
                    }
                );
            }
            let near = with_colour(source, |colour| {
                colour.sdr_white_nits = SourceFact::Authoritative(206);
            });
            assert!(matches!(
                admit_hdr(request(StreamingPreset::Hdr), &near, &nvenc()),
                PresetAdmission::Admit(_)
            ));
        }
    }

    #[test]
    fn hdr_without_a_proven_hdr_output_downgrades_to_genuine_grading() {
        for output_hdr_active in [
            SourceFact::Unknown,
            SourceFact::Inferred(true),
            SourceFact::Authoritative(false),
        ] {
            let source = CaptureSourceEvidence {
                output_hdr_active,
                ..rgb10_sdr()
            };
            let admission = admit_hdr(request(StreamingPreset::Hdr), &source, &nvenc());
            let PresetAdmission::Downgrade { served, reason, .. } = admission else {
                panic!("expected downgrade, got {admission:?}");
            };
            assert_eq!(reason, AdmissionReason::HdrOutputNotProven);
            assert_eq!(served.pipeline, PipelineId::Grading);
            let degradation = admission.degradation(StreamingPreset::Hdr, 30).unwrap();
            assert!(degradation.transfer_changed && degradation.primaries_changed);
        }
    }

    #[test]
    fn hdr_never_keeps_its_label_on_sdr_pixels() {
        let sdr_on_hdr_output = CaptureSourceEvidence {
            output_hdr_active: SourceFact::Authoritative(true),
            ..rgb10_sdr()
        };
        let admission = admit_hdr(request(StreamingPreset::Hdr), &sdr_on_hdr_output, &nvenc());
        assert!(matches!(
            admission,
            PresetAdmission::Downgrade {
                reason: AdmissionReason::SdrSource,
                served: AdmittedCapture {
                    pipeline: PipelineId::Grading,
                    ..
                },
                ..
            }
        ));
    }

    #[test]
    fn hdr_is_unavailable_when_grading_is_not_genuine_either() {
        let admission = admit_hdr(request(StreamingPreset::Hdr), &sdr8(), &nvenc());
        assert_eq!(
            admission,
            PresetAdmission::Unavailable {
                requested: StreamingPreset::Hdr,
                reason: AdmissionReason::EightBitSource,
                fallback: Some(AdmissionReason::EightBitSource),
            }
        );
        let upconverted = CaptureSourceEvidence {
            sample_precision: SourceFact::Authoritative(SamplePrecision::UpconvertedEightBit),
            ..rgb10_pq_hdr()
        };
        assert!(matches!(
            admit_hdr(request(StreamingPreset::Hdr), &upconverted, &nvenc()),
            PresetAdmission::Unavailable { .. }
        ));
    }

    #[test]
    fn hdr_on_software_is_refused_without_a_grading_fallback() {
        assert!(matches!(
            admit_hdr(request(StreamingPreset::Hdr), &rgb10_pq_hdr(), &openh264()),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::SoftwareFallback(_),
                fallback: None,
                ..
            }
        ));
    }

    #[test]
    fn decision_tokens_are_stable() {
        assert_eq!(
            admit_auto(request(StreamingPreset::Auto), &sdr8(), &nvenc()).decision_token(),
            "admit"
        );
        assert_eq!(
            AdmissionReason::HdrOutputNotProven.token(),
            "hdr_output_not_proven"
        );
        assert_eq!(
            SourceTransform::LinearToPq { sdr_white_nits: 1 }.token(),
            "linear-to-pq"
        );
    }

    #[test]
    fn a_runtime_shortfall_serves_speed_as_auto_and_says_why() {
        let rated = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            cursor: CursorEvidence::Composited,
            sustained_fps: SourceFact::Authoritative(60),
            ..sdr8()
        };
        let admission =
            admit_speed_after_runtime_shortfall(request(StreamingPreset::Speed), &rated, &nvenc());
        let PresetAdmission::Downgrade {
            requested,
            served,
            reason,
        } = admission
        else {
            panic!("{admission:?}");
        };
        assert_eq!(requested, StreamingPreset::Speed);
        assert_eq!(reason, AdmissionReason::SpeedRuntimeBelowFloor);
        assert_eq!(reason.token(), "speed_runtime_below_floor");
        assert_eq!(
            AdmissionReason::from_token("speed_runtime_below_floor", PipelineId::Speed),
            Some(reason)
        );
        let epoch = served.epoch().unwrap();
        assert_eq!(epoch.pipeline, PipelineId::Auto);
        assert_eq!(epoch.fps, 30);
    }

    #[test]
    fn an_admitted_epoch_runs_the_served_contract() {
        let source = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            cursor: CursorEvidence::Composited,
            ..sdr8()
        };
        let speed_as_auto = admit_speed(request(StreamingPreset::Speed), &source, &nvenc());
        let epoch = speed_as_auto.served().unwrap().epoch().unwrap();
        assert_eq!(epoch.pipeline, PipelineId::Auto);
        assert_eq!(epoch.priority, MotionPriority::Detail);
        assert_eq!(epoch.intent, EncodeIntent::Interactive);
        assert_eq!(epoch.fps, 30);
        assert_eq!(epoch.live_fps(60), 30);
        assert_eq!(epoch.live_fps(0), 1);
        assert_eq!(epoch.cursor_mode, CursorMode::Host);
        assert_eq!(epoch.video.codec, VideoCodec::H265);
        assert_eq!(epoch.video.bit_depth, crate::BitDepth::Eight);
        let rated = CaptureSourceEvidence {
            sustained_fps: SourceFact::Authoritative(35),
            ..source
        };
        let speed = admit_speed(request(StreamingPreset::Speed), &rated, &nvenc());
        let epoch = speed.served().unwrap().epoch().unwrap();
        assert_eq!(epoch.pipeline, PipelineId::Speed);
        assert_eq!(epoch.priority, MotionPriority::Motion);
        assert_eq!(epoch.fps, 35);
        let software = admit_auto(request(StreamingPreset::Auto), &source, &openh264());
        assert_eq!(
            software.served().unwrap().epoch().unwrap().keyframe,
            KeyframePolicy::SOFTWARE_FALLBACK
        );
        // No cursor in the pixels and none reported: the Deck draws its own.
        let cursorless = CaptureSourceEvidence {
            cursor: CursorEvidence::Absent,
            ..source
        };
        let auto = admit_auto(request(StreamingPreset::Auto), &cursorless, &nvenc());
        assert_eq!(auto.served().unwrap().cursor_mode, Some(CursorMode::Local));
        assert!(auto.served().unwrap().epoch().is_some());
        let unstated = CaptureSourceEvidence {
            cursor: CursorEvidence::Unknown,
            ..source
        };
        let auto = admit_auto(request(StreamingPreset::Auto), &unstated, &nvenc());
        assert_eq!(auto.served().unwrap().cursor_mode, None);
        assert_eq!(auto.served().unwrap().epoch(), None);
    }

    #[test]
    fn the_admitted_rate_never_exceeds_the_requested_rate() {
        let source = CaptureSourceEvidence {
            copy_path: CopyPath::HostCopy,
            cursor: CursorEvidence::Composited,
            ..sdr8()
        };
        let at = |preset: StreamingPreset, max_fps| AdmissionRequest {
            max_fps,
            ..request(preset)
        };
        // Auto asked for 24 fps: served at 24, not the contract's 30, and
        // asking for less is not a clamp.
        let auto = admit_auto(at(StreamingPreset::Auto, 24), &source, &nvenc());
        let served = auto.served().unwrap();
        assert_eq!(served.fps_ceiling, 24);
        assert_eq!(served.fps_limited_by, None);
        assert_eq!(served.epoch().unwrap().fps, 24);
        assert!(
            auto.degradation(StreamingPreset::Auto, 24)
                .unwrap()
                .is_exact()
        );
        // Speed at 45 served as Auto: Auto's 30 is a clamp of the 45 asked.
        let speed = admit_speed(at(StreamingPreset::Speed, 45), &source, &nvenc());
        assert_eq!(speed.served().unwrap().fps_ceiling, 30);
        assert!(
            speed
                .degradation(StreamingPreset::Speed, 45)
                .unwrap()
                .fps_clamped
        );
        // A qualified rate above the request does not raise it.
        let rated = CaptureSourceEvidence {
            sustained_fps: SourceFact::Authoritative(50),
            ..source
        };
        let speed = admit_speed(at(StreamingPreset::Speed, 45), &rated, &nvenc());
        let served = speed.served().unwrap();
        assert_eq!(served.pipeline, PipelineId::Speed);
        assert_eq!(served.fps_ceiling, 45);
        assert_eq!(served.fps_limited_by, None);
        // A qualified rate below the request clamps it.
        let slow = CaptureSourceEvidence {
            sustained_fps: SourceFact::Authoritative(35),
            ..source
        };
        let speed = admit_speed(at(StreamingPreset::Speed, 45), &slow, &nvenc());
        assert_eq!(speed.served().unwrap().fps_ceiling, 35);
        assert!(
            speed
                .degradation(StreamingPreset::Speed, 45)
                .unwrap()
                .fps_clamped
        );
    }
}
