use arcen_protocol::messages::VideoSelectionIntent;

use crate::EncodeIntent;

use super::{
    BitratePolicy, CodecPolicy, HDR_QUEUE_POLICY, HEVC_FIDELITY_LADDER, KEEL_IDLE_QP_OFF,
    KeyframePolicy, PipelineContract, PipelineId, buffer, hdr10,
};
use crate::video::MotionPriority;

pub(super) const CONTRACT: PipelineContract = PipelineContract {
    id: PipelineId::Hdr,
    max_fps: 30,
    priority: MotionPriority::Detail,
    intent: EncodeIntent::Quality,
    encoder_buffer_frames: buffer(MotionPriority::Detail, EncodeIntent::Quality),
    selection: VideoSelectionIntent::ColorFidelity,
    colour: hdr10(),
    codec_policy: CodecPolicy::Ladder(&HEVC_FIDELITY_LADDER),
    bitrate: BitratePolicy::LinkCappedWithCeiling {
        ceiling_bps: 500_000_000,
    },
    probe_step: crate::rate_control::ProbeStep::EvidenceGatedTargetRelative,
    keel: KEEL_IDLE_QP_OFF,
    queue: HDR_QUEUE_POLICY,
    decode_latency: super::DecodeLatencyPolicy::DEFAULT,
    keyframe: KeyframePolicy::ON_DEMAND_ONLY,
    favours: "proven HDR10 PQ/BT.2020 fidelity, up to 30 fps",
    gives_up: "interaction latency by using the quality encoder path with an 8-frame buffer; falls back visibly when the host cannot prove HDR",
    summary: "Favours proven HDR10 PQ/BT.2020 fidelity, up to 30 fps; gives up interaction latency by using the quality encoder path with an 8-frame buffer and falls back visibly when the host cannot prove HDR.",
};
