//! Pure video frame, conversion, backend-plan, and optional software-codec APIs.

mod bitrate;
mod convert;
mod desktop_encoding;
mod frame;
mod frame_queue;
mod intent;
mod obu;
mod plan;
mod policy;
pub mod pq_white;
mod preset;
mod qpmap;
#[cfg(feature = "software-av1-source")]
mod software_av1;
#[cfg(feature = "software-h264-source")]
mod software_h264;
mod variant;
mod wire_frame;

pub use bitrate::{average_bitrate_bps, link_capped_average_bitrate_bps};
pub use convert::{
    ColorTransform, ConversionError, PackedRgb10Layout, Rgb10Signal, SCRGB_CONVERSION_TEST_VECTORS,
    ScrgbConversionTestVector, ScrgbPqTransform, ScrgbSdrTransform, WIDE_INPUT_MAX,
    convert_bgra_to_i420, convert_bgra_to_i420_rows, convert_bgra_to_i444,
    convert_bgra_to_i444_p16, convert_bgra_to_i444_p16_rows, convert_bgra_to_i444_rows,
    convert_bgra_to_nv12, convert_bgra_to_nv12_rows, convert_packed_rgb10_to_bgra8,
    convert_packed_rgb10_to_i444_p16, convert_packed_rgb10_to_p010, convert_scrgb_to_pq_i444_p16,
    convert_scrgb_to_sdr_i444_p16, half_to_f32, linear_nits_to_pq_signal, linear_to_bt709,
    linear_to_srgb, scrgb_component_to_pq_code,
};
pub use desktop_encoding::{
    DesktopPlan, DesktopPlanError, DesktopSignalEncoding, conversion_for_output,
    resolve_desktop_plan,
};
pub use frame::{
    FrameLayoutError, I420Frame, I420FrameMut, I444Frame, I444FrameMut, I444P16FrameMut,
    Nv12FrameMut,
};
pub use frame_queue::{
    FrameClassification, PinGenerationRecovery, RoomState, SharedVideoQueue, VideoQueuePush,
    VideoQueueWaitStats,
};
pub use intent::{
    ClientVideoRequestError, ResolvedClientVideoRequest, resolve_client_video_request,
};
pub use obu::av1_low_overhead_has_sequence_header;
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
    QpMapPolicy,
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
pub use variant::{PROBE_MATRIX, VariantIdError, VideoVariant};
pub use wire_frame::{
    FramedVideoCodec, VideoWireProfile, VideoWireRoute, video_frame_message, video_header,
    wire_bit_depth, wire_chroma, wire_matrix, wire_range,
};
