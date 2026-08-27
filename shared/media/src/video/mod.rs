//! Pure video frame, conversion, backend-plan, and optional software-codec APIs.

mod convert;
mod frame;
mod intent;
mod obu;
mod plan;
mod policy;
mod qpmap;
#[cfg(feature = "software-av1-source")]
mod software_av1;
#[cfg(feature = "software-h264-source")]
mod software_h264;
mod variant;

pub use convert::{
    ColorTransform, ConversionError, convert_bgra_to_i420, convert_bgra_to_i420_rows,
    convert_bgra_to_i444, convert_bgra_to_i444_p16, convert_bgra_to_i444_p16_rows,
    convert_bgra_to_i444_rows, convert_bgra_to_nv12, convert_bgra_to_nv12_rows,
};
pub use frame::{
    FrameLayoutError, I420Frame, I420FrameMut, I444Frame, I444FrameMut, I444P16FrameMut,
    Nv12FrameMut,
};
pub use intent::{
    ClientVideoRequestError, ResolvedClientVideoRequest, resolve_client_video_request,
};
pub use obu::av1_low_overhead_has_sequence_header;
pub use plan::{
    AcceleratorClass, BackendAvailability, BackendCandidate, BackendLimits,
    BackendUnavailableNotice, BackendUnavailableReason, EncoderBackend, EncoderRequest,
    MediaPlanError, MediaRequest, PlanDegradation, ReadyExpectation, ReadyProtocolError,
    ResolvedMediaPlan, UnavailableProtocolError, format_ready_v1, format_unavailable_v1,
    parse_ready_v1, parse_unavailable_v1, resolve_media_plan, resolve_media_plan_degrading,
};
pub use policy::{
    ClientColorRequest, ColorCeiling, ColorMatrixCapabilities, ColorPolicy, HostInitialVideoError,
    HostInitialVideoPolicy, ResolvedHostInitialVideo, adaptive_codec_ladder,
    cap_bit_depth_to_client, color_contract_is_servable, resolve_client_color_request,
    resolve_client_color_request_with_matrix_caps, resolve_host_initial_video,
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
