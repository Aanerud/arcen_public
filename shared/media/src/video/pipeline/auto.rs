// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

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
    frame_rate: super::FrameRatePolicy::FollowDisplay {
        floor_fps: 1,
        ceiling_fps: crate::video::MAX_SESSION_FPS,
    },
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
    present_timing: super::PresentTiming::Newest,
    keyframe: KeyframePolicy::ON_DEMAND_ONLY,
    favours: "a sharp picture at your display's refresh rate, shown the moment it is decoded without tearing",
    gives_up: "frame rate first when the link or the host cannot keep up, and climbs back when they can",
    summary: "Sharp picture at your display's refresh rate. On a weak link it lowers the frame rate first and keeps the picture sharp.",
};
