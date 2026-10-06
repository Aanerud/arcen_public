use arcen_protocol::messages::VideoSelectionIntent;

use crate::{EncodeIntent, VideoCodec};

use super::{
    AUTO_SPEED_LADDER, BitratePolicy, CodecPolicy, DEFAULT_QUEUE_POLICY, DecodeLatencyPolicy,
    KEEL_IDLE_QP_OFF, KeyframePolicy, PipelineContract, PipelineId, buffer, sdr_420,
};
use crate::video::MotionPriority;

pub(super) const CONTRACT: PipelineContract = PipelineContract {
    id: PipelineId::Speed,
    max_fps: 60,
    priority: MotionPriority::Motion,
    intent: EncodeIntent::Interactive,
    encoder_buffer_frames: buffer(MotionPriority::Motion, EncodeIntent::Interactive),
    selection: VideoSelectionIntent::AdaptivePerformance,
    colour: sdr_420(VideoCodec::H264),
    codec_policy: CodecPolicy::Ladder(&AUTO_SPEED_LADDER),
    bitrate: BitratePolicy::MotionCappedStartShapeCeiling,
    probe_step: crate::rate_control::ProbeStep::CeilingFraction,
    keel: KEEL_IDLE_QP_OFF,
    queue: DEFAULT_QUEUE_POLICY,
    decode_latency: DecodeLatencyPolicy::SPEED,
    keyframe: KeyframePolicy::ON_DEMAND_ONLY,
    favours: "smooth motion, up to 60 fps",
    gives_up: "sharpness when the link is tight, and uses the lowest-latency encoder settings (1-frame buffer)",
    summary: "Favours smooth motion, up to 60 fps; gives up sharpness when the link is tight and uses the lowest-latency encoder settings (1-frame buffer).",
};
