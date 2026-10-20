//! Import route selection, epoch source evidence and capture-path telemetry.
//!
//! Each route is classified truthfully with a shared [`CopyPath`]:
//!
//! | Route | Memory | Copy path | Offered when |
//! | --- | --- | --- | --- |
//! | [`ImportRoute::HostCopyUpload`] | `MemFd`/`MemPtr` shared memory | `host-copy` | always |
//! | [`ImportRoute::DmabufGpuConvert`] | DMA-BUF | `gpu-convert` (EGL image -> GL blit -> CUDA array copy) | the importer opened and passed its self-test on the encoder GPU, the compositor is that same GPU, EGL lists a modifier, and the host's qualification verified the route against a host-copy reference |
//! | [`ImportRoute::HostCopyConvert`] | `MemFd`/`MemPtr` shared memory | `host-copy` (BGRx rows converted to I420 on the CPU for `OpenH264`) | the software composition (`linux-wayland-software`), which never offers DMA-BUF |
//!
//! No route here is `zero-copy-import`: the DMA-BUF route copies on the GPU
//! twice, without a CPU read-back. Unknown or different GPUs refuse the
//! DMA-BUF route (no cross-GPU path); every refusal names its reason and the
//! epoch runs on shared memory.

use arcen_media::video::{
    CaptureBackend, CaptureMemory, CaptureSourceEvidence, CopyPath, CursorEvidence, FormatModifier,
    GpuMatch, ModifierClass, SourceFact,
};
use arcen_telemetry::{
    CorrelationId, FieldValue, LifecycleEventKind, StructuredFields, ValidatedLifecycleEvent,
};

use super::format::{MemoryOffer, NegotiatedFormat};

/// How pixels get from a producer buffer into the encoder's CUDA input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportRoute {
    /// Map shared memory, copy rows host-to-device into the retained frame.
    HostCopyUpload,
    /// Import a DMA-BUF as an EGL image, blit it into a CUDA-registered GL
    /// texture, copy that into the retained frame (`super::gpu_convert`).
    DmabufGpuConvert,
    /// Read shared memory on the CPU and convert it into the software
    /// encoder's retained I420 planes; no GPU is involved.
    HostCopyConvert,
}

impl ImportRoute {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::HostCopyUpload => "host-copy-upload",
            Self::DmabufGpuConvert => "dmabuf-gl-blit",
            Self::HostCopyConvert => "shm-cpu-convert",
        }
    }

    /// The copy path this route reports.
    #[must_use]
    pub const fn copy_path(self) -> CopyPath {
        match self {
            Self::HostCopyUpload | Self::HostCopyConvert => CopyPath::HostCopy,
            Self::DmabufGpuConvert => CopyPath::GpuConvert,
        }
    }

    /// The route frames in `memory` take into the CUDA/NVENC encoder.
    #[must_use]
    pub const fn for_memory(memory: CaptureMemory) -> Option<Self> {
        match memory {
            CaptureMemory::HostShared => Some(Self::HostCopyUpload),
            CaptureMemory::DmaBuf { .. } => Some(Self::DmabufGpuConvert),
            CaptureMemory::GpuDevice => None,
        }
    }
}

/// What the DMA-BUF importer established at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmabufImporterFacts {
    /// Modifiers the importer can take as a `GL_TEXTURE_2D` for the offered
    /// formats (EGL `eglQueryDmaBufModifiersEXT`), implicit excluded.
    pub modifiers: Vec<FormatModifier>,
    /// Compositor GPU relative to the encoder GPU.
    pub gpu: GpuMatch,
    /// The host's qualification verified the route's pixels against a
    /// host-copy reference (`gpu-import=verified`, or the qualification
    /// harness itself).
    pub host_verified: bool,
}

/// The memory to offer and the route frames of each kind take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportDecision {
    /// What to offer the producer.
    pub offer: MemoryOffer,
    /// Preferred route.
    pub preferred: ImportRoute,
    /// The better route that was not taken, and why.
    pub fallback_from: Option<(ImportRoute, &'static str)>,
}

