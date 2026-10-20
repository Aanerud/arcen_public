//! `linux-wayland-speed`: the Wayland Speed composition.
//!
//! Speed is the shared motion-first contract (`arcen_media::video::pipeline`
//! `speed`): up to its frame-rate ceiling, 4:2:0 eight-bit limited BT.709,
//! interactive encoder, motion priority, a one-frame VBV and
//! `DecodeLatencyPolicy::SPEED` on the Deck. It uses the same eight-bit SDR
//! source family as Auto but is admitted separately, by the shared
//! `admit_speed`:
//!
//! - a route that copies (anything but a zero-copy import) must be
//!   rate-qualified. Without a qualified sustained source rate the request
//!   is visibly served as Auto (`copy_path_not_rate_qualified`);
//! - with a qualified rate below the contract's ceiling, Speed is served at
//!   that rate and the clamp is reported (`capture_rate_below_contract`);
//! - a software encoder follows the shared software rule (a visible
//!   downgrade).
//!
//! The rate is evidence measured on the host (`capture-rate=`, from a
//! qualification run of this stack), never a constant here: no compositor
//! version or frame rate is assumed by this code.

use arcen_media::video::{
    admit_speed, admit_speed_after_runtime_shortfall, paced_source_offer_fps, pipeline_contract,
    AdmissionRequest, CaptureRateEvidence, CaptureSourceEvidence, EncoderCapability,
    PipelineContract, PipelineId, PresetAdmission,
};

use crate::wayland::format::FormatOffer;

/// The Speed composition and its rate evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SpeedPipeline {
    qualified_rate: Option<u32>,
    rate_source: CaptureRateEvidence,
    runtime_shortfall: bool,
}

impl SpeedPipeline {
    /// The contract this composition requests.
    pub const ID: PipelineId = PipelineId::Speed;

