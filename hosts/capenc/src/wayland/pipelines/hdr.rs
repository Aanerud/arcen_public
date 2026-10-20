//! `linux-wayland-hdr`: the Wayland HDR composition.
//!
//! HDR is the shared HDR10 contract (`arcen_media::video::pipeline` `hdr`):
//! 4:4:4 ten-bit full-range BT.2020 / PQ / BT.2020 NCL, the quality encoder,
//! up to its frame-rate ceiling. A colour-managed compositor in an HDR
//! (BT.2100) colour state screencasts its composition as PQ / BT.2020; this
//! composition passes the PQ signal through (`Rgb10Signal::Direct`), never
//! through a linear-light transform:
//!
//! - ten-bit PQ first: the codes are the contract's codes, exactly;
//! - half-float PQ next, rounded to ten bits.
//!
//! The shared `admit_hdr` decides. It needs the output's HDR colour state
//! proven (the session's fidelity route plus the producer's PQ tags), SDR
//! white at BT.2408's 203 nits, and wide content proven by the host's
//! qualification; otherwise it serves Grading only when Grading itself is
//! admissible, and is otherwise unavailable with both reasons. Xorg cannot
//! do this at all: its HDR requests are served as Grading.

use arcen_media::video::{
    admit_hdr, paced_source_offer_fps, pipeline_contract, AdmissionRequest, CapturePixelFormat,
    CaptureSourceEvidence, EncoderCapability, PipelineContract, PipelineId, PresetAdmission,
};

use crate::wayland::format::FormatOffer;

/// Formats HDR asks for, most preferred first.
pub const HDR_FORMATS: &[CapturePixelFormat] = &[
    CapturePixelFormat::Xbgr2101010,
    CapturePixelFormat::Xrgb2101010,
    CapturePixelFormat::RgbaF16,
];

/// The HDR composition. It has no path-specific state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HdrPipeline;

impl HdrPipeline {
    /// The contract this composition requests.
    pub const ID: PipelineId = PipelineId::Hdr;

    /// The shared contract.
    #[must_use]
    pub const fn contract(self) -> PipelineContract {
        pipeline_contract(Self::ID)
    }

    /// The wide formats, asking for up to twice the served rate.
    #[must_use]
    pub fn offer(self, requested_fps: u32) -> FormatOffer {
        FormatOffer::wide_host(
            HDR_FORMATS,
            paced_source_offer_fps(self.contract().max_fps.min(requested_fps.max(1))),
        )
    }

    /// The shared HDR admission.
    #[must_use]
    pub fn admit(
        self,
        request: AdmissionRequest,
        source: &CaptureSourceEvidence,
        encoder: &EncoderCapability,
    ) -> PresetAdmission {
        admit_hdr(request, source, encoder)
    }
}

#[cfg(test)]
mod tests {
    use arcen_media::video::{
        AdmissionReason, EncoderBackend, Rgb10Signal, SourceFact, SourceTransform, StreamingPreset,
    };

    use super::*;
    use crate::wayland::pipelines::auto::tests::encoder;
    use crate::wayland::pipelines::grading::tests::{mutter_hdr_readback, request};

    #[test]
    fn hdr_passes_pq_through_from_either_wide_format() {
        for (format, transform) in [
            (
                CapturePixelFormat::Xbgr2101010,
                SourceTransform::PackedRgb10(Rgb10Signal::Direct),
            ),
            (
                CapturePixelFormat::RgbaF16,
                SourceTransform::HalfFloatPq(Rgb10Signal::Direct),
            ),
        ] {
            let PresetAdmission::Admit(plan) = HdrPipeline.admit(
                request(StreamingPreset::Hdr),
                &mutter_hdr_readback(format),
                &encoder(EncoderBackend::NativeNvenc),
            ) else {
                panic!("{format:?} not admitted");
            };
            assert_eq!(plan.pipeline, PipelineId::Hdr);
            assert_eq!(plan.transform, transform);
        }
        assert_eq!(HdrPipeline.offer(30).formats, HDR_FORMATS);
    }

    #[test]
    fn hdr_without_a_proven_hdr_state_is_served_as_genuine_grading() {
        let unproven = CaptureSourceEvidence {
            output_hdr_active: SourceFact::Unknown,
            ..mutter_hdr_readback(CapturePixelFormat::RgbaF16)
        };
        let PresetAdmission::Downgrade { served, reason, .. } = HdrPipeline.admit(
            request(StreamingPreset::Hdr),
            &unproven,
            &encoder(EncoderBackend::NativeNvenc),
        ) else {
            panic!("expected a downgrade");
        };
        assert_eq!(reason, AdmissionReason::HdrOutputNotProven);
        assert_eq!(served.pipeline, PipelineId::Grading);
    }
}