/// Chooses the routes for an epoch. `dmabuf` is `None` when the importer
/// could not be opened (or the request did not try).
#[must_use]
pub fn decide_import(dmabuf: Option<&DmabufImporterFacts>) -> ImportDecision {
    let host = |reason| ImportDecision {
        offer: MemoryOffer::HostOnly,
        preferred: ImportRoute::HostCopyUpload,
        fallback_from: Some((ImportRoute::DmabufGpuConvert, reason)),
    };
    let Some(facts) = dmabuf else {
        return host("dmabuf_importer_unavailable");
    };
    match facts.gpu {
        GpuMatch::Same => {}
        GpuMatch::Different => return host("gpu_mismatch"),
        GpuMatch::Unknown => return host("gpu_identity_unproven"),
    }
    let modifiers: Vec<FormatModifier> = facts
        .modifiers
        .iter()
        .copied()
        .filter(|modifier| modifier.class() != ModifierClass::Implicit)
        .collect();
    if modifiers.is_empty() {
        return host("no_importable_modifier");
    }
    if !facts.host_verified {
        return host("gpu_import_not_verified_on_host");
    }
    ImportDecision {
        offer: MemoryOffer::DmaBufThenHost { modifiers },
        preferred: ImportRoute::DmabufGpuConvert,
        fallback_from: None,
    }
}

/// Why an epoch's source cannot be offered to admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceRefusal {
    /// A same-GPU route without a proven same GPU.
    GpuNotSame,
}

impl EvidenceRefusal {
    /// Stable token.
    #[must_use]
    pub const fn token(self) -> &'static str {
        match self {
            Self::GpuNotSame => "gpu_not_same",
        }
    }
}

/// Epoch-level facts gathered from the first delivered frames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpochFacts {
    /// Negotiated format.
    pub negotiated: NegotiatedFormat,
    /// Memory the frames arrived in.
    pub memory: CaptureMemory,
    /// Route those frames take.
    pub route: ImportRoute,
    /// Source/encoder GPU relation.
    pub gpu: GpuMatch,
    /// Cursor representation.
    pub cursor: CursorEvidence,
    /// Measured sustained rate on this path, frames per second.
    pub measured_fps: Option<u32>,
}

/// Builds the shared admission input for an epoch, or refuses it.
///
/// # Errors
///
/// [`EvidenceRefusal`] when the route lacks proof.
pub fn capture_source_evidence(
    facts: &EpochFacts,
) -> Result<CaptureSourceEvidence, EvidenceRefusal> {
    let copy_path = facts.route.copy_path();
    if copy_path.requires_same_gpu() && facts.gpu != GpuMatch::Same {
        return Err(EvidenceRefusal::GpuNotSame);
    }
    Ok(CaptureSourceEvidence {
        backend: CaptureBackend::PipeWire,
        format: facts.negotiated.format,
        colour: facts.negotiated.colour,
        sample_precision: SourceFact::Unknown,
        output_hdr_active: SourceFact::Unknown,
        copy_path,
        gpu: facts.gpu,
        // A rate measured on this host on this path is authoritative for
        // the path; it is still only a measurement.
        sustained_fps: facts
            .measured_fps
            .map_or(SourceFact::Unknown, SourceFact::Authoritative),
        cursor: facts.cursor,
    })
}