    /// A Speed composition with the host's qualified sustained source rate,
    /// when one was measured.
    #[must_use]
    pub const fn new(qualified_rate: Option<u32>) -> Self {
        Self {
            qualified_rate,
            rate_source: CaptureRateEvidence::Measured,
            runtime_shortfall: false,
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

    /// The same composition after the Pier's runtime guard found Speed
    /// below its floor in this attachment (`speed-runtime=below-floor`):
    /// served as Auto, with that reason.
    #[must_use]
    pub const fn after_runtime_shortfall(self, runtime_shortfall: bool) -> Self {
        Self {
            runtime_shortfall,
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

    /// The eight-bit SDR family, asking for up to twice the served rate
    /// (`arcen_media::video::paced_source_offer_fps`): a compositor that
    /// paces its capture drops frames whose arrival jitters inside one
    /// interval, so an offer of exactly the served rate delivers well below
    /// it. The compositor caps the offer at the output's refresh; the epoch
    /// still encodes at the served rate, and superseded raw frames are shed
    /// by the latest-wins lease mailbox with their damage carried forward.
    /// Nothing wider is offered on this path.
    #[must_use]
    pub fn offer(self, requested_fps: u32) -> FormatOffer {
        if self.runtime_shortfall {
            // Served as Auto: ask for no more than Auto needs.
            return super::auto::AutoPipeline.offer(requested_fps);
        }
        FormatOffer::sdr8_host(paced_source_offer_fps(
            self.contract().max_fps.min(requested_fps.max(1)),
        ))
    }

    /// The qualified sustained source rate, as admission evidence.
    #[must_use]
    pub const fn rate_evidence(self) -> Option<u32> {
        self.qualified_rate
    }

    /// The shared Speed admission.
    #[must_use]
    pub fn admit(
        self,
        request: AdmissionRequest,
        source: &CaptureSourceEvidence,
        encoder: &EncoderCapability,
    ) -> PresetAdmission {
        if self.runtime_shortfall {
            admit_speed_after_runtime_shortfall(request, source, encoder)
        } else {
            admit_speed(request, source, encoder)
        }
    }
}

#[cfg(test)]
mod tests {
    use arcen_media::video::{
        AdmissionReason, CapturePixelFormat, CopyPath, DecodeLatencyPolicy, EncoderBackend,
        GpuMatch, MotionPriority, SourceFact, StreamingPreset,
    };
    use arcen_media::{EncodeIntent, VideoCodec};
    use arcen_protocol::messages::CursorMode;

    use super::*;
    use crate::wayland::pipelines::auto::tests::{encoder, gnome40_host_copy};

    fn request() -> AdmissionRequest {
        AdmissionRequest {
            preset: StreamingPreset::Speed,
            requested_codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            max_fps: 60,
        }
    }

    fn rated(fps: Option<u32>, copy_path: CopyPath) -> CaptureSourceEvidence {
        CaptureSourceEvidence {
            copy_path,
            gpu: GpuMatch::Same,
            sustained_fps: fps.map_or(SourceFact::Unknown, SourceFact::Authoritative),
            ..gnome40_host_copy()
        }
    }

    #[test]
    fn a_runtime_shortfall_is_served_as_auto_and_asks_for_auto_only() {
        let pipeline = SpeedPipeline::new(Some(60)).after_runtime_shortfall(true);
        let admission = pipeline.admit(
            request(),
            &rated(Some(60), CopyPath::GpuConvert),
            &encoder(EncoderBackend::NativeNvenc),
        );
        let PresetAdmission::Downgrade { served, reason, .. } = admission else {
            panic!("{admission:?}");
        };
        assert_eq!(reason, AdmissionReason::SpeedRuntimeBelowFloor);
        assert_eq!(served.pipeline, PipelineId::Auto);
        assert_eq!(
            pipeline.offer(60).max_fps,
            super::super::auto::AutoPipeline.offer(60).max_fps
        );
        let inferred =
            SpeedPipeline::new(Some(60)).with_rate_evidence(CaptureRateEvidence::Inferred {
                width: 2560,
                height: 1600,
            });
        assert!(inferred.rate_source().is_inferred());
        assert_eq!(inferred.offer(60).max_fps, 120);
    }

    #[test]
    fn speed_reads_its_numbers_from_the_shared_contract() {
        let contract = SpeedPipeline::default().contract();
        assert_eq!(contract.max_fps, 60);
        assert_eq!(contract.priority, MotionPriority::Motion);
        assert_eq!(contract.intent, EncodeIntent::Interactive);
        assert!((contract.encoder_buffer_frames - 1.0).abs() < f64::EPSILON);
        assert_eq!(contract.decode_latency, DecodeLatencyPolicy::SPEED);
        // Twice the served rate: the compositor's pacing needs the headroom.
        assert_eq!(SpeedPipeline::default().offer(60).max_fps, 120);
        assert_eq!(SpeedPipeline::default().offer(45).max_fps, 90);
        assert_eq!(
            SpeedPipeline::default().offer(60).formats,
            vec![CapturePixelFormat::Bgrx8, CapturePixelFormat::Bgra8]
        );
        assert_eq!(SpeedPipeline::new(Some(35)).rate_evidence(), Some(35));
    }

    #[test]
    fn an_unqualified_host_copy_is_visibly_served_as_auto() {
        let admission = SpeedPipeline::default().admit(
            request(),
            &gnome40_host_copy(),
            &encoder(EncoderBackend::NativeNvenc),
        );
        let PresetAdmission::Downgrade { served, reason, .. } = admission else {
            panic!("{admission:?}");
        };
        assert_eq!(reason, AdmissionReason::CopyPathNotRateQualified);
        assert_eq!(served.pipeline, PipelineId::Auto);
        assert_eq!(served.fps_ceiling, 30);
        let degradation = admission.degradation(StreamingPreset::Speed, 60).unwrap();
        assert!(degradation.fps_clamped);
        // The epoch then runs Auto, not Speed.
        let plan = served.epoch().unwrap();
        assert_eq!(plan.priority, MotionPriority::Detail);
        assert_eq!(plan.fps, 30);
    }

    #[test]
    fn a_qualified_rate_below_the_ceiling_clamps_speed_and_says_so() {
        let admission = SpeedPipeline::new(Some(35)).admit(
            request(),
            &rated(Some(35), CopyPath::HostCopy),
            &encoder(EncoderBackend::NativeNvenc),
        );
        let PresetAdmission::Admit(served) = admission else {
            panic!("{admission:?}");
        };
        assert_eq!(served.pipeline, PipelineId::Speed);
        assert_eq!(served.fps_ceiling, 35);
        assert_eq!(
            served.fps_limited_by,
            Some(AdmissionReason::CaptureRateBelowContract)
        );
        assert!(
            admission
                .degradation(StreamingPreset::Speed, 60)
                .unwrap()
                .fps_clamped
        );
        let plan = served.epoch().unwrap();
        assert_eq!(plan.priority, MotionPriority::Motion);
        assert_eq!(plan.cursor_mode, CursorMode::Host);
    }

    #[test]
    fn a_qualified_route_at_the_ceiling_serves_full_speed() {
        for copy_path in [CopyPath::HostCopy, CopyPath::GpuConvert] {
            let admission = SpeedPipeline::new(Some(60)).admit(
                request(),
                &rated(Some(60), copy_path),
                &encoder(EncoderBackend::NativeNvenc),
            );
            let PresetAdmission::Admit(served) = admission else {
                panic!("{admission:?}");
            };
            assert_eq!(served.fps_ceiling, 60, "{copy_path:?}");
            assert_eq!(served.fps_limited_by, None);
            assert!(admission
                .degradation(StreamingPreset::Speed, 60)
                .unwrap()
                .is_exact());
        }
        // A zero-copy import needs no rate measurement.
        let zero_copy = SpeedPipeline::default().admit(
            request(),
            &rated(None, CopyPath::ZeroCopyImport),
            &encoder(EncoderBackend::NativeNvenc),
        );
        assert!(matches!(zero_copy, PresetAdmission::Admit(served) if served.fps_ceiling == 60));
    }

    #[test]
    fn the_software_rule_visibly_degrades_speed() {
        let admission = SpeedPipeline::default().admit(
            request(),
            &gnome40_host_copy(),
            &encoder(EncoderBackend::OpenH264),
        );
        assert!(
            matches!(
                admission,
                PresetAdmission::Downgrade {
                    served,
                    reason: AdmissionReason::SoftwareFallback(_),
                    ..
                } if served.pipeline == PipelineId::Software && served.fps_ceiling == 30
            ),
            "{admission:?}"
        );
    }
}
