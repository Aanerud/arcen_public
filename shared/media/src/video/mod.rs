//! Pure video frame, conversion, backend-plan, and optional software-codec APIs.

mod bitrate;
mod capture_admission;
pub mod capture_handoff;
mod capture_lease;
mod capture_notice;
mod capture_rate;
mod capture_region;
mod capture_restart;
mod capture_surface;
pub mod compositor_fidelity;
mod convert;
mod desktop_depth;
mod desktop_encoding;
mod fps_limiter;
mod frame;
mod frame_queue;
mod intent;
mod obu;
pub mod pipeline;
mod plan;
mod policy;
pub mod pq_white;
mod preset;
mod qpmap;
#[cfg(feature = "software-av1-source")]
mod software_av1;
#[cfg(feature = "software-h264-source")]
#[allow(unsafe_code)]
mod software_h264;
#[cfg(feature = "software-h264-source")]
mod software_loop;
#[cfg(feature = "software-h264-source")]
mod software_session;
mod variant;
mod wire_frame;

pub use bitrate::{average_bitrate_bps, link_capped_average_bitrate_bps};
pub use capture_admission::{
    AdmissionReason, AdmissionRequest, AdmittedCapture, AdmittedEpoch, CaptureSourceEvidence,
    EncoderCapability, PresetAdmission, SourceTransform, admit_auto, admit_capture_source,
    admit_grading, admit_hdr, admit_speed, admit_speed_after_runtime_shortfall,
};
pub use capture_lease::{
    CaptureLease, CaptureLeaseCounters, CaptureLeaseReleaser, CaptureMailbox, CaptureTakeError,
    PendingCaptureDamage, QueuedCapture,
};
pub use capture_notice::{
    ADMISSION_PREFIX, AdmissionNoticeError, CaptureAdmissionNotice, FirstFrameEvidence,
    permitted_answer,
};
pub use capture_rate::{
    CaptureRateEvidence, PACED_SOURCE_HEADROOM, SPEED_GUARD_SHORT_WINDOWS, SPEED_GUARD_WINDOWS,
    SpeedGuardVerdict, SpeedRuntimeGuard, SustainedRateMeter, paced_output_refresh_hz,
    paced_source_offer_fps,
};
pub use capture_region::{
    CaptureDamage, CaptureRegion, RegionPolicy, changed_bounds, union_regions,
};
pub use capture_restart::{
    CAPTURE_RESTART_WINDOW, CAPTURE_RESTARTS_PER_WINDOW, CaptureRestartBudget,
    CaptureRestartDecision,
};
pub use capture_surface::{
    CaptureGeometry, CaptureImporter, CaptureMemory, CapturePixelFormat, CaptureSurface,
    CaptureSurfaceError, CaptureTimestamps, CopyPath, CursorEvidence, DamageEvidence,
    FormatModifier, GpuIdentity, GpuMatch, MAX_CAPTURE_DAMAGE_RECTS, MAX_CAPTURE_PLANES,
    ModifierClass, PciAddress, PlaneLayout, SamplePrecision, SourceColour, SourceFact,
    SourcePrecision, SourceTransfer, ValidatedCaptureSurface, compare_gpus,
};
pub use convert::{
    ColorTransform, ConversionError, FrameRegion, PackedRgb10Layout, Rgb10Signal,
    SCRGB_CONVERSION_TEST_VECTORS, ScrgbConversionTestVector, ScrgbPqTransform, ScrgbSdrTransform,
    WIDE_INPUT_MAX, convert_bgra_to_i420, convert_bgra_to_i420_rows, convert_bgra_to_i444,
    convert_bgra_to_i444_p16, convert_bgra_to_i444_p16_rows, convert_bgra_to_i444_rows,
    convert_bgra_to_nv12, convert_bgra_to_nv12_rows, convert_packed_rgb10_region_to_i444_p16,
    convert_packed_rgb10_to_bgra8, convert_packed_rgb10_to_i444_p16, convert_packed_rgb10_to_p010,
    convert_rgba_f16_pq_to_packed_rgb10, convert_scrgb_to_pq_i444_p16,
    convert_scrgb_to_sdr_i444_p16, half_to_f32, linear_nits_to_pq_signal, linear_to_bt709,
    linear_to_srgb, scrgb_component_to_pq_code,
};
pub use desktop_depth::{
    DepthDecision, DepthReason, DesktopDepth, DesktopDepthPolicy, FixedDepthOutcome,
    choose_desktop_depth, serve_on_fixed_depth,
};
pub use desktop_encoding::{
    DesktopPlan, DesktopPlanError, DesktopSignalEncoding, conversion_for_output,
    resolve_desktop_plan,
};
pub use fps_limiter::{FrameRateLimiter, FrameRateVerdict, FrameRateWindow, classify_frame_rate};
pub use frame::{
    FrameLayoutError, I420Frame, I420FrameMut, I444Frame, I444FrameMut, I444P16FrameMut,
    Nv12FrameMut,
};
pub use frame_queue::{
    FrameClassification, FullFrameRequestCoalescer, FullFrameRequestDecision, KeyframeRequestRetry,
    PinGenerationRecovery, RoomState, SharedVideoQueue, VideoQueuePush, VideoQueueWaitStats,
};
pub use intent::{
    ClientVideoRequestError, ResolvedClientVideoRequest, resolve_client_video_request,
};
pub use obu::av1_low_overhead_has_sequence_header;
pub use pipeline::{
    BitratePolicy, CodecPolicy, DecodeLatencyPolicy, EncodedFrameOverflowDisposition,
    EncodedFrameOverflowPolicy, KeelPolicy, KeyframePolicy, PROVEN_PATH_CEILING_BPS,
    PROVEN_PATH_HEADROOM, PipelineContract, PipelineId, PipelineQpMapDefault, QueuePolicy,
    RawFrameOverflowDisposition, RawFrameOverflowPolicy, ServedPipelineContext,
    SoftwareFallbackDecision, aggregate_encoder_backend, aggregate_plan_encoder_backend,
    clamp_live_bitrate_target, operational_bitrate_bounds, operational_encode_intent,
    operational_encoder_bitrate_bounds, operational_keyframe_policy, operational_motion_priority,
    operational_pipeline, operational_rate_control_policy, pipeline_codec_ladder,
    pipeline_contract, pipeline_for_request, proven_path_ceiling_bps, resolve_request_pipeline,
    served_pipeline, served_pipeline_wants_display_hdr, session_wants_display_hdr,
    software_fallback_decision,
};
pub use plan::{
    AcceleratorClass, BackendAvailability, BackendCandidate, BackendLimits,
    BackendUnavailableNotice, BackendUnavailableReason, CaptureBackend, ConversionBackend,
    EncoderBackend, EncoderRequest, MediaPlanError, MediaRequest, PlanDegradation,
    ReadyExpectation, ReadyProtocolError, ResolvedMediaPlan, UnavailableProtocolError,
    format_ready_v1, format_ready_v1_with_capture, format_ready_v1_with_capture_and_conversion,
    format_unavailable_v1, parse_ready_capture, parse_ready_conversion, parse_ready_v1,
    parse_unavailable_v1, resolve_media_plan, resolve_media_plan_degrading,
};
pub use policy::{
    ClientColorRequest, ColorCeiling, ColorMatrixCapabilities, ColorPolicy, HostInitialVideoError,
    HostInitialVideoPolicy, ResolvedHostInitialVideo, adaptive_codec_ladder,
    cap_bit_depth_to_client, color_contract_is_servable, resolve_client_color_request,
    resolve_client_color_request_with_matrix_caps, resolve_host_initial_video,
    resolve_host_initial_video_with_supported_codecs,
};
pub use preset::{
    MotionPriority, PresetContract, StreamingPreset, contract, encoder_buffer_frames,
};
pub use qpmap::{
    KEEL_BLOCK_SIZE, MAX_ABS_QP_DELTA, QpBias, QpDeltaMapBuilder, QpMapError, QpMapGeometry,
    QpMapPolicy, QpMapStats,
};
#[cfg(feature = "software-av1-source")]
pub use software_av1::{
    Av1FrameKind, EncodedAv1AccessUnit, FinishedAv1AccessUnit, MAX_SOFTWARE_AV1_ACCESS_UNIT_BYTES,
    SoftwareAv1Config, SoftwareAv1Encoder, SoftwareAv1Error, SoftwareAv1Stats,
};
#[cfg(feature = "software-h264-source")]
pub use software_h264::{
    EncodedAccessUnit, EncodedFrameKind, MAX_SOFTWARE_H264_ACCESS_UNIT_BYTES, SoftwareH264Config,
    SoftwareH264Encoder, SoftwareH264Error, SoftwareH264Stats,
};
#[cfg(feature = "software-h264-source")]
pub use software_loop::{LoopStats, SoftwareEncodeLoop, StagedPicture};
#[cfg(feature = "software-h264-source")]
pub use software_session::{
    LiveChange, SoftwareH264Session, SoftwareSessionConfig, SoftwareSessionError, StageDamage,
    software_bitrate_bounds,
};
pub use variant::{PROBE_MATRIX, VariantIdError, VideoVariant};
pub use wire_frame::{
    FramedVideoCodec, VideoWireProfile, VideoWireRoute, video_frame_message, video_header,
    wire_bit_depth, wire_chroma, wire_matrix, wire_range,
};