/// Builds the `CAPTURE_PATH_SELECTED` lifecycle event (ADR 0014 fields).
///
/// # Errors
///
/// A telemetry contract error if a field were malformed (a programming error).
pub fn capture_path_event(
    correlation: CorrelationId,
    facts: &EpochFacts,
    fallback_from: Option<(ImportRoute, &'static str)>,
) -> Result<ValidatedLifecycleEvent, String> {
    let copy_path = facts.route.copy_path();
    let mut fields = StructuredFields::default();
    let mut put = |key: &str, value: FieldValue| {
        fields
            .insert(key, value)
            .map_err(|error| format!("{key}: {error}"))
    };
    let text = |value: &str| FieldValue::String(value.to_string());
    put("backend", text(CaptureBackend::PipeWire.ready_token()))?;
    put("zero_copy", FieldValue::Boolean(copy_path.zero_copy()))?;
    put("pixel_format", text(facts.negotiated.format.token()))?;
    if let Some(bytes) = facts
        .negotiated
        .format
        .min_row_bytes(0, facts.negotiated.width)
        .and_then(|row| row.checked_mul(u64::from(facts.negotiated.height)))
        .and_then(|bytes| i64::try_from(bytes).ok())
    {
        put("bytes_per_frame", FieldValue::Integer(bytes))?;
    }
    if let Some((route, reason)) = fallback_from {
        put("fallback_from", text(route.token()))?;
        put("reason", text(reason))?;
    }
    put("copy_path", text(copy_path.token()))?;
    put("memory", text(facts.memory.token()))?;
    if let CaptureMemory::DmaBuf { modifier } = facts.memory {
        put("modifier_class", text(modifier.class().token()))?;
    }
    put("gpu_match", text(facts.gpu.token()))?;
    let colour = facts.negotiated.colour;
    put(
        "source_transfer",
        text(
            colour
                .transfer
                .value()
                .map_or("unknown", |value| value.token()),
        ),
    )?;
    put(
        "source_primaries",
        text(
            colour
                .primaries
                .value()
                .map_or("unknown", |value| value.token()),
        ),
    )?;
    put(
        "sample_precision",
        text(if facts.negotiated.format.precision().is_wide() {
            "unknown"
        } else {
            "eight_bit"
        }),
    )?;
    put("cursor", text(facts.cursor.token()))?;
    ValidatedLifecycleEvent::new(LifecycleEventKind::CapturePathSelected, correlation, fields)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use arcen_media::video::{
        admit_capture_source, AdmissionRequest, EncoderBackend, EncoderCapability, PipelineId,
        PresetAdmission, SourceColour, StreamingPreset,
    };
    use arcen_media::VideoCodec;

    use super::*;
    use crate::wayland::layout::tests::negotiated;

    const TILED: FormatModifier = FormatModifier(0x0300_0000_004f_e010);

    fn facts(gpu: GpuMatch, host_verified: bool) -> DmabufImporterFacts {
        DmabufImporterFacts {
            modifiers: vec![FormatModifier::IMPLICIT, TILED],
            gpu,
            host_verified,
        }
    }

    #[test]
    fn host_copy_is_the_route_without_a_verified_dmabuf_importer() {
        let decision = decide_import(None);
        assert_eq!(decision.offer, MemoryOffer::HostOnly);
        assert_eq!(decision.preferred, ImportRoute::HostCopyUpload);
        assert_eq!(decision.preferred.copy_path(), CopyPath::HostCopy);
        let decision = decide_import(Some(&facts(GpuMatch::Same, false)));
        assert_eq!(decision.offer, MemoryOffer::HostOnly);
        assert_eq!(
            decision.fallback_from,
            Some((
                ImportRoute::DmabufGpuConvert,
                "gpu_import_not_verified_on_host"
            ))
        );
    }

    #[test]
    fn wrong_or_unknown_gpu_refuses_dmabuf() {
        for (gpu, reason) in [
            (GpuMatch::Different, "gpu_mismatch"),
            (GpuMatch::Unknown, "gpu_identity_unproven"),
        ] {
            let decision = decide_import(Some(&facts(gpu, true)));
            assert_eq!(decision.offer, MemoryOffer::HostOnly);
            assert_eq!(
                decision.fallback_from,
                Some((ImportRoute::DmabufGpuConvert, reason))
            );
        }
    }

    #[test]
    fn a_verified_same_gpu_importer_offers_its_explicit_modifiers() {
        let decision = decide_import(Some(&facts(GpuMatch::Same, true)));
        assert_eq!(
            decision.offer,
            MemoryOffer::DmaBufThenHost {
                modifiers: vec![TILED]
            }
        );
        assert_eq!(decision.preferred.copy_path(), CopyPath::GpuConvert);
        assert_eq!(decision.fallback_from, None);
        let implicit_only = DmabufImporterFacts {
            modifiers: vec![FormatModifier::IMPLICIT],
            ..facts(GpuMatch::Same, true)
        };
        assert_eq!(
            decide_import(Some(&implicit_only)).fallback_from,
            Some((ImportRoute::DmabufGpuConvert, "no_importable_modifier"))
        );
        assert_eq!(
            ImportRoute::for_memory(CaptureMemory::DmaBuf { modifier: TILED }),
            Some(ImportRoute::DmabufGpuConvert)
        );
        assert_eq!(ImportRoute::for_memory(CaptureMemory::GpuDevice), None);
    }

    fn epoch(route: ImportRoute, memory: CaptureMemory) -> EpochFacts {
        EpochFacts {
            negotiated: negotiated(1920, 1080),
            memory,
            route,
            gpu: GpuMatch::Unknown,
            cursor: CursorEvidence::SeparateMetadata,
            measured_fps: None,
        }
    }

    fn nvenc() -> EncoderCapability {
        EncoderCapability {
            backend: EncoderBackend::NativeNvenc,
            limits: EncoderBackend::NativeNvenc.contract(),
        }
    }

    fn request(preset: StreamingPreset) -> AdmissionRequest {
        AdmissionRequest {
            preset,
            requested_codec: VideoCodec::H265,
            width: 1920,
            height: 1080,
            max_fps: preset.pipeline_contract().max_fps,
        }
    }

    #[test]
    fn shm_host_copy_admits_auto_and_serves_speed_as_auto_without_a_rate() {
        let evidence = capture_source_evidence(&epoch(
            ImportRoute::HostCopyUpload,
            CaptureMemory::HostShared,
        ))
        .unwrap();
        assert_eq!(evidence.copy_path, CopyPath::HostCopy);
        assert_eq!(evidence.colour, SourceColour::UNKNOWN);
        let auto = admit_capture_source(request(StreamingPreset::Auto), &evidence, &nvenc());
        match auto {
            PresetAdmission::Admit(served) => {
                assert_eq!(served.pipeline, PipelineId::Auto);
                assert!(served.colour_assumed);
            }
            other => panic!("{other:?}"),
        }
        let speed = admit_capture_source(request(StreamingPreset::Speed), &evidence, &nvenc());
        assert!(
            matches!(&speed, PresetAdmission::Downgrade { served, .. } if served.pipeline == PipelineId::Auto),
            "{speed:?}"
        );
        for preset in [StreamingPreset::Grading, StreamingPreset::Hdr] {
            assert!(matches!(
                admit_capture_source(request(preset), &evidence, &nvenc()),
                PresetAdmission::Unavailable { .. }
            ));
        }
    }

    #[test]
    fn gpu_convert_reaches_admission_only_on_the_same_gpu() {
        let mut facts = epoch(
            ImportRoute::DmabufGpuConvert,
            CaptureMemory::DmaBuf { modifier: TILED },
        );
        assert_eq!(
            capture_source_evidence(&facts),
            Err(EvidenceRefusal::GpuNotSame)
        );
        facts.gpu = GpuMatch::Same;
        let evidence = capture_source_evidence(&facts).unwrap();
        assert_eq!(evidence.copy_path, CopyPath::GpuConvert);
        assert!(!evidence.copy_path.zero_copy());
    }

    #[test]
    fn capture_path_event_carries_the_adr_0014_fields() {
        let event = capture_path_event(
            CorrelationId::new("capenc-wayland").unwrap(),
            &epoch(ImportRoute::HostCopyUpload, CaptureMemory::HostShared),
            Some((ImportRoute::DmabufGpuConvert, "dmabuf_importer_unavailable")),
        )
        .unwrap();
        let fields = event.fields().as_map();
        let text = |key: &str| match fields.get(key) {
            Some(FieldValue::String(value)) => value.clone(),
            other => panic!("{key}: {other:?}"),
        };
        assert_eq!(text("backend"), "pipewire");
        assert_eq!(text("copy_path"), "host-copy");
        assert_eq!(text("memory"), "host-shared");
        assert_eq!(text("gpu_match"), "unknown");
        assert_eq!(text("pixel_format"), "bgrx8");
        assert_eq!(text("cursor"), "metadata");
        assert_eq!(text("source_transfer"), "unknown");
        assert_eq!(text("fallback_from"), "dmabuf-gl-blit");
        assert_eq!(fields.get("zero_copy"), Some(&FieldValue::Boolean(false)));
        assert_eq!(
            fields.get("bytes_per_frame"),
            Some(&FieldValue::Integer(1920 * 1080 * 4))
        );
        assert!(!fields.contains_key("modifier_class"));
    }
}
