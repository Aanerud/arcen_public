use arcen_protocol::messages::VideoSelectionIntent;

use crate::EncodeIntent;

use super::{
    BitratePolicy, CodecPolicy, DEFAULT_QUEUE_POLICY, HEVC_FIDELITY_LADDER, KEEL_IDLE_QP_OFF,
    KeyframePolicy, PipelineContract, PipelineId, buffer, grading_reference,
};
use crate::video::MotionPriority;

const GRADING_BITRATE_CEILING_BPS: u32 = 250_000_000;

pub(super) const CONTRACT: PipelineContract = PipelineContract {
    id: PipelineId::Grading,
    max_fps: 30,
    priority: MotionPriority::Detail,
    intent: EncodeIntent::Quality,
    encoder_buffer_frames: buffer(MotionPriority::Detail, EncodeIntent::Quality),
    selection: VideoSelectionIntent::ColorFidelity,
    colour: grading_reference(),
    codec_policy: CodecPolicy::Ladder(&HEVC_FIDELITY_LADDER),
    bitrate: BitratePolicy::LinkCappedWithCeiling {
        ceiling_bps: GRADING_BITRATE_CEILING_BPS,
    },
    probe_step: crate::rate_control::ProbeStep::TargetRelative,
    keel: KEEL_IDLE_QP_OFF,
    queue: DEFAULT_QUEUE_POLICY,
    decode_latency: super::DecodeLatencyPolicy::DEFAULT,
    keyframe: KeyframePolicy::ON_DEMAND_ONLY,
    favours: "10-bit 4:4:4 BT.709 SDR detail, up to 30 fps",
    gives_up: "interaction latency by using the quality encoder path with an 8-frame buffer",
    summary: "Favours 10-bit 4:4:4 BT.709 SDR detail, up to 30 fps; gives up interaction latency by using the quality encoder path with an 8-frame buffer.",
};
