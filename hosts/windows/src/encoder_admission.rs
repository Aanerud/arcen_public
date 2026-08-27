//! Pure Windows integration for aggregate encoder-set admission.
//!
//! Every candidate retains the topology planner's stable
//! `(adapter_luid, target_id)` binding. Adapter descriptions are used only to
//! enforce the operator allowlist; they are never used as output identity.

use std::collections::{BTreeMap, BTreeSet};

use arcen_media::video::{adaptive_codec_ladder, EncoderBackend};
use arcen_media::{
    admit_encoder_sets, ActivityClass, BitrateBudgetKbps,
    ChromaSubsampling as MediaChromaSubsampling, DirtyRatio, EncoderAdmissionError,
    EncoderAdmissionThresholds, EncoderBindingId, EncoderMeasurementAdapter, EncoderProbeFailure,
    EncoderProbeRequest, EncoderProbeTrace, EncoderSetAttemptOutcome, EncoderSetCandidate,
    EncoderSetDecision, MediaContractError, MediaStreamEpoch, RegionActivityProfile,
    RegionActivityProfiles, RegionAdmissionPriority, RegionEncoderBinding, RegionGeneration,
    RegionId, RegionMediaPlan, RegionMediaRoster, VideoCodec as MediaVideoCodec,
    VideoConfiguration,
};
use arcen_protocol::messages::MonitorQualityIntentMsg;
use arcen_protocol::{ChromaSubsampling, VideoCodec};

use crate::capenc::EncoderSelection;
use crate::multi_monitor_capenc::{
    build_pipeline_specs, MonitorPipelineSpec, MonitorPipelineTemplate, MultiCapencConfigError,
};
use crate::multi_monitor_topology::WindowsTopologyPlan;

#[derive(Debug)]
pub enum WindowsEncoderAdmissionError {
    Pipeline(MultiCapencConfigError),
    Media(MediaContractError),
    Region(arcen_media::RegionContractError),
    Admission(EncoderAdmissionError),
    NoAllowedAdapters,
    UnapprovedAdapter {
        monitor_id: u16,
        adapter_name: String,
    },
    ConcreteEncoderRequired,
    FullColorRequiresYuv444 {
        monitor_id: u16,
    },
    FullColorExceedsHardwareCeiling {
        required: usize,
        ceiling: usize,
    },
    SoftwareFallbackDisabled {
        requested: usize,
        ceiling: usize,
    },
    SoftwareGeometryUnsupported {
        monitor_id: u16,
        width: u32,
        height: u32,
    },
    MissingPipelineSpec {
        monitor_id: u16,
    },
    PipelineGeometryMismatch {
        monitor_id: u16,
    },
    NonConcreteEncoder {
        monitor_id: u16,
    },
}

impl std::fmt::Display for WindowsEncoderAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pipeline(error) => write!(formatter, "Windows pipeline planning failed: {error}"),
            Self::Media(error) => write!(formatter, "Windows media contract failed: {error}"),
            Self::Region(error) => write!(formatter, "Windows region contract failed: {error}"),
            Self::Admission(error) => write!(formatter, "aggregate encoder admission failed: {error}"),
            Self::NoAllowedAdapters => {
                formatter.write_str("multi-monitor encoder admission has no allowed adapters")
            }
            Self::UnapprovedAdapter {
                monitor_id,
                adapter_name,
            } => write!(
                formatter,
                "monitor {monitor_id} is bound to unapproved adapter {adapter_name:?}"
            ),
            Self::ConcreteEncoderRequired => formatter.write_str(
                "Windows multi-monitor encoder admission requires a concrete NVENC or OpenH264 primary plan",
            ),
            Self::FullColorRequiresYuv444 { monitor_id } => write!(
                formatter,
                "monitor {monitor_id} requires full color but the host plan is not YUV444"
            ),
            Self::FullColorExceedsHardwareCeiling { required, ceiling } => write!(
                formatter,
                "{required} full-color monitors exceed the operator hardware ceiling {ceiling}"
            ),
            Self::SoftwareFallbackDisabled { requested, ceiling } => write!(
                formatter,
                "{requested} monitors exceed the operator hardware ceiling {ceiling} and software fallback is disabled"
            ),
            Self::SoftwareGeometryUnsupported {
                monitor_id,
                width,
                height,
            } => write!(
                formatter,
                "monitor {monitor_id} exact geometry {width}x{height} is unsupported by OpenH264"
            ),
            Self::MissingPipelineSpec { monitor_id } => {
                write!(formatter, "monitor {monitor_id} has no matching pipeline spec")
            }
            Self::PipelineGeometryMismatch { monitor_id } => write!(
                formatter,
                "monitor {monitor_id} pipeline geometry differs from the topology plan"
            ),
            Self::NonConcreteEncoder { monitor_id } => write!(
                formatter,
                "monitor {monitor_id} retained a missing or automatic encoder selection"
            ),
        }
    }
}

impl std::error::Error for WindowsEncoderAdmissionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pipeline(error) => Some(error),
            Self::Media(error) => Some(error),
            Self::Region(error) => Some(error),
            Self::Admission(error) => Some(error),
            _ => None,
        }
    }
}

impl From<MultiCapencConfigError> for WindowsEncoderAdmissionError {
    fn from(error: MultiCapencConfigError) -> Self {
        Self::Pipeline(error)
    }
}

impl From<MediaContractError> for WindowsEncoderAdmissionError {
    fn from(error: MediaContractError) -> Self {
        Self::Media(error)
    }
}

impl From<arcen_media::RegionContractError> for WindowsEncoderAdmissionError {
    fn from(error: arcen_media::RegionContractError) -> Self {
        Self::Region(error)
    }
}

impl From<EncoderAdmissionError> for WindowsEncoderAdmissionError {
    fn from(error: EncoderAdmissionError) -> Self {
        Self::Admission(error)
    }
}

