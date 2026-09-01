//! Pure Linux integration for aggregate encoder-set admission.
//!
//! Candidate construction reuses `multi_capenc`'s exact pipeline planner.
//! Every hardware binding therefore remains tied to the committed dedicated
//! Xorg head/output index; no measurement adapter is allowed to auto-select a
//! different GPU or backend.

use arcen_media::video::{adaptive_codec_ladder, EncoderBackend};
use arcen_media::{
    admit_encoder_sets, ActivityClass, BitrateBudgetKbps, ChromaSubsampling, DirtyRatio,
    EncoderAdmissionError, EncoderAdmissionThresholds, EncoderBindingId, EncoderMeasurementAdapter,
    EncoderProbeFailure, EncoderProbeRequest, EncoderProbeTrace, EncoderSetAttemptOutcome,
    EncoderSetCandidate, EncoderSetDecision, MediaContractError, MediaStreamEpoch,
    RegionActivityProfile, RegionActivityProfiles, RegionAdmissionPriority, RegionEncoderBinding,
    RegionGeneration, RegionId, RegionMediaPlan, RegionMediaRoster, VideoCodec, VideoConfiguration,
};
use thiserror::Error;

use crate::display::topology::LinuxTopologyPlan;

use super::capenc::EncoderSelection;
use super::multi_capenc::{
    build_pipeline_specs_with_resources, MonitorPipelineSpec, MonitorPipelineTemplate,
    MultiCapencConfigError,
};

