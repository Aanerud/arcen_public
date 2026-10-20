//! `linux-wayland-auto`: the Wayland Auto composition.
//!
//! Auto is the shared detail-first contract (`arcen_media::video::pipeline`
//! `auto`): up to its frame-rate ceiling, 4:2:0 eight-bit limited BT.709,
//! interactive encoder, detail priority and its VBV. This composition asks
//! the compositor for the eight-bit SDR family at that ceiling and admits
//! the source through the shared `admit_auto`. Auto needs no source-rate
//! qualification: a copying route is labelled with its copy path, not
//! refused. A software encoder follows the shared software rule.

use arcen_media::video::{
    admit_auto, pipeline_contract, AdmissionRequest, CaptureSourceEvidence, EncoderCapability,
    PipelineContract, PipelineId, PresetAdmission,
};

use crate::wayland::format::FormatOffer;

/// The Auto composition. It has no path-specific state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AutoPipeline;

impl AutoPipeline {
    /// The contract this composition requests.
    pub const ID: PipelineId = PipelineId::Auto;

    /// The shared contract.
    #[must_use]
    pub const fn contract(self) -> PipelineContract {
        pipeline_contract(Self::ID)
    }

    /// The eight-bit SDR family at the contract's ceiling, or the
    /// request's when lower. Asking for no more than will be encoded lets
    /// the compositor skip frames nobody will encode.
    #[must_use]
    pub fn offer(self, requested_fps: u32) -> FormatOffer {
        FormatOffer::sdr8_host(self.contract().max_fps.min(requested_fps.max(1)))
    }

    /// Auto takes no rate qualification.
    #[must_use]
    pub const fn rate_evidence(self) -> Option<u32> {
        None
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
        }
    }

    #[test]
    fn auto_reads_its_numbers_from_the_shared_contract() {
        let contract = AutoPipeline.contract();
        assert_eq!(contract.max_fps, 30);
        assert_eq!(contract.priority, MotionPriority::Detail);
        assert_eq!(contract.intent, EncodeIntent::Interactive);
        assert!((contract.encoder_buffer_frames - 2.0).abs() < f64::EPSILON);
        assert_eq!(contract.keyframe, KeyframePolicy::ON_DEMAND_ONLY);
        assert!(contract.keel.idle_cadence_required);
        let offer = AutoPipeline.offer(60);
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
            let admission = AutoPipeline.admit(
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
        let admission = AutoPipeline.admit(
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
        assert_eq!(AutoPipeline.offer(24).max_fps, 24);
        assert_eq!(AutoPipeline.offer(60).max_fps, 30);
    }

    #[test]
    fn wide_or_hdr_sources_never_reach_auto() {
        let wide = CaptureSourceEvidence {
            format: CapturePixelFormat::Xrgb2101010,
            ..gnome40_host_copy()
        };
        assert!(matches!(
            AutoPipeline.admit(
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
            AutoPipeline.admit(
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
            AutoPipeline.admit(
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
        let admission = AutoPipeline.admit(
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