#[derive(Clone, Debug)]
struct PlannedEncoderSet {
    candidate: EncoderSetCandidate,
    specs: Vec<MonitorPipelineSpec>,
    template: MonitorPipelineTemplate,
}

/// Ordered Windows candidate sets plus their exact stable output bindings.
#[derive(Clone, Debug)]
pub struct WindowsEncoderAdmissionPlan {
    sets: Vec<PlannedEncoderSet>,
    profiles: RegionActivityProfiles,
}

impl WindowsEncoderAdmissionPlan {
    #[allow(dead_code)]
    #[must_use]
    pub fn candidate_count(&self) -> usize {
        self.sets.len()
    }

    #[must_use]
    pub fn primary_specs(&self) -> &[MonitorPipelineSpec] {
        &self.sets[0].specs
    }

    #[must_use]
    pub const fn activity_profiles(&self) -> &RegionActivityProfiles {
        &self.profiles
    }

    #[allow(dead_code)]
    #[must_use]
    pub fn selected_specs(&self, decision: &EncoderSetDecision) -> Option<&[MonitorPipelineSpec]> {
        let index = decision.selected_candidate_index()?;
        self.sets.get(index).map(|set| set.specs.as_slice())
    }

    /// The negotiated media roster of the encoder set this decision accepted.
    ///
    /// This is the authority for every admitted region's published bitrate:
    /// `multi_monitor_gate::build_applied_capability` reads
    /// `RegionMediaPlan::bitrate_budget` off it verbatim rather than
    /// recomputing a nominal budget from the applied geometry, so the wire can
    /// only ever carry a budget this admission actually accepted.
    #[must_use]
    pub fn selected_media_roster(
        &self,
        decision: &EncoderSetDecision,
    ) -> Option<&RegionMediaRoster> {
        let index = decision.selected_candidate_index()?;
        self.sets.get(index).map(|set| set.candidate.roster())
    }

    #[must_use]
    pub fn selected_template(
        &self,
        decision: &EncoderSetDecision,
    ) -> Option<&MonitorPipelineTemplate> {
        let index = decision.selected_candidate_index()?;
        self.sets.get(index).map(|set| &set.template)
    }

    #[allow(dead_code)]
    /// Runs the shared concurrent measurement framework over these exact
    /// stable output/backend bindings.
    ///
    /// # Errors
    ///
    /// Returns shared validation errors before an adapter is invoked.
    pub fn admit<A: EncoderMeasurementAdapter>(
        &self,
        profiles: &RegionActivityProfiles,
        thresholds: EncoderAdmissionThresholds,
        adapter: &A,
    ) -> Result<EncoderSetDecision, WindowsEncoderAdmissionError> {
        let candidates = self.sets.iter().map(|set| set.candidate.clone()).collect();
        Ok(admit_encoder_sets(
            candidates, profiles, thresholds, adapter,
        )?)
    }

    /// Executes production bounded child probes against a freshly resolved
    /// stable-output inventory.
    ///
    /// # Errors
    ///
    /// Returns shared validation failures. Per-child failures remain candidate
    /// outcomes so measured reassignment can continue.
    pub fn admit_runtime(
        &self,
        thresholds: EncoderAdmissionThresholds,
        inventory: &crate::multi_monitor_topology::PhysicalOutputInventory,
    ) -> Result<EncoderSetDecision, WindowsEncoderAdmissionError> {
        let adapter = WindowsChildProbeAdapter {
            plan: self,
            inventory,
        };
        self.admit(&self.profiles, thresholds, &adapter)
    }
}

struct WindowsChildProbeAdapter<'a> {
    plan: &'a WindowsEncoderAdmissionPlan,
    inventory: &'a crate::multi_monitor_topology::PhysicalOutputInventory,
}

impl EncoderMeasurementAdapter for WindowsChildProbeAdapter<'_> {
    fn measure(
        &self,
        request: &EncoderProbeRequest,
    ) -> Result<EncoderProbeTrace, EncoderProbeFailure> {
        let set = self.plan.sets.get(request.candidate_index).ok_or_else(|| {
            EncoderProbeFailure::invalid("probe candidate index is outside the Windows plan")
        })?;
        let spec = set
            .specs
            .iter()
            .find(|spec| spec.session_monitor_id == request.plan.session_monitor_id)
            .ok_or_else(|| {
                EncoderProbeFailure::invalid("Windows probe pipeline spec is missing")
            })?;
        let expected_binding = set
            .candidate
            .binding(request.plan.session_monitor_id)
            .ok_or_else(|| EncoderProbeFailure::invalid("Windows probe binding is missing"))?;
        if expected_binding != &request.binding_id {
            return Err(EncoderProbeFailure::invalid(
                "Windows probe binding differs from the planned candidate",
            ));
        }
        let mut resolved = crate::multi_monitor_capenc::resolve_pipeline_specs(
            std::slice::from_ref(spec),
            self.inventory,
            &set.template,
        )
        .map_err(|error| EncoderProbeFailure::context_open(error.to_string()))?;
        let pipeline = resolved
            .pop()
            .ok_or_else(|| EncoderProbeFailure::invalid("Windows probe resolution is empty"))?;
        let mut command = crate::capenc::admission_probe_command(&pipeline.config)
            .map_err(EncoderProbeFailure::context_open)?;
        arcen_capenc::admission_probe::run_admission_probe_child(&mut command, request)
    }
}

