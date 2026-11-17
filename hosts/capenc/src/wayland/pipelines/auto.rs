// SPDX-License-Identifier: AGPL-3.0-only WITH LicenseRef-Arcen-Store-Exception
// Copyright (C) 2026 TUTTIFRUTTI AS
//! `linux-wayland-auto`: the Wayland Auto composition.
//!
//! Auto is the shared detail-first contract (`arcen_media::video::pipeline`
//! `auto`): the Deck display's refresh, 4:2:0 eight-bit limited BT.709,
//! interactive encoder, detail priority and its VBV. This composition asks
//! the compositor for the eight-bit SDR family and admits the source through
//! the shared `admit_auto`. Every Wayland route copies, so Auto above
//! `UNQUALIFIED_COPY_FPS` rests on the host's qualified rate for this output
//! (`capture-rate=`, the same evidence Speed uses); without one it is served
//! at that rate, labelled `copy_path_not_rate_qualified`, never refused. A
//! software encoder follows the shared software rule.

use arcen_media::video::{
    admit_auto, follow_display_offer_fps, pipeline_contract, AdmissionRequest, CaptureRateEvidence,
    CaptureSourceEvidence, EncoderCapability, PipelineContract, PipelineId, PresetAdmission,
};

use crate::wayland::format::FormatOffer;

/// The Auto composition and the host's qualified rate for its output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoPipeline {
    qualified_rate: Option<u32>,
    rate_source: CaptureRateEvidence,
}

impl AutoPipeline {
    /// The contract this composition requests.
    pub const ID: PipelineId = PipelineId::Auto;

    /// An Auto composition with the host's qualified sustained source rate
    /// for this output, when one was measured.
    #[must_use]
    pub const fn new(qualified_rate: Option<u32>) -> Self {
        Self {
            qualified_rate,
            rate_source: CaptureRateEvidence::Measured,
        }
    }

    /// The same composition, its rate measured or inferred
    /// (`capture-rate-evidence=`).
    #[must_use]
    pub const fn with_rate_evidence(self, rate_source: CaptureRateEvidence) -> Self {
        Self {
            rate_source,
            ..self
        }
    }

    /// What the rate evidence rests on.
    #[must_use]
    pub const fn rate_source(self) -> CaptureRateEvidence {
        self.rate_source
    }

    /// The shared contract.
    #[must_use]
    pub const fn contract(self) -> PipelineContract {
        pipeline_contract(Self::ID)
    }

    /// The eight-bit SDR family at the shared
    /// `follow_display_offer_fps` for the rate Auto will encode.
    #[must_use]
    pub fn offer(self, requested_fps: u32) -> FormatOffer {
        FormatOffer::sdr8_host(follow_display_offer_fps(
            self.contract().fps_ceiling().min(requested_fps.max(1)),
            self.qualified_rate,
        ))
    }

    /// The qualified sustained source rate, as admission evidence.
    #[must_use]
    pub const fn rate_evidence(self) -> Option<u32> {
        self.qualified_rate
    }

