// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! Shared contract for CPU software fallback hosts.
//!
//! Linux and Windows consume this contract when no GPU encoder is usable.
//! macOS Pier intentionally does not: every supported macOS host has
//! `VideoToolbox`, so there is no separate software-fallback pipeline to select.

use arcen_protocol::messages::VideoSelectionIntent;

use crate::video::BackendUnavailableReason;
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

/// One refusal sentence per (fidelity request, hardware cause), each a
/// literal so a Deck-facing close reason stays `&'static str` and short.
macro_rules! hardware_refusal {
    ($what:literal, $cause:expr) => {
        match $cause {
            BackendUnavailableReason::UnsupportedDisplay => concat!(
                $what,
                " refused: the GPU encoder cannot capture this desktop (size or depth); software is 8-bit only"
            ),
            BackendUnavailableReason::SessionLimit => concat!(
                $what,
                " refused: every GPU encoder session is in use; software fallback is H.264 8-bit only"
            ),
            BackendUnavailableReason::HardwareUnavailable => concat!(
                $what,
                " refused: no usable GPU encoder (device, driver or vGPU licence); software is 8-bit only"
            ),
            BackendUnavailableReason::RuntimeMissing => concat!(
                $what,
                " refused: the GPU encoder runtime is missing; software fallback is H.264 8-bit only"
            ),
            BackendUnavailableReason::UnsupportedConfiguration => concat!(
                $what,
                " refused: the GPU encoder lacks this 10-bit format; software fallback is H.264 8-bit only"
            ),
            BackendUnavailableReason::NotBuilt => concat!(
                $what,
                " refused: this host was built without a GPU encoder; software fallback is 8-bit only"
            ),
        }
    };
}

/// [`fallback_decision`] after the hardware encoder reported `cause`: a
/// refusal names why the GPU path was not taken, so the user is not told
/// only that software cannot serve a request the host's GPU should have.
pub(super) const fn fallback_decision_after_hardware(
    requested: Option<PipelineId>,
    selection: VideoSelectionIntent,
    cause: BackendUnavailableReason,
) -> SoftwareFallbackDecision {
    let decision = fallback_decision(requested, selection);
    if !decision.is_refusal() {
        return decision;
    }
    SoftwareFallbackDecision::Refuse {
        reason: match requested {
            Some(PipelineId::Hdr) => hardware_refusal!("HDR", cause),
            Some(PipelineId::Grading) => hardware_refusal!("Grading", cause),
            _ => hardware_refusal!("Colour fidelity", cause),
        },
    }
}

/// Why a Grading, HDR or colour-fidelity request was refused, as a stable
/// code: the GPU encoder's own typed reason, or a host that only has the
/// software encoder. A Pier that has room leads its refusal with the code
/// (`gpu_sessions_in_use: Grading refused: …`) so an operator can look it up
/// in `docs/operations/error-codes.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FidelityRefusal {
    /// The host only has the software encoder (configured, or no encoder was
    /// tried), which is H.264 8-bit 4:2:0.
    SoftwareOnly,
    /// The GPU encoder path cannot capture this desktop at its size or depth
    /// (a ten-bit capture surface it cannot create, a desktop over its pixel
    /// budget).
    GpuDisplayUnsupported,
    /// Every GPU encoder session is in use (a consumer GPU's session limit, or
    /// other sessions on the same GPU).
    GpuSessionsInUse,
    /// No usable GPU encoder: no supported device for this display, a driver
    /// failure, or an unlicensed vGPU.
    GpuEncoderUnavailable,
    /// The GPU encoder's runtime library is not installed.
    GpuRuntimeMissing,
    /// The GPU encoder cannot encode the ten-bit format (HEVC Main 4:4:4 10 or
    /// Main10) this request needs.
    GpuFormatUnsupported,
    /// This host's build has no GPU encoder.
    GpuEncoderNotBuilt,
}

impl FidelityRefusal {
    pub const ALL: [Self; 7] = [
        Self::SoftwareOnly,
        Self::GpuDisplayUnsupported,
        Self::GpuSessionsInUse,
        Self::GpuEncoderUnavailable,
        Self::GpuRuntimeMissing,
        Self::GpuFormatUnsupported,
        Self::GpuEncoderNotBuilt,
    ];

    /// The refusal for a hardware encoder that reported `cause`, or for a host
    /// that tried none.
    #[must_use]
    pub const fn after_hardware(cause: Option<BackendUnavailableReason>) -> Self {
        match cause {
            None => Self::SoftwareOnly,
            Some(BackendUnavailableReason::UnsupportedDisplay) => Self::GpuDisplayUnsupported,
            Some(BackendUnavailableReason::SessionLimit) => Self::GpuSessionsInUse,
            Some(BackendUnavailableReason::HardwareUnavailable) => Self::GpuEncoderUnavailable,
            Some(BackendUnavailableReason::RuntimeMissing) => Self::GpuRuntimeMissing,
            Some(BackendUnavailableReason::UnsupportedConfiguration) => Self::GpuFormatUnsupported,
            Some(BackendUnavailableReason::NotBuilt) => Self::GpuEncoderNotBuilt,
        }
    }

    /// The stable code.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::SoftwareOnly => "software_encoder_only",
            Self::GpuDisplayUnsupported => "gpu_display_unsupported",
            Self::GpuSessionsInUse => "gpu_sessions_in_use",
            Self::GpuEncoderUnavailable => "gpu_encoder_unavailable",
            Self::GpuRuntimeMissing => "gpu_runtime_missing",
            Self::GpuFormatUnsupported => "gpu_format_unsupported",
            Self::GpuEncoderNotBuilt => "gpu_encoder_not_built",
        }
    }
}

/// The refusal a software-only fallback gives a fidelity request, led by its
/// [`FidelityRefusal`] code, or `None` when the request may be served
/// (possibly degraded). `cause` is the hardware encoder's typed reason; `None`
/// means no hardware encoder was tried.
pub(super) fn coded_refusal(
    requested: Option<PipelineId>,
    selection: VideoSelectionIntent,
    cause: Option<BackendUnavailableReason>,
) -> Option<String> {
    let decision = match cause {
        Some(cause) => fallback_decision_after_hardware(requested, selection, cause),
        None => fallback_decision(requested, selection),
    };
    decision.is_refusal().then(|| {
        format!(
            "{}: {}",
            FidelityRefusal::after_hardware(cause).code(),
            decision.reason()
        )
    })
}