/// Builds measured-admission candidates from the exact Windows topology and
/// capture planner.
///
/// Candidate zero honors the operator hardware ceiling. Later candidates
/// progressively move only non-full-color regions to exact OpenH264/YUV420
/// plans. The operator adapter allowlist is rechecked against every stable
/// topology binding before any candidate is returned.
///
/// A host whose resolved session encoder is the OpenH264 software path
/// (`capenc::EncoderSelection::SoftwareH264` — every host without direct NVENC)
/// has no hardware encode sessions at all, so it plans exactly one
/// all-software candidate. That is the same exact OpenH264/YUV420 region plan
/// this function already produces for hardware-exhausted regions, and it is
/// still subject to measured runtime admission, so a software host is never
/// admitted on paper alone. Full-color (4:4:4) regions remain refused there:
/// the software fallback encodes nothing but H.264 4:2:0.
///
/// # Errors
///
/// Returns an allowlist, quality, fallback geometry, pipeline, or shared media
/// contract error.
pub fn plan_encoder_sets(
    topology: &WindowsTopologyPlan,
    template: &MonitorPipelineTemplate,
    quality_intents: &BTreeMap<String, MonitorQualityIntentMsg>,
    allowed_adapters: &[String],
    hardware_context_ceiling: Option<u8>,
    allow_software_fallback: bool,
) -> Result<WindowsEncoderAdmissionPlan, WindowsEncoderAdmissionError> {
    if allowed_adapters.is_empty() {
        return Err(WindowsEncoderAdmissionError::NoAllowedAdapters);
    }
    for monitor in &topology.monitors {
        if !allowed_adapters
            .iter()
            .any(|allowed| allowed.eq_ignore_ascii_case(&monitor.adapter_name))
        {
            return Err(WindowsEncoderAdmissionError::UnapprovedAdapter {
                monitor_id: monitor.session_monitor_id.get(),
                adapter_name: monitor.adapter_name.clone(),
            });
        }
    }
    let hardware_encode = match template.encoder {
        Some(EncoderSelection::Nvenc) => true,
        Some(EncoderSelection::SoftwareH264) => false,
        Some(EncoderSelection::Auto) | None => {
            return Err(WindowsEncoderAdmissionError::ConcreteEncoderRequired);
        }
    };

    let monitor_count = topology.monitors.len();
    let hardware_max = if hardware_encode {
        hardware_context_ceiling
            .map_or(monitor_count, usize::from)
            .min(monitor_count)
    } else {
        0
    };
    let full_color_required = topology
        .monitors
        .iter()
        .filter(|monitor| quality_intent(monitor, quality_intents).is_full_color())
        .count();
    if full_color_required > hardware_max {
        return Err(
            WindowsEncoderAdmissionError::FullColorExceedsHardwareCeiling {
                required: full_color_required,
                ceiling: hardware_max,
            },
        );
    }
    // `allow_software_fallback` is the operator's permission to *give up*
    // hardware sessions this host actually has. A host with no hardware
    // encoder never had any to give up, so its single all-software set is not
    // a fallback and does not consult that permission.
    if hardware_encode && monitor_count > hardware_max && !allow_software_fallback {
        return Err(WindowsEncoderAdmissionError::SoftwareFallbackDisabled {
            requested: monitor_count,
            ceiling: hardware_max,
        });
    }

    let hardware_counts = if hardware_encode && allow_software_fallback {
        (full_color_required..=hardware_max)
            .rev()
            .collect::<Vec<_>>()
    } else {
        vec![hardware_max]
    };
    let codec_templates = adaptive_codec_templates(template);
    let mut sets = Vec::with_capacity(hardware_counts.len() * codec_templates.len());
    // Exhaust same-GPU codec options before moving any region to OpenH264.
    for hardware_count in hardware_counts {
        for codec_template in &codec_templates {
            let specs = match specs_for_hardware_count(
                topology,
                codec_template,
                quality_intents,
                hardware_count,
            ) {
                Ok(specs) => specs,
                Err(WindowsEncoderAdmissionError::SoftwareGeometryUnsupported { .. })
                    if !sets.is_empty() =>
                {
                    // A lower-hardware fallback is optional once a stronger
                    // candidate exists. Do not invalidate the usable all-NVENC
                    // or mixed candidate merely because an additional
                    // OpenH264 degradation cannot represent a large monitor.
                    continue;
                }
                Err(error) => return Err(error),
            };
            let candidate = candidate_from_specs(topology, codec_template, &specs)?;
            sets.push(PlannedEncoderSet {
                candidate,
                specs,
                template: codec_template.clone(),
            });
        }
    }
    let profiles = activity_profiles(topology, quality_intents, &sets)?;
    Ok(WindowsEncoderAdmissionPlan { sets, profiles })
}

fn adaptive_codec_templates(template: &MonitorPipelineTemplate) -> Vec<MonitorPipelineTemplate> {
    if template.encoder != Some(EncoderSelection::Nvenc)
        || template.video_selection
            != arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance
    {
        return vec![template.clone()];
    }
    adaptive_codec_ladder(crate::capenc::media_codec(template.codec))
        .iter()
        .map(|codec| {
            let mut candidate = template.clone();
            candidate.codec = crate::capenc::protocol_codec(*codec);
            candidate.chroma = ChromaSubsampling::Yuv420;
            if *codec == MediaVideoCodec::H264 {
                candidate.bit_depth = arcen_media::BitDepth::Eight;
            }
            candidate
        })
        .collect()
}