    /// The shared Auto admission.
    #[must_use]
    pub fn admit(
        self,
        request: AdmissionRequest,
        source: &CaptureSourceEvidence,
        encoder: &EncoderCapability,
    ) -> PresetAdmission {
        admit_auto(request, source, encoder)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use arcen_media::video::{
        AdmissionReason, CaptureBackend, CapturePixelFormat, CopyPath, CursorEvidence,
        EncoderBackend, GpuMatch, KeyframePolicy, MotionPriority, SourceColour, SourceFact,
        SourceTransform, StreamingPreset,
    };
    use arcen_media::{EncodeIntent, VideoCodec};
    use arcen_protocol::messages::CursorMode;

    use super::*;
    use crate::wayland::format::MemoryOffer;
    use crate::wayland::pipelines::sdr8_video;

    /// What GNOME 40 delivers: shared-memory BGRx, no colour metadata, a
    /// host copy into CUDA, the cursor composited by request.
    pub(crate) fn gnome40_host_copy() -> CaptureSourceEvidence {
        CaptureSourceEvidence {
            backend: CaptureBackend::PipeWire,
            format: CapturePixelFormat::Bgrx8,
            colour: SourceColour::UNKNOWN,
            sample_precision: SourceFact::Unknown,
            output_hdr_active: SourceFact::Unknown,
            copy_path: CopyPath::HostCopy,
            gpu: GpuMatch::Unknown,
            sustained_fps: SourceFact::Unknown,
            cursor: CursorEvidence::Composited,
        }
    }

    pub(crate) fn encoder(backend: EncoderBackend) -> EncoderCapability {
        EncoderCapability {
            backend,
            limits: backend.contract(),
        }
    }

    fn request(codec: VideoCodec) -> AdmissionRequest {
        AdmissionRequest {
            preset: StreamingPreset::Auto,
            requested_codec: codec,
            width: 1920,
            height: 1080,
            max_fps: 30,
            sdr_transfer: arcen_media::TransferCharacteristics::Bt709,
        }
    }

    #[test]
    fn auto_reads_its_numbers_from_the_shared_contract() {
        let contract = AutoPipeline::default().contract();
        assert_eq!(contract.max_fps, 30);
        assert_eq!(contract.priority, MotionPriority::Detail);
        assert_eq!(contract.intent, EncodeIntent::Interactive);
        assert!((contract.encoder_buffer_frames - 2.0).abs() < f64::EPSILON);
        assert_eq!(contract.keyframe, KeyframePolicy::ON_DEMAND_ONLY);
        assert!(contract.keel.idle_cadence_required);
        let offer = AutoPipeline::default().offer(60);
        assert_eq!(offer.max_fps, contract.max_fps);
        assert_eq!(
            offer.formats,
            vec![CapturePixelFormat::Bgrx8, CapturePixelFormat::Bgra8]
        );
        assert_eq!(offer.memory, MemoryOffer::HostOnly);
    }

    #[test]
    fn gnome40_host_copy_is_admitted_as_labelled_auto() {
        for codec in [VideoCodec::H264, VideoCodec::H265] {
            let admission = AutoPipeline::default().admit(
                request(codec),
                &gnome40_host_copy(),
                &encoder(EncoderBackend::NativeNvenc),
            );
            let PresetAdmission::Admit(served) = admission else {
                panic!("{admission:?}");
            };
            assert_eq!(served.pipeline, PipelineId::Auto);
            assert_eq!(served.codec, codec);
            assert_eq!(served.fps_ceiling, 30);
            assert_eq!(served.fps_limited_by, None);
            assert_eq!(served.transform, SourceTransform::Sdr8Rgb);
            assert_eq!(served.copy_path, CopyPath::HostCopy);
            // Mutter 40 states no colour: BT.709 is assumed and reported so.
            assert!(served.colour_assumed);
            assert_eq!(served.cursor_mode, Some(CursorMode::Host));
            let plan = served.epoch().unwrap();
            assert_eq!(plan.video, sdr8_video(codec));
            assert_eq!(
                admission
                    .degradation(StreamingPreset::Auto, 30)
                    .map(|d| d.is_exact()),
                Some(true)
            );
        }
    }

    #[test]
    fn a_24_fps_request_is_served_offered_and_encoded_at_24() {
        let request = AdmissionRequest {
            max_fps: 24,
            ..request(VideoCodec::H264)
        };
        let admission = AutoPipeline::default().admit(
            request,
            &gnome40_host_copy(),
            &encoder(EncoderBackend::NativeNvenc),
        );
        let served = admission.served().unwrap();
        assert_eq!(served.pipeline, PipelineId::Auto);
        assert_eq!(served.fps_ceiling, 24);
        assert_eq!(served.fps_limited_by, None);
        assert_eq!(served.epoch().unwrap().fps, 24);
        assert!(admission
            .degradation(StreamingPreset::Auto, 24)
            .unwrap()
            .is_exact());
        assert_eq!(AutoPipeline::default().offer(24).max_fps, 24);
        assert_eq!(AutoPipeline::default().offer(60).max_fps, 30);
    }

    #[test]
    fn auto_at_a_qualified_display_rate_is_served_and_offered_with_headroom() {
        let qualified = AutoPipeline::new(Some(60));
        assert_eq!(qualified.offer(60).max_fps, 120);
        assert_eq!(qualified.offer(30).max_fps, 30);
        let at_sixty = AdmissionRequest {
            max_fps: 60,
            ..request(VideoCodec::H265)
        };
        let measured = CaptureSourceEvidence {
            sustained_fps: SourceFact::Authoritative(60),
            ..gnome40_host_copy()
        };
        let served = qualified.admit(at_sixty, &measured, &encoder(EncoderBackend::NativeNvenc));
        let served = served.served().unwrap();
        assert_eq!(
            (served.pipeline, served.fps_ceiling),
            (PipelineId::Auto, 60)
        );
        // Without the host's measurement: Auto's long-standing 30, labelled.
        let unmeasured = AutoPipeline::default().admit(
            at_sixty,
            &gnome40_host_copy(),
            &encoder(EncoderBackend::NativeNvenc),
        );
        let served = unmeasured.served().unwrap();
        assert_eq!(
            (served.fps_ceiling, served.fps_limited_by),
            (30, Some(AdmissionReason::CopyPathNotRateQualified))
        );
    }

    #[test]
    fn wide_or_hdr_sources_never_reach_auto() {
        let wide = CaptureSourceEvidence {
            format: CapturePixelFormat::Xrgb2101010,
            ..gnome40_host_copy()
        };
        assert!(matches!(
            AutoPipeline::default().admit(
                request(VideoCodec::H264),
                &wide,
                &encoder(EncoderBackend::NativeNvenc)
            ),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::WideSourceOnEightBitPath,
                ..
            }
        ));
        let pq = CaptureSourceEvidence {
            colour: SourceColour {
                transfer: SourceFact::Authoritative(arcen_media::video::SourceTransfer::Pq),
                ..SourceColour::UNKNOWN
            },
            ..gnome40_host_copy()
        };
        assert!(matches!(
            AutoPipeline::default().admit(
                request(VideoCodec::H264),
                &pq,
                &encoder(EncoderBackend::NativeNvenc)
            ),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::HdrOnEightBitSource,
                ..
            }
        ));
    }

    #[test]
    fn an_unproven_same_gpu_route_is_refused_but_host_copy_needs_no_gpu_proof() {
        let gpu_convert = CaptureSourceEvidence {
            copy_path: CopyPath::GpuConvert,
            ..gnome40_host_copy()
        };
        assert!(matches!(
            AutoPipeline::default().admit(
                request(VideoCodec::H264),
                &gpu_convert,
                &encoder(EncoderBackend::NativeNvenc)
            ),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::GpuIdentityUnproven,
                ..
            }
        ));
    }

    #[test]
    fn the_software_rule_serves_auto_unchanged() {
        let admission = AutoPipeline::default().admit(
            request(VideoCodec::H264),
            &gnome40_host_copy(),
            &encoder(EncoderBackend::OpenH264),
        );
        let PresetAdmission::Admit(served) = admission else {
            panic!("{admission:?}");
        };
        assert_eq!(served.pipeline, PipelineId::Software);
        assert_eq!(served.codec, VideoCodec::H264);
        assert_eq!(served.fps_ceiling, 30);
    }
}
