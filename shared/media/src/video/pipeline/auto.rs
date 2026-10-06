use arcen_protocol::messages::VideoSelectionIntent;

use crate::{EncodeIntent, VideoCodec};

use super::{
    AUTO_SPEED_LADDER, BitratePolicy, CodecPolicy, DEFAULT_QUEUE_POLICY, KEEL_IDLE_QP_OFF,
    KeyframePolicy, PipelineContract, PipelineId, buffer, sdr_420,
};
use crate::video::MotionPriority;

pub(super) const CONTRACT: PipelineContract = PipelineContract {
    id: PipelineId::Auto,
    max_fps: 30,
    priority: MotionPriority::Detail,
    intent: EncodeIntent::Interactive,
    encoder_buffer_frames: buffer(MotionPriority::Detail, EncodeIntent::Interactive),
    selection: VideoSelectionIntent::AdaptivePerformance,
    colour: sdr_420(VideoCodec::H264),
    codec_policy: CodecPolicy::Ladder(&AUTO_SPEED_LADDER),
    bitrate: BitratePolicy::LinkCappedAverage,
    probe_step: crate::rate_control::ProbeStep::CeilingFraction,
    keel: KEEL_IDLE_QP_OFF,
    queue: DEFAULT_QUEUE_POLICY,
    decode_latency: super::DecodeLatencyPolicy::DEFAULT,
    keyframe: KeyframePolicy::ON_DEMAND_ONLY,
    favours: "a sharp picture, up to 30 fps",
    gives_up: "frame rate when the link is tight",
    summary: "Favours a sharp picture, up to 30 fps; gives up frame rate when the link is tight.",
};