fn activity_profiles(
    topology: &WindowsTopologyPlan,
    quality_intents: &BTreeMap<String, MonitorQualityIntentMsg>,
    sets: &[PlannedEncoderSet],
) -> Result<RegionActivityProfiles, WindowsEncoderAdmissionError> {
    let generation = RegionGeneration::new(topology.generation.get())?;
    let mut profiles = Vec::with_capacity(topology.monitors.len());
    for monitor in &topology.monitors {
        let full_color = quality_intent(monitor, quality_intents).is_full_color();
        let active = monitor.primary || full_color;
        let target_fps = if active {
            sets.iter()
                .filter_map(|set| {
                    set.candidate
                        .roster()
                        .plan(monitor.session_monitor_id)
                        .map(|plan| plan.fps)
                })
                .min()
                .ok_or(WindowsEncoderAdmissionError::MissingPipelineSpec {
                    monitor_id: monitor.session_monitor_id.get(),
                })?
        } else {
            1
        };
        profiles.push(RegionActivityProfile {
            session_monitor_id: monitor.session_monitor_id,
            region_generation: generation,
            region_id: RegionId::new(u32::from(monitor.session_monitor_id.get()))?,
            activity_class: if active {
                ActivityClass::FullMotion
            } else {
                ActivityClass::Idle
            },
            dirty_ratio: if active {
                DirtyRatio::FULL
            } else {
                DirtyRatio::ZERO
            },
            target_fps,
            priority: if full_color {
                RegionAdmissionPriority::FullColorRequired
            } else {
                RegionAdmissionPriority::Standard
            },
        });
    }
    Ok(RegionActivityProfiles::new(profiles)?)
}

pub fn emit_admission_telemetry(decision: &EncoderSetDecision) {
    for attempt in decision.attempts() {
        match &attempt.outcome {
            EncoderSetAttemptOutcome::Passed(measurements)
            | EncoderSetAttemptOutcome::ThresholdFailed { measurements, .. } => {
                tracing::info!(
                    candidate = attempt.candidate_index,
                    passed = attempt.outcome.passed(),
                    p50_encode_ms = ?measurements.p50_encode_latency.as_millis(),
                    p95_encode_ms = ?measurements.p95_encode_latency.as_millis(),
                    p50_queue_ms = ?measurements.p50_queue_age.as_millis(),
                    p95_queue_ms = ?measurements.p95_queue_age.as_millis(),
                    delivered_millifps = measurements.delivered_millifps,
                    fairness_basis_points = measurements.fairness_basis_points,
                    "aggregate encoder admission measurement"
                );
                for region in &measurements.per_region {
                    tracing::info!(
                        candidate = attempt.candidate_index,
                        monitor_id = region.session_monitor_id.get(),
                        p50_encode_ms = ?region.p50_encode_latency.as_millis(),
                        p95_encode_ms = ?region.p95_encode_latency.as_millis(),
                        p50_queue_ms = ?region.p50_queue_age.as_millis(),
                        p95_queue_ms = ?region.p95_queue_age.as_millis(),
                        delivered_millifps = region.delivered_millifps,
                        delivery_ratio_basis_points = region.delivery_ratio_basis_points,
                        "region encoder admission measurement"
                    );
                }
            }
            EncoderSetAttemptOutcome::ProbeFailed { failures } => {
                tracing::warn!(
                    candidate = attempt.candidate_index,
                    failures = failures.len(),
                    "aggregate encoder admission probe failed"
                );
            }
        }
    }
    tracing::info!(
        decision = match decision {
            EncoderSetDecision::Accept { .. } => "accept",
            EncoderSetDecision::Reassign { .. } => "reassign",
            EncoderSetDecision::Reject { .. } => "reject",
        },
        selected_candidate = ?decision.selected_candidate_index(),
        "aggregate encoder admission decision"
    );
}

trait QualityIntentExt {
    fn is_full_color(self) -> bool;
}

impl QualityIntentExt for MonitorQualityIntentMsg {
    fn is_full_color(self) -> bool {
        self == Self::FullColorRequired
    }
}

fn quality_intent(
    monitor: &crate::multi_monitor_topology::WindowsMonitorPlan,
    quality_intents: &BTreeMap<String, MonitorQualityIntentMsg>,
) -> MonitorQualityIntentMsg {
    quality_intents
        .get(&monitor.client_display_id)
        .copied()
        .unwrap_or_default()
}

fn specs_for_hardware_count(
    topology: &WindowsTopologyPlan,
    template: &MonitorPipelineTemplate,
    quality_intents: &BTreeMap<String, MonitorQualityIntentMsg>,
    hardware_count: usize,
) -> Result<Vec<MonitorPipelineSpec>, WindowsEncoderAdmissionError> {
    let mut specs = build_pipeline_specs(topology)?;
    let mut priority = topology.monitors.iter().enumerate().collect::<Vec<_>>();
    priority.sort_by_key(|(index, monitor)| {
        let intent = quality_intent(monitor, quality_intents);
        let requires_hardware =
            intent.is_full_color() || !software_geometry_supported(monitor.width, monitor.height);
        (
            !requires_hardware,
            !intent.is_full_color(),
            !monitor.primary,
            std::cmp::Reverse(u64::from(monitor.width) * u64::from(monitor.height)),
            *index,
        )
    });
    let hardware_ids = priority
        .into_iter()
        .take(hardware_count)
        .map(|(_, monitor)| monitor.session_monitor_id)
        .collect::<BTreeSet<_>>();

    for (monitor, spec) in topology.monitors.iter().zip(&mut specs) {
        let intent = quality_intent(monitor, quality_intents);
        if hardware_ids.contains(&monitor.session_monitor_id) {
            if intent.is_full_color() && template.chroma != ChromaSubsampling::Yuv444 {
                return Err(WindowsEncoderAdmissionError::FullColorRequiresYuv444 {
                    monitor_id: monitor.session_monitor_id.get(),
                });
            }
            spec.set_media_policy(
                EncoderSelection::Nvenc,
                template.codec,
                if intent == MonitorQualityIntentMsg::BandwidthOptimized {
                    ChromaSubsampling::Yuv420
                } else {
                    template.chroma
                },
            );
            continue;
        }
        if intent.is_full_color() {
            return Err(
                WindowsEncoderAdmissionError::FullColorExceedsHardwareCeiling {
                    required: 1,
                    ceiling: hardware_count,
                },
            );
        }
        if !software_geometry_supported(monitor.width, monitor.height) {
            return Err(WindowsEncoderAdmissionError::SoftwareGeometryUnsupported {
                monitor_id: monitor.session_monitor_id.get(),
                width: monitor.width,
                height: monitor.height,
            });
        }
        spec.set_media_policy(
            EncoderSelection::SoftwareH264,
            VideoCodec::H264,
            ChromaSubsampling::Yuv420,
        );
    }
    Ok(specs)
}

