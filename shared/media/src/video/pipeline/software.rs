//! Shared contract for CPU software fallback hosts.
//!
//! Linux and Windows consume this contract when no GPU encoder is usable.
//! macOS Pier intentionally does not: every supported macOS host has
//! `VideoToolbox`, so there is no separate software-fallback pipeline to select.

use arcen_protocol::messages::VideoSelectionIntent;

use crate::{EncodeIntent, VideoCodec};

use super::{
    BitratePolicy, CodecPolicy, DEFAULT_QUEUE_POLICY, KEEL_IDLE_QP_OFF, KeyframePolicy,
    PipelineContract, PipelineId, SOFTWARE_LADDER, SoftwareFallbackDecision, buffer, sdr_420,
};
use crate::video::MotionPriority;

pub(super) const CONTRACT: PipelineContract = PipelineContract {
    id: PipelineId::Software,
    max_fps: 30,
    priority: MotionPriority::Detail,
    intent: EncodeIntent::Interactive,
    encoder_buffer_frames: buffer(MotionPriority::Detail, EncodeIntent::Interactive),
    selection: VideoSelectionIntent::AdaptivePerformance,
    colour: sdr_420(VideoCodec::H264),
    codec_policy: CodecPolicy::Ladder(&SOFTWARE_LADDER),
    bitrate: BitratePolicy::SoftwareCpuH264 {
        start_1080p30_bps: 4_000_000,
        ceiling_1080p30_bps: 8_000_000,
        floor_bps: 750_000,
    },
    probe_step: crate::rate_control::ProbeStep::CeilingFraction,
    keel: KEEL_IDLE_QP_OFF,
    queue: DEFAULT_QUEUE_POLICY,
    decode_latency: super::DecodeLatencyPolicy::DEFAULT,
    keyframe: KeyframePolicy::SOFTWARE_FALLBACK,
    favours: "CPU-only VM compatibility with H.264 4:2:0 at up to 30 fps",
    gives_up: "60 fps Speed, Grading, HDR, host cursor composition, and GPU encoder efficiency",
    summary: "Fallback for CPU-only hosts: H.264 4:2:0 at up to 30 fps; Speed is visibly degraded, while Grading and HDR require hardware and are refused.",
};

pub(super) const fn fallback_decision(
    requested: Option<PipelineId>,
    selection: VideoSelectionIntent,
) -> SoftwareFallbackDecision {
    match requested {
        Some(PipelineId::Speed) => SoftwareFallbackDecision::Degrade {
            reason: "software fallback serves Speed as Software/Auto H.264 8-bit 4:2:0 at 30 fps",
        },
        Some(PipelineId::Grading) => SoftwareFallbackDecision::Refuse {
            reason: "Grading requires a hardware 10-bit 4:4:4 encoder; software fallback is H.264 8-bit 4:2:0 only",
        },
        Some(PipelineId::Hdr) => SoftwareFallbackDecision::Refuse {
            reason: "HDR requires a hardware 10-bit PQ/BT.2020 encoder; software fallback is H.264 8-bit 4:2:0 only",
        },
        Some(PipelineId::Auto | PipelineId::Software) => SoftwareFallbackDecision::Serve {
            reason: "software fallback serves the Auto contract",
        },
        None => match selection {
            VideoSelectionIntent::ColorFidelity => SoftwareFallbackDecision::Refuse {
                reason: "colour-fidelity requests require a hardware 10-bit 4:4:4 encoder; software fallback is H.264 8-bit 4:2:0 only",
            },
            VideoSelectionIntent::AdaptivePerformance | VideoSelectionIntent::Exact => {
                SoftwareFallbackDecision::Serve {
                    reason: "software fallback serves the compatible H.264 8-bit request",
                }
            }
        },
    }
}