#[derive(Debug, Error)]
pub enum LinuxEncoderAdmissionError {
    #[error("Linux encoder candidate planning failed: {0}")]
    Pipeline(#[from] MultiCapencConfigError),
    #[error("Linux encoder candidate media contract failed: {0}")]
    Media(#[from] MediaContractError),
    #[error("Linux encoder activity contract failed: {0}")]
    Region(#[from] arcen_media::RegionContractError),
    #[error("aggregate encoder admission failed: {0}")]
    Admission(#[from] EncoderAdmissionError),
    #[error("monitor {monitor_id} has no matching pipeline spec")]
    MissingPipelineSpec { monitor_id: u16 },
    #[error("monitor {monitor_id} pipeline geometry differs from the topology plan")]
    PipelineGeometryMismatch { monitor_id: u16 },
    #[error("monitor {monitor_id} has no explicit Xorg display binding")]
    MissingDisplayBinding { monitor_id: u16 },
    #[error("monitor {monitor_id} retained a non-concrete encoder request {encoder:?}")]
    NonConcreteEncoder {
        monitor_id: u16,
        encoder: EncoderSelection,
    },
    #[error("monitor {monitor_id} uses unsupported Annex-B codec {codec:?}")]
    UnsupportedCodec { monitor_id: u16, codec: String },
}

#[derive(Clone, Debug)]
struct PlannedEncoderSet {
    candidate: EncoderSetCandidate,
    specs: Vec<MonitorPipelineSpec>,
}

/// Ordered Linux candidate sets plus the exact capenc specs behind each one.
#[derive(Clone, Debug)]
pub struct LinuxEncoderAdmissionPlan {
    sets: Vec<PlannedEncoderSet>,
    profiles: RegionActivityProfiles,
}

impl LinuxEncoderAdmissionPlan {
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

    #[must_use]
    pub fn selected_specs(&self, decision: &EncoderSetDecision) -> Option<&[MonitorPipelineSpec]> {
        let index = decision.selected_candidate_index()?;
        self.sets.get(index).map(|set| set.specs.as_slice())
    }

    /// The negotiated media roster of the encoder set this decision accepted.
    ///
    /// This is the authority for every admitted region's published bitrate:
    /// `session::multi_monitor::build_applied_capability` reads
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

    /// Runs the shared concurrent measurement framework over these exact
    /// planned bindings.
    ///
    /// # Errors
    ///
    /// Returns shared validation errors before an adapter is invoked.
    pub fn admit<A: EncoderMeasurementAdapter>(
        &self,
        profiles: &RegionActivityProfiles,
        thresholds: EncoderAdmissionThresholds,
        adapter: &A,
    ) -> Result<EncoderSetDecision, LinuxEncoderAdmissionError> {
        let candidates = self.sets.iter().map(|set| set.candidate.clone()).collect();
        Ok(admit_encoder_sets(
            candidates, profiles, thresholds, adapter,
        )?)
    }

    /// Executes the production bounded child-process probes.
    ///
    /// # Errors
    ///
    /// Returns shared validation failures. Individual child failures remain
    /// measured candidate outcomes so later reassignment candidates can run.
    pub fn admit_runtime(
        &self,
        thresholds: EncoderAdmissionThresholds,
    ) -> Result<EncoderSetDecision, LinuxEncoderAdmissionError> {
        let adapter = LinuxChildProbeAdapter { plan: self };
        self.admit(&self.profiles, thresholds, &adapter)
    }
}

struct LinuxChildProbeAdapter<'a> {
    plan: &'a LinuxEncoderAdmissionPlan,
}

impl EncoderMeasurementAdapter for LinuxChildProbeAdapter<'_> {
    fn measure(
        &self,
        request: &EncoderProbeRequest,
    ) -> Result<EncoderProbeTrace, EncoderProbeFailure> {
        let set = self.plan.sets.get(request.candidate_index).ok_or_else(|| {
            EncoderProbeFailure::invalid("probe candidate index is outside the Linux plan")
        })?;
        let spec = set
            .specs
            .iter()
            .find(|spec| spec.session_monitor_id == request.plan.session_monitor_id)
            .ok_or_else(|| EncoderProbeFailure::invalid("Linux probe pipeline spec is missing"))?;
        let expected_binding = set
            .candidate
            .binding(request.plan.session_monitor_id)
            .ok_or_else(|| EncoderProbeFailure::invalid("Linux probe binding is missing"))?;
        if expected_binding != &request.binding_id {
            return Err(EncoderProbeFailure::invalid(
                "Linux probe binding differs from the planned candidate",
            ));
        }
        let mut command = super::capenc::admission_probe_command(&spec.config)
            .map_err(|error| EncoderProbeFailure::context_open(error.to_string()))?;
        arcen_capenc::admission_probe::run_admission_probe_child(command.as_std_mut(), request)
    }
}

/// Builds ordered Linux candidates from the exact multi-capenc resource
/// planner.
///
/// Candidate zero honors the operator's optional hardware ceiling. Later
/// candidates progressively move only non-full-color regions to exact
/// OpenH264 plans. No count is inferred from a vendor name or model.
///
/// A *further-degraded* candidate is only offered when every region it moves
/// to OpenH264 fits that encoder's exact limits. One whose geometry OpenH264
/// would have to clamp is simply not offered — it is a reassignment this host
/// declines to measure, never a reason to refuse the whole session. Candidate
/// zero is the one mandatory set: its refusal is exactly the refusal
/// [`super::multi_capenc::validate_monitor_resource_policy`] already returns
/// for the same ceiling at admission time, so auth-time admission and
/// attachment-time planning can never disagree about whether the operator's
/// configured ceiling can serve a committed topology.
///
/// # Errors
///
/// Returns the existing multi-capenc policy error or a media/binding contract
/// error.
pub fn plan_encoder_sets(
    topology: &LinuxTopologyPlan,
    template: &MonitorPipelineTemplate,
    hardware_context_ceiling: Option<u8>,
    allow_software_fallback: bool,
) -> Result<LinuxEncoderAdmissionPlan, LinuxEncoderAdmissionError> {
    let monitor_count = topology.monitors.len();
    let hardware_max = hardware_context_ceiling
        .map_or(monitor_count, usize::from)
        .min(monitor_count);
    let full_color_required = topology
        .monitors
        .iter()
        .filter(|monitor| {
            monitor.quality_intent
                == arcen_protocol::messages::MonitorQualityIntentMsg::FullColorRequired
        })
        .count();
    if full_color_required > hardware_max {
        return Err(MultiCapencConfigError::FullColorExceedsNvencLimit {
            required: full_color_required,
            limit: hardware_max,
        }
        .into());
    }
    if monitor_count > hardware_max && !allow_software_fallback {
        return Err(MultiCapencConfigError::SoftwareFallbackDisabled {
            requested: monitor_count,
            limit: hardware_max,
        }
        .into());
    }

    let hardware_counts =
        if template.encoder == EncoderSelection::NativeNvenc && allow_software_fallback {
            (full_color_required..=hardware_max)
                .rev()
                .collect::<Vec<_>>()
        } else {
            vec![hardware_max]
        };

    let codec_templates = adaptive_codec_templates(template);
    let mut sets = Vec::with_capacity(hardware_counts.len() * codec_templates.len());
    // Exhaust same-GPU codec options before moving any region to software.
    // This preserves the product ranking: hardware AV1 -> hardware HEVC ->
    // hardware H.264 -> mixed/software candidates.
    for hardware_count in &hardware_counts {
        for codec_template in &codec_templates {
            let limit = u8::try_from(*hardware_count).unwrap_or(u8::MAX);
            let specs = match build_pipeline_specs_with_resources(
                topology,
                codec_template,
                Some(limit),
                allow_software_fallback,
            ) {
                Ok(specs) => specs,
                Err(MultiCapencConfigError::SoftwareGeometryWouldClamp { .. })
                    if !sets.is_empty() =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            let candidate = candidate_from_specs(topology, &specs)?;
            sets.push(PlannedEncoderSet { candidate, specs });
        }
    }
    debug_assert!(
        !sets.is_empty(),
        "candidate zero is either planned or returned as an error"
    );
    let profiles = activity_profiles(topology, &sets)?;
    Ok(LinuxEncoderAdmissionPlan { sets, profiles })
}

/// Expand one adaptive NVENC preference into uniform, whole-roster codec
/// candidates. The aggregate admission framework measures candidate zero for
/// every region before moving to candidate one, so no monitor can independently
/// fall back to a different codec.
fn adaptive_codec_templates(template: &MonitorPipelineTemplate) -> Vec<MonitorPipelineTemplate> {
    if template.encoder != EncoderSelection::NativeNvenc
        || template.video_selection
            != arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance
    {
        return vec![template.clone()];
    }

    let Some(preferred) = VideoCodec::from_token(&template.codec) else {
        return vec![template.clone()];
    };
    adaptive_codec_ladder(preferred)
        .iter()
        .map(|codec| {
            let mut candidate = template.clone();
            candidate.codec = codec.token().to_string();
            candidate.yuv444 = false;
            if *codec == VideoCodec::H264 {
                candidate.bit_depth = arcen_media::BitDepth::Eight;
            }
            candidate
        })
        .collect()
}

fn activity_profiles(
    topology: &LinuxTopologyPlan,
    sets: &[PlannedEncoderSet],
) -> Result<RegionActivityProfiles, LinuxEncoderAdmissionError> {
    let generation = RegionGeneration::new(topology.generation.get())?;
    let mut profiles = Vec::with_capacity(topology.monitors.len());
    for monitor in &topology.monitors {
        let full_color = monitor.quality_intent
            == arcen_protocol::messages::MonitorQualityIntentMsg::FullColorRequired;
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
                .ok_or(LinuxEncoderAdmissionError::MissingPipelineSpec {
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
                    target: crate::logging::target::CAPENC,
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
                        target: crate::logging::target::CAPENC,
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
                    target: crate::logging::target::CAPENC,
                    candidate = attempt.candidate_index,
                    failures = failures.len(),
                    "aggregate encoder admission probe failed"
                );
                // Without the per-region cause, a rejected candidate is
                // indistinguishable from a slow one in the field: name the
                // exact region, failure kind and detail that refused.
                for failure in failures {
                    tracing::warn!(
                        target: crate::logging::target::CAPENC,
                        candidate = attempt.candidate_index,
                        monitor_id = failure.session_monitor_id.get(),
                        kind = ?failure.failure.kind,
                        detail = %failure.failure.detail,
                        "region encoder admission probe failed"
                    );
                }
            }
        }
    }
    tracing::info!(
        target: crate::logging::target::CAPENC,
        decision = match decision {
            EncoderSetDecision::Accept { .. } => "accept",
            EncoderSetDecision::Reassign { .. } => "reassign",
            EncoderSetDecision::Reject { .. } => "reject",
        },
        selected_candidate = ?decision.selected_candidate_index(),
        "aggregate encoder admission decision"
    );
}

fn candidate_from_specs(
    topology: &LinuxTopologyPlan,
    specs: &[MonitorPipelineSpec],
) -> Result<EncoderSetCandidate, LinuxEncoderAdmissionError> {
    let epoch = MediaStreamEpoch::new(topology.generation.get())?;
    let mut plans = Vec::with_capacity(topology.monitors.len());
    let mut bindings = Vec::with_capacity(topology.monitors.len());
    for monitor in &topology.monitors {
        let spec = specs
            .iter()
            .find(|spec| spec.session_monitor_id == monitor.session_monitor_id)
            .ok_or(LinuxEncoderAdmissionError::MissingPipelineSpec {
                monitor_id: monitor.session_monitor_id.get(),
            })?;
        if spec.config.width != monitor.width || spec.config.height != monitor.height {
            return Err(LinuxEncoderAdmissionError::PipelineGeometryMismatch {
                monitor_id: monitor.session_monitor_id.get(),
            });
        }
        let display = spec
            .config
            .display
            .as_deref()
            .filter(|display| !display.trim().is_empty())
            .ok_or(LinuxEncoderAdmissionError::MissingDisplayBinding {
                monitor_id: monitor.session_monitor_id.get(),
            })?;
        let backend = match spec.config.encoder {
            EncoderSelection::NativeNvenc => EncoderBackend::NativeNvenc,
            EncoderSelection::SoftwareH264 => EncoderBackend::OpenH264,
            encoder @ (EncoderSelection::Auto
            | EncoderSelection::WindowsMediaFoundation
            | EncoderSelection::SoftwareAv1) => {
                return Err(LinuxEncoderAdmissionError::NonConcreteEncoder {
                    monitor_id: monitor.session_monitor_id.get(),
                    encoder,
                });
            }
        };
        let codec = VideoCodec::from_token(&spec.config.codec).ok_or_else(|| {
            LinuxEncoderAdmissionError::UnsupportedCodec {
                monitor_id: monitor.session_monitor_id.get(),
                codec: spec.config.codec.clone(),
            }
        })?;
        let chroma = if spec.config.yuv444 {
            ChromaSubsampling::Yuv444
        } else {
            ChromaSubsampling::Yuv420
        };
        plans.push(RegionMediaPlan::new(
            monitor.session_monitor_id,
            epoch,
            backend,
            VideoConfiguration {
                codec,
                chroma,
                bit_depth: spec.config.bit_depth,
                range: spec.config.color_range,
                matrix: spec.config.color_matrix,
                ..VideoConfiguration::legacy_h264()
            },
            spec.config.width,
            spec.config.height,
            spec.config.fps,
            BitrateBudgetKbps::nominal_for_geometry(
                spec.config.width,
                spec.config.height,
                spec.config.fps,
            ),
        )?);
        bindings.push(RegionEncoderBinding {
            session_monitor_id: monitor.session_monitor_id,
            binding_id: EncoderBindingId::new(format!(
                "linux-xorg:display={display}:head={}:output={}:backend={}",
                spec.head,
                spec.output_index,
                backend.ready_token()
            ))?,
        });
    }
    let roster = RegionMediaRoster::new(plans)?;
    Ok(EncoderSetCandidate::new(roster, bindings)?)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use arcen_media::{
        ActivityClass, DirtyRatio, EncoderProbeSample, LogicalPoint, LogicalRect, LogicalSize,
        PhysicalSize, RegionActivityProfile, RegionAdmissionPriority, RegionGeneration, RegionId,
        Rotation, Scale120, SessionMonitorId, TopologyGeneration,
    };
    use arcen_protocol::messages::{CursorMode, MonitorQualityIntentMsg};
    use arcen_telemetry::CorrelationId;

    use super::*;
    use crate::display::topology::LinuxMonitorPlan;

    fn sid(value: u16) -> SessionMonitorId {
        SessionMonitorId::new(value).expect("monitor id")
    }

    fn monitor(
        id: u16,
        head: &str,
        primary: bool,
        quality_intent: MonitorQualityIntentMsg,
    ) -> LinuxMonitorPlan {
        sized_monitor(id, head, primary, quality_intent, 1_920, 1_080)
    }

    fn sized_monitor(
        id: u16,
        head: &str,
        primary: bool,
        quality_intent: MonitorQualityIntentMsg,
        width: u32,
        height: u32,
    ) -> LinuxMonitorPlan {
        LinuxMonitorPlan {
            session_monitor_id: sid(id),
            client_display_id: format!("display-{id}"),
            head: head.to_owned(),
            x: 0,
            y: 0,
            width,
            height,
            logical_rect: LogicalRect::new(
                LogicalPoint::from_pixels(0, 0).expect("origin"),
                LogicalSize::from_pixels(u64::from(width), u64::from(height)).expect("size"),
            )
            .expect("rect"),
            physical_size: PhysicalSize::new(width, height).expect("physical size"),
            scale: Scale120::new(120).expect("scale"),
            rotation: Rotation::Degrees0,
            primary,
            quality_intent,
            mode_token: format!("{width}x{height}"),
        }
    }

    /// The exact committed topology pier-linux.example.internal planned for the Deck's
    /// `match_layout` request on 2026-08-10: a 3008x1692 primary (above
    /// OpenH264's 1920x1200 contract) beside a 1800x1168 secondary (inside
    /// it), with neither monitor demanding full color.
    fn oversize_primary_topology() -> LinuxTopologyPlan {
        LinuxTopologyPlan {
            generation: TopologyGeneration::new(1).expect("generation"),
            virtual_width: 4_808,
            virtual_height: 1_962,
            monitors: vec![
                sized_monitor(
                    1,
                    "DFP-0",
                    true,
                    MonitorQualityIntentMsg::HostDefault,
                    3_008,
                    1_692,
                ),
                sized_monitor(
                    2,
                    "DFP-1",
                    false,
                    MonitorQualityIntentMsg::HostDefault,
                    1_800,
                    1_168,
                ),
            ],
        }
    }

    fn topology() -> LinuxTopologyPlan {
        LinuxTopologyPlan {
            generation: TopologyGeneration::new(7).expect("generation"),
            virtual_width: 3_840,
            virtual_height: 1_080,
            monitors: vec![
                monitor(
                    1,
                    "DFP-0",
                    true,
                    MonitorQualityIntentMsg::BandwidthOptimized,
                ),
                monitor(
                    2,
                    "DFP-2",
                    false,
                    MonitorQualityIntentMsg::FullColorRequired,
                ),
            ],
        }
    }

    fn template() -> MonitorPipelineTemplate {
        MonitorPipelineTemplate {
            binary: PathBuf::from("/nonexistent/arcen-capenc"),
            codec: "h265".to_owned(),
            encoder: EncoderSelection::NativeNvenc,
            fps: 60,
            yuv444: true,
            bit_depth: arcen_media::BitDepth::Ten,
            color_range: arcen_media::ColorRange::Full,
            color_matrix: arcen_media::ColorMatrix::Bt709,
            transfer: arcen_media::TransferCharacteristics::Bt709,
            color_primaries: arcen_media::ColorPrimaries::Bt709,
            intent: arcen_media::EncodeIntent::default(),
            qp_map: arcen_media::video::QpMapPolicy::default(),
            video_selection: arcen_protocol::messages::VideoSelectionIntent::Exact,
            cursor_mode: CursorMode::Local,
            display: Some(":99".to_owned()),
            xauthority: None,
            execution: None,
            session_log_id: CorrelationId::from_uuid_v4_bytes([0; 16]),
        }
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
    fn full_color_secondary_keeps_hardware_priority_in_reassignment() {
        let plan = plan_encoder_sets(&topology(), &template(), None, true).expect("plan");
        assert_eq!(plan.candidate_count(), 2);

        let fallback = &plan.sets[1];
        assert_eq!(
            fallback.specs[0].config.encoder,
            EncoderSelection::SoftwareH264
        );
        assert_eq!(
            fallback.specs[1].config.encoder,
            EncoderSelection::NativeNvenc
        );
        assert!(fallback.specs[1].config.yuv444);
        assert!(fallback
            .candidate
            .binding(sid(2))
            .expect("binding")
            .as_str()
            .contains("display=:99"));
        assert!(fallback
            .candidate
            .binding(sid(2))
            .expect("binding")
            .as_str()
            .contains("head=DFP-2"));
    }

    #[test]
    fn configured_ceiling_is_a_bound_not_a_vendor_claim() {
        let plan = plan_encoder_sets(&topology(), &template(), Some(1), true).expect("plan");
        assert_eq!(plan.candidate_count(), 1);
        assert_eq!(
            plan.primary_specs()
                .iter()
                .filter(|spec| spec.config.encoder == EncoderSelection::NativeNvenc)
                .count(),
            1
        );
    }

    #[test]
    fn adaptive_codec_candidates_are_uniform_and_ordered_for_the_whole_roster() {
        let mut topology = topology();
        for monitor in &mut topology.monitors {
            monitor.quality_intent = MonitorQualityIntentMsg::BandwidthOptimized;
        }
        let mut template = template();
        template.codec = "av1".to_string();
        template.yuv444 = false;
        template.video_selection =
            arcen_protocol::messages::VideoSelectionIntent::AdaptivePerformance;

        let plan =
            plan_encoder_sets(&topology, &template, None, false).expect("adaptive codec plan");
        assert_eq!(plan.candidate_count(), 3);
        assert_eq!(
            plan.sets
                .iter()
                .map(|set| set.specs[0].config.codec.as_str())
                .collect::<Vec<_>>(),
            ["av1", "h265", "h264"]
        );
        for set in &plan.sets {
            let codec = &set.specs[0].config.codec;
            assert!(
                set.specs.iter().all(|spec| spec.config.codec == *codec),
                "one aggregate candidate must never mix codecs across monitors"
            );
        }
        assert!(plan.sets[2]
            .specs
            .iter()
            .all(|spec| spec.config.bit_depth == arcen_media::BitDepth::Eight));
    }

    #[test]
    fn activity_profiles_can_cover_the_exact_linux_roster() {
        let plan = plan_encoder_sets(&topology(), &template(), Some(1), true).expect("plan");
        let profiles = RegionActivityProfiles::new(vec![
            RegionActivityProfile {
                session_monitor_id: sid(1),
                region_generation: RegionGeneration::new(7).expect("generation"),
                region_id: RegionId::new(1).expect("region"),
                activity_class: ActivityClass::Idle,
                dirty_ratio: DirtyRatio::ZERO,
                target_fps: 1,
                priority: RegionAdmissionPriority::Standard,
            },
            RegionActivityProfile {
                session_monitor_id: sid(2),
                region_generation: RegionGeneration::new(7).expect("generation"),
                region_id: RegionId::new(2).expect("region"),
                activity_class: ActivityClass::FullMotion,
                dirty_ratio: DirtyRatio::FULL,
                target_fps: 60,
                priority: RegionAdmissionPriority::FullColorRequired,
            },
        ])
        .expect("profiles");
        assert_eq!(
            profiles.profiles().len(),
            plan.primary_specs().len(),
            "activity and exact planned encoder rosters stay aligned"
        );
    }

    #[test]
    fn generated_profiles_distinguish_primary_full_color_and_idle_siblings() {
        let mut topology = topology();
        topology.virtual_width = 5_760;
        topology.monitors.push(monitor(
            3,
            "DFP-3",
            false,
            MonitorQualityIntentMsg::BandwidthOptimized,
        ));
        let plan = plan_encoder_sets(&topology, &template(), None, true).expect("plan");

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
        let plan = plan_encoder_sets(&topology(), &template(), None, true).expect("plan");
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
        assert_eq!(selected[0].config.encoder, EncoderSelection::SoftwareH264);
        assert_eq!(selected[1].config.encoder, EncoderSelection::NativeNvenc);
        assert!(matches!(
            decision.attempts()[0].outcome,
            EncoderSetAttemptOutcome::ThresholdFailed { .. }
        ));
    }

    /// Regression: an over-1920x1200 primary must not make the whole
    /// multi-monitor session unplannable just because the *most* degraded
    /// candidate (every region on OpenH264) is infeasible for it. Before this
    /// fix, `hardware_counts` always ended at `full_color_required` — `0` when
    /// no monitor demands full color — and that all-software candidate's
    /// `SoftwareGeometryWouldClamp` aborted planning, so pier-linux.example.internal closed every
    /// Match My Layout attachment with "capture/encoder initialization failed"
    /// and could never advertise `input_capabilities.region_input=available`.
    #[test]
    fn an_infeasible_all_software_candidate_is_skipped_not_fatal() {
        let plan = plan_encoder_sets(&oversize_primary_topology(), &template(), None, true)
            .expect("an oversize primary must still plan its hardware candidate");

        assert_eq!(
            plan.candidate_count(),
            2,
            "only the all-software candidate is infeasible for a 3008x1692 region"
        );
        assert!(
            plan.primary_specs()
                .iter()
                .all(|spec| spec.config.encoder == EncoderSelection::NativeNvenc),
            "candidate zero keeps every region on hardware"
        );
        let reassigned = &plan.sets[1];
        assert_eq!(
            reassigned.specs[0].config.encoder,
            EncoderSelection::NativeNvenc,
            "the oversize primary is never reassigned to OpenH264"
        );
        assert_eq!(
            reassigned.specs[1].config.encoder,
            EncoderSelection::SoftwareH264,
            "only the region OpenH264 can encode exactly is reassigned"
        );
    }

    /// The operator ceiling pier-linux.example.internal actually runs (`nvenc_session_limit: 1`)
    /// with the same topology: candidate zero pins the oversize primary to
    /// NVENC and the exactly-encodable secondary to OpenH264, and no further
    /// degradation is offered.
    #[test]
    fn a_single_hardware_context_ceiling_still_plans_an_oversize_primary() {
        let plan = plan_encoder_sets(&oversize_primary_topology(), &template(), Some(1), true)
            .expect("ceiling of one must plan the committed topology");

        assert_eq!(plan.candidate_count(), 1);
        assert_eq!(
            plan.primary_specs()[0].config.encoder,
            EncoderSelection::NativeNvenc
        );
        assert_eq!(
            plan.primary_specs()[1].config.encoder,
            EncoderSelection::SoftwareH264
        );
    }

    /// Candidate zero stays mandatory: when the operator's own ceiling forces a
    /// region OpenH264 cannot encode exactly onto software, planning still
    /// refuses — the identical refusal `validate_monitor_resource_policy`
    /// returns at admission time, so the two can never disagree.
    #[test]
    fn candidate_zero_infeasibility_still_refuses_and_matches_admission() {
        let mut topology = oversize_primary_topology();
        topology.monitors[0].primary = false;
        topology.monitors[1].primary = true;

        let planned = plan_encoder_sets(&topology, &template(), Some(1), true);
        assert!(matches!(
            planned,
            Err(LinuxEncoderAdmissionError::Pipeline(
                MultiCapencConfigError::SoftwareGeometryWouldClamp {
                    width: 3_008,
                    height: 1_692,
                    ..
                }
            ))
        ));
        assert!(
            crate::media::multi_capenc::validate_monitor_resource_policy(&topology, Some(1), true)
                .is_err(),
            "auth-time admission refuses exactly what attachment-time planning refuses"
        );
    }
}