fn software_geometry_supported(width: u32, height: u32) -> bool {
    let limits = EncoderBackend::OpenH264.contract();
    width != 0
        && height != 0
        && width <= limits.max_width
        && height <= limits.max_height
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
}

fn candidate_from_specs(
    topology: &WindowsTopologyPlan,
    template: &MonitorPipelineTemplate,
    specs: &[MonitorPipelineSpec],
) -> Result<EncoderSetCandidate, WindowsEncoderAdmissionError> {
    let epoch = MediaStreamEpoch::new(topology.generation.get())?;
    let mut plans = Vec::with_capacity(topology.monitors.len());
    let mut bindings = Vec::with_capacity(topology.monitors.len());
    for monitor in &topology.monitors {
        let spec = specs
            .iter()
            .find(|spec| spec.session_monitor_id == monitor.session_monitor_id)
            .ok_or(WindowsEncoderAdmissionError::MissingPipelineSpec {
                monitor_id: monitor.session_monitor_id.get(),
            })?;
        if spec.width != monitor.width || spec.height != monitor.height {
            return Err(WindowsEncoderAdmissionError::PipelineGeometryMismatch {
                monitor_id: monitor.session_monitor_id.get(),
            });
        }
        let (backend, codec, chroma) = match (spec.encoder, spec.codec, spec.chroma) {
            (Some(EncoderSelection::Nvenc), Some(codec), Some(chroma)) => {
                (EncoderBackend::NativeNvenc, codec, chroma)
            }
            (Some(EncoderSelection::SoftwareH264), Some(codec), Some(chroma)) => {
                (EncoderBackend::OpenH264, codec, chroma)
            }
            _ => {
                return Err(WindowsEncoderAdmissionError::NonConcreteEncoder {
                    monitor_id: monitor.session_monitor_id.get(),
                });
            }
        };
        let software = backend == EncoderBackend::OpenH264;
        let fps = if software {
            template
                .fps
                .min(EncoderBackend::OpenH264.contract().max_fps)
        } else {
            template.fps
        };
        plans.push(RegionMediaPlan::new(
            monitor.session_monitor_id,
            epoch,
            backend,
            VideoConfiguration {
                codec: media_codec(codec),
                chroma: media_chroma(chroma),
                bit_depth: if software {
                    arcen_media::BitDepth::Eight
                } else {
                    template.bit_depth
                },
                range: template.color_range,
                matrix: if software && template.color_matrix.is_identity() {
                    arcen_media::ColorMatrix::Bt709
                } else {
                    template.color_matrix
                },
                ..VideoConfiguration::legacy_h264()
            },
            spec.width,
            spec.height,
            fps,
            BitrateBudgetKbps::nominal_for_geometry(spec.width, spec.height, fps),
        )?);
        bindings.push(RegionEncoderBinding {
            session_monitor_id: monitor.session_monitor_id,
            binding_id: EncoderBindingId::new(format!(
                "windows-dxgi:luid={:08x}:{:08x}:target={}:backend={}",
                monitor.adapter_luid.high_part,
                monitor.adapter_luid.low_part,
                monitor.target_id,
                backend.ready_token()
            ))?,
        });
    }
    let roster = RegionMediaRoster::new(plans)?;
    Ok(EncoderSetCandidate::new(roster, bindings)?)
}

const fn media_codec(codec: VideoCodec) -> MediaVideoCodec {
    match codec {
        VideoCodec::Jpeg => MediaVideoCodec::Jpeg,
        VideoCodec::H264 => MediaVideoCodec::H264,
        VideoCodec::H265 => MediaVideoCodec::H265,
        VideoCodec::Vp9 => MediaVideoCodec::Vp9,
        VideoCodec::Av1 => MediaVideoCodec::Av1,
    }
}

