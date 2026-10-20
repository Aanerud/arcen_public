//! `linux-wayland-grading`: the Wayland Grading composition.
//!
//! Grading is the shared ten-bit SDR contract (`arcen_media::video::pipeline`
//! `grading`): 4:4:4 ten-bit full-range BT.709, the quality encoder, detail
//! priority, up to its frame-rate ceiling. A colour-managed compositor that
//! screencasts wide formats does so from an HDR (BT.2100) composition
//! (Mutter 50 and later offers them only then), so this composition takes
//! that composition and converts it to BT.709 SDR in shared code, as Windows
//! Grading converts its FP16 composition:
//!
//! - half-float PQ / BT.2020 first: decoded from the half-float signal it
//!   keeps 853 of the 1024 ten-bit SDR codes of an SDR ramp
//!   (`Rgb10Signal::map_pq_half`);
//! - ten-bit PQ / BT.2020 next: 445 of them, still above eight-bit's 256;
//! - nothing eight-bit, and nothing that the host's qualification has not
//!   proven to carry wide content (the shared admission refuses it).
//!
//! The shared `admit_grading` decides; SDR white must be reported at
//! BT.2408's 203 nits for the PQ decode to be the right one.

use arcen_media::video::{
    admit_grading, paced_source_offer_fps, pipeline_contract, AdmissionRequest, CapturePixelFormat,
    CaptureSourceEvidence, EncoderCapability, PipelineContract, PipelineId, PresetAdmission,
};

use crate::wayland::format::FormatOffer;

/// Formats Grading asks for, most preferred first.
pub const GRADING_FORMATS: &[CapturePixelFormat] = &[
    CapturePixelFormat::RgbaF16,
    CapturePixelFormat::Xbgr2101010,
    CapturePixelFormat::Xrgb2101010,
];

/// The Grading composition. It has no path-specific state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GradingPipeline;

impl GradingPipeline {
    /// The contract this composition requests.
    pub const ID: PipelineId = PipelineId::Grading;

    /// The shared contract.
    #[must_use]
    pub const fn contract(self) -> PipelineContract {
        pipeline_contract(Self::ID)
    }

    /// The wide formats, asking for up to twice the served rate as Speed
    /// does (`paced_source_offer_fps`): the compositor paces its capture.
    #[must_use]
    pub fn offer(self, requested_fps: u32) -> FormatOffer {
        FormatOffer::wide_host(
            GRADING_FORMATS,
            paced_source_offer_fps(self.contract().max_fps.min(requested_fps.max(1))),
        )
    }

    /// The shared Grading admission.
    #[must_use]
    pub fn admit(
        self,
        request: AdmissionRequest,
        source: &CaptureSourceEvidence,
        encoder: &EncoderCapability,
    ) -> PresetAdmission {
        admit_grading(request, source, encoder)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use arcen_media::video::{
        AdmissionReason, CaptureBackend, CopyPath, CursorEvidence, EncoderBackend, GpuMatch,
        Rgb10Signal, SamplePrecision, SourceColour, SourceFact, SourceTransfer, SourceTransform,
        StreamingPreset,
    };
    use arcen_media::{ColorPrimaries, EncodeIntent, VideoCodec};

    use super::*;
    use crate::wayland::pipelines::auto::tests::encoder;

    /// What a Mutter 50 HDR composition delivers through the wide read-back,
    /// with every fact the qualification proves.
    pub(crate) fn mutter_hdr_readback(format: CapturePixelFormat) -> CaptureSourceEvidence {
        CaptureSourceEvidence {
            backend: CaptureBackend::PipeWire,
            format,
            colour: SourceColour {
                primaries: SourceFact::Authoritative(ColorPrimaries::Bt2020),
                transfer: SourceFact::Authoritative(SourceTransfer::Pq),
                sdr_white_nits: SourceFact::Authoritative(203),
                ..SourceColour::UNKNOWN
            },
            sample_precision: SourceFact::Authoritative(SamplePrecision::NativeWide),
            output_hdr_active: SourceFact::Authoritative(true),
            copy_path: CopyPath::HostCopy,
            gpu: GpuMatch::Same,
            sustained_fps: SourceFact::Unknown,
            cursor: CursorEvidence::Composited,
        }
    }

    pub(crate) fn request(preset: StreamingPreset) -> AdmissionRequest {
        AdmissionRequest {
            preset,
            requested_codec: VideoCodec::H265,
            width: 1920,
            height: 1080,
            max_fps: 30,
        }
    }

    #[test]
    fn grading_reads_its_numbers_from_the_shared_contract() {
        let contract = GradingPipeline.contract();
        assert_eq!(contract.max_fps, 30);
        assert_eq!(contract.intent, EncodeIntent::Quality);
        let offer = GradingPipeline.offer(30);
        assert_eq!(offer.formats, GRADING_FORMATS);
        assert!(offer
            .formats
            .iter()
            .all(|format| format.precision().is_wide()));
        assert_eq!(offer.max_fps, paced_source_offer_fps(30));
    }

    #[test]
    fn grading_converts_the_hdr_composition_to_sdr() {
        for (format, transform) in [
            (
                CapturePixelFormat::RgbaF16,
                SourceTransform::HalfFloatPq(Rgb10Signal::PqBt2020ToSdrBt709),
            ),
            (
                CapturePixelFormat::Xbgr2101010,
                SourceTransform::PackedRgb10(Rgb10Signal::PqBt2020ToSdrBt709),
            ),
        ] {
            let PresetAdmission::Admit(plan) = GradingPipeline.admit(
                request(StreamingPreset::Grading),
                &mutter_hdr_readback(format),
                &encoder(EncoderBackend::NativeNvenc),
            ) else {
                panic!("{format:?} not admitted");
            };
            assert_eq!(plan.pipeline, PipelineId::Grading);
            assert_eq!(plan.transform, transform);
            assert_eq!(plan.copy_path, CopyPath::HostCopy);
        }
    }

    #[test]
    fn grading_refuses_unproven_or_eight_bit_content() {
        // Mutter's shared-memory wide buffers carry eight-bit pixels: without
        // the host's proof for that memory, nothing is served.
        let unproven = CaptureSourceEvidence {
            sample_precision: SourceFact::Unknown,
            ..mutter_hdr_readback(CapturePixelFormat::Xbgr2101010)
        };
        assert!(matches!(
            GradingPipeline.admit(
                request(StreamingPreset::Grading),
                &unproven,
                &encoder(EncoderBackend::NativeNvenc)
            ),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::SourcePrecisionUnproven,
                ..
            }
        ));
        let eight = CaptureSourceEvidence {
            format: CapturePixelFormat::Bgrx8,
            ..mutter_hdr_readback(CapturePixelFormat::Xbgr2101010)
        };
        assert!(matches!(
            GradingPipeline.admit(
                request(StreamingPreset::Grading),
                &eight,
                &encoder(EncoderBackend::NativeNvenc)
            ),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::EightBitSource,
                ..
            }
        ));
    }
}
