//! `linux-wayland-software`: the Wayland composition for hosts without a
//! usable NVENC.
//!
//! The request still resolves to Auto or Speed with the shared preset rule
//! (`super::WaylandPipeline::select`); this composition serves it with the
//! shared `OpenH264` session under the shared Software contract
//! (`arcen_media::video::pipeline` `software`): H.264 4:2:0 eight-bit at up
//! to 30 fps. Admission is the requested contract's own shared admission
//! (`admit_auto`/`admit_speed`) given an `OpenH264` encoder, which applies
//! the shared software rule: Auto is served as Software, Speed is visibly
//! downgraded to it, and a stream larger than `OpenH264`'s limits is refused
//! by name. The source is the proven shared-memory route only; DMA-BUF is
//! never offered, and nothing here touches CUDA, EGL or NVENC.
//!
//! The epoch's encode half is the shared
//! `arcen_media::video::SoftwareEncodeLoop`: each delivered picture is
//! hashed once by Keel, only its changed 16x16 block rows are converted into
//! the retained I420 planes, an unchanged picture costs no encode, and
//! Keel's idle cadence encodes on change, on a requested keyframe, and
//! otherwise only at the one-second keepalive, re-encoding the retained
//! picture.

#![cfg_attr(not(all(feature = "wayland", target_os = "linux")), allow(dead_code))]

use arcen_media::video::{
    pipeline_contract, AdmissionRequest, CaptureSourceEvidence, EncoderBackend, EncoderCapability,
    PipelineContract, PipelineId, PresetAdmission,
};

use super::WaylandPipeline;
use crate::wayland::format::FormatOffer;

/// The software composition serving a requested Auto or Speed contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoftwarePipeline {
    requested: WaylandPipeline,
}

impl SoftwarePipeline {
    /// The contract this composition serves.
    pub const SERVED: PipelineId = PipelineId::Software;

    /// Serves `requested` in software.
    #[must_use]
    pub const fn new(requested: WaylandPipeline) -> Self {
        Self { requested }
    }

    /// The shared Software contract.
    #[must_use]
    pub const fn contract(self) -> PipelineContract {
        pipeline_contract(Self::SERVED)
    }

    /// The `OpenH264` encoder this composition admits against.
    #[must_use]
    pub const fn encoder() -> EncoderCapability {
        EncoderCapability {
            backend: EncoderBackend::OpenH264,
            limits: EncoderBackend::OpenH264.contract(),
        }
    }

    /// The eight-bit SDR family over shared memory, at no more than the
    /// Software contract's ceiling: asking the compositor for frames
    /// `OpenH264` will never encode only costs it read-backs.
    #[must_use]
    pub fn offer(self, requested_fps: u32) -> FormatOffer {
        FormatOffer::sdr8_host(self.contract().max_fps.min(requested_fps.max(1)))
    }

    /// The requested contract's own shared admission, given `OpenH264`.
    #[must_use]
    pub fn admit(
        self,
        request: AdmissionRequest,
        source: &CaptureSourceEvidence,
    ) -> PresetAdmission {
        self.requested.admit(request, source, &Self::encoder())
    }
}

#[cfg(test)]
mod tests {
    use arcen_media::video::{
        AdmissionReason, CopyPath, CursorEvidence, SourceTransform, StreamingPreset,
    };
    use arcen_media::VideoCodec;
    use arcen_protocol::messages::CursorMode;

    use super::*;
    use crate::wayland::format::MemoryOffer;
    use crate::wayland::pipelines::auto::tests::gnome40_host_copy;
    use crate::wayland::pipelines::{auto::AutoPipeline, speed::SpeedPipeline};

    fn request(preset: StreamingPreset, width: u32, height: u32, fps: u32) -> AdmissionRequest {
        AdmissionRequest {
            preset,
            requested_codec: VideoCodec::H264,
            width,
            height,
            max_fps: fps,
        }
    }

    #[test]
    fn the_offer_is_shared_memory_at_the_software_ceiling() {
        let software = SoftwarePipeline::new(WaylandPipeline::Speed(SpeedPipeline::new(None)));
        let offer = software.offer(60);
        assert_eq!(offer.max_fps, 30);
        assert_eq!(offer.memory, MemoryOffer::HostOnly);
        assert_eq!(software.offer(24).max_fps, 24);
    }

    #[test]
    fn auto_is_served_as_software_with_the_compositors_cursor() {
        let software = SoftwarePipeline::new(WaylandPipeline::Auto(AutoPipeline));
        let admission = software.admit(
            request(StreamingPreset::Auto, 1920, 1080, 30),
            &gnome40_host_copy(),
        );
        let PresetAdmission::Admit(served) = admission else {
            panic!("{admission:?}");
        };
        assert_eq!(served.pipeline, PipelineId::Software);
        assert_eq!(served.codec, VideoCodec::H264);
        assert_eq!(served.fps_ceiling, 30);
        assert_eq!(served.copy_path, CopyPath::HostCopy);
        assert_eq!(served.transform, SourceTransform::Sdr8Rgb);
        assert_eq!(served.cursor_mode, Some(CursorMode::Host));
        // A metadata cursor stays the Deck's.
        let metadata = CaptureSourceEvidence {
            cursor: CursorEvidence::SeparateMetadata,
            ..gnome40_host_copy()
        };
        let served = software
            .admit(request(StreamingPreset::Auto, 1920, 1080, 30), &metadata)
            .served()
            .copied()
            .expect("served");
        assert_eq!(served.cursor_mode, Some(CursorMode::Local));
    }

    #[test]
    fn speed_is_visibly_downgraded_and_oversize_refused() {
        let software = SoftwarePipeline::new(WaylandPipeline::Speed(SpeedPipeline::new(Some(60))));
        let admission = software.admit(
            request(StreamingPreset::Speed, 1920, 1080, 60),
            &gnome40_host_copy(),
        );
        let PresetAdmission::Downgrade { served, reason, .. } = admission else {
            panic!("{admission:?}");
        };
        assert_eq!(served.pipeline, PipelineId::Software);
        assert_eq!(served.fps_ceiling, 30);
        assert!(matches!(reason, AdmissionReason::SoftwareFallback(_)));
        let auto = SoftwarePipeline::new(WaylandPipeline::Auto(AutoPipeline));
        assert!(matches!(
            auto.admit(
                request(StreamingPreset::Auto, 2560, 1600, 30),
                &gnome40_host_copy()
            ),
            PresetAdmission::Unavailable {
                reason: AdmissionReason::EncoderGeometryExceeded,
                ..
            }
        ));
    }
}