const fn media_chroma(chroma: ChromaSubsampling) -> MediaChromaSubsampling {
    match chroma {
        ChromaSubsampling::Yuv420 => MediaChromaSubsampling::Yuv420,
        ChromaSubsampling::Yuv422 => MediaChromaSubsampling::Yuv422,
        ChromaSubsampling::Yuv444 => MediaChromaSubsampling::Yuv444,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use arcen_media::{
        BitDepth, ColorMatrix, ColorRange, EncoderProbeSample, LogicalPoint, LogicalRect,
        LogicalSize, Rotation, Scale120, SessionMonitorId, TopologyGeneration,
    };
    use arcen_protocol::messages::CursorMode;
    use arcen_telemetry::CorrelationId;

    use super::*;
    use crate::multi_monitor_topology::WindowsMonitorPlan;
    use crate::nvapi::AdapterLuid;

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("monitor id")
    }

    fn monitor(id: u16, target_id: u32, primary: bool, adapter_name: &str) -> WindowsMonitorPlan {
        WindowsMonitorPlan {
            session_monitor_id: sid(id),
            client_display_id: format!("display-{id}"),
            adapter_luid: AdapterLuid {
                low_part: 0x1020_3040,
                high_part: 0x5060_7080,
            },
            target_id,
            adapter_output_index: target_id,
            adapter_name: adapter_name.to_owned(),
            global_index: target_id,
            device_name: format!(r"\\.\DISPLAY{}", target_id + 1),
            x: 0,
            y: 0,
            width: 1_792,
            height: 1_072,
            mode_width: 1_792,
            mode_height: 1_072,
            logical_rect: LogicalRect::new(
                LogicalPoint::from_pixels(0, 0).expect("origin"),
                LogicalSize::from_pixels(1_792, 1_072).expect("size"),
            )
            .expect("rect"),
            scale: Scale120::new(120).expect("scale"),
            refresh_hz: 60,
            rotation: Rotation::Degrees0,
            primary,
        }
    }

    fn topology(adapter_name: &str) -> WindowsTopologyPlan {
        WindowsTopologyPlan {
            generation: TopologyGeneration::new(9).expect("generation"),
            desktop_x: 0,
            desktop_y: 0,
            desktop_width: 3_584,
            desktop_height: 1_072,
            monitors: vec![
                monitor(1, 3, true, adapter_name),
                monitor(2, 7, false, adapter_name),
            ],
            requires_custom_timing: false,
        }
    }

    fn template() -> MonitorPipelineTemplate {
        MonitorPipelineTemplate {
            codec: VideoCodec::H265,
            chroma: ChromaSubsampling::Yuv444,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            intent: arcen_media::EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            fps: 60,
            encoder: Some(EncoderSelection::Nvenc),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            cursor_mode: CursorMode::Local,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    fn intents() -> BTreeMap<String, MonitorQualityIntentMsg> {
        BTreeMap::from([
            (
                "display-1".to_owned(),
                MonitorQualityIntentMsg::BandwidthOptimized,
            ),
            (
                "display-2".to_owned(),
                MonitorQualityIntentMsg::FullColorRequired,
            ),
        ])
    }

    struct StallPrimaryAdapter;

    impl EncoderMeasurementAdapter for StallPrimaryAdapter {
        fn measure(
            &self,
            request: &EncoderProbeRequest,
        ) -> Result<EncoderProbeTrace, EncoderProbeFailure> {
            let encode_latency = if request.candidate_index == 0 {
                Duration::from_millis(865)
            } else {
                Duration::from_millis(5)
            };
            Ok(EncoderProbeTrace {
                elapsed: request.measurement_window,
                samples: request
                    .sample_frames
                    .iter()
                    .map(|frame| EncoderProbeSample {
                        sequence: frame.sequence,
                        kind: frame.kind,
                        queue_age: Duration::from_millis(1),
                        encode_latency,
                        delivered: true,
                    })
                    .collect(),
            })
        }
    }

    fn thresholds() -> EncoderAdmissionThresholds {
        EncoderAdmissionThresholds {
            measurement_window: Duration::from_secs(1),
            max_probe_duration: Duration::from_secs(10),
            warmup_frames: 2,
            max_sample_frames_per_region: 120,
            max_p95_encode_latency: Duration::from_millis(20),
            max_p95_queue_age: Duration::from_millis(10),
            min_delivered_fps_basis_points: 9_000,
            min_fairness_basis_points: 9_500,
        }
    }

    #[test]
    fn full_color_secondary_keeps_nvenc_in_every_reassignment() {
        let plan = plan_encoder_sets(
            &topology("Approved GPU"),
            &template(),
            &intents(),
            &["approved gpu".to_owned()],
            None,
            true,
        )
        .expect("plan");
        assert_eq!(plan.candidate_count(), 2);
        assert_eq!(
            plan.sets[1].specs[0].encoder,
            Some(EncoderSelection::SoftwareH264)
        );
        assert_eq!(plan.sets[1].specs[1].encoder, Some(EncoderSelection::Nvenc));
        assert_eq!(
            plan.sets[1].specs[1].chroma,
            Some(ChromaSubsampling::Yuv444)
        );
    }

    #[test]
    fn unapproved_gpu_is_rejected_before_candidate_creation() {
        let error = plan_encoder_sets(
            &topology("Compute Reserved GPU"),
            &template(),
            &intents(),
            &["Approved GPU".to_owned()],
            None,
            true,
        )
        .expect_err("unapproved adapter must fail closed");
        assert!(matches!(
            error,
            WindowsEncoderAdmissionError::UnapprovedAdapter { .. }
        ));
    }

    #[test]
    fn binding_uses_stable_luid_and_target_not_enumeration_index() {
        let topology = topology("Approved GPU");
        let plan = plan_encoder_sets(
            &topology,
            &template(),
            &intents(),
            &["Approved GPU".to_owned()],
            Some(1),
            true,
        )
        .expect("plan");
        let binding = plan.sets[0]
            .candidate
            .binding(sid(2))
            .expect("binding")
            .as_str();
        assert!(binding.contains("luid=50607080:10203040"));
        assert!(binding.contains("target=7"));
        assert!(!binding.contains("global"));
        assert!(!binding.contains("Approved GPU"));
    }

    #[test]
    fn generated_profiles_distinguish_primary_full_color_and_idle_siblings() {
        let mut topology = topology("Approved GPU");
        topology.desktop_width = 5_376;
        topology.monitors.push(monitor(3, 9, false, "Approved GPU"));
        let plan = plan_encoder_sets(
            &topology,
            &template(),
            &intents(),
            &["Approved GPU".to_owned()],
            None,
            true,
        )
        .expect("plan");

        let primary = plan.activity_profiles().profile(sid(1)).expect("primary");
        assert_eq!(primary.activity_class, ActivityClass::FullMotion);
        assert_eq!(primary.dirty_ratio, DirtyRatio::FULL);
        assert_eq!(primary.priority, RegionAdmissionPriority::Standard);

        let full_color = plan
            .activity_profiles()
            .profile(sid(2))
            .expect("full color");
        assert_eq!(full_color.activity_class, ActivityClass::FullMotion);
        assert_eq!(full_color.dirty_ratio, DirtyRatio::FULL);
        assert_eq!(
            full_color.priority,
            RegionAdmissionPriority::FullColorRequired
        );

        let idle = plan.activity_profiles().profile(sid(3)).expect("idle");
        assert_eq!(idle.activity_class, ActivityClass::Idle);
        assert_eq!(idle.dirty_ratio, DirtyRatio::ZERO);
        assert_eq!(idle.target_fps, 1);
    }

    #[test]
    fn measured_stall_selects_reassigned_specs_before_startup() {
        let plan = plan_encoder_sets(
            &topology("Approved GPU"),
            &template(),
            &intents(),
            &["Approved GPU".to_owned()],
            None,
            true,
        )
        .expect("plan");
        let decision = plan
            .admit(plan.activity_profiles(), thresholds(), &StallPrimaryAdapter)
            .expect("decision");
        assert!(matches!(
            decision,
            EncoderSetDecision::Reassign {
                selected_candidate_index: 1,
                ..
            }
        ));
        let selected = plan.selected_specs(&decision).expect("reassigned specs");
        assert_eq!(selected[0].encoder, Some(EncoderSelection::SoftwareH264));
        assert_eq!(selected[1].encoder, Some(EncoderSelection::Nvenc));
        assert!(matches!(
            decision.attempts()[0].outcome,
            EncoderSetAttemptOutcome::ThresholdFailed { .. }
        ));
    }
    fn software_template() -> MonitorPipelineTemplate {
        MonitorPipelineTemplate {
            codec: VideoCodec::H264,
            chroma: ChromaSubsampling::Yuv420,
            bit_depth: BitDepth::Eight,
            color_range: ColorRange::Limited,
            color_matrix: ColorMatrix::Bt709,
            intent: arcen_media::EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            fps: 60,
            encoder: Some(EncoderSelection::SoftwareH264),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            cursor_mode: CursorMode::Local,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
    }

    fn standard_intents() -> BTreeMap<String, MonitorQualityIntentMsg> {
        BTreeMap::from([
            (
                "display-1".to_owned(),
                MonitorQualityIntentMsg::BandwidthOptimized,
            ),
            (
                "display-2".to_owned(),
                MonitorQualityIntentMsg::BandwidthOptimized,
            ),
        ])
    }

    #[test]
    fn software_overflow_keeps_large_displays_on_nvenc() {
        let mut primary = monitor(1, 3, true, "Approved GPU");
        primary.width = 1_512;
        primary.height = 950;
        primary.mode_width = 1_512;
        primary.mode_height = 950;
        let mut landscape = monitor(2, 7, false, "Approved GPU");
        landscape.width = 2_560;
        landscape.height = 1_440;
        landscape.mode_width = 2_560;
        landscape.mode_height = 1_440;
        let mut portrait = monitor(3, 8, false, "Approved GPU");
        portrait.width = 1_440;
        portrait.height = 2_560;
        portrait.mode_width = 2_560;
        portrait.mode_height = 1_440;
        portrait.rotation = Rotation::Degrees270;
        let topology = WindowsTopologyPlan {
            generation: TopologyGeneration::new(10).expect("generation"),
            desktop_x: -5_120,
            desktop_y: -1_870,
            desktop_width: 6_632,
            desktop_height: 2_820,
            monitors: vec![primary, landscape, portrait],
            requires_custom_timing: true,
        };
        let intents = BTreeMap::from([
            (
                "display-1".to_owned(),
                MonitorQualityIntentMsg::BandwidthOptimized,
            ),
            (
                "display-2".to_owned(),
                MonitorQualityIntentMsg::BandwidthOptimized,
            ),
            (
                "display-3".to_owned(),
                MonitorQualityIntentMsg::BandwidthOptimized,
            ),
        ]);

        let plan = plan_encoder_sets(
            &topology,
            &template(),
            &intents,
            &["approved gpu".to_owned()],
            Some(2),
            true,
        )
        .expect("two NVENC plus one software candidate");

        assert_eq!(plan.candidate_count(), 1);
        assert_eq!(
            plan.sets[0].specs[0].encoder,
            Some(EncoderSelection::SoftwareH264)
        );
        assert_eq!(plan.sets[0].specs[1].encoder, Some(EncoderSelection::Nvenc));
        assert_eq!(plan.sets[0].specs[2].encoder, Some(EncoderSelection::Nvenc));
    }

    /// Regression for `regress-comaintenance-multimon`: a host whose resolved
    /// session encoder is the OpenH264 software path and has no hardware
    /// encode sessions, so it plans exactly one all-software candidate rather
    /// than being refused outright for not being NVENC.
    #[test]
    fn software_only_host_plans_one_all_software_candidate() {
        let plan = plan_encoder_sets(
            &topology("Microsoft Basic Render Driver"),
            &software_template(),
            &standard_intents(),
            &["microsoft basic render driver".to_owned()],
            None,
            false,
        )
        .expect("plan");
        assert_eq!(plan.candidate_count(), 1);
        for spec in &plan.sets[0].specs {
            assert_eq!(spec.encoder, Some(EncoderSelection::SoftwareH264));
            assert_eq!(spec.codec, Some(VideoCodec::H264));
            assert_eq!(spec.chroma, Some(ChromaSubsampling::Yuv420));
        }
        let roster = plan.sets[0].candidate.roster();
        for id in [sid(1), sid(2)] {
            assert_eq!(
                roster.plan(id).expect("region plan").backend,
                EncoderBackend::OpenH264
            );
        }
    }

    /// The software ceiling is honest in both directions: a 4:4:4 client
    /// intent is refused rather than silently downgraded, because the
    /// software fallback encodes nothing but H.264 4:2:0.
    #[test]
    fn software_only_host_refuses_a_full_color_intent() {
        let error = plan_encoder_sets(
            &topology("Microsoft Basic Render Driver"),
            &software_template(),
            &intents(),
            &["Microsoft Basic Render Driver".to_owned()],
            None,
            true,
        )
        .expect_err("full color must fail closed on a software host");
        assert!(
            matches!(
                error,
                WindowsEncoderAdmissionError::FullColorExceedsHardwareCeiling {
                    required: 1,
                    ceiling: 0,
                }
            ),
            "unexpected error: {error}"
        );
    }

    /// `allow_software_fallback` is permission to give up hardware sessions
    /// this host has. A host with no hardware encoder never had any, so the
    /// operator policy does not apply to it.
    #[test]
    fn software_only_host_does_not_consult_the_hardware_fallback_policy() {
        for allow_software_fallback in [false, true] {
            let plan = plan_encoder_sets(
                &topology("Microsoft Basic Render Driver"),
                &software_template(),
                &standard_intents(),
                &["Microsoft Basic Render Driver".to_owned()],
                Some(2),
                allow_software_fallback,
            )
            .expect("plan");
            assert_eq!(plan.candidate_count(), 1);
            assert_eq!(
                plan.sets[0].specs[0].encoder,
                Some(EncoderSelection::SoftwareH264)
            );
        }
    }

    #[test]
    fn software_only_host_still_fails_closed_on_an_unapproved_adapter() {
        let error = plan_encoder_sets(
            &topology("Compute Reserved GPU"),
            &software_template(),
            &standard_intents(),
            &["Microsoft Basic Render Driver".to_owned()],
            None,
            false,
        )
        .expect_err("unapproved adapter must fail closed");
        assert!(matches!(
            error,
            WindowsEncoderAdmissionError::UnapprovedAdapter { .. }
        ));
    }

    #[test]
    fn an_unresolved_encoder_template_is_still_rejected() {
        for encoder in [None, Some(EncoderSelection::Auto)] {
            let mut template = software_template();
            template.encoder = encoder;
            let error = plan_encoder_sets(
                &topology("Microsoft Basic Render Driver"),
                &template,
                &standard_intents(),
                &["Microsoft Basic Render Driver".to_owned()],
                None,
                false,
            )
            .expect_err("an unresolved encoder must fail closed");
            assert!(
                matches!(error, WindowsEncoderAdmissionError::ConcreteEncoderRequired),
                "unexpected error: {error}"
            );
        }
    }

    /// The NVENC path keeps today's exact candidate ladder: the software
    /// admission above is additive only.
    #[test]
    fn nvenc_host_candidate_ladder_is_unchanged() {
        let plan = plan_encoder_sets(
            &topology("Approved GPU"),
            &template(),
            &standard_intents(),
            &["Approved GPU".to_owned()],
            None,
            true,
        )
        .expect("plan");
        assert_eq!(plan.candidate_count(), 3);
        assert_eq!(plan.sets[0].specs[0].encoder, Some(EncoderSelection::Nvenc));
        assert_eq!(
            plan.sets[2].specs[0].encoder,
            Some(EncoderSelection::SoftwareH264)
        );
    }

    #[test]
    fn mf_monitor_candidates_use_eight_bit_while_preserving_supported_range() {
        let mut template = template();
        template.bit_depth = BitDepth::Ten;
        template.color_range = ColorRange::Full;
        let plan = plan_encoder_sets(
            &topology("Approved GPU"),
            &template,
            &standard_intents(),
            &["Approved GPU".to_owned()],
            None,
            true,
        )
        .expect("plan");
        let software_set = plan
            .sets
            .iter()
            .find(|set| {
                set.specs
                    .iter()
                    .any(|spec| spec.encoder == Some(EncoderSelection::SoftwareH264))
            })
            .expect("software candidate");
        for region in software_set.candidate.roster().plans() {
            if region.backend == EncoderBackend::OpenH264 {
                assert_eq!(region.video.bit_depth, BitDepth::Eight);
                assert_eq!(region.video.range, ColorRange::Full);
                assert_eq!(region.video.matrix, ColorMatrix::Bt709);
            }
        }
    }

    #[test]
    fn adaptive_nvenc_candidates_are_uniform_and_ordered_for_the_whole_roster() {
        let mut template = template();
        template.codec = VideoCodec::Av1;
        template.chroma = ChromaSubsampling::Yuv420;
        template.video_selection =
            arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance;
        let plan = plan_encoder_sets(
            &topology("Approved GPU"),
            &template,
            &standard_intents(),
            &["Approved GPU".to_owned()],
            None,
            false,
        )
        .expect("adaptive plan");
        assert_eq!(plan.candidate_count(), 3);
        assert_eq!(
            plan.sets
                .iter()
                .map(|set| set.template.codec)
                .collect::<Vec<_>>(),
            [VideoCodec::Av1, VideoCodec::H265, VideoCodec::H264]
        );
        for set in &plan.sets {
            assert!(set
                .specs
                .iter()
                .all(|spec| spec.codec == Some(set.template.codec)));
        }
        assert_eq!(plan.sets[2].template.bit_depth, BitDepth::Eight);
    }

    #[test]
    fn adaptive_codecs_and_software_reassignment_fit_the_shared_candidate_bound() {
        let mut template = template();
        template.codec = VideoCodec::H265;
        template.chroma = ChromaSubsampling::Yuv420;
        template.video_selection =
            arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance;
        let plan = plan_encoder_sets(
            &topology("Approved GPU"),
            &template,
            &standard_intents(),
            &["Approved GPU".to_owned()],
            None,
            true,
        )
        .expect("adaptive plan with software reassignment");

        assert_eq!(plan.candidate_count(), 6);
        assert!(plan.candidate_count() <= arcen_media::MAX_ENCODER_SET_CANDIDATES);
        plan.admit(plan.activity_profiles(), thresholds(), &StallPrimaryAdapter)
            .expect("the shared admission bound must accept the generated candidate roster");
    }
}
