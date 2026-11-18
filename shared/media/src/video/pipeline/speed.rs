// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

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
    frame_rate: super::FrameRatePolicy::FollowDisplay {
        floor_fps: 60,
        ceiling_fps: crate::video::MAX_SESSION_FPS,
    },
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
    present_timing: super::PresentTiming::Newest,
    keyframe: KeyframePolicy::ON_DEMAND_ONLY,
    favours: "the lowest latency: at least 60 fps (your display's refresh when faster), the lowest-latency encoder settings (1-frame buffer), and each frame shown as soon as it is decoded, without tearing",
    gives_up: "sharpness first when the link is tight",
    summary: "Smoothest motion, at least 60 fps and quickest to respond. On a weak link it keeps the frame rate and lets the picture soften.",
};
