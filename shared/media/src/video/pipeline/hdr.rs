// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS

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
    frame_rate: super::FrameRatePolicy::Fixed { fps: 30 },
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
    present_timing: super::PresentTiming::Vsync,
    keyframe: KeyframePolicy::ON_DEMAND_ONLY,
    favours: "proven HDR10 PQ/BT.2020 fidelity, up to 30 fps",
    gives_up: "interaction latency by using the quality encoder path with an 8-frame buffer; falls back visibly when the host cannot prove HDR",
    summary: "10-bit HDR (PQ, BT.2020), at 30 fps. Exact HDR comes first, so it responds more slowly; tells you when it falls back because the host cannot show HDR.",
};
